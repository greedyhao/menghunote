//! ADR 0014 S2a：同库两个 vault 的隔离证明。
//!
//! 两个 vault 故意使用**相同的局部 ID**（device_id / object_id / message_id / log seq /
//! invite_id / token_hash / checkpoint 请求 request_id / lease id），逐项证明
//! auth、控制链、对象/parts/log、邀请、reservation/lease、幂等、GC、usage 都不串库。

mod support;

use std::sync::Arc;

use support::relay;
use support::relay::auth::{self, AuthContext, Realm};
use support::relay::envelope::Header;
use support::relay::error::ErrorCode;
use support::relay::ids::{self, sha256, Id16};
use support::relay::objects;
use support::relay::scope::VaultScope;
use support::relay::time::{Clock, FakeClock};
use support::relay::vault::{self, InitRequest};
use support::{certificate_bytes, commit_for, single_part_envelope_at, Fixture, TestHook};

const VAULT_B: Id16 = [0xB7u8; 16];
const SAME_DEVICE: Id16 = [2u8; 16];

/// 第二个 vault（与 A 共用同一个库；局部 ID 故意与 A 重复）。
struct SecondVault {
    scope: VaultScope,
    device: Id16,
    device_ctx: AuthContext,
}

fn create_second_vault(fx: &Fixture) -> SecondVault {
    let clock = fx.clock.clone();
    let hook = fx.hook.clone();
    let cfg = fx.cfg.clone();
    let scope = VaultScope::new(VAULT_B);
    let sign_pk = fx.device_signing_key.verifying_key().to_bytes();
    let hpke_pk = [4u8; 32];
    let init = InitRequest {
        vault_id: VAULT_B,
        device_id: SAME_DEVICE,
        sign_pk,
        hpke_pk,
        manager_pub: fx.manager_pub,
        setup_secret: "setup-secret".to_string(),
        certificate: certificate_bytes(&VAULT_B, &SAME_DEVICE, &sign_pk, &hpke_pk, 1),
        certificate_signature: b"certificate-sig".to_vec(),
    };
    fx.db
        .inner()
        .write_blocking(scope, move |conn| {
            vault::create_vault_in_scope(conn, clock.as_ref(), &cfg, hook.as_ref(), &init, false)
        })
        .expect("create second vault");
    let vault = fx
        .db
        .inner()
        .read(scope, vault::load)
        .expect("second vault row");
    assert_eq!(vault.vault_id, VAULT_B);
    assert_eq!(
        vault.control_rev, 1,
        "第二个 vault 的控制链从头开始，不继承 A"
    );
    SecondVault {
        scope,
        device: SAME_DEVICE,
        device_ctx: AuthContext {
            realm: Realm::Device,
            subject: SAME_DEVICE,
            device_id: Some(SAME_DEVICE),
            auth_rev: vault.auth_rev,
            vault: scope,
        },
    }
}

fn scope_a(fx: &Fixture) -> VaultScope {
    VaultScope::new(fx.vault_id)
}

fn put_part(
    fx: &Fixture,
    scope: VaultScope,
    ctx: AuthContext,
    envelope: &[u8],
) -> support::relay::Result<objects::PartPutOutcome> {
    let header = Header::parse(envelope).expect("envelope header");
    let clock = fx.clock.clone();
    let vault_id = *scope.vault_id();
    let object_id = header.object_id;
    let part_no = header.part_index;
    let envelope = envelope.to_vec();
    fx.db.inner().write_blocking(scope, move |conn| {
        objects::put_part(
            conn,
            clock.as_ref(),
            &objects::PartUpload {
                actor: &ctx,
                vault_id: &vault_id,
                object_id: &object_id,
                part_no,
                envelope: &envelope,
                reservation: None,
            },
        )
    })
}

/// 两个 vault 同时存在、且指向各自的 vault 行。
#[test]
fn two_vaults_coexist_and_scope_selects_the_right_row() {
    let fx = Fixture::new("multi-coexist");
    let second = create_second_vault(&fx);
    let a = fx
        .db
        .inner()
        .read(scope_a(&fx), vault::load)
        .expect("vault A");
    let b = fx
        .db
        .inner()
        .read(second.scope, vault::load)
        .expect("vault B");
    assert_eq!(a.vault_id, fx.vault_id);
    assert_eq!(b.vault_id, VAULT_B);
    assert_ne!(a.log_id, b.log_id, "每 vault 独立 log_id/log_incarnation");
    assert_ne!(a.cursor_key, b.cursor_key, "每 vault 独立游标 HMAC key");
    // 未知 vault 的 scope 读到 None（404），不会"回退到任意 vault"。
    let missing = VaultScope::new([0xEEu8; 16]);
    assert!(fx
        .db
        .inner()
        .read(missing, vault::load_opt)
        .expect("load missing")
        .is_none());
    // v1 legacy 默认 vault 仍是 A（唯一显式标记）。
    let legacy = fx
        .db
        .read_instance(relay::db::schema::legacy_default_vault_id)
        .expect("legacy");
    assert_eq!(legacy, Some(fx.vault_id));
}

#[test]
fn auth_tokens_and_control_chain_are_isolated() {
    let fx = Fixture::new("multi-auth");
    let second = create_second_vault(&fx);
    let clock = fx.clock.clone();
    let hook = fx.hook.clone();

    // 同 subject（相同 device_id）在两个 vault 都能拿 challenge/token，但 scope 不同。
    let issue = |scope: VaultScope| -> String {
        let clock = clock.clone();
        let hook = hook.clone();
        fx.db
            .inner()
            .write_blocking(scope, move |conn| {
                let challenge =
                    auth::create_challenge(conn, clock.as_ref(), Realm::Device, &SAME_DEVICE)?;
                let outcome = auth::issue_token(
                    conn,
                    clock.as_ref(),
                    hook.as_ref(),
                    &challenge.challenge_id,
                    b"message",
                    b"signature",
                )?;
                Ok(outcome.token)
            })
            .expect("issue token")
    };
    let token_a = issue(scope_a(&fx));
    let token_b = issue(second.scope);

    let clock2 = clock.clone();
    let token_a_for_auth = token_a.clone();
    let ctx_a = fx
        .db
        .inner()
        .read(scope_a(&fx), move |conn| {
            auth::authenticate(conn, clock2.as_ref(), &token_a_for_auth)
        })
        .expect("token A in scope A");
    let clock2 = clock.clone();
    let token_b_for_a = token_b.clone();
    let crossed = fx.db.inner().read(scope_a(&fx), move |conn| {
        auth::authenticate(conn, clock2.as_ref(), &token_b_for_a)
    });
    assert_eq!(
        crossed
            .expect_err("B token must not authenticate in A")
            .code(),
        Some(ErrorCode::Unauthorized)
    );
    let clock2 = clock.clone();
    let ctx_b = fx
        .db
        .inner()
        .read(second.scope, move |conn| {
            auth::authenticate(conn, clock2.as_ref(), &token_b)
        })
        .expect("token B in scope B");
    assert_eq!(ctx_a.device_id, Some(SAME_DEVICE));
    assert_eq!(ctx_b.device_id, Some(SAME_DEVICE));
    // 旧 v1 兼容探测：A 的 token 在"B 视角"下有效 → 路由层据此回 404（同旧行为），
    // 而不是 401 泄露/混淆。
    let clock2 = clock.clone();
    let token_a_for_probe = token_a.clone();
    let elsewhere = fx
        .db
        .read_instance(move |conn| {
            auth::token_valid_in_other_vault(
                conn,
                clock2.as_ref(),
                &token_a_for_probe,
                second.scope,
            )
        })
        .expect("probe");
    assert!(
        elsewhere,
        "A 的 token 相对 B scope 是'别的 vault 的有效凭据'"
    );

    // 控制链：只往 A 追加，B 的 control_rev / 链头 / 管理钥不变。
    let before_b = fx.db.inner().read(second.scope, vault::load).expect("B");
    fx.db
        .inner()
        .write_blocking(scope_a(&fx), |conn| {
            vault::append_control(conn, 111, b"only-a")
        })
        .expect("append A control");
    let after_a = fx.db.inner().read(scope_a(&fx), vault::load).expect("A");
    let after_b = fx.db.inner().read(second.scope, vault::load).expect("B");
    assert_eq!(after_a.control_rev, before_b.control_rev + 1);
    assert_eq!(
        after_b.control_rev, before_b.control_rev,
        "B 的控制链不受影响"
    );
    let head_a = fx
        .db
        .inner()
        .read(scope_a(&fx), vault::control_head)
        .expect("head A");
    let head_b = fx
        .db
        .inner()
        .read(second.scope, vault::control_head)
        .expect("head B");
    assert_eq!(head_a.0, 2);
    assert_eq!(head_b.0, 1);
    assert_ne!(head_a.1, head_b.1, "两个 vault 的链头记录不会互相覆盖");
}

#[test]
fn objects_parts_and_log_are_isolated_with_identical_local_ids() {
    let fx = Fixture::new("multi-objects");
    let second = create_second_vault(&fx);
    let object_id = ids::random_hash32().expect("object id");
    let scope_a = scope_a(&fx);
    let ctx_a = fx.device_ctx();

    // 同一个 object_id + 相同的 message/seq/epoch，在两个 vault 写不同字节。
    let envelope_a = single_part_envelope_at(
        support::relay::envelope::ObjectType::Update,
        fx.vault_id,
        object_id,
        1,
        0,
        512,
        1,
    );
    let envelope_b = single_part_envelope_at(
        support::relay::envelope::ObjectType::Update,
        VAULT_B,
        object_id,
        1,
        0,
        544,
        1,
    );
    put_part(&fx, scope_a, ctx_a, &envelope_a).expect("part A");
    put_part(&fx, second.scope, second.device_ctx, &envelope_b).expect("part B");

    let read_a = fx
        .db
        .inner()
        .read(scope_a, |conn| objects::read_part(conn, &object_id, 0))
        .expect("read A");
    let read_b = fx
        .db
        .inner()
        .read(second.scope, |conn| objects::read_part(conn, &object_id, 0))
        .expect("read B");
    assert_eq!(read_a.0, envelope_a);
    assert_eq!(read_b.0, envelope_b);
    assert_ne!(read_a.0, read_b.0, "同 object_id 的密文必须各自独立");

    // complete：同 commit 字节在两个 vault 各自成功（幂等表按 vault 分槽）。
    let digest_a = support::digest_of(std::slice::from_ref(&envelope_a)).expect("digest A");
    let digest_b = support::digest_of(std::slice::from_ref(&envelope_b)).expect("digest B");
    let commit_a = commit_for(&digest_a);
    let commit_b = commit_for(&digest_b);
    let hook = Arc::new(TestHook::new());
    let clock = fx.clock.clone();
    let outcome_a = {
        let hook = hook.clone();
        let clock = clock.clone();
        let ctx = ctx_a;
        let commit = commit_a.clone();
        fx.db
            .inner()
            .write_blocking(scope_a, move |conn| {
                objects::complete(
                    conn,
                    clock.as_ref(),
                    hook.as_ref(),
                    &objects::CompleteUpload {
                        actor: &ctx,
                        vault_id: &fx.vault_id,
                        object_id: &object_id,
                        commit: &commit,
                        signature: b"signature",
                    },
                )
            })
            .expect("complete A")
    };
    let outcome_b = {
        let hook = hook.clone();
        let clock = clock.clone();
        let ctx = second.device_ctx;
        let commit = commit_b.clone();
        fx.db
            .inner()
            .write_blocking(second.scope, move |conn| {
                objects::complete(
                    conn,
                    clock.as_ref(),
                    hook.as_ref(),
                    &objects::CompleteUpload {
                        actor: &ctx,
                        vault_id: &VAULT_B,
                        object_id: &object_id,
                        commit: &commit,
                        signature: b"signature",
                    },
                )
            })
            .expect("complete B")
    };
    assert_eq!(outcome_a.relay_seq, Some(1), "A 的日志从 1 开始");
    assert_eq!(
        outcome_b.relay_seq,
        Some(1),
        "B 的日志也从 1 开始，不继承 A 的 head"
    );
    assert!(!outcome_a.duplicate && !outcome_b.duplicate);

    // 幂等 tombstone 不串：A 的 commit 在 B 重放 → 同 object 不同 commit → 409（属于 B
    // 自己的 object 行），但 B 的 usage/log 不变；A 的行仍是 A 的 commit。
    let before_b_log = fx
        .db
        .inner()
        .read(second.scope, relay::log::count)
        .expect("B log count");
    let replay_in_b = {
        let hook = hook.clone();
        let clock = clock.clone();
        let ctx = second.device_ctx;
        let commit = commit_a.clone();
        fx.db.inner().write_blocking(second.scope, move |conn| {
            objects::complete(
                conn,
                clock.as_ref(),
                hook.as_ref(),
                &objects::CompleteUpload {
                    actor: &ctx,
                    vault_id: &VAULT_B,
                    object_id: &object_id,
                    commit: &commit,
                    signature: b"signature",
                },
            )
        })
    };
    assert_eq!(
        replay_in_b.expect_err("A commit in B must conflict").code(),
        Some(ErrorCode::IdConflict)
    );
    assert_eq!(
        fx.db
            .inner()
            .read(second.scope, relay::log::count)
            .expect("B log count"),
        before_b_log
    );

    // 日志：两边都是 seq 1（对象 complete 的自增 seq 不会因另一 vault 有行而跳号）。
    let log_a = fx
        .db
        .inner()
        .read(scope_a, relay::log::timeline)
        .expect("log A");
    let log_b = fx
        .db
        .inner()
        .read(second.scope, relay::log::timeline)
        .expect("log B");
    assert_eq!(log_a.len(), 1);
    assert_eq!(log_b.len(), 1);
    assert_eq!(log_a[0].0, 1);
    assert_eq!(log_b[0].0, 1);
    // 游标：A 的 head 只由 A 的日志推进。
    let vault_a = fx.db.inner().read(scope_a, vault::load).expect("A");
    let vault_b = fx.db.inner().read(second.scope, vault::load).expect("B");
    assert_eq!(vault_a.head_seq, 1);
    assert_eq!(vault_b.head_seq, 1);
    let cursor_a = relay::log::cursor_at(&vault_a, 1).expect("cursor A");
    let cursor_b = relay::log::cursor_at(&vault_b, 1).expect("cursor B");
    assert_ne!(cursor_a, cursor_b, "游标 HMAC key 不同 → 游标不可互换");
    let decoded_a = relay::cursor::CursorKey::new(vault_a.cursor_key)
        .decode(&cursor_a)
        .expect("decode cursor A");
    let clock2 = fx.clock.clone();
    let page_b = fx.db.inner().read(second.scope, move |conn| {
        relay::log::page(conn, &vault_b, &decoded_a, 10, 10)
    });
    let err = page_b.expect_err("A 的游标在 B 必须被拒绝");
    assert_eq!(err.code(), Some(ErrorCode::BadCursor));
    let _ = clock2;
}

#[test]
fn invites_reservations_leases_and_idempotency_are_isolated() {
    let fx = Fixture::new("multi-invite");
    let second = create_second_vault(&fx);
    let clock = fx.clock.clone();
    let hook = fx.hook.clone();
    let scope_a = scope_a(&fx);

    // 同 invite_id + 同 token_hash + 同 request_id 在两个 vault 各建一份邀请。
    let token = [0x42u8; 32];
    let invite_id = [0x43u8; 16];
    let insert_same_invite = |scope: VaultScope| {
        fx.db
            .inner()
            .write_blocking(scope, move |conn| {
                conn.execute(
                    "INSERT INTO invites(vault_id, invite_id, token_hash, expires_at,
                                         pending_device, used_at)
                     VALUES (?1, ?2, ?3, 9999999999, NULL, NULL)",
                    (
                        conn.scope().as_slice(),
                        invite_id.as_slice(),
                        sha256(&token).as_slice(),
                    ),
                )?;
                Ok(())
            })
            .expect("same invite id in both vaults")
    };
    insert_same_invite(scope_a);
    insert_same_invite(second.scope);

    // 兑换 A 的邀请：只创建 A 的 pending 设备，B 的同 id 邀请仍未消费。
    let redeem = |scope: VaultScope, expected_vault: Id16| {
        let clock = clock.clone();
        let hook = hook.clone();
        fx.db
            .inner()
            .write_blocking(scope, move |conn| {
                support::relay::pairing::redeem_invite(
                    conn,
                    clock.as_ref(),
                    hook.as_ref(),
                    &invite_id,
                    &token,
                    &support::relay::pairing::NewDevice {
                        device_id: [0x45u8; 16],
                        sign_pk: [0x46u8; 32],
                        hpke_pk: [0x47u8; 32],
                        proof: b"proof".to_vec(),
                        proof_signature: b"proof-sig".to_vec(),
                    },
                )
            })
            .map(|outcome| (expected_vault, outcome))
    };
    let redeemed_a = redeem(scope_a, fx.vault_id).expect("redeem A");
    assert_eq!(redeemed_a.1.pending_device, [0x45u8; 16]);
    let invite_a = fx
        .db
        .inner()
        .read(scope_a, move |conn| {
            support::relay::pairing::invite(conn, &invite_id)
        })
        .expect("invite A")
        .expect("row");
    let invite_b = fx
        .db
        .inner()
        .read(second.scope, move |conn| {
            support::relay::pairing::invite(conn, &invite_id)
        })
        .expect("invite B")
        .expect("row");
    assert_eq!(invite_a.used_at, Some(fx.clock.now_secs()));
    assert_eq!(invite_a.pending_device, Some([0x45u8; 16]));
    assert!(
        invite_b.used_at.is_none(),
        "B 的同 id 邀请不得被 A 的兑换消费"
    );
    assert!(invite_b.pending_device.is_none());
    let pending_a = fx
        .db
        .inner()
        .read(scope_a, relay::pairing::pending_devices)
        .expect("pending A");
    let pending_b = fx
        .db
        .inner()
        .read(second.scope, relay::pairing::pending_devices)
        .expect("pending B");
    assert_eq!(pending_a, vec![[0x45u8; 16]]);
    assert!(pending_b.is_empty(), "B 没有 pending 设备");

    // 维护 reservation：同 resource_id 在两个 vault 各一份（UNIQUE 带 vault 前缀）。
    let resource = [0x48u8; 16];
    let create_reservation = |scope: VaultScope, owner: Id16, bytes: i64| {
        let clock = clock.clone();
        fx.db
            .inner()
            .write_blocking(scope, move |conn| {
                support::relay::quota::create_reservation(
                    conn,
                    clock.now_secs(),
                    support::relay::quota::ReservationPurpose::Checkpoint,
                    resource.as_slice(),
                    &owner,
                    bytes,
                    clock.now_secs() + 600,
                )
            })
            .expect("reservation")
    };
    let res_a = create_reservation(scope_a, fx.device, 1000);
    let res_b = create_reservation(second.scope, second.device, 2000);
    assert_ne!(res_a, res_b);
    let row_a = fx
        .db
        .inner()
        .read(scope_a, move |conn| {
            support::relay::quota::reservation(conn, &res_a)
        })
        .expect("reservation A")
        .expect("row");
    let row_b = fx
        .db
        .inner()
        .read(second.scope, move |conn| {
            support::relay::quota::reservation(conn, &res_b)
        })
        .expect("reservation B")
        .expect("row");
    assert_eq!(row_a.reserved_bytes, 1000);
    assert_eq!(row_b.reserved_bytes, 2000);
    // 交叉读：A scope 看不到 B 的 reservation。
    let crossed = fx
        .db
        .inner()
        .read(scope_a, move |conn| {
            support::relay::quota::reservation(conn, &res_b)
        })
        .expect("cross read");
    assert!(crossed.is_none(), "reservation 必须按 vault 作用域读");

    // bootstrap lease：同 lease_id 两份；续租只改自己那份。
    let lease_id = [0x49u8; 16];
    let lease_created = clock.now_secs();
    let insert_lease = |scope: VaultScope, device: Id16| {
        fx.db
            .inner()
            .write_blocking(scope, move |conn| {
                conn.execute(
                    "INSERT INTO bootstrap_leases(vault_id, lease_id, device_id,
                                                  primary_checkpoint, fallback_checkpoint,
                                                  floor_seq, created_at, expires_at)
                     VALUES (?1, ?2, ?3, ?4, ?4, 0, ?5, ?5)",
                    (
                        conn.scope().as_slice(),
                        lease_id.as_slice(),
                        device.as_slice(),
                        [0x4Au8; 16].as_slice(),
                        lease_created + 600,
                    ),
                )?;
                Ok(())
            })
            .expect("lease")
    };
    insert_lease(scope_a, fx.device);
    insert_lease(second.scope, second.device);
    let renew_clock = fx.clock.clone();
    renew_clock.advance_secs(60);
    let renew = |scope: VaultScope, device: Id16| {
        let clock = renew_clock.clone();
        let ctx = AuthContext {
            realm: Realm::Device,
            subject: device,
            device_id: Some(device),
            auth_rev: 1,
            vault: scope,
        };
        fx.db
            .inner()
            .write_blocking(scope, move |conn| {
                support::relay::checkpoints::renew_lease(conn, clock.as_ref(), &ctx, &lease_id)
            })
            .expect("renew")
    };
    let renewed_a = renew(scope_a, fx.device);
    assert!(renewed_a.expires_at > lease_created + 600);
    let lease_b_row = fx
        .db
        .read_instance(|conn| {
            let expires: i64 = conn.query_row(
                "SELECT expires_at FROM bootstrap_leases WHERE vault_id = ?1 AND lease_id = ?2",
                (VAULT_B.as_slice(), lease_id.as_slice()),
                |row| row.get(0),
            )?;
            Ok(expires)
        })
        .expect("lease B row");
    assert_eq!(
        lease_b_row,
        lease_created + 600,
        "A 的续租不得改 B 的 lease"
    );
}

#[test]
fn quota_usage_and_gc_are_isolated() {
    let fx = Fixture::new("multi-gc");
    let second = create_second_vault(&fx);
    let scope_a = scope_a(&fx);
    let clock = fx.clock.clone();

    // usage：A 扣 100 字节，B 仍为 0。
    fx.db
        .inner()
        .write_blocking(scope_a, |conn| {
            support::relay::quota::charge_normal(conn, 100)
        })
        .expect("charge A");
    let usage_a = fx
        .db
        .inner()
        .read(scope_a, support::relay::quota::usage)
        .expect("usage A");
    let usage_b = fx
        .db
        .inner()
        .read(second.scope, support::relay::quota::usage)
        .expect("usage B");
    assert_eq!(usage_a.normal_bytes, 100);
    assert_eq!(usage_b.normal_bytes, 0, "B 的账本不受 A 的写入影响");

    // 配额：把 B 的 normal_limit 调到 16 字节，B 立即 507，A 仍可写。
    fx.db
        .inner()
        .write_blocking(second.scope, |conn| {
            conn.execute(
                "UPDATE vaults SET normal_limit = 16 WHERE vault_id = ?1",
                [conn.scope().as_slice()],
            )?;
            Ok(())
        })
        .expect("shrink B quota");
    let b_large = fx.db.inner().write_blocking(second.scope, |conn| {
        support::relay::quota::charge_normal(conn, 32)
    });
    assert_eq!(
        b_large.expect_err("B over quota").code(),
        Some(ErrorCode::QuotaExceeded)
    );
    fx.db
        .inner()
        .write_blocking(scope_a, |conn| {
            support::relay::quota::charge_normal(conn, 32)
        })
        .expect("A still writable");

    // GC / staging 清理：同 object_id 的 staging 对象在两边都超过 24h，清理 A 只删 A。
    let object_id = [0x4Bu8; 32];
    let insert_staging = |scope: VaultScope| {
        fx.db
            .inner()
            .write_blocking(scope, move |conn| {
                conn.execute(
                    "INSERT INTO objects(vault_id, object_id, message_id, producer, epoch, type,
                                         generation, sender_seq, part_count, plain_len, state,
                                         budget_class, created_at, touched_at)
                     VALUES (?1, ?2, ?3, ?4, 1, 1, 0, 0, 1, 10, 'staging', 'normal', 1, 1)",
                    (
                        conn.scope().as_slice(),
                        object_id.as_slice(),
                        [0x4Cu8; 16].as_slice(),
                        SAME_DEVICE.as_slice(),
                    ),
                )?;
                conn.execute(
                    "INSERT INTO object_parts(vault_id, object_id, part_no, envelope, digest)
                     VALUES (?1, ?2, 0, ?3, ?4)",
                    (
                        conn.scope().as_slice(),
                        object_id.as_slice(),
                        vec![0x4Du8; 220],
                        [0x4Eu8; 32].as_slice(),
                    ),
                )?;
                Ok(())
            })
            .expect("staging object")
    };
    insert_staging(scope_a);
    insert_staging(second.scope);
    let cleanup_clock = clock.clone();
    let deleted = fx
        .db
        .inner()
        .write_blocking(scope_a, move |conn| {
            objects::cleanup_staging(conn, cleanup_clock.now_secs() + 100_000)
        })
        .expect("cleanup A");
    assert_eq!(deleted, 1);
    let a_row = fx
        .db
        .inner()
        .read(scope_a, move |conn| objects::load_opt(conn, &object_id))
        .expect("A object");
    let b_row = fx
        .db
        .inner()
        .read(second.scope, move |conn| {
            objects::load_opt(conn, &object_id)
        })
        .expect("B object");
    assert!(a_row.is_none(), "A 的 staging 对象被回收");
    assert!(b_row.is_some(), "B 的同 id 对象不得被 A 的 GC 删除");
    let b_parts = fx
        .db
        .inner()
        .read(second.scope, move |conn| objects::parts(conn, &object_id))
        .expect("B parts");
    assert_eq!(b_parts.len(), 1, "B 的分片密文保留");

    // 整 tick：A 的 GC 不得改动 B 的 usage / head / 行数。
    let before_b = fx
        .db
        .inner()
        .read(second.scope, vault::load)
        .expect("B before");
    let before_b_usage = fx
        .db
        .inner()
        .read(second.scope, support::relay::quota::usage)
        .expect("B usage");
    let gc_now = clock.now_secs();
    fx.db
        .inner()
        .write_blocking(scope_a, move |conn| {
            support::relay::gc::tick(conn, &FakeClock::new(gc_now))
        })
        .expect("gc A");
    let after_b = fx
        .db
        .inner()
        .read(second.scope, vault::load)
        .expect("B after");
    let after_b_usage = fx
        .db
        .inner()
        .read(second.scope, support::relay::quota::usage)
        .expect("B usage");
    assert_eq!(after_b.head_seq, before_b.head_seq);
    assert_eq!(after_b.gc_floor, before_b.gc_floor);
    assert_eq!(after_b_usage, before_b_usage);
}

#[test]
fn checkpoint_request_id_is_idempotent_per_vault() {
    let fx = Fixture::new("multi-dedup");
    let second = create_second_vault(&fx);
    let scope_a = scope_a(&fx);
    let request_id = [0x55u8; 16];
    let clock = fx.clock.clone();
    let create = |scope: VaultScope, device: Id16| {
        let clock = clock.clone();
        let ctx = AuthContext {
            realm: Realm::Device,
            subject: device,
            device_id: Some(device),
            auth_rev: 1,
            vault: scope,
        };
        fx.db
            .inner()
            .write_blocking(scope, move |conn| {
                support::relay::checkpoints::create(
                    conn,
                    clock.as_ref(),
                    &ctx,
                    4096,
                    None,
                    None,
                    Some(request_id),
                )
            })
            .expect("checkpoint create")
    };
    let first_a = create(scope_a, fx.device);
    let first_b = create(second.scope, second.device);
    assert_ne!(first_a.checkpoint_id, first_b.checkpoint_id);
    // 各自的 request_id 重放回各自的结果（dedup 表按 vault 分槽）。
    let replay_a = create(scope_a, fx.device);
    let replay_b = create(second.scope, second.device);
    assert_eq!(replay_a.checkpoint_id, first_a.checkpoint_id);
    assert_eq!(replay_b.checkpoint_id, first_b.checkpoint_id);
    // 交叉读：A scope 看不到 B 的 checkpoint 行。
    let crossed = fx.db.inner().read(scope_a, move |conn| {
        support::relay::checkpoints::get(conn, &first_b.checkpoint_id)
    });
    assert_eq!(
        crossed.expect_err("B checkpoint invisible in A").code(),
        Some(ErrorCode::NotFound)
    );
}

/// 作用域断言：用 B 的 scope 读不到 A 的行（不存在隐式回退/按 subject 猜租户）。
#[test]
fn scope_never_falls_back_to_another_vault() {
    let fx = Fixture::new("multi-scope");
    let second = create_second_vault(&fx);
    let seen = fx
        .db
        .inner()
        .read(second.scope, vault::load)
        .expect("B row");
    assert_eq!(seen.vault_id, VAULT_B);
    assert_ne!(seen.vault_id, fx.vault_id);
    // A 的行在 B 的 scope 里不可见（`load_opt` 只按 scope 的 vault_id 查）。
    let a_in_b = fx
        .db
        .inner()
        .read(second.scope, vault::load_opt)
        .expect("B opt")
        .map(|vault| vault.vault_id);
    assert_eq!(a_in_b, Some(VAULT_B));
}
