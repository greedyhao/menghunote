//! ADR 0012 S3：**自动同步接线**（真实 `AppShell` + 真实 `dev-relay` / 脚本 runner）。
//!
//! ## 纪律（AGENTS.md §4/§6）
//!
//! - **不 sleep、不猜步数**：时钟是注入的 [`ManualInstantClock`]，每个用例显式推进；
//!   收敛判据永远是**明确状态**（`Completed` 报告 / `dev_sync_sync_in_progress()` /
//!   `StatusState` / 脚本 runner 的显式事件计数），不是"轮询够了"或"步数用完了"；
//! - **不用 `yield_now` 自旋**：脚本 runner 在 entered / NeedGate / GrantGate /
//!   Integrated / Finish / Cancel 等关键点用 `Condvar` 唤醒测试（[`wait_script`] /
//!   [`pump_until`]）；真实 runner（relay + SQLite 驱动）没有测试可见的显式信号，那里
//!   只把 CPU 让给后台线程（[`let_worker_run`]），判据仍是明确状态；
//! - [`CONVERGE_DEADLINE`] 是**死锁护栏**（墙钟），不是收敛条件：收敛判据只有各用例
//!   的明确状态/事件；
//! - 墙钟 ticker 一律**关闭**（`AutoSyncConfig::no_ticker`），每拍显式
//!   `AppShell::dev_sync_auto_tick`；
//! - 临时目录 `CARGO_TARGET_TMPDIR`（`target/tmp`），不用 `%TEMP%`；
//! - Linux 用户主键的 `XDG_DATA_HOME` 也指到 `target/tmp`（Windows 是 no-op）。
//!
//! feature 关闭时整个文件编译为空（0 个测试）。

#![cfg(feature = "dev-sync")]

use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use gpui::{
    div, App, AppContext as _, Context, Entity, InteractiveElement as _, Modifiers,
    ParentElement as _, Render, Styled as _, TestAppContext, VisualTestContext,
};
use menghunote_app::dev_sync::gate::DevSyncConfig;
use menghunote_app::dev_sync::scheduler::{SchedulerState, Trigger};
use menghunote_app::dev_sync::service::{
    OperationClock, ServiceConfig, SyncControl, SyncRunner, SyncService,
};
use menghunote_app::dev_sync::setup::{self, SetupMode, SetupSecrets};
use menghunote_app::dev_sync::{
    AutoSync, AutoSyncConfig, Command, Event, HeldGate, OpId, OpOrigin, OpOutcome, Phase,
    StatusState, SyncOnceReport, SyncPhase,
};
use menghunote_app::dev_vault;
use menghunote_app::editor::view::EditorView;
use menghunote_app::shell::AppShell;
use menghunote_dev_relay::DevRelay;
use menghunote_storage::{
    append_updates_with_sync_intents, list_tree, load_body_doc, local_sync_intent,
    store_inbox_verified, SyncContext, UpdateRecord, Vault,
};
use menghunote_sync::dev_local::DevLocal;
use menghunote_sync::identity::LocalIdentityV1;
use menghunote_sync::ids;
use tempfile::{Builder as TempDirBuilder, TempDir};

/// 死锁护栏（**不是**收敛判据）：卡住时在 30s 墙钟内给出可读失败，而不是挂住 CI。
const CONVERGE_DEADLINE: Duration = Duration::from_secs(30);

/// 把 CPU 让给真实线程一小会儿，让它们有机会推进。
///
/// 这是**让渡**，不是收敛判据：
/// - 脚本 runner 在每个关键事件上 `Condvar::notify_all`，所以这里会被显式事件提前唤醒；
/// - 真实 runner（relay / SQLite 驱动）没有测试可见的信号，只能让渡一个很短的墙钟片；
///   调用方（[`pump_until`] / [`wait_state`]）用**明确状态**决定是否收敛。
fn let_worker_run(shared: Option<&ScriptShared>) {
    match shared {
        Some(shared) => {
            let (lock, condvar) = &**shared;
            let script = lock.lock().expect("脚本锁");
            let _ = condvar.wait_timeout(script, Duration::from_millis(2));
        }
        None => std::thread::sleep(Duration::from_millis(1)),
    }
}

/// 测试进程级的 `XDG_DATA_HOME`（Linux 用户主键；Windows 是 no-op）。
fn ensure_test_user_key() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("dev-sync-autosync-xdg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("创建测试 XDG_DATA_HOME 失败");
        std::env::set_var("XDG_DATA_HOME", &dir);
        menghunote_sync::identity_wrap::initialize_user_key()
            .expect("初始化测试用户主键失败（ADR 0008）");
    });
}

/// 在 `CARGO_TARGET_TMPDIR`（`target/tmp`）下开一个独立临时目录。
fn temp_root(prefix: &str) -> TempDir {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("dev-sync-autosync-{prefix}-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("创建 target/tmp 下的测试根目录失败");
    TempDirBuilder::new()
        .prefix("case-")
        .tempdir_in(&root)
        .expect("在 target/tmp 下创建临时目录失败")
}

// ---------------------------------------------------------------------------
// 注入的手动单调时钟（scheduler 与 service deadline 共用同一份）
// ---------------------------------------------------------------------------

/// 手动 `Instant` 时钟：`now = base + offset`，测试显式 `advance`。
#[derive(Clone)]
struct ManualInstantClock {
    base: Instant,
    offset: Arc<Mutex<Duration>>,
}

impl ManualInstantClock {
    fn new() -> Self {
        Self {
            base: Instant::now(),
            offset: Arc::new(Mutex::new(Duration::ZERO)),
        }
    }

    fn advance(&self, by: Duration) {
        let mut offset = self.offset.lock().expect("手动时钟锁");
        *offset += by;
    }
}

impl OperationClock for ManualInstantClock {
    fn now(&self) -> Instant {
        self.base + *self.offset.lock().expect("手动时钟锁")
    }
}

/// 测试用 `AutoSync`：手动时钟 + 关闭墙钟 ticker（显式 tick）。
fn manual_auto(clock: &ManualInstantClock) -> AutoSync {
    AutoSync::new(
        AutoSyncConfig::new().no_ticker(),
        Arc::new(clock.clone()),
        clock.now(),
    )
}

// ---------------------------------------------------------------------------
// 窗口/AppShell 助手
// ---------------------------------------------------------------------------

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

/// 在某个窗口里建一个 `AppShell`（注入服务 + 自动同步接线），并把窗口句柄带回来。
fn add_shell_window_auto(
    cx: &mut TestAppContext,
    vault: Vault,
    label: &'static str,
    sync: SyncContext,
    service: Arc<SyncService>,
    auto: AutoSync,
) -> (Entity<ShellHost>, gpui::AnyWindowHandle) {
    let slot = std::rc::Rc::new(std::cell::Cell::new(None));
    let captured = slot.clone();
    let (host, window_cx) = cx.add_window_view(move |window, cx| {
        captured.set(Some(window.window_handle()));
        let shell = cx.new(|cx| {
            AppShell::new_with_dev_sync_service_auto(
                vault,
                label.to_owned(),
                sync,
                service,
                Some(auto),
                cx,
            )
        });
        shell.update(cx, |shell, cx| shell.open_settings_headless(cx));
        ShellHost { shell }
    });
    let handle = slot
        .get()
        .or_else(|| window_cx.windows().first().copied())
        .expect("窗口句柄必须可取");
    (host, handle)
}

/// 生产构造路线（真实 `DevLocalRunner`）+ 注入的确定性自动同步（手动时钟/无 ticker）。
fn add_real_shell_window(
    cx: &mut TestAppContext,
    vault: Vault,
    label: &'static str,
    config: DevSyncConfig,
    sync: SyncContext,
    clock: &ManualInstantClock,
) -> (Entity<ShellHost>, gpui::AnyWindowHandle) {
    let service = SyncService::new_dev_local(
        ServiceConfig::new(config.service.relay.clone()),
        config.vault.clone(),
    );
    add_shell_window_auto(cx, vault, label, sync, service, manual_auto(clock))
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

fn with_view<R>(
    cx: &mut VisualTestContext,
    view: &Entity<EditorView>,
    f: impl FnOnce(&mut EditorView, &mut Context<EditorView>) -> R,
) -> R {
    cx.update(|_window, app| view.update(app, |view, cx| f(view, cx)))
}

fn current_text(cx: &mut VisualTestContext, host: &Entity<ShellHost>) -> String {
    read_shell(cx, host, |shell, app| shell.dev_sync_current_text(app))
        .expect("当前活动文档的镜像正文")
}

fn active_view(cx: &mut VisualTestContext, host: &Entity<ShellHost>) -> Entity<EditorView> {
    read_shell(cx, host, |shell, _app| shell.dev_sync_active_view()).expect("活动文档视图")
}

fn is_held(cx: &mut VisualTestContext, host: &Entity<ShellHost>) -> bool {
    read_shell(cx, host, |shell, app| {
        shell
            .dev_sync_active_view()
            .is_some_and(|view| view.read(app).dev_sync_is_held())
    })
}

fn is_composing(cx: &mut VisualTestContext, host: &Entity<ShellHost>) -> bool {
    read_shell(cx, host, |shell, app| {
        shell
            .dev_sync_active_view()
            .is_some_and(|view| view.read(app).dev_sync_is_composing())
    })
}

fn is_saved(cx: &mut VisualTestContext, host: &Entity<ShellHost>) -> bool {
    read_shell(cx, host, |shell, app| {
        shell
            .dev_sync_active_view()
            .is_some_and(|view| view.read(app).dev_sync_is_saved())
    })
}

fn local_edit_seq(cx: &mut VisualTestContext, host: &Entity<ShellHost>) -> u64 {
    read_shell(cx, host, |shell, app| {
        shell
            .dev_sync_active_view()
            .map(|view| view.read(app).dev_sync_local_edit_seq())
            .unwrap_or(0)
    })
}

fn last_sync(cx: &mut VisualTestContext, host: &Entity<ShellHost>) -> Option<SyncOnceReport> {
    read_shell(cx, host, |shell, _app| shell.dev_sync_last_sync().cloned())
}

fn status(
    cx: &mut VisualTestContext,
    host: &Entity<ShellHost>,
) -> menghunote_app::dev_sync::SyncStatus {
    read_shell(cx, host, |shell, _app| shell.dev_sync_status()).expect("自动同步状态投影")
}

fn status_line(cx: &mut VisualTestContext, host: &Entity<ShellHost>) -> String {
    read_shell(cx, host, |shell, _app| shell.dev_sync_status_line()).expect("状态行")
}

fn status_error_line(cx: &mut VisualTestContext, host: &Entity<ShellHost>) -> Option<String> {
    read_shell(cx, host, |shell, _app| shell.dev_sync_status_error_line())
}

fn in_progress(cx: &mut VisualTestContext, host: &Entity<ShellHost>) -> bool {
    read_shell(cx, host, |shell, _app| shell.dev_sync_sync_in_progress())
}

fn scheduler_state(cx: &mut VisualTestContext, host: &Entity<ShellHost>) -> Option<SchedulerState> {
    read_shell(cx, host, |shell, _app| {
        shell.dev_sync_auto().map(|auto| auto.scheduler().state())
    })
}

fn two_phase_phase(cx: &mut VisualTestContext, host: &Entity<ShellHost>) -> Option<SyncPhase> {
    read_shell(cx, host, |shell, app| {
        shell
            .dev_sync_panel()
            .and_then(|panel| panel.read(app).service().snapshot().two_phase)
            .map(|status| status.phase)
    })
}

/// 一拍：tick（自动同步决策/派发）→ pump（推进两阶段）→ 跑掉异步任务。
fn tick(cx: &mut VisualTestContext, host: &Entity<ShellHost>) {
    with_shell(cx, host, |shell, cx| {
        shell.dev_sync_auto_tick(cx);
        shell.dev_sync_pump(cx);
    });
    cx.run_until_parked();
}

/// 反复推进 GPUI，直到**明确状态** `pred` 成立。
///
/// - 每拍之间调用 [`let_worker_run`]：脚本 runner 的显式事件会提前唤醒；真实线程只是被
///   让渡 CPU；
/// - 收敛判据只有 `pred`；[`CONVERGE_DEADLINE`] 是死锁护栏，不是"等够了"；
/// - 不用固定次数 `yield_now` 猜，也不 sleep 当判据（AGENTS §4）。
fn pump_until(
    cx: &mut VisualTestContext,
    host: &Entity<ShellHost>,
    shared: Option<&ScriptShared>,
    what: &str,
    mut pred: impl FnMut(&mut VisualTestContext, &Entity<ShellHost>) -> bool,
) {
    let deadline = Instant::now() + CONVERGE_DEADLINE;
    loop {
        tick(cx, host);
        if pred(cx, host) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{what}：{CONVERGE_DEADLINE:?} 内没有收敛（状态：{:?}，行：{:?}）",
            status(cx, host),
            status_line(cx, host)
        );
        let_worker_run(shared);
    }
}

/// 同 [`pump_until`]，但每拍先推进注入时钟 `step`（周期/退避/续批的"到期"由测试显式驱动）。
fn pump_until_clock(
    cx: &mut VisualTestContext,
    host: &Entity<ShellHost>,
    clock: &ManualInstantClock,
    shared: Option<&ScriptShared>,
    step: Duration,
    what: &str,
    mut pred: impl FnMut(&mut VisualTestContext, &Entity<ShellHost>) -> bool,
) {
    let deadline = Instant::now() + CONVERGE_DEADLINE;
    loop {
        clock.advance(step);
        tick(cx, host);
        if pred(cx, host) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{what}：{CONVERGE_DEADLINE:?} 内没有收敛（状态：{:?}，行：{:?}）",
            status(cx, host),
            status_line(cx, host)
        );
        let_worker_run(shared);
    }
}

/// 只在**真实**驱动/数据库线程上才需要的有界等待（脚本 runner 一律用显式事件）。
///
/// 真实 driver/relay 没有测试可见的信号（本任务不改生产），这里每次只把 CPU 让渡 1 ms；
/// 收敛判据仍是 `f` 的明确状态，墙钟上限只是死锁护栏。
fn wait_state(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + CONVERGE_DEADLINE;
    loop {
        if f() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{what}：{CONVERGE_DEADLINE:?} 内没有收敛"
        );
        let_worker_run(None);
    }
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

/// 把当前文档的本地编辑显式 flush 到 durable（headless 里没有墙钟 flush ticker）。
///
/// 做法与生产同一条路径：登记一次外部提交闸门 → 驱到 `Held`（这一步会 flush 到明确目标
/// 并等 durable 水位追上）→ 立刻 `cancel` 释放。它只推进 durable 水位，不改正文、不改 head。
///
/// 收敛判据是闸门状态 `Held`；flush 由真实的存储驱动线程完成，因此用 [`wait_state`]。
fn flush_to_durable(cx: &mut VisualTestContext, host: &Entity<ShellHost>) {
    let ticket = with_shell(cx, host, |shell, cx| shell.dev_sync_begin_external(cx))
        .expect("登记外部提交闸门失败");
    wait_state("把本地编辑 flush 到 durable", || {
        match with_shell(cx, host, |shell, cx| {
            shell.dev_sync_poll_external(&ticket, cx)
        }) {
            menghunote_storage::session::GateState::Held(_) => true,
            menghunote_storage::session::GateState::Flushing
            | menghunote_storage::session::GateState::WaitingIME => false,
            other => panic!("闸门未收敛到 Held：{other:?}"),
        }
    });
    with_shell(cx, host, |shell, cx| {
        shell.dev_sync_cancel_external(&ticket, cx)
    })
    .expect("释放闸门失败");
    tick(cx, host);
}

/// 泵到"本地编辑已 durable（DurableAck）且 debounce 已登记"。
fn settle_local_edit(cx: &mut VisualTestContext, host: &Entity<ShellHost>) {
    flush_to_durable(cx, host);
    wait_state("本地编辑变 durable 并登记 debounce", || {
        tick(cx, host);
        let pending = read_shell(cx, host, |shell, _app| {
            shell.dev_sync_auto().map(|auto| auto.local_edit_pending())
        });
        let scheduled_local_edit = matches!(
            scheduler_state(cx, host),
            Some(SchedulerState::Scheduled {
                trigger: Trigger::LocalEdit,
                ..
            })
        );
        is_saved(cx, host) && pending == Some(false) && scheduled_local_edit
    });
}

/// 真实点击侧栏第 `index` 行（先序下标；切换文档走生产路径）。
fn click_tree_row(cx: &mut VisualTestContext, index: usize) {
    let selector = format!("tree-row-{index}");
    // `debug_bounds` 只收 `&'static str`：测试里泄漏一份 selector 字符串可接受。
    let selector: &'static str = Box::leak(selector.into_boxed_str());
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("{selector} 不存在"));
    cx.simulate_click(
        gpui::point(
            bounds.origin.x + gpui::px(24.0),
            bounds.origin.y + gpui::px(8.0),
        ),
        Modifiers::default(),
    );
    cx.run_until_parked();
}

// ---------------------------------------------------------------------------
// 真实 A/B 一对实例（Create → 播种 → 发布基线 → Join）
// ---------------------------------------------------------------------------

struct Pair {
    relay: DevRelay,
    _dir: TempDir,
    a_path: PathBuf,
    b_path: PathBuf,
    a_vault: Vault,
    b_vault: Vault,
    a_identity: LocalIdentityV1,
    b_identity: LocalIdentityV1,
    a_sync: SyncContext,
    b_sync: SyncContext,
    doc_id: String,
    seed_text: String,
}

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
            recovery_code: Some(recovery),
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
        b_identity: b.identity,
        a_sync: SyncContext::new(a_peer, a_epoch),
        b_sync: SyncContext::new(b_peer, b_epoch),
        doc_id,
        seed_text,
    }
}

/// 走真实 storage 路径往正文末尾追加一段（同一 A 事务 + sync intent）。
fn append_body(vault: &Vault, doc_id: &str, peer: u64, epoch: u64, text: &str) {
    let (doc, head) = load_body_doc(vault, doc_id, peer, None).expect("加载正文失败");
    let pos = doc.scalar_len();
    doc.insert_scalars(pos, text).expect("追加正文失败");
    let base = head.version_vector().expect("解码 head VV 失败");
    let update = doc.pending_update(&base).expect("导出正文增量失败");
    let seq = head.head_seq.saturating_add(1);
    let intent = local_sync_intent(
        doc_id,
        seq,
        "dev-sync-autosync",
        SyncContext::new(peer, epoch),
    )
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
// 脚本 runner：确定性驱动两阶段握手（无网络；可卡住/放行每个阶段）
// ---------------------------------------------------------------------------

/// 脚本 runner ↔ 测试的**显式事件通道**（`Condvar`）：runner 在每个关键点记账并唤醒测试。
type ScriptShared = Arc<(Mutex<Script>, Condvar)>;

#[derive(Debug, Clone)]
struct Script {
    /// Prepare 阶段卡住，直到测试 `release_prepare`（验证"网络阶段可编辑/可切换"）。
    prepare_blocked: bool,
    prepare_released: bool,
    /// 是否进入两阶段握手（发 NeedGate 并等 GrantGate/Finish）。
    need_gate: bool,
    /// 本次 run 的失败文案（`None` = 成功）。
    fail: Option<String>,
    /// `Integrated` 上报的计数。
    integrated: usize,
    deferred: usize,
    /// 观测（测试的收敛判据，全部由 runner 显式记账）：
    /// 进入次数 / 已发出的 NeedGate / 已返回次数。
    entered: usize,
    need_gate_sent: usize,
    returned: usize,
    /// 是否收到授权 / 是否收到 Finish / 是否在 NeedGate 被取消。
    saw_grant: bool,
    saw_finish: bool,
    saw_cancel: bool,
    cancelled_at_need_gate: bool,
}

impl Default for Script {
    fn default() -> Self {
        Self {
            prepare_blocked: false,
            prepare_released: false,
            need_gate: true,
            fail: None,
            integrated: 1,
            deferred: 0,
            entered: 0,
            need_gate_sent: 0,
            returned: 0,
            saw_grant: false,
            saw_finish: false,
            saw_cancel: false,
            cancelled_at_need_gate: false,
        }
    }
}

/// runner 侧记账 + 唤醒测试（**在锁内**调用；每个关键事件都要 notify）。
fn notify_script(shared: &ScriptShared, change: impl FnOnce(&mut Script)) {
    let (lock, condvar) = &**shared;
    let mut script = lock.lock().expect("脚本锁");
    change(&mut script);
    condvar.notify_all();
}

struct ScriptedRunner {
    control: Option<Receiver<SyncControl>>,
    shared: ScriptShared,
}

impl ScriptedRunner {
    fn new(script: Script) -> (Self, ScriptShared) {
        let shared: ScriptShared = Arc::new((Mutex::new(script), Condvar::new()));
        (
            Self {
                control: None,
                shared: shared.clone(),
            },
            shared,
        )
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Script> {
        self.shared.0.lock().expect("脚本锁")
    }

    /// 等 Prepare 放行（`prepare_blocked` 关闭时立即返回）。
    fn wait_prepare(&self) {
        let (lock, condvar) = &*self.shared;
        let mut script = lock.lock().expect("脚本锁");
        while script.prepare_blocked && !script.prepare_released {
            script = condvar.wait(script).expect("脚本等待 prepare");
        }
    }

    /// 等一条本次 op 的控制消息（忽略别的 op 的迟到消息）。
    fn wait_control(&mut self, op_id: OpId) -> Result<SyncControl, String> {
        loop {
            let Some(receiver) = self.control.as_ref() else {
                return Err("两阶段同步不可用：worker 没有接管控制通道".to_owned());
            };
            let message = receiver.recv().map_err(|_| "控制通道已关闭".to_owned())?;
            match &message {
                SyncControl::GrantGate { op_id: id, .. }
                | SyncControl::Finish { op_id: id }
                | SyncControl::Cancel { op_id: id }
                    if *id == op_id =>
                {
                    return Ok(message);
                }
                SyncControl::Stop => return Ok(message),
                _ => continue,
            }
        }
    }
}

impl SyncRunner for ScriptedRunner {
    fn run(&mut self, _op: Command, _events: &mut dyn FnMut(Event)) -> Result<OpOutcome, String> {
        Ok(OpOutcome::new("脚本 runner：设备/控制命令"))
    }

    fn attach_control(&mut self, control: Receiver<SyncControl>) {
        self.control = Some(control);
    }

    fn run_gated_controlled(
        &mut self,
        _op: Command,
        _gate: Option<&HeldGate>,
        control: &menghunote_app::dev_sync::service::OpControl,
        events: &mut dyn FnMut(Event),
    ) -> Result<OpOutcome, String> {
        // 无会话同步路径：同样支持卡 Prepare 与失败脚本。
        self.wait_prepare();
        if control.is_cancelled() {
            return Err("同步已取消：在外部集成前停止（未追加任何本地行）".to_owned());
        }
        events(Event::Progress {
            op_id: OpId::UNASSIGNED,
            phase: Phase::Preparing,
            detail: "脚本 runner：无会话 Prepare".to_owned(),
        });
        let fail = self.lock().fail.clone();
        match fail {
            Some(message) => Err(message),
            None => Ok(OpOutcome::new("脚本 runner：无会话完成")),
        }
    }

    fn run_two_phase(
        &mut self,
        op_id: OpId,
        control: &menghunote_app::dev_sync::service::OpControl,
        events: &mut dyn FnMut(Event),
    ) -> Result<OpOutcome, String> {
        notify_script(&self.shared, |script| script.entered += 1);
        let result = self.run_two_phase_steps(op_id, control, events);
        // 不论成功/失败/取消都记账：这是"本轮已离开 runner"的显式信号。
        notify_script(&self.shared, |script| script.returned += 1);
        result
    }
}

impl ScriptedRunner {
    fn run_two_phase_steps(
        &mut self,
        op_id: OpId,
        control: &menghunote_app::dev_sync::service::OpControl,
        events: &mut dyn FnMut(Event),
    ) -> Result<OpOutcome, String> {
        events(Event::Progress {
            op_id,
            phase: Phase::Preparing,
            detail: "脚本 runner：Prepare（网络阶段，不持闸）".to_owned(),
        });
        self.wait_prepare();
        if control.is_cancelled() {
            return Err("同步已取消：在外部集成前停止（未追加任何本地行）".to_owned());
        }
        let (need_gate, integrated, deferred, fail) = {
            let script = self.lock();
            (
                script.need_gate,
                script.integrated,
                script.deferred,
                script.fail.clone(),
            )
        };
        if !need_gate {
            return match fail {
                Some(message) => Err(message),
                None => Ok(OpOutcome::new("脚本 runner：完成（无握手）")),
            };
        }
        events(Event::NeedGate {
            op_id,
            authorization_epoch: 7,
            stored: integrated,
            deferred,
            summary: format!("脚本 Prepare：stored={integrated} deferred={deferred}"),
        });
        // 事件已经进了通道：这就是"测试可以安全泵一拍"的显式信号。
        notify_script(&self.shared, |script| script.need_gate_sent += 1);
        match self.wait_control(op_id)? {
            SyncControl::GrantGate { .. } => {
                notify_script(&self.shared, |script| script.saw_grant = true);
            }
            SyncControl::Cancel { .. } => {
                notify_script(&self.shared, |script| {
                    script.saw_cancel = true;
                    script.cancelled_at_need_gate = true;
                });
                return Err("同步已取消：在外部集成前停止（未追加任何本地行）".to_owned());
            }
            SyncControl::Stop => return Err("worker 已停止：等待持闸授权时收到 Stop".to_owned()),
            SyncControl::Finish { .. } => {
                return Err("控制消息顺序错误：尚未授权就收到 Finish".to_owned());
            }
        }
        events(Event::Integrated {
            op_id,
            changed_heads: Vec::new(),
            integrated,
            deferred,
            summary: format!("脚本 Integrate：integrated={integrated} deferred={deferred}"),
        });
        match self.wait_control(op_id)? {
            SyncControl::Finish { .. } => {
                notify_script(&self.shared, |script| script.saw_finish = true);
            }
            SyncControl::Cancel { .. } => {
                notify_script(&self.shared, |script| script.saw_cancel = true);
                return Err(
                    "集成已提交，但交付前收到取消：本次未 ACK（durable 变化保留）".to_owned(),
                );
            }
            SyncControl::Stop => {
                return Err("worker 已停止：等待交付完成回执时收到 Stop".to_owned())
            }
            SyncControl::GrantGate { .. } => {
                return Err("控制消息顺序错误：集成完成后又收到 GrantGate".to_owned());
            }
        }
        match fail {
            Some(message) => Err(message),
            None => Ok(OpOutcome::new("脚本 runner：两阶段完成")),
        }
    }
}

/// 等脚本 runner 的**显式事件**（`Condvar` 唤醒；墙钟上限只是死锁护栏）。
///
/// 只能用于"worker 不需要宿主推进"的事件（entered / NeedGate 已发出 / …）；
/// 需要宿主 pump 才能推进的事件一律用 [`pump_until`]。
fn wait_script(shared: &ScriptShared, what: &str, mut pred: impl FnMut(&Script) -> bool) -> Script {
    let (lock, condvar) = &**shared;
    let deadline = Instant::now() + CONVERGE_DEADLINE;
    let mut script = lock.lock().expect("脚本锁");
    loop {
        if pred(&script) {
            return script.clone();
        }
        let now = Instant::now();
        assert!(
            now < deadline,
            "{what}：脚本 runner 在 {CONVERGE_DEADLINE:?} 内没有到达该事件（脚本：{script:?}）"
        );
        let (next, timeout) = condvar
            .wait_timeout(script, deadline - now)
            .expect("脚本等待");
        script = next;
        assert!(
            !timeout.timed_out() || pred(&script),
            "{what}：脚本 runner 在 {CONVERGE_DEADLINE:?} 内没有到达该事件（脚本：{script:?}）"
        );
    }
}

/// 脚本化 `AppShell` 的测试夹具（dev vault 播种 + 手动时钟 + 无 ticker）。
struct ScriptedFixture {
    host: Entity<ShellHost>,
    cx: VisualTestContext,
    shared: ScriptShared,
    _dir: TempDir,
}

fn add_scripted_shell(
    cx: &mut TestAppContext,
    prefix: &str,
    script: Script,
    clock: &ManualInstantClock,
    seed_inbox: usize,
) -> ScriptedFixture {
    let dir = temp_root(prefix);
    let path = dir.path().join("vault.sqlite3");
    let dev = dev_vault::open_vault_at_with_peer(&path, 1).expect("打开 dev vault 失败");
    assert!(dev.seeded, "空库必须本次播种");
    // 造 durable「已接收·待应用」（真实 inbox 计数；脚本 runner 不会真的集成它们）。
    let log_id = [0x5a; 16];
    for seq in 1..=u64::try_from(seed_inbox).expect("seed_inbox 转 u64") {
        store_inbox_verified(
            &dev.vault,
            &log_id,
            seq,
            &[0x11; 32],
            1,
            b"scripted-payload",
        )
        .expect("写入脚本 inbox 行失败");
    }
    let sync = SyncContext::new(1, 1);
    let (runner, shared) = ScriptedRunner::new(script);
    let service =
        SyncService::with_runner(ServiceConfig::new("http://127.0.0.1:1"), Box::new(runner));
    let (host, handle) = add_shell_window_auto(
        cx,
        dev.vault.clone(),
        "dev-sync-autosync-script",
        sync,
        service,
        manual_auto(clock),
    );
    let app_cx = cx.clone();
    let vcx = VisualTestContext::from_window(handle, &app_cx);
    ScriptedFixture {
        host,
        cx: vcx,
        shared,
        _dir: dir,
    }
}

fn release_prepare(shared: &ScriptShared) {
    notify_script(shared, |script| {
        script.prepare_blocked = false;
        script.prepare_released = true;
    });
}

fn script_snapshot(shared: &ScriptShared) -> Script {
    shared.0.lock().expect("脚本锁").clone()
}

// ---------------------------------------------------------------------------
// 1) 启动有待发送 → 自动 Prepare/NeedGate/Held/Finish，B 收到（真实 dev relay）
// ---------------------------------------------------------------------------

#[gpui::test]
async fn startup_with_durable_outbox_syncs_and_peer_receives(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let pair = establish_pair("startup");
    let clock = ManualInstantClock::new();
    let relay_url = pair.relay.base_url().to_owned();

    // A 在开窗**之前**就有一段本地编辑（durable outbox 待发送）：重启恢复的等价场景。
    append_body(
        &pair.a_vault,
        &pair.doc_id,
        pair.a_sync.peer_id,
        pair.a_identity.current_epoch,
        "startup-pending",
    );

    let (host_a, a_handle) = add_real_shell_window(
        cx,
        pair.a_vault.clone(),
        "dev-sync-a",
        config_for(&relay_url, &pair.a_path),
        pair.a_sync,
        &clock,
    );
    let app_cx = cx.clone();
    let mut cx_a = VisualTestContext::from_window(a_handle, &app_cx);
    cx_a.run_until_parked();

    // 构造期只登记"启动合并"，不该已经派发（避免构造期重入状态机）。
    let (queued, state, started) = read_shell(&mut cx_a, &host_a, |shell, _app| {
        shell.dev_sync_auto().map_or((false, None, None), |auto| {
            (
                auto.startup_queued(),
                Some(auto.scheduler().state()),
                Some(auto.scheduler().is_running()),
            )
        })
    });
    assert!(state.is_some(), "自动同步必须已接线");
    assert!(queued, "启动时检测到 durable 待发送，必须排一次");
    assert_eq!(
        state,
        Some(SchedulerState::Idle),
        "首拍之前不该有 pending 决策"
    );
    assert_eq!(started, Some(false));

    // 收敛判据：**本轮真的收尾**（有报告且不在途）。真实 runner 没有测试可见的信号，
    // 因此靠明确状态判定；每拍之间只把 CPU 让给 worker/relay（不 sleep 当判据）。
    // 观测：Prepare 阶段（网络阶段）绝不能持闸，且必须经历 NeedGate → Held。
    let mut saw_prepare_unheld = false;
    let mut saw_need_gate = false;
    let mut saw_held = false;
    pump_until(
        &mut cx_a,
        &host_a,
        None,
        "启动自动同步收敛",
        |cx, host| {
            if two_phase_phase(cx, host) == Some(SyncPhase::Prepare) {
                assert!(
                    !is_held(cx, host),
                    "网络阶段（Prepare）绝不能持闸（ADR 0012）"
                );
                assert!(
                    cx.debug_bounds("editor-sync-held").is_none(),
                    "Prepare 期间不能出现 editor-sync-held"
                );
                saw_prepare_unheld = true;
            }
            if two_phase_phase(cx, host) == Some(SyncPhase::NeedGate) {
                saw_need_gate = true;
            }
            if is_held(cx, host) {
                saw_held = true;
            }
            !in_progress(cx, host) && last_sync(cx, host).is_some()
        },
    );
    cx_a.run_until_parked();
    eprintln!(
        "[autosync-test] saw_prepare_unheld={saw_prepare_unheld} saw_need_gate={saw_need_gate} saw_held={saw_held}"
    );

    let report = last_sync(&mut cx_a, &host_a).expect("A 必须有一轮自动同步报告");
    assert!(report.is_ok(), "启动自动同步必须成功：{report:?}");
    assert!(
        report.dispatched_under_gate,
        "两阶段必须在 Held 之后才 integrate：{report:?}"
    );
    assert!(
        saw_held && saw_need_gate,
        "本轮必须真的经历 NeedGate → Held（saw_need_gate={saw_need_gate} saw_held={saw_held}）"
    );
    assert!(!is_held(&mut cx_a, &host_a), "收尾后必须已解闸");
    let line = status_line(&mut cx_a, &host_a);
    assert!(line.starts_with("已同步"), "成功后状态行：{line}");
    assert_eq!(status_error_line(&mut cx_a, &host_a), None);
    assert!(
        !matches!(
            scheduler_state(&mut cx_a, &host_a),
            Some(SchedulerState::Running) | Some(SchedulerState::Backoff { .. })
        ),
        "一轮成功后不能在运行/退避里：{:?}",
        scheduler_state(&mut cx_a, &host_a)
    );

    // B：新 AppShell（重启语义）→ 推进到周期点，自动拉取并交付 A 的文字。
    let (host_b, b_handle) = add_real_shell_window(
        cx,
        pair.b_vault.clone(),
        "dev-sync-b",
        config_for(&relay_url, &pair.b_path),
        pair.b_sync,
        &clock,
    );
    let mut cx_b = VisualTestContext::from_window(b_handle, &app_cx);
    cx_b.run_until_parked();
    assert_eq!(
        current_text(&mut cx_b, &host_b),
        pair.seed_text,
        "B 起始正文来自基线"
    );

    clock.advance(Duration::from_secs(45));
    tick(&mut cx_b, &host_b);
    pump_until(
        &mut cx_b,
        &host_b,
        None,
        "B 的自动同步收敛",
        |cx, host| !in_progress(cx, host) && last_sync(cx, host).is_some(),
    );
    let report_b = last_sync(&mut cx_b, &host_b).expect("B 必须有一轮自动同步报告");
    assert!(report_b.is_ok(), "B 的自动同步必须成功：{report_b:?}");
    assert!(
        report_b.dispatched_under_gate,
        "B 的两阶段也必须在 Held 之后 integrate：{report_b:?}"
    );
    let text_b = current_text(&mut cx_b, &host_b);
    assert!(
        text_b.contains("startup-pending"),
        "B 不重开文档就必须看到 A 的文字：{:?}",
        &text_b[..text_b.len().min(48)]
    );
    assert!(!is_held(&mut cx_b, &host_b), "B 收尾后必须解闸");

    // 远端 adopt 的 ACK 不触发自动同步：本地编辑代数仍为 0，也没有 LocalEdit pending。
    assert_eq!(
        local_edit_seq(&mut cx_b, &host_b),
        0,
        "远端 adopt 不算本地编辑"
    );
    assert_eq!(
        read_shell(&mut cx_b, &host_b, |shell, _app| shell
            .dev_sync_auto()
            .map(|auto| auto.local_edit_pending())),
        Some(false)
    );
    assert!(
        !matches!(
            scheduler_state(&mut cx_b, &host_b),
            Some(SchedulerState::Running)
        ),
        "B 收尾后不能还在运行：{:?}",
        scheduler_state(&mut cx_b, &host_b)
    );
}

// ---------------------------------------------------------------------------
// 2) 编辑 2s 尾缘 debounce、合并、手动优先、自动间隔 ≥5s
// ---------------------------------------------------------------------------

#[gpui::test]
async fn local_edit_debounces_and_manual_priority_wins(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let pair = establish_pair("debounce");
    let clock = ManualInstantClock::new();
    let relay_url = pair.relay.base_url().to_owned();
    let (host, handle) = add_real_shell_window(
        cx,
        pair.a_vault.clone(),
        "dev-sync-a",
        config_for(&relay_url, &pair.a_path),
        pair.a_sync,
        &clock,
    );
    let app_cx = cx.clone();
    let mut c = VisualTestContext::from_window(handle, &app_cx);
    c.run_until_parked();

    focus_editor(&mut c);
    c.simulate_input("ab");
    c.run_until_parked();
    settle_local_edit(&mut c, &host);
    let edit_at = clock.now();
    let state = scheduler_state(&mut c, &host).expect("调度器");
    let SchedulerState::Scheduled { due, trigger } = state else {
        panic!("编辑 DurableAck 后应有一条 pending，实际 {state:?}");
    };
    assert_eq!(trigger, Trigger::LocalEdit);
    assert_eq!(
        due,
        edit_at + Duration::from_secs(2),
        "尾缘 debounce 必须是 2s"
    );

    // "必须不发生"：注入时钟 1.5s + **单次** tick，不能启动（不靠自旋碰运气）。
    clock.advance(Duration::from_millis(1500));
    tick(&mut c, &host);
    assert!(!in_progress(&mut c, &host), "2s 之前不能启动自动同步");
    assert_eq!(
        scheduler_state(&mut c, &host),
        Some(SchedulerState::Scheduled {
            due: edit_at + Duration::from_secs(2),
            trigger: Trigger::LocalEdit,
        }),
        "2s 之前必须还留着同一条 pending"
    );

    // 合并：连续编辑只保留一条 pending，到期时间按最后一次编辑重算。
    c.simulate_input("cd");
    c.run_until_parked();
    settle_local_edit(&mut c, &host);
    let second_edit_at = clock.now();
    let state = scheduler_state(&mut c, &host).expect("调度器");
    let SchedulerState::Scheduled { due, trigger } = state else {
        panic!("连续编辑后应仍只有一条 pending，实际 {state:?}");
    };
    assert_eq!(trigger, Trigger::LocalEdit);
    assert_eq!(due, second_edit_at + Duration::from_secs(2));

    // 手动优先：debounce 还没到期也能立刻启动一轮（派发是同步的，本拍不可能已收尾）。
    with_shell(&mut c, &host, |shell, cx| shell.dev_sync_sync_once(cx))
        .expect("手动同步必须被接受（手动优先）");
    assert!(in_progress(&mut c, &host), "手动同步必须立即在途");
    assert_eq!(
        read_shell(&mut c, &host, |shell, _app| shell.dev_sync_round_origin()),
        Some(OpOrigin::Manual)
    );
    pump_until(&mut c, &host, None, "手动同步收尾", |cx, host| {
        !in_progress(cx, host) && last_sync(cx, host).is_some()
    });
    assert!(last_sync(&mut c, &host).expect("手动报告").is_ok());
    assert!(!is_held(&mut c, &host));

    // 自动启动间隔 ≥5s：手动轮不算自动启动，编辑 debounce 到期后应能启动；
    // 启动后紧接着的第二轮必须等满 5s。
    c.simulate_input("ef");
    c.run_until_parked();
    settle_local_edit(&mut c, &host);
    clock.advance(Duration::from_secs(2));
    // 收敛判据：调度器**真的派发了这一轮**（`dev_sync_sync` 已登记 = 在途）。
    // 若这中间又有本地编辑把尾缘 debounce 重算（连续输入），就按新的到期时间再来一拍——
    // 注入时钟 + 明确状态，绝不用固定次数 yield 猜。
    let dispatch_deadline = Instant::now() + CONVERGE_DEADLINE;
    loop {
        tick(&mut c, &host);
        if in_progress(&mut c, &host) {
            break;
        }
        let state = scheduler_state(&mut c, &host);
        let Some(SchedulerState::Scheduled { due, trigger }) = state else {
            panic!("debounce 到期必须派发自动同步，实际 {state:?}");
        };
        assert_eq!(trigger, Trigger::LocalEdit, "被挡住时 pending 仍是本地编辑");
        // 尾缘被重算：把注入时钟推到新的到期时间（注入时钟，不是自旋）。
        // 若到期时间已经过去却仍没派发（被 5s 最小启动间隔挡住），按 1s 推进。
        let until_due = due.saturating_duration_since(clock.now());
        clock.advance(if until_due.is_zero() {
            Duration::from_secs(1)
        } else {
            until_due
        });
        assert!(
            Instant::now() < dispatch_deadline,
            "debounce 到期后 {CONVERGE_DEADLINE:?} 内没有派发自动同步"
        );
    }
    assert_eq!(
        read_shell(&mut c, &host, |shell, _app| shell.dev_sync_round_origin()),
        Some(OpOrigin::Auto)
    );
    pump_until(&mut c, &host, None, "自动轮收尾", |cx, host| {
        !in_progress(cx, host)
    });
    assert!(!is_held(&mut c, &host));
}

// ---------------------------------------------------------------------------
// 3) 失败退避 / 仅可重试才重试 / 鉴权类进入需处理
// ---------------------------------------------------------------------------

#[gpui::test]
async fn backoff_jitter_and_permanent_failures_pause_for_attention(cx: &mut TestAppContext) {
    let clock = ManualInstantClock::new();
    let script = Script {
        // 不握手：Prepare 直接失败（网络阶段）。
        need_gate: false,
        fail: Some("同步失败（Prepare）：relay 传输失败：connection refused".to_owned()),
        integrated: 0,
        deferred: 0,
        ..Script::default()
    };
    let fixture = add_scripted_shell(cx, "backoff", script, &clock, 0);
    let host = fixture.host.clone();
    let shared = fixture.shared.clone();
    let mut c = fixture.cx;
    c.run_until_parked();

    // 启动时没有 durable 待办 → 用周期（45s）触发第一轮。
    clock.advance(Duration::from_secs(45));
    tick(&mut c, &host);
    // 显式信号：worker 真的进了第 1 轮（这一轮不需要宿主推进，可直接阻塞等）。
    wait_script(&shared, "周期到期必须启动第 1 轮", |script| {
        script.entered >= 1
    });
    let failed_at = clock.now();
    pump_until(
        &mut c,
        &host,
        Some(&shared),
        "第 1 轮失败退避",
        |cx, host| {
            !in_progress(cx, host)
                && matches!(
                    scheduler_state(cx, host),
                    Some(SchedulerState::Backoff { .. })
                )
        },
    );
    let state = scheduler_state(&mut c, &host).expect("调度器");
    let SchedulerState::Backoff { until, attempt } = state else {
        panic!("可重试失败必须退避，实际 {state:?}");
    };
    assert_eq!(attempt, 1);
    assert_eq!(
        until,
        failed_at + Duration::from_secs(5),
        "第 1 次失败退避 5s"
    );
    let line = status_line(&mut c, &host);
    assert!(
        line.starts_with("重试于 "),
        "状态行必须显示重试时间：{line}"
    );
    assert_eq!(
        status_error_line(&mut c, &host),
        None,
        "普通退避不显示打扰行"
    );

    // 没到期不能启动：注入时钟 + **单次** tick（不是"自旋看它起不起来"）。
    clock.advance(Duration::from_millis(4500));
    tick(&mut c, &host);
    assert_eq!(
        scheduler_state(&mut c, &host),
        Some(SchedulerState::Backoff { until, attempt: 1 }),
        "退避没到期不能启动"
    );
    assert_eq!(
        script_snapshot(&shared).entered,
        1,
        "退避没到期不能让 worker 进入新一轮"
    );

    // 到期自动重试：显式信号 = worker 进入第 2 轮（不是"恰好在这一拍还在途"）。
    clock.advance(Duration::from_secs(1));
    tick(&mut c, &host);
    wait_script(&shared, "退避到期必须自动重试", |script| {
        script.entered >= 2
    });
    pump_until(
        &mut c,
        &host,
        Some(&shared),
        "第 2 轮失败退避",
        |cx, host| {
            !in_progress(cx, host)
                && matches!(
                    scheduler_state(cx, host),
                    Some(SchedulerState::Backoff { .. })
                )
        },
    );
    let state = scheduler_state(&mut c, &host).expect("调度器");
    let SchedulerState::Backoff { attempt, .. } = state else {
        panic!("第 2 次失败必须继续退避，实际 {state:?}");
    };
    assert_eq!(attempt, 2, "失败计数只增不减");

    // 鉴权类（非安全）：不自动重试，进入「需要处理」；手动重试**允许**。
    {
        let mut script = shared.0.lock().expect("脚本锁");
        script.fail = Some("同步失败（Prepare）：relay 拒绝：UNAUTHORIZED".to_owned());
        shared.1.notify_all();
    }
    clock.advance(Duration::from_secs(30));
    tick(&mut c, &host);
    wait_script(&shared, "退避到期后应再试一次", |script| {
        script.entered >= 3
    });
    pump_until(
        &mut c,
        &host,
        Some(&shared),
        "鉴权失败进入需要处理",
        |cx, host| {
            !in_progress(cx, host)
                && matches!(status(cx, host).state, StatusState::NeedsAttention { .. })
        },
    );
    let line = status_line(&mut c, &host);
    assert!(
        line.contains("需要处理："),
        "鉴权失败必须进入需处理：{line}"
    );
    assert!(line.contains("UNAUTHORIZED"), "{line}");
    let error_line = status_error_line(&mut c, &host).expect("需要处理必须给出一行可操作提示");
    assert!(error_line.contains("可点「同步」重试"), "{error_line}");
    assert_eq!(
        read_shell(&mut c, &host, |shell, _app| shell
            .dev_sync_auto()
            .map(|auto| auto.scheduler().is_paused())),
        Some(true)
    );

    // 不盲重试：推进到周期点 + **单次** tick 也不启动。
    clock.advance(Duration::from_secs(60));
    tick(&mut c, &host);
    assert!(!in_progress(&mut c, &host), "需处理状态不能自动重试");
    assert_eq!(
        script_snapshot(&shared).entered,
        3,
        "需处理状态不能再让 worker 进入新一轮"
    );

    // 用户显式重试（非安全错误）：清掉暂停、按手动语义启动；成功后恢复"已同步"。
    {
        let mut script = shared.0.lock().expect("脚本锁");
        script.fail = None;
        shared.1.notify_all();
    }
    with_shell(&mut c, &host, |shell, cx| shell.dev_sync_sync_once(cx))
        .expect("非安全「需要处理」必须允许手动重试");
    pump_until(
        &mut c,
        &host,
        Some(&shared),
        "手动重试收尾",
        |cx, host| {
            !in_progress(cx, host) && last_sync(cx, host).is_some_and(|report| report.is_ok())
        },
    );
    let line = status_line(&mut c, &host);
    assert!(line.starts_with("已同步"), "手动重试成功后状态行：{line}");
    assert_eq!(status_error_line(&mut c, &host), None);

    // 安全类（TLS）：进入 `SecurityPaused`，**不自动重试、也不允许手动重试**；
    // 状态行/错误行指向「设置 → 安全指纹」，且不引导盲点「同步」。
    {
        let mut script = shared.0.lock().expect("脚本锁");
        script.fail = Some("同步失败（Prepare）：TLS 校验失败：certificate expired".to_owned());
        shared.1.notify_all();
    }
    clock.advance(Duration::from_secs(45));
    tick(&mut c, &host);
    wait_script(&shared, "周期到期应再试一次", |script| {
        script.entered >= 5
    });
    pump_until(
        &mut c,
        &host,
        Some(&shared),
        "TLS 失败进入安全暂停",
        |cx, host| {
            !in_progress(cx, host)
                && matches!(status(cx, host).state, StatusState::SecurityPaused { .. })
        },
    );
    let line = status_line(&mut c, &host);
    assert_eq!(line, "需要处理：安全暂停（TLS 校验失败）", "{line}");
    let error_line = status_error_line(&mut c, &host).expect("安全暂停必须给出一行可操作提示");
    assert!(error_line.contains("安全指纹"), "{error_line}");
    assert!(error_line.contains("不会自动清 pin"), "{error_line}");
    assert!(
        !error_line.contains("可点「同步」重试"),
        "安全暂停不得引导盲点重试：{error_line}"
    );
    assert_eq!(
        read_shell(&mut c, &host, |shell, _app| shell
            .dev_sync_auto()
            .map(|auto| auto.scheduler().is_paused())),
        Some(true)
    );
    assert_eq!(
        read_shell(&mut c, &host, |shell, _app| shell
            .dev_sync_auto()
            .and_then(|auto| auto.security_category())
            .map(|category| category.code())),
        Some("tls"),
        "设置页/状态必须能看到脱敏类别"
    );

    // 不自动重试：推进周期 + 单次 tick 都不启动。
    clock.advance(Duration::from_secs(120));
    tick(&mut c, &host);
    assert!(!in_progress(&mut c, &host), "安全暂停不能自动重试");
    assert_eq!(
        script_snapshot(&shared).entered,
        5,
        "安全暂停不能再让 worker 进入新一轮"
    );

    // 手动同步也被明确拒绝（不自动清 pin/不盲信），并指向设置/指纹。
    let rejected = with_shell(&mut c, &host, |shell, cx| shell.dev_sync_sync_once(cx))
        .expect_err("安全暂停必须拒绝手动同步");
    assert!(rejected.contains("安全暂停"), "{rejected}");
    assert!(rejected.contains("安全指纹"), "{rejected}");
    assert_eq!(
        script_snapshot(&shared).entered,
        5,
        "被拒绝的手动同步不得派发 worker"
    );
    assert!(matches!(
        status(&mut c, &host).state,
        StatusState::SecurityPaused { .. }
    ));
}

// ---------------------------------------------------------------------------
// 4) Prepare 期间可输入/切换；NeedGate 遇到 IME 组合 → defer，结束后自动继续
// ---------------------------------------------------------------------------

#[gpui::test]
async fn prepare_stays_editable_and_ime_defers_the_gate(cx: &mut TestAppContext) {
    let clock = ManualInstantClock::new();
    let script = Script {
        prepare_blocked: true,
        need_gate: true,
        integrated: 1,
        deferred: 0,
        ..Script::default()
    };
    let fixture = add_scripted_shell(cx, "prepare", script, &clock, 0);
    let host = fixture.host.clone();
    let shared = fixture.shared.clone();
    let mut c = fixture.cx;
    c.run_until_parked();

    // 触发一轮自动同步（周期）：显式信号 = worker 真的进了 Prepare（它随后卡在放行上）。
    clock.advance(Duration::from_secs(45));
    tick(&mut c, &host);
    wait_script(&shared, "周期到期必须派发自动轮", |script| {
        script.entered >= 1
    });
    assert!(in_progress(&mut c, &host), "自动轮必须已派发");
    assert_eq!(
        read_shell(&mut c, &host, |shell, _app| shell.dev_sync_round_origin()),
        Some(OpOrigin::Auto)
    );

    // Prepare（网络阶段）：不持闸、可输入、可切换文档。
    assert!(!is_held(&mut c, &host), "Prepare 阶段不能持闸");
    assert!(
        c.debug_bounds("editor-sync-held").is_none(),
        "Prepare 阶段不能有 editor-sync-held"
    );
    focus_editor(&mut c);
    c.simulate_input("typing-while-preparing");
    c.run_until_parked();
    assert!(
        current_text(&mut c, &host).contains("typing-while-preparing"),
        "网络阶段必须能继续输入"
    );
    let before_view = active_view(&mut c, &host);
    click_tree_row(&mut c, 2);
    let after_view = active_view(&mut c, &host);
    assert_ne!(
        before_view, after_view,
        "Prepare 阶段必须允许切换文档（网络阶段不冻结切换）"
    );

    // 放行 Prepare → 显式信号：worker 已把 NeedGate 发进事件通道（随后停在等授权）。
    release_prepare(&shared);
    wait_script(&shared, "放行后必须发出 NeedGate", |script| {
        script.need_gate_sent >= 1
    });
    pump_until(
        &mut c,
        &host,
        Some(&shared),
        "两阶段停在 NeedGate 等授权",
        |cx, host| two_phase_phase(cx, host) == Some(SyncPhase::NeedGate),
    );
    // 活动视图进入 IME 组合：自动同步不得强行结束组合。
    let view = active_view(&mut c, &host);
    with_view(&mut c, &view, |view, cx| {
        view.dev_sync_debug_begin_composition(cx);
    });
    c.run_until_parked();
    assert!(is_composing(&mut c, &host), "测试必须真的进入 IME 组合态");
    assert!(!is_held(&mut c, &host), "组合中不能持闸");

    // 推 2s：拿不到空闲 → defer。判据是 worker 的**显式信号**（在 NeedGate 收到取消），
    // 不是"转够圈数"；注入时钟每拍推进 500ms 触达 2s 的 defer 上限。
    pump_until_clock(
        &mut c,
        &host,
        &clock,
        Some(&shared),
        Duration::from_millis(500),
        "约 2s 拿不到空闲必须 defer",
        |_cx, _host| script_snapshot(&shared).cancelled_at_need_gate,
    );
    pump_until(
        &mut c,
        &host,
        Some(&shared),
        "defer 收尾（释放操作槽）",
        |cx, host| !in_progress(cx, host) && !is_held(cx, host),
    );
    assert!(!is_held(&mut c, &host), "defer 不能留下持闸");
    assert!(
        read_shell(&mut c, &host, |shell, _app| shell
            .dev_sync_auto()
            .map(|auto| auto.scheduler().attempt()))
        .unwrap_or(u32::MAX)
            == 0,
        "defer 不能累计失败次数"
    );
    assert!(
        script_snapshot(&shared).cancelled_at_need_gate,
        "defer 必须发生在授权之前（不能在 Held 里取消）"
    );

    // 组合自然结束 → 自动继续（不用用户点击）。
    let view = active_view(&mut c, &host);
    with_view(&mut c, &view, |view, cx| {
        view.dev_sync_debug_commit_composition("测试", cx);
    });
    c.run_until_parked();
    assert!(!is_composing(&mut c, &host), "组合必须已经自然结束");

    // 收敛判据：**某一轮真的走完握手**（saw_grant && saw_finish）并收尾——这比"又进入了
    // 一轮"更严格：defer 之后如果闸门还没 Held，下一轮可能再次 defer，测试必须继续推进
    // 直到自动同步真的拿到授权（期间没有任何用户点击）。
    pump_until_clock(
        &mut c,
        &host,
        &clock,
        Some(&shared),
        Duration::from_millis(250),
        "defer 之后必须自动再跑一轮并完成握手",
        |cx, host| {
            let script = script_snapshot(&shared);
            script.entered >= 2 && script.saw_grant && script.saw_finish && !in_progress(cx, host)
        },
    );
    let script = script_snapshot(&shared);
    assert!(
        script.entered >= 2,
        "defer 之后必须自动再跑一轮（entered={}）",
        script.entered
    );
    assert!(
        script.saw_grant && script.saw_finish,
        "第二轮必须走完握手：{script:?}"
    );
    assert_eq!(status_error_line(&mut c, &host), None);
    assert!(status_line(&mut c, &host).starts_with("已同步"));
}

// ---------------------------------------------------------------------------
// 5) 跨操作续批：有进展继续、无进展暂停并如实显示剩余
// ---------------------------------------------------------------------------

#[gpui::test]
async fn continuation_runs_while_progressing_and_stops_without_progress(cx: &mut TestAppContext) {
    let clock = ManualInstantClock::new();
    // 第 1 轮：integrated=1（有进展）；第 2 轮改成 integrated=0（零进展）。
    let script = Script {
        need_gate: true,
        integrated: 1,
        deferred: 1,
        ..Script::default()
    };
    // 启动前就有 2 条 durable「已接收·待应用」→ 启动合并 → 第 1 轮。
    let fixture = add_scripted_shell(cx, "continuation", script, &clock, 2);
    let host = fixture.host.clone();
    let shared = fixture.shared.clone();
    let mut c = fixture.cx;
    c.run_until_parked();

    // 启动合并（构造期已检测到 durable 待应用）→ 第一拍就派发第 1 轮。
    clock.advance(Duration::from_secs(1));
    tick(&mut c, &host);
    wait_script(&shared, "启动合并必须派发第 1 轮", |script| {
        script.entered >= 1
    });
    // 注入时钟**不动**：收尾时刻就是当前时刻，因此"续批 due = 收尾时刻 + 1s"可精确断言。
    pump_until(
        &mut c,
        &host,
        Some(&shared),
        "第 1 轮收尾",
        |cx, host| !in_progress(cx, host) && last_sync(cx, host).is_some(),
    );
    assert_eq!(script_snapshot(&shared).entered, 1, "第 1 轮必须先跑一次");
    // 有进展（integrated=1）→ 收尾后留下的必须是"续批"pending（不是周期）。
    assert_eq!(
        scheduler_state(&mut c, &host),
        Some(SchedulerState::Scheduled {
            due: clock.now() + Duration::from_secs(1),
            trigger: Trigger::Continuation,
        }),
        "有进展时收尾必须排一条续批 pending"
    );
    assert!(
        status_line(&mut c, &host).contains("已接收·待应用 2"),
        "状态行必须如实显示剩余：{}",
        status_line(&mut c, &host)
    );
    assert!(
        status(&mut c, &host).deferred >= 1,
        "worker 上报的 deferred 必须如实进状态：{:?}",
        status(&mut c, &host)
    );

    // 有进展（integrated=1）→ 跨操作续批（受 5s 最小启动间隔约束）。
    {
        let mut script = shared.0.lock().expect("脚本锁");
        script.integrated = 0;
        shared.1.notify_all();
    }
    // 显式信号：worker 真的进了第 2 轮（注入时钟逐拍推进到续批到期 + 最小启动间隔）。
    pump_until_clock(
        &mut c,
        &host,
        &clock,
        Some(&shared),
        Duration::from_secs(1),
        "有进展时必须跨操作续批",
        |_cx, _host| script_snapshot(&shared).entered >= 2,
    );
    pump_until(
        &mut c,
        &host,
        Some(&shared),
        "第 2 轮收尾",
        |cx, host| !in_progress(cx, host) && script_snapshot(&shared).returned >= 2,
    );

    // 第 2 轮零进展 → 暂停这一轮（不继续刷同一批），如实显示剩余。
    let status = status(&mut c, &host);
    assert!(
        matches!(status.state, StatusState::NoProgress { remaining } if remaining == 2),
        "零进展必须暂停续批并显示真实剩余：{status:?}"
    );
    assert!(status_line(&mut c, &host).contains("续批暂停：无进展"));
    // "必须不发生"：显式推 5 拍（注入时钟），entered 不能继续增长。
    let entered = script_snapshot(&shared).entered;
    for _ in 0..5 {
        clock.advance(Duration::from_secs(1));
        tick(&mut c, &host);
    }
    assert_eq!(
        script_snapshot(&shared).entered,
        entered,
        "零进展之后不得继续空转"
    );
}

// ---------------------------------------------------------------------------
// 6) 重启（新 AppShell）时检测到 durable 待应用 → 自动同步
// ---------------------------------------------------------------------------

#[gpui::test]
async fn restart_with_durable_inbox_resumes_automatically(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let pair = establish_pair("restart");
    let clock = ManualInstantClock::new();
    let relay_url = pair.relay.base_url().to_owned();

    // A 改一段并直接推给 relay（模拟"另一台设备早已同步过"）。
    append_body(
        &pair.a_vault,
        &pair.doc_id,
        pair.a_sync.peer_id,
        pair.a_identity.current_epoch,
        "restart-pending",
    );
    DevLocal::sync_once(&relay_url, &pair.a_vault, &pair.a_identity).expect("A 直接同步失败");

    // B 只跑 Prepare（pull 到 durable inbox，**不** integrate）：等价于"上次退出时还有待应用"。
    DevLocal::sync_prepare(&relay_url, &pair.b_vault, &pair.b_identity, None)
        .expect("B 直接 Prepare 失败");
    assert!(
        menghunote_storage::count_inbox_ready(&pair.b_vault).unwrap_or(0) > 0,
        "B 启动前必须有 durable 待应用"
    );

    // 新 AppShell（重启）：启动合并检测到待应用 → 第一拍自动同步。
    let (host, handle) = add_real_shell_window(
        cx,
        pair.b_vault.clone(),
        "dev-sync-b",
        config_for(&relay_url, &pair.b_path),
        pair.b_sync,
        &clock,
    );
    let app_cx = cx.clone();
    let mut c = VisualTestContext::from_window(handle, &app_cx);
    c.run_until_parked();
    assert_eq!(
        read_shell(&mut c, &host, |shell, _app| shell
            .dev_sync_auto()
            .map(|auto| auto.startup_queued())),
        Some(true),
        "重启后检测到 durable 待应用必须排一次"
    );
    clock.advance(Duration::from_millis(500));
    tick(&mut c, &host);
    pump_until(
        &mut c,
        &host,
        None,
        "重启后的自动同步收敛",
        |cx, host| !in_progress(cx, host) && last_sync(cx, host).is_some(),
    );
    let report = last_sync(&mut c, &host).expect("重启后必须有自动同步报告");
    assert!(report.is_ok(), "重启后的自动同步必须成功：{report:?}");
    assert!(
        report.changes >= 1,
        "必须交付至少 1 条已落盘变化：{report:?}"
    );
    assert!(
        current_text(&mut c, &host).contains("restart-pending"),
        "重启后必须自动把待应用内容交付进正文"
    );
    assert_eq!(
        menghunote_storage::count_inbox_ready(&pair.b_vault).unwrap_or(0),
        0
    );
}

// ---------------------------------------------------------------------------
// 7) 状态行/错误行文案（selector）+ 手动同步/发布回归
// ---------------------------------------------------------------------------

#[gpui::test]
async fn status_line_selector_and_manual_regressions(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let pair = establish_pair("status");
    let clock = ManualInstantClock::new();
    let relay_url = pair.relay.base_url().to_owned();
    let (host, handle) = add_real_shell_window(
        cx,
        pair.a_vault.clone(),
        "dev-sync-a",
        config_for(&relay_url, &pair.a_path),
        pair.a_sync,
        &clock,
    );
    let app_cx = cx.clone();
    let mut c = VisualTestContext::from_window(handle, &app_cx);
    c.run_until_parked();

    // 常驻状态行：selector 存在 + 初始文案。
    assert!(
        c.debug_bounds("sync-status").is_some(),
        "常驻状态行 selector `sync-status` 必须存在"
    );
    assert_eq!(status_line(&mut c, &host), "尚未同步");
    assert_eq!(status_error_line(&mut c, &host), None, "初始不该有打扰行");

    // 本地编辑（未同步）→ 状态行如实显示"待发送 N"。
    focus_editor(&mut c);
    c.simulate_input("status-line");
    c.run_until_parked();
    settle_local_edit(&mut c, &host);
    clock.advance(Duration::from_secs(1));
    tick(&mut c, &host);
    let line = status_line(&mut c, &host);
    assert!(
        line.contains("待发送"),
        "未同步的本地编辑必须显示待发送：{line}"
    );

    // 手动同步（回归）：仍然可用、仍然在 Held 下派发。
    with_shell(&mut c, &host, |shell, cx| shell.dev_sync_sync_once(cx)).expect("手动同步");
    pump_until(&mut c, &host, None, "手动同步收尾", |cx, host| {
        !in_progress(cx, host) && last_sync(cx, host).is_some()
    });
    let report = last_sync(&mut c, &host).expect("手动同步报告");
    assert!(report.is_ok(), "{report:?}");
    assert!(report.dispatched_under_gate, "{report:?}");
    let line = status_line(&mut c, &host);
    assert!(
        line.starts_with("已同步 ") && line.contains(':'),
        "成功一轮后状态行是 已同步 HH:MM：{line}"
    );
    assert!(
        !line.contains("待发送"),
        "同步完成后不该再显示待发送：{line}"
    );

    // 发布基线（回归）：手动路径不受自动同步影响。
    with_shell(&mut c, &host, |shell, cx| {
        shell.dev_sync_publish_baseline(cx)
    })
    .expect("发布基线");
    pump_until(&mut c, &host, None, "发布基线收尾", |cx, host| {
        !in_progress(cx, host)
            && read_shell(cx, host, |shell, _app| {
                shell.dev_sync_last_publish().cloned()
            })
            .is_some_and(|report| report.is_ok())
    });
    assert!(
        read_shell(&mut c, &host, |shell, _app| shell
            .dev_sync_last_publish()
            .cloned())
        .is_some_and(|report| report.is_ok()),
        "发布基线必须成功且有报告"
    );

    // 自动周期轮（回归）：推进 45s 后自动同步仍然能跑完。
    clock.advance(Duration::from_secs(45));
    tick(&mut c, &host);
    pump_until(&mut c, &host, None, "周期自动轮收敛", |cx, host| {
        !in_progress(cx, host)
    });
    assert!(status_line(&mut c, &host).starts_with("已同步"));
    assert!(!is_held(&mut c, &host));
}
