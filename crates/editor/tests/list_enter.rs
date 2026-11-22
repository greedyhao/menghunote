//! ADR 0019 S1：列表 Enter 的纯模型契约（同级续写 / 空项退出 / 嵌套 / 有序 / IME 守卫）。

mod support;

use menghunote_editor::{EditorController, EditorError, UnicodeScalar};

fn open(source: &str) -> EditorController {
    EditorController::open(support::snapshot(source)).expect("打开测试快照失败")
}

fn set_caret(controller: &mut EditorController, scalar: usize) {
    controller
        .set_caret(UnicodeScalar::new(scalar))
        .expect("caret 必须合法");
}

#[test]
fn enter_on_non_empty_item_inserts_same_level_marker() {
    let mut controller = open("- alpha\n");
    // 行尾（换行之前）：真实用户在 item 行按 Enter 的落点。
    set_caret(&mut controller, 7);
    let request = controller
        .list_enter()
        .expect("Enter 不应失败")
        .expect("应产生一个 EditRequest");
    assert_eq!(request.edits.len(), 1, "一次 Enter 只能有一个 ScalarEdit");
    assert_eq!(controller.source_text(), "- alpha\n- \n");
    // caret 落在新 marker 之后（空 item 的内容起点）。
    assert_eq!(controller.selection().head().get(), 10);
}

#[test]
fn enter_on_empty_item_exits_the_list() {
    let mut controller = open("- alpha\n- \n");
    // 空项的内容起点 = marker 之后（scalar 10；行尾在换行之前）。
    set_caret(&mut controller, 10);
    let request = controller
        .list_enter()
        .expect("Enter 不应失败")
        .expect("空项退出也要产生一个 EditRequest");
    assert_eq!(request.edits.len(), 1);
    assert_eq!(controller.source_text(), "- alpha\n\n");
    assert_eq!(controller.selection().head().get(), 8);
}

#[test]
fn enter_on_nested_item_keeps_nesting_indent() {
    let source = "- a\n  - b\n";
    let mut controller = open(source);
    // 第二行行尾（最后一个换行之前）。
    set_caret(&mut controller, source.chars().count() - 1);
    controller
        .list_enter()
        .expect("Enter 不应失败")
        .expect("应产生请求");
    assert_eq!(controller.source_text(), "- a\n  - b\n  - \n");
}

#[test]
fn enter_on_ordered_item_uses_next_number_without_renumbering() {
    let source = "1. one\n2. two\n";
    let mut controller = open(source);
    set_caret(&mut controller, source.chars().count() - 1);
    controller
        .list_enter()
        .expect("Enter 不应失败")
        .expect("应产生请求");
    // 新 item 用 3；已有 1/2 完全不动（不整篇重编号）。
    assert_eq!(controller.source_text(), "1. one\n2. two\n3. \n");
}

#[test]
fn enter_inside_marker_or_not_in_list_falls_back() {
    let mut controller = open("- item\n");
    // caret 在 marker 里（scalar 0）：不接管，返回 None 让 app 走普通换行。
    set_caret(&mut controller, 0);
    assert!(controller.list_enter().expect("不报错").is_none());

    let mut plain = open("plain text\n");
    let end = plain.source_text().chars().count();
    set_caret(&mut plain, end);
    assert!(plain.list_enter().expect("不报错").is_none());
}

#[test]
fn enter_is_rejected_during_ime_composition() {
    let mut controller = open("- item\n");
    let end = controller.source_text().chars().count();
    set_caret(&mut controller, end);
    controller.set_preedit("nihao").expect("进入组合");
    let error = controller
        .list_enter()
        .expect_err("IME 组合期间不得触发结构动作");
    assert!(matches!(error, EditorError::PreeditActive), "{error:?}");
    controller.clear_preedit();
    assert_eq!(controller.source_text(), "- item\n", "被拒后 source 不变");
}

#[test]
fn enter_with_selection_is_a_plain_replacement() {
    let mut controller = open("- alpha\n");
    controller
        .set_selection(menghunote_editor::Selection::new(
            UnicodeScalar::new(2),
            UnicodeScalar::new(4),
        ))
        .expect("选区合法");
    controller
        .list_enter()
        .expect("Enter 不应失败")
        .expect("替换选区应产生请求");
    assert_eq!(controller.source_text(), "- \npha\n");
}

#[test]
fn enter_on_continuation_or_fence_line_falls_back_to_plain_newline() {
    // 续行/子段落/内部围栏属于 item 内容但不在首行：S1 不猜结构，交回普通换行。
    let source = "- item\n\n  ```\n  code\n  ```\n";
    let mut controller = open(source);
    let code_end = source.find("  code\n").expect("正文") + "  code".len();
    let scalar = source[..code_end].chars().count();
    set_caret(&mut controller, scalar);
    assert!(
        controller.list_enter().expect("不报错").is_none(),
        "内部围栏行不触发列表结构动作"
    );
    assert_eq!(controller.source_text(), source, "未产生编辑");
}
