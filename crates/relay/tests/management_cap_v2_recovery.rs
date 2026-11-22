//! ADR 0015 **S4c**：`RecoveryReset`（13）/`RecoveryKeyChange`（14）与 realm 4。
//!
//! 覆盖：
//! 1. Reset 正例：离线恢复根（`recover` 域）+ 新钥 PoP → 换 authority 代、原子装新 policy/
//!    新代 caps；旧代 cap 与旧恢复 bearer 立即失效，新代 cap/恢复 bearer 可用；
//! 2. Reset 负例：单签、错 PoP、新旧钥相同、旧 creator/manager 旁路、复活 revoked、
//!    降门限、legacy vault、跨 vault 全部拒绝且不推进控制链；
//! 3. KeyChange 正例/负例：两台 POLICY + 新钥 PoP；只换恢复钥（不改 policy/成员），
//!    `authority_rev+1` 让旧代 cap 失效并原子装新代 caps；单签/旧钥 PoP 拒绝；
//! 4. `recovery_only`：不足两台 ACTIVE 时进入；普通管理写 403、设备 realm 仍可读；
//!    再 Reset（带设备 add）回到 active；
//! 5. realm 4：subject = `SHA256(recovery_pub)[..16]`、TTL 60s、只能读恢复证据；
//!    每请求 rev 相等/authority；设备 bearer 不能读恢复证据。
//!
//! 全部真实 relay 路由 + 真实 Ed25519；无 sleep（FakeClock + 显式收敛点）。

mod support;

use axum::http::StatusCode;
use axum::Router;
use ed25519_dalek::SigningKey;
use relay::ids::Id16;
use relay::wire;
use support::v2::*;
use support::*;

use menghunote_sync_proto as proto;

// ---------------------------------------------------------------- 夹具

fn new_fixture(label: &str) -> V2Fixture {
    tokio::task::block_in_place(|| V2Fixture::new(label))
}

fn blocking<T>(f: impl FnOnce() -> T) -> T {
    tokio::task::block_in_place(f)
}

fn assert_error(body: &[u8], expected: &str) {
    assert_eq!(
        error_code(body),
        expected,
        "错误码不符（实际 {}）",
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

async fn upgraded(label: &str) -> (V2Fixture, Router) {
    let v2 = new_fixture(label);
    let app = app(&v2);
    let (status, body) = upgrade_default(&v2, &app, [0xA0; 16]).await;
    assert_eq!(status, StatusCode::OK, "upgrade: {}", error_code(&body));
    (v2, app)
}

/// 控制链一行：`(rev, prev_hash, record_hash, signed_record)`。
type ChainRow = (i64, Vec<u8>, Vec<u8>, Vec<u8>);

fn assert_chain_continuous(v2: &V2Fixture) {
    let rows: Vec<ChainRow> = v2
        .fx
        .db
        .read(|conn| {
            let mut stmt = conn.prepare(
                "SELECT rev, prev_hash, record_hash, signed_record FROM control_commits
                 WHERE vault_id = ?1 ORDER BY rev ASC",
            )?;
            let rows = stmt.query_map([conn.scope().as_slice()], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                ))
            })?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
        .expect("control rows");
    let mut previous = [0u8; 32];
    for (index, (rev, prev_hash, record_hash, signed_record)) in rows.iter().enumerate() {
        assert_eq!(
            *rev,
            i64::try_from(index + 1).expect("rev"),
            "控制链 rev 必须从 1 连续"
        );
        assert_eq!(prev_hash.as_slice(), previous.as_slice());
        let expected = relay::ids::sha256_parts(&[previous.as_slice(), signed_record.as_slice()]);
        assert_eq!(record_hash.as_slice(), expected.as_slice());
        previous = expected;
    }
}

fn device_status(v2: &V2Fixture, device_id: &Id16) -> Option<String> {
    v2.fx
        .db
        .read(move |conn| {
            Ok(conn
                .query_row(
                    "SELECT status FROM devices WHERE vault_id = ?1 AND device_id = ?2",
                    (conn.scope().as_slice(), device_id.as_slice()),
                    |row| row.get::<_, String>(0),
                )
                .ok())
        })
        .expect("device status")
}

fn credentials(v2: &V2Fixture, device_id: &Id16) -> Vec<(i64, Vec<u8>)> {
    v2.fx
        .db
        .read(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT serial, certificate FROM device_credentials
                 WHERE vault_id = ?1 AND device_id = ?2 ORDER BY serial ASC",
            )?;
            let rows = stmt.query_map((conn.scope().as_slice(), device_id.as_slice()), |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))
            })?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
        .expect("credentials")
}

// ---------------------------------------------------------------- 控制记录构造

/// 按 signer 顺序匹配私钥签名（role2 用 `recover` 域、role3 用 `pop` 域）。
fn sign_with(
    v2: &V2Fixture,
    operation: &proto::OperationV2,
    keyed: &[(proto::SignerV2, &SigningKey)],
) -> proto::SignedControlV2 {
    let keys: Vec<&SigningKey> = operation
        .signers
        .iter()
        .map(|signer| {
            keyed
                .iter()
                .find(|(candidate, _)| candidate == signer)
                .map(|(_, key)| *key)
                .unwrap_or_else(|| panic!("缺少 signer 私钥: {signer:?}"))
        })
        .collect();
    v2.sign_operation_with(operation, &keys)
}

fn recovery_signer(pk: [u8; 32]) -> proto::SignerV2 {
    proto::SignerV2 {
        role: proto::SignerRole::OfflineRecovery,
        device_id: None,
        cap_id: None,
        pk,
    }
}

fn device_signer_with_cap(device_id: Id16, cap_id: [u8; 16], pk: [u8; 32]) -> proto::SignerV2 {
    proto::SignerV2 {
        role: proto::SignerRole::Device,
        device_id: Some(device_id),
        cap_id: Some(cap_id),
        pk,
    }
}

fn pop_signer(device_id: Id16, pk: [u8; 32]) -> proto::SignerV2 {
    proto::SignerV2 {
        role: proto::SignerRole::TargetPop,
        device_id: Some(device_id),
        cap_id: None,
        pk,
    }
}

/// 新代 bootstrap cap（`issued_*` 绑定**提交后**的值）。
fn new_cap(
    v2: &V2Fixture,
    cap_id: [u8; 16],
    device_id: Id16,
    post_control_rev: u64,
    post_authz_rev: u64,
    post_authority_rev: u64,
) -> proto::ManagementCapV2 {
    new_cap_with_pk(
        v2,
        cap_id,
        device_id,
        v2.sign_pk(&device_id),
        post_control_rev,
        post_authz_rev,
        post_authority_rev,
    )
}

fn new_cap_with_pk(
    v2: &V2Fixture,
    cap_id: [u8; 16],
    device_id: Id16,
    sign_pk: [u8; 32],
    post_control_rev: u64,
    post_authz_rev: u64,
    post_authority_rev: u64,
) -> proto::ManagementCapV2 {
    let vault = v2.vault();
    proto::ManagementCapV2 {
        version: proto::V2_WIRE_VERSION,
        instance_id: vault.instance_id,
        vault_id: vault.vault_id,
        cap_id,
        subject_device_id: device_id,
        subject_ed_pk: sign_pk,
        scopes: proto::CapScope::new(proto::CapScope::ALL).expect("scopes"),
        not_before: v2.now_u64().saturating_sub(10),
        expires_at: v2.now_u64().saturating_add(86_400),
        issued_control_rev: post_control_rev,
        issued_authz_rev: post_authz_rev,
        issuer_authority_rev: post_authority_rev,
        delegable: false,
    }
}

fn reset_payload(
    v2: &V2Fixture,
    fresh_recovery_pk: [u8; 32],
    caps: Vec<proto::ManagementCapV2>,
    device_adds: Vec<proto::DeviceCredentialPayloadV2>,
    revoke_ids: Vec<Id16>,
) -> proto::RecoveryResetPayloadV2 {
    proto::RecoveryResetPayloadV2 {
        policy: v2.default_policy(),
        fresh_recovery_pk,
        bootstrap_caps: caps,
        device_adds,
        revoke_ids,
    }
}

fn reset_operation(
    v2: &V2Fixture,
    request_id: [u8; 16],
    payload: &proto::RecoveryResetPayloadV2,
    signers: Vec<proto::SignerV2>,
) -> proto::OperationV2 {
    v2.base_operation(
        proto::ControlKindV2::RecoveryReset,
        request_id,
        payload.encode().expect("reset payload"),
        signers,
        v2.now_u64(),
    )
}

fn key_change_operation(
    v2: &V2Fixture,
    request_id: [u8; 16],
    payload: &proto::RecoveryKeyChangePayloadV2,
    signers: Vec<proto::SignerV2>,
) -> proto::OperationV2 {
    v2.base_operation(
        proto::ControlKindV2::RecoveryKeyChange,
        request_id,
        payload.encode().expect("key change payload"),
        signers,
        v2.now_u64(),
    )
}

async fn submit_ok(app: &Router, v2: &V2Fixture, signed: &proto::SignedControlV2) -> ReceiptView {
    let (status, body) = submit(app, &v2.vault_id(), signed).await;
    assert_eq!(status, StatusCode::OK, "submit: {}", error_code(&body));
    receipt(&body)
}

async fn submit_err(
    app: &Router,
    v2: &V2Fixture,
    signed: &proto::SignedControlV2,
    expected: StatusCode,
    code: &str,
) {
    let (status, body) = submit(app, &v2.vault_id(), signed).await;
    assert_eq!(
        status,
        expected,
        "期望 {expected}，实际 {status}: {}",
        error_code(&body)
    );
    assert_error(&body, code);
}

// ---------------------------------------------------------------- 设备 add 材料

struct PendingDevice {
    device_id: Id16,
    key: SigningKey,
    sign_pk: [u8; 32],
    hpke_pk: [u8; 32],
}

/// 测试专用：直接投影一台 PENDING 设备（真实路径是邀请兑换；恢复场景下邀请已不可用）。
fn insert_pending(
    v2: &V2Fixture,
    device_id: Id16,
    seed: [u8; 32],
    hpke_pk: [u8; 32],
) -> PendingDevice {
    let key = SigningKey::from_bytes(&seed);
    let sign_pk = key.verifying_key().to_bytes();
    let vault_id = v2.vault_id();
    v2.fx
        .db
        .write_blocking(move |conn| {
            conn.execute(
                "INSERT INTO devices(vault_id, device_id, sign_pk, hpke_pk, certificate,
                                     certificate_signature, status, created_at)
                 VALUES (?1, ?2, ?3, ?4, X'', NULL, 'pending', ?5)",
                (
                    conn.scope().as_slice(),
                    device_id.as_slice(),
                    sign_pk.as_slice(),
                    hpke_pk.as_slice(),
                    BASE_TIME,
                ),
            )?;
            Ok(())
        })
        .expect("insert pending");
    let _ = vault_id;
    PendingDevice {
        device_id,
        key,
        sign_pk,
        hpke_pk,
    }
}

/// 设备 add 的证书 + 当前 epoch 的 grant（`issued_*`/`authority_rev` 绑定提交后的值）。
fn add_parts(
    v2: &V2Fixture,
    pending: &PendingDevice,
    grant_id: [u8; 16],
    post_authz_rev: u64,
    post_authority_rev: u64,
) -> proto::DeviceCredentialPayloadV2 {
    let vault = v2.vault();
    let cert = proto::DeviceCertV2 {
        version: proto::V2_WIRE_VERSION,
        instance_id: vault.instance_id,
        vault_id: vault.vault_id,
        device_id: pending.device_id,
        sign_ed25519_pub: pending.sign_pk,
        hpke_x25519_pub: pending.hpke_pk,
        cert_serial: 1,
        data_rights: proto::Rights::new(proto::Rights::ALL).expect("rights"),
        issued_authz_rev: post_authz_rev,
    };
    let aad = proto::GrantAadV2 {
        version: proto::V2_WIRE_VERSION,
        instance_id: vault.instance_id,
        vault_id: vault.vault_id,
        epoch: vault.active_epoch,
        recipient_device_id: pending.device_id,
        recipient_hpke_pub: pending.hpke_pk,
        authority_rev: post_authority_rev,
        grant_id,
        issued_authz_rev: post_authz_rev,
    };
    proto::DeviceCredentialPayloadV2 {
        cert,
        grants: vec![proto::GrantPackageV2 {
            aad,
            enc: [0x9Au8; 32],
            ciphertext: vec![0xB9u8; 40],
        }],
    }
}

// ---------------------------------------------------------------- 1. Reset 正例

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_reset_switches_authority_generation_and_installs_new_caps() {
    let (v2, app) = upgraded("s4c-reset").await;
    let vault_before = v2.vault();
    assert_eq!(vault_before.authz_state.as_str(), "active");
    let old_recovery_pk = vault_before.recovery_pub.expect("recovery pub");

    // 旧恢复 bearer（realm 4）在 Reset 前可用。
    let old_recovery_token =
        recovery_login(&app, &v2, recovery_subject(&old_recovery_pk), &v2.recovery)
            .await
            .expect("old recovery token");
    let (status, _) = call(
        &app,
        get(
            &format!("{}/recovery/evidence", hex_path(&v2.vault_id())),
            Some(&old_recovery_token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // 新恢复根（独立 seed）+ 新代 caps（两台设备各一枚 CAP_ADMIN）。
    let fresh = SigningKey::from_bytes(&[0x5Du8; 32]);
    let fresh_pk = fresh.verifying_key().to_bytes();
    let post_control_rev = vault_before.control_rev + 1;
    let post_authz_rev = vault_before.authz_rev + 1;
    let post_authority_rev = vault_before.authority_rev + 1;
    let caps = vec![
        new_cap(
            &v2,
            [0xD1; 16],
            v2.extra[0].0,
            post_control_rev,
            post_authz_rev,
            post_authority_rev,
        ),
        new_cap(
            &v2,
            [0xD2; 16],
            v2.extra[1].0,
            post_control_rev,
            post_authz_rev,
            post_authority_rev,
        ),
    ];
    let payload = reset_payload(&v2, fresh_pk, caps, Vec::new(), Vec::new());
    let operation = reset_operation(
        &v2,
        [0xF1; 16],
        &payload,
        vec![recovery_signer(old_recovery_pk), recovery_signer(fresh_pk)],
    );
    let signed = sign_with(
        &v2,
        &operation,
        &[
            (recovery_signer(old_recovery_pk), &v2.recovery),
            (recovery_signer(fresh_pk), &fresh),
        ],
    );
    let receipt = submit_ok(&app, &v2, &signed).await;
    assert_eq!(receipt.kind, 13);
    assert_eq!(receipt.control_rev, post_control_rev);
    assert_eq!(receipt.authz_rev, post_authz_rev);
    assert_eq!(receipt.authority_rev, post_authority_rev);

    let vault_after = v2.vault();
    assert_eq!(vault_after.authority_rev, post_authority_rev);
    assert_eq!(vault_after.recovery_pub, Some(fresh_pk));
    assert_eq!(vault_after.authz_state.as_str(), "active");
    assert!(vault_after.auth_rev > vault_before.auth_rev);

    // 旧代 cap 立即失效（authority 代不符）；新代 cap 可用。
    let (status, body) = call(
        &app,
        post_cbor(
            &format!("{}/authz/challenges", hex_path(&v2.vault_id())),
            None,
            vec![
                (0, wire::uint(3)),
                (1, wire::bytes(v2.extra[0].0.to_vec())),
                (2, wire::bytes([0xC1u8; 16].to_vec())),
            ],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{}", error_code(&body));
    let new_token = authz_login(
        &app,
        &v2.vault_id(),
        v2.extra[0].0,
        &v2.extra[0].1,
        Some([0xD1; 16]),
    )
    .await
    .expect("new generation cap token");
    let (status, _) = call(
        &app,
        get(
            &format!("{}/state", hex_path(&v2.vault_id())),
            Some(&new_token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // 旧恢复 bearer 失效；新恢复 bearer 可用。
    let (status, body) = call(
        &app,
        get(
            &format!("{}/recovery/evidence", hex_path(&v2.vault_id())),
            Some(&old_recovery_token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{}", error_code(&body));
    assert_error(&body, "REVOKED");
    let new_recovery_token = recovery_login(&app, &v2, recovery_subject(&fresh_pk), &fresh)
        .await
        .expect("new recovery token");
    let (status, body) = call(
        &app,
        get(
            &format!("{}/recovery/evidence", hex_path(&v2.vault_id())),
            Some(&new_recovery_token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&body));
    let evidence = cbor_fields(&body);
    assert_eq!(
        wire::uint_field(&evidence, 1).expect("authority"),
        post_authority_rev
    );
    assert_eq!(
        wire::bytes_field(&evidence, 5).expect("recovery pub"),
        fresh_pk.to_vec()
    );
    assert_chain_continuous(&v2);
}

/// realm 4（离线恢复根）的 challenge → token（`authz_login` 只覆盖 realm 1/3）。
async fn recovery_login(
    app: &Router,
    v2: &V2Fixture,
    subject: Id16,
    signing: &SigningKey,
) -> Result<String, (StatusCode, Vec<u8>)> {
    let (status, body) = call(
        app,
        post_cbor(
            &format!("{}/authz/challenges", hex_path(&v2.vault_id())),
            None,
            vec![(0, wire::uint(4)), (1, wire::bytes(subject.to_vec()))],
        ),
    )
    .await;
    if status != StatusCode::OK {
        return Err((status, body));
    }
    let challenge_bytes = wire::bytes_field(&cbor_fields(&body), 0).expect("challenge bytes");
    let signature = {
        use ed25519_dalek::Signer as _;
        signing
            .sign(&proto::auth_signing_input_v2(&challenge_bytes))
            .to_bytes()
            .to_vec()
    };
    let (status, body) = call(
        app,
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
    if status != StatusCode::OK {
        return Err((status, body));
    }
    Ok(wire::text_field(&cbor_fields(&body), 0).expect("token"))
}

fn recovery_subject(recovery_pk: &[u8; 32]) -> Id16 {
    let digest = relay::ids::sha256(recovery_pk);
    let mut subject = [0u8; 16];
    subject.copy_from_slice(&digest[..16]);
    subject
}

// ---------------------------------------------------------------- 2. Reset 负例

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_reset_rejects_single_sign_bad_pop_and_legacy_bypasses() {
    let (v2, app) = upgraded("s4c-reset-neg").await;
    let vault_before = v2.vault();
    let old_recovery_pk = vault_before.recovery_pub.expect("recovery pub");
    let fresh = SigningKey::from_bytes(&[0x5Eu8; 32]);
    let fresh_pk = fresh.verifying_key().to_bytes();
    let post_control_rev = vault_before.control_rev + 1;
    let post_authz_rev = vault_before.authz_rev + 1;
    let post_authority_rev = vault_before.authority_rev + 1;
    let caps = vec![
        new_cap(
            &v2,
            [0xD1; 16],
            v2.extra[0].0,
            post_control_rev,
            post_authz_rev,
            post_authority_rev,
        ),
        new_cap(
            &v2,
            [0xD2; 16],
            v2.extra[1].0,
            post_control_rev,
            post_authz_rev,
            post_authority_rev,
        ),
    ];
    let payload = reset_payload(&v2, fresh_pk, caps.clone(), Vec::new(), Vec::new());

    // —— 单签（只有旧根）→ 403 ——
    let operation = reset_operation(
        &v2,
        [0xF2; 16],
        &payload,
        vec![recovery_signer(old_recovery_pk)],
    );
    let signed = v2.sign_operation(&operation);
    submit_err(&app, &v2, &signed, StatusCode::FORBIDDEN, "FORBIDDEN").await;

    // —— 新钥 PoP 用错私钥 → 400 SIGNATURE_INVALID ——
    let operation = reset_operation(
        &v2,
        [0xF3; 16],
        &payload,
        vec![recovery_signer(old_recovery_pk), recovery_signer(fresh_pk)],
    );
    let imposter = SigningKey::from_bytes(&[0x5Fu8; 32]);
    let signed = sign_with(
        &v2,
        &operation,
        &[
            (recovery_signer(old_recovery_pk), &v2.recovery),
            (recovery_signer(fresh_pk), &imposter),
        ],
    );
    submit_err(
        &app,
        &v2,
        &signed,
        StatusCode::BAD_REQUEST,
        "SIGNATURE_INVALID",
    )
    .await;

    // —— 新钥 = 旧钥（不换钥）→ 409 INVALID_STATE ——
    let same_payload = reset_payload(&v2, old_recovery_pk, caps.clone(), Vec::new(), Vec::new());
    let operation = reset_operation(
        &v2,
        [0xF4; 16],
        &same_payload,
        vec![recovery_signer(old_recovery_pk)],
    );
    let signed = v2.sign_operation(&operation);
    submit_err(&app, &v2, &signed, StatusCode::CONFLICT, "INVALID_STATE").await;

    // —— 旧 creator/manager 单签旁路：manager 私钥冒充 role2 → 403 ——
    let manager_pk = v2.manager_key().verifying_key().to_bytes();
    let operation = reset_operation(
        &v2,
        [0xF5; 16],
        &payload,
        vec![recovery_signer(manager_pk), recovery_signer(fresh_pk)],
    );
    let signed = sign_with(
        &v2,
        &operation,
        &[
            (recovery_signer(manager_pk), v2.manager_key()),
            (recovery_signer(fresh_pk), &fresh),
        ],
    );
    submit_err(&app, &v2, &signed, StatusCode::FORBIDDEN, "FORBIDDEN").await;

    // —— 复活 revoked 设备 → 403 ——
    let pending = blocking(|| insert_pending(&v2, [0x7A; 16], [0x3A; 32], [0x4A; 32]));
    let add = add_parts(
        &v2,
        &pending,
        [0x91; 16],
        post_authz_rev,
        post_authority_rev,
    );
    let pending_id = pending.device_id;
    let db = v2.fx.db.clone();
    blocking(move || {
        db.write_blocking(move |conn| {
            conn.execute(
                "UPDATE devices SET status = 'revoked' WHERE vault_id = ?1 AND device_id = ?2",
                (conn.scope().as_slice(), pending_id.as_slice()),
            )?;
            Ok(())
        })
        .expect("revoke pending")
    });
    let revive_payload = reset_payload(&v2, fresh_pk, caps.clone(), vec![add], Vec::new());
    let operation = reset_operation(
        &v2,
        [0xF6; 16],
        &revive_payload,
        vec![recovery_signer(old_recovery_pk), recovery_signer(fresh_pk)],
    );
    let signed = sign_with(
        &v2,
        &operation,
        &[
            (recovery_signer(old_recovery_pk), &v2.recovery),
            (recovery_signer(fresh_pk), &fresh),
        ],
    );
    submit_err(&app, &v2, &signed, StatusCode::FORBIDDEN, "FORBIDDEN").await;

    // —— 降门限（min_quorum=1）在 proto 层就拒绝 ——
    let mut downgraded = v2.default_policy();
    downgraded.min_quorum = 1;
    assert!(downgraded.validate().is_err());
    assert!(downgraded.encode().is_err());

    // —— 控制链未推进（全部负例零写入）——
    assert_eq!(
        control_count(&v2),
        i64::try_from(vault_before.control_rev).expect("rev")
    );
    assert_eq!(v2.vault().authority_rev, vault_before.authority_rev);

    // —— legacy vault（未 Upgrade）没有恢复根 → 409 ——
    let legacy = new_fixture("s4c-reset-legacy");
    let legacy_app = support::v2::app(&legacy);
    let legacy_cap = proto::ManagementCapV2 {
        version: proto::V2_WIRE_VERSION,
        instance_id: legacy.vault().instance_id,
        vault_id: legacy.vault_id(),
        cap_id: [0xDB; 16],
        subject_device_id: legacy.genesis_device(),
        subject_ed_pk: legacy.sign_pk(&legacy.genesis_device()),
        scopes: proto::CapScope::new(proto::CapScope::ALL).expect("scopes"),
        not_before: u64::try_from(legacy.now() - 10).expect("not_before"),
        expires_at: u64::try_from(legacy.now() + 3_600).expect("expires_at"),
        issued_control_rev: 2,
        issued_authz_rev: 1,
        issuer_authority_rev: 1,
        delegable: false,
    };
    let legacy_payload = reset_payload(&legacy, fresh_pk, vec![legacy_cap], Vec::new(), Vec::new());
    let operation = reset_operation(
        &legacy,
        [0xF7; 16],
        &legacy_payload,
        vec![recovery_signer(fresh_pk)],
    );
    let signed = legacy.sign_operation(&operation);
    submit_err(
        &legacy_app,
        &legacy,
        &signed,
        StatusCode::CONFLICT,
        "INVALID_STATE",
    )
    .await;

    // —— 跨 vault：另一租户的恢复记录不能落到本 vault ——
    let other = blocking(|| v2.create_second_vault([0xE8; 16]));
    let mut foreign = operation.clone();
    foreign.vault_id = other.vault_id;
    foreign.canonical_path = proto::canonical_submit_path(&other.vault_id);
    let foreign_signed = v2.sign_operation(&foreign);
    let (status, body) = submit(&app, &other.vault_id, &foreign_signed).await;
    assert!(
        status == StatusCode::FORBIDDEN || status == StatusCode::CONFLICT,
        "跨 vault 必须拒绝（实际 {status}: {}）",
        error_code(&body)
    );
    assert_chain_continuous(&v2);
}

// ---------------------------------------------------------------- 3. KeyChange

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_key_change_swaps_only_the_recovery_root() {
    let (v2, app) = upgraded("s4c-keychange").await;
    let vault_before = v2.vault();
    let old_recovery_pk = vault_before.recovery_pub.expect("recovery pub");
    let fresh = SigningKey::from_bytes(&[0x5Du8; 32]);
    let fresh_pk = fresh.verifying_key().to_bytes();
    let post_control_rev = vault_before.control_rev + 1;
    let post_authz_rev = vault_before.authz_rev + 1;
    let post_authority_rev = vault_before.authority_rev + 1;
    let caps = vec![
        new_cap(
            &v2,
            [0xD1; 16],
            v2.extra[0].0,
            post_control_rev,
            post_authz_rev,
            post_authority_rev,
        ),
        new_cap(
            &v2,
            [0xD2; 16],
            v2.extra[1].0,
            post_control_rev,
            post_authz_rev,
            post_authority_rev,
        ),
    ];
    let payload = proto::RecoveryKeyChangePayloadV2 {
        fresh_recovery_pk: fresh_pk,
        bootstrap_caps: caps.clone(),
    };
    let signers = vec![
        device_signer_with_cap(v2.extra[0].0, [0xC1; 16], v2.sign_pk(&v2.extra[0].0)),
        device_signer_with_cap(v2.extra[1].0, [0xC2; 16], v2.sign_pk(&v2.extra[1].0)),
        recovery_signer(fresh_pk),
    ];

    // —— 单签（缺一台 POLICY）→ 403 ——
    let operation = key_change_operation(
        &v2,
        [0xE1; 16],
        &payload,
        vec![signers[0].clone(), signers[2].clone()],
    );
    let signed = sign_with(
        &v2,
        &operation,
        &[
            (signers[0].clone(), v2.key(&v2.extra[0].0)),
            (signers[2].clone(), &fresh),
        ],
    );
    submit_err(&app, &v2, &signed, StatusCode::FORBIDDEN, "FORBIDDEN").await;

    // —— 旧恢复钥当 PoP（没持有新钥）→ 400 ——
    let operation = key_change_operation(&v2, [0xE2; 16], &payload, signers.clone());
    let signed = sign_with(
        &v2,
        &operation,
        &[
            (signers[0].clone(), v2.key(&v2.extra[0].0)),
            (signers[1].clone(), v2.key(&v2.extra[1].0)),
            (signers[2].clone(), &v2.recovery),
        ],
    );
    submit_err(
        &app,
        &v2,
        &signed,
        StatusCode::BAD_REQUEST,
        "SIGNATURE_INVALID",
    )
    .await;

    // —— 正例 ——
    let operation = key_change_operation(&v2, [0xE3; 16], &payload, signers.clone());
    let signed = sign_with(
        &v2,
        &operation,
        &[
            (signers[0].clone(), v2.key(&v2.extra[0].0)),
            (signers[1].clone(), v2.key(&v2.extra[1].0)),
            (signers[2].clone(), &fresh),
        ],
    );
    let receipt = submit_ok(&app, &v2, &signed).await;
    assert_eq!(receipt.kind, 14);
    assert_eq!(receipt.authority_rev, post_authority_rev);
    assert_eq!(receipt.authz_rev, post_authz_rev);

    let vault_after = v2.vault();
    assert_eq!(vault_after.recovery_pub, Some(fresh_pk));
    assert_eq!(vault_after.authority_rev, post_authority_rev);
    // 只换恢复钥：policy 与成员集合不变。
    assert_eq!(vault_after.policy_bytes, vault_before.policy_bytes);
    assert_eq!(
        vault_after.authz_state.as_str(),
        vault_before.authz_state.as_str()
    );
    assert_eq!(
        device_status(&v2, &v2.genesis_device()).as_deref(),
        Some("active")
    );
    // 旧代 cap 失效（authority 代不符）；新代 cap 可用。
    let (status, body) = call(
        &app,
        post_cbor(
            &format!("{}/authz/challenges", hex_path(&v2.vault_id())),
            None,
            vec![
                (0, wire::uint(3)),
                (1, wire::bytes(v2.extra[0].0.to_vec())),
                (2, wire::bytes([0xC1u8; 16].to_vec())),
            ],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{}", error_code(&body));
    let new_token = authz_login(
        &app,
        &v2.vault_id(),
        v2.extra[1].0,
        &v2.extra[1].1,
        Some([0xD2; 16]),
    )
    .await
    .expect("new generation cap token");
    let (status, _) = call(
        &app,
        get(
            &format!("{}/state", hex_path(&v2.vault_id())),
            Some(&new_token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // 旧恢复 bearer 失效。
    let (status, _) = call(
        &app,
        post_cbor(
            &format!("{}/authz/challenges", hex_path(&v2.vault_id())),
            None,
            vec![
                (0, wire::uint(4)),
                (1, wire::bytes(recovery_subject(&old_recovery_pk).to_vec())),
            ],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_chain_continuous(&v2);
}

// ---------------------------------------------------------------- 4. recovery_only

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_only_allows_only_offline_reset_and_never_single_sign() {
    let (v2, app) = upgraded("s4c-recovery-only").await;
    let vault_before = v2.vault();
    let old_recovery_pk = vault_before.recovery_pub.expect("recovery pub");
    let fresh = SigningKey::from_bytes(&[0x5Du8; 32]);
    let fresh_pk = fresh.verifying_key().to_bytes();

    // Reset #1：撤销两台设备 → 只剩 1 台 ACTIVE → recovery_only。
    let post_control_rev = vault_before.control_rev + 1;
    let post_authz_rev = vault_before.authz_rev + 1;
    let post_authority_rev = vault_before.authority_rev + 1;
    let caps = vec![new_cap(
        &v2,
        [0xD1; 16],
        v2.extra[0].0,
        post_control_rev,
        post_authz_rev,
        post_authority_rev,
    )];
    let payload = reset_payload(
        &v2,
        fresh_pk,
        caps,
        Vec::new(),
        vec![v2.genesis_device(), v2.extra[1].0],
    );
    let operation = reset_operation(
        &v2,
        [0xE4; 16],
        &payload,
        vec![recovery_signer(old_recovery_pk), recovery_signer(fresh_pk)],
    );
    let signed = sign_with(
        &v2,
        &operation,
        &[
            (recovery_signer(old_recovery_pk), &v2.recovery),
            (recovery_signer(fresh_pk), &fresh),
        ],
    );
    let receipt = submit_ok(&app, &v2, &signed).await;
    assert_eq!(receipt.kind, 13);
    assert_eq!(v2.vault().authz_state.as_str(), "recovery_only");
    assert_eq!(
        device_status(&v2, &v2.genesis_device()).as_deref(),
        Some("revoked")
    );

    // 普通管理写（CapGrant）在 recovery_only 下 403（旧 cap 也已失效）。
    let cap_payload = proto::CapGrantPayloadV2 {
        cap: new_cap(
            &v2,
            [0xD9; 16],
            v2.extra[0].0,
            vault_before.control_rev + 2,
            post_authz_rev + 1,
            post_authority_rev,
        ),
    };
    let operation = v2.two_signer_operation(
        proto::ControlKindV2::CapGrant,
        [0xE5; 16],
        cap_payload.encode().expect("cap payload"),
        &[(v2.extra[0].0, [0xD1; 16]), (v2.extra[1].0, [0xD2; 16])],
        v2.now_u64(),
    );
    let signed = v2.sign_operation(&operation);
    let (status, body) = submit(&app, &v2.vault_id(), &signed).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{}", error_code(&body));

    // 设备 realm 仍可读（撤权不等于删数据）。
    let device_token = authz_login(&app, &v2.vault_id(), v2.extra[0].0, &v2.extra[0].1, None)
        .await
        .expect("device token in recovery_only");
    let (status, _) = call(
        &app,
        get(
            &format!("{}/state", hex_path(&v2.vault_id())),
            Some(&device_token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Reset #2：带设备 add（PENDING → ACTIVE）→ 回到 active。
    let pending = blocking(|| insert_pending(&v2, [0x7B; 16], [0x3B; 32], [0x4B; 32]));
    let vault_mid = v2.vault();
    let post2_control_rev = vault_mid.control_rev + 1;
    let post2_authz_rev = vault_mid.authz_rev + 1;
    let post2_authority_rev = vault_mid.authority_rev + 1;
    let add = add_parts(
        &v2,
        &pending,
        [0x92; 16],
        post2_authz_rev,
        post2_authority_rev,
    );
    let caps2 = vec![
        new_cap(
            &v2,
            [0xD3; 16],
            v2.extra[0].0,
            post2_control_rev,
            post2_authz_rev,
            post2_authority_rev,
        ),
        new_cap_with_pk(
            &v2,
            [0xD4; 16],
            pending.device_id,
            pending.sign_pk,
            post2_control_rev,
            post2_authz_rev,
            post2_authority_rev,
        ),
    ];
    let fresh2 = SigningKey::from_bytes(&[0x5Cu8; 32]);
    let fresh2_pk = fresh2.verifying_key().to_bytes();
    let payload = reset_payload(&v2, fresh2_pk, caps2, vec![add], Vec::new());
    let operation = reset_operation(
        &v2,
        [0xE6; 16],
        &payload,
        vec![
            recovery_signer(fresh_pk),
            recovery_signer(fresh2_pk),
            pop_signer(pending.device_id, pending.sign_pk),
        ],
    );
    let signed = sign_with(
        &v2,
        &operation,
        &[
            (recovery_signer(fresh_pk), &fresh),
            (recovery_signer(fresh2_pk), &fresh2),
            (pop_signer(pending.device_id, pending.sign_pk), &pending.key),
        ],
    );
    let receipt = submit_ok(&app, &v2, &signed).await;
    assert_eq!(receipt.kind, 13);
    assert_eq!(v2.vault().authz_state.as_str(), "active");
    assert_eq!(
        device_status(&v2, &pending.device_id).as_deref(),
        Some("active")
    );
    // 凭证历史 + 当前 epoch 的 grant 都落库。
    let history = credentials(&v2, &pending.device_id);
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].0, 1);
    let grant_count: i64 = v2
        .fx
        .db
        .read(move |conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM device_grants
                 WHERE vault_id = ?1 AND device_id = ?2",
                (conn.scope().as_slice(), pending.device_id.as_slice()),
                |row| row.get(0),
            )?)
        })
        .expect("grant count");
    assert_eq!(grant_count, 1);
    // 新代 cap 可用（recovery_only 已解除）。
    let token = authz_login(
        &app,
        &v2.vault_id(),
        pending.device_id,
        &pending.key,
        Some([0xD4; 16]),
    )
    .await
    .expect("new device management token");
    let (status, _) = call(
        &app,
        get(&format!("{}/state", hex_path(&v2.vault_id())), Some(&token)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_chain_continuous(&v2);
}

// ---------------------------------------------------------------- 5. realm 4

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_realm_is_subject_bound_short_lived_and_read_only() {
    let (v2, app) = upgraded("s4c-realm4").await;
    let recovery_pk = v2.vault().recovery_pub.expect("recovery pub");
    let subject = recovery_subject(&recovery_pk);
    let evidence_path = format!("{}/recovery/evidence", hex_path(&v2.vault_id()));

    // 无 bearer → 401。
    let (status, _) = call(&app, get(&evidence_path, None)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // 设备 bearer 不能读恢复证据（realm 1 不授予恢复面）。
    let device_token = authz_login(&app, &v2.vault_id(), v2.extra[0].0, &v2.extra[0].1, None)
        .await
        .expect("device token");
    let (status, _) = call(&app, get(&evidence_path, Some(&device_token))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // challenge 的 subject 必须是 SHA256(recovery_pub)[..16]；错 subject → 403。
    let (status, body) = call(
        &app,
        post_cbor(
            &format!("{}/authz/challenges", hex_path(&v2.vault_id())),
            None,
            vec![(0, wire::uint(4)), (1, wire::bytes([0x11u8; 16].to_vec()))],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{}", error_code(&body));
    // realm 4 不接受 cap_id。
    let (status, _) = call(
        &app,
        post_cbor(
            &format!("{}/authz/challenges", hex_path(&v2.vault_id())),
            None,
            vec![
                (0, wire::uint(4)),
                (1, wire::bytes(subject.to_vec())),
                (2, wire::bytes([0xC1u8; 16].to_vec())),
            ],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // 正例：challenge → token（TTL 60s）→ 读证据。
    let token = recovery_login(&app, &v2, subject, &v2.recovery)
        .await
        .expect("recovery token");
    let (status, body) = call(&app, get(&evidence_path, Some(&token))).await;
    assert_eq!(status, StatusCode::OK, "{}", error_code(&body));
    let evidence = cbor_fields(&body);
    assert_eq!(
        wire::text_field(&evidence, 0).expect("authz state"),
        "active"
    );
    assert_eq!(
        wire::bytes_field(&evidence, 5).expect("recovery pub"),
        recovery_pk.to_vec()
    );
    // 成员集合摘要可复算（恢复端据此决定新 caps 绑定）。
    let expected_member = v2
        .fx
        .db
        .read(relay::control_v2::member_cert_set_digest)
        .expect("member digest");
    assert_eq!(
        wire::bytes_field(&evidence, 10).expect("member hash"),
        expected_member.to_vec()
    );

    // 恢复 bearer 不能当设备 bearer 用（不能读数据面）。
    let (status, _) = call(
        &app,
        get(&format!("{}/state", hex_path(&v2.vault_id())), Some(&token)),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // TTL 60s：FakeClock 推进 61s 后 token 过期。
    let issued_at = v2.now();
    v2.fx.clock.set(issued_at + 61);
    let (status, _) = call(&app, get(&evidence_path, Some(&token))).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    v2.fx.clock.set(issued_at);
}
