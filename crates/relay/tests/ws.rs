//! `/wake` WebSocket：初始水位推送 + 吊销立即断开（ADR 0006 §5 WS、§9 控制面）。

mod support;

use std::time::Duration;

use futures_util::StreamExt;
use relay::api::{router, AppState};
use support::*;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

fn block<T>(f: impl FnOnce() -> T) -> T {
    tokio::task::block_in_place(f)
}

fn b64(bytes: &[u8]) -> String {
    relay::ids::b64url_encode(bytes)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wake_pushes_state_and_closes_immediately_on_revoke() {
    let fx = block(|| Fixture::new("ws-wake"));
    let state = AppState::new(fx.db.inner().clone(), fx.cfg.clone(), fx.hook.clone());
    let app = router(state.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let token = block(|| fx.device_token());
    let uri = format!("ws://{}/v1/vaults/{}/wake", addr, b64(&fx.vault_id));
    let mut request = uri.clone().into_client_request().expect("ws request");
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {token}").parse().expect("header"),
    );
    let (mut socket, _) = tokio_tungstenite::connect_async(request)
        .await
        .expect("ws connect");

    // 初始帧必须是 {v:1,head_seq,epoch,control_rev} 小 JSON。
    let first = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .expect("initial frame timeout")
        .expect("initial frame")
        .expect("initial frame ok");
    let text = match first {
        Message::Text(text) => text.to_string(),
        other => panic!("unexpected frame: {other:?}"),
    };
    let value: serde_json::Value = serde_json::from_str(&text).expect("wake json");
    assert_eq!(value["v"], 1);
    assert_eq!(value["epoch"], 1);
    assert_eq!(value["head_seq"], 0);
    assert!(value["control_rev"].as_u64().is_some());

    // 无效 token 不能建立 WS（升级前 401）。
    let mut bad_request = uri.into_client_request().expect("ws request");
    bad_request.headers_mut().insert(
        "Authorization",
        "Bearer AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
            .parse()
            .expect("header"),
    );
    let bad = tokio_tungstenite::connect_async(bad_request).await;
    assert!(bad.is_err(), "无效 token 必须拒绝升级");

    // 吊销设备 → 广播 → 服务端立即关闭连接（close code 1008）。
    let device = fx.device;
    let now = block(|| fx.clock_now());
    block(|| {
        fx.db
            .write_blocking(move |conn| relay::auth::revoke_device(conn, now, &device))
            .expect("revoke")
    });
    state.notify();

    let closed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match socket.next().await {
                None => return true,
                Some(Ok(Message::Close(_))) => return true,
                Some(Err(_)) => return true,
                Some(Ok(_)) => continue,
            }
        }
    })
    .await
    .expect("ws close timeout");
    assert!(closed, "吊销后连接必须断开");
}
