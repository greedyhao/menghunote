//! 同步向导（ADR 0011，S1+S2+S3）的 headless UI 测试。
//!
//! 覆盖的验收线：
//!
//! 1. 没有公开配置 → `AppRoot` 停在 Welcome（selector 固定）；
//! 2. 真实 dev relay + 临时目录：Create → Working（**关窗被拦截**）→ BackupRequired
//!    （展示 `vault_id` + 恢复码）→ 勾选备份 → Main 空树 → 配置写入且 active 指向该实例
//!    → "重启"（新 `AppRoot` 根视图）自动进 Main；
//! 3. **Join（S3）**：A（真实 Create + 建笔记 + 发布基线）→ B 在 `JoinForm` 输入
//!    relay/vault_id/恢复码/新目录 → Working（关窗拦截 + 取消请求）→ 迟到的成功仍进
//!    Main、自动打开 A 的笔记（正文正确）、配置 active 写入、重启直接 Main；
//! 4. **Join 失败恢复矩阵**：错格式恢复码 / 错但格式合法的恢复码 → 回表单、保留公开字段、
//!    脱敏错误、可改后重试成功；A 未发布基线 → 明确"请先发布基线"且不进 Main；
//!    目录已有库/身份、目录被占用 → 明确错误且不覆盖；relay 不可达 → 可重试；
//! 5. 配置损坏 / 身份缺失 / 库与配置错配 → Error 页，**不创建新库**；
//! 6. 库/身份事实与配置一致时，AppRoot 不需要任何 env/flag（本文件全程不设
//!    `MENGHUNOTE_DEV_VAULT` / `MENGHUNOTE_DEV_SYNC`）。
//!
//! 纪律（AGENTS.md §4/§6）：
//!
//! - 临时目录一律在 `CARGO_TARGET_TMPDIR`（`target/tmp`），不用 `%TEMP%`；Linux 用户
//!   主键的 `XDG_DATA_HOME` 也指到 `target/tmp`；
//! - **不 sleep**：Create/Join 的收敛点是 `wait_for_*_terminal`（阻塞在 worker 终态
//!   channel，有超时）与 `run_until_parked`；不是"事件队列为空"这种伪判据；
//! - relay 用进程内 `menghunote-dev-relay`（真实 axum，放行验签 hook）。⚠️ 非生产：
//!   本文件不能作为鉴权/验签验收。
//!
//! feature 关闭时整个文件编译为空（0 个测试），默认 `cargo test -p menghunote-app`
//! 完全不受影响。

#![cfg(feature = "dev-sync")]

use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui::{Entity, Modifiers, TestAppContext, VisualTestContext};
use menghunote_app::dev_sync::DevIdentityStore;
use menghunote_app::onboarding::{
    AppRoot, AppRootOptions, DevSyncProfile, InstanceEntry, InstanceLock, ProfileLayout,
    ProfileStore, RelayApiVersion, RelayMode, RootStateLabel, DEFAULT_OPERATION_BUDGET,
    LOCK_FILE_NAME, VAULT_FILE_NAME,
};
use menghunote_app::ui_text;
use menghunote_dev_relay::DevRelay;
use menghunote_storage::{
    append_updates_with_sync_intents, list_tree, load_body_doc, local_sync_intent, StructureWriter,
    SyncContext, UpdateRecord, Vault,
};
use menghunote_sync::create_link::{CreateLink, CreateTicket};
use menghunote_sync::dev_local::DevLocal;
use menghunote_sync::http_transport::DEFAULT_TRANSPORT_TIMEOUTS;
use menghunote_sync::identity::LocalIdentityV1;
use menghunote_sync::random::OsRandom;
use menghunote_sync::recovery::RecoverySeed;
use menghunote_sync::relay_context::RelayContext;
use menghunote_sync::remote_init::create_remote;
use menghunote_sync::SyncError;
use tempfile::{Builder as TempDirBuilder, TempDir};

/// 测试进程级的 `XDG_DATA_HOME`（Linux 用户主键；Windows 是 no-op）。
fn ensure_test_user_key() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("onboarding-ui-xdg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("创建测试 XDG_DATA_HOME 失败");
        std::env::set_var("XDG_DATA_HOME", &dir);
        menghunote_sync::identity_wrap::initialize_user_key()
            .expect("初始化测试用户主键失败（ADR 0008）");
    });
}

/// 为全新 vault 预留票据并编码成创建链接（测试只走 App 的链接入口）。
fn create_link_for(relay: &DevRelay, vault_id: [u8; 16], ttl_secs: u64) -> String {
    let raw = relay
        .issue_create_ticket(vault_id, Duration::from_secs(ttl_secs))
        .expect("发行创建票据失败");
    let ticket = CreateTicket::from_slice(&raw).expect("票据必须是 32B");
    CreateLink::encode(
        RelayMode::LocalDev,
        relay.base_url(),
        vault_id,
        ticket,
        None,
    )
    .expect("编码创建链接失败")
    .url
}

/// 同上，但用 HTTPS（remote_tls）链接。
fn create_remote_link_for(relay: &DevRelay, vault_id: [u8; 16], ttl_secs: u64) -> String {
    let raw = relay
        .issue_create_ticket(vault_id, Duration::from_secs(ttl_secs))
        .expect("发行创建票据失败");
    let ticket = CreateTicket::from_slice(&raw).expect("票据必须是 32B");
    CreateLink::encode(
        RelayMode::RemoteTls,
        relay.base_url(),
        vault_id,
        ticket,
        None,
    )
    .expect("编码 HTTPS 创建链接失败")
    .url
}

/// 真实表单动作：进入 Create、粘贴链接、应用。
fn apply_create_link(root: &Entity<AppRoot>, cx: &mut VisualTestContext, link: &str) {
    let link = link.to_owned();
    root.update(cx, |root, cx| {
        root.enter_create_link_form(cx);
        root.form_set_create_link(&link, cx);
        root.form_apply_create_link(cx);
    });
    cx.run_until_parked();
}

/// 在 `CARGO_TARGET_TMPDIR`（`target/tmp`）下开一个独立临时目录 + 显式公开目录布局。
fn temp_layout(prefix: &str) -> (TempDir, ProfileLayout) {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("onboarding-ui-{prefix}-{}", std::process::id()));
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

/// 开一个 `AppRoot` 根视图（并注册关窗分派，测试可以 `simulate_close`）。
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

/// 只读取值（`Entity::read` 要 `&App`；测试里经 `update` 借一次上下文）。
fn state_of(root: &Entity<AppRoot>, cx: &mut VisualTestContext) -> RootStateLabel {
    root.update(cx, |root, _| root.state_label())
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

/// 验收 1：没有配置 → Welcome（创建/加入/实例列表都在）。
#[gpui::test]
async fn onboarding_welcome_is_shown_without_any_config(cx: &mut TestAppContext) {
    let (_dir, layout) = temp_layout("welcome");
    let options =
        AppRootOptions::for_test(layout.clone(), "http://127.0.0.1:18080", "inst-welcome");
    let (root, cx) = open_root(cx, options);
    cx.run_until_parked();

    assert_eq!(state_of(&root, cx), RootStateLabel::Welcome);
    for selector in [
        "app-root",
        "app-root-welcome",
        "welcome-vault-list",
        "welcome-new-local",
        "welcome-open-local",
        "welcome-sync-remote",
    ] {
        expect_bounds(cx, selector);
    }
    // 没有 active 实例：Main 与备份页都不存在；没有本地库时左栏没有本地行。
    assert!(cx.debug_bounds("app-root-main").is_none());
    assert!(cx.debug_bounds("app-root-backup").is_none());
    for old in [
        "app-root-create",
        "app-root-join",
        "app-root-local",
        "app-root-instances",
    ] {
        assert!(
            cx.debug_bounds(old).is_none(),
            "旧 Welcome 入口必须移除：{old}"
        );
    }
    assert!(cx.debug_bounds("welcome-vault-local").is_none());
    // 没有配置时不该凭空创建库/身份/配置。
    assert!(
        !layout.config_path.exists(),
        "读不到配置不算错误，也不得写配置"
    );
}

/// P2：`dev-sync` 构建**保留**开发警示条与「继续本地」开发入口。
///
/// 产品构建的不存在性由 `sync_ui_product` 断言（同一 selector 在两种构建里互斥）。
#[gpui::test]
async fn dev_build_shows_dev_banner_and_local_entry(cx: &mut TestAppContext) {
    let (_dir, layout) = temp_layout("dev-banner");
    let mut options = AppRootOptions::for_test(layout, "http://127.0.0.1:18080", "inst-dev-banner");
    options.allow_local_vault = true;
    let (root, cx) = open_root(cx, options);
    cx.run_until_parked();

    assert_eq!(state_of(&root, cx), RootStateLabel::Welcome);
    expect_bounds(cx, "app-root-dev-banner");
    expect_bounds(cx, "welcome-new-local");
    expect_bounds(cx, "welcome-open-local");
    expect_bounds(cx, "welcome-sync-remote");
    assert!(
        cx.debug_bounds("app-root-local").is_none(),
        "旧「继续本地」按钮入口必须移除（本地仓库改为左列表 + 右栏操作卡）"
    );
    // A 组第二切片：dev-sync 构建保留旧「继续本地（DEV ONLY）」文案（同一 selector，
    // 产品构建是「仅本地使用」）。
    assert_eq!(
        AppRoot::local_entry_label(),
        ui_text::local_dev_action(),
        "dev-sync 构建必须保留旧「继续本地」开发入口文案"
    );
    assert!(
        AppRoot::local_entry_label().contains("DEV ONLY"),
        "dev 入口必须明确 DEV ONLY：{}",
        AppRoot::local_entry_label()
    );
}

/// 回归：Welcome 的「同步远程仓库」卡片可以用 Tab 从链接切到实例目录，也可以鼠标点击聚焦。
///
/// 之前 `FormInput` 只有 `track_focus`、没有 `tab_stop(true)`，并且向导字段
/// `click_to_focus=false`：键盘 Tab 与鼠标点击都进不了输入框。新的统一表单一屏内
/// 只有链接 + 实例目录（+ dev 的同意勾选），不再有 relay/vault_id/恢复码多字段。
#[gpui::test]
async fn welcome_sync_form_tab_and_click_can_focus_link_and_dir(cx: &mut TestAppContext) {
    let (_dir, layout) = temp_layout("sync-form-focus");
    let options = AppRootOptions::for_test(
        layout.clone(),
        "http://127.0.0.1:18080",
        "inst-sync-form-focus",
    );
    let (root, cx) = open_root(cx, options);
    cx.run_until_parked();
    let card = expect_bounds(cx, "welcome-sync-remote");
    cx.simulate_click(card.center(), Modifiers::default());
    cx.run_until_parked();
    assert_eq!(
        state_of(&root, cx),
        RootStateLabel::Welcome,
        "展开同步卡不得整页跳到旧 CreateForm"
    );
    for selector in [
        "welcome-sync-link",
        "welcome-sync-dir",
        "welcome-sync-dir-browse",
        "welcome-sync-confirm",
        "welcome-sync-consent",
    ] {
        expect_bounds(cx, selector);
    }
    // 统一表单不再有旧的手动字段（欢迎页也没有旧入口）。
    for absent in [
        "app-root-join-relay-input",
        "app-root-vault-id-input",
        "app-root-recovery-input",
        "app-root-invite-input",
        "app-root-create-link-input",
    ] {
        assert!(
            cx.debug_bounds(absent).is_none(),
            "统一远程表单不得出现手动字段/旧入口：{absent}"
        );
    }
    let initial_dir = root
        .update(cx, |root, cx| root.welcome_sync_dir_text(cx))
        .expect("同步表单必须有实例目录");

    // 展开后宿主聚焦链接；输入进链接（目录不变）。
    cx.simulate_input("mn-pair://create-draft");
    cx.run_until_parked();
    assert_eq!(
        root.update(cx, |root, cx| root.welcome_sync_link_is_empty(cx)),
        Some(false),
        "初始焦点必须在同步链接输入框"
    );
    assert_eq!(
        root.update(cx, |root, cx| root.welcome_sync_dir_text(cx)),
        Some(initial_dir.clone()),
        "输入链接时不得写进实例目录"
    );

    // Tab 必须切到实例目录；输入进目录。
    cx.simulate_keystrokes("tab");
    cx.run_until_parked();
    cx.simulate_input("-tab");
    cx.run_until_parked();
    let dir_after_tab = root
        .update(cx, |root, cx| root.welcome_sync_dir_text(cx))
        .expect("同步表单必须有实例目录");
    assert!(
        dir_after_tab.ends_with("-tab"),
        "Tab 后输入必须进入实例目录：{dir_after_tab}"
    );

    // 鼠标点击链接 → 输入进链接；再点实例目录 → 输入进目录。
    let link_bounds = expect_bounds(cx, "welcome-sync-link");
    cx.simulate_click(link_bounds.center(), Modifiers::default());
    cx.run_until_parked();
    cx.simulate_input("-link-click");
    cx.run_until_parked();
    assert_eq!(
        root.update(cx, |root, cx| root.welcome_sync_link_is_empty(cx)),
        Some(false),
        "点击链接后输入必须进入链接（且仍有内容）"
    );
    let dir_after_click_link = root
        .update(cx, |root, cx| root.welcome_sync_dir_text(cx))
        .expect("同步表单必须有实例目录");
    assert_eq!(
        dir_after_click_link, dir_after_tab,
        "点击链接后输入不得写进实例目录"
    );

    let dir_bounds = expect_bounds(cx, "welcome-sync-dir");
    cx.simulate_click(dir_bounds.center(), Modifiers::default());
    cx.run_until_parked();
    cx.simulate_input("-dir-click");
    cx.run_until_parked();
    let dir_after_click_dir = root
        .update(cx, |root, cx| root.welcome_sync_dir_text(cx))
        .expect("同步表单必须有实例目录");
    assert!(
        dir_after_click_dir.ends_with("-dir-click"),
        "点击实例目录后输入必须进入目录：{dir_after_click_dir}"
    );
}

/// 统一远程表单：不认识/坏形式的链接必须内联报错，**不联网、不建文件**。
#[gpui::test]
async fn welcome_sync_rejects_unknown_links_inline_without_creating_files(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let (_dir, layout) = temp_layout("welcome-bad-link");
    let target_dir = layout.instances_dir.join("inst-bad-link");
    let options =
        AppRootOptions::for_test(layout.clone(), "http://127.0.0.1:18080", "inst-bad-link");
    let (root, cx) = open_root(cx, options);
    cx.run_until_parked();

    let card = expect_bounds(cx, "welcome-sync-remote");
    cx.simulate_click(card.center(), Modifiers::default());
    cx.run_until_parked();
    root.update(cx, |root, cx| {
        root.welcome_set_sync_link("https://example.com/not-a-pair-link", cx);
        root.welcome_set_sync_dir(&target_dir.display().to_string(), cx);
        root.welcome_set_sync_consent(true, cx);
        root.welcome_confirm_sync(cx);
    });
    cx.run_until_parked();
    assert_eq!(
        state_of(&root, cx),
        RootStateLabel::Welcome,
        "无法识别的链接必须留在 Welcome（不进入 Working）"
    );
    let error = root
        .update(cx, |root, _| root.welcome_error().map(str::to_owned))
        .expect("必须内联报错");
    assert!(error.contains("无法识别"), "错误必须点明链接形式：{error}");
    assert!(!target_dir.exists(), "链接解析失败不得创建目录/库");
}

/// 验收 2：真实 Create 全链路 + 关窗拦截 + 备份确认 + 重启自动进 Main。
#[gpui::test]
async fn create_flow_backs_up_code_then_opens_empty_main_and_restarts(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let (_dir, layout) = temp_layout("create");
    let instance_id = "inst-create-e2e";
    let options = AppRootOptions {
        layout: layout.clone(),
        default_relay: relay.base_url().to_owned(),
        instance_id: Some(instance_id.to_owned()),
        allow_local_vault: false,
        operation_budget: DEFAULT_OPERATION_BUDGET,
    };
    let (root, cx) = open_root(cx, options.clone());
    cx.run_until_parked();
    assert_eq!(state_of(&root, cx), RootStateLabel::Welcome);

    // ---- 真实点击「同步远程仓库」→ 同一张卡展开统一表单（不整页跳走）----
    let card = expect_bounds(cx, "welcome-sync-remote");
    cx.simulate_click(card.center(), Modifiers::default());
    cx.run_until_parked();
    assert_eq!(
        state_of(&root, cx),
        RootStateLabel::Welcome,
        "展开同步卡必须仍在 Welcome（旧的 CreateForm 入口已移除）"
    );
    for selector in [
        "welcome-sync-link",
        "welcome-sync-dir",
        "welcome-sync-dir-browse",
        "welcome-sync-confirm",
        "welcome-sync-consent",
    ] {
        expect_bounds(cx, selector);
    }
    // 统一表单不再显示任何手动 relay / vault_id / 恢复码 / 邀请链接字段。
    for selector in [
        "app-root-create-form",
        "app-root-join-form",
        "app-root-create-link-input",
        "app-root-join-relay-input",
        "app-root-vault-id-input",
        "app-root-recovery-input",
        "app-root-invite-input",
    ] {
        assert!(
            cx.debug_bounds(selector).is_none(),
            "统一远程表单不得显示旧入口/手动字段：{selector}"
        );
    }

    // ---- 粘贴一张真实创建链接（v2 一次性票据）----
    let vault_id = [0x5Bu8; 16];
    let link = create_link_for(&relay, vault_id, 900);
    let instance_dir = layout
        .instance_dir(instance_id)
        .expect("合法实例 id")
        .display()
        .to_string();
    root.update(cx, |root, cx| {
        root.welcome_set_sync_link(&link, cx);
        root.welcome_set_sync_dir(&instance_dir, cx);
    });

    // ---- 未勾选同意：提交被拒（留在 Welcome + 内联错误），不触发任何网络 ----
    root.update(cx, |root, cx| root.welcome_confirm_sync(cx));
    cx.run_until_parked();
    assert_eq!(state_of(&root, cx), RootStateLabel::Welcome);
    assert!(
        cx.debug_bounds("welcome-error").is_some(),
        "未勾选同意必须在卡片里给出内联错误，而不是悄悄开始创建"
    );

    // ---- 勾选同意 → 提交 → Working（后台线程；UI 不阻塞） ----
    root.update(cx, |root, cx| {
        root.welcome_set_sync_consent(true, cx);
        root.welcome_confirm_sync(cx);
    });
    cx.run_until_parked();
    assert_eq!(state_of(&root, cx), RootStateLabel::Working);

    expect_bounds(cx, "app-root-working");
    expect_bounds(cx, "app-root-progress");
    expect_bounds(cx, "app-root-cancel");

    // ---- 关窗在 Working 必须被拦截（迟到的成功不能丢恢复码） ----
    assert!(
        !cx.simulate_close(),
        "Working 状态的关窗必须被拦截（不能丢恢复码）"
    );
    cx.run_until_parked();
    assert_eq!(state_of(&root, cx), RootStateLabel::Working);

    // ---- 请求取消：只停止自动继续，worker 不中断；迟到的成功仍必须进备份页 ----
    root.update(cx, |root, cx| root.cancel_create(cx));
    let cancel_notice = root
        .update(cx, |root, _| root.notice().map(str::to_owned))
        .expect("请求取消必须给提示");
    assert!(
        cancel_notice.contains("真实收尾"),
        "取消提示必须说明等真实收尾：{cancel_notice}"
    );

    // ---- 显式收敛点：阻塞等 Create 终态（不 sleep；有超时上限） ----
    let finished = root.update(cx, |root, cx| {
        root.wait_for_create_terminal(cx, DEFAULT_OPERATION_BUDGET)
    });
    cx.run_until_parked();
    assert!(finished, "Create 必须在 120s 预算内到达终态");
    assert_eq!(
        state_of(&root, cx),
        RootStateLabel::BackupRequired,
        "Create 成功且身份落盘后必须停在备份页（不能直接进 Main）"
    );
    for selector in [
        "app-root-backup",
        "app-root-vault-id",
        "app-root-recovery-code",
        "app-root-copy-code",
        "app-root-backup-confirm",
        "app-root-continue",
    ] {
        expect_bounds(cx, selector);
    }
    // 取消请求没有吞掉迟到的成功：仍然停在备份页（恢复码不能丢）。
    let after_cancel_notice = root
        .update(cx, |root, _| root.notice().map(str::to_owned))
        .unwrap_or_default();
    assert!(
        after_cancel_notice.contains("已请求取消"),
        "取消后迟到的成功必须说明「已请求取消但已成功」：{after_cancel_notice:?}"
    );
    let vault_id = root
        .update(cx, |root, _| root.vault_id().map(str::to_owned))
        .expect("备份页必须有 vault_id");
    let recovery_code = root
        .update(cx, |root, _| root.recovery_code().map(str::to_owned))
        .expect("备份页必须有恢复码");
    assert_eq!(vault_id.len(), 32, "vault_id 是 32 位十六进制：{vault_id}");
    assert!(
        recovery_code.starts_with("mnrec1"),
        "恢复码格式不对：{recovery_code}"
    );
    // 备份确认前**不登记**配置（写失败/取消都不该留下 active 指针）。
    assert!(!layout.config_path.exists(), "备份确认之前不得写公开配置");

    // ---- 勾选「已备份」→ 继续 → Main（真实空树） ----
    root.update(cx, |root, cx| {
        root.set_backup_confirmed(true, cx);
        root.confirm_backup(cx);
    });
    cx.run_until_parked();
    assert_eq!(state_of(&root, cx), RootStateLabel::Main);
    expect_bounds(cx, "app-root-main");
    // Main 是真实 AppShell：空树仍然有侧栏「＋ 笔记」，但没有 tree-row。
    expect_bounds(cx, "sidebar-new-note");
    assert!(
        cx.debug_bounds("tree-row-0").is_none(),
        "Create 之后必须是真实空树（不得播种 demo）"
    );
    assert!(
        root.update(cx, |root, _| root.recovery_code().is_none()),
        "进入 Main 后恢复码必须离开应用状态"
    );

    // ---- 公开配置：原子写入且 active 指向该实例；不得出现任何秘密 ----
    let raw = std::fs::read_to_string(&layout.config_path).expect("Main 之后必须有公开配置");
    let profile = layout
        .store()
        .load()
        .expect("配置必须是合法 JSON")
        .expect("Main 之后必须有公开配置");
    assert_eq!(profile.active_instance.as_deref(), Some(instance_id));
    let entry = profile.find(instance_id).expect("配置里必须有该实例条目");
    assert_eq!(entry.vault_id, vault_id);
    assert_eq!(entry.device_id.len(), 32);
    assert_eq!(entry.relay_url, relay.base_url());
    assert_eq!(entry.vault_path, format!("{instance_dir}/vault.sqlite3"));
    for forbidden in ["mnrec1", "recovery", "secret", "manager", "private"] {
        assert!(
            !raw.to_ascii_lowercase().contains(forbidden),
            "公开配置不得出现 {forbidden}：{raw}"
        );
    }
    assert!(
        relay.setup_secret().is_empty() || !raw.contains(relay.setup_secret()),
        "公开配置不得出现 setup secret"
    );

    // ---- 模拟重启：关窗（Main 有界排空 → 释放实例锁）→ 新 AppRoot 自动进 Main ----
    assert!(cx.simulate_close(), "Main 状态允许关窗（有界排空）");
    cx.run_until_parked();
    assert!(
        !layout
            .instance_dir(instance_id)
            .expect("合法 id")
            .join(menghunote_app::onboarding::LOCK_FILE_NAME)
            .exists(),
        "正常关窗必须释放实例锁（否则重启会被自己挡住）"
    );
    let restarted = cx.update(|window, cx| {
        let options = options.clone();
        window.replace_root(cx, move |_window, cx| AppRoot::new(options, cx))
    });
    cx.run_until_parked();
    assert_eq!(
        state_of(&restarted, cx),
        RootStateLabel::Main,
        "已有有效配置 + 身份时，新 AppRoot 必须自动进 Main（不闪 Welcome）"
    );
    expect_bounds(cx, "app-root-main");
    let active = restarted
        .update(cx, |root, _| root.active_entry().cloned())
        .expect("自动打开的实例必须有公开条目");
    assert_eq!(active.instance_id, instance_id);
    assert_eq!(active.vault_id, vault_id);
}

/// P1 本机启动偏好（同步侧）：同步实例打开成功 → `<数据根>/startup.json` 记
/// `{"version":1,"mode":"sync"}` → 重建 `AppRoot` 仍自动进同一实例 Main（不闪 Welcome）。
#[gpui::test]
async fn sync_instance_open_is_remembered_and_resumes_after_restart(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let (_dir, layout) = temp_layout("startup-sync");
    let instance_id = "inst-startup-sync";
    let created = create_instance(&relay, &layout, instance_id);
    write_profile(&layout, &created, instance_id);
    let options = AppRootOptions::for_test(layout.clone(), relay.base_url(), instance_id);
    let (root, cx) = open_root(cx, options.clone());
    cx.run_until_parked();
    assert_eq!(state_of(&root, cx), RootStateLabel::Main);

    let startup_path = layout.startup_path();
    assert!(
        startup_path.is_file(),
        "同步实例打开成功必须写本机启动偏好：{}",
        startup_path.display()
    );
    let value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&startup_path).expect("读启动偏好失败"))
            .expect("启动偏好必须是 JSON");
    assert_eq!(value["version"], serde_json::json!(1));
    assert_eq!(
        value["mode"], "sync",
        "同步实例打开成功必须记 mode=sync（不是 local）"
    );
    // 启动偏好是**本机**文件：不得写进 sync.json / 同步配置。
    let profile_raw = std::fs::read_to_string(&layout.config_path).expect("读公开配置失败");
    let profile_json: serde_json::Value =
        serde_json::from_str(&profile_raw).expect("公开配置必须是 JSON");
    assert!(
        profile_json.get("mode").is_none() && profile_json.get("section").is_none(),
        "启动/设置偏好不得混进 sync.json：{profile_raw}"
    );

    // 重启：不点任何入口，直接回到同一同步实例 Main。
    assert!(cx.simulate_close(), "Main 状态允许关窗（有界排空）");
    cx.run_until_parked();
    let restarted = cx.update(|window, cx| {
        let options = options.clone();
        window.replace_root(cx, move |_window, cx| AppRoot::new(options, cx))
    });
    cx.run_until_parked();
    assert_eq!(
        state_of(&restarted, cx),
        RootStateLabel::Main,
        "偏好 sync + 有效 active：重启必须直接进同步 Main"
    );
    assert!(
        !restarted.update(cx, |root, _| root.is_local_mode()),
        "同步实例的 Main 不得被标成本地（不同步）模式"
    );
    let active = restarted
        .update(cx, |root, _| root.active_entry().cloned())
        .expect("自动打开的实例必须有公开条目");
    assert_eq!(active.instance_id, instance_id);
    assert_eq!(active.vault_id, created.vault_id);
}

/// 验收 3：配置损坏 / 身份缺失 / 库与配置错配 → Error，且不创建新库。
#[gpui::test]
async fn broken_config_or_identity_lands_on_error_without_rebuilding(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let (_dir, layout) = temp_layout("broken");

    // ---- 3a. 配置 JSON 损坏 ----
    // 单 vault dev relay 一次只能 genesis 一个 vault，每个夹具实例各起一个 relay。
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    std::fs::create_dir_all(layout.config_path.parent().expect("配置有父目录"))
        .expect("创建配置目录失败");
    std::fs::write(&layout.config_path, b"{ this is not json").expect("写坏配置失败");
    let options = AppRootOptions::for_test(layout.clone(), relay.base_url(), "inst-broken-config");
    let (root, cx) = open_root(cx, options);
    cx.run_until_parked();
    assert_eq!(state_of(&root, cx), RootStateLabel::Error);
    expect_bounds(cx, "app-root-error");
    expect_bounds(cx, "app-root-error-message");
    expect_bounds(cx, "app-root-retry");
    let message = root
        .update(cx, |root, _| root.error_message().map(str::to_owned))
        .expect("Error 页必须有文案");
    assert!(
        message.contains("损坏"),
        "配置损坏必须明确说清楚（且不自动重建）：{message}"
    );
    assert!(
        !layout.instances_dir.exists()
            || std::fs::read_dir(&layout.instances_dir)
                .map_or(true, |mut dir| dir.next().is_none()),
        "Error 页不得凭空创建实例目录内容"
    );

    // ---- 3b. 身份缺失 ----
    // 用真实 Create 造一个实例（生产路径；测试只关心引导页如何拒绝）。
    let relay_b = DevRelay::start().expect("起进程内 dev relay 失败");
    let created = create_instance(&relay_b, &layout, "inst-missing-identity");
    std::fs::remove_file(menghunote_app::dev_sync::DevIdentityStore::path_for(
        &created.vault_path,
    ))
    .expect("删除身份文件失败");
    write_profile(&layout, &created, "inst-missing-identity");
    let options =
        AppRootOptions::for_test(layout.clone(), relay_b.base_url(), "inst-missing-identity");
    let (root, cx) = open_root(cx, options);
    cx.run_until_parked();
    assert_eq!(state_of(&root, cx), RootStateLabel::Error);
    let message = root
        .update(cx, |root, _| root.error_message().map(str::to_owned))
        .expect("必须显示错误");
    assert!(
        message.contains("身份"),
        "身份缺失必须明确指出（不自动重建）：{message}"
    );
    assert!(
        !menghunote_app::dev_sync::DevIdentityStore::path_for(&created.vault_path).exists(),
        "加载失败绝不能顺手补建身份文件"
    );

    // ---- 3c. 库与配置错配（vault_id 不一致）→ Error，不覆盖库/身份 ----
    let relay_c = DevRelay::start().expect("起进程内 dev relay 失败");
    let created = create_instance(&relay_c, &layout, "inst-mismatch");
    let mut tampered = created.clone();
    tampered.vault_id = "0".repeat(32);
    write_profile(&layout, &tampered, "inst-mismatch");
    let options = AppRootOptions::for_test(layout.clone(), relay_c.base_url(), "inst-mismatch");
    let (root, cx) = open_root(cx, options);
    cx.run_until_parked();
    assert_eq!(state_of(&root, cx), RootStateLabel::Error);
    let message = root
        .update(cx, |root, _| root.error_message().map(str::to_owned))
        .expect("必须显示错误");
    assert!(
        message.contains("不匹配"),
        "库/配置错配必须明确指出：{message}"
    );
    assert!(created.vault_path.is_file(), "错配不得删除用户库");
    assert!(
        menghunote_app::dev_sync::DevIdentityStore::path_for(&created.vault_path).is_file(),
        "错配不得删除用户身份"
    );
}

/// 一个已创建实例的公开事实（测试夹具）。
#[derive(Clone)]
struct CreatedInstance {
    instance_id: String,
    vault_path: PathBuf,
    vault_id: String,
    device_id: String,
    relay_url: String,
}

/// 走真实 `DevLocal::create` + 身份落盘（与向导 worker 同一条库路径）。
fn create_instance(relay: &DevRelay, layout: &ProfileLayout, instance_id: &str) -> CreatedInstance {
    let vault_path = layout.vault_path(instance_id).expect("合法实例 id");
    let created = menghunote_sync::dev_local::DevLocal::create(
        relay.base_url(),
        relay.setup_secret(),
        &vault_path,
        "onboarding-ui 夹具",
    )
    .expect("夹具 Create 失败");
    menghunote_app::dev_sync::DevIdentityStore
        .persist_protected(&vault_path, &created.identity)
        .expect("夹具身份落盘失败");
    CreatedInstance {
        instance_id: instance_id.to_owned(),
        vault_path,
        vault_id: menghunote_sync::ids::hex_of(&created.vault_id),
        device_id: menghunote_sync::ids::hex_of(&created.identity.device_id),
        relay_url: relay.base_url().to_owned(),
    }
}

/// 写一份指向夹具实例的公开配置（`active_instance` = 它）。
fn write_profile(layout: &ProfileLayout, created: &CreatedInstance, active: &str) {
    let store: ProfileStore = layout.store();
    let mut profile = DevSyncProfile::default();
    profile.upsert_active(InstanceEntry {
        instance_id: created.instance_id.clone(),
        display_name: None,
        relay_mode: RelayMode::LocalDev,
        api_version: RelayApiVersion::V1,
        relay_url: created.relay_url.clone(),
        vault_path: created.vault_path.to_string_lossy().into_owned(),
        vault_id: created.vault_id.clone(),
        device_id: created.device_id.clone(),
    });
    profile.active_instance = Some(active.to_owned());
    store.save(&profile).expect("写测试配置失败");
}

/// 配置保存失败 → Error（可重试保存），恢复权限后重试进入 Main，**不重跑 Create**。
#[cfg(unix)]
#[gpui::test]
async fn profile_save_failure_is_retried_without_recreating(cx: &mut TestAppContext) {
    use std::os::unix::fs::PermissionsExt as _;

    ensure_test_user_key();
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let (_dir, layout) = temp_layout("save-fail");
    let instance_id = "inst-save-fail";
    let options = AppRootOptions {
        layout: layout.clone(),
        default_relay: relay.base_url().to_owned(),
        instance_id: Some(instance_id.to_owned()),
        allow_local_vault: false,
        operation_budget: DEFAULT_OPERATION_BUDGET,
    };
    let (root, cx) = open_root(cx, options);
    cx.run_until_parked();
    let instance_dir = layout
        .instance_dir(instance_id)
        .expect("合法实例 id")
        .display()
        .to_string();
    let link = create_link_for(&relay, [0x5Cu8; 16], 900);
    apply_create_link(&root, cx, &link);
    root.update(cx, |root, cx| {
        root.form_set_dir(&instance_dir, cx);
        root.form_set_consent(true, cx);
        root.submit_create(cx);
    });
    cx.run_until_parked();
    let finished = root.update(cx, |root, cx| {
        root.wait_for_create_terminal(cx, DEFAULT_OPERATION_BUDGET)
    });
    cx.run_until_parked();
    assert!(finished, "Create 必须到达终态");
    assert_eq!(state_of(&root, cx), RootStateLabel::BackupRequired);
    let vault_id_before = root
        .update(cx, |root, _| root.vault_id().map(str::to_owned))
        .expect("备份页必须有 vault_id");

    // 让配置写失败：父目录只读（Unix 权限；Windows 用 ACL，本测试只在 Unix 跑）。
    let config_dir = layout
        .config_path
        .parent()
        .expect("配置有父目录")
        .to_path_buf();
    std::fs::create_dir_all(&config_dir).expect("创建配置目录失败");
    std::fs::set_permissions(&config_dir, std::fs::Permissions::from_mode(0o500))
        .expect("设置只读失败");
    root.update(cx, |root, cx| {
        root.set_backup_confirmed(true, cx);
        root.confirm_backup(cx);
    });
    cx.run_until_parked();
    // 立刻恢复权限：后续断言/临时目录清理不再依赖只读目录。
    std::fs::set_permissions(&config_dir, std::fs::Permissions::from_mode(0o700))
        .expect("恢复权限失败");
    assert_eq!(
        state_of(&root, cx),
        RootStateLabel::Error,
        "配置写失败必须停在 Error（不能假装进 Main）"
    );
    let message = root
        .update(cx, |root, _| root.error_message().map(str::to_owned))
        .expect("Error 页必须有文案");
    assert!(
        message.contains("配置保存失败") && message.contains("重试保存"),
        "错误必须说清「重试保存、不重跑 Create」：{message}"
    );
    assert!(!layout.config_path.exists(), "写失败不得留下半份配置");

    // 重试保存 → Main；vault_id 不变 = 没有重跑 Create。
    root.update(cx, |root, cx| root.retry(cx));
    cx.run_until_parked();
    assert_eq!(state_of(&root, cx), RootStateLabel::Main);
    expect_bounds(cx, "app-root-main");
    let profile = layout
        .store()
        .load()
        .expect("读配置失败")
        .expect("重试保存后必须有配置");
    assert_eq!(profile.active_instance.as_deref(), Some(instance_id));
    assert_eq!(
        profile.find(instance_id).expect("实例条目").vault_id,
        vault_id_before,
        "重试只重写配置：实例 vault_id 不得变化（说明没有重跑 Create）"
    );
}

/// 实例目录被另一个进程占用：按 ADR 回到 Welcome（实例选择页）而不是 Error；
/// 释放锁后再点同一实例可以正常进 Main。
#[gpui::test]
async fn instance_lock_conflict_returns_to_welcome_then_opens_after_release(
    cx: &mut TestAppContext,
) {
    ensure_test_user_key();
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let (_dir, layout) = temp_layout("locked");
    let created = create_instance(&relay, &layout, "inst-locked");
    write_profile(&layout, &created, "inst-locked");
    let instance_dir = created
        .vault_path
        .parent()
        .expect("fixture 库有父目录")
        .to_path_buf();
    // 模拟"另一个进程"：先占住实例目录锁。
    let held = InstanceLock::acquire(&instance_dir).expect("测试占锁失败");
    let options = AppRootOptions::for_test(layout.clone(), relay.base_url(), "inst-locked");
    let (root, cx) = open_root(cx, options);
    cx.run_until_parked();
    assert_eq!(
        state_of(&root, cx),
        RootStateLabel::Welcome,
        "目录占用不是实例损坏：必须回到实例选择页（Welcome）"
    );
    assert!(cx.debug_bounds("app-root-error").is_none());
    let notice = root
        .update(cx, |root, _| root.notice().map(str::to_owned))
        .expect("占锁必须给明确原因");
    assert!(
        notice.contains("另一个进程"),
        "占锁提示必须可诊断：{notice}"
    );
    // 释放锁（另一个进程退出）→ 再点同一实例 → Main。
    drop(held);
    root.update(cx, |root, cx| root.open_saved_instance("inst-locked", cx));
    cx.run_until_parked();
    assert_eq!(state_of(&root, cx), RootStateLabel::Main);
    expect_bounds(cx, "app-root-main");
}

/// 保证测试的 `Duration` 预算与 ADR 一致（避免有人把它悄悄改短）。
#[test]
fn operation_budget_matches_adr() {
    assert_eq!(DEFAULT_OPERATION_BUDGET, Duration::from_secs(120));
}

// ---------------------------------------------------------------------------
// S3：Join 向导（真实 A→发布基线→B 只凭 vault_id + 恢复码加入）
// ---------------------------------------------------------------------------

/// A 侧实例：公开事实 + 仍然打开的库/身份 + 一次性恢复码（测试夹具）。
///
/// 用真实库函数建（ADR 0011 S3 允许"A 用 Create 向导或库函数"），这样 B 侧才能
/// 在同一个测试里读到真正的正文/基线，而不是 mock。
struct Creator {
    facts: CreatedInstance,
    recovery_code: String,
    vault: Vault,
    identity: LocalIdentityV1,
}

/// 走真实 `DevLocal::create` + 身份落盘，并把库/身份留在夹具里（不立即丢弃）。
fn create_creator(relay: &DevRelay, layout: &ProfileLayout, instance_id: &str) -> Creator {
    let vault_path = layout.vault_path(instance_id).expect("合法实例 id");
    let created = DevLocal::create(
        relay.base_url(),
        relay.setup_secret(),
        &vault_path,
        "onboarding-ui Join 夹具",
    )
    .expect("Creator Create 失败");
    DevIdentityStore
        .persist_protected(&vault_path, &created.identity)
        .expect("Creator 身份落盘失败");
    Creator {
        facts: CreatedInstance {
            instance_id: instance_id.to_owned(),
            vault_path,
            vault_id: menghunote_sync::ids::hex_of(&created.vault_id),
            device_id: menghunote_sync::ids::hex_of(&created.identity.device_id),
            relay_url: relay.base_url().to_owned(),
        },
        recovery_code: created.recovery_code.clone(),
        identity: created.identity,
        vault: created.vault,
    }
}

/// 用真实 storage 路径在 A 的库里建一篇笔记（结构 CRDT + 正文 + 同事务 sync intent）。
fn seed_creator_note(creator: &Creator, title: &str, text: &str) -> String {
    let peer = creator.identity.loro_peer_id();
    let epoch = creator.identity.current_epoch;
    let mut writer =
        StructureWriter::open_with_sync(&creator.vault, peer, Some(SyncContext::new(peer, epoch)))
            .expect("打开结构写入口失败");
    let seed = writer
        .doc()
        .create_note(title, 1)
        .expect("结构 CRDT 新建笔记失败");
    writer
        .commit("onboarding-ui-join-seed")
        .expect("结构提交失败");

    let (doc, head) =
        load_body_doc(&creator.vault, &seed.body_doc_id, peer, None).expect("加载正文文档失败");
    doc.insert_scalars(0, text).expect("写入正文失败");
    let base = head.version_vector().expect("解码 head VV 失败");
    let update = doc.pending_update(&base).expect("导出正文增量失败");
    let seq = head.head_seq.saturating_add(1);
    let intent = local_sync_intent(
        &seed.body_doc_id,
        seq,
        "onboarding-ui-join-body",
        SyncContext::new(peer, epoch),
    )
    .expect("派生 sync intent 失败");
    append_updates_with_sync_intents(
        &creator.vault,
        &seed.body_doc_id,
        head.head_seq,
        &[UpdateRecord::new(intent.batch_id.clone(), update)],
        &[intent],
    )
    .expect("正文 + intent 事务失败");
    seed.body_doc_id
}

/// A 发布初始基线（两份 checkpoint；B 的 bootstrap 需要它）。
fn publish_creator_baseline(relay: &DevRelay, creator: &Creator) {
    let report = DevLocal::publish_initial_pair(
        relay.base_url(),
        &creator.vault,
        &creator.identity,
        &creator.recovery_code,
    )
    .expect("A 发布初始基线失败");
    assert!(report.validated, "初始基线必须真实读回/导入通过");
}

/// 打开 JoinForm 并填入公开字段（恢复码由调用方按用例设置）。
fn open_join_form(
    root: &Entity<AppRoot>,
    cx: &mut VisualTestContext,
    relay_url: &str,
    vault_id: &str,
    dir: &str,
) {
    let relay_url = relay_url.to_owned();
    let vault_id = vault_id.to_owned();
    let dir = dir.to_owned();
    root.update(cx, |root, cx| {
        root.enter_join_form(cx);
        root.join_form_set_relay(&relay_url, cx);
        root.join_form_set_vault_id(&vault_id, cx);
        root.join_form_set_dir(&dir, cx);
    });
    cx.run_until_parked();
}

/// 验收 1（S3 主链路）：Join → Working（关窗拦截 + 取消请求）→ 迟到成功进 Main，
/// 自动打开 A 的笔记、写配置、重启直接 Main。
#[gpui::test]
async fn join_flow_opens_creator_note_writes_config_and_restarts(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let (_dir, layout) = temp_layout("join-flow");
    let creator = create_creator(&relay, &layout, "inst-join-a");
    seed_creator_note(&creator, "A 的笔记", "A 写的第一行\n");
    publish_creator_baseline(&relay, &creator);

    let instance_id = "inst-join-b";
    let options = AppRootOptions {
        layout: layout.clone(),
        default_relay: relay.base_url().to_owned(),
        instance_id: Some(instance_id.to_owned()),
        allow_local_vault: false,
        operation_budget: DEFAULT_OPERATION_BUDGET,
    };
    let (root, cx) = open_root(cx, options.clone());
    cx.run_until_parked();
    assert_eq!(state_of(&root, cx), RootStateLabel::Welcome);

    // ---- 旧 JoinForm 仍保留为内部/恢复码路径（欢迎页已不再暴露手动表单）----
    root.update(cx, |root, cx| root.enter_join_form(cx));
    cx.run_until_parked();
    assert_eq!(
        state_of(&root, cx),
        RootStateLabel::JoinForm,
        "旧 JoinForm 必须仍可进入（内部/测试路径，供恢复码加入使用）"
    );
    for selector in [
        "app-root-join-form",
        "app-root-join-relay-input",
        "app-root-join-relay-hint",
        "app-root-vault-id-input",
        "app-root-recovery-input",
        "app-root-join-dir-input",
        "app-root-join-submit",
        "app-root-join-back",
    ] {
        expect_bounds(cx, selector);
    }
    // 模式按 relay 地址自动推断：Join 不再渲染任何模式单选项。
    for absent in [
        "app-root-join-mode-local_dev",
        "app-root-join-mode-remote_tls",
        "app-root-join-mode",
    ] {
        assert!(
            cx.debug_bounds(absent).is_none(),
            "Join 表单不得再有模式单选控件：{absent}"
        );
    }

    let target_dir = layout
        .instance_dir(instance_id)
        .expect("合法实例 id")
        .display()
        .to_string();
    open_join_form(
        &root,
        cx,
        relay.base_url(),
        &creator.facts.vault_id,
        &target_dir,
    );
    root.update(cx, |root, cx| {
        root.join_form_set_recovery_code(&creator.recovery_code, cx);
        root.submit_join(cx);
    });
    cx.run_until_parked();
    assert_eq!(state_of(&root, cx), RootStateLabel::Working);
    assert!(
        root.update(cx, |root, _| root.working_is_join()),
        "Working 必须来自 Join（不是 Create）"
    );
    expect_bounds(cx, "app-root-working");
    expect_bounds(cx, "app-root-progress");
    expect_bounds(cx, "app-root-cancel");

    // Working 中拒绝重复提交（不会开第二个 worker/在 relay 上再造设备）。
    root.update(cx, |root, cx| root.submit_join(cx));
    cx.run_until_parked();
    assert_eq!(state_of(&root, cx), RootStateLabel::Working);

    // 关窗必须被拦截：迟到的成功不能丢。
    assert!(
        !cx.simulate_close(),
        "Join 的 Working 关窗必须被拦截（成功必须进 Main 并写配置）"
    );
    cx.run_until_parked();
    assert_eq!(state_of(&root, cx), RootStateLabel::Working);

    // 请求取消：只停止"自动继续"，worker 不中断；迟到的成功仍必须写配置并进 Main。
    root.update(cx, |root, cx| root.cancel_join(cx));
    let cancel_notice = root
        .update(cx, |root, _| root.notice().map(str::to_owned))
        .expect("请求取消必须给提示");
    assert!(
        cancel_notice.contains("真实收尾"),
        "取消提示必须说明等真实收尾：{cancel_notice}"
    );

    // 显式收敛点：阻塞等 Join 终态（不 sleep；有超时上限）。
    let finished = root.update(cx, |root, cx| {
        root.wait_for_join_terminal(cx, DEFAULT_OPERATION_BUDGET)
    });
    cx.run_until_parked();
    assert!(finished, "Join 必须在操作预算内到达终态");
    assert_eq!(
        state_of(&root, cx),
        RootStateLabel::Main,
        "Join 成功必须直接进 Main（没有恢复码备份页）"
    );
    expect_bounds(cx, "app-root-main");
    expect_bounds(cx, "sidebar-new-note");
    let after_cancel_notice = root
        .update(cx, |root, _| root.notice().map(str::to_owned))
        .unwrap_or_default();
    assert!(
        after_cancel_notice.contains("已请求取消"),
        "取消后迟到的成功必须说明「已请求取消但已成功」：{after_cancel_notice:?}"
    );

    // ---- 自动打开 A 的笔记，正文正确 ----
    let shell = root
        .update(cx, |root, _| root.main_shell().cloned())
        .expect("Main 必须有真实 AppShell");
    let text = cx
        .update(|_window, app| shell.read(app).dev_sync_current_text(app))
        .expect("Join 之后 AppShell 必须自动打开第一篇");
    assert!(
        text.contains("A 写的第一行"),
        "B 打开的正文必须来自 A 的基线，实际 {text:?}"
    );

    // ---- 不播种 demo：真实树里只有 A 的一篇 ----
    let b_vault_path = layout.vault_path(instance_id).expect("合法实例 id");
    let reopened = Vault::open(&b_vault_path).expect("B 的库必须可重开");
    let tree = list_tree(&reopened).expect("读取 B 的结构投影失败");
    assert_eq!(
        tree.len(),
        1,
        "Join 不得播种演示树（只应有 A 的那一篇）：{tree:?}"
    );
    assert_eq!(tree[0].title, "A 的笔记");
    drop(reopened);

    // ---- 公开配置：active + 公开事实；不得出现任何秘密 ----
    let raw = std::fs::read_to_string(&layout.config_path).expect("Join 成功必须写公开配置");
    let profile = layout
        .store()
        .load()
        .expect("配置必须是合法 JSON")
        .expect("Join 成功必须写公开配置");
    assert_eq!(profile.active_instance.as_deref(), Some(instance_id));
    let entry = profile.find(instance_id).expect("配置里必须有该实例条目");
    assert_eq!(entry.vault_id, creator.facts.vault_id);
    assert_eq!(entry.device_id.len(), 32);
    assert_eq!(entry.relay_url, relay.base_url());
    assert_eq!(entry.vault_path, format!("{target_dir}/vault.sqlite3"));
    assert_eq!(
        profile.instances.len(),
        1,
        "重复提交不得登记第二个实例：{profile:?}"
    );
    for forbidden in ["mnrec1", "recovery", "secret", "manager", "private"] {
        assert!(
            !raw.to_ascii_lowercase().contains(forbidden),
            "公开配置不得出现 {forbidden}：{raw}"
        );
    }

    // ---- 模拟重启：关窗（Main 有界排空 → 释放实例锁）→ 新 AppRoot 自动进 Main ----
    assert!(cx.simulate_close(), "Main 状态允许关窗（有界排空）");
    cx.run_until_parked();
    assert!(
        !layout
            .instance_dir(instance_id)
            .expect("合法 id")
            .join(LOCK_FILE_NAME)
            .exists(),
        "正常关窗必须释放实例锁（否则重启会被自己挡住）"
    );
    let restarted = cx.update(|window, cx| {
        let options = options.clone();
        window.replace_root(cx, move |_window, cx| AppRoot::new(options, cx))
    });
    cx.run_until_parked();
    assert_eq!(
        state_of(&restarted, cx),
        RootStateLabel::Main,
        "已有有效配置 + 身份时，新 AppRoot 必须自动进 Main（不闪 Welcome）"
    );
    expect_bounds(cx, "app-root-main");
    let active = restarted
        .update(cx, |root, _| root.active_entry().cloned())
        .expect("自动打开的实例必须有公开条目");
    assert_eq!(active.instance_id, instance_id);
    assert_eq!(active.vault_id, creator.facts.vault_id);
}

/// 验收 2（S3 失败恢复）：错恢复码（格式非法 → 内容错误）保留表单与公开字段、
/// 脱敏错误、可改后重试成功；任何一次失败都不得留下库/身份/配置。
#[gpui::test]
async fn wrong_recovery_code_keeps_fields_and_retries_successfully(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let (_dir, layout) = temp_layout("join-retry");
    let creator = create_creator(&relay, &layout, "inst-retry-a");
    seed_creator_note(&creator, "重试后的笔记", "重试成功正文\n");
    publish_creator_baseline(&relay, &creator);

    let instance_id = "inst-retry-b";
    let options =
        AppRootOptions::for_test(layout.clone(), relay.base_url(), "unused-create-instance");
    let options = AppRootOptions {
        instance_id: Some(instance_id.to_owned()),
        ..options
    };
    let (root, cx) = open_root(cx, options.clone());
    let target_dir = layout
        .instance_dir(instance_id)
        .expect("合法实例 id")
        .display()
        .to_string();
    open_join_form(
        &root,
        cx,
        relay.base_url(),
        &creator.facts.vault_id,
        &target_dir,
    );
    let b_vault_path = layout.vault_path(instance_id).expect("合法实例 id");
    let b_identity_path = DevIdentityStore::path_for(&b_vault_path);

    // ---- (a) 格式非法恢复码：联网之前就被拒 ----
    let malformed = "mnrec1not-a-code";
    root.update(cx, |root, cx| {
        root.join_form_set_recovery_code(malformed, cx);
        root.submit_join(cx);
    });
    cx.run_until_parked();
    let finished = root.update(cx, |root, cx| {
        root.wait_for_join_terminal(cx, DEFAULT_OPERATION_BUDGET)
    });
    cx.run_until_parked();
    assert!(finished, "非法恢复码必须在预算内到达终态");
    assert_eq!(
        state_of(&root, cx),
        RootStateLabel::JoinForm,
        "失败必须回到表单（不是 Error 页、更不是 Main）"
    );
    let error = root
        .update(cx, |root, _| root.join_form_error().map(str::to_owned))
        .expect("表单必须显示脱敏错误");
    assert!(
        error.contains("恢复码格式非法"),
        "必须明确说格式非法：{error}"
    );
    assert!(!error.contains(malformed), "不得回显恢复码：{error}");
    assert!(!error.contains("not-a-code"), "连片段都不能出现：{error}");
    // 公开字段全部保留；恢复码已从输入框清空（需重输）。
    assert_eq!(
        root.update(cx, |root, cx| root.join_form_relay(cx)),
        Some(relay.base_url().to_owned())
    );
    assert_eq!(
        root.update(cx, |root, cx| root.join_form_vault_id(cx)),
        Some(creator.facts.vault_id.clone())
    );
    assert_eq!(
        root.update(cx, |root, cx| root.join_form_dir(cx)),
        Some(target_dir.clone())
    );
    assert_eq!(
        root.update(cx, |root, cx| root.join_form_recovery_is_empty(cx)),
        Some(true),
        "提交后恢复码必须从输入框清空（不在 UI 里继续持有正文）"
    );
    assert!(!b_vault_path.exists(), "非法恢复码不得创建本地库");
    assert!(!b_identity_path.exists(), "非法恢复码不得创建身份文件");
    assert!(!layout.config_path.exists(), "失败不得写公开配置");

    // ---- (b) 格式合法但内容错误：relay 认证拒绝 ----
    let wrong_seed = RecoverySeed::generate(&mut OsRandom).expect("生成假恢复码种子失败");
    let wrong_code = wrong_seed.to_code().expect("编码假恢复码失败");
    root.update(cx, |root, cx| {
        root.join_form_set_recovery_code(wrong_code.as_str(), cx);
        root.submit_join(cx);
    });
    cx.run_until_parked();
    let finished = root.update(cx, |root, cx| {
        root.wait_for_join_terminal(cx, DEFAULT_OPERATION_BUDGET)
    });
    cx.run_until_parked();
    assert!(finished);
    assert_eq!(state_of(&root, cx), RootStateLabel::JoinForm);
    let error = root
        .update(cx, |root, _| root.join_form_error().map(str::to_owned))
        .expect("表单必须显示脱敏错误");
    assert!(
        error.contains("不匹配") || error.contains("拒绝"),
        "错恢复码必须说清是认证/匹配失败：{error}"
    );
    assert!(!error.contains("mnrec1"), "不得回显恢复码：{error}");
    assert!(
        !error.contains(wrong_code.as_str()),
        "不得回显用户输入的恢复码正文"
    );
    assert_eq!(
        root.update(cx, |root, cx| root.join_form_vault_id(cx)),
        Some(creator.facts.vault_id.clone()),
        "第二次失败仍必须保留公开字段"
    );
    assert!(!b_vault_path.exists(), "认证失败不得留下半初始化库");
    assert!(!b_identity_path.exists());

    // ---- (c) 改成正确恢复码 → Main ----
    root.update(cx, |root, cx| {
        root.join_form_set_recovery_code(&creator.recovery_code, cx);
        root.submit_join(cx);
    });
    cx.run_until_parked();
    let finished = root.update(cx, |root, cx| {
        root.wait_for_join_terminal(cx, DEFAULT_OPERATION_BUDGET)
    });
    cx.run_until_parked();
    assert!(finished);
    assert_eq!(state_of(&root, cx), RootStateLabel::Main);
    assert!(layout.config_path.is_file(), "重试成功后必须写公开配置");
}

/// 验收 3（S3）：A 未发布基线 → Join 明确提示"请创建者先发布基线"，不进 Main、不写配置。
#[gpui::test]
async fn join_before_baseline_is_published_prompts_and_stays_out_of_main(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let (_dir, layout) = temp_layout("join-no-baseline");
    // A 只 Create（genesis + 初始槽），**不发布**两份 checkpoint。
    let creator = create_creator(&relay, &layout, "inst-nobase-a");
    seed_creator_note(&creator, "还没发布的笔记", "不该被 B 看到\n");

    let instance_id = "inst-nobase-b";
    let options = AppRootOptions {
        layout: layout.clone(),
        default_relay: relay.base_url().to_owned(),
        instance_id: Some(instance_id.to_owned()),
        allow_local_vault: false,
        operation_budget: DEFAULT_OPERATION_BUDGET,
    };
    let (root, cx) = open_root(cx, options);
    let target_dir = layout
        .instance_dir(instance_id)
        .expect("合法实例 id")
        .display()
        .to_string();
    open_join_form(
        &root,
        cx,
        relay.base_url(),
        &creator.facts.vault_id,
        &target_dir,
    );
    root.update(cx, |root, cx| {
        root.join_form_set_recovery_code(&creator.recovery_code, cx);
        root.submit_join(cx);
    });
    cx.run_until_parked();
    let finished = root.update(cx, |root, cx| {
        root.wait_for_join_terminal(cx, DEFAULT_OPERATION_BUDGET)
    });
    cx.run_until_parked();
    assert!(finished);
    assert_eq!(
        state_of(&root, cx),
        RootStateLabel::JoinForm,
        "没有基线不得进入 Main"
    );
    let error = root
        .update(cx, |root, _| root.join_form_error().map(str::to_owned))
        .expect("必须给出明确原因");
    assert!(error.contains("发布基线"), "必须指向「发布基线」：{error}");
    assert!(
        error.contains("NO_VALID_BASELINE"),
        "技术细节应保留（诊断需要）：{error}"
    );
    assert!(cx.debug_bounds("app-root-main").is_none());
    assert!(!layout.config_path.exists(), "失败不得写公开配置");
    let b_vault_path = layout.vault_path(instance_id).expect("合法实例 id");
    assert!(!b_vault_path.exists(), "没有基线不得留下半初始化库");
    assert!(
        !DevIdentityStore::path_for(&b_vault_path).exists(),
        "没有基线不得留下身份文件"
    );
}

/// 验收 4（S3）：目标目录已有库/身份、或被另一个进程占用 → 明确错误、不覆盖；
/// 释放后同一目录可重试成功。
#[gpui::test]
async fn join_refuses_occupied_or_existing_directories(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let (_dir, layout) = temp_layout("join-dir");
    let creator = create_creator(&relay, &layout, "inst-dir-a");
    seed_creator_note(&creator, "目录冲突测试笔记", "正文\n");
    publish_creator_baseline(&relay, &creator);

    let instance_id = "inst-dir-b";
    let options = AppRootOptions {
        layout: layout.clone(),
        default_relay: relay.base_url().to_owned(),
        instance_id: Some(instance_id.to_owned()),
        allow_local_vault: false,
        operation_budget: DEFAULT_OPERATION_BUDGET,
    };
    let (root, cx) = open_root(cx, options);
    let a_dir = creator
        .facts
        .vault_path
        .parent()
        .expect("A 的库有父目录")
        .display()
        .to_string();
    let a_identity_before = std::fs::read(DevIdentityStore::path_for(&creator.facts.vault_path))
        .expect("A 的身份必须可读");

    // ---- (a) 指向 A 的目录（已有库/身份）→ 明确拒绝、不覆盖 ----
    open_join_form(&root, cx, relay.base_url(), &creator.facts.vault_id, &a_dir);
    root.update(cx, |root, cx| {
        root.join_form_set_recovery_code(&creator.recovery_code, cx);
        root.submit_join(cx);
    });
    cx.run_until_parked();
    assert_eq!(state_of(&root, cx), RootStateLabel::JoinForm);
    let error = root
        .update(cx, |root, _| root.join_form_error().map(str::to_owned))
        .expect("必须给明确错误");
    assert!(
        error.contains("已有库文件") || error.contains("已有身份文件"),
        "必须说清目录里已有实例文件：{error}"
    );
    assert!(
        std::fs::read(DevIdentityStore::path_for(&creator.facts.vault_path))
            .expect("A 的身份必须仍可读")
            == a_identity_before,
        "不得覆盖 A 的身份文件"
    );
    assert!(creator.facts.vault_path.is_file(), "不得删除 A 的库");

    // ---- (b) 目标目录被另一个进程锁定 → 明确错误 ----
    let b_dir = layout.instance_dir(instance_id).expect("合法实例 id");
    let b_dir_text = b_dir.display().to_string();
    let held = InstanceLock::acquire(&b_dir).expect("测试占锁失败");
    root.update(cx, |root, cx| {
        root.join_form_set_dir(&b_dir_text, cx);
        root.submit_join(cx);
    });
    cx.run_until_parked();
    assert_eq!(state_of(&root, cx), RootStateLabel::JoinForm);
    let error = root
        .update(cx, |root, _| root.join_form_error().map(str::to_owned))
        .expect("锁冲突必须给明确错误");
    assert!(
        error.contains("另一个进程"),
        "锁冲突必须可诊断（不删除用户目录）：{error}"
    );
    assert!(
        b_dir.join(LOCK_FILE_NAME).exists(),
        "别人的锁文件必须原样保留"
    );

    // ---- (c) 释放锁 → 同一目录重试成功 ----
    drop(held);
    root.update(cx, |root, cx| root.submit_join(cx));
    cx.run_until_parked();
    let finished = root.update(cx, |root, cx| {
        root.wait_for_join_terminal(cx, DEFAULT_OPERATION_BUDGET)
    });
    cx.run_until_parked();
    assert!(finished);
    assert_eq!(state_of(&root, cx), RootStateLabel::Main);
}

/// 验收 5（S3）：relay 不可达 → 可重试；不建身份/配置；改回可达 relay 后成功。
#[gpui::test]
async fn join_unreachable_relay_is_retryable(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let (_dir, layout) = temp_layout("join-unreachable");
    let creator = create_creator(&relay, &layout, "inst-unreach-a");
    seed_creator_note(&creator, "不可达测试笔记", "正文\n");
    publish_creator_baseline(&relay, &creator);

    // 固定不可达端口（拒绝连接）：不要“绑定后释放动态端口”，否则并行测试可能
    // 抢到该端口，让第一次 Join 变成可达/挂起，重试时又会与旧 worker 的目录锁冲突。
    let dead_relay = "http://127.0.0.1:1".to_owned();

    let instance_id = "inst-unreach-b";
    let options = AppRootOptions {
        layout: layout.clone(),
        default_relay: relay.base_url().to_owned(),
        instance_id: Some(instance_id.to_owned()),
        allow_local_vault: false,
        operation_budget: DEFAULT_OPERATION_BUDGET,
    };
    let (root, cx) = open_root(cx, options);
    let target_dir = layout
        .instance_dir(instance_id)
        .expect("合法实例 id")
        .display()
        .to_string();
    open_join_form(&root, cx, &dead_relay, &creator.facts.vault_id, &target_dir);
    root.update(cx, |root, cx| {
        root.join_form_set_recovery_code(&creator.recovery_code, cx);
        root.submit_join(cx);
    });
    cx.run_until_parked();
    let finished = root.update(cx, |root, cx| {
        root.wait_for_join_terminal(cx, DEFAULT_OPERATION_BUDGET)
    });
    cx.run_until_parked();
    assert!(finished, "不可达也必须有终态（不能挂着）");
    assert_eq!(state_of(&root, cx), RootStateLabel::JoinForm);
    let error = root
        .update(cx, |root, _| root.join_form_error().map(str::to_owned))
        .expect("不可达必须给明确错误");
    assert!(
        error.contains("不可达") || error.contains("超时"),
        "必须说清 relay 不可达：{error}"
    );
    assert!(error.contains("重试"), "必须明确可重试：{error}");
    // 不误建身份/配置；本次新建的空目录允许保留（只清理本次产物）。
    let b_vault_path = layout.vault_path(instance_id).expect("合法实例 id");
    assert!(!b_vault_path.exists(), "不可达不得创建库");
    assert!(
        !DevIdentityStore::path_for(&b_vault_path).exists(),
        "不可达不得创建身份文件"
    );
    assert!(!layout.config_path.exists(), "不可达不得写配置");

    // 改回真实 relay → 重试成功。
    root.update(cx, |root, cx| {
        root.join_form_set_relay(relay.base_url(), cx);
        root.join_form_set_recovery_code(&creator.recovery_code, cx);
        root.submit_join(cx);
    });
    cx.run_until_parked();
    let finished = root.update(cx, |root, cx| {
        root.wait_for_join_terminal(cx, DEFAULT_OPERATION_BUDGET)
    });
    cx.run_until_parked();
    assert!(finished);
    assert_eq!(state_of(&root, cx), RootStateLabel::Main);
    assert!(layout.config_path.is_file());
}

/// 验收 6（S3）：Join 成功但配置写失败 → Error「重试保存」；重试**只重写配置**：
/// 不重跑 Join（relay 上不会多出第二台 B 设备），也不丢已落盘的库/身份。
#[cfg(unix)]
#[gpui::test]
async fn join_profile_save_failure_is_retried_without_rejoining(cx: &mut TestAppContext) {
    use std::os::unix::fs::PermissionsExt as _;

    ensure_test_user_key();
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let (_dir, layout) = temp_layout("join-save-fail");
    let creator = create_creator(&relay, &layout, "inst-jsf-a");
    seed_creator_note(&creator, "配置失败重试笔记", "正文\n");
    publish_creator_baseline(&relay, &creator);

    let instance_id = "inst-jsf-b";
    let options = AppRootOptions {
        layout: layout.clone(),
        default_relay: relay.base_url().to_owned(),
        instance_id: Some(instance_id.to_owned()),
        allow_local_vault: false,
        operation_budget: DEFAULT_OPERATION_BUDGET,
    };
    let (root, cx) = open_root(cx, options);
    let target_dir = layout
        .instance_dir(instance_id)
        .expect("合法实例 id")
        .display()
        .to_string();
    open_join_form(
        &root,
        cx,
        relay.base_url(),
        &creator.facts.vault_id,
        &target_dir,
    );

    // 让配置写失败：父目录只读（Unix 权限；Windows 用 ACL，本测试只在 Unix 跑）。
    let config_dir = layout
        .config_path
        .parent()
        .expect("配置有父目录")
        .to_path_buf();
    std::fs::create_dir_all(&config_dir).expect("创建配置目录失败");
    std::fs::set_permissions(&config_dir, std::fs::Permissions::from_mode(0o500))
        .expect("设置只读失败");

    root.update(cx, |root, cx| {
        root.join_form_set_recovery_code(&creator.recovery_code, cx);
        root.submit_join(cx);
    });
    cx.run_until_parked();
    let finished = root.update(cx, |root, cx| {
        root.wait_for_join_terminal(cx, DEFAULT_OPERATION_BUDGET)
    });
    cx.run_until_parked();
    // 立刻恢复权限：后续断言/临时目录清理不再依赖只读目录。
    std::fs::set_permissions(&config_dir, std::fs::Permissions::from_mode(0o700))
        .expect("恢复权限失败");
    assert!(finished, "Join 必须到达终态");
    assert_eq!(
        state_of(&root, cx),
        RootStateLabel::Error,
        "配置写失败必须停在 Error（Join 已成功，但不能假装进 Main）"
    );
    let message = root
        .update(cx, |root, _| root.error_message().map(str::to_owned))
        .expect("Error 页必须有文案");
    assert!(
        message.contains("配置保存失败") && message.contains("重试保存"),
        "错误必须说清「重试保存、不重新加入」：{message}"
    );
    assert!(!layout.config_path.exists(), "写失败不得留下半份配置");
    assert!(
        layout.vault_path(instance_id).expect("合法 id").is_file(),
        "Join 已成功：库文件必须仍在（不能被配置失败连带清掉）"
    );

    // 重试保存 → Main；没有重跑 Join（relay 上仍只有 A / B 两台设备）。
    root.update(cx, |root, cx| root.retry(cx));
    cx.run_until_parked();
    assert_eq!(state_of(&root, cx), RootStateLabel::Main);
    expect_bounds(cx, "app-root-main");
    let profile = layout
        .store()
        .load()
        .expect("读配置失败")
        .expect("重试保存后必须有配置");
    assert_eq!(profile.active_instance.as_deref(), Some(instance_id));
    let b_vault_path = layout.vault_path(instance_id).expect("合法实例 id");
    let b_vault = Vault::open(&b_vault_path).expect("B 的库必须可重开");
    let b_identity = DevIdentityStore
        .load(&b_vault_path)
        .expect("B 的身份必须已落盘");
    assert_eq!(
        profile.find(instance_id).expect("实例条目").device_id,
        menghunote_sync::ids::hex_of(&b_identity.device_id),
        "配置里的 device_id 必须就是已落盘身份那台设备"
    );
    let members = DevLocal::members(relay.base_url(), &b_vault, &b_identity)
        .expect("读取成员视图失败（B 是普通设备，只读可见）");
    assert_eq!(
        members.len(),
        2,
        "重试保存不得重跑 Join（否则 B 会再登记一台设备）：{members:?}"
    );
}

/// 验收 7（S3 秘密输入）：恢复码可以**粘贴**（`Ctrl+V`，`Shift+Insert` 同义），
/// 提交后输入框清空且不进任何公开事实/配置；粘贴进来的恢复码与手输等价。
#[gpui::test]
async fn join_recovery_code_can_be_pasted_from_clipboard(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let (_dir, layout) = temp_layout("join-paste");
    let creator = create_creator(&relay, &layout, "inst-paste-a");
    seed_creator_note(&creator, "粘贴恢复码笔记", "粘贴路径正文\n");
    publish_creator_baseline(&relay, &creator);

    let instance_id = "inst-paste-b";
    let options = AppRootOptions {
        layout: layout.clone(),
        default_relay: relay.base_url().to_owned(),
        instance_id: Some(instance_id.to_owned()),
        allow_local_vault: false,
        operation_budget: DEFAULT_OPERATION_BUDGET,
    };
    let (root, cx) = open_root(cx, options);
    let target_dir = layout
        .instance_dir(instance_id)
        .expect("合法实例 id")
        .display()
        .to_string();
    // 公开字段用 API 填；恢复码走真实剪贴板 + 真实按键（进入 JoinForm 后焦点在恢复码框）。
    open_join_form(
        &root,
        cx,
        relay.base_url(),
        &creator.facts.vault_id,
        &target_dir,
    );
    assert_eq!(
        root.update(cx, |root, cx| root.join_form_recovery_is_empty(cx)),
        Some(true),
        "进入表单时恢复码必须是空（不预填、不复用上一次）"
    );

    cx.write_to_clipboard(gpui::ClipboardItem::new_string(
        creator.recovery_code.clone(),
    ));
    // `Shift+Insert` 与 `Ctrl+V` 是同一套粘贴路径（两种键位都要能贴）。
    cx.simulate_keystrokes("shift-insert");
    cx.run_until_parked();
    assert_eq!(
        root.update(cx, |root, cx| root.join_form_recovery_is_empty(cx)),
        Some(false),
        "Shift+Insert 必须把剪贴板内容粘贴进恢复码框"
    );
    root.update(cx, |root, cx| root.join_form_set_recovery_code("", cx));
    cx.run_until_parked();
    cx.simulate_keystrokes("ctrl-v");
    cx.run_until_parked();
    assert_eq!(
        root.update(cx, |root, cx| root.join_form_recovery_is_empty(cx)),
        Some(false),
        "Ctrl+V 必须把剪贴板内容粘贴进恢复码框"
    );

    root.update(cx, |root, cx| root.submit_join(cx));
    cx.run_until_parked();
    let finished = root.update(cx, |root, cx| {
        root.wait_for_join_terminal(cx, DEFAULT_OPERATION_BUDGET)
    });
    cx.run_until_parked();
    assert!(finished, "粘贴的恢复码必须能完成 Join");
    assert_eq!(state_of(&root, cx), RootStateLabel::Main);
    // 公开配置里绝不能出现恢复码正文。
    let raw = std::fs::read_to_string(&layout.config_path).expect("Join 成功必须写配置");
    assert!(
        !raw.contains(creator.recovery_code.as_str()),
        "公开配置不得出现恢复码正文：{raw}"
    );
    assert!(!raw.to_ascii_lowercase().contains("mnrec1"), "{raw}");
}

// ---------------------------------------------------------------------------
// S4：设置面板（查看/修改同步配置；重启生效）
// ---------------------------------------------------------------------------

/// 一个当前没有监听的 loopback 地址（绑定后立刻释放）：足够做"合法但不在跑"的
/// relay 目标。设置保存只写公开配置、不连网，所以这里不需要真实 relay。
fn free_loopback_url() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("绑定临时端口失败");
    let addr = listener.local_addr().expect("取本地地址失败");
    drop(listener);
    format!("http://{addr}")
}

/// 写一份指向多个夹具实例的公开配置（指定 active）。
fn write_profile_multi(layout: &ProfileLayout, created: &[&CreatedInstance], active: &str) {
    let store: ProfileStore = layout.store();
    let mut profile = DevSyncProfile::default();
    for item in created {
        profile.upsert_active(InstanceEntry {
            instance_id: item.instance_id.clone(),
            display_name: None,
            relay_mode: RelayMode::LocalDev,
            api_version: RelayApiVersion::V1,
            relay_url: item.relay_url.clone(),
            vault_path: item.vault_path.to_string_lossy().into_owned(),
            vault_id: item.vault_id.clone(),
            device_id: item.device_id.clone(),
        });
    }
    profile.active_instance = Some(active.to_owned());
    store.save(&profile).expect("写测试配置失败");
}

fn open_settings(root: &Entity<AppRoot>, cx: &mut VisualTestContext) {
    root.update(cx, |root, cx| root.open_settings(cx));
    cx.run_until_parked();
}

/// 点设置页左侧分类导航（`settings-nav-<section>`）并断言右侧真的切到了该分类。
fn open_settings_nav(cx: &mut VisualTestContext, section: &'static str) {
    let selector: &'static str = Box::leak(format!("settings-nav-{section}").into_boxed_str());
    let bounds = expect_bounds(cx, selector);
    cx.simulate_click(bounds.center(), Modifiers::default());
    cx.run_until_parked();
    let section_selector: &'static str =
        Box::leak(format!("settings-section-{section}").into_boxed_str());
    expect_bounds(cx, section_selector);
}

/// 验收 1（S4）：创建实例后打开设置 → 列表含该实例、active 标记、公开 id/path 正确；
/// 面板没有秘密字段（秘密在类型上就不存在）。
#[gpui::test]
async fn settings_lists_public_instance_without_secret_fields(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let (_dir, layout) = temp_layout("settings-list");
    let created = create_instance(&relay, &layout, "inst-settings-list");
    write_profile(&layout, &created, "inst-settings-list");
    let options = AppRootOptions::for_test(layout.clone(), relay.base_url(), "inst-settings-list");
    let (root, cx) = open_root(cx, options);
    cx.run_until_parked();
    assert_eq!(state_of(&root, cx), RootStateLabel::Main);

    // 真实点击 Main 的固定入口打开设置（覆盖层；Main 仍在）。
    let entry = expect_bounds(cx, "app-root-settings");
    cx.simulate_click(entry.center(), Modifiers::default());
    cx.run_until_parked();
    assert!(
        root.update(cx, |root, _| root.settings_is_open()),
        "点击「设置」必须打开面板"
    );
    // 底栏：非 relay 分类只有「关闭」；relay 分类才有保存/错误/重启提示。
    for selector in ["dev-sync-settings", "settings-nav", "settings-close"] {
        expect_bounds(cx, selector);
    }
    assert!(
        cx.debug_bounds("settings-save").is_none(),
        "外观等即时生效分类不得显示「保存 relay」按钮"
    );
    assert!(
        cx.debug_bounds("settings-error").is_none(),
        "非 relay 分类不显示错误占位/提示"
    );
    open_settings_nav(cx, "relay");
    expect_bounds(cx, "settings-relay");
    for selector in [
        "settings-save",
        "settings-cancel",
        "settings-error",
        "settings-notice",
    ] {
        expect_bounds(cx, selector);
    }
    // 「实例」分类已移除（用户反馈没存在感）：active 实例的**公开**信息挪到
    // 「同步」分类底部（只读、可复制；不含任何秘密）。
    open_settings_nav(cx, "sync");
    let section = expect_bounds(cx, "settings-section-sync");
    expect_bounds(cx, "settings-toggle-sync");
    let info = expect_bounds(cx, "settings-public-info");
    expect_bounds(cx, "settings-api-version");
    for selector in [
        "settings-copy-vault-id",
        "settings-copy-device-id",
        "settings-copy-path",
    ] {
        expect_bounds(cx, selector);
    }
    // 公开信息必须与内容区左对齐、不超出内容宽度、放在同步面板下方。
    let section_left = section.origin.x.as_f32();
    let section_right = section_left + section.size.width.as_f32();
    assert!(
        (info.origin.x.as_f32() - section_left).abs() <= 1.0,
        "公开信息必须与内容区左对齐：info={info:?} section={section:?}"
    );
    assert!(
        info.origin.x.as_f32() + info.size.width.as_f32() <= section_right + 1.0,
        "公开信息不得超出内容区：info={info:?} section={section:?}"
    );
    let sync_content = expect_bounds(cx, "settings-toggle-sync");
    assert!(
        info.origin.y.as_f32()
            >= sync_content.origin.y.as_f32() + sync_content.size.height.as_f32() - 0.5,
        "公开信息必须在同步面板下方：info={info:?} sync={sync_content:?}"
    );
    expect_bounds(cx, "app-root-main");
    // 设置里不存在秘密输入控件（setup secret/恢复码/root/manager seed）。
    for absent in [
        "settings-secret-input",
        "settings-recovery-input",
        "settings-root-seed",
        "settings-manager-seed",
    ] {
        assert!(
            cx.debug_bounds(absent).is_none(),
            "设置面板不得出现秘密字段：{absent}"
        );
    }

    let entries = root.update(cx, |root, _| root.settings_entries());
    assert_eq!(entries.len(), 1);
    let listed = &entries[0];
    assert_eq!(listed.instance_id, created.instance_id);
    assert_eq!(listed.vault_id, created.vault_id);
    assert_eq!(listed.device_id, created.device_id);
    assert_eq!(listed.relay_url, created.relay_url);
    assert_eq!(
        listed.vault_path,
        created.vault_path.to_string_lossy(),
        "设置列表必须显示配置里的库路径"
    );
    let active = root
        .update(cx, |root, _| root.settings_active_entry())
        .expect("必须有 active 实例");
    assert_eq!(active.instance_id, created.instance_id);
    assert_eq!(
        root.update(cx, |root, cx| root.settings_relay(cx)),
        Some(created.relay_url.clone()),
        "relay 输入框必须预填 active 实例的公开 relay"
    );
    // 公开配置不得出现秘密（设置打开/关闭都不改配置）。
    let raw = std::fs::read_to_string(&layout.config_path).expect("配置必须存在");
    for forbidden in ["mnrec1", "recovery", "secret", "manager", "private"] {
        assert!(
            !raw.to_ascii_lowercase().contains(forbidden),
            "公开配置不得出现 {forbidden}：{raw}"
        );
    }

    // 取消只关闭面板，不写配置、不重建 Main。
    let shell_id = root
        .update(cx, |root, _| {
            root.main_shell().map(|shell| shell.entity_id())
        })
        .expect("Main 必须有 AppShell");
    let cancel = expect_bounds(cx, "settings-close");
    cx.simulate_click(cancel.center(), Modifiers::default());
    cx.run_until_parked();
    assert!(!root.update(cx, |root, _| root.settings_is_open()));
    assert!(
        cx.debug_bounds("dev-sync-settings").is_none(),
        "取消后覆盖层必须消失"
    );
    assert_eq!(
        root.update(cx, |root, _| root
            .main_shell()
            .map(|shell| shell.entity_id())),
        Some(shell_id),
        "打开/取消设置不得重建 AppShell"
    );
}

/// 设置页左侧分类导航（Obsidian「选项」式）：默认「外观」，点分类只渲染该分类内容；
/// 旧 selector 在对应分类打开时仍然存在；覆盖层限高、内容区独立滚动；会话内记住选择。
#[gpui::test]
async fn settings_nav_switches_sections_and_keeps_old_selectors(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let (_dir, layout) = temp_layout("settings-nav");
    let created = create_instance(&relay, &layout, "inst-settings-nav");
    write_profile(&layout, &created, "inst-settings-nav");
    let options = AppRootOptions::for_test(layout.clone(), relay.base_url(), "inst-settings-nav");
    let (root, cx) = open_root(cx, options);
    cx.run_until_parked();
    assert_eq!(state_of(&root, cx), RootStateLabel::Main);

    let entry = expect_bounds(cx, "app-root-settings");
    cx.simulate_click(entry.center(), Modifiers::default());
    cx.run_until_parked();

    // 左侧导航固定、8 个固定分类 + 分界线 + 已启用的插件分类；默认选中「外观」。
    // 「实例」分类已移除（创建 vault 改由 Main 左下角的「管理仓库」承担）。
    expect_bounds(cx, "settings-nav");
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
        let nav: &'static str = Box::leak(format!("settings-nav-{section}").into_boxed_str());
        expect_bounds(cx, nav);
    }
    // 插件分类默认启用（模板/日记），显示在分界线之后。
    for section in ["templates", "daily_notes"] {
        let nav: &'static str = Box::leak(format!("settings-nav-{section}").into_boxed_str());
        expect_bounds(cx, nav);
    }
    // 顺序断言：固定项 → 分界线 → 插件项（y 坐标递增）。
    let mut previous = f32::MIN;
    for selector in [
        "settings-nav-appearance",
        "settings-nav-language",
        "settings-nav-shortcuts",
        "settings-nav-sync",
        "settings-nav-relay",
        "settings-nav-security",
        "settings-nav-plugins",
        "settings-nav-about",
        "settings-nav-divider",
        "settings-nav-templates",
        "settings-nav-daily_notes",
    ] {
        let selector: &'static str = Box::leak(selector.to_owned().into_boxed_str());
        let bounds = expect_bounds(cx, selector);
        let y = bounds.origin.y.as_f32();
        assert!(
            y > previous,
            "{selector} 必须在上一项之下（y={y}，上一项 y={previous}）"
        );
        previous = y;
    }
    let divider = expect_bounds(cx, "settings-nav-divider");
    assert!(
        divider.size.height.as_f32() <= 2.0,
        "分界线必须是 1px 细分隔线：{divider:?}"
    );
    assert_eq!(
        root.update(cx, |root, _| root.settings_section()),
        menghunote_app::onboarding::app_root::SettingsSection::Appearance,
        "默认分类必须是「外观」"
    );
    for selector in [
        "settings-section-appearance",
        "settings-appearance",
        "settings-theme-light",
        "settings-theme-dark",
        "settings-theme-system",
        "settings-theme-current",
    ] {
        expect_bounds(cx, selector);
    }
    assert!(
        cx.debug_bounds("settings-language").is_none(),
        "只渲染当前分类：外语区不得同时存在"
    );

    // 内容区独立滚动、外层限高（不把整张卡片拉成长页）。
    let card = expect_bounds(cx, "dev-sync-settings");
    let content = expect_bounds(cx, "settings-content");
    assert!(
        content.size.height.as_f32() <= card.size.height.as_f32() + 0.5,
        "内容区不得超出面板高度：content={content:?} card={card:?}"
    );

    // 「语言」分类。
    open_settings_nav(cx, "language");
    for selector in [
        "settings-section-language",
        "settings-language",
        "settings-language-zh-CN",
        "settings-language-en",
        "settings-language-system",
        "settings-language-current",
    ] {
        expect_bounds(cx, selector);
    }
    assert!(
        cx.debug_bounds("settings-appearance").is_none(),
        "切到语言后外观区必须不存在"
    );

    // 「模板」分类：旧 `settings-templates*` 全在。
    open_settings_nav(cx, "templates");
    for selector in [
        "settings-section-templates",
        "settings-templates",
        "settings-templates-folder",
        "settings-templates-example",
    ] {
        expect_bounds(cx, selector);
    }

    // 「日记」分类：旧 `settings-daily-notes*` 全在。
    open_settings_nav(cx, "daily_notes");
    for selector in [
        "settings-section-daily_notes",
        "settings-daily-notes",
        "settings-daily-notes-folder",
        "settings-daily-notes-example",
    ] {
        expect_bounds(cx, selector);
    }

    // 「同步」分类：进入分类即直接渲染同步面板（不再需要点「展开」；没有「收起」）。
    open_settings_nav(cx, "sync");
    expect_bounds(cx, "settings-section-sync");
    let sync_content = expect_bounds(cx, "settings-toggle-sync");
    let sync_panel = expect_bounds(cx, "dev-sync-panel");
    assert!(
        sync_content.size.height.as_f32() > 40.0,
        "settings-toggle-sync 现在是同步内容容器，必须直接包含面板而不是折叠按钮：{sync_content:?}"
    );
    let panel_bottom = sync_panel.origin.y.as_f32() + sync_panel.size.height.as_f32();
    let content_bottom = sync_content.origin.y.as_f32() + sync_content.size.height.as_f32();
    assert!(
        sync_panel.origin.y.as_f32() >= sync_content.origin.y.as_f32()
            && panel_bottom <= content_bottom + 0.5,
        "同步面板必须位于 settings-toggle-sync 内容容器内：panel={sync_panel:?} container={sync_content:?}"
    );

    // 「实例」分类已移除：左栏没有该项，也不会渲染对应的内容区。
    assert!(
        cx.debug_bounds("settings-nav-instances").is_none(),
        "设置左栏不得再有「实例」分类"
    );
    assert!(
        cx.debug_bounds("settings-section-instances").is_none(),
        "设置内容区不得再渲染「实例」分类"
    );
    // 公开信息（API 面）随「同步」分类渲染。
    expect_bounds(cx, "settings-api-version");

    // 「Relay」分类：输入框 + 只读的自动模式说明（不再有模式单选）。
    open_settings_nav(cx, "relay");
    expect_bounds(cx, "settings-section-relay");
    expect_bounds(cx, "settings-relay");
    expect_bounds(cx, "settings-relay-mode-hint");
    for absent in [
        "settings-mode-local_dev",
        "settings-mode-remote_tls",
        "settings-mode",
    ] {
        assert!(
            cx.debug_bounds(absent).is_none(),
            "Relay 分类不得再有模式选择控件：{absent}"
        );
    }

    // 「安全」分类：指纹 + 错误类别。
    open_settings_nav(cx, "security");
    expect_bounds(cx, "settings-section-security");
    expect_bounds(cx, "settings-fingerprint");
    expect_bounds(cx, "settings-security-category");

    // 「关于」分类：版本/构建信息只读。
    open_settings_nav(cx, "about");
    expect_bounds(cx, "settings-section-about");
    expect_bounds(cx, "settings-about-version");
    expect_bounds(cx, "settings-about-build");

    // 会话内记住上次分类：关闭再打开仍停在「关于」。
    let cancel = expect_bounds(cx, "settings-close");
    cx.simulate_click(cancel.center(), Modifiers::default());
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("dev-sync-settings").is_none(),
        "关闭后覆盖层必须卸载"
    );
    let entry = expect_bounds(cx, "app-root-settings");
    cx.simulate_click(entry.center(), Modifiers::default());
    cx.run_until_parked();
    expect_bounds(cx, "settings-section-about");

    // 收尾：关闭设置（不影响后续断言/关窗）。
    let cancel = expect_bounds(cx, "settings-close");
    cx.simulate_click(cancel.center(), Modifiers::default());
    cx.run_until_parked();
}

/// 验收 2（S4）：合法 loopback relay → 保存 → 配置更新 + 「重启后生效」；
/// 当前 Main 不热切换；重启 AppRoot 后新 relay 生效。
#[gpui::test]
async fn settings_saves_relay_and_applies_after_restart(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let (_dir, layout) = temp_layout("settings-relay");
    let created = create_instance(&relay, &layout, "inst-settings-relay");
    write_profile(&layout, &created, "inst-settings-relay");
    let options = AppRootOptions::for_test(layout.clone(), relay.base_url(), "inst-settings-relay");
    let (root, cx) = open_root(cx, options.clone());
    cx.run_until_parked();
    assert_eq!(state_of(&root, cx), RootStateLabel::Main);
    let shell = root
        .update(cx, |root, _| root.main_shell().cloned())
        .expect("Main 必须有 AppShell");
    let shell_id = shell.entity_id();
    let old_relay = cx
        .update(|_window, app| {
            shell
                .read(app)
                .dev_sync_panel()
                .map(|panel| panel.read(app).service().config().relay.clone())
        })
        .expect("Main 的同步面板必须存在");

    open_settings(&root, cx);
    let new_relay = free_loopback_url();
    root.update(cx, |root, cx| {
        root.settings_set_relay(&new_relay, cx);
        root.settings_save(cx);
    });
    cx.run_until_parked();
    assert!(
        root.update(cx, |root, _| root.settings_error().map(str::to_owned))
            .is_none(),
        "合法 loopback relay 不应报错：{:?}",
        root.update(cx, |root, _| root.settings_error().map(str::to_owned))
    );
    let notice = root
        .update(cx, |root, _| root.settings_notice().map(str::to_owned))
        .expect("保存成功必须有提示");
    assert!(
        notice.contains("重启") && notice.contains("生效"),
        "必须明确「重启后生效」：{notice}"
    );
    // 配置文件更新（只改 relay；其它公开事实不变）。
    let profile = layout
        .store()
        .load()
        .expect("配置必须可读")
        .expect("配置必须存在");
    let saved = profile.find("inst-settings-relay").expect("实例条目");
    assert_eq!(saved.relay_url, new_relay);
    assert_eq!(saved.vault_id, created.vault_id);
    assert_eq!(saved.device_id, created.device_id);
    assert_eq!(
        saved.vault_path,
        created.vault_path.to_string_lossy(),
        "保存 relay 不得改路径"
    );
    // 当前窗口不热切换：AppShell 实体不变，面板/服务仍用旧 relay。
    assert_eq!(
        root.update(cx, |root, _| root
            .main_shell()
            .map(|shell| shell.entity_id())),
        Some(shell_id)
    );
    let runtime_relay = cx
        .update(|_window, app| {
            shell
                .read(app)
                .dev_sync_panel()
                .map(|panel| panel.read(app).service().config().relay.clone())
        })
        .expect("Main 的同步面板必须存在");
    assert_eq!(
        runtime_relay, old_relay,
        "保存配置不得热切换当前 worker/relay"
    );

    // 重启：关闭窗口（Main 有界排空）→ 新 AppRoot 用新 relay 打开同一实例。
    let cancel = expect_bounds(cx, "settings-close");
    cx.simulate_click(cancel.center(), Modifiers::default());
    cx.run_until_parked();
    assert!(cx.simulate_close(), "Main 状态允许关窗");
    cx.run_until_parked();
    let restarted = cx.update(|window, cx| {
        let options = options.clone();
        window.replace_root(cx, move |_window, cx| AppRoot::new(options, cx))
    });
    cx.run_until_parked();
    assert_eq!(state_of(&restarted, cx), RootStateLabel::Main);
    let restarted_shell = restarted
        .update(cx, |root, _| root.main_shell().cloned())
        .expect("重启后 Main 必须有 AppShell");
    let restarted_relay = cx
        .update(|_window, app| {
            restarted_shell
                .read(app)
                .dev_sync_panel()
                .map(|panel| panel.read(app).service().config().relay.clone())
        })
        .expect("重启后的同步面板必须存在");
    assert_eq!(
        restarted_relay, new_relay,
        "重启后必须用配置文件里的新 relay 打开实例"
    );
}

/// 验收 3（S4）：非法 relay（空/非 loopback/https/userinfo/路径）→ 内联错误、不写配置。
#[gpui::test]
async fn settings_rejects_invalid_relay_without_writing(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let (_dir, layout) = temp_layout("settings-invalid");
    let created = create_instance(&relay, &layout, "inst-settings-invalid");
    write_profile(&layout, &created, "inst-settings-invalid");
    let options =
        AppRootOptions::for_test(layout.clone(), relay.base_url(), "inst-settings-invalid");
    let (root, cx) = open_root(cx, options);
    cx.run_until_parked();
    open_settings(&root, cx);
    let before = std::fs::read(&layout.config_path).expect("配置必须存在");

    let cases = [
        ("", "不能为空"),
        // 其它 scheme 按 remote_tls 推断，由权威 parser 报"只接受 https"。
        ("ftp://relay.example", "https"),
        ("http://relay.example:18080", "loopback"),
        ("http://192.168.1.9:18080", "loopback"),
        ("http://0.0.0.0:18080", "loopback"),
        ("http://127.0.0.2:18080", "loopback"),
        ("http://user:pass@127.0.0.1:18080", "userinfo"),
        ("http://127.0.0.1:18080/api", "canonical origin"),
    ];
    for (bad, needle) in cases {
        root.update(cx, |root, cx| {
            root.settings_set_relay(bad, cx);
            root.settings_save(cx);
        });
        cx.run_until_parked();
        let error = root
            .update(cx, |root, _| root.settings_error().map(str::to_owned))
            .unwrap_or_else(|| panic!("非法 relay {bad:?} 必须内联报错"));
        assert!(
            error.contains(needle),
            "非法 relay {bad:?} 的错误必须可诊断（应含 {needle}）：{error}"
        );
        if bad.contains("pass") {
            assert!(!error.contains("pass"), "错误不得回显 userinfo：{error}");
            assert!(!error.contains("user:"), "错误不得回显 userinfo：{error}");
        }
        let raw = std::fs::read(&layout.config_path).expect("配置必须仍可读");
        assert_eq!(raw, before, "非法 relay {bad:?} 不得写配置");
        assert!(
            root.update(cx, |root, _| root.settings_is_open()),
            "非法 relay 不能关掉设置面板"
        );
    }
    // 面板仍显示原来的合法 relay；取消不写。
    assert!(root.update(cx, |root, cx| root.settings_relay(cx).is_some()));
}

/// 验收 4（S4）：多实例切换 active → 只写配置 + 「重启后生效」；当前 Main 不变，
/// 重启后打开新 active 实例。
#[gpui::test]
async fn settings_switches_active_and_restarts_into_it(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let relay_a = DevRelay::start().expect("起进程内 dev relay 失败");
    let relay_b = DevRelay::start().expect("起进程内 dev relay 失败");
    let (_dir, layout) = temp_layout("settings-active");
    let created_a = create_instance(&relay_a, &layout, "inst-active-a");
    let created_b = create_instance(&relay_b, &layout, "inst-active-b");
    write_profile_multi(&layout, &[&created_a, &created_b], "inst-active-a");
    let options = AppRootOptions::for_test(layout.clone(), relay_a.base_url(), "inst-active-a");
    let (root, cx) = open_root(cx, options.clone());
    cx.run_until_parked();
    assert_eq!(state_of(&root, cx), RootStateLabel::Main);
    assert_eq!(
        root.update(cx, |root, _| root
            .active_entry()
            .map(|entry| entry.instance_id.clone())),
        Some("inst-active-a".to_owned())
    );
    let shell_id = root
        .update(cx, |root, _| {
            root.main_shell().map(|shell| shell.entity_id())
        })
        .expect("Main 必须有 AppShell");

    open_settings(&root, cx);
    assert_eq!(
        root.update(cx, |root, _| root.settings_entries().len()),
        2,
        "设置必须列出配置里的两个实例"
    );
    assert_eq!(
        root.update(cx, |root, _| root
            .settings_active_entry()
            .map(|entry| entry.instance_id))
            .as_deref(),
        Some("inst-active-a")
    );
    // 关闭设置：「设为默认」已从设置页挪到 Main 左下角「管理仓库」。
    let close = expect_bounds(cx, "settings-close");
    cx.simulate_click(close.center(), Modifiers::default());
    cx.run_until_parked();
    assert!(!root.update(cx, |root, _| root.settings_is_open()));

    // 左下角切换菜单 →「管理仓库…」→ 第二行（active 优先排序后 A 在前、B 在后）
    // 的「设为默认」。
    let switcher = expect_bounds(cx, "app-root-vault-switcher");
    cx.simulate_click(switcher.center(), Modifiers::default());
    cx.run_until_parked();
    let manage = expect_bounds(cx, "app-root-vault-manage");
    cx.simulate_click(manage.center(), Modifiers::default());
    cx.run_until_parked();
    expect_bounds(cx, "app-root-vault-manager");
    let set_active = expect_bounds(cx, "vault-manager-default-1");
    cx.simulate_click(set_active.center(), Modifiers::default());
    cx.run_until_parked();
    let notice = root
        .update(cx, |root, _| root.vault_manager_notice().map(str::to_owned))
        .expect("切换 active 必须有提示");
    assert!(
        notice.contains("重启") && notice.contains("生效"),
        "切换 active 必须标注「重启后生效」：{notice}"
    );
    let profile = layout
        .store()
        .load()
        .expect("配置必须可读")
        .expect("配置必须存在");
    assert_eq!(
        profile.active_instance.as_deref(),
        Some("inst-active-b"),
        "切换 active 必须原子写入配置文件"
    );
    // 不热切换：当前 Main 仍是 A，AppShell 实体不变。
    assert_eq!(
        root.update(cx, |root, _| root
            .active_entry()
            .map(|entry| entry.instance_id.clone())),
        Some("inst-active-a".to_owned()),
        "当前主界面必须保持不变，直到重启"
    );
    assert_eq!(
        root.update(cx, |root, _| root
            .main_shell()
            .map(|shell| shell.entity_id())),
        Some(shell_id)
    );
    // 管理仓库只写 active/startup，不改当前会话；重新打开设置时按磁盘 active 重读。
    let manager_close = expect_bounds(cx, "vault-manager-close");
    cx.simulate_click(manager_close.center(), Modifiers::default());
    cx.run_until_parked();
    open_settings(&root, cx);
    assert_eq!(
        root.update(cx, |root, _| root
            .settings_active_entry()
            .map(|entry| entry.instance_id)),
        Some("inst-active-b".to_owned()),
        "重新打开设置必须按磁盘 active 显示 B"
    );
    assert_eq!(
        root.update(cx, |root, cx| root.settings_relay(cx)),
        Some(created_b.relay_url.clone()),
        "设置 relay 输入框必须跟到新 active（避免写错实例）"
    );

    // 重启 → 打开新 active B。
    let cancel = expect_bounds(cx, "settings-close");
    cx.simulate_click(cancel.center(), Modifiers::default());
    cx.run_until_parked();
    assert!(cx.simulate_close());
    cx.run_until_parked();
    let restarted = cx.update(|window, cx| {
        let options = options.clone();
        window.replace_root(cx, move |_window, cx| AppRoot::new(options, cx))
    });
    cx.run_until_parked();
    assert_eq!(state_of(&restarted, cx), RootStateLabel::Main);
    assert_eq!(
        restarted.update(cx, |root, _| root
            .active_entry()
            .map(|entry| entry.instance_id.clone())),
        Some("inst-active-b".to_owned()),
        "重启后必须打开新 active 实例"
    );
    let restarted_shell = restarted
        .update(cx, |root, _| root.main_shell().cloned())
        .expect("重启后 Main 必须有 AppShell");
    let restarted_relay = cx
        .update(|_window, app| {
            restarted_shell
                .read(app)
                .dev_sync_panel()
                .map(|panel| panel.read(app).service().config().relay.clone())
        })
        .expect("重启后的同步面板必须存在");
    assert_eq!(restarted_relay, created_b.relay_url);
}

/// 验收 5（S4，Unix）：配置写失败 → 内联错误可重试；不重建实例、不跑 Create/Join。
#[cfg(unix)]
#[gpui::test]
async fn settings_save_failure_is_retryable_without_recreating(cx: &mut TestAppContext) {
    use std::os::unix::fs::PermissionsExt as _;

    ensure_test_user_key();
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let (_dir, layout) = temp_layout("settings-save-fail");
    let created = create_instance(&relay, &layout, "inst-settings-fail");
    write_profile(&layout, &created, "inst-settings-fail");
    let options = AppRootOptions::for_test(layout.clone(), relay.base_url(), "inst-settings-fail");
    let (root, cx) = open_root(cx, options);
    cx.run_until_parked();
    open_settings(&root, cx);
    let shell_id = root
        .update(cx, |root, _| {
            root.main_shell().map(|shell| shell.entity_id())
        })
        .expect("Main 必须有 AppShell");
    let before = std::fs::read(&layout.config_path).expect("配置必须存在");
    let vault_before = std::fs::read(&created.vault_path).expect("库必须存在");
    let identity_path = DevIdentityStore::path_for(&created.vault_path);
    let identity_before = std::fs::read(&identity_path).expect("身份必须存在");

    // 让配置写失败：父目录只读（Unix 权限；Windows 用 ACL，本测试只在 Unix 跑）。
    let config_dir = layout
        .config_path
        .parent()
        .expect("配置有父目录")
        .to_path_buf();
    std::fs::create_dir_all(&config_dir).expect("创建配置目录失败");
    std::fs::set_permissions(&config_dir, std::fs::Permissions::from_mode(0o500))
        .expect("设置只读失败");
    let new_relay = free_loopback_url();
    root.update(cx, |root, cx| {
        root.settings_set_relay(&new_relay, cx);
        root.settings_save(cx);
    });
    cx.run_until_parked();
    // 立刻恢复权限：后续重试/临时目录清理不再依赖只读目录。
    std::fs::set_permissions(&config_dir, std::fs::Permissions::from_mode(0o700))
        .expect("恢复权限失败");
    let error = root
        .update(cx, |root, _| root.settings_error().map(str::to_owned))
        .expect("写失败必须内联报错");
    assert!(
        error.contains("配置保存失败") && error.contains("重试"),
        "写失败必须说清可重试：{error}"
    );
    assert_eq!(
        std::fs::read(&layout.config_path).expect("配置必须仍在"),
        before,
        "写失败不得留下半份配置"
    );
    assert_eq!(
        root.update(cx, |root, _| root
            .main_shell()
            .map(|shell| shell.entity_id())),
        Some(shell_id),
        "写失败不得重建 AppShell"
    );
    assert!(
        !root.update(cx, |root, _| root.is_working()),
        "保存配置失败不得触发 Create/Join"
    );
    assert!(created.vault_path.is_file(), "写失败不得删库");
    assert!(identity_path.is_file(), "写失败不得删身份");
    assert_eq!(
        std::fs::read(&created.vault_path).expect("库必须可读"),
        vault_before
    );
    assert_eq!(
        std::fs::read(&identity_path).expect("身份必须可读"),
        identity_before
    );

    // 恢复权限后重试保存 → 成功；公开事实不变（没有重跑 Create/Join）。
    root.update(cx, |root, cx| root.settings_save(cx));
    cx.run_until_parked();
    assert!(
        root.update(cx, |root, _| root.settings_error().map(str::to_owned))
            .is_none(),
        "恢复权限后重试保存必须成功：{:?}",
        root.update(cx, |root, _| root.settings_error().map(str::to_owned))
    );
    let notice = root
        .update(cx, |root, _| root.settings_notice().map(str::to_owned))
        .expect("重试成功必须有提示");
    assert!(notice.contains("重启后生效"), "{notice}");
    let profile = layout
        .store()
        .load()
        .expect("配置必须可读")
        .expect("配置必须存在");
    let saved = profile.find("inst-settings-fail").expect("实例条目");
    assert_eq!(saved.relay_url, new_relay);
    assert_eq!(saved.vault_id, created.vault_id);
    assert_eq!(saved.device_id, created.device_id);
}

/// 验收 6（S4）：Main 打开设置再关闭 → 不写配置、`AppShell`/当前文档未重建。
#[gpui::test]
async fn settings_cancel_keeps_main_shell_and_document(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let (_dir, layout) = temp_layout("settings-cancel");
    let created = create_instance(&relay, &layout, "inst-settings-cancel");
    write_profile(&layout, &created, "inst-settings-cancel");
    let options =
        AppRootOptions::for_test(layout.clone(), relay.base_url(), "inst-settings-cancel");
    let (root, cx) = open_root(cx, options);
    cx.run_until_parked();
    assert_eq!(state_of(&root, cx), RootStateLabel::Main);
    // 建一篇笔记并打开（Main 里真实文档状态）。
    let new_note = expect_bounds(cx, "sidebar-new-note");
    cx.simulate_click(new_note.center(), Modifiers::default());
    cx.run_until_parked();
    expect_bounds(cx, "tree-row-0");
    let shell = root
        .update(cx, |root, _| root.main_shell().cloned())
        .expect("Main 必须有 AppShell");
    let shell_id = shell.entity_id();
    let text_before = cx
        .update(|_window, app| shell.read(app).dev_sync_current_text(app))
        .expect("打开笔记后必须有当前正文");
    let before = std::fs::read(&layout.config_path).expect("配置必须存在");

    open_settings(&root, cx);
    assert!(root.update(cx, |root, _| root.settings_is_open()));
    // 改了输入框但点「关闭（不保存）」：不得写配置。
    root.update(cx, |root, cx| {
        root.settings_set_relay("http://127.0.0.1:1", cx)
    });
    let cancel = expect_bounds(cx, "settings-close");
    cx.simulate_click(cancel.center(), Modifiers::default());
    cx.run_until_parked();
    assert!(!root.update(cx, |root, _| root.settings_is_open()));
    assert_eq!(
        std::fs::read(&layout.config_path).expect("配置必须仍在"),
        before,
        "取消不得写配置"
    );
    assert_eq!(
        root.update(cx, |root, _| root
            .main_shell()
            .map(|shell| shell.entity_id())),
        Some(shell_id),
        "关闭设置不得重建 AppShell"
    );
    expect_bounds(cx, "tree-row-0");
    let text_after = cx
        .update(|_window, app| shell.read(app).dev_sync_current_text(app))
        .expect("关闭设置后当前正文必须仍在");
    assert_eq!(text_after, text_before, "关闭设置不得重置当前文档内容");
}

// ---------------------------------------------------------------------------
// S3a（ADR 0013）：remote 模式 UI / 安全指纹 / 启动 pending 对账
// ---------------------------------------------------------------------------

/// 在已存在的实例库上写一条指定阶段的 `sync.init_pending`（测试夹具）。
fn seed_pending(
    vault_path: &Path,
    phase: menghunote_sync::init_journal::InitPhase,
    created: &CreatedInstance,
) {
    use menghunote_sync::init_journal::{
        advance_phase, begin_pending, InitKind, InitPendingV1, InitPhase,
    };
    let vault = Vault::open(vault_path).expect("打开夹具库");
    let vault_id = menghunote_sync::ids::parse_hex::<16>(&created.vault_id).expect("vault id");
    let device_id = menghunote_sync::ids::parse_hex::<16>(&created.device_id).expect("device id");
    let pending = InitPendingV1::prepared(
        InitKind::NewGenesis,
        menghunote_app::onboarding::RelayMode::LocalDev,
        created.relay_url.clone(),
        vault_id,
        device_id,
        1,
    );
    begin_pending(&vault, &pending).expect("写 pending");
    match phase {
        InitPhase::Prepared => {}
        InitPhase::GenesisRequested => {
            advance_phase(&vault, InitPhase::Prepared, InitPhase::GenesisRequested)
                .expect("请求已发");
        }
        InitPhase::IdentityWritten => {
            advance_phase(&vault, InitPhase::Prepared, InitPhase::GenesisRequested)
                .expect("请求已发");
            advance_phase(
                &vault,
                InitPhase::GenesisRequested,
                InitPhase::IdentityWritten,
            )
            .expect("身份已写");
        }
        InitPhase::PinCommitted => {
            advance_phase(&vault, InitPhase::Prepared, InitPhase::GenesisRequested)
                .expect("请求已发");
            advance_phase(&vault, InitPhase::GenesisRequested, InitPhase::PinCommitted)
                .expect("pin 已提交");
        }
        InitPhase::Done => {
            advance_phase(&vault, InitPhase::Prepared, InitPhase::GenesisRequested)
                .expect("请求已发");
            advance_phase(&vault, InitPhase::GenesisRequested, InitPhase::PinCommitted)
                .expect("pin 已提交");
            advance_phase(&vault, InitPhase::PinCommitted, InitPhase::Done).expect("done");
        }
    }
    assert_eq!(
        menghunote_sync::init_journal::load_pending(&vault, None)
            .expect("读 pending")
            .expect("pending 仍在")
            .phase,
        phase
    );
}

fn pending_phase(vault_path: &Path) -> Option<menghunote_sync::init_journal::InitPhase> {
    let vault = Vault::open(vault_path).expect("打开库");
    menghunote_sync::init_journal::load_pending(&vault, None)
        .expect("读 pending")
        .map(|pending| pending.phase)
}

fn corrupt_pending(vault_path: &Path) {
    let vault = Vault::open(vault_path).expect("打开库");
    vault
        .set_meta(
            menghunote_sync::init_journal::META_INIT_PENDING,
            b"definitely-not-a-cde-pending",
        )
        .expect("写坏 pending");
}

/// remote 模式：**不再要求**"确认连接 HTTPS"勾选（用户反馈：按 relay 配置连接即可），
/// 保存后提示重启；配置写 ProfileV2；重启后按新模式打开（不降级 local_dev）。
#[gpui::test]
async fn settings_remote_mode_saves_without_consent_and_restarts(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let (_dir, layout) = temp_layout("settings-remote");
    let created = create_instance(&relay, &layout, "inst-settings-remote");
    write_profile(&layout, &created, "inst-settings-remote");
    let options =
        AppRootOptions::for_test(layout.clone(), relay.base_url(), "inst-settings-remote");
    let (root, cx) = open_root(cx, options);
    cx.run_until_parked();
    assert_eq!(state_of(&root, cx), RootStateLabel::Main);
    open_settings(&root, cx);

    // 直接输入合法 HTTPS origin → 自动推断 remote_tls 并保存成功（没有额外确认框）。
    root.update(cx, |root, cx| {
        root.settings_set_relay("https://relay.example", cx);
        root.settings_save(cx);
    });
    cx.run_until_parked();
    assert!(
        root.update(cx, |root, _| root.settings_error().map(str::to_owned))
            .is_none(),
        "合法 remote_tls 不需要额外确认即可保存"
    );
    let notice = root
        .update(cx, |root, _| root.settings_notice().map(str::to_owned))
        .expect("保存后必须有提示");
    assert!(notice.contains("重启后生效"), "{notice}");
    assert!(notice.contains("不会清 pin"), "{notice}");
    let profile = layout.store().load().expect("读配置").expect("配置存在");
    let entry = profile.find("inst-settings-remote").expect("实例条目");
    assert_eq!(entry.relay_mode, RelayMode::RemoteTls);
    assert_eq!(entry.relay_url, "https://relay.example");

    // 重启：同一份配置按 remote_tls 打开（vault/identity/pin 都在，不降级）。
    assert!(cx.simulate_close(), "Main 状态允许关窗（释放实例锁）");
    cx.run_until_parked();
    let restarted = cx.update(|window, cx| {
        let options = AppRootOptions::for_test(
            layout.clone(),
            "https://relay.example".to_owned(),
            "inst-settings-remote",
        );
        window.replace_root(cx, move |_window, cx| AppRoot::new(options, cx))
    });
    cx.run_until_parked();
    let root = restarted;
    assert_eq!(
        state_of(&root, cx),
        RootStateLabel::Main,
        "重启后必须按保存的 remote 模式打开已存在实例"
    );
    assert_eq!(
        root.update(cx, |root, _| root
            .active_entry()
            .map(|entry| entry.relay_mode)),
        Some(RelayMode::RemoteTls)
    );
}

/// 模式不再手选：https → remote_tls，http loopback → local_dev（dev-sync）；
/// 非法 URL 由权威 parser 拒绝，不写配置、不回显 userinfo。
#[gpui::test]
async fn settings_infers_mode_from_relay_text_and_rejects_invalid_urls(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let (_dir, layout) = temp_layout("settings-mode-url");
    let created = create_instance(&relay, &layout, "inst-mode-url");
    write_profile(&layout, &created, "inst-mode-url");
    let options = AppRootOptions::for_test(layout.clone(), relay.base_url(), "inst-mode-url");
    let (root, cx) = open_root(cx, options);
    cx.run_until_parked();
    open_settings(&root, cx);

    // https → 自动推断 remote_tls（不再需要先切换模式）。
    root.update(cx, |root, cx| {
        root.settings_set_relay("https://relay.example", cx);
        root.settings_save(cx);
    });
    cx.run_until_parked();
    assert!(
        root.update(cx, |root, _| root.settings_error().map(str::to_owned))
            .is_none(),
        "https 输入必须自动按 remote_tls 保存：{:?}",
        root.update(cx, |root, _| root.settings_error().map(str::to_owned))
    );
    let profile = layout.store().load().expect("读配置").expect("配置存在");
    let entry = profile.find("inst-mode-url").expect("实例条目");
    assert_eq!(entry.relay_mode, RelayMode::RemoteTls);
    assert_eq!(entry.relay_url, "https://relay.example");

    // http loopback → 自动推断 local_dev（dev-sync 构建允许）。
    let loopback = free_loopback_url();
    root.update(cx, |root, cx| {
        root.settings_set_relay(&loopback, cx);
        root.settings_save(cx);
    });
    cx.run_until_parked();
    assert!(
        root.update(cx, |root, _| root.settings_error().map(str::to_owned))
            .is_none(),
        "http loopback 必须自动按 local_dev 保存：{:?}",
        root.update(cx, |root, _| root.settings_error().map(str::to_owned))
    );
    let profile = layout.store().load().expect("读配置").expect("配置存在");
    let entry = profile.find("inst-mode-url").expect("实例条目");
    assert_eq!(entry.relay_mode, RelayMode::LocalDev);
    assert_eq!(entry.relay_url, loopback);
    let after_good = std::fs::read(&layout.config_path).expect("配置必须存在");

    // 非法 URL：按输入 scheme 推断后仍由权威 parser 拒绝，配置不被写坏、不回显 userinfo。
    for (bad, needle) in [
        ("https://user:pass@relay.example", "userinfo"),
        ("https://relay.example/api", "canonical origin"),
        ("https://relay.example?x=1", "query"),
        ("ftp://relay.example", "https"),
        ("http://relay.example:18080", "loopback"),
    ] {
        root.update(cx, |root, cx| {
            root.settings_set_relay(bad, cx);
            root.settings_save(cx);
        });
        cx.run_until_parked();
        let error = root
            .update(cx, |root, _| root.settings_error().map(str::to_owned))
            .unwrap_or_else(|| panic!("非法 relay {bad:?} 必须拒绝"));
        assert!(error.contains(needle), "{bad} → {error}");
        assert!(!error.contains("pass"), "不得回显 userinfo：{error}");
        assert_eq!(
            std::fs::read(&layout.config_path).expect("配置必须仍可读"),
            after_good,
            "非法 relay {bad:?} 不得写配置"
        );
    }
}

/// 有 pin：设置页显示公开指纹、可复制完整公开值、无秘密/无重置按钮。
#[gpui::test]
async fn settings_fingerprint_shows_and_copies_only_public_values(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let (_dir, layout) = temp_layout("settings-fingerprint");
    let created = create_instance(&relay, &layout, "inst-fingerprint");
    write_profile(&layout, &created, "inst-fingerprint");
    let options = AppRootOptions::for_test(layout.clone(), relay.base_url(), "inst-fingerprint");
    let (root, cx) = open_root(cx, options);
    cx.run_until_parked();
    open_settings(&root, cx);

    open_settings_nav(cx, "security");
    for selector in [
        "settings-fingerprint",
        "settings-fingerprint-headline",
        "settings-copy-fingerprint",
        "settings-security-category",
    ] {
        expect_bounds(cx, selector);
    }
    open_settings_nav(cx, "relay");
    for selector in ["settings-relay", "settings-relay-mode-hint"] {
        expect_bounds(cx, selector);
    }
    for absent in [
        "settings-mode-local_dev",
        "settings-mode-remote_tls",
        "settings-mode",
    ] {
        assert!(
            cx.debug_bounds(absent).is_none(),
            "Relay 分类不得再提供模式选择：{absent}"
        );
    }
    // 只读：没有任何重置 pin / 强制信任 / 接受无效证书的按钮。
    for absent in [
        "settings-reset-pin",
        "settings-trust-any",
        "settings-accept-invalid-cert",
        "settings-force-trust",
    ] {
        assert!(cx.debug_bounds(absent).is_none(), "设置页不得提供 {absent}");
    }
    let state = root
        .update(cx, |root, _| root.settings_fingerprint_state())
        .expect("设置必须带指纹快照");
    assert!(state.has_pin(), "Create 后必须已有 durable pin");
    let headline = state.headline();
    assert!(headline.contains("durable"), "{headline}");
    let previous_notice = root.update(cx, |root, _| root.settings_notice().map(str::to_owned));
    let _ = previous_notice;
    root.update(cx, |root, cx| root.settings_copy_fingerprint(cx));
    cx.run_until_parked();
    let copied = cx
        .read_from_clipboard()
        .and_then(|item| item.text())
        .expect("复制后剪贴板必须有文本");
    for forbidden in ["mnrec1", "secret", "root", "private", "token"] {
        assert!(
            !copied.to_ascii_lowercase().contains(forbidden),
            "指纹复制不得含 {forbidden}：{copied}"
        );
    }
    assert!(copied.contains("relay instance"), "{copied}");
    assert!(copied.contains("record：rev 1"), "{copied}");
    let notice = root
        .update(cx, |root, _| root.settings_notice().map(str::to_owned))
        .expect("复制后必须有提示");
    assert!(notice.contains("已复制安全指纹"), "{notice}");

    // 公开配置里也不能出现秘密。
    let raw = std::fs::read_to_string(&layout.config_path).expect("配置存在");
    for forbidden in ["mnrec1", "secret", "root", "private"] {
        assert!(!raw.to_ascii_lowercase().contains(forbidden), "{raw}");
    }
}

/// 无 pin：显示「未建立信任/NeedsTrust」，不提供自动信任；复制也只给公开说明。
#[gpui::test]
async fn settings_fingerprint_without_pin_says_not_trusted(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let (_dir, layout) = temp_layout("settings-no-pin");
    let created = create_instance(&relay, &layout, "inst-no-pin");
    // 删掉 durable pin（模拟还没建立信任的历史实例），不动库/身份。
    {
        let vault = Vault::open(&created.vault_path).expect("打开库");
        vault
            .exec_transaction(
                &[menghunote_storage::Statement::new(
                    "DELETE FROM meta WHERE key = ?1",
                    vec![menghunote_storage::Value::Text(
                        "sync.control_pin".to_owned(),
                    )],
                )],
                &[],
            )
            .expect("删除 pin");
    }
    write_profile(&layout, &created, "inst-no-pin");
    let options = AppRootOptions::for_test(layout.clone(), relay.base_url(), "inst-no-pin");
    let (root, cx) = open_root(cx, options);
    cx.run_until_parked();
    assert_eq!(
        state_of(&root, cx),
        RootStateLabel::Main,
        "无 pin 仍可打开（未同步）"
    );
    open_settings(&root, cx);
    let state = root
        .update(cx, |root, _| root.settings_fingerprint_state())
        .expect("设置必须带指纹快照");
    assert!(!state.has_pin());
    let headline = state.headline();
    assert!(headline.contains("未建立信任"), "{headline}");
    assert!(headline.contains("NeedsTrust"), "{headline}");
    assert!(headline.contains("不会自动信任"), "{headline}");
    open_settings_nav(cx, "security");
    assert!(cx.debug_bounds("settings-copy-fingerprint").is_some());
    expect_bounds(cx, "settings-security-category");
    for absent in ["settings-reset-pin", "settings-trust-any"] {
        assert!(cx.debug_bounds(absent).is_none());
    }
}

/// v1 公开配置（没有 relay_mode）在启动时**显式迁移**为 v2/local_dev 并写回。
#[gpui::test]
async fn v1_profile_is_migrated_on_boot_without_recreating(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let (_dir, layout) = temp_layout("v1-migrate");
    let created = create_instance(&relay, &layout, "inst-v1");
    std::fs::create_dir_all(layout.config_path.parent().expect("父目录")).expect("建配置目录");
    std::fs::write(
        &layout.config_path,
        format!(
            "{{\"version\":1,\"active_instance\":\"inst-v1\",\"instances\":[\
             {{\"instance_id\":\"inst-v1\",\"relay_url\":\"{}\",\
             \"vault_path\":\"{}\",\"vault_id\":\"{}\",\"device_id\":\"{}\"}}]}}",
            created.relay_url,
            created.vault_path.to_string_lossy(),
            created.vault_id,
            created.device_id,
        ),
    )
    .expect("写 v1 配置");
    let options = AppRootOptions::for_test(layout.clone(), relay.base_url(), "inst-v1");
    let (root, cx) = open_root(cx, options);
    cx.run_until_parked();
    assert_eq!(state_of(&root, cx), RootStateLabel::Main);
    let raw = std::fs::read_to_string(&layout.config_path).expect("配置必须写回");
    let value: serde_json::Value = serde_json::from_str(&raw).expect("v2 JSON");
    assert_eq!(value["version"], serde_json::json!(2));
    assert_eq!(value["instances"][0]["relay_mode"], "local_dev");
    assert!(created.vault_path.is_file(), "迁移不得重建/删除库");
}

// ---------------------------------------------------------------------------
// S3b（ADR 0013）：remote_tls 的真实 TLS Create/Join（进程内 TLS relay + 显式自签根）
// ---------------------------------------------------------------------------

/// 远端 Create 的 A 侧夹具：真实 TLS relay + 真实 `remote_init::create_remote`。
struct RemoteCreator {
    identity: LocalIdentityV1,
    vault_id: [u8; 16],
    recovery_code: String,
    vault_path: PathBuf,
}

impl RemoteCreator {
    fn vault_id_hex(&self) -> String {
        menghunote_sync::ids::hex_of(&self.vault_id)
    }
}

/// 远端 A：TLS context（显式注入自签根）+ `create_remote` + 真实身份落盘 + 一篇笔记 + 基线。
fn create_remote_creator(
    relay: &DevRelay,
    layout: &ProfileLayout,
    instance_id: &str,
) -> RemoteCreator {
    let ca = relay.tls_ca_der().expect("必须是 TLS relay").to_vec();
    let ctx = RelayContext::remote_tls(relay.base_url(), DEFAULT_TRANSPORT_TIMEOUTS)
        .expect("remote_tls context")
        .with_test_extra_roots(vec![ca])
        .expect("注入自签根");
    let vault_path = layout.vault_path(instance_id).expect("合法实例 id");
    let store = DevIdentityStore;
    let persist_path = vault_path.clone();
    let created = create_remote(
        ctx.clone(),
        relay.setup_secret(),
        &vault_path,
        "onboarding-ui remote A",
        move |identity| {
            store
                .persist_protected(&persist_path, identity)
                .map_err(|error| SyncError::Io(format!("dev 身份持久化失败：{error:#}")))
        },
    )
    .expect("远端 A Create 失败");
    // A 的笔记（结构 + 正文 + sync intent）：B Join 后必须能自动打开它。
    let peer = created.identity.loro_peer_id();
    let epoch = created.identity.current_epoch;
    let mut writer =
        StructureWriter::open_with_sync(&created.vault, peer, Some(SyncContext::new(peer, epoch)))
            .expect("打开结构写入口失败");
    let seed = writer
        .doc()
        .create_note("A 的远端笔记", 1)
        .expect("结构 CRDT 新建笔记失败");
    writer
        .commit("onboarding-ui-remote-seed")
        .expect("结构提交失败");
    let (doc, head) =
        load_body_doc(&created.vault, &seed.body_doc_id, peer, None).expect("加载正文文档失败");
    doc.insert_scalars(0, "A 的远端正文\n")
        .expect("写入正文失败");
    let base = head.version_vector().expect("解码 head VV 失败");
    let update = doc.pending_update(&base).expect("导出正文增量失败");
    let seq = head.head_seq.saturating_add(1);
    let intent = local_sync_intent(
        &seed.body_doc_id,
        seq,
        "onboarding-ui-remote-body",
        SyncContext::new(peer, epoch),
    )
    .expect("派生 sync intent 失败");
    append_updates_with_sync_intents(
        &created.vault,
        &seed.body_doc_id,
        head.head_seq,
        &[UpdateRecord::new(intent.batch_id.clone(), update)],
        &[intent],
    )
    .expect("正文 + intent 事务失败");
    // 发布初始基线（两份 checkpoint；B 的 bootstrap 需要它）。
    let published =
        DevLocal::publish_initial_pair_with_identity(&ctx, &created.vault, &created.identity)
            .expect("A 发布基线失败");
    assert!(published.validated, "初始基线必须真实读回/导入通过");
    // A 的库句柄在夹具里不再需要（B 用自己的库；pending 夹具会重新打开）。
    drop(created.vault);
    RemoteCreator {
        identity: created.identity,
        vault_id: created.vault_id,
        recovery_code: created.recovery_code,
        vault_path,
    }
}

/// 打开 AppRoot 并注入 TLS relay 的自签根（只加显式信任根，不关闭证书校验）。
fn open_remote_root<'a>(
    cx: &'a mut TestAppContext,
    relay: &DevRelay,
    layout: &ProfileLayout,
    instance_id: &str,
) -> (Entity<AppRoot>, &'a mut VisualTestContext) {
    let options = AppRootOptions::for_test(layout.clone(), relay.base_url(), instance_id);
    let (root, cx) = open_root(cx, options);
    let ca = relay.tls_ca_der().expect("必须是 TLS relay").to_vec();
    root.update(cx, |root, _| root.set_test_relay_roots(vec![ca]));
    cx.run_until_parked();
    (root, cx)
}

/// 用 HTTPS 创建链接填好 remote Create 表单并提交（调用方负责等待终态）。
fn submit_remote_create(root: &Entity<AppRoot>, cx: &mut VisualTestContext, link: &str, dir: &str) {
    let link = link.to_owned();
    let dir = dir.to_owned();
    root.update(cx, |root, cx| {
        root.enter_create_link_form(cx);
        root.form_set_create_link(&link, cx);
        root.form_apply_create_link(cx);
        root.form_set_dir(&dir, cx);
        root.form_set_consent(true, cx);
        root.submit_create(cx);
    });
    cx.run_until_parked();
}

/// 填好 remote Join 表单并提交（调用方负责等待终态）。
fn submit_remote_join(
    root: &Entity<AppRoot>,
    cx: &mut VisualTestContext,
    relay_url: &str,
    vault_id: &str,
    recovery_code: &str,
    dir: &str,
) {
    let relay_url = relay_url.to_owned();
    let vault_id = vault_id.to_owned();
    let recovery_code = recovery_code.to_owned();
    let dir = dir.to_owned();
    root.update(cx, |root, cx| {
        root.enter_join_form(cx);
        // 没有模式单选项：https 地址在 submit_join 里自动推断为 remote_tls。
        root.join_form_set_relay(&relay_url, cx);
        root.join_form_set_vault_id(&vault_id, cx);
        root.join_form_set_recovery_code(&recovery_code, cx);
        root.join_form_set_dir(&dir, cx);
        root.submit_join(cx);
    });
    cx.run_until_parked();
}

/// 在已有库上写一条 **remote_tls** 的 pending（测试夹具；覆盖 HTTPS origin/模式编码）。
fn seed_remote_pending(
    vault_path: &Path,
    phase: menghunote_sync::init_journal::InitPhase,
    origin: &str,
    vault_id: [u8; 16],
    device_id: [u8; 16],
) {
    use menghunote_sync::init_journal::{
        advance_phase, begin_pending, load_pending, InitKind, InitPendingV1, InitPhase,
    };
    let vault = Vault::open(vault_path).expect("打开夹具库");
    // 已完成初始化的库没有 pending；这里直接写一条新的 remote pending（模拟崩溃窗口）。
    assert_eq!(
        load_pending(&vault, None).expect("读 pending"),
        None,
        "夹具库不应已有 pending"
    );
    let pending = InitPendingV1::prepared(
        InitKind::NewGenesis,
        RelayMode::RemoteTls,
        origin,
        vault_id,
        device_id,
        1,
    );
    begin_pending(&vault, &pending).expect("写 remote pending");
    match phase {
        InitPhase::Prepared => {}
        InitPhase::GenesisRequested => {
            advance_phase(&vault, InitPhase::Prepared, InitPhase::GenesisRequested)
                .expect("请求已发");
        }
        other => panic!("本夹具只支持 Prepared/GenesisRequested：{other:?}"),
    }
}

/// dev-sync 构建：remote_tls 不再要求「确认连接 HTTPS」；DEV 状态标注与「同意开发同步」
/// 仍保留（缺同意不建目录/不联网）。产品构建的等价断言见 `sync_ui_product.rs`。
#[gpui::test]
async fn dev_build_remote_mode_keeps_dev_notice_and_dev_consent_only(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let relay = DevRelay::start_tls().expect("起进程内 TLS relay 失败");
    let (_dir, layout) = temp_layout("remote-consent");
    let (root, cx) = open_remote_root(cx, &relay, &layout, "inst-remote-consent");
    assert_eq!(state_of(&root, cx), RootStateLabel::Welcome);

    let dir = layout
        .instance_dir("inst-remote-consent")
        .expect("合法实例 id")
        .display()
        .to_string();
    let link = create_remote_link_for(&relay, [0x71u8; 16], 900);
    root.update(cx, |root, cx| {
        root.enter_create_link_form(cx);
        root.form_set_create_link(&link, cx);
        root.form_apply_create_link(cx);
        root.form_set_dir(&dir, cx);
    });
    cx.run_until_parked();
    // dev-sync 构建保留 DEV 状态标注；但**不再**有"确认连接 HTTPS"勾选（用户反馈）。
    expect_bounds(cx, "app-root-remote-dev-notice");
    expect_bounds(cx, "app-root-consent");
    assert!(
        cx.debug_bounds("app-root-remote-consent").is_none(),
        "remote_tls 不再要求「确认连接 HTTPS」勾选"
    );

    // 缺「同意开发同步」→ 只有 DEV 构建才拒绝（产品构建没有这个勾选/校验）。
    root.update(cx, |root, cx| root.submit_create(cx));
    cx.run_until_parked();
    assert_eq!(state_of(&root, cx), RootStateLabel::CreateForm);
    let error = root
        .update(cx, |root, _| root.form_error().map(str::to_owned))
        .expect("缺 DEV 同意必须给表单错误");
    assert!(error.contains("同意开发同步"), "{error}");
    assert!(
        !layout
            .instance_dir("inst-remote-consent")
            .expect("目录")
            .exists(),
        "缺同意不得建实例目录/库"
    );
    assert!(!layout.config_path.exists(), "缺同意不得写配置");

    // 勾选 DEV 同意后提交：进后台并真实到达备份页（这里不需要远程 HTTPS 确认）。
    root.update(cx, |root, cx| {
        root.form_set_consent(true, cx);
        root.submit_create(cx);
    });
    cx.run_until_parked();
    assert_eq!(
        state_of(&root, cx),
        RootStateLabel::Working,
        "DEV 同意后必须直接进入后台（没有远程确认拦截）"
    );
    let finished = root.update(cx, |root, cx| {
        root.wait_for_create_terminal(cx, DEFAULT_OPERATION_BUDGET)
    });
    cx.run_until_parked();
    assert!(finished, "remote Create 必须在预算内到达终态");
    assert_eq!(state_of(&root, cx), RootStateLabel::BackupRequired);
}

/// remote Create 真实 TLS 全链路：genesis → 强 pin → 备份页 → Main → ProfileV2（remote_tls）
/// → 设置页指纹 → 重启自动 Main。
#[gpui::test]
async fn remote_create_over_tls_writes_remote_profile_pin_and_restarts(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let relay = DevRelay::start_tls().expect("起进程内 TLS relay 失败");
    let (_dir, layout) = temp_layout("remote-create");
    let instance_id = "inst-remote-create";
    let (root, cx) = open_remote_root(cx, &relay, &layout, instance_id);

    let dir = layout
        .instance_dir(instance_id)
        .expect("合法实例 id")
        .display()
        .to_string();
    let link = create_remote_link_for(&relay, [0x72u8; 16], 900);
    submit_remote_create(&root, cx, &link, &dir);
    assert_eq!(
        state_of(&root, cx),
        RootStateLabel::Working,
        "remote Create 必须真实进入后台（不再被 S3b 边界拒绝）"
    );
    let finished = root.update(cx, |root, cx| {
        root.wait_for_create_terminal(cx, DEFAULT_OPERATION_BUDGET)
    });
    cx.run_until_parked();
    assert!(finished, "remote Create 必须在预算内到达终态");
    assert_eq!(state_of(&root, cx), RootStateLabel::BackupRequired);
    let vault_id = root
        .update(cx, |root, _| root.vault_id().map(str::to_owned))
        .expect("备份页必须有 vault_id");
    let recovery_code = root
        .update(cx, |root, _| root.recovery_code().map(str::to_owned))
        .expect("备份页必须有恢复码");
    assert_eq!(vault_id.len(), 32);
    assert!(recovery_code.starts_with("mnrec1"));
    assert!(!layout.config_path.exists(), "备份确认前不得写配置");

    root.update(cx, |root, cx| {
        root.set_backup_confirmed(true, cx);
        root.confirm_backup(cx);
    });
    cx.run_until_parked();
    assert_eq!(state_of(&root, cx), RootStateLabel::Main);

    // 配置：ProfileV2 + remote_tls + canonical https origin（不降级 local_dev）。
    let profile = layout.store().load().expect("读配置").expect("必须有配置");
    let entry = profile.find(instance_id).expect("实例条目");
    assert_eq!(entry.relay_mode, RelayMode::RemoteTls);
    assert_eq!(entry.relay_url, relay.base_url());
    assert_eq!(entry.vault_id, vault_id);
    let raw = std::fs::read_to_string(&layout.config_path).expect("读配置原文");
    for forbidden in ["mnrec1", "recovery", "secret", "manager", "private"] {
        assert!(
            !raw.to_ascii_lowercase().contains(forbidden),
            "公开配置不得出现 {forbidden}：{raw}"
        );
    }

    // 强 pin：rev1 + record hash（本地证书复算，TLS 后验证链前缀）。
    let vault_path = layout.vault_path(instance_id).expect("库路径");
    let vault_id_bytes = menghunote_sync::ids::parse_hex::<16>(&vault_id).expect("vault id");
    let pin = {
        let vault = Vault::open(&vault_path).expect("打开 remote 实例库");
        menghunote_sync::control_pin::load_control_pin(&vault, vault_id_bytes)
            .expect("读 pin")
            .expect("remote Create 必须提交强 pin")
    };
    assert_eq!(pin.record.rev, 1);
    assert!(pin.record.hash.is_some(), "强 pin 必须带 record hash");
    assert!(pin.bootstrap.is_none(), "genesis 还没有历史锚");

    // 设置页安全指纹出现（只读，无自动信任按钮）。
    open_settings(&root, cx);
    open_settings_nav(cx, "security");
    expect_bounds(cx, "settings-fingerprint");
    let fingerprint = root
        .update(cx, |root, _| root.settings_fingerprint_state())
        .expect("设置页必须有指纹快照");
    match fingerprint {
        menghunote_app::onboarding::FingerprintState::Pin(pin) => {
            assert_eq!(pin.mode, RelayMode::RemoteTls);
            assert_eq!(pin.origin, relay.base_url());
            assert_eq!(pin.record_rev, 1);
            assert!(pin.record_hash.is_some());
        }
        other => panic!("remote Create 后必须显示 pin 指纹：{other:?}"),
    }
    root.update(cx, |root, cx| root.close_settings(cx));
    cx.run_until_parked();

    // 重启：同一份 remote 配置 + 身份 + pin → 自动 Main。
    assert!(cx.simulate_close(), "Main 状态允许关窗");
    cx.run_until_parked();
    let restarted = cx.update(|window, cx| {
        let options = AppRootOptions::for_test(layout.clone(), relay.base_url(), instance_id);
        window.replace_root(cx, move |_window, cx| AppRoot::new(options, cx))
    });
    cx.run_until_parked();
    assert_eq!(state_of(&restarted, cx), RootStateLabel::Main);
    assert_eq!(
        restarted.update(cx, |root, _| root
            .active_entry()
            .map(|entry| entry.relay_mode)),
        Some(RelayMode::RemoteTls)
    );
}

/// remote Join 真实 TLS 全链路：A（create_remote + 基线）→ B 用恢复码 Join → Main
/// 自动打开 A 的笔记 → ProfileV2（remote_tls）→ pin（record + bootstrap 同视图）→ 重启。
#[gpui::test]
async fn remote_join_over_tls_opens_creator_note_and_writes_remote_profile(
    cx: &mut TestAppContext,
) {
    ensure_test_user_key();
    let relay = DevRelay::start_tls().expect("起进程内 TLS relay 失败");
    let (_dir, layout) = temp_layout("remote-join");
    let creator = create_remote_creator(&relay, &layout, "inst-remote-a");
    let instance_id = "inst-remote-b";
    let (root, cx) = open_remote_root(cx, &relay, &layout, instance_id);

    let dir = layout
        .instance_dir(instance_id)
        .expect("合法实例 id")
        .display()
        .to_string();
    submit_remote_join(
        &root,
        cx,
        relay.base_url(),
        &creator.vault_id_hex(),
        &creator.recovery_code,
        &dir,
    );
    assert_eq!(state_of(&root, cx), RootStateLabel::Working);
    let finished = root.update(cx, |root, cx| {
        root.wait_for_join_terminal(cx, DEFAULT_OPERATION_BUDGET)
    });
    cx.run_until_parked();
    assert!(finished, "remote Join 必须在预算内到达终态");
    assert_eq!(
        state_of(&root, cx),
        RootStateLabel::Main,
        "remote Join 成功必须直接进 Main"
    );
    // 自动打开 A 的笔记（真实基线导入，不是 mock）。
    let shell = root
        .update(cx, |root, _| root.main_shell().cloned())
        .expect("Main 必须有真实 AppShell");
    let text = cx
        .update(|_window, app| shell.read(app).dev_sync_current_text(app))
        .expect("Join 后必须自动打开第一篇");
    assert!(
        text.contains("A 的远端正文"),
        "正文必须来自 A 的基线：{text:?}"
    );

    // 配置：remote_tls + canonical origin。
    let profile = layout.store().load().expect("读配置").expect("必须有配置");
    let entry = profile.find(instance_id).expect("实例条目");
    assert_eq!(entry.relay_mode, RelayMode::RemoteTls);
    assert_eq!(entry.relay_url, relay.base_url());
    assert_eq!(entry.vault_id, creator.vault_id_hex());

    // pin：record hash + bootstrap 锚（同 rev），说明 Join 用了同一验证视图。
    let b_vault_path = layout.vault_path(instance_id).expect("库路径");
    let pin = {
        let vault = Vault::open(&b_vault_path).expect("打开 B 的库");
        menghunote_sync::control_pin::load_control_pin(&vault, creator.vault_id)
            .expect("读 pin")
            .expect("remote Join 必须提交 durable pin")
    };
    assert!(pin.record.hash.is_some(), "Join 必须提交 record hash");
    let anchor = pin.bootstrap.expect("Join 必须带 bootstrap 锚");
    assert_eq!(anchor.rev, pin.record.rev, "record 与 bootstrap 必须同视图");

    // 设置页指纹出现。
    open_settings(&root, cx);
    open_settings_nav(cx, "security");
    expect_bounds(cx, "settings-fingerprint");
    root.update(cx, |root, cx| root.close_settings(cx));
    cx.run_until_parked();

    // 重启 → Main。
    assert!(cx.simulate_close(), "Main 状态允许关窗");
    cx.run_until_parked();
    let restarted = cx.update(|window, cx| {
        let options = AppRootOptions::for_test(layout.clone(), relay.base_url(), instance_id);
        window.replace_root(cx, move |_window, cx| AppRoot::new(options, cx))
    });
    cx.run_until_parked();
    assert_eq!(state_of(&restarted, cx), RootStateLabel::Main);
}

/// remote 失败路径：未信任 TLS / 无效票据 → 不建库/不写配置；错误提示安全暂停。
#[gpui::test]
async fn remote_create_failures_leave_no_vault_or_profile(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let relay = DevRelay::start_tls().expect("起进程内 TLS relay 失败");
    let (_dir, layout) = temp_layout("remote-create-fail");

    // (a) 未注入自签根 = 系统根拒绝 → TLS 失败且不建库。
    let instance_id = "inst-remote-untrusted";
    let options = AppRootOptions::for_test(layout.clone(), relay.base_url(), instance_id);
    let (root, cx) = open_root(cx, options);
    cx.run_until_parked();
    let dir = layout
        .instance_dir(instance_id)
        .expect("合法实例 id")
        .display()
        .to_string();
    let link = create_remote_link_for(&relay, [0x73u8; 16], 900);
    submit_remote_create(&root, cx, &link, &dir);
    let finished = root.update(cx, |root, cx| {
        root.wait_for_create_terminal(cx, DEFAULT_OPERATION_BUDGET)
    });
    cx.run_until_parked();
    assert!(finished);
    assert_eq!(state_of(&root, cx), RootStateLabel::Error);
    let title = root
        .update(cx, |root, _| root.error_title().map(str::to_owned))
        .expect("Error 页必须有标题");
    assert!(
        title.contains("安全暂停"),
        "TLS 失败必须标安全暂停：{title}"
    );
    let message = root
        .update(cx, |root, _| root.error_message().map(str::to_owned))
        .expect("Error 页必须有文案");
    assert!(message.contains("TLS"), "{message}");
    assert!(
        !layout.vault_path(instance_id).expect("库路径").exists(),
        "TLS 失败不得留下库文件"
    );
    assert!(!layout.config_path.exists(), "失败不得写配置");

    // (b) 注入自签根但票据无效 → relay 明确 403；同样不建库/不写配置。
    let instance_id = "inst-remote-bad-ticket";
    let (root, cx) = open_remote_root(cx, &relay, &layout, instance_id);
    let dir = layout
        .instance_dir(instance_id)
        .expect("合法实例 id")
        .display()
        .to_string();
    let bogus_ticket = CreateTicket::from_bytes([0x66u8; 32]);
    let link = CreateLink::encode(
        RelayMode::RemoteTls,
        relay.base_url(),
        [0x74u8; 16],
        bogus_ticket,
        None,
    )
    .expect("编码无效票据链接失败")
    .url;
    submit_remote_create(&root, cx, &link, &dir);
    let finished = root.update(cx, |root, cx| {
        root.wait_for_create_terminal(cx, DEFAULT_OPERATION_BUDGET)
    });
    cx.run_until_parked();
    assert!(finished);
    assert_eq!(state_of(&root, cx), RootStateLabel::Error);
    assert!(
        !layout.vault_path(instance_id).expect("库路径").exists(),
        "无效票据不得留下库文件"
    );
    assert!(!layout.config_path.exists(), "失败不得写配置");
}

/// remote Join 失败：错（格式合法）恢复码 → 不建库/不写配置、保留表单公开字段。
#[gpui::test]
async fn remote_join_with_wrong_recovery_code_keeps_form_and_leaves_no_vault(
    cx: &mut TestAppContext,
) {
    ensure_test_user_key();
    let relay = DevRelay::start_tls().expect("起进程内 TLS relay 失败");
    let (_dir, layout) = temp_layout("remote-join-fail");
    let creator = create_remote_creator(&relay, &layout, "inst-remote-join-a");
    let instance_id = "inst-remote-join-b";
    let (root, cx) = open_remote_root(cx, &relay, &layout, instance_id);

    let wrong_code = RecoverySeed::generate(&mut OsRandom)
        .expect("生成错误种子")
        .to_code()
        .expect("编码错误恢复码")
        .as_str()
        .to_owned();
    let dir = layout
        .instance_dir(instance_id)
        .expect("合法实例 id")
        .display()
        .to_string();
    submit_remote_join(
        &root,
        cx,
        relay.base_url(),
        &creator.vault_id_hex(),
        &wrong_code,
        &dir,
    );
    let finished = root.update(cx, |root, cx| {
        root.wait_for_join_terminal(cx, DEFAULT_OPERATION_BUDGET)
    });
    cx.run_until_parked();
    assert!(finished);
    assert_eq!(
        state_of(&root, cx),
        RootStateLabel::JoinForm,
        "失败必须回 Join 表单（可重试）"
    );
    let error = root
        .update(cx, |root, _| root.join_form_error().map(str::to_owned))
        .expect("必须给表单错误");
    assert!(
        error.contains("恢复码") || error.contains("relay 拒绝"),
        "{error}"
    );
    assert!(
        !error.contains(&wrong_code),
        "错误文案不得回显恢复码：{error}"
    );
    assert_eq!(
        root.update(cx, |root, cx| root.join_form_vault_id(cx)),
        Some(creator.vault_id_hex())
    );
    assert!(
        !layout.vault_path(instance_id).expect("库路径").exists(),
        "错恢复码不得留下库文件"
    );
    assert!(!layout.config_path.exists(), "失败不得写配置");
}

/// 启动对账（remote）：`Prepared` pending 在**不联网**的情况下安全放弃，然后正常进 Main。
#[gpui::test]
async fn startup_remote_pending_prepared_is_aborted_without_network(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let relay = DevRelay::start_tls().expect("起进程内 TLS relay 失败");
    let (_dir, layout) = temp_layout("remote-pending-prepared");
    let creator = create_remote_creator(&relay, &layout, "inst-remote-pending");
    // 夹具 A 已经完成初始化；再手工塞一条 Prepared 的 remote pending（模拟写前记录后崩溃）。
    seed_remote_pending(
        &creator.vault_path,
        menghunote_sync::init_journal::InitPhase::Prepared,
        relay.base_url(),
        creator.vault_id,
        creator.identity.device_id,
    );
    // 公开配置按 remote 模式登记（remote Create 成功后 app 会写这份配置）。
    let mut profile = DevSyncProfile::default();
    profile.upsert_active(InstanceEntry {
        instance_id: "inst-remote-pending".to_owned(),
        display_name: None,
        relay_mode: RelayMode::RemoteTls,
        api_version: RelayApiVersion::V1,
        relay_url: relay.base_url().to_owned(),
        vault_path: creator.vault_path.to_string_lossy().into_owned(),
        vault_id: creator.vault_id_hex(),
        device_id: menghunote_sync::ids::hex_of(&creator.identity.device_id),
    });
    layout.store().save(&profile).expect("写配置失败");

    let options = AppRootOptions::for_test(
        layout.clone(),
        relay.base_url().to_owned(),
        "inst-remote-pending",
    );
    let (root, cx) = open_root(cx, options);
    cx.run_until_parked();
    assert_eq!(
        state_of(&root, cx),
        RootStateLabel::Main,
        "Prepared pending 必须被安全放弃（不联网、不重建）后进 Main"
    );
    let vault = Vault::open(&creator.vault_path).expect("重开库");
    assert_eq!(
        menghunote_sync::init_journal::load_pending(&vault, None).expect("读 pending"),
        None,
        "abort 后不得留下 pending"
    );
}

/// 启动对账：`Prepared`（请求未发）= 安全放弃；无身份 → Error 恢复页（不重建/不删库）。
#[gpui::test]
async fn startup_pending_prepared_is_aborted_and_reports_missing_identity(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let (_dir, layout) = temp_layout("pending-prepared");
    let created = create_instance(&relay, &layout, "inst-prep");
    // 崩溃点：写前记录已落盘，但身份文件还没写。
    std::fs::remove_file(menghunote_app::dev_sync::DevIdentityStore::path_for(
        &created.vault_path,
    ))
    .expect("删除身份");
    seed_pending(
        &created.vault_path,
        menghunote_sync::init_journal::InitPhase::Prepared,
        &created,
    );
    write_profile(&layout, &created, "inst-prep");
    let before_len = std::fs::metadata(&created.vault_path)
        .expect("库存在")
        .len();

    let options = AppRootOptions::for_test(layout.clone(), relay.base_url(), "inst-prep");
    let (root, cx) = open_root(cx, options);
    cx.run_until_parked();
    assert_eq!(state_of(&root, cx), RootStateLabel::Error);
    assert!(
        !root.update(cx, |root, _| root.has_main_shell()),
        "对账失败不得启动同步 worker"
    );
    assert!(
        pending_phase(&created.vault_path).is_none(),
        "Prepared 必须在请求发出前安全放弃（清 pending）"
    );
    let after_len = std::fs::metadata(&created.vault_path)
        .expect("库仍在")
        .len();
    assert!(
        after_len >= before_len,
        "放弃 pending 不得截断/重建用户库（{before_len} → {after_len}）"
    );
    let message = root
        .update(cx, |root, _| root.error_message().map(str::to_owned))
        .expect("必须显示错误");
    assert!(message.contains("身份"), "{message}");
}

/// 启动对账：`Done` 只清理 pending，随后正常进入 Main（不联网）。
#[gpui::test]
async fn startup_pending_done_is_cleared_and_opens_main(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let (_dir, layout) = temp_layout("pending-done");
    let created = create_instance(&relay, &layout, "inst-done");
    seed_pending(
        &created.vault_path,
        menghunote_sync::init_journal::InitPhase::Done,
        &created,
    );
    write_profile(&layout, &created, "inst-done");
    let options = AppRootOptions::for_test(layout.clone(), relay.base_url(), "inst-done");
    let (root, cx) = open_root(cx, options);
    cx.run_until_parked();
    assert_eq!(state_of(&root, cx), RootStateLabel::Main);
    assert!(root.update(cx, |root, _| root.has_main_shell()));
    assert!(
        pending_phase(&created.vault_path).is_none(),
        "Done 必须清理 pending"
    );
}

/// 启动对账：请求已发 + 有身份 → 用已有材料补 finalize（不重复 genesis），随后 Done。
#[gpui::test]
async fn startup_pending_requested_with_identity_finishes_and_opens(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let (_dir, layout) = temp_layout("pending-pin");
    let created = create_instance(&relay, &layout, "inst-pin");
    seed_pending(
        &created.vault_path,
        menghunote_sync::init_journal::InitPhase::PinCommitted,
        &created,
    );
    write_profile(&layout, &created, "inst-pin");
    let options = AppRootOptions::for_test(layout.clone(), relay.base_url(), "inst-pin");
    let (root, cx) = open_root(cx, options);
    cx.run_until_parked();
    assert_eq!(
        state_of(&root, cx),
        RootStateLabel::Main,
        "已请求 + 有身份必须能补 finalize 并进入 Main"
    );
    assert!(
        pending_phase(&created.vault_path).is_none(),
        "finalize 后必须 Done + 清理"
    );
    assert!(created.vault_path.is_file());
    assert!(menghunote_app::dev_sync::DevIdentityStore::path_for(&created.vault_path).is_file());
}

/// 启动对账：损坏 pending → Error（PinCorrupt/ConfigInvalid 类），不自动重建/删库。
#[gpui::test]
async fn startup_corrupt_pending_lands_on_error_without_deleting_data(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let (_dir, layout) = temp_layout("pending-corrupt");
    let created = create_instance(&relay, &layout, "inst-bad-pending");
    corrupt_pending(&created.vault_path);
    write_profile(&layout, &created, "inst-bad-pending");
    let options = AppRootOptions::for_test(layout.clone(), relay.base_url(), "inst-bad-pending");
    let (root, cx) = open_root(cx, options);
    cx.run_until_parked();
    assert_eq!(state_of(&root, cx), RootStateLabel::Error);
    assert!(!root.update(cx, |root, _| root.has_main_shell()));
    let message = root
        .update(cx, |root, _| root.error_message().map(str::to_owned))
        .expect("必须显示错误");
    assert!(message.contains("对账失败"), "{message}");
    assert!(created.vault_path.is_file(), "不得删除用户库");
    assert!(
        menghunote_app::dev_sync::DevIdentityStore::path_for(&created.vault_path).is_file(),
        "不得删除用户身份"
    );
}

/// 无 pending 的普通启动：不因对账联网（relay 指向未监听端口也能进入 Main）。
#[gpui::test]
async fn startup_without_pending_does_not_contact_the_relay(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let (_dir, layout) = temp_layout("no-pending-offline");
    let created = create_instance(&relay, &layout, "inst-offline");
    assert!(
        pending_phase(&created.vault_path).is_none(),
        "Create 成功后 pending 必须已清理"
    );
    // 配置指向一个当前没有监听的 loopback 端口：若启动对账擅自联网，这里会失败。
    let mut offline = created.clone();
    offline.relay_url = free_loopback_url();
    write_profile(&layout, &offline, "inst-offline");
    let options =
        AppRootOptions::for_test(layout.clone(), offline.relay_url.clone(), "inst-offline");
    let (root, cx) = open_root(cx, options);
    cx.run_until_parked();
    assert_eq!(
        state_of(&root, cx),
        RootStateLabel::Main,
        "无 pending 时启动对账必须纯 no-op（不联网）"
    );
    assert!(root.update(cx, |root, _| root.has_main_shell()));
}

// ---------------------------------------------------------------------------
// 5) 统一表单「仓库名称」：Create 成功写 display_name，三处显示同步，不改路径
// ---------------------------------------------------------------------------

#[gpui::test]
async fn welcome_sync_create_writes_display_name_and_shows_it(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let (_dir, layout) = temp_layout("create-display-name");
    let instance_id = "inst-create-name";
    let options = AppRootOptions {
        layout: layout.clone(),
        default_relay: relay.base_url().to_owned(),
        instance_id: Some(instance_id.to_owned()),
        allow_local_vault: false,
        operation_budget: DEFAULT_OPERATION_BUDGET,
    };
    let (root, cx) = open_root(cx, options);
    cx.run_until_parked();
    assert_eq!(state_of(&root, cx), RootStateLabel::Welcome);

    // 展开统一同步卡：链接 / 实例目录 / 可选的「仓库名称」。
    let card = expect_bounds(cx, "welcome-sync-remote");
    cx.simulate_click(card.center(), Modifiers::default());
    cx.run_until_parked();
    expect_bounds(cx, "welcome-sync-link");
    expect_bounds(cx, "welcome-sync-dir");
    expect_bounds(cx, "welcome-sync-name");
    assert_eq!(
        root.update(cx, |root, cx| root.welcome_sync_name_text(cx)),
        Some(String::new()),
        "名称默认留空（= 使用目录名）"
    );

    // 填名称（含首尾空白/中文/emoji）→ 真实 Create → 备份 → Main。
    let vault_id = [0x6Cu8; 16];
    let link = create_link_for(&relay, vault_id, 900);
    let instance_dir = layout.instance_dir(instance_id).expect("合法实例 id");
    let instance_dir_text = instance_dir.display().to_string();
    root.update(cx, |root, cx| {
        root.welcome_set_sync_link(&link, cx);
        root.welcome_set_sync_dir(&instance_dir_text, cx);
        root.welcome_set_sync_name("  研究 库🚀  ", cx);
        root.welcome_set_sync_consent(true, cx);
    });
    assert_eq!(
        root.update(cx, |root, cx| root.welcome_sync_name_text(cx)),
        Some("研究 库🚀".to_owned())
    );
    root.update(cx, |root, cx| root.welcome_confirm_sync(cx));
    cx.run_until_parked();
    assert_eq!(state_of(&root, cx), RootStateLabel::Working);
    assert!(
        root.update(cx, |root, cx| root
            .wait_for_create_terminal(cx, DEFAULT_OPERATION_BUDGET)),
        "Create 必须在预算内收敛"
    );
    cx.run_until_parked();
    assert_eq!(state_of(&root, cx), RootStateLabel::BackupRequired);
    root.update(cx, |root, cx| {
        root.set_backup_confirmed(true, cx);
        root.confirm_backup(cx);
    });
    cx.run_until_parked();
    assert_eq!(state_of(&root, cx), RootStateLabel::Main);

    // 公开配置：display_name = trim 后的名字；目录/路径/vault_id 是真实事实。
    let profile = layout.store().load().expect("读配置").expect("配置存在");
    let entry = profile.find(instance_id).expect("实例已登记");
    assert_eq!(
        entry.display_name.as_deref(),
        Some("研究 库🚀"),
        "Create 成功时必须写 display_name（trim）"
    );
    assert_eq!(
        PathBuf::from(&entry.vault_path),
        instance_dir.join(VAULT_FILE_NAME),
        "名称只影响显示：目标路径仍是用户选的实例目录"
    );
    assert_eq!(profile.active_instance.as_deref(), Some(instance_id));

    // 三处显示同名字：左下角按钮 / 切换菜单当前行 / 管理仓库行。
    assert_eq!(
        root.update(cx, |root, cx| root.current_vault_name(cx)),
        "研究 库🚀",
        "左下角必须显示自定义名"
    );
    let switcher = expect_bounds(cx, "app-root-vault-switcher");
    cx.simulate_click(switcher.center(), Modifiers::default());
    cx.run_until_parked();
    let rows = root.update(cx, |root, cx| root.vault_menu_rows(cx));
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].label, "✓ 研究 库🚀", "菜单当前行必须同名");
    let manage = expect_bounds(cx, "app-root-vault-manage");
    cx.simulate_click(manage.center(), Modifiers::default());
    cx.run_until_parked();
    let manager = root.update(cx, |root, _| root.vault_manager_rows());
    assert_eq!(manager.len(), 1);
    assert_eq!(manager[0].name, "研究 库🚀", "管理仓库必须显示自定义名");
    assert_eq!(manager[0].path, entry.vault_path, "管理仓库仍显示真实路径");

    // 留空 = None：行内重命名清空后再保存，显示回退目录名。
    let rename = expect_bounds(cx, "vault-manager-rename-0");
    cx.simulate_click(rename.center(), Modifiers::default());
    cx.run_until_parked();
    root.update(cx, |root, cx| root.vault_rename_set_text("", cx));
    let save = expect_bounds(cx, "vault-manager-rename-save");
    cx.simulate_click(save.center(), Modifiers::default());
    cx.run_until_parked();
    let cleared = layout.store().load().expect("读配置").expect("配置存在");
    assert_eq!(
        cleared
            .find(instance_id)
            .and_then(|e| e.display_name.clone()),
        None,
        "留空保存 = 清除自定义名（落盘为 null/缺省）"
    );
    assert_eq!(
        root.update(cx, |root, cx| root.current_vault_name(cx)),
        instance_id,
        "清除后左下角回退目录名"
    );
}
