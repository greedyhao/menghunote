//! 评审 0002 §五 T20 剩余：发布 checkpoint 必须保证"未来 live"（bootstrap 会长期
//! 保留的最新两份基线 + 日志仍保留的 UPDATE）能装进正常预算 Q；否则维护预算会在
//! 反复 checkpoint 后永久把库顶到 Q 之上。只允许拒绝/暂停，不得删除安全根。
//!
//! reservation 过期、Q/R 峰值与逐片 507 已由 `core.rs::maintenance_reservation_formula_and_per_part_charging`
//! 与 `fault_budget.rs` 覆盖；本文件只补"发布前未来 live"这一条契约。

mod support;

use support::*;

use relay::checkpoints;
use relay::envelope::ObjectType;
use relay::error::ErrorCode;
use relay::refs::RefRole;
use relay::Id16;

struct StagedBaseline {
    checkpoint_id: Id16,
    covered_seq: u64,
    manifest_oid: [u8; 32],
}

/// 在 active epoch 建立一份"维护预算"基线（reservation 覆盖），但**不发布**。
fn stage_maintenance_baseline(fx: &Fixture, estimated: i64, plain_len: i64) -> StagedBaseline {
    let ctx = fx.device_ctx();
    let db = fx.db.clone();
    let clock = fx.clock.clone();
    let outcome = db
        .write_blocking(move |conn| {
            checkpoints::create(conn, clock.as_ref(), &ctx, estimated, None, None, None)
        })
        .expect("create maintenance checkpoint");
    let cid = outcome.checkpoint_id;
    let reservation = outcome.reservation_id;
    let mut objects = Vec::new();
    for (object_type, generation) in [(ObjectType::Snapshot, 1u64), (ObjectType::Manifest, 0u64)] {
        let object_id = support::random_object_id();
        let envelope = support::single_part_envelope_at(
            object_type,
            fx.vault_id,
            object_id,
            generation,
            0,
            u64::try_from(plain_len).expect("plain len"),
            1,
        );
        fx.put_part(&envelope, Some(&reservation))
            .expect("put maintenance part");
        fx.complete_object(&object_id, &envelope)
            .expect("complete maintenance object");
        objects.push(object_id);
    }
    let (snapshot, manifest) = (objects[0], objects[1]);
    fx.db
        .write_blocking(move |conn| {
            checkpoints::add_refs(
                conn,
                1024,
                &cid,
                &[(snapshot, RefRole::Data), (manifest, RefRole::Manifest)],
            )
        })
        .expect("add refs");
    StagedBaseline {
        checkpoint_id: cid,
        covered_seq: outcome.covered_seq,
        manifest_oid: manifest,
    }
}

/// 发布一份已 staged 的基线（签名由 `TestHook` 接受）。
fn publish(fx: &Fixture, staged: &StagedBaseline) -> Result<(), relay::Error> {
    let ctx = fx.device_ctx();
    let db = fx.db.clone();
    let clock = fx.clock.clone();
    let hook = fx.hook.clone();
    let cid = staged.checkpoint_id;
    let covered_seq = staged.covered_seq;
    let manifest_oid = staged.manifest_oid;
    let signed_commit = support::Fixture::fake_v2_commit(
        fx.vault_id,
        cid,
        fx.vault().active_epoch,
        covered_seq,
        manifest_oid,
        fx.device,
    );
    db.write_blocking(move |conn| {
        checkpoints::publish(
            conn,
            clock.as_ref(),
            hook.as_ref(),
            &ctx,
            &cid,
            &checkpoints::PublishRequest {
                covered_seq,
                manifest_oid,
                refs_digest: [9u8; 32],
                signed_commit: &signed_commit,
                signature: b"signature",
            },
        )
        .map(|_| ())
    })
}

/// 最近两份 published checkpoint 的 live 密文字节总数（与 `quota::future_live_bytes`
/// 同口径：data + MANIFEST refs + log 仍保留的 UPDATE；本文件无 orphan ASSET）。
fn live_pair_bytes(fx: &Fixture) -> i64 {
    fx.db
        .read(|conn| {
            Ok(conn.query_row(
                "SELECT COALESCE(SUM(length(p.envelope)), 0)
                 FROM object_parts p
                 WHERE p.object_id IN (
                     SELECT r.object_id FROM checkpoint_refs r
                      WHERE r.role IN ('data','manifest')
                        AND r.checkpoint_id IN (
                            SELECT checkpoint_id FROM checkpoints
                            WHERE state = 'published'
                            ORDER BY published_at DESC, rowid DESC LIMIT 2
                        )
                     UNION
                     SELECT l.object_id FROM log l
                      WHERE l.kind = 'update' AND l.object_id IS NOT NULL
                 )",
                [],
                |row| row.get(0),
            )?)
        })
        .expect("live bytes")
}

/// 拒绝路径必须无副作用：对象分片/日志/账本/GC 计划/基线状态全部保持不变。
#[derive(Debug, PartialEq, Eq)]
struct Observed {
    parts: i64,
    part_bytes: i64,
    log_rows: i64,
    published: i64,
    staging: i64,
    gc_plans: i64,
    normal_bytes: i64,
    maintenance_bytes: i64,
}

fn observe(fx: &Fixture) -> Observed {
    fx.db
        .read(|conn| {
            Ok(Observed {
                parts: conn.query_row("SELECT COUNT(*) FROM object_parts", [], |row| row.get(0))?,
                part_bytes: conn.query_row(
                    "SELECT COALESCE(SUM(length(envelope)), 0) FROM object_parts",
                    [],
                    |row| row.get(0),
                )?,
                log_rows: conn.query_row("SELECT COUNT(*) FROM log", [], |row| row.get(0))?,
                published: conn.query_row(
                    "SELECT COUNT(*) FROM checkpoints WHERE state = 'published'",
                    [],
                    |row| row.get(0),
                )?,
                staging: conn.query_row(
                    "SELECT COUNT(*) FROM checkpoints WHERE state = 'staging'",
                    [],
                    |row| row.get(0),
                )?,
                gc_plans: conn.query_row("SELECT COUNT(*) FROM gc_plans", [], |row| row.get(0))?,
                normal_bytes: conn.query_row(
                    "SELECT normal_bytes FROM usage WHERE vault_id = ?1",
                    [conn.scope().as_slice()],
                    |row| row.get(0),
                )?,
                maintenance_bytes: conn.query_row(
                    "SELECT maintenance_bytes FROM usage WHERE vault_id = ?1",
                    [conn.scope().as_slice()],
                    |row| row.get(0),
                )?,
            })
        })
        .expect("observe")
}

#[test]
fn publish_rejects_when_latest_two_baselines_exceed_normal_limit() {
    // Q = 3000，峰值 Q+R = 6000；单份基线（snapshot+manifest+log UPDATE）约 2044B 可装进 Q，
    // 两份长期 live 约 3868B 超过 Q。
    let fx = Fixture::with_limits("future-live-q", 3000, 6000);
    fx.create_object(ObjectType::Update, 1, 1, 8);
    let first = stage_maintenance_baseline(&fx, 2000, 700);
    publish(&fx, &first).expect("first baseline");
    let live_after_first = live_pair_bytes(&fx);
    assert!(
        live_after_first <= fx.vault().normal_limit,
        "前置：单份基线必须能装进 Q（{live_after_first}）"
    );

    // 第二份基线本身也在单个 reservation/Q 内；但发布后"最新两份"的 live 超过 Q。
    let second = stage_maintenance_baseline(&fx, 2000, 700);
    // 第二份基线贡献 data+manifest 2*(700+212)=1824B；log UPDATE 已包含在 live_after_first。
    let live_if_published = live_after_first + 1824;
    assert!(
        live_if_published > fx.vault().normal_limit,
        "前置：两份长期 baseline 的 live 会超 Q（{live_if_published} vs Q）"
    );
    let live_before_gate = fx
        .db
        .read(|conn| {
            relay::quota::future_live_bytes(conn, fx.vault().active_epoch, &[second.checkpoint_id])
        })
        .expect("live");
    assert!(
        live_before_gate > fx.vault().normal_limit,
        "前置：计入口径（含 log UPDATE）同样超 Q（{live_before_gate}）"
    );

    let before = observe(&fx);
    let error = publish(&fx, &second)
        .expect_err("未来 live > Q 时，第二份 checkpoint 必须在 publish 前被拒绝");
    // 只允许可恢复的拒绝/暂停，不做破坏性回收；错误码与既有配额语义一致（507）。
    assert_eq!(
        error.code(),
        Some(ErrorCode::QuotaExceeded),
        "期望 507 QUOTA_EXCEEDED，实际 {error:?}"
    );
    assert_eq!(
        observe(&fx),
        before,
        "拒绝必须无副作用：对象/日志/账本/GC 计划/基线状态均不变"
    );
    // 安全根不得因拒绝被删除：第一份仍是 published，第二份仍是 staging。
    assert_eq!(
        fx.checkpoint_row(&first.checkpoint_id).state,
        checkpoints::CheckpointState::Published,
        "任何配额拒绝都不得抢删安全根"
    );
    assert_eq!(
        fx.checkpoint_row(&second.checkpoint_id).state,
        checkpoints::CheckpointState::Staging,
        "被拒绝的 checkpoint 只能留在 staging，可扩容后重试"
    );
}

/// 已发布两份基线时，第三份若会让"最新两份 live"超 Q，必须被拒绝，且旧两份基线
/// 与其对象/日志原封不动（只拒绝、不回收安全根）。
#[test]
fn rejected_publish_keeps_published_pair_objects_and_log_unchanged() {
    // Q 能容纳两份小基线（各 data+manifest 1824B，加 log UPDATE 220B = 3868B）；
    // 第三份较大，发布后最新两份（第二 + 第三）会超 Q。
    let fx = Fixture::with_limits("future-live-pair", 4500, 12000);
    fx.create_object(ObjectType::Update, 1, 1, 8);

    let first = stage_maintenance_baseline(&fx, 2000, 700);
    publish(&fx, &first).expect("first baseline");
    let second = stage_maintenance_baseline(&fx, 2000, 700);
    publish(&fx, &second).expect("第二份仍在 Q 内，必须成功");

    let third = stage_maintenance_baseline(&fx, 4000, 1500);
    let live = fx
        .db
        .read(|conn| {
            relay::quota::future_live_bytes(conn, fx.vault().active_epoch, &[third.checkpoint_id])
        })
        .expect("live");
    assert!(
        live > fx.vault().normal_limit,
        "前置：第三份发布后最新两份 live 超 Q（{live}）"
    );

    let before = observe(&fx);
    let error = publish(&fx, &third).expect_err("第三份 checkpoint 必须被拒绝");
    assert_eq!(error.code(), Some(ErrorCode::QuotaExceeded));
    assert_eq!(
        observe(&fx),
        before,
        "拒绝前后旧两份基线/对象分片/日志/账本/GC 计划必须完全一致"
    );
    for baseline in [&first, &second] {
        assert_eq!(
            fx.checkpoint_row(&baseline.checkpoint_id).state,
            checkpoints::CheckpointState::Published,
            "已发布的安全根不得被配额拒绝连带删除或降级"
        );
    }
    assert_eq!(
        fx.checkpoint_row(&third.checkpoint_id).state,
        checkpoints::CheckpointState::Staging
    );
}
