//! P1 产品体验的本机偏好门禁（headless）：
//!
//! 1. **本地模式自动进入**：点「仅本地使用」成功 → `<数据根>/startup.json` 记
//!    `{"version":1,"mode":"local"}` → 重启 `AppRoot` 直接进本地 Main，库内容不重置；
//! 2. 本机偏好 `local` 但本地库被删 → 回 Welcome + 明确提示，**不凭空建库**、
//!    不进错误页、不 panic；
//! 3. 启动偏好损坏/未知值 → Welcome（不报错页）；
//! 4. **设置分类记忆**：`settings-ui.json` 记上次分类，重建 `AppRoot` 后恢复；
//!    已关闭的插件分类 clamp 到「插件」；未知 key/损坏文件回退「外观」。
//!
//! 产品 `sync-ui` 构建优先：本地路径断言只在**产品构建**（`sync-ui` 且非
//! `dev-sync`）下编译；设置分类记忆两种构建共用同一份 `AppRoot` 逻辑。
//!
//! 纪律（AGENTS.md §6）：临时目录在 `CARGO_TARGET_TMPDIR`；不 sleep；收敛点
//! `run_until_parked()`；不 touch 真实用户目录（显式 `ProfileLayout::new`）。

#![cfg(feature = "sync-ui")]

use std::path::{Path, PathBuf};

use gpui::{Entity, Modifiers, TestAppContext, VisualTestContext};
use menghunote_app::onboarding::app_root::SettingsSection;
use menghunote_app::onboarding::{AppRoot, AppRootOptions, ProfileLayout, RootStateLabel};
#[cfg(not(feature = "dev-sync"))]
use menghunote_app::onboarding::{DevSyncProfile, InstanceEntry, RelayApiVersion, RelayMode};
#[cfg(not(feature = "dev-sync"))]
use menghunote_app::shell::AppShell;
use tempfile::{Builder as TempDirBuilder, TempDir};

/// 在 `CARGO_TARGET_TMPDIR`（`target/tmp`）下开独立临时目录 + 显式公开布局。
fn temp_layout(prefix: &str) -> (TempDir, ProfileLayout) {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("startup-resume-{prefix}-{}", std::process::id()));
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
        instance_id: Some("inst-startup-resume".to_owned()),
        // 产品 `discover` 默认打开本地入口；显式构造时要打开，才能测本地路径。
        allow_local_vault: true,
        operation_budget: std::time::Duration::from_secs(2),
    }
}

/// 开一个 `AppRoot` 根视图（注册关窗分派，测试可以模拟重启）。
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

fn state_of(root: &Entity<AppRoot>, cx: &mut VisualTestContext) -> RootStateLabel {
    root.update(cx, |root, _| root.state_label())
}

/// `AppShell` 侧栏当前展示的标题（先序 DFS；空库 = 空）。
#[cfg(not(feature = "dev-sync"))]
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

fn write_json(path: &Path, value: &serde_json::Value) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("创建 JSON 目录失败");
    }
    std::fs::write(
        path,
        serde_json::to_vec_pretty(value).expect("序列化 JSON 失败"),
    )
    .expect("写 JSON 失败");
}

fn read_json(path: &Path) -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(path).expect("读 JSON 失败")).expect("JSON 必须合法")
}

/// 重建 `AppRoot`（模拟重启）：不经过任何 Welcome 点击。
fn restart(cx: &mut VisualTestContext, options: AppRootOptions) -> Entity<AppRoot> {
    assert!(
        cx.simulate_close(),
        "Main 状态允许关窗（本地模式无同步收尾）"
    );
    cx.run_until_parked();
    cx.update(|window, cx| window.replace_root(cx, move |_window, cx| AppRoot::new(options, cx)))
}

fn open_settings(root: &Entity<AppRoot>, cx: &mut VisualTestContext) {
    root.update(cx, |root, cx| root.open_settings(cx));
    cx.run_until_parked();
    assert!(
        root.update(cx, |root, _| root.settings_is_open()),
        "设置页必须真的打开"
    );
}

fn close_settings(cx: &mut VisualTestContext) {
    let close = expect_bounds(cx, "settings-close");
    cx.simulate_click(close.center(), Modifiers::default());
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("dev-sync-settings").is_none(),
        "关闭后设置覆盖层必须卸载"
    );
}

// ---------------------------------------------------------------------------
// 1) 本地模式自动进入（产品构建）
// ---------------------------------------------------------------------------

/// 本地打开成功 → 重启直接进本地 Main；库不重置，也不写同步配置。
#[cfg(not(feature = "dev-sync"))]
#[gpui::test]
async fn local_open_is_remembered_and_resumes_without_resetting_vault(cx: &mut TestAppContext) {
    let (_dir, layout) = temp_layout("local-resume");
    let vault_path = layout.local_vault_path();
    let startup_path = layout.startup_path();
    let config_path = layout.config_path.clone();
    let instances_dir = layout.instances_dir.clone();
    let options = product_options(layout.clone());
    let (root, cx) = open_root(cx, options.clone());
    cx.run_until_parked();

    assert_eq!(state_of(&root, cx), RootStateLabel::Welcome);
    assert!(
        !startup_path.exists(),
        "进入 Welcome 本身不得写启动偏好（只有打开成功才写）"
    );
    let local = expect_bounds(cx, "app-root-local");
    cx.simulate_click(local.center(), Modifiers::default());
    cx.run_until_parked();

    assert_eq!(state_of(&root, cx), RootStateLabel::Main);
    assert!(root.update(cx, |root, _| root.is_local_mode()));
    assert!(vault_path.is_file(), "本地库必须创建在数据根下");
    let startup = read_json(&startup_path);
    assert_eq!(
        startup["mode"], "local",
        "本地打开成功必须写 mode=local（只记入口，不含正文/同步信息）"
    );
    assert_eq!(startup["version"], serde_json::json!(1));
    assert!(!config_path.exists(), "本地模式不得写 sync.json");
    assert!(!instances_dir.exists(), "本地模式不得创建 instances/");

    // 建一篇笔记：重启后必须还在（库不重置）。
    let shell = root
        .update(cx, |root, _| root.main_shell().cloned())
        .expect("Main 必须有 AppShell");
    let new_note = expect_bounds(cx, "sidebar-new-note");
    cx.simulate_click(new_note.center(), Modifiers::default());
    cx.run_until_parked();
    assert_eq!(sidebar_titles(&shell, cx).len(), 1);

    let restarted = restart(cx, product_options(layout.clone()));
    assert_eq!(
        state_of(&restarted, cx),
        RootStateLabel::Main,
        "偏好 local + 本地库存在：重启必须直接进 Main（不再回 Welcome）"
    );
    assert!(
        restarted.update(cx, |root, _| root.is_local_mode()),
        "恢复后的 Main 必须仍是本地（不同步）模式"
    );
    assert!(
        cx.debug_bounds("app-root-welcome").is_none(),
        "自动恢复不得先闪 Welcome"
    );
    expect_bounds(cx, "app-root-main");
    expect_bounds(cx, "app-root-local-mode");
    let restarted_shell = restarted
        .update(cx, |root, _| root.main_shell().cloned())
        .expect("自动恢复必须构造真实 AppShell");
    assert_eq!(
        sidebar_titles(&restarted_shell, cx).len(),
        1,
        "重启必须复用同一个本地库（笔记仍在、不重置）"
    );
    assert_eq!(
        read_json(&startup_path)["mode"],
        "local",
        "自动恢复后偏好仍是 local"
    );
    assert!(!config_path.exists(), "自动恢复也不得写 sync.json");
}

/// 偏好 local 但本地库被删：回 Welcome + 明确提示；不创建库、不进错误页。
#[cfg(not(feature = "dev-sync"))]
#[gpui::test]
async fn deleted_local_vault_returns_to_welcome_with_explicit_notice(cx: &mut TestAppContext) {
    let (_dir, layout) = temp_layout("local-deleted");
    let vault_path = layout.local_vault_path();
    let options = product_options(layout.clone());
    let (root, cx) = open_root(cx, options.clone());
    cx.run_until_parked();
    let local = expect_bounds(cx, "app-root-local");
    cx.simulate_click(local.center(), Modifiers::default());
    cx.run_until_parked();
    assert_eq!(state_of(&root, cx), RootStateLabel::Main);

    // 模拟用户删除本地库（连目录一起删）：偏好文件仍在数据根下。
    std::fs::remove_dir_all(layout.local_dir()).expect("删除本地库目录失败");
    assert!(!vault_path.exists());

    let restarted = restart(cx, product_options(layout.clone()));
    assert_eq!(
        state_of(&restarted, cx),
        RootStateLabel::Welcome,
        "本地库不存在必须回 Welcome（不凭空创建、不进 Main）"
    );
    assert_eq!(
        restarted.update(cx, |root, _| root.error_title().map(str::to_owned)),
        None,
        "本地库被删不是错误页：必须回 Welcome"
    );
    assert!(!restarted.update(cx, |root, _| root.has_main_shell()));
    expect_bounds(cx, "app-root-welcome");
    assert!(cx.debug_bounds("app-root-error").is_none());
    let notice = restarted
        .update(cx, |root, _| root.notice().map(str::to_owned))
        .expect("必须给出明确提示");
    assert!(
        notice.contains("不存在") && notice.contains(&vault_path.display().to_string()),
        "提示必须说明本地库缺失与公开路径：{notice}"
    );
    assert!(
        !vault_path.exists(),
        "启动引导不得凭空创建本地库（只有用户点入口才创建）"
    );
    assert_eq!(
        read_json(&layout.startup_path())["mode"],
        "local",
        "未成功打开任何入口时偏好保持原值（不静默改成 sync）"
    );
}

/// 本地模式里把某个同步实例「设为默认」→ 本机启动入口切到 `sync`（兑现
/// 「重启后生效」的界面承诺）；重启走同步打开路径，而不是再回到本地库。
#[cfg(not(feature = "dev-sync"))]
#[gpui::test]
async fn setting_active_instance_from_local_mode_switches_startup_preference(
    cx: &mut TestAppContext,
) {
    let (_dir, layout) = temp_layout("local-set-active");
    let options = product_options(layout.clone());
    let (root, cx) = open_root(cx, options.clone());
    cx.run_until_parked();
    let local = expect_bounds(cx, "app-root-local");
    cx.simulate_click(local.center(), Modifiers::default());
    cx.run_until_parked();
    assert!(root.update(cx, |root, _| root.is_local_mode()));
    assert_eq!(read_json(&layout.startup_path())["mode"], "local");

    // 设置页可用的公开实例（**故意指向不存在的库**：本测试只验证带来的启动决策，
    // 不联网、不打开真实同步实例）。
    let identity_vault = layout.vault_path("inst-fake").expect("合法实例 id");
    let store = layout.store();
    let mut profile = DevSyncProfile::default();
    profile.upsert_active(InstanceEntry {
        instance_id: "inst-fake".to_owned(),
        relay_mode: RelayMode::RemoteTls,
        api_version: RelayApiVersion::V1,
        relay_url: "https://relay.example".to_owned(),
        vault_path: identity_vault.to_string_lossy().into_owned(),
        vault_id: "00112233445566778899aabbccddeeff".to_owned(),
        device_id: "ffeeddccbbaa99887766554433221100".to_owned(),
    });
    store.save(&profile).expect("写测试公开配置失败");

    // 真实点击「设为默认」（设置页从磁盘重读配置后可见该实例）。
    open_settings(&root, cx);
    let instances = expect_bounds(cx, "settings-nav-instances");
    cx.simulate_click(instances.center(), Modifiers::default());
    cx.run_until_parked();
    let set_active = expect_bounds(cx, "settings-instance-0-active");
    cx.simulate_click(set_active.center(), Modifiers::default());
    cx.run_until_parked();
    assert_eq!(
        read_json(&layout.startup_path())["mode"],
        "sync",
        "「设为默认」必须让下次启动走同步入口（否则重启仍回本地库）"
    );
    assert!(
        root.update(cx, |root, _| root.is_local_mode()),
        "设为默认不热切换当前 Main（仍是本地库）"
    );

    // 重启：启动偏好已切到 sync → 走同步打开路径（fixture 库不存在 → 既有 Error 页，
    // 而不是本地 Main）：证明启动决策真的不再以本地库优先。
    let restarted = restart(cx, options);
    assert_eq!(
        state_of(&restarted, cx),
        RootStateLabel::Error,
        "偏好 sync 时必须走同步打开路径（fixture 库不存在 → Error）"
    );
    assert!(
        !restarted.update(cx, |root, _| root.is_local_mode()),
        "不得回退到本地 Main"
    );
    assert_eq!(
        restarted.update(cx, |root, _| root.error_title().map(str::to_owned)),
        Some("无法打开已有实例".to_owned())
    );
}

/// 启动偏好损坏/未知值 → Welcome（不报错页、不 panic）。
///
/// 解析矩阵（未知 `mode`、未来版本、空文件等）由 `onboarding::layout` 的单元测试
/// 覆盖；这里只验证 GUI 启动引导的最终落点。
#[cfg(not(feature = "dev-sync"))]
#[gpui::test]
async fn corrupt_startup_preference_falls_back_to_welcome(cx: &mut TestAppContext) {
    let (_dir, layout) = temp_layout("pref-corrupt");
    std::fs::create_dir_all(layout.data_root()).expect("创建数据根失败");
    std::fs::write(layout.startup_path(), b"{ broken").expect("写坏启动偏好失败");
    let before = std::fs::read(layout.startup_path()).expect("读回失败");

    let (root, cx) = open_root(cx, product_options(layout.clone()));
    cx.run_until_parked();
    assert_eq!(
        state_of(&root, cx),
        RootStateLabel::Welcome,
        "损坏偏好必须回 Welcome"
    );
    assert_eq!(
        root.update(cx, |root, _| root.error_title().map(str::to_owned)),
        None,
        "损坏偏好不得进错误页"
    );
    assert!(!root.update(cx, |root, _| root.has_main_shell()));
    expect_bounds(cx, "app-root-welcome");
    assert_eq!(
        std::fs::read(layout.startup_path()).expect("读回失败"),
        before,
        "启动引导不得改写损坏的偏好文件"
    );
}

// ---------------------------------------------------------------------------
// 2) 设置分类记忆（两种 sync-ui 构建共用）
// ---------------------------------------------------------------------------

/// 切换分类 → 重建 `AppRoot` → 打开设置恢复上次分类（真实点击导航）。
#[gpui::test]
async fn settings_section_is_remembered_across_app_root_restarts(cx: &mut TestAppContext) {
    let (_dir, layout) = temp_layout("settings-memory");
    let settings_path = layout.settings_ui_path();

    let (root, cx) = open_root(cx, product_options(layout.clone()));
    cx.run_until_parked();
    open_settings(&root, cx);
    assert_eq!(
        root.update(cx, |root, _| root.settings_section()),
        SettingsSection::Appearance,
        "没有偏好时默认「外观」"
    );
    let language = expect_bounds(cx, "settings-nav-language");
    cx.simulate_click(language.center(), Modifiers::default());
    cx.run_until_parked();
    expect_bounds(cx, "settings-section-language");
    assert_eq!(
        read_json(&settings_path)["section"],
        "language",
        "切换分类必须写本机 settings-ui.json"
    );
    close_settings(cx);

    let restarted = restart(cx, product_options(layout.clone()));
    assert_eq!(
        state_of(&restarted, cx),
        RootStateLabel::Welcome,
        "没有打开过库（只开设置）时启动仍回 Welcome"
    );
    open_settings(&restarted, cx);
    assert_eq!(
        restarted.update(cx, |root, _| root.settings_section()),
        SettingsSection::Language,
        "重建 AppRoot 后打开设置必须恢复到上次分类"
    );
    expect_bounds(cx, "settings-section-language");
    assert!(
        cx.debug_bounds("settings-section-appearance").is_none(),
        "只渲染恢复后的当前分类"
    );

    // 再切一次「关于」并重启：验证不是"恰好记住了语言"。
    let about = expect_bounds(cx, "settings-nav-about");
    cx.simulate_click(about.center(), Modifiers::default());
    cx.run_until_parked();
    expect_bounds(cx, "settings-section-about");
    close_settings(cx);
    let restarted = restart(cx, product_options(layout.clone()));
    open_settings(&restarted, cx);
    assert_eq!(
        restarted.update(cx, |root, _| root.settings_section()),
        SettingsSection::About
    );
    expect_bounds(cx, "settings-section-about");
}

/// 偏好里是已关闭的插件分类 → 打开设置 clamp 到「插件」（内容非空）。
#[gpui::test]
async fn closed_plugin_section_is_clamped_to_plugins(cx: &mut TestAppContext) {
    let templates_guard = menghunote_app::templates::lock_for_test();
    let prefs_dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "startup-resume-plugin-prefs-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&prefs_dir).expect("创建插件偏好目录失败");
    let prefs = TempDirBuilder::new()
        .prefix("prefs-")
        .tempdir_in(&prefs_dir)
        .expect("创建插件临时偏好目录失败");
    templates_guard.reset(Some(
        prefs
            .path()
            .join(menghunote_app::templates::TEMPLATES_FILE_NAME),
    ));
    let mut template_prefs = menghunote_app::templates::preferences();
    template_prefs.enabled = false;
    menghunote_app::templates::set_preferences(template_prefs);
    assert!(!menghunote_app::templates::preferences().enabled);

    let (_dir, layout) = temp_layout("settings-clamp");
    // 偏好文件写一个"上次停在模板分类"（此时模板已关闭）。
    write_json(
        &layout.settings_ui_path(),
        &serde_json::json!({ "version": 1, "section": "templates" }),
    );

    let (root, cx) = open_root(cx, product_options(layout.clone()));
    cx.run_until_parked();
    open_settings(&root, cx);
    assert_eq!(
        root.update(cx, |root, _| root.settings_section()),
        SettingsSection::Plugins,
        "已关闭的插件分类必须 clamp 到「插件」"
    );
    expect_bounds(cx, "settings-section-plugins");
    assert!(
        cx.debug_bounds("settings-nav-templates").is_none(),
        "关闭后左栏不得再显示模板分类"
    );
    assert!(
        cx.debug_bounds("settings-section-templates").is_none(),
        "不得渲染已关闭插件的设置内容"
    );
    // 磁盘值保持用户上次选择（clamp 是恢复时的投影，不静默改写偏好）。
    assert_eq!(
        read_json(&layout.settings_ui_path())["section"],
        "templates"
    );
}

/// 未知分类 key → 回退「外观」（文件里是合法 JSON，但 section 不认识）。
#[gpui::test]
async fn unknown_settings_section_key_falls_back_to_appearance(cx: &mut TestAppContext) {
    let (_dir, layout) = temp_layout("settings-unknown");
    write_json(
        &layout.settings_ui_path(),
        &serde_json::json!({ "version": 1, "section": "no-such-section" }),
    );
    let before = std::fs::read(layout.settings_ui_path()).expect("读回失败");

    let (root, cx) = open_root(cx, product_options(layout.clone()));
    cx.run_until_parked();
    open_settings(&root, cx);
    assert_eq!(
        root.update(cx, |root, _| root.settings_section()),
        SettingsSection::Appearance,
        "未知 key 必须回退「外观」"
    );
    expect_bounds(cx, "settings-section-appearance");
    assert_eq!(
        std::fs::read(layout.settings_ui_path()).expect("读回失败"),
        before,
        "未知 key 不得被静默改写"
    );
}

/// 损坏的设置偏好文件 → 回退「外观」（不 panic、不空白页）。
///
/// 未知 key/未来版本的完整矩阵由 `onboarding::app_root` 的单元测试覆盖；这里验证
/// GUI 打开设置后的最终分类与文件不被改写。
#[gpui::test]
async fn corrupt_settings_preference_falls_back_to_appearance(cx: &mut TestAppContext) {
    let (_dir, layout) = temp_layout("settings-corrupt");
    std::fs::create_dir_all(layout.data_root()).expect("创建数据根失败");
    std::fs::write(layout.settings_ui_path(), b"{ not json").expect("写坏设置偏好失败");
    let before = std::fs::read(layout.settings_ui_path()).expect("读回失败");

    let (root, cx) = open_root(cx, product_options(layout.clone()));
    cx.run_until_parked();
    open_settings(&root, cx);
    assert_eq!(
        root.update(cx, |root, _| root.settings_section()),
        SettingsSection::Appearance,
        "未知 key/损坏必须回退「外观」"
    );
    expect_bounds(cx, "settings-section-appearance");
    assert_eq!(
        std::fs::read(layout.settings_ui_path()).expect("读回失败"),
        before,
        "打开设置不得改写损坏的偏好"
    );
}

/// 切换分类只在设置打开时写盘；关掉设置/同分类重复点击不重复写。
#[gpui::test]
async fn settings_memory_writes_only_on_real_section_change(cx: &mut TestAppContext) {
    let (_dir, layout) = temp_layout("settings-write");
    let settings_path: PathBuf = layout.settings_ui_path();
    let (root, cx) = open_root(cx, product_options(layout));
    cx.run_until_parked();
    // 设置没打开：切换是 no-op，不写盘。
    root.update(cx, |root, cx| {
        root.settings_select_section(SettingsSection::About, cx);
    });
    assert!(!settings_path.exists(), "设置没打开时不得写分类偏好");

    open_settings(&root, cx);
    let about = expect_bounds(cx, "settings-nav-about");
    cx.simulate_click(about.center(), Modifiers::default());
    cx.run_until_parked();
    let written = std::fs::read(&settings_path).expect("切换分类必须写盘");
    assert_eq!(read_json(&settings_path)["section"], "about");
    // 同一分类重复点击不重写（文件字节不变）。
    let again = expect_bounds(cx, "settings-nav-about");
    cx.simulate_click(again.center(), Modifiers::default());
    cx.run_until_parked();
    assert_eq!(
        std::fs::read(&settings_path).expect("读回失败"),
        written,
        "同一分类重复点击不得重复写盘"
    );
    assert!(
        !settings_path.with_extension("json.tmp").exists(),
        "原子写完成后不能留下临时文件"
    );
}
