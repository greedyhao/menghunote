//! ADR 0019 S3b3：app headless「表格删除行 / 删除列」集成测试。
//!
//! 真实 `AppShell` + 临时 vault（`CARGO_TARGET_TMPDIR`、不 sleep）+ 真实 `#[gpui::test]`
//! 窗口：断言
//!
//! 1. 浮层里的删除按钮**只在选着某个单元格时出现**（悬浮/只选中整块都没有），且表头行
//!    不提供「删除行」；
//! 2. 点「删除行」→ 只删选中的数据行（整行含行尾换行、逐字节）、表格仍是 Object、
//!    caret 不落进表格源码内部、单元格选中态修正到最近可用格；
//! 3. 点「删除列」→ 表头 / 分隔行 / 所有数据行都少一列（对齐冒号保留）、至少保留 1 列，
//!    只剩一列时按钮禁用且点击不产生任何请求；
//! 4. 一次点击 = 一个撤销组（一次 `Ctrl+Z` 恢复原表，`Ctrl+Shift+Z` 重做）；
//! 5. 重开 vault 后删除结果都在，表格仍投影成对象；
//! 6. 没有“无确认的快捷键直接删”：`Ctrl+Delete` / `Ctrl+Backspace` 不动表格结构。
//!
//! 像素级外观（禁用态颜色、DPI、真实窗口）仍必须真机验证；这里证明
//! “几何 → 点击 → 一个 `EditRequest` → 源码/undo/持久化”链路。

use std::path::Path;

use gpui::{
    div, AppContext as _, Entity, InteractiveElement as _, Modifiers, ParentElement as _,
    Styled as _, TestAppContext, VisualTestContext,
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
        .join(format!("headless-table-delete-{}", std::process::id()));
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
        .commit("headless-table-delete-seed")
        .expect("结构提交失败");

    let (doc, head) = load_body_doc(vault, &seed.body_doc_id, 1, None).expect("加载正文文档失败");
    doc.insert_scalars(0, text).expect("写入正文失败");
    let base = head.version_vector().expect("解码 head VV 失败");
    let update = doc.pending_update(&base).expect("导出正文增量失败");
    append_updates(
        vault,
        &seed.body_doc_id,
        head.head_seq,
        &[UpdateRecord::new("headless-table-delete", update)],
    )
    .expect("A 事务写入正文失败");
    seed.body_doc_id
}

fn open_shell(
    cx: &mut TestAppContext,
    vault: Vault,
) -> (Entity<ShellHost>, &mut VisualTestContext) {
    let (host, cx) = cx.add_window_view(move |_, cx| {
        let shell = cx.new(|cx| AppShell::new(vault, "headless-table-delete".to_owned(), cx));
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

fn selected_cell(
    view: &Entity<EditorView>,
    cx: &mut VisualTestContext,
) -> Option<(usize, usize, usize)> {
    cx.update(|_window, app| view.read(app).selected_table_cell())
}

/// 当前可见表格快照（只有一张表时直接取第一张；不存在就失败，避免假通过）。
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

/// 点击一个单元格（单击 = 选中该格，不移动正文 caret）。
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

/// 点击一个 debug selector 目标（找不到就失败：按钮必须真实画出来）。
fn click_selector(cx: &mut VisualTestContext, selector: &'static str) {
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("`{selector}` 必须可见"));
    cx.simulate_click(bounds.center(), Modifiers::default());
    cx.run_until_parked();
}

/// 悬浮到表格对象中心（不点选，只让浮层出现）。
fn hover_table(line: usize, view: &Entity<EditorView>, cx: &mut VisualTestContext) {
    let rect = cx
        .update(|_window, app| {
            view.read(app)
                .object_rects()
                .into_iter()
                .find(|(object_line, _)| *object_line == line)
                .map(|(_, rect)| rect)
        })
        .unwrap_or_else(|| panic!("对象行 {line} 必须可见"));
    cx.simulate_mouse_move(rect.center(), None, Modifiers::default());
    cx.run_until_parked();
}

/// 表格表头行号（0 起）。
const TABLE_LINE: usize = 2;

/// 3 条数据行 / 2 列 + 两种对齐的表格。
const ROWS_SOURCE: &str =
    "# 表格\n\n| 名称 | 数量 |\n|:--|--:|\n| 苹果 | 3 |\n| 香蕉 | 5 |\n| 橙子 | 7 |\n\n后文\n";
/// 删掉「香蕉」那一行之后（逐字节：整行含行尾换行）。
const ROWS_SOURCE_AFTER_DELETE: &str =
    "# 表格\n\n| 名称 | 数量 |\n|:--|--:|\n| 苹果 | 3 |\n| 橙子 | 7 |\n\n后文\n";

/// 2 条数据行 / 3 列 + 三种对齐的表格（删列用）。
const COLUMNS_SOURCE: &str =
    "# 表格\n\n| 名称 | 数量 | 备注 |\n|:--|:-:|--:|\n| 苹果 | 3 | 红 |\n| 香蕉 | 5 | 黄 |\n\n后文\n";
/// 删中间列（`数量`）之后。
const COLUMNS_SOURCE_MIDDLE_DELETED: &str =
    "# 表格\n\n| 名称 | 备注 |\n|:--|--:|\n| 苹果 | 红 |\n| 香蕉 | 黄 |\n\n后文\n";
/// 删末列（`备注`）之后。
const COLUMNS_SOURCE_LAST_DELETED: &str =
    "# 表格\n\n| 名称 | 数量 |\n|:--|:-:|\n| 苹果 | 3 |\n| 香蕉 | 5 |\n\n后文\n";
/// 删首列（`名称`）之后。
const COLUMNS_SOURCE_FIRST_DELETED: &str =
    "# 表格\n\n| 数量 | 备注 |\n|:-:|--:|\n| 3 | 红 |\n| 5 | 黄 |\n\n后文\n";

/// 只有 1 列的表格（“至少保留 1 列”用）。
const ONE_COLUMN_SOURCE: &str = "# 表格\n\n| 名称 |\n|---|\n| 苹果 |\n\n后文\n";

#[gpui::test]
async fn delete_row_button_removes_the_selected_data_row(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "删除行", ROWS_SOURCE);

    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);
    let placeholder = menghunote_editor::OBJECT_PLACEHOLDER;

    // 悬浮（未选格）：只有加行加列，没有删除按钮（避免误删）。
    hover_table(TABLE_LINE, &view, cx);
    assert!(
        cx.debug_bounds("table-insert-row").is_some(),
        "悬浮表格必须显示「+ 行」"
    );
    assert!(
        cx.debug_bounds("table-delete-row").is_none()
            && cx.debug_bounds("table-delete-column").is_none(),
        "没有选中单元格时不得显示删除按钮"
    );

    // 单击数据行（row 2 = 「香蕉」）的「数量」格：进入单元格选中态，删除按钮出现。
    click_cell(TABLE_LINE, 2, 1, &view, cx);
    assert_eq!(selected_cell(&view, cx), Some((TABLE_LINE, 2, 1)));
    assert!(
        cx.debug_bounds("table-delete-row").is_some(),
        "选中数据行单元格后必须显示「删除行」"
    );
    assert!(
        cx.debug_bounds("table-delete-column").is_some(),
        "选中单元格后必须显示「删除列」"
    );
    assert_eq!(caret_of(&view, cx), (0, 0), "单击单元格不移动正文 caret");

    // 点「删除行」：只删这一条数据行（逐字节），表格仍是 Object。
    click_selector(cx, "table-delete-row");
    assert_eq!(
        source_of(&view, cx),
        ROWS_SOURCE_AFTER_DELETE,
        "删除行必须整行含行尾换行一起删（其余字节逐字不变）"
    );
    let table = table_of(&view, cx);
    assert_eq!((table.rows, table.columns), (2, 2), "行数 -1、列数不变");
    assert!(table.selected, "删除后表格仍是当前选中对象");
    assert!(
        !table.editing_source && table.display.is_some(),
        "删除后表格仍必须是 Object（不得进入源码模式）"
    );
    let display = display_of(&view, cx);
    assert!(
        display.contains(placeholder) && !display.contains("| 名称 |"),
        "删除后仍投影成对象占位符：{display:?}"
    );
    // 单元格选中态修正：同列**上一行**（原 row 1 = 苹果）。
    assert_eq!(
        selected_cell(&view, cx),
        Some((TABLE_LINE, 1, 1)),
        "删行后选中格必须修正到最近可用格（同列上一行）"
    );
    // caret 不落进表格源码内部（原本在文档开头，删除都在它之后）。
    assert_eq!(caret_of(&view, cx), (0, 0), "caret 不得被删除挪进表格");
    // 几何按新行数刷新：绘制表头 + 2 条数据行。
    let painted = cx.update(|_window, app| view.read(app).table_paint_rows());
    assert_eq!(
        painted,
        vec![(TABLE_LINE, true, 2)],
        "删除后必须绘制表头 + 2 条数据行"
    );

    // 一次点击 = 一个撤销组：一次 Ctrl+Z 恢复原表，Ctrl+Shift+Z 重做。
    cx.simulate_keystrokes("ctrl-z");
    cx.run_until_parked();
    assert_eq!(source_of(&view, cx), ROWS_SOURCE, "一次撤销恢复整张表");
    assert_eq!(table_of(&view, cx).rows, 3);
    cx.simulate_keystrokes("ctrl-shift-z");
    cx.run_until_parked();
    assert_eq!(
        source_of(&view, cx),
        ROWS_SOURCE_AFTER_DELETE,
        "重做恢复同一次删除"
    );

    close_shell(&host, cx);
}

#[gpui::test]
async fn header_cell_has_no_delete_row_button_and_column_delete_keeps_alignment(
    cx: &mut TestAppContext,
) {
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "表头删列", COLUMNS_SOURCE);

    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);

    // 表头行：不提供「删除行」（首版明确不删表头），但「删除列」可用。
    click_cell(TABLE_LINE, 0, 1, &view, cx);
    assert_eq!(selected_cell(&view, cx), Some((TABLE_LINE, 0, 1)));
    assert!(
        cx.debug_bounds("table-delete-row").is_none(),
        "选中表头行时不得提供「删除行」"
    );
    assert!(
        cx.debug_bounds("table-delete-column").is_some(),
        "表头行也必须能删列"
    );

    // 点「删除列」：表头 / 分隔行 / 所有数据行都少这一列（对齐冒号保留）。
    click_selector(cx, "table-delete-column");
    assert_eq!(
        source_of(&view, cx),
        COLUMNS_SOURCE_MIDDLE_DELETED,
        "删列必须同时改表头、分隔行与所有数据行；其余 cell 与对齐冒号逐字保留"
    );
    let table = table_of(&view, cx);
    assert_eq!((table.rows, table.columns), (2, 2), "列数 -1、行数不变");
    assert!(table.display.is_some() && !table.editing_source);
    assert_eq!(
        selected_cell(&view, cx),
        Some((TABLE_LINE, 0, 1)),
        "删中间列后选中原右侧格（右邻左移补位）"
    );
    assert_eq!(caret_of(&view, cx), (0, 0), "点浮层按钮不移动 caret");

    // 一次撤销把整列（表头/分隔行/所有数据行）一起撤回。
    cx.simulate_keystrokes("ctrl-z");
    cx.run_until_parked();
    assert_eq!(source_of(&view, cx), COLUMNS_SOURCE);
    assert_eq!(table_of(&view, cx).columns, 3);

    close_shell(&host, cx);
}

#[gpui::test]
async fn delete_first_and_last_columns_are_byte_exact(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "删首末列", COLUMNS_SOURCE);

    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);

    // 删末列（备注）：选数据行末格。
    click_cell(TABLE_LINE, 1, 2, &view, cx);
    click_selector(cx, "table-delete-column");
    assert_eq!(
        source_of(&view, cx),
        COLUMNS_SOURCE_LAST_DELETED,
        "删末列：保留落尾 `|`，不留下多余空白"
    );
    assert_eq!(table_of(&view, cx).columns, 2);
    assert_eq!(
        selected_cell(&view, cx),
        Some((TABLE_LINE, 1, 1)),
        "删末列后选中左侧格"
    );

    // 撤销恢复 3 列，再删首列（名称）。
    cx.simulate_keystrokes("ctrl-z");
    cx.run_until_parked();
    assert_eq!(source_of(&view, cx), COLUMNS_SOURCE);
    click_cell(TABLE_LINE, 1, 0, &view, cx);
    click_selector(cx, "table-delete-column");
    assert_eq!(
        source_of(&view, cx),
        COLUMNS_SOURCE_FIRST_DELETED,
        "删首列：保留行首 `|` 与其余 cell 的空白"
    );
    let table = table_of(&view, cx);
    assert_eq!(table.columns, 2);
    assert_eq!(
        selected_cell(&view, cx),
        Some((TABLE_LINE, 1, 0)),
        "删首列后选中原右侧格（左移补位到第 0 列）"
    );
    assert_eq!(caret_of(&view, cx), (0, 0));

    close_shell(&host, cx);
}

#[gpui::test]
async fn last_remaining_column_is_disabled_and_deleting_the_row_still_works(
    cx: &mut TestAppContext,
) {
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "只剩一列", ONE_COLUMN_SOURCE);

    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);
    click_cell(TABLE_LINE, 1, 0, &view, cx);
    assert!(
        cx.debug_bounds("table-delete-column").is_some(),
        "只剩 1 列时「删除列」仍要画出来（禁用态 + 提示），不能悄悄消失"
    );

    // 点禁用的「删除列」：不产生任何请求，源码逐字节不变。
    click_selector(cx, "table-delete-column");
    assert_eq!(
        source_of(&view, cx),
        ONE_COLUMN_SOURCE,
        "只剩 1 列时必须禁用删除（不能把表格改成非法结构）"
    );
    assert_eq!(table_of(&view, cx).columns, 1);
    assert_eq!(selected_cell(&view, cx), Some((TABLE_LINE, 1, 0)));

    // 删除行仍然可用；删掉唯一的数据行后表格只剩表头 + 分隔行，仍是 Object。
    click_selector(cx, "table-delete-row");
    assert_eq!(
        source_of(&view, cx),
        "# 表格\n\n| 名称 |\n|---|\n\n后文\n",
        "删除唯一数据行：整行含行尾换行，不留空行"
    );
    let table = table_of(&view, cx);
    assert_eq!((table.rows, table.columns), (0, 1));
    assert!(table.display.is_some() && !table.editing_source);
    assert_eq!(
        selected_cell(&view, cx),
        None,
        "没有数据行可落 → 清空单元格选中态（不猜不存在的格子）"
    );

    close_shell(&host, cx);
}

#[gpui::test]
async fn deleted_rows_and_columns_survive_reopen(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault_path = dir.path().join("vault.sqlite3");
    let vault = Vault::open(&vault_path).expect("打开临时 vault");
    let doc_id = seed_note(
        &vault,
        "持久化删除",
        "# 表格\n\n| a | b | c |\n|---|---|---|\n| 1 | 2 | 3 |\n| 4 | 5 | 6 |\n\n后文\n",
    );

    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);
    click_cell(TABLE_LINE, 1, 1, &view, cx);
    click_selector(cx, "table-delete-row");
    // 删行后选中格修正到 row 1 / col 1，浮层按钮仍在（选中态保留），直接删列。
    click_selector(cx, "table-delete-column");
    assert_eq!(
        source_of(&view, cx),
        "# 表格\n\n| a | c |\n|---|---|\n| 4 | 6 |\n\n后文\n",
        "先删 a 行、再删 b 列（两次点击 = 两个撤销组，各自逐字节正确）"
    );
    close_shell(&host, cx);

    // 1) durable 后正文逐字正确。
    let persisted = {
        let vault = Vault::open(&vault_path).expect("重开 vault");
        let (doc, _head) = load_body_doc(&vault, &doc_id, 1, None).expect("重放正文失败");
        doc.text().to_owned()
    };
    assert_eq!(
        persisted, "# 表格\n\n| a | c |\n|---|---|\n| 4 | 6 |\n\n后文\n",
        "落库后必须保留删除结果"
    );

    // 2) 重新打开 shell（新 Vault / 新视图）：表格仍是对象，行列数来自新 source。
    let vault = Vault::open(&vault_path).expect("再次打开 vault");
    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);
    let table = table_of(&view, cx);
    assert_eq!(
        (table.line, table.rows, table.columns),
        (TABLE_LINE, 1, 2),
        "重开后表格行列数必须与源码一致"
    );
    assert_eq!(table.end_line, TABLE_LINE + 2, "表头/分隔行 + 1 条数据行");
    assert!(table.display.is_some(), "重开后表格仍是 Object");
    let display = display_of(&view, cx);
    assert!(
        display.contains(menghunote_editor::OBJECT_PLACEHOLDER) && !display.contains("| a | c |"),
        "重开后不得直接把表格源码显示出来：{display:?}"
    );
    close_shell(&host, cx);
}

#[gpui::test]
async fn delete_buttons_need_a_selected_cell_and_no_shortcut_deletes(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "删除入口", ROWS_SOURCE);

    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);

    // 未选格：没有删除按钮。
    hover_table(TABLE_LINE, &view, cx);
    assert!(cx.debug_bounds("table-delete-row").is_none());
    assert!(cx.debug_bounds("table-delete-column").is_none());

    // 选格后出现；Esc 退出单元格选中态后它们一起消失（加行加列仍在）。
    click_cell(TABLE_LINE, 1, 0, &view, cx);
    assert!(cx.debug_bounds("table-delete-row").is_some());
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert_eq!(selected_cell(&view, cx), None, "Esc 清除单元格选中态");
    assert!(
        cx.debug_bounds("table-delete-row").is_none()
            && cx.debug_bounds("table-delete-column").is_none(),
        "退出单元格选中态后删除按钮必须一起消失"
    );
    assert!(
        cx.debug_bounds("table-insert-row").is_some(),
        "加行仍可用（S3b1 行为不回退）"
    );

    // 不存在“无确认的快捷键直接删”：带 Ctrl 的 Delete / Backspace 什么都不做。
    click_cell(TABLE_LINE, 2, 0, &view, cx);
    let before = source_of(&view, cx);
    cx.simulate_keystrokes("ctrl-delete");
    cx.run_until_parked();
    cx.simulate_keystrokes("ctrl-backspace");
    cx.run_until_parked();
    assert_eq!(
        source_of(&view, cx),
        before,
        "Ctrl+Delete / Ctrl+Backspace 不得删除表格行/列"
    );
    assert_eq!(table_of(&view, cx).rows, 3);

    close_shell(&host, cx);
}

#[gpui::test]
async fn caret_never_lands_inside_the_table_after_deleting_a_row(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "删除后 caret", ROWS_SOURCE);

    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);

    // 先进入单元格编辑再 Enter 提交（无改动）：caret 落到整表末尾（S3b2 契约）。
    click_cell(TABLE_LINE, 1, 0, &view, cx);
    cx.simulate_keystrokes("f2");
    cx.run_until_parked();
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    let table = table_of(&view, cx);
    let before_end = {
        let source = source_of(&view, cx);
        menghunote_editor::SourceByte::new(table.span.1)
            .to_scalar(&source)
            .expect("表格末尾是 scalar 边界")
            .get()
    };
    assert_eq!(
        caret_of(&view, cx),
        (before_end, before_end),
        "提交后 caret 在整表末尾"
    );

    // 退出单元格编辑的那一帧，浮层还按“表格被压成一行”的旧几何定位：动一下鼠标
    // （真实用户总会动一下）让浮层拿到表格重新变成 Object 的几何。
    let rect = cell_rect(TABLE_LINE, 1, 0, &view, cx);
    cx.simulate_mouse_move(rect.center(), None, Modifiers::default());
    cx.run_until_parked();

    // 删除选中的那条数据行：caret 必须跟着到新表格末尾（绝不落进表格内部）。
    assert_eq!(selected_cell(&view, cx), Some((TABLE_LINE, 1, 0)));
    click_selector(cx, "table-delete-row");
    let table = table_of(&view, cx);
    let source = source_of(&view, cx);
    let new_end = menghunote_editor::SourceByte::new(table.span.1)
        .to_scalar(&source)
        .expect("新表格末尾是 scalar 边界")
        .get();
    assert_eq!(
        caret_of(&view, cx),
        (new_end, new_end),
        "删除后 caret 必须停在新表格末尾（不能落进表格源码内部）"
    );
    assert_eq!(table.rows, 2);
    assert!(table.display.is_some() && !table.editing_source);

    close_shell(&host, cx);
}
