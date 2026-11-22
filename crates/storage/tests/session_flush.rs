//! 后台 flush 与 durable 契约：异步票据、ACK 顺序、崩溃后重放。

mod support;

use std::time::{Duration, Instant};

use menghunote_storage::session::{
    DocumentSession, DriverOptions, EditOutcome, EditRequest, ExternalCommitTicket, FlushReason,
    GateError, GateState, LiveRev, ManualClock, ScalarEdit, SessionDriver, SessionEpoch,
    SessionEvent, SessionOptions,
};
use menghunote_storage::{
    append_updates, intent_message_id, intent_object_id, load_body_doc, load_body_state,
    materialize_body, outbox_row, BodyDoc, SyncContext, SyncOutboxState, UpdateRecord, Value,
    Vault,
};
use support::*;

fn options(peer: u64) -> SessionOptions {
    SessionOptions {
        peer_id: peer,
        session_epoch: SessionEpoch(3),
        ..SessionOptions::default()
    }
}

/// 测试用同步上下文：peer 故意不用常量 1，epoch = 1（Phase 1）。
///
/// 与 `sync_intent.rs` 的 `CTX` 同口径：本地 intent 只依赖
/// `SessionOptions { peer_id, sync }`，当前代码路径**不需要**额外的
/// epoch/log/relay meta（`local_sync_intent` 是纯函数，`key_epoch` 来自这里）。
const SYNC_CTX: SyncContext = SyncContext {
    peer_id: 0xA1CE_0001,
    key_epoch: 1,
};

/// 打开「会产生本地出站意图」的会话配置。
fn options_with_sync() -> SessionOptions {
    SessionOptions {
        sync: Some(SYNC_CTX),
        ..options(SYNC_CTX.peer_id)
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

/// 把闸门驱到明确的失败态（时间只作为上限，不用 sleep 当收敛判据）。
fn drive_gate_to_failure(
    session: &mut DocumentSession,
    ticket: &ExternalCommitTicket,
) -> GateError {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match session.poll_external_commit(ticket) {
            GateState::Failed(error) => return error,
            GateState::Flushing | GateState::WaitingIME => std::thread::yield_now(),
            other => panic!("闸门没有给出明确失败：{other:?}"),
        }
        assert!(Instant::now() < deadline, "闸门 10s 内没有给出明确失败");
    }
}

/// 读 flusher 失败原因（闸门是 UI/驱动读取 `last_write_error` 的既有入口）。
fn flush_failure_message(session: &mut DocumentSession, ticket: &ExternalCommitTicket) -> String {
    match drive_gate_to_failure(session, ticket) {
        GateError::FlushFailed { message, .. } => message,
        other => panic!("期望 GateError::FlushFailed，实际 {other:?}"),
    }
}

/// 有界等待 flusher 记录失败原因。
///
/// 普通写失败没有 `DurableAck` 这个收敛点：`last_write_error` 由 flusher 在同一条
/// 命令内写完后就不再变化，因此轮询它是有界的显式状态，不是用 sleep 猜收敛。
fn wait_for_write_error(session: &DocumentSession, timeout: Duration) -> String {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(message) = session.last_write_error() {
            return message;
        }
        assert!(
            Instant::now() < deadline,
            "flush 在 {timeout:?} 内没有记录写失败原因"
        );
        std::thread::yield_now();
    }
}

/// `sync_outbox` 行数（null 的 intent 不落行，所以也可以当“有没有产生意图”）。
fn outbox_count(vault: &Vault) -> i64 {
    count(vault, "SELECT count(*) FROM sync_outbox", &[])
}

/// 测试用 SQLite 触发器：安装后由 `Drop` 兜底删除（panic 安全，正常路径显式 `remove`）。
struct SqlTrigger {
    vault: Vault,
    name: &'static str,
}

impl SqlTrigger {
    fn install(vault: &Vault, name: &'static str, sql: &str) -> Self {
        vault.exec_batch(sql).expect("安装故障注入触发器");
        Self {
            vault: vault.clone(),
            name,
        }
    }

    /// 正常路径显式删除；`Drop` 的 `IF EXISTS` 只是 panic 安全网。
    fn remove(&self) {
        self.vault
            .exec_batch(&format!("DROP TRIGGER IF EXISTS {}", self.name))
            .expect("删除故障注入触发器");
    }
}

impl Drop for SqlTrigger {
    fn drop(&mut self) {
        let _ = self
            .vault
            .exec_batch(&format!("DROP TRIGGER IF EXISTS {}", self.name));
    }
}

/// P1-3 生产接线：head 被外部推进后，flusher 必须在同一个后台线程里
/// `load_head` + 重建 sink，并重试**同一前缀**一次；成功后正常裁剪/推进 durable + ACK。
///
/// 外部推进用「同一份远端 update 已经先落库、会话又把它导入了 live」构造：新 head 的
/// ops 全部落在未 ACK 前缀的 `end_vv` 内，因此原样重试不会写坏 `head_vv`/`frontier`
/// （下面的完整重放会验证这一点）。
#[test]
fn flush_recovers_from_head_conflict_by_rebuilding_sink() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let mut session = open_session(&vault, options(PEER_A));

    // 会话先把远端 update 导入 live（journal 里留下同一批 op 的未 ACK 增量）。
    let remote = BodyDoc::create("doc-1", PEER_B).expect("远端副本");
    remote.insert_scalars(0, "远端").expect("远端编辑");
    let remote_bytes = remote.export_all_updates().expect("导出远端 update");
    let remote_update = pending(&remote, &loro::VersionVector::default()).expect("远端增量");
    session
        .apply_remote(&remote_bytes)
        .expect("导入远端 update")
        .expect("必须有变更");

    // 外部 writer（同步引擎 integrate / 另一个 Vault）把同一份 update 先落库：head 0→1。
    let advanced = append_updates(
        &vault,
        "doc-1",
        0,
        &[UpdateRecord::new("external-1", remote_update)],
    )
    .expect("外部推进 head");
    assert_eq!(advanced.head_seq, 1);
    assert_eq!(
        session.durable_head_seq(),
        0,
        "会话的 CAS 基准仍停在打开时的 head 0"
    );

    // 修复前：这里每次都拿旧 head 0 撞 CAS —— 不写库、不裁剪、不 ACK，下一次还是旧 head。
    let ticket = session.flush(FlushReason::Manual);
    let ack = session
        .wait_for_durable(ticket.target_live_rev, Duration::from_secs(5))
        .expect("HeadConflict 后必须自动重建 sink 并发出 DurableAck");
    assert_eq!(ack.through_live_rev, LiveRev(1));
    assert_eq!(
        ack.head_seq, 2,
        "重试写在外部那一条之后（同一前缀、新 CAS 基准）"
    );
    assert_eq!(session.durable_head_seq(), 2);
    assert_eq!(session.durable_through(), LiveRev(1));
    assert_eq!(session.pending_len(), 0, "成功后必须裁剪前缀");
    assert_eq!(session.markdown(), "远端");

    // 正文/head 正确：完整重放必须通过 `loro_docs.head_frontier` 的完整性校验。
    let (replayed, head) = load_body_doc(&vault, "doc-1", PEER_A, None).expect("重放正文");
    assert_eq!(replayed.text(), "远端");
    assert_eq!(head.head_seq, 2);
    let head_vv = head.version_vector().expect("解码 head vv");
    let live_vv = loro::VersionVector::decode(&session.snapshot().live_vv).expect("解码 live vv");
    assert_eq!(
        head_vv, live_vv,
        "durable head 的 ops 必须与本会话 live 一致"
    );
    assert_eq!(
        count(
            &vault,
            "SELECT count(*) FROM loro_updates WHERE doc_id = 'doc-1'",
            &[]
        ),
        2,
        "外部一条 + 自动重建后重试的一条"
    );
}

/// P1-3 反向边界：外部 head 含本会话 live **未知**的并发 ops 时，原样重试会写坏
/// `head_vv`/`frontier`（重放完整性校验直接 Corrupt），因此自动重建必须拒绝并进入
/// 不可恢复态：`last_write_error` 明确提示「需要重开文档」、journal 原样保留、
/// 后续 flush 立即失败且不再写库。
#[test]
fn flush_reports_unrecoverable_after_bounded_rebuild() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let mut session = open_session(&vault, options(PEER_A));

    session.apply(request(&session, "c1", 0, vec![edit(0, 0, "本地")]));

    // 外部 writer 写入本会话完全不知道的并发 ops：head 0→1。
    let external = BodyDoc::create("doc-1", PEER_B).expect("外部副本");
    external.insert_scalars(0, "远端").expect("外部编辑");
    let external_update = pending(&external, &loro::VersionVector::default()).expect("外部增量");
    append_updates(
        &vault,
        "doc-1",
        0,
        &[UpdateRecord::new("external-1", external_update)],
    )
    .expect("外部推进 head");

    // 闸门必须收敛到 Failed 并带出可执行的原因；修复前只会带出裸 HeadConflict，
    // 且同一个失败会永久重复（journal 不裁剪、durable 水位不前进）。
    let ticket = session.begin_external_commit().expect("begin");
    let message = flush_failure_message(&mut session, &ticket);
    assert!(
        message.contains("需要重开文档"),
        "失败原因必须明确提示重开：{message}"
    );
    assert!(
        message.contains("未知的并发 ops"),
        "必须说明为什么不能安全自动重建：{message}"
    );

    assert_eq!(session.pending_len(), 1, "不可恢复时 journal 必须原样保留");
    let rows = || {
        count(
            &vault,
            "SELECT count(*) FROM loro_updates WHERE doc_id = 'doc-1'",
            &[],
        )
    };
    assert_eq!(rows(), 1, "只有外部那一行落库（旧前缀不得被硬写）");
    assert_eq!(head(&vault, "doc-1").head_seq, 1, "库中 head 不得被改写");

    // 后续 flush 立即失败：不发 ACK、不再写库（不能无限重试/无限写）。
    for _ in 0..3 {
        let attempt = session.flush(FlushReason::Manual);
        assert!(
            session
                .wait_for_durable(attempt.target_live_rev, Duration::from_millis(200))
                .is_none(),
            "不可恢复后不得再发出 DurableAck"
        );
        assert_eq!(rows(), 1, "不可恢复后不得再写 loro_updates");
        assert_eq!(head(&vault, "doc-1").head_seq, 1, "不可恢复后不得推进 head");
    }

    // 同一个错误仍可被闸门读到（不是一次性的诊断字符串）。
    match session.poll_external_commit(&ticket) {
        GateState::Failed(GateError::FlushFailed { message: again, .. }) => {
            assert_eq!(again, message, "不可恢复状态必须稳定可读");
        }
        other => panic!("期望持续的 Failed，实际 {other:?}"),
    }
    assert_eq!(
        session.markdown(),
        "本地",
        "live 保持会话内容，靠关闭并重开文档与库对账"
    );

    // P1：不可恢复原因必须能被会话**直接**读到（UI/驱动不必先走外部提交闸门），
    // 且与闸门看到的是同一个字符串；反复读取与再次 flush 都不得改变它。
    let direct = session
        .last_write_error()
        .expect("不可恢复失败必须能从会话读到");
    assert_eq!(direct, message, "会话与闸门必须看到同一个失败原因");
    assert!(
        direct.contains("需要重开文档"),
        "会话读到的原因必须可执行：{direct}"
    );
    for _ in 0..3 {
        session.flush(FlushReason::Manual);
        assert_eq!(
            session.last_write_error().as_deref(),
            Some(message.as_str()),
            "多次读取/再次 flush 不得改变不可恢复原因"
        );
    }
    assert_eq!(session.pending_len(), 1, "读取失败原因不得动 journal");
}

/// P1-3 的另一个不可恢复分支：自动重建要读新 head，读不到/解码失败同样必须 fail-closed
/// （不尝试写、不无限重试），并明确提示需要重开文档。
#[test]
fn flush_reports_unrecoverable_when_new_head_is_unreadable() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let mut session = open_session(&vault, options(PEER_A));
    session.apply(request(&session, "c1", 0, vec![edit(0, 0, "本地")]));

    // 外部推进 head 后把 head_vv 写坏：CAS 冲突后能读到行，但 VV 解码必然失败。
    let external = BodyDoc::create("doc-1", PEER_B).expect("外部副本");
    external.insert_scalars(0, "远端").expect("外部编辑");
    let external_update = pending(&external, &loro::VersionVector::default()).expect("外部增量");
    append_updates(
        &vault,
        "doc-1",
        0,
        &[UpdateRecord::new("external-1", external_update)],
    )
    .expect("外部推进 head");
    vault
        .exec(
            "UPDATE loro_docs SET head_vv = x'01' WHERE doc_id = ?1",
            &[Value::Text("doc-1".to_owned())],
        )
        .expect("写坏 head_vv");

    let ticket = session.begin_external_commit().expect("begin");
    let message = flush_failure_message(&mut session, &ticket);
    assert!(
        message.contains("需要重开文档"),
        "失败原因必须明确提示重开：{message}"
    );
    assert!(
        message.contains("解码失败"),
        "必须说明新 head 读不出来：{message}"
    );
    assert_eq!(session.pending_len(), 1, "不可恢复时 journal 必须原样保留");
    assert_eq!(
        count(
            &vault,
            "SELECT count(*) FROM loro_updates WHERE doc_id = 'doc-1'",
            &[]
        ),
        1,
        "拒绝重建后不得写旧前缀"
    );
}

/// P2 门禁（Phase 1B 前）：HeadConflict 自动重建 sink 时，必须把同一个 `SyncContext`
/// 带进新 sink；本地 intent 的 `seq` 必须按**新** CAS 基准派生（新 head + 1），不能沿用
/// 旧 head 算出的 seq，也不能重复/留下孤儿 intent。
///
/// 场景与 [`flush_recovers_from_head_conflict_by_rebuilding_sink`] 相同：会话先导入远端
/// update，外部 writer 再把同一份 update 落库推进 head。区别是 journal 前缀里还有一条
/// 本地编辑，因此 flush 会同时写本地/远端两条记录并产生一条本地 intent。
#[test]
fn flush_rebuild_keeps_sync_intent_seq_at_new_head_plus_one() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let mut session = open_session(&vault, options_with_sync());

    // 本地编辑先入 journal，远端 update 随后导入：一次 flush 的前缀 = [本地, 远端]。
    session.apply(request(&session, "c1", 0, vec![edit(0, 0, "本地")]));
    let remote = BodyDoc::create("doc-1", PEER_B).expect("远端副本");
    remote.insert_scalars(0, "远端").expect("远端编辑");
    let remote_bytes = remote.export_all_updates().expect("导出远端 update");
    let remote_update = pending(&remote, &loro::VersionVector::default()).expect("远端增量");
    session
        .apply_remote(&remote_bytes)
        .expect("导入远端 update")
        .expect("必须有变更");

    // 外部 writer（同步引擎 integrate 的等价物）先把同一份 update 落库：head 0→1。
    let advanced = append_updates(
        &vault,
        "doc-1",
        0,
        &[UpdateRecord::new("external-1", remote_update)],
    )
    .expect("外部推进 head");
    assert_eq!(advanced.head_seq, 1);
    assert_eq!(
        session.durable_head_seq(),
        0,
        "会话的 CAS 基准仍停在打开时的 head 0"
    );

    let ticket = session.flush(FlushReason::Manual);
    let ack = session
        .wait_for_durable(ticket.target_live_rev, Duration::from_secs(5))
        .expect("HeadConflict 后必须重建 sink 并发出 DurableAck");
    assert_eq!(ack.through_live_rev, LiveRev(2));
    assert_eq!(ack.head_seq, 3, "外部一条 + 重试前缀两条");
    assert_eq!(session.durable_head_seq(), 3);
    assert_eq!(session.durable_through(), LiveRev(2));
    assert_eq!(session.pending_len(), 0, "成功后必须裁剪前缀");
    assert_eq!(
        session.last_write_error(),
        None,
        "成功写入后必须清空失败原因"
    );

    // 恰一条本地 intent：远端条目 / 外部条目都不得产生 intent。
    assert_eq!(
        outbox_count(&vault),
        1,
        "一次本地编辑只应产生一条 intent（远端/外部条目必须没有）"
    );
    let expected_seq = advanced.head_seq + 1;
    let expected_message = intent_message_id("doc-1", expected_seq, SYNC_CTX);
    let row = outbox_row(&vault, &expected_message)
        .expect("读 outbox")
        .expect("重建后仍必须产生本地 intent（说明 SyncContext 被带进新 sink）");
    assert_eq!(row.doc_id, "doc-1");
    assert_eq!(
        row.seq, expected_seq,
        "intent seq 必须按新 CAS 基准派生（新 head {} + 1）",
        advanced.head_seq
    );
    assert_eq!(row.key_epoch, SYNC_CTX.key_epoch);
    assert_eq!(row.state, SyncOutboxState::Pending);
    assert_eq!(
        row.object_id,
        Some(intent_object_id(&expected_message, SYNC_CTX)),
        "object_id 必须按重建后的 message 派生"
    );
    assert!(
        row.batch_id.starts_with("session-"),
        "intent 必须沿用本地 journal 条目的 batch，实际 {:?}",
        row.batch_id
    );

    // 直接查 outbox 列：没有第二张 intent、没有指向外部行的孤儿。
    let raw = vault
        .query("SELECT doc_id, seq, key_epoch, state FROM sync_outbox", &[])
        .expect("查 sync_outbox");
    assert_eq!(raw.len(), 1, "不得重复写 intent：{raw:?}");
    assert_eq!(raw[0][0], Value::Text("doc-1".to_owned()));
    assert_eq!(
        raw[0][1],
        Value::Integer(i64::try_from(expected_seq).expect("seq")),
        "intent 必须指向重试写入的本地 update"
    );
    assert_eq!(raw[0][2], Value::Integer(1));
    assert_eq!(raw[0][3], Value::Text("pending".to_owned()));

    // intent 指向的本地行存在且唯一；总行数 = 外部 1 + 本地 1 + 远端 1。
    assert_eq!(
        count(
            &vault,
            "SELECT count(*) FROM loro_updates WHERE doc_id='doc-1' AND seq=?1",
            &[Value::Integer(i64::try_from(expected_seq).expect("seq"))],
        ),
        1,
        "intent 指向的 update 必须存在且唯一"
    );
    assert_eq!(
        count(
            &vault,
            "SELECT count(*) FROM loro_updates WHERE doc_id = 'doc-1'",
            &[],
        ),
        3,
        "外部 + 本地 + 远端各一条，重试不得重复写"
    );

    // 正文/head 重放一致：重建后的重试没有写坏 head_vv/frontier。
    let live_markdown = session.markdown();
    assert!(
        live_markdown.contains("本地") && live_markdown.contains("远端"),
        "live 正文应包含本地与远端内容：{live_markdown:?}"
    );
    let (replayed, head) = load_body_doc(&vault, "doc-1", PEER_A, None).expect("重放正文");
    assert_eq!(replayed.text(), live_markdown);
    assert_eq!(head.head_seq, 3);
    let head_vv = head.version_vector().expect("解码 head vv");
    let live_vv = loro::VersionVector::decode(&session.snapshot().live_vv).expect("解码 live vv");
    assert_eq!(
        head_vv, live_vv,
        "durable head 的 ops 必须与本会话 live 一致"
    );
}

/// P2 门禁：非 `HeadConflict` 的普通写失败（磁盘/SQLite）必须保持「同一前缀下次重试」：
/// 不 ACK、不裁剪、不推进 durable/head；`last_write_error` 可读且**不得**升级成不可恢复。
/// 移除注入后同一前缀必须能成功落库。
#[test]
fn flush_retryable_write_failure_keeps_prefix_then_recovers() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let mut session = open_session(&vault, options_with_sync());
    session.apply(request(&session, "c1", 0, vec![edit(0, 0, "本地")]));
    assert_eq!(session.pending_len(), 1);

    // 注入非 HeadConflict 的写失败：A 事务在 INSERT 处被 SQLite ABORT。
    let trigger = SqlTrigger::install(
        &vault,
        "injected_flush_disk_failure",
        "CREATE TRIGGER injected_flush_disk_failure BEFORE INSERT ON loro_updates \
         BEGIN SELECT RAISE(ABORT, 'injected disk'); END;",
    );

    let ticket = session.flush(FlushReason::Manual);
    let message = wait_for_write_error(&session, Duration::from_secs(10));
    assert!(!message.is_empty(), "失败原因必须可读");
    assert!(
        !message.contains("需要重开文档"),
        "普通写失败不得升级成不可恢复：{message}"
    );
    assert!(
        message.contains("injected disk"),
        "失败原因必须来自注入的写失败：{message}"
    );
    assert!(
        session
            .wait_for_durable(ticket.target_live_rev, Duration::ZERO)
            .is_none(),
        "普通写失败不得发出 DurableAck"
    );

    // 前缀原样保留；没有 durable/head/intent 被写。
    assert_eq!(session.pending_len(), 1, "失败后必须保留同一前缀");
    assert_eq!(session.durable_head_seq(), 0);
    assert_eq!(session.durable_through(), LiveRev(0));
    assert_eq!(
        count(
            &vault,
            "SELECT count(*) FROM loro_updates WHERE doc_id = 'doc-1'",
            &[],
        ),
        0,
        "失败事务不得写 loro_updates"
    );
    assert_eq!(head(&vault, "doc-1").head_seq, 0, "失败事务不得推进 head");
    assert_eq!(outbox_count(&vault), 0, "失败事务不得留下本地 intent");

    // DROP 触发器后重试：同一前缀成功，水位/正文/intent 都补齐。
    trigger.remove();
    let ticket = session.flush(FlushReason::Manual);
    let ack = session
        .wait_for_durable(ticket.target_live_rev, Duration::from_secs(5))
        .expect("注入移除后同一前缀必须可重试成功");
    assert_eq!(ack.through_live_rev, LiveRev(1));
    assert_eq!(ack.head_seq, 1);
    assert_eq!(session.pending_len(), 0);
    assert_eq!(session.durable_head_seq(), 1);
    assert_eq!(
        session.last_write_error(),
        None,
        "成功写入后必须清空失败原因"
    );
    assert_eq!(session.markdown(), "本地");
    assert_eq!(
        count(
            &vault,
            "SELECT count(*) FROM loro_updates WHERE doc_id = 'doc-1'",
            &[],
        ),
        1
    );
    assert_eq!(head(&vault, "doc-1").head_seq, 1);
    assert_eq!(outbox_count(&vault), 1, "成功后必须补写本地 intent");
    let row = outbox_row(&vault, &intent_message_id("doc-1", 1, SYNC_CTX))
        .expect("读 intent")
        .expect("本地 intent");
    assert_eq!(row.seq, 1);

    let (replayed, head_state) = load_body_doc(&vault, "doc-1", PEER_A, None).expect("重放");
    assert_eq!(replayed.text(), "本地");
    assert_eq!(head_state.head_seq, 1);
}

/// `RebuildOutcome::Retryable` 门禁：head 冲突触发自动重建后，重试**同一前缀**时若遇到
/// 非 HeadConflict 写失败（磁盘/SQLite），必须保持「可重试」——不 ACK、不裁剪、不升级成
/// 不可恢复；移除注入后下一次 flush 仍能自动重建并成功。
///
/// 注入点选 `BEFORE INSERT ON loro_updates`：第一次写会在 CAS（第 0 条语句）就因 head
/// 过期失败，根本到不了 INSERT，因此触发器只命中「重建后的重试」这一次。
#[test]
fn flush_rebuild_retryable_write_failure_keeps_prefix_then_recovers() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let mut session = open_session(&vault, options_with_sync());

    session.apply(request(&session, "c1", 0, vec![edit(0, 0, "本地")]));
    let remote = BodyDoc::create("doc-1", PEER_B).expect("远端副本");
    remote.insert_scalars(0, "远端").expect("远端编辑");
    let remote_bytes = remote.export_all_updates().expect("导出远端 update");
    let remote_update = pending(&remote, &loro::VersionVector::default()).expect("远端增量");
    session
        .apply_remote(&remote_bytes)
        .expect("导入远端 update")
        .expect("必须有变更");

    // 外部先落同一份 update：head 0→1，让本命令第 1 次写入必然拿到 HeadConflict。
    append_updates(
        &vault,
        "doc-1",
        0,
        &[UpdateRecord::new("external-1", remote_update)],
    )
    .expect("外部推进 head");

    let trigger = SqlTrigger::install(
        &vault,
        "injected_rebuild_retry_disk",
        "CREATE TRIGGER injected_rebuild_retry_disk BEFORE INSERT ON loro_updates \
         BEGIN SELECT RAISE(ABORT, 'injected retry disk'); END;",
    );

    let ticket = session.flush(FlushReason::Manual);
    let message = wait_for_write_error(&session, Duration::from_secs(10));
    assert!(
        message.contains("自动重建后重试前缀失败"),
        "必须走 RebuildOutcome::Retryable：{message}"
    );
    assert!(
        !message.contains("需要重开文档"),
        "可重试失败不得升级成不可恢复：{message}"
    );
    assert!(
        session
            .wait_for_durable(ticket.target_live_rev, Duration::ZERO)
            .is_none(),
        "重建后的重试失败不得发出 DurableAck"
    );
    assert_eq!(session.pending_len(), 2, "同一前缀必须原样保留");
    assert_eq!(session.durable_head_seq(), 0);
    assert_eq!(outbox_count(&vault), 0, "回滚的写事务不得留下 intent");
    assert_eq!(
        count(
            &vault,
            "SELECT count(*) FROM loro_updates WHERE doc_id = 'doc-1'",
            &[],
        ),
        1,
        "只有外部那条：重建后的重试整笔回滚"
    );
    assert_eq!(head(&vault, "doc-1").head_seq, 1);

    // 移除注入后下一次 flush：仍从旧 CAS 基准开始，先冲突、再自动重建，这次成功。
    trigger.remove();
    let ticket = session.flush(FlushReason::Manual);
    let ack = session
        .wait_for_durable(ticket.target_live_rev, Duration::from_secs(5))
        .expect("注入移除后必须收敛到 DurableAck");
    assert_eq!(ack.through_live_rev, LiveRev(2));
    assert_eq!(ack.head_seq, 3);
    assert_eq!(session.pending_len(), 0);
    assert_eq!(
        session.last_write_error(),
        None,
        "成功写入后必须清空失败原因"
    );
    assert_eq!(outbox_count(&vault), 1);
    let row = outbox_row(&vault, &intent_message_id("doc-1", 2, SYNC_CTX))
        .expect("读 intent")
        .expect("重建后的本地 intent");
    assert_eq!(row.seq, 2);
    assert_eq!(row.key_epoch, SYNC_CTX.key_epoch);
}

/// 二次冲突门禁：`auto_rebuilt` 用尽后重试仍撞 head 冲突时，必须 fail-closed 进入
/// 不可恢复态（提示重开），journal/库原样保留，后续 flush 立即失败不再碰数据库。
///
/// 注入 `BEFORE UPDATE OF head_seq ON loro_docs ... RAISE(IGNORE)`：第一次写因为 head 已是
/// 1（外部推进）而天然 CAS 失败，不触发该触发器；重建后的重试 CAS 命中 `head_seq=1` 时
/// 被 `RAISE(IGNORE)` 跳过（0 行），稳定复现「同一命令内第二次 HeadConflict」的交错。
/// `head_seq <= CAS 基准` 的回退分支要求第一次写入失败后 head 倒退，公开接口下
/// `head_seq` 只前进、无法确定性构造，因此本测试只覆盖“二次冲突”这一支。
#[test]
fn flush_reports_unrecoverable_on_second_head_conflict() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let mut session = open_session(&vault, options_with_sync());

    session.apply(request(&session, "c1", 0, vec![edit(0, 0, "本地")]));
    let remote = BodyDoc::create("doc-1", PEER_B).expect("远端副本");
    remote.insert_scalars(0, "远端").expect("远端编辑");
    let remote_bytes = remote.export_all_updates().expect("导出远端 update");
    let remote_update = pending(&remote, &loro::VersionVector::default()).expect("远端增量");
    session
        .apply_remote(&remote_bytes)
        .expect("导入远端 update")
        .expect("必须有变更");

    // 外部先落同一份 update：head 0→1（前缀兼容，重建本身是允许的）。
    append_updates(
        &vault,
        "doc-1",
        0,
        &[UpdateRecord::new("external-1", remote_update)],
    )
    .expect("外部推进 head");

    // 只吞掉重建后重试的 CAS，不改 head：模拟「load_head 之后、重试写入之前 head 又被
    // 推进」的交错结果（第二次 CAS 未命中）。
    let trigger = SqlTrigger::install(
        &vault,
        "swallow_second_head_cas",
        "CREATE TRIGGER swallow_second_head_cas BEFORE UPDATE OF head_seq ON loro_docs \
         BEGIN SELECT RAISE(IGNORE); END;",
    );

    let ticket = session.flush(FlushReason::Manual);
    let message = wait_for_write_error(&session, Duration::from_secs(10));
    assert!(
        message.contains("自动重建后再发生 head 冲突"),
        "必须走二次冲突的不可恢复分支：{message}"
    );
    assert!(
        message.contains("需要重开文档"),
        "不可恢复原因必须明确提示重开：{message}"
    );
    assert!(
        session
            .wait_for_durable(ticket.target_live_rev, Duration::ZERO)
            .is_none(),
        "二次冲突不得发出 DurableAck"
    );
    assert_eq!(session.pending_len(), 2, "不可恢复时 journal 必须原样保留");
    assert_eq!(session.durable_head_seq(), 0);
    assert_eq!(outbox_count(&vault), 0, "失败事务不得留下 intent");
    assert_eq!(
        count(
            &vault,
            "SELECT count(*) FROM loro_updates WHERE doc_id = 'doc-1'",
            &[],
        ),
        1,
        "只有外部那条：自动重建的重试也不得写库"
    );
    assert_eq!(head(&vault, "doc-1").head_seq, 1, "库中 head 不得被改写");

    // 移除触发器后再次 flush：不可恢复态必须短路（若真的碰库，这次就会成功）。
    trigger.remove();
    let attempt = session.flush(FlushReason::Manual);
    assert!(
        session
            .wait_for_durable(attempt.target_live_rev, Duration::ZERO)
            .is_none(),
        "不可恢复后不得再发出 DurableAck"
    );
    assert_eq!(
        count(
            &vault,
            "SELECT count(*) FROM loro_updates WHERE doc_id = 'doc-1'",
            &[],
        ),
        1,
        "不可恢复后不得再写 loro_updates"
    );
    assert_eq!(head(&vault, "doc-1").head_seq, 1, "不可恢复后不得推进 head");
    assert_eq!(
        session.last_write_error().as_deref(),
        Some(message.as_str()),
        "失败原因必须稳定可读"
    );

    // 消息里的救济方式（关闭并重开）必须真的可用：重开后从 durable 前缀加载，
    // 未 durable 的本地改动不会自动落库（这是文档化语义，不是数据静默丢失）。
    drop(session);
    let mut reopened = open_session(&vault, options_with_sync());
    assert_eq!(reopened.durable_head_seq(), 1);
    assert_eq!(reopened.markdown(), "远端", "重开后只保留 durable 前缀");
    reopened.apply(request(&reopened, "c2", 0, vec![edit(0, 0, "新")]));
    let ticket = reopened.flush(FlushReason::Manual);
    let ack = reopened
        .wait_for_durable(ticket.target_live_rev, Duration::from_secs(5))
        .expect("重开后必须能继续落库");
    assert_eq!(ack.head_seq, 2);
    assert_eq!(reopened.pending_len(), 0);
    assert!(
        reopened.markdown().contains('新') && reopened.markdown().contains("远端"),
        "重开后的正文必须同时包含 durable 前缀与新编辑：{:?}",
        reopened.markdown()
    );
}

/// 打开一个不启 ticker 的驱动（假时钟）：失败路径的收敛只靠显式 `poll`，不靠真睡。
fn open_test_driver(vault: &Vault) -> SessionDriver {
    SessionDriver::open_with_clock(
        vault,
        "doc-1",
        DriverOptions {
            session: options(PEER_A),
            auto_tick: false,
            ..DriverOptions::default()
        },
        Box::new(ManualClock::new()),
    )
    .expect("打开驱动")
}

/// 把驱动推进到「不可恢复 flush 失败」，返回闸门读到的失败原因。
///
/// 构造方式与 [`flush_reports_unrecoverable_after_bounded_rebuild`] 相同：外部 writer 写入
/// 本会话 live 未知的并发 ops，自动重建必须拒绝。
fn drive_driver_to_unrecoverable(driver: &mut SessionDriver, vault: &Vault) -> String {
    driver.apply(request(driver.session(), "c1", 0, vec![edit(0, 0, "本地")]));

    let external = BodyDoc::create("doc-1", PEER_B).expect("外部副本");
    external.insert_scalars(0, "远端").expect("外部编辑");
    let external_update = pending(&external, &loro::VersionVector::default()).expect("外部增量");
    append_updates(
        vault,
        "doc-1",
        0,
        &[UpdateRecord::new("external-1", external_update)],
    )
    .expect("外部推进 head");

    let ticket = driver.begin_external_commit().expect("begin");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match driver.poll_external_commit(&ticket) {
            GateState::Failed(GateError::FlushFailed { message, .. }) => return message,
            GateState::Flushing | GateState::WaitingIME => std::thread::yield_now(),
            other => panic!("闸门没有给出明确失败：{other:?}"),
        }
        assert!(Instant::now() < deadline, "闸门 10s 内没有给出明确失败");
    }
}

/// P1：不可恢复的 flush 失败必须能通过 `SessionDriver::last_write_error` 直接读到
/// （UI 不必先走外部提交闸门），并且没有失败时是 `None`。
#[test]
fn driver_forwards_unrecoverable_flush_error() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let mut driver = open_test_driver(&vault);
    assert_eq!(
        driver.last_write_error(),
        None,
        "没有失败时驱动必须返回 None"
    );

    let message = drive_driver_to_unrecoverable(&mut driver, &vault);
    let forwarded = driver
        .last_write_error()
        .expect("驱动必须转发不可恢复的失败原因");
    assert_eq!(forwarded, message, "驱动与闸门必须看到同一个原因");
    assert!(
        forwarded.contains("需要重开文档"),
        "驱动读到的原因必须可执行：{forwarded}"
    );
    assert_eq!(
        driver.last_write_error().as_deref(),
        Some(forwarded.as_str()),
        "多次读取必须稳定"
    );
}

/// P1：`CloseReport.flush_error` 必须带出没排空的原因；报告拿到之后驱动仍可读同一个值。
#[test]
fn close_report_surfaces_unrecoverable_flush_error() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let mut driver = open_test_driver(&vault);
    let message = drive_driver_to_unrecoverable(&mut driver, &vault);

    // 不可恢复时 `close` 必然排不空：timeout 0 也必须立刻返回（不挂住 UI）。
    let report = driver.close(Duration::ZERO);
    let close_error = report
        .flush_error
        .clone()
        .expect("CloseReport 必须带出 flush 失败原因");
    assert_eq!(close_error, message, "关闭报告与闸门必须一致");
    assert!(
        close_error.contains("需要重开文档"),
        "CloseReport 的原因必须可执行：{close_error}"
    );
    assert!(report.timed_out, "不可恢复时关闭必然超时：{report:?}");
    assert_eq!(report.pending_edits, 1, "未 ACK 条目必须原样保留");
    assert!(
        report.durable.is_none(),
        "不可恢复后不得再发出 DurableAck：{report:?}"
    );
    // 报告拿走后驱动仍在：同一个原因必须还能读到。
    assert_eq!(
        driver.last_write_error(),
        Some(close_error),
        "关闭报告产生后原因仍须可读"
    );
}
