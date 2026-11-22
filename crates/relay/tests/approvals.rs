//! ADR 0016 **S4d2**：relay 审批收件箱（通道 B）HTTP 全链路。
//!
//! 覆盖：
//! 1. 创建：realm3 主体/cap 必须等于信封的 A 槽；非 canonical/篡改/缺 A 签/非 Device 槽
//!    缺签/单签 kind/满签包/超限全部拒绝；CAS/到期/错 cap/同设备两 cap 全部拒绝；
//!    幂等 `(V,R,h)` 返回原 job，同 R 异 h → 409；
//! 2. 列表/读取：只看自己的待办（recipient / `role=initiator`）、分页 ≤50、状态过滤、
//!    非 A/B 的 ACTIVE 管理设备一律 404、跨租户 token 401；
//! 3. 加签：固定槽位（realm3 主体/cap 必须匹配该 Device 槽）、域验签、幂等、冲突 409、
//!    撤权/head 前进后失败；
//! 4. 终态：提交后 `EXECUTED`（dedup 逐字节命中）、到期 `EXPIRED`、rev 变化 `STALE`、
//!    取消 `CANCELLED`（只停止协调，满签包仍可 submit）、24h 墓碑删除；
//! 5. 限额：同源设备 ≤8 待办（507）、vault 待办分页、limit 上限收敛。
//!
//! 全部走真实 relay 路由 + 真实 Ed25519 + `FakeClock`，**无 sleep**。
//!
//! 注意：relay 的 realm3 bearer 绑定 `authz_rev/authority_rev`；每次控制提交后旧 token
//! 立即失效（ADR 0015 §鉴权），因此用例在控制链推进后会重新登录——这本身就是安全断言。

mod support;

use axum::http::StatusCode;
use axum::Router;
use ed25519_dalek::{Signer as _, SigningKey};
use relay::ids::{hex_lower, sha256, Id16};
use relay::wire;
use support::v2::*;
use support::*;

use menghunote_sync_proto as proto;

const CAP_A: [u8; 16] = [0xC1; 16];
const CAP_B: [u8; 16] = [0xC2; 16];
/// A/B 的 bootstrap cap TTL：拉到 7 天，让"到期/24h 墓碑"用例仍在 cap 有效期内。
const CAP_TTL: u64 = 7 * 24 * 60 * 60;

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

/// 升级 + 返回 bootstrap caps（A/B 的授权证据，逐字节等于 relay DB 里的 canonical cap）。
async fn setup(label: &str) -> (V2Fixture, Router, Vec<proto::ManagementCapV2>) {
    let v2 = new_fixture(label);
    let app = app(&v2);
    let mut parts = v2.default_upgrade_parts();
    // 拉长 A/B 的 cap 有效期（默认 1 天不足以覆盖 24h 墓碑用例）。
    parts.caps = vec![
        v2.bootstrap_cap(CAP_A, device_a(&v2), proto::CapScope::ALL, CAP_TTL),
        v2.bootstrap_cap(CAP_B, device_b(&v2), proto::CapScope::ALL, CAP_TTL),
    ];
    let caps = parts.caps.clone();
    let operation = v2.upgrade_operation([0xA0; 16], &parts, v2.now_u64());
    let signed = v2.sign_operation(&operation);
    let (status, body) = submit(&app, &v2.vault_id(), &signed).await;
    assert_eq!(status, StatusCode::OK, "upgrade: {}", error_code(&body));
    (v2, app, caps)
}

fn device_a(v2: &V2Fixture) -> Id16 {
    v2.extra[0].0
}

fn device_b(v2: &V2Fixture) -> Id16 {
    v2.extra[1].0
}

fn key_a(v2: &V2Fixture) -> &SigningKey {
    &v2.extra[0].1
}

fn key_b(v2: &V2Fixture) -> &SigningKey {
    &v2.extra[1].1
}

async fn token_for(
    v2: &V2Fixture,
    app: &Router,
    device: Id16,
    key: &SigningKey,
    cap: Option<[u8; 16]>,
) -> String {
    authz_login(app, &v2.vault_id(), device, key, cap)
        .await
        .unwrap_or_else(|(status, body)| {
            panic!("authz login failed: {status} {}", error_code(&body))
        })
}

async fn login_a(v2: &V2Fixture, app: &Router) -> String {
    token_for(v2, app, device_a(v2), key_a(v2), Some(CAP_A)).await
}

async fn login_b(v2: &V2Fixture, app: &Router) -> String {
    token_for(v2, app, device_b(v2), key_b(v2), Some(CAP_B)).await
}

// ---------------------------------------------------------------- 包构造

/// 一张"即将被授予"的 cap（issued rev 绑定当前提交后值）。
fn new_cap(
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

/// 两签 `CapGrant`（门限 = 两张 CAP_ADMIN cap）。
fn grant_operation(
    v2: &V2Fixture,
    request_id: [u8; 16],
    cap: proto::ManagementCapV2,
    signers: &[(Id16, Id16)],
) -> proto::OperationV2 {
    v2.two_signer_operation(
        proto::ControlKindV2::CapGrant,
        request_id,
        proto::CapGrantPayloadV2 { cap }
            .encode()
            .expect("grant payload"),
        signers,
        v2.now_u64(),
    )
}

fn admin_signers(v2: &V2Fixture) -> [(Id16, Id16); 2] {
    [(device_a(v2), CAP_A), (device_b(v2), CAP_B)]
}

fn signer_index(operation: &proto::OperationV2, device: Id16) -> usize {
    operation
        .signers
        .iter()
        .position(|signer| signer.device_id == Some(device))
        .expect("device signer")
}

/// 按槽位角色签 `OperationV2`（测试不复刻协议逻辑，直接用 proto 的域函数）。
fn sign_slot(operation: &proto::OperationV2, index: usize, key: &SigningKey) -> [u8; 64] {
    let signer = &operation.signers[index];
    let input = match signer.role {
        proto::SignerRole::Device => proto::control_signing_input(operation, hash_fn),
        proto::SignerRole::CreatorUpgrade => proto::upgrade_signing_input(operation, hash_fn),
        proto::SignerRole::OfflineRecovery => proto::recover_signing_input(operation, hash_fn),
        proto::SignerRole::TargetPop => proto::pop_signing_input(operation, &signer.pk, hash_fn),
    }
    .expect("signing input");
    key.sign(&input).to_bytes()
}

/// canonical `ApprovalEnvelopeV1` 字节（ADR 0016 §共同包，键 0..6）。
fn envelope_bytes(
    operation: &proto::OperationV2,
    initiator_index: usize,
    signatures: &[(usize, [u8; 64])],
    caps: &[proto::ManagementCapV2],
) -> Vec<u8> {
    let op_bytes = operation.encode().expect("op encode");
    let digest = sha256(&op_bytes);
    let mut entries = signatures.to_vec();
    entries.sort_by_key(|(index, _)| *index);
    let mut caps = caps.to_vec();
    caps.sort_by_key(|cap| cap.cap_id);
    wire::encode(vec![
        (0, wire::uint(1)),
        (1, wire::bytes(op_bytes)),
        (2, wire::bytes(digest)),
        (
            3,
            wire::uint(u64::try_from(initiator_index).expect("index")),
        ),
        (
            4,
            wire::array(
                entries
                    .into_iter()
                    .map(|(index, signature)| {
                        wire::map(vec![
                            (0, wire::uint(u64::try_from(index).expect("index"))),
                            (1, wire::bytes(signature.to_vec())),
                        ])
                    })
                    .collect(),
            ),
        ),
        (
            5,
            wire::array(
                caps.iter()
                    .map(|cap| wire::bytes(cap.encode().expect("cap encode")))
                    .collect(),
            ),
        ),
        (
            6,
            wire::map(vec![
                (0, wire::uint(operation.expected_control_rev)),
                (1, wire::bytes(operation.prev_control_hash)),
                (2, wire::uint(operation.expected_authz_rev)),
                (3, wire::uint(operation.authority_rev)),
            ]),
        ),
    ])
    .expect("envelope encode")
}

/// A 单签的默认包（CapGrant：A/B 两张 CAP_ADMIN）。
fn default_envelope(
    v2: &V2Fixture,
    caps: &[proto::ManagementCapV2],
    request_id: [u8; 16],
    new_cap_id: [u8; 16],
) -> (proto::OperationV2, Vec<u8>) {
    let cap = new_cap(
        v2,
        new_cap_id,
        v2.genesis_device(),
        proto::CapScope::APPROVE,
        3_600,
    );
    let operation = grant_operation(v2, request_id, cap, &admin_signers(v2));
    let index = signer_index(&operation, device_a(v2));
    let signature = sign_slot(&operation, index, key_a(v2));
    let envelope = envelope_bytes(&operation, index, &[(index, signature)], caps);
    (operation, envelope)
}

// ---------------------------------------------------------------- HTTP 驱动

fn approvals_uri(v2: &V2Fixture, suffix: &str) -> String {
    format!("{}/approvals{suffix}", hex_path(&v2.vault_id()))
}

async fn post_envelope(
    app: &Router,
    v2: &V2Fixture,
    token: Option<&str>,
    envelope: &[u8],
) -> (StatusCode, Vec<u8>) {
    call(
        app,
        post_cbor(
            &approvals_uri(v2, ""),
            token,
            vec![(0, wire::bytes(envelope.to_vec()))],
        ),
    )
    .await
}

async fn list_jobs(
    app: &Router,
    v2: &V2Fixture,
    token: &str,
    query: &str,
) -> (StatusCode, Vec<u8>) {
    let uri = approvals_uri(v2, query);
    call(app, get(&uri, Some(token))).await
}

async fn get_job(
    app: &Router,
    v2: &V2Fixture,
    token: &str,
    request_id: &Id16,
) -> (StatusCode, Vec<u8>) {
    let uri = approvals_uri(v2, &format!("/{}", hex_lower(request_id)));
    call(app, get(&uri, Some(token))).await
}

async fn post_signature(
    app: &Router,
    v2: &V2Fixture,
    token: &str,
    request_id: &Id16,
    signature: &[u8; 64],
) -> (StatusCode, Vec<u8>) {
    let uri = format!(
        "{}/approvals/{}/signatures",
        hex_path(&v2.vault_id()),
        hex_lower(request_id)
    );
    call(
        app,
        post_cbor(
            &uri,
            Some(token),
            vec![(0, wire::bytes(signature.to_vec()))],
        ),
    )
    .await
}

async fn post_cancel(
    app: &Router,
    v2: &V2Fixture,
    token: &str,
    request_id: &Id16,
) -> (StatusCode, Vec<u8>) {
    let uri = format!(
        "{}/approvals/{}/cancel",
        hex_path(&v2.vault_id()),
        hex_lower(request_id)
    );
    let request = axum::http::Request::builder()
        .method("POST")
        .uri(&uri)
        .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
        .body(axum::body::Body::empty())
        .expect("request");
    call(app, request).await
}

fn job_from_create(body: &[u8]) -> wire::Fields {
    wire::map_field(&cbor_fields(body), 0).expect("job map")
}

fn job_from_response(body: &[u8]) -> wire::Fields {
    wire::map_field(&cbor_fields(body), 0).expect("job map")
}

fn job_status(job: &wire::Fields) -> String {
    wire::text_field(job, 2).expect("status")
}

fn job_request_id(job: &wire::Fields) -> Id16 {
    wire::id16_field(job, 0).expect("request id")
}

fn list_entries(body: &[u8]) -> Vec<wire::Fields> {
    wire::array_field(&cbor_fields(body), 0)
        .expect("jobs")
        .iter()
        .map(|value| wire::value_map(value).expect("job"))
        .collect()
}

// ---------------------------------------------------------------- 1. 正例全链路

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_read_sign_submit_then_executed() {
    let (v2, app, caps) = setup("s4d2-happy").await;
    let token_a = login_a(&v2, &app).await;
    let token_b = login_b(&v2, &app).await;
    let request_id = [0x71; 16];
    let (operation, envelope) = default_envelope(&v2, &caps, request_id, [0xE1; 16]);

    // A 创建：201 + PENDING + recipient = B。
    let (status, body) = post_envelope(&app, &v2, Some(&token_a), &envelope).await;
    assert_eq!(status, StatusCode::CREATED, "create: {}", error_code(&body));
    let job = job_from_create(&body);
    assert!(wire::bool_field(&cbor_fields(&body), 1).expect("created"));
    assert_eq!(job_status(&job), "PENDING");
    assert_eq!(job_request_id(&job), request_id);
    assert_eq!(
        wire::bytes_field(&job, 6).expect("recipient"),
        device_b(&v2).to_vec()
    );
    assert_eq!(
        wire::uint_field(&job, 9).expect("not_after"),
        v2.now_u64() + 300
    );
    // expires_at = min(op.not_after, 参与 cap 到期) = op.not_after（cap TTL 7 天）。
    assert_eq!(
        wire::uint_field(&job, 11).expect("expires"),
        v2.now_u64() + 300
    );

    // A 的 outgoing 视图可见；B 的待办可见。
    let (status, body) = list_jobs(&app, &v2, &token_a, "?role=initiator").await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&body));
    assert_eq!(list_entries(&body).len(), 1);
    let (status, body) = list_jobs(&app, &v2, &token_b, "").await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&body));
    let entries = list_entries(&body);
    assert_eq!(entries.len(), 1);
    assert_eq!(job_status(&entries[0]), "PENDING");

    // B 读取详情：完整 op + A 的签名 + cap 证据。
    let (status, body) = get_job(&app, &v2, &token_b, &request_id).await;
    assert_eq!(status, StatusCode::OK, "read: {}", error_code(&body));
    let detail = cbor_fields(&body);
    assert_eq!(
        wire::bytes_field(&detail, 1).expect("op bytes"),
        operation.encode().expect("op encode")
    );
    assert_eq!(wire::array_field(&detail, 2).expect("signatures").len(), 1);
    assert_eq!(wire::array_field(&detail, 3).expect("caps").len(), 2);

    // B 加签 → READY。
    let index_b = signer_index(&operation, device_b(&v2));
    let signature_b = sign_slot(&operation, index_b, key_b(&v2));
    let (status, body) = post_signature(&app, &v2, &token_b, &request_id, &signature_b).await;
    assert_eq!(status, StatusCode::OK, "sign: {}", error_code(&body));
    assert_eq!(job_status(&job_from_response(&body)), "READY");

    // 任何持有者提交满签包 → 控制链前进。
    let index_a = signer_index(&operation, device_a(&v2));
    let signature_a = sign_slot(&operation, index_a, key_a(&v2));
    let signed = proto::SignedControlV2 {
        operation: operation.clone(),
        signatures: vec![signature_a, signature_b],
    };
    let (status, body) = submit(&app, &v2.vault_id(), &signed).await;
    assert_eq!(status, StatusCode::OK, "submit: {}", error_code(&body));
    let receipt = receipt(&body);
    assert_eq!(receipt.kind, 2, "CapGrant");

    // 控制链推进后旧 bearer 失效（authz rev 前进）→ 重新登录。
    let token_b = login_b(&v2, &app).await;
    // 再次读取 → EXECUTED（dedup 逐字节命中），默认待办视图为空。
    let (status, body) = get_job(&app, &v2, &token_b, &request_id).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "read executed: {}",
        error_code(&body)
    );
    let job = job_from_response(&body);
    assert_eq!(job_status(&job), "EXECUTED");
    assert_eq!(
        wire::opt_uint_field(&job, 19).expect("receipt"),
        Some(receipt.control_rev)
    );
    let (status, body) = list_jobs(&app, &v2, &token_b, "").await;
    assert_eq!(status, StatusCode::OK);
    assert!(list_entries(&body).is_empty(), "执行后不应再出现在待办");
    let (status, body) = list_jobs(&app, &v2, &token_b, "?status=EXECUTED").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list_entries(&body).len(), 1, "墓碑摘要 24h 内可查");
}

// ---------------------------------------------------------------- 2. 创建负例

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_requires_realm3_and_matching_initiator() {
    let (v2, app, caps) = setup("s4d2-create-auth").await;
    let request_id = [0x72; 16];
    let (_operation, envelope) = default_envelope(&v2, &caps, request_id, [0xE2; 16]);

    // 无 bearer → 401。
    let (status, body) = post_envelope(&app, &v2, None, &envelope).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{}", error_code(&body));
    // realm1 设备 token（无 cap）→ 403 MANAGEMENT_REQUIRED。
    let device_token = token_for(
        &v2,
        &app,
        v2.genesis_device(),
        &v2.fx.device_signing_key,
        None,
    )
    .await;
    let (status, body) = post_envelope(&app, &v2, Some(&device_token), &envelope).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{}", error_code(&body));
    assert_error(&body, "MANAGEMENT_REQUIRED");
    // B 的 realm3 token 上传"A 的信封"→ 404（realm3 主体/cap 必须匹配 A 槽）。
    let token_b = login_b(&v2, &app).await;
    let (status, body) = post_envelope(&app, &v2, Some(&token_b), &envelope).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{}", error_code(&body));
    assert_error(&body, "NOT_FOUND");
    // 跨租户 token（同 relay 的第二个 vault）→ 401（token 只在签发 scope 内有效）。
    let second = tokio::task::block_in_place(|| v2.create_second_vault([0x5A; 16]));
    let uri = format!("/v2/vaults/{}/approvals", hex_lower(&second.vault_id));
    let token_a = login_a(&v2, &app).await;
    let (status, body) = call(
        &app,
        post_cbor(
            &uri,
            Some(&token_a),
            vec![(0, wire::bytes(envelope.clone()))],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{}", error_code(&body));
    // 原始 vault 上仍然创建成功（证明上面的失败没有副作用）。
    let (status, body) = post_envelope(&app, &v2, Some(&token_a), &envelope).await;
    assert_eq!(status, StatusCode::CREATED, "{}", error_code(&body));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_rejects_tampered_incomplete_and_unsupported_kinds() {
    let (v2, app, caps) = setup("s4d2-create-negative").await;
    let token_a = login_a(&v2, &app).await;
    let request_id = [0x73; 16];
    let (operation, envelope) = default_envelope(&v2, &caps, request_id, [0xE3; 16]);
    let index_a = signer_index(&operation, device_a(&v2));

    // 篡改签名（同一 h 但 64B 非真签）→ 400 SIGNATURE_INVALID。
    let mut bad = envelope.clone();
    {
        let value = proto::decode_cde(&bad).expect("cde");
        let proto::CdeValue::Map(mut fields) = value else {
            panic!("map")
        };
        fields[4] = (
            4,
            wire::array(vec![wire::map(vec![
                (0, wire::uint(u64::try_from(index_a).expect("index"))),
                (1, wire::bytes([0x11u8; 64])),
            ])]),
        );
        bad = proto::encode_cde(&proto::CdeValue::Map(fields)).expect("encode");
    }
    let (status, body) = post_envelope(&app, &v2, Some(&token_a), &bad).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{}", error_code(&body));
    assert_error(&body, "SIGNATURE_INVALID");

    // 缺 A 签（只放 B 的槽）→ 400（A 必须先签）。
    let index_b = signer_index(&operation, device_b(&v2));
    let signature_b = sign_slot(&operation, index_b, key_b(&v2));
    let only_b = envelope_bytes(&operation, index_a, &[(index_b, signature_b)], &caps);
    let (status, body) = post_envelope(&app, &v2, Some(&token_a), &only_b).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{}", error_code(&body));
    assert_error(&body, "INVALID_REQUEST");

    // 满签包 → 409（应直接 submit，不该占收件箱）。
    let signature_a = sign_slot(&operation, index_a, key_a(&v2));
    let full = envelope_bytes(
        &operation,
        index_a,
        &[(index_a, signature_a), (index_b, signature_b)],
        &caps,
    );
    let (status, body) = post_envelope(&app, &v2, Some(&token_a), &full).await;
    assert_eq!(status, StatusCode::CONFLICT, "{}", error_code(&body));
    assert_error(&body, "INVALID_STATE");

    // 非 canonical（尾部多余字节）→ 400。
    let mut trailing = envelope.clone();
    trailing.push(0);
    let (status, body) = post_envelope(&app, &v2, Some(&token_a), &trailing).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{}", error_code(&body));

    // cap 证据与 relay 已验证控制链的 canonical cap 不一致（同 id 但字段被改）→ 400。
    let mut forged = caps.clone();
    forged[0].expires_at += 1;
    let forged_evidence = envelope_bytes(&operation, index_a, &[(index_a, signature_a)], &forged);
    let (status, body) = post_envelope(&app, &v2, Some(&token_a), &forged_evidence).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{}", error_code(&body));
    assert_error(&body, "INVALID_REQUEST");

    // 单签 kind（InviteCreate）→ 400（不走通用收件箱）。
    let invite_payload = proto::InviteCreatePayloadV2 {
        invite_id: [0x91; 16],
        token_hash: sha256(&[0x92u8; 32]),
        expires_at: v2.now_u64() + 600,
    }
    .encode()
    .expect("invite payload");
    let invite = v2.one_signer_operation(
        proto::ControlKindV2::InviteCreate,
        [0x74; 16],
        invite_payload,
        device_a(&v2),
        CAP_A,
        v2.now_u64(),
    );
    let invite_sig = sign_slot(&invite, 0, key_a(&v2));
    let invite_envelope = envelope_bytes(&invite, 0, &[(0, invite_sig)], &caps);
    let (status, body) = post_envelope(&app, &v2, Some(&token_a), &invite_envelope).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{}", error_code(&body));

    // 非 Device 槽未签（DeviceApprove 的 TargetPop）→ 400（PoP 必须沿 A 收集）。
    let target = v2.genesis_device();
    let signers = sort_signers(vec![
        proto::SignerV2 {
            role: proto::SignerRole::Device,
            device_id: Some(device_a(&v2)),
            cap_id: Some(CAP_A),
            pk: v2.sign_pk(&device_a(&v2)),
        },
        proto::SignerV2 {
            role: proto::SignerRole::Device,
            device_id: Some(device_b(&v2)),
            cap_id: Some(CAP_B),
            pk: v2.sign_pk(&device_b(&v2)),
        },
        proto::SignerV2 {
            role: proto::SignerRole::TargetPop,
            device_id: Some(target),
            cap_id: None,
            pk: v2.sign_pk(&target),
        },
    ]);
    let approve = v2.base_operation(
        proto::ControlKindV2::DeviceApprove,
        [0x75; 16],
        // 布局校验先于语义校验：用 canonical CDE（空 map）即可触发"非 Device 槽未签"。
        wire::encode(vec![]).expect("payload"),
        signers,
        v2.now_u64(),
    );
    let approve_index_a = signer_index(&approve, device_a(&v2));
    let approve_sig = sign_slot(&approve, approve_index_a, key_a(&v2));
    let approve_envelope = envelope_bytes(
        &approve,
        approve_index_a,
        &[(approve_index_a, approve_sig)],
        &caps,
    );
    let (status, body) = post_envelope(&app, &v2, Some(&token_a), &approve_envelope).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{}", error_code(&body));
    assert_error(&body, "INVALID_REQUEST");

    // 超限（>1MiB 控制面 body）→ 413。
    let oversized = vec![0u8; 1024 * 1024 + 64];
    let (status, body) = call(
        &app,
        post_raw(&approvals_uri(&v2, ""), Some(&token_a), oversized),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::PAYLOAD_TOO_LARGE,
        "{}",
        error_code(&body)
    );
    assert_error(&body, "REQUEST_TOO_LARGE");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_rejects_stale_expired_and_ineligible_caps() {
    let (v2, app, caps) = setup("s4d2-create-cas").await;

    // (a) head 前进：旧的 expected_control_rev/prev_hash 必须 CAS 失败。
    let (operation, envelope) = default_envelope(&v2, &caps, [0x76; 16], [0xE6; 16]);
    let signed = v2.sign_operation(&operation);
    let (status, body) = submit(&app, &v2.vault_id(), &signed).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "advance chain: {}",
        error_code(&body)
    );
    // 控制链前进 → 旧 realm3 token 失效，重新登录后再试（证明是 CAS 而非鉴权失败）。
    let token_a = login_a(&v2, &app).await;
    let (status, body) = post_envelope(&app, &v2, Some(&token_a), &envelope).await;
    assert_eq!(status, StatusCode::CONFLICT, "{}", error_code(&body));
    assert_error(&body, "CAS_CONFLICT");

    // (b) 过期：签名/截止时间在 300s 窗口之外（先用当前 head 构造，再让时间失效）。
    let past = v2.now_u64().saturating_sub(400);
    let stale_cap = proto::ManagementCapV2 {
        not_before: past,
        expires_at: past + 600,
        ..new_cap(
            &v2,
            [0xE7; 16],
            v2.genesis_device(),
            proto::CapScope::APPROVE,
            600,
        )
    };
    let stale = v2.two_signer_operation(
        proto::ControlKindV2::CapGrant,
        [0x77; 16],
        proto::CapGrantPayloadV2 { cap: stale_cap }
            .encode()
            .expect("payload"),
        &admin_signers(&v2),
        past,
    );
    let stale_a = signer_index(&stale, device_a(&v2));
    let stale_sig = sign_slot(&stale, stale_a, key_a(&v2));
    let stale_envelope = envelope_bytes(&stale, stale_a, &[(stale_a, stale_sig)], &caps);
    let (status, body) = post_envelope(&app, &v2, Some(&token_a), &stale_envelope).await;
    assert_eq!(status, StatusCode::GONE, "{}", error_code(&body));
    assert_error(&body, "EXPIRED");

    // (c) B 的槽位 cap 在 relay DB 不存在 → 403（cap 证据是客户端材料，不是授权）。
    let fake_b_cap = new_cap(
        &v2,
        [0xDD; 16],
        device_b(&v2),
        proto::CapScope::APPROVE,
        600,
    );
    let mut operation = grant_operation(
        &v2,
        [0x78; 16],
        new_cap(
            &v2,
            [0xE8; 16],
            v2.genesis_device(),
            proto::CapScope::APPROVE,
            600,
        ),
        &admin_signers(&v2),
    );
    {
        let index_b = signer_index(&operation, device_b(&v2));
        operation.signers[index_b].cap_id = Some([0xDD; 16]);
    }
    let index_a = signer_index(&operation, device_a(&v2));
    let signature = sign_slot(&operation, index_a, key_a(&v2));
    let mut evidence = caps.clone();
    // 证据集合 = role0 槽引用的 cap 集合（A 的 CAP_A + B 的假 0xDD）；不得夹带别的 cap。
    evidence.retain(|cap| cap.cap_id != CAP_B);
    evidence.push(fake_b_cap);
    let envelope = envelope_bytes(&operation, index_a, &[(index_a, signature)], &evidence);
    let (status, body) = post_envelope(&app, &v2, Some(&token_a), &envelope).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{}", error_code(&body));
    assert_error(&body, "FORBIDDEN");

    // (d) 同设备两 cap 凑数 → 403（两签必须两个不同 ACTIVE 设备）。
    let device = device_a(&v2);
    let mut operation = grant_operation(
        &v2,
        [0x79; 16],
        new_cap(
            &v2,
            [0xE9; 16],
            v2.genesis_device(),
            proto::CapScope::APPROVE,
            600,
        ),
        &admin_signers(&v2),
    );
    for signer in &mut operation.signers {
        if signer.device_id == Some(device_b(&v2)) {
            signer.device_id = Some(device);
            signer.pk = v2.sign_pk(&device);
        }
    }
    let index_a = signer_index(&operation, device);
    let signature = sign_slot(&operation, index_a, key_a(&v2));
    // 两槽的 cap 都是 CAP_A/CAP_B（都存在且有效），失败点是"同一设备"而不是 cap 不存在。
    let envelope = envelope_bytes(&operation, index_a, &[(index_a, signature)], &caps);
    let (status, body) = post_envelope(&app, &v2, Some(&token_a), &envelope).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{}", error_code(&body));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_is_idempotent_and_rejects_equivocation() {
    let (v2, app, caps) = setup("s4d2-idempotent").await;
    let token_a = login_a(&v2, &app).await;
    let request_id = [0x7A; 16];
    let (_operation, envelope) = default_envelope(&v2, &caps, request_id, [0xEA; 16]);

    let (status, body) = post_envelope(&app, &v2, Some(&token_a), &envelope).await;
    assert_eq!(status, StatusCode::CREATED, "{}", error_code(&body));
    let first = job_from_create(&body);
    let seq = wire::uint_field(&first, 12).expect("seq");

    // 同 (V,R,h) 重放 → 200 + created=false + 同一个 seq（不新建、不续期、不改状态）。
    let (status, body) = post_envelope(&app, &v2, Some(&token_a), &envelope).await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&body));
    assert!(!wire::bool_field(&cbor_fields(&body), 1).expect("created"));
    let replay = job_from_create(&body);
    assert_eq!(wire::uint_field(&replay, 12).expect("seq"), seq);

    // 同 R 不同 h（换一个新 cap payload）→ 409 ID_CONFLICT。
    let (_other, conflicting) = default_envelope(&v2, &caps, request_id, [0xEB; 16]);
    let (status, body) = post_envelope(&app, &v2, Some(&token_a), &conflicting).await;
    assert_eq!(status, StatusCode::CONFLICT, "{}", error_code(&body));
    assert_error(&body, "ID_CONFLICT");
}

// ---------------------------------------------------------------- 3. 可见性

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_and_read_are_limited_to_initiator_and_recipient() {
    let (v2, app, caps) = setup("s4d2-visibility").await;
    let token_a = login_a(&v2, &app).await;
    let request_id = [0x7B; 16];
    let (_operation, envelope) = default_envelope(&v2, &caps, request_id, [0xEC; 16]);
    let (status, body) = post_envelope(&app, &v2, Some(&token_a), &envelope).await;
    assert_eq!(status, StatusCode::CREATED, "{}", error_code(&body));

    // 给 genesis 设备（ACTIVE、非 A/B）发一张 APPROVE cap（正常控制链，A+B 两签）。
    let third_cap_id = [0xCE; 16];
    let third = v2.genesis_device();
    let cap = new_cap(&v2, third_cap_id, third, proto::CapScope::APPROVE, 3_600);
    let operation = grant_operation(&v2, [0x7C; 16], cap, &admin_signers(&v2));
    let signed = v2.sign_operation(&operation);
    let (status, body) = submit(&app, &v2.vault_id(), &signed).await;
    assert_eq!(status, StatusCode::OK, "grant third: {}", error_code(&body));
    let token_c = token_for(
        &v2,
        &app,
        third,
        &v2.fx.device_signing_key,
        Some(third_cap_id),
    )
    .await;
    // 授权变化后旧 token 全部失效 → 重新登录 A/B。
    let token_a = login_a(&v2, &app).await;
    let token_b = login_b(&v2, &app).await;

    // 非 A/B 的 ACTIVE 管理设备：读 → 404，列表为空（不泄露存在性）。
    let (status, body) = get_job(&app, &v2, &token_c, &request_id).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{}", error_code(&body));
    let (status, body) = list_jobs(&app, &v2, &token_c, "").await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&body));
    assert!(list_entries(&body).is_empty());
    // 非 A/B 的 ACTIVE 管理设备加签 → 404（没有属于它的冻结槽）。
    let fake_signature = [0x33u8; 64];
    let (status, body) = post_signature(&app, &v2, &token_c, &request_id, &fake_signature).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{}", error_code(&body));

    // 未知 request_id → 404；未知 query 键 → 400；limit 收敛到 50。
    let (status, _) = get_job(&app, &v2, &token_b, &[0x7Fu8; 16]).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, body) = list_jobs(&app, &v2, &token_b, "?bogus=1").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{}", error_code(&body));
    let (status, body) = list_jobs(&app, &v2, &token_b, "?limit=999").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        wire::uint_field(&cbor_fields(&body), 3).expect("page limit"),
        50
    );
    let (status, body) = list_jobs(&app, &v2, &token_b, "?role=initiator").await;
    assert_eq!(status, StatusCode::OK);
    assert!(list_entries(&body).is_empty(), "B 没有 outgoing");
    let (status, body) = list_jobs(&app, &v2, &token_a, "?role=recipient").await;
    assert_eq!(status, StatusCode::OK);
    assert!(list_entries(&body).is_empty(), "A 没有待办（它是发起者）");
}

// ---------------------------------------------------------------- 4. 加签负例

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn signature_is_bound_to_frozen_slot_and_reverified() {
    let (v2, app, caps) = setup("s4d2-sign-negative").await;
    let token_a = login_a(&v2, &app).await;
    let token_b = login_b(&v2, &app).await;
    let request_id = [0x7D; 16];
    let (operation, envelope) = default_envelope(&v2, &caps, request_id, [0xED; 16]);
    let (status, body) = post_envelope(&app, &v2, Some(&token_a), &envelope).await;
    assert_eq!(status, StatusCode::CREATED, "{}", error_code(&body));

    let index_b = signer_index(&operation, device_b(&v2));
    // 错钥（A 的私钥）签 B 的槽 → 400 SIGNATURE_INVALID。
    let wrong = sign_slot(&operation, index_b, key_a(&v2));
    let (status, body) = post_signature(&app, &v2, &token_b, &request_id, &wrong).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{}", error_code(&body));
    assert_error(&body, "SIGNATURE_INVALID");
    // 全零占位 → 400。
    let (status, body) = post_signature(&app, &v2, &token_b, &request_id, &[0u8; 64]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{}", error_code(&body));
    // A 用自己的 realm3 主体加签：只能命中 A 自己的冻结槽，且签名与已存逐字节相同
    // （Device 域只覆盖 op 摘要，与槽位无关）→ 幂等 200，不会写到 B 的槽。
    let (status, body) = post_signature(&app, &v2, &token_a, &request_id, &wrong).await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&body));
    assert_eq!(job_status(&job_from_response(&body)), "PENDING");

    // 正常加签两次：同字节幂等，异字节 409。
    let signature_b = sign_slot(&operation, index_b, key_b(&v2));
    let (status, body) = post_signature(&app, &v2, &token_b, &request_id, &signature_b).await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&body));
    assert_eq!(job_status(&job_from_response(&body)), "READY");
    let (status, body) = post_signature(&app, &v2, &token_b, &request_id, &signature_b).await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&body));
    // 另一个 64B 值（即便不是 B 的签名）在槽位已签后 → 409 ID_CONFLICT。
    let (status, body) = post_signature(&app, &v2, &token_b, &request_id, &[0x22u8; 64]).await;
    assert_eq!(status, StatusCode::CONFLICT, "{}", error_code(&body));
    assert_error(&body, "ID_CONFLICT");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revoke_or_chain_advance_makes_job_stale_and_unreadable() {
    let (v2, app, caps) = setup("s4d2-stale").await;
    let token_a = login_a(&v2, &app).await;
    let token_b = login_b(&v2, &app).await;
    let request_id = [0x7E; 16];
    let (_operation, envelope) = default_envelope(&v2, &caps, request_id, [0xEE; 16]);
    let (status, body) = post_envelope(&app, &v2, Some(&token_a), &envelope).await;
    assert_eq!(status, StatusCode::CREATED, "{}", error_code(&body));

    // 撤 B 的 cap（两签 CapRevoke；A+B 都持 CAP_ADMIN）。
    let revoke = v2.two_signer_operation(
        proto::ControlKindV2::CapRevoke,
        [0x80; 16],
        proto::CapRevokePayloadV2 { cap_id: CAP_B }
            .encode()
            .expect("payload"),
        &admin_signers(&v2),
        v2.now_u64(),
    );
    let signed = v2.sign_operation(&revoke);
    let (status, body) = submit(&app, &v2.vault_id(), &signed).await;
    assert_eq!(status, StatusCode::OK, "revoke: {}", error_code(&body));

    // B 的旧 token 每请求重验 cap → 403 REVOKED（读/加签都被切断）。
    let (status, body) = list_jobs(&app, &v2, &token_b, "").await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{}", error_code(&body));
    assert_error(&body, "REVOKED");
    let (status, body) = get_job(&app, &v2, &token_b, &request_id).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{}", error_code(&body));

    // A 仍可读（cap 未撤销）：授权 rev 前进 → job 惰性 STALE，正文清空。
    let token_a = login_a(&v2, &app).await;
    let (status, body) = get_job(&app, &v2, &token_a, &request_id).await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&body));
    let job = job_from_response(&body);
    assert_eq!(job_status(&job), "STALE");
    assert!(
        wire::opt_field(&cbor_fields(&body), 1).is_none(),
        "STALE 墓碑不再返回 op 正文"
    );
    // 终态不可取消 → 409 INVALID_STATE。
    let (status, body) = post_cancel(&app, &v2, &token_a, &request_id).await;
    assert_eq!(status, StatusCode::CONFLICT, "{}", error_code(&body));
    assert_error(&body, "INVALID_STATE");
}

// ---------------------------------------------------------------- 5. 到期/墓碑/取消

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expiry_tombstone_and_cancel_semantics() {
    let (v2, app, caps) = setup("s4d2-expiry").await;
    let token_a = login_a(&v2, &app).await;
    let token_b = login_b(&v2, &app).await;

    // (a) 到期：op.not_after = now+300 → 301s 后惰性 EXPIRED + 正文清空。
    let request_id = [0x81; 16];
    let (operation, envelope) = default_envelope(&v2, &caps, request_id, [0xF1; 16]);
    let (status, body) = post_envelope(&app, &v2, Some(&token_a), &envelope).await;
    assert_eq!(status, StatusCode::CREATED, "{}", error_code(&body));
    v2.fx.clock.advance_secs(301);
    // token TTL 600s，仍未过期；cap TTL 7 天。
    let (status, body) = list_jobs(&app, &v2, &token_b, "").await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&body));
    assert!(list_entries(&body).is_empty(), "到期后不在待办");
    let (status, body) = get_job(&app, &v2, &token_a, &request_id).await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&body));
    let job = job_from_response(&body);
    assert_eq!(job_status(&job), "EXPIRED");
    assert!(
        wire::opt_field(&cbor_fields(&body), 1).is_none(),
        "正文已清空"
    );
    let index_b = signer_index(&operation, device_b(&v2));
    let signature_b = sign_slot(&operation, index_b, key_b(&v2));
    let (status, body) = post_signature(&app, &v2, &token_b, &request_id, &signature_b).await;
    assert_eq!(status, StatusCode::GONE, "{}", error_code(&body));
    assert_error(&body, "EXPIRED");

    // 墓碑 24h 后删除（只留摘要 24h）：+24h 后由一次**成功**请求提交 sweep
    // （失败请求会整体回滚，只让下次成功请求再清一次——维护性变更不保证在错误路径提交）。
    v2.fx.clock.advance_secs(24 * 60 * 60);
    let token_a = login_a(&v2, &app).await;
    let token_b = login_b(&v2, &app).await;
    let (status, body) = list_jobs(&app, &v2, &token_a, "").await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&body));
    assert!(list_entries(&body).is_empty());
    let (status, body) = get_job(&app, &v2, &token_a, &request_id).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{}", error_code(&body));
    let rows: i64 = v2
        .fx
        .db
        .read(|conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM approval_jobs WHERE vault_id = ?1 AND request_id = ?2",
                (conn.scope().as_slice(), request_id.as_slice()),
                |row| row.get(0),
            )?)
        })
        .expect("count");
    assert_eq!(rows, 0, "24h 后墓碑行必须被删除");
    let signatures: i64 = v2
        .fx
        .db
        .read(|conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM approval_signatures WHERE vault_id = ?1 AND request_id = ?2",
                (conn.scope().as_slice(), request_id.as_slice()),
                |row| row.get(0),
            )?)
        })
        .expect("count signatures");
    assert_eq!(signatures, 0);

    // (b) 取消：只停止协调，已收集的满签包仍可 submit。
    let request_id = [0x82; 16];
    let (operation, envelope) = default_envelope(&v2, &caps, request_id, [0xF2; 16]);
    let (status, body) = post_envelope(&app, &v2, Some(&token_a), &envelope).await;
    assert_eq!(status, StatusCode::CREATED, "{}", error_code(&body));
    let index_b = signer_index(&operation, device_b(&v2));
    let signature_b = sign_slot(&operation, index_b, key_b(&v2));
    let (status, body) = post_signature(&app, &v2, &token_b, &request_id, &signature_b).await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&body));
    // A 取消 → CANCELLED；重复取消幂等。
    let (status, body) = post_cancel(&app, &v2, &token_a, &request_id).await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&body));
    assert_eq!(job_status(&job_from_response(&body)), "CANCELLED");
    let (status, body) = post_cancel(&app, &v2, &token_a, &request_id).await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&body));
    // B 再加签 → 409 INVALID_STATE。
    let (status, body) = post_signature(&app, &v2, &token_b, &request_id, &signature_b).await;
    assert_eq!(status, StatusCode::CONFLICT, "{}", error_code(&body));
    // 已外发的满签包仍可提交（取消不改控制链/不回撤签名）。
    let index_a = signer_index(&operation, device_a(&v2));
    let signature_a = sign_slot(&operation, index_a, key_a(&v2));
    let signed = proto::SignedControlV2 {
        operation: operation.clone(),
        signatures: vec![signature_a, signature_b],
    };
    let (status, body) = submit(&app, &v2.vault_id(), &signed).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "submit after cancel: {}",
        error_code(&body)
    );
    // 取消后的 job 记录保持 CANCELLED（"cancel" 只停止协调）。
    let token_a = login_a(&v2, &app).await;
    let (status, body) = get_job(&app, &v2, &token_a, &request_id).await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&body));
    assert_eq!(job_status(&job_from_response(&body)), "CANCELLED");
}

// ---------------------------------------------------------------- 6. 限额/分页

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn per_device_limits_and_pagination_are_bounded() {
    let (v2, app, caps) = setup("s4d2-limits").await;
    let token_a = login_a(&v2, &app).await;
    let token_b = login_b(&v2, &app).await;

    // 分页：先创建 3 个 job。
    let mut request_ids = Vec::new();
    for index in 0..3u8 {
        let request_id = [0x90 + index; 16];
        let cap_id = [0xA0 + index; 16];
        let (_operation, envelope) = default_envelope(&v2, &caps, request_id, cap_id);
        let (status, body) = post_envelope(&app, &v2, Some(&token_a), &envelope).await;
        assert_eq!(status, StatusCode::CREATED, "{}", error_code(&body));
        request_ids.push(request_id);
    }
    let (status, body) = list_jobs(&app, &v2, &token_b, "?limit=2").await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&body));
    assert_eq!(list_entries(&body).len(), 2);
    let next = wire::opt_uint_field(&cbor_fields(&body), 1)
        .expect("next")
        .expect("next_seq");
    let (status, body) =
        list_jobs(&app, &v2, &token_b, &format!("?limit=2&after_seq={next}")).await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&body));
    assert_eq!(list_entries(&body).len(), 1);
    assert!(wire::opt_uint_field(&cbor_fields(&body), 1)
        .expect("next")
        .is_none());

    // 同源设备 8 待办上限：现在 3 个，再建 5 个到 8，然后第 9 个 → 507。
    for index in 3..8u8 {
        let request_id = [0x90 + index; 16];
        let cap_id = [0xA0 + index; 16];
        let (_operation, envelope) = default_envelope(&v2, &caps, request_id, cap_id);
        let (status, body) = post_envelope(&app, &v2, Some(&token_a), &envelope).await;
        assert_eq!(status, StatusCode::CREATED, "{}", error_code(&body));
        request_ids.push(request_id);
    }
    let (operation, envelope) = default_envelope(&v2, &caps, [0x9F; 16], [0xAF; 16]);
    let (status, body) = post_envelope(&app, &v2, Some(&token_a), &envelope).await;
    assert_eq!(
        status,
        StatusCode::INSUFFICIENT_STORAGE,
        "{}",
        error_code(&body)
    );
    assert_error(&body, "QUOTA_EXCEEDED");
    // 取消一个 → 释放配额，新 job 可以创建（限额按"活 job"计）。
    let (status, _) = post_cancel(&app, &v2, &token_a, &request_ids[0]).await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = post_envelope(&app, &v2, Some(&token_a), &envelope).await;
    assert_eq!(status, StatusCode::CREATED, "{}", error_code(&body));
    // 新 job 只带 A 的一条签名（缺签不是零占位）。
    let signed_count = wire::array_field(&job_from_create(&body), 17)
        .expect("signed indices")
        .len();
    assert_eq!(signed_count, 1);
    let _ = operation;
}
