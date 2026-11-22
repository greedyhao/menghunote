//! GC 计划/完整性门禁与 key epoch 轮换屏障（ADR 0006 §7/§8/§13.3、§9 GC/epoch 矩阵）。

mod support;

use support::*;

use relay::checkpoints;
use relay::envelope::ObjectType;
use relay::error::{Error, ErrorCode};
use relay::ids::Id16;
use relay::log;
use relay::objects;
use relay::rotations::RotationState;
use relay::sig::{Expectation, SignatureCheck, SignatureHook};
use std::sync::Arc;

fn code(err: &Error) -> Option<ErrorCode> {
    err.code()
}

const DAY: i64 = 24 * 60 * 60;

#[test]
fn gc_plan_is_persistent_and_deadline_is_never_reset() {
    let fx = Fixture::new("gc-plan");
    // seq1 → C1(S=1)
    fx.create_object(ObjectType::Update, 1, 1, 32);
    let c1 = fx.publish_baseline();
    // seq2 → C2(S=2)：产生 plan(cut=1, eligible=now+7d)
    fx.create_object(ObjectType::Update, 1, 2, 32);
    let c2 = fx.publish_baseline();
    let plans = fx.gc_plans();
    assert_eq!(plans.len(), 1, "两 checkpoint 才产生计划: {plans:?}");
    assert_eq!(plans[0].1, 1);
    assert_eq!(plans[0].2, BASE_TIME + 7 * DAY);
    let deadline = plans[0].2;

    // 24h 后再发一个 pair：新计划出现，但**已有计划 deadline 不被重置**。
    fx.clock.advance_secs(DAY);
    fx.create_object(ObjectType::Update, 1, 3, 32);
    let c3 = fx.publish_baseline();
    let plans = fx.gc_plans();
    assert_eq!(plans.len(), 2, "{plans:?}");
    assert_eq!(
        plans[0].2, deadline,
        "后来的 checkpoint 不得重置已有计划的 7 天 deadline"
    );

    // 未到期：不删任何日志（cut=1 的计划在 base+7d 到期，但 cut=2 的到 base+8d）。
    fx.clock.advance_secs(5 * DAY);
    let outcome = fx.gc();
    assert!(!outcome.blocked);
    assert_eq!(outcome.deleted_log_rows, 0);
    assert_eq!(fx.vault().gc_floor, 0);

    // 到期（now = base + 6d + 2d = base+8d）：floor = min(max cut=2, 较旧基线 S=2) = 2。
    fx.clock.advance_secs(2 * DAY);
    let outcome = fx.gc();
    assert!(!outcome.blocked, "{outcome:?}");
    assert_eq!(outcome.cut_seq, 2);
    assert_eq!(outcome.floor, 2);
    assert_eq!(outcome.deleted_log_rows, 2);
    assert_eq!(outcome.retired_update_objects, 2);
    assert_eq!(fx.vault().gc_floor, 2);
    assert_eq!(fx.log_seqs(), vec![3], "只删前缀，seq3 仍在");

    // update 对象的幂等 tombstone 仍在（行保留、密文已删、可重试拿原 seq）。
    let retired: objects::ObjectRow = fx
        .db
        .read(|conn| {
            let mut stmt = conn.prepare(
                "SELECT object_id FROM objects WHERE type = 1 AND budget_class = 'retired'",
            )?;
            let ids: Vec<Vec<u8>> = stmt
                .query_map([], |row| row.get::<_, Vec<u8>>(0))?
                .collect::<std::result::Result<_, _>>()?;
            Ok(ids)
        })
        .expect("retired ids")
        .into_iter()
        .next()
        .map(|bytes| {
            let id: relay::ids::Hash32 = bytes.as_slice().try_into().expect("id");
            fx.db.read(|conn| objects::load(conn, &id)).expect("row")
        })
        .expect("retired object");
    assert_eq!(retired.budget_class, objects::BudgetClass::Retired);
    assert!(retired.digest.is_some(), "tombstone 保留摘要/签名元数据");
    // 重试 complete（原 commit）仍返回原 seq 且不再分配新日志。
    let head_before = fx.head_seq();
    let retry = fx
        .complete_with_commit(&retired.object_id, b"irrelevant-commit")
        .expect_err("retired object 已不可再 complete（tombstone 只认同原 commit）");
    assert!(matches!(
        code(&retry),
        Some(ErrorCode::IdConflict) | Some(ErrorCode::Incomplete)
    ));
    assert_eq!(fx.head_seq(), head_before);

    // 旧游标（< gc_floor）→ 410，不静默跳水位。
    let vault = fx.vault();
    let after = relay::cursor::for_vault(&vault, 1).expect("cursor");
    let err = fx
        .db
        .read(|conn| log::page(conn, &vault, &after, 10, 512))
        .expect_err("expired");
    assert_eq!(code(&err), Some(ErrorCode::CursorExpired));

    // 再次 GC：floor 不变，幂等。
    let again = fx.gc();
    assert_eq!(again.deleted_log_rows, 0);
    let _ = (c1, c2, c3);
}

#[test]
fn gc_blocks_on_corrupt_baseline_and_bootstrap_refuses_it() {
    let fx = Fixture::new("gc-corrupt");
    fx.create_object(ObjectType::Update, 1, 1, 32);
    let c1 = fx.publish_baseline();
    fx.create_object(ObjectType::Update, 1, 2, 32);
    let _c2 = fx.publish_baseline();
    fx.clock.advance_secs(8 * DAY);

    // 损坏 C1 引用的某个对象的分片密文（不改 digest 列 → §13.3 摘要校验必失败）。
    let refs = fx
        .db
        .read(|conn| checkpoints::refs(conn, &c1))
        .expect("refs");
    let (victim, _) = refs[0];
    let tampered = vec![0x11u8; 300];
    let object_id = victim;
    fx.raw_write(move |conn| {
        conn.execute(
            "UPDATE object_parts SET envelope = ?1 WHERE object_id = ?2 AND part_no = 0",
            (tampered.as_slice(), object_id.as_slice()),
        )?;
        Ok(())
    })
    .expect("corrupt part");

    let outcome = fx.gc();
    assert!(outcome.blocked, "损坏基线必须阻止 GC: {outcome:?}");
    assert!(!outcome.unhealthy.is_empty());
    assert_eq!(fx.vault().gc_floor, 0, "floor 绝不越过可恢复点");
    let checkpoint = fx.checkpoint_row(&c1);
    assert_eq!(
        checkpoint.health,
        checkpoints::Health::Unhealthy,
        "损坏 checkpoint 必须标 unhealthy"
    );

    // 只剩一份健康基线 → bootstrap 409 NO_VALID_BASELINE。
    let err = fx.bootstrap().expect_err("no valid baseline");
    assert_eq!(code(&err), Some(ErrorCode::NoValidBaseline));
}

#[test]
fn bootstrap_requires_two_baselines_and_lease_expiry_is_410() {
    let fx = Fixture::new("bootstrap-lease");
    fx.create_object(ObjectType::Update, 1, 1, 32);
    fx.publish_baseline();
    // 只有一份基线 → 409。
    let err = fx.bootstrap().expect_err("need two baselines");
    assert_eq!(code(&err), Some(ErrorCode::NoValidBaseline));

    fx.create_object(ObjectType::Update, 1, 2, 32);
    let c2 = fx.publish_baseline();
    let bootstrap = fx.bootstrap().expect("bootstrap");
    assert_eq!(bootstrap.primary.checkpoint_id, c2);
    assert_eq!(bootstrap.lease.floor_seq, 1);
    assert_eq!(bootstrap.head_seq, 2);

    // 续租：24h 内有效；过期后 410 LEASE_EXPIRED。
    let lease_id = bootstrap.lease.lease_id;
    let renewed = fx.renew_lease(&lease_id).expect("renew");
    assert!(renewed.expires_at > renewed.created_at);
    fx.clock.advance_secs(DAY + 1);
    let err = fx.renew_lease(&lease_id).expect_err("lease expired");
    assert_eq!(code(&err), Some(ErrorCode::LeaseExpired));
}

#[test]
fn rotation_freeze_blocks_old_epoch_writes_then_activate_switches_epoch() {
    let fx = Fixture::new("rotation-happy");
    // 冻结前写一条 update（<= S）。
    fx.create_object(ObjectType::Update, 1, 1, 32);
    let head_before_freeze = fx.head_seq();

    let view = fx.begin_rotation(&[]);
    assert_eq!(view.rotation.state, RotationState::Frozen);
    assert_eq!(view.rotation.from_epoch, 1);
    assert_eq!(view.rotation.to_epoch, 2);
    assert_eq!(view.rotation.freeze_seq, head_before_freeze);
    let rotation_id = view.rotation.rotation_id;
    assert_eq!(fx.vault().state, relay::vault::VaultState::Rotating);

    // 旧 epoch 追加一律 409（即便 S 之后刚解冻也不允许）。
    let object_id = random_object_id();
    let old_epoch =
        support::single_part_envelope_at(ObjectType::Update, fx.vault_id, object_id, 1, 2, 32, 1);
    let err = fx.put_part(&old_epoch, None).expect_err("frozen epoch");
    assert_eq!(code(&err), Some(ErrorCode::EpochMismatch));

    // 候选 epoch 的两份基线（走维护 reservation）+ 全成员 grant。
    let (candidate_a, candidate_b) = fx.publish_candidate_baselines(2);
    assert_ne!(candidate_a, candidate_b);
    // §8 步 5：候选 epoch 必须发布新槽（password + recovery）才能 activate。
    let reservation = fx.rotation_reservation(&rotation_id);
    fx.stage_candidate_slots(2, &reservation);
    fx.stage_grant(&rotation_id, &fx.device);
    let staged = fx.rotation_view(&rotation_id);
    assert_eq!(staged.rotation.state, RotationState::Staging);
    assert_eq!(staged.grants_count, 1);
    // activate 前：普通成员看不到候选 epoch 的 grant。
    assert!(
        fx.latest_grant(&fx.device).is_none(),
        "staged grant 激活前不可读"
    );

    // §27.2：缺 new_manager / 转换字段不匹配 / 缺签名 → 409，且绝不推进 manager_rev。
    let err = fx
        .activate_rotation_without_manager(&rotation_id)
        .expect_err("new_manager required");
    assert_eq!(code(&err), Some(ErrorCode::CasConflict));
    assert_eq!(fx.vault().manager_rev, 1, "必须未被推进");

    let row = fx.rotation_view(&rotation_id).rotation;
    let stale = relay::rotations::NewManager {
        transition: fx.manager_transition(&row, 0, 2, [9u8; 32]),
        old_signature: Some(b"old".to_vec()),
        new_signature: Some(b"new".to_vec()),
    };
    let err = fx
        .activate_rotation_with_manager(&rotation_id, stale)
        .expect_err("stale old_rev");
    assert_eq!(code(&err), Some(ErrorCode::CasConflict));
    assert_eq!(fx.vault().manager_rev, 1, "必须未被推进");

    // 篡改绑定字段（rotation_id 不变但 S 改一位）双签再对也不得通过。
    let mut tampered = fx.manager_transition(&row, 1, 2, [9u8; 32]);
    let mut decoded =
        relay::rotations::ManagerTransition::decode(&tampered).expect("decode transition");
    decoded.covered_seq = decoded.covered_seq.saturating_add(1);
    tampered = decoded.encode().expect("re-encode");
    let bad_bind = relay::rotations::NewManager {
        transition: tampered,
        old_signature: Some(b"old".to_vec()),
        new_signature: Some(b"new".to_vec()),
    };
    let err = fx
        .activate_rotation_with_manager(&rotation_id, bad_bind)
        .expect_err("S 绑定失败");
    assert_eq!(code(&err), Some(ErrorCode::CasConflict));
    assert_eq!(fx.vault().manager_rev, 1, "必须未被推进");

    let single_signed = relay::rotations::NewManager {
        transition: fx.manager_transition(&row, 1, 2, [9u8; 32]),
        old_signature: None,
        new_signature: None,
    };
    let err = fx
        .activate_rotation_with_manager(&rotation_id, single_signed)
        .expect_err("missing signatures");
    assert_eq!(code(&err), Some(ErrorCode::CasConflict));
    assert_eq!(fx.vault().manager_rev, 1, "必须未被推进");

    let outcome = fx.activate_rotation(&rotation_id).expect("activate");
    assert_eq!(fx.vault().manager_rev, 2, "双签通过后推进");
    let manager_row: (Vec<u8>, Option<Vec<u8>>, Option<Vec<u8>>) = fx
        .db
        .read(|conn| {
            Ok(conn.query_row(
                "SELECT transition, old_signature, new_signature FROM manager_keys WHERE rev = 2",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?)
        })
        .expect("manager keys rev2");
    assert!(!manager_row.0.is_empty(), "转换记录必须持久化");
    assert!(
        manager_row.1.is_some() && manager_row.2.is_some(),
        "双签必须持久化"
    );
    assert_eq!(outcome.epoch, 2);
    let vault = fx.vault();
    assert_eq!(vault.active_epoch, 2);
    assert_eq!(vault.state, relay::vault::VaultState::Active);

    // 旧 epoch 仍然 409；新 epoch 可以写。
    let err = fx
        .put_part(&old_epoch, None)
        .expect_err("old epoch after activate");
    assert_eq!(code(&err), Some(ErrorCode::EpochMismatch));
    let new_object = random_object_id();
    let new_epoch =
        support::single_part_envelope_at(ObjectType::Update, fx.vault_id, new_object, 1, 1, 32, 2);
    fx.put_part(&new_epoch, None).expect("new epoch write");

    // 最新 grant 指向新 epoch，且旧 epoch 被标 retired。
    let grant = fx.latest_grant(&fx.device).expect("grant");
    assert_eq!(grant.epoch, 2);
    assert_eq!(grant.signature.as_deref(), Some(b"signature".as_slice()));
    let epochs = fx
        .db
        .read(|conn| {
            let mut stmt = conn.prepare("SELECT epoch, state FROM epochs ORDER BY epoch")?;
            let rows: Vec<(u64, String)> = stmt
                .query_map([], |row| {
                    Ok((
                        u64::try_from(row.get::<_, i64>(0)?).expect("epoch"),
                        row.get::<_, String>(1)?,
                    ))
                })?
                .collect::<std::result::Result<_, _>>()?;
            Ok(rows)
        })
        .expect("epochs");
    assert!(epochs.contains(&(1, "retired".to_string())));
    assert!(epochs.contains(&(2, "active".to_string())));

    // 重复 activate → 409 INVALID_STATE（幂等但不能重放）。
    let err = fx
        .activate_rotation(&rotation_id)
        .expect_err("double activate");
    assert_eq!(code(&err), Some(ErrorCode::InvalidState));
}

#[test]
fn rotation_abort_after_revocation_never_unfreezes_old_epoch() {
    let fx = Fixture::new("rotation-abort");
    let second_device = [9u8; 16];
    fx.add_device(second_device);

    let view = fx.begin_rotation(&[second_device]);
    assert_eq!(view.rotation.revoked_count, 1);
    assert_eq!(fx.vault().state, relay::vault::VaultState::Rotating);
    // 被吊销设备立即不能认证。
    let err = fx
        .db
        .read(|conn| {
            relay::auth::create_challenge(
                conn,
                fx.clock.as_ref(),
                relay::auth::Realm::Device,
                &second_device,
            )
        })
        .expect_err("revoked");
    assert_eq!(code(&err), Some(ErrorCode::Revoked));

    let rotation_id = view.rotation.rotation_id;
    let aborted = fx.abort_rotation(&rotation_id).expect("abort");
    assert_eq!(aborted.rotation.state, RotationState::Aborted);
    // 关键：吊销过设备的轮换失败后，旧 epoch **不得**解冻（§8 步 8 / §10）。
    assert_eq!(fx.vault().state, relay::vault::VaultState::Rotating);
    let err = fx
        .activate_rotation(&rotation_id)
        .expect_err("aborted candidate cannot activate");
    assert_eq!(code(&err), Some(ErrorCode::InvalidState));
    // 候选 epoch/root 永不复用：epoch 2 被标 aborted，下一次重新随机到更高 epoch。
    let epoch2 = fx
        .db
        .read(|conn| relay::vault::epoch_state(conn, 2))
        .expect("epoch state");
    assert_eq!(epoch2, Some(relay::vault::EpochState::Aborted));

    let retry = fx.begin_rotation(&[]);
    assert_eq!(retry.rotation.to_epoch, 3, "重新开始必须分配更高 epoch");
    assert_eq!(retry.rotation.revoked_count, 0);

    // 精确复现绕过：第二次 rotation 本身没有吊销，但 abort 仍不得解冻旧 epoch。
    let retry_id = retry.rotation.rotation_id;
    let retry_aborted = fx.abort_rotation(&retry_id).expect("second abort");
    assert_eq!(retry_aborted.rotation.state, RotationState::Aborted);
    assert_eq!(
        fx.vault().state,
        relay::vault::VaultState::Rotating,
        "旧 epoch 冻结期间发生过吊销 → 永久不可解冻"
    );
    let old_epoch = support::single_part_envelope_at(
        ObjectType::Update,
        fx.vault_id,
        random_object_id(),
        1,
        2,
        32,
        1,
    );
    let err = fx
        .put_part(&old_epoch, None)
        .expect_err("old epoch still frozen");
    assert_eq!(code(&err), Some(ErrorCode::EpochMismatch));

    // 正常无吊销的 abort 仍能解冻（回归，单独 fixture 见下）。
}

#[test]
fn abort_without_revocation_still_unfreezes() {
    let fx = Fixture::new("rotation-abort-clean");
    fx.create_object(ObjectType::Update, 1, 1, 32);
    let view = fx.begin_rotation(&[]);
    assert_eq!(view.rotation.revoked_count, 0);
    fx.abort_rotation(&view.rotation.rotation_id)
        .expect("abort");
    assert_eq!(fx.vault().state, relay::vault::VaultState::Active);
    assert!(
        !fx.db
            .read(|conn| relay::vault::epoch_revoked_once(conn, 1))
            .expect("revoked_once"),
        "无吊销不得留持久事实"
    );
    // 解冻后旧 epoch 可以继续写。
    let object_id = random_object_id();
    let envelope =
        support::single_part_envelope_at(ObjectType::Update, fx.vault_id, object_id, 1, 2, 32, 1);
    fx.put_part(&envelope, None).expect("unfrozen write");
}

#[test]
fn v4_upgrade_backfills_revoked_once_after_abort() {
    let fx = Fixture::new("rotation-v4-backfill");
    let second = [6u8; 16];
    fx.add_device(second);
    let view = fx.begin_rotation(&[second]);
    fx.abort_rotation(&view.rotation.rotation_id)
        .expect("abort");
    // 模拟 v4 库：删掉新列并把版本降回 4，然后重开触发 v4→v5 迁移。
    fx.db
        .write_blocking(|conn| {
            conn.execute("ALTER TABLE epochs DROP COLUMN revoked_once", [])?;
            conn.execute(
                "UPDATE relay_meta SET value = ?1 WHERE key = 'schema_version'",
                ["4".as_bytes()],
            )?;
            Ok(())
        })
        .expect("downgrade to v4");
    let path = fx.into_db_path();
    let cleanup_path = path.clone();
    let clock = std::sync::Arc::new(relay::time::FakeClock::new(BASE_TIME));
    let db = relay::db::Db::open(relay::db::DbConfig::new(path), clock).expect("reopen");
    let vault = db.read(relay::vault::load).expect("vault");
    assert_eq!(vault.state, relay::vault::VaultState::Rotating);
    let backfilled = db
        .read(|conn| relay::vault::epoch_revoked_once(conn, 1))
        .expect("backfilled");
    assert!(backfilled, "升级必须回填旧库的吊销事实，不能清零");
    drop(db);
    support::cleanup_db_dir(&cleanup_path);
}

#[test]
fn revoked_epoch_fact_survives_restart() {
    let fx = Fixture::new("rotation-revoked-restart");
    let second = [7u8; 16];
    fx.add_device(second);
    let view = fx.begin_rotation(&[second]);
    fx.abort_rotation(&view.rotation.rotation_id)
        .expect("abort");

    let path = fx.into_db_path();
    let cleanup_path = path.clone();
    let clock = std::sync::Arc::new(relay::time::FakeClock::new(BASE_TIME));
    let db = relay::db::Db::open(relay::db::DbConfig::new(path), clock).expect("reopen");
    let vault = db.read(relay::vault::load).expect("vault");
    assert_eq!(vault.state, relay::vault::VaultState::Rotating);
    let revoked_once = db
        .read(|conn| relay::vault::epoch_revoked_once(conn, 1))
        .expect("revoked_once");
    assert!(revoked_once, "吊销事实必须重启后仍在");
    // 候选 epoch 已 aborted，下一次只能分配更高 epoch（不复用）。
    assert!(vault.next_epoch > view.rotation.to_epoch);
    drop(db);
    support::cleanup_db_dir(&cleanup_path);
}

#[test]
fn membership_change_after_candidate_grants_forces_new_epoch() {
    // §8 步 8：ROTATING 中若追加吊销且候选 grant 已为其生成 → 废弃候选 epoch。
    let fx = Fixture::new("rotation-membership");
    let second = [8u8; 16];
    fx.add_device(second);
    let view = fx.begin_rotation(&[]);
    let rotation_id = view.rotation.rotation_id;
    fx.stage_grant(&rotation_id, &second);
    // 追加吊销：abort + 新 epoch（不允许继续用同一新根）。
    fx.abort_rotation(&rotation_id).expect("abort");
    let next = fx.begin_rotation(&[second]);
    assert!(next.rotation.to_epoch > view.rotation.to_epoch);
    assert_eq!(next.rotation.revoked_count, 1);
    // 被吊销设备的 staged grant 不得残留给候选 epoch 用。
    let grants: i64 = fx
        .db
        .read(|conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM device_grants WHERE epoch = ?1",
                [i64::try_from(view.rotation.to_epoch).expect("epoch")],
                |row| row.get(0),
            )?)
        })
        .expect("grant count");
    assert_eq!(grants, 0, "废弃候选 epoch 的 grant 必须删除");
}

/// 只验证 grant 期望字段的 hook（用于证明 stage_grant 用 relay 事实构造绑定）。
#[derive(Debug)]
struct GrantBindingHook {
    recipient: Id16,
    epoch: u64,
    hpke_enc: [u8; 32],
    ciphertext_sha256: relay::ids::Hash32,
}

impl SignatureHook for GrantBindingHook {
    fn verify(&self, check: &SignatureCheck<'_>) -> relay::error::Result<()> {
        let Expectation::EpochGrant { grant } = check.expect else {
            return Ok(());
        };
        if grant.recipient_device_id != &self.recipient
            || grant.epoch != self.epoch
            || grant.hpke_enc != &self.hpke_enc
            || grant.ciphertext_sha256 != &self.ciphertext_sha256
        {
            return Err(relay::error::Error::SignatureRejected);
        }
        Ok(())
    }
}

fn stage_with_hook(
    fx: &Fixture,
    rotation: &Id16,
    recipient: &Id16,
    hook: Arc<GrantBindingHook>,
    signature: Vec<u8>,
) -> relay::error::Result<()> {
    let ctx = fx.manager_ctx();
    let rotation = *rotation;
    let recipient = *recipient;
    let clock = fx.clock.clone();
    let db = fx.db.clone();
    db.write_blocking(move |conn| {
        relay::rotations::stage_grant(
            conn,
            hook.as_ref(),
            &ctx,
            &rotation,
            &recipient,
            &relay::rotations::GrantInput {
                grant_id: [3u8; 16],
                hpke_enc: [1u8; 32],
                hpke_ciphertext: b"ciphertext",
                signed_header: b"aad||enc||hash",
                signature: &signature,
                manager_rev: 1,
            },
            clock.as_ref(),
        )
    })
    .map(|_| ())
}

#[test]
fn candidate_checkpoints_do_not_participate_in_gc_or_bootstrap() {
    let fx = Fixture::new("candidate-isolated");
    fx.create_object(ObjectType::Update, 1, 1, 32);
    let _c1 = fx.publish_baseline();
    let view = fx.begin_rotation(&[]);
    let rotation_id = view.rotation.rotation_id;
    let (a, b) = fx.publish_candidate_baselines(2);
    for cid in [a, b] {
        let state = fx
            .db
            .read(|conn| checkpoints::get(conn, &cid))
            .expect("checkpoint")
            .state;
        assert_eq!(
            state,
            checkpoints::CheckpointState::Candidate,
            "候选 epoch 只能 candidate"
        );
    }
    assert!(fx.gc_plans().is_empty(), "候选 checkpoint 不得产生 GC 计划");
    assert_eq!(fx.vault().gc_floor, 0);
    let bootstrap = fx.bootstrap().expect_err("candidate 不得 bootstrap");
    assert_eq!(code(&bootstrap), Some(ErrorCode::NoValidBaseline));

    // GC 会过期维护 reservation：先拿槽 reservation，再推进 8 天验证候选不产生计划。
    let reservation = fx.rotation_reservation(&rotation_id);
    fx.stage_candidate_slots(2, &reservation);
    let seqs_before = fx.log_seqs();
    fx.clock.advance_secs(8 * DAY);
    let outcome = fx.gc();
    assert_eq!(outcome.cut_seq, 0, "没有到期计划");
    assert_eq!(outcome.deleted_log_rows, 0);
    assert_eq!(fx.vault().gc_floor, 0);
    assert_eq!(
        fx.log_seqs(),
        seqs_before,
        "旧日志不得被候选 checkpoint 影响"
    );

    fx.stage_grant(&rotation_id, &fx.device);
    fx.activate_rotation(&rotation_id).expect("activate");
    for cid in [a, b] {
        let state = fx
            .db
            .read(|conn| checkpoints::get(conn, &cid))
            .expect("checkpoint")
            .state;
        assert_eq!(
            state,
            checkpoints::CheckpointState::Published,
            "activate promote"
        );
    }
    let plans = fx.gc_plans();
    assert_eq!(plans.len(), 1, "activate 后产生 7 天计划: {plans:?}");
    assert_eq!(plans[0].0, 2, "计划属于新 active epoch");
    assert_eq!(plans[0].2, fx.clock_now() + 7 * DAY);
    let lease = fx.bootstrap().expect("activate 后可 bootstrap");
    assert_eq!(lease.epoch, 2);
}

#[test]
fn abort_marks_candidate_checkpoints_aborted_and_unreusable() {
    let fx = Fixture::new("candidate-abort");
    fx.create_object(ObjectType::Update, 1, 1, 32);
    let view = fx.begin_rotation(&[]);
    let rotation_id = view.rotation.rotation_id;
    let (a, b) = fx.publish_candidate_baselines(2);
    fx.abort_rotation(&rotation_id).expect("abort");
    for cid in [a, b] {
        let state = fx
            .db
            .read(|conn| checkpoints::get(conn, &cid))
            .expect("checkpoint")
            .state;
        assert_eq!(state, checkpoints::CheckpointState::Aborted);
    }
    assert!(fx.gc_plans().is_empty());
    assert!(fx.bootstrap().is_err());
    assert_eq!(
        fx.db
            .read(|conn| relay::vault::epoch_state(conn, 2))
            .expect("epoch"),
        Some(relay::vault::EpochState::Aborted)
    );
    let next = fx.begin_rotation(&[]);
    assert!(next.rotation.to_epoch > 2, "aborted 候选 epoch 不复用");
}

#[test]
fn begin_bumps_auth_rev_even_without_revocations() {
    let fx = Fixture::new("begin-auth-rev");
    let token = fx.device_token();
    let before = fx.vault().auth_rev;
    fx.begin_rotation(&[]);
    assert!(fx.vault().auth_rev > before, "freeze 必须 bump auth_rev");
    let err = fx
        .db
        .read(|conn| relay::auth::authenticate(conn, fx.clock.as_ref(), &token))
        .expect_err("旧 token 必须失效");
    assert_eq!(code(&err), Some(ErrorCode::Revoked));
}

#[test]
fn stage_grant_binds_recipient_epoch_enc_and_ciphertext() {
    let fx = Fixture::new("grant-binding");
    let other = [5u8; 16];
    fx.add_device(other);
    let view = fx.begin_rotation(&[]);
    let rotation_id = view.rotation.rotation_id;
    let cipher_sha = relay::ids::sha256(b"ciphertext");
    let good = Arc::new(GrantBindingHook {
        recipient: other,
        epoch: 2,
        hpke_enc: [1u8; 32],
        ciphertext_sha256: cipher_sha,
    });
    // 错 recipient。
    let err = stage_with_hook(&fx, &rotation_id, &fx.device, good.clone(), vec![0x11; 64])
        .expect_err("recipient mismatch");
    assert_eq!(code(&err), Some(ErrorCode::ManagementRequired));
    // 错 epoch。
    let bad_epoch = Arc::new(GrantBindingHook {
        recipient: other,
        epoch: 3,
        hpke_enc: [1u8; 32],
        ciphertext_sha256: cipher_sha,
    });
    let err = stage_with_hook(&fx, &rotation_id, &other, bad_epoch, vec![0x11; 64])
        .expect_err("epoch mismatch");
    assert_eq!(code(&err), Some(ErrorCode::ManagementRequired));
    // 错 enc。
    let bad_enc = Arc::new(GrantBindingHook {
        recipient: other,
        epoch: 2,
        hpke_enc: [9u8; 32],
        ciphertext_sha256: cipher_sha,
    });
    let err = stage_with_hook(&fx, &rotation_id, &other, bad_enc, vec![0x11; 64])
        .expect_err("enc mismatch");
    assert_eq!(code(&err), Some(ErrorCode::ManagementRequired));
    // 错 ciphertext 摘要。
    let bad_cipher = Arc::new(GrantBindingHook {
        recipient: other,
        epoch: 2,
        hpke_enc: [1u8; 32],
        ciphertext_sha256: relay::ids::sha256(b"other-ciphertext"),
    });
    let err = stage_with_hook(&fx, &rotation_id, &other, bad_cipher, vec![0x11; 64])
        .expect_err("ciphertext mismatch");
    assert_eq!(code(&err), Some(ErrorCode::ManagementRequired));
    // 缺签名。
    let err = stage_with_hook(&fx, &rotation_id, &other, good.clone(), Vec::new())
        .expect_err("missing signature");
    assert_eq!(code(&err), Some(ErrorCode::ManagementRequired));
    // 正确绑定 + 签名持久化。
    stage_with_hook(&fx, &rotation_id, &other, good, vec![0x11; 64]).expect("valid grant");
    let stored: Vec<u8> = fx
        .db
        .read(|conn| {
            Ok(conn.query_row(
                "SELECT signature FROM device_grants WHERE epoch = 2 AND device_id = ?1",
                [other.as_slice()],
                |row| row.get(0),
            )?)
        })
        .expect("stored sig");
    assert_eq!(stored, vec![0x11; 64]);
}

#[test]
fn asset_registry_survives_log_gc_and_two_rotations() {
    let fx = Fixture::new("asset-registry");
    fx.create_object(ObjectType::Asset, 0, 0, 64);
    let asset_id = fx
        .db
        .read(|conn| {
            Ok(conn.query_row(
                "SELECT object_id FROM objects WHERE type = 4 ORDER BY rowid DESC LIMIT 1",
                [],
                |row| row.get::<_, Vec<u8>>(0),
            )?)
        })
        .expect("asset id");
    let asset_id: relay::ids::Hash32 = asset_id.as_slice().try_into().expect("asset id");
    // 资产在 epoch 1 冻结前完成。
    fx.create_object(ObjectType::Update, 1, 2, 32);
    let _c1 = fx.publish_baseline();
    fx.create_object(ObjectType::Update, 1, 3, 32);
    let _c2 = fx.publish_baseline();

    let listed = fx
        .db
        .read(|conn| relay::objects::list_assets(conn, 1, fx.head_seq(), None, 100))
        .expect("list assets");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].object_id, asset_id);

    // 表里没有真实 hash/MIME/byte_len。
    let columns: Vec<String> = fx
        .db
        .read(|conn| {
            let mut stmt = conn.prepare("PRAGMA table_info(asset_objects)")?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        })
        .expect("columns");
    for forbidden in ["real_sha256", "mime", "byte_len", "plain_len", "digest"] {
        assert!(
            !columns.iter().any(|column| column == forbidden),
            "asset_objects 不得存 {forbidden}: {columns:?}"
        );
    }

    // begin 冻结资产集合摘要，且与重算一致。
    let digest = fx
        .db
        .read(|conn| relay::objects::asset_set_digest(conn, 1, fx.head_seq()))
        .expect("digest");
    let view = fx.begin_rotation(&[]);
    let rotation_id = view.rotation.rotation_id;
    assert_eq!(
        fx.rotation_view(&rotation_id).rotation.asset_set_digest,
        Some(digest),
        "begin 必须冻结资产集合摘要"
    );

    // 完成第一次轮换 → epoch 2。
    let (a, b) = fx.publish_candidate_baselines(2);
    let _ = (a, b);
    let reservation = fx.rotation_reservation(&rotation_id);
    fx.stage_candidate_slots(2, &reservation);
    fx.stage_grant(&rotation_id, &fx.device);
    fx.activate_rotation(&rotation_id).expect("activate 2");

    // 第二次轮换 → epoch 3；epoch 1 的资产仍可枚举（不依赖 log）。
    let view2 = fx.begin_rotation(&[]);
    let rotation2 = view2.rotation.rotation_id;
    assert_eq!(view2.rotation.from_epoch, 2);
    let (_a2, _b2) = fx.publish_candidate_baselines(3);
    let reservation2 = fx.rotation_reservation(&rotation2);
    fx.stage_candidate_slots(3, &reservation2);
    fx.stage_grant(&rotation2, &fx.device);
    fx.activate_rotation(&rotation2).expect("activate 3");
    let listed_after = fx
        .db
        .read(|conn| relay::objects::list_assets(conn, 1, i64::MAX as u64, None, 100))
        .expect("list assets after rotations");
    assert_eq!(listed_after.len(), 1, "两轮轮换后 epoch1 资产仍完整");

    // GC 掉资产日志前缀后，登记表仍能枚举。
    fx.clock.advance_secs(8 * DAY);
    let outcome = fx.gc();
    assert!(!outcome.blocked, "{outcome:?}");
    assert!(outcome.deleted_log_rows > 0, "日志前缀应被回收");
    let listed_gc = fx
        .db
        .read(|conn| relay::objects::list_assets(conn, 1, i64::MAX as u64, None, 100))
        .expect("list assets after gc");
    assert_eq!(listed_gc.len(), 1, "log GC 后资产枚举不得丢失");
    assert_eq!(listed_gc[0].object_id, asset_id);
}

#[test]
fn retired_slot_is_unreadable_and_body_is_purged() {
    let fx = Fixture::new("slot-retire");
    let slot_id = fx.put_staging_slot(relay::slots::SlotKind::Password);
    // 设为 active → 可读。
    fx.db
        .write_blocking(move |conn| {
            conn.execute(
                "UPDATE key_slots SET state = 'active', retire_at = NULL WHERE slot_id = ?1",
                [slot_id.as_slice()],
            )?;
            Ok(())
        })
        .expect("activate slot row");
    assert!(
        fx.db
            .read(|conn| relay::slots::slot_object_readable(conn, &slot_id, BASE_TIME))
            .expect("readable active"),
        "active 槽可读"
    );
    // retiring 且未到期 → 仍可读。
    fx.db
        .write_blocking(move |conn| {
            conn.execute(
                "UPDATE key_slots SET state = 'retiring', retire_at = ?1 WHERE slot_id = ?2",
                (BASE_TIME + 10, slot_id.as_slice()),
            )?;
            Ok(())
        })
        .expect("retire slot");
    assert!(
        fx.db
            .read(|conn| relay::slots::slot_object_readable(conn, &slot_id, BASE_TIME))
            .expect("readable retiring"),
        "retiring 未到期仍可读"
    );
    // 到期：读判定 410 语义（不可读），GC 后行/body/parts 全删。
    fx.clock.advance_secs(11);
    let now = fx.clock_now();
    assert!(
        !fx.db
            .read(|conn| relay::slots::slot_object_readable(conn, &slot_id, now))
            .expect("expired"),
        "过期槽不可读"
    );
    let outcome = fx.gc();
    assert_eq!(outcome.purged_slots, 1);
    let rows: i64 = fx
        .db
        .read(|conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM key_slots WHERE slot_id = ?1",
                [slot_id.as_slice()],
                |row| row.get(0),
            )?)
        })
        .expect("slot rows");
    assert_eq!(rows, 0, "key_slots 行必须删除");
    let parts: i64 = fx
        .db
        .read(|conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM object_parts WHERE object_id = ?1",
                [slot_id.as_slice()],
                |row| row.get(0),
            )?)
        })
        .expect("parts");
    assert_eq!(parts, 0, "KEY_SLOT body/parts 必须删除");
    assert!(
        !fx.db
            .read(|conn| relay::slots::slot_object_readable(conn, &slot_id, now))
            .expect("gone"),
        "删行后即使知道 slot_id 也不可读"
    );
}

// ---------------- task-100：activate 独立性 / 只 promote 指名 cid / 旧 cp prune ----------------

/// 构造一个候选 epoch 的 checkpoint（含真实 CheckpointCommit），返回 cid。
fn candidate_ckpt(fx: &Fixture, epoch: u64) -> (Id16, Id16) {
    let outcome = fx.create_candidate_checkpoint(epoch);
    (outcome.checkpoint_id, outcome.reservation_id)
}

#[test]
fn activate_requires_named_candidates() {
    let fx = Fixture::new("activate-named");
    fx.create_object(ObjectType::Update, 1, 1, 32);
    let rotation_id = fx.begin_rotation(&[]).rotation.rotation_id;
    fx.publish_candidate_baselines(2);
    let reservation = fx.rotation_reservation(&rotation_id);
    fx.stage_candidate_slots(2, &reservation);
    fx.stage_grant(&rotation_id, &fx.device);

    // 不指名两个 cid → 409 INCOMPLETE（不能"自动挑两份"）。
    let err = fx
        .activate_rotation_unnamed(&rotation_id)
        .expect_err("named required");
    assert_eq!(code(&err), Some(ErrorCode::Incomplete));
    assert_eq!(fx.vault().active_epoch, 1, "不得切换 epoch");
}

#[test]
fn activate_rejects_non_independent_candidates() {
    let fx = Fixture::new("activate-independent");
    fx.create_object(ObjectType::Update, 1, 1, 32);
    let rotation_id = fx.begin_rotation(&[]).rotation.rotation_id;
    let (_, reservation) = candidate_ckpt(&fx, 2);
    // 多造几个候选 checkpoint（每个都用同一轮换 reservation 上传对象）。
    let shared_snapshot_a = fx.create_candidate_object(ObjectType::Snapshot, 1, 2, &reservation);
    let shared_manifest = fx.create_candidate_object(ObjectType::Manifest, 0, 2, &reservation);
    let manifest_a = fx.create_candidate_object(ObjectType::Manifest, 0, 2, &reservation);
    let manifest_b = fx.create_candidate_object(ObjectType::Manifest, 0, 2, &reservation);
    let snapshot_a = fx.create_candidate_object(ObjectType::Snapshot, 1, 2, &reservation);
    let snapshot_b = fx.create_candidate_object(ObjectType::Snapshot, 1, 2, &reservation);
    let valid_snapshot_a = fx.create_candidate_object(ObjectType::Snapshot, 1, 2, &reservation);
    let valid_snapshot_b = fx.create_candidate_object(ObjectType::Snapshot, 1, 2, &reservation);
    let valid_manifest_a = fx.create_candidate_object(ObjectType::Manifest, 0, 2, &reservation);
    let valid_manifest_b = fx.create_candidate_object(ObjectType::Manifest, 0, 2, &reservation);

    let (cp_shared_snapshot_a, _) = candidate_ckpt(&fx, 2);
    let (cp_shared_snapshot_b, _) = candidate_ckpt(&fx, 2);
    let (cp_shared_manifest_a, _) = candidate_ckpt(&fx, 2);
    let (cp_shared_manifest_b, _) = candidate_ckpt(&fx, 2);
    let (cp_valid_a, _) = candidate_ckpt(&fx, 2);
    let (cp_valid_b, _) = candidate_ckpt(&fx, 2);
    let (cp_extra, _) = candidate_ckpt(&fx, 2);

    // a) 两个 cid 的 SNAPSHOT refs 相同（manifest 不同）。
    fx.add_candidate_refs(cp_shared_snapshot_a, shared_snapshot_a, manifest_a);
    fx.add_candidate_refs(cp_shared_snapshot_b, shared_snapshot_a, manifest_b);
    // b) 两个 cid 的 manifest 相同（snapshot 不同）。
    fx.add_candidate_refs(cp_shared_manifest_a, snapshot_a, shared_manifest);
    fx.add_candidate_refs(cp_shared_manifest_b, snapshot_b, shared_manifest);
    // c) 完全独立的一对（正例）+ 一个未被指名的多余候选。
    fx.add_candidate_refs(cp_valid_a, valid_snapshot_a, valid_manifest_a);
    fx.add_candidate_refs(cp_valid_b, valid_snapshot_b, valid_manifest_b);
    fx.add_candidate_refs(cp_extra, valid_snapshot_a, valid_manifest_b);

    let covered = fx.checkpoint_row(&cp_valid_a).covered_seq;
    for cid in [
        cp_shared_snapshot_a,
        cp_shared_snapshot_b,
        cp_shared_manifest_a,
        cp_shared_manifest_b,
        cp_valid_a,
        cp_valid_b,
        cp_extra,
    ] {
        let manifest = match cid {
            _ if cid == cp_shared_snapshot_a => manifest_a,
            _ if cid == cp_shared_snapshot_b => manifest_b,
            _ if cid == cp_shared_manifest_a || cid == cp_shared_manifest_b => shared_manifest,
            _ if cid == cp_valid_a => valid_manifest_a,
            _ => valid_manifest_b,
        };
        fx.publish_candidate_checkpoint(cid, covered, manifest);
    }

    // a) 复用同一 snapshot → 409。
    let err = fx
        .activate_rotation_named(&rotation_id, cp_shared_snapshot_a, cp_shared_snapshot_b)
        .expect_err("shared snapshot");
    assert_eq!(code(&err), Some(ErrorCode::CasConflict));
    // b) 复用同一 manifest → 409。
    let err = fx
        .activate_rotation_named(&rotation_id, cp_shared_manifest_a, cp_shared_manifest_b)
        .expect_err("shared manifest");
    assert_eq!(code(&err), Some(ErrorCode::CasConflict));
    // c) 同一个 cid 指名两次 → 409。
    let err = fx
        .activate_rotation_named(&rotation_id, cp_valid_a, cp_valid_a)
        .expect_err("same cid");
    assert_eq!(code(&err), Some(ErrorCode::CasConflict));
    assert_eq!(fx.vault().active_epoch, 1, "被拒绝的 activate 不得切 epoch");
    // 被拒绝的候选没有 promote，也没有产生 GC 计划。
    for cid in [cp_shared_snapshot_a, cp_shared_manifest_a, cp_valid_a] {
        assert_eq!(
            fx.checkpoint_row(&cid).state,
            checkpoints::CheckpointState::Candidate
        );
    }
    assert!(fx.gc_plans().is_empty());

    // d) 正例：指名唯一独立的一对；多余候选作废，只有指名的两份 published。
    let reservation = fx.rotation_reservation(&rotation_id);
    fx.stage_candidate_slots(2, &reservation);
    fx.stage_grant(&rotation_id, &fx.device);
    let outcome = fx
        .activate_rotation_named(&rotation_id, cp_valid_a, cp_valid_b)
        .expect("activate named pair");
    assert_eq!(outcome.primary_checkpoint, cp_valid_a);
    assert_eq!(outcome.fallback_checkpoint, cp_valid_b);
    assert_eq!(
        fx.checkpoint_row(&cp_valid_a).role,
        Some(checkpoints::CheckpointRole::Primary)
    );
    assert_eq!(
        fx.checkpoint_row(&cp_valid_b).role,
        Some(checkpoints::CheckpointRole::Fallback)
    );
    assert_eq!(
        fx.checkpoint_row(&cp_extra).state,
        checkpoints::CheckpointState::Aborted,
        "未被指名的候选必须作废"
    );
    let row = fx.rotation_view(&rotation_id).rotation;
    assert_eq!(row.primary_checkpoint, Some(cp_valid_a));
    assert_eq!(row.fallback_checkpoint, Some(cp_valid_b));
    assert_eq!(row.activation_commit, Some(outcome.activation_commit));
    assert!(row.activated_at.is_some());
    let plans = fx.gc_plans();
    assert_eq!(
        plans.len(),
        1,
        "activate 为指名的 pair 建 7 天计划: {plans:?}"
    );
    let published: i64 = fx
        .db
        .read(|conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM checkpoints WHERE epoch = 2 AND state = 'published'",
                [],
                |row| row.get(0),
            )?)
        })
        .expect("published count");
    assert_eq!(published, 2, "只有被指名的两份 published");
}

#[test]
fn old_checkpoint_is_pruned_only_after_grace_and_without_lease() {
    let fx = Fixture::new("cp-prune");
    fx.create_object(ObjectType::Update, 1, 1, 32);
    let c1 = fx.publish_baseline();
    fx.create_object(ObjectType::Update, 1, 2, 32);
    let _c2 = fx.publish_baseline();
    // 一份覆盖 C2/C1 的 bootstrap lease（24h）——先把 C1/C2 变成"可被引用"。
    let lease = fx.bootstrap().expect("lease");
    assert_eq!(lease.fallback.as_ref().expect("fallback").checkpoint_id, c1);
    // 第三份基线让 C1 不再是"最新两份"。
    fx.create_object(ObjectType::Update, 1, 3, 32);
    let _c3 = fx.publish_baseline();
    // 模拟时间流逝：把 C1 的 published_at 回拨 8 天（它已不是最新两份）。
    let c1_for_sql = c1;
    fx.raw_write(move |conn| {
        conn.execute(
            "UPDATE checkpoints SET published_at = published_at - 8 * 86400 WHERE checkpoint_id = ?1",
            [c1_for_sql.as_slice()],
        )?;
        Ok(())
    })
    .expect("backdate c1");

    // lease 仍有效 → 即使过了 7 天也不得 prune。
    let outcome = fx.gc();
    assert_eq!(outcome.pruned_checkpoints, 0, "{outcome:?}");
    assert_eq!(
        fx.checkpoint_row(&c1).state,
        checkpoints::CheckpointState::Published,
        "有效 lease 引用中的 checkpoint 不得 prune"
    );

    // lease 过期（created+7d 上限或 24h TTL）→ prune 生效：清 refs、保留签名骨架。
    fx.clock.advance_secs(DAY + 1);
    let outcome = fx.gc();
    assert_eq!(outcome.pruned_checkpoints, 1, "{outcome:?}");
    let pruned = fx.checkpoint_row(&c1);
    assert_eq!(pruned.state, checkpoints::CheckpointState::Pruned);
    assert!(pruned.signed_commit.is_some(), "签名骨架必须保留");
    assert!(pruned.commit_signature.is_some());
    assert!(pruned.manifest_oid.is_some());
    assert!(pruned.refs_digest.is_some());
    let refs = fx
        .db
        .read(move |conn| checkpoints::refs(conn, &c1))
        .expect("refs");
    assert!(refs.is_empty(), "prune 必须清 refs: {refs:?}");
    // 被 prune 的 C1 属于**当前 active epoch**：只清 refs，不动密文（同 epoch 孤儿删除
    // 按评审 §一 10 后置；跨 epoch 旧副本在 epoch retired 后才清理）。
    assert_eq!(outcome.retired_pruned_objects, 0, "{outcome:?}");
}

#[test]
fn seven_day_cipher_gc_is_separate_from_fourteen_day_slot_ttl() {
    let fx = Fixture::new("gc-slot-separation");
    // 一个 active password 槽（本轮换前的老槽）。
    let old_slot = fx.put_staging_slot(relay::slots::SlotKind::Password);
    let old_slot_for_sql = old_slot;
    fx.raw_write(move |conn| {
        conn.execute(
            "UPDATE key_slots SET state = 'active', retire_at = NULL WHERE slot_id = ?1",
            [old_slot_for_sql.as_slice()],
        )?;
        Ok(())
    })
    .expect("activate old slot");

    // 老的 C1（active epoch 基线）必须在 freeze 之前发布。
    fx.create_object(ObjectType::Update, 1, 1, 32);
    let _c1 = fx.publish_baseline();
    let rotation_id = fx.begin_rotation(&[]).rotation.rotation_id;
    let (_a, _b) = fx.publish_candidate_baselines(2);
    let reservation = fx.rotation_reservation(&rotation_id);
    fx.stage_candidate_slots(2, &reservation);
    fx.stage_grant(&rotation_id, &fx.device);
    fx.activate_rotation(&rotation_id).expect("activate");

    // 14 天 - 1 秒：旧槽仍可读；7 天已在途中——7 天只影响旧密文/旧 cp，不影响槽。
    fx.clock.advance_secs(14 * DAY - 1);
    let outcome = fx.gc();
    assert_eq!(outcome.purged_slots, 0, "14 天期限未到，槽不得删");
    let slots = fx
        .db
        .read(|conn| relay::slots::list(conn, fx.clock_now()))
        .expect("slots");
    assert!(slots.iter().any(|slot| slot.slot_id == old_slot));

    // 到达 14 天：tick 真正删行 + body/parts（不依赖 GC 计划）。
    fx.clock.advance_secs(1);
    let outcome = fx.gc();
    assert!(outcome.purged_slots >= 1, "{outcome:?}");
    let slots = fx
        .db
        .read(|conn| relay::slots::list_management(conn, fx.clock_now()))
        .expect("slots");
    assert!(!slots.iter().any(|slot| slot.slot_id == old_slot));
}
