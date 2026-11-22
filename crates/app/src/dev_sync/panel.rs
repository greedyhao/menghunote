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
//! ## 轮换密钥 / 应用轮换（ADR 0009 Phase 2 切片 6b）
//!
//! 设备列表区域里有两个**独立**入口——移除设备只删凭证（ADR 0010 默认），轮换只有用户
//! 显式点这里才会发生，两者不自动串联：
//!
//! - `dev-sync-rotate`「轮换密钥…」：仅 creator 可用（`can_manage` + 有活动文档 + 无在途）。
//!   点它**只展开二次确认**（固定文案 [`ROTATE_CONFIRM_TEXT`]）；确认后才把
//!   [`RotateRequest`]（恢复码 + revoke 列表）交给宿主持闸派发。
//! - `dev-sync-apply-epoch`「应用轮换」：非 manager（Join/B 侧）的主动方；同样先展开
//!   确认（[`APPLY_CONFIRM_TEXT`]）再请求派发。
//!
//! 选项：
//!
//! | 控件 | 默认 | 语义 |
//! | --- | --- | --- |
//! | `dev-sync-rotate-reissue` | 勾选 | 用当前恢复码重封候选 epoch 的 recovery 槽；未勾选时显示固定警告 [`ROTATE_NO_RECOVERY_WARNING`] |
//! | `dev-sync-rotate-recovery` | 空 | **秘密输入**（遮罩、允许粘贴、不向平台报告正文）；提交后立刻清空，只在请求里留一份 `Zeroizing` |
//! | `dev-sync-rotate-revoke` | 勾选 | 把**已移除**设备写进 signed revoke 列表；UI 不手选设备（按 `status=revoked` 过滤 members） |
//!
//! 不可用时不隐藏按钮，而是置灰 + 一行明确原因（[`DevSyncPanel::rotate_block_reason`] /
//! [`DevSyncPanel::apply_block_reason`]）：无管理密钥、无活动文档、单操作在途、worker 已停。
//! 轮换在途时阶段文案固定为 [`ROTATE_STAGES_TEXT`] + worker 的粗粒度明细（另一行
//! `dev-sync-state` 仍显示 `Phase`），结束后不再显示。
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
//! | `dev-sync-sync` / `stop` | 「同步」「停止」按钮（顶部不再有「创建」「加入」：真实入口在 AppRoot Welcome） |
//! | `dev-sync-publish` | 「重试发布基线」按钮（**只在**本机有管理钥且 durable 基线缺失/未知为缺失时渲染；正常隐藏） |
//! | `dev-sync-devices` | 设备列表区域（含固定警告文案） |
//! | `dev-sync-refresh-devices` | 刷新设备列表按钮 |
//! | `dev-sync-member-<i>` | 第 i 行设备（i 与 `MemberView` 的稳定升序一致） |
//! | `dev-sync-member-<i>-id` | 该行的短设备 id（+ 指纹摘要） |
//! | `dev-sync-member-<i>-status` | 该行的访问状态（含「本机」标记） |
//! | `dev-sync-remove-<i>` | 该行的移除按钮（仅 Active 且非本机时渲染） |
//! | `dev-sync-confirm-revoke` / `dev-sync-cancel-revoke` | 二次确认的确认/取消按钮 |
//! | `dev-sync-invite` | 「邀请设备…」按钮（ADR 0014 S1 / 0017 S5b；无管理钥/无基线/忙时置灰并说明；**不要求活动文档**） |
//! | `dev-sync-invite-ttl` | 邀请 TTL 文案（公开；不含 token） |
//! | `dev-sync-copy-invite` | 「复制邀请链接」（显式动作，不自动复制；**链接明文不渲染**） |
//! | `dev-sync-invite-copied` | 复制后的折叠行（「已复制；关闭面板即失效」；不再展示链接/复制按钮） |
//! | `dev-sync-approve-<短id>` | pending 行的「批准」按钮（仅管理设备渲染） |
//! | `dev-sync-approve-confirm` / `dev-sync-approve-cancel` | 批准确认区的确认/取消 |
//! | `dev-sync-approve-warning` | 批准确认区：目标 device_id + sign/hpke 短 hash（双端比对） |
//! | `dev-sync-rotation` | 轮换/应用轮换区域（设备列表区域的一部分） |
//! | `dev-sync-rotate` / `dev-sync-apply-epoch` | 「轮换密钥…」/「应用轮换」按钮（不可用时置灰） |
//! | `dev-sync-rotate-confirm` / `dev-sync-rotate-cancel` | 轮换确认区的确认/取消 |
//! | `dev-sync-rotate-reissue` / `dev-sync-rotate-revoke` | 恢复码重封装 / 写入签名撤销列表复选框 |
//! | `dev-sync-rotate-recovery` | 恢复码**秘密**输入（遮罩；仅勾选重封装时渲染） |
//! | `dev-sync-rotate-warning` | 轮换确认区的固定警告行 |
//! | `dev-sync-apply-confirm` / `dev-sync-apply-cancel` | 应用新 epoch 确认区的确认/取消 |
//!
//! 邀请链接**永不**以明文进入布局：URL/token 只存在 worker 内存、`InviteLinkView`
//! 与点击复制那一刻的剪贴板；复制成功后邀请块折叠（`dev-sync-invite-copied`）。上一版
//! 会把 `mn-pair://join?...` 整段画在面板上（截图/录屏就泄露 token），S5b 之后不再这样。
//!
//! **`dev-sync-sync` / `dev-sync-publish` 不直接对 service 发命令**（切片 4c-2 / 4c-4）：
//! 它们只发 [`DevSyncPanelEvent::SyncRequested`] / [`DevSyncPanelEvent::PublishRequested`]，
//! 由 `AppShell` 走"先持闸、再派发 worker"的状态机（见 `dev_sync::sync_once`）。
//! 直接 `try_command(SyncOnce)` / `try_command(PublishInitialPair)` 会被生产 runner
//! 以"缺持闸证明"拒绝——面板不制造那条假路径。
//!
//! 「重试发布基线」只在两条 worker 公开事实同时成立时**才渲染**：
//! `can_manage`（本机有管理密钥）与"durable 初始基线**已知缺失**"（`baseline_known &&
//! published_baseline.is_none()`）。Create 已自动发布基线的正常实例不显示该按钮；Join
//! 设备也不显示。按钮仍只是请求（[`DevSyncPanelEvent::PublishRequested`]），真正的
//! 权威拒绝由 AppShell/worker 给出；界面不可用时的原因仍写进 `dev-sync-notice`。
//!
//! **轮换/应用新 epoch 也一样不直接发命令**：确认后只登记请求并发
//! [`DevSyncPanelEvent::RotateRequested`] / [`DevSyncPanelEvent::ApplyEpochRequested`]；
//! 宿主用 [`DevSyncPanel::take_rotate_request`] 取走恢复码（秘密不进事件），再走 6a 的
//! 持闸状态机。轮换期间输入冻结、自动同步暂停，按钮一律置灰。
//!
//! selector 只是测试元数据（`debug_assertions + test-support` 下才有记录），业务逻辑
//! **不得**依赖它们。

use std::sync::Arc;
use std::time::Duration;

use gpui::prelude::*;
use gpui::{
    div, px, App, ClipboardItem, Context, Entity, IntoElement, MouseButton, Render, SharedString,
    Window,
};

use crate::editor::theme;
use crate::onboarding::input::FormInput;

use menghunote_sync::http_relay::{MemberStatus, MemberView};
use menghunote_sync::DeviceId;

use super::service::{ServiceConfig, SyncService};
use super::state::{
    short_device_id, Command, InviteLinkView, OpResult, StateSnapshot, REVOKE_UNCHANGED_MARKER,
};
use super::sync_once::RotateRequest;

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
    /// 用户在轮换确认区点了「确认轮换」（切片 6b）：请求已登记在面板里，宿主用
    /// [`DevSyncPanel::take_rotate_request`] 取走（恢复码/revoke 列表不在事件里，
    /// 避免事件被 Debug/日志带走秘密）。
    RotateRequested,
    /// 用户在「应用轮换」确认区点了确认（切片 6b）。
    ApplyEpochRequested,
}

impl gpui::EventEmitter<DevSyncPanelEvent> for DevSyncPanel {}

/// 面板轮询间隔（事件泵；只影响 dev-only 面板，不是业务时钟）。
const PUMP_INTERVAL: Duration = Duration::from_millis(40);
/// 面板宽度（逻辑像素）。
/// 顶部固定文案：任何 dev 演示都必须一眼看出"这不是生产"。
const BANNER: &str = "DEV ONLY / 非生产 / localhost";

/// 轮换确认区的**固定文案**（ADR 0009 Phase 2 切片 6b；一字不改）。
///
/// 它把这次操作的四面真相一次说完：重新加密整库、两份候选基线、旧 epoch 谁还能解、
/// 以及操作期间的输入冻结。任何 UI 改动都不得删句。
pub const ROTATE_CONFIRM_TEXT: &str = "轮换会用新密钥重新加密整库，并生成两份候选基线；\
旧 epoch 数据不能再被新设备解密；被移除设备仍可解其已获得的数据；轮换期间输入会冻结。是否继续？";

/// 未勾选「重新封装恢复码」时的**固定警告**（ADR 0009：`None` 不补发）。
pub const ROTATE_NO_RECOVERY_WARNING: &str = "轮换后新设备将无法用恢复码加入";

/// 已勾选「重新封装恢复码」但输入框为空时的提示（确认会被阻止，不发半截请求）。
pub const ROTATE_RECOVERY_EMPTY_WARNING: &str =
    "已勾选重新封装恢复码：请粘贴/输入当前恢复码（空则不提交轮换）";

/// 「应用轮换」（应用新 epoch）确认区的**固定文案**。
pub const APPLY_CONFIRM_TEXT: &str =
    "应用新 epoch 需要重新验证授权并导入基线；期间输入会冻结。是否继续？";

/// 轮换进行中的阶段文案（ADR 0009 Phase 2：申请 → 候选基线 → 激活 → 推广）。
///
/// 四段是**库级轮换的固定语义**；worker 的粗粒度 `Progress` 明细跟在后面（真正的
/// 当前步骤），另一行 `dev-sync-state` 仍显示 `Phase` + 明细，不会把"轮换中"画成完成。
pub const ROTATE_STAGES_TEXT: &str = "轮换中（申请/候选/激活/推广）";

/// 轮换确认区里恢复码输入框的 placeholder（不含任何真实恢复码）。
const RECOVERY_PLACEHOLDER: &str = "当前恢复码 mnrec1…（只用于本次重封）";

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

/// 批准成功/幂等的固定 notice（ADR 0014 S1）。
fn approved_notice(target: &DeviceId, fingerprint: &str, changed: bool) -> String {
    let short = short_device_id(target);
    if changed {
        format!("已批准 {short}（指纹 {fingerprint}）：请在加入端点「检查状态」导入基线")
    } else {
        format!("{short} 此前已批准（幂等；未重复签名，指纹 {fingerprint}）")
    }
}

/// 状态行文案：状态 + 在途 op + 关闭门禁（只在有关闭流程时显示）。
///
/// 纯投影（只读 [`StateSnapshot`]）：不依赖 GPUI 上下文，单元测试可以直接断言。
#[must_use]
pub fn state_text(snapshot: &StateSnapshot) -> String {
    let mut text = format!("状态：{}", snapshot.state.label());
    if let Some((op_id, kind)) = snapshot.in_flight {
        text.push_str(&format!("｜在途 {} {op_id}", kind.label()));
    }
    if let Some((phase, detail)) = &snapshot.progress {
        text.push_str(&format!("｜{}：{detail}", phase.label()));
    }
    if snapshot.state.label().starts_with("关闭中") || snapshot.worker_stopped {
        text.push_str(&format!("｜关闭：{}", snapshot.close.label()));
    }
    text
}

/// 错误行文案（最近错误；没有就显示"无"）。
///
/// 纯投影：headless 测试拿不到元素文本，只能从面板投影断言 `dev-sync-error` 会显示什么
/// （失败原因不得只留在快照里）。
#[must_use]
pub fn error_text(snapshot: &StateSnapshot) -> String {
    match &snapshot.last_error {
        Some(error) => format!("最近错误：{error}"),
        None => "最近错误：无".to_owned(),
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
    /// 待二次确认的批准目标（`None` = 没有批准确认区；ADR 0014 S1 §3）。
    pending_approve: Option<DeviceId>,
    /// 已消费过的批准事实（避免同一事实重复写 notice）。
    seen_approval: Option<(DeviceId, String, bool)>,
    /// 面板已经消费过的移除结果（避免同一事实重复写 notice）。
    seen_revocation: Option<(DeviceId, OpResult)>,
    /// 宿主视图里当前有没有活动文档（切片 6b：轮换/apply 必须在活动文档上持闸）。
    ///
    /// 这是**宿主事实**，面板自己不推断：`AppShell` 在打开/关闭文档与每次泵时写入。
    has_active_document: bool,
    /// 轮换确认区是否展开（点击只展开，不发命令）。
    pending_rotate_confirm: bool,
    /// 「应用轮换」确认区是否展开。
    pending_apply_confirm: bool,
    /// 是否重新封装恢复码（默认勾选 = ADR 0009 推荐路径）。
    rotate_reissue: bool,
    /// 是否把已移除设备写入签名撤销列表（ADR 0010：默认勾选，UI 不手选设备）。
    rotate_revoke: bool,
    /// 恢复码输入（秘密字段：遮罩 + 允许粘贴 + 不报告平台正文）。
    recovery_input: Entity<FormInput>,
    /// 已登记、等宿主取走的轮换请求（恢复码在这里，不进事件/Debug/日志）。
    pending_rotate_request: Option<RotateRequest>,
    /// 待办的自动刷新（面板打开 / 同步完成各置一次；由 40 ms 事件泵派发）。
    ///
    /// 为什么不在构造/事件回调里**立刻**发命令：那会和用户/宿主的同步命令抢单操作
    /// 串行位（`SyncOnce` 会被 `Busy` 拒绝）。放到泵里、只在空闲时发，顺序可控。
    refresh_pending: bool,
    /// 当前邀请链接是否已复制过：复制成功后邀请块折叠为「已复制；关闭面板即失效」，
    /// 不再展示链接/复制按钮（再生成走「邀请设备…」）。
    invite_copied: bool,
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
        // 恢复码输入是**秘密字段**：遮罩、允许粘贴、不向平台报告正文（ADR 0011 的
        // `FormInput`）。它注册成独立实体，面板只持有句柄，提交后立刻 `clear()`。
        let recovery_input = cx.new(|cx| {
            FormInput::new(RECOVERY_PLACEHOLDER, true, cx)
                .selector("dev-sync-rotate-recovery")
                .click_to_focus()
        });
        let panel = Self {
            service,
            relay,
            snapshot,
            notice: None,
            sync_line: None,
            task_label: "同步",
            host_busy: false,
            pending_revoke: None,
            pending_approve: None,
            seen_approval: None,
            seen_revocation: None,
            has_active_document: false,
            pending_rotate_confirm: false,
            pending_apply_confirm: false,
            rotate_reissue: true,
            rotate_revoke: true,
            recovery_input,
            pending_rotate_request: None,
            // 面板打开即读一次设备列表（ADR 0010 切片 2a-2）：由 40 ms 泵在空闲时派发。
            refresh_pending: true,
            invite_copied: false,
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

    /// 本机设备 id（`None` = worker 还没上报身份；测试用来区分"本机行"）。
    #[must_use]
    pub fn self_device(&self) -> Option<DeviceId> {
        self.snapshot.self_device
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

    /// 宿主更新**轮换密钥**的持闸状态（同一行，前缀「轮换」；切片 6a）。
    ///
    /// 轮换是长操作（同步+重加密+activate+重包）：面板必须能显示「轮换中」，
    /// 而不是只转个空圈（ADR 0009 Phase 2）。
    pub fn set_rotate_state(&mut self, busy: bool, line: impl Into<String>) {
        self.set_task_state(busy, "轮换", line);
    }

    /// 宿主更新**应用新 epoch**的持闸状态（同一行，前缀「应用 epoch」；切片 6a）。
    pub fn set_apply_epoch_state(&mut self, busy: bool, line: impl Into<String>) {
        self.set_task_state(busy, "应用 epoch", line);
    }

    fn set_task_state(&mut self, busy: bool, label: &'static str, line: impl Into<String>) {
        self.host_busy = busy;
        self.task_label = label;
        self.sync_line = Some(line.into());
    }

    /// 宿主写入"当前有没有活动文档"这个事实（切片 6b：轮换/apply 的硬前置条件）。
    ///
    /// 返回是否变化（调用方据此决定要不要 `cx.notify()`）。
    pub fn set_active_document(&mut self, active: bool) -> bool {
        if self.has_active_document == active {
            return false;
        }
        self.has_active_document = active;
        // 文档没了：展开中的轮换确认区必须收起（否则会留下一个提交必然被拒的表单）。
        if !active {
            self.pending_rotate_confirm = false;
            self.pending_apply_confirm = false;
        }
        true
    }

    /// 当前有没有活动文档（宿主写入的事实）。
    #[must_use]
    pub const fn has_active_document(&self) -> bool {
        self.has_active_document
    }

    /// 轮换按钮**为什么不可用**（`None` = 可用）。面板只做"提前给原因"，
    /// 权威判定仍在 worker（重读身份/库/闸门）。
    #[must_use]
    pub fn rotate_block_reason(&self) -> Option<&'static str> {
        if self.snapshot.worker_stopped {
            return Some("dev-sync worker 已停止（请重启实例）");
        }
        if self.snapshot.in_flight.is_some() || self.host_busy {
            return Some("已有操作在途（单操作串行），等面板回到空闲再点");
        }
        if self.snapshot.self_device.is_none() {
            return Some("还没有读到本机设备身份（面板打开后会自动刷新设备列表），请稍后再试");
        }
        if !self.snapshot.can_manage_devices {
            return Some("本机没有管理密钥（Join 设备不能发起轮换）；请用 Create 设备发起");
        }
        if !self.has_active_document {
            return Some(
                "当前没有打开的笔记；轮换需要先在活动文档上持闸冻结（请先打开一篇笔记再试）",
            );
        }
        None
    }

    /// 「应用轮换」为什么不可用（`None` = 可用）。
    ///
    /// 与轮换不同：**不要求管理密钥**（B/Join 设备正是这一侧的主动方）；只要求 worker
    /// 活着、单操作槽空闲、已经读到本机身份、并且有活动文档可以持闸。
    #[must_use]
    pub fn apply_block_reason(&self) -> Option<&'static str> {
        if self.snapshot.worker_stopped {
            return Some("dev-sync worker 已停止（请重启实例）");
        }
        if self.snapshot.in_flight.is_some() || self.host_busy {
            return Some("已有操作在途（单操作串行），等面板回到空闲再点");
        }
        if self.snapshot.self_device.is_none() {
            return Some("还没有读到本机设备身份（面板打开后会自动刷新设备列表），请稍后再试");
        }
        if !self.has_active_document {
            return Some(
                "当前没有打开的笔记；apply 需要先在活动文档上持闸冻结（请先打开一篇笔记再试）",
            );
        }
        None
    }

    /// 轮换确认区是否展开。
    #[must_use]
    pub const fn rotate_confirm_open(&self) -> bool {
        self.pending_rotate_confirm
    }

    /// 「应用轮换」确认区是否展开。
    #[must_use]
    pub const fn apply_confirm_open(&self) -> bool {
        self.pending_apply_confirm
    }

    /// 「重新封装恢复码」是否勾选（默认 `true`）。
    #[must_use]
    pub const fn rotate_reissue_enabled(&self) -> bool {
        self.rotate_reissue
    }

    /// 「写入签名撤销列表」是否勾选（默认 `true`，ADR 0010）。
    #[must_use]
    pub const fn rotate_revoke_enabled(&self) -> bool {
        self.rotate_revoke
    }

    /// 恢复码输入框里的**长度**（测试断言"已粘贴/已清空"；**不暴露正文**）。
    #[must_use]
    pub fn rotate_recovery_len(&self, cx: &App) -> usize {
        self.recovery_input.read(cx).text().len()
    }

    /// 恢复码输入框是否秘密字段（遮罩 + 不向平台报告正文）。
    #[must_use]
    pub fn rotate_recovery_is_secret(&self, cx: &App) -> bool {
        self.recovery_input.read(cx).is_secret()
    }

    /// 轮换确认区当前应显示的**固定警告**（`None` = 没有警告）。
    ///
    /// 两种来源：未勾选重封装（固定文案）与勾选但输入为空（提交会被阻止）。
    #[must_use]
    pub fn rotate_warning_text(&self, cx: &App) -> Option<&'static str> {
        if !self.rotate_reissue {
            return Some(ROTATE_NO_RECOVERY_WARNING);
        }
        if self.rotate_recovery_len(cx) == 0 {
            return Some(ROTATE_RECOVERY_EMPTY_WARNING);
        }
        None
    }

    /// 轮换进行中的阶段文案（`None` = 当前不在轮换）。
    #[must_use]
    pub fn rotation_stage_text(&self) -> Option<String> {
        if !self.host_busy || self.task_label != "轮换" {
            return None;
        }
        let detail = self.sync_line.as_deref().unwrap_or("等待 worker");
        Some(format!("{ROTATE_STAGES_TEXT}：{detail}"))
    }

    /// 取走已登记的轮换请求（宿主在 `dev_sync_pump` 里调用；**只取一次**）。
    ///
    /// 恢复码随请求一起移交给宿主 → `Command::Rotate`，面板不再留副本。
    pub fn take_rotate_request(&mut self) -> Option<RotateRequest> {
        self.pending_rotate_request.take()
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
        if self.snapshot.last_approved != self.seen_approval {
            self.seen_approval = self.snapshot.last_approved.clone();
            if let Some((target, fingerprint, changed)) = &self.snapshot.last_approved {
                self.notice = Some(approved_notice(target, fingerprint, *changed));
            }
        }
        changed
    }

    /// 空闲时派发一次待办的设备列表刷新（只从 40 ms 泵调用，不在渲染里发命令）。
    fn dispatch_pending_refresh(&mut self) {
        if !self.refresh_pending {
            return;
        }
        // 确认区展开时不要抢单操作槽：用户正在填恢复码/做决定，此时插一条刷新会让
        // 「确认轮换」立刻变成"已有操作在途"。等确认区收起再刷新（待办不丢）。
        if self.pending_rotate_confirm || self.pending_apply_confirm {
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
        if matches!(&command, Command::SyncOnce) {
            self.notice = Some("请求同步：由 AppShell 持闸后派发 worker".to_owned());
            cx.emit(DevSyncPanelEvent::SyncRequested);
            cx.notify();
            return;
        }
        // `Command` 不再 `Copy`（`Rotate` 带秘密/列表）：先取标签，再移动。
        let kind = command.kind();
        self.notice = Some(match self.service.try_command(command) {
            Ok(op_id) => format!("已排队：{} {op_id}", kind.label()),
            Err(busy) => format!("已拒绝：{}（{}）", kind.label(), busy.reason.label()),
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
        state_text(&self.snapshot)
    }

    /// 错误行文案（最近错误；没有就显示"无"）。
    ///
    /// 公开给宿主/测试读取：headless 测试拿不到元素文本，只能从面板投影断言
    /// `dev-sync-error` 会显示什么（失败原因不得只留在快照里）。
    #[must_use]
    pub fn error_text(&self) -> String {
        error_text(&self.snapshot)
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
        // `Command` 不再 `Copy`：用一次性槽把它交给点击回调（同一个按钮连点两次时第二次
        // 无命令可发，面板已经忙/拒绝，语义安全）。
        let command = std::rc::Rc::new(std::cell::RefCell::new(Some(command)));
        self.action_button(selector, label, danger, dimmed, cx, move |panel, cx| {
            if let Some(command) = command.borrow_mut().take() {
                panel.send(command, cx);
            }
        })
    }

    /// 当前邀请链接（`None` = 没生成过；token 只在内存）。
    #[must_use]
    pub fn invite_link(&self) -> Option<&InviteLinkView> {
        self.snapshot.invite_link.as_ref()
    }

    /// 正在等确认的批准目标（`None` = 没有确认区）。
    #[must_use]
    pub fn pending_approve_target(&self) -> Option<DeviceId> {
        self.pending_approve
    }

    /// 「邀请设备…」为什么不可用（`None` = 可用）。
    ///
    /// ADR 0014 S1 / ADR 0017 S5b：无管理钥 / 有在途操作 / 宿主持闸忙 都禁用并说明；
    /// **不要求活动文档**——邀请是管理操作（vault + 身份 + 管理私钥即可），刚创建、
    /// 还没打开/发布任何笔记的实例也必须能直接复制邀请链接给其他设备。
    #[must_use]
    pub fn invite_block_reason(&self) -> Option<&'static str> {
        if self.snapshot.worker_stopped {
            return Some("worker 已停止：不能生成邀请");
        }
        if !self.snapshot.can_manage_devices {
            return Some("本机没有管理密钥（Join/邀请加入的设备）：不能生成邀请链接");
        }
        // 本地前置检查（与 worker 同一条 durable 事实）：没有基线时链接会让加入端在
        // bootstrap 阶段撞 NO_VALID_BASELINE；面板提前给原因，worker 仍会再兜一次。
        if self.snapshot.baseline_known && self.snapshot.published_baseline.is_none() {
            return Some(
                "创建端尚未发布初始基线（Create 会自动发布，失败可重试）：请先在同步面板点「重试发布基线」，成功前邀请链接不会生成",
            );
        }
        if self.snapshot.in_flight.is_some() || self.host_busy {
            return Some("有操作在途：等收尾后再生成邀请");
        }
        if self.snapshot.state.is_busy() {
            return Some("同步/轮换在途：等收尾后再生成邀请");
        }
        None
    }

    /// 批准确认区的文案（目标 device_id + sign/hpke 短 hash；双端人工比对用）。
    ///
    /// headless 测试拿不到元素文本，靠这个纯投影断言"双端指纹"确实显示了。
    #[must_use]
    pub fn approve_confirm_text(&self) -> Option<String> {
        let target = self.pending_approve?;
        let member = self
            .snapshot
            .devices
            .iter()
            .find(|member| member.device_id == target)?;
        Some(format!(
            "批准设备 {}｜sign {}｜hpke {}（请与加入端显示的短 hash 逐字核对）",
            menghunote_sync::ids::hex_of(&target),
            menghunote_sync::invite_link::key_short_hash(&member.sign_pk),
            menghunote_sync::invite_link::key_short_hash(&member.hpke_pk)
        ))
    }

    /// 点击「邀请设备…」：直接派发 `CreateInvite`（单操作串行，结果经事件回面板）。
    fn request_invite(&mut self, cx: &mut Context<Self>) {
        if let Some(reason) = self.invite_block_reason() {
            self.notice = Some(format!("未生成邀请：{reason}"));
            cx.notify();
            return;
        }
        // 新链接替换旧链接（旧 token 不再展示，避免复制到已过期的那个）；折叠态复位。
        self.invite_copied = false;
        self.send(Command::CreateInvite, cx);
    }

    /// 「复制链接」：显式动作（不自动复制），并把风险提示写进 notice。
    fn copy_invite_link(&mut self, cx: &mut Context<Self>) {
        let Some(url) = self
            .snapshot
            .invite_link
            .as_ref()
            .map(|link| link.url.clone())
        else {
            self.notice = Some("还没有可复制的邀请链接：请先点「邀请设备…」".to_owned());
            cx.notify();
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(url));
        // 复制成功后折叠邀请块：链接不再留在布局里（截图/录屏也不会泄露 token）。
        self.invite_copied = true;
        self.notice = Some(
            "已复制邀请链接（含一次性 token，默认 15 分钟内有效）：只私发给要加入的那台设备；\
             不要截图/公开发布，剪贴板历史可能保留副本。面板已隐藏链接，关闭面板即失效"
                .to_owned(),
        );
        cx.notify();
    }

    /// 点击 pending 行的「批准」：只展开确认区（先核对双端指纹），不发命令。
    fn request_approve(&mut self, target: DeviceId, cx: &mut Context<Self>) {
        if !self.snapshot.can_manage_devices {
            self.notice = Some("未展开批准：本机没有管理密钥".to_owned());
            cx.notify();
            return;
        }
        self.pending_approve = Some(target);
        self.pending_revoke = None;
        self.notice = Some(
            "等待确认批准：请与加入端核对设备 id 与 sign/hpke 短 hash，再点「确认批准」".to_owned(),
        );
        cx.notify();
    }

    /// 确认批准：清掉确认区并派发命令（失败不清 pending、不重签）。
    fn confirm_approve(&mut self, cx: &mut Context<Self>) {
        let Some(target) = self.pending_approve.take() else {
            return;
        };
        self.send(Command::ApproveDevice { device: target }, cx);
    }

    /// 设备列表区域（ADR 0010 切片 2a-2）：固定警告 + 刷新按钮 + 每台设备一行 + 确认区。
    ///
    /// ADR 0014 S1 把邀请/批准入口也放在这里（同一份成员视图，pending 行才渲染批准）。
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
            // 批准按钮：**只**给 Pending 行、且本机有管理密钥时渲染（ADR 0014 S1；
            // 非管理设备连入口都不出现）。
            if member.status == MemberStatus::Pending && can_manage {
                let target = member.device_id;
                let selector = format!("dev-sync-approve-{short}");
                let dimmed = busy || self.snapshot.state.is_busy();
                row = row.child(self.action_button(
                    selector,
                    "批准…",
                    false,
                    dimmed,
                    cx,
                    move |panel, cx| panel.request_approve(target, cx),
                ));
            }
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

        // 批准确认区：先展示目标 device_id 与双端可核对的短 hash，确认后才发命令。
        if let Some(text) = self.approve_confirm_text() {
            let target = self.pending_approve;
            list = list
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(theme::text_color())
                        .child(SharedString::from("确认批准这台设备？")),
                )
                .child(
                    div()
                        .debug_selector(|| "dev-sync-approve-warning".to_string())
                        .text_size(px(10.0))
                        .text_color(theme::status_bar_text())
                        .child(SharedString::from(text)),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap_1()
                        .child(self.action_button(
                            "dev-sync-approve-confirm",
                            "确认批准",
                            false,
                            false,
                            cx,
                            move |panel, cx| panel.confirm_approve(cx),
                        ))
                        .child(self.action_button(
                            "dev-sync-approve-cancel",
                            "取消",
                            false,
                            false,
                            cx,
                            move |panel, _cx| panel.pending_approve = None,
                        )),
                );
            let _ = target;
        }

        // 邀请区（ADR 0014 S1）：按钮常显（不可用时置灰 + 原因）；链接只在内存里展示。
        let invite_reason = self.invite_block_reason();
        let mut invite_area = div()
            .id("dev-sync-invite-area")
            .debug_selector(|| "dev-sync-invite-area".to_string())
            .flex()
            .flex_col()
            .gap_1()
            .pt(px(4.0))
            .border_t_1()
            .border_color(theme::sidebar_border())
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(theme::text_color())
                    .child(SharedString::from("邀请设备（单 vault；创建者批准后生效）")),
            )
            .child(self.action_button(
                "dev-sync-invite",
                "邀请设备…",
                false,
                invite_reason.is_some(),
                cx,
                |panel, cx| panel.request_invite(cx),
            ));
        if let Some(reason) = invite_reason {
            invite_area = invite_area.child(
                div()
                    .debug_selector(|| "dev-sync-invite-blocked".to_string())
                    .text_size(px(10.0))
                    .text_color(theme::status_bar_text())
                    .child(SharedString::from(reason)),
            );
        }
        if let Some(link) = self.snapshot.invite_link.as_ref() {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_secs());
            // **不渲染** `mn-pair://join?...` 明文（含 token）：链接只进剪贴板。
            invite_area = invite_area
                .child(
                    div()
                        .text_size(px(10.0))
                        .text_color(theme::status_bar_text())
                        .child(SharedString::from(
                            "链接明文不会显示在面板上：只通过「复制邀请链接」进剪贴板（一次性 token）",
                        )),
                )
                .child(
                    div()
                        .debug_selector(|| "dev-sync-invite-ttl".to_string())
                        .text_size(px(10.0))
                        .text_color(theme::status_bar_text())
                        .child(SharedString::from(link.ttl_text(now))),
                );
            if self.invite_copied {
                // 复制成功后折叠：不再展示链接/复制按钮（新链接走「邀请设备…」）。
                invite_area = invite_area.child(
                    div()
                        .debug_selector(|| "dev-sync-invite-copied".to_string())
                        .text_size(px(10.0))
                        .text_color(theme::caret_color())
                        .child(SharedString::from(
                            "已复制邀请链接；关闭面板即失效（链接不再显示）",
                        )),
                );
            } else {
                invite_area = invite_area.child(self.action_button(
                    "dev-sync-copy-invite",
                    "复制邀请链接",
                    false,
                    false,
                    cx,
                    |panel, cx| panel.copy_invite_link(cx),
                ));
            }
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
                        "本机有管理密钥：可以移除其它设备；初始基线在 Create 时自动发布，失败可重试。"
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
            .child(invite_area)
            .child(self.render_rotation(cx))
    }

    /// 轮换密钥 / 应用轮换的入口 + 两段二次确认（ADR 0009 Phase 2 切片 6b）。
    ///
    /// 两个动作**独立**于「移除设备」：移除只删凭证（ADR 0010 默认），轮换只有用户显式
    /// 点这里才会发生；轮换也不会自动串联到移除。
    fn render_rotation(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let rotate_dimmed = self.rotate_block_reason().is_some();
        let apply_dimmed = self.apply_block_reason().is_some();
        let hint = self.rotation_hint();
        let mut block = div()
            .id("dev-sync-rotation")
            .debug_selector(|| "dev-sync-rotation".to_string())
            .flex()
            .flex_col()
            .gap_1()
            .pt(px(4.0))
            .border_t_1()
            .border_color(theme::sidebar_border())
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .gap_1()
                    .child(self.action_button(
                        "dev-sync-rotate",
                        "轮换密钥…",
                        true,
                        rotate_dimmed,
                        cx,
                        |panel, cx| panel.request_rotate_confirm(cx),
                    ))
                    .child(self.action_button(
                        "dev-sync-apply-epoch",
                        "应用轮换",
                        false,
                        apply_dimmed,
                        cx,
                        |panel, cx| panel.request_apply_confirm(cx),
                    )),
            )
            .child(
                div()
                    .text_size(px(10.0))
                    .text_color(theme::status_bar_text())
                    .child(SharedString::from(hint)),
            );

        if self.pending_rotate_confirm {
            block = block.child(self.render_rotate_confirm(cx));
        }
        if self.pending_apply_confirm {
            block = block.child(self.render_apply_confirm(cx));
        }
        block
    }

    /// 轮换入口的状态行：先给"正在轮换"的阶段（四段 + worker 明细），再给不可用原因。
    fn rotation_hint(&self) -> String {
        if let Some(stage) = self.rotation_stage_text() {
            return stage;
        }
        if let Some(reason) = self.rotate_block_reason() {
            return format!("轮换不可用：{reason}");
        }
        "轮换可用：会重新加密整库并生成两份候选基线（被移除设备仍可解旧数据）".to_owned()
    }

    /// 轮换二次确认区：固定文案 + 恢复码重封装选项 + 撤销列表选项 + 警告 + 确认/取消。
    fn render_rotate_confirm(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let reissue = self.rotate_reissue;
        let warning = self.rotate_warning_text(cx);
        let warning_text = warning.unwrap_or("恢复码将随新 epoch 重封，提交后输入框立即清空");
        let warning_color = if warning.is_some() {
            theme::danger_text()
        } else {
            theme::status_bar_text()
        };
        let revoked = self.revoked_device_ids().len();
        let mut block = div()
            .flex()
            .flex_col()
            .gap_1()
            .p(px(6.0))
            .border_1()
            .border_color(theme::input_border())
            .rounded_sm()
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(theme::text_color())
                    .child(SharedString::from(ROTATE_CONFIRM_TEXT)),
            )
            .child(self.checkbox_row(
                "dev-sync-rotate-reissue",
                reissue,
                "重新封装恢复码（推荐）",
                cx,
                |panel, cx| panel.toggle_reissue(cx),
            ));
        if reissue {
            block = block
                .child(
                    div()
                        .text_size(px(10.0))
                        .text_color(theme::status_bar_text())
                        .child(SharedString::from(
                            "当前恢复码（遮罩输入；只用于本次重封，提交后立即清空）",
                        )),
                )
                .child(self.recovery_input.clone());
        }
        block
            .child(self.checkbox_row(
                "dev-sync-rotate-revoke",
                self.rotate_revoke,
                "同时把已移除设备写入签名撤销列表",
                cx,
                |panel, cx| panel.toggle_revoke(cx),
            ))
            .child(
                div()
                    .text_size(px(10.0))
                    .text_color(theme::status_bar_text())
                    .child(SharedString::from(format!(
                        "已移除设备 {revoked} 台会被写进本次 signed revoke 列表（不手选，按列表状态）"
                    ))),
            )
            .child(
                div()
                    .debug_selector(|| "dev-sync-rotate-warning".to_string())
                    .text_size(px(10.0))
                    .text_color(warning_color)
                    .child(SharedString::from(warning_text)),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_1()
                    .child(self.action_button(
                        "dev-sync-rotate-confirm",
                        "确认轮换",
                        true,
                        false,
                        cx,
                        |panel, cx| panel.confirm_rotate(cx),
                    ))
                    .child(self.action_button(
                        "dev-sync-rotate-cancel",
                        "取消",
                        false,
                        false,
                        cx,
                        |panel, cx| panel.cancel_rotate(cx),
                    )),
            )
    }

    /// 「应用轮换」二次确认区（固定文案 + 确认/取消）。
    fn render_apply_confirm(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap_1()
            .p(px(6.0))
            .border_1()
            .border_color(theme::input_border())
            .rounded_sm()
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(theme::text_color())
                    .child(SharedString::from(APPLY_CONFIRM_TEXT)),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_1()
                    .child(self.action_button(
                        "dev-sync-apply-confirm",
                        "确认应用",
                        false,
                        false,
                        cx,
                        |panel, cx| panel.confirm_apply(cx),
                    ))
                    .child(self.action_button(
                        "dev-sync-apply-cancel",
                        "取消",
                        false,
                        false,
                        cx,
                        |panel, cx| panel.cancel_apply(cx),
                    )),
            )
    }

    /// 一行复选框（与 ADR 0011 的向导同一形态；无第三方组件依赖）。
    fn checkbox_row(
        &self,
        selector: &'static str,
        checked: bool,
        label: &'static str,
        cx: &mut Context<Self>,
        on_click: impl Fn(&mut DevSyncPanel, &mut Context<DevSyncPanel>) + 'static,
    ) -> gpui::Stateful<gpui::Div> {
        div()
            .id(SharedString::from(selector))
            .debug_selector(move || selector.to_string())
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .cursor_pointer()
            .child(
                div()
                    .w(px(14.0))
                    .h(px(14.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .border_1()
                    .border_color(theme::input_border())
                    .bg(theme::input_bg())
                    .text_size(px(11.0))
                    .text_color(theme::text_color())
                    .child(SharedString::from(if checked { "✓" } else { " " })),
            )
            .child(
                div()
                    .text_size(px(10.0))
                    .text_color(theme::text_color())
                    .child(SharedString::from(label)),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |panel, _event: &gpui::MouseDownEvent, _window, cx| {
                    on_click(panel, cx);
                    cx.notify();
                    cx.stop_propagation();
                }),
            )
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

    /// 点击「轮换密钥…」：只展开**二次确认区**，不发任何命令（切片 6b）。
    ///
    /// 不可用时给明确原因并**不**展开——确认区本身就是"你要为这次重加密签字"的位置，
    /// 让一个注定被拒的表单展开只会误导。
    fn request_rotate_confirm(&mut self, cx: &mut Context<Self>) {
        if let Some(reason) = self.rotate_block_reason() {
            self.pending_rotate_confirm = false;
            self.notice = Some(format!("未展开轮换确认：{reason}"));
            cx.notify();
            return;
        }
        // 两个确认区互斥：同时展开会让"确认"按钮指向不明。
        self.pending_apply_confirm = false;
        self.pending_rotate_confirm = true;
        self.notice =
            Some("等待确认：轮换会用新密钥重新加密整库（期间输入冻结、自动同步暂停）".to_owned());
        cx.notify();
    }

    /// 点击「应用轮换」：展开应用新 epoch 的二次确认区（不发命令）。
    fn request_apply_confirm(&mut self, cx: &mut Context<Self>) {
        if let Some(reason) = self.apply_block_reason() {
            self.pending_apply_confirm = false;
            self.notice = Some(format!("未展开应用新 epoch 确认：{reason}"));
            cx.notify();
            return;
        }
        self.pending_rotate_confirm = false;
        self.pending_apply_confirm = true;
        self.notice =
            Some("等待确认：应用新 epoch 会重新验证授权并导入基线（期间输入冻结）".to_owned());
        cx.notify();
    }

    /// 取消轮换：收起确认区、**清空恢复码**、不写任何东西。
    fn cancel_rotate(&mut self, cx: &mut Context<Self>) {
        if self.pending_rotate_confirm {
            self.pending_rotate_confirm = false;
            self.recovery_input.update(cx, |input, _| input.clear());
            self.notice = Some("已取消轮换：没有写入任何内容（epoch/身份不变）".to_owned());
        }
        cx.notify();
    }

    /// 取消「应用轮换」：收起确认区，不发命令。
    fn cancel_apply(&mut self, cx: &mut Context<Self>) {
        if self.pending_apply_confirm {
            self.pending_apply_confirm = false;
            self.notice = Some("已取消应用新 epoch：没有写入任何内容".to_owned());
        }
        cx.notify();
    }

    fn toggle_reissue(&mut self, cx: &mut Context<Self>) {
        self.rotate_reissue = !self.rotate_reissue;
        if !self.rotate_reissue {
            // 不重封就把输入框里的秘密丢掉（不再需要，也不该留在内存里）。
            self.recovery_input.update(cx, |input, _| input.clear());
        }
        cx.notify();
    }

    fn toggle_revoke(&mut self, cx: &mut Context<Self>) {
        self.rotate_revoke = !self.rotate_revoke;
        cx.notify();
    }

    /// 本次要写进 signed begin body 的吊销列表（ADR 0010）。
    ///
    /// UI **不要求手选设备**：后端按 relay 的 `status=revoked` 列表来；接口只接受设备
    /// 列表，所以这里从当前 members 里过滤已移除设备。
    fn revoked_device_ids(&self) -> Vec<DeviceId> {
        self.snapshot
            .devices
            .iter()
            .filter(|member| member.status == MemberStatus::Revoked)
            .map(|member| member.device_id)
            .collect()
    }

    /// 确认轮换：读一次恢复码（按需）→ 清空输入 → 登记请求 → 通知宿主持闸派发。
    ///
    /// 顺序理由：恢复码**只读一次**并立刻从输入框清掉（ADR 0009/0011「只显示一次、
    /// 提交后清除」）；请求里的秘密由 [`RotateRequest`] 的 `Zeroizing` 持有，不进
    /// 事件/Debug/日志。
    fn confirm_rotate(&mut self, cx: &mut Context<Self>) {
        if let Some(reason) = self.rotate_block_reason() {
            self.pending_rotate_confirm = false;
            self.recovery_input.update(cx, |input, _| input.clear());
            self.notice = Some(format!("轮换未提交：{reason}"));
            cx.notify();
            return;
        }
        let recovery = if self.rotate_reissue {
            let text = self.recovery_input.read(cx).text().trim().to_owned();
            if text.is_empty() {
                // 不收起确认区：让用户补输入或取消勾选，而不是派发一个缺恢复码的请求。
                self.notice = Some(format!("轮换未提交：{ROTATE_RECOVERY_EMPTY_WARNING}"));
                cx.notify();
                return;
            }
            Some(text)
        } else {
            None
        };
        // 秘密只读一次：无论后面成功与否，输入框立刻清空。
        self.recovery_input.update(cx, |input, _| input.clear());
        self.pending_rotate_confirm = false;
        let revoke = if self.rotate_revoke {
            self.revoked_device_ids()
        } else {
            Vec::new()
        };
        let reissued = recovery.is_some();
        let revoked = revoke.len();
        self.pending_rotate_request = Some(RotateRequest::new(recovery, revoke));
        self.notice = Some(if reissued {
            format!(
                "请求轮换：恢复码将随新 epoch 重封（吊销 {revoked} 台）；由 AppShell 持闸后派发 worker"
            )
        } else {
            format!(
                "请求轮换：不重封恢复码（吊销 {revoked} 台）——{ROTATE_NO_RECOVERY_WARNING}；由 AppShell 持闸后派发 worker"
            )
        });
        cx.emit(DevSyncPanelEvent::RotateRequested);
        cx.notify();
    }

    /// 确认「应用轮换」：登记请求并通知宿主持闸派发。
    fn confirm_apply(&mut self, cx: &mut Context<Self>) {
        if let Some(reason) = self.apply_block_reason() {
            self.pending_apply_confirm = false;
            self.notice = Some(format!("应用新 epoch 未提交：{reason}"));
            cx.notify();
            return;
        }
        self.pending_apply_confirm = false;
        self.notice = Some("请求应用新 epoch：由 AppShell 持闸后派发 worker".to_owned());
        cx.emit(DevSyncPanelEvent::ApplyEpochRequested);
        cx.notify();
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
        // ADR 0017 S5b 收口：顶部**不再**有「创建」「加入」——真实入口在 AppRoot
        // Welcome/向导里；面板里重复的按钮只会让人以为在面板里创建就能跳过备份页。
        // 保留「同步」「停止」；「发布基线」只在缺失时出现（见下）。
        let buttons = [("dev-sync-sync", "同步", Command::SyncOnce, false)];
        let mut row = div().flex().flex_row().flex_wrap().gap_1();
        for (selector, label, command, danger) in buttons {
            row = row.child(self.button(selector, label, command, danger, cx));
        }
        // 「重试发布基线」：只在**本机有管理钥**且 durable 基线**已知缺失**时渲染。
        // 正常实例（Create 已自动发布）不显示；未知（还没读到设备/基线事实）也不显示，
        // 避免首帧闪一下。仍只是请求：由 AppShell 持闸（现有 `request_publish`）。
        let publish_missing = self.snapshot.can_manage_devices
            && self.snapshot.baseline_known
            && self.snapshot.published_baseline.is_none();
        if publish_missing {
            let publish_dimmed = self.snapshot.in_flight.is_some() || self.host_busy;
            row = row.child(self.action_button(
                "dev-sync-publish",
                "重试发布基线",
                false,
                publish_dimmed,
                cx,
                |panel, cx| panel.request_publish(cx),
            ));
        }
        row = row.child(self.button("dev-sync-stop", "停止", Command::Stop, true, cx));
        let devices = self.render_devices(cx);

        div()
            .id("dev-sync-panel")
            // 测试元数据：面板根（headless 测试断言存在与尺寸）。
            .debug_selector(|| "dev-sync-panel".to_string())
            .w_full()
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
            .child(
                div()
                    .text_size(px(10.0))
                    .text_color(theme::status_bar_text())
                    .child(SharedString::from(if publish_missing {
                        "初始基线未发布：创建后会自动发布，失败可重试（重试需要先打开一篇笔记）"
                    } else {
                        "初始基线在创建时自动发布；基线已就绪时此面板不显示发布按钮"
                    })),
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
        let mut snapshot = StateSnapshot {
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
            baseline_known: false,
            published_baseline: None,
            last_revoked: None,
            invite_link: None,
            last_approved: None,
            last_rotation: None,
            last_rotation_op: None,
        };
        let text = state_text(&snapshot);
        assert!(text.contains("执行中"), "必须显示状态：{text}");
        assert!(text.contains("#3"), "必须显示在途 op：{text}");
        assert!(text.contains("推送 2 条"), "必须显示最新进度：{text}");
        assert_eq!(error_text(&snapshot), "最近错误：无");
        snapshot.last_error = Some("切片 4 未实现".to_owned());
        assert_eq!(error_text(&snapshot), "最近错误：切片 4 未实现");
        // 关闭中要显示关闭门禁（Blocked 的原因必须能看见）。
        snapshot.state = DevSyncState::Closing;
        snapshot.close = CloseState::Blocked("durable 未确认".to_owned());
        let text = state_text(&snapshot);
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

    /// 轮换/应用的固定文案必须一字不漏：它们是这次操作的全部真相（ADR 0009 §安全边界）。
    #[test]
    fn rotation_copy_covers_the_operation_boundaries() {
        for needle in [
            "轮换会用新密钥重新加密整库",
            "生成两份候选基线",
            "旧 epoch 数据不能再被新设备解密",
            "被移除设备仍可解其已获得的数据",
            "轮换期间输入会冻结",
            "是否继续？",
        ] {
            assert!(
                ROTATE_CONFIRM_TEXT.contains(needle),
                "轮换确认文案必须包含 {needle:?}：{ROTATE_CONFIRM_TEXT}"
            );
        }
        for needle in [
            "应用新 epoch",
            "重新验证授权",
            "导入基线",
            "输入会冻结",
            "是否继续？",
        ] {
            assert!(
                APPLY_CONFIRM_TEXT.contains(needle),
                "apply 确认文案必须包含 {needle:?}：{APPLY_CONFIRM_TEXT}"
            );
        }
        for needle in ["申请", "候选", "激活", "推广"] {
            assert!(
                ROTATE_STAGES_TEXT.contains(needle),
                "轮换阶段文案必须包含 {needle:?}：{ROTATE_STAGES_TEXT}"
            );
        }
        assert!(
            ROTATE_NO_RECOVERY_WARNING.contains("无法用恢复码加入"),
            "{ROTATE_NO_RECOVERY_WARNING}"
        );
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
