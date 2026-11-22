//! 评审 0002 §五 T13（relay 侧）：未吊销设备离线 90 天、跨多次轮换、GC 7 天 /
//! 槽 14 天后，最新 grant 仍可读取（grant 无 TTL）；被吊销设备不得被自动重加。
//!
//! 时间全用 FakeClock 推进（无 sleep）；设备 B 在整段过程中不发起任何请求，直到
//! 90 天后才重新认证并读取 latest grant。客户端 HPKE 解密/merge 未上传编辑由
//! `crates/sync/tests/*` 的真实 HPKE/Loro 用例覆盖（本 crate 不依赖 sync）。

mod support;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use relay::api::{router, AppState};
use relay::auth::{self, Realm};
use relay::ids::Id16;
use relay::wire;
use support::*;
use tower::ServiceExt;

const DAY: i64 = 86_400;

fn block<T>(f: impl FnOnce() -> T) -> T {
    tokio::task::block_in_place(f)
}

async fn get(app: &axum::Router, uri: &str, token: &str) -> (StatusCode, Vec<u8>) {
    let request = Request::builder()
        .method("GET")
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .expect("request");
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

fn b64(bytes: &[u8]) -> String {
    relay::ids::b64url_encode(bytes)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn offline_device_gets_latest_grant_after_ninety_days_and_gc_but_revoked_is_not_readded() {
    let fx = block(|| Fixture::new("offline-grant"));
    let offline: Id16 = [0x33; 16];
    let revoked: Id16 = [0x44; 16];
    block(|| {
        fx.add_device(offline);
        fx.add_device(revoked);
    });

    // 所有 fixture 同步调用放进 block_in_place（SQLite 写是阻塞的）。
    let token = block(|| {
        // 第一次轮换：显式吊销 C；B 之后完全离线（不认证、不读 grant）。
        let rotation2 = fx
            .begin_rotation_at(&[revoked], [0x21; 16])
            .rotation
            .rotation_id;
        fx.publish_candidate_baselines(2);
        let reservation = fx.rotation_reservation(&rotation2);
        fx.stage_candidate_slots(2, &reservation);
        fx.stage_grant(&rotation2, &fx.device);
        fx.stage_grant(&rotation2, &offline);
        fx.activate_rotation_at(&rotation2, [0x22; 16])
            .expect("activate epoch 2");
        assert_eq!(fx.vault().active_epoch, 2);

        // 第二次轮换：被吊销的 C 不得被自动重加（stage 直接 REVOKED），B 仍只离线持有旧根。
        let rotation3 = fx.begin_rotation_at(&[], [0x23; 16]).rotation.rotation_id;
        // Fixture::stage_grant 用 expect；这里直接走核心函数以断言错误码。
        let ctx = fx.manager_ctx();
        let manager_rev = fx.vault().manager_rev;
        let hook = fx.hook.clone();
        let clock = fx.clock.clone();
        let db = fx.db.clone();
        let stage_error = db
            .write_blocking(move |conn| {
                relay::rotations::stage_grant(
                    conn,
                    hook.as_ref(),
                    &ctx,
                    &rotation3,
                    &revoked,
                    &relay::rotations::GrantInput {
                        grant_id: [0x55; 16],
                        hpke_enc: [1u8; 32],
                        hpke_ciphertext: b"grant-ciphertext",
                        signed_header: b"aad||enc||hash",
                        signature: b"signature",
                        manager_rev,
                    },
                    clock.as_ref(),
                )
            })
            .expect_err("被吊销设备不得重新 stage grant");
        assert_eq!(
            stage_error.code(),
            Some(relay::error::ErrorCode::Revoked),
            "{stage_error:?}"
        );
        fx.publish_candidate_baselines(3);
        let reservation = fx.rotation_reservation(&rotation3);
        fx.stage_candidate_slots(3, &reservation);
        fx.stage_grant(&rotation3, &fx.device);
        fx.stage_grant(&rotation3, &offline);
        fx.activate_rotation_at(&rotation3, [0x24; 16])
            .expect("activate epoch 3");
        assert_eq!(fx.vault().active_epoch, 3);

        // B 离线整整 90 天；期间反复 GC（超过 7 天 checkpoint 宽限与 14 天槽 TTL）。
        fx.clock.advance_secs(90 * DAY);
        for _ in 0..3 {
            let _ = fx.gc();
            fx.clock.advance_secs(DAY);
        }

        // C 被吊销：连 challenge 都拿不到；B 未吊销：可重新认证。
        let clock = fx.clock.clone();
        let db = fx.db.clone();
        let revoked_challenge = db.write_blocking(move |conn| {
            auth::create_challenge(conn, clock.as_ref(), Realm::Device, &revoked)
        });
        assert!(
            revoked_challenge.is_err(),
            "被吊销设备必须 REVOKED，不能自动重加"
        );
        assert!(
            fx.latest_grant(&revoked).is_none(),
            "被吊销设备不得有最新 grant"
        );

        let grant = fx.latest_grant(&offline).expect("B 的最新 grant 必须保留");
        assert_eq!(grant.epoch, 3, "grant 必须指向当前 active epoch");
        assert_eq!(grant.device_id, offline);
        assert!(
            !grant.hpke_ciphertext.is_empty(),
            "grant 密文不得被 TTL 清掉"
        );
        assert!(!grant.signed_header.is_empty());
        assert!(grant.signature.is_some());
        assert!(
            grant.activation_ref.is_some(),
            "grant 应保留其激活来源 rotation"
        );

        // 旧槽在 14 天 TTL 后已被回收；grant 不是槽，仍可读。
        let old_slots: i64 = fx
            .db
            .read(|conn| {
                Ok(conn.query_row(
                    "SELECT COUNT(*) FROM key_slots WHERE epoch <> 3",
                    [],
                    |row| row.get(0),
                )?)
            })
            .expect("old slots");
        assert_eq!(old_slots, 0, "旧 epoch 槽必须在 14 天 TTL 后回收");
        let active_slots: i64 = fx
            .db
            .read(|conn| {
                Ok(conn.query_row(
                    "SELECT COUNT(*) FROM key_slots WHERE epoch = 3",
                    [],
                    |row| row.get(0),
                )?)
            })
            .expect("active slots");
        assert!(active_slots > 0, "新 epoch 的槽必须仍在");

        fx.device_token_for(&offline)
    });
    // 90 天后 B 重新认证并通过真实 HTTP 读 latest grant；bootstrap 仍返回 active 对。
    let app = router(AppState::new(
        fx.db.clone(),
        fx.cfg.clone(),
        Arc::new(relay::verify::Ed25519Hook),
    ));
    let uri = format!(
        "/v1/vaults/{}/devices/self/grants/latest",
        b64(&fx.vault_id)
    );
    let (status, body) = get(&app, &uri, &token).await;
    assert_eq!(status, StatusCode::OK, "B 的 latest grant 必须可读");
    let fields = cbor_fields(&body);
    assert_eq!(wire::uint_field(&fields, 0).expect("grant epoch"), 3);
    let grant_device: [u8; 16] = wire::bytes_field(&fields, 1)
        .expect("grant device")
        .as_slice()
        .try_into()
        .expect("16B device id");
    assert_eq!(grant_device, offline);
    assert!(
        !wire::bytes_field(&fields, 4)
            .expect("grant ciphertext")
            .is_empty(),
        "HTTP grant 密文必须保留"
    );
    let bundle = block(|| fx.bootstrap().expect("90 天后 bootstrap 仍可用"));
    assert_eq!(bundle.epoch, 3);
}
