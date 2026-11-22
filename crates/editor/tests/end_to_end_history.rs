//! 切片 4：两 peer + Undo/Redo/Remote 的无头端到端。
//!
//! 用真实 `DocumentSession`（Loro + journal）与远端 `BodyDoc` 产生的 update，
//! 验证编辑器的镜像在 undo/redo/remote 三条路径上都与会话一致。

mod e2e_support;

use e2e_support::*;
use menghunote_editor::{EditorError, OutcomeEffect};
use menghunote_storage::session::{ChangeSource, EditOutcome, LiveRev};

#[test]
fn undo_keeps_remote_emoji_and_redo_does_not_duplicate_it() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault);
    let mut session = open_session(&vault, 64, None);
    let mut editor = open_editor(&session);

    // 本地编辑一步（单独成组，便于精确 undo）。
    let request = editor
        .compose_replace_selection("甲")
        .expect("组合")
        .expect("请求");
    editor
        .apply_outcome(EditOutcome::Applied(applied(session.apply(request))))
        .expect("确认本地回显");
    assert_eq!(session.history().undo_count, 1, "本地编辑应是一步可撤销");

    // 远端 peer 并发写入 😀，由会话导入 → LiveDelta。
    let delta = session
        .apply_remote(&remote_update(0, "😀"))
        .expect("远端导入")
        .expect("应有变更");
    assert_eq!(delta.from_rev, LiveRev(1));
    assert_eq!(delta.to_rev, LiveRev(2));
    assert_eq!(delta.source, ChangeSource::Remote);
    let effect = editor.apply_live_delta(delta).expect("应用远端 delta");
    assert_eq!(effect, OutcomeEffect::MirrorUpdated);
    assert_eq!(editor.source_text(), session.markdown());
    assert!(editor.source_text().contains('😀'));

    // undo：只撤本地 甲，远端 😀 必须留下；splice 来自真实会话。
    let undone = applied(session.undo().expect("undo").expect("有可撤销项"));
    assert_eq!(undone.source, ChangeSource::Undo);
    assert!(
        !undone.local_echo,
        "undo 不是本地输入，必须按 splice 更新镜像"
    );
    let effect = editor
        .apply_outcome(EditOutcome::Applied(undone))
        .expect("应用 undo");
    assert_eq!(effect, OutcomeEffect::MirrorUpdated);
    assert!(!editor.source_text().contains('甲'), "undo 撤掉本地字符");
    assert!(editor.source_text().contains('😀'), "undo 不得回退远端字符");
    assert_eq!(editor.source_text(), session.markdown());

    // redo：恢复本地字符，远端 😀 不重复。
    let redone = applied(session.redo().expect("redo").expect("有可重做项"));
    assert_eq!(redone.source, ChangeSource::Redo);
    editor
        .apply_outcome(EditOutcome::Applied(redone))
        .expect("应用 redo");
    assert!(editor.source_text().contains('甲'), "redo 恢复本地字符");
    assert_eq!(
        editor.source_text().matches('😀').count(),
        1,
        "redo 不能让远端字符重复"
    );
    assert_eq!(editor.source_text(), session.markdown());
}

#[test]
fn remote_delta_while_local_command_pending_requires_resync() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault);
    let mut session = open_session(&vault, 64, None);
    let mut editor = open_editor(&session);

    // 本地请求已交给会话，但编辑器**故意不消费** Applied：pending 仍然保留。
    let request = editor
        .compose_replace_selection("甲")
        .expect("组合")
        .expect("请求");
    let command_id = request.command_id.clone();
    let outcome = session.apply(request);
    assert!(matches!(outcome, EditOutcome::Applied(_)));
    assert_eq!(editor.pending_command(), Some(command_id.as_str()));
    let local_text = session.markdown();
    assert_eq!(local_text, "甲");

    // 会话侧先处理远端导入（rev1 → rev2）。
    let delta = session
        .apply_remote(&remote_update(0, "😀"))
        .expect("远端导入")
        .expect("应有变更");
    assert_eq!(delta.from_rev, LiveRev(1));

    // pending 期间收到远端 delta：必须返回重建信号，绝不静默叠加错误坐标。
    let error = editor
        .apply_live_delta(delta)
        .expect_err("pending 期间不能安全叠加远端 delta");
    assert_eq!(
        error,
        EditorError::ConcurrentDeltaWithPending(command_id),
        "必须给出重建信号"
    );
    assert!(error.is_resync_required());
    assert!(editor.needs_resync());
    assert_eq!(editor.source_text(), local_text, "镜像不得被叠加远端字符");
    assert!(!editor.source_text().contains('😀'), "不能静默混入远端文本");

    // 重建之前，编辑器拒绝继续编辑。
    assert_eq!(
        editor
            .compose_replace_selection("乙")
            .expect_err("需要重建"),
        EditorError::ResyncRequired
    );

    // 按会话新快照重建：与会话一致，远端 😀 也在。
    editor.adopt_snapshot(session.snapshot()).expect("重建");
    assert!(!editor.needs_resync());
    assert_eq!(editor.source_text(), session.markdown());
    assert!(editor.source_text().contains('😀'));
}
