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
    ProfileStore, RootStateLabel, DEFAULT_OPERATION_BUDGET, LOCK_FILE_NAME,
};
use menghunote_dev_relay::DevRelay;
use menghunote_storage::{
    append_updates_with_sync_intents, list_tree, load_body_doc, local_sync_intent, StructureWriter,
    SyncContext, UpdateRecord, Vault,
};
use menghunote_sync::dev_local::DevLocal;
use menghunote_sync::identity::LocalIdentityV1;
use menghunote_sync::random::OsRandom;
use menghunote_sync::recovery::RecoverySeed;
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
        dir.path().join("config/dev-sync.json"),
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
        "app-root-create",
        "app-root-join",
        "app-root-instances",
    ] {
        expect_bounds(cx, selector);
    }
    // 没有 active 实例：Main 与备份页都不存在；for_test 关闭了「继续本地」入口。
    assert!(cx.debug_bounds("app-root-main").is_none());
    assert!(cx.debug_bounds("app-root-backup").is_none());
    assert!(
        cx.debug_bounds("app-root-local").is_none(),
        "for_test 明确关闭本地 dev vault 入口"
    );
    // 没有配置时不该凭空创建库/身份/配置。
    assert!(
        !layout.config_path.exists(),
        "读不到配置不算错误，也不得写配置"
    );
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

    // ---- 真实点击「创建新的同步实例」→ 表单 ----
    let create = expect_bounds(cx, "app-root-create");
    cx.simulate_click(create.center(), Modifiers::default());
    cx.run_until_parked();
    assert_eq!(
        state_of(&root, cx),
        RootStateLabel::CreateForm,
        "点击「创建」必须真实进入表单"
    );
    let create_form = expect_bounds(cx, "app-root-create-form");
    assert!(
        create_form.size.height.as_f32() > 0.0,
        "表单必须有实际布局：{create_form:?}"
    );
    for selector in [
        "app-root-relay-input",
        "app-root-secret-input",
        "app-root-dir-input",
        "app-root-consent",
        "app-root-submit",
        "app-root-back",
    ] {
        expect_bounds(cx, selector);
    }

    // ---- 未勾选同意：提交被拒（留在表单 + 明确原因），不触发任何网络 ----
    let instance_dir = layout
        .instance_dir(instance_id)
        .expect("合法实例 id")
        .display()
        .to_string();
    root.update(cx, |root, cx| {
        root.form_set_relay(relay.base_url(), cx);
        root.form_set_secret(relay.setup_secret(), cx);
        root.form_set_dir(&instance_dir, cx);
        root.submit_create(cx);
    });
    cx.run_until_parked();
    assert_eq!(state_of(&root, cx), RootStateLabel::CreateForm);
    assert!(
        cx.debug_bounds("app-root-form-error").is_some(),
        "未勾选同意必须在表单里给出错误，而不是悄悄开始创建"
    );

    // ---- 勾选同意 → 提交 → Working（后台线程；UI 不阻塞） ----
    root.update(cx, |root, cx| {
        root.form_set_consent(true, cx);
        root.submit_create(cx);
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
    root.update(cx, |root, cx| {
        root.enter_create_form(cx);
        root.form_set_relay(relay.base_url(), cx);
        root.form_set_secret(relay.setup_secret(), cx);
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

    // ---- 真实点击「加入已有同步实例」→ JoinForm ----
    let join = expect_bounds(cx, "app-root-join");
    cx.simulate_click(join.center(), Modifiers::default());
    cx.run_until_parked();
    assert_eq!(
        state_of(&root, cx),
        RootStateLabel::JoinForm,
        "点击「加入」必须真实进入 Join 表单（不再是「即将支持」）"
    );
    for selector in [
        "app-root-join-form",
        "app-root-join-relay-input",
        "app-root-vault-id-input",
        "app-root-recovery-input",
        "app-root-join-dir-input",
        "app-root-join-submit",
        "app-root-join-back",
    ] {
        expect_bounds(cx, selector);
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

    // 选一个当前没人监听的 loopback 端口（绑定后立刻释放）：连接会被拒绝。
    let dead_relay = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("绑定临时端口失败");
        let addr = listener.local_addr().expect("取本地地址失败");
        drop(listener);
        format!("http://{addr}")
    };

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
    for selector in [
        "dev-sync-settings",
        "settings-relay",
        "settings-save",
        "settings-cancel",
        "settings-instance-0",
        "settings-instance-0-active",
        "settings-error",
        "settings-notice",
    ] {
        expect_bounds(cx, selector);
    }
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
    let cancel = expect_bounds(cx, "settings-cancel");
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
    let cancel = expect_bounds(cx, "settings-cancel");
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
        ("https://127.0.0.1:18080", "loopback"),
        ("http://relay.example:18080", "loopback"),
        ("http://192.168.1.9:18080", "loopback"),
        ("http://0.0.0.0:18080", "loopback"),
        ("http://user:pass@127.0.0.1:18080", "loopback"),
        ("http://127.0.0.1:18080/api", "传输策略"),
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
    // 真实点击第二行（active 优先排序后 A 在前、B 在后）的「设为默认」。
    let set_active = expect_bounds(cx, "settings-instance-1-active");
    cx.simulate_click(set_active.center(), Modifiers::default());
    cx.run_until_parked();
    let notice = root
        .update(cx, |root, _| root.settings_notice().map(str::to_owned))
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
    assert_eq!(
        root.update(cx, |root, cx| root.settings_relay(cx)),
        Some(created_b.relay_url.clone()),
        "切换 active 后 relay 输入框必须跟到新 active（避免写错实例）"
    );

    // 重启 → 打开新 active B。
    let cancel = expect_bounds(cx, "settings-cancel");
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
    let cancel = expect_bounds(cx, "settings-cancel");
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
