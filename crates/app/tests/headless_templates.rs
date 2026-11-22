//! 第一方「模板」「日记」核心插件的 headless 集成测试（默认构建也跑）。
//!
//! 覆盖（真实 `AppShell` + 临时 vault + `#[gpui::test]` 窗口，**不 sleep**）：
//!
//! 1. 右键「插入 → 模板…」→ 选择器 → 选中后**在 caret 插入**渲染后的正文，
//!    其它字节不变；一次 `Ctrl+Z` 恢复（一个撤销组）；
//! 2. 侧栏「从模板新建…」→ 新笔记标题 = 模板标题（重名加序号，**不覆盖**），
//!    创建后打开且正文已渲染；
//! 3. 模板文件夹不存在/为空时选择器给**明确空态**（不自动创建文件夹）；
//! 4. 「今日日记」：不存在 → 创建目录+笔记（日期格式里的 `/` 生成子目录）、正文用
//!    日记模板渲染；已存在 → 直接打开（不重复创建）；模板丢失 → 非阻塞提示 + 空日记；
//! 5. 插件关闭时隐藏入口（不写正文、不 panic）；
//! 6. 模板正文里的相对日期（`{{date:-1d}}` / `{{date:+1d}}`）按本地今天渲染。
//!
//! 纪律（AGENTS.md §6）：临时目录一律 `CARGO_TARGET_TMPDIR`；收敛点只用
//! `run_until_parked()`；偏好全局状态用 `lock_for_test()` 串行。
//!
//! 像素/DPI 仍必须 Windows 真机验收；这里证明状态机与"一次 EditRequest"链路。

use std::path::Path;

use gpui::{
    div, point, px, AppContext as _, Entity, InteractiveElement as _, Modifiers, MouseButton,
    ParentElement as _, Styled as _, TestAppContext, VisualTestContext,
};
use menghunote_app::daily_notes;
use menghunote_app::editor::view::EditorView;
use menghunote_app::shell::AppShell;
use menghunote_app::templates::{self, DateTimeParts};
use menghunote_storage::{
    append_updates, list_tree, list_tree_sorted, load_body_doc, StructureWriter, TreeSort,
    UpdateRecord, Vault,
};
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

fn temp_vault_dir(prefix: &str) -> TempDir {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "headless-templates-{prefix}-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).expect("创建 target/tmp 失败");
    TempDirBuilder::new()
        .prefix("vault-")
        .tempdir_in(&root)
        .expect("创建临时 vault 失败")
}

fn temp_preferences_dir() -> TempDir {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("headless-templates-prefs-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("创建 target/tmp 失败");
    TempDirBuilder::new()
        .prefix("prefs-")
        .tempdir_in(&root)
        .expect("创建临时偏好目录失败")
}

/// 正文 A 事务：把 `text` 写进已存在的 body doc。
fn write_body(vault: &Vault, body_doc_id: &str, text: &str) {
    let (doc, head) = load_body_doc(vault, body_doc_id, 1, None).expect("加载正文失败");
    doc.insert_scalars(0, text).expect("写入正文失败");
    let base = head.version_vector().expect("解码 VV 失败");
    let update = doc.pending_update(&base).expect("导出增量失败");
    append_updates(
        vault,
        body_doc_id,
        head.head_seq,
        &[UpdateRecord::new("headless-templates", update)],
    )
    .expect("A 事务写入正文失败");
}

/// 在根级建一篇笔记（结构提交 + 正文），返回节点 ID。
fn seed_note(vault: &Vault, title: &str, text: &str) -> String {
    let mut writer = StructureWriter::open(vault, 1).expect("打开结构写入口失败");
    let seed = writer
        .doc()
        .create_note(title, 1)
        .expect("结构 CRDT 新建笔记失败");
    writer
        .commit("headless-templates-seed")
        .expect("结构提交失败");
    write_body(vault, &seed.body_doc_id, text);
    seed.node_id
}

/// 在根级建一个文件夹，返回节点 ID。
fn seed_folder(vault: &Vault, title: &str) -> String {
    let mut writer = StructureWriter::open(vault, 1).expect("打开结构写入口失败");
    let node_id = writer
        .doc()
        .create_folder(title, 1)
        .expect("结构 CRDT 新建目录失败");
    writer
        .commit("headless-templates-folder")
        .expect("结构提交失败");
    node_id
}

/// 在已有文件夹里建一篇笔记（结构提交 + 正文），返回节点 ID。
fn seed_note_in_folder(vault: &Vault, folder: &str, title: &str, text: &str) -> String {
    let mut writer = StructureWriter::open(vault, 1).expect("打开结构写入口失败");
    let seed = writer
        .doc()
        .create_note(title, 1)
        .expect("结构 CRDT 新建笔记失败");
    writer
        .doc()
        .move_node(&seed.node_id, Some(folder), 1)
        .expect("挂到模板文件夹失败");
    writer
        .commit("headless-templates-inner")
        .expect("结构提交失败");
    write_body(vault, &seed.body_doc_id, text);
    seed.node_id
}

fn open_shell(
    cx: &mut TestAppContext,
    vault: Vault,
) -> (Entity<ShellHost>, &mut VisualTestContext) {
    let (host, cx) = cx.add_window_view(move |_, cx| {
        let shell = cx.new(|cx| AppShell::new(vault, "headless-templates".to_owned(), cx));
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
        host.read(app)
            .shell
            .read(app)
            .active_editor_view()
            .expect("打开笔记后必须有活动编辑器视图")
    })
}

fn source_of(view: &Entity<EditorView>, cx: &mut VisualTestContext) -> String {
    cx.update(|_window, app| view.read(app).source_text())
}

fn notice_of(host: &Entity<ShellHost>, cx: &mut VisualTestContext) -> Option<String> {
    cx.update(|_window, app| {
        host.read(app)
            .shell
            .read(app)
            .notice_text()
            .map(str::to_owned)
    })
}

fn click_selector(cx: &mut VisualTestContext, selector: &'static str) {
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("`{selector}` 必须可见"));
    cx.simulate_click(bounds.center(), Modifiers::default());
    cx.run_until_parked();
}

fn right_click_point(cx: &mut VisualTestContext) -> gpui::Point<gpui::Pixels> {
    let pane = cx
        .debug_bounds("editor-current-doc")
        .expect("编辑器面板必须画出来");
    point(pane.origin.x + px(80.0), pane.origin.y + px(20.0))
}

/// 模板选择器里的相对路径列表（渲染与测试共用同一份发现结果）。
fn template_paths(vault: &Vault) -> Vec<String> {
    let tree = list_tree(vault).expect("读树失败");
    templates::collect_templates(&tree, &templates::preferences().folder)
        .items
        .into_iter()
        .map(|item| item.relative_path)
        .collect()
}

/// 侧栏（目录优先 + 文件名 A-Z）里标题对应的先序下标。
fn tree_row(vault: &Vault, title: &str) -> usize {
    list_tree_sorted(vault, TreeSort::NameAsc)
        .expect("读树失败")
        .iter()
        .position(|entry| entry.title == title)
        .unwrap_or_else(|| panic!("结构树里找不到「{title}」"))
}

/// 点侧栏某一行（真实鼠标事件）。启动时打开的是排序后的第一篇笔记，
/// 需要确定性打开某篇时显式点它。
fn click_tree_row(cx: &mut VisualTestContext, index: usize) {
    let selector: &'static str = Box::leak(format!("tree-row-{index}").into_boxed_str());
    let row = cx.debug_bounds(selector).expect("树行必须有 selector");
    cx.simulate_click(row.center(), Modifiers::default());
    cx.run_until_parked();
}

fn note_titles(vault: &Vault) -> Vec<String> {
    list_tree(vault)
        .expect("读树失败")
        .into_iter()
        .filter(|entry| entry.is_note())
        .map(|entry| entry.title)
        .collect()
}

#[gpui::test]
async fn insert_template_from_context_menu_adds_one_undo_step(cx: &mut TestAppContext) {
    let _templates = templates::lock_for_test();
    let _daily = daily_notes::lock_for_test();
    let prefs_dir = temp_preferences_dir();
    _templates.reset(Some(prefs_dir.path().join(templates::TEMPLATES_FILE_NAME)));

    let dir = temp_vault_dir("insert");
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    // 先建普通笔记（AppShell 会自动打开树里第一篇笔记 → 必须确定性）。
    seed_note(&vault, "笔记", "开头\n");
    let folder = seed_folder(&vault, "模板");
    seed_note_in_folder(
        &vault,
        &folder,
        "日记模板",
        "T:{{title}} D:{{date:YYYY/MM/DD}}\n",
    );
    assert_eq!(template_paths(&vault), vec!["日记模板".to_owned()]);

    let (host, cx) = open_shell(cx, vault.clone());
    // 目录优先 + A-Z：启动时打开的是「模板」目录里的模板笔记，显式点开普通笔记。
    click_tree_row(cx, tree_row(&vault, "笔记"));
    let view = active_view(&host, cx);
    assert_eq!(source_of(&view, cx), "开头\n", "打开的是普通笔记");

    // 右键 → 「插入」菜单多出「模板…」一项；右键不移动 caret、不改正文。
    let position = right_click_point(cx);
    cx.simulate_mouse_down(position, MouseButton::Right, Modifiers::default());
    cx.run_until_parked();
    assert!(cx.debug_bounds("editor-context-menu").is_some());
    assert!(
        cx.debug_bounds("editor-context-insert-template").is_some(),
        "模板插件启用时右键菜单必须有「模板…」项"
    );
    assert_eq!(source_of(&view, cx), "开头\n");

    click_selector(cx, "editor-context-insert-template");
    assert!(
        cx.debug_bounds("template-picker").is_some(),
        "菜单项必须真的画出选择器覆盖层"
    );
    assert!(cx.debug_bounds("template-picker-item-0").is_some());
    assert_eq!(source_of(&view, cx), "开头\n", "打开选择器不得改正文");

    #[cfg(feature = "sync-ui")]
    let edit_seq = cx.update(|_window, app| view.read(app).dev_sync_local_edit_seq());

    click_selector(cx, "template-picker-item-0");
    let expected_date = templates::format_datetime(&DateTimeParts::now(), "YYYY/MM/DD");
    // caret 在文档开头（打开笔记时 0），模板插在 caret 处：其余字节逐字节不变。
    assert_eq!(
        source_of(&view, cx),
        format!("T:笔记 D:{expected_date}\n开头\n")
    );
    assert!(
        cx.debug_bounds("template-picker").is_none(),
        "选中后选择器必须关闭"
    );

    // 一次插入 = 一个本地编辑（一次 EditRequest）。
    #[cfg(feature = "sync-ui")]
    assert_eq!(
        cx.update(|_window, app| view.read(app).dev_sync_local_edit_seq()),
        edit_seq + 1,
        "一次插入模板只能产生一个 EditRequest"
    );

    // 一次 Ctrl+Z 恢复（一个撤销组）。
    cx.simulate_keystrokes("ctrl-z");
    cx.run_until_parked();
    assert_eq!(source_of(&view, cx), "开头\n", "一次撤销必须完全恢复");

    // 有选区时：插入必须**替换选区**（不是追加），并且仍是一个撤销组。
    cx.simulate_keystrokes("shift-down");
    cx.run_until_parked();
    let before = source_of(&view, cx);
    let (start, end) = cx.update(|_window, app| {
        let selection = view.read(app).selection();
        assert!(
            selection.anchor() != selection.head(),
            "shift-down 必须产生非空选区（测试前提）"
        );
        (
            selection
                .start()
                .to_source_byte(&before)
                .expect("选区起点必须是 source 边界")
                .get(),
            selection
                .end()
                .to_source_byte(&before)
                .expect("选区终点必须是 source 边界")
                .get(),
        )
    });
    let position = right_click_point(cx);
    cx.simulate_mouse_down(position, MouseButton::Right, Modifiers::default());
    cx.run_until_parked();
    click_selector(cx, "editor-context-insert-template");
    click_selector(cx, "template-picker-item-0");
    let rendered = format!("T:笔记 D:{expected_date}\n");
    let expected = format!("{}{}{}", &before[..start], rendered, &before[end..]);
    assert_eq!(
        source_of(&view, cx),
        expected,
        "有选区时模板必须替换选区（{start}..{end}）"
    );
    cx.simulate_keystrokes("ctrl-z");
    cx.run_until_parked();
    assert_eq!(source_of(&view, cx), before, "替换选区也必须一个撤销组");

    close_shell(&host, cx);
}

/// 相对日期（P1）：模板里的 `{{date:-1d}}` / `{{date:+1d}}` 按**本地今天**偏移渲染，
/// 与其它占位符混用时互不影响；非法偏移原样保留（纯逻辑单测已覆盖，这里只验真实插入链路）。
#[gpui::test]
async fn insert_template_renders_relative_dates(cx: &mut TestAppContext) {
    let _templates = templates::lock_for_test();
    let _daily = daily_notes::lock_for_test();
    let prefs_dir = temp_preferences_dir();
    _templates.reset(Some(prefs_dir.path().join(templates::TEMPLATES_FILE_NAME)));

    let dir = temp_vault_dir("relative");
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "笔记", "开头\n");
    let folder = seed_folder(&vault, "模板");
    seed_note_in_folder(
        &vault,
        &folder,
        "相对日期",
        "昨天 {{date:-1d:YYYY-MM-DD}} 明天 {{date:+1d:YYYY-MM-DD}} 标题 {{title}}\n",
    );

    let (host, cx) = open_shell(cx, vault.clone());
    // 启动时可能打开模板目录里的笔记，显式点开普通笔记（caret 在 0）。
    click_tree_row(cx, tree_row(&vault, "笔记"));
    let view = active_view(&host, cx);

    let position = right_click_point(cx);
    cx.simulate_mouse_down(position, MouseButton::Right, Modifiers::default());
    cx.run_until_parked();
    click_selector(cx, "editor-context-insert-template");
    click_selector(cx, "template-picker-item-0");

    let today = DateTimeParts::now();
    let yesterday = today.add_days(-1).expect("昨天必须可算");
    let tomorrow = today.add_days(1).expect("明天必须可算");
    assert_eq!(
        source_of(&view, cx),
        format!(
            "昨天 {} 明天 {} 标题 笔记\n开头\n",
            templates::format_datetime(&yesterday, "YYYY-MM-DD"),
            templates::format_datetime(&tomorrow, "YYYY-MM-DD"),
        ),
        "模板里的相对日期必须按本地今天偏移渲染"
    );

    close_shell(&host, cx);
}

#[gpui::test]
async fn new_from_template_creates_opens_and_never_overwrites(cx: &mut TestAppContext) {
    let _templates = templates::lock_for_test();
    let _daily = daily_notes::lock_for_test();
    let prefs_dir = temp_preferences_dir();
    _templates.reset(Some(prefs_dir.path().join(templates::TEMPLATES_FILE_NAME)));

    let dir = temp_vault_dir("new");
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "笔记", "原文\n");
    let folder = seed_folder(&vault, "模板");
    seed_note_in_folder(&vault, &folder, "周记", "# {{title}}\n");

    let (host, cx) = open_shell(cx, vault.clone());
    click_tree_row(cx, tree_row(&vault, "笔记"));
    assert_eq!(source_of(&active_view(&host, cx), cx), "原文\n");

    // 侧栏「从模板新建…」→ 选择器 → 选中 → 新笔记打开且正文渲染。
    click_selector(cx, "sidebar-new-from-template");
    assert!(cx.debug_bounds("template-picker").is_some());
    click_selector(cx, "template-picker-item-0");
    assert_eq!(
        source_of(&active_view(&host, cx), cx),
        "# 周记\n",
        "新笔记必须带着替换后的模板正文打开"
    );
    let titles = note_titles(&vault);
    assert!(titles.contains(&"笔记".to_owned()), "已有笔记不能被覆盖");
    assert!(
        titles.contains(&"周记".to_owned()),
        "新笔记标题默认模板标题"
    );

    // 再来一次：根级已有「周记」→ 自动加序号，仍然不覆盖。
    click_selector(cx, "sidebar-new-from-template");
    click_selector(cx, "template-picker-item-0");
    let titles = note_titles(&vault);
    assert_eq!(
        titles
            .iter()
            .filter(|title| title.starts_with("周记"))
            .count(),
        3,
        "第二次必须新建另一篇（不覆盖已有笔记）：{titles:?}"
    );
    assert!(
        titles.contains(&"周记 2".to_owned()),
        "同父级重名默认加序号：{titles:?}"
    );
    assert_eq!(
        source_of(&active_view(&host, cx), cx),
        "# 周记 2\n",
        "`{{title}}` 用新笔记标题（含去重序号）"
    );

    close_shell(&host, cx);
}

#[gpui::test]
async fn picker_shows_explicit_empty_state_when_folder_missing(cx: &mut TestAppContext) {
    let _templates = templates::lock_for_test();
    let _daily = daily_notes::lock_for_test();
    let prefs_dir = temp_preferences_dir();
    _templates.reset(Some(prefs_dir.path().join(templates::TEMPLATES_FILE_NAME)));

    let dir = temp_vault_dir("empty");
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "笔记", "正文\n");

    let (host, cx) = open_shell(cx, vault.clone());
    let tree_before = list_tree(&vault).expect("读树失败").len();

    click_selector(cx, "sidebar-new-from-template");
    assert!(
        cx.debug_bounds("template-picker-empty").is_some(),
        "模板文件夹不存在时必须有明确空态"
    );
    assert_eq!(
        list_tree(&vault).expect("读树失败").len(),
        tree_before,
        "空态不得自动创建模板文件夹"
    );
    // Esc 关闭选择器（输入框不在组合态）。
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("template-picker").is_none(),
        "Esc 关闭选择器"
    );

    close_shell(&host, cx);
}

#[gpui::test]
async fn picker_shows_empty_state_for_existing_empty_folder(cx: &mut TestAppContext) {
    let _templates = templates::lock_for_test();
    let _daily = daily_notes::lock_for_test();
    let prefs_dir = temp_preferences_dir();
    _templates.reset(Some(prefs_dir.path().join(templates::TEMPLATES_FILE_NAME)));

    let dir = temp_vault_dir("empty-folder");
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "笔记", "正文\n");
    // 文件夹存在但没有笔记：空态必须区分「不存在」与「为空」。
    seed_folder(&vault, "模板");
    let list = templates::collect_templates(&list_tree(&vault).expect("读树失败"), "模板");
    assert!(list.folder_exists && list.items.is_empty());

    let (host, cx) = open_shell(cx, vault.clone());
    click_selector(cx, "sidebar-new-from-template");
    assert!(
        cx.debug_bounds("template-picker-empty").is_some(),
        "空文件夹也必须有明确空态"
    );
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(cx.debug_bounds("template-picker").is_none());

    close_shell(&host, cx);
}

/// 设置今天的日记偏好（写入测试临时 `daily-notes.json`）。
fn set_daily_preferences(
    guard: &daily_notes::DailyNotesTestGuard,
    prefs_dir: &TempDir,
    date_format: &str,
    folder: &str,
    template: &str,
) {
    guard.reset(Some(
        prefs_dir.path().join(daily_notes::DAILY_NOTES_FILE_NAME),
    ));
    daily_notes::set_preferences(daily_notes::DailyNotesPreferences {
        enabled: true,
        date_format: date_format.to_owned(),
        folder: folder.to_owned(),
        template: template.to_owned(),
    });
}

#[gpui::test]
async fn daily_note_creates_nested_path_then_reopens_the_same_note(cx: &mut TestAppContext) {
    let _templates = templates::lock_for_test();
    let _daily = daily_notes::lock_for_test();
    let prefs_dir = temp_preferences_dir();
    _templates.reset(Some(prefs_dir.path().join(templates::TEMPLATES_FILE_NAME)));

    let dir = temp_vault_dir("daily");
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "笔记", "正文\n");
    let folder = seed_folder(&vault, "模板");
    seed_note_in_folder(
        &vault,
        &folder,
        "日记模板",
        "D:{{date:YYYY-MM-DD}} T:{{title}}\n",
    );
    // 日期格式里的 `/` 生成子目录；模板是任意普通笔记（精确路径）。
    set_daily_preferences(&_daily, &prefs_dir, "YYYY/MM/DD", "日记", "模板/日记模板");

    let (host, cx) = open_shell(cx, vault.clone());
    let parts = DateTimeParts::now();
    let expected_path = format!(
        "日记/{}/{}/{}",
        templates::format_datetime(&parts, "YYYY"),
        templates::format_datetime(&parts, "MM"),
        templates::format_datetime(&parts, "DD"),
    );

    click_selector(cx, "sidebar-daily-note");
    let notice = notice_of(&host, cx).expect("创建今日日记必须给提示");
    assert!(
        notice.contains(&expected_path),
        "提示必须带路径（实际：{notice}）"
    );
    assert_eq!(
        source_of(&active_view(&host, cx), cx),
        format!(
            "D:{} T:{}\n",
            templates::format_datetime(&parts, "YYYY-MM-DD"),
            templates::format_datetime(&parts, "DD")
        ),
        "日记正文必须用日记模板渲染（title = 笔记标题 = 最后一段）"
    );
    let notes_after_create = note_titles(&vault);
    assert!(
        notes_after_create.contains(&templates::format_datetime(&parts, "DD")),
        "今日日记必须建出来：{notes_after_create:?}"
    );

    // 再点一次：同路径已存在 → 直接打开，不重复创建。
    click_selector(cx, "sidebar-daily-note");
    let notice = notice_of(&host, cx).expect("打开今日日记必须给提示");
    assert!(
        notice.contains("已打开"),
        "已存在时必须走「打开」而不是「创建」：{notice}"
    );
    assert_eq!(
        note_titles(&vault),
        notes_after_create,
        "重复点击不得重复创建日记"
    );
    assert_eq!(
        source_of(&active_view(&host, cx), cx),
        format!(
            "D:{} T:{}\n",
            templates::format_datetime(&parts, "YYYY-MM-DD"),
            templates::format_datetime(&parts, "DD")
        )
    );

    close_shell(&host, cx);
}

#[gpui::test]
async fn daily_note_missing_template_creates_empty_note_with_notice(cx: &mut TestAppContext) {
    let _templates = templates::lock_for_test();
    let _daily = daily_notes::lock_for_test();
    let prefs_dir = temp_preferences_dir();
    _templates.reset(Some(prefs_dir.path().join(templates::TEMPLATES_FILE_NAME)));

    let dir = temp_vault_dir("daily-missing");
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "笔记", "正文\n");
    // 默认日期格式（没有 `/`）：日记落在「日记/YYYY-MM-DD」。
    set_daily_preferences(&_daily, &prefs_dir, "YYYY-MM-DD", "日记", "模板/不存在");

    let (host, cx) = open_shell(cx, vault.clone());
    let parts = DateTimeParts::now();
    let expected_path = format!("日记/{}", templates::format_datetime(&parts, "YYYY-MM-DD"));

    click_selector(cx, "sidebar-daily-note");
    let notice = notice_of(&host, cx).expect("模板丢失也必须给提示");
    assert!(
        notice.contains("模板") && notice.contains("不存在"),
        "模板丢失必须非阻塞提示：{notice}"
    );
    assert_eq!(
        source_of(&active_view(&host, cx), cx),
        "",
        "模板丢失时创建空日记（不写入任何正文）"
    );
    let titles = note_titles(&vault);
    assert!(
        titles.contains(&templates::format_datetime(&parts, "YYYY-MM-DD")),
        "空日记仍然必须建出来：{titles:?}"
    );
    assert!(
        templates::note_index_by_path(&list_tree(&vault).expect("读树失败"), &expected_path)
            .is_some(),
        "日记路径必须是 {expected_path}"
    );

    close_shell(&host, cx);
}

#[gpui::test]
async fn disabled_plugins_hide_entries_and_do_nothing(cx: &mut TestAppContext) {
    let _templates = templates::lock_for_test();
    let _daily = daily_notes::lock_for_test();
    let prefs_dir = temp_preferences_dir();
    _templates.reset(Some(prefs_dir.path().join(templates::TEMPLATES_FILE_NAME)));
    _daily.reset(Some(
        prefs_dir.path().join(daily_notes::DAILY_NOTES_FILE_NAME),
    ));
    // 两个插件都关掉：入口隐藏，任何旧路径（例如右键菜单）也不出现「模板…」。
    let mut templates_prefs = templates::preferences();
    templates_prefs.enabled = false;
    templates::set_preferences(templates_prefs);
    let mut daily_prefs = daily_notes::preferences();
    daily_prefs.enabled = false;
    daily_notes::set_preferences(daily_prefs);

    let dir = temp_vault_dir("disabled");
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "笔记", "正文\n");

    let (host, cx) = open_shell(cx, vault.clone());
    assert!(
        cx.debug_bounds("sidebar-new-from-template").is_none(),
        "模板关闭时必须隐藏侧栏入口"
    );
    assert!(
        cx.debug_bounds("sidebar-daily-note").is_none(),
        "日记关闭时必须隐藏侧栏入口"
    );

    let position = right_click_point(cx);
    cx.simulate_mouse_down(position, MouseButton::Right, Modifiers::default());
    cx.run_until_parked();
    assert!(cx.debug_bounds("editor-context-insert-table").is_some());
    assert!(
        cx.debug_bounds("editor-context-insert-template").is_none(),
        "模板关闭时右键菜单不得出现「模板…」"
    );
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert_eq!(source_of(&active_view(&host, cx), cx), "正文\n");

    close_shell(&host, cx);
}
