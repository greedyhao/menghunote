//! `SessionDriver` 的 undo/redo/history 转发与 dirty 规则（切片 6b 支撑）。

mod support;

use std::time::{Duration, Instant};

use menghunote_storage::session::{
    ChangeSource, DocumentSession, DriverEvent, DriverOptions, DurableAck, EditOutcome,
    EditRequest, FlushPolicy, FlushReason, LiveRev, ManualClock, RejectReason, ScalarEdit,
    SessionDriver, SessionEpoch, SessionOptions,
};
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

fn options(peer: u64, capacity: usize) -> SessionOptions {
    SessionOptions {
        peer_id: peer,
        session_epoch: SessionEpoch(15),
        journal_capacity: capacity,
        ..SessionOptions::default()
    }
}

fn open_driver(
    vault: &menghunote_storage::Vault,
    clock: ManualClock,
    capacity: usize,
) -> SessionDriver {
    open_driver_with(vault, clock, capacity, None)
}

fn open_driver_with(
    vault: &menghunote_storage::Vault,
    clock: ManualClock,
    capacity: usize,
    materialize_delay: Option<Duration>,
) -> SessionDriver {
    SessionDriver::open_with_clock_and_materialize_delay(
        vault,
        "doc-1",
        DriverOptions {
            session: options(PEER_A, capacity),
            policy: FlushPolicy::default(),
            auto_tick: false,
            tick_interval: None,
        },
        Box::new(clock),
        materialize_delay,
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

fn applied(outcome: &Option<EditOutcome>) -> &menghunote_storage::session::Applied {
    match outcome {
        Some(EditOutcome::Applied(applied)) => applied,
        other => panic!("期望 Applied，实际 {other:?}"),
    }
}

/// 把当前 dirty 全部排空（flush + 等 durable + 等 B 物化），并丢掉收尾事件。
///
/// 注意：不能假设「`pump()` 永远为空」——B 物化是后台线程，它的 `Materialized`
/// 事件可能在任意时刻出现（见 task-27 的 flaky 根因）。这里的职责是把状态**收敛**，
/// 之后的断言只针对本测试真正关心的语义（是否 dirty / 是否 flush）。
fn settle(driver: &mut SessionDriver, target: LiveRev) {
    driver.flush(FlushReason::Manual);
    wait_durable(driver, target, Duration::from_secs(5)).expect("DurableAck");
    assert_eq!(driver.pending_edits(), 0);
    assert_eq!(driver.next_deadline(), None, "排空后不应再有等待窗口");
    assert!(
        driver.wait_materialize(Duration::from_secs(5)),
        "B 物化应排空"
    );
    let _ = driver.pump();
}

/// 断言「没有新的 flush 事件」= 没有新的 dirty（后台 B 物化事件不算）。
fn assert_no_flush_events(driver: &mut SessionDriver, context: &str) {
    let events = driver.pump();
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, DriverEvent::FlushScheduled { .. })),
        "{context}：不应产生 flush 事件，实际：{events:?}"
    );
}

#[test]
fn undo_marks_dirty_and_becomes_durable() {
    let (dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let clock = ManualClock::new();
    let mut driver = open_driver(&vault, clock, 64);

    driver.apply(request(driver.session(), "c1", 0, vec![edit(0, 0, "甲")]));
    settle(&mut driver, LiveRev(1));
    assert_eq!(driver.markdown(), "甲");

    // undo = 正文改动：必须 mark_dirty（窗口重新开始）并且能落 durable
    let undone = driver.undo().expect("undo 不报错");
    let undone = applied(&undone);
    assert_eq!(undone.source, ChangeSource::Undo);
    assert_eq!(undone.live_rev, LiveRev(2));
    assert!(
        !undone.local_echo,
        "撤销不是本地输入，调用方按 splice 更新镜像"
    );
    assert_eq!(driver.markdown(), "");
    assert_eq!(
        driver.next_deadline(),
        Some(Duration::from_millis(250)),
        "undo 必须重新计入 dirty 窗口"
    );

    driver.flush(FlushReason::Manual);
    let ack =
        wait_durable(&mut driver, LiveRev(2), Duration::from_secs(5)).expect("undo 也要 durable");
    assert_eq!(ack.through_live_rev, LiveRev(2));
    drop(driver);

    // 撤销结果真的落库：重开会话正文为空
    let session = DocumentSession::open(&vault, "doc-1", options(PEER_A, 64)).expect("重开");
    assert_eq!(session.markdown(), "");
    let _ = dir;
}

#[test]
fn undo_with_empty_stack_does_not_mark_dirty() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let clock = ManualClock::new();
    let mut driver = open_driver(&vault, clock, 64);

    driver.apply(request(driver.session(), "c1", 0, vec![edit(0, 0, "甲")]));
    settle(&mut driver, LiveRev(1));
    driver.undo().expect("第一次 undo");
    settle(&mut driver, LiveRev(2));
    assert_eq!(driver.markdown(), "");

    // 栈空：Ok(None)，且不产生 dirty / 不进 journal
    let empty = driver.undo().expect("空栈 undo 不报错");
    assert!(empty.is_none(), "没有可撤销项应返回 None");
    assert_eq!(driver.pending_edits(), 0);
    assert_eq!(driver.next_deadline(), None, "空栈 undo 不得 dirty");
    assert_no_flush_events(&mut driver, "空栈 undo");

    // 连按多次仍然不 dirty
    for _ in 0..3 {
        assert!(driver.undo().expect("空栈 undo").is_none());
    }
    assert_eq!(driver.next_deadline(), None);
    assert_eq!(driver.pending_edits(), 0);
    assert_eq!(driver.markdown(), "");
}

#[test]
fn empty_undo_reports_no_flush_while_body_materialization_is_in_flight() {
    // task-27 的回归测试：旧断言用「pump() 完全为空」表达「没有 dirty」，
    // 但 B 物化是后台线程，它的 Materialized 事件可以在任意时刻出现。
    // 这里用故障注入让 B **确定性地迟到**，把「后台事件与断言窗口重叠」变成必然，
    // 再断言真正的语义：没有 FlushScheduled、next_deadline 始终为 None。
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let clock = ManualClock::new();
    let mut driver = open_driver_with(&vault, clock, 64, Some(Duration::from_millis(200)));

    // 一次编辑 + 撤销：撤销栈清空，且两批 update 都要落盘
    driver.apply(request(driver.session(), "c1", 0, vec![edit(0, 0, "甲")]));
    driver.undo().expect("undo");
    driver.flush(FlushReason::Manual);
    wait_durable(&mut driver, LiveRev(2), Duration::from_secs(5)).expect("DurableAck");
    assert!(!driver.history().can_undo, "撤销后栈应清空");

    // 空栈 undo：既不能 dirty，也不能产生 flush 事件
    assert!(driver.undo().expect("空栈 undo").is_none());
    assert_eq!(driver.next_deadline(), None, "空栈 undo 不得 dirty");
    assert_eq!(driver.pending_edits(), 0);

    // 接下来几秒里 B 物化结果一定会到达（延迟注入 200ms；给足余量避免在慢机上误判）
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut saw_materialize = 0usize;
    while Instant::now() < deadline && saw_materialize == 0 {
        for event in driver.pump() {
            match event {
                DriverEvent::Materialized(_) => saw_materialize += 1,
                DriverEvent::FlushScheduled { reason, ticket } => panic!(
                    "空栈 undo 之后出现 flush 事件（reason={reason:?}, target={:?}）",
                    ticket.target_live_rev
                ),
                _ => {}
            }
        }
        assert_eq!(driver.next_deadline(), None, "整个窗口都不得 dirty");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        saw_materialize >= 1,
        "延迟注入下应能观测到迟到的 B 物化事件（用来证明窗口里确实有后台事件）"
    );

    // 等待辅助的契约：返回后不应再冒出上一次等待之前就该完成的 B 结果
    assert!(driver.wait_materialize(Duration::from_secs(5)));
    let leftover = driver.pump();
    assert!(
        !leftover
            .iter()
            .any(|event| matches!(event, DriverEvent::Materialized(_))),
        "wait_materialize 返回后仍有 B 结果：{leftover:?}"
    );
}

#[test]
fn undo_during_composition_is_rejected_without_dirty() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let clock = ManualClock::new();
    let mut driver = open_driver(&vault, clock, 64);

    driver.apply(request(driver.session(), "c1", 0, vec![edit(0, 0, "甲")]));
    settle(&mut driver, LiveRev(1));
    assert_eq!(driver.markdown(), "甲");

    driver.begin_composition();
    let blocked = driver.undo().expect("组合期间 undo 不报错");
    match blocked {
        Some(EditOutcome::Rejected(rejected)) => {
            assert_eq!(rejected.reason, RejectReason::CompositionActive);
        }
        other => panic!("组合期间 undo 必须被拒，实际 {other:?}"),
    }
    assert_eq!(driver.markdown(), "甲", "被拒的 undo 不得改文本");
    assert_eq!(driver.pending_edits(), 0, "被拒的 undo 不得进 journal");
    assert_eq!(driver.next_deadline(), None, "被拒的 undo 不得 dirty");
    assert_no_flush_events(&mut driver, "组合态被拒的 undo");

    // 结束组合后 undo 恢复可用
    assert!(driver.end_composition().is_empty());
    let undone = driver.undo().expect("结束后 undo");
    assert_eq!(applied(&undone).live_rev, LiveRev(2));
    assert_eq!(driver.markdown(), "");
}

#[test]
fn redo_marks_dirty_and_becomes_durable() {
    let (dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let clock = ManualClock::new();
    let mut driver = open_driver(&vault, clock, 64);

    driver.apply(request(driver.session(), "c1", 0, vec![edit(0, 0, "甲")]));
    settle(&mut driver, LiveRev(1));
    driver.undo().expect("undo");
    settle(&mut driver, LiveRev(2));
    assert_eq!(driver.markdown(), "");

    // redo 也是正文改动：dirty + durable
    let redone = driver.redo().expect("redo 不报错");
    let redone = applied(&redone);
    assert_eq!(redone.source, ChangeSource::Redo);
    assert_eq!(redone.live_rev, LiveRev(3));
    assert!(!redone.local_echo);
    assert_eq!(driver.markdown(), "甲");
    assert_eq!(driver.next_deadline(), Some(Duration::from_millis(250)));

    driver.flush(FlushReason::Manual);
    let ack = wait_durable(&mut driver, LiveRev(3), Duration::from_secs(5)).expect("redo durable");
    assert_eq!(ack.through_live_rev, LiveRev(3));
    drop(driver);

    let session = DocumentSession::open(&vault, "doc-1", options(PEER_A, 64)).expect("重开");
    assert_eq!(session.markdown(), "甲", "重做结果要落库");
    let _ = dir;
}

#[test]
fn redo_with_empty_stack_does_not_mark_dirty() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let clock = ManualClock::new();
    let mut driver = open_driver(&vault, clock, 64);

    driver.apply(request(driver.session(), "c1", 0, vec![edit(0, 0, "甲")]));
    settle(&mut driver, LiveRev(1));

    // 没有 undo 过：redo 栈为空
    let empty = driver.redo().expect("空栈 redo 不报错");
    assert!(empty.is_none());
    assert_eq!(driver.next_deadline(), None);
    assert_eq!(driver.pending_edits(), 0);
    assert_no_flush_events(&mut driver, "空栈 redo");
    assert_eq!(driver.markdown(), "甲");
}

#[test]
fn history_counts_reflect_driver_operations() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let clock = ManualClock::new();
    let mut driver = open_driver(&vault, clock, 64);
    assert!(!driver.history().can_undo, "初始没有可撤销项");

    driver.apply(request(driver.session(), "c1", 0, vec![edit(0, 0, "甲")]));
    let after_apply = driver.history();
    assert!(after_apply.can_undo);
    assert_eq!(after_apply.undo_count, 1);
    assert_eq!(after_apply.redo_count, 0);

    driver.undo().expect("undo");
    let after_undo = driver.history();
    assert!(after_undo.can_redo);
    assert_eq!(after_undo.redo_count, 1);

    driver.redo().expect("redo");
    let after_redo = driver.history();
    assert!(after_redo.can_undo);
    assert_eq!(after_redo.redo_count, 0);
}

#[test]
fn undo_under_backpressure_is_rejected_without_dirty() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let clock = ManualClock::new();
    let mut driver = open_driver(&vault, clock, 1);

    driver.apply(request(driver.session(), "c1", 0, vec![edit(0, 0, "甲")]));
    assert_eq!(driver.pending_edits(), 1, "journal 已满");

    let blocked = driver.undo().expect("背压 undo 不报错");
    match blocked {
        Some(EditOutcome::Rejected(rejected)) => {
            assert_eq!(rejected.reason, RejectReason::Backpressure);
        }
        other => panic!("背压时 undo 必须被拒，实际 {other:?}"),
    }
    assert_eq!(driver.markdown(), "甲", "被拒的 undo 不得改文本");
    assert_eq!(driver.pending_edits(), 1, "被拒的 undo 不得进 journal");

    // flush 之后可撤销
    driver.flush(FlushReason::Manual);
    wait_durable(&mut driver, LiveRev(1), Duration::from_secs(5)).expect("ack");
    let undone = driver.undo().expect("释放容量后 undo");
    assert_eq!(applied(&undone).live_rev, LiveRev(2));
    assert_eq!(driver.markdown(), "");
}
