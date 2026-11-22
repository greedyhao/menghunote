//! menghunote 桌面端 app 层（GPUI 视图，ADR 0003 §7 切片 5）。
//!
//! 本 crate 是**唯一允许依赖 GPUI** 的层：
//!
//! - [`editor`]：单 `Element` 的源码视图（软折行绘制、命中、选区、滚动）；
//! - [`dev_vault`]：开发用 vault 的路径约定与演示笔记播种；
//! - [`platform`]：平台小工具（目前只有 Windows DPI 感知的读回校验）。
//!
//! 依赖方向（ADR 0003 §3）：`app → editor/storage/gpui`。`editor` / `storage` /
//! `text` 都不依赖 GPUI，也不依赖本 crate。

// 本 crate 只有 `platform` 模块里的三个 Win32 DPI 查询需要 unsafe，
// 因此用 `deny` + 该模块级别的 `allow`（其余代码仍不得出现 unsafe）。
#![deny(unsafe_code)]

pub mod dev_vault;
pub mod editor;
pub mod export;
pub mod platform;
pub mod shell;
pub mod sidebar_rename;
