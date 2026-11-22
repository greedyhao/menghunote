//! ADR 0009 切片 3b：外部提交闸门 + `adopt_committed_delta` 接到 **EditorView / AppShell**。
//!
//! 这里验的是"UI 真的变了"，不是存储层语义（3a）：
//!
//! 1. **EditorView**：adopt 之后不重开文档，镜像正文就含远端文字；Held 期间键入无效
//!    （`editor-sync-held` selector 存在、文本一点没变）；`end` 之后键入立刻生效。
//! 2. **AppShell**（真实 AppShell + 临时 dev-sync 库）：对**当前活动文档** begin → poll →
//!    adopt → pump，编辑器文本变化、`EditorView` **实体没变**（没有切换/重开）；
//!    再键入并走生产关窗排空 → 新 update 落库（没有 `HeadConflict`）。
//! 3. **失败路径**：过期租约 / 篡改 payload 都失败且**文本不变、闸门仍 Held**；
//!    `cancel` 先把已 durable 的外部行交付进 live（不丢数据）再释放，之后键入恢复。
//!
//! 纪律（AGENTS.md §6）：临时目录在 `CARGO_TARGET_TMPDIR` 下；**不用 sleep** —— 收敛点只有
//! `run_until_parked()` 与闸门/durable 的明确状态（`yield_now` 只让出 CPU）。

#![cfg(feature = "dev-sync")]

use std::path::Path;
use std::time::{Duration, Instant};

use gpui::{
    div, App, AppContext as _, Context, Entity, InteractiveElement as _, Modifiers,
    ParentElement as _, Render, Styled as _, TestAppContext, VisualTestContext,
};
use menghunote_app::dev_sync::gate::DevSyncConfig;
use menghunote_app::dev_sync::service::ServiceConfig;
use menghunote_app::dev_sync::DevIdentityStore;
use menghunote_app::dev_vault;
use menghunote_app::editor::view::EditorView;
use menghunote_app::shell::AppShell;
use menghunote_editor::EditorController;
use menghunote_storage::session::{
    prepare_committed_delta, AdoptError, AppliedHeads, DriverOptions, ExternalCommitLease,
    ExternalCommitTicket, GateState, LiveRev, SessionDriver,
};
use menghunote_storage::{
    append_updates, create_body_doc, list_tree, load_body_doc, load_head, SyncContext,
    UpdateRecord, Vault,
};
use tempfile::{Builder as TempDirBuilder, TempDir};

/// EditorView 级测试的本机 peer（与外部 writer 区分）。
const LOCAL_PEER: u64 = 0x5EED_0003;
/// 播种正文用的 peer。
const SEED_PEER: u64 = 1;
/// 模拟"另一个 writer"的 Loro peer（必须与本地 peer 区分开）。
const PEER_EXTERNAL: u64 = 3;
/// 没有外层 shell 的 EditorView 测试正文（多行，保证点击能落在文字上）。
const EDITOR_SEED: &str = "第一行\n第二行\n第三行\n第四行\n第五行\n第六行\n";

/// 测试进程级的 Linux 用户主键（Windows 是 no-op），与 `dev_sync_intents.rs` 同构。
fn ensure_test_user_key() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("dev-sync-adoption-xdg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("创建测试 XDG_DATA_HOME 失败");
        std::env::set_var("XDG_DATA_HOME", &dir);
        menghunote_sync::identity_wrap::initialize_user_key()
            .expect("初始化测试用户主键失败（ADR 0008）");
    });
}

/// 在 `CARGO_TARGET_TMPDIR`（`target/tmp`）下开一个独立临时目录。
fn temp_root(prefix: &str) -> TempDir {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("dev-sync-adoption-{prefix}-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("创建 target/tmp 下的测试根目录失败");
    TempDirBuilder::new()
        .prefix("vault-")
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

/// 建 `loro_docs` 行并把种子正文落库（模拟"文档一开始就有内容"）。
fn seed_body_doc(vault: &Vault, doc_id: &str, text: &str) {
    create_body_doc(vault, doc_id).expect("建 loro_docs 行");
    let (doc, head) = load_body_doc(vault, doc_id, SEED_PEER, None).expect("加载正文");
    doc.insert_scalars(0, text).expect("写入种子正文");
    let base = head.version_vector().expect("解码 head_vv");
    let update = doc.pending_update(&base).expect("种子增量");
    append_updates(
        vault,
        doc_id,
        head.head_seq,
        &[UpdateRecord::new("seed", update)],
    )
    .expect("落库种子");
}

/// 模拟 `engine.integrate`：从库中当前 head 重放一份独立副本 → 改 → 一个 A 事务追加。
fn external_append(vault: &Vault, doc_id: &str, at: usize, text: &str) -> u64 {
    let current = load_head(vault, doc_id).expect("库中 head");
    let base = current.version_vector().expect("解码 head_vv");
    let (doc, _) = load_body_doc(vault, doc_id, PEER_EXTERNAL, None).expect("重放外部副本");
    doc.insert_scalars(at, text).expect("外部编辑");
    let update = doc.pending_update(&base).expect("外部增量");
    append_updates(
        vault,
        doc_id,
        current.head_seq,
        &[UpdateRecord::new("engine-integrate", update)],
    )
    .expect("外部 A 事务")
    .head_seq
}

/// 把闸门驱到 `Held`（EditorView 级）：只等**明确状态**，时间只是安全上限。
fn drive_to_held_view(
    view: &Entity<EditorView>,
    cx: &mut VisualTestContext,
    ticket: &ExternalCommitTicket,
) -> ExternalCommitLease {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match view.update(cx, |view, cx| view.dev_sync_poll_external(ticket, cx)) {
            GateState::Held(lease) => return lease,
            GateState::Flushing => std::thread::yield_now(),
            GateState::WaitingIME => panic!("没有 composition 时不应 WaitingIME"),
            other => panic!("闸门未收敛到 Held：{other:?}"),
        }
        assert!(Instant::now() < deadline, "闸门 10s 内没有进入 Held");
    }
}

/// 把闸门驱到 `Held`（AppShell 级）。
fn drive_to_held_shell(
    cx: &mut VisualTestContext,
    host: &Entity<ShellHost>,
    ticket: &ExternalCommitTicket,
) -> ExternalCommitLease {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match with_shell(cx, host, |shell, cx| {
            shell.dev_sync_poll_external(ticket, cx)
        }) {
            GateState::Held(lease) => return lease,
            GateState::Flushing => std::thread::yield_now(),
            GateState::WaitingIME => panic!("没有 composition 时不应 WaitingIME"),
            other => panic!("闸门未收敛到 Held：{other:?}"),
        };
        assert!(Instant::now() < deadline, "闸门 10s 内没有进入 Held");
    }
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

/// 读 `Entity`（`Entity::read` 要 `&App`，测试里只能通过 `update` 拿到）。
fn read_view<R>(
    cx: &mut VisualTestContext,
    view: &Entity<EditorView>,
    f: impl FnOnce(&EditorView) -> R,
) -> R {
    let view = view.clone();
    cx.update(|_window, app| f(view.read(app)))
}

fn read_shell<R>(
    cx: &mut VisualTestContext,
    host: &Entity<ShellHost>,
    f: impl FnOnce(&AppShell, &App) -> R,
) -> R {
    let host = host.clone();
    cx.update(|_window, app| f(host.read(app).shell.read(app), app))
}

/// 当前活动文档的镜像正文（断言主入口）。
fn current_text(cx: &mut VisualTestContext, host: &Entity<ShellHost>) -> String {
    read_shell(cx, host, |shell, app| shell.dev_sync_current_text(app))
        .expect("当前活动文档的镜像正文")
}

// ---------------------------------------------------------------------------
// 1) EditorView：adopt 立刻可见 + Held 冻结输入 + 恢复后键入生效
// ---------------------------------------------------------------------------

#[gpui::test]
async fn editor_view_applies_adopted_delta_and_freezes_typing_while_held(cx: &mut TestAppContext) {
    let dir = temp_root("editor-view");
    let vault_path = dir.path().join("vault.sqlite3");
    let vault = Vault::open(&vault_path).expect("打开临时 vault");
    seed_body_doc(&vault, "doc-1", EDITOR_SEED);
    let writer = vault.clone();

    let mut options = DriverOptions::default();
    options.session.peer_id = LOCAL_PEER;
    let driver = SessionDriver::open(&vault, "doc-1", options).expect("打开驱动");
    let controller = EditorController::open(driver.session().snapshot()).expect("初始化控制器");
    let (view, cx) = cx.add_window_view(move |_, cx| {
        EditorView::new(controller, driver, "dev-sync-adoption".to_owned(), cx)
    });
    cx.run_until_parked();

    // 与真机一致：点正文取得焦点（点在首行上，避免落到空白区）。
    let pane = cx
        .debug_bounds("editor-current-doc")
        .expect("编辑器根 selector 必须画出");
    let first_line = gpui::point(
        pane.origin.x + gpui::px(20.0),
        pane.origin.y + gpui::px(10.0),
    );
    cx.simulate_click(first_line, Modifiers::default());
    cx.run_until_parked();
    assert_eq!(
        read_view(cx, &view, |view| view.dev_sync_current_text()),
        EDITOR_SEED
    );

    // 1) begin → Held（没有本地脏数据：poll 直接 Held）
    let ticket = view
        .update(cx, |view, cx| view.dev_sync_begin_external(cx))
        .expect("begin");
    let lease = drive_to_held_view(&view, cx, &ticket);
    assert!(
        read_view(cx, &view, |view| view.dev_sync_is_held()),
        "必须进入 Held"
    );
    // 生产路径靠 50 ms 的 pump 发现状态变化；测试里显式泵一次（它负责 notify/重绘）。
    view.update(cx, |view, cx| view.dev_sync_pump(cx));
    cx.run_until_parked();
    let held = cx
        .debug_bounds("editor-sync-held")
        .expect("Held 时必须画出 selector=editor-sync-held");
    assert!(
        held.size.width.as_f32() > 0.0 && held.size.height.as_f32() > 0.0,
        "Held 标记必须有非零尺寸：{held:?}"
    );

    // 2) Held 期间键入无效（一次都不能落到驱动）
    cx.simulate_keystrokes("b");
    cx.run_until_parked();
    assert_eq!(
        read_view(cx, &view, |view| view.dev_sync_current_text()),
        EDITOR_SEED,
        "Held 期间键入不得改动文本"
    );

    // 3) 外部 writer 追加 → prepare → adopt → pump：不重开文档就能看到远端文字
    let selection_before = read_view(cx, &view, |view| view.selection());
    let target_head_seq = external_append(&writer, "doc-1", EDITOR_SEED.chars().count(), "远端");
    let prepared = prepare_committed_delta(&writer, "doc-1", lease.durable_head_seq())
        .expect("prepare 外部增量");
    assert_eq!(prepared.target_head_seq, target_head_seq);
    let outcome = view
        .update(cx, |view, cx| {
            view.dev_sync_adopt_external(&lease, prepared, cx)
        })
        .expect("adopt");
    assert_eq!(outcome.delta.to_rev, LiveRev(1));
    assert!(
        view.update(cx, |view, cx| view.dev_sync_pump(cx)),
        "pump 必须报告可视变化"
    );
    cx.run_until_parked();
    assert_eq!(
        read_view(cx, &view, |view| view.dev_sync_current_text()),
        format!("{EDITOR_SEED}远端"),
        "adopt 之后当前镜像必须包含远端文字（没有重开文档）"
    );
    // 远端插在光标之后：选区/光标必须原样保留（3a 的 live 变换 + 控制器的 splice 变换）。
    assert_eq!(
        read_view(cx, &view, |view| view.selection()),
        selection_before,
        "adopt 不得重置选区/光标"
    );

    // 4) end → 释放 → 键入恢复生效
    let heads = AppliedHeads::load(&writer, "doc-1").expect("读库中 heads");
    view.update(cx, |view, cx| view.dev_sync_end_external(lease, heads, cx))
        .expect("end");
    assert!(
        !read_view(cx, &view, |view| view.dev_sync_is_held()),
        "end 之后必须退出 Held"
    );
    view.update(cx, |view, cx| view.dev_sync_pump(cx));
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("editor-sync-held").is_none(),
        "Held 结束后 selector 必须消失"
    );
    cx.simulate_keystrokes("c");
    cx.run_until_parked();
    let after_typing = read_view(cx, &view, |view| view.dev_sync_current_text());
    assert!(
        after_typing.contains('c'),
        "恢复后键入必须生效：{after_typing:?}"
    );
}

// ---------------------------------------------------------------------------
// 2) AppShell：真实 shell + dev-sync 库，adopt 后不重开即可见
// ---------------------------------------------------------------------------

#[gpui::test]
async fn app_shell_adopts_external_commit_without_reopening(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let dir = temp_root("app-shell");
    let vault_path = dir.path().join("vault.sqlite3");
    let identity = DevIdentityStore::load_or_create(&vault_path).expect("dev identity");
    let peer = identity.loro_peer_id();
    let sync = SyncContext::new(peer, identity.current_epoch);
    let dev = dev_vault::open_vault_at_with_peer(&vault_path, peer).expect("打开实例库");
    assert_ne!(peer, PEER_EXTERNAL, "外部 writer 必须用不同 peer");

    let doc_id = list_tree(&dev.vault)
        .expect("读结构树")
        .first()
        .and_then(|entry| entry.body_doc_id.clone())
        .expect("第一篇笔记必须有 body_doc_id");
    let writer = dev.vault.clone();
    let vault = dev.vault.clone();
    let config = DevSyncConfig {
        relay_mode: menghunote_app::onboarding::RelayMode::LocalDev,
        vault: vault_path.clone(),
        service: ServiceConfig::new("http://127.0.0.1:18080"),
    };
    let (host, cx) = cx.add_window_view(move |_, cx| {
        let shell = cx.new(|cx| {
            AppShell::new_with_dev_sync(vault, "dev-sync-adoption".to_owned(), config, sync, cx)
        });
        ShellHost { shell }
    });
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("dev-sync-panel").is_some(),
        "dev-sync 必须真的启用（面板已挂载）"
    );

    // 活动文档 = 打开的第一篇（AppShell::new 的既有行为），点正文聚焦。
    let pane = cx.debug_bounds("editor-pane").expect("编辑器面板 selector");
    let first_line = gpui::point(
        pane.origin.x + gpui::px(20.0),
        pane.origin.y + gpui::px(10.0),
    );
    cx.simulate_click(first_line, Modifiers::default());
    cx.run_until_parked();
    let text_before = current_text(cx, &host);
    assert!(
        text_before.contains(dev_vault::DEMO_TITLE),
        "活动文档必须是播种的第一篇演示笔记"
    );
    let view_before =
        read_shell(cx, &host, |shell, _app| shell.dev_sync_active_view()).expect("当前活动视图");

    // 1) begin → Held
    let ticket = with_shell(cx, &host, |shell, cx| shell.dev_sync_begin_external(cx))
        .expect("begin 必须成功");
    let lease = drive_to_held_shell(cx, &host, &ticket);
    // 生产路径靠编辑器自己的 50 ms pump 发现状态变化；测试里显式泵一次。
    with_shell(cx, &host, |shell, cx| shell.dev_sync_pump(cx));
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("editor-sync-held").is_some(),
        "Held 时活动编辑器必须画出 editor-sync-held"
    );

    // 2) 外部 writer 追加同一个 doc → prepare → adopt → pump
    external_append(&writer, &doc_id, 0, "外部提交：");
    let prepared = prepare_committed_delta(&writer, &doc_id, lease.durable_head_seq())
        .expect("prepare 外部增量");
    let outcome = with_shell(cx, &host, |shell, cx| {
        shell.dev_sync_adopt_external(&lease, prepared, cx)
    })
    .expect("adopt 必须成功");
    assert_eq!(outcome.ack.head_seq, lease.durable_head_seq() + 1);
    assert!(
        with_shell(cx, &host, |shell, cx| shell.dev_sync_pump(cx)),
        "pump 必须把 Remote/Durable 推进编辑器"
    );
    cx.run_until_parked();

    // 3) 断言：文本变了、**实体没变**（没有切换/重开）
    let text_after = current_text(cx, &host);
    assert!(
        text_after.starts_with("外部提交："),
        "adopt 之后当前编辑器文本必须含远端写入（前缀）：{:?}",
        &text_after[..text_after.len().min(24)]
    );
    assert!(text_after.len() > text_before.len());
    let view_after =
        read_shell(cx, &host, |shell, _app| shell.dev_sync_active_view()).expect("当前活动视图");
    assert!(
        view_before == view_after,
        "adopt 不能重建/切换 EditorView（必须是同一个实体）"
    );

    // 4) end 之后本地继续键入并走生产关窗排空：必须真的落库（没有 HeadConflict）
    let heads = AppliedHeads::load(&writer, &doc_id).expect("读库中 heads");
    with_shell(cx, &host, |shell, cx| {
        shell.dev_sync_end_external(lease, heads, cx)
    })
    .expect("end");
    with_shell(cx, &host, |shell, cx| shell.dev_sync_pump(cx));
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("editor-sync-held").is_none(),
        "end 之后 selector 必须消失"
    );
    let head_before_typing = load_head(&writer, &doc_id).expect("head").head_seq;
    cx.simulate_keystrokes("Z");
    cx.run_until_parked();
    with_shell(cx, &host, |shell, cx| shell.close_all(cx));
    cx.run_until_parked();

    let head_after_typing = load_head(&writer, &doc_id).expect("head").head_seq;
    assert_eq!(
        head_after_typing,
        head_before_typing + 1,
        "本地键入必须落成一条新 update（HeadConflict 会让 head 停在 {head_before_typing}）"
    );
    let (persisted, _) = load_body_doc(&writer, &doc_id, 0, None).expect("重放落库正文");
    let persisted_text = persisted.text();
    assert!(
        persisted_text.contains('Z'),
        "关窗排空之后落库正文必须含本地键入：{:?}",
        &persisted_text[..persisted_text.len().min(32)]
    );
    assert!(
        persisted_text.contains("外部提交："),
        "落库正文必须同时保留已 adopt 的远端写入：{:?}",
        &persisted_text[..persisted_text.len().min(32)]
    );
}

// ---------------------------------------------------------------------------
// 3) 失败路径：过期租约 / 篡改 payload 不动 UI；cancel 交付后恢复
// ---------------------------------------------------------------------------

#[gpui::test]
async fn adopt_failures_keep_text_and_held_then_cancel_releases(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let dir = temp_root("failures");
    let vault_path = dir.path().join("vault.sqlite3");
    let identity = DevIdentityStore::load_or_create(&vault_path).expect("dev identity");
    let peer = identity.loro_peer_id();
    let sync = SyncContext::new(peer, identity.current_epoch);
    let dev = dev_vault::open_vault_at_with_peer(&vault_path, peer).expect("打开实例库");
    let doc_id = list_tree(&dev.vault)
        .expect("读结构树")
        .first()
        .and_then(|entry| entry.body_doc_id.clone())
        .expect("第一篇笔记必须有 body_doc_id");
    let writer = dev.vault.clone();
    let vault = dev.vault.clone();
    let config = DevSyncConfig {
        relay_mode: menghunote_app::onboarding::RelayMode::LocalDev,
        vault: vault_path.clone(),
        service: ServiceConfig::new("http://127.0.0.1:18080"),
    };
    let (host, cx) = cx.add_window_view(move |_, cx| {
        let shell = cx.new(|cx| {
            AppShell::new_with_dev_sync(vault, "dev-sync-adoption".to_owned(), config, sync, cx)
        });
        ShellHost { shell }
    });
    cx.run_until_parked();
    let pane = cx.debug_bounds("editor-pane").expect("编辑器面板 selector");
    let first_line = gpui::point(
        pane.origin.x + gpui::px(20.0),
        pane.origin.y + gpui::px(10.0),
    );
    cx.simulate_click(first_line, Modifiers::default());
    cx.run_until_parked();

    // 第一代：正常 begin/end，留下一个**过期租约**。
    let first_ticket =
        with_shell(cx, &host, |shell, cx| shell.dev_sync_begin_external(cx)).expect("begin#1");
    let stale_lease = drive_to_held_shell(cx, &host, &first_ticket);
    let heads = AppliedHeads::load(&writer, &doc_id).expect("读库中 heads");
    with_shell(cx, &host, |shell, cx| {
        shell.dev_sync_end_external(stale_lease.clone(), heads, cx)
    })
    .expect("end#1");

    // 第二代：Held 期间外部提交一条，然后逐项构造非法 adopt。
    let ticket =
        with_shell(cx, &host, |shell, cx| shell.dev_sync_begin_external(cx)).expect("begin#2");
    let lease = drive_to_held_shell(cx, &host, &ticket);
    external_append(&writer, &doc_id, 0, "外部行：");
    let prepared = prepare_committed_delta(&writer, &doc_id, lease.durable_head_seq())
        .expect("prepare 外部增量");
    let text_before = current_text(cx, &host);

    // a) 过期租约 → LeaseMismatch，文本不变、闸门仍 Held
    match with_shell(cx, &host, |shell, cx| {
        shell.dev_sync_adopt_external(&stale_lease, prepared.clone(), cx)
    }) {
        Err(AdoptError::LeaseMismatch { .. }) => {}
        other => panic!("过期租约必须被拒，实际 {other:?}"),
    }
    with_shell(cx, &host, |shell, cx| shell.dev_sync_pump(cx));
    cx.run_until_parked();
    assert_eq!(
        current_text(cx, &host),
        text_before,
        "adopt 失败不得改动 UI 文本"
    );
    assert!(
        cx.debug_bounds("editor-sync-held").is_some(),
        "adopt 失败后闸门必须仍 Held"
    );

    // b) payload 被篡改（不是库里那一份）→ PayloadMismatch，状态同样不变
    let mut tampered = prepared.clone();
    tampered.updates[0] = vec![0u8; 8];
    match with_shell(cx, &host, |shell, cx| {
        shell.dev_sync_adopt_external(&lease, tampered, cx)
    }) {
        Err(AdoptError::PayloadMismatch { .. }) => {}
        other => panic!("篡改 payload 必须被拒，实际 {other:?}"),
    }
    assert_eq!(current_text(cx, &host), text_before);
    assert!(cx.debug_bounds("editor-sync-held").is_some());

    // c) cancel：先把已 durable 的外部行交付进 live（不丢数据）再释放闸门
    with_shell(cx, &host, |shell, cx| {
        shell.dev_sync_cancel_external(&ticket, cx)
    })
    .expect("cancel");
    with_shell(cx, &host, |shell, cx| shell.dev_sync_pump(cx));
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("editor-sync-held").is_none(),
        "cancel 之后 selector 必须消失"
    );
    let text_after = current_text(cx, &host);
    assert!(
        text_after.starts_with("外部行："),
        "cancel 必须先交付已 durable 的增量（不丢数据）：{:?}",
        &text_after[..text_after.len().min(24)]
    );

    // d) 恢复后键入生效
    cx.simulate_keystrokes("q");
    cx.run_until_parked();
    assert!(
        current_text(cx, &host).contains('q'),
        "闸门释放后键入必须生效"
    );
}
