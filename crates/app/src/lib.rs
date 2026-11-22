//! menghunote 桌面端 app 层（GPUI 视图，ADR 0003 §7 切片 5）。
//!
//! 本 crate 是**唯一允许依赖 GPUI** 的层：
//!
//! - [`editor`]：单 `Element` 的源码视图（软折行绘制、命中、选区、滚动）；
//! - [`dev_vault`]：开发用 vault 的路径约定与演示笔记播种；
//! - `dev_sync`（仅 `dev-sync` feature）：dev-only 同步面板与串行 worker（ADR 0009）；
//! - [`platform`]：平台小工具（目前只有 Windows DPI 感知的读回校验）。
//!
//! 依赖方向（ADR 0003 §3）：`app → editor/storage/gpui`。`editor` / `storage` /
//! `text` 都不依赖 GPUI，也不依赖本 crate。

// 本 crate 只有 `platform` 模块里的三个 Win32 DPI 查询需要 unsafe，
// 因此用 `deny` + 该模块级别的 `allow`（其余代码仍不得出现 unsafe）。
#![deny(unsafe_code)]

pub mod dev_vault;
// 附件插入（任务：开发版附件插入）：只在 `--features dev-sync` 下编译，默认构建里
// 这个模块不存在（它的 UI 入口也只挂在 dev-sync 面板同一道门禁下）。
#[cfg(feature = "dev-sync")]
pub mod attachment;
// dev-only 同步面板（ADR 0009）：只在 `--features dev-sync` 下编译，
// 默认构建里这个模块不存在、也不引入 `menghunote-sync` 依赖树。
#[cfg(feature = "dev-sync")]
pub mod dev_sync;
pub mod editor;
pub mod export;
// relay 模式/URL 的构建期能力闸（ADR 0013 §1）：`sync` feature 下存在；
// 只有 `dev-sync` 构建才把 `local_dev` 视为可接受模式。
#[cfg(feature = "sync")]
pub mod sync_gate;
// 同步初始化/配置的 UI 向导与设置（ADR 0011，S1–S4）：同样只在 `dev-sync` feature 下编译。
#[cfg(feature = "dev-sync")]
pub mod onboarding;
pub mod platform;
pub mod search_input;
pub mod search_overlay;
pub mod search_service;
pub mod shell;
pub mod sidebar_rename;
pub mod text_shortcuts;
