//! 第一方「模板」「日记」插件设置页的 headless 门禁（`sync-ui` 产品构建）。
//!
//! 覆盖（真实 `AppRoot` 设置页 + 临时公开配置目录，**不 sleep**）：
//!
//! 1. 设置页真实画出「模板」「日记」两区，固定 selector（`settings-templates*` /
//!    `settings-daily-notes*`）都有非零尺寸；
//! 2. 日期/时间示例行随设置变化（当前值的真实格式化结果，不是写死文案）；
//! 3. 字段可打字：Enter 提交 → 写本机 `templates.json` / `daily-notes.json`
//!    （原子写），再读回一致；空字段被拒绝且不写盘；
//! 4. 启停只在「插件」页总开关，插件设置区不再重复开关；
//! 5. 打开设置页不改变既有外观/语言区（它们仍在、仍可点）；
//! 6. 「插件」分类总开关页：关闭模板/日记后左栏分类消失、写盘、重启读盘仍关闭，
//!    程序化停用当前插件分类会自动回退到「插件」页（内容非空）。
//!
//! 像素/DPI 仍必须 Windows 真机验收；这里证明状态机、持久化与 selector 契约。

#![cfg(feature = "sync-ui")]

use std::path::Path;

use gpui::{
    point, px, Entity, Modifiers, ScrollDelta, ScrollEvent, ScrollSource, TestAppContext,
    TouchPhase, VisualTestContext,
};
use menghunote_app::daily_notes;
use menghunote_app::onboarding::app_root::SettingsSection;
use menghunote_app::onboarding::{AppRoot, AppRootOptions, ProfileLayout};
use menghunote_app::templates;
use tempfile::{Builder as TempDirBuilder, TempDir};

/// 在 `CARGO_TARGET_TMPDIR`（`target/tmp`）下开独立临时目录 + 显式公开布局。
fn temp_layout(prefix: &str) -> (TempDir, ProfileLayout) {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "headless-notes-plugins-{prefix}-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).expect("创建 target/tmp 下的测试根目录失败");
    let dir = TempDirBuilder::new()
        .prefix("case-")
        .tempdir_in(&root)
        .expect("在 target/tmp 下创建临时目录失败");
    let layout = ProfileLayout::new(
        dir.path().join("config/sync.json"),
        dir.path().join("data/instances"),
    );
    (dir, layout)
}

fn product_options(layout: ProfileLayout) -> AppRootOptions {
    AppRootOptions {
        layout,
        default_relay: "https://relay.example".to_owned(),
        instance_id: Some("inst-product".to_owned()),
        allow_local_vault: false,
        operation_budget: std::time::Duration::from_secs(2),
    }
}

/// 偏好目录（同一测试内模板/日记各一个 JSON）。
fn temp_preferences_dir(prefix: &str) -> TempDir {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "headless-notes-plugins-prefs-{prefix}-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).expect("创建 target/tmp 下的偏好根目录失败");
    TempDirBuilder::new()
        .prefix("prefs-")
        .tempdir_in(&root)
        .expect("在 target/tmp 下创建临时偏好目录失败")
}

fn open_root(
    cx: &mut TestAppContext,
    layout: ProfileLayout,
) -> (Entity<AppRoot>, &mut VisualTestContext) {
    let options = product_options(layout);
    let (root, cx) = cx.add_window_view(move |_window, cx| AppRoot::new(options, cx));
    cx.run_until_parked();
    (root, cx)
}

fn open_settings(root: &Entity<AppRoot>, cx: &mut VisualTestContext) {
    cx.update(|_window, app| {
        root.update(app, |root, cx| {
            root.open_settings(cx);
        });
    });
    cx.run_until_parked();
    assert!(
        cx.update(|_window, app| root.read(app).settings_is_open()),
        "设置页必须真的打开"
    );
}

fn bounds_of(cx: &mut VisualTestContext, selector: &'static str) -> gpui::Bounds<gpui::Pixels> {
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("设置页必须画出 selector={selector}"));
    assert!(
        bounds.size.width.as_f32() > 0.0 && bounds.size.height.as_f32() > 0.0,
        "{selector} 必须有非零尺寸：{bounds:?}"
    );
    bounds
}

/// 设置卡片相对窗口滚动的目标：把 `target` 滚进可见区域（真实滚轮事件，不 sleep）。
///
/// 模板/日记两区追加在设置卡片**末尾**（不挪动既有同步/实例/relay 控件）；在默认
/// headless 窗口里它们需要向下滚动才可点击——这正是真实用户的操作路径。
fn scroll_into_view(cx: &mut VisualTestContext, target: &'static str) {
    let card = bounds_of(cx, "settings-content");
    let card_bottom = card.origin.y + card.size.height;
    for _ in 0..40 {
        let bounds = bounds_of(cx, target);
        let center = bounds.center();
        if center.y > card.origin.y && center.y < card_bottom {
            return;
        }
        assert!(
            center.y >= card_bottom,
            "{target} 需要在卡片上方（当前滚动方向只支持向下）：{bounds:?} / {card:?}"
        );
        // 负 delta = 滚轮向下（内容上移），与编辑器/表格测试的符号约定一致。
        cx.simulate_event(ScrollEvent {
            position: card.center(),
            delta: ScrollDelta::Pixels(point(px(0.0), px(-160.0))),
            modifiers: Modifiers::default(),
            touch_phase: TouchPhase::Moved,
            source: ScrollSource::Pointer,
        });
        cx.run_until_parked();
    }
    panic!("滚动后 {target} 仍不可见");
}

fn click_selector(cx: &mut VisualTestContext, selector: &'static str) {
    scroll_into_view(cx, selector);
    let bounds = bounds_of(cx, selector);
    cx.simulate_click(bounds.center(), Modifiers::default());
    cx.run_until_parked();
}

/// 点设置页左侧分类导航（`settings-nav-<section>`），右侧只渲染该分类。
fn open_settings_nav(cx: &mut VisualTestContext, section: &'static str) {
    let selector: &'static str = Box::leak(format!("settings-nav-{section}").into_boxed_str());
    let bounds = bounds_of(cx, selector);
    cx.simulate_click(bounds.center(), Modifiers::default());
    cx.run_until_parked();
    let section_selector: &'static str =
        Box::leak(format!("settings-section-{section}").into_boxed_str());
    bounds_of(cx, section_selector);
}

#[gpui::test]
async fn settings_page_shows_templates_and_daily_notes_sections(cx: &mut TestAppContext) {
    let templates_guard = templates::lock_for_test();
    let daily_guard = daily_notes::lock_for_test();
    let prefs_dir = temp_preferences_dir("sections");
    let templates_path = prefs_dir.path().join(templates::TEMPLATES_FILE_NAME);
    let daily_path = prefs_dir.path().join(daily_notes::DAILY_NOTES_FILE_NAME);
    templates_guard.reset(Some(templates_path.clone()));
    daily_guard.reset(Some(daily_path.clone()));

    let (_dir, layout) = temp_layout("sections");
    let (root, cx) = open_root(cx, layout);
    open_settings(&root, cx);

    // 设置页现在是「左侧分类导航 + 右侧只渲染当前分类」：
    // 先断言导航齐全 + 默认选中「外观」，再逐分类点开断言旧 selector 仍然存在。
    bounds_of(cx, "dev-sync-settings");
    bounds_of(cx, "settings-nav");
    // 「实例」分类已移除（vault 创建/管理改由 Main 左下角的「管理仓库」承担）。
    for section in [
        "appearance",
        "language",
        "shortcuts",
        "sync",
        "relay",
        "security",
        "plugins",
        "about",
    ] {
        let selector: &'static str = Box::leak(format!("settings-nav-{section}").into_boxed_str());
        bounds_of(cx, selector);
    }
    // 插件分类默认启用，显示在分界线之后。
    for section in ["templates", "daily_notes"] {
        let selector: &'static str = Box::leak(format!("settings-nav-{section}").into_boxed_str());
        bounds_of(cx, selector);
    }
    bounds_of(cx, "settings-nav-divider");
    assert!(
        cx.debug_bounds("settings-section-appearance").is_some(),
        "默认分类必须是「外观」"
    );
    bounds_of(cx, "settings-appearance");

    // 语言是独立分类。
    open_settings_nav(cx, "language");
    bounds_of(cx, "settings-language");
    assert!(
        cx.debug_bounds("settings-appearance").is_none(),
        "只渲染当前分类：切到语言后外观区必须不存在"
    );

    // 模板分类：模板区 + 全部固定 selector。
    open_settings_nav(cx, "templates");
    for selector in [
        "settings-templates",
        "settings-templates-folder",
        "settings-templates-date-format",
        "settings-templates-time-format",
        "settings-templates-example",
    ] {
        bounds_of(cx, selector);
    }
    assert!(
        cx.debug_bounds("settings-templates-toggle").is_none(),
        "插件启停只保留「插件」页总开关，模板设置区不得再有开关"
    );
    assert!(cx.debug_bounds("settings-templates-state").is_none());

    // 日记分类：日记区 + 全部固定 selector。
    open_settings_nav(cx, "daily_notes");
    for selector in [
        "settings-daily-notes",
        "settings-daily-notes-folder",
        "settings-daily-notes-date-format",
        "settings-daily-notes-template",
        "settings-daily-notes-example",
    ] {
        bounds_of(cx, selector);
    }
    assert!(
        cx.debug_bounds("settings-daily-notes-toggle").is_none(),
        "插件启停只保留「插件」页总开关，日记设置区不得再有开关"
    );
    assert!(cx.debug_bounds("settings-daily-notes-state").is_none());

    // 示例行是当前设置的真实格式化结果（默认 2026-11-18 样的形状由日期决定，
    // 只断言与 `templates::format_datetime` 的结果一致）。
    let prefs = templates::preferences();
    let parts = templates::DateTimeParts::now();
    let expected_example = format!(
        "{} {}",
        templates::format_datetime(&parts, &prefs.date_format),
        templates::format_datetime(&parts, &prefs.time_format)
    );
    assert!(
        menghunote_app::ui_text::templates_example(&expected_example).contains(&expected_example),
        "示例文案必须包含真实格式化结果"
    );

    // 启停只在「插件」页：总开关立即写盘（本机 JSON，不同步）。
    open_settings_nav(cx, "plugins");
    bounds_of(cx, "settings-section-plugins");
    assert!(templates::preferences().enabled, "默认启用");
    click_selector(cx, "settings-plugin-templates-toggle");
    assert!(!templates::preferences().enabled, "点击后必须停用");
    let saved = templates::load_preferences(&templates_path);
    assert!(!saved.enabled, "停用必须写盘：{}", templates_path.display());
    assert!(
        cx.debug_bounds("settings-nav-templates").is_none(),
        "停用后左栏不得再显示模板分类"
    );
    click_selector(cx, "settings-plugin-templates-toggle");
    assert!(templates::preferences().enabled);
    assert!(templates::load_preferences(&templates_path).enabled);

    click_selector(cx, "settings-plugin-daily_notes-toggle");
    assert!(!daily_notes::preferences().enabled);
    assert!(!daily_notes::load_preferences(&daily_path).enabled);
    click_selector(cx, "settings-plugin-daily_notes-toggle");
    assert!(daily_notes::preferences().enabled);
    assert!(daily_notes::load_preferences(&daily_path).enabled);

    // 设置页仍打开（开关只改本机偏好，不关闭面板、不重建主界面）。
    bounds_of(cx, "dev-sync-settings");
}

#[gpui::test]
async fn settings_fields_commit_on_enter_and_persist_atomically(cx: &mut TestAppContext) {
    let templates_guard = templates::lock_for_test();
    let daily_guard = daily_notes::lock_for_test();
    let prefs_dir = temp_preferences_dir("fields");
    let templates_path = prefs_dir.path().join(templates::TEMPLATES_FILE_NAME);
    let daily_path = prefs_dir.path().join(daily_notes::DAILY_NOTES_FILE_NAME);
    templates_guard.reset(Some(templates_path.clone()));
    daily_guard.reset(Some(daily_path.clone()));

    let (_dir, layout) = temp_layout("fields");
    let (root, cx) = open_root(cx, layout);
    open_settings(&root, cx);
    open_settings_nav(cx, "templates");

    // 点字段 → 焦点进 IME 输入框（捕获相聚焦）；单击**不选中**，输入追加到末尾。
    click_selector(cx, "settings-templates-folder");
    cx.simulate_input("_X");
    cx.run_until_parked();
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    let saved = templates::load_preferences(&templates_path);
    assert_eq!(
        saved.folder, "模板_X",
        "单击字段不得全选：输入必须追加在末尾（实际 {:?}）",
        saved.folder
    );

    // 显式 Ctrl+A 后才替换整段。
    click_selector(cx, "settings-templates-folder");
    cx.simulate_keystrokes("ctrl-a");
    cx.simulate_input("我的模板");
    cx.run_until_parked();
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    let saved = templates::load_preferences(&templates_path);
    assert_eq!(
        saved.folder,
        "我的模板",
        "Enter 必须提交字段并写盘（{}）",
        templates_path.display()
    );
    assert!(
        !templates_path.with_extension("json.tmp").exists(),
        "原子写完成后不能留下临时文件"
    );

    // 日期格式：Enter 提交后示例行随新格式变化（真实格式化，不是写死）。
    click_selector(cx, "settings-templates-date-format");
    cx.simulate_keystrokes("ctrl-a");
    cx.simulate_input("YYYY/MM");
    cx.run_until_parked();
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    let saved = templates::load_preferences(&templates_path);
    assert_eq!(saved.date_format, "YYYY/MM");
    let parts = templates::DateTimeParts::now();
    assert!(templates::format_datetime(&parts, &saved.date_format).contains('/'));

    // 空字段（Esc 放弃输入不写盘；这里用 ctrl-a + backspace + Enter 制造空提交）。
    let finger = templates::preferences();
    click_selector(cx, "settings-templates-time-format");
    cx.simulate_keystrokes("ctrl-a");
    cx.simulate_keystrokes("backspace");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(
        templates::preferences().time_format,
        finger.time_format,
        "空时间格式必须被拒绝（不改内存值）"
    );
    assert_eq!(
        templates::load_preferences(&templates_path).time_format,
        finger.time_format,
        "空时间格式必须被拒绝（不写盘）"
    );

    // 日记：模板路径允许空（= 无模板）；非空时反斜杠/首尾斜杠被规整。
    open_settings_nav(cx, "daily_notes");
    click_selector(cx, "settings-daily-notes-template");
    cx.simulate_input(" 模板\\日记模板 ");
    cx.run_until_parked();
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    let saved = daily_notes::load_preferences(&daily_path);
    assert_eq!(
        saved.template,
        "模板/日记模板",
        "模板路径必须规整后写盘（{}）",
        daily_path.display()
    );

    // 清空模板 → 「无模板」（Cell 模式允许空提交）。
    click_selector(cx, "settings-daily-notes-template");
    cx.simulate_keystrokes("ctrl-a");
    cx.simulate_keystrokes("backspace");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(
        daily_notes::load_preferences(&daily_path).template,
        "",
        "清空 = 无模板"
    );

    // 日期格式里的 `/` 在示例里就是子目录形态。
    click_selector(cx, "settings-daily-notes-date-format");
    cx.simulate_keystrokes("ctrl-a");
    cx.simulate_input("YYYY/MM/DD");
    cx.run_until_parked();
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    let saved = daily_notes::load_preferences(&daily_path);
    assert_eq!(saved.date_format, "YYYY/MM/DD");
    let today = daily_notes::note_path(&saved, &parts).expect("路径必须可算");
    assert!(
        today.full.contains('/'),
        "日期格式里的 / 必须产生子目录：{}",
        today.full
    );
}

/// 「插件」分类（Obsidian 核心插件式总开关页）：只列有运行时开关的第一方插件；
/// 关闭后左栏分类立即消失并写盘（同一份 `enabled`，不新建状态），重新打开后回来。
#[gpui::test]
async fn plugins_page_toggles_hide_nav_and_persist_to_disk(cx: &mut TestAppContext) {
    let templates_guard = templates::lock_for_test();
    let daily_guard = daily_notes::lock_for_test();
    let prefs_dir = temp_preferences_dir("plugins-page");
    let templates_path = prefs_dir.path().join(templates::TEMPLATES_FILE_NAME);
    let daily_path = prefs_dir.path().join(daily_notes::DAILY_NOTES_FILE_NAME);
    templates_guard.reset(Some(templates_path.clone()));
    daily_guard.reset(Some(daily_path.clone()));

    let (_dir, layout) = temp_layout("plugins-page");
    let (root, cx) = open_root(cx, layout);
    open_settings(&root, cx);
    open_settings_nav(cx, "plugins");

    // 总开关页：两行（名称/说明/开关/状态）+ 固定 selector。
    for selector in [
        "settings-plugins",
        "settings-plugin-templates",
        "settings-plugin-templates-toggle",
        "settings-plugin-templates-state",
        "settings-plugin-daily_notes",
        "settings-plugin-daily_notes-toggle",
        "settings-plugin-daily_notes-state",
    ] {
        bounds_of(cx, selector);
    }
    // 只列有运行时开关的插件：Mermaid/图谱没有可切换的运行时开关，不在这里。
    for absent in ["settings-plugin-mermaid", "settings-plugin-graph"] {
        assert!(
            cx.debug_bounds(absent).is_none(),
            "插件页不得列没有运行时开关的模块：{absent}"
        );
    }

    // 默认启用：分界线之后两个插件分类都在。
    bounds_of(cx, "settings-nav-templates");
    bounds_of(cx, "settings-nav-daily_notes");
    bounds_of(cx, "settings-nav-divider");

    // 关掉模板：立即隐藏左栏「模板」；写本机 templates.json；仍停在插件页。
    click_selector(cx, "settings-plugin-templates-toggle");
    assert!(!templates::preferences().enabled, "点击后必须停用模板");
    assert!(
        !templates::load_preferences(&templates_path).enabled,
        "停用必须写盘：{}",
        templates_path.display()
    );
    assert!(
        cx.debug_bounds("settings-nav-templates").is_none(),
        "模板关闭后左栏分类必须消失"
    );
    bounds_of(cx, "settings-nav-daily_notes");
    bounds_of(cx, "settings-section-plugins");
    // 关闭后这一行仍在（可以重新打开），状态文本变成「已停用」。
    bounds_of(cx, "settings-plugin-templates");
    bounds_of(cx, "settings-plugin-templates-state");

    // 模拟重启：从磁盘重新加载 → 仍是关闭；重开设置页后左栏仍没有模板。
    templates_guard.init(templates_path.clone());
    assert!(
        !templates::preferences().enabled,
        "重启读取 templates.json 后模板仍是关闭"
    );
    let cancel = bounds_of(cx, "settings-close");
    cx.simulate_click(cancel.center(), Modifiers::default());
    cx.run_until_parked();
    open_settings(&root, cx);
    assert!(
        cx.debug_bounds("settings-nav-templates").is_none(),
        "重启（重新读盘）后左栏不得显示已关闭的模板分类"
    );

    // 回到插件页重新打开：左栏分类立即回来，并写回 true。
    open_settings_nav(cx, "plugins");
    click_selector(cx, "settings-plugin-templates-toggle");
    assert!(templates::preferences().enabled);
    assert!(templates::load_preferences(&templates_path).enabled);
    bounds_of(cx, "settings-nav-templates");

    // 日记同理：关掉 → 左栏消失；打开 → 回来。
    click_selector(cx, "settings-plugin-daily_notes-toggle");
    assert!(!daily_notes::preferences().enabled);
    assert!(!daily_notes::load_preferences(&daily_path).enabled);
    assert!(
        cx.debug_bounds("settings-nav-daily_notes").is_none(),
        "日记关闭后左栏分类必须消失"
    );
    bounds_of(cx, "settings-nav-templates");
    click_selector(cx, "settings-plugin-daily_notes-toggle");
    assert!(daily_notes::preferences().enabled);
    bounds_of(cx, "settings-nav-daily_notes");
}

/// 停在插件自己的分类里关闭该插件：自动回退到「插件」页（不显示空白页）。
#[gpui::test]
async fn disabling_active_plugin_section_falls_back_to_plugins(cx: &mut TestAppContext) {
    let templates_guard = templates::lock_for_test();
    let _daily_guard = daily_notes::lock_for_test();
    let prefs_dir = temp_preferences_dir("plugin-fallback");
    let templates_path = prefs_dir.path().join(templates::TEMPLATES_FILE_NAME);
    templates_guard.reset(Some(templates_path.clone()));

    let (_dir, layout) = temp_layout("plugin-fallback");
    let (root, cx) = open_root(cx, layout);
    open_settings(&root, cx);
    open_settings_nav(cx, "templates");
    assert!(templates::preferences().enabled, "默认启用");

    // 模拟外部/程序化启停：停在模板分类时关闭插件，必须自动回退到「插件」。
    root.update(cx, |root, cx| {
        root.settings_toggle_plugin(SettingsSection::Templates, cx);
    });
    assert!(!templates::preferences().enabled);
    assert_eq!(
        root.update(cx, |root, _| root.settings_section()),
        SettingsSection::Plugins,
        "停在被关闭的插件分类时必须回退到「插件」"
    );
    // 内容非空：插件页 + 模板行都在；旧分类内容/导航项都不在。
    bounds_of(cx, "settings-section-plugins");
    bounds_of(cx, "settings-plugins");
    bounds_of(cx, "settings-plugin-templates");
    assert!(
        cx.debug_bounds("settings-section-templates").is_none(),
        "回退后不得继续显示模板分类内容"
    );
    assert!(
        cx.debug_bounds("settings-nav-templates").is_none(),
        "回退后左栏不得显示模板分类"
    );

    // 从插件页重新打开：左栏分类恢复。
    click_selector(cx, "settings-plugin-templates-toggle");
    assert!(templates::preferences().enabled);
    bounds_of(cx, "settings-nav-templates");
}
