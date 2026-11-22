//! ADR 0009 Phase 2 / 切片 6a：**轮换接到 app 机制层**（身份写回 + worker
//! `Rotate`/`ApplyEpoch` + 成功后重建活动视图）。
//!
//! 四个用例都用**真实** `DevRelay`（进程内 axum + 临时 SQLite + 放行验签的测试脚手架，
//! ⚠️ 非生产）与真实 HTTP：
//!
//! 1. `rotate_then_apply_keeps_both_sides_editing_in_the_new_epoch`：A `Rotate` → E2，
//!    B `ApplyEpoch` → E2；双方继续编辑并同步可见；**活动视图被重建**，新编辑的
//!    `sync_outbox.key_epoch = 2`（读库里真实的意图行，不看内存字段）。
//! 2. `recovery_code_option_controls_whether_new_devices_can_join`：`Some(当前恢复码)`
//!    轮换后新设备 C 能用同一恢复码 Join 到 E2；`None` 再轮换到 E3 后 Join 明确失败。
//! 3. `rotation_freezes_input_structure_and_switch_and_failure_keeps_old_state`：黑洞 relay
//!    让轮换停在"持闸 + worker 在途"：输入/结构写入/文档切换全部冻结；失败后闸门释放、
//!    身份文件逐字节不变、旧视图保留、输入/切换恢复。
//! 4. `revoked_device_cannot_apply_and_stays_on_old_epoch`：A 轮换时吊销 B；B 的
//!    `ApplyEpoch` 明确拒绝，身份/视图保持 E1（不半推广）。
//!
//! ## 纪律（AGENTS.md §4/§6）
//!
//! - 临时目录一律 `CARGO_TARGET_TMPDIR`（`target/tmp`），不用 `%TEMP%`；
//! - **不 sleep**：收敛判据是 `dev_sync_rotation_in_progress()` 转 false、`Completed`
//!   回执、闸门 `Held`/释放这些**状态**；忙等只用 `yield_now` + 有界 deadline；
//! - Linux 用户主键的 `XDG_DATA_HOME` 也指到 `target/tmp`（Windows 是 no-op）；
//! - relay 只用于 dev：本文件**不能**作为鉴权/验签验收。
//!
//! feature 关闭时整个文件编译为空（0 个测试）。

#![cfg(feature = "dev-sync")]

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use gpui::{
    div, AppContext as _, Context, Entity, InteractiveElement as _, Modifiers, ParentElement as _,
    Render, Styled as _, TestAppContext, VisualTestContext,
};
use menghunote_app::dev_sync::gate::DevSyncConfig;
use menghunote_app::dev_sync::service::ServiceConfig;
use menghunote_app::dev_sync::setup::{self, SetupMode, SetupSecrets};
use menghunote_app::dev_sync::{
    DevIdentityStore, DevSyncPanel, SyncOnceReport, APPLY_CONFIRM_TEXT, ROTATE_CONFIRM_TEXT,
    ROTATE_NO_RECOVERY_WARNING, ROTATE_RECOVERY_EMPTY_WARNING, ROTATE_STAGES_TEXT,
};
use menghunote_app::dev_vault;
use menghunote_app::shell::AppShell;
use menghunote_dev_relay::DevRelay;
use menghunote_storage::{
    list_tree, load_body_doc, outbox_rows, SyncContext, SyncOutboxRow, SyncOutboxState, Vault,
};
use menghunote_sync::dev_local::DevLocal;
use menghunote_sync::http_relay::{MemberStatus, MemberView};
use menghunote_sync::identity::LocalIdentityV1;
use menghunote_sync::ids;
use menghunote_sync::recovery::RecoverySeed;
use tempfile::{Builder as TempDirBuilder, TempDir};

/// 收敛的安全上限（真实 relay 是 loopback，正常毫秒级；超时说明状态机卡死）。
const DEADLINE: Duration = Duration::from_secs(60);

/// 测试进程级的 `XDG_DATA_HOME`（Linux 用户主键；Windows 是 no-op）。
fn ensure_test_user_key() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("dev-sync-rotation-xdg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("创建测试 XDG_DATA_HOME 失败");
        std::env::set_var("XDG_DATA_HOME", &dir);
        // 轮换是实验性高级入口，默认不在设备页出现；本文件显式打开。
        std::env::set_var("MENGHUNOTE_EXPERIMENTAL_ROTATION", "1");
        menghunote_sync::identity_wrap::initialize_user_key()
            .expect("初始化测试用户主键失败（ADR 0008）");
    });
}

/// 在 `CARGO_TARGET_TMPDIR`（`target/tmp`）下开一个独立临时目录。
fn temp_root(prefix: &str) -> TempDir {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("dev-sync-rotation-{prefix}-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("创建 target/tmp 下的测试根目录失败");
    TempDirBuilder::new()
        .prefix("case-")
        .tempdir_in(&root)
        .expect("在 target/tmp 下创建临时目录失败")
}

/// 窗口根视图：给 `AppShell` 一个稳定的宿主 selector。
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

fn config_for(relay_url: &str, vault_path: &Path) -> DevSyncConfig {
    DevSyncConfig {
        relay_mode: menghunote_app::onboarding::RelayMode::LocalDev,
        vault: vault_path.to_path_buf(),
        service: ServiceConfig::new(relay_url),
    }
}

fn create_secrets(relay: &DevRelay) -> SetupSecrets {
    SetupSecrets {
        setup_secret: Some(relay.setup_secret().to_owned()),
        ..SetupSecrets::default()
    }
}

// ---------------------------------------------------------------------------
// A/B 一对真实实例（Create → 播种 → 发布基线 → Join）
// ---------------------------------------------------------------------------

struct Pair {
    relay: DevRelay,
    _dir: TempDir,
    a_path: PathBuf,
    b_path: PathBuf,
    a_vault: Vault,
    b_vault: Vault,
    a_identity: LocalIdentityV1,
    a_sync: SyncContext,
    b_sync: SyncContext,
    /// 两边都打开的那篇笔记（先序第一篇）。
    doc_id: String,
    /// 该笔记初始正文（含演示标题）。
    seed_text: String,
    /// Create 返回的恢复码（只在本进程内传递，不写日志）。
    recovery_code: String,
}

/// Create A → 播种演示树 → 发布初始基线 → B 只凭 `vault_id + 恢复码` Join。
fn establish_pair(prefix: &str) -> Pair {
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let dir = temp_root(prefix);

    let a_path = dir.path().join("inst-a").join("vault.sqlite3");
    let a = setup::prepare(
        &config_for(relay.base_url(), &a_path),
        SetupMode::Create,
        create_secrets(&relay),
    )
    .expect("A Create 失败");
    let a_peer = a.identity.loro_peer_id();
    let a_epoch = a.identity.current_epoch;
    let a_vault_id = a.vault_id;
    let recovery = a.recovery_code.clone().expect("Create 必须返回恢复码");
    drop(a.vault);
    let dev_a = dev_vault::open_vault_at_with_peer(&a_path, a_peer).expect("A 打开实例库失败");
    assert!(dev_a.seeded, "A 的空库必须本次播种（演示树）");

    DevLocal::publish_initial_pair(relay.base_url(), &dev_a.vault, &a.identity, &recovery)
        .expect("A 发布初始基线失败");

    let b_path = dir.path().join("inst-b").join("vault.sqlite3");
    let b = setup::prepare(
        &config_for(relay.base_url(), &b_path),
        SetupMode::Join,
        SetupSecrets {
            vault_id: Some(ids::hex_of(&a_vault_id)),
            recovery_code: Some(recovery.clone()),
            ..SetupSecrets::default()
        },
    )
    .expect("B Join 失败");
    let b_peer = b.identity.loro_peer_id();
    let b_epoch = b.identity.current_epoch;
    drop(b.vault);
    let dev_b = dev_vault::open_vault_at_with_peer(&b_path, b_peer).expect("B 打开实例库失败");
    assert!(!dev_b.seeded, "B 的树来自基线，不得再播种");

    let entry = list_tree(&dev_a.vault)
        .expect("读 A 结构树失败")
        .into_iter()
        .find(|entry| entry.is_note())
        .expect("A 必须有第一篇笔记");
    let doc_id = entry.body_doc_id.clone().expect("笔记必须有 body_doc_id");
    let seed_text = load_body_doc(&dev_a.vault, &doc_id, a_peer, None)
        .expect("读 A 初始正文失败")
        .0
        .text();

    Pair {
        relay,
        _dir: dir,
        a_path,
        b_path,
        a_vault: dev_a.vault.clone(),
        b_vault: dev_b.vault.clone(),
        a_identity: a.identity,
        a_sync: SyncContext::new(a_peer, a_epoch),
        b_sync: SyncContext::new(b_peer, b_epoch),
        doc_id,
        seed_text,
        recovery_code: recovery,
    }
}

// ---------------------------------------------------------------------------
// AppShell 测试助手
// ---------------------------------------------------------------------------

fn add_shell_window(
    cx: &mut TestAppContext,
    vault: Vault,
    label: &'static str,
    dev_sync: DevSyncConfig,
    sync: SyncContext,
) -> (Entity<ShellHost>, gpui::AnyWindowHandle) {
    let slot = std::rc::Rc::new(std::cell::Cell::new(None));
    let captured = slot.clone();
    let (host, window_cx) = cx.add_window_view(move |window, cx| {
        captured.set(Some(window.window_handle()));
        let shell =
            cx.new(|cx| AppShell::new_with_dev_sync(vault, label.to_owned(), dev_sync, sync, cx));
        shell.update(cx, |shell, cx| shell.open_settings_headless(cx));
        ShellHost { shell }
    });
    let handle = slot
        .get()
        .or_else(|| window_cx.windows().first().copied())
        .expect("窗口句柄必须可取");
    (host, handle)
}

fn with_shell<R>(
    cx: &mut VisualTestContext,
    host: &Entity<ShellHost>,
    f: impl FnOnce(&mut AppShell, &mut Context<AppShell>) -> R,
) -> R {
    cx.update(|_window, app| {
        host.update(app, |host, cx| {
            host.shell.update(cx, |shell, cx| f(shell, cx))
        })
    })
}

fn read_shell<R>(
    cx: &mut VisualTestContext,
    host: &Entity<ShellHost>,
    f: impl FnOnce(&AppShell, &gpui::App) -> R,
) -> R {
    cx.update(|_window, app| f(host.read(app).shell.read(app), app))
}

fn current_text(cx: &mut VisualTestContext, host: &Entity<ShellHost>) -> String {
    read_shell(cx, host, |shell, app| shell.dev_sync_current_text(app))
        .expect("当前活动文档的镜像正文")
}

fn active_view(
    cx: &mut VisualTestContext,
    host: &Entity<ShellHost>,
) -> Option<Entity<menghunote_app::editor::view::EditorView>> {
    read_shell(cx, host, |shell, _app| shell.dev_sync_active_view())
}

fn active_held(cx: &mut VisualTestContext, host: &Entity<ShellHost>) -> bool {
    read_shell(cx, host, |shell, app| {
        shell
            .dev_sync_active_view()
            .is_some_and(|view| view.read(app).dev_sync_is_held())
    })
}

fn last_report(cx: &mut VisualTestContext, host: &Entity<ShellHost>) -> Option<SyncOnceReport> {
    read_shell(cx, host, |shell, _app| {
        shell.dev_sync_last_rotation().cloned()
    })
}

fn last_sync_report(
    cx: &mut VisualTestContext,
    host: &Entity<ShellHost>,
) -> Option<SyncOnceReport> {
    read_shell(cx, host, |shell, _app| shell.dev_sync_last_sync().cloned())
}

fn rotation_in_progress(cx: &mut VisualTestContext, host: &Entity<ShellHost>) -> bool {
    read_shell(cx, host, |shell, _app| {
        shell.dev_sync_rotation_in_progress()
    })
}

fn panel_rotation_line(cx: &mut VisualTestContext, host: &Entity<ShellHost>) -> Option<String> {
    read_shell(cx, host, |shell, app| {
        shell.dev_sync_panel().and_then(|panel| {
            panel
                .read(app)
                .sync_line_text()
                .map(std::borrow::ToOwned::to_owned)
        })
    })
}

fn panel_notice(cx: &mut VisualTestContext, host: &Entity<ShellHost>) -> Option<String> {
    read_shell(cx, host, |shell, app| {
        shell.dev_sync_panel().and_then(|panel| {
            panel
                .read(app)
                .notice_text()
                .map(std::borrow::ToOwned::to_owned)
        })
    })
}

fn focus_editor(cx: &mut VisualTestContext) {
    let pane = cx.debug_bounds("editor-pane").expect("编辑器面板 selector");
    cx.simulate_click(
        gpui::point(
            pane.origin.x + gpui::px(20.0),
            pane.origin.y + gpui::px(10.0),
        ),
        Modifiers::default(),
    );
    cx.run_until_parked();
}

/// 泵到"没有在途的持闸任务"（**状态收敛**，不是 sleep）。
fn drive(cx: &mut VisualTestContext, host: &Entity<ShellHost>) {
    let deadline = Instant::now() + DEADLINE;
    loop {
        with_shell(cx, host, |shell, cx| shell.dev_sync_pump(cx));
        cx.run_until_parked();
        let finished = read_shell(cx, host, |shell, _app| !shell.dev_sync_sync_in_progress());
        if finished {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "持闸任务 60s 内没有收敛（面板：{:?}，通知：{:?}）",
            panel_rotation_line(cx, host),
            panel_notice(cx, host)
        );
        std::thread::yield_now();
    }
}

/// 起一个"只 listen、不应答"的 relay：connect 成功，读响应永远阻塞。
struct BlackholeRelay {
    listener: TcpListener,
}

impl BlackholeRelay {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定黑洞 relay 端口失败");
        Self { listener }
    }

    fn url(&self) -> String {
        format!("http://{}", self.listener.local_addr().expect("黑洞地址"))
    }
}

/// 读身份文件的 epoch（不经过 app 身份缓存：磁盘才是 durable 真源）。
fn disk_epoch(vault_path: &Path) -> u64 {
    DevIdentityStore
        .load(vault_path)
        .expect("读取磁盘身份失败")
        .current_epoch
}

/// 泵到"本地新编辑已经写出 sync_outbox 意图"（状态收敛，不是 sleep）。
///
/// 为什么必须在同步**之前**读：durable ACK 会删除 outbox 行（`mark_acked`），同步成功后
/// 就再也看不到这条意图了。这里等的是编辑器 flush 自己写完行，属于明确状态。
fn wait_for_outbox_epoch(
    cx: &mut VisualTestContext,
    host: &Entity<ShellHost>,
    vault: &Vault,
    doc_id: &str,
) -> u64 {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        with_shell(cx, host, |shell, cx| shell.dev_sync_pump(cx));
        cx.run_until_parked();
        if let Some(epoch) = latest_outbox_epoch(vault, doc_id) {
            return epoch;
        }
        assert!(
            Instant::now() < deadline,
            "新编辑 10s 内没有写出 sync_outbox 意图（doc={doc_id}）"
        );
        std::thread::yield_now();
    }
}

/// 某篇正文文档在本地 outbox 里**最大 seq** 那一行的 `key_epoch`。
///
/// 轮换会重包旧行，但 `Sent` 历史行仍可能留在库里；"新编辑用什么 epoch"要看最新一行。
fn latest_outbox_epoch(vault: &Vault, doc_id: &str) -> Option<u64> {
    let mut rows: Vec<SyncOutboxRow> = Vec::new();
    for state in [
        SyncOutboxState::Pending,
        SyncOutboxState::Encrypted,
        SyncOutboxState::Sent,
    ] {
        rows.extend(outbox_rows(vault, state, 1000).expect("读 sync_outbox 失败"));
    }
    rows.into_iter()
        .filter(|row| row.doc_id == doc_id)
        .max_by_key(|row| (row.seq, row.message_id.clone()))
        .map(|row| row.key_epoch)
}

// ---------------------------------------------------------------------------
// 1) A rotate → E2；B apply → E2；双方继续编辑 + 同步可见；新意图 epoch=2
// ---------------------------------------------------------------------------

#[gpui::test]
async fn rotate_then_apply_keeps_both_sides_editing_in_the_new_epoch(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let pair = establish_pair("roundtrip");
    let relay_url = pair.relay.base_url().to_owned();

    let (host_a, a_handle) = add_shell_window(
        cx,
        pair.a_vault.clone(),
        "dev-sync-rotation-a",
        config_for(&relay_url, &pair.a_path),
        pair.a_sync,
    );
    let (host_b, b_handle) = add_shell_window(
        cx,
        pair.b_vault.clone(),
        "dev-sync-rotation-b",
        config_for(&relay_url, &pair.b_path),
        pair.b_sync,
    );
    let app_cx = cx.clone();
    let mut cx_a = VisualTestContext::from_window(a_handle, &app_cx);
    let mut cx_b = VisualTestContext::from_window(b_handle, &app_cx);
    cx_a.run_until_parked();
    cx_b.run_until_parked();

    let view_a_before = active_view(&mut cx_a, &host_a).expect("A 启动时必须打开第一篇");
    let view_b_before = active_view(&mut cx_b, &host_b).expect("B 启动时必须打开第一篇");
    assert_eq!(current_text(&mut cx_a, &host_a), pair.seed_text);
    assert_eq!(current_text(&mut cx_b, &host_b), pair.seed_text);

    // --- A：轮换到 E2（None = 不补发恢复码，报告必须明确警告）---
    with_shell(&mut cx_a, &host_a, |shell, cx| {
        shell.dev_sync_rotate(None, cx)
    })
    .unwrap_or_else(|error| panic!("A 轮换被拒绝：{error}"));
    drive(&mut cx_a, &host_a);
    let report_a = last_report(&mut cx_a, &host_a).expect("A 轮换必须留下报告");
    assert!(report_a.is_ok(), "A 轮换必须成功：{report_a:?}");
    let facts_a = report_a.rotation.as_ref().expect("轮换报告必须带公开事实");
    assert_eq!(facts_a.from_epoch, 1);
    assert_eq!(facts_a.to_epoch, Some(2));
    assert!(!facts_a.recovery_code_reissued, "None 不得声称补发恢复码");
    assert_eq!(facts_a.failure_phase, None);
    assert!(
        report_a.notice.contains("epoch 1 → 2"),
        "面板必须显示 epoch 迁移：{:?}",
        report_a.notice
    );
    assert!(
        report_a.notice.contains("无法用恢复码加入"),
        "None 必须明确警告新设备无法用恢复码加入：{:?}",
        report_a.notice
    );
    assert!(!active_held(&mut cx_a, &host_a), "轮换完成后闸门必须释放");
    assert!(
        cx_a.debug_bounds("editor-sync-held").is_none(),
        "轮换完成后不得残留 editor-sync-held"
    );
    // 活动视图必须被重建（旧 EditorView/SessionDriver 属于旧 epoch）。
    let view_a_after = active_view(&mut cx_a, &host_a).expect("轮换后必须重新打开笔记");
    assert_ne!(
        view_a_before, view_a_after,
        "轮换后必须重建活动视图（新 epoch 的 SessionDriver）"
    );
    assert_eq!(
        current_text(&mut cx_a, &host_a),
        pair.seed_text,
        "重建视图后正文必须与 durable 一致"
    );
    assert_eq!(
        disk_epoch(&pair.a_path),
        2,
        "受保护身份文件必须已写回新 epoch"
    );

    // --- B：apply 到 E2 ---
    with_shell(&mut cx_b, &host_b, |shell, cx| {
        shell.dev_sync_apply_epoch(cx)
    })
    .unwrap_or_else(|error| panic!("B apply 被拒绝：{error}"));
    drive(&mut cx_b, &host_b);
    let report_b = last_report(&mut cx_b, &host_b).expect("B apply 必须留下报告");
    assert!(report_b.is_ok(), "B apply 必须成功：{report_b:?}");
    let facts_b = report_b
        .rotation
        .as_ref()
        .expect("apply 报告必须带公开事实");
    assert_eq!(facts_b.from_epoch, 1);
    assert_eq!(facts_b.to_epoch, Some(2));
    assert!(!facts_b.already_current);
    assert!(
        report_b.notice.contains("已应用新 epoch") && report_b.notice.contains("1 → 2"),
        "面板必须显示 apply 的 epoch 迁移：{:?}",
        report_b.notice
    );
    assert!(!active_held(&mut cx_b, &host_b), "apply 完成后闸门必须释放");
    let view_b_after = active_view(&mut cx_b, &host_b).expect("apply 后必须重新打开笔记");
    assert_ne!(
        view_b_before, view_b_after,
        "apply 后必须重建活动视图（新 epoch 的 SessionDriver）"
    );
    assert_eq!(current_text(&mut cx_b, &host_b), pair.seed_text);
    assert_eq!(disk_epoch(&pair.b_path), 2, "B 身份文件必须写回新 epoch");

    // --- A 键入 rot-a → A 同步 → B 同步：B 不重开就看到文字 ---
    focus_editor(&mut cx_a);
    cx_a.simulate_keystrokes("end");
    cx_a.run_until_parked();
    cx_a.simulate_input("rot-a");
    cx_a.run_until_parked();
    assert!(current_text(&mut cx_a, &host_a).contains("rot-a"));
    // 新意图必须带 E2（在 ACK 删除 outbox 行之前读）。
    assert_eq!(
        wait_for_outbox_epoch(&mut cx_a, &host_a, &pair.a_vault, &pair.doc_id),
        2,
        "A 轮换后新编辑的 intent 必须带 epoch 2"
    );
    with_shell(&mut cx_a, &host_a, |shell, cx| shell.dev_sync_sync_once(cx))
        .unwrap_or_else(|error| panic!("A 轮换后同步被拒绝：{error}"));
    drive(&mut cx_a, &host_a);
    assert!(
        last_sync_report(&mut cx_a, &host_a).is_some_and(|report| report.is_ok()),
        "A 轮换后的同步必须成功"
    );

    with_shell(&mut cx_b, &host_b, |shell, cx| shell.dev_sync_sync_once(cx))
        .unwrap_or_else(|error| panic!("B 轮换后同步被拒绝：{error}"));
    drive(&mut cx_b, &host_b);
    assert!(
        last_sync_report(&mut cx_b, &host_b).is_some_and(|report| report.is_ok()),
        "B 轮换后的同步必须成功"
    );
    assert!(
        current_text(&mut cx_b, &host_b).contains("rot-a"),
        "B 必须看到 A 在 E2 写入的文字：{:?}",
        current_text(&mut cx_b, &host_b)
    );

    // --- B 键入 rot-b → B 同步 → A 同步：A 看到 ---
    focus_editor(&mut cx_b);
    cx_b.simulate_keystrokes("end");
    cx_b.run_until_parked();
    cx_b.simulate_input("rot-b");
    cx_b.run_until_parked();
    assert!(current_text(&mut cx_b, &host_b).contains("rot-b"));
    assert_eq!(
        wait_for_outbox_epoch(&mut cx_b, &host_b, &pair.b_vault, &pair.doc_id),
        2,
        "B apply 后新编辑的 intent 必须带 epoch 2"
    );
    with_shell(&mut cx_b, &host_b, |shell, cx| shell.dev_sync_sync_once(cx))
        .unwrap_or_else(|error| panic!("B 第二轮同步被拒绝：{error}"));
    drive(&mut cx_b, &host_b);
    with_shell(&mut cx_a, &host_a, |shell, cx| shell.dev_sync_sync_once(cx))
        .unwrap_or_else(|error| panic!("A 第二轮同步被拒绝：{error}"));
    drive(&mut cx_a, &host_a);
    let text_a = current_text(&mut cx_a, &host_a);
    assert!(
        text_a.contains("rot-a") && text_a.contains("rot-b"),
        "A 必须同时看到双方在 E2 的文字：{:?}",
        &text_a[..text_a.len().min(64)]
    );
}

// ---------------------------------------------------------------------------
// 2) 恢复码开关：Some → 新设备可 Join；None → Join 明确失败
// ---------------------------------------------------------------------------

#[gpui::test]
async fn recovery_code_option_controls_whether_new_devices_can_join(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let pair = establish_pair("recovery");
    let relay_url = pair.relay.base_url().to_owned();

    let (host_a, a_handle) = add_shell_window(
        cx,
        pair.a_vault.clone(),
        "dev-sync-rotation-recovery-a",
        config_for(&relay_url, &pair.a_path),
        pair.a_sync,
    );
    let app_cx = cx.clone();
    let mut cx = VisualTestContext::from_window(a_handle, &app_cx);
    cx.run_until_parked();

    // 1) Some(当前恢复码)：报告必须如实说"恢复码已随新 epoch 重封"。
    with_shell(&mut cx, &host_a, |shell, cx| {
        shell.dev_sync_rotate(Some(pair.recovery_code.clone()), cx)
    })
    .unwrap_or_else(|error| panic!("A 带恢复码轮换被拒绝：{error}"));
    drive(&mut cx, &host_a);
    let report = last_report(&mut cx, &host_a).expect("轮换报告");
    assert!(report.is_ok(), "带恢复码轮换必须成功：{report:?}");
    let facts = report.rotation.as_ref().expect("公开事实");
    assert!(facts.recovery_code_reissued, "Some 必须报告已重封恢复槽");
    assert!(
        report.notice.contains("恢复码已补发"),
        "报告必须说明恢复码已补发：{:?}",
        report.notice
    );

    // C：只凭 vault_id + 恢复码 Join → 必须落在 E2（新根）。
    let c_path = pair._dir.path().join("inst-c").join("vault.sqlite3");
    let c = setup::prepare(
        &config_for(&relay_url, &c_path),
        SetupMode::Join,
        SetupSecrets {
            vault_id: Some(ids::hex_of(&pair.a_identity.vault_id)),
            recovery_code: Some(pair.recovery_code.clone()),
            ..SetupSecrets::default()
        },
    )
    .expect("轮换后新设备必须能用恢复码 Join");
    assert_eq!(c.identity.current_epoch, 2, "C 必须直接 Join 到轮换后的 E2");
    drop(c.vault);

    // 2) None：再轮换到 E3；新设备用同一恢复码必须明确失败。
    with_shell(&mut cx, &host_a, |shell, cx| {
        shell.dev_sync_rotate(None, cx)
    })
    .unwrap_or_else(|error| panic!("A 第二次轮换被拒绝：{error}"));
    drive(&mut cx, &host_a);
    let report = last_report(&mut cx, &host_a).expect("第二次轮换报告");
    assert!(report.is_ok(), "第二次轮换必须成功：{report:?}");
    let facts = report.rotation.as_ref().expect("公开事实");
    assert_eq!(facts.from_epoch, 2);
    assert_eq!(facts.to_epoch, Some(3));
    assert!(!facts.recovery_code_reissued);
    assert!(
        report.notice.contains("无法用恢复码加入"),
        "None 必须明确警告：{:?}",
        report.notice
    );

    let d_path = pair._dir.path().join("inst-d").join("vault.sqlite3");
    let error = match setup::prepare(
        &config_for(&relay_url, &d_path),
        SetupMode::Join,
        SetupSecrets {
            vault_id: Some(ids::hex_of(&pair.a_identity.vault_id)),
            recovery_code: Some(pair.recovery_code.clone()),
            ..SetupSecrets::default()
        },
    ) {
        Ok(_) => panic!("E3 的恢复槽用随机种子封装，旧恢复码必须 Join 失败"),
        Err(error) => error,
    };
    let message = format!("{error:#}");
    assert!(
        message.contains("MNE") || message.contains("恢复槽") || message.contains("KEY_SLOT"),
        "失败必须点明恢复槽解不开：{message}"
    );
    assert!(!d_path.exists(), "Join 失败不得留下半初始化库");
}

// ---------------------------------------------------------------------------
// 3) 轮换期间冻结输入/结构/切换；失败释放闸门、身份逐字节不变、旧视图保留
// ---------------------------------------------------------------------------

#[gpui::test]
async fn rotation_freezes_input_structure_and_switch_and_failure_keeps_old_state(
    cx: &mut TestAppContext,
) {
    ensure_test_user_key();
    let pair = establish_pair("freeze");
    // setup 用真实 relay；窗口里的 worker 指向黑洞（本地闸门/库照常，只有网络卡住）。
    let blackhole = BlackholeRelay::start();
    let identity_path = DevIdentityStore::path_for(&pair.a_path);
    let identity_before = std::fs::read(&identity_path).expect("读轮换前身份文件失败");
    let tree_before = list_tree(&pair.a_vault).expect("读结构树失败");

    let (host, handle) = add_shell_window(
        cx,
        pair.a_vault.clone(),
        "dev-sync-rotation-freeze",
        config_for(&blackhole.url(), &pair.a_path),
        pair.a_sync,
    );
    let app_cx = cx.clone();
    let mut cx = VisualTestContext::from_window(handle, &app_cx);
    cx.run_until_parked();
    let view_before = active_view(&mut cx, &host).expect("启动时必须打开第一篇");
    let text_before = current_text(&mut cx, &host);
    assert_eq!(text_before, pair.seed_text);

    // 1) 发起轮换：必须先 Held，再派发 worker（worker 会卡在传输上）。
    with_shell(&mut cx, &host, |shell, cx| shell.dev_sync_rotate(None, cx))
        .unwrap_or_else(|error| panic!("轮换被拒绝：{error}"));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        with_shell(&mut cx, &host, |shell, cx| shell.dev_sync_pump(cx));
        cx.run_until_parked();
        if active_held(&mut cx, &host) && rotation_in_progress(&mut cx, &host) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "10s 内没有进入「轮换 + 持闸」（面板：{:?}）",
            panel_rotation_line(&mut cx, &host)
        );
        std::thread::yield_now();
    }
    assert!(
        cx.debug_bounds("editor-sync-held").is_some(),
        "轮换持闸期间必须画出 editor-sync-held"
    );
    let line = panel_rotation_line(&mut cx, &host).expect("面板必须有持闸任务行");
    assert!(
        line.contains("轮换"),
        "面板必须显示「轮换中」而不是普通同步：{line:?}"
    );
    // 切片 6b：轮换中的**阶段文案**（申请/候选/激活/推广）必须可见；两个入口此刻
    // 单操作在途 → 都给原因（而不是允许展开第二个确认区）。
    let stage = rotation_stage(&mut cx, &host).expect("轮换在途必须有阶段文案");
    assert!(
        stage.starts_with(ROTATE_STAGES_TEXT),
        "阶段文案必须含固定四段：{stage:?}"
    );
    assert!(
        rotate_block_reason(&mut cx, &host).is_some_and(|reason| reason.contains("在途")),
        "轮换期间再次发起轮换必须给「在途」原因"
    );
    assert!(
        apply_block_reason(&mut cx, &host).is_some_and(|reason| reason.contains("在途")),
        "轮换期间 apply 必须给「在途」原因"
    );
    ui_click(&mut cx, "dev-sync-rotate");
    assert!(
        !read_panel(&mut cx, &host, DevSyncPanel::rotate_confirm_open),
        "轮换在途时不得再展开确认区"
    );

    // 2) 输入冻结：键入不得进镜像/库。
    focus_editor(&mut cx);
    cx.simulate_input("frozen-input");
    cx.run_until_parked();
    assert_eq!(
        current_text(&mut cx, &host),
        text_before,
        "轮换持闸期间输入必须被冻结"
    );

    // 3) 结构写入冻结：新建笔记被拒。
    let new_note = cx
        .debug_bounds("sidebar-new-note")
        .expect("selector=sidebar-new-note");
    cx.simulate_click(new_note.center(), Modifiers::default());
    cx.run_until_parked();
    assert_eq!(
        list_tree(&pair.a_vault).expect("读结构树失败"),
        tree_before,
        "轮换期间不得写入结构"
    );
    assert_eq!(
        read_shell(&mut cx, &host, |shell, _app| shell
            .dev_sync_sidebar_notice()
            .map(std::borrow::ToOwned::to_owned)),
        Some("同步进行中，稍后再改结构".to_owned()),
        "结构被拒时必须给出侧栏提示"
    );

    // 4) 文档切换冻结：点第二篇笔记不得切换。
    let second_row = cx.debug_bounds("tree-row-2").expect("第二篇笔记行");
    cx.simulate_click(second_row.center(), Modifiers::default());
    cx.run_until_parked();
    assert_eq!(
        active_view(&mut cx, &host),
        Some(view_before.clone()),
        "轮换期间不得切换活动文档"
    );
    assert_eq!(
        read_shell(&mut cx, &host, |shell, _app| shell
            .dev_sync_sidebar_notice()
            .map(std::borrow::ToOwned::to_owned)),
        Some("同步进行中，稍后再打开/切换文档".to_owned()),
        "切换被拒时必须给出侧栏提示"
    );

    // 5) 关闭黑洞 → worker 明确失败 → 闸门释放；身份文件逐字节不变、旧视图保留。
    drop(blackhole);
    drive(&mut cx, &host);
    let report = last_report(&mut cx, &host).expect("失败也必须留下报告");
    assert!(report.error.is_some(), "轮换失败必须是明确错误：{report:?}");
    let error = report.error.as_deref().unwrap_or_default();
    assert!(
        error.contains("轮换失败") && error.contains("阶段"),
        "失败必须带公开阶段：{error}"
    );
    assert!(
        !rotation_in_progress(&mut cx, &host),
        "失败后必须释放 busy 状态"
    );
    assert!(!active_held(&mut cx, &host), "失败后闸门必须释放");
    assert_eq!(
        std::fs::read(&identity_path).expect("读轮换后身份文件失败"),
        identity_before,
        "轮换失败时身份文件必须逐字节不变"
    );
    assert_eq!(disk_epoch(&pair.a_path), 1, "失败后身份仍是 E1");
    assert_eq!(
        active_view(&mut cx, &host),
        Some(view_before.clone()),
        "失败必须保留旧视图（不重开、不切换）"
    );
    assert_eq!(current_text(&mut cx, &host), text_before);

    // 6) 结束后恢复：输入与切换都重新可用。
    focus_editor(&mut cx);
    cx.simulate_input("after-failure");
    cx.run_until_parked();
    assert!(
        current_text(&mut cx, &host).contains("after-failure"),
        "轮换失败后输入必须恢复"
    );
    let second_row = cx.debug_bounds("tree-row-2").expect("第二篇笔记行");
    cx.simulate_click(second_row.center(), Modifiers::default());
    cx.run_until_parked();
    assert!(
        active_view(&mut cx, &host).is_some_and(|view| view != view_before),
        "轮换失败后文档切换必须恢复"
    );
}

// ---------------------------------------------------------------------------
// 4) B 被吊销 → apply 明确拒绝、不半推广
// ---------------------------------------------------------------------------

#[gpui::test]
async fn revoked_device_cannot_apply_and_stays_on_old_epoch(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let pair = establish_pair("revoked");
    let relay_url = pair.relay.base_url().to_owned();
    let b_path = pair.b_path.clone();
    let b_identity_path = DevIdentityStore::path_for(&b_path);
    let b_identity_before = std::fs::read(&b_identity_path).expect("读 B 身份文件失败");
    let b_device = DevIdentityStore
        .load(&b_path)
        .expect("读 B 磁盘身份失败")
        .device_id;

    let (host_a, a_handle) = add_shell_window(
        cx,
        pair.a_vault.clone(),
        "dev-sync-rotation-revoked-a",
        config_for(&relay_url, &pair.a_path),
        pair.a_sync,
    );
    let (host_b, b_handle) = add_shell_window(
        cx,
        pair.b_vault.clone(),
        "dev-sync-rotation-revoked-b",
        config_for(&relay_url, &b_path),
        pair.b_sync,
    );
    let app_cx = cx.clone();
    let mut cx_a = VisualTestContext::from_window(a_handle, &app_cx);
    let mut cx_b = VisualTestContext::from_window(b_handle, &app_cx);
    cx_a.run_until_parked();
    cx_b.run_until_parked();
    let view_b_before = active_view(&mut cx_b, &host_b).expect("B 必须打开第一篇");
    let text_b_before = current_text(&mut cx_b, &host_b);

    // A：轮换并吊销 B（signed begin body 里的 revoke[]）。
    with_shell(&mut cx_a, &host_a, |shell, cx| {
        shell.dev_sync_rotate_revoking(None, vec![b_device], cx)
    })
    .unwrap_or_else(|error| panic!("A 轮换（revoke B）被拒绝：{error}"));
    drive(&mut cx_a, &host_a);
    let report_a = last_report(&mut cx_a, &host_a).expect("A 轮换报告");
    assert!(report_a.is_ok(), "A 轮换必须成功：{report_a:?}");
    let facts_a = report_a.rotation.as_ref().expect("公开事实");
    assert_eq!(facts_a.to_epoch, Some(2));
    assert_eq!(facts_a.revoked, 1, "签名 revoke 列表必须含 B");

    // B：apply 必须明确拒绝（不在 grant 列表），且不推广。
    with_shell(&mut cx_b, &host_b, |shell, cx| {
        shell.dev_sync_apply_epoch(cx)
    })
    .unwrap_or_else(|error| panic!("B apply 被拒绝（宿主层）：{error}"));
    drive(&mut cx_b, &host_b);
    let report_b = last_report(&mut cx_b, &host_b).expect("B apply 失败也必须留下报告");
    assert!(
        report_b.error.is_some(),
        "被吊销设备 apply 必须明确失败：{report_b:?}"
    );
    let error = report_b.error.as_deref().unwrap_or_default();
    assert!(
        error.contains("不在") || error.contains("grant") || error.contains("授权"),
        "失败必须点明不在 grant 列表：{error}"
    );
    let facts_b = report_b.rotation.as_ref().expect("失败也必须有公开事实");
    assert_eq!(facts_b.from_epoch, 1);
    assert_eq!(facts_b.to_epoch, None, "NotGranted 不得声称有新 epoch");
    assert!(facts_b.failure_phase.is_some(), "失败必须带公开阶段");

    // 不半推广：身份文件逐字节不变、仍是 E1；旧视图/正文保留。
    assert_eq!(
        std::fs::read(&b_identity_path).expect("读 B 身份文件失败"),
        b_identity_before,
        "apply 被拒绝时 B 的身份文件必须逐字节不变"
    );
    assert_eq!(disk_epoch(&b_path), 1, "B 必须保持旧 epoch");
    assert_eq!(
        active_view(&mut cx_b, &host_b),
        Some(view_b_before),
        "apply 被拒绝不得重建/切换 B 的活动视图"
    );
    assert_eq!(current_text(&mut cx_b, &host_b), text_b_before);

    // B 仍可本地编辑（旧 epoch 的本地写入不被阻塞）。
    focus_editor(&mut cx_b);
    cx_b.simulate_input("still-local");
    cx_b.run_until_parked();
    assert!(current_text(&mut cx_b, &host_b).contains("still-local"));
}

// ---------------------------------------------------------------------------
// 切片 6b：轮换/应用轮换接到**设备面板 UI**（固定 selector 的真实点击）
//
// 这一组不再直接调 `AppShell::dev_sync_rotate`：它点的是面板上的
// `dev-sync-rotate` / `dev-sync-rotate-confirm` / `dev-sync-apply-epoch` 等固定
// selector，覆盖"按钮可见性 → 确认文案 → 恢复码/撤销选项 → 持闸派发 → 结果文案"。
// 收敛判据仍是状态（`dev_sync_rotation_in_progress()`、`in_flight`、闸门释放），
// 没有 sleep；headless 下 GPUI 定时器不会自己走，面板的 40 ms 泵用显式推进时钟。
// ---------------------------------------------------------------------------

/// 面板实体（真实挂载；`None` = 该实例没有面板）。
fn panel_entity(cx: &mut VisualTestContext, host: &Entity<ShellHost>) -> Entity<DevSyncPanel> {
    read_shell(cx, host, |shell, _app| shell.dev_sync_panel()).expect("面板必须挂载")
}

/// 只读面板投影。
fn read_panel<R>(
    cx: &mut VisualTestContext,
    host: &Entity<ShellHost>,
    f: impl FnOnce(&DevSyncPanel) -> R,
) -> R {
    let panel = panel_entity(cx, host);
    cx.update(|_window, app| f(panel.read(app)))
}

fn panel_devices(cx: &mut VisualTestContext, host: &Entity<ShellHost>) -> Vec<MemberView> {
    read_panel(cx, host, |panel| panel.devices().to_vec())
}

fn panel_notice_string(cx: &mut VisualTestContext, host: &Entity<ShellHost>) -> String {
    read_panel(cx, host, |panel| {
        panel.notice_text().unwrap_or_default().to_owned()
    })
}

fn panel_error_string(cx: &mut VisualTestContext, host: &Entity<ShellHost>) -> String {
    read_panel(cx, host, DevSyncPanel::error_text)
}

/// 恢复码输入框里的长度（**不暴露正文**；断言"已粘贴/已清空"用）。
fn recovery_len(cx: &mut VisualTestContext, host: &Entity<ShellHost>) -> usize {
    let panel = panel_entity(cx, host);
    cx.update(|_window, app| panel.read(app).rotate_recovery_len(app))
}

fn recovery_is_secret(cx: &mut VisualTestContext, host: &Entity<ShellHost>) -> bool {
    let panel = panel_entity(cx, host);
    cx.update(|_window, app| panel.read(app).rotate_recovery_is_secret(app))
}

fn rotate_warning(cx: &mut VisualTestContext, host: &Entity<ShellHost>) -> Option<&'static str> {
    let panel = panel_entity(cx, host);
    cx.update(|_window, app| panel.read(app).rotate_warning_text(app))
}

fn rotate_block_reason(
    cx: &mut VisualTestContext,
    host: &Entity<ShellHost>,
) -> Option<&'static str> {
    read_panel(cx, host, DevSyncPanel::rotate_block_reason)
}

fn apply_block_reason(
    cx: &mut VisualTestContext,
    host: &Entity<ShellHost>,
) -> Option<&'static str> {
    read_panel(cx, host, DevSyncPanel::apply_block_reason)
}

fn rotation_stage(cx: &mut VisualTestContext, host: &Entity<ShellHost>) -> Option<String> {
    read_panel(cx, host, DevSyncPanel::rotation_stage_text)
}

/// 动态 selector → `&'static str`（`debug_bounds` 只接受静态字符串；测试进程内泄漏几个
/// 设备行 selector 属可接受开销，不进生产代码）。
fn probe(selector: &str) -> &'static str {
    Box::leak(selector.to_owned().into_boxed_str())
}

fn reopen_settings_if_closed(cx: &mut VisualTestContext) {
    if cx.debug_bounds("settings-panel").is_none() {
        if let Some(gear) = cx.debug_bounds("sync-status") {
            cx.simulate_click(gear.center(), Modifiers::default());
            cx.run_until_parked();
        }
    }
}

fn expect_selector(cx: &mut VisualTestContext, selector: &'static str) {
    if cx.debug_bounds(selector).is_none() {
        reopen_settings_if_closed(cx);
    }
    assert!(
        cx.debug_bounds(selector).is_some(),
        "必须画出 selector={selector}"
    );
}

fn expect_missing(cx: &mut VisualTestContext, selector: &'static str, why: &str) {
    assert!(
        cx.debug_bounds(selector).is_none(),
        "{why}（selector={selector} 不得存在）"
    );
}

/// 点一个固定 selector（必须存在且非零尺寸）。
fn ui_click(cx: &mut VisualTestContext, selector: &'static str) {
    if cx.debug_bounds(selector).is_none() {
        reopen_settings_if_closed(cx);
    }
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("必须画出 selector={selector}"));
    cx.simulate_click(bounds.center(), Modifiers::default());
    cx.run_until_parked();
}

/// 推进一次面板 40 ms 泵 + 宿主泵（headless 定时器需要显式推时钟）。
fn ui_pump(cx: &mut VisualTestContext, host: &Entity<ShellHost>) {
    cx.executor()
        .advance_clock(menghunote_app::dev_sync::SYNC_PUMP_INTERVAL);
    with_shell(cx, host, |shell, cx| {
        shell.dev_sync_pump(cx);
    });
    cx.run_until_parked();
}

/// 泵到设备列表满足条件且单操作槽空闲（**状态收敛**，不是 sleep）。
fn wait_for_devices(
    cx: &mut VisualTestContext,
    host: &Entity<ShellHost>,
    description: &str,
    expect: impl Fn(&[MemberView]) -> bool,
) {
    let deadline = Instant::now() + DEADLINE;
    loop {
        ui_pump(cx, host);
        let current = panel_devices(cx, host);
        // 列表事件在 `Completed` 之前发出：必须同时要求 in_flight 归零，否则下一条
        // 命令会被 Busy 拒绝。
        let idle = read_panel(cx, host, |panel| {
            panel.service().snapshot().in_flight.is_none()
        });
        if idle && expect(&current) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "60s 内没有收敛（{description}）：{current:?}"
        );
        std::thread::yield_now();
    }
}

/// 粘贴恢复码到轮换确认区的秘密输入框（真剪贴板 + 真按键）。
fn paste_recovery(cx: &mut VisualTestContext, code: &str) {
    ui_click(cx, "dev-sync-rotate-recovery");
    cx.write_to_clipboard(gpui::ClipboardItem::new_string(code.to_owned()));
    cx.simulate_keystrokes("ctrl-v");
    cx.run_until_parked();
}

/// 空库实例（**不播种演示树**）：没有活动文档，用来验"轮换需要先打开一篇"。
struct EmptyVault {
    relay: DevRelay,
    _dir: TempDir,
    path: PathBuf,
    vault: Vault,
    sync: SyncContext,
}

fn establish_empty_instance(prefix: &str) -> EmptyVault {
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let dir = temp_root(prefix);
    let path = dir.path().join("inst").join("vault.sqlite3");
    let created = setup::prepare(
        &config_for(relay.base_url(), &path),
        SetupMode::Create,
        create_secrets(&relay),
    )
    .expect("空库 Create 失败");
    assert!(
        list_tree(&created.vault).expect("读空树失败").is_empty(),
        "Create 之后结构树必须是空的（本用例要「没有活动文档」）"
    );
    let epoch = created.identity.current_epoch;
    let peer = created.identity.loro_peer_id();
    EmptyVault {
        relay,
        _dir: dir,
        path,
        vault: created.vault,
        sync: SyncContext::new(peer, epoch),
    }
}

// ---------------------------------------------------------------------------
// 1) A 点「轮换密钥…」→ 确认区 → 确认（None）→ E2；B 点「应用轮换」→ E2；
//    双方继续编辑并同步；设备列表/移除入口回归
// ---------------------------------------------------------------------------

#[gpui::test]
async fn ui_rotate_then_apply_keeps_both_sides_editing(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let pair = establish_pair("ui-roundtrip");
    let relay_url = pair.relay.base_url().to_owned();
    let (host_a, a_handle) = add_shell_window(
        cx,
        pair.a_vault.clone(),
        "dev-sync-ui-rotate-a",
        config_for(&relay_url, &pair.a_path),
        pair.a_sync,
    );
    let (host_b, b_handle) = add_shell_window(
        cx,
        pair.b_vault.clone(),
        "dev-sync-ui-rotate-b",
        config_for(&relay_url, &pair.b_path),
        pair.b_sync,
    );
    let app_cx = cx.clone();
    let mut cx_a = VisualTestContext::from_window(a_handle, &app_cx);
    let mut cx_b = VisualTestContext::from_window(b_handle, &app_cx);
    cx_a.run_until_parked();
    cx_b.run_until_parked();
    wait_for_devices(&mut cx_a, &host_a, "A 读到两台设备", |members| {
        members.len() == 2
    });
    wait_for_devices(&mut cx_b, &host_b, "B 读到两台设备", |members| {
        members.len() == 2
    });

    let view_a_before = active_view(&mut cx_a, &host_a).expect("A 必须有活动文档");
    let view_b_before = active_view(&mut cx_b, &host_b).expect("B 必须有活动文档");
    assert_eq!(current_text(&mut cx_a, &host_a), pair.seed_text);

    // --- A：点「轮换密钥…」只展开确认区（固定文案 + 全部 selector），不动数据库 ---
    ui_click(&mut cx_a, "dev-sync-rotate");
    assert!(
        read_panel(&mut cx_a, &host_a, DevSyncPanel::rotate_confirm_open),
        "点「轮换密钥…」必须展开二次确认"
    );
    for selector in [
        "dev-sync-rotate-confirm",
        "dev-sync-rotate-cancel",
        "dev-sync-rotate-reissue",
        "dev-sync-rotate-revoke",
        "dev-sync-rotate-recovery",
        "dev-sync-rotate-warning",
    ] {
        expect_selector(&mut cx_a, selector);
    }
    assert!(
        ROTATE_CONFIRM_TEXT.contains("轮换会用新密钥重新加密整库"),
        "固定确认文案不得改字：{ROTATE_CONFIRM_TEXT}"
    );
    assert_eq!(disk_epoch(&pair.a_path), 1, "只展开确认不得写身份/epoch");
    assert!(!rotation_in_progress(&mut cx_a, &host_a));
    assert!(!active_held(&mut cx_a, &host_a), "只展开确认不得持闸");

    // 取消：确认区消失、不轮换、不持闸。
    ui_click(&mut cx_a, "dev-sync-rotate-cancel");
    assert!(!read_panel(
        &mut cx_a,
        &host_a,
        DevSyncPanel::rotate_confirm_open
    ));
    expect_missing(&mut cx_a, "dev-sync-rotate-confirm", "取消后确认区必须收起");
    assert_eq!(disk_epoch(&pair.a_path), 1, "取消不得轮换");
    assert!(read_shell(&mut cx_a, &host_a, |shell, _| {
        shell.dev_sync_last_rotation().is_none()
    }));

    // --- 再展开：取消勾选「重新封装恢复码」→ 固定警告 + 输入框消失 → 确认（None）---
    ui_click(&mut cx_a, "dev-sync-rotate");
    assert!(read_panel(
        &mut cx_a,
        &host_a,
        DevSyncPanel::rotate_reissue_enabled
    ));
    ui_click(&mut cx_a, "dev-sync-rotate-reissue");
    assert!(!read_panel(
        &mut cx_a,
        &host_a,
        DevSyncPanel::rotate_reissue_enabled
    ));
    assert_eq!(
        rotate_warning(&mut cx_a, &host_a),
        Some(ROTATE_NO_RECOVERY_WARNING),
        "未勾选重封装必须有固定警告"
    );
    expect_missing(
        &mut cx_a,
        "dev-sync-rotate-recovery",
        "未勾选重封装时不得显示恢复码输入",
    );
    ui_click(&mut cx_a, "dev-sync-rotate-confirm");
    assert!(!read_panel(
        &mut cx_a,
        &host_a,
        DevSyncPanel::rotate_confirm_open
    ));
    drive(&mut cx_a, &host_a);

    let report_a = last_report(&mut cx_a, &host_a).expect("A 轮换必须留下报告");
    assert!(report_a.is_ok(), "A 轮换必须成功：{report_a:?}");
    let facts_a = report_a.rotation.as_ref().expect("轮换报告必须带公开事实");
    assert_eq!(facts_a.from_epoch, 1);
    assert_eq!(facts_a.to_epoch, Some(2));
    assert!(!facts_a.recovery_code_reissued, "None 不得声称补发恢复码");
    assert!(
        report_a.notice.contains("epoch 1 → 2") && report_a.notice.contains("无法用恢复码加入"),
        "None 必须显示 epoch 迁移 + 恢复码警告：{:?}",
        report_a.notice
    );
    assert_eq!(disk_epoch(&pair.a_path), 2, "身份文件必须写回新 epoch");
    assert!(!active_held(&mut cx_a, &host_a), "轮换完成后闸门必须释放");
    assert!(
        rotation_stage(&mut cx_a, &host_a).is_none(),
        "轮换结束后不得继续显示「轮换中」"
    );
    let notice = panel_notice_string(&mut cx_a, &host_a);
    assert!(
        notice.contains("epoch 1 → 2"),
        "面板 notice 必须显示轮换结果：{notice}"
    );
    let line = panel_rotation_line(&mut cx_a, &host_a).expect("持闸任务行");
    assert!(line.contains("轮换完成"), "面板任务行：{line}");
    let view_a_after = active_view(&mut cx_a, &host_a).expect("轮换后必须重新打开笔记");
    assert_ne!(view_a_before, view_a_after, "轮换后必须重建活动视图");
    assert_eq!(current_text(&mut cx_a, &host_a), pair.seed_text);

    // --- B：点「应用轮换」→ 确认文案/取消 → 确认 → E2 ---
    wait_for_devices(&mut cx_b, &host_b, "B 面板仍空闲", |members| {
        members.len() == 2
    });
    ui_click(&mut cx_b, "dev-sync-apply-epoch");
    assert!(
        read_panel(&mut cx_b, &host_b, DevSyncPanel::apply_confirm_open),
        "点「应用轮换」必须展开二次确认"
    );
    assert!(
        APPLY_CONFIRM_TEXT.contains("导入基线") && APPLY_CONFIRM_TEXT.contains("输入会冻结"),
        "固定确认文案不得改字：{APPLY_CONFIRM_TEXT}"
    );
    for selector in ["dev-sync-apply-confirm", "dev-sync-apply-cancel"] {
        expect_selector(&mut cx_b, selector);
    }
    ui_click(&mut cx_b, "dev-sync-apply-cancel");
    assert!(!read_panel(
        &mut cx_b,
        &host_b,
        DevSyncPanel::apply_confirm_open
    ));
    expect_missing(&mut cx_b, "dev-sync-apply-confirm", "取消后确认区必须收起");
    assert_eq!(disk_epoch(&pair.b_path), 1, "取消 apply 不得写身份");

    ui_click(&mut cx_b, "dev-sync-apply-epoch");
    ui_click(&mut cx_b, "dev-sync-apply-confirm");
    drive(&mut cx_b, &host_b);
    let report_b = last_report(&mut cx_b, &host_b).expect("B apply 必须留下报告");
    assert!(report_b.is_ok(), "B apply 必须成功：{report_b:?}");
    let facts_b = report_b.rotation.as_ref().expect("公开事实");
    assert_eq!(facts_b.from_epoch, 1);
    assert_eq!(facts_b.to_epoch, Some(2));
    assert!(!facts_b.already_current);
    assert!(
        report_b.notice.contains("已应用新 epoch") && report_b.notice.contains("1 → 2"),
        "面板必须显示 apply 的 epoch 迁移：{:?}",
        report_b.notice
    );
    assert_eq!(disk_epoch(&pair.b_path), 2, "B 身份文件必须写回新 epoch");
    assert_ne!(
        view_b_before,
        active_view(&mut cx_b, &host_b).expect("apply 后必须重新打开笔记"),
        "apply 后必须重建活动视图"
    );

    // --- 继续编辑 + 用面板「同步」按钮（手动同步回归）双向可见 ---
    focus_editor(&mut cx_a);
    cx_a.simulate_keystrokes("end");
    cx_a.run_until_parked();
    cx_a.simulate_input("ui-rot-a");
    cx_a.run_until_parked();
    assert!(current_text(&mut cx_a, &host_a).contains("ui-rot-a"));
    assert_eq!(
        wait_for_outbox_epoch(&mut cx_a, &host_a, &pair.a_vault, &pair.doc_id),
        2,
        "轮换后新编辑的 intent 必须带 epoch 2"
    );
    ui_click(&mut cx_a, "dev-sync-sync");
    drive(&mut cx_a, &host_a);
    assert!(
        last_sync_report(&mut cx_a, &host_a).is_some_and(|report| report.is_ok()),
        "A 轮换后点面板「同步」必须成功"
    );

    ui_click(&mut cx_b, "dev-sync-sync");
    drive(&mut cx_b, &host_b);
    assert!(
        last_sync_report(&mut cx_b, &host_b).is_some_and(|report| report.is_ok()),
        "B 轮换后点面板「同步」必须成功"
    );
    assert!(
        current_text(&mut cx_b, &host_b).contains("ui-rot-a"),
        "B 必须看到 A 在 E2 写入的文字：{:?}",
        current_text(&mut cx_b, &host_b)
    );

    // --- 回归：轮换后设备列表仍可用，对方行仍有「移除设备」入口 ---
    wait_for_devices(
        &mut cx_a,
        &host_a,
        "A 轮换后仍能读设备列表",
        |members| members.len() == 2,
    );
    let members = panel_devices(&mut cx_a, &host_a);
    let self_a = read_panel(&mut cx_a, &host_a, DevSyncPanel::self_device).expect("本机身份");
    let b_index = members
        .iter()
        .position(|member| member.device_id != self_a)
        .expect("列表里必须有对方设备");
    assert!(
        members
            .iter()
            .all(|member| member.status == MemberStatus::Active),
        "轮换不得改动设备访问状态：{members:?}"
    );
    expect_selector(&mut cx_a, probe(&format!("dev-sync-remove-{b_index}")));
}

// ---------------------------------------------------------------------------
// 2) 恢复码重封装：Some（面板粘贴）→ 新设备可 Join；None → 明确提示 + Join 失败
// ---------------------------------------------------------------------------

#[gpui::test]
async fn ui_recovery_reissue_controls_whether_new_devices_can_join(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let pair = establish_pair("ui-recovery");
    let relay_url = pair.relay.base_url().to_owned();
    let (host, handle) = add_shell_window(
        cx,
        pair.a_vault.clone(),
        "dev-sync-ui-recovery-a",
        config_for(&relay_url, &pair.a_path),
        pair.a_sync,
    );
    let app_cx = cx.clone();
    let mut cx = VisualTestContext::from_window(handle, &app_cx);
    cx.run_until_parked();
    wait_for_devices(&mut cx, &host, "A 读到两台设备", |members| {
        members.len() == 2
    });
    assert!(read_panel(&mut cx, &host, DevSyncPanel::can_manage_devices));

    // --- 展开确认：默认勾选重封装，但输入为空 → 确认被阻止（不派发半截请求）---
    ui_click(&mut cx, "dev-sync-rotate");
    assert!(
        read_panel(&mut cx, &host, DevSyncPanel::rotate_reissue_enabled),
        "默认必须推荐「重新封装恢复码」"
    );
    assert!(
        recovery_is_secret(&mut cx, &host),
        "恢复码输入必须是秘密字段（遮罩 + 不向平台报告正文）"
    );
    assert_eq!(recovery_len(&mut cx, &host), 0);
    assert_eq!(
        rotate_warning(&mut cx, &host),
        Some(ROTATE_RECOVERY_EMPTY_WARNING)
    );
    ui_click(&mut cx, "dev-sync-rotate-confirm");
    assert!(
        read_panel(&mut cx, &host, DevSyncPanel::rotate_confirm_open),
        "空恢复码时确认区必须保持展开（不派发半截请求）"
    );
    assert!(
        !rotation_in_progress(&mut cx, &host),
        "空恢复码不得开始轮换"
    );
    assert_eq!(disk_epoch(&pair.a_path), 1, "空恢复码不得写身份");
    assert!(
        panel_notice_string(&mut cx, &host).contains("空则不提交"),
        "确认被阻止必须给明确原因：{}",
        panel_notice_string(&mut cx, &host)
    );

    // --- 真剪贴板粘贴 → 警告消失 → 确认（Some 路径）---
    paste_recovery(&mut cx, &pair.recovery_code);
    assert_eq!(
        recovery_len(&mut cx, &host),
        pair.recovery_code.len(),
        "Ctrl+V 必须把恢复码贴进秘密输入框"
    );
    assert_eq!(rotate_warning(&mut cx, &host), None);
    ui_click(&mut cx, "dev-sync-rotate-confirm");
    assert!(!read_panel(
        &mut cx,
        &host,
        DevSyncPanel::rotate_confirm_open
    ));
    assert_eq!(
        recovery_len(&mut cx, &host),
        0,
        "提交后恢复码必须立刻清空（只用一次）"
    );
    drive(&mut cx, &host);
    let report = last_report(&mut cx, &host).expect("轮换报告");
    assert!(report.is_ok(), "带恢复码轮换必须成功：{report:?}");
    let facts = report.rotation.as_ref().expect("公开事实");
    assert_eq!(facts.to_epoch, Some(2));
    assert!(facts.recovery_code_reissued, "Some 必须报告已重封恢复槽");
    assert!(
        report.notice.contains("恢复码已补发"),
        "报告必须说明恢复码已补发：{:?}",
        report.notice
    );
    // 秘密不落任何公开/日志型投影（面板行、notice、错误行、报告 Debug）。
    let mut projections = vec![
        panel_notice_string(&mut cx, &host),
        panel_error_string(&mut cx, &host),
        panel_rotation_line(&mut cx, &host).unwrap_or_default(),
        report.notice.clone(),
        format!("{report:?}"),
    ];
    projections.push(format!("{:?}", facts));
    for text in &projections {
        assert!(
            !text.contains(pair.recovery_code.as_str()),
            "公开投影不得包含恢复码正文：{text}"
        );
        assert!(
            !text.to_ascii_lowercase().contains("mnrec1"),
            "公开投影不得包含恢复码前缀：{text}"
        );
    }

    // --- C：只凭 vault_id + 同一恢复码 Join → 必须直接落在 E2 ---
    let c_path = pair._dir.path().join("inst-c").join("vault.sqlite3");
    let c = setup::prepare(
        &config_for(&relay_url, &c_path),
        SetupMode::Join,
        SetupSecrets {
            vault_id: Some(ids::hex_of(&pair.a_identity.vault_id)),
            recovery_code: Some(pair.recovery_code.clone()),
            ..SetupSecrets::default()
        },
    )
    .expect("轮换后新设备必须能用同一恢复码 Join");
    assert_eq!(c.identity.current_epoch, 2, "C 必须直接 Join 到轮换后的 E2");
    assert!(
        !list_tree(&c.vault).expect("读 C 结构树失败").is_empty(),
        "C 必须拿到 A 的基线（含演示树）"
    );
    drop(c.vault);

    // --- None：不重封装 → 固定警告 → 再轮换到 E3；新设备 Join 明确失败 ---
    wait_for_devices(&mut cx, &host, "A 面板回到空闲", |members| {
        members.len() >= 2
    });
    ui_click(&mut cx, "dev-sync-rotate");
    ui_click(&mut cx, "dev-sync-rotate-reissue");
    assert_eq!(
        rotate_warning(&mut cx, &host),
        Some(ROTATE_NO_RECOVERY_WARNING)
    );
    ui_click(&mut cx, "dev-sync-rotate-confirm");
    drive(&mut cx, &host);
    let report = last_report(&mut cx, &host).expect("第二次轮换报告");
    assert!(report.is_ok(), "第二次轮换必须成功：{report:?}");
    let facts = report.rotation.as_ref().expect("公开事实");
    assert_eq!(facts.from_epoch, 2);
    assert_eq!(facts.to_epoch, Some(3));
    assert!(!facts.recovery_code_reissued);
    assert!(
        report.notice.contains("无法用恢复码加入"),
        "None 必须明确警告：{:?}",
        report.notice
    );

    let d_path = pair._dir.path().join("inst-d").join("vault.sqlite3");
    let error = match setup::prepare(
        &config_for(&relay_url, &d_path),
        SetupMode::Join,
        SetupSecrets {
            vault_id: Some(ids::hex_of(&pair.a_identity.vault_id)),
            recovery_code: Some(pair.recovery_code.clone()),
            ..SetupSecrets::default()
        },
    ) {
        Ok(_) => panic!("E3 的恢复槽用随机种子封装，旧恢复码必须 Join 失败"),
        Err(error) => error,
    };
    let message = format!("{error:#}");
    assert!(
        message.contains("MNE") || message.contains("恢复槽") || message.contains("KEY_SLOT"),
        "失败必须点明恢复槽解不开：{message}"
    );
    assert!(!d_path.exists(), "Join 失败不得留下半初始化库");
}

// ---------------------------------------------------------------------------
// 3) 按钮可见性/可用性：Join 设备、没有活动文档、单操作在途
// ---------------------------------------------------------------------------

#[gpui::test]
async fn ui_rotation_buttons_reflect_manager_document_and_busy(cx: &mut TestAppContext) {
    ensure_test_user_key();

    // --- 1) Join B（非 manager）：轮换不可用（给原因、点不开确认区）；应用轮换可用 ---
    let pair = establish_pair("ui-visibility");
    let relay_url = pair.relay.base_url().to_owned();
    let (host_b, b_handle) = add_shell_window(
        cx,
        pair.b_vault.clone(),
        "dev-sync-ui-visibility-b",
        config_for(&relay_url, &pair.b_path),
        pair.b_sync,
    );
    let app_cx = cx.clone();
    let mut cx_b = VisualTestContext::from_window(b_handle, &app_cx);
    cx_b.run_until_parked();
    wait_for_devices(&mut cx_b, &host_b, "B 读到两台设备", |members| {
        members.len() == 2
    });
    assert!(!read_panel(
        &mut cx_b,
        &host_b,
        DevSyncPanel::can_manage_devices
    ));
    let reason = rotate_block_reason(&mut cx_b, &host_b).expect("Join 设备不得发起轮换");
    assert!(reason.contains("没有管理密钥"), "{reason}");
    ui_click(&mut cx_b, "dev-sync-rotate");
    assert!(
        !read_panel(&mut cx_b, &host_b, DevSyncPanel::rotate_confirm_open),
        "Join 设备点「轮换密钥…」不得展开确认区（无 rotate 能力）"
    );
    assert!(
        panel_notice_string(&mut cx_b, &host_b).contains("未展开轮换确认"),
        "必须给明确原因：{}",
        panel_notice_string(&mut cx_b, &host_b)
    );
    assert_eq!(
        apply_block_reason(&mut cx_b, &host_b),
        None,
        "非 manager 正是「应用轮换」的主动方"
    );
    expect_selector(&mut cx_b, "dev-sync-apply-epoch");

    // --- 2) manager 但没有活动文档（空库）：先打开一篇 ---
    let empty = establish_empty_instance("ui-visibility-empty");
    let empty_relay = empty.relay.base_url().to_owned();
    let (host_e, e_handle) = add_shell_window(
        cx,
        empty.vault.clone(),
        "dev-sync-ui-visibility-empty",
        config_for(&empty_relay, &empty.path),
        empty.sync,
    );
    let mut cx_e = VisualTestContext::from_window(e_handle, &app_cx);
    cx_e.run_until_parked();
    wait_for_devices(
        &mut cx_e,
        &host_e,
        "空库实例读到一台设备",
        |members| members.len() == 1,
    );
    assert!(
        read_panel(&mut cx_e, &host_e, DevSyncPanel::can_manage_devices),
        "Create 设备必须有管理密钥"
    );
    assert!(!read_panel(
        &mut cx_e,
        &host_e,
        DevSyncPanel::has_active_document
    ));
    let reason = rotate_block_reason(&mut cx_e, &host_e).expect("没有活动文档时轮换不可用");
    assert!(reason.contains("没有打开的笔记"), "{reason}");
    let reason = apply_block_reason(&mut cx_e, &host_e).expect("没有活动文档时 apply 不可用");
    assert!(reason.contains("没有打开的笔记"), "{reason}");
    ui_click(&mut cx_e, "dev-sync-rotate");
    assert!(!read_panel(
        &mut cx_e,
        &host_e,
        DevSyncPanel::rotate_confirm_open
    ));
    ui_click(&mut cx_e, "dev-sync-apply-epoch");
    assert!(!read_panel(
        &mut cx_e,
        &host_e,
        DevSyncPanel::apply_confirm_open
    ));

    // --- 3) 单操作在途（黑洞 relay 让设备列表读卡在途）：两个按钮都禁用 ---
    let stuck = establish_empty_instance("ui-visibility-busy");
    let blackhole = BlackholeRelay::start();
    let (host_s, s_handle) = add_shell_window(
        cx,
        stuck.vault.clone(),
        "dev-sync-ui-visibility-busy",
        config_for(&blackhole.url(), &stuck.path),
        stuck.sync,
    );
    let mut cx_s = VisualTestContext::from_window(s_handle, &app_cx);
    cx_s.run_until_parked();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        // 面板打开即自动读一次设备列表：黑洞 relay 让这条命令停在在途（状态收敛判据）。
        cx_s.executor()
            .advance_clock(menghunote_app::dev_sync::SYNC_PUMP_INTERVAL);
        with_shell(&mut cx_s, &host_s, |shell, cx| {
            shell.dev_sync_pump(cx);
        });
        cx_s.run_until_parked();
        let busy = read_panel(&mut cx_s, &host_s, |panel| {
            panel.service().snapshot().in_flight.is_some()
        });
        if busy {
            break;
        }
        assert!(Instant::now() < deadline, "10s 内设备列表没有进入在途");
        std::thread::yield_now();
    }
    let reason = rotate_block_reason(&mut cx_s, &host_s).expect("在途时轮换不可用");
    assert!(reason.contains("在途"), "{reason}");
    let reason = apply_block_reason(&mut cx_s, &host_s).expect("在途时 apply 不可用");
    assert!(reason.contains("在途"), "{reason}");
    ui_click(&mut cx_s, "dev-sync-rotate");
    assert!(!read_panel(
        &mut cx_s,
        &host_s,
        DevSyncPanel::rotate_confirm_open
    ));
    ui_click(&mut cx_s, "dev-sync-apply-epoch");
    assert!(!read_panel(
        &mut cx_s,
        &host_s,
        DevSyncPanel::apply_confirm_open
    ));
}

// ---------------------------------------------------------------------------
// 4) 确认/取消 + 失败（错恢复码）：视图/身份不变、错误明确、可重试成功
// ---------------------------------------------------------------------------

#[gpui::test]
async fn ui_cancelled_and_failed_rotation_keep_state_then_retry_succeeds(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let pair = establish_pair("ui-failure");
    let relay_url = pair.relay.base_url().to_owned();
    let (host, handle) = add_shell_window(
        cx,
        pair.a_vault.clone(),
        "dev-sync-ui-failure-a",
        config_for(&relay_url, &pair.a_path),
        pair.a_sync,
    );
    let app_cx = cx.clone();
    let mut cx = VisualTestContext::from_window(handle, &app_cx);
    cx.run_until_parked();
    wait_for_devices(&mut cx, &host, "A 读到两台设备", |members| {
        members.len() == 2
    });
    let view_before = active_view(&mut cx, &host).expect("A 必须有活动文档");
    let identity_path = DevIdentityStore::path_for(&pair.a_path);
    let identity_before = std::fs::read(&identity_path).expect("读轮换前身份文件失败");

    // --- 取消：确认区消失、不写任何东西 ---
    ui_click(&mut cx, "dev-sync-rotate");
    paste_recovery(&mut cx, &pair.recovery_code);
    assert_eq!(recovery_len(&mut cx, &host), pair.recovery_code.len());
    ui_click(&mut cx, "dev-sync-rotate-cancel");
    assert!(!read_panel(
        &mut cx,
        &host,
        DevSyncPanel::rotate_confirm_open
    ));
    assert_eq!(
        recovery_len(&mut cx, &host),
        0,
        "取消后恢复码输入必须清空（不留秘密）"
    );
    assert_eq!(disk_epoch(&pair.a_path), 1, "取消不得轮换");
    assert!(read_shell(&mut cx, &host, |shell, _| {
        shell.dev_sync_last_rotation().is_none()
    }));
    assert_eq!(
        std::fs::read(&identity_path).expect("读身份失败"),
        identity_before,
        "取消不得改身份文件"
    );

    // --- 错恢复码：明确失败、身份/视图不变、不谎称 journal ---
    let wrong_code = RecoverySeed::from_array([0x5a; 32])
        .to_code()
        .expect("编码错误恢复码失败")
        .to_string();
    assert_ne!(wrong_code, pair.recovery_code);
    ui_click(&mut cx, "dev-sync-rotate");
    paste_recovery(&mut cx, &wrong_code);
    ui_click(&mut cx, "dev-sync-rotate-confirm");
    drive(&mut cx, &host);
    let failed = last_report(&mut cx, &host).expect("失败也必须留下报告");
    assert!(!failed.is_ok(), "错恢复码必须明确失败：{failed:?}");
    let error = failed.error.clone().expect("失败必须带明确原因");
    assert!(
        error.contains("恢复码与当前管理身份不符"),
        "错误必须点明恢复码不符：{error}"
    );
    assert!(
        failed.notice.contains("轮换失败"),
        "面板 notice 必须显示轮换失败：{:?}",
        failed.notice
    );
    assert!(
        !failed.notice.contains("rotation journal"),
        "验证阶段（未开始）失败不得谎称保留了 journal：{:?}",
        failed.notice
    );
    assert_eq!(disk_epoch(&pair.a_path), 1, "失败必须保持旧 epoch");
    assert_eq!(
        std::fs::read(&identity_path).expect("读身份失败"),
        identity_before,
        "失败后身份文件必须逐字节不变"
    );
    assert_eq!(
        active_view(&mut cx, &host),
        Some(view_before.clone()),
        "失败必须保持旧视图（同一实体）"
    );
    assert!(!active_held(&mut cx, &host), "失败后闸门必须释放");
    assert!(rotation_stage(&mut cx, &host).is_none());
    let line = panel_rotation_line(&mut cx, &host).expect("持闸任务行");
    assert!(line.contains("轮换失败"), "任务行必须显示失败：{line}");
    for text in [
        panel_notice_string(&mut cx, &host),
        panel_error_string(&mut cx, &host),
        line,
        failed.notice.clone(),
        error,
        format!("{failed:?}"),
    ] {
        assert!(
            !text.contains(wrong_code.as_str()),
            "公开投影不得包含恢复码正文：{text}"
        );
        assert!(
            !text.to_ascii_lowercase().contains("mnrec1"),
            "公开投影不得包含恢复码前缀：{text}"
        );
    }

    // --- 可重试：用**正确**恢复码再点一次 → 成功到 E2 ---
    ui_click(&mut cx, "dev-sync-rotate");
    paste_recovery(&mut cx, &pair.recovery_code);
    ui_click(&mut cx, "dev-sync-rotate-confirm");
    drive(&mut cx, &host);
    let retried = last_report(&mut cx, &host).expect("重试报告");
    assert!(retried.is_ok(), "失败后必须能重试成功：{retried:?}");
    let facts = retried.rotation.as_ref().expect("公开事实");
    assert_eq!(facts.from_epoch, 1);
    assert_eq!(facts.to_epoch, Some(2));
    assert!(facts.recovery_code_reissued);
    assert_eq!(disk_epoch(&pair.a_path), 2);
    assert_ne!(
        active_view(&mut cx, &host),
        Some(view_before),
        "成功轮换必须重建活动视图"
    );
    assert_eq!(current_text(&mut cx, &host), pair.seed_text);
}

// ---------------------------------------------------------------------------
// 5) 被移除设备 apply：明确 relay/grant 错误、身份/视图不变、可重试（不粘死）
// ---------------------------------------------------------------------------

#[gpui::test]
async fn ui_removed_device_apply_epoch_reports_error_and_keeps_identity(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let pair = establish_pair("ui-revoked-apply");
    let relay_url = pair.relay.base_url().to_owned();
    let (host_a, a_handle) = add_shell_window(
        cx,
        pair.a_vault.clone(),
        "dev-sync-ui-revoked-a",
        config_for(&relay_url, &pair.a_path),
        pair.a_sync,
    );
    let (host_b, b_handle) = add_shell_window(
        cx,
        pair.b_vault.clone(),
        "dev-sync-ui-revoked-b",
        config_for(&relay_url, &pair.b_path),
        pair.b_sync,
    );
    let app_cx = cx.clone();
    let mut cx_a = VisualTestContext::from_window(a_handle, &app_cx);
    let mut cx_b = VisualTestContext::from_window(b_handle, &app_cx);
    cx_a.run_until_parked();
    cx_b.run_until_parked();
    wait_for_devices(&mut cx_a, &host_a, "A 读到两台设备", |members| {
        members.len() == 2
    });
    wait_for_devices(&mut cx_b, &host_b, "B 读到两台设备", |members| {
        members.len() == 2
    });
    let view_b_before = active_view(&mut cx_b, &host_b).expect("B 必须有活动文档");
    let identity_b_path = DevIdentityStore::path_for(&pair.b_path);
    let identity_b_before = std::fs::read(&identity_b_path).expect("读 B 身份失败");

    // --- A：面板移除 B（ADR 0010 默认：只删凭证，不轮换）---
    let members = panel_devices(&mut cx_a, &host_a);
    let self_a = read_panel(&mut cx_a, &host_a, DevSyncPanel::self_device).expect("本机身份");
    let b_index = members
        .iter()
        .position(|member| member.device_id != self_a)
        .expect("列表里必须有 B");
    let b_device = members[b_index].device_id;
    ui_click(&mut cx_a, probe(&format!("dev-sync-remove-{b_index}")));
    assert_eq!(
        read_panel(&mut cx_a, &host_a, DevSyncPanel::pending_revoke_target),
        Some(b_device),
        "点移除只进入二次确认"
    );
    ui_click(&mut cx_a, "dev-sync-confirm-revoke");
    wait_for_devices(&mut cx_a, &host_a, "B 变成已移除", |members| {
        members
            .iter()
            .any(|member| member.device_id == b_device && member.status == MemberStatus::Revoked)
    });
    assert_eq!(disk_epoch(&pair.a_path), 1, "只删凭证不得轮换 epoch");

    // --- A：面板轮换（撤销列表默认勾选；不手选设备 → 从 members 过滤 revoked）---
    ui_click(&mut cx_a, "dev-sync-rotate");
    assert!(read_panel(
        &mut cx_a,
        &host_a,
        DevSyncPanel::rotate_revoke_enabled
    ));
    ui_click(&mut cx_a, "dev-sync-rotate-reissue");
    assert_eq!(
        rotate_warning(&mut cx_a, &host_a),
        Some(ROTATE_NO_RECOVERY_WARNING)
    );
    ui_click(&mut cx_a, "dev-sync-rotate-confirm");
    drive(&mut cx_a, &host_a);
    let report_a = last_report(&mut cx_a, &host_a).expect("A 轮换报告");
    assert!(report_a.is_ok(), "A 轮换必须成功：{report_a:?}");
    let facts_a = report_a.rotation.as_ref().expect("公开事实");
    assert_eq!(facts_a.from_epoch, 1);
    assert_eq!(facts_a.to_epoch, Some(2));
    assert_eq!(facts_a.revoked, 1, "已移除设备必须进 signed revoke 列表");

    // --- B：点「应用轮换」→ 明确失败（已被移除，不在 grant 列表）、身份/视图不变 ---
    assert_eq!(apply_block_reason(&mut cx_b, &host_b), None);
    ui_click(&mut cx_b, "dev-sync-apply-epoch");
    assert!(read_panel(
        &mut cx_b,
        &host_b,
        DevSyncPanel::apply_confirm_open
    ));
    ui_click(&mut cx_b, "dev-sync-apply-confirm");
    drive(&mut cx_b, &host_b);
    let failed = last_report(&mut cx_b, &host_b).expect("B apply 必须留下报告");
    assert!(!failed.is_ok(), "被移除设备 apply 必须明确失败：{failed:?}");
    let error = failed.error.clone().expect("失败必须带明确原因");
    assert!(
        !error.trim().is_empty(),
        "relay/grant 失败必须给出可读原因：{error}"
    );
    assert!(
        failed.notice.contains("应用新 epoch 失败"),
        "notice 必须显示 apply 失败：{:?}",
        failed.notice
    );
    assert_eq!(disk_epoch(&pair.b_path), 1, "被移除设备必须保持旧 epoch");
    assert_eq!(
        std::fs::read(&identity_b_path).expect("读 B 身份失败"),
        identity_b_before,
        "apply 失败后 B 的身份文件必须逐字节不变"
    );
    assert_eq!(
        active_view(&mut cx_b, &host_b),
        Some(view_b_before.clone()),
        "apply 失败必须保持旧视图"
    );
    assert!(!active_held(&mut cx_b, &host_b), "失败后闸门必须释放");

    // --- 可重试（不粘死）：再点一次仍明确失败，且不自动重试/不半推广 ---
    assert!(!rotation_in_progress(&mut cx_b, &host_b));
    assert_eq!(apply_block_reason(&mut cx_b, &host_b), None);
    ui_click(&mut cx_b, "dev-sync-apply-epoch");
    ui_click(&mut cx_b, "dev-sync-apply-confirm");
    drive(&mut cx_b, &host_b);
    let retried = last_report(&mut cx_b, &host_b).expect("重试报告");
    assert!(!retried.is_ok(), "重试仍必须明确失败：{retried:?}");
    assert_eq!(disk_epoch(&pair.b_path), 1, "重试失败仍不得推进 epoch");
}
