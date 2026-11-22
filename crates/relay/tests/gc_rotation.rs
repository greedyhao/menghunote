//! GC 计划/完整性门禁与 key epoch 轮换屏障（ADR 0006 §7/§8/§13.3、§9 GC/epoch 矩阵）。

mod support;

use support::*;

use relay::checkpoints;
use relay::envelope::ObjectType;
use relay::error::{Error, ErrorCode};
use relay::log;
use relay::objects;
use relay::rotations::RotationState;

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

    let outcome = fx.activate_rotation(&rotation_id).expect("activate");
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
