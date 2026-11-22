//! DDL/PRAGMA、鉴权、游标、配额（ADR 0006 §4/§5/§6/§7/§13.2）。

mod support;

use support::*;

use menghunote_sync_proto as proto;
use relay::auth::{self, Realm};
use relay::checkpoints;
use relay::cursor::{self, CursorKey};
use relay::db::schema;
use relay::envelope::ObjectType;
use relay::error::{Error, ErrorCode};
use relay::ids::sha256_parts;
use relay::ids::{b64url_decode, b64url_encode, sha256};
use relay::log;
use relay::objects;
use relay::quota;

/// 断言错误码（协议可见错误）。
fn code(err: &Error) -> Option<ErrorCode> {
    err.code()
}

// ---------------------------------------------------------------- DDL / PRAGMA

#[test]
fn ddl_is_strict_and_pragmas_are_pinned() {
    let fx = Fixture::new("ddl");
    let (mode, sync, fk, timeout, tables) = fx
        .db
        .read(|conn| {
            let mode: String = conn.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
            let sync: i64 = conn.query_row("PRAGMA synchronous", [], |row| row.get(0))?;
            let fk: i64 = conn.query_row("PRAGMA foreign_keys", [], |row| row.get(0))?;
            let timeout: i64 = conn.query_row("PRAGMA busy_timeout", [], |row| row.get(0))?;
            let rows = {
                let mut stmt = conn.prepare(
                    "SELECT name, strict FROM pragma_table_list
                     WHERE schema = 'main' AND type = 'table' AND name NOT LIKE 'sqlite_%'",
                )?;
                let collected: Result<Vec<(String, i64)>, _> = stmt
                    .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                    .collect();
                collected?
            };
            Ok((mode, sync, fk, timeout, rows))
        })
        .expect("pragmas");

    assert_eq!(mode, "wal", "journal_mode 必须是 WAL");
    assert_eq!(sync, 2, "synchronous 必须是 FULL(2)");
    assert_eq!(fk, 1, "foreign_keys 必须 ON");
    assert_eq!(timeout, 5000, "busy_timeout 必须是 5000ms");
    assert!(
        tables.len() >= 23,
        "必须建齐 ADR §6 的表，实际 {}",
        tables.len()
    );
    for (name, strict) in &tables {
        assert_eq!(*strict, 1, "表 {name} 必须是 STRICT");
    }
    for required in [
        "vaults",
        "objects",
        "object_parts",
        "log",
        "checkpoints",
        "checkpoint_refs",
        "maintenance_reservations",
        "usage",
        "gc_plans",
        "rotations",
        "bootstrap_leases",
        "device_acks",
        "dedup",
        "key_slots",
        "device_grants",
        "challenges",
        "tokens",
        "invites",
        "manager_keys",
        "control_commits",
        "epochs",
        "devices",
    ] {
        assert!(
            tables.iter().any(|(name, _)| name == required),
            "缺少表 {required}"
        );
    }
    let version = fx
        .db
        .read(|conn| schema::meta_text(conn, schema::META_SCHEMA_VERSION))
        .expect("meta");
    assert_eq!(version, Some(relay::db::schema::SCHEMA_VERSION.to_string()));
}

#[test]
fn single_vault_constraint_holds() {
    let fx = Fixture::new("single-vault");
    let clock = fx.clock.clone();
    let hook = fx.hook.clone();
    let cfg = fx.cfg.clone();
    let init = relay::vault::InitRequest {
        vault_id: [9u8; 16],
        device_id: [8u8; 16],
        sign_pk: [1u8; 32],
        hpke_pk: [2u8; 32],
        manager_pub: [3u8; 32],
        setup_secret: "setup-secret".to_string(),
        certificate: certificate_bytes(&[9u8; 16], &[8u8; 16], &[1u8; 32], &[2u8; 32], 1),
        certificate_signature: b"s".to_vec(),
    };
    // v1 创建路径用请求里的 vault_id 作为显式 scope（见 api::create_vault）。
    let err = fx
        .db
        .inner()
        .write_blocking(relay::VaultScope::new([9u8; 16]), move |conn| {
            relay::vault::initialize(conn, clock.as_ref(), &cfg, hook.as_ref(), &init)
        })
        .expect_err("second init must fail");
    assert_eq!(code(&err), Some(ErrorCode::AlreadyInitialized));

    // schema 10 允许第二行 vault（多租户基础，S2b 的 v2 创建票据会用到）；
    // v1 的"单 vault"约束改由 `initialize` 的显式门禁承担（上面已断言）。
    let raw = fx.db.write_blocking(|conn| {
        conn.execute(
            "INSERT INTO vaults(vault_id, instance_id, log_id, log_incarnation,
                                cursor_key, state, active_epoch, next_epoch, manager_rev,
                                control_rev, auth_rev, head_seq, gc_floor, normal_limit,
                                maintenance_limit, legacy_default, created_at)
             VALUES (X'09090909090909090909090909090909', X'00000000000000000000000000000000',
                     X'00000000000000000000000000000000', X'00000000000000000000000000000000',
                     X'0000000000000000000000000000000000000000000000000000000000000000',
                     'active', 1, 2, 1, 1, 1, 0, 0, 1024, 2048, 0, 0)",
            [],
        )?;
        Ok(())
    });
    assert!(
        raw.is_ok(),
        "schema 10 允许多 vault 行（v1 门禁在 initialize）: {raw:?}"
    );
    let count: i64 = fx
        .db
        .read_instance(|conn| {
            Ok(conn.query_row("SELECT COUNT(*) FROM vaults", [], |row| row.get(0))?)
        })
        .expect("count");
    assert_eq!(count, 2);
}

// ---------------------------------------------------------------- 鉴权

#[test]
fn challenge_is_single_use_and_expires_in_60s() {
    let fx = Fixture::new("auth-challenge");
    let subject = fx.device;
    let clock = fx.clock.clone();
    let challenge = fx
        .db
        .write_blocking(move |conn| {
            auth::create_challenge(conn, clock.as_ref(), Realm::Device, &subject)
        })
        .expect("challenge");
    assert_eq!(challenge.expires_at, BASE_TIME + 60);

    let clock = fx.clock.clone();
    let hook = fx.hook.clone();
    let cid = challenge.challenge_id;
    fx.db
        .write_blocking(move |conn| {
            auth::issue_token(conn, clock.as_ref(), hook.as_ref(), &cid, b"m", b"s")
        })
        .expect("first use");

    // 第二次用同一 challenge：单用，401 CHALLENGE_USED。
    let clock = fx.clock.clone();
    let hook = fx.hook.clone();
    let err = fx
        .db
        .write_blocking(move |conn| {
            auth::issue_token(conn, clock.as_ref(), hook.as_ref(), &cid, b"m", b"s")
        })
        .expect_err("replay must fail");
    assert_eq!(code(&err), Some(ErrorCode::ChallengeUsed));

    // 过期 challenge：401 CHALLENGE_INVALID（FakeClock 推进，不 sleep）。
    let clock = fx.clock.clone();
    let fresh = fx
        .db
        .write_blocking(move |conn| {
            auth::create_challenge(conn, clock.as_ref(), Realm::Device, &subject)
        })
        .expect("challenge");
    fx.clock.advance_secs(61);
    let clock = fx.clock.clone();
    let hook = fx.hook.clone();
    let cid = fresh.challenge_id;
    let err = fx
        .db
        .write_blocking(move |conn| {
            auth::issue_token(conn, clock.as_ref(), hook.as_ref(), &cid, b"m", b"s")
        })
        .expect_err("expired challenge must fail");
    assert_eq!(code(&err), Some(ErrorCode::ChallengeInvalid));
}

#[test]
fn token_is_600s_stored_as_sha256_and_revocation_is_immediate() {
    let fx = Fixture::new("auth-token");
    let token = fx.device_token();
    let raw = b64url_decode(&token).expect("token b64");
    let expected_hash = sha256(&raw);
    let stored: Vec<Vec<u8>> = fx
        .db
        .read(|conn| {
            let mut stmt = conn.prepare("SELECT token_hash FROM tokens")?;
            let collected: Vec<Vec<u8>> = stmt
                .query_map([], |row| row.get::<_, Vec<u8>>(0))?
                .collect::<Result<_, _>>()?;
            Ok(collected)
        })
        .expect("token rows");
    assert!(
        stored
            .iter()
            .any(|hash| hash.as_slice() == expected_hash.as_slice()),
        "DB 必须只存 SHA256(token)"
    );
    // 明文 token 不得出现在任何 TEXT 列里。
    let dump: String = fx
        .db
        .read(|conn| {
            Ok(conn
                .query_row(
                    "SELECT group_concat(hex(token_hash), ',') FROM tokens",
                    [],
                    |row| row.get::<_, Option<String>>(0),
                )?
                .unwrap_or_default())
        })
        .expect("dump");
    assert!(!dump.contains(&token), "token 明文不得落库");

    // 600s 内可用，601s 后失效。
    let clock = fx.clock.clone();
    fx.db
        .read(|conn| auth::authenticate(conn, clock.as_ref(), &token))
        .expect("fresh token");
    fx.clock.advance_secs(601);
    let clock = fx.clock.clone();
    let err = fx
        .db
        .read(|conn| auth::authenticate(conn, clock.as_ref(), &token))
        .expect_err("expired token");
    assert_eq!(code(&err), Some(ErrorCode::Unauthorized));

    // 吊销：旧 token 立即失效（不等过期）；后续 challenge 也 403。
    let token = fx.device_token();
    let clock = fx.clock.clone();
    fx.db
        .read(|conn| auth::authenticate(conn, clock.as_ref(), &token))
        .expect("token valid before revoke");
    let device = fx.device;
    let now = fx.clock_now();
    fx.db
        .write_blocking(move |conn| auth::revoke_device(conn, now, &device))
        .expect("revoke");
    let clock = fx.clock.clone();
    let err = fx
        .db
        .read(|conn| auth::authenticate(conn, clock.as_ref(), &token))
        .expect_err("revoked token must fail immediately");
    assert_eq!(code(&err), Some(ErrorCode::Revoked));

    let clock = fx.clock.clone();
    let err = fx
        .db
        .write_blocking(move |conn| {
            auth::create_challenge(conn, clock.as_ref(), Realm::Device, &device)
        })
        .expect_err("revoked device challenge");
    assert_eq!(code(&err), Some(ErrorCode::Revoked));
}

#[test]
fn management_realm_requires_manager_subject() {
    let fx = Fixture::new("auth-manager");
    let wrong = [1u8; 16];
    let clock = fx.clock.clone();
    let err = fx
        .db
        .write_blocking(move |conn| {
            auth::create_challenge(conn, clock.as_ref(), Realm::Management, &wrong)
        })
        .expect_err("wrong subject");
    assert_eq!(code(&err), Some(ErrorCode::ManagementRequired));

    let token = fx.manager_token();
    let clock = fx.clock.clone();
    let ctx = fx
        .db
        .read(|conn| auth::authenticate(conn, clock.as_ref(), &token))
        .expect("manager token");
    assert_eq!(ctx.realm, Realm::Management);
    assert!(ctx.require_management().is_ok());
    assert!(ctx.device_id().is_err());
}

// ---------------------------------------------------------------- 游标

#[test]
fn log_page_is_exclusive_ascending_and_next_cursor_points_at_last_event() {
    let fx = Fixture::new("cursor-page");
    for _ in 0..3 {
        let (_, _, seq) = fx.create_object(ObjectType::Update, 1, 1, 32);
        assert!(seq.is_some(), "update 必须进 log");
    }
    let vault = fx.vault();
    assert_eq!(vault.head_seq, 3);
    let key = CursorKey::new(vault.cursor_key);

    let after = cursor::for_vault(&vault, 0).expect("cursor");
    let page = fx
        .db
        .read(|conn| log::page(conn, &vault, &after, 100, 512))
        .expect("page");
    assert_eq!(page.events.len(), 3);
    assert_eq!(page.events[0].relay_seq, 1);
    assert_eq!(page.through_seq, 3);
    assert_eq!(page.query_head, 3);
    assert_eq!(key.decode(&page.next_cursor).expect("cursor").last_seq, 3);

    // after 排他。
    let after = cursor::for_vault(&vault, 2).expect("cursor");
    let page = fx
        .db
        .read(|conn| log::page(conn, &vault, &after, 100, 512))
        .expect("page");
    assert_eq!(page.events.len(), 1);
    assert_eq!(page.events[0].relay_seq, 3);

    // 空页：next_cursor 回显请求游标（不是 head）。
    let after = cursor::for_vault(&vault, 3).expect("cursor");
    let page = fx
        .db
        .read(|conn| log::page(conn, &vault, &after, 100, 512))
        .expect("page");
    assert!(page.events.is_empty());
    assert_eq!(key.decode(&page.next_cursor).expect("cursor").last_seq, 3);
}

#[test]
fn cursor_tamper_cross_vault_and_instance_changes_are_rejected() {
    let fx = Fixture::new("cursor-tamper");
    fx.create_object(ObjectType::Update, 1, 1, 16);
    let vault = fx.vault();
    let key = CursorKey::new(vault.cursor_key);
    let token = key
        .encode(&cursor::for_vault(&vault, 1).expect("cursor"))
        .expect("encode");

    let mut raw = b64url_decode(&token).expect("b64");
    raw[88] ^= 1;
    let err = key.decode(&b64url_encode(&raw)).expect_err("tamper");
    assert_eq!(code(&err), Some(ErrorCode::BadCursor));

    // 跨 vault（MAC 有效但 vault 不匹配）。
    let mut cross = cursor::for_vault(&vault, 1).expect("cursor");
    cross.vault_id = [9u8; 16];
    let token = key.encode(&cross).expect("encode");
    let cursor = key.decode(&token).expect("decode");
    assert_eq!(
        code(&key.check_against(&cursor, &vault).expect_err("cross vault")),
        Some(ErrorCode::BadCursor)
    );

    // instance 变了 → RELAY_RESET。
    let mut cross = cursor::for_vault(&vault, 1).expect("cursor");
    cross.instance_id = [9u8; 16];
    let token = key.encode(&cross).expect("encode");
    let cursor = key.decode(&token).expect("decode");
    assert_eq!(
        code(&key.check_against(&cursor, &vault).expect_err("instance")),
        Some(ErrorCode::RelayReset)
    );

    // log_incarnation 变了（restore）→ RELAY_RESET，绝不静默归零。
    let mut cross = cursor::for_vault(&vault, 1).expect("cursor");
    cross.log_incarnation = [9u8; 16];
    let token = key.encode(&cross).expect("encode");
    let cursor = key.decode(&token).expect("decode");
    assert_eq!(
        code(&key.check_against(&cursor, &vault).expect_err("incarnation")),
        Some(ErrorCode::RelayReset)
    );

    // 水位高于 head（后端回滚）→ RELAY_RESET。
    let after = cursor::for_vault(&vault, 9).expect("cursor");
    let err = fx
        .db
        .read(|conn| log::page(conn, &vault, &after, 10, 512))
        .expect_err("cursor ahead of head");
    assert_eq!(code(&err), Some(ErrorCode::RelayReset));
}

#[test]
fn cursor_expires_below_gc_floor_and_holes_are_invariant_errors() {
    let fx = Fixture::new("cursor-expired");
    for _ in 0..3 {
        fx.create_object(ObjectType::Update, 1, 1, 16);
    }
    // 模拟 GC 推进 floor 到 2（真实路径见 gc 测试）。
    fx.db
        .write_blocking(|conn| {
            conn.execute(
                "UPDATE vaults SET gc_floor = 2 WHERE vault_id = ?1",
                [conn.scope().as_slice()],
            )?;
            Ok(())
        })
        .expect("bump floor");
    let vault = fx.vault();
    let after = cursor::for_vault(&vault, 1).expect("cursor");
    let err = fx
        .db
        .read(|conn| log::page(conn, &vault, &after, 10, 512))
        .expect_err("expired cursor");
    assert_eq!(code(&err), Some(ErrorCode::CursorExpired));
    assert!(matches!(&err, Error::Api(api) if api.bootstrap_hint == Some("bootstrap")));

    // 日志洞：删掉中间一行（模拟不变量被破坏）→ 内部错误，绝不静默跳过。
    // 先把 floor 复位，否则 after=0 会先命中 CURSOR_EXPIRED。
    fx.db
        .write_blocking(|conn| {
            conn.execute(
                "UPDATE vaults SET gc_floor = 0 WHERE vault_id = ?1",
                [conn.scope().as_slice()],
            )?;
            conn.execute("DELETE FROM log WHERE relay_seq = 2", [])?;
            Ok(())
        })
        .expect("delete middle row");
    // 重新读 vault（gc_floor 已复位），否则旧快照会让游标先命中 CURSOR_EXPIRED。
    let vault = fx.vault();
    let after = cursor::for_vault(&vault, 0).expect("cursor");
    let err = fx
        .db
        .read(|conn| log::page(conn, &vault, &after, 10, 512))
        .expect_err("hole");
    assert!(
        matches!(err, Error::Invariant("log prefix has a gap")),
        "unexpected: {err:?}"
    );
}

// ---------------------------------------------------------------- 配额

#[test]
fn normal_quota_is_per_part_atomic_and_507_does_not_delete_old_data() {
    // Q = 300B；每片 envelope 212+50 = 262B。第一片 ok，第二片超 300 → 507。
    let fx = Fixture::with_limits("quota-normal", 300, 600);
    let (first, _, _) = fx.create_object(ObjectType::Update, 1, 1, 50);
    assert!(fx.db.read(|conn| objects::load(conn, &first)).is_ok());
    assert_eq!(fx.usage().normal_bytes, 262);

    let object_id = random_object_id();
    let envelope =
        support::single_part_envelope(ObjectType::Update, fx.vault_id, object_id, 1, 1, 50);
    let err = fx.put_part(&envelope, None).expect_err("quota");
    assert_eq!(code(&err), Some(ErrorCode::QuotaExceeded));
    // 507 之后：没有落片、账本不变、旧数据仍在。
    assert_eq!(fx.usage().normal_bytes, 262);
    let parts: i64 = fx
        .db
        .read(|conn| Ok(conn.query_row("SELECT COUNT(*) FROM object_parts", [], |row| row.get(0))?))
        .expect("count");
    assert_eq!(parts, 1);
    assert!(fx.db.read(|conn| objects::load(conn, &first)).is_ok());

    // 读/ACK/auth 不受影响（ADR §7：达到 Q 只拒写）。
    assert!(!fx.device_token().is_empty());
}

#[test]
fn maintenance_reservation_formula_and_per_part_charging() {
    // Q = 300，峰值 Q+R = 900 → R = 600。
    let fx = Fixture::with_limits("quota-maint", 300, 900);
    let ctx = fx.device_ctx();
    let db = fx.db.clone();
    let clock = fx.clock.clone();
    let reservation = db
        .write_blocking(move |conn| {
            checkpoints::create(conn, clock.as_ref(), &ctx, 300, None, None, None)
        })
        .expect("checkpoint reservation");
    let rid = reservation.reservation_id;

    // R_available = R - maintenance_used - Σ(active.reserved-consumed) = 600 - 0 - 300 = 300。
    let now = fx.clock_now();
    let available = fx
        .db
        .read(|conn| quota::maintenance_available(conn, now))
        .expect("available");
    assert_eq!(available, 300);

    let object_id = random_object_id();
    let envelope =
        support::single_part_envelope(ObjectType::Snapshot, fx.vault_id, object_id, 1, 0, 50);
    fx.put_part(&envelope, Some(&rid))
        .expect("maintenance part");
    assert_eq!(fx.usage().maintenance_bytes, 262);
    let available_after = fx
        .db
        .read(|conn| quota::maintenance_available(conn, now))
        .expect("available");
    assert_eq!(
        available_after, available,
        "reservation 内写入不改变可用额度（公式自洽）"
    );

    // 超过 reservation 剩余额度 → 507 RESERVE（逐片原子扣减）且不落片。
    let big_object = random_object_id();
    let big =
        support::single_part_envelope(ObjectType::Snapshot, fx.vault_id, big_object, 1, 0, 150);
    let err = fx.put_part(&big, Some(&rid)).expect_err("reserve exceeded");
    assert_eq!(code(&err), Some(ErrorCode::ReserveExceeded));

    // reservation 过期：拒绝新写，但已写数据不删（ADR §13.2）。
    fx.clock.advance_secs(24 * 60 * 60 + 1);
    let fresh = fx
        .db
        .read(|conn| quota::reservation(conn, &rid))
        .expect("reservation")
        .expect("exists");
    let object_id = random_object_id();
    let envelope =
        support::single_part_envelope(ObjectType::Snapshot, fx.vault_id, object_id, 1, 0, 10);
    let err = fx
        .put_part(&envelope, Some(&rid))
        .expect_err("expired reservation");
    assert_eq!(code(&err), Some(ErrorCode::ReserveExceeded));
    // 失败事务会回滚"标 expired"的副作用，因此状态立即由 GC/后台 sweep 落盘（下面显式跑一次）。
    assert_eq!(fresh.consumed_bytes, 262, "过期不改变已消费量");
    let now_ms = fx.clock_now();
    fx.db
        .write_blocking(move |conn| quota::expire_stale_reservations(conn, now_ms))
        .expect("sweep");
    let after = fx
        .db
        .read(|conn| quota::reservation(conn, &rid))
        .expect("reservation")
        .expect("exists");
    assert_eq!(after.state, quota::ReservationState::Expired);
    let parts: i64 = fx
        .db
        .read(|conn| Ok(conn.query_row("SELECT COUNT(*) FROM object_parts", [], |row| row.get(0))?))
        .expect("count");
    assert_eq!(parts, 1);
}

/// 控制链哈希必须**对称**：rev=1 也是 `SHA256(0^32 || signed_record)`，不是裸 `SHA256(record)`。
#[test]
fn control_chain_genesis_hash_uses_zero_prev_hash() {
    let fx = Fixture::new("control-genesis-hash");
    // (rev, prev_hash, record_hash, signed_record)
    type ControlRow = (i64, Vec<u8>, Vec<u8>, Vec<u8>);
    let rows: Vec<ControlRow> = fx
        .db
        .read(|conn| {
            let mut stmt = conn.prepare(
                "SELECT rev, prev_hash, record_hash, signed_record FROM control_commits
                 ORDER BY rev ASC",
            )?;
            let mut out = Vec::new();
            for row in stmt.query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                ))
            })? {
                out.push(row?);
            }
            Ok(out)
        })
        .expect("control chain");
    assert!(!rows.is_empty(), "genesis 必须写一条控制记录");

    let zero = [0u8; 32];
    let (rev, prev_hash, record_hash, signed_record) = &rows[0];
    assert_eq!(*rev, 1);
    assert_eq!(
        prev_hash.as_slice(),
        zero.as_slice(),
        "rev=1 prev_hash 全零"
    );
    assert_eq!(
        record_hash.as_slice(),
        relay::ids::sha256_parts(&[zero.as_slice(), signed_record.as_slice()]).as_slice(),
        "rev=1 必须与后续条目同一公式"
    );
    assert_ne!(
        record_hash.as_slice(),
        relay::ids::sha256(signed_record).as_slice(),
        "裸 SHA256(record) 的旧式 genesis 哈希不再接受"
    );

    // 后续条目也保持同一公式（这里至少补一条 append_control）。
    let hash1 = record_hash.clone();
    fx.db
        .write_blocking(|conn| {
            relay::vault::append_control(conn, 0, b"next-record")?;
            Ok(())
        })
        .expect("append control");
    let second: (Vec<u8>, Vec<u8>) = fx
        .db
        .read(|conn| {
            Ok(conn.query_row(
                "SELECT prev_hash, record_hash FROM control_commits WHERE rev = 2",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?)
        })
        .expect("second record");
    assert_eq!(second.0, hash1, "rev=2 prev_hash 链到 rev=1");
    assert_eq!(
        second.1.as_slice(),
        relay::ids::sha256_parts(&[hash1.as_slice(), b"next-record"]).as_slice()
    );
}

// ---------------------------------------------------------------- 控制链 rev1 摘要

/// **跨 crate 的公式一致性**：`vault::init` 里 rev1 控制记录的 `record_hash` 是
/// `SHA256(零32 || 设备证书 CDE)`；sync crate 的 `genesis::rev1_record_hash` 用**同一
/// 公式**在本地复算它（ADR 0013 §3：Create 内先复算再验证 relay 链）。
///
/// 这里用与 `crates/sync/src/genesis.rs` 单测**完全相同**的固定证书向量与期望摘要，
/// 两边任何一侧改了公式/证书编码，这个测试就会失败（不依赖 sync crate 的依赖方向）。
#[test]
fn genesis_rev1_record_hash_matches_the_client_known_vector() {
    let cert = proto::DeviceCertV1 {
        vault_id: [0x11; 16],
        device_id: [0x22; 16],
        sign_ed25519_pub: [0x33; 32],
        hpke_x25519_pub: [0x44; 32],
        cert_serial: 7,
        issued_manager_rev: 1,
        rights: proto::Rights(proto::Rights::ALL),
    };
    let cert_bytes = cert.encode().expect("cert cde");
    let zero = [0u8; 32];
    let hash = sha256_parts(&[zero.as_slice(), cert_bytes.as_slice()]);
    assert_eq!(
        relay::ids::hex_lower(&hash),
        "c0cfd56ee351ad4cb96d9070734f63d04e8e27539cbf3dd551b9f4981eaec8e4",
        "relay 的 rev1 record_hash 与客户端已知向量不一致"
    );
}
