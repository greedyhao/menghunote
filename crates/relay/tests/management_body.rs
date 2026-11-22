//! §26 安全回归：管理写只解析**唯一 canonical body**，外层镜像必须逐字节一致。
//!
//! 这些用例用**真实 Ed25519**（`Fixture::with_real_manager` + `Ed25519Hook`），
//! 因为弱 `TestHook` 无法证明"签名绑定真正被消费的字段"（评审 0001 补充条）：
//! - 外层字段被替换、签名不变 → 403（镜像一致性检查）；
//! - 签名体内字段被改、外层跟着改 → 验签失败（403/400）；
//! - 两份一致 → 接受；
//! - 同 request_id 同 body → 原样回放；同 request_id 不同 body → 409。

mod support;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use ed25519_dalek::{Signer, SigningKey};
use http_body_util::BodyExt;
use relay::api::{router, AppState};
use relay::ids::{sha256, Hash32, Id16};
use relay::verify::Ed25519Hook;
use relay::wire;
use rusqlite::OptionalExtension;
use support::*;
use tower::ServiceExt;

fn block<T>(f: impl FnOnce() -> T) -> T {
    tokio::task::block_in_place(f)
}

/// 真实 Ed25519 的 AppState（管理签名/设备签名都走密码学校验）。
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

fn put_cbor(uri: &str, token: &str, fields: wire::Fields) -> Request<Body> {
    Request::builder()
        .method("PUT")
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, wire::CONTENT_TYPE)
        .body(Body::from(cbor_body(fields)))
        .expect("request")
}

fn get(uri: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .expect("request")
}

fn response_fields(body: &[u8]) -> wire::Fields {
    cbor_fields(body)
}

fn error_code(body: &[u8]) -> String {
    wire::text_field(&response_fields(body), 0).unwrap_or_default()
}

fn b64(bytes: &[u8]) -> String {
    relay::ids::b64url_encode(bytes)
}

fn rotation_id_of(body: &[u8]) -> Id16 {
    let view = wire::value_map(&response_fields(body)[0].1).expect("rotation view");
    wire::id16_field(&view, 0).expect("rotation id")
}

fn invite_id_of(body: &[u8]) -> Id16 {
    wire::id16_field(&response_fields(body), 0).expect("invite id")
}

fn rotation_count(fx: &Fixture) -> i64 {
    fx.db
        .read(|conn| Ok(conn.query_row("SELECT COUNT(*) FROM rotations", [], |row| row.get(0))?))
        .expect("rotation count")
}

fn invite_count(fx: &Fixture) -> i64 {
    fx.db
        .read(|conn| Ok(conn.query_row("SELECT COUNT(*) FROM invites", [], |row| row.get(0))?))
        .expect("invite count")
}

const BEGIN_PATH: &str = "/rotations";

fn begin_path(fx: &Fixture) -> String {
    format!("/v1/vaults/{}{BEGIN_PATH}", b64(&fx.vault_id))
}

fn abort_path(fx: &Fixture, rotation: &Id16) -> String {
    format!(
        "/v1/vaults/{}/rotations/{}/abort",
        b64(&fx.vault_id),
        b64(rotation)
    )
}

fn activate_path(fx: &Fixture, rotation: &Id16) -> String {
    format!(
        "/v1/vaults/{}/rotations/{}/activate",
        b64(&fx.vault_id),
        b64(rotation)
    )
}

fn grant_path(fx: &Fixture, rotation: &Id16, device: &Id16) -> String {
    format!(
        "/v1/vaults/{}/rotations/{}/grants/{}",
        b64(&fx.vault_id),
        b64(rotation),
        b64(device)
    )
}

fn invites_path(fx: &Fixture) -> String {
    format!("/v1/vaults/{}/invites", b64(&fx.vault_id))
}

fn approve_path(fx: &Fixture, device: &Id16) -> String {
    format!(
        "/v1/vaults/{}/devices/{}/approve",
        b64(&fx.vault_id),
        b64(device)
    )
}

fn slot_activate_path(fx: &Fixture, kind: &str) -> String {
    format!("/v1/vaults/{}/key-slots/{kind}/activate", b64(&fx.vault_id))
}

/// begin 的 canonical body（与 sync 客户端 `rotation::begin_body` 同构）。
fn begin_body(
    expected_epoch: u64,
    expected_control_rev: u64,
    request_id: &Id16,
    revoke: &[Id16],
) -> Vec<u8> {
    cbor_body(vec![
        (0, wire::uint(expected_epoch)),
        (1, wire::uint(expected_control_rev)),
        (2, wire::bytes(*request_id)),
        (
            3,
            wire::array(revoke.iter().map(|id| wire::bytes(*id)).collect()),
        ),
    ])
}

/// begin 的旧 wire 外层（镜像字段 1..=4 必须与 body 一致）。
fn begin_outer(
    fx: &Fixture,
    body: &[u8],
    request_id: Id16,
    control_rev: u64,
    expected_epoch: u64,
    revoke: &[Id16],
) -> wire::Fields {
    let signature = fx.sign_management(
        relay::proto::HttpMethod::Post,
        &begin_path(fx),
        request_id,
        control_rev,
        body,
    );
    vec![
        (0, Fixture::signed_wrapper(body.to_vec(), signature)),
        (1, wire::bytes(request_id)),
        (2, wire::uint(control_rev)),
        (3, wire::uint(expected_epoch)),
        (
            4,
            wire::array(revoke.iter().map(|id| wire::bytes(*id)).collect()),
        ),
    ]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn begin_rotation_consumes_only_the_signed_canonical_body() {
    let fx = block(|| Fixture::with_real_manager("mgmt-begin"));
    let app = app_ed25519(&fx);
    let token = block(|| fx.manager_token());
    let vault = fx.vault();
    let expected_epoch = vault.active_epoch;
    let control_rev = vault.control_rev;

    // --- 负例 1：外层 expected_epoch 被替换、签名不变 → 403（不能只执行外层）。
    let request_id = [0x11u8; 16];
    let body = begin_body(expected_epoch, control_rev, &request_id, &[]);
    let mut outer = begin_outer(&fx, &body, request_id, control_rev, expected_epoch, &[]);
    outer[3].1 = wire::uint(expected_epoch + 9);
    let (status, resp) = call(&app, post_cbor(&begin_path(&fx), Some(&token), outer)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{}", error_code(&resp));
    assert_eq!(error_code(&resp), "FORBIDDEN");
    assert_eq!(rotation_count(&fx), 0, "被拒绝的请求不得产生 rotation");

    // --- 负例 2：外层 revoke 列表被替换、签名不变 → 403。
    let mut outer = begin_outer(&fx, &body, request_id, control_rev, expected_epoch, &[]);
    outer[4].1 = wire::array(vec![wire::bytes([0xEEu8; 16])]);
    let (status, resp) = call(&app, post_cbor(&begin_path(&fx), Some(&token), outer)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{}", error_code(&resp));
    assert_eq!(rotation_count(&fx), 0);

    // --- 负例 3：签名体内字段被改（body 换、签名不变）→ 验签失败 403。
    let tampered = begin_body(expected_epoch, control_rev, &request_id, &[[0xEEu8; 16]]);
    let good = begin_body(expected_epoch, control_rev, &request_id, &[]);
    let signature = fx.sign_management(
        relay::proto::HttpMethod::Post,
        &begin_path(&fx),
        request_id,
        control_rev,
        &good,
    );
    let outer = vec![
        (0, Fixture::signed_wrapper(tampered, signature)),
        (1, wire::bytes(request_id)),
        (2, wire::uint(control_rev)),
        (3, wire::uint(expected_epoch)),
        (4, wire::array(vec![wire::bytes([0xEEu8; 16])])),
    ];
    let (status, resp) = call(&app, post_cbor(&begin_path(&fx), Some(&token), outer)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{}", error_code(&resp));
    assert_eq!(error_code(&resp), "MANAGEMENT_REQUIRED");
    assert_eq!(rotation_count(&fx), 0);

    // --- 正例：两份一致 → 接受；同 request_id 同 body 重放 → 原响应。
    let outer = begin_outer(&fx, &body, request_id, control_rev, expected_epoch, &[]);
    let (status, resp) = call(&app, post_cbor(&begin_path(&fx), Some(&token), outer)).await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&resp));
    let rotation = rotation_id_of(&resp);
    assert_eq!(rotation_count(&fx), 1);

    // begin 推进了 auth_rev：旧 token 立即失效（ADR §8 步 2）。
    let stale = begin_outer(&fx, &body, request_id, control_rev, expected_epoch, &[]);
    let (status, resp) = call(&app, post_cbor(&begin_path(&fx), Some(&token), stale)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "旧 token 必须立即失效");
    assert_eq!(error_code(&resp), "REVOKED");

    // 重新认证后同 request_id + 同 body → 原样回放（响应丢失可安全找回）。
    let token = block(|| fx.manager_token());
    let outer = begin_outer(&fx, &body, request_id, control_rev, expected_epoch, &[]);
    let (status, resp) = call(&app, post_cbor(&begin_path(&fx), Some(&token), outer)).await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&resp));
    assert_eq!(
        rotation_id_of(&resp),
        rotation,
        "replay 必须回放原 rotation"
    );
    assert_eq!(rotation_count(&fx), 1, "replay 不得新建 rotation");

    // --- 负例 4：同 request_id 不同 body（签名有效）→ 409。
    let token = block(|| fx.manager_token());
    let other = begin_body(expected_epoch, control_rev, &request_id, &[[0xABu8; 16]]);
    // 外层镜像与 body 一致（避免 403），签名也有效；差异只在 body 内容。
    let outer = begin_outer(
        &fx,
        &other,
        request_id,
        control_rev,
        expected_epoch,
        &[[0xABu8; 16]],
    );
    let (status, resp) = call(&app, post_cbor(&begin_path(&fx), Some(&token), outer)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{}", error_code(&resp));
    assert_eq!(error_code(&resp), "ID_CONFLICT");
    assert_eq!(rotation_count(&fx), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn abort_binds_request_id_and_control_rev_and_replays() {
    let fx = block(|| Fixture::with_real_manager("mgmt-abort"));
    let app = app_ed25519(&fx);
    // 先用 fixture 走一次 begin（TestHook 只用于准备状态；本用例只测 abort 的 canonical body）。
    let rotation = block(|| fx.begin_rotation(&[]).rotation.rotation_id);
    let control_rev = fx.vault().control_rev;
    let request_id = [0x21u8; 16];
    // abort 的 canonical body 是空 map。
    let body = cbor_body(Vec::new());
    let signature = fx.sign_management(
        relay::proto::HttpMethod::Post,
        &abort_path(&fx, &rotation),
        request_id,
        control_rev,
        &body,
    );
    let outer = vec![
        (0, Fixture::signed_wrapper(body.clone(), signature.clone())),
        (1, wire::bytes(request_id)),
        (2, wire::uint(control_rev)),
    ];
    let token = block(|| fx.manager_token());

    // --- 负例：外层 expected_control_rev 被替换、签名不变 → 403。
    let mut bad = outer.clone();
    bad[2].1 = wire::uint(control_rev + 1);
    let (status, resp) = call(
        &app,
        post_cbor(&abort_path(&fx, &rotation), Some(&token), bad),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{}", error_code(&resp));
    assert_eq!(
        fx.rotation_view(&rotation).rotation.state,
        relay::rotations::RotationState::Frozen,
        "被拒绝的 abort 不得改变状态"
    );

    // --- 负例：non-empty body（未知键）→ 400。
    let non_empty = cbor_body(vec![(0, wire::uint(1))]);
    let sig = fx.sign_management(
        relay::proto::HttpMethod::Post,
        &abort_path(&fx, &rotation),
        request_id,
        control_rev,
        &non_empty,
    );
    let fields = vec![
        (0, Fixture::signed_wrapper(non_empty, sig)),
        (1, wire::bytes(request_id)),
        (2, wire::uint(control_rev)),
    ];
    let (status, resp) = call(
        &app,
        post_cbor(&abort_path(&fx, &rotation), Some(&token), fields),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{}", error_code(&resp));

    // --- 正例：接受；同 request_id + 同 body 重放 → 字节一致的原响应。
    let (status, first) = call(
        &app,
        post_cbor(&abort_path(&fx, &rotation), Some(&token), outer.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&first));
    assert_eq!(
        fx.rotation_view(&rotation).rotation.state,
        relay::rotations::RotationState::Aborted
    );
    let (status, replay) = call(
        &app,
        post_cbor(&abort_path(&fx, &rotation), Some(&token), outer),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(replay, first, "replay 必须逐字节回放原响应");

    // §35.3：abort 控制记录必须携带**完整可重建**的管理签名输入事实。
    // relay 以前只存 canonical body（空 map）+ 签名，客户端无法重建 path 里的 rotation id。
    let signed_record: Vec<u8> = fx
        .db
        .read(|conn| {
            Ok(conn.query_row(
                "SELECT signed_record FROM control_commits ORDER BY rev DESC LIMIT 1",
                [],
                |row| row.get(0),
            )?)
        })
        .expect("最新控制记录");
    let record =
        relay::control::SignedControlRecord::decode(&signed_record).expect("decode control");
    assert_eq!(record.kind, relay::control::ControlKind::RotationAbort);
    assert_eq!(record.signed_body, cbor_body(Vec::new()));
    let event_bytes = record.event.as_deref().expect("abort 必须带 typed 事件");
    let event = relay::control::RotationAborted::decode(event_bytes).expect("decode abort event");
    assert_eq!(event.rotation_id, rotation);
    assert_eq!(event.method, relay::proto::HttpMethod::Post);
    assert_eq!(event.path, abort_path(&fx, &rotation));
    assert_eq!(event.body_digest, sha256(&record.signed_body));
    // 用事件里的 method/path + 已签 body 重建管理签名输入，必须能验过真实管理 key。
    let body_digest = sha256(
        &relay::proto::management_request_cde(event.method, &event.path, &record.signed_body)
            .expect("request cde"),
    );
    let input = relay::proto::management_signing_input(
        &fx.vault().instance_id,
        &fx.vault_id,
        &record.request_id,
        record.expected_control_rev,
        &body_digest,
    )
    .expect("management signing input");
    fx.manager_signing_key
        .as_ref()
        .expect("manager key")
        .verifying_key()
        .verify_strict(
            &input,
            &ed25519_dalek::Signature::from_bytes(
                &record
                    .signature
                    .as_slice()
                    .try_into()
                    .expect("64B signature"),
            ),
        )
        .expect("abort 事件必须能重建被签输入并验签");

    // `/control` 必须把同一条 typed 记录**原样**返回（客户端不查 relay 库）。
    let control_uri = format!("/v1/vaults/{}/control?after_rev=0", b64(&fx.vault_id));
    let (status, control_body) = call(&app, get(&control_uri, &token)).await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&control_body));
    let control = response_fields(&control_body);
    let entries = wire::array_field(&control, 0).expect("control entries");
    let abort_entry = entries
        .iter()
        .map(|entry| wire::value_map(entry).expect("entry"))
        .find(|entry| {
            wire::opt_field(entry, 4).is_some_and(|kind| *kind == wire::text("rotation.abort"))
        })
        .expect("rotation.abort 控制记录");
    assert_eq!(
        wire::bytes_field(&abort_entry, 3).expect("signed_record"),
        signed_record,
        "/control 必须原样返回带 typed 事件的 abort 记录"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invite_creation_rejects_body_tamper_and_request_id_reuse() {
    let fx = block(|| Fixture::with_real_manager("mgmt-invite"));
    let app = app_ed25519(&fx);
    let token = block(|| fx.manager_token());
    let control_rev = fx.vault().control_rev;
    let request_id = [0x31u8; 16];
    let token_raw = [0x77u8; 32];
    let token_hash: Hash32 = sha256(&token_raw);
    let body = cbor_body(vec![(0, wire::bytes(token_hash))]);
    let signature = fx.sign_management(
        relay::proto::HttpMethod::Post,
        &invites_path(&fx),
        request_id,
        control_rev,
        &body,
    );
    let outer = vec![
        (0, Fixture::signed_wrapper(body.clone(), signature.clone())),
        (1, wire::bytes(request_id)),
        (2, wire::uint(control_rev)),
    ];

    // --- 负例：签名体内 token_hash 被改（外层不变）→ 镜像一致但验签失败 403。
    let tampered = cbor_body(vec![(0, wire::bytes(sha256(b"other")))]);
    let fields = vec![
        (0, Fixture::signed_wrapper(tampered, signature.clone())),
        (1, wire::bytes(request_id)),
        (2, wire::uint(control_rev)),
    ];
    let (status, resp) = call(&app, post_cbor(&invites_path(&fx), Some(&token), fields)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{}", error_code(&resp));
    assert_eq!(error_code(&resp), "MANAGEMENT_REQUIRED");
    assert_eq!(invite_count(&fx), 0);

    // --- 正例：201 + 幂等重放同一 invite。
    let (status, first) = call(
        &app,
        post_cbor(&invites_path(&fx), Some(&token), outer.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{}", error_code(&first));
    assert_eq!(invite_count(&fx), 1);
    let invite = invite_id_of(&first);
    let (status, replay) = call(&app, post_cbor(&invites_path(&fx), Some(&token), outer)).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(replay, first);
    assert_eq!(invite_id_of(&replay), invite);
    assert_eq!(invite_count(&fx), 1, "replay 不得新建邀请");

    // --- 负例：同 request_id 不同 body（签名有效）→ 409。
    let other = cbor_body(vec![(0, wire::bytes(sha256(b"third")))]);
    let sig = fx.sign_management(
        relay::proto::HttpMethod::Post,
        &invites_path(&fx),
        request_id,
        control_rev,
        &other,
    );
    let fields = vec![
        (0, Fixture::signed_wrapper(other, sig)),
        (1, wire::bytes(request_id)),
        (2, wire::uint(control_rev)),
    ];
    let (status, resp) = call(&app, post_cbor(&invites_path(&fx), Some(&token), fields)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{}", error_code(&resp));
    assert_eq!(error_code(&resp), "ID_CONFLICT");
    assert_eq!(invite_count(&fx), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn password_activation_binds_outer_cas_and_slot_in_signed_body() {
    let fx = block(|| Fixture::with_real_manager("mgmt-slot"));
    let app = app_ed25519(&fx);
    let device_token = block(|| fx.device_token());
    let slot_id = block(|| fx.put_staging_slot(relay::slots::SlotKind::Password));
    let expected_rev = 0u64;
    let body = cbor_body(vec![
        (0, wire::uint(expected_rev)),
        (1, wire::bytes(slot_id)),
    ]);
    // 设备私钥对 canonical body 签名（§26：CAS/槽指针必须进被签体）。
    let signature = fx.device_signing_key.sign(&body).to_bytes().to_vec();
    let outer = vec![
        (0, wire::uint(expected_rev)),
        (1, wire::bytes(slot_id)),
        (2, Fixture::signed_wrapper(body.clone(), signature.clone())),
    ];
    let uri = slot_activate_path(&fx, "password");

    // --- 负例：外层 new_slot_id 被替换、签名不变 → 403（镜像不一致）。
    let mut bad = outer.clone();
    bad[1].1 = wire::bytes([0x99u8; 32]);
    let (status, resp) = call(&app, post_cbor(&uri, Some(&device_token), bad)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{}", error_code(&resp));
    assert_eq!(
        fx.db.read(relay::slots::active_password_rev).expect("rev"),
        0,
        "被拒绝的请求不得切换 active 槽"
    );

    // --- 负例：签名体内 CAS 被改、外层镜像跟着改 → 设备验签失败 400。
    let tampered = cbor_body(vec![
        (0, wire::uint(expected_rev + 1)),
        (1, wire::bytes(slot_id)),
    ]);
    let fields = vec![
        (0, wire::uint(expected_rev + 1)),
        (1, wire::bytes(slot_id)),
        (2, Fixture::signed_wrapper(tampered, signature)),
    ];
    let (status, resp) = call(&app, post_cbor(&uri, Some(&device_token), fields)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{}", error_code(&resp));
    assert_eq!(error_code(&resp), "SIGNATURE_INVALID");

    // --- 正例：接受并切换 active 槽。
    let (status, resp) = call(&app, post_cbor(&uri, Some(&device_token), outer)).await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&resp));
    assert_eq!(
        wire::uint_field(&response_fields(&resp), 0).expect("rev"),
        1
    );
    assert_eq!(
        fx.db.read(relay::slots::active_password_rev).expect("rev"),
        1
    );
}

/// grant staging 的 canonical 字段就是请求 map；签名输入由这些字段重新构造
/// `GrantAadV1 || enc || SHA256(ciphertext)`，任一所消费字段被替换都验签失败。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grant_staging_binds_every_consumed_field() {
    let fx = block(|| Fixture::with_real_manager("mgmt-grant"));
    let app = app_ed25519(&fx);
    let rotation = block(|| fx.begin_rotation(&[]).rotation.rotation_id);
    let token = block(|| fx.manager_token());
    let vault = fx.vault();
    let manager = fx.manager_signing_key.as_ref().expect("manager key");
    let grant_id = [0x44u8; 16];
    let hpke_enc = [0x45u8; 32];
    let hpke_ciphertext = vec![0x55u8; 64];
    let ciphertext_sha256: Hash32 = sha256(&hpke_ciphertext);
    let aad = relay::proto::GrantAadV1 {
        pv: 1,
        vault_id: fx.vault_id,
        epoch: 2,
        recipient_device_id: fx.device,
        recipient_x25519_pub: [4u8; 32],
        manager_rev: vault.manager_rev,
        grant_id,
    };
    let signed_header = relay::proto::grant_signed_bytes(&aad, &hpke_enc, &ciphertext_sha256)
        .expect("signed header");
    let signature = manager.sign(&signed_header).to_bytes().to_vec();
    let fields = vec![
        (0, wire::bytes(grant_id)),
        (1, wire::bytes(hpke_enc)),
        (2, wire::bytes(hpke_ciphertext.clone())),
        (3, wire::bytes(signed_header.clone())),
        (4, wire::bytes(signature.clone())),
        (5, wire::uint(vault.manager_rev)),
    ];
    let uri = grant_path(&fx, &rotation, &fx.device);

    // --- 负例：替换 hpke_ciphertext（签名不变）→ 验签失败 403。
    let mut bad = fields.clone();
    bad[2].1 = wire::bytes(vec![0x66u8; 64]);
    let (status, resp) = call(&app, put_cbor(&uri, &token, bad)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{}", error_code(&resp));
    assert_eq!(error_code(&resp), "MANAGEMENT_REQUIRED");
    assert_eq!(
        fx.db
            .read(|conn| Ok(
                conn.query_row("SELECT COUNT(*) FROM device_grants", [], |row| {
                    row.get::<_, i64>(0)
                })?
            ))
            .expect("grants"),
        0,
        "被拒绝的 grant 不得入库"
    );

    // --- 负例：替换 signed_header（relay 会重建待签字节，必须完全一致）→ 403。
    let mut bad = fields.clone();
    bad[3].1 = wire::bytes(b"forged-aad".to_vec());
    let (status, resp) = call(&app, put_cbor(&uri, &token, bad)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{}", error_code(&resp));

    // --- 正例：接受并 stage。
    let (status, resp) = call(&app, put_cbor(&uri, &token, fields)).await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&resp));
    assert_eq!(
        fx.db
            .read(|conn| Ok(
                conn.query_row("SELECT COUNT(*) FROM device_grants", [], |row| {
                    row.get::<_, i64>(0)
                })?
            ))
            .expect("grants"),
        1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn approve_device_binds_certificate_grant_and_request_body() {
    let fx = block(|| Fixture::with_real_manager("mgmt-approve"));
    let app = app_ed25519(&fx);
    // ---- 准备：管理 token、邀请、待批准设备（真实设备密钥）。
    let token = block(|| fx.manager_token());
    let control_rev = fx.vault().control_rev;
    let token_raw = [0x72u8; 32];
    let token_hash: Hash32 = sha256(&token_raw);
    let invite_body = cbor_body(vec![(0, wire::bytes(token_hash))]);
    let invite_request_id = [0x43u8; 16];
    let invite_sig = fx.sign_management(
        relay::proto::HttpMethod::Post,
        &invites_path(&fx),
        invite_request_id,
        control_rev,
        &invite_body,
    );
    let fields = vec![
        (0, Fixture::signed_wrapper(invite_body.clone(), invite_sig)),
        (1, wire::bytes(invite_request_id)),
        (2, wire::uint(control_rev)),
    ];
    let (status, resp) = call(&app, post_cbor(&invites_path(&fx), Some(&token), fields)).await;
    assert_eq!(status, StatusCode::CREATED, "{}", error_code(&resp));
    let invite = invite_id_of(&resp);

    let device = [0x42u8; 16];
    let device_key = SigningKey::from_bytes(&[0x52u8; 32]);
    let sign_pk = device_key.verifying_key().to_bytes();
    let hpke_pk = [0x62u8; 32];
    let proof = [0x33u8; 32];
    let proof_signature = device_key.sign(&proof).to_bytes().to_vec();
    let redeem_fields = vec![
        (0, wire::bytes(token_raw)),
        (1, wire::bytes(device)),
        (2, wire::bytes(sign_pk)),
        (3, wire::bytes(hpke_pk)),
        (4, wire::bytes(proof)),
        (5, wire::bytes(proof_signature)),
    ];
    let (status, resp) = call(
        &app,
        post_cbor(
            &format!(
                "/v1/vaults/{}/invites/{}/redeem",
                b64(&fx.vault_id),
                b64(&invite)
            ),
            None,
            redeem_fields,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{}", error_code(&resp));

    // ---- 批准材料：证书（管理签）+ grant（管理签）。
    let manager = fx.manager_signing_key.as_ref().expect("manager key");
    let vault = fx.vault();
    let cert = relay::verify::new_certificate(&fx.vault_id, &device, &sign_pk, &hpke_pk, 1);
    let certificate = cert.encode().expect("cert cde");
    let certificate_signature = manager
        .sign(&relay::proto::device_cert_signed_bytes(&cert).expect("cert signed bytes"))
        .to_bytes()
        .to_vec();
    let grant_id = [0x44u8; 16];
    let hpke_enc = [0x45u8; 32];
    let hpke_ciphertext = vec![0x55u8; 64];
    let ciphertext_sha256: Hash32 = sha256(&hpke_ciphertext);
    let aad = relay::proto::GrantAadV1 {
        pv: 1,
        vault_id: fx.vault_id,
        epoch: vault.active_epoch,
        recipient_device_id: device,
        recipient_x25519_pub: hpke_pk,
        manager_rev: vault.manager_rev,
        grant_id,
    };
    let signed_header = relay::proto::grant_signed_bytes(&aad, &hpke_enc, &ciphertext_sha256)
        .expect("grant signed bytes");
    let grant_signature = manager.sign(&signed_header).to_bytes().to_vec();
    let control_rev = vault.control_rev;
    let request_id = [0x46u8; 16];
    let body = cbor_body(vec![
        (0, wire::bytes(certificate.clone())),
        (1, wire::bytes(certificate_signature.clone())),
        (2, wire::bytes(grant_id)),
        (3, wire::bytes(hpke_enc)),
        (4, wire::bytes(hpke_ciphertext.clone())),
        (5, wire::bytes(grant_signature.clone())),
        (6, wire::bytes(request_id)),
        (7, wire::uint(control_rev)),
    ]);
    let signature = fx.sign_management(
        relay::proto::HttpMethod::Post,
        &approve_path(&fx, &device),
        request_id,
        control_rev,
        &body,
    );
    let outer = vec![
        (0, Fixture::signed_wrapper(body.clone(), signature.clone())),
        (1, wire::bytes(request_id)),
        (2, wire::uint(control_rev)),
        (3, wire::bytes(certificate.clone())),
        (4, wire::bytes(certificate_signature.clone())),
        (5, wire::bytes(grant_id)),
        (6, wire::bytes(hpke_enc)),
        (7, wire::bytes(hpke_ciphertext.clone())),
        (8, wire::bytes(grant_signature.clone())),
    ];

    // --- 负例：外层 certificate 被替换（body 不变、签名不变）→ 403。
    let mut bad = outer.clone();
    bad[3].1 = wire::bytes(b"other-certificate");
    let (status, resp) = call(
        &app,
        post_cbor(&approve_path(&fx, &device), Some(&token), bad),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{}", error_code(&resp));
    assert_eq!(
        fx.db
            .read(relay::pairing::pending_devices)
            .expect("pending"),
        vec![device],
        "被拒绝的 approve 不得激活设备"
    );

    // --- 负例：签名体内 grant_id 被改（外层镜像跟着改）→ 管理验签失败 403。
    let tampered = cbor_body(vec![
        (0, wire::bytes(certificate.clone())),
        (1, wire::bytes(certificate_signature.clone())),
        (2, wire::bytes([0x99u8; 16])),
        (3, wire::bytes(hpke_enc)),
        (4, wire::bytes(hpke_ciphertext.clone())),
        (5, wire::bytes(grant_signature.clone())),
        (6, wire::bytes(request_id)),
        (7, wire::uint(control_rev)),
    ]);
    let mut fields = outer.clone();
    fields[0].1 = Fixture::signed_wrapper(tampered, signature);
    fields[5].1 = wire::bytes([0x99u8; 16]);
    let (status, resp) = call(
        &app,
        post_cbor(&approve_path(&fx, &device), Some(&token), fields),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{}", error_code(&resp));
    assert_eq!(error_code(&resp), "MANAGEMENT_REQUIRED");

    // --- 正例：接受并激活设备。
    let (status, first) = call(
        &app,
        post_cbor(&approve_path(&fx, &device), Some(&token), outer.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&first));
    assert_eq!(
        wire::id16_field(&response_fields(&first), 0).expect("device"),
        device
    );
    assert!(block(|| fx.db.read(relay::pairing::pending_devices))
        .expect("pending")
        .is_empty());

    // --- 幂等重放：同 request_id + 同 body → 原响应字节。
    let (status, replay) = call(
        &app,
        post_cbor(&approve_path(&fx, &device), Some(&token), outer),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(replay, first, "replay 必须逐字节回放原响应");

    // --- 修复回归：approve 必须把 grant 签名（与 signed_header/grant_id）落库，
    // `GET devices/self/grants/latest` 返回同一份 signature64，且可用管理公钥验签。
    let device_token = block(|| fx.device_token_for(&device));
    let (status, latest) = call(
        &app,
        get(
            &format!(
                "/v1/vaults/{}/devices/self/grants/latest",
                b64(&fx.vault_id)
            ),
            &device_token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&latest));
    let latest = response_fields(&latest);
    assert_eq!(
        wire::uint_field(&latest, 0).expect("epoch"),
        vault.active_epoch
    );
    assert_eq!(
        wire::bytes_field(&latest, 6).expect("signature64"),
        grant_signature,
        "latest grant 必须返回 approve 请求里的同一份签名"
    );
    assert_eq!(
        wire::id16_field(&latest, 7).expect("grant_id"),
        grant_id,
        "latest grant 必须返回 grant_id"
    );
    // leaf manifest（signed_header）就是被签名输入：用管理公钥验签。
    let leaf = wire::bytes_field(&latest, 5).expect("signed header");
    assert_eq!(leaf, signed_header);
    let manager_pub = fx
        .manager_signing_key
        .as_ref()
        .expect("manager key")
        .verifying_key();
    manager_pub
        .verify_strict(&leaf, &to_signature(&grant_signature))
        .expect("latest grant 的签名必须能被管理公钥验证");
    // DB 里也确实持久化了签名（不是响应侧临时拼的）。
    let epoch = i64::try_from(vault.active_epoch).expect("epoch");
    let stored: Option<Vec<u8>> = block(|| {
        fx.db.read(move |conn| {
            let row: Option<Option<Vec<u8>>> = conn
                .query_row(
                    "SELECT signature FROM device_grants WHERE epoch = ?1 AND device_id = ?2",
                    (epoch, device.as_slice()),
                    |row| row.get::<_, Option<Vec<u8>>>(0),
                )
                .optional()?;
            Ok(row.flatten())
        })
    })
    .expect("db grant signature");
    assert_eq!(stored.as_deref(), Some(grant_signature.as_slice()));
}

fn to_signature(bytes: &[u8]) -> ed25519_dalek::Signature {
    let array: [u8; 64] = bytes.try_into().expect("signature len");
    ed25519_dalek::Signature::from_bytes(&array)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn activate_rotation_consumes_manager_transition_only_from_signed_body() {
    let fx = block(|| Fixture::with_real_manager("mgmt-activate"));
    let app = app_ed25519(&fx);
    // ---- 准备：freeze + 两份候选基线 + grant + 新槽（fixture 的 TestHook 只用于准备）。
    let rotation = block(|| fx.begin_rotation(&[]).rotation.rotation_id);
    let row = block(|| fx.rotation_view(&rotation).rotation);
    let (primary, fallback) = block(|| fx.publish_candidate_baselines(row.to_epoch));
    block(|| fx.stage_grant(&rotation, &fx.device));
    let reservation = block(|| fx.rotation_reservation(&rotation));
    block(|| fx.stage_candidate_slots(row.to_epoch, &reservation));

    let old_manager = fx.manager_signing_key.as_ref().expect("manager key");
    let new_manager = SigningKey::from_bytes(&[0x77u8; 32]);
    let new_pub = new_manager.verifying_key().to_bytes();
    let old_rev = fx.vault().manager_rev;
    let transition = block(|| fx.manager_transition(&row, old_rev, old_rev + 1, new_pub));
    let decoded_transition =
        relay::rotations::ManagerTransition::decode(&transition).expect("decode transition");
    // §34：complete activation T（与 DB 事实一致）由旧/新管理 key 双签。
    let activation_t = block(|| {
        fx.db.read(|conn| {
            let vault = relay::vault::load(conn)?;
            support::activation_t_for(conn, &vault, &row, &decoded_transition, &primary, &fallback)
        })
    })
    .expect("activation T");
    let activation_t_bytes = activation_t.encode().expect("T encode");
    let activation_t_input =
        relay::proto::activation_t_signing_input(&activation_t).expect("T signing input");
    let old_signature = old_manager.sign(&transition).to_bytes().to_vec();
    let new_signature = new_manager.sign(&transition).to_bytes().to_vec();
    let old_t_signature = old_manager.sign(&activation_t_input).to_bytes().to_vec();
    let new_t_signature = new_manager.sign(&activation_t_input).to_bytes().to_vec();
    let manager_map = wire::map(vec![
        (0, wire::bytes(transition.clone())),
        (1, wire::bytes(old_signature.clone())),
        (2, wire::bytes(new_signature.clone())),
        (3, wire::bytes(activation_t_bytes)),
        (4, wire::bytes(old_t_signature.clone())),
        (5, wire::bytes(new_t_signature.clone())),
    ]);
    // §28.1 条目 6：activate body 还必须带整套材料摘要（expected CAS）。
    let (member_digest, grants_digest, slot_digest, asset_digest) = block(|| {
        fx.db.read(|conn| {
            let to_epoch = row.to_epoch;
            Ok((
                relay::rotations::member_cert_digest(conn)?,
                relay::rotations::grants_digest(conn, to_epoch)?.1,
                relay::rotations::slot_set_digest(conn, to_epoch)?,
                relay::rotations::get(conn, &rotation)?.asset_set_digest,
            ))
        })
    })
    .expect("activate digests");
    let body = cbor_body(vec![
        (0, manager_map.clone()),
        (1, wire::bytes(primary)),
        (2, wire::bytes(fallback)),
        (3, wire::bytes(member_digest)),
        (4, wire::bytes(grants_digest)),
        (5, wire::bytes(slot_digest)),
        (6, wire::bytes(asset_digest.expect("begin 冻结资产摘要"))),
    ]);
    let token = block(|| fx.manager_token());
    let control_rev = fx.vault().control_rev;
    let request_id = [0x51u8; 16];
    let signature = fx.sign_management(
        relay::proto::HttpMethod::Post,
        &activate_path(&fx, &rotation),
        request_id,
        control_rev,
        &body,
    );
    let outer = vec![
        (0, Fixture::signed_wrapper(body.clone(), signature.clone())),
        (1, wire::bytes(request_id)),
        (2, wire::uint(control_rev)),
        (3, manager_map.clone()),
    ];
    let uri = activate_path(&fx, &rotation);

    // --- 负例：外层 new_manager 被替换（body 不变、签名不变）→ 403。
    let other_transition =
        block(|| fx.manager_transition(&row, old_rev, old_rev + 1, [0x88u8; 32]));
    let mut bad = outer.clone();
    bad[3].1 = wire::map(vec![
        (0, wire::bytes(other_transition.clone())),
        (1, wire::bytes(old_signature.clone())),
        (2, wire::bytes(new_signature.clone())),
    ]);
    let (status, resp) = call(&app, post_cbor(&uri, Some(&token), bad)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{}", error_code(&resp));
    assert_eq!(fx.vault().active_epoch, 1, "被拒绝的 activate 不得切 epoch");

    // --- 负例：签名体内 transition 被改（外层镜像跟着改）→ 管理验签失败 403。
    let tampered = cbor_body(vec![
        (
            0,
            wire::map(vec![
                (0, wire::bytes(other_transition.clone())),
                (1, wire::bytes(old_signature.clone())),
                (2, wire::bytes(new_signature.clone())),
            ]),
        ),
        (1, wire::bytes(primary)),
        (2, wire::bytes(fallback)),
    ]);
    let mut fields = outer.clone();
    fields[0].1 = Fixture::signed_wrapper(tampered, signature);
    fields[3].1 = wire::map(vec![
        (0, wire::bytes(other_transition.clone())),
        (1, wire::bytes(old_signature.clone())),
        (2, wire::bytes(new_signature.clone())),
    ]);
    let (status, resp) = call(&app, post_cbor(&uri, Some(&token), fields)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{}", error_code(&resp));
    assert_eq!(error_code(&resp), "MANAGEMENT_REQUIRED");

    // --- 正例：两份一致 → 激活；响应含激活 commit 信息。
    let (status, resp) = call(&app, post_cbor(&uri, Some(&token), outer)).await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&resp));
    let fields = response_fields(&resp);
    assert_eq!(wire::uint_field(&fields, 0).expect("epoch"), row.to_epoch);
    let vault = fx.vault();
    assert_eq!(vault.active_epoch, row.to_epoch);
    assert_eq!(vault.manager_rev, old_rev + 1);
    // 管理公钥已切换为新 key。
    assert_eq!(
        block(|| fx.db.read(relay::vault::manager_pubkey)).expect("manager pub"),
        Some(new_pub)
    );
    // 同 request_id 的结果可从幂等表找回（激活推进 auth_rev/manager_rev 后必须用查询，
    // 而不是重放旧签名 —— GET rotations/current 在 task-101 提供）。
    let stored = block(|| {
        fx.db
            .read(|conn| relay::management::lookup(conn, "management/activate", &request_id))
    })
    .expect("lookup")
    .expect("activate 的结果必须可幂等查回");
    assert_eq!(stored.1, resp, "缓存的响应体必须与原响应逐字节一致");
}
