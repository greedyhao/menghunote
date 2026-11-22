//! ADR 0019 S3b：app headless「表格加行 / 加列」集成测试。
//!
//! 真实 `AppShell` + 临时 vault（`CARGO_TARGET_TMPDIR`、不 sleep）+ 真实 `#[gpui::test]`
//! 窗口：断言
//!
//! 1. 选中/悬浮整块表格时，右下角出现「+ 行 / + 列」浮层；浮层整体在表格**外面**
//!    （不遮挡单元格文本），图片没有这个浮层，进入源码模式也不显示；
//! 2. 点「+ 行」→ 源码只多一条空数据行（逐字节）、列数不变、新行 cell 为空、
//!    分隔行与已有行一个字节不变；点「+ 列」→ 表头/分隔行/所有数据行都多一列；
//! 3. 点按钮**不移动 caret/选区**（按文本位置映射，caret 不会落进表格内部）、
//!    表格仍保持 Object 模式与选中态，几何按新的行列数刷新；
//! 4. 一次点击 = 一个撤销组（一次 `Ctrl+Z` 恢复原表，`Ctrl+Shift+Z` 重做）；
//! 5. 重开 vault 后插入的行列都在，表格仍投影成对象；
//! 6. 只悬浮（不点选）也能加行，且 caret 完全不动；窄窗口浮层仍夹在编辑区内。
//!
//! 像素级外观（DPI、真实窗口、按钮观感）仍必须真机验证；这里证明“几何 → 点击 →
//! 一个 `EditRequest` → 源码/undo/持久化”链路。

use std::path::Path;

use gpui::{
    div, px, size, AppContext as _, Entity, InteractiveElement as _, Modifiers, ParentElement as _,
    Styled as _, TestAppContext, VisualTestContext,
};
use menghunote_app::editor::theme;
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
        .join(format!("headless-table-edit-{}", std::process::id()));
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
        .commit("headless-table-edit-seed")
        .expect("结构提交失败");

    let (doc, head) = load_body_doc(vault, &seed.body_doc_id, 1, None).expect("加载正文文档失败");
    doc.insert_scalars(0, text).expect("写入正文失败");
    let base = head.version_vector().expect("解码 head VV 失败");
    let update = doc.pending_update(&base).expect("导出正文增量失败");
    append_updates(
        vault,
        &seed.body_doc_id,
        head.head_seq,
        &[UpdateRecord::new("headless-table-edit", update)],
    )
    .expect("A 事务写入正文失败");
    seed.body_doc_id
}

fn open_shell(
    cx: &mut TestAppContext,
    vault: Vault,
) -> (Entity<ShellHost>, &mut VisualTestContext) {
    let (host, cx) = cx.add_window_view(move |_, cx| {
        let shell = cx.new(|cx| AppShell::new(vault, "headless-table-edit".to_owned(), cx));
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

/// 表格对象的窗口矩形（不可见直接失败，避免“点空”假通过）。
fn object_rect(
    line: usize,
    view: &Entity<EditorView>,
    cx: &mut VisualTestContext,
) -> gpui::Bounds<gpui::Pixels> {
    cx.update(|_window, app| {
        view.read(app)
            .object_rects()
            .into_iter()
            .find(|(object_line, _)| *object_line == line)
            .map(|(_, rect)| rect)
            .unwrap_or_else(|| panic!("对象行 {line} 必须可见"))
    })
}

/// 单击表格中心并**退出**原地编辑：表格保持选中，浮层可用（编辑期间浮层隐藏）。
fn select_table(line: usize, view: &Entity<EditorView>, cx: &mut VisualTestContext) {
    let rect = object_rect(line, view, cx);
    cx.simulate_click(rect.center(), Modifiers::default());
    cx.run_until_parked();
    cx.simulate_keystrokes("escape");
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

/// 2 列 / 1 行数据 + 两种对齐的表格。
const TABLE_SOURCE: &str = "# 表格\n\n| 名称 | 数量 |\n|:--|--:|\n| 苹果 | 3 |\n\n后文\n";
/// 表格表头行号（0 起）。
const TABLE_LINE: usize = 2;
/// 加一条空数据行之后（逐字节）。
const TABLE_ROW_ADDED: &str =
    "# 表格\n\n| 名称 | 数量 |\n|:--|--:|\n| 苹果 | 3 |\n|  |  |\n\n后文\n";
/// 再加一列之后（逐字节：表头/分隔行/两条数据行各追加一列）。
const TABLE_ROW_AND_COLUMN_ADDED: &str =
    "# 表格\n\n| 名称 | 数量 | |\n|:--|--:| ---|\n| 苹果 | 3 | |\n|  |  | |\n\n后文\n";

#[gpui::test]
async fn corner_buttons_add_a_row_then_a_column(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "加行加列", TABLE_SOURCE);

    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);
    let placeholder = menghunote_editor::OBJECT_PLACEHOLDER;

    // 没有悬浮/选中：不显示边缘浮层（“只在实际需要时显示”）。
    assert!(
        cx.debug_bounds("table-actions").is_none(),
        "未悬浮/未选中时不得显示加行加列浮层"
    );
    assert!(cx.debug_bounds("table-insert-row").is_none());

    // 单击表格（落在某个单元格上）：表格是当前选中对象，同时**直接进入原地编辑**
    // （ADR 0019 S3b2 的 2026-11-17 交互修正）。编辑期间表格仍是 Object，
    // 但工具条/加行加列浮层必须隐藏（避免遮挡 cell 输入框/坐标冲突）。
    let rect = object_rect(TABLE_LINE, &view, cx);
    cx.simulate_click(rect.center(), Modifiers::default());
    cx.run_until_parked();
    let table = table_of(&view, cx);
    assert!(table.selected, "单击后表格必须处于选中态");
    assert_eq!((table.rows, table.columns), (1, 2));
    assert!(!table.editing_source && table.display.is_some());
    assert!(table.editing_cell.is_some(), "单击单元格直接进入原地编辑");
    assert!(
        cx.debug_bounds("table-actions").is_none(),
        "单元格编辑期间不得显示加行加列浮层"
    );
    assert_eq!(caret_of(&view, cx), (0, 0), "单击单元格不移动正文 caret");

    // Esc 取消这次编辑（没输入 → 零请求），选中态保留 → 浮层重新出现。
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(
        !cx.update(|_window, app| view.read(app).is_editing_table_cell()),
        "Esc 退出单元格编辑"
    );
    assert_eq!(caret_of(&view, cx), (0, 0), "取消不移动 caret");
    let actions = cx
        .debug_bounds("table-actions")
        .expect("选中表格必须显示右下角浮层");
    let rect = object_rect(TABLE_LINE, &view, cx);
    assert!(
        actions.origin.y >= rect.origin.y + rect.size.height,
        "浮层必须在表格底边之下（不得遮挡单元格文本）：actions={actions:?} table={rect:?}"
    );
    assert!(
        actions.origin.x.as_f32() >= rect.origin.x.as_f32() - 0.5,
        "浮层必须贴住表格右边缘：actions={actions:?} table={rect:?}"
    );
    assert!(
        (actions.origin.x + actions.size.width).as_f32()
            <= (rect.origin.x + rect.size.width).as_f32() + 0.5,
        "浮层不得超出表格右边缘：actions={actions:?} table={rect:?}"
    );
    assert!(
        cx.debug_bounds("table-insert-row").is_some(),
        "必须有「+ 行」"
    );
    assert!(
        cx.debug_bounds("table-insert-column").is_some(),
        "必须有「+ 列」"
    );

    // 悬浮/工具条按**上一帧几何**定位：退出编辑后动一下鼠标，让浮层拿到最新几何。
    let rect = object_rect(TABLE_LINE, &view, cx);
    cx.simulate_mouse_move(rect.center(), None, Modifiers::default());
    cx.run_until_parked();

    // ---- 点「+ 行」：只追加一条空数据行 ----
    click_selector(cx, "table-insert-row");
    assert_eq!(
        source_of(&view, cx),
        TABLE_ROW_ADDED,
        "加行只能追加一条空数据行（其余字节逐字不变）"
    );
    let table = table_of(&view, cx);
    assert_eq!((table.rows, table.columns), (2, 2), "行数 +1、列数不变");
    assert!(table.selected, "加行后表格必须保持选中态");
    assert!(
        !table.editing_source && table.display.is_some(),
        "加行后表格仍是 Object（不得进入源码模式）"
    );
    let display = display_of(&view, cx);
    assert!(
        display.contains(placeholder) && !display.contains("| 苹果 |"),
        "加行后仍投影成对象占位符：{display:?}"
    );
    // caret：点浮层按钮不移动正文 caret（本用例 caret 一直在文档开头）。
    assert_eq!(
        caret_of(&view, cx),
        (0, 0),
        "点浮层按钮不移动正文 caret（更不能落进表格内部）"
    );
    // 几何按新行数刷新：矩形高度 +1 行。
    let grown = object_rect(TABLE_LINE, &view, cx);
    assert!(
        (grown.size.height.as_f32() - (rect.size.height.as_f32() + theme::TABLE_ROW_HEIGHT)).abs()
            < 0.5,
        "加行后表格矩形必须长高一行：before={rect:?} after={grown:?}"
    );
    let painted = cx.update(|_window, app| view.read(app).table_paint_rows());
    assert_eq!(
        painted,
        vec![(TABLE_LINE, true, 2)],
        "加行后必须绘制表头 + 2 条数据行"
    );

    // ---- 点「+ 列」：表头 / 分隔行 / 所有数据行都多一列 ----
    click_selector(cx, "table-insert-column");
    assert_eq!(
        source_of(&view, cx),
        TABLE_ROW_AND_COLUMN_ADDED,
        "加列必须在表头/分隔行/每条数据行末尾各追加一列"
    );
    let table = table_of(&view, cx);
    assert_eq!((table.rows, table.columns), (2, 3), "列数 +1、行数不变");
    assert!(table.selected, "加列后表格必须保持选中态");
    assert!(!table.editing_source);
    assert_eq!(
        caret_of(&view, cx),
        (0, 0),
        "加列后正文 caret 仍然不动（表格内部不接受文本落点）"
    );
    let display = display_of(&view, cx);
    assert!(
        display.contains(placeholder) && !display.contains("| 名称 |"),
        "加列后仍投影成对象占位符：{display:?}"
    );
    // 工具条仍可用（S3a 行为不回退）。
    assert!(cx.debug_bounds("object-toolbar-edit").is_some());
    assert!(cx.debug_bounds("table-actions").is_some());

    close_shell(&host, cx);
}

#[gpui::test]
async fn hovering_a_table_shows_the_buttons_without_touching_the_caret(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "悬浮加行", TABLE_SOURCE);

    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);
    // 打开笔记时 caret 在文档开头；全程不点正文。
    assert_eq!(caret_of(&view, cx), (0, 0));

    // 悬浮表格（不点选）→ 右下角浮层出现，表格仍是未选中态。
    let rect = object_rect(TABLE_LINE, &view, cx);
    cx.simulate_mouse_move(rect.center(), None, Modifiers::default());
    cx.run_until_parked();
    assert!(
        !table_of(&view, cx).selected,
        "悬浮不得把表格变成选中态（选中只由单击/选区产生）"
    );
    assert!(
        cx.debug_bounds("table-insert-row").is_some(),
        "悬浮表格也必须显示「+ 行」（Obsidian 式）"
    );

    // 点「+ 行」：加行成功，caret 仍停在文档开头（点浮层绝不落正文 caret）。
    click_selector(cx, "table-insert-row");
    assert_eq!(source_of(&view, cx), TABLE_ROW_ADDED);
    assert_eq!(
        caret_of(&view, cx),
        (0, 0),
        "点击浮层按钮不得移动 caret（悬浮态更没有理由选中表格）"
    );
    assert!(!table_of(&view, cx).selected, "点浮层按钮不改变表格选中态");
    close_shell(&host, cx);
}

#[gpui::test]
async fn one_click_is_one_undo_group(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "撤销加行", TABLE_SOURCE);

    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);
    select_table(TABLE_LINE, &view, cx);

    click_selector(cx, "table-insert-row");
    assert_eq!(source_of(&view, cx), TABLE_ROW_ADDED);

    // 一次点击 = 一个撤销组：一次 Ctrl+Z 就回到原表。
    cx.simulate_keystrokes("ctrl-z");
    cx.run_until_parked();
    assert_eq!(
        source_of(&view, cx),
        TABLE_SOURCE,
        "一次撤销必须恢复整张表（文件名/分隔行/数据行都不留残片）"
    );
    let table = table_of(&view, cx);
    assert_eq!((table.rows, table.columns), (1, 2), "撤销后行数回到 1");

    // 重做恢复同一条空行。
    cx.simulate_keystrokes("ctrl-shift-z");
    cx.run_until_parked();
    assert_eq!(source_of(&view, cx), TABLE_ROW_ADDED, "重做恢复空数据行");

    // 加列同样是一个撤销组。
    select_table(TABLE_LINE, &view, cx);
    click_selector(cx, "table-insert-column");
    assert_eq!(source_of(&view, cx), TABLE_ROW_AND_COLUMN_ADDED);
    cx.simulate_keystrokes("ctrl-z");
    cx.run_until_parked();
    assert_eq!(
        source_of(&view, cx),
        TABLE_ROW_ADDED,
        "一次撤销必须把整列（表头/分隔行/所有数据行）一起撤回"
    );
    assert_eq!(table_of(&view, cx).columns, 2);

    close_shell(&host, cx);
}

#[gpui::test]
async fn inserted_rows_and_columns_survive_reopen(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault_path = dir.path().join("vault.sqlite3");
    let vault = Vault::open(&vault_path).expect("打开临时 vault");
    let doc_id = seed_note(&vault, "持久化加行加列", TABLE_SOURCE);

    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);
    select_table(TABLE_LINE, &view, cx);
    click_selector(cx, "table-insert-row");
    // 浮层几何在加行后变了，重新取一次再点「+ 列」。
    click_selector(cx, "table-insert-column");
    assert_eq!(source_of(&view, cx), TABLE_ROW_AND_COLUMN_ADDED);
    close_shell(&host, cx);

    // 1) durable 后正文逐字正确。
    let persisted = {
        let vault = Vault::open(&vault_path).expect("重开 vault");
        let (doc, _head) = load_body_doc(&vault, &doc_id, 1, None).expect("重放正文失败");
        doc.text().to_owned()
    };
    assert_eq!(
        persisted, TABLE_ROW_AND_COLUMN_ADDED,
        "落库后必须保留新增的行与列"
    );

    // 2) 重新打开 shell（新 Vault / 新视图）：表格仍是对象，行列数来自新 source。
    let vault = Vault::open(&vault_path).expect("再次打开 vault");
    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);
    let table = table_of(&view, cx);
    assert_eq!(
        (table.line, table.rows, table.columns),
        (TABLE_LINE, 2, 3),
        "重开后表格行列数必须与源码一致"
    );
    assert_eq!(
        table.end_line,
        TABLE_LINE + 3,
        "重开后表格跨表头/分隔行 + 2 条数据行"
    );
    assert!(table.display.is_some(), "重开后表格仍是 Object");
    let display = display_of(&view, cx);
    assert!(
        display.contains(menghunote_editor::OBJECT_PLACEHOLDER) && !display.contains("|  |  | |"),
        "重开后不得直接把表格源码显示出来：{display:?}"
    );
    close_shell(&host, cx);
}

#[gpui::test]
async fn corner_buttons_are_table_only_and_hidden_in_source_mode(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    let text = format!(
        "![图](asset://{})\n\n| a | b |\n|---|---|\n| 1 | 2 |\n",
        "a".repeat(64)
    );
    seed_note(&vault, "图片与表格", &text);

    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);

    // 图片对象：没有加行加列浮层（只对表格出现）。
    let image_rect = object_rect(0, &view, cx);
    cx.simulate_click(image_rect.center(), Modifiers::default());
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("object-toolbar").is_some(),
        "图片必须有自己的工具条"
    );
    assert!(
        cx.debug_bounds("table-actions").is_none() && cx.debug_bounds("table-insert-row").is_none(),
        "图片不得显示表格的加行加列浮层"
    );

    // 表格：浮层出现；进入源码模式后浮层与按钮一起消失（源码模式不是 Object）。
    select_table(2, &view, cx);
    assert!(
        cx.debug_bounds("table-insert-row").is_some(),
        "选中表格后必须有「+ 行」"
    );
    click_selector(cx, "object-toolbar-edit");
    assert!(
        display_of(&view, cx).contains("| a | b |"),
        "工具条「编辑源码」必须仍然可用（S3a 行为不回退）"
    );
    assert!(
        cx.debug_bounds("table-actions").is_none(),
        "源码模式下不得显示加行加列浮层"
    );
    // Esc 回到表格对象：浮层重新可用。
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    select_table(2, &view, cx);
    assert!(
        cx.debug_bounds("table-insert-row").is_some(),
        "退出源码模式后浮层必须恢复"
    );
    close_shell(&host, cx);
}

/// 只压住“浮层不得与表格矩形相交”这条几何契约：窄窗口下浮层必须夹在视口内，
/// 仍然整体位于表格底边之下。
#[gpui::test]
async fn narrow_window_keeps_the_corner_buttons_inside_the_viewport(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "窄窗口加行", TABLE_SOURCE);

    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);
    // 320 = 最左侧图标栏 44 + 侧栏 220 + 编辑区 56（旧基线 320 时编辑区 100）。
    // 要复现"窄但可用"的编辑区，窗口宽度按 ribbon 宽度整体加宽，几何契约不变。
    cx.simulate_resize(size(px(364.0), px(600.0)));
    cx.run_until_parked();
    let rect = object_rect(TABLE_LINE, &view, cx);
    cx.simulate_click(rect.center(), Modifiers::default());
    cx.run_until_parked();
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    let actions = cx
        .debug_bounds("table-actions")
        .expect("窄窗口下浮层仍必须显示");
    let pane = cx
        .debug_bounds("editor-pane")
        .expect("编辑器面板必须画出来");
    assert!(
        actions.origin.x.as_f32() >= pane.origin.x.as_f32() - 0.5
            && (actions.origin.x + actions.size.width).as_f32()
                <= (pane.origin.x + pane.size.width).as_f32() + 0.5,
        "浮层必须夹在编辑区内：actions={actions:?} pane={pane:?}"
    );
    assert!(
        actions.origin.y >= rect.origin.y + rect.size.height,
        "浮层仍必须在表格底边之下：actions={actions:?} table={rect:?}"
    );
    // 点一下：窄窗口也必须能加行（不能只是画出来）。
    click_selector(cx, "table-insert-row");
    assert_eq!(source_of(&view, cx), TABLE_ROW_ADDED);
    close_shell(&host, cx);
}
