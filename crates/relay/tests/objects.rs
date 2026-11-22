//! 对象/分片流水线：短事务、幂等 ACK、并发 complete、崩溃点重放
//! （ADR 0006 §5/§6/§13.1、§9 重放/冲突/崩溃矩阵）。

mod support;

use support::*;

use relay::checkpoints;
use relay::envelope::{self, Header, ObjectType};
use relay::error::{Error, ErrorCode};
use relay::log;
use relay::objects;
use relay::time::Clock;
use relay::Id16;

fn code(err: &Error) -> Option<ErrorCode> {
    err.code()
}

fn part_count(fx: &Fixture) -> i64 {
    fx.db
        .read(|conn| Ok(conn.query_row("SELECT COUNT(*) FROM object_parts", [], |row| row.get(0))?))
        .expect("count")
}

fn object_count(fx: &Fixture) -> i64 {
    fx.db
        .read(|conn| Ok(conn.query_row("SELECT COUNT(*) FROM objects", [], |row| row.get(0))?))
        .expect("object count")
}

fn reservation_consumed(fx: &Fixture, reservation: &Id16) -> i64 {
    let reservation = *reservation;
    fx.db
        .read(move |conn| {
            Ok(conn.query_row(
                "SELECT consumed_bytes FROM maintenance_reservations
                 WHERE reservation_id = ?1 AND vault_id = ?2",
                (reservation.as_slice(), conn.scope().as_slice()),
                |row| row.get(0),
            )?)
        })
        .expect("reservation consumed")
}

/// 在 active epoch 建一份 checkpoint（拿到 purpose=checkpoint 的维护 reservation）。
fn create_active_checkpoint(fx: &Fixture) -> checkpoints::CreateOutcome {
    let ctx = fx.device_ctx();
    let db = fx.db.clone();
    let clock = fx.clock.clone();
    db.write_blocking(move |conn| {
        checkpoints::create(conn, clock.as_ref(), &ctx, 4096, None, None, None)
    })
    .expect("create checkpoint")
}

/// 两片 SNAPSHOT 信封（固定切片：第一片满 cap，第二片 64B）。
fn two_part_snapshot_envelopes(fx: &Fixture, object_id: relay::ids::Hash32) -> (Vec<u8>, Vec<u8>) {
    let cap = envelope::NON_ASSET_CAP_PLAIN;
    let total = cap + 64;
    let header = |part_index: u32, this_len: u64| Header {
        protocol_version: envelope::PROTOCOL_VERSION,
        object_type: ObjectType::Snapshot,
        cipher_suite: envelope::CIPHER_SUITE_XCHACHA20_POLY1305,
        flags: 0,
        vault_id: fx.vault_id,
        object_id,
        message_id: message_id_for(&object_id),
        producer_device_id: [2u8; 16],
        generation: 1,
        key_epoch: 1,
        sender_seq: 0,
        part_index,
        part_count: 2,
        total_plaintext_len: total,
        this_plaintext_len: u32::try_from(this_len).expect("this len"),
        ciphertext_len: u32::try_from(this_len + 16).expect("ct len"),
        context_hash: [0u8; 32],
        nonce: [9u8; 24],
    };
    let body = |len: u64| vec![0xAAu8; usize::try_from(len + 16).expect("body len")];
    (
        header(0, cap).encode(&body(cap)),
        header(1, 64).encode(&body(64)),
    )
}

fn log_count(fx: &Fixture) -> u64 {
    fx.db.read(log::count).expect("log count")
}

#[test]
fn incomplete_object_never_enters_log() {
    let fx = Fixture::new("objects-incomplete");
    let object_id = random_object_id();
    let envelope =
        support::single_part_envelope(ObjectType::Update, fx.vault_id, object_id, 1, 1, 64);

    // 先 complete（一片都没上传）→ 409 INCOMPLETE，log 必须为空。
    let digest = digest_of(std::slice::from_ref(&envelope)).expect("digest");
    let err = fx
        .complete_with_commit(&object_id, &commit_for(&digest))
        .expect_err("incomplete");
    assert_eq!(code(&err), Some(ErrorCode::Incomplete));
    assert_eq!(log_count(&fx), 0, "片没齐禁止进 log");
    assert_eq!(fx.head_seq(), 0, "也不得分配 relay_seq");

    // 上传该片后 complete 才能进 log（原子：complete + seq + log + 幂等 ACK）。
    fx.put_part(&envelope, None).expect("part");
    let outcome = fx.complete_object(&object_id, &envelope).expect("complete");
    assert_eq!(outcome.relay_seq, Some(1));
    assert!(!outcome.duplicate);
    assert_eq!(log_count(&fx), 1);
    assert_eq!(fx.head_seq(), 1);
    // 幂等 ACK 已落库。
    let dedup: i64 = fx
        .db
        .read(|conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM dedup WHERE scope = 'object-complete'",
                [],
                |row| row.get(0),
            )?)
        })
        .expect("dedup count");
    assert_eq!(dedup, 1);
}

#[test]
fn complete_is_idempotent_and_conflicts_are_rejected() {
    let fx = Fixture::new("objects-idempotent");
    let object_id = random_object_id();
    let envelope =
        support::single_part_envelope(ObjectType::Update, fx.vault_id, object_id, 1, 1, 64);
    fx.put_part(&envelope, None).expect("part");
    let digest = digest_of(std::slice::from_ref(&envelope)).expect("digest");
    let commit = commit_for(&digest);

    let first = fx.complete_with_commit(&object_id, &commit).expect("first");
    assert_eq!(first.relay_seq, Some(1));
    // 同 commit 字节重试（模拟"commit 后断响应"）→ 原 seq，不新分配。
    let retry = fx.complete_with_commit(&object_id, &commit).expect("retry");
    assert!(retry.duplicate);
    assert_eq!(retry.relay_seq, Some(1));
    assert_eq!(log_count(&fx), 1, "重试不得产生第二条 log");
    assert_eq!(fx.head_seq(), 1);

    // 同 object 不同 commit → 409 ID_CONFLICT（不可变）。
    let mut other = commit.clone();
    other.push(0xEE);
    let err = fx
        .complete_with_commit(&object_id, &other)
        .expect_err("different commit");
    assert_eq!(code(&err), Some(ErrorCode::IdConflict));

    // 同分片同字节重试 → 200 幂等（不重复计费）。
    let usage_before = fx.usage().normal_bytes;
    let again = fx.put_part(&envelope, None).expect("idempotent part");
    assert!(again.duplicate);
    assert_eq!(fx.usage().normal_bytes, usage_before);
    assert_eq!(part_count(&fx), 1);

    // 同分片不同字节 → 409 PART_CONFLICT。
    let mut tampered = envelope.clone();
    tampered[220] ^= 0xFF; // 改密文体，header 与摘要都会变
    let err = fx.put_part(&tampered, None).expect_err("part conflict");
    assert_eq!(code(&err), Some(ErrorCode::PartConflict));
    assert_eq!(part_count(&fx), 1);
}

#[test]
fn object_identity_is_immutable_across_producers_and_contexts() {
    let fx = Fixture::new("objects-identity");
    let object_id = random_object_id();
    let envelope =
        support::single_part_envelope(ObjectType::Update, fx.vault_id, object_id, 1, 1, 64);
    fx.put_part(&envelope, None).expect("part");

    // 不同 message_id / generation / sender_seq：同 object_id 的固定上下文必须完全一致。
    for (generation, sender_seq, plain_len) in [(1u64, 2u64, 64u64), (2, 1, 64), (1, 1, 32)] {
        let conflicting = support::single_part_envelope(
            ObjectType::Update,
            fx.vault_id,
            object_id,
            generation,
            sender_seq,
            plain_len,
        );
        // 只有 header 字段不同 → ID_CONFLICT（不允许"同 id 换内容"）。
        let err = fx.put_part(&conflicting, None).expect_err("id conflict");
        assert!(
            matches!(code(&err), Some(ErrorCode::IdConflict)),
            "generation={generation} seq={sender_seq} len={plain_len} 应冲突，实际 {err:?}"
        );
    }

    // 不同 producer（不是当前设备）→ 403 FORBIDDEN。
    let other_device = [3u8; 16];
    let mut header_bytes =
        support::single_part_envelope(ObjectType::Update, fx.vault_id, object_id, 1, 1, 64);
    header_bytes[76..92].copy_from_slice(&other_device);
    let err = fx.put_part(&header_bytes, None).expect_err("producer");
    assert_eq!(code(&err), Some(ErrorCode::Forbidden));
}

#[test]
fn concurrent_complete_yields_exactly_one_seq() {
    let fx = Fixture::new("objects-concurrent");
    let object_id = random_object_id();
    let envelope =
        support::single_part_envelope(ObjectType::Update, fx.vault_id, object_id, 1, 1, 64);
    fx.put_part(&envelope, None).expect("part");
    let digest = digest_of(std::slice::from_ref(&envelope)).expect("digest");
    let commit = commit_for(&digest);

    let results = std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for _ in 0..2 {
            let db = fx.db.clone();
            let clock = fx.clock.clone();
            let hook = fx.hook.clone();
            let ctx = fx.device_ctx();
            let vault_id = fx.vault_id;
            let commit = commit.clone();
            handles.push(scope.spawn(move || {
                db.write_blocking(move |conn| {
                    objects::complete(
                        conn,
                        clock.as_ref(),
                        hook.as_ref(),
                        &objects::CompleteUpload {
                            actor: &ctx,
                            vault_id: &vault_id,
                            object_id: &object_id,
                            commit: &commit,
                            signature: b"signature",
                        },
                    )
                })
            }));
        }
        handles
            .into_iter()
            .map(|handle| handle.join().expect("thread"))
            .collect::<Vec<_>>()
    });

    assert_eq!(results.len(), 2);
    let mut seqs = Vec::new();
    let mut duplicates = 0;
    for result in results {
        let outcome = result.expect("both complete calls succeed (one duplicate)");
        seqs.push(outcome.relay_seq.expect("seq"));
        if outcome.duplicate {
            duplicates += 1;
        }
    }
    assert_eq!(duplicates, 1, "并发只有一个真正 commit，另一个拿幂等 ACK");
    assert_eq!(seqs[0], seqs[1]);
    assert_eq!(log_count(&fx), 1, "并发不得产生两条 log");
    assert_eq!(fx.head_seq(), 1);
}

#[test]
fn cursor_after_commit_gives_stable_delivery_order() {
    let fx = Fixture::new("objects-order");
    let mut seqs = Vec::new();
    for index in 0..4u64 {
        let (_, _, seq) = fx.create_object(ObjectType::Update, 1, index + 1, 32);
        seqs.push(seq.expect("update seq"));
    }
    assert_eq!(seqs, vec![1, 2, 3, 4]);
    let timeline = fx.db.read(log::timeline).expect("timeline");
    assert_eq!(timeline.len(), 4);
    assert!(
        timeline.windows(2).all(|pair| pair[1].0 == pair[0].0 + 1),
        "日志前缀必须连续"
    );
}

#[test]
fn crash_replay_after_parts_and_after_complete() {
    // 崩溃点 1：parts 已入库、complete 之前进程重启 → 重放 complete 成功。
    let fx = Fixture::new("objects-crash");
    let object_id = random_object_id();
    let envelope =
        support::single_part_envelope(ObjectType::Update, fx.vault_id, object_id, 1, 1, 64);
    fx.put_part(&envelope, None).expect("part");
    let digest = digest_of(std::slice::from_ref(&envelope)).expect("digest");
    let commit = commit_for(&digest);

    // "重启"：关闭 writer 线程，重新打开同一个库（同一 FakeClock）。
    let vault_id = fx.vault_id;
    let path = fx.into_db_path();
    let cleanup_path = path.clone();
    let clock = std::sync::Arc::new(relay::time::FakeClock::new(BASE_TIME));
    let db = relay::db::Db::open(relay::db::DbConfig::new(path), clock.clone()).expect("reopen");
    let db = TestDb::new(db, relay::VaultScope::new(vault_id));
    let vault = db.read(relay::vault::load).expect("vault");
    let ctx = relay::auth::AuthContext {
        realm: relay::auth::Realm::Device,
        subject: [2u8; 16],
        device_id: Some([2u8; 16]),
        auth_rev: vault.auth_rev,
        authz_rev: 0,
        authority_rev: 0,
        cap_id: None,
        vault: relay::VaultScope::new(vault_id),
    };
    let hook = std::sync::Arc::new(TestHook::new());
    let vault_id = vault.vault_id;
    let outcome = {
        let clock = clock.clone();
        let hook = hook.clone();
        let commit = commit.clone();
        db.write_blocking(move |conn| {
            objects::complete(
                conn,
                clock.as_ref(),
                hook.as_ref(),
                &objects::CompleteUpload {
                    actor: &ctx,
                    vault_id: &vault_id,
                    object_id: &object_id,
                    commit: &commit,
                    signature: b"signature",
                },
            )
        })
    }
    .expect("complete after restart");
    assert_eq!(outcome.relay_seq, Some(1));

    // 崩溃点 2：complete 已提交但响应"丢失" → 再重试拿原 seq，不新分配。
    let again = {
        let clock = clock.clone();
        let hook = hook.clone();
        let commit = commit.clone();
        db.write_blocking(move |conn| {
            objects::complete(
                conn,
                clock.as_ref(),
                hook.as_ref(),
                &objects::CompleteUpload {
                    actor: &ctx,
                    vault_id: &vault_id,
                    object_id: &object_id,
                    commit: &commit,
                    signature: b"signature",
                },
            )
        })
    }
    .expect("retry after lost response");
    assert!(again.duplicate);
    assert_eq!(again.relay_seq, Some(1));
    let head: u64 = db
        .read(|conn| {
            let value: i64 = conn.query_row(
                "SELECT head_seq FROM vaults WHERE vault_id = ?1",
                [conn.scope().as_slice()],
                |row| row.get(0),
            )?;
            Ok(u64::try_from(value).expect("head"))
        })
        .expect("head");
    assert_eq!(head, 1, "重放不得把 head_seq 推高");
    assert_eq!(clock.now_secs(), BASE_TIME);
    drop(db);
    support::cleanup_db_dir(&cleanup_path);
}

// ---------------------------------------------------------------------------
// task-121：同字节跨 reservation 幂等复用
// ---------------------------------------------------------------------------

/// 普通预算对象与 checkpoint 维护对象互相同字节重放：都返回 duplicate，不重复计费，
/// 不新增对象/分片；不同字节仍然冲突。
#[test]
fn same_bytes_replay_is_idempotent_across_normal_and_checkpoint_reservations() {
    let fx = Fixture::new("objects-cross-reservation");
    let checkpoint = create_active_checkpoint(&fx);
    let reservation = checkpoint.reservation_id;

    // 普通预算对象（reservation=None）→ checkpoint reservation 重放。
    let object_id = random_object_id();
    let envelope = single_part_envelope(ObjectType::Asset, fx.vault_id, object_id, 0, 0, 64);
    let first = fx.put_part(&envelope, None).expect("normal part");
    assert!(!first.duplicate);
    let usage = fx.usage();
    let objects = object_count(&fx);
    let consumed_before = reservation_consumed(&fx, &reservation);

    let replay = fx
        .put_part(&envelope, Some(&reservation))
        .expect("normal→checkpoint 同字节重放必须成功");
    assert!(replay.duplicate);
    assert_eq!(replay.object_id, object_id);
    assert_eq!(fx.usage(), usage, "幂等重放不得重复计费");
    assert_eq!(object_count(&fx), objects, "对象只存一份");
    assert_eq!(part_count(&fx), 1, "分片只存一份");
    assert_eq!(
        reservation_consumed(&fx, &reservation),
        consumed_before,
        "重放不得消耗 checkpoint reservation"
    );

    // checkpoint 维护对象 → 普通预算重放（反向）。
    let object_id2 = random_object_id();
    let envelope2 = single_part_envelope(ObjectType::Snapshot, fx.vault_id, object_id2, 1, 0, 64);
    fx.put_part(&envelope2, Some(&reservation))
        .expect("maintenance part");
    let usage_after_maintenance = fx.usage();
    assert!(usage_after_maintenance.maintenance_bytes > 0);
    let replay2 = fx
        .put_part(&envelope2, None)
        .expect("checkpoint→normal 同字节重放必须成功");
    assert!(replay2.duplicate);
    assert_eq!(
        fx.usage(),
        usage_after_maintenance,
        "反向重放也不得重复计费"
    );
    assert_eq!(object_count(&fx), objects + 1);
    assert_eq!(part_count(&fx), 2);

    // 不同字节即便换 reservation 也必须冲突。
    let mut tampered = envelope.clone();
    let last = tampered.len() - 1;
    tampered[last] ^= 0xFF;
    let err = fx
        .put_part(&tampered, Some(&reservation))
        .expect_err("不同字节必须拒绝");
    assert_eq!(code(&err), Some(ErrorCode::PartConflict));
    assert_eq!(part_count(&fx), 2, "冲突不得写入分片");
}

/// 轮换期用 rotation reservation 写入的候选 ASSET，激活后由 ordinary checkpoint 的
/// checkpoint reservation 重放同一 `object_id`/同字节：success（duplicate），不再
/// `409 ID_CONFLICT`。
#[test]
fn rotation_reservation_object_is_reusable_by_a_checkpoint_reservation() {
    let fx = Fixture::new("objects-rotation-reuse");
    let rotation = fx.begin_rotation(&[]).rotation.rotation_id;
    let (_primary, _fallback) = fx.publish_candidate_baselines(2);
    let rotation_reservation = fx.rotation_reservation(&rotation);

    let object_id = random_object_id();
    let envelope = single_part_envelope_at(ObjectType::Asset, fx.vault_id, object_id, 0, 0, 64, 2);
    let first = fx
        .put_part(&envelope, Some(&rotation_reservation))
        .expect("rotation part");
    assert!(!first.duplicate);

    fx.stage_candidate_slots(2, &rotation_reservation);
    fx.stage_grant(&rotation, &fx.device);
    fx.activate_rotation(&rotation).expect("activate");
    assert_eq!(fx.vault().active_epoch, 2);

    let checkpoint = create_active_checkpoint(&fx);
    let checkpoint_reservation = checkpoint.reservation_id;
    assert_ne!(
        checkpoint_reservation, rotation_reservation,
        "激活后 checkpoint 必须换用新的 checkpoint reservation"
    );

    let usage = fx.usage();
    let objects = object_count(&fx);
    let parts = part_count(&fx);
    let consumed_before = reservation_consumed(&fx, &checkpoint_reservation);
    let replay = fx
        .put_part(&envelope, Some(&checkpoint_reservation))
        .expect("rotation→checkpoint 同字节重放必须成功，而不是 409 ID_CONFLICT");
    assert!(replay.duplicate);
    assert_eq!(fx.usage(), usage, "不得重复计费");
    assert_eq!(object_count(&fx), objects, "对象只存一份");
    assert_eq!(part_count(&fx), parts, "分片只存一份");
    assert_eq!(
        reservation_consumed(&fx, &checkpoint_reservation),
        consumed_before,
        "重放不得消耗新的 checkpoint reservation"
    );

    // 同一对象的不同字节仍拒绝（不允许借幂等路径替换密文）。
    let mut tampered = envelope.clone();
    let last = tampered.len() - 1;
    tampered[last] ^= 0x7F;
    let err = fx
        .put_part(&tampered, Some(&checkpoint_reservation))
        .expect_err("不同字节必须冲突");
    assert_eq!(code(&err), Some(ErrorCode::PartConflict));
}

/// 补**缺失**分片时仍必须沿用对象既有预算来源：同字节重放放行，但换个 reservation
/// 续传剩余片仍然 409（避免对象的 reservation 消耗与实际字节脱节）。
#[test]
fn missing_part_across_reservations_still_requires_the_same_budget_source() {
    let fx = Fixture::with_limits("objects-mixed-source", 4 * 1024 * 1024, 8 * 1024 * 1024);
    let checkpoint = create_active_checkpoint(&fx);
    let reservation = checkpoint.reservation_id;

    let object_id = random_object_id();
    let (part0, part1) = two_part_snapshot_envelopes(&fx, object_id);
    fx.put_part(&part0, None).expect("part 0");

    // part 1 尚未落库 → 换预算来源补齐仍拒绝。
    let err = fx
        .put_part(&part1, Some(&reservation))
        .expect_err("缺失分片不得换预算来源");
    assert_eq!(code(&err), Some(ErrorCode::IdConflict));

    // 同来源补齐成功；随后两片都可以跨 reservation 幂等重放。
    fx.put_part(&part1, None).expect("part 1 same source");
    let usage = fx.usage();
    let parts = part_count(&fx);
    let replay0 = fx
        .put_part(&part0, Some(&reservation))
        .expect("replay part 0");
    let replay1 = fx
        .put_part(&part1, Some(&reservation))
        .expect("replay part 1");
    assert!(replay0.duplicate && replay1.duplicate);
    assert_eq!(fx.usage(), usage);
    assert_eq!(part_count(&fx), parts);
}
