//! menghunote 插件 API 的**纯契约**层（ADR 0020 P2）。
//!
//! 本 crate 只定义第三方插件将来需要的声明式数据与 host 服务接口：
//!
//! - [`PluginId`]：小写字母/数字/`.`/`-`，1–64 字节，首尾不能是分隔符；
//! - [`PluginVersion`]：`major.minor.patch`（严格 ASCII 数字、无前导零）；
//! - [`ApiVersion`]：`u16`，host 精确支持集合当前只有 `1`；
//! - [`PluginManifest`]：id/name/version/api_version/locales/capabilities/
//!   commands/panels/settings，以及 [`PluginManifest::validate`]；
//! - [`Capability`]：8 个能力的声明值（未知能力在 serde 反序列化阶段拒绝）；
//! - [`host`]：`notes_read`/`notes_write`/`settings`/`notifications` 的最小
//!   host 服务 trait（只使用 host 自有类型）。
//!
//! # 边界（本切片明确不做）
//!
//! - **不实现加载器**：不安装/加载/卸载插件，不执行命令，不接 app 的
//!   Vault/编辑器；没有插件进入产品的运行时路径。
//! - **不选择 WASM/独立进程**：ADR 0020 §8 问题 2 仍开放，本 crate 不引入
//!   WASM/进程/IPC 依赖。
//! - **不暴露 SQLite/密钥/GPUI 类型**：host trait 只用 [`NoteId`] /
//!   [`NoteSnapshot`] / [`SettingValue`] / [`Notification`] 等自有类型。
//! - **不做授权 UI、签名/来源校验**：manifest 的 capability 只是声明；
//!   [`PluginManifest::validate`] 失败时（未来）必须拒绝加载。
//! - **不冻结 UI schema**：面板 slot、命令参数、设置项 UI 都保持最小或不定义。
//!
//! # 依赖方向
//!
//! `plugin-api` 只依赖 `serde` 与纯 crate `menghunote-i18n`（复用 host 的
//! locale 解析）；不依赖 GPUI/storage/sync/relay/editor。`app` 可以依赖本
//! crate，本 crate 不反向依赖 `app`。
//!
//! # manifest JSON 形状（示例）
//!
//! ```json
//! {
//!   "id": "com.example.notes",
//!   "name": "Example Notes",
//!   "version": "1.2.3",
//!   "api_version": 1,
//!   "locales": ["zh-CN", "en"],
//!   "capabilities": ["notes_read", "settings"],
//!   "commands": [
//!     { "id": "open-dashboard", "title": "Open dashboard" }
//!   ],
//!   "panels": [
//!     { "id": "dashboard", "title": "Dashboard", "slot": "right-sidebar" }
//!   ],
//!   "settings": [
//!     { "id": "enabled", "type": "bool", "default": true, "scope": "user" }
//!   ]
//! }
//! ```
//!
//! 反序列化入口是 `serde_json::from_str::<PluginManifest>(...)`（本 crate 的
//! 运行时依赖不包含 JSON 解析器；加载器负责选 JSON 实现并调用
//! [`PluginManifest::validate`]）。反序列化失败（缺必需字段、未知
//! capability/type/scope、非法版本字符串）同样必须拒绝加载。

#![deny(unsafe_code)]

pub mod capability;
pub mod host;
pub mod id;
pub mod manifest;
pub mod version;

pub use capability::Capability;
pub use host::{
    HostError, HostServices, NoteId, NoteSnapshot, NoteSummary, NotesReadService,
    NotesWriteService, Notification, NotificationLevel, NotificationService, SettingsService,
};
pub use id::{PluginId, PluginIdError, PLUGIN_ID_MAX_LEN};
pub use manifest::{
    CommandSpec, PanelSpec, PluginManifest, SettingScope, SettingSpec, SettingType, SettingValue,
    ValidationError,
};
pub use version::{ApiVersion, PluginVersion, PluginVersionError, SUPPORTED_API_VERSIONS};
