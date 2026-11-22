//! dev-only 同步面板（ADR 0009 Phase 1 / 切片 1；ADR 0010 切片 2a-2 加设备列表）。
//!
//! ## 这一层只做投影
//!
//! 面板不碰 socket、不碰 crypto、不碰 SQLite，也**不读身份文件**：它只做三件事——
//!
//! 1. 每帧从 [`SyncService::poll`] 收一次事件（非阻塞），更新只读快照；
//! 2. 把快照画成状态行 + relay 行 + 错误行 + 设备列表 + 按钮；
//! 3. 点按钮 → [`SyncService::try_command`]；被拒绝时把**明确原因**写在面板上。
//!
//! ## 门禁
//!
//! 只有 `--features dev-sync` **且**运行时门禁通过（[`super::gate`]）才会创建本视图
//! （见 `shell.rs` 的 `AppShell::new_with_dev_sync`）。默认构建里本模块根本不存在，
//! 未启用时 `AppShell.dev_sync` 是 `None`，不渲染、不占布局。
//!
//! ## 设备列表与「移除设备」（ADR 0010 切片 2a-2）
//!
//! - 数据源是 worker 上报的 [`MemberView`] 只读投影（`DeviceListed` 事件）；
//! - **移除设备（不轮换密钥）**：只阻断目标对 relay 的访问。旧数据/旧 root 仍可能被
//!   解密，管理材料（`manager_seed`/恢复码）不会被撤销——固定警告文案见
//!   [`REVOKE_WARNING`]，按钮与确认框都必须带它；
//! - 点击「移除」只进入**二次确认**（`dev-sync-confirm-revoke`/`dev-sync-cancel-revoke`），
//!   确认后才发 [`Command::RevokeDevice`]；
//! - 本机行显示「本机」且**没有移除按钮**；Join 设备（无管理密钥）的按钮置灰并在点击
//!   时给明确原因；pending/revoked/expired 行也没有移除按钮；
//! - 自动刷新只发生在两个明确时点：面板打开（40 ms 泵的第一拍）与同步完成（宿主置位），
//!   没有后台轮询；用户也可以点 `dev-sync-refresh-devices`。
//!
//! ## 固定 selector（headless 测试契约，见 `docs/notes/ui-headless-tests.md`）
//!
//! | selector | 元素 |
//! | --- | --- |
//! | `dev-sync-panel` | 面板根（绝对定位覆盖层，不挤压编辑器布局） |
//! | `dev-sync-state` | 状态行（含在途 op / 关闭门禁） |
//! | `dev-sync-relay` | relay 行（只显示公开地址） |
//! | `dev-sync-error` | 最近错误行 |
//! | `dev-sync-notice` | 最近一次按钮操作的结果（排队 / 被拒绝 / 已移除） |
//! | `dev-sync-sync-state` | 持闸同步行（切片 4c-2：空闲 / 已持闸 / N 条变化） |
//! | `dev-sync-create` / `join` / `sync` / `publish` / `stop` | 5 个按钮 |
//! | `dev-sync-devices` | 设备列表区域（含固定警告文案） |
//! | `dev-sync-refresh-devices` | 刷新设备列表按钮 |
//! | `dev-sync-member-<i>` | 第 i 行设备（i 与 `MemberView` 的稳定升序一致） |
//! | `dev-sync-member-<i>-id` | 该行的短设备 id（+ 指纹摘要） |
//! | `dev-sync-member-<i>-status` | 该行的访问状态（含「本机」标记） |
//! | `dev-sync-remove-<i>` | 该行的移除按钮（仅 Active 且非本机时渲染） |
//! | `dev-sync-confirm-revoke` / `dev-sync-cancel-revoke` | 二次确认的确认/取消按钮 |
//!
//! **`dev-sync-sync` / `dev-sync-publish` 不直接对 service 发命令**（切片 4c-2 / 4c-4）：
//! 它们只发 [`DevSyncPanelEvent::SyncRequested`] / [`DevSyncPanelEvent::PublishRequested`]，
//! 由 `AppShell` 走"先持闸、再派发 worker"的状态机（见 `dev_sync::sync_once`）。
//! 直接 `try_command(SyncOnce)` / `try_command(PublishInitialPair)` 会被生产 runner
//! 以"缺持闸证明"拒绝——面板不制造那条假路径。
//!
//! 发布按钮的前置条件只有一条来自 worker 的公开事实：`can_manage`（本机身份有没有管理
//! 密钥）。没有管理密钥（Join 设备）时按钮置灰，点击只给明确原因、**不**发请求——真正
//! 的权威拒绝仍由 worker 重新读身份后给出。
//!
//! selector 只是测试元数据（`debug_assertions + test-support` 下才有记录），业务逻辑
//! **不得**依赖它们。

use std::sync::Arc;
use std::time::Duration;

use gpui::prelude::*;
use gpui::{div, px, Context, IntoElement, MouseButton, Render, SharedString, Window};

use crate::editor::theme;

use menghunote_sync::http_relay::{MemberStatus, MemberView};
use menghunote_sync::DeviceId;

use super::service::{ServiceConfig, SyncService};
use super::state::{short_device_id, Command, OpResult, StateSnapshot, REVOKE_UNCHANGED_MARKER};

/// 面板 → 宿主（`AppShell`）的请求。
///
/// 出现的原因：`SyncOnce` 必须由 UI **先持外部提交闸门**再派发（切片 4c-2），而面板只
/// 认识 `SyncService`。所以「同步」按钮不直接发命令，而是把意图交给宿主；宿主在下一个
/// `dev_sync_pump` 里走完整状态机。
///
/// 事件是**同步**派发的：宿主收到后只置一个本次帧消费的标记（不在事件回调里改面板实体，
/// 避免同一实体重入更新）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DevSyncPanelEvent {
    /// 用户点了「同步」。
    SyncRequested,
    /// 用户点了「发布基线」（切片 4c-4：同样必须由宿主先持闸）。
    PublishRequested,
}

impl gpui::EventEmitter<DevSyncPanelEvent> for DevSyncPanel {}

/// 面板轮询间隔（事件泵；只影响 dev-only 面板，不是业务时钟）。
const PUMP_INTERVAL: Duration = Duration::from_millis(40);
/// 面板宽度（逻辑像素）。
const PANEL_WIDTH: f32 = 300.0;
/// 顶部固定文案：任何 dev 演示都必须一眼看出"这不是生产"。
const BANNER: &str = "DEV ONLY / 非生产 / localhost";

/// 「移除设备（不轮换密钥）」的**固定安全文案**（ADR 0010 §安全边界）。
///
/// 只要界面上出现移除入口（按钮/确认框），这段文字就必须同时可见——它是这项操作的
/// 全部真相：只断 relay 访问，不追回旧数据、不撤销管理材料。
pub const REVOKE_WARNING: &str = "移除设备（不轮换密钥）：仅阻断它对 relay 的访问；\
旧数据/旧 root 仍可能被解密；管理材料不会被撤销；请保留最新恢复码。";

/// 状态标签（面板行内文案；与 [`MemberStatus`] 一一对应）。
fn status_label(status: MemberStatus) -> &'static str {
    match status {
        MemberStatus::Active => "可用",
        MemberStatus::Revoked => "已移除",
        MemberStatus::Pending => "待批准",
        MemberStatus::Expired => "已过期",
    }
}

/// 行内状态文案（含「本机」标记）：纯函数，headless 测试无法读元素文本，靠单测固定。
fn member_status_text(status: MemberStatus, is_self: bool) -> String {
    let mut text = status_label(status).to_owned();
    if is_self {
        text.push_str(" · 本机");
    }
    text
}

/// 移除成功后的固定 notice（含 ADR 0010 要求的两句话）。
fn revoked_notice(target: &DeviceId, result: &OpResult) -> String {
    let short = short_device_id(target);
    match result {
        // `auth_rev` 只在**真变化**时推进（重复移除返回 `changed=false`）：不能一律
        // 宣称"认证已刷新"，否则就是谎报（ADR 0010 幂等语义）。
        OpResult::Ok { summary } if summary.contains(REVOKE_UNCHANGED_MARKER) => {
            format!("已移除 {short}（仅断访问）；此前已撤销，认证未再次推进")
        }
        OpResult::Ok { .. } => {
            format!("已移除 {short}（仅断访问）；认证已刷新，下次同步会重新认证")
        }
        OpResult::Failed { message } => format!("移除 {short} 失败：{message}"),
    }
}

/// dev-only 同步面板。
pub struct DevSyncPanel {
    service: Arc<SyncService>,
    /// relay 公开地址（不含 secret；面板只显示它）。
    relay: String,
    /// 只读状态快照（渲染的唯一数据源）。
    snapshot: StateSnapshot,
    /// 最近一次按钮操作的结果（"已排队 op #3" / "已拒绝：…" / "已移除 …"）。
    notice: Option<String>,
    /// 持闸任务行（切片 4c-2 / 4c-4；由宿主 `AppShell` 写，「同步中/已持闸/N 条变化」）。
    sync_line: Option<String>,
    /// 持闸任务行的前缀（「同步」/「发布」；由宿主按任务种类设置）。
    task_label: &'static str,
    /// 宿主是否正在跑持闸同步（只用于按钮置灰/提示；真正的拒绝仍由宿主给原因）。
    host_busy: bool,
    /// 待二次确认的移除目标（`None` = 没有确认区）。
    pending_revoke: Option<DeviceId>,
    /// 面板已经消费过的移除结果（避免同一事实重复写 notice）。
    seen_revocation: Option<(DeviceId, OpResult)>,
    /// 待办的自动刷新（面板打开 / 同步完成各置一次；由 40 ms 事件泵派发）。
    ///
    /// 为什么不在构造/事件回调里**立刻**发命令：那会和用户/宿主的同步命令抢单操作
    /// 串行位（`SyncOnce` 会被 `Busy` 拒绝）。放到泵里、只在空闲时发，顺序可控。
    refresh_pending: bool,
}

impl DevSyncPanel {
    /// 创建面板并启动事件泵（默认 [`SyncService::new`]，即未实现 runner）。
    ///
    /// 生产路径用 [`Self::with_service`] 注入 [`SyncService::new_dev_local`]（真实
    /// `SyncOnce`）。两者都起专用 OS 线程，面板只持 `Arc`。
    pub fn new(config: ServiceConfig, cx: &mut Context<Self>) -> Self {
        Self::with_service(SyncService::new(config), cx)
    }

    /// 用既有服务创建面板（切片 4c-2：宿主可注入真实 dev-local worker）。
    pub fn with_service(service: Arc<SyncService>, cx: &mut Context<Self>) -> Self {
        let relay = service.config().relay.clone();
        let snapshot = service.snapshot();
        let panel = Self {
            service,
            relay,
            snapshot,
            notice: None,
            sync_line: None,
            task_label: "同步",
            host_busy: false,
            pending_revoke: None,
            seen_revocation: None,
            // 面板打开即读一次设备列表（ADR 0010 切片 2a-2）：由 40 ms 泵在空闲时派发。
            refresh_pending: true,
        };
        panel.spawn_pump(cx);
        panel
    }

    /// 最近一次按钮操作的结果（测试/宿主读取；不含 secret）。
    #[must_use]
    pub fn notice_text(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    /// 持闸同步行文案（`None` = 还没跑过持闸同步）。
    #[must_use]
    pub fn sync_line_text(&self) -> Option<&str> {
        self.sync_line.as_deref()
    }

    /// 当前设备列表快照（测试断言用；只读投影，不含密钥）。
    #[must_use]
    pub fn devices(&self) -> &[MemberView] {
        &self.snapshot.devices
    }

    /// 正在等二次确认的移除目标（`None` = 没有确认区）。
    #[must_use]
    pub fn pending_revoke_target(&self) -> Option<DeviceId> {
        self.pending_revoke
    }

    /// 本机是否有管理密钥（`false` = 移除按钮必须置灰）。
    #[must_use]
    pub fn can_manage_devices(&self) -> bool {
        self.snapshot.can_manage_devices
    }

    /// 宿主更新**同步**的持闸状态（按钮置灰 + 面板行文案）。
    pub fn set_sync_state(&mut self, busy: bool, line: impl Into<String>) {
        self.set_task_state(busy, "同步", line);
    }

    /// 宿主更新**自动同步**的在途状态（同一行，前缀换成「自动同步」）。
    ///
    /// ADR 0012 S3：自动轮与手动轮共用同一个单操作槽，面板必须能一眼看出
    /// "这一轮是自动的"（手动按钮此时只排队，不派发第二条）。
    pub fn set_auto_sync_state(&mut self, busy: bool, line: impl Into<String>) {
        self.set_task_state(busy, "自动同步", line);
    }

    /// 宿主更新**发布基线**的持闸状态（同一行，前缀换成「发布」）。
    pub fn set_publish_state(&mut self, busy: bool, line: impl Into<String>) {
        self.set_task_state(busy, "发布", line);
    }

    fn set_task_state(&mut self, busy: bool, label: &'static str, line: impl Into<String>) {
        self.host_busy = busy;
        self.task_label = label;
        self.sync_line = Some(line.into());
    }

    /// 宿主写一行"最近操作"（同步完成/失败/被拒绝的原因）。
    pub fn set_notice(&mut self, notice: impl Into<String>) {
        self.notice = Some(notice.into());
    }

    /// 请求在下一拍自动刷新一次设备列表（宿主在**同步成功后**调用）。
    pub fn request_device_refresh(&mut self) {
        self.refresh_pending = true;
    }

    /// 面板使用的服务（切片 4 的关闭路径/测试会用到）。
    #[must_use]
    pub fn service(&self) -> &Arc<SyncService> {
        &self.service
    }

    /// 收一次事件并刷新快照；返回快照是否变化（变化才需要 `cx.notify()`）。
    pub fn pump(&mut self) -> bool {
        let events = self.service.poll();
        let snapshot = self.service.snapshot();
        let changed = !events.is_empty() || snapshot != self.snapshot;
        self.snapshot = snapshot;
        // 新的「移除成功」→ 固定 notice（"已移除 <id>（仅断访问）…"）。
        if self.snapshot.last_revoked != self.seen_revocation {
            self.seen_revocation = self.snapshot.last_revoked.clone();
            if let Some((target, result)) = &self.snapshot.last_revoked {
                self.notice = Some(revoked_notice(target, result));
            }
        }
        changed
    }

    /// 空闲时派发一次待办的设备列表刷新（只从 40 ms 泵调用，不在渲染里发命令）。
    fn dispatch_pending_refresh(&mut self) {
        if !self.refresh_pending {
            return;
        }
        // 有在途 op / worker 已停：保留待办，下一拍再试（不与同步抢单操作串行位）。
        if self.snapshot.in_flight.is_some() || self.snapshot.worker_stopped {
            return;
        }
        if self.service.try_command(Command::ListDevices).is_ok() {
            self.refresh_pending = false;
        }
    }

    /// 发一条命令（按钮点击）。失败原因写在面板上，**不吞**。
    ///
    /// `SyncOnce` 例外：它必须由宿主先持闸（切片 4c-2），这里只发一个请求事件。
    /// 「发布基线」走 [`Self::request_publish`]（同样只发请求，4c-4）。
    fn send(&mut self, command: Command, cx: &mut Context<Self>) {
        if command == Command::SyncOnce {
            self.notice = Some("请求同步：由 AppShell 持闸后派发 worker".to_owned());
            cx.emit(DevSyncPanelEvent::SyncRequested);
            cx.notify();
            return;
        }
        self.notice = Some(match self.service.try_command(command) {
            Ok(op_id) => format!("已排队：{} {op_id}", command.kind().label()),
            Err(busy) => format!(
                "已拒绝：{}（{}）",
                command.kind().label(),
                busy.reason.label()
            ),
        });
        // 控制命令（Stop）可能已经回执：立刻收一次，别等下一帧。
        self.pump();
    }

    /// 事件泵：定时收事件（worker 在别的线程里），顺带派发自动刷新。
    fn spawn_pump(&self, cx: &mut Context<Self>) {
        let timers = cx.background_executor().clone();
        cx.spawn(async move |this, cx| loop {
            timers.timer(PUMP_INTERVAL).await;
            let updated = this.update(cx, |panel, cx| {
                panel.dispatch_pending_refresh();
                if panel.pump() {
                    cx.notify();
                }
            });
            if updated.is_err() {
                // 面板已被释放（窗口关闭）：结束循环。
                break;
            }
        })
        .detach();
    }

    /// 状态行文案：状态 + 在途 op + 关闭门禁（只在有关闭流程时显示）。
    fn state_text(&self) -> String {
        let mut text = format!("状态：{}", self.snapshot.state.label());
        if let Some((op_id, kind)) = self.snapshot.in_flight {
            text.push_str(&format!("｜在途 {} {op_id}", kind.label()));
        }
        if let Some((phase, detail)) = &self.snapshot.progress {
            text.push_str(&format!("｜{}：{detail}", phase.label()));
        }
        if self.snapshot.state.label().starts_with("关闭中") || self.snapshot.worker_stopped {
            text.push_str(&format!("｜关闭：{}", self.snapshot.close.label()));
        }
        text
    }

    /// 错误行文案（最近错误；没有就显示"无"）。
    ///
    /// 公开给宿主/测试读取：headless 测试拿不到元素文本，只能从面板投影断言
    /// `dev-sync-error` 会显示什么（失败原因不得只留在快照里）。
    #[must_use]
    pub fn error_text(&self) -> String {
        match &self.snapshot.last_error {
            Some(error) => format!("最近错误：{error}"),
            None => "最近错误：无".to_owned(),
        }
    }

    /// 一个可点击元素（统一样式；`dimmed` 只影响视觉，真正的拒绝由业务逻辑给原因）。
    fn action_button(
        &self,
        selector: impl Into<String>,
        label: impl Into<SharedString>,
        danger: bool,
        dimmed: bool,
        cx: &mut Context<Self>,
        on_click: impl Fn(&mut DevSyncPanel, &mut Context<DevSyncPanel>) + 'static,
    ) -> impl IntoElement {
        let selector = selector.into();
        let text_color = if danger {
            theme::danger_text()
        } else if dimmed {
            theme::status_bar_text()
        } else {
            theme::text_color()
        };
        let debug_name = selector.clone();
        let mut element = div()
            .id(SharedString::from(selector))
            .debug_selector(move || debug_name)
            .px(px(6.0))
            .py(px(3.0))
            .rounded_sm()
            .border_1()
            .border_color(theme::input_border())
            .bg(theme::input_bg())
            .text_size(px(11.0))
            .text_color(text_color)
            .cursor_pointer()
            .child(label.into())
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |panel, _event: &gpui::MouseDownEvent, _window, cx| {
                    on_click(panel, cx);
                    cx.notify();
                    // 面板是覆盖层：别让点击穿透到下面的编辑器。
                    cx.stop_propagation();
                }),
            );
        if dimmed {
            // 淡化但仍可点：点了会拿到明确原因（比彻底 disabled 更可诊断）。
            element = element.opacity(0.6);
        }
        element
    }

    /// 一个命令按钮（点击 → `try_command`）。
    fn button(
        &self,
        selector: &'static str,
        label: &'static str,
        command: Command,
        danger: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        // 视觉上淡化"当前点不动"的操作命令；真正的拒绝仍由 `try_command`/宿主给出原因，
        // 面板不自己发明规则（状态机才是唯一真源）。切片 4c-2 起，持闸同步在途
        // （`host_busy`，此时 service 可能还没 Busy）也算"点不动"。
        let dimmed = (self.snapshot.in_flight.is_some() || self.host_busy) && !command.is_control();
        self.action_button(selector, label, danger, dimmed, cx, move |panel, cx| {
            panel.send(command, cx);
        })
    }

    /// 设备列表区域（ADR 0010 切片 2a-2）：固定警告 + 刷新按钮 + 每台设备一行 + 确认区。
    fn render_devices(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let self_device = self.snapshot.self_device;
        let can_manage = self.snapshot.can_manage_devices;
        let busy = self.snapshot.in_flight.is_some() || self.host_busy;

        let mut list = div().flex().flex_col().gap_1();
        if self.snapshot.devices.is_empty() {
            list = list.child(
                div()
                    .text_size(px(10.0))
                    .text_color(theme::status_bar_text())
                    .child(SharedString::from(
                        "设备列表：尚未读到（点「刷新设备」重试）",
                    )),
            );
        }
        for (index, member) in self.snapshot.devices.iter().enumerate() {
            // 本机标记与移除权限：两者都只来自 worker 上报的公开事实。
            let is_self = self_device == Some(member.device_id);
            let short = short_device_id(&member.device_id);
            let fingerprint = member
                .fingerprint
                .as_ref()
                .map(|fingerprint| {
                    let digest: String = fingerprint.chars().take(8).collect();
                    format!("指纹 {digest}")
                })
                .unwrap_or_else(|| "指纹 —".to_owned());
            let status = member_status_text(member.status, is_self);
            let mut row = div()
                .debug_selector(move || format!("dev-sync-member-{index}"))
                .flex()
                .flex_col()
                .gap_0p5()
                .px(px(4.0))
                .py(px(2.0))
                .border_1()
                .border_color(theme::input_border())
                .rounded_sm()
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap_1()
                        .items_center()
                        .child(
                            div()
                                .debug_selector(move || format!("dev-sync-member-{index}-status"))
                                .text_size(px(11.0))
                                .text_color(theme::text_color())
                                .child(SharedString::from(status)),
                        )
                        .child(
                            div()
                                .debug_selector(move || format!("dev-sync-member-{index}-id"))
                                .text_size(px(10.0))
                                .text_color(theme::status_bar_text())
                                .child(SharedString::from(format!("{short} · {fingerprint}"))),
                        ),
                );
            // 移除按钮：**只**给 Active 且非本机的行渲染。pending/revoked/expired/本机
            // 连按钮都不出现（确认流入口越少越好）。
            if member.status == MemberStatus::Active && !is_self {
                let target = member.device_id;
                // 置灰只是视觉：缺管理密钥/忙时点击会拿到明确原因（比彻底 disabled 更可诊断）。
                let dimmed = !can_manage || busy;
                row = row.child(self.action_button(
                    format!("dev-sync-remove-{index}"),
                    "移除设备（不轮换密钥）",
                    true,
                    dimmed,
                    cx,
                    move |panel, cx| panel.request_revoke(target, cx),
                ));
            }
            list = list.child(row);
        }

        // 二次确认区：显示目标短 id + 同一段固定警告；确认后才真正发命令。
        if let Some(target) = self.pending_revoke {
            let short = short_device_id(&target);
            list = list
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(theme::danger_text())
                        .child(SharedString::from(format!("确认移除 {short}？"))),
                )
                .child(
                    div()
                        .text_size(px(10.0))
                        .text_color(theme::status_bar_text())
                        .child(SharedString::from(REVOKE_WARNING)),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap_1()
                        .child(self.action_button(
                            "dev-sync-confirm-revoke",
                            "确认移除",
                            true,
                            false,
                            cx,
                            move |panel, cx| panel.confirm_revoke(target, cx),
                        ))
                        .child(self.action_button(
                            "dev-sync-cancel-revoke",
                            "取消",
                            false,
                            false,
                            cx,
                            move |panel, _cx| panel.pending_revoke = None,
                        )),
                );
        }

        div()
            .id("dev-sync-devices")
            .debug_selector(|| "dev-sync-devices".to_string())
            .flex()
            .flex_col()
            .gap_1()
            .pt(px(4.0))
            .border_t_1()
            .border_color(theme::sidebar_border())
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(theme::danger_text())
                    .child(SharedString::from(REVOKE_WARNING)),
            )
            .child(
                div()
                    .text_size(px(10.0))
                    .text_color(theme::status_bar_text())
                    .child(SharedString::from(if can_manage {
                        "本机有管理密钥：可以移除其它设备、发布初始基线。"
                    } else {
                        "本机没有管理密钥（Join 设备）：移除设备与发布基线都不可用。"
                    })),
            )
            .child(self.action_button(
                "dev-sync-refresh-devices",
                "刷新设备",
                false,
                false,
                cx,
                |panel, cx| panel.send(Command::ListDevices, cx),
            ))
            .child(list)
    }

    /// 点击「移除」：只在有管理密钥时进入二次确认（否则给明确原因）。
    fn request_revoke(&mut self, target: DeviceId, _cx: &mut Context<Self>) {
        if !self.snapshot.can_manage_devices {
            self.notice = Some(
                "本机没有管理密钥（Join 设备）：无法移除设备；请用 Create 设备或最新恢复码"
                    .to_owned(),
            );
            self.pending_revoke = None;
            return;
        }
        if self.snapshot.self_device == Some(target) {
            // UI 兜底：本机行本来就不渲染按钮，这里防伪造/状态过期的点击。
            self.notice = Some("不能移除本机设备（会立即自断同步与管理访问）".to_owned());
            self.pending_revoke = None;
            return;
        }
        self.pending_revoke = Some(target);
        self.notice = Some(format!(
            "等待确认：移除 {}（仅断访问）",
            short_device_id(&target)
        ));
    }

    /// 点击「发布基线」（切片 4c-4）：**只发请求**，持闸与派发由 `AppShell` 做。
    ///
    /// 三条前置条件都只在面板层做"提前给原因"，不代替 worker 的权威判定：
    /// 1. worker 已停 → 不可能发布；
    /// 2. 已有操作在途（同步/发布/设备命令）或宿主正持闸 → 单操作串行，拒绝；
    /// 3. 还没读到本机身份 / 没有管理密钥（Join 设备）→ 不发请求（按钮也置灰）。
    pub fn request_publish(&mut self, cx: &mut Context<Self>) {
        let reason = if self.snapshot.worker_stopped {
            Some("发布基线：dev-sync worker 已停止（请重启实例）".to_owned())
        } else if self.snapshot.in_flight.is_some() || self.host_busy {
            Some("发布基线：已有操作在途（单操作串行），等面板回到空闲再点".to_owned())
        } else if self.snapshot.self_device.is_none() {
            Some(
                "发布基线：还没有读到本机设备身份（面板打开后会自动刷新设备列表），请稍后再试"
                    .to_owned(),
            )
        } else if !self.snapshot.can_manage_devices {
            Some("发布基线：本机没有管理密钥（Join 设备不能发布）；请用 Create 设备发布".to_owned())
        } else {
            None
        };
        match reason {
            Some(reason) => self.notice = Some(reason),
            None => {
                self.notice = Some("请求发布基线：由 AppShell 持闸后派发 worker".to_owned());
                cx.emit(DevSyncPanelEvent::PublishRequested);
            }
        }
        cx.notify();
    }

    /// 确认移除：清掉确认区并派发命令（成功/失败分别走 notice / error 行）。
    fn confirm_revoke(&mut self, target: DeviceId, cx: &mut Context<Self>) {
        self.pending_revoke = None;
        self.send(Command::RevokeDevice { target }, cx);
    }
}

impl Render for DevSyncPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 每帧先收一次事件：本帧画出来的就是最新状态（不依赖外部 tick 的时序）。
        self.pump();

        let state_text = self.state_text();
        let relay_text = format!("relay：{}", self.relay);
        let error_text = self.error_text();
        let notice_text = match &self.notice {
            Some(notice) => format!("最近操作：{notice}"),
            None => "最近操作：无".to_owned(),
        };
        // 持闸任务行由宿主写"状态 + 变化数"（「已持闸 / 同步中 / 完成（N 条变化）」）；
        // 没跑过持闸任务时显示空闲。详细结果在 `dev-sync-notice`。
        let sync_text = match &self.sync_line {
            Some(line) => format!("{}：{line}", self.task_label),
            None => "同步：空闲".to_owned(),
        };
        let buttons = [
            ("dev-sync-create", "创建", Command::Create, false),
            ("dev-sync-join", "加入", Command::Join, false),
            ("dev-sync-sync", "同步", Command::SyncOnce, false),
        ];
        let mut row = div().flex().flex_row().flex_wrap().gap_1();
        for (selector, label, command, danger) in buttons {
            row = row.child(self.button(selector, label, command, danger, cx));
        }
        // 「发布基线」（切片 4c-4）：**不直接发命令**——与「同步」一样先请宿主持闸。
        // 置灰条件：没有管理密钥（Join 设备）/ 有操作在途 / 宿主正在持闸；
        // 真正的拒绝仍由 worker 读身份后给（面板不发明规则）。
        let publish_dimmed = self.snapshot.in_flight.is_some()
            || self.host_busy
            || !self.snapshot.can_manage_devices;
        row = row.child(self.action_button(
            "dev-sync-publish",
            "发布基线",
            false,
            publish_dimmed,
            cx,
            |panel, cx| panel.request_publish(cx),
        ));
        row = row.child(self.button("dev-sync-stop", "停止", Command::Stop, true, cx));
        let devices = self.render_devices(cx);

        div()
            .id("dev-sync-panel")
            // 测试元数据：面板根（headless 测试断言存在与尺寸）。
            .debug_selector(|| "dev-sync-panel".to_string())
            .absolute()
            .top(px(8.0))
            .right(px(8.0))
            .w(px(PANEL_WIDTH))
            .flex()
            .flex_col()
            .gap_1()
            .p(px(10.0))
            .bg(theme::status_bar_bg())
            .border_1()
            .border_color(theme::sidebar_border())
            .rounded_sm()
            .shadow_lg()
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(theme::danger_text())
                    .child(SharedString::from(BANNER)),
            )
            .child(
                div()
                    .debug_selector(|| "dev-sync-state".to_string())
                    .text_size(px(12.0))
                    .text_color(theme::text_color())
                    .child(SharedString::from(state_text)),
            )
            .child(
                div()
                    .debug_selector(|| "dev-sync-relay".to_string())
                    .text_size(px(11.0))
                    .text_color(theme::status_bar_text())
                    .child(SharedString::from(relay_text)),
            )
            .child(
                div()
                    .debug_selector(|| "dev-sync-error".to_string())
                    .text_size(px(11.0))
                    .text_color(theme::status_bar_text())
                    .child(SharedString::from(error_text)),
            )
            .child(
                div()
                    .debug_selector(|| "dev-sync-notice".to_string())
                    .text_size(px(11.0))
                    .text_color(theme::status_bar_text())
                    .child(SharedString::from(notice_text)),
            )
            .child(
                div()
                    // 切片 4c-2：持闸同步行（空闲 / 已持闸 / 完成 N 条变化）。
                    .debug_selector(|| "dev-sync-sync-state".to_string())
                    .text_size(px(11.0))
                    .text_color(theme::status_bar_text())
                    .child(SharedString::from(sync_text)),
            )
            .child(row)
            .child(devices)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dev_sync::state::{CloseState, DevSyncState};

    /// `state_text`/`error_text` 是纯投影：只验"该显示的都在"，不依赖 GPUI 上下文。
    #[test]
    fn state_text_projects_busy_progress_and_close() {
        let config = ServiceConfig::new("http://127.0.0.1:18080");
        // 不启动 worker 的轻量构造：本测试只读投影函数，不需要事件泵。
        let service = SyncService::new(config.clone());
        let mut panel = DevSyncPanel {
            service,
            relay: config.relay.clone(),
            snapshot: StateSnapshot {
                state: DevSyncState::Busy {
                    op_id: super::super::state::OpId::new(3),
                    kind: super::super::state::CommandKind::SyncOnce,
                },
                in_flight: Some((
                    super::super::state::OpId::new(3),
                    super::super::state::CommandKind::SyncOnce,
                )),
                progress: Some((super::super::state::Phase::Pushing, "推送 2 条".to_owned())),
                last_completed: None,
                last_error: None,
                close: CloseState::Pending,
                worker_stopped: false,
                deadline_exceeded: None,
                cancel_requested: false,
                two_phase: None,
                devices: Vec::new(),
                self_device: None,
                can_manage_devices: false,
                last_revoked: None,
            },
            notice: None,
            sync_line: None,
            task_label: "同步",
            host_busy: false,
            pending_revoke: None,
            seen_revocation: None,
            refresh_pending: false,
        };
        let text = panel.state_text();
        assert!(text.contains("执行中"), "必须显示状态：{text}");
        assert!(text.contains("#3"), "必须显示在途 op：{text}");
        assert!(text.contains("推送 2 条"), "必须显示最新进度：{text}");
        assert_eq!(panel.error_text(), "最近错误：无");
        panel.snapshot.last_error = Some("切片 4 未实现".to_owned());
        assert_eq!(panel.error_text(), "最近错误：切片 4 未实现");
        // 关闭中要显示关闭门禁（Blocked 的原因必须能看见）。
        panel.snapshot.state = DevSyncState::Closing;
        panel.snapshot.close = CloseState::Blocked("durable 未确认".to_owned());
        let text = panel.state_text();
        assert!(text.contains("关闭"), "关闭中必须显示关闭行：{text}");
        assert!(text.contains("durable 未确认"), "阻断原因必须可见：{text}");
    }

    /// 本机行必须显示「本机」；其它行不显示（headless 只能断言 selector 存在，文本靠这里固定）。
    #[test]
    fn member_status_text_marks_the_local_device() {
        assert_eq!(member_status_text(MemberStatus::Active, false), "可用");
        assert_eq!(
            member_status_text(MemberStatus::Active, true),
            "可用 · 本机"
        );
        for status in [
            MemberStatus::Revoked,
            MemberStatus::Pending,
            MemberStatus::Expired,
        ] {
            let text = member_status_text(status, false);
            assert!(!text.contains("本机"), "{text}");
        }
    }

    /// 固定警告文案必须一字不漏地包含 ADR 0010 的四条边界。
    #[test]
    fn revoke_warning_covers_the_adr_boundaries() {
        for needle in [
            "移除设备（不轮换密钥）",
            "仅阻断它对 relay 的访问",
            "旧数据/旧 root 仍可能被解密",
            "管理材料不会被撤销",
            "请保留最新恢复码",
        ] {
            assert!(
                REVOKE_WARNING.contains(needle),
                "警告文案必须包含 {needle:?}：{REVOKE_WARNING}"
            );
        }
    }

    /// 移除成功的 notice 必须说清"仅断访问"与"认证已刷新"，且重复移除不谎报。
    #[test]
    fn revoked_notice_states_access_only_and_auth_refresh() {
        let target = [0xab; 16];
        let notice = revoked_notice(
            &target,
            &OpResult::Ok {
                summary: "已移除 abababab（仅断访问；auth_rev 7）".to_owned(),
            },
        );
        assert!(notice.contains("已移除 abababab"), "{notice}");
        assert!(notice.contains("仅断访问"), "{notice}");
        assert!(notice.contains("认证已刷新"), "{notice}");

        let repeat = revoked_notice(
            &target,
            &OpResult::Ok {
                summary: format!("abababab 此前已是 revoked（{}）", REVOKE_UNCHANGED_MARKER),
            },
        );
        assert!(repeat.contains("仅断访问"), "{repeat}");
        assert!(
            !repeat.contains("认证已刷新"),
            "重复移除不得谎称认证已刷新：{repeat}"
        );
    }
}
