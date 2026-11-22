//! ADR 0009 Phase 1 / 切片 4c-2：**持闸同步**（UI 持闸 → worker 跑完整 sync → UI adopt/end）。
//!
//! 四个测试，都用**真实** `DevRelay`（进程内 axum + 临时 SQLite，放行验签的测试脚手架，
//! ⚠️ 非生产）+ 两个独立 vault：
//!
//! 1. `two_shells_exchange_text_without_reopening`：A/B 两个 `AppShell`（同一
//!    `TestAppContext` 的两个 window），A 键入 → A 持闸同步 → B 持闸同步 → B 的编辑器
//!    **不重开**就看到 A 的文字；反向再来一次；最后两边都还能本地键入并落库（无
//!    `HeadConflict`）。
//! 2. `failed_sync_releases_gate_and_can_retry`：relay 换成"只 listen 不应答"的黑洞 →
//!    worker 卡在传输上（此时闸门确实 Held、重复命令被拒）→ 关闭黑洞 → worker 明确失败 →
//!    闸门释放、正文不变、可再次重试。
//! 3. `panel_sync_button_runs_the_held_sync_path`：`dev-sync-sync` 按钮（headless 点击）
//!    走同一条路径：面板请求 → `AppShell` 持闸 → worker 同步 → adopt → 面板出现
//!    「同步完成：N 条变化」。
//! 4. `structure_change_reprojects_the_sidebar_after_sync`：远端把结构（树）改了 → 同步后
//!    按结构 head 变化**重投影**并刷新侧栏（新行真实出现）；活动文档正文/实体不变。
//!
//! ## 纪律（AGENTS.md §4/§6）
//!
//! - 临时目录一律 `CARGO_TARGET_TMPDIR`（`target/tmp`），不用 `%TEMP%`；
//! - **不 sleep**：收敛判据是 `dev_sync_sync_in_progress()` 转 false、worker 明确终态
//!   （`Completed`）、闸门 `Held`/释放这些**状态**；忙等只用 `yield_now` + 有界 deadline
//!   （与 `dev_sync_adoption.rs` 的 gate 驱动同一套写法）；
//! - Linux 用户主键的 `XDG_DATA_HOME` 也指到 `target/tmp`（Windows 是 no-op）；
//! - relay 只用于 dev：本文件**不能**作为鉴权/验签验收。
//!
//! feature 关闭时整个文件编译为空（0 个测试）。

#![cfg(feature = "dev-sync")]

use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use gpui::{
    div, App, AppContext as _, Context, Entity, InteractiveElement as _, Modifiers,
    ParentElement as _, Render, Styled as _, TestAppContext, VisualTestContext,
};
use menghunote_app::dev_sync::gate::DevSyncConfig;
use menghunote_app::dev_sync::service::DEFAULT_TRANSPORT_TIMEOUTS;
use menghunote_app::dev_sync::service::{
    DevLocalRunner, OpControl, OperationClock, ServiceConfig, SyncControl, SyncRunner,
};
use menghunote_app::dev_sync::setup::{self, SetupMode, SetupSecrets};
use menghunote_app::dev_sync::{Command, Event, HeldGate, OpId, OpOutcome, SyncService};
use menghunote_app::dev_vault;
use menghunote_app::editor::view::EditorView;
use menghunote_app::shell::AppShell;
use menghunote_dev_relay::DevRelay;
use menghunote_storage::{
    append_updates_with_sync_intents, list_tree, load_body_doc, load_head, local_sync_intent,
    sync_cursor_state, take_inbox_ready, BodyDoc, HeadState, StructureWriter, SyncContext,
    UpdateRecord, Vault,
};
use menghunote_sync::dev_local::DevLocal;
use menghunote_sync::identity::LocalIdentityV1;
use menghunote_sync::ids;
use tempfile::{Builder as TempDirBuilder, TempDir};

/// 同步收敛的安全上限（真实 relay 是 loopback，正常毫秒级；超时说明状态机卡死）。
const SYNC_DEADLINE: Duration = Duration::from_secs(30);

/// 测试进程级的 `XDG_DATA_HOME`（Linux 用户主键；Windows 是 no-op）。
fn ensure_test_user_key() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("dev-sync-sync-once-xdg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("创建测试 XDG_DATA_HOME 失败");
        std::env::set_var("XDG_DATA_HOME", &dir);
        menghunote_sync::identity_wrap::initialize_user_key()
            .expect("初始化测试用户主键失败（ADR 0008）");
    });
}

/// 在 `CARGO_TARGET_TMPDIR`（`target/tmp`）下开一个独立临时目录。
fn temp_root(prefix: &str) -> TempDir {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "dev-sync-sync-once-{prefix}-{}",
        std::process::id()
    ));
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
// A/B 一对真实实例（Create → 播种 → 发布基线 → Join）
// ---------------------------------------------------------------------------

/// `establish_pair` 的产物：两边各自的库/身份与共同的那篇笔记。
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
}

/// Create A → 播种演示树 → 发布初始基线 → B 只凭 `vault_id + 恢复码` Join。
///
/// 用 `dev_vault::open_vault_at_with_peer`（生产 dev vault 入口）给 A 播种：这样 A/B 都有
/// **同一棵**结构树与同一篇正文，`AppShell` 打开的先序第一篇也是同一个 body doc。
fn establish_pair(prefix: &str) -> Pair {
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let dir = temp_root(prefix);

    // A：真实 genesis（身份受保护落盘）。
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
    // `prepare` 的库句柄只用于启动；窗口/发布用生产入口重建的句柄。
    drop(a.vault);
    let dev_a = dev_vault::open_vault_at_with_peer(&a_path, a_peer).expect("A 打开实例库失败");
    assert!(dev_a.seeded, "A 的空库必须本次播种（演示树）");

    // 发布初始基线：B 的 `/bootstrap` 需要真实的两份 published checkpoint。
    DevLocal::publish_initial_pair(relay.base_url(), &dev_a.vault, &a.identity, &recovery)
        .expect("A 发布初始基线失败");

    // B：只凭 vault_id + 恢复码，不读 A 的目录/身份/root。
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

    // 两边打开同一篇：先序第一篇笔记（结构投影顺序相同）。
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
    }
}

/// 走真实 storage 路径往正文末尾追加一段（同一 A 事务 + sync intent）。
fn append_body(vault: &Vault, doc_id: &str, peer: u64, epoch: u64, text: &str) {
    let (doc, head) = load_body_doc(vault, doc_id, peer, None).expect("加载正文失败");
    let pos = doc.scalar_len();
    doc.insert_scalars(pos, text).expect("追加正文失败");
    commit_body(
        vault,
        doc_id,
        peer,
        epoch,
        &doc,
        &head,
        "dev-sync-sync-once",
    );
}

/// 把内存里的正文增量按 head CAS 落库，并同事务写一条出站意图（生产 flush 路径）。
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
// AppShell 测试助手（两个 window / 两个 VisualTestContext）
// ---------------------------------------------------------------------------

/// 在某个窗口里建一个 `AppShell`，并把窗口句柄带回来（两个窗口需要各自定位）。
fn add_shell_window(
    cx: &mut TestAppContext,
    vault: Vault,
    label: &'static str,
    dev_sync: DevSyncConfig,
    sync: SyncContext,
) -> (Entity<ShellHost>, gpui::AnyWindowHandle) {
    // `add_window_view` 的闭包拿得到 `&mut Window`；用它把句柄带出 borrow 期。
    let slot = std::rc::Rc::new(std::cell::Cell::new(None));
    let captured = slot.clone();
    let (host, window_cx) = cx.add_window_view(move |window, cx| {
        captured.set(Some(window.window_handle()));
        let shell =
            cx.new(|cx| AppShell::new_with_dev_sync(vault, label.to_owned(), dev_sync, sync, cx));
        shell.update(cx, |shell, cx| shell.open_settings_headless(cx));
        ShellHost { shell }
    });
    // 借用结束：句柄已带出；`window_cx` 只在本函数尾部被丢弃。
    let handle = slot
        .get()
        .or_else(|| window_cx.windows().first().copied())
        .expect("窗口句柄必须可取");
    (host, handle)
}

/// 在 `AppShell` 上执行一次需要 `&mut Context<AppShell>` 的操作。
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

/// 只读 `AppShell`。
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

fn active_held(cx: &mut VisualTestContext, host: &Entity<ShellHost>) -> bool {
    read_shell(cx, host, |shell, app| {
        shell
            .dev_sync_active_view()
            .is_some_and(|view| view.read(app).dev_sync_is_held())
    })
}

fn last_report(
    cx: &mut VisualTestContext,
    host: &Entity<ShellHost>,
) -> Option<menghunote_app::dev_sync::SyncOnceReport> {
    read_shell(cx, host, |shell, _app| shell.dev_sync_last_sync().cloned())
}

/// 只读服务快照（ADR 0012 S1 的 `deadline_exceeded`/`in_flight` 断言用）。
fn service_snapshot(
    cx: &mut VisualTestContext,
    host: &Entity<ShellHost>,
) -> Option<menghunote_app::dev_sync::StateSnapshot> {
    read_shell(cx, host, |shell, app| {
        shell
            .dev_sync_panel()
            .map(|panel| panel.read(app).service().snapshot())
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

/// 点正文首行聚焦（真实命中链路；不点空白区）。
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

/// 发一次持闸同步（**必须**被接受；被拒就 fail 并带上原因）。
fn start_sync(cx: &mut VisualTestContext, host: &Entity<ShellHost>) {
    with_shell(cx, host, |shell, cx| shell.dev_sync_sync_once(cx))
        .unwrap_or_else(|error| panic!("持闸同步被拒绝：{error}"));
}

/// 泵到"持闸同步结束"（**状态收敛**，不是 sleep）。
fn drive_sync(cx: &mut VisualTestContext, host: &Entity<ShellHost>) {
    let deadline = Instant::now() + SYNC_DEADLINE;
    loop {
        with_shell(cx, host, |shell, cx| shell.dev_sync_pump(cx));
        cx.run_until_parked();
        let finished = read_shell(cx, host, |shell, _app| !shell.dev_sync_sync_in_progress());
        if finished {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "持闸同步 30s 内没有收敛（面板：{:?}，服务：{:?}）",
            panel_sync_line(cx, host),
            service_snapshot(cx, host)
        );
        std::thread::yield_now();
    }
}

// ---------------------------------------------------------------------------
// 1) 两 AppShell 闭环：A 键入 → A 同步 → B 同步（不重开就看到 A 的文字）→ 反向
// ---------------------------------------------------------------------------

#[gpui::test]
async fn two_shells_exchange_text_without_reopening(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let pair = establish_pair("two-shells");
    let relay_url = pair.relay.base_url().to_owned();

    // 两个窗口（同一 TestAppContext）：A 与 B 各自持有自己的 Vault/身份/服务。
    let (host_a, a_handle) = add_shell_window(
        cx,
        pair.a_vault.clone(),
        "dev-sync-a",
        config_for(&relay_url, &pair.a_path),
        pair.a_sync,
    );
    let (host_b, b_handle) = add_shell_window(
        cx,
        pair.b_vault.clone(),
        "dev-sync-b",
        config_for(&relay_url, &pair.b_path),
        pair.b_sync,
    );
    let app_cx = cx.clone();
    let mut cx_a = VisualTestContext::from_window(a_handle, &app_cx);
    let mut cx_b = VisualTestContext::from_window(b_handle, &app_cx);
    cx_a.run_until_parked();
    cx_b.run_until_parked();

    // 前置：两边打开的是同一篇（先序第一篇），并且面板已挂载。
    for (side, cx_side, host) in [("A", &mut cx_a, &host_a), ("B", &mut cx_b, &host_b)] {
        assert!(
            cx_side.debug_bounds("dev-sync-panel").is_some(),
            "{side} 的 dev-sync 面板必须挂载"
        );
        let view = active_view(cx_side, host).unwrap_or_else(|| panic!("{side} 必须打开一篇笔记"));
        let doc = read_shell(cx_side, host, |_shell, app| {
            view.read(app).dev_sync_doc_id().to_owned()
        });
        assert_eq!(
            doc, pair.doc_id,
            "{side} 打开的先序第一篇必须是同一个 body doc"
        );
        assert!(
            cx_side.debug_bounds("editor-sync-held").is_none(),
            "{side} 同步前不该持有闸门"
        );
    }

    // --- 轮次 1：A 键入 → A 持闸同步 → B 持闸同步 → B 不重开就看到 A 的文字 ---
    // 两边初始正文相同（来自基线）。
    for (side, cx_side, host) in [("A", &mut cx_a, &host_a), ("B", &mut cx_b, &host_b)] {
        assert_eq!(
            current_text(cx_side, host),
            pair.seed_text,
            "{side} 的初始正文必须是基线里那份"
        );
    }
    let view_b_before = active_view(&mut cx_b, &host_b).expect("B 的活动视图");

    focus_editor(&mut cx_a);
    // 用 simulate_input 打**字面文本**（simulate_keystrokes 会把 "A1" 当成一个按键组合）。
    cx_a.simulate_input("from-a");
    cx_a.run_until_parked();
    let text_a_typed = current_text(&mut cx_a, &host_a);
    assert!(text_a_typed.contains("from-a"), "A 的键入必须立刻进镜像");

    start_sync(&mut cx_a, &host_a);
    drive_sync(&mut cx_a, &host_a);
    let report_a = last_report(&mut cx_a, &host_a).expect("A 必须有持闸同步报告");
    assert!(
        report_a.dispatched_under_gate,
        "A 的 SyncOnce 必须在 Held 之后派发（顺序证明）：{report_a:?}"
    );
    assert!(report_a.is_ok(), "A 的同步必须成功：{report_a:?}");
    assert_eq!(report_a.error, None, "A 的同步不得留下错误：{report_a:?}");
    assert!(
        !active_held(&mut cx_a, &host_a),
        "A 同步完成后闸门必须已释放"
    );
    assert!(
        cx_a.debug_bounds("editor-sync-held").is_none(),
        "A 同步完成后 editor-sync-held 必须消失"
    );

    start_sync(&mut cx_b, &host_b);
    drive_sync(&mut cx_b, &host_b);
    let report_b = last_report(&mut cx_b, &host_b).expect("B 必须有持闸同步报告");
    assert!(
        report_b.dispatched_under_gate,
        "B 的 SyncOnce 必须在 Held 之后派发（顺序证明）：{report_b:?}"
    );
    assert!(report_b.is_ok(), "B 的同步必须成功：{report_b:?}");
    assert!(
        report_b.changes >= 1,
        "B 本轮必须交付至少 1 条已落盘变化（A 的键入）：{report_b:?}"
    );
    assert!(
        !active_held(&mut cx_b, &host_b),
        "B 同步完成后闸门必须已释放"
    );
    let text_b_after = current_text(&mut cx_b, &host_b);
    assert!(
        text_b_after.contains("from-a"),
        "B 不重开文档就必须看到 A 的文字：{:?}",
        &text_b_after[..text_b_after.len().min(48)]
    );
    let view_b_after = active_view(&mut cx_b, &host_b).expect("B 的活动视图");
    assert!(
        view_b_before == view_b_after,
        "B 的 Activity View 不能重建/切换（同一个 Entity）"
    );

    // --- 轮次 2：B 键入 → B 同步 → A 同步 → A 看到 B 的文字 ---
    focus_editor(&mut cx_b);
    // adopt 之后 B 的 caret 可能停在 A 插入的字符之间；先按 End 移到**首行末尾**再输入，
    // 避免"两边在同一位置并发插入"造成 CRDT 交错（那不是本片要测的东西）。
    cx_b.simulate_keystrokes("end");
    cx_b.run_until_parked();
    cx_b.simulate_input("from-b");
    cx_b.run_until_parked();
    assert!(
        current_text(&mut cx_b, &host_b).contains("from-b"),
        "B 的键入必须立刻进镜像"
    );
    start_sync(&mut cx_b, &host_b);
    drive_sync(&mut cx_b, &host_b);
    let report_b2 = last_report(&mut cx_b, &host_b).expect("B 第二轮报告");
    assert!(report_b2.is_ok(), "B 的第二轮同步必须成功：{report_b2:?}");
    assert!(
        !active_held(&mut cx_b, &host_b),
        "B 第二轮同步完成后闸门必须已释放"
    );

    start_sync(&mut cx_a, &host_a);
    drive_sync(&mut cx_a, &host_a);
    let report_a2 = last_report(&mut cx_a, &host_a).expect("A 第二轮报告");
    assert!(report_a2.is_ok(), "A 的第二轮同步必须成功：{report_a2:?}");
    assert!(
        report_a2.changes >= 1,
        "A 本轮必须交付至少 1 条已落盘变化（B 的键入）：{report_a2:?}"
    );
    assert!(
        !active_held(&mut cx_a, &host_a),
        "A 第二轮完成后闸门必须已释放"
    );
    let text_a_after = current_text(&mut cx_a, &host_a);
    assert!(
        text_a_after.contains("from-a") && text_a_after.contains("from-b"),
        "A 必须同时看到自己的与 B 的文字：{:?}",
        &text_a_after[..text_a_after.len().min(48)]
    );

    // --- 收尾：本地再键入 + 生产关窗排空 → 新 update 落库（无 HeadConflict）---
    let head_before = load_head(&pair.a_vault, &pair.doc_id)
        .expect("读 A head")
        .head_seq;
    cx_a.simulate_input("z");
    cx_a.run_until_parked();
    with_shell(&mut cx_a, &host_a, |shell, cx| shell.close_all(cx));
    cx_a.run_until_parked();
    let head_after = load_head(&pair.a_vault, &pair.doc_id)
        .expect("关窗后读 A head")
        .head_seq;
    assert_eq!(
        head_after,
        head_before + 1,
        "本地键入必须落成一条新 update（HeadConflict 会让 head 停在 {head_before}）"
    );
    let persisted = load_body_doc(&pair.a_vault, &pair.doc_id, 0, None)
        .expect("重放 A 落库正文")
        .0
        .text();
    assert!(
        persisted.contains('z') && persisted.contains("from-a") && persisted.contains("from-b"),
        "落库正文必须同时包含本地键入与两轮远端文字：{:?}",
        &persisted[..persisted.len().min(48)]
    );
}

// ---------------------------------------------------------------------------
// 1b) 附件插入：A 插入真实文件 → A 同步 + 发布基线（ASSET 分片上传）→ B 同步后
//     取到**相同资产字节**，并且正文里的 `asset://` 链接也到了 B（不重开）
// ---------------------------------------------------------------------------

/// 真实点击一个 selector（headless：先查 bounds 再点中心，不猜坐标）。
/// 通过**真实粘贴链路**在某个 shell 里插入一个本地文件（ADR 0019 S2b：粘贴/拖放是
/// 唯一导入入口，旧的「插入附件…」路径输入条已移除）。
fn paste_file_via_clipboard(cx: &mut VisualTestContext, host: &Entity<ShellHost>, path: &Path) {
    cx.write_to_clipboard(gpui::ClipboardItem {
        entries: vec![gpui::ClipboardEntry::ExternalPaths(gpui::ExternalPaths(
            vec![path.to_path_buf()].into_iter().collect(),
        ))],
    });
    cx.simulate_keystrokes("ctrl-v");
    cx.run_until_parked();
    let error = read_shell(cx, host, |shell, _app| {
        shell
            .import_error_text()
            .map(std::borrow::ToOwned::to_owned)
    });
    assert_eq!(error, None, "A 粘贴导入附件失败：{error:?}");
}

#[gpui::test]
async fn attachment_asset_bytes_and_link_reach_the_other_shell(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let pair = establish_pair("attachment-asset");
    let relay_url = pair.relay.base_url().to_owned();
    let note_id = list_tree(&pair.a_vault)
        .expect("读 A 结构树失败")
        .into_iter()
        .find(|entry| entry.is_note())
        .and_then(|entry| entry.note_id)
        .expect("A 必须有第一篇笔记的 note_id");
    // A 与 B（同一进程两个 window，各自独立 Vault/身份/服务）。
    let (host_a, a_handle) = add_shell_window(
        cx,
        pair.a_vault.clone(),
        "dev-sync-a",
        config_for(&relay_url, &pair.a_path),
        pair.a_sync,
    );
    let (host_b, b_handle) = add_shell_window(
        cx,
        pair.b_vault.clone(),
        "dev-sync-b",
        config_for(&relay_url, &pair.b_path),
        pair.b_sync,
    );
    let app_cx = cx.clone();
    let mut cx_a = VisualTestContext::from_window(a_handle, &app_cx);
    let mut cx_b = VisualTestContext::from_window(b_handle, &app_cx);
    cx_a.run_until_parked();
    cx_b.run_until_parked();

    // --- A：插入一个真实附件（中文 + 空格路径；内容固定便于逐字节比对） ---
    let dir = temp_root("attachment-asset-file");
    let file_dir = dir.path().join("附件 目录");
    std::fs::create_dir_all(&file_dir).expect("创建中文/空格目录失败");
    let data = b"menghunote attachment bytes over dev relay".to_vec();
    let file = file_dir.join("报告 最终.txt");
    std::fs::write(&file, &data).expect("写临时附件失败");
    paste_file_via_clipboard(&mut cx_a, &host_a, &file);
    let refs =
        menghunote_storage::list_asset_refs(&pair.a_vault, &note_id).expect("读 A asset_refs 失败");
    assert_eq!(refs.len(), 1, "A 必须正好一条资产引用：{refs:?}");
    let asset_hash = refs[0].asset_hash;
    let markdown_label = "[附件：报告 最终.txt](asset://";
    let text_a = current_text(&mut cx_a, &host_a);
    assert!(
        text_a.contains(markdown_label),
        "A 的正文必须出现附件链接：{:?}",
        text_a.chars().take(80).collect::<String>()
    );

    // A：持闸同步 → 正文更新（含链接）推到 relay；**普通同步同时增量上传 ASSET 对象**
    // （不再依赖「发布基线」才上传资产）。
    start_sync(&mut cx_a, &host_a);
    drive_sync(&mut cx_a, &host_a);
    let report_a = last_report(&mut cx_a, &host_a).expect("A 必须有同步报告");
    assert!(report_a.is_ok(), "A 的同步必须成功：{report_a:?}");

    // 前置：B 此刻**还没有**资产字节（证明确实是同步把它带过去的，不是 bootstrap）。
    assert_eq!(
        menghunote_storage::asset_bytes(&pair.b_vault, asset_hash).expect("读 B 资产失败"),
        None,
        "B 同步前不应有该资产"
    );

    // --- B：持闸同步 → 拉正文更新 + ASSET 事件（验证 + 落本地 assets 表）---
    start_sync(&mut cx_b, &host_b);
    drive_sync(&mut cx_b, &host_b);
    let report_b = last_report(&mut cx_b, &host_b).expect("B 必须有同步报告");
    assert!(report_b.is_ok(), "B 的同步必须成功：{report_b:?}");
    assert!(
        report_b.changes >= 1,
        "B 本轮必须交付至少 1 条变化（A 的正文 update）：{report_b:?}"
    );
    assert!(
        !active_held(&mut cx_b, &host_b),
        "B 同步完成后闸门必须已释放"
    );

    // B 不重开文档就能看到 A 插入的链接。
    let text_b = current_text(&mut cx_b, &host_b);
    assert!(
        text_b.contains(markdown_label),
        "B 的正文必须出现同一条附件链接：{:?}",
        text_b.chars().take(80).collect::<String>()
    );
    // B 取到**相同字节**（内容寻址：哈希由明文算，再用名字查回）。
    assert_eq!(
        menghunote_storage::asset_bytes(&pair.b_vault, asset_hash).expect("读 B 资产失败"),
        Some(data.clone()),
        "B 必须能取到与 A 完全一致的资产字节"
    );
    let meta_b = menghunote_storage::asset_meta(&pair.b_vault, asset_hash)
        .expect("读 B 资产元数据失败")
        .expect("B 必须有该资产");
    assert_eq!(meta_b.byte_len, Some(data.len() as i64));
    assert_eq!(meta_b.media_type.as_deref(), Some("text/plain"));
    assert!(
        !menghunote_storage::list_assets(&pair.b_vault)
            .expect("列 B 资产失败")
            .is_empty(),
        "B 的 assets 表必须有该资产"
    );

    // --- A：发布基线（回归）：checkpoint 必须复用普通同步已上传的对象，不得 ID_CONFLICT ---
    let publish = DevLocal::publish_initial_pair_with_identity(
        relay_url.as_str(),
        &pair.a_vault,
        &pair.a_identity,
    )
    .expect("普通同步之后发布基线失败（资产对象被重复上传/预算来源冲突）");
    assert!(
        !publish.assets.is_empty(),
        "基线必须包含该资产（复用普通同步已上传的对象）：{publish:?}"
    );

    // 发布之后再各同步一轮：不重复上传、不冲突，B 的字节保持不变。
    start_sync(&mut cx_a, &host_a);
    drive_sync(&mut cx_a, &host_a);
    let after_publish_a = last_report(&mut cx_a, &host_a).expect("A 必须有同步报告");
    assert!(
        after_publish_a.is_ok(),
        "A 发布后同步必须成功：{after_publish_a:?}"
    );
    start_sync(&mut cx_b, &host_b);
    drive_sync(&mut cx_b, &host_b);
    let after_publish_b = last_report(&mut cx_b, &host_b).expect("B 必须有同步报告");
    assert!(
        after_publish_b.is_ok(),
        "B 发布后同步必须成功：{after_publish_b:?}"
    );
    assert_eq!(
        menghunote_storage::asset_bytes(&pair.b_vault, asset_hash).expect("读 B 资产失败"),
        Some(data.clone()),
        "发布基线不得改写/重复上传 B 已取到的资产"
    );

    // 收尾：两边都能正常关窗（不把在途同步拖进关窗）。
    with_shell(&mut cx_a, &host_a, |shell, cx| shell.close_all(cx));
    with_shell(&mut cx_b, &host_b, |shell, cx| shell.close_all(cx));
    cx_a.run_until_parked();
    cx_b.run_until_parked();
}

// ---------------------------------------------------------------------------
// 2) 失败路径：传输失败 → 闸门释放、正文不变、可重试；同步中 Busy 拒绝重复命令
// ---------------------------------------------------------------------------

/// "只 listen、不应答"的 relay：客户端 connect 会成功（内核 backlog），但永远等不到响应。
///
/// 不需要后台线程：只要不 `accept`，请求就卡在读响应上。drop 它会让内核重置排队的连接，
/// worker 的阻塞读随之失败（这正是要测的"传输失败"路径）。
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
async fn failed_sync_releases_gate_and_can_retry(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let pair = establish_pair("failure");
    // setup 用真实 relay；窗口里的 worker 指向黑洞（本地闸门/库照常，只有网络失败）。
    let blackhole = BlackholeRelay::start();

    let (host, handle) = add_shell_window(
        cx,
        pair.b_vault.clone(),
        "dev-sync-failure",
        config_for(&blackhole.url(), &pair.b_path),
        pair.b_sync,
    );
    let app_cx = cx.clone();
    let mut cx = VisualTestContext::from_window(handle, &app_cx);
    cx.run_until_parked();
    let text_before = current_text(&mut cx, &host);
    assert_eq!(text_before, pair.seed_text, "失败前正文必须是基线里那份");

    // 1) 发起同步：ADR 0012 S2a 的网络阶段**不持闸**，worker 卡在传输上（没有 Completed）。
    start_sync(&mut cx, &host);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        with_shell(&mut cx, &host, |shell, cx| shell.dev_sync_pump(cx));
        cx.run_until_parked();
        if service_snapshot(&mut cx, &host).is_some_and(|snapshot| {
            snapshot
                .two_phase
                .as_ref()
                .is_some_and(|two| two.phase == menghunote_app::dev_sync::SyncPhase::Prepare)
        }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "10s 内没有进入两阶段 Prepare（同步没有真正开始）"
        );
        std::thread::yield_now();
    }
    assert!(
        !active_held(&mut cx, &host),
        "网络阶段（Prepare）绝不能持闸（否则同步会周期性冻结输入）"
    );
    assert!(
        cx.debug_bounds("editor-sync-held").is_none(),
        "网络阶段不该出现 editor-sync-held"
    );
    let line = panel_sync_line(&mut cx, &host).expect("面板必须有同步行");
    assert!(
        line.contains("网络") || line.contains("同步中") || line.contains("等待持闸"),
        "面板必须显示网络阶段/同步中：{line:?}"
    );
    assert!(
        read_shell(&mut cx, &host, |shell, _app| shell
            .dev_sync_sync_in_progress()),
        "同步必须仍在途（黑洞不应答）"
    );

    // 2) 重复命令必须被明确拒绝（Busy 语义），且不能动闸门。
    let duplicate = with_shell(&mut cx, &host, |shell, cx| shell.dev_sync_sync_once(cx));
    let duplicate_error = match duplicate {
        Ok(()) => panic!("持闸同步在途时必须拒绝第二条命令"),
        Err(error) => error,
    };
    assert!(
        duplicate_error.contains("同步已在进行"),
        "拒绝原因必须说清已在同步：{duplicate_error}"
    );
    assert!(
        !active_held(&mut cx, &host),
        "重复命令被拒后网络阶段仍然不持闸（不受影响）"
    );

    // 3) 关闭黑洞 → worker 的阻塞读失败 → 明确失败 → 闸门释放、正文不变。
    drop(blackhole);
    drive_sync(&mut cx, &host);
    let report = last_report(&mut cx, &host).expect("失败也必须留下报告");
    assert!(
        !report.dispatched_under_gate,
        "Prepare 失败发生在持闸之前（没有 GrantGate，不是 Held 下派发）：{report:?}"
    );
    assert!(
        report.error.is_some(),
        "relay 不可达必须是明确失败（不能谎报成功）：{report:?}"
    );
    assert_eq!(report.changes, 0, "没有外部落库行时不该交付变化");
    assert!(
        !active_held(&mut cx, &host),
        "Prepare 失败从未持闸（也不可能留下冻结）"
    );
    assert!(
        cx.debug_bounds("editor-sync-held").is_none(),
        "失败后 editor-sync-held 必须消失"
    );
    let text_after_failure = current_text(&mut cx, &host);
    assert_eq!(text_after_failure, text_before, "同步失败不得改动 UI 正文");

    // 4) 失败后本地输入必须仍然有效（证明网络阶段没有冻结），并能再次重试。
    focus_editor(&mut cx);
    cx.simulate_keystrokes("R");
    cx.run_until_parked();
    assert!(
        current_text(&mut cx, &host).contains('R'),
        "Prepare 失败后本地键入必须保持有效"
    );
    start_sync(&mut cx, &host);
    drive_sync(&mut cx, &host);
    let retry = last_report(&mut cx, &host).expect("重试报告");
    assert!(
        retry.error.is_some(),
        "relay 仍不可达时重试必须再次明确失败（不是 Busy、不是假成功）：{retry:?}"
    );
    assert!(!active_held(&mut cx, &host), "重试失败后闸门同样不该被持有");
}

// ---------------------------------------------------------------------------
// 2b) 超时安全（ADR 0012 S1/S2a）：Prepare 超时只 Cancelling，不持闸、不派第二操作
// ---------------------------------------------------------------------------

/// 网络阶段（Prepare）超时：
///
/// - 看门狗只把 op 标成 `Cancelling`，**不**合成可解闸的终态；
/// - 闸门全程没有 Held（Prepare 不持闸），正文可继续编辑；
/// - 第二条命令被拒（不派发第二操作）；
/// - barrier（黑洞 relay）放行后，真实回执到达才收尾，并注明"超时后真实收尾"。
///
/// 边界内的超时（已 Held、正在 integrate）另见 `dev_sync_two_phase.rs` 的注入 runner
/// 用例——那里能在真实 AppShell 上把 worker 卡在 `Integrate` 阶段。
#[gpui::test]
async fn prepare_timeout_never_holds_the_gate_and_waits_for_the_real_receipt(
    cx: &mut TestAppContext,
) {
    ensure_test_user_key();
    let pair = establish_pair("prepare-timeout");
    // setup 用真实 relay；窗口的 worker 指向黑洞（卡在传输上），operation deadline = 0。
    let blackhole = BlackholeRelay::start();
    let mut config = config_for(&blackhole.url(), &pair.b_path);
    config.service.operation_deadline = Some(Duration::ZERO);

    let (host, handle) = add_shell_window(
        cx,
        pair.b_vault.clone(),
        "dev-sync-prepare-timeout",
        config,
        pair.b_sync,
    );
    let app_cx = cx.clone();
    let mut cx = VisualTestContext::from_window(handle, &app_cx);
    cx.run_until_parked();
    let text_before = current_text(&mut cx, &host);
    assert_eq!(text_before, pair.seed_text, "超时前正文必须是基线");

    // 1) 发起同步：网络阶段，闸门不 Held。
    start_sync(&mut cx, &host);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        with_shell(&mut cx, &host, |shell, cx| shell.dev_sync_pump(cx));
        cx.run_until_parked();
        if service_snapshot(&mut cx, &host).is_some_and(|snapshot| {
            snapshot
                .two_phase
                .as_ref()
                .is_some_and(|two| two.phase == menghunote_app::dev_sync::SyncPhase::Prepare)
        }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "10s 内没有进入 Prepare（同步没有真正开始）"
        );
        std::thread::yield_now();
    }
    assert!(!active_held(&mut cx, &host), "Prepare 阶段绝不能持闸");

    // 2) 泵到看门狗判定超时（状态收敛点 = snapshot.deadline_exceeded）。
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        with_shell(&mut cx, &host, |shell, cx| shell.dev_sync_pump(cx));
        cx.run_until_parked();
        if service_snapshot(&mut cx, &host)
            .is_some_and(|snapshot| snapshot.deadline_exceeded.is_some())
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "10s 内没有观察到 operation deadline"
        );
        std::thread::yield_now();
    }

    // 3) 超时后：闸门仍不 Held、单操作位仍占着、没有伪造的 Completed。
    assert!(
        !active_held(&mut cx, &host),
        "网络阶段超时后也不该持闸（没有 GrantGate）"
    );
    assert!(
        read_shell(&mut cx, &host, |shell, _app| shell
            .dev_sync_sync_in_progress()),
        "超时后同步必须仍在途，不能假装结束"
    );
    let snapshot = service_snapshot(&mut cx, &host).expect("服务快照");
    let op_id = snapshot.in_flight.expect("单操作位必须仍被占着").0;
    assert_eq!(
        snapshot.deadline_exceeded,
        Some(op_id),
        "超时必须以 deadline_exceeded 标记，而不是终态"
    );
    assert!(
        snapshot.last_completed.is_none(),
        "看门狗绝不能合成可解闸的 Completed：{snapshot:?}"
    );
    assert!(
        matches!(
            snapshot.state,
            menghunote_app::dev_sync::DevSyncState::Cancelling { op_id: cancelled }
                if cancelled == op_id
        ),
        "状态必须是 Cancelling：{:?}",
        snapshot.state
    );

    // 4) 第二条命令：明确拒绝，且不能派发。
    let duplicate = with_shell(&mut cx, &host, |shell, cx| shell.dev_sync_sync_once(cx));
    let duplicate_error = match duplicate {
        Ok(()) => panic!("超时未收尾时不得派发第二操作"),
        Err(error) => error,
    };
    assert!(
        duplicate_error.contains("已在进行")
            || duplicate_error.contains("真实收尾")
            || duplicate_error.contains("超时"),
        "拒绝原因必须可读：{duplicate_error}"
    );
    assert!(
        !active_held(&mut cx, &host),
        "重复命令被拒后闸门仍不该 Held"
    );
    assert_eq!(
        service_snapshot(&mut cx, &host).expect("快照").in_flight,
        Some((op_id, menghunote_app::dev_sync::CommandKind::SyncTwoPhase)),
        "单操作位不能被复用"
    );

    // 5) 网络阶段不冻结：超时期间仍然可以输入（Prepare 不持闸）。
    focus_editor(&mut cx);
    cx.simulate_input("during-prepare");
    cx.run_until_parked();
    assert!(
        current_text(&mut cx, &host).contains("during-prepare"),
        "Prepare（含超时等待）期间输入必须有效"
    );

    // 6) 放行（drop 黑洞 → Prepare 的真实传输失败）→ 真实回执到达才收尾。
    drop(blackhole);
    drive_sync(&mut cx, &host);
    assert!(
        !active_held(&mut cx, &host),
        "真实回执到达后闸门必须仍是未持有（从未 GrantGate）"
    );
    let report = last_report(&mut cx, &host).expect("必须留下报告");
    let error = report.error.clone().expect("黑洞 relay 必须明确失败");
    assert!(
        error.contains("超时后真实收尾") || error.contains("取消"),
        "报告必须注明超时/取消后的真实收尾（不丢）：{error}"
    );
    assert!(
        !report.dispatched_under_gate,
        "Prepare 失败不会走到 GrantGate：{report:?}"
    );
    let snapshot = service_snapshot(&mut cx, &host).expect("快照");
    assert_eq!(snapshot.deadline_exceeded, None);
    assert!(snapshot.in_flight.is_none(), "真实终态之后单操作位才释放");
    assert!(
        current_text(&mut cx, &host).contains("during-prepare"),
        "失败收尾不得丢掉网络阶段已键入的本地正文"
    );
}

// ---------------------------------------------------------------------------
// 3) 面板按钮：`dev-sync-sync` 点击走同一条持闸路径
// ---------------------------------------------------------------------------

#[gpui::test]
async fn panel_sync_button_runs_the_held_sync_path(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let pair = establish_pair("panel-button");
    let relay_url = pair.relay.base_url().to_owned();

    // A 侧不走 UI：直接按生产 storage 路径追加一段并同步（把"远端有变化"造出来）。
    append_body(
        &pair.a_vault,
        &pair.doc_id,
        pair.a_identity.loro_peer_id(),
        pair.a_identity.current_epoch,
        "来自 A 的远端文字\n",
    );
    let a_sync =
        DevLocal::sync_once(&relay_url, &pair.a_vault, &pair.a_identity).expect("A 直接同步失败");
    assert!(a_sync.push.failed == 0, "A 的推送不得有失败项");

    let (host, handle) = add_shell_window(
        cx,
        pair.b_vault.clone(),
        "dev-sync-button",
        config_for(&relay_url, &pair.b_path),
        pair.b_sync,
    );
    let app_cx = cx.clone();
    let mut cx = VisualTestContext::from_window(handle, &app_cx);
    cx.run_until_parked();

    // 前置：面板「同步」行还是空闲、正文还没有远端文字。
    assert_eq!(panel_sync_line(&mut cx, &host).as_deref(), None);
    let text_before = current_text(&mut cx, &host);
    assert!(
        !text_before.contains("来自 A 的远端文字"),
        "点按钮之前 B 不该已经看到 A 的新文字"
    );
    assert!(
        cx.debug_bounds("dev-sync-sync-state").is_some(),
        "同步行必须画出"
    );

    // 真实点击面板「同步」按钮。
    let button = cx
        .debug_bounds("dev-sync-sync")
        .expect("必须画出 selector=dev-sync-sync");
    cx.simulate_click(button.center(), Modifiers::default());
    cx.run_until_parked();
    let notice_after_click = panel_notice(&mut cx, &host).expect("点击必须写面板 notice");
    assert!(
        notice_after_click.contains("请求同步"),
        "按钮只发请求（持闸由 AppShell 做）：{notice_after_click:?}"
    );

    // 泵到收敛：与直接调用 `dev_sync_sync_once` 走的是同一条路径。
    drive_sync(&mut cx, &host);
    let report = last_report(&mut cx, &host).expect("按钮路径也必须留下报告");
    assert!(
        report.dispatched_under_gate,
        "按钮路径必须先在 Held 之后派发：{report:?}"
    );
    assert!(report.is_ok(), "按钮路径的同步必须成功：{report:?}");
    assert!(
        report.changes >= 1,
        "按钮路径必须交付 A 的远端变化：{report:?}"
    );
    assert!(!active_held(&mut cx, &host), "按钮路径结束后闸门必须释放");

    let line_after = panel_sync_line(&mut cx, &host).expect("同步行必须存在");
    assert!(
        line_after.contains("完成") && line_after.contains("条变化"),
        "面板同步行必须显示完成与变化条数：{line_after:?}"
    );
    assert!(
        panel_notice(&mut cx, &host).is_some_and(|notice| notice.contains("同步完成")),
        "面板最近操作必须显示同步完成"
    );
    let text_after = current_text(&mut cx, &host);
    assert!(
        text_after.contains("来自 A 的远端文字"),
        "按钮路径同步之后 B 的编辑器必须看到远端文字（不重开）"
    );
}

// ---------------------------------------------------------------------------
// 4) 结构树：远端改了结构 → 同步后重投影 + 刷新侧栏（不是假刷新）
// ---------------------------------------------------------------------------

#[gpui::test]
async fn structure_change_reprojects_the_sidebar_after_sync(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let pair = establish_pair("structure");
    let relay_url = pair.relay.base_url().to_owned();

    // A：走真实结构写入路径新增一篇笔记（同事务写 sync intent），再直接推给 relay。
    {
        let mut writer = StructureWriter::open_with_sync(
            &pair.a_vault,
            pair.a_identity.loro_peer_id(),
            Some(pair.a_sync),
        )
        .expect("打开 A 的结构写入口失败");
        writer
            .doc()
            .create_note("远端新增笔记", 1)
            .expect("A 新建笔记失败");
        writer.commit("dev-sync-structure").expect("A 结构提交失败");
    }
    DevLocal::sync_once(&relay_url, &pair.a_vault, &pair.a_identity).expect("A 同步结构变更失败");

    let (host, handle) = add_shell_window(
        cx,
        pair.b_vault.clone(),
        "dev-sync-structure",
        config_for(&relay_url, &pair.b_path),
        pair.b_sync,
    );
    let app_cx = cx.clone();
    let mut cx = VisualTestContext::from_window(handle, &app_cx);
    cx.run_until_parked();

    // 同步前：B 只有基线里的 3 个节点，侧栏没有第 4 行。
    assert_eq!(
        list_tree(&pair.b_vault).expect("读 B 结构树失败").len(),
        3,
        "同步前 B 的树必须还是基线那 3 个节点"
    );
    assert!(
        cx.debug_bounds("tree-row-3").is_none(),
        "同步前侧栏不该有第 4 行"
    );
    let view_before = active_view(&mut cx, &host).expect("B 的活动视图");
    let text_before = current_text(&mut cx, &host);

    start_sync(&mut cx, &host);
    drive_sync(&mut cx, &host);
    let report = last_report(&mut cx, &host).expect("结构同步报告");
    assert!(report.is_ok(), "结构同步必须成功：{report:?}");
    assert!(
        report.structure_refreshed,
        "结构 head 变了就必须重投影（不能假刷新）：{report:?}"
    );

    // 存储侧与 UI 侧都必须出现新节点（重投影 + refresh_tree 的真实效果）。
    let tree = list_tree(&pair.b_vault).expect("同步后读 B 结构树失败");
    assert_eq!(tree.len(), 4, "同步后 B 的树必须是 4 个节点：{tree:?}");
    assert!(
        tree.iter()
            .any(|entry| entry.title == "远端新增笔记" && entry.is_note()),
        "新节点必须来自真实结构 CRDT 投影"
    );
    let row = cx
        .debug_bounds("tree-row-3")
        .expect("同步后侧栏必须画出第 4 行（refresh_tree 真的跑了）");
    assert!(
        row.size.width.as_f32() > 0.0 && row.size.height.as_f32() > 0.0,
        "新行必须有非零尺寸：{row:?}"
    );

    // 本片只管活动文档正文：它不变，视图也不重开。
    assert_eq!(
        current_text(&mut cx, &host),
        text_before,
        "结构变更不该改动当前活动文档的正文"
    );
    assert_eq!(
        active_view(&mut cx, &host),
        Some(view_before),
        "结构刷新不能重开/切换活动文档"
    );
}

// ---------------------------------------------------------------------------
// 5) 空库 / 无活动文档：无会话同步（不持闸也能同步，拿到结构后自动打开第一篇）
//
// 这一组覆盖 ADR 0009 的已知边界修复：以前 `dev_sync_sync_once` 在"没有活动文档"时
// 直接报"当前没有打开的笔记（闸门/adopt 无处转发）"，空库因此永远同步不了。现在没有
// 活动文档走**无会话同步**（`Command::SyncOnceNoSession`）：不持闸、不 adopt，但同步
// 期间冻结结构写入，结束后按结构 head 重投影并自动打开第一篇。
// ---------------------------------------------------------------------------

/// 空库单实例：Create（**不播种演示树**）→ 结构树为空、窗口里没有活动文档。
struct EmptyInstance {
    relay: DevRelay,
    _dir: TempDir,
    path: PathBuf,
    vault: Vault,
    sync: SyncContext,
}

/// Create 一个空库实例（与生产 Create 同一条路径，只是**不**走 `dev_vault` 播种入口）。
fn establish_empty_instance(prefix: &str) -> EmptyInstance {
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let dir = temp_root(prefix);
    let path = dir.path().join("inst").join("vault.sqlite3");
    let created = setup::prepare(
        &config_for(relay.base_url(), &path),
        SetupMode::Create,
        create_secrets(&relay),
    )
    .expect("Create 失败");
    assert!(
        list_tree(&created.vault)
            .expect("读空库结构树失败")
            .is_empty(),
        "Create 之后结构树必须是空的（不播种演示树）"
    );
    let peer = created.identity.loro_peer_id();
    let epoch = created.identity.current_epoch;
    EmptyInstance {
        relay,
        _dir: dir,
        path,
        vault: created.vault,
        sync: SyncContext::new(peer, epoch),
    }
}

/// 空库一对实例：A Create → 发布**空**基线 → B 只凭 vault_id + 恢复码 Join。
///
/// 与 [`establish_pair`] 的差别只有一处：两边都**不**走
/// `dev_vault::open_vault_at_with_peer`（那个入口会在空树上播种 3 个演示节点），
/// 所以 A/B 的结构树都是空的——这正是"没有活动文档"的场景。
struct EmptyPair {
    relay: DevRelay,
    _dir: TempDir,
    b_path: PathBuf,
    a_vault: Vault,
    b_vault: Vault,
    a_identity: LocalIdentityV1,
    a_sync: SyncContext,
    b_sync: SyncContext,
}

fn establish_empty_pair(prefix: &str) -> EmptyPair {
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let dir = temp_root(prefix);

    // A：真实 genesis（身份受保护落盘），空树 + 发布空基线。
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
    assert!(
        list_tree(&a.vault).expect("读 A 空树失败").is_empty(),
        "Create 之后 A 的树必须是空的（不播种演示树）"
    );
    DevLocal::publish_initial_pair(relay.base_url(), &a.vault, &a.identity, &recovery)
        .expect("A 发布空基线失败");

    // B：只凭 vault_id + 恢复码，不读 A 的目录/身份/root。
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
    assert!(
        list_tree(&b.vault).expect("读 B 空树失败").is_empty(),
        "空基线 Join 之后 B 的树必须是空的"
    );

    EmptyPair {
        relay,
        _dir: dir,
        b_path,
        a_vault: a.vault,
        b_vault: b.vault,
        a_identity: a.identity,
        a_sync: SyncContext::new(a_peer, a_epoch),
        b_sync: SyncContext::new(b_peer, b_epoch),
    }
}

/// 在**空库**里新建一篇笔记：结构 op + 正文都走真实 storage 路径（同事务写 sync intent）。
///
/// 返回新笔记的 body doc id。
fn create_note_from_scratch(pair: &EmptyPair, title: &str, body: &str) -> String {
    let peer = pair.a_identity.loro_peer_id();
    let epoch = pair.a_identity.current_epoch;
    let body_doc_id = {
        let mut writer = StructureWriter::open_with_sync(&pair.a_vault, peer, Some(pair.a_sync))
            .expect("打开 A 的结构写入口失败");
        let seed = writer.doc().create_note(title, 1).expect("A 新建笔记失败");
        writer.commit("dev-sync-empty").expect("A 结构提交失败");
        seed.body_doc_id
    };
    append_body(&pair.a_vault, &body_doc_id, peer, epoch, body);
    body_doc_id
}

/// 空 B 拉取 A 新建的笔记：**没有活动文档**也能同步，拿到结构后自动打开第一篇。
#[gpui::test]
async fn empty_b_pulls_a_new_note_and_opens_it(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let pair = establish_empty_pair("empty-b");
    let relay_url = pair.relay.base_url().to_owned();

    // A 侧（不开窗）：Create/发布空基线之后新建一篇笔记（结构 + 正文），再自己同步一轮。
    let title = "A 的第一篇笔记";
    let body = "来自 A 的正文：空库同步拿到它\n";
    let doc_id = create_note_from_scratch(&pair, title, body);
    DevLocal::sync_once(&relay_url, &pair.a_vault, &pair.a_identity).expect("A 同步新笔记失败");

    // B：空库 + 空树 → 窗口里**没有活动文档**。
    let (host, handle) = add_shell_window(
        cx,
        pair.b_vault.clone(),
        "dev-sync-empty-b",
        config_for(&relay_url, &pair.b_path),
        pair.b_sync,
    );
    let app_cx = cx.clone();
    let mut cx = VisualTestContext::from_window(handle, &app_cx);
    cx.run_until_parked();

    assert!(
        cx.debug_bounds("dev-sync-panel").is_some(),
        "B 的 dev-sync 面板必须挂载"
    );
    assert!(
        read_shell(&mut cx, &host, |shell, _app| shell.dev_sync_active_view()).is_none(),
        "空库不该有活动文档（这正是旧行为直接报错的前提）"
    );

    // 关键回归：没有活动文档时点「同步」必须被接受（旧行为：直接报"没有打开的笔记"）。
    start_sync(&mut cx, &host);
    drive_sync(&mut cx, &host);

    let report = last_report(&mut cx, &host).expect("空库同步也必须留下报告");
    assert!(report.is_ok(), "空库同步必须成功：{report:?}");
    assert_eq!(report.error, None, "空库同步不得留下错误：{report:?}");
    assert!(
        report.no_session,
        "没有活动文档必须走无会话路径：{report:?}"
    );
    assert!(
        !report.dispatched_under_gate,
        "无会话路径没有持闸证明（不能谎报）：{report:?}"
    );
    assert_eq!(report.doc_id, None, "无会话同步没有活动文档：{report:?}");
    assert!(
        report.structure_refreshed,
        "结构 head 变了必须重投影（A 新建了笔记）：{report:?}"
    );
    assert_eq!(
        report.opened_note.as_deref(),
        Some(title),
        "必须自动打开 A 新建的笔记：{report:?}"
    );

    // 存储侧：B 的树里出现 A 的笔记。
    let tree = list_tree(&pair.b_vault).expect("同步后读 B 结构树失败");
    assert!(
        tree.iter()
            .any(|entry| entry.title == title && entry.is_note()),
        "B 的树必须出现 A 的笔记：{tree:?}"
    );

    // UI 侧：自动打开了第一篇，正文正确。
    let view = read_shell(&mut cx, &host, |shell, _app| shell.dev_sync_active_view())
        .expect("同步完成后必须自动打开第一篇");
    let opened_doc = read_shell(&mut cx, &host, |_shell, app| {
        view.read(app).dev_sync_doc_id().to_owned()
    });
    assert_eq!(opened_doc, doc_id, "自动打开的必须就是 A 新建的那篇");
    let text = current_text(&mut cx, &host);
    assert!(
        text.contains("来自 A 的正文"),
        "自动打开的正文必须是 A 的：{:?}",
        &text[..text.len().min(64)]
    );

    // 面板：notice 说清已打开哪篇，且不再有旧错误。
    let notice = panel_notice(&mut cx, &host).expect("面板必须有 notice");
    assert!(
        notice.contains("同步完成") && notice.contains(title),
        "notice 必须写清已打开哪篇：{notice:?}"
    );
    assert!(
        !notice.contains("没有打开的笔记"),
        "不得再有「没有打开的笔记」错误：{notice:?}"
    );
    let line = panel_sync_line(&mut cx, &host).expect("同步行必须存在");
    assert!(line.contains("完成"), "同步行必须显示完成：{line:?}");

    // 回归：有了活动文档之后，再点同步必须回到**持闸**路径（原有语义不变）。
    start_sync(&mut cx, &host);
    drive_sync(&mut cx, &host);
    let held_report = last_report(&mut cx, &host).expect("第二轮必须留下报告");
    assert!(held_report.is_ok(), "第二轮必须成功：{held_report:?}");
    assert!(
        !held_report.no_session,
        "有活动文档必须走持闸路径（不能被无会话路径截走）：{held_report:?}"
    );
    assert!(
        held_report.dispatched_under_gate,
        "有活动文档必须在 Held 之后派发：{held_report:?}"
    );
    assert!(!active_held(&mut cx, &host), "第二轮结束后闸门必须已释放");
}

/// 空 A 同步：没有笔记时点同步也不报错（push 空、不凭空造笔记、不自动打开）。
#[gpui::test]
async fn empty_a_syncs_without_notes(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let instance = establish_empty_instance("empty-a");

    let (host, handle) = add_shell_window(
        cx,
        instance.vault.clone(),
        "dev-sync-empty-a",
        config_for(instance.relay.base_url(), &instance.path),
        instance.sync,
    );
    let app_cx = cx.clone();
    let mut cx = VisualTestContext::from_window(handle, &app_cx);
    cx.run_until_parked();
    assert!(
        read_shell(&mut cx, &host, |shell, _app| shell.dev_sync_active_view()).is_none(),
        "空库不该有活动文档"
    );

    start_sync(&mut cx, &host);
    drive_sync(&mut cx, &host);

    let report = last_report(&mut cx, &host).expect("空库同步必须留下报告");
    assert!(report.is_ok(), "空库同步必须成功（push 空）：{report:?}");
    assert_eq!(report.error, None, "空库同步不得留下错误：{report:?}");
    assert!(
        report.no_session,
        "没有活动文档必须走无会话路径：{report:?}"
    );
    assert_eq!(report.changes, 0, "无会话路径没有可交付的正文变化");
    assert!(
        !report.structure_refreshed,
        "结构 head 没变时不得假刷新：{report:?}"
    );
    assert_eq!(report.opened_note, None, "树里没有笔记就没什么可打开");
    let summary = report
        .worker_summary
        .as_deref()
        .expect("成功的同步必须带 worker 摘要");
    assert!(
        summary.contains("推送 0"),
        "空库的推送必须是 0 条：{summary:?}"
    );

    // 空库同步不得凭空造出笔记，也不得切换出活动文档。
    assert!(
        list_tree(&instance.vault)
            .expect("同步后读空库结构树失败")
            .is_empty(),
        "空库同步之后结构树必须仍然为空"
    );
    assert!(
        read_shell(&mut cx, &host, |shell, _app| shell.dev_sync_active_view()).is_none(),
        "树里没有笔记时不该自动打开任何文档"
    );
}

/// 同步期间结构写入被冻结（新建笔记/文件夹被拒），同步结束后恢复。
#[gpui::test]
async fn structure_writes_are_frozen_during_a_sync(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let instance = establish_empty_instance("freeze");
    // 之前用真实 relay 做 Create；窗口里的 worker 指向黑洞：本地结构/状态照常，
    // 只有网络请求会卡住（这样同步能稳定地停在"在途"状态）。
    let blackhole = BlackholeRelay::start();

    let (host, handle) = add_shell_window(
        cx,
        instance.vault.clone(),
        "dev-sync-freeze",
        config_for(&blackhole.url(), &instance.path),
        instance.sync,
    );
    let app_cx = cx.clone();
    let mut cx = VisualTestContext::from_window(handle, &app_cx);
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("sidebar-new-note").is_some(),
        "侧栏「＋ 笔记」必须有测试 selector"
    );
    assert!(
        cx.debug_bounds("sidebar-new-folder").is_some(),
        "侧栏「＋ 文件夹」必须有测试 selector"
    );

    // 1) 发起无会话同步：命令已派发 → 立刻处于"同步在途"（结构写入必须冻结）。
    start_sync(&mut cx, &host);
    assert!(
        read_shell(&mut cx, &host, |shell, _app| shell
            .dev_sync_sync_in_progress()),
        "无会话同步派发后必须处于在途状态"
    );

    // 2) 同步在途时点「＋ 笔记」：必须被拒（不写库、不打开文档），并给出侧栏提示。
    let new_note = cx
        .debug_bounds("sidebar-new-note")
        .expect("selector=sidebar-new-note");
    cx.simulate_click(new_note.center(), Modifiers::default());
    cx.run_until_parked();
    assert!(
        list_tree(&instance.vault)
            .expect("同步中读结构树失败")
            .is_empty(),
        "同步进行中不得写入结构（新建笔记必须被拒）"
    );
    assert_eq!(
        read_shell(&mut cx, &host, |shell, _app| shell
            .dev_sync_sidebar_notice()
            .map(std::borrow::ToOwned::to_owned)),
        Some("同步进行中，稍后再改结构".to_owned()),
        "被拒时必须给出侧栏提示"
    );

    // 3) 同步在途时点「＋ 文件夹」：同样被拒。
    let new_folder = cx
        .debug_bounds("sidebar-new-folder")
        .expect("selector=sidebar-new-folder");
    cx.simulate_click(new_folder.center(), Modifiers::default());
    cx.run_until_parked();
    assert!(
        list_tree(&instance.vault)
            .expect("同步中读结构树失败")
            .is_empty(),
        "同步进行中不得写入结构（新建文件夹必须被拒）"
    );

    // 4) 关掉黑洞 → worker 明确失败 → 同步结束（失败只保留错误、不冻结后续操作）。
    drop(blackhole);
    drive_sync(&mut cx, &host);
    let report = last_report(&mut cx, &host).expect("失败也必须留下报告");
    assert!(
        report.error.is_some(),
        "黑库 relay 必须明确失败（不能谎报成功）：{report:?}"
    );
    assert!(
        report.no_session,
        "没有活动文档必须走无会话路径：{report:?}"
    );
    assert!(
        !read_shell(&mut cx, &host, |shell, _app| shell
            .dev_sync_sync_in_progress()),
        "失败后必须释放 busy 状态"
    );

    // 5) 结束后恢复：同一个按钮这次必须真的建出笔记并打开它。
    let new_note = cx
        .debug_bounds("sidebar-new-note")
        .expect("selector=sidebar-new-note");
    cx.simulate_click(new_note.center(), Modifiers::default());
    cx.run_until_parked();
    let tree = list_tree(&instance.vault).expect("同步后读结构树失败");
    assert_eq!(tree.len(), 1, "同步结束后结构写入必须恢复：{tree:?}");
    assert!(tree[0].is_note(), "新建的必须是一篇笔记：{tree:?}");
    let opened = read_shell(&mut cx, &host, |shell, _app| shell.dev_sync_active_view());
    assert!(
        opened.is_some(),
        "同步结束后新建笔记必须照常自动打开（后续操作不被冻住）"
    );
}

// ---------------------------------------------------------------------------
// 6) ADR 0012 S2a：可暂停两阶段（Prepare 不冻结 / 授权绑定 / 取消 / 超时）
// ---------------------------------------------------------------------------

/// 读面板里的服务句柄（授权竞争/取消用例需要直接发控制命令）。
fn service_of(cx: &mut VisualTestContext, host: &Entity<ShellHost>) -> Arc<SyncService> {
    read_shell(cx, host, |shell, app| {
        shell
            .dev_sync_panel()
            .expect("dev-sync 面板必须挂载")
            .read(app)
            .service()
            .clone()
    })
}

/// 在某个窗口里建一个 `AppShell`，并注入**测试用** `SyncService`（ADR 0012 S2a）。
fn add_shell_window_with_service(
    cx: &mut TestAppContext,
    vault: Vault,
    label: &'static str,
    sync: SyncContext,
    service: Arc<SyncService>,
) -> (Entity<ShellHost>, gpui::AnyWindowHandle) {
    let slot = std::rc::Rc::new(std::cell::Cell::new(None));
    let captured = slot.clone();
    let (host, window_cx) = cx.add_window_view(move |window, cx| {
        captured.set(Some(window.window_handle()));
        let shell = cx.new(|cx| {
            AppShell::new_with_dev_sync_service(vault, label.to_owned(), sync, service, cx)
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

/// 可暂停的 TCP 转发代理：把连接原样转发到真实 relay，但可以在建立连接后**压住**，
/// 直到测试放行。用来把 worker 稳定地卡在 Prepare 的网络阶段（真实 relay 太快，观察不到）。
struct PausableProxy {
    addr: SocketAddr,
    pause: Arc<(Mutex<bool>, Condvar)>,
    paused: Arc<(Mutex<usize>, Condvar)>,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
}

impl PausableProxy {
    fn start(upstream: String) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("代理监听失败");
        let addr = listener.local_addr().expect("代理地址");
        let pause = Arc::new((Mutex::new(true), Condvar::new()));
        let paused = Arc::new((Mutex::new(0usize), Condvar::new()));
        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
        {
            let pause = pause.clone();
            let paused = paused.clone();
            let shutdown = shutdown.clone();
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    if shutdown.load(std::sync::atomic::Ordering::SeqCst) {
                        break;
                    }
                    let Ok(client) = stream else { continue };
                    {
                        let (lock, cv) = &*pause;
                        let mut held = lock.lock().expect("代理 pause 锁");
                        if *held {
                            let (count, count_cv) = &*paused;
                            *count.lock().expect("代理 paused 计数") += 1;
                            count_cv.notify_all();
                            while *held && !shutdown.load(std::sync::atomic::Ordering::SeqCst) {
                                held = cv.wait(held).expect("代理 pause wait");
                            }
                            *count.lock().expect("代理 paused 计数") -= 1;
                        }
                    }
                    // `upstream` 是 relay 的 base URL（http://127.0.0.1:PORT）：
                    // `TcpStream::connect` 只接受 host:port。
                    let host_port = upstream
                        .strip_prefix("http://")
                        .unwrap_or(upstream.as_str());
                    let Ok(server) = TcpStream::connect(host_port) else {
                        continue;
                    };
                    let mut client_read = client.try_clone().expect("client clone");
                    let mut server_write = server.try_clone().expect("server clone");
                    std::thread::spawn(move || {
                        let _ = std::io::copy(&mut client_read, &mut server_write);
                        let _ = server_write.shutdown(std::net::Shutdown::Write);
                    });
                    std::thread::spawn(move || {
                        let mut server_read = server;
                        let mut client_write = client;
                        let _ = std::io::copy(&mut server_read, &mut client_write);
                        // 上游关闭后必须把 EOF 传给客户端（否则客户端会一直等 keep-alive
                        // 的下一条响应；`try_clone` 让 socket 不会因 drop 而关闭）。
                        let _ = client_write.shutdown(std::net::Shutdown::Write);
                    });
                }
            });
        }
        Self {
            addr,
            pause,
            paused,
            shutdown,
        }
    }

    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// 放行（不再暂停后续连接）。
    fn release(&self) {
        let (lock, cv) = &*self.pause;
        *lock.lock().expect("代理 pause 锁") = false;
        cv.notify_all();
    }

    /// 有界等"至少一个连接被暂停"（证明 worker 真的卡在 Prepare 的网络阶段）。
    fn wait_paused(&self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        let (lock, cv) = &*self.paused;
        let mut count = lock.lock().expect("代理 paused 计数");
        while *count == 0 {
            let now = Instant::now();
            assert!(now < deadline, "代理 10s 内没有观察到被暂停的连接");
            let (next, timed) = cv
                .wait_timeout(count, deadline - now)
                .expect("代理 paused wait");
            count = next;
            assert!(!timed.timed_out() || *count > 0, "等待被暂停连接超时");
        }
    }
}

impl Drop for PausableProxy {
    fn drop(&mut self) {
        self.shutdown
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.release();
        // 唤醒 accept 线程（连接一下自己的端口即可）。
        let _ = TcpStream::connect(self.addr);
    }
}

/// 测试用手动时钟：只让 operation deadline 在测试显式推进后到期。
struct TestClock {
    base: Instant,
    offset: Mutex<Duration>,
}

impl TestClock {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            base: Instant::now(),
            offset: Mutex::new(Duration::ZERO),
        })
    }

    fn advance(&self, by: Duration) {
        *self.offset.lock().expect("测试时钟锁") += by;
    }
}

impl OperationClock for TestClock {
    fn now(&self) -> Instant {
        self.base + *self.offset.lock().expect("测试时钟锁")
    }
}

/// 受控两阶段 runner（ADR 0012 S2a）：发 NeedGate → 等 GrantGate → **卡在 Integrate** →
/// 发 Integrated → 等 Finish → 成功。只用于在真实 `AppShell` 上验证窗口行为（持闸范围、
/// 超时保持 Held、无第二操作），不碰库/网络（交付走真实 storage 会话）。
struct ControlledPhaseRunner {
    control: Option<Receiver<SyncControl>>,
    gate: Arc<PhaseGate>,
    /// 放行 Integrate 时模拟"事务已提交"：在 worker 线程里新开连接追加一条正文。
    commit: Option<CommitFixture>,
}

/// 受控 runner 在 Integrate 阶段"提交"的内容（真实 storage 路径 + 独立连接）。
struct CommitFixture {
    vault_path: PathBuf,
    doc_id: String,
    peer: u64,
    epoch: u64,
    text: String,
}

#[derive(Default)]
struct PhaseState {
    grant_seen: bool,
    release_integrate: bool,
    finished: bool,
}

struct PhaseGate {
    state: Mutex<PhaseState>,
    condvar: Condvar,
}

impl PhaseGate {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(PhaseState::default()),
            condvar: Condvar::new(),
        })
    }

    fn observed(&self, predicate: impl Fn(&PhaseState) -> bool) -> bool {
        predicate(&self.state.lock().expect("phase gate 锁"))
    }

    fn release_integrate(&self) {
        let mut state = self.state.lock().expect("phase gate 锁");
        state.release_integrate = true;
        self.condvar.notify_all();
    }
}

impl SyncRunner for ControlledPhaseRunner {
    fn run(&mut self, _op: Command, _events: &mut dyn FnMut(Event)) -> Result<OpOutcome, String> {
        Err("受控两阶段 runner 只支持两阶段同步".to_owned())
    }

    fn attach_control(&mut self, control: Receiver<SyncControl>) {
        self.control = Some(control);
    }

    fn run_two_phase(
        &mut self,
        op_id: OpId,
        _control: &menghunote_app::dev_sync::service::OpControl,
        events: &mut dyn FnMut(Event),
    ) -> Result<OpOutcome, String> {
        events(Event::NeedGate {
            op_id,
            authorization_epoch: 4242,
            stored: 0,
            deferred: 0,
            summary: "受控 Prepare（不持闸）".to_owned(),
        });
        let receiver = self.control.as_ref().ok_or("缺少控制通道")?;
        loop {
            match receiver.recv().map_err(|_| "控制通道已关闭")? {
                SyncControl::GrantGate { op_id: id, .. } if id == op_id => break,
                SyncControl::Stop => return Err("worker 已停止".to_owned()),
                _ => continue,
            }
        }
        {
            let mut state = self.gate.state.lock().expect("phase gate 锁");
            state.grant_seen = true;
            self.gate.condvar.notify_all();
            while !state.release_integrate {
                state = self.gate.condvar.wait(state).expect("phase gate wait");
            }
        }
        if let Some(commit) = &self.commit {
            // 模拟"integrate 的本地事务已经提交"：独立连接、真实 storage 路径。
            let vault = Vault::open(&commit.vault_path).expect("runner 打开库失败");
            append_body(
                &vault,
                &commit.doc_id,
                commit.peer,
                commit.epoch,
                &commit.text,
            );
        }
        events(Event::Integrated {
            op_id,
            changed_heads: Vec::new(),
            integrated: 0,
            deferred: 0,
            summary: "受控 Integrate（已提交）".to_owned(),
        });
        loop {
            match receiver.recv().map_err(|_| "控制通道已关闭")? {
                SyncControl::Finish { op_id: id } if id == op_id => break,
                SyncControl::Cancel { op_id: id } if id == op_id => {
                    // 与生产 worker 的 `wait_for_finish` 同语义：事务已提交、不做 ack，
                    // 已 durable 的变化由 UI 交付。
                    return Err(
                        "集成已提交，但交付前收到取消：本次未 ACK（durable 变化保留，由 UI 交付）"
                            .to_owned(),
                    );
                }
                SyncControl::Stop => return Err("worker 已停止".to_owned()),
                _ => continue,
            }
        }
        let mut state = self.gate.state.lock().expect("phase gate 锁");
        state.finished = true;
        self.gate.condvar.notify_all();
        Ok(OpOutcome::new("受控两阶段完成"))
    }
}

/// 找一棵树里指定标题的节点（含先序下标；点击侧栏行要用下标）。
fn tree_index_of(tree: &[menghunote_storage::TreeEntry], title: &str) -> usize {
    tree.iter()
        .position(|entry| entry.title == title && entry.is_note())
        .unwrap_or_else(|| panic!("树里找不到笔记「{title}」"))
}

/// ADR 0012 S2a 用例 1：**网络阶段可编辑/切换**，且 NeedGate 用"当时"的活动文档持闸。
///
/// A 改第二篇笔记并推 relay；B 的 worker 用可暂停代理卡在 Prepare：
/// - Prepare 期间 B 可以继续输入、可以切换到第二篇（真实 UI 点击）；
/// - 放行后 NeedGate 以**切换后**的第二篇持闸；adopt 后 B 不重开就看到了 A 的文字；
/// - 收尾后继续编辑正常。
#[gpui::test]
async fn prepare_allows_typing_and_switching_then_adopts_into_the_current_document(
    cx: &mut TestAppContext,
) {
    ensure_test_user_key();
    let pair = establish_pair("two-phase-switch");
    let relay_url = pair.relay.base_url().to_owned();

    // A：改"目录里的第二篇"并推到 relay（B 的 Prepare 会把它拉成 durable inbox）。
    let tree_a = list_tree(&pair.a_vault).expect("读 A 树");
    let second_index = tree_index_of(&tree_a, menghunote_app::dev_vault::DEMO_SECOND_TITLE);
    let second_doc = tree_a[second_index]
        .body_doc_id
        .clone()
        .expect("第二篇必须有 body doc");
    append_body(
        &pair.a_vault,
        &second_doc,
        pair.a_identity.loro_peer_id(),
        pair.a_identity.current_epoch,
        "远端第二篇文字\n",
    );
    DevLocal::sync_once(&relay_url, &pair.a_vault, &pair.a_identity).expect("A 推送第二篇失败");

    // B：worker 指向"可暂停代理"（真实 relay 太快，观察不到 Prepare 窗口）。
    let proxy = PausableProxy::start(relay_url.clone());
    let (host, handle) = add_shell_window(
        cx,
        pair.b_vault.clone(),
        "dev-sync-two-phase-switch",
        config_for(&proxy.url(), &pair.b_path),
        pair.b_sync,
    );
    let app_cx = cx.clone();
    let mut cx = VisualTestContext::from_window(handle, &app_cx);
    cx.run_until_parked();
    assert_eq!(current_text(&mut cx, &host), pair.seed_text);

    start_sync(&mut cx, &host);
    proxy.wait_paused(Duration::from_secs(10));
    // 网络阶段：worker 已派发 Prepare 但卡在 pull，不持闸。
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        with_shell(&mut cx, &host, |shell, cx| shell.dev_sync_pump(cx));
        cx.run_until_parked();
        let preparing = service_snapshot(&mut cx, &host).is_some_and(|snapshot| {
            snapshot
                .two_phase
                .as_ref()
                .is_some_and(|two| two.phase == menghunote_app::dev_sync::SyncPhase::Prepare)
        });
        if preparing {
            break;
        }
        assert!(Instant::now() < deadline, "10s 内没有进入 Prepare");
        std::thread::yield_now();
    }
    assert!(!active_held(&mut cx, &host), "Prepare 不得持闸");
    assert!(
        cx.debug_bounds("editor-sync-held").is_none(),
        "Prepare 不得画 editor-sync-held"
    );

    // 网络阶段输入有效。
    focus_editor(&mut cx);
    cx.simulate_input("during-prepare");
    cx.run_until_parked();
    assert!(
        current_text(&mut cx, &host).contains("during-prepare"),
        "Prepare 期间必须能继续输入"
    );

    // 网络阶段可以切换文档（Switching 是真实 UI 点击，不是直接调用私有方法）。
    let second_row_selector: &'static str =
        Box::leak(format!("tree-row-{second_index}").into_boxed_str());
    let second_row = cx
        .debug_bounds(second_row_selector)
        .expect("侧栏第二篇行必须存在");
    cx.simulate_click(second_row.center(), Modifiers::default());
    cx.run_until_parked();
    let view_now = active_view(&mut cx, &host).expect("切换后必须有活动视图");
    let doc_now = read_shell(&mut cx, &host, |_shell, app| {
        view_now.read(app).dev_sync_doc_id().to_owned()
    });
    assert_eq!(doc_now, second_doc, "Prepare 期间必须允许切到第二篇");
    assert!(
        !current_text(&mut cx, &host).contains("during-prepare"),
        "切换后不再显示上一篇的正文"
    );
    assert!(!active_held(&mut cx, &host), "切换本身不得触发持闸");

    // 放行网络 → NeedGate 用**当时**的活动文档（第二篇）持闸 → 集成 → 交付。
    proxy.release();
    drive_sync(&mut cx, &host);
    let report = last_report(&mut cx, &host).expect("必须有两阶段报告");
    assert!(report.is_ok(), "两阶段同步必须成功：{report:?}");
    assert_eq!(
        report.doc_id.as_deref(),
        Some(second_doc.as_str()),
        "NeedGate 必须按切换后的活动文档持闸：{report:?}"
    );
    assert!(report.dispatched_under_gate, "GrantGate 之后才算持闸派发");
    assert!(!active_held(&mut cx, &host), "收尾后闸门必须释放");
    let text_after = current_text(&mut cx, &host);
    assert!(
        text_after.contains("远端第二篇文字"),
        "adopt 后不重开就必须看到远端文字：{text_after:?}"
    );

    // 收尾后继续编辑正常（网络阶段/持闸窗口都结束）。
    cx.simulate_input("after-sync");
    cx.run_until_parked();
    assert!(
        current_text(&mut cx, &host).contains("after-sync"),
        "同步完成后继续编辑必须正常"
    );
}

/// ADR 0012 S2a 用例 2：**Prepare 后取消**——不 integrate、不 append，已 pull 的 durable
/// inbox 不丢；之后正常同步仍能把它交付进 live。
#[gpui::test]
async fn cancel_after_need_gate_keeps_the_durable_inbox_and_never_appends(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let pair = establish_pair("two-phase-cancel");
    let relay_url = pair.relay.base_url().to_owned();

    append_body(
        &pair.a_vault,
        &pair.doc_id,
        pair.a_identity.loro_peer_id(),
        pair.a_identity.current_epoch,
        "取消前的远端文字\n",
    );
    DevLocal::sync_once(&relay_url, &pair.a_vault, &pair.a_identity).expect("A 推送失败");

    let (host, handle) = add_shell_window(
        cx,
        pair.b_vault.clone(),
        "dev-sync-two-phase-cancel",
        config_for(&relay_url, &pair.b_path),
        pair.b_sync,
    );
    let app_cx = cx.clone();
    let mut cx = VisualTestContext::from_window(handle, &app_cx);
    cx.run_until_parked();

    let head_before = load_head(&pair.b_vault, &pair.doc_id)
        .expect("读 B head")
        .head_seq;
    let cursor_before = sync_cursor_state(&pair.b_vault)
        .expect("读 B 游标")
        .received_seq;
    let text_before = current_text(&mut cx, &host);

    start_sync(&mut cx, &host);
    // 等 worker 停下等授权（NeedGate）。此时 Prepare 已把远端变化落成 durable inbox。
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        with_shell(&mut cx, &host, |shell, cx| shell.dev_sync_pump(cx));
        cx.run_until_parked();
        let need_gate = service_snapshot(&mut cx, &host).is_some_and(|snapshot| {
            snapshot
                .two_phase
                .as_ref()
                .is_some_and(|two| two.phase == menghunote_app::dev_sync::SyncPhase::NeedGate)
        });
        if need_gate {
            break;
        }
        assert!(Instant::now() < deadline, "10s 内没有进入 NeedGate");
        std::thread::yield_now();
    }
    assert!(!active_held(&mut cx, &host), "NeedGate 时还没有持闸");
    let cursor_after_prepare = sync_cursor_state(&pair.b_vault)
        .expect("读 B 游标")
        .received_seq;
    assert!(
        cursor_after_prepare > cursor_before,
        "Prepare 必须推进 pull 游标（{cursor_before} → {cursor_after_prepare}）"
    );
    assert!(
        !take_inbox_ready(&pair.b_vault, 32)
            .expect("读 inbox")
            .is_empty(),
        "Prepare 必须把远端变化落成 durable inbox"
    );

    // 用户在授权前取消（服务级取消命令；S1 语义）。
    let service = service_of(&mut cx, &host);
    let op_id = service.snapshot().in_flight.expect("必须有在途 op").0;
    let cancelled = service.try_command(Command::Cancel).expect("取消在途");
    assert_eq!(cancelled, op_id);

    drive_sync(&mut cx, &host);
    let report = last_report(&mut cx, &host).expect("取消也必须留下报告");
    assert!(
        report
            .error
            .as_deref()
            .is_some_and(|error| error.contains("取消")),
        "报告必须如实写「取消」，不能谎报成功：{report:?}"
    );
    assert!(!report.dispatched_under_gate, "取消发生在授权前");
    assert!(!active_held(&mut cx, &host), "取消后不得留下持闸");
    assert_eq!(
        load_head(&pair.b_vault, &pair.doc_id)
            .expect("读 B head")
            .head_seq,
        head_before,
        "取消后绝不能 append（head 必须不动）"
    );
    assert_eq!(
        current_text(&mut cx, &host),
        text_before,
        "取消不得改动 UI 正文"
    );
    assert!(
        !take_inbox_ready(&pair.b_vault, 32)
            .expect("读 inbox")
            .is_empty(),
        "取消不得丢弃已 pull 的 durable inbox"
    );

    // 再次同步：同一份 inbox 被正常交付（证明取消没有丢数据）。
    start_sync(&mut cx, &host);
    drive_sync(&mut cx, &host);
    let retry = last_report(&mut cx, &host).expect("重试报告");
    assert!(retry.is_ok(), "取消后的正常重试必须成功：{retry:?}");
    assert!(
        retry.changes >= 1,
        "重试必须交付 Prepare 已拉取的那条变化：{retry:?}"
    );
    assert!(
        current_text(&mut cx, &host).contains("取消前的远端文字"),
        "重试后必须看到先前 durable inbox 里的文字"
    );
}

/// ADR 0012 S2a 用例 3：**授权绑定**——错误 op/授权代际/持闸 head 一律拒绝，且不 append。
#[gpui::test]
async fn authorization_is_bound_to_op_epoch_and_head(_cx: &mut TestAppContext) {
    ensure_test_user_key();
    let pair = establish_pair("two-phase-binding");
    let relay_url = pair.relay.base_url().to_owned();

    // 直接驱动服务（不挂 AppShell）：只看授权绑定与 worker 的集成前复核。
    let service = SyncService::new_dev_local(ServiceConfig::new(&relay_url), pair.b_path.clone());
    let op_id = service.try_command_two_phase().expect("发起 Prepare");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        service.poll();
        let need_gate = service.snapshot().two_phase.as_ref().is_some_and(|two| {
            two.op_id == op_id && two.phase == menghunote_app::dev_sync::SyncPhase::NeedGate
        });
        if need_gate {
            break;
        }
        assert!(Instant::now() < deadline, "10s 内没有进入 NeedGate");
        std::thread::yield_now();
    }
    let snapshot = service.snapshot();
    let epoch = snapshot
        .two_phase
        .as_ref()
        .and_then(|two| two.authorization_epoch)
        .expect("NeedGate 必须给出授权代际");
    let head_before = load_head(&pair.b_vault, &pair.doc_id)
        .expect("读 B head")
        .head_seq;
    let gate = HeldGate::new(pair.doc_id.clone(), head_before, 1).with_session_epoch(1);

    // 错误 op_id：服务层直接拒绝（根本不会把授权交给 worker）。
    assert!(
        service
            .grant_gate(OpId::new(9999), epoch, gate.clone())
            .is_err(),
        "错误 op_id 的授权必须被拒绝"
    );
    // 错误授权代际（旧 epoch）：拒绝。
    assert!(
        service
            .grant_gate(op_id, epoch.wrapping_add(1), gate.clone())
            .is_err(),
        "错误授权代际的 GrantGate 必须被拒绝"
    );
    // 错误 head：服务层放行，worker 在集成前复核并拒绝（fail-closed，不 append）。
    let wrong_head = HeldGate::new(pair.doc_id.clone(), head_before + 7, 1).with_session_epoch(1);
    service
        .grant_gate(op_id, epoch, wrong_head)
        .expect("服务层只做 op/代际绑定；head 由 worker 复核");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        service.poll();
        if service
            .snapshot()
            .last_completed
            .as_ref()
            .is_some_and(|(completed, _, _)| *completed == op_id)
        {
            break;
        }
        assert!(Instant::now() < deadline, "10s 内没有拿到真实终态");
        std::thread::yield_now();
    }
    let completed = service.snapshot().last_completed.expect("终态");
    let error = completed
        .2
        .error()
        .expect("错误 head 必须明确失败")
        .to_owned();
    assert!(
        error.contains("持闸基准") || error.contains("未追加"),
        "错误 head 的拒绝原因必须点明持闸基准：{error}"
    );
    assert_eq!(
        load_head(&pair.b_vault, &pair.doc_id)
            .expect("读 B head")
            .head_seq,
        head_before,
        "被拒绝的授权绝不能 append"
    );

    let _ticket = service.request_stop();
    assert!(service.reap_worker(), "worker 必须在 Stop 后退出");
}

/// ADR 0012 S2a 用例 4：**Integrate 超时**——保持 Held/Unknown，真实回执后才交付/释放，
/// 期间不派发第二操作。用注入 runner + 手动时钟，不靠 sleep。
#[gpui::test]
async fn timeout_during_integrate_keeps_the_gate_until_the_real_receipt(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let pair = establish_pair("two-phase-timeout-integrate");
    let gate = PhaseGate::new();
    let clock = TestClock::new();
    let config = ServiceConfig {
        mode: menghunote_app::onboarding::RelayMode::LocalDev,
        relay: "http://127.0.0.1:1".to_owned(),
        api_version: menghunote_app::onboarding::RelayApiVersion::V1,
        operation_deadline: Some(Duration::from_secs(10)),
        transport: DEFAULT_TRANSPORT_TIMEOUTS,
    };
    let service = SyncService::with_runner_and_clock(
        config,
        Box::new(ControlledPhaseRunner {
            control: None,
            gate: gate.clone(),
            commit: None,
        }),
        clock.clone(),
    );

    let (host, handle) = add_shell_window_with_service(
        cx,
        pair.b_vault.clone(),
        "dev-sync-two-phase-timeout-integrate",
        pair.b_sync,
        service.clone(),
    );
    let app_cx = cx.clone();
    let mut cx = VisualTestContext::from_window(handle, &app_cx);
    cx.run_until_parked();

    start_sync(&mut cx, &host);
    // 等 runner 已经收到 GrantGate、闸门 Held、卡在 Integrate（期间必须持续泵 UI）。
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        with_shell(&mut cx, &host, |shell, cx| shell.dev_sync_pump(cx));
        cx.run_until_parked();
        if gate.observed(|state| state.grant_seen) && active_held(&mut cx, &host) {
            break;
        }
        assert!(Instant::now() < deadline, "10s 内没有进入 Held/授权");
        std::thread::yield_now();
    }
    let op_id = service.snapshot().in_flight.expect("在途 op").0;
    assert_eq!(
        service.snapshot().two_phase.as_ref().map(|two| two.phase),
        Some(menghunote_app::dev_sync::SyncPhase::Integrate),
        "授权后阶段必须是 Integrate"
    );

    // 只推进手动时钟：超时 → Cancelling，但**不**打断已授权的事务。
    clock.advance(Duration::from_secs(11));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        with_shell(&mut cx, &host, |shell, cx| shell.dev_sync_pump(cx));
        cx.run_until_parked();
        if service.snapshot().deadline_exceeded.is_some() {
            break;
        }
        assert!(Instant::now() < deadline, "10s 内没有观察到超时");
        std::thread::yield_now();
    }

    // 超时后：Held 必须保持、没有伪造终态、第二条命令被拒。
    assert!(
        active_held(&mut cx, &host),
        "Integrate 超时后闸门必须保持 Held（worker 仍可能写外部行）"
    );
    assert!(
        service.snapshot().last_completed.is_none(),
        "看门狗不能合成可解闸的 Completed"
    );
    let duplicate = with_shell(&mut cx, &host, |shell, cx| shell.dev_sync_sync_once(cx));
    assert!(duplicate.is_err(), "超时未收尾时不得派发第二操作");
    assert!(active_held(&mut cx, &host), "重复命令被拒后闸门仍 Held");
    assert_eq!(
        service.snapshot().in_flight.expect("单操作位").0,
        op_id,
        "单操作位不能被复用"
    );

    // 放行真实事务：发 Integrated → UI 交付/解闸 → Finish → 真实 Completed（迟到回执）。
    gate.release_integrate();
    drive_sync(&mut cx, &host);
    assert!(!active_held(&mut cx, &host), "真实回执到达后闸门必须释放");
    let report = last_report(&mut cx, &host).expect("必须有报告");
    assert!(report.is_ok(), "真实回执成功后应收尾成功：{report:?}");
    assert!(
        report
            .worker_summary
            .as_deref()
            .is_some_and(|summary| summary.contains("超时后真实收尾")),
        "迟到回执必须标注「超时后真实收尾」（不能丢）：{report:?}"
    );
    let snapshot = service.snapshot();
    assert_eq!(snapshot.deadline_exceeded, None);
    assert!(snapshot.in_flight.is_none());

    let _ticket = service.request_stop();
    assert!(service.reap_worker());
}

/// ADR 0012 S2a 用例 2b：**Integrate 中取消**——事务必须跑完并交付，取消不能吞掉已提交变化。
///
/// 受控 runner 在放行时用真实 storage 路径追加一条正文（模拟 integrate 已提交），然后发
/// `Integrated`；此时测试已发出 `Cancel`，worker 在收尾前停手并明确失败。UI 仍必须
/// `adopt → end` 交付那条已 durable 的变化，再如实报告失败。
#[gpui::test]
async fn cancel_during_integrate_waits_for_the_transaction_and_delivers(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let pair = establish_pair("two-phase-cancel-integrate");
    let gate = PhaseGate::new();
    let clock = TestClock::new();
    let config = ServiceConfig {
        mode: menghunote_app::onboarding::RelayMode::LocalDev,
        relay: "http://127.0.0.1:1".to_owned(),
        api_version: menghunote_app::onboarding::RelayApiVersion::V1,
        operation_deadline: Some(Duration::from_secs(10)),
        transport: DEFAULT_TRANSPORT_TIMEOUTS,
    };
    let service = SyncService::with_runner_and_clock(
        config,
        Box::new(ControlledPhaseRunner {
            control: None,
            gate: gate.clone(),
            commit: Some(CommitFixture {
                vault_path: pair.b_path.clone(),
                doc_id: pair.doc_id.clone(),
                peer: pair.b_sync.peer_id,
                epoch: pair.b_sync.key_epoch,
                text: "integrate 已提交的文字\n".to_owned(),
            }),
        }),
        clock.clone(),
    );

    let (host, handle) = add_shell_window_with_service(
        cx,
        pair.b_vault.clone(),
        "dev-sync-two-phase-cancel-integrate",
        pair.b_sync,
        service.clone(),
    );
    let app_cx = cx.clone();
    let mut cx = VisualTestContext::from_window(handle, &app_cx);
    cx.run_until_parked();
    let head_before = load_head(&pair.b_vault, &pair.doc_id)
        .expect("读 B head")
        .head_seq;
    let text_before = current_text(&mut cx, &host);

    start_sync(&mut cx, &host);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        with_shell(&mut cx, &host, |shell, cx| shell.dev_sync_pump(cx));
        cx.run_until_parked();
        if gate.observed(|state| state.grant_seen) && active_held(&mut cx, &host) {
            break;
        }
        assert!(Instant::now() < deadline, "10s 内没有进入 Held/授权");
        std::thread::yield_now();
    }
    let op_id = service.snapshot().in_flight.expect("在途 op").0;
    // 取消在事务内到达：worker 不能中途回滚，已开始的 integrate 必须跑完。
    assert_eq!(
        service.try_command(Command::Cancel).expect("取消在途"),
        op_id
    );
    gate.release_integrate();

    drive_sync(&mut cx, &host);
    let report = last_report(&mut cx, &host).expect("必须留下报告");
    assert!(
        report
            .error
            .as_deref()
            .is_some_and(|error| error.contains("取消")),
        "取消必须如实呈现为失败，不能谎报成功：{report:?}"
    );
    assert!(!active_held(&mut cx, &host), "交付已提交变化后闸门必须释放");
    assert_eq!(
        report.changes, 1,
        "Integrate 中取消仍必须交付已提交的那条变化：{report:?}"
    );
    let head_after = load_head(&pair.b_vault, &pair.doc_id)
        .expect("读 B head")
        .head_seq;
    assert_eq!(head_after, head_before + 1, "事务已提交（head 前进一条）");
    let text_after = current_text(&mut cx, &host);
    assert!(
        text_after.contains("integrate 已提交的文字"),
        "已 durable 的变化必须交付进 live（不能吞掉）：{text_after:?}"
    );
    assert!(
        text_after.starts_with(text_before.trim_end()),
        "交付必须基于原正文追加，不能重开/重置：{text_after:?}"
    );

    let _ticket = service.request_stop();
    assert!(service.reap_worker());
}

// ---------------------------------------------------------------------------
// 7) ADR 0012 S2b：结构→正文依赖在 worker 的 Integrate 内收敛（不再补一轮）
// ---------------------------------------------------------------------------

/// 计数两阶段 runner：包一层真实 [`DevLocalRunner`]，数 Prepare / NeedGate 次数。
///
/// 用途：断言"一次点击只 Prepare 一次、只 NeedGate 一次"——UI 不再因为依赖顺序
/// 发第二轮网络同步。
struct CountingTwoPhaseRunner {
    inner: DevLocalRunner,
    prepares: Arc<std::sync::atomic::AtomicUsize>,
    need_gates: Arc<std::sync::atomic::AtomicUsize>,
}

impl SyncRunner for CountingTwoPhaseRunner {
    fn run(&mut self, op: Command, events: &mut dyn FnMut(Event)) -> Result<OpOutcome, String> {
        self.inner.run(op, events)
    }

    fn run_gated(
        &mut self,
        op: Command,
        gate: Option<&HeldGate>,
        events: &mut dyn FnMut(Event),
    ) -> Result<OpOutcome, String> {
        self.inner.run_gated(op, gate, events)
    }

    fn run_gated_controlled(
        &mut self,
        op: Command,
        gate: Option<&HeldGate>,
        control: &OpControl,
        events: &mut dyn FnMut(Event),
    ) -> Result<OpOutcome, String> {
        self.inner.run_gated_controlled(op, gate, control, events)
    }

    fn attach_control(&mut self, control: Receiver<SyncControl>) {
        self.inner.attach_control(control);
    }

    fn run_two_phase(
        &mut self,
        op_id: OpId,
        control: &OpControl,
        events: &mut dyn FnMut(Event),
    ) -> Result<OpOutcome, String> {
        self.prepares
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let need_gates = self.need_gates.clone();
        let mut emit = |event: Event| {
            if matches!(event, Event::NeedGate { .. }) {
                need_gates.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            events(event);
        };
        self.inner.run_two_phase(op_id, control, &mut emit)
    }
}

/// ADR 0012 S2b 用例 1：A 新建笔记 → B **一次**两阶段同步后树与正文都出现。
///
/// 关键断言（回归 S2a 的已知缺口）：
/// - 结构先行集成 → worker 内只投影建立 body 行 → 正文在同一次 Integrate 内集成
///   （summary「延后 0」），不需要第二次网络同步/点击；
/// - worker 只 Prepare 一次、只 NeedGate 一次；
/// - 侧栏第 4 行与正文都真实出现，闸门释放后打开新笔记能看到 A 的文字。
#[gpui::test]
async fn two_phase_sync_converges_structure_and_body_without_a_second_round(
    cx: &mut TestAppContext,
) {
    ensure_test_user_key();
    let pair = establish_pair("two-phase-converge");
    let relay_url = pair.relay.base_url().to_owned();

    // A：真实结构写入（同事务 sync intent）+ 正文提交，再推 relay。
    let title = "S2b 一次收敛的新笔记";
    let body = "来自 A、必须一次同步就出现的正文\n";
    let a_peer = pair.a_identity.loro_peer_id();
    let a_epoch = pair.a_identity.current_epoch;
    let new_doc = {
        let mut writer = StructureWriter::open_with_sync(&pair.a_vault, a_peer, Some(pair.a_sync))
            .expect("打开 A 的结构写入口失败");
        let seed = writer.doc().create_note(title, 1).expect("A 新建笔记失败");
        writer
            .commit("dev-sync-s2b-structure")
            .expect("A 结构提交失败");
        seed.body_doc_id
    };
    append_body(&pair.a_vault, &new_doc, a_peer, a_epoch, body);
    let a_push =
        DevLocal::sync_once(&relay_url, &pair.a_vault, &pair.a_identity).expect("A 推送失败");
    assert!(
        a_push.push.acked >= 2,
        "结构与正文都必须推到 relay：{a_push:?}"
    );

    // B：真实 worker，外面包一层计数；单操作 slot 与面板服务仍走真实路径。
    let prepares = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let need_gates = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let runner = CountingTwoPhaseRunner {
        inner: DevLocalRunner::new(relay_url.clone(), pair.b_path.clone()),
        prepares: prepares.clone(),
        need_gates: need_gates.clone(),
    };
    let service = SyncService::with_runner(ServiceConfig::new(relay_url.clone()), Box::new(runner));
    let (host, handle) = add_shell_window_with_service(
        cx,
        pair.b_vault.clone(),
        "dev-sync-two-phase-converge",
        pair.b_sync,
        service.clone(),
    );
    let app_cx = cx.clone();
    let mut cx = VisualTestContext::from_window(handle, &app_cx);
    cx.run_until_parked();

    // 同步前：B 的树里没有这篇，侧栏没有第 4 行。
    let tree_before = list_tree(&pair.b_vault).expect("读 B 树失败");
    assert!(
        tree_before.iter().all(|entry| entry.title != title),
        "同步前不该有 A 的新笔记"
    );
    assert!(
        cx.debug_bounds("tree-row-3").is_none(),
        "同步前侧栏不该有第 4 行"
    );

    start_sync(&mut cx, &host);
    drive_sync(&mut cx, &host);
    let report = last_report(&mut cx, &host).expect("必须留下报告");
    assert!(report.is_ok(), "两阶段同步必须成功：{report:?}");
    assert!(!report.no_session, "有活动文档必须走两阶段持闸路径");
    assert!(report.dispatched_under_gate, "GrantGate 之后才算持闸派发");
    assert!(
        report.structure_refreshed,
        "结构 head 变了必须刷新侧栏（不假刷新）：{report:?}"
    );
    let summary = report.worker_summary.clone().unwrap_or_default();
    assert!(
        summary.contains("延后 0"),
        "正文依赖必须在同一次 Integrate 内收敛（不需要补轮）：{summary:?}"
    );

    // 存储侧：树出现第 4 个节点，且正文文档行在同一轮里已建立并集成。
    let tree = list_tree(&pair.b_vault).expect("同步后读 B 树失败");
    let index = tree_index_of(&tree, title);
    assert_eq!(index, 3, "新笔记必须是第 4 个先序节点：{tree:?}");
    assert_eq!(
        tree[index].body_doc_id.as_deref(),
        Some(new_doc.as_str()),
        "结构投影必须建立 body 文档行"
    );

    // UI 侧：侧栏真实画出第 4 行；点开就是 A 的正文（不是空正文、也不需要再同步）。
    let row = cx
        .debug_bounds("tree-row-3")
        .expect("同步后侧栏必须画出第 4 行（结构刷新真的跑了）");
    cx.simulate_click(row.center(), Modifiers::default());
    cx.run_until_parked();
    let text = current_text(&mut cx, &host);
    assert!(
        text.contains("来自 A、必须一次同步就出现的正文"),
        "打开新笔记必须直接看到 A 的正文（同一次同步已集成）：{:?}",
        &text[..text.len().min(64)]
    );

    // 只 Prepare 一次、只 NeedGate 一次：没有任何"依赖修补补轮"。
    assert_eq!(
        prepares.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "一次点击只允许一次 Prepare"
    );
    assert_eq!(
        need_gates.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "一次点击只允许一次 NeedGate"
    );
    assert!(!active_held(&mut cx, &host), "收尾后闸门必须释放");

    let _ticket = service.request_stop();
    assert!(service.reap_worker());
}

/// ADR 0012 S2b 用例 3：非活动文档同步后**下次打开**从 durable journal 重放。
///
/// A 同时改两篇；B 的活动原文只 adopt 当前这篇，不 adopt 非活动正文。随后真实点击
/// 切到第二篇 → 新 session 从库里重放（看到 A 的新正文）；再切走/切回，旧 session
/// 不得把旧内容带回来。
#[gpui::test]
async fn inactive_documents_replay_from_the_durable_journal_after_sync(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let pair = establish_pair("inactive-replay");
    let relay_url = pair.relay.base_url().to_owned();
    let a_peer = pair.a_identity.loro_peer_id();
    let a_epoch = pair.a_identity.current_epoch;

    let tree_a = list_tree(&pair.a_vault).expect("读 A 树失败");
    let first_index = tree_a
        .iter()
        .position(|entry| entry.title == dev_vault::DEMO_TITLE && entry.is_note())
        .expect("A 树必须有第一篇");
    let first_doc = tree_a[first_index]
        .body_doc_id
        .clone()
        .expect("第一篇必须有 body doc");
    let second_index = tree_index_of(&tree_a, dev_vault::DEMO_SECOND_TITLE);
    let second_doc = tree_a[second_index]
        .body_doc_id
        .clone()
        .expect("第二篇必须有 body doc");
    append_body(
        &pair.a_vault,
        &first_doc,
        a_peer,
        a_epoch,
        "A 改了第一篇（活动）\n",
    );
    append_body(
        &pair.a_vault,
        &second_doc,
        a_peer,
        a_epoch,
        "A 改了第二篇（非活动）\n",
    );
    DevLocal::sync_once(&relay_url, &pair.a_vault, &pair.a_identity).expect("A 推送两篇失败");

    let (host, handle) = add_shell_window(
        cx,
        pair.b_vault.clone(),
        "dev-sync-inactive-replay",
        config_for(&relay_url, &pair.b_path),
        pair.b_sync,
    );
    let app_cx = cx.clone();
    let mut cx = VisualTestContext::from_window(handle, &app_cx);
    cx.run_until_parked();

    // 活动文档 = 第一篇；同步前第二篇在库里还是旧正文。
    let view = active_view(&mut cx, &host).expect("B 必须有活动视图");
    assert_eq!(
        read_shell(&mut cx, &host, |_shell, app| view
            .read(app)
            .dev_sync_doc_id()
            .to_owned()),
        first_doc,
        "B 的活动文档必须是先序第一篇"
    );
    let second_before = load_body_doc(&pair.b_vault, &second_doc, 1, None)
        .expect("读 B 第二篇失败")
        .0
        .text();
    assert!(
        !second_before.contains("A 改了第二篇"),
        "同步前第二篇不该有远端正文：{second_before:?}"
    );

    start_sync(&mut cx, &host);
    drive_sync(&mut cx, &host);
    let report = last_report(&mut cx, &host).expect("必须留下报告");
    assert!(report.is_ok(), "两阶段同步必须成功：{report:?}");
    assert!(
        !report.structure_refreshed,
        "只有正文变化时不该重投影结构（不假刷新）：{report:?}"
    );
    let text_first = current_text(&mut cx, &host);
    assert!(
        text_first.contains("A 改了第一篇（活动）"),
        "活动文档必须 adopt 远端正文：{text_first:?}"
    );

    // 真实点击切到第二篇：新 session 从 durable journal 重放，看到 A 的正文。
    let second_row = cx.debug_bounds("tree-row-2").expect("第二篇行必须存在");
    cx.simulate_click(second_row.center(), Modifiers::default());
    cx.run_until_parked();
    let text_second = current_text(&mut cx, &host);
    assert!(
        text_second.contains("A 改了第二篇（非活动）"),
        "非活动文档下次打开必须从 durable journal 重放：{text_second:?}"
    );

    // 切走再切回：旧 session 不得返回旧内容。
    let first_row = cx.debug_bounds("tree-row-0").expect("第一篇行必须存在");
    cx.simulate_click(first_row.center(), Modifiers::default());
    cx.run_until_parked();
    let second_row = cx.debug_bounds("tree-row-2").expect("第二篇行必须存在");
    cx.simulate_click(second_row.center(), Modifiers::default());
    cx.run_until_parked();
    let text_again = current_text(&mut cx, &host);
    assert!(
        text_again.contains("A 改了第二篇（非活动）"),
        "切换后旧 session 不能返回旧内容：{text_again:?}"
    );
    assert!(
        second_before.is_empty() || text_again.contains(second_before.trim_end()),
        "重放必须保留原有正文：{text_again:?}"
    );
}

/// 受控两阶段 runner：集成阶段提交一条真实正文（独立连接），UI 交付之后**收尾失败**。
///
/// 用途（ADR 0012 S2b）：部分集成成功后即使 Finish（push tail/ACK）失败，也必须先把
/// 已 durable 内容交付进 live，并且报告如实为失败（不谎报成功）。
struct FinishFailsRunner {
    control: Option<Receiver<SyncControl>>,
    commit: CommitFixture,
}

impl SyncRunner for FinishFailsRunner {
    fn run(&mut self, _op: Command, _events: &mut dyn FnMut(Event)) -> Result<OpOutcome, String> {
        Err("Finish 失败 runner 只支持两阶段同步".to_owned())
    }

    fn attach_control(&mut self, control: Receiver<SyncControl>) {
        self.control = Some(control);
    }

    fn run_two_phase(
        &mut self,
        op_id: OpId,
        _control: &OpControl,
        events: &mut dyn FnMut(Event),
    ) -> Result<OpOutcome, String> {
        events(Event::NeedGate {
            op_id,
            authorization_epoch: 7,
            stored: 0,
            deferred: 0,
            summary: "受控 Prepare（不持闸）".to_owned(),
        });
        let receiver = self.control.as_ref().ok_or("缺少控制通道")?;
        loop {
            match receiver.recv().map_err(|_| "控制通道已关闭")? {
                SyncControl::GrantGate { op_id: id, .. } if id == op_id => break,
                SyncControl::Stop => return Err("worker 已停止".to_owned()),
                _ => continue,
            }
        }
        // 模拟 Integrate 的真实事务已提交（独立连接）。
        let vault = Vault::open(&self.commit.vault_path).expect("runner 打开库失败");
        append_body(
            &vault,
            &self.commit.doc_id,
            self.commit.peer,
            self.commit.epoch,
            &self.commit.text,
        );
        events(Event::Integrated {
            op_id,
            changed_heads: vec![self.commit.doc_id.clone()],
            integrated: 1,
            deferred: 0,
            summary: "受控 Integrate（已提交）".to_owned(),
        });
        loop {
            match receiver.recv().map_err(|_| "控制通道已关闭")? {
                SyncControl::Finish { op_id: id } if id == op_id => break,
                SyncControl::Stop => return Err("worker 已停止".to_owned()),
                _ => continue,
            }
        }
        Err("同步失败（Finish）：注入的 push tail/ACK 失败".to_owned())
    }
}

/// ADR 0012 S2b 用例 2c：部分集成成功 + 收尾（ACK/push tail）失败 → **先交付**已 durable
/// 内容，报告如实为失败，不谎报成功。
#[gpui::test]
async fn finish_failure_after_integration_delivers_durable_content_first(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let pair = establish_pair("finish-fails");
    let relay_url = pair.relay.base_url().to_owned();
    let doc_id = pair.doc_id.clone();
    let runner = FinishFailsRunner {
        control: None,
        commit: CommitFixture {
            vault_path: pair.b_path.clone(),
            doc_id: doc_id.clone(),
            peer: pair.b_sync.peer_id,
            epoch: pair.b_sync.key_epoch,
            text: "集成已提交、收尾失败的正文\n".to_owned(),
        },
    };
    let service = SyncService::with_runner(ServiceConfig::new(relay_url), Box::new(runner));
    let (host, handle) = add_shell_window_with_service(
        cx,
        pair.b_vault.clone(),
        "dev-sync-finish-fails",
        pair.b_sync,
        service.clone(),
    );
    let app_cx = cx.clone();
    let mut cx = VisualTestContext::from_window(handle, &app_cx);
    cx.run_until_parked();
    let head_before = load_head(&pair.b_vault, &doc_id)
        .expect("读 B head 失败")
        .head_seq;

    start_sync(&mut cx, &host);
    drive_sync(&mut cx, &host);
    let report = last_report(&mut cx, &host).expect("必须留下报告");
    assert!(
        report
            .error
            .as_deref()
            .is_some_and(|error| error.contains("注入的 push tail/ACK 失败")),
        "收尾失败必须如实呈现（不能谎报成功）：{report:?}"
    );
    assert_eq!(
        report.changes, 1,
        "ACK 失败前必须先交付已 durable 的集成内容：{report:?}"
    );
    let head_after = load_head(&pair.b_vault, &doc_id)
        .expect("读 B head 失败")
        .head_seq;
    assert_eq!(
        head_after,
        head_before + 1,
        "集成事务已提交（head 前进一条）"
    );
    let text = current_text(&mut cx, &host);
    assert!(
        text.contains("集成已提交、收尾失败的正文"),
        "已 durable 的内容必须先交付进 live（不能丢）：{text:?}"
    );
    assert!(!active_held(&mut cx, &host), "交付完成后闸门必须释放");
    assert_eq!(service.snapshot().in_flight, None, "收尾后不得占着操作位");

    let _ticket = service.request_stop();
    assert!(service.reap_worker());
}
