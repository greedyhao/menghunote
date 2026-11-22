//! menghunote 桌面端 app 层（GPUI 视图，ADR 0003 §7 切片 5）。
//!
//! 本 crate 是**唯一允许依赖 GPUI** 的层：
//!
//! - [`editor`]：单 `Element` 的源码视图（软折行绘制、命中、选区、滚动）；
//! - [`mermaid`]：Mermaid 源码 → SVG 字符串（M1 渲染底座；不含编辑器预览/缓存）；
//! - [`dev_vault`]：开发用 vault 的路径约定与演示笔记播种；
//! - `dev_sync`（`sync-ui` feature，默认开启）：同步面板/设置/worker（ADR 0009；其中
//!   `setup`/`cli`/`management` 仍只在 `dev-sync` 下编译）；
//! - [`platform`]：平台小工具（目前只有 Windows DPI 感知的读回校验）。
//!
//! 依赖方向（ADR 0003 §3）：`app → editor/storage/gpui`。`editor` / `storage` /
//! `text` 都不依赖 GPUI，也不依赖本 crate。

// 本 crate 只有 `platform` 模块里的三个 Win32 DPI 查询需要 unsafe，
// 因此用 `deny` + 该模块级别的 `allow`（其余代码仍不得出现 unsafe）。
#![deny(unsafe_code)]

pub mod dev_vault;
// 应用统一资产源（`with_assets`）：gpui-excalidraw 手绘图标 + GPUI Kit 默认资产。
// 默认构建也编进来：画布笔记不依赖同步/dev-sync。
pub mod assets;
// 应用图标（窗口/任务栏）：SVG 母版 + 运行时 PNG 解码；不引 SVG 运行时依赖。
pub mod app_icon;
// 画布笔记（`.excalidraw` 标题后缀）+ Excalidraw 编辑器覆盖层（第一方编入模块）。
pub mod drawing;
// 第一方核心插件「日记」（Obsidian 风格）：本机 `daily-notes.json` + 今日日记
// 路径/创建；默认构建也编进来（不依赖同步/dev-sync）。
pub mod daily_notes;
// 附件/图片导入（ADR 0019 §2/§3，S2b）：读字节 → 内容寻址 `assets` → `asset_refs` →
// 正文 `asset://` 链接 + 引用对账。**默认构建也编进来**：粘贴/拖放导入不依赖同步，
// 旧的"路径输入条"UI 已移除，本模块只保留可复用核心与 `VaultAssetWriter`。
pub mod attachment;
// 同步面板/worker（ADR 0009）：`sync-ui` 下编译（**默认 feature**）；只开 `sync`
// 或 `--no-default-features` 时这个模块不存在。其中 dev 专用的 `setup`/`cli`/
// V2 `management` 子模块仍由 `dev-sync` 门禁（见 `dev_sync/mod.rs`）。
#[cfg(feature = "sync-ui")]
pub mod dev_sync;
pub mod editor;
pub mod export;
// Wiki 关系图谱（ADR 0019 backlog `backlog-wiki-links-graph.md` 的 G0+G1）：
// **纯模型 + 视口数学**，只读 `tree`/`notes`/`links`，不改 schema、不参与同步。
// 默认构建也编进来：图谱不依赖 dev-sync。
pub mod graph;
// Mermaid 渲染底座（M1，ADR 0019 §15）：纯函数 `render_svg`，不接编辑器、不做缓存；
// 依赖 `merman` 精确 pin（见 crates/app/Cargo.toml）。M2 才接 Object Block 预览。
pub mod mermaid;
// 第一方内置模块只读注册表（ADR 0020 P2）：只投影编译期事实，没有运行时
// 开关/设置项 UI；Sync 的编入事实跟随 `sync-ui`（默认开启）。
pub mod modules;
pub mod note_properties;
// 笔记「历史记录…」时间线覆盖层（ADR 0024 H1，本机历史）：只读本地 versions 表，
// 恢复=新建一篇新笔记（append-only）。默认构建也编进来，不依赖同步/dev-sync。
pub mod note_history;
// relay 模式/URL 的构建期能力闸（ADR 0013 §1）：`sync` feature 下存在；
// 只有 `dev-sync` 构建才把 `local_dev` 视为可接受模式。
#[cfg(feature = "sync")]
pub mod sync_gate;
// 同步初始化/配置的 UI 向导与设置（ADR 0011，S1–S4）：`sync-ui`（默认）下编译。
// 产品构建只允许 `remote_tls`；`local_dev` 的许可仍由 `dev-sync` 单独控制。
#[cfg(feature = "sync-ui")]
pub mod onboarding;
pub mod platform;
pub mod search_input;
pub mod search_overlay;
pub mod search_service;
pub mod shell;
pub mod sidebar_rename;
// 快捷键中心模型 + 本机 `shortcuts.json`（设置页「快捷键」分类）：编辑器/宿主
// 都从这里查询当前绑定。默认构建也编进来（与同步无关）。
pub mod shortcuts;
pub mod text_shortcuts;
// 应用级主题（ADR 0021 P1/P2）：语义 token 集 + 浅色/深色/跟随系统 + 本机偏好。
// **默认构建也编进来**：编辑器/侧栏/状态栏都靠它取色，不依赖 dev-sync。
pub mod templates;
pub mod theme;
// 设置页的「外观」选择（浅色/深色/跟随系统）；只投影主题状态，不持有文档。
pub mod theme_settings;
// 设置页的「语言」选择与本机 `language.json`（ADR 0020 P1）；与主题相互独立。
pub mod language_settings;
// 首批迁移的固定 UI 文案（侧栏/状态栏/编辑器右键菜单/设置页）；渲染与测试共用。
pub mod ui_text;
