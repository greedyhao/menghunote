//! ADR 0019 S3：任务列表复选框的**纯模型**契约。
//!
//! 覆盖：
//! 1. 任务框 source 区间来自 `menghunote-markdown`，投影把它换成不可见占位
//!    （`TASK_BOX_DISPLAY`），caret/选区进入任务框或 marker 时整段显现源码；
//! 2. `ProjectionMap::replace_display_range` 给出占位的 display 区间（app 自绘/命中用）；
//! 3. `EditorController::toggle_task_marker`：一次点击一个 `EditRequest`（单 ScalarEdit）、
//!    只改 3 个字节、caret 不变、勾选统一小写 `x`、非任务项 no-op。

mod support;

use menghunote_editor::{
    DisplayByte, EditorController, MarkdownDocument, ProjectionMap, Selection, SourceByte,
    SourceRange, UnicodeScalar, TASK_BOX_DISPLAY,
};
use menghunote_storage::session::EditRequest;

fn parse(text: &str) -> MarkdownDocument {
    MarkdownDocument::parse(text).expect("Markdown 解析不应失败")
}

fn project(doc: &MarkdownDocument, caret: usize) -> ProjectionMap {
    ProjectionMap::from_markdown_with_lists(doc, Selection::caret(UnicodeScalar::new(caret)))
}

fn caret_at_end(source: &str) -> usize {
    source.chars().count()
}

fn box_source(doc: &MarkdownDocument, index: usize) -> SourceRange {
    let item = &doc.list_items()[index];
    let span = item.task_marker.expect("任务项必须有 task_marker");
    SourceRange::new(
        SourceByte::new(span.start),
        SourceByte::new(span.end),
        doc.source().len(),
    )
    .expect("任务框区间必须合法")
}

fn open(source: &str) -> EditorController {
    EditorController::open(support::snapshot(source)).expect("打开测试快照失败")
}

/// 消费本地回声（单飞行：下一个 `EditRequest` 前必须确认掉 `pending`）。
fn confirm(controller: &mut EditorController, request: &EditRequest, live_rev: u64) {
    controller
        .apply_outcome(support::applied(
            &request.command_id,
            live_rev,
            true,
            support::splice(0, 0, ""),
        ))
        .expect("本地回声必须被接受");
}

fn set_caret(controller: &mut EditorController, scalar: usize) {
    controller
        .set_caret(UnicodeScalar::new(scalar))
        .expect("caret 必须合法");
}

#[test]
fn task_box_is_replaced_with_an_invisible_placeholder() {
    let source = "- [ ] todo\n- [x] done\n";
    let doc = parse(source);
    let map = project(&doc, caret_at_end(source));
    assert_eq!(
        map.display_text().as_ref(),
        format!("• {TASK_BOX_DISPLAY} todo\n• {TASK_BOX_DISPLAY} done\n"),
        "隐藏态下任务框应换成不可见占位（不是源码 `[ ]`）"
    );

    // 占位是一个 Replace 段：source 正好等于任务框 3 字节。
    let first = box_source(&doc, 0);
    let second = box_source(&doc, 1);
    let range = map
        .replace_display_range(first)
        .expect("隐藏态下任务框必须是被 Replace 的段");
    assert_eq!(range.end - range.start, TASK_BOX_DISPLAY.len());
    assert!(map.replace_display_range(second).is_some());

    // display 上该区间的内容就是占位文本，不是 bullet/编号。
    let display = map.display_text();
    assert_eq!(&display.as_ref()[range.clone()], TASK_BOX_DISPLAY);

    // source → display：任务框内部收敛到占位起点；框尾正好是占位终点。
    assert_eq!(
        map.display_byte_at_source(SourceByte::new(first.start().get())),
        Some(DisplayByte::new(range.start))
    );
    assert_eq!(
        map.display_byte_at_source(SourceByte::new(first.start().get() + 1)),
        Some(DisplayByte::new(range.start))
    );
    assert_eq!(
        map.display_byte_at_source(SourceByte::new(first.end().get())),
        Some(DisplayByte::new(range.end))
    );

    // 反查：display 选中占位 → source 完整包含任务框（复制/删除都不得只拿半个框）。
    assert_eq!(
        map.source_range_of_display_range(
            DisplayByte::new(range.start)..DisplayByte::new(range.end)
        ),
        Some(first)
    );
}

#[test]
fn caret_or_selection_inside_the_task_box_reveals_marker_and_box() {
    let source = "- [ ] todo\n";
    let doc = parse(source);
    // scalar 3 是任务框内部的空格：0 '-', 1 ' ', 2 '[', 3 ' ', 4 ']'。
    let revealed = project(&doc, 3);
    assert_eq!(
        revealed.display_text().as_ref(),
        source,
        "caret 进入任务框必须显示源码 `- [ ]`，不再画占位"
    );
    assert!(
        revealed
            .replace_display_range(box_source(&doc, 0))
            .is_none(),
        "显现源码时没有 Replace 段（app 不画复选框）"
    );

    // 非空选区与任务框相交同样显现。
    let selection = Selection::new(UnicodeScalar::new(2), UnicodeScalar::new(4));
    let selected = ProjectionMap::from_markdown_with_lists(&doc, selection);
    assert_eq!(selected.display_text().as_ref(), source);
    assert!(selected
        .replace_display_range(box_source(&doc, 0))
        .is_none());

    // caret 在 `]` 之后（框外）不显现：仍显示勾选占位。
    let outside = project(&doc, 5);
    assert!(
        outside.replace_display_range(box_source(&doc, 0)).is_some(),
        "caret 在任务框之后不应显现源码"
    );
}

#[test]
fn toggle_writes_only_the_three_box_bytes_and_keeps_the_caret() {
    let source = "# 标题\n\n- [ ] parent\n  - [x] child\n\n1. [ ] ordered\n\n普通段落\n";
    let mut controller = open(source);
    // caret 放在最后一段：切换前面的任务框不得把正文 caret 挪走。
    let caret = source.chars().count() - 1;
    set_caret(&mut controller, caret);
    let before = controller.selection();

    let request = controller
        .toggle_task_marker(0)
        .expect("切换不应失败")
        .expect("应产生一个 EditRequest");
    assert_eq!(request.edits.len(), 1, "一次点击只能有一个 ScalarEdit");
    assert_eq!(controller.selection(), before, "点击复选框不能落正文 caret");
    let expected = source.replacen("- [ ] parent", "- [x] parent", 1);
    assert_eq!(controller.source_text(), expected, "只改任务框 3 个字节");
    assert_eq!(controller.list_items()[0].task, Some(true));
    confirm(&mut controller, &request, 8);

    // 再点一次：`[x]` → `[ ]`，其它字节仍旧逐字不变。
    let request = controller
        .toggle_task_marker(0)
        .expect("第二次切换不应失败")
        .expect("应产生请求");
    assert_eq!(request.edits.len(), 1);
    assert_eq!(controller.source_text(), source);
    assert_eq!(controller.selection(), before);
    assert_eq!(controller.list_items()[0].task, Some(false));
    confirm(&mut controller, &request, 9);
}

#[test]
fn uppercase_and_nested_and_ordered_boxes_toggle_independently() {
    let source = "- [ ] parent\n  - [X] child\n\n1. [ ] ordered\n";
    let mut controller = open(source);
    set_caret(&mut controller, caret_at_end(source));

    // `[X]` 视为已勾选 → 写 `[ ]`；再点一次写小写 `[x]`（固定选择：不保留大写）。
    let first = controller
        .toggle_task_marker(1)
        .expect("切换嵌套项失败")
        .expect("应产生请求");
    assert_eq!(
        controller.source_text(),
        "- [ ] parent\n  - [ ] child\n\n1. [ ] ordered\n"
    );
    confirm(&mut controller, &first, 8);
    let second = controller
        .toggle_task_marker(1)
        .expect("切换嵌套项失败")
        .expect("应产生请求");
    assert_eq!(
        controller.source_text(),
        "- [ ] parent\n  - [x] child\n\n1. [ ] ordered\n",
        "勾选统一写小写 x"
    );
    confirm(&mut controller, &second, 9);

    // 有序 item 独立切换，父/子 item 不受影响。
    let third = controller
        .toggle_task_marker(2)
        .expect("切换有序项失败")
        .expect("应产生请求");
    assert_eq!(
        controller.source_text(),
        "- [ ] parent\n  - [x] child\n\n1. [x] ordered\n"
    );
    confirm(&mut controller, &third, 10);
}

#[test]
fn non_task_items_are_a_no_op() {
    let source = "- plain\n\n普通段落\n";
    let mut controller = open(source);
    assert!(controller
        .toggle_task_marker(0)
        .expect("非任务项不应报错")
        .is_none());
    assert!(controller
        .toggle_task_marker(99)
        .expect("越界不应报错")
        .is_none());
    assert_eq!(controller.source_text(), source, "no-op 不得改任何字节");
}

#[test]
fn toggled_box_stays_a_hidden_placeholder_when_caret_is_elsewhere() {
    let source = "- [ ] todo\n";
    let mut controller = open(source);
    set_caret(&mut controller, 0);
    let request = controller
        .toggle_task_marker(0)
        .expect("切换失败")
        .expect("应产生请求");
    confirm(&mut controller, &request, 8);
    // caret 在 marker 里（scalar 0）→ 源码显现；移到框外后恢复占位。
    assert_eq!(controller.display_text().as_ref(), "- [x] todo\n");
    set_caret(&mut controller, 8);
    assert_eq!(
        controller.display_text().as_ref(),
        format!("• {TASK_BOX_DISPLAY} todo\n")
    );
    assert_eq!(controller.list_items()[0].task, Some(true));
}
