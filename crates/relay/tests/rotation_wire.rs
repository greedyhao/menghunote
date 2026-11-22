//! task-101：轮换 wire 增补（评审 0001 §三 10 项 / ADR §28.1 条目 4–10）。
//!
//! 覆盖：rotations/current + request_id 幂等查结果、r view 全量字段、候选 checkpoint
//! 元数据/role/request-id 创建、staged grants 仅 management、typed 控制链与
//! `epoch.activated`、activate 材料 CAS + 精确 commit/cids、restart/revoke-more、
//! 冻结资产分页、degraded 读回。

mod support;

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

/// 通过 HTTP 用真实管理签名 activate（材料必须先由 [`prepare_rotation`] 就绪）。
async fn activate_via_http(fx: &Fixture, app: &axum::Router, rotation: &Id16) -> Vec<u8> {
    let row = block(|| fx.rotation_view(rotation).rotation);
    let primary = row.primary_checkpoint;
    let _ = primary;
    let candidates = block(|| {
        fx.db
            .read(|conn| checkpoints::candidates_for_epoch(conn, row.to_epoch))
    })
    .expect("candidates");
    let (primary, fallback) = (candidates[0].checkpoint_id, candidates[1].checkpoint_id);
    let old_manager = fx.manager_signing_key.as_ref().expect("manager key");
    let new_manager = SigningKey::from_bytes(&[0x77u8; 32]);
    let new_pub = new_manager.verifying_key().to_bytes();
    let old_rev = fx.vault().manager_rev;
    let transition = block(|| fx.manager_transition(&row, old_rev, old_rev + 1, new_pub));
    let old_signature = old_manager.sign(&transition).to_bytes().to_vec();
    let new_signature = new_manager.sign(&transition).to_bytes().to_vec();
    let manager_map = wire::map(vec![
        (0, wire::bytes(transition)),
        (1, wire::bytes(old_signature)),
        (2, wire::bytes(new_signature)),
    ]);
    let (member_digest, grants_digest, slot_digest, asset_digest) = block(|| {
        fx.db.read(|conn| {
            Ok((
                relay::rotations::member_cert_digest(conn)?,
                relay::rotations::grants_digest(conn, row.to_epoch)?.1,
                relay::rotations::slot_set_digest(conn, row.to_epoch)?,
                relay::rotations::get(conn, rotation)?.asset_set_digest,
            ))
        })
    })
    .expect("digests");
    let body = cbor_body(vec![
        (0, manager_map.clone()),
        (1, wire::bytes(primary)),
        (2, wire::bytes(fallback)),
        (3, wire::bytes(member_digest)),
        (4, wire::bytes(grants_digest)),
        (5, wire::bytes(slot_digest)),
        (6, wire::bytes(asset_digest.expect("begin 冻结资产摘要"))),
    ]);
    let control_rev = fx.vault().control_rev;
    let request_id = [0x51u8; 16];
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
    let (status, resp) = call(
        app,
        post_cbor(
            &url(fx, &format!("/rotations/{}/activate", b64(rotation))),
            Some(&token),
            outer,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&resp));
    resp
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

    // 完成激活后：epoch.activated 事件显式带 rid/E/S/commit hash/control_rev。
    let activate_resp = activate_via_http(&fx, &app, &rotation).await;
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
    let rotation_row = block(|| fx.rotation_view(&rotation).rotation);
    assert_eq!(rotation_row.activation_commit, Some(activation_commit));
    assert_eq!(rotation_row.activated_control_rev, Some(event.control_rev));
}

fn to_signature(bytes: &[u8]) -> ed25519_dalek::Signature {
    let array: [u8; 64] = bytes.try_into().expect("signature len");
    ed25519_dalek::Signature::from_bytes(&array)
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
