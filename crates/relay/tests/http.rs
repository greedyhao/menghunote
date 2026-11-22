//! in-process HTTP 路由测试（axum `Router::oneshot`，不监听端口）。
//!
//! 控制面是 `application/cbor` + CDE（`relay::wire`）；对象分片是 octet-stream。

mod support;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use ed25519_dalek::{Signer, SigningKey};
use http_body_util::BodyExt;
use relay::api::{router, AppState};
use relay::config::RelayConfig;
use relay::db::Db;
use relay::pairing;
use relay::proto;
use relay::sig::{Expectation, SignDomain, SignatureCheck, SignatureHook};
use relay::time::FakeClock;
use relay::verify::{self, Ed25519Hook};
use relay::wire;
use support::*;
use tower::ServiceExt;

/// 在 tokio 多线程 runtime 里安全地执行同步 DB 调用（`block_in_place` + writer 线程）。
fn block<T>(f: impl FnOnce() -> T) -> T {
    tokio::task::block_in_place(f)
}

fn app(fx: &Fixture) -> axum::Router {
    let state = AppState::new(fx.db.inner().clone(), fx.cfg.clone(), fx.hook.clone());
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

fn get(uri: &str, token: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().method("GET").uri(uri);
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    builder.body(Body::empty()).expect("request")
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

fn put_bytes(uri: &str, token: &str, bytes: Vec<u8>) -> Request<Body> {
    Request::builder()
        .method("PUT")
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body(Body::from(bytes))
        .expect("request")
}

fn cbor_response(body: &[u8]) -> wire::Fields {
    cbor_fields(body)
}

fn error_code(body: &[u8]) -> String {
    let fields = cbor_response(body);
    wire::text_field(&fields, 0).unwrap_or_default()
}

fn b64(bytes: &[u8]) -> String {
    relay::ids::b64url_encode(bytes)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auth_challenge_token_and_state_endpoints() {
    let fx = block(|| Fixture::new("http-auth"));
    let app = app(&fx);

    // 无 Bearer → 401 UNAUTHORIZED（CBOR 错误体）。
    let (status, body) = call(
        &app,
        get(&format!("/v1/vaults/{}/state", b64(&fx.vault_id)), None),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(error_code(&body), "UNAUTHORIZED");
    assert_eq!(cbor_response(&body).first().map(|(key, _)| *key), Some(0));

    // challenge：realm tag 1=device。
    let (status, body) = call(
        &app,
        post_cbor(
            "/v1/auth/challenges",
            None,
            vec![(0, wire::uint(1)), (1, wire::bytes(fx.device))],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let challenge = cbor_response(&body);
    let challenge_id = wire::id16_field(&challenge, 0).expect("challenge id");
    assert_eq!(wire::uint_field(&challenge, 1).expect("realm"), 1);

    // token。
    let (status, body) = call(
        &app,
        post_cbor(
            "/v1/auth/tokens",
            None,
            vec![
                (0, wire::bytes(challenge_id.to_vec())),
                (1, wire::bytes(b"challenge-message")),
                (2, wire::bytes(b"signature")),
            ],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let token = wire::text_field(&cbor_response(&body), 0).expect("token");
    assert_eq!(token.len(), 43);

    // state。
    let (status, body) = call(
        &app,
        get(
            &format!("/v1/vaults/{}/state", b64(&fx.vault_id)),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let state = cbor_response(&body);
    assert_eq!(wire::uint_field(&state, 4).expect("epoch"), 1);
    assert_eq!(wire::uint_field(&state, 7).expect("head_seq"), 0);
    assert_eq!(wire::text_field(&state, 3).expect("state"), "active");
    // 键 14：seq=0 的初始游标（客户端拿不到 HMAC 游标时的兜底）。
    let state_cursor = wire::text_field(&state, 14).expect("state initial cursor");
    assert!(state_cursor.len() > 80);

    // 跨 vault → 404。
    let (status, _) = call(
        &app,
        get(
            &format!("/v1/vaults/{}/state", b64(&[9u8; 16])),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // control 链：genesis 证书在 rev 1。
    let (status, body) = call(
        &app,
        get(
            &format!("/v1/vaults/{}/control?after_rev=0", b64(&fx.vault_id)),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let control = cbor_response(&body);
    let commits = wire::array_field(&control, 0).expect("commits");
    let first = wire::value_map(&commits[0]).expect("commit");
    assert_eq!(wire::uint_field(&first, 0).expect("rev"), 1);
    // members 键 5 = genesis 设备证书签名（§17.1）。
    let members = wire::array_field(&control, 3).expect("members");
    let genesis_member = wire::value_map(&members[0]).expect("member");
    assert_eq!(
        wire::bytes_field(&genesis_member, 5).expect("genesis cert signature"),
        b"certificate-sig"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn object_pipeline_over_http_is_idempotent() {
    let fx = block(|| Fixture::new("http-objects"));
    let app = app(&fx);
    let token = block(|| fx.device_token());
    let object_id = random_object_id();
    let envelope = support::single_part_envelope(
        relay::envelope::ObjectType::Update,
        fx.vault_id,
        object_id,
        1,
        1,
        64,
    );
    let part_uri = format!(
        "/v1/vaults/{}/objects/{}/parts/0",
        b64(&fx.vault_id),
        b64(&object_id)
    );

    let (status, body) = call(&app, put_bytes(&part_uri, &token, envelope.clone())).await;
    assert_eq!(status, StatusCode::CREATED, "{:?}", error_code(&body));
    let ack = cbor_response(&body);
    assert!(!wire::bool_field(&ack, 3).expect("duplicate"));

    // 同字节重试 → 200 duplicate。
    let (status, body) = call(&app, put_bytes(&part_uri, &token, envelope.clone())).await;
    assert_eq!(status, StatusCode::OK);
    assert!(wire::bool_field(&cbor_response(&body), 3).expect("dup"));

    // GET part → 原样字节 + ETag。
    let (status, body) = call(&app, get(&part_uri, Some(&token))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, envelope);

    // GET object（CBOR 元数据）。
    let object_uri = format!(
        "/v1/vaults/{}/objects/{}",
        b64(&fx.vault_id),
        b64(&object_id)
    );
    let (status, body) = call(&app, get(&object_uri, Some(&token))).await;
    assert_eq!(status, StatusCode::OK);
    let object = cbor_response(&body);
    assert_eq!(wire::text_field(&object, 9).expect("state"), "staging");
    assert_eq!(wire::array_field(&object, 12).expect("parts").len(), 1);

    // complete → 201 + relay_seq=1。
    let digest = digest_of(std::slice::from_ref(&envelope)).expect("digest");
    let complete_uri = format!(
        "/v1/vaults/{}/objects/{}/complete",
        b64(&fx.vault_id),
        b64(&object_id)
    );
    let (status, body) = call(
        &app,
        post_cbor(
            &complete_uri,
            Some(&token),
            vec![
                (0, wire::bytes(commit_for(&digest))),
                (1, wire::bytes(b"signature")),
            ],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{:?}", error_code(&body));
    let complete = cbor_response(&body);
    assert_eq!(wire::opt_uint_field(&complete, 1).expect("seq"), Some(1));
    assert!(!wire::bool_field(&complete, 2).expect("dup"));

    // 重复 complete → 200 duplicate。
    let (status, body) = call(
        &app,
        post_cbor(
            &complete_uri,
            Some(&token),
            vec![
                (0, wire::bytes(commit_for(&digest))),
                (1, wire::bytes(b"signature")),
            ],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let complete = cbor_response(&body);
    assert!(wire::bool_field(&complete, 2).expect("dup"));

    // GET log：游标 after=0 → 一条事件，next_cursor 是 text。
    let vault = block(|| fx.vault());
    let cursor = relay::cursor::CursorKey::new(vault.cursor_key)
        .encode(&relay::cursor::for_vault(&vault, 0).expect("cursor"))
        .expect("token");
    let log_uri = format!(
        "/v1/vaults/{}/log?after={}&limit=10",
        b64(&fx.vault_id),
        cursor
    );
    let (status, body) = call(&app, get(&log_uri, Some(&token))).await;
    assert_eq!(status, StatusCode::OK);
    let log = cbor_response(&body);
    let events = wire::array_field(&log, 0).expect("events");
    assert_eq!(events.len(), 1);
    let event = wire::value_map(&events[0]).expect("event");
    assert_eq!(wire::uint_field(&event, 0).expect("seq"), 1);
    // ADR §2：接收端要能验签，log 事件必须带签名（key 5）。
    assert_eq!(
        wire::bytes_field(&event, 5).expect("event signature"),
        b"signature"
    );
    assert!(wire::text_field(&log, 1).expect("next cursor").len() > 100);

    // 篡改游标 → 400 BAD_CURSOR。
    let bad_uri = format!(
        "/v1/vaults/{}/log?after={}AAAA&limit=10",
        b64(&fx.vault_id),
        cursor
    );
    let (status, body) = call(&app, get(&bad_uri, Some(&token))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error_code(&body), "BAD_CURSOR");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn updates_batch_is_partial_success() {
    let fx = block(|| Fixture::new("http-updates"));
    let app = app(&fx);
    let token = block(|| fx.device_token());
    let mut items = Vec::new();
    let mut accepted = None;
    for index in 0..2u64 {
        let object_id = random_object_id();
        let envelope = support::single_part_envelope(
            relay::envelope::ObjectType::Update,
            fx.vault_id,
            object_id,
            1,
            index + 1,
            64,
        );
        block(|| fx.put_part(&envelope, None)).expect("part");
        let digest = digest_of(std::slice::from_ref(&envelope)).expect("digest");
        let commit = if index == 1 {
            commit_for(&[0xAB; 32])
        } else {
            accepted = Some((object_id, commit_for(&digest)));
            commit_for(&digest)
        };
        items.push(wire::map(vec![
            (0, wire::bytes(object_id)),
            (1, wire::bytes(commit)),
            (2, wire::bytes(b"signature")),
        ]));
    }
    let (status, body) = call(
        &app,
        post_cbor(
            &format!("/v1/vaults/{}/updates", b64(&fx.vault_id)),
            Some(&token),
            vec![(0, wire::array(items))],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{:?}", error_code(&body));
    let response = cbor_response(&body);
    let results = wire::array_field(&response, 0).expect("items");
    assert_eq!(results.len(), 2);
    let first = wire::value_map(&results[0]).expect("item");
    let second = wire::value_map(&results[1]).expect("item");
    assert_eq!(wire::text_field(&first, 1).expect("status"), "accepted");
    assert_eq!(wire::opt_uint_field(&first, 2).expect("seq"), Some(1));
    assert_eq!(wire::text_field(&second, 1).expect("status"), "failed");
    assert_eq!(
        wire::text_field(&second, 3).expect("code"),
        "SIGNATURE_INVALID"
    );
    assert_eq!(block(|| fx.head_seq()), 1);

    // 已提交项重试 → duplicate + 原 seq。
    let (object_id, commit) = accepted.expect("accepted");
    let (status, body) = call(
        &app,
        post_cbor(
            &format!("/v1/vaults/{}/updates", b64(&fx.vault_id)),
            Some(&token),
            vec![(
                0,
                wire::array(vec![wire::map(vec![
                    (0, wire::bytes(object_id)),
                    (1, wire::bytes(commit)),
                    (2, wire::bytes(b"signature")),
                ])]),
            )],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let response = cbor_response(&body);
    let results = wire::array_field(&response, 0).expect("items");
    let first = wire::value_map(&results[0]).expect("item");
    assert_eq!(wire::text_field(&first, 1).expect("status"), "duplicate");
    assert_eq!(wire::opt_uint_field(&first, 2).expect("seq"), Some(1));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn asset_listing_and_oversized_part() {
    let fx = block(|| Fixture::new("http-assets"));
    let app = app(&fx);
    let token = block(|| fx.device_token());
    // 一个 asset（generation=0）完成后应出现在 `?type=asset&epoch=1&through=1`。
    let (asset, envelope, seq) = {
        let object_id = random_object_id();
        let envelope = support::single_part_envelope(
            relay::envelope::ObjectType::Asset,
            fx.vault_id,
            object_id,
            0,
            0,
            48,
        );
        block(|| fx.put_part(&envelope, None)).expect("part");
        let outcome = block(|| fx.complete_object(&object_id, &envelope)).expect("complete");
        (object_id, envelope, outcome.relay_seq.expect("asset seq"))
    };
    let list_uri = format!(
        "/v1/vaults/{}/objects?type=asset&epoch=1&through={}",
        b64(&fx.vault_id),
        seq
    );
    let (status, body) = call(&app, get(&list_uri, Some(&token))).await;
    assert_eq!(status, StatusCode::OK);
    let list = cbor_response(&body);
    let objects = wire::array_field(&list, 0).expect("objects");
    assert_eq!(objects.len(), 1);
    let entry = wire::value_map(&objects[0]).expect("entry");
    assert_eq!(wire::hash32_field(&entry, 0).expect("oid"), asset);
    assert_eq!(wire::uint_field(&entry, 1).expect("type"), 4);
    assert_eq!(
        wire::hash32_field(&entry, 6).expect("digest"),
        digest_of(std::slice::from_ref(&envelope)).expect("digest")
    );

    // 分页：after=asset 之后没有更多。
    let list_uri = format!(
        "/v1/vaults/{}/objects?type=asset&epoch=1&through={}&after={}",
        b64(&fx.vault_id),
        seq,
        b64(&asset)
    );
    let (status, body) = call(&app, get(&list_uri, Some(&token))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(wire::array_field(&cbor_response(&body), 0)
        .expect("objects")
        .is_empty());

    // 超限 body → 413，且不计费、不落片。
    let object_id = random_object_id();
    let uri = format!(
        "/v1/vaults/{}/objects/{}/parts/0",
        b64(&fx.vault_id),
        b64(&object_id)
    );
    let huge = vec![0u8; relay::envelope::MAX_ASSET_ENVELOPE + 1];
    let (status, _) = call(&app, put_bytes(&uri, &token, huge)).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    let parts: i64 = block(|| {
        fx.db
            .read(|conn| {
                Ok(conn.query_row("SELECT COUNT(*) FROM object_parts", [], |row| row.get(0))?)
            })
            .expect("count")
    });
    assert_eq!(parts, 1, "413 不得写入新的分片");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revoke_mid_session_immediately_rejects_token_over_http() {
    let fx = block(|| Fixture::new("http-revoke"));
    let app = app(&fx);
    let token = block(|| fx.device_token());
    let uri = format!("/v1/vaults/{}/state", b64(&fx.vault_id));
    let (status, _) = call(&app, get(&uri, Some(&token))).await;
    assert_eq!(status, StatusCode::OK);

    let device = fx.device;
    let now = block(|| fx.clock_now());
    block(|| {
        fx.db
            .write_blocking(move |conn| relay::auth::revoke_device(conn, now, &device))
            .expect("revoke")
    });
    let (status, body) = call(&app, get(&uri, Some(&token))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(error_code(&body), "REVOKED");
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn genesis_and_state_provide_initial_cursor_and_stale_cursor_is_410() {
    // 不走 Fixture：Fixture 已经初始化过 vault，这里要测 HTTP genesis 本身的响应。
    let path = temp_db_path("http-genesis-cursor");
    let cleanup_path = path.clone();
    let clock = Arc::new(FakeClock::new(BASE_TIME));
    let cfg = Arc::new(RelayConfig::new(path).with_setup_secret("setup-secret"));
    let db = Db::open(cfg.db.clone(), clock.clone()).expect("db");
    let hook = Arc::new(TestHook::new());
    let app = router(AppState::new(db.clone(), cfg.clone(), hook));

    let vault_id = [0x41u8; 16];
    let device = [0x42u8; 16];
    let sign_pk = [0x43u8; 32];
    let hpke_pk = [0x44u8; 32];
    let certificate = certificate_bytes(&vault_id, &device, &sign_pk, &hpke_pk, 1);
    let (status, body) = call(
        &app,
        post_cbor(
            "/v1/vaults",
            None,
            vec![
                (0, wire::bytes(vault_id.to_vec())),
                (1, wire::bytes(device.to_vec())),
                (2, wire::bytes(sign_pk.to_vec())),
                (3, wire::bytes(hpke_pk.to_vec())),
                (4, wire::bytes([7u8; 32].to_vec())),
                (5, wire::text("setup-secret")),
                (6, wire::bytes(certificate)),
                (7, wire::bytes(b"certificate-sig".to_vec())),
            ],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{:?}", error_code(&body));
    let init = cbor_response(&body);
    // 键 5：初始游标（推荐主路径，客户端写入 sync.relay_cursor）。
    let genesis_cursor = wire::text_field(&init, 5).expect("genesis cursor");

    // 设备 token。
    let (status, body) = call(
        &app,
        post_cbor(
            "/v1/auth/challenges",
            None,
            vec![(0, wire::uint(1)), (1, wire::bytes(device.to_vec()))],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let challenge_id = wire::id16_field(&cbor_response(&body), 0).expect("challenge");
    let (status, body) = call(
        &app,
        post_cbor(
            "/v1/auth/tokens",
            None,
            vec![
                (0, wire::bytes(challenge_id.to_vec())),
                (1, wire::bytes(b"challenge-message".to_vec())),
                (2, wire::bytes(b"signature".to_vec())),
            ],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let token = wire::text_field(&cbor_response(&body), 0).expect("token");

    // genesis 游标被 /log 接受（head=0 → 空页）。
    let log_uri = format!(
        "/v1/vaults/{}/log?after={}&limit=10",
        b64(&vault_id),
        genesis_cursor
    );
    let (status, body) = call(&app, get(&log_uri, Some(&token))).await;
    assert_eq!(status, StatusCode::OK, "{:?}", error_code(&body));
    let log = cbor_response(&body);
    assert!(wire::array_field(&log, 0).expect("events").is_empty());
    assert!(wire::text_field(&log, 1).expect("next cursor").len() > 80);

    // state 键 14 与 genesis 键 5 一致，同样可用于 /log。
    let (status, body) = call(
        &app,
        get(
            &format!("/v1/vaults/{}/state", b64(&vault_id)),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let state = cbor_response(&body);
    let state_cursor = wire::text_field(&state, 14).expect("state initial cursor");
    assert_eq!(state_cursor, genesis_cursor);
    let log_uri = format!(
        "/v1/vaults/{}/log?after={}&limit=10",
        b64(&vault_id),
        state_cursor
    );
    let (status, _) = call(&app, get(&log_uri, Some(&token))).await;
    assert_eq!(status, StatusCode::OK);

    // gc_floor > 0 后旧游标 → 410 CURSOR_EXPIRED（客户端据此走 bootstrap）。
    let handle = db.clone();
    block(|| {
        handle
            .write_blocking(relay::VaultScope::new(vault_id), |conn| {
                // CHECK: gc_floor <= head_seq；先推进 head 再抬 floor。
                conn.execute(
                    "UPDATE vaults SET head_seq = 1 WHERE vault_id = ?1",
                    [conn.scope().as_slice()],
                )?;
                conn.execute(
                    "UPDATE vaults SET gc_floor = 1 WHERE vault_id = ?1",
                    [conn.scope().as_slice()],
                )?;
                Ok(())
            })
            .expect("bump gc floor")
    });
    let log_uri = format!(
        "/v1/vaults/{}/log?after={}&limit=10",
        b64(&vault_id),
        genesis_cursor
    );
    let (status, body) = call(&app, get(&log_uri, Some(&token))).await;
    assert_eq!(status, StatusCode::GONE);
    assert_eq!(error_code(&body), "CURSOR_EXPIRED");
    drop(app);
    drop(db);
    support::cleanup_db_dir(&cleanup_path);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn control_members_expose_verifiable_certificate_signature() {
    let fx = block(|| Fixture::new("http-cert-sig"));
    let app = app(&fx);
    let token = block(|| fx.device_token());
    let vault = block(|| fx.vault());

    // 用真实 Ed25519 管理 key 签一张新设备证书（relay 的 dev hook 只做记账，客户端负责验签）。
    let signing = SigningKey::from_bytes(&[0x55u8; 32]);
    let manager_pub = signing.verifying_key().to_bytes();
    let device_id = [0x61u8; 16];
    let sign_pk = [0x62u8; 32];
    let hpke_pk = [0x63u8; 32];
    let certificate = verify::new_certificate(&vault.vault_id, &device_id, &sign_pk, &hpke_pk, 1);
    let certificate_bytes = certificate.encode().expect("certificate cde");
    let signed_bytes = proto::device_cert_signed_bytes(&certificate).expect("signed bytes");
    let certificate_signature = signing.sign(&signed_bytes).to_bytes().to_vec();

    // 邀请 → 兑换（PENDING）。
    let invite_token = [0x71u8; 32];
    let invite_hash = relay::ids::sha256(&invite_token);
    let ctx = fx.manager_ctx();
    let db = fx.db.clone();
    let clock = fx.clock.clone();
    let hook = fx.hook.clone();
    let control_rev = vault.control_rev;
    let invite_id = block(move || {
        db.write_blocking(move |conn| {
            pairing::create_invite(
                conn,
                clock.as_ref(),
                hook.as_ref(),
                &ctx,
                &pairing::InviteRequest {
                    token_hash: invite_hash,
                    requested_expires_at: None,
                    expected_control_rev: control_rev,
                    request_id: [0x72u8; 16],
                    path: "/v1/vaults/1/invites".to_string(),
                    unsigned_body: b"invite".to_vec(),
                    signature: b"sig".to_vec(),
                },
            )
        })
        .expect("invite")
        .invite_id
    });
    let db = fx.db.clone();
    let clock = fx.clock.clone();
    let hook = fx.hook.clone();
    block(move || {
        db.write_blocking(move |conn| {
            pairing::redeem_invite(
                conn,
                clock.as_ref(),
                hook.as_ref(),
                &invite_id,
                &invite_token,
                &pairing::NewDevice {
                    device_id,
                    sign_pk,
                    hpke_pk,
                    proof: b"proof".to_vec(),
                    proof_signature: b"proof-sig".to_vec(),
                },
            )
        })
        .expect("redeem")
    });

    // 批准（证书签名传给 relay 持久化）。
    let ctx = fx.manager_ctx();
    let db = fx.db.clone();
    let clock = fx.clock.clone();
    let hook = fx.hook.clone();
    let stored_signature = certificate_signature.clone();
    let control_rev = fx.vault().control_rev;
    block(move || {
        db.write_blocking(move |conn| {
            pairing::approve_device(
                conn,
                clock.as_ref(),
                hook.as_ref(),
                &ctx,
                &device_id,
                &pairing::ApproveRequest {
                    certificate: certificate_bytes,
                    certificate_signature: stored_signature,
                    grant_id: [0x73u8; 16],
                    hpke_enc: [0x74u8; 32],
                    hpke_ciphertext: b"hpke-ciphertext".to_vec(),
                    grant_signature: b"grant-sig".to_vec(),
                    expected_control_rev: control_rev,
                    request_id: [0x75u8; 16],
                    path: "/v1/vaults/1/devices/2/approve".to_string(),
                    unsigned_body: b"approve".to_vec(),
                    management_signature: b"mgmt-sig".to_vec(),
                },
            )
        })
        .expect("approve")
    });

    // /control members 键 5 = 证书签名。
    let (status, body) = call(
        &app,
        get(
            &format!("/v1/vaults/{}/control?after_rev=0", b64(&fx.vault_id)),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let control = cbor_response(&body);
    let members = wire::array_field(&control, 3).expect("members");
    let member = members
        .iter()
        .map(|member| wire::value_map(member).expect("member"))
        .find(|member| wire::id16_field(member, 0).expect("device id") == device_id)
        .expect("approved member");
    assert_eq!(
        member
            .iter()
            .find(|(key, _)| *key == 5)
            .map(|(_, value)| value),
        Some(&wire::bytes(certificate_signature.clone()))
    );

    // 客户端 §17.1 验签必须通过。
    let expect = Expectation::DeviceCert { cert: &certificate };
    let check = SignatureCheck {
        domain: SignDomain::DeviceApprove,
        vault_id: &vault.vault_id,
        device_id: Some(&device_id),
        public_key: &manager_pub,
        message: &[],
        signature: &certificate_signature,
        expect: &expect,
    };
    Ed25519Hook
        .verify(&check)
        .expect("verifiable certificate signature");

    // 篡改签名 → 客户端必须拒绝（relay 只存不验）。
    let mut tampered = certificate_signature.clone();
    tampered[0] ^= 0xFF;
    let bad = SignatureCheck {
        signature: &tampered,
        ..check
    };
    assert!(Ed25519Hook.verify(&bad).is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v9_database_migrates_to_10_and_legacy_v1_auth_still_serves() {
    // 真 schema 9 库（单 vault + 全量租户数据）→ 迁移到 10 后走真实 HTTP：
    // legacy 默认 vault 解析、v1 challenge/token、/state 与 /control 都能服务旧 vault。
    let path = temp_db_path("http-migrate-v9");
    let cleanup_path = path.clone();
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    let conn = support::v9::fixture(&path);
    // 旧行 NULL 签名必须保持 NULL（迁移不得补造签名）。
    conn.execute(
        "UPDATE devices SET certificate_signature = NULL WHERE device_id = ?1",
        [support::v9::DEVICE_ACTIVE.as_slice()],
    )
    .expect("null cert signature");
    conn.execute("UPDATE checkpoints SET commit_signature = NULL", [])
        .expect("null checkpoint signature");
    drop(conn);

    let clock = Arc::new(FakeClock::new(BASE_TIME));
    let cfg = Arc::new(RelayConfig::new(path.clone()).with_setup_secret("setup-secret"));
    let db = Db::open(cfg.db.clone(), clock).expect("migrate to v10");
    let (version, has_column, has_checkpoint_signature, has_revoked_once, signature) =
        block(|| {
            db.read_instance(|conn| {
                let version =
                    relay::db::schema::meta_text(conn, relay::db::schema::META_SCHEMA_VERSION)?;
                let columns_of =
                    |table: &str| -> std::result::Result<Vec<String>, rusqlite::Error> {
                        let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
                        let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
                        rows.collect()
                    };
                let device_columns = columns_of("devices")?;
                let checkpoint_columns = columns_of("checkpoints")?;
                let epoch_columns = columns_of("epochs")?;
                let signature: Option<Vec<u8>> = conn.query_row(
                    "SELECT certificate_signature FROM devices WHERE device_id = ?1",
                    [support::v9::DEVICE_ACTIVE.as_slice()],
                    |row| row.get(0),
                )?;
                Ok((
                    version,
                    device_columns
                        .iter()
                        .any(|column| column == "certificate_signature"),
                    checkpoint_columns
                        .iter()
                        .any(|column| column == "commit_signature"),
                    epoch_columns.iter().any(|column| column == "revoked_once"),
                    signature,
                ))
            })
            .expect("migration state")
        });
    assert_eq!(version, Some(relay::db::schema::SCHEMA_VERSION.to_string()));
    assert!(has_column, "迁移不得丢 devices.certificate_signature");
    assert!(
        has_checkpoint_signature,
        "迁移不得丢 checkpoints.commit_signature"
    );
    assert!(has_revoked_once, "迁移不得丢 epochs.revoked_once");
    assert!(signature.is_none(), "旧行证书签名保持 NULL");
    assert_eq!(
        block(|| db.read_instance(relay::db::schema::list_vault_ids)).expect("vaults"),
        vec![support::v9::VAULT],
        "迁移后只有一个 vault 行"
    );

    // v1 旧 API：无 Bearer → 401；legacy 默认 vault 解析成功即可挑战/签发 token。
    let app = router(AppState::new(
        db.clone(),
        cfg.clone(),
        Arc::new(TestHook::new()),
    ));
    let (status, _) = call(
        &app,
        get(
            &format!("/v1/vaults/{}/state", b64(&support::v9::VAULT)),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, body) = call(
        &app,
        post_cbor(
            "/v1/auth/challenges",
            None,
            vec![
                (0, wire::uint(1)),
                (1, wire::bytes(support::v9::DEVICE_ACTIVE)),
            ],
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "v1 认证必须仍绑定 legacy 默认 vault"
    );
    let challenge = cbor_response(&body);
    let challenge_id = wire::id16_field(&challenge, 0).expect("challenge id");
    assert_eq!(
        wire::bytes_field(&challenge, 5).expect("vault id"),
        support::v9::VAULT.to_vec()
    );
    assert_eq!(
        wire::bytes_field(&challenge, 6).expect("instance id"),
        support::v9::INSTANCE.to_vec()
    );

    let (status, body) = call(
        &app,
        post_cbor(
            "/v1/auth/tokens",
            None,
            vec![
                (0, wire::bytes(challenge_id.to_vec())),
                (1, wire::bytes(b"challenge-message")),
                (2, wire::bytes(b"signature")),
            ],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "v1 token 仍可签发");
    let token = wire::text_field(&cbor_response(&body), 0).expect("token");

    // /state：迁移保留的 rev/水位与 v9 行逐项一致。
    let uri = format!("/v1/vaults/{}/state", b64(&support::v9::VAULT));
    let (status, body) = call(&app, get(&uri, Some(&token))).await;
    assert_eq!(status, StatusCode::OK);
    let state = cbor_response(&body);
    assert_eq!(wire::uint_field(&state, 4).expect("epoch"), 1);
    assert_eq!(wire::uint_field(&state, 5).expect("control_rev"), 2);
    assert_eq!(wire::uint_field(&state, 6).expect("auth_rev"), 4);
    assert_eq!(wire::uint_field(&state, 7).expect("head_seq"), 3);
    assert_eq!(wire::uint_field(&state, 8).expect("gc_floor"), 1);
    assert_eq!(wire::uint_field(&state, 9).expect("normal_bytes"), 600);
    assert_eq!(
        wire::uint_field(&state, 10).expect("maintenance_bytes"),
        400
    );

    // /control：旧行 NULL 签名省略而不是 panic/写空。
    let (status, body) = call(
        &app,
        get(
            &format!(
                "/v1/vaults/{}/control?after_rev=0",
                b64(&support::v9::VAULT)
            ),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let members = wire::array_field(&cbor_response(&body), 3).expect("members");
    let genesis_member = wire::value_map(&members[0]).expect("member");
    assert!(
        wire::opt_field(&genesis_member, 5).is_none(),
        "NULL 签名必须省略而不是 panic/写空"
    );
    drop(app);
    drop(db);
    support::cleanup_db_dir(&cleanup_path);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bootstrap_entries_carry_verifiable_commit_and_signature() {
    let fx = block(|| Fixture::new("http-bootstrap-signed"));
    let older = block(|| fx.publish_signed_baseline());
    let newer = block(|| fx.publish_signed_baseline());
    let token = block(|| fx.device_token());
    let app = app(&fx);
    let uri = format!("/v1/vaults/{}/bootstrap", b64(&fx.vault_id));

    let (status, body) = call(&app, post_cbor(&uri, Some(&token), Vec::new())).await;
    assert_eq!(status, StatusCode::OK);
    let fields = cbor_response(&body);
    let primary = wire::map_field(&fields, 0).expect("primary");
    let fallback = wire::map_field(&fields, 1).expect("fallback");
    let verifying =
        ed25519_dalek::VerifyingKey::from_bytes(&fx.device_pubkey()).expect("device pubkey");
    for (entry, expected_id) in [(&primary, newer), (&fallback, older)] {
        assert_eq!(
            wire::bytes_field(entry, 0)
                .expect("checkpoint_id")
                .as_slice(),
            expected_id.as_slice()
        );
        // §20.1：键 6 = signed_commit、键 7 = 64B 生产者签名。
        let commit_bytes = wire::bytes_field(entry, 6).expect("signed_commit key 6");
        let signature_bytes = wire::bytes_field(entry, 7).expect("signature key 7");
        assert_eq!(signature_bytes.len(), 64, "签名必须是 64B Ed25519");
        let commit =
            relay::proto::CheckpointCommit::decode(&commit_bytes).expect("decode CheckpointCommit");
        assert_eq!(commit.checkpoint_id, expected_id);
        assert_eq!(commit.producer_device_id, fx.device);
        let signing_input =
            relay::proto::checkpoint_signing_input(&commit).expect("checkpoint signing input");
        let signature =
            ed25519_dalek::Signature::from_slice(&signature_bytes).expect("signature encoding");
        verifying
            .verify_strict(&signing_input, &signature)
            .expect("bootstrap 返回的 commit+signature 必须可验签");
        // 客户端交叉校验：S / manifest_oid / refs_digest 必须与被签名 commit 一致。
        assert_eq!(
            wire::uint_field(entry, 2).expect("covered_seq"),
            commit.covered_relay_seq
        );
        assert_eq!(
            wire::bytes_field(entry, 3)
                .expect("manifest_oid")
                .as_slice(),
            commit.manifest_object_id.as_slice()
        );
        assert_eq!(
            wire::bytes_field(entry, 4).expect("refs_digest").as_slice(),
            commit.refs_digest.as_slice()
        );
    }

    // 旧 checkpoint 没有签名：键 6 保留、键 7 省略（客户端按"无签名"拒绝，不能静默通过）。
    block(|| {
        let handle = fx.db.clone();
        handle
            .write_blocking(move |conn| {
                conn.execute(
                    "UPDATE checkpoints SET commit_signature = NULL WHERE checkpoint_id = ?1",
                    [older.as_slice()],
                )?;
                Ok(())
            })
            .expect("clear legacy signature");
    });
    let (status, body) = call(&app, post_cbor(&uri, Some(&token), Vec::new())).await;
    assert_eq!(status, StatusCode::OK);
    let fields = cbor_response(&body);
    let fallback = wire::map_field(&fields, 1).expect("fallback");
    assert_eq!(
        wire::bytes_field(&fallback, 0)
            .expect("checkpoint_id")
            .as_slice(),
        older.as_slice()
    );
    assert!(
        wire::bytes_field(&fallback, 6).is_ok(),
        "signed_commit 仍应保留"
    );
    assert!(
        wire::opt_field(&fallback, 7).is_none(),
        "无签名的旧 checkpoint 必须省略键 7"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_key_slot_object_is_410_even_when_id_is_known() {
    let fx = block(|| Fixture::new("http-slot-expired"));
    let slot_id = block(|| fx.put_staging_slot(relay::slots::SlotKind::Password));
    block(|| {
        fx.db
            .write_blocking(move |conn| {
                conn.execute(
                    "UPDATE key_slots SET state = 'retiring', retire_at = ?1 WHERE slot_id = ?2",
                    (BASE_TIME + 1, slot_id.as_slice()),
                )?;
                Ok(())
            })
            .expect("retire slot");
    });
    let app = app(&fx);
    let token = block(|| fx.device_token());
    let uri = format!("/v1/vaults/{}/objects/{}", b64(&fx.vault_id), b64(&slot_id));
    let (status, _) = call(&app, get(&uri, Some(&token))).await;
    assert_eq!(status, StatusCode::OK, "retire_at 之前仍可读");
    block(|| fx.clock.advance_secs(2));
    let (status, body) = call(&app, get(&uri, Some(&token))).await;
    assert_eq!(status, StatusCode::GONE, "过期槽对象 GET 必须 410");
    let fields = cbor_response(&body);
    assert_eq!(
        wire::text_field(&fields, 0).expect("code"),
        "RETIRED",
        "错误码应是 RETIRED"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn control_exposes_manager_transition_with_dual_signatures() {
    let fx = block(|| Fixture::new("http-control-manager"));
    block(|| {
        fx.create_object(relay::envelope::ObjectType::Update, 1, 1, 32);
        let view = fx.begin_rotation(&[]);
        let rotation_id = view.rotation.rotation_id;
        let (_a, _b) = fx.publish_candidate_baselines(2);
        let reservation = fx.rotation_reservation(&rotation_id);
        fx.stage_candidate_slots(2, &reservation);
        fx.stage_grant(&rotation_id, &fx.device);
        fx.activate_rotation(&rotation_id)
            .expect("activate with dual-signed manager");
    });
    let token = block(|| fx.device_token());
    let app = app(&fx);
    let uri = format!("/v1/vaults/{}/control?after_rev=0", b64(&fx.vault_id));
    let (status, body) = call(&app, get(&uri, Some(&token))).await;
    assert_eq!(status, StatusCode::OK);
    let fields = cbor_response(&body);
    let chain = wire::array_field(&fields, 5).expect("manager chain");
    assert_eq!(chain.len(), 2, "rev1 genesis + rev2 transition");
    let entry = wire::value_map(&chain[1]).expect("rev2 entry");
    assert_eq!(wire::uint_field(&entry, 0).expect("rev"), 2);
    assert_eq!(wire::uint_field(&entry, 3).expect("old_rev"), 1);
    let transition_bytes = wire::bytes_field(&entry, 2).expect("transition");
    let transition = relay::rotations::ManagerTransition::decode(&transition_bytes)
        .expect("manager transition cde");
    assert_eq!(transition.new_rev, 2);
    assert_eq!(transition.old_rev, 1);
    assert_eq!(transition.vault_id, fx.vault_id);
    assert!(!wire::bytes_field(&entry, 4).expect("old sig").is_empty());
    assert!(!wire::bytes_field(&entry, 5).expect("new sig").is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn key_slots_realm_visibility_hides_staging_from_devices() {
    let fx = block(|| Fixture::new("http-key-slots-realm"));
    let staging = block(|| fx.put_staging_slot(relay::slots::SlotKind::Recovery));
    let device_token = block(|| fx.device_token());
    let manager_token = block(|| fx.manager_token());
    let app = app(&fx);
    let uri = format!("/v1/vaults/{}/key-slots", b64(&fx.vault_id));

    // device realm：只能看到 active / retiring。
    let (status, body) = call(&app, get(&uri, Some(&device_token))).await;
    assert_eq!(status, StatusCode::OK);
    let entries = wire::array_field(&cbor_response(&body), 0).expect("slots array");
    for entry in &entries {
        let slot = wire::value_map(entry).expect("slot map");
        let state = wire::text_field(&slot, 4).expect("state");
        assert_ne!(state, "staging", "device realm 不得看到 staging");
        assert_ne!(
            wire::bytes_field(&slot, 0).expect("slot_id").as_slice(),
            staging.as_slice()
        );
    }

    // management realm：额外看到 staging（恢复设备自批要读 recovery 候选槽）。
    let (status, body) = call(&app, get(&uri, Some(&manager_token))).await;
    assert_eq!(status, StatusCode::OK);
    let entries = wire::array_field(&cbor_response(&body), 0).expect("slots array");
    let staging_entry = entries
        .iter()
        .map(|entry| wire::value_map(entry).expect("slot map"))
        .find(|slot| wire::bytes_field(slot, 0).expect("slot_id").as_slice() == staging.as_slice())
        .expect("management realm 必须看到 staging");
    assert_eq!(
        wire::text_field(&staging_entry, 4).expect("state"),
        "staging"
    );
    assert_eq!(
        wire::text_field(&staging_entry, 2).expect("kind"),
        "recovery"
    );
    assert!(
        !wire::bytes_field(&staging_entry, 3)
            .expect("descriptor")
            .is_empty(),
        "staging 条目必须带 descriptor"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn management_realm_reads_key_slot_objects_only() {
    let fx = block(|| Fixture::new("http-object-realm"));
    // KEY_SLOT 对象（已完成）+ 其他类型的已完成对象。
    let slot_id = block(|| fx.put_staging_slot(relay::slots::SlotKind::Password));
    let update_id = block(|| {
        fx.create_object(relay::envelope::ObjectType::Update, 1, 1, 32)
            .0
    });
    let snapshot_id = block(|| fx.create_data_object(relay::envelope::ObjectType::Snapshot, 64));
    let device_token = block(|| fx.device_token());
    let manager_token = block(|| fx.manager_token());
    let app = app(&fx);
    let vault = b64(&fx.vault_id);
    let object_uri = |id: &relay::ids::Hash32| format!("/v1/vaults/{vault}/objects/{}", b64(id));
    let part_uri =
        |id: &relay::ids::Hash32| format!("/v1/vaults/{vault}/objects/{}/parts/0", b64(id));

    // §22：management realm 可以只读 KEY_SLOT 的元数据 + 分片。
    let (status, body) = call(&app, get(&object_uri(&slot_id), Some(&manager_token))).await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&body));
    assert_eq!(
        wire::uint_field(&cbor_response(&body), 4).expect("object type"),
        5,
        "type 必须是 KEY_SLOT(5)"
    );
    let (status, body) = call(&app, get(&part_uri(&slot_id), Some(&manager_token))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body.is_empty(), "KEY_SLOT 分片必须可读");

    // 其他类型不放宽：UPDATE / SNAPSHOT → 403 MANAGEMENT_REQUIRED（元数据与分片一致）。
    for id in [&update_id, &snapshot_id] {
        let (status, body) = call(&app, get(&object_uri(id), Some(&manager_token))).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "management 读非 KEY_SLOT 必须 403"
        );
        assert_eq!(error_code(&body), "MANAGEMENT_REQUIRED");
        let (status, body) = call(&app, get(&part_uri(id), Some(&manager_token))).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(error_code(&body), "MANAGEMENT_REQUIRED");
    }

    // device realm 行为不变：KEY_SLOT 仍可读（元数据 + 分片）。
    let (status, body) = call(&app, get(&object_uri(&slot_id), Some(&device_token))).await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&body));
    let (status, _) = call(&app, get(&part_uri(&slot_id), Some(&device_token))).await;
    assert_eq!(status, StatusCode::OK);

    // 无 token → 401。
    let (status, body) = call(&app, get(&object_uri(&slot_id), None)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(error_code(&body), "UNAUTHORIZED");

    // 吊销设备：旧 device token 立即失效（403 REVOKED）；management 权限不依赖设备状态，
    // 重新签发的管理 token 仍能读 KEY_SLOT。
    block(|| {
        let handle = fx.db.clone();
        let device = fx.device;
        handle
            .write_blocking(move |conn| relay::auth::revoke_device(conn, BASE_TIME, &device))
            .expect("revoke fixture device");
    });
    let (status, body) = call(&app, get(&object_uri(&slot_id), Some(&device_token))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(error_code(&body), "REVOKED");
    let (status, body) = call(&app, get(&object_uri(&slot_id), Some(&manager_token))).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "吊销推进 auth_rev，旧 token 全失效"
    );
    assert_eq!(error_code(&body), "REVOKED");
    let manager_token = block(|| fx.manager_token());
    let (status, _) = call(&app, get(&object_uri(&slot_id), Some(&manager_token))).await;
    assert_eq!(status, StatusCode::OK, "新管理 token 仍可只读 KEY_SLOT");
}
