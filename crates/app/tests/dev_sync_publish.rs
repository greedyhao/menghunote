//! ADR 0009 切片 4c-4：**在面板里发布初始基线**（不再需要"关 A → CLI publish → 重开 A"）。
//!
//! 四个测试，都用**真实** `DevRelay`（进程内 axum + 临时 SQLite，放行验签的测试脚手架，
//! ⚠️ 非生产）+ 独立 vault：
//!
//! 1. `publish_from_panel_then_b_join_sees_the_note`：A 窗口真实建笔记（点侧栏「＋ 笔记」+
//!    键入）→ 点面板「发布基线」→ **A 不重启**，B 只凭 `vault_id + 恢复码` Join 就读到正文；
//! 2. `publish_without_manager_key_is_refused`：Join 设备（没有管理密钥）点发布 → 面板明确
//!    提示、宿主/worker 明确拒绝、且不产生任何发布；
//! 3. `publish_requires_a_held_gate_and_rejects_duplicates`：没有活动文档 / 裸 worker 命令
//!    （无 `HeldGate`）/ 同步在途时重复点发布 → 一律明确拒绝；失败后闸门释放、输入恢复；
//! 4. `publish_twice_is_idempotent_and_sync_still_works`：发布 → 继续编辑 + 同步成功 →
//!    再发布一次（4b 幂等，durable 基线事实指向较新那份，B 仍能 Join）。
//!
//! ## 纪律（AGENTS.md §4/§6）
//!
//! - 临时目录一律 `CARGO_TARGET_TMPDIR`（`target/tmp`），不用 `%TEMP%`；
//! - **不 sleep**：收敛判据是 `dev_sync_sync_in_progress()` 转 false、worker 明确终态、
//!   闸门 `Held`/释放这些**状态**（忙等只用 `yield_now` + 有界 deadline）；
//! - Linux 用户主键的 `XDG_DATA_HOME` 也指到 `target/tmp`（Windows 是 no-op）；
//! - relay 只用于 dev：本文件**不能**作为鉴权/验签验收。
//!
//! feature 关闭时整个文件编译为空（0 个测试）。

#![cfg(feature = "dev-sync")]

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use gpui::{
    div, App, AppContext as _, Context, Entity, InteractiveElement as _, Modifiers,
    ParentElement as _, Render, Styled as _, TestAppContext, VisualTestContext,
};
use menghunote_app::dev_sync::gate::DevSyncConfig;
use menghunote_app::dev_sync::service::ServiceConfig;
use menghunote_app::dev_sync::setup::{self, SetupMode, SetupSecrets};
use menghunote_app::dev_sync::{Command, HeldGate};
use menghunote_app::dev_vault;
use menghunote_app::editor::view::EditorView;
use menghunote_app::shell::AppShell;
use menghunote_dev_relay::DevRelay;
use menghunote_storage::{
    append_updates_with_sync_intents, list_tree, load_body_doc, load_head, local_sync_intent,
    BodyDoc, HeadState, StructureWriter, SyncContext, UpdateRecord, Vault,
};
use menghunote_sync::dev_local::DevLocal;
use menghunote_sync::ids;
use tempfile::{Builder as TempDirBuilder, TempDir};

/// 收敛的安全上限（真实 relay 是 loopback，正常毫秒级；超时说明状态机卡死）。
const DEADLINE: Duration = Duration::from_secs(30);

/// 测试进程级的 `XDG_DATA_HOME`（Linux 用户主键；Windows 是 no-op）。
fn ensure_test_user_key() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("dev-sync-publish-xdg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("创建测试 XDG_DATA_HOME 失败");
        std::env::set_var("XDG_DATA_HOME", &dir);
        menghunote_sync::identity_wrap::initialize_user_key()
            .expect("初始化测试用户主键失败（ADR 0008）");
    });
}

/// 在 `CARGO_TARGET_TMPDIR`（`target/tmp`）下开一个独立临时目录。
fn temp_root(prefix: &str) -> TempDir {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("dev-sync-publish-{prefix}-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("创建 target/tmp 下的测试根目录失败");
    TempDirBuilder::new()
        .prefix("case-")
        .tempdir_in(&root)
        .expect("在 target/tmp 下创建临时目录失败")
}

/// 窗口根视图：给 `AppShell` 一个稳定的宿主 selector（与其它 headless 测试同构）。
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
// 实例准备（生产路径）：Create / Join / 直接写一篇笔记
// ---------------------------------------------------------------------------

/// A 侧 Create 之后的句柄（身份 + 库 + 恢复码；`CreatedVault` 只在这里用一次）。
struct SideA {
    vault: Vault,
    identity: menghunote_sync::identity::LocalIdentityV1,
    vault_id: menghunote_sync::VaultId,
    peer: u64,
    epoch: u64,
    recovery: String,
    path: PathBuf,
}

/// 走生产入口 Create（`setup::prepare`）并重建窗口用的库句柄（**不播种**演示树）。
fn create_a(relay: &DevRelay, path: PathBuf) -> SideA {
    let outcome = setup::prepare(
        &config_for(relay.base_url(), &path),
        SetupMode::Create,
        create_secrets(relay),
    )
    .expect("A Create 失败");
    let peer = outcome.identity.loro_peer_id();
    let epoch = outcome.identity.current_epoch;
    let vault_id = outcome.vault_id;
    let recovery = outcome
        .recovery_code
        .clone()
        .expect("Create 必须返回恢复码");
    let identity = outcome.identity;
    drop(outcome.vault);
    let dev =
        dev_vault::open_vault_at_with_peer_opts(&path, peer, false).expect("A 重新打开实例库失败");
    assert!(!dev.seeded, "dev-sync Create 之后不得再播种演示树");
    SideA {
        vault: dev.vault,
        identity,
        vault_id,
        peer,
        epoch,
        recovery,
        path,
    }
}

/// 用真实 storage 路径写一篇笔记（结构 CRDT + 正文，同一 A 事务 + sync intent）。
fn seed_note(vault: &Vault, peer: u64, epoch: u64, title: &str, text: &str) -> String {
    let ctx = SyncContext::new(peer, epoch);
    let mut writer =
        StructureWriter::open_with_sync(vault, peer, Some(ctx)).expect("打开结构写入口失败");
    let seed = writer
        .doc()
        .create_note(title, 1)
        .expect("结构 CRDT 新建笔记失败");
    writer
        .commit("dev-sync-publish-seed")
        .expect("结构提交失败");

    let (doc, head) = load_body_doc(vault, &seed.body_doc_id, peer, None).expect("加载正文失败");
    doc.insert_scalars(0, text).expect("写入正文失败");
    commit_body(
        vault,
        &seed.body_doc_id,
        peer,
        epoch,
        &doc,
        &head,
        "dev-sync-publish",
    );
    seed.body_doc_id
}

fn commit_body(
    vault: &Vault,
    doc_id: &str,
    peer: u64,
    epoch: u64,
    doc: &BodyDoc,
    head: &HeadState,
    batch: &str,
) {
    let base = head.version_vector().expect("解码 head VV 失败");
    let update = doc.pending_update(&base).expect("导出正文增量失败");
    let seq = head.head_seq.saturating_add(1);
    let intent = local_sync_intent(doc_id, seq, batch, SyncContext::new(peer, epoch))
        .expect("派生 sync intent 失败");
    append_updates_with_sync_intents(
        vault,
        doc_id,
        head.head_seq,
        &[UpdateRecord::new(intent.batch_id.clone(), update)],
        &[intent],
    )
    .expect("A 事务写入正文 + intent 失败");
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
    f: impl FnOnce(&AppShell, &App) -> R,
) -> R {
    cx.update(|_window, app| f(host.read(app).shell.read(app), app))
}

fn current_text(cx: &mut VisualTestContext, host: &Entity<ShellHost>) -> String {
    read_shell(cx, host, |shell, app| shell.dev_sync_current_text(app))
        .expect("当前活动文档的镜像正文")
}

fn active_view(cx: &mut VisualTestContext, host: &Entity<ShellHost>) -> Option<Entity<EditorView>> {
    read_shell(cx, host, |shell, _app| shell.dev_sync_active_view())
}

fn active_doc_id(cx: &mut VisualTestContext, host: &Entity<ShellHost>) -> Option<String> {
    read_shell(cx, host, |shell, app| {
        shell
            .dev_sync_active_view()
            .map(|view| view.read(app).dev_sync_doc_id().to_owned())
    })
}

fn active_held(cx: &mut VisualTestContext, host: &Entity<ShellHost>) -> bool {
    read_shell(cx, host, |shell, app| {
        shell
            .dev_sync_active_view()
            .is_some_and(|view| view.read(app).dev_sync_is_held())
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

fn panel_sync_line(cx: &mut VisualTestContext, host: &Entity<ShellHost>) -> Option<String> {
    read_shell(cx, host, |shell, app| {
        shell.dev_sync_panel().and_then(|panel| {
            panel
                .read(app)
                .sync_line_text()
                .map(std::borrow::ToOwned::to_owned)
        })
    })
}

fn last_publish(
    cx: &mut VisualTestContext,
    host: &Entity<ShellHost>,
) -> Option<menghunote_app::dev_sync::SyncOnceReport> {
    read_shell(cx, host, |shell, _app| {
        shell.dev_sync_last_publish().cloned()
    })
}

fn last_sync(
    cx: &mut VisualTestContext,
    host: &Entity<ShellHost>,
) -> Option<menghunote_app::dev_sync::SyncOnceReport> {
    read_shell(cx, host, |shell, _app| shell.dev_sync_last_sync().cloned())
}

fn publish_in_progress(cx: &mut VisualTestContext, host: &Entity<ShellHost>) -> bool {
    read_shell(cx, host, |shell, _app| shell.dev_sync_publish_in_progress())
}

fn sync_in_progress(cx: &mut VisualTestContext, host: &Entity<ShellHost>) -> bool {
    read_shell(cx, host, |shell, _app| shell.dev_sync_sync_in_progress())
}

fn service(
    cx: &mut VisualTestContext,
    host: &Entity<ShellHost>,
) -> std::sync::Arc<menghunote_app::dev_sync::SyncService> {
    read_shell(cx, host, |shell, app| {
        shell
            .dev_sync_panel()
            .map(|panel| panel.read(app).service().clone())
            .expect("面板必须挂载")
    })
}

/// 点正文首行焦点（真实命中链路）。
fn focus_editor(cx: &mut VisualTestContext) {
    let pane = cx.debug_bounds("editor-pane").expect("编辑器面板 selector");
    // 笔记顶部有固定 header（路径行 + 大标题，约 90px）：点 y+10 会落在 header 上，
    // 既不会提交"新建笔记后自动进入的标题编辑"，也不会把焦点交给正文。
    // 点在正文上部，走真实的「点正文 → 提交标题编辑 + 正文拿焦点」路径。
    cx.simulate_click(
        gpui::point(
            pane.origin.x + gpui::px(20.0),
            pane.origin.y + gpui::px(140.0),
        ),
        Modifiers::default(),
    );
    cx.run_until_parked();
}

/// 点面板「发布基线」按钮（真实点击路径）。
fn click_publish(cx: &mut VisualTestContext, host: &Entity<ShellHost>) {
    // 设置页是模态的：前面 focus_editor 的点击会落到遮罩上并关闭它；点「发布」前重新打开。
    with_shell(cx, host, |shell, cx| shell.open_settings_headless(cx));
    cx.run_until_parked();
    let button = cx
        .debug_bounds("dev-sync-publish")
        .expect("面板必须画出 dev-sync-publish");
    cx.simulate_click(button.center(), Modifiers::default());
    cx.run_until_parked();
}

/// 点侧栏「＋ 笔记」（真实命中路径）。
fn click_new_note(cx: &mut VisualTestContext) {
    let button = cx
        .debug_bounds("sidebar-new-note")
        .expect("侧栏必须画出「＋ 笔记」");
    cx.simulate_click(button.center(), Modifiers::default());
    cx.run_until_parked();
}

/// 泵到"持闸任务结束"（状态收敛，不是 sleep）。
fn drive_until_idle(cx: &mut VisualTestContext, host: &Entity<ShellHost>, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        with_shell(cx, host, |shell, cx| shell.dev_sync_pump(cx));
        cx.run_until_parked();
        if !sync_in_progress(cx, host) && !publish_in_progress(cx, host) {
            return;
        }
        assert!(Instant::now() < deadline, "持闸任务 30s 内没有收敛");
        std::thread::yield_now();
    }
}

/// 在面板实体上执行一次宿主/测试动作（`request_publish` 等）。
fn with_panel<R>(
    cx: &mut VisualTestContext,
    host: &Entity<ShellHost>,
    f: impl FnOnce(
        &mut menghunote_app::dev_sync::DevSyncPanel,
        &mut gpui::Context<menghunote_app::dev_sync::DevSyncPanel>,
    ) -> R,
) -> R {
    cx.update(|_window, app| {
        let panel = host
            .read(app)
            .shell
            .read(app)
            .dev_sync_panel()
            .expect("面板必须挂载");
        panel.update(app, f)
    })
}

/// 泵到面板拿到 durable 基线事实（发布成功后刷新设备列表才会带回 `baseline_known`）。
fn drive_baseline_fact(cx: &mut VisualTestContext, host: &Entity<ShellHost>, present: bool) {
    let deadline = Instant::now() + DEADLINE;
    loop {
        cx.executor()
            .advance_clock(menghunote_app::dev_sync::SYNC_PUMP_INTERVAL);
        with_shell(cx, host, |shell, cx| shell.dev_sync_pump(cx));
        cx.run_until_parked();
        let ready = read_shell(cx, host, |shell, app| {
            shell.dev_sync_panel().is_some_and(|panel| {
                let snapshot = panel.read(app).service().snapshot();
                snapshot.baseline_known
                    && snapshot.published_baseline.is_some() == present
                    && snapshot.in_flight.is_none()
            })
        });
        if ready {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "面板 30s 内没有读到预期的基线事实（present={present}）"
        );
        std::thread::yield_now();
    }
}

/// 泵到面板拿到 worker 上报的设备身份（`can_manage` 只有 worker 能告诉我们）。
///
/// 面板打开时的首次列表刷新由它自己的 40 ms 泵派发，所以这里必须同时**推进测试时钟**
/// （无头测试没有真实时钟；不 advance 就永远不会派发 `ListDevices`）。
fn drive_device_facts(cx: &mut VisualTestContext, host: &Entity<ShellHost>) {
    let deadline = Instant::now() + DEADLINE;
    loop {
        cx.executor()
            .advance_clock(menghunote_app::dev_sync::SYNC_PUMP_INTERVAL);
        with_shell(cx, host, |shell, cx| shell.dev_sync_pump(cx));
        cx.run_until_parked();
        let ready = read_shell(cx, host, |shell, app| {
            shell
                .dev_sync_panel()
                .map(|panel| {
                    let snapshot = panel.read(app).service().snapshot();
                    snapshot.self_device.is_some() && snapshot.in_flight.is_none()
                })
                .unwrap_or(false)
        });
        if ready {
            return;
        }
        assert!(Instant::now() < deadline, "面板 30s 内没有读到设备身份");
        std::thread::yield_now();
    }
}

// ---------------------------------------------------------------------------
// 1) 面板发布 → B Join 读到 A 的笔记（**A 不重启**）
// ---------------------------------------------------------------------------

#[gpui::test]
async fn publish_from_panel_then_b_join_sees_the_note(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let dir = temp_root("panel");
    let a = create_a(&relay, dir.path().join("inst-a").join("vault.sqlite3"));
    assert!(
        list_tree(&a.vault).expect("读结构树失败").is_empty(),
        "Create 之后必须是空树（用户点「＋ 笔记」才有内容）"
    );
    let relay_url = relay.base_url().to_owned();
    let a_vault_id = a.vault_id;
    let recovery = a.recovery.clone();

    let (host, handle) = add_shell_window(
        cx,
        a.vault.clone(),
        "dev-sync-publish-panel",
        config_for(&relay_url, &a.path),
        SyncContext::new(a.peer, a.epoch),
    );
    let app_cx = cx.clone();
    let mut cx = VisualTestContext::from_window(handle, &app_cx);
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("dev-sync-panel").is_some(),
        "dev-sync 面板必须挂载"
    );
    // Create（CLI 路径）没有基线：读到事实后面板才渲染「重试发布基线」。
    drive_device_facts(&mut cx, &host);
    assert!(
        cx.debug_bounds("dev-sync-publish").is_some(),
        "基线缺失时面板必须画出「重试发布基线」按钮"
    );
    // Create 之后没有活动文档：发布必须先被明确拒绝（发布需要持闸）。
    let error = with_shell(&mut cx, &host, |shell, cx| {
        shell.dev_sync_publish_baseline(cx)
    })
    .expect_err("没有活动文档时发布必须被拒绝");
    assert!(
        error.contains("没有打开的笔记"),
        "拒绝原因必须说清没有活动文档：{error}"
    );

    // 1) A 窗口真实建笔记（点侧栏按钮）+ 键入正文。
    click_new_note(&mut cx);
    assert_eq!(
        list_tree(&a.vault).expect("读结构树失败").len(),
        1,
        "点「＋ 笔记」必须真的建出 1 篇笔记"
    );
    let view_before = active_view(&mut cx, &host).expect("新建笔记必须被打开");
    let doc_id = active_doc_id(&mut cx, &host).expect("必须知道活动正文 doc id");
    focus_editor(&mut cx);
    cx.simulate_input("面板发布路径的正文");
    cx.run_until_parked();
    assert!(
        current_text(&mut cx, &host).contains("面板发布路径的正文"),
        "键入必须立刻进镜像正文"
    );

    // 2) 点面板「发布基线」：只发请求，AppShell 持闸后派发 worker。
    drive_device_facts(&mut cx, &host);
    click_publish(&mut cx, &host);
    let notice = panel_notice(&mut cx, &host).expect("点击必须写面板 notice");
    assert!(
        notice.contains("请求发布基线"),
        "按钮只发请求（持闸由 AppShell 做）：{notice:?}"
    );
    drive_until_idle(&mut cx, &host, DEADLINE);
    let report = last_publish(&mut cx, &host).expect("发布必须留下报告");
    assert!(
        report.dispatched_under_gate,
        "发布命令必须在 Held 之后派发（顺序证明）：{report:?}"
    );
    assert!(report.is_ok(), "面板发布必须成功：{report:?}");
    assert_eq!(report.error, None, "发布不得留下错误：{report:?}");
    assert!(
        report.notice.contains("初始基线已发布") && report.notice.contains("覆盖 seq"),
        "notice 必须带 checkpoint 短 id/覆盖 seq：{report:?}"
    );
    assert!(
        !active_held(&mut cx, &host),
        "发布完成后闸门必须释放（否则编辑器永久冻结）"
    );
    assert_eq!(
        active_view(&mut cx, &host),
        Some(view_before),
        "发布不能重开/切换活动文档（A 进程没有重启）"
    );
    let line = panel_sync_line(&mut cx, &host).expect("发布也必须写持闸任务行");
    assert!(
        line.contains("初始基线已发布"),
        "面板任务行必须显示发布结果：{line:?}"
    );
    // 发布成功后 durable 事实刷新：按钮必须消失（正常实例不显示发布入口）。
    drive_baseline_fact(&mut cx, &host, true);
    assert!(
        cx.debug_bounds("dev-sync-publish").is_none(),
        "基线已发布后不得再渲染发布按钮"
    );

    // 3) durable 基线事实 + notice 里的短 checkpoint id 必须一致。
    let baseline = DevLocal::published_baseline(&a.vault)
        .expect("读已发布基线失败")
        .expect("发布后必须留下 durable 基线事实");
    let short: String = ids::hex_of(&baseline.checkpoint_id)
        .chars()
        .take(8)
        .collect();
    assert!(
        report.notice.contains(&short),
        "notice 必须出现 checkpoint 短 id {short}：{report:?}"
    );

    // 4) B 只凭 vault_id + 恢复码 Join：读到 A 在窗口里建的那篇（A 没有重启过）。
    let b_path = dir.path().join("inst-b").join("vault.sqlite3");
    let b = DevLocal::join(&relay_url, &a_vault_id, &recovery, &b_path).expect("B Join 失败");
    let b_tree = list_tree(&b.vault).expect("读 B 结构树失败");
    assert!(
        b_tree.iter().any(|entry| entry.is_note()),
        "B 的树必须来自刚发布的那份基线：{b_tree:?}"
    );
    let b_text = load_body_doc(&b.vault, &doc_id, b.identity.loro_peer_id(), None)
        .expect("读 B 正文失败")
        .0
        .text();
    assert!(
        b_text.contains("面板发布路径的正文"),
        "B 必须读到 A 在窗口里键入的正文：{:?}",
        &b_text[..b_text.len().min(48)]
    );
}

// ---------------------------------------------------------------------------
// 2) Join 设备（没有管理密钥）不能发布
// ---------------------------------------------------------------------------

#[gpui::test]
async fn publish_without_manager_key_is_refused(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let dir = temp_root("no-manager");
    let a = create_a(&relay, dir.path().join("inst-a").join("vault.sqlite3"));
    // A 先有一篇笔记并发布（CLI 路径，用恢复码），这样 B 才 Join 得到基线。
    let note_doc = seed_note(&a.vault, a.peer, a.epoch, "基线笔记", "基线正文\n");
    DevLocal::publish_initial_pair(relay.base_url(), &a.vault, &a.identity, &a.recovery)
        .expect("A 发布基线失败");
    let b_path = dir.path().join("inst-b").join("vault.sqlite3");
    let b = setup::prepare(
        &config_for(relay.base_url(), &b_path),
        SetupMode::Join,
        SetupSecrets {
            vault_id: Some(ids::hex_of(&a.vault_id)),
            recovery_code: Some(a.recovery.clone()),
            ..SetupSecrets::default()
        },
    )
    .expect("B Join 失败");
    let b_peer = b.identity.loro_peer_id();
    let b_epoch = b.identity.current_epoch;
    drop(b.vault);
    let dev_b =
        dev_vault::open_vault_at_with_peer_opts(&b_path, b_peer, false).expect("B 重新打开失败");

    let relay_url = relay.base_url().to_owned();
    let (host, handle) = add_shell_window(
        cx,
        dev_b.vault.clone(),
        "dev-sync-publish-no-manager",
        config_for(&relay_url, &b_path),
        SyncContext::new(b_peer, b_epoch),
    );
    let app_cx = cx.clone();
    let mut cx = VisualTestContext::from_window(handle, &app_cx);
    cx.run_until_parked();
    drive_device_facts(&mut cx, &host);
    let can_manage = read_shell(&mut cx, &host, |shell, app| {
        shell
            .dev_sync_panel()
            .map(|panel| panel.read(app).can_manage_devices())
            .unwrap_or(true)
    });
    assert!(!can_manage, "Join 设备必须没有管理密钥（按钮置灰的前提）");
    assert!(
        cx.debug_bounds("dev-sync-publish").is_none(),
        "Join 设备不得渲染发布入口（无论基线是否存在）"
    );

    // 面板入口仍必须给明确原因（即使按钮不渲染，也要能诊断伪造/过期的调用）。
    with_panel(&mut cx, &host, |panel, cx| panel.request_publish(cx));
    let notice = panel_notice(&mut cx, &host).expect("请求必须写 notice");
    assert!(
        notice.contains("没有管理密钥"),
        "Join 设备请求发布必须明确提示缺管理密钥：{notice:?}"
    );
    assert!(
        !publish_in_progress(&mut cx, &host),
        "被拒绝的发布不得进入持闸任务"
    );

    // 宿主入口也必须明确拒绝（不只靠按钮置灰）。
    let error = with_shell(&mut cx, &host, |shell, cx| {
        shell.dev_sync_publish_baseline(cx)
    })
    .expect_err("Join 设备调用发布入口必须被拒绝");
    assert!(
        error.contains("管理密钥"),
        "宿主拒绝原因必须说清缺管理密钥：{error}"
    );
    assert!(
        !active_held(&mut cx, &host) && !publish_in_progress(&mut cx, &host),
        "被拒绝的发布不得持闸"
    );

    // worker 层再兜一次：即使有人绕过 UI 直接派发带持闸证明的命令，也必须拒绝。
    let doc_id = active_doc_id(&mut cx, &host).expect("B 必须打开了基线里的笔记");
    assert_eq!(doc_id, note_doc, "B 打开的是基线里那篇");
    let head_seq = load_head(&dev_b.vault, &doc_id)
        .expect("读 B 的活动文档 head 失败")
        .head_seq;
    let service = service(&mut cx, &host);
    let op = service
        .try_command_with_gate(
            Command::PublishInitialPair,
            HeldGate::new(doc_id.clone(), head_seq, 1),
        )
        .expect("带持闸证明的命令应被服务接受（拒绝要发生在 worker 里）");
    drop(service);
    let deadline = Instant::now() + DEADLINE;
    loop {
        with_shell(&mut cx, &host, |shell, cx| shell.dev_sync_pump(cx));
        cx.run_until_parked();
        let done = read_shell(&mut cx, &host, |shell, app| {
            shell
                .dev_sync_panel()
                .map(|panel| {
                    let snapshot = panel.read(app).service().snapshot();
                    snapshot
                        .last_completed
                        .as_ref()
                        .is_some_and(|(id, _, _)| *id == op)
                })
                .unwrap_or(false)
        });
        if done {
            break;
        }
        assert!(Instant::now() < deadline, "worker 拒绝 30s 内没有回执");
        std::thread::yield_now();
    }
    let error_text = read_shell(&mut cx, &host, |shell, app| {
        shell
            .dev_sync_panel()
            .map(|panel| panel.read(app).error_text())
            .unwrap_or_default()
    });
    assert!(
        error_text.contains("管理密钥"),
        "worker 必须再拒绝一次并给出明确原因：{error_text}"
    );
}

// ---------------------------------------------------------------------------
// 3) 持闸纪律：没有活动文档 / 裸命令 / 同步在途 一律拒绝；失败后输入恢复
// ---------------------------------------------------------------------------

/// "只 listen、不应答"的 relay：connect 成功但永远等不到响应（确定性造出"在途"窗口）。
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

#[gpui::test]
async fn publish_requires_a_held_gate_and_rejects_duplicate_commands(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let dir = temp_root("gate");
    // Create 用真实 relay；窗口里的 worker 指向黑洞（本地闸门/库照常，只有网络失败）。
    let a = create_a(&relay, dir.path().join("inst-a").join("vault.sqlite3"));
    let blackhole = BlackholeRelay::start();
    let (host, handle) = add_shell_window(
        cx,
        a.vault.clone(),
        "dev-sync-publish-gate",
        config_for(&blackhole.url(), &a.path),
        SyncContext::new(a.peer, a.epoch),
    );
    let app_cx = cx.clone();
    let mut cx = VisualTestContext::from_window(handle, &app_cx);
    cx.run_until_parked();

    // 1) 没有活动文档 → 发布明确拒绝（发布需要持闸，持闸需要一个活动文档）。
    let error = with_shell(&mut cx, &host, |shell, cx| {
        shell.dev_sync_publish_baseline(cx)
    })
    .expect_err("没有活动文档时发布必须被拒绝");
    assert!(error.contains("没有打开的笔记"), "{error}");

    // 2) 裸 worker 命令（**没有** HeldGate）→ worker 明确拒绝（不盲跑）。
    let service = service(&mut cx, &host);
    let op = service
        .try_command(Command::PublishInitialPair)
        .expect("服务层接受命令，拒绝必须发生在 worker 里");
    drop(service);
    let deadline = Instant::now() + DEADLINE;
    loop {
        with_shell(&mut cx, &host, |shell, cx| shell.dev_sync_pump(cx));
        cx.run_until_parked();
        let error_text = read_shell(&mut cx, &host, |shell, app| {
            shell
                .dev_sync_panel()
                .map(|panel| {
                    let snapshot = panel.read(app).service().snapshot();
                    if snapshot
                        .last_completed
                        .as_ref()
                        .is_some_and(|(id, _, _)| *id == op)
                    {
                        snapshot.last_error.clone().unwrap_or_default()
                    } else {
                        String::new()
                    }
                })
                .unwrap_or_default()
        });
        if !error_text.is_empty() {
            assert!(
                error_text.contains("持闸"),
                "裸命令必须被持闸证明拦下：{error_text}"
            );
            break;
        }
        assert!(Instant::now() < deadline, "裸命令 30s 内没有明确失败");
        std::thread::yield_now();
    }

    // 3) 有笔记后：先让"同步"在网络阶段在途（黑洞不应答），再点发布 → Busy 拒绝。
    //    ADR 0012 S2a：网络阶段不持闸，所以这里等的是"同步在途"，不是 Held。
    click_new_note(&mut cx);
    assert_eq!(list_tree(&a.vault).expect("读结构树失败").len(), 1);
    focus_editor(&mut cx);
    cx.simulate_input("gate-path");
    cx.run_until_parked();
    with_shell(&mut cx, &host, |shell, cx| shell.dev_sync_sync_once(cx))
        .unwrap_or_else(|error| panic!("持闸同步被拒绝：{error}"));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        with_shell(&mut cx, &host, |shell, cx| shell.dev_sync_pump(cx));
        cx.run_until_parked();
        if read_shell(&mut cx, &host, |shell, _app| {
            shell.dev_sync_sync_in_progress()
        }) {
            break;
        }
        assert!(Instant::now() < deadline, "同步 10s 内没有在途");
        std::thread::yield_now();
    }
    assert!(!active_held(&mut cx, &host), "网络阶段（Prepare）不持闸");
    let duplicate = with_shell(&mut cx, &host, |shell, cx| {
        shell.dev_sync_publish_baseline(cx)
    })
    .expect_err("同步在途时必须拒绝发布");
    assert!(
        duplicate.contains("同步已在进行"),
        "拒绝原因必须说清同步在途（单操作串行）：{duplicate}"
    );
    assert!(!active_held(&mut cx, &host), "被拒后网络阶段仍不持闸");
    assert!(
        !publish_in_progress(&mut cx, &host),
        "被拒绝的发布不得占用任务槽"
    );

    // 4) 关掉黑洞 → worker 明确失败 → 闸门释放、正文不变、输入恢复。
    drop(blackhole);
    drive_until_idle(&mut cx, &host, DEADLINE);
    let sync_report = last_sync(&mut cx, &host).expect("失败也必须留下同步报告");
    assert!(
        sync_report.error.is_some(),
        "relay 不可达必须是明确失败：{sync_report:?}"
    );
    assert!(!active_held(&mut cx, &host), "失败后闸门必须释放");
    let text_after_failure = current_text(&mut cx, &host);
    assert_eq!(text_after_failure, "gate-path", "同步失败不得改动正文");
    focus_editor(&mut cx);
    cx.simulate_input("R");
    cx.run_until_parked();
    assert!(
        current_text(&mut cx, &host).contains('R'),
        "失败释放闸门后本地键入必须恢复生效"
    );

    // 5) 恢复后再点发布：可以走完整持闸路径（黑洞已关 → 明确失败，但闸门/正文安全）。
    let before_publish_text = current_text(&mut cx, &host);
    with_shell(&mut cx, &host, |shell, cx| {
        shell.dev_sync_publish_baseline(cx)
    })
    .expect("闸门空闲时发布入口必须接受（网络失败由 worker 报告）");
    drive_until_idle(&mut cx, &host, DEADLINE);
    let publish_report = last_publish(&mut cx, &host).expect("发布失败也必须留下报告");
    assert!(
        publish_report.dispatched_under_gate,
        "发布必须在 Held 之后派发：{publish_report:?}"
    );
    assert!(
        publish_report.error.is_some(),
        "relay 不可达时发布必须明确失败（不能谎报成功）：{publish_report:?}"
    );
    assert!(!active_held(&mut cx, &host), "发布失败后闸门同样必须释放");
    assert_eq!(
        current_text(&mut cx, &host),
        before_publish_text,
        "发布失败不得改动正文"
    );
}

// ---------------------------------------------------------------------------
// 4) 发布两次 + 发布后继续编辑/同步
// ---------------------------------------------------------------------------

#[gpui::test]
async fn publish_twice_is_idempotent_and_sync_still_works(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let dir = temp_root("twice");
    let a = create_a(&relay, dir.path().join("inst-a").join("vault.sqlite3"));
    let relay_url = relay.base_url().to_owned();
    let a_vault_id = a.vault_id;
    let recovery = a.recovery.clone();
    let (host, handle) = add_shell_window(
        cx,
        a.vault.clone(),
        "dev-sync-publish-twice",
        config_for(&relay_url, &a.path),
        SyncContext::new(a.peer, a.epoch),
    );
    let app_cx = cx.clone();
    let mut cx = VisualTestContext::from_window(handle, &app_cx);
    cx.run_until_parked();

    // 笔记 + 正文 → 第一次发布。
    click_new_note(&mut cx);
    focus_editor(&mut cx);
    cx.simulate_input("第一版正文");
    cx.run_until_parked();
    let doc_id = active_doc_id(&mut cx, &host).expect("必须有活动文档");
    drive_device_facts(&mut cx, &host);
    click_publish(&mut cx, &host);
    drive_until_idle(&mut cx, &host, DEADLINE);
    let first = last_publish(&mut cx, &host).expect("第一次发布报告");
    assert!(first.is_ok(), "第一次发布必须成功：{first:?}");
    let baseline_first = DevLocal::published_baseline(&a.vault)
        .expect("读基线事实失败")
        .expect("第一次发布必须留下 durable 基线事实");

    // 发布完成后：输入恢复、继续编辑 → 同步成功（发布没有留下冻结/脏基准）。
    focus_editor(&mut cx);
    // adopt 之后 caret 可能停在字符之间；先按 End 移到行尾再输入（避免与已有文字交错）。
    cx.simulate_keystrokes("end");
    cx.run_until_parked();
    cx.simulate_input("，发布后继续编辑");
    cx.run_until_parked();
    assert!(
        current_text(&mut cx, &host).contains("发布后继续编辑"),
        "发布后必须还能继续输入"
    );
    with_shell(&mut cx, &host, |shell, cx| shell.dev_sync_sync_once(cx))
        .expect("发布完成后同步入口必须可用");
    drive_until_idle(&mut cx, &host, DEADLINE);
    let sync_report = last_sync(&mut cx, &host).expect("同步报告");
    assert!(
        sync_report.is_ok(),
        "发布之后同步必须正常（闸门/基准没有残留）：{sync_report:?}"
    );

    // 第二次发布：4b 幂等语义——成功、checkpoint 不同、durable 事实指向较新那份。
    // 基线已存在时面板**不渲染**发布按钮，所以这里走宿主入口（同一条持闸状态机）。
    with_shell(&mut cx, &host, |shell, cx| {
        shell.dev_sync_publish_baseline(cx)
    })
    .expect("基线已存在时宿主入口仍可用于重发（幂等）");
    drive_until_idle(&mut cx, &host, DEADLINE);
    let second = last_publish(&mut cx, &host).expect("第二次发布报告");
    assert!(second.is_ok(), "重跑发布必须可重试：{second:?}");
    assert_eq!(second.error, None, "{second:?}");
    let baseline_second = DevLocal::published_baseline(&a.vault)
        .expect("读基线事实失败")
        .expect("重跑后仍必须有 durable 基线事实");
    assert_ne!(
        baseline_first.checkpoint_id, baseline_second.checkpoint_id,
        "重跑发布必须产生新的 checkpoint（不是复用旧事实）"
    );
    assert!(
        baseline_second.covered_seq >= baseline_first.covered_seq,
        "新基线的覆盖水位不能倒退"
    );

    // B 在两次发布之后仍能 Join，并读到发布后继续编辑的正文（最新两份 published 可用）。
    let b_path = dir.path().join("inst-b").join("vault.sqlite3");
    let b = DevLocal::join(&relay_url, &a_vault_id, &recovery, &b_path)
        .expect("两次发布之后 B Join 必须仍成功");
    let b_text = load_body_doc(&b.vault, &doc_id, b.identity.loro_peer_id(), None)
        .expect("读 B 正文失败")
        .0
        .text();
    assert!(
        b_text.contains("第一版正文") && b_text.contains("发布后继续编辑"),
        "B 必须读到发布前后的正文：{:?}",
        &b_text[..b_text.len().min(64)]
    );
}
