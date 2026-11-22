//! dev-only 同步面板 + 串行 worker（ADR 0009 Phase 1 / 切片 1）。
//!
//! ## 只在 feature 下存在
//!
//! 整个模块由 `crates/app/Cargo.toml` 的 `dev-sync` feature 门禁（`dep:menghunote-sync`）。
//! 默认构建里它**不存在**：`lib.rs` 是 `#[cfg(feature = "dev-sync")] pub mod dev_sync;`，
//! `AppShell` 的结构体字段、渲染分支、`main.rs` 的门禁解析也全部 cfg 掉——默认产物
//! 零行为变化、不引入 sync 依赖树。
//!
//! ## 四块职责（互不越界）
//!
//! | 文件 | 职责 | 允许依赖 |
//! | --- | --- | --- |
//! | [`state`] | 纯状态机：单操作在途、事件合并/不丢、关闭票据判定、设备列表投影 | `std` + 设备视图类型（`MemberView`，无 IO） |
//! | [`service`] | 专用 OS 线程 + channel + 操作 deadline + 设备列表/移除的执行体 | `state` + `menghunote-sync`（传输配置 + `DevLocal` 数据入口） |
//! | [`gate`] | 运行时门禁（`--dev-sync` / env / 独立 vault / localhost relay） | `service` |
//! | [`identity`] | 持久设备身份文件（`dev-identity.bin`）→ Loro peer；旧/非空库拒绝 | `menghunote-sync` + `menghunote-storage` |
//! | [`setup`] | 启动生命周期：Create / Join / 复用已有身份的显式模式（切片 4c-1） | `gate` + `identity` + `menghunote-sync` |
//! | [`cli`] | 手工演示交付：Create 一次性恢复码块 + `--dev-sync-info` / `--dev-sync-publish`（切片 4c-3） | `setup` + `identity` + `menghunote-sync` + storage |
//! | [`sync_once`] | 持闸同步的协调状态/报告（切片 4c-2）；GPUI `Entity` + storage 会话类型 | `state` + `menghunote-storage` + `editor::view` |
//! | [`panel`] | GPUI 视图：只读投影 + 按钮 + 设备列表/确认流 + 轮换/apply 确认区 | `state` + `service` + `onboarding::input`（秘密单行控件）+ GPUI |
//! | [`scheduler`] | **纯**自动同步调度器（ADR 0012 S1；无 ticker） | 仅 `std`（时间由调用方注入） |
//! | [`autosync`] | 自动同步接线（ADR 0012 S3）：时刻表/失败分类/续批/常驻状态投影 | `scheduler` + `service::OperationClock` + storage（只读计数） |
//!
//! ## 切片 4c-2 与 ADR 0012 S2a（手动同步两阶段）
//!
//! 有活动文档的手动「同步」由 [`service::DevLocalRunner::run_two_phase`] 在 worker 线程里
//! 执行**可暂停的三阶段**：Prepare（网络，不持闸）→ 等 `SyncControl::GrantGate` →
//! Integrate → 等 `SyncControl::Finish` → push tail + ack。UI 侧顺序
//! （Prepare → NeedGate → Held → GrantGate → Integrated → adopt/end → Finish）在
//! `AppShell` 的 `dev_sync_sync_once` / `dev_sync_pump` 里，状态与报告类型见 [`sync_once`]。
//! 发布基线/无会话路径仍走旧整轮 `DevLocal::sync_once`；两者共用单操作 slot。
//! Create/Join 仍走 CLI（[`setup`] + [`cli`]）；「发布基线」（切片 4c-4）与同步共用同一条
//! 持闸状态机：面板按钮只发 [`panel::DevSyncPanelEvent::PublishRequested`]，`AppShell` 先
//! 持闸再派发 `PublishInitialPair`，worker 调
//! `DevLocal::publish_initial_pair_with_identity`（不需要恢复码）。
//!
//! ## 切片 2a-2（设备列表与移除设备，ADR 0010）
//!
//! 数据入口在 sync 侧（[`menghunote_sync::dev_local::DevLocal::members`] /
//! [`menghunote_sync::dev_local::DevLocal::revoke_device`]），app 侧只加两条命令
//! （`ListDevices` / `RevokeDevice`）与两条领域事件（`DevicesListed` / `DeviceRevoked`）。
//! 面板不碰网络、不读身份文件、不管理密钥；**移除设备 ≠ 轮换密钥**，轮换入口不在本片。
//!
//! ## Phase 2 切片 6b（轮换接到设备面板 UI）
//!
//! [`panel`] 的设备列表区域新增两个**独立**入口：`dev-sync-rotate`「轮换密钥…」（仅
//! creator：有管理密钥 + 有活动文档 + 无在途）与 `dev-sync-apply-epoch`「应用轮换」
//! （非 manager 侧的主动方）。两者都只展开二次确认，确认后把请求交给宿主
//! （[`panel::DevSyncPanel::take_rotate_request`]）再走 6a 的持闸状态机——面板从不直接
//! `try_command(Rotate/ApplyEpoch)`，也不自己决定恢复码要不要重封（那是用户的勾选）。
//!
//! 恢复码输入复用 [`crate::onboarding::input::FormInput`]（遮罩、允许粘贴、不向平台报告
//! 正文）；它只在**勾选重封装**时渲染，提交/取消后立刻清空，恢复码只随 `RotateRequest`
//! 的 `Zeroizing` 存活。轮换在途时整段持闸（6a）：输入冻结、按钮置灰、自动同步暂停，
//! 面板任务行显示「轮换中（申请/候选/激活/推广）」+ worker 明细。
//!
//! ## 切片 4c-3（手工演示缺口）
//!
//! [`cli::create_handoff_block`] 把 `vault_id + 恢复码` 一次性交给操作者（只 stdout、
//! 不落盘）；[`cli::inspect`] / [`cli::publish_baseline`] 是"不开窗、打印完即退出"的
//! 查询/发布路径，完整两实例步骤见 `docs/notes/dev-sync-manual-test.md`。

pub mod autosync;
pub mod cli;
pub mod gate;
pub mod identity;
pub mod management;
pub mod panel;
pub mod scheduler;
pub mod service;
pub mod setup;
pub mod state;
pub mod sync_once;

pub use autosync::{
    classify_failure, classify_sync_error, durable_backlog, hhmm_utc, AutoSync, AutoSyncConfig,
    Backlog, FailureClass, OpOrigin, RoundOutcome, SecurityCategory, StatusState, SyncStatus,
};
pub use cli::{create_handoff_block, info_block, inspect, publish_baseline, publish_block};
pub use gate::{DevSyncConfig, Gate, GateError};
pub use identity::{ensure_instance_vault_ready, DevIdentityStore};
pub use management::{
    approval_view, build_snapshot, format_data_rights, format_fingerprint_128,
    format_fingerprint_256, format_scopes, format_unix_utc, key_fingerprint,
    sanitize_management_text, short_hash, short_id, ApprovalView, CapView, DeviceStatusView,
    DeviceView, ManagementArtifact, ManagementCommand, ManagementEvent, ManagementPanel,
    ManagementReport, ManagementRunConfig, ManagementRunner, ManagementService, ManagementSnapshot,
    PolicyView, RealManagementRunner, ReceiptView, SlotView, MANAGEMENT_PUMP_INTERVAL,
    MAX_MANAGEMENT_ERROR_CHARS, V2_DISABLED_MESSAGE,
};
pub use panel::{
    DevSyncPanel, DevSyncPanelEvent, APPLY_CONFIRM_TEXT, REVOKE_WARNING, ROTATE_CONFIRM_TEXT,
    ROTATE_NO_RECOVERY_WARNING, ROTATE_RECOVERY_EMPTY_WARNING, ROTATE_STAGES_TEXT,
};
pub use scheduler::{
    Decision, EditOrigin, Finished, LocalEditAck, Scheduler, SchedulerConfig, SchedulerState,
    SkipReason, Trigger,
};
pub use service::{
    DevLocalRunner, OperationClock, ServiceConfig, SyncControl, SyncRunner, SyncService,
    SystemOperationClock, TransportTimeouts, UnimplementedRunner,
};
pub use setup::{CliMode, SetupMode, SetupOutcome, SetupSecrets};
pub use state::{
    short_device_id, Busy, BusyReason, CloseState, Command, CommandKind, DevSyncState, Event,
    HeldGate, OpId, OpOutcome, OpResult, Phase, RotationFacts, RotationTask, StateSnapshot,
    StopTicket, SyncPhase, SyncState, TwoPhaseStatus, REVOKE_UNCHANGED_MARKER,
};
pub use sync_once::{
    release_gate_allowed, ActiveSyncOnce, GatedTask, RotateRequest, SyncOnceReport, SyncOnceStage,
    SyncOnceTarget, SYNC_PUMP_INTERVAL,
};

/// 实验性 V2 管理 UI 是否展示（默认关闭）。
///
/// 个人用户只需要 V1 的设备列表/邀请/移除；V2 的 cap/双签/审批是高风险多管理员场景，
/// 不应出现在正常设置页。需要验证 S4d1c 时显式设置 `MENGHUNOTE_EXPERIMENTAL_V2=1`。
pub(crate) fn experimental_v2_ui() -> bool {
    std::env::var_os("MENGHUNOTE_EXPERIMENTAL_V2").is_some()
}
