//! ADR 0015 **S4b1**：`UpgradePolicy` 控制提交、v1 关闭、`/authz/*` 正负例。
//!
//! 覆盖：
//! 1. legacy vault（真实 manager + ≥2 ACTIVE 设备）→ Upgrade 成功：authz_state/revs/caps/policy/
//!    recovery/upgrade_control_rev 正确；旧 v1 challenge/token/管理写被拒；旧 bytes/rev/pin 保留；
//! 2. 重复/错误签名/错误 manifest/门限不足/降 quorum/超 TTL/错 old_manager_rev/CAS/时间窗拒绝，
//!    且失败不推进控制链；
//! 3. 唯一路径/固定 body/幂等（同摘要回放、异摘要 409）；
//! 4. `/authz/challenges|tokens`：realm 3 cap 绑定、realm 1、错 cap/错签/重放/过期/旧 rev、
//!    token TTL = min(600, cap 剩余)；
//! 5. legacy vault 上 `/authz/*` 不可用、v1 行为不变。
//!
//! 全部走真实 relay 路由 + 真实 Ed25519；无 sleep（FakeClock 推进）。

mod support;

use axum::http::StatusCode;
use ed25519_dalek::Signer as _;
use relay::ids::{b64url_encode, hex_lower, sha256};
use relay::wire;
use support::v2::*;
use support::*;

use menghunote_sync_proto as proto;

fn new_fixture(label: &str) -> V2Fixture {
    tokio::task::block_in_place(|| V2Fixture::new(label))
}

fn assert_error(body: &[u8], expected: &str) {
    assert_eq!(
        error_code(body),
        expected,
        "错误码不符（body={:?}）",
        error_code(body)
    );
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

struct CapRowView {
    canonical: Vec<u8>,
    issued_control_rev: i64,
    issued_authz_rev: i64,
    issuer_authority_rev: i64,
    revoked_control_rev: Option<i64>,
}

fn cap_rows(v2: &V2Fixture) -> Vec<CapRowView> {
    v2.fx
        .db
        .read(|conn| {
            let mut stmt = conn.prepare(
                "SELECT canonical_bytes, issued_control_rev, issued_authz_rev,
                        issuer_authority_rev, revoked_control_rev
                 FROM management_caps WHERE vault_id = ?1 ORDER BY cap_id ASC",
            )?;
            let rows = stmt.query_map([conn.scope().as_slice()], |row| {
                Ok(CapRowView {
                    canonical: row.get(0)?,
                    issued_control_rev: row.get(1)?,
                    issued_authz_rev: row.get(2)?,
                    issuer_authority_rev: row.get(3)?,
                    revoked_control_rev: row.get(4)?,
                })
            })?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
        .expect("cap rows")
}

/// 手工构造一个原始 `OperationV2`（用于"未知 kind fail-stop"；不经过 typed 校验）。
fn raw_submit_body(
    v2: &V2Fixture,
    kind: u64,
    request_id: [u8; 16],
    head_rev: u64,
    prev_hash: [u8; 32],
    payload: Vec<u8>,
    signed_at: u64,
) -> Vec<u8> {
    let vault = v2.vault();
    let signer = proto::CborValue::Map(vec![
        (0, proto::CborValue::UInt(0)),
        (1, proto::CborValue::Bytes(v2.extra[0].0.to_vec())),
        (2, proto::CborValue::Bytes(Vec::new())),
        (
            3,
            proto::CborValue::Bytes(v2.sign_pk(&v2.extra[0].0).to_vec()),
        ),
    ]);
    let operation = proto::CborValue::Map(vec![
        (0, proto::CborValue::UInt(2)),
        (1, proto::CborValue::Bytes(vault.instance_id.to_vec())),
        (2, proto::CborValue::Bytes(vault.vault_id.to_vec())),
        (3, proto::CborValue::UInt(kind)),
        (4, proto::CborValue::Bytes(request_id.to_vec())),
        (5, proto::CborValue::UInt(head_rev)),
        (6, proto::CborValue::Bytes(prev_hash.to_vec())),
        (7, proto::CborValue::UInt(0)),
        (8, proto::CborValue::UInt(1)),
        (9, proto::CborValue::UInt(signed_at)),
        (10, proto::CborValue::UInt(signed_at + 300)),
        (11, proto::CborValue::UInt(2)),
        (
            12,
            proto::CborValue::Text(proto::canonical_submit_path(&vault.vault_id)),
        ),
        (13, proto::CborValue::Bytes(payload)),
        (14, proto::CborValue::Array(vec![signer])),
    ]);
    let signed = proto::CborValue::Map(vec![
        (
            0,
            proto::CborValue::Bytes(proto::encode_cde(&operation).expect("operation")),
        ),
        (
            1,
            proto::CborValue::Array(vec![proto::CborValue::Bytes(vec![0u8; 64])]),
        ),
    ]);
    proto::encode_cde(&proto::CborValue::Map(vec![(
        0,
        proto::CborValue::Bytes(proto::encode_cde(&signed).expect("signed")),
    )]))
    .expect("body")
}

// ---------------------------------------------------------------- 1. 成功升级 + v1 关闭

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upgrade_activates_v2_and_disables_v1_management_and_auth() {
    let v2 = new_fixture("s4b1-upgrade-ok");
    let app = app(&v2);
    let vault_before = v2.vault();
    let (head_rev, head_hash) = v2.head();
    assert_eq!(vault_before.authz_state, relay::vault::AuthzState::Legacy);
    assert_eq!(vault_before.authz_rev, 0);
    assert_eq!(vault_before.authority_rev, 0);
    assert!(vault_before.recovery_pub.is_none());

    // legacy 上 v1 challenge（realm 2）仍可用（v1 行为不变），旧 manager token 留作升级后探针。
    let manager_token = tokio::task::block_in_place(|| v2.fx.manager_token());
    let subject = relay::auth::Realm::management_subject(&v2.fx.manager_pub);
    let (status, body) = call(
        &app,
        post_cbor(
            &format!("{}/auth/challenges", hex_path(&v2.vault_id())),
            None,
            vec![(0, wire::uint(2)), (1, wire::bytes(subject.to_vec()))],
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "legacy v1 challenge: {}",
        error_code(&body)
    );

    // 升级提交。
    let request_id = [0x71u8; 16];
    let parts = v2.default_upgrade_parts();
    let operation = v2.upgrade_operation(request_id, &parts, v2.now_u64());
    let signed = v2.sign_operation(&operation);
    let signed_bytes = signed.encode().expect("signed bytes");
    let (status, body) = submit(&app, &v2.vault_id(), &signed).await;
    assert_eq!(status, StatusCode::OK, "upgrade: {}", error_code(&body));
    let receipt = receipt(&body);
    assert_eq!(receipt.control_rev, head_rev + 1);
    assert_eq!(receipt.kind, 0);
    assert_eq!(receipt.request_id, request_id);
    assert_eq!(receipt.authz_rev, 1);
    assert_eq!(receipt.authority_rev, 1);
    assert_eq!(
        receipt.head_hash,
        sha256(&[head_hash.to_vec(), signed_bytes.clone()].concat())
    );

    // vault 行：只切授权状态，旧 manager rev/bytes 保留。
    let vault = v2.vault();
    assert_eq!(vault.authz_state, relay::vault::AuthzState::Active);
    assert_eq!(vault.authz_version, 2);
    assert_eq!(vault.authz_rev, 1);
    assert_eq!(vault.authority_rev, 1);
    assert_eq!(vault.upgrade_control_rev, Some(head_rev + 1));
    assert_eq!(vault.manager_rev, vault_before.manager_rev);
    assert_eq!(
        vault.recovery_pub,
        Some(v2.recovery.verifying_key().to_bytes())
    );
    assert_eq!(
        vault.policy_bytes.as_deref(),
        Some(parts.policy.encode().expect("policy").as_slice())
    );
    assert_eq!(vault.control_rev, head_rev + 1);
    // 授权变化同事务推进 auth_rev（旧 token 立即失效）。
    assert_eq!(vault.auth_rev, vault_before.auth_rev + 1);
    // 旧管理公钥仍在（保留 bytes/rev/pin），但已不能管理。
    assert_eq!(
        v2.fx
            .db
            .read(relay::vault::manager_pubkey)
            .expect("manager pub"),
        Some(v2.fx.manager_pub)
    );

    // 控制链：prev 指向旧 head，record_hash 与回执一致，signed_record 是原字节。
    let (rev, record_hash, prev_hash, signed_record) = v2
        .fx
        .db
        .read(|conn| {
            Ok(conn.query_row(
                "SELECT rev, record_hash, prev_hash, signed_record FROM control_commits
                 WHERE vault_id = ?1 ORDER BY rev DESC LIMIT 1",
                [conn.scope().as_slice()],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                        row.get::<_, Vec<u8>>(3)?,
                    ))
                },
            )?)
        })
        .expect("head row");
    assert_eq!(rev, i64::try_from(head_rev + 1).expect("rev"));
    assert_eq!(record_hash, receipt.head_hash.to_vec());
    assert_eq!(prev_hash, head_hash.to_vec());
    assert_eq!(signed_record, signed_bytes);

    // bootstrap caps 原子安装：rev 绑定提交后值、未撤销、canonical bytes 原样。
    let caps = cap_rows(&v2);
    assert_eq!(caps.len(), 2);
    for (index, row) in caps.iter().enumerate() {
        let expected = parts.caps[index].encode().expect("cap encode");
        assert_eq!(row.canonical, expected);
        assert_eq!(
            row.issued_control_rev,
            i64::try_from(head_rev + 1).expect("rev")
        );
        assert_eq!(row.issued_authz_rev, 1);
        assert_eq!(row.issuer_authority_rev, 1);
        assert!(row.revoked_control_rev.is_none());
    }

    // v1 鉴权全部关闭（realm 1 与 realm 2 都一样）。
    for realm in [1u64, 2] {
        let (status, body) = call(
            &app,
            post_cbor(
                &format!("{}/auth/challenges", hex_path(&v2.vault_id())),
                None,
                vec![(0, wire::uint(realm)), (1, wire::bytes(subject.to_vec()))],
            ),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "v1 challenge realm {realm} 必须被关闭"
        );
        assert_error(&body, "MANAGEMENT_REQUIRED");
    }

    // 升级前签出的旧 manager token 在两个路径上都失效（旧语义 /v2 旁路关闭）。
    for prefix in ["/v1/vaults", "/v2/vaults"] {
        let uri = format!("{prefix}/{}/invites", hex_lower(&v2.vault_id()));
        let (status, body) = call(&app, post_cbor(&uri, Some(&manager_token), vec![])).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{prefix} 旧管理写必须被拒");
        assert_error(&body, "REVOKED");
    }

    // 即使绕过 token（直接调领域层），v1 管理签名也被 authz_state 门禁挡住。
    let path = "/v2/vaults/00/invites";
    let body = wire::encode(vec![(0, wire::uint(1))]).expect("body");
    let signature = v2.fx.sign_management(
        proto::HttpMethod::Post,
        path,
        [0x31; 16],
        vault.control_rev,
        &body,
    );
    let error = tokio::task::block_in_place(|| {
        v2.fx.db.write_blocking(move |conn| {
            relay::auth::require_management(
                conn,
                &relay::verify::Ed25519Hook,
                &relay::auth::ManagementProof {
                    method_tag: 2,
                    path,
                    request_id: [0x31; 16],
                    expected_control_rev: vault.control_rev,
                    signature: &signature,
                },
                &body,
            )
        })
    })
    .expect_err("legacy 管理写必须被拒");
    assert_eq!(error.code(), Some(relay::ErrorCode::ManagementRequired));

    // /authz：realm 3（bootstrap cap）+ realm 1 都能拿到可用 token。
    let token_mgmt = authz_login(
        &app,
        &v2.vault_id(),
        v2.extra[0].0,
        &v2.extra[0].1,
        Some([0xC1; 16]),
    )
    .await
    .expect("realm 3 token");
    let (status, _) = call(
        &app,
        get(
            &format!("{}/state", hex_path(&v2.vault_id())),
            Some(&token_mgmt),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "realm 3 bearer 可读数据面");
    let token_device = authz_login(
        &app,
        &v2.vault_id(),
        v2.genesis_device(),
        v2.genesis_key(),
        None,
    )
    .await
    .expect("realm 1 token");
    let (status, _) = call(
        &app,
        get(
            &format!("{}/control", hex_path(&v2.vault_id())),
            Some(&token_device),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "realm 1 bearer 可读控制链");
}

// ---------------------------------------------------------------- 2. 升级负例

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upgrade_rejects_bad_quorum_manifest_caps_time_and_cas_without_state_change() {
    let v2 = new_fixture("s4b1-upgrade-negative");
    let app = app(&v2);
    let (head_rev, head_hash) = v2.head();
    let baseline_count = control_count(&v2);
    let now = v2.now_u64();
    let request_id = [0x80u8; 16];

    // 每个负例：返回预期错误码，且控制链/vault 状态不变。
    let mut cases: Vec<(String, proto::OperationV2, u16, &str)> = Vec::new();

    // 只确认一台设备（门限不足）。
    {
        let parts = v2.default_upgrade_parts();
        let mut operation = v2.upgrade_operation(request_id, &parts, now);
        operation.signers = sort_signers(vec![
            device_signer(&v2, v2.extra[0].0),
            proto::SignerV2 {
                role: proto::SignerRole::CreatorUpgrade,
                device_id: None,
                cap_id: None,
                pk: v2.manager_key().verifying_key().to_bytes(),
            },
            proto::SignerV2 {
                role: proto::SignerRole::OfflineRecovery,
                device_id: None,
                cap_id: None,
                pk: parts.recovery_pk,
            },
        ]);
        cases.push(("单设备确认".to_owned(), operation, 403, "FORBIDDEN"));
    }

    // 同一设备两个不同公钥确认（不能凑门限）。
    {
        let parts = v2.default_upgrade_parts();
        let mut operation = v2.upgrade_operation(request_id, &parts, now);
        operation.signers = sort_signers(vec![
            device_signer(&v2, v2.extra[0].0),
            proto::SignerV2 {
                role: proto::SignerRole::Device,
                device_id: Some(v2.extra[0].0),
                cap_id: None,
                pk: [0x99; 32],
            },
            proto::SignerV2 {
                role: proto::SignerRole::CreatorUpgrade,
                device_id: None,
                cap_id: None,
                pk: v2.manager_key().verifying_key().to_bytes(),
            },
            proto::SignerV2 {
                role: proto::SignerRole::OfflineRecovery,
                device_id: None,
                cap_id: None,
                pk: parts.recovery_pk,
            },
        ]);
        cases.push(("同设备两公钥".to_owned(), operation, 403, "FORBIDDEN"));
    }

    // manifest 少一台 ACTIVE 设备。
    {
        let mut parts = v2.default_upgrade_parts();
        parts.manifest.pop();
        let operation = v2.upgrade_operation(request_id, &parts, now);
        cases.push(("manifest 缺设备".to_owned(), operation, 403, "FORBIDDEN"));
    }

    // manifest 公钥与 DB 不一致。
    {
        let mut parts = v2.default_upgrade_parts();
        let vault = v2.vault();
        parts.manifest[0] = proto::DeviceCertV2 {
            version: 2,
            instance_id: vault.instance_id,
            vault_id: vault.vault_id,
            device_id: v2.genesis_device(),
            sign_ed25519_pub: [0x99; 32],
            hpke_x25519_pub: [0x98; 32],
            cert_serial: 1,
            data_rights: proto::Rights::new(proto::Rights::ALL).expect("rights"),
            issued_authz_rev: 1,
        }
        .encode()
        .expect("cert");
        let operation = v2.upgrade_operation(request_id, &parts, now);
        cases.push(("manifest 公钥不符".to_owned(), operation, 403, "FORBIDDEN"));
    }

    // old_manager_rev 不是当前 v1 管理 rev。
    {
        let mut parts = v2.default_upgrade_parts();
        parts.old_manager_rev += 1;
        let operation = v2.upgrade_operation(request_id, &parts, now);
        cases.push(("old_manager_rev".to_owned(), operation, 403, "FORBIDDEN"));
    }

    // 只给一台设备 CAP_ADMIN（拒绝把 quorum 降为 1）。
    {
        let mut parts = v2.default_upgrade_parts();
        parts.caps[1] = v2.bootstrap_cap([0xC2; 16], v2.extra[1].0, proto::CapScope::INVITE, 600);
        let operation = v2.upgrade_operation(request_id, &parts, now);
        cases.push(("CAP_ADMIN quorum<2".to_owned(), operation, 403, "FORBIDDEN"));
    }

    // cap TTL 超 policy.max_cap_ttl。
    {
        let mut parts = v2.default_upgrade_parts();
        parts.caps[0] = v2.bootstrap_cap(
            [0xC1; 16],
            v2.extra[0].0,
            proto::CapScope::ALL,
            proto::MAX_CAP_TTL_SECS_V2 + 1,
        );
        let operation = v2.upgrade_operation(request_id, &parts, now);
        cases.push(("cap TTL 超限".to_owned(), operation, 403, "FORBIDDEN"));
    }

    // 升级记录夹带 role3（TargetPop 属于 DeviceApprove）。
    {
        let parts = v2.default_upgrade_parts();
        let mut operation = v2.upgrade_operation(request_id, &parts, now);
        operation.signers = sort_signers(
            operation
                .signers
                .into_iter()
                .chain([proto::SignerV2 {
                    role: proto::SignerRole::TargetPop,
                    device_id: Some(v2.genesis_device()),
                    cap_id: None,
                    pk: v2.sign_pk(&v2.genesis_device()),
                }])
                .collect(),
        );
        cases.push(("夹带 role3".to_owned(), operation, 403, "FORBIDDEN"));
    }

    // role0 带 cap（升级前没有 cap）。
    {
        let parts = v2.default_upgrade_parts();
        let mut operation = v2.upgrade_operation(request_id, &parts, now);
        operation.signers = sort_signers(
            operation
                .signers
                .into_iter()
                .map(|signer| {
                    if signer.role == proto::SignerRole::Device {
                        proto::SignerV2 {
                            cap_id: Some([0x77; 16]),
                            ..signer
                        }
                    } else {
                        signer
                    }
                })
                .collect(),
        );
        cases.push(("role0 带 cap".to_owned(), operation, 403, "FORBIDDEN"));
    }

    // CAS/head/rev/authority 不一致。
    for (label, mutate) in [
        (
            "expected_control_rev 旧值",
            (|operation: &mut proto::OperationV2| operation.expected_control_rev += 1)
                as fn(&mut proto::OperationV2),
        ),
        (
            "prev_control_hash 错",
            (|operation: &mut proto::OperationV2| operation.prev_control_hash = [0u8; 32])
                as fn(&mut proto::OperationV2),
        ),
        (
            "expected_authz_rev 非 0",
            (|operation: &mut proto::OperationV2| operation.expected_authz_rev = 1)
                as fn(&mut proto::OperationV2),
        ),
        (
            "authority_rev 非 1",
            (|operation: &mut proto::OperationV2| operation.authority_rev = 2)
                as fn(&mut proto::OperationV2),
        ),
    ] {
        let parts = v2.default_upgrade_parts();
        let mut operation = v2.upgrade_operation(request_id, &parts, now);
        mutate(&mut operation);
        cases.push((label.to_owned(), operation, 409, "CAS_CONFLICT"));
    }

    for (label, signed_at, expected_status, expected_code) in [
        ("签名过旧", now - 301, 410, "EXPIRED"),
        ("签名在未来", now + 31, 400, "INVALID_REQUEST"),
        ("not_after 超窗", now, 400, "INVALID_REQUEST"),
    ] {
        let parts = v2.default_upgrade_parts();
        let mut operation = v2.upgrade_operation(request_id, &parts, signed_at);
        if label == "not_after 超窗" {
            operation.not_after = signed_at + 301;
        }
        cases.push((label.to_owned(), operation, expected_status, expected_code));
    }

    for (label, operation, status, code) in cases {
        let signed = v2.sign_operation(&operation);
        let (actual_status, body) = submit(&app, &v2.vault_id(), &signed).await;
        assert_eq!(
            actual_status,
            StatusCode::from_u16(status).expect("status"),
            "{label}: {}",
            error_code(&body)
        );
        assert_error(&body, code);
        assert_eq!(control_count(&v2), baseline_count, "{label} 不得推进控制链");
        assert_eq!(v2.vault().authz_state, relay::vault::AuthzState::Legacy);
        assert_eq!(v2.head(), (head_rev, head_hash), "{label} 不得移动 head");
    }

    // 错误签名（role0 用另一台设备的私钥签）→ SIGNATURE_INVALID。
    {
        let parts = v2.default_upgrade_parts();
        let operation = v2.upgrade_operation(request_id, &parts, now);
        let keys = vec![
            v2.key(&v2.extra[1].0), // role0[0] 应为 extra0
            v2.key(&v2.extra[1].0),
            v2.manager_key(),
            &v2.recovery,
        ];
        let signed = v2.sign_operation_with(&operation, &keys);
        let (status, body) = submit(&app, &v2.vault_id(), &signed).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_error(&body, "SIGNATURE_INVALID");
        assert_eq!(control_count(&v2), baseline_count);
    }

    // 合法提交最终成功（前面的拒绝没有留下副作用）。
    let parts = v2.default_upgrade_parts();
    let operation = v2.upgrade_operation(request_id, &parts, now);
    let signed = v2.sign_operation(&operation);
    let (status, body) = submit(&app, &v2.vault_id(), &signed).await;
    assert_eq!(status, StatusCode::OK, "upgrade: {}", error_code(&body));
    assert_eq!(v2.vault().authz_state, relay::vault::AuthzState::Active);
    assert_eq!(control_count(&v2), baseline_count + 1);
}

// ---------------------------------------------------------------- 3. 路径/body/幂等

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn control_submit_path_body_and_idempotency_are_fixed() {
    let v2 = new_fixture("s4b1-submit-idempotency");
    let app = app(&v2);
    let request_id = [0x72u8; 16];
    let parts = v2.default_upgrade_parts();
    let operation = v2.upgrade_operation(request_id, &parts, v2.now_u64());
    let signed = v2.sign_operation(&operation);
    let signed_bytes = signed.encode().expect("signed bytes");

    // 非唯一路径别名一律 404（base64 / 大写 hex / query）。
    for uri in [
        format!(
            "/v2/vaults/{}/control/submit",
            b64url_encode(&v2.vault_id())
        ),
        format!(
            "/v2/vaults/{}A{}/control/submit",
            &hex_lower(&v2.vault_id())[..1],
            &hex_lower(&v2.vault_id())[2..]
        ),
        format!(
            "/v2/vaults/{}/control/submit?after_rev=0",
            hex_lower(&v2.vault_id())
        ),
        format!("/v2/vaults/{}/control/Submit", hex_lower(&v2.vault_id())),
    ] {
        let (status, _) = call(
            &app,
            post_cbor(&uri, None, vec![(0, wire::bytes(signed_bytes.clone()))]),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
    }

    // 固定 body：附加字段/非 bytes 一律 400。
    let (status, body) = call(
        &app,
        post_cbor(
            &format!("{}/control/submit", hex_path(&v2.vault_id())),
            None,
            vec![(0, wire::bytes(signed_bytes.clone())), (1, wire::uint(1))],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{}", error_code(&body));
    let (status, body) = call(
        &app,
        post_cbor(
            &format!("{}/control/submit", hex_path(&v2.vault_id())),
            None,
            vec![(0, wire::uint(7))],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{}", error_code(&body));

    // 首次提交成功。
    let (status, first) = submit(&app, &v2.vault_id(), &signed).await;
    assert_eq!(status, StatusCode::OK, "upgrade: {}", error_code(&first));
    let receipt_view = receipt(&first);
    assert_eq!(receipt_view.request_id, request_id);

    // 同 request_id + 同字节 → 原样回放（字节一致）。
    let (status, replay) = submit(&app, &v2.vault_id(), &signed).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(replay, first, "幂等重放必须字节一致");

    // 同 request_id + 不同摘要 → 409 ID_CONFLICT。
    let parts = v2.default_upgrade_parts();
    let mut other = v2.upgrade_operation(request_id, &parts, v2.now_u64() + 1);
    other.not_after = v2.now_u64() + 302;
    let other_signed = v2.sign_operation(&other);
    let (status, body) = submit(&app, &v2.vault_id(), &other_signed).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_error(&body, "ID_CONFLICT");

    // 另一个 vault 的操作提交到本路由 → 403（租户/实例不匹配）。
    let mut foreign = v2.upgrade_operation([0x73; 16], &parts, v2.now_u64());
    foreign.vault_id = [0xEE; 16];
    foreign.prev_control_hash = [0u8; 32];
    foreign.canonical_path = proto::canonical_submit_path(&foreign.vault_id);
    let foreign_signed = v2.sign_operation(&foreign);
    let (status, body) = submit(&app, &v2.vault_id(), &foreign_signed).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{}", error_code(&body));

    // 未知 kind（15）在严格解码处 fail-stop；已知但未实现的 kind（1 PolicyReplace）501。
    let (head_rev, head_hash) = v2.head();
    let payload = proto::encode_cde(&proto::CborValue::Map(vec![(
        0,
        proto::CborValue::Bytes(vec![1u8; 16]),
    )]))
    .expect("payload");
    let body = raw_submit_body(
        &v2,
        15,
        [0x74; 16],
        head_rev,
        head_hash,
        payload.clone(),
        v2.now_u64(),
    );
    let (status, response) = submit_raw(&app, &v2.vault_id(), body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{}", error_code(&response));
    assert_eq!(control_count(&v2), i64::try_from(head_rev).expect("rev"));
    let body = raw_submit_body(
        &v2,
        1,
        [0x75; 16],
        head_rev,
        head_hash,
        payload,
        v2.now_u64(),
    );
    let (status, response) = submit_raw(&app, &v2.vault_id(), body).await;
    assert_eq!(
        status,
        StatusCode::NOT_IMPLEMENTED,
        "{}",
        error_code(&response)
    );
    assert_error(&response, "NOT_IMPLEMENTED");
    assert_eq!(control_count(&v2), i64::try_from(head_rev).expect("rev"));
}

// ---------------------------------------------------------------- 4. /authz 正负例

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn authz_challenges_and_tokens_enforce_cap_binding_and_single_use() {
    let v2 = new_fixture("s4b1-authz");
    let app = app(&v2);
    let (status, body) = upgrade_default(&v2, &app, [0x91; 16]).await;
    assert_eq!(status, StatusCode::OK, "upgrade: {}", error_code(&body));

    // 第三台设备拿一个短 TTL 的 INVITE cap（token TTL = min(600, cap 剩余)）。
    let short_cap_id = [0xC3; 16];
    let device = v2.genesis_device();
    let new_cap = {
        let vault = v2.vault();
        proto::ManagementCapV2 {
            version: 2,
            instance_id: vault.instance_id,
            vault_id: vault.vault_id,
            cap_id: short_cap_id,
            subject_device_id: device,
            subject_ed_pk: v2.sign_pk(&device),
            scopes: proto::CapScope::new(proto::CapScope::INVITE).expect("scopes"),
            not_before: v2.now_u64(),
            expires_at: v2.now_u64() + 120,
            issued_control_rev: v2.head().0 + 1,
            issued_authz_rev: vault.authz_rev + 1,
            issuer_authority_rev: vault.authority_rev,
            delegable: false,
        }
    };
    let operation = v2.two_signer_operation(
        proto::ControlKindV2::CapGrant,
        [0x92; 16],
        proto::CapGrantPayloadV2 {
            cap: new_cap.clone(),
        }
        .encode()
        .expect("payload"),
        &[(v2.extra[0].0, [0xC1; 16]), (v2.extra[1].0, [0xC2; 16])],
        v2.now_u64(),
    );
    let signed = v2.sign_operation(&operation);
    let (status, body) = submit(&app, &v2.vault_id(), &signed).await;
    assert_eq!(status, StatusCode::OK, "grant: {}", error_code(&body));

    // 正例：realm 3 token TTL = min(600, 120)。
    let _token = authz_login(
        &app,
        &v2.vault_id(),
        device,
        v2.genesis_key(),
        Some(short_cap_id),
    )
    .await
    .expect("short cap token");
    let (status, body) = call(
        &app,
        post_cbor(
            &format!("{}/authz/challenges", hex_path(&v2.vault_id())),
            None,
            vec![
                (0, wire::uint(3)),
                (1, wire::bytes(device.to_vec())),
                (2, wire::bytes(short_cap_id.to_vec())),
            ],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let challenge_bytes = wire::bytes_field(&cbor_fields(&body), 0).expect("challenge");
    let signature = v2
        .genesis_key()
        .sign(&proto::auth_signing_input_v2(&challenge_bytes))
        .to_bytes()
        .to_vec();
    let (status, body) = call(
        &app,
        post_cbor(
            &format!("{}/authz/tokens", hex_path(&v2.vault_id())),
            None,
            vec![
                (0, wire::bytes(challenge_bytes.clone())),
                (1, wire::bytes(signature.clone())),
            ],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let issued = cbor_fields(&body);
    assert_eq!(
        wire::uint_field(&issued, 1).expect("expires"),
        v2.now_u64() + 120,
        "管理 token 不得超过 cap 剩余寿命"
    );
    assert_eq!(wire::id16_field(&issued, 7).expect("cap"), short_cap_id);

    // 重放同一 challenge → CHALLENGE_USED（单用）。
    let (status, body) = call(
        &app,
        post_cbor(
            &format!("{}/authz/tokens", hex_path(&v2.vault_id())),
            None,
            vec![
                (0, wire::bytes(challenge_bytes)),
                (1, wire::bytes(signature)),
            ],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_error(&body, "CHALLENGE_USED");

    // cap 到期只按 deadline 失效：短 TTL cap 的 token 活不过 cap（认证先过期 → 401）。
    v2.fx.clock.advance_secs(121);
    let (status, body) = call(
        &app,
        get(
            &format!("{}/state", hex_path(&v2.vault_id())),
            Some(&_token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{}", error_code(&body));
    v2.fx.clock.set(BASE_TIME);

    // 错 cap（用别人的 cap 给本设备申请）→ 403。
    let (status, body) = call(
        &app,
        post_cbor(
            &format!("{}/authz/challenges", hex_path(&v2.vault_id())),
            None,
            vec![
                (0, wire::uint(3)),
                (1, wire::bytes(device.to_vec())),
                (2, wire::bytes([0xC2; 16].to_vec())),
            ],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{}", error_code(&body));

    // 未知 cap → 403（不泄露 cap 是否存在）。
    let (status, body) = call(
        &app,
        post_cbor(
            &format!("{}/authz/challenges", hex_path(&v2.vault_id())),
            None,
            vec![
                (0, wire::uint(3)),
                (1, wire::bytes(device.to_vec())),
                (2, wire::bytes([0xFE; 16].to_vec())),
            ],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{}", error_code(&body));

    // realm 3 不带 cap / realm 1 带 cap → 400。
    for fields in [
        vec![(0, wire::uint(3)), (1, wire::bytes(device.to_vec()))],
        vec![
            (0, wire::uint(1)),
            (1, wire::bytes(device.to_vec())),
            (2, wire::bytes(short_cap_id.to_vec())),
        ],
    ] {
        let (status, body) = call(
            &app,
            post_cbor(
                &format!("{}/authz/challenges", hex_path(&v2.vault_id())),
                None,
                fields,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{}", error_code(&body));
    }

    // 错签名 → CHALLENGE_INVALID。
    let (status, body) = call(
        &app,
        post_cbor(
            &format!("{}/authz/challenges", hex_path(&v2.vault_id())),
            None,
            vec![(0, wire::uint(1)), (1, wire::bytes(device.to_vec()))],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let challenge_bytes = wire::bytes_field(&cbor_fields(&body), 0).expect("challenge");
    let (status, body) = call(
        &app,
        post_cbor(
            &format!("{}/authz/tokens", hex_path(&v2.vault_id())),
            None,
            vec![
                (0, wire::bytes(challenge_bytes.clone())),
                (1, wire::bytes(vec![0u8; 64])),
            ],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_error(&body, "CHALLENGE_INVALID");

    // challenge 60s 过期 → CHALLENGE_INVALID。
    v2.fx.clock.advance_secs(61);
    let signature = v2
        .genesis_key()
        .sign(&proto::auth_signing_input_v2(&challenge_bytes))
        .to_bytes()
        .to_vec();
    let (status, body) = call(
        &app,
        post_cbor(
            &format!("{}/authz/tokens", hex_path(&v2.vault_id())),
            None,
            vec![
                (0, wire::bytes(challenge_bytes)),
                (1, wire::bytes(signature)),
            ],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_error(&body, "CHALLENGE_INVALID");

    // 授权 rev 前进后，未兑现 challenge 立即失效（challenge 行投影的 revs 不再等于当前）。
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
    let stale_challenge = wire::bytes_field(&cbor_fields(&body), 0).expect("challenge");
    let stale_signature = v2
        .genesis_key()
        .sign(&proto::auth_signing_input_v2(&stale_challenge))
        .to_bytes()
        .to_vec();
    {
        // 授予另一张 cap → authz/auth rev 前进，旧 challenge 失效。
        let vault = v2.vault();
        let (head_rev, _) = v2.head();
        let next_cap = proto::ManagementCapV2 {
            version: 2,
            instance_id: vault.instance_id,
            vault_id: vault.vault_id,
            cap_id: [0xC4; 16],
            subject_device_id: v2.extra[0].0,
            subject_ed_pk: v2.sign_pk(&v2.extra[0].0),
            scopes: proto::CapScope::new(proto::CapScope::INVITE).expect("scopes"),
            not_before: v2.now_u64(),
            expires_at: v2.now_u64() + 600,
            issued_control_rev: head_rev + 1,
            issued_authz_rev: vault.authz_rev + 1,
            issuer_authority_rev: vault.authority_rev,
            delegable: false,
        };
        let operation = v2.two_signer_operation(
            proto::ControlKindV2::CapGrant,
            [0x93; 16],
            proto::CapGrantPayloadV2 { cap: next_cap }
                .encode()
                .expect("payload"),
            &[(v2.extra[0].0, [0xC1; 16]), (v2.extra[1].0, [0xC2; 16])],
            v2.now_u64(),
        );
        let signed = v2.sign_operation(&operation);
        let (status, body) = submit(&app, &v2.vault_id(), &signed).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "second grant: {}",
            error_code(&body)
        );
    }
    let (status, body) = call(
        &app,
        post_cbor(
            &format!("{}/authz/tokens", hex_path(&v2.vault_id())),
            None,
            vec![
                (0, wire::bytes(stale_challenge)),
                (1, wire::bytes(stale_signature)),
            ],
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "旧 rev challenge: {}",
        error_code(&body)
    );
    assert_error(&body, "CHALLENGE_INVALID");

    // token 过期（600s）后 `/state` → 401。
    v2.fx.clock.set(BASE_TIME);
    let token = authz_login(&app, &v2.vault_id(), device, v2.genesis_key(), None)
        .await
        .expect("device token");
    let (status, _) = call(
        &app,
        get(&format!("{}/state", hex_path(&v2.vault_id())), Some(&token)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    v2.fx.clock.set(v2.fx.clock_now() + 601);
    let (status, body) = call(
        &app,
        get(&format!("{}/state", hex_path(&v2.vault_id())), Some(&token)),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{}", error_code(&body));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn authz_endpoints_are_unavailable_on_legacy_vault_and_v1_stays_green() {
    let v2 = new_fixture("s4b1-legacy");
    let app = app(&v2);
    // `/authz/*` 在未启用 V2 的 vault 上 fail-stop（409），不会自动升级。
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
    assert_eq!(status, StatusCode::CONFLICT);
    assert_error(&body, "INVALID_STATE");

    // v1 行为不变：device realm challenge/token 正常。
    let token = tokio::task::block_in_place(|| v2.fx.device_token());
    let (status, _) = call(
        &app,
        get(&format!("{}/state", hex_path(&v2.vault_id())), Some(&token)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}
