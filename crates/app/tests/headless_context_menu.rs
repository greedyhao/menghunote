//! ADR 0019 S3b4：app headless「正文右键 → 插入 → 表格」集成测试。
//!
//! 真实 `AppShell` + 临时 vault（`CARGO_TARGET_TMPDIR`、不 sleep）+ 真实 `#[gpui::test]`
//! 窗口：断言
//!
//! 1. 右键正文弹出「插入 → 表格」菜单：菜单可见，**正文 caret 不动**；`Esc` 关闭、
//!    点击菜单外关闭（都不改正文）；
//! 2. 点「表格」→ 在当前 caret 处插入默认 GFM 表格（2 列 × 1 条数据行、cell 全空），
//!    正文其它字节逐字节不变，一次点击 = 一个撤销组（一次 `Ctrl+Z` 恢复、`Ctrl+Shift+Z`
//!    重做）；
//! 3. 插入后表格按 Object 渲染（display 只有占位符、有对象矩形），caret 在表格之后；
//! 4. 空文档里也能插入，表格落在第 0 行且仍是 Object。
//!
//! 像素级外观（菜单阴影/DPI/真实窗口）仍必须真机验证；这里证明“右键 → 菜单 → 一次
//! EditRequest → 源码/undo/Object 投影”链路。

use std::path::Path;

use gpui::{
    div, point, px, AppContext as _, Entity, InteractiveElement as _, Modifiers, MouseButton,
    ParentElement as _, Styled as _, TestAppContext, VisualTestContext,
};
use menghunote_app::editor::view::EditorView;
use menghunote_app::shell::AppShell;
use menghunote_editor::SourceByte;
use menghunote_storage::{append_updates, load_body_doc, StructureWriter, UpdateRecord, Vault};
use tempfile::{Builder as TempDirBuilder, TempDir};

/// 默认新表格（与 `menghunote_editor::new_table_text("\\n")` 一致）。
const DEFAULT_TABLE: &str = "|  |  |\n| --- | --- |\n|  |  |\n";

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
        .join(format!("headless-context-menu-{}", std::process::id()));
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
        .commit("headless-context-menu-seed")
        .expect("结构提交失败");

    let (doc, head) = load_body_doc(vault, &seed.body_doc_id, 1, None).expect("加载正文文档失败");
    doc.insert_scalars(0, text).expect("写入正文失败");
    let base = head.version_vector().expect("解码 head VV 失败");
    let update = doc.pending_update(&base).expect("导出正文增量失败");
    append_updates(
        vault,
        &seed.body_doc_id,
        head.head_seq,
        &[UpdateRecord::new("headless-context-menu", update)],
    )
    .expect("A 事务写入正文失败");
    seed.body_doc_id
}

fn open_shell(
    cx: &mut TestAppContext,
    vault: Vault,
) -> (Entity<ShellHost>, &mut VisualTestContext) {
    let (host, cx) = cx.add_window_view(move |_, cx| {
        let shell = cx.new(|cx| AppShell::new(vault, "headless-context-menu".to_owned(), cx));
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

/// 点击一个 debug selector 目标（找不到就失败：菜单/菜单项必须真实画出来）。
fn click_selector(cx: &mut VisualTestContext, selector: &'static str) {
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("`{selector}` 必须可见"));
    cx.simulate_click(bounds.center(), Modifiers::default());
    cx.run_until_parked();
}

/// 编辑器面板左上角的正文内一个稳定右键落点（菜单位置只影响面板坐标，不影响 caret）。
fn right_click_point(cx: &mut VisualTestContext) -> gpui::Point<gpui::Pixels> {
    let editor = cx
        .debug_bounds("editor-current-doc")
        .expect("编辑器正文必须画出来");
    point(editor.origin.x + px(80.0), editor.origin.y + px(20.0))
}

/// 正文前 3 行（第 2 行是正文行），以及引用/列表/围栏都不参与。
const NOTE_SOURCE: &str = "开头\n\n正文\n\n结尾\n";
/// 把 caret 的字节位置换算成 scalar（日志/断言用）。
fn scalar_of_byte(source: &str, byte: usize) -> usize {
    SourceByte::new(byte)
        .to_scalar(source)
        .expect("测试用字节必须是 scalar 边界")
        .get()
}

#[gpui::test]
async fn right_click_opens_the_insert_menu_without_moving_the_caret(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "右键菜单", NOTE_SOURCE);

    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);
    assert_eq!(caret_of(&view, cx), (0, 0), "打开笔记时 caret 在文档开头");

    // 右键正文：菜单出现，「插入 → 表格」项可见；caret 一动不动。
    let position = right_click_point(cx);
    cx.simulate_mouse_down(position, MouseButton::Right, Modifiers::default());
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("editor-context-menu").is_some(),
        "右键正文必须弹出菜单"
    );
    assert!(
        cx.debug_bounds("editor-context-insert-table").is_some(),
        "菜单里必须有「表格」项"
    );
    assert_eq!(
        caret_of(&view, cx),
        (0, 0),
        "右键只能决定菜单位置，不能移动正文 caret"
    );
    assert_eq!(source_of(&view, cx), NOTE_SOURCE, "右键不改正文");

    // Esc 关闭菜单，不改正文/caret。
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("editor-context-menu").is_none(),
        "Esc 必须关闭右键菜单"
    );
    assert_eq!(caret_of(&view, cx), (0, 0));
    assert_eq!(source_of(&view, cx), NOTE_SOURCE);

    // 再开一次：点击菜单外关闭（这次点击照常落 caret，不插入任何东西）。
    cx.simulate_mouse_down(position, MouseButton::Right, Modifiers::default());
    cx.run_until_parked();
    assert!(cx.debug_bounds("editor-context-menu").is_some());
    // 菜单位于右键落点（local x≈80），点它的左侧空白处 = 菜单外。
    let editor = cx
        .debug_bounds("editor-current-doc")
        .expect("编辑器正文必须画出来");
    let outside = point(editor.origin.x + px(30.0), editor.origin.y + px(20.0));
    cx.simulate_click(outside, Modifiers::default());
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("editor-context-menu").is_none(),
        "点击菜单外必须关闭菜单"
    );
    assert_eq!(
        source_of(&view, cx),
        NOTE_SOURCE,
        "打开/关闭菜单都不得改正文"
    );

    close_shell(&host, cx);
}

#[gpui::test]
async fn clicking_insert_table_adds_the_default_table_and_keeps_other_bytes(
    cx: &mut TestAppContext,
) {
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "菜单插入表格", NOTE_SOURCE);

    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);

    // 不用鼠标落 caret：下移两行正好到「正文」行首（确定性、与菜单位置无关）。
    cx.simulate_keystrokes("down");
    cx.simulate_keystrokes("down");
    cx.run_until_parked();
    let paragraph_byte = NOTE_SOURCE.find("正文").expect("正文行");
    let caret_before = scalar_of_byte(NOTE_SOURCE, paragraph_byte);
    assert_eq!(
        caret_of(&view, cx),
        (caret_before, caret_before),
        "两次下移后 caret 在正文行首"
    );

    // 右键（落点在别处）→ caret 不动 → 点菜单项插入。
    let position = right_click_point(cx);
    cx.simulate_mouse_down(position, MouseButton::Right, Modifiers::default());
    cx.run_until_parked();
    assert_eq!(
        caret_of(&view, cx),
        (caret_before, caret_before),
        "右键不得移动 caret（插入位置必须还是正文行首）"
    );
    click_selector(cx, "editor-context-insert-table");

    // 逐字节：前缀（开头 + 空行）与后缀（正文/空行/结尾）都不变，只在 caret 处多出
    // “表格 + 一个空行”（前面已有空行，所以不再补）。
    let expected = format!("开头\n\n{DEFAULT_TABLE}\n正文\n\n结尾\n");
    assert_eq!(
        source_of(&view, cx),
        expected,
        "插入表格不得改写正文其它字节（只插入表格与分隔空行）"
    );
    let table = table_of(&view, cx);
    assert_eq!(
        (table.line, table.rows, table.columns),
        (2, 1, 2),
        "默认表格：2 列 × 1 条数据行，落在 caret 处"
    );
    assert!(
        !table.editing_source && table.display.is_some(),
        "插入后表格必须按 Object 渲染（不得进入源码模式）"
    );
    let display = display_of(&view, cx);
    assert!(
        display.contains(menghunote_editor::OBJECT_PLACEHOLDER) && !display.contains("|  |  |"),
        "display 里只能是对象占位符，不能显示表格源码：{display:?}"
    );
    let rects = cx.update(|_window, app| view.read(app).object_rects());
    assert!(
        rects.iter().any(|(line, _)| *line == table.line),
        "插入的表格必须有对象矩形（真实绘制）：{rects:?}"
    );
    // caret 在表格之后（插入文本末尾 = 「正文」行首之前）。
    let caret_after = scalar_of_byte(&expected, expected.find("正文").expect("正文行"));
    assert_eq!(
        caret_of(&view, cx),
        (caret_after, caret_after),
        "插入后 caret 必须在表格之后（不能落进表格源码内部）"
    );

    // 一次点击 = 一个撤销组：一次 Ctrl+Z 恢复原正文，Ctrl+Shift+Z 重做。
    cx.simulate_keystrokes("ctrl-z");
    cx.run_until_parked();
    assert_eq!(source_of(&view, cx), NOTE_SOURCE, "一次撤销恢复原正文");
    assert!(
        cx.update(|_window, app| view.read(app).table_snapshots().is_empty()),
        "撤销后不再有表格对象"
    );
    cx.simulate_keystrokes("ctrl-shift-z");
    cx.run_until_parked();
    assert_eq!(source_of(&view, cx), expected, "重做恢复同一张表");

    close_shell(&host, cx);
}

#[gpui::test]
async fn insert_table_into_an_empty_document_is_an_object_on_line_zero(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "空文档插表", "");

    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);
    assert_eq!(source_of(&view, cx), "");
    let position = right_click_point(cx);
    cx.simulate_mouse_down(position, MouseButton::Right, Modifiers::default());
    cx.run_until_parked();
    click_selector(cx, "editor-context-insert-table");

    assert_eq!(
        source_of(&view, cx),
        DEFAULT_TABLE,
        "空文档插入表格：前后都不需要补空行"
    );
    let table = table_of(&view, cx);
    assert_eq!((table.line, table.rows, table.columns), (0, 1, 2));
    assert!(table.display.is_some() && !table.editing_source);
    assert_eq!(
        caret_of(&view, cx),
        (
            scalar_of_byte(DEFAULT_TABLE, DEFAULT_TABLE.len()),
            scalar_of_byte(DEFAULT_TABLE, DEFAULT_TABLE.len())
        ),
        "caret 落在表格之后（文档末尾）"
    );

    close_shell(&host, cx);
}
