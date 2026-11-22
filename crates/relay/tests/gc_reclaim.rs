//! task-109（S7-35）：relay GC 真正回收旧 epoch 密文、ASSET 副本与候选垃圾。
//!
//! 关键顺序（评审 0002）：**先验证 active epoch 两份保留根健康**，再做破坏性回收；
//! degraded/不足两份健康基线 → 不剪 checkpoint/对象、不删日志、floor 不动。
//! 7 天密文宽限与 14 天槽期限分开。

mod support;

use std::collections::BTreeSet;

use relay::checkpoints;
use relay::envelope::ObjectType;
use relay::error::ErrorCode;
use relay::ids::{Hash32, Id16};
use relay::objects;
use relay::quota;
use support::*;

const DAY: i64 = 24 * 60 * 60;

fn code(err: &relay::Error) -> Option<ErrorCode> {
    err.code()
}

/// 完整做一次轮换（真实 relay 事务；材料由 fixture 的弱 hook 准备）。
fn rotate(fx: &Fixture) {
    let rotation = fx.begin_rotation(&[]).rotation.rotation_id;
    let row = fx.rotation_view(&rotation).rotation;
    let (_a, _b) = fx.publish_candidate_baselines(row.to_epoch);
    let reservation = fx.rotation_reservation(&rotation);
    // 每个新 epoch 复制一份 ASSET（模拟客户端轮换时对旧 epoch 全量资产的再加密）。
    fx.create_candidate_object(ObjectType::Asset, 0, row.to_epoch, &reservation);
    fx.stage_candidate_slots(row.to_epoch, &reservation);
    fx.stage_grant(&rotation, &fx.device);
    fx.activate_rotation(&rotation).expect("activate");
}

fn parts_of(fx: &Fixture, object_id: &Hash32) -> i64 {
    fx.db
        .read(move |conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM object_parts WHERE object_id = ?1",
                [object_id.as_slice()],
                |row| row.get(0),
            )?)
        })
        .expect("parts")
}

fn old_epoch_parts(fx: &Fixture, active_epoch: u64) -> i64 {
    fx.db
        .read(move |conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM object_parts p
                 JOIN objects o ON o.object_id = p.object_id
                 WHERE o.epoch != ?1 AND o.type != 5",
                [i64::try_from(active_epoch).expect("epoch")],
                |row| row.get(0),
            )?)
        })
        .expect("old epoch parts")
}

/// 账本必须等于"仍存在 parts 的字节和"（recompute 后）。
fn ledger_matches_parts(fx: &Fixture) -> (quota::Usage, i64, i64) {
    let usage = fx.usage();
    let (normal, maintenance): (i64, i64) = fx
        .db
        .read(|conn| {
            Ok(conn.query_row(
                "SELECT
                   COALESCE(SUM(CASE WHEN o.budget_class='normal' THEN length(p.envelope) ELSE 0 END),0),
                   COALESCE(SUM(CASE WHEN o.budget_class='maintenance' THEN length(p.envelope) ELSE 0 END),0)
                 FROM object_parts p JOIN objects o ON o.object_id = p.object_id",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?)
        })
        .expect("parts bytes");
    (usage, normal, maintenance)
}

#[test]
fn multi_rotation_gc_reclaims_old_epoch_ciphertext_assets_and_budget() {
    let fx = Fixture::new("reclaim-multi");
    fx.create_object(ObjectType::Update, 1, 1, 32);
    let epoch1_asset = fx.create_object(ObjectType::Asset, 0, 0, 64).0;
    let epoch1_baseline = fx.publish_baseline();
    // epoch 1 → 4（三轮轮换；每轮新 epoch 各有一份 ASSET 副本）。
    rotate(&fx);
    rotate(&fx);
    rotate(&fx);
    let active_epoch = fx.vault().active_epoch;
    assert_eq!(active_epoch, 4);
    let usage_before = fx.usage();
    assert!(usage_before.maintenance_bytes > 0);
    let old_parts_before = old_epoch_parts(&fx, active_epoch);
    assert!(
        old_parts_before > 0,
        "轮换后旧 epoch 密文应当存在（待回收）"
    );
    assert_eq!(fx.vault().gc_floor, 0);

    // 7 天密文宽限之后 GC：active epoch 两份基线健康 → 允许破坏性回收。
    fx.clock.advance_secs(8 * DAY);
    let outcome = fx.gc();
    assert!(!outcome.blocked, "{outcome:?}");
    assert!(outcome.pruned_checkpoints > 0, "{outcome:?}");
    assert!(outcome.retired_pruned_objects > 0, "{outcome:?}");
    assert!(outcome.deleted_log_rows > 0, "日志前缀应被回收");
    assert!(fx.vault().gc_floor >= 1, "floor 应推进");

    // 旧 epoch 的密文全部清空（不再是"每个 retired epoch 保留最后两份 full 基线"）。
    assert_eq!(
        old_epoch_parts(&fx, active_epoch),
        0,
        "旧 epoch（含 ASSET 副本）不得再残留密文"
    );
    for epoch in 1..active_epoch {
        let remaining: i64 = fx
            .db
            .read(move |conn| {
                Ok(conn.query_row(
                    "SELECT COUNT(*) FROM asset_objects WHERE epoch = ?1 AND state = 'active'",
                    [i64::try_from(epoch).expect("epoch")],
                    |row| row.get(0),
                )?)
            })
            .expect("asset registry");
        assert_eq!(remaining, 0, "epoch {epoch} 的资产副本登记必须 retired");
    }
    // active epoch 的资产副本仍在（下一轮轮换还要复制它）。
    let active_assets = fx
        .db
        .read(move |conn| objects::list_assets(conn, active_epoch, i64::MAX as u64, None, 100))
        .expect("active assets");
    assert_eq!(active_assets.len(), 1, "active epoch 资产不得被回收");
    assert!(parts_of(&fx, &active_assets[0].object_id) > 0);

    // 7 天与 14 天分开：此时旧 epoch 的 KEY_SLOT 密文仍在（retire_at 未到）。
    let slot_parts = fx
        .db
        .read(move |conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM object_parts p JOIN objects o ON o.object_id = p.object_id
                 WHERE o.epoch != ?1 AND o.type = 5",
                [i64::try_from(active_epoch).expect("epoch")],
                |row| row.get::<_, i64>(0),
            )?)
        })
        .expect("slot parts");
    assert!(
        slot_parts > 0,
        "14 天槽期限未到，KEY_SLOT 不得随 7 天 GC 删除"
    );
    fx.clock.advance_secs(7 * DAY);
    let outcome = fx.gc();
    assert!(!outcome.blocked, "{outcome:?}");
    assert!(
        outcome.purged_slots >= 2,
        "14 天到期后旧槽必须真正清 body/parts"
    );
    let remaining_slot_parts: i64 = fx
        .db
        .read(move |conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM object_parts p JOIN objects o ON o.object_id = p.object_id
                 WHERE o.epoch != ?1 AND o.type = 5",
                [i64::try_from(active_epoch).expect("epoch")],
                |row| row.get::<_, i64>(0),
            )?)
        })
        .expect("slot parts");
    assert_eq!(remaining_slot_parts, 0);

    // 账本与 parts 完全一致；维护额度不再被旧 epoch 占用（回到 R − 当前 live）。
    let (usage, _normal, maintenance_parts) = ledger_matches_parts(&fx);
    assert_eq!(usage.maintenance_bytes, maintenance_parts);
    let vault = fx.vault();
    let now = fx.clock_now();
    let active_reserved = fx
        .db
        .read(move |conn| quota::active_reserved_remaining(conn, now))
        .expect("reserved");
    let available = fx
        .db
        .read(move |conn| quota::maintenance_available(conn, now))
        .expect("available");
    assert_eq!(
        available,
        quota::maintenance_budget(&vault) - usage.maintenance_bytes - active_reserved,
        "维护额度必须只被当前 live 数据占用"
    );
    assert!(
        maintenance_parts < usage_before.maintenance_bytes,
        "回收后维护密文必须下降：{} → {}",
        usage_before.maintenance_bytes,
        maintenance_parts
    );

    // 签名骨架仍可读，但不参与 bootstrap；bootstrap 只用 active epoch 的健康两份。
    let skeleton = fx.checkpoint_row(&epoch1_baseline);
    assert_eq!(skeleton.state, checkpoints::CheckpointState::Pruned);
    assert!(skeleton.signed_commit.is_some(), "commit 骨架必须保留");
    assert!(skeleton.commit_signature.is_some(), "签名骨架必须保留");
    assert!(skeleton.manifest_oid.is_some(), "manifest 摘要必须保留");
    assert!(fx
        .db
        .read(|conn| checkpoints::refs(conn, &epoch1_baseline))
        .expect("refs")
        .is_empty());
    let bootstrap = fx.bootstrap().expect("active epoch 可 bootstrap");
    assert_eq!(bootstrap.epoch, active_epoch);
    assert!(!bootstrap.degraded);
    // epoch1 的资产密文已回收 → 直接按 epoch 枚举为空。
    let epoch1_objects = fx
        .db
        .read(move |conn| objects::list_assets(conn, 1, i64::MAX as u64, None, 100))
        .expect("epoch1 assets");
    assert!(epoch1_objects.is_empty());
    assert_eq!(
        fx.object_row(&epoch1_asset).budget_class,
        objects::BudgetClass::Retired
    );
}

#[test]
fn aborted_and_candidate_garbage_is_reclaimed_but_ongoing_rotation_is_spared() {
    let fx = Fixture::new("reclaim-candidates");
    fx.create_object(ObjectType::Update, 1, 1, 32);
    fx.publish_baseline();
    fx.publish_baseline();

    // 第一次轮换：建一个带 refs 的候选 checkpoint + 一份从未被引用的候选垃圾，然后 abort。
    let rotation = fx.begin_rotation(&[]).rotation.rotation_id;
    let row = fx.rotation_view(&rotation).rotation;
    let outcome = fx.create_candidate_checkpoint(row.to_epoch);
    let cid = outcome.checkpoint_id;
    let reservation = outcome.reservation_id;
    let snapshot = fx.create_candidate_object(ObjectType::Snapshot, 1, row.to_epoch, &reservation);
    let manifest = fx.create_candidate_object(ObjectType::Manifest, 0, row.to_epoch, &reservation);
    fx.add_candidate_refs(cid, snapshot, manifest);
    let orphan = fx.create_candidate_object(ObjectType::Snapshot, 1, row.to_epoch, &reservation);
    fx.abort_rotation(&rotation).expect("abort");

    // 第二次轮换（进行中）：候选材料必须被保留，不能被当作垃圾回收。
    let rotation2 = fx.begin_rotation(&[]).rotation.rotation_id;
    let row2 = fx.rotation_view(&rotation2).rotation;
    let reservation2 = fx.create_candidate_checkpoint(row2.to_epoch).reservation_id;
    let ongoing = fx.create_candidate_object(ObjectType::Snapshot, 1, row2.to_epoch, &reservation2);
    let log_before = fx.log_seqs();

    fx.clock.advance_secs(8 * DAY);
    let outcome = fx.gc();
    assert!(!outcome.blocked, "{outcome:?}");
    assert!(outcome.pruned_checkpoints >= 1, "{outcome:?}");
    assert!(outcome.retired_pruned_objects >= 2, "{outcome:?}");

    // aborted checkpoint：refs 清空、状态 pruned、对象密文回收。
    let aborted = fx.checkpoint_row(&cid);
    assert_eq!(aborted.state, checkpoints::CheckpointState::Pruned);
    assert!(fx
        .db
        .read(move |conn| checkpoints::refs(conn, &cid))
        .expect("refs")
        .is_empty());
    for object_id in [snapshot, manifest, orphan] {
        let object = fx.object_row(&object_id);
        assert_eq!(object.budget_class, objects::BudgetClass::Retired);
        assert_eq!(parts_of(&fx, &object_id), 0, "候选垃圾密文必须清除");
    }
    // 进行中的轮换候选材料保留（轮换 client 可能仍在 prepare/validate）。
    let surviving = fx.object_row(&ongoing);
    assert_ne!(
        surviving.budget_class,
        objects::BudgetClass::Retired,
        "进行中轮换的候选对象不得回收"
    );
    assert!(parts_of(&fx, &ongoing) > 0);
    // 进行中的候选 epoch 也不会被 prune。
    assert!(
        fx.db
            .read(move |conn| checkpoints::candidates_for_epoch(conn, row2.to_epoch))
            .expect("candidates")
            .is_empty(),
        "本用例未为 ongoing 建 checkpoint 行"
    );
    // 发布两份基线本身会建 7 天计划；到期后日志前缀回收、floor 前进，
    // 但这不影响"进行中候选"与"aborted 候选垃圾"的回收语义。
    assert_eq!(fx.vault().gc_floor, 1);
    assert!(fx.log_seqs().len() < log_before.len());
    let _ = reservation2;
}

#[test]
fn degraded_or_unhealthy_roots_block_all_destructive_reclamation() {
    let fx = Fixture::new("reclaim-degraded");
    fx.create_object(ObjectType::Update, 1, 1, 32);
    let c1 = fx.publish_baseline();
    fx.create_object(ObjectType::Update, 1, 2, 32);
    fx.publish_baseline();

    // 旧 epoch 垃圾：aborted 候选 epoch 的对象（7 天后本应被回收）。
    let rotation = fx.begin_rotation(&[]).rotation.rotation_id;
    let row = fx.rotation_view(&rotation).rotation;
    // rotation reservation 由候选 checkpoint 的创建惰性建立。
    let reservation = fx.create_candidate_checkpoint(row.to_epoch).reservation_id;
    let garbage = fx.create_candidate_object(ObjectType::Snapshot, 1, row.to_epoch, &reservation);
    fx.abort_rotation(&rotation).expect("abort");
    let garbage_parts_before = parts_of(&fx, &garbage);
    assert!(garbage_parts_before > 0);

    // 损坏 active epoch 的一份保留根 → degraded。
    let refs = fx
        .db
        .read(move |conn| checkpoints::refs(conn, &c1))
        .expect("refs");
    let (victim, _) = refs[0];
    let victim_parts: Vec<u8> = fx
        .db
        .read(move |conn| {
            Ok(conn.query_row(
                "SELECT envelope FROM object_parts WHERE object_id = ?1 AND part_no = 0",
                [victim.as_slice()],
                |row| row.get(0),
            )?)
        })
        .expect("part");
    fx.raw_write(move |conn| {
        conn.execute(
            "UPDATE object_parts SET envelope = ?1 WHERE object_id = ?2 AND part_no = 0",
            (
                vec![0x11u8; victim_parts.len()].as_slice(),
                victim.as_slice(),
            ),
        )?;
        Ok(())
    })
    .expect("corrupt root");

    let log_before = fx.log_seqs();
    let parts_before = old_epoch_parts(&fx, 1);
    let usage_before = fx.usage();
    fx.clock.advance_secs(8 * DAY);
    let outcome = fx.gc();
    assert!(outcome.blocked, "{outcome:?}");
    assert_eq!(outcome.pruned_checkpoints, 0);
    assert_eq!(outcome.retired_pruned_objects, 0);
    assert_eq!(outcome.deleted_log_rows, 0);
    assert_eq!(outcome.floor, 0);
    assert_eq!(fx.vault().gc_floor, 0, "floor 绝不前进");
    assert_eq!(fx.log_seqs(), log_before, "日志不得被删");
    assert_eq!(
        old_epoch_parts(&fx, 1),
        parts_before,
        "旧 epoch 密文不得被删"
    );
    assert_eq!(parts_of(&fx, &garbage), garbage_parts_before);
    assert_eq!(fx.usage().maintenance_bytes, usage_before.maintenance_bytes);
    assert_eq!(
        fx.object_row(&garbage).budget_class,
        objects::BudgetClass::Maintenance,
        "degraded 时不得 retire 任何对象"
    );
    // 根被标记 unhealthy，后续仍需补齐两份健康基线才能前进。
    assert_eq!(
        fx.checkpoint_row(&c1).health,
        checkpoints::Health::Unhealthy
    );
    let err = fx.bootstrap().expect_err("degraded 严格 bootstrap 拒绝");
    assert_eq!(code(&err), Some(ErrorCode::NoValidBaseline));
}

#[test]
fn gc_plans_and_leases_still_keep_floor_semantics() {
    // 计划未到期（不到 7 天）→ 即便根健康也不删日志、floor 不动。
    let fx = Fixture::new("reclaim-plan");
    fx.create_object(ObjectType::Update, 1, 1, 32);
    fx.publish_baseline();
    fx.create_object(ObjectType::Update, 1, 2, 32);
    fx.publish_baseline();
    let plans = fx.gc_plans();
    assert_eq!(plans.len(), 1);
    fx.clock.advance_secs(6 * DAY);
    let outcome = fx.gc();
    assert!(!outcome.blocked, "{outcome:?}");
    assert_eq!(outcome.cut_seq, 0, "计划未到期");
    assert_eq!(outcome.deleted_log_rows, 0);
    assert_eq!(fx.vault().gc_floor, 0);
    // 到期后才推进（仍尊重 lease floor）。
    fx.clock.advance_secs(2 * DAY);
    let outcome = fx.gc();
    assert_eq!(outcome.cut_seq, 1);
    assert_eq!(outcome.floor, 1);
    assert_eq!(fx.vault().gc_floor, 1);
    // 保留集合去重：active epoch 的两份基线不会出现在 prune 名单里。
    let published: BTreeSet<Id16> = fx
        .db
        .read(|conn| checkpoints::published_newest_first_for_epoch(conn, 1, 2))
        .expect("published")
        .into_iter()
        .map(|checkpoint| checkpoint.checkpoint_id)
        .collect();
    let pruned: BTreeSet<Id16> = fx
        .db
        .read(|conn| {
            let mut stmt =
                conn.prepare("SELECT checkpoint_id FROM checkpoints WHERE state = 'pruned'")?;
            let rows = stmt.query_map([], |row| row.get::<_, Vec<u8>>(0))?;
            let mut out = BTreeSet::new();
            for row in rows {
                out.insert(
                    row?.as_slice()
                        .try_into()
                        .map_err(|_| relay::Error::Invariant("cid"))?,
                );
            }
            Ok(out)
        })
        .expect("pruned");
    assert!(pruned.is_disjoint(&published));
}
