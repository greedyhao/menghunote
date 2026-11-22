//! grapheme 导航 / 删除：交互边界只用于"选哪一段"，写进请求的仍是 scalar 坐标。
//!
//! 本文件同时锁定 task-16 的按键级 API：`delete_backward()`（Backspace）与
//! `delete_forward()`（Delete）。它们与普通输入走**同一条** `compose_edits` 路径
//! （乐观写镜像 + pending + OutcomeEffect），不允许 app 侧另拼 `ScalarEdit`。

mod support;

use menghunote_editor::{
    units, EditorController, GraphemeStep, OutcomeEffect, Selection, UnicodeScalar,
};
use menghunote_storage::session::{EditRequest, LiveRev};

/// A / 中 / 😀 / ZWJ 家庭 / e + 组合重音：12 scalars、4 个以上 grapheme。
const TEXT: &str = "A中😀\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}e\u{301}";

fn open(markdown: &str) -> EditorController {
    EditorController::open(support::snapshot(markdown)).expect("测试快照必须可打开")
}

/// 给一次本地删除喂回声（`local_echo = true`），让控制器回到可继续编辑的状态。
///
/// 这条路径与真实会话的 `Applied` 相同：镜像在 `delete_*` 时已乐观写入，
/// 回声只推进 `live_rev`，不得二次应用。
fn confirm_local_echo(controller: &mut EditorController, request: &EditRequest, live_rev: u64) {
    let edit = request.edits.first().expect("删除至少产生一条编辑");
    let deleted = u64::try_from(edit.end.get() - edit.start.get()).expect("u64 范围");
    let outcome = support::applied(
        &request.command_id,
        live_rev,
        true,
        support::splice(edit.start.get(), deleted, &edit.replacement),
    );
    let effect = controller.apply_outcome(outcome).expect("本地回声");
    assert_eq!(effect, OutcomeEffect::LocalEchoConfirmed);
}

#[test]
fn forward_navigation_skips_grapheme_internals() {
    let mut controller = open(TEXT);
    assert_eq!(controller.scalar_len(), 12);

    // 0 → 1(A) → 2(中) → 3(😀) → 10(家庭 emoji 整体跳过) → 12(e+重音 整体跳过)。
    for (step, expected) in [1usize, 2, 3, 10, 12].into_iter().enumerate() {
        controller
            .move_grapheme(GraphemeStep::Forward, false)
            .expect("grapheme 移动");
        assert_eq!(
            controller.selection().head(),
            UnicodeScalar::new(expected),
            "第 {} 次前移",
            step + 1
        );
    }

    // 尾边界再前移是 no-op。
    controller
        .move_grapheme(GraphemeStep::Forward, false)
        .expect("尾边界");
    assert_eq!(controller.selection().head(), UnicodeScalar::new(12));
}

#[test]
fn backward_navigation_skips_grapheme_internals() {
    let mut controller = open(TEXT);
    controller
        .set_caret(UnicodeScalar::new(12))
        .expect("尾边界合法");

    for (step, expected) in [10usize, 3, 2, 1, 0].into_iter().enumerate() {
        controller
            .move_grapheme(GraphemeStep::Backward, false)
            .expect("grapheme 移动");
        assert_eq!(
            controller.selection().head(),
            UnicodeScalar::new(expected),
            "第 {} 次后移",
            step + 1
        );
    }

    controller
        .move_grapheme(GraphemeStep::Backward, false)
        .expect("首边界");
    assert_eq!(controller.selection().head(), UnicodeScalar::new(0));
}

#[test]
fn extend_keeps_anchor_for_selection() {
    let mut controller = open(TEXT);
    controller
        .move_grapheme(GraphemeStep::Forward, true)
        .expect("扩选 1");
    assert_eq!(
        controller.selection(),
        Selection::new(UnicodeScalar::new(0), UnicodeScalar::new(1))
    );
    controller
        .move_grapheme(GraphemeStep::Forward, true)
        .expect("扩选 2");
    assert_eq!(
        controller.selection(),
        Selection::new(UnicodeScalar::new(0), UnicodeScalar::new(2))
    );
    // 收起扩选：anchor 跟到新 head。
    controller
        .move_grapheme(GraphemeStep::Forward, false)
        .expect("收起");
    assert_eq!(
        controller.selection(),
        Selection::new(UnicodeScalar::new(3), UnicodeScalar::new(3))
    );
}

#[test]
fn delete_backward_removes_the_whole_grapheme() {
    let mut controller = open(TEXT);
    controller
        .set_caret(UnicodeScalar::new(12))
        .expect("尾边界合法");
    let request = controller
        .delete_backward()
        .expect("删除必须成功")
        .expect("非空文档必须产生请求");

    // e + 组合重音是一个 grapheme：删除整个 10..12，而不是删掉两个 scalar 之一。
    assert_eq!(request.edits.len(), 1, "一次删除只产生一个 ScalarEdit");
    assert_eq!(request.base_live_rev, LiveRev(support::BASE_REV));
    assert_eq!(request.edits[0].start, UnicodeScalar::new(10));
    assert_eq!(request.edits[0].end, UnicodeScalar::new(12));
    assert_eq!(request.edits[0].replacement, "");
    // 绝不是字节坐标 33..36。
    assert_ne!(request.edits[0].start.get(), 33);
    assert_eq!(
        controller.source_text(),
        "A中😀\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}"
    );
    assert_eq!(controller.scalar_len(), 10);
}

#[test]
fn delete_backward_removes_zwj_family_as_one_unit() {
    let mut controller = open(TEXT);
    // 家庭 emoji 位于 scalar 3..10；光标放在它右侧（`e` 之前）。
    controller
        .set_caret(UnicodeScalar::new(10))
        .expect("家庭 emoji 右侧是合法边界");
    let request = controller
        .delete_backward()
        .expect("删除必须成功")
        .expect("请求");

    assert_eq!(request.edits.len(), 1);
    assert_eq!(request.edits[0].start, UnicodeScalar::new(3));
    assert_eq!(
        request.edits[0].end,
        UnicodeScalar::new(10),
        "ZWJ 家庭整体（7 个 scalar）算一个交互单位"
    );
    assert_ne!(request.edits[0].start.get(), 8, "不是字节偏移");
    assert_eq!(controller.source_text(), "A中😀e\u{301}");
    assert_eq!(controller.scalar_len(), 5);
}

#[test]
fn delete_forward_removes_the_next_grapheme() {
    let mut controller = open(TEXT);
    let request = controller
        .delete_forward()
        .expect("删除必须成功")
        .expect("请求");
    assert_eq!(request.edits.len(), 1);
    assert_eq!(request.edits[0].start, UnicodeScalar::new(0));
    assert_eq!(request.edits[0].end, UnicodeScalar::new(1));
    assert_eq!(
        controller.source_text(),
        "中😀\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}e\u{301}"
    );
}

#[test]
fn delete_forward_does_not_split_surrogate_pair_emoji() {
    let mut controller = open("A😀B");
    assert_eq!(units::utf16_len("A😀B"), 4, "😀 占 2 个 UTF-16 unit");

    controller
        .set_caret(UnicodeScalar::new(1))
        .expect("光标在 😀 之前");
    let request = controller
        .delete_forward()
        .expect("删除必须成功")
        .expect("请求");

    // 😀 是 1 个 scalar（但 2 个 UTF-16 code unit）：删除必须整对删掉，不能拆半。
    assert_eq!(request.edits.len(), 1);
    assert_eq!(request.edits[0].start, UnicodeScalar::new(1));
    assert_eq!(request.edits[0].end, UnicodeScalar::new(2));
    assert_eq!(controller.source_text(), "AB");
    assert_eq!(
        units::utf16_len(&controller.source_text()),
        2,
        "代理对的两个 code unit 一起被删"
    );
}

#[test]
fn delete_with_selection_deletes_the_selection_as_one_edit() {
    let mut controller = open(TEXT);
    controller
        .set_selection(Selection::new(UnicodeScalar::new(0), UnicodeScalar::new(2)))
        .expect("选区合法");
    let request = controller
        .delete_backward()
        .expect("删除必须成功")
        .expect("请求");

    // 有选区时删选区：**一个** ScalarEdit，绝不是一个 grapheme 一个请求。
    assert_eq!(request.edits.len(), 1);
    assert_eq!(request.base_live_rev, LiveRev(support::BASE_REV));
    assert_eq!(request.edits[0].start, UnicodeScalar::new(0));
    assert_eq!(request.edits[0].end, UnicodeScalar::new(2));
    assert_eq!(request.edits[0].replacement, "");
    assert_eq!(
        controller.source_text(),
        "😀\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}e\u{301}"
    );

    // Delete 方向同样先删选区，而不是向后删一个 grapheme。
    let mut forward = open("abc");
    forward
        .set_selection(Selection::new(UnicodeScalar::new(0), UnicodeScalar::new(3)))
        .expect("选区合法");
    let request = forward.delete_forward().expect("删除").expect("请求");
    assert_eq!(request.edits.len(), 1);
    assert_eq!(request.edits[0].start, UnicodeScalar::new(0));
    assert_eq!(request.edits[0].end, UnicodeScalar::new(3));
    assert_eq!(forward.source_text(), "");
}

#[test]
fn delete_on_empty_document_is_a_noop() {
    let mut controller = open("");
    let live_rev = controller.live_rev();
    let saved_before = controller.is_saved();

    assert!(controller.delete_backward().expect("退格").is_none());
    assert!(controller.delete_forward().expect("Delete").is_none());

    assert_eq!(controller.source_text(), "");
    assert_eq!(controller.live_rev(), live_rev, "no-op 不得推进 live_rev");
    assert!(controller.pending_command().is_none(), "no-op 不得产生请求");
    assert_eq!(
        controller.is_saved(),
        saved_before,
        "no-op 不得改变保存状态"
    );
}

#[test]
fn consecutive_backward_deletes_cross_lines_through_the_edit_path() {
    let mut controller = open("ab\ncd\n");
    controller
        .set_caret(UnicodeScalar::new(controller.scalar_len()))
        .expect("尾光标");

    let mut remaining: Vec<char> = "ab\ncd\n".chars().collect();
    let mut removed = Vec::new();
    let mut live_rev = support::BASE_REV;
    let mut requests = 0usize;

    while let Some(request) = controller.delete_backward().expect("退格") {
        requests += 1;
        assert_eq!(request.edits.len(), 1, "一次退格只产生一个 ScalarEdit");
        assert_eq!(request.base_live_rev, LiveRev(live_rev));

        let edit = &request.edits[0];
        assert_eq!(
            edit.end.get(),
            edit.start.get() + 1,
            "本夹具里每个 grapheme 都恰好 1 个 scalar"
        );
        removed.push(remaining.remove(edit.start.get()));
        assert_eq!(
            controller.source_text(),
            remaining.iter().collect::<String>(),
            "镜像必须立即反映这次删除"
        );

        // 回声确认后 pending 清空，下一次退格才能继续走同一路径。
        live_rev += 1;
        confirm_local_echo(&mut controller, &request, live_rev);
        assert!(controller.pending_command().is_none());
    }

    // 连续退格跨行：先删换行，再删上一行字符，直到空文档。
    assert_eq!(removed, vec!['\n', 'd', 'c', '\n', 'b', 'a']);
    assert_eq!(requests, 6);
    assert_eq!(controller.source_text(), "");
    assert_eq!(controller.live_rev(), LiveRev(live_rev));
}

#[test]
fn delete_at_document_edges_is_a_noop() {
    let mut controller = open(TEXT);
    let live_rev = controller.live_rev();

    assert!(controller.delete_backward().expect("首边界删除").is_none());
    controller
        .set_caret(UnicodeScalar::new(12))
        .expect("尾边界合法");
    assert!(controller.delete_forward().expect("尾边界删除").is_none());

    assert_eq!(controller.source_text(), TEXT);
    assert_eq!(
        controller.live_rev(),
        live_rev,
        "边界 no-op 不得推进 live_rev"
    );
    assert!(controller.pending_command().is_none());
}

#[test]
fn navigation_does_not_emit_grapheme_coordinates() {
    // 只移动光标不产生请求；随后一次删除产生的仍是 scalar 区间。
    let mut controller = open("e\u{301}x");
    controller
        .move_grapheme(GraphemeStep::Forward, false)
        .expect("移动到 x 之前");
    assert_eq!(controller.selection().head(), UnicodeScalar::new(2));
    let request = controller
        .compose_replace_selection("Y")
        .expect("组合")
        .expect("请求");
    assert_eq!(request.edits[0].start, UnicodeScalar::new(2));
}
