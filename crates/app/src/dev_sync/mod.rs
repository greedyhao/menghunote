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
//! | [`panel`] | GPUI 视图：只读投影 + 5 个按钮 + 设备列表/确认流 | `state` + `service` + GPUI |
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
//! ## 切片 4c-3（手工演示缺口）
//!
//! [`cli::create_handoff_block`] 把 `vault_id + 恢复码` 一次性交给操作者（只 stdout、
//! 不落盘）；[`cli::inspect`] / [`cli::publish_baseline`] 是"不开窗、打印完即退出"的
//! 查询/发布路径，完整两实例步骤见 `docs/notes/dev-sync-manual-test.md`。

pub mod autosync;
pub mod cli;
pub mod gate;
pub mod identity;
pub mod panel;
pub mod scheduler;
pub mod service;
pub mod setup;
pub mod state;
pub mod sync_once;

pub use autosync::{
    classify_failure, durable_backlog, hhmm_utc, AutoSync, AutoSyncConfig, Backlog, FailureClass,
    OpOrigin, RoundOutcome, StatusState, SyncStatus,
};
pub use cli::{create_handoff_block, info_block, inspect, publish_baseline, publish_block};
pub use gate::{DevSyncConfig, Gate, GateError};
pub use identity::{ensure_instance_vault_ready, DevIdentityStore};
pub use panel::REVOKE_WARNING;
pub use panel::{DevSyncPanel, DevSyncPanelEvent};
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
    HeldGate, OpId, OpOutcome, OpResult, Phase, StateSnapshot, StopTicket, SyncPhase, SyncState,
    TwoPhaseStatus, REVOKE_UNCHANGED_MARKER,
};
pub use sync_once::{
    release_gate_allowed, ActiveSyncOnce, GatedTask, SyncOnceReport, SyncOnceStage, SyncOnceTarget,
    SYNC_PUMP_INTERVAL,
};
