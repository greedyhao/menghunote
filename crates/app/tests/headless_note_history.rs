//! ADR 0024 本机历史的 headless 端到端门禁（真实 `AppShell` + 临时 vault，不 sleep）。
//!
//! 覆盖：
//!
//! 1. 编辑后推进 2 秒（`cx.executor().advance_clock`）→ versions 有行；再次 2 秒
//!    无变化不新增；
//! 2. 右键笔记能看到「历史记录…」菜单并打开覆盖层（空态 selector）；
//! 3. 点击版本行 → 右侧只读预览该版本正文；
//! 4. 恢复为新笔记：新笔记存在、正文等于历史正文、原标题/原正文不变；
//! 5. 切换笔记前 flush 当前版本（不依赖 2 秒 timer）。
//!
//! 纪律（AGENTS.md §6）：临时目录一律建在 `CARGO_TARGET_TMPDIR` 下；
//! 唯一的收敛点是 `run_until_parked()`，不 `sleep`、不依赖系统时间。
//! 像素/DPI/真实窗口仍需 Windows 真机验收。

use std::path::Path;
use std::time::Duration;

use gpui::{
    div, AppContext as _, Entity, InteractiveElement as _, Modifiers, MouseButton,
    ParentElement as _, Render, Styled as _, TestAppContext, VisualTestContext,
};
use menghunote_app::note_history::NoteHistoryPanel;
use menghunote_app::shell::AppShell;
use menghunote_storage::{
    append_updates, list_note_versions, list_tree_sorted, load_body_doc, StructureWriter, TreeSort,
    UpdateRecord, Vault,
};
use tempfile::{Builder as TempDirBuilder, TempDir};

/// 窗口根视图（给 `AppShell` 一个稳定 selector）。
struct ShellHost {
    shell: Entity<AppShell>,
}

impl Render for ShellHost {
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

fn temp_vault(prefix: &str) -> (TempDir, Vault) {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "headless-note-history-{prefix}-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).expect("创建 target/tmp 下的测试根目录失败");
    let dir = TempDirBuilder::new()
        .prefix("vault-")
        .tempdir_in(&root)
        .expect("在 target/tmp 下创建临时目录失败");
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    (dir, vault)
}

/// 正文 A 事务：把 `text` 写进已存在的 body doc（与其它 headless 测试同一条路径）。
fn write_body(vault: &Vault, body_doc_id: &str, text: &str) {
    let (doc, head) = load_body_doc(vault, body_doc_id, 1, None).expect("加载正文失败");
    doc.insert_scalars(0, text).expect("写入正文失败");
    let base = head.version_vector().expect("解码 VV 失败");
    let update = doc.pending_update(&base).expect("导出增量失败");
    append_updates(
        vault,
        body_doc_id,
        head.head_seq,
        &[UpdateRecord::new("headless-note-history", update)],
    )
    .expect("A 事务写入正文失败");
}

/// 建一篇普通笔记（标题 + 正文），返回 node_id。
fn seed_note(vault: &Vault, title: &str, text: &str) -> String {
    let mut writer = StructureWriter::open(vault, 1).expect("打开结构写入口失败");
    let seed = writer
        .doc()
        .create_note(title, 1)
        .expect("结构 CRDT 新建笔记失败");
    writer
        .commit("headless-note-history-seed")
        .expect("结构提交失败");
    write_body(vault, &seed.body_doc_id, text);
    seed.node_id
}

fn open_shell(
    cx: &mut TestAppContext,
    vault: Vault,
) -> (Entity<ShellHost>, &mut VisualTestContext) {
    let (host, cx) = cx.add_window_view(move |_, cx| {
        let shell = cx.new(|cx| AppShell::new(vault, "headless-note-history".to_owned(), cx));
        ShellHost { shell }
    });
    cx.run_until_parked();
    (host, cx)
}

fn shell_of(host: &Entity<ShellHost>, cx: &mut VisualTestContext) -> Entity<AppShell> {
    cx.update(|_window, app| host.read(app).shell.clone())
}

fn active_note_id(host: &Entity<ShellHost>, cx: &mut VisualTestContext) -> String {
    cx.update(|_window, app| {
        host.read(app)
            .shell
            .read(app)
            .active_note_id()
            .expect("打开笔记后必须有 active note")
            .to_owned()
    })
}

fn active_view(
    host: &Entity<ShellHost>,
    cx: &mut VisualTestContext,
) -> Entity<menghunote_app::editor::view::EditorView> {
    cx.update(|_window, app| {
        host.read(app)
            .shell
            .read(app)
            .active_editor_view()
            .expect("打开笔记后必须有活动编辑器视图")
    })
}

fn source_of(
    view: &Entity<menghunote_app::editor::view::EditorView>,
    cx: &mut VisualTestContext,
) -> String {
    cx.update(|_window, app| view.read(app).source_text())
}

fn click_selector(cx: &mut VisualTestContext, selector: &'static str) {
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("`{selector}` 必须可见"));
    cx.simulate_click(bounds.center(), Modifiers::default());
    cx.run_until_parked();
}

fn right_click_selector(cx: &mut VisualTestContext, selector: &'static str) {
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("`{selector}` 必须可见"));
    cx.simulate_mouse_down(bounds.center(), MouseButton::Right, Modifiers::default());
    cx.run_until_parked();
}

/// 点正文并 `Ctrl+Home`，让后续输入落在文档开头（与菜单位置无关）。
fn focus_editor_start(cx: &mut VisualTestContext) {
    let pane = cx
        .debug_bounds("editor-pane")
        .expect("右侧编辑器面板必须画出 selector=editor-pane");
    cx.simulate_click(pane.center(), Modifiers::default());
    cx.run_until_parked();
    cx.simulate_keystrokes("ctrl-home");
    cx.run_until_parked();
}

fn type_text(cx: &mut VisualTestContext, text: &str) {
    cx.simulate_input(text);
    cx.run_until_parked();
}

/// 推进一个 2 秒捕获周期（不 sleep）。
fn advance_capture_tick(cx: &mut VisualTestContext) {
    cx.executor().advance_clock(Duration::from_secs(2));
    cx.run_until_parked();
}

fn versions(vault: &Vault, note_id: &str) -> Vec<(i64, String, String)> {
    list_note_versions(vault, note_id, 200)
        .expect("读取历史版本失败")
        .into_iter()
        .map(|version| (version.version_id, version.title, version.source))
        .collect()
}

fn history_panel(host: &Entity<ShellHost>, cx: &mut VisualTestContext) -> Entity<NoteHistoryPanel> {
    cx.update(|_window, app| {
        host.read(app)
            .shell
            .read(app)
            .note_history_entity()
            .expect("历史记录覆盖层必须打开")
    })
}

#[gpui::test]
async fn edit_then_two_seconds_captures_one_version_without_duplicates(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("capture");
    seed_note(&vault, "捕获测试", "初始正文\n");

    let (host, cx) = open_shell(cx, vault.clone());
    let note_id = active_note_id(&host, cx);
    let view = active_view(&host, cx);
    assert!(
        versions(&vault, &note_id).is_empty(),
        "首次打开不得立刻写版本"
    );

    focus_editor_start(cx);
    type_text(cx, "追加一行");
    let changed = source_of(&view, cx);
    assert!(
        changed.contains("追加一行"),
        "输入必须真的进正文：{changed:?}"
    );

    // 没到 2 秒：仍然没有版本。
    assert!(
        versions(&vault, &note_id).is_empty(),
        "2 秒收敛点之前不应写版本（这是 timer 的职责）"
    );

    advance_capture_tick(cx);
    let captured = list_note_versions(&vault, &note_id, 200).expect("读取版本");
    assert_eq!(captured.len(), 1, "停止输入 2 秒后必须恰好捕获一个版本");
    assert_eq!(captured[0].title, "捕获测试");
    assert_eq!(captured[0].source, "local");
    let body = menghunote_storage::load_note_version(&vault, captured[0].version_id)
        .expect("读版本正文")
        .expect("版本必须存在")
        .body;
    assert_eq!(body, changed, "历史版本正文必须等于捕获时的源文");

    // 源文没变：再推进 2 秒不新增。
    advance_capture_tick(cx);
    assert_eq!(
        versions(&vault, &note_id).len(),
        1,
        "同一篇正文不得重复写版本"
    );

    // 再改一次 + 2 秒：追加第二个版本。
    type_text(cx, "第二段");
    advance_capture_tick(cx);
    assert_eq!(versions(&vault, &note_id).len(), 2, "正文变化后追加新版本");
}

#[gpui::test]
async fn right_click_note_opens_history_panel_and_escape_closes(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("menu");
    seed_note(&vault, "菜单测试", "正文\n");

    let (_host, cx) = open_shell(cx, vault);

    right_click_selector(cx, "tree-row-0");
    assert!(
        cx.debug_bounds("context-menu").is_some(),
        "右键笔记必须弹出菜单"
    );
    assert!(
        cx.debug_bounds("menu-item-1").is_some(),
        "笔记菜单第 2 项（历史记录）必须画出"
    );
    assert!(
        cx.debug_bounds("menu-item-history").is_some(),
        "历史动作必须有可读 selector `menu-item-history`"
    );

    click_selector(cx, "menu-item-history");
    assert!(
        cx.debug_bounds("context-menu").is_none(),
        "点菜单项后右键菜单必须关闭"
    );
    assert!(
        cx.debug_bounds("note-history-panel").is_some(),
        "「历史记录…」必须打开时间线覆盖层"
    );
    assert!(
        cx.debug_bounds("note-history-empty").is_some(),
        "还没捕获版本时必须显示空态"
    );
    assert!(cx.debug_bounds("note-history-preview").is_some());
    assert!(cx.debug_bounds("note-history-restore").is_some());
    assert!(cx.debug_bounds("note-history-close").is_some());

    // Esc 由面板自己的 focus handle 接收。
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("note-history-panel").is_none(),
        "Esc 必须关闭历史覆盖层"
    );
}

#[gpui::test]
async fn clicking_a_version_row_previews_its_body(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("preview");
    seed_note(&vault, "预览测试", "旧正文\n");

    let (host, cx) = open_shell(cx, vault.clone());
    let note_id = active_note_id(&host, cx);
    let view = active_view(&host, cx);

    focus_editor_start(cx);
    type_text(cx, "历史版本内容");
    let captured_source = source_of(&view, cx);
    advance_capture_tick(cx);
    assert_eq!(versions(&vault, &note_id).len(), 1);

    // 捕获后再改正文：当前源文与历史版本不同，预览必须显示**旧**版本。
    type_text(cx, "当前新增内容");
    let current_source = source_of(&view, cx);
    assert_ne!(current_source, captured_source);

    right_click_selector(cx, "tree-row-0");
    click_selector(cx, "menu-item-history");
    let panel = history_panel(&host, cx);
    assert_eq!(
        cx.update(|_window, app| panel.read(app).version_count()),
        1,
        "面板必须列出已捕获的版本"
    );

    click_selector(cx, "note-history-row-0");
    assert_eq!(
        cx.update(|_window, app| panel.read(app).selected_index()),
        Some(0)
    );
    let preview = cx
        .update(|_window, app| panel.read(app).preview_body().map(str::to_owned))
        .expect("点击版本行后必须有预览正文");
    assert_eq!(preview, captured_source, "预览必须是该历史版本的正文");
    assert_ne!(preview, current_source, "预览不得跟随当前正文");
}

#[gpui::test]
async fn restore_creates_a_new_note_and_leaves_the_original_untouched(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("restore");
    seed_note(&vault, "原始笔记", "原始正文\n");

    let (host, cx) = open_shell(cx, vault.clone());
    let original_note_id = active_note_id(&host, cx);
    let original_view = active_view(&host, cx);
    let original_tree = list_tree_sorted(&vault, TreeSort::NameAsc).expect("读结构树");
    let original_entry = original_tree
        .iter()
        .find(|entry| entry.note_id.as_deref() == Some(original_note_id.as_str()))
        .cloned()
        .expect("原始笔记必须在树里");
    let original_body_doc = original_entry
        .body_doc_id
        .clone()
        .expect("原始笔记必须有 body_doc_id");
    let original_title = original_entry.title.clone();

    // 历史版本：先写一段并捕获。
    focus_editor_start(cx);
    type_text(cx, "历史版本甲");
    let historical_body = source_of(&original_view, cx);
    advance_capture_tick(cx);
    assert_eq!(versions(&vault, &original_note_id).len(), 1);

    // 捕获后继续改当前正文（未捕获），恢复时不得用它覆盖历史正文。
    type_text(cx, "当前版本乙");
    let current_source = source_of(&original_view, cx);
    assert_ne!(current_source, historical_body);

    right_click_selector(cx, "tree-row-0");
    click_selector(cx, "menu-item-history");
    let panel = history_panel(&host, cx);
    click_selector(cx, "note-history-row-0");
    let preview = cx
        .update(|_window, app| panel.read(app).preview_body().map(str::to_owned))
        .expect("恢复前必须有预览正文");
    assert_eq!(preview, historical_body);

    click_selector(cx, "note-history-restore");
    assert!(
        cx.debug_bounds("note-history-panel").is_none(),
        "恢复后必须关闭历史覆盖层"
    );

    // 新笔记：标题唯一（原标题 +（历史）），正文等于历史正文。
    let new_note_id = active_note_id(&host, cx);
    assert_ne!(new_note_id, original_note_id, "恢复必须打开新建的笔记");
    let new_title = cx.update(|_window, app| {
        host.read(app)
            .shell
            .read(app)
            .active_note_title()
            .expect("新笔记必须有标题")
    });
    assert_eq!(new_title, format!("{original_title}（历史）"));
    let new_view = active_view(&host, cx);
    assert_eq!(
        source_of(&new_view, cx),
        historical_body,
        "新笔记正文必须等于历史版本正文"
    );

    // 关窗排空后从库里复核：新笔记正文写入；原标题/原正文不变。
    let shell = shell_of(&host, cx);
    cx.update(|_window, app| shell.update(app, |shell, cx| shell.close_all(cx)));
    cx.run_until_parked();

    let tree = list_tree_sorted(&vault, TreeSort::NameAsc).expect("读结构树");
    assert_eq!(
        tree.iter().filter(|entry| entry.is_note()).count(),
        2,
        "恢复只应新增一篇根级笔记"
    );
    let original_after = tree
        .iter()
        .find(|entry| entry.note_id.as_deref() == Some(original_note_id.as_str()))
        .expect("原笔记不能被删除/覆盖");
    assert_eq!(original_after.title, original_title, "原标题不变");
    let (original_body, _) =
        load_body_doc(&vault, &original_body_doc, 1, None).expect("重放原正文失败");
    assert_eq!(
        original_body.text(),
        current_source,
        "原正文必须仍是恢复前的当前正文"
    );

    let new_entry = tree
        .iter()
        .find(|entry| entry.note_id.as_deref() == Some(new_note_id.as_str()))
        .expect("新笔记必须在树里");
    let new_body_doc = new_entry.body_doc_id.clone().expect("新笔记 body_doc_id");
    let (new_body, _) = load_body_doc(&vault, &new_body_doc, 1, None).expect("重放新正文失败");
    assert_eq!(new_body.text(), historical_body, "新笔记的持久化正文");
}

#[gpui::test]
async fn switching_notes_flushes_the_current_version_without_waiting_for_timer(
    cx: &mut TestAppContext,
) {
    let (_dir, vault) = temp_vault("switch");
    // 侧栏是「目录优先 + 文件名 A-Z」：两篇根级笔记按标题排序。
    seed_note(&vault, "A 笔记", "A 正文\n");
    seed_note(&vault, "B 笔记", "B 正文\n");

    let (host, cx) = open_shell(cx, vault.clone());
    let first_note_id = active_note_id(&host, cx);
    let first_view = active_view(&host, cx);

    focus_editor_start(cx);
    type_text(cx, "切换前写入");
    let pending_source = source_of(&first_view, cx);
    assert!(
        versions(&vault, &first_note_id).is_empty(),
        "未推进 2 秒时还没有版本（flush 由切换触发）"
    );

    // 点另一行切换（不推进 clock）：open_entry 必须在切换前 flush。
    let tree = list_tree_sorted(&vault, TreeSort::NameAsc).expect("读结构树");
    let other_row = tree
        .iter()
        .filter(|entry| entry.is_note())
        .position(|entry| entry.note_id.as_deref() != Some(first_note_id.as_str()))
        .expect("必须还有第二篇笔记");
    let selector: &'static str = Box::leak(format!("tree-row-{other_row}").into_boxed_str());
    click_selector(cx, selector);

    assert_ne!(
        active_note_id(&host, cx),
        first_note_id,
        "点击另一篇必须切换 active"
    );
    let captured = list_note_versions(&vault, &first_note_id, 200).expect("读取版本");
    assert_eq!(captured.len(), 1, "切换笔记前必须 flush 一个版本");
    let body = menghunote_storage::load_note_version(&vault, captured[0].version_id)
        .expect("读版本正文")
        .expect("版本存在")
        .body;
    assert_eq!(body, pending_source, "flush 的正文必须是切换前的源文");
}

#[gpui::test]
async fn closing_the_shell_flushes_the_current_version(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("close");
    seed_note(&vault, "关窗测试", "初始正文");

    let (host, cx) = open_shell(cx, vault.clone());
    let note_id = active_note_id(&host, cx);
    let view = active_view(&host, cx);

    focus_editor_start(cx);
    type_text(cx, "关窗前写入");
    let expected = source_of(&view, cx);
    assert!(versions(&vault, &note_id).is_empty());

    // 生产关窗路径（main.rs / AppRoot Main）都会先调 close_all 做有界排空。
    let shell = shell_of(&host, cx);
    cx.update(|_window, app| shell.update(app, |shell, cx| shell.close_all(cx)));
    cx.run_until_parked();

    let captured = list_note_versions(&vault, &note_id, 200).expect("读取版本");
    assert_eq!(captured.len(), 1, "关窗前必须 flush 当前版本");
    let body = menghunote_storage::load_note_version(&vault, captured[0].version_id)
        .expect("读版本正文")
        .expect("版本存在")
        .body;
    assert_eq!(body, expected, "关窗 flush 的正文必须是关闭前的源文");

    // 再次 close_all（幂等）：body_hash 去重，不新增第二行。
    let shell = shell_of(&host, cx);
    cx.update(|_window, app| shell.update(app, |shell, cx| shell.close_all(cx)));
    cx.run_until_parked();
    assert_eq!(
        versions(&vault, &note_id).len(),
        1,
        "重复关闭不得重复写版本"
    );
}
