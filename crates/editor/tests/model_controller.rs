//! `EditorController` 的协议事件夹具测试（ADR 0003 §7 切片 3）。
//!
//! 这些测试**不启动真实 storage**：`OpenSnapshot` / `Applied` / `LiveDelta` /
//! `EditOutcome` 全部手工构造，验证"镜像只按确认 delta 更新一次"这条不变量。

mod support;

use menghunote_editor::{EditorController, EditorError, OutcomeEffect, Selection, UnicodeScalar};
use menghunote_storage::session::{
    EditOutcome, Generation, LiveRev, RejectReason, Rejected, ScalarEdit, SessionEpoch, StaleBase,
};

fn open(markdown: &str) -> EditorController {
    EditorController::open(support::snapshot(markdown)).expect("测试快照必须可打开")
}

#[test]
fn open_mirrors_snapshot_and_uses_scalar_selection() {
    let controller = open("A中😀e\u{301}");
    assert_eq!(controller.source_text(), "A中😀e\u{301}");
    assert_eq!(controller.scalar_len(), 5);
    assert_eq!(controller.live_rev(), LiveRev(support::BASE_REV));
    assert_eq!(
        controller.selection(),
        Selection::caret(UnicodeScalar::new(0))
    );
    assert!(controller.projection().is_identity());
    assert!(controller.pending_command().is_none());
    assert!(!controller.needs_resync());
    assert_eq!(controller.doc_id(), support::DOC_ID);
    assert_eq!(controller.generation(), Generation(3));
    assert_eq!(controller.session_epoch(), SessionEpoch(9));
}

#[test]
fn compose_local_edit_builds_request_and_preapplies() {
    let mut controller = open("ab");
    controller
        .set_caret(UnicodeScalar::new(2))
        .expect("2 是合法标量位置");

    let request = controller
        .compose_replace_selection("X")
        .expect("组合编辑必须成功")
        .expect("非空替换必须产生请求");

    assert_eq!(request.doc_id, support::DOC_ID);
    assert_eq!(request.generation, Generation(3));
    assert_eq!(request.session_epoch, SessionEpoch(9));
    assert_eq!(request.base_live_rev, LiveRev(support::BASE_REV));
    assert_eq!(
        request.edits,
        vec![ScalarEdit {
            start: UnicodeScalar::new(2),
            end: UnicodeScalar::new(2),
            replacement: "X".to_owned(),
        }]
    );
    assert!(!request.command_id.is_empty());
    assert_eq!(request.origin, "local");
    assert_eq!(request.undo_group, None);

    assert_eq!(
        controller.pending_command(),
        Some(request.command_id.as_str())
    );
    // 本地输入已乐观写入镜像一次（等待回声确认）。
    assert_eq!(controller.source_text(), "abX");
    assert_eq!(
        controller.selection(),
        Selection::caret(UnicodeScalar::new(3))
    );
}

#[test]
fn local_echo_confirms_without_second_application() {
    let mut controller = open("ab");
    controller
        .set_caret(UnicodeScalar::new(2))
        .expect("合法位置");
    let request = controller
        .compose_replace_selection("X")
        .expect("组合")
        .expect("请求");

    let effect = controller
        .apply_outcome(support::applied(
            &request.command_id,
            8,
            true,
            support::splice(2, 0, "X"),
        ))
        .expect("本地回声必须被接受");
    assert_eq!(effect, OutcomeEffect::LocalEchoConfirmed);
    assert_eq!(controller.source_text(), "abX", "回声不得二次应用 splice");
    assert_eq!(controller.live_rev(), LiveRev(8));
    assert!(controller.pending_command().is_none());

    // 同一确认重复投递：忽略，镜像仍不变。
    let effect = controller
        .apply_outcome(support::applied(
            &request.command_id,
            8,
            true,
            support::splice(2, 0, "X"),
        ))
        .expect("重复投递按幂等处理");
    assert_eq!(effect, OutcomeEffect::IgnoredDuplicate);
    assert_eq!(controller.source_text(), "abX");
}

#[test]
fn non_echo_applied_applies_splice_exactly_once() {
    let mut controller = open("ab");
    let effect = controller
        .apply_outcome(support::applied(
            "remote-1",
            8,
            false,
            support::splice(1, 0, "中"),
        ))
        .expect("非回声确认必须应用");
    assert_eq!(effect, OutcomeEffect::MirrorUpdated);
    assert_eq!(controller.source_text(), "a中b");
    assert_eq!(controller.live_rev(), LiveRev(8));

    let effect = controller
        .apply_outcome(support::applied(
            "remote-1",
            8,
            false,
            support::splice(1, 0, "中"),
        ))
        .expect("重复确认");
    assert_eq!(effect, OutcomeEffect::IgnoredDuplicate);
    assert_eq!(controller.source_text(), "a中b");
}

#[test]
fn live_delta_applies_once_and_advances_rev() {
    let mut controller = open("ab");
    let effect = controller
        .apply_live_delta(support::live_delta(7, 8, support::splice(2, 0, "😀")))
        .expect("LiveDelta 必须应用");
    assert_eq!(effect, OutcomeEffect::MirrorUpdated);
    assert_eq!(controller.source_text(), "ab😀");
    assert_eq!(controller.live_rev(), LiveRev(8));

    let effect = controller
        .apply_live_delta(support::live_delta(7, 8, support::splice(2, 0, "😀")))
        .expect("重复 delta");
    assert_eq!(effect, OutcomeEffect::IgnoredDuplicate);
    assert_eq!(controller.source_text(), "ab😀", "重复 delta 不得二次插入");
}

#[test]
fn live_delta_rev_gap_requires_resync() {
    let mut controller = open("ab");
    let error = controller
        .apply_live_delta(support::live_delta(9, 10, support::splice(0, 0, "X")))
        .expect_err("rev 不连续必须报错");
    assert_eq!(
        error,
        EditorError::RevGap {
            expected: LiveRev(7),
            actual: LiveRev(9),
        }
    );
    assert!(controller.needs_resync());
    assert_eq!(controller.source_text(), "ab", "不连续时不得改动镜像");
    assert_eq!(
        controller
            .compose_replace_selection("Y")
            .expect_err("需要重建"),
        EditorError::ResyncRequired
    );
}

#[test]
fn live_delta_while_command_in_flight_requires_resync() {
    let mut controller = open("ab");
    controller.set_caret(UnicodeScalar::new(2)).expect("合法");
    let request = controller
        .compose_replace_selection("X")
        .expect("组合")
        .expect("请求");

    let error = controller
        .apply_live_delta(support::live_delta(7, 8, support::splice(0, 0, "Z")))
        .expect_err("在飞期间不能安全叠加远端 delta");
    assert_eq!(
        error,
        EditorError::ConcurrentDeltaWithPending(request.command_id)
    );
    assert!(controller.needs_resync());
    assert_eq!(controller.source_text(), "abX", "不可靠的 delta 不得应用");
}

#[test]
fn wrong_doc_live_delta_is_rejected() {
    let mut controller = open("ab");
    let mut delta = support::live_delta(7, 8, support::splice(0, 0, "X"));
    delta.doc_id = "body-other".to_owned();
    let error = controller
        .apply_live_delta(delta)
        .expect_err("文档不符必须报错");
    assert_eq!(
        error,
        EditorError::WrongDoc {
            expected: support::DOC_ID.to_owned(),
            actual: "body-other".to_owned(),
        }
    );
    assert_eq!(controller.source_text(), "ab");
}

#[test]
fn invalid_edit_sets_are_rejected_without_mutation() {
    let mut controller = open("A中😀e\u{301}");
    let len = controller.scalar_len();
    assert_eq!(len, 5);

    assert_eq!(
        controller.compose_edits(Vec::new()).expect_err("空集合"),
        EditorError::EmptyEditSet
    );
    assert_eq!(
        controller
            .compose_edits(vec![ScalarEdit {
                start: UnicodeScalar::new(3),
                end: UnicodeScalar::new(2),
                replacement: String::new(),
            }])
            .expect_err("倒序"),
        EditorError::ReversedEdit { start: 3, end: 2 }
    );
    assert_eq!(
        controller
            .compose_edits(vec![ScalarEdit {
                start: UnicodeScalar::new(0),
                end: UnicodeScalar::new(len + 1),
                replacement: String::new(),
            }])
            .expect_err("越界"),
        EditorError::ScalarOutOfBounds {
            index: len + 1,
            len,
        }
    );
    assert_eq!(
        controller
            .compose_edits(vec![
                ScalarEdit {
                    start: UnicodeScalar::new(3),
                    end: UnicodeScalar::new(4),
                    replacement: String::new(),
                },
                ScalarEdit {
                    start: UnicodeScalar::new(1),
                    end: UnicodeScalar::new(2),
                    replacement: String::new(),
                },
            ])
            .expect_err("未排序"),
        EditorError::EditsNotSorted {
            previous_start: 3,
            next_start: 1,
        }
    );
    assert_eq!(
        controller
            .compose_edits(vec![
                ScalarEdit {
                    start: UnicodeScalar::new(0),
                    end: UnicodeScalar::new(2),
                    replacement: String::new(),
                },
                ScalarEdit {
                    start: UnicodeScalar::new(1),
                    end: UnicodeScalar::new(3),
                    replacement: String::new(),
                },
            ])
            .expect_err("重叠"),
        EditorError::OverlappingEdits {
            previous_end: 2,
            next_start: 1,
        }
    );

    // 全部失败都必须保持镜像与在飞命令不变（先校验、后写入）。
    assert_eq!(controller.source_text(), "A中😀e\u{301}");
    assert!(controller.pending_command().is_none());
}

#[test]
fn second_compose_while_command_in_flight_is_refused() {
    let mut controller = open("ab");
    controller.set_caret(UnicodeScalar::new(2)).expect("合法");
    let first = controller
        .compose_replace_selection("X")
        .expect("组合")
        .expect("请求");

    let error = controller
        .compose_replace_selection("Y")
        .expect_err("S1 单飞行");
    assert_eq!(error, EditorError::CommandInFlight(first.command_id));
    assert_eq!(controller.source_text(), "abX");
}

#[test]
fn multi_edit_request_applies_in_reverse_order() {
    let mut controller = open("abcdef");
    let request = controller
        .compose_edits(vec![
            ScalarEdit {
                start: UnicodeScalar::new(1),
                end: UnicodeScalar::new(2),
                replacement: "X".to_owned(),
            },
            ScalarEdit {
                start: UnicodeScalar::new(4),
                end: UnicodeScalar::new(5),
                replacement: "Y".to_owned(),
            },
        ])
        .expect("多编辑必须成功");

    assert_eq!(request.edits.len(), 2);
    assert_eq!(controller.source_text(), "aXcdYf");
    // 光标落在最后一条编辑（start=4）的替换文本之后，且已计入前一条的净位移 0。
    assert_eq!(
        controller.selection(),
        Selection::caret(UnicodeScalar::new(5))
    );
}

#[test]
fn remote_splice_transforms_selection() {
    let mut controller = open("abcd");
    controller.set_caret(UnicodeScalar::new(3)).expect("合法");

    // 在光标之前插入 1 个 scalar：光标右移。
    controller
        .apply_live_delta(support::live_delta(7, 8, support::splice(0, 0, "中")))
        .expect("插入 delta");
    assert_eq!(
        controller.selection(),
        Selection::caret(UnicodeScalar::new(4))
    );

    // 删除光标之前两个 scalar（此时 "bc" 位于 2..4）：光标再左移两位。
    controller
        .apply_live_delta(support::live_delta(8, 9, support::splice(2, 2, "")))
        .expect("删除 delta");
    assert_eq!(controller.source_text(), "中ad");
    assert_eq!(
        controller.selection(),
        Selection::caret(UnicodeScalar::new(2))
    );
}

#[test]
fn scalar_selection_never_confuses_bytes() {
    let fixture = "A中😀e\u{301}"; // 11 bytes / 5 scalars
    let mut controller = open(fixture);
    // 标量 2 = 『中』之后、😀 之前；若误当字节 2 会落在『中』内部。
    controller
        .set_caret(UnicodeScalar::new(2))
        .expect("标量位置合法");
    let request = controller
        .compose_replace_selection("X")
        .expect("组合")
        .expect("请求");
    assert_eq!(request.edits[0].start, UnicodeScalar::new(2));
    assert_eq!(request.edits[0].end, UnicodeScalar::new(2));
    assert_eq!(controller.source_text(), "A中X😀e\u{301}");

    // 越界标量位置（5 是尾边界，12 才越界）必须报错。
    let mut controller = open(fixture);
    assert!(controller.set_caret(UnicodeScalar::new(5)).is_ok());
    assert_eq!(
        controller
            .set_caret(UnicodeScalar::new(12))
            .expect_err("标量越界"),
        EditorError::ScalarOutOfBounds { index: 12, len: 5 }
    );
}

#[test]
fn local_echo_without_pending_command_is_rejected() {
    let mut controller = open("ab");
    let error = controller
        .apply_outcome(support::applied(
            "unknown-command",
            8,
            true,
            support::splice(0, 0, "X"),
        ))
        .expect_err("没有乐观编辑却收到回声");
    assert_eq!(
        error,
        EditorError::LocalEchoWithoutPending("unknown-command".to_owned())
    );
    assert!(controller.needs_resync());
    assert_eq!(controller.source_text(), "ab");
}

#[test]
fn non_echo_for_pending_command_requires_resync() {
    let mut controller = open("ab");
    controller.set_caret(UnicodeScalar::new(2)).expect("合法");
    let request = controller
        .compose_replace_selection("X")
        .expect("组合")
        .expect("请求");
    let error = controller
        .apply_outcome(support::applied(
            &request.command_id,
            8,
            false,
            support::splice(2, 0, "X"),
        ))
        .expect_err("乐观写入与协议声明冲突");
    assert_eq!(error, EditorError::MirrorDiverged(request.command_id));
    assert!(controller.needs_resync());
}

#[test]
fn stale_base_marks_resync_and_adopt_recovers() {
    let mut controller = open("ab");
    controller.set_caret(UnicodeScalar::new(2)).expect("合法");
    let request = controller
        .compose_replace_selection("X")
        .expect("组合")
        .expect("请求");

    let stale = EditOutcome::Stale(StaleBase {
        command_id: request.command_id.clone(),
        current_live_rev: LiveRev(9),
        compensation: None,
    });
    let error = controller.apply_outcome(stale).expect_err("stale");
    assert_eq!(
        error,
        EditorError::StaleBase {
            command_id: request.command_id,
            current_live_rev: LiveRev(9),
        }
    );
    assert!(controller.needs_resync());
    assert!(controller.pending_command().is_none());
    assert_eq!(controller.source_text(), "abX", "S1 不做猜测式回滚");

    controller
        .adopt_snapshot(support::snapshot("new"))
        .expect("按新快照重建");
    assert_eq!(controller.source_text(), "new");
    assert_eq!(controller.live_rev(), LiveRev(7));
    assert!(!controller.needs_resync());
    assert_eq!(
        controller.selection(),
        Selection::caret(UnicodeScalar::new(0))
    );
}

#[test]
fn rejected_command_marks_resync() {
    let mut controller = open("ab");
    let request = controller
        .compose_replace_selection("X")
        .expect("组合")
        .expect("请求");
    let rejected = EditOutcome::Rejected(Rejected {
        command_id: request.command_id.clone(),
        reason: RejectReason::InvalidRange,
    });
    let error = controller.apply_outcome(rejected).expect_err("rejected");
    assert_eq!(
        error,
        EditorError::Rejected {
            command_id: request.command_id,
            reason: RejectReason::InvalidRange,
        }
    );
    assert!(controller.needs_resync());
    assert_eq!(
        controller
            .compose_replace_selection("Y")
            .expect_err("需要重建"),
        EditorError::ResyncRequired
    );
}

#[test]
fn backpressure_keeps_pending_for_same_command_retry() {
    let mut controller = open("ab");
    controller.set_caret(UnicodeScalar::new(2)).expect("合法");
    let request = controller
        .compose_replace_selection("X")
        .expect("组合")
        .expect("请求");

    // 背压：请求未被受理（Loro 未改），不能把控制器判死。
    let error = controller
        .apply_outcome(EditOutcome::Rejected(Rejected {
            command_id: request.command_id.clone(),
            reason: RejectReason::Backpressure,
        }))
        .expect_err("背压");
    assert_eq!(
        error,
        EditorError::Backpressured(request.command_id.clone())
    );
    assert!(!error.is_resync_required());
    assert!(!controller.needs_resync());
    assert_eq!(
        controller.pending_command(),
        Some(request.command_id.as_str()),
        "pending 必须保留以支持同一命令重试"
    );
    assert_eq!(controller.source_text(), "abX", "背压不改动镜像");

    // flush 后用同一 command_id 的 Applied 回声确认成功。
    let effect = controller
        .apply_outcome(support::applied(
            &request.command_id,
            8,
            true,
            support::splice(2, 0, "X"),
        ))
        .expect("重试成功");
    assert_eq!(effect, OutcomeEffect::LocalEchoConfirmed);
    assert_eq!(controller.source_text(), "abX");
    assert!(controller.pending_command().is_none());
}

#[test]
fn empty_collapsed_edit_is_a_noop() {
    let mut controller = open("abc");
    assert!(controller
        .compose_replace_selection("")
        .expect("空替换是 no-op")
        .is_none());
    assert!(controller.pending_command().is_none());
    assert_eq!(controller.source_text(), "abc");
}

#[test]
fn external_snapshot_has_wrong_doc_id_case_sensitive() {
    // 仅作为 WrongDoc 错误路径的补充：换个大小写也算不同文档。
    let mut controller = open("ab");
    let mut delta = support::live_delta(7, 8, support::splice(0, 0, "X"));
    delta.doc_id = support::DOC_ID.to_uppercase();
    assert!(controller.apply_live_delta(delta).is_err());
    assert!(!controller.needs_resync(), "文档不符不应污染当前镜像");
}
