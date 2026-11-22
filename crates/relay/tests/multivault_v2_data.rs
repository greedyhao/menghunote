//! ADR 0014 **S2c**：`/v2/vaults/{id}/...` 全部数据面业务路由 + 租户隔离。
//!
//! 覆盖（任务书 §3）：
//! 1. 两个 vault（v2 创建票据）各自用 v2 路由完成 control / log 追加与拉取 / 对象分片
//!    PUT+GET+complete / 批量 updates / checkpoint 建+refs+发布 / bootstrap+lease / ack /
//!    key-slots 读 / invite+redeem+approve / latest grant / rotation 基础路径（begin →
//!    current → view → grants → assets → abort）；
//! 2. **相同局部 ID**（device_id / object_id / log seq / request_id / grant_id）两 vault
//!    交叉请求 404/403，互不串数据；
//! 3. token/challenge 跨 vault 拒绝；v1 legacy token 只能到它所属的默认 vault；
//! 4. v2 业务限流 429 且 v1 不受影响；未授权错误不泄露租户存在性；
//! 5. v2 路由清单的 realm 门禁（逐条断言 403 而不是 404）；
//! 6. `/v2/vaults/{v}/wake` WebSocket 真实监听端口 + 跨 vault token 拒绝。
//!
//! 无 sleep：所有断言都在明确的 HTTP 响应 / WS 帧收敛点之后。

mod support;

use std::time::Duration;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use relay::admin;
use relay::api::{router, AppState};
use relay::auth::Realm;
use relay::envelope::ObjectType;
use relay::ids::{b64url_encode, sha256, Hash32, Id16};
use relay::ratelimit::RateLimitSpec;
use relay::wire;
use support::*;
use tower::ServiceExt;

const VAULT_B: Id16 = [0xB7u8; 16];
const VAULT_C: Id16 = [0xC7u8; 16];
/// genesis 设备：与夹具 A 的设备**同一个局部 ID**（device 也按 vault 隔离）。
const DEVICE: Id16 = [2u8; 16];
/// 只在 v2 里存在的设备（边界用例：v1 legacy 默认 vault 不认识它）。
const DEVICE_ONLY_V2: Id16 = [0x0Eu8; 16];
/// 两个 vault 故意用**同一个**待批准设备 id / 邀请 token / request_id / grant id。
const PENDING_DEVICE: Id16 = [0x0Bu8; 16];
const INVITE_TOKEN: [u8; 32] = [0x77u8; 32];
const INVITE_REQUEST_ID: Id16 = [0x55u8; 16];
const APPROVE_REQUEST_ID: Id16 = [0x56u8; 16];
const GRANT_ID: Id16 = [0x44u8; 16];
const BEGIN_REQUEST_ID: Id16 = [0x66u8; 16];
const ABORT_REQUEST_ID: Id16 = [0x67u8; 16];
const REVOKE_REQUEST_ID: Id16 = [0x68u8; 16];
const REVOKE_AGAIN_REQUEST_ID: Id16 = [0x69u8; 16];
const OBJECT_ID: Hash32 = [0x42u8; 32];
const BATCH_OBJECT_ID: Hash32 = [0x43u8; 32];
const PENDING_SIGN_PK: [u8; 32] = [0x0Cu8; 32];
const PENDING_HPKE_PK: [u8; 32] = [0x0Du8; 32];

/// 在 tokio 多线程 runtime 里执行同步 DB 调用（writer 线程 + `block_in_place`）。
fn block<T>(f: impl FnOnce() -> T) -> T {
    tokio::task::block_in_place(f)
}

fn fixture(label: &str) -> Fixture {
    // v2 create/auth 限流关闭；业务限流用默认值（3000/min，足够本文件全部请求）。
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

fn post_empty(uri: &str, token: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().method("POST").uri(uri);
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    builder.body(Body::empty()).expect("request")
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

fn patch(uri: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method("PATCH")
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .expect("request")
}

fn b64(bytes: &[u8]) -> String {
    b64url_encode(bytes)
}

/// `/v2/vaults/{vault}{suffix}`。
fn v2(vault: &Id16, suffix: &str) -> String {
    format!("/v2/vaults/{}{suffix}", b64(vault))
}

fn code_of(fields: &wire::Fields) -> String {
    wire::text_field(fields, 0).unwrap_or_default()
}

fn error_code(body: &[u8]) -> String {
    code_of(&cbor_fields(body))
}

fn retryable(body: &[u8]) -> bool {
    wire::bool_field(&cbor_fields(body), 1).expect("retryable")
}

struct CreatedVault {
    cursor: String,
    instance: Id16,
}

/// 用 v2 创建票据建一个 vault（与 v1 genesis 同响应形状）。
async fn create_v2_vault(app: &axum::Router, fx: &Fixture, vault_id: Id16) -> CreatedVault {
    create_v2_vault_with_device(app, fx, vault_id, DEVICE).await
}

async fn create_v2_vault_with_device(
    app: &axum::Router,
    fx: &Fixture,
    vault_id: Id16,
    device_id: Id16,
) -> CreatedVault {
    let sign_pk = fx.device_signing_key.verifying_key().to_bytes();
    let hpke_pk = [4u8; 32];
    let certificate = certificate_bytes(&vault_id, &device_id, &sign_pk, &hpke_pk, 1);
    let ticket = block(|| {
        admin::create_reservation_for(fx.db.inner(), vault_id, Duration::from_secs(900))
            .expect("reserve create ticket")
            .token
    });
    let fields = vec![
        (0, wire::bytes(ticket.to_vec())),
        (1, wire::bytes(vault_id.to_vec())),
        (2, wire::bytes(device_id.to_vec())),
        (3, wire::bytes(sign_pk.to_vec())),
        (4, wire::bytes(hpke_pk.to_vec())),
        (5, wire::bytes(fx.manager_pub.to_vec())),
        (6, wire::bytes(certificate)),
        (7, wire::bytes(b"certificate-sig".to_vec())),
    ];
    let (status, body) = call(app, post_cbor("/v2/vaults", None, fields)).await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "create {}: {}",
        b64(&vault_id),
        error_code(&body)
    );
    let created = cbor_fields(&body);
    CreatedVault {
        cursor: wire::text_field(&created, 5).expect("initial cursor"),
        instance: wire::id16_field(&created, 0).expect("instance id"),
    }
}

/// 走 v2 challenge → token 拿一个 token。
async fn auth_token(app: &axum::Router, vault_id: Id16, realm: Realm, subject: &Id16) -> String {
    let realm_tag = match realm {
        Realm::Device => 1u64,
        Realm::Management => 2u64,
        Realm::ManagementDevice => 3u64,
        Realm::Recovery => 4u64,
    };
    let (status, body) = call(
        app,
        post_cbor(
            &v2(&vault_id, "/auth/challenges"),
            None,
            vec![
                (0, wire::uint(realm_tag)),
                (1, wire::bytes(subject.to_vec())),
            ],
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "challenge {}: {}",
        b64(&vault_id),
        error_code(&body)
    );
    let challenge = wire::id16_field(&cbor_fields(&body), 0).expect("challenge id");
    let (status, body) = call(
        app,
        post_cbor(
            &v2(&vault_id, "/auth/tokens"),
            None,
            vec![
                (0, wire::bytes(challenge.to_vec())),
                (1, wire::bytes(b"challenge-message".to_vec())),
                (2, wire::bytes(b"signature".to_vec())),
            ],
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "token {}: {}",
        b64(&vault_id),
        error_code(&body)
    );
    wire::text_field(&cbor_fields(&body), 0).expect("token")
}

fn signed_wrapper(payload: Vec<u8>, signature: &[u8]) -> wire::CdeValue {
    wire::map(vec![
        (0, wire::bytes(payload)),
        (1, wire::bytes(signature.to_vec())),
    ])
}

/// 一个待上传对象的固定头（§16.1：非 UPDATE 的 sender_seq 必须为 0）。
struct ObjectSpec {
    object_type: ObjectType,
    object_id: Hash32,
    generation: u64,
    sender_seq: u64,
    plain_len: u64,
}

impl ObjectSpec {
    fn update(object_id: Hash32, sender_seq: u64, plain_len: u64) -> Self {
        Self {
            object_type: ObjectType::Update,
            object_id,
            generation: 1,
            sender_seq,
            plain_len,
        }
    }

    fn snapshot(object_id: Hash32, plain_len: u64) -> Self {
        Self {
            object_type: ObjectType::Snapshot,
            object_id,
            generation: 1,
            sender_seq: 0,
            plain_len,
        }
    }

    fn manifest(object_id: Hash32, plain_len: u64) -> Self {
        Self {
            object_type: ObjectType::Manifest,
            object_id,
            generation: 0,
            sender_seq: 0,
            plain_len,
        }
    }
}

/// 上传一个单片对象（PUT part → POST complete），返回 (envelope, relay_seq)。
///
/// `relay_seq` 只有进 log 的类型（update/asset）才有；snapshot/manifest 为 `None`。
async fn upload_object(
    app: &axum::Router,
    vault_id: Id16,
    token: &str,
    spec: ObjectSpec,
) -> (Vec<u8>, Option<u64>) {
    let ObjectSpec {
        object_type,
        object_id,
        generation,
        sender_seq,
        plain_len,
    } = spec;
    let envelope = single_part_envelope(
        object_type,
        vault_id,
        object_id,
        generation,
        sender_seq,
        plain_len,
    );
    let part_uri = v2(&vault_id, &format!("/objects/{}/parts/0", b64(&object_id)));
    let (status, body) = call(app, put_bytes(&part_uri, token, envelope.clone())).await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "put part {}: {}",
        b64(&object_id),
        error_code(&body)
    );
    // 同字节重试幂等（v2 与 v1 同一实现）。
    let (status, _) = call(app, put_bytes(&part_uri, token, envelope.clone())).await;
    assert_eq!(status, StatusCode::OK);
    let digest = digest_of(std::slice::from_ref(&envelope)).expect("object digest");
    let complete_uri = v2(&vault_id, &format!("/objects/{}/complete", b64(&object_id)));
    let (status, body) = call(
        app,
        post_cbor(
            &complete_uri,
            Some(token),
            vec![
                (0, wire::bytes(commit_for(&digest))),
                (1, wire::bytes(b"signature".to_vec())),
            ],
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "complete {}: {}",
        b64(&object_id),
        error_code(&body)
    );
    let seq = wire::opt_uint_field(&cbor_fields(&body), 1).expect("relay_seq");
    (envelope, seq)
}

/// 建 + refs + 发布一个 baseline checkpoint，返回 checkpoint_id。
async fn publish_checkpoint(
    app: &axum::Router,
    vault_id: Id16,
    token: &str,
    snapshot: Hash32,
    manifest: Hash32,
) -> Id16 {
    let (status, body) = call(
        app,
        post_cbor(
            &v2(&vault_id, "/checkpoints"),
            Some(token),
            vec![(0, wire::uint(4096))],
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "checkpoint create: {}",
        error_code(&body)
    );
    let created = cbor_fields(&body);
    let checkpoint_id = wire::id16_field(&created, 0).expect("checkpoint id");
    let covered = wire::uint_field(&created, 2).expect("covered_seq");
    let refs = vec![
        wire::map(vec![
            (0, wire::bytes(snapshot.to_vec())),
            (1, wire::text("data")),
        ]),
        wire::map(vec![
            (0, wire::bytes(manifest.to_vec())),
            (1, wire::text("manifest")),
        ]),
    ];
    let refs_uri = v2(
        &vault_id,
        &format!("/checkpoints/{}/refs", b64(&checkpoint_id)),
    );
    let (status, body) = call(
        app,
        post_cbor(&refs_uri, Some(token), vec![(0, wire::array(refs))]),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "checkpoint refs: {}",
        error_code(&body)
    );
    let publish_uri = v2(
        &vault_id,
        &format!("/checkpoints/{}/publish", b64(&checkpoint_id)),
    );
    let (status, body) = call(
        app,
        post_cbor(
            &publish_uri,
            Some(token),
            vec![
                (0, wire::uint(covered)),
                (1, wire::bytes(manifest.to_vec())),
                (2, wire::bytes([9u8; 32].to_vec())),
                (3, wire::bytes(b"checkpoint-commit".to_vec())),
                (4, wire::bytes(b"signature".to_vec())),
            ],
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "checkpoint publish: {}",
        error_code(&body)
    );
    checkpoint_id
}

/// 一个 vault 通过 v2 路由跑完全部数据面业务后留下的事实（供跨 vault 对比）。
struct VaultFacts {
    device_token: String,
    manager_token: String,
    cursor0: String,
    object_envelope: Vec<u8>,
    log_seqs: Vec<u64>,
    log_next_cursor: String,
    checkpoints: [Id16; 2],
    lease_id: Id16,
    invite_id: Id16,
    rotation_id: Id16,
    control_rev: u64,
}

/// 单 vault 的完整 v2 业务流（全部经真实 axum handler；无 DB 直接写）。
async fn run_flow(
    app: &axum::Router,
    fx: &Fixture,
    vault_id: Id16,
    created: &CreatedVault,
) -> VaultFacts {
    let manager_subject = Realm::management_subject(&fx.manager_pub);
    let device_token = auth_token(app, vault_id, Realm::Device, &DEVICE).await;
    let manager_token = auth_token(app, vault_id, Realm::Management, &manager_subject).await;

    // ---- state / control ----
    let (status, body) = call(app, get(&v2(&vault_id, "/state"), Some(&device_token))).await;
    assert_eq!(status, StatusCode::OK, "state: {}", error_code(&body));
    let state = cbor_fields(&body);
    assert_eq!(
        wire::bytes_field(&state, 0).expect("vault id"),
        vault_id.to_vec()
    );
    assert_eq!(
        wire::text_field(&state, 14).expect("cursor"),
        created.cursor,
        "state 键 14 必须与创建响应的初始游标一致"
    );
    assert_eq!(
        wire::bytes_field(&state, 2).expect("instance"),
        created.instance.to_vec()
    );

    let (status, body) = call(app, get(&v2(&vault_id, "/control"), Some(&manager_token))).await;
    assert_eq!(status, StatusCode::OK, "control: {}", error_code(&body));
    let control = cbor_fields(&body);
    assert_eq!(wire::uint_field(&control, 1).expect("control_rev"), 1);
    let members = wire::array_field(&control, 3).expect("members");
    assert_eq!(members.len(), 1, "genesis 只有一台成员");
    let member = wire::value_map(&members[0]).expect("member");
    assert_eq!(
        wire::bytes_field(&member, 0).expect("device id"),
        DEVICE.to_vec(),
        "同一 device_id 在各自 vault 里都是独立成员"
    );

    // ---- 对象：PUT part / GET part / GET object / complete / PUT 幂等 ----
    let (object_envelope, seq) = upload_object(
        app,
        vault_id,
        &device_token,
        ObjectSpec::update(OBJECT_ID, 1, 64),
    )
    .await;
    assert_eq!(seq, Some(1), "每个 vault 的 log 序号都从 1 开始");

    let part_uri = v2(&vault_id, &format!("/objects/{}/parts/0", b64(&OBJECT_ID)));
    let (status, body) = call(app, get(&part_uri, Some(&device_token))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, object_envelope, "GET part 必须回本 vault 的原始信封");

    let object_uri = v2(&vault_id, &format!("/objects/{}", b64(&OBJECT_ID)));
    let (status, body) = call(app, get(&object_uri, Some(&device_token))).await;
    assert_eq!(status, StatusCode::OK);
    let object = cbor_fields(&body);
    assert_eq!(wire::text_field(&object, 9).expect("state"), "complete");
    assert_eq!(wire::array_field(&object, 12).expect("parts").len(), 1);

    // ---- 批量 updates（第二个对象，同一局部 id）----
    let batch_envelope =
        single_part_envelope(ObjectType::Update, vault_id, BATCH_OBJECT_ID, 1, 2, 64);
    let batch_part = v2(
        &vault_id,
        &format!("/objects/{}/parts/0", b64(&BATCH_OBJECT_ID)),
    );
    let (status, _) = call(
        app,
        put_bytes(&batch_part, &device_token, batch_envelope.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let batch_digest = digest_of(std::slice::from_ref(&batch_envelope)).expect("digest");
    let updates_uri = v2(&vault_id, "/updates");
    let (status, body) = call(
        app,
        post_cbor(
            &updates_uri,
            Some(&device_token),
            vec![(
                0,
                wire::array(vec![wire::map(vec![
                    (0, wire::bytes(BATCH_OBJECT_ID.to_vec())),
                    (1, wire::bytes(commit_for(&batch_digest))),
                    (2, wire::bytes(b"signature".to_vec())),
                ])]),
            )],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "updates: {}", error_code(&body));
    let updates = cbor_fields(&body);
    let results = wire::array_field(&updates, 0).expect("update results");
    let result = wire::value_map(&results[0]).expect("result");
    assert_eq!(wire::text_field(&result, 1).expect("status"), "accepted");
    assert_eq!(wire::opt_uint_field(&result, 2).expect("seq"), Some(2));

    // ---- log 拉取（游标 + 序号在两 vault 内独立）----
    let (status, body) = call(
        app,
        get(
            &v2(
                &vault_id,
                &format!("/log?after={}&limit=10", created.cursor),
            ),
            Some(&device_token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "log: {}", error_code(&body));
    let log = cbor_fields(&body);
    let events = wire::array_field(&log, 0).expect("events");
    let mut log_seqs = Vec::new();
    for event in &events {
        let event = wire::value_map(event).expect("event");
        log_seqs.push(wire::uint_field(&event, 0).expect("relay_seq"));
    }
    assert_eq!(log_seqs, vec![1, 2]);
    let first_event = wire::value_map(&events[0]).expect("first event");
    assert_eq!(
        wire::bytes_field(&first_event, 2).expect("object id"),
        OBJECT_ID.to_vec()
    );
    let log_next_cursor = wire::text_field(&log, 1).expect("next cursor");

    // ---- ack / 资产列表 / key-slots 读 ----
    let (status, body) = call(
        app,
        post_cbor(
            &v2(&vault_id, "/acks"),
            Some(&device_token),
            vec![(0, wire::uint(2)), (1, wire::uint(2)), (2, wire::uint(1))],
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "acks: {}",
        error_code(&body)
    );

    let (status, body) = call(
        app,
        get(
            &v2(&vault_id, "/objects?type=asset&epoch=1&through=2"),
            Some(&device_token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "asset list: {}", error_code(&body));
    assert!(wire::array_field(&cbor_fields(&body), 0)
        .expect("assets")
        .is_empty());

    let (status, body) = call(app, get(&v2(&vault_id, "/key-slots"), Some(&device_token))).await;
    assert_eq!(status, StatusCode::OK, "key-slots: {}", error_code(&body));
    assert!(wire::array_field(&cbor_fields(&body), 0)
        .expect("slots")
        .is_empty());

    // ---- 两份 baseline + bootstrap + lease 续期 ----
    let mut checkpoints = [[0u8; 16]; 2];
    for slot in &mut checkpoints {
        let snapshot = random_object_id();
        let manifest = random_object_id();
        upload_object(
            app,
            vault_id,
            &device_token,
            ObjectSpec::snapshot(snapshot, 64),
        )
        .await;
        upload_object(
            app,
            vault_id,
            &device_token,
            ObjectSpec::manifest(manifest, 32),
        )
        .await;
        let checkpoint = publish_checkpoint(app, vault_id, &device_token, snapshot, manifest).await;
        let (status, body) = call(
            app,
            get(
                &v2(&vault_id, &format!("/checkpoints/{}", b64(&checkpoint))),
                Some(&device_token),
            ),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "get checkpoint: {}",
            error_code(&body)
        );
        let detail = wire::map_field(&cbor_fields(&body), 0).expect("checkpoint detail");
        assert_eq!(
            wire::text_field(&detail, 1).expect("checkpoint state"),
            "published"
        );
        *slot = checkpoint;
    }

    let (status, body) = call(
        app,
        post_empty(&v2(&vault_id, "/bootstrap"), Some(&device_token)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "bootstrap: {}", error_code(&body));
    let bootstrap = cbor_fields(&body);
    // 只有 UPDATE 进 log（§2）；两份 baseline 的 snapshot/manifest 不占 relay_seq。
    assert_eq!(wire::uint_field(&bootstrap, 2).expect("head_seq"), 2);
    assert_eq!(wire::uint_field(&bootstrap, 4).expect("epoch"), 1);
    assert!(!wire::bool_field(&bootstrap, 7).expect("degraded"));
    let lease_id = wire::id16_field(&bootstrap, 5).expect("lease id");
    let lease_expires = wire::uint_field(&bootstrap, 6).expect("lease expires");

    let (status, body) = call(
        app,
        patch(
            &v2(&vault_id, &format!("/bootstrap-leases/{}", b64(&lease_id))),
            &device_token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "renew lease: {}", error_code(&body));
    let lease = cbor_fields(&body);
    assert_eq!(wire::id16_field(&lease, 0).expect("lease id"), lease_id);
    assert!(
        wire::uint_field(&lease, 1).expect("expires") >= lease_expires,
        "续租不得缩短期限"
    );

    // ---- 邀请：创建（管理）→ 兑换（公开）→ 批准（管理）----
    let canonical_invite = wire::encode(vec![(0, wire::bytes(sha256(&INVITE_TOKEN).to_vec()))])
        .expect("invite canonical body");
    let invite_fields = vec![
        (0, signed_wrapper(canonical_invite, b"management-signature")),
        (1, wire::bytes(INVITE_REQUEST_ID.to_vec())),
        (2, wire::uint(1)),
    ];
    let (status, body) = call(
        app,
        post_cbor(
            &v2(&vault_id, "/invites"),
            Some(&manager_token),
            invite_fields,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "invite: {}", error_code(&body));
    let invite_id = wire::id16_field(&cbor_fields(&body), 0).expect("invite id");

    let (status, body) = call(
        app,
        post_cbor(
            &v2(&vault_id, &format!("/invites/{}/redeem", b64(&invite_id))),
            None,
            vec![
                (0, wire::bytes(INVITE_TOKEN.to_vec())),
                (1, wire::bytes(PENDING_DEVICE.to_vec())),
                (2, wire::bytes(PENDING_SIGN_PK.to_vec())),
                (3, wire::bytes(PENDING_HPKE_PK.to_vec())),
                (4, wire::bytes(b"proof".to_vec())),
                (5, wire::bytes(b"proof-signature".to_vec())),
            ],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "redeem: {}", error_code(&body));
    assert_eq!(
        wire::id16_field(&cbor_fields(&body), 0).expect("pending device"),
        PENDING_DEVICE
    );

    let certificate = certificate_bytes(
        &vault_id,
        &PENDING_DEVICE,
        &PENDING_SIGN_PK,
        &PENDING_HPKE_PK,
        1,
    );
    let canonical_approve = wire::encode(vec![
        (0, wire::bytes(certificate.clone())),
        (1, wire::bytes(b"certificate-sig".to_vec())),
        (2, wire::bytes(GRANT_ID.to_vec())),
        (3, wire::bytes([1u8; 32].to_vec())),
        (4, wire::bytes(b"ciphertext".to_vec())),
        (5, wire::bytes(b"grant-signature".to_vec())),
        (6, wire::bytes(APPROVE_REQUEST_ID.to_vec())),
        (7, wire::uint(1)),
    ])
    .expect("approve canonical body");
    let approve_fields = vec![
        (
            0,
            signed_wrapper(canonical_approve, b"management-signature"),
        ),
        (1, wire::bytes(APPROVE_REQUEST_ID.to_vec())),
        (2, wire::uint(1)),
    ];
    let (status, body) = call(
        app,
        post_cbor(
            &v2(
                &vault_id,
                &format!("/devices/{}/approve", b64(&PENDING_DEVICE)),
            ),
            Some(&manager_token),
            approve_fields,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "approve: {}", error_code(&body));
    let approve = cbor_fields(&body);
    assert_eq!(
        wire::bytes_field(&approve, 0).expect("device id"),
        PENDING_DEVICE.to_vec()
    );
    assert_eq!(wire::uint_field(&approve, 3).expect("control_rev"), 2);

    // 新成员可用 v2 路由；self grant 只返回本 vault 的 grant。
    let pending_token_pre_rotation =
        auth_token(app, vault_id, Realm::Device, &PENDING_DEVICE).await;
    let (status, body) = call(
        app,
        get(
            &v2(&vault_id, "/devices/self/grants/latest"),
            Some(&pending_token_pre_rotation),
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "latest grant: {}",
        error_code(&body)
    );
    let grant = cbor_fields(&body);
    assert_eq!(wire::uint_field(&grant, 0).expect("epoch"), 1);
    assert_eq!(
        wire::bytes_field(&grant, 1).expect("device id"),
        PENDING_DEVICE.to_vec()
    );
    assert_eq!(
        wire::bytes_field(&grant, 4).expect("ciphertext"),
        b"ciphertext",
        "grant 必须来自本 vault 的 approve"
    );

    // control 已推进：2 个成员、control_rev=2。
    let (status, body) = call(app, get(&v2(&vault_id, "/control"), Some(&manager_token))).await;
    assert_eq!(status, StatusCode::OK);
    let control = cbor_fields(&body);
    assert_eq!(wire::uint_field(&control, 1).expect("control_rev"), 2);
    assert_eq!(wire::array_field(&control, 3).expect("members").len(), 2);

    // ---- rotation 基础路径：begin → current → view → grants → assets → abort ----
    let canonical_begin = wire::encode(vec![
        (0, wire::uint(1)),
        (1, wire::uint(2)),
        (2, wire::bytes(BEGIN_REQUEST_ID.to_vec())),
        (3, wire::array(Vec::new())),
    ])
    .expect("begin canonical body");
    let begin_fields = vec![
        (0, signed_wrapper(canonical_begin, b"management-signature")),
        (1, wire::bytes(BEGIN_REQUEST_ID.to_vec())),
        (2, wire::uint(2)),
        (3, wire::uint(1)),
        (4, wire::array(Vec::new())),
    ];
    let (status, body) = call(
        app,
        post_cbor(
            &v2(&vault_id, "/rotations"),
            Some(&manager_token),
            begin_fields,
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "begin rotation: {}",
        error_code(&body)
    );
    let begin = cbor_fields(&body);
    let view = wire::map_field(&begin, 0).expect("rotation view");
    let rotation_id = wire::id16_field(&view, 0).expect("rotation id");
    assert_eq!(wire::text_field(&view, 1).expect("state"), "frozen");
    assert_eq!(wire::uint_field(&view, 2).expect("from_epoch"), 1);
    assert_eq!(wire::uint_field(&view, 3).expect("to_epoch"), 2);

    // begin 让全部旧 token 失效（§24.1①），重认证后继续。
    let (status, body) = call(app, get(&v2(&vault_id, "/state"), Some(&device_token))).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "冻结后旧 token 必须立即失效");
    assert_eq!(error_code(&body), "REVOKED");
    let manager_token = auth_token(app, vault_id, Realm::Management, &manager_subject).await;
    let device_token = auth_token(app, vault_id, Realm::Device, &DEVICE).await;
    let pending_token = auth_token(app, vault_id, Realm::Device, &PENDING_DEVICE).await;

    // current（带 begin 的 request_id → 幂等回放），证明 v2 的 dedup 也按 vault 隔离。
    let (status, body) = call(
        app,
        get(
            &v2(
                &vault_id,
                &format!("/rotations/current?request_id={}", b64(&BEGIN_REQUEST_ID)),
            ),
            Some(&manager_token),
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "rotations/current: {}",
        error_code(&body)
    );
    let current = cbor_fields(&body);
    let current_view = wire::map_field(&current, 0).expect("current view");
    assert_eq!(
        wire::id16_field(&current_view, 0).expect("rotation id"),
        rotation_id
    );
    assert!(
        wire::opt_field(&current, 1).is_some(),
        "同 request_id 必须能找回 begin 响应"
    );

    let (status, body) = call(
        app,
        get(
            &v2(&vault_id, &format!("/rotations/{}", b64(&rotation_id))),
            Some(&manager_token),
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "get rotation: {}",
        error_code(&body)
    );
    let rotation = wire::map_field(&cbor_fields(&body), 0).expect("rotation view");
    assert_eq!(
        wire::id16_field(&rotation, 0).expect("rotation id"),
        rotation_id
    );
    assert_eq!(wire::text_field(&rotation, 1).expect("state"), "frozen");

    let (status, body) = call(
        app,
        get(
            &v2(
                &vault_id,
                &format!("/rotations/{}/grants", b64(&rotation_id)),
            ),
            Some(&manager_token),
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "rotation grants: {}",
        error_code(&body)
    );
    let grants = cbor_fields(&body);
    assert_eq!(wire::uint_field(&grants, 2).expect("grants_count"), 0);
    assert_eq!(
        wire::bytes_field(&grants, 1).expect("grants_digest").len(),
        32
    );

    let (status, body) = call(
        app,
        get(
            &v2(
                &vault_id,
                &format!("/rotations/{}/assets", b64(&rotation_id)),
            ),
            Some(&device_token),
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "rotation assets: {}",
        error_code(&body)
    );
    assert_eq!(
        wire::uint_field(&cbor_fields(&body), 1).expect("freeze_seq"),
        2
    );

    // frozen 状态下 leaves 不可作为 inclusion 证据：设备 409，管理 403（路由已接线 + realm 门禁）。
    let leaves_uri = v2(
        &vault_id,
        &format!("/rotations/{}/grants/leaves", b64(&rotation_id)),
    );
    let (status, body) = call(app, get(&leaves_uri, Some(&device_token))).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(error_code(&body), "INVALID_STATE");
    let (status, body) = call(app, get(&leaves_uri, Some(&manager_token))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(error_code(&body), "MANAGEMENT_REQUIRED");

    let canonical_abort = wire::encode(Vec::new()).expect("abort canonical body");
    let abort_fields = vec![
        (0, signed_wrapper(canonical_abort, b"management-signature")),
        (1, wire::bytes(ABORT_REQUEST_ID.to_vec())),
        (2, wire::uint(3)),
    ];
    let (status, body) = call(
        app,
        post_cbor(
            &v2(
                &vault_id,
                &format!("/rotations/{}/abort", b64(&rotation_id)),
            ),
            Some(&manager_token),
            abort_fields,
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "abort rotation: {}",
        error_code(&body)
    );
    let aborted = wire::map_field(&cbor_fields(&body), 0).expect("aborted view");
    assert_eq!(wire::text_field(&aborted, 1).expect("state"), "aborted");

    let (status, body) = call(app, get(&v2(&vault_id, "/state"), Some(&device_token))).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "state after abort: {}",
        error_code(&body)
    );
    let state = cbor_fields(&body);
    assert_eq!(wire::text_field(&state, 3).expect("vault state"), "active");
    let control_rev = wire::uint_field(&state, 5).expect("control_rev");

    // 批准后的成员在轮换 abort 后同样可读（同一 device_id 在本 vault 独立成立）。
    let (status, body) = call(app, get(&v2(&vault_id, "/state"), Some(&pending_token))).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "member state: {}",
        error_code(&body)
    );

    // ---- 管理端独立吊销（ADR 0010）：只断访问，control_rev 不变 ----
    let canonical_revoke = wire::encode(vec![
        (0, wire::bytes(PENDING_DEVICE.to_vec())),
        (1, wire::bytes(REVOKE_REQUEST_ID.to_vec())),
        (2, wire::uint(control_rev)),
    ])
    .expect("revoke canonical body");
    let revoke_fields = vec![
        (0, signed_wrapper(canonical_revoke, b"management-signature")),
        (1, wire::bytes(REVOKE_REQUEST_ID.to_vec())),
        (2, wire::uint(control_rev)),
    ];
    let revoke_uri = v2(
        &vault_id,
        &format!("/devices/{}/revoke", b64(&PENDING_DEVICE)),
    );
    let (status, body) = call(
        app,
        post_cbor(&revoke_uri, Some(&manager_token), revoke_fields),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "revoke: {}", error_code(&body));
    let revoked = cbor_fields(&body);
    assert_eq!(
        wire::id16_field(&revoked, 0).expect("target"),
        PENDING_DEVICE
    );
    assert_eq!(wire::text_field(&revoked, 1).expect("status"), "revoked");
    assert!(wire::bool_field(&revoked, 2).expect("changed"));
    assert_eq!(
        wire::uint_field(&revoked, 4).expect("control_rev"),
        control_rev,
        "独立吊销不推进 control_rev"
    );
    let auth_rev_after_revoke = wire::uint_field(&revoked, 3).expect("auth_rev");
    // 旧 token 立即失效（auth_rev 推进）。
    let (status, _) = call(app, get(&v2(&vault_id, "/state"), Some(&device_token))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // 第一次吊销推进了 auth_rev → 先重认证，才能验证幂等重试。
    let manager_token = auth_token(app, vault_id, Realm::Management, &manager_subject).await;
    // 同目标再次吊销（不同 request_id）→ 幂等 changed=false，不再推进版本。
    let canonical_again = wire::encode(vec![
        (0, wire::bytes(PENDING_DEVICE.to_vec())),
        (1, wire::bytes(REVOKE_AGAIN_REQUEST_ID.to_vec())),
        (2, wire::uint(control_rev)),
    ])
    .expect("revoke-again canonical body");
    let again_fields = vec![
        (0, signed_wrapper(canonical_again, b"management-signature")),
        (1, wire::bytes(REVOKE_AGAIN_REQUEST_ID.to_vec())),
        (2, wire::uint(control_rev)),
    ];
    let (status, body) = call(
        app,
        post_cbor(&revoke_uri, Some(&manager_token), again_fields),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "revoke again: {}",
        error_code(&body)
    );
    let again = cbor_fields(&body);
    assert!(!wire::bool_field(&again, 2).expect("changed"));
    assert_eq!(
        wire::uint_field(&again, 3).expect("auth_rev"),
        auth_rev_after_revoke
    );

    // 第一次吊销已让旧 token 失效；重新认证后再取事实（facts 里的 token 必须仍然可用）。
    let device_token = auth_token(app, vault_id, Realm::Device, &DEVICE).await;

    VaultFacts {
        device_token,
        manager_token,
        cursor0: created.cursor.clone(),
        object_envelope,
        log_seqs,
        log_next_cursor,
        checkpoints,
        lease_id,
        invite_id,
        rotation_id,
        control_rev,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_vaults_complete_v2_business_flows_without_crossing_local_ids() {
    let fx = block(|| fixture("v2-data-two-vaults"));
    let app = app(&fx);
    let created_b = create_v2_vault(&app, &fx, VAULT_B).await;
    let created_c = create_v2_vault(&app, &fx, VAULT_C).await;
    let facts_b = run_flow(&app, &fx, VAULT_B, &created_b).await;
    let facts_c = run_flow(&app, &fx, VAULT_C, &created_c).await;

    // 1) **相同局部 ID** 不串：同一 object_id 在两个 vault 各自有独立信封/分片。
    let part_b = v2(&VAULT_B, &format!("/objects/{}/parts/0", b64(&OBJECT_ID)));
    let part_c = v2(&VAULT_C, &format!("/objects/{}/parts/0", b64(&OBJECT_ID)));
    let (status, body_b) = call(&app, get(&part_b, Some(&facts_b.device_token))).await;
    assert_eq!(status, StatusCode::OK);
    let (status, body_c) = call(&app, get(&part_c, Some(&facts_c.device_token))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body_b, facts_b.object_envelope);
    assert_eq!(body_c, facts_c.object_envelope);
    assert_ne!(body_b, body_c, "相同 object_id 的内容必须彼此独立");

    // 相同 log 序号：两边都从 1 开始，但游标（HMAC key 按 vault）与事件字节不同。
    assert_eq!(facts_b.log_seqs, vec![1, 2]);
    assert_eq!(facts_c.log_seqs, vec![1, 2]);
    assert_ne!(facts_b.log_next_cursor, facts_c.log_next_cursor);
    assert_ne!(
        facts_b.cursor0, facts_c.cursor0,
        "seq=0 游标也按 vault 派生"
    );
    assert!(!facts_b.log_next_cursor.is_empty());

    // 相同 request_id（邀请 / begin）互不影响：两边都建出了各自的邀请与 rotation。
    assert_ne!(facts_b.invite_id, facts_c.invite_id);
    assert_ne!(facts_b.rotation_id, facts_c.rotation_id);
    assert_ne!(facts_b.lease_id, facts_c.lease_id);

    // 相同 checkpoint_id 不存在跨库：B 的 cid 在 C 里 404（不泄露、不串数据）。
    let (status, body) = call(
        &app,
        get(
            &v2(
                &VAULT_C,
                &format!("/checkpoints/{}", b64(&facts_b.checkpoints[0])),
            ),
            Some(&facts_c.device_token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{}", error_code(&body));
    // 反向同样。
    let (status, _) = call(
        &app,
        get(
            &v2(
                &VAULT_B,
                &format!("/checkpoints/{}", b64(&facts_c.checkpoints[0])),
            ),
            Some(&facts_b.device_token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // 跨 vault token：一律 404（不泄露目标租户是否存在），且绝不放行。
    for (route_vault, token) in [
        (VAULT_B, &facts_c.device_token),
        (VAULT_C, &facts_b.device_token),
        (VAULT_B, &facts_c.manager_token),
        (VAULT_C, &facts_b.manager_token),
    ] {
        let (status, _) = call(&app, get(&v2(&route_vault, "/state"), Some(token))).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "跨 vault token 必须 404");
    }

    // 相同 request_id 的幂等回放只能在本 vault 命中：C 的 begin 回放给出 C 的 rotation。
    let (status, body) = call(
        &app,
        get(
            &v2(
                &VAULT_C,
                &format!("/rotations/current?request_id={}", b64(&BEGIN_REQUEST_ID)),
            ),
            Some(&facts_c.manager_token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let replay = wire::bytes_field(&cbor_fields(&body), 1).expect("replayed response");
    let replayed_view = cbor_fields(&replay);
    let replayed_user = wire::map_field(&replayed_view, 0).expect("replayed view");
    assert_eq!(
        wire::id16_field(&replayed_user, 0).expect("rotation id"),
        facts_c.rotation_id,
        "同 request_id 的回放必须属于本 vault"
    );

    // 两个 vault 的 ack/lease/rotation 事实都各自成立（每条流都断言过一次），这里再确认
    // control_rev 相同（两边都走了 invite+approve+begin+abort）。
    assert_eq!(facts_b.control_rev, 4);
    assert_eq!(facts_c.control_rev, 4);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v2_auth_boundaries_and_v1_legacy_scope_stay_separate() {
    let fx = block(|| fixture("v2-data-auth-boundaries"));
    let app = app(&fx);
    // B 用一台**只存在于 B** 的设备，证明 v1 legacy 默认 vault 不会按 subject 猜租户。
    create_v2_vault_with_device(&app, &fx, VAULT_B, DEVICE_ONLY_V2).await;
    let token_b = auth_token(&app, VAULT_B, Realm::Device, &DEVICE_ONLY_V2).await;
    let manager_subject = Realm::management_subject(&fx.manager_pub);
    let manager_b = auth_token(&app, VAULT_B, Realm::Management, &manager_subject).await;
    let token_a = block(|| fx.device_token());

    // v2 token 只能进它自己的 vault。
    let (status, _) = call(&app, get(&v2(&VAULT_B, "/state"), Some(&token_b))).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call(&app, get(&v2(&VAULT_B, "/state"), Some(&token_a))).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "v1 legacy token 只能到它所属的 vault（A），到 B 的 v2 路由 404"
    );
    let (status, _) = call(
        &app,
        get(
            &format!("/v1/vaults/{}/state", b64(&fx.vault_id)),
            Some(&token_b),
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "v2 token 不能访问 v1 legacy 默认 vault（另一个 vault）"
    );

    // 同一 vault 内 token 的授权边界是 **vault scope**，不是 URL 版本：v1 为 A 签发的
    // token 对 A 的 v2 路由同样有效（同一租户、同一 realm，无权限提升）。这是 S2c 的
    // 明确语义（区分 audience 需要 DDL，非本片目标）。
    let (status, _) = call(
        &app,
        get(
            &format!("/v2/vaults/{}/state", b64(&fx.vault_id)),
            Some(&token_a),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // v1 challenge 对"只在 B 里存在的设备"仍 403 REVOKED（不按 subject 猜租户）。
    let (status, body) = call(
        &app,
        post_cbor(
            "/v1/auth/challenges",
            None,
            vec![
                (0, wire::uint(1)),
                (1, wire::bytes(DEVICE_ONLY_V2.to_vec())),
            ],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(error_code(&body), "REVOKED");

    // challenge 跨 vault：A 的 v1 challenge 在 B 的 v2 tokens 端点查不到 → 401。
    let (status, body) = call(
        &app,
        post_cbor(
            "/v1/auth/challenges",
            None,
            vec![(0, wire::uint(1)), (1, wire::bytes(DEVICE.to_vec()))],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let challenge_a = wire::id16_field(&cbor_fields(&body), 0).expect("challenge id");
    let (status, body) = call(
        &app,
        post_cbor(
            &v2(&VAULT_B, "/auth/tokens"),
            None,
            vec![
                (0, wire::bytes(challenge_a.to_vec())),
                (1, wire::bytes(b"challenge-message".to_vec())),
                (2, wire::bytes(b"signature".to_vec())),
            ],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(error_code(&body), "CHALLENGE_INVALID");

    // 反向：B 的 v2 challenge 在 v1 tokens 端点也查不到。
    let (status, body) = call(
        &app,
        post_cbor(
            &v2(&VAULT_B, "/auth/challenges"),
            None,
            vec![
                (0, wire::uint(1)),
                (1, wire::bytes(DEVICE_ONLY_V2.to_vec())),
            ],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let challenge_b = wire::id16_field(&cbor_fields(&body), 0).expect("challenge id");
    let (status, body) = call(
        &app,
        post_cbor(
            "/v1/auth/tokens",
            None,
            vec![
                (0, wire::bytes(challenge_b.to_vec())),
                (1, wire::bytes(b"challenge-message".to_vec())),
                (2, wire::bytes(b"signature".to_vec())),
            ],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(error_code(&body), "CHALLENGE_INVALID");

    // 管理 realm 同样按 vault 绑定：B 的管理 token 在 A 的 v2 管理路由上 404。
    let (status, _) = call(
        &app,
        post_empty(&v2(&fx.vault_id, "/invites"), Some(&manager_b)),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // v1 legacy 路由不受 v2 影响：A 的 device token 在 v1/v2 都正常。
    for uri in [
        format!("/v1/vaults/{}/state", b64(&fx.vault_id)),
        format!("/v1/vaults/{}/control?after_rev=0", b64(&fx.vault_id)),
    ] {
        let (status, _) = call(&app, get(&uri, Some(&token_a))).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v2_route_inventory_enforces_realms_without_404() {
    let fx = block(|| fixture("v2-data-realms"));
    let app = app(&fx);
    create_v2_vault(&app, &fx, VAULT_C).await;
    let manager_subject = Realm::management_subject(&fx.manager_pub);
    let device = auth_token(&app, VAULT_C, Realm::Device, &DEVICE).await;
    let manager = auth_token(&app, VAULT_C, Realm::Management, &manager_subject).await;
    let object = b64(&[0x42u8; 32]);
    let cid = b64(&[0x11u8; 16]);
    let rid = b64(&[0x22u8; 16]);
    let lid = b64(&[0x33u8; 16]);

    // 设备专用路由：管理 token → 403 MANAGEMENT_REQUIRED（不是 404，证明路由存在）。
    let device_only: Vec<(&str, Request<Body>)> = vec![
        ("log", get(&v2(&VAULT_C, "/log"), Some(&manager))),
        ("acks", post_empty(&v2(&VAULT_C, "/acks"), Some(&manager))),
        (
            "updates",
            post_empty(&v2(&VAULT_C, "/updates"), Some(&manager)),
        ),
        (
            "put part",
            put_bytes(
                &v2(&VAULT_C, &format!("/objects/{object}/parts/0")),
                &manager,
                Vec::new(),
            ),
        ),
        (
            "complete",
            post_empty(
                &v2(&VAULT_C, &format!("/objects/{object}/complete")),
                Some(&manager),
            ),
        ),
        (
            "objects",
            get(&v2(&VAULT_C, "/objects?type=asset"), Some(&manager)),
        ),
        (
            "checkpoints",
            post_empty(&v2(&VAULT_C, "/checkpoints"), Some(&manager)),
        ),
        (
            "refs",
            post_empty(
                &v2(&VAULT_C, &format!("/checkpoints/{cid}/refs")),
                Some(&manager),
            ),
        ),
        (
            "publish",
            post_empty(
                &v2(&VAULT_C, &format!("/checkpoints/{cid}/publish")),
                Some(&manager),
            ),
        ),
        (
            "bootstrap",
            post_empty(&v2(&VAULT_C, "/bootstrap"), Some(&manager)),
        ),
        (
            "lease",
            patch(&v2(&VAULT_C, &format!("/bootstrap-leases/{lid}")), &manager),
        ),
        (
            "latest grant",
            get(&v2(&VAULT_C, "/devices/self/grants/latest"), Some(&manager)),
        ),
        (
            "put slot",
            put_bytes(
                &v2(&VAULT_C, &format!("/key-slots/{object}")),
                &manager,
                Vec::new(),
            ),
        ),
        (
            "activate slot",
            post_empty(
                &v2(&VAULT_C, "/key-slots/password/activate"),
                Some(&manager),
            ),
        ),
        (
            "leaves",
            get(
                &v2(&VAULT_C, &format!("/rotations/{rid}/grants/leaves")),
                Some(&manager),
            ),
        ),
    ];
    for (label, request) in device_only {
        let (status, body) = call(&app, request).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "{label} 必须要求设备 realm（404 说明路由缺失）"
        );
        assert_eq!(error_code(&body), "MANAGEMENT_REQUIRED", "{label}");
    }

    // 管理专用路由：设备 token → 403 MANAGEMENT_REQUIRED。
    let management_only: Vec<(&str, Request<Body>)> = vec![
        (
            "invites",
            post_empty(&v2(&VAULT_C, "/invites"), Some(&device)),
        ),
        (
            "approve",
            post_empty(
                &v2(
                    &VAULT_C,
                    &format!("/devices/{}/approve", b64(&PENDING_DEVICE)),
                ),
                Some(&device),
            ),
        ),
        (
            "revoke",
            post_empty(
                &v2(
                    &VAULT_C,
                    &format!("/devices/{}/revoke", b64(&PENDING_DEVICE)),
                ),
                Some(&device),
            ),
        ),
        (
            "rotations",
            post_empty(&v2(&VAULT_C, "/rotations"), Some(&device)),
        ),
        (
            "grants",
            get(
                &v2(&VAULT_C, &format!("/rotations/{rid}/grants")),
                Some(&device),
            ),
        ),
        (
            "stage grant",
            put_bytes(
                &v2(
                    &VAULT_C,
                    &format!("/rotations/{rid}/grants/{}", b64(&PENDING_DEVICE)),
                ),
                &device,
                Vec::new(),
            ),
        ),
        (
            "restart",
            post_empty(
                &v2(&VAULT_C, &format!("/rotations/{rid}/restart")),
                Some(&device),
            ),
        ),
        (
            "revoke-more",
            post_empty(
                &v2(&VAULT_C, &format!("/rotations/{rid}/revoke-more")),
                Some(&device),
            ),
        ),
        (
            "activate",
            post_empty(
                &v2(&VAULT_C, &format!("/rotations/{rid}/activate")),
                Some(&device),
            ),
        ),
        (
            "abort",
            post_empty(
                &v2(&VAULT_C, &format!("/rotations/{rid}/abort")),
                Some(&device),
            ),
        ),
    ];
    for (label, request) in management_only {
        let (status, body) = call(&app, request).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "{label} 必须要求管理 realm（404 说明路由缺失）"
        );
        assert_eq!(error_code(&body), "MANAGEMENT_REQUIRED", "{label}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v2_business_rate_limit_returns_429_without_touching_v1() {
    let fx = block(|| Fixture::with_v2_data_limit("v2-data-rate", RateLimitSpec::new(2, 60)));
    let app = app(&fx);
    create_v2_vault(&app, &fx, VAULT_B).await;
    let token_b = auth_token(&app, VAULT_B, Realm::Device, &DEVICE).await;
    let token_a = block(|| fx.device_token());

    // 业务入口：2 次放行，第 3 次 429（固定窗口实例级）。
    for _ in 0..2 {
        let (status, _) = call(&app, get(&v2(&VAULT_B, "/state"), Some(&token_b))).await;
        assert_eq!(status, StatusCode::OK);
    }
    let (status, body) = call(&app, get(&v2(&VAULT_B, "/state"), Some(&token_b))).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(error_code(&body), "RATE_LIMITED");
    assert!(retryable(&body), "429 必须标 retryable");

    // v2 auth 不受业务限流影响（该面在夹具里被关闭）；v1 完全不受影响。
    let _ = auth_token(&app, VAULT_B, Realm::Device, &DEVICE).await;
    for _ in 0..3 {
        let (status, _) = call(
            &app,
            get(
                &format!("/v1/vaults/{}/state", b64(&fx.vault_id)),
                Some(&token_a),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "v1 不受 v2 业务限流影响");
    }
    // v2 的 429 不消耗任何配额/不建库：vault 列表仍是 A+B。
    assert_eq!(
        block(|| fx.db.inner().read_instance(|conn| {
            Ok(conn.query_row("SELECT COUNT(*) FROM vaults", [], |row| {
                row.get::<_, i64>(0)
            })?)
        }))
        .expect("count vaults"),
        2
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unauthorized_errors_do_not_leak_vault_existence() {
    let fx = block(|| fixture("v2-data-no-leak"));
    let app = app(&fx);
    create_v2_vault(&app, &fx, VAULT_B).await;
    // C 是**真实存在**的另一个租户：跨 vault 与"不存在"必须不可区分。
    create_v2_vault(&app, &fx, VAULT_C).await;
    let token_b = auth_token(&app, VAULT_B, Realm::Device, &DEVICE).await;
    let unknown: Id16 = [0xEEu8; 16];

    // 无 token：已知 vault 与未知 vault 的错误体必须逐字节一致（401）。
    let (status_known, body_known) = call(&app, get(&v2(&VAULT_B, "/control"), None)).await;
    let (status_unknown, body_unknown) = call(&app, get(&v2(&unknown, "/control"), None)).await;
    assert_eq!(status_known, StatusCode::UNAUTHORIZED);
    assert_eq!(status_unknown, StatusCode::UNAUTHORIZED);
    assert_eq!(body_known, body_unknown);
    assert_eq!(error_code(&body_known), "UNAUTHORIZED");

    // 有效 token 但路由到别的 vault：已知/未知都 404 且响应体一致。
    let (status_known, body_known) =
        call(&app, get(&v2(&unknown, "/control"), Some(&token_b))).await;
    let (status_unknown, body_unknown) = call(
        &app,
        get(
            &format!("/v2/vaults/{}/control", b64(&VAULT_C)),
            Some(&token_b),
        ),
    )
    .await;
    assert_eq!(status_known, StatusCode::NOT_FOUND);
    assert_eq!(status_unknown, StatusCode::NOT_FOUND);
    assert_eq!(body_known, body_unknown);
    assert_eq!(error_code(&body_known), "NOT_FOUND");

    // 错误体里不得出现任何 vault id 的编码（十六进制/base64url）。
    let text = String::from_utf8_lossy(&body_known).to_ascii_lowercase();
    for vault in [VAULT_B, VAULT_C, unknown] {
        assert!(!text.contains(&b64(&vault).to_ascii_lowercase()));
        assert!(!text.contains(&relay::ids::hex_lower(&vault)));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v2_wake_websocket_pushes_state_and_rejects_cross_vault_token() {
    use futures_util::StreamExt;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::Message;

    let fx = block(|| fixture("v2-data-wake"));
    let state = AppState::new(fx.db.inner().clone(), fx.cfg.clone(), fx.hook.clone());
    let app = router(state.clone());
    let created = create_v2_vault(&app, &fx, VAULT_B).await;
    assert!(
        !created.cursor.is_empty(),
        "创建响应必须带初始游标（与 v1 相同）"
    );
    let token_b = auth_token(&app, VAULT_B, Realm::Device, &DEVICE).await;
    let token_a = block(|| fx.device_token());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let served = app.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, served).await;
    });

    let connect = |uri: String, token: &str| {
        let token = token.to_string();
        async move {
            let mut request = uri.into_client_request().expect("ws request");
            request.headers_mut().insert(
                "Authorization",
                format!("Bearer {token}").parse().expect("header"),
            );
            tokio_tungstenite::connect_async(request).await
        }
    };

    // v2 wake：初始帧必须是同一水位 JSON（v1 同格式）。
    let wake_uri = format!("ws://{addr}{}", v2(&VAULT_B, "/wake"));
    let (mut socket, _) = connect(wake_uri.clone(), &token_b)
        .await
        .expect("v2 ws connect");
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

    // 写一条并广播 → 连接收到新水位（不靠 sleep：以帧为收敛点）。
    upload_object(
        &app,
        VAULT_B,
        &token_b,
        ObjectSpec::update([0x51u8; 32], 1, 32),
    )
    .await;
    state.notify();
    // 收敛点：在有限时间内收到 head_seq=1 的帧（首 tick 也会立即推一次当前水位，
    // 因此不能假定"下一帧就是新水位"）。
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut new_head = false;
    while tokio::time::Instant::now() < deadline {
        let frame = tokio::time::timeout_at(deadline, socket.next())
            .await
            .expect("wake frame timeout")
            .expect("wake frame")
            .expect("wake frame ok");
        let text = match frame {
            Message::Text(text) => text.to_string(),
            other => panic!("unexpected frame: {other:?}"),
        };
        let value: serde_json::Value = serde_json::from_str(&text).expect("wake json");
        if value["head_seq"] == 1 {
            new_head = true;
            break;
        }
    }
    assert!(new_head, "v2 wake 必须推送本 vault 的新水位");

    // 跨 vault token 一律拒绝升级：
    // - A 的 v1 legacy token → B 的 v2 wake；
    // - B 的 v2 token → v1 legacy A 的 wake。
    assert!(connect(wake_uri, &token_a).await.is_err());
    let v1_wake = format!("ws://{addr}/v1/vaults/{}/wake", b64(&fx.vault_id));
    assert!(connect(v1_wake, &token_b).await.is_err());
}
