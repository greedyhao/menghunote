//! 插件 manifest 与可读校验错误（ADR 0020 §3.3 / §10.2）。
//!
//! manifest 是声明式的纯数据：`id` / `name` / `version` / `api_version` /
//! `locales` / `capabilities` / `commands` / `panels` / `settings`。字段刻意
//! 保持最小，**不冻结**命令参数、面板布局或设置项 UI schema；未来扩展只能
//! 在 host 明确支持后进行。
//!
//! ## 校验与“拒绝加载”规则
//!
//! [`PluginManifest::validate`] 收集**全部**错误（不短路）：
//!
//! - `id` 必须满足 [`PluginId`] 规则；`name` trim 后非空；
//! - `api_version` 必须落在 [`SUPPORTED_API_VERSIONS`] 精确集合内；
//! - `locales` 必须能被 [`menghunote_i18n::Locale`] 解析（当前 `zh`/`en`
//!   及其常见变体），解析后不得重复；
//! - `capabilities` 不得重复；未知能力在 serde 反序列化阶段就已拒绝；
//! - 同种 spec 内 id trim 后不得为空且不得重复（命令/面板/设置项各自独立
//!   命名空间）；`title`/`slot` 不得为空；设置项 `default` 必须与声明
//!   `type` 一致。
//!
//! **未来加载器必须在 [`PluginManifest::validate`] 返回 `Err` 时拒绝加载**；
//! 本切片只提供契约与校验，不实现加载/安装/卸载、签名校验或权限 UI。
//! JSON 反序列化失败同样必须拒绝加载——serde 会拒绝缺失必需字段、未知
//! capability/type/scope 与非法版本字符串。
//!
//! ## locales 与 i18n 的关系
//!
//! 本 crate 复用纯 crate `menghunote-i18n` 的 [`Locale`] 解析，而不是再写一套
//! 语言标签规则：host 当前只支持中文/英文，manifest 声明其它语言没有可用的
//! 回退目录。将来 i18n 增加语言时，manifest 校验自动跟随同一份事实。

use std::collections::HashSet;
use std::fmt;

use menghunote_i18n::Locale;
use serde::{Deserialize, Serialize};

use crate::capability::Capability;
use crate::id::{PluginId, PluginIdError};
use crate::version::{ApiVersion, PluginVersion, SUPPORTED_API_VERSIONS};

/// 插件 manifest。
///
/// `locales` / `capabilities` / `commands` / `panels` / `settings` 可以省略
/// （serde 默认空集合）；`id` / `name` / `version` / `api_version` 是必需字段。
/// 未知字段被容忍（向前兼容），但未知 capability/type/scope 会被 serde 拒绝。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginManifest {
    /// 插件 id（格式见 [`PluginId`]）。
    pub id: String,
    /// 展示名（必填，trim 后不能为空）。
    pub name: String,
    /// 插件自身版本 `major.minor.patch`。
    pub version: PluginVersion,
    /// 依赖的 host API 版本（当前只支持 `1`）。
    pub api_version: ApiVersion,
    /// 插件提供的 UI 语言（可省略；当前只接受中文/英文变体）。
    #[serde(default)]
    pub locales: Vec<String>,
    /// 申请的能力（可省略；不得重复）。
    #[serde(default)]
    pub capabilities: Vec<Capability>,
    /// 命令声明（可省略；id 在同集合内唯一）。
    #[serde(default)]
    pub commands: Vec<CommandSpec>,
    /// 面板声明（可省略；slot 是 host 定义的不透明字符串）。
    #[serde(default)]
    pub panels: Vec<PanelSpec>,
    /// 设置项声明（可省略；id 在同集合内唯一）。
    #[serde(default)]
    pub settings: Vec<SettingSpec>,
}

impl PluginManifest {
    /// 校验 manifest，返回全部错误。
    ///
    /// 空 `Ok(())` 才允许未来加载器继续；任何一条错误都必须拒绝加载。
    /// 本方法不 panic、不做 IO、不读环境变量。
    ///
    /// # Errors
    ///
    /// 返回按“manifest 级 → commands → panels → settings”顺序收集的错误；
    /// 同一集合内按声明顺序。
    pub fn validate(&self) -> Result<(), Vec<ValidationError>> {
        let mut errors = Vec::new();

        if let Err(reason) = PluginId::parse(&self.id) {
            errors.push(ValidationError::InvalidPluginId {
                id: self.id.clone(),
                reason,
            });
        }
        if self.name.trim().is_empty() {
            errors.push(ValidationError::EmptyName);
        }
        if !SUPPORTED_API_VERSIONS.contains(&self.api_version) {
            errors.push(ValidationError::UnsupportedApiVersion {
                found: self.api_version,
                supported: SUPPORTED_API_VERSIONS,
            });
        }

        self.validate_locales(&mut errors);
        self.validate_capabilities(&mut errors);
        self.validate_commands(&mut errors);
        self.validate_panels(&mut errors);
        self.validate_settings(&mut errors);

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    fn validate_locales(&self, errors: &mut Vec<ValidationError>) {
        let mut seen = HashSet::new();
        for raw in &self.locales {
            match Locale::parse(raw) {
                Some(locale) => {
                    if !seen.insert(locale) {
                        errors.push(ValidationError::DuplicateLocale { value: raw.clone() });
                    }
                }
                None => errors.push(ValidationError::InvalidLocale { value: raw.clone() }),
            }
        }
    }

    fn validate_capabilities(&self, errors: &mut Vec<ValidationError>) {
        let mut seen = HashSet::new();
        for capability in &self.capabilities {
            if !seen.insert(*capability) {
                errors.push(ValidationError::DuplicateCapability {
                    capability: *capability,
                });
            }
        }
    }

    fn validate_commands(&self, errors: &mut Vec<ValidationError>) {
        let mut seen = HashSet::new();
        for (index, command) in self.commands.iter().enumerate() {
            let id = command.id.trim();
            if id.is_empty() {
                errors.push(ValidationError::EmptyCommandId { index });
            } else if !seen.insert(id) {
                errors.push(ValidationError::DuplicateCommandId { id: id.to_owned() });
            }
            if command.title.trim().is_empty() {
                errors.push(ValidationError::EmptyCommandTitle { index });
            }
        }
    }

    fn validate_panels(&self, errors: &mut Vec<ValidationError>) {
        let mut seen = HashSet::new();
        for (index, panel) in self.panels.iter().enumerate() {
            let id = panel.id.trim();
            if id.is_empty() {
                errors.push(ValidationError::EmptyPanelId { index });
            } else if !seen.insert(id) {
                errors.push(ValidationError::DuplicatePanelId { id: id.to_owned() });
            }
            if panel.title.trim().is_empty() {
                errors.push(ValidationError::EmptyPanelTitle { index });
            }
            if panel.slot.trim().is_empty() {
                errors.push(ValidationError::EmptyPanelSlot { index });
            }
        }
    }

    fn validate_settings(&self, errors: &mut Vec<ValidationError>) {
        let mut seen = HashSet::new();
        for (index, setting) in self.settings.iter().enumerate() {
            let id = setting.id.trim();
            if id.is_empty() {
                errors.push(ValidationError::EmptySettingId { index });
            } else if !seen.insert(id) {
                errors.push(ValidationError::DuplicateSettingId { id: id.to_owned() });
            }
            let actual = setting.default.kind();
            if actual != setting.value_type {
                errors.push(ValidationError::SettingDefaultMismatch {
                    index,
                    declared: setting.value_type,
                    actual,
                });
            }
        }
    }
}

/// 命令声明：id / 标题 / 可选说明。参数与快捷键**不在本切片冻结**。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandSpec {
    /// 命令 id（同一个 manifest 内唯一；本切片只要求非空）。
    pub id: String,
    /// 命令标题（不是 i18n key；插件文案命名空间后置）。
    pub title: String,
    /// 可选说明。
    #[serde(default)]
    pub description: Option<String>,
}

/// 面板声明：id / 标题 / host 定义的 slot。
///
/// `slot` 是**不透明字符串**，不是 UISchema：host 只接受自己认识的 slot，
/// 不认识时由未来加载器拒绝/忽略；本切片不定义布局。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PanelSpec {
    /// 面板 id（同一个 manifest 内唯一；本切片只要求非空）。
    pub id: String,
    /// 面板标题。
    pub title: String,
    /// host 定义的挂载位置（不透明字符串，不能为空）。
    pub slot: String,
}

/// 设置项声明：id / 类型 / 默认值 / 作用域。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingSpec {
    /// 设置项 id（同一个 manifest 内唯一；本切片只要求非空）。
    pub id: String,
    /// 值类型（JSON 字段名是 `type`）。
    #[serde(rename = "type")]
    pub value_type: SettingType,
    /// 默认值；必须与 `value_type` 一致。
    pub default: SettingValue,
    /// 作用域（host 决定持久化位置；本切片不实现存储）。
    pub scope: SettingScope,
}

/// 设置项值类型（最小集合，不冻结 UI schema）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingType {
    /// 布尔开关。
    Bool,
    /// 整数。
    Integer,
    /// 文本。
    Text,
}

impl SettingType {
    /// manifest JSON 中的稳定拼写。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Bool => "bool",
            Self::Integer => "integer",
            Self::Text => "text",
        }
    }
}

impl fmt::Display for SettingType {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// 设置项默认值（JSON 中用原生 bool / 整数 / 字符串表示，不加包装对象）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SettingValue {
    /// 布尔值。
    Bool(bool),
    /// 整数（`i64`；本切片不支持浮点）。
    Integer(i64),
    /// 文本。
    Text(String),
}

impl SettingValue {
    /// 对应的 [`SettingType`]。
    #[must_use]
    pub const fn kind(&self) -> SettingType {
        match self {
            Self::Bool(_) => SettingType::Bool,
            Self::Integer(_) => SettingType::Integer,
            Self::Text(_) => SettingType::Text,
        }
    }
}

/// 设置项作用域。
///
/// 只表达“应该存在哪里”的意图，**不承诺同步语义**：当前 host 设置均在本机
/// 文件中，插件设置存储与同步策略后置，由 host 决定。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingScope {
    /// 跟随用户 profile（本机偏好）。
    User,
    /// 跟随某个 vault（由 host 决定持久化位置）。
    Vault,
}

/// manifest 校验失败原因（可读、无 panic）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationError {
    /// 插件 id 不符合 [`PluginId`] 规则。
    InvalidPluginId {
        /// 原始 id。
        id: String,
        /// 具体原因。
        reason: PluginIdError,
    },
    /// `name` 为空或只有空白。
    EmptyName,
    /// `api_version` 不在 host 支持的精确集合内。
    UnsupportedApiVersion {
        /// manifest 声明的版本。
        found: ApiVersion,
        /// host 支持的版本集合。
        supported: &'static [ApiVersion],
    },
    /// locale 无法解析为 host 支持的语言。
    InvalidLocale {
        /// 原始 locale 字符串。
        value: String,
    },
    /// locale 解析后重复（例如 `zh` 与 `zh-CN`）。
    DuplicateLocale {
        /// 后出现的原始 locale 字符串。
        value: String,
    },
    /// capability 重复声明。
    DuplicateCapability {
        /// 重复的能力。
        capability: Capability,
    },
    /// 命令 id 为空。
    EmptyCommandId {
        /// commands 数组下标（0 起）。
        index: usize,
    },
    /// 命令标题为空。
    EmptyCommandTitle {
        /// commands 数组下标（0 起）。
        index: usize,
    },
    /// 命令 id 重复。
    DuplicateCommandId {
        /// trim 后的重复 id。
        id: String,
    },
    /// 面板 id 为空。
    EmptyPanelId {
        /// panels 数组下标（0 起）。
        index: usize,
    },
    /// 面板标题为空。
    EmptyPanelTitle {
        /// panels 数组下标（0 起）。
        index: usize,
    },
    /// 面板 slot 为空。
    EmptyPanelSlot {
        /// panels 数组下标（0 起）。
        index: usize,
    },
    /// 面板 id 重复。
    DuplicatePanelId {
        /// trim 后的重复 id。
        id: String,
    },
    /// 设置项 id 为空。
    EmptySettingId {
        /// settings 数组下标（0 起）。
        index: usize,
    },
    /// 设置项 id 重复。
    DuplicateSettingId {
        /// trim 后的重复 id。
        id: String,
    },
    /// 设置项默认值类型与声明类型不一致。
    SettingDefaultMismatch {
        /// settings 数组下标（0 起）。
        index: usize,
        /// manifest 声明的类型。
        declared: SettingType,
        /// 默认值实际类型。
        actual: SettingType,
    },
}

impl fmt::Display for ValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPluginId { id, reason } => {
                write!(formatter, "插件 id {id:?} 非法：{reason}")
            }
            Self::EmptyName => formatter.write_str("插件 name 不能为空"),
            Self::UnsupportedApiVersion { found, supported } => {
                let supported = supported
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(
                    formatter,
                    "不支持的 api_version {found}（host 支持：{supported}）"
                )
            }
            Self::InvalidLocale { value } => {
                write!(
                    formatter,
                    "不支持的 locale {value:?}（当前只支持 zh-CN / en）"
                )
            }
            Self::DuplicateLocale { value } => {
                write!(formatter, "重复的 locale {value:?}（解析为同一语言）")
            }
            Self::DuplicateCapability { capability } => {
                write!(formatter, "重复的 capability {capability:?}")
            }
            Self::EmptyCommandId { index } => {
                write!(formatter, "commands[{index}].id 不能为空")
            }
            Self::EmptyCommandTitle { index } => {
                write!(formatter, "commands[{index}].title 不能为空")
            }
            Self::DuplicateCommandId { id } => {
                write!(formatter, "重复的 command id {id:?}")
            }
            Self::EmptyPanelId { index } => {
                write!(formatter, "panels[{index}].id 不能为空")
            }
            Self::EmptyPanelTitle { index } => {
                write!(formatter, "panels[{index}].title 不能为空")
            }
            Self::EmptyPanelSlot { index } => {
                write!(formatter, "panels[{index}].slot 不能为空")
            }
            Self::DuplicatePanelId { id } => {
                write!(formatter, "重复的 panel id {id:?}")
            }
            Self::EmptySettingId { index } => {
                write!(formatter, "settings[{index}].id 不能为空")
            }
            Self::DuplicateSettingId { id } => {
                write!(formatter, "重复的 setting id {id:?}")
            }
            Self::SettingDefaultMismatch {
                index,
                declared,
                actual,
            } => write!(
                formatter,
                "settings[{index}].default 类型 {actual} 与声明的 type {declared} 不一致"
            ),
        }
    }
}

impl std::error::Error for ValidationError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_manifest() -> PluginManifest {
        PluginManifest {
            id: "com.example.notes".to_owned(),
            name: "Example Notes".to_owned(),
            version: PluginVersion::new(1, 2, 3),
            api_version: ApiVersion::CURRENT,
            locales: vec!["zh-CN".to_owned(), "en".to_owned()],
            capabilities: vec![
                Capability::NotesRead,
                Capability::NotesWrite,
                Capability::Settings,
            ],
            commands: vec![
                CommandSpec {
                    id: "open-dashboard".to_owned(),
                    title: "Open dashboard".to_owned(),
                    description: Some("打开概览".to_owned()),
                },
                CommandSpec {
                    id: "count-words".to_owned(),
                    title: "Count words".to_owned(),
                    description: None,
                },
            ],
            panels: vec![PanelSpec {
                id: "dashboard".to_owned(),
                title: "Dashboard".to_owned(),
                slot: "right-sidebar".to_owned(),
            }],
            settings: vec![
                SettingSpec {
                    id: "enabled".to_owned(),
                    value_type: SettingType::Bool,
                    default: SettingValue::Bool(true),
                    scope: SettingScope::User,
                },
                SettingSpec {
                    id: "limit".to_owned(),
                    value_type: SettingType::Integer,
                    default: SettingValue::Integer(10),
                    scope: SettingScope::Vault,
                },
                SettingSpec {
                    id: "greeting".to_owned(),
                    value_type: SettingType::Text,
                    default: SettingValue::Text("hello".to_owned()),
                    scope: SettingScope::User,
                },
            ],
        }
    }

    fn validate(manifest: &PluginManifest) -> Vec<ValidationError> {
        manifest.validate().expect_err("构造的 manifest 应校验失败")
    }

    #[test]
    fn complete_manifest_passes_validation() {
        assert_eq!(valid_manifest().validate(), Ok(()));
    }

    #[test]
    fn manifest_level_errors_are_collected_without_short_circuit() {
        let mut manifest = valid_manifest();
        manifest.id = "bad_id".to_owned();
        manifest.name = "  ".to_owned();
        manifest.api_version = ApiVersion::new(2);
        manifest.locales = vec!["fr".to_owned(), "zh".to_owned(), "zh-CN".to_owned()];
        manifest.capabilities = vec![
            Capability::NotesRead,
            Capability::NotesRead,
            Capability::Network,
        ];

        let errors = validate(&manifest);
        assert!(errors.contains(&ValidationError::EmptyName));
        assert!(errors.iter().any(|error| matches!(
            error,
            ValidationError::InvalidPluginId {
                reason: PluginIdError::InvalidCharacter { character: '_' },
                ..
            }
        )));
        assert!(errors.iter().any(|error| matches!(
            error,
            ValidationError::UnsupportedApiVersion { found, .. } if found.get() == 2
        )));
        assert!(errors.contains(&ValidationError::InvalidLocale {
            value: "fr".to_owned()
        }));
        assert!(errors.contains(&ValidationError::DuplicateLocale {
            value: "zh-CN".to_owned()
        }));
        assert!(errors.contains(&ValidationError::DuplicateCapability {
            capability: Capability::NotesRead
        }));
        assert_eq!(errors.len(), 6, "{errors:#?}");
    }

    #[test]
    fn spec_ids_must_be_non_empty_and_unique() {
        let mut manifest = valid_manifest();
        manifest.commands.push(CommandSpec {
            id: " open-dashboard ".to_owned(),
            title: " ".to_owned(),
            description: None,
        });
        manifest.panels = vec![
            PanelSpec {
                id: "".to_owned(),
                title: "".to_owned(),
                slot: " ".to_owned(),
            },
            PanelSpec {
                id: "dashboard".to_owned(),
                title: "Dashboard".to_owned(),
                slot: "right-sidebar".to_owned(),
            },
            PanelSpec {
                id: "dashboard".to_owned(),
                title: "Duplicate".to_owned(),
                slot: "bottom".to_owned(),
            },
        ];
        manifest.settings = vec![
            SettingSpec {
                id: "enabled".to_owned(),
                value_type: SettingType::Bool,
                default: SettingValue::Bool(true),
                scope: SettingScope::User,
            },
            SettingSpec {
                id: "enabled".to_owned(),
                value_type: SettingType::Integer,
                default: SettingValue::Text("wrong".to_owned()),
                scope: SettingScope::Vault,
            },
        ];

        let errors = validate(&manifest);
        assert!(errors.contains(&ValidationError::EmptyCommandTitle { index: 2 }));
        assert!(errors.contains(&ValidationError::DuplicateCommandId {
            id: "open-dashboard".to_owned()
        }));
        assert!(errors.contains(&ValidationError::EmptyPanelId { index: 0 }));
        assert!(errors.contains(&ValidationError::EmptyPanelTitle { index: 0 }));
        assert!(errors.contains(&ValidationError::EmptyPanelSlot { index: 0 }));
        assert!(errors.contains(&ValidationError::DuplicatePanelId {
            id: "dashboard".to_owned()
        }));
        assert!(errors.contains(&ValidationError::DuplicateSettingId {
            id: "enabled".to_owned()
        }));
        assert!(errors.contains(&ValidationError::SettingDefaultMismatch {
            index: 1,
            declared: SettingType::Integer,
            actual: SettingType::Text,
        }));
    }

    #[test]
    fn setting_defaults_match_their_declared_types() {
        for (value_type, default) in [
            (SettingType::Bool, SettingValue::Bool(false)),
            (SettingType::Integer, SettingValue::Integer(-1)),
            (SettingType::Text, SettingValue::Text(String::new())),
        ] {
            assert_eq!(default.kind(), value_type);
        }
    }

    #[test]
    fn validation_errors_are_readable_and_implement_error() {
        let errors = validate(&{
            let mut manifest = valid_manifest();
            manifest.id = String::new();
            manifest.name = String::new();
            manifest
        });
        assert_eq!(errors.len(), 2);
        for error in &errors {
            let text = error.to_string();
            assert!(!text.is_empty(), "{error:?}");
            let _: &dyn std::error::Error = error;
        }
    }
}
