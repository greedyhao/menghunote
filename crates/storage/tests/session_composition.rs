//! composition 闸门（ADR 0002 §7 的存储侧）：延后远端、拒绝本地正文改动、flush 不受影响。

mod support;

use std::time::Duration;

use menghunote_storage::session::{
    ChangeSource, DocumentSession, DriverEvent, DriverOptions, DurableAck, EditOutcome,
    EditRequest, FlushPolicy, FlushReason, LiveRev, ManualClock, RejectReason, ScalarEdit,
    SessionDriver, SessionEpoch, SessionEvent, SessionOptions,
};
use menghunote_storage::{load_body_state, BodyDoc};
use support::*;

/// 远端另一个 peer（PEER_A=1 / PEER_B=2 之外的第三台设备）。
const PEER_C: u64 = 3;

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
        session_epoch: SessionEpoch(13),
        ..SessionOptions::default()
    }
}

fn open(vault: &menghunote_storage::Vault, doc_id: &str) -> DocumentSession {
    DocumentSession::open(vault, doc_id, options(PEER_A)).expect("打开会话")
}

/// 造一条「远端设备」的 update（独立副本 → 导出）。
fn remote_update(doc_id: &str, peer: u64, text: &str) -> Vec<u8> {
    let doc = BodyDoc::create(doc_id, peer).expect("远端副本");
    doc.insert_scalars(0, text).expect("远端编辑");
    doc.export_all_updates().expect("导出 update")
}

fn wait_durable(session: &DocumentSession, target: LiveRev) -> Option<DurableAck> {
    session.wait_for_durable(target, Duration::from_secs(5))
}

#[test]
fn deferred_remote_merges_in_arrival_order_after_composition() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let mut session = open(&vault, "doc-1");

    session.begin_composition();
    assert!(session.is_composing());
    assert_eq!(session.composition_depth(), 1);

    // 组合期间的远端导入：进队列、不应用、不报错
    let first = remote_update("doc-1", PEER_B, "乙");
    let second = remote_update("doc-1", PEER_C, "丙");
    assert_eq!(session.apply_remote(&first).expect("延后不报错"), None);
    assert_eq!(session.apply_remote(&second).expect("延后不报错"), None);
    assert_eq!(session.markdown(), "", "组合期间不得把远端导入合进 live");
    assert_eq!(session.deferred_remote_len(), 2);
    assert_eq!(session.live_rev(), LiveRev(0));

    // 结束组合：按到达顺序串行合并
    let merged = session.end_composition();
    assert_eq!(merged.len(), 2, "两条延后的远端 update 都要合并");
    assert_eq!(merged[0].from_rev, LiveRev(0));
    assert_eq!(merged[0].to_rev, LiveRev(1));
    assert_eq!(merged[1].from_rev, LiveRev(1));
    assert_eq!(merged[1].to_rev, LiveRev(2));
    assert_eq!(merged[0].source, ChangeSource::Remote);
    assert_eq!(session.deferred_remote_len(), 0);
    assert!(!session.is_composing());

    let text = session.markdown();
    assert!(text.contains('乙') && text.contains('丙'), "实际：{text:?}");
    assert_eq!(session.live_rev(), LiveRev(2));

    // 延后合并的 delta **只**从 end_composition 返回，不再重复进事件队列（防镜像双写）
    let events: Vec<_> = session
        .events()
        .into_iter()
        .filter(|event| matches!(event, SessionEvent::Remote(_)))
        .collect();
    assert!(
        events.is_empty(),
        "延后合并不得再推事件（否则调用方会应用两次）：{events:?}"
    );
    assert_eq!(merged[0].splice.at.get(), 0);
    assert_eq!(merged[1].to_rev, LiveRev(2));
}

#[test]
fn local_commands_rejected_during_composition_and_retry_succeeds() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let mut session = open(&vault, "doc-1");
    session.apply(request(&session, "c1", 0, vec![edit(0, 0, "甲")]));

    session.begin_composition();
    let text_before = session.markdown();
    let rev_before = session.live_rev();
    let pending_before = session.pending_len();

    // 其它真实正文改动：拒绝，且不碰 Loro
    let blocked = session.apply(request(&session, "c2", 1, vec![edit(1, 1, "乙")]));
    match blocked {
        EditOutcome::Rejected(rejected) => {
            assert_eq!(rejected.reason, RejectReason::CompositionActive);
            assert_eq!(rejected.command_id, "c2");
        }
        other => panic!("期望 CompositionActive，实际 {other:?}"),
    }
    assert_eq!(session.markdown(), text_before, "拒绝时文本不得变化");
    assert_eq!(session.live_rev(), rev_before);
    assert_eq!(
        session.pending_len(),
        pending_before,
        "被拒请求不得进 journal"
    );

    // undo/redo 同样被闸门拒绝
    match session.undo().expect("undo 不报错") {
        Some(EditOutcome::Rejected(rejected)) => {
            assert_eq!(rejected.reason, RejectReason::CompositionActive)
        }
        other => panic!("undo 在组合期间必须被拒，实际 {other:?}"),
    }
    assert_eq!(session.markdown(), text_before);

    // 结束组合后，同一个 command_id 重试成功
    assert!(session.end_composition().is_empty(), "没有延后的远端");
    let retried = session.apply(request(&session, "c2", 1, vec![edit(1, 1, "乙")]));
    let applied = match retried {
        EditOutcome::Applied(applied) => applied,
        other => panic!("重试应成功，实际 {other:?}"),
    };
    assert_eq!(applied.live_rev, LiveRev(2));
    assert_eq!(session.markdown(), "甲乙");
}

#[test]
fn nested_begin_counts_and_repeated_end_is_a_noop() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let mut session = open(&vault, "doc-1");
    session.begin_composition();

    // 嵌套 begin：深度计数
    assert_eq!(session.begin_composition(), 2);
    assert_eq!(session.composition_depth(), 2);
    session
        .apply_remote(&remote_update("doc-1", PEER_B, "乙"))
        .expect("延后");

    // 内层 end：仍在组合中，不得合并
    assert!(session.end_composition().is_empty());
    assert_eq!(session.composition_depth(), 1);
    assert!(session.is_composing());
    assert_eq!(session.deferred_remote_len(), 1, "内层结束不得提前合并");
    assert_eq!(session.markdown(), "");

    // 最外层 end：合并
    assert_eq!(session.end_composition().len(), 1);
    assert_eq!(session.composition_depth(), 0);
    assert!(!session.is_composing());
    assert!(session.markdown().contains('乙'));

    // 多余的 end：no-op（平台可能重复回调 cancel）
    assert!(session.end_composition().is_empty());
    assert_eq!(session.composition_depth(), 0);
    assert!(!session.is_composing());
    assert!(session.markdown().contains('乙'));
}

#[test]
fn flush_still_works_during_composition() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let mut session = open(&vault, "doc-1");
    session.apply(request(&session, "c1", 0, vec![edit(0, 0, "甲")]));

    session.begin_composition();
    // 组合期间 flush 已提交内容必须照常工作（闸门只管新的正文改动）
    let ticket = session.flush(FlushReason::Manual);
    assert_eq!(ticket.target_live_rev, LiveRev(1));
    let ack = wait_durable(&session, LiveRev(1)).expect("组合期间也要能 durable");
    assert_eq!(ack.head_seq, 1);
    assert_eq!(session.pending_len(), 0);
    assert!(session.is_composing(), "flush 不改变组合状态");
    assert_eq!(session.markdown(), "甲");
}

#[test]
fn backpressure_keeps_deferred_remote_until_flush() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let mut session = DocumentSession::open(
        &vault,
        "doc-1",
        SessionOptions {
            journal_capacity: 1,
            ..options(PEER_A)
        },
    )
    .expect("打开会话");

    session.apply(request(&session, "c1", 0, vec![edit(0, 0, "甲")]));
    assert_eq!(session.pending_len(), 1, "journal 已满");

    session.begin_composition();
    // 组合期间远端先入队（闸门在容量判断之前），不会被背压拒掉
    session
        .apply_remote(&remote_update("doc-1", PEER_B, "乙"))
        .expect("延后");
    assert_eq!(session.deferred_remote_len(), 1);

    // 结束组合时容量不足：合并停止、队列保留（不丢远端数据）
    assert!(session.end_composition().is_empty());
    assert_eq!(session.deferred_remote_len(), 1);
    assert_eq!(session.markdown(), "甲");

    // flush 之后再合并
    session.flush(FlushReason::Manual);
    wait_durable(&session, LiveRev(1)).expect("durable");
    let merged = session.merge_deferred_remote();
    assert_eq!(merged.len(), 1);
    assert_eq!(session.deferred_remote_len(), 0);
    assert!(session.markdown().contains('乙'));
}

#[test]
fn another_documents_session_is_not_affected() {
    let (_dir, vault) = fresh_vault();
    let _seed_one = seed_note(&vault, "note-1", "doc-1");
    let _seed_two = seed_note(&vault, "note-2", "doc-2");

    let mut first = open(&vault, "doc-1");
    let mut second = open(&vault, "doc-2");
    first.begin_composition();

    // 另一个 Doc 的会话不受影响（闸门是 per-session、单 Doc）
    let outcome = second.apply(request(&second, "other-1", 0, vec![edit(0, 0, "别处")]));
    assert!(matches!(outcome, EditOutcome::Applied(_)));
    assert_eq!(second.markdown(), "别处");
    second
        .apply_remote(&remote_update("doc-2", PEER_B, "远端"))
        .expect("别的 Doc 的远端导入不受闸门影响");
    assert!(second.markdown().contains("别处"));

    assert!(first.is_composing());
    assert_eq!(first.markdown(), "");
    first.end_composition();
}

#[test]
fn driver_gate_merges_deferred_and_flushes_them() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let clock = ManualClock::new();
    let mut driver = SessionDriver::open_with_clock(
        &vault,
        "doc-1",
        DriverOptions {
            session: options(PEER_A),
            policy: FlushPolicy::default(),
            auto_tick: false,
            tick_interval: None,
        },
        Box::new(clock),
    )
    .expect("打开驱动");

    driver.begin_composition();
    assert!(driver.is_composing());
    // 本地命令被拒
    match driver.apply(request(driver.session(), "c1", 0, vec![edit(0, 0, "甲")])) {
        EditOutcome::Rejected(rejected) => {
            assert_eq!(rejected.reason, RejectReason::CompositionActive)
        }
        other => panic!("期望 CompositionActive，实际 {other:?}"),
    }
    // 远端延后
    assert_eq!(
        driver
            .apply_remote(&remote_update("doc-1", PEER_B, "乙"))
            .expect("延后"),
        None
    );
    assert_eq!(driver.deferred_remote_len(), 1);

    // 结束组合：合并 + 计入 dirty；随后 flush 必须落 durable 并物化
    let merged = driver.end_composition();
    assert_eq!(merged.len(), 1);
    assert!(!driver.is_composing());
    assert!(driver.markdown().contains('乙'));

    driver.flush(FlushReason::Manual);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut durable = false;
    while std::time::Instant::now() < deadline && !durable {
        for event in driver.pump() {
            if let DriverEvent::Durable(ack) = event {
                if ack.through_live_rev >= LiveRev(1) {
                    durable = true;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(durable, "结束组合后的远端 op 必须能 durable");
    assert!(driver.wait_materialize(Duration::from_secs(5)));
    assert_eq!(
        load_body_state(&vault, "note-1")
            .expect("状态")
            .body_md
            .as_deref(),
        Some("乙")
    );
}
