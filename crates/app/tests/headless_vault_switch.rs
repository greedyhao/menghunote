//! vault 新建/切换/路径选择的 headless 验收（产品级 vault 管理）。
//!
//! 覆盖：
//!
//! 1. **设置 → 实例「打开」**：Main 里点另一个实例 → 旧 `AppShell` 被关闭、旧实例锁
//!    释放、目标实例 Main 就绪（不重启）；active + `startup.json` 记 `sync`；
//!    点当前实例 → no-op（不重建）；
//! 2. **设置 →「返回欢迎页」**：关闭 Main、释放锁、回到 Welcome，实例列表仍在；
//! 3. **设置 →「新建 / 加入同步 vault…」**：关 Main + 直接进 Create/Join 表单；
//! 4. **Create/Join 默认实例目录**在用户文档目录（不是 LOCALAPPDATA），且可改成
//!    任意绝对路径；
//! 5. **本地（不同步）库**：默认路径不在 LOCALAPPDATA；可打开自定义路径（不存在则
//!    新建空库）并写入 `startup.json`，重启回到同一路径；
//! 6. **旧 `startup.json`**（只有 `{version,mode}`）：仍能打开旧数据根下的本地库。
//!
//! 纪律（AGENTS.md §6）：临时目录在 `CARGO_TARGET_TMPDIR`；不 sleep；收敛点
//! `run_until_parked()`（以及生产自己的关窗排空）；显式 `ProfileLayout` 不碰真实用户
//! 目录，`discover` 用例只注入 env 闭包。

#![cfg(feature = "sync-ui")]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui::{px, Entity, Modifiers, TestAppContext, VisualTestContext};
use menghunote_app::dev_sync::DevIdentityStore;
use menghunote_app::onboarding::app_root::SettingsSection;
use menghunote_app::onboarding::layout::{load_startup_preferences, StartupMode, VAULT_FILE_NAME};
use menghunote_app::onboarding::{
    AppRoot, AppRootOptions, DevSyncProfile, InstanceEntry, ProfileLayout, RelayApiVersion,
    RelayMode, RootStateLabel, LOCK_FILE_NAME,
};
use menghunote_app::shell::{AppShell, RIBBON_WIDTH};
use menghunote_storage::{StructureWriter, Vault};
use tempfile::{Builder as TempDirBuilder, TempDir};

/// 底部动作行内两按钮之间的间隙（**设计值**：与 `app_root::BOTTOM_ACTIONS_GAP` 对齐；
/// 这里独立写死，避免测试跟着实现一起漂移）。
const BOTTOM_ACTIONS_GAP: f32 = 6.0;

/// 测试进程级的 `XDG_DATA_HOME`（Linux 用户主键；Windows 是 no-op）。
fn ensure_test_user_key() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("vault-switch-xdg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("创建测试 XDG_DATA_HOME 失败");
        std::env::set_var("XDG_DATA_HOME", &dir);
        menghunote_sync::identity_wrap::initialize_user_key()
            .expect("初始化测试用户主键失败（ADR 0008）");
    });
}

/// 在 `CARGO_TARGET_TMPDIR`（`target/tmp`）下开独立临时目录 + 显式公开布局。
fn temp_layout(prefix: &str) -> (TempDir, ProfileLayout) {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("vault-switch-{prefix}-{}", std::process::id()));
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

/// 测试用 `AppRootOptions`（产品构建默认 `remote_tls`）。
fn product_options(layout: ProfileLayout) -> AppRootOptions {
    AppRootOptions {
        layout,
        default_relay: "https://relay.example".to_owned(),
        instance_id: Some("inst-vault-switch".to_owned()),
        allow_local_vault: true,
        operation_budget: Duration::from_secs(2),
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

fn click(cx: &mut VisualTestContext, selector: &'static str) {
    let bounds = expect_bounds(cx, selector);
    cx.simulate_click(bounds.center(), Modifiers::default());
    cx.run_until_parked();
}

fn read_json(path: &Path) -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(path).expect("读 JSON 失败")).expect("JSON 必须合法")
}

fn open_settings(root: &Entity<AppRoot>, cx: &mut VisualTestContext) {
    root.update(cx, |root, cx| root.open_settings(cx));
    cx.run_until_parked();
    assert!(
        root.update(cx, |root, _| root.settings_is_open()),
        "设置页必须真的打开"
    );
}

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

/// 造一个真实可打开的同步实例（空库 + 真实持久身份 + 一篇笔记）并返回公开配置条目。
fn make_instance(layout: &ProfileLayout, instance_id: &str, title: &str) -> InstanceEntry {
    let dir = layout.instance_dir(instance_id).expect("合法实例 id");
    std::fs::create_dir_all(&dir).expect("创建实例目录失败");
    let vault_path = dir.join(VAULT_FILE_NAME);
    let identity = {
        let vault = Vault::open(&vault_path).expect("打开测试库失败");
        let identity = DevIdentityStore::load_or_create(&vault_path).expect("创建测试身份失败");
        let mut writer =
            StructureWriter::open(&vault, identity.loro_peer_id()).expect("打开结构写入口失败");
        writer
            .doc()
            .create_note(title, 1)
            .expect("创建测试笔记失败");
        writer
            .commit("vault-switch-seed")
            .expect("提交测试笔记失败");
        drop(writer);
        identity
    };
    InstanceEntry {
        instance_id: instance_id.to_owned(),
        display_name: None,
        relay_mode: RelayMode::RemoteTls,
        api_version: RelayApiVersion::V2,
        relay_url: "https://relay.example".to_owned(),
        vault_path: vault_path.to_string_lossy().into_owned(),
        vault_id: menghunote_sync::ids::hex_of(&identity.vault_id),
        device_id: menghunote_sync::ids::hex_of(&identity.device_id),
    }
}

fn save_two_instances(
    layout: &ProfileLayout,
    first: &InstanceEntry,
    second: &InstanceEntry,
) -> PathBuf {
    let mut profile = DevSyncProfile::default();
    profile.upsert_active(first.clone());
    profile.instances.push(second.clone());
    layout.store().save(&profile).expect("写测试公开配置失败");
    layout.config_path.clone()
}

fn lock_path(layout: &ProfileLayout, instance_id: &str) -> PathBuf {
    layout
        .instance_dir(instance_id)
        .expect("合法实例 id")
        .join(LOCK_FILE_NAME)
}

// ---------------------------------------------------------------------------
// 1) 左下角 vault 切换菜单（Obsidian 式）：当前 ✓、列出其它实例、Esc/点空白关闭
// ---------------------------------------------------------------------------

/// 左下角按钮 = 当前库**短名字**（目录名，不是 `vault.sqlite3`/完整路径）+ `▾`；
/// 点击在按钮**上方**展开菜单：当前实例带 ✓、其它实例逐行、底部「管理仓库…」；
/// Esc 与点空白都只关菜单，不切换实例。
#[gpui::test]
async fn vault_switcher_opens_menu_with_current_check_and_other_instances(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let (_dir, layout) = temp_layout("menu-open");
    let first = make_instance(&layout, "inst-a", "甲库笔记");
    let second = make_instance(&layout, "inst-b", "乙库笔记");
    save_two_instances(&layout, &first, &second);

    let (root, cx) = open_root(cx, product_options(layout.clone()));
    cx.run_until_parked();
    assert_eq!(state_of(&root, cx), RootStateLabel::Main);

    // 按钮文案 = 实例目录名（不是库文件名/完整路径）；两个入口在**同一行**。
    let switcher = expect_bounds(cx, "app-root-vault-switcher");
    assert_eq!(
        root.update(cx, |root, cx| root.current_vault_name(cx)),
        "inst-a",
        "左下角必须显示 vault 的短名字（目录名）"
    );
    let settings = expect_bounds(cx, "app-root-settings");
    let row = expect_bounds(cx, "app-root-bottom-actions");
    // 动作行：贴近侧栏左边界（跳过 ribbon），两个按钮都在行内。
    assert!(
        (row.origin.x.as_f32() - (8.0 + RIBBON_WIDTH)).abs() <= 0.5,
        "底部动作行必须左对齐 8+RIBBON_WIDTH：row={row:?}"
    );
    assert!(
        (switcher.origin.x.as_f32() - row.origin.x.as_f32()).abs() <= 0.5,
        "切换器必须与动作行左对齐：switcher={switcher:?} row={row:?}"
    );
    // 同一行：origin.y 相同、行高一致（设置按钮不能把行撑高）。
    assert!(
        (switcher.origin.y.as_f32() - settings.origin.y.as_f32()).abs() <= 0.5,
        "切换器与设置按钮必须同一行（origin.y 相同）：switcher={switcher:?} settings={settings:?}"
    );
    assert!(
        (switcher.size.height.as_f32() - settings.size.height.as_f32()).abs() <= 0.5,
        "切换器与设置按钮必须同一行高：switcher={switcher:?} settings={settings:?}"
    );
    assert!(
        (switcher.origin.y.as_f32() + switcher.size.height.as_f32()
            - row.origin.y.as_f32()
            - row.size.height.as_f32())
        .abs()
            <= 0.5,
        "动作行高度必须由按钮撑满（同一底边）：switcher={switcher:?} row={row:?}"
    );
    // 设置按钮缩短：不撑满剩余宽度，且明显窄于切换器。
    assert!(
        settings.size.width.as_f32() <= 70.0,
        "设置按钮必须缩短（<=70px）：{settings:?}"
    );
    assert!(
        settings.size.width.as_f32() < switcher.size.width.as_f32(),
        "设置按钮必须窄于切换器（不撑满剩余宽度）：settings={settings:?} switcher={switcher:?}"
    );
    // 切换器占满剩余宽度：右边界 + 动作行间隙 = 设置按钮左边界。
    assert!(
        (switcher.origin.x.as_f32()
            + switcher.size.width.as_f32()
            + BOTTOM_ACTIONS_GAP
            - settings.origin.x.as_f32())
        .abs()
            <= 0.5,
        "切换器右边界 + {BOTTOM_ACTIONS_GAP} 必须等于设置按钮左边界：switcher={switcher:?} settings={settings:?}"
    );
    assert!(
        (row.origin.x.as_f32() + row.size.width.as_f32()
            - settings.origin.x.as_f32()
            - settings.size.width.as_f32())
        .abs()
            <= 0.5,
        "设置按钮右端必须贴住动作行右边界：settings={settings:?} row={row:?}"
    );
    assert!(
        !root.update(cx, |root, _| root.vault_menu_is_open()),
        "初始不得展开菜单"
    );
    assert!(cx.debug_bounds("app-root-vault-menu").is_none());

    // 点击按钮 → 菜单在按钮上方向上展开，不超出窗口。
    click(cx, "app-root-vault-switcher");
    assert!(
        root.update(cx, |root, _| root.vault_menu_is_open()),
        "点击左下角按钮必须打开切换菜单"
    );
    let menu = expect_bounds(cx, "app-root-vault-menu");
    expect_bounds(cx, "app-root-vault-menu-current");
    expect_bounds(cx, "app-root-vault-menu-instance-1");
    expect_bounds(cx, "app-root-vault-menu-divider");
    expect_bounds(cx, "app-root-vault-manage");
    // 行模型与渲染共用：当前行带 ✓，其它行是短名字 + 简短 API/mode。
    let rows = root.update(cx, |root, cx| root.vault_menu_rows(cx));
    assert_eq!(rows.len(), 2, "菜单必须列出两个已保存实例");
    assert!(rows[0].current, "第一行必须是当前 vault（active 优先）");
    assert!(
        rows[0].label.starts_with('✓'),
        "当前 vault 行必须带 ✓：{}",
        rows[0].label
    );
    assert!(
        rows[0].label.contains("inst-a"),
        "当前行必须显示目录名：{}",
        rows[0].label
    );
    assert_eq!(rows[0].meta, "当前 vault");
    assert!(!rows[1].current);
    assert_eq!(rows[1].label, "inst-b", "其它行显示目标 vault 的短名字");
    assert!(
        !rows[1].label.contains('✓'),
        "只有当前 vault 行带 ✓：{}",
        rows[1].label
    );
    assert!(
        rows[1].meta.contains("v2") && rows[1].meta.contains("remote_tls"),
        "其它行必须有简短 API/mode：{}",
        rows[1].meta
    );
    assert!(
        menu.origin.y.as_f32() + menu.size.height.as_f32() <= row.origin.y.as_f32() + 0.5,
        "菜单必须在动作行**上方**展开（不能盖住按钮）：menu={menu:?} row={row:?}"
    );
    // 菜单底边贴着动作行顶部（设计值 4px 间隙），不是悬在旧的两行布局高度上。
    let menu_gap = row.origin.y.as_f32() - (menu.origin.y.as_f32() + menu.size.height.as_f32());
    assert!(
        (0.0..=8.0).contains(&menu_gap),
        "菜单底边与动作行顶部的间隙必须很小（不悬空、不覆盖）：gap={menu_gap} menu={menu:?} row={row:?}"
    );
    let window = expect_bounds(cx, "app-root");
    assert!(
        menu.origin.x.as_f32() >= window.origin.x.as_f32()
            && menu.origin.x.as_f32() + menu.size.width.as_f32()
                <= window.origin.x.as_f32() + window.size.width.as_f32(),
        "菜单不得超出窗口：menu={menu:?} window={window:?}"
    );
    // 菜单打开后设置页**不能**被打开（这是下拉菜单，不是设置页）。
    assert!(
        !root.update(cx, |root, _| root.settings_is_open()),
        "切换菜单不得打开设置页"
    );

    // Esc → 关闭菜单，但 Main/实例不变。
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(
        !root.update(cx, |root, _| root.vault_menu_is_open()),
        "Esc 必须关闭切换菜单"
    );
    assert!(cx.debug_bounds("app-root-vault-menu").is_none());
    assert_eq!(state_of(&root, cx), RootStateLabel::Main);

    // 点空白（菜单外）→ 关闭菜单，不切换实例、不把点击漏给编辑器。
    click(cx, "app-root-vault-switcher");
    expect_bounds(cx, "app-root-vault-menu");
    let outside = gpui::point(window.origin.x + px(600.0), window.origin.y + px(60.0));
    cx.simulate_click(outside, Modifiers::default());
    cx.run_until_parked();
    assert!(
        !root.update(cx, |root, _| root.vault_menu_is_open()),
        "点空白必须关闭切换菜单"
    );
    assert!(cx.debug_bounds("app-root-vault-menu").is_none());
    assert_eq!(
        root.update(cx, |root, _| root
            .opened_instance()
            .map(|entry| entry.instance_id.clone())),
        Some("inst-a".to_owned()),
        "关闭菜单不得改变当前实例"
    );

    // 再点按钮本身 = 收起（不是重新打开）。
    click(cx, "app-root-vault-switcher");
    expect_bounds(cx, "app-root-vault-menu");
    click(cx, "app-root-vault-switcher");
    assert!(
        !root.update(cx, |root, _| root.vault_menu_is_open()),
        "再次点击按钮必须收起菜单"
    );

    // 同一行右侧的「设置」按钮仍然独立可用：真实点击 → 打开设置（不是切换菜单）。
    click(cx, "app-root-settings");
    assert!(
        root.update(cx, |root, _| root.settings_is_open()),
        "真实点击紧凑「设置」按钮必须打开设置页"
    );
    assert!(
        !root.update(cx, |root, _| root.vault_menu_is_open()),
        "设置按钮不得打开切换菜单"
    );
    expect_bounds(cx, "dev-sync-settings");
    // 关闭设置后两个入口都回到 Main（不切换实例）。
    let close = expect_bounds(cx, "settings-close");
    cx.simulate_click(close.center(), Modifiers::default());
    cx.run_until_parked();
    assert!(!root.update(cx, |root, _| root.settings_is_open()));
    assert_eq!(state_of(&root, cx), RootStateLabel::Main);
    // 菜单仍然能从切换器打开（两个 selector 互不替代）。
    click(cx, "app-root-vault-switcher");
    expect_bounds(cx, "app-root-vault-menu");
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(cx.debug_bounds("app-root-vault-menu").is_none());
}

// ---------------------------------------------------------------------------
// 1b) 底部动作行：长 vault 名不得把「设置」挤出侧栏 / 把行撑成两行
// ---------------------------------------------------------------------------

/// 切换器是 `flex_1 + min_w(0)`（长名字只在自身内部省略），设置是定宽 `flex_shrink_0`：
/// 超长目录名既不能改变两侧宽度，也不能让动作行变高/换行；两个 selector 仍各自可点。
#[gpui::test]
async fn bottom_actions_stay_one_row_with_long_vault_name(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let (_dir, layout) = temp_layout("bottom-actions");
    let long_id = "inst-very-long-vault-directory-name-0123456789";
    let entry = make_instance(&layout, long_id, "长名字库");
    let mut profile = DevSyncProfile::default();
    profile.upsert_active(entry);
    layout.store().save(&profile).expect("写测试公开配置失败");

    let (root, cx) = open_root(cx, product_options(layout.clone()));
    cx.run_until_parked();
    assert_eq!(state_of(&root, cx), RootStateLabel::Main);
    assert_eq!(
        root.update(cx, |root, cx| root.current_vault_name(cx)),
        long_id,
        "切换器必须拿到完整目录名（省略只发生在布局里，不改数据）"
    );

    let row = expect_bounds(cx, "app-root-bottom-actions");
    let switcher = expect_bounds(cx, "app-root-vault-switcher");
    let settings = expect_bounds(cx, "app-root-settings");
    assert!(
        (switcher.origin.y.as_f32() - settings.origin.y.as_f32()).abs() <= 0.5,
        "长名字不得把设置按钮挤到第二行：switcher={switcher:?} settings={settings:?}"
    );
    assert!(
        (switcher.size.height.as_f32() - settings.size.height.as_f32()).abs() <= 0.5,
        "长名字不得改变两个按钮的行高：switcher={switcher:?} settings={settings:?}"
    );
    assert!(
        row.size.height.as_f32() <= 32.5,
        "动作行必须保持单行高（不能因长名字换行变高）：{row:?}"
    );
    // 长名字只影响切换器**内部**省略：两侧宽度仍由行宽与定宽设置决定。
    assert!(
        (settings.size.width.as_f32() - 64.0).abs() <= 0.5,
        "设置按钮必须保持紧凑宽度 64px：{settings:?}"
    );
    assert!(
        (switcher.size.width.as_f32() + BOTTOM_ACTIONS_GAP + settings.size.width.as_f32()
            - row.size.width.as_f32())
        .abs()
            <= 0.5,
        "长名字不得改变切换器占满剩余宽度：switcher={switcher:?} settings={settings:?} row={row:?}"
    );
    let window = expect_bounds(cx, "app-root");
    assert!(
        row.origin.x.as_f32() + row.size.width.as_f32()
            <= window.origin.x.as_f32() + window.size.width.as_f32() + 0.5,
        "动作行不得把「设置」挤出窗口：row={row:?} window={window:?}"
    );

    // 长名字不影响命中：设置与切换器仍分别打开各自覆盖层。
    click(cx, "app-root-settings");
    assert!(
        root.update(cx, |root, _| root.settings_is_open()),
        "长名字场景下真实点击「设置」仍必须打开设置页"
    );
    let close = expect_bounds(cx, "settings-close");
    cx.simulate_click(close.center(), Modifiers::default());
    cx.run_until_parked();
    assert!(!root.update(cx, |root, _| root.settings_is_open()));
    click(cx, "app-root-vault-switcher");
    expect_bounds(cx, "app-root-vault-menu");
}

// ---------------------------------------------------------------------------
// 2) 菜单里点第二个实例：关旧开新（不重启、不经过设置）
// ---------------------------------------------------------------------------
#[gpui::test]
async fn vault_menu_switches_to_second_instance_without_restart(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let (_dir, layout) = temp_layout("menu-switch");
    let first = make_instance(&layout, "inst-a", "甲库笔记");
    let second = make_instance(&layout, "inst-b", "乙库笔记");
    save_two_instances(&layout, &first, &second);
    let config_path = layout.config_path.clone();
    let startup_path = layout.startup_path();
    let a_lock = lock_path(&layout, "inst-a");
    let b_lock = lock_path(&layout, "inst-b");

    let (root, cx) = open_root(cx, product_options(layout.clone()));
    cx.run_until_parked();
    assert_eq!(state_of(&root, cx), RootStateLabel::Main);
    assert!(a_lock.is_file(), "A 打开后必须持有实例锁");
    let old_shell = root
        .update(cx, |root, _| root.main_shell().cloned())
        .expect("Main 必须有 AppShell");
    assert_eq!(sidebar_titles(&old_shell, cx), vec!["甲库笔记".to_owned()]);

    // 左下角菜单 → 第二个实例行；不打开设置页。
    click(cx, "app-root-vault-switcher");
    let open_second = expect_bounds(cx, "app-root-vault-menu-instance-1");
    cx.simulate_click(open_second.center(), Modifiers::default());
    cx.run_until_parked();

    assert_eq!(state_of(&root, cx), RootStateLabel::Main, "切换不需要重启");
    assert!(
        !root.update(cx, |root, _| root.settings_is_open()),
        "菜单直接切换，不得打开设置页"
    );
    assert!(
        !root.update(cx, |root, _| root.vault_menu_is_open()),
        "切换后菜单必须关闭"
    );
    assert_eq!(
        root.update(cx, |root, _| root
            .opened_instance()
            .map(|entry| entry.instance_id.clone())),
        Some("inst-b".to_owned()),
        "必须真的打开目标实例"
    );
    let new_shell = root
        .update(cx, |root, _| root.main_shell().cloned())
        .expect("切换后 Main 必须有新的 AppShell");
    assert_ne!(
        old_shell.entity_id(),
        new_shell.entity_id(),
        "不能复用旧 AppShell（切换 = 关旧开新）"
    );
    assert_eq!(
        sidebar_titles(&new_shell, cx),
        vec!["乙库笔记".to_owned()],
        "Main 必须显示目标库的内容"
    );
    assert!(!a_lock.exists(), "旧实例锁必须在切换时释放");
    assert!(b_lock.is_file(), "目标实例必须持有自己的锁");
    drop(old_shell);

    // 公开配置 active + 本机启动偏好都切到 B。
    let profile = layout
        .store()
        .load()
        .expect("读配置失败")
        .expect("配置必须存在");
    assert_eq!(
        profile.active_instance.as_deref(),
        Some("inst-b"),
        "打开目标实例后必须把它记为 active"
    );
    let preferences = load_startup_preferences(&startup_path).expect("启动偏好必须存在");
    assert_eq!(preferences.mode, StartupMode::Sync, "下次启动应回同步实例");
    assert!(config_path.is_file());
    // 旧配置的 vault_path 原样保留（不迁移、不改写）。
    let kept = profile
        .find("inst-a")
        .expect("A 仍在配置里")
        .vault_path
        .clone();
    assert_eq!(
        kept, first.vault_path,
        "切换实例不得改写其它实例的 vault_path"
    );
    // 切换后按钮显示目标库短名。
    assert_eq!(
        root.update(cx, |root, cx| root.current_vault_name(cx)),
        "inst-b"
    );
}

/// 切换失败：进既有 Error 页（可重试/返回欢迎页），旧 Main/锁都必须已经收尾。
#[gpui::test]
async fn vault_menu_switch_failure_lands_on_error_and_releases_old_lock(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let (_dir, layout) = temp_layout("menu-failure");
    let first = make_instance(&layout, "inst-a", "甲库笔记");
    let second = make_instance(&layout, "inst-b", "乙库笔记");
    save_two_instances(&layout, &first, &second);
    let a_lock = lock_path(&layout, "inst-a");
    let b_lock = lock_path(&layout, "inst-b");

    let (root, cx) = open_root(cx, product_options(layout.clone()));
    cx.run_until_parked();
    assert_eq!(state_of(&root, cx), RootStateLabel::Main);
    assert!(a_lock.is_file());

    // 目标库文件被删（配置/身份还在）：打开必然失败。
    std::fs::remove_file(&second.vault_path).expect("删除 B 库文件失败");
    click(cx, "app-root-vault-switcher");
    let open_second = expect_bounds(cx, "app-root-vault-menu-instance-1");
    cx.simulate_click(open_second.center(), Modifiers::default());
    cx.run_until_parked();

    assert_eq!(
        state_of(&root, cx),
        RootStateLabel::Error,
        "切换失败必须进既有 Error 页"
    );
    assert!(
        !root.update(cx, |root, _| root.has_main_shell()),
        "失败后不得保留旧 Main（更不能同时留着两个）"
    );
    assert!(root.update(cx, |root, _| root.opened_instance().is_none()));
    assert!(!a_lock.exists(), "切换前必须已释放旧实例锁");
    assert!(!b_lock.exists(), "目标没打开成功时不应留下锁");
    expect_bounds(cx, "app-root-error");
    expect_bounds(cx, "app-root-error-message");
    expect_bounds(cx, "app-root-retry");
    expect_bounds(cx, "app-root-back-welcome");
}

/// 点当前实例 = no-op：不重建、不重新加载、锁保持，只把菜单收起来。
#[gpui::test]
async fn vault_menu_current_instance_is_a_noop(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let (_dir, layout) = temp_layout("menu-current");
    let first = make_instance(&layout, "inst-a", "甲库笔记");
    let second = make_instance(&layout, "inst-b", "乙库笔记");
    save_two_instances(&layout, &first, &second);
    let a_lock = lock_path(&layout, "inst-a");

    let (root, cx) = open_root(cx, product_options(layout.clone()));
    cx.run_until_parked();
    let shell = root
        .update(cx, |root, _| root.main_shell().cloned())
        .expect("Main 必须有 AppShell");

    click(cx, "app-root-vault-switcher");
    let current = expect_bounds(cx, "app-root-vault-menu-current");
    cx.simulate_click(current.center(), Modifiers::default());
    cx.run_until_parked();

    assert_eq!(state_of(&root, cx), RootStateLabel::Main);
    assert!(
        !root.update(cx, |root, _| root.vault_menu_is_open()),
        "点当前实例后菜单必须关闭"
    );
    assert!(
        !root.update(cx, |root, _| root.settings_is_open()),
        "点当前实例不得打开设置页"
    );
    let same = root
        .update(cx, |root, _| root.main_shell().cloned())
        .expect("Main 仍应存在");
    assert_eq!(
        shell.entity_id(),
        same.entity_id(),
        "当前实例不得重建 AppShell"
    );
    assert!(a_lock.is_file(), "no-op 不得释放/重建实例锁");
    assert_eq!(
        root.update(cx, |root, _| root
            .opened_instance()
            .map(|entry| entry.instance_id.clone())),
        Some("inst-a".to_owned())
    );
}

// ---------------------------------------------------------------------------
// 3) 管理仓库覆盖层：唯一创建/管理入口
// ---------------------------------------------------------------------------

#[gpui::test]
async fn vault_manager_opens_and_sync_remote_enters_unified_form(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let (_dir, layout) = temp_layout("manager-new");
    let first = make_instance(&layout, "inst-a", "甲库笔记");
    let second = make_instance(&layout, "inst-b", "乙库笔记");
    save_two_instances(&layout, &first, &second);
    let a_lock = lock_path(&layout, "inst-a");

    let (root, cx) = open_root(cx, product_options(layout.clone()));
    cx.run_until_parked();
    assert_eq!(state_of(&root, cx), RootStateLabel::Main);

    // 左下角菜单 →「管理仓库…」→ 独立覆盖层（标题「管理仓库」+ 关闭按钮）。
    click(cx, "app-root-vault-switcher");
    click(cx, "app-root-vault-manage");
    assert!(
        root.update(cx, |root, _| root.vault_manager_is_open()),
        "「管理仓库…」必须打开管理仓库覆盖层"
    );
    expect_bounds(cx, "app-root-vault-manager");
    expect_bounds(cx, "app-root-vault-manager-backdrop");
    expect_bounds(cx, "vault-manager-close");
    expect_bounds(cx, "vault-manager-instance-0");
    expect_bounds(cx, "vault-manager-open-0");
    expect_bounds(cx, "vault-manager-default-0");
    // 同步远程仓库只有**一个**入口；旧的「新建同步 / 加入同步」两个按钮已合并。
    expect_bounds(cx, "vault-manager-sync-remote");
    assert!(
        cx.debug_bounds("vault-manager-new-sync").is_none()
            && cx.debug_bounds("vault-manager-join-sync").is_none(),
        "管理仓库不得再分成「新建同步」「加入同步」两个入口"
    );
    expect_bounds(cx, "vault-manager-back-welcome");
    assert!(
        !root.update(cx, |root, _| root.settings_is_open()),
        "管理仓库不是设置页"
    );
    assert!(
        cx.debug_bounds("settings-instances").is_none()
            && cx.debug_bounds("settings-nav-instances").is_none(),
        "设置里不得再有「实例」分类"
    );

    // Esc → 关闭管理仓库（焦点在覆盖层上），Main 与实例锁保持不变。
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(
        !root.update(cx, |root, _| root.vault_manager_is_open()),
        "Esc 必须关闭管理仓库"
    );
    assert!(cx.debug_bounds("app-root-vault-manager").is_none());
    assert_eq!(state_of(&root, cx), RootStateLabel::Main);
    assert!(a_lock.is_file());

    // 再打开 → 唯一入口「同步远程仓库…」：关 Main + 释放锁 → Welcome 展开统一表单。
    click(cx, "app-root-vault-switcher");
    click(cx, "app-root-vault-manage");
    click(cx, "vault-manager-sync-remote");
    assert_eq!(
        state_of(&root, cx),
        RootStateLabel::Welcome,
        "同步远程仓库必须回到 Welcome 的统一表单（不再进 CreateForm/JoinForm）"
    );
    expect_bounds(cx, "app-root-welcome");
    expect_bounds(cx, "welcome-sync-link");
    expect_bounds(cx, "welcome-sync-dir");
    expect_bounds(cx, "welcome-sync-confirm");
    assert!(
        cx.debug_bounds("app-root-create-form").is_none()
            && cx.debug_bounds("app-root-join-form").is_none(),
        "统一表单不得复用旧 Create/Join 页面"
    );
    assert!(!root.update(cx, |root, _| root.has_main_shell()));
    assert!(!root.update(cx, |root, _| root.opened_instance().is_some()));
    assert!(!root.update(cx, |root, _| root.vault_manager_is_open()));
    assert!(!a_lock.exists(), "进入同步远程仓库前必须释放旧实例锁");
    assert_eq!(
        root.update(cx, |root, _| root.entries().len()),
        2,
        "公开配置里的实例列表仍在"
    );
}

#[gpui::test]
async fn vault_manager_back_welcome_shows_obsidian_vault_list(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let (_dir, layout) = temp_layout("manager-back");
    let first = make_instance(&layout, "inst-a", "甲库笔记");
    let second = make_instance(&layout, "inst-b", "乙库笔记");
    save_two_instances(&layout, &first, &second);
    let a_lock = lock_path(&layout, "inst-a");

    let (root, cx) = open_root(cx, product_options(layout.clone()));
    cx.run_until_parked();
    assert_eq!(state_of(&root, cx), RootStateLabel::Main);

    click(cx, "app-root-vault-switcher");
    click(cx, "app-root-vault-manage");
    click(cx, "vault-manager-back-welcome");

    assert_eq!(
        state_of(&root, cx),
        RootStateLabel::Welcome,
        "「返回欢迎页」必须回到 Welcome"
    );
    assert!(
        !root.update(cx, |root, _| root.has_main_shell()),
        "返回 Welcome 后不得保留 AppShell"
    );
    assert!(
        !root.update(cx, |root, _| root.is_local_mode()),
        "本地模式提示/标记必须清掉"
    );
    assert!(!root.update(cx, |root, _| root.vault_manager_is_open()));
    assert!(!a_lock.exists(), "返回 Welcome 必须释放实例锁");
    expect_bounds(cx, "app-root-welcome");
    expect_bounds(cx, "welcome-vault-list");
    expect_bounds(cx, "welcome-vault-0");
    expect_bounds(cx, "welcome-vault-1");
    assert!(
        cx.debug_bounds("app-root-instance-0").is_none(),
        "旧实例行 selector 不得再渲染"
    );
    assert!(
        cx.debug_bounds("dev-sync-settings").is_none(),
        "设置覆盖层必须关闭"
    );
    assert_eq!(
        root.update(cx, |root, _| root.entries().len()),
        2,
        "同步实例列表必须仍在（可重新打开）"
    );
}

// ---------------------------------------------------------------------------
// 4) 设置页不再有「实例」分类；旧 settings-ui.json 的 instances 回退不崩
// ---------------------------------------------------------------------------

#[gpui::test]
async fn settings_nav_has_no_instances_section_and_legacy_preference_falls_back(
    cx: &mut TestAppContext,
) {
    let (_dir, layout) = temp_layout("settings-no-instances");
    std::fs::create_dir_all(layout.data_root()).expect("创建数据根失败");
    // 旧版本记住的是「实例」分类：现在必须回退到一个可用分类（外观），不崩/不空白。
    std::fs::write(
        layout.settings_ui_path(),
        b"{\n  \"version\": 1,\n  \"section\": \"instances\"\n}\n",
    )
    .expect("写旧 settings-ui.json 失败");
    let before = std::fs::read(layout.settings_ui_path()).expect("读回失败");

    let (root, cx) = open_root(cx, product_options(layout.clone()));
    cx.run_until_parked();
    assert_eq!(state_of(&root, cx), RootStateLabel::Welcome);
    open_settings(&root, cx);

    assert_eq!(
        root.update(cx, |root, _| root.settings_section()),
        SettingsSection::Appearance,
        "旧记忆 instances 必须回退「外观」（不崩、不空白）"
    );
    expect_bounds(cx, "settings-section-appearance");
    assert!(cx.debug_bounds("settings-nav-instances").is_none());
    assert!(cx.debug_bounds("settings-section-instances").is_none());
    for nav in [
        "settings-nav-appearance",
        "settings-nav-language",
        "settings-nav-shortcuts",
        "settings-nav-sync",
        "settings-nav-relay",
        "settings-nav-security",
        "settings-nav-plugins",
        "settings-nav-about",
    ] {
        let nav: &'static str = Box::leak(nav.to_owned().into_boxed_str());
        expect_bounds(cx, nav);
    }
    // 磁盘值保持用户上次选择（回退是投影，不静默改写偏好）。
    assert_eq!(
        std::fs::read(layout.settings_ui_path()).expect("读回失败"),
        before
    );
    // 枚举层面也不再暴露「实例」分类。
    assert!(
        SettingsSection::ALL
            .iter()
            .all(|section| section.key() != "instances"),
        "SettingsSection 不得再包含「实例」"
    );
    assert_eq!(SettingsSection::parse("instances"), None);
}
// ---------------------------------------------------------------------------
// 3) Create/Join 默认目录 = 用户文档目录（不是 LOCALAPPDATA）
// ---------------------------------------------------------------------------

/// 按平台默认布局解析（注入临时 HOME/USERPROFILE + XDG/LOCALAPPDATA，不碰真实目录）。
fn discovered_layout(root: &Path) -> ProfileLayout {
    let mut env: HashMap<String, String> = HashMap::new();
    #[cfg(not(windows))]
    {
        let home = root.join("home");
        let config = root.join("config");
        let data = root.join("data");
        for dir in [&home, &home.join("Documents"), &config, &data] {
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
    }
    #[cfg(windows)]
    {
        let home = root.join("home");
        let local = root.join("localappdata");
        for dir in [&home, &home.join("Documents"), &local] {
            std::fs::create_dir_all(dir).expect("创建临时平台目录失败");
        }
        env.insert(
            "USERPROFILE".to_owned(),
            home.to_string_lossy().into_owned(),
        );
        env.insert(
            "LOCALAPPDATA".to_owned(),
            local.to_string_lossy().into_owned(),
        );
    }
    ProfileLayout::discover(move |key| env.get(key).cloned()).expect("临时默认布局解析失败")
}

#[gpui::test]
async fn create_and_join_default_to_user_directory_and_accept_any_absolute_path(
    cx: &mut TestAppContext,
) {
    let (_dir, _) = temp_layout("default-dir-marker");
    let layout = discovered_layout(_dir.path());
    let expected_dir = layout
        .default_instance_dir("inst-vault-switch")
        .expect("合法实例 id");
    let expected_text = expected_dir.display().to_string();
    let lower = expected_text.to_lowercase();
    assert!(
        !lower.contains("localappdata") && !lower.contains("appdata"),
        "默认实例目录不得落在 LOCALAPPDATA：{expected_text}"
    );
    assert!(
        expected_bounds_for_prefix(&expected_text),
        "默认实例目录必须在用户文档目录下：{expected_text}"
    );

    let options = AppRootOptions {
        layout: layout.clone(),
        default_relay: "https://relay.example".to_owned(),
        instance_id: Some("inst-vault-switch".to_owned()),
        allow_local_vault: true,
        operation_budget: Duration::from_secs(2),
    };
    let (root, cx) = open_root(cx, options);
    cx.run_until_parked();
    assert_eq!(state_of(&root, cx), RootStateLabel::Welcome);

    // Create 默认目录。
    root.update(cx, |root, cx| root.enter_create_form(cx));
    cx.run_until_parked();
    assert_eq!(
        root.update(cx, |root, cx| root.form_dir_text(cx)),
        Some(expected_text.clone()),
        "Create 表单默认目录必须是用户文档目录"
    );
    // 可以改成任意绝对路径（写入 FormInput，不联网/不建目录）。
    let custom = _dir.path().join("custom/create-vault");
    let custom_text = custom.display().to_string();
    root.update(cx, |root, cx| root.form_set_dir(&custom_text, cx));
    assert_eq!(
        root.update(cx, |root, cx| root.form_dir_text(cx)),
        Some(custom_text.clone()),
        "Create 表单必须接受任意绝对路径"
    );

    // Join 默认目录（同一默认根）。
    root.update(cx, |root, cx| {
        root.back_to_welcome(cx);
        root.enter_join_form(cx);
    });
    cx.run_until_parked();
    assert_eq!(
        root.update(cx, |root, cx| root.join_form_dir(cx)),
        Some(expected_text),
        "Join 表单默认目录也必须是用户文档目录"
    );
    let custom_join = _dir.path().join("custom/join-vault");
    let custom_join_text = custom_join.display().to_string();
    root.update(cx, |root, cx| root.join_form_set_dir(&custom_join_text, cx));
    assert_eq!(
        root.update(cx, |root, cx| root.join_form_dir(cx)),
        Some(custom_join_text),
        "Join 表单必须接受任意绝对路径"
    );
    assert!(
        !root
            .update(cx, |root, cx| root.join_form_dir(cx))
            .unwrap_or_default()
            .to_lowercase()
            .contains("appdata"),
        "自定义路径不得被改写回 LOCALAPPDATA"
    );
}

/// 断言路径在用户目录（HOME/USERPROFILE）下、而不是配置/数据根下。
fn expected_bounds_for_prefix(path: &str) -> bool {
    // 测试注入的临时根由 `discovered_layout` 决定：Linux HOME=<case>/home，
    // Windows USERPROFILE=<case>/home。两者都用 `home` 子目录。
    path.contains("/home/") || path.contains("\\home\\")
}

// ---------------------------------------------------------------------------
// 4) 本地（不同步）vault：自定义路径 + 持久化 + 旧 startup.json 兼容
// ---------------------------------------------------------------------------

#[cfg(not(feature = "dev-sync"))]
#[gpui::test]
async fn local_vault_uses_custom_path_and_is_remembered_across_restart(cx: &mut TestAppContext) {
    let (dir, layout) = temp_layout("local-custom");
    let default_path = layout.default_local_vault_path();
    let default_text = default_path.display().to_string();
    assert!(
        !default_text.to_lowercase().contains("appdata"),
        "本地库默认路径不得落在 LOCALAPPDATA：{default_text}"
    );

    let custom = dir.path().join("my-vaults/notes.sqlite3");
    let custom_text = custom.display().to_string();
    let startup_path = layout.startup_path();

    let (root, cx) = open_root(cx, product_options(layout.clone()));
    cx.run_until_parked();
    assert_eq!(state_of(&root, cx), RootStateLabel::Welcome);
    expect_bounds(cx, "welcome-new-local");
    expect_bounds(cx, "welcome-open-local");
    assert_eq!(
        root.update(cx, |root, cx| root.local_action_label(cx)),
        "新建本地 vault",
        "库不存在时按钮要明确是「新建」"
    );
    assert!(!startup_path.exists(), "进入 Welcome 本身不得写启动偏好");

    // 展开「新建本地仓库」→ 设置自定义路径 → 确认（不存在 = 新建空库）。
    click(cx, "welcome-new-local");
    expect_bounds(cx, "welcome-new-local-dir");
    expect_bounds(cx, "welcome-new-local-browse");
    root.update(cx, |root, cx| {
        root.welcome_set_new_local_dir(&custom_text, cx)
    });
    assert_eq!(
        root.update(cx, |root, cx| root.welcome_new_local_dir_text(cx)),
        Some(custom_text.clone())
    );
    click(cx, "welcome-new-local-confirm");
    assert_eq!(state_of(&root, cx), RootStateLabel::Main);
    assert!(root.update(cx, |root, _| root.is_local_mode()));
    assert!(custom.is_file(), "自定义路径不存在时必须新建空库");
    assert!(!default_path.exists(), "不得同时把默认路径也创建出来");
    let startup = read_json(&startup_path);
    assert_eq!(startup["mode"], "local");
    assert_eq!(startup["version"], serde_json::json!(1));
    assert_eq!(
        startup["local_vault_path"].as_str(),
        Some(custom_text.as_str()),
        "startup.json 必须记住实际本地路径"
    );

    // 重启：直接恢复同一路径，输入框也显示它。
    assert!(cx.simulate_close(), "本地 Main 允许关窗");
    cx.run_until_parked();
    let restarted = cx.update(|window, cx| {
        window.replace_root(cx, move |_window, cx| {
            AppRoot::new(product_options(layout.clone()), cx)
        })
    });
    cx.run_until_parked();
    assert_eq!(state_of(&restarted, cx), RootStateLabel::Main);
    assert!(restarted.update(cx, |root, _| root.is_local_mode()));
    assert_eq!(
        restarted.update(cx, |root, cx| root.local_path_text(cx)),
        Some(custom_text.clone()),
        "重启后输入框必须显示上次路径"
    );
    assert_eq!(
        restarted.update(cx, |root, cx| root.local_action_label(cx)),
        "打开本地 vault",
        "库存在时按钮应变成「打开」"
    );
    assert!(
        restarted
            .update(cx, |root, _| root.main_shell().cloned())
            .is_some(),
        "自动恢复必须有 AppShell"
    );
}

/// 旧 `startup.json`（只有 `{version,mode}`）+ 旧数据根下的 `local/vault.sqlite3`：
/// 升级后仍继续打开旧库（不能看起来"丢库"），并把新路径补写进偏好。
#[cfg(not(feature = "dev-sync"))]
#[gpui::test]
async fn legacy_startup_json_without_path_still_opens_old_local_vault(cx: &mut TestAppContext) {
    let (_dir, layout) = temp_layout("legacy-startup");
    let legacy_path = layout.local_vault_path();
    menghunote_app::dev_vault::open_local_vault_at(&legacy_path).expect("创建旧本地库失败");
    std::fs::create_dir_all(layout.data_root()).expect("建数据根失败");
    std::fs::write(
        layout.startup_path(),
        b"{\n  \"version\": 1,\n  \"mode\": \"local\"\n}\n",
    )
    .expect("写旧 startup.json 失败");

    let (root, cx) = open_root(cx, product_options(layout.clone()));
    cx.run_until_parked();
    assert_eq!(
        state_of(&root, cx),
        RootStateLabel::Main,
        "旧偏好 + 旧库存在时必须直接恢复 Main"
    );
    assert!(root.update(cx, |root, _| root.is_local_mode()));
    assert_eq!(
        root.update(cx, |root, cx| root.local_path_text(cx)),
        Some(legacy_path.display().to_string()),
        "旧格式没有 local_vault_path：必须回退到旧数据根下的 local/vault.sqlite3"
    );
    let preferences = load_startup_preferences(&layout.startup_path()).expect("偏好必须可读");
    assert_eq!(preferences.mode, StartupMode::Local);
    assert_eq!(
        preferences.local_vault_path.as_deref(),
        Some(legacy_path.as_path()),
        "恢复成功后应把实际路径补写进 startup.json（下次不再依赖旧回退）"
    );
}

/// 平台默认布局：vault 默认根在用户 Documents（配置仍在平台配置目录）。
#[test]
fn discovered_layout_defaults_vaults_under_user_documents_not_localappdata() {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "vault-switch-discover-default-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).expect("创建测试根失败");
    let home = root.join("home");
    let documents = home.join("Documents");
    std::fs::create_dir_all(&documents).expect("创建 Documents 失败");
    let layout = discovered_layout(&root);

    let local = layout.default_local_vault_path();
    let instance = layout
        .default_instance_dir("inst-default")
        .expect("合法实例 id");
    for path in [&local, &instance] {
        let text = path.to_string_lossy().to_lowercase();
        assert!(!text.contains("localappdata"), "{text}");
        assert!(!text.contains("appdata"), "{text}");
        assert!(
            path.starts_with(documents.join("menghunote")),
            "默认 vault 必须在用户 Documents/menghunote 下：{}",
            path.display()
        );
    }
    assert_eq!(
        local,
        documents
            .join("menghunote")
            .join("local")
            .join(VAULT_FILE_NAME)
    );
    // 配置仍是小公开文件，留在平台配置目录（并未搬到文档目录）。
    assert!(
        !layout.config_path.starts_with(documents.join("menghunote")),
        "sync.json 仍应留在应用配置目录：{}",
        layout.config_path.display()
    );
}

// ---------------------------------------------------------------------------
// 7) 底栏「同步」→ AppRoot 设置页的「同步」分类（不再打开 AppShell 旧组合设置页）
// ---------------------------------------------------------------------------

/// 用户反馈：点右下角状态栏「同步」时，设置按钮突出、同步下方混入外观/语言/模板。
///
/// 生产 `AppRoot` 创建的 `AppShell` 把设置入口交给宿主：点击 `sync-status` 必须打开
/// **AppRoot 设置页**并选中 `SettingsSection::Sync`；`AppShell` 自己的组合设置页
/// （`settings-panel`）不得打开。
#[gpui::test]
async fn bottom_sync_status_opens_app_root_sync_settings(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let (_dir, layout) = temp_layout("sync-status-route");
    let first = make_instance(&layout, "inst-a", "甲库笔记");
    let mut profile = DevSyncProfile::default();
    profile.upsert_active(first.clone());
    layout.store().save(&profile).expect("写测试公开配置失败");

    let (root, cx) = open_root(cx, product_options(layout.clone()));
    cx.run_until_parked();
    assert_eq!(state_of(&root, cx), RootStateLabel::Main);
    let shell = root
        .update(cx, |root, _| root.main_shell().cloned())
        .expect("Main 必须有 AppShell");
    assert!(
        !shell.update(cx, |shell, _| shell.settings_open()),
        "启动时 AppShell 自带设置页必须是关的"
    );

    // 底栏状态行真实参与布局 → 点击（不是直接调方法）。
    let status = expect_bounds(cx, "sync-status");
    cx.simulate_click(status.center(), Modifiers::default());
    cx.run_until_parked();

    assert!(
        root.update(cx, |root, _| root.settings_is_open()),
        "点底栏「同步」必须打开 AppRoot 设置页"
    );
    assert_eq!(
        root.update(cx, |root, _| root.settings_section()),
        SettingsSection::Sync,
        "必须直接选中「同步」分类"
    );
    expect_bounds(cx, "settings-section-sync");
    expect_bounds(cx, "settings-toggle-sync");
    // 旧组合设置页不打开：不再出现「同步」下面混排外观/语言/模板。
    assert!(
        cx.debug_bounds("settings-panel").is_none(),
        "不得再打开 AppShell 旧组合设置页（settings-panel）"
    );
    assert!(
        !shell.update(cx, |shell, _| shell.settings_open()),
        "AppShell 自己的 settings_open 必须保持 false（入口由 AppRoot 接管）"
    );
    assert!(
        cx.debug_bounds("settings-section-appearance").is_none(),
        "只渲染「同步」分类内容，不混入外观/语言/模板"
    );
    drop(shell);
}

// ---------------------------------------------------------------------------
// 8) Obsidian 式 Welcome：左栏仓库列表（同步实例 + 本地库）直接打开
// ---------------------------------------------------------------------------

/// 左栏列出两个同步实例 + 本地库；点击实例行直接进入 Main（不要求先「设为默认」），
/// 点击本地行直接打开本地库（不经过管理页）。
#[gpui::test]
async fn welcome_left_list_lists_sync_and_local_and_opens_directly(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let (_dir, layout) = temp_layout("welcome-list");
    let first = make_instance(&layout, "inst-a", "甲库笔记");
    let second = make_instance(&layout, "inst-b", "乙库笔记");
    // 只登记、不设 active：启动停在 Welcome（不是自动打开 Main）。
    let mut profile = DevSyncProfile::default();
    profile.instances.push(first.clone());
    profile.instances.push(second.clone());
    layout.store().save(&profile).expect("写测试公开配置失败");
    // 默认本地库已存在：左栏出现「本地仓库」行。
    std::fs::create_dir_all(layout.local_dir()).expect("创建 local 目录失败");
    menghunote_app::dev_vault::open_local_vault_at(&layout.local_vault_path())
        .expect("创建测试本地库失败");

    let (root, cx) = open_root(cx, product_options(layout.clone()));
    cx.run_until_parked();
    assert_eq!(state_of(&root, cx), RootStateLabel::Welcome);
    expect_bounds(cx, "welcome-vault-list");
    expect_bounds(cx, "welcome-vault-0");
    expect_bounds(cx, "welcome-vault-1");
    expect_bounds(cx, "welcome-vault-local");

    // 点击第 0 行（inst-a）→ 直接打开该实例，不需要先「设为默认」。
    let row = expect_bounds(cx, "welcome-vault-0");
    cx.simulate_click(row.center(), Modifiers::default());
    cx.run_until_parked();
    assert_eq!(state_of(&root, cx), RootStateLabel::Main);
    assert_eq!(
        root.update(cx, |root, _| root
            .opened_instance()
            .map(|entry| entry.instance_id.clone())),
        Some("inst-a".to_owned()),
        "点左栏实例行必须直接打开对应的库"
    );
    assert!(
        lock_path(&layout, "inst-a").is_file(),
        "打开实例后应持有该实例目录锁"
    );

    // 返回 Welcome，再点本地行 → 直接进入本地（不同步）Main。
    root.update(cx, |root, cx| root.back_to_welcome(cx));
    cx.run_until_parked();
    assert_eq!(state_of(&root, cx), RootStateLabel::Welcome);
    let local = expect_bounds(cx, "welcome-vault-local");
    cx.simulate_click(local.center(), Modifiers::default());
    cx.run_until_parked();
    assert_eq!(state_of(&root, cx), RootStateLabel::Main);
    assert!(
        root.update(cx, |root, _| root.is_local_mode()),
        "点左栏本地行必须进入本地（不同步）Main"
    );
    expect_bounds(cx, "app-root-local-mode");
}

// ---------------------------------------------------------------------------
// 9) 新建本地仓库：目录 → 空 vault.sqlite3；已有库不覆盖；startup 写 local
// ---------------------------------------------------------------------------

#[gpui::test]
async fn welcome_new_local_creates_empty_vault_and_refuses_existing(cx: &mut TestAppContext) {
    let (_dir, layout) = temp_layout("welcome-new-local");
    let custom_dir = layout.data_root().join("custom-vaults").join("notes");
    let vault_path = custom_dir.join(VAULT_FILE_NAME);
    let startup_path = layout.startup_path();
    let (root, cx) = open_root(cx, product_options(layout.clone()));
    cx.run_until_parked();
    assert_eq!(state_of(&root, cx), RootStateLabel::Welcome);

    // 展开 → 填目录 → 创建；空库（不得播种 demo）。
    let card = expect_bounds(cx, "welcome-new-local");
    cx.simulate_click(card.center(), Modifiers::default());
    cx.run_until_parked();
    root.update(cx, |root, cx| {
        root.welcome_set_new_local_dir(&custom_dir.display().to_string(), cx);
        root.welcome_confirm_new_local(cx);
    });
    cx.run_until_parked();
    assert_eq!(state_of(&root, cx), RootStateLabel::Main);
    assert!(root.update(cx, |root, _| root.is_local_mode()));
    assert!(vault_path.is_file(), "必须在所选目录创建 vault.sqlite3");
    let shell = root
        .update(cx, |root, _| root.main_shell().cloned())
        .expect("本地 Main 必须有 AppShell");
    assert!(
        sidebar_titles(&shell, cx).is_empty(),
        "新建本地仓库不得播种 demo 笔记"
    );
    let startup = read_json(&startup_path);
    assert_eq!(startup["mode"], "local");
    assert_eq!(
        startup["local_vault_path"].as_str(),
        Some(vault_path.to_string_lossy().as_ref()),
        "startup.json 必须写实际本地路径"
    );

    // 返回 Welcome：左栏本地行出现（同一路径）。
    root.update(cx, |root, cx| root.back_to_welcome(cx));
    cx.run_until_parked();
    expect_bounds(cx, "welcome-vault-local");

    // 「新建」同一目录必须明确提示改用「打开」，不得覆盖既有库。
    let before = std::fs::read(&vault_path).expect("读库失败");
    root.update(cx, |root, cx| {
        root.welcome_set_new_local_dir(&custom_dir.display().to_string(), cx);
        root.welcome_confirm_new_local(cx);
    });
    cx.run_until_parked();
    assert_eq!(
        state_of(&root, cx),
        RootStateLabel::Welcome,
        "已有库时新建必须留在 Welcome（不覆盖）"
    );
    let error = root
        .update(cx, |root, _| root.welcome_error().map(str::to_owned))
        .expect("必须有内联错误");
    assert!(
        error.contains("打开本地仓库") || error.contains("已有库"),
        "错误必须提示改用「打开」：{error}"
    );
    assert_eq!(
        std::fs::read(&vault_path).expect("读库失败"),
        before,
        "已有库字节不得被新建覆盖"
    );
}

// ---------------------------------------------------------------------------
// 10) 打开本地仓库：已有库打开；缺失明确报错且不创建
// ---------------------------------------------------------------------------

#[gpui::test]
async fn welcome_open_local_opens_existing_and_rejects_missing(cx: &mut TestAppContext) {
    let (_dir, layout) = temp_layout("welcome-open-local");
    let missing = layout.data_root().join("not-there").join(VAULT_FILE_NAME);
    let (root, cx) = open_root(cx, product_options(layout.clone()));
    cx.run_until_parked();

    let card = expect_bounds(cx, "welcome-open-local");
    cx.simulate_click(card.center(), Modifiers::default());
    cx.run_until_parked();
    root.update(cx, |root, cx| {
        root.welcome_set_open_local_path(&missing.display().to_string(), cx);
        root.welcome_confirm_open_local(cx);
    });
    cx.run_until_parked();
    assert_eq!(state_of(&root, cx), RootStateLabel::Welcome);
    let error = root
        .update(cx, |root, _| root.welcome_error().map(str::to_owned))
        .expect("缺失库必须内联报错");
    assert!(
        error.contains("不存在") && error.contains(&missing.display().to_string()),
        "错误必须点名缺失路径：{error}"
    );
    assert!(!missing.exists(), "打开缺失库不得凭空创建文件");

    // 创建一个真实库后从同一表单打开（已有库复用，不重置）。
    std::fs::create_dir_all(missing.parent().expect("父目录")).expect("建目录失败");
    let vault = Vault::open(&missing).expect("创建本地库失败");
    let mut writer = StructureWriter::open(&vault, 1).expect("结构写入口失败");
    writer
        .doc()
        .create_note("已存在的本地笔记", 1)
        .expect("建笔记失败");
    writer.commit("welcome-open-local-seed").expect("提交失败");
    drop(writer);
    root.update(cx, |root, cx| root.welcome_confirm_open_local(cx));
    cx.run_until_parked();
    assert_eq!(state_of(&root, cx), RootStateLabel::Main);
    assert!(root.update(cx, |root, _| root.is_local_mode()));
    let shell = root
        .update(cx, |root, _| root.main_shell().cloned())
        .expect("本地 Main 必须有 AppShell");
    assert_eq!(
        sidebar_titles(&shell, cx),
        vec!["已存在的本地笔记".to_owned()],
        "打开已有库必须复用内容（不是新建空库）"
    );
}

// ---------------------------------------------------------------------------
// 7) 用户自定义仓库名称（display_name）：显示优先 + 行内重命名 + 向后兼容
// ---------------------------------------------------------------------------

/// 与 `click` 相同，但 selector 是运行时拼接的（`vault-manager-rename-<i>` 等）。
fn click_selector(cx: &mut VisualTestContext, selector: &'static str) {
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("必须画出 selector={selector}"));
    assert!(
        bounds.size.width.as_f32() > 0.0 && bounds.size.height.as_f32() > 0.0,
        "{selector} 必须有非零尺寸：{bounds:?}"
    );
    cx.simulate_click(bounds.center(), Modifiers::default());
    cx.run_until_parked();
}

/// 有自定义名时：左下角按钮、切换菜单当前行、管理仓库三处用同一个名字；
/// 名字**不改变**目录/路径/身份（显示层只读 `display_name`）。
#[gpui::test]
async fn display_name_is_used_by_switcher_menu_and_manager(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let (_dir, layout) = temp_layout("display-name-show");
    let mut first = make_instance(&layout, "inst-a", "甲库笔记");
    first.display_name = Some("研究库".to_owned());
    let mut second = make_instance(&layout, "inst-b", "乙库笔记");
    second.display_name = Some("写作 库 🚀".to_owned());
    let first_path = first.vault_path.clone();
    save_two_instances(&layout, &first, &second);

    let (root, cx) = open_root(cx, product_options(layout.clone()));
    cx.run_until_parked();
    assert_eq!(state_of(&root, cx), RootStateLabel::Main);

    // 左下角按钮 = 自定义名（不是 `inst-a`/`vault.sqlite3`）。
    assert_eq!(
        root.update(cx, |root, cx| root.current_vault_name(cx)),
        "研究库",
        "左下角必须显示自定义名"
    );

    // 切换菜单：当前行与左下角同名，其它行显示各自名字。
    click(cx, "app-root-vault-switcher");
    let rows = root.update(cx, |root, cx| root.vault_menu_rows(cx));
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].label, "✓ 研究库", "菜单当前行必须与左下角同名");
    assert_eq!(rows[1].label, "写作 库 🚀", "其它行显示自定义名");

    // 管理仓库：每行 = 名称 + 路径（路径仍在，说明命名不改路径）。
    click(cx, "app-root-vault-manage");
    let rows = root.update(cx, |root, _| root.vault_manager_rows());
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].name, "研究库");
    assert_eq!(rows[0].instance_id, "inst-a");
    assert_eq!(
        rows[0].path, first_path,
        "自定义名只影响显示：管理仓库仍显示真实库路径"
    );
    assert_eq!(rows[1].name, "写作 库 🚀");
    assert!(
        rows[1].path.contains("inst-b"),
        "第二行路径必须仍是实例目录：{}",
        rows[1].path
    );
    // 磁盘上的路径/vault_id 没有被改名动过。
    let profile = layout.store().load().expect("读配置").expect("配置存在");
    let a = profile.find("inst-a").expect("A 仍在");
    assert_eq!(a.vault_path, first_path);
    assert_eq!(a.display_name.as_deref(), Some("研究库"));
    assert_eq!(a.vault_id, first.vault_id);
}

/// 管理仓库「重命名」：行内输入 → 保存 → `sync.json` 更新、左下角/菜单/管理仓库同步；
/// 不移动目录、不改 active/vault_path/vault_id。
#[gpui::test]
async fn vault_manager_rename_updates_sync_json_and_all_three_places(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let (_dir, layout) = temp_layout("display-name-rename");
    let first = make_instance(&layout, "inst-a", "甲库笔记");
    let second = make_instance(&layout, "inst-b", "乙库笔记");
    let config_path = save_two_instances(&layout, &first, &second);
    let before = layout.store().load().expect("读配置").expect("配置存在");
    let b_before = before.find("inst-b").cloned().expect("B 存在");
    let b_dir = layout.instance_dir("inst-b").expect("合法 id");

    let (root, cx) = open_root(cx, product_options(layout.clone()));
    cx.run_until_parked();
    assert_eq!(state_of(&root, cx), RootStateLabel::Main);
    assert_eq!(
        root.update(cx, |root, cx| root.current_vault_name(cx)),
        "inst-a",
        "没有自定义名时回退目录名（既有行为不变）"
    );

    click(cx, "app-root-vault-switcher");
    click(cx, "app-root-vault-manage");
    assert!(root.update(cx, |root, _| root.vault_manager_is_open()));

    // 行内重命名第二行（inst-b）：打开不写盘。
    let config_before_rename = std::fs::read(&config_path).expect("读配置原文");
    click_selector(cx, "vault-manager-rename-1");
    assert!(
        root.update(cx, |root, _| root.vault_rename_is_open()),
        "点击「重命名」必须打开行内输入"
    );
    assert_eq!(
        root.update(cx, |root, _| root
            .vault_rename_instance_id()
            .map(str::to_owned)),
        Some("inst-b".to_owned())
    );
    expect_bounds(cx, "vault-manager-rename-input");
    expect_bounds(cx, "vault-manager-rename-save");
    expect_bounds(cx, "vault-manager-rename-cancel");
    assert_eq!(
        std::fs::read(&config_path).expect("读配置原文"),
        config_before_rename,
        "打开重命名不得写盘"
    );

    // 输入新名字 → 保存。
    root.update(cx, |root, cx| {
        root.vault_rename_set_text("  乙库 工作区  ", cx)
    });
    click(cx, "vault-manager-rename-save");
    assert!(
        !root.update(cx, |root, _| root.vault_rename_is_open()),
        "保存成功后必须关闭行内输入"
    );
    let after = layout.store().load().expect("读配置").expect("配置存在");
    let b_after = after.find("inst-b").cloned().expect("B 仍在");
    assert_eq!(
        b_after.display_name.as_deref(),
        Some("乙库 工作区"),
        "sync.json 必须写入 trim 后的自定义名"
    );
    assert_eq!(b_after.vault_path, b_before.vault_path, "不得移动/改库路径");
    assert_eq!(b_after.vault_id, b_before.vault_id, "不得改 vault_id");
    assert_eq!(b_after.device_id, b_before.device_id, "不得改 device_id");
    assert_eq!(
        after.active_instance.as_deref(),
        Some("inst-a"),
        "重命名不得改 active_instance"
    );
    assert!(b_dir.is_dir(), "重命名不得移动实例目录");

    // 三处同步：菜单行、管理仓库行（当前按钮仍是 inst-a，切过去后再验证）。
    let menu = root.update(cx, |root, cx| root.vault_menu_rows(cx));
    assert_eq!(menu[1].label, "乙库 工作区", "切换菜单必须立刻刷新");
    let manager = root.update(cx, |root, _| root.vault_manager_rows());
    assert_eq!(manager[1].name, "乙库 工作区", "管理仓库必须立刻刷新");

    // 真实切到 B（关旧开新）后，左下角显示同一个名字。
    // 先关管理仓库（它盖住左下角按钮；这时点按钮只会关覆盖层）。
    click(cx, "vault-manager-close");
    assert!(!root.update(cx, |root, _| root.vault_manager_is_open()));
    click(cx, "app-root-vault-switcher");
    click(cx, "app-root-vault-menu-instance-1");
    assert_eq!(
        root.update(cx, |root, cx| root.current_vault_name(cx)),
        "乙库 工作区",
        "切换后左下角必须显示自定义名"
    );
    assert_eq!(state_of(&root, cx), RootStateLabel::Main);
}

/// 空白 = 清除自定义名（回退目录名）；Esc/「取消」不改磁盘；
/// 超长名字在行内报错且不写盘。
#[gpui::test]
async fn vault_manager_rename_blank_clears_and_escape_cancels(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let (_dir, layout) = temp_layout("display-name-cancel");
    let mut first = make_instance(&layout, "inst-a", "甲库笔记");
    first.display_name = Some("甲库".to_owned());
    let mut profile = DevSyncProfile::default();
    profile.upsert_active(first);
    layout.store().save(&profile).expect("写配置失败");
    let config_path = layout.config_path.clone();

    let (root, cx) = open_root(cx, product_options(layout.clone()));
    cx.run_until_parked();
    assert_eq!(state_of(&root, cx), RootStateLabel::Main);
    assert_eq!(
        root.update(cx, |root, cx| root.current_vault_name(cx)),
        "甲库"
    );

    click(cx, "app-root-vault-switcher");
    click(cx, "app-root-vault-manage");

    // 行内输入自动聚焦：打开后直接打字就进输入框（不需要再点一次）。
    click_selector(cx, "vault-manager-rename-0");
    assert_eq!(
        root.update(cx, |root, cx| root.vault_rename_text(cx)),
        Some("甲库".to_owned()),
        "重命名输入预填当前自定义名"
    );
    cx.simulate_input("新");
    cx.run_until_parked();
    assert_eq!(
        root.update(cx, |root, cx| root.vault_rename_text(cx)),
        Some("甲库新".to_owned()),
        "行内输入必须自动聚焦（打开后可直接打字）"
    );

    // 空白 → 清除自定义名，回退目录名（sync.json 里是 null）。
    root.update(cx, |root, cx| root.vault_rename_set_text("   ", cx));
    click(cx, "vault-manager-rename-save");
    assert!(!root.update(cx, |root, _| root.vault_rename_is_open()));
    let cleared = layout.store().load().expect("读配置").expect("配置存在");
    assert_eq!(
        cleared.find("inst-a").and_then(|e| e.display_name.clone()),
        None,
        "空白输入必须清除自定义名"
    );
    assert_eq!(
        root.update(cx, |root, cx| root.current_vault_name(cx)),
        "inst-a",
        "清除后左下角回退目录名"
    );
    assert_eq!(
        root.update(cx, |root, _| root.vault_manager_rows())[0].name,
        "inst-a"
    );

    // Esc：取消，不写盘、不关管理仓库。
    let config_before = std::fs::read(&config_path).expect("读配置原文");
    click_selector(cx, "vault-manager-rename-0");
    root.update(cx, |root, cx| root.vault_rename_set_text("临时名字", cx));
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(
        !root.update(cx, |root, _| root.vault_rename_is_open()),
        "Esc 必须只取消行内重命名"
    );
    assert!(
        root.update(cx, |root, _| root.vault_manager_is_open()),
        "Esc 取消重命名不得关闭管理仓库"
    );
    assert_eq!(
        std::fs::read(&config_path).expect("读配置原文"),
        config_before,
        "Esc/取消不得改磁盘配置"
    );

    // 「取消」按钮：同样不改磁盘。
    click_selector(cx, "vault-manager-rename-0");
    root.update(cx, |root, cx| root.vault_rename_set_text("另一个名字", cx));
    click(cx, "vault-manager-rename-cancel");
    assert!(!root.update(cx, |root, _| root.vault_rename_is_open()));
    assert_eq!(
        std::fs::read(&config_path).expect("读配置原文"),
        config_before,
        "取消按钮不得改磁盘配置"
    );

    // 超长名字：行内报错、保持输入、配置不变。
    click_selector(cx, "vault-manager-rename-0");
    let too_long = "库".repeat(65);
    root.update(cx, |root, cx| root.vault_rename_set_text(&too_long, cx));
    click(cx, "vault-manager-rename-save");
    assert!(
        root.update(cx, |root, _| root.vault_rename_is_open()),
        "非法名字必须保留行内输入（可修正后重试）"
    );
    let error = root
        .update(cx, |root, _| root.vault_rename_error().map(str::to_owned))
        .expect("非法名字必须有行内错误");
    assert!(error.contains("64"), "错误必须说明长度上限：{error}");
    expect_bounds(cx, "vault-manager-rename-error");
    assert_eq!(
        std::fs::read(&config_path).expect("读配置原文"),
        config_before,
        "非法名字不得写盘"
    );
    root.update(cx, |root, cx| root.cancel_vault_rename(cx));
}

/// 向后兼容：磁盘上是**没有 `display_name` 字段**的旧 v2 配置 → 仍能进 Main，
/// 显示回退目录名；window 头一次保存也不会把版本升到 3。
#[gpui::test]
async fn legacy_v2_profile_without_display_name_still_opens_main(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let (_dir, layout) = temp_layout("display-name-legacy");
    let entry = make_instance(&layout, "inst-legacy", "旧配置笔记");
    std::fs::create_dir_all(layout.config_path.parent().expect("父目录")).expect("建目录失败");
    let json = format!(
        r#"{{
          "version": 2,
          "active_instance": "inst-legacy",
          "instances": [{{
            "instance_id": "inst-legacy",
            "relay_mode": "remote_tls",
            "api_version": 2,
            "relay_url": "https://relay.example",
            "vault_path": {:?},
            "vault_id": {:?},
            "device_id": {:?}
          }}]
        }}"#,
        entry.vault_path, entry.vault_id, entry.device_id
    );
    std::fs::write(&layout.config_path, json).expect("写旧 v2 配置失败");

    let (root, cx) = open_root(cx, product_options(layout.clone()));
    cx.run_until_parked();
    assert_eq!(
        state_of(&root, cx),
        RootStateLabel::Main,
        "缺 display_name 的旧 v2 配置必须仍能打开 Main"
    );
    assert_eq!(
        root.update(cx, |root, cx| root.current_vault_name(cx)),
        "inst-legacy",
        "没有自定义名时回退目录名"
    );
    assert_eq!(
        root.update(cx, |root, _| root.entries())[0].display_name,
        None
    );
    click(cx, "app-root-vault-switcher");
    click(cx, "app-root-vault-manage");
    // 旧配置上也能直接重命名（新增能力不要求重写配置）。
    click_selector(cx, "vault-manager-rename-0");
    root.update(cx, |root, cx| root.vault_rename_set_text("旧库新名", cx));
    click(cx, "vault-manager-rename-save");
    let loaded = layout.store().load().expect("读配置").expect("配置存在");
    assert_eq!(loaded.version, 2, "只增可选字段：配置版本仍为 2");
    assert_eq!(
        loaded
            .find("inst-legacy")
            .and_then(|e| e.display_name.clone()),
        Some("旧库新名".to_owned())
    );
    assert_eq!(
        root.update(cx, |root, cx| root.current_vault_name(cx)),
        "旧库新名"
    );
}
