//! dev-sync 面板的 headless UI 测试（ADR 0009 Phase 1 / 切片 1）。
//!
//! 只验三件**布局/挂载**事实（像素/IME/真机观感仍靠 Windows `ui_smoke.ps1`）：
//!
//! 1. feature + 运行时启用时，`AppShell` 上真的挂出了面板与「同步」「停止」按钮
//!    （selector 有非零 bounds）；顶部**没有**「创建」「加入」（真实入口在 AppRoot），
//!    也没有发布入口（没有管理身份/基线事实时不应凭空渲染）；
//! 2. 未启用时（`AppShell::new` 这条默认构造路径）面板 selector **不存在**——即"不渲染、
//!    不占布局"；
//! 3. 面板是覆盖层：它出现时右侧编辑器面板仍然存在、根视图仍占满窗口（不挤压布局）。
//!
//! 收敛点仍是 `run_until_parked()`（唯一），**不 sleep**；临时库建在 `CARGO_TARGET_TMPDIR`。

#![cfg(feature = "dev-sync")]

use std::path::Path;

use gpui::{
    div, AppContext as _, Entity, InteractiveElement as _, Modifiers, ParentElement as _, Render,
    Styled as _, Subscription, TestAppContext,
};
use menghunote_app::dev_sync::gate::DevSyncConfig;
use menghunote_app::dev_sync::service::ServiceConfig;
use menghunote_app::dev_vault::{self, DevVault};
use menghunote_app::shell::AppShell;
use tempfile::{Builder as TempDirBuilder, TempDir};

/// 窗口根视图：给 `AppShell` 一个稳定的宿主 selector（`AppShell` 不是 div）。
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

/// 在 `CARGO_TARGET_TMPDIR`（`target/tmp`）下开一个独立临时 dev vault。
fn open_temp_dev_vault() -> (TempDir, DevVault) {
    let root =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("dev-sync-ui-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("创建 target/tmp 下的测试根目录失败");
    let dir = TempDirBuilder::new()
        .prefix("vault-")
        .tempdir_in(&root)
        .expect("在 target/tmp 下创建临时目录失败");
    let dev = dev_vault::open_vault_at(&dir.path().join("vault.sqlite3"))
        .expect("打开临时 dev vault 失败");
    (dir, dev)
}

/// 运行时门禁通过时用的配置（relay 只做显示/校验，不发起网络请求）。
fn dev_sync_config() -> DevSyncConfig {
    DevSyncConfig {
        relay_mode: menghunote_app::onboarding::RelayMode::LocalDev,
        vault: std::path::PathBuf::from("/tmp/menghunote-dev-sync-ui/vault.sqlite3"),
        service: ServiceConfig::new("http://127.0.0.1:18080"),
    }
}

/// 启用：面板与 5 个按钮必须真实参与布局（bounds 非零），且不挤压编辑器。
#[gpui::test]
async fn dev_sync_panel_and_buttons_are_mounted_when_enabled(cx: &mut TestAppContext) {
    let (_dir, dev) = open_temp_dev_vault();
    let vault = dev.vault.clone();
    let config = dev_sync_config();
    let (_host, cx) = cx.add_window_view(move |_, cx| {
        let shell = cx.new(|cx| {
            AppShell::new_with_dev_sync(
                vault,
                "dev-sync-ui-enabled".to_owned(),
                config,
                // 切片 2b：AppShell 接收完整 SyncContext（切片 1 的面板测试只需要一个合法上下文）。
                menghunote_storage::SyncContext::new(dev_vault::DEV_PEER_ID, 1),
                cx,
            )
        });
        shell.update(cx, |shell, cx| shell.open_settings_headless(cx));
        ShellHost { shell }
    });
    cx.run_until_parked();

    let panel = cx
        .debug_bounds("dev-sync-panel")
        .expect("启用 dev-sync 后必须画出 selector=dev-sync-panel");
    assert!(
        panel.size.width.as_f32() > 0.0 && panel.size.height.as_f32() > 0.0,
        "面板必须有非零尺寸（真实布局，不是退化尺寸）：{panel:?}"
    );
    for selector in [
        "dev-sync-state",
        "dev-sync-relay",
        "dev-sync-error",
        "dev-sync-sync-state",
        "dev-sync-sync",
        "dev-sync-stop",
    ] {
        let bounds = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("必须画出 selector={selector}"));
        assert!(
            bounds.size.width.as_f32() > 0.0 && bounds.size.height.as_f32() > 0.0,
            "{selector} 必须有非零尺寸：{bounds:?}"
        );
    }
    // 个人用户默认只看到设备列表/邀请/移除；轮换是实验性高级入口。
    assert!(
        cx.debug_bounds("dev-sync-rotation").is_none(),
        "默认设备页不得出现轮换高级入口"
    );
    // 面板不再重复 Create/Join 入口；发布入口只在"有管理钥 + 基线已知缺失"时渲染
    // （本夹具没有身份/relay，事实未知 → 不渲染）。
    for selector in ["dev-sync-create", "dev-sync-join", "dev-sync-publish"] {
        assert!(
            cx.debug_bounds(selector).is_none(),
            "不得渲染 selector={selector}"
        );
    }
    // 覆盖层不挤压编辑器：两者都必须在。
    assert!(
        cx.debug_bounds("editor-pane").is_some(),
        "面板是覆盖层，右侧编辑器面板必须还在"
    );
    let host = cx
        .debug_bounds("app-shell-host")
        .expect("根视图必须画出 headless 帧");
    assert!(
        host.size.width.as_f32() > 0.0 && host.size.height.as_f32() > 0.0,
        "根视图必须占满窗口：{host:?}"
    );
    println!("[dev-sync-ui] panel = {panel:?}，host = {host:?}");
}

/// 未启用（默认构造路径）：面板不渲染、不占布局。
#[gpui::test]
async fn dev_sync_panel_is_absent_when_not_enabled(cx: &mut TestAppContext) {
    let (_dir, dev) = open_temp_dev_vault();
    let vault = dev.vault.clone();
    let (_host, cx) = cx.add_window_view(move |_, cx| {
        let shell = cx.new(|cx| AppShell::new(vault, "dev-sync-ui-disabled".to_owned(), cx));
        shell.update(cx, |shell, cx| shell.open_settings_headless(cx));
        ShellHost { shell }
    });
    cx.run_until_parked();

    for selector in [
        "dev-sync-panel",
        "dev-sync-state",
        "dev-sync-relay",
        "dev-sync-error",
        "dev-sync-sync-state",
        "dev-sync-create",
        "dev-sync-join",
        "dev-sync-sync",
        "dev-sync-publish",
        "dev-sync-stop",
        "dev-sync-rotate",
        "dev-sync-apply-epoch",
    ] {
        assert!(
            cx.debug_bounds(selector).is_none(),
            "未启用 dev-sync 时不该画出 selector={selector}（默认构建零行为变化）"
        );
    }
    // 默认布局照旧：根视图与编辑器都在。
    let host = cx
        .debug_bounds("app-shell-host")
        .expect("未启用 dev-sync 时根视图必须照常画出");
    assert!(
        host.size.width.as_f32() > 0.0 && host.size.height.as_f32() > 0.0,
        "根视图必须占满窗口：{host:?}"
    );
    assert!(
        cx.debug_bounds("editor-pane").is_some(),
        "未启用 dev-sync 时右侧编辑器面板必须照旧存在"
    );
    println!("[dev-sync-ui] 未启用：panel 不存在，host = {host:?}");
}

#[gpui::test]
async fn sync_settings_can_close_and_reopen_from_the_bottom_status_bar(cx: &mut TestAppContext) {
    let (_dir, dev) = open_temp_dev_vault();
    let vault = dev.vault.clone();
    let config = dev_sync_config();
    let (_host, cx) = cx.add_window_view(move |_, cx| {
        let shell = cx.new(|cx| {
            AppShell::new_with_dev_sync(
                vault,
                "dev-sync-ui-settings".to_owned(),
                config,
                menghunote_storage::SyncContext::new(dev_vault::DEV_PEER_ID, 1),
                cx,
            )
        });
        shell.update(cx, |shell, cx| shell.open_settings_headless(cx));
        ShellHost { shell }
    });
    cx.run_until_parked();

    let close = cx
        .debug_bounds("settings-close")
        .expect("设置页必须有关闭按钮");
    cx.simulate_click(close.center(), Modifiers::default());
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("settings-panel").is_none(),
        "关闭后设置卡片必须消失"
    );
    assert!(
        cx.debug_bounds("dev-sync-panel").is_none(),
        "关闭后同步面板不得继续常驻"
    );
    assert!(
        cx.debug_bounds("sync-status").is_some(),
        "底栏必须保留同步状态"
    );
    let gear = cx
        .debug_bounds("sync-status")
        .expect("底栏同步状态必须可见");
    cx.simulate_click(gear.center(), Modifiers::default());
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("settings-panel").is_some(),
        "点底部状态必须重新打开设置页"
    );
    assert!(
        cx.debug_bounds("dev-sync-panel").is_some(),
        "打开设置后同步面板必须可见"
    );
}

#[gpui::test]
async fn sync_panel_is_hidden_until_settings_opened(cx: &mut TestAppContext) {
    let (_dir, dev) = open_temp_dev_vault();
    let vault = dev.vault.clone();
    let config = dev_sync_config();
    let (_host, cx) = cx.add_window_view(move |_, cx| {
        let shell = cx.new(|cx| {
            AppShell::new_with_dev_sync(
                vault,
                "dev-sync-ui-hidden".to_owned(),
                config,
                menghunote_storage::SyncContext::new(dev_vault::DEV_PEER_ID, 1),
                cx,
            )
        });
        ShellHost { shell }
    });
    cx.run_until_parked();

    assert!(
        cx.debug_bounds("dev-sync-panel").is_none(),
        "默认启动不得常驻同步面板"
    );
    assert!(
        cx.debug_bounds("settings-panel").is_none(),
        "默认启动不得打开设置页"
    );
    assert!(
        cx.debug_bounds("sync-status").is_some(),
        "底栏必须显示同步状态"
    );
    let gear = cx.debug_bounds("sync-status").expect("底栏必须有同步状态");
    cx.simulate_click(gear.center(), Modifiers::default());
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("dev-sync-panel").is_some(),
        "点齿轮后同步面板才进入布局"
    );
}

/// 宿主接管（生产 `AppRoot`）：底栏 `sync-status` 只发 `OpenSyncSettings` 事件，
/// 不打开 `AppShell` 自己的组合设置页（`settings-panel` 不得出现）。
#[gpui::test]
async fn host_managed_status_click_emits_event_without_opening_shell_settings(
    cx: &mut TestAppContext,
) {
    struct ManagedHost {
        shell: Entity<AppShell>,
        sync_events: usize,
        _sub: Option<Subscription>,
    }

    impl Render for ManagedHost {
        fn render(
            &mut self,
            _window: &mut gpui::Window,
            _cx: &mut gpui::Context<Self>,
        ) -> impl gpui::IntoElement {
            div()
                .size_full()
                .debug_selector(|| "managed-host".to_string())
                .child(self.shell.clone())
        }
    }

    let (_dir, dev) = open_temp_dev_vault();
    let vault = dev.vault.clone();
    let config = dev_sync_config();
    let (host, cx) = cx.add_window_view(move |_, cx| {
        let shell = cx.new(|cx| {
            AppShell::new_with_dev_sync(
                vault,
                "dev-sync-ui-managed".to_owned(),
                config,
                menghunote_storage::SyncContext::new(dev_vault::DEV_PEER_ID, 1),
                cx,
            )
        });
        shell.update(cx, |shell, _| shell.set_settings_host_managed(true));
        let mut host = ManagedHost {
            shell,
            sync_events: 0,
            _sub: None,
        };
        let sub = cx.subscribe(
            &host.shell,
            |host: &mut ManagedHost,
             _shell,
             event: &menghunote_app::shell::AppShellEvent,
             cx: &mut gpui::Context<ManagedHost>| match *event {
                menghunote_app::shell::AppShellEvent::OpenSyncSettings => {
                    host.sync_events += 1;
                    cx.notify();
                }
            },
        );
        host._sub = Some(sub);
        host
    });
    cx.run_until_parked();

    let status = cx
        .debug_bounds("sync-status")
        .expect("底栏必须有 sync-status");
    cx.simulate_click(status.center(), Modifiers::default());
    cx.run_until_parked();

    assert_eq!(
        cx.update(|_window, app| host.read(app).sync_events),
        1,
        "点击底栏必须发出一次 OpenSyncSettings 事件"
    );
    assert!(
        cx.debug_bounds("settings-panel").is_none(),
        "宿主接管时不得打开 AppShell 旧组合设置页"
    );
    assert!(
        cx.debug_bounds("dev-sync-panel").is_none(),
        "宿主接管时同步面板只能由 AppRoot 设置页渲染"
    );
}
