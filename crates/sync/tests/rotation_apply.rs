//! P3-I/J 纯状态机测试：epoch 检测、激活 commit 绑定、grant 外签、restart 判定。

mod support;

use support::*;

use menghunote_storage::Vault;
use menghunote_sync::api_version::RelayApiVersion;
use menghunote_sync::relay::{RelayClient, RelayState, RotationView};
use menghunote_sync::rotation_apply::{
    apply_verified_baseline, decide_epoch_action, detect_epoch_change, verify_activation_commit,
    verify_self_grant_header, ApplyOutcome, BaselinePayload, EpochAction, RestartReason,
};
use menghunote_sync::{DeviceId, VaultId};
use menghunote_sync_proto::{ActivationCommitV1, GrantAadV1};
use sha2::{Digest, Sha256};

const VAULT_ID: VaultId = [0x11; 16];

fn view() -> RotationView {
    RotationView {
        rotation_id: [0x21; 16],
        state: "activated".to_owned(),
        from_epoch: 1,
        to_epoch: 2,
        freeze_seq: 6,
        revoked_count: 0,
        member_set_hash: Some([0x22; 32]),
        grants_count: 1,
        grants_digest: Some([0x23; 32]),
        published_checkpoints: 2,
        control_rev: 4,
        auth_rev: 3,
        manager_rev: 2,
        from_epoch_revoked_once: false,
        freeze_asset_count: 3,
        freeze_asset_digest: Some([0x24; 32]),
        member_cert_digest: Some([0x25; 32]),
        primary_checkpoint: Some([0x26; 16]),
        fallback_checkpoint: Some([0x27; 16]),
        activation_commit: Some(vec![0xAA]),
        activated_at: Some(1_700_000_000),
        activated_control_rev: Some(4),
        candidate_count: 2,
        candidate_ready: true,
        reservation_id: None,
    }
}

fn state(epoch: u64) -> RelayState {
    RelayState {
        vault_id: VAULT_ID,
        log_id: [0x31; 16],
        instance_id: [0x32; 16],
        epoch,
        control_rev: 4,
        auth_rev: 3,
        head_seq: 6,
        gc_floor: 0,
        current_rotation_id: None,
    }
}

#[test]
fn epoch_change_requires_activated_rotation_and_control_record() {
    ensure_test_identity_wrap();
    let control = vec![(4, "epoch.activated".to_owned())];
    let change = detect_epoch_change(&state(2), &view(), &control, 1)
        .expect("detect")
        .expect("有变化");
    assert_eq!(change.to_epoch, 2);
    assert!(detect_epoch_change(&state(1), &view(), &control, 1)
        .expect("detect")
        .is_none());
    assert!(detect_epoch_change(&state(2), &view(), &[], 1).is_err());
    // 控制链上只有别的 rev 的激活记录 → 不算（防止任取一条历史激活记录）。
    let wrong_rev = vec![(3, "epoch.activated".to_owned())];
    assert!(detect_epoch_change(&state(2), &view(), &wrong_rev, 1).is_err());
    let mut frozen = view();
    frozen.state = "frozen".to_owned();
    assert!(detect_epoch_change(&state(2), &frozen, &control, 1).is_err());
}

#[test]
fn activation_commit_must_bind_vault_rotation_and_epoch() {
    ensure_test_identity_wrap();
    let commit = ActivationCommitV1 {
        vault_id: VAULT_ID,
        instance_id: [0x22; 16],
        rotation_id: [0x21; 16],
        from_epoch: 1,
        to_epoch: 2,
        covered_seq: 6,
        primary_checkpoint: [0x26; 16],
        fallback_checkpoint: [0x27; 16],
        primary_covered_seq: 6,
        fallback_covered_seq: 6,
        primary_manifest: [0x41; 32],
        fallback_manifest: [0x42; 32],
        member_set_hash: [0x43; 32],
        grants_digest: [0x44; 32],
        asset_set_digest: Some([0x45; 32]),
        control_rev: 4,
    }
    .encode()
    .expect("encode");
    let hash: [u8; 32] = Sha256::digest(&commit).into();
    verify_activation_commit(&commit, &hash, VAULT_ID, [0x22; 16], [0x21; 16], 1, 2, 4)
        .expect("valid commit");
    assert!(
        verify_activation_commit(&commit, &hash, VAULT_ID, [0x22; 16], [0x21; 16], 1, 3, 4)
            .is_err()
    );
    assert!(
        verify_activation_commit(&commit, &hash, VAULT_ID, [0x22; 16], [0x99; 16], 1, 2, 4)
            .is_err()
    );
    // relay 字段不是 CDE：把它当 CDE 解码必须失败（不得把 hash 当 commit）。
    assert!(ActivationCommitV1::decode(&hash).is_err());
    // hash 不匹配（S/control_rev/rid 任一被改都会变）→ 拒绝。
    let mut tampered = ActivationCommitV1::decode(&commit).expect("decode");
    tampered.covered_seq = 7;
    let tampered = tampered.encode().expect("encode");
    let tampered_hash: [u8; 32] = Sha256::digest(&tampered).into();
    assert!(
        verify_activation_commit(&tampered, &hash, VAULT_ID, [0x22; 16], [0x21; 16], 1, 2, 4)
            .is_err()
    );
    assert!(verify_activation_commit(
        &commit,
        &tampered_hash,
        VAULT_ID,
        [0x22; 16],
        [0x21; 16],
        1,
        2,
        4
    )
    .is_err());
}

#[test]
fn self_grant_outer_signature_and_recipient_are_checked() {
    ensure_test_identity_wrap();
    let manager = menghunote_sync::device::ManagementKey::from_recovery(
        &menghunote_sync::recovery::RecoverySeed::from_array([0x51; 32]),
        &VAULT_ID,
    )
    .expect("manager");
    let device: DeviceId = [0x52; 16];
    let aad = GrantAadV1 {
        pv: 1,
        vault_id: VAULT_ID,
        epoch: 2,
        manager_rev: 2,
        grant_id: [0x53; 16],
        recipient_device_id: device,
        recipient_x25519_pub: [0x54; 32],
    };
    let mut signed_header = aad.encode().expect("aad");
    signed_header.extend_from_slice(&[0x55; 32]);
    signed_header.extend_from_slice(&[0x56; 32]);
    let signature = manager.sign(&signed_header);
    verify_self_grant_header(&signed_header, &signature, &manager.public_key(), device, 2)
        .expect("valid grant");
    assert!(verify_self_grant_header(
        &signed_header,
        &signature,
        &manager.public_key(),
        [0x57; 16],
        2
    )
    .is_err());
}

#[test]
fn baseline_merge_imports_snapshot_and_keeps_local_doc() {
    ensure_test_identity_wrap();
    let (_dir, vault) = fresh_vault();
    let local = seed_doc(&vault, "local-doc", 7);
    local
        .insert_scalars(0, "offline edit")
        .expect("local write");
    commit_local(&vault, &local);
    let remote = menghunote_storage::BodyDoc::create("remote-doc", 0x99).expect("remote doc");
    remote
        .insert_scalars(0, "remote baseline")
        .expect("remote write");
    let snapshot = remote.export_snapshot().expect("snapshot");
    let payload = BaselinePayload {
        checkpoint_id: [0x62; 16],
        baseline_s: 6,
        docs: vec![("remote-doc".to_owned(), snapshot)],
        assets: Vec::new(),
    };
    apply_verified_baseline(&vault, &payload, "c6").expect("merge");
    let local_after = menghunote_storage::load_body_doc(&vault, "local-doc", 7, None)
        .expect("load local")
        .0;
    assert_eq!(
        local_after.text().to_string(),
        "offline edit",
        "本地离线编辑保留"
    );
    let remote_after = menghunote_storage::load_body_doc(&vault, "remote-doc", 0x99, None)
        .expect("load remote")
        .0;
    assert_eq!(remote_after.text().to_string(), "remote baseline");
}

#[test]
fn failed_engine_apply_keeps_push_frozen_and_success_thaws() {
    ensure_test_identity_wrap();
    let (_dir, vault) = fresh_vault();
    let device = Device::new(0x72);
    let doc = seed_doc(&vault, "engine-doc", device.identity.peer_id);
    doc.insert_scalars(0, "pending").expect("write");
    commit_with_intent(&vault, &doc, &uuid_like(0x7D), [0x7E; 32], EPOCH);
    let mut relay = FakeRelay::new(&[&device]);
    let mut engine = menghunote_sync::engine::SyncEngine::new(
        &vault,
        &mut relay,
        menghunote_sync::engine::EngineIdentity {
            device_id: device.device_id,
            signing_key: device.identity.signing_key.clone(),
            peer_id: device.identity.peer_id,
        },
        device.root.clone(),
    );
    // 没有 rotation，apply 返回 Paused；push 必须保持冻结。
    let identity = menghunote_sync::identity::LocalIdentityV1 {
        vault_id: VAULT_ID,
        device_id: device.device_id,
        peer_id: menghunote_sync::ids::PeerId::from_bytes([0x73; 16]),
        ed25519_seed: menghunote_sync::keys::Secret32::from_array([0x74; 32]),
        x25519_secret: menghunote_sync::keys::Secret32::from_array([0x75; 32]),
        current_epoch: 1,
        current_root: menghunote_sync::keys::Secret32::from_array([0x76; 32]),
        manager_seed: None,
        pinned_instance: [0x77; 16],
        pinned_manager: [0x78; 32],
        control_rev: 1,
        pending_rotation: None,
    };
    let identity_path = _dir.path().join("identity.bin");
    std::fs::write(&identity_path, identity.protect().expect("protect")).expect("write");
    let candidate = menghunote_sync::keys::RootSecretV1::new(
        VAULT_ID,
        2,
        menghunote_sync::keys::Secret32::from_array([0x79; 32]),
    );
    let outcome = engine
        .apply_epoch_change(
            &vault,
            &identity_path,
            &identity,
            [0x7A; 16],
            6,
            "c6",
            &candidate,
            [0x7B; 32],
            [0x7C; 32],
            2,
            None,
        )
        .expect("apply attempt");
    assert!(matches!(
        outcome,
        menghunote_sync::rotation_apply::ApplyOutcome::Paused { .. }
    ));
    let error = engine.encrypt_pending(1).expect_err("必须保持冻结");
    assert!(matches!(
        error,
        menghunote_sync::SyncError::Paused(menghunote_sync::relay::PauseReason::RotationFrozen)
    ));
    engine.thaw_rotation();
    assert!(engine.encrypt_pending(1).is_ok());
}

use menghunote_sync::checkpoint::{CheckpointReport, DocSnapshotRef};
use menghunote_sync::hpke::x25519_public_key;
use menghunote_sync::keys::RootSecretV1;
use menghunote_sync::recovery::RecoverySeed;
use menghunote_sync::relay::{
    BootstrapBundle, BootstrapCheckpoint, BootstrapLease, CheckpointLease, CheckpointManifest,
    CheckpointPublish, CheckpointRef, CheckpointStatus, FrozenAssetPage, ManagerChainEntry,
    ObjectSubmission, PublishedCheckpoint, RelayLogPage, RotationBegin, SelfGrantEvidence,
    SlotRecord, SlotUpload, UpdateOutcome, UpdateSubmission,
};
use menghunote_sync::relay::{EpochActivationLink, VerifiedActivationT};
use menghunote_sync::rotation_apply::{
    activation_commit_materials, apply_epoch_change_with_client, grant_set_digest,
    GrantLeafEvidence,
};
use menghunote_sync::upload::{encrypt_local_parts, UploadedObject};
use menghunote_sync::SyncError;
use menghunote_sync_proto::{
    activation_t_signing_input, checkpoint_signing_input, refs_digest_input, ActivationTV1,
    CheckpointCommit, ManifestV1, ObjectRefV1, ObjectType,
};

/// `RelayClient` 的通用转发宏：wrapper 只覆盖需要注入/篡改的方法，其余原样转发。
macro_rules! forward_relay {
    ($($name:ident($($arg:ident: $ty:ty),*) -> $ret:ty),* $(,)?) => {
        $(fn $name(&mut self $(, $arg: $ty)*) -> $ret { self.inner.$name($($arg),*) })*
    };
}

/// `/state` 永远不带 current_rotation_id；`/bootstrap` 与 `/control` 注入 T/commit 证据。
struct ActivationApplyRelay {
    inner: FakeRelay,
    /// `/control` 里"已复算"的 T 与双签；`None` = 无 T（必须 fail-closed）。
    evidence: Option<VerifiedActivationT>,
    /// 完整 grant leaf（None 时只有 grants_count==1 能证明 inclusion）。
    leaves: Option<Vec<GrantLeafEvidence>>,
    /// `/bootstrap` 返回的主/备基线（含 signed_commit + 生产者签名）。
    bootstrap: Option<BootstrapBundle>,
    /// 模拟 activate 后 `/state` 不再返回 current_rotation_id。
    hide_current_rotation: bool,
    /// 注入的 `/control` activation 链（`None` = 默认空链，fail-closed）。
    chain: Option<Vec<EpochActivationLink>>,
}

impl ActivationApplyRelay {
    fn new(
        inner: FakeRelay,
        evidence: VerifiedActivationT,
        bootstrap: BootstrapBundle,
        chain: Vec<EpochActivationLink>,
    ) -> Self {
        Self {
            inner,
            evidence: Some(evidence),
            leaves: None,
            bootstrap: Some(bootstrap),
            hide_current_rotation: false,
            chain: Some(chain),
        }
    }
}

impl RelayClient for ActivationApplyRelay {
    fn state(&mut self) -> Result<RelayState, SyncError> {
        let mut state = self.inner.state()?;
        if self.hide_current_rotation {
            state.current_rotation_id = None;
        }
        Ok(state)
    }

    fn current_rotation(&mut self) -> Result<Option<RotationView>, SyncError> {
        // 多轮熄灭后 relay 只留最新 activated 视图；HashMap 迭代顺序不稳定，按 epoch 取最大。
        Ok(self
            .inner
            .rotations
            .values()
            .max_by_key(|view| view.to_epoch)
            .cloned())
    }

    fn rotation(&mut self, rotation_id: [u8; 16]) -> Result<RotationView, SyncError> {
        self.inner.rotation(rotation_id)
    }

    fn control_kinds(&mut self) -> Result<Vec<(u64, String)>, SyncError> {
        self.inner.control_kinds()
    }

    fn self_grant_evidence(&mut self) -> Result<SelfGrantEvidence, SyncError> {
        self.inner.self_grant_evidence()
    }

    fn checkpoint_status(
        &mut self,
        checkpoint_id: [u8; 16],
    ) -> Result<CheckpointStatus, SyncError> {
        self.inner.checkpoint_status(checkpoint_id)
    }

    fn checkpoint_manifest(
        &mut self,
        checkpoint_id: &[u8; 16],
    ) -> Result<CheckpointManifest, SyncError> {
        self.inner.checkpoint_manifest(checkpoint_id)
    }

    fn device_cert(
        &mut self,
        device_id: &DeviceId,
    ) -> Result<Option<menghunote_sync_proto::DeviceCertV1>, SyncError> {
        self.inner.device_cert(device_id)
    }

    fn bootstrap(&mut self) -> Result<BootstrapBundle, SyncError> {
        self.bootstrap
            .clone()
            .ok_or(SyncError::Unsupported("测试未配置 bootstrap 基线"))
    }

    fn bootstrap_degraded(&mut self) -> Result<BootstrapBundle, SyncError> {
        self.bootstrap()
    }

    fn reauthenticate(&mut self) -> Result<(), SyncError> {
        self.inner.reauthenticate()
    }

    /// 注入"`/control` 已复算"的 T：这是 `RelayClient` 的第一等入口，
    /// engine 的 `apply_epoch_change` 也走它。
    fn verified_activation_t(
        &mut self,
        _rotation_id: [u8; 16],
    ) -> Result<Option<VerifiedActivationT>, SyncError> {
        Ok(self.evidence.clone())
    }

    fn grant_leaf_evidence(
        &mut self,
        _rotation_id: [u8; 16],
    ) -> Result<Option<Vec<GrantLeafEvidence>>, SyncError> {
        Ok(self.leaves.clone())
    }

    /// 注入 `/control` activation 链：只返回本地 epoch 之后（`to_epoch > from_epoch`）的环。
    fn epoch_activations(
        &mut self,
        from_epoch: u64,
    ) -> Result<Vec<EpochActivationLink>, SyncError> {
        Ok(self
            .chain
            .clone()
            .unwrap_or_default()
            .into_iter()
            .filter(|link| link.evidence.activation_t.to_epoch > from_epoch)
            .collect())
    }

    forward_relay! {
        log(cursor: Option<&str>, limit: u32) -> Result<RelayLogPage, SyncError>,
        part(object_id: &menghunote_sync::ObjectId, part_no: u32) -> Result<Vec<u8>, SyncError>,
        put_part(object_id: &menghunote_sync::ObjectId, part_no: u32, envelope: &[u8]) -> Result<(), SyncError>,
        submit_updates(items: Vec<UpdateSubmission>) -> Result<Vec<UpdateOutcome>, SyncError>,
        submit_object(item: ObjectSubmission) -> Result<UpdateOutcome, SyncError>,
        ack(received_seq: u64, integrated_seq: u64, epoch: u64) -> Result<(), SyncError>,
        put_key_slot(upload: SlotUpload) -> Result<SlotRecord, SyncError>,
        list_key_slots() -> Result<Vec<SlotRecord>, SyncError>,
        activate_password_slot(expected_slot_set_rev: u64, slot_id: menghunote_sync::ObjectId, signed_message: Vec<u8>, signature: [u8; 64]) -> Result<u64, SyncError>,
        create_checkpoint(estimated_bytes: u64, epoch: Option<u64>) -> Result<CheckpointLease, SyncError>,
        add_checkpoint_refs(checkpoint_id: menghunote_sync_proto::CheckpointId, refs: Vec<CheckpointRef>) -> Result<u64, SyncError>,
        publish_checkpoint(publish: CheckpointPublish) -> Result<PublishedCheckpoint, SyncError>,
        renew_bootstrap_lease(lease_id: [u8; 16]) -> Result<BootstrapLease, SyncError>,
        begin_rotation(begin: RotationBegin) -> Result<RotationView, SyncError>,
        rotation_for_request(request_id: [u8; 16]) -> Result<Option<RotationView>, SyncError>,
        management_chain() -> Result<Vec<ManagerChainEntry>, SyncError>,
        rotation_assets(rotation_id: [u8; 16], after: Option<[u8; 32]>, limit: u32) -> Result<FrozenAssetPage, SyncError>,
        active_member_hpke_keys() -> Result<Vec<(DeviceId, [u8; 32])>, SyncError>,
    }
}

/// 一份自洽候选基线的测试材料（status + `CheckpointCommit` + MANIFEST 密文对象）。
struct TestBaseline {
    checkpoint_id: [u8; 16],
    covered_seq: u64,
    cursor: String,
    manifest_oid: [u8; 32],
    signed_commit: Vec<u8>,
    signature: [u8; 64],
}

fn baseline_cursor(seq: u64) -> String {
    format!("c{seq}")
}

/// 测试侧复算 `refs_digest`（与 `crates/sync/src/bootstrap.rs::manifest_refs` 同口径）。
fn baseline_refs_digest(manifest: &ManifestV1) -> [u8; 32] {
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
    Sha256::digest(refs_digest_input(&refs).expect("refs digest input")).into()
}

#[allow(clippy::too_many_arguments)]
fn build_baseline(
    relay: &mut FakeRelay,
    root: &RootSecretV1,
    checkpoint_id: [u8; 16],
    covered_seq: u64,
    epoch: u64,
    manifest_oid: [u8; 32],
    producer: &Device,
    message_seed: u8,
) -> TestBaseline {
    let manifest = ManifestV1 {
        vault_id: VAULT_ID,
        key_epoch: root.key_epoch,
        checkpoint_id,
        covered_relay_seq: covered_seq,
        min_reader: 0,
        structure_doc_id: "structure".to_owned(),
        docs: Vec::new(),
        assets: Vec::new(),
    };
    // `refs_digest` 必须**实算**（与 `bootstrap::manifest_refs` + `refs_digest_input` 同口径）：
    // 签名 commit 覆盖的对象集合就是 manifest 的 docs/assets。夹具不能再传常量，否则
    // I 阶段新增的 refs 复算会把所有正例打死。
    let refs_digest = baseline_refs_digest(&manifest);
    let manifest_bytes = manifest.encode().expect("manifest");
    let (parts, manifest_digest) = encrypt_local_parts(
        root,
        ObjectType::Manifest,
        &manifest_oid,
        [message_seed; 16],
        producer.device_id,
        0,
        0,
        &manifest_bytes,
        &[],
    )
    .expect("manifest parts");
    for (index, envelope, _) in parts {
        relay.parts.insert((manifest_oid, index), envelope);
    }
    let commit = CheckpointCommit {
        vault_id: VAULT_ID,
        checkpoint_id,
        epoch,
        covered_relay_seq: covered_seq,
        manifest_object_id: manifest_oid,
        manifest_digest,
        refs_digest,
        producer_device_id: producer.device_id,
        validation_version: 1,
    };
    let signed_commit = commit.encode().expect("commit encode");
    let input = checkpoint_signing_input(&commit).expect("checkpoint signing input");
    let signature = producer.identity.signing_key.sign(&input);
    relay.checkpoint_status.insert(
        checkpoint_id,
        CheckpointStatus {
            checkpoint_id,
            state: "published".to_owned(),
            role: None,
            covered_seq,
            manifest_object_id: Some(manifest_oid),
            refs_digest: Some(refs_digest),
            candidate_ready: true,
            epoch,
        },
    );
    TestBaseline {
        checkpoint_id,
        covered_seq,
        cursor: baseline_cursor(covered_seq),
        manifest_oid,
        signed_commit,
        signature,
    }
}

/// 一份自洽的 HPKE grant（AAD/enc/ciphertext/签名/包 digest 全部由同一组字段派生）。
struct TestGrant {
    aad: GrantAadV1,
    hpke_enc: [u8; 32],
    ciphertext: Vec<u8>,
    signed_header: Vec<u8>,
    signature: [u8; 64],
    device_id: DeviceId,
}

impl TestGrant {
    fn evidence(&self) -> SelfGrantEvidence {
        let ciphertext_sha256: [u8; 32] = Sha256::digest(&self.ciphertext).into();
        let signed_header_sha256: [u8; 32] = Sha256::digest(&self.signed_header).into();
        let package_input = menghunote_sync_proto::grant_package_digest(
            &self.hpke_enc,
            &ciphertext_sha256,
            &signed_header_sha256,
            &self.signature,
        );
        SelfGrantEvidence {
            epoch: self.aad.epoch,
            device_id: self.device_id,
            manager_rev: self.aad.manager_rev,
            hpke_enc: self.hpke_enc,
            ciphertext: self.ciphertext.clone(),
            signed_header: self.signed_header.clone(),
            signature: self.signature.to_vec(),
            package_digest: Some(Sha256::digest(&package_input).into()),
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn build_self_grant(
    manager: &ManagementKey,
    device_id: DeviceId,
    recipient_pub: [u8; 32],
    epoch: u64,
    manager_rev: u64,
    grant_id: [u8; 16],
    hpke_enc: [u8; 32],
    ciphertext: Vec<u8>,
) -> TestGrant {
    let aad = GrantAadV1 {
        pv: 1,
        vault_id: VAULT_ID,
        epoch,
        recipient_device_id: device_id,
        recipient_x25519_pub: recipient_pub,
        manager_rev,
        grant_id,
    };
    let ciphertext_sha256: [u8; 32] = Sha256::digest(&ciphertext).into();
    let signed_header = aad
        .grant_signed_bytes(&hpke_enc, &ciphertext_sha256)
        .expect("grant signed header");
    let signature = manager.sign(&signed_header);
    TestGrant {
        aad,
        hpke_enc,
        ciphertext,
        signed_header,
        signature,
        device_id,
    }
}

/// B 侧 `apply_epoch_change_with_client`（网络入口）的完整 Fake 夹具：
/// relay epoch 2 已激活、两份自洽候选基线（MANIFEST 密文 + 签名的 `CheckpointCommit`）、
/// 双签 activation T、latest self grant；T 的 pair/commit digest/grant digest 与实际一致。
struct ClientApplyFixture {
    _dir: tempfile::TempDir,
    vault: menghunote_storage::Vault,
    relay: ActivationApplyRelay,
    identity: menghunote_sync::identity::LocalIdentityV1,
    identity_path: std::path::PathBuf,
    candidate: RootSecretV1,
    old_manager: ManagementKey,
    new_manager: ManagementKey,
    device_id: DeviceId,
    recipient_pub: [u8; 32],
    producer: Device,
    view: RotationView,
}

impl ClientApplyFixture {
    fn reinsert_view(&mut self) {
        self.relay.inner.rotations.clear();
        self.relay
            .inner
            .rotations
            .insert(self.view.rotation_id, self.view.clone());
    }

    /// 篡改 T 后必须用旧/新管理 key 重新双签，否则会先撞"签名失败"而测不到目标门禁。
    fn resign_activation_t(&mut self) {
        {
            let evidence = self.relay.evidence.as_mut().expect("T evidence");
            let input =
                activation_t_signing_input(&evidence.activation_t).expect("T signing input");
            evidence.old_t_signature = self.old_manager.sign(&input).to_vec();
            evidence.new_t_signature = self.new_manager.sign(&input).to_vec();
        }
        // `/control` 链尾与 latest T 在现实中是同一份缓存；测试篡改后必须同步，
        // 否则会先撞"链尾不是 latest"而测不到目标门禁。
        self.sync_chain_tail();
    }

    /// 把 `relay.evidence` 的最新内容同步到链尾（测试里两者来自同一 `/control` 事实）。
    fn sync_chain_tail(&mut self) {
        let evidence = self.relay.evidence.clone();
        let Some(chain) = self.relay.chain.as_mut() else {
            return;
        };
        if let (Some(tail), Some(evidence)) = (chain.last_mut(), evidence) {
            tail.evidence = evidence;
        }
    }

    /// 模拟"激活 pair 被普通健康 checkpoint 替换"（评审 0003 P1-C 的真实场景）：在
    /// epoch 2 用同一 producer 建**新的一对** published 基线（新 cid/manifest/S/cursor），
    /// 写入 `checkpoint_status` + 对象分片，并把 `/bootstrap` 主/备都指向新 pair。
    ///
    /// 这正是 relay `checkpoints::prune_retired` 之后的样子：旧激活 pair 已被 prune 出
    /// `/bootstrap`（`checkpoint_refs` 也已删），T 里点名的 cid 再也取不到 cursor。
    fn install_post_activation_pair(&mut self, s: u64) -> ([u8; 16], [u8; 16]) {
        self.install_post_activation_pair_at_epoch(s, 2)
    }

    /// 同上，但按指定 epoch 造 pair（用于"错 epoch"负例）。
    fn install_post_activation_pair_at_epoch(
        &mut self,
        s: u64,
        epoch: u64,
    ) -> ([u8; 16], [u8; 16]) {
        let primary = build_baseline(
            &mut self.relay.inner,
            &self.candidate,
            [0x38; 16],
            s,
            epoch,
            [0x58; 32],
            &self.producer,
            0x04,
        );
        let fallback = build_baseline(
            &mut self.relay.inner,
            &self.candidate,
            [0x39; 16],
            s,
            epoch,
            [0x5D; 32],
            &self.producer,
            0x05,
        );
        let bundle = self.relay.bootstrap.as_mut().expect("bootstrap");
        bundle.primary = BootstrapCheckpoint {
            checkpoint_id: primary.checkpoint_id,
            epoch,
            covered_seq: primary.covered_seq,
            manifest_oid: primary.manifest_oid,
            refs_digest: None,
            cursor: primary.cursor.clone(),
            signed_commit: Some(primary.signed_commit.clone()),
            signature: Some(primary.signature),
        };
        bundle.fallback = BootstrapCheckpoint {
            checkpoint_id: fallback.checkpoint_id,
            epoch,
            covered_seq: fallback.covered_seq,
            manifest_oid: fallback.manifest_oid,
            refs_digest: None,
            cursor: fallback.cursor.clone(),
            signed_commit: Some(fallback.signed_commit.clone()),
            signature: Some(fallback.signature),
        };
        // 新基线覆盖到 S，relay 必然已经产生到 S 的更新：两处 head 都抬到 s。
        bundle.head_seq = bundle.head_seq.max(s);
        self.relay.inner.head_seq = self.relay.inner.head_seq.max(s);
        (primary.checkpoint_id, fallback.checkpoint_id)
    }

    /// 吊销 producer（只影响 `active_member_hpke_keys`；历史证书仍在 `certs` 里）：
    /// 模拟"生产者曾经合法、现在已不是 active 成员"。
    fn revoke_producer(&mut self) {
        self.relay.inner.revoked.insert(self.producer.device_id);
    }

    /// 重写当前 `/bootstrap` 主条目的 `signed_commit` 并用 producer 重签：
    /// 用来构造"签名自洽但字段错误"的负例（错 epoch / 坏 refs 等）。
    ///
    /// **不动** `status`/`entry` 的其它字段：这样负例只改目标字段，失败点才会落在
    /// checkpoint 级认证而不是前面的"status 与选中 cid/S/epoch 不符"。
    fn rewrite_bootstrap_commit(&mut self, mutate: impl FnOnce(&mut CheckpointCommit)) {
        let bundle = self.relay.bootstrap.as_mut().expect("bootstrap");
        let signed_commit = bundle
            .primary
            .signed_commit
            .clone()
            .expect("primary signed_commit");
        let mut commit = CheckpointCommit::decode(&signed_commit).expect("decode commit");
        mutate(&mut commit);
        let bytes = commit.encode().expect("re-encode commit");
        let input = checkpoint_signing_input(&commit).expect("signing input");
        let signature = self.producer.identity.signing_key.sign(&input);
        bundle.primary.signed_commit = Some(bytes);
        bundle.primary.signature = Some(signature);
    }

    /// 用另一份自洽 grant（新 `grant_id` → 新 `signed_header`）替换 latest self grant；
    /// T 的 `grants_digest` 仍只覆盖原 header。
    fn replace_self_grant(&mut self) {
        let other = build_self_grant(
            &self.old_manager,
            self.device_id,
            self.recipient_pub,
            2,
            // 与合法 staging grant 同口径（AAD = 旧管理 rev），只换 grant_id/header，
            // 让失败点落在 T.grants_digest inclusion 而不是 AAD rev。
            1,
            [0x59; 16],
            [0x5A; 32],
            vec![0x5B; 16],
        );
        self.relay.inner.self_grant = Some(other.evidence());
    }
}

fn client_apply_fixture() -> ClientApplyFixture {
    // 默认：候选基线覆盖 S = freeze S = 6。
    client_apply_fixture_with_s(6, 6)
}

/// `freeze_seq` = T/rotation 绑定的冻结 S；`candidate_s` = 两份候选基线实际覆盖的 S。
///
/// 合法情形只要求 `candidate_s >= freeze_seq`（冻结后创建的候选 checkpoint 可以覆盖
/// 更晚的 S）；`candidate_s < freeze_seq` 必须被拒绝。
fn client_apply_fixture_with_s(freeze_seq: u64, candidate_s: u64) -> ClientApplyFixture {
    let (dir, vault) = fresh_vault();
    // support 的 `Device` 用 support::VAULT_ID 造证书；本文件用 [0x11] 的 VAULT_ID，
    // 因此建好设备/relay 后统一改成本文件的 vault。
    let mut producer = Device::new(0xA5);
    producer.vault_id = VAULT_ID;
    let mut relay = FakeRelay::new(&[&producer]);
    relay.vault_id = VAULT_ID;
    relay.epoch = 2;
    relay.control_rev = 4;
    // 候选基线覆盖到 `candidate_s`，relay head 必须已产生到该 S（否则 I 阶段的
    // `S <= min(head)` 边界会（正确地）拒绝）。
    relay.head_seq = freeze_seq.max(candidate_s);
    relay.control_kinds = vec![(4, "epoch.activated".to_owned())];

    let old_manager =
        ManagementKey::from_recovery(&RecoverySeed::from_array([0x51; 32]), &VAULT_ID)
            .expect("old manager");
    let new_manager =
        ManagementKey::from_recovery(&RecoverySeed::from_array([0x5B; 32]), &VAULT_ID)
            .expect("new manager");
    let device_id: DeviceId = [0x64; 16];
    let x25519_secret = Secret32::from_array([0x67; 32]);
    let recipient_pub = x25519_public_key(&x25519_secret);
    let candidate = RootSecretV1::new(VAULT_ID, 2, Secret32::from_array([0x6B; 32]));

    let primary = build_baseline(
        &mut relay,
        &candidate,
        [0x26; 16],
        candidate_s,
        2,
        [0x46; 32],
        &producer,
        0x01,
    );
    let fallback = build_baseline(
        &mut relay,
        &candidate,
        [0x27; 16],
        candidate_s,
        2,
        [0x42; 32],
        &producer,
        0x02,
    );

    let grant = build_self_grant(
        &old_manager,
        device_id,
        recipient_pub,
        2,
        // staging grant 在 activate 之前写入，AAD 里的 manager_rev 是**旧**管理 rev
        // （relay `stage_grant` 对 new rev 返回 CAS_CONFLICT），即 T.old_manager_rev=1。
        1,
        [0x53; 16],
        [0x54; 32],
        vec![0x57; 16],
    );
    let grants_digest =
        grant_set_digest(&[GrantLeafEvidence::new(device_id, &grant.signed_header)]);
    relay.self_grant = Some(grant.evidence());

    let mut view = view();
    view.freeze_seq = freeze_seq;
    view.primary_checkpoint = Some(primary.checkpoint_id);
    view.fallback_checkpoint = Some(fallback.checkpoint_id);
    view.grants_digest = Some(grants_digest);
    let primary_commit_digest: [u8; 32] = Sha256::digest(&primary.signed_commit).into();
    let fallback_commit_digest: [u8; 32] = Sha256::digest(&fallback.signed_commit).into();

    let activation_t = ActivationTV1 {
        vault_id: VAULT_ID,
        instance_id: relay.instance_id,
        rotation_id: view.rotation_id,
        from_epoch: 1,
        to_epoch: 2,
        freeze_seq,
        old_manager_rev: 1,
        new_manager_rev: 2,
        new_manager_pubkey: new_manager.public_key(),
        prev_control_hash: [0x5C; 32],
        expected_control_rev: 4,
        primary_cid: primary.checkpoint_id,
        primary_commit_digest,
        fallback_cid: fallback.checkpoint_id,
        fallback_commit_digest,
        slot_set_digest: [0x5D; 32],
        member_cert_digest: [0x5E; 32],
        grants_digest,
        old_asset_set_digest: view.freeze_asset_digest.expect("freeze asset digest"),
        new_asset_set_digest: [0x5F; 32],
    };
    let t_input = activation_t_signing_input(&activation_t).expect("T signing input");
    let evidence = VerifiedActivationT {
        activation_t,
        old_t_signature: old_manager.sign(&t_input).to_vec(),
        new_t_signature: new_manager.sign(&t_input).to_vec(),
    };

    // relay 键 20：由 view + 两个 checkpoint status 重建 `ActivationCommitV1` 后取 SHA256。
    let primary_status = relay
        .checkpoint_status
        .get(&primary.checkpoint_id)
        .expect("primary status")
        .clone();
    let fallback_status = relay
        .checkpoint_status
        .get(&fallback.checkpoint_id)
        .expect("fallback status")
        .clone();
    let commit = activation_commit_materials(
        VAULT_ID,
        relay.instance_id,
        &view,
        &primary_status,
        &fallback_status,
    )
    .expect("commit materials");
    let commit_hash: [u8; 32] = Sha256::digest(commit.encode().expect("commit encode")).into();
    view.activation_commit = Some(commit_hash.to_vec());
    relay.rotations.insert(view.rotation_id, view.clone());
    let chain = vec![EpochActivationLink {
        // 单轮：activation 记录在控制链 rev 4；其前一条（rev 3）hash 由 T 绑定。
        control_rev: 4,
        prev_control_hash: [0x5C; 32],
        view: view.clone(),
        evidence: evidence.clone(),
        activation_commit: commit.clone(),
    }];

    let bootstrap = BootstrapBundle {
        primary: BootstrapCheckpoint {
            checkpoint_id: primary.checkpoint_id,
            epoch: 2,
            covered_seq: primary.covered_seq,
            manifest_oid: primary.manifest_oid,
            refs_digest: None,
            cursor: primary.cursor.clone(),
            signed_commit: Some(primary.signed_commit.clone()),
            signature: Some(primary.signature),
        },
        fallback: BootstrapCheckpoint {
            checkpoint_id: fallback.checkpoint_id,
            epoch: 2,
            covered_seq: fallback.covered_seq,
            manifest_oid: fallback.manifest_oid,
            refs_digest: None,
            cursor: fallback.cursor.clone(),
            signed_commit: Some(fallback.signed_commit.clone()),
            signature: Some(fallback.signature),
        },
        head_seq: freeze_seq.max(candidate_s),
        gc_floor: 0,
        epoch: 2,
        lease: BootstrapLease {
            lease_id: [0xBF; 16],
            expires_at: 30_000,
            floor_seq: freeze_seq.max(candidate_s),
        },
        degraded: false,
    };

    let identity = menghunote_sync::identity::LocalIdentityV1 {
        vault_id: VAULT_ID,
        device_id,
        peer_id: menghunote_sync::ids::PeerId::from_bytes([0x65; 16]),
        ed25519_seed: Secret32::from_array([0x66; 32]),
        x25519_secret,
        current_epoch: 1,
        current_root: Secret32::from_array([0x68; 32]),
        manager_seed: None,
        pinned_instance: [0x22; 16],
        pinned_manager: old_manager.public_key(),
        control_rev: 3,
        pending_rotation: None,
    };
    let identity_path = dir.path().join("identity.bin");
    std::fs::write(&identity_path, identity.protect().expect("protect")).expect("write identity");

    ClientApplyFixture {
        _dir: dir,
        vault,
        relay: ActivationApplyRelay::new(relay, evidence, bootstrap, chain),
        identity,
        identity_path,
        candidate,
        old_manager,
        new_manager,
        device_id,
        recipient_pub,
        producer,
        view,
    }
}

fn run_client_apply(
    fixture: &mut ClientApplyFixture,
    checkpoint_id: [u8; 16],
    baseline_s: u64,
    baseline_cursor: &str,
) -> ApplyOutcome {
    apply_epoch_change_with_client(
        &mut fixture.relay,
        &fixture.vault,
        &fixture.identity_path,
        &fixture.identity,
        checkpoint_id,
        baseline_s,
        baseline_cursor,
        &fixture.candidate,
        fixture.old_manager.public_key(),
        fixture.new_manager.public_key(),
        2,
        None,
    )
    .expect("with client")
}

/// 门禁拒绝必须**不写库、不推 cursor**。
fn assert_not_applied(vault: &menghunote_storage::Vault) {
    let cursor = menghunote_storage::sync_cursor_state(vault).expect("cursor");
    assert_eq!(cursor.received_seq, 0, "不得推进 received 游标");
    assert_eq!(cursor.integrated_seq, 0, "不得推进 integrated 游标");
    assert!(
        menghunote_storage::baseline_apply_state(vault)
            .expect("baseline state")
            .is_none(),
        "不得写 baseline apply journal"
    );
}

/// 认证/门禁拒绝的**完整**零副作用断言：在 [`assert_not_applied`] 之上再加"没有进入
/// applying"——所有认证检查都必须在 `apply_epoch_change` → `begin_apply` 之前完成。
fn assert_no_baseline_apply(vault: &menghunote_storage::Vault) {
    assert_not_applied(vault);
    assert!(
        menghunote_sync::rotation_journal::RotationApplyPhase::load(vault)
            .expect("phase")
            .is_none(),
        "不得进入 applying（拒绝必须发生在 begin_apply 之前）"
    );
}

/// 身份文件（根/epoch/pin/管理权）快照；负例前后必须逐字节一致。
fn identity_snapshot(fixture: &ClientApplyFixture) -> Vec<u8> {
    std::fs::read(&fixture.identity_path).expect("read identity")
}

fn assert_identity_unchanged(fixture: &ClientApplyFixture, before: &[u8]) {
    assert_eq!(
        identity_snapshot(fixture),
        before,
        "身份文件必须不变（根/epoch/pin/管理权不得推广）"
    );
}

/// 写本地水位（`integrated_seq` + `confirmed_highwater`）：验证"基线 S 低于本地水位"被拒。
fn set_local_watermark(vault: &menghunote_storage::Vault, integrated: u64, highwater: u64) {
    vault
        .set_meta(
            menghunote_storage::META_SYNC_INTEGRATED_SEQ,
            &integrated.to_be_bytes(),
        )
        .expect("set integrated");
    menghunote_storage::set_confirmed_highwater(vault, highwater).expect("set highwater");
}

/// 构造一条**历史** activation 链环（只有公开 view/checkpoint 事实，不建真实基线对象）。
///
/// 链验证只需要：T 双签、T↔view 绑定、由 view + 两个 checkpoint 公开元数据重建的
/// `ActivationCommitV1`、以及控制链位置（rev + prev_control_hash）。中间环的 baseline
/// 与 grant **不需要**下载——latest baseline 覆盖历史。
#[allow(clippy::too_many_arguments)]
fn build_chain_link(
    relay: &mut FakeRelay,
    instance_id: [u8; 16],
    old_manager: &ManagementKey,
    new_manager: &ManagementKey,
    rotation_id: [u8; 16],
    from_epoch: u64,
    to_epoch: u64,
    control_rev: u64,
    prev_control_hash: [u8; 32],
    old_manager_rev: u64,
    new_manager_rev: u64,
    primary_cid: [u8; 16],
    fallback_cid: [u8; 16],
    primary_manifest: [u8; 32],
    fallback_manifest: [u8; 32],
    grants_digest: [u8; 32],
    asset_set_digest: [u8; 32],
) -> EpochActivationLink {
    for (cid, manifest) in [
        (primary_cid, primary_manifest),
        (fallback_cid, fallback_manifest),
    ] {
        relay.checkpoint_status.insert(
            cid,
            CheckpointStatus {
                checkpoint_id: cid,
                state: "published".to_owned(),
                role: None,
                covered_seq: 6,
                manifest_object_id: Some(manifest),
                refs_digest: Some([0x4A; 32]),
                candidate_ready: true,
                epoch: to_epoch,
            },
        );
    }
    let mut view = RotationView {
        rotation_id,
        state: "activated".to_owned(),
        from_epoch,
        to_epoch,
        freeze_seq: 6,
        revoked_count: 0,
        member_set_hash: Some([0x44; 32]),
        grants_count: 1,
        grants_digest: Some(grants_digest),
        published_checkpoints: 2,
        control_rev,
        auth_rev: 3,
        manager_rev: new_manager_rev,
        from_epoch_revoked_once: false,
        freeze_asset_count: 0,
        freeze_asset_digest: Some(asset_set_digest),
        member_cert_digest: Some([0x45; 32]),
        primary_checkpoint: Some(primary_cid),
        fallback_checkpoint: Some(fallback_cid),
        activation_commit: None,
        activated_at: Some(1_700_000_000),
        activated_control_rev: Some(control_rev),
        candidate_count: 0,
        candidate_ready: false,
        reservation_id: None,
    };
    let primary_status = relay
        .checkpoint_status
        .get(&primary_cid)
        .cloned()
        .expect("primary status");
    let fallback_status = relay
        .checkpoint_status
        .get(&fallback_cid)
        .cloned()
        .expect("fallback status");
    let commit = activation_commit_materials(
        VAULT_ID,
        instance_id,
        &view,
        &primary_status,
        &fallback_status,
    )
    .expect("历史环 commit materials");
    let commit_hash: [u8; 32] = Sha256::digest(commit.encode().expect("commit encode")).into();
    view.activation_commit = Some(commit_hash.to_vec());
    let activation_t = ActivationTV1 {
        vault_id: VAULT_ID,
        instance_id,
        rotation_id,
        from_epoch,
        to_epoch,
        freeze_seq: 6,
        old_manager_rev: u32::try_from(old_manager_rev).expect("old manager rev"),
        new_manager_rev: u32::try_from(new_manager_rev).expect("new manager rev"),
        new_manager_pubkey: new_manager.public_key(),
        prev_control_hash,
        expected_control_rev: control_rev,
        primary_cid,
        primary_commit_digest: [0xA1; 32],
        fallback_cid,
        fallback_commit_digest: [0xB2; 32],
        slot_set_digest: [0x38; 32],
        member_cert_digest: [0x39; 32],
        grants_digest,
        old_asset_set_digest: asset_set_digest,
        new_asset_set_digest: [0x3C; 32],
    };
    let t_input = activation_t_signing_input(&activation_t).expect("T signing input");
    let evidence = VerifiedActivationT {
        activation_t,
        old_t_signature: old_manager.sign(&t_input).to_vec(),
        new_t_signature: new_manager.sign(&t_input).to_vec(),
    };
    relay.rotations.insert(rotation_id, view.clone());
    EpochActivationLink {
        control_rev,
        prev_control_hash,
        view,
        evidence,
        activation_commit: commit,
    }
}

/// 构造 latest（E3）链环：真实主/备基线（`build_baseline` 已写入 status/manifest 对象）、
/// latest self grant 的 grants_digest，以及 T 里的 commit 摘要。
#[allow(clippy::too_many_arguments)]
fn build_latest_link(
    relay: &mut FakeRelay,
    instance_id: [u8; 16],
    old_manager: &ManagementKey,
    new_manager: &ManagementKey,
    rotation_id: [u8; 16],
    from_epoch: u64,
    to_epoch: u64,
    control_rev: u64,
    prev_control_hash: [u8; 32],
    old_manager_rev: u64,
    new_manager_rev: u64,
    primary: &TestBaseline,
    fallback: &TestBaseline,
    grants_digest: [u8; 32],
    asset_set_digest: [u8; 32],
    freeze_seq: u64,
) -> EpochActivationLink {
    let mut view = RotationView {
        rotation_id,
        state: "activated".to_owned(),
        from_epoch,
        to_epoch,
        freeze_seq,
        revoked_count: 0,
        member_set_hash: Some([0x44; 32]),
        grants_count: 1,
        grants_digest: Some(grants_digest),
        published_checkpoints: 2,
        control_rev,
        auth_rev: 3,
        manager_rev: new_manager_rev,
        from_epoch_revoked_once: false,
        freeze_asset_count: 0,
        freeze_asset_digest: Some(asset_set_digest),
        member_cert_digest: Some([0x45; 32]),
        primary_checkpoint: Some(primary.checkpoint_id),
        fallback_checkpoint: Some(fallback.checkpoint_id),
        activation_commit: None,
        activated_at: Some(1_700_000_000),
        activated_control_rev: Some(control_rev),
        candidate_count: 2,
        candidate_ready: true,
        reservation_id: None,
    };
    let primary_status = relay
        .checkpoint_status
        .get(&primary.checkpoint_id)
        .cloned()
        .expect("primary status");
    let fallback_status = relay
        .checkpoint_status
        .get(&fallback.checkpoint_id)
        .cloned()
        .expect("fallback status");
    let commit = activation_commit_materials(
        VAULT_ID,
        instance_id,
        &view,
        &primary_status,
        &fallback_status,
    )
    .expect("latest commit materials");
    let commit_hash: [u8; 32] = Sha256::digest(commit.encode().expect("commit encode")).into();
    view.activation_commit = Some(commit_hash.to_vec());
    let activation_t = ActivationTV1 {
        vault_id: VAULT_ID,
        instance_id,
        rotation_id,
        from_epoch,
        to_epoch,
        freeze_seq,
        old_manager_rev: u32::try_from(old_manager_rev).expect("old manager rev"),
        new_manager_rev: u32::try_from(new_manager_rev).expect("new manager rev"),
        new_manager_pubkey: new_manager.public_key(),
        prev_control_hash,
        expected_control_rev: control_rev,
        primary_cid: primary.checkpoint_id,
        primary_commit_digest: Sha256::digest(&primary.signed_commit).into(),
        fallback_cid: fallback.checkpoint_id,
        fallback_commit_digest: Sha256::digest(&fallback.signed_commit).into(),
        slot_set_digest: [0x38; 32],
        member_cert_digest: [0x39; 32],
        grants_digest,
        old_asset_set_digest: asset_set_digest,
        new_asset_set_digest: [0x3C; 32],
    };
    let t_input = activation_t_signing_input(&activation_t).expect("T signing input");
    let evidence = VerifiedActivationT {
        activation_t,
        old_t_signature: old_manager.sign(&t_input).to_vec(),
        new_t_signature: new_manager.sign(&t_input).to_vec(),
    };
    relay.rotations.insert(rotation_id, view.clone());
    EpochActivationLink {
        control_rev,
        prev_control_hash,
        view,
        evidence,
        activation_commit: commit,
    }
}

/// 离线跨多 epoch 的完整夹具：B 本地 E1，relay 已连续激活 E1→E2、E2→E3。
///
/// 链 = 两环：
/// - `E1→E2`（控制链 rev 4，旧/新管理 M1/M2）；
/// - `E2→E3`（rev 7，旧/新管理 M2/M3）。
///
/// latest grant/baseline 都属于 E3；中间环没有可下载的基线（验证链只需公开事实）。
struct MultiEpochFixture {
    _dir: tempfile::TempDir,
    vault: menghunote_storage::Vault,
    relay: ActivationApplyRelay,
    identity: menghunote_sync::identity::LocalIdentityV1,
    identity_path: std::path::PathBuf,
    candidate: RootSecretV1,
    old_manager: ManagementKey,
    middle_manager: ManagementKey,
    new_manager: ManagementKey,
    device_id: DeviceId,
    producer: Device,
    latest_view: RotationView,
    chain: Vec<EpochActivationLink>,
}

fn multi_epoch_client_fixture() -> MultiEpochFixture {
    let (dir, vault) = fresh_vault();
    let mut producer = Device::new(0xA5);
    producer.vault_id = VAULT_ID;
    let mut relay = FakeRelay::new(&[&producer]);
    relay.vault_id = VAULT_ID;
    relay.epoch = 3;
    relay.control_rev = 7;
    relay.head_seq = 6;
    relay.control_kinds = vec![
        (4, "epoch.activated".to_owned()),
        (7, "epoch.activated".to_owned()),
    ];
    let instance_id = relay.instance_id;

    let old_manager =
        ManagementKey::from_recovery(&RecoverySeed::from_array([0x51; 32]), &VAULT_ID)
            .expect("old manager");
    let middle_manager =
        ManagementKey::from_recovery(&RecoverySeed::from_array([0x5B; 32]), &VAULT_ID)
            .expect("middle manager");
    let new_manager =
        ManagementKey::from_recovery(&RecoverySeed::from_array([0x6D; 32]), &VAULT_ID)
            .expect("new manager");
    let device_id: DeviceId = [0x64; 16];
    let x25519_secret = Secret32::from_array([0x67; 32]);
    let recipient_pub = x25519_public_key(&x25519_secret);
    let candidate = RootSecretV1::new(VAULT_ID, 3, Secret32::from_array([0x6B; 32]));

    // 第一环：E1→E2，管理 M1→M2。
    let link2 = build_chain_link(
        &mut relay,
        instance_id,
        &old_manager,
        &middle_manager,
        [0x21; 16],
        1,
        2,
        4,
        [0x61; 32],
        1,
        2,
        [0x45; 16],
        [0x46; 16],
        [0x47; 32],
        [0x48; 32],
        [0x71; 32],
        [0x72; 32],
    );

    // latest E3：真实主/备基线 + latest self grant（外签 = E3 的旧管理 M2）。
    let primary = build_baseline(
        &mut relay, &candidate, [0x36; 16], 6, 3, [0x4B; 32], &producer, 0x01,
    );
    let fallback = build_baseline(
        &mut relay, &candidate, [0x37; 16], 6, 3, [0x4D; 32], &producer, 0x02,
    );
    let grant = build_self_grant(
        &middle_manager,
        device_id,
        recipient_pub,
        3,
        // latest（E3）staging grant 的 AAD 是 E3 的旧管理 rev（M2 的 rev=2），
        // 不是推广目标 rev=3。
        2,
        [0x53; 16],
        [0x54; 32],
        vec![0x57; 16],
    );
    let grants_digest =
        grant_set_digest(&[GrantLeafEvidence::new(device_id, &grant.signed_header)]);
    relay.self_grant = Some(grant.evidence());

    // 第二环：E2→E3，管理 M2→M3。
    let link3 = build_latest_link(
        &mut relay,
        instance_id,
        &middle_manager,
        &new_manager,
        [0x31; 16],
        2,
        3,
        7,
        [0x73; 32],
        2,
        3,
        &primary,
        &fallback,
        grants_digest,
        [0x74; 32],
        6,
    );
    let evidence = link3.evidence.clone();
    let latest_view = link3.view.clone();

    let bootstrap = BootstrapBundle {
        primary: BootstrapCheckpoint {
            checkpoint_id: primary.checkpoint_id,
            epoch: 3,
            covered_seq: primary.covered_seq,
            manifest_oid: primary.manifest_oid,
            refs_digest: None,
            cursor: primary.cursor.clone(),
            signed_commit: Some(primary.signed_commit.clone()),
            signature: Some(primary.signature),
        },
        fallback: BootstrapCheckpoint {
            checkpoint_id: fallback.checkpoint_id,
            epoch: 3,
            covered_seq: fallback.covered_seq,
            manifest_oid: fallback.manifest_oid,
            refs_digest: None,
            cursor: fallback.cursor.clone(),
            signed_commit: Some(fallback.signed_commit.clone()),
            signature: Some(fallback.signature),
        },
        head_seq: 6,
        gc_floor: 0,
        epoch: 3,
        lease: BootstrapLease {
            lease_id: [0xBF; 16],
            expires_at: 30_000,
            floor_seq: 6,
        },
        degraded: false,
    };

    let identity = menghunote_sync::identity::LocalIdentityV1 {
        vault_id: VAULT_ID,
        device_id,
        peer_id: menghunote_sync::ids::PeerId::from_bytes([0x65; 16]),
        ed25519_seed: Secret32::from_array([0x66; 32]),
        x25519_secret,
        // B 离线时仍停在 E1。
        current_epoch: 1,
        current_root: Secret32::from_array([0x68; 32]),
        manager_seed: None,
        pinned_instance: [0x22; 16],
        pinned_manager: old_manager.public_key(),
        control_rev: 3,
        pending_rotation: None,
    };
    let identity_path = dir.path().join("identity.bin");
    std::fs::write(&identity_path, identity.protect().expect("protect")).expect("write identity");

    let mut relay = ActivationApplyRelay::new(
        relay,
        evidence,
        bootstrap,
        vec![link2.clone(), link3.clone()],
    );
    relay.hide_current_rotation = true;

    MultiEpochFixture {
        _dir: dir,
        vault,
        relay,
        identity,
        identity_path,
        candidate,
        old_manager,
        middle_manager,
        new_manager,
        device_id,
        producer,
        latest_view,
        chain: vec![link2, link3],
    }
}

fn run_multi_apply(
    fixture: &mut MultiEpochFixture,
    checkpoint_id: [u8; 16],
    baseline_s: u64,
    baseline_cursor: &str,
) -> ApplyOutcome {
    run_multi_apply_with_manager(
        fixture,
        checkpoint_id,
        baseline_s,
        baseline_cursor,
        fixture.old_manager.public_key(),
    )
}

/// 多 epoch 链尾（E3）用"普通健康 checkpoint 替换激活 pair"的 `/bootstrap` 视图：
/// 链/ latest T 不变，只是当前 pair 的 cid 不再出现在 T 里。
fn multi_install_post_activation_pair(fixture: &mut MultiEpochFixture, s: u64) -> [u8; 16] {
    let primary = build_baseline(
        &mut fixture.relay.inner,
        &fixture.candidate,
        [0x3A; 16],
        s,
        3,
        [0x5E; 32],
        &fixture.producer,
        0x06,
    );
    let fallback = build_baseline(
        &mut fixture.relay.inner,
        &fixture.candidate,
        [0x3B; 16],
        s,
        3,
        [0x5F; 32],
        &fixture.producer,
        0x07,
    );
    let bundle = fixture.relay.bootstrap.as_mut().expect("bootstrap");
    bundle.primary = BootstrapCheckpoint {
        checkpoint_id: primary.checkpoint_id,
        epoch: 3,
        covered_seq: primary.covered_seq,
        manifest_oid: primary.manifest_oid,
        refs_digest: None,
        cursor: primary.cursor.clone(),
        signed_commit: Some(primary.signed_commit.clone()),
        signature: Some(primary.signature),
    };
    bundle.fallback = BootstrapCheckpoint {
        checkpoint_id: fallback.checkpoint_id,
        epoch: 3,
        covered_seq: fallback.covered_seq,
        manifest_oid: fallback.manifest_oid,
        refs_digest: None,
        cursor: fallback.cursor.clone(),
        signed_commit: Some(fallback.signed_commit.clone()),
        signature: Some(fallback.signature),
    };
    bundle.head_seq = bundle.head_seq.max(s);
    fixture.relay.inner.head_seq = fixture.relay.inner.head_seq.max(s);
    primary.checkpoint_id
}

fn run_multi_apply_with_manager(
    fixture: &mut MultiEpochFixture,
    checkpoint_id: [u8; 16],
    baseline_s: u64,
    baseline_cursor: &str,
    manager_public: [u8; 32],
) -> ApplyOutcome {
    apply_epoch_change_with_client(
        &mut fixture.relay,
        &fixture.vault,
        &fixture.identity_path,
        &fixture.identity,
        checkpoint_id,
        baseline_s,
        baseline_cursor,
        &fixture.candidate,
        manager_public,
        fixture.new_manager.public_key(),
        3,
        None,
    )
    .expect("multi-epoch with client")
}

#[test]
fn offline_across_multiple_epochs_verifies_chain_and_applies_latest() {
    ensure_test_identity_wrap();
    // 评审 0003 追加的实码缺陷：B 本地 E1，relay 已完成 1→2、2→3。
    // latest T(E3) 的 from=2 != 本地 1，但整条链 E1→E2→E3 可逐环验证：
    // 必须用链尾 T 的 grant/baseline 完成 apply，并把身份推广到 E3。
    let mut fixture = multi_epoch_client_fixture();
    assert_eq!(fixture.chain.len(), 2, "夹具必须是两环链");
    assert_eq!(fixture.identity.current_epoch, 1);
    assert_eq!(
        fixture.chain[1].evidence.activation_t.from_epoch, 2,
        "latest T 的 from 不是本地 epoch，正是缺陷触发条件"
    );
    let outcome = run_multi_apply(&mut fixture, [0x36; 16], 6, "c6");
    match outcome {
        ApplyOutcome::Applied {
            baseline_s: 6,
            promoted: true,
        } => {}
        other => panic!("多轮离线必须由链尾授权并 Applied，实际 {other:?}"),
    }
    let promoted = menghunote_sync::identity::LocalIdentityV1::unprotect(
        &std::fs::read(&fixture.identity_path).expect("read identity"),
        &VAULT_ID,
        &fixture.device_id,
    )
    .expect("promoted identity");
    assert_eq!(promoted.current_epoch, 3, "身份必须一次性推广到链尾 epoch");
    assert_eq!(promoted.pinned_manager, fixture.new_manager.public_key());
    assert_eq!(promoted.control_rev, 7, "control_rev 必须是链尾激活值");
    assert_eq!(fixture.latest_view.to_epoch, 3);
}

#[test]
fn offline_device_already_at_middle_epoch_applies_only_new_tail() {
    ensure_test_identity_wrap();
    // 已推广到 E2 的设备再遇 E3：客户端按 from_epoch 过滤后只剩 E2→E3 一环，
    // 链首锚点 = E2 的管理 key（M2），仍然只能由链尾 T 授权。
    let mut fixture = multi_epoch_client_fixture();
    fixture.identity.current_epoch = 2;
    fixture.identity.pinned_manager = fixture.middle_manager.public_key();
    // 真实设备推广到 E2 时旁路状态一定已落盘（rename 之前写）；这里补齐，
    // 否则门禁无法把链首的 T.old_manager_rev 锚定到本地管理链位置（会 fail-closed）。
    menghunote_sync::identity::IdentityRotationStateV1 {
        current_epoch: 2,
        control_rev: 4,
        manager_rev: 2,
        previous_roots: vec![menghunote_sync::identity::HistoricalRootV1::new(
            1,
            menghunote_sync::keys::Secret32::from_array(*fixture.identity.current_root.expose()),
        )],
    }
    .save(&fixture.vault, &VAULT_ID, &fixture.device_id)
    .expect("save rotation state");
    let manager_public = fixture.middle_manager.public_key();
    let outcome = run_multi_apply_with_manager(&mut fixture, [0x36; 16], 6, "c6", manager_public);
    match outcome {
        ApplyOutcome::Applied {
            baseline_s: 6,
            promoted: true,
        } => {}
        other => panic!("已到 E2 的设备必须能应用 E3，实际 {other:?}"),
    }
    let promoted = menghunote_sync::identity::LocalIdentityV1::unprotect(
        &std::fs::read(&fixture.identity_path).expect("read identity"),
        &VAULT_ID,
        &fixture.device_id,
    )
    .expect("promoted identity");
    assert_eq!(promoted.current_epoch, 3);
    assert_eq!(promoted.pinned_manager, fixture.new_manager.public_key());
}

#[test]
fn offline_multi_epoch_missing_middle_activation_is_rejected() {
    ensure_test_identity_wrap();
    // 缺"创建 E2"的那一环（E1→E2）：链首 from=2 而本地仍是 1 → 拒绝，不写库。
    let mut fixture = multi_epoch_client_fixture();
    fixture.relay.chain.as_mut().expect("chain").remove(0);
    let outcome = run_multi_apply(&mut fixture, [0x36; 16], 6, "c6");
    match outcome {
        ApplyOutcome::Paused { reason } => {
            assert!(reason.contains("链首"), "reason={reason}")
        }
        other => panic!("缺中间激活必须 Paused，实际 {other:?}"),
    }
    assert_not_applied(&fixture.vault);
}

#[test]
fn offline_multi_epoch_bad_intermediate_t_signature_is_rejected() {
    ensure_test_identity_wrap();
    // E1→E2 的 T 旧管理签名坏：链首之后逐环验签必须失败。
    let mut fixture = multi_epoch_client_fixture();
    fixture.relay.chain.as_mut().expect("chain")[0]
        .evidence
        .old_t_signature[0] ^= 0x01;
    let outcome = run_multi_apply(&mut fixture, [0x36; 16], 6, "c6");
    match outcome {
        ApplyOutcome::Paused { reason } => {
            assert!(reason.contains("签名"), "reason={reason}")
        }
        other => panic!("中间环 T 签名坏必须 Paused，实际 {other:?}"),
    }
    assert_not_applied(&fixture.vault);
}

#[test]
fn offline_multi_epoch_chain_head_must_equal_local_epoch() {
    ensure_test_identity_wrap();
    // 本地 epoch 不在链首（这里以 0 模拟链首缺失）：不得把中间环当起点。
    let mut fixture = multi_epoch_client_fixture();
    fixture.identity.current_epoch = 0;
    let outcome = run_multi_apply(&mut fixture, [0x36; 16], 6, "c6");
    match outcome {
        ApplyOutcome::Paused { reason } => {
            assert!(reason.contains("链首"), "reason={reason}")
        }
        other => panic!("链首不在本机 epoch 必须 Paused，实际 {other:?}"),
    }
    assert_not_applied(&fixture.vault);
}

#[test]
fn offline_multi_epoch_latest_t_must_be_chain_tail() {
    ensure_test_identity_wrap();
    // 截断链尾（只剩 E1→E2）但 latest T 是 E3：链尾 != latest → 拒绝。
    let mut fixture = multi_epoch_client_fixture();
    fixture.relay.chain.as_mut().expect("chain").pop();
    let outcome = run_multi_apply(&mut fixture, [0x36; 16], 6, "c6");
    match outcome {
        ApplyOutcome::Paused { reason } => {
            assert!(reason.contains("链尾"), "reason={reason}")
        }
        other => panic!("latest T 不在链尾必须 Paused，实际 {other:?}"),
    }
    assert_not_applied(&fixture.vault);
}

#[test]
fn offline_multi_epoch_prev_control_hash_must_match_chain_position() {
    ensure_test_identity_wrap();
    // 链上位置（rev 7 的前一条 record_hash）与 T.prev_control_hash 不一致 → 拒绝。
    let mut fixture = multi_epoch_client_fixture();
    fixture.relay.chain.as_mut().expect("chain")[1].prev_control_hash[0] ^= 0x01;
    let outcome = run_multi_apply(&mut fixture, [0x36; 16], 6, "c6");
    match outcome {
        ApplyOutcome::Paused { reason } => {
            assert!(reason.contains("prev_control_hash"), "reason={reason}")
        }
        other => panic!("prev_control_hash 与链位置不符必须 Paused，实际 {other:?}"),
    }
    assert_not_applied(&fixture.vault);
}

#[test]
fn with_client_runs_state_view_control_grant_manifest_and_promotes() {
    ensure_test_identity_wrap();
    let mut fixture = client_apply_fixture();
    let outcome = run_client_apply(&mut fixture, [0x26; 16], 6, "c6");
    match outcome {
        ApplyOutcome::Applied { baseline_s: 6, .. } => {}
        other => panic!("期望 Applied，实际 {other:?}"),
    }
    // 推广后本地身份必须指向候选 epoch / 新管理 key。
    let promoted = menghunote_sync::identity::LocalIdentityV1::unprotect(
        &std::fs::read(&fixture.identity_path).expect("read identity"),
        &VAULT_ID,
        &fixture.device_id,
    )
    .expect("promoted identity");
    assert_eq!(promoted.current_epoch, 2);
    assert_eq!(promoted.pinned_manager, fixture.new_manager.public_key());
}

#[test]
fn with_client_accepts_full_grant_leaf_set_inclusion() {
    ensure_test_identity_wrap();
    let mut fixture = client_apply_fixture();
    let own_leaf = GrantLeafEvidence::new(
        fixture.device_id,
        &fixture
            .relay
            .inner
            .self_grant
            .as_ref()
            .expect("self grant")
            .signed_header,
    );
    fixture.relay.leaves = Some(vec![own_leaf]);
    let outcome = run_client_apply(&mut fixture, [0x26; 16], 6, "c6");
    match outcome {
        ApplyOutcome::Applied { baseline_s: 6, .. } => {}
        other => panic!("完整 leaf inclusion 应 Applied，实际 {other:?}"),
    }
}

#[test]
fn state_without_current_rotation_still_applies_activated_epoch() {
    ensure_test_identity_wrap();
    let mut fixture = client_apply_fixture();
    // `/state` 激活后不再返回 current_rotation_id（只对 frozen/staging 返回）；
    // 客户端必须用 state.epoch + current_rotation + 控制链 epoch.activated 判定。
    fixture.relay.hide_current_rotation = true;
    let outcome = run_client_apply(&mut fixture, [0x26; 16], 6, "c6");
    match outcome {
        ApplyOutcome::Applied { baseline_s: 6, .. } => {}
        other => panic!("期望 Applied，实际 {other:?}"),
    }
}

#[test]
fn with_client_rejects_missing_activation_t_without_writing() {
    ensure_test_identity_wrap();
    let mut fixture = client_apply_fixture();
    fixture.relay.evidence = None;
    let outcome = run_client_apply(&mut fixture, [0x26; 16], 6, "c6");
    match outcome {
        ApplyOutcome::Paused { reason } => {
            assert!(reason.contains("activation T"), "reason={reason}")
        }
        other => panic!("缺 T 必须 Paused，实际 {other:?}"),
    }
    assert_not_applied(&fixture.vault);
}

#[test]
fn with_client_rejects_unverified_activation_t_signature() {
    ensure_test_identity_wrap();
    let mut fixture = client_apply_fixture();
    // T 的内容没变，但旧管理签名被换 → 本地复算双签必须失败（不信 relay "已验证"自报）。
    fixture
        .relay
        .evidence
        .as_mut()
        .expect("T evidence")
        .old_t_signature[0] ^= 0x01;
    fixture.sync_chain_tail();
    let outcome = run_client_apply(&mut fixture, [0x26; 16], 6, "c6");
    match outcome {
        ApplyOutcome::Paused { reason } => {
            assert!(reason.contains("签名"), "reason={reason}")
        }
        other => panic!("T 双签未通过必须 Paused，实际 {other:?}"),
    }
    assert_not_applied(&fixture.vault);
}

#[test]
fn with_client_rejects_tampered_activation_commit_hash() {
    ensure_test_identity_wrap();
    let mut fixture = client_apply_fixture();
    fixture.view.activation_commit = Some(vec![0xEE; 32]);
    fixture.reinsert_view();
    let outcome = run_client_apply(&mut fixture, [0x26; 16], 6, "c6");
    match outcome {
        ApplyOutcome::Paused { reason } => {
            assert!(reason.contains("activation_commit"), "reason={reason}")
        }
        other => panic!("必须 Paused，实际 {other:?}"),
    }
    assert_not_applied(&fixture.vault);
}

#[test]
fn with_client_rejects_tampered_activation_materials() {
    ensure_test_identity_wrap();
    // rid / from_epoch / S 任一被改：重建出的 CDE 与 relay 的 SHA256 不再一致。
    for tamper in 0..3 {
        let mut fixture = client_apply_fixture();
        match tamper {
            0 => fixture.view.rotation_id = [0x99; 16],
            1 => fixture.view.from_epoch = 3,
            _ => fixture.view.freeze_seq = 7,
        }
        fixture.reinsert_view();
        let outcome = run_client_apply(&mut fixture, [0x26; 16], 6, "c6");
        match outcome {
            ApplyOutcome::Paused { reason } => assert!(
                reason.contains("activation_commit")
                    || reason.contains("绑定")
                    || reason.contains("epoch/S"),
                "tamper={tamper} reason={reason}"
            ),
            other => panic!("tamper={tamper} 必须 Paused，实际 {other:?}"),
        }
        assert_not_applied(&fixture.vault);
    }
}

#[test]
fn with_client_rejects_missing_or_wrong_epoch_activated_control_record() {
    ensure_test_identity_wrap();
    for control in [
        vec![(4, "rotation.begin".to_owned())],
        vec![(3, "epoch.activated".to_owned())],
        Vec::new(),
    ] {
        let mut fixture = client_apply_fixture();
        fixture.relay.inner.control_kinds = control.clone();
        let outcome = run_client_apply(&mut fixture, [0x26; 16], 6, "c6");
        match outcome {
            ApplyOutcome::Paused { reason } => {
                assert!(
                    reason.contains("epoch.activated"),
                    "control={control:?} reason={reason}"
                )
            }
            other => panic!("control={control:?} 必须 Paused，实际 {other:?}"),
        }
        assert_not_applied(&fixture.vault);
    }
}

#[test]
fn post_activation_checkpoint_outside_t_pair_is_applied() {
    ensure_test_identity_wrap();
    // 评审 0003 P1-C 的核心行为翻转：激活后 relay 用**普通健康 checkpoint** 替换激活
    // pair（`checkpoints::prune_retired` 会 prune 旧 pair/清 refs），旧设备（仍是 E1 身份）
    // 必须仍能 apply。旧行为要求 `cid ∈ T.pair`，两集合不相交 → 任何选择都 Paused。
    let mut fixture = client_apply_fixture();
    let (primary, fallback) = fixture.install_post_activation_pair(7);
    let t = &fixture
        .relay
        .evidence
        .as_ref()
        .expect("T evidence")
        .activation_t;
    assert_ne!(
        primary, t.primary_cid,
        "新 pair 的 cid 不得等于 T 的 primary"
    );
    assert_ne!(
        primary, t.fallback_cid,
        "新 pair 的 cid 不得等于 T 的 fallback"
    );
    assert_ne!(fallback, t.primary_cid);
    {
        // `/bootstrap` 只剩新 pair：T 点名的旧 pair 已被 prune 掉。
        let bundle = fixture.relay.bootstrap.as_ref().expect("bootstrap");
        assert_eq!(bundle.primary.checkpoint_id, primary);
        assert_eq!(bundle.fallback.checkpoint_id, fallback);
    }
    let outcome = run_client_apply(&mut fixture, primary, 7, "c7");
    match outcome {
        ApplyOutcome::Applied {
            baseline_s: 7,
            promoted: true,
        } => {}
        other => panic!("激活后普通健康 checkpoint 必须可 apply，实际 {other:?}"),
    }
    // receipt 必须记的是这份新基线（不是 T 里的 pair，也不是旧全量基线）。
    let receipt = menghunote_sync::rotation_journal::RotationApplyReceipt::load(&fixture.vault)
        .expect("receipt")
        .expect("apply receipt 必须存在");
    assert_eq!(receipt.checkpoint_id(), primary, "receipt.cid 必须是新基线");
    assert_eq!(receipt.baseline_s(), 7, "receipt.S 必须是新基线 S");
    assert_eq!(
        receipt.baseline_cursor(),
        "c7",
        "receipt.cursor 必须是该 S 的游标"
    );
    // 身份推广到 E2，根换成候选根。
    let promoted = menghunote_sync::identity::LocalIdentityV1::unprotect(
        &std::fs::read(&fixture.identity_path).expect("read identity"),
        &VAULT_ID,
        &fixture.device_id,
    )
    .expect("promoted identity");
    assert_eq!(promoted.current_epoch, 2);
    assert_eq!(
        promoted.current_root.expose(),
        fixture.candidate.root.expose()
    );
    // 本地水位/cursor 推到新基线 S。
    let cursor = menghunote_storage::sync_cursor_state(&fixture.vault).expect("cursor");
    assert_eq!(cursor.integrated_seq, 7);
    assert_eq!(
        fixture
            .vault
            .get_meta("sync.relay_cursor")
            .expect("meta")
            .as_deref(),
        Some(b"c7".as_slice())
    );
}

#[test]
fn post_activation_checkpoint_with_inactive_producer_is_rejected() {
    ensure_test_identity_wrap();
    // 非 pair 路径唯一的内容授权是"生产者是已验证控制链里的 active 成员"：
    // 生产者被吊销后，即使 commit 签名仍有效、manifest/refs 全对，也必须拒绝。
    let mut fixture = client_apply_fixture();
    let (primary, _) = fixture.install_post_activation_pair(7);
    fixture.revoke_producer();
    let before = identity_snapshot(&fixture);
    let outcome = run_client_apply(&mut fixture, primary, 7, "c7");
    match outcome {
        ApplyOutcome::Paused { reason } => assert!(
            reason.contains("active") || reason.contains("成员"),
            "reason={reason}"
        ),
        other => panic!("非 active 生产者必须 Paused，实际 {other:?}"),
    }
    assert_no_baseline_apply(&fixture.vault);
    assert_identity_unchanged(&fixture, &before);
}

#[test]
fn post_activation_checkpoint_with_unregistered_producer_is_rejected() {
    ensure_test_identity_wrap();
    let mut fixture = client_apply_fixture();
    let (primary, _) = fixture.install_post_activation_pair(7);
    // 控制链里根本没有该设备证书：不能"证书缺失即通过"。
    fixture
        .relay
        .inner
        .certs
        .remove(&fixture.producer.device_id);
    let before = identity_snapshot(&fixture);
    let outcome = run_client_apply(&mut fixture, primary, 7, "c7");
    match outcome {
        ApplyOutcome::Paused { reason } => assert!(
            reason.contains("生产者") && reason.contains("证书"),
            "reason={reason}"
        ),
        other => panic!("缺生产者证书必须 Paused，实际 {other:?}"),
    }
    assert_no_baseline_apply(&fixture.vault);
    assert_identity_unchanged(&fixture, &before);
}

#[test]
fn post_activation_checkpoint_with_bad_commit_signature_is_rejected() {
    ensure_test_identity_wrap();
    let mut fixture = client_apply_fixture();
    let (primary, _) = fixture.install_post_activation_pair(7);
    {
        let bundle = fixture.relay.bootstrap.as_mut().expect("bootstrap");
        let signature = bundle.primary.signature.as_mut().expect("signature");
        signature[0] ^= 0x01;
    }
    let before = identity_snapshot(&fixture);
    let outcome = run_client_apply(&mut fixture, primary, 7, "c7");
    match outcome {
        ApplyOutcome::Paused { reason } => assert!(reason.contains("签名"), "reason={reason}"),
        other => panic!("错签 commit 必须 Paused，实际 {other:?}"),
    }
    assert_no_baseline_apply(&fixture.vault);
    assert_identity_unchanged(&fixture, &before);
}

#[test]
fn post_activation_checkpoint_with_wrong_epoch_is_rejected() {
    ensure_test_identity_wrap();
    // 签名自洽但 commit 声明 epoch=3，而 activation 目标是 2：不能把 E3 的内容用 E2 根写。
    let mut fixture = client_apply_fixture();
    let (primary, _) = fixture.install_post_activation_pair(7);
    fixture.rewrite_bootstrap_commit(|commit| commit.epoch = 3);
    let before = identity_snapshot(&fixture);
    let outcome = run_client_apply(&mut fixture, primary, 7, "c7");
    match outcome {
        ApplyOutcome::Paused { reason } => assert!(reason.contains("epoch"), "reason={reason}"),
        other => panic!("错 epoch 必须 Paused，实际 {other:?}"),
    }
    assert_no_baseline_apply(&fixture.vault);
    assert_identity_unchanged(&fixture, &before);
}

#[test]
fn post_activation_checkpoint_below_gc_floor_is_rejected() {
    ensure_test_identity_wrap();
    // 低于 GC floor 的游标已不可继续：从它续读必缺洞。
    let mut fixture = client_apply_fixture();
    let (primary, _) = fixture.install_post_activation_pair(7);
    fixture
        .relay
        .bootstrap
        .as_mut()
        .expect("bootstrap")
        .gc_floor = 8;
    let before = identity_snapshot(&fixture);
    let outcome = run_client_apply(&mut fixture, primary, 7, "c7");
    match outcome {
        ApplyOutcome::Paused { reason } => assert!(reason.contains("GC floor"), "reason={reason}"),
        other => panic!("S < gc_floor 必须 Paused，实际 {other:?}"),
    }
    assert_no_baseline_apply(&fixture.vault);
    assert_identity_unchanged(&fixture, &before);
}

#[test]
fn post_activation_checkpoint_above_relay_head_is_rejected() {
    ensure_test_identity_wrap();
    // 不得消费 relay 尚未产生的更新（`/bootstrap` 与 `/state` 取更严格一侧）。
    let mut fixture = client_apply_fixture();
    let (primary, _) = fixture.install_post_activation_pair(7);
    fixture
        .relay
        .bootstrap
        .as_mut()
        .expect("bootstrap")
        .head_seq = 6;
    let before = identity_snapshot(&fixture);
    let outcome = run_client_apply(&mut fixture, primary, 7, "c7");
    match outcome {
        ApplyOutcome::Paused { reason } => assert!(reason.contains("head_seq"), "reason={reason}"),
        other => panic!("S > head 必须 Paused，实际 {other:?}"),
    }
    assert_no_baseline_apply(&fixture.vault);
    assert_identity_unchanged(&fixture, &before);
}

#[test]
fn post_activation_checkpoint_below_local_watermark_is_rejected() {
    ensure_test_identity_wrap();
    // 本地已 integrated 到 8，却用覆盖 S=7 的基线做 promotion：receipt 的 S 会落后于
    // 实际消费位置（随后 pull 重复/跳段），必须拒绝。
    let mut fixture = client_apply_fixture();
    let (primary, _) = fixture.install_post_activation_pair(7);
    set_local_watermark(&fixture.vault, 8, 4);
    let before = identity_snapshot(&fixture);
    let outcome = run_client_apply(&mut fixture, primary, 7, "c7");
    match outcome {
        ApplyOutcome::Paused { reason } => assert!(reason.contains("水位"), "reason={reason}"),
        other => panic!("S < 本地水位必须 Paused，实际 {other:?}"),
    }
    // 本地水位本来就 > 0：断言"没有被改动"而不是"仍为 0"。
    let cursor = menghunote_storage::sync_cursor_state(&fixture.vault).expect("cursor");
    assert_eq!(cursor.received_seq, 0, "不得推进 received 游标");
    assert_eq!(cursor.integrated_seq, 8, "不得改动本地 integrated 水位");
    assert_eq!(
        cursor.confirmed_highwater,
        Some(4),
        "不得改动 confirmed_highwater"
    );
    assert!(
        menghunote_storage::baseline_apply_state(&fixture.vault)
            .expect("baseline state")
            .is_none(),
        "不得写 baseline apply journal"
    );
    assert!(
        menghunote_sync::rotation_journal::RotationApplyPhase::load(&fixture.vault)
            .expect("phase")
            .is_none(),
        "不得进入 applying"
    );
    assert_identity_unchanged(&fixture, &before);
}

#[test]
fn post_activation_checkpoint_with_bad_refs_digest_is_rejected() {
    ensure_test_identity_wrap();
    // refs_digest 必须由 manifest 复算：签名 commit 覆盖的对象集合不能与 manifest 脱节。
    let mut fixture = client_apply_fixture();
    let (primary, _) = fixture.install_post_activation_pair(7);
    fixture.rewrite_bootstrap_commit(|commit| commit.refs_digest = [0x77; 32]);
    let before = identity_snapshot(&fixture);
    let outcome = run_client_apply(&mut fixture, primary, 7, "c7");
    match outcome {
        ApplyOutcome::Paused { reason } => {
            assert!(reason.contains("refs_digest"), "reason={reason}")
        }
        other => panic!("坏 refs_digest 必须 Paused，实际 {other:?}"),
    }
    assert_no_baseline_apply(&fixture.vault);
    assert_identity_unchanged(&fixture, &before);
}

#[test]
fn post_activation_checkpoint_missing_manifest_parts_is_rejected() {
    ensure_test_identity_wrap();
    // manifest 对象分片被 GC/prune 掉：`part()` 报错（可重试），本地零写入。
    let mut fixture = client_apply_fixture();
    let (primary, _) = fixture.install_post_activation_pair(7);
    let manifest_oid = fixture
        .relay
        .bootstrap
        .as_ref()
        .expect("bootstrap")
        .primary
        .manifest_oid;
    fixture
        .relay
        .inner
        .parts
        .retain(|(object_id, _), _| *object_id != manifest_oid);
    let before = identity_snapshot(&fixture);
    let error = apply_epoch_change_with_client(
        &mut fixture.relay,
        &fixture.vault,
        &fixture.identity_path,
        &fixture.identity,
        primary,
        7,
        "c7",
        &fixture.candidate,
        fixture.old_manager.public_key(),
        fixture.new_manager.public_key(),
        2,
        None,
    )
    .expect_err("缺 manifest 分片必须报错");
    assert!(
        matches!(error, SyncError::RelayApi { .. } | SyncError::Object(_)),
        "意外错误：{error:?}"
    );
    assert_no_baseline_apply(&fixture.vault);
    assert_identity_unchanged(&fixture, &before);
}

#[test]
fn post_activation_checkpoint_state_not_published_is_rejected() {
    ensure_test_identity_wrap();
    // staging/candidate/aborted/pruned 都不是"当前合法基线"。
    for state in ["candidate", "staging", "aborted", "pruned"] {
        let mut fixture = client_apply_fixture();
        let (primary, _) = fixture.install_post_activation_pair(7);
        fixture
            .relay
            .inner
            .checkpoint_status
            .get_mut(&primary)
            .expect("status")
            .state = state.to_owned();
        let before = identity_snapshot(&fixture);
        let outcome = run_client_apply(&mut fixture, primary, 7, "c7");
        match outcome {
            ApplyOutcome::Paused { reason } => assert!(
                reason.contains("published"),
                "state={state} reason={reason}"
            ),
            other => panic!("state={state} 必须 Paused，实际 {other:?}"),
        }
        assert_no_baseline_apply(&fixture.vault);
        assert_identity_unchanged(&fixture, &before);
    }
}

#[test]
fn post_activation_checkpoint_with_degraded_bootstrap_is_rejected() {
    ensure_test_identity_wrap();
    // 降级 bootstrap 只允许显式请求的诊断用途，绝不能作为根推广的授权材料。
    let mut fixture = client_apply_fixture();
    let (primary, _) = fixture.install_post_activation_pair(7);
    fixture
        .relay
        .bootstrap
        .as_mut()
        .expect("bootstrap")
        .degraded = true;
    let before = identity_snapshot(&fixture);
    let outcome = run_client_apply(&mut fixture, primary, 7, "c7");
    match outcome {
        ApplyOutcome::Paused { reason } => assert!(reason.contains("degraded"), "reason={reason}"),
        other => panic!("degraded bootstrap 必须 Paused，实际 {other:?}"),
    }
    assert_no_baseline_apply(&fixture.vault);
    assert_identity_unchanged(&fixture, &before);
}

#[test]
fn selected_cid_not_in_bootstrap_pair_is_rejected() {
    ensure_test_identity_wrap();
    // 没有"任意 cid"后门：commit/cursor 只能来自当前 `/bootstrap` 的主/备 pair
    // （relay 游标不可自造），否则客户端会从错误位置续读。
    let mut fixture = client_apply_fixture();
    let third = build_baseline(
        &mut fixture.relay.inner,
        &fixture.candidate,
        [0x3C; 16],
        6,
        2,
        [0x60; 32],
        &fixture.producer,
        0x08,
    );
    let before = identity_snapshot(&fixture);
    let outcome = run_client_apply(&mut fixture, third.checkpoint_id, 6, "c6");
    match outcome {
        ApplyOutcome::Paused { reason } => {
            assert!(reason.contains("主/备 pair"), "reason={reason}")
        }
        other => panic!("不在 bootstrap pair 的 cid 必须 Paused，实际 {other:?}"),
    }
    assert_no_baseline_apply(&fixture.vault);
    assert_identity_unchanged(&fixture, &before);
}

#[test]
fn post_activation_checkpoint_applies_after_multi_epoch_chain() {
    ensure_test_identity_wrap();
    // 链尾（E3）同样可能被普通健康 checkpoint 替换：多 epoch 追赶 + 非 pair 路径都必须成立。
    let mut fixture = multi_epoch_client_fixture();
    let primary = multi_install_post_activation_pair(&mut fixture, 6);
    let latest_t = &fixture
        .relay
        .evidence
        .as_ref()
        .expect("T evidence")
        .activation_t;
    assert_ne!(primary, latest_t.primary_cid);
    assert_ne!(primary, latest_t.fallback_cid);
    let outcome = run_multi_apply(&mut fixture, primary, 6, "c6");
    match outcome {
        ApplyOutcome::Applied {
            baseline_s: 6,
            promoted: true,
        } => {}
        other => panic!("多 epoch 链尾的非 pair 基线必须 Applied，实际 {other:?}"),
    }
    let promoted = menghunote_sync::identity::LocalIdentityV1::unprotect(
        &std::fs::read(&fixture.identity_path).expect("read identity"),
        &VAULT_ID,
        &fixture.device_id,
    )
    .expect("promoted identity");
    assert_eq!(promoted.current_epoch, 3);
}

#[test]
fn activation_pair_commit_digest_mismatch_is_not_downgraded() {
    ensure_test_identity_wrap();
    // 反降级：`cid ∈ T.pair` 时 T 的双签点名了该 commit 的 digest，必须逐字节相等。
    // 网络入口对**每一次** apply 都会先跑 checkpoint 级认证并把结果传给门禁，因此这里
    // "弱路径材料可用"（认证已通过）也不能让 pair 路径绕过 T 的 digest 绑定——否则恶意
    // relay 可在激活 pair 仍健康时改喂"同 cid、不同基线"，把轮换冻结点唯一的
    // have-frozen 保证静默弱化。
    let mut fixture = client_apply_fixture();
    let evidence = fixture.relay.evidence.as_mut().expect("T evidence");
    evidence.activation_t.primary_commit_digest = [0x99; 32];
    fixture.resign_activation_t();
    let outcome = run_client_apply(&mut fixture, [0x26; 16], 6, "c6");
    match outcome {
        ApplyOutcome::Paused { reason } => {
            assert!(reason.contains("commit digest"), "reason={reason}")
        }
        other => panic!("commit digest 不符必须 Paused，实际 {other:?}"),
    }
    assert_not_applied(&fixture.vault);
}

#[test]
fn with_client_rejects_freeze_seq_mismatch() {
    ensure_test_identity_wrap();
    let mut fixture = client_apply_fixture();
    fixture
        .relay
        .evidence
        .as_mut()
        .expect("T evidence")
        .activation_t
        .freeze_seq = 7;
    fixture.resign_activation_t();
    let outcome = run_client_apply(&mut fixture, [0x26; 16], 6, "c6");
    match outcome {
        ApplyOutcome::Paused { reason } => assert!(
            reason.contains("epoch/S") || reason.contains("T 授权材料"),
            "reason={reason}"
        ),
        other => panic!("S 不符必须 Paused，实际 {other:?}"),
    }
    assert_not_applied(&fixture.vault);
}

#[test]
fn with_client_rejects_baseline_s_or_cursor_mismatch() {
    ensure_test_identity_wrap();
    // (baseline_s, cursor) 都必须精确等于 T 为选中 checkpoint 授权的 S/游标。
    for (s, cursor) in [(7_u64, "c6"), (6, "c7")] {
        let mut fixture = client_apply_fixture();
        let outcome = run_client_apply(&mut fixture, [0x26; 16], s, cursor);
        match outcome {
            ApplyOutcome::Paused { reason } => assert!(
                reason.contains("checkpoint 状态") || reason.contains("bootstrap 条目"),
                "s={s} cursor={cursor} reason={reason}"
            ),
            other => panic!("s={s} cursor={cursor} 必须 Paused，实际 {other:?}"),
        }
        assert_not_applied(&fixture.vault);
    }
}

#[test]
fn with_client_rejects_grant_not_covered_by_t_grants_digest() {
    ensure_test_identity_wrap();
    let mut fixture = client_apply_fixture();
    fixture.replace_self_grant();
    let outcome = run_client_apply(&mut fixture, [0x26; 16], 6, "c6");
    match outcome {
        ApplyOutcome::Paused { reason } => {
            assert!(reason.contains("grants_digest"), "reason={reason}")
        }
        other => panic!("self grant 不在 T grants_digest 内必须 Paused，实际 {other:?}"),
    }
    assert_not_applied(&fixture.vault);
}

#[test]
fn with_client_rejects_tampered_grant_ciphertext() {
    ensure_test_identity_wrap();
    let mut fixture = client_apply_fixture();
    let grant = fixture.relay.inner.self_grant.as_mut().expect("self grant");
    grant.ciphertext[0] ^= 0x01;
    let outcome = run_client_apply(&mut fixture, [0x26; 16], 6, "c6");
    match outcome {
        ApplyOutcome::Paused { reason } => {
            assert!(reason.contains("signed_header"), "reason={reason}")
        }
        other => panic!("ciphertext 被换必须 Paused，实际 {other:?}"),
    }
    assert_not_applied(&fixture.vault);
}

/// staging grant 的 AAD 必须带**旧**管理 rev：合法夹具（AAD=1、T.old=1、T.new=2）必须
/// 能完成 apply —— 旧实现拿 AAD 去比 `T.new_manager_rev`，E1→E2 时会让 B 永远 Paused。
#[test]
fn with_client_accepts_staging_grant_aad_at_old_manager_rev() {
    ensure_test_identity_wrap();
    let mut fixture = client_apply_fixture();
    let grant = fixture.relay.inner.self_grant.as_ref().expect("self grant");
    let aad = GrantAadV1::decode(&grant.signed_header[..grant.signed_header.len() - 64])
        .expect("grant AAD");
    assert_eq!(aad.manager_rev, 1, "staging grant AAD 必须是旧管理 rev");
    let t = &fixture
        .relay
        .evidence
        .as_ref()
        .expect("T evidence")
        .activation_t;
    assert_eq!(t.old_manager_rev, 1, "T.old 是 staging 时的管理 rev");
    assert_eq!(t.new_manager_rev, 2, "T.new = old + 1 只用于 promote");
    let outcome = run_client_apply(&mut fixture, [0x26; 16], 6, "c6");
    match outcome {
        ApplyOutcome::Applied {
            baseline_s: 6,
            promoted: true,
        } => {}
        other => panic!("AAD=old、T.new=old+1 必须 Applied，实际 {other:?}"),
    }
    // 推广后身份/旁路状态必须到新管理 rev（AAD 的旧 rev 不阻止 promote）。
    let promoted = menghunote_sync::identity::LocalIdentityV1::unprotect(
        &std::fs::read(&fixture.identity_path).expect("read identity"),
        &VAULT_ID,
        &fixture.device_id,
    )
    .expect("promoted identity");
    assert_eq!(promoted.current_epoch, 2);
    assert_eq!(promoted.pinned_manager, fixture.new_manager.public_key());
    let rotation =
        menghunote_sync::identity::IdentityRotationStateV1::load_current(&fixture.vault, &promoted)
            .expect("rotation state")
            .expect("推广必须写旁路状态");
    assert_eq!(rotation.manager_rev, 2, "旁路状态必须记录 T.new rev");
}

/// 反例：staging grant 的 AAD 被写成**推广目标** rev（new）→ 必须 Paused、不写库。
#[test]
fn with_client_rejects_staging_grant_aad_at_new_manager_rev() {
    ensure_test_identity_wrap();
    let mut fixture = client_apply_fixture();
    let bad = build_self_grant(
        &fixture.old_manager,
        fixture.device_id,
        fixture.recipient_pub,
        2,
        2, // AAD = new rev（真实 staging 不会出现）
        [0x58; 16],
        [0x5D; 32],
        vec![0x5E; 16],
    );
    fixture.relay.inner.self_grant = Some(bad.evidence());
    let outcome = run_client_apply(&mut fixture, [0x26; 16], 6, "c6");
    match outcome {
        ApplyOutcome::Paused { reason } => {
            assert!(reason.contains("AAD"), "reason={reason}")
        }
        other => panic!("AAD=new rev 必须 Paused，实际 {other:?}"),
    }
    assert_not_applied(&fixture.vault);
}

/// 反例：AAD 的 rev 既不是 old 也不是 new（被篡改/跨代错配）→ 必须 Paused、不写库。
#[test]
fn with_client_rejects_staging_grant_aad_not_old_manager_rev() {
    ensure_test_identity_wrap();
    let mut fixture = client_apply_fixture();
    let bad = build_self_grant(
        &fixture.old_manager,
        fixture.device_id,
        fixture.recipient_pub,
        2,
        0, // 与 T.old_manager_rev=1 不符
        [0x59; 16],
        [0x5C; 32],
        vec![0x5B; 16],
    );
    fixture.relay.inner.self_grant = Some(bad.evidence());
    let outcome = run_client_apply(&mut fixture, [0x26; 16], 6, "c6");
    match outcome {
        ApplyOutcome::Paused { reason } => {
            assert!(reason.contains("AAD"), "reason={reason}")
        }
        other => panic!("AAD 的 rev 与 T.old 不符必须 Paused，实际 {other:?}"),
    }
    assert_not_applied(&fixture.vault);
}

/// 反例：T.old_manager_rev 与**本地管理链位置**不一致（链内 old==new 均自洽但整体错位）。
///
/// 这里把本地旁路状态的 manager_rev 抬到 3，而链首（=唯一一环）T.old 仍是 2：
/// AAD=2 与 T.old=2 能过 grant 门，但链首 rev 锚不上本地状态 → 必须 Paused。
#[test]
fn offline_device_rejects_chain_head_manager_rev_not_anchored_locally() {
    ensure_test_identity_wrap();
    let mut fixture = multi_epoch_client_fixture();
    fixture.identity.current_epoch = 2;
    fixture.identity.pinned_manager = fixture.middle_manager.public_key();
    menghunote_sync::identity::IdentityRotationStateV1 {
        current_epoch: 2,
        control_rev: 4,
        manager_rev: 3, // 本地位置 rev=3，但链首 T.old_manager_rev=2
        previous_roots: Vec::new(),
    }
    .save(&fixture.vault, &VAULT_ID, &fixture.device_id)
    .expect("save rotation state");
    let manager_public = fixture.middle_manager.public_key();
    let outcome = run_multi_apply_with_manager(&mut fixture, [0x36; 16], 6, "c6", manager_public);
    match outcome {
        ApplyOutcome::Paused { reason } => assert!(
            reason.contains("链首") && reason.contains("rev"),
            "reason={reason}"
        ),
        other => panic!("链首 rev 与本地管理链位置不符必须 Paused，实际 {other:?}"),
    }
    assert_not_applied(&fixture.vault);
}

#[test]
fn with_client_accepts_baseline_s_after_freeze_seq() {
    ensure_test_identity_wrap();
    // T 绑定的是**冻结** S=6；候选基线在冻结之后建到 S=7 → 合法，必须能 apply。
    let mut fixture = client_apply_fixture_with_s(6, 7);
    let outcome = run_client_apply(&mut fixture, [0x26; 16], 7, "c7");
    match outcome {
        ApplyOutcome::Applied {
            baseline_s: 7,
            promoted: true,
        } => {}
        other => panic!("baseline_s > freeze_seq 必须 Applied，实际 {other:?}"),
    }
    // cursor 推到候选基线自己的 S（不是 freeze S）。
    let cursor = menghunote_storage::sync_cursor_state(&fixture.vault).expect("cursor");
    assert_eq!(cursor.integrated_seq, 7, "integrated 必须等于候选基线 S");
    assert_eq!(
        fixture
            .vault
            .get_meta("sync.relay_cursor")
            .expect("meta")
            .as_deref(),
        Some(b"c7".as_slice()),
        "relay 游标必须是该 S 的 /bootstrap 游标"
    );
    let promoted = menghunote_sync::identity::LocalIdentityV1::unprotect(
        &std::fs::read(&fixture.identity_path).expect("read identity"),
        &VAULT_ID,
        &fixture.device_id,
    )
    .expect("promoted identity");
    assert_eq!(promoted.current_epoch, 2);
}

#[test]
fn with_client_rejects_candidate_baseline_before_freeze_seq() {
    ensure_test_identity_wrap();
    // 候选基线只覆盖 S=5，而 T 冻结在 S=6：候选不得早于 freeze S（activation commit
    // 的 primary/fallback covered_seq 也必须满足下界）。
    let mut fixture = client_apply_fixture_with_s(6, 5);
    let outcome = run_client_apply(&mut fixture, [0x26; 16], 5, "c5");
    match outcome {
        ApplyOutcome::Paused { reason } => assert!(
            reason.contains("freeze_seq") || reason.contains("T 授权材料"),
            "reason={reason}"
        ),
        other => panic!("候选 S < freeze_seq 必须 Paused，实际 {other:?}"),
    }
    assert_not_applied(&fixture.vault);
}

#[test]
fn with_client_rejects_baseline_s_below_selected_checkpoint() {
    ensure_test_identity_wrap();
    // 选中的 checkpoint 覆盖 S=6，却按 S=5 去 apply：不能把 cursor 推到错误的 S。
    let mut fixture = client_apply_fixture();
    let outcome = run_client_apply(&mut fixture, [0x26; 16], 5, "c5");
    match outcome {
        ApplyOutcome::Paused { reason } => assert!(
            reason.contains("checkpoint 状态") || reason.contains("S"),
            "reason={reason}"
        ),
        other => panic!("baseline_s 与 checkpoint S 不符必须 Paused，实际 {other:?}"),
    }
    assert_not_applied(&fixture.vault);
}

#[test]
fn with_client_rejects_cursor_of_another_s() {
    ensure_test_identity_wrap();
    // S=6 的 checkpoint 必须配 S=6 的 `/bootstrap` 游标；复用 S=7 的游标 = 从错位置续读。
    for (s, cursor) in [(6_u64, "c7"), (6, ""), (6, "c6x")] {
        let mut fixture = client_apply_fixture();
        let outcome = run_client_apply(&mut fixture, [0x26; 16], s, cursor);
        match outcome {
            ApplyOutcome::Paused { reason } => assert!(
                reason.contains("cursor") || reason.contains("bootstrap 条目"),
                "cursor={cursor:?} reason={reason}"
            ),
            other => panic!("cursor={cursor:?} 必须 Paused，实际 {other:?}"),
        }
        assert_not_applied(&fixture.vault);
    }
}

#[test]
fn client_without_activation_t_override_fails_closed() {
    ensure_test_identity_wrap();
    // 只实现 `RelayClient`、不覆盖 `verified_activation_t` 的客户端拿默认 `Ok(None)`：
    // 不是"验证通过"，必须 Paused 且不写库。engine 的 `C: RelayClient` 正是这种调用面。
    let mut fixture = client_apply_fixture();
    let outcome = apply_epoch_change_with_client(
        &mut fixture.relay.inner,
        &fixture.vault,
        &fixture.identity_path,
        &fixture.identity,
        [0x26; 16],
        6,
        "c6",
        &fixture.candidate,
        fixture.old_manager.public_key(),
        fixture.new_manager.public_key(),
        2,
        None,
    )
    .expect("client");
    match outcome {
        ApplyOutcome::Paused { reason } => {
            assert!(reason.contains("activation T"), "reason={reason}")
        }
        other => panic!("缺 T 的客户端必须 Paused，实际 {other:?}"),
    }
    assert_not_applied(&fixture.vault);
}

#[test]
fn engine_apply_uses_trait_activation_t_and_is_not_always_paused() {
    ensure_test_identity_wrap();
    // engine 的 `C: RelayClient` 路径必须走 trait 的 `verified_activation_t`，
    // FakeRelay 注入合法 T 后不再恒 Paused；Applied 后必须换根/解冻。
    let mut fixture = client_apply_fixture();
    let engine_root = RootSecretV1::new(VAULT_ID, 1, fixture.identity.current_root.clone());
    let mut engine = menghunote_sync::engine::SyncEngine::new(
        &fixture.vault,
        &mut fixture.relay,
        menghunote_sync::engine::EngineIdentity {
            device_id: fixture.device_id,
            signing_key: fixture.producer.identity.signing_key.clone(),
            peer_id: fixture.producer.identity.peer_id,
        },
        engine_root,
    );
    let outcome = engine
        .apply_epoch_change(
            &fixture.vault,
            &fixture.identity_path,
            &fixture.identity,
            [0x26; 16],
            6,
            "c6",
            &fixture.candidate,
            fixture.old_manager.public_key(),
            fixture.new_manager.public_key(),
            2,
            None,
        )
        .expect("engine apply");
    match outcome {
        ApplyOutcome::Applied {
            baseline_s: 6,
            promoted: true,
        } => {}
        other => panic!("engine 路径必须 Applied，实际 {other:?}"),
    }
    assert_eq!(
        engine.root().key_epoch,
        2,
        "Applied 后必须换到候选 epoch 根"
    );
    assert_eq!(
        fixture
            .vault
            .get_meta("sync.rotation_apply_phase")
            .expect("meta")
            .as_deref(),
        Some(b"applied".as_slice())
    );
    // 推广成功会置 `sync.repack_required=1`（旧 epoch outbox 待重包）：durable barrier
    // 必须继续冻结 push/encrypt，只有重包完成后 `thaw_rotation` 才解冻。
    let frozen = engine.encrypt_pending(1);
    assert!(
        matches!(
            frozen,
            Err(menghunote_sync::SyncError::Paused(
                menghunote_sync::relay::PauseReason::RotationFrozen
            ))
        ),
        "{frozen:?}"
    );
    fixture
        .vault
        .set_meta("sync.repack_required", b"0")
        .expect("clear repack flag");
    engine.thaw_rotation();
    let thawed = engine.encrypt_pending(1);
    assert!(thawed.is_ok(), "重包完成后必须解冻：{thawed:?}");
    let promoted = menghunote_sync::identity::LocalIdentityV1::unprotect(
        &std::fs::read(&fixture.identity_path).expect("read identity"),
        &VAULT_ID,
        &fixture.device_id,
    )
    .expect("promoted identity");
    assert_eq!(promoted.current_epoch, 2);
    assert_eq!(promoted.pinned_manager, fixture.new_manager.public_key());
}

#[test]
fn apply_persists_applying_phase_before_merge_and_applied_after_success() {
    ensure_test_identity_wrap();
    // 评审 0003 P1-3：任何 baseline 写库/merge 之前，`sync.rotation_apply_phase` 必须已是
    // `applying`。用 SQLite trigger 把这条顺序做成硬约束——phase 晚写/不写都会让 merge 失败。
    let mut fixture = client_apply_fixture();
    fixture
        .vault
        .exec_batch(
            "CREATE TRIGGER require_apply_phase BEFORE INSERT ON baseline_apply \
             WHEN COALESCE(CAST((SELECT value FROM meta \
                  WHERE key='sync.rotation_apply_phase') AS TEXT),'') <> 'applying' \
             BEGIN SELECT RAISE(ABORT,'phase not applying'); END;",
        )
        .expect("注入 phase 顺序校验");
    // receipt 必须在 merge **成功之后**才写：merge 开始时 receipt 必须还不存在。
    fixture
        .vault
        .exec_batch(
            "CREATE TRIGGER require_no_receipt BEFORE INSERT ON baseline_apply \
             WHEN length(COALESCE((SELECT value FROM meta \
                  WHERE key='sync.rotation_apply_receipt'), x'')) > 0 \
             BEGIN SELECT RAISE(ABORT,'receipt written before merge'); END;",
        )
        .expect("注入 receipt 顺序校验");

    let outcome = run_client_apply(&mut fixture, [0x26; 16], 6, "c6");
    assert!(
        matches!(
            outcome,
            ApplyOutcome::Applied {
                baseline_s: 6,
                promoted: true
            }
        ),
        "merge 前已落 applying 时必须成功：{outcome:?}"
    );
    // 成功后才转 applied（engine barrier 据此解冻）。
    assert_eq!(
        fixture
            .vault
            .get_meta("sync.rotation_apply_phase")
            .expect("phase")
            .as_deref(),
        Some(b"applied".as_slice())
    );
    // 推广后的身份 pin 是本次 activation 的已验证值，而不是旧 control_rev=3。
    let promoted = menghunote_sync::identity::LocalIdentityV1::unprotect(
        &std::fs::read(&fixture.identity_path).expect("read identity"),
        &VAULT_ID,
        &fixture.device_id,
    )
    .expect("promoted identity");
    assert_eq!(promoted.current_epoch, 2);
    assert_eq!(promoted.control_rev, 4, "control_rev 必须是本次激活值");
    assert_eq!(promoted.pinned_instance, fixture.relay.inner.instance_id);
    let pin = menghunote_sync::rotation_activate::AuthenticatedPinV1::load_current(
        &fixture.vault,
        &promoted,
    )
    .expect("pin")
    .expect("pin");
    assert_eq!(pin.epoch, 2);
    assert_eq!(pin.control_rev, 4);
    assert_eq!(pin.instance_id, fixture.relay.inner.instance_id);
    let commit = activation_commit_materials(
        VAULT_ID,
        fixture.relay.inner.instance_id,
        &fixture.view,
        &fixture
            .relay
            .inner
            .checkpoint_status
            .get(&fixture.view.primary_checkpoint.expect("primary"))
            .expect("primary status")
            .clone(),
        &fixture
            .relay
            .inner
            .checkpoint_status
            .get(&fixture.view.fallback_checkpoint.expect("fallback"))
            .expect("fallback status")
            .clone(),
    )
    .expect("commit materials");
    let expected_hash: [u8; 32] = Sha256::digest(commit.encode().expect("commit encode")).into();
    assert_eq!(
        pin.activation_hash, expected_hash,
        "pin 必须绑定本次 activation commit 摘要"
    );
    // 正常 apply 必须留下 typed receipt，字段与本次激活/基线/pin 逐项一致。
    let receipt = menghunote_sync::rotation_journal::RotationApplyReceipt::load(&fixture.vault)
        .expect("receipt")
        .expect("receipt 必须存在");
    assert_eq!(receipt.vault_id(), VAULT_ID);
    assert_eq!(receipt.device_id(), fixture.device_id);
    assert_eq!(receipt.rotation_id(), fixture.view.rotation_id);
    assert_eq!(receipt.checkpoint_id(), [0x26; 16]);
    assert_eq!(receipt.baseline_s(), 6);
    assert_eq!(receipt.baseline_cursor(), "c6");
    assert_eq!(receipt.from_epoch(), 1);
    assert_eq!(receipt.epoch(), 2);
    assert_eq!(receipt.control_rev(), 4);
    assert_eq!(receipt.instance_id(), fixture.relay.inner.instance_id);
    assert_eq!(
        receipt.new_manager_public(),
        fixture.new_manager.public_key()
    );
    assert_eq!(receipt.new_manager_rev(), 2);
    assert_eq!(receipt.activation_hash(), expected_hash);
}

#[test]
fn apply_failure_after_merge_before_promotion_freezes_restarted_engine() {
    ensure_test_identity_wrap();
    // 评审 0003 P1-3 的崩溃点：baseline merge 已提交、身份 promotion 之前失败。
    // RAM freeze 会随进程消失，持久 phase 必须让重启后的 engine 继续冻结。
    let mut fixture = client_apply_fixture();
    let old_identity = std::fs::read(&fixture.identity_path).expect("old identity");
    // promotion 的临时身份父目录不存在 → merge 成功后、rename 前失败。
    let missing = fixture
        ._dir
        .path()
        .join("missing-identity-dir")
        .join("identity.bin");
    let outcome = apply_epoch_change_with_client(
        &mut fixture.relay,
        &fixture.vault,
        &missing,
        &fixture.identity,
        [0x26; 16],
        6,
        "c6",
        &fixture.candidate,
        fixture.old_manager.public_key(),
        fixture.new_manager.public_key(),
        2,
        None,
    )
    .expect("客户端 apply");
    assert!(
        matches!(outcome, ApplyOutcome::Paused { .. }),
        "{outcome:?}"
    );
    // 失败保持冻结 phase（不是 applied）。
    assert_eq!(
        fixture
            .vault
            .get_meta("sync.rotation_apply_phase")
            .expect("phase")
            .as_deref(),
        Some(b"paused".as_slice())
    );
    // merge 已提交：cursor 停在基线 S=6，不再前进；身份未推广。
    let cursor = menghunote_storage::sync_cursor_state(&fixture.vault).expect("cursor");
    assert_eq!(cursor.received_seq, 6);
    assert_eq!(cursor.integrated_seq, 6);
    assert_eq!(
        fixture
            .vault
            .get_meta(menghunote_sync::engine::RELAY_CURSOR_META)
            .expect("relay cursor")
            .as_deref(),
        Some(b"c6".as_slice())
    );
    assert_eq!(
        std::fs::read(&fixture.identity_path).expect("identity"),
        old_identity,
        "promotion 失败不得替换旧身份"
    );

    // 模拟进程重启：重新打开 Vault，engine 必须从持久 phase 重建冻结。
    let reopened = Vault::open(fixture._dir.path().join("vaults").join("primary.sqlite3"))
        .expect("重开 vault");
    let old_root = RootSecretV1::new(VAULT_ID, 1, fixture.identity.current_root.clone());
    let mut engine = menghunote_sync::engine::SyncEngine::new(
        &reopened,
        &mut fixture.relay,
        menghunote_sync::engine::EngineIdentity {
            device_id: fixture.device_id,
            signing_key: fixture.producer.identity.signing_key.clone(),
            peer_id: fixture.producer.identity.peer_id,
        },
        old_root,
    );
    assert!(engine.is_rotation_frozen(), "重启后必须仍冻结");
    let encrypt_error = engine.encrypt_pending(1);
    assert!(
        matches!(
            encrypt_error,
            Err(SyncError::Paused(
                menghunote_sync::relay::PauseReason::RotationFrozen
            ))
        ),
        "旧写不得继续：{encrypt_error:?}"
    );
    let push_error = engine.push();
    assert!(
        matches!(
            push_error,
            Err(SyncError::Paused(
                menghunote_sync::relay::PauseReason::RotationFrozen
            ))
        ),
        "旧写不得继续：{push_error:?}"
    );
    // 一次 RAM thaw 不能绕过持久 paused phase。
    engine.thaw_rotation();
    assert!(engine.is_rotation_frozen(), "paused phase 不得被 thaw 解除");
    // 旧根与 relay 新 epoch 不符：pull 也拒绝，游标不再推进。
    let before = menghunote_storage::sync_cursor_state(&reopened).expect("cursor");
    assert!(matches!(
        engine.pull(),
        Err(SyncError::Paused(
            menghunote_sync::relay::PauseReason::EpochMismatch
        ))
    ));
    let after = menghunote_storage::sync_cursor_state(&reopened).expect("cursor");
    assert_eq!(
        (
            before.received_seq,
            before.integrated_seq,
            before.confirmed_highwater
        ),
        (
            after.received_seq,
            after.integrated_seq,
            after.confirmed_highwater
        ),
        "失败/冻结后不得再推进水位"
    );
    assert_eq!(
        reopened
            .get_meta(menghunote_sync::engine::RELAY_CURSOR_META)
            .expect("relay cursor")
            .as_deref(),
        Some(b"c6".as_slice()),
        "relay 游标不得被冻结后的 pull 改写"
    );
}

#[test]
fn apply_failure_during_merge_keeps_paused_phase_and_cursor() {
    ensure_test_identity_wrap();
    // 屏障已落盘之后 merge 失败：phase 必须保持非 applied，且 cursor/journal 不前进。
    let mut fixture = client_apply_fixture();
    fixture
        .vault
        .exec_batch(
            "CREATE TRIGGER fail_merge BEFORE INSERT ON baseline_apply \
             BEGIN SELECT RAISE(ABORT,'injected merge'); END;",
        )
        .expect("注入 merge 失败");
    let outcome = run_client_apply(&mut fixture, [0x26; 16], 6, "c6");
    assert!(
        matches!(outcome, ApplyOutcome::Paused { .. }),
        "{outcome:?}"
    );
    assert_eq!(
        fixture
            .vault
            .get_meta("sync.rotation_apply_phase")
            .expect("phase")
            .as_deref(),
        Some(b"paused".as_slice())
    );
    assert_not_applied(&fixture.vault);
    // receipt 在 merge 成功前不得写入。
    assert_eq!(
        menghunote_sync::rotation_journal::RotationApplyReceipt::load(&fixture.vault)
            .expect("receipt"),
        None
    );
}

/// 崩溃点：rename 已提交、`sync.rotation_apply_phase='applied'` 未写。
///
/// 用 SQLite trigger 在写 `applied` 时注入失败，留下的持久现场正是：
/// `phase=applying` + receipt + 推广后的 identity + `sync.repack_required=1`。
#[test]
fn rename_committed_before_applied_recovers_from_durable_receipt() {
    ensure_test_identity_wrap();
    let mut fixture = client_apply_fixture();
    fixture
        .vault
        .exec_batch(
            "CREATE TRIGGER fail_applied_phase BEFORE UPDATE ON meta \
             WHEN OLD.key='sync.rotation_apply_phase' \
              AND CAST(NEW.value AS TEXT)='applied' \
             BEGIN SELECT RAISE(ABORT,'injected crash before applied'); END;",
        )
        .expect("注入 applied 落盘失败");

    let error = apply_epoch_change_with_client(
        &mut fixture.relay,
        &fixture.vault,
        &fixture.identity_path,
        &fixture.identity,
        [0x26; 16],
        6,
        "c6",
        &fixture.candidate,
        fixture.old_manager.public_key(),
        fixture.new_manager.public_key(),
        2,
        None,
    )
    .expect_err("applied 落盘失败必须返回 Err");
    assert!(
        format!("{error}").contains("injected crash before applied")
            || matches!(error, SyncError::Storage(_)),
        "错误应来自 injected trigger：{error:?}"
    );

    // 崩溃现场：phase 仍是 applying，receipt 已 durable，身份已经 rename 到 epoch 2。
    assert_eq!(
        fixture
            .vault
            .get_meta("sync.rotation_apply_phase")
            .expect("phase")
            .as_deref(),
        Some(b"applying".as_slice())
    );
    let receipt = menghunote_sync::rotation_journal::RotationApplyReceipt::load(&fixture.vault)
        .expect("receipt")
        .expect("receipt 必须已写");
    assert_eq!(receipt.epoch(), 2);
    assert_eq!(receipt.baseline_s(), 6);
    let promoted = menghunote_sync::identity::LocalIdentityV1::unprotect(
        &std::fs::read(&fixture.identity_path).expect("read identity"),
        &VAULT_ID,
        &fixture.device_id,
    )
    .expect("promoted identity");
    assert_eq!(promoted.current_epoch, 2);
    let before = menghunote_storage::sync_cursor_state(&fixture.vault).expect("cursor");
    assert_eq!((before.received_seq, before.integrated_seq), (6, 6));

    // 模拟进程重启：重开 Vault，engine 构造期仍因 applying 冻结。
    fixture
        .vault
        .exec_batch("DROP TRIGGER fail_applied_phase")
        .expect("移除故障注入");
    let reopened = Vault::open(fixture._dir.path().join("vaults").join("primary.sqlite3"))
        .expect("重开 vault");
    let promoted_root = RootSecretV1::new(VAULT_ID, 2, promoted.current_root.clone());
    let mut engine = menghunote_sync::engine::SyncEngine::new(
        &reopened,
        &mut fixture.relay,
        menghunote_sync::engine::EngineIdentity {
            device_id: fixture.device_id,
            signing_key: fixture.producer.identity.signing_key.clone(),
            peer_id: fixture.producer.identity.peer_id,
        },
        promoted_root,
    );
    assert!(engine.is_rotation_frozen(), "applying 屏障必须跨重启保留");

    // same-epoch 收尾：不要求 epoch change，凭 receipt 认证后写 applied。
    let outcome = engine
        .reconcile_rotation_apply(&fixture.identity_path, &promoted)
        .expect("reconcile")
        .expect("应识别 applying 并收尾");
    assert!(
        matches!(
            outcome,
            ApplyOutcome::Applied {
                baseline_s: 6,
                promoted: true
            }
        ),
        "{outcome:?}"
    );
    assert_eq!(
        reopened
            .get_meta("sync.rotation_apply_phase")
            .expect("phase")
            .as_deref(),
        Some(b"applied".as_slice())
    );
    // 水位不得被收尾推错：仍是所选基线 S=6。
    let after = menghunote_storage::sync_cursor_state(&reopened).expect("cursor");
    assert_eq!(
        (
            after.received_seq,
            after.integrated_seq,
            after.confirmed_highwater
        ),
        (
            before.received_seq,
            before.integrated_seq,
            before.confirmed_highwater
        )
    );
    assert_eq!(
        reopened
            .get_meta(menghunote_sync::engine::RELAY_CURSOR_META)
            .expect("relay cursor")
            .as_deref(),
        Some(b"c6".as_slice())
    );
    assert_eq!(engine.root().key_epoch, 2);

    // 重包屏障必须保留：收尾后仍冻结，直到 repack 完成才可解冻（不是永久冻结）。
    assert!(engine.is_rotation_frozen(), "repack 未完成前必须保持冻结");
    reopened
        .set_meta("sync.repack_required", b"0")
        .expect("重包完成");
    engine.thaw_rotation();
    assert!(!engine.is_rotation_frozen(), "重包完成后必须可解冻");

    // 重复入口是幂等的：phase 已 applied → 不再有 applying 需要收尾。
    assert!(engine
        .reconcile_rotation_apply(&fixture.identity_path, &promoted)
        .expect("second reconcile")
        .is_none());
}

/// same-epoch + applying 但 receipt 丢失：必须 fail-closed，不能无条件清 phase。
#[test]
fn same_epoch_applying_without_receipt_fails_closed() {
    ensure_test_identity_wrap();
    let mut fixture = client_apply_fixture();
    let outcome = run_client_apply(&mut fixture, [0x26; 16], 6, "c6");
    assert!(
        matches!(outcome, ApplyOutcome::Applied { .. }),
        "{outcome:?}"
    );
    let promoted = menghunote_sync::identity::LocalIdentityV1::unprotect(
        &std::fs::read(&fixture.identity_path).expect("read identity"),
        &VAULT_ID,
        &fixture.device_id,
    )
    .expect("promoted identity");
    let before = menghunote_storage::sync_cursor_state(&fixture.vault).expect("cursor");

    // 模拟 receipt 丢失 + 崩溃残留的 applying。
    menghunote_sync::rotation_journal::RotationApplyPhase::Applying
        .store(&fixture.vault)
        .expect("applying");
    menghunote_sync::rotation_journal::RotationApplyReceipt::clear(&fixture.vault)
        .expect("clear receipt");

    // 网络入口看到同 epoch，也必须先走 receipt 校验并 Paused。
    let outcome = apply_epoch_change_with_client(
        &mut fixture.relay,
        &fixture.vault,
        &fixture.identity_path,
        &promoted,
        [0x26; 16],
        6,
        "c6",
        &fixture.candidate,
        fixture.old_manager.public_key(),
        fixture.new_manager.public_key(),
        2,
        None,
    )
    .expect("client apply");
    match outcome {
        ApplyOutcome::Paused { reason } => {
            assert!(reason.contains("receipt"), "reason={reason}")
        }
        other => panic!("缺 receipt 必须 fail-closed，实际 {other:?}"),
    }
    assert_eq!(
        fixture
            .vault
            .get_meta("sync.rotation_apply_phase")
            .expect("phase")
            .as_deref(),
        Some(b"applying".as_slice()),
        "缺 receipt 时 phase 不得被无条件清除"
    );
    let after = menghunote_storage::sync_cursor_state(&fixture.vault).expect("cursor");
    assert_eq!(
        (before.received_seq, before.integrated_seq),
        (after.received_seq, after.integrated_seq)
    );

    // startup 入口同样 fail-closed 并保持冻结。
    let reopened = Vault::open(fixture._dir.path().join("vaults").join("primary.sqlite3"))
        .expect("重开 vault");
    let mut engine = menghunote_sync::engine::SyncEngine::new(
        &reopened,
        &mut fixture.relay,
        menghunote_sync::engine::EngineIdentity {
            device_id: fixture.device_id,
            signing_key: fixture.producer.identity.signing_key.clone(),
            peer_id: fixture.producer.identity.peer_id,
        },
        RootSecretV1::new(VAULT_ID, 2, promoted.current_root.clone()),
    );
    assert!(engine.is_rotation_frozen());
    let outcome = engine
        .reconcile_rotation_apply(&fixture.identity_path, &promoted)
        .expect("reconcile")
        .expect("applying 必须被识别");
    assert!(
        matches!(outcome, ApplyOutcome::Paused { .. }),
        "{outcome:?}"
    );
    assert!(engine.is_rotation_frozen(), "fail-closed 必须保持冻结");
}

/// same-epoch + applying 但 receipt 与身份/pin 不匹配：必须 fail-closed。
#[test]
fn same_epoch_applying_with_mismatched_receipt_fails_closed() {
    ensure_test_identity_wrap();
    let mut fixture = client_apply_fixture();
    let outcome = run_client_apply(&mut fixture, [0x26; 16], 6, "c6");
    assert!(
        matches!(outcome, ApplyOutcome::Applied { .. }),
        "{outcome:?}"
    );
    let promoted = menghunote_sync::identity::LocalIdentityV1::unprotect(
        &std::fs::read(&fixture.identity_path).expect("read identity"),
        &VAULT_ID,
        &fixture.device_id,
    )
    .expect("promoted identity");

    // 篡改 receipt 的 activation_hash（第 13 个字段）：phase=applying 的残留 receipt
    // 与 authenticated pin 不再匹配。
    let raw = fixture
        .vault
        .get_meta("sync.rotation_apply_receipt")
        .expect("receipt meta")
        .expect("receipt");
    let mut fields = menghunote_sync::cbor::decode(&raw).expect("decode receipt");
    for (key, value) in &mut fields {
        if *key == 13 {
            *value = menghunote_sync::cbor::bytes(vec![0xEE; 32]);
        }
    }
    let tampered = menghunote_sync::cbor::encode(fields).expect("encode receipt");
    fixture
        .vault
        .set_meta("sync.rotation_apply_receipt", &tampered)
        .expect("store tampered receipt");
    menghunote_sync::rotation_journal::RotationApplyPhase::Applying
        .store(&fixture.vault)
        .expect("applying");

    let outcome = apply_epoch_change_with_client(
        &mut fixture.relay,
        &fixture.vault,
        &fixture.identity_path,
        &promoted,
        [0x26; 16],
        6,
        "c6",
        &fixture.candidate,
        fixture.old_manager.public_key(),
        fixture.new_manager.public_key(),
        2,
        None,
    )
    .expect("client apply");
    match outcome {
        ApplyOutcome::Paused { reason } => {
            assert!(
                reason.contains("pin") || reason.contains("不匹配"),
                "reason={reason}"
            )
        }
        other => panic!("receipt 不匹配必须 fail-closed，实际 {other:?}"),
    }
    assert_eq!(
        fixture
            .vault
            .get_meta("sync.rotation_apply_phase")
            .expect("phase")
            .as_deref(),
        Some(b"applying".as_slice())
    );

    // 再篡改 S（与 completed journal 不符）也必须 fail-closed。
    let mut fields = menghunote_sync::cbor::decode(&raw).expect("decode receipt");
    for (key, value) in &mut fields {
        if *key == 5 {
            *value = menghunote_sync::cbor::uint(5);
        }
    }
    let tampered = menghunote_sync::cbor::encode(fields).expect("encode receipt");
    fixture
        .vault
        .set_meta("sync.rotation_apply_receipt", &tampered)
        .expect("store tampered receipt");
    let outcome = apply_epoch_change_with_client(
        &mut fixture.relay,
        &fixture.vault,
        &fixture.identity_path,
        &promoted,
        [0x26; 16],
        6,
        "c6",
        &fixture.candidate,
        fixture.old_manager.public_key(),
        fixture.new_manager.public_key(),
        2,
        None,
    )
    .expect("client apply");
    match outcome {
        ApplyOutcome::Paused { reason } => {
            assert!(
                reason.contains("journal") || reason.contains("不匹配"),
                "reason={reason}"
            )
        }
        other => panic!("receipt S 不匹配必须 fail-closed，实际 {other:?}"),
    }
}

/// 离线跨多 epoch（1→2→3）的 rename 后崩溃也要能收尾：receipt 的 `from_epoch`
/// 必须是**本地起点 1**（而不是链尾 T 的 from_epoch=2），否则 previous_roots 对不上。
#[test]
fn multi_epoch_rename_before_applied_reconciles_with_local_from_epoch() {
    ensure_test_identity_wrap();
    let mut fixture = multi_epoch_client_fixture();
    fixture
        .vault
        .exec_batch(
            "CREATE TRIGGER fail_applied_phase BEFORE UPDATE ON meta \
             WHEN OLD.key='sync.rotation_apply_phase' \
              AND CAST(NEW.value AS TEXT)='applied' \
             BEGIN SELECT RAISE(ABORT,'injected crash before applied'); END;",
        )
        .expect("注入 applied 落盘失败");
    apply_epoch_change_with_client(
        &mut fixture.relay,
        &fixture.vault,
        &fixture.identity_path,
        &fixture.identity,
        [0x36; 16],
        6,
        "c6",
        &fixture.candidate,
        fixture.old_manager.public_key(),
        fixture.new_manager.public_key(),
        3,
        None,
    )
    .expect_err("applied 落盘失败必须返回 Err");

    let receipt = menghunote_sync::rotation_journal::RotationApplyReceipt::load(&fixture.vault)
        .expect("receipt")
        .expect("receipt");
    assert_eq!(receipt.from_epoch(), 1, "receipt 必须记本地 apply 起点");
    assert_eq!(receipt.epoch(), 3);
    assert_eq!(receipt.new_manager_rev(), 3);
    let promoted = menghunote_sync::identity::LocalIdentityV1::unprotect(
        &std::fs::read(&fixture.identity_path).expect("read identity"),
        &VAULT_ID,
        &fixture.device_id,
    )
    .expect("promoted identity");
    assert_eq!(promoted.current_epoch, 3);

    fixture
        .vault
        .exec_batch("DROP TRIGGER fail_applied_phase")
        .expect("移除故障注入");
    let reopened = Vault::open(fixture._dir.path().join("vaults").join("primary.sqlite3"))
        .expect("重开 vault");
    let producer = Device::new(0xA5);
    let mut engine = menghunote_sync::engine::SyncEngine::new(
        &reopened,
        &mut fixture.relay,
        menghunote_sync::engine::EngineIdentity {
            device_id: fixture.device_id,
            signing_key: producer.identity.signing_key.clone(),
            peer_id: producer.identity.peer_id,
        },
        RootSecretV1::new(VAULT_ID, 3, promoted.current_root.clone()),
    );
    let outcome = engine
        .reconcile_rotation_apply(&fixture.identity_path, &promoted)
        .expect("reconcile")
        .expect("applying 必须被识别");
    assert!(
        matches!(
            outcome,
            ApplyOutcome::Applied {
                baseline_s: 6,
                promoted: true
            }
        ),
        "{outcome:?}"
    );
    assert_eq!(
        reopened
            .get_meta("sync.rotation_apply_phase")
            .expect("phase")
            .as_deref(),
        Some(b"applied".as_slice())
    );
    assert_eq!(engine.root().key_epoch, 3);
}

#[test]
fn collect_baseline_payload_returns_raw_loro_snapshot_bytes() {
    ensure_test_identity_wrap();
    let device = Device::new(0x91);
    let mut relay = FakeRelay::new(&[&device]);
    let root = device.root.clone();
    let doc = menghunote_storage::BodyDoc::create("doc-baseline", 0x92).expect("doc");
    doc.insert_scalars(0, "baseline hello").expect("write");
    let loro_bytes = doc.export_snapshot().expect("snapshot");
    let snapshot = menghunote_sync_proto::SnapshotV1 {
        doc_id: "doc-baseline".to_owned(),
        kind: 1,
        generation: 1,
        codec_version: "loro-v1".to_owned(),
        app_doc_schema: 1,
        min_reader: 0,
        full_history: true,
        vv: doc.vv().encode(),
        frontier: doc.frontiers().encode(),
        loro_snapshot_bytes: loro_bytes,
    };
    let snapshot_cbor = snapshot.encode().expect("snapshot encode");
    let object_id = [0x93; 32];
    let (parts, object_digest) = encrypt_local_parts(
        &root,
        ObjectType::Snapshot,
        &object_id,
        [0x94; 16],
        device.device_id,
        1,
        0,
        &snapshot_cbor,
        &[],
    )
    .expect("snapshot parts");
    let part_count = parts.len() as u32;
    for (index, envelope, _) in parts {
        relay.parts.insert((object_id, index), envelope);
    }
    let report = CheckpointReport {
        checkpoint_id: [0x95; 16],
        epoch: root.key_epoch,
        covered_seq: 6,
        docs: vec![DocSnapshotRef {
            doc_id: "doc-baseline".to_owned(),
            kind: 1,
            generation: 1,
            vv: snapshot.vv.clone(),
            frontier: snapshot.frontier.clone(),
            object: UploadedObject {
                object_id,
                part_count,
                total_plain_len: snapshot_cbor.len() as u64,
                object_digest,
            },
        }],
        assets: Vec::new(),
        manifest: UploadedObject {
            object_id: [0x96; 32],
            part_count: 1,
            total_plain_len: 1,
            object_digest: [0x97; 32],
        },
        refs_digest: [0x98; 32],
        published_at: None,
        validated: false,
    };
    let payload =
        menghunote_sync::rotation_apply::collect_baseline_payload(&mut relay, &root, &report)
            .expect("collect baseline");
    assert_eq!(payload.docs.len(), 1);
    assert_eq!(payload.docs[0].0, "doc-baseline");
    // storage 要 raw Loro bytes：能直接 import，且 oplog VV 与快照声明一致。
    let restored = loro::LoroDoc::new();
    restored
        .import(&payload.docs[0].1)
        .expect("import raw loro bytes");
    assert_eq!(restored.oplog_vv().encode(), snapshot.vv);
    assert_ne!(
        payload.docs[0].1, snapshot_cbor,
        "不得把 SnapshotV1 CDE 直接推进 docs"
    );
}

#[test]
fn collect_manifest_payload_returns_raw_loro_snapshot_bytes() {
    ensure_test_identity_wrap();
    let device = Device::new(0x81);
    let mut relay = FakeRelay::new(&[&device]);
    let root = device.root.clone();
    let doc = menghunote_storage::BodyDoc::create("doc-1", 0x82).expect("doc");
    doc.insert_scalars(0, "hello baseline").expect("write");
    let snapshot_bytes = doc.export_snapshot().expect("export snapshot");
    let snapshot = menghunote_sync_proto::SnapshotV1 {
        doc_id: "doc-1".to_owned(),
        kind: 1,
        generation: 1,
        codec_version: "loro-v1".to_owned(),
        app_doc_schema: 1,
        min_reader: 0,
        full_history: true,
        vv: doc.vv().encode(),
        frontier: doc.frontiers().encode(),
        loro_snapshot_bytes: snapshot_bytes,
    };
    let snapshot_cbor = snapshot.encode().expect("snapshot");
    let object_id = [0x83; 32];
    let (parts, object_digest) = menghunote_sync::upload::encrypt_local_parts(
        &root,
        menghunote_sync_proto::ObjectType::Snapshot,
        &object_id,
        [0x84; 16],
        device.device_id,
        1,
        0,
        &snapshot_cbor,
        &[],
    )
    .expect("snapshot parts");
    let part_count = parts.len() as u32;
    for (index, envelope, _) in parts {
        relay.parts.insert((object_id, index), envelope);
    }
    let manifest = menghunote_sync_proto::ManifestV1 {
        vault_id: VAULT_ID,
        key_epoch: root.key_epoch,
        checkpoint_id: [0x85; 16],
        covered_relay_seq: 6,
        min_reader: 0,
        structure_doc_id: "structure".to_owned(),
        docs: vec![menghunote_sync_proto::ManifestDoc {
            doc_id: "doc-1".to_owned(),
            kind: 1,
            generation: 1,
            key_epoch: root.key_epoch,
            vv: snapshot.vv.clone(),
            frontier: snapshot.frontier.clone(),
            snapshot_object_id: object_id,
            part_count,
            total_plain_len: snapshot_cbor.len() as u64,
            object_digest,
        }],
        assets: Vec::new(),
    };
    let payload = menghunote_sync::rotation_apply::collect_manifest_payload(
        &mut relay,
        &root,
        [0x85; 16],
        6,
        &manifest.encode().expect("manifest"),
    )
    .expect("collect");
    assert_eq!(payload.docs.len(), 1);
    // `collect_manifest_payload` 必须输出内层 Loro snapshot，而不是 SnapshotV1 CDE：
    // 直接 import 成功且 oplog VV 与快照声明一致。
    let restored = loro::LoroDoc::new();
    restored
        .import(&payload.docs[0].1)
        .expect("import raw loro");
    assert_eq!(restored.oplog_vv().encode(), snapshot.vv);
}

#[test]
fn fetch_manifest_object_decrypts_manifest_by_oid() {
    ensure_test_identity_wrap();
    use menghunote_sync_proto::ObjectType;
    let device = Device::new(0x86);
    let mut relay = FakeRelay::new(&[&device]);
    let root = device.root.clone();
    let manifest = menghunote_sync_proto::ManifestV1 {
        vault_id: VAULT_ID,
        key_epoch: root.key_epoch,
        checkpoint_id: [0x87; 16],
        covered_relay_seq: 6,
        min_reader: 0,
        structure_doc_id: "structure".to_owned(),
        docs: Vec::new(),
        assets: Vec::new(),
    };
    let plaintext = manifest.encode().expect("manifest");
    let object_id = [0x88; 32];
    let (parts, digest) = menghunote_sync::upload::encrypt_local_parts(
        &root,
        ObjectType::Manifest,
        &object_id,
        [0x89; 16],
        device.device_id,
        0,
        0,
        &plaintext,
        &[],
    )
    .expect("manifest parts");
    for (index, envelope, _) in parts {
        relay.parts.insert((object_id, index), envelope);
    }
    let (bytes, got_digest) =
        menghunote_sync::rotation_apply::fetch_manifest_object(&mut relay, &root, &object_id)
            .expect("fetch manifest");
    assert_eq!(bytes, plaintext);
    assert_eq!(got_digest, digest);
    assert!(menghunote_sync_proto::ManifestV1::decode(&bytes).is_ok());
}

#[test]
fn empty_verified_baseline_finishes_cursor_atomically() {
    ensure_test_identity_wrap();
    let (_dir, vault) = fresh_vault();
    let payload = BaselinePayload {
        checkpoint_id: [0x61; 16],
        baseline_s: 6,
        docs: Vec::new(),
        assets: Vec::new(),
    };
    let state = apply_verified_baseline(&vault, &payload, "c6").expect("apply");
    assert_eq!(state.baseline_s, 6);
    assert_eq!(
        state.status,
        menghunote_storage::BaselineApplyStatus::Completed
    );
    assert_eq!(state.chosen_cursor.as_deref(), Some("c6"));
    let cursor = menghunote_storage::sync_cursor_state(&vault).expect("cursor");
    assert_eq!(cursor.received_seq, 6);
    assert_eq!(cursor.integrated_seq, 6);
}

#[test]
fn restart_decision_uses_higher_epoch_for_leak_or_revocation() {
    ensure_test_identity_wrap();
    let view = view();
    assert_eq!(
        decide_epoch_action(&view, 1, None).expect("apply"),
        EpochAction::Apply {
            rotation_id: view.rotation_id,
            from_epoch: 1,
            to_epoch: 2,
        }
    );
    assert_eq!(
        decide_epoch_action(&view, 1, Some(RestartReason::CandidateRootLeaked)).expect("restart"),
        EpochAction::Restart {
            next_epoch: 3,
            reason: RestartReason::CandidateRootLeaked,
        }
    );
    assert_eq!(
        decide_epoch_action(&view, 1, Some(RestartReason::RecipientRevoked))
            .expect("restart revoked"),
        EpochAction::Restart {
            next_epoch: 3,
            reason: RestartReason::RecipientRevoked,
        }
    );
    assert!(decide_epoch_action(&view, 2, None).is_err());
}

// ---------------------------------------------------------------- 控制链分页 / pin / 吊销证书（评审 0002 P0-2）

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use menghunote_sync::cbor;
use menghunote_sync::device::{DeviceHpkeKey, DeviceSigningKey, ManagementKey};
use menghunote_sync::http_relay::{
    ControlPin, HttpRelayClient, HttpRelayConfig, HttpRequest, HttpResponse, HttpTransport,
};
use menghunote_sync::keys::Secret32;
use menghunote_sync_proto::{
    device_cert_signed_bytes, management_request_cde, management_signing_input, member_cert_digest,
    ControlKind as WireControlKind, DeviceCertV1, HttpMethod, MemberCertDigestInputV1, Rights,
    SignedControlRecord,
};

const CONTROL_VAULT: VaultId = [0x5a; 16];
const CONTROL_INSTANCE: [u8; 16] = [0x22; 16];
const CONTROL_DEVICE: DeviceId = [0x04; 16];
const CONTROL_AUTH_EXPIRES: u64 = 4_000_000_000;

fn cbor_response(fields: cbor::Fields) -> HttpResponse {
    HttpResponse {
        status: 200,
        body: cbor::encode(fields).expect("测试响应编码"),
    }
}

fn control_auth_responses() -> Vec<HttpResponse> {
    vec![
        cbor_response(vec![
            (0, cbor::bytes([0x01; 16])),
            (1, cbor::uint(1)),
            (2, cbor::bytes(CONTROL_DEVICE)),
            (3, cbor::bytes([0x02; 32])),
            (4, cbor::uint(CONTROL_AUTH_EXPIRES - 100)),
            (5, cbor::bytes(CONTROL_VAULT)),
            (6, cbor::bytes(CONTROL_INSTANCE)),
            (7, cbor::uint(CONTROL_AUTH_EXPIRES - 600)),
        ]),
        cbor_response(vec![
            (0, cbor::text("control-token")),
            (1, cbor::uint(CONTROL_AUTH_EXPIRES)),
            (2, cbor::uint(CONTROL_AUTH_EXPIRES - 600)),
            (3, cbor::uint(1)),
            (4, cbor::uint(1)),
        ]),
    ]
}

fn control_state_response(control_rev: u64, epoch: u64) -> HttpResponse {
    cbor_response(vec![
        (0, cbor::bytes(CONTROL_VAULT)),
        (1, cbor::bytes([0x11; 16])),
        (2, cbor::bytes(CONTROL_INSTANCE)),
        (3, cbor::text("active")),
        (4, cbor::uint(epoch)),
        (5, cbor::uint(control_rev)),
        (6, cbor::uint(1)),
        (7, cbor::uint(6)),
        (8, cbor::uint(0)),
    ])
}

#[derive(Clone, Default)]
struct RequestLog(Rc<RefCell<Vec<HttpRequest>>>);

impl RequestLog {
    fn control_paths(&self) -> Vec<String> {
        self.0
            .borrow()
            .iter()
            .filter(|request| request.path.contains("/control"))
            .map(|request| request.path.clone())
            .collect()
    }
}

struct ControlTransport {
    responses: VecDeque<HttpResponse>,
    log: RequestLog,
}

impl HttpTransport for ControlTransport {
    fn send(&mut self, request: &HttpRequest) -> Result<HttpResponse, menghunote_sync::SyncError> {
        self.log.0.borrow_mut().push(request.clone());
        self.responses
            .pop_front()
            .ok_or(menghunote_sync::SyncError::Transport(
                "测试没有更多预设响应",
            ))
    }
}

fn control_client(
    responses: Vec<HttpResponse>,
    manager: &ManagementKey,
) -> (HttpRelayClient<ControlTransport>, RequestLog) {
    let signing = DeviceSigningKey::from_seed(Secret32::from_array([0x04; 32]));
    let log = RequestLog::default();
    let client = HttpRelayClient::new(
        ControlTransport {
            responses: responses.into(),
            log: log.clone(),
        },
        HttpRelayConfig {
            base_url: "http://127.0.0.1:8080".to_owned(),
            api_version: RelayApiVersion::V1,
            vault_id: CONTROL_VAULT,
            device_id: CONTROL_DEVICE,
            manager_pub: Some(manager.public_key()),
        },
        signing,
    );
    (client, log)
}

/// 模拟 relay 的控制链：rev 1 = genesis（裸 CDE），rev 2+ = 管理签名的
/// `SignedControlRecord`；`manager_chain` 对应 `/control` 键 5。
struct ControlChain {
    manager: ManagementKey,
    manager_rev: u64,
    prev: [u8; 32],
    rev: u64,
    commits: Vec<cbor::CdeValue>,
    manager_chain: Vec<cbor::CdeValue>,
    next_request: u8,
}

impl ControlChain {
    fn new(manager: ManagementKey) -> Self {
        let genesis = cbor::encode(vec![(0, cbor::bytes(CONTROL_VAULT))]).expect("genesis");
        Self::with_genesis(manager, genesis)
    }

    fn with_genesis(manager: ManagementKey, genesis: Vec<u8>) -> Self {
        let mut chain = Self {
            manager,
            manager_rev: 1,
            prev: [0u8; 32],
            rev: 0,
            commits: Vec::new(),
            manager_chain: Vec::new(),
            next_request: 0x30,
        };
        let manager_pub = chain.manager.public_key();
        chain.manager_chain.push(cbor::CdeValue::Map(vec![
            (0, cbor::uint(1)),
            (1, cbor::bytes(manager_pub)),
            (2, cbor::bytes(genesis.clone())),
            (3, cbor::uint(1)),
        ]));
        chain.push_raw(genesis);
        chain
    }

    fn push_raw(&mut self, record: Vec<u8>) -> [u8; 32] {
        self.append_commit(None, record)
    }

    /// 追加一条**已签加入事件**：relay `approve` 会把设备证书 CDE 原字节作为裸记录
    /// 追加到控制链（rev 1 genesis 同理）；客户端据此认定"链上存在加入事件"。
    fn push_join(&mut self, member: &TestMember) -> [u8; 32] {
        self.push_raw(member.cert.encode().expect("成员证书编码"))
    }

    fn append_commit(&mut self, outer_kind: Option<&str>, record: Vec<u8>) -> [u8; 32] {
        let mut input = Vec::with_capacity(64 + record.len());
        input.extend_from_slice(&self.prev);
        input.extend_from_slice(&record);
        let hash: [u8; 32] = Sha256::digest(&input).into();
        self.rev += 1;
        let mut entry = vec![
            (0, cbor::uint(self.rev)),
            (1, cbor::bytes(self.prev)),
            (2, cbor::bytes(hash)),
            (3, cbor::bytes(record)),
        ];
        if let Some(kind) = outer_kind {
            entry.push((4, cbor::text(kind)));
        }
        self.commits.push(cbor::CdeValue::Map(entry));
        self.prev = hash;
        hash
    }

    fn sign_record(
        &self,
        kind: WireControlKind,
        body: &[u8],
        path: &str,
        event: Option<Vec<u8>>,
    ) -> Vec<u8> {
        self.sign_record_with(kind, body, path, event, &self.manager, self.manager_rev)
    }

    /// 用指定管理 key / `manager_rev` 签一条 typed 控制记录。
    ///
    /// 负例需要"旧 manager 在后续 rev 仍能产生合法签名"：签名本身有效，但 rev 已不是
    /// 当时有效管理权，客户端必须 fail-stop。
    fn sign_record_with(
        &self,
        kind: WireControlKind,
        body: &[u8],
        path: &str,
        event: Option<Vec<u8>>,
        signer: &ManagementKey,
        manager_rev: u64,
    ) -> Vec<u8> {
        let request_id = [self.next_request; 16];
        let expected_control_rev = self.rev;
        let body_digest: [u8; 32] = Sha256::digest(
            management_request_cde(HttpMethod::Post, path, body).expect("request cde"),
        )
        .into();
        let input = management_signing_input(
            &CONTROL_INSTANCE,
            &CONTROL_VAULT,
            &request_id,
            expected_control_rev,
            &body_digest,
        )
        .expect("management signing input");
        let signature = signer.sign(&input);
        SignedControlRecord {
            kind,
            request_id,
            manager_rev,
            expected_control_rev,
            signed_body: body.to_vec(),
            signature: signature.to_vec(),
            event,
        }
        .encode()
        .expect("signed control record")
    }

    fn push_typed(
        &mut self,
        kind: WireControlKind,
        path: &str,
        event: Option<Vec<u8>>,
    ) -> [u8; 32] {
        self.push_typed_with_body(kind, &[], path, event)
    }

    fn push_typed_with_body(
        &mut self,
        kind: WireControlKind,
        body: &[u8],
        path: &str,
        event: Option<Vec<u8>>,
    ) -> [u8; 32] {
        self.next_request = self.next_request.wrapping_add(1);
        let record = self.sign_record(kind, body, path, event);
        self.append_commit(Some(kind.as_str()), record)
    }

    fn push_typed_signed_by(
        &mut self,
        kind: WireControlKind,
        body: &[u8],
        path: &str,
        event: Option<Vec<u8>>,
        signer: &ManagementKey,
        manager_rev: u64,
    ) -> [u8; 32] {
        self.next_request = self.next_request.wrapping_add(1);
        let record = self.sign_record_with(kind, body, path, event, signer, manager_rev);
        self.append_commit(Some(kind.as_str()), record)
    }

    fn push_begin(&mut self) -> [u8; 32] {
        let path = format!(
            "/v1/vaults/{}/rotations",
            menghunote_sync::base64url::encode(&CONTROL_VAULT)
        );
        self.push_typed(WireControlKind::RotationBegin, &path, None)
    }

    /// 追加一条带不同已签 body 的 `rotation.begin`（用于"替换前缀"负例）。
    fn push_begin_revoking(&mut self, revoke: &[DeviceId]) -> [u8; 32] {
        let path = format!(
            "/v1/vaults/{}/rotations",
            menghunote_sync::base64url::encode(&CONTROL_VAULT)
        );
        let body = menghunote_sync_proto::RotationBeginBodyV1 {
            expected_epoch: 1,
            expected_control_rev: self.rev,
            request_id: [0u8; 16],
            revoke: revoke.to_vec(),
        }
        .encode()
        .expect("rotation.begin body");
        self.push_typed_with_body(WireControlKind::RotationBegin, &body, &path, None)
    }

    /// 追加一条携带完整 activation T + 旧/新管理双签的激活记录。
    ///
    /// 前提：调用方刚用 `push_manager_transition` 把管理权切到 `self.manager`
    /// （`self.manager_rev`）。控制记录本身由**旧**管理 key（`old_manager`，
    /// rev = `self.manager_rev - 1`）签名——与 relay `activate` 一致；T 由旧、新
    /// 两把 key 双签，`prev_control_hash` 钉住当前链尾。
    fn push_activated_with_t(
        &mut self,
        old_manager: &ManagementKey,
        rotation_id: [u8; 16],
        from_epoch: u64,
        to_epoch: u64,
    ) -> [u8; 32] {
        let new_manager = self.manager.clone();
        let new_rev = self.manager_rev;
        let old_rev = new_rev.checked_sub(1).expect("管理 rev 必须 >0");
        let covered_seq = 6;
        let material = menghunote_sync::rotation_slots::ActivationTMaterial {
            vault_id: CONTROL_VAULT,
            instance_id: CONTROL_INSTANCE,
            rotation_id,
            from_epoch,
            to_epoch,
            freeze_seq: covered_seq,
            old_manager_rev: old_rev,
            new_manager_rev: new_rev,
            new_manager_public: new_manager.public_key(),
            prev_control_hash: self.prev,
            expected_control_rev: self.rev + 1,
            primary_checkpoint: [0x41; 16],
            primary_commit_digest: [0xA1; 32],
            fallback_checkpoint: [0x42; 16],
            fallback_commit_digest: [0xB2; 32],
            slot_set_digest: [0x38; 32],
            member_cert_digest: [0x39; 32],
            grants_digest: [0x45; 32],
            old_asset_set_digest: [0x3B; 32],
            new_asset_set_digest: [0x3C; 32],
        };
        let activation_t =
            menghunote_sync::rotation_slots::build_activation_t(&material).expect("activation T");
        let t_input = menghunote_sync_proto::activation_t_signing_input(&activation_t)
            .expect("T signing input");
        let old_t_signature = old_manager.sign(&t_input).to_vec();
        let new_t_signature = new_manager.sign(&t_input).to_vec();
        let event = menghunote_sync_proto::EpochActivated {
            vault_id: CONTROL_VAULT,
            instance_id: CONTROL_INSTANCE,
            rotation_id,
            from_epoch,
            to_epoch,
            covered_seq,
            primary_checkpoint: [0x41; 16],
            fallback_checkpoint: [0x42; 16],
            activation_commit: [0x43; 32],
            member_set_hash: [0x44; 32],
            grants_digest: [0x45; 32],
            control_rev: self.rev + 1,
            auth_rev: 3,
            activation_t: Some(activation_t.encode().expect("T encode")),
            old_t_signature: Some(old_t_signature),
            new_t_signature: Some(new_t_signature),
        }
        .encode()
        .expect("epoch activated event");
        let path = format!(
            "/v1/vaults/{}/rotations/{}/activate",
            menghunote_sync::base64url::encode(&CONTROL_VAULT),
            menghunote_sync::base64url::encode(&rotation_id)
        );
        self.push_typed_signed_by(
            WireControlKind::EpochActivated,
            &[],
            &path,
            Some(event),
            old_manager,
            old_rev,
        )
    }

    /// 追加一条任意 rev / 任意 epoch 跨度的管理权转换（旧/新管理 key 双签）。
    ///
    /// `from_epoch`/`to_epoch` 直接写进被双签的 `ManagerTransitionV1`；restart 允许
    /// `to > from + 1`（ADR 0006 §33），本 helper 用于验证历史多段转换 + 跳 epoch。
    fn push_manager_transition(
        &mut self,
        new_manager: &ManagementKey,
        rotation_id: [u8; 16],
        from_epoch: u64,
        to_epoch: u64,
        old_rev: u64,
        new_rev: u64,
    ) {
        let transition = menghunote_sync::rotation::ManagerTransitionV1 {
            vault_id: CONTROL_VAULT,
            instance_id: CONTROL_INSTANCE,
            rotation_id,
            from_epoch,
            to_epoch,
            old_rev,
            new_rev,
            new_pubkey: new_manager.public_key(),
            covered_seq: 6,
        }
        .encode()
        .expect("manager transition");
        let old_signature = self.manager.sign(&transition);
        let new_signature = new_manager.sign(&transition);
        self.manager_chain.push(cbor::CdeValue::Map(vec![
            (0, cbor::uint(new_rev)),
            (1, cbor::bytes(new_manager.public_key())),
            (2, cbor::bytes(transition)),
            (3, cbor::uint(old_rev)),
            (4, cbor::bytes(old_signature.to_vec())),
            (5, cbor::bytes(new_signature.to_vec())),
        ]));
        self.manager = new_manager.clone();
        self.manager_rev = new_rev;
    }

    /// 追加管理链 rev2（旧/新管理 key 双签），用于验证历史证书仍可由旧 key 验签。
    fn push_manager_rev2(&mut self, new_manager: &ManagementKey, rotation_id: [u8; 16]) {
        self.push_manager_transition(new_manager, rotation_id, 1, 2, 1, 2);
    }

    fn head_hash(&self) -> [u8; 32] {
        let last = self.commits.last().expect("至少 genesis");
        let fields = cbor::value_map(last).expect("commit map");
        cbor::bytes_fixed(&fields, 2).expect("record hash")
    }

    /// 取指定 rev 的 `signed_record` 原字节（用于把同一条已签记录拼到替换过前缀的链上）。
    fn record_bytes(&self, rev: u64) -> Vec<u8> {
        let index = usize::try_from(rev.checked_sub(1).expect("rev")).expect("rev 索引");
        let fields = cbor::value_map(&self.commits[index]).expect("commit map");
        cbor::bytes_field(&fields, 3).expect("signed record")
    }

    fn page(&self, commits: &[cbor::CdeValue], members: &[cbor::CdeValue]) -> HttpResponse {
        cbor_response(vec![
            (0, cbor::array(commits.to_vec())),
            (1, cbor::uint(self.rev)),
            (2, cbor::uint(1)),
            (3, cbor::array(members.to_vec())),
            (4, cbor::array(Vec::new())),
            (5, cbor::array(self.manager_chain.clone())),
        ])
    }

    fn full_page(&self, members: &[cbor::CdeValue]) -> HttpResponse {
        self.page(&self.commits, members)
    }
}

struct TestMember {
    device_id: DeviceId,
    cert: DeviceCertV1,
}

fn control_member(seed: u8, _manager: &ManagementKey, issued_manager_rev: u64) -> TestMember {
    let signing = DeviceSigningKey::from_seed(Secret32::from_array([seed; 32]));
    let hpke = DeviceHpkeKey::from_secret(Secret32::from_array([seed.wrapping_add(1); 32]));
    TestMember {
        device_id: [seed; 16],
        cert: DeviceCertV1 {
            vault_id: CONTROL_VAULT,
            device_id: [seed; 16],
            sign_ed25519_pub: signing.public_key(),
            hpke_x25519_pub: hpke.public_key(),
            cert_serial: 1,
            issued_manager_rev,
            rights: Rights(Rights::ALL),
        },
    }
}

fn member_fields(member: &TestMember, status: &str, signer: &ManagementKey) -> cbor::CdeValue {
    let signature = signer.sign(&device_cert_signed_bytes(&member.cert).expect("证书签名输入"));
    cbor::CdeValue::Map(vec![
        (0, cbor::bytes(member.device_id)),
        (1, cbor::bytes(member.cert.encode().expect("证书编码"))),
        (2, cbor::text(status)),
        (3, cbor::bytes(member.cert.sign_ed25519_pub)),
        (4, cbor::bytes(member.cert.hpke_x25519_pub)),
        (5, cbor::bytes(signature.to_vec())),
    ])
}

fn pending_member_fields(device_id: DeviceId) -> cbor::CdeValue {
    cbor::CdeValue::Map(vec![
        (0, cbor::bytes(device_id)),
        (1, cbor::bytes(Vec::new())),
        (2, cbor::text("pending")),
        (3, cbor::bytes([0u8; 32])),
        (4, cbor::bytes([0u8; 32])),
    ])
}

fn rotations_path() -> String {
    format!(
        "/v1/vaults/{}/rotations",
        menghunote_sync::base64url::encode(&CONTROL_VAULT)
    )
}

#[test]
fn control_chain_paginates_beyond_256_records() {
    ensure_test_identity_wrap();
    let manager = ManagementKey::from_seed(Secret32::from_array([0x51; 32]));
    let mut chain = ControlChain::new(manager.clone());
    for _ in 0..299 {
        chain.push_begin();
    }
    assert_eq!(chain.rev, 300);
    let member = control_member(0x61, &manager, 1);
    let members = vec![member_fields(&member, "active", &manager)];

    let mut responses = control_auth_responses();
    responses.push(control_state_response(300, 1));
    responses.push(chain.page(&chain.commits[..256], &members));
    responses.push(chain.page(&chain.commits[256..], &members));
    let (mut client, log) = control_client(responses, &manager);

    let kinds = RelayClient::control_kinds(&mut client).expect(">256 条必须分页续验到 head");
    assert_eq!(kinds.len(), 300);
    assert_eq!(kinds.first(), Some(&(1, "raw".to_owned())));
    assert_eq!(kinds.last(), Some(&(300, "rotation.begin".to_owned())));
    assert!(client.device_cert(&member.device_id).unwrap().is_some());
    let paths = log.control_paths();
    assert_eq!(
        paths,
        vec![
            format!(
                "/v1/vaults/{}/control?after_rev=0",
                menghunote_sync::base64url::encode(&CONTROL_VAULT)
            ),
            format!(
                "/v1/vaults/{}/control?after_rev=256",
                menghunote_sync::base64url::encode(&CONTROL_VAULT)
            ),
        ]
    );
}

#[test]
fn control_pin_rejects_lower_rev_and_replaced_genesis() {
    ensure_test_identity_wrap();
    let manager = ManagementKey::from_seed(Secret32::from_array([0x51; 32]));
    let mut chain = ControlChain::new(manager.clone());
    chain.push_begin();
    let member = control_member(0x62, &manager, 1);
    let members = vec![member_fields(&member, "active", &manager)];

    // 低 pin：relay head=2，本地 pin=3 → fail-stop。
    let mut responses = control_auth_responses();
    responses.push(control_state_response(2, 1));
    responses.push(chain.full_page(&members));
    let (client, _log) = control_client(responses, &manager);
    let mut client = client.with_control_pin(ControlPin::new(CONTROL_INSTANCE, 3));
    assert!(matches!(
        RelayClient::control_kinds(&mut client),
        Err(menghunote_sync::SyncError::RelayWire(
            "控制链低于本地 pin（回退）"
        ))
    ));

    // 同 rev 替换 genesis：链尾 record_hash 与 pin 不符 → fail-stop。
    let original_hash = chain.head_hash();
    let other_genesis = cbor::encode(vec![(0, cbor::bytes([0xee; 16]))]).expect("另一份 genesis");
    let mut replaced = ControlChain::with_genesis(manager.clone(), other_genesis);
    replaced.push_begin();
    let mut responses = control_auth_responses();
    responses.push(control_state_response(2, 1));
    responses.push(replaced.full_page(&members));
    let (client, _log) = control_client(responses, &manager);
    let mut client = client
        .with_control_pin(ControlPin::new(CONTROL_INSTANCE, 2).with_record_hash(original_hash));
    assert!(matches!(
        RelayClient::control_kinds(&mut client),
        Err(menghunote_sync::SyncError::Equivocation(message)) if message.contains("record_hash")
    ));

    // head 前进但旧 rev 被替换：pin 的 record_hash 必须仍是新链前缀 → fail-stop。
    replaced.push_begin();
    assert_eq!(replaced.rev, 3);
    let mut responses = control_auth_responses();
    responses.push(control_state_response(3, 1));
    responses.push(replaced.full_page(&members));
    let (client, _log) = control_client(responses, &manager);
    let mut client = client
        .with_control_pin(ControlPin::new(CONTROL_INSTANCE, 2).with_record_hash(original_hash));
    assert!(matches!(
        RelayClient::control_kinds(&mut client),
        Err(menghunote_sync::SyncError::Equivocation(message)) if message.contains("record_hash")
    ));
}

#[test]
fn control_chain_rejects_missing_reordered_and_empty_pages() {
    ensure_test_identity_wrap();
    let manager = ManagementKey::from_seed(Secret32::from_array([0x51; 32]));
    let mut chain = ControlChain::new(manager.clone());
    for _ in 0..299 {
        chain.push_begin();
    }
    let member = control_member(0x63, &manager, 1);
    let members = vec![member_fields(&member, "active", &manager)];
    let first_page = chain.page(&chain.commits[..256], &members);

    // 缺页：第二页少了 rev=258。
    let mut missing = chain.commits[256..].to_vec();
    missing.remove(1);
    let mut responses = control_auth_responses();
    responses.push(control_state_response(300, 1));
    responses.push(first_page.clone());
    responses.push(chain.page(&missing, &members));
    let (mut client, _log) = control_client(responses, &manager);
    assert!(matches!(
        RelayClient::control_kinds(&mut client),
        Err(menghunote_sync::SyncError::RelayWire("控制链 rev 不连续"))
    ));

    // 重排：第二页前两条顺序颠倒。
    let mut reordered = chain.commits[256..].to_vec();
    reordered.swap(0, 1);
    let mut responses = control_auth_responses();
    responses.push(control_state_response(300, 1));
    responses.push(first_page.clone());
    responses.push(chain.page(&reordered, &members));
    let (mut client, _log) = control_client(responses, &manager);
    assert!(matches!(
        RelayClient::control_kinds(&mut client),
        Err(menghunote_sync::SyncError::RelayWire("控制链 rev 不连续"))
    ));

    // 空页但 head 还没到：缺页 fail-stop。
    let mut responses = control_auth_responses();
    responses.push(control_state_response(300, 1));
    responses.push(first_page);
    responses.push(chain.page(&[], &members));
    let (mut client, _log) = control_client(responses, &manager);
    assert!(matches!(
        RelayClient::control_kinds(&mut client),
        Err(menghunote_sync::SyncError::RelayWire(
            "控制链分页未前进（缺页）"
        ))
    ));
}

#[test]
fn revoked_member_cert_verifies_history_but_stays_out_of_active_set() {
    ensure_test_identity_wrap();
    let old_manager = ManagementKey::from_seed(Secret32::from_array([0x51; 32]));
    let new_manager = ManagementKey::from_seed(Secret32::from_array([0x52; 32]));
    let rotation_id = [0x21; 16];
    let mut chain = ControlChain::new(old_manager.clone());
    chain.push_begin();
    chain.push_manager_rev2(&new_manager, rotation_id);
    chain.push_activated_with_t(&old_manager, rotation_id, 1, 2);

    let active = control_member(0x64, &new_manager, 2);
    let revoked = control_member(0x65, &old_manager, 1);
    let expired = control_member(0x67, &old_manager, 1);
    // active 集合只认已签加入事件：active 设备必须在控制链上有一条裸证书记录。
    // revoked/expired 故意不加：它们只用于 ≤S 历史验签，relay 的 status 不能授权。
    chain.push_join(&active);
    assert_eq!(chain.rev, 4);
    let active_fields = member_fields(&active, "active", &new_manager);
    let revoked_fields = member_fields(&revoked, "revoked", &old_manager);
    let expired_fields = member_fields(&expired, "expired", &old_manager);
    let pending_id: DeviceId = [0x66; 16];
    let members = vec![
        active_fields.clone(),
        revoked_fields.clone(),
        expired_fields.clone(),
        pending_member_fields(pending_id),
    ];

    let mut responses = control_auth_responses();
    responses.push(control_state_response(chain.rev, 2));
    responses.push(chain.full_page(&members));
    let (mut client, _log) = control_client(responses, &old_manager);

    let kinds = RelayClient::control_kinds(&mut client).expect("含吊销/待批准成员的控制链");
    assert_eq!(
        kinds,
        vec![
            (1, "raw".to_owned()),
            (2, "rotation.begin".to_owned()),
            (3, "epoch.activated".to_owned()),
            (4, "raw".to_owned()),
        ],
        "S=freeze_seq 的激活记录与 active 加入事件都必须在链上"
    );
    // ≤S 历史验签仍可使用吊销设备的旧证书（旧管理 key 已在管理链上）。
    let revoked_cert = client
        .device_cert(&revoked.device_id)
        .unwrap()
        .expect("吊销证书仍应可读，用于验证 ≤S 记录");
    assert_eq!(revoked_cert.issued_manager_rev, 1);
    let expired_cert = client
        .device_cert(&expired.device_id)
        .unwrap()
        .expect("过期证书仍应可读，用于历史验签");
    assert_eq!(expired_cert.issued_manager_rev, 1);
    assert!(client.device_cert(&active.device_id).unwrap().is_some());
    assert!(
        client.device_cert(&pending_id).unwrap().is_none(),
        "待批准设备没有证书，不得进入成员集合"
    );

    // 当前 active 集合只含 active 设备。
    let active_keys = RelayClient::active_member_hpke_keys(&mut client).unwrap();
    assert_eq!(
        active_keys,
        vec![(active.device_id, active.cert.hpke_x25519_pub)]
    );
    let active_input = vec![MemberCertDigestInputV1 {
        device_id: active.device_id,
        certificate: active.cert.encode().expect("active cert"),
        certificate_signature: Some(
            new_manager.sign(&device_cert_signed_bytes(&active.cert).expect("cert input")),
        )
        .map(|signature| signature.to_vec()),
    }];
    let expected_digest: [u8; 32] = Sha256::digest(member_cert_digest(&active_input)).into();
    assert_eq!(
        client.active_member_cert_digest().unwrap(),
        expected_digest,
        "吊销设备不得进入当前 active 成员集合"
    );

    // 吊销证书的签名仍被强制验证：篡改后整条控制链必须拒绝。
    let mut tampered = cbor::value_map(&revoked_fields).expect("member map");
    let mut signature = cbor::bytes_field(&tampered, 5).expect("signature");
    signature[0] ^= 0x01;
    tampered[5].1 = cbor::bytes(signature);
    let members = vec![
        active_fields,
        cbor::CdeValue::Map(tampered),
        expired_fields,
        pending_member_fields(pending_id),
    ];
    let mut responses = control_auth_responses();
    responses.push(control_state_response(chain.rev, 2));
    responses.push(chain.full_page(&members));
    let (mut client, _log) = control_client(responses, &old_manager);
    assert!(matches!(
        RelayClient::control_kinds(&mut client),
        Err(menghunote_sync::SyncError::Signature)
    ));
}

#[test]
fn control_chain_verifies_historical_manager_transitions_and_epoch_jumps() {
    ensure_test_identity_wrap();
    // T21：多个历史管理权转换 + 跳 epoch（restart 形态）仍必须逐段用**当时**的
    // 管理 key 验签；≤S 的历史吊销证书仍可用于验历史，但不得进入 active 集合。
    let m1 = ManagementKey::from_seed(Secret32::from_array([0x51; 32]));
    let m2 = ManagementKey::from_seed(Secret32::from_array([0x52; 32]));
    let m3 = ManagementKey::from_seed(Secret32::from_array([0x53; 32]));
    let first_rotation = [0x71; 16];
    let second_rotation = [0x72; 16];

    let mut chain = ControlChain::new(m1.clone());
    // rev1 genesis → 管理 rev2；epoch 1 → 3 是跳 epoch（没有 epoch 2 的激活记录）。
    chain.push_manager_transition(&m2, first_rotation, 1, 3, 1, 2);
    chain.push_activated_with_t(&m1, first_rotation, 1, 3);
    // rev2 → 管理 rev3；epoch 3 → 4。
    chain.push_manager_transition(&m3, second_rotation, 3, 4, 2, 3);
    chain.push_activated_with_t(&m2, second_rotation, 3, 4);

    let active = control_member(0x73, &m3, 3);
    let revoked = control_member(0x74, &m1, 1);
    // active 集合只认已签加入事件：m3 签发的证书必须作为裸记录出现在控制链上。
    chain.push_join(&active);
    assert_eq!(chain.rev, 4);

    let active_fields = member_fields(&active, "active", &m3);
    let revoked_fields = member_fields(&revoked, "revoked", &m1);
    let members = vec![active_fields.clone(), revoked_fields.clone()];

    let mut responses = control_auth_responses();
    responses.push(control_state_response(chain.rev, 4));
    responses.push(chain.full_page(&members));
    let (mut client, _log) = control_client(responses, &m1);
    let kinds = RelayClient::control_kinds(&mut client).expect("历史多段管理链必须通过");
    assert_eq!(
        kinds,
        vec![
            (1, "raw".to_owned()),
            (2, "epoch.activated".to_owned()),
            (3, "epoch.activated".to_owned()),
            (4, "raw".to_owned()),
        ],
        "两条激活记录与 active 加入事件都必须在控制链上（rev2/rev3/rev4）"
    );

    // 历史吊销证书（rev1 旧管理 key 签发）仍可读，用于 ≤S 历史验签。
    let revoked_cert = client
        .device_cert(&revoked.device_id)
        .unwrap()
        .expect("历史吊销证书必须保留");
    assert_eq!(revoked_cert.issued_manager_rev, 1);
    assert_eq!(revoked_cert.sign_ed25519_pub, revoked.cert.sign_ed25519_pub);
    let active_cert = client
        .device_cert(&active.device_id)
        .unwrap()
        .expect("新管理 key 签发证书可读");
    assert_eq!(active_cert.issued_manager_rev, 3);
    let active_keys = RelayClient::active_member_hpke_keys(&mut client).expect("active keys");
    assert_eq!(
        active_keys,
        vec![(active.device_id, active.cert.hpke_x25519_pub)],
        "被吊销设备不得进入 active 集合"
    );

    // 负例 1：第二段管理转换的旧管理签名被篡改 → Signature。
    let mut tampered_chain = ControlChain::new(m1.clone());
    tampered_chain.push_manager_transition(&m2, first_rotation, 1, 3, 1, 2);
    tampered_chain.push_activated_with_t(&m1, first_rotation, 1, 3);
    tampered_chain.push_manager_transition(&m3, second_rotation, 3, 4, 2, 3);
    tampered_chain.push_activated_with_t(&m2, second_rotation, 3, 4);
    let last = tampered_chain.manager_chain.len() - 1;
    let mut entry = cbor::value_map(&tampered_chain.manager_chain[last]).expect("manager entry");
    let mut old_signature = cbor::bytes_field(&entry, 4).expect("old signature");
    old_signature[0] ^= 0x01;
    entry[4].1 = cbor::bytes(old_signature);
    tampered_chain.manager_chain[last] = cbor::CdeValue::Map(entry);
    let mut responses = control_auth_responses();
    responses.push(control_state_response(tampered_chain.rev, 4));
    responses.push(tampered_chain.full_page(&members));
    let (mut client, _log) = control_client(responses, &m1);
    assert!(matches!(
        RelayClient::control_kinds(&mut client),
        Err(menghunote_sync::SyncError::Signature)
    ));

    // 负例 2：缺少第二段管理转换，但控制记录已经用 rev3 签名 → fail-stop。
    let mut missing_chain = ControlChain::new(m1.clone());
    missing_chain.push_manager_transition(&m2, first_rotation, 1, 3, 1, 2);
    missing_chain.push_activated_with_t(&m1, first_rotation, 1, 3);
    missing_chain.push_manager_transition(&m3, second_rotation, 3, 4, 2, 3);
    missing_chain.push_activated_with_t(&m2, second_rotation, 3, 4);
    missing_chain.manager_chain.pop();
    let mut responses = control_auth_responses();
    responses.push(control_state_response(missing_chain.rev, 4));
    responses.push(missing_chain.full_page(&members));
    let (mut client, _log) = control_client(responses, &m1);
    assert!(matches!(
        RelayClient::control_kinds(&mut client),
        Err(menghunote_sync::SyncError::RelayWire(
            "activation T 的新管理 rev 缺少对应管理权转换"
        ))
    ));

    // 负例 3：管理转换把 epoch 往回跳（to <= from）→ fail-stop。
    let mut backward = ControlChain::new(m1.clone());
    backward.push_manager_transition(&m2, first_rotation, 3, 2, 1, 2);
    let mut responses = control_auth_responses();
    responses.push(control_state_response(1, 1));
    responses.push(backward.full_page(&members));
    let (mut client, _log) = control_client(responses, &m1);
    assert!(matches!(
        RelayClient::control_kinds(&mut client),
        Err(menghunote_sync::SyncError::RelayWire(
            "管理转换链字段/rev 不连续"
        ))
    ));
}

#[test]
fn old_manager_cert_join_after_manager_transition_is_rejected_with_pin() {
    ensure_test_identity_wrap();
    // 评审 0003 P0-A 负例：保留真实 1→2 前缀 + pin，持旧 manager1 私钥的攻击者在
    // 合法前缀之后追加一份 issued_manager_rev=1 的 attacker 证书（重算 hash、标
    // active）。它虽然能通过"按历史 rev 验签"，但 manager1 已不是当时有效管理权，
    // 必须在进入成员集合/HPKE keys/grant 规划之前 fail-stop。
    let m1 = ManagementKey::from_seed(Secret32::from_array([0x51; 32]));
    let m2 = ManagementKey::from_seed(Secret32::from_array([0x52; 32]));
    let rotation_id = [0x81; 16];
    let genesis = control_member(0x81, &m1, 1);
    let mut chain = ControlChain::with_genesis(m1.clone(), genesis.cert.encode().expect("genesis"));
    chain.push_manager_rev2(&m2, rotation_id);
    chain.push_activated_with_t(&m1, rotation_id, 1, 2);
    let pinned_rev = chain.rev;
    let pinned_hash = chain.head_hash();
    assert_eq!(pinned_rev, 2);

    let attacker = control_member(0x82, &m1, 1);
    chain.push_join(&attacker);
    assert_eq!(chain.rev, 3);
    let members = vec![
        member_fields(&genesis, "active", &m1),
        member_fields(&attacker, "active", &m1),
    ];

    let mut responses = control_auth_responses();
    responses.push(control_state_response(chain.rev, 2));
    responses.push(chain.full_page(&members));
    // 第二次消费（active keys）仍会重新拉 /control 并再次被拒。
    responses.push(control_state_response(chain.rev, 2));
    responses.push(chain.full_page(&members));
    let (client, _log) = control_client(responses, &m1);
    let mut client = client.with_control_pin(
        ControlPin::new(CONTROL_INSTANCE, pinned_rev).with_record_hash(pinned_hash),
    );

    assert!(matches!(
        RelayClient::control_kinds(&mut client),
        Err(menghunote_sync::SyncError::RelayWire(
            "成员加入证书的 issued_manager_rev 不是当前有效管理权"
        ))
    ));
    assert_eq!(
        client.cached_control_rev(),
        None,
        "fail-stop 不得安装部分 ControlState"
    );
    assert!(matches!(
        RelayClient::active_member_hpke_keys(&mut client),
        Err(menghunote_sync::SyncError::RelayWire(
            "成员加入证书的 issued_manager_rev 不是当前有效管理权"
        ))
    ));
    assert_eq!(
        client.cached_control_rev(),
        None,
        "attacker 证书永不进入 active/HPKE 集合"
    );
}

#[test]
fn effective_manager_authority_advances_only_via_verified_t() {
    ensure_test_identity_wrap();
    // 合法 T + 合法前缀 → 通过；推进后 manager2 可继续签管理动作；
    // 旧 manager1 在后续 rev 的"签名有效但 rev 已失效"记录必须 fail-stop。
    let m1 = ManagementKey::from_seed(Secret32::from_array([0x51; 32]));
    let m2 = ManagementKey::from_seed(Secret32::from_array([0x52; 32]));
    let rotation_id = [0x83; 16];
    let genesis = control_member(0x83, &m1, 1);
    let path = rotations_path();
    let mut chain = ControlChain::with_genesis(m1.clone(), genesis.cert.encode().expect("genesis"));
    chain.push_manager_rev2(&m2, rotation_id);
    chain.push_activated_with_t(&m1, rotation_id, 1, 2);
    // 管理权推进到 rev2 后，manager2 的 typed 记录必须被接受。
    chain.push_typed_signed_by(WireControlKind::RotationBegin, &[], &path, None, &m2, 2);
    let members = vec![member_fields(&genesis, "active", &m1)];

    let mut responses = control_auth_responses();
    responses.push(control_state_response(chain.rev, 2));
    responses.push(chain.full_page(&members));
    let (mut client, _log) = control_client(responses, &m1);
    let kinds = RelayClient::control_kinds(&mut client).expect("合法 T + 合法前缀必须通过");
    assert_eq!(
        kinds,
        vec![
            (1, "raw".to_owned()),
            (2, "epoch.activated".to_owned()),
            (3, "rotation.begin".to_owned()),
        ]
    );
    assert_eq!(
        RelayClient::active_member_hpke_keys(&mut client).expect("active keys"),
        vec![(genesis.device_id, genesis.cert.hpke_x25519_pub)],
        "合法 T 不得把 genesis 设备挤出 active"
    );

    // 负例：同一前缀，但 rev3 用旧 manager1 的合法签名 + manager_rev=1。
    let mut stale = ControlChain::with_genesis(m1.clone(), genesis.cert.encode().expect("genesis"));
    stale.push_manager_rev2(&m2, rotation_id);
    stale.push_activated_with_t(&m1, rotation_id, 1, 2);
    stale.push_typed_signed_by(WireControlKind::RotationBegin, &[], &path, None, &m1, 1);
    let mut responses = control_auth_responses();
    responses.push(control_state_response(stale.rev, 2));
    responses.push(stale.full_page(&members));
    let (mut client, _log) = control_client(responses, &m1);
    assert!(matches!(
        RelayClient::control_kinds(&mut client),
        Err(menghunote_sync::SyncError::RelayWire(
            "控制记录 manager_rev 不是当前有效管理权"
        ))
    ));

    // 负例：from_epoch 不是该位置的有效 epoch（第一个 T 已把有效 epoch 推到 2）。
    // T/事件与双签都自洽，但旧管理权位置对不上 → 必须在推进任何状态前拒绝。
    let m3 = ManagementKey::from_seed(Secret32::from_array([0x53; 32]));
    let second_rotation = [0x84; 16];
    let mut wrong_from =
        ControlChain::with_genesis(m1.clone(), genesis.cert.encode().expect("genesis"));
    wrong_from.push_manager_rev2(&m2, rotation_id);
    wrong_from.push_activated_with_t(&m1, rotation_id, 1, 2);
    wrong_from.push_manager_transition(&m3, second_rotation, 1, 3, 2, 3);
    wrong_from.push_activated_with_t(&m2, second_rotation, 1, 3);
    let mut responses = control_auth_responses();
    responses.push(control_state_response(wrong_from.rev, 3));
    responses.push(wrong_from.full_page(&members));
    let (mut client, _log) = control_client(responses, &m1);
    assert!(matches!(
        RelayClient::control_kinds(&mut client),
        Err(menghunote_sync::SyncError::RelayWire(
            "epoch.activated 的 from_epoch 与当前有效 epoch 不符"
        ))
    ));
}

#[test]
fn activation_t_prev_control_hash_rejects_replaced_prefix() {
    ensure_test_identity_wrap();
    // 评审 0003 P0-A：恶意 relay 把 activation 记录之前的 rev2（rotation.begin）
    // 换成另一条已签记录并重算整条 hash 链——relay 自洽 hash 全对，但 T 里被签的
    // prev_control_hash 仍指向原前缀，必须拒绝。
    let m1 = ManagementKey::from_seed(Secret32::from_array([0x51; 32]));
    let m2 = ManagementKey::from_seed(Secret32::from_array([0x52; 32]));
    let rotation_id = [0x84; 16];
    let genesis = control_member(0x84, &m1, 1);
    let genesis_bytes = genesis.cert.encode().expect("genesis");
    let members = vec![member_fields(&genesis, "active", &m1)];

    // 合法链：rev1 genesis → rev2 rotation.begin → rev3 activation T。
    let mut legit = ControlChain::with_genesis(m1.clone(), genesis_bytes.clone());
    legit.push_begin();
    legit.push_manager_rev2(&m2, rotation_id);
    legit.push_activated_with_t(&m1, rotation_id, 1, 2);
    assert_eq!(legit.rev, 3);
    let activation_record = legit.record_bytes(3);

    let mut responses = control_auth_responses();
    responses.push(control_state_response(legit.rev, 2));
    responses.push(legit.full_page(&members));
    let (mut client, _log) = control_client(responses, &m1);
    RelayClient::control_kinds(&mut client).expect("合法前缀 + 合法 T 必须通过");

    // 替换前缀：rev2 换成带 revoke body 的 rotation.begin，再拼同一条 activation
    // 记录并重算 stored prev/record_hash（stored 链完全自洽）。
    let mut replaced = ControlChain::with_genesis(m1.clone(), genesis_bytes);
    replaced.push_begin_revoking(&[]);
    replaced.push_manager_rev2(&m2, rotation_id);
    replaced.append_commit(Some("epoch.activated"), activation_record);
    assert_ne!(replaced.head_hash(), legit.head_hash());
    let mut responses = control_auth_responses();
    responses.push(control_state_response(replaced.rev, 2));
    responses.push(replaced.full_page(&members));
    let (mut client, _log) = control_client(responses, &m1);
    assert!(matches!(
        RelayClient::control_kinds(&mut client),
        Err(menghunote_sync::SyncError::RelayWire(
            "activation T 的 prev_control_hash 与链上实际前一条记录不符"
        ))
    ));
}

#[test]
fn revoked_history_cert_verifies_old_objects_but_cannot_rejoin() {
    ensure_test_identity_wrap();
    // 历史吊销证书仍可用于 ≤S 旧对象验签；但管理权推进到 rev2 后，再拿它当新的
    // 加入事件（issued_manager_rev=1）必须 fail-stop，不能产生新授权。
    let m1 = ManagementKey::from_seed(Secret32::from_array([0x51; 32]));
    let m2 = ManagementKey::from_seed(Secret32::from_array([0x52; 32]));
    let rotation_id = [0x85; 16];
    let genesis = control_member(0x85, &m1, 1);
    let revoked = control_member(0x86, &m1, 1);
    let genesis_bytes = genesis.cert.encode().expect("genesis");

    let mut chain = ControlChain::with_genesis(m1.clone(), genesis_bytes.clone());
    chain.push_join(&revoked);
    chain.push_begin_revoking(&[revoked.device_id]);
    chain.push_manager_rev2(&m2, rotation_id);
    chain.push_activated_with_t(&m1, rotation_id, 1, 2);
    let members = vec![
        member_fields(&genesis, "active", &m1),
        member_fields(&revoked, "revoked", &m1),
    ];

    let mut responses = control_auth_responses();
    responses.push(control_state_response(chain.rev, 2));
    responses.push(chain.full_page(&members));
    let (mut client, _log) = control_client(responses, &m1);
    RelayClient::control_kinds(&mut client).expect("含历史吊销证书的控制链必须通过");
    assert_eq!(
        client.device_cert(&revoked.device_id).unwrap(),
        Some(revoked.cert.clone()),
        "吊销证书仍须保留用于 ≤S 历史验签"
    );
    assert_eq!(
        RelayClient::active_member_hpke_keys(&mut client).expect("active keys"),
        vec![(genesis.device_id, genesis.cert.hpke_x25519_pub)],
        "吊销证书不得进入 active/HPKE"
    );

    // 攻击链：同一真实前缀之后重新追加 revoked 的裸加入事件。
    let mut attack = ControlChain::with_genesis(m1.clone(), genesis_bytes);
    attack.push_join(&revoked);
    attack.push_begin_revoking(&[revoked.device_id]);
    attack.push_manager_rev2(&m2, rotation_id);
    attack.push_activated_with_t(&m1, rotation_id, 1, 2);
    attack.push_join(&revoked);
    let mut responses = control_auth_responses();
    responses.push(control_state_response(attack.rev, 2));
    responses.push(attack.full_page(&members));
    let (mut client, _log) = control_client(responses, &m1);
    assert!(matches!(
        RelayClient::control_kinds(&mut client),
        Err(menghunote_sync::SyncError::RelayWire(
            "成员加入证书的 issued_manager_rev 不是当前有效管理权"
        ))
    ));
}

#[test]
fn control_chain_rejects_forged_outer_kind_even_with_consistent_hashes() {
    ensure_test_identity_wrap();
    // T21：外层未签名 kind 只做复核；被签记录内部是 rotation.begin，外层伪造
    // rotation.abort 必须 fail-stop（不能拿"自洽 hash"当授权）。
    let manager = ManagementKey::from_seed(Secret32::from_array([0x51; 32]));
    let mut chain = ControlChain::new(manager.clone());
    chain.push_begin();
    let last = chain.commits.len() - 1;
    let mut commit = cbor::value_map(&chain.commits[last]).expect("commit");
    // 键 4 是 relay 红线下发的外层 kind（不参与 record_hash）。
    commit.retain(|(key, _)| *key != 4);
    commit.push((4, cbor::text("rotation.abort")));
    chain.commits[last] = cbor::CdeValue::Map(commit);

    let member = control_member(0x75, &manager, 1);
    let members = vec![member_fields(&member, "active", &manager)];
    let mut responses = control_auth_responses();
    responses.push(control_state_response(chain.rev, 1));
    responses.push(chain.full_page(&members));
    let (mut client, _log) = control_client(responses, &manager);
    assert!(matches!(
        RelayClient::control_kinds(&mut client),
        Err(menghunote_sync::SyncError::RelayWire(
            "控制记录外层 kind 与签名记录不一致"
        ))
    ));
}
