//! `InputDocument` + preedit：UTF-16 接口只针对输入文档，且 preedit 永不污染 source。

mod support;

use menghunote_editor::{
    CoordError, EditorController, EditorError, GraphemeStep, InputUtf16, Selection, UnicodeScalar,
};
use menghunote_storage::session::ScalarEdit;

fn open(markdown: &str) -> EditorController {
    EditorController::open(support::snapshot(markdown)).expect("测试快照必须可打开")
}

#[test]
fn input_document_equals_source_without_composition() {
    let controller = open("A中😀e\u{301}");
    let document = controller.input_document().expect("输入文档");
    assert_eq!(document.text(), "A中😀e\u{301}");
    assert_eq!(document.preedit(), "");
    assert_eq!(document.byte_len(), 11);
    assert_eq!(document.scalar_len(), 5);
    assert_eq!(document.utf16_len(), 6);
}

#[test]
fn preedit_lives_only_in_the_input_document() {
    let mut controller = open("A中😀e\u{301}");
    controller
        .set_caret(UnicodeScalar::new(1))
        .expect("标量位置合法");
    controller.set_preedit("\u{1F600}").expect("设置 preedit");

    // source 与在飞命令都不受 preedit 影响。
    assert_eq!(controller.source_text(), "A中😀e\u{301}");
    assert!(
        controller.pending_command().is_none(),
        "preedit 不得产生 ScalarEdit"
    );
    assert_eq!(controller.preedit_text(), Some("\u{1F600}"));

    let document = controller.input_document().expect("输入文档");
    assert_eq!(document.text(), "A\u{1F600}中\u{1F600}e\u{301}");
    assert_eq!(document.preedit(), "\u{1F600}");
    assert_eq!(document.byte_len(), 15);
    assert_eq!(document.scalar_len(), 6);
    assert_eq!(document.utf16_len(), 8);
    // preedit 从 UTF-16 序号 1 开始，宽 2（非 BMP 字符是代理对）。
    assert_eq!(
        document.preedit_utf16_range().expect("preedit 范围"),
        (InputUtf16::new(1), InputUtf16::new(3))
    );

    // 组合期间拒绝一切正文编辑（因此 preedit 不可能变成 source）。
    assert_eq!(
        controller
            .compose_replace_selection("X")
            .expect_err("组合期间禁止编辑"),
        EditorError::PreeditActive
    );
    assert_eq!(
        controller
            .delete_grapheme(GraphemeStep::Backward)
            .expect_err("组合期间禁止编辑"),
        EditorError::PreeditActive
    );
    assert_eq!(controller.source_text(), "A中😀e\u{301}");
}

#[test]
fn utf16_offsets_are_relative_to_the_input_document() {
    let mut controller = open("A中😀e\u{301}");
    controller
        .set_caret(UnicodeScalar::new(1))
        .expect("标量位置合法");
    controller.set_preedit("\u{1F600}").expect("设置 preedit");
    let document = controller.input_document().expect("输入文档");

    // 输入文档 = "A😀中😀e◌́"；序号 2 落在第一个 😀 的代理对中间。
    assert_eq!(
        document.utf16_to_byte_offset(InputUtf16::new(2)),
        Err(CoordError::Utf16InsideSurrogatePair { index: 2, len: 8 })
    );
    // 序号 3 = 第一个 😀 之后，对应 scalar 2（输入文档的『中』）。
    assert_eq!(
        document
            .utf16_to_scalar_index(InputUtf16::new(3))
            .expect("合法边界"),
        2
    );
    // 越界序号报错，不截断。
    assert!(document.utf16_to_byte_offset(InputUtf16::new(9)).is_err());
}

#[test]
fn commit_preedit_emits_exactly_one_request_and_clears_composition() {
    let mut controller = open("A中😀e\u{301}");
    controller
        .set_caret(UnicodeScalar::new(1))
        .expect("标量位置合法");
    controller.set_preedit("\u{1F600}").expect("设置 preedit");

    let request = controller
        .commit_preedit("\u{1F600}")
        .expect("提交必须成功")
        .expect("非空提交必须产生请求");

    assert_eq!(
        request.edits,
        vec![ScalarEdit {
            start: UnicodeScalar::new(1),
            end: UnicodeScalar::new(1),
            replacement: "\u{1F600}".to_owned(),
        }]
    );
    // 只有 commit 文本进入 source 一次；preedit 本身没有独立写入。
    assert_eq!(controller.source_text(), "A\u{1F600}中\u{1F600}e\u{301}");
    assert_eq!(controller.preedit_text(), None);
    assert_eq!(
        controller.pending_command(),
        Some(request.command_id.as_str())
    );
    // 组合结束后输入文档就是 source。
    assert_eq!(
        controller.input_document().expect("输入文档").text(),
        controller.source_text()
    );
}

#[test]
fn empty_commit_on_collapsed_selection_is_a_noop() {
    let mut controller = open("ab");
    controller.set_caret(UnicodeScalar::new(1)).expect("合法");
    controller.set_preedit("中").expect("设置 preedit");

    assert!(controller
        .commit_preedit("")
        .expect("空提交必须成功")
        .is_none());
    assert_eq!(controller.source_text(), "ab");
    assert_eq!(controller.preedit_text(), None);
    assert!(controller.pending_command().is_none());
}

#[test]
fn empty_commit_with_selection_deletes_the_selection() {
    let mut controller = open("A中😀e\u{301}");
    controller
        .set_selection(Selection::new(UnicodeScalar::new(1), UnicodeScalar::new(3)))
        .expect("选区合法"); // 『中』+ 😀
    controller.set_preedit("X").expect("设置 preedit");

    let request = controller
        .commit_preedit("")
        .expect("提交必须成功")
        .expect("删除选区必须产生请求");
    assert_eq!(
        request.edits,
        vec![ScalarEdit {
            start: UnicodeScalar::new(1),
            end: UnicodeScalar::new(3),
            replacement: String::new(),
        }]
    );
    assert_eq!(controller.source_text(), "Ae\u{301}");
}

#[test]
fn clear_preedit_cancels_without_touching_source_or_selection() {
    let mut controller = open("abc");
    let selection = Selection::new(UnicodeScalar::new(1), UnicodeScalar::new(2));
    controller.set_selection(selection).expect("选区合法");
    controller.set_preedit("中").expect("设置 preedit");
    controller.clear_preedit();

    assert_eq!(controller.source_text(), "abc");
    assert_eq!(controller.selection(), selection);
    assert_eq!(controller.preedit_text(), None);
    assert!(controller
        .compose_replace_selection("X")
        .expect("可以编辑")
        .is_some());
}

#[test]
fn commit_without_composition_is_an_error() {
    let mut controller = open("ab");
    assert_eq!(
        controller.commit_preedit("x").expect_err("没有组合"),
        EditorError::NoPreedit
    );
}

#[test]
fn preedit_is_refused_while_a_command_is_in_flight() {
    let mut controller = open("ab");
    controller.set_caret(UnicodeScalar::new(2)).expect("合法");
    let request = controller
        .compose_replace_selection("X")
        .expect("组合")
        .expect("请求");

    assert_eq!(
        controller
            .set_preedit("中")
            .expect_err("在飞命令期间不叠加组合"),
        EditorError::CommandInFlight(request.command_id)
    );
    assert!(controller.preedit_text().is_none());
}

#[test]
fn remote_delta_during_composition_cancels_preedit_without_editing() {
    let mut controller = open("ab");
    controller.set_preedit("中").expect("设置 preedit");
    controller
        .apply_live_delta(support::live_delta(7, 8, support::splice(1, 0, "Z")))
        .expect("远端 delta 必须应用");

    // 组合基准已变：S1 直接取消组合，既不写 source 也不产生编辑请求。
    assert_eq!(controller.preedit_text(), None);
    assert_eq!(controller.source_text(), "aZb");
    assert!(controller.pending_command().is_none());
}
