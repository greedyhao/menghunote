//! ADR 0014 **S1**：dev-only 设备面板的「邀请设备…」与「批准 pending 设备」headless 测试。
//!
//! 覆盖：
//! 1. 创建端生成邀请链接：`dev-sync-invite` → 链接出现（`dev-sync-invite-link`/`-ttl`/
//!    `dev-sync-copy-invite`）→ 链接是 `mn-pair://join?...`、**不含恢复码**、`Debug` 不含 token；
//! 2. B 用链接兑换 PENDING（库级入口，真实 relay）→ A 面板出现 pending 行与
//!    `dev-sync-approve-<短id>` → 点击进入确认区（显示 device_id + sign/hpke 短 hash）→
//!    确认 → B 变 Active → B 检查/bootstrap 后能同步到 A 的笔记；
//! 3. 非管理设备（Join）没有批准入口；邀请按钮被明确置灰并给出原因；
//! 4. 没有活动文档时邀请按钮也置灰（ADR 0014 S1 的前置条件）。
//!
//! ⚠️ **非生产**：`menghunote-dev-relay` 的 hook 对任何签名一律放行；本文件验证面板/worker
//! 与库级邀请流程的接线，不是 relay 鉴权验收。
//!
//! feature 关闭时整个文件编译为空（0 个测试）。

#![cfg(feature = "dev-sync")]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use gpui::{
    div, AppContext as _, Entity, InteractiveElement as _, Modifiers, ParentElement as _, Render,
    Styled as _, TestAppContext, VisualTestContext,
};
use menghunote_app::dev_sync::gate::DevSyncConfig;
use menghunote_app::dev_sync::service::{DevLocalRunner, ServiceConfig, SyncRunner as _};
use menghunote_app::dev_sync::setup::{self, SetupMode, SetupSecrets};
use menghunote_app::dev_sync::state::Command;
use menghunote_app::dev_sync::DevSyncPanel;
use menghunote_app::dev_vault;
use menghunote_app::shell::AppShell;
use menghunote_dev_relay::DevRelay;
use menghunote_storage::SyncContext;
use menghunote_sync::dev_local::DevLocal;
use menghunote_sync::http_transport::DEFAULT_TRANSPORT_TIMEOUTS;
use menghunote_sync::invite_link::{InviteCheck, InviteFinish, InviteJoin, InviteLinkTarget};
use menghunote_sync::relay_context::RelayContext;
use tempfile::{Builder as TempDirBuilder, TempDir};

const DEADLINE: Duration = Duration::from_secs(30);

fn ensure_test_user_key() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("dev-sync-invite-xdg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("创建测试 XDG_DATA_HOME 失败");
        std::env::set_var("XDG_DATA_HOME", &dir);
        menghunote_sync::identity_wrap::initialize_user_key()
            .expect("初始化测试用户主键失败（ADR 0008）");
    });
}

fn temp_root(prefix: &str) -> TempDir {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("dev-sync-invite-{prefix}-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("创建 target/tmp 下的测试根目录失败");
    TempDirBuilder::new()
        .prefix("case-")
        .tempdir_in(&root)
        .expect("在 target/tmp 下创建临时目录失败")
}

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

/// 创建 A（真实 Create + 演示树 + 发布基线），返回身份/库/路径。
struct Creator {
    relay: DevRelay,
    dir: TempDir,
    path: PathBuf,
    vault: menghunote_storage::Vault,
    identity: menghunote_sync::identity::LocalIdentityV1,
    recovery_code: String,
    note_doc: String,
}

fn creator(prefix: &str) -> Creator {
    ensure_test_user_key();
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let dir = temp_root(prefix);
    let path = dir.path().join("inst-a").join("vault.sqlite3");
    let created = setup::prepare(
        &config_for(relay.base_url(), &path),
        SetupMode::Create,
        SetupSecrets {
            setup_secret: Some(relay.setup_secret().to_owned()),
            ..SetupSecrets::default()
        },
    )
    .expect("A Create 失败");
    let peer = created.identity.loro_peer_id();
    let recovery_code = created
        .recovery_code
        .clone()
        .expect("Create 必须返回恢复码");
    drop(created.vault);
    let dev = dev_vault::open_vault_at_with_peer(&path, peer).expect("A 打开实例库失败");
    DevLocal::publish_initial_pair(
        relay.base_url(),
        &dev.vault,
        &created.identity,
        &recovery_code,
    )
    .expect("A 发布初始基线失败");
    // 演示树里的第一篇正文（B bootstrap 后必须能看到）。
    let note_doc = menghunote_storage::list_tree(&dev.vault)
        .expect("读结构树失败")
        .into_iter()
        .find_map(|entry| entry.body_doc_id)
        .expect("演示树必须有正文");
    Creator {
        relay,
        dir,
        path,
        vault: dev.vault.clone(),
        identity: created.identity,
        recovery_code,
        note_doc,
    }
}

fn add_shell_window(
    cx: &mut TestAppContext,
    vault: menghunote_storage::Vault,
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
    f: impl FnOnce(&mut AppShell, &mut gpui::Context<AppShell>) -> R,
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

fn panel_entity(cx: &mut VisualTestContext, host: &Entity<ShellHost>) -> Entity<DevSyncPanel> {
    read_shell(cx, host, |shell, _app| shell.dev_sync_panel()).expect("面板必须挂载")
}

fn read_panel<R>(
    cx: &mut VisualTestContext,
    host: &Entity<ShellHost>,
    f: impl FnOnce(&DevSyncPanel) -> R,
) -> R {
    let panel = panel_entity(cx, host);
    cx.update(|_window, app| f(panel.read(app)))
}

fn probe(selector: &str) -> &'static str {
    Box::leak(selector.to_owned().into_boxed_str())
}

fn click_selector(cx: &mut VisualTestContext, selector: &str) {
    let selector = probe(selector);
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("必须画出 selector={selector}"));
    cx.simulate_click(bounds.center(), Modifiers::default());
    cx.run_until_parked();
}

/// 泵面板直到条件成立（显式推进测试时钟 + 宿主泵，不用 sleep）。
fn drive(
    cx: &mut VisualTestContext,
    host: &Entity<ShellHost>,
    description: &str,
    expect: impl Fn(&DevSyncPanel) -> bool,
) {
    let deadline = Instant::now() + DEADLINE;
    loop {
        cx.executor()
            .advance_clock(menghunote_app::dev_sync::SYNC_PUMP_INTERVAL);
        with_shell(cx, host, |shell, cx| shell.dev_sync_pump(cx));
        cx.run_until_parked();
        if read_panel(cx, host, &expect) {
            return;
        }
        assert!(Instant::now() < deadline, "30s 内没有收敛（{description}）");
        std::thread::yield_now();
    }
}

// ---------------------------------------------------------------------------
// 1) 邀请链接 + 批准（真实 relay）
// ---------------------------------------------------------------------------

#[gpui::test]
async fn panel_generates_invite_and_approves_pending_device(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let a = creator("panel");
    let relay_url = a.relay.base_url().to_owned();
    let a_peer = a.identity.loro_peer_id();
    let a_epoch = a.identity.current_epoch;
    let a_device = a.identity.device_id;
    let a_vault_id = a.identity.vault_id;

    let (host, handle) = add_shell_window(
        cx,
        a.vault.clone(),
        "dev-sync-invite-a",
        config_for(&relay_url, &a.path),
        SyncContext::new(a_peer, a_epoch),
    );
    let app_cx = cx.clone();
    let mut cx = VisualTestContext::from_window(handle, &app_cx);
    cx.run_until_parked();
    cx.executor()
        .advance_clock(menghunote_app::dev_sync::SYNC_PUMP_INTERVAL);
    cx.run_until_parked();

    // 前置：面板常驻 selector 在（设备区域/邀请按钮）。
    assert!(cx.debug_bounds("dev-sync-devices").is_some());
    assert!(cx.debug_bounds("dev-sync-invite").is_some());

    // 等 worker 上报身份事实（管理钥有无由 worker 读受保护身份后上报）。
    drive(&mut cx, &host, "A 读到管理钥能力", |panel| {
        panel.can_manage_devices()
    });
    // AppShell 构造期自动打开第一篇 → 有活动文档（邀请按钮的硬前置）。
    with_shell(&mut cx, &host, |shell, cx| shell.dev_sync_pump(cx));
    cx.run_until_parked();
    assert_eq!(
        read_panel(&mut cx, &host, |panel| panel.invite_block_reason()),
        None,
        "有管理钥 + 活动文档 + 空闲时必须允许生成邀请"
    );

    // 点击「邀请设备…」→ 链接出现在面板里（内存）。
    click_selector(&mut cx, "dev-sync-invite");
    drive(
        &mut cx,
        &host,
        "邀请链接已生成且操作收尾",
        |panel| panel.invite_link().is_some() && panel.service().snapshot().in_flight.is_none(),
    );
    let link = read_panel(&mut cx, &host, |panel| {
        panel.invite_link().expect("链接").clone()
    });
    assert!(
        link.url.starts_with("mn-pair://join?v=1&relay="),
        "{}",
        link.url
    );
    assert!(link.url.contains("#token="), "token 必须在 fragment");
    assert!(
        !link.url.contains(&a.recovery_code),
        "邀请链接绝不能包含恢复码"
    );
    // Debug 不打 token。
    let debug = format!("{link:?}");
    assert!(!debug.contains("mn-pair"), "{debug}");
    assert!(debug.contains("<redacted: contains token>"), "{debug}");
    // 链接/TTL/复制按钮都参与布局。
    for selector in [
        "dev-sync-invite-link",
        "dev-sync-invite-ttl",
        "dev-sync-copy-invite",
    ] {
        let bounds = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("必须画出 selector={selector}"));
        assert!(
            bounds.size.width.as_f32() > 0.0 && bounds.size.height.as_f32() > 0.0,
            "{selector} 必须有非零尺寸"
        );
    }

    // B：用 A 面板生成的链接兑换（库级入口，真实 relay）→ PENDING。
    let target = InviteLinkTarget::parse(&link.url).expect("解析失败");
    let context = RelayContext::local_dev(&relay_url, DEFAULT_TRANSPORT_TIMEOUTS).expect("context");
    let b_path = a.dir.path().join("inst-b").join("vault.sqlite3");
    let started = InviteJoin::redeem(
        &context,
        &target,
        &b_path,
        u64::try_from(a.relay.now()).expect("now"),
    )
    .expect("B 兑换失败");
    let b_device = started.pending.device_id;
    assert_eq!(started.pending.vault_id, a_vault_id);
    drop(started.vault);

    // A 刷新设备列表 → 出现 pending 行 + 批准按钮（仅管理设备）。
    drive(&mut cx, &host, "A 空闲后刷新设备", |panel| {
        panel.service().snapshot().in_flight.is_none()
    });
    click_selector(&mut cx, "dev-sync-refresh-devices");
    drive(&mut cx, &host, "A 看到 pending 设备", |panel| {
        panel.service().snapshot().in_flight.is_none()
            && panel
                .devices()
                .iter()
                .any(|member| member.device_id == b_device)
    });
    let short = menghunote_app::dev_sync::short_device_id(&b_device);
    let approve_selector = format!("dev-sync-approve-{short}");
    let bounds = cx
        .debug_bounds(probe(&approve_selector))
        .unwrap_or_else(|| panic!("pending 行必须有批准按钮：{approve_selector}"));
    assert!(bounds.size.width.as_f32() > 0.0);

    // 点击批准 → 确认区展示 device_id + sign/hpke 短 hash（双端比对）。
    click_selector(&mut cx, &approve_selector);
    let confirm = read_panel(&mut cx, &host, |panel| panel.approve_confirm_text())
        .expect("确认区必须显示目标/指纹");
    assert!(
        confirm.contains(&menghunote_sync::ids::hex_of(&b_device)),
        "{confirm}"
    );
    assert!(confirm.contains("sign "), "{confirm}");
    assert!(confirm.contains("hpke "), "{confirm}");
    assert!(cx.debug_bounds("dev-sync-approve-warning").is_some());
    assert!(cx.debug_bounds("dev-sync-approve-confirm").is_some());

    click_selector(&mut cx, "dev-sync-approve-confirm");
    drive(&mut cx, &host, "B 变为 Active", |panel| {
        panel.devices().iter().any(|member| {
            member.device_id == b_device
                && member.status == menghunote_sync::http_relay::MemberStatus::Active
        })
    });
    // 批准后 B 的批准按钮消失（不再 pending）。
    assert!(cx.debug_bounds(probe(&approve_selector)).is_none());
    let notice = read_panel(&mut cx, &host, |panel| {
        panel.notice_text().map(str::to_owned)
    })
    .expect("批准必须有 notice");
    assert!(notice.contains(&short), "{notice}");

    // B 检查 + bootstrap：批准真的授了权（数据面闭环）。
    let vault = menghunote_storage::Vault::open(&b_path).expect("重开 B 库");
    let now = u64::try_from(a.relay.now()).expect("now");
    assert_eq!(
        InviteJoin::check(&context, &vault, now).expect("check"),
        InviteCheck::Approved
    );
    let InviteFinish::Joined(joined) = InviteJoin::finish(&context, &vault, now).expect("finish")
    else {
        panic!("批准后必须直接 bootstrap");
    };
    assert_eq!(joined.identity.device_id, b_device);
    assert!(joined.identity.manager_seed.is_none());
    let blob = joined.identity.protect().expect("身份封装");
    menghunote_app::dev_sync::DevIdentityStore
        .persist_protected(
            &b_path,
            &menghunote_sync::identity::LocalIdentityV1::unprotect(
                &blob,
                &joined.identity.vault_id,
                &joined.identity.device_id,
            )
            .expect("解封"),
        )
        .expect("B 身份落盘");
    DevLocal::sync_once(&relay_url, &vault, &joined.identity).expect("B 同步");
    let text = menghunote_storage::load_body_doc(
        &vault,
        &a.note_doc,
        joined.identity.loro_peer_id(),
        None,
    )
    .expect("B 读正文")
    .0
    .text();
    assert!(!text.is_empty(), "B 必须看到 A 的笔记正文");

    // A 仍持有管理钥（批准不改变管理权）。
    assert!(a.identity.manager_seed.is_some());
    let _ = a_device;
}

// ---------------------------------------------------------------------------
// 2) 非管理设备：没有批准入口；邀请按钮置灰并说明
// ---------------------------------------------------------------------------

#[gpui::test]
async fn join_device_has_no_invite_or_approve_entry(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let a = creator("nonmanager");
    let relay_url = a.relay.base_url().to_owned();
    // B：用恢复码 Join（普通设备，没有管理私钥）。
    let b_path = a.dir.path().join("inst-b").join("vault.sqlite3");
    let b = setup::prepare(
        &config_for(&relay_url, &b_path),
        SetupMode::Join,
        SetupSecrets {
            vault_id: Some(menghunote_sync::ids::hex_of(&a.identity.vault_id)),
            recovery_code: Some(a.recovery_code.clone()),
            ..SetupSecrets::default()
        },
    )
    .expect("B Join 失败");
    let b_peer = b.identity.loro_peer_id();
    let b_epoch = b.identity.current_epoch;
    let b_device = b.identity.device_id;
    assert!(b.identity.manager_seed.is_none());
    drop(b.vault);
    let dev_b = dev_vault::open_vault_at_with_peer(&b_path, b_peer).expect("B 打开实例库失败");

    let (host, handle) = add_shell_window(
        cx,
        dev_b.vault.clone(),
        "dev-sync-invite-b",
        config_for(&relay_url, &b_path),
        SyncContext::new(b_peer, b_epoch),
    );
    let app_cx = cx.clone();
    let mut cx = VisualTestContext::from_window(handle, &app_cx);
    cx.run_until_parked();
    with_shell(&mut cx, &host, |shell, cx| shell.dev_sync_pump(cx));
    click_selector(&mut cx, "dev-sync-refresh-devices");
    drive(&mut cx, &host, "B 读到设备列表", |panel| {
        !panel.devices().is_empty()
    });

    // 邀请按钮：明确置灰 + 原因（缺管理密钥）。
    let reason = read_panel(&mut cx, &host, |panel| panel.invite_block_reason());
    assert!(
        reason.is_some_and(|reason| reason.contains("管理密钥")),
        "Join 设备必须说明缺管理密钥：{reason:?}"
    );
    click_selector(&mut cx, "dev-sync-invite");
    let notice = read_panel(&mut cx, &host, |panel| {
        panel.notice_text().map(str::to_owned)
    });
    assert!(
        notice
            .clone()
            .is_some_and(|notice| notice.contains("管理密钥")),
        "点邀请必须得到明确原因：{notice:?}"
    );
    assert!(
        read_panel(&mut cx, &host, |panel| panel.invite_link().is_none()),
        "非管理设备不得生成链接"
    );

    // 批准入口：pending 行的批准按钮一律不渲染（用 A 制造一个 pending 设备来看）。
    let context = RelayContext::local_dev(&relay_url, DEFAULT_TRANSPORT_TIMEOUTS).expect("context");
    let link = menghunote_sync::invite_link::create_invite_link(&context, &a.vault, &a.identity)
        .expect("A 生成邀请");
    let target = InviteLinkTarget::parse(&link.url).expect("解析");
    let c_path = a.dir.path().join("inst-c").join("vault.sqlite3");
    let c = InviteJoin::redeem(
        &context,
        &target,
        &c_path,
        u64::try_from(a.relay.now()).unwrap(),
    )
    .expect("C 兑换");
    let c_device = c.pending.device_id;
    drop(c.vault);
    click_selector(&mut cx, "dev-sync-refresh-devices");
    drive(&mut cx, &host, "B 看到 C 是 pending", |panel| {
        panel
            .devices()
            .iter()
            .any(|member| member.device_id == c_device)
    });
    let short = menghunote_app::dev_sync::short_device_id(&c_device);
    assert!(
        cx.debug_bounds(probe(&format!("dev-sync-approve-{short}")))
            .is_none(),
        "非管理设备不得有批准入口"
    );
    let _ = b_device;
}

// ---------------------------------------------------------------------------
// 3) 无活动文档：邀请按钮置灰
// ---------------------------------------------------------------------------

#[test]
fn worker_refuses_invite_and_approve_without_manager_key() {
    ensure_test_user_key();
    let a = creator("worker-guards");
    let relay_url = a.relay.base_url().to_owned();
    let b_path = a.dir.path().join("inst-b").join("vault.sqlite3");
    let b = setup::prepare(
        &config_for(&relay_url, &b_path),
        SetupMode::Join,
        SetupSecrets {
            vault_id: Some(menghunote_sync::ids::hex_of(&a.identity.vault_id)),
            recovery_code: Some(a.recovery_code.clone()),
            ..SetupSecrets::default()
        },
    )
    .expect("B Join 失败");
    drop(b.vault);

    let mut runner = DevLocalRunner::new(&relay_url, b_path);
    let mut events = Vec::new();
    let mut emit = |event: menghunote_app::dev_sync::state::Event| events.push(event);
    for command in [
        Command::CreateInvite,
        Command::ApproveDevice { device: [0x11; 16] },
    ] {
        let error = runner
            .run(command, &mut emit)
            .expect_err("Join 设备必须被明确拒绝");
        assert!(
            error.contains("管理密钥"),
            "错误必须点明缺管理密钥：{error}"
        );
    }
    assert!(
        !events.iter().any(|event| matches!(
            event,
            menghunote_app::dev_sync::state::Event::InviteLinkCreated { .. }
                | menghunote_app::dev_sync::state::Event::DeviceApproved { .. }
        )),
        "被拒绝的操作不得发出成功/领域事件：{events:?}"
    );
}

/// 面板宿主（`add_window_view` 需要一个 `Render` 视图）。
struct PanelHost {
    panel: Entity<DevSyncPanel>,
}

impl Render for PanelHost {
    fn render(
        &mut self,
        _window: &mut gpui::Window,
        _cx: &mut gpui::Context<Self>,
    ) -> impl gpui::IntoElement {
        div()
            .size_full()
            .debug_selector(|| "panel-host".to_string())
            .child(self.panel.clone())
    }
}

/// 只回一条设备列表的假 runner（隔离"没有活动文档"这个宿主事实，不联网）。
struct ListOnlyRunner;

impl menghunote_app::dev_sync::service::SyncRunner for ListOnlyRunner {
    fn run(
        &mut self,
        op: Command,
        events: &mut dyn FnMut(menghunote_app::dev_sync::state::Event),
    ) -> Result<menghunote_app::dev_sync::state::OpOutcome, String> {
        match op {
            Command::ListDevices => {
                events(menghunote_app::dev_sync::state::Event::DevicesListed {
                    members: Vec::new(),
                    self_device: [0x0a; 16],
                    can_manage: true,
                });
                Ok(menghunote_app::dev_sync::state::OpOutcome::new(
                    "设备列表已刷新（0 台）",
                ))
            }
            other => Err(format!("fake runner 不支持 {:?}", other.kind())),
        }
    }
}

#[gpui::test]
async fn invite_button_is_blocked_without_active_document(cx: &mut TestAppContext) {
    use menghunote_app::dev_sync::service::{ServiceConfig, SyncService};
    use std::sync::Arc;

    let service = SyncService::with_runner(
        ServiceConfig::new("http://127.0.0.1:18080"),
        Box::new(ListOnlyRunner),
    );
    let slot = std::rc::Rc::new(std::cell::RefCell::new(None));
    let captured = slot.clone();
    let (_host, cx) = cx.add_window_view(move |_window, cx| {
        let panel = cx.new(move |cx| DevSyncPanel::with_service(Arc::clone(&service), cx));
        *captured.borrow_mut() = Some(panel.clone());
        PanelHost { panel }
    });
    let panel = slot.borrow().clone().expect("面板实体");
    // 泵到 can_manage=true（面板打开自动刷新），但宿主从未写入"有活动文档"。
    let deadline = Instant::now() + DEADLINE;
    loop {
        cx.executor()
            .advance_clock(menghunote_app::dev_sync::SYNC_PUMP_INTERVAL);
        cx.run_until_parked();
        let ready = cx.update(|_window, app| panel.read(app).can_manage_devices());
        if ready {
            break;
        }
        assert!(Instant::now() < deadline, "面板 30s 内没读到管理能力");
    }
    let reason = cx.update(|_window, app| panel.read(app).invite_block_reason());
    assert!(
        reason.is_some_and(|reason| reason.contains("活动文档")),
        "没有活动文档必须说明原因：{reason:?}"
    );
    let bounds = cx
        .debug_bounds("dev-sync-invite")
        .expect("邀请按钮必须常显（不可用时置灰）");
    assert!(bounds.size.width.as_f32() > 0.0);
}
