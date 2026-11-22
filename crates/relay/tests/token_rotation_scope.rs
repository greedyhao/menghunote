//! 评审 0002 §五 T14：begin/activate 前后旧 device / management token 必须对
//! **所有** read/write/slot/WS 端点立即失效；重新认证后的新 token 才能继续使用。
//!
//! 这里不把弱 `TestHook` 当作密码学证据：本用例只断言 **auth_rev 吊销语义与
//! 端点覆盖**（真实 SQLite + 真实 HTTP 路由 + 真实 WS 升级）；管理签名本身由
//! `management_body.rs` 用真实 Ed25519 覆盖。

mod support;

use std::time::Duration;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use futures_util::StreamExt;
use http_body_util::BodyExt;
use relay::api::{router, AppState};
use relay::wire;
use support::*;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tower::ServiceExt;

fn block<T>(f: impl FnOnce() -> T) -> T {
    tokio::task::block_in_place(f)
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

fn post(uri: &str, token: &str, body: Vec<u8>) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, wire::CONTENT_TYPE)
        .body(Body::from(body))
        .expect("request")
}

fn put(uri: &str, token: &str, body: Vec<u8>) -> Request<Body> {
    Request::builder()
        .method("PUT")
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, wire::CONTENT_TYPE)
        .body(Body::from(body))
        .expect("request")
}

fn b64(bytes: &[u8]) -> String {
    relay::ids::b64url_encode(bytes)
}

fn error_code(body: &[u8]) -> String {
    wire::text_field(&cbor_fields(body), 0).unwrap_or_default()
}

fn empty_cbor() -> Vec<u8> {
    cbor_body(Vec::new())
}

/// 旧 token 访问一个端点：必须 403 REVOKED，且响应不得泄露业务体。
async fn assert_revoked(app: &axum::Router, request: Request<Body>, label: &str) {
    let (status, body) = call(app, request).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "{label}: 旧 token 必须被拒绝，实际 {status} / {:?}",
        error_code(&body)
    );
    assert_eq!(error_code(&body), "REVOKED", "{label}");
}

/// 旧 device token 的读/写/slot 端点全集。
fn device_requests(vault: &[u8; 16], checkpoint: &[u8; 16]) -> Vec<(String, Request<Body>)> {
    let base = format!("/v1/vaults/{}", b64(vault));
    let object = b64(&[0xAB; 32]);
    let slot = b64(&[0xCD; 32]);
    let checkpoint = b64(checkpoint);
    vec![
        (
            "GET state".to_owned(),
            get(&format!("{base}/state"), "__TOKEN__"),
        ),
        (
            "GET control".to_owned(),
            get(&format!("{base}/control?after_rev=0"), "__TOKEN__"),
        ),
        (
            "GET log".to_owned(),
            get(&format!("{base}/log?after=c0&limit=10"), "__TOKEN__"),
        ),
        (
            "GET key-slots".to_owned(),
            get(&format!("{base}/key-slots"), "__TOKEN__"),
        ),
        (
            "GET objects".to_owned(),
            get(&format!("{base}/objects?type=asset&epoch=1"), "__TOKEN__"),
        ),
        (
            "GET object".to_owned(),
            get(&format!("{base}/objects/{object}"), "__TOKEN__"),
        ),
        (
            "GET checkpoint".to_owned(),
            get(&format!("{base}/checkpoints/{checkpoint}"), "__TOKEN__"),
        ),
        (
            "GET rotations/current".to_owned(),
            get(&format!("{base}/rotations/current"), "__TOKEN__"),
        ),
        (
            "GET latest grant".to_owned(),
            get(&format!("{base}/devices/self/grants/latest"), "__TOKEN__"),
        ),
        (
            "POST acks".to_owned(),
            post(&format!("{base}/acks"), "__TOKEN__", empty_cbor()),
        ),
        (
            "POST updates".to_owned(),
            post(&format!("{base}/updates"), "__TOKEN__", empty_cbor()),
        ),
        (
            "POST checkpoints".to_owned(),
            post(&format!("{base}/checkpoints"), "__TOKEN__", empty_cbor()),
        ),
        (
            "POST bootstrap".to_owned(),
            post(&format!("{base}/bootstrap"), "__TOKEN__", empty_cbor()),
        ),
        (
            "POST checkpoint refs".to_owned(),
            post(
                &format!("{base}/checkpoints/{checkpoint}/refs"),
                "__TOKEN__",
                empty_cbor(),
            ),
        ),
        (
            "POST checkpoint publish".to_owned(),
            post(
                &format!("{base}/checkpoints/{checkpoint}/publish"),
                "__TOKEN__",
                empty_cbor(),
            ),
        ),
        (
            "PUT part".to_owned(),
            put(
                &format!("{base}/objects/{object}/parts/0"),
                "__TOKEN__",
                vec![0u8; 32],
            ),
        ),
        (
            "POST object complete".to_owned(),
            post(
                &format!("{base}/objects/{object}/complete"),
                "__TOKEN__",
                empty_cbor(),
            ),
        ),
        (
            "PUT key-slot".to_owned(),
            put(
                &format!("{base}/key-slots/{slot}"),
                "__TOKEN__",
                empty_cbor(),
            ),
        ),
        (
            "POST key-slot activate".to_owned(),
            post(
                &format!("{base}/key-slots/password/activate"),
                "__TOKEN__",
                empty_cbor(),
            ),
        ),
    ]
}

/// 旧 management token 的管理写/管理读端点（auth_rev 必须先于 realm 判定生效）。
fn management_requests(
    vault: &[u8; 16],
    rotation: &[u8; 16],
    device: &[u8; 16],
) -> Vec<(String, Request<Body>)> {
    let base = format!("/v1/vaults/{}", b64(vault));
    let rotation = b64(rotation);
    let device = b64(device);
    vec![
        (
            "POST begin rotation".to_owned(),
            post(&format!("{base}/rotations"), "__TOKEN__", empty_cbor()),
        ),
        (
            "PUT stage grant".to_owned(),
            put(
                &format!("{base}/rotations/{rotation}/grants/{device}"),
                "__TOKEN__",
                empty_cbor(),
            ),
        ),
        (
            "POST activate rotation".to_owned(),
            post(
                &format!("{base}/rotations/{rotation}/activate"),
                "__TOKEN__",
                empty_cbor(),
            ),
        ),
        (
            "POST abort rotation".to_owned(),
            post(
                &format!("{base}/rotations/{rotation}/abort"),
                "__TOKEN__",
                empty_cbor(),
            ),
        ),
        (
            "POST invite".to_owned(),
            post(&format!("{base}/invites"), "__TOKEN__", empty_cbor()),
        ),
        (
            "POST approve device".to_owned(),
            post(
                &format!("{base}/devices/{device}/approve"),
                "__TOKEN__",
                empty_cbor(),
            ),
        ),
    ]
}

fn with_token(request: Request<Body>, token: &str) -> Request<Body> {
    let mut parts = request.into_parts();
    parts.0.headers.insert(
        header::AUTHORIZATION,
        format!("Bearer {token}").parse().expect("header"),
    );
    Request::from_parts(parts.0, parts.1)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn old_device_and_management_tokens_are_dead_on_every_endpoint_after_begin_and_activate() {
    let mut fx = block(|| Fixture::new("token-scope"));
    let app = router(AppState::new(
        fx.db.clone(),
        fx.cfg.clone(),
        fx.hook.clone(),
    ));
    let vault = fx.vault_id;
    // 先有一份已发布基线（给出真实 checkpoint id；不依赖它即可，但让 GET 指向真实行）。
    let checkpoint = block(|| fx.publish_baseline());
    // 旧 token 在 begin 之前签发（之后 begin/activate 会推进 auth_rev）。
    let old_device_token = block(|| fx.device_token());
    let old_manager_token = block(|| fx.manager_token());

    let base = format!("/v1/vaults/{}", b64(&vault));
    let (status, _) = call(&app, get(&format!("{base}/state"), &old_device_token)).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call(
        &app,
        get(
            &format!("{base}/checkpoints/{}", b64(&checkpoint)),
            &old_device_token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = call(
        &app,
        get(&format!("{base}/control?after_rev=0"), &old_manager_token),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "begin 前管理 token 应可用：code={:?}",
        error_code(&body)
    );

    let rotation = block(|| fx.begin_rotation(&[]).rotation.rotation_id);

    // begin 已推进 auth_rev：旧 device + 旧 management token 对所有端点都必须失效。
    for (label, request) in device_requests(&vault, &checkpoint) {
        assert_revoked(&app, with_token(request, &old_device_token), &label).await;
    }
    for (label, request) in management_requests(&vault, &rotation, &fx.device) {
        assert_revoked(&app, with_token(request, &old_manager_token), &label).await;
    }
    // 旧 management token 的读端点同样失效（auth_rev 在 realm 判定之前）。
    for uri in [
        format!("{base}/state"),
        format!("{base}/control?after_rev=0"),
        format!("{base}/key-slots"),
        format!("{base}/rotations/current"),
    ] {
        assert_revoked(&app, get(&uri, &old_manager_token), &uri).await;
    }

    // 重新认证后的新 token 可用；begin 之后再认证的 token 在 activate 之后同样失效。
    let after_begin_device = block(|| fx.device_token());
    let (status, _) = call(&app, get(&format!("{base}/state"), &after_begin_device)).await;
    assert_eq!(status, StatusCode::OK, "begin 后重新认证必须可用");
    let after_begin_manager = block(|| fx.manager_token());
    let (status, _) = call(
        &app,
        get(&format!("{base}/control?after_rev=0"), &after_begin_manager),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "begin 后重新认证的管理 token 必须可用"
    );

    // 完成一次真实 activate（候选基线 + 槽 + grant），它会再次推进 auth_rev / 轮换管理 key。
    block(|| {
        let _ = fx.publish_candidate_baselines(2);
        let reservation = fx.rotation_reservation(&rotation);
        fx.stage_candidate_slots(2, &reservation);
        fx.stage_grant(&rotation, &fx.device);
        fx.activate_rotation(&rotation).expect("activate")
    });
    assert_eq!(block(|| fx.vault()).active_epoch, 2);
    fx.manager_pub = [0x5A; 32];

    // activate 后：begin 之前与 begin 之后的 token 全部失效。
    for token in [
        &old_device_token,
        &after_begin_device,
        &old_manager_token,
        &after_begin_manager,
    ] {
        assert_revoked(
            &app,
            get(&format!("{base}/state"), token),
            "activate 后 state",
        )
        .await;
    }
    for (label, request) in device_requests(&vault, &checkpoint) {
        assert_revoked(&app, with_token(request, &after_begin_device), &label).await;
    }
    for (label, request) in management_requests(&vault, &rotation, &fx.device) {
        assert_revoked(&app, with_token(request, &after_begin_manager), &label).await;
    }

    // activate 后重新认证：新 device token 与**新管理 key** token 可用。
    let new_device_token = block(|| fx.device_token());
    let (status, _) = call(&app, get(&format!("{base}/state"), &new_device_token)).await;
    assert_eq!(status, StatusCode::OK, "activate 后新设备 token 必须可用");
    let new_manager_token = block(|| fx.manager_token());
    let (status, body) = call(
        &app,
        get(&format!("{base}/control?after_rev=0"), &new_manager_token),
    )
    .await;
    let manager_pub = block(|| {
        fx.db
            .read(relay::vault::manager_pubkey)
            .expect("manager pub")
    });
    assert_eq!(
        status,
        StatusCode::OK,
        "activate 后新管理 token 必须可用：code={:?} manager_pub={manager_pub:?}",
        error_code(&body)
    );

    // WS /wake：旧 token 连升级都建立不了；新 token 能收到 epoch=2 水位。
    let state = AppState::new(fx.db.clone(), fx.cfg.clone(), fx.hook.clone());
    let ws_app = router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, ws_app).await;
    });
    let wake_uri = format!("ws://{}/v1/vaults/{}/wake", addr, b64(&vault));
    for token in [&old_device_token, &after_begin_device, &old_manager_token] {
        let mut request = wake_uri.clone().into_client_request().expect("ws request");
        request.headers_mut().insert(
            "Authorization",
            format!("Bearer {token}").parse().expect("header"),
        );
        assert!(
            tokio_tungstenite::connect_async(request).await.is_err(),
            "旧 token 不得建立 WS 升级"
        );
    }
    let mut request = wake_uri.into_client_request().expect("ws request");
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {new_device_token}")
            .parse()
            .expect("header"),
    );
    let (mut socket, _) = tokio_tungstenite::connect_async(request)
        .await
        .expect("新 token 应建立 WS");
    let first = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .expect("initial frame timeout")
        .expect("initial frame")
        .expect("initial frame ok");
    let Message::Text(text) = first else {
        panic!("期望文本水位帧，实际 {first:?}");
    };
    let value: serde_json::Value = serde_json::from_str(&text).expect("wake json");
    assert_eq!(value["epoch"], 2, "WS 必须看到 activate 后的新 epoch");
    assert_eq!(value["v"], 1);
}
