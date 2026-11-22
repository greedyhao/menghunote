//! ADR 0015 **S4b1**：`CapGrant`/`CapRevoke`/`InviteCreate` 与撤销后的 token/WS 失效。
//!
//! 覆盖：
//! 1. 两签 CapGrant：正例 + 单签/同设备/超 scope/过期 cap/错 authority/自升权/错 rev/
//!    重复 cap id/错签名全部拒绝；授权变化推进 authz/auth 并让旧 bearer 失效；
//! 2. CapRevoke：两签成功、写 `revoked_control_rev`、authz/auth 前进、Bearer 与 WS 立即失效；
//!    重复撤销 409、未知 cap 404、单签 403；
//! 3. InviteCreate：一台 INVITE cap、TTL/scope/重复 id/双签/跨 vault/签发 cap 撤销后兑换失效；
//! 4. 全部走真实 relay 路由 + 真实 Ed25519；无 sleep。

mod support;

use std::time::Duration;

use axum::http::StatusCode;
use axum::Router;
use ed25519_dalek::{Signer as _, SigningKey};
use futures_util::StreamExt;
use relay::ids::{b64url_encode, sha256, Id16};
use relay::wire;
use support::v2::*;
use support::*;
use tokio_tungstenite::tungstenite::Message;

use menghunote_sync_proto as proto;

fn new_fixture(label: &str) -> V2Fixture {
    tokio::task::block_in_place(|| V2Fixture::new(label))
}

fn assert_error(body: &[u8], expected: &str) {
    assert_eq!(
        error_code(body),
        expected,
        "body code = {}",
        error_code(body)
    );
}

fn admin_signers(v2: &V2Fixture) -> [(Id16, Id16); 2] {
    [(v2.extra[0].0, [0xC1; 16]), (v2.extra[1].0, [0xC2; 16])]
}

fn control_count(v2: &V2Fixture) -> i64 {
    v2.fx
        .db
        .read(|conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM control_commits WHERE vault_id = ?1",
                [conn.scope().as_slice()],
                |row| row.get(0),
            )?)
        })
        .expect("control count")
}

/// 构造一张"即将被授予"的 cap（issued rev 绑定当前提交后值）。
fn grant_cap(
    v2: &V2Fixture,
    cap_id: [u8; 16],
    device_id: Id16,
    scopes: u32,
    ttl_secs: u64,
) -> proto::ManagementCapV2 {
    let vault = v2.vault();
    let (head_rev, _) = v2.head();
    proto::ManagementCapV2 {
        version: proto::V2_WIRE_VERSION,
        instance_id: vault.instance_id,
        vault_id: vault.vault_id,
        cap_id,
        subject_device_id: device_id,
        subject_ed_pk: v2.sign_pk(&device_id),
        scopes: proto::CapScope::new(scopes).expect("scopes"),
        not_before: v2.now_u64(),
        expires_at: v2.now_u64() + ttl_secs,
        issued_control_rev: head_rev + 1,
        issued_authz_rev: vault.authz_rev + 1,
        issuer_authority_rev: vault.authority_rev,
        delegable: false,
    }
}

fn grant_operation(
    v2: &V2Fixture,
    request_id: [u8; 16],
    cap: proto::ManagementCapV2,
    signers: &[(Id16, Id16)],
    signed_at: u64,
) -> proto::OperationV2 {
    v2.two_signer_operation(
        proto::ControlKindV2::CapGrant,
        request_id,
        proto::CapGrantPayloadV2 { cap }
            .encode()
            .expect("grant payload"),
        signers,
        signed_at,
    )
}

fn revoke_operation(
    v2: &V2Fixture,
    request_id: [u8; 16],
    cap_id: [u8; 16],
    signers: &[(Id16, Id16)],
    signed_at: u64,
) -> proto::OperationV2 {
    v2.two_signer_operation(
        proto::ControlKindV2::CapRevoke,
        request_id,
        proto::CapRevokePayloadV2 { cap_id }
            .encode()
            .expect("revoke payload"),
        signers,
        signed_at,
    )
}

fn invite_operation(
    v2: &V2Fixture,
    request_id: [u8; 16],
    invite_id: [u8; 16],
    token: &[u8; 32],
    expires_at: u64,
    signer: (Id16, Id16),
    signed_at: u64,
) -> proto::OperationV2 {
    let payload = proto::InviteCreatePayloadV2 {
        invite_id,
        token_hash: sha256(token),
        expires_at,
    }
    .encode()
    .expect("invite payload");
    v2.two_signer_operation(
        proto::ControlKindV2::InviteCreate,
        request_id,
        payload,
        &[signer],
        signed_at,
    )
}

fn rows_of_cap(v2: &V2Fixture, cap_id: &[u8; 16]) -> Option<(i64, i64, Option<i64>, Vec<u8>)> {
    v2.fx
        .db
        .read(|conn| {
            Ok(conn
                .query_row(
                    "SELECT issued_control_rev, issued_authz_rev, revoked_control_rev,
                            canonical_bytes
                     FROM management_caps WHERE vault_id = ?1 AND cap_id = ?2",
                    (conn.scope().as_slice(), cap_id.as_slice()),
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, Option<i64>>(2)?,
                            row.get::<_, Vec<u8>>(3)?,
                        ))
                    },
                )
                .ok())
        })
        .expect("cap row")
}

// ---------------------------------------------------------------- 1. CapGrant

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cap_grant_requires_two_admin_signatures_scope_and_current_revs() {
    let v2 = new_fixture("s4b1-cap-grant");
    let app = app(&v2);
    let (status, body) = upgrade_default(&v2, &app, [0xA0; 16]).await;
    assert_eq!(status, StatusCode::OK, "upgrade: {}", error_code(&body));

    // 升级后（authz_rev = 1）签出的 bearer：授权变化后必须立即失效。
    let stale_token = authz_login(
        &app,
        &v2.vault_id(),
        v2.extra[0].0,
        &v2.extra[0].1,
        Some([0xC1; 16]),
    )
    .await
    .expect("realm 3 token");
    let vault_before = v2.vault();
    let count_before = control_count(&v2);

    // 正例：给 genesis 设备授予 INVITE|APPROVE（两台 CAP_ADMIN 同签）。
    let cap_x = [0xD1u8; 16];
    let grant = grant_cap(
        &v2,
        cap_x,
        v2.genesis_device(),
        proto::CapScope::INVITE | proto::CapScope::APPROVE,
        600,
    );
    let operation = grant_operation(
        &v2,
        [0xA1; 16],
        grant.clone(),
        &admin_signers(&v2),
        v2.now_u64(),
    );
    let signed = v2.sign_operation(&operation);
    let (status, body) = submit(&app, &v2.vault_id(), &signed).await;
    assert_eq!(status, StatusCode::OK, "grant: {}", error_code(&body));
    let receipt = receipt(&body);
    assert_eq!(receipt.kind, 2);
    assert_eq!(receipt.control_rev, vault_before.control_rev + 1);
    assert_eq!(receipt.authz_rev, 2);
    assert_eq!(receipt.authority_rev, 1);
    assert_eq!(control_count(&v2), count_before + 1);

    let vault_after = v2.vault();
    assert_eq!(vault_after.authz_rev, 2);
    assert_eq!(vault_after.authority_rev, 1);
    assert_eq!(vault_after.auth_rev, vault_before.auth_rev + 1);
    let row = rows_of_cap(&v2, &cap_x).expect("cap row");
    assert_eq!(row.0, i64::try_from(receipt.control_rev).expect("rev"));
    assert_eq!(row.1, 2);
    assert!(row.2.is_none());
    assert_eq!(row.3, grant.encode().expect("cap encode"));

    // 旧 bearer（authz_rev=1）在授权变化后失效。
    let (status, body) = call(
        &app,
        get(
            &format!("{}/state", hex_path(&v2.vault_id())),
            Some(&stale_token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_error(&body, "REVOKED");

    // —— 负例 ——
    let mut rejected = 0usize;
    // 单签。
    {
        let cap = grant_cap(
            &v2,
            [0xD2; 16],
            v2.genesis_device(),
            proto::CapScope::INVITE,
            600,
        );
        let operation =
            grant_operation(&v2, [0xA2; 16], cap, &[admin_signers(&v2)[0]], v2.now_u64());
        let signed = v2.sign_operation(&operation);
        let (status, body) = submit(&app, &v2.vault_id(), &signed).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_error(&body, "FORBIDDEN");
        rejected += 1;
    }
    // 同一设备两把 cap（不能凑门限；C2 的 subject 是另一台设备）。
    {
        let cap = grant_cap(
            &v2,
            [0xD3; 16],
            v2.genesis_device(),
            proto::CapScope::INVITE,
            600,
        );
        let signers = [(v2.extra[0].0, [0xC1; 16]), (v2.extra[0].0, [0xC2; 16])];
        let operation = grant_operation(&v2, [0xA3; 16], cap, &signers, v2.now_u64());
        let signed = v2.sign_operation(&operation);
        let (status, body) = submit(&app, &v2.vault_id(), &signed).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_error(&body, "FORBIDDEN");
        rejected += 1;
    }
    // 自升权：先授 restricted cap（只有 INVITE），再用它签发含 APPROVE 的新 cap。
    let restricted = [0xD4u8; 16];
    {
        let cap = grant_cap(&v2, restricted, v2.extra[1].0, proto::CapScope::INVITE, 600);
        let operation = grant_operation(&v2, [0xA4; 16], cap, &admin_signers(&v2), v2.now_u64());
        let signed = v2.sign_operation(&operation);
        let (status, body) = submit(&app, &v2.vault_id(), &signed).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "restricted grant: {}",
            error_code(&body)
        );
    }
    {
        let cap = grant_cap(
            &v2,
            [0xD5; 16],
            v2.genesis_device(),
            proto::CapScope::INVITE | proto::CapScope::APPROVE,
            600,
        );
        let signers = [(v2.extra[0].0, [0xC1; 16]), (v2.extra[1].0, restricted)];
        let operation = grant_operation(&v2, [0xA5; 16], cap, &signers, v2.now_u64());
        let signed = v2.sign_operation(&operation);
        let (status, body) = submit(&app, &v2.vault_id(), &signed).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_error(&body, "FORBIDDEN");
        rejected += 1;
    }
    // 到期 cap 不能当签发者（授予 60s 后推进时钟）。
    let expiring = [0xD6u8; 16];
    {
        let cap = grant_cap(&v2, expiring, v2.extra[1].0, proto::CapScope::ALL, 60);
        let operation = grant_operation(&v2, [0xA6; 16], cap, &admin_signers(&v2), v2.now_u64());
        let signed = v2.sign_operation(&operation);
        let (status, body) = submit(&app, &v2.vault_id(), &signed).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "expiring grant: {}",
            error_code(&body)
        );
    }
    v2.fx.clock.advance_secs(61);
    {
        let cap = grant_cap(
            &v2,
            [0xD7; 16],
            v2.genesis_device(),
            proto::CapScope::INVITE,
            600,
        );
        let signers = [(v2.extra[0].0, [0xC1; 16]), (v2.extra[1].0, expiring)];
        let operation = grant_operation(&v2, [0xA7; 16], cap, &signers, v2.now_u64());
        let signed = v2.sign_operation(&operation);
        let (status, body) = submit(&app, &v2.vault_id(), &signed).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_error(&body, "FORBIDDEN");
        rejected += 1;
    }
    // 错 authority 的 cap（直接落库，模拟旧 authority 代签发的 cap）。
    let stale_authority = [0xE1u8; 16];
    {
        let vault = v2.vault();
        let (head_rev, _) = v2.head();
        let stale_cap = proto::ManagementCapV2 {
            version: 2,
            instance_id: vault.instance_id,
            vault_id: vault.vault_id,
            cap_id: stale_authority,
            subject_device_id: v2.extra[0].0,
            subject_ed_pk: v2.sign_pk(&v2.extra[0].0),
            scopes: proto::CapScope::new(proto::CapScope::ALL).expect("scopes"),
            not_before: v2.now_u64(),
            expires_at: v2.now_u64() + 3600,
            issued_control_rev: head_rev,
            issued_authz_rev: 1,
            issuer_authority_rev: vault.authority_rev + 1,
            delegable: false,
        };
        let canonical = stale_cap.encode().expect("stale cap");
        let subject = v2.extra[0].0;
        let subject_pk = v2.sign_pk(&subject);
        let now_i = i64::try_from(v2.now_u64()).expect("now");
        let issued_control = i64::try_from(head_rev).expect("rev");
        let authority = i64::try_from(vault.authority_rev + 1).expect("authority");
        let db = v2.fx.db.clone();
        tokio::task::block_in_place(|| {
            db.write_blocking(move |conn| {
                conn.execute(
                    "INSERT INTO management_caps(vault_id, cap_id, canonical_bytes, subject_device,
                                                 subject_pk, scopes, not_before, expires_at,
                                                 issued_control_rev, issued_authz_rev,
                                                 issuer_authority_rev, revoked_control_rev,
                                                 delegable, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 1, ?10, NULL, 0, ?11)",
                    (
                        conn.scope().as_slice(),
                        stale_authority.as_slice(),
                        canonical.as_slice(),
                        subject.as_slice(),
                        subject_pk.as_slice(),
                        i64::from(proto::CapScope::ALL),
                        now_i,
                        now_i + 3600,
                        issued_control,
                        authority,
                        now_i,
                    ),
                )?;
                Ok(())
            })
        })
        .expect("insert stale authority cap");
        let cap = grant_cap(
            &v2,
            [0xD8; 16],
            v2.genesis_device(),
            proto::CapScope::INVITE,
            600,
        );
        let signers = [
            (v2.extra[0].0, stale_authority),
            (v2.extra[1].0, [0xC2; 16]),
        ];
        let operation = grant_operation(&v2, [0xA8; 16], cap, &signers, v2.now_u64());
        let signed = v2.sign_operation(&operation);
        let (status, body) = submit(&app, &v2.vault_id(), &signed).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_error(&body, "FORBIDDEN");
        rejected += 1;
    }
    // 新 cap 的 rev 绑定错误 / subject key 不符 / TTL 超限 / 重复 cap id。
    for (label, expected_status, expected_code, mutate) in [
        (
            "issued_control_rev",
            403,
            "FORBIDDEN",
            (|cap: &mut proto::ManagementCapV2| cap.issued_control_rev -= 1)
                as fn(&mut proto::ManagementCapV2),
        ),
        (
            "issued_authz_rev",
            403,
            "FORBIDDEN",
            (|cap: &mut proto::ManagementCapV2| cap.issued_authz_rev = 1)
                as fn(&mut proto::ManagementCapV2),
        ),
        (
            "issuer_authority_rev",
            403,
            "FORBIDDEN",
            (|cap: &mut proto::ManagementCapV2| cap.issuer_authority_rev += 1)
                as fn(&mut proto::ManagementCapV2),
        ),
        (
            "subject key",
            403,
            "FORBIDDEN",
            (|cap: &mut proto::ManagementCapV2| cap.subject_ed_pk = [0x99; 32])
                as fn(&mut proto::ManagementCapV2),
        ),
        (
            "ttl 超限",
            403,
            "FORBIDDEN",
            (|cap: &mut proto::ManagementCapV2| {
                cap.expires_at = cap.not_before + proto::MAX_CAP_TTL_SECS_V2 + 1
            }) as fn(&mut proto::ManagementCapV2),
        ),
        (
            "重复 cap id",
            409,
            "ID_CONFLICT",
            (|cap: &mut proto::ManagementCapV2| cap.cap_id = [0xD1; 16])
                as fn(&mut proto::ManagementCapV2),
        ),
    ] {
        let mut cap = grant_cap(
            &v2,
            [0xD9; 16],
            v2.genesis_device(),
            proto::CapScope::INVITE,
            600,
        );
        mutate(&mut cap);
        let operation = grant_operation(&v2, [0xA9; 16], cap, &admin_signers(&v2), v2.now_u64());
        let signed = v2.sign_operation(&operation);
        let (status, body) = submit(&app, &v2.vault_id(), &signed).await;
        assert_eq!(
            status,
            StatusCode::from_u16(expected_status).expect("status"),
            "{label}: {}",
            error_code(&body)
        );
        assert_error(&body, expected_code);
        rejected += 1;
    }
    // 旧 authz rev（CAS）。
    {
        let cap = grant_cap(
            &v2,
            [0xDA; 16],
            v2.genesis_device(),
            proto::CapScope::INVITE,
            600,
        );
        let mut operation =
            grant_operation(&v2, [0xAA; 16], cap, &admin_signers(&v2), v2.now_u64());
        operation.expected_authz_rev = 1;
        let signed = v2.sign_operation(&operation);
        let (status, body) = submit(&app, &v2.vault_id(), &signed).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_error(&body, "CAS_CONFLICT");
        rejected += 1;
    }
    // 错签名（signer0 用 signer1 的私钥签）。
    {
        let cap = grant_cap(
            &v2,
            [0xDB; 16],
            v2.genesis_device(),
            proto::CapScope::INVITE,
            600,
        );
        let operation = grant_operation(&v2, [0xAB; 16], cap, &admin_signers(&v2), v2.now_u64());
        let keys = vec![&v2.extra[1].1, &v2.extra[1].1];
        let signed = v2.sign_operation_with(&operation, &keys);
        let (status, body) = submit(&app, &v2.vault_id(), &signed).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_error(&body, "SIGNATURE_INVALID");
        rejected += 1;
    }

    // 失败不得推进控制链：成功次数 = 正例 3（X/restricted/expiring）。
    assert_eq!(control_count(&v2), count_before + 3);
    assert_eq!(rejected, 13);
}

// ---------------------------------------------------------------- 2. CapRevoke + WS

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cap_revoke_kills_bearer_and_websocket_and_requires_two_signatures() {
    let v2 = new_fixture("s4b1-cap-revoke");
    let state = relay::api::AppState::new(
        v2.fx.db.inner().clone(),
        v2.fx.cfg.clone(),
        std::sync::Arc::new(relay::verify::Ed25519Hook),
    );
    let app: Router = relay::api::router(state.clone());
    let (status, body) = upgrade_default(&v2, &app, [0xB0; 16]).await;
    assert_eq!(status, StatusCode::OK, "upgrade: {}", error_code(&body));

    // 先授予一张可撤销的 cap（INVITE）。
    let target = [0xE5u8; 16];
    let cap = grant_cap(
        &v2,
        target,
        v2.genesis_device(),
        proto::CapScope::INVITE,
        600,
    );
    let operation = grant_operation(&v2, [0xB1; 16], cap, &admin_signers(&v2), v2.now_u64());
    let signed = v2.sign_operation(&operation);
    let (status, body) = submit(&app, &v2.vault_id(), &signed).await;
    assert_eq!(status, StatusCode::OK, "grant: {}", error_code(&body));

    // 目标 cap 的 bearer（撤销后必须失效）。
    let target_token = authz_login(
        &app,
        &v2.vault_id(),
        v2.genesis_device(),
        v2.genesis_key(),
        Some(target),
    )
    .await
    .expect("target cap token");
    // 另一台设备的 bearer（cap 未撤销，但授权 rev 前进后同样失效）。
    let other_token = authz_login(
        &app,
        &v2.vault_id(),
        v2.extra[0].0,
        &v2.extra[0].1,
        Some([0xC1; 16]),
    )
    .await
    .expect("admin cap token");

    // 真实 WS：realm 3 bearer 建连并先收到一帧（ADR 0015：逐帧重验 cap/双 rev）。
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let serve_router = relay::api::router(state.clone());
    tokio::spawn(async move {
        let _ = axum::serve(listener, serve_router).await;
    });
    let uri = format!(
        "ws://{}/v2/vaults/{}/wake",
        addr,
        b64url_encode(&v2.vault_id())
    );
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut request = uri.into_client_request().expect("ws request");
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {other_token}").parse().expect("header"),
    );
    let (mut socket, _) = tokio_tungstenite::connect_async(request)
        .await
        .expect("ws connect");
    let first = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .expect("frame timeout")
        .expect("frame")
        .expect("frame ok");
    let Message::Text(text) = first else {
        panic!("unexpected frame: {first:?}");
    };
    let value: serde_json::Value = serde_json::from_str(&text).expect("json");
    assert_eq!(value["authz_rev"], 2, "帧里必须带授权 rev");

    // 两签撤销（C1 + C2）。
    let vault_before = v2.vault();
    let count_before = control_count(&v2);
    let operation = revoke_operation(&v2, [0xB2; 16], target, &admin_signers(&v2), v2.now_u64());
    let signed = v2.sign_operation(&operation);
    let (status, body) = submit(&app, &v2.vault_id(), &signed).await;
    assert_eq!(status, StatusCode::OK, "revoke: {}", error_code(&body));
    let receipt = receipt(&body);
    assert_eq!(receipt.kind, 3);
    assert_eq!(receipt.authz_rev, vault_before.authz_rev + 1);
    let vault_after = v2.vault();
    assert_eq!(vault_after.authz_rev, vault_before.authz_rev + 1);
    assert_eq!(vault_after.auth_rev, vault_before.auth_rev + 1);
    assert_eq!(control_count(&v2), count_before + 1);
    let row = rows_of_cap(&v2, &target).expect("cap row");
    assert_eq!(
        row.2,
        Some(i64::try_from(receipt.control_rev).expect("rev"))
    );

    // 撤销后：目标 cap 的 bearer 与"旧 rev" bearer 都失效。
    for token in [&target_token, &other_token] {
        let (status, body) = call(
            &app,
            get(&format!("{}/state", hex_path(&v2.vault_id())), Some(token)),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_error(&body, "REVOKED");
    }

    // WS 必须被服务端关闭（close 1008），不靠 30s tick（notify 立即触发）。
    let closed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match socket.next().await {
                None => return true,
                Some(Ok(Message::Close(_))) | Some(Err(_)) => return true,
                Some(Ok(_)) => continue,
            }
        }
    })
    .await
    .expect("ws close timeout");
    assert!(closed, "cap 撤销/双 rev 变化后 WS 必须断开");

    // 重复撤销（新 request_id）→ 409 INVALID_STATE；未知 cap → 404；单签 → 403。
    let operation = revoke_operation(&v2, [0xB3; 16], target, &admin_signers(&v2), v2.now_u64());
    let signed = v2.sign_operation(&operation);
    let (status, body) = submit(&app, &v2.vault_id(), &signed).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_error(&body, "INVALID_STATE");

    let operation = revoke_operation(
        &v2,
        [0xB4; 16],
        [0xFE; 16],
        &admin_signers(&v2),
        v2.now_u64(),
    );
    let signed = v2.sign_operation(&operation);
    let (status, body) = submit(&app, &v2.vault_id(), &signed).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_error(&body, "NOT_FOUND");

    let operation = revoke_operation(
        &v2,
        [0xB5; 16],
        [0xC2; 16],
        &[admin_signers(&v2)[0]],
        v2.now_u64(),
    );
    let signed = v2.sign_operation(&operation);
    let (status, body) = submit(&app, &v2.vault_id(), &signed).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_error(&body, "FORBIDDEN");

    assert_eq!(control_count(&v2), count_before + 1, "拒绝不得推进控制链");
}

// ---------------------------------------------------------------- 3. InviteCreate + 跨 vault

fn redeem_device() -> (Id16, SigningKey, [u8; 32], Vec<u8>, Vec<u8>) {
    let key = SigningKey::from_bytes(&[0x3Fu8; 32]);
    let sign_pk = key.verifying_key().to_bytes();
    let proof = b"device-pop".to_vec();
    let signature = key.sign(&proof).to_bytes().to_vec();
    ([0x0Fu8; 16], key, sign_pk, proof, signature)
}

async fn redeem(
    app: &Router,
    vault_id: &Id16,
    invite_id: &[u8; 16],
    token: &[u8; 32],
    device: (Id16, &SigningKey, [u8; 32]),
) -> (StatusCode, Vec<u8>) {
    let (device_id, key, sign_pk) = device;
    let hpke_pk = [0x16u8; 32];
    let proof = b"device-pop".to_vec();
    let signature = key.sign(&proof).to_bytes().to_vec();
    call(
        app,
        post_cbor(
            &format!(
                "{}/invites/{}/redeem",
                hex_path(vault_id),
                b64url_encode(invite_id)
            ),
            None,
            vec![
                (0, wire::bytes(token.to_vec())),
                (1, wire::bytes(device_id.to_vec())),
                (2, wire::bytes(sign_pk.to_vec())),
                (3, wire::bytes(hpke_pk.to_vec())),
                (4, wire::bytes(proof)),
                (5, wire::bytes(signature)),
            ],
        ),
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invite_create_is_single_invite_cap_scoped_and_dies_with_its_cap() {
    let v2 = new_fixture("s4b1-invite");
    let app = app(&v2);
    let (status, body) = upgrade_default(&v2, &app, [0xC0; 16]).await;
    assert_eq!(status, StatusCode::OK, "upgrade: {}", error_code(&body));
    let vault_before = v2.vault();
    let count_before = control_count(&v2);

    // 正例：一台 INVITE cap（C1）创建邀请；只写 PENDING 事实。
    let invite_id = [0x51u8; 16];
    let token = [0x52u8; 32];
    let operation = invite_operation(
        &v2,
        [0xC1; 16],
        invite_id,
        &token,
        v2.now_u64() + 600,
        (v2.extra[0].0, [0xC1; 16]),
        v2.now_u64(),
    );
    let signed = v2.sign_operation(&operation);
    let (status, body) = submit(&app, &v2.vault_id(), &signed).await;
    assert_eq!(status, StatusCode::OK, "invite: {}", error_code(&body));
    let receipt = receipt(&body);
    assert_eq!(receipt.kind, 11);
    assert_eq!(receipt.control_rev, vault_before.control_rev + 1);
    // 邀请只推进 control。
    assert_eq!(receipt.authz_rev, vault_before.authz_rev);
    let vault_after = v2.vault();
    assert_eq!(vault_after.authz_rev, vault_before.authz_rev);
    assert_eq!(vault_after.auth_rev, vault_before.auth_rev);
    let invite_row = v2
        .fx
        .db
        .read(|conn| {
            Ok(conn
                .query_row(
                    "SELECT token_hash, expires_at, pending_device, used_at, issuer_cap_id
                     FROM invites WHERE vault_id = ?1 AND invite_id = ?2",
                    (conn.scope().as_slice(), invite_id.as_slice()),
                    |row| {
                        Ok((
                            row.get::<_, Vec<u8>>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, Option<Vec<u8>>>(2)?,
                            row.get::<_, Option<i64>>(3)?,
                            row.get::<_, Option<Vec<u8>>>(4)?,
                        ))
                    },
                )
                .ok())
        })
        .expect("invite row")
        .expect("invite exists");
    assert_eq!(invite_row.0, sha256(&token).to_vec());
    assert_eq!(
        invite_row.1,
        i64::try_from(v2.now_u64() + 600).expect("expires")
    );
    assert!(invite_row.2.is_none());
    assert!(invite_row.3.is_none());
    assert_eq!(invite_row.4, Some(vec![0xC1u8; 16]));

    // TTL 超过 policy.max_invite_ttl / 过期时间 → 400。
    for (label, expires) in [
        ("TTL 超限", v2.now_u64() + proto::MAX_INVITE_TTL_SECS_V2 + 1),
        ("已过期", v2.now_u64().saturating_sub(1)),
    ] {
        let operation = invite_operation(
            &v2,
            [0xC2; 16],
            [0x53; 16],
            &[0x54; 32],
            expires,
            (v2.extra[0].0, [0xC1; 16]),
            v2.now_u64(),
        );
        let signed = v2.sign_operation(&operation);
        let (status, body) = submit(&app, &v2.vault_id(), &signed).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{label}: {}",
            error_code(&body)
        );
        assert_error(&body, "INVALID_REQUEST");
    }

    // 错 scope：只持 CAP_ADMIN 的 cap 不能创建邀请。
    let admin_only = [0xE9u8; 16];
    {
        let cap = grant_cap(
            &v2,
            admin_only,
            v2.extra[1].0,
            proto::CapScope::CAP_ADMIN,
            600,
        );
        let operation = grant_operation(&v2, [0xC3; 16], cap, &admin_signers(&v2), v2.now_u64());
        let signed = v2.sign_operation(&operation);
        let (status, body) = submit(&app, &v2.vault_id(), &signed).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "grant admin-only: {}",
            error_code(&body)
        );
    }
    {
        let operation = invite_operation(
            &v2,
            [0xC4; 16],
            [0x55; 16],
            &[0x56; 32],
            v2.now_u64() + 600,
            (v2.extra[1].0, admin_only),
            v2.now_u64(),
        );
        let signed = v2.sign_operation(&operation);
        let (status, body) = submit(&app, &v2.vault_id(), &signed).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_error(&body, "FORBIDDEN");
    }
    // 双签（kind 11 只要一台）→ 403。
    {
        let operation = v2.two_signer_operation(
            proto::ControlKindV2::InviteCreate,
            [0xC5; 16],
            proto::InviteCreatePayloadV2 {
                invite_id: [0x57; 16],
                token_hash: sha256(&[0x58; 32]),
                expires_at: v2.now_u64() + 600,
            }
            .encode()
            .expect("payload"),
            &admin_signers(&v2),
            v2.now_u64(),
        );
        let signed = v2.sign_operation(&operation);
        let (status, body) = submit(&app, &v2.vault_id(), &signed).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_error(&body, "FORBIDDEN");
    }
    // 重复 invite id / token_hash → 409。
    for (index, (label, invite, token)) in [
        ("重复 invite id", invite_id, [0x59u8; 32]),
        ("重复 token_hash", [0x5Au8; 16], token),
    ]
    .into_iter()
    .enumerate()
    {
        let operation = invite_operation(
            &v2,
            [0xC6 + u8::try_from(index).expect("index"); 16],
            invite,
            &token,
            v2.now_u64() + 600,
            (v2.extra[0].0, [0xC1; 16]),
            v2.now_u64(),
        );
        let signed = v2.sign_operation(&operation);
        let (status, body) = submit(&app, &v2.vault_id(), &signed).await;
        assert_eq!(
            status,
            StatusCode::CONFLICT,
            "{label}: {}",
            error_code(&body)
        );
        assert_error(&body, "ID_CONFLICT");
    }

    // —— 跨 vault：第二个 vault（legacy、独立设备/密钥）——
    let second = tokio::task::block_in_place(|| v2.create_second_vault([0xB2u8; 16]));
    let b_path = hex_path(&second.vault_id);
    // B 未启用 V2：/authz fail-stop，不会看到 A 的 cap。
    let (status, body) = call(
        &app,
        post_cbor(
            &format!("{b_path}/authz/challenges"),
            None,
            vec![
                (0, wire::uint(3)),
                (1, wire::bytes(v2.extra[0].0.to_vec())),
                (2, wire::bytes([0xC1; 16].to_vec())),
            ],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_error(&body, "INVALID_STATE");
    // A 的操作提交到 B 路由 → 403（tenant/instance 不匹配），不串控制链。
    let (status, body) = submit(&app, &second.vault_id, &signed_a_operation(&v2)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_error(&body, "FORBIDDEN");
    // A 的 challenge 在 B 的 token 端点 → 401（scope 查询查不到）。
    let challenge_bytes = {
        let (status, body) = call(
            &app,
            post_cbor(
                &format!("{b_path}/authz/challenges"),
                None,
                vec![
                    (0, wire::uint(1)),
                    (1, wire::bytes(second.genesis_device.to_vec())),
                ],
            ),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::CONFLICT,
            "B 未启用 V2: {}",
            error_code(&body)
        );
        let (status, body) = call(
            &app,
            post_cbor(
                &format!("{}/authz/challenges", hex_path(&v2.vault_id())),
                None,
                vec![
                    (0, wire::uint(1)),
                    (1, wire::bytes(v2.genesis_device().to_vec())),
                ],
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        wire::bytes_field(&cbor_fields(&body), 0).expect("challenge bytes")
    };
    let signature = v2
        .genesis_key()
        .sign(&proto::auth_signing_input_v2(&challenge_bytes))
        .to_bytes()
        .to_vec();
    let (status, body) = call(
        &app,
        post_cbor(
            &format!("{b_path}/authz/tokens"),
            None,
            vec![
                (0, wire::bytes(challenge_bytes)),
                (1, wire::bytes(signature)),
            ],
        ),
    )
    .await;
    // B 尚未启用 V2 → fail-stop INVALID_STATE（无论 A 的 challenge 是否合法都查不到/用不了）。
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "跨 vault challenge: {}",
        error_code(&body)
    );
    assert_error(&body, "INVALID_STATE");

    // A 的邀请在 B 路由兑换 → 409（邀请事实按 vault 隔离）；在 A 路由可兑换。
    let (device_id, key, sign_pk, _, _) = redeem_device();
    let (status, body) = redeem(
        &app,
        &second.vault_id,
        &invite_id,
        &token,
        (device_id, &key, sign_pk),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "B 上兑换 A 邀请: {}",
        error_code(&body)
    );
    let (status, body) = redeem(
        &app,
        &v2.vault_id(),
        &invite_id,
        &token,
        (device_id, &key, sign_pk),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "A 上兑换: {}",
        error_code(&body)
    );
    let pending_in_a: i64 = v2
        .fx
        .db
        .read(|conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM devices
                 WHERE vault_id = ?1 AND device_id = ?2 AND status = 'pending'",
                (conn.scope().as_slice(), device_id.as_slice()),
                |row| row.get(0),
            )?)
        })
        .expect("pending count");
    assert_eq!(pending_in_a, 1, "兑换只落在 A");
    let pending_in_b: i64 = v2
        .fx
        .db
        .read(|conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM devices
                 WHERE vault_id = ?1 AND device_id = ?2",
                (second.vault_id.as_slice(), device_id.as_slice()),
                |row| row.get(0),
            )?)
        })
        .expect("pending count B");
    assert_eq!(pending_in_b, 0, "B 不得被写入");

    // 签发 cap 撤销后，未兑换邀请失效（兑换时重验）。
    let dead_invite = [0x5Bu8; 16];
    let dead_token = [0x5Cu8; 32];
    let operation = invite_operation(
        &v2,
        [0xC7; 16],
        dead_invite,
        &dead_token,
        v2.now_u64() + 600,
        (v2.extra[0].0, [0xC1; 16]),
        v2.now_u64(),
    );
    let signed = v2.sign_operation(&operation);
    let (status, body) = submit(&app, &v2.vault_id(), &signed).await;
    assert_eq!(status, StatusCode::OK, "dead invite: {}", error_code(&body));
    // 两签撤销 C1（被撤销 cap 自己也可以作为签发者之一）。
    let operation = revoke_operation(
        &v2,
        [0xC8; 16],
        [0xC1; 16],
        &admin_signers(&v2),
        v2.now_u64(),
    );
    let signed = v2.sign_operation(&operation);
    let (status, body) = submit(&app, &v2.vault_id(), &signed).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "revoke issuer: {}",
        error_code(&body)
    );
    let (device_id, key, sign_pk, _, _) = redeem_device();
    let (status, body) = redeem(
        &app,
        &v2.vault_id(),
        &dead_invite,
        &dead_token,
        (device_id, &key, sign_pk),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{}", error_code(&body));
    assert_error(&body, "FORBIDDEN");

    // 只成功 4 次（邀请、admin-only grant、再撤销 C1、C1 撤销）——失败不推进链。
    assert_eq!(control_count(&v2), count_before + 4);
}

/// 供跨 vault 负例使用：A 上再构造一个合法操作（不提交）。
fn signed_a_operation(v2: &V2Fixture) -> proto::SignedControlV2 {
    let operation = invite_operation(
        v2,
        [0xC9; 16],
        [0x5Du8; 16],
        &[0x5Eu8; 32],
        v2.now_u64() + 600,
        (v2.extra[0].0, [0xC1; 16]),
        v2.now_u64(),
    );
    v2.sign_operation(&operation)
}
