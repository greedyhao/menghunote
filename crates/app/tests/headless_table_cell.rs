//! ADR 0019 S3b2（2026-11-17 交互修正）：app headless「表格单元格原地编辑」集成测试。
//!
//! 真实 `AppShell` + 临时 vault（`CARGO_TARGET_TMPDIR`、不 sleep）+ 真实 `#[gpui::test]`
//! 窗口：断言
//!
//! 1. **单击**单元格直接进入原地编辑：表格仍是 Object（display 仍是占位符、其余行列
//!    仍在本帧绘制几何里），输入框矩形就是被点 cell 的矩形（不遮挡整表）；
//! 2. 编辑期间正文源码不变（草稿只在输入框里），输入/退格/粘贴/IME 都只改草稿；
//! 3. `Enter` 提交 = **一条** `EditRequest`（一次 `Ctrl+Z` 回到原表）；`Esc` 取消
//!    （零请求、源码逐字节不变）；点击浮层外提交；`Tab` 提交并移到下一格；
//! 4. 空 cell 可提交空串；未转义 `|` 写成 `\|`；换行/制表符变空格；
//! 5. 加行/加列后的新 cell 可编辑；持久化后重开，cell 内的字仍在、表格仍是 Object。
//!
//! 像素级外观（输入框位置/DPI/真实窗口 IME）仍必须真机验证；这里证明
//! “几何 → 单击 → 浮层输入 → 一个 `EditRequest` → 源码/undo/持久化”链路，
//! 以及编辑态**没有把整表投影成一行 cell 文本**。

use std::path::Path;

use gpui::{
    div, point, px, AppContext as _, Entity, InteractiveElement as _, Modifiers, MouseButton,
    MouseDownEvent, MouseUpEvent, ParentElement as _, Styled as _, TestAppContext,
    VisualTestContext,
};
use menghunote_app::editor::view::EditorView;
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
        .join(format!("headless-table-cell-{}", std::process::id()));
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
        .commit("headless-table-cell-seed")
        .expect("结构提交失败");

    let (doc, head) = load_body_doc(vault, &seed.body_doc_id, 1, None).expect("加载正文文档失败");
    doc.insert_scalars(0, text).expect("写入正文失败");
    let base = head.version_vector().expect("解码 head VV 失败");
    let update = doc.pending_update(&base).expect("导出正文增量失败");
    append_updates(
        vault,
        &seed.body_doc_id,
        head.head_seq,
        &[UpdateRecord::new("headless-table-cell", update)],
    )
    .expect("A 事务写入正文失败");
    seed.body_doc_id
}

fn open_shell(
    cx: &mut TestAppContext,
    vault: Vault,
) -> (Entity<ShellHost>, &mut VisualTestContext) {
    let (host, cx) = cx.add_window_view(move |_, cx| {
        let shell = cx.new(|cx| AppShell::new(vault, "headless-table-cell".to_owned(), cx));
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

fn display_of(view: &Entity<EditorView>, cx: &mut VisualTestContext) -> String {
    cx.update(|_window, app| view.read(app).display_text())
}

fn source_of(view: &Entity<EditorView>, cx: &mut VisualTestContext) -> String {
    cx.update(|_window, app| view.read(app).source_text())
}

fn caret_of(view: &Entity<EditorView>, cx: &mut VisualTestContext) -> (usize, usize) {
    cx.update(|_window, app| {
        let selection = view.read(app).selection();
        (selection.anchor().get(), selection.head().get())
    })
}

fn is_editing_cell(view: &Entity<EditorView>, cx: &mut VisualTestContext) -> bool {
    cx.update(|_window, app| view.read(app).is_editing_table_cell())
}

fn cell_target(
    view: &Entity<EditorView>,
    cx: &mut VisualTestContext,
) -> Option<(usize, usize, usize)> {
    cx.update(|_window, app| view.read(app).table_cell_edit_target())
}

fn selected_cell(
    view: &Entity<EditorView>,
    cx: &mut VisualTestContext,
) -> Option<(usize, usize, usize)> {
    cx.update(|_window, app| view.read(app).selected_table_cell())
}

/// 单个可见表格快照（不存在就失败，避免假通过）。
fn table_of(
    view: &Entity<EditorView>,
    cx: &mut VisualTestContext,
) -> menghunote_app::editor::view::TableSnapshot {
    let snapshots = cx.update(|_window, app| view.read(app).table_snapshots());
    snapshots
        .into_iter()
        .next()
        .expect("必须解析出整块表格对象")
}

/// 第 `row` 行（0 = 表头）第 `column` 列单元格的窗口矩形（不可见直接失败）。
fn cell_rect(
    line: usize,
    row: usize,
    column: usize,
    view: &Entity<EditorView>,
    cx: &mut VisualTestContext,
) -> gpui::Bounds<gpui::Pixels> {
    cx.update(|_window, app| {
        view.read(app)
            .table_cell_rects()
            .into_iter()
            .find(|(object_line, r, c, _)| *object_line == line && *r == row && *c == column)
            .map(|(_, _, _, rect)| rect)
            .unwrap_or_else(|| panic!("单元格（行 {line} 的 {row},{column}）必须可见"))
    })
}

/// 点击一个单元格（单击 = 选中并**直接进入原地编辑**）。
fn click_cell(
    line: usize,
    row: usize,
    column: usize,
    view: &Entity<EditorView>,
    cx: &mut VisualTestContext,
) {
    let rect = cell_rect(line, row, column, view, cx);
    cx.simulate_click(rect.center(), Modifiers::default());
    cx.run_until_parked();
}

/// 双击一个单元格（一次手势里 `click_count = 2`）：与单击同义。
fn double_click_cell(
    line: usize,
    row: usize,
    column: usize,
    view: &Entity<EditorView>,
    cx: &mut VisualTestContext,
) {
    let rect = cell_rect(line, row, column, view, cx);
    let position = rect.center();
    cx.simulate_event(MouseDownEvent {
        position,
        modifiers: Modifiers::default(),
        button: MouseButton::Left,
        click_count: 2,
        first_mouse: false,
    });
    cx.simulate_event(MouseUpEvent {
        position,
        modifiers: Modifiers::default(),
        button: MouseButton::Left,
        click_count: 2,
    });
    cx.run_until_parked();
}

/// 点击一个 debug selector 目标（找不到就失败：按钮必须真实画出来）。
fn click_selector(cx: &mut VisualTestContext, selector: &'static str) {
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("`{selector}` 必须可见"));
    cx.simulate_click(bounds.center(), Modifiers::default());
    cx.run_until_parked();
}

/// 两矩形逐像素相等（容差 0.5px，避免布局取整抖动）。
fn rect_eq(a: gpui::Bounds<gpui::Pixels>, b: gpui::Bounds<gpui::Pixels>) -> bool {
    (a.origin.x.as_f32() - b.origin.x.as_f32()).abs() <= 0.5
        && (a.origin.y.as_f32() - b.origin.y.as_f32()).abs() <= 0.5
        && (a.size.width.as_f32() - b.size.width.as_f32()).abs() <= 0.5
        && (a.size.height.as_f32() - b.size.height.as_f32()).abs() <= 0.5
}

/// 表格源码末尾的 scalar（`TABLE_SOURCE` 里表格之后是空行 + `后文`）。
fn table_end_scalar(source: &str) -> usize {
    let byte = source.find("\n\n后文").expect("后文位置");
    source[..byte].chars().count()
}

/// 2 列 / 1 条数据行 + 两种对齐的表格。
const TABLE_SOURCE: &str = "# 表格\n\n| 名称 | 数量 |\n|:--|--:|\n| 苹果 | 3 |\n\n后文\n";
/// 表格表头行号（0 起）。
const TABLE_LINE: usize = 2;

#[gpui::test]
async fn single_click_edits_in_place_and_keeps_the_whole_table_visible(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "单击原地编辑", TABLE_SOURCE);

    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);
    let placeholder = menghunote_editor::OBJECT_PLACEHOLDER;
    assert_eq!(caret_of(&view, cx), (0, 0), "打开笔记时 caret 在文档开头");

    let cell = cell_rect(TABLE_LINE, 1, 0, &view, cx);
    cx.simulate_click(cell.center(), Modifiers::default());
    cx.run_until_parked();

    // 单击 = 直接进入原地编辑（不需要先选中、不需要 F2）。
    assert!(is_editing_cell(&view, cx), "单击必须直接进入单元格编辑");
    let focused =
        cx.update(|window, app| view.read(app).table_cell_edit_input_focused(window, app));
    assert!(focused, "进入编辑后键盘焦点必须在单元格输入框上");
    assert_eq!(cell_target(&view, cx), Some((TABLE_LINE, 1, 0)));
    assert_eq!(selected_cell(&view, cx), Some((TABLE_LINE, 1, 0)));
    assert_eq!(caret_of(&view, cx), (0, 0), "编辑单元格不移动正文 caret");

    // 表格仍然是完整 Object：display 是占位符，源码/`|` 都没有展开。
    let table = table_of(&view, cx);
    assert_eq!(table.editing_cell, Some((1, 0)));
    assert!(table.display.is_some(), "编辑期间表格必须仍是 Object");
    assert!(!table.editing_source, "单元格编辑不是整表源码模式");
    let display = display_of(&view, cx);
    assert!(
        display.contains(placeholder) && !display.contains('|') && !display.contains("苹果"),
        "编辑态 display 仍只是一个表格占位符（不能塌缩成 cell 文本）：{display:?}"
    );

    // 整表仍在绘制（表头 + 1 条数据行），其它 cell 的矩形都在。
    let painted = cx.update(|_window, app| view.read(app).table_paint_rows());
    assert_eq!(
        painted,
        vec![(TABLE_LINE, true, 1)],
        "编辑期间必须继续画整表"
    );
    let rects = cx.update(|_window, app| view.read(app).table_cell_rects());
    assert_eq!(rects.len(), 4, "2 列 × 2 行 = 4 个 cell 矩形：{rects:?}");

    // 输入框只覆盖被点的那一格（不是整表、不是整行）。
    let input = cx
        .update(|_window, app| view.read(app).table_cell_edit_input_bounds())
        .expect("编辑期间必须有输入框矩形");
    assert!(
        rect_eq(input, cell),
        "输入框必须正好落在被点 cell 的矩形上：input={input:?} cell={cell:?}"
    );
    assert!(
        cx.debug_bounds("table-cell-editor").is_some(),
        "输入框必须真实画出来（debug selector）"
    );
    assert!(
        cx.debug_bounds("object-toolbar").is_none(),
        "原地编辑期间悬浮工具条必须隐藏（避免遮住输入框）"
    );
    let table_rect = cx
        .update(|_window, app| view.read(app).object_rects())
        .into_iter()
        .find(|(line, _)| *line == TABLE_LINE)
        .map(|(_, rect)| rect)
        .expect("表格对象矩形");
    assert!(
        (input.size.width.as_f32() < table_rect.size.width.as_f32() - 0.5)
            || (input.size.height.as_f32() < table_rect.size.height.as_f32() - 0.5),
        "输入框不能覆盖整张表：input={input:?} table={table_rect:?}"
    );

    // 进入编辑不写正文、不产生请求。
    assert_eq!(source_of(&view, cx), TABLE_SOURCE, "进入编辑不得改源码");
    assert_eq!(caret_of(&view, cx), (0, 0));
    close_shell(&host, cx);
}

#[gpui::test]
async fn typing_then_enter_commits_one_edit_request_and_one_undo_group(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "单元格输入", TABLE_SOURCE);

    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);
    click_cell(TABLE_LINE, 1, 0, &view, cx);
    assert!(is_editing_cell(&view, cx));

    // 打字只进浮层草稿：正文源码直到提交前逐字节不变。
    cx.simulate_input("派");
    cx.run_until_parked();
    assert_eq!(
        source_of(&view, cx),
        TABLE_SOURCE,
        "编辑期间草稿不进正文（提交时才写一次）"
    );
    // 表格其余行列仍然可见（编辑没有把整表压成一行文本）。
    assert!(table_of(&view, cx).display.is_some());
    assert_eq!(
        cx.update(|_window, app| view.read(app).table_paint_rows()),
        vec![(TABLE_LINE, true, 1)]
    );

    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert!(!is_editing_cell(&view, cx), "Enter 提交并退出编辑");
    assert_eq!(
        source_of(&view, cx),
        "# 表格\n\n| 名称 | 数量 |\n|:--|--:|\n| 苹果派 | 3 |\n\n后文\n",
        "只改该 cell 的源码区间；两侧 `|`/对齐/其它 cell/行尾换行逐字不变"
    );
    let table = table_of(&view, cx);
    assert_eq!(table.editing_cell, None);
    assert!(table.display.is_some(), "提交后表格仍是 Object");
    let display = display_of(&view, cx);
    assert!(
        display.contains(menghunote_editor::OBJECT_PLACEHOLDER) && !display.contains("苹果派"),
        "提交后 display 回到占位符：{display:?}"
    );
    assert_eq!(
        caret_of(&view, cx),
        (
            table_end_scalar(&source_of(&view, cx)),
            table_end_scalar(&source_of(&view, cx))
        ),
        "提交后 caret 落在整表末尾（不落进表格源码内部）"
    );

    // 一次提交 = 一个撤销组。
    cx.simulate_keystrokes("ctrl-z");
    cx.run_until_parked();
    assert_eq!(source_of(&view, cx), TABLE_SOURCE, "一次撤销回到原文");
    cx.simulate_keystrokes("ctrl-shift-z");
    cx.run_until_parked();
    assert_eq!(
        source_of(&view, cx),
        "# 表格\n\n| 名称 | 数量 |\n|:--|--:|\n| 苹果派 | 3 |\n\n后文\n"
    );

    close_shell(&host, cx);
}

#[gpui::test]
async fn escape_cancels_the_edit_without_touching_the_source(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "取消单元格编辑", TABLE_SOURCE);

    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);
    click_cell(TABLE_LINE, 1, 0, &view, cx);
    cx.simulate_input("毁掉");
    cx.run_until_parked();
    assert_eq!(source_of(&view, cx), TABLE_SOURCE, "草稿还没提交");

    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(!is_editing_cell(&view, cx), "Esc 退出编辑");
    assert_eq!(
        source_of(&view, cx),
        TABLE_SOURCE,
        "Esc 丢弃浮层草稿：正文逐字节不变"
    );
    let display = display_of(&view, cx);
    assert!(
        display.contains(menghunote_editor::OBJECT_PLACEHOLDER) && !display.contains("苹果"),
        "取消后表格仍是 Object：{display:?}"
    );
    // 取消不产生请求：一次 Ctrl+Z 不会“撤销取消”（文本仍然等于原文）。
    cx.simulate_keystrokes("ctrl-z");
    cx.run_until_parked();
    assert_eq!(source_of(&view, cx), TABLE_SOURCE, "取消没有进 undo 历史");

    close_shell(&host, cx);
}

#[gpui::test]
async fn double_click_also_edits_and_tab_commits_then_advances(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "双击与 Tab", TABLE_SOURCE);

    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);

    // 双击 = 直接进入编辑（一次手势）。
    double_click_cell(TABLE_LINE, 1, 1, &view, cx);
    assert!(is_editing_cell(&view, cx), "双击必须进入单元格编辑");
    assert_eq!(cell_target(&view, cx), Some((TABLE_LINE, 1, 1)));
    assert!(
        table_of(&view, cx).display.is_some(),
        "双击编辑时表格仍是 Object"
    );

    // Tab 提交并移到下一格（最后一格之后退出）。
    cx.simulate_input("4");
    cx.run_until_parked();
    cx.simulate_keystrokes("tab");
    cx.run_until_parked();
    assert!(!is_editing_cell(&view, cx), "最后一格 Tab 之后退出编辑");
    assert_eq!(
        source_of(&view, cx),
        "# 表格\n\n| 名称 | 数量 |\n|:--|--:|\n| 苹果 | 34 |\n\n后文\n",
        "Tab 先提交当前格"
    );
    let end = table_end_scalar(&source_of(&view, cx));
    assert_eq!(caret_of(&view, cx), (end, end), "退出后 caret 在整表末尾");

    // 回到第一格：输入后 Tab 会提交并进入第二格。
    click_cell(TABLE_LINE, 1, 0, &view, cx);
    cx.simulate_input("派");
    cx.run_until_parked();
    cx.simulate_keystrokes("tab");
    cx.run_until_parked();
    assert!(is_editing_cell(&view, cx), "Tab 之后继续编辑下一格");
    assert_eq!(
        cell_target(&view, cx),
        Some((TABLE_LINE, 1, 1)),
        "同一行的下一列"
    );
    assert_eq!(
        source_of(&view, cx),
        "# 表格\n\n| 名称 | 数量 |\n|:--|--:|\n| 苹果派 | 34 |\n\n后文\n",
        "Tab 先提交上一格"
    );
    assert!(
        table_of(&view, cx).display.is_some(),
        "切换格时表格仍是 Object"
    );
    cx.simulate_input("5");
    cx.run_until_parked();
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(
        source_of(&view, cx),
        "# 表格\n\n| 名称 | 数量 |\n|:--|--:|\n| 苹果派 | 345 |\n\n后文\n"
    );

    close_shell(&host, cx);
}

#[gpui::test]
async fn clicking_outside_commits_the_edit(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "点外面提交", TABLE_SOURCE);

    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);
    click_cell(TABLE_LINE, 1, 0, &view, cx);
    cx.simulate_input("派");
    cx.run_until_parked();

    // 点击表格下面的正文（`后文` 那一行）：先提交，再把 caret 落到点击处。
    let geometry = cx.update(|_window, app| view.read(app).visual_row_metrics());
    let last = geometry.last().expect("至少有一行");
    let pane = cx
        .debug_bounds("editor-current-doc")
        .expect("编辑器面板必须画出来");
    let target_row = last.row;
    let target_y = pane.origin.y
        + px(24.0)
        + px(geometry
            .iter()
            .take(target_row + 1)
            .map(|row| row.line_height)
            .sum::<f32>());
    cx.simulate_click(
        point(pane.origin.x + px(60.0), target_y),
        Modifiers::default(),
    );
    cx.run_until_parked();
    assert!(!is_editing_cell(&view, cx), "点击浮层外必须提交并退出编辑");
    assert_eq!(
        source_of(&view, cx),
        "# 表格\n\n| 名称 | 数量 |\n|:--|--:|\n| 苹果派 | 3 |\n\n后文\n",
        "点外面等同提交（一次请求、一个撤销组）"
    );
    assert_eq!(
        selected_cell(&view, cx),
        None,
        "离开表格后不再有单元格选中态"
    );
    cx.simulate_keystrokes("ctrl-z");
    cx.run_until_parked();
    assert_eq!(source_of(&view, cx), TABLE_SOURCE, "整次编辑是一个撤销组");

    close_shell(&host, cx);
}

#[gpui::test]
async fn new_cells_from_added_row_and_column_are_editable(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "加行列后编辑", TABLE_SOURCE);

    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);
    let rect = cx
        .update(|_window, app| {
            view.read(app)
                .object_rects()
                .into_iter()
                .find(|(line, _)| *line == TABLE_LINE)
                .map(|(_, rect)| rect)
        })
        .expect("表格对象必须可见");
    cx.simulate_click(rect.center(), Modifiers::default());
    cx.run_until_parked();
    // 单击表格 = 进入单元格编辑；先 Esc 退出，浮层才会出现（编辑期浮层隐藏）。
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    click_selector(cx, "table-insert-row");
    click_selector(cx, "table-insert-column");
    assert_eq!(
        source_of(&view, cx),
        "# 表格\n\n| 名称 | 数量 | |\n|:--|--:| ---|\n| 苹果 | 3 | |\n|  |  | |\n\n后文\n"
    );
    // 浮层点击会触发重绘；再动一下鼠标确保几何是最新的（悬浮/命中用上一帧几何）。
    let rect = cell_rect(TABLE_LINE, 0, 2, &view, cx);
    cx.simulate_mouse_move(rect.center(), None, Modifiers::default());
    cx.run_until_parked();

    // 新数据行（row 2）第一格：空 cell 可单击直接编辑。
    click_cell(TABLE_LINE, 2, 0, &view, cx);
    assert!(is_editing_cell(&view, cx), "加行后的新空 cell 必须可编辑");
    assert_eq!(cell_target(&view, cx), Some((TABLE_LINE, 2, 0)));
    assert!(
        table_of(&view, cx).display.is_some(),
        "编辑新空 cell 时表格仍是 Object"
    );
    cx.simulate_input("西瓜");
    cx.run_until_parked();
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(
        source_of(&view, cx),
        "# 表格\n\n| 名称 | 数量 | |\n|:--|--:| ---|\n| 苹果 | 3 | |\n|  西瓜|  | |\n\n后文\n",
        "新行的空 cell 写入成功，其它字节不变"
    );

    // 新列（表头第 3 格）同样可编辑。
    click_cell(TABLE_LINE, 0, 2, &view, cx);
    assert!(is_editing_cell(&view, cx), "加列后的新空 cell 必须可编辑");
    cx.simulate_input("备注");
    cx.run_until_parked();
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(
        source_of(&view, cx),
        "# 表格\n\n| 名称 | 数量 | 备注|\n|:--|--:| ---|\n| 苹果 | 3 | |\n|  西瓜|  | |\n\n后文\n"
    );

    close_shell(&host, cx);
}

#[gpui::test]
async fn cell_edit_is_persisted_and_the_table_survives_reopen(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault_path = dir.path().join("vault.sqlite3");
    let vault = Vault::open(&vault_path).expect("打开临时 vault");
    let doc_id = seed_note(&vault, "持久化单元格", TABLE_SOURCE);

    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);
    click_cell(TABLE_LINE, 0, 0, &view, cx);
    // 预填文本光标在末尾：追加即可（新交互不再全选）。
    cx.simulate_input("品名");
    cx.run_until_parked();
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(
        source_of(&view, cx),
        "# 表格\n\n| 名称品名 | 数量 |\n|:--|--:|\n| 苹果 | 3 |\n\n后文\n"
    );
    close_shell(&host, cx);

    // 1) durable 后正文逐字正确。
    let persisted = {
        let vault = Vault::open(&vault_path).expect("重开 vault");
        let (doc, _head) = load_body_doc(&vault, &doc_id, 1, None).expect("重放正文失败");
        doc.text().to_owned()
    };
    assert_eq!(
        persisted, "# 表格\n\n| 名称品名 | 数量 |\n|:--|--:|\n| 苹果 | 3 |\n\n后文\n",
        "单元格编辑必须落库"
    );

    // 2) 重新打开：表格仍是 Object，cell 文本来自新 source，且可以继续编辑。
    let vault = Vault::open(&vault_path).expect("再次打开 vault");
    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);
    let table = table_of(&view, cx);
    assert_eq!((table.line, table.rows, table.columns), (TABLE_LINE, 1, 2));
    assert!(table.display.is_some(), "重开后表格仍是 Object");
    assert!(
        !display_of(&view, cx).contains('|'),
        "重开后不得直接显示表格源码"
    );
    click_cell(TABLE_LINE, 1, 1, &view, cx);
    assert!(is_editing_cell(&view, cx), "重开后 cell 仍可继续编辑");
    assert_eq!(cell_target(&view, cx), Some((TABLE_LINE, 1, 1)));
    assert_eq!(
        cx.update(|_window, app| view.read(app).table_paint_rows()),
        vec![(TABLE_LINE, true, 1)]
    );

    close_shell(&host, cx);
}

#[gpui::test]
async fn typing_a_pipe_is_escaped_and_enter_never_breaks_the_table(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "编辑键不破结构", TABLE_SOURCE);

    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);
    click_cell(TABLE_LINE, 1, 0, &view, cx);

    // 输入竖线：提交时必须写成 `\|`，表格列数不变。
    cx.simulate_input("|");
    cx.run_until_parked();
    assert_eq!(source_of(&view, cx), TABLE_SOURCE, "草稿不写正文");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(
        source_of(&view, cx),
        "# 表格\n\n| 名称 | 数量 |\n|:--|--:|\n| 苹果\\| | 3 |\n\n后文\n",
        "cell 内的字面管道必须转义，否则表格会被切成两列"
    );
    let table = table_of(&view, cx);
    assert_eq!(table.columns, 2, "输入 `|` 之后列数不变");
    assert_eq!(table.rows, 1, "Enter 提交不会插入新行");
    assert!(source_of(&view, cx).ends_with("后文\n"));

    close_shell(&host, cx);
}

#[gpui::test]
async fn backspace_ime_and_paste_inside_a_cell_only_change_the_draft(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "格内退格输入法粘贴", TABLE_SOURCE);

    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);
    click_cell(TABLE_LINE, 1, 0, &view, cx);
    assert!(is_editing_cell(&view, cx));

    // Backspace：删的是浮层草稿的最后一个字，正文还不受影响。
    cx.simulate_keystrokes("backspace");
    cx.run_until_parked();
    assert_eq!(source_of(&view, cx), TABLE_SOURCE, "退格只改草稿");

    // debug 构建的 F9 = 真实 app 侧 IME 路径（组合 → 上屏），只改输入框草稿。
    cx.simulate_keystrokes("f9");
    cx.run_until_parked();
    assert_eq!(source_of(&view, cx), TABLE_SOURCE, "IME 上屏也只进草稿");
    assert!(is_editing_cell(&view, cx), "IME 上屏不退出单元格编辑");

    // 多行 + 字面管道的粘贴：换行 → 空格、`|` → `\|`（提交时统一转义）。
    cx.write_to_clipboard(gpui::ClipboardItem::new_string("甲\n乙|丙".to_owned()));
    cx.simulate_keystrokes("ctrl-v");
    cx.run_until_parked();
    assert_eq!(source_of(&view, cx), TABLE_SOURCE, "粘贴也只进草稿");

    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(
        source_of(&view, cx),
        "# 表格\n\n| 名称 | 数量 |\n|:--|--:|\n| 苹中文甲 乙\\|丙 | 3 |\n\n后文\n",
        "退格 + IME + 多行粘贴全部在提交时一次性写入（单行、转义管道）"
    );
    assert_eq!(table_of(&view, cx).columns, 2, "列数不变");
    assert!(!is_editing_cell(&view, cx));

    close_shell(&host, cx);
}

#[gpui::test]
async fn enter_on_a_selected_cell_reopens_the_overlay_and_empty_text_clears_it(
    cx: &mut TestAppContext,
) {
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "回车重进与清空", TABLE_SOURCE);

    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);
    click_cell(TABLE_LINE, 1, 1, &view, cx);
    assert_eq!(selected_cell(&view, cx), Some((TABLE_LINE, 1, 1)));
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(!is_editing_cell(&view, cx), "Esc 退出编辑（选中态保留）");
    assert_eq!(selected_cell(&view, cx), Some((TABLE_LINE, 1, 1)));

    // 选中格上 Enter = 重新进入原地编辑（旧入口保留，但不再是唯一入口）。
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert!(
        is_editing_cell(&view, cx),
        "选中单元格后 Enter 可重新进入编辑"
    );
    assert_eq!(cell_target(&view, cx), Some((TABLE_LINE, 1, 1)));

    // 全选删空 + Enter：空 cell 可提交空串（清空该格）。
    cx.simulate_keystrokes("ctrl-a");
    cx.simulate_keystrokes("backspace");
    cx.run_until_parked();
    assert!(is_editing_cell(&view, cx));
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(
        source_of(&view, cx),
        "# 表格\n\n| 名称 | 数量 |\n|:--|--:|\n| 苹果 |  |\n\n后文\n",
        "空串提交等于清空该 cell（其它字节不变）"
    );

    close_shell(&host, cx);
}

#[gpui::test]
async fn clicking_another_cell_commits_the_current_one_and_edits_the_new_one(
    cx: &mut TestAppContext,
) {
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "切格提交", TABLE_SOURCE);

    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);
    click_cell(TABLE_LINE, 1, 0, &view, cx);
    cx.simulate_input("派");
    cx.run_until_parked();
    assert_eq!(source_of(&view, cx), TABLE_SOURCE, "草稿还没提交");

    // 直接点另一格：先提交上一格（一条请求），再开始编辑新格。
    click_cell(TABLE_LINE, 1, 1, &view, cx);
    assert!(is_editing_cell(&view, cx), "切格后继续处于编辑态");
    assert_eq!(
        cell_target(&view, cx),
        Some((TABLE_LINE, 1, 1)),
        "正在编辑的是新点中的格"
    );
    assert_eq!(
        source_of(&view, cx),
        "# 表格\n\n| 名称 | 数量 |\n|:--|--:|\n| 苹果派 | 3 |\n\n后文\n",
        "切格先提交上一格"
    );
    assert!(
        table_of(&view, cx).display.is_some(),
        "切格期间表格仍然是 Object"
    );

    cx.simulate_input("4");
    cx.run_until_parked();
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(
        source_of(&view, cx),
        "# 表格\n\n| 名称 | 数量 |\n|:--|--:|\n| 苹果派 | 34 |\n\n后文\n"
    );
    close_shell(&host, cx);
}
