//! task-101：轮换 wire 增补（评审 0001 §三 10 项 / ADR §28.1 条目 4–10）。
//!
//! 覆盖：rotations/current + request_id 幂等查结果、r view 全量字段、候选 checkpoint
//! 元数据/role/request-id 创建、staged grants 仅 management、typed 控制链与
//! `epoch.activated`、activate 材料 CAS + 精确 commit/cids、restart/revoke-more、
//! 冻结资产分页、degraded 读回。

mod support;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use ed25519_dalek::{Signer, SigningKey};
use http_body_util::BodyExt;
use relay::api::{router, AppState};
use relay::checkpoints;
use relay::control::{EpochActivated, SignedControlRecord};
use relay::envelope::ObjectType;
use relay::error::ErrorCode;
use relay::ids::{sha256, Id16};
use relay::proto::ActivationTV1;
use relay::verify::Ed25519Hook;
use relay::wire;
use support::*;
use tower::ServiceExt;

const DAY: i64 = 24 * 60 * 60;

fn block<T>(f: impl FnOnce() -> T) -> T {
    tokio::task::block_in_place(f)
}

fn app_ed25519(fx: &Fixture) -> axum::Router {
    let state = AppState::new(fx.db.clone(), fx.cfg.clone(), Arc::new(Ed25519Hook));
    router(state)
}

async fn call(app: &axum::Router, request: Request<Body>) -> (StatusCode, Vec<u8>) {
    let response = app.clone().oneshot(request).await.expect("response");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes()
        .to_vec();
    (status, bytes)
}

fn get(uri: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .expect("request")
}

/// 不带 token 的 GET（认证负例）。
fn get_no_token(uri: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .expect("request")
}

fn post_cbor(uri: &str, token: Option<&str>, fields: wire::Fields) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, wire::CONTENT_TYPE);
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    builder
        .body(Body::from(cbor_body(fields)))
        .expect("request")
}

fn fields_of(body: &[u8]) -> wire::Fields {
    cbor_fields(body)
}

fn error_code(body: &[u8]) -> String {
    wire::text_field(&fields_of(body), 0).unwrap_or_default()
}

fn b64(bytes: &[u8]) -> String {
    relay::ids::b64url_encode(bytes)
}

fn url(fx: &Fixture, suffix: &str) -> String {
    format!("/v1/vaults/{}{suffix}", b64(&fx.vault_id))
}

/// §35.4：设备 realm 的 grant leaf manifest 端点。
fn leaves_url(fx: &Fixture, rotation: &Id16) -> String {
    url(fx, &format!("/rotations/{}/grants/leaves", b64(rotation)))
}

/// 断言 leaf manifest 的字段可复算，并且数量/轮换/状态与预期一致。
///
/// `grants_digest` 口径：`SHA256(concat(sorted(device_id || SHA256(signed_header))))`。
fn assert_leaf_manifest(
    fields: &wire::Fields,
    rotation: &Id16,
    expected_state: &str,
    expected_count: usize,
) {
    let leaves = wire::array_field(fields, 0).expect("leaves");
    assert_eq!(leaves.len(), expected_count);
    let mut preimage = Vec::new();
    for leaf in &leaves {
        let leaf = wire::value_map(leaf).expect("leaf");
        let device_id = wire::id16_field(&leaf, 0).expect("leaf device");
        let signed_header = wire::bytes_field(&leaf, 2).expect("signed header");
        let digest = wire::hash32_field(&leaf, 1).expect("header digest");
        assert_eq!(
            digest,
            sha256(&signed_header),
            "leaf 的 signed_header_sha256 必须可复算"
        );
        preimage.push((device_id, digest));
    }
    preimage.sort_by_key(|(device_id, _)| *device_id);
    let mut concat = Vec::new();
    for (device_id, digest) in preimage {
        concat.extend_from_slice(&device_id);
        concat.extend_from_slice(&digest);
    }
    assert_eq!(
        wire::hash32_field(fields, 1).expect("grants digest"),
        sha256(&concat),
        "返回的 leaf 必须能复算 grants_digest"
    );
    assert_eq!(
        u64::try_from(leaves.len()).expect("leaves len"),
        wire::uint_field(fields, 2).expect("grants_count")
    );
    assert_eq!(
        wire::text_field(fields, 4).expect("rotation state"),
        expected_state
    );
    assert_eq!(wire::id16_field(fields, 5).expect("rotation id"), *rotation);
}

fn begin_body(
    expected_epoch: u64,
    control_rev: u64,
    request_id: &Id16,
    revoke: &[Id16],
) -> Vec<u8> {
    cbor_body(vec![
        (0, wire::uint(expected_epoch)),
        (1, wire::uint(control_rev)),
        (2, wire::bytes(*request_id)),
        (
            3,
            wire::array(revoke.iter().map(|id| wire::bytes(*id)).collect()),
        ),
    ])
}

/// 真实管理签名 + 旧 wire 外层镜像的 begin 请求。
fn begin_request(fx: &Fixture, token: &str, request_id: Id16) -> (Request<Body>, Vec<u8>) {
    let vault = fx.vault();
    let body = begin_body(vault.active_epoch, vault.control_rev, &request_id, &[]);
    let signature = fx.sign_management(
        relay::proto::HttpMethod::Post,
        &url(fx, "/rotations"),
        request_id,
        vault.control_rev,
        &body,
    );
    let outer = vec![
        (0, Fixture::signed_wrapper(body.clone(), signature)),
        (1, wire::bytes(request_id)),
        (2, wire::uint(vault.control_rev)),
        (3, wire::uint(vault.active_epoch)),
        (4, wire::array(Vec::new())),
    ];
    (post_cbor(&url(fx, "/rotations"), Some(token), outer), body)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn current_rotation_and_request_id_lookup() {
    let fx = block(|| Fixture::with_real_manager("wire-current"));
    let app = app_ed25519(&fx);
    let manager = block(|| fx.manager_token());

    // 未开始轮换：current 为空。
    let (status, resp) = call(&app, get(&url(&fx, "/rotations/current"), &manager)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!fields_of(&resp).iter().any(|(key, _)| *key == 0));

    let request_id = [0x11u8; 16];
    let (request, body) = begin_request(&fx, &manager, request_id);
    let (status, first) = call(&app, request).await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&first));
    let view = wire::value_map(&fields_of(&first)[0].1).expect("rotation view");
    let rotation = wire::id16_field(&view, 0).expect("rotation id");
    let _ = body;

    // device realm 也能读 current（评审 §三 1：management+device）。
    // 注意：begin 推进了 auth_rev，必须用新 token。
    let device = block(|| fx.device_token());
    let (status, resp) = call(&app, get(&url(&fx, "/rotations/current"), &device)).await;
    assert_eq!(status, StatusCode::OK);
    let current = wire::value_map(&fields_of(&resp)[0].1).expect("current view");
    assert_eq!(wire::id16_field(&current, 0).expect("current id"), rotation);

    // state.current_rotation_id（键 15）。
    let (status, state) = call(&app, get(&url(&fx, "/state"), &device)).await;
    assert_eq!(status, StatusCode::OK);
    let state = fields_of(&state);
    assert_eq!(
        wire::id16_field(&state, 15).expect("current rotation id"),
        rotation
    );

    // request_id 幂等查结果：回放原始 begin 响应字节（响应丢失后的安全找回路径）。
    let uri = format!(
        "{}?request_id={}",
        url(&fx, "/rotations/current"),
        relay::ids::hex_lower(request_id.as_slice())
    );
    let manager = block(|| fx.manager_token());
    let (status, replay) = call(&app, get(&uri, &manager)).await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&replay));
    let replay = fields_of(&replay);
    assert_eq!(
        wire::bytes_field(&replay, 1).expect("stored response"),
        first
    );
    assert_eq!(
        wire::text_field(&replay, 2).expect("scope"),
        "management/begin"
    );

    // 未知 request_id：200 且不带结果字段（客户端据此判断"没执行过"）。
    let unknown = format!(
        "{}?request_id={}",
        url(&fx, "/rotations/current"),
        relay::ids::hex_lower([0xFFu8; 16].as_slice())
    );
    let (status, resp) = call(&app, get(&unknown, &manager)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!fields_of(&resp).iter().any(|(key, _)| *key == 1));
}

/// 准备一轮候选材料（两份基线 + 全成员 grant + 两个新槽）。
fn prepare_rotation(fx: &Fixture, rotation: &Id16) -> (Id16, Id16) {
    let row = block(|| fx.rotation_view(rotation).rotation);
    let (primary, fallback) = block(|| fx.publish_candidate_baselines(row.to_epoch));
    block(|| fx.stage_grant(rotation, &fx.device));
    let reservation = block(|| fx.rotation_reservation(rotation));
    block(|| fx.stage_candidate_slots(row.to_epoch, &reservation));
    (primary, fallback)
}

/// 一次 activate 的材料（transition + T + 真实旧/新管理双签）。
///
/// ADR §34：T 的每个字段都按 relay 侧事实构造；负例在副本上篡改后重新双签，
/// 用来证明"签名有效但绑定不一致"同样被 409 拒绝。
struct ActivationMaterials {
    transition: Vec<u8>,
    old_transition_signature: Vec<u8>,
    new_transition_signature: Vec<u8>,
    activation_t: ActivationTV1,
    new_manager: SigningKey,
}

impl ActivationMaterials {
    fn manager_map(
        &self,
        activation_t_bytes: &[u8],
        old_t_signature: &[u8],
        new_t_signature: &[u8],
    ) -> wire::CdeValue {
        wire::map(vec![
            (0, wire::bytes(self.transition.clone())),
            (1, wire::bytes(self.old_transition_signature.clone())),
            (2, wire::bytes(self.new_transition_signature.clone())),
            (3, wire::bytes(activation_t_bytes.to_vec())),
            (4, wire::bytes(old_t_signature.to_vec())),
            (5, wire::bytes(new_t_signature.to_vec())),
        ])
    }
}

static ACTIVATE_REQUEST: AtomicU64 = AtomicU64::new(0x6000);

fn next_request_id() -> Id16 {
    let value = ACTIVATE_REQUEST.fetch_add(1, Ordering::SeqCst);
    let mut id = [0u8; 16];
    id[..8].copy_from_slice(&value.to_be_bytes());
    id
}

/// 按 T 记录重新签名（旧 key 来自 fixture，新 key 来自本次 activate 材料）。
fn resign_activation_t(
    fx: &Fixture,
    materials: &ActivationMaterials,
    activation_t: &ActivationTV1,
) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let bytes = activation_t.encode().expect("T encode");
    let input = relay::proto::activation_t_signing_input(activation_t).expect("T signing input");
    let old_manager = fx.manager_signing_key.as_ref().expect("manager key");
    let old_signature = old_manager.sign(&input).to_bytes().to_vec();
    let new_signature = materials.new_manager.sign(&input).to_bytes().to_vec();
    (bytes, old_signature, new_signature)
}

fn activation_materials(
    fx: &Fixture,
    rotation: &Id16,
    primary: Id16,
    fallback: Id16,
) -> ActivationMaterials {
    let row = block(|| fx.rotation_view(rotation).rotation);
    let old_manager = fx.manager_signing_key.as_ref().expect("manager key");
    let new_manager = SigningKey::from_bytes(&[0x77u8; 32]);
    let new_pub = new_manager.verifying_key().to_bytes();
    let old_rev = fx.vault().manager_rev;
    let transition = block(|| fx.manager_transition(&row, old_rev, old_rev + 1, new_pub));
    let decoded_transition =
        relay::rotations::ManagerTransition::decode(&transition).expect("decode transition");
    let activation_t = block(|| {
        fx.db.read(|conn| {
            let vault = relay::vault::load(conn)?;
            support::activation_t_for(conn, &vault, &row, &decoded_transition, &primary, &fallback)
        })
    })
    .expect("activation T");
    let old_transition_signature = old_manager.sign(&transition).to_bytes().to_vec();
    let new_transition_signature = new_manager.sign(&transition).to_bytes().to_vec();
    ActivationMaterials {
        transition,
        old_transition_signature,
        new_transition_signature,
        activation_t,
        new_manager,
    }
}

fn activate_body(
    fx: &Fixture,
    rotation: &Id16,
    manager_map: wire::CdeValue,
    primary: Id16,
    fallback: Id16,
) -> Vec<u8> {
    let row = block(|| fx.rotation_view(rotation).rotation);
    let (member_digest, grants_digest, slot_digest) = block(|| {
        fx.db.read(|conn| {
            Ok((
                relay::rotations::member_cert_digest(conn)?,
                relay::rotations::grants_digest(conn, row.to_epoch)?.1,
                relay::rotations::slot_set_digest(conn, row.to_epoch)?,
            ))
        })
    })
    .expect("digests");
    cbor_body(vec![
        (0, manager_map),
        (1, wire::bytes(primary)),
        (2, wire::bytes(fallback)),
        (3, wire::bytes(member_digest)),
        (4, wire::bytes(grants_digest)),
        (5, wire::bytes(slot_digest)),
        (
            6,
            wire::bytes(row.asset_set_digest.expect("begin 冻结资产摘要")),
        ),
    ])
}

/// 用真实管理签名提交一次 activate（body 与 outer 的 new_manager 逐字节一致）。
async fn post_activate(
    fx: &Fixture,
    app: &axum::Router,
    rotation: &Id16,
    body: Vec<u8>,
    manager_map: wire::CdeValue,
) -> (StatusCode, Vec<u8>) {
    let control_rev = fx.vault().control_rev;
    let request_id = next_request_id();
    let signature = fx.sign_management(
        relay::proto::HttpMethod::Post,
        &url(fx, &format!("/rotations/{}/activate", b64(rotation))),
        request_id,
        control_rev,
        &body,
    );
    let outer = vec![
        (0, Fixture::signed_wrapper(body, signature)),
        (1, wire::bytes(request_id)),
        (2, wire::uint(control_rev)),
        (3, manager_map),
    ];
    let token = block(|| fx.manager_token());
    call(
        app,
        post_cbor(
            &url(fx, &format!("/rotations/{}/activate", b64(rotation))),
            Some(&token),
            outer,
        ),
    )
    .await
}

/// 断言一次 activate 被 409 拒绝，且 `manager_rev` / `active_epoch` 完全不推进。
#[allow(clippy::too_many_arguments)]
async fn expect_activate_conflict(
    fx: &Fixture,
    app: &axum::Router,
    rotation: &Id16,
    primary: Id16,
    fallback: Id16,
    expected_manager_rev: u64,
    expected_epoch: u64,
    manager_map: wire::CdeValue,
) {
    let body = activate_body(fx, rotation, manager_map.clone(), primary, fallback);
    let (status, resp) = post_activate(fx, app, rotation, body, manager_map).await;
    assert_eq!(status, StatusCode::CONFLICT, "{}", error_code(&resp));
    assert_eq!(
        error_code(&resp),
        "CAS_CONFLICT",
        "T 绑定失败必须是 409 CAS_CONFLICT"
    );
    let vault = fx.vault();
    assert_eq!(
        vault.manager_rev, expected_manager_rev,
        "不得推进 manager_rev"
    );
    assert_eq!(vault.active_epoch, expected_epoch, "不得切换 active_epoch");
}

/// 篡改 T 的某个字段后**重新用旧/新管理 key 双签**：签名有效，但绑定必须失败。
#[allow(clippy::too_many_arguments)]
async fn expect_tampered_t_conflict(
    fx: &Fixture,
    app: &axum::Router,
    rotation: &Id16,
    primary: Id16,
    fallback: Id16,
    expected_manager_rev: u64,
    expected_epoch: u64,
    materials: &ActivationMaterials,
    mutate: impl FnOnce(&mut ActivationTV1),
) {
    let mut tampered = materials.activation_t.clone();
    mutate(&mut tampered);
    let (bytes, old_signature, new_signature) = resign_activation_t(fx, materials, &tampered);
    let manager_map = materials.manager_map(&bytes, &old_signature, &new_signature);
    expect_activate_conflict(
        fx,
        app,
        rotation,
        primary,
        fallback,
        expected_manager_rev,
        expected_epoch,
        manager_map,
    )
    .await;
}

fn without_manager_keys(manager_map: wire::CdeValue, keys: &[u64]) -> wire::CdeValue {
    let wire::CdeValue::Map(entries) = manager_map else {
        panic!("new_manager 必须是 map");
    };
    wire::CdeValue::Map(
        entries
            .into_iter()
            .filter(|(key, _)| !keys.contains(key))
            .collect(),
    )
}
async fn activate_via_http(
    fx: &Fixture,
    app: &axum::Router,
    rotation: &Id16,
) -> (Vec<u8>, ActivationMaterials) {
    let row = block(|| fx.rotation_view(rotation).rotation);
    let candidates = block(|| {
        fx.db
            .read(|conn| checkpoints::candidates_for_epoch(conn, row.to_epoch))
    })
    .expect("candidates");
    let (primary, fallback) = (candidates[0].checkpoint_id, candidates[1].checkpoint_id);
    let materials = activation_materials(fx, rotation, primary, fallback);
    let (t_bytes, old_t_signature, new_t_signature) =
        resign_activation_t(fx, &materials, &materials.activation_t);
    let manager_map = materials.manager_map(&t_bytes, &old_t_signature, &new_t_signature);
    let body = activate_body(fx, rotation, manager_map.clone(), primary, fallback);
    let (status, resp) = post_activate(fx, app, rotation, body, manager_map).await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&resp));
    (resp, materials)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rotation_view_exposes_full_facts_and_typed_control_records() {
    let fx = block(|| Fixture::with_real_manager("wire-view"));
    let app = app_ed25519(&fx);
    let manager = block(|| fx.manager_token());
    let request_id = [0x11u8; 16];
    let (request, _) = begin_request(&fx, &manager, request_id);
    let (status, first) = call(&app, request).await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&first));
    let rotation = wire::id16_field(&wire::value_map(&fields_of(&first)[0].1).expect("view"), 0)
        .expect("rotation id");

    let (primary, fallback) = prepare_rotation(&fx, &rotation);
    let row = block(|| fx.rotation_view(&rotation).rotation);

    // r view 全量字段（device realm 可读）。
    let device = block(|| fx.device_token());
    // §35.4：staging 阶段 active 设备可读完整 leaf manifest，且可复算 grants_digest。
    let (status, leaves_resp) = call(&app, get(&leaves_url(&fx, &rotation), &device)).await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&leaves_resp));
    assert_leaf_manifest(&fields_of(&leaves_resp), &rotation, "staging", 1);
    let staging_grants_digest =
        wire::hash32_field(&fields_of(&leaves_resp), 1).expect("staging grants digest");
    let manager = block(|| fx.manager_token());
    let (status, resp) = call(
        &app,
        get(
            &url(&fx, &format!("/rotations/{}", b64(&rotation))),
            &device,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&resp));
    let view = wire::value_map(&fields_of(&resp)[0].1).expect("view");
    let vault = fx.vault();
    assert_eq!(
        wire::uint_field(&view, 10).expect("control_rev"),
        vault.control_rev
    );
    assert_eq!(
        wire::uint_field(&view, 11).expect("auth_rev"),
        vault.auth_rev
    );
    assert_eq!(
        wire::uint_field(&view, 12).expect("manager_rev"),
        vault.manager_rev
    );
    assert_eq!(wire::uint_field(&view, 13).expect("revoked_once"), 0);
    assert_eq!(wire::uint_field(&view, 14).expect("cutoff"), row.freeze_seq);
    assert_eq!(wire::uint_field(&view, 15).expect("asset count"), 0);
    assert!(wire::opt_field(&view, 16).is_some(), "冻结资产 digest");
    assert_eq!(
        wire::hash32_field(&view, 17).expect("member cert digest"),
        block(|| fx.db.read(relay::rotations::member_cert_digest)).expect("digest")
    );
    // candidates：两个 cid + health + readback-ready。
    let candidates = wire::array_field(&view, 23).expect("candidates");
    assert_eq!(candidates.len(), 2);
    for candidate in candidates {
        let candidate = wire::value_map(&candidate).expect("candidate");
        let cid = wire::id16_field(&candidate, 0).expect("cid");
        assert!(cid == primary || cid == fallback);
        assert_eq!(wire::text_field(&candidate, 8).expect("health"), "healthy");
        assert!(wire::bool_field(&candidate, 9).expect("ready"));
    }
    // slots：两个候选槽 + descriptor digest。
    let slots = wire::array_field(&view, 24).expect("slots");
    assert_eq!(slots.len(), 2);
    for slot in slots {
        let slot = wire::value_map(&slot).expect("slot");
        assert_eq!(wire::uint_field(&slot, 4).expect("epoch"), row.to_epoch);
        assert_eq!(
            wire::bytes_field(&slot, 3)
                .expect("descriptor digest")
                .len(),
            32
        );
    }
    // reservation：owner/expiry/bytes。
    let reservation_view = wire::map_field(&view, 25).expect("reservation");
    assert_eq!(
        wire::id16_field(&reservation_view, 1).expect("owner"),
        fx.device
    );
    assert!(wire::uint_field(&reservation_view, 2).expect("expiry") > 0);
    assert_eq!(wire::uint_field(&view, 26).expect("candidate_ready"), 1);

    // management 也能读 r view（恢复路径）。
    let (status, _) = call(
        &app,
        get(
            &url(&fx, &format!("/rotations/{}", b64(&rotation))),
            &manager,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // typed 控制链：begin 记录是 SignedControlRecord，管理签名可用当前 manager key 验证。
    let (status, control) = call(&app, get(&url(&fx, "/control?after_rev=0"), &manager)).await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&control));
    let entries = wire::array_field(&fields_of(&control), 0).expect("entries");
    let begin_entry = entries
        .iter()
        .map(|entry| wire::value_map(entry).expect("entry"))
        .find(|entry| {
            wire::opt_field(entry, 4)
                .and_then(|value| match value {
                    wire::CdeValue::Text(text) => Some(text == "rotation.begin"),
                    _ => None,
                })
                .unwrap_or(false)
        })
        .expect("rotation.begin 控制记录");
    let record_bytes = wire::bytes_field(&begin_entry, 3).expect("record");
    let record = SignedControlRecord::decode(&record_bytes).expect("decode record");
    assert_eq!(record.request_id, request_id);
    // 用被签 body 重算管理签名输入并验签（真实 Ed25519）。
    let instance = fx.vault().instance_id;
    let body_digest = sha256(
        &relay::proto::management_request_cde(
            relay::proto::HttpMethod::Post,
            &url(&fx, "/rotations"),
            &record.signed_body,
        )
        .expect("request cde"),
    );
    let input = relay::proto::management_signing_input(
        &instance,
        &fx.vault_id,
        &request_id,
        record.expected_control_rev,
        &body_digest,
    )
    .expect("signing input");
    let manager_pub = fx.vault().manager_rev;
    let _ = manager_pub;
    let key = fx.manager_signing_key.as_ref().expect("manager key");
    key.verifying_key()
        .verify_strict(&input, &to_signature(&record.signature))
        .expect("控制记录签名必须可验证");

    // 完成激活后：epoch.activated 事件显式带 rid/E/S/commit hash/control_rev + activation T 双签。
    let (activate_resp, materials) = activate_via_http(&fx, &app, &rotation).await;
    let activate_fields = fields_of(&activate_resp);
    let activation_commit = wire::hash32_field(&activate_fields, 6).expect("commit");
    let activated_row = block(|| fx.rotation_view(&rotation).rotation);
    let activated_primary = wire::id16_field(&activate_fields, 4).expect("primary");
    let activated_fallback = wire::id16_field(&activate_fields, 5).expect("fallback");
    assert_eq!(activated_row.primary_checkpoint, Some(activated_primary));
    assert_eq!(activated_row.fallback_checkpoint, Some(activated_fallback));
    assert_eq!(
        [activated_primary, activated_fallback]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>(),
        [primary, fallback]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>(),
        "响应里的两个 cid 必须是被指名的两份（顺序由 role 决定）"
    );
    // /control 只需任意已认证 realm；激活后旧 manager token 已失效，用 device token。
    let control_token = block(|| fx.device_token());
    let (status, control) =
        call(&app, get(&url(&fx, "/control?after_rev=0"), &control_token)).await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&control));
    let entries = wire::array_field(&fields_of(&control), 0).expect("entries");
    let activated = entries
        .iter()
        .map(|entry| wire::value_map(entry).expect("entry"))
        .find(|entry| {
            wire::opt_field(entry, 4)
                .and_then(|value| match value {
                    wire::CdeValue::Text(text) => Some(text == "epoch.activated"),
                    _ => None,
                })
                .unwrap_or(false)
        })
        .expect("epoch.activated 控制记录");
    let record = SignedControlRecord::decode(&wire::bytes_field(&activated, 3).expect("record"))
        .expect("decode");
    let event = EpochActivated::decode(record.event.as_deref().expect("event")).expect("event");
    assert_eq!(event.rotation_id, rotation);
    assert_eq!(event.from_epoch, row.from_epoch);
    assert_eq!(event.to_epoch, row.to_epoch);
    assert_eq!(event.covered_seq, row.freeze_seq);
    assert_eq!(event.activation_commit, activation_commit);
    assert_eq!(
        event.control_rev,
        wire::uint_field(&activate_fields, 1).expect("control rev")
    );
    // §34：/control 返回的 T 必须能被 ActivationTV1::decode，并由旧/新管理 key 复算验签。
    let t_bytes = event.activation_t.as_deref().expect("事件必须携带 T");
    let activation_t = ActivationTV1::decode(t_bytes).expect("T decode");
    assert_eq!(activation_t, materials.activation_t);
    let t_input = relay::proto::activation_t_signing_input(&activation_t).expect("T signing input");
    let expected_old_t_signature = fx
        .manager_signing_key
        .as_ref()
        .expect("manager key")
        .sign(&t_input)
        .to_bytes()
        .to_vec();
    let expected_new_t_signature = materials.new_manager.sign(&t_input).to_bytes().to_vec();
    assert_eq!(
        event.old_t_signature.as_deref(),
        Some(expected_old_t_signature.as_slice()),
        "事件旧 key 双签必须原样持久化"
    );
    assert_eq!(
        event.new_t_signature.as_deref(),
        Some(expected_new_t_signature.as_slice()),
        "事件新 key 双签必须原样持久化"
    );
    fx.manager_signing_key
        .as_ref()
        .expect("manager key")
        .verifying_key()
        .verify_strict(
            &t_input,
            &to_signature(
                event
                    .old_t_signature
                    .as_deref()
                    .expect("旧 key 对 T 的签名"),
            ),
        )
        .expect("旧管理 key 的 T 签名必须可复算验证");
    materials
        .new_manager
        .verifying_key()
        .verify_strict(
            &t_input,
            &to_signature(
                event
                    .new_t_signature
                    .as_deref()
                    .expect("新 key 对 T 的签名"),
            ),
        )
        .expect("新管理 key 的 T 签名必须可复算验证");
    assert_eq!(activation_t.primary_cid, activated_primary);
    assert_eq!(activation_t.fallback_cid, activated_fallback);
    assert_eq!(activation_t.expected_control_rev, event.control_rev);
    let rotation_row = block(|| fx.rotation_view(&rotation).rotation);
    assert_eq!(rotation_row.activation_commit, Some(activation_commit));
    assert_eq!(rotation_row.activated_control_rev, Some(event.control_rev));

    // §35.4：激活后 endpoint 仍只返回该 rotation 的完整 leaf manifest（state=activated），
    // 且 grants_digest 与 staging 阶段逐字节一致（B 侧 inclusion 证据不随激活漂移）。
    let leaves_token = block(|| fx.device_token());
    let (status, leaves_after) = call(&app, get(&leaves_url(&fx, &rotation), &leaves_token)).await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&leaves_after));
    assert_leaf_manifest(&fields_of(&leaves_after), &rotation, "activated", 1);
    assert_eq!(
        wire::hash32_field(&fields_of(&leaves_after), 1).expect("activated grants digest"),
        staging_grants_digest,
        "激活不得改变已锁定 grants_digest"
    );
}

fn to_signature(bytes: &[u8]) -> ed25519_dalek::Signature {
    let array: [u8; 64] = bytes.try_into().expect("signature len");
    ed25519_dalek::Signature::from_bytes(&array)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn activation_t_rejects_tampering_missing_fields_and_wrong_keys_without_state_change() {
    let fx = block(|| Fixture::with_real_manager("wire-activation-t"));
    let app = app_ed25519(&fx);
    let manager = block(|| fx.manager_token());
    let (request, _) = begin_request(&fx, &manager, [0x31u8; 16]);
    let (status, first) = call(&app, request).await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&first));
    let rotation = wire::id16_field(&wire::value_map(&fields_of(&first)[0].1).expect("view"), 0)
        .expect("rotation id");
    let (primary, fallback) = prepare_rotation(&fx, &rotation);
    let old_rev = fx.vault().manager_rev;
    let epoch = fx.vault().active_epoch;
    let materials = activation_materials(&fx, &rotation, primary, fallback);
    let (valid_t, valid_old_signature, valid_new_signature) =
        resign_activation_t(&fx, &materials, &materials.activation_t);
    let valid_map = materials.manager_map(&valid_t, &valid_old_signature, &valid_new_signature);

    // 缺 T / 缺任一 T 签名 / T 不是 ActivationTV1 CDE / T 签名非 64B → 409。
    let missing_sets: [&[u64]; 3] = [&[3], &[4], &[5]];
    for missing in missing_sets {
        expect_activate_conflict(
            &fx,
            &app,
            &rotation,
            primary,
            fallback,
            old_rev,
            epoch,
            without_manager_keys(valid_map.clone(), missing),
        )
        .await;
    }
    let bad_t = materials.manager_map(
        b"not-an-activation-t",
        &valid_old_signature,
        &valid_new_signature,
    );
    expect_activate_conflict(
        &fx, &app, &rotation, primary, fallback, old_rev, epoch, bad_t,
    )
    .await;
    let short_signature =
        materials.manager_map(&valid_t, &valid_old_signature[..63], &valid_new_signature);
    expect_activate_conflict(
        &fx,
        &app,
        &rotation,
        primary,
        fallback,
        old_rev,
        epoch,
        short_signature,
    )
    .await;

    // 错 key：T 的两份签名各自由正确的旧/新管理 key 验证，第三方签名 → 409。
    let t_input =
        relay::proto::activation_t_signing_input(&materials.activation_t).expect("T signing input");
    let imposter = SigningKey::from_bytes(&[0x66u8; 32]);
    let wrong_old_signature = imposter.sign(&t_input).to_bytes().to_vec();
    let wrong_new_signature = imposter.sign(&t_input).to_bytes().to_vec();
    for manager_map in [
        materials.manager_map(&valid_t, &wrong_old_signature, &valid_new_signature),
        materials.manager_map(&valid_t, &valid_old_signature, &wrong_new_signature),
    ] {
        expect_activate_conflict(
            &fx,
            &app,
            &rotation,
            primary,
            fallback,
            old_rev,
            epoch,
            manager_map,
        )
        .await;
    }

    // 逐字段篡改 T 后重新双签（签名有效、绑定不一致）→ 409。
    expect_tampered_t_conflict(
        &fx,
        &app,
        &rotation,
        primary,
        fallback,
        old_rev,
        epoch,
        &materials,
        |t| {
            t.vault_id[0] ^= 0xFF;
        },
    )
    .await;
    expect_tampered_t_conflict(
        &fx,
        &app,
        &rotation,
        primary,
        fallback,
        old_rev,
        epoch,
        &materials,
        |t| {
            t.instance_id[0] ^= 0xFF;
        },
    )
    .await;
    expect_tampered_t_conflict(
        &fx,
        &app,
        &rotation,
        primary,
        fallback,
        old_rev,
        epoch,
        &materials,
        |t| {
            t.rotation_id[0] ^= 0xFF;
        },
    )
    .await;
    expect_tampered_t_conflict(
        &fx,
        &app,
        &rotation,
        primary,
        fallback,
        old_rev,
        epoch,
        &materials,
        |t| {
            t.from_epoch += 1;
            t.to_epoch += 2;
        },
    )
    .await;
    expect_tampered_t_conflict(
        &fx,
        &app,
        &rotation,
        primary,
        fallback,
        old_rev,
        epoch,
        &materials,
        |t| {
            t.to_epoch += 1;
        },
    )
    .await;
    expect_tampered_t_conflict(
        &fx,
        &app,
        &rotation,
        primary,
        fallback,
        old_rev,
        epoch,
        &materials,
        |t| {
            t.freeze_seq += 1;
        },
    )
    .await;
    expect_tampered_t_conflict(
        &fx,
        &app,
        &rotation,
        primary,
        fallback,
        old_rev,
        epoch,
        &materials,
        |t| {
            t.old_manager_rev += 1;
            t.new_manager_rev += 1;
        },
    )
    .await;
    expect_tampered_t_conflict(
        &fx,
        &app,
        &rotation,
        primary,
        fallback,
        old_rev,
        epoch,
        &materials,
        |t| {
            t.new_manager_pubkey[0] ^= 0xFF;
        },
    )
    .await;
    expect_tampered_t_conflict(
        &fx,
        &app,
        &rotation,
        primary,
        fallback,
        old_rev,
        epoch,
        &materials,
        |t| {
            t.prev_control_hash[0] ^= 0xFF;
        },
    )
    .await;
    expect_tampered_t_conflict(
        &fx,
        &app,
        &rotation,
        primary,
        fallback,
        old_rev,
        epoch,
        &materials,
        |t| {
            t.expected_control_rev += 1;
        },
    )
    .await;
    expect_tampered_t_conflict(
        &fx,
        &app,
        &rotation,
        primary,
        fallback,
        old_rev,
        epoch,
        &materials,
        |t| {
            t.primary_cid[0] ^= 0xFF;
        },
    )
    .await;
    expect_tampered_t_conflict(
        &fx,
        &app,
        &rotation,
        primary,
        fallback,
        old_rev,
        epoch,
        &materials,
        |t| {
            t.primary_commit_digest[0] ^= 0xFF;
        },
    )
    .await;
    expect_tampered_t_conflict(
        &fx,
        &app,
        &rotation,
        primary,
        fallback,
        old_rev,
        epoch,
        &materials,
        |t| {
            t.fallback_cid[0] ^= 0xFF;
        },
    )
    .await;
    expect_tampered_t_conflict(
        &fx,
        &app,
        &rotation,
        primary,
        fallback,
        old_rev,
        epoch,
        &materials,
        |t| {
            t.fallback_commit_digest[0] ^= 0xFF;
        },
    )
    .await;
    expect_tampered_t_conflict(
        &fx,
        &app,
        &rotation,
        primary,
        fallback,
        old_rev,
        epoch,
        &materials,
        |t| {
            t.slot_set_digest[0] ^= 0xFF;
        },
    )
    .await;
    expect_tampered_t_conflict(
        &fx,
        &app,
        &rotation,
        primary,
        fallback,
        old_rev,
        epoch,
        &materials,
        |t| {
            t.member_cert_digest[0] ^= 0xFF;
        },
    )
    .await;
    expect_tampered_t_conflict(
        &fx,
        &app,
        &rotation,
        primary,
        fallback,
        old_rev,
        epoch,
        &materials,
        |t| {
            t.grants_digest[0] ^= 0xFF;
        },
    )
    .await;
    expect_tampered_t_conflict(
        &fx,
        &app,
        &rotation,
        primary,
        fallback,
        old_rev,
        epoch,
        &materials,
        |t| {
            t.old_asset_set_digest[0] ^= 0xFF;
        },
    )
    .await;
    expect_tampered_t_conflict(
        &fx,
        &app,
        &rotation,
        primary,
        fallback,
        old_rev,
        epoch,
        &materials,
        |t| {
            t.new_asset_set_digest[0] ^= 0xFF;
        },
    )
    .await;

    // 所有拒绝都不推进状态：轮换仍停在 staging、两份候选未被 promote。
    assert_eq!(
        block(|| fx.rotation_view(&rotation).rotation.state),
        relay::rotations::RotationState::Staging,
        "被拒绝的 activate 不得改变 rotation 状态"
    );

    // 合法 T（与 DB 事实逐项一致 + 旧/新双签）通过。
    let (status, resp) = post_activate(
        &fx,
        &app,
        &rotation,
        activate_body(&fx, &rotation, valid_map.clone(), primary, fallback),
        valid_map,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&resp));
    assert_eq!(fx.vault().active_epoch, epoch + 1);
    assert_eq!(fx.vault().manager_rev, old_rev + 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn candidate_checkpoint_role_and_request_id_creation() {
    let fx = block(|| Fixture::with_real_manager("wire-candidate"));
    let app = app_ed25519(&fx);
    let manager = block(|| fx.manager_token());
    let request_id = [0x11u8; 16];
    let (request, _) = begin_request(&fx, &manager, request_id);
    let (status, first) = call(&app, request).await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&first));
    let to_epoch = 2u64;
    let device = block(|| fx.device_token());
    let request_id = [0x21u8; 16];
    let fields = vec![
        (0, wire::uint(4096)),
        (1, wire::uint(to_epoch)),
        (2, wire::text("primary")),
        (3, wire::bytes(request_id)),
    ];
    let (status, created) = call(
        &app,
        post_cbor(&url(&fx, "/checkpoints"), Some(&device), fields.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{}", error_code(&created));
    let created = fields_of(&created);
    let cid = wire::id16_field(&created, 0).expect("cid");
    assert_eq!(wire::text_field(&created, 5).expect("role"), "primary");

    // 同 request_id 同请求 → 原结果（幂等）。
    let (status, replay) = call(
        &app,
        post_cbor(&url(&fx, "/checkpoints"), Some(&device), fields.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(wire::id16_field(&fields_of(&replay), 0).expect("cid"), cid);

    // 同 request_id 不同请求 → 409。
    let mut other = fields;
    other[0].1 = wire::uint(8192);
    let (status, resp) = call(
        &app,
        post_cbor(&url(&fx, "/checkpoints"), Some(&device), other),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{}", error_code(&resp));

    // 同一 epoch + role 再建 → 409（固定 role 唯一）。
    let (status, resp) = call(
        &app,
        post_cbor(
            &url(&fx, "/checkpoints"),
            Some(&device),
            vec![
                (0, wire::uint(4096)),
                (1, wire::uint(to_epoch)),
                (2, wire::text("primary")),
            ],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{}", error_code(&resp));

    // active epoch 不允许带 role。
    let (status, resp) = call(
        &app,
        post_cbor(
            &url(&fx, "/checkpoints"),
            Some(&device),
            vec![(0, wire::uint(4096)), (2, wire::text("fallback"))],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{}", error_code(&resp));

    // candidate GET：staging、role、readback-ready=false（未发布）；不冒充 published。
    let (status, detail) = call(
        &app,
        get(&url(&fx, &format!("/checkpoints/{}", b64(&cid))), &device),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&detail));
    let detail = wire::value_map(&fields_of(&detail)[0].1).expect("detail");
    assert_eq!(wire::text_field(&detail, 1).expect("state"), "staging");
    assert_eq!(wire::text_field(&detail, 2).expect("role"), "primary");
    assert!(!wire::bool_field(&detail, 9).expect("ready"));
    assert_eq!(wire::uint_field(&detail, 10).expect("epoch"), to_epoch);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn staged_grants_are_management_only_and_carry_package_digest() {
    let fx = block(|| Fixture::with_real_manager("wire-grants"));
    let app = app_ed25519(&fx);
    let manager = block(|| fx.manager_token());
    let request_id = [0x11u8; 16];
    let (request, _) = begin_request(&fx, &manager, request_id);
    let (status, first) = call(&app, request).await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&first));
    let rotation = wire::id16_field(&wire::value_map(&fields_of(&first)[0].1).expect("view"), 0)
        .expect("rotation id");
    block(|| fx.stage_grant(&rotation, &fx.device));

    let device = block(|| fx.device_token());
    let manager = block(|| fx.manager_token());
    let grants_uri = url(&fx, &format!("/rotations/{}/grants", b64(&rotation)));
    // device realm 不得读 staged grants。
    let (status, resp) = call(&app, get(&grants_uri, &device)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{}", error_code(&resp));
    // management 可读，且带 signature64/grant_id/activation_ref/leaf/包 digest。
    let (status, resp) = call(&app, get(&grants_uri, &manager)).await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&resp));
    let grants = wire::array_field(&fields_of(&resp), 0).expect("grants");
    assert_eq!(grants.len(), 1);
    let grant = wire::value_map(&grants[0]).expect("grant");
    assert_eq!(wire::bytes_field(&grant, 0).expect("device"), fx.device);
    assert_eq!(wire::bytes_field(&grant, 6).expect("signature").len(), 9);
    assert_eq!(wire::id16_field(&grant, 7).expect("grant id"), [3u8; 16]);
    assert_eq!(
        wire::id16_field(&grant, 8).expect("activation ref"),
        rotation
    );
    assert!(!wire::bytes_field(&grant, 5).expect("leaf").is_empty());
    assert_eq!(wire::bytes_field(&grant, 9).expect("digest").len(), 32);

    // 最新 grant（激活后）：额外返回 grant_id/activation_ref/leaf digest/包 digest。
    block(|| {
        let row = fx.rotation_view(&rotation).rotation;
        let (_a, _b) = fx.publish_candidate_baselines(row.to_epoch);
        let reservation = fx.rotation_reservation(&rotation);
        fx.stage_candidate_slots(row.to_epoch, &reservation);
    });
    block(|| fx.activate_rotation(&rotation)).expect("activate");
    let device = block(|| fx.device_token());
    let (status, latest) = call(&app, get(&url(&fx, "/devices/self/grants/latest"), &device)).await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&latest));
    let latest = fields_of(&latest);
    assert_eq!(wire::id16_field(&latest, 7).expect("grant id"), [3u8; 16]);
    assert!(wire::opt_field(&latest, 8).is_none() || wire::id16_field(&latest, 8).is_ok());
    assert_eq!(wire::hash32_field(&latest, 9).expect("leaf digest"), {
        let header = wire::bytes_field(&latest, 5).expect("signed header");
        sha256(&header)
    });
    assert_eq!(
        wire::hash32_field(&latest, 10)
            .expect("package digest")
            .len(),
        32
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_and_revoke_more_are_atomic_and_keep_frozen() {
    let fx = block(|| Fixture::with_real_manager("wire-restart"));
    let app = app_ed25519(&fx);
    let manager = block(|| fx.manager_token());
    let request_id = [0x11u8; 16];
    let (request, _) = begin_request(&fx, &manager, request_id);
    let (status, first) = call(&app, request).await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&first));
    let rotation = wire::id16_field(&wire::value_map(&fields_of(&first)[0].1).expect("view"), 0)
        .expect("rotation id");
    // 候选材料：两份基线 + grant + 槽（让 revoke-more 触发废弃候选）。
    let row = block(|| fx.rotation_view(&rotation).rotation);
    block(|| {
        let (_a, _b) = fx.publish_candidate_baselines(row.to_epoch);
        let reservation = fx.rotation_reservation(&rotation);
        fx.stage_candidate_slots(row.to_epoch, &reservation);
        fx.stage_grant(&rotation, &fx.device);
    });

    // restart：原子废弃候选并分配更高 epoch，vault 保持 ROTATING。
    let manager = block(|| fx.manager_token());
    let control_rev = fx.vault().control_rev;
    let body = cbor_body(Vec::new());
    let request_id = [0x31u8; 16];
    let signature = fx.sign_management(
        relay::proto::HttpMethod::Post,
        &url(&fx, &format!("/rotations/{}/restart", b64(&rotation))),
        request_id,
        control_rev,
        &body,
    );
    let fields = vec![
        (0, Fixture::signed_wrapper(body.clone(), signature)),
        (1, wire::bytes(request_id)),
        (2, wire::uint(control_rev)),
    ];
    let (status, resp) = call(
        &app,
        post_cbor(
            &url(&fx, &format!("/rotations/{}/restart", b64(&rotation))),
            Some(&manager),
            fields,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&resp));
    let response = fields_of(&resp);
    assert!(wire::bool_field(&response, 1).expect("restarted"));
    let restart_view = wire::value_map(&response[0].1).expect("view");
    let new_rotation = wire::id16_field(&restart_view, 0).expect("new rotation");
    assert_ne!(new_rotation, rotation);
    assert_eq!(wire::text_field(&restart_view, 1).expect("state"), "frozen");
    assert_eq!(fx.vault().state, relay::vault::VaultState::Rotating);
    assert_eq!(
        block(|| fx.rotation_view(&rotation).rotation.state),
        relay::rotations::RotationState::Aborted,
        "旧候选必须废弃"
    );
    assert_eq!(
        block(|| fx.db.read(|conn| relay::vault::epoch_state(conn, 2))).expect("epoch"),
        Some(relay::vault::EpochState::Aborted)
    );

    // revoke-more：为已生成候选 grant 的设备追加吊销 → 原子重启到更高 epoch。
    let second = [9u8; 16];
    block(|| fx.add_device(second));
    let new_row = block(|| fx.rotation_view(&new_rotation).rotation);
    block(|| {
        let (_a, _b) = fx.publish_candidate_baselines(new_row.to_epoch);
        let reservation = fx.rotation_reservation(&new_rotation);
        fx.stage_candidate_slots(new_row.to_epoch, &reservation);
        fx.stage_grant(&new_rotation, &second);
    });
    let manager = block(|| fx.manager_token());
    let control_rev = fx.vault().control_rev;
    let body = cbor_body(vec![(0, wire::array(vec![wire::bytes(second)]))]);
    let request_id = [0x32u8; 16];
    let signature = fx.sign_management(
        relay::proto::HttpMethod::Post,
        &url(
            &fx,
            &format!("/rotations/{}/revoke-more", b64(&new_rotation)),
        ),
        request_id,
        control_rev,
        &body,
    );
    let fields = vec![
        (0, Fixture::signed_wrapper(body.clone(), signature)),
        (1, wire::bytes(request_id)),
        (2, wire::uint(control_rev)),
        (3, wire::array(vec![wire::bytes(second)])),
    ];
    let (status, resp) = call(
        &app,
        post_cbor(
            &url(
                &fx,
                &format!("/rotations/{}/revoke-more", b64(&new_rotation)),
            ),
            Some(&manager),
            fields,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&resp));
    let response = fields_of(&resp);
    assert!(wire::bool_field(&response, 1).expect("restarted"));
    assert_eq!(wire::uint_field(&response, 2).expect("revoked"), 1);
    let final_view = wire::value_map(&response[0].1).expect("view");
    let final_rotation = wire::id16_field(&final_view, 0).expect("final rotation");
    assert_ne!(final_rotation, new_rotation);
    assert_eq!(wire::text_field(&final_view, 1).expect("state"), "frozen");
    assert_eq!(fx.vault().state, relay::vault::VaultState::Rotating);
    assert_eq!(fx.vault().active_epoch, 1, "绝不能解冻旧 epoch");
    // 被吊销设备不能认证。
    let err = block(|| {
        fx.db.read(|conn| {
            relay::auth::create_challenge(
                conn,
                fx.clock.as_ref(),
                relay::auth::Realm::Device,
                &second,
            )
        })
    })
    .expect_err("revoked device");
    assert_eq!(err.code(), Some(ErrorCode::Revoked));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn frozen_assets_paginate_independently_of_log() {
    let fx = block(|| Fixture::with_real_manager("wire-assets"));
    let app = app_ed25519(&fx);
    let asset = block(|| {
        let (object_id, _, _) = fx.create_object(ObjectType::Asset, 0, 0, 64);
        object_id
    });
    let manager = block(|| fx.manager_token());
    let request_id = [0x11u8; 16];
    let (request, _) = begin_request(&fx, &manager, request_id);
    let (status, first) = call(&app, request).await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&first));
    let rotation = wire::id16_field(&wire::value_map(&fields_of(&first)[0].1).expect("view"), 0)
        .expect("rotation id");
    let row = block(|| fx.rotation_view(&rotation).rotation);
    let device = block(|| fx.device_token());
    let uri = url(
        &fx,
        &format!("/rotations/{}/assets?limit=1", b64(&rotation)),
    );
    let (status, page) = call(&app, get(&uri, &device)).await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&page));
    let page = fields_of(&page);
    let entries = wire::array_field(&page, 0).expect("entries");
    assert_eq!(entries.len(), 1);
    let entry = wire::value_map(&entries[0]).expect("entry");
    assert_eq!(wire::hash32_field(&entry, 0).expect("object"), asset);
    assert_eq!(wire::uint_field(&page, 1).expect("cutoff"), row.freeze_seq);
    assert_eq!(wire::uint_field(&page, 3).expect("total"), 1);
    let digest = wire::hash32_field(&page, 2).expect("digest");
    assert_eq!(row.asset_set_digest, Some(digest));

    // 删掉 log（模拟日志前缀 GC）后仍可枚举、digest 不变。
    block(|| {
        fx.raw_write(|conn| {
            conn.execute("DELETE FROM log WHERE kind = 'asset'", [])?;
            Ok(())
        })
    })
    .expect("drop asset log");
    let (status, page) = call(&app, get(&uri, &device)).await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&page));
    let page = fields_of(&page);
    assert_eq!(wire::array_field(&page, 0).expect("entries").len(), 1);
    assert_eq!(wire::hash32_field(&page, 2).expect("digest"), digest);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn degraded_bootstrap_reads_single_baseline_while_gc_blocks() {
    let fx = block(|| Fixture::with_real_manager("wire-degraded"));
    let app = app_ed25519(&fx);
    block(|| {
        fx.create_object(ObjectType::Update, 1, 1, 32);
        fx.publish_baseline();
        fx.create_object(ObjectType::Update, 1, 2, 32);
        fx.publish_baseline();
    });
    let device = block(|| fx.device_token());
    let bootstrap_uri = url(&fx, "/bootstrap");
    let (status, resp) = call(&app, post_cbor(&bootstrap_uri, Some(&device), Vec::new())).await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&resp));
    assert!(!wire::bool_field(&fields_of(&resp), 7).expect("degraded"));

    // 损坏其中一份基线的某个分片 → 严格 bootstrap 409；degraded 读回另一份。
    let c1 = {
        // 最新两份按 published_at；取较早的那份。
        let mut ids: Vec<Id16> = fx
            .db
            .read(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT checkpoint_id FROM checkpoints WHERE state='published'
                     ORDER BY published_at ASC, rowid ASC",
                )?;
                let rows = stmt.query_map([], |row| row.get::<_, Vec<u8>>(0))?;
                let mut out = Vec::new();
                for row in rows {
                    out.push(
                        row?.as_slice()
                            .try_into()
                            .map_err(|_| relay::Error::Invariant("cid"))?,
                    );
                }
                Ok(out)
            })
            .expect("checkpoints");
        ids.remove(0)
    };
    let refs = block(|| fx.db.read(|conn| checkpoints::refs(conn, &c1))).expect("refs");
    let (victim, _) = refs[0];
    block(|| {
        fx.raw_write(move |conn| {
            conn.execute(
                "UPDATE object_parts SET envelope = ?1 WHERE object_id = ?2 AND part_no = 0",
                (vec![0x11u8; 300].as_slice(), victim.as_slice()),
            )?;
            Ok(())
        })
    })
    .expect("corrupt baseline");
    let device = block(|| fx.device_token());
    let (status, resp) = call(&app, post_cbor(&bootstrap_uri, Some(&device), Vec::new())).await;
    assert_eq!(status, StatusCode::CONFLICT, "{}", error_code(&resp));
    assert_eq!(error_code(&resp), "NO_VALID_BASELINE");

    let device = block(|| fx.device_token());
    let degraded_uri = format!("{bootstrap_uri}?degraded=1");
    let (status, resp) = call(&app, post_cbor(&degraded_uri, Some(&device), Vec::new())).await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&resp));
    let fields = fields_of(&resp);
    assert!(
        wire::bool_field(&fields, 7).expect("degraded"),
        "必须标记降级"
    );
    assert!(
        wire::opt_field(&fields, 1).is_none(),
        "降级时不应返回 fallback 条目"
    );
    assert!(wire::opt_field(&fields, 0).is_some(), "仍应返回可用基线");

    // GC 在补齐两份健康基线前不得推进 floor。
    fx.clock.advance_secs(8 * DAY);
    let outcome = block(|| fx.gc());
    assert!(outcome.blocked, "{outcome:?}");
    assert_eq!(fx.vault().gc_floor, 0);
}

// ============================================================ 评审 0002 T06
// activate 在每个 SQL 点失败后的全有或全无 + commit 后丢回复的 exact 恢复。
//
// 真实 SQLite/WAL + 真实 Ed25519 管理双签 + 真实 axum HTTP；故障点用 TRIGGER 注入。
// 失败后 vault/epoch/rotation/manager_keys/control/checkpoints/slots/reservation/log/dedup
// 必须逐项不变；撤销注入后同一 request_id/body 必须成功并可原样回放。

#[derive(Debug, Clone, PartialEq, Eq)]
struct ActivateFacts {
    manager_rev: u64,
    active_epoch: u64,
    next_epoch: u64,
    state: String,
    control_rev: u64,
    auth_rev: u64,
    head_seq: u64,
    manager_keys: i64,
    controls: i64,
    dedup: i64,
    control_log: i64,
    rotation_state: String,
    primary_checkpoint: Option<Vec<u8>>,
    fallback_checkpoint: Option<Vec<u8>>,
    activation_commit: Option<Vec<u8>>,
    epoch1_state: String,
    epoch2_state: String,
    slots_active: i64,
    slots_staging: i64,
    primary_state: String,
    fallback_state: String,
    reservation_state: String,
    gc_plans: i64,
}

#[allow(clippy::too_many_arguments)]
fn activate_facts(fx: &Fixture, rotation: &Id16, primary: Id16, fallback: Id16) -> ActivateFacts {
    let vault = fx.vault();
    fx.db
        .read(move |conn| {
            let epoch_state = |epoch: u64| -> rusqlite::Result<String> {
                conn.query_row(
                    "SELECT state FROM epochs WHERE epoch = ?1",
                    [i64::try_from(epoch).expect("epoch")],
                    |row| row.get(0),
                )
            };
            let count =
                |sql: &str| -> rusqlite::Result<i64> { conn.query_row(sql, [], |row| row.get(0)) };
            let (rotation_state, primary_checkpoint, fallback_checkpoint, activation_commit) = conn
                .query_row(
                    "SELECT state, primary_checkpoint, fallback_checkpoint, activation_commit
                     FROM rotations WHERE rotation_id = ?1",
                    [rotation.as_slice()],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, Option<Vec<u8>>>(1)?,
                            row.get::<_, Option<Vec<u8>>>(2)?,
                            row.get::<_, Option<Vec<u8>>>(3)?,
                        ))
                    },
                )?;
            let checkpoint_state = |cid: &Id16| -> rusqlite::Result<String> {
                conn.query_row(
                    "SELECT state FROM checkpoints WHERE checkpoint_id = ?1",
                    [cid.as_slice()],
                    |row| row.get(0),
                )
            };
            Ok(ActivateFacts {
                manager_rev: vault.manager_rev,
                active_epoch: vault.active_epoch,
                next_epoch: vault.next_epoch,
                state: format!("{:?}", vault.state),
                control_rev: vault.control_rev,
                auth_rev: vault.auth_rev,
                head_seq: vault.head_seq,
                manager_keys: count("SELECT COUNT(*) FROM manager_keys")?,
                controls: count("SELECT COUNT(*) FROM control_commits")?,
                dedup: count("SELECT COUNT(*) FROM dedup")?,
                control_log: count("SELECT COUNT(*) FROM log WHERE kind = 'control'")?,
                rotation_state,
                primary_checkpoint,
                fallback_checkpoint,
                activation_commit,
                epoch1_state: epoch_state(1)?,
                epoch2_state: epoch_state(2)?,
                slots_active: count("SELECT COUNT(*) FROM key_slots WHERE state = 'active'")?,
                slots_staging: count("SELECT COUNT(*) FROM key_slots WHERE state = 'staging'")?,
                primary_state: checkpoint_state(&primary)?,
                fallback_state: checkpoint_state(&fallback)?,
                reservation_state: conn.query_row(
                    "SELECT state FROM maintenance_reservations
                     WHERE purpose = 'rotation' AND resource_id = ?1",
                    [rotation.as_slice()],
                    |row| row.get(0),
                )?,
                gc_plans: count("SELECT COUNT(*) FROM gc_plans")?,
            })
        })
        .expect("activate facts")
}

fn install_fault_trigger(fx: &Fixture, sql: &str) {
    let sql = sql.to_owned();
    fx.raw_write(move |conn| {
        conn.execute_batch(&sql)?;
        Ok(())
    })
    .expect("安装故障注入触发器");
}

fn drop_fault_trigger(fx: &Fixture, name: &str) {
    let sql = format!("DROP TRIGGER {name}");
    fx.raw_write(move |conn| {
        conn.execute_batch(&sql)?;
        Ok(())
    })
    .expect("撤销故障注入触发器");
}

#[allow(clippy::too_many_arguments)]
fn activate_http_request(
    fx: &Fixture,
    rotation: &Id16,
    body: &[u8],
    manager_map: &wire::CdeValue,
    request_id: Id16,
    control_rev: u64,
    signature: &[u8],
    token: &str,
) -> Request<Body> {
    let outer = vec![
        (
            0,
            Fixture::signed_wrapper(body.to_vec(), signature.to_vec()),
        ),
        (1, wire::bytes(request_id)),
        (2, wire::uint(control_rev)),
        (3, manager_map.clone()),
    ];
    post_cbor(
        &url(fx, &format!("/rotations/{}/activate", b64(rotation))),
        Some(token),
        outer,
    )
}

/// 为任意管理公钥签发真实 Ed25519 管理 token（激活后新管理 key 的认证路径）。
fn manager_token_for_key(fx: &Fixture, key: &SigningKey) -> String {
    let public = key.verifying_key().to_bytes();
    let subject = relay::auth::Realm::management_subject(&public);
    let clock = fx.clock.clone();
    let challenge = fx
        .db
        .write_blocking(move |conn| {
            relay::auth::create_challenge(
                conn,
                clock.as_ref(),
                relay::auth::Realm::Management,
                &subject,
            )
        })
        .expect("management challenge");
    let instance = fx.vault().instance_id;
    let challenge_wire = relay::proto::Challenge {
        pv: 1,
        relay_instance_id: instance,
        origin_sha256: sha256(b"http://relay.test"),
        vault_id: fx.vault_id,
        realm: relay::proto::AuthRealm::Management,
        subject,
        challenge_id: challenge.challenge_id,
        nonce: challenge.nonce,
        expires_at: u64::try_from(challenge.expires_at).expect("expires_at"),
    };
    let message = challenge_wire.encode().expect("challenge encode");
    let signing_input = relay::proto::auth_signing_input(&challenge_wire).expect("signing input");
    let signature = key.sign(&signing_input).to_bytes().to_vec();
    let challenge_id = challenge.challenge_id;
    let clock = fx.clock.clone();
    fx.db
        .write_blocking(move |conn| {
            relay::auth::issue_token(
                conn,
                clock.as_ref(),
                &Ed25519Hook,
                &challenge_id,
                &message,
                &signature,
            )
        })
        .expect("management token")
        .token
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn activate_is_all_or_nothing_at_each_sql_point_and_replays_exact_request() {
    let fx = block(|| Fixture::with_real_manager("wire-activate-sql"));
    let app = app_ed25519(&fx);

    // 真实 begin（HTTP + 管理签名）。
    let manager = block(|| fx.manager_token());
    let begin_id = [0x5A; 16];
    let (request, _body) = begin_request(&fx, &manager, begin_id);
    let (status, first) = call(&app, request).await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&first));
    let rotation = wire::id16_field(&wire::value_map(&fields_of(&first)[0].1).expect("view"), 0)
        .expect("rotation id");

    // 候选材料（真实 relay 事务；grant/槽用测试 hook 准备，activate 本身走 Ed25519）。
    let (primary, fallback) = prepare_rotation(&fx, &rotation);
    let materials = block(|| activation_materials(&fx, &rotation, primary, fallback));
    let (t_bytes, old_t_signature, new_t_signature) =
        resign_activation_t(&fx, &materials, &materials.activation_t);
    let manager_map = materials.manager_map(&t_bytes, &old_t_signature, &new_t_signature);
    let body = block(|| activate_body(&fx, &rotation, manager_map.clone(), primary, fallback));

    let initial = block(|| activate_facts(&fx, &rotation, primary, fallback));
    let control_rev = initial.control_rev;
    let activate_id = [0x6B; 16];
    let signature = block(|| {
        fx.sign_management(
            relay::proto::HttpMethod::Post,
            &url(&fx, &format!("/rotations/{}/activate", b64(&rotation))),
            activate_id,
            control_rev,
            &body,
        )
    });

    // 每个 activate 事务内的 SQL 点各失败一次。
    let points: &[(&str, &str)] = &[
        (
            "fail_manager_keys",
            "CREATE TRIGGER fail_manager_keys BEFORE INSERT ON manager_keys \
             BEGIN SELECT RAISE(ABORT,'injected manager_keys'); END;",
        ),
        (
            "fail_manager_rev",
            "CREATE TRIGGER fail_manager_rev BEFORE UPDATE ON vault \
             WHEN NEW.manager_rev <> OLD.manager_rev \
             BEGIN SELECT RAISE(ABORT,'injected manager_rev'); END;",
        ),
        (
            "fail_slots_retire",
            "CREATE TRIGGER fail_slots_retire BEFORE UPDATE ON key_slots \
             BEGIN SELECT RAISE(ABORT,'injected slots'); END;",
        ),
        (
            "fail_epoch_switch",
            "CREATE TRIGGER fail_epoch_switch BEFORE UPDATE ON epochs \
             WHEN NEW.state = 'active' BEGIN SELECT RAISE(ABORT,'injected epoch'); END;",
        ),
        (
            "fail_rotation_activated",
            "CREATE TRIGGER fail_rotation_activated BEFORE UPDATE ON rotations \
             WHEN NEW.state = 'activated' \
             BEGIN SELECT RAISE(ABORT,'injected rotation state'); END;",
        ),
        (
            "fail_vault_switch",
            "CREATE TRIGGER fail_vault_switch BEFORE UPDATE ON vault \
             WHEN NEW.active_epoch <> OLD.active_epoch \
             BEGIN SELECT RAISE(ABORT,'injected active epoch'); END;",
        ),
        (
            "fail_activation_control",
            "CREATE TRIGGER fail_activation_control BEFORE INSERT ON control_commits \
             BEGIN SELECT RAISE(ABORT,'injected control'); END;",
        ),
        (
            "fail_activation_reservation",
            "CREATE TRIGGER fail_activation_reservation BEFORE UPDATE ON maintenance_reservations \
             BEGIN SELECT RAISE(ABORT,'injected reservation'); END;",
        ),
        (
            "fail_promote_checkpoints",
            "CREATE TRIGGER fail_promote_checkpoints BEFORE UPDATE ON checkpoints \
             BEGIN SELECT RAISE(ABORT,'injected checkpoint'); END;",
        ),
        (
            "fail_gc_plan",
            "CREATE TRIGGER fail_gc_plan BEFORE INSERT ON gc_plans \
             BEGIN SELECT RAISE(ABORT,'injected gc plan'); END;",
        ),
        (
            "fail_activation_dedup",
            "CREATE TRIGGER fail_activation_dedup BEFORE INSERT ON dedup \
             BEGIN SELECT RAISE(ABORT,'injected dedup'); END;",
        ),
    ];
    for (name, sql) in points {
        block(|| install_fault_trigger(&fx, sql));
        let token = block(|| fx.manager_token());
        let (status, response) = call(
            &app,
            activate_http_request(
                &fx,
                &rotation,
                &body,
                &manager_map,
                activate_id,
                control_rev,
                &signature,
                &token,
            ),
        )
        .await;
        assert!(
            !status.is_success(),
            "注入点 {name} 必须让 activate 失败，实际 {status} {}",
            error_code(&response)
        );
        assert_eq!(
            block(|| activate_facts(&fx, &rotation, primary, fallback)),
            initial,
            "注入点 {name} 失败后不得留下任何部分事实"
        );
        block(|| drop_fault_trigger(&fx, name));
    }

    // 撤销全部注入后，同一 request_id/body 必须成功。
    let stale_token = block(|| fx.manager_token());
    let token = stale_token.clone();
    let (status, response) = call(
        &app,
        activate_http_request(
            &fx,
            &rotation,
            &body,
            &manager_map,
            activate_id,
            control_rev,
            &signature,
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&response));

    let applied = block(|| activate_facts(&fx, &rotation, primary, fallback));
    assert_eq!(applied.manager_rev, initial.manager_rev + 1);
    assert_eq!(applied.active_epoch, initial.active_epoch + 1);
    assert_eq!(applied.next_epoch, initial.next_epoch);
    assert_eq!(applied.state, "Active");
    assert_eq!(applied.control_rev, initial.control_rev + 1);
    assert_eq!(applied.auth_rev, initial.auth_rev + 1);
    assert_eq!(
        applied.head_seq, initial.head_seq,
        "activate 不再把 control 事件写入数据日志（control 只进控制链）"
    );
    assert_eq!(applied.manager_keys, initial.manager_keys + 1);
    assert_eq!(applied.controls, initial.controls + 1);
    assert_eq!(applied.dedup, initial.dedup + 1);
    assert_eq!(applied.control_log, initial.control_log);
    assert_eq!(applied.rotation_state, "activated");
    assert_eq!(applied.primary_checkpoint.as_deref(), Some(&primary[..]));
    assert_eq!(applied.fallback_checkpoint.as_deref(), Some(&fallback[..]));
    assert_eq!(applied.activation_commit.as_ref().map(Vec::len), Some(32));
    assert_eq!(applied.epoch1_state, "retired");
    assert_eq!(applied.epoch2_state, "active");
    assert_eq!(applied.slots_active, 2, "候选 password/recovery 槽上线");
    assert_eq!(applied.slots_staging, 0);
    assert_eq!(applied.primary_state, "published");
    assert_eq!(applied.fallback_state, "published");
    assert_eq!(applied.reservation_state, "complete");
    assert_eq!(applied.gc_plans, initial.gc_plans + 1);

    // commit 后"丢回复"：旧管理签名/旧 token 必须失效（fail-closed）。
    let (status, refused) = call(
        &app,
        activate_http_request(
            &fx,
            &rotation,
            &body,
            &manager_map,
            activate_id,
            control_rev,
            &signature,
            &stale_token,
        ),
    )
    .await;
    assert!(
        !status.is_success(),
        "激活后旧 manager token/签名不得再认证，实际 {status} {}",
        error_code(&refused)
    );
    assert_eq!(
        block(|| activate_facts(&fx, &rotation, primary, fallback)),
        applied,
        "被拒绝的旧签名重放必须零效应"
    );

    // exact request 查询走**新管理 key**：必须回放原始响应字节，且零新增事实。
    let new_manager_token = block(|| manager_token_for_key(&fx, &materials.new_manager));
    let lookup = format!(
        "{}?request_id={}",
        url(&fx, "/rotations/current"),
        relay::ids::hex_lower(activate_id.as_slice())
    );
    let (status, stored) = call(&app, get(&lookup, &new_manager_token)).await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&stored));
    let stored = fields_of(&stored);
    assert_eq!(
        wire::bytes_field(&stored, 1).expect("stored response"),
        response,
        "exact request 必须回放激活的原始响应字节"
    );
    assert_eq!(
        wire::text_field(&stored, 2).expect("scope"),
        "management/activate"
    );
    assert_eq!(
        block(|| activate_facts(&fx, &rotation, primary, fallback)),
        applied
    );

    // 重启（新连接）仍持正确 active epoch / manager rev / 激活 commit。
    let db = {
        let cfg = fx.cfg.clone();
        let clock = fx.clock.clone();
        block(|| relay::db::Db::open(cfg.db.clone(), clock)).expect("reopen")
    };
    let (epoch, manager_rev, rotation_state, commit) = db
        .read(move |conn| {
            let vault = relay::vault::load(conn)?;
            let (state, commit): (String, Option<Vec<u8>>) = conn.query_row(
                "SELECT state, activation_commit FROM rotations WHERE rotation_id = ?1",
                [rotation.as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            Ok((vault.active_epoch, vault.manager_rev, state, commit))
        })
        .expect("reopen facts");
    assert_eq!(epoch, applied.active_epoch);
    assert_eq!(manager_rev, applied.manager_rev);
    assert_eq!(rotation_state, "activated");
    assert_eq!(commit.as_deref(), applied.activation_commit.as_deref());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grant_leaf_manifest_rejects_no_token_revoked_and_non_member_device() {
    let fx = block(|| Fixture::with_real_manager("wire-leaves-auth"));
    let app = app_ed25519(&fx);
    let rotation = block(|| fx.begin_rotation(&[]).rotation.rotation_id);
    let row = block(|| fx.rotation_view(&rotation).rotation);
    block(|| fx.publish_candidate_baselines(row.to_epoch));
    block(|| fx.stage_grant(&rotation, &fx.device));
    assert_eq!(
        block(|| fx.rotation_view(&rotation).rotation.state),
        relay::rotations::RotationState::Staging
    );

    // 无 token：认证前就拒绝（不得泄露 rotation 是否存在）。
    let (status, body) = call(&app, get_no_token(&leaves_url(&fx, &rotation))).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{}", error_code(&body));

    // active 但不在该 rotation 的成员集合：403，不能借设备 realm 枚举别人的 grant。
    let outsider = [0x6fu8; 16];
    block(|| fx.add_device(outsider));
    let outsider_token = block(|| fx.device_token_for(&outsider));
    let (status, body) = call(&app, get(&leaves_url(&fx, &rotation), &outsider_token)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{}", error_code(&body));
    assert_eq!(error_code(&body), "FORBIDDEN");

    // 已吊销设备：即使旧 token 还没到期也必须拒绝（auth_rev/status 双门禁）。
    let device_token = block(|| fx.device_token());
    let device = fx.device;
    block(|| {
        fx.db
            .write_blocking(move |conn| {
                conn.execute(
                    "UPDATE devices SET status = 'revoked' WHERE device_id = ?1",
                    [device.as_slice()],
                )?;
                Ok(())
            })
            .expect("revoke device")
    });
    let (status, body) = call(&app, get(&leaves_url(&fx, &rotation), &device_token)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{}", error_code(&body));
    assert_eq!(error_code(&body), "REVOKED");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grant_leaf_manifest_rejects_frozen_and_aborted_rotation() {
    let fx = block(|| Fixture::new("wire-leaves-state"));
    let app = app_ed25519(&fx);
    let rotation = block(|| fx.begin_rotation(&[]).rotation.rotation_id);
    let token = block(|| fx.device_token());

    // frozen：grant 还没收齐，不能当作完整 inclusion 证据。
    let (status, body) = call(&app, get(&leaves_url(&fx, &rotation), &token)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{}", error_code(&body));
    assert_eq!(error_code(&body), "INVALID_STATE");

    // aborted：staged grant 已删，同样不能返回"空 leaf 全集"。
    block(|| fx.abort_rotation(&rotation).expect("abort"));
    let (status, body) = call(&app, get(&leaves_url(&fx, &rotation), &token)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{}", error_code(&body));
    assert_eq!(error_code(&body), "INVALID_STATE");
}
