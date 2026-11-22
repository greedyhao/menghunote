//! 评审 0002 §五 T04/T05 剩余（relay 侧）：
//! 候选 checkpoint 在 ready 后拖 8 天 / 多候选 / abort 都**不得** publish、不得进入
//! bootstrap、不得被 GC 当成安全根；两候选复用同一 snapshot/manifest 的独立性负例
//! 已在 `gc_rotation::activate_rejects_non_independent_candidates` 覆盖。
//!
//! 时间用 FakeClock 推进，无 sleep；所有状态断言走真实 SQLite 行。

mod support;

use support::*;

use relay::checkpoints::{self, CheckpointState};
use relay::envelope::ObjectType;

const DAY: i64 = 86_400;

fn candidate_ready(fx: &Fixture, cid: &relay::ids::Id16) -> bool {
    let cid = *cid;
    fx.db
        .read(move |conn| {
            let row = checkpoints::get(conn, &cid)?;
            checkpoints::candidate_ready(conn, &row)
        })
        .expect("candidate_ready")
}

fn count_candidates(fx: &Fixture, epoch: u64) -> i64 {
    fx.db
        .read(move |conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM checkpoints WHERE epoch = ?1 AND state = 'candidate'",
                [i64::try_from(epoch).expect("epoch")],
                |row| row.get(0),
            )?)
        })
        .expect("candidate count")
}

#[test]
fn ready_candidates_survive_eight_days_but_never_publish_gc_or_bootstrap() {
    let fx = Fixture::new("candidate-lifecycle");
    // 先建立两份 active epoch 的已发布基线（bootstrap 需要两份）。
    let first = fx.publish_baseline();
    fx.create_object(ObjectType::Update, 1, 1, 32);
    let second = fx.publish_baseline();
    assert_ne!(first, second);
    // 再推一条 update，使候选 epoch 的 S 严格高于已发布基线（证明"不假覆盖"）。
    fx.create_object(ObjectType::Update, 1, 2, 32);
    let vault = fx.vault();
    let published_primary_s = fx.checkpoint_row(&second).covered_seq;
    let published_fallback_s = fx.checkpoint_row(&first).covered_seq;
    assert!(published_primary_s < vault.head_seq, "候选 S 必须更高");

    let rotation = fx.begin_rotation(&[]).rotation.rotation_id;
    let freeze = fx.rotation_view(&rotation).rotation.freeze_seq;
    assert_eq!(freeze, vault.head_seq);

    // 多候选：默认一对 + 额外的第 3/4 个候选（各自独立 snapshot/manifest）。
    let (pair_a, pair_b) = fx.publish_candidate_baselines(2);
    let mut extras = Vec::new();
    for _ in 0..2 {
        let outcome = fx.create_candidate_checkpoint(2);
        let rid = outcome.reservation_id;
        let snapshot = fx.create_candidate_object(ObjectType::Snapshot, 1, 2, &rid);
        let manifest = fx.create_candidate_object(ObjectType::Manifest, 0, 2, &rid);
        fx.add_candidate_refs(outcome.checkpoint_id, snapshot, manifest);
        fx.publish_candidate_checkpoint(outcome.checkpoint_id, outcome.covered_seq, manifest);
        extras.push(outcome.checkpoint_id);
    }
    let all = [pair_a, pair_b, extras[0], extras[1]];
    for cid in all {
        let row = fx.checkpoint_row(&cid);
        assert_eq!(row.state, CheckpointState::Candidate);
        assert_eq!(row.covered_seq, freeze);
        assert!(candidate_ready(&fx, &cid), "候选必须 readback-ready");
        assert_eq!(count_candidates(&fx, 2), 4);
    }

    // 拖 8 天：超过 checkpoint 7 天宽限与 24h reservation；进行中的轮换候选不得被 GC 删，
    // 也不得因为"老了"被 publish。
    fx.clock.advance_secs(8 * DAY);
    let outcome = fx.gc();
    assert_eq!(outcome.pruned_checkpoints, 0, "进行中轮换的候选不得剪枝");
    for cid in all {
        let row = fx.checkpoint_row(&cid);
        assert_eq!(row.state, CheckpointState::Candidate, "8 天后仍是候选");
        assert!(candidate_ready(&fx, &cid));
    }

    // bootstrap 只能拿到 active epoch 的 published 对，不得把候选当基线/覆盖 S。
    let bundle = fx.bootstrap().expect("bootstrap");
    assert_eq!(bundle.primary.checkpoint_id, second);
    assert_eq!(
        bundle.fallback.as_ref().expect("fallback").checkpoint_id,
        first
    );
    assert_eq!(bundle.primary.covered_seq, published_primary_s);
    assert_eq!(
        bundle.fallback.as_ref().expect("fallback").covered_seq,
        published_fallback_s
    );
    assert!(
        bundle.primary.covered_seq < freeze,
        "bootstrap 不得假覆盖候选 S"
    );
    assert_ne!(bundle.primary.checkpoint_id, pair_a);
    assert_ne!(bundle.primary.checkpoint_id, extras[0]);

    // abort：未激活的候选全部作废；安全根（已发布对）保持不变，bootstrap 仍可用。
    fx.abort_rotation(&rotation).expect("abort");
    for cid in all {
        assert_eq!(
            fx.checkpoint_row(&cid).state,
            CheckpointState::Aborted,
            "abort 后候选不得再被 promote/复用"
        );
    }
    let outcome = fx.gc();
    assert!(
        outcome.pruned_checkpoints >= 4,
        "abort 且过 7 天的候选应被回收，实际 {outcome:?}"
    );
    for cid in all {
        let state = fx.checkpoint_row(&cid).state;
        assert!(
            matches!(state, CheckpointState::Aborted | CheckpointState::Pruned),
            "候选只能是 aborted/pruned，绝不能 published，实际 {state:?}"
        );
    }
    assert_eq!(
        fx.checkpoint_row(&first).state,
        CheckpointState::Published,
        "安全根不得被候选回收误删"
    );
    assert_eq!(fx.checkpoint_row(&second).state, CheckpointState::Published);
    let bundle = fx.bootstrap().expect("abort 后 bootstrap 仍可用");
    assert_eq!(bundle.primary.checkpoint_id, second);
    assert!(bundle.fallback.is_some());
}
