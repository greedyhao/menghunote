//! ADR 0019 S3b2：app headless「表格单元格选择 / 就地编辑」集成测试。
//!
//! 真实 `AppShell` + 临时 vault（`CARGO_TARGET_TMPDIR`、不 sleep）+ 真实 `#[gpui::test]`
//! 窗口：断言
//!
//! 1. 单击单元格 → 只选中该格（高亮），表格仍是 Object（display 里没有 `|`/整表源码），
//!    **不移动正文 caret**；
//! 2. 再次单击同一格 / 双击 / `F2` / `Enter` → 进入该 cell 的就地编辑：display 只剩
//!    该 cell 文本，caret/选区在该文本内；输入只改该 cell 的源码区间；
//! 3. `Enter` 提交（一条 `EditRequest` = 一次 `Ctrl+Z` 回到原表）；`Esc` 取消（源码逐字节
//!    回到进入编辑前）；`Tab` 提交并移到下一格；点击 cell 外提交并落 caret；
//! 4. 加行/加列之后的新（空）cell 可点击、可编辑；
//! 5. 持久化后重开，cell 内的字仍在，表格仍是 Object。
//!
//! 像素级外观（高亮/描边颜色、DPI、真实窗口）仍必须真机验证；这里证明
//! “几何 → 点击 → 一个 `EditRequest` → 源码/undo/持久化”链路。

use std::path::Path;

use gpui::{
    div, px, AppContext as _, Entity, InteractiveElement as _, Modifiers, MouseButton,
    MouseDownEvent, MouseUpEvent, ParentElement as _, Styled as _, TestAppContext,
    VisualTestContext,
};
use menghunote_app::editor::view::EditorView;
use menghunote_app::shell::AppShell;
use menghunote_editor::SourceByte;
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

/// 点击一个单元格（`click_count = 1`）。
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

/// 双击一个单元格（一次手势里 `click_count = 2`）。
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
async fn single_click_selects_the_cell_without_moving_the_caret(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "选单元格", TABLE_SOURCE);

    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);
    let placeholder = menghunote_editor::OBJECT_PLACEHOLDER;
    assert_eq!(caret_of(&view, cx), (0, 0), "打开笔记时 caret 在文档开头");

    // 单元格矩形来自真实绘制几何（表头 1 行 + 1 条数据行）。
    let rects = cx.update(|_window, app| view.read(app).table_cell_rects());
    assert_eq!(rects.len(), 4, "2 列 × 2 行 = 4 个单元格矩形：{rects:?}");
    let rect = cell_rect(TABLE_LINE, 1, 0, &view, cx);

    // 单击数据行第一格：只选中该格，表格仍是 Object，caret 一动不动。
    cx.simulate_click(rect.center(), Modifiers::default());
    cx.run_until_parked();
    assert_eq!(
        selected_cell(&view, cx),
        Some((TABLE_LINE, 1, 0)),
        "单击必须选中该单元格"
    );
    let table = table_of(&view, cx);
    assert!(table.selected, "表格仍是当前选中对象（浮层/工具条可用）");
    assert_eq!(table.selected_cell, Some((1, 0)));
    assert_eq!(table.editing_cell, None, "单击不进入编辑");
    assert!(!table.editing_source, "单击不进入整表源码模式");
    assert!(table.display.is_some(), "表格仍投影成一个 Object 占位符");
    let display = display_of(&view, cx);
    assert!(
        display.contains(placeholder) && !display.contains('|') && !display.contains("苹果"),
        "display 必须只是占位符（不展开整表源码）：{display:?}"
    );
    assert_eq!(caret_of(&view, cx), (0, 0), "单击单元格不得移动正文 caret");
    assert!(!is_editing_cell(&view, cx), "单击一次不进入编辑");

    close_shell(&host, cx);
}

#[gpui::test]
async fn second_click_and_f2_enter_the_cell_editing_only_the_cell_text(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "再次单击进入编辑", TABLE_SOURCE);

    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);
    click_cell(TABLE_LINE, 1, 0, &view, cx);
    assert_eq!(selected_cell(&view, cx), Some((TABLE_LINE, 1, 0)));

    // 再次单击同一格 → 进入编辑。
    click_cell(TABLE_LINE, 1, 0, &view, cx);
    assert!(is_editing_cell(&view, cx), "再次单击同一格必须进入编辑");
    assert_eq!(cell_target(&view, cx), Some((TABLE_LINE, 1, 0)));
    assert_eq!(
        display_of(&view, cx),
        "表格\n\n苹果\n\n后文\n",
        "编辑态 display 只显示该 cell 文本（没有 `|`、没有整表源码）"
    );
    let table = table_of(&view, cx);
    assert_eq!(table.editing_cell, Some((1, 0)));
    assert!(table.display.is_none(), "编辑中的表格不再是 Object 占位符");
    assert!(!table.editing_source, "单元格编辑不是整表源码模式");
    // caret 落在该 cell 文本末尾（scalar: 表格起点之前的字符数 + "苹果"）。
    let cell_end = TABLE_SOURCE
        .find("苹果")
        .expect("cell 文本")
        .checked_add("苹果".len())
        .expect("offset");
    let expected = SourceByte::new(cell_end)
        .to_scalar(TABLE_SOURCE)
        .expect("scalar");
    assert_eq!(
        caret_of(&view, cx),
        (expected.get(), expected.get()),
        "caret 落在该 cell 文本内"
    );
    // 单元格内的选区/光标只能在 cell 里：点表格外的正文会先提交。
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(!is_editing_cell(&view, cx), "Esc 退出编辑");
    assert_eq!(source_of(&view, cx), TABLE_SOURCE, "没有改动 → 源码不变");

    // F2：选中单元格后直接进入编辑。
    click_cell(TABLE_LINE, 0, 1, &view, cx);
    cx.simulate_keystrokes("f2");
    cx.run_until_parked();
    assert!(is_editing_cell(&view, cx), "F2 必须进入单元格编辑");
    assert_eq!(cell_target(&view, cx), Some((TABLE_LINE, 0, 1)));
    assert_eq!(display_of(&view, cx), "表格\n\n数量\n\n后文\n");
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();

    // 双击：一次手势直接进入编辑。
    double_click_cell(TABLE_LINE, 1, 1, &view, cx);
    assert!(is_editing_cell(&view, cx), "双击必须进入单元格编辑");
    assert_eq!(cell_target(&view, cx), Some((TABLE_LINE, 1, 1)));
    assert_eq!(display_of(&view, cx), "表格\n\n3\n\n后文\n");

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
    click_cell(TABLE_LINE, 1, 0, &view, cx);
    assert!(is_editing_cell(&view, cx));

    cx.simulate_input("派");
    cx.run_until_parked();
    assert_eq!(
        display_of(&view, cx),
        "表格\n\n苹果派\n\n后文\n",
        "输入立刻可见"
    );
    assert_eq!(
        source_of(&view, cx),
        "# 表格\n\n| 名称 | 数量 |\n|:--|--:|\n| 苹果派 | 3 |\n\n后文\n",
        "只改该 cell 的源码区间；两侧 `|`/对齐/其它 cell/行尾换行逐字不变"
    );

    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert!(!is_editing_cell(&view, cx), "Enter 提交并退出编辑");
    let table = table_of(&view, cx);
    assert!(table.display.is_some(), "提交后表格重新是 Object");
    assert!(table.selected, "提交后表格仍是当前选中对象");
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

    // 一次提交 = 一个撤销组：一次 Ctrl+Z 回到进入编辑前的整篇原文。
    cx.simulate_keystrokes("ctrl-z");
    cx.run_until_parked();
    assert_eq!(
        source_of(&view, cx),
        TABLE_SOURCE,
        "一次撤销必须把整格输入一起撤回（不是逐次按键撤销）"
    );
    // 重做恢复同一格。
    cx.simulate_keystrokes("ctrl-shift-z");
    cx.run_until_parked();
    assert_eq!(
        source_of(&view, cx),
        "# 表格\n\n| 名称 | 数量 |\n|:--|--:|\n| 苹果派 | 3 |\n\n后文\n"
    );

    close_shell(&host, cx);
}

#[gpui::test]
async fn escape_cancels_the_edit_and_restores_the_cell(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "取消单元格编辑", TABLE_SOURCE);

    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);
    click_cell(TABLE_LINE, 1, 0, &view, cx);
    click_cell(TABLE_LINE, 1, 0, &view, cx);
    cx.simulate_input("毁掉");
    cx.run_until_parked();
    assert!(source_of(&view, cx).contains("苹果毁掉"));

    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(!is_editing_cell(&view, cx), "Esc 退出编辑");
    assert_eq!(
        source_of(&view, cx),
        TABLE_SOURCE,
        "Esc 恢复进入编辑前的 cell 文本"
    );
    let display = display_of(&view, cx);
    assert!(
        display.contains(menghunote_editor::OBJECT_PLACEHOLDER) && !display.contains("苹果"),
        "取消后表格重新投影成对象：{display:?}"
    );
    // 取消不产生请求：一次 Ctrl+Z 不会“撤销取消”（文本仍然等于原文）。
    cx.simulate_keystrokes("ctrl-z");
    cx.run_until_parked();
    assert_eq!(
        source_of(&view, cx),
        TABLE_SOURCE,
        "取消没有进 undo 历史（否则一次撤销会把取消再反向应用）"
    );

    close_shell(&host, cx);
}

#[gpui::test]
async fn tab_commits_the_cell_and_moves_to_the_next_one(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "Tab 下一格", TABLE_SOURCE);

    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);
    click_cell(TABLE_LINE, 1, 0, &view, cx);
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
        "# 表格\n\n| 名称 | 数量 |\n|:--|--:|\n| 苹果派 | 3 |\n\n后文\n",
        "Tab 先提交当前格"
    );
    assert_eq!(
        display_of(&view, cx),
        "表格\n\n3\n\n后文\n",
        "display 切到下一格"
    );

    cx.simulate_input("4");
    cx.run_until_parked();
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(
        source_of(&view, cx),
        "# 表格\n\n| 名称 | 数量 |\n|:--|--:|\n| 苹果派 | 34 |\n\n后文\n"
    );
    assert!(!is_editing_cell(&view, cx));

    // 最后一格之后 Tab：提交并退出表格（caret 在整表末尾）。
    // 该格在 Tab/Enter 之后仍保持单元格选中态，所以 F2 就能直接再次进入编辑。
    assert_eq!(selected_cell(&view, cx), Some((TABLE_LINE, 1, 1)));
    cx.simulate_keystrokes("f2");
    cx.run_until_parked();
    assert!(is_editing_cell(&view, cx), "F2 重新进入最后一格");
    cx.simulate_keystrokes("tab");
    cx.run_until_parked();
    assert!(!is_editing_cell(&view, cx), "表格最后一格之后退出编辑");
    let end = table_end_scalar(&source_of(&view, cx));
    assert_eq!(caret_of(&view, cx), (end, end), "退出后 caret 在整表末尾");

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
    click_cell(TABLE_LINE, 1, 0, &view, cx);
    cx.simulate_input("派");
    cx.run_until_parked();

    // 点击表格下面的正文（`后文` 那一行）：先提交，再把 caret 落到点击处。
    let geometry = cx.update(|_window, app| view.read(app).visual_row_metrics());
    let last = geometry.last().expect("至少有一行");
    let pane = cx
        .debug_bounds("editor-pane")
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
        gpui::point(pane.origin.x + px(60.0), target_y),
        Modifiers::default(),
    );
    cx.run_until_parked();
    assert!(
        !is_editing_cell(&view, cx),
        "点击 cell 外必须提交并退出编辑"
    );
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

    // 新数据行（row 2）第一格：空 cell 可点击/可编辑。
    click_cell(TABLE_LINE, 2, 0, &view, cx);
    assert_eq!(selected_cell(&view, cx), Some((TABLE_LINE, 2, 0)));
    click_cell(TABLE_LINE, 2, 0, &view, cx);
    assert!(is_editing_cell(&view, cx), "加行后的新空 cell 必须可编辑");
    assert_eq!(
        display_of(&view, cx),
        "表格\n\n\n\n后文\n",
        "空 cell 的 display 是空行（表格其余部分全部隐藏）"
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
    click_cell(TABLE_LINE, 0, 0, &view, cx);
    // 选中 cell 文本再整体替换：`Home` 到格首、`Shift+End` 选中该行到格尾。
    cx.simulate_keystrokes("end");
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
    click_cell(TABLE_LINE, 1, 1, &view, cx);
    assert!(is_editing_cell(&view, cx), "重开后 cell 仍可继续编辑");
    assert_eq!(display_of(&view, cx), "表格\n\n3\n\n后文\n");

    close_shell(&host, cx);
}

#[gpui::test]
async fn editing_keys_do_not_leak_into_the_table_structure(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "编辑键不破结构", TABLE_SOURCE);

    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);
    click_cell(TABLE_LINE, 1, 0, &view, cx);
    click_cell(TABLE_LINE, 1, 0, &view, cx);

    // Enter 提交（不换行）；再进一次，输入竖线必须被写成 `\|`，表格列数不变。
    cx.simulate_input("|");
    cx.run_until_parked();
    assert_eq!(
        source_of(&view, cx),
        "# 表格\n\n| 名称 | 数量 |\n|:--|--:|\n| 苹果\\| | 3 |\n\n后文\n",
        "cell 内的字面管道必须转义，否则表格会被切成两列"
    );
    let table = table_of(&view, cx);
    assert_eq!(table.columns, 2, "输入 `|` 之后列数不变");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(table_of(&view, cx).rows, 1, "Enter 提交不会插入新行");
    assert!(source_of(&view, cx).ends_with("后文\n"));

    close_shell(&host, cx);
}

#[gpui::test]
async fn backspace_and_ime_inside_a_cell_use_the_normal_editor_path(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "格内退格与输入法", TABLE_SOURCE);

    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);
    click_cell(TABLE_LINE, 1, 0, &view, cx);
    click_cell(TABLE_LINE, 1, 0, &view, cx);
    assert!(is_editing_cell(&view, cx));

    // Backspace 走与正文完全相同的删除路径（投影 → 源码最小间），只删该 cell 的字节。
    cx.simulate_keystrokes("backspace");
    cx.run_until_parked();
    assert_eq!(
        source_of(&view, cx),
        "# 表格\n\n| 名称 | 数量 |\n|:--|--:|\n| 苹 | 3 |\n\n后文\n",
        "退格只删该 cell 的最后一个字"
    );
    assert_eq!(display_of(&view, cx), "表格\n\n苹\n\n后文\n");

    // debug 构建的 F9 = 真实 app 侧 IME 路径（update_preedit → commit_composition）。
    cx.simulate_keystrokes("f9");
    cx.run_until_parked();
    assert_eq!(
        source_of(&view, cx),
        "# 表格\n\n| 名称 | 数量 |\n|:--|--:|\n| 苹中文 | 3 |\n\n后文\n",
        "IME 上屏在单元格编辑期间也只改这一格（仍是一次输入）"
    );
    assert!(is_editing_cell(&view, cx), "IME 上屏不退出单元格编辑");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(
        source_of(&view, cx),
        "# 表格\n\n| 名称 | 数量 |\n|:--|--:|\n| 苹中文 | 3 |\n\n后文\n",
        "Enter 提交后源码不变（提交不再产生第二条请求）"
    );
    assert!(!is_editing_cell(&view, cx));

    close_shell(&host, cx);
}

#[gpui::test]
async fn pasting_text_into_a_cell_keeps_the_table_structure(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "格内粘贴", TABLE_SOURCE);

    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);
    click_cell(TABLE_LINE, 1, 0, &view, cx);
    click_cell(TABLE_LINE, 1, 0, &view, cx);
    assert!(is_editing_cell(&view, cx));

    // 多行 + 字面管道的纯文本：换行 → 空格、`|` → `\|`，表格结构不变。
    cx.write_to_clipboard(gpui::ClipboardItem::new_string("甲\n乙|丙".to_owned()));
    cx.simulate_keystrokes("ctrl-v");
    cx.run_until_parked();
    assert_eq!(
        source_of(&view, cx),
        "# 表格\n\n| 名称 | 数量 |\n|:--|--:|\n| 苹果甲 乙\\|丙 | 3 |\n\n后文\n",
        "粘贴进 cell 的文本必须单行且转义管道"
    );
    assert_eq!(table_of(&view, cx).columns, 2, "列数不变");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert!(!is_editing_cell(&view, cx));

    close_shell(&host, cx);
}

#[gpui::test]
async fn enter_and_tab_while_a_cell_is_selected_edit_the_cell(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "回车进入格", TABLE_SOURCE);

    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);
    click_cell(TABLE_LINE, 1, 1, &view, cx);
    assert_eq!(selected_cell(&view, cx), Some((TABLE_LINE, 1, 1)));
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert!(is_editing_cell(&view, cx), "选中单元格后 Enter = 进入编辑");
    assert_eq!(cell_target(&view, cx), Some((TABLE_LINE, 1, 1)));
    cx.simulate_input("4");
    cx.run_until_parked();
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(
        source_of(&view, cx),
        "# 表格\n\n| 名称 | 数量 |\n|:--|--:|\n| 苹果 | 34 |\n\n后文\n",
        "Enter 提交一次替换（不是换行）"
    );
    assert_eq!(table_of(&view, cx).rows, 1, "行数不变");

    close_shell(&host, cx);
}
