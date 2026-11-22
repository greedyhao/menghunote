//! 后台 flush 与 durable 契约：异步票据、ACK 顺序、崩溃后重放。

mod support;

use std::time::{Duration, Instant};

use menghunote_storage::session::{
    DocumentSession, EditOutcome, EditRequest, FlushReason, LiveRev, ScalarEdit, SessionEpoch,
    SessionEvent, SessionOptions,
};
use menghunote_storage::{load_body_state, materialize_body, BodyDoc, Vault};
use support::*;

fn options(peer: u64) -> SessionOptions {
    SessionOptions {
        peer_id: peer,
        session_epoch: SessionEpoch(3),
        ..SessionOptions::default()
    }
}

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

fn open_session(vault: &Vault, options: SessionOptions) -> DocumentSession {
    DocumentSession::open(vault, "doc-1", options).expect("打开会话")
}

#[test]
fn flush_returns_ticket_without_waiting_for_sqlite() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let options = SessionOptions {
        flush_delay: Some(Duration::from_millis(300)),
        ..options(PEER_A)
    };
    let mut session = open_session(&vault, options);
    session.apply(request(&session, "c1", 0, vec![edit(0, 0, "甲")]));

    let started = Instant::now();
    let ticket = session.flush(FlushReason::Manual);
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_millis(150),
        "flush 必须在调用线程立即返回票据（实际 {elapsed:?}）"
    );
    assert_eq!(ticket.target_live_rev, LiveRev(1));
    assert!(
        session.events().is_empty(),
        "票据返回时后台还没写完（延迟注入生效）"
    );

    // ACK 只会异步到达
    let ack = session
        .wait_for_durable(ticket.target_live_rev, Duration::from_secs(5))
        .expect("应收到 DurableAck");
    assert_eq!(ack.through_live_rev, LiveRev(1));
    assert_eq!(ack.head_seq, 1);
    assert!(!ack.head_frontier.is_empty(), "ACK 要带 durable frontier");
    assert_eq!(session.durable_head_seq(), 1);
    assert_eq!(session.durable_through(), LiveRev(1));
}

#[test]
fn durable_acks_are_monotonic_and_cover_each_target() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let mut session = open_session(&vault, options(PEER_A));

    let mut acks = Vec::new();
    for (index, ch) in ["一", "二", "三"].iter().enumerate() {
        let command = format!("c{}", index + 1);
        let expected_rev = LiveRev(index as u64 + 1);
        let outcome = session.apply(request(
            &session,
            &command,
            index as u64,
            vec![edit(index, index, ch)],
        ));
        assert!(matches!(outcome, EditOutcome::Applied(_)));
        let ticket = session.flush(FlushReason::Interval);
        assert_eq!(ticket.target_live_rev, expected_rev);
        let ack = session
            .wait_for_durable(ticket.target_live_rev, Duration::from_secs(5))
            .expect("应收到 DurableAck");
        assert_eq!(ack.through_live_rev, expected_rev);
        assert_eq!(ack.head_seq, index as u64 + 1, "head_seq 逐批递增");
        acks.push(ack);
    }

    // ACK 的批次号与水位严格递增
    for pair in acks.windows(2) {
        assert!(pair[1].batch_id > pair[0].batch_id, "batch 必须递增");
        assert!(
            pair[1].through_live_rev > pair[0].through_live_rev,
            "through_live_rev 必须递增"
        );
    }
    assert_eq!(session.markdown(), "一二三");
    assert_eq!(session.pending_len(), 0);
}

#[test]
fn crash_after_a_before_b_recovers_on_reopen() {
    let (dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let mut session = open_session(&vault, options(PEER_A));
    session.apply(request(
        &session,
        "c1",
        0,
        vec![edit(0, 0, "A中😀e\u{301}")],
    ));
    let ticket = session.flush(FlushReason::Manual);
    session
        .wait_for_durable(ticket.target_live_rev, Duration::from_secs(5))
        .expect("durable");

    // A 已提交、B 未完成：此时崩溃（直接丢会话与库句柄）
    assert!(load_body_state(&vault, "note-1")
        .expect("状态")
        .body_md
        .is_none());
    drop(session);
    drop(vault);

    // 重开：正文从 loro_updates 重放出来，durable 水位还在
    let vault = open_vault(&vault_path(&dir));
    let session = open_session(&vault, options(PEER_A));
    let snapshot = session.snapshot();
    assert_eq!(snapshot.markdown, "A中😀e\u{301}");
    assert_eq!(snapshot.durable_head_seq, 1);
    assert_eq!(snapshot.live_rev, LiveRev(0), "live_rev 是新会话的基准");

    // B 物化补齐
    materialize_body(&vault, "note-1", "doc-1", 1, PEER_A).expect("B 物化");
    let state = load_body_state(&vault, "note-1").expect("状态");
    assert_eq!(state.body_md.as_deref(), Some("A中😀e\u{301}"));
    assert_eq!(state.body_seq, 1);
}

#[test]
fn unflushed_edits_are_lost_on_reopen_but_durable_prefix_survives() {
    let (dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let mut session = open_session(&vault, options(PEER_A));

    session.apply(request(&session, "c1", 0, vec![edit(0, 0, "甲")]));
    session.apply(request(&session, "c2", 1, vec![edit(1, 1, "乙")]));
    let ticket = session.flush(FlushReason::Manual);
    session
        .wait_for_durable(ticket.target_live_rev, Duration::from_secs(5))
        .expect("durable");

    // 这两条没 flush：只存在于内存与 Loro 的未 ACK 状态
    session.apply(request(&session, "c3", 2, vec![edit(2, 2, "丙")]));
    session.apply(request(&session, "c4", 3, vec![edit(3, 3, "丁")]));
    assert_eq!(session.markdown(), "甲乙丙丁");
    drop(session);
    drop(vault);

    // 重开：只保留已 ACK 的连续前缀（甲乙），未 ACK 的丙丁丢掉——「只丢连续前缀之后」
    let vault = open_vault(&vault_path(&dir));
    let session = open_session(&vault, options(PEER_A));
    assert_eq!(session.markdown(), "甲乙");
    // 两条 update 各自占一个 seq
    assert_eq!(session.snapshot().durable_head_seq, 2);
    assert_eq!(
        vault
            .query(
                "SELECT count(*) FROM loro_updates WHERE doc_id = 'doc-1'",
                &[]
            )
            .expect("count")
            .len(),
        1
    );
    assert_eq!(
        count(
            &vault,
            "SELECT count(*) FROM loro_updates WHERE doc_id = 'doc-1'",
            &[]
        ),
        2,
        "只有已 ACK 的两条 update 落库"
    );
}

#[test]
fn flush_without_pending_edits_is_harmless() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let mut session = open_session(&vault, options(PEER_A));

    let ticket = session.flush(FlushReason::Manual);
    assert_eq!(ticket.target_live_rev, LiveRev(0));
    assert!(session
        .wait_for_durable(ticket.target_live_rev, Duration::from_secs(5))
        .is_some());
    assert_eq!(session.durable_head_seq(), 0);
    assert_eq!(session.markdown(), "");
}

#[test]
fn remote_event_is_pushed_to_the_queue() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let mut session = open_session(&vault, options(PEER_A));

    let remote = BodyDoc::create("doc-1", PEER_B).expect("远端副本");
    remote.insert_scalars(0, "远端").expect("远端编辑");
    let update = remote.export_all_updates().expect("导出");
    session
        .apply_remote(&update)
        .expect("导入")
        .expect("有变更");

    let events = session.events();
    let remote_events = events
        .iter()
        .filter(|event| matches!(event, SessionEvent::Remote(_)))
        .count();
    assert_eq!(remote_events, 1, "远端变更要进事件队列");
    assert!(events
        .iter()
        .any(|event| matches!(event, SessionEvent::Remote(delta) if delta.to_rev == LiveRev(1))));
    assert!(session.events().is_empty(), "事件队列是取走语义");
}
