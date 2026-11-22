//! ADR 0003 §7 切片 4：**编辑器 ↔ 真实会话**的无头端到端。
//!
//! 这里不再手工构造协议事件，而是打开真实 `DocumentSession` / `SessionDriver`
//!（Loro + SQLite + journal + 后台 flush/物化），验证跨 crate 契约。

mod e2e_support;

use std::time::{Duration, Instant};

use e2e_support::*;
use menghunote_editor::{OutcomeEffect, UnicodeScalar};
use menghunote_storage::session::{
    DriverEvent, EditOutcome, FlushReason, LiveRev, ManualClock, RejectReason,
};
use menghunote_storage::{load_body_state, materialize_body, MaterializeOutcome};

fn flush_reasons(events: &[DriverEvent]) -> Vec<FlushReason> {
    events
        .iter()
        .filter_map(|event| match event {
            DriverEvent::FlushScheduled { reason, .. } => Some(*reason),
            _ => None,
        })
        .collect()
}

#[test]
fn local_echo_is_applied_once_and_mirror_matches_session() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault);
    let mut session = open_session(&vault, 64, None);
    let mut editor = open_editor(&session);

    // 整段输入 NFD 夹具：本地输入先乐观写入镜像，会话回显不得二次应用。
    let request = editor
        .compose_replace_selection(NFD)
        .expect("组合编辑")
        .expect("非空替换");
    let echo = applied(session.apply(request));
    assert!(echo.local_echo, "本地命令的 Applied 必须是回声");
    let effect = editor
        .apply_outcome(EditOutcome::Applied(echo))
        .expect("确认回声");
    assert_eq!(effect, OutcomeEffect::LocalEchoConfirmed);
    assert_eq!(editor.source_text(), NFD, "回声不得把文本插两遍");
    assert_eq!(
        editor.source_text(),
        session.markdown(),
        "镜像必须与会话一致"
    );

    // 再输一个非 BMP 字符（4 字节 / 2 个 UTF-16 unit），单位仍不混。
    editor
        .set_caret(UnicodeScalar::new(editor.scalar_len()))
        .expect("尾光标");
    let request = editor
        .compose_replace_selection("\u{1F680}")
        .expect("组合编辑")
        .expect("非空替换");
    let echo = applied(session.apply(request));
    assert_eq!(echo.live_rev, LiveRev(2));
    let effect = editor
        .apply_outcome(EditOutcome::Applied(echo))
        .expect("确认回声");
    assert_eq!(effect, OutcomeEffect::LocalEchoConfirmed);
    assert_eq!(editor.source_text(), format!("{NFD}\u{1F680}"));
    assert_eq!(editor.source_text(), session.markdown());
    assert_eq!(session.live_rev(), LiveRev(2));
}

#[test]
fn fixed_window_merges_edits_into_one_flush() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault);
    let clock = ManualClock::new();
    let mut driver = open_driver(&vault, clock.clone(), 64, None);
    let mut editor = open_editor(driver.session());

    clock.set(Duration::ZERO);
    let request = editor
        .compose_replace_selection("甲")
        .expect("组合")
        .expect("请求");
    let effect = editor
        .apply_outcome(EditOutcome::Applied(applied(driver.apply(request))))
        .expect("确认");
    assert_eq!(effect, OutcomeEffect::LocalEchoConfirmed);

    // t=100ms 继续输入：固定窗口从**首次** dirty 起算，不被后续输入重置。
    clock.set(Duration::from_millis(100));
    let request = editor
        .compose_replace_selection("乙")
        .expect("组合")
        .expect("请求");
    editor
        .apply_outcome(EditOutcome::Applied(applied(driver.apply(request))))
        .expect("确认");

    assert_eq!(
        driver.next_deadline(),
        Some(Duration::from_millis(250)),
        "窗口起点锁在首次 dirty"
    );
    assert_eq!(driver.pending_edits(), 2);

    clock.set(Duration::from_millis(249));
    assert!(
        flush_reasons(&driver.tick(clock.get())).is_empty(),
        "249ms 不应 flush"
    );
    clock.set(Duration::from_millis(250));
    assert_eq!(
        flush_reasons(&driver.tick(clock.get())),
        vec![FlushReason::Interval],
        "250ms 固定窗口到期：两次编辑合并成一次 flush"
    );

    let ack = wait_durable(&mut driver, LiveRev(2), Duration::from_secs(5)).expect("DurableAck");
    assert_eq!(ack.through_live_rev, LiveRev(2));
    assert_eq!(driver.pending_edits(), 0);
    assert_eq!(editor.source_text(), driver.markdown());
}

#[test]
fn backpressure_leaves_text_unchanged_and_retries_same_command() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault);
    let clock = ManualClock::new();
    // 容量 2 + 250ms 慢盘：前两条占满 journal，第三条必然撞背压。
    let mut driver = open_driver(&vault, clock, 2, Some(Duration::from_millis(250)));
    let mut editor = open_editor(driver.session());

    let request = editor
        .compose_replace_selection("甲")
        .expect("组合")
        .expect("请求");
    editor
        .apply_outcome(EditOutcome::Applied(applied(driver.apply(request))))
        .expect("确认");
    let request = editor
        .compose_replace_selection("乙")
        .expect("组合")
        .expect("请求");
    editor
        .apply_outcome(EditOutcome::Applied(applied(driver.apply(request))))
        .expect("确认");
    assert_eq!(driver.pending_edits(), 2);

    let text_before = driver.markdown();
    assert_eq!(text_before, "甲乙");

    // 第三条：journal 满 → Rejected{Backpressure}；Loro / 会话文本必须不变。
    let request = editor
        .compose_replace_selection("丙")
        .expect("组合")
        .expect("请求");
    let optimistic = editor.source_text(); // 镜像已乐观写入一次
    let backpressured = rejected(driver.apply(request.clone()));
    assert_eq!(backpressured.reason, RejectReason::Backpressure);
    assert_eq!(driver.markdown(), text_before, "背压时 Loro 未被修改");
    assert_eq!(driver.pending_edits(), 2, "被拒请求不得进 journal");

    // 编辑器：背压不是失效；保留 pending，镜像也没有被二次写入。
    let error = editor
        .apply_outcome(EditOutcome::Rejected(backpressured))
        .expect_err("背压");
    assert_eq!(
        error,
        menghunote_editor::EditorError::Backpressured(request.command_id.clone())
    );
    assert!(!editor.needs_resync(), "背压是暂不受理，不是不可恢复");
    assert_eq!(
        editor.pending_command(),
        Some(request.command_id.as_str()),
        "同一命令必须可以重试"
    );
    assert_eq!(editor.source_text(), optimistic, "背压不产生额外镜像写入");

    // flush 释放容量后，用**同一个 command_id / 同一份 edits** 重试成功。
    driver.flush(FlushReason::Manual);
    // 慢盘：票据已投递但后台还在 sleep(250ms)，容量尚未释放，此刻重试仍被拒。
    assert_eq!(driver.pending_edits(), 2, "flush 是异步的，慢盘未写完");
    assert_eq!(
        rejected(driver.apply(request.clone())).reason,
        RejectReason::Backpressure,
        "容量未释放之前重试仍会被背压"
    );
    wait_durable(&mut driver, LiveRev(2), Duration::from_secs(5)).expect("DurableAck");
    let retried = applied(driver.apply(request.clone()));
    assert!(retried.local_echo, "本地重试成功的确认仍是回声");
    assert_eq!(retried.live_rev, LiveRev(3));
    let effect = editor
        .apply_outcome(EditOutcome::Applied(retried))
        .expect("确认重试");
    assert_eq!(effect, OutcomeEffect::LocalEchoConfirmed);
    assert_eq!(editor.source_text(), driver.markdown());
    assert_eq!(driver.markdown(), "甲乙丙");

    // 会话幂等：同一命令再投递返回同一结果，不推进、不重复插入。
    let again = applied(driver.apply(request));
    assert_eq!(again.live_rev, LiveRev(3));
    assert_eq!(driver.markdown(), "甲乙丙");
}

#[test]
fn old_ack_does_not_mark_new_input_saved() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault);
    let clock = ManualClock::new();
    let mut driver = open_driver(&vault, clock, 64, Some(Duration::from_millis(200)));
    let mut editor = open_editor(driver.session());

    let request = editor
        .compose_replace_selection("甲")
        .expect("组合")
        .expect("请求");
    editor
        .apply_outcome(EditOutcome::Applied(applied(driver.apply(request))))
        .expect("确认");
    driver.flush(FlushReason::Manual);
    let first_ack =
        wait_durable(&mut driver, LiveRev(1), Duration::from_secs(5)).expect("首个 ACK");
    editor.apply_durable_ack(&first_ack);
    assert!(editor.is_saved(), "ACK 覆盖当前 live_rev 后才算已保存");

    // ACK 之后继续输入：状态必须回到"保存中"。
    let request = editor
        .compose_replace_selection("乙")
        .expect("组合")
        .expect("请求");
    editor
        .apply_outcome(EditOutcome::Applied(applied(driver.apply(request))))
        .expect("确认");
    assert_eq!(editor.live_rev(), LiveRev(2));
    editor.apply_durable_ack(&first_ack); // 旧 ACK 再投递也不得标已保存
    assert!(
        !editor.is_saved(),
        "旧 ACK 只覆盖到 rev1，不能把 rev2 标成已保存"
    );

    // 新 flush 后的新 ACK 追上 rev2，才重新变成已保存。
    driver.flush(FlushReason::Manual);
    assert!(
        !editor.is_saved(),
        "flush 只是投递票据，ACK 未到之前仍是保存中"
    );
    let second_ack = wait_durable(&mut driver, LiveRev(2), Duration::from_secs(5)).expect("新 ACK");
    assert!(second_ack.through_live_rev >= LiveRev(2));
    editor.apply_durable_ack(&second_ack);
    assert!(editor.is_saved(), "新 ACK 覆盖新 live_rev 后才算已保存");
}

#[test]
fn acked_prefix_survives_restart_and_materializes_body() {
    let (dir, vault) = fresh_vault();
    let _seed = seed_note(&vault);
    let mut session = open_session(&vault, 64, None);
    let mut editor = open_editor(&session);

    // 已 ACK 前缀。
    let request = editor
        .compose_replace_selection(NFD)
        .expect("组合")
        .expect("请求");
    editor
        .apply_outcome(EditOutcome::Applied(applied(session.apply(request))))
        .expect("确认");
    let ticket = session.flush(FlushReason::Manual);
    let ack = session
        .wait_for_durable(ticket.target_live_rev, Duration::from_secs(5))
        .expect("DurableAck");
    assert_eq!(ack.head_seq, 1);

    // 未 ACK 的部分：模拟 A 已提交、B 未完成就崩溃。
    editor
        .set_caret(UnicodeScalar::new(editor.scalar_len()))
        .expect("尾光标");
    let request = editor
        .compose_replace_selection("尾")
        .expect("组合")
        .expect("请求");
    editor
        .apply_outcome(EditOutcome::Applied(applied(session.apply(request))))
        .expect("确认");
    assert_eq!(session.markdown(), format!("{NFD}尾"));
    assert_eq!(session.pending_len(), 1, "第二条还没 ACK");
    assert!(
        load_body_state(&vault, NOTE_ID)
            .expect("状态")
            .body_md
            .is_none(),
        "B 未完成前 body_md 仍是 NULL"
    );

    // 重启：丢掉未 ACK 部分，已 ACK 前缀由 replay 恢复。
    drop(session);
    drop(vault);
    let vault = open_vault(&dir);
    let reopened = open_session(&vault, 64, None);
    assert_eq!(reopened.markdown(), NFD, "只有已 ACK 前缀重放回来");
    assert!(!reopened.markdown().contains('尾'), "未 ACK 编辑按设计丢弃");
    assert_eq!(reopened.snapshot().durable_head_seq, 1);
    assert!(
        load_body_state(&vault, NOTE_ID)
            .expect("状态")
            .body_md
            .is_none(),
        "B 未完成时重启仍是 NULL"
    );

    // B 由物化补齐（固定 frontier，不读 live doc）。
    let outcome = materialize_body(&vault, NOTE_ID, DOC_ID, ack.head_seq, PEER_A).expect("B 物化");
    assert!(matches!(outcome, MaterializeOutcome::Applied { .. }));
    let state = load_body_state(&vault, NOTE_ID).expect("状态");
    assert_eq!(state.body_md.as_deref(), Some(NFD));
    assert_eq!(state.body_seq, 1);

    let editor = open_editor(&reopened);
    assert_eq!(editor.source_text(), NFD);
    assert!(editor.is_saved(), "重启后 live_rev 从 0 开始，基线即已保存");
}

#[test]
fn switch_flush_and_close_are_bounded_and_do_not_block_caller() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault);
    let clock = ManualClock::new();
    let mut driver = open_driver(&vault, clock, 64, Some(Duration::from_millis(300)));
    let mut editor = open_editor(driver.session());

    let request = editor
        .compose_replace_selection("甲")
        .expect("组合")
        .expect("请求");
    editor
        .apply_outcome(EditOutcome::Applied(applied(driver.apply(request))))
        .expect("确认");
    let request = editor
        .compose_replace_selection("乙")
        .expect("组合")
        .expect("请求");
    editor
        .apply_outcome(EditOutcome::Applied(applied(driver.apply(request))))
        .expect("确认");
    assert_eq!(driver.pending_edits(), 2);

    // Switch flush：只投票据，不等 300ms 慢盘写完。
    let started = Instant::now();
    let ticket = driver.flush(FlushReason::Switch);
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_millis(250),
        "flush 不得阻塞调用线程（慢盘 300ms，实际 {elapsed:?}）"
    );
    assert_eq!(ticket.target_live_rev, LiveRev(2));
    let ack = wait_durable(&mut driver, LiveRev(2), Duration::from_secs(5)).expect("Switch ACK");
    assert_eq!(ack.through_live_rev, LiveRev(2));
    assert_eq!(driver.pending_edits(), 0);

    // 切换后继续输入：新的固定窗口重新起算。
    let request = editor
        .compose_replace_selection("丙")
        .expect("组合")
        .expect("请求");
    editor
        .apply_outcome(EditOutcome::Applied(applied(driver.apply(request))))
        .expect("确认");
    assert!(driver.next_deadline().is_some(), "切换后新输入重新起窗口");

    // Close：有界排空（flush → 等 ACK → 等 B），并把 body_md 补齐。
    let started = Instant::now();
    let report = driver.close(Duration::from_secs(5));
    let elapsed = started.elapsed();
    assert!(!report.timed_out, "Close 必须排空：{report:?}");
    assert_eq!(report.pending_edits, 0);
    assert!(
        report.durable.is_some(),
        "Close 要等到覆盖最后 live_rev 的 ACK"
    );
    assert!(report.materialized >= 1, "Close 期间 B 必须完成");
    assert!(
        elapsed < Duration::from_secs(5),
        "Close 必须有界（{elapsed:?}）"
    );

    let state = load_body_state(&vault, NOTE_ID).expect("状态");
    assert_eq!(state.body_md.as_deref(), Some("甲乙丙"));
    assert_eq!(editor.source_text(), "甲乙丙");
    assert_eq!(editor.source_text(), driver.markdown());
}
