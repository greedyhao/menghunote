//! ADR 0020 P1：真实 `AppShell` 设置页的「语言」切换（headless，`dev-sync` 构建）。
//!
//! 只验**设置页入口 + 立即生效 + 不重建编辑器**：
//!
//! 1. 设置页里有「语言」区与三个模式按钮（固定 selector，`debug_bounds` 非零）；
//! 2. 点「English」→ 全局语言变英文、偏好写盘、设置页仍打开、编辑器实体不变；
//! 3. 切语言不改变正文/选区/滚动/撤销栈/本地编辑代数（不产生 `EditRequest`）；
//! 4. 英文下侧栏/状态栏/右键菜单的固定文案来自同一 `ui_text` 入口；右键菜单
//!    真实画得出来（不 panic、不缺 selector）；
//! 5. 点「中文」「跟随系统」按预期解析（`System` == `Locale::detect()`）。
//!
//! 像素/DPI 仍需 Windows 真机验收（headless 只能驱动同一状态机）。

#![cfg(feature = "dev-sync")]

use std::path::Path;

use gpui::{
    div, AppContext as _, Entity, InteractiveElement as _, Modifiers, MouseButton,
    ParentElement as _, Render, Styled as _, TestAppContext, VisualTestContext,
};
use menghunote_app::dev_sync::gate::DevSyncConfig;
use menghunote_app::dev_sync::service::ServiceConfig;
use menghunote_app::dev_vault::{self, DevVault};
use menghunote_app::editor::theme;
use menghunote_app::editor::view::EditorView;
use menghunote_app::language_settings;
use menghunote_app::shell::AppShell;
use menghunote_app::ui_text;
use menghunote_i18n::{Locale, LocaleMode};
use tempfile::{Builder as TempDirBuilder, TempDir};

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

fn open_temp_dev_vault() -> (TempDir, DevVault) {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("headless-language-ui-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("创建 target/tmp 下的测试根目录失败");
    let dir = TempDirBuilder::new()
        .prefix("vault-")
        .tempdir_in(&root)
        .expect("在 target/tmp 下创建临时目录失败");
    let dev = dev_vault::open_vault_at(&dir.path().join("vault.sqlite3"))
        .expect("打开临时 dev vault 失败");
    (dir, dev)
}

fn dev_sync_config() -> DevSyncConfig {
    DevSyncConfig {
        relay_mode: menghunote_app::onboarding::RelayMode::LocalDev,
        vault: std::path::PathBuf::from("/tmp/menghunote-headless-language-ui/vault.sqlite3"),
        service: ServiceConfig::new("http://127.0.0.1:18080"),
    }
}

fn open_shell(
    cx: &mut TestAppContext,
    vault: menghunote_storage::Vault,
) -> (Entity<ShellHost>, &mut VisualTestContext) {
    let config = dev_sync_config();
    let (host, cx) = cx.add_window_view(move |_, cx| {
        let shell = cx.new(|cx| {
            AppShell::new_with_dev_sync(
                vault,
                "headless-language-ui".to_owned(),
                config,
                menghunote_storage::SyncContext::new(dev_vault::DEV_PEER_ID, 1),
                cx,
            )
        });
        shell.update(cx, |shell, cx| shell.open_settings_headless(cx));
        ShellHost { shell }
    });
    cx.run_until_parked();
    (host, cx)
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

fn click_selector(cx: &mut VisualTestContext, selector: &'static str) {
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("设置页必须画出 selector={selector}"));
    assert!(
        bounds.size.width.as_f32() > 0.0 && bounds.size.height.as_f32() > 0.0,
        "{selector} 必须有非零尺寸：{bounds:?}"
    );
    cx.simulate_click(bounds.center(), Modifiers::default());
    cx.run_until_parked();
}

fn reopen_settings(host: &Entity<ShellHost>, cx: &mut VisualTestContext) {
    cx.update(|_window, app| {
        let shell = host.read(app).shell.clone();
        shell.update(app, |shell, cx| shell.open_settings_headless(cx));
    });
    cx.run_until_parked();
}

fn close_shell(host: &Entity<ShellHost>, cx: &mut VisualTestContext) {
    cx.update(|_window, app| {
        let shell = host.read(app).shell.clone();
        shell.update(app, |shell, cx| shell.close_all(cx));
    });
    cx.run_until_parked();
}

/// 设置页切语言：立即生效、写盘、不重建编辑器、不动文档状态。
#[gpui::test]
async fn settings_page_switches_language_without_rebuilding_editor(cx: &mut TestAppContext) {
    let guard = language_settings::lock_for_test();
    let dir = open_temp_dev_vault();
    let language_path = dir.0.path().join(language_settings::LANGUAGE_FILE_NAME);
    guard.reset(Some(language_path.clone()));

    let (_dir, dev) = dir;
    let (host, cx) = open_shell(cx, dev.vault.clone());
    let view = active_view(&host, cx);
    let view_id = view.entity_id();

    // 设置页确实挂出了「语言」区与三个按钮（紧挨外观）。
    for selector in [
        "settings-appearance",
        "settings-language",
        "settings-language-zh-CN",
        "settings-language-en",
        "settings-language-system",
        "settings-language-current",
    ] {
        let bounds = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("设置页必须画出 selector={selector}"));
        assert!(
            bounds.size.width.as_f32() > 0.0,
            "{selector} 必须有非零宽度：{bounds:?}"
        );
    }

    // 先制造一个撤销组 + 选区 + 滚动（点编辑器会关闭设置覆盖层）。
    let pane = cx
        .debug_bounds("editor-current-doc")
        .expect("编辑器面板必须画出来");
    cx.simulate_click(
        gpui::point(
            pane.origin.x + gpui::px(theme::PAD_X + 2.0),
            pane.origin.y + gpui::px(theme::PAD_Y + 10.0),
        ),
        Modifiers::default(),
    );
    cx.run_until_parked();
    cx.simulate_keystrokes("home");
    cx.simulate_input("x");
    cx.run_until_parked();
    cx.simulate_keystrokes("shift-down");
    cx.run_until_parked();
    cx.update(|_window, app| {
        view.update(app, |view, cx| view.scroll_by(20.0, cx));
    });
    cx.run_until_parked();

    reopen_settings(&host, cx);
    assert!(
        cx.debug_bounds("settings-language-en").is_some(),
        "重新打开设置后必须仍有语言按钮"
    );

    let source = cx.update(|_window, app| view.read(app).source_text());
    let selection = cx.update(|_window, app| view.read(app).selection());
    let scroll_y = cx.update(|_window, app| view.read(app).scroll_y());
    let edit_seq = cx.update(|_window, app| view.read(app).dev_sync_local_edit_seq());

    // 点「English」：立即生效。
    click_selector(cx, "settings-language-en");
    assert_eq!(menghunote_i18n::current_locale(), Locale::En);
    assert_eq!(menghunote_i18n::mode(), LocaleMode::En);
    assert_eq!(ui_text::sidebar_notes(), "Notes");
    assert_eq!(ui_text::sidebar_new_note(), "+ Note");
    assert_eq!(ui_text::sidebar_new_folder(), "+ Folder");
    assert_eq!(ui_text::sidebar_export(), "Export");
    assert_eq!(ui_text::sidebar_graph(), "Graph");
    assert_eq!(ui_text::sidebar_settings(), "⚙ Settings");
    assert_eq!(ui_text::status_sync_attention(), "Sync: action needed");
    assert_eq!(ui_text::editor_menu_insert(), "Insert");
    assert_eq!(ui_text::editor_menu_table(), "Table");
    let raw = std::fs::read_to_string(&language_path).expect("切语言必须写本机偏好");
    assert!(raw.contains("\"locale\": \"en\""), "{raw}");

    // 设置页仍打开、编辑器实体不变（没有重建）。
    assert!(
        cx.debug_bounds("settings-panel").is_some(),
        "切语言不得关闭设置页"
    );
    assert_eq!(view.entity_id(), view_id, "切语言不得重建编辑器");

    // 文档状态原样保留，且没有新的本地编辑（不产生 EditRequest）。
    assert_eq!(
        cx.update(|_window, app| view.read(app).source_text()),
        source,
        "切语言不得改正文"
    );
    assert_eq!(
        cx.update(|_window, app| view.read(app).selection()),
        selection,
        "切语言不得丢选区"
    );
    assert_eq!(
        cx.update(|_window, app| view.read(app).scroll_y()),
        scroll_y,
        "切语言不得改变滚动位置"
    );
    assert_eq!(
        cx.update(|_window, app| view.read(app).dev_sync_local_edit_seq()),
        edit_seq,
        "切语言不得产生 EditRequest"
    );

    // 撤销栈仍在：Ctrl+Z 回退输入。
    cx.simulate_keystrokes("ctrl-z");
    cx.run_until_parked();
    assert_ne!(
        cx.update(|_window, app| view.read(app).source_text()),
        source,
        "切语言后撤销栈必须仍可用"
    );

    // 英文下右键菜单能真实画出来（菜单文案走 `ui_text`，不 panic）。
    click_selector(cx, "settings-close");
    assert!(
        cx.debug_bounds("settings-panel").is_none(),
        "点关闭必须关掉设置页"
    );
    let position = gpui::point(
        pane.origin.x + gpui::px(80.0),
        pane.origin.y + gpui::px(20.0),
    );
    cx.simulate_mouse_down(position, MouseButton::Right, Modifiers::default());
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("editor-context-menu").is_some(),
        "英文界面右键必须弹出菜单"
    );
    assert!(
        cx.debug_bounds("editor-context-insert-table").is_some(),
        "英文界面菜单里必须有「Table」项"
    );
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();

    // 点「中文」→ 中文；点「跟随系统」→ 按 LC_ALL/LANG 解析。
    reopen_settings(&host, cx);
    click_selector(cx, "settings-language-zh-CN");
    assert_eq!(menghunote_i18n::mode(), LocaleMode::ZhCn);
    assert_eq!(menghunote_i18n::current_locale(), Locale::ZhCn);
    assert_eq!(ui_text::sidebar_notes(), "笔记");
    assert_eq!(ui_text::sidebar_new_note(), "＋ 笔记");
    assert_eq!(ui_text::editor_menu_table(), "表格");
    let raw = std::fs::read_to_string(&language_path).expect("读回本机偏好");
    assert!(raw.contains("\"locale\": \"zh-CN\""), "{raw}");
    assert_eq!(view.entity_id(), view_id, "切回中文不得重建编辑器");

    click_selector(cx, "settings-language-system");
    assert_eq!(menghunote_i18n::mode(), LocaleMode::System);
    assert_eq!(
        menghunote_i18n::current_locale(),
        Locale::detect(),
        "跟随系统必须按 LC_ALL/LANG 解析"
    );
    let raw = std::fs::read_to_string(&language_path).expect("读回本机偏好");
    assert!(raw.contains("\"locale\": \"system\""), "{raw}");

    close_shell(&host, cx);
}
