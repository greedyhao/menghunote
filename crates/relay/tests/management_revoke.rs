//! ADR 0010 切片 1a：管理端**独立吊销**端点
//! `POST /v1/vaults/{vault}/devices/{device}/revoke` 的验收用例。
//!
//! 覆盖（ADR 0010「必须验收的测试」1–5 中属于 relay 的部分）：
//! 1. 真 HTTP 两设备：吊销 B 后 B 的旧 token 立即 REVOKED，A/manager 可重认证，
//!    B 的新 challenge 与 push/pull 全部被拒；
//! 2. `/control` 保留 B 的历史证书但 ACTIVE 集合排除它；`control_rev` 不变、`auth_rev` 变化；
//!    后续 grant/轮换规划（`member_set_hash` / staging 门禁）不含 B；
//! 3. 响应丢失重放、重复请求、跨 target 同 id 冲突、控制链前进后的成功回放、
//!    `auth_rev` 仅首次真实变化推进；
//! 4. 唯一 ACTIVE / pending / initializing / rotating / 缺目标 / 两台并发互删，
//!    且失败**无副作用**（status/版本/控制链/dedup 逐项断言）；
//! 5. 错管理签名（即使命中已成功的 id）也拒绝、target/path/外层镜像篡改、
//!    过期 CAS 拒绝。
//!
//! 全部管理签名走**真实 Ed25519**（`Fixture::with_real_manager` + `Ed25519Hook`）：
//! 弱 `TestHook` 无法证明"签名绑定 path/canonical body"这类安全断言。

mod support;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use ed25519_dalek::{Signer, SigningKey};
use http_body_util::BodyExt;
use relay::api::{router, AppState};
use relay::ids::{sha256, Id16};
use relay::rotations::RotationState;
use relay::verify::Ed25519Hook;
use relay::wire;
use support::*;
use tower::ServiceExt;

/// 管理 realm 的 challenge 标签（`create_challenge` 的字段 0）。
const REALM_MANAGEMENT: u64 = 2;
/// 设备 realm 的 challenge 标签。
const REALM_DEVICE: u64 = 1;

fn block<T>(f: impl FnOnce() -> T) -> T {
    tokio::task::block_in_place(f)
}

/// 真实管理私钥 + 密码学验签的 router（管理写/设备签名都走真实 Ed25519）。
fn setup(label: &str) -> (Fixture, axum::Router) {
    // fixture 初始化会走 `Db::write_blocking`，必须在 runtime 的 blocking 段里做。
    let fx = block(|| Fixture::with_real_manager(label));
    let app = router(AppState::new(
        fx.db.clone(),
        fx.cfg.clone(),
        Arc::new(Ed25519Hook),
    ));
    (fx, app)
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

fn get(uri: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .expect("request")
}

fn b64(bytes: &[u8]) -> String {
    relay::ids::b64url_encode(bytes)
}

fn error_code(body: &[u8]) -> String {
    wire::text_field(&cbor_fields(body), 0).unwrap_or_default()
}

fn revoke_path(fx: &Fixture, target: &Id16) -> String {
    format!(
        "/v1/vaults/{}/devices/{}/revoke",
        b64(&fx.vault_id),
        b64(target)
    )
}

fn state_path(fx: &Fixture) -> String {
    format!("/v1/vaults/{}/state", b64(&fx.vault_id))
}

fn control_path(fx: &Fixture) -> String {
    format!("/v1/vaults/{}/control?after_rev=0", b64(&fx.vault_id))
}

/// 被签名的 canonical body：`CDE{0: target, 1: request_id, 2: expected_control_rev}`。
fn revoke_body(target: &Id16, request_id: &Id16, control_rev: u64) -> Vec<u8> {
    cbor_body(vec![
        (0, wire::bytes(*target)),
        (1, wire::bytes(*request_id)),
        (2, wire::uint(control_rev)),
    ])
}

/// 用真实管理私钥签名，并按 ADR 0010 的外层镜像布局组装请求体。
///
/// `signed_path` 是**签名覆盖**的 path（正常情形等于请求 path）；调用方可以刻意错开
/// 它来构造 path/body 镜像篡改的负例。
fn envelope(
    fx: &Fixture,
    signed_path: &str,
    body: Vec<u8>,
    request_id: Id16,
    control_rev: u64,
) -> wire::Fields {
    let signature = fx.sign_management(
        relay::proto::HttpMethod::Post,
        signed_path,
        request_id,
        control_rev,
        &body,
    );
    vec![
        (0, Fixture::signed_wrapper(body, signature)),
        (1, wire::bytes(request_id)),
        (2, wire::uint(control_rev)),
    ]
}

/// 正常请求：签名 path、path、body target 三者一致。
fn revoke_request(
    fx: &Fixture,
    target: &Id16,
    request_id: Id16,
    control_rev: u64,
) -> (String, wire::Fields) {
    let path = revoke_path(fx, target);
    let body = revoke_body(target, &request_id, control_rev);
    let fields = envelope(fx, &path, body, request_id, control_rev);
    (path, fields)
}

/// 用**另一把**私钥签名（错管理签名负例）。
fn envelope_bad_key(
    fx: &Fixture,
    signed_path: &str,
    body: Vec<u8>,
    request_id: Id16,
    control_rev: u64,
) -> wire::Fields {
    let impostor = SigningKey::from_bytes(&[0x99u8; 32]);
    let vault = fx.vault();
    let request_cde =
        relay::proto::management_request_cde(relay::proto::HttpMethod::Post, signed_path, &body)
            .expect("request cde");
    let body_digest = sha256(&request_cde);
    let input = relay::proto::management_signing_input(
        &vault.instance_id,
        &fx.vault_id,
        &request_id,
        control_rev,
        &body_digest,
    )
    .expect("management signing input");
    let signature = impostor.sign(&input).to_bytes().to_vec();
    vec![
        (0, Fixture::signed_wrapper(body, signature)),
        (1, wire::bytes(request_id)),
        (2, wire::uint(control_rev)),
    ]
}

/// 响应体：`{0: target, 1: "revoked", 2: changed, 3: auth_rev, 4: control_rev}`。
#[derive(Debug, Clone, PartialEq, Eq)]
struct RevokeResponse {
    target: Id16,
    status: String,
    changed: bool,
    auth_rev: u64,
    control_rev: u64,
}

fn revoke_response(body: &[u8]) -> RevokeResponse {
    let fields = cbor_fields(body);
    RevokeResponse {
        target: wire::id16_field(&fields, 0).expect("target"),
        status: wire::text_field(&fields, 1).expect("status"),
        changed: wire::bool_field(&fields, 2).expect("changed"),
        auth_rev: wire::uint_field(&fields, 3).expect("auth_rev"),
        control_rev: wire::uint_field(&fields, 4).expect("control_rev"),
    }
}

/// 真实 HTTP challenge → token 流程（真 Ed25519 签名，`origin_sha256` 不参与 relay 校验）。
async fn http_auth(app: &axum::Router, realm: u64, subject: Id16, key: &SigningKey) -> String {
    let (status, body) = call(
        app,
        post_cbor(
            "/v1/auth/challenges",
            None,
            vec![(0, wire::uint(realm)), (1, wire::bytes(subject))],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "challenge: {}", error_code(&body));
    let fields = cbor_fields(&body);
    let challenge_id = wire::id16_field(&fields, 0).expect("challenge id");
    let nonce: [u8; 32] = wire::bytes_field(&fields, 3)
        .expect("nonce")
        .as_slice()
        .try_into()
        .expect("32B nonce");
    let expires_at = wire::uint_field(&fields, 4).expect("expires_at");
    let vault_id = wire::id16_field(&fields, 5).expect("vault_id");
    let instance_id = wire::id16_field(&fields, 6).expect("instance_id");
    let message = relay::proto::Challenge {
        pv: 1,
        relay_instance_id: instance_id,
        origin_sha256: [0u8; 32],
        vault_id,
        realm: if realm == REALM_DEVICE {
            relay::proto::AuthRealm::Device
        } else {
            relay::proto::AuthRealm::Management
        },
        subject,
        challenge_id,
        nonce,
        expires_at,
    };
    let signed = relay::proto::auth_signing_input(&message).expect("auth signing input");
    let signature = key.sign(&signed).to_bytes().to_vec();
    let (status, body) = call(
        app,
        post_cbor(
            "/v1/auth/tokens",
            None,
            vec![
                (0, wire::bytes(challenge_id)),
                (1, wire::bytes(message.encode().expect("challenge cde"))),
                (2, wire::bytes(signature)),
            ],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "token: {}", error_code(&body));
    wire::text_field(&cbor_fields(&body), 0).expect("token")
}

/// `/control` 的成员条目（`{0 device_id, 1 certificate, 2 status, 3 sign_pk, 4 hpke_pk, 5 sig?}`）。
fn control_members(body: &[u8]) -> Vec<wire::Fields> {
    let fields = cbor_fields(body);
    wire::array_field(&fields, 3)
        .expect("members")
        .iter()
        .map(|value| wire::value_map(value).expect("member map"))
        .collect()
}

fn member_of(members: &[wire::Fields], device: &Id16) -> wire::Fields {
    members
        .iter()
        .find(|member| wire::id16_field(member, 0).expect("member id") == *device)
        .cloned()
        .expect("member must be listed")
}

fn active_member_ids(members: &[wire::Fields]) -> Vec<Id16> {
    members
        .iter()
        .filter(|member| wire::text_field(member, 2).is_ok_and(|status| status == "active"))
        .map(|member| wire::id16_field(member, 0).expect("member id"))
        .collect()
}

/// 管理端 dedup 记录（断言"失败不留写入/成功只写一次"）。
fn dedup_rows(fx: &Fixture, request_id: &Id16) -> usize {
    let id = *request_id;
    fx.db
        .read(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT COUNT(*) FROM dedup WHERE scope = 'management/device-revoke' AND id = ?1",
            )?;
            let count: i64 = stmt.query_row([id.as_slice()], |row| row.get(0))?;
            Ok(usize::try_from(count).expect("count"))
        })
        .expect("dedup rows")
}

// ---------------------------------------------------------------- 1. 立即断访问

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revoking_active_device_cuts_access_and_replays() {
    let (fx, app) = setup("revoke-access");
    let a = fx.device;
    let b: Id16 = [0x0Bu8; 16];
    let b_certificate = b"cert-B-0x0B".to_vec();
    block(|| fx.add_device_with(b, &b_certificate, "active"));

    let b_token = block(|| fx.device_token_for(&b));
    let manager_token = block(|| fx.manager_token());
    let before = fx.vault();
    let control_records = block(|| fx.control_count());
    let state_uri = state_path(&fx);

    // --- 正例：管理身份 R（管理 bearer + 当前管理密钥签名）吊销 B。
    let request_id = [0x11u8; 16];
    let (path, outer) = revoke_request(&fx, &b, request_id, before.control_rev);
    let (status, first) = call(&app, post_cbor(&path, Some(&manager_token), outer.clone())).await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&first));
    let response = revoke_response(&first);
    assert_eq!(
        response,
        RevokeResponse {
            target: b,
            status: "revoked".to_owned(),
            changed: true,
            auth_rev: before.auth_rev + 1,
            control_rev: before.control_rev,
        }
    );

    // --- DB 事实：只改 status/revoked_at，证书/公钥/历史全保留。
    let snapshot = block(|| fx.device_snapshot(&b)).expect("B row");
    assert_eq!(snapshot.status, "revoked");
    assert!(snapshot.revoked_at.is_some(), "revoked_at 必须落库");
    assert_eq!(snapshot.certificate, b_certificate, "证书必须保留");
    let after = fx.vault();
    assert_eq!(after.auth_rev, before.auth_rev + 1, "auth_rev 推进一次");
    assert_eq!(after.control_rev, before.control_rev, "control_rev 不变");
    assert_eq!(
        block(|| fx.control_count()),
        control_records,
        "不追加控制记录"
    );

    // --- B 的旧 token 立即失效：pull（/log）与 push（/updates）都 REVOKED。
    let log_uri = format!("/v1/vaults/{}/log?after=abcd&limit=4", b64(&fx.vault_id));
    let (status, body) = call(&app, get(&log_uri, &b_token)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{}", error_code(&body));
    assert_eq!(error_code(&body), "REVOKED");
    let updates_uri = format!("/v1/vaults/{}/updates", b64(&fx.vault_id));
    let (status, body) = call(
        &app,
        post_cbor(
            &updates_uri,
            Some(&b_token),
            vec![(0, wire::array(Vec::new()))],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{}", error_code(&body));
    assert_eq!(error_code(&body), "REVOKED");

    // --- B 的新 challenge 被拒（不区分未注册/已吊销，避免设备枚举）。
    let (status, body) = call(
        &app,
        post_cbor(
            "/v1/auth/challenges",
            None,
            vec![(0, wire::uint(REALM_DEVICE)), (1, wire::bytes(b))],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{}", error_code(&body));
    assert_eq!(error_code(&body), "REVOKED");

    // --- 调用者自己的旧 management bearer 也失效（auth_rev 是全 vault 的）。
    let (status, body) = call(&app, get(&state_uri, &manager_token)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{}", error_code(&body));
    assert_eq!(error_code(&body), "REVOKED");

    // --- A（设备）与 manager 都能重新认证（真实 challenge/Ed25519 签名）。
    let a_token = http_auth(&app, REALM_DEVICE, a, &fx.device_signing_key).await;
    let (status, body) = call(&app, get(&state_uri, &a_token)).await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&body));
    let manager_key = fx.manager_signing_key.as_ref().expect("manager key");
    let manager_subject = relay::auth::Realm::management_subject(&fx.manager_pub);
    let manager_token = http_auth(&app, REALM_MANAGEMENT, manager_subject, manager_key).await;
    assert!(!manager_token.is_empty());

    // --- 响应丢失重放：同 id + 同 B → 逐字节原响应，且**不再推进 auth_rev**。
    let (status, replay) = call(&app, post_cbor(&path, Some(&manager_token), outer)).await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&replay));
    assert_eq!(replay, first, "重放必须逐字节回放原响应");
    assert_eq!(
        fx.vault().auth_rev,
        before.auth_rev + 1,
        "重放不得再推进 auth_rev"
    );
    assert_eq!(dedup_rows(&fx, &request_id), 1, "同一 id 只有一条幂等记录");
}

// ---------------------------------------------------------------- 2. /control 与控制链

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn control_keeps_certificate_history_but_excludes_revoked_from_active_set() {
    let (fx, app) = setup("revoke-control");
    let a = fx.device;
    let b: Id16 = [0x0Bu8; 16];
    let b_certificate = b"cert-B-history".to_vec();
    block(|| fx.add_device_with(b, &b_certificate, "active"));

    let manager_token = block(|| fx.manager_token());
    let before = fx.vault();
    let control_records = block(|| fx.control_count());
    let request_id = [0x21u8; 16];
    let (path, outer) = revoke_request(&fx, &b, request_id, before.control_rev);
    let (status, body) = call(&app, post_cbor(&path, Some(&manager_token), outer)).await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&body));

    // `/control`：control_rev 不变、控制记录不增，但成员状态变了、证书还在。
    let token = block(|| fx.manager_token());
    let (status, control) = call(&app, get(&control_path(&fx), &token)).await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&control));
    let fields = cbor_fields(&control);
    assert_eq!(
        wire::uint_field(&fields, 1).expect("control_rev"),
        before.control_rev,
        "吊销不得推进 control_rev"
    );
    assert_eq!(
        wire::array_field(&fields, 0).expect("entries").len(),
        usize::try_from(control_records).expect("count"),
        "吊销不得追加控制记录"
    );
    let members = control_members(&control);
    let revoked_member = member_of(&members, &b);
    assert_eq!(
        wire::text_field(&revoked_member, 2).expect("status"),
        "revoked"
    );
    assert_eq!(
        wire::bytes_field(&revoked_member, 1).expect("certificate"),
        b_certificate,
        "历史证书必须保留（否则旧对象的 producer 校验会断）"
    );
    assert_eq!(
        active_member_ids(&members),
        vec![a],
        "ACTIVE 集合必须只剩 A"
    );

    // `/state`：control_rev 相同、auth_rev 已变（客户端缓存必须看 auth_rev）。
    let (status, state) = call(&app, get(&state_path(&fx), &token)).await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&state));
    let state = cbor_fields(&state);
    assert_eq!(
        wire::uint_field(&state, 5).expect("control_rev"),
        before.control_rev
    );
    assert_eq!(
        wire::uint_field(&state, 6).expect("auth_rev"),
        before.auth_rev + 1
    );

    // 后续规划集合（grant 计划/activate 门禁用同一个 ACTIVE 集合）不含 B：
    // `member_set_hash` = SHA256(排序后的 ACTIVE device_id)，且 staging 门禁只要求 A 的 grant。
    let member_hash = block(|| {
        fx.db
            .read(relay::rotations::member_set_hash)
            .expect("member_set_hash")
    });
    assert_eq!(member_hash, sha256(a.as_slice()), "规划集合只有 A");
    assert_eq!(block(|| fx.active_device_ids()), vec![a]);

    let rotation = block(|| fx.begin_rotation(&[]).rotation.rotation_id);
    let to_epoch = block(|| fx.rotation_view(&rotation).rotation.to_epoch);
    block(|| fx.publish_candidate_baselines(to_epoch));
    let reservation = block(|| fx.rotation_reservation(&rotation));
    block(|| fx.stage_candidate_slots(to_epoch, &reservation));
    block(|| fx.stage_grant(&rotation, &a));
    assert_eq!(
        block(|| fx.rotation_view(&rotation).rotation.state),
        RotationState::Staging,
        "B 已吊销：轮换不需要 B 的候选 grant"
    );

    // 被吊销设备不能再拿新 grant（显式轮换时它也不在收件人集合里）。
    let ctx = fx.manager_ctx();
    let hook = fx.hook.clone();
    let clock = fx.clock.clone();
    let manager_rev = fx.vault().manager_rev;
    let err = block(|| {
        fx.db.write_blocking(move |conn| {
            relay::rotations::stage_grant(
                conn,
                hook.as_ref(),
                &ctx,
                &rotation,
                &b,
                &relay::rotations::GrantInput {
                    grant_id: [0x31u8; 16],
                    hpke_enc: [0x32u8; 32],
                    hpke_ciphertext: b"ciphertext",
                    signed_header: b"aad||enc||hash",
                    signature: b"signature",
                    manager_rev,
                },
                clock.as_ref(),
            )
        })
    })
    .expect_err("revoked recipient");
    assert!(
        matches!(&err, relay::Error::Api(api) if api.code == relay::ErrorCode::Revoked),
        "{err}"
    );
}

// ---------------------------------------------------------------- 3. 幂等/冲突/回放

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_cross_target_conflict_and_replay_after_chain_advance() {
    let (fx, app) = setup("revoke-idempotency");
    let a = fx.device;
    let b: Id16 = [0x0Bu8; 16];
    block(|| fx.add_device_with(b, b"cert-B", "active"));

    let token = block(|| fx.manager_token());
    let before = fx.vault();
    let request_id = [0x41u8; 16];
    let (path, outer) = revoke_request(&fx, &b, request_id, before.control_rev);
    let (status, first) = call(&app, post_cbor(&path, Some(&token), outer.clone())).await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&first));
    let revoked_at = block(|| fx.device_snapshot(&b)).expect("B row").revoked_at;
    assert_eq!(fx.vault().auth_rev, before.auth_rev + 1);

    // ① 重复请求（新 request_id、target 已 revoked）→ changed=false 且不推进任何版本。
    let token = block(|| fx.manager_token());
    let repeat_id = [0x42u8; 16];
    let current = fx.vault();
    let (path2, repeat) = revoke_request(&fx, &b, repeat_id, current.control_rev);
    let (status, body) = call(&app, post_cbor(&path2, Some(&token), repeat)).await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&body));
    let response = revoke_response(&body);
    assert!(!response.changed, "重复吊销必须 changed=false");
    assert_eq!(response.auth_rev, current.auth_rev);
    assert_eq!(response.control_rev, current.control_rev);
    let now = fx.vault();
    assert_eq!(now.auth_rev, current.auth_rev, "重复请求不得推进 auth_rev");
    assert_eq!(now.control_rev, current.control_rev);
    assert_eq!(
        block(|| fx.device_snapshot(&b)).expect("B row").revoked_at,
        revoked_at,
        "重复请求不得改 revoked_at"
    );

    // ② 跨 target 复用同一 request_id → 409 ID_CONFLICT（幂等 scope 固定、target 在 body 内）。
    let (path_a, cross) = revoke_request(&fx, &a, request_id, current.control_rev);
    let (status, body) = call(&app, post_cbor(&path_a, Some(&token), cross)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{}", error_code(&body));
    assert_eq!(error_code(&body), "ID_CONFLICT");
    assert_eq!(
        block(|| fx.device_snapshot(&a)).expect("A row").status,
        "active",
        "冲突请求不得改动 A"
    );

    // ③ 控制链前进（begin 轮换推进 control_rev/auth_rev 并进入 rotating）后，
    //    同 id + 同 body 仍必须成功回放原响应（幂等查询先于 CAS/业务守卫）。
    let rotation = block(|| fx.begin_rotation(&[]).rotation.rotation_id);
    let advanced = fx.vault();
    assert_eq!(advanced.control_rev, current.control_rev + 1);
    let token = block(|| fx.manager_token());
    let (status, replay) = call(&app, post_cbor(&path, Some(&token), outer)).await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&replay));
    assert_eq!(replay, first, "控制链前进后仍必须逐字节回放");
    assert_eq!(
        fx.vault().auth_rev,
        advanced.auth_rev,
        "回放不推进 auth_rev"
    );
    assert_eq!(
        fx.vault().control_rev,
        advanced.control_rev,
        "回放不推进 control_rev"
    );
    assert_eq!(
        block(|| fx.rotation_view(&rotation).rotation.state),
        RotationState::Frozen,
        "回放不得改动轮换状态"
    );

    // ④ 验签先于回放：同一（已成功）id 但签名错误 → 403，绝不回放缓存响应。
    let bad = envelope_bad_key(
        &fx,
        &path,
        revoke_body(&b, &request_id, before.control_rev),
        request_id,
        before.control_rev,
    );
    let (status, body) = call(&app, post_cbor(&path, Some(&token), bad)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{}", error_code(&body));
    assert_eq!(error_code(&body), "MANAGEMENT_REQUIRED");

    // ⑤ 过期 CAS（新 id + 旧 control_rev）→ 409 CAS_CONFLICT，并携带当前 rev。
    let stale_id = [0x43u8; 16];
    let (stale_path, stale) = revoke_request(&fx, &b, stale_id, before.control_rev);
    let (status, body) = call(&app, post_cbor(&stale_path, Some(&token), stale)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{}", error_code(&body));
    assert_eq!(error_code(&body), "CAS_CONFLICT");
    assert_eq!(
        wire::opt_uint_field(&cbor_fields(&body), 3).expect("current rev"),
        Some(advanced.control_rev),
        "CAS_CONFLICT 必须携带当前 control_rev"
    );
    assert_eq!(dedup_rows(&fx, &stale_id), 0, "失败的 CAS 不得留幂等记录");
}

// ---------------------------------------------------------------- 4a. 守卫

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_pending_and_last_active_are_rejected_without_side_effects() {
    let (fx, app) = setup("revoke-guards");
    let a = fx.device;
    let pending: Id16 = [0x0Cu8; 16];
    block(|| fx.add_device_with(pending, &[], "pending"));
    let before = fx.vault();
    let control_records = block(|| fx.control_count());
    let token = block(|| fx.manager_token());

    // ① 目标不存在 → 404 NOT_FOUND。
    let missing: Id16 = [0xEEu8; 16];
    let request_id = [0x51u8; 16];
    let (path, outer) = revoke_request(&fx, &missing, request_id, before.control_rev);
    let (status, body) = call(&app, post_cbor(&path, Some(&token), outer)).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{}", error_code(&body));
    assert_eq!(error_code(&body), "NOT_FOUND");

    // ② pending 目标 → 409 INVALID_STATE（空证书占位不能直接置 revoked）。
    let request_id = [0x52u8; 16];
    let (path, outer) = revoke_request(&fx, &pending, request_id, before.control_rev);
    let (status, body) = call(&app, post_cbor(&path, Some(&token), outer)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{}", error_code(&body));
    assert_eq!(error_code(&body), "INVALID_STATE");
    assert_eq!(
        block(|| fx.device_snapshot(&pending))
            .expect("pending row")
            .status,
        "pending"
    );

    // ③ 唯一 ACTIVE 目标 → 409 INVALID_STATE（不能吊销到 0 台 ACTIVE）。
    let request_id = [0x53u8; 16];
    let (path, outer) = revoke_request(&fx, &a, request_id, before.control_rev);
    let (status, body) = call(&app, post_cbor(&path, Some(&token), outer)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{}", error_code(&body));
    assert_eq!(error_code(&body), "INVALID_STATE");

    // 失败无副作用：status/版本/控制链/dedup 逐项断言。
    assert_eq!(
        block(|| fx.device_snapshot(&a)).expect("A row").status,
        "active"
    );
    let after = fx.vault();
    assert_eq!(
        (after.auth_rev, after.control_rev),
        (before.auth_rev, before.control_rev)
    );
    assert_eq!(
        block(|| fx.control_count()),
        control_records,
        "不追加控制记录"
    );
    for id in [[0x51u8; 16], [0x52u8; 16], [0x53u8; 16]] {
        assert_eq!(dedup_rows(&fx, &id), 0, "失败请求不得写幂等记录");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn vault_state_guards_block_active_targets_only() {
    let (fx, app) = setup("revoke-vault-state");
    let b: Id16 = [0x0Bu8; 16];
    let revoked: Id16 = [0x0Du8; 16];
    block(|| {
        fx.add_device_with(b, b"cert-B", "active");
        fx.add_device_with(revoked, b"cert-D", "revoked");
    });

    // ① initializing → 409 INVALID_STATE（不能在没有可信状态时改访问权）。
    block(|| {
        fx.raw_write(|conn| {
            relay::vault::set_state(conn, relay::vault::VaultState::Initializing)?;
            Ok(())
        })
        .expect("set initializing")
    });
    let before = fx.vault();
    let token = block(|| fx.manager_token());
    let request_id = [0x61u8; 16];
    let (path, outer) = revoke_request(&fx, &b, request_id, before.control_rev);
    let (status, body) = call(&app, post_cbor(&path, Some(&token), outer)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{}", error_code(&body));
    assert_eq!(error_code(&body), "INVALID_STATE");
    assert_eq!(
        block(|| fx.device_snapshot(&b)).expect("B row").status,
        "active"
    );
    assert_eq!(fx.vault().auth_rev, before.auth_rev);
    block(|| {
        fx.raw_write(|conn| {
            relay::vault::set_state(conn, relay::vault::VaultState::Active)?;
            Ok(())
        })
        .expect("set active")
    });

    // ② rotating → 409 ROTATING（ACTIVE 目标的访问状态由轮换流程统一管）。
    let rotation = block(|| fx.begin_rotation(&[]).rotation.rotation_id);
    let rotating = fx.vault();
    assert_eq!(rotating.state, relay::vault::VaultState::Rotating);
    let token = block(|| fx.manager_token());
    let request_id = [0x62u8; 16];
    let (path, outer) = revoke_request(&fx, &b, request_id, rotating.control_rev);
    let (status, body) = call(&app, post_cbor(&path, Some(&token), outer)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{}", error_code(&body));
    assert_eq!(error_code(&body), "ROTATING");
    assert_eq!(
        block(|| fx.device_snapshot(&b)).expect("B row").status,
        "active"
    );
    let after = fx.vault();
    assert_eq!(
        (after.auth_rev, after.control_rev),
        (rotating.auth_rev, rotating.control_rev)
    );
    assert_eq!(dedup_rows(&fx, &[0x61u8; 16]), 0);
    assert_eq!(dedup_rows(&fx, &[0x62u8; 16]), 0);

    // ③ 已 revoked 的目标在轮换期间仍是幂等空操作（changed=false，不推进版本）。
    let token = block(|| fx.manager_token());
    let request_id = [0x63u8; 16];
    let current = fx.vault();
    let (path, outer) = revoke_request(&fx, &revoked, request_id, current.control_rev);
    let (status, body) = call(&app, post_cbor(&path, Some(&token), outer)).await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&body));
    assert!(!revoke_response(&body).changed);
    assert_eq!(fx.vault().auth_rev, current.auth_rev);
    assert_eq!(
        block(|| fx.rotation_view(&rotation).rotation.state),
        RotationState::Frozen,
        "空操作不得推进轮换"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_mutual_revoke_leaves_exactly_one_active() {
    let (fx, app) = setup("revoke-concurrent");
    let a = fx.device;
    let b: Id16 = [0x0Bu8; 16];
    block(|| fx.add_device_with(b, b"cert-B", "active"));
    let before = fx.vault();
    let token = block(|| fx.manager_token());

    // 两台设备的管理请求**并发**互删；同一 `BEGIN IMMEDIATE` 内数 ACTIVE 与更新。
    let (path_a, outer_a) = revoke_request(&fx, &a, [0x71u8; 16], before.control_rev);
    let (path_b, outer_b) = revoke_request(&fx, &b, [0x72u8; 16], before.control_rev);
    let (first, second) = tokio::join!(
        call(&app, post_cbor(&path_a, Some(&token), outer_a)),
        call(&app, post_cbor(&path_b, Some(&token), outer_b)),
    );

    let successes =
        usize::from(first.0 == StatusCode::OK) + usize::from(second.0 == StatusCode::OK);
    assert_eq!(successes, 1, "并发互删最多成功一个：{first:?} / {second:?}");
    let loser = if first.0 == StatusCode::OK {
        second.0
    } else {
        first.0
    };
    assert!(
        matches!(loser, StatusCode::FORBIDDEN | StatusCode::CONFLICT),
        "失败方必须是 403 REVOKED 或 409 INVALID_STATE，实际 {loser}"
    );
    let active = block(|| fx.active_device_ids());
    assert_eq!(active.len(), 1, "必须留一台 ACTIVE（不能吊销到 0 台）");
    assert_eq!(
        fx.vault().auth_rev,
        before.auth_rev + 1,
        "auth_rev 只推进一次"
    );
    assert_eq!(
        fx.vault().control_rev,
        before.control_rev,
        "control_rev 不变"
    );
}

// ---------------------------------------------------------------- 5. 签名/镜像/CAS

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn signature_mirror_and_cas_rejections() {
    let (fx, app) = setup("revoke-rejections");
    let a = fx.device;
    let b: Id16 = [0x0Bu8; 16];
    block(|| fx.add_device_with(b, b"cert-B", "active"));
    let before = fx.vault();
    let device_token = block(|| fx.device_token());
    let manager_token = block(|| fx.manager_token());

    // ① 设备 realm bearer（非管理）→ 403 MANAGEMENT_REQUIRED。
    let request_id = [0x81u8; 16];
    let (path, outer) = revoke_request(&fx, &b, request_id, before.control_rev);
    let (status, body) = call(&app, post_cbor(&path, Some(&device_token), outer.clone())).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{}", error_code(&body));
    assert_eq!(error_code(&body), "MANAGEMENT_REQUIRED");

    // ② 错管理签名（另一把私钥）→ 403 MANAGEMENT_REQUIRED。
    let body_bytes = revoke_body(&b, &request_id, before.control_rev);
    let bad = envelope_bad_key(
        &fx,
        &path,
        body_bytes.clone(),
        request_id,
        before.control_rev,
    );
    let (status, body) = call(&app, post_cbor(&path, Some(&manager_token), bad)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{}", error_code(&body));
    assert_eq!(error_code(&body), "MANAGEMENT_REQUIRED");

    // ③ path 的 target 被替换（签名覆盖原 path）→ 验签失败 403。
    let (path_a, _) = revoke_request(&fx, &a, request_id, before.control_rev);
    let (status, body) = call(&app, post_cbor(&path_a, Some(&manager_token), outer)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{}", error_code(&body));
    assert_eq!(error_code(&body), "MANAGEMENT_REQUIRED");

    // ④ 签名有效但 path target 与 body target 不一致 → 镜像语义 403 FORBIDDEN
    //    （"签一套、执行另一套"）。
    let mismatched = envelope(
        &fx,
        &path_a,
        body_bytes.clone(),
        request_id,
        before.control_rev,
    );
    let (status, body) = call(&app, post_cbor(&path_a, Some(&manager_token), mismatched)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{}", error_code(&body));
    assert_eq!(error_code(&body), "FORBIDDEN");

    // ⑤ 外层 request_id 镜像与 body 不一致 → 403 FORBIDDEN。
    let body_other_id = revoke_body(&b, &[0x82u8; 16], before.control_rev);
    let mirrored = envelope(&fx, &path, body_other_id, request_id, before.control_rev);
    let (status, body) = call(&app, post_cbor(&path, Some(&manager_token), mirrored)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{}", error_code(&body));
    assert_eq!(error_code(&body), "FORBIDDEN");

    // ⑥ 外层 expected_control_rev 镜像与 body 不一致 → 403 FORBIDDEN。
    let body_other_rev = revoke_body(&b, &request_id, before.control_rev + 1);
    let mirrored = envelope(&fx, &path, body_other_rev, request_id, before.control_rev);
    let (status, body) = call(&app, post_cbor(&path, Some(&manager_token), mirrored)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{}", error_code(&body));
    assert_eq!(error_code(&body), "FORBIDDEN");

    // ⑦ 非法 body（target 不是 16B bytes）→ 400 INVALID_REQUEST。
    let invalid = cbor_body(vec![
        (0, wire::text("not-an-id")),
        (1, wire::bytes(request_id)),
        (2, wire::uint(before.control_rev)),
    ]);
    let fields = envelope(&fx, &path, invalid, request_id, before.control_rev);
    let (status, body) = call(&app, post_cbor(&path, Some(&manager_token), fields)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{}", error_code(&body));
    assert_eq!(error_code(&body), "INVALID_REQUEST");

    // 失败无副作用：B 仍 ACTIVE、版本不变、没有幂等记录。
    assert_eq!(
        block(|| fx.device_snapshot(&b)).expect("B row").status,
        "active"
    );
    let after = fx.vault();
    assert_eq!(
        (after.auth_rev, after.control_rev),
        (before.auth_rev, before.control_rev)
    );
    assert_eq!(dedup_rows(&fx, &request_id), 0);
    assert_eq!(dedup_rows(&fx, &[0x82u8; 16]), 0);

    // ⑧ 过期 CAS（新 id + 错误的 expected_control_rev，签名自带该值）→ 409 CAS_CONFLICT。
    let stale_id = [0x83u8; 16];
    let (stale_path, stale) = revoke_request(&fx, &b, stale_id, before.control_rev + 1);
    let (status, body) = call(&app, post_cbor(&stale_path, Some(&manager_token), stale)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{}", error_code(&body));
    assert_eq!(error_code(&body), "CAS_CONFLICT");
    assert_eq!(
        wire::opt_uint_field(&cbor_fields(&body), 3).expect("current rev"),
        Some(before.control_rev)
    );

    // ⑨ 正例：同样的 path/body 在正确 rev 下成功（证明上面拒绝的是镜像/签名而非业务）。
    let request_id = [0x84u8; 16];
    let (path, outer) = revoke_request(&fx, &b, request_id, before.control_rev);
    let (status, body) = call(&app, post_cbor(&path, Some(&manager_token), outer)).await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&body));
    let response = revoke_response(&body);
    assert!(response.changed);
    assert_eq!(response.auth_rev, before.auth_rev + 1);
}
