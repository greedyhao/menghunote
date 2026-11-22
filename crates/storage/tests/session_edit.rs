//! 会话核：受理顺序、幂等、stale、背压、撤销不回退远端。

mod support;

use std::time::Duration;

use menghunote_storage::session::{
    ChangeSource, DocumentSession, EditOutcome, EditRequest, FlushReason, Generation, LiveRev,
    PortError, RejectReason, ScalarEdit, SessionEpoch, SessionOptions,
};
use menghunote_storage::{
    append_updates, load_body_state, load_head, materialize_body, BodyDoc, UpdateRecord,
};
use support::*;

const NFD: &str = "A中😀e\u{301}";

fn options(peer: u64) -> SessionOptions {
    SessionOptions {
        peer_id: peer,
        session_epoch: SessionEpoch(7),
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

fn applied(outcome: &EditOutcome) -> &menghunote_storage::session::Applied {
    match outcome {
        EditOutcome::Applied(applied) => applied,
        other => panic!("期望 Applied，实际 {other:?}"),
    }
}

/// 建库 + seeds 一个正文文档行/笔记行，返回会话。
fn open_session(
    capacity: usize,
) -> (
    tempfile::TempDir,
    menghunote_storage::Vault,
    DocumentSession,
) {
    let (dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let options = SessionOptions {
        journal_capacity: capacity,
        ..options(PEER_A)
    };
    let session = DocumentSession::open(&vault, "doc-1", options).expect("打开会话");
    (dir, vault, session)
}

#[test]
fn apply_edits_in_reverse_order_and_advance_live_rev() {
    let (_dir, _vault, mut session) = open_session(64);
    let snapshot = session.snapshot();
    assert_eq!(snapshot.live_rev, LiveRev(0));
    assert_eq!(snapshot.markdown, "");
    assert_eq!(snapshot.durable_head_seq, 0);
    assert_eq!(snapshot.session_epoch, SessionEpoch(7));

    // 第一次：整段插入（初始基准 0）
    let first = session.apply(request(&session, "c1", 0, vec![edit(0, 0, NFD)]));
    let first = applied(&first);
    assert_eq!(first.live_rev, LiveRev(1));
    assert!(first.local_echo, "本地输入不得二次应用 splice");
    assert_eq!(first.source, ChangeSource::Local);
    assert_eq!(first.splice.delete_scalars, 0);
    assert_eq!(first.splice.insert, NFD);
    assert_eq!(session.markdown(), NFD);
    assert_eq!(session.pending_len(), 1);
    assert_eq!(session.live_rev(), LiveRev(1));

    // 第二次：两处互不重叠的替换，必须按**逆序**应用
    //   [0,1) → "X"；[2,3) → "Y"（相对基准 1 的坐标）
    let second = session.apply(request(
        &session,
        "c2",
        1,
        vec![edit(0, 1, "X"), edit(2, 3, "Y")],
    ));
    let second = applied(&second);
    assert_eq!(second.live_rev, LiveRev(2));
    // 逆序：先改 [2,3) → "A中Ye\u{301}"，再改 [0,1) → "X中Ye\u{301}"
    assert_eq!(session.markdown(), "X中Ye\u{301}");
    assert_eq!(session.pending_len(), 2);
}

#[test]
fn stale_base_is_rejected_without_touching_text() {
    let (_dir, _vault, mut session) = open_session(64);
    session.apply(request(&session, "c1", 0, vec![edit(0, 0, "甲")]));
    let before = session.markdown();

    let outcome = session.apply(request(&session, "c2", 0, vec![edit(1, 1, "乙")]));
    match outcome {
        EditOutcome::Stale(stale) => {
            assert_eq!(stale.current_live_rev, LiveRev(1), "告诉调用方新基准");
            assert_eq!(stale.command_id, "c2");
        }
        other => panic!("期望 Stale，实际 {other:?}"),
    }
    assert_eq!(session.markdown(), before, "stale 请求不得修改文本");
    assert_eq!(session.pending_len(), 1);
    assert_eq!(session.live_rev(), LiveRev(1));
}

#[test]
fn wrong_generation_epoch_and_doc_are_rejected() {
    let (_dir, _vault, mut session) = open_session(64);

    let mut bad_generation = request(&session, "g1", 0, vec![edit(0, 0, "x")]);
    bad_generation.generation = Generation(99);
    assert!(matches!(
        session.apply(bad_generation),
        EditOutcome::Rejected(ref rejected) if rejected.reason == RejectReason::WrongGeneration
    ));

    let mut bad_epoch = request(&session, "e1", 0, vec![edit(0, 0, "x")]);
    bad_epoch.session_epoch = SessionEpoch(1234);
    assert!(matches!(
        session.apply(bad_epoch),
        EditOutcome::Rejected(ref rejected) if rejected.reason == RejectReason::WrongEpoch
    ));

    let mut bad_doc = request(&session, "d1", 0, vec![edit(0, 0, "x")]);
    bad_doc.doc_id = "doc-other".to_owned();
    assert!(matches!(
        session.apply(bad_doc),
        EditOutcome::Rejected(ref rejected) if rejected.reason == RejectReason::UnknownDoc
    ));

    assert_eq!(session.markdown(), "");
    assert_eq!(session.pending_len(), 0);
}

#[test]
fn duplicate_command_id_returns_first_outcome_without_reapplying() {
    let (_dir, _vault, mut session) = open_session(64);
    let first = session.apply(request(&session, "same", 0, vec![edit(0, 0, "甲")]));
    let text_after_first = session.markdown();
    let rev_after_first = session.live_rev();

    // 同一个 command_id 再发一次（即便基准已经过期）：必须返回首次结果
    let second = session.apply(request(&session, "same", 0, vec![edit(0, 0, "甲")]));
    assert_eq!(first, second, "重复 command 必须得到同一结果");
    assert_eq!(session.markdown(), text_after_first, "不得重复应用");
    assert_eq!(session.pending_len(), 1, "不得产生第二条 journal 条目");
    assert_eq!(session.live_rev(), rev_after_first);
}

#[test]
fn backpressure_rejects_before_touching_loro() {
    let (_dir, _vault, mut session) = open_session(2);
    session.apply(request(&session, "c1", 0, vec![edit(0, 0, "甲")]));
    session.apply(request(&session, "c2", 1, vec![edit(1, 1, "乙")]));
    assert_eq!(session.pending_len(), 2);

    let text_before = session.markdown();
    let rev_before = session.live_rev();
    let outcome = session.apply(request(&session, "c3", 2, vec![edit(2, 2, "丙")]));
    match outcome {
        EditOutcome::Rejected(rejected) => assert_eq!(rejected.reason, RejectReason::Backpressure),
        other => panic!("期望 Backpressure，实际 {other:?}"),
    }
    assert_eq!(session.markdown(), text_before, "背压时 Loro 必须未被修改");
    assert_eq!(session.live_rev(), rev_before);
    assert_eq!(session.pending_len(), 2, "被拒请求不能进 journal");

    // flush 之后容量释放，同一个命令（新 command_id）可以成功
    let ticket = session.flush(FlushReason::Manual);
    assert!(session
        .wait_for_durable(ticket.target_live_rev, Duration::from_secs(5))
        .is_some());
    assert_eq!(session.pending_len(), 0);

    let outcome = session.apply(request(&session, "c3b", 2, vec![edit(2, 2, "丙")]));
    assert_eq!(applied(&outcome).live_rev, LiveRev(3));
    assert_eq!(session.markdown(), "甲乙丙");
}

#[test]
fn invalid_ranges_are_rejected_before_any_change() {
    let (_dir, _vault, mut session) = open_session(64);
    session.apply(request(&session, "c1", 0, vec![edit(0, 0, NFD)]));
    let text_before = session.markdown();

    // 重叠（[0,2) 与 [1,3)）
    let overlap = session.apply(request(
        &session,
        "bad-1",
        1,
        vec![edit(0, 2, "x"), edit(1, 3, "y")],
    ));
    assert!(matches!(
        overlap,
        EditOutcome::Rejected(ref rejected) if rejected.reason == RejectReason::InvalidRange
    ));

    // 未排序（后面的区间在前）
    let unsorted = session.apply(request(
        &session,
        "bad-2",
        1,
        vec![edit(3, 4, "x"), edit(0, 1, "y")],
    ));
    assert!(matches!(
        unsorted,
        EditOutcome::Rejected(ref rejected) if rejected.reason == RejectReason::InvalidRange
    ));

    // 越界（文本只有 5 个 scalar）
    let out_of_bounds = session.apply(request(&session, "bad-3", 1, vec![edit(0, 6, "x")]));
    assert!(matches!(
        out_of_bounds,
        EditOutcome::Rejected(ref rejected) if rejected.reason == RejectReason::InvalidRange
    ));

    // 起止倒序
    let reversed = session.apply(request(&session, "bad-4", 1, vec![edit(3, 2, "x")]));
    assert!(matches!(
        reversed,
        EditOutcome::Rejected(ref rejected) if rejected.reason == RejectReason::InvalidRange
    ));

    assert_eq!(session.markdown(), text_before);
    assert_eq!(session.pending_len(), 1);
}

#[test]
fn undo_does_not_revert_remote_characters() {
    let (_dir, _vault, mut session) = open_session(64);

    // 本地输入（peer A），留成一步撤销
    let local = session.apply(request(&session, "local-1", 0, vec![edit(0, 0, "甲")]));
    assert_eq!(applied(&local).live_rev, LiveRev(1));
    assert_eq!(session.history().undo_count, 1);

    // 远端副本（同一 doc_id、另一个 peer）并发写入，导入到 live
    let remote = BodyDoc::create("doc-1", PEER_B).expect("远端副本");
    remote.insert_scalars(0, "😀").expect("远端编辑");
    let update = remote.export_all_updates().expect("导出远端 update");
    let delta = session
        .apply_remote(&update)
        .expect("远端导入")
        .expect("应有变更");
    assert_eq!(delta.source, ChangeSource::Remote);
    assert_eq!(delta.from_rev, LiveRev(1));
    assert_eq!(delta.to_rev, LiveRev(2));
    assert!(session.markdown().contains('😀'));
    assert!(session.markdown().contains('甲'));
    assert!(
        session
            .events()
            .iter()
            .any(|event| matches!(event, menghunote_storage::session::SessionEvent::Remote(_))),
        "远端变更要进事件队列"
    );

    // 撤销：只撤本地的 `甲`，远端 `😀` 必须留下
    let undone = session.undo().expect("撤销").expect("有可撤销项");
    let undone = applied(&undone);
    assert_eq!(undone.source, ChangeSource::Undo);
    assert!(
        !undone.local_echo,
        "撤销不是本地输入，调用方要按 splice 更新镜像"
    );
    assert!(!session.markdown().contains('甲'), "undo 撤掉本地字符");
    assert!(session.markdown().contains('😀'), "undo 不得回退远端字符");
    assert_eq!(
        undone.splice.delete_scalars, 1,
        "撤销删掉本地那 1 个 scalar"
    );
    assert_eq!(undone.splice.insert, "", "撤销没有插入新字符");

    // redo 只恢复本地字符，远端不重复
    let redone = session.redo().expect("重做").expect("有可重做项");
    let redone = applied(&redone);
    assert_eq!(redone.source, ChangeSource::Redo);
    assert!(session.markdown().contains('甲'));
    assert_eq!(
        session.markdown().matches('😀').count(),
        1,
        "远端字符不重复"
    );
}

#[test]
fn journal_keeps_remote_and_local_batches_durable_together() {
    let (_dir, vault, mut session) = open_session(64);
    session.apply(request(&session, "c1", 0, vec![edit(0, 0, "甲")]));

    let remote = BodyDoc::create("doc-1", PEER_B).expect("远端副本");
    remote.insert_scalars(0, "乙").expect("远端编辑");
    let update = remote.export_all_updates().expect("导出");
    session
        .apply_remote(&update)
        .expect("导入")
        .expect("有变更");

    let ticket = session.flush(FlushReason::Manual);
    let ack = session
        .wait_for_durable(ticket.target_live_rev, Duration::from_secs(5))
        .expect("应收到 DurableAck");
    assert_eq!(ack.through_live_rev, LiveRev(2));
    // 一次 A 事务写了 2 条 update，每条占一个 seq
    assert_eq!(ack.head_seq, 2);
    assert_eq!(load_head(&vault, "doc-1").expect("head").head_seq, 2);

    // 两个 peer 的字符都进了 A 事务：重开会话还在
    drop(session);
    let session = DocumentSession::open(&vault, "doc-1", options(PEER_A)).expect("重开会话");
    assert!(session.markdown().contains('甲'));
    assert!(session.markdown().contains('乙'));
    assert_eq!(session.snapshot().durable_head_seq, 2);
}

#[test]
fn remote_import_is_rejected_under_backpressure() {
    let (_dir, _vault, mut session) = open_session(1);
    session.apply(request(&session, "c1", 0, vec![edit(0, 0, "甲")]));
    assert_eq!(session.pending_len(), 1, "容量已满");

    let remote = BodyDoc::create("doc-1", PEER_B).expect("远端副本");
    remote.insert_scalars(0, "乙").expect("远端编辑");
    let update = remote.export_all_updates().expect("导出");
    let error = session
        .apply_remote(&update)
        .expect_err("背压应拒绝远端导入");
    assert!(matches!(error, PortError::Storage(_)));
    assert_eq!(session.markdown(), "甲", "被拒的远端导入不得修改 Loro");
}

#[test]
fn opening_unknown_document_is_an_error() {
    let (_dir, vault) = fresh_vault();
    let error = DocumentSession::open(&vault, "missing", options(PEER_A)).expect_err("应失败");
    assert!(matches!(error, PortError::NotOpen(_)));
}

#[test]
fn empty_edit_request_is_a_noop_applied() {
    let (_dir, _vault, mut session) = open_session(64);
    let outcome = session.apply(request(&session, "empty", 0, Vec::new()));
    let outcome = applied(&outcome);
    assert_eq!(outcome.live_rev, LiveRev(0), "空编辑不推进 live_rev");
    assert_eq!(session.pending_len(), 0, "空编辑不进 journal");
    assert_eq!(session.markdown(), "");
}

#[test]
fn materialization_after_a_before_b_uses_session_text() {
    // 会话写 A，B 还没跑；用会话最终文本做物化后 notes.body_md 必须一致
    let (_dir, vault, mut session) = open_session(64);
    session.apply(request(&session, "c1", 0, vec![edit(0, 0, NFD)]));
    let ticket = session.flush(FlushReason::Manual);
    session
        .wait_for_durable(ticket.target_live_rev, Duration::from_secs(5))
        .expect("DurableAck");

    assert!(load_body_state(&vault, "note-1")
        .expect("状态")
        .body_md
        .is_none());
    materialize_body(&vault, "note-1", "doc-1", 1, PEER_A).expect("B 物化");
    let state = load_body_state(&vault, "note-1").expect("状态");
    assert_eq!(state.body_md.as_deref(), Some(NFD));
    assert_eq!(state.body_seq, 1);
    assert_eq!(session.markdown(), NFD);
}

#[test]
fn direct_a_transaction_batch_is_visible_to_session_open() {
    // 会话核之外写入的 A 批次（例如同步切片）也必须能被新会话重放出来
    let (_dir, vault, session) = open_session(64);
    let doc = BodyDoc::restore("doc-1", PEER_A, None, &[]).expect("独立副本");
    doc.insert_scalars(0, "外部写入").expect("编辑");
    let update = doc
        .pending_update(&loro::VersionVector::default())
        .expect("导出");
    append_updates(&vault, "doc-1", 0, &[UpdateRecord::new("external", update)]).expect("A 事务");
    drop(session);

    let session = DocumentSession::open(&vault, "doc-1", options(PEER_A)).expect("打开");
    assert_eq!(session.markdown(), "外部写入");
    assert_eq!(session.snapshot().durable_head_seq, 1);
}

#[test]
fn late_requests_from_old_epoch_are_rejected_after_reopen() {
    let (dir, vault, mut session) = open_session(64);
    session.apply(request(&session, "c1", 0, vec![edit(0, 0, "甲")]));
    let ticket = session.flush(FlushReason::Close);
    session
        .wait_for_durable(ticket.target_live_rev, Duration::from_secs(5))
        .expect("durable");
    drop(session);
    drop(vault);

    // 重开：新会话有新的 epoch 与新的 live_rev 基准
    let vault = open_vault(&vault_path(&dir));
    let mut reopened = DocumentSession::open(&vault, "doc-1", options(PEER_A)).expect("重开");
    assert_eq!(reopened.markdown(), "甲");
    assert_eq!(reopened.live_rev(), LiveRev(0), "live_rev 是会话内编号");

    // 旧会话的迟到请求（旧 epoch）必须被拒
    let mut late = request(&reopened, "late", 0, vec![edit(1, 1, "乙")]);
    late.session_epoch = SessionEpoch(2);
    assert!(matches!(
        reopened.apply(late),
        EditOutcome::Rejected(ref rejected) if rejected.reason == RejectReason::WrongEpoch
    ));
    assert_eq!(reopened.markdown(), "甲", "被拒的迟到请求不得改动正文");
}
