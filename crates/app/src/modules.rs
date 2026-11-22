//! 第一方内置模块注册表（ADR 0020 P2）。
//!
//! 本模块只把**编译期事实**投影成只读表：
//!
//! | id | 模块 | 编译条件 | 说明 |
//! | --- | --- | --- | --- |
//! | `sync` | Sync | `sync-ui`（默认 feature） | 同步 UI/worker（ADR 0009）；只开 `sync` 时只有 relay 能力闸，不算可用模块 |
//! | `mermaid` | Mermaid | 总是编入 | fenced block 预览与缓存（ADR 0019 §15） |
//! | `import-export` | Import / Export | 总是编入 | 粘贴/拖放附件导入 + 整库 Markdown 导出 |
//! | `graph` | Graph | 总是编入 | Wiki 双链关系图谱（G0–G2） |
//!
//! 当前**没有**运行时启用/禁用开关，也没有设置页 UI；[`ModuleDescriptor::enabled`]
//! 在本切片恒等于 `compiled`。将来若引入持久化开关，应在设置层实现，并保持
//! 本注册表只读（不要在 UI 里直接改描述符）。
//!
//! 顺序是**契约**：`Sync → Mermaid → ImportExport → Graph`；测试锁定顺序、
//! id 唯一与 `compiled` 的 feature 事实。不要按字母/哈希顺序“整理”这张表。
//!
//! 文案字段（`display_name` / `description`）是**注册表元数据**，本切片没有
//! 任何 UI 消费；它们不是 i18n key，也不承诺用户可见文案的最终形态。设置页
//! 只读展示要到 P3 之后另开切片，并且必须走 i18n message id，而不是直接渲染
//! 这里的英文字符串。

use std::fmt;

/// 第一方内置模块的稳定标识。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum BuiltinModuleId {
    /// 同步（`sync-ui` 构建；默认开启）。
    Sync,
    /// Mermaid 预览。
    Mermaid,
    /// 附件导入 / Markdown 导出。
    ImportExport,
    /// Wiki 关系图谱。
    Graph,
}

impl BuiltinModuleId {
    /// 固定顺序（注册表与测试共用的唯一顺序）。
    pub const ALL: [Self; 4] = [Self::Sync, Self::Mermaid, Self::ImportExport, Self::Graph];

    /// 稳定字符串 id（不是 i18n key；字符规则与插件 id 一致）。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sync => "sync",
            Self::Mermaid => "mermaid",
            Self::ImportExport => "import-export",
            Self::Graph => "graph",
        }
    }

    /// 注册表元数据用的展示名（当前无 UI 消费，不是最终 UI 文案）。
    #[must_use]
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Sync => "Sync",
            Self::Mermaid => "Mermaid",
            Self::ImportExport => "Import / Export",
            Self::Graph => "Graph",
        }
    }

    /// 注册表元数据用的说明（当前无 UI 消费，不是最终 UI 文案）。
    #[must_use]
    pub const fn description(self) -> &'static str {
        match self {
            Self::Sync => "End-to-end encrypted sync with a user-hosted relay.",
            Self::Mermaid => "Render Mermaid fenced blocks as read-only diagrams.",
            Self::ImportExport => "Paste/drag-in attachments and export the vault as Markdown.",
            Self::Graph => "Explore wiki-link relationships in local and global graph views.",
        }
    }

    /// 编入该模块的 Cargo feature（没有 feature 门禁时为 `None`）。
    #[must_use]
    pub const fn feature_gate(self) -> Option<ModuleFeatureGate> {
        match self {
            Self::Sync => Some(ModuleFeatureGate::SyncUi),
            Self::Mermaid | Self::ImportExport | Self::Graph => None,
        }
    }

    /// 当前构建是否编入该模块（编译期事实，不读环境变量/设置）。
    #[must_use]
    pub const fn is_compiled(self) -> bool {
        match self {
            Self::Sync => cfg!(feature = "sync-ui"),
            Self::Mermaid | Self::ImportExport | Self::Graph => true,
        }
    }
}

impl fmt::Display for BuiltinModuleId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// 控制第一方模块是否编入的 Cargo feature 门禁。
///
/// 只枚举**真实存在**的门禁；Mermaid/ImportExport/Graph 目前没有可关闭的
/// feature，因此它们的 [`ModuleDescriptor::feature_gate`] 是 `None`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModuleFeatureGate {
    /// `crates/app` 的 `sync-ui` feature（包含 `sync`；默认开启）。
    SyncUi,
}

impl ModuleFeatureGate {
    /// 对应的 Cargo feature 名。
    #[must_use]
    pub const fn cargo_feature(self) -> &'static str {
        match self {
            Self::SyncUi => "sync-ui",
        }
    }
}

impl fmt::Display for ModuleFeatureGate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.cargo_feature())
    }
}

/// 第一方内置模块的描述（只读快照，不含任何运行时句柄）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModuleDescriptor {
    /// 稳定 id。
    pub id: BuiltinModuleId,
    /// 注册表元数据展示名（当前无 UI 消费）。
    pub display_name: &'static str,
    /// 注册表元数据说明（当前无 UI 消费）。
    pub description: &'static str,
    /// 当前构建是否编入该模块。
    pub compiled: bool,
    /// 用户可见启用状态。
    ///
    /// 本切片没有运行时开关/设置项，因此**恒等于 `compiled`**；不要把它当作
    /// 可写的配置项。
    pub enabled: bool,
    /// 编译期门禁（`Some` 表示只有该 feature 打开时才编入）。
    pub feature_gate: Option<ModuleFeatureGate>,
}

const BUILTIN_MODULE_COUNT: usize = BuiltinModuleId::ALL.len();

const BUILTIN_MODULES: [ModuleDescriptor; BUILTIN_MODULE_COUNT] = [
    descriptor(BuiltinModuleId::Sync),
    descriptor(BuiltinModuleId::Mermaid),
    descriptor(BuiltinModuleId::ImportExport),
    descriptor(BuiltinModuleId::Graph),
];

const fn descriptor(id: BuiltinModuleId) -> ModuleDescriptor {
    let compiled = id.is_compiled();
    ModuleDescriptor {
        id,
        display_name: id.display_name(),
        description: id.description(),
        compiled,
        enabled: compiled,
        feature_gate: id.feature_gate(),
    }
}

/// 只读注册表：固定顺序 [`BuiltinModuleId::ALL`]，不 panic、不做 IO。
#[must_use]
pub fn builtin_modules() -> &'static [ModuleDescriptor] {
    &BUILTIN_MODULES
}

/// 按 id 取描述符（固定匹配，不索引数组，因此不会 panic）。
#[must_use]
pub const fn builtin_module(id: BuiltinModuleId) -> ModuleDescriptor {
    descriptor(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn registry_order_is_fixed_and_ids_are_unique() {
        let modules = builtin_modules();
        assert_eq!(modules.len(), BuiltinModuleId::ALL.len());
        assert_eq!(
            modules.iter().map(|module| module.id).collect::<Vec<_>>(),
            BuiltinModuleId::ALL.to_vec()
        );
        // 显式写出顺序，避免只验证“和 ALL 一致”而 ALL 被错误改动。
        assert_eq!(
            modules.iter().map(|module| module.id).collect::<Vec<_>>(),
            vec![
                BuiltinModuleId::Sync,
                BuiltinModuleId::Mermaid,
                BuiltinModuleId::ImportExport,
                BuiltinModuleId::Graph,
            ]
        );

        let ids: HashSet<_> = modules.iter().map(|module| module.id).collect();
        assert_eq!(ids.len(), modules.len(), "模块 id 必须唯一");

        for module in modules {
            assert!(!module.display_name.trim().is_empty(), "{module:?}");
            assert!(!module.description.trim().is_empty(), "{module:?}");
            assert_eq!(module.id.to_string(), module.id.as_str());
        }
    }

    #[test]
    fn sync_compiled_with_sync_ui_feature() {
        let sync = builtin_module(BuiltinModuleId::Sync);
        assert_eq!(sync.feature_gate, Some(ModuleFeatureGate::SyncUi));
        assert_eq!(
            sync.feature_gate.map(ModuleFeatureGate::cargo_feature),
            Some("sync-ui")
        );
        assert_eq!(sync.compiled, cfg!(feature = "sync-ui"));

        #[cfg(feature = "sync-ui")]
        {
            assert!(
                sync.compiled,
                "sync-ui 构建（含默认/dev-sync）必须编入 Sync"
            );
            assert!(sync.enabled);
        }
        #[cfg(not(feature = "sync-ui"))]
        {
            assert!(!sync.compiled, "无 sync-ui 的构建不能编入 Sync");
            assert!(!sync.enabled);
        }
    }

    #[test]
    fn always_compiled_modules_have_no_feature_gate() {
        for id in [
            BuiltinModuleId::Mermaid,
            BuiltinModuleId::ImportExport,
            BuiltinModuleId::Graph,
        ] {
            let module = builtin_module(id);
            assert!(module.compiled, "{id} 当前总是编入");
            assert!(module.enabled, "本切片 enabled 恒等于 compiled");
            assert_eq!(module.feature_gate, None, "{id}");
        }
    }

    #[test]
    fn lookup_matches_registry_for_every_id() {
        for id in BuiltinModuleId::ALL {
            let module = builtin_module(id);
            assert_eq!(module.id, id);
            assert_eq!(
                Some(&module),
                builtin_modules()
                    .iter()
                    .find(|candidate| candidate.id == id)
            );
        }
    }
}
