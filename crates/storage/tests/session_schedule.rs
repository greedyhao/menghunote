//! 存储侧调度：固定窗口、64 KiB、Close 排空、B 物化后台驱动。

mod support;

use std::time::{Duration, Instant};

use menghunote_storage::session::{
    CloseReport, DocumentSession, DriverEvent, DriverOptions, DurableAck, EditRequest, FlushPolicy,
    FlushReason, LiveRev, ManualClock, ScalarEdit, SessionDriver, SessionEpoch, SessionOptions,
};
use menghunote_storage::{create_note_slot, load_body_state, MaterializeOutcome, Vault};
use support::*;

fn edit(start: usize, end: usize, replacement: &str) -> ScalarEdit {
    ScalarEdit {
        start: menghunote_text::UnicodeScalar::new(start),
        end: menghunote_text::UnicodeScalar::new(end),
        replacement: replacement.to_owned(),
    }
}

fn request(
    session: &DocumentSession,
    command: &str,
    base: u64,
    edits: Vec<ScalarEdit>,
) -> EditRequest {
    EditRequest {
        doc_id: session.doc_id().to_owned(),
        generation: session.generation(),
        session_epoch: session.session_epoch(),
        command_id: command.to_owned(),
        base_live_rev: LiveRev(base),
        edits,
        origin: "local".to_owned(),
        undo_group: None,
    }
}

fn options(peer: u64) -> SessionOptions {
    SessionOptions {
        peer_id: peer,
        session_epoch: SessionEpoch(11),
        ..SessionOptions::default()
    }
}

fn driver_options(session: SessionOptions, policy: FlushPolicy) -> DriverOptions {
    DriverOptions {
        session,
        policy,
        auto_tick: false,
        tick_interval: None,
    }
}

fn open_driver(vault: &Vault, clock: ManualClock, policy: FlushPolicy) -> SessionDriver {
    SessionDriver::open_with_clock(
        vault,
        "doc-1",
        driver_options(options(PEER_A), policy),
        Box::new(clock),
    )
    .expect("打开驱动")
}

fn wait_durable(
    driver: &mut SessionDriver,
    target: LiveRev,
    timeout: Duration,
) -> Option<DurableAck> {
    let deadline = Instant::now() + timeout;
    loop {
        for event in driver.pump() {
            if let DriverEvent::Durable(ack) = event {
                if ack.through_live_rev >= target {
                    return Some(ack);
                }
            }
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn flush_events(events: &[DriverEvent]) -> Vec<FlushReason> {
    events
        .iter()
        .filter_map(|event| match event {
            DriverEvent::FlushScheduled { reason, .. } => Some(*reason),
            _ => None,
        })
        .collect()
}

#[test]
fn fixed_window_starts_at_first_dirty_not_last_input() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let clock = ManualClock::new();
    let mut driver = open_driver(&vault, clock.clone(), FlushPolicy::default());

    // t=0 首次 dirty：窗口起点锁在 0
    driver.apply(request(driver.session(), "c1", 0, vec![edit(0, 0, "甲")]));
    assert_eq!(
        driver.next_deadline(),
        Some(Duration::from_millis(250)),
        "窗口从首次 dirty 起算"
    );

    // t=100ms 继续输入：**不重置**窗口起点
    clock.set(Duration::from_millis(100));
    driver.apply(request(driver.session(), "c2", 1, vec![edit(1, 1, "乙")]));
    assert_eq!(
        driver.next_deadline(),
        Some(Duration::from_millis(250)),
        "连续输入不得延期（固定窗口，不是 debounce）"
    );

    // t=249ms 还不到期
    clock.set(Duration::from_millis(249));
    let events = driver.tick(clock.get());
    assert!(flush_events(&events).is_empty(), "249ms 不应 flush");
    assert_eq!(driver.pending_edits(), 2);

    // t=250ms 到期：与前一次输入无关，都会 flush
    clock.set(Duration::from_millis(250));
    let events = driver.tick(clock.get());
    assert_eq!(
        flush_events(&events),
        vec![FlushReason::Interval],
        "250ms 固定窗口到期必须 flush"
    );
    assert_eq!(driver.next_deadline(), None);

    let ack = wait_durable(&mut driver, LiveRev(2), Duration::from_secs(5)).expect("DurableAck");
    assert_eq!(ack.through_live_rev, LiveRev(2));
    assert_eq!(driver.pending_edits(), 0);
}

#[test]
fn continuous_input_still_meets_the_window_deadline() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let clock = ManualClock::new();
    let mut driver = open_driver(&vault, clock.clone(), FlushPolicy::default());

    for (index, step) in [0u64, 60, 120, 180, 240].iter().enumerate() {
        clock.set(Duration::from_millis(*step));
        let start = driver.session().markdown().chars().count();
        let command = format!("c{}", index + 1);
        driver.apply(request(
            driver.session(),
            &command,
            driver.live_rev().0,
            vec![edit(start, start, "字")],
        ));
    }
    assert_eq!(driver.next_deadline(), Some(Duration::from_millis(250)));

    let events = driver.tick(Duration::from_millis(250));
    assert_eq!(
        flush_events(&events),
        vec![FlushReason::Interval],
        "首字符后 250ms 必到，不被后续输入拖后"
    );
    wait_durable(&mut driver, LiveRev(5), Duration::from_secs(5)).expect("DurableAck");
}

#[test]
fn reaching_64_kib_flushes_immediately() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let clock = ManualClock::new();
    let mut driver = open_driver(&vault, clock.clone(), FlushPolicy::default());

    // 小编辑不触发（默认阈值 64 KiB）
    driver.apply(request(driver.session(), "c1", 0, vec![edit(0, 0, "小")]));
    let events = driver.pump();
    assert!(
        flush_events(&events).is_empty(),
        "小编辑不应触发 Size flush"
    );

    // 一次插入 70 KiB → 立即 flush(Size)
    let big = "x".repeat(70 * 1024);
    driver.apply(request(driver.session(), "c2", 1, vec![edit(1, 1, &big)]));
    let events = driver.pump();
    assert_eq!(
        flush_events(&events),
        vec![FlushReason::Size],
        "达到 64 KiB 必须立即 flush，不等定时器"
    );
    assert_eq!(driver.next_deadline(), None, "Size flush 也清掉窗口");
    wait_durable(&mut driver, LiveRev(2), Duration::from_secs(5)).expect("DurableAck");
}

#[test]
fn close_drains_pending_edits_and_materializes_body() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let clock = ManualClock::new();
    let mut driver = open_driver(&vault, clock, FlushPolicy::default());

    driver.apply(request(driver.session(), "c1", 0, vec![edit(0, 0, "甲")]));
    driver.apply(request(driver.session(), "c2", 1, vec![edit(1, 1, "乙")]));
    assert_eq!(driver.pending_edits(), 2);

    let report: CloseReport = driver.close(Duration::from_secs(5));
    assert!(!report.timed_out, "Close 必须排空：{report:?}");
    assert_eq!(report.pending_edits, 0);
    assert_eq!(report.pending_materialize, 0);
    assert!(
        report.durable.is_some(),
        "Close 要等到覆盖最后 live_rev 的 ACK"
    );
    assert_eq!(
        report.durable.as_ref().expect("ack").through_live_rev,
        LiveRev(2)
    );
    assert!(report.materialized >= 1, "Close 期间至少完成一次 B 物化");

    let state = load_body_state(&vault, "note-1").expect("状态");
    assert_eq!(state.body_md.as_deref(), Some("甲乙"), "B 物化必须落地");
    assert_eq!(
        count(
            &vault,
            "SELECT count(*) FROM projection_outbox WHERE doc_id = 'doc-1'",
            &[]
        ),
        0
    );
}

#[test]
fn body_materialization_waits_for_durable_ack_and_uses_fixed_frontier() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let clock = ManualClock::new();
    let mut session_options = options(PEER_A);
    session_options.flush_delay = Some(Duration::from_millis(150));
    let mut driver = SessionDriver::open_with_clock(
        &vault,
        "doc-1",
        driver_options(session_options, FlushPolicy::default()),
        Box::new(clock),
    )
    .expect("打开驱动");

    driver.apply(request(driver.session(), "c1", 0, vec![edit(0, 0, "甲")]));
    let ticket = driver.flush(FlushReason::Manual);
    assert_eq!(ticket.target_live_rev, LiveRev(1));

    // ACK 还没到：B 不得入队，body_md 仍是 NULL
    let events = driver.pump();
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, DriverEvent::Materialized(_))),
        "没有 DurableAck 之前不得物化"
    );
    assert!(load_body_state(&vault, "note-1")
        .expect("状态")
        .body_md
        .is_none());

    // ACK 之后：B 用固定 head_seq=1 物化出「甲」
    wait_durable(&mut driver, LiveRev(1), Duration::from_secs(5)).expect("DurableAck");
    assert!(driver.wait_materialize(Duration::from_secs(5)));
    assert_eq!(
        load_body_state(&vault, "note-1")
            .expect("状态")
            .body_md
            .as_deref(),
        Some("甲")
    );

    // live 里继续写「乙」但没 flush：固定 frontier 的 B 仍只物化到 seq 1
    driver.apply(request(driver.session(), "c2", 1, vec![edit(1, 1, "乙")]));
    assert_eq!(driver.markdown(), "甲乙");
    let events = driver.pump();
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, DriverEvent::Materialized(_))),
        "未 flush 的编辑不得触发 B"
    );
    assert_eq!(
        load_body_state(&vault, "note-1")
            .expect("状态")
            .body_md
            .as_deref(),
        Some("甲"),
        "B 只物化 durable 前缀，不读 live doc"
    );

    let report = driver.close(Duration::from_secs(5));
    assert!(!report.timed_out);
    assert_eq!(
        load_body_state(&vault, "note-1")
            .expect("状态")
            .body_md
            .as_deref(),
        Some("甲乙")
    );
}

#[test]
fn stale_body_job_is_rejected_by_cas() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let clock = ManualClock::new();
    let mut driver = open_driver(&vault, clock, FlushPolicy::default());

    driver.apply(request(driver.session(), "c1", 0, vec![edit(0, 0, "甲")]));
    driver.flush(FlushReason::Manual);
    wait_durable(&mut driver, LiveRev(1), Duration::from_secs(5)).expect("ack1");
    driver.apply(request(driver.session(), "c2", 1, vec![edit(1, 1, "乙")]));
    driver.flush(FlushReason::Manual);
    let ack = wait_durable(&mut driver, LiveRev(2), Duration::from_secs(5)).expect("ack2");
    assert_eq!(ack.head_seq, 2, "两条 update 各占一个 seq");

    // 先让后台 B 把两个 ACK 都物化完（避免与显式调用竞争）
    assert!(
        driver.wait_materialize(Duration::from_secs(5)),
        "后台 B 应排空"
    );
    assert_eq!(
        load_body_state(&vault, "note-1")
            .expect("状态")
            .body_md
            .as_deref(),
        Some("甲乙"),
        "后台 B 已物化到 seq 2"
    );

    // 同一水位重跑：CAS 一行不命中 → Stale（幂等，不重复写）
    let same = driver
        .materialize_now("note-1", "doc-1", 2)
        .expect("同水位重跑不报错");
    assert!(matches!(
        same,
        MaterializeOutcome::Stale { body_seq: 2, .. }
    ));

    // 旧任务（seq 1）迟到：CAS 拒绝，绝不覆盖新结果
    let stale = driver
        .materialize_now("note-1", "doc-1", 1)
        .expect("旧任务不报错");
    match stale {
        MaterializeOutcome::Stale { note_id, body_seq } => {
            assert_eq!(note_id, "note-1");
            assert_eq!(body_seq, 2, "库里仍是新水位");
        }
        other => panic!("旧任务必须被 CAS 拒绝，实际 {other:?}"),
    }
    assert_eq!(
        load_body_state(&vault, "note-1")
            .expect("状态")
            .body_md
            .as_deref(),
        Some("甲乙"),
        "旧任务不得覆盖新结果"
    );
    assert_eq!(
        count(
            &vault,
            "SELECT count(*) FROM projection_outbox WHERE doc_id = 'doc-1'",
            &[]
        ),
        0,
        "物化到目标 seq 后 outbox 必须清空"
    );
}

#[test]
fn slow_disk_does_not_block_scheduling() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let clock = ManualClock::new();
    let mut session_options = options(PEER_A);
    session_options.flush_delay = Some(Duration::from_millis(300));
    let mut driver = SessionDriver::open_with_clock(
        &vault,
        "doc-1",
        driver_options(session_options, FlushPolicy::default()),
        Box::new(clock),
    )
    .expect("打开驱动");

    let started = Instant::now();
    driver.apply(request(driver.session(), "c1", 0, vec![edit(0, 0, "甲")]));
    let ticket = driver.flush(FlushReason::Manual);
    let _ = driver.pump();
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_millis(150),
        "慢盘下 apply/flush/pump 都不得阻塞调用方（实际 {elapsed:?}）"
    );
    assert_eq!(ticket.target_live_rev, LiveRev(1));

    // 关闭时有界排空：慢盘 300ms 也必须在超时内完成
    let report = driver.close(Duration::from_secs(5));
    assert!(!report.timed_out, "Close 应在超时内排空：{report:?}");
    assert_eq!(driver.pending_edits(), 0);
}

#[test]
fn auto_ticker_triggers_flush_without_explicit_tick() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let policy = FlushPolicy {
        interval: Duration::from_millis(30),
        ..FlushPolicy::default()
    };
    let mut driver = SessionDriver::open(
        &vault,
        "doc-1",
        DriverOptions {
            session: options(PEER_A),
            policy,
            auto_tick: true,
            tick_interval: Some(Duration::from_millis(5)),
        },
    )
    .expect("打开驱动");

    driver.apply(request(driver.session(), "c1", 0, vec![edit(0, 0, "甲")]));
    let ack = wait_durable(&mut driver, LiveRev(1), Duration::from_secs(3))
        .expect("ticker 应自动触发 flush");
    assert_eq!(ack.through_live_rev, LiveRev(1));
    assert!(driver.wait_materialize(Duration::from_secs(3)));
    assert_eq!(
        load_body_state(&vault, "note-1")
            .expect("状态")
            .body_md
            .as_deref(),
        Some("甲")
    );
}

#[test]
fn materialize_failure_is_recorded_and_retriable() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let clock = ManualClock::new();
    let mut driver = open_driver(&vault, clock, FlushPolicy::default());

    driver.apply(request(driver.session(), "c1", 0, vec![edit(0, 0, "甲")]));
    // 模拟「笔记行还没投影出来」：B 会失败
    vault
        .exec("DELETE FROM notes WHERE note_id = 'note-1'", &[])
        .expect("删除笔记行");
    driver.flush(FlushReason::Manual);

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut failure = None;
    while Instant::now() < deadline && failure.is_none() {
        for event in driver.pump() {
            if let DriverEvent::MaterializeFailed { note_id, message } = event {
                failure = Some((note_id, message));
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let (_note_hint, message) = failure.expect("B 失败必须被记录并上报");
    assert!(
        message.contains("笔记行"),
        "失败信息要说清原因，实际：{message}"
    );

    // 补回笔记行后重试
    create_note_slot(&vault, "note-1", "doc-1", "测试笔记", 1).expect("补回笔记行");
    assert_eq!(driver.retry_failed(), 1, "失败任务可重试");
    assert!(driver.wait_materialize(Duration::from_secs(5)));
    assert_eq!(
        load_body_state(&vault, "note-1")
            .expect("状态")
            .body_md
            .as_deref(),
        Some("甲"),
        "重试后 B 必须成功"
    );
}

#[test]
fn manual_flush_cancels_the_pending_window() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let clock = ManualClock::new();
    let mut driver = open_driver(&vault, clock.clone(), FlushPolicy::default());

    driver.apply(request(driver.session(), "c1", 0, vec![edit(0, 0, "甲")]));
    assert!(driver.next_deadline().is_some());
    driver.flush(FlushReason::Manual);
    assert_eq!(driver.next_deadline(), None, "手动 flush 后窗口作废");
    let scheduled = driver.pump();
    assert_eq!(flush_events(&scheduled), vec![FlushReason::Manual]);

    // 时钟越过窗口也不会再触发第二次 flush
    clock.set(Duration::from_secs(10));
    let events = driver.tick(clock.get());
    assert!(
        flush_events(&events).is_empty(),
        "没有新 dirty 不应重复 flush：{events:?}"
    );
    wait_durable(&mut driver, LiveRev(1), Duration::from_secs(5)).expect("ack");
}
