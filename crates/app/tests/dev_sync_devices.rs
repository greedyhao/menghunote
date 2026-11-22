//! ADR 0010 切片 2a-2：dev-only 同步面板的**设备列表**与**移除设备（不轮换密钥）**。
//!
//! 覆盖四层，全部确定性（**不 sleep**）：
//!
//! 1. service + fake runner：`ListDevices` / `RevokeDevice` 的领域事件
//!    （`DevicesListed` / `DeviceRevoked`）与快照投影；失败走 `last_error`，不写"已移除"；
//! 2. 生产 runner 的本地守卫：**本机禁止移除**、**缺管理密钥**（Join 设备）都在发任何
//!    管理端请求之前明确失败，且没有副作用；
//! 3. AppShell（真实 dev-relay + 两个 window）：
//!    A 面板列表含 A/B（A 标「本机」且没有移除按钮）→ 点 B 的移除 → 二次确认 →
//!    确认后 B=Revoked、按钮消失、notice 写「已移除 …（仅断访问）… 认证已刷新」；
//!    Join 的 B 面板能读列表但移除按钮点不动（缺管理密钥）；
//! 4. 移除后的访问事实：B 的持闸同步明确失败（`REVOKED`），A 的同步照常成功。
//!
//! ⚠️ **非生产**：`menghunote-dev-relay` 的 hook 对任何签名一律放行；本文件验证的是
//! "面板/worker 真的按 ADR 0010 的默认语义（只断访问）工作"，不是 relay 的鉴权验收。
//!
//! 纪律（AGENTS.md §4/§6）：临时目录一律在 `CARGO_TARGET_TMPDIR`；**不 sleep**，收敛判据
//! 只有状态（`in_flight` 归零、列表出现目标状态、`Completed` 终态）；headless 下 GPUI 定时器
//! 不自动走时，需要"面板打开自动刷新"的用例用 `executor().advance_clock` 显式推进测试时钟。
//!
//! feature 关闭时整个文件编译为空（0 个测试）。

#![cfg(feature = "dev-sync")]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    div, AppContext as _, Context, Entity, InteractiveElement as _, Modifiers, ParentElement as _,
    Render, Styled as _, TestAppContext, VisualTestContext,
};
use menghunote_app::dev_sync::gate::DevSyncConfig;
use menghunote_app::dev_sync::service::{DevLocalRunner, ServiceConfig, SyncRunner, SyncService};
use menghunote_app::dev_sync::setup::{self, SetupMode, SetupSecrets};
use menghunote_app::dev_sync::state::{
    Command, Event, OpId, OpOutcome, OpResult, Phase, REVOKE_UNCHANGED_MARKER,
};
use menghunote_app::dev_sync::DevSyncPanel;
use menghunote_app::dev_vault;
use menghunote_app::shell::AppShell;
use menghunote_dev_relay::DevRelay;
use menghunote_storage::{list_tree, SyncContext, Vault};
use menghunote_sync::dev_local::DevLocal;
use menghunote_sync::http_relay::{MemberStatus, MemberView};
use menghunote_sync::identity::LocalIdentityV1;
use menghunote_sync::ids;
use menghunote_sync::DeviceId;
use tempfile::{Builder as TempDirBuilder, TempDir};

/// 事件/状态收敛的安全上限（真实 relay 走 loopback，正常毫秒级；超时说明状态机卡死）。
const DEADLINE: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// 基础设施：测试用户主键 / target/tmp / 固定 selector 宿主
// ---------------------------------------------------------------------------

/// 测试进程级的 `XDG_DATA_HOME`（Linux 用户主键；Windows 是 no-op）。
fn ensure_test_user_key() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("dev-sync-devices-xdg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("创建测试 XDG_DATA_HOME 失败");
        std::env::set_var("XDG_DATA_HOME", &dir);
        menghunote_sync::identity_wrap::initialize_user_key()
            .expect("初始化测试用户主键失败（ADR 0008）");
    });
}

/// 在 `CARGO_TARGET_TMPDIR`（`target/tmp`）下开一个独立临时目录。
fn temp_root(prefix: &str) -> TempDir {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("dev-sync-devices-{prefix}-{}", std::process::id()));
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

/// 一个成员的 fake 视图（`sign_pk`/`hpke_pk` 是占位：面板不得显示/依赖它们）。
fn member(device_id: DeviceId, status: MemberStatus, fingerprint: Option<&str>) -> MemberView {
    MemberView {
        device_id,
        status,
        fingerprint: fingerprint.map(str::to_owned),
        sign_pk: [0x11; 32],
        hpke_pk: [0x22; 32],
        issued_manager_rev: (status == MemberStatus::Active).then_some(1),
    }
}

// ---------------------------------------------------------------------------
// 1) service + fake runner：List/Revoke 的领域事件与快照投影
// ---------------------------------------------------------------------------

/// 只按预设脚本响应设备命令的假 runner（不碰网络/磁盘，确定性）。
struct FakeDeviceRunner {
    self_device: DeviceId,
    can_manage: bool,
    members: Vec<MemberView>,
    /// 预设的 `ListDevices` 失败原因。
    list_error: Option<String>,
    /// 预设的 `RevokeDevice` 失败原因。
    revoke_error: Option<String>,
    /// 移除后返回的成员视图。
    after_revoke: Vec<MemberView>,
}

impl SyncRunner for FakeDeviceRunner {
    fn run(&mut self, op: Command, events: &mut dyn FnMut(Event)) -> Result<OpOutcome, String> {
        match op {
            Command::ListDevices => {
                events(Event::Progress {
                    op_id: OpId::UNASSIGNED,
                    phase: Phase::Transport,
                    detail: "fake: 读取设备列表".to_owned(),
                });
                if let Some(message) = &self.list_error {
                    return Err(message.clone());
                }
                events(Event::DevicesListed {
                    members: self.members.clone(),
                    self_device: self.self_device,
                    can_manage: self.can_manage,
                    published_baseline: None,
                });
                Ok(OpOutcome::new(format!(
                    "设备列表已刷新（{} 台）",
                    self.members.len()
                )))
            }
            Command::RevokeDevice { target } => {
                if let Some(message) = &self.revoke_error {
                    return Err(message.clone());
                }
                let summary = format!(
                    "已移除 {}（仅断访问；auth_rev 7）",
                    menghunote_app::dev_sync::short_device_id(&target)
                );
                events(Event::DeviceRevoked {
                    target,
                    result: OpResult::Ok {
                        summary: summary.clone(),
                    },
                });
                events(Event::DevicesListed {
                    members: self.after_revoke.clone(),
                    self_device: self.self_device,
                    can_manage: self.can_manage,
                    published_baseline: None,
                });
                Ok(OpOutcome::new(summary))
            }
            other => Err(format!("fake runner 不支持 {:?}", other.kind())),
        }
    }
}

fn fake_service(runner: FakeDeviceRunner) -> Arc<SyncService> {
    SyncService::with_runner(
        ServiceConfig::new("http://127.0.0.1:18080"),
        Box::new(runner),
    )
}

/// 跑完本次排队的命令并关停 worker：`reap_worker` 的 join 是唯一需要的收敛点。
fn drain(service: &SyncService) -> Vec<Event> {
    let _ticket = service.request_stop();
    assert!(service.reap_worker(), "worker 必须在 Stop 后退出");
    service.poll()
}

/// 取走事件直到在途 op 收尾（有界自旋：既不是 sleep，也不依赖事件队列为空）。
fn wait_idle(service: &SyncService, collected: &mut Vec<Event>) {
    let deadline = Instant::now() + DEADLINE;
    loop {
        collected.extend(service.poll());
        let snapshot = service.snapshot();
        if snapshot.in_flight.is_none() && snapshot.last_completed.is_some() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "30s 内没有等到 op 收尾：{:?}",
            snapshot.state
        );
        std::thread::yield_now();
    }
}

#[test]
fn service_surfaces_devices_listed_and_device_revoked_events() {
    let a: DeviceId = [0x0a; 16];
    let b: DeviceId = [0x0b; 16];
    let before = vec![
        member(
            a,
            MemberStatus::Active,
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
        ),
        member(
            b,
            MemberStatus::Active,
            Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
        ),
    ];
    // 移除后同一台设备变 Revoked（历史证书保留 → 指纹不变）。
    let after = vec![
        member(
            a,
            MemberStatus::Active,
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
        ),
        member(
            b,
            MemberStatus::Revoked,
            Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
        ),
    ];
    // 单操作串行：列表与移除各用一台新服务（每条命令独立收尾），不并发派发。
    let list_service = fake_service(FakeDeviceRunner {
        self_device: a,
        can_manage: true,
        members: before.clone(),
        list_error: None,
        revoke_error: None,
        after_revoke: after.clone(),
    });
    let list_op = list_service
        .try_command(Command::ListDevices)
        .expect("刷新设备命令");
    let list_events = drain(&list_service);
    let listed: Vec<&Event> = list_events
        .iter()
        .filter(|event| matches!(event, Event::DevicesListed { .. }))
        .collect();
    assert_eq!(
        listed.len(),
        1,
        "刷新设备必须恰好一条列表事件：{list_events:?}"
    );
    match listed[0] {
        Event::DevicesListed {
            members,
            self_device,
            can_manage,
            ..
        } => {
            assert_eq!(members, &before, "第一条列表事件必须是移除前的快照");
            assert_eq!(*self_device, a);
            assert!(*can_manage, "fake 有管理密钥");
        }
        other => panic!("必须是 DevicesListed：{other:?}"),
    }
    let service = fake_service(FakeDeviceRunner {
        self_device: a,
        can_manage: true,
        members: before.clone(),
        list_error: None,
        revoke_error: None,
        after_revoke: after.clone(),
    });
    let revoke_op = service
        .try_command(Command::RevokeDevice { target: b })
        .expect("移除设备命令");
    let events = drain(&service);
    let revoked = events
        .iter()
        .find_map(|event| match event {
            Event::DeviceRevoked { target, result } => Some((*target, result.clone())),
            _ => None,
        })
        .expect("必须有一条 DeviceRevoked");
    assert_eq!(revoked.0, b, "事件必须带被移除的目标设备");
    assert_eq!(
        revoked.1,
        OpResult::Ok {
            summary: format!(
                "已移除 {}（仅断访问；auth_rev 7）",
                menghunote_app::dev_sync::short_device_id(&b)
            )
        },
        "成功摘要必须说清仅断访问"
    );
    assert_eq!(
        events
            .iter()
            .find(|event| matches!(event, Event::DevicesListed { .. }))
            .expect("列表事件")
            .op_id(),
        OpId::UNASSIGNED,
        "领域事件不属于任何在途 op"
    );
    // 两条命令都要有 Ok 终态（不谎报/不吞）。
    let completed: Vec<&Event> = events
        .iter()
        .filter(|event| matches!(event, Event::Completed { .. }))
        .collect();
    assert_eq!(completed.len(), 1, "移除命令必须有终态：{events:?}");
    for event in completed {
        match event {
            Event::Completed { op_id, result, .. } => {
                assert_eq!(*op_id, revoke_op);
                assert!(result.is_ok(), "移除命令必须成功：{result:?}");
            }
            other => panic!("已过滤：{other:?}"),
        }
    }
    // 列表命令的终态也必须成功（同一个断言口径，分开的服务各查一次）。
    assert!(
        list_events.iter().any(|event| matches!(
            event,
            Event::Completed {
                op_id,
                result: OpResult::Ok { .. },
                ..
            } if *op_id == list_op
        )),
        "刷新设备命令必须有 Ok 终态：{list_events:?}"
    );

    // 快照投影：面板只看这个（列表 + 本机 + 管理能力 + 最近一次移除）。
    let snapshot = service.snapshot();
    assert_eq!(snapshot.devices, after, "快照必须是最新（移除后）列表");
    assert_eq!(snapshot.self_device, Some(a));
    assert!(snapshot.can_manage_devices);
    assert_eq!(
        snapshot.last_revoked,
        Some((
            b,
            OpResult::Ok {
                summary: format!(
                    "已移除 {}（仅断访问；auth_rev 7）",
                    menghunote_app::dev_sync::short_device_id(&b)
                )
            }
        ))
    );
    assert_eq!(snapshot.last_error, None, "成功后不得留下错误");
    assert!(snapshot.in_flight.is_none(), "两条命令都必须收尾");
}

#[test]
fn service_reports_device_failures_without_claiming_success() {
    let a: DeviceId = [0x0a; 16];
    let b: DeviceId = [0x0b; 16];
    // 列表成功过一次（拿到基线），随后移除失败：只有错误、没有"已移除"。
    let service = fake_service(FakeDeviceRunner {
        self_device: a,
        can_manage: true,
        members: vec![member(a, MemberStatus::Active, None)],
        list_error: None,
        revoke_error: Some("移除设备被拒绝：本机没有管理密钥（Join 设备）".to_owned()),
        after_revoke: Vec::new(),
    });
    let mut collected = Vec::new();
    service
        .try_command(Command::ListDevices)
        .expect("刷新设备命令");
    wait_idle(&service, &mut collected);
    service
        .try_command(Command::RevokeDevice { target: b })
        .expect("移除设备命令");
    let mut events = drain(&service);
    collected.append(&mut events);
    let events = collected;

    let failure = events
        .iter()
        .find_map(|event| match event {
            Event::Completed {
                result: OpResult::Failed { message },
                ..
            } => Some(message.clone()),
            _ => None,
        })
        .expect("必须有失败终态");
    assert!(
        failure.contains("管理密钥"),
        "失败原因必须原样可见：{failure}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::DeviceRevoked { .. })),
        "失败时不得发 DeviceRevoked（那是成功事实）：{events:?}"
    );
    let snapshot = service.snapshot();
    assert_eq!(snapshot.last_revoked, None, "失败不得写成已移除");
    assert!(
        snapshot
            .last_error
            .as_deref()
            .is_some_and(|error| error.contains("管理密钥")),
        "错误必须进面板错误行：{:?}",
        snapshot.last_error
    );
    assert_eq!(
        snapshot.devices.len(),
        1,
        "失败不得清空已有列表（只读快照保留）"
    );
}

// ---------------------------------------------------------------------------
// 2) 生产 runner 的本地守卫：本机 / 缺管理密钥（真实 relay + 两台设备）
// ---------------------------------------------------------------------------

/// A（Create，有管理密钥）与 B（Join，无管理密钥）两台真实实例。
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
    /// A/B 的公开设备 id（面板列表里按 `device_id` 升序）。
    a_device: DeviceId,
    b_device: DeviceId,
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
            recovery_code: Some(recovery),
            ..SetupSecrets::default()
        },
    )
    .expect("B Join 失败");
    let b_peer = b.identity.loro_peer_id();
    let b_epoch = b.identity.current_epoch;
    let b_device = b.identity.device_id;
    drop(b.vault);
    let dev_b = dev_vault::open_vault_at_with_peer(&b_path, b_peer).expect("B 打开实例库失败");
    assert!(!dev_b.seeded, "B 的树来自基线，不得再播种");
    assert!(
        b.identity.manager_seed.is_none(),
        "Join 设备不得持有管理密钥"
    );

    Pair {
        relay,
        _dir: dir,
        a_path,
        b_path,
        a_vault: dev_a.vault.clone(),
        b_vault: dev_b.vault.clone(),
        a_device: a.identity.device_id,
        b_device,
        a_identity: a.identity,
        a_sync: SyncContext::new(a_peer, a_epoch),
        b_sync: SyncContext::new(b_peer, b_epoch),
    }
}

#[test]
fn local_guards_refuse_self_and_missing_manager_key() {
    ensure_test_user_key();
    let pair = establish_pair("guards");
    let relay_url = pair.relay.base_url().to_owned();
    let a_device = pair.a_device;
    let b_device = pair.b_device;

    // ① A 移除自己：本地拒绝（在打开管理端请求之前）。
    let mut runner_a = DevLocalRunner::new(&relay_url, pair.a_path.clone());
    let mut progress = Vec::new();
    let mut emit = |event: Event| progress.push(event);
    let error = runner_a
        .run(Command::RevokeDevice { target: a_device }, &mut emit)
        .expect_err("本机移除必须被拒绝");
    assert!(error.contains("本机"), "错误必须点明不能移除本机：{error}");

    // ② B（Join 设备）移除 A：缺管理密钥，明确失败。
    let mut runner_b = DevLocalRunner::new(&relay_url, pair.b_path.clone());
    let error = runner_b
        .run(Command::RevokeDevice { target: a_device }, &mut emit)
        .expect_err("缺管理密钥必须被拒绝");
    assert!(
        error.contains("管理密钥"),
        "错误必须点明缺管理密钥：{error}"
    );

    // ③ 无副作用：A/B 都还是 Active。
    let members = DevLocal::members(&relay_url, &pair.a_vault, &pair.a_identity)
        .expect("守卫拒绝后仍应能读列表");
    assert_eq!(members.len(), 2);
    assert!(
        members
            .iter()
            .all(|view| view.status == MemberStatus::Active),
        "本地守卫拒绝后不得有副作用：{members:?}"
    );
    assert!(
        members.iter().any(|view| view.device_id == b_device),
        "B 必须仍在列表里"
    );
    // ④ 库路径不可用时，设备命令也在打开库这一步就明确失败（不 panic、不静默）。
    let mut runner_missing = DevLocalRunner::new(&relay_url, pair.a_path.join("missing.sqlite3"));
    let error = runner_missing
        .run_gated(Command::ListDevices, None, &mut emit)
        .expect_err("库路径不可用必须明确失败");
    assert!(
        error.contains("打开本地库失败"),
        "错误必须点明打不开库：{error}"
    );

    drop(emit);
    assert!(
        progress
            .iter()
            .all(|event| matches!(event, Event::Progress { .. })),
        "runner 只上报 Progress（终态由 worker 生成）"
    );
}

// ---------------------------------------------------------------------------
// 3/4) AppShell（真实 dev-relay + 两个 window）：列表 / 确认流 / 访问事实
// ---------------------------------------------------------------------------

/// 在某个窗口里建一个 `AppShell`，并把窗口句柄带回来。
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

fn devices(cx: &mut VisualTestContext, host: &Entity<ShellHost>) -> Vec<MemberView> {
    read_panel(cx, host, |panel| panel.devices().to_vec())
}

/// 设备在面板列表里的下标（列表按 `device_id` 稳定升序）。
fn device_index(devices: &[MemberView], target: DeviceId) -> usize {
    devices
        .iter()
        .position(|view| view.device_id == target)
        .unwrap_or_else(|| panic!("设备列表缺少目标设备：{devices:?}"))
}

/// 泵到设备列表满足条件（面板事件泵 + 服务 poll；**不 sleep**）。
fn drive_devices(
    cx: &mut VisualTestContext,
    host: &Entity<ShellHost>,
    description: &str,
    expect: impl Fn(&[MemberView]) -> bool,
) {
    let deadline = Instant::now() + DEADLINE;
    loop {
        // headless 下 GPUI 定时器不会自己走：显式推进测试时钟，让面板的 40 ms 泵跑一拍
        // （"面板打开自动刷新"就靠这一拍派发）。
        cx.executor()
            .advance_clock(menghunote_app::dev_sync::SYNC_PUMP_INTERVAL);
        with_shell(cx, host, |shell, cx| shell.dev_sync_pump(cx));
        cx.run_until_parked();
        let current = devices(cx, host);
        // 列表事件由 op 执行体在 `Completed` **之前**发出，所以"设备变了"不等于
        // "这次 op 已收尾"；必须同时要求 `in_flight` 归零，否则下一条命令会被 Busy 拒绝。
        let idle = read_panel(cx, host, |panel| {
            panel.service().snapshot().in_flight.is_none()
        });
        if idle && expect(&current) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "30s 内没有收敛（{description}）：{current:?}"
        );
        std::thread::yield_now();
    }
}

/// 动态 selector → `&'static str`（`debug_bounds` 只接受静态字符串；测试进程寿命内泄漏
/// 几个设备行 selector 属于可接受的测试开销，不进生产代码）。
fn probe(selector: &str) -> &'static str {
    Box::leak(selector.to_owned().into_boxed_str())
}

/// 点一个动态 selector（设备行按钮用）。
fn click_dynamic(cx: &mut VisualTestContext, selector: &str) {
    click_selector(cx, probe(selector));
}

/// 点一个固定 selector（必须存在且非零尺寸）。
fn click_selector(cx: &mut VisualTestContext, selector: &'static str) {
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("必须画出 selector={selector}"));
    cx.simulate_click(bounds.center(), Modifiers::default());
    cx.run_until_parked();
}

/// 走一次持闸同步（A/B 共用），失败直接 panic 带上原因。
fn run_held_sync(cx: &mut VisualTestContext, host: &Entity<ShellHost>) {
    with_shell(cx, host, |shell, cx| shell.dev_sync_sync_once(cx))
        .unwrap_or_else(|error| panic!("持闸同步被拒绝：{error}"));
    let deadline = Instant::now() + DEADLINE;
    loop {
        with_shell(cx, host, |shell, cx| shell.dev_sync_pump(cx));
        cx.run_until_parked();
        let finished = read_shell(cx, host, |shell, _app| !shell.dev_sync_sync_in_progress());
        if finished {
            return;
        }
        assert!(Instant::now() < deadline, "持闸同步 30s 内没有收敛");
        std::thread::yield_now();
    }
}

/// 同步成功后宿主会**自动**刷新一次设备列表（ADR 0010 切片 2a-2）：泵到"列表命令"
/// 收尾（op id 晚于同步 op，摘要是列表刷新），证明这条链路真的跑过。
fn wait_device_refresh_after(cx: &mut VisualTestContext, host: &Entity<ShellHost>, sync_op: OpId) {
    let panel = panel_entity(cx, host);
    let service = cx.update(|_window, app| panel.read(app).service().clone());
    let deadline = Instant::now() + DEADLINE;
    loop {
        cx.executor()
            .advance_clock(menghunote_app::dev_sync::SYNC_PUMP_INTERVAL);
        with_shell(cx, host, |shell, cx| shell.dev_sync_pump(cx));
        cx.run_until_parked();
        let snapshot = service.snapshot();
        if let Some((op_id, _, OpResult::Ok { summary })) = &snapshot.last_completed {
            if *op_id > sync_op && summary.contains("设备列表已刷新") {
                return;
            }
        }
        assert!(
            Instant::now() < deadline,
            "同步完成后 30s 内没有自动刷新设备列表：{snapshot:?}"
        );
        std::thread::yield_now();
    }
}

fn last_report(
    cx: &mut VisualTestContext,
    host: &Entity<ShellHost>,
) -> menghunote_app::dev_sync::SyncOnceReport {
    read_shell(cx, host, |shell, _app| shell.dev_sync_last_sync().cloned())
        .expect("每次持闸同步都必须留下报告")
}

#[gpui::test]
async fn panel_lists_devices_revokes_other_and_join_device_cannot_remove(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let pair = establish_pair("panel");
    let b_device = pair.b_device;
    let relay_url = pair.relay.base_url().to_owned();
    let a_device = pair.a_device;

    let (host_a, a_handle) = add_shell_window(
        cx,
        pair.a_vault.clone(),
        "dev-sync-devices-a",
        config_for(&relay_url, &pair.a_path),
        pair.a_sync,
    );
    let (host_b, b_handle) = add_shell_window(
        cx,
        pair.b_vault.clone(),
        "dev-sync-devices-b",
        config_for(&relay_url, &pair.b_path),
        pair.b_sync,
    );
    let app_cx = cx.clone();
    let mut cx_a = VisualTestContext::from_window(a_handle, &app_cx);
    let mut cx_b = VisualTestContext::from_window(b_handle, &app_cx);
    cx_a.run_until_parked();
    cx_b.run_until_parked();

    // 前置：两边都挂出了设备区域（固定警告文案是区域的一部分）。`advance_clock` 会让
    // 面板的 40 ms 泵跑一拍——两边的"面板打开自动刷新"就从这里派发。
    for (side, cx_side) in [("A", &mut cx_a), ("B", &mut cx_b)] {
        let _ = side;
        cx_side.executor().advance_clock(Duration::from_millis(40));
    }
    cx_a.run_until_parked();
    cx_b.run_until_parked();
    assert!(cx_a.debug_bounds("dev-sync-devices").is_some());
    assert!(cx_a.debug_bounds("dev-sync-refresh-devices").is_some());

    // --- A：列表含 A/B，A 是「本机」且没有移除按钮，B 有按钮 ---
    drive_devices(&mut cx_a, &host_a, "A 读到两台设备", |members| {
        members.len() == 2
    });
    let members_a = devices(&mut cx_a, &host_a);
    let index_a = device_index(&members_a, a_device);
    let index_b = device_index(&members_a, b_device);
    assert!(
        members_a
            .iter()
            .all(|view| view.status == MemberStatus::Active),
        "create/join 后两台设备都应 Active：{members_a:?}"
    );
    assert!(
        read_panel(&mut cx_a, &host_a, DevSyncPanel::can_manage_devices),
        "A（Create 设备）必须有管理密钥"
    );
    assert!(
        cx_a.debug_bounds(probe(&format!("dev-sync-remove-{index_a}")))
            .is_none(),
        "本机行不得渲染移除按钮"
    );
    assert!(
        cx_a.debug_bounds(probe(&format!("dev-sync-member-{index_a}-status")))
            .is_some(),
        "本机行的状态元素必须画出"
    );
    assert!(
        cx_a.debug_bounds(probe(&format!("dev-sync-remove-{index_b}")))
            .is_some(),
        "其它 Active 设备必须有移除按钮"
    );

    // 手动「刷新设备」按钮：真实重新读一次（notice 显示已排队），并等它收尾。
    click_selector(&mut cx_a, "dev-sync-refresh-devices");
    let refresh_notice = read_panel(&mut cx_a, &host_a, |panel| {
        panel.notice_text().unwrap_or_default().to_owned()
    });
    assert!(
        refresh_notice.contains("刷新设备"),
        "刷新按钮必须真的发命令并写 notice：{refresh_notice}"
    );
    drive_devices(&mut cx_a, &host_a, "手动刷新收尾", |members| {
        members.len() == 2
    });

    // --- A：点移除 → 二次确认（带目标短 id + 固定警告）→ 确认 ---
    click_dynamic(&mut cx_a, &format!("dev-sync-remove-{index_b}"));
    assert_eq!(
        read_panel(&mut cx_a, &host_a, DevSyncPanel::pending_revoke_target),
        Some(b_device),
        "点移除只进入二次确认，不直接发命令"
    );
    assert!(
        cx_a.debug_bounds("dev-sync-confirm-revoke").is_some()
            && cx_a.debug_bounds("dev-sync-cancel-revoke").is_some(),
        "确认区必须真实画出确认/取消按钮"
    );
    // 取消路径：清掉确认区且不发命令。
    click_selector(&mut cx_a, "dev-sync-cancel-revoke");
    assert_eq!(
        read_panel(&mut cx_a, &host_a, DevSyncPanel::pending_revoke_target),
        None,
        "取消后确认区必须消失"
    );
    assert!(
        devices(&mut cx_a, &host_a)
            .iter()
            .all(|view| view.status == MemberStatus::Active),
        "取消后不得有任何设备被移除"
    );

    // 再来一次并确认。
    click_dynamic(&mut cx_a, &format!("dev-sync-remove-{index_b}"));
    click_selector(&mut cx_a, "dev-sync-confirm-revoke");
    drive_devices(&mut cx_a, &host_a, "B 变成 Revoked", |members| {
        members
            .iter()
            .any(|view| view.device_id == b_device && view.status == MemberStatus::Revoked)
    });
    let members_after = devices(&mut cx_a, &host_a);
    assert_eq!(
        members_after[device_index(&members_after, a_device)].status,
        MemberStatus::Active,
        "移除 B 不得影响 A"
    );
    assert!(
        cx_a.debug_bounds(probe(&format!("dev-sync-remove-{index_b}")))
            .is_none(),
        "已移除设备不再有移除按钮"
    );
    let notice = read_panel(&mut cx_a, &host_a, |panel| {
        panel.notice_text().unwrap_or_default().to_owned()
    });
    assert!(
        notice.contains("已移除") && notice.contains("仅断访问"),
        "notice 必须说清已移除且仅断访问：{notice}"
    );
    assert!(
        notice.contains("认证已刷新"),
        "notice 必须提示认证已刷新：{notice}"
    );
    assert!(
        read_panel(&mut cx_a, &host_a, |panel| panel.error_text()).contains("最近错误：无"),
        "成功移除不得在错误行留东西"
    );
    // 移除后 A 自己仍能同步（`auth_rev` 前进 → 新客户端重新认证）。
    run_held_sync(&mut cx_a, &host_a);
    let report_a = last_report(&mut cx_a, &host_a);
    assert!(report_a.is_ok(), "移除 B 之后 A 必须仍能同步：{report_a:?}");
    // 同步完成后宿主必须自动刷新一次设备列表（不是假刷新：真的派发并收尾了新命令）。
    let sync_op = {
        let panel = panel_entity(&mut cx_a, &host_a);
        cx_a.update(|_window, app| {
            panel
                .read(app)
                .service()
                .snapshot()
                .last_completed
                .map(|(op_id, _, _)| op_id)
                .expect("同步必须有终态")
        })
    };
    wait_device_refresh_after(&mut cx_a, &host_a, sync_op);
    let members_after_refresh = devices(&mut cx_a, &host_a);
    assert_eq!(
        members_after_refresh.len(),
        2,
        "自动刷新后列表必须仍是两台设备"
    );
    assert!(
        members_after_refresh
            .iter()
            .any(|view| view.device_id == b_device && view.status == MemberStatus::Revoked),
        "自动刷新后的列表必须保留 B=Revoked：{members_after_refresh:?}"
    );

    // --- B（Join 设备）：能读列表，但移除按钮点不动（缺管理密钥）---
    drive_devices(&mut cx_b, &host_b, "B 读到两台设备", |members| {
        members.len() == 2
    });
    assert!(
        !read_panel(&mut cx_b, &host_b, DevSyncPanel::can_manage_devices),
        "Join 设备没有管理密钥"
    );
    let members_b = devices(&mut cx_b, &host_b);
    let index_a_b = device_index(&members_b, a_device);
    assert!(
        cx_b.debug_bounds(probe(&format!("dev-sync-remove-{index_a_b}")))
            .is_some(),
        "Join 设备也渲染移除按钮（只是置灰）"
    );
    click_dynamic(&mut cx_b, &format!("dev-sync-remove-{index_a_b}"));
    assert_eq!(
        read_panel(&mut cx_b, &host_b, DevSyncPanel::pending_revoke_target),
        None,
        "缺管理密钥时点击不得进入确认区"
    );
    assert!(
        cx_b.debug_bounds("dev-sync-confirm-revoke").is_none(),
        "缺管理密钥时必须没有确认区"
    );
    let notice_b = read_panel(&mut cx_b, &host_b, |panel| {
        panel.notice_text().unwrap_or_default().to_owned()
    });
    assert!(
        notice_b.contains("管理密钥"),
        "必须明确提示本机没有管理密钥：{notice_b}"
    );

    // --- 访问事实：B 的持闸同步明确失败（REVOKED），本地库里什么都还在 ---
    run_held_sync(&mut cx_b, &host_b);
    let report_b = last_report(&mut cx_b, &host_b);
    let error = report_b.error.as_deref().unwrap_or_default();
    assert!(
        error.contains("REVOKED"),
        "被移除设备的同步必须以 REVOKED 明确失败：{report_b:?}"
    );
    assert!(
        !list_tree(&pair.a_vault)
            .expect("读 A 结构树失败")
            .is_empty(),
        "A 的本地数据必须原样保留"
    );
}

#[gpui::test]
async fn repeat_revoke_does_not_claim_auth_refresh(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let pair = establish_pair("repeat");
    let b_device = pair.b_device;
    let relay_url = pair.relay.base_url().to_owned();

    // 先在 shell 之外用同一身份移除 B（真管理端请求），再让面板走一次重复移除。
    DevLocal::revoke_device(&relay_url, &pair.a_vault, &pair.a_identity, b_device)
        .expect("A 移除 B 失败");

    let (host, handle) = add_shell_window(
        cx,
        pair.a_vault.clone(),
        "dev-sync-devices-repeat",
        config_for(&relay_url, &pair.a_path),
        pair.a_sync,
    );
    let app_cx = cx.clone();
    let mut cx = VisualTestContext::from_window(handle, &app_cx);
    cx.run_until_parked();

    // 面板读到吊销后的列表：B=Revoked，因此**没有**移除按钮（重复移除只能靠 API）。
    drive_devices(&mut cx, &host, "读到 B 已 Revoked", |members| {
        members.len() == 2
    });
    let members = devices(&mut cx, &host);
    let index_b = device_index(&members, b_device);
    assert_eq!(
        members[index_b].status,
        MemberStatus::Revoked,
        "B 必须显示已移除"
    );
    assert!(
        cx.debug_bounds(probe(&format!("dev-sync-remove-{index_b}")))
            .is_none(),
        "Revoked 行不得再有移除按钮（不把 revoked 翻回 active）"
    );

    // 直接走 service 的重复移除：必须成功但**不**谎称"认证已刷新"。
    let panel = panel_entity(&mut cx, &host);
    let service = cx.update(|_window, app| panel.read(app).service().clone());
    service
        .try_command(Command::RevokeDevice { target: b_device })
        .expect("重复移除命令必须被接受（幂等语义由 relay 保证）");
    let deadline = Instant::now() + DEADLINE;
    loop {
        with_shell(&mut cx, &host, |shell, cx| shell.dev_sync_pump(cx));
        cx.run_until_parked();
        let done =
            service.snapshot().last_completed.is_some() && service.snapshot().in_flight.is_none();
        if done {
            break;
        }
        assert!(Instant::now() < deadline, "重复移除 30s 内没有收敛");
        std::thread::yield_now();
    }
    let snapshot = service.snapshot();
    assert_eq!(snapshot.last_error, None, "重复移除必须成功：{snapshot:?}");
    let last_revoked = snapshot.last_revoked.clone().expect("必须有移除事实");
    assert_eq!(last_revoked.0, b_device);
    match &last_revoked.1 {
        OpResult::Ok { summary } => assert!(
            summary.contains(REVOKE_UNCHANGED_MARKER),
            "重复移除的摘要必须标记认证未再次推进：{summary}"
        ),
        other => panic!("重复移除必须是成功事实：{other:?}"),
    }
    let notice = read_panel(&mut cx, &host, |panel| {
        panel.notice_text().unwrap_or_default().to_owned()
    });
    assert!(
        notice.contains("已移除") && notice.contains("仅断访问"),
        "重复移除的 notice 仍须说清仅断访问：{notice}"
    );
    assert!(
        !notice.contains("认证已刷新"),
        "重复移除不得谎称认证已刷新：{notice}"
    );
}
