//! 轮换阶段 D–F（task-103）：资产重加密、两基线独立证据、grants。

mod support;

use std::collections::VecDeque;

use support::*;

use menghunote_storage::ensure_structure_row;
use menghunote_sync::api_version::{RelayApi, RelayApiVersion};
use menghunote_sync::base64url;
use menghunote_sync::cbor;
use menghunote_sync::checkpoint::StaticAssets;
use menghunote_sync::device::{DeviceHpkeKey, ManagementKey};
use menghunote_sync::genesis;
use menghunote_sync::hpke::open_epoch_grant;
use menghunote_sync::http_relay::{HttpRequest, HttpResponse, HttpTransport};
use menghunote_sync::keys::{RootSecretV1, Secret32};
use menghunote_sync::recovery::RecoverySeed;
use menghunote_sync::rotation_slots::grants_digest;
use menghunote_sync::rotation_stage::{self, GrantLeaf};
use menghunote_sync::upload::fetch_object;
use menghunote_sync::SyncError;
use menghunote_sync_proto::{AuthRealm, GrantStageBodyV1, ObjectType};
use sha2::{Digest, Sha256};

fn setup() -> (
    tempfile::TempDir,
    menghunote_storage::Vault,
    genesis::GenesisMaterials,
    FakeRelay,
) {
    let (dir, vault) = fresh_vault();
    ensure_structure_row(&vault).expect("structure");
    vault
        .set_meta(menghunote_sync::engine::RELAY_CURSOR_META, b"c0")
        .expect("写 relay 游标");
    let materials = genesis::build_materials(
        VAULT_ID,
        [0x44; 16],
        "pw",
        1_700_000_000,
        &mut TestRandom::new(0x99_29),
    )
    .expect("genesis");
    let mut relay = FakeRelay::new(&[]);
    relay
        .certs
        .insert(materials.device_id, materials.certificate().clone());
    (dir, vault, materials, relay)
}

fn new_root() -> RootSecretV1 {
    RootSecretV1::new(VAULT_ID, EPOCH + 1, Secret32::from_array([0x5B; 32]))
}

#[test]
fn frozen_assets_are_reencrypted_with_new_root_and_tombstone_is_copied() {
    ensure_test_identity_wrap();
    let (_dir, vault, materials, mut relay) = setup();
    // 两个资产：一个普通，一个"无引用/墓碑"资产（StaticAssets 直接给出）。
    let assets = StaticAssets::new(vec![
        ("text/plain".to_owned(), b"live asset".to_vec()),
        (
            "application/octet-stream".to_owned(),
            b"orphan asset".to_vec(),
        ),
    ])
    .expect("assets");
    let root = new_root();
    let reencrypted = rotation_stage::reencrypt_assets(
        &vault,
        &mut relay,
        VAULT_ID,
        materials.device_id,
        &materials.signing_key,
        &materials.root,
        &root,
        &assets,
        None,
    )
    .expect("reencrypt");
    assert_eq!(reencrypted.len(), 2);
    for asset in &reencrypted {
        let old_id = menghunote_sync::checkpoint::asset_object_id(
            &materials.root,
            &asset.real_sha256,
            asset.byte_len,
            &asset.media_type,
        )
        .expect("旧根 object id");
        assert_ne!(old_id, asset.object_id, "新旧 HMAC object_id 必须不同");
        let total_plain = asset.byte_len + 42 + asset.media_type.len() as u64;
        let (_, bytes) = fetch_object(
            &mut relay,
            &root,
            ObjectType::Asset,
            &asset.object_id,
            part_count_for_test(asset.byte_len),
            total_plain,
            &asset.object_digest,
            &[],
        )
        .expect("new root 解密");
        assert!(!bytes.is_empty());
        assert!(
            fetch_object(
                &mut relay,
                &materials.root,
                ObjectType::Asset,
                &asset.object_id,
                part_count_for_test(asset.byte_len),
                total_plain,
                &asset.object_digest,
                &[],
            )
            .is_err(),
            "旧根不能解新密文"
        );
    }
}

fn part_count_for_test(byte_len: u64) -> u32 {
    // ASSET 明文上限 4MiB；测试资产都很小。
    let _ = byte_len;
    1
}

#[test]
fn two_baselines_have_independent_objects_and_evidence() {
    ensure_test_identity_wrap();
    let (_dir, vault, materials, mut relay) = setup();
    let doc = seed_doc(&vault, "note-1", materials.peer_id_u64());
    doc.insert_scalars(0, "baseline doc").expect("正文");
    commit_local(&vault, &doc);
    let assets =
        StaticAssets::new(vec![("text/plain".to_owned(), b"shared".to_vec())]).expect("asset");
    let root = new_root();
    relay.epoch = EPOCH + 1;
    let first = rotation_stage::prepare_new_epoch_baseline(
        &vault,
        &mut relay,
        VAULT_ID,
        materials.device_id,
        materials.signing_key.clone(),
        root.clone(),
        materials.peer_id_u64(),
        &assets,
    )
    .expect("baseline 1");
    let second = rotation_stage::prepare_new_epoch_baseline(
        &vault,
        &mut relay,
        VAULT_ID,
        materials.device_id,
        materials.signing_key.clone(),
        root.clone(),
        materials.peer_id_u64(),
        &assets,
    )
    .expect("baseline 2");
    assert_ne!(first.report.checkpoint_id, second.report.checkpoint_id);
    let ids1 = rotation_stage::snapshot_object_ids(&first);
    let ids2 = rotation_stage::snapshot_object_ids(&second);
    assert!(
        ids1.iter().all(|id| !ids2.contains(id)),
        "snapshot 对象必须独立"
    );
    let e1 = rotation_stage::capture_baseline_evidence(&mut relay, &root, &first, &assets.hashes())
        .expect("evidence 1");
    let e2 =
        rotation_stage::capture_baseline_evidence(&mut relay, &root, &second, &assets.hashes())
            .expect("evidence 2");
    rotation_stage::assert_independent_baselines(&e1, &e2, 0).expect("independent");
    assert!(
        rotation_stage::assert_independent_baselines(&e1, &e1, 0).is_err(),
        "复用同一 baseline 必须拒绝 ready"
    );
    assert_eq!(e1.asset_count, 1);
    assert_eq!(e1.asset_set_digest, e2.asset_set_digest);
    for doc in e1.docs.iter().filter(|doc| doc.doc_id == "note-1") {
        assert!(!doc.vv.is_empty(), "manifest 实际 VV 不能为空");
        assert!(!doc.frontier.is_empty(), "frontier 不能为空");
        assert!(doc.min_reader > 0, "min_reader 必须冻结");
        assert!(doc.full_history, "必须是 full-history");
    }
    assert!(
        e1.docs.iter().any(|doc| doc.doc_id == "structure"),
        "structure 必须包含在基线"
    );
}

#[test]
fn shallow_or_overclaimed_vv_snapshot_is_rejected() {
    ensure_test_identity_wrap();
    use menghunote_sync::checkpoint::KIND_BODY;
    use menghunote_sync_proto::SnapshotV1;
    let (_dir, vault, materials, _relay) = setup();
    let doc = seed_doc(&vault, "note-1", materials.peer_id_u64());
    doc.insert_scalars(0, "history").expect("正文");
    let mut snapshot = SnapshotV1 {
        doc_id: "note-1".to_owned(),
        kind: KIND_BODY,
        generation: 0,
        codec_version: "loro-v1".to_owned(),
        app_doc_schema: 1,
        min_reader: 1,
        full_history: true,
        vv: doc.vv().encode(),
        frontier: doc.frontiers().encode(),
        loro_snapshot_bytes: doc.export_snapshot().expect("snapshot"),
    };
    rotation_stage::verify_snapshot_integrity(&snapshot).expect("合法 full-history 快照");

    // T04：浅/state-only 快照必须拒绝。
    snapshot.full_history = false;
    assert!(
        rotation_stage::verify_snapshot_integrity(&snapshot).is_err(),
        "full_history=false 必须拒绝"
    );
    snapshot.full_history = true;

    // T18：声称的 VV 高于实际 Loro 历史（缺 op）必须拒绝。
    let mut vv = loro::VersionVector::decode(&snapshot.vv).expect("vv");
    vv.insert(999, 1);
    snapshot.vv = vv.encode();
    assert!(
        rotation_stage::verify_snapshot_integrity(&snapshot).is_err(),
        "VV 高于实际历史必须拒绝"
    );
}

#[test]
fn grants_are_persisted_in_journal_and_reloadable() {
    ensure_test_identity_wrap();
    use menghunote_sync::rotation_journal::{RotationJournal, RotationPhase};
    let (_dir, vault, _materials, _relay) = setup();
    let mut journal =
        RotationJournal::freeze(&vault, EPOCH, [0x75; 16], 1, [0x85; 32]).expect("freeze");
    let leaves = vec![vec![0xA1, 0x01, 0x41, 0xAA], vec![0xA2, 0x02, 0x42, 0xBB]];
    let digest = journal.set_grants(&vault, &leaves).expect("set grants");
    assert_eq!(journal.phase, RotationPhase::Frozen);
    let reloaded = RotationJournal::load(&vault)
        .expect("load")
        .expect("journal");
    assert_eq!(reloaded.grant_leaf_manifests, leaves);
    assert_eq!(reloaded.grant_leaf_digest, Some(digest));
    assert_eq!(reloaded.grants_digest, Some(digest));
}

#[test]
fn frozen_asset_pagination_is_used_and_candidate_ready_is_gated() {
    ensure_test_identity_wrap();
    use menghunote_sync::relay::{CheckpointStatus, FrozenAssetRef};
    let (_dir, vault, materials, mut relay) = setup();
    let assets =
        StaticAssets::new(vec![("text/plain".to_owned(), b"frozen-1".to_vec())]).expect("asset");
    let old_baseline = rotation_stage::prepare_new_epoch_baseline(
        &vault,
        &mut relay,
        VAULT_ID,
        materials.device_id,
        materials.signing_key.clone(),
        materials.root.clone(),
        materials.peer_id_u64(),
        &assets,
    )
    .expect("old baseline");
    relay.frozen_assets = old_baseline
        .report
        .assets
        .iter()
        .map(|asset| FrozenAssetRef {
            object_id: asset.object.object_id,
            generation: 0,
            key_epoch: EPOCH,
            part_count: asset.object.part_count,
            total_plain_len: asset.object.total_plain_len,
            object_digest: asset.object.object_digest,
        })
        .collect();
    let root = new_root();
    let (reencrypted, digest, total) = rotation_stage::reencrypt_frozen_assets(
        &vault,
        &mut relay,
        VAULT_ID,
        materials.device_id,
        &materials.signing_key,
        &materials.root,
        &root,
        [0x77; 16],
        1,
        None,
    )
    .expect("frozen reencrypt");
    assert_eq!(total, 1);
    assert_eq!(reencrypted.len(), 1);
    assert!(digest.is_none(), "Fake 未设置 asset_set_digest");

    relay.checkpoint_status.insert(
        [0x66; 16],
        CheckpointStatus {
            checkpoint_id: [0x66; 16],
            state: "candidate".to_owned(),
            role: Some("primary".to_owned()),
            covered_seq: 5,
            manifest_object_id: None,
            refs_digest: None,
            candidate_ready: true,
            epoch: EPOCH + 1,
        },
    );
    rotation_stage::assert_candidate_ready(&mut relay, [0x66; 16], "primary", 5)
        .expect("candidate ready");
    relay
        .checkpoint_status
        .get_mut(&[0x66; 16])
        .expect("status")
        .state = "published".to_owned();
    assert!(
        rotation_stage::assert_candidate_ready(&mut relay, [0x66; 16], "primary", 5).is_err(),
        "published 不能再作为 candidate ready"
    );
}

#[test]
fn grants_open_with_each_device_and_wrong_device_fails() {
    ensure_test_identity_wrap();
    let (_dir, _vault, materials, _relay) = setup();
    let new = new_root();
    let device_a = DeviceHpkeKey::generate(&mut TestRandom::new(1)).expect("hpke a");
    let device_b = DeviceHpkeKey::generate(&mut TestRandom::new(2)).expect("hpke b");
    let manager =
        ManagementKey::from_recovery(&materials.recovery_seed, &VAULT_ID).expect("manager");
    let leaves = rotation_stage::plan_grants(
        &new,
        &manager,
        menghunote_sync::genesis::GENESIS_MANAGER_REV,
        &[
            (materials.device_id, device_a.public_key()),
            ([0x66; 16], device_b.public_key()),
        ],
    )
    .expect("grants");
    assert_eq!(leaves.len(), 2);
    let aad = menghunote_sync_proto::GrantAadV1 {
        pv: 1,
        vault_id: VAULT_ID,
        epoch: new.key_epoch,
        recipient_device_id: materials.device_id,
        recipient_x25519_pub: device_a.public_key(),
        manager_rev: menghunote_sync::genesis::GENESIS_MANAGER_REV,
        grant_id: leaves[0].grant_id,
    };
    let opened = open_epoch_grant(
        device_a.secret(),
        &leaves[0].hpke_enc,
        &aad,
        &leaves[0].ciphertext,
    )
    .expect("device A opens");
    assert_eq!(
        opened.encode().expect("encode"),
        new.encode().expect("encode")
    );
    let wrong = menghunote_sync_proto::GrantAadV1 {
        recipient_device_id: [0x66; 16],
        ..aad
    };
    assert!(open_epoch_grant(
        device_a.secret(),
        &leaves[0].hpke_enc,
        &wrong,
        &leaves[0].ciphertext
    )
    .is_err());
}

/// 捕获 `stage_grants` 发出的 HTTP 请求，并按顺序回放预设响应。
#[derive(Default)]
struct CaptureTransport {
    responses: VecDeque<HttpResponse>,
    requests: Vec<HttpRequest>,
}

impl CaptureTransport {
    fn with_responses(responses: Vec<HttpResponse>) -> Self {
        Self {
            responses: responses.into(),
            requests: Vec::new(),
        }
    }
}

impl HttpTransport for CaptureTransport {
    fn send(&mut self, request: &HttpRequest) -> Result<HttpResponse, SyncError> {
        self.requests.push(request.clone());
        self.responses
            .pop_front()
            .ok_or(SyncError::Transport("测试没有更多预设响应"))
    }
}

fn cbor_response(fields: cbor::Fields) -> HttpResponse {
    HttpResponse {
        status: 200,
        body: cbor::encode(fields).expect("测试响应编码"),
    }
}

/// management realm challenge 响应（字段顺序见 `recovery_enroll::management_token`）。
fn management_challenge_response(manager: &ManagementKey, now: u64) -> HttpResponse {
    let subject: [u8; 16] = Sha256::digest(manager.public_key())[..16]
        .try_into()
        .expect("management subject");
    cbor_response(vec![
        (0, cbor::bytes([0x01; 16])),
        (1, cbor::uint(u64::from(AuthRealm::Management as u8))),
        (2, cbor::bytes(subject.to_vec())),
        (3, cbor::bytes([0x02; 32])),
        (4, cbor::uint(now + 600)),
        (5, cbor::bytes(VAULT_ID)),
        (6, cbor::bytes([0x03; 16])),
    ])
}

fn management_token_response(now: u64) -> HttpResponse {
    cbor_response(vec![
        (0, cbor::text("test-management-token")),
        (1, cbor::uint(now + 600)),
        (2, cbor::uint(now)),
        (3, cbor::uint(u64::from(AuthRealm::Management as u8))),
        (4, cbor::uint(1)),
    ])
}

/// relay `PUT .../grants/{device}` 的成功响应：`{0: view-map}`。
fn staged_grant_response() -> HttpResponse {
    cbor_response(vec![(0, cbor::CdeValue::Map(Vec::new()))])
}

#[test]
fn stage_grants_body_key0_is_grant_id_and_digest_covers_signed_header_only() {
    ensure_test_identity_wrap();
    let manager = ManagementKey::from_recovery(&RecoverySeed::from_array([0x11; 32]), &VAULT_ID)
        .expect("manager");
    let now = 1_700_000_000;
    let leaves = vec![
        GrantLeaf {
            device_id: [0x11; 16],
            epoch: EPOCH + 1,
            grant_id: [0xA1; 16],
            hpke_enc: [0x01; 32],
            ciphertext: vec![0x02; 48],
            signed_header: vec![0x03; 80],
            signature: [0x04; 64],
            manager_rev: 2,
            leaf_digest: [0x00; 32],
        },
        GrantLeaf {
            device_id: [0x22; 16],
            epoch: EPOCH + 1,
            grant_id: [0xB2; 16],
            hpke_enc: [0x05; 32],
            ciphertext: vec![0x06; 64],
            signed_header: vec![0x07; 96],
            signature: [0x08; 64],
            manager_rev: 2,
            leaf_digest: [0x00; 32],
        },
    ];

    let mut transport = CaptureTransport::with_responses(vec![
        management_challenge_response(&manager, now),
        management_token_response(now),
        staged_grant_response(),
        staged_grant_response(),
    ]);
    rotation_stage::stage_grants(
        &mut transport,
        &RelayApi::new("http://relay.test", RelayApiVersion::V1),
        VAULT_ID,
        [0x55; 16],
        &manager,
        &leaves,
        now,
    )
    .expect("stage grants");

    let puts: Vec<&HttpRequest> = transport
        .requests
        .iter()
        .filter(|request| request.method == "PUT")
        .collect();
    assert_eq!(puts.len(), leaves.len());
    for (leaf, request) in leaves.iter().zip(puts) {
        assert!(
            request.path.ends_with(&base64url::encode(&leaf.device_id)),
            "URL 仍按 device_id 定位"
        );
        let body = GrantStageBodyV1::decode(&request.body).expect("stage body CDE");
        assert_eq!(body.grant_id, leaf.grant_id, "键 0 必须是真实 grant_id");
        assert_ne!(
            body.grant_id, leaf.device_id,
            "不能用 device_id 顶替 grant_id"
        );
        assert_eq!(body.hpke_enc, leaf.hpke_enc);
        assert_eq!(body.hpke_ciphertext, leaf.ciphertext);
        assert_eq!(body.signed_header, leaf.signed_header);
        assert_eq!(body.signature, leaf.signature);
        assert_eq!(body.manager_rev, leaf.manager_rev);
    }

    // grants_digest 只覆盖 (device_id, SHA256(signed_header))，grant_id 不参与。
    let digest = grants_digest(&leaves).expect("grants digest");
    let mut changed = leaves.clone();
    changed[0].grant_id = [0xFF; 16];
    assert_eq!(
        grants_digest(&changed).expect("changed digest"),
        digest,
        "grant_id 不在 grants_digest 覆盖范围内"
    );

    let mut basis = leaves
        .iter()
        .map(|leaf| {
            (
                leaf.device_id,
                <[u8; 32]>::from(Sha256::digest(&leaf.signed_header)),
            )
        })
        .collect::<Vec<_>>();
    basis.sort_by_key(|(device_id, _)| *device_id);
    let mut input = Vec::new();
    for (device_id, header_hash) in basis {
        input.extend_from_slice(&device_id);
        input.extend_from_slice(&header_hash);
    }
    let expected: [u8; 32] = Sha256::digest(&input).into();
    assert_eq!(
        digest, expected,
        "grants_digest 口径：sorted(device_id || SHA256(signed_header))"
    );
}

#[test]
fn candidate_asset_dependency_must_equal_frozen_set() {
    ensure_test_identity_wrap();
    // T12 assets 依赖：候选基线必须覆盖冻结资产集合；只复制一部分（或引入额外资产）
    // 都不得被当作同一批可 ready 的材料。
    let (_dir, vault, materials, mut relay) = setup();
    relay.epoch = EPOCH + 1;
    let asset_a =
        StaticAssets::new(vec![("text/plain".to_owned(), b"asset-a".to_vec())]).expect("asset a");
    let both = StaticAssets::new(vec![
        ("text/plain".to_owned(), b"asset-a".to_vec()),
        ("image/png".to_owned(), b"asset-b".to_vec()),
    ])
    .expect("both");

    let first = rotation_stage::prepare_new_epoch_baseline(
        &vault,
        &mut relay,
        VAULT_ID,
        materials.device_id,
        materials.signing_key.clone(),
        new_root(),
        materials.peer_id_u64(),
        &asset_a,
    )
    .expect("baseline with one asset");
    let evidence = rotation_stage::capture_baseline_evidence(
        &mut relay,
        &new_root(),
        &first,
        &asset_a.hashes(),
    )
    .expect("自身资产集合一致");
    assert_eq!(evidence.asset_count, 1);
    assert!(
        rotation_stage::capture_baseline_evidence(&mut relay, &new_root(), &first, &both.hashes())
            .is_err(),
        "冻结资产集合有 B 而候选只复制 A，必须拒绝"
    );

    let second = rotation_stage::prepare_new_epoch_baseline(
        &vault,
        &mut relay,
        VAULT_ID,
        materials.device_id,
        materials.signing_key.clone(),
        new_root(),
        materials.peer_id_u64(),
        &both,
    )
    .expect("baseline with both assets");
    let second_evidence =
        rotation_stage::capture_baseline_evidence(&mut relay, &new_root(), &second, &both.hashes())
            .expect("两份冻结资产");
    assert_eq!(second_evidence.asset_count, 2);
    assert_ne!(
        evidence.asset_set_digest, second_evidence.asset_set_digest,
        "资产集合不同时 digest 必须不同"
    );
    assert!(
        rotation_stage::assert_independent_baselines(&evidence, &second_evidence, 0).is_err(),
        "资产集合不一致的两份候选不得一起 ready"
    );
}
