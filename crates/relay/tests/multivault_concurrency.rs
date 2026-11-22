//! ADR 0014 S2a：同一 writer 下的多租户并发/公平性。
//!
//! 保留"单 writer 线程"语义（不引入 per-vault 库/writer）；这里证明：
//! - 两个 vault 并发写同一个库不会死锁、也不互相串改；
//! - 配额/GC 按 vault 独立：一个租户的 507 或 GC 不影响另一个租户的写入；
//! - 每个 vault 的 `usage`/`objects`/`parts` 行数只由自己的写入决定。

mod support;

use support::relay;
use support::relay::auth::{AuthContext, Realm};
use support::relay::error::ErrorCode;
use support::relay::ids::{random_hash32, Id16};
use support::relay::objects;
use support::relay::scope::VaultScope;
use support::relay::time::Clock;
use support::relay::vault::{self, InitRequest};
use support::{certificate_bytes, single_part_envelope_at, Fixture};

const VAULT_B: Id16 = [0xC7u8; 16];
const SAME_DEVICE: Id16 = [2u8; 16];

fn create_second_vault(fx: &Fixture) -> AuthContext {
    let scope = VaultScope::new(VAULT_B);
    let clock = fx.clock.clone();
    let hook = fx.hook.clone();
    let cfg = fx.cfg.clone();
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
        .expect("second vault");
    let vault = fx.db.inner().read(scope, vault::load).expect("B row");
    AuthContext {
        realm: Realm::Device,
        subject: SAME_DEVICE,
        device_id: Some(SAME_DEVICE),
        auth_rev: vault.auth_rev,
        vault: VaultScope::new(VAULT_B),
    }
}

fn put_one(
    fx: &Fixture,
    scope: VaultScope,
    ctx: AuthContext,
    vault_id: Id16,
    plain_len: u64,
) -> relay::Result<()> {
    let object_id = random_hash32().expect("object id");
    let envelope = single_part_envelope_at(
        relay::envelope::ObjectType::Update,
        vault_id,
        object_id,
        1,
        0,
        plain_len,
        1,
    );
    let clock = fx.clock.clone();
    let header = relay::envelope::Header::parse(&envelope).expect("header");
    fx.db.inner().write_blocking(scope, move |conn| {
        objects::put_part(
            conn,
            clock.as_ref(),
            &objects::PartUpload {
                actor: &ctx,
                vault_id: &vault_id,
                object_id: &header.object_id,
                part_no: 0,
                envelope: &envelope,
                reservation: None,
            },
        )?;
        Ok(())
    })
}

#[test]
fn concurrent_writes_from_two_vaults_stay_isolated_and_complete() {
    let fx = Fixture::new("multi-concurrent");
    let _ctx_b = create_second_vault(&fx);
    let scope_a = VaultScope::new(fx.vault_id);
    let scope_b = VaultScope::new(VAULT_B);
    let ctx_a = fx.device_ctx();
    let ctx_b = AuthContext {
        realm: Realm::Device,
        subject: SAME_DEVICE,
        device_id: Some(SAME_DEVICE),
        auth_rev: 1,
        vault: VaultScope::new(VAULT_B),
    };

    const WRITES: usize = 24;
    std::thread::scope(|threads| {
        let fixture = &fx;
        let handle_a = threads.spawn(move || {
            for _ in 0..WRITES {
                put_one(fixture, scope_a, ctx_a, fixture.vault_id, 512).expect("A write");
            }
        });
        let handle_b = threads.spawn(move || {
            for _ in 0..WRITES {
                put_one(fixture, scope_b, ctx_b, VAULT_B, 640).expect("B write");
            }
        });
        // 同时跑 A 的 GC：不能删/阻塞 B 的写入。
        let gc_now = fx.clock.now_secs();
        let gc = threads.spawn(move || {
            fixture
                .db
                .inner()
                .write_blocking(scope_a, move |conn| {
                    relay::gc::tick(conn, &relay::time::FakeClock::new(gc_now))
                })
                .expect("gc A");
        });
        handle_a.join().expect("A thread");
        handle_b.join().expect("B thread");
        gc.join().expect("gc thread");
    });

    // 行数/账本只由本 vault 的写入决定。
    let count = |scope: VaultScope| -> i64 {
        fx.db
            .inner()
            .read(scope, |conn| {
                Ok(conn.query_row(
                    "SELECT COUNT(*) FROM objects WHERE vault_id = ?1",
                    [conn.scope().as_slice()],
                    |row| row.get(0),
                )?)
            })
            .expect("count")
    };
    let usage = |scope: VaultScope| {
        fx.db
            .inner()
            .read(scope, relay::quota::usage)
            .expect("usage")
    };
    let count_a = count(scope_a);
    let count_b = count(scope_b);
    assert_eq!(count_a, WRITES as i64, "A 只能看到自己的对象");
    assert_eq!(count_b, WRITES as i64, "B 只能看到自己的对象");
    // 每片 envelope = 212B 固定头 + plain + 16B tag。
    // envelope = 212B 固定头 + plaintext（AEAD tag 已含在信封开销里）。
    let per_part = |plain: i64| 212 + plain;
    assert_eq!(usage(scope_a).normal_bytes, WRITES as i64 * per_part(512));
    assert_eq!(usage(scope_b).normal_bytes, WRITES as i64 * per_part(640));
    // head_seq 未被 parts 推进（parts 不进 log）。
    assert_eq!(
        fx.db
            .inner()
            .read(scope_a, vault::load)
            .expect("A")
            .head_seq,
        0
    );
    assert_eq!(
        fx.db
            .inner()
            .read(scope_b, vault::load)
            .expect("B")
            .head_seq,
        0
    );
    // 日志/幂等表都没有跨租户行。
    let dedup_a: i64 = fx
        .db
        .inner()
        .read(scope_a, |conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM dedup WHERE vault_id = ?1",
                [conn.scope().as_slice()],
                |row| row.get(0),
            )?)
        })
        .expect("dedup A");
    assert_eq!(dedup_a, 0);
}

#[test]
fn quota_and_gc_of_one_vault_do_not_block_the_other() {
    let fx = Fixture::new("multi-fair");
    let _ctx_b = create_second_vault(&fx);
    let scope_a = VaultScope::new(fx.vault_id);
    let scope_b = VaultScope::new(VAULT_B);
    let ctx_a = fx.device_ctx();
    let ctx_b = AuthContext {
        realm: Realm::Device,
        subject: SAME_DEVICE,
        device_id: Some(SAME_DEVICE),
        auth_rev: 1,
        vault: VaultScope::new(VAULT_B),
    };

    // B 配额缩到 1KiB：B 写到 507 就停；A 不受影响继续写。
    fx.db
        .inner()
        .write_blocking(scope_b, |conn| {
            conn.execute(
                "UPDATE vaults SET normal_limit = 1024 WHERE vault_id = ?1",
                [conn.scope().as_slice()],
            )?;
            Ok(())
        })
        .expect("shrink B");

    std::thread::scope(|threads| {
        let fixture = &fx;
        let a = threads.spawn(move || {
            let mut ok = 0usize;
            for _ in 0..8 {
                if put_one(fixture, scope_a, ctx_a, fixture.vault_id, 512).is_ok() {
                    ok += 1;
                }
            }
            ok
        });
        let b = threads.spawn(move || {
            let mut ok = 0usize;
            let mut exceeded = 0usize;
            for _ in 0..8 {
                match put_one(fixture, scope_b, ctx_b, VAULT_B, 640) {
                    Ok(()) => ok += 1,
                    Err(err) => {
                        assert_eq!(err.code(), Some(ErrorCode::QuotaExceeded));
                        exceeded += 1;
                    }
                }
            }
            (ok, exceeded)
        });
        assert_eq!(a.join().expect("A thread"), 8, "A 的配额不受 B 影响");
        let (b_ok, b_exceeded) = b.join().expect("B thread");
        assert!(b_ok >= 1, "B 至少能写到配额上限");
        assert!(b_exceeded >= 1, "B 超过自己的配额必须 507");
        assert_eq!(b_ok + b_exceeded, 8);
    });

    // A 的账本与对象数不受 B 的 507 影响。
    assert_eq!(
        fx.db
            .inner()
            .read(scope_a, relay::quota::usage)
            .expect("A usage")
            .normal_bytes,
        8 * (212 + 512)
    );
    assert!(
        fx.db
            .inner()
            .read(scope_b, relay::quota::usage)
            .expect("B usage")
            .normal_bytes
            <= 1024
    );
    let b_usage = fx
        .db
        .inner()
        .read(scope_b, relay::quota::usage)
        .expect("B usage");
    // B 的 objects 行数 = 成功写入数。
    let b_objects: i64 = fx
        .db
        .inner()
        .read(scope_b, |conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM objects WHERE vault_id = ?1",
                [conn.scope().as_slice()],
                |row| row.get(0),
            )?)
        })
        .expect("B objects");
    assert_eq!(
        b_objects,
        b_usage.normal_bytes / (212 + 640),
        "B 的账本与对象行数必须一致"
    );
}

/// 同一 `object_id` 在两个 vault 并发写：两个 vault 各自成功且互不覆盖。
#[test]
fn same_object_id_in_two_vaults_is_written_independently() {
    let fx = Fixture::new("multi-same-object");
    let _ctx_b = create_second_vault(&fx);
    let scope_a = VaultScope::new(fx.vault_id);
    let scope_b = VaultScope::new(VAULT_B);
    let object_id = [0x99u8; 32];
    let ctx_a = fx.device_ctx();
    let ctx_b = AuthContext {
        realm: Realm::Device,
        subject: SAME_DEVICE,
        device_id: Some(SAME_DEVICE),
        auth_rev: 1,
        vault: VaultScope::new(VAULT_B),
    };
    let write = |fx: &Fixture, scope: VaultScope, ctx: AuthContext, vault_id: Id16, len: u64| {
        let envelope = single_part_envelope_at(
            relay::envelope::ObjectType::Update,
            vault_id,
            object_id,
            1,
            0,
            len,
            1,
        );
        put_part_with(fx.clock.clone(), fx, scope, ctx, envelope)
    };
    let fixture = &fx;
    std::thread::scope(|threads| {
        let a = threads.spawn(move || write(fixture, scope_a, ctx_a, fixture.vault_id, 512));
        let b = threads.spawn(move || write(fixture, scope_b, ctx_b, VAULT_B, 640));
        a.join().expect("A").expect("A put");
        b.join().expect("B").expect("B put");
    });
    let read = |scope: VaultScope| {
        fx.db
            .inner()
            .read(scope, move |conn| objects::read_part(conn, &object_id, 0))
            .expect("read")
    };
    let (a_bytes, _) = read(scope_a);
    let (b_bytes, _) = read(scope_b);
    assert_eq!(a_bytes.len(), 212 + 512);
    assert_eq!(b_bytes.len(), 212 + 640);
    assert_ne!(a_bytes, b_bytes);
}

fn put_part_with(
    clock: std::sync::Arc<relay::time::FakeClock>,
    fx: &Fixture,
    scope: VaultScope,
    ctx: AuthContext,
    envelope: Vec<u8>,
) -> relay::Result<()> {
    let header = relay::envelope::Header::parse(&envelope).expect("header");
    let vault_id = *scope.vault_id();
    fx.db.inner().write_blocking(scope, move |conn| {
        objects::put_part(
            conn,
            clock.as_ref(),
            &objects::PartUpload {
                actor: &ctx,
                vault_id: &vault_id,
                object_id: &header.object_id,
                part_no: 0,
                envelope: &envelope,
                reservation: None,
            },
        )?;
        Ok(())
    })
}
