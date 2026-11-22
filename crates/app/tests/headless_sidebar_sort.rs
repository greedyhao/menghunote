//! 最左侧图标栏（ribbon）与侧栏排序（目录优先）的 headless 集成测试。
//!
//! 覆盖（真实 `AppShell` + 临时 vault + `#[gpui::test]`，**不 sleep**）：
//!
//! 1. ribbon 在侧栏**左边**、宽度固定，只放快捷操作（今日日记/模板/图谱/搜索）；
//!    **设置不在 ribbon 里**（设置入口仍然是侧栏底部 `app-root-settings`）；
//! 2. ribbon 的每个按钮走既有动作（搜索浮层、图谱覆盖层、模板选择器、日记）；
//!    旧文字入口的固定 selector（`sidebar-daily-note` / `sidebar-new-from-template` /
//!    `sidebar-graph`）作为兼容别名仍然可点，视觉上只有一个按钮；
//! 3. 侧栏默认「目录优先 + 文件名 A-Z」；排序菜单 6 个键（勾选当前项）、点空白关闭；
//! 4. 六种排序键都真的重排（文件名 A-Z/Z-A、修改时间、创建时间）；
//! 5. 重排不丢折叠状态与当前打开的笔记（按 `node_id` 重新定位）；
//! 6. 偏好写本机 `sidebar.json`（原子写、不同步、不写正文），损坏/未知值回退默认。
//!
//! 纪律（AGENTS.md §6）：临时目录一律 `CARGO_TARGET_TMPDIR`；收敛点只用
//! `run_until_parked()`；偏好全局状态用 `lock_sidebar_for_test()` 串行。

use std::path::Path;

use gpui::{
    div, AppContext as _, Entity, InteractiveElement as _, Modifiers, ParentElement as _, Render,
    Styled as _, TestAppContext, VisualTestContext,
};
use menghunote_app::daily_notes;
use menghunote_app::shell::{
    lock_sidebar_for_test, AppShell, SidebarPreferences, RIBBON_WIDTH, SIDEBAR_FILE_NAME,
    SIDEBAR_PATH_ENV,
};
use menghunote_app::templates;
use menghunote_storage::{
    append_updates, list_tree_sorted, load_body_doc, StructureWriter, TreeSort, UpdateRecord, Vault,
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

fn temp_dir(prefix: &str) -> TempDir {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("headless-sidebar-{prefix}-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("创建 target/tmp 失败");
    TempDirBuilder::new()
        .prefix("case-")
        .tempdir_in(&root)
        .expect("创建临时目录失败")
}

fn temp_vault(prefix: &str) -> (TempDir, Vault) {
    let dir = temp_dir(prefix);
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    (dir, vault)
}

/// 正文 A 事务：把 `text` 写进已存在的 body doc（与 `headless_templates` 同一条路径）。
fn write_body(vault: &Vault, body_doc_id: &str, text: &str) {
    let (doc, head) = load_body_doc(vault, body_doc_id, 1, None).expect("加载正文失败");
    doc.insert_scalars(0, text).expect("写入正文失败");
    let base = head.version_vector().expect("解码 VV 失败");
    let update = doc.pending_update(&base).expect("导出增量失败");
    append_updates(
        vault,
        body_doc_id,
        head.head_seq,
        &[UpdateRecord::new("headless-sidebar", update)],
    )
    .expect("A 事务写入正文失败");
}

/// 一次结构提交里建「笔记 + 目录 + 嵌套笔记」，时间戳显式给定（排序断言用）。
///
/// 展示顺序（目录优先 + A-Z）：`Zeta`(目录, created 11) → `gamma`(子笔记, 13)
/// → `Alpha`(12) → `beta`(10)；正文统一写标题本身。
fn seed_tree(vault: &Vault) {
    let mut writer = StructureWriter::open(vault, 1).expect("打开结构写入口失败");
    // 创建顺序与展示顺序故意不同：beta(10) → Zeta(11) → Alpha(12) → 子笔记 gamma(13)。
    let beta = writer.doc().create_note("beta", 10).expect("笔记 beta");
    let folder = writer.doc().create_folder("Zeta", 11).expect("目录 Zeta");
    let alpha = writer.doc().create_note("Alpha", 12).expect("笔记 Alpha");
    let gamma = writer.doc().create_note("gamma", 13).expect("笔记 gamma");
    writer
        .doc()
        .move_node(&gamma.node_id, Some(&folder), 14)
        .expect("gamma 挂到 Zeta");
    writer
        .commit("headless-sidebar-seed")
        .expect("结构提交失败");
    for (doc_id, title) in [
        (&beta.body_doc_id, "beta"),
        (&alpha.body_doc_id, "Alpha"),
        (&gamma.body_doc_id, "gamma"),
    ] {
        write_body(vault, doc_id, title);
    }
}

fn open_shell(
    cx: &mut TestAppContext,
    vault: Vault,
) -> (Entity<ShellHost>, &mut VisualTestContext) {
    let (host, cx) = cx.add_window_view(move |_, cx| {
        let shell = cx.new(|cx| AppShell::new(vault, "headless-sidebar".to_owned(), cx));
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

fn bounds_of(cx: &mut VisualTestContext, selector: &'static str) -> gpui::Bounds<gpui::Pixels> {
    cx.debug_bounds(selector)
        .unwrap_or_else(|| panic!("必须画出 selector={selector}"))
}

fn click_selector(cx: &mut VisualTestContext, selector: &'static str) {
    let bounds = bounds_of(cx, selector);
    cx.simulate_click(bounds.center(), Modifiers::default());
    cx.run_until_parked();
}

fn sidebar_titles(host: &Entity<ShellHost>, cx: &mut VisualTestContext) -> Vec<String> {
    cx.update(|_window, app| {
        host.read(app)
            .shell
            .read(app)
            .sidebar_titles()
            .into_iter()
            .map(str::to_owned)
            .collect()
    })
}

fn sort_key(host: &Entity<ShellHost>, cx: &mut VisualTestContext) -> TreeSort {
    cx.update(|_window, app| host.read(app).shell.read(app).sidebar_sort_key())
}

/// 打开排序菜单并点某个键（每次都要重开：选中后菜单自动关闭）。
fn choose_sort(cx: &mut VisualTestContext, key: &'static str) {
    click_selector(cx, "sidebar-sort");
    assert!(
        cx.debug_bounds("sidebar-sort-menu").is_some(),
        "点「排序」必须弹出菜单"
    );
    click_selector(cx, key);
    assert!(
        cx.debug_bounds("sidebar-sort-menu").is_none(),
        "选中排序键后菜单必须关闭"
    );
}

// ---------------------------------------------------------------------------
// ribbon
// ---------------------------------------------------------------------------

/// ribbon 在侧栏左边、宽度固定；只有快捷操作，**没有设置**。
#[gpui::test]
async fn ribbon_sits_left_of_the_sidebar_with_quick_actions_only(cx: &mut TestAppContext) {
    let guard = lock_sidebar_for_test();
    guard.reset(None);
    let (_dir, vault) = temp_vault("ribbon-layout");
    seed_tree(&vault);
    let (host, cx) = open_shell(cx, vault.clone());

    let ribbon = bounds_of(cx, "ribbon");
    let sidebar = bounds_of(cx, "sidebar");
    assert!(
        (ribbon.size.width.as_f32() - RIBBON_WIDTH).abs() < 0.5,
        "ribbon 宽度必须是 {RIBBON_WIDTH}px：{ribbon:?}"
    );
    assert!(
        ribbon.origin.x <= sidebar.origin.x + gpui::px(0.5),
        "ribbon 必须在侧栏左边：ribbon={ribbon:?} sidebar={sidebar:?}"
    );
    assert!(
        (sidebar.size.width.as_f32() - 220.0).abs() < 0.5,
        "侧栏宽度必须保持 220px（不被 ribbon 挤变）：{sidebar:?}"
    );

    // 四个快捷操作都在，且都在 ribbon 之内（竖直排列）。
    for selector in [
        "ribbon-daily",
        "ribbon-template",
        "ribbon-graph",
        "ribbon-search",
    ] {
        let bounds = bounds_of(cx, selector);
        assert!(
            bounds.origin.x >= ribbon.origin.x - gpui::px(0.5)
                && bounds.origin.x + bounds.size.width
                    <= ribbon.origin.x + ribbon.size.width + gpui::px(0.5),
            "{selector} 必须落在 ribbon 内：{bounds:?} / {ribbon:?}"
        );
    }
    // 设置不在 ribbon 里（入口仍然是侧栏底部；`AppRoot` 的 `app-root-settings`）。
    assert!(
        cx.debug_bounds("ribbon-settings").is_none(),
        "ribbon 不得包含设置按钮（设置继续放侧栏底部）"
    );

    // 旧文字入口的固定 selector 作为兼容别名仍然存在，且指向同一个 ribbon 按钮。
    for (legacy, ribbon_selector) in [
        ("sidebar-daily-note", "ribbon-daily"),
        ("sidebar-new-from-template", "ribbon-template"),
        ("sidebar-graph", "ribbon-graph"),
    ] {
        let legacy_bounds = bounds_of(cx, legacy);
        let ribbon_bounds = bounds_of(cx, ribbon_selector);
        assert!(
            legacy_bounds.center() == ribbon_bounds.center(),
            "{legacy} 必须是 {ribbon_selector} 的兼容别名（同一个按钮）"
        );
    }

    close_shell(&host, cx);
}

/// ribbon 按钮复用既有动作：搜索浮层 / 图谱覆盖层 / 模板选择器 / 今日日记。
#[gpui::test]
async fn ribbon_buttons_reuse_existing_actions(cx: &mut TestAppContext) {
    let guard = lock_sidebar_for_test();
    guard.reset(None);
    let _templates = templates::lock_for_test();
    let _daily = daily_notes::lock_for_test();
    let prefs_dir = temp_dir("ribbon-prefs");
    _templates.reset(Some(prefs_dir.path().join(templates::TEMPLATES_FILE_NAME)));
    _daily.reset(Some(
        prefs_dir.path().join(daily_notes::DAILY_NOTES_FILE_NAME),
    ));

    let (_dir, vault) = temp_vault("ribbon-actions");
    seed_tree(&vault);
    let (host, cx) = open_shell(cx, vault.clone());

    // 搜索：ribbon-search 打开 Ctrl+K 的同一个浮层；Esc 关闭。
    click_selector(cx, "ribbon-search");
    assert!(
        cx.debug_bounds("search-overlay").is_some(),
        "ribbon-search 必须打开搜索浮层"
    );
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(cx.debug_bounds("search-overlay").is_none());

    // 图谱：ribbon-graph 打开全屏覆盖层；Esc 关闭。
    click_selector(cx, "ribbon-graph");
    assert!(
        cx.debug_bounds("graph-overlay").is_some(),
        "ribbon-graph 必须打开图谱覆盖层"
    );
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(cx.debug_bounds("graph-overlay").is_none());

    // 模板：ribbon-template 打开模板选择器（默认模板文件夹不存在时显示明确空态）。
    click_selector(cx, "ribbon-template");
    assert!(
        cx.debug_bounds("template-picker").is_some(),
        "ribbon-template 必须打开模板选择器"
    );
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(cx.debug_bounds("template-picker").is_none());

    // 今日日记：ribbon-daily 走既有「今日日记」动作（不存在则创建并打开）。
    let before = sidebar_titles(&host, cx);
    click_selector(cx, "ribbon-daily");
    let after = sidebar_titles(&host, cx);
    let prefs = daily_notes::preferences();
    let parts = templates::DateTimeParts::now();
    let expected = daily_notes::note_path(&prefs, &parts).expect("日记路径必须可算");
    assert!(
        after.len() > before.len(),
        "ribbon-daily 必须真的创建今日日记：before={before:?} after={after:?}"
    );
    assert!(
        after.iter().any(|title| title == &expected.title),
        "新建的日记标题必须是 {expected:?}：{after:?}"
    );

    close_shell(&host, cx);
}

// ---------------------------------------------------------------------------
// 侧栏排序
// ---------------------------------------------------------------------------

/// 默认「目录优先 + 文件名 A-Z」；菜单 6 项、点空白关闭。
#[gpui::test]
async fn sort_menu_lists_six_keys_and_defaults_to_folder_first_name_asc(cx: &mut TestAppContext) {
    let guard = lock_sidebar_for_test();
    guard.reset(None);
    let (_dir, vault) = temp_vault("sort-menu");
    seed_tree(&vault);
    let (host, cx) = open_shell(cx, vault.clone());

    assert_eq!(
        sidebar_titles(&host, cx),
        vec!["Zeta", "gamma", "Alpha", "beta"],
        "默认：每个层级目录在前，组内按文件名 A-Z"
    );
    assert_eq!(sort_key(&host, cx), TreeSort::NameAsc);

    click_selector(cx, "sidebar-sort");
    assert!(cx.debug_bounds("sidebar-sort-menu").is_some());
    for key in [
        "sidebar-sort-name_asc",
        "sidebar-sort-name_desc",
        "sidebar-sort-updated_newest",
        "sidebar-sort-updated_oldest",
        "sidebar-sort-created_newest",
        "sidebar-sort-created_oldest",
    ] {
        bounds_of(cx, key);
    }

    // 点空白（编辑器面板）关闭菜单，且不改排序。
    let pane = bounds_of(cx, "editor-pane");
    cx.simulate_click(pane.center(), Modifiers::default());
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("sidebar-sort-menu").is_none(),
        "点空白必须关闭排序菜单"
    );
    assert_eq!(sort_key(&host, cx), TreeSort::NameAsc);

    close_shell(&host, cx);
}

/// 六种排序键都真的重排（目录永远在前；时间键下目录组按标题）。
#[gpui::test]
async fn six_sort_keys_reorder_folder_first_and_are_persisted(cx: &mut TestAppContext) {
    let guard = lock_sidebar_for_test();
    let prefs_dir = temp_dir("sort-keys");
    let prefs_path = prefs_dir.path().join(SIDEBAR_FILE_NAME);
    guard.reset(Some(prefs_path.clone()));

    let (_dir, vault) = temp_vault("sort-keys");
    seed_tree(&vault);
    let (host, cx) = open_shell(cx, vault.clone());

    choose_sort(cx, "sidebar-sort-name_desc");
    assert_eq!(sort_key(&host, cx), TreeSort::NameDesc);
    assert_eq!(
        sidebar_titles(&host, cx),
        vec!["Zeta", "gamma", "beta", "Alpha"],
        "Z-A：目录仍在前，笔记组内逆序"
    );
    let saved =
        SidebarPreferences::parse(&std::fs::read_to_string(&prefs_path).expect("排序偏好必须写盘"));
    assert_eq!(saved.sort, TreeSort::NameDesc);
    assert!(
        !prefs_path.with_extension("json.tmp").exists(),
        "原子写完成后不能留下临时文件"
    );

    choose_sort(cx, "sidebar-sort-updated_newest");
    assert_eq!(
        sidebar_titles(&host, cx),
        vec!["Zeta", "gamma", "Alpha", "beta"],
        "修改时间新→旧：gamma(13) > Alpha(12) > beta(10)；目录仍按标题（子节点跟随父目录）"
    );

    choose_sort(cx, "sidebar-sort-updated_oldest");
    assert_eq!(
        sidebar_titles(&host, cx),
        vec!["Zeta", "gamma", "beta", "Alpha"],
        "修改时间旧→新：beta(10) < Alpha(12) < gamma(13)"
    );

    choose_sort(cx, "sidebar-sort-created_newest");
    assert_eq!(
        sidebar_titles(&host, cx),
        vec!["Zeta", "gamma", "Alpha", "beta"],
        "创建时间新→旧：gamma(13) > Alpha(12) > beta(10)"
    );

    choose_sort(cx, "sidebar-sort-created_oldest");
    assert_eq!(
        sidebar_titles(&host, cx),
        vec!["Zeta", "gamma", "beta", "Alpha"],
        "创建时间旧→新"
    );

    choose_sort(cx, "sidebar-sort-name_asc");
    assert_eq!(
        sidebar_titles(&host, cx),
        vec!["Zeta", "gamma", "Alpha", "beta"]
    );

    close_shell(&host, cx);
}

/// 重排不丢折叠状态与当前打开的笔记（都按 `node_id` 保存）。
#[gpui::test]
async fn sorting_keeps_collapsed_folders_and_selected_note(cx: &mut TestAppContext) {
    let guard = lock_sidebar_for_test();
    guard.reset(None);
    let (_dir, vault) = temp_vault("sort-state");
    seed_tree(&vault);
    let (host, cx) = open_shell(cx, vault.clone());

    // 默认顺序：Zeta(0) / gamma(1) / Alpha(2) / beta(3)。
    assert_eq!(
        sidebar_titles(&host, cx),
        vec!["Zeta", "gamma", "Alpha", "beta"]
    );
    // 打开 beta（行 3）。
    click_selector(cx, "tree-row-3");
    assert!(cx.debug_bounds("tree-row-3-selected").is_some());
    // 折叠 Zeta（行 0）：子行 gamma 隐藏。
    click_selector(cx, "tree-row-0");
    assert!(
        cx.debug_bounds("tree-row-1").is_none(),
        "折叠后子行必须隐藏"
    );

    // 切到 Z-A：树顺序变成 Zeta(0) / gamma(1) / beta(2) / Alpha(3)。
    choose_sort(cx, "sidebar-sort-name_desc");
    assert_eq!(
        sidebar_titles(&host, cx),
        vec!["Zeta", "gamma", "beta", "Alpha"]
    );
    assert!(
        cx.debug_bounds("tree-row-1").is_none(),
        "重排不得丢掉折叠状态（gamma 仍被 Zeta 折叠遮住）"
    );
    assert!(
        cx.debug_bounds("tree-row-2-selected").is_some(),
        "重排后当前打开笔记（beta）必须仍在自己的新行上带 -selected"
    );
    assert!(
        cx.debug_bounds("tree-row-3-selected").is_none(),
        "别的行不得被误标成已打开"
    );
    assert!(
        cx.debug_bounds("editor-current-doc").is_some(),
        "重排不得关掉当前文档"
    );

    close_shell(&host, cx);
}

// ---------------------------------------------------------------------------
// 偏好持久化（纯函数；不需要窗口）
// ---------------------------------------------------------------------------

#[test]
fn preferences_round_trip_and_corrupt_values_fall_back_to_default() {
    let guard = lock_sidebar_for_test();
    let dir = temp_dir("prefs-io");
    let path = dir.path().join(SIDEBAR_FILE_NAME);
    guard.reset(Some(path.clone()));

    // 写入 → 读回（原子写、无临时文件残留）。
    for sort in TreeSort::ALL {
        menghunote_app::shell::save_sidebar_preferences(&path, SidebarPreferences { sort })
            .expect("写偏好");
        assert!(!path.with_extension("json.tmp").exists());
        assert_eq!(
            menghunote_app::shell::load_sidebar_preferences(&path).sort,
            sort
        );
    }

    // 损坏 / 未知版本 / 未知排序键 → 默认（文件名 A-Z），不报错、不 panic。
    for (label, text) in [
        ("not-json", "这不是 JSON".to_owned()),
        ("empty", String::new()),
        (
            "version",
            "{\n  \"version\": 99,\n  \"sort\": \"name_desc\"\n}\n".to_owned(),
        ),
        (
            "unknown-sort",
            "{\n  \"version\": 1,\n  \"sort\": \"by-magic\"\n}\n".to_owned(),
        ),
        ("missing-sort", "{\n  \"version\": 1\n}\n".to_owned()),
    ] {
        std::fs::write(&path, &text).expect("写损坏文件");
        assert_eq!(
            SidebarPreferences::parse(&text).sort,
            TreeSort::NameAsc,
            "{label} 必须回退默认"
        );
        assert_eq!(
            menghunote_app::shell::load_sidebar_preferences(&path).sort,
            TreeSort::NameAsc,
            "{label}（从文件读）必须回退默认"
        );
    }

    // 不存在的文件同样是默认。
    let missing = dir.path().join("nope.json");
    assert_eq!(
        menghunote_app::shell::load_sidebar_preferences(&missing).sort,
        TreeSort::NameAsc
    );

    // 默认路径解析不 panic（平台配置目录 / HOME / XDG 至少有一个可得）。
    assert!(
        menghunote_app::shell::sidebar_settings_path().is_some(),
        "平台配置目录（或 HOME/XDG）必须能解析出偏好路径"
    );
    assert!(
        std::env::var_os(SIDEBAR_PATH_ENV).is_none(),
        "本测试不设置路径覆盖环境变量（默认路径分支才有判别力）"
    );
}

/// 启动时加载 `sidebar.json`：文件里的排序决定第一帧顺序；损坏回退默认。
#[gpui::test]
async fn startup_reads_sort_preference_from_disk(cx: &mut TestAppContext) {
    let guard = lock_sidebar_for_test();
    let prefs_dir = temp_dir("prefs-startup");
    let path = prefs_dir.path().join(SIDEBAR_FILE_NAME);
    guard.reset(Some(path.clone()));

    let (_dir, vault) = temp_vault("prefs-startup");
    seed_tree(&vault);

    // 写 Z-A，再显式按该路径重载偏好（模拟"上一次会话选了 Z-A 后重启"）。
    menghunote_app::shell::save_sidebar_preferences(
        &path,
        SidebarPreferences {
            sort: TreeSort::NameDesc,
        },
    )
    .expect("写偏好");
    menghunote_app::shell::init_sidebar_sort_from_path(path.clone());

    let (host, cx) = open_shell(cx, vault.clone());
    assert_eq!(sort_key(&host, cx), TreeSort::NameDesc);
    assert_eq!(
        sidebar_titles(&host, cx),
        vec!["Zeta", "gamma", "beta", "Alpha"],
        "启动第一帧就必须按偏好排序（目录仍在前）"
    );
    close_shell(&host, cx);

    // 损坏文件 → 默认顺序。
    std::fs::write(&path, b"{ broken").expect("写损坏偏好");
    menghunote_app::shell::init_sidebar_sort_from_path(path);
    let (_dir2, vault2) = temp_vault("prefs-startup-corrupt");
    seed_tree(&vault2);
    let (host2, cx) = open_shell(cx, vault2.clone());
    assert_eq!(sort_key(&host2, cx), TreeSort::NameAsc);
    assert_eq!(
        sidebar_titles(&host2, cx),
        vec!["Zeta", "gamma", "Alpha", "beta"]
    );
    close_shell(&host2, cx);
}

/// 目录优先的标题断言与存储层一致（大小写/中文稳定；不同排序键都不打乱层级）。
#[gpui::test]
async fn sidebar_order_matches_storage_sorted_view(cx: &mut TestAppContext) {
    let guard = lock_sidebar_for_test();
    guard.reset(None);
    let (_dir, vault) = temp_vault("sorted-view");
    seed_tree(&vault);
    let expected: Vec<String> = list_tree_sorted(&vault, TreeSort::NameAsc)
        .expect("读树")
        .into_iter()
        .map(|entry| entry.title)
        .collect();
    let (host, cx) = open_shell(cx, vault.clone());
    assert_eq!(sidebar_titles(&host, cx), expected);
    close_shell(&host, cx);
}
