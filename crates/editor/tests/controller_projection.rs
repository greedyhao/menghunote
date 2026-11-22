//! S2-3a：`EditorController` 的显示投影集成测试。
//!
//! 覆盖：identity、caret 进出构造改变 display、选区跨构造双显现、编辑 / 删除 /
//! undo / redo / IME 提交后 display 与 source 同步、选区与双向坐标映射。

mod support;

use menghunote_editor::{DisplayByte, EditorController, Selection, SourceByte, UnicodeScalar};
use menghunote_storage::session::LiveRev;

fn open(markdown: &str) -> EditorController {
    EditorController::open(support::snapshot(markdown)).expect("测试快照必须可打开")
}

#[test]
fn plain_document_projects_identity() {
    let controller = open("hello");
    assert!(controller.projection().is_identity());
    assert_eq!(controller.display_text().as_ref(), "hello");
    assert_eq!(controller.display_len().get(), 5);
    assert_eq!(
        controller.source_to_display(SourceByte::new(3)),
        Some(DisplayByte::new(3))
    );
    assert_eq!(
        controller.display_to_source(DisplayByte::new(3)),
        Some(SourceByte::new(3))
    );
    assert_eq!(
        controller.selection_in_display(),
        Some((DisplayByte::new(0), DisplayByte::new(0)))
    );
    assert_eq!(
        controller.display_text().as_ref(),
        controller.projection().display_text().as_ref(),
        "display_text 必须与投影同源"
    );
}

#[test]
fn moving_caret_changes_display_len_without_changing_source() {
    let mut controller = open("a **bold** b");
    let source = controller.source_text();

    // caret 在构造外：`**` 隐藏。
    assert_eq!(controller.display_text().as_ref(), "a bold b");
    assert_eq!(controller.display_len().get(), 8);

    // caret 进入粗体内容：标记回到可见 → display 变长，但 source 不变。
    controller
        .set_caret(UnicodeScalar::new(5))
        .expect("5 是合法 caret");
    assert_eq!(controller.source_text(), source, "移动 caret 不得改 source");
    assert_eq!(controller.display_text().as_ref(), "a **bold** b");
    assert_eq!(controller.display_len().get(), 12);
    assert_eq!(
        controller.display_len(),
        controller.projection().display_len()
    );

    // 移回构造外：标记再次隐藏。
    controller
        .set_caret(UnicodeScalar::new(1))
        .expect("1 是合法 caret");
    assert_eq!(controller.source_text(), source);
    assert_eq!(controller.display_text().as_ref(), "a bold b");
    assert_eq!(controller.display_len().get(), 8);

    // 构造尾边界（inclusive end）仍算“在构造内”；再往外一格才隐藏。
    controller
        .set_caret(UnicodeScalar::new(10))
        .expect("10 是合法 caret");
    assert_eq!(controller.display_text().as_ref(), "a **bold** b");
    controller
        .set_caret(UnicodeScalar::new(11))
        .expect("11 是合法 caret");
    assert_eq!(controller.display_text().as_ref(), "a bold b");
}

#[test]
fn selection_spanning_two_constructs_reveals_both() {
    let mut controller = open("**a** *b*");
    // 默认 caret 在 0，落在粗体 span 内：粗体显现，斜体隐藏。
    assert_eq!(controller.display_text().as_ref(), "**a** b");

    controller
        .set_selection(Selection::new(UnicodeScalar::new(0), UnicodeScalar::new(9)))
        .expect("全选合法");
    assert_eq!(
        controller.display_text().as_ref(),
        "**a** *b*",
        "选区跨两个构造 → 两处标记都显现"
    );

    controller
        .set_selection(Selection::new(UnicodeScalar::new(5), UnicodeScalar::new(6)))
        .expect("选区合法");
    assert_eq!(
        controller.display_text().as_ref(),
        "a b",
        "选区不碰任何构造 → 两处标记都隐藏"
    );
}

#[test]
fn insert_delete_undo_redo_keep_display_in_sync() {
    let mut controller = open("a **b** c");
    assert_eq!(controller.display_text().as_ref(), "a b c");

    // 输入：末尾插入 X。
    controller
        .set_caret(UnicodeScalar::new(9))
        .expect("末尾 caret 合法");
    let insert = controller
        .compose_replace_selection("X")
        .expect("输入必须成功")
        .expect("非空输入必须产生请求");
    assert_eq!(controller.source_text(), "a **b** cX");
    assert_eq!(controller.display_text().as_ref(), "a b cX");
    controller
        .apply_outcome(support::applied(
            &insert.command_id,
            8,
            true,
            support::splice(9, 0, "X"),
        ))
        .expect("本地回声确认");
    assert_eq!(controller.live_rev(), LiveRev(8));

    // 删除：Backspace 删掉 X。
    let delete = controller
        .delete_backward()
        .expect("删除必须成功")
        .expect("有可删字符");
    assert_eq!(controller.source_text(), "a **b** c");
    assert_eq!(controller.display_text().as_ref(), "a b c");
    assert_eq!(
        controller.selection(),
        Selection::caret(UnicodeScalar::new(9))
    );
    controller
        .apply_outcome(support::applied(
            &delete.command_id,
            9,
            true,
            support::splice(9, 1, ""),
        ))
        .expect("本地回声确认");
    assert_eq!(
        controller.display_text().as_ref(),
        controller.projection().display_text().as_ref()
    );

    // undo：非本地回声，镜像按 splice 应用（把 X 插回）。
    controller
        .apply_outcome(support::applied(
            "undo-1",
            10,
            false,
            support::splice(9, 0, "X"),
        ))
        .expect("undo 的 Applied 必须被接受");
    assert_eq!(controller.source_text(), "a **b** cX");
    assert_eq!(controller.display_text().as_ref(), "a b cX");
    assert_eq!(controller.display_len().get(), 6);

    // redo：再删掉 X。
    controller
        .apply_outcome(support::applied(
            "redo-1",
            11,
            false,
            support::splice(9, 1, ""),
        ))
        .expect("redo 的 Applied 必须被接受");
    assert_eq!(controller.source_text(), "a **b** c");
    assert_eq!(controller.display_text().as_ref(), "a b c");
    assert_eq!(controller.display_len().get(), 5);
    assert_eq!(
        controller.display_text().as_ref(),
        controller.projection().display_text().as_ref()
    );
}

#[test]
fn ime_commit_updates_source_and_display() {
    let mut controller = open("ab");
    controller
        .set_caret(UnicodeScalar::new(2))
        .expect("末尾 caret 合法");

    // preedit 只存在于输入文档，不进入 source / display。
    controller.set_preedit("拼").expect("开始组合");
    assert_eq!(controller.source_text(), "ab");
    assert_eq!(controller.display_text().as_ref(), "ab");

    let request = controller
        .commit_preedit("拼")
        .expect("提交必须成功")
        .expect("非空提交必须产生请求");
    assert_eq!(controller.source_text(), "ab拼");
    assert_eq!(controller.display_text().as_ref(), "ab拼");
    assert_eq!(controller.display_len().get(), 5);
    controller
        .apply_outcome(support::applied(
            &request.command_id,
            8,
            true,
            support::splice(2, 0, "拼"),
        ))
        .expect("本地回声确认");
    assert_eq!(controller.display_text().as_ref(), "ab拼");
}

#[test]
fn editing_inside_construct_keeps_markers_visible() {
    let mut controller = open("**a**");
    assert_eq!(controller.display_text().as_ref(), "**a**");

    controller
        .set_caret(UnicodeScalar::new(2))
        .expect("内容起点");
    let request = controller
        .compose_replace_selection("X")
        .expect("输入必须成功")
        .expect("请求");
    assert_eq!(controller.source_text(), "**Xa**");
    assert_eq!(
        controller.display_text().as_ref(),
        "**Xa**",
        "编辑后 caret 仍在新构造内，标记保持可见"
    );
    controller
        .apply_outcome(support::applied(
            &request.command_id,
            8,
            true,
            support::splice(2, 0, "X"),
        ))
        .expect("本地回声确认");
}

#[test]
fn mappings_and_selection_follow_projection_bias() {
    let mut controller = open("a **b** c");
    // caret 0 在构造外 → display "a b c"（隐藏粗体的两段 `**`）。
    assert_eq!(controller.display_text().as_ref(), "a b c");

    // source → display：省略段内部取段起点，省略段末尾取下个可见段起点。
    for (source_at, expected) in [
        (0usize, 0usize),
        (2, 2),
        (3, 2),
        (4, 2),
        (5, 3),
        (7, 3),
        (9, 5),
    ] {
        assert_eq!(
            controller.source_to_display(SourceByte::new(source_at)),
            Some(DisplayByte::new(expected)),
            "source {source_at}"
        );
    }
    // display → source：接缝取 After，末尾端点 = source_len。
    assert_eq!(
        controller.display_to_source(DisplayByte::new(2)),
        Some(SourceByte::new(4))
    );
    assert_eq!(
        controller.display_to_source(DisplayByte::new(3)),
        Some(SourceByte::new(7))
    );
    assert_eq!(
        controller.display_to_source(DisplayByte::new(5)),
        Some(SourceByte::new(9))
    );

    // 选区只碰 " c"（不碰粗体 span）→ 粗体仍隐藏，display 坐标 3..5。
    controller
        .set_selection(Selection::new(UnicodeScalar::new(7), UnicodeScalar::new(9)))
        .expect("选区合法");
    assert_eq!(controller.display_text().as_ref(), "a b c");
    assert_eq!(
        controller.selection_in_display(),
        Some((DisplayByte::new(3), DisplayByte::new(5)))
    );

    // 反向选区也返回有序区间。
    controller
        .set_selection(Selection::new(UnicodeScalar::new(9), UnicodeScalar::new(7)))
        .expect("反向选区合法");
    assert_eq!(
        controller.selection_in_display(),
        Some((DisplayByte::new(3), DisplayByte::new(5)))
    );

    // 全选 → 构造显现，display 与 source 等长。
    controller
        .set_selection(Selection::new(UnicodeScalar::new(0), UnicodeScalar::new(9)))
        .expect("全选合法");
    assert_eq!(controller.display_text().as_ref(), "a **b** c");
    assert_eq!(
        controller.selection_in_display(),
        Some((DisplayByte::new(0), DisplayByte::new(9)))
    );
}
