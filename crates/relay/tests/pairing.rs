//! 配对 / 设备批准 / key-slot / 轮换新槽 / genesis 域校验（ADR 0006 §4/§5/§8/§17）。

mod support;

use support::*;

use relay::auth;
use relay::db::{Db, DbConfig};
use relay::envelope::ObjectType;
use relay::error::{Error, ErrorCode};
use relay::ids::{random_id16, sha256};
use relay::pairing;
use relay::proto;
use relay::rotations::RotationState;

use relay::slots;
use relay::time::FakeClock;
use relay::vault;
use relay::verify::Ed25519Hook;

fn code(err: &Error) -> Option<ErrorCode> {
    err.code()
}

fn new_device(seed: u8) -> pairing::NewDevice {
    pairing::NewDevice {
        device_id: [seed; 16],
        sign_pk: [seed.wrapping_add(0x10); 32],
        hpke_pk: [seed.wrapping_add(0x20); 32],
        proof: b"invite-proof".to_vec(),
        proof_signature: b"invite-proof-sig".to_vec(),
    }
}

fn create_invite(fx: &Fixture, token: &[u8; 32]) -> relay::ids::Id16 {
    let ctx = fx.manager_ctx();
    let hash = sha256(token);
    let db = fx.db.clone();
    let clock = fx.clock.clone();
    let hook = fx.hook.clone();
    db.write_blocking(move |conn| {
        pairing::create_invite(
            conn,
            clock.as_ref(),
            hook.as_ref(),
            &ctx,
            &pairing::InviteRequest {
                token_hash: hash,
                requested_expires_at: None,
                expected_control_rev: ctx.auth_rev,
                request_id: [0x71u8; 16],
                path: r#"/v1/vaults/1/invites"#.to_string(),
                unsigned_body: b"invite-body".to_vec(),
                signature: b"invite-sig".to_vec(),
            },
        )
    })
    .expect("create invite")
    .invite_id
}

#[test]
fn invite_is_single_use_expires_and_concurrent_redeem_has_one_winner() {
    let fx = Fixture::new("pair-invite");
    let token = [0x11u8; 32];
    let invite_id = create_invite(&fx, &token);

    // 正常兑换 → PENDING。
    let device = new_device(0x31);
    let outcome = {
        let db = fx.db.clone();
        let clock = fx.clock.clone();
        let hook = fx.hook.clone();
        db.write_blocking(move |conn| {
            pairing::redeem_invite(
                conn,
                clock.as_ref(),
                hook.as_ref(),
                &invite_id,
                &token,
                &device,
            )
        })
    }
    .expect("redeem");
    assert_eq!(outcome.pending_device, [0x31u8; 16]);
    assert_eq!(outcome.fingerprint.len(), 32, "设备指纹是 32 hex");
    let pending = fx
        .db
        .read(pairing::pending_devices)
        .expect("pending devices");
    assert_eq!(pending, vec![[0x31u8; 16]]);

    // 重复消费 → INVITE_USED（同一个 T 不能创第二台设备）。
    let again = new_device(0x32);
    let err = {
        let db = fx.db.clone();
        let clock = fx.clock.clone();
        let hook = fx.hook.clone();
        db.write_blocking(move |conn| {
            pairing::redeem_invite(
                conn,
                clock.as_ref(),
                hook.as_ref(),
                &invite_id,
                &token,
                &again,
            )
        })
    }
    .expect_err("double redeem");
    assert_eq!(code(&err), Some(ErrorCode::InviteUsed));

    // 错 token → FORBIDDEN（不泄露邀请是否存在）。
    let other_invite = create_invite(&fx, &[0x22u8; 32]);
    let wrong = new_device(0x33);
    let err = {
        let db = fx.db.clone();
        let clock = fx.clock.clone();
        let hook = fx.hook.clone();
        db.write_blocking(move |conn| {
            pairing::redeem_invite(
                conn,
                clock.as_ref(),
                hook.as_ref(),
                &other_invite,
                &[0x99u8; 32],
                &wrong,
            )
        })
    }
    .expect_err("wrong token");
    assert_eq!(code(&err), Some(ErrorCode::Forbidden));

    // 过期 → EXPIRED（FakeClock 推进 24h+1s，不 sleep）。
    fx.clock.advance_secs(24 * 60 * 60 + 1);
    let later = new_device(0x34);
    let err = {
        let db = fx.db.clone();
        let clock = fx.clock.clone();
        let hook = fx.hook.clone();
        db.write_blocking(move |conn| {
            pairing::redeem_invite(
                conn,
                clock.as_ref(),
                hook.as_ref(),
                &other_invite,
                &[0x22u8; 32],
                &later,
            )
        })
    }
    .expect_err("expired");
    assert_eq!(code(&err), Some(ErrorCode::Expired));

    // 并发兑换同一邀请：恰好一个成功。
    let invite_id = create_invite(&fx, &[0x44u8; 32]);
    let results = std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for seed in [0x51u8, 0x52, 0x53] {
            let db = fx.db.clone();
            let clock = fx.clock.clone();
            let hook = fx.hook.clone();
            handles.push(scope.spawn(move || {
                let device = new_device(seed);
                db.write_blocking(move |conn| {
                    pairing::redeem_invite(
                        conn,
                        clock.as_ref(),
                        hook.as_ref(),
                        &invite_id,
                        &[0x44u8; 32],
                        &device,
                    )
                })
            }));
        }
        handles
            .into_iter()
            .map(|handle| handle.join().expect("thread"))
            .collect::<Vec<_>>()
    });
    let winners = results.iter().filter(|result| result.is_ok()).count();
    assert_eq!(winners, 1, "并发兑换只能有一个赢家: {results:?}");
    let losers = results
        .iter()
        .filter_map(|result| result.as_ref().err())
        .filter(|err| code(err) == Some(ErrorCode::InviteUsed))
        .count();
    assert_eq!(losers, 2);
}

#[test]
fn approve_device_binds_certificate_and_grants() {
    let fx = Fixture::new("pair-approve");
    let token = [0x61u8; 32];
    let invite_id = create_invite(&fx, &token);
    let device = new_device(0x62);
    let device_id = device.device_id;
    let sign_pk = device.sign_pk;
    let hpke_pk = device.hpke_pk;
    {
        let db = fx.db.clone();
        let clock = fx.clock.clone();
        let hook = fx.hook.clone();
        db.write_blocking(move |conn| {
            pairing::redeem_invite(
                conn,
                clock.as_ref(),
                hook.as_ref(),
                &invite_id,
                &token,
                &device,
            )
        })
        .expect("redeem");
    }

    let vault = fx.vault();
    let certificate = certificate_bytes(&vault.vault_id, &device_id, &sign_pk, &hpke_pk, 1);
    let grant_id = random_id16().expect("grant id");
    let ctx = fx.manager_ctx();
    let db = fx.db.clone();
    let clock = fx.clock.clone();
    let hook = fx.hook.clone();
    let outcome = db
        .write_blocking(move |conn| {
            pairing::approve_device(
                conn,
                clock.as_ref(),
                hook.as_ref(),
                &ctx,
                &device_id,
                &pairing::ApproveRequest {
                    certificate,
                    certificate_signature: b"cert-sig".to_vec(),
                    grant_id,
                    hpke_enc: [0x71u8; 32],
                    hpke_ciphertext: b"hpke-ciphertext".to_vec(),
                    grant_signature: b"grant-sig".to_vec(),
                    expected_control_rev: vault.control_rev,
                    request_id: [0x72u8; 16],
                    path: "/v1/vaults/1/devices/2/approve".to_string(),
                    unsigned_body: b"approve-body".to_vec(),
                    management_signature: b"mgmt-sig".to_vec(),
                },
            )
        })
        .expect("approve");
    assert_eq!(outcome.device_id, device_id);
    assert_eq!(outcome.fingerprint.len(), 32);
    assert_eq!(outcome.epoch, 1);
    assert_eq!(
        outcome.certificate_signature, b"cert-sig",
        "approve 响应带证书签名"
    );

    let status: String = fx
        .db
        .read(move |conn| {
            Ok(conn.query_row(
                "SELECT status FROM devices WHERE device_id = ?1",
                [device_id.as_slice()],
                |row| row.get(0),
            )?)
        })
        .expect("status");
    assert_eq!(status, "active");
    let grants: i64 = fx
        .db
        .read(move |conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM device_grants WHERE device_id = ?1 AND epoch = 1",
                [device_id.as_slice()],
                |row| row.get(0),
            )?)
        })
        .expect("grants");
    assert_eq!(grants, 1, "批准时写入当前 epoch 的 grant");
    // §17.1 证书签名必须持久化（控制链后续要给别的设备验签）。
    let stored: Vec<u8> = fx
        .db
        .read(move |conn| {
            Ok(conn.query_row(
                "SELECT certificate_signature FROM devices WHERE device_id = ?1",
                [device_id.as_slice()],
                |row| row.get(0),
            )?)
        })
        .expect("certificate signature");
    assert_eq!(stored, b"cert-sig");
    assert!(
        fx.vault().control_rev > vault.control_rev,
        "设备证书进控制链"
    );

    // 证书绑错设备 → 400（防 approve 换钥匙/换设备）。
    let other_cert = certificate_bytes(&vault.vault_id, &[0x63u8; 16], &sign_pk, &hpke_pk, 1);
    let fx_control_rev = fx.vault().control_rev;
    let ctx = fx.manager_ctx();
    let db = fx.db.clone();
    let clock = fx.clock.clone();
    let hook = fx.hook.clone();
    let err = db
        .write_blocking(move |conn| {
            pairing::approve_device(
                conn,
                clock.as_ref(),
                hook.as_ref(),
                &ctx,
                &device_id,
                &pairing::ApproveRequest {
                    certificate: other_cert,
                    certificate_signature: b"sig".to_vec(),
                    grant_id: [0x64u8; 16],
                    hpke_enc: [0x71u8; 32],
                    hpke_ciphertext: b"ct".to_vec(),
                    grant_signature: b"sig".to_vec(),
                    expected_control_rev: fx_control_rev,
                    request_id: [0x65u8; 16],
                    path: "/approve".to_string(),
                    unsigned_body: b"body".to_vec(),
                    management_signature: b"sig".to_vec(),
                },
            )
        })
        .expect_err("cert mismatch");
    assert_eq!(code(&err), Some(ErrorCode::InvalidRequest));
}

#[test]
fn key_slot_cas_activation_removes_old_password_slot() {
    let fx = Fixture::new("pair-slots");
    let vault = fx.vault();
    let mut staged = Vec::new();
    for round in 0..2u8 {
        let slot_id = random_object_id();
        let descriptor = support::slot_descriptor(
            slot_id,
            vault.vault_id,
            1,
            slots::SlotKind::Password,
            BASE_TIME as u64 + u64::from(round),
        );
        let envelope = support::key_slot_envelope(vault.vault_id, slot_id, 1, &descriptor, 64);
        fx.put_part(&envelope, None).expect("slot part");
        fx.complete_object(&slot_id, &envelope)
            .expect("slot complete");
        let ctx = fx.device_ctx();
        let db = fx.db.clone();
        let clock = fx.clock.clone();
        let hook = fx.hook.clone();
        let row = db
            .write_blocking(move |conn| {
                slots::put(
                    conn,
                    clock.as_ref(),
                    hook.as_ref(),
                    &ctx,
                    &slots::SlotPutRequest {
                        slot_id,
                        epoch: 1,
                        kind: slots::SlotKind::Password,
                        descriptor: descriptor.clone(),
                        signed_message: b"slot-proof".to_vec(),
                        signature: b"slot-sig".to_vec(),
                    },
                )
            })
            .expect("slot put");
        assert_eq!(row.state, slots::SlotState::Staging);
        staged.push(slot_id);
    }
    assert_eq!(fx.db.read(slots::active_password_rev).expect("rev"), 0);

    // 同 slot_id 不同描述符（仍 staging 时）→ ID_CONFLICT。
    let conflicting = support::slot_descriptor(
        staged[0],
        vault.vault_id,
        1,
        slots::SlotKind::Password,
        BASE_TIME as u64 + 99,
    );
    let ctx = fx.device_ctx();
    let db = fx.db.clone();
    let clock = fx.clock.clone();
    let hook = fx.hook.clone();
    let first_staged = staged[0];
    let err = db
        .write_blocking(move |conn| {
            slots::put(
                conn,
                clock.as_ref(),
                hook.as_ref(),
                &ctx,
                &slots::SlotPutRequest {
                    slot_id: first_staged,
                    epoch: 1,
                    kind: slots::SlotKind::Password,
                    descriptor: conflicting,
                    signed_message: b"slot-proof".to_vec(),
                    signature: b"slot-sig".to_vec(),
                },
            )
        })
        .expect_err("id conflict");
    assert_eq!(code(&err), Some(ErrorCode::IdConflict));

    // 错 CAS → 409。
    let ctx = fx.device_ctx();
    let db = fx.db.clone();
    let clock = fx.clock.clone();
    let hook = fx.hook.clone();
    let first = staged[0];
    let err = db
        .write_blocking(move |conn| {
            slots::activate_password(
                conn,
                clock.as_ref(),
                hook.as_ref(),
                &ctx,
                &slots::PasswordActivateRequest {
                    expected_slot_set_rev: 5,
                    new_slot_id: first,
                    signed_message: b"activate".to_vec(),
                    signature: b"sig".to_vec(),
                },
            )
        })
        .expect_err("cas");
    assert_eq!(code(&err), Some(ErrorCode::CasConflict));

    // 正确 CAS → rev=1，槽 active。
    let ctx = fx.device_ctx();
    let db = fx.db.clone();
    let clock = fx.clock.clone();
    let hook = fx.hook.clone();
    let rev = db
        .write_blocking(move |conn| {
            slots::activate_password(
                conn,
                clock.as_ref(),
                hook.as_ref(),
                &ctx,
                &slots::PasswordActivateRequest {
                    expected_slot_set_rev: 0,
                    new_slot_id: first,
                    signed_message: b"activate".to_vec(),
                    signature: b"sig".to_vec(),
                },
            )
        })
        .expect("activate");
    assert_eq!(rev, 1);
    assert_eq!(fx.db.read(slots::active_password_rev).expect("rev"), 1);

    // 换第二个槽：旧 active 立即移除（ADR §3，不存在长期双 active）。
    let ctx = fx.device_ctx();
    let db = fx.db.clone();
    let clock = fx.clock.clone();
    let hook = fx.hook.clone();
    let second = staged[1];
    let rev = db
        .write_blocking(move |conn| {
            slots::activate_password(
                conn,
                clock.as_ref(),
                hook.as_ref(),
                &ctx,
                &slots::PasswordActivateRequest {
                    expected_slot_set_rev: 1,
                    new_slot_id: second,
                    signed_message: b"activate2".to_vec(),
                    signature: b"sig".to_vec(),
                },
            )
        })
        .expect("activate second");
    assert_eq!(rev, 2);
    let active = fx.db.read(slots::active_slots).expect("active");
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].slot_id, second);
}

#[test]
fn rotation_activate_requires_new_staged_slots() {
    let fx = Fixture::new("pair-rotation-slots");
    fx.create_object(ObjectType::Update, 1, 1, 32);
    let view = fx.begin_rotation(&[]);
    let rotation_id = view.rotation.rotation_id;
    assert_eq!(view.rotation.state, RotationState::Frozen);
    let (_candidate_a, _candidate_b) = fx.publish_candidate_baselines(2);
    fx.stage_grant(&rotation_id, &fx.device);
    // 没有 staging 新槽 → 409 INCOMPLETE。
    let err = fx
        .activate_rotation(&rotation_id)
        .expect_err("missing slots");
    assert_eq!(code(&err), Some(ErrorCode::Incomplete));

    // 发布候选槽（用第一个候选 checkpoint 的维护 reservation）后再激活。
    let reservation = fx.rotation_reservation(&rotation_id);
    fx.stage_candidate_slots(2, &reservation);
    let outcome = fx.activate_rotation(&rotation_id).expect("activate");
    assert_eq!(outcome.epoch, 2);
    let active = fx.db.read(slots::active_slots).expect("active slots");
    assert_eq!(active.len(), 2, "password + recovery 都激活");
    assert!(active.iter().all(|slot| slot.epoch == 2));
}

#[test]
fn genesis_verifies_device_certificate_domain() {
    // 用真实 Ed25519 验签：证书必须由管理 key 对 `MN/v1/device-cert\0 || CDE(cert)` 签名。
    use ed25519_dalek::{Signer, SigningKey};
    let manager_seed = [7u8; 32];
    let signing = SigningKey::from_bytes(&manager_seed);
    let manager_pub = signing.verifying_key().to_bytes();
    let vault_id = [0x91u8; 16];
    let device_id = [0x92u8; 16];
    let sign_pk = [0x93u8; 32];
    let hpke_pk = [0x94u8; 32];
    let cert = relay::verify::new_certificate(&vault_id, &device_id, &sign_pk, &hpke_pk, 1);
    let cert_bytes = cert.encode().expect("cert cde");
    let signed_bytes = proto::device_cert_signed_bytes(&cert).expect("signed bytes");
    let signature = signing.sign(&signed_bytes).to_bytes().to_vec();

    let open = |label: &str| {
        let path = temp_db_path(label);
        let clock = std::sync::Arc::new(FakeClock::new(BASE_TIME));
        let db = Db::open(DbConfig::new(path.clone()), clock.clone()).expect("db");
        (db, clock, path)
    };
    let cfg = std::sync::Arc::new(
        relay::RelayConfig::new(temp_db_path("pair-genesis-cfg")).with_setup_secret("setup"),
    );
    let (db, clock, ok_path) = open("pair-genesis-ok");
    let hook = std::sync::Arc::new(Ed25519Hook);
    let init = vault::InitRequest {
        vault_id,
        device_id,
        sign_pk,
        hpke_pk,
        manager_pub,
        setup_secret: "setup".to_string(),
        certificate: cert_bytes.clone(),
        certificate_signature: signature.clone(),
    };
    {
        let cfg = cfg.clone();
        db.write_blocking(relay::VaultScope::new(vault_id), move |conn| {
            vault::initialize(conn, clock.as_ref(), &cfg, hook.as_ref(), &init)
        })
        .expect("genesis with valid certificate");
    }

    // 换设备证书（签名对不上）→ SIGNATURE_INVALID。
    let (db, clock, bad_sig_path) = open("pair-genesis-bad-sig");
    let hook = std::sync::Arc::new(Ed25519Hook);
    let other = relay::verify::new_certificate(&vault_id, &device_id, &sign_pk, &hpke_pk, 1);
    let init = vault::InitRequest {
        vault_id,
        device_id,
        sign_pk,
        hpke_pk,
        manager_pub,
        setup_secret: "setup".to_string(),
        certificate: other.encode().expect("cert"),
        certificate_signature: b"deadbeef".to_vec(),
    };
    let err = {
        let cfg = cfg.clone();
        db.write_blocking(relay::VaultScope::new(vault_id), move |conn| {
            vault::initialize(conn, clock.as_ref(), &cfg, hook.as_ref(), &init)
        })
        .expect_err("bad certificate signature")
    };
    assert_eq!(code(&err), Some(ErrorCode::SignatureInvalid));

    // 证书绑定别的设备 → INVALID_REQUEST（即使签名有效）。
    let (db, clock, wrong_device_path) = open("pair-genesis-wrong-device");
    let hook = std::sync::Arc::new(Ed25519Hook);
    let wrong = relay::verify::new_certificate(&vault_id, &[0x99; 16], &sign_pk, &hpke_pk, 1);
    let init = vault::InitRequest {
        vault_id,
        device_id,
        sign_pk,
        hpke_pk,
        manager_pub,
        setup_secret: "setup".to_string(),
        certificate: wrong.encode().expect("cert"),
        certificate_signature: b"sig".to_vec(),
    };
    let err = {
        let cfg = cfg.clone();
        db.write_blocking(relay::VaultScope::new(vault_id), move |conn| {
            vault::initialize(conn, clock.as_ref(), &cfg, hook.as_ref(), &init)
        })
        .expect_err("cert bound to other device")
    };
    assert_eq!(code(&err), Some(ErrorCode::InvalidRequest));
    let _ = auth::setup_consumed;
    for path in [ok_path, bad_sig_path, wrong_device_path] {
        support::cleanup_db_dir(&path);
    }
}
