//! ADR 0014 **S2b**：`/v2/vaults` 一次性创建票据 + `/v2/vaults/{id}/auth/*` 租户鉴权。
//!
//! 覆盖（任务书 §3）：
//! 1. 票据：发行→创建成功→`consumed_at`；重复消费/过期/错 vault/错 ticket/超 quota 拒绝；
//!    并发双消费只一个；失败不建 vault、不消费票据；
//! 2. 两 vault：两张票据建两个 vault；v2 auth 各自成功；token/challenge 跨 vault 拒绝；
//!    v1 legacy 仍只到默认 vault；
//! 3. 创建事务：genesis + 票据消费原子（坏证书失败 → 票据回滚）；
//! 4. 限流：v2 create/auth 超限返回 429，v1 不受影响。
//!
//! 全部走 in-process axum `Router::oneshot`（不监听端口），默认关闭 v2 限流，
//! 只有限流用例显式配置低阈值。

mod support;

use std::time::Duration;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use relay::admin;
use relay::api::{router, AppState};
use relay::ratelimit::RateLimitSpec;
use relay::vault::{self, InitRequest};
use relay::wire;
use support::*;
use tower::ServiceExt;

const VAULT_B: [u8; 16] = [0xB7u8; 16];
const VAULT_C: [u8; 16] = [0xC7u8; 16];
const VAULT_D: [u8; 16] = [0xD7u8; 16];
const VAULT_E: [u8; 16] = [0xE7u8; 16];
const VAULT_F: [u8; 16] = [0xF7u8; 16];
const DEVICE_B: [u8; 16] = [0x0Bu8; 16];

/// 在 tokio 多线程 runtime 里执行同步 DB 调用（writer 线程 + `block_in_place`）。
fn block<T>(f: impl FnOnce() -> T) -> T {
    tokio::task::block_in_place(f)
}

/// 默认关闭 v2 限流的夹具（只有限流用例显式开启）。
fn fixture(label: &str) -> Fixture {
    Fixture::with_v2_rate_limits(label, None, None)
}

fn app(fx: &Fixture) -> axum::Router {
    router(AppState::new(
        fx.db.inner().clone(),
        fx.cfg.clone(),
        fx.hook.clone(),
    ))
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

fn get(uri: &str, token: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().method("GET").uri(uri);
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    builder.body(Body::empty()).expect("request")
}

fn post_cbor(uri: &str, fields: wire::Fields) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, wire::CONTENT_TYPE)
        .body(Body::from(cbor_body(fields)))
        .expect("request")
}

fn error_code(body: &[u8]) -> String {
    code_of(&cbor_fields(body))
}

fn code_of(fields: &wire::Fields) -> String {
    wire::text_field(fields, 0).unwrap_or_default()
}

fn b64(id: &[u8; 16]) -> String {
    relay::ids::b64url_encode(id)
}

fn ticket_for(fx: &Fixture, vault_id: [u8; 16], ttl_secs: u64, quota: u32) -> [u8; 32] {
    admin::issue_create_ticket(
        fx.db.inner(),
        vault_id,
        Duration::from_secs(ttl_secs),
        quota,
    )
    .expect("issue create ticket")
}

/// 构造一个属主/设备公钥匹配的 `InitRequest`（证书签名由 `TestHook` 放行）。
fn init_request(
    vault_id: [u8; 16],
    device_id: [u8; 16],
    sign_pk: [u8; 32],
    manager_pub: [u8; 32],
) -> InitRequest {
    let hpke_pk = [4u8; 32];
    InitRequest {
        vault_id,
        device_id,
        sign_pk,
        hpke_pk,
        manager_pub,
        setup_secret: String::new(),
        certificate: certificate_bytes(&vault_id, &device_id, &sign_pk, &hpke_pk, 1),
        certificate_signature: b"certificate-sig".to_vec(),
    }
}

/// v2 create 请求体（字段编号见 `api::create_vault_v2` 文档）。
fn create_fields(ticket: &[u8], request: &InitRequest) -> wire::Fields {
    vec![
        (0, wire::bytes(ticket.to_vec())),
        (1, wire::bytes(request.vault_id.to_vec())),
        (2, wire::bytes(request.device_id.to_vec())),
        (3, wire::bytes(request.sign_pk.to_vec())),
        (4, wire::bytes(request.hpke_pk.to_vec())),
        (5, wire::bytes(request.manager_pub.to_vec())),
        (6, wire::bytes(request.certificate.clone())),
        (7, wire::bytes(request.certificate_signature.clone())),
    ]
}

fn create_request(ticket: &[u8], request: &InitRequest) -> Request<Body> {
    post_cbor("/v2/vaults", create_fields(ticket, request))
}

fn vault_exists(fx: &Fixture, vault_id: [u8; 16]) -> bool {
    fx.db
        .inner()
        .read(relay::VaultScope::new(vault_id), vault::load_opt)
        .expect("load vault")
        .is_some()
}

fn vault_count(fx: &Fixture) -> i64 {
    fx.db
        .read_instance(|conn| {
            Ok(conn.query_row("SELECT COUNT(*) FROM vaults", [], |row| row.get(0))?)
        })
        .expect("count vaults")
}

fn ticket_row(fx: &Fixture, ticket: &[u8]) -> admin::CreateTicketRow {
    admin::load_create_ticket(fx.db.inner(), ticket)
        .expect("load ticket")
        .expect("ticket row")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ticket_create_is_single_use_and_response_matches_v1_shape() {
    let fx = block(|| fixture("v2-ticket-create"));
    let app = app(&fx);
    let sign_pk = fx.device_signing_key.verifying_key().to_bytes();
    let init = init_request(VAULT_B, DEVICE_B, sign_pk, fx.manager_pub);
    let ticket = block(|| ticket_for(&fx, VAULT_B, 900, 1));
    assert_eq!(vault_count(&fx), 1, "夹具只有 legacy 默认 vault A");

    let (status, body) = call(&app, create_request(&ticket, &init)).await;
    assert_eq!(status, StatusCode::CREATED, "body={:?}", error_code(&body));
    let created = cbor_fields(&body);
    let instance = wire::bytes_field(&created, 0).expect("instance");
    assert_eq!(instance.len(), 16);
    assert_eq!(
        instance,
        fx.vault().instance_id.to_vec(),
        "多租户同库共享实例 id"
    );
    assert_eq!(wire::bytes_field(&created, 1).expect("log").len(), 16);
    assert_eq!(wire::uint_field(&created, 2).expect("epoch"), 1);
    assert_eq!(wire::uint_field(&created, 3).expect("control"), 1);
    assert_eq!(wire::uint_field(&created, 4).expect("auth"), 1);
    let cursor = wire::text_field(&created, 5).expect("cursor");
    assert!(cursor.len() > 80, "响应必须带 seq=0 的 HMAC 游标");

    // 库内：B 行存在、不是 legacy 默认；票据额度归零且写 consumed_at。
    let b = fx
        .db
        .inner()
        .read(relay::VaultScope::new(VAULT_B), vault::load)
        .expect("vault B");
    assert_eq!(b.vault_id, VAULT_B);
    assert_eq!(b.active_epoch, 1);
    assert_eq!(b.control_rev, 1);
    assert_eq!(b.auth_rev, 1);
    assert_eq!(b.manager_rev, 1);
    let legacy_default: i64 = fx
        .db
        .inner()
        .read(relay::VaultScope::new(VAULT_B), |conn| {
            Ok(conn.raw().query_row(
                "SELECT legacy_default FROM vaults WHERE vault_id = ?1",
                [VAULT_B.as_slice()],
                |row| row.get(0),
            )?)
        })
        .expect("legacy_default");
    assert_eq!(legacy_default, 0, "票据创建不得产生 v1 legacy 默认 vault");
    let row = block(|| ticket_row(&fx, &ticket));
    assert_eq!(row.quota, 0);
    assert_eq!(row.consumed_at, Some(BASE_TIME));
    assert_eq!(vault_count(&fx), 2);

    // 重复消费：403 FORBIDDEN，不能再建库。
    let (status, body) = call(&app, create_request(&ticket, &init)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(error_code(&body), "FORBIDDEN");
    assert_eq!(vault_count(&fx), 2);

    // 明文票据不得落库：库里只有 SHA256。
    let plaintext_rows: i64 = fx
        .db
        .read_instance(|conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM create_tickets WHERE token_hash = ?1",
                [ticket.as_slice()],
                |row| row.get(0),
            )?)
        })
        .expect("plaintext probe");
    assert_eq!(plaintext_rows, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ticket_failures_do_not_create_vaults_or_consume_tickets() {
    let fx = block(|| fixture("v2-ticket-failures"));
    let app = app(&fx);
    let sign_pk = fx.device_signing_key.verifying_key().to_bytes();
    let init_b = init_request(VAULT_B, DEVICE_B, sign_pk, fx.manager_pub);
    let ticket_b = block(|| ticket_for(&fx, VAULT_B, 900, 1));

    // 未知票据（32B 随机）→ 403，不建库。
    let (status, body) = call(&app, create_request(&[0x99u8; 32], &init_b)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(error_code(&body), "FORBIDDEN");
    assert!(!vault_exists(&fx, VAULT_B));

    // 票据长度错误 → 同一个 403（不区分原因）。
    let (status, _) = call(&app, create_request(&[0x99u8; 31], &init_b)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // 错 vault：票据绑定 B，但请求要建 C → 403，票据不消费。
    let init_c = init_request(VAULT_C, DEVICE_B, sign_pk, fx.manager_pub);
    let (status, _) = call(&app, create_request(&ticket_b, &init_c)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(!vault_exists(&fx, VAULT_C));
    let row = block(|| ticket_row(&fx, &ticket_b));
    assert_eq!(row.quota, 1);
    assert_eq!(row.consumed_at, None);

    // 坏证书（证书绑定 C，但请求是 B）：创建失败 → 400，票据回滚不消费、不建库。
    let mut bad = init_request(VAULT_B, DEVICE_B, sign_pk, fx.manager_pub);
    bad.certificate = init_c.certificate.clone();
    let (status, body) = call(&app, create_request(&ticket_b, &bad)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error_code(&body), "INVALID_REQUEST");
    assert!(!vault_exists(&fx, VAULT_B));
    let row = block(|| ticket_row(&fx, &ticket_b));
    assert_eq!(row.quota, 1);
    assert_eq!(row.consumed_at, None);

    // 过期：TTL 到点后 403，不建库、不消费。
    let ticket_expired = block(|| ticket_for(&fx, VAULT_C, 60, 1));
    fx.clock.advance_secs(61);
    let (status, _) = call(&app, create_request(&ticket_expired, &init_c)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(!vault_exists(&fx, VAULT_C));
    let row = block(|| ticket_row(&fx, &ticket_expired));
    assert_eq!(row.quota, 1);
    assert_eq!(row.consumed_at, None);

    // 证明前面的失败都没消费：同一张票据用正确请求仍能成功。
    let (status, _) = call(&app, create_request(&ticket_b, &init_b)).await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(vault_exists(&fx, VAULT_B));
    assert_eq!(vault_count(&fx), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quota_is_enforced_and_failed_create_does_not_consume() {
    let fx = block(|| fixture("v2-ticket-quota"));
    let app = app(&fx);
    let sign_pk = fx.device_signing_key.verifying_key().to_bytes();
    let init_d = init_request(VAULT_D, DEVICE_B, sign_pk, fx.manager_pub);
    let ticket_d = block(|| ticket_for(&fx, VAULT_D, 900, 2));

    let (status, _) = call(&app, create_request(&ticket_d, &init_d)).await;
    assert_eq!(status, StatusCode::CREATED);
    let row = block(|| ticket_row(&fx, &ticket_d));
    assert_eq!(row.quota, 1, "成功创建消费 1 次额度");
    assert_eq!(row.consumed_at, None, "额度未归零不写 consumed_at");

    // 同票据重试同一 vault：票据仍有效 → 走创建 → 409 ALREADY_INITIALIZED；
    // 事务回滚，额度保持 1（失败不消费），不产生第二个 vault。
    let (status, body) = call(&app, create_request(&ticket_d, &init_d)).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(error_code(&body), "ALREADY_INITIALIZED");
    let row = block(|| ticket_row(&fx, &ticket_d));
    assert_eq!(row.quota, 1);
    assert_eq!(row.consumed_at, None);
    assert_eq!(vault_count(&fx), 2);

    // 同票据换 vault → 403（绑定不符），不消费。
    let init_e = init_request(VAULT_E, DEVICE_B, sign_pk, fx.manager_pub);
    let (status, _) = call(&app, create_request(&ticket_d, &init_e)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let row = block(|| ticket_row(&fx, &ticket_d));
    assert_eq!(row.quota, 1);

    // 一次性票据（quota=1）：成功后额度归零 + consumed_at，再试统一 403。
    let ticket_e = block(|| ticket_for(&fx, VAULT_E, 900, 1));
    let (status, _) = call(&app, create_request(&ticket_e, &init_e)).await;
    assert_eq!(status, StatusCode::CREATED);
    let row = block(|| ticket_row(&fx, &ticket_e));
    assert_eq!(row.quota, 0);
    assert_eq!(row.consumed_at, Some(BASE_TIME));
    let (status, _) = call(&app, create_request(&ticket_e, &init_e)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(vault_count(&fx), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_double_consume_only_one_wins() {
    let fx = block(|| fixture("v2-ticket-concurrent"));
    let app = app(&fx);
    let sign_pk = fx.device_signing_key.verifying_key().to_bytes();
    let init = init_request(VAULT_F, DEVICE_B, sign_pk, fx.manager_pub);
    let ticket = block(|| ticket_for(&fx, VAULT_F, 900, 1));

    let first = create_request(&ticket, &init);
    let second = create_request(&ticket, &init);
    let (left, right) = tokio::join!(call(&app, first), call(&app, second));
    let mut statuses = [left.0, right.0];
    statuses.sort_by_key(|status| status.as_u16());
    assert_eq!(
        statuses,
        [StatusCode::CREATED, StatusCode::FORBIDDEN],
        "并发双消费必须恰好一个成功、一个 403"
    );

    assert!(vault_exists(&fx, VAULT_F));
    assert_eq!(vault_count(&fx), 2, "只允许建出一个 vault");
    let row = block(|| ticket_row(&fx, &ticket));
    assert_eq!(row.quota, 0);
    assert_eq!(row.consumed_at, Some(BASE_TIME));
}

/// v2 challenge → token 的直接封装（测试可断言状态码与错误体）。
async fn challenge(
    app: &axum::Router,
    vault_id: [u8; 16],
    subject: [u8; 16],
) -> (StatusCode, wire::Fields) {
    let (status, body) = call(
        app,
        post_cbor(
            &format!("/v2/vaults/{}/auth/challenges", b64(&vault_id)),
            vec![(0, wire::uint(1)), (1, wire::bytes(subject.to_vec()))],
        ),
    )
    .await;
    (status, cbor_fields(&body))
}

async fn token(
    app: &axum::Router,
    vault_id: [u8; 16],
    challenge_id: &[u8],
) -> (StatusCode, wire::Fields) {
    let (status, body) = call(
        app,
        post_cbor(
            &format!("/v2/vaults/{}/auth/tokens", b64(&vault_id)),
            vec![
                (0, wire::bytes(challenge_id.to_vec())),
                (1, wire::bytes(b"challenge-message")),
                (2, wire::bytes(b"signature")),
            ],
        ),
    )
    .await;
    (status, cbor_fields(&body))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_vaults_authenticate_separately_and_v1_stays_on_legacy_default() {
    let fx = block(|| fixture("v2-two-vaults"));
    let app = app(&fx);
    let sign_pk = fx.device_signing_key.verifying_key().to_bytes();
    // B 用**不同**设备 id，证明 v1 challenge 不会按 subject 猜到 B。
    let init_b = init_request(VAULT_B, DEVICE_B, sign_pk, fx.manager_pub);
    let ticket = block(|| ticket_for(&fx, VAULT_B, 900, 1));
    let (status, _) = call(&app, create_request(&ticket, &init_b)).await;
    assert_eq!(status, StatusCode::CREATED);

    // A 的 v2 auth（路由 vault=A）。
    let (status, fields) = challenge(&app, fx.vault_id, fx.device).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        wire::bytes_field(&fields, 5).expect("challenge vault"),
        fx.vault_id.to_vec()
    );
    let challenge_a = wire::bytes_field(&fields, 0).expect("challenge id");
    let (status, fields) = token(&app, fx.vault_id, &challenge_a).await;
    assert_eq!(status, StatusCode::OK);
    let token_a = wire::text_field(&fields, 0).expect("token A");
    assert_eq!(wire::uint_field(&fields, 3).expect("realm"), 1);

    // B 的 v2 auth（路由 vault=B；设备 B 只在 B 里 active）。
    let (status, fields) = challenge(&app, VAULT_B, DEVICE_B).await;
    assert_eq!(status, StatusCode::OK);
    let challenge_b = wire::bytes_field(&fields, 0).expect("challenge id");
    let (status, fields) = token(&app, VAULT_B, &challenge_b).await;
    assert_eq!(status, StatusCode::OK);
    let token_b = wire::text_field(&fields, 0).expect("token B");

    // 各自 v2 state 成功。
    let (status, body) = call(
        &app,
        get(
            &format!("/v2/vaults/{}/state", b64(&fx.vault_id)),
            Some(&token_a),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        wire::bytes_field(&cbor_fields(&body), 0).expect("vault id"),
        fx.vault_id.to_vec()
    );
    let (status, _) = call(
        &app,
        get(
            &format!("/v2/vaults/{}/state", b64(&VAULT_B)),
            Some(&token_b),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // token 跨 vault：v2 路由 404（不泄露目标租户是否存在）。
    let (status, _) = call(
        &app,
        get(
            &format!("/v2/vaults/{}/state", b64(&VAULT_B)),
            Some(&token_a),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(
        &app,
        get(
            &format!("/v2/vaults/{}/state", b64(&fx.vault_id)),
            Some(&token_b),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // challenge 跨 vault：A 的 challenge 在 B 的 scope 里查不到 → 401 CHALLENGE_INVALID。
    let (status, body) = token(&app, VAULT_B, &challenge_a).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(code_of(&body), "CHALLENGE_INVALID");

    // 未知 vault 的 v2 challenge → 404（不建库、不泄露其它租户）。
    let (status, _) = challenge(&app, [0xEEu8; 16], fx.device).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // v1 legacy 仍只到默认 vault A：
    // - v1 challenge 用 B 的设备 id → 403 REVOKED（A 里没有这台设备）；
    let (status, body) = call(
        &app,
        post_cbor(
            "/v1/auth/challenges",
            vec![(0, wire::uint(1)), (1, wire::bytes(DEVICE_B.to_vec()))],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(error_code(&body), "REVOKED");
    // - A 的设备仍可走 v1 challenge/token；v1 state 用 token A 成功。
    let (status, body) = call(
        &app,
        post_cbor(
            "/v1/auth/challenges",
            vec![(0, wire::uint(1)), (1, wire::bytes(fx.device.to_vec()))],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let v1_challenge = wire::bytes_field(&cbor_fields(&body), 0).expect("v1 challenge");
    let (status, body) = call(
        &app,
        post_cbor(
            "/v1/auth/tokens",
            vec![
                (0, wire::bytes(v1_challenge)),
                (1, wire::bytes(b"challenge-message")),
                (2, wire::bytes(b"signature")),
            ],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let v1_token = wire::text_field(&cbor_fields(&body), 0).expect("v1 token");
    let (status, _) = call(
        &app,
        get(
            &format!("/v1/vaults/{}/state", b64(&fx.vault_id)),
            Some(&v1_token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // - B 的 v2 token 在 v1 的 A scope 里仍按旧语义 404（token 有效但属于别的 vault）。
    let (status, _) = call(
        &app,
        get(
            &format!("/v1/vaults/{}/state", b64(&fx.vault_id)),
            Some(&token_b),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v2_rate_limits_return_429_without_touching_v1() {
    let fx = block(|| {
        Fixture::with_v2_rate_limits(
            "v2-rate-limits",
            RateLimitSpec::new(1, 60),
            RateLimitSpec::new(1, 60),
        )
    });
    let app = app(&fx);
    let sign_pk = fx.device_signing_key.verifying_key().to_bytes();
    let init_b = init_request(VAULT_B, DEVICE_B, sign_pk, fx.manager_pub);
    let init_c = init_request(VAULT_C, DEVICE_B, sign_pk, fx.manager_pub);
    let ticket_b = block(|| ticket_for(&fx, VAULT_B, 900, 1));
    let ticket_c = block(|| ticket_for(&fx, VAULT_C, 900, 1));

    // 第 1 次 create 放行，第 2 次 429（阈值 = 1/60s）。
    let (status, _) = call(&app, create_request(&ticket_b, &init_b)).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = call(&app, create_request(&ticket_c, &init_c)).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(error_code(&body), "RATE_LIMITED");
    assert!(!vault_exists(&fx, VAULT_C), "限流拒绝不得建库");
    let row = block(|| ticket_row(&fx, &ticket_c));
    assert_eq!(row.quota, 1, "限流拒绝不得消费票据");
    assert_eq!(row.consumed_at, None);

    // v2 auth 同样限流：第 1 次 challenge 放行，第 2 次 429。
    let (status, _) = challenge(&app, fx.vault_id, fx.device).await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = challenge(&app, fx.vault_id, fx.device).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(code_of(&body), "RATE_LIMITED");

    // v1 完全不受影响：连续两次 challenge 都 200。
    for _ in 0..2 {
        let (status, _) = call(
            &app,
            post_cbor(
                "/v1/auth/challenges",
                vec![(0, wire::uint(1)), (1, wire::bytes(fx.device.to_vec()))],
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ticket_create_does_not_consume_setup_secret_or_touch_v1_metadata() {
    let fx = block(|| fixture("v2-ticket-metadata"));
    let app = app(&fx);
    let setup_before = fx
        .db
        .read_instance(|conn| {
            relay::db::schema::meta_text(conn, relay::db::schema::META_SETUP_CONSUMED)
        })
        .expect("setup.consumed");
    assert_eq!(setup_before.as_deref(), Some("1"));

    let sign_pk = fx.device_signing_key.verifying_key().to_bytes();
    let init = init_request(VAULT_B, DEVICE_B, sign_pk, fx.manager_pub);
    let ticket = block(|| ticket_for(&fx, VAULT_B, 900, 1));
    let (status, _) = call(&app, create_request(&ticket, &init)).await;
    assert_eq!(status, StatusCode::CREATED);

    // 实例级 setup secret 状态不变（v2 票据不是 setup secret）。
    let setup_after = fx
        .db
        .read_instance(|conn| {
            relay::db::schema::meta_text(conn, relay::db::schema::META_SETUP_CONSUMED)
        })
        .expect("setup.consumed");
    assert_eq!(setup_after, setup_before);
    // schema 版本 / legacy 默认 vault 不变。
    let version = fx
        .db
        .read_instance(|conn| {
            relay::db::schema::meta_text(conn, relay::db::schema::META_SCHEMA_VERSION)
        })
        .expect("schema version");
    assert_eq!(version, Some(relay::db::schema::SCHEMA_VERSION.to_string()));
    let legacy = fx
        .db
        .read_instance(relay::db::schema::legacy_default_vault_id)
        .expect("legacy");
    assert_eq!(legacy, Some(fx.vault_id));
}
