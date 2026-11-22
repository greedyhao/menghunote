//! ADR 0009 切片 3a：**外部提交闸门** + **已 durable 增量 adopt**（存储层）。
//!
//! 覆盖：
//!
//! 1. 本地编辑 → flush → Held → 外部 `append_updates`（模拟 engine integrate）→ prepare →
//!    adopt：文本看到远端、`sync_outbox` 0 新行、Held 期间本地写入被拒；
//! 2. `end` 之后本地继续编辑 → flush 成功（无 `HeadConflict`），新 outbox 行数 = 本地 update 数；
//! 3. 有 pending 编辑时 poll 先 `Flushing`；事件队列空不能冒充 quiescence；
//! 4. composition 中 begin → `WaitingIME`（不强行结束 IME），结束后再到 Held；
//! 5. 过期租约 / base 不匹配 / 非连续 / target vv 不一致 / payload 被篡改 → 错误且状态不变；
//! 6. 同 head 与重复交付同一份 delta 都幂等（不重复推进 `live_rev`）；
//! 7. adopt 后 Undo 仍只撤**本地**操作；splice 能从 adopt 前文本精确得到 adopt 后文本；
//! 8. `prepare_committed_delta` 在无新行时返回空 delta；
//! 9. 失败/取消路径（`end` 未 adopt、`cancel`、未 adopt 行、flush 冲突）都先交付已 durable
//!    的变化，且闸门不静默释放。
//!
//! 纪律（AGENTS.md §6）：等待只针对**明确状态**（Held / Failed / DurableAck），时间只是
//! 安全上限；临时目录由 `support` 放在 `CARGO_TARGET_TMPDIR` 下。

mod support;

use std::time::{Duration, Instant};

use loro::VersionVector;

use menghunote_storage::session::{
    prepare_committed_delta, AdoptError, AppliedHeads, ChangeSource, DeltaSplice, DocumentSession,
    EditOutcome, EditRequest, ExternalCommitLease, ExternalCommitTicket, FlushReason, GateError,
    GateState, LiveRev, RejectReason, ScalarEdit, SessionEpoch, SessionEvent, SessionOptions,
};
use menghunote_storage::{
    append_updates, load_body_doc, load_head, SyncContext, UpdateRecord, Vault,
};
use support::{count, fresh_vault, seed_note, PEER_B};

/// 模拟 engine 的外部提交 peer（与 PEER_A/PEER_B 区分开）。
const PEER_C: u64 = 3;

/// 测试用同步上下文：启用它才能证明 adopt / 交付**不产生**出站意图。
const CTX: SyncContext = SyncContext {
    peer_id: 0xA1CE_0003,
    key_epoch: 1,
};

fn edits(start: usize, end: usize, replacement: &str) -> ScalarEdit {
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

fn options(sync: Option<SyncContext>, flush_delay: Option<Duration>) -> SessionOptions {
    SessionOptions {
        peer_id: CTX.peer_id,
        session_epoch: SessionEpoch(21),
        flush_delay,
        sync,
        ..SessionOptions::default()
    }
}

fn open(vault: &Vault, options: SessionOptions) -> DocumentSession {
    DocumentSession::open(vault, "doc-1", options).expect("打开会话")
}

fn outbox_rows(vault: &Vault) -> i64 {
    count(vault, "SELECT count(*) FROM sync_outbox", &[])
}

fn update_rows(vault: &Vault) -> i64 {
    count(
        vault,
        "SELECT count(*) FROM loro_updates WHERE doc_id='doc-1'",
        &[],
    )
}

/// 模拟 `engine.integrate`：从库中当前 head 重放一份独立副本 → 改 → 一个 A 事务追加。
fn external_append(vault: &Vault, at: usize, text: &str) -> u64 {
    let current = load_head(vault, "doc-1").expect("库中 head");
    let base = current.version_vector().expect("解码 head_vv");
    let (doc, _) = load_body_doc(vault, "doc-1", PEER_C, None).expect("重放外部副本");
    doc.insert_scalars(at, text).expect("外部编辑");
    let update = doc.pending_update(&base).expect("外部增量");
    append_updates(
        vault,
        "doc-1",
        current.head_seq,
        &[UpdateRecord::new("engine-integrate", update)],
    )
    .expect("外部 A 事务")
    .head_seq
}

/// 造一条不落库的远端 update（只用于断言 Held 期间 `apply_remote` 被拒）。
fn remote_bytes(peer: u64, text: &str) -> Vec<u8> {
    let doc = menghunote_storage::BodyDoc::create("doc-1", peer).expect("远端副本");
    doc.insert_scalars(0, text).expect("远端编辑");
    doc.export_all_updates().expect("导出远端 update")
}

/// 只等**明确状态**（Held）；时间只是安全上限，不是收敛判据。
fn drive_to_held(
    session: &mut DocumentSession,
    ticket: &ExternalCommitTicket,
) -> ExternalCommitLease {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match session.poll_external_commit(ticket) {
            GateState::Held(lease) => return lease,
            GateState::Flushing => std::thread::yield_now(),
            GateState::WaitingIME => panic!("没有 composition 时不应 WaitingIME"),
            other => panic!("闸门未收敛到 Held：{other:?}"),
        }
        assert!(Instant::now() < deadline, "闸门 10s 内没有进入 Held");
    }
}

fn applied_heads(vault: &Vault) -> AppliedHeads {
    AppliedHeads::load(vault, "doc-1").expect("读库中 heads")
}

/// 用 `LiveDelta.splice` 更新一份文本镜像：越界即 panic（即"光标/选区不越界"）。
fn apply_splice(text: &str, splice: &DeltaSplice) -> String {
    let chars: Vec<char> = text.chars().collect();
    let at = splice.at.get();
    let delete = usize::try_from(splice.delete_scalars).expect("delete_scalars 应能放进 usize");
    assert!(
        at + delete <= chars.len(),
        "splice 越界：at={at} delete={delete} len={}",
        chars.len()
    );
    let mut out: String = chars[..at].iter().collect();
    out.push_str(&splice.insert);
    out.extend(&chars[at + delete..]);
    out
}

/// 把本地编辑 flush 到 durable（测试里最常用的起点）。
fn flush_durable(session: &mut DocumentSession, target: LiveRev) {
    session.flush(FlushReason::Manual);
    session
        .wait_for_durable(target, Duration::from_secs(5))
        .expect("本地 flush 必须收敛到 DurableAck");
    assert_eq!(session.pending_len(), 0);
}

// ---------------------------------------------------------------------------
// 1) 本地编辑 → Held → 外部 append → prepare → adopt；Held 期间写入被拒
//    2) end 之后本地继续编辑 → flush 成功且 outbox 行数只数本地 update
// ---------------------------------------------------------------------------

#[test]
fn external_append_adopted_without_outbox_rows_and_local_flush_survives_end() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let mut session = open(&vault, options(Some(CTX), None));

    assert!(matches!(
        session.apply(request(&session, "c1", 0, vec![edits(0, 0, "本地")])),
        EditOutcome::Applied(_)
    ));
    flush_durable(&mut session, LiveRev(1));
    assert_eq!(outbox_rows(&vault), 1, "本地 update 必须产生 1 条意图");
    assert_eq!(update_rows(&vault), 1);

    // 闸门到 Held：此刻没有 pending，且目标已 durable
    let ticket = session.begin_external_commit().expect("begin");
    assert_eq!(ticket.target_live_rev(), LiveRev(1));
    assert_eq!(ticket.baseline_head_seq(), 1);
    let lease = drive_to_held(&mut session, &ticket);
    assert_eq!(lease.durable_head_seq(), 1);
    assert_eq!(session.external_commit_lease(), Some(&lease));
    assert!(session.is_external_commit_held());

    // Held 期间本地写入 / 组合 / 远端导入一律 fail-closed
    match session.apply(request(&session, "c2", 1, vec![edits(2, 2, "禁")])) {
        EditOutcome::Rejected(rejected) => {
            assert_eq!(rejected.reason, RejectReason::NotReady);
            assert_eq!(rejected.command_id, "c2");
        }
        other => panic!("Held 期间必须拒绝 apply，实际 {other:?}"),
    }
    assert!(
        session.try_begin_composition().is_err(),
        "Held 期间必须拒绝开始 IME 组合"
    );
    assert_eq!(session.composition_depth(), 0);
    assert_eq!(
        session.begin_composition(),
        0,
        "兼容入口也不得进入组合（fail-closed）"
    );
    assert!(
        session.apply_remote(&remote_bytes(PEER_B, "远")).is_err(),
        "Held 期间必须拒绝远端导入"
    );
    match session.undo().expect("undo 不 panic") {
        Some(EditOutcome::Rejected(rejected)) => {
            assert_eq!(rejected.reason, RejectReason::NotReady)
        }
        other => panic!("Held 期间 undo 必须被拒，实际 {other:?}"),
    }
    assert_eq!(session.markdown(), "本地", "被拒的写入不得改动文本");
    assert_eq!(session.live_rev(), LiveRev(1));
    assert_eq!(session.pending_len(), 0);

    // 外部提交（模拟 engine integrate）：head 1 → 2
    assert_eq!(external_append(&vault, 2, "远端"), 2);

    let prepared = prepare_committed_delta(&vault, "doc-1", session.durable_head_seq())
        .expect("prepare 外部增量");
    assert_eq!(prepared.base_seq, 1);
    assert_eq!(prepared.target_head_seq, 2);
    assert_eq!(prepared.updates.len(), 1);

    let outcome = session
        .adopt_committed_delta(&lease, prepared)
        .expect("adopt");
    assert_eq!(outcome.delta.source, ChangeSource::Remote);
    assert_eq!(outcome.delta.cause, None);
    assert_eq!(outcome.delta.from_rev, LiveRev(1));
    assert_eq!(outcome.delta.to_rev, LiveRev(2));
    assert_eq!(outcome.ack.head_seq, 2);
    assert_eq!(outcome.ack.through_live_rev, LiveRev(2));
    assert_eq!(session.markdown(), "本地远端", "当前文本必须包含远端修改");
    assert_eq!(session.live_rev(), LiveRev(2));
    assert_eq!(
        session.durable_head_seq(),
        2,
        "adopt 必须把 flusher 的 CAS 基准重定到外部 head"
    );
    assert_eq!(outbox_rows(&vault), 1, "adopt 不得产生新的出站意图");
    assert_eq!(
        update_rows(&vault),
        2,
        "adopt 只导入已 durable 的行，不再写库"
    );

    // end → 释放
    session
        .end_external_commit(lease, applied_heads(&vault))
        .expect("end");
    assert!(!session.is_external_commit_held());

    // 继续本地编辑 → flush 必须成功（rebase 生效，无 HeadConflict）
    assert!(matches!(
        session.apply(request(&session, "c3", 2, vec![edits(4, 4, "续")])),
        EditOutcome::Applied(_)
    ));
    session.flush(FlushReason::Manual);
    let ack = session
        .wait_for_durable(LiveRev(3), Duration::from_secs(5))
        .expect("end 之后的本地 flush 必须成功");
    assert_eq!(ack.head_seq, 3);
    assert_eq!(session.markdown(), "本地远端续");
    assert_eq!(update_rows(&vault), 3, "新 update 行数 = 本地 update 数");
    assert_eq!(
        outbox_rows(&vault),
        2,
        "新 outbox 行数与本地 update 一致（2 条本地 + adopt 0 条）"
    );
}

// ---------------------------------------------------------------------------
// 3) 闸门等待：有 pending 时先 Flushing；空事件队列不得冒充 quiescence
// ---------------------------------------------------------------------------

#[test]
fn gate_waits_for_explicit_durable_target_not_empty_event_queue() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    // 注入慢盘：第一次 poll 之后 flush 还没写完，状态必须是明确的 Flushing
    let mut session = open(&vault, options(None, Some(Duration::from_millis(250))));
    session.apply(request(&session, "c1", 0, vec![edits(0, 0, "甲")]));

    let ticket = session.begin_external_commit().expect("begin");
    assert_eq!(session.poll_external_commit(&ticket), GateState::Flushing);
    assert!(
        session.events().is_empty(),
        "此刻事件队列为空，但不能当成 quiescence"
    );
    assert_eq!(
        session.poll_external_commit(&ticket),
        GateState::Flushing,
        "队列空不得让闸门跳过明确目标"
    );
    assert_eq!(session.pending_len(), 1, "flush 已投递但还没 durable");
    assert!(!session.is_external_commit_held());

    let lease = drive_to_held(&mut session, &ticket);
    assert_eq!(lease.target_live_rev(), LiveRev(1));
    assert_eq!(lease.durable_head_seq(), 1);
    assert_eq!(session.pending_len(), 0);
    assert_eq!(session.durable_through(), LiveRev(1));
}

// ---------------------------------------------------------------------------
// 4) composition 中 begin → WaitingIME；自然结束后再到 Held
// ---------------------------------------------------------------------------

#[test]
fn gate_waits_for_ime_composition_instead_of_ending_it() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let mut session = open(&vault, options(None, None));
    session.apply(request(&session, "c1", 0, vec![edits(0, 0, "甲")]));
    flush_durable(&mut session, LiveRev(1));

    assert_eq!(session.begin_composition(), 1);
    let ticket = session.begin_external_commit().expect("begin");
    assert_eq!(session.poll_external_commit(&ticket), GateState::WaitingIME);
    assert!(session.is_composing(), "闸门不得强行结束 IME");
    assert_eq!(session.composition_depth(), 1);

    assert!(session.end_composition().is_empty());
    let lease = drive_to_held(&mut session, &ticket);
    assert_eq!(lease.target_live_rev(), LiveRev(1));
    assert!(
        session.try_begin_composition().is_err(),
        "Held 期间不得开始新的组合"
    );
    session
        .end_external_commit(lease, applied_heads(&vault))
        .expect("end");
}

// ---------------------------------------------------------------------------
// 5) 过期租约 / base 不匹配 / 非连续 / vv 不一致 / payload 篡改：错误且状态不变
// ---------------------------------------------------------------------------

#[test]
fn stale_lease_base_and_bad_deltas_leave_state_unchanged() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let mut session = open(&vault, options(Some(CTX), None));

    session.apply(request(&session, "c1", 0, vec![edits(0, 0, "本地")]));
    flush_durable(&mut session, LiveRev(1));

    // 第一代租约：正常结束，之后作废
    let first = session.begin_external_commit().expect("begin");
    let stale_lease = drive_to_held(&mut session, &first);
    session
        .end_external_commit(stale_lease.clone(), applied_heads(&vault))
        .expect("end");

    // 第二代租约：Held 期间外部提交一条，然后逐项构造非法 delta
    let second = session.begin_external_commit().expect("begin");
    let lease = drive_to_held(&mut session, &second);
    assert_eq!(external_append(&vault, 2, "远端"), 2);

    let text_before = session.markdown();
    let rev_before = session.live_rev();
    let head_before = session.durable_head_seq();
    assert_eq!(head_before, 1);
    let good = prepare_committed_delta(&vault, "doc-1", head_before).expect("prepare");

    // a) 过期租约
    match session.adopt_committed_delta(&stale_lease, good.clone()) {
        Err(AdoptError::LeaseMismatch { .. }) => {}
        other => panic!("过期租约必须被拒，实际 {other:?}"),
    }
    // b) target vv 与导入结果不一致
    let mut bad_vv = good.clone();
    bad_vv.target_vv = VersionVector::default().encode();
    match session.adopt_committed_delta(&lease, bad_vv) {
        Err(AdoptError::TargetMismatch { field }) => assert_eq!(field, "vv"),
        other => panic!("vv 不一致必须被拒，实际 {other:?}"),
    }
    // c) 非连续（target 没有被 updates 覆盖）
    let mut non_contiguous = good.clone();
    non_contiguous.target_head_seq = head_before + 2;
    match session.adopt_committed_delta(&lease, non_contiguous) {
        Err(AdoptError::NonContiguous { .. }) => {}
        other => panic!("非连续必须被拒，实际 {other:?}"),
    }
    // d) payload 不是库里那一份（不是"已 durable 的那份"）
    let mut tampered = good.clone();
    tampered.updates[0] = vec![0u8; 8];
    match session.adopt_committed_delta(&lease, tampered) {
        Err(AdoptError::PayloadMismatch { seq }) => assert_eq!(seq, 2),
        other => panic!("payload 篡改必须被拒，实际 {other:?}"),
    }
    // e) base 过期（从 0 出发的旧 delta）
    let stale_base = prepare_committed_delta(&vault, "doc-1", 0).expect("prepare 0");
    match session.adopt_committed_delta(&lease, stale_base) {
        Err(AdoptError::StaleBase {
            base_seq,
            durable_head_seq,
        }) => {
            assert_eq!(base_seq, 0);
            assert_eq!(durable_head_seq, 1);
        }
        other => panic!("过期 base 必须被拒，实际 {other:?}"),
    }

    // 任何失败都不得改动 live / durable 水位
    assert_eq!(session.markdown(), text_before);
    assert_eq!(session.live_rev(), rev_before);
    assert_eq!(session.durable_head_seq(), head_before);
    assert_eq!(update_rows(&vault), 2, "库中只有外部那一条新行");

    // 合法 delta 仍然可以成功；非法的 end 不得释放闸门
    let adopted = session
        .adopt_committed_delta(&lease, good.clone())
        .expect("合法 adopt 必须成功");
    assert_eq!(adopted.delta.to_rev, LiveRev(2));
    assert_eq!(session.markdown(), "本地远端");
    assert!(session.is_external_commit_held());

    match session.end_external_commit(stale_lease, applied_heads(&vault)) {
        Err(GateError::LeaseMismatch { .. }) => {}
        other => panic!("过期租约不得释放闸门，实际 {other:?}"),
    }
    assert!(session.is_external_commit_held(), "失败不得静默释放");
    match session.end_external_commit(
        lease.clone(),
        AppliedHeads {
            head_seq: 99,
            frontier: Vec::new(),
            vv: Vec::new(),
        },
    ) {
        Err(GateError::HeadsMismatch { .. }) => {}
        other => panic!("applied 与库中不一致必须被拒，实际 {other:?}"),
    }
    assert!(session.is_external_commit_held(), "失败不得静默释放");
    session
        .end_external_commit(lease, applied_heads(&vault))
        .expect("end");
    assert!(!session.is_external_commit_held());
}

// ---------------------------------------------------------------------------
// 6)+8) 同 head 的空 delta：prepare 返回空，adopt 幂等不推进 rev；重复交付同一份也幂等
// ---------------------------------------------------------------------------

#[test]
fn same_head_delta_and_repeated_adopt_are_idempotent() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let mut session = open(&vault, options(Some(CTX), None));
    session.apply(request(&session, "c1", 0, vec![edits(0, 0, "本地")]));
    flush_durable(&mut session, LiveRev(1));

    let ticket = session.begin_external_commit().expect("begin");
    let lease = drive_to_held(&mut session, &ticket);

    // 8) 没有新行 → 空 delta
    let empty = prepare_committed_delta(&vault, "doc-1", session.durable_head_seq())
        .expect("prepare 空 delta");
    assert_eq!(empty.base_seq, 1);
    assert_eq!(empty.target_head_seq, 1);
    assert!(empty.updates.is_empty(), "同 head 必须返回空 updates");

    let idempotent = session
        .adopt_committed_delta(&lease, empty.clone())
        .expect("空 delta 幂等成功");
    assert_eq!(idempotent.delta.from_rev, LiveRev(1));
    assert_eq!(
        idempotent.delta.to_rev,
        LiveRev(1),
        "同 head 不得推进 live_rev"
    );
    assert!(idempotent.delta.splice.insert.is_empty());
    assert_eq!(idempotent.delta.splice.delete_scalars, 0);
    assert_eq!(session.markdown(), "本地");
    assert_eq!(session.live_rev(), LiveRev(1));

    // 6) 有非空 delta 时：重复交付同一份不得重复推进
    assert_eq!(external_append(&vault, 2, "远端"), 2);
    let prepared = prepare_committed_delta(&vault, "doc-1", session.durable_head_seq())
        .expect("prepare 非空 delta");
    let first = session
        .adopt_committed_delta(&lease, prepared.clone())
        .expect("首次 adopt");
    assert_eq!(first.delta.to_rev, LiveRev(2));
    assert_eq!(session.markdown(), "本地远端");

    let again = session
        .adopt_committed_delta(&lease, prepared)
        .expect("重复 adopt 必须幂等");
    assert_eq!(again.delta.from_rev, LiveRev(2));
    assert_eq!(
        again.delta.to_rev,
        LiveRev(2),
        "重复 adopt 不得推进 live_rev"
    );
    assert!(again.delta.splice.insert.is_empty(), "不得重复插入文本");
    assert_eq!(again.ack.head_seq, 2);
    assert_eq!(session.markdown(), "本地远端");
    assert_eq!(session.live_rev(), LiveRev(2));
    assert_eq!(outbox_rows(&vault), 1);
    assert_eq!(update_rows(&vault), 2);

    session
        .end_external_commit(lease, applied_heads(&vault))
        .expect("end");
}

// ---------------------------------------------------------------------------
// 7) adopt 保留本地 Undo（远端不进本地 Undo）与 splice/选区边界
// ---------------------------------------------------------------------------

#[test]
fn adopt_preserves_local_undo_and_splice_bounds() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let mut session = open(&vault, options(None, None));

    // 一步本地编辑 = 一个撤销项
    session.apply(request(&session, "c1", 0, vec![edits(0, 0, "本地")]));
    flush_durable(&mut session, LiveRev(1));

    let ticket = session.begin_external_commit().expect("begin");
    let lease = drive_to_held(&mut session, &ticket);
    assert_eq!(external_append(&vault, 2, "远端"), 2);
    let before_text = session.markdown();
    let prepared =
        prepare_committed_delta(&vault, "doc-1", session.durable_head_seq()).expect("prepare");
    let outcome = session
        .adopt_committed_delta(&lease, prepared)
        .expect("adopt");

    // splice 必须能从 adopt 前文本精确得到 adopt 后文本（镜像/选区按它变换才不越界）
    let mirrored = apply_splice(&before_text, &outcome.delta.splice);
    assert_eq!(mirrored, session.markdown());
    assert_eq!(mirrored, "本地远端");
    assert!(
        outcome.delta.splice.at.get()
            + usize::try_from(outcome.delta.splice.delete_scalars).expect("delete 长度")
            <= before_text.chars().count(),
        "splice 必须落在 adopt 前文本范围内"
    );

    session
        .end_external_commit(lease, applied_heads(&vault))
        .expect("end");

    // 远端导入不进本地 Undo：undo 只撤本地那一步
    match session.undo().expect("undo 不报错") {
        Some(EditOutcome::Applied(applied)) => {
            assert_eq!(applied.source, ChangeSource::Undo);
        }
        other => panic!("adopt 之后本地操作仍必须可撤销，实际 {other:?}"),
    }
    assert_eq!(
        session.markdown(),
        "远端",
        "undo 只能撤本地字符，远端必须留下"
    );
    assert!(
        session.undo().expect("undo 不报错").is_none(),
        "远端不进本地 Undo，此时已没有可撤销项"
    );
}

// ---------------------------------------------------------------------------
// 9a) 失败/取消路径：end 未 adopt，也必须先交付已 durable 的变化
// ---------------------------------------------------------------------------

#[test]
fn end_without_adopt_delivers_durable_changes_and_local_flush_still_works() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let mut session = open(&vault, options(Some(CTX), None));
    session.apply(request(&session, "c1", 0, vec![edits(0, 0, "本地")]));
    flush_durable(&mut session, LiveRev(1));

    let ticket = session.begin_external_commit().expect("begin");
    let lease = drive_to_held(&mut session, &ticket);
    assert_eq!(external_append(&vault, 2, "远端"), 2);
    assert_eq!(session.markdown(), "本地", "adopt 之前 live 还没看到外部行");

    session
        .end_external_commit(lease, applied_heads(&vault))
        .expect("end 必须交付已 durable 的变化");
    assert_eq!(
        session.markdown(),
        "本地远端",
        "end 必须把 Held 期间 durable 的行交付进 live"
    );
    assert_eq!(session.live_rev(), LiveRev(2));
    assert_eq!(session.durable_head_seq(), 2);
    assert_eq!(outbox_rows(&vault), 1, "交付不得产生出站意图");
    let remote_events = session
        .events()
        .into_iter()
        .filter(|event| matches!(event, SessionEvent::Remote(_)))
        .count();
    assert_eq!(
        remote_events, 1,
        "交付的增量必须进事件队列（3b 靠它刷新镜像）"
    );

    session.apply(request(&session, "c2", 2, vec![edits(4, 4, "续")]));
    session.flush(FlushReason::Manual);
    let ack = session
        .wait_for_durable(LiveRev(3), Duration::from_secs(5))
        .expect("end 之后本地 flush 必须成功");
    assert_eq!(ack.head_seq, 3);
    assert_eq!(session.markdown(), "本地远端续");
}

// ---------------------------------------------------------------------------
// 9b) 未 adopt 的外部行：闸门明确失败，cancel 先交付再释放
// ---------------------------------------------------------------------------

#[test]
fn unadopted_rows_fail_the_gate_and_cancel_delivers_them() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let mut session = open(&vault, options(Some(CTX), None));

    // 闸门之外的外部写入（模拟上一次崩溃留下的未 adopt 行）
    assert_eq!(external_append(&vault, 0, "外"), 1);
    let ticket = session.begin_external_commit().expect("begin");
    match session.poll_external_commit(&ticket) {
        GateState::Failed(GateError::Unadopted {
            durable_head_seq,
            database_head_seq,
        }) => {
            assert_eq!(durable_head_seq, 0);
            assert_eq!(database_head_seq, 1);
        }
        other => panic!("必须明确报告未 adopt 的外部行，实际 {other:?}"),
    }
    assert!(!session.is_external_commit_held());
    assert_eq!(session.markdown(), "", "闸门本身不得私自导入");

    let delivered = session
        .cancel_external_commit(&ticket)
        .expect("cancel 必须交付")
        .expect("有可交付行");
    assert_eq!(delivered.source, ChangeSource::Remote);
    assert_eq!(session.markdown(), "外");
    assert_eq!(session.live_rev(), LiveRev(1));
    assert!(!session.is_external_commit_held());
    assert_eq!(outbox_rows(&vault), 0, "交付不得产生出站意图");

    // 释放后本地编辑 + flush 正常（基准已重定）
    session.apply(request(&session, "c1", 1, vec![edits(1, 1, "本")]));
    session.flush(FlushReason::Manual);
    let ack = session
        .wait_for_durable(LiveRev(2), Duration::from_secs(5))
        .expect("取消重定基准后 flush 必须成功");
    assert_eq!(ack.head_seq, 2);
    assert_eq!(session.markdown(), "外本");
}

// ---------------------------------------------------------------------------
// 9c) flush 冲突：明确报告、不静默释放、有未 ACK 时 cancel fail-closed
// ---------------------------------------------------------------------------

#[test]
fn failed_flush_is_reported_and_never_silently_released() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    // 注入慢盘：让外部行在 flush 真正写之前插入，制造 head CAS 冲突
    let mut session = open(&vault, options(None, Some(Duration::from_millis(500))));
    session.apply(request(&session, "c1", 0, vec![edits(0, 0, "本")]));

    let ticket = session.begin_external_commit().expect("begin");
    assert_eq!(session.poll_external_commit(&ticket), GateState::Flushing);
    // 冲突源：闸门之外推进 head（正常流程不会发生，这里验证 fail-closed）
    assert_eq!(external_append(&vault, 0, "外"), 1);

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match session.poll_external_commit(&ticket) {
            GateState::Failed(GateError::FlushFailed { .. }) => break,
            GateState::Flushing => std::thread::yield_now(),
            other => panic!("预期 FlushFailed，实际 {other:?}"),
        }
        assert!(Instant::now() < deadline, "flush 失败必须被明确报告");
    }

    // 不得静默释放：票据仍占用闸门；有未 ACK 更新时 cancel 必须 fail-closed
    assert!(!session.is_external_commit_held());
    assert!(matches!(
        session.begin_external_commit(),
        Err(GateError::AlreadyHeld { .. })
    ));
    assert!(matches!(
        session.cancel_external_commit(&ticket),
        Err(GateError::Pending { .. })
    ));
    assert_eq!(session.markdown(), "本", "本地编辑不得被回滚");
    assert_eq!(update_rows(&vault), 1, "只有冲突的外部行落库");
}
