//! 第一方「日记」核心插件（Obsidian 风格）。
//!
//! ## 行为（已确认的产品决定）
//!
//! - 侧栏动作行「今日日记」按钮：计算 `folder + "/" + date_format` 的路径；
//! - 该路径已有笔记 → **直接打开**（不重复创建、不覆盖）；
//! - 没有 → 自动创建所需目录 + 笔记（结构写入一条 `StructureWriter` 提交）；
//!   若有模板，按源文读取并用占位符替换后作为正文；模板丢失时**非阻塞提示**并创建空日记；
//! - 日期格式里的 `/` 产生**子目录**（`日记/2026/11/18`；标题 = 最后一段）。
//!
//! ## 设置（本机 `daily-notes.json`）
//!
//! `enabled` / `date_format` / `folder` / `template`；只写本机配置目录，不同步、不写正文。
//! `template` 是**任意普通笔记路径**（精确匹配），不要求和模板插件文件夹一致；空 = 无模板。
//!
//! ## 占位符
//!
//! 复用模板插件的同一套引擎（`{{date}}` / `{{time}}` / `{{title}}` /
//! `{{date:FORMAT}}` / `{{time:FORMAT}}`）。`{{title}}` 是**日记笔记标题**（最后一段）；
//! 默认日期/时间格式取模板插件设置里的 `date_format` / `time_format`（只读，不写盘）。
//!
//! ## selector（headless 测试点击/断言用）
//!
//! | selector | 含义 |
//! | --- | --- |
//! | `settings-daily-notes` | 设置页「日记」区根 |
//! | `settings-daily-notes-toggle` / `-state` | 启用开关 / 当前状态文本 |
//! | `settings-plugin-daily_notes` / `-toggle` / `-state` | 「插件」分类里的日记总开关（同一份 `enabled`；由 `AppRoot` 渲染） |
//! | `settings-daily-notes-folder` / `-date-format` / `-template` | 三个输入框 |
//! | `settings-daily-notes-example` / `-notice` | 今天路径示例 / 保存提示 |
//!
//! ## 已知限制
//!
//! - 路径按**精确字符串**匹配（大小写敏感），与笔记树一致；
//! - 不实现「昨天/明天」相对日期、日历视图、周期笔记；
//! - 模板路径没有选择器（手填路径；后续可接模板选择器）。

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

use gpui::prelude::*;
use gpui::{
    div, px, Context, Entity, IntoElement, MouseButton, Render, SharedString, Subscription, Window,
};
use menghunote_storage::TreeEntry;

use crate::editor::theme;
use crate::sidebar_rename::{RenameEvent, RenameInput};
use crate::templates::{self, DateTimeParts};
use crate::ui_text;

/// 偏好文件当前版本。
pub const DAILY_NOTES_PREFERENCES_VERSION: u32 = 1;

/// 偏好文件名（平台配置目录下）。
pub const DAILY_NOTES_FILE_NAME: &str = "daily-notes.json";

/// 测试/便携用的路径覆盖环境变量。
pub const DAILY_NOTES_PATH_ENV: &str = "MENGHUNOTE_DAILY_NOTES_PATH";

/// 默认日记文件夹（新笔记位置）。
pub const DEFAULT_DAILY_FOLDER: &str = "日记";

/// 默认日期格式（`/` 产生子目录）。
pub const DEFAULT_DAILY_DATE_FORMAT: &str = "YYYY-MM-DD";

/// 单篇日记路径的最大目录层数（防御病态格式，不是产品限制）。
pub const MAX_DAILY_SEGMENTS: usize = 64;

// ---------------------------------------------------------------------------
// 本机偏好（daily-notes.json）
// ---------------------------------------------------------------------------

/// 日记插件本机偏好。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DailyNotesPreferences {
    /// 是否启用（默认 `true`）。
    pub enabled: bool,
    /// 日期格式；`/` 产生子目录。
    pub date_format: String,
    /// 新笔记位置（多级目录）。
    pub folder: String,
    /// 可选模板的**普通笔记路径**（空 = 无模板）。
    pub template: String,
}

impl Default for DailyNotesPreferences {
    fn default() -> Self {
        Self {
            enabled: true,
            date_format: DEFAULT_DAILY_DATE_FORMAT.to_owned(),
            folder: DEFAULT_DAILY_FOLDER.to_owned(),
            template: String::new(),
        }
    }
}

impl DailyNotesPreferences {
    /// 从 JSON 文本解析；损坏/未知版本回退默认（逐字段容错）。
    #[must_use]
    pub fn parse(text: &str) -> Self {
        let Some(version) = templates::json_number_field(text, "version") else {
            return Self::default();
        };
        if version != DAILY_NOTES_PREFERENCES_VERSION {
            return Self::default();
        }
        let defaults = Self::default();
        Self {
            enabled: templates::json_bool_field(text, "enabled").unwrap_or(defaults.enabled),
            date_format: templates::json_string_field(text, "date_format")
                .unwrap_or(defaults.date_format),
            folder: templates::json_string_field(text, "folder").unwrap_or(defaults.folder),
            template: templates::json_string_field(text, "template").unwrap_or(defaults.template),
        }
    }

    /// 序列化为稳定 JSON。
    #[must_use]
    pub fn to_json(&self) -> String {
        format!(
            "{{\n  \"version\": {DAILY_NOTES_PREFERENCES_VERSION},\n  \"enabled\": {},\n  \"date_format\": \"{}\",\n  \"folder\": \"{}\",\n  \"template\": \"{}\"\n}}\n",
            self.enabled,
            templates::json_escape(&self.date_format),
            templates::json_escape(&self.folder),
            templates::json_escape(&self.template),
        )
    }

    /// 规整后的日记文件夹（空 → 回退默认，避免把日记建到根级）。
    #[must_use]
    pub fn normalized_folder(&self) -> String {
        let normalized = templates::normalize_folder(&self.folder);
        if normalized.is_empty() {
            templates::normalize_folder(DEFAULT_DAILY_FOLDER)
        } else {
            normalized
        }
    }

    /// 规整后的模板笔记路径（空 = 无模板）。
    #[must_use]
    pub fn normalized_template(&self) -> String {
        normalize_note_path(&self.template)
    }
}

/// 规整笔记路径：反斜杠转 `/`、去首尾空白/斜杠、折叠空段。
#[must_use]
pub fn normalize_note_path(path: &str) -> String {
    path.replace('\\', "/")
        .split('/')
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>()
        .join("/")
}

// ---------------------------------------------------------------------------
// 本地路径/读写
// ---------------------------------------------------------------------------

/// 平台配置目录下的日记偏好文件路径。
#[must_use]
pub fn settings_path() -> Option<PathBuf> {
    templates::config_path(DAILY_NOTES_PATH_ENV, DAILY_NOTES_FILE_NAME)
}

/// 读取偏好；文件缺失/损坏回退默认。
#[must_use]
pub fn load_preferences(path: &Path) -> DailyNotesPreferences {
    let Ok(text) = std::fs::read_to_string(path) else {
        return DailyNotesPreferences::default();
    };
    DailyNotesPreferences::parse(&text)
}

/// 原子写入偏好。
pub fn save_preferences(path: &Path, preferences: &DailyNotesPreferences) -> io::Result<()> {
    templates::atomic_write(path, &preferences.to_json())
}

/// 进程级偏好缓存（`None` = 尚未加载）。
static STATE: RwLock<Option<DailyNotesPreferences>> = RwLock::new(None);
/// 写盘目标。
static STORE_PATH: RwLock<Option<PathBuf>> = RwLock::new(None);

fn read_state() -> RwLockReadGuard<'static, Option<DailyNotesPreferences>> {
    STATE.read().unwrap_or_else(|error| error.into_inner())
}

fn write_state() -> RwLockWriteGuard<'static, Option<DailyNotesPreferences>> {
    STATE.write().unwrap_or_else(|error| error.into_inner())
}

fn read_store_path() -> RwLockReadGuard<'static, Option<PathBuf>> {
    STORE_PATH.read().unwrap_or_else(|error| error.into_inner())
}

fn write_store_path() -> RwLockWriteGuard<'static, Option<PathBuf>> {
    STORE_PATH
        .write()
        .unwrap_or_else(|error| error.into_inner())
}

/// 当前日记偏好（首次调用时从平台配置目录懒加载）。
#[must_use]
pub fn preferences() -> DailyNotesPreferences {
    let cached = read_state().clone();
    if let Some(preferences) = cached {
        return preferences;
    }
    let (loaded, path) = match settings_path() {
        Some(path) => (load_preferences(&path), Some(path)),
        None => (DailyNotesPreferences::default(), None),
    };
    *write_state() = Some(loaded.clone());
    *write_store_path() = path;
    loaded
}

/// 覆盖当前偏好并写盘（`true` = 真的变了）。
pub fn set_preferences(next: DailyNotesPreferences) -> bool {
    let changed = preferences() != next;
    *write_state() = Some(next.clone());
    if changed {
        persist(&next);
    }
    changed
}

fn persist(preferences: &DailyNotesPreferences) {
    let path = read_store_path().clone().or_else(settings_path);
    let Some(path) = path else {
        return;
    };
    *write_store_path() = Some(path.clone());
    if let Err(error) = save_preferences(&path, preferences) {
        eprintln!("[menghunote] 保存日记偏好失败：{error}");
    }
}

/// 从显式路径加载偏好（测试/便携；同时记住写盘目标）。
#[doc(hidden)]
pub fn init_from_path(path: PathBuf) {
    let preferences = load_preferences(&path);
    *write_state() = Some(preferences);
    *write_store_path() = Some(path);
}

// ---------------------------------------------------------------------------
// 日记路径（纯逻辑）
// ---------------------------------------------------------------------------

/// 今天的日记路径分解。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DailyNotePath {
    /// 完整路径（`日记/2026/11/18`）。
    pub full: String,
    /// 父目录路径（`日记/2026/11`；为空表示根级）。
    pub folder: String,
    /// 笔记标题（最后一段，`18`）。
    pub title: String,
    /// 全部路径段（目录段在前，标题在最后）。
    pub segments: Vec<String>,
}

/// 路径计算失败的原因（UI 只展示固定文案，不回显用户原文以外的路径）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DailyPathError {
    /// 日期格式没有产出任何路径段。
    EmptyDate,
    /// 路径层数超过 [`MAX_DAILY_SEGMENTS`]（病态格式）。
    TooDeep,
}

impl DailyPathError {
    /// 给用户看的原因（当前语言）。
    #[must_use]
    pub fn message(self) -> String {
        match self {
            Self::EmptyDate | Self::TooDeep => ui_text::daily_notes_invalid_path(),
        }
    }
}

/// 计算某一天的日记路径：`folder + "/" + format_datetime(date_format)`。
pub fn note_path(
    preferences: &DailyNotesPreferences,
    parts: &DateTimeParts,
) -> Result<DailyNotePath, DailyPathError> {
    let mut segments: Vec<String> = preferences
        .normalized_folder()
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(str::to_owned)
        .collect();
    let rendered = templates::format_datetime(parts, &preferences.date_format);
    let date_segments: Vec<String> = rendered
        .split('/')
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .map(str::to_owned)
        .collect();
    if date_segments.is_empty() {
        return Err(DailyPathError::EmptyDate);
    }
    segments.extend(date_segments);
    if segments.len() > MAX_DAILY_SEGMENTS {
        return Err(DailyPathError::TooDeep);
    }
    let title = segments.last().cloned().unwrap_or_default();
    let folder = segments[..segments.len() - 1].join("/");
    Ok(DailyNotePath {
        full: segments.join("/"),
        folder,
        title,
        segments,
    })
}

/// 解析到的日记模板（普通笔记）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DailyTemplateNote {
    /// 树里的下标（打开/读取用）。
    pub index: usize,
    /// 笔记标题。
    pub title: String,
    /// 正文文档 ID。
    pub body_doc_id: String,
}

/// 按设置里的模板路径找**普通笔记**（精确路径；找不到 = 模板丢失）。
#[must_use]
pub fn resolve_template(
    tree: &[TreeEntry],
    preferences: &DailyNotesPreferences,
) -> Option<DailyTemplateNote> {
    let path = preferences.normalized_template();
    if path.is_empty() {
        return None;
    }
    let index = templates::note_index_by_path(tree, &path)?;
    let entry = tree.get(index)?;
    Some(DailyTemplateNote {
        index,
        title: entry.title.clone(),
        body_doc_id: entry.body_doc_id.clone()?,
    })
}

// ---------------------------------------------------------------------------
// 设置页「日记」区（自包含实体）
// ---------------------------------------------------------------------------

/// 日记设置里的可编辑字段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DailyNotesField {
    /// 日记文件夹。
    Folder,
    /// 日期格式。
    DateFormat,
    /// 模板笔记路径（可空）。
    Template,
}

impl DailyNotesField {
    /// 所有字段（渲染顺序）。
    pub const ALL: [Self; 3] = [Self::Folder, Self::DateFormat, Self::Template];

    /// 固定 selector。
    #[must_use]
    pub const fn selector(self) -> &'static str {
        match self {
            Self::Folder => "settings-daily-notes-folder",
            Self::DateFormat => "settings-daily-notes-date-format",
            Self::Template => "settings-daily-notes-template",
        }
    }

    /// 字段展示名（当前语言）。
    #[must_use]
    pub fn label(self) -> String {
        match self {
            Self::Folder => ui_text::daily_notes_folder_label(),
            Self::DateFormat => ui_text::daily_notes_date_format_label(),
            Self::Template => ui_text::daily_notes_template_label(),
        }
    }
}

/// 设置页「日记」区：自包含实体（三个 IME 输入框 + 启用开关）。
pub struct DailyNotesSettingsPanel {
    fields: Vec<(DailyNotesField, Entity<RenameInput>)>,
    notice: Option<String>,
    _subs: Vec<Subscription>,
}

/// 日记设置面板事件：设置页左栏（插件分类可见性）需要即时刷新。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DailyNotesSettingsEvent {
    /// `enabled` 变化（在本面板里打开/关闭日记插件）。
    EnabledChanged,
}

impl gpui::EventEmitter<DailyNotesSettingsEvent> for DailyNotesSettingsPanel {}

impl DailyNotesSettingsPanel {
    /// 按当前偏好组装面板。
    pub fn new(cx: &mut Context<Self>) -> Self {
        let preferences = preferences();
        let mut fields = Vec::with_capacity(DailyNotesField::ALL.len());
        let mut subs = Vec::with_capacity(DailyNotesField::ALL.len());
        for field in DailyNotesField::ALL {
            let initial = match field {
                DailyNotesField::Folder => preferences.folder.clone(),
                DailyNotesField::DateFormat => preferences.date_format.clone(),
                DailyNotesField::Template => preferences.template.clone(),
            };
            // 模板路径允许空（= 无模板），用 Cell 模式（Enter 提交原始文本）。
            let input = cx.new(|cx| match field {
                DailyNotesField::Template => RenameInput::new_cell_at_end(&initial, cx),
                _ => RenameInput::new_at_end(&initial, cx),
            });
            subs.push(cx.subscribe(
                &input,
                move |panel: &mut Self, _input, event: &RenameEvent, cx| {
                    panel.on_field_event(field, event, cx);
                },
            ));
            fields.push((field, input));
        }
        Self {
            fields,
            notice: None,
            _subs: subs,
        }
    }

    /// 某个字段的输入框（测试/诊断用）。
    #[must_use]
    pub fn field_input(&self, field: DailyNotesField) -> Option<Entity<RenameInput>> {
        self.fields
            .iter()
            .find(|(current, _)| *current == field)
            .map(|(_, input)| input.clone())
    }

    /// 保存提示。
    #[must_use]
    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    /// 今天路径的展示文本（失败时给固定错误文案）。
    #[must_use]
    pub fn example(preferences: &DailyNotesPreferences, parts: &DateTimeParts) -> String {
        match note_path(preferences, parts) {
            Ok(path) => path.full,
            Err(error) => error.message(),
        }
    }

    fn on_field_event(
        &mut self,
        field: DailyNotesField,
        event: &RenameEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            RenameEvent::Commit(text) => self.commit_field(field, text, cx),
            RenameEvent::Blank => {
                self.notice = Some(ui_text::daily_notes_rejected());
                cx.notify();
            }
            RenameEvent::Cancel | RenameEvent::Changed | RenameEvent::Advance { .. } => {}
        }
    }

    fn commit_field(&mut self, field: DailyNotesField, text: &str, cx: &mut Context<Self>) {
        let mut preferences = preferences();
        let value = text.trim().to_owned();
        let valid = match field {
            DailyNotesField::Folder => !templates::normalize_folder(&value).is_empty(),
            DailyNotesField::DateFormat => {
                !templates::format_datetime(&DateTimeParts::now(), &value)
                    .trim()
                    .is_empty()
            }
            // 空 = 无模板，合法。
            DailyNotesField::Template => true,
        };
        if !valid {
            self.notice = Some(ui_text::daily_notes_rejected());
            cx.notify();
            return;
        }
        match field {
            DailyNotesField::Folder => preferences.folder = value,
            DailyNotesField::DateFormat => preferences.date_format = value,
            DailyNotesField::Template => preferences.template = normalize_note_path(&value),
        }
        set_preferences(preferences);
        self.notice = Some(ui_text::daily_notes_saved());
        cx.notify();
    }

    fn toggle_enabled(&mut self, cx: &mut Context<Self>) {
        let mut preferences = preferences();
        preferences.enabled = !preferences.enabled;
        set_preferences(preferences);
        self.notice = None;
        cx.emit(DailyNotesSettingsEvent::EnabledChanged);
        cx.notify();
    }

    fn field_row(&self, field: DailyNotesField) -> impl IntoElement {
        let Some(input) = self.field_input(field) else {
            return div();
        };
        let focus_input = input.clone();
        let selector = field.selector();
        div()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .w_full()
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(theme::status_bar_text())
                    .child(SharedString::from(field.label())),
            )
            .child(
                div()
                    .id(selector)
                    .debug_selector(move || selector.to_string())
                    .w_full()
                    .h(px(26.0))
                    .px_2()
                    .flex()
                    .items_center()
                    .bg(theme::input_bg())
                    .border_1()
                    .border_color(theme::input_border())
                    .rounded_sm()
                    .capture_any_mouse_down(move |_event, window, cx| {
                        focus_input.update(cx, |input, cx| input.focus_for_edit(window, cx));
                    })
                    .child(div().flex_1().min_w(px(0.0)).h_full().child(input)),
            )
    }
}

impl Render for DailyNotesSettingsPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let preferences = preferences();
        let parts = DateTimeParts::now();
        let example = Self::example(&preferences, &parts);
        let state_text = if preferences.enabled {
            ui_text::daily_notes_enabled()
        } else {
            ui_text::daily_notes_disabled()
        };
        let (background, text) = if preferences.enabled {
            (
                theme::search_toggle_active(),
                theme::search_toggle_active_text(),
            )
        } else {
            (theme::menu_bg(), theme::text_color())
        };
        div()
            .id("settings-daily-notes")
            .debug_selector(|| "settings-daily-notes".to_string())
            .w_full()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(theme::text_color())
                    .child(SharedString::from(ui_text::daily_notes_title())),
            )
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(theme::status_bar_text())
                    .child(SharedString::from(ui_text::daily_notes_hint())),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .id("settings-daily-notes-toggle")
                            .debug_selector(|| "settings-daily-notes-toggle".to_string())
                            .px_3()
                            .py_1()
                            .rounded_sm()
                            .border_1()
                            .border_color(theme::input_border())
                            .bg(background)
                            .text_size(px(12.0))
                            .text_color(text)
                            .cursor_pointer()
                            .child(SharedString::from(state_text.clone()))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|panel, _event, _window, cx| {
                                    panel.toggle_enabled(cx);
                                }),
                            ),
                    )
                    .child(
                        div()
                            .debug_selector(|| "settings-daily-notes-state".to_string())
                            .text_size(px(11.0))
                            .text_color(theme::status_bar_text())
                            .child(SharedString::from(state_text)),
                    ),
            )
            .children(
                DailyNotesField::ALL
                    .into_iter()
                    .map(|field| self.field_row(field)),
            )
            .child(
                div()
                    .debug_selector(|| "settings-daily-notes-template-hint".to_string())
                    .text_size(px(11.0))
                    .text_color(theme::status_bar_text())
                    .child(SharedString::from(ui_text::daily_notes_template_hint())),
            )
            .child(
                div()
                    .debug_selector(|| "settings-daily-notes-example".to_string())
                    .text_size(px(11.0))
                    .text_color(theme::status_bar_text())
                    .child(SharedString::from(ui_text::daily_notes_example(&example))),
            )
            .child(
                div()
                    .debug_selector(|| "settings-daily-notes-notice".to_string())
                    .text_size(px(11.0))
                    .text_color(theme::caret_color())
                    .child(SharedString::from(self.notice.clone().unwrap_or_default())),
            )
    }
}

// ---------------------------------------------------------------------------
// 测试钩子
// ---------------------------------------------------------------------------

/// 偏好是进程级全局状态，改它的测试必须串行。
static TEST_LOCK: Mutex<()> = Mutex::new(());

/// 日记偏好测试守卫。
#[doc(hidden)]
pub struct DailyNotesTestGuard {
    _lock: MutexGuard<'static, ()>,
}

impl DailyNotesTestGuard {
    /// 重置到默认值并指定写盘目标。
    pub fn reset(&self, store_path: Option<PathBuf>) {
        daily_notes_test_reset(store_path);
    }

    /// 从给定路径加载（模拟重启）。
    pub fn init(&self, path: PathBuf) {
        init_from_path(path);
    }
}

impl Drop for DailyNotesTestGuard {
    fn drop(&mut self) {
        daily_notes_test_reset(None);
    }
}

fn daily_notes_test_reset(store_path: Option<PathBuf>) {
    *write_state() = Some(DailyNotesPreferences::default());
    *write_store_path() = store_path;
}

/// 串行化日记偏好测试并自动复位。
#[doc(hidden)]
#[must_use]
pub fn lock_for_test() -> DailyNotesTestGuard {
    let lock = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    // 先置默认 + 清空写盘目标：避免测试进程懒加载真实用户配置。
    daily_notes_test_reset(None);
    DailyNotesTestGuard { _lock: lock }
}

// ---------------------------------------------------------------------------
// 单测（纯逻辑）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use menghunote_storage::NodeKind;

    fn parts() -> DateTimeParts {
        DateTimeParts {
            year: 2026,
            month: 11,
            day: 18,
            hour: 8,
            minute: 14,
            second: 9,
        }
    }

    fn entry(node_id: &str, title: &str, parent: Option<&str>, kind: NodeKind) -> TreeEntry {
        TreeEntry {
            node_id: node_id.to_owned(),
            note_id: (kind == NodeKind::Note).then(|| format!("{node_id}-note")),
            body_doc_id: (kind == NodeKind::Note).then(|| format!("{node_id}-body")),
            title: title.to_owned(),
            parent_id: parent.map(str::to_owned),
            kind,
            sibling_rank: 0,
            depth: 0,
            created_at: None,
            updated_at: None,
        }
    }

    fn sample_tree() -> Vec<TreeEntry> {
        vec![
            entry("t-templates", "模板", None, NodeKind::Folder),
            entry("t-weekly", "日记模板", Some("t-templates"), NodeKind::Note),
            entry("t-diary", "日记", None, NodeKind::Folder),
            entry("t-note", "普通", None, NodeKind::Note),
        ]
    }

    fn test_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/tmp")
            .join(format!("daily-notes-preferences-{}", std::process::id()))
    }

    #[test]
    fn builds_nested_path_and_title_from_date_format() {
        let preferences = DailyNotesPreferences::default();
        let path = note_path(&preferences, &parts()).expect("路径必须可算");
        assert_eq!(path.full, "日记/2026-11-18");
        assert_eq!(path.folder, "日记");
        assert_eq!(path.title, "2026-11-18");

        let nested = DailyNotesPreferences {
            date_format: "YYYY/MM/DD".to_owned(),
            folder: "日记/晨间".to_owned(),
            ..DailyNotesPreferences::default()
        };
        let path = note_path(&nested, &parts()).expect("路径必须可算");
        assert_eq!(path.full, "日记/晨间/2026/11/18");
        assert_eq!(path.folder, "日记/晨间/2026/11");
        assert_eq!(path.title, "18");
        assert_eq!(
            path.segments,
            vec!["日记", "晨间", "2026", "11", "18"]
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        );

        // 同日多次计算必须完全一致（幂等；"已存在就打开"依赖它）。
        assert_eq!(note_path(&nested, &parts()), note_path(&nested, &parts()));
    }

    #[test]
    fn invalid_date_format_is_reported_not_panicked() {
        let preferences = DailyNotesPreferences {
            date_format: "/".to_owned(),
            ..DailyNotesPreferences::default()
        };
        assert_eq!(
            note_path(&preferences, &parts()),
            Err(DailyPathError::EmptyDate)
        );

        let deep = DailyNotesPreferences {
            date_format: "YYYY/MM/DD/HH/mm/ss".to_owned(),
            folder: (0..64)
                .map(|index| format!("d{index}"))
                .collect::<Vec<_>>()
                .join("/"),
            ..DailyNotesPreferences::default()
        };
        assert_eq!(note_path(&deep, &parts()), Err(DailyPathError::TooDeep));
    }

    #[test]
    fn empty_folder_falls_back_to_default() {
        let preferences = DailyNotesPreferences {
            folder: "  / ".to_owned(),
            ..DailyNotesPreferences::default()
        };
        assert_eq!(preferences.normalized_folder(), "日记");
        assert_eq!(
            note_path(&preferences, &parts()).map(|path| path.full),
            Ok("日记/2026-11-18".to_owned())
        );
    }

    #[test]
    fn resolves_template_by_ordinary_note_path() {
        let tree = sample_tree();
        let no_template = DailyNotesPreferences::default();
        assert_eq!(resolve_template(&tree, &no_template), None);

        let preferences = DailyNotesPreferences {
            template: " 模板\\日记模板 ".to_owned(),
            ..DailyNotesPreferences::default()
        };
        assert_eq!(preferences.normalized_template(), "模板/日记模板");
        assert_eq!(
            resolve_template(&tree, &preferences),
            Some(DailyTemplateNote {
                index: 1,
                title: "日记模板".to_owned(),
                body_doc_id: "t-weekly-body".to_owned(),
            })
        );

        // 模板丢失 / 指向目录 / 指向模板文件夹本身：都是 None（非阻塞提示路径）。
        let missing = DailyNotesPreferences {
            template: "模板/不存在".to_owned(),
            ..DailyNotesPreferences::default()
        };
        assert_eq!(resolve_template(&tree, &missing), None);
        let folder = DailyNotesPreferences {
            template: "日记".to_owned(),
            ..DailyNotesPreferences::default()
        };
        assert_eq!(resolve_template(&tree, &folder), None);
    }

    #[test]
    fn preferences_json_round_trips_and_rejects_corruption() {
        let preferences = DailyNotesPreferences {
            enabled: false,
            date_format: "YYYY/MM/DD".to_owned(),
            folder: "日记/晨间".to_owned(),
            template: "模板/日记模板".to_owned(),
        };
        let json = preferences.to_json();
        assert_eq!(DailyNotesPreferences::parse(&json), preferences);
        assert!(json.contains("\"enabled\": false"));

        assert_eq!(
            DailyNotesPreferences::parse("{\"version\": 1}"),
            DailyNotesPreferences::default()
        );
        for corrupt in [
            "",
            "{ broken",
            "[]",
            "{\"version\": 99}",
            "{\"folder\": \"x\"}",
        ] {
            assert_eq!(
                DailyNotesPreferences::parse(corrupt),
                DailyNotesPreferences::default(),
                "损坏输入必须回退默认：{corrupt:?}"
            );
        }
    }

    #[test]
    fn init_from_path_restores_persisted_preferences_and_corruption_falls_back() {
        let guard = lock_for_test();
        let root = test_root().join("init");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("创建 target/tmp 失败");
        let path = root.join(DAILY_NOTES_FILE_NAME);

        let expected = DailyNotesPreferences {
            enabled: false,
            date_format: "YYYY/MM/DD".to_owned(),
            folder: "日志".to_owned(),
            template: "模板/日记模板".to_owned(),
        };
        save_preferences(&path, &expected).expect("写偏好失败");
        guard.init(path.clone());
        assert_eq!(preferences(), expected, "重启后必须恢复盘上的偏好");

        std::fs::write(&path, b"{ broken").expect("写坏文件失败");
        guard.init(path.clone());
        assert_eq!(preferences(), DailyNotesPreferences::default());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn preferences_file_round_trip_and_corrupt_file_falls_back() {
        let root = test_root();
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("创建 target/tmp 失败");
        let path = root.join(DAILY_NOTES_FILE_NAME);

        assert_eq!(load_preferences(&path), DailyNotesPreferences::default());

        let preferences = DailyNotesPreferences {
            enabled: true,
            date_format: "YYYY/MM/DD".to_owned(),
            folder: "日记".to_owned(),
            template: "模板/日记模板".to_owned(),
        };
        save_preferences(&path, &preferences).expect("写偏好失败");
        assert_eq!(load_preferences(&path), preferences);
        assert!(
            !path.with_extension("json.tmp").exists(),
            "原子写完成后不能留下临时文件"
        );

        std::fs::write(&path, b"{ broken").expect("写坏文件失败");
        assert_eq!(load_preferences(&path), DailyNotesPreferences::default());

        let _ = std::fs::remove_dir_all(&root);
    }
}
