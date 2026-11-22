//! P1（同步产品化）：**默认 `sync-ui` 构建**（不含 `dev-sync`）的 headless 门禁。
//!
//! 本文件只在"产品构建"下编译：`sync-ui` 打开、`dev-sync` 关闭。它补的是默认
//! `cargo test -p menghunote-app` 的产品路径覆盖（dev-sync 专属测试仍由各自
//! `#![cfg(feature = "dev-sync")]` 文件保留，不会静默变成 0 覆盖）：
//!
//! 1. 默认构建能构造并渲染 `AppRoot`，停在 Welcome（onboarding 入口确实存在，
//!    「仅本地使用（不同步）」入口 `app-root-local` 在 Welcome 上）；
//! 2. `local_dev` 在 UI 层被明确拒绝（create 链接应用阶段，不联网、不消费票据）；
//!    同一表单接受 `remote_tls` 链接，说明产品路径本身可用；
//! 3. 没有挂同步面板的 `AppShell`（`AppShell::new`）仍能渲染状态栏，设置入口是 no-op，
//!    不会因为 `sync-ui` 编译进来就 panic。
//!
//! A 组第二切片追加（P2 产品路径的可用性补全）：
//!
//! 4. P2 数据目录/迁移/DEV 标注（见 `product_startup_*`）；
//! 5. **仅本地使用**：点击 `app-root-local` → Main（无同步面板/同步状态），
//!    数据根下 `local/vault.sqlite3` 被创建；不写 `sync.json`、不建 `instances/`、
//!    不写身份文件；空库**不播种 demo**；本机 `startup.json` 记 `mode=local`，
//!    重启后**直接回到本地 Main**（库内容不重置）；
//! 6. 本地库打不开 → GUI Error 页（`set_error`），不静默退出。
//!
//! 纪律：临时目录在 `CARGO_TARGET_TMPDIR`，不 sleep，收敛点 `run_until_parked()`。

#![cfg(all(feature = "sync-ui", not(feature = "dev-sync")))]

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use gpui::{
    div, AppContext as _, Entity, InteractiveElement as _, Modifiers, ParentElement as _, Render,
    Styled as _, TestAppContext, VisualTestContext,
};
use menghunote_app::dev_vault;
use menghunote_app::onboarding::{
    AppRoot, AppRootOptions, ProfileLayout, RelayMode, RootStateLabel,
};
use menghunote_app::shell::AppShell;
use menghunote_app::ui_text;
use menghunote_sync::create_link::{CreateLink, CreateTicket};
use tempfile::{Builder as TempDirBuilder, TempDir};

/// 在 `CARGO_TARGET_TMPDIR`（`target/tmp`）下开独立临时目录 + 显式公开布局。
fn temp_layout(prefix: &str) -> (TempDir, ProfileLayout) {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("sync-ui-product-{prefix}-{}", std::process::id()));
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

/// 在临时根上注入平台目录环境变量，返回 `(env, 新配置路径, 旧配置路径)`。
///
/// 只影响这次 `AppRootOptions::discover` 的 env 闭包，**不写真实用户目录**。
fn product_env(root: &Path) -> (HashMap<String, String>, PathBuf, PathBuf) {
    let mut env: HashMap<String, String> = HashMap::new();
    #[cfg(not(windows))]
    let (new_config, legacy_config) = {
        let home = root.join("home");
        let config = root.join("config");
        let data = root.join("data");
        for dir in [&home, &config, &data] {
            std::fs::create_dir_all(dir).expect("创建临时 XDG 目录失败");
        }
        env.insert("HOME".to_owned(), home.to_string_lossy().into_owned());
        env.insert(
            "XDG_CONFIG_HOME".to_owned(),
            config.to_string_lossy().into_owned(),
        );
        env.insert(
            "XDG_DATA_HOME".to_owned(),
            data.to_string_lossy().into_owned(),
        );
        (
            config.join("menghunote").join("sync.json"),
            config.join("menghunote-dev").join("dev-sync.json"),
        )
    };
    #[cfg(windows)]
    let (new_config, legacy_config) = {
        let local = root.join("localappdata");
        std::fs::create_dir_all(&local).expect("创建临时 LOCALAPPDATA 失败");
        env.insert(
            "LOCALAPPDATA".to_owned(),
            local.to_string_lossy().into_owned(),
        );
        (
            local.join("menghunote").join("sync.json"),
            local.join("menghunote-dev").join("dev-sync.json"),
        )
    };
    (env, new_config, legacy_config)
}

fn product_options(layout: ProfileLayout) -> AppRootOptions {
    AppRootOptions {
        layout,
        // 产品构建默认 relay 不应是 local_dev；这里给一个 HTTPS 占位。
        default_relay: "https://relay.example".to_owned(),
        instance_id: Some("inst-product".to_owned()),
        // `for_test`/显式构造默认关闭本地入口；产品 `discover` 是打开的，这里的
        // 测试**显式打开**（A 组第二切片：产品 Welcome 必须有「仅本地使用」）。
        allow_local_vault: true,
        operation_budget: std::time::Duration::from_secs(2),
    }
}

/// 必须存在且非零尺寸的 selector（headless 断言布局参与）。
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

/// `AppShell` 侧栏当前展示的标题（先序 DFS；空库 = 空）。
fn sidebar_titles(shell: &Entity<AppShell>, cx: &mut VisualTestContext) -> Vec<String> {
    let shell = shell.clone();
    cx.update(|_window, app| {
        shell
            .read(app)
            .sidebar_titles()
            .into_iter()
            .map(str::to_owned)
            .collect()
    })
}

fn create_link(mode: RelayMode, relay: &str) -> String {
    CreateLink::encode(
        mode,
        relay,
        [0x11; 16],
        CreateTicket::from_bytes([0x22; 32]),
        None,
    )
    .expect("构造创建链接失败")
    .url
}

/// 1) 产品构建默认入口存在：`AppRoot` 渲染 Welcome，没有 dev-sync 专属设置面板。
#[gpui::test]
async fn product_root_starts_at_welcome(cx: &mut TestAppContext) {
    let (_dir, layout) = temp_layout("welcome");
    let options = product_options(layout);
    let (root, cx) = cx.add_window_view(move |_window, cx| AppRoot::new(options, cx));
    cx.run_until_parked();

    assert_eq!(
        root.update(cx, |root, _| root.state_label()),
        RootStateLabel::Welcome,
        "产品构建默认应停在 Welcome"
    );
    assert!(cx.debug_bounds("app-root").is_some(), "必须画出 app-root");
    assert!(
        cx.debug_bounds("app-root-welcome").is_some(),
        "Welcome 入口必须真实参与布局"
    );
    // P2/A2：产品构建不出现 DEV 警示条；Welcome 必须有「仅本地使用（不同步）」入口。
    assert!(
        cx.debug_bounds("app-root-dev-banner").is_none(),
        "产品构建不得出现 DEV 警示条"
    );
    assert!(
        cx.debug_bounds("app-root-local").is_some(),
        "产品构建必须有「仅本地使用（不同步）」入口"
    );
    assert_eq!(
        AppRoot::local_entry_label(),
        ui_text::local_action(),
        "产品入口文案必须走 ui_text（仅本地使用）"
    );
    assert!(
        !AppRoot::local_entry_label().contains("DEV"),
        "产品入口不得带 DEV 标注：{}",
        AppRoot::local_entry_label()
    );
    // V2 管理是 dev-only：产品构建即使设置了实验环境变量也不应存在。
    assert!(
        cx.debug_bounds("settings-toggle-management").is_none(),
        "产品构建不得出现 V2 管理入口"
    );
}

/// 2) local_dev 在应用创建链接时被明确拒绝；remote_tls 链接正常应用（不联网）。
#[gpui::test]
async fn product_root_rejects_local_dev_and_accepts_remote_tls(cx: &mut TestAppContext) {
    let (_dir, layout) = temp_layout("links");
    let options = product_options(layout);
    let (root, cx) = cx.add_window_view(move |_window, cx| AppRoot::new(options, cx));
    cx.run_until_parked();

    let local = create_link(RelayMode::LocalDev, "http://127.0.0.1:18080");
    root.update(cx, |root, cx| {
        root.enter_create_link_form(cx);
        root.form_set_create_link(&local, cx);
        root.form_apply_create_link(cx);
    });
    cx.run_until_parked();
    let error = root
        .update(cx, |root, _| root.form_error().map(str::to_owned))
        .expect("local_dev 链接必须被拒绝");
    assert!(
        error.contains("不开放 relay 模式") && error.contains("local_dev"),
        "错误必须说明构建不开放 local_dev：{error}"
    );
    assert!(
        !root.update(cx, |root, _| root.form_has_create_link()),
        "被拒绝的链接不得进入状态（票据不被消费）"
    );
    assert_eq!(
        root.update(cx, |root, _| root.form_mode()),
        Some(RelayMode::RemoteTls),
        "产品构建的表单默认模式必须是 remote_tls"
    );

    let remote = create_link(RelayMode::RemoteTls, "https://relay.example");
    root.update(cx, |root, cx| {
        root.form_set_create_link(&remote, cx);
        root.form_apply_create_link(cx);
    });
    cx.run_until_parked();
    assert!(
        root.update(cx, |root, _| root.form_has_create_link()),
        "remote_tls 链接应可应用"
    );
    assert_eq!(
        root.update(cx, |root, _| root.form_mode()),
        Some(RelayMode::RemoteTls)
    );
    assert_eq!(
        root.update(cx, |root, _| root.state_label()),
        RootStateLabel::CreateForm
    );
}

/// 3) 未挂同步面板的 AppShell（默认构造）不 panic；状态栏存在、设置入口是 no-op。
#[gpui::test]
async fn plain_shell_without_sync_panel_renders(cx: &mut TestAppContext) {
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

    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("sync-ui-product-shell-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("创建测试目录失败");
    let dir = TempDirBuilder::new()
        .prefix("vault-")
        .tempdir_in(&root)
        .expect("创建临时目录失败");
    let dev = dev_vault::open_vault_at(&dir.path().join("vault.sqlite3"))
        .expect("打开临时 dev vault 失败");

    let shell = cx.new(|cx| AppShell::new(dev.vault, "sync-ui-product".to_owned(), cx));
    let (host, cx) = cx.add_window_view(move |_window, _cx| ShellHost { shell });
    cx.run_until_parked();

    assert!(cx.debug_bounds("status-bar").is_some(), "状态栏必须存在");
    assert!(
        cx.debug_bounds("dev-sync-panel").is_none(),
        "产品构建没有同步面板实体"
    );

    let shell = cx.update(|_window, app| host.read(app).shell.clone());
    shell.update(cx, |shell, cx| {
        shell.open_settings_headless(cx);
        assert!(!shell.settings_open(), "没有同步面板时设置入口是 no-op");
        shell.close_all(cx);
    });
    cx.run_until_parked();
}

/// 4) P2/A2：产品构建默认目录 = `menghunote` / `sync.json`；`AppRoot` 启动引导把旧开发
///    配置一次性保守迁移（复制公开实例列表，库/身份/旧文件都不动），产品构建不显示
///    DEV 警示但**保留**「仅本地使用」入口。
#[gpui::test]
async fn product_startup_uses_new_layout_migrates_legacy_and_shows_local_entry(
    cx: &mut TestAppContext,
) {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("sync-ui-product-paths-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("创建测试根目录失败");
    let dir = TempDirBuilder::new()
        .prefix("paths-")
        .tempdir_in(&root)
        .expect("创建临时目录失败");
    let (env, new_config, legacy_config) = product_env(dir.path());

    // 旧开发目录里的 v2 配置（remote_tls；**没有 active**，启动后停在 Welcome）+ 库文件。
    let old_vault = dir
        .path()
        .join("old")
        .join("inst-old")
        .join("vault.sqlite3");
    std::fs::create_dir_all(old_vault.parent().expect("旧实例目录")).expect("建旧实例目录");
    std::fs::write(&old_vault, b"sqlite-bytes").expect("写旧库文件");
    let legacy_json = serde_json::json!({
        "version": 2,
        "instances": [{
            "instance_id": "inst-old",
            "relay_mode": "remote_tls",
            "api_version": 2,
            "relay_url": "https://relay.example",
            "vault_path": old_vault.to_string_lossy(),
            "vault_id": "00112233445566778899aabbccddeeff",
            "device_id": "ffeeddccbbaa99887766554433221100"
        }]
    });
    std::fs::create_dir_all(legacy_config.parent().expect("旧配置目录")).expect("建旧配置目录");
    std::fs::write(
        &legacy_config,
        serde_json::to_vec_pretty(&legacy_json).expect("序列化旧配置失败"),
    )
    .expect("写旧配置失败");
    let legacy_before = std::fs::read(&legacy_config).expect("读旧配置失败");

    let discover_env = env.clone();
    let options = AppRootOptions::discover(move |key| discover_env.get(key).cloned())
        .expect("产品构建必须能解析默认布局");
    assert_eq!(
        options.layout.config_path, new_config,
        "产品配置文件名必须是 sync.json"
    );
    assert!(
        options
            .layout
            .instances_dir
            .ends_with("menghunote/instances"),
        "实例目录必须是 menghunote/instances：{:?}",
        options.layout.instances_dir
    );
    assert!(
        options.allow_local_vault,
        "产品构建默认打开本地（不同步）入口"
    );
    assert!(
        options.default_relay.is_empty(),
        "产品构建默认 relay 不得是 loopback"
    );
    assert!(
        !new_config.exists(),
        "布局解析本身不写文件（迁移发生在 AppRoot 启动引导）"
    );

    // 真启动：迁移 → 读新配置 → 没有 active 停在 Welcome。
    let (app_root, cx) = cx.add_window_view(move |_window, cx| AppRoot::new(options, cx));
    cx.run_until_parked();
    assert!(
        new_config.exists(),
        "AppRoot 启动必须把旧配置迁移到 sync.json"
    );
    assert_eq!(
        app_root.update(cx, |root, _| root.state_label()),
        RootStateLabel::Welcome,
        "没有 active 实例时迁移后应停在 Welcome"
    );
    assert!(
        cx.debug_bounds("app-root-dev-banner").is_none(),
        "产品构建不得出现 DEV 警示条"
    );
    assert!(
        cx.debug_bounds("app-root-local").is_some(),
        "产品构建 Welcome 必须有「仅本地使用」入口"
    );
    assert_eq!(
        std::fs::read(&legacy_config).expect("旧配置必须仍在"),
        legacy_before,
        "旧配置必须原样保留（不删、不改写）"
    );
    assert_eq!(
        std::fs::read(&old_vault).expect("旧库必须仍在"),
        b"sqlite-bytes",
        "库文件不得移动/复制"
    );
    let value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&new_config).expect("读新配置失败"))
            .expect("新配置必须是 JSON");
    assert_eq!(value["version"], serde_json::json!(2), "新配置必须是 v2");
    assert_eq!(
        value["instances"][0]["vault_path"],
        serde_json::json!(old_vault.to_string_lossy().into_owned()),
        "vault_path 必须保持旧路径"
    );
    assert_eq!(value["instances"][0]["relay_mode"], "remote_tls");

    // 二次启动：幂等（新配置逐字节不变）。
    let new_before = std::fs::read(&new_config).expect("读新配置失败");
    let second =
        AppRootOptions::discover(move |key| env.get(key).cloned()).expect("二次启动必须可用");
    assert_eq!(second.layout.config_path, new_config);
    assert!(
        !second
            .layout
            .migrate_legacy_profile()
            .expect("二次迁移不报错"),
        "重复启动不得再迁移"
    );
    assert_eq!(
        std::fs::read(&new_config).expect("读新配置失败"),
        new_before,
        "重复启动不得改写新配置"
    );
}

/// 5) P2：旧配置损坏时产品构建**在 GUI 错误页提示**（不重建、不覆盖、不白屏）。
#[gpui::test]
async fn product_startup_shows_error_page_for_corrupt_legacy_config(cx: &mut TestAppContext) {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("sync-ui-product-corrupt-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("创建测试根目录失败");
    let dir = TempDirBuilder::new()
        .prefix("corrupt-")
        .tempdir_in(&root)
        .expect("创建临时目录失败");
    let (env, new_config, legacy_config) = product_env(dir.path());

    std::fs::create_dir_all(legacy_config.parent().expect("旧配置目录")).expect("建旧配置目录");
    std::fs::write(&legacy_config, b"{ not json").expect("写坏旧配置");
    let legacy_before = std::fs::read(&legacy_config).expect("读旧配置失败");

    let options = AppRootOptions::discover(move |key| env.get(key).cloned())
        .expect("目录解析与旧配置内容无关");
    let (app_root, cx) = cx.add_window_view(move |_window, cx| AppRoot::new(options, cx));
    cx.run_until_parked();

    assert_eq!(
        app_root.update(cx, |root, _| root.state_label()),
        RootStateLabel::Error,
        "旧配置损坏必须进 GUI 错误页（不是无提示退出/白屏）"
    );
    assert!(
        cx.debug_bounds("app-root-error").is_some(),
        "错误页必须真实渲染"
    );
    assert!(!new_config.exists(), "损坏的旧配置不得触发重建");
    assert_eq!(
        std::fs::read(&legacy_config).expect("旧配置必须仍在"),
        legacy_before,
        "损坏的旧配置必须原样保留"
    );
}

/// 6) P2：旧开发 `local_dev` 配置在产品构建里**跳过迁移**，不挡在错误页。
///
/// 这是升级最常见路径：v1/旧 dev 构建默认写 `local_dev`，产品构建不开放该模式；
/// 旧文件必须原样保留，产品以空配置进入 Welcome，用户可新建 remote_tls 实例。
#[gpui::test]
async fn product_startup_skips_legacy_local_dev_without_error_page(cx: &mut TestAppContext) {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("sync-ui-product-legacy-dev-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("创建测试根目录失败");
    let dir = TempDirBuilder::new()
        .prefix("legacy-dev-")
        .tempdir_in(&root)
        .expect("创建临时目录失败");
    let (env, new_config, legacy_config) = product_env(dir.path());

    let old_vault = dir
        .path()
        .join("old")
        .join("inst-dev")
        .join("vault.sqlite3");
    std::fs::create_dir_all(old_vault.parent().expect("旧实例目录")).expect("建旧实例目录");
    std::fs::write(&old_vault, b"sqlite-bytes").expect("写旧库文件");
    let legacy_json = serde_json::json!({
        "version": 2,
        "instances": [{
            "instance_id": "inst-dev",
            "relay_mode": "local_dev",
            "relay_url": "http://127.0.0.1:18080",
            "vault_path": old_vault.to_string_lossy(),
            "vault_id": "00112233445566778899aabbccddeeff",
            "device_id": "ffeeddccbbaa99887766554433221100"
        }]
    });
    std::fs::create_dir_all(legacy_config.parent().expect("旧配置目录")).expect("建旧配置目录");
    std::fs::write(
        &legacy_config,
        serde_json::to_vec_pretty(&legacy_json).expect("序列化旧配置失败"),
    )
    .expect("写旧配置失败");
    let legacy_before = std::fs::read(&legacy_config).expect("读旧配置失败");

    let options = AppRootOptions::discover(move |key| env.get(key).cloned())
        .expect("产品构建必须能解析默认布局");
    let (app_root, cx) = cx.add_window_view(move |_window, cx| AppRoot::new(options, cx));
    cx.run_until_parked();

    assert_eq!(
        app_root.update(cx, |root, _| root.state_label()),
        RootStateLabel::Welcome,
        "旧 local_dev 配置不应把产品挡在错误页"
    );
    assert!(
        cx.debug_bounds("app-root-welcome").is_some(),
        "应正常显示 Welcome"
    );
    assert!(!new_config.exists(), "不支持的旧模式不得写新配置");
    assert_eq!(
        std::fs::read(&legacy_config).expect("旧配置必须仍在"),
        legacy_before,
        "旧配置必须原样保留"
    );
    assert_eq!(
        std::fs::read(&old_vault).expect("旧库必须仍在"),
        b"sqlite-bytes",
        "旧库不得移动/复制"
    );
}

/// 7) A 组第二切片：产品「仅本地使用（不同步）」——不联网、不建同步实例、不播种 demo。
///
/// 覆盖：入口存在 → 点击进 Main → `local/vault.sqlite3` 创建 → 无 `sync.json` /
/// 无 `instances/` / 无身份文件 → 空库（侧栏空）但可新建 → 无同步面板/同步状态/DEV 标注。
#[gpui::test]
async fn product_local_entry_opens_empty_local_vault_without_sync(cx: &mut TestAppContext) {
    let (_dir, layout) = temp_layout("local-open");
    let vault_path = layout.local_vault_path();
    let config_path = layout.config_path.clone();
    let instances_dir = layout.instances_dir.clone();
    let identity_path = layout.local_dir().join("dev-identity.bin");
    let options = product_options(layout.clone());

    // 注册真实关窗分派：本测试要模拟"再次启动"，必须走与 `main.rs` 相同的路径。
    let (root, cx) = cx.add_window_view(move |window, cx| {
        let root = AppRoot::new(options, cx);
        AppRoot::install_close_handler(window, cx.entity(), cx);
        root
    });
    cx.run_until_parked();

    assert_eq!(
        root.update(cx, |root, _| root.state_label()),
        RootStateLabel::Welcome
    );
    assert!(
        !vault_path.exists(),
        "进入 Welcome 本身不得创建本地库（只有点入口才打开）"
    );
    let local = expect_bounds(cx, "app-root-local");

    // 真实点击入口（不是直接调方法）：验证 Welcome 的动作接线。
    cx.simulate_click(local.center(), Modifiers::default());
    cx.run_until_parked();

    assert_eq!(
        root.update(cx, |root, _| root.state_label()),
        RootStateLabel::Main,
        "点击「仅本地使用」必须进入 Main"
    );
    assert!(root.update(cx, |root, _| root.has_main_shell()));
    assert!(
        root.update(cx, |root, _| root.is_local_mode()),
        "Main 必须标记为本地（不同步）模式"
    );
    assert_eq!(
        root.update(cx, |root, _| root.notice().map(str::to_owned)),
        Some(ui_text::local_mode_notice()),
        "本地模式提示必须走 ui_text「本地模式：不同步」"
    );
    expect_bounds(cx, "app-root-main");
    expect_bounds(cx, "app-root-local-mode");
    assert!(
        cx.debug_bounds("dev-sync-panel").is_none(),
        "本地模式不得出现同步面板"
    );
    assert!(
        cx.debug_bounds("sync-status").is_none(),
        "本地模式不得出现同步状态"
    );
    assert!(
        cx.debug_bounds("app-root-dev-banner").is_none(),
        "本地模式不得出现 DEV 标注"
    );

    // 文件落点与数据安全：只建本地库，不登记同步、不建实例、不写身份。
    assert!(
        vault_path.is_file(),
        "本地库必须创建在 <数据根>/local/vault.sqlite3：{vault_path:?}"
    );
    assert_eq!(
        vault_path,
        layout
            .data_root()
            .join(menghunote_app::onboarding::LOCAL_DIR_NAME)
            .join("vault.sqlite3"),
        "本地库路径必须是 ProfileLayout 数据根下 local/vault.sqlite3"
    );
    assert!(!config_path.exists(), "本地入口不得写 sync.json");
    assert!(!instances_dir.exists(), "本地入口不得创建 instances/ 目录");
    assert!(!identity_path.exists(), "本地入口不得写身份文件");

    // 空库：没有 demo 笔记，但编辑器与「＋ 笔记」都正常。
    let shell = root
        .update(cx, |root, _| root.main_shell().cloned())
        .expect("Main 必须有真实 AppShell");
    assert!(
        sidebar_titles(&shell, cx).is_empty(),
        "本地空库不得播种 demo 笔记"
    );
    let new_note = expect_bounds(cx, "sidebar-new-note");
    cx.simulate_click(new_note.center(), Modifiers::default());
    cx.run_until_parked();
    assert_eq!(
        sidebar_titles(&shell, cx).len(),
        1,
        "本地库必须能正常新建笔记"
    );

    // ---- 再次启动：本机启动偏好记 `local`，直接回到同一本地库（不重置、不重复播种） ----
    let startup_path = layout.startup_path();
    assert!(startup_path.is_file(), "本地打开成功必须写本机启动偏好");
    let startup: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&startup_path).expect("读启动偏好失败"))
            .expect("启动偏好必须是 JSON");
    assert_eq!(
        startup["mode"], "local",
        "本地打开成功必须记 mode=local（不同步）"
    );
    assert!(cx.simulate_close(), "本地 Main 允许关窗（没有同步收尾）");
    cx.run_until_parked();
    let restarted = cx.update(|window, cx| {
        let options = product_options(layout.clone());
        window.replace_root(cx, move |_window, cx| AppRoot::new(options, cx))
    });
    cx.run_until_parked();
    assert_eq!(
        restarted.update(cx, |root, _| root.state_label()),
        RootStateLabel::Main,
        "本机启动偏好 local + 本地库存在：重启必须直接进本地 Main（不再回 Welcome）"
    );
    assert!(
        restarted.update(cx, |root, _| root.is_local_mode()),
        "自动恢复的 Main 必须仍是本地（不同步）模式"
    );
    assert!(
        restarted.update(cx, |root, _| root.has_main_shell()),
        "自动恢复必须构造真实 AppShell"
    );
    expect_bounds(cx, "app-root-main");
    expect_bounds(cx, "app-root-local-mode");
    let shell = restarted
        .update(cx, |root, _| root.main_shell().cloned())
        .expect("自动恢复必须有 AppShell");
    assert_eq!(
        sidebar_titles(&shell, cx).len(),
        1,
        "再次打开同一路径必须复用已有库（笔记仍在、不重置）"
    );
    assert!(vault_path.is_file(), "复用后库文件仍是同一路径");
    assert!(!config_path.exists(), "再次打开也不得写 sync.json");
}

/// 8) A 组第二切片：本地库打不开 → GUI Error 页（不是静默退出/白屏）。
#[gpui::test]
async fn product_local_open_failure_lands_on_error_page(cx: &mut TestAppContext) {
    let (_dir, layout) = temp_layout("local-fail");
    let vault_path = layout.local_vault_path();
    std::fs::create_dir_all(vault_path.parent().expect("本地库有父目录"))
        .expect("创建 local/ 目录失败");
    // 非 SQLite 字节：打开必然失败（不覆盖、不重建）。
    std::fs::write(&vault_path, b"this is not a sqlite database").expect("写坏库文件失败");
    let broken_before = std::fs::read(&vault_path).expect("读坏库文件失败");
    let options = product_options(layout);

    let (root, cx) = cx.add_window_view(move |window, cx| {
        let root = AppRoot::new(options, cx);
        AppRoot::install_close_handler(window, cx.entity(), cx);
        root
    });
    cx.run_until_parked();
    let local = expect_bounds(cx, "app-root-local");
    cx.simulate_click(local.center(), Modifiers::default());
    cx.run_until_parked();

    assert_eq!(
        root.update(cx, |root, _| root.state_label()),
        RootStateLabel::Error,
        "本地库打不开必须进 GUI Error 页"
    );
    assert!(
        !root.update(cx, |root, _| root.has_main_shell()),
        "失败不得带病进入 Main"
    );
    assert_eq!(
        root.update(cx, |root, _| root.error_title().map(str::to_owned)),
        Some(ui_text::local_open_failed_title())
    );
    expect_bounds(cx, "app-root-error");
    expect_bounds(cx, "app-root-error-message");
    assert_eq!(
        std::fs::read(&vault_path).expect("坏库文件必须仍在"),
        broken_before,
        "打不开时不得覆盖/重建本地库"
    );
}
