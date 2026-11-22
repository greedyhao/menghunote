//! ADR 0019 S3b5：app headless「表格行列拖拽重排」集成测试。
//!
//! 真实 `AppShell` + 临时 vault（`CARGO_TARGET_TMPDIR`、不 sleep）+ 真实 `#[gpui::test]`
//! 窗口：断言
//!
//! 1. 把手**只在表格悬浮/选中时**出现：列把手每列一个、行把手每条数据行一个，
//!    **表头没有行把手**；把手压在边框上，不遮挡单元格文字，也不抢单元格点击；
//! 2. 拖拽链路：按下把手（不落 caret、不选格）→ 拖动（插入指示线）→ 释放
//!    = **一次** `EditRequest`（一次 `Ctrl+Z` 回到原顺序、`Ctrl+Shift+Z` 重做）；
//! 3. 列重排：表头 / 分隔行 / 所有数据行同步，对齐标记跟着列走；行重排：整行内容
//!    按槽位移动，表头与分隔行固定；两者都只改顺序（表格总字节数不变，逐字节断言）；
//! 4. `Esc` / 拖出表格取消：**零请求**、源码逐字节不变；
//! 5. caret 全程不变（拖拽期间不移动正文 caret），表格仍是 Object（不展开源码）；
//! 6. 重开 vault 后重排结果都在，表格仍是 Object。
//!
//! 像素级外观（把手大小/颜色/手形光标、DPI、真实窗口）仍必须真机验证；这里证明
//! “几何 → 按下/拖动/释放 → 一次 `EditRequest` → 源码/undo/持久化”链路。

use std::path::Path;

use gpui::{
    div, point, px, AppContext as _, Bounds, Entity, InteractiveElement as _, Modifiers,
    MouseButton, ParentElement as _, Pixels, Point, Styled as _, TestAppContext, VisualTestContext,
};
use menghunote_app::editor::geometry::TableDragAxis;
use menghunote_app::editor::view::{EditorView, TableDragState};
use menghunote_app::shell::AppShell;
use menghunote_storage::{append_updates, load_body_doc, StructureWriter, UpdateRecord, Vault};
use tempfile::{Builder as TempDirBuilder, TempDir};

/// 窗口根视图（给 `AppShell` 一个稳定 selector）。
struct ShellHost {
    shell: Entity<AppShell>,
}

impl gpui::Render for ShellHost {
    fn render(
        &mut self,
        _window: &mut gpui::Window,
        _cx: &mut gpui::Context<Self>,
    ) -> impl gpui::IntoElement {
        div()
            .size_full()
            .debug_selector(|| "app-shell-host".to_string())
            .child(self.shell.clone())
    }
}

fn temp_vault_dir() -> TempDir {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("headless-table-drag-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("创建 target/tmp 下的测试根目录失败");
    TempDirBuilder::new()
        .prefix("vault-")
        .tempdir_in(&root)
        .expect("在 target/tmp 下创建临时目录失败")
}

/// 在空 vault 里建一篇正文（结构 CRDT → 投影 → 正文 A 事务）。
fn seed_note(vault: &Vault, title: &str, text: &str) -> String {
    let mut writer = StructureWriter::open(vault, 1).expect("打开结构写入口失败");
    let seed = writer
        .doc()
        .create_note(title, 1)
        .expect("结构 CRDT 新建笔记失败");
    writer
        .commit("headless-table-drag-seed")
        .expect("结构提交失败");

    let (doc, head) = load_body_doc(vault, &seed.body_doc_id, 1, None).expect("加载正文文档失败");
    doc.insert_scalars(0, text).expect("写入正文失败");
    let base = head.version_vector().expect("解码 head VV 失败");
    let update = doc.pending_update(&base).expect("导出正文增量失败");
    append_updates(
        vault,
        &seed.body_doc_id,
        head.head_seq,
        &[UpdateRecord::new("headless-table-drag", update)],
    )
    .expect("A 事务写入正文失败");
    seed.body_doc_id
}

fn open_shell(
    cx: &mut TestAppContext,
    vault: Vault,
) -> (Entity<ShellHost>, &mut VisualTestContext) {
    let (host, cx) = cx.add_window_view(move |_, cx| {
        let shell = cx.new(|cx| AppShell::new(vault, "headless-table-drag".to_owned(), cx));
        ShellHost { shell }
    });
    cx.run_until_parked();
    (host, cx)
}

fn close_shell(host: &Entity<ShellHost>, cx: &mut VisualTestContext) {
    cx.update(|_window, app| {
        let shell = host.read(app).shell.clone();
        shell.update(app, |shell, cx| shell.close_all(cx));
    });
    cx.run_until_parked();
}

fn active_view(host: &Entity<ShellHost>, cx: &mut VisualTestContext) -> Entity<EditorView> {
    cx.update(|_window, app| {
        let shell = host.read(app).shell.clone();
        shell
            .read(app)
            .active_editor_view()
            .expect("打开笔记后必须有活动编辑器视图")
    })
}

fn source_of(view: &Entity<EditorView>, cx: &mut VisualTestContext) -> String {
    cx.update(|_window, app| view.read(app).source_text())
}

fn display_of(view: &Entity<EditorView>, cx: &mut VisualTestContext) -> String {
    cx.update(|_window, app| view.read(app).display_text())
}

fn caret_of(view: &Entity<EditorView>, cx: &mut VisualTestContext) -> (usize, usize) {
    cx.update(|_window, app| {
        let selection = view.read(app).selection();
        (selection.anchor().get(), selection.head().get())
    })
}

fn selected_cell(
    view: &Entity<EditorView>,
    cx: &mut VisualTestContext,
) -> Option<(usize, usize, usize)> {
    cx.update(|_window, app| view.read(app).selected_table_cell())
}

fn drag_state(view: &Entity<EditorView>, cx: &mut VisualTestContext) -> Option<TableDragState> {
    cx.update(|_window, app| view.read(app).table_drag())
}

/// 当前表格对象（只有一张表；不存在就失败，避免假通过）。
fn table_of(
    view: &Entity<EditorView>,
    cx: &mut VisualTestContext,
) -> menghunote_app::editor::view::TableSnapshot {
    cx.update(|_window, app| view.read(app).table_snapshots())
        .into_iter()
        .next()
        .expect("必须解析出整块表格对象")
}

/// 表格外框（窗口坐标；必须可见）。
fn table_rect(
    line: usize,
    view: &Entity<EditorView>,
    cx: &mut VisualTestContext,
) -> Bounds<Pixels> {
    cx.update(|_window, app| {
        view.read(app)
            .object_rects()
            .into_iter()
            .find(|(object_line, _)| *object_line == line)
            .map(|(_, rect)| rect)
            .unwrap_or_else(|| panic!("对象行 {line} 必须可见"))
    })
}

/// 某个把手的绘制矩形（必须先悬浮/选中让把手出现）。
fn handle_rect(
    axis: TableDragAxis,
    index: usize,
    view: &Entity<EditorView>,
    cx: &mut VisualTestContext,
) -> Bounds<Pixels> {
    cx.update(|_window, app| {
        view.read(app)
            .table_drag_handles()
            .into_iter()
            .find(|handle| handle.axis == axis && handle.index == index)
            .map(|handle| handle.paint)
            .unwrap_or_else(|| panic!("{axis:?} 把手 #{index} 必须可见"))
    })
}

/// 悬浮到表格中心（让把手/工具条出现；不点选）。
fn hover_table(line: usize, view: &Entity<EditorView>, cx: &mut VisualTestContext) {
    let rect = table_rect(line, view, cx);
    cx.simulate_mouse_move(rect.center(), None, Modifiers::default());
    cx.run_until_parked();
}

/// 按下把手（开始拖拽）。
fn press_handle(
    axis: TableDragAxis,
    index: usize,
    view: &Entity<EditorView>,
    cx: &mut VisualTestContext,
) {
    let center = handle_rect(axis, index, view, cx).center();
    cx.simulate_mouse_down(center, MouseButton::Left, Modifiers::default());
    cx.run_until_parked();
}

/// 按住左键拖到 `target`（更新插入槽位）。
fn drag_to(target: Point<Pixels>, cx: &mut VisualTestContext) {
    cx.simulate_mouse_move(target, MouseButton::Left, Modifiers::default());
    cx.run_until_parked();
}

/// 在 `target` 释放左键。
fn release_at(target: Point<Pixels>, cx: &mut VisualTestContext) {
    cx.simulate_mouse_up(target, MouseButton::Left, Modifiers::default());
    cx.run_until_parked();
}

/// 一次完整拖拽：按下 `from` 把手 → 拖到 `target` → 释放。
fn drag_handle(
    axis: TableDragAxis,
    from: usize,
    target: Point<Pixels>,
    view: &Entity<EditorView>,
    cx: &mut VisualTestContext,
) {
    press_handle(axis, from, view, cx);
    drag_to(target, cx);
    release_at(target, cx);
}

/// 表格表头行号（0 起）。
const TABLE_LINE: usize = 2;

/// 列把手 selector（`&'static str`：`debug_bounds` 要求静态字符串）。
const COLUMN_HANDLE_SELECTORS: [&str; 3] = [
    "table-drag-column-0",
    "table-drag-column-1",
    "table-drag-column-2",
];
/// 数据行把手 selector（不含表头）。
const ROW_HANDLE_SELECTORS: [&str; 2] = ["table-drag-row-0", "table-drag-row-1"];

/// 2 条数据行 / 3 列 + 三种对齐的表格。
const COLUMNS_SOURCE: &str =
    "# 表格\n\n| 名称 | 数量 | 备注 |\n|:--|:-:|--:|\n| 苹果 | 3 | 红 |\n| 香蕉 | 5 | 黄 |\n\n后文\n";
/// 首列拖到末列之后（`名称` → 最后）。
const COLUMNS_SOURCE_AFTER_FIRST_TO_LAST: &str =
    "# 表格\n\n| 数量 | 备注 | 名称 |\n|:-:|--:|:--|\n| 3 | 红 | 苹果 |\n| 5 | 黄 | 香蕉 |\n\n后文\n";

/// 3 条数据行 / 2 列的表格。
const ROWS_SOURCE: &str =
    "# 表格\n\n| 名称 | 数量 |\n|:--|--:|\n| 苹果 | 3 |\n| 香蕉 | 5 |\n| 橙子 | 7 |\n\n后文\n";
/// 首行拖到末尾之后（`苹果` → 最后）。
const ROWS_SOURCE_AFTER_FIRST_TO_LAST: &str =
    "# 表格\n\n| 名称 | 数量 |\n|:--|--:|\n| 香蕉 | 5 |\n| 橙子 | 7 |\n| 苹果 | 3 |\n\n后文\n";

#[gpui::test]
async fn handles_show_only_when_hovered_and_never_cover_cell_text(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "把手显示", COLUMNS_SOURCE);

    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);

    // 未悬浮/未选中：没有任何把手。
    assert!(
        cx.debug_bounds("table-drag-column-0").is_none()
            && cx.debug_bounds("table-drag-row-0").is_none(),
        "表格未悬浮/未选中时不得显示拖拽把手"
    );
    assert_eq!(drag_state(&view, cx), None, "没有手势就没有拖拽状态");

    // 悬浮表格中心：列把手 3 个、数据行把手 2 个，表头没有行把手。
    hover_table(TABLE_LINE, &view, cx);
    for (column, selector) in COLUMN_HANDLE_SELECTORS.iter().enumerate() {
        assert!(
            cx.debug_bounds(selector).is_some(),
            "悬浮时必须显示第 {column} 列的列把手"
        );
    }
    for (row, selector) in ROW_HANDLE_SELECTORS.iter().enumerate() {
        assert!(
            cx.debug_bounds(selector).is_some(),
            "悬浮时必须显示第 {row} 条数据行的行把手"
        );
    }
    assert!(
        cx.debug_bounds("table-drag-row-2").is_none(),
        "只有 2 条数据行：不得凭空画出第 3 个行把手"
    );

    // 把手几何：压在边框上（不遮挡单元格文字）。
    let handles = cx.update(|_window, app| view.read(app).table_drag_handles());
    assert_eq!(handles.len(), 5, "3 个列把手 + 2 个行把手");
    let rect = table_rect(TABLE_LINE, &view, cx);
    for handle in &handles {
        match handle.axis {
            TableDragAxis::Column => {
                // 纵向中心 = 表头上边框（一半在外、一半压在表头 padding 里）。
                let center_y = handle.paint.center().y.as_f32();
                assert!(
                    (center_y - rect.origin.y.as_f32()).abs() <= 0.5,
                    "列把手必须压在表头上边框上：{handle:?}"
                );
            }
            TableDragAxis::Row => {
                // 横向中心 = 表格左边框，且只在**数据行**高度上（不含表头）。
                let center_x = handle.paint.center().x.as_f32();
                assert!(
                    (center_x - rect.origin.x.as_f32()).abs() <= 0.5,
                    "行把手必须压在表格左边框上：{handle:?}"
                );
                assert!(
                    handle.paint.center().y.as_f32() > rect.origin.y.as_f32(),
                    "行把手不得出现在表头行：{handle:?}"
                );
            }
        }
    }
    // 单元格文字从 cell 左边 + `TABLE_CELL_PAD_X` 起：行把手压进单元格的宽度必须小于它。
    let cells = cx.update(|_window, app| view.read(app).table_cell_rects());
    for handle in handles.iter().filter(|h| h.axis == TableDragAxis::Row) {
        for (object_line, row, column, cell) in &cells {
            if *object_line != TABLE_LINE || *column != 0 || *row == 0 {
                continue;
            }
            let overlap = (handle.paint.origin.x.as_f32() + handle.paint.size.width.as_f32())
                - cell.origin.x.as_f32();
            assert!(
                overlap <= menghunote_app::editor::theme::TABLE_CELL_PAD_X,
                "行把手压进单元格 {overlap}px，会盖住文字（阈值 = 单元格内边距）"
            );
        }
    }

    // 把手不抢单元格点击：点单元格中心仍是“选中该格”，caret 不动。
    let cell = cx
        .update(|_window, app| view.read(app).table_cell_rects())
        .into_iter()
        .find(|(line, row, column, _)| *line == TABLE_LINE && *row == 1 && *column == 1)
        .map(|(_, _, _, rect)| rect)
        .expect("数据行（1,1）必须可见");
    cx.simulate_click(cell.center(), Modifiers::default());
    cx.run_until_parked();
    assert_eq!(
        selected_cell(&view, cx),
        Some((TABLE_LINE, 1, 1)),
        "点单元格仍然只能选中该格（把手不得抢命中）"
    );
    assert_eq!(caret_of(&view, cx), (0, 0), "点单元格不移动正文 caret");

    close_shell(&host, cx);
}

#[gpui::test]
async fn pressing_a_handle_does_not_move_caret_or_produce_an_edit(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "按下把手", COLUMNS_SOURCE);

    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);
    hover_table(TABLE_LINE, &view, cx);

    // 原地点一下列把手：开始拖拽 → 原位释放 = 取消（零请求）。
    let center = handle_rect(TableDragAxis::Column, 0, &view, cx).center();
    cx.simulate_mouse_down(center, MouseButton::Left, Modifiers::default());
    cx.run_until_parked();
    let drag = drag_state(&view, cx).expect("按下列把手必须进入拖拽状态");
    assert_eq!(
        (drag.line, drag.axis, drag.from),
        (TABLE_LINE, TableDragAxis::Column, 0)
    );
    assert_eq!(drag.slot, None, "还没移动：没有插入槽位");
    assert_eq!(caret_of(&view, cx), (0, 0), "按把手不移动正文 caret");
    assert_eq!(selected_cell(&view, cx), None, "按把手不选中单元格");

    release_at(center, cx);
    assert_eq!(drag_state(&view, cx), None, "释放后拖拽状态清空");
    assert_eq!(
        source_of(&view, cx),
        COLUMNS_SOURCE,
        "原位释放不产生任何编辑"
    );
    assert_eq!(caret_of(&view, cx), (0, 0));
    assert_eq!(table_of(&view, cx).columns, 3);

    close_shell(&host, cx);
}

#[gpui::test]
async fn drag_first_column_to_last_is_byte_exact_and_one_undo(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "拖列重排", COLUMNS_SOURCE);

    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);
    hover_table(TABLE_LINE, &view, cx);
    let placeholder = menghunote_editor::OBJECT_PLACEHOLDER;

    // 拖第 0 列把手到最后一列的中心（右半 → 槽位 = columns）。
    press_handle(TableDragAxis::Column, 0, &view, cx);
    // 落点取末列把手**中心偏右**（右半格 → 槽位 = 列数；正中点恰好压在半格边界上，
    // 像素四舍五入会让“半格”判定落到左侧，测试不该依赖这种边界）。
    let last = handle_rect(TableDragAxis::Column, 2, &view, cx);
    let drop = point(last.center().x + px(6.0), last.center().y);
    drag_to(drop, cx);
    let drag = drag_state(&view, cx).expect("拖动中必须有拖拽状态");
    assert_eq!(drag.slot, Some(3), "指针在末列右半：槽位 = 列数");
    assert_eq!(drag.target, Some(2), "最终下标 = 末列");
    assert_eq!(caret_of(&view, cx), (0, 0), "拖拽期间 caret 一动不动");
    assert!(
        cx.debug_bounds("table-drop-indicator").is_some(),
        "拖动到有效槽位必须画插入指示线"
    );

    release_at(drop, cx);
    assert_eq!(
        source_of(&view, cx),
        COLUMNS_SOURCE_AFTER_FIRST_TO_LAST,
        "列重排必须同时改表头/分隔行/所有数据行，其它字节逐字保留"
    );
    let table = table_of(&view, cx);
    assert_eq!((table.rows, table.columns), (2, 3), "行/列数不变");
    assert!(table.selected, "拖完之后表格仍是当前选中对象");
    assert!(
        !table.editing_source && table.display.is_some(),
        "拖完表格仍必须是 Object（不得进入源码模式）"
    );
    let display = display_of(&view, cx);
    assert!(
        display.contains(placeholder) && !display.contains("| 数量 |"),
        "拖完仍投影成对象占位符：{display:?}"
    );
    assert_eq!(
        source_of(&view, cx).len(),
        COLUMNS_SOURCE.len(),
        "重排是纯顺序变化：表格总字节数不变"
    );
    assert_eq!(caret_of(&view, cx), (0, 0), "释放也不移动正文 caret");
    assert!(
        cx.debug_bounds("table-drop-indicator").is_none(),
        "释放后插入指示线必须消失"
    );

    // 一次拖拽释放 = 一个撤销组：一次 Ctrl+Z 回到原顺序，Ctrl+Shift+Z 重做。
    cx.simulate_keystrokes("ctrl-z");
    cx.run_until_parked();
    assert_eq!(source_of(&view, cx), COLUMNS_SOURCE, "一次撤销恢复原列顺序");
    cx.simulate_keystrokes("ctrl-shift-z");
    cx.run_until_parked();
    assert_eq!(
        source_of(&view, cx),
        COLUMNS_SOURCE_AFTER_FIRST_TO_LAST,
        "重做恢复同一次重排"
    );

    close_shell(&host, cx);
}

#[gpui::test]
async fn drag_first_data_row_to_last_keeps_header_and_is_byte_exact(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "拖行重排", ROWS_SOURCE);

    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);
    hover_table(TABLE_LINE, &view, cx);

    press_handle(TableDragAxis::Row, 0, &view, cx);
    // 拖到**最后一条数据行**的下半部分 → 槽位 = row_count。
    let last_row = handle_rect(TableDragAxis::Row, 2, &view, cx);
    let drop = point(last_row.center().x, last_row.center().y + px(3.0));
    drag_to(drop, cx);
    let drag = drag_state(&view, cx).expect("拖动中必须有拖拽状态");
    assert_eq!((drag.axis, drag.from), (TableDragAxis::Row, 0));
    assert_eq!(drag.slot, Some(3), "最后一行下半：槽位 = 数据行数");
    assert_eq!(drag.target, Some(2), "最终下标 = 末行");

    release_at(drop, cx);
    assert_eq!(
        source_of(&view, cx),
        ROWS_SOURCE_AFTER_FIRST_TO_LAST,
        "行重排必须整行（含行尾换行）按槽位移动，表头/分隔行/前后正文逐字节不变"
    );
    let table = table_of(&view, cx);
    assert_eq!((table.line, table.rows, table.columns), (TABLE_LINE, 3, 2));
    assert_eq!(table.end_line, TABLE_LINE + 4, "表头/分隔行 + 3 条数据行");
    assert!(table.display.is_some() && !table.editing_source);
    assert_eq!(caret_of(&view, cx), (0, 0));

    cx.simulate_keystrokes("ctrl-z");
    cx.run_until_parked();
    assert_eq!(source_of(&view, cx), ROWS_SOURCE, "一次撤销恢复原行顺序");

    // 末行 → 首行（落点取第一行把手的上半部分 → 槽位 0）。
    let first = handle_rect(TableDragAxis::Row, 0, &view, cx);
    let drop = point(first.center().x, first.center().y - px(3.0));
    drag_handle(TableDragAxis::Row, 2, drop, &view, cx);
    assert_eq!(
        source_of(&view, cx),
        "# 表格\n\n| 名称 | 数量 |\n|:--|--:|\n| 橙子 | 7 |\n| 苹果 | 3 |\n| 香蕉 | 5 |\n\n后文\n",
        "末行拖到最前：表头/分隔行固定，其它行顺序顺势后移"
    );

    close_shell(&host, cx);
}

#[gpui::test]
async fn dragging_out_of_the_table_or_pressing_escape_cancels_without_requests(
    cx: &mut TestAppContext,
) {
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "取消拖拽", ROWS_SOURCE);

    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);
    hover_table(TABLE_LINE, &view, cx);

    // 1) 拖出表格（远远离开表格 + 超过取消边距）→ 槽位 None，释放即取消。
    press_handle(TableDragAxis::Row, 0, &view, cx);
    let rect = table_rect(TABLE_LINE, &view, cx);
    let outside = point(rect.center().x, rect.origin.y - px(400.0));
    drag_to(outside, cx);
    assert_eq!(
        drag_state(&view, cx).and_then(|drag| drag.slot),
        None,
        "拖出表格后没有插入槽位"
    );
    assert!(
        cx.debug_bounds("table-drop-indicator").is_none(),
        "拖出表格后指示线必须消失"
    );
    release_at(outside, cx);
    assert_eq!(drag_state(&view, cx), None);
    assert_eq!(
        source_of(&view, cx),
        ROWS_SOURCE,
        "拖出表格释放必须取消（逐字节不变）"
    );

    // 2) 拖到有效槽位后按 Esc：取消，且随后的释放不再产生请求。
    press_handle(TableDragAxis::Row, 0, &view, cx);
    let last_row = handle_rect(TableDragAxis::Row, 2, &view, cx).center();
    drag_to(last_row, cx);
    assert!(drag_state(&view, cx).and_then(|drag| drag.target).is_some());
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert_eq!(drag_state(&view, cx), None, "Esc 必须立即结束拖拽");
    release_at(last_row, cx);
    assert_eq!(
        source_of(&view, cx),
        ROWS_SOURCE,
        "Esc 取消后释放不得再产生编辑"
    );
    assert_eq!(caret_of(&view, cx), (0, 0), "取消不移动正文 caret");

    close_shell(&host, cx);
}

#[gpui::test]
async fn reordered_rows_and_columns_survive_reopen(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault_path = dir.path().join("vault.sqlite3");
    let vault = Vault::open(&vault_path).expect("打开临时 vault");
    let doc_id = seed_note(&vault, "持久化重排", COLUMNS_SOURCE);

    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);
    hover_table(TABLE_LINE, &view, cx);
    // 先把首列拖到末列（落点在末列右半 → 槽位 = 列数）。
    let last = handle_rect(TableDragAxis::Column, 2, &view, cx);
    let drop = point(last.center().x + px(6.0), last.center().y);
    drag_handle(TableDragAxis::Column, 0, drop, &view, cx);
    assert_eq!(source_of(&view, cx), COLUMNS_SOURCE_AFTER_FIRST_TO_LAST);
    close_shell(&host, cx);

    // 1) durable 后正文逐字正确。
    let persisted = {
        let vault = Vault::open(&vault_path).expect("重开 vault");
        let (doc, _head) = load_body_doc(&vault, &doc_id, 1, None).expect("重放正文失败");
        doc.text().to_owned()
    };
    assert_eq!(
        persisted, COLUMNS_SOURCE_AFTER_FIRST_TO_LAST,
        "落库后必须保留重排结果"
    );

    // 2) 重新打开 shell：表格仍是对象，列顺序来自新 source。
    let vault = Vault::open(&vault_path).expect("再次打开 vault");
    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);
    let table = table_of(&view, cx);
    assert_eq!(
        (table.line, table.rows, table.columns),
        (TABLE_LINE, 2, 3),
        "重开后行列数与源码一致"
    );
    assert!(table.display.is_some(), "重开后表格仍是 Object");
    let display = display_of(&view, cx);
    assert!(
        display.contains(menghunote_editor::OBJECT_PLACEHOLDER) && !display.contains("| 数量 |"),
        "重开后不得直接把表格源码显示出来：{display:?}"
    );
    // 重开后再拖回来（末列 → 首列）：落点取首列**左半**（槽位 0），逐字节回到原文。
    hover_table(TABLE_LINE, &view, cx);
    let rect = table_rect(TABLE_LINE, &view, cx);
    let first = handle_rect(TableDragAxis::Column, 0, &view, cx);
    let drop = point(rect.origin.x + px(4.0), first.center().y);
    drag_handle(TableDragAxis::Column, 2, drop, &view, cx);
    assert_eq!(
        source_of(&view, cx),
        COLUMNS_SOURCE,
        "反向拖回必须逐字节恢复原顺序"
    );
    close_shell(&host, cx);
}

#[gpui::test]
async fn selected_cell_follows_the_dragged_content(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "选中格跟随", COLUMNS_SOURCE);

    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);

    // 先选中「香蕉 / 数量」这一格（row 1 = 第一条数据行，col 1 = 数量）。
    hover_table(TABLE_LINE, &view, cx);
    let cell = cx
        .update(|_window, app| view.read(app).table_cell_rects())
        .into_iter()
        .find(|(line, row, column, _)| *line == TABLE_LINE && *row == 1 && *column == 1)
        .map(|(_, _, _, rect)| rect)
        .expect("数据行（1,1）必须可见");
    cx.simulate_click(cell.center(), Modifiers::default());
    cx.run_until_parked();
    assert_eq!(selected_cell(&view, cx), Some((TABLE_LINE, 1, 1)));

    // 首列拖到末列：被让位的列整体左移一格 → 选中的「数量」落到第 0 列。
    let last = handle_rect(TableDragAxis::Column, 2, &view, cx);
    let drop = point(last.center().x + px(6.0), last.center().y);
    drag_handle(TableDragAxis::Column, 0, drop, &view, cx);
    assert_eq!(
        source_of(&view, cx),
        COLUMNS_SOURCE_AFTER_FIRST_TO_LAST,
        "列重排逐字节正确"
    );
    assert_eq!(
        selected_cell(&view, cx),
        Some((TABLE_LINE, 1, 0)),
        "选中的内容跟着列走（列 1 → 0），不留一个指向别的内容的选中格"
    );

    // 首条数据行拖到末行（本表 2 条数据行：行 0 → 行 1，人话行号 1 → 2）。
    let last_row = handle_rect(TableDragAxis::Row, 1, &view, cx);
    let drop = point(last_row.center().x, last_row.center().y + px(3.0));
    drag_handle(TableDragAxis::Row, 0, drop, &view, cx);
    assert_eq!(
        selected_cell(&view, cx),
        Some((TABLE_LINE, 2, 0)),
        "选中的内容跟着行走（首行 → 末行）"
    );
    assert_eq!(caret_of(&view, cx), (0, 0), "两次拖拽都不移动正文 caret");
    let table = table_of(&view, cx);
    assert!(
        table.display.is_some() && !table.editing_source,
        "表格仍是 Object"
    );

    close_shell(&host, cx);
}

#[gpui::test]
async fn handles_are_hidden_while_a_cell_is_being_edited(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "编辑时无把手", COLUMNS_SOURCE);

    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);
    hover_table(TABLE_LINE, &view, cx);
    assert!(
        cx.debug_bounds("table-drag-column-0").is_some(),
        "悬浮时应该有列把手"
    );

    // 进入单元格就地编辑：整表被压成一行 cell 文本，把手必须一起消失。
    let cell = cx
        .update(|_window, app| view.read(app).table_cell_rects())
        .into_iter()
        .find(|(line, row, column, _)| *line == TABLE_LINE && *row == 1 && *column == 0)
        .map(|(_, _, _, rect)| rect)
        .expect("数据行（1,0）必须可见");
    cx.simulate_click(cell.center(), Modifiers::default());
    cx.run_until_parked();
    cx.simulate_keystrokes("f2");
    cx.run_until_parked();
    assert!(
        cx.update(|_window, app| view.read(app).is_editing_table_cell()),
        "F2 必须进入单元格编辑"
    );
    assert_eq!(
        cx.update(|_window, app| view.read(app).table_drag_handles())
            .len(),
        0,
        "单元格编辑期间没有拖拽把手（表格已不是 Object）"
    );
    assert!(
        cx.debug_bounds("table-drag-column-0").is_none()
            && cx.debug_bounds("table-drag-row-0").is_none(),
        "单元格编辑期间不得画出任何把手"
    );
    // 直接调入口也必须被拒（陈旧帧/测试路径都不能绕过闸门）。
    assert!(
        !cx.update(|_window, app| {
            view.update(app, |view, cx| {
                view.begin_table_drag(TABLE_LINE, TableDragAxis::Column, 0, cx)
            })
        }),
        "单元格编辑期间不得开始拖拽"
    );

    // Esc 退出单元格编辑后把手回来。
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    hover_table(TABLE_LINE, &view, cx);
    assert!(
        cx.debug_bounds("table-drag-column-0").is_some(),
        "退出单元格编辑后列把手必须回来"
    );

    close_shell(&host, cx);
}
