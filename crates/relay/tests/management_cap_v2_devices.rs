//! ADR 0015 **S4b2**：`DeviceApprove`（kind 4）/`DeviceRevoke`（kind 5）/
//! `DeviceRightsChange`（kind 16）。
//!
//! 覆盖：
//! 1. 批准：真实 PENDING（邀请兑换，已提交 sign/hpke 公钥 + 真实 PoP）→ 两签 APPROVE 成功；
//!    凭证历史/`devices`/grant 投影/`authz`+`auth` rev/Bearer 可用；已 ACTIVE 的逐字节重放幂等；
//!    单签/同设备两 cap/错 PENDING key/错 PoP/换包/错 recipient/错 rev/serial 复用/未知设备/
//!    陈旧 rev/复活 revoked 全部拒绝且不推进控制链；
//! 2. 撤销：一台下线 → 其 Bearer、管理 cap、WS 立即失效并失去签名资格；`authz/auth` 前进、
//!    目标全部 cap 落 `revoked_control_rev`；重复撤销幂等（不推进 rev）；最后 quorum 拒绝；
//!    PENDING 目标拒绝；绑定被撤 cap 的未兑换邀请失效；跨 vault 404；v1 管理写关闭；
//! 3. 改权：READ→READ|WRITE 成功、serial 递增、旧证书历史保留、数据 grant 仍可取；
//!    可写 quorum 降破/错 serial/单签/未变化/换钥/已 revoked 全部拒绝；
//! 4. 全部走真实 relay 路由 + 真实 Ed25519；无 sleep（FakeClock + 显式收敛点）；链 hash 逐条重算。

mod support;

use std::time::Duration;

use axum::http::StatusCode;
use axum::Router;
use ed25519_dalek::{Signer as _, SigningKey};
use futures_util::StreamExt;
use relay::ids::{b64url_encode, Id16};
use relay::wire;
use support::v2::*;
use support::*;
use tokio_tungstenite::tungstenite::Message;

use menghunote_sync_proto as proto;

// ---------------------------------------------------------------- 通用夹具

fn new_fixture(label: &str) -> V2Fixture {
    tokio::task::block_in_place(|| V2Fixture::new(label))
}

fn assert_error(body: &[u8], expected: &str) {
    assert_eq!(
        error_code(body),
        expected,
        "错误码不符（实际 {}）",
        error_code(body)
    );
}

/// 两台 CAP_ADMIN 的 bootstrap cap（`default_upgrade_parts` 装的 C1/C2）。
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

struct DeviceState {
    status: String,
    sign_pk: Vec<u8>,
    hpke_pk: Vec<u8>,
    certificate: Vec<u8>,
    approval_control_rev: Option<i64>,
    issued_authz_rev: Option<i64>,
}

fn device_state(v2: &V2Fixture, device_id: &Id16) -> Option<DeviceState> {
    v2.fx
        .db
        .read(|conn| {
            Ok(conn
                .query_row(
                    "SELECT status, sign_pk, hpke_pk, certificate, approval_control_rev,
                            issued_authz_rev
                     FROM devices WHERE vault_id = ?1 AND device_id = ?2",
                    (conn.scope().as_slice(), device_id.as_slice()),
                    |row| {
                        Ok(DeviceState {
                            status: row.get(0)?,
                            sign_pk: row.get(1)?,
                            hpke_pk: row.get(2)?,
                            certificate: row.get(3)?,
                            approval_control_rev: row.get(4)?,
                            issued_authz_rev: row.get(5)?,
                        })
                    },
                )
                .ok())
        })
        .expect("device state")
}

/// `device_credentials` 历史：`(serial, certificate)` 按 serial 升序。
fn credentials(v2: &V2Fixture, device_id: &Id16) -> Vec<(i64, Vec<u8>)> {
    v2.fx
        .db
        .read(|conn| {
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

/// `device_grants` 行：`(hpke_enc, ciphertext, signed_header, grant_id, approval_rev)`。
type GrantRow = (Vec<u8>, Vec<u8>, Vec<u8>, Option<Vec<u8>>, Option<i64>);

fn grant_row(v2: &V2Fixture, epoch: u64, device_id: &Id16) -> Option<GrantRow> {
    v2.fx
        .db
        .read(|conn| {
            Ok(conn
                .query_row(
                    "SELECT hpke_enc, hpke_ciphertext, signed_header, grant_id,
                            approval_control_rev
                     FROM device_grants WHERE vault_id = ?1 AND epoch = ?2 AND device_id = ?3",
                    (
                        conn.scope().as_slice(),
                        i64::try_from(epoch).expect("epoch"),
                        device_id.as_slice(),
                    ),
                    |row| {
                        Ok((
                            row.get::<_, Vec<u8>>(0)?,
                            row.get::<_, Vec<u8>>(1)?,
                            row.get::<_, Vec<u8>>(2)?,
                            row.get::<_, Option<Vec<u8>>>(3)?,
                            row.get::<_, Option<i64>>(4)?,
                        ))
                    },
                )
                .ok())
        })
        .expect("grant row")
}

fn cap_revoked_control_rev(v2: &V2Fixture, cap_id: &[u8; 16]) -> Option<i64> {
    v2.fx
        .db
        .read(|conn| {
            Ok(conn
                .query_row(
                    "SELECT revoked_control_rev FROM management_caps
                     WHERE vault_id = ?1 AND cap_id = ?2",
                    (conn.scope().as_slice(), cap_id.as_slice()),
                    |row| row.get::<_, Option<i64>>(0),
                )
                .ok()
                .flatten())
        })
        .expect("cap row")
}

/// 控制链一行：`(rev, prev_hash, record_hash, signed_record)`。
type ChainRow = (i64, Vec<u8>, Vec<u8>, Vec<u8>);

/// 逐条重算控制链：`record_hash = SHA256(prev_hash || signed_record)`，rev 从 1 连续。
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
        assert_eq!(
            prev_hash.as_slice(),
            previous.as_slice(),
            "rev {rev} 的 prev_hash 不指向上一条"
        );
        let expected = relay::ids::sha256_parts(&[previous.as_slice(), signed_record.as_slice()]);
        assert_eq!(
            record_hash.as_slice(),
            expected.as_slice(),
            "rev {rev} 的 record_hash 不可复算"
        );
        previous = expected;
    }
}

// ---------------------------------------------------------------- PENDING 设备

struct PendingDevice {
    device_id: Id16,
    key: SigningKey,
    sign_pk: [u8; 32],
    hpke_pk: [u8; 32],
}

/// 用一台 INVITE cap 建邀请并兑换成 PENDING（真实 PoP 签名），返回设备材料。
#[allow(clippy::too_many_arguments)]
async fn make_pending(
    v2: &V2Fixture,
    app: &Router,
    request_id: [u8; 16],
    device_id: Id16,
    seed: [u8; 32],
    hpke_pk: [u8; 32],
    issuer: (Id16, Id16),
    invite_id: [u8; 16],
    token: [u8; 32],
) -> PendingDevice {
    let key = SigningKey::from_bytes(&seed);
    let sign_pk = key.verifying_key().to_bytes();
    let payload = proto::InviteCreatePayloadV2 {
        invite_id,
        token_hash: relay::ids::sha256(&token),
        expires_at: v2.now_u64() + 600,
    }
    .encode()
    .expect("invite payload");
    let operation = v2.one_signer_operation(
        proto::ControlKindV2::InviteCreate,
        request_id,
        payload,
        issuer.0,
        issuer.1,
        v2.now_u64(),
    );
    let signed = v2.sign_operation(&operation);
    let (status, body) = submit(app, &v2.vault_id(), &signed).await;
    assert_eq!(status, StatusCode::OK, "invite: {}", error_code(&body));

    let proof = format!("pop-{device_id:?}").into_bytes();
    let signature = key.sign(&proof).to_bytes().to_vec();
    let (status, body) = call(
        app,
        post_cbor(
            &format!(
                "{}/invites/{}/redeem",
                hex_path(&v2.vault_id()),
                b64url_encode(&invite_id)
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
    .await;
    assert_eq!(status, StatusCode::CREATED, "redeem: {}", error_code(&body));
    let state = device_state(v2, &device_id).expect("pending device");
    assert_eq!(state.status, "pending");
    assert_eq!(state.sign_pk, sign_pk.to_vec());
    assert_eq!(state.hpke_pk, hpke_pk.to_vec());
    PendingDevice {
        device_id,
        key,
        sign_pk,
        hpke_pk,
    }
}

// ---------------------------------------------------------------- payload 构造

/// 构造一份凭证 + 当前 epoch 的 grant 包（`issued_authz_rev` = 提交后 authz rev）。
#[allow(clippy::too_many_arguments)]
fn credential_parts(
    v2: &V2Fixture,
    device_id: Id16,
    sign_pk: [u8; 32],
    hpke_pk: [u8; 32],
    serial: u64,
    rights: u32,
    grant_id: [u8; 16],
    ciphertext: Vec<u8>,
) -> (proto::DeviceCertV2, proto::GrantPackageV2) {
    let vault = v2.vault();
    let issued_authz_rev = vault.authz_rev + 1;
    let cert = proto::DeviceCertV2 {
        version: proto::V2_WIRE_VERSION,
        instance_id: vault.instance_id,
        vault_id: vault.vault_id,
        device_id,
        sign_ed25519_pub: sign_pk,
        hpke_x25519_pub: hpke_pk,
        cert_serial: serial,
        data_rights: proto::Rights::new(rights).expect("rights"),
        issued_authz_rev,
    };
    let aad = proto::GrantAadV2 {
        version: proto::V2_WIRE_VERSION,
        instance_id: vault.instance_id,
        vault_id: vault.vault_id,
        epoch: vault.active_epoch,
        recipient_device_id: device_id,
        recipient_hpke_pub: hpke_pk,
        authority_rev: vault.authority_rev,
        grant_id,
        issued_authz_rev,
    };
    (
        cert,
        proto::GrantPackageV2 {
            aad,
            enc: [0x9Au8; 32],
            ciphertext,
        },
    )
}

#[allow(clippy::too_many_arguments)]
fn credential_operation(
    v2: &V2Fixture,
    kind: proto::ControlKindV2,
    request_id: [u8; 16],
    cert: &proto::DeviceCertV2,
    grants: Vec<proto::GrantPackageV2>,
    signers: &[(Id16, Id16)],
    pop: Option<(Id16, [u8; 32])>,
    signed_at: u64,
) -> proto::OperationV2 {
    let payload = proto::DeviceCredentialPayloadV2 {
        cert: cert.clone(),
        grants,
    }
    .encode()
    .expect("credential payload");
    let mut signer_list: Vec<proto::SignerV2> = signers
        .iter()
        .map(|(device_id, cap_id)| proto::SignerV2 {
            role: proto::SignerRole::Device,
            device_id: Some(*device_id),
            cap_id: Some(*cap_id),
            pk: v2.sign_pk(device_id),
        })
        .collect();
    if let Some((device_id, pk)) = pop {
        signer_list.push(proto::SignerV2 {
            role: proto::SignerRole::TargetPop,
            device_id: Some(device_id),
            cap_id: None,
            pk,
        });
    }
    v2.base_operation(kind, request_id, payload, signer_list, signed_at)
}

fn revoke_operation(
    v2: &V2Fixture,
    request_id: [u8; 16],
    mut targets: Vec<Id16>,
    signers: &[(Id16, Id16)],
    signed_at: u64,
) -> proto::OperationV2 {
    targets.sort_unstable();
    let payload = proto::DeviceRevokePayloadV2 {
        device_ids: targets,
    }
    .encode()
    .expect("revoke payload");
    v2.two_signer_operation(
        proto::ControlKindV2::DeviceRevoke,
        request_id,
        payload,
        signers,
        signed_at,
    )
}

/// `DeviceRevoke`（签发者公钥由调用方给，可用于夹具外的已批准设备）。
#[allow(clippy::too_many_arguments)]
fn revoke_operation_with(
    v2: &V2Fixture,
    request_id: [u8; 16],
    mut targets: Vec<Id16>,
    signers: &[(Id16, Id16, [u8; 32])],
    signed_at: u64,
) -> proto::OperationV2 {
    targets.sort_unstable();
    let payload = proto::DeviceRevokePayloadV2 {
        device_ids: targets,
    }
    .encode()
    .expect("revoke payload");
    let signer_list = signers
        .iter()
        .map(|(device_id, cap_id, pk)| proto::SignerV2 {
            role: proto::SignerRole::Device,
            device_id: Some(*device_id),
            cap_id: Some(*cap_id),
            pk: *pk,
        })
        .collect();
    v2.base_operation(
        proto::ControlKindV2::DeviceRevoke,
        request_id,
        payload,
        signer_list,
        signed_at,
    )
}

/// 单签 `InviteCreate`（签发者可以是夹具外的已批准设备，公钥由调用方给）。
#[allow(clippy::too_many_arguments)]
fn invite_operation_with(
    v2: &V2Fixture,
    request_id: [u8; 16],
    invite_id: [u8; 16],
    token: &[u8; 32],
    expires_at: u64,
    device_id: Id16,
    cap_id: Id16,
    sign_pk: [u8; 32],
    signed_at: u64,
) -> proto::OperationV2 {
    let payload = proto::InviteCreatePayloadV2 {
        invite_id,
        token_hash: relay::ids::sha256(token),
        expires_at,
    }
    .encode()
    .expect("invite payload");
    let signers = vec![proto::SignerV2 {
        role: proto::SignerRole::Device,
        device_id: Some(device_id),
        cap_id: Some(cap_id),
        pk: sign_pk,
    }];
    v2.base_operation(
        proto::ControlKindV2::InviteCreate,
        request_id,
        payload,
        signers,
        signed_at,
    )
}

/// 用一把显式私钥签一条单 signer 记录（夹具外的设备用）。
fn sign_single_device(operation: &proto::OperationV2, key: &SigningKey) -> proto::SignedControlV2 {
    let input = proto::control_signing_input(operation, hash_fn).expect("control input");
    proto::SignedControlV2 {
        operation: operation.clone(),
        signatures: vec![key.sign(&input).to_bytes()],
    }
}

/// 按 signer 顺序取私钥签名（PoP 用 `pop` 传入的那把）。
fn sign_operation_keys<'a>(
    v2: &'a V2Fixture,
    operation: &proto::OperationV2,
    pop: Option<&'a SigningKey>,
) -> proto::SignedControlV2 {
    let keys: Vec<&SigningKey> = operation
        .signers
        .iter()
        .map(|signer| match signer.role {
            proto::SignerRole::Device => v2.key(&signer.device_id.expect("device signer")),
            proto::SignerRole::TargetPop => pop.expect("PoP 私钥"),
            other => panic!("未预期的 signer role {other:?}"),
        })
        .collect();
    v2.sign_operation_with(operation, &keys)
}

async fn submit_ok(app: &Router, v2: &V2Fixture, signed: &proto::SignedControlV2) -> ReceiptView {
    let (status, body) = submit(app, &v2.vault_id(), signed).await;
    assert_eq!(status, StatusCode::OK, "submit: {}", error_code(&body));
    receipt(&body)
}

/// 提交一份**已构建**的批准记录（证书/grant 由调用方给，便于重放逐字节相同的包）。
async fn submit_approve(
    v2: &V2Fixture,
    app: &Router,
    request_id: [u8; 16],
    pending: &PendingDevice,
    cert: &proto::DeviceCertV2,
    grants: Vec<proto::GrantPackageV2>,
) -> ReceiptView {
    let operation = credential_operation(
        v2,
        proto::ControlKindV2::DeviceApprove,
        request_id,
        cert,
        grants,
        &admin_signers(v2),
        Some((pending.device_id, pending.sign_pk)),
        v2.now_u64(),
    );
    let signed = sign_operation_keys(v2, &operation, Some(&pending.key));
    submit_ok(app, v2, &signed).await
}

/// 按标准材料批准一台 PENDING 设备（serial=1、当前 epoch grant），返回回执与原始包。
async fn approve_default(
    v2: &V2Fixture,
    app: &Router,
    request_id: [u8; 16],
    pending: &PendingDevice,
    rights: u32,
    grant_id: [u8; 16],
    ciphertext: Vec<u8>,
) -> (ReceiptView, proto::DeviceCertV2, proto::GrantPackageV2) {
    let (cert, grant) = credential_parts(
        v2,
        pending.device_id,
        pending.sign_pk,
        pending.hpke_pk,
        1,
        rights,
        grant_id,
        ciphertext,
    );
    let receipt = submit_approve(v2, app, request_id, pending, &cert, vec![grant.clone()]).await;
    (receipt, cert, grant)
}

// ---------------------------------------------------------------- 1. DeviceApprove

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn device_approve_binds_pending_keys_pop_and_grant_packages() {
    let v2 = new_fixture("s4b2-approve");
    let app = app(&v2);
    let (status, body) = upgrade_default(&v2, &app, [0xA0; 16]).await;
    assert_eq!(status, StatusCode::OK, "upgrade: {}", error_code(&body));

    // PENDING 设备 P1：邀请由 C1（INVITE 位）签发，兑换时提交 sign/hpke 公钥。
    let p1 = make_pending(
        &v2,
        &app,
        [0xB0; 16],
        [0x71; 16],
        [0x31; 32],
        [0x41; 32],
        (v2.extra[0].0, [0xC1; 16]),
        [0x51; 16],
        [0x61; 32],
    )
    .await;

    // —— 正例：两签 APPROVE + 目标 PoP ——
    let vault_before = v2.vault();
    let count_before = control_count(&v2);
    let (receipt, approve_cert, approve_grant) = approve_default(
        &v2,
        &app,
        [0xB1; 16],
        &p1,
        proto::Rights::ALL,
        [0x91; 16],
        vec![0xB1; 48],
    )
    .await;
    assert_eq!(receipt.kind, 4);
    assert_eq!(receipt.control_rev, vault_before.control_rev + 1);
    assert_eq!(receipt.authz_rev, 2, "批准推进 authz rev");
    assert_eq!(receipt.authority_rev, 1);
    assert_eq!(control_count(&v2), count_before + 1);

    let vault_after = v2.vault();
    assert_eq!(vault_after.authz_rev, 2);
    assert_eq!(vault_after.auth_rev, vault_before.auth_rev + 1);

    // devices 投影 + 凭证历史 + grant 投影。
    let state = device_state(&v2, &p1.device_id).expect("approved device");
    assert_eq!(state.status, "active");
    assert_eq!(state.sign_pk, p1.sign_pk.to_vec());
    assert_eq!(state.hpke_pk, p1.hpke_pk.to_vec());
    assert_eq!(
        state.approval_control_rev,
        Some(i64::try_from(receipt.control_rev).expect("rev"))
    );
    assert_eq!(state.issued_authz_rev, Some(2));
    let cert = proto::DeviceCertV2::decode(&state.certificate).expect("v2 cert");
    assert_eq!(cert.device_id, p1.device_id);
    assert_eq!(cert.cert_serial, 1);
    assert_eq!(
        cert.data_rights,
        proto::Rights::new(proto::Rights::ALL).expect("rights")
    );
    assert_eq!(cert.issued_authz_rev, 2);

    let history = credentials(&v2, &p1.device_id);
    assert_eq!(history.len(), 1, "首份 V2 凭证");
    assert_eq!(history[0].0, 1);
    assert_eq!(history[0].1, state.certificate);

    let grant = grant_row(&v2, vault_after.active_epoch, &p1.device_id).expect("grant row");
    assert_eq!(grant.3.as_deref(), Some([0x91u8; 16].as_slice()));
    assert_eq!(
        grant.4,
        Some(i64::try_from(receipt.control_rev).expect("rev"))
    );
    let aad = proto::GrantAadV2::decode(&grant.2).expect("grant aad");
    assert_eq!(aad.recipient_device_id, p1.device_id);
    assert_eq!(aad.recipient_hpke_pub, p1.hpke_pk);
    assert_eq!(aad.epoch, vault_after.active_epoch);
    assert_eq!(aad.authority_rev, 1);
    assert_eq!(aad.issued_authz_rev, 2);
    assert_eq!(grant.0, vec![0x9Au8; 32], "grant 包 enc 原样落库");

    // 批准后本设备可用 V2 设备 bearer 取自己的 grant。
    let token = authz_login(&app, &v2.vault_id(), p1.device_id, &p1.key, None)
        .await
        .expect("approved device token");
    let (status, body) = call(
        &app,
        get(
            &format!("{}/devices/self/grants/latest", hex_path(&v2.vault_id())),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "latest grant: {}",
        error_code(&body)
    );
    let fields = cbor_fields(&body);
    assert_eq!(
        wire::uint_field(&fields, 0).expect("epoch"),
        vault_after.active_epoch
    );
    assert_eq!(
        wire::bytes_field(&fields, 4).expect("ciphertext"),
        vec![0xB1u8; 48]
    );

    // —— 已 ACTIVE：逐字节重放幂等（只追加控制记录，不推进 authz/auth） ——
    let operation = credential_operation(
        &v2,
        proto::ControlKindV2::DeviceApprove,
        [0xB2; 16],
        &approve_cert,
        vec![approve_grant.clone()],
        &admin_signers(&v2),
        None,
        v2.now_u64(),
    );
    let signed = sign_operation_keys(&v2, &operation, None);
    let receipt = submit_ok(&app, &v2, &signed).await;
    assert_eq!(receipt.kind, 4);
    assert_eq!(receipt.authz_rev, 2, "幂等重放不推进 authz rev");
    assert_eq!(control_count(&v2), count_before + 2);
    assert_eq!(credentials(&v2, &p1.device_id).len(), 1);
    // 证书不同（同一设备的新 rev）→ 409：必须走 kind 16，不能在 kind 4 里偷改证书。
    {
        let (mut other_cert, other_grant) = credential_parts(
            &v2,
            p1.device_id,
            p1.sign_pk,
            p1.hpke_pk,
            1,
            proto::Rights::READ,
            [0x92; 16],
            vec![0xB2; 48],
        );
        other_cert.issued_authz_rev = 2;
        let operation = credential_operation(
            &v2,
            proto::ControlKindV2::DeviceApprove,
            [0xC4; 16],
            &other_cert,
            vec![other_grant],
            &admin_signers(&v2),
            None,
            v2.now_u64(),
        );
        let signed = sign_operation_keys(&v2, &operation, None);
        let (status, body) = submit(&app, &v2.vault_id(), &signed).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_error(&body, "INVALID_STATE");
    }

    // —— 负例 ——
    // 单签（2 台里只签 1 台）+ PoP。
    {
        let (cert, grant) = credential_parts(
            &v2,
            p1.device_id,
            p1.sign_pk,
            p1.hpke_pk,
            1,
            proto::Rights::ALL,
            [0x91; 16],
            vec![0xB1; 48],
        );
        let operation = credential_operation(
            &v2,
            proto::ControlKindV2::DeviceApprove,
            [0xB3; 16],
            &cert,
            vec![grant],
            &[admin_signers(&v2)[0]],
            None,
            v2.now_u64(),
        );
        let signed = sign_operation_keys(&v2, &operation, None);
        let (status, body) = submit(&app, &v2.vault_id(), &signed).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_error(&body, "FORBIDDEN");
    }
    // 同设备两 cap（C2 属于另一台设备）→ signer/cap 主体不符 → 403。
    {
        let (cert, grant) = credential_parts(
            &v2,
            p1.device_id,
            p1.sign_pk,
            p1.hpke_pk,
            1,
            proto::Rights::ALL,
            [0x91; 16],
            vec![0xB1; 48],
        );
        let operation = credential_operation(
            &v2,
            proto::ControlKindV2::DeviceApprove,
            [0xB4; 16],
            &cert,
            vec![grant],
            &[(v2.extra[0].0, [0xC1; 16]), (v2.extra[0].0, [0xC2; 16])],
            None,
            v2.now_u64(),
        );
        let signed = sign_operation_keys(&v2, &operation, None);
        let (status, body) = submit(&app, &v2.vault_id(), &signed).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_error(&body, "FORBIDDEN");
    }

    // 再建一台 PENDING 设备 P2：所有"错 PENDING key / 错 PoP / 错 grant"负例都在它上面。
    let p2 = make_pending(
        &v2,
        &app,
        [0xB5; 16],
        [0x72; 16],
        [0x32; 32],
        [0x42; 32],
        (v2.extra[0].0, [0xC1; 16]),
        [0x52; 16],
        [0x62; 32],
    )
    .await;

    // 错 PENDING key：证书里的 sign_pk 不是兑换时提交的那把（PoP 用新钥自签）。
    {
        let other = SigningKey::from_bytes(&[0x33; 32]);
        let other_pk = other.verifying_key().to_bytes();
        let (cert, grant) = credential_parts(
            &v2,
            p2.device_id,
            other_pk,
            p2.hpke_pk,
            1,
            proto::Rights::ALL,
            [0x92; 16],
            vec![0xB2; 48],
        );
        let operation = credential_operation(
            &v2,
            proto::ControlKindV2::DeviceApprove,
            [0xB6; 16],
            &cert,
            vec![grant],
            &admin_signers(&v2),
            Some((p2.device_id, other_pk)),
            v2.now_u64(),
        );
        let signed = sign_operation_keys(&v2, &operation, Some(&other));
        let (status, body) = submit(&app, &v2.vault_id(), &signed).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_error(&body, "FORBIDDEN");
    }
    // 错 PoP：signer.pk 是目标的公钥，但用别的私钥签。
    {
        let (cert, grant) = credential_parts(
            &v2,
            p2.device_id,
            p2.sign_pk,
            p2.hpke_pk,
            1,
            proto::Rights::ALL,
            [0x92; 16],
            vec![0xB2; 48],
        );
        let operation = credential_operation(
            &v2,
            proto::ControlKindV2::DeviceApprove,
            [0xB7; 16],
            &cert,
            vec![grant],
            &admin_signers(&v2),
            Some((p2.device_id, p2.sign_pk)),
            v2.now_u64(),
        );
        let wrong = v2.key(&v2.extra[1].0);
        let signed = sign_operation_keys(&v2, &operation, Some(wrong));
        let (status, body) = submit(&app, &v2.vault_id(), &signed).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_error(&body, "SIGNATURE_INVALID");
    }
    // 缺 PoP（首次准入必须带 role3）→ 403。
    {
        let (cert, grant) = credential_parts(
            &v2,
            p2.device_id,
            p2.sign_pk,
            p2.hpke_pk,
            1,
            proto::Rights::ALL,
            [0x92; 16],
            vec![0xB2; 48],
        );
        let operation = credential_operation(
            &v2,
            proto::ControlKindV2::DeviceApprove,
            [0xB8; 16],
            &cert,
            vec![grant],
            &admin_signers(&v2),
            None,
            v2.now_u64(),
        );
        let signed = sign_operation_keys(&v2, &operation, None);
        let (status, body) = submit(&app, &v2.vault_id(), &signed).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_error(&body, "FORBIDDEN");
    }
    // PoP 的 device_id 不是目标设备 → 403。
    {
        let (cert, grant) = credential_parts(
            &v2,
            p2.device_id,
            p2.sign_pk,
            p2.hpke_pk,
            1,
            proto::Rights::ALL,
            [0x92; 16],
            vec![0xB2; 48],
        );
        let operation = credential_operation(
            &v2,
            proto::ControlKindV2::DeviceApprove,
            [0xB9; 16],
            &cert,
            vec![grant],
            &admin_signers(&v2),
            Some((v2.extra[1].0, p2.sign_pk)),
            v2.now_u64(),
        );
        let signed = sign_operation_keys(&v2, &operation, Some(&p2.key));
        let (status, body) = submit(&app, &v2.vault_id(), &signed).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_error(&body, "FORBIDDEN");
    }
    // 换包：先按合法包签名，再把 payload 换成另一份包（签名不再覆盖）→ 400。
    {
        let (cert, grant) = credential_parts(
            &v2,
            p2.device_id,
            p2.sign_pk,
            p2.hpke_pk,
            1,
            proto::Rights::ALL,
            [0x92; 16],
            vec![0xB2; 48],
        );
        let operation = credential_operation(
            &v2,
            proto::ControlKindV2::DeviceApprove,
            [0xBA; 16],
            &cert,
            vec![grant.clone()],
            &admin_signers(&v2),
            Some((p2.device_id, p2.sign_pk)),
            v2.now_u64(),
        );
        let mut signed = sign_operation_keys(&v2, &operation, Some(&p2.key));
        let mut swapped = grant;
        swapped.ciphertext = vec![0xFF; 48];
        swapped.enc = [0xEE; 32];
        signed.operation.payload = proto::DeviceCredentialPayloadV2 {
            cert,
            grants: vec![swapped],
        }
        .encode()
        .expect("swapped payload");
        let (status, body) = submit(&app, &v2.vault_id(), &signed).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_error(&body, "SIGNATURE_INVALID");
    }
    // 错 recipient（grant 写给别的设备）→ 403。
    {
        let (cert, mut grant) = credential_parts(
            &v2,
            p2.device_id,
            p2.sign_pk,
            p2.hpke_pk,
            1,
            proto::Rights::ALL,
            [0x92; 16],
            vec![0xB2; 48],
        );
        grant.aad.recipient_device_id = v2.extra[0].0;
        let operation = credential_operation(
            &v2,
            proto::ControlKindV2::DeviceApprove,
            [0xBB; 16],
            &cert,
            vec![grant],
            &admin_signers(&v2),
            Some((p2.device_id, p2.sign_pk)),
            v2.now_u64(),
        );
        let signed = sign_operation_keys(&v2, &operation, Some(&p2.key));
        let (status, body) = submit(&app, &v2.vault_id(), &signed).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_error(&body, "FORBIDDEN");
    }
    // 错 grant 的 issued rev / 错 epoch / 错 authority → 403。
    for (label, mutate) in [
        (
            "issued rev",
            (|grant: &mut proto::GrantPackageV2| grant.aad.issued_authz_rev = 9)
                as fn(&mut proto::GrantPackageV2),
        ),
        ("epoch", |grant: &mut proto::GrantPackageV2| {
            grant.aad.epoch += 1
        }),
        ("authority", |grant: &mut proto::GrantPackageV2| {
            grant.aad.authority_rev += 1
        }),
    ] {
        let (cert, mut grant) = credential_parts(
            &v2,
            p2.device_id,
            p2.sign_pk,
            p2.hpke_pk,
            1,
            proto::Rights::ALL,
            [0x92; 16],
            vec![0xB2; 48],
        );
        mutate(&mut grant);
        let operation = credential_operation(
            &v2,
            proto::ControlKindV2::DeviceApprove,
            [0xBC; 16],
            &cert,
            vec![grant],
            &admin_signers(&v2),
            Some((p2.device_id, p2.sign_pk)),
            v2.now_u64(),
        );
        let signed = sign_operation_keys(&v2, &operation, Some(&p2.key));
        let (status, body) = submit(&app, &v2.vault_id(), &signed).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "{label}: {}",
            error_code(&body)
        );
        assert_error(&body, "FORBIDDEN");
    }
    // serial 跳号（PENDING 的首份凭证必须是 serial=1）→ 403。
    {
        let (cert, grant) = credential_parts(
            &v2,
            p2.device_id,
            p2.sign_pk,
            p2.hpke_pk,
            2,
            proto::Rights::ALL,
            [0x92; 16],
            vec![0xB2; 48],
        );
        let operation = credential_operation(
            &v2,
            proto::ControlKindV2::DeviceApprove,
            [0xBD; 16],
            &cert,
            vec![grant],
            &admin_signers(&v2),
            Some((p2.device_id, p2.sign_pk)),
            v2.now_u64(),
        );
        let signed = sign_operation_keys(&v2, &operation, Some(&p2.key));
        let (status, body) = submit(&app, &v2.vault_id(), &signed).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_error(&body, "FORBIDDEN");
    }
    {
        let (mut cert, grant) = credential_parts(
            &v2,
            p2.device_id,
            p2.sign_pk,
            p2.hpke_pk,
            2,
            proto::Rights::ALL,
            [0x92; 16],
            vec![0xB2; 48],
        );
        cert.issued_authz_rev = 1;
        let operation = credential_operation(
            &v2,
            proto::ControlKindV2::DeviceApprove,
            [0xBE; 16],
            &cert,
            vec![grant],
            &admin_signers(&v2),
            Some((p2.device_id, p2.sign_pk)),
            v2.now_u64(),
        );
        let signed = sign_operation_keys(&v2, &operation, Some(&p2.key));
        let (status, body) = submit(&app, &v2.vault_id(), &signed).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_error(&body, "FORBIDDEN");
    }
    // 未知设备 → 404。
    {
        let (cert, grant) = credential_parts(
            &v2,
            [0x7F; 16],
            p2.sign_pk,
            p2.hpke_pk,
            1,
            proto::Rights::ALL,
            [0x92; 16],
            vec![0xB2; 48],
        );
        let operation = credential_operation(
            &v2,
            proto::ControlKindV2::DeviceApprove,
            [0xBF; 16],
            &cert,
            vec![grant],
            &admin_signers(&v2),
            Some(([0x7F; 16], p2.sign_pk)),
            v2.now_u64(),
        );
        let signed = sign_operation_keys(&v2, &operation, Some(&p2.key));
        let (status, body) = submit(&app, &v2.vault_id(), &signed).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_error(&body, "NOT_FOUND");
    }
    // 陈旧 expected_authz_rev（CAS）→ 409。
    {
        let (cert, grant) = credential_parts(
            &v2,
            p2.device_id,
            p2.sign_pk,
            p2.hpke_pk,
            1,
            proto::Rights::ALL,
            [0x92; 16],
            vec![0xB2; 48],
        );
        let mut operation = credential_operation(
            &v2,
            proto::ControlKindV2::DeviceApprove,
            [0xC0; 16],
            &cert,
            vec![grant],
            &admin_signers(&v2),
            Some((p2.device_id, p2.sign_pk)),
            v2.now_u64(),
        );
        operation.expected_authz_rev = 1;
        let signed = sign_operation_keys(&v2, &operation, Some(&p2.key));
        let (status, body) = submit(&app, &v2.vault_id(), &signed).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_error(&body, "CAS_CONFLICT");
    }
    // grant_id 已在用（P1 的 0x91）→ 409 ID_CONFLICT。
    {
        let (cert, grant) = credential_parts(
            &v2,
            p2.device_id,
            p2.sign_pk,
            p2.hpke_pk,
            1,
            proto::Rights::ALL,
            [0x91; 16],
            vec![0xB3; 48],
        );
        let operation = credential_operation(
            &v2,
            proto::ControlKindV2::DeviceApprove,
            [0xC1; 16],
            &cert,
            vec![grant],
            &admin_signers(&v2),
            Some((p2.device_id, p2.sign_pk)),
            v2.now_u64(),
        );
        let signed = sign_operation_keys(&v2, &operation, Some(&p2.key));
        let (status, body) = submit(&app, &v2.vault_id(), &signed).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_error(&body, "ID_CONFLICT");
    }
    // 被撤设备不能复活：先批准 P2，再撤销，然后再次批准 → 403。
    {
        approve_default(
            &v2,
            &app,
            [0xC6; 16],
            &p2,
            proto::Rights::ALL,
            [0x93; 16],
            vec![0xB4; 48],
        )
        .await;
        let operation = revoke_operation(
            &v2,
            [0xC7; 16],
            vec![p2.device_id],
            &admin_signers(&v2),
            v2.now_u64(),
        );
        let signed = v2.sign_operation(&operation);
        let receipt = submit_ok(&app, &v2, &signed).await;
        assert_eq!(receipt.kind, 5);
        assert_eq!(
            device_state(&v2, &p2.device_id).expect("p2").status,
            "revoked"
        );
        let (cert, grant) = credential_parts(
            &v2,
            p2.device_id,
            p2.sign_pk,
            p2.hpke_pk,
            1,
            proto::Rights::ALL,
            [0x93; 16],
            vec![0xB4; 48],
        );
        let operation = credential_operation(
            &v2,
            proto::ControlKindV2::DeviceApprove,
            [0xC3; 16],
            &cert,
            vec![grant],
            &admin_signers(&v2),
            Some((p2.device_id, p2.sign_pk)),
            v2.now_u64(),
        );
        let signed = sign_operation_keys(&v2, &operation, Some(&p2.key));
        let (status, body) = submit(&app, &v2.vault_id(), &signed).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_error(&body, "FORBIDDEN");
    }

    // 失败不得推进控制链：成功 = 批准 P1 + 幂等重放 + 批准 P2 + 撤销 P2 = 4（另 1 条是 P2 的邀请）。
    assert_eq!(control_count(&v2), count_before + 5);
    assert_chain_continuous(&v2);
}

// ---------------------------------------------------------------- 2. DeviceRevoke

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn device_revoke_cascades_caps_tokens_ws_and_keeps_last_quorum() {
    let v2 = new_fixture("s4b2-revoke");
    let state = relay::api::AppState::new(
        v2.fx.db.inner().clone(),
        v2.fx.cfg.clone(),
        std::sync::Arc::new(relay::verify::Ed25519Hook),
    );
    let app: Router = relay::api::router(state.clone());
    let (status, body) = upgrade_default(&v2, &app, [0xD0; 16]).await;
    assert_eq!(status, StatusCode::OK, "upgrade: {}", error_code(&body));

    // 让 genesis 设备持一张 ALL cap，撤掉 extra0（C1）后高危 quorum 仍 ≥2。
    let genesis_cap = [0xE1u8; 16];
    {
        let vault = v2.vault();
        let (head_rev, _) = v2.head();
        let cap = proto::ManagementCapV2 {
            version: proto::V2_WIRE_VERSION,
            instance_id: vault.instance_id,
            vault_id: vault.vault_id,
            cap_id: genesis_cap,
            subject_device_id: v2.genesis_device(),
            subject_ed_pk: v2.sign_pk(&v2.genesis_device()),
            scopes: proto::CapScope::new(proto::CapScope::ALL).expect("scopes"),
            not_before: v2.now_u64(),
            expires_at: v2.now_u64() + 86_400,
            issued_control_rev: head_rev + 1,
            issued_authz_rev: vault.authz_rev + 1,
            issuer_authority_rev: vault.authority_rev,
            delegable: false,
        };
        let operation = v2.two_signer_operation(
            proto::ControlKindV2::CapGrant,
            [0xD1; 16],
            proto::CapGrantPayloadV2 { cap }
                .encode()
                .expect("cap payload"),
            &admin_signers(&v2),
            v2.now_u64(),
        );
        let signed = v2.sign_operation(&operation);
        let receipt = submit_ok(&app, &v2, &signed).await;
        assert_eq!(receipt.kind, 2);
    }
    // 一台可被撤销的 PENDING → ACTIVE 设备（P，READ|WRITE，无 cap）。
    let p = make_pending(
        &v2,
        &app,
        [0xD2; 16],
        [0x73; 16],
        [0x34; 32],
        [0x44; 32],
        (v2.extra[0].0, [0xC1; 16]),
        [0x53; 16],
        [0x63; 32],
    )
    .await;
    approve_default(
        &v2,
        &app,
        [0xD3; 16],
        &p,
        proto::Rights::ALL,
        [0x94; 16],
        vec![0xB5; 48],
    )
    .await;

    // 给 P 一张 INVITE-only cap，用它建一份**未兑换**邀请。
    let p_invite_cap = [0xE2u8; 16];
    {
        let vault = v2.vault();
        let (head_rev, _) = v2.head();
        let cap = proto::ManagementCapV2 {
            version: proto::V2_WIRE_VERSION,
            instance_id: vault.instance_id,
            vault_id: vault.vault_id,
            cap_id: p_invite_cap,
            subject_device_id: p.device_id,
            subject_ed_pk: p.sign_pk,
            scopes: proto::CapScope::new(proto::CapScope::INVITE).expect("scopes"),
            not_before: v2.now_u64(),
            expires_at: v2.now_u64() + 86_400,
            issued_control_rev: head_rev + 1,
            issued_authz_rev: vault.authz_rev + 1,
            issuer_authority_rev: vault.authority_rev,
            delegable: false,
        };
        let operation = v2.two_signer_operation(
            proto::ControlKindV2::CapGrant,
            [0xD4; 16],
            proto::CapGrantPayloadV2 { cap }
                .encode()
                .expect("cap payload"),
            &admin_signers(&v2),
            v2.now_u64(),
        );
        let signed = v2.sign_operation(&operation);
        submit_ok(&app, &v2, &signed).await;
    }
    // 错 scope：只持 INVITE 的 cap 不能当 REVOKE 的签名者 → 403（两位各具全部 scope）。
    {
        let operation = revoke_operation_with(
            &v2,
            [0xF1; 16],
            vec![v2.extra[1].0],
            &[
                (p.device_id, p_invite_cap, p.sign_pk),
                (v2.extra[1].0, [0xC2; 16], v2.sign_pk(&v2.extra[1].0)),
            ],
            v2.now_u64(),
        );
        // signers 按 (role, device, cap, pk) 升序：extra1(0x2B) 在前，P(0x73) 在后。
        let keys = [&v2.extra[1].1, &p.key];
        let signed = v2.sign_operation_with(&operation, &keys);
        let (status, body) = submit(&app, &v2.vault_id(), &signed).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{}", error_code(&body));
        assert_error(&body, "FORBIDDEN");
    }

    let dead_invite = [0x54u8; 16];
    let dead_token = [0x64u8; 32];
    {
        let operation = invite_operation_with(
            &v2,
            [0xD5; 16],
            dead_invite,
            &dead_token,
            v2.now_u64() + 600,
            p.device_id,
            p_invite_cap,
            p.sign_pk,
            v2.now_u64(),
        );
        let signed = sign_single_device(&operation, &p.key);
        submit_ok(&app, &v2, &signed).await;
    }

    // extra0 的 realm 3 bearer + 真实 WS；P 的设备 bearer。
    let extra0_token = authz_login(
        &app,
        &v2.vault_id(),
        v2.extra[0].0,
        &v2.extra[0].1,
        Some([0xC1; 16]),
    )
    .await
    .expect("extra0 token");

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
        format!("Bearer {extra0_token}").parse().expect("header"),
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
    assert_eq!(value["authz_rev"], 4, "帧里必须带授权 rev");

    // —— 正例：两签撤销 extra0 ——
    let vault_before = v2.vault();
    let count_before = control_count(&v2);
    let operation = revoke_operation(
        &v2,
        [0xD6; 16],
        vec![v2.extra[0].0],
        &admin_signers(&v2),
        v2.now_u64(),
    );
    let signed = v2.sign_operation(&operation);
    let receipt = submit_ok(&app, &v2, &signed).await;
    assert_eq!(receipt.kind, 5);
    assert_eq!(receipt.control_rev, vault_before.control_rev + 1);
    assert_eq!(receipt.authz_rev, vault_before.authz_rev + 1);
    assert_eq!(control_count(&v2), count_before + 1);
    let vault_after = v2.vault();
    assert_eq!(vault_after.authz_rev, vault_before.authz_rev + 1);
    assert_eq!(vault_after.auth_rev, vault_before.auth_rev + 1);

    // 设备下线 + 名下全部 cap 落 `revoked_control_rev`。
    let state_row = device_state(&v2, &v2.extra[0].0).expect("extra0");
    assert_eq!(state_row.status, "revoked");
    assert_eq!(
        cap_revoked_control_rev(&v2, &[0xC1; 16]),
        Some(i64::try_from(receipt.control_rev).expect("rev"))
    );
    // 其 realm 3 bearer 立即失效。
    let (status, body) = call(
        &app,
        get(
            &format!("{}/state", hex_path(&v2.vault_id())),
            Some(&extra0_token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_error(&body, "REVOKED");
    // WS 由 notify 立即触发重验并关闭。
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
    assert!(closed, "设备撤销后 WS 必须断开");

    // 被撤设备失去签名资格：用它的旧 cap（C1）再签一条控制记录 → 403。
    {
        let operation = revoke_operation(
            &v2,
            [0xD7; 16],
            vec![p.device_id],
            &[(v2.extra[0].0, [0xC1; 16]), (v2.extra[1].0, [0xC2; 16])],
            v2.now_u64(),
        );
        let signed = v2.sign_operation(&operation);
        let (status, body) = submit(&app, &v2.vault_id(), &signed).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_error(&body, "FORBIDDEN");
    }

    // —— 重复撤销：签名事实仍上链，但不推进 authz/auth（changed=false） ——
    {
        let authz_before = v2.vault().authz_rev;
        let auth_before = v2.vault().auth_rev;
        let operation = revoke_operation(
            &v2,
            [0xD8; 16],
            vec![v2.extra[0].0],
            &[
                (v2.genesis_device(), genesis_cap),
                (v2.extra[1].0, [0xC2; 16]),
            ],
            v2.now_u64(),
        );
        let signed = v2.sign_operation(&operation);
        let receipt = submit_ok(&app, &v2, &signed).await;
        assert_eq!(receipt.authz_rev, authz_before, "重复撤销不推进 authz");
        assert_eq!(v2.vault().auth_rev, auth_before, "重复撤销不推进 auth");
    }

    // 单签撤销 → 403（两签是硬门限，不因"目标已下线"放宽）。
    {
        let operation = revoke_operation(
            &v2,
            [0xF0; 16],
            vec![v2.extra[1].0],
            &[(v2.genesis_device(), genesis_cap)],
            v2.now_u64(),
        );
        let signed = v2.sign_operation(&operation);
        let (status, body) = submit(&app, &v2.vault_id(), &signed).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_error(&body, "FORBIDDEN");
    }

    // —— 撤销 P：INVITE 不是高危 scope，quorum 不受影响；其未兑换邀请失效 ——
    {
        let operation = revoke_operation(
            &v2,
            [0xD9; 16],
            vec![p.device_id],
            &[
                (v2.genesis_device(), genesis_cap),
                (v2.extra[1].0, [0xC2; 16]),
            ],
            v2.now_u64(),
        );
        let signed = v2.sign_operation(&operation);
        let receipt = submit_ok(&app, &v2, &signed).await;
        assert_eq!(receipt.kind, 5);
        assert_eq!(
            cap_revoked_control_rev(&v2, &p_invite_cap),
            Some(i64::try_from(receipt.control_rev).expect("rev"))
        );
    }
    // 未兑换邀请：签发 cap 随设备撤销而失效 → 兑换 403。
    {
        let key = SigningKey::from_bytes(&[0x35; 32]);
        let sign_pk = key.verifying_key().to_bytes();
        let proof = b"pop".to_vec();
        let signature = key.sign(&proof).to_bytes().to_vec();
        let (status, body) = call(
            &app,
            post_cbor(
                &format!(
                    "{}/invites/{}/redeem",
                    hex_path(&v2.vault_id()),
                    b64url_encode(&dead_invite)
                ),
                None,
                vec![
                    (0, wire::bytes(dead_token.to_vec())),
                    (1, wire::bytes([0x74u8; 16].to_vec())),
                    (2, wire::bytes(sign_pk.to_vec())),
                    (3, wire::bytes([0x45u8; 32].to_vec())),
                    (4, wire::bytes(proof)),
                    (5, wire::bytes(signature)),
                ],
            ),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{}", error_code(&body));
        assert_error(&body, "FORBIDDEN");
    }

    // —— 最后 quorum：撤 extra1（C2）会让 APPROVE/REVOKE/CAP_ADMIN 只剩 genesis ——
    {
        let operation = revoke_operation(
            &v2,
            [0xDA; 16],
            vec![v2.extra[1].0],
            &[
                (v2.genesis_device(), genesis_cap),
                (v2.extra[1].0, [0xC2; 16]),
            ],
            v2.now_u64(),
        );
        let signed = v2.sign_operation(&operation);
        let (status, body) = submit(&app, &v2.vault_id(), &signed).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{}", error_code(&body));
        assert_error(&body, "FORBIDDEN");
    }
    // 一把撤掉 genesis + extra1（剩 0 台 ACTIVE）→ 403。
    {
        let operation = revoke_operation(
            &v2,
            [0xDB; 16],
            vec![v2.genesis_device(), v2.extra[1].0],
            &[
                (v2.genesis_device(), genesis_cap),
                (v2.extra[1].0, [0xC2; 16]),
            ],
            v2.now_u64(),
        );
        let signed = v2.sign_operation(&operation);
        let (status, body) = submit(&app, &v2.vault_id(), &signed).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_error(&body, "FORBIDDEN");
    }
    // PENDING 目标：撤销未批准的设备 → 403。
    {
        let q = make_pending(
            &v2,
            &app,
            [0xDC; 16],
            [0x75; 16],
            [0x36; 32],
            [0x46; 32],
            (v2.extra[1].0, [0xC2; 16]),
            [0x55; 16],
            [0x65; 32],
        )
        .await;
        let operation = revoke_operation(
            &v2,
            [0xDD; 16],
            vec![q.device_id],
            &[
                (v2.genesis_device(), genesis_cap),
                (v2.extra[1].0, [0xC2; 16]),
            ],
            v2.now_u64(),
        );
        let signed = v2.sign_operation(&operation);
        let (status, body) = submit(&app, &v2.vault_id(), &signed).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_error(&body, "FORBIDDEN");
    }
    // 跨 vault：把 A 的撤销提交到 B 路由 → 403（tenant/instance 不匹配）。
    let second = tokio::task::block_in_place(|| v2.create_second_vault([0xB2u8; 16]));
    {
        let operation = revoke_operation(
            &v2,
            [0xDE; 16],
            vec![v2.extra[0].0],
            &[
                (v2.genesis_device(), genesis_cap),
                (v2.extra[1].0, [0xC2; 16]),
            ],
            v2.now_u64(),
        );
        let signed = v2.sign_operation(&operation);
        let (status, body) = submit(&app, &second.vault_id, &signed).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_error(&body, "FORBIDDEN");
    }
    // 目标只在 B 存在 → A 上 404（不泄露其它租户）。
    {
        let operation = revoke_operation(
            &v2,
            [0xDF; 16],
            vec![second.genesis_device],
            &[
                (v2.genesis_device(), genesis_cap),
                (v2.extra[1].0, [0xC2; 16]),
            ],
            v2.now_u64(),
        );
        let signed = v2.sign_operation(&operation);
        let (status, body) = submit(&app, &v2.vault_id(), &signed).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_error(&body, "NOT_FOUND");
    }
    // V2 启用后 v1 管理写关闭：有效设备 bearer 走旧 revoke 路由 → 403 MANAGEMENT_REQUIRED。
    {
        let extra1_token = authz_login(
            &app,
            &v2.vault_id(),
            v2.extra[1].0,
            &v2.extra[1].1,
            Some([0xC2; 16]),
        )
        .await
        .expect("extra1 token");
        let (status, body) = call(
            &app,
            post_cbor(
                &format!(
                    "{}/devices/{}/revoke",
                    hex_path(&v2.vault_id()),
                    b64url_encode(&v2.extra[1].0)
                ),
                Some(&extra1_token),
                vec![(0, wire::uint(1))],
            ),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{}", error_code(&body));
        assert_error(&body, "MANAGEMENT_REQUIRED");
    }

    assert!(
        cap_revoked_control_rev(&v2, &[0xC2; 16]).is_none(),
        "C2 仍有效"
    );
    assert_chain_continuous(&v2);
}

// ---------------------------------------------------------------- 3. DeviceRightsChange

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rights_change_increments_serial_keeps_history_and_writable_quorum() {
    let v2 = new_fixture("s4b2-rights");
    let app = app(&v2);
    let (status, body) = upgrade_default(&v2, &app, [0xE0; 16]).await;
    assert_eq!(status, StatusCode::OK, "upgrade: {}", error_code(&body));

    // 只读设备 P：serial=1、READ。
    let p = make_pending(
        &v2,
        &app,
        [0xE1; 16],
        [0x76; 16],
        [0x37; 32],
        [0x47; 32],
        (v2.extra[0].0, [0xC1; 16]),
        [0x56; 16],
        [0x66; 32],
    )
    .await;
    let (approve_receipt, _, _) = approve_default(
        &v2,
        &app,
        [0xE2; 16],
        &p,
        proto::Rights::READ,
        [0x95; 16],
        vec![0xB6; 48],
    )
    .await;
    assert_eq!(approve_receipt.kind, 4);
    let old_cert = device_state(&v2, &p.device_id).expect("p").certificate;
    assert_eq!(credentials(&v2, &p.device_id).len(), 1);

    // 正例：READ → READ|WRITE（serial 2）。
    let (cert, grant) = credential_parts(
        &v2,
        p.device_id,
        p.sign_pk,
        p.hpke_pk,
        2,
        proto::Rights::ALL,
        [0x96; 16],
        vec![0xC1; 48],
    );
    let operation = credential_operation(
        &v2,
        proto::ControlKindV2::DeviceRightsChange,
        [0xE3; 16],
        &cert,
        vec![grant],
        &admin_signers(&v2),
        None,
        v2.now_u64(),
    );
    let signed = sign_operation_keys(&v2, &operation, None);
    let vault_before = v2.vault();
    let receipt = submit_ok(&app, &v2, &signed).await;
    assert_eq!(receipt.kind, 16);
    assert_eq!(receipt.control_rev, vault_before.control_rev + 1);
    assert_eq!(receipt.authz_rev, vault_before.authz_rev + 1);
    let vault_after = v2.vault();
    assert_eq!(vault_after.authz_rev, vault_before.authz_rev + 1);
    assert_eq!(vault_after.auth_rev, vault_before.auth_rev + 1);

    // 新证书 + serial 递增 + 旧证书历史保留。
    let state_row = device_state(&v2, &p.device_id).expect("p after rights");
    assert_eq!(state_row.status, "active");
    assert_eq!(
        state_row.approval_control_rev,
        Some(i64::try_from(receipt.control_rev).expect("rev"))
    );
    let new_cert = proto::DeviceCertV2::decode(&state_row.certificate).expect("new cert");
    assert_eq!(new_cert.cert_serial, 2);
    assert_eq!(
        new_cert.data_rights,
        proto::Rights::new(proto::Rights::ALL).expect("rights")
    );
    assert_ne!(state_row.certificate, old_cert, "设备行必须换成新证书");
    let history = credentials(&v2, &p.device_id);
    assert_eq!(history.len(), 2, "旧证书历史不覆盖");
    assert_eq!(history[0].0, 1);
    assert_eq!(
        proto::DeviceCertV2::decode(&history[0].1)
            .expect("old cert")
            .data_rights,
        proto::Rights::new(proto::Rights::READ).expect("rights"),
        "旧证书仍可验，且保留原权限"
    );
    assert_eq!(history[1].0, 2);
    assert_eq!(history[1].1, state_row.certificate);

    // 数据 grant 不被误撤：本 epoch 的 grant 行仍在，且换成新包。
    let grant_row = grant_row(&v2, vault_after.active_epoch, &p.device_id).expect("grant row");
    assert_eq!(grant_row.3.as_deref(), Some([0x96u8; 16].as_slice()));
    assert_eq!(grant_row.1, vec![0xC1u8; 48]);

    // 新 bearer 可用，且 `latest grant` 能取到改权后的包。
    let token = authz_login(&app, &v2.vault_id(), p.device_id, &p.key, None)
        .await
        .expect("rights token");
    let (status, body) = call(
        &app,
        get(
            &format!("{}/devices/self/grants/latest", hex_path(&v2.vault_id())),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "latest grant: {}",
        error_code(&body)
    );
    let fields = cbor_fields(&body);
    assert_eq!(
        wire::bytes_field(&fields, 4).expect("ciphertext"),
        vec![0xC1u8; 48]
    );

    // 允许的降权（可写设备仍 ≥2）：extra0 ALL → READ（serial 2）。
    let extra0_certificate = device_state(&v2, &v2.extra[0].0)
        .expect("extra0")
        .certificate;
    assert!(
        proto::DeviceCertV1::decode(&extra0_certificate)
            .expect("v1 cert")
            .rights
            .can_write(),
        "extra0 初始可写"
    );
    let (cert, grant) = credential_parts(
        &v2,
        v2.extra[0].0,
        v2.sign_pk(&v2.extra[0].0),
        extra0_hpke_pk(&v2),
        2,
        proto::Rights::READ,
        [0x97; 16],
        vec![0xC2; 48],
    );
    let operation = credential_operation(
        &v2,
        proto::ControlKindV2::DeviceRightsChange,
        [0xE4; 16],
        &cert,
        vec![grant],
        &admin_signers(&v2),
        None,
        v2.now_u64(),
    );
    let signed = sign_operation_keys(&v2, &operation, None);
    submit_ok(&app, &v2, &signed).await;

    // 允许的第二次降权（可写设备 4→3）：extra1 ALL → READ。
    {
        let extra1 = v2.extra[1].0;
        let (cert, grant) = credential_parts(
            &v2,
            extra1,
            v2.sign_pk(&extra1),
            extra1_hpke_pk(&v2),
            2,
            proto::Rights::READ,
            [0x98; 16],
            vec![0xC3; 48],
        );
        let operation = credential_operation(
            &v2,
            proto::ControlKindV2::DeviceRightsChange,
            [0xE5; 16],
            &cert,
            vec![grant],
            &admin_signers(&v2),
            None,
            v2.now_u64(),
        );
        let signed = sign_operation_keys(&v2, &operation, None);
        submit_ok(&app, &v2, &signed).await;
    }
    // 负例：再降 genesis 会让可写设备只剩 1 台（P）→ 403。
    {
        let genesis_certificate = device_state(&v2, &v2.genesis_device())
            .expect("genesis")
            .certificate;
        let genesis_hpke: [u8; 32] = device_state(&v2, &v2.genesis_device())
            .expect("genesis")
            .hpke_pk
            .as_slice()
            .try_into()
            .expect("hpke pk");
        assert!(proto::DeviceCertV1::decode(&genesis_certificate)
            .expect("v1 cert")
            .rights
            .can_write());
        let (cert, grant) = credential_parts(
            &v2,
            v2.genesis_device(),
            v2.sign_pk(&v2.genesis_device()),
            genesis_hpke,
            2,
            proto::Rights::READ,
            [0x9E; 16],
            vec![0xC9; 48],
        );
        let operation = credential_operation(
            &v2,
            proto::ControlKindV2::DeviceRightsChange,
            [0xEC; 16],
            &cert,
            vec![grant],
            &admin_signers(&v2),
            None,
            v2.now_u64(),
        );
        let signed = sign_operation_keys(&v2, &operation, None);
        let (status, body) = submit(&app, &v2.vault_id(), &signed).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "最后可写 quorum: {}",
            error_code(&body)
        );
        assert_error(&body, "FORBIDDEN");
    }
    // 错 serial（应为 3，提交 4）→ 403。
    {
        let (cert, grant) = credential_parts(
            &v2,
            p.device_id,
            p.sign_pk,
            p.hpke_pk,
            4,
            proto::Rights::READ,
            [0x99; 16],
            vec![0xC4; 48],
        );
        let operation = credential_operation(
            &v2,
            proto::ControlKindV2::DeviceRightsChange,
            [0xE6; 16],
            &cert,
            vec![grant],
            &admin_signers(&v2),
            None,
            v2.now_u64(),
        );
        let signed = sign_operation_keys(&v2, &operation, None);
        let (status, body) = submit(&app, &v2.vault_id(), &signed).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_error(&body, "FORBIDDEN");
    }
    // 单签 → 403。
    {
        let (cert, grant) = credential_parts(
            &v2,
            p.device_id,
            p.sign_pk,
            p.hpke_pk,
            3,
            proto::Rights::READ,
            [0x9A; 16],
            vec![0xC5; 48],
        );
        let operation = credential_operation(
            &v2,
            proto::ControlKindV2::DeviceRightsChange,
            [0xE7; 16],
            &cert,
            vec![grant],
            &[admin_signers(&v2)[0]],
            None,
            v2.now_u64(),
        );
        let signed = sign_operation_keys(&v2, &operation, None);
        let (status, body) = submit(&app, &v2.vault_id(), &signed).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_error(&body, "FORBIDDEN");
    }
    // 权限没变化（ALL → ALL，serial 仍递增）→ 409 INVALID_STATE。
    {
        let (cert, grant) = credential_parts(
            &v2,
            p.device_id,
            p.sign_pk,
            p.hpke_pk,
            3,
            proto::Rights::ALL,
            [0x9B; 16],
            vec![0xC6; 48],
        );
        let operation = credential_operation(
            &v2,
            proto::ControlKindV2::DeviceRightsChange,
            [0xE8; 16],
            &cert,
            vec![grant],
            &admin_signers(&v2),
            None,
            v2.now_u64(),
        );
        let signed = sign_operation_keys(&v2, &operation, None);
        let (status, body) = submit(&app, &v2.vault_id(), &signed).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_error(&body, "INVALID_STATE");
    }
    // 换钥（hpke 公钥变了）→ 403。
    {
        let (cert, grant) = credential_parts(
            &v2,
            p.device_id,
            p.sign_pk,
            [0x48; 32],
            3,
            proto::Rights::READ,
            [0x9C; 16],
            vec![0xC7; 48],
        );
        let operation = credential_operation(
            &v2,
            proto::ControlKindV2::DeviceRightsChange,
            [0xE9; 16],
            &cert,
            vec![grant],
            &admin_signers(&v2),
            None,
            v2.now_u64(),
        );
        let signed = sign_operation_keys(&v2, &operation, None);
        let (status, body) = submit(&app, &v2.vault_id(), &signed).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_error(&body, "FORBIDDEN");
    }
    // 撤销 P 之后再改权 → 403（不得复活）。
    {
        let operation = revoke_operation(
            &v2,
            [0xEA; 16],
            vec![p.device_id],
            &admin_signers(&v2),
            v2.now_u64(),
        );
        let signed = v2.sign_operation(&operation);
        submit_ok(&app, &v2, &signed).await;
        assert_eq!(
            device_state(&v2, &p.device_id).expect("p").status,
            "revoked"
        );
        let (cert, grant) = credential_parts(
            &v2,
            p.device_id,
            p.sign_pk,
            p.hpke_pk,
            3,
            proto::Rights::READ,
            [0x9D; 16],
            vec![0xC8; 48],
        );
        let operation = credential_operation(
            &v2,
            proto::ControlKindV2::DeviceRightsChange,
            [0xEB; 16],
            &cert,
            vec![grant],
            &admin_signers(&v2),
            None,
            v2.now_u64(),
        );
        let signed = sign_operation_keys(&v2, &operation, None);
        let (status, body) = submit(&app, &v2.vault_id(), &signed).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_error(&body, "FORBIDDEN");
    }

    assert_chain_continuous(&v2);
}

/// `extra[0]` 的 hpke 公钥（夹具里是 `seed[0] ^ 0x5A` 的重复字节）。
fn extra0_hpke_pk(_v2: &V2Fixture) -> [u8; 32] {
    [EXTRA_DEVICES[0].1[0] ^ 0x5A; 32]
}

/// `extra[1]` 的 hpke 公钥。
fn extra1_hpke_pk(_v2: &V2Fixture) -> [u8; 32] {
    [EXTRA_DEVICES[1].1[0] ^ 0x5A; 32]
}
