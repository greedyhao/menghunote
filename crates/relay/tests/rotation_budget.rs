//! 评审 0002 P1-5 / P1-E：rotation candidate activate 的 `future_live≤Q` 门禁，
//! 以及 activate/GC 后的 live 计量与预算归类。
//!
//! 覆盖：
//! 1. 只有 candidate pair（data refs）超 Q → activate 507、零写入、不删根；
//! 2. 只有 MANIFEST 密文让 pair 超 Q → 同样拒绝；
//! 3. 只有同 epoch **finalized orphan ASSET** 让 live 超 Q → 同样拒绝；
//! 4. 连续两轮近 Q rotation + 7 天宽限 GC 后仍能继续 reservation/rotation，且
//!    `usage` 账本恒等于真实 parts：live 对象 normal、旧 epoch 宽限 maintenance。
//!
//! 签名：本文件不使用 `TestHook`。begin / 对象 complete / candidate publish / grant /
//! slot / activate 全部走真实 Ed25519（`Ed25519Hook` + 管理/设备私钥），只依赖真实
//! SQLite 夹具。

mod support;

use ed25519_dalek::{Signer, SigningKey};

use relay::checkpoints;
use relay::envelope::ObjectType;
use relay::error::ErrorCode;
use relay::ids::{sha256, Hash32, Id16};
use relay::objects;
use relay::proto;
use relay::quota;
use relay::refs::RefRole;
use relay::rotations;
use relay::slots::{self, SlotKind};
use relay::vault;
use relay::verify::Ed25519Hook;
use support::*;

const DAY: i64 = 24 * 60 * 60;

/// 管理签名输入与 `relay/src/verify.rs` 完全同构（`MN/v1/manage\0 || instance ||
/// vault || request_id || control_rev(BE64) || SHA256(CDE{method,path,body})`）。
fn sign_management_with(
    fx: &Fixture,
    manager: &SigningKey,
    method: proto::HttpMethod,
    path: &str,
    request_id: Id16,
    expected_control_rev: u64,
    body: &[u8],
) -> Vec<u8> {
    let vault = fx.vault();
    let request_cde =
        proto::management_request_cde(method, path, body).expect("management request cde");
    let body_digest = sha256(&request_cde);
    let input = proto::management_signing_input(
        &vault.instance_id,
        &fx.vault_id,
        &request_id,
        expected_control_rev,
        &body_digest,
    )
    .expect("management signing input");
    manager.sign(&input).to_bytes().to_vec()
}

/// 用真实管理签名开始一轮轮换，返回 rotation id。
fn begin_rotation_signed(fx: &Fixture, manager: &SigningKey, request_id: Id16) -> Id16 {
    let vault = fx.vault();
    let body = b"rotation-begin-body".to_vec();
    let path = "/v1/vaults/1/rotations";
    let signature = sign_management_with(
        fx,
        manager,
        proto::HttpMethod::Post,
        path,
        request_id,
        vault.control_rev,
        &body,
    );
    let body_sha256 = sha256(&body);
    let ctx = fx.manager_ctx();
    let db = fx.db.clone();
    let clock = fx.clock.clone();
    db.write_blocking(move |conn| {
        rotations::begin(
            conn,
            clock.as_ref(),
            &Ed25519Hook,
            &ctx,
            &rotations::BeginRequest {
                revoke: &[],
                expected_epoch: vault.active_epoch,
                expected_control_rev: vault.control_rev,
                request_id,
                path,
                body_sha256,
                signed: &body,
                signature: &signature,
            },
        )
    })
    .expect("begin rotation")
    .rotation
    .rotation_id
}

/// 最小 activate：只带两个 cid，不带 new_manager/material digest。用于"Q 门禁必须在
/// 任何 manager/material 效果之前"的负例——若门禁没先触发，这里只会 409 而不是 507。
fn activate_bare_signed(
    fx: &Fixture,
    manager: &SigningKey,
    rotation: &Id16,
    primary: Id16,
    fallback: Id16,
    request_id: Id16,
) -> relay::Result<rotations::ActivateOutcome> {
    let body = b"activate-body".to_vec();
    let path = "/v1/vaults/1/rotations/activate";
    let control_rev = fx.vault().control_rev;
    let signature = sign_management_with(
        fx,
        manager,
        proto::HttpMethod::Post,
        path,
        request_id,
        control_rev,
        &body,
    );
    let body_sha256 = sha256(&body);
    let ctx = fx.manager_ctx();
    let db = fx.db.clone();
    let clock = fx.clock.clone();
    let rotation = *rotation;
    db.write_blocking(move |conn| {
        rotations::activate(
            conn,
            clock.as_ref(),
            &Ed25519Hook,
            &ctx,
            &rotation,
            &rotations::ActivateRequest {
                request_id,
                path,
                body_sha256,
                expected_control_rev: control_rev,
                signed: &body,
                signature: &signature,
                new_manager: None,
                primary_checkpoint: Some(primary),
                fallback_checkpoint: Some(fallback),
                member_cert_digest: None,
                grants_digest: None,
                slot_set_digest: None,
                asset_set_digest: None,
            },
        )
    })
}

/// 完整 activate：真实管理签名 + 真实旧/新管理 key 的 transition 双签 + 完整 T 双签。
#[allow(clippy::too_many_arguments)]
fn activate_full_signed(
    fx: &Fixture,
    old_manager: &SigningKey,
    rotation: &Id16,
    primary: Id16,
    fallback: Id16,
    new_manager: &SigningKey,
    request_id: Id16,
) -> relay::Result<rotations::ActivateOutcome> {
    let row = fx.rotation_view(rotation).rotation;
    let vault = fx.vault();
    let old_rev = vault.manager_rev;
    let new_pub = new_manager.verifying_key().to_bytes();
    let transition = fx.manager_transition(&row, old_rev, old_rev + 1, new_pub);
    let decoded = rotations::ManagerTransition::decode(&transition).expect("decode transition");
    let activation_t = {
        let db = fx.db.clone();
        let row = row.clone();
        db.read(move |conn| {
            let vault = vault::load(conn)?;
            activation_t_for(conn, &vault, &row, &decoded, &primary, &fallback)
        })
    }
    .expect("activation T");
    let t_input = proto::activation_t_signing_input(&activation_t).expect("T signing input");
    let t_bytes = activation_t.encode().expect("T encode");
    let old_t_signature = old_manager.sign(&t_input).to_bytes().to_vec();
    let new_t_signature = new_manager.sign(&t_input).to_bytes().to_vec();
    let old_transition_signature = old_manager.sign(&transition).to_bytes().to_vec();
    let new_transition_signature = new_manager.sign(&transition).to_bytes().to_vec();
    let (member_digest, grants_digest, slot_digest) = {
        let db = fx.db.clone();
        let to_epoch = row.to_epoch;
        db.read(move |conn| {
            Ok((
                rotations::member_cert_digest(conn)?,
                rotations::grants_digest(conn, to_epoch)?.1,
                rotations::slot_set_digest(conn, to_epoch)?,
            ))
        })
    }
    .expect("activation digests");

    let body = b"activate-body".to_vec();
    let path = "/v1/vaults/1/rotations/activate";
    let control_rev = vault.control_rev;
    let signature = sign_management_with(
        fx,
        old_manager,
        proto::HttpMethod::Post,
        path,
        request_id,
        control_rev,
        &body,
    );
    let body_sha256 = sha256(&body);
    let new_manager = rotations::NewManager {
        transition,
        old_signature: Some(old_transition_signature),
        new_signature: Some(new_transition_signature),
        activation_t: Some(t_bytes),
        old_t_signature: Some(old_t_signature),
        new_t_signature: Some(new_t_signature),
    };
    let ctx = fx.manager_ctx();
    let db = fx.db.clone();
    let clock = fx.clock.clone();
    let rotation = *rotation;
    let asset_set_digest = row.asset_set_digest;
    db.write_blocking(move |conn| {
        rotations::activate(
            conn,
            clock.as_ref(),
            &Ed25519Hook,
            &ctx,
            &rotation,
            &rotations::ActivateRequest {
                request_id,
                path,
                body_sha256,
                expected_control_rev: control_rev,
                signed: &body,
                signature: &signature,
                new_manager: Some(new_manager),
                primary_checkpoint: Some(primary),
                fallback_checkpoint: Some(fallback),
                member_cert_digest: Some(member_digest),
                grants_digest: Some(grants_digest),
                slot_set_digest: Some(slot_digest),
                asset_set_digest,
            },
        )
    })
}

fn object_ref_v1(meta: &relay::refs::ObjectRefMeta) -> proto::ObjectRefV1 {
    proto::ObjectRefV1 {
        object_id: meta.object_id,
        object_type: proto::ObjectType::try_from(meta.object_type.as_u8()).expect("object type"),
        generation: meta.generation,
        key_epoch: meta.key_epoch,
        part_count: meta.part_count,
        total_plain_len: meta.total_plain_len,
        object_digest: meta.object_digest,
    }
}

/// 真实 Ed25519 签名的对象 complete（对象字段由 relay 侧 DB 行提供）。
fn complete_object_signed(
    fx: &Fixture,
    object_id: &Hash32,
) -> relay::Result<objects::CompleteOutcome> {
    let object = fx.db.read(move |conn| objects::load(conn, object_id))?;
    let digest = {
        let object = object.clone();
        fx.db
            .read(move |conn| objects::recompute_digest(conn, &object))?
    };
    let commit = proto::ObjectCommit {
        vault_id: fx.vault_id,
        object_id: *object_id,
        message_id: object.message_id,
        object_type: proto::ObjectType::try_from(object.object_type.as_u8()).expect("object type"),
        generation: object.generation,
        key_epoch: object.epoch,
        sender_seq: object.sender_seq,
        part_count: object.part_count,
        total_len: object.plain_len,
        object_digest: digest,
    };
    let message = commit.encode().expect("object commit cde");
    let signing_input = proto::object_commit_signing_input(&commit).expect("commit signing input");
    let signature = fx
        .device_signing_key
        .sign(&signing_input)
        .to_bytes()
        .to_vec();
    let ctx = fx.device_ctx();
    let db = fx.db.clone();
    let clock = fx.clock.clone();
    let object_id = *object_id;
    let vault_id = fx.vault_id;
    db.write_blocking(move |conn| {
        objects::complete(
            conn,
            clock.as_ref(),
            &Ed25519Hook,
            &objects::CompleteUpload {
                actor: &ctx,
                vault_id: &vault_id,
                object_id: &object_id,
                commit: &message,
                signature: &signature,
            },
        )
    })
}

/// 单片、指定大小的对象（可带维护 reservation）。
fn create_object_signed(
    fx: &Fixture,
    object_type: ObjectType,
    generation: u64,
    sender_seq: u64,
    key_epoch: u64,
    plain_len: u64,
    reservation: Option<&Id16>,
) -> Hash32 {
    let object_id = random_object_id();
    let envelope = single_part_envelope_at(
        object_type,
        fx.vault_id,
        object_id,
        generation,
        sender_seq,
        plain_len,
        key_epoch,
    );
    fx.put_part(&envelope, reservation).expect("put part");
    complete_object_signed(fx, &object_id).expect("complete object");
    object_id
}

/// 真实 Ed25519 签名发布 checkpoint（active epoch → published；候选 epoch → candidate）。
fn publish_checkpoint_signed(
    fx: &Fixture,
    cid: Id16,
    covered: u64,
    manifest: Hash32,
) -> relay::Result<checkpoints::PublishOutcome> {
    let checkpoint = fx.checkpoint_row(&cid);
    let data_refs = fx.db.read(move |conn| {
        let mut refs = Vec::new();
        for (object_id, role) in checkpoints::refs(conn, &cid)? {
            if role == RefRole::Data {
                refs.push(object_ref_v1(&objects::object_ref_meta(conn, &object_id)?));
            }
        }
        Ok(refs)
    })?;
    let refs_input = proto::refs_digest_input(&data_refs).expect("refs digest input");
    let refs_digest = sha256(&refs_input);
    let manifest_digest = fx
        .db
        .read(move |conn| objects::object_ref_meta(conn, &manifest))
        .expect("manifest meta")
        .object_digest;
    let commit = proto::CheckpointCommit {
        vault_id: fx.vault_id,
        checkpoint_id: cid,
        epoch: checkpoint.epoch,
        covered_relay_seq: covered,
        manifest_object_id: manifest,
        manifest_digest,
        refs_digest,
        producer_device_id: fx.device,
        validation_version: 1,
    };
    let message = commit.encode().expect("checkpoint commit cde");
    let signing_input = proto::checkpoint_signing_input(&commit).expect("checkpoint signing input");
    let signature = fx
        .device_signing_key
        .sign(&signing_input)
        .to_bytes()
        .to_vec();
    let ctx = fx.device_ctx();
    let db = fx.db.clone();
    let clock = fx.clock.clone();
    db.write_blocking(move |conn| {
        checkpoints::publish(
            conn,
            clock.as_ref(),
            &Ed25519Hook,
            &ctx,
            &cid,
            &checkpoints::PublishRequest {
                covered_seq: covered,
                manifest_oid: manifest,
                refs_digest,
                signed_commit: &message,
                signature: &signature,
            },
        )
    })
}

/// 发布一份 active epoch 基线（真实签名）。
fn publish_active_baseline_signed(fx: &Fixture, snapshot_plain: u64, manifest_plain: u64) -> Id16 {
    let epoch = fx.vault().active_epoch;
    let ctx = fx.device_ctx();
    let db = fx.db.clone();
    let clock = fx.clock.clone();
    let outcome = db
        .write_blocking(move |conn| {
            checkpoints::create(conn, clock.as_ref(), &ctx, 4096, None, None, None)
        })
        .expect("create baseline");
    let rid = outcome.reservation_id;
    let snapshot = create_object_signed(
        fx,
        ObjectType::Snapshot,
        1,
        0,
        epoch,
        snapshot_plain,
        Some(&rid),
    );
    let manifest = create_object_signed(
        fx,
        ObjectType::Manifest,
        0,
        0,
        epoch,
        manifest_plain,
        Some(&rid),
    );
    fx.add_candidate_refs(outcome.checkpoint_id, snapshot, manifest);
    publish_checkpoint_signed(fx, outcome.checkpoint_id, outcome.covered_seq, manifest)
        .expect("publish active baseline");
    outcome.checkpoint_id
}

/// 候选 epoch 的一对独立基线（真实签名）。
fn stage_candidate_pair(
    fx: &Fixture,
    to_epoch: u64,
    snapshot_plain: u64,
    manifest_plain: u64,
) -> (Id16, Id16) {
    stage_candidate_pair_sized(fx, to_epoch, snapshot_plain, manifest_plain, 4096)
}

/// 指定 reservation 估算值的 checkpoint 创建。
///
/// `checkpoints::create` 默认按 4096B 估算；近 Q 的基线必须显式给足，否则计费阶段
/// 会先撞 507 RESERVE。候选 epoch 的两份基线/新槽**共享同一份** rotation
/// reservation，所以只有第一份候选 checkpoint 的估算值生效。
fn create_checkpoint_sized(
    fx: &Fixture,
    epoch: Option<u64>,
    estimated_bytes: i64,
) -> checkpoints::CreateOutcome {
    let ctx = fx.device_ctx();
    let db = fx.db.clone();
    let clock = fx.clock.clone();
    db.write_blocking(move |conn| {
        checkpoints::create(
            conn,
            clock.as_ref(),
            &ctx,
            estimated_bytes,
            epoch,
            None,
            None,
        )
    })
    .expect("create checkpoint sized")
}

/// 候选 epoch 的一对独立基线（snapshot/manifest 明文与 reservation 估算可指定）。
fn stage_candidate_pair_sized(
    fx: &Fixture,
    to_epoch: u64,
    snapshot_plain: u64,
    manifest_plain: u64,
    estimated_bytes: i64,
) -> (Id16, Id16) {
    let mut ids = Vec::new();
    for _ in 0..2 {
        let outcome = create_checkpoint_sized(fx, Some(to_epoch), estimated_bytes);
        let rid = outcome.reservation_id;
        let snapshot = create_object_signed(
            fx,
            ObjectType::Snapshot,
            1,
            0,
            to_epoch,
            snapshot_plain,
            Some(&rid),
        );
        let manifest = create_object_signed(
            fx,
            ObjectType::Manifest,
            0,
            0,
            to_epoch,
            manifest_plain,
            Some(&rid),
        );
        fx.add_candidate_refs(outcome.checkpoint_id, snapshot, manifest);
        publish_checkpoint_signed(fx, outcome.checkpoint_id, outcome.covered_seq, manifest)
            .expect("publish candidate");
        ids.push(outcome.checkpoint_id);
    }
    (ids[0], ids[1])
}

/// 发布一份 active epoch 基线；snapshot/manifest 明文与 reservation 估算可指定。
fn publish_active_baseline_sized(
    fx: &Fixture,
    snapshot_plain: u64,
    manifest_plain: u64,
    estimated_bytes: i64,
) -> Id16 {
    let epoch = fx.vault().active_epoch;
    let outcome = create_checkpoint_sized(fx, None, estimated_bytes);
    let rid = outcome.reservation_id;
    let snapshot = create_object_signed(
        fx,
        ObjectType::Snapshot,
        1,
        0,
        epoch,
        snapshot_plain,
        Some(&rid),
    );
    let manifest = create_object_signed(
        fx,
        ObjectType::Manifest,
        0,
        0,
        epoch,
        manifest_plain,
        Some(&rid),
    );
    fx.add_candidate_refs(outcome.checkpoint_id, snapshot, manifest);
    publish_checkpoint_signed(fx, outcome.checkpoint_id, outcome.covered_seq, manifest)
        .expect("publish active baseline sized");
    outcome.checkpoint_id
}

/// 真实管理签名的 HPKE grant（relay 会用 DB 事实重建 AAD 再验签）。
fn stage_grant_signed(fx: &Fixture, manager: &SigningKey, rotation: &Id16) {
    let row = fx.rotation_view(rotation).rotation;
    let manager_rev = fx.vault().manager_rev;
    let grant_id: Id16 = [0x44; 16];
    let hpke_enc = [0x45; 32];
    let hpke_ciphertext = vec![0x55; 64];
    let ciphertext_sha256 = sha256(&hpke_ciphertext);
    let aad = proto::GrantAadV1 {
        pv: 1,
        vault_id: fx.vault_id,
        epoch: row.to_epoch,
        recipient_device_id: fx.device,
        recipient_x25519_pub: [4u8; 32],
        manager_rev,
        grant_id,
    };
    let signed_header =
        proto::grant_signed_bytes(&aad, &hpke_enc, &ciphertext_sha256).expect("grant signed bytes");
    let signature = manager.sign(&signed_header).to_bytes().to_vec();
    let ctx = fx.manager_ctx();
    let db = fx.db.clone();
    let clock = fx.clock.clone();
    let rotation = *rotation;
    let device = fx.device;
    db.write_blocking(move |conn| {
        rotations::stage_grant(
            conn,
            &Ed25519Hook,
            &ctx,
            &rotation,
            &device,
            &rotations::GrantInput {
                grant_id,
                hpke_enc,
                hpke_ciphertext: &hpke_ciphertext,
                signed_header: &signed_header,
                signature: &signature,
                manager_rev,
            },
            clock.as_ref(),
        )
    })
    .expect("stage grant");
}

/// 候选 epoch 的 password/recovery staging 槽（对象 complete 与槽登记都走真实签名）。
fn stage_candidate_slots_signed(fx: &Fixture, rotation: &Id16, to_epoch: u64) -> (Hash32, Hash32) {
    let rid = fx.rotation_reservation(rotation);
    let mut ids = Vec::new();
    for kind in [SlotKind::Password, SlotKind::Recovery] {
        let slot_id = random_object_id();
        let descriptor = slot_descriptor(slot_id, fx.vault_id, to_epoch, kind, BASE_TIME as u64);
        let envelope = key_slot_envelope(fx.vault_id, slot_id, to_epoch, &descriptor, 64);
        fx.put_part(&envelope, Some(&rid)).expect("slot part");
        complete_object_signed(fx, &slot_id).expect("slot complete");
        let signed_message = format!("slot-proof-{to_epoch}-{}", kind.as_str()).into_bytes();
        let signature = fx
            .device_signing_key
            .sign(&signed_message)
            .to_bytes()
            .to_vec();
        let ctx = fx.device_ctx();
        let db = fx.db.clone();
        let clock = fx.clock.clone();
        db.write_blocking(move |conn| {
            slots::put(
                conn,
                clock.as_ref(),
                &Ed25519Hook,
                &ctx,
                &slots::SlotPutRequest {
                    slot_id,
                    epoch: to_epoch,
                    kind,
                    descriptor,
                    signed_message,
                    signature,
                },
            )
        })
        .expect("slot put");
        ids.push(slot_id);
    }
    (ids[0], ids[1])
}

/// 独立写的 live 口径（与生产 SQL 分开实现，用作交叉校验）：
/// 最新两份 published 的 data+manifest refs ∪ log 仍保留 UPDATE ∪ 同 epoch 未被 pair
/// data 引用的未 retired ASSET；`object_id` 去重。
fn live_bytes_sql(fx: &Fixture) -> i64 {
    let epoch = fx.vault().active_epoch;
    fx.db
        .read(move |conn| {
            Ok(conn.query_row(
                "WITH pair(checkpoint_id) AS (
                     SELECT checkpoint_id FROM checkpoints
                      WHERE state = 'published' AND epoch = ?1 AND health != 'unhealthy'
                      ORDER BY published_at DESC, rowid DESC LIMIT 2
                 ),
                 live(object_id) AS (
                     SELECT r.object_id FROM checkpoint_refs r
                      WHERE r.role IN ('data','manifest')
                        AND r.checkpoint_id IN (SELECT checkpoint_id FROM pair)
                     UNION
                     SELECT l.object_id FROM log l
                      WHERE l.kind = 'update' AND l.object_id IS NOT NULL
                     UNION
                     SELECT a.object_id FROM asset_objects a
                      WHERE a.epoch = ?1 AND a.state != 'retired'
                        AND a.object_id NOT IN (
                            SELECT r.object_id FROM checkpoint_refs r
                             WHERE r.role = 'data'
                               AND r.checkpoint_id IN (SELECT checkpoint_id FROM pair)
                        )
                 )
                 SELECT COALESCE(SUM(length(p.envelope)), 0)
                   FROM object_parts p
                  WHERE p.object_id IN (SELECT object_id FROM live)",
                [i64::try_from(epoch).expect("epoch")],
                |row| row.get(0),
            )?)
        })
        .expect("live bytes sql")
}

fn parts_total(fx: &Fixture) -> i64 {
    fx.db
        .read(|conn| {
            Ok(conn.query_row(
                "SELECT COALESCE(SUM(length(envelope)), 0) FROM object_parts",
                [],
                |row| row.get(0),
            )?)
        })
        .expect("parts total")
}

fn class_bytes(fx: &Fixture) -> (i64, i64) {
    fx.db
        .read(|conn| {
            Ok(conn.query_row(
                "SELECT
                    COALESCE(SUM(CASE WHEN o.budget_class='normal' THEN length(p.envelope) ELSE 0 END),0),
                    COALESCE(SUM(CASE WHEN o.budget_class='maintenance' THEN length(p.envelope) ELSE 0 END),0)
                 FROM object_parts p JOIN objects o ON o.object_id = p.object_id",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?)
        })
        .expect("class bytes")
}

/// 峰值不变量：账本真实总量必须 ≤ `Q+R`（`maintenance_limit`）。
///
/// 评审 0003 P1-E 残余：`normal ≤ Q` 与 `maintenance + reserved ≤ R` 都不足以保证
/// 峰值——`recompute` 把退下 pair 的 normal 归类为 maintenance 后，normal 桶会重新
/// 空出来，只验分类桶的写入就能把真实密文推过 `Q+R`。
fn assert_within_peak(fx: &Fixture) {
    let usage = fx.usage();
    let vault = fx.vault();
    assert!(
        usage.normal_bytes + usage.maintenance_bytes <= vault.maintenance_limit,
        "真实密文总量 {} + {} 超过峰值 Q+R = {}",
        usage.normal_bytes,
        usage.maintenance_bytes,
        vault.maintenance_limit
    );
    assert!(!fx.db.read(quota::peak_exceeded).expect("peak_exceeded"));
}

/// 账本对账：usage 两个桶必须分别等于真实 parts 的 normal/maintenance 和；
/// 且 normal 桶必须等于独立 live 口径（激活/GC 后 live 对象归 normal）。
fn assert_ledger(fx: &Fixture) {
    let usage = fx.usage();
    let (normal_parts, maintenance_parts) = class_bytes(fx);
    assert_eq!(
        usage.normal_bytes, normal_parts,
        "normal 账本必须等于 normal 对象的真实 parts"
    );
    assert_eq!(
        usage.maintenance_bytes, maintenance_parts,
        "maintenance 账本必须等于 maintenance 对象的真实 parts"
    );
    assert_eq!(
        usage.normal_bytes + usage.maintenance_bytes,
        parts_total(fx),
        "账本必须覆盖全部真实 parts"
    );
    assert_eq!(
        usage.normal_bytes,
        live_bytes_sql(fx),
        "normal 桶必须精确等于 live 集合（pair data+MANIFEST / log UPDATE / orphan ASSET）"
    );
    assert!(
        usage.normal_bytes <= fx.vault().normal_limit,
        "live {} 不得超过 Q {}",
        usage.normal_bytes,
        fx.vault().normal_limit
    );
}

fn old_epoch_parts(fx: &Fixture, active_epoch: u64) -> i64 {
    fx.db
        .read(move |conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM object_parts p
                  JOIN objects o ON o.object_id = p.object_id
                  WHERE o.epoch != ?1 AND o.type != 5",
                [i64::try_from(active_epoch).expect("epoch")],
                |row| row.get(0),
            )?)
        })
        .expect("old epoch parts")
}

/// 旧 epoch（当前已非 active）仍 published 的宽限基线，其全部 refs 对象必须是 maintenance。
fn assert_old_baselines_maintenance(fx: &Fixture, old_epoch: u64) {
    let rows: Vec<(String, i64)> = fx
        .db
        .read(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT o.budget_class, COUNT(*)
                   FROM checkpoint_refs r
                   JOIN objects o ON o.object_id = r.object_id
                  WHERE r.checkpoint_id IN (
                      SELECT checkpoint_id FROM checkpoints
                       WHERE state = 'published' AND epoch = ?1
                  )
                  GROUP BY o.budget_class",
            )?;
            let rows = stmt.query_map([i64::try_from(old_epoch).expect("epoch")], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .expect("old baseline classes");
    assert!(
        !rows.is_empty(),
        "旧 epoch 应仍有 published 宽限基线（epoch {old_epoch}）"
    );
    assert!(
        rows.iter().all(|(class, _)| class == "maintenance"),
        "旧 epoch 宽限基线的 snapshot/manifest 必须全部归 maintenance，实际 {rows:?}"
    );
}

/// 全量事实快照：507 前后必须逐项相等（零写入）。
#[derive(Debug, PartialEq, Eq)]
struct Facts {
    vault_state: String,
    active_epoch: i64,
    control_rev: i64,
    auth_rev: i64,
    manager_rev: i64,
    head_seq: i64,
    gc_floor: i64,
    rotation_state: String,
    rotation_primary: Option<Vec<u8>>,
    rotation_fallback: Option<Vec<u8>>,
    checkpoints: Vec<(Vec<u8>, String, Option<String>)>,
    parts: i64,
    part_bytes: i64,
    objects: i64,
    log_rows: i64,
    gc_plans: i64,
    manager_keys: i64,
    control_commits: i64,
    dedup: i64,
    reservations: Vec<(String, i64, i64)>,
    usage: (i64, i64),
}

fn facts(fx: &Fixture, rotation: &Id16) -> Facts {
    let rotation = *rotation;
    fx.db
        .read(move |conn| {
            let (vault_state, active_epoch, control_rev, auth_rev, manager_rev, head_seq, gc_floor):
                (String, i64, i64, i64, i64, i64, i64) = conn.query_row(
                "SELECT state, active_epoch, control_rev, auth_rev, manager_rev, head_seq, gc_floor
                   FROM vault WHERE singleton = 1",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                },
            )?;
            let (rotation_state, rotation_primary, rotation_fallback): (
                String,
                Option<Vec<u8>>,
                Option<Vec<u8>>,
            ) = conn.query_row(
                "SELECT state, primary_checkpoint, fallback_checkpoint
                       FROM rotations WHERE rotation_id = ?1",
                [rotation.as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;
            let checkpoints = {
                let mut stmt = conn.prepare(
                    "SELECT checkpoint_id, state, role FROM checkpoints ORDER BY rowid ASC",
                )?;
                let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            };
            let (parts, part_bytes): (i64, i64) = conn.query_row(
                "SELECT COUNT(*), COALESCE(SUM(length(envelope)), 0) FROM object_parts",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            let count =
                |sql: &str| -> rusqlite::Result<i64> { conn.query_row(sql, [], |row| row.get(0)) };
            let reservations = {
                let mut stmt = conn.prepare(
                    "SELECT state, reserved_bytes, consumed_bytes
                       FROM maintenance_reservations ORDER BY rowid ASC",
                )?;
                let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            };
            let usage = conn.query_row(
                "SELECT normal_bytes, maintenance_bytes FROM usage WHERE singleton = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            Ok(Facts {
                vault_state,
                active_epoch,
                control_rev,
                auth_rev,
                manager_rev,
                head_seq,
                gc_floor,
                rotation_state,
                rotation_primary,
                rotation_fallback,
                checkpoints,
                parts,
                part_bytes,
                objects: count("SELECT COUNT(*) FROM objects")?,
                log_rows: count("SELECT COUNT(*) FROM log")?,
                gc_plans: count("SELECT COUNT(*) FROM gc_plans")?,
                manager_keys: count("SELECT COUNT(*) FROM manager_keys")?,
                control_commits: count("SELECT COUNT(*) FROM control_commits")?,
                dedup: count("SELECT COUNT(*) FROM dedup")?,
                reservations,
                usage,
            })
        })
        .expect("facts")
}

fn expect_quota_507(result: relay::Result<rotations::ActivateOutcome>) -> relay::Error {
    let error = result.expect_err("future_live > Q 时 activate 必须被拒绝");
    assert_eq!(
        error.code(),
        Some(ErrorCode::QuotaExceeded),
        "期望 507 QUOTA_EXCEEDED，实际 {error:?}"
    );
    error
}

/// 只有 candidate pair 自身超 Q：activate 必须在任何 promotion/manager/control/log/GC
/// 效果之前返回 507，且不删除任何安全根。
#[test]
fn activate_rejects_candidate_pair_over_q_with_zero_effects() {
    let fx = Fixture::with_real_manager_limits("rotation-gate-pair", 1500, 12000);
    let manager = fx.manager_signing_key.clone().expect("manager key");
    let rotation = begin_rotation_signed(&fx, &manager, [0xA1; 16]);
    let row = fx.rotation_view(&rotation).rotation;
    // 每份 snapshot 明文 800 → 信封 1012B；两份 data 已 2024B > Q=1500。
    let (primary, fallback) = stage_candidate_pair(&fx, row.to_epoch, 800, 32);
    let live = fx
        .db
        .read(move |conn| quota::future_live_bytes(conn, row.to_epoch, &[primary, fallback]))
        .expect("future live");
    assert!(
        live > fx.vault().normal_limit,
        "前置：prospective pair live 必须超 Q（{live} vs Q）"
    );

    let before = facts(&fx, &rotation);
    expect_quota_507(activate_bare_signed(
        &fx, &manager, &rotation, primary, fallback, [0xB1; 16],
    ));
    assert_eq!(
        facts(&fx, &rotation),
        before,
        "507 必须零写入：checkpoints/rotation/vault/manager/control/log/gc_plans/usage 均不变"
    );
    for cid in [primary, fallback] {
        assert_eq!(
            fx.checkpoint_row(&cid).state,
            checkpoints::CheckpointState::Candidate,
            "被拒的候选不得 promote/abort"
        );
    }
    assert_eq!(
        fx.rotation_view(&rotation).rotation.state,
        rotations::RotationState::Frozen,
        "被拒的 activate 不得推进 rotation 状态"
    );
    assert_eq!(
        fx.db
            .read(|conn| Ok(conn.query_row(
                "SELECT COUNT(*) FROM checkpoint_refs r
                 JOIN checkpoints c ON c.checkpoint_id = r.checkpoint_id
                 WHERE c.state = 'published'",
                [],
                |row| row.get::<_, i64>(0)
            )?))
            .expect("published refs"),
        0,
        "任何安全根都不得因配额拒绝被删除或发布"
    );
}

/// 只有 MANIFEST 密文把 pair 推过 Q：旧口径（只算 data）会放行，新门禁必须拒绝。
#[test]
fn activate_rejects_when_manifest_pushes_pair_over_q() {
    let fx = Fixture::with_real_manager_limits("rotation-gate-manifest", 1500, 12000);
    let manager = fx.manager_signing_key.clone().expect("manager key");
    let rotation = begin_rotation_signed(&fx, &manager, [0xA2; 16]);
    let row = fx.rotation_view(&rotation).rotation;
    // data: 2*(250+212)=924B 仍 ≤ Q=1500；manifest: 2*(200+212)=824B → 合计 1748B > Q。
    let (primary, fallback) = stage_candidate_pair(&fx, row.to_epoch, 250, 200);

    let data_only: i64 = fx
        .db
        .read(move |conn| {
            Ok(conn.query_row(
                "SELECT COALESCE(SUM(length(p.envelope)), 0)
                   FROM object_parts p
                  WHERE p.object_id IN (
                      SELECT object_id FROM checkpoint_refs
                       WHERE checkpoint_id IN (?1, ?2) AND role = 'data'
                  )",
                [primary.as_slice(), fallback.as_slice()],
                |row| row.get(0),
            )?)
        })
        .expect("data only bytes");
    assert!(
        data_only <= fx.vault().normal_limit,
        "前置：只算 data 的旧口径必须 ≤ Q（{data_only}）"
    );
    let live = fx
        .db
        .read(move |conn| quota::future_live_bytes(conn, row.to_epoch, &[primary, fallback]))
        .expect("future live");
    assert_eq!(
        live,
        data_only + 2 * (200 + 212),
        "MANIFEST 密文必须计入 live"
    );
    assert!(live > fx.vault().normal_limit, "前置：pair live 超 Q");

    let before = facts(&fx, &rotation);
    expect_quota_507(activate_bare_signed(
        &fx, &manager, &rotation, primary, fallback, [0xB2; 16],
    ));
    assert_eq!(
        facts(&fx, &rotation),
        before,
        "MANIFEST 触发 507 时同样必须零写入"
    );
}

/// 只有同 epoch finalized orphan ASSET 把 live 推过 Q：必须拒绝且零写入。
#[test]
fn activate_rejects_when_finalized_orphan_asset_pushes_live_over_q() {
    let fx = Fixture::with_real_manager_limits("rotation-gate-asset", 2600, 12000);
    let manager = fx.manager_signing_key.clone().expect("manager key");
    let rotation = begin_rotation_signed(&fx, &manager, [0xA3; 16]);
    let row = fx.rotation_view(&rotation).rotation;
    // pair: data 2*(250+212)=924B + manifest 2*(100+212)=624B → 1548B ≤ Q=2600。
    let (primary, fallback) = stage_candidate_pair(&fx, row.to_epoch, 250, 100);
    let before_asset = fx
        .db
        .read(move |conn| quota::future_live_bytes(conn, row.to_epoch, &[primary, fallback]))
        .expect("future live");
    assert!(
        before_asset <= fx.vault().normal_limit,
        "前置：无 orphan ASSET 时 pair 必须装进 Q（{before_asset}）"
    );

    // 候选 epoch 的一份 finalized ASSET，未被任一候选基线引用（轮换仍需复制它）。
    let rid = fx.rotation_reservation(&rotation);
    let asset = create_object_signed(&fx, ObjectType::Asset, 0, 0, row.to_epoch, 900, Some(&rid));
    // 900+212=1112B → live = 1548+1112=2660B > Q。
    let live = fx
        .db
        .read(move |conn| quota::future_live_bytes(conn, row.to_epoch, &[primary, fallback]))
        .expect("future live");
    assert_eq!(
        live,
        before_asset + 900 + 212,
        "finalized orphan ASSET 必须计入 live"
    );
    assert!(live > fx.vault().normal_limit, "前置：orphan ASSET 超 Q");
    let registered: (String, i64) = fx
        .db
        .read(move |conn| {
            Ok(conn.query_row(
                "SELECT state, finalized_seq FROM asset_objects
                  WHERE epoch = ?1 AND object_id = ?2",
                (
                    i64::try_from(row.to_epoch).expect("epoch"),
                    asset.as_slice(),
                ),
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?)
        })
        .expect("asset registry");
    assert_eq!(
        registered.0, "active",
        "orphan ASSET 必须是 finalized active"
    );
    let referenced: i64 = fx
        .db
        .read(move |conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM checkpoint_refs WHERE object_id = ?1",
                [asset.as_slice()],
                |row| row.get(0),
            )?)
        })
        .expect("refs");
    assert_eq!(
        referenced, 0,
        "该 ASSET 不得被 pair 引用（否则不是 orphan）"
    );

    let before = facts(&fx, &rotation);
    expect_quota_507(activate_bare_signed(
        &fx, &manager, &rotation, primary, fallback, [0xB3; 16],
    ));
    assert_eq!(
        facts(&fx, &rotation),
        before,
        "orphan ASSET 触发 507 时同样必须零写入"
    );
}

/// 连续两轮近 Q rotation（每轮后 7 天宽限 GC）后仍可再次 reservation/rotation；
/// 每步 `usage` 与真实 parts 对账，live=normal、旧宽限=maintenance。
#[test]
fn near_q_rotations_with_grace_gc_stay_reconciled_and_can_continue() {
    // Q=2600；每轮 candidate pair = snapshot 650(862B)*2 + manifest 32(244B)*2 = 2212B，
    // 接近但不超过 Q；初始 update 312B 在首轮仍被 log 保留，live=2524B 仍 ≤ Q。
    let fx = Fixture::with_real_manager_limits("rotation-near-q", 2600, 12000);
    let mut manager = fx.manager_signing_key.clone().expect("manager key");

    // 初始 active epoch：一条 update（log live）+ 一份 published 基线。
    create_object_signed(&fx, ObjectType::Update, 1, 1, 1, 100, None);
    publish_active_baseline_signed(&fx, 64, 32);
    assert_ledger(&fx);

    let mut live_after_act = Vec::new();
    for index in 0..2u8 {
        let begin_id = [0xC0 + index; 16];
        let rotation = begin_rotation_signed(&fx, &manager, begin_id);
        let row = fx.rotation_view(&rotation).rotation;
        let (primary, fallback) = stage_candidate_pair(&fx, row.to_epoch, 650, 32);
        let prospective = fx
            .db
            .read(move |conn| quota::future_live_bytes(conn, row.to_epoch, &[primary, fallback]))
            .expect("future live");
        assert!(
            prospective <= fx.vault().normal_limit,
            "第 {} 轮 prospective live {} 必须装进 Q {}",
            index + 1,
            prospective,
            fx.vault().normal_limit
        );

        stage_grant_signed(&fx, &manager, &rotation);
        stage_candidate_slots_signed(&fx, &rotation, row.to_epoch);
        let next_manager = SigningKey::from_bytes(&[0x71 + index; 32]);
        activate_full_signed(
            &fx,
            &manager,
            &rotation,
            primary,
            fallback,
            &next_manager,
            [0xD0 + index; 16],
        )
        .expect("activate near-Q rotation");
        manager = next_manager;
        live_after_act.push(fx.usage().normal_bytes);

        assert_ledger(&fx);
        assert_old_baselines_maintenance(&fx, row.from_epoch);
        assert_eq!(
            fx.rotation_view(&rotation).rotation.state,
            rotations::RotationState::Activated
        );

        // 7 天宽限 GC：旧 epoch 密文/对象必须真正回收，维护额度回涨。
        fx.clock.advance_secs(8 * DAY);
        let outcome = fx.gc();
        assert!(
            !outcome.blocked,
            "第 {} 轮 GC 被阻断：{outcome:?}",
            index + 1
        );
        let active = fx.vault().active_epoch;
        assert_eq!(
            old_epoch_parts(&fx, active),
            0,
            "第 {} 轮宽限 GC 后旧 epoch 密文必须清空",
            index + 1
        );
        assert_ledger(&fx);
        let now = fx.clock_now();
        let available = fx
            .db
            .read(move |conn| quota::maintenance_available(conn, now))
            .expect("maintenance available");
        assert!(
            available > 0,
            "宽限 GC 后维护额度必须回涨（第 {} 轮，available={available}）",
            index + 1
        );
    }
    assert_eq!(
        live_after_act,
        vec![2524, 2212],
        "激活后 live：首轮含仍保留的 log UPDATE，GC 后只剩 pair"
    );

    // 两轮 + 宽限 GC 之后，第三次 reservation/rotation 仍必须成功（R 未被 live 永久占用）。
    let now = fx.clock_now();
    let available = fx
        .db
        .read(move |conn| quota::maintenance_available(conn, now))
        .expect("available");
    assert!(
        available >= 4096,
        "第三轮应有足够维护额度建立 4096B reservation（available={available}）"
    );
    let rotation = begin_rotation_signed(&fx, &manager, [0xC3; 16]);
    let row = fx.rotation_view(&rotation).rotation;
    let (primary, fallback) = stage_candidate_pair(&fx, row.to_epoch, 650, 32);
    stage_grant_signed(&fx, &manager, &rotation);
    stage_candidate_slots_signed(&fx, &rotation, row.to_epoch);
    let next_manager = SigningKey::from_bytes(&[0x73; 32]);
    activate_full_signed(
        &fx,
        &manager,
        &rotation,
        primary,
        fallback,
        &next_manager,
        [0xD2; 16],
    )
    .expect("第三轮 rotation 仍可激活");
    assert_ledger(&fx);
    assert_eq!(fx.vault().active_epoch, 4);
    assert_old_baselines_maintenance(&fx, 3);
}

// ---------------------------------------------------------------- 峰值 Q+R（评审 0003 P1-E 残余）

/// 一次写事务内的账本/数据快照，用于断言"507 零效应"。
fn write_snapshot(fx: &Fixture) -> ((i64, i64), i64, i64, i64) {
    let usage = fx.usage();
    fx.db
        .read(|conn| {
            let (parts, part_bytes): (i64, i64) = conn.query_row(
                "SELECT COUNT(*), COALESCE(SUM(length(envelope)), 0) FROM object_parts",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            let objects: i64 =
                conn.query_row("SELECT COUNT(*) FROM objects", [], |row| row.get(0))?;
            Ok((
                (usage.normal_bytes, usage.maintenance_bytes),
                parts,
                part_bytes,
                objects,
            ))
        })
        .expect("write snapshot")
}

/// 评审 0003 P1-E 残余（峰值绕过）：`recompute` 把退下 pair 的 normal 归类为 maintenance
/// 后，normal 桶重新装得下 4kB，但真实总量已经过 `Q+R`——`charge_normal` 必须用真实
/// 总量拒绝，且同事务零效应。
///
/// 构造（Q=10000，峰值 Q+R=20000，即默认 R=Q；全部走真实 SQLite + Ed25519）：
/// - epoch1 两份 published 基线：snapshot 4000B + MANIFEST 244B → 老 pair normal 8488B；
/// - 轮换到 epoch2：候选 pair 2×(4308+244)=9104B 经 R 上传（维护 reservation），
///   激活后升为 normal；epoch1 宽限 8488B + 新槽 552B 降为 maintenance；
/// - epoch2 再经 R 发布新 CP：snapshot 500B + MANIFEST 244B → 新 pair = 4552+744 = 5296B
///   normal；maintenance = 4552+8488+552 = 13592B；真实总量 18888B；
/// - 再 normal PUT 4000B：只验 Q 时 normal 9296 ≤ Q 会放行，真实总量 22888 > 20000。
#[test]
fn normal_put_rejected_when_reclassification_pushes_real_total_over_peak() {
    let fx = Fixture::with_real_manager_limits("quota-peak-exploit", 10_000, 20_000);
    let manager = fx.manager_signing_key.clone().expect("manager key");
    let epoch1 = fx.vault().active_epoch;

    // epoch1：老 pair normal = 2 × (4000 + 244) = 8488B。
    publish_active_baseline_sized(&fx, 3788, 32, 8_192);
    publish_active_baseline_sized(&fx, 3788, 32, 8_192);
    assert_eq!(
        (fx.usage().normal_bytes, fx.usage().maintenance_bytes),
        (8488, 0),
        "epoch1 老 pair 必须是 live/normal 8488B"
    );
    assert_within_peak(&fx);

    // 轮换到 epoch2：候选 pair 9104B + 两个新槽 552B = 9656B ≤ R=10000。
    let rotation = begin_rotation_signed(&fx, &manager, [0xE1; 16]);
    let row = fx.rotation_view(&rotation).rotation;
    let (primary, fallback) = stage_candidate_pair_sized(&fx, row.to_epoch, 4096, 32, 9_656);
    assert_within_peak(&fx);
    stage_grant_signed(&fx, &manager, &rotation);
    stage_candidate_slots_signed(&fx, &rotation, row.to_epoch);
    let next_manager = SigningKey::from_bytes(&[0xE2; 32]);
    activate_full_signed(
        &fx,
        &manager,
        &rotation,
        primary,
        fallback,
        &next_manager,
        [0xE3; 16],
    )
    .expect("activate rotation");
    assert_ledger(&fx);
    assert_within_peak(&fx);
    assert_eq!(fx.vault().active_epoch, epoch1 + 1);
    assert_eq!(
        (fx.usage().normal_bytes, fx.usage().maintenance_bytes),
        (9104, 9040),
        "激活后：新 pair 9104 normal；旧 epoch 宽限 8488 + 新槽 552 转 maintenance"
    );

    // epoch2 再发一份新 CP（经 checkpoint reservation，即 R 预算）：
    // available = R − 9040 = 960，故估算 896、实际 744。
    publish_active_baseline_sized(&fx, 288, 32, 896);
    let usage = fx.usage();
    assert_eq!(
        (usage.normal_bytes, usage.maintenance_bytes),
        (5296, 13592),
        "publish 后 recompute 必须把旧较新 pair 降为 maintenance（这正是绕过点）"
    );
    assert_eq!(usage.total(), 18_888);
    assert_ledger(&fx);
    assert_within_peak(&fx);

    // 4kB normal 写入：normal 9296 ≤ Q=10000（旧门禁会放行），
    // 但真实总量 18888+4000=22888 > 20000 → 必须 507 且零效应。
    let object_id = random_object_id();
    let epoch = fx.vault().active_epoch;
    let envelope = single_part_envelope_at(
        ObjectType::Update,
        fx.vault_id,
        object_id,
        1,
        1,
        3788,
        epoch,
    );
    let before = write_snapshot(&fx);
    let before_facts = facts(&fx, &rotation);
    let error = fx
        .put_part(&envelope, None)
        .expect_err("归类后 normal 仍装得下 4kB，但真实总量超峰值必须 507");
    assert_eq!(
        error.code(),
        Some(ErrorCode::QuotaExceeded),
        "期望 507 QUOTA_EXCEEDED，实际 {error:?}"
    );
    assert_eq!(
        write_snapshot(&fx),
        before,
        "507 必须零效应：账本/parts/objects 不变"
    );
    assert_eq!(
        facts(&fx, &rotation),
        before_facts,
        "507 必须零效应：日志/水位/检查点不变"
    );
    assert!(
        fx.db.read(|conn| objects::load(conn, &object_id)).is_err(),
        "被拒的写入不得留下 objects/staging 行"
    );

    // 峰值边界包含等号：恰好补齐到 Q+R 的 1112B 必须允许。
    let exact_id = random_object_id();
    let exact =
        single_part_envelope_at(ObjectType::Update, fx.vault_id, exact_id, 1, 2, 900, epoch);
    fx.put_part(&exact, None)
        .expect("恰好把真实总量补到峰值必须允许（≤ Q+R）");
    assert_eq!(fx.usage().total(), 20_000);
    assert_within_peak(&fx);

    // 再多 1 片（最小信封 213B）也必须 507，且只由总量门禁触发（normal 仍 ≤ Q）。
    let over_id = random_object_id();
    let over = single_part_envelope_at(ObjectType::Update, fx.vault_id, over_id, 1, 3, 1, epoch);
    let before = write_snapshot(&fx);
    let error = fx
        .put_part(&over, None)
        .expect_err("真实总量已到峰值，任何增量都必须 507");
    assert_eq!(error.code(), Some(ErrorCode::QuotaExceeded));
    assert!(
        fx.usage().normal_bytes + 213 <= fx.vault().normal_limit,
        "前置：该写入本身仍在 Q 预算内，拒绝只能来自真实总量门禁"
    );
    assert_eq!(write_snapshot(&fx), before, "峰值拒绝同样零效应");
}

/// 默认配置 `R = Q`（`Q = peak/2`）下连续两轮近 Q rotation + 宽限对象仍不超峰值；
/// 维护额度不足时拒绝且账本不变。
///
/// 每轮候选 pair = 2×(48968+244)=98424B（98.4% Q），+ 新槽 552B = 98976 ≤ R=100000；
/// 第一轮后 epoch1 宽限 520B 与新槽 552B 留在 maintenance，第二轮激活后旧 pair 再入
/// maintenance → 总量 197952B（98.98% 峰值 200000）仍 ≤ Q+R。
#[test]
fn default_r_equals_q_two_near_q_rotations_stay_within_peak() {
    let fx = Fixture::with_real_manager_limits("quota-peak-default-r", 100_000, 200_000);
    let mut manager = fx.manager_signing_key.clone().expect("manager key");
    // 初始基线：snapshot 64(276B) + MANIFEST 32(244B) = 520B。
    publish_active_baseline_signed(&fx, 64, 32);
    assert_eq!(fx.usage().normal_bytes, 520);
    assert_within_peak(&fx);

    const PAIR: i64 = 98_424;
    const ROTATION_RESERVATION: i64 = PAIR + 552;

    let mut request_seed = 0xD0u8;
    for round in 0..2u8 {
        let rotation = begin_rotation_signed(&fx, &manager, [0xC0 + round; 16]);
        let row = fx.rotation_view(&rotation).rotation;
        let (primary, fallback) =
            stage_candidate_pair_sized(&fx, row.to_epoch, 48_756, 32, ROTATION_RESERVATION);
        assert_ledger(&fx);
        assert_within_peak(&fx);
        stage_grant_signed(&fx, &manager, &rotation);
        stage_candidate_slots_signed(&fx, &rotation, row.to_epoch);
        assert_ledger(&fx);
        assert_within_peak(&fx);
        let next_manager = SigningKey::from_bytes(&[0x71 + round; 32]);
        let request_id = [request_seed; 16];
        request_seed = request_seed.wrapping_add(1);
        activate_full_signed(
            &fx,
            &manager,
            &rotation,
            primary,
            fallback,
            &next_manager,
            request_id,
        )
        .expect("activate near-Q rotation");
        manager = next_manager;
        assert_ledger(&fx);
        assert_within_peak(&fx);

        if round == 0 {
            // 第一轮激活后：新 pair 98424 normal；epoch1 宽限 520 + 新槽 552 → 1072。
            assert_eq!(
                (fx.usage().normal_bytes, fx.usage().maintenance_bytes),
                (98_424, 1_072)
            );
            // 宽限对象仍在时再要 98976B：R_available = 100000 − 1072 = 98928 < 98976
            // → 507 RESERVE（维护额度不足），账本与数据零变化。
            let ctx = fx.device_ctx();
            let db = fx.db.clone();
            let clock = fx.clock.clone();
            let before = facts(&fx, &rotation);
            let error = db
                .write_blocking(move |conn| {
                    checkpoints::create(
                        conn,
                        clock.as_ref(),
                        &ctx,
                        ROTATION_RESERVATION,
                        None,
                        None,
                        None,
                    )
                })
                .expect_err("宽限对象占住 R 时必须 507 RESERVE");
            assert_eq!(
                error.code(),
                Some(ErrorCode::ReserveExceeded),
                "期望 507 RESERVE，实际 {error:?}"
            );
            assert_eq!(
                facts(&fx, &rotation),
                before,
                "额度拒绝必须零写入（账本不变）"
            );
        } else {
            // 第二轮激活后：新 pair 98424 normal；旧 pair 98424 + 两代槽 1104 → 99528。
            assert_eq!(
                (fx.usage().normal_bytes, fx.usage().maintenance_bytes),
                (98_424, 99_528)
            );
            assert_eq!(fx.usage().total(), 197_952);
        }

        // 7 天宽限 + 8 天推进后 GC：旧 epoch 密文必须真正回收（R 也随之回涨）。
        fx.clock.advance_secs(8 * DAY);
        let outcome = fx.gc();
        assert!(
            !outcome.blocked,
            "第 {} 轮 GC 被阻断：{outcome:?}",
            round + 1
        );
        assert_eq!(
            old_epoch_parts(&fx, fx.vault().active_epoch),
            0,
            "第 {} 轮宽限 GC 后旧 epoch 密文必须清空",
            round + 1
        );
        assert_ledger(&fx);
        assert_within_peak(&fx);
    }

    // 两轮之后账本仍 ≤ 峰值；宽限 GC 已回收旧 pair，维护额度已回涨。
    let now = fx.clock_now();
    let available = fx
        .db
        .read(move |conn| quota::maintenance_available(conn, now))
        .expect("maintenance available");
    assert!(
        available > 0,
        "两轮 + 宽限 GC 后维护额度必须回涨（available={available}）"
    );
    assert_within_peak(&fx);
}

/// 历史数据已超峰（运维缩容/旧版本升级）：`recompute` 必须原样落真实账本，不得抹平；
/// 只允许读与降级回收，任何增量写入 507 且零效应。
#[test]
fn recompute_does_not_mask_historical_over_peak_and_blocks_writes() {
    // 先在大配额下写入真实密文（Q=20000，峰值 40000），随后缩容到 Q=10000/峰值 20000。
    let fx = Fixture::with_real_manager_limits("quota-peak-historical", 20_000, 40_000);
    publish_active_baseline_sized(&fx, 3788, 32, 8_192);
    publish_active_baseline_sized(&fx, 3788, 32, 8_192);
    let live = fx.vault().active_epoch;
    assert_eq!(fx.usage().normal_bytes, 8488);

    // 一份 staging checkpoint 的大对象（真实 Ed25519 签名的信封，经 maintenance
    // reservation 上传；不 complete，模拟历史遗留的暂存密文）。估算 16512，实际写 16000。
    let staged = create_checkpoint_sized(&fx, None, 16_512);
    let staged_object = random_object_id();
    let staged_envelope = single_part_envelope_at(
        ObjectType::Snapshot,
        fx.vault_id,
        staged_object,
        1,
        0,
        15_788,
        live,
    );
    fx.put_part(&staged_envelope, Some(&staged.reservation_id))
        .expect("historical staging part");
    assert_eq!(fx.usage().maintenance_bytes, 16_000);
    assert_eq!(fx.usage().total(), 24_488);

    // 运维缩容：真实数据 24488 > 新峰值 20000（历史超峰）。
    fx.raw_write(|conn| {
        conn.execute(
            "UPDATE vault SET normal_limit = 10000, maintenance_limit = 20000 WHERE singleton = 1",
            [],
        )?;
        Ok(())
    })
    .expect("shrink limits");
    fx.db
        .write_blocking(quota::recompute)
        .expect("recompute after shrink");
    assert_eq!(
        (fx.usage().normal_bytes, fx.usage().maintenance_bytes),
        (8488, 16_000),
        "recompute 必须原样落真实账本（不得把总量抹到峰值以内）"
    );
    assert_eq!(fx.usage().total(), 24_488);
    assert!(
        fx.db.read(quota::peak_exceeded).expect("peak_exceeded"),
        "历史超峰必须可诊断"
    );
    assert_ledger(&fx);

    // 读路径不受影响。
    assert!(!fx.device_token().is_empty());
    assert!(fx.db.read(quota::usage).is_ok());

    // 增量写入：normal（normal 桶仍装得下）与 maintenance（reservation 仍有余额）
    // 都必须被真实总量门禁拒绝，且零效应。
    let before = write_snapshot(&fx);
    let normal_id = random_object_id();
    let normal_envelope =
        single_part_envelope_at(ObjectType::Update, fx.vault_id, normal_id, 1, 1, 1, live);
    let error = fx
        .put_part(&normal_envelope, None)
        .expect_err("历史超峰时 normal 写入必须 507");
    assert_eq!(error.code(), Some(ErrorCode::QuotaExceeded));
    assert_eq!(write_snapshot(&fx), before, "超峰拒绝必须零效应");

    let now = fx.clock_now();
    let reservation = staged.reservation_id;
    let error = fx
        .db
        .write_blocking(move |conn| quota::charge_maintenance(conn, now, &reservation, 213))
        .expect_err("历史超峰时 maintenance 写入必须 507");
    assert_eq!(
        error.code(),
        Some(ErrorCode::QuotaExceeded),
        "reservation 仍有余额，拒绝只能来自真实总量门禁，实际 {error:?}"
    );
    assert_eq!(write_snapshot(&fx), before, "超峰拒绝必须零效应");

    // 降级回收：24h 后清理暂存对象 + 对账，真实总量回落到峰值以内。
    fx.clock.advance_secs(25 * 60 * 60);
    let now = fx.clock_now();
    let deleted = fx
        .db
        .write_blocking(move |conn| {
            let deleted = objects::cleanup_staging(conn, now - 24 * 60 * 60)?;
            quota::recompute(conn)?;
            Ok(deleted)
        })
        .expect("degrade: cleanup staging + recompute");
    assert_eq!(deleted, 1, "降级必须真正回收暂存密文");
    assert_eq!(
        (fx.usage().normal_bytes, fx.usage().maintenance_bytes),
        (8488, 0),
        "回收后账本必须等于剩余真实 parts"
    );
    assert!(
        !fx.db.read(quota::peak_exceeded).expect("peak_exceeded"),
        "回收后不再是超峰状态"
    );
    assert_within_peak(&fx);

    // 恢复到峰值以内后写入重新可用（真实 Ed25519 commit）。
    let recovered_id = random_object_id();
    let recovered_envelope =
        single_part_envelope_at(ObjectType::Update, fx.vault_id, recovered_id, 1, 2, 1, live);
    fx.put_part(&recovered_envelope, None)
        .expect("总量回落后同一片必须可写");
    assert_eq!(fx.usage().total(), 8488 + 213);
    complete_object_signed(&fx, &recovered_id).expect("recovered complete");
    assert_within_peak(&fx);
}
