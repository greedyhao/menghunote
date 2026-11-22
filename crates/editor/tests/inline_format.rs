//! `EditorController::toggle_inline_delimiters`（Ctrl+B / Ctrl+I）的纯模型测试。
//!
//! 这里**不启动 GPUI / storage**：直接打开夹具快照，验证
//!
//! 1. 有选区 → 用 `**` / `*` 包裹，选区保留在**内层**文本上；
//! 2. 再按一次（选区文本本身以定界符开头/结尾）→ 去掉包裹，选区保留在文本上；
//! 3. 折叠光标 → 插入成对标记（`****` / `**`），caret 在中间；
//! 4. 反向选区（anchor > head）的包裹/取消都保持方向；
//! 5. IME 组合中拒绝（`PreeditActive`，正文零变化、零请求）；
//! 6. 一次快捷键 = **一个** `EditRequest`（包裹两条插入也在同一个请求里）。

mod support;

use menghunote_editor::{EditorController, EditorError, Selection, UnicodeScalar};
use menghunote_storage::session::EditRequest;

fn open(markdown: &str) -> EditorController {
    EditorController::open(support::snapshot(markdown)).expect("测试快照必须可打开")
}

fn select(controller: &mut EditorController, anchor: usize, head: usize) {
    controller
        .set_selection(Selection::new(
            UnicodeScalar::new(anchor),
            UnicodeScalar::new(head),
        ))
        .expect("夹具选区必须在边界上");
}

fn caret(controller: &EditorController) -> (usize, usize) {
    let selection = controller.selection();
    (selection.anchor().get(), selection.head().get())
}

fn submitted(request: Option<EditRequest>, what: &str) -> EditRequest {
    request.unwrap_or_else(|| panic!("{what} 必须产生一个 EditRequest"))
}

/// 确认一条请求（本地回声），让控制器可以继续下一条命令（S1 单飞行）。
fn confirm(controller: &mut EditorController, request: &EditRequest, live_rev: u64) {
    controller
        .apply_outcome(support::applied(
            &request.command_id,
            live_rev,
            true,
            support::splice(0, 0, ""),
        ))
        .expect("本地回声必须被接受");
    assert!(
        controller.pending_command().is_none(),
        "确认后不能还压着 pending"
    );
}

#[test]
fn bold_wraps_selection_and_keeps_the_inner_selection() {
    let mut controller = open("abc");
    select(&mut controller, 0, 3);

    let request = submitted(
        controller
            .toggle_inline_delimiters("**")
            .expect("包裹粗体必须成功"),
        "Ctrl+B 包裹",
    );

    assert_eq!(controller.source_text(), "**abc**");
    assert_eq!(caret(&controller), (2, 5), "选区保留在内层 abc 上");
    assert_eq!(request.edits.len(), 2, "包裹 = 两条插入，但只有一个请求");
    assert_eq!(
        request.edits,
        vec![
            menghunote_storage::session::ScalarEdit {
                start: UnicodeScalar::new(0),
                end: UnicodeScalar::new(0),
                replacement: "**".to_owned(),
            },
            menghunote_storage::session::ScalarEdit {
                start: UnicodeScalar::new(3),
                end: UnicodeScalar::new(3),
                replacement: "**".to_owned(),
            },
        ],
        "两条插入都在同一个请求里（一个撤销组）"
    );
    assert_eq!(
        controller.pending_command(),
        Some(request.command_id.as_str())
    );
}

#[test]
fn pressing_bold_again_unwraps_and_keeps_the_text_selection() {
    let mut controller = open("**abc**");
    select(&mut controller, 0, 7);

    let request = submitted(
        controller
            .toggle_inline_delimiters("**")
            .expect("取消粗体必须成功"),
        "Ctrl+B 取消",
    );

    assert_eq!(controller.source_text(), "abc");
    assert_eq!(caret(&controller), (0, 3), "取消后选区是文本 abc");
    assert_eq!(request.edits.len(), 1, "取消包裹 = 一条替换");
}

#[test]
fn italic_wraps_and_unwraps_with_single_stars() {
    let mut controller = open("abc");
    select(&mut controller, 1, 3);
    let request = submitted(
        controller
            .toggle_inline_delimiters("*")
            .expect("包裹斜体必须成功"),
        "Ctrl+I 包裹",
    );
    assert_eq!(controller.source_text(), "a*bc*");
    assert_eq!(caret(&controller), (2, 4), "选区保留在 bc 上");
    confirm(&mut controller, &request, 8);

    // 再按一次：选区文本本身就是 `*bc*` → 去掉包裹。
    submitted(
        controller
            .toggle_inline_delimiters("*")
            .expect("取消斜体必须成功"),
        "Ctrl+I 取消",
    );
    assert_eq!(controller.source_text(), "abc");
    assert_eq!(caret(&controller), (1, 3));
}

#[test]
fn collapsed_caret_inserts_paired_markers_with_the_caret_in_the_middle() {
    let mut controller = open("abc");
    controller
        .set_caret(UnicodeScalar::new(1))
        .expect("合法 caret");

    submitted(
        controller
            .toggle_inline_delimiters("**")
            .expect("折叠光标插入粗体标记"),
        "Ctrl+B 折叠插入",
    );
    assert_eq!(controller.source_text(), "a****bc");
    assert_eq!(caret(&controller), (3, 3), "caret 在 **** 中间");

    let mut controller = open("abc");
    controller
        .set_caret(UnicodeScalar::new(1))
        .expect("合法 caret");
    submitted(
        controller
            .toggle_inline_delimiters("*")
            .expect("折叠光标插入斜体标记"),
        "Ctrl+I 折叠插入",
    );
    assert_eq!(controller.source_text(), "a**bc");
    assert_eq!(caret(&controller), (2, 2), "caret 在 ** 中间");
}

#[test]
fn pressing_bold_again_with_the_preserved_inner_selection_unwraps_the_outside_markers() {
    // 真实交互：Ctrl+B 包裹后选区保留在内层，再按一次必须去掉**外侧**标记。
    let mut controller = open("abc");
    select(&mut controller, 1, 3);
    let wrapped = submitted(
        controller.toggle_inline_delimiters("**").expect("包裹粗体"),
        "Ctrl+B 包裹",
    );
    assert_eq!(controller.source_text(), "a**bc**");
    assert_eq!(caret(&controller), (3, 5));
    confirm(&mut controller, &wrapped, 8);

    let unwrapped = submitted(
        controller
            .toggle_inline_delimiters("**")
            .expect("取消外侧粗体标记"),
        "Ctrl+B 取消",
    );
    assert_eq!(controller.source_text(), "abc");
    assert_eq!(caret(&controller), (1, 3), "取消后选区回到文本");
    assert_eq!(
        unwrapped.edits.len(),
        2,
        "外侧取消 = 只删两侧标记（两条删除，一个请求）"
    );
}

#[test]
fn reversed_selection_keeps_its_direction_after_wrapping() {
    let mut controller = open("abcd");
    select(&mut controller, 3, 1); // anchor=3, head=1（向后选）
    submitted(
        controller
            .toggle_inline_delimiters("**")
            .expect("反向选区包裹必须成功"),
        "反向选区 Ctrl+B",
    );
    assert_eq!(controller.source_text(), "a**bc**d");
    assert_eq!(
        caret(&controller),
        (5, 3),
        "方向保持：anchor 的对应端仍在 head 之后"
    );
}

#[test]
fn ime_composition_rejects_formatting_without_any_request() {
    let mut controller = open("abc");
    select(&mut controller, 0, 3);
    controller.set_preedit("zhongwen").expect("进入组合态");
    let before = controller.source_text();

    let error = controller
        .toggle_inline_delimiters("**")
        .expect_err("组合期间不允许生成正文编辑");
    assert_eq!(error, EditorError::PreeditActive);
    assert_eq!(controller.source_text(), before, "正文零变化");
    assert!(controller.pending_command().is_none(), "零请求");
}

#[test]
fn invalid_delimiter_is_a_noop() {
    let mut controller = open("abc");
    select(&mut controller, 0, 3);
    assert!(
        controller
            .toggle_inline_delimiters("")
            .expect("空定界符不 panic")
            .is_none(),
        "空定界符不产生请求"
    );
    assert_eq!(controller.source_text(), "abc");
    assert_eq!(caret(&controller), (0, 3), "选区不动");
}

#[test]
fn bold_on_a_bold_italic_selection_removes_one_layer() {
    // `***x***` 以 `**` 开头/结尾 → Ctrl+B 去掉一层，剩 `*x*`。
    let mut controller = open("***x***");
    select(&mut controller, 0, 7);
    submitted(
        controller
            .toggle_inline_delimiters("**")
            .expect("去掉一层粗体"),
        "嵌套粗体取消",
    );
    assert_eq!(controller.source_text(), "*x*");
    assert_eq!(caret(&controller), (0, 3));
}
