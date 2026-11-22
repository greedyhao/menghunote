//! 快捷键中心模型 + 本机 `shortcuts.json` 偏好（设置页「快捷键」分类）。
//!
//! 这个模块是**唯一**的快捷键来源：编辑器（搜索/重命名/粗体/斜体）、
//! `AppShell`（新建笔记/图谱）与 `AppRoot`（打开设置）都查询这里的当前绑定，
//! 不再各自写死 `Ctrl+K` / `F2` / `Ctrl+B` / `Ctrl+I`。
//!
//! ## 命令表（默认值沿用接线前的既有行为）
//!
//! | `CommandId` | 稳定 key | 中文名 | 默认绑定 |
//! | --- | --- | --- | --- |
//! | [`CommandId::Search`] | `search` | 搜索 | `Ctrl+K` + `Ctrl+F`（两个都是默认别名） |
//! | [`CommandId::RenameNote`] | `rename_note` | 重命名笔记 | `F2` |
//! | [`CommandId::FormatBold`] | `format_bold` | 加粗 | `Ctrl+B` |
//! | [`CommandId::FormatItalic`] | `format_italic` | 斜体 | `Ctrl+I` |
//! | [`CommandId::NewNote`] | `new_note` | 新建笔记 | `Ctrl+N` |
//! | [`CommandId::ToggleGraph`] | `toggle_graph` | 打开图谱 | `Ctrl+G` |
//! | [`CommandId::OpenSettings`] | `open_settings` | 打开设置 | `Ctrl+,` |
//!
//! ## 文件格式（`<数据根>/shortcuts.json`）
//!
//! ```json
//! {
//!   "version": 1,
//!   "bindings": [
//!     { "command": "search", "keys": ["ctrl-k", "ctrl-f"] }
//!   ]
//! }
//! ```
//!
//! - 文件缺失 / 不可读 / 损坏 / 版本不认识 → **全部**回退默认值；
//! - 未知命令整条忽略（前向兼容，不阻塞启动）；
//! - 已知命令但 keys 为空 / 全部非法 → 该命令回退默认；
//! - 单条非法组合忽略；同一命令重复组合去重；
//! - 原子写：同目录临时文件 → rename，失败退化为直接覆盖；
//! - 只写本机偏好：不进 vault、不写正文、不参与同步。
//!
//! ## 录制规则（设置页用，避免"打字被吞"）
//!
//! 新绑定必须是 **Ctrl/Cmd 或 Alt + 一个键**，或 **F1–F24 功能键**；裸字母/
//! 数字/Enter/Tab/Backspace 一律拒绝。组合态（IME 拼写）、纯修饰键也会被拒绝。
//!
//! ## 冲突语义
//!
//! **同一个规范化组合被多个命令使用即冲突**。允许保存（冲突只是提醒），
//! 但设置页必须红标 + 顶部汇总。运行时按固定顺序匹配、先命中的处理：
//! 粗体 → 斜体 → 搜索 → 重命名 → 新建笔记 → 图谱 → 打开设置
//! （设置页的 `CommandId::ALL` 行顺序与这个优先级无关，仅用于展示）。
//!
//! ## 进程级状态
//!
//! 当前绑定保存在进程级 `RwLock`（与主题/语言同一模式），启动时
//! [`init_from_disk`] 从平台数据目录加载（测试/便携用 `MENGHUNOTE_SHORTCUTS_PATH`
//! 或 [`init_from_path`]）。改绑立即生效（下一次按键查询的就是新值），不重建编辑器。
//!
//! ## 已知限制
//!
//! - Windows/Linux 上 `Ctrl+,` 由平台后端映射成 `,`；headless 测试用同一套
//!   `Keystroke` 解析器验证，真机键盘布局/DPI 仍需人工确认；
//! - `Cmd`（macOS）按 `platform` 修饰键处理；本机 Linux 无法真机验证；
//! - 设置页只列固定命令表；插件命令注册后置。

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock, RwLock, RwLockReadGuard, RwLockWriteGuard};

use gpui::Keystroke;
use menghunote_i18n::{msg, t, t_with};

/// 偏好文件当前版本。
pub const SHORTCUTS_VERSION: u32 = 1;

/// 数据根下的偏好文件名。
pub const SHORTCUTS_FILE_NAME: &str = "shortcuts.json";

/// 测试/便携用的路径覆盖环境变量（优先于平台数据目录）。
pub const SHORTCUTS_PATH_ENV: &str = "MENGHUNOTE_SHORTCUTS_PATH";

// ---------------------------------------------------------------------------
// 命令表
// ---------------------------------------------------------------------------

/// 可配置的命令（稳定字符串 key 是持久化 / 测试契约，不随 UI 文案变化）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CommandId {
    /// 打开搜索浮层。
    Search,
    /// 重命名侧栏选中项。
    RenameNote,
    /// 行内粗体（`**`）。
    FormatBold,
    /// 行内斜体（`*`）。
    FormatItalic,
    /// 新建笔记（根级）。
    NewNote,
    /// 打开/关闭图谱。
    ToggleGraph,
    /// 打开设置页。
    OpenSettings,
}

impl CommandId {
    /// 全部命令（顺序 = 设置页行顺序 = 运行时匹配优先级）。
    pub const ALL: [CommandId; 7] = [
        CommandId::Search,
        CommandId::RenameNote,
        CommandId::FormatBold,
        CommandId::FormatItalic,
        CommandId::NewNote,
        CommandId::ToggleGraph,
        CommandId::OpenSettings,
    ];

    /// 持久化 / selector 用的稳定短名。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            CommandId::Search => "search",
            CommandId::RenameNote => "rename_note",
            CommandId::FormatBold => "format_bold",
            CommandId::FormatItalic => "format_italic",
            CommandId::NewNote => "new_note",
            CommandId::ToggleGraph => "toggle_graph",
            CommandId::OpenSettings => "open_settings",
        }
    }

    /// 稳定短名 → 命令；未知命令返回 `None`（加载时忽略）。
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|command| command.as_str() == value)
    }

    /// 默认绑定（每个字符串都是 [`KeyCombo::parse`] 接受的规范形式）。
    #[must_use]
    pub const fn default_keys(self) -> &'static [&'static str] {
        match self {
            CommandId::Search => &["ctrl-k", "ctrl-f"],
            CommandId::RenameNote => &["f2"],
            CommandId::FormatBold => &["ctrl-b"],
            CommandId::FormatItalic => &["ctrl-i"],
            CommandId::NewNote => &["ctrl-n"],
            CommandId::ToggleGraph => &["ctrl-g"],
            CommandId::OpenSettings => &["ctrl-,"],
        }
    }

    /// 中文（当前语言）显示名。
    #[must_use]
    pub fn label(self) -> String {
        let id = match self {
            CommandId::Search => msg::settings::SHORTCUT_SEARCH,
            CommandId::RenameNote => msg::settings::SHORTCUT_RENAME_NOTE,
            CommandId::FormatBold => msg::settings::SHORTCUT_FORMAT_BOLD,
            CommandId::FormatItalic => msg::settings::SHORTCUT_FORMAT_ITALIC,
            CommandId::NewNote => msg::settings::SHORTCUT_NEW_NOTE,
            CommandId::ToggleGraph => msg::settings::SHORTCUT_TOGGLE_GRAPH,
            CommandId::OpenSettings => msg::settings::SHORTCUT_OPEN_SETTINGS,
        };
        t(id)
    }

    /// 该命令的默认组合（解析后的形式；内置默认必须全部合法）。
    #[must_use]
    pub fn default_bindings(self) -> Vec<KeyCombo> {
        self.default_keys()
            .iter()
            .map(|text| KeyCombo::parse(text).expect("内置默认快捷键必须是合法组合"))
            .collect()
    }
}

// ---------------------------------------------------------------------------
// 按键组合
// ---------------------------------------------------------------------------

/// 规范化的按键组合（持久化字符串 = [`Self::as_str`]，展示 = [`Self::display`]）。
///
/// 修饰键是**精确匹配**：`Ctrl+K` 不会匹配 `Ctrl+Shift+K`，与接线前的行为一致。
/// 字母键大小写不敏感（Caps Lock 下平台可能给 `K`），但 `Shift` 仍以修饰键状态为准。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct KeyCombo {
    key: String,
    control: bool,
    alt: bool,
    shift: bool,
    platform: bool,
    function: bool,
}

impl KeyCombo {
    /// 解析规范字符串（`ctrl-k` / `f2` / `ctrl-shift-k` / `cmd-k` / `ctrl-,`）。
    ///
    /// 只做**语法**校验；是否能作为快捷键由 [`Self::is_bindable`] 判断。
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        if text.is_empty() {
            return None;
        }
        let parts: Vec<&str> = text.split('-').collect();
        let last = parts.len().checked_sub(1)?;
        let mut combo = Self {
            key: String::new(),
            control: false,
            alt: false,
            shift: false,
            platform: false,
            function: false,
        };
        for (index, part) in parts.iter().enumerate() {
            if index < last {
                match part.to_ascii_lowercase().as_str() {
                    "ctrl" | "control" => combo.control = true,
                    "alt" => combo.alt = true,
                    "shift" => combo.shift = true,
                    "cmd" | "super" | "win" => combo.platform = true,
                    "fn" => combo.function = true,
                    _ => return None,
                }
            } else {
                combo.key = canonical_key_name(part)?;
            }
        }
        if is_modifier_key(&combo.key) {
            return None;
        }
        Some(combo)
    }

    /// 从 GPUI 的真实按键事件构造（录制用；纯修饰键返回 `None`）。
    #[must_use]
    pub fn from_keystroke(keystroke: &Keystroke) -> Option<Self> {
        let key = canonical_key_name(&keystroke.key)?;
        if is_modifier_key(&key) {
            return None;
        }
        let modifiers = keystroke.modifiers;
        Some(Self {
            key,
            control: modifiers.control,
            alt: modifiers.alt,
            shift: modifiers.shift,
            platform: modifiers.platform,
            function: modifiers.function,
        })
    }

    /// 是否满足录制规则：Ctrl/Cmd 或 Alt，或 F1–F24；裸字母/数字/Enter/Tab/Backspace 不允许。
    #[must_use]
    pub fn is_bindable(&self) -> bool {
        self.control || self.alt || self.platform || is_function_key(&self.key)
    }

    /// 是否匹配一次真实按键（修饰键精确、字母大小写不敏感）。
    #[must_use]
    pub fn matches(&self, keystroke: &Keystroke) -> bool {
        let Some(key) = canonical_key_name(&keystroke.key) else {
            return false;
        };
        let modifiers = keystroke.modifiers;
        self.key == key
            && self.control == modifiers.control
            && self.alt == modifiers.alt
            && self.shift == modifiers.shift
            && self.platform == modifiers.platform
            && self.function == modifiers.function
    }

    /// 持久化 / 冲突分组的规范字符串（修饰键顺序固定：ctrl、alt、shift、cmd、fn）。
    #[must_use]
    pub fn as_str(&self) -> String {
        let mut text = String::new();
        if self.control {
            text.push_str("ctrl-");
        }
        if self.alt {
            text.push_str("alt-");
        }
        if self.shift {
            text.push_str("shift-");
        }
        if self.platform {
            text.push_str("cmd-");
        }
        if self.function {
            text.push_str("fn-");
        }
        text.push_str(&key_for_storage(&self.key));
        text
    }

    /// UI 展示形式：`Ctrl+K` / `Ctrl+Shift+K` / `F2` / `Ctrl+,`。
    #[must_use]
    pub fn display(&self) -> String {
        let mut text = String::new();
        if self.control {
            text.push_str("Ctrl+");
        }
        if self.alt {
            text.push_str("Alt+");
        }
        if self.shift {
            text.push_str("Shift+");
        }
        if self.platform {
            text.push_str("Cmd+");
        }
        if self.function {
            text.push_str("Fn+");
        }
        text.push_str(&display_key_name(&self.key));
        text
    }
}

/// 单词别名 → 规范键名；大小写统一成小写（字母键的大小写差异不影响匹配）。
fn canonical_key_name(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let lower = raw.to_ascii_lowercase();
    let mapped = match lower.as_str() {
        "comma" => ",",
        "period" | "dot" => ".",
        "slash" => "/",
        "semicolon" => ";",
        "quote" | "apostrophe" => "'",
        "minus" | "dash" | "hyphen" => "-",
        "plus" => "+",
        "equals" | "equal" => "=",
        "bracketleft" | "leftbracket" => "[",
        "bracketright" | "rightbracket" => "]",
        "backslash" => "\\",
        "backquote" | "grave" => "`",
        "spacebar" => "space",
        _ => return Some(lower),
    };
    Some(mapped.to_owned())
}

/// 展示时给符号/单词保留可读名字；字母与功能键大写。
fn display_key_name(key: &str) -> String {
    if key.chars().count() == 1 {
        let mut chars = key.chars();
        let character = chars.next().unwrap_or_default();
        return character.to_uppercase().collect();
    }
    match key {
        "enter" => "Enter".to_owned(),
        "tab" => "Tab".to_owned(),
        "escape" => "Esc".to_owned(),
        "space" => "Space".to_owned(),
        "backspace" => "Backspace".to_owned(),
        "delete" => "Delete".to_owned(),
        "insert" => "Insert".to_owned(),
        "left" => "Left".to_owned(),
        "right" => "Right".to_owned(),
        "up" => "Up".to_owned(),
        "down" => "Down".to_owned(),
        "home" => "Home".to_owned(),
        "end" => "End".to_owned(),
        "pageup" => "PageUp".to_owned(),
        "pagedown" => "PageDown".to_owned(),
        _ => key.to_ascii_uppercase(),
    }
}

/// 存储时避开分隔符 `-` / 特殊符号歧义（`-` 存成 `dash`，`+` 存成 `plus`）。
fn key_for_storage(key: &str) -> String {
    match key {
        "-" => "dash".to_owned(),
        "+" => "plus".to_owned(),
        _ => key.to_owned(),
    }
}

/// 纯修饰键（单独按下不应成为绑定）。
fn is_modifier_key(key: &str) -> bool {
    matches!(
        key,
        "control"
            | "ctrl"
            | "shift"
            | "alt"
            | "platform"
            | "function"
            | "cmd"
            | "super"
            | "win"
            | "fn"
    )
}

/// `F1`–`F24`（小写 `f1`…`f24`）。
fn is_function_key(key: &str) -> bool {
    let Some(number) = key.strip_prefix('f') else {
        return false;
    };
    if number.is_empty() || number.len() > 2 || !number.bytes().all(|byte| byte.is_ascii_digit()) {
        return false;
    }
    number
        .parse::<u8>()
        .is_ok_and(|number| (1..=24).contains(&number))
}

// ---------------------------------------------------------------------------
// 持久化模型
// ---------------------------------------------------------------------------

/// 本机快捷键偏好：命令 → 一个或多个组合。
///
/// `path` 是**写盘目标**（`None` = 只在内存中生效，测试/未初始化）。
/// 缺命令/非法条目在构造时就已按默认值补齐，因此运行中的 store 永远有全部 7 个命令。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShortcutStore {
    path: Option<PathBuf>,
    bindings: Vec<(CommandId, Vec<KeyCombo>)>,
}

impl Default for ShortcutStore {
    fn default() -> Self {
        Self {
            path: None,
            bindings: default_bindings(),
        }
    }
}

/// 一个冲突组：同一个规范化组合被多个命令使用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShortcutConflict {
    /// 冲突的规范化组合。
    pub key: KeyCombo,
    /// 使用该组合的全部命令（按命令表顺序）。
    pub commands: Vec<CommandId>,
}

/// 设置页一行的投影（渲染与 headless 断言共用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShortcutRow {
    /// 命令。
    pub command: CommandId,
    /// 当前绑定的全部组合（默认 search 有两个别名）。
    pub bindings: Vec<KeyCombo>,
    /// 该命令是否参与至少一个冲突组。
    pub conflicted: bool,
}

impl ShortcutRow {
    /// 展示文本：`Ctrl+K / Ctrl+F`。
    #[must_use]
    pub fn value_text(&self) -> String {
        display_bindings(&self.bindings)
    }
}

/// 设置页快照：全部行 + 冲突组（同一把读锁内计算，渲染不会看到半更新状态）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShortcutSnapshot {
    /// 每个命令一行（固定顺序）。
    pub rows: Vec<ShortcutRow>,
    /// 冲突组（空 = 无冲突）。
    pub conflicts: Vec<ShortcutConflict>,
}

impl ShortcutSnapshot {
    /// 冲突组数量。
    #[must_use]
    pub fn conflict_count(&self) -> usize {
        self.conflicts.len()
    }

    /// 顶部汇总文案（`无冲突` / `冲突 N 组`）。
    #[must_use]
    pub fn summary(&self) -> String {
        conflicts_summary(self.conflict_count())
    }
}

impl ShortcutStore {
    /// 默认绑定 + 指定写盘目标。
    #[must_use]
    pub fn new(path: Option<PathBuf>) -> Self {
        Self {
            path,
            bindings: default_bindings(),
        }
    }

    /// 从文件加载；缺失/不可读/损坏/版本不支持 → 默认值。
    ///
    /// 无论文件是否存在，都会记住 `path` 作为后续保存目标。
    #[must_use]
    pub fn load(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let bindings = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| parse_document(&text))
            .unwrap_or_else(default_bindings);
        Self {
            path: Some(path),
            bindings,
        }
    }

    /// 从 JSON 文本解析（不绑写盘目标；测试/导入用）。
    #[must_use]
    pub fn parse(text: &str) -> Self {
        Self {
            path: None,
            bindings: parse_document(text).unwrap_or_else(default_bindings),
        }
    }

    /// 写盘目标（`None` = 只在内存中生效）。
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// 当前绑定表（固定命令顺序）。
    #[must_use]
    pub fn bindings(&self) -> &[(CommandId, Vec<KeyCombo>)] {
        &self.bindings
    }

    /// 某个命令的绑定（永远返回默认补齐后的非空列表）。
    #[must_use]
    pub fn binding(&self, command: CommandId) -> &[KeyCombo] {
        self.bindings
            .iter()
            .find(|(candidate, _)| *candidate == command)
            .map_or(&[], |(_, bindings)| bindings.as_slice())
    }

    /// 整体重设某个命令的绑定；空表/全部非法组合是 no-op（不会把命令改成"无绑定"）。
    pub fn set_binding(&mut self, command: CommandId, keys: Vec<KeyCombo>) {
        let mut valid: Vec<KeyCombo> = Vec::new();
        for key in keys {
            if key.is_bindable() && !valid.contains(&key) {
                valid.push(key);
            }
        }
        if valid.is_empty() {
            return;
        }
        if let Some((_, slot)) = self
            .bindings
            .iter_mut()
            .find(|(candidate, _)| *candidate == command)
        {
            *slot = valid;
        }
    }

    /// 恢复某个命令的内置默认绑定。
    pub fn reset(&mut self, command: CommandId) {
        if let Some((_, slot)) = self
            .bindings
            .iter_mut()
            .find(|(candidate, _)| *candidate == command)
        {
            *slot = command.default_bindings();
        }
    }

    /// 冲突组：同一个规范化组合被多个命令使用即冲突。
    #[must_use]
    pub fn conflicts(&self) -> Vec<ShortcutConflict> {
        let mut groups: Vec<(KeyCombo, Vec<CommandId>)> = Vec::new();
        for (command, bindings) in &self.bindings {
            for key in bindings {
                match groups.iter_mut().find(|(candidate, _)| candidate == key) {
                    Some((_, commands)) => {
                        if !commands.contains(command) {
                            commands.push(*command);
                        }
                    }
                    None => groups.push((key.clone(), vec![*command])),
                }
            }
        }
        groups
            .into_iter()
            .filter(|(_, commands)| commands.len() > 1)
            .map(|(key, commands)| ShortcutConflict { key, commands })
            .collect()
    }

    /// 某个命令是否参与冲突。
    #[must_use]
    pub fn is_conflicted(&self, command: CommandId) -> bool {
        self.conflicts()
            .iter()
            .any(|conflict| conflict.commands.contains(&command))
    }

    /// 稳定 JSON（写入前先落临时文件再 rename）。
    #[must_use]
    pub fn to_json(&self) -> String {
        let mut text = format!("{{\n  \"version\": {SHORTCUTS_VERSION},\n  \"bindings\": [\n");
        for (index, (command, bindings)) in self.bindings.iter().enumerate() {
            let keys = bindings
                .iter()
                .map(|key| format!("\"{}\"", json_escape(&key.as_str())))
                .collect::<Vec<_>>()
                .join(", ");
            text.push_str(&format!(
                "    {{ \"command\": \"{}\", \"keys\": [{keys}] }}{}\n",
                command.as_str(),
                if index + 1 == self.bindings.len() {
                    ""
                } else {
                    ","
                }
            ));
        }
        text.push_str("  ]\n}\n");
        text
    }

    /// 原子写入：同目录临时文件 → rename；失败时旧文件原样保留。
    ///
    /// `path == None` 时直接返回 `Ok(())`（只在内存中生效）。
    pub fn save(&self) -> io::Result<()> {
        let Some(path) = self.path.as_deref() else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = self.to_json();
        let temp = path.with_extension("json.tmp");
        std::fs::write(&temp, text.as_bytes())?;
        match std::fs::rename(&temp, path) {
            Ok(()) => Ok(()),
            Err(_) => {
                // Windows 上目标存在时 rename 可能失败：退化为直接覆盖写。
                let result = std::fs::write(path, text.as_bytes());
                let _ = std::fs::remove_file(&temp);
                result
            }
        }
    }
}

/// 默认绑定表（固定命令顺序；每个命令至少一个组合）。
#[must_use]
pub fn default_bindings() -> Vec<(CommandId, Vec<KeyCombo>)> {
    CommandId::ALL
        .into_iter()
        .map(|command| (command, command.default_bindings()))
        .collect()
}

/// 绑定列表的展示文本（空列表显示「—」，正常路径不会为空）。
#[must_use]
pub fn display_bindings(bindings: &[KeyCombo]) -> String {
    if bindings.is_empty() {
        "—".to_owned()
    } else {
        bindings
            .iter()
            .map(KeyCombo::display)
            .collect::<Vec<_>>()
            .join(" / ")
    }
}

// ---------------------------------------------------------------------------
// 平台路径 / 进程级状态
// ---------------------------------------------------------------------------

/// 平台数据目录下的快捷键偏好路径；解析不到时返回 `None`。
///
/// 1. `MENGHUNOTE_SHORTCUTS_PATH`（测试/便携显式覆盖）；
/// 2. Windows：`%LOCALAPPDATA%\menghunote\shortcuts.json`；
/// 3. 其它：`$XDG_DATA_HOME/menghunote/shortcuts.json`（缺省 `~/.local/share`）。
///
/// 与 [`crate::onboarding::ProfileLayout::data_root`] 的平台默认值一致：
/// 快捷键是**本机应用偏好**，不在 vault 里、不参与同步。
#[must_use]
pub fn settings_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(SHORTCUTS_PATH_ENV) {
        return Some(PathBuf::from(path));
    }
    #[cfg(target_os = "windows")]
    {
        std::env::var_os("LOCALAPPDATA").map(|base| {
            PathBuf::from(base)
                .join("menghunote")
                .join(SHORTCUTS_FILE_NAME)
        })
    }
    #[cfg(not(target_os = "windows"))]
    {
        let base = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share"))
            })?;
        Some(base.join("menghunote").join(SHORTCUTS_FILE_NAME))
    }
}

static STATE: OnceLock<RwLock<ShortcutStore>> = OnceLock::new();

fn state() -> &'static RwLock<ShortcutStore> {
    STATE.get_or_init(|| RwLock::new(ShortcutStore::default()))
}

fn read_state() -> RwLockReadGuard<'static, ShortcutStore> {
    state().read().unwrap_or_else(|error| error.into_inner())
}

fn write_state() -> RwLockWriteGuard<'static, ShortcutStore> {
    state().write().unwrap_or_else(|error| error.into_inner())
}

/// 启动时从平台数据目录加载（`main.rs` 在打开窗口前调一次）。
///
/// 文件缺失/损坏回退默认值，不阻塞启动；解析不到平台目录时保持默认值。
pub fn init_from_disk() {
    if let Some(path) = settings_path() {
        init_from_path(path);
    }
}

/// 从**显式路径**加载（测试/便携/`AppRoot` 按 `ProfileLayout` 数据根注入）。
///
/// 文件缺失/损坏回退默认值，并记住该路径作为后续修改的写盘目标。
pub fn init_from_path(path: PathBuf) {
    *write_state() = ShortcutStore::load(path);
}

/// 当前绑定表的克隆（设置页快照 / 诊断用）。
#[must_use]
pub fn current_store() -> ShortcutStore {
    read_state().clone()
}

/// 当前写盘目标（测试/诊断用）。
#[must_use]
pub fn store_path() -> Option<PathBuf> {
    read_state().path().map(Path::to_path_buf)
}

/// 当前设置页快照（行 + 冲突，同一把读锁内计算）。
#[must_use]
pub fn snapshot() -> ShortcutSnapshot {
    let store = read_state();
    let conflicts = store.conflicts();
    let rows = store
        .bindings()
        .iter()
        .map(|(command, bindings)| ShortcutRow {
            command: *command,
            bindings: bindings.clone(),
            conflicted: conflicts
                .iter()
                .any(|conflict| conflict.commands.contains(command)),
        })
        .collect();
    ShortcutSnapshot { rows, conflicts }
}

/// 当前绑定里是否有任何组合匹配这次按键。
#[must_use]
pub fn matches(command: CommandId, keystroke: &Keystroke) -> bool {
    read_state()
        .binding(command)
        .iter()
        .any(|key| key.matches(keystroke))
}

/// 某个命令的当前绑定（克隆；`EditorView`/`AppShell` 查询用）。
#[must_use]
pub fn binding(command: CommandId) -> Vec<KeyCombo> {
    read_state().binding(command).to_vec()
}

/// 当前冲突组（设置页/运行时诊断用）。
#[must_use]
pub fn conflicts() -> Vec<ShortcutConflict> {
    read_state().conflicts()
}

/// 改一个命令的绑定并立即写盘（下一次按键即生效）。
///
/// 组合不满足录制规则 → `InvalidInput`；写盘失败 → 返回 `Err`，但**内存里已经生效**
/// （调用方只记日志，不阻止用户继续使用本次会话）。
pub fn assign(command: CommandId, key: KeyCombo) -> io::Result<()> {
    if !key.is_bindable() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "快捷键必须带 Ctrl/Cmd 或 Alt，或使用 F1–F24",
        ));
    }
    let mut store = write_state();
    store.set_binding(command, vec![key]);
    store.save()
}

/// 恢复一个命令的默认绑定并写盘。
pub fn reset_binding(command: CommandId) -> io::Result<()> {
    let mut store = write_state();
    store.reset(command);
    store.save()
}

// ---------------------------------------------------------------------------
// 设置页文案（i18n 唯一入口；渲染与测试共用）
// ---------------------------------------------------------------------------

/// 快捷键区标题。
#[must_use]
pub fn title() -> String {
    t(msg::settings::SHORTCUTS_TITLE)
}

/// 快捷键区说明。
#[must_use]
pub fn hint() -> String {
    t(msg::settings::SHORTCUTS_HINT)
}

/// 顶部冲突汇总（`无冲突` / `冲突 N 组`）。
#[must_use]
pub fn conflicts_summary(count: usize) -> String {
    if count == 0 {
        t(msg::settings::SHORTCUTS_NO_CONFLICTS)
    } else {
        let count_text = count.to_string();
        t_with(
            msg::settings::SHORTCUTS_CONFLICTS,
            &[("count", count_text.as_str())],
        )
    }
}

/// 「修改」按钮。
#[must_use]
pub fn modify_label() -> String {
    t(msg::settings::SHORTCUTS_MODIFY)
}

/// 行内冲突红标。
#[must_use]
pub fn conflict_badge() -> String {
    t(msg::settings::SHORTCUTS_CONFLICT_BADGE)
}

/// 「恢复默认」按钮。
#[must_use]
pub fn reset_label() -> String {
    t(msg::settings::SHORTCUTS_RESET)
}

/// 录制态提示。
#[must_use]
pub fn capture_prompt() -> String {
    t(msg::settings::SHORTCUTS_CAPTURE)
}

/// 非法组合提示。
#[must_use]
pub fn capture_invalid() -> String {
    t(msg::settings::SHORTCUTS_CAPTURE_INVALID)
}

/// 已生效但写盘失败提示。
#[must_use]
pub fn capture_failed(reason: &str) -> String {
    t_with(
        msg::settings::SHORTCUTS_CAPTURE_FAILED,
        &[("reason", reason)],
    )
}

// ---------------------------------------------------------------------------
// JSON 解析（手写，避免给 `--no-default-features` 拉 serde）
// ---------------------------------------------------------------------------

/// 极简 JSON 游标：只服务本模块写出的固定形状，**严格**拒绝其它东西。
///
/// 之所以不引 `serde_json`：`serde`/`serde_json` 是 `sync-ui` 的可选依赖，
/// 而快捷键在 `--no-default-features` 构建里也要生效。解析失败一律回退默认值。
struct Json<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Json<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            bytes: text.as_bytes(),
            pos: 0,
        }
    }

    fn skip_ws(&mut self) {
        while let Some(byte) = self.bytes.get(self.pos) {
            if byte.is_ascii_whitespace() {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn consume(&mut self, expected: u8) -> bool {
        if self.peek() == Some(expected) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, expected: u8) -> Option<()> {
        self.consume(expected).then_some(())
    }

    fn parse_string(&mut self) -> Option<String> {
        self.expect(b'"')?;
        let mut value = String::new();
        loop {
            let byte = *self.bytes.get(self.pos)?;
            self.pos += 1;
            match byte {
                b'"' => return Some(value),
                b'\\' => {
                    let escaped = *self.bytes.get(self.pos)?;
                    self.pos += 1;
                    match escaped {
                        b'"' => value.push('"'),
                        b'\\' => value.push('\\'),
                        b'/' => value.push('/'),
                        b'n' => value.push('\n'),
                        b't' => value.push('\t'),
                        b'r' => value.push('\r'),
                        _ => return None,
                    }
                }
                // 命令表与按键名都是 ASCII；其它字节视为损坏（回退默认值）。
                0..=0x7f => value.push(byte as char),
                _ => return None,
            }
        }
    }

    fn parse_u32(&mut self) -> Option<u32> {
        let start = self.pos;
        while self.bytes.get(self.pos).is_some_and(u8::is_ascii_digit) {
            self.pos += 1;
        }
        if start == self.pos {
            return None;
        }
        std::str::from_utf8(&self.bytes[start..self.pos])
            .ok()?
            .parse()
            .ok()
    }

    /// `"keys": ["ctrl-k", "ctrl-f"]`
    fn parse_keys(&mut self) -> Option<Vec<String>> {
        self.expect(b'[')?;
        let mut keys = Vec::new();
        loop {
            self.skip_ws();
            if self.consume(b']') {
                return Some(keys);
            }
            keys.push(self.parse_string()?);
            self.skip_ws();
            if self.consume(b',') {
                continue;
            }
            self.skip_ws();
            self.expect(b']')?;
            return Some(keys);
        }
    }

    /// `{ "command": "search", "keys": [...] }`（字段顺序任意；未知字段 → 损坏）。
    fn parse_binding(&mut self) -> Option<(String, Vec<String>)> {
        self.expect(b'{')?;
        let mut command = None;
        let mut keys = None;
        loop {
            self.skip_ws();
            if self.consume(b'}') {
                break;
            }
            let field = self.parse_string()?;
            self.skip_ws();
            self.expect(b':')?;
            self.skip_ws();
            match field.as_str() {
                "command" => command = Some(self.parse_string()?),
                "keys" => keys = Some(self.parse_keys()?),
                _ => return None,
            }
            self.skip_ws();
            if self.consume(b',') {
                continue;
            }
            self.skip_ws();
            self.expect(b'}')?;
            break;
        }
        Some((command?, keys?))
    }

    fn parse_bindings(&mut self) -> Option<Vec<(String, Vec<String>)>> {
        self.expect(b'[')?;
        let mut bindings = Vec::new();
        loop {
            self.skip_ws();
            if self.consume(b']') {
                return Some(bindings);
            }
            bindings.push(self.parse_binding()?);
            self.skip_ws();
            if self.consume(b',') {
                continue;
            }
            self.skip_ws();
            self.expect(b']')?;
            return Some(bindings);
        }
    }
}

/// 顶层文档：`{ "version": 1, "bindings": [...] }`。
///
/// 任何语法/版本问题返回 `None`（调用方回退默认值）。
fn parse_document(text: &str) -> Option<Vec<(CommandId, Vec<KeyCombo>)>> {
    let mut json = Json::new(text);
    json.skip_ws();
    json.expect(b'{')?;
    let mut version = None;
    let mut entries: Option<Vec<(String, Vec<String>)>> = None;
    loop {
        json.skip_ws();
        if json.consume(b'}') {
            break;
        }
        let field = json.parse_string()?;
        json.skip_ws();
        json.expect(b':')?;
        json.skip_ws();
        match field.as_str() {
            "version" => version = Some(json.parse_u32()?),
            "bindings" => entries = Some(json.parse_bindings()?),
            // 未知字段不静默忽略：未来版本可能改变语义，整份文件回退默认更安全。
            _ => return None,
        }
        json.skip_ws();
        if json.consume(b',') {
            continue;
        }
        json.skip_ws();
        json.expect(b'}')?;
        break;
    }
    json.skip_ws();
    if json.pos != json.bytes.len() {
        return None;
    }
    if version? != SHORTCUTS_VERSION {
        return None;
    }
    let entries = entries?;
    Some(resolve_entries(entries))
}

/// 把文件条目合并到默认表：未知命令忽略；已知命令的非法/空组合保留默认值。
fn resolve_entries(entries: Vec<(String, Vec<String>)>) -> Vec<(CommandId, Vec<KeyCombo>)> {
    let mut resolved: Vec<(CommandId, Vec<KeyCombo>)> = default_bindings();
    // 重复命令条目以最后一次为准（与"后写覆盖"直觉一致）。
    let mut seen: HashMap<&'static str, usize> = HashMap::new();
    for (command, keys) in entries {
        let Some(command) = CommandId::parse(&command) else {
            continue;
        };
        let mut valid: Vec<KeyCombo> = Vec::new();
        for key in keys {
            let Some(key) = KeyCombo::parse(&key) else {
                continue;
            };
            if key.is_bindable() && !valid.contains(&key) {
                valid.push(key);
            }
        }
        let key = command.as_str();
        let index = *seen.entry(key).or_insert_with(|| {
            resolved
                .iter()
                .position(|(candidate, _)| *candidate == command)
                .expect("命令表必须覆盖全部 CommandId")
        });
        if !valid.is_empty() {
            resolved[index].1 = valid;
        }
    }
    resolved
}

/// JSON 字符串转义（只处理写盘会遇到的 `"` / `\` / 控制字符）。
fn json_escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            _ => escaped.push(character),
        }
    }
    escaped
}

// ---------------------------------------------------------------------------
// serde：`CommandId` 的稳定字符串 key（仅 sync-ui 构建有 serde 依赖）
// ---------------------------------------------------------------------------

#[cfg(feature = "sync-ui")]
impl serde::Serialize for CommandId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

#[cfg(feature = "sync-ui")]
impl<'de> serde::Deserialize<'de> for CommandId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct CommandVisitor;

        impl serde::de::Visitor<'_> for CommandVisitor {
            type Value = CommandId;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a stable shortcut command id")
            }

            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
                CommandId::parse(value).ok_or_else(|| {
                    E::unknown_variant(
                        value,
                        &[
                            "search",
                            "rename_note",
                            "format_bold",
                            "format_italic",
                            "new_note",
                            "toggle_graph",
                            "open_settings",
                        ],
                    )
                })
            }
        }

        deserializer.deserialize_str(CommandVisitor)
    }
}

// ---------------------------------------------------------------------------
// 测试钩子
// ---------------------------------------------------------------------------

/// 进程级快捷键测试锁：全局绑定是共享状态，改它的测试必须串行。
static TEST_LOCK: Mutex<()> = Mutex::new(());

/// 快捷键测试守卫：持有期间串行；`reset` 把全局状态指向确定状态，
/// `Drop` 自动复位到"默认绑定 + 不持久化"（避免跨测试污染）。
pub struct ShortcutTestGuard {
    _lock: MutexGuard<'static, ()>,
}

impl ShortcutTestGuard {
    /// 复位全局绑定（`path = Some` 时同时指定写盘目标）。
    pub fn reset(&self, path: Option<PathBuf>) {
        *write_state() = ShortcutStore::new(path);
    }
}

impl Drop for ShortcutTestGuard {
    fn drop(&mut self) {
        *write_state() = ShortcutStore::default();
    }
}

/// 串行化快捷键测试并自动复位（只给 `crates/app/tests/**` 与 crate 内测试用）。
#[doc(hidden)]
#[must_use]
pub fn lock_for_test() -> ShortcutTestGuard {
    ShortcutTestGuard {
        _lock: TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn key(text: &str) -> KeyCombo {
        KeyCombo::parse(text).unwrap_or_else(|| panic!("按键组合必须可解析：{text}"))
    }

    fn keystroke(text: &'static str) -> Keystroke {
        Keystroke::parse(text).expect("按键解析失败")
    }

    fn temp_root(name: &str) -> PathBuf {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/tmp")
            .join(format!("shortcuts-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("创建 target/tmp 测试目录失败");
        root
    }

    #[test]
    fn default_bindings_preserve_existing_behavior() {
        let store = ShortcutStore::default();
        assert_eq!(
            display_bindings(store.binding(CommandId::Search)),
            "Ctrl+K / Ctrl+F"
        );
        assert_eq!(display_bindings(store.binding(CommandId::RenameNote)), "F2");
        assert_eq!(
            display_bindings(store.binding(CommandId::FormatBold)),
            "Ctrl+B"
        );
        assert_eq!(
            display_bindings(store.binding(CommandId::FormatItalic)),
            "Ctrl+I"
        );
        assert_eq!(
            display_bindings(store.binding(CommandId::NewNote)),
            "Ctrl+N"
        );
        assert_eq!(
            display_bindings(store.binding(CommandId::ToggleGraph)),
            "Ctrl+G"
        );
        assert_eq!(
            display_bindings(store.binding(CommandId::OpenSettings)),
            "Ctrl+,"
        );
        assert_eq!(store.bindings().len(), CommandId::ALL.len());
        assert!(store.conflicts().is_empty(), "默认值不得互相冲突");
        // 默认 search 的两个别名都命中；Ctrl+Shift+K 不属于任何默认命令。
        assert!(store.binding(CommandId::Search)[0].matches(&keystroke("ctrl-k")));
        assert!(store.binding(CommandId::Search)[1].matches(&keystroke("ctrl-f")));
        assert!(!store.binding(CommandId::Search)[0].matches(&keystroke("ctrl-shift-k")));
    }

    #[test]
    fn capture_rules_reject_typing_but_allow_modified_and_function_keys() {
        for valid in [
            "ctrl-k",
            "ctrl-shift-k",
            "alt-x",
            "cmd-k",
            "ctrl-,",
            "ctrl-enter",
            "ctrl-tab",
            "f1",
            "f2",
            "f24",
            "shift-f2",
            "ctrl-alt-delete",
        ] {
            assert!(key(valid).is_bindable(), "{valid} 必须可绑定");
        }
        for invalid in [
            "a",
            "1",
            "enter",
            "tab",
            "backspace",
            "escape",
            "shift-a",
            "/",
            "f0",
            "f25",
        ] {
            assert!(!key(invalid).is_bindable(), "{invalid} 不得成为快捷键");
        }
        // 纯修饰键不是合法组合。
        assert_eq!(KeyCombo::parse("ctrl"), None);
        assert_eq!(KeyCombo::parse("ctrl-shift"), None);
        assert_eq!(KeyCombo::from_keystroke(&keystroke("control")), None);
    }

    #[test]
    fn key_combo_round_trips_display_and_matching() {
        let combo = key("ctrl-shift-k");
        assert_eq!(combo.as_str(), "ctrl-shift-k");
        assert_eq!(combo.display(), "Ctrl+Shift+K");
        assert_eq!(KeyCombo::parse(&combo.as_str()), Some(combo.clone()));
        assert!(combo.matches(&keystroke("ctrl-shift-k")));
        assert!(!combo.matches(&keystroke("ctrl-k")));
        assert!(!combo.matches(&keystroke("ctrl-alt-shift-k")));
        // Caps Lock 下平台可能给大写字母：仍按同一个键匹配（Shift 状态不变）。
        let caps_lock = Keystroke {
            key: "B".to_owned(),
            key_char: None,
            modifiers: keystroke("ctrl-b").modifiers,
        };
        assert!(key("ctrl-b").matches(&caps_lock));
        // Ctrl+, 与 F2 的展示/往返。
        assert_eq!(key("ctrl-,").display(), "Ctrl+,");
        assert_eq!(key("f2").display(), "F2");
        // 大小写/别名输入归一到同一规范形式。
        assert_eq!(key("Ctrl-Comma").as_str(), "ctrl-,");
    }

    #[test]
    fn json_round_trip_save_and_reload() {
        let root = temp_root("json-roundtrip");
        let path = root.join(SHORTCUTS_FILE_NAME);
        let mut store = ShortcutStore::new(Some(path.clone()));
        store.set_binding(CommandId::FormatBold, vec![key("ctrl-shift-b")]);
        store.set_binding(CommandId::Search, vec![key("ctrl-shift-f")]);
        store.save().expect("保存快捷键失败");
        assert!(
            !path.with_extension("json.tmp").exists(),
            "原子写不得残留临时文件"
        );

        let raw = std::fs::read_to_string(&path).expect("读回快捷键失败");
        assert!(raw.contains("\"version\": 1"));
        assert!(raw.contains("\"command\": \"format_bold\", \"keys\": [\"ctrl-shift-b\"]"));
        assert!(raw.contains("\"command\": \"search\", \"keys\": [\"ctrl-shift-f\"]"));

        let reloaded = ShortcutStore::load(&path);
        assert_eq!(
            reloaded.binding(CommandId::FormatBold),
            &[key("ctrl-shift-b")]
        );
        assert_eq!(reloaded.binding(CommandId::Search), &[key("ctrl-shift-f")]);
        assert_eq!(reloaded.binding(CommandId::RenameNote), &[key("f2")]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_corrupt_and_unknown_commands_fall_back_per_command() {
        // 缺失文件：全部默认。
        let root = temp_root("missing");
        let missing = ShortcutStore::load(root.join(SHORTCUTS_FILE_NAME));
        assert_eq!(missing.bindings(), ShortcutStore::default().bindings());
        let _ = std::fs::remove_dir_all(&root);

        // 损坏 / 版本不支持 / 未知字段：全部默认。
        for corrupt in [
            "",
            "{ broken",
            "[]",
            "{\"version\": 1}",
            "{\"version\": 1, \"bindings\": []}",
            "{\"version\": 99, \"bindings\": []}",
            "{\"version\": 1, \"bindings\": [], \"x\": 1}",
            "{\"version\": 1, \"bindings\": [{ \"command\": \"search\" }]}",
        ] {
            let store = ShortcutStore::parse(corrupt);
            assert_eq!(
                store.bindings(),
                ShortcutStore::default().bindings(),
                "损坏 JSON 必须回退默认：{corrupt}"
            );
        }

        // 未知命令忽略；已知命令的非法组合保留默认；部分合法则只保留合法项。
        let text = r#"
        {
          "version": 1,
          "bindings": [
            { "command": "not_a_command", "keys": ["ctrl-k"] },
            { "command": "format_bold", "keys": ["b"] },
            { "command": "format_italic", "keys": ["ctrl-i", "nonsense", "i"] },
            { "command": "new_note", "keys": ["ctrl-shift-n"] }
          ]
        }
        "#;
        let store = ShortcutStore::parse(text);
        assert_eq!(store.binding(CommandId::FormatBold), &[key("ctrl-b")]);
        assert_eq!(store.binding(CommandId::FormatItalic), &[key("ctrl-i")]);
        assert_eq!(store.binding(CommandId::NewNote), &[key("ctrl-shift-n")]);
        assert_eq!(
            store.binding(CommandId::Search),
            &[key("ctrl-k"), key("ctrl-f")]
        );
        assert_eq!(store.binding(CommandId::OpenSettings), &[key("ctrl-,")]);
    }

    #[test]
    fn conflicts_group_same_key_and_reset_restores_defaults() {
        let mut store = ShortcutStore::default();
        store.set_binding(CommandId::FormatBold, vec![key("ctrl-k")]);
        let conflicts = store.conflicts();
        assert_eq!(conflicts.len(), 1, "同一 Ctrl+K 只应产生一组冲突");
        assert_eq!(conflicts[0].key, key("ctrl-k"));
        assert_eq!(
            conflicts[0].commands,
            vec![CommandId::Search, CommandId::FormatBold]
        );
        assert!(store.is_conflicted(CommandId::Search));
        assert!(store.is_conflicted(CommandId::FormatBold));
        assert!(!store.is_conflicted(CommandId::FormatItalic));

        // 同一个命令内部重复同一个 key 不算冲突（去重）。
        store.set_binding(CommandId::FormatItalic, vec![key("ctrl-i"), key("ctrl-i")]);
        assert_eq!(store.binding(CommandId::FormatItalic), &[key("ctrl-i")]);

        // 恢复默认只影响该命令。
        store.reset(CommandId::FormatBold);
        assert_eq!(store.binding(CommandId::FormatBold), &[key("ctrl-b")]);
        assert!(store.conflicts().is_empty());
        assert_eq!(
            store.binding(CommandId::Search),
            &[key("ctrl-k"), key("ctrl-f")]
        );
    }

    #[test]
    fn snapshot_summary_and_row_text() {
        let mut store = ShortcutStore::default();
        store.set_binding(CommandId::NewNote, vec![key("ctrl-g")]);
        let conflicts = store.conflicts();
        let snapshot = ShortcutSnapshot {
            rows: CommandId::ALL
                .into_iter()
                .map(|command| ShortcutRow {
                    command,
                    bindings: store.binding(command).to_vec(),
                    conflicted: conflicts.iter().any(|c| c.commands.contains(&command)),
                })
                .collect(),
            conflicts,
        };
        assert_eq!(snapshot.conflict_count(), 1);
        assert!(snapshot.summary().contains('1'), "汇总必须包含冲突组数");
        let new_note = snapshot
            .rows
            .iter()
            .find(|row| row.command == CommandId::NewNote)
            .expect("必须有新建笔记行");
        assert!(new_note.conflicted);
        assert_eq!(new_note.value_text(), "Ctrl+G");
        assert!(CommandId::Search.label().contains("搜索"));
    }

    #[test]
    fn global_init_assign_and_reset_are_persisted_and_isolated() {
        let guard = lock_for_test();
        let root = temp_root("global");
        let path = root.join(SHORTCUTS_FILE_NAME);
        guard.reset(None);

        // 未初始化路径：只在内存生效，不写盘、不报错。
        assign(CommandId::Search, key("ctrl-shift-k")).expect("内存改绑必须成功");
        assert!(matches(CommandId::Search, &keystroke("ctrl-shift-k")));
        assert!(!matches(CommandId::Search, &keystroke("ctrl-k")));
        assert_eq!(store_path(), None);

        // 指定路径后：立即写盘；重新初始化（模拟重启）仍生效。
        guard.reset(Some(path.clone()));
        assign(CommandId::Search, key("ctrl-shift-k")).expect("改绑必须成功");
        assert!(matches(CommandId::Search, &keystroke("ctrl-shift-k")));
        let raw = std::fs::read_to_string(&path).expect("改绑必须落盘");
        assert!(raw.contains("ctrl-shift-k"));

        init_from_path(path.clone());
        assert!(matches(CommandId::Search, &keystroke("ctrl-shift-k")));
        assert!(!matches(CommandId::Search, &keystroke("ctrl-k")));

        reset_binding(CommandId::Search).expect("恢复默认必须成功");
        assert!(matches(CommandId::Search, &keystroke("ctrl-k")));
        assert!(matches(CommandId::Search, &keystroke("ctrl-f")));
        let reloaded = ShortcutStore::load(&path);
        assert_eq!(
            reloaded.binding(CommandId::Search),
            &[key("ctrl-k"), key("ctrl-f")]
        );

        // 非法组合被拒绝，且不改变当前绑定。
        let before = binding(CommandId::Search);
        assert!(assign(CommandId::Search, key("a")).is_err());
        assert_eq!(binding(CommandId::Search), before);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(feature = "sync-ui")]
    #[test]
    fn command_id_serializes_as_stable_string_with_serde() {
        assert_eq!(
            serde_json::to_string(&CommandId::Search).expect("序列化命令失败"),
            "\"search\""
        );
        assert_eq!(
            serde_json::from_str::<CommandId>("\"format_bold\"").expect("反序列化失败"),
            CommandId::FormatBold
        );
        assert!(serde_json::from_str::<CommandId>("\"not_a_command\"").is_err());
    }
}
