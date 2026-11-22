//! 设置页「快捷键」分类的 headless UI 测试（默认 `sync-ui` 构建）。
//!
//! 覆盖验收线：
//!
//! 1. 左侧导航出现「快捷键」（在「语言」之后、「同步」之前），7 行默认绑定可见；
//! 2. 点 `shortcut-edit-search` 后按 `Ctrl+Shift+K`：绑定更新 + `shortcuts.json`
//!    写入 + 重新初始化仍生效；
//! 3. 两个命令设成同一个 key：两行 `shortcut-conflict-*` 可见、顶部汇总有冲突，
//!    且保存仍然成功（冲突只是提醒）；
//! 4. 录制态按 `Esc` 取消：绑定不变、文件不写；
//! 5. 录制态按键被根视图 **capture 相** 吞掉，不会同时触发原命令/正文。
//!
//! 纪律：临时目录在 `CARGO_TARGET_TMPDIR`；收敛点只用 `run_until_parked`，不 sleep。
//! 全局快捷键状态用 `shortcuts::lock_for_test()` 串行并自动复位，避免跨测试污染。

#![cfg(feature = "sync-ui")]

use std::path::{Path, PathBuf};

use gpui::{Entity, Modifiers, TestAppContext, VisualTestContext};
use menghunote_app::onboarding::app_root::SettingsSection;
use menghunote_app::onboarding::{
    AppRoot, AppRootOptions, ProfileLayout, DEFAULT_OPERATION_BUDGET,
};
use menghunote_app::shortcuts::{self, CommandId, KeyCombo};
use tempfile::{Builder as TempDirBuilder, TempDir};

fn temp_layout(prefix: &str) -> (TempDir, ProfileLayout) {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "headless-shortcuts-{prefix}-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).expect("创建 target/tmp 测试目录失败");
    let dir = TempDirBuilder::new()
        .prefix("case-")
        .tempdir_in(&root)
        .expect("创建临时目录失败");
    let layout = ProfileLayout::new(
        dir.path().join("config/sync.json"),
        dir.path().join("data/instances"),
    );
    (dir, layout)
}

fn options(layout: ProfileLayout) -> AppRootOptions {
    AppRootOptions {
        layout,
        default_relay: "https://relay.example".to_owned(),
        instance_id: Some("inst-shortcuts".to_owned()),
        allow_local_vault: false,
        operation_budget: DEFAULT_OPERATION_BUDGET,
    }
}

fn open_root(
    cx: &mut TestAppContext,
    options: AppRootOptions,
) -> (Entity<AppRoot>, &mut VisualTestContext) {
    cx.add_window_view(move |window, cx| {
        let root = AppRoot::new(options, cx);
        AppRoot::install_close_handler(window, cx.entity(), cx);
        root
    })
}

fn open_settings(root: &Entity<AppRoot>, cx: &mut VisualTestContext) {
    root.update(cx, |root, cx| root.open_settings(cx));
    cx.run_until_parked();
}

fn expect_bounds(cx: &mut VisualTestContext, selector: &'static str) -> gpui::Bounds<gpui::Pixels> {
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("必须画出 selector={selector}"));
    assert!(
        bounds.size.width.as_f32() > 0.0 && bounds.size.height.as_f32() > 0.0,
        "{selector} 必须有非零尺寸：{bounds:?}"
    );
    bounds
}

fn leak(value: String) -> &'static str {
    Box::leak(value.into_boxed_str())
}

fn click_selector(cx: &mut VisualTestContext, selector: &'static str) {
    let bounds = expect_bounds(cx, selector);
    cx.simulate_click(bounds.center(), Modifiers::default());
    cx.run_until_parked();
}

fn open_shortcuts_section(cx: &mut VisualTestContext) {
    click_selector(cx, "settings-nav-shortcuts");
    expect_bounds(cx, "settings-section-shortcuts");
    expect_bounds(cx, "settings-shortcuts");
}

fn configure(guard: &shortcuts::ShortcutTestGuard, layout: &ProfileLayout) -> PathBuf {
    let path = layout.shortcuts_path();
    guard.reset(Some(path.clone()));
    path
}

fn key(text: &str) -> KeyCombo {
    KeyCombo::parse(text).unwrap_or_else(|| panic!("测试组合必须合法：{text}"))
}

/// 验收 1：导航项位置正确 + 7 行默认绑定可见。
#[gpui::test]
async fn shortcuts_section_lists_seven_commands_with_defaults(cx: &mut TestAppContext) {
    let guard = shortcuts::lock_for_test();
    let (_dir, layout) = temp_layout("rows");
    let _path = configure(&guard, &layout);
    let (root, cx) = open_root(cx, options(layout.clone()));
    cx.run_until_parked();
    open_settings(&root, cx);

    // 导航顺序：语言 → 快捷键 → 同步。
    let language = expect_bounds(cx, "settings-nav-language");
    let shortcuts_nav = expect_bounds(cx, "settings-nav-shortcuts");
    let sync = expect_bounds(cx, "settings-nav-sync");
    assert!(
        language.origin.y.as_f32() < shortcuts_nav.origin.y.as_f32()
            && shortcuts_nav.origin.y.as_f32() < sync.origin.y.as_f32(),
        "「快捷键」必须在「语言」之后、「同步」之前：language={language:?} shortcuts={shortcuts_nav:?} sync={sync:?}"
    );
    open_shortcuts_section(cx);
    assert_eq!(
        root.update(cx, |root, _| root.settings_section()),
        SettingsSection::Shortcuts
    );
    let summary = expect_bounds(cx, "shortcut-conflicts-summary");
    assert!(summary.size.width.as_f32() > 0.0);

    // 7 行 + 7 个值 + 7 组按钮；默认绑定与模型一致。
    let snapshot = shortcuts::snapshot();
    assert_eq!(snapshot.rows.len(), CommandId::ALL.len());
    assert!(snapshot.conflicts.is_empty());
    for row in &snapshot.rows {
        let id = row.command.as_str();
        expect_bounds(cx, leak(format!("shortcut-row-{id}")));
        expect_bounds(cx, leak(format!("shortcut-value-{id}")));
        expect_bounds(cx, leak(format!("shortcut-edit-{id}")));
        expect_bounds(cx, leak(format!("shortcut-reset-{id}")));
        assert!(
            cx.debug_bounds(leak(format!("shortcut-conflict-{id}")))
                .is_none(),
            "默认值不得显示冲突红标：{id}"
        );
    }
    let defaults = shortcuts::ShortcutStore::default();
    assert_eq!(
        shortcuts::display_bindings(defaults.binding(CommandId::Search)),
        "Ctrl+K / Ctrl+F"
    );
    assert_eq!(
        shortcuts::display_bindings(defaults.binding(CommandId::FormatBold)),
        "Ctrl+B"
    );
    // 设置页没打开录制态。
    assert!(
        cx.debug_bounds("shortcut-capture").is_none(),
        "初始不得处于录制态"
    );
    assert!(
        !layout.shortcuts_path().exists(),
        "只查看设置不得写 shortcuts.json"
    );
}

/// 验收 2：点「修改」→ 按 Ctrl+Shift+K → 值/文件更新，重新初始化仍生效。
#[gpui::test]
async fn capture_updates_binding_and_persists(cx: &mut TestAppContext) {
    let guard = shortcuts::lock_for_test();
    let (_dir, layout) = temp_layout("capture");
    let path = configure(&guard, &layout);
    let (root, cx) = open_root(cx, options(layout.clone()));
    cx.run_until_parked();
    open_settings(&root, cx);
    open_shortcuts_section(cx);

    click_selector(cx, "shortcut-edit-search");
    expect_bounds(cx, "shortcut-capture");
    cx.simulate_keystrokes("ctrl-shift-k");
    cx.run_until_parked();

    // 录制态结束 + 值仍渲染。
    assert!(
        cx.debug_bounds("shortcut-capture").is_none(),
        "成功录制后必须退出录制态"
    );
    expect_bounds(cx, "shortcut-value-search");
    assert_eq!(
        shortcuts::binding(CommandId::Search),
        vec![key("ctrl-shift-k")],
        "搜索命令必须只剩新绑定（默认别名被替换）"
    );
    assert!(shortcuts::matches(
        CommandId::Search,
        &gpui::Keystroke::parse("ctrl-shift-k").expect("按键解析失败")
    ));
    assert!(
        !shortcuts::matches(
            CommandId::Search,
            &gpui::Keystroke::parse("ctrl-k").expect("按键解析失败")
        ),
        "旧 Ctrl+K 必须不再触发搜索"
    );

    // 落盘 + 重新初始化（模拟重启）仍生效。
    let raw = std::fs::read_to_string(&path).expect("改绑必须写 shortcuts.json");
    assert!(raw.contains("\"version\": 1"));
    assert!(raw.contains("\"command\": \"search\", \"keys\": [\"ctrl-shift-k\"]"));
    shortcuts::init_from_path(path.clone());
    assert_eq!(
        shortcuts::binding(CommandId::Search),
        vec![key("ctrl-shift-k")]
    );

    // 「恢复默认」只恢复该命令。
    click_selector(cx, "shortcut-reset-search");
    assert_eq!(
        shortcuts::binding(CommandId::Search),
        vec![key("ctrl-k"), key("ctrl-f")]
    );
    let raw = std::fs::read_to_string(&path).expect("恢复默认必须写盘");
    assert!(raw.contains("\"command\": \"search\", \"keys\": [\"ctrl-k\", \"ctrl-f\"]"));
}

/// 验收 3：两个命令同一个 key → 两行红标 + 顶部汇总；保存仍然成功。
#[gpui::test]
async fn conflicts_are_visible_and_still_save(cx: &mut TestAppContext) {
    let guard = shortcuts::lock_for_test();
    let (_dir, layout) = temp_layout("conflicts");
    let path = configure(&guard, &layout);
    let (root, cx) = open_root(cx, options(layout.clone()));
    cx.run_until_parked();
    open_settings(&root, cx);
    open_shortcuts_section(cx);

    // 默认 search 已是 Ctrl+K；把加粗也改成 Ctrl+K。
    click_selector(cx, "shortcut-edit-format_bold");
    cx.simulate_keystrokes("ctrl-k");
    cx.run_until_parked();

    let snapshot = shortcuts::snapshot();
    assert_eq!(snapshot.conflict_count(), 1, "必须检测到一组冲突");
    assert_eq!(snapshot.conflicts[0].commands.len(), 2);
    assert!(
        snapshot.summary().contains('1'),
        "顶部汇总必须包含冲突组数：{}",
        snapshot.summary()
    );
    expect_bounds(cx, "shortcut-conflict-search");
    expect_bounds(cx, "shortcut-conflict-format_bold");
    expect_bounds(cx, "shortcut-conflicts-summary");
    assert!(
        cx.debug_bounds("shortcut-conflict-format_italic").is_none(),
        "未冲突行不得显示红标"
    );
    assert_eq!(
        shortcuts::binding(CommandId::FormatBold),
        vec![key("ctrl-k")]
    );

    // 冲突不影响保存：文件里两个命令都是 ctrl-k。
    let raw = std::fs::read_to_string(&path).expect("冲突绑定也必须写盘");
    assert!(raw.contains("\"command\": \"search\", \"keys\": [\"ctrl-k\", \"ctrl-f\"]"));
    assert!(raw.contains("\"command\": \"format_bold\", \"keys\": [\"ctrl-k\"]"));
    // 重新初始化后冲突仍在（跨重启保留）。
    shortcuts::init_from_path(path);
    assert_eq!(shortcuts::snapshot().conflict_count(), 1);
}

/// 验收 4：录制态 `Esc` 取消，不改变绑定、不写文件。
#[gpui::test]
async fn escape_cancels_capture_without_changing_binding(cx: &mut TestAppContext) {
    let guard = shortcuts::lock_for_test();
    let (_dir, layout) = temp_layout("escape");
    let path = configure(&guard, &layout);
    let before = shortcuts::binding(CommandId::Search);
    let (root, cx) = open_root(cx, options(layout.clone()));
    cx.run_until_parked();
    open_settings(&root, cx);
    open_shortcuts_section(cx);

    click_selector(cx, "shortcut-edit-search");
    expect_bounds(cx, "shortcut-capture");
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("shortcut-capture").is_none(),
        "Esc 必须退出录制态"
    );
    assert_eq!(
        shortcuts::binding(CommandId::Search),
        before,
        "Esc 取消不得改变绑定"
    );
    assert!(!path.exists(), "取消录制不得写 shortcuts.json");
    expect_bounds(cx, "shortcut-value-search");
}

/// 验收 5（设置内）：录制中按原绑定的组合键不会同时触发命令。
///
/// 这里按 `Ctrl+K` 录制「加粗」：如果 capture 相没有吞掉按键，编辑器/宿主会先打开
/// 搜索浮层；断言搜索浮层不存在，且加粗绑定确实被改成 Ctrl+K。
#[gpui::test]
async fn capture_swallows_keys_before_other_handlers(cx: &mut TestAppContext) {
    let guard = shortcuts::lock_for_test();
    let (_dir, layout) = temp_layout("swallow");
    configure(&guard, &layout);
    let (root, cx) = open_root(cx, options(layout.clone()));
    cx.run_until_parked();
    open_settings(&root, cx);
    open_shortcuts_section(cx);

    click_selector(cx, "shortcut-edit-format_bold");
    cx.simulate_keystrokes("ctrl-k");
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("search-overlay").is_none(),
        "录制态按 Ctrl+K 不得被当成「打开搜索」"
    );
    assert_eq!(
        shortcuts::binding(CommandId::FormatBold),
        vec![key("ctrl-k")]
    );
}

/// 验收（C）：`Ctrl+,` 在 Welcome 直接打开设置；已打开时不重复触发。
#[gpui::test]
async fn ctrl_comma_opens_settings(cx: &mut TestAppContext) {
    let guard = shortcuts::lock_for_test();
    let (_dir, layout) = temp_layout("open-settings");
    configure(&guard, &layout);
    let (root, cx) = open_root(cx, options(layout));
    cx.run_until_parked();
    assert!(!root.update(cx, |root, _| root.settings_is_open()));

    cx.simulate_keystrokes("ctrl-,");
    cx.run_until_parked();
    assert!(
        root.update(cx, |root, _| root.settings_is_open()),
        "Ctrl+, 必须打开设置页（Welcome 也可用）"
    );
    expect_bounds(cx, "dev-sync-settings");

    // 设置已打开：再按一次不重复打开（仍是同一个面板，不崩溃）。
    cx.simulate_keystrokes("ctrl-,");
    cx.run_until_parked();
    assert!(root.update(cx, |root, _| root.settings_is_open()));
    expect_bounds(cx, "dev-sync-settings");
}

/// 验收（C）：Main 里由 `AppRoot` 统一路由 `Ctrl+,` / `Ctrl+N` / `Ctrl+G`。
///
/// 产品默认构建用「仅本地使用」进入真实 Main（dev-sync 构建的本地入口是另一条
/// dev vault 路径，行为不同，这里显式跳过；AppShell 独立入口的按键测试见
/// `headless_shortcut_keys.rs`）。
#[cfg(not(feature = "dev-sync"))]
#[gpui::test]
async fn main_global_shortcuts_are_routed_by_app_root(cx: &mut TestAppContext) {
    let guard = shortcuts::lock_for_test();
    let (_dir, layout) = temp_layout("main-keys");
    configure(&guard, &layout);
    let mut options = options(layout.clone());
    options.allow_local_vault = true;
    let (root, cx) = open_root(cx, options);
    cx.run_until_parked();

    // Welcome → 新建本地仓库（默认目录）→ Main（空库）。
    click_selector(cx, "welcome-new-local");
    click_selector(cx, "welcome-new-local-confirm");
    assert!(
        root.update(cx, |root, _| root.main_shell().is_some()),
        "仅本地使用必须进入 Main"
    );

    // Ctrl+, 打开 AppRoot 设置页。
    cx.simulate_keystrokes("ctrl-,");
    cx.run_until_parked();
    assert!(
        root.update(cx, |root, _| root.settings_is_open()),
        "Main 里 Ctrl+, 必须打开设置"
    );
    click_selector(cx, "settings-close");

    // Ctrl+N 新建笔记（空库 → 出现侧栏第一行）。
    assert!(cx.debug_bounds("tree-row-0").is_none(), "本地空库没有笔记");
    cx.simulate_keystrokes("ctrl-n");
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("tree-row-0").is_some(),
        "Main 里 Ctrl+N 必须新建笔记"
    );

    // Enter 结束大标题编辑后 Ctrl+G 打开图谱（标题编辑态会挡全局快捷键）。
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    cx.simulate_keystrokes("ctrl-g");
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("graph-overlay").is_some(),
        "Main 里 Ctrl+G 必须打开图谱"
    );
}
