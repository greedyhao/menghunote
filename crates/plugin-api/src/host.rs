//! Host 服务契约（最小，ADR 0020 §3.3 / §10.5）。
//!
//! 本模块只定义**纯 Rust trait 与 host 自有类型**：不出现 SQLite、密钥、
//! GPUI、storage/sync 类型，也不选择 WASM/独立进程（ADR 0020 §8 问题 2
//! 仍未决）。本切片**不实现加载器、不实现任何运行时**，也没有把这些 trait
//! 接到 app 的 Vault/编辑器上。
//!
//! ## 能力与接口
//!
//! | capability | 本切片接口 | 说明 |
//! | --- | --- | --- |
//! | `notes_read` | [`NotesReadService`] | 列表/读取 host 提供的正文快照 |
//! | `notes_write` | [`NotesWriteService`] | 创建/更新正文；host 负责事务与撤销 |
//! | `settings` | [`SettingsService`] | 读写插件自己的设置项 |
//! | `notifications` | [`NotificationService`] | 向用户显示通知 |
//! | `network` / `filesystem` / `clipboard` / `commands` | **无接口** | 只允许声明，未开放 |
//!
//! [`HostServices`] 是 host 侧“完整实现”的标记 trait，便于未来加载器做静态
//! 约束；**不要**把 `&dyn HostServices` 交给插件。未来加载器应按 manifest
//! 的 capability 构造只实现相应子 trait 的 host 对象（或拒绝未授权调用），
//! 并拒绝任何 manifest 校验失败/未授权能力。本切片没有授权 UI 或加载路径。
//!
//! 所有方法都返回 [`HostError`]；这里不定义错误分类的稳定 wire 码，避免在
//! 插件 API 冻结前锁定错误 taxonomy。

use std::fmt;

use crate::manifest::SettingValue;

/// host 提供的笔记标识（不透明；插件只能把 host 给的值原样传回）。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NoteId(String);

impl NoteId {
    /// 由 host 构造（本切片不做格式校验；它不是持久化 id 的 wire 契约）。
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// 借用原始字符串。
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for NoteId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// 笔记列表项：只有 id 与标题，不含存储/路径/密钥。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteSummary {
    /// 笔记标识。
    pub id: NoteId,
    /// 笔记标题（host 决定未命名笔记的显示）。
    pub title: String,
}

/// 笔记快照：id / 标题 / Markdown 正文。
///
/// 正文是 host 的文档 API 结果，不是 SQLite 行、不是 Loro/CRDT 类型；写回时
/// 由 host 决定合并、撤销与同步边界。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteSnapshot {
    /// 笔记标识。
    pub id: NoteId,
    /// 笔记标题。
    pub title: String,
    /// Markdown 源文。
    pub body: String,
}

/// 通知级别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NotificationLevel {
    /// 普通信息。
    Info,
    /// 警告。
    Warning,
    /// 错误。
    Error,
}

/// 一条用户可见通知。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    /// 级别。
    pub level: NotificationLevel,
    /// 标题。
    pub title: String,
    /// 正文。
    pub body: String,
}

/// host 服务错误。
///
/// 用不透明消息而不是稳定错误码：本切片只要可读、可传播；未来运行时可以
/// 在内部映射，不把 SQLite/网络错误原文暴露给插件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostError {
    message: String,
}

impl HostError {
    /// 由可读消息构造。
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// 错误消息。
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for HostError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for HostError {}

/// `notes_read` 能力：读取 host 的笔记。
pub trait NotesReadService {
    /// 列出当前插件可见的笔记。
    ///
    /// # Errors
    ///
    /// 未授权（capability 未授予）或 host 内部失败时返回 [`HostError`]。
    fn list_notes(&self) -> Result<Vec<NoteSummary>, HostError>;

    /// 读取一篇笔记的快照。
    ///
    /// # Errors
    ///
    /// 未授权、不存在或 host 内部失败时返回 [`HostError`]。
    fn read_note(&self, id: &NoteId) -> Result<NoteSnapshot, HostError>;
}

/// `notes_write` 能力：创建/更新 host 的笔记。
///
/// 本切片不实现编辑器/同步接线；未来 host 必须把写入纳入自己的事务、撤销
/// 与同步边界，不能把 SQLite 或 CRDT 句柄交给插件。
pub trait NotesWriteService {
    /// 创建一篇笔记并返回 host 分配的 id。
    ///
    /// # Errors
    ///
    /// 未授权或 host 内部失败时返回 [`HostError`]。
    fn create_note(&self, title: &str, body: &str) -> Result<NoteId, HostError>;

    /// 更新一篇笔记的正文。
    ///
    /// # Errors
    ///
    /// 未授权、不存在或 host 内部失败时返回 [`HostError`]。
    fn update_note(&self, id: &NoteId, body: &str) -> Result<(), HostError>;
}

/// `settings` 能力：读写插件自己的设置项。
pub trait SettingsService {
    /// 读取设置项；未设置时返回 `None`。
    ///
    /// # Errors
    ///
    /// 未授权或 host 内部失败时返回 [`HostError`]。
    fn get_setting(&self, key: &str) -> Result<Option<SettingValue>, HostError>;

    /// 写入设置项。
    ///
    /// # Errors
    ///
    /// 未授权、值非法或 host 内部失败时返回 [`HostError`]。
    fn set_setting(&self, key: &str, value: SettingValue) -> Result<(), HostError>;
}

/// `notifications` 能力：向用户显示通知。
pub trait NotificationService {
    /// 显示一条通知。
    ///
    /// # Errors
    ///
    /// 未授权或 host 内部失败时返回 [`HostError`]。
    fn notify(&self, notification: Notification) -> Result<(), HostError>;
}

/// host 完整服务集的标记 trait。
///
/// 仅用于 host 侧静态约束；未来加载器应把**子 trait** 交给插件，而不是
/// 本 trait 对象，以免未授权能力被顺手拿到。
pub trait HostServices:
    NotesReadService + NotesWriteService + SettingsService + NotificationService
{
}

impl<T> HostServices for T where
    T: ?Sized + NotesReadService + NotesWriteService + SettingsService + NotificationService
{
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct FakeHost {
        notes: Vec<NoteSnapshot>,
        settings: Vec<(String, SettingValue)>,
    }

    impl NotesReadService for FakeHost {
        fn list_notes(&self) -> Result<Vec<NoteSummary>, HostError> {
            Ok(self
                .notes
                .iter()
                .map(|note| NoteSummary {
                    id: note.id.clone(),
                    title: note.title.clone(),
                })
                .collect())
        }

        fn read_note(&self, id: &NoteId) -> Result<NoteSnapshot, HostError> {
            self.notes
                .iter()
                .find(|note| &note.id == id)
                .cloned()
                .ok_or_else(|| HostError::new("没有这篇笔记"))
        }
    }

    impl NotesWriteService for FakeHost {
        fn create_note(&self, _title: &str, _body: &str) -> Result<NoteId, HostError> {
            // 测试用假 host 只读，不实现真正写入。
            Err(HostError::new("只读测试 host"))
        }

        fn update_note(&self, _id: &NoteId, _body: &str) -> Result<(), HostError> {
            Err(HostError::new("只读测试 host"))
        }
    }

    impl SettingsService for FakeHost {
        fn get_setting(&self, key: &str) -> Result<Option<SettingValue>, HostError> {
            Ok(self
                .settings
                .iter()
                .find(|(stored, _)| stored == key)
                .map(|(_, value)| value.clone()))
        }

        fn set_setting(&self, _key: &str, _value: SettingValue) -> Result<(), HostError> {
            Err(HostError::new("只读测试 host"))
        }
    }

    impl NotificationService for FakeHost {
        fn notify(&self, _notification: Notification) -> Result<(), HostError> {
            Err(HostError::new("只读测试 host"))
        }
    }

    #[test]
    fn capability_traits_are_object_safe_and_composable() {
        let host = FakeHost {
            notes: vec![NoteSnapshot {
                id: NoteId::new("n1"),
                title: "笔记".to_owned(),
                body: "# 正文".to_owned(),
            }],
            settings: vec![("enabled".to_owned(), SettingValue::Bool(true))],
        };

        let notes: &dyn NotesReadService = &host;
        assert_eq!(
            notes.list_notes().expect("列表应成功"),
            [NoteSummary {
                id: NoteId::new("n1"),
                title: "笔记".to_owned()
            }]
        );
        assert_eq!(
            notes
                .read_note(&NoteId::new("n1"))
                .expect("读取应成功")
                .body,
            "# 正文"
        );

        let settings: &dyn SettingsService = &host;
        assert_eq!(
            settings.get_setting("enabled").expect("读取设置应成功"),
            Some(SettingValue::Bool(true))
        );

        fn assert_host_services<T: HostServices + ?Sized>(_host: &T) {}
        assert_host_services(&host);

        let error = notes
            .read_note(&NoteId::new("missing"))
            .expect_err("应失败");
        assert_eq!(error.to_string(), "没有这篇笔记");
        assert_eq!(error.message(), "没有这篇笔记");
    }

    #[test]
    fn host_types_do_not_require_storage_or_gpui() {
        // 只是编译期事实：host 类型可在纯 crate 测试中使用。
        let note = NoteSnapshot {
            id: NoteId::new("id"),
            title: "title".to_owned(),
            body: "body".to_owned(),
        };
        assert_eq!(note.id.as_str(), "id");
        assert_eq!(note.id.to_string(), "id");
        let level = NotificationLevel::Warning;
        assert_eq!(level, NotificationLevel::Warning);
    }
}
