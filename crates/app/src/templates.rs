//! 第一方「模板」核心插件（Obsidian 风格）。
//!
//! ## 产品决定（已确认，别再改回去）
//!
//! - **模板就是普通笔记**：不建独立表、不隐藏文件夹、不做保留名、不做搜索/图谱过滤。
//!   模板文件夹只是**可配置的普通目录**（默认 `模板`），其子树下的任意笔记都是模板。
//! - 模板正文按**源文**读取（`BodyDoc::text()`），占位符替换后插入/新建；模板笔记本身
//!   从不移动、不复制、不删除资产字节。
//! - 设置只写本机 `templates.json`（[`settings_path`]），**不同步、不写正文**。
//!
//! ## 占位符（纯函数，可单测）
//!
//! | 写法 | 含义 |
//! | --- | --- |
//! | `{{date}}` | 用设置里的 `date_format` 格式化本地日期 |
//! | `{{time}}` | 用设置里的 `time_format` 格式化本地时间 |
//! | `{{title}}` | 目标笔记标题（插入到当前笔记 = 当前笔记标题；从模板新建 = 新标题） |
//! | `{{date:FORMAT}}` / `{{time:FORMAT}}` | 单次覆盖格式（空格式 = 回退默认） |
//! | `{{date:OFFSET}}` | **相对日期**：偏移后的日期按默认格式（`-1d` = 昨天） |
//! | `{{date:OFFSET:FORMAT}}` | 相对日期 + 单次覆盖格式（`-1d:YYYY/MM/DD`） |
//!
//! 支持的 token：`YYYY` `YY` `MM` `M` `DD` `D` `HH` `H` `mm` `m` `ss` `s`
//! （最长匹配；其它字符原样保留）。未知占位符（`{{foo}}`）**原样保留**，不 panic。
//!
//! ## 相对日期语法（2026-11-19 P1）
//!
//! `{{date:OFFSET}}` / `{{date:OFFSET:FORMAT}}` 里的 `OFFSET`：
//!
//! - 可选符号 `+` / `-`（缺省 = 正数），**1–2 位** ASCII 数字，单位 `d`（天）/
//!   `w`（周）/ `m`（月）/ `y`（年），小写、无空白：`-1d` `+2w` `-1m` `+1y`；
//! - 例：`{{date:-1d}}` = 昨天、`{{date:+1d}}` = 明天、
//!   `{{date:-1m:YYYY/MM/DD}}` = 上个月同一天（带格式）；
//! - **先判偏移、再判格式**：`date:` 后的第一个冒号前是偏移，其后整体是格式
//!   （格式里可以再有冒号，如 `-1d:YYYY:MM:DD` 或 `-1d:HH:mm`）；
//! - **原样保留**（整段 `{{...}}` 不动、不 panic）：空偏移（`{{date::YYYY}}`）、
//!   非法偏移（`{{date:+d}}` / `{{date:1x}}` / `{{date:1x:YYYY}}`）、偏移越界、
//!   以及没有冒号但以 `+`/`-`/数字开头的值（这些一律按偏移尝试处理，不做字面格式）；
//! - 兼容旧语义：`{{date:}}` = 默认格式；`{{date:YYYY-MM-DD}}` = 格式串；
//!   格式串里的冒号仍是字面量（`{{date:YYYY:MM}}` → `2026:11`）；
//! - 日期算术走本地墙钟日期（`chrono::NaiveDate`）：跨月/跨年/闰日正确；
//!   月偏移**月末对齐**（`2024-01-31 +1m` → `2024-02-29`）、年偏移把 `2/29` 落到
//!   `2/28`；时间分量（时/分/秒）不变；不做时区/夏令时换算；
//! - `{{time:...}}` **不支持**偏移（时间偏移没有产品语义），仍只按格式渲染。
//!
//! 不做脚本、正则——这些明确后置。
//!
//! ## 选择器 / 设置面板 selector（headless 测试点击/断言用）
//!
//! | selector | 含义 |
//! | --- | --- |
//! | `settings-templates` | 设置页「模板」区根 |
//! | `settings-plugin-templates` / `-toggle` / `-state` | 「插件」分类里的唯一启停开关（本区不再重复开关） |
//! | `settings-templates-folder` / `-date-format` / `-time-format` | 三个输入框 |
//! | `settings-templates-example` / `-notice` | 日期时间示例 / 保存提示 |
//! | `template-picker` | 选择器覆盖层根 |
//! | `template-picker-search` / `-list` / `-empty` | 搜索框 / 列表 / 空态 |
//! | `template-picker-item-<i>` | 列表第 i 行（当前过滤结果里的行号） |
//!
//! ## 已知限制
//!
//! - 模板发现按**路径前缀**精确匹配（大小写敏感），与笔记树一致；
//! - 标题里含 `/` 的笔记路径会有歧义（普通笔记允许这种标题），本插件不特殊处理；
//! - 设置页字段 Enter 保存、Esc 放弃本次输入（不做失焦自动保存）；
//! - 不做模板变量日期偏移/脚本/正则/命令面板。

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

use chrono::{Datelike, Timelike};
use gpui::prelude::*;
use gpui::{
    div, px, Context, Entity, FocusHandle, IntoElement, KeyDownEvent, MouseButton, Render,
    SharedString, Subscription, Window,
};
use menghunote_storage::{load_body_doc, TreeEntry, Vault};

use crate::editor::theme;
use crate::sidebar_rename::{RenameEvent, RenameInput};
use crate::ui_text;

/// 偏好文件当前版本。
pub const TEMPLATES_PREFERENCES_VERSION: u32 = 1;

/// 偏好文件名（平台配置目录下）。
pub const TEMPLATES_FILE_NAME: &str = "templates.json";

/// 测试/便携用的路径覆盖环境变量。
pub const TEMPLATES_PATH_ENV: &str = "MENGHUNOTE_TEMPLATES_PATH";

/// 默认模板文件夹（普通目录名，支持多级）。
pub const DEFAULT_TEMPLATE_FOLDER: &str = "模板";

/// 默认日期格式。
pub const DEFAULT_DATE_FORMAT: &str = "YYYY-MM-DD";

/// 默认时间格式。
pub const DEFAULT_TIME_FORMAT: &str = "HH:mm";

// ---------------------------------------------------------------------------
// 本机偏好（templates.json）
// ---------------------------------------------------------------------------

/// 模板插件本机偏好。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplatesPreferences {
    /// 是否启用（默认 `true`）。
    pub enabled: bool,
    /// 模板文件夹（普通目录路径，支持多级；空 = 未配置）。
    pub folder: String,
    /// 默认日期格式。
    pub date_format: String,
    /// 默认时间格式。
    pub time_format: String,
}

impl Default for TemplatesPreferences {
    fn default() -> Self {
        Self {
            enabled: true,
            folder: DEFAULT_TEMPLATE_FOLDER.to_owned(),
            date_format: DEFAULT_DATE_FORMAT.to_owned(),
            time_format: DEFAULT_TIME_FORMAT.to_owned(),
        }
    }
}

impl TemplatesPreferences {
    /// 从 JSON 文本解析；损坏/未知版本/未知字段一律按字段回退默认值。
    ///
    /// 手写解析（不把 `serde_json` 拉进默认构建）：格式固定为扁平的
    /// `version/enabled/folder/date_format/time_format`。
    #[must_use]
    pub fn parse(text: &str) -> Self {
        let Some(version) = json_number_field(text, "version") else {
            return Self::default();
        };
        if version != TEMPLATES_PREFERENCES_VERSION {
            return Self::default();
        }
        let defaults = Self::default();
        Self {
            enabled: json_bool_field(text, "enabled").unwrap_or(defaults.enabled),
            folder: json_string_field(text, "folder").unwrap_or(defaults.folder),
            date_format: json_string_field(text, "date_format").unwrap_or(defaults.date_format),
            time_format: json_string_field(text, "time_format").unwrap_or(defaults.time_format),
        }
    }

    /// 序列化为稳定 JSON（写入前先落临时文件再 rename）。
    #[must_use]
    pub fn to_json(&self) -> String {
        format!(
            "{{\n  \"version\": {TEMPLATES_PREFERENCES_VERSION},\n  \"enabled\": {},\n  \"folder\": \"{}\",\n  \"date_format\": \"{}\",\n  \"time_format\": \"{}\"\n}}\n",
            self.enabled,
            json_escape(&self.folder),
            json_escape(&self.date_format),
            json_escape(&self.time_format),
        )
    }

    /// 规整后的模板文件夹（去掉首尾空白与 `/`；空 = 未配置，不做模板发现）。
    #[must_use]
    pub fn normalized_folder(&self) -> String {
        normalize_folder(&self.folder)
    }

    /// 日期/时间默认格式（占位符渲染用）。
    #[must_use]
    pub fn placeholder_formats(&self) -> PlaceholderFormats<'_> {
        PlaceholderFormats {
            date: &self.date_format,
            time: &self.time_format,
        }
    }
}

/// 占位符默认格式（引用偏好里的两个字符串，避免复制）。
#[derive(Debug, Clone, Copy)]
pub struct PlaceholderFormats<'a> {
    /// `{{date}}` 的默认格式。
    pub date: &'a str,
    /// `{{time}}` 的默认格式。
    pub time: &'a str,
}

// ---------------------------------------------------------------------------
// 本地路径/原子写
// ---------------------------------------------------------------------------

/// 平台配置目录下的某个偏好文件路径；`env_override` 优先。
#[must_use]
pub(crate) fn config_path(env_override: &str, file_name: &str) -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(env_override) {
        return Some(PathBuf::from(path));
    }
    #[cfg(target_os = "windows")]
    {
        std::env::var_os("LOCALAPPDATA")
            .map(|base| PathBuf::from(base).join("menghunote").join(file_name))
    }
    #[cfg(target_os = "macos")]
    {
        std::env::var_os("HOME").map(|home| {
            PathBuf::from(home)
                .join("Library/Application Support/menghunote")
                .join(file_name)
        })
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
        Some(base.join("menghunote").join(file_name))
    }
}

/// 原子写文本：同目录临时文件 → rename；rename 失败退化为直接覆盖。
pub(crate) fn atomic_write(path: &Path, text: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
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

/// 平台配置目录下的模板偏好文件路径。
#[must_use]
pub fn settings_path() -> Option<PathBuf> {
    config_path(TEMPLATES_PATH_ENV, TEMPLATES_FILE_NAME)
}

/// 读取偏好；文件不存在/不可读/损坏都回退默认（不报错、不阻塞启动）。
#[must_use]
pub fn load_preferences(path: &Path) -> TemplatesPreferences {
    let Ok(text) = std::fs::read_to_string(path) else {
        return TemplatesPreferences::default();
    };
    TemplatesPreferences::parse(&text)
}

/// 原子写入偏好。
pub fn save_preferences(path: &Path, preferences: &TemplatesPreferences) -> io::Result<()> {
    atomic_write(path, &preferences.to_json())
}

// ---------------------------------------------------------------------------
// 运行时状态（懒加载 + 本机写盘目标）
// ---------------------------------------------------------------------------

/// 进程级偏好缓存（`None` = 尚未从磁盘加载）。
static STATE: RwLock<Option<TemplatesPreferences>> = RwLock::new(None);

/// 写盘目标（只在本机偏好文件路径可用时 `Some`）。
static STORE_PATH: RwLock<Option<PathBuf>> = RwLock::new(None);

fn read_state() -> RwLockReadGuard<'static, Option<TemplatesPreferences>> {
    STATE.read().unwrap_or_else(|error| error.into_inner())
}

fn write_state() -> RwLockWriteGuard<'static, Option<TemplatesPreferences>> {
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

/// 当前模板偏好（首次调用时从平台配置目录懒加载）。
///
/// 为什么不放在 `main.rs` 显式初始化：偏好是**纯本机设置**，任何渲染路径第一次
/// 读到它时加载即可；文件缺失/损坏都回退默认，不影响启动，也不需要新增启动顺序。
#[must_use]
pub fn preferences() -> TemplatesPreferences {
    let cached = read_state().clone();
    if let Some(preferences) = cached {
        return preferences;
    }
    let (loaded, path) = match settings_path() {
        Some(path) => (load_preferences(&path), Some(path)),
        None => (TemplatesPreferences::default(), None),
    };
    *write_state() = Some(loaded.clone());
    *write_store_path() = path;
    loaded
}

/// 覆盖当前偏好并写盘（返回 `true` 表示真的变了）。
///
/// 设置页字段提交后调用；**不阻塞**，写盘失败只影响下次启动（当前会话仍生效）。
pub fn set_preferences(next: TemplatesPreferences) -> bool {
    let changed = preferences() != next;
    *write_state() = Some(next.clone());
    if changed {
        persist(&next);
    }
    changed
}

fn persist(preferences: &TemplatesPreferences) {
    let path = read_store_path().clone().or_else(settings_path);
    let Some(path) = path else {
        return;
    };
    *write_store_path() = Some(path.clone());
    if let Err(error) = save_preferences(&path, preferences) {
        eprintln!("[menghunote] 保存模板偏好失败：{error}");
    }
}

/// 从**显式路径**加载偏好（测试/便携入口；同时记住后续写盘目标）。
#[doc(hidden)]
pub fn init_from_path(path: PathBuf) {
    let preferences = load_preferences(&path);
    *write_state() = Some(preferences);
    *write_store_path() = Some(path);
}

// ---------------------------------------------------------------------------
// 日期/时间 token 格式化（纯逻辑）
// ---------------------------------------------------------------------------

/// 本地日期时间分量（不依赖 `chrono` 类型，纯函数可单测）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DateTimeParts {
    /// 年（完整 4 位）。
    pub year: i32,
    /// 月 1–12。
    pub month: u32,
    /// 日 1–31。
    pub day: u32,
    /// 时 0–23。
    pub hour: u32,
    /// 分 0–59。
    pub minute: u32,
    /// 秒 0–59。
    pub second: u32,
}

impl DateTimeParts {
    /// 当前**本地**时间。
    #[must_use]
    pub fn now() -> Self {
        let now = chrono::Local::now();
        Self {
            year: now.year(),
            month: now.month(),
            day: now.day(),
            hour: now.hour(),
            minute: now.minute(),
            second: now.second(),
        }
    }

    /// 按天偏移（负 = 过去）；时/分/秒不变，只推进**本地墙钟日期**。
    ///
    /// 跨月/跨年/闰日由公历计算（`chrono::NaiveDate`），不做时区/夏令时换算；
    /// 公历范围外返回 `None`（调用方原样保留占位符）。
    #[must_use]
    pub fn add_days(self, days: i32) -> Option<Self> {
        let date = chrono::NaiveDate::from_ymd_opt(self.year, self.month, self.day)?;
        let shifted = date.checked_add_signed(chrono::Duration::days(i64::from(days)))?;
        Some(self.with_date(shifted))
    }

    /// 按月偏移（负 = 过去）；目标月没有该日时取该月最后一天（月末对齐）。
    #[must_use]
    pub fn add_months(self, months: i32) -> Option<Self> {
        let date = chrono::NaiveDate::from_ymd_opt(self.year, self.month, self.day)?;
        Some(self.with_date(shift_months(date, months)?))
    }

    /// 按年偏移（负 = 过去）；`2 月 29 日` 落到平年时取 `2 月 28 日`。
    #[must_use]
    pub fn add_years(self, years: i32) -> Option<Self> {
        self.add_months(years.checked_mul(12)?)
    }

    /// 只替换日期分量，保留时间分量。
    fn with_date(self, date: chrono::NaiveDate) -> Self {
        Self {
            year: date.year(),
            month: date.month(),
            day: date.day(),
            hour: self.hour,
            minute: self.minute,
            second: self.second,
        }
    }
}

/// 公历按月偏移；目标月没有该日时取该月最后一天（与日记「上个月同一天」一致）。
fn shift_months(date: chrono::NaiveDate, months: i32) -> Option<chrono::NaiveDate> {
    // 以「年 * 12 + 月序号」做整数运算，避免逐月循环；负年份用 div_euclid 归一。
    let total = date
        .year()
        .checked_mul(12)?
        .checked_add(i32::try_from(date.month0()).ok()?)?
        .checked_add(months)?;
    let year = total.div_euclid(12);
    let month = u32::try_from(total.rem_euclid(12)).ok()?.checked_add(1)?;
    let day = date.day().min(days_in_month(year, month)?);
    chrono::NaiveDate::from_ymd_opt(year, month, day)
}

/// 公历某月的天数（`month` 1–12；非法年月返回 `None`）。
fn days_in_month(year: i32, month: u32) -> Option<u32> {
    let first = chrono::NaiveDate::from_ymd_opt(year, month, 1)?;
    let (next_year, next_month) = if month == 12 {
        (year.checked_add(1)?, 1)
    } else {
        (year, month + 1)
    };
    let next = chrono::NaiveDate::from_ymd_opt(next_year, next_month, 1)?;
    u32::try_from((next - first).num_days()).ok()
}

/// 相对日期单位（`{{date:-1d}}` 里的 `d` / `w` / `m` / `y`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelativeDateUnit {
    /// 天。
    Day,
    /// 周（7 天）。
    Week,
    /// 月（公历；目标月没有该日时取月末）。
    Month,
    /// 年（公历；`2/29` 落到平年时取 `2/28`）。
    Year,
}

/// 相对日期偏移（`{{date:-1d}}` 的 `-1d`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RelativeDateOffset {
    /// 有符号数量（严格解析时绝对值 0–99）。
    pub amount: i32,
    /// 单位。
    pub unit: RelativeDateUnit,
}

impl RelativeDateOffset {
    /// 严格解析偏移表达式：`[+-]?` + **1–2 位** ASCII 数字 + `d`/`w`/`m`/`y`。
    ///
    /// 只接受小写单位、不接受空白/多余字符；不合法返回 `None`
    /// （调用方据此**原样保留**整个占位符，不 panic、不猜测）。
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let mut characters = text.chars();
        let mut first = characters.next()?;
        let mut sign = 1i32;
        if first == '+' || first == '-' {
            if first == '-' {
                sign = -1;
            }
            first = characters.next()?;
        }
        let mut amount = 0i32;
        let mut digits = 0usize;
        let mut unit: Option<RelativeDateUnit> = None;
        for character in std::iter::once(first).chain(characters) {
            if character.is_ascii_digit() {
                // 单位之后不能再有数字；最多两位。
                if unit.is_some() || digits == 2 {
                    return None;
                }
                digits += 1;
                amount = amount * 10 + i32::from(character as u8 - b'0');
                continue;
            }
            if unit.is_some() {
                return None;
            }
            unit = match character {
                'd' => Some(RelativeDateUnit::Day),
                'w' => Some(RelativeDateUnit::Week),
                'm' => Some(RelativeDateUnit::Month),
                'y' => Some(RelativeDateUnit::Year),
                _ => return None,
            };
        }
        if digits == 0 {
            return None;
        }
        Some(Self {
            amount: sign * amount,
            unit: unit?,
        })
    }

    /// 是否是「偏移尝试」：以 `+` / `-` / ASCII 数字开头。
    ///
    /// 用来把 `{{date:1x}}` 这类**非法偏移**（原样保留）与
    /// `{{date:YYYY-MM-DD}}` 这类格式串（仍按格式渲染）区分开。
    #[must_use]
    pub fn looks_like_offset(text: &str) -> bool {
        match text.chars().next() {
            Some('+' | '-') => true,
            Some(character) => character.is_ascii_digit(),
            None => false,
        }
    }

    /// 应用到日期分量（时间分量不变）；越界/日期非法返回 `None`。
    #[must_use]
    pub fn apply(self, parts: &DateTimeParts) -> Option<DateTimeParts> {
        match self.unit {
            RelativeDateUnit::Day => parts.add_days(self.amount),
            RelativeDateUnit::Week => parts.add_days(self.amount.checked_mul(7)?),
            RelativeDateUnit::Month => parts.add_months(self.amount),
            RelativeDateUnit::Year => parts.add_years(self.amount),
        }
    }
}

/// 支持的时间格式 token（**最长匹配优先**，所以 4 字符在前）。
const TOKENS: [&str; 12] = [
    "YYYY", "YY", "MM", "DD", "HH", "mm", "ss", "M", "D", "H", "m", "s",
];

fn token_value(parts: &DateTimeParts, token: &str) -> String {
    match token {
        "YYYY" => format!("{:04}", parts.year),
        "YY" => format!("{:02}", parts.year.rem_euclid(100)),
        "MM" => format!("{:02}", parts.month),
        "M" => parts.month.to_string(),
        "DD" => format!("{:02}", parts.day),
        "D" => parts.day.to_string(),
        "HH" => format!("{:02}", parts.hour),
        "H" => parts.hour.to_string(),
        "mm" => format!("{:02}", parts.minute),
        "m" => parts.minute.to_string(),
        "ss" => format!("{:02}", parts.second),
        "s" => parts.second.to_string(),
        _ => String::new(),
    }
}

/// 按 token 格式化本地时间。
///
/// 规则：从左到右扫描，**先匹配最长 token**（`YYYY` > `YY`，`MM` > `M`……）；
/// 未匹配的字符原样保留（未知 token 不报错、不丢弃）。不做转义语法。
#[must_use]
pub fn format_datetime(parts: &DateTimeParts, format: &str) -> String {
    let mut out = String::with_capacity(format.len() + 8);
    let mut rest = format;
    'outer: while !rest.is_empty() {
        for token in TOKENS {
            if let Some(after) = rest.strip_prefix(token) {
                out.push_str(&token_value(parts, token));
                rest = after;
                continue 'outer;
            }
        }
        let Some(character) = rest.chars().next() else {
            break;
        };
        out.push(character);
        rest = &rest[character.len_utf8()..];
    }
    out
}

/// 把模板正文里的占位符替换为实际值（未知占位符原样保留）。
///
/// 只识别 `{{date}}` / `{{time}}` / `{{title}}` / `{{date:FMT}}` / `{{time:FMT}}` /
/// `{{date:OFFSET}}` / `{{date:OFFSET:FMT}}`；其它 `{{...}}` 与未闭合的 `{{`
/// 一律逐字保留（不 panic、不猜测）。相对日期的语法与边界见模块文档。
#[must_use]
pub fn render_placeholders(
    text: &str,
    parts: &DateTimeParts,
    title: &str,
    formats: PlaceholderFormats<'_>,
) -> String {
    let mut out = String::with_capacity(text.len() + 32);
    let mut cursor = 0usize;
    while let Some(offset) = text[cursor..].find("{{") {
        let start = cursor + offset;
        out.push_str(&text[cursor..start]);
        let inner_start = start + 2;
        let Some(close) = text[inner_start..].find("}}") else {
            // 未闭合：剩余内容原样保留，结束扫描。
            out.push_str(&text[start..]);
            return out;
        };
        let inner_end = inner_start + close;
        let inner = &text[inner_start..inner_end];
        if let Some(replacement) = placeholder_value(inner, parts, title, formats) {
            out.push_str(&replacement);
        } else {
            out.push_str(&text[start..inner_end + 2]);
        }
        cursor = inner_end + 2;
    }
    out.push_str(&text[cursor..]);
    out
}

fn placeholder_value(
    inner: &str,
    parts: &DateTimeParts,
    title: &str,
    formats: PlaceholderFormats<'_>,
) -> Option<String> {
    if inner == "title" {
        return Some(title.to_owned());
    }
    if inner == "date" {
        return Some(format_datetime(parts, formats.date));
    }
    if inner == "time" {
        return Some(format_datetime(parts, formats.time));
    }
    if let Some(rest) = inner.strip_prefix("date:") {
        return date_override(rest, parts, formats.date);
    }
    if let Some(format) = inner.strip_prefix("time:") {
        let format = if format.is_empty() {
            formats.time
        } else {
            format
        };
        return Some(format_datetime(parts, format));
    }
    None
}

/// `{{date:REST}}`：**先判偏移，再判格式**。
///
/// - `REST` 是合法偏移（`-1d`）→ 偏移后的日期按默认格式；
/// - `REST` 是 `偏移:格式`（`-1d:YYYY/MM/DD`）→ 偏移后的日期按该格式；
/// - 看起来是偏移但不合法（`+d` / `1x` / `1x:YYYY` / 空偏移 `:YYYY`）或偏移越界
///   → `None`（调用方把整个占位符**原样保留**）；
/// - 其它 → 沿用旧语义：`REST` 整体是格式串（可含冒号，如 `YYYY:MM`）。
fn date_override(rest: &str, parts: &DateTimeParts, default_format: &str) -> Option<String> {
    // 旧语义：空格式回退默认（`{{date:}}`）。
    if rest.is_empty() {
        return Some(format_datetime(parts, default_format));
    }
    let (head, tail) = match rest.split_once(':') {
        Some((head, tail)) => (head, Some(tail)),
        None => (rest, None),
    };
    if let Some(offset) = RelativeDateOffset::parse(head) {
        let shifted = offset.apply(parts)?;
        let format = match tail {
            None | Some("") => default_format,
            Some(format) => format,
        };
        return Some(format_datetime(&shifted, format));
    }
    // 冒号前是空串（`date::FMT`）= 空偏移；或值以 `+`/`-`/数字开头 = 非法偏移尝试。
    // 两者都按「非法偏移」原样保留，不猜测用户意图。
    if (tail.is_some() && head.is_empty()) || RelativeDateOffset::looks_like_offset(head) {
        return None;
    }
    // 旧语义：REST 整体是格式串。
    Some(format_datetime(parts, rest))
}

// ---------------------------------------------------------------------------
// 模板发现（普通笔记 + 路径拼接）
// ---------------------------------------------------------------------------

/// 一篇模板（普通笔记）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateItem {
    /// 树节点 ID。
    pub node_id: String,
    /// 笔记 ID（普通笔记语义）。
    pub note_id: String,
    /// 正文文档 ID（插入时按源文读取）。
    pub body_doc_id: String,
    /// 笔记标题（树里的最新值）。
    pub title: String,
    /// 相对模板文件夹的路径（`日记模板/周记`；同标题靠它区分）。
    pub relative_path: String,
}

/// 模板发现结果（含空态原因，供选择器与测试共用）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TemplateList {
    /// 规整后的模板文件夹（空 = 未配置）。
    pub folder: String,
    /// 该文件夹在树里是否存在（空文件夹也存在，只是没有笔记）。
    pub folder_exists: bool,
    /// 模板（按相对路径字典序稳定排序）。
    pub items: Vec<TemplateItem>,
}

/// 规整文件夹路径：去首尾空白与 `/`；内部连续 `/` 折叠。
#[must_use]
pub fn normalize_folder(folder: &str) -> String {
    folder
        .trim()
        .trim_matches('/')
        .split('/')
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>()
        .join("/")
}

/// 树里每个节点的完整路径（`parent` 递归 + `/` 拼接；根级 = 标题）。
///
/// 脏数据（父节点缺失/环）不会死循环：向上最多走 `tree.len()` 步。
#[must_use]
pub fn node_paths(tree: &[TreeEntry]) -> HashMap<String, String> {
    let by_id: HashMap<&str, &TreeEntry> = tree
        .iter()
        .map(|entry| (entry.node_id.as_str(), entry))
        .collect();
    let mut paths = HashMap::with_capacity(tree.len());
    for entry in tree {
        let mut titles: Vec<&str> = Vec::new();
        let mut current = Some(entry);
        let mut steps = 0usize;
        while let Some(node) = current {
            if steps > tree.len() {
                break;
            }
            steps += 1;
            titles.push(node.title.as_str());
            current = node
                .parent_id
                .as_deref()
                .and_then(|parent| by_id.get(parent).copied());
        }
        titles.reverse();
        paths.insert(entry.node_id.clone(), titles.join("/"));
    }
    paths
}

/// 列出模板文件夹子树下的全部笔记（**不创建**文件夹、不改树）。
#[must_use]
pub fn collect_templates(tree: &[TreeEntry], folder: &str) -> TemplateList {
    let folder = normalize_folder(folder);
    if folder.is_empty() {
        return TemplateList {
            folder,
            folder_exists: false,
            items: Vec::new(),
        };
    }
    let paths = node_paths(tree);
    let folder_exists = tree.iter().any(|entry| {
        entry.is_folder()
            && paths
                .get(&entry.node_id)
                .is_some_and(|path| *path == folder)
    });
    let prefix = format!("{folder}/");
    let mut items: Vec<TemplateItem> = tree
        .iter()
        .filter(|entry| entry.is_note())
        .filter_map(|entry| {
            let path = paths.get(&entry.node_id)?;
            let relative = path.strip_prefix(&prefix)?;
            if relative.is_empty() {
                return None;
            }
            Some(TemplateItem {
                node_id: entry.node_id.clone(),
                note_id: entry.note_id.clone()?,
                body_doc_id: entry.body_doc_id.clone()?,
                title: entry.title.clone(),
                relative_path: relative.to_owned(),
            })
        })
        .collect();
    items.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    TemplateList {
        folder,
        folder_exists,
        items,
    }
}

/// 按完整路径找笔记在 `tree` 里的下标（精确匹配；找不到返回 `None`）。
#[must_use]
pub fn note_index_by_path(tree: &[TreeEntry], path: &str) -> Option<usize> {
    let paths = node_paths(tree);
    tree.iter()
        .position(|entry| entry.is_note() && paths.get(&entry.node_id).is_some_and(|p| p == path))
}

/// 按完整路径找节点（笔记或目录）的标题与下标。
#[must_use]
pub fn node_index_by_path(tree: &[TreeEntry], path: &str) -> Option<usize> {
    let paths = node_paths(tree);
    tree.iter()
        .position(|entry| paths.get(&entry.node_id).is_some_and(|p| p == path))
}

/// 读取任意正文文档的源文（模板/日记共用；失败返回可展示的原因）。
pub fn read_body_source(vault: &Vault, body_doc_id: &str) -> Result<String, String> {
    let (doc, _head) =
        load_body_doc(vault, body_doc_id, 0, None).map_err(|error| format!("{error}"))?;
    Ok(doc.text())
}

/// 读取模板正文（源文）；失败返回可展示的原因（不 panic、不改库）。
pub fn read_template_body(vault: &Vault, item: &TemplateItem) -> Result<String, String> {
    read_body_source(vault, &item.body_doc_id)
}

// ---------------------------------------------------------------------------
// 模板选择器（覆盖层实体）
// ---------------------------------------------------------------------------

/// 选择器用途：插入到当前 caret / 从模板新建。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemplatePickerPurpose {
    /// 插入到当前笔记 caret（有选区则替换）。
    Insert,
    /// 用模板新建一篇笔记并打开。
    NewNote,
}

/// 选择器事件（宿主据此执行动作）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemplatePickerEvent {
    /// 选中了模板（`items` 下标）。
    Picked {
        /// `TemplateList::items` 里的下标（不是过滤后的行号）。
        index: usize,
    },
    /// 用户关闭（`Esc` / 点空白）。
    Closed,
}

/// 模板选择器：搜索 + ↑/↓ + Enter + 点击 + Esc。
pub struct TemplatePicker {
    purpose: TemplatePickerPurpose,
    list: TemplateList,
    search: Entity<RenameInput>,
    filter: String,
    /// 过滤后的行 → `list.items` 下标。
    matches: Vec<usize>,
    selected: usize,
    focus_handle: FocusHandle,
    _search_events: Subscription,
}

impl TemplatePicker {
    /// 用一次模板发现结果组装选择器（内部创建 IME 输入框）。
    pub fn new(list: TemplateList, purpose: TemplatePickerPurpose, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| RenameInput::new_cell_at_end("", cx));
        let search_events = cx.subscribe(
            &search,
            |picker: &mut Self, _input, event: &RenameEvent, cx| {
                picker.on_search_event(event, cx);
            },
        );
        let mut picker = Self {
            purpose,
            list,
            search,
            filter: String::new(),
            matches: Vec::new(),
            selected: 0,
            focus_handle: cx.focus_handle(),
            _search_events: search_events,
        };
        picker.refilter();
        picker
    }

    /// 选择器用途。
    #[must_use]
    pub fn purpose(&self) -> TemplatePickerPurpose {
        self.purpose
    }

    /// 模板发现结果（只读）。
    #[must_use]
    pub fn list(&self) -> &TemplateList {
        &self.list
    }

    /// 当前过滤结果（`items` 下标）。
    #[must_use]
    pub fn matches(&self) -> &[usize] {
        &self.matches
    }

    /// 当前选中行（过滤结果行号）。
    #[must_use]
    pub fn selected_row(&self) -> usize {
        self.selected
    }

    /// 搜索框实体（宿主/测试用）。
    #[must_use]
    pub fn search_input(&self) -> Entity<RenameInput> {
        self.search.clone()
    }

    /// 把焦点交给搜索框（打开时调用一次）。
    pub fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.search.update(cx, |input, cx| input.focus(window, cx));
    }

    /// 当前过滤串。
    #[must_use]
    pub fn filter(&self) -> &str {
        &self.filter
    }

    fn on_search_event(&mut self, event: &RenameEvent, cx: &mut Context<Self>) {
        match event {
            RenameEvent::Changed => {
                self.filter = self.search.read(cx).text().to_owned();
                self.refilter();
                cx.notify();
            }
            RenameEvent::Commit(_) => self.pick_selected(cx),
            RenameEvent::Cancel => cx.emit(TemplatePickerEvent::Closed),
            RenameEvent::Blank | RenameEvent::Advance { .. } => {}
        }
    }

    fn refilter(&mut self) {
        let needle = self.filter.trim().to_lowercase();
        self.matches = self
            .list
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| {
                needle.is_empty()
                    || item.title.to_lowercase().contains(&needle)
                    || item.relative_path.to_lowercase().contains(&needle)
            })
            .map(|(index, _)| index)
            .collect();
        if self.matches.is_empty() {
            self.selected = 0;
        } else if self.selected >= self.matches.len() {
            self.selected = self.matches.len() - 1;
        }
    }

    fn move_selection(&mut self, forward: bool, cx: &mut Context<Self>) {
        if self.matches.is_empty() {
            return;
        }
        let count = self.matches.len();
        self.selected = if forward {
            (self.selected + 1) % count
        } else {
            (self.selected + count - 1) % count
        };
        cx.notify();
    }

    fn pick_selected(&mut self, cx: &mut Context<Self>) {
        if let Some(&index) = self.matches.get(self.selected) {
            cx.emit(TemplatePickerEvent::Picked { index });
        }
    }

    fn pick_row(&mut self, row: usize, cx: &mut Context<Self>) {
        if let Some(&index) = self.matches.get(row) {
            cx.emit(TemplatePickerEvent::Picked { index });
        }
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        // 组合态的方向键属于输入法候选，不能拿来移列表选中。
        if self.search.read(cx).state().marked_utf16().is_some() {
            return;
        }
        match event.keystroke.key.as_str() {
            "up" => {
                self.move_selection(false, cx);
                cx.stop_propagation();
            }
            "down" => {
                self.move_selection(true, cx);
                cx.stop_propagation();
            }
            _ => {}
        }
    }

    fn empty_text(&self) -> String {
        if self.list.folder.is_empty() {
            return ui_text::templates_empty_no_folder();
        }
        if !self.list.items.is_empty() {
            return ui_text::templates_empty_no_match();
        }
        if self.list.folder_exists {
            ui_text::templates_empty_folder(&self.list.folder)
        } else {
            ui_text::templates_empty_missing(&self.list.folder)
        }
    }

    fn search_box(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let input = self.search.clone();
        let focus_input = self.search.clone();
        div()
            .id("template-picker-search")
            .debug_selector(|| "template-picker-search".to_string())
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
            .child(div().flex_1().min_w(px(0.0)).h_full().child(input))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_picker, _event, _window, cx| cx.stop_propagation()),
            )
    }
}

impl gpui::EventEmitter<TemplatePickerEvent> for TemplatePicker {}

impl Render for TemplatePicker {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let title = match self.purpose {
            TemplatePickerPurpose::Insert => ui_text::templates_picker_title_insert(),
            TemplatePickerPurpose::NewNote => ui_text::templates_picker_title_new(),
        };
        let focused = self.focus_handle.clone();
        let rows: Vec<_> = self
            .matches
            .iter()
            .enumerate()
            .map(|(row, &index)| {
                let item = &self.list.items[index];
                let selected = row == self.selected;
                let label = item.title.clone();
                let path = item.relative_path.clone();
                div()
                    .id(SharedString::from(format!("template-picker-item-{row}")))
                    .debug_selector(move || format!("template-picker-item-{row}"))
                    .w_full()
                    .h(px(38.0))
                    .flex_shrink_0()
                    .px_2()
                    .flex()
                    .flex_col()
                    .justify_center()
                    .gap(px(1.0))
                    .rounded_sm()
                    .cursor_pointer()
                    .when(selected, |this| this.bg(theme::sidebar_selected_bg()))
                    .hover(|style| style.bg(theme::sidebar_selected_bg()))
                    .child(
                        div()
                            .w_full()
                            .overflow_hidden()
                            .text_ellipsis()
                            .text_size(px(12.0))
                            .text_color(theme::text_color())
                            .child(SharedString::from(label)),
                    )
                    .child(
                        div()
                            .w_full()
                            .overflow_hidden()
                            .text_ellipsis()
                            .text_size(px(10.0))
                            .text_color(theme::status_bar_text())
                            .child(SharedString::from(path)),
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |picker, _event, _window, cx| {
                            picker.pick_row(row, cx);
                            cx.stop_propagation();
                        }),
                    )
            })
            .collect();

        let list_height = (rows.len() as f32 * 38.0).clamp(0.0, 320.0);
        let panel_height = list_height + 92.0;
        let mut list = div()
            .id("template-picker-list")
            .debug_selector(|| "template-picker-list".to_string())
            .w_full()
            .h(px(list_height))
            .flex()
            .flex_col()
            .gap(px(1.0))
            .overflow_y_scroll()
            .children(rows);
        if self.matches.is_empty() {
            list = list.child(
                div()
                    .debug_selector(|| "template-picker-empty".to_string())
                    .w_full()
                    .py_2()
                    .text_size(px(11.0))
                    .text_color(theme::status_bar_text())
                    .child(SharedString::from(self.empty_text())),
            );
        }

        let panel = div()
            .id("template-picker-panel")
            .debug_selector(|| "template-picker-panel".to_string())
            .w(px(520.0))
            .h(px(panel_height))
            .flex()
            .flex_col()
            .gap_1()
            .p(px(8.0))
            .bg(theme::menu_bg())
            .border_1()
            .border_color(theme::sidebar_border())
            .rounded_md()
            .shadow_lg()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_picker, _event, _window, cx| cx.stop_propagation()),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(theme::text_color())
                            .child(SharedString::from(title)),
                    )
                    .child(
                        div()
                            .text_size(px(10.0))
                            .text_color(theme::status_bar_text())
                            .child(SharedString::from(ui_text::templates_picker_hint())),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_size(px(11.0))
                            .text_color(theme::status_bar_text())
                            .child(SharedString::from(ui_text::templates_picker_search())),
                    )
                    .child(self.search_box(cx)),
            )
            .child(list);

        div()
            .id("template-picker")
            .debug_selector(|| "template-picker".to_string())
            .absolute()
            .inset_0()
            .flex()
            .flex_col()
            .items_center()
            .pt(px(72.0))
            .bg(theme::overlay_scrim())
            .track_focus(&focused)
            .on_key_down(cx.listener(Self::on_key_down))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_picker, _event, _window, cx| {
                    cx.emit(TemplatePickerEvent::Closed);
                    cx.stop_propagation();
                }),
            )
            .child(panel)
    }
}

// ---------------------------------------------------------------------------
// 设置页「模板」区（自包含实体）
// ---------------------------------------------------------------------------

/// 模板设置里的可编辑字段（提交/校验用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemplatesField {
    /// 模板文件夹。
    Folder,
    /// 默认日期格式。
    DateFormat,
    /// 默认时间格式。
    TimeFormat,
}

impl TemplatesField {
    /// 所有字段（渲染顺序）。
    pub const ALL: [Self; 3] = [Self::Folder, Self::DateFormat, Self::TimeFormat];

    /// 固定 selector 后缀。
    #[must_use]
    pub const fn selector(self) -> &'static str {
        match self {
            Self::Folder => "settings-templates-folder",
            Self::DateFormat => "settings-templates-date-format",
            Self::TimeFormat => "settings-templates-time-format",
        }
    }

    /// 字段展示名（当前语言）。
    #[must_use]
    pub fn label(self) -> String {
        match self {
            Self::Folder => ui_text::templates_folder_label(),
            Self::DateFormat => ui_text::templates_date_format_label(),
            Self::TimeFormat => ui_text::templates_time_format_label(),
        }
    }
}

/// 设置页「模板」区：自包含实体（拥有三个 IME 输入框 + 启用开关）。
///
/// 为什么是实体而不是纯函数：设置字段要能打字（必须实现 `EntityInputHandler`，
/// 组件 Input 没有 IME 接入），输入框状态需要一个宿主实体持有。
pub struct TemplatesSettingsPanel {
    fields: Vec<(TemplatesField, Entity<RenameInput>)>,
    notice: Option<String>,
    _subs: Vec<Subscription>,
}

impl TemplatesSettingsPanel {
    /// 按当前偏好组装面板（字段 Enter 提交 → 写本机 `templates.json`）。
    pub fn new(cx: &mut Context<Self>) -> Self {
        let preferences = preferences();
        let mut fields = Vec::with_capacity(TemplatesField::ALL.len());
        let mut subs = Vec::with_capacity(TemplatesField::ALL.len());
        for field in TemplatesField::ALL {
            let initial = match field {
                TemplatesField::Folder => preferences.folder.clone(),
                TemplatesField::DateFormat => preferences.date_format.clone(),
                TemplatesField::TimeFormat => preferences.time_format.clone(),
            };
            let input = cx.new(|cx| RenameInput::new_at_end(&initial, cx));
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

    /// 当前某个字段的输入框（测试/诊断用）。
    #[must_use]
    pub fn field_input(&self, field: TemplatesField) -> Option<Entity<RenameInput>> {
        self.fields
            .iter()
            .find(|(current, _)| *current == field)
            .map(|(_, input)| input.clone())
    }

    /// 保存提示（`None` = 没有提示）。
    #[must_use]
    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    /// 当前日期/时间示例（设置页展示与测试共用）。
    #[must_use]
    pub fn example(preferences: &TemplatesPreferences, parts: &DateTimeParts) -> String {
        let date = format_datetime(parts, &preferences.date_format);
        let time = format_datetime(parts, &preferences.time_format);
        format!("{date} {time}")
    }

    fn on_field_event(
        &mut self,
        field: TemplatesField,
        event: &RenameEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            RenameEvent::Commit(text) => self.commit_field(field, text, cx),
            RenameEvent::Blank => {
                self.notice = Some(ui_text::templates_rejected());
                cx.notify();
            }
            RenameEvent::Cancel | RenameEvent::Changed | RenameEvent::Advance { .. } => {}
        }
    }

    fn commit_field(&mut self, field: TemplatesField, text: &str, cx: &mut Context<Self>) {
        let mut preferences = preferences();
        let value = text.trim().to_owned();
        let valid = match field {
            TemplatesField::Folder => !normalize_folder(&value).is_empty(),
            // 格式必须真的产出非空文本（例如 `/` 这种只产出分隔符的格式要拒绝）。
            TemplatesField::DateFormat => !format_datetime(&DateTimeParts::now(), &value)
                .trim()
                .is_empty(),
            TemplatesField::TimeFormat => !format_datetime(&DateTimeParts::now(), &value)
                .trim()
                .is_empty(),
        };
        if !valid {
            self.notice = Some(ui_text::templates_rejected());
            cx.notify();
            return;
        }
        match field {
            TemplatesField::Folder => preferences.folder = value,
            TemplatesField::DateFormat => preferences.date_format = value,
            TemplatesField::TimeFormat => preferences.time_format = value,
        }
        set_preferences(preferences);
        self.notice = Some(ui_text::templates_saved());
        cx.notify();
    }

    fn field_row(&self, field: TemplatesField) -> impl IntoElement {
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

impl Render for TemplatesSettingsPanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let preferences = preferences();
        let parts = DateTimeParts::now();
        let example = Self::example(&preferences, &parts);
        div()
            .id("settings-templates")
            .debug_selector(|| "settings-templates".to_string())
            .w_full()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(theme::text_color())
                    .child(SharedString::from(ui_text::templates_title())),
            )
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(theme::status_bar_text())
                    .child(SharedString::from(ui_text::templates_hint())),
            )
            .children(
                TemplatesField::ALL
                    .into_iter()
                    .map(|field| self.field_row(field)),
            )
            .child(
                div()
                    .debug_selector(|| "settings-templates-example".to_string())
                    .text_size(px(11.0))
                    .text_color(theme::status_bar_text())
                    .child(SharedString::from(ui_text::templates_example(&example))),
            )
            .child(
                div()
                    .debug_selector(|| "settings-templates-notice".to_string())
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

/// 模板偏好测试守卫：`reset` 设路径+默认值；`Drop` 恢复默认并清空写盘目标。
#[doc(hidden)]
pub struct TemplatesTestGuard {
    _lock: MutexGuard<'static, ()>,
}

impl TemplatesTestGuard {
    /// 重置到默认值，并指定写盘目标（`None` = 不持久化）。
    pub fn reset(&self, store_path: Option<PathBuf>) {
        templates_test_reset(store_path);
    }

    /// 直接从给定路径加载（模拟重启）。
    pub fn init(&self, path: PathBuf) {
        init_from_path(path);
    }
}

impl Drop for TemplatesTestGuard {
    fn drop(&mut self) {
        templates_test_reset(None);
    }
}

fn templates_test_reset(store_path: Option<PathBuf>) {
    *write_state() = Some(TemplatesPreferences {
        enabled: true,
        folder: DEFAULT_TEMPLATE_FOLDER.to_owned(),
        date_format: DEFAULT_DATE_FORMAT.to_owned(),
        time_format: DEFAULT_TIME_FORMAT.to_owned(),
    });
    *write_store_path() = store_path;
}

/// 串行化模板偏好测试并自动复位（只给 `crates/app/tests/**` 与 crate 内测试用）。
#[doc(hidden)]
#[must_use]
pub fn lock_for_test() -> TemplatesTestGuard {
    let lock = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    // 先置默认 + 清空写盘目标：避免测试进程懒加载真实用户配置（`$XDG_CONFIG_HOME`）。
    templates_test_reset(None);
    TemplatesTestGuard { _lock: lock }
}

// ---------------------------------------------------------------------------
// 手写 JSON（只支持本模块写出的扁平对象）
// ---------------------------------------------------------------------------

/// JSON 字符串转义（只服务 `to_json`；解析在 [`json_string_field`]）。
pub(crate) fn json_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other => out.push(other),
        }
    }
    out
}

fn field_value_start<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("\"{key}\"");
    let start = text.find(&needle)? + needle.len();
    let rest = &text[start..];
    let rest = rest.trim_start().strip_prefix(':')?;
    Some(rest.trim_start())
}

pub(crate) fn json_string_field(text: &str, key: &str) -> Option<String> {
    let rest = field_value_start(text, key)?;
    let rest = rest.strip_prefix('"')?;
    let mut out = String::new();
    let mut characters = rest.chars();
    while let Some(character) = characters.next() {
        match character {
            '"' => return Some(out),
            '\\' => match characters.next()? {
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                '"' => out.push('"'),
                '\\' => out.push('\\'),
                other => out.push(other),
            },
            other => out.push(other),
        }
    }
    None
}

pub(crate) fn json_bool_field(text: &str, key: &str) -> Option<bool> {
    let rest = field_value_start(text, key)?;
    if rest.starts_with("true") {
        Some(true)
    } else if rest.starts_with("false") {
        Some(false)
    } else {
        None
    }
}

pub(crate) fn json_number_field(text: &str, key: &str) -> Option<u32> {
    let rest = field_value_start(text, key)?;
    let end = rest
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
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

    fn formats() -> PlaceholderFormats<'static> {
        PlaceholderFormats {
            date: "YYYY-MM-DD",
            time: "HH:mm",
        }
    }

    fn test_root() -> PathBuf {
        // lib 单测没有 `CARGO_TARGET_TMPDIR`，统一落仓库 target/tmp（AGENTS §6）。
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/tmp")
            .join(format!("templates-preferences-{}", std::process::id()))
    }

    #[test]
    fn formats_all_tokens_and_keeps_unknown_text() {
        let parts = parts();
        assert_eq!(format_datetime(&parts, "YYYY-MM-DD"), "2026-11-18");
        assert_eq!(format_datetime(&parts, "YY/M/D"), "26/11/18");
        assert_eq!(format_datetime(&parts, "HH:mm:ss"), "08:14:09");
        assert_eq!(format_datetime(&parts, "H:m:s"), "8:14:9");
        // 未知 token 原样保留（不丢弃、不 panic）。
        assert_eq!(format_datetime(&parts, "YYYY QQ MM"), "2026 QQ 11");
        // 纯字面量 / 空格式。
        assert_eq!(format_datetime(&parts, "周记"), "周记");
        assert_eq!(format_datetime(&parts, ""), "");
        // 最长匹配优先：YYYY 不会被当成两个 YY。
        assert_eq!(format_datetime(&parts, "YYYYYY"), "202626");
    }

    #[test]
    fn placeholders_cover_defaults_overrides_and_unknowns() {
        let parts = parts();
        assert_eq!(
            render_placeholders("{{date}} {{time}} {{title}}", &parts, "笔记", formats()),
            "2026-11-18 08:14 笔记"
        );
        assert_eq!(
            render_placeholders("{{date:YYYY/MM}} {{time:H:mm}}", &parts, "x", formats()),
            "2026/11 8:14"
        );
        // 空格式回退默认。
        assert_eq!(
            render_placeholders("{{date:}}|{{time:}}", &parts, "x", formats()),
            "2026-11-18|08:14"
        );
        // 未知占位符/未闭合的花括号原样保留。
        assert_eq!(
            render_placeholders("a {{foo}} b {{date", &parts, "x", formats()),
            "a {{foo}} b {{date"
        );
        assert_eq!(
            render_placeholders("{{ Date }} {{TITLE}}", &parts, "x", formats()),
            "{{ Date }} {{TITLE}}"
        );
        // 多个占位符、正文前后缀。
        assert_eq!(
            render_placeholders(
                "# {{title}}\n\n日期 {{date:YYYY年M月D日}}\n",
                &parts,
                "周记",
                formats()
            ),
            "# 周记\n\n日期 2026年11月18日\n"
        );
    }

    // ---- 相对日期（P1）：解析 / 偏移 / 渲染 / 非法输入 ----

    fn date_parts(year: i32, month: u32, day: u32) -> DateTimeParts {
        DateTimeParts {
            year,
            month,
            day,
            hour: 8,
            minute: 14,
            second: 9,
        }
    }

    #[test]
    fn relative_offsets_parse_strictly() {
        let offset =
            |text: &str| RelativeDateOffset::parse(text).map(|offset| (offset.amount, offset.unit));
        assert_eq!(offset("-1d"), Some((-1, RelativeDateUnit::Day)));
        assert_eq!(offset("+1d"), Some((1, RelativeDateUnit::Day)));
        assert_eq!(offset("1d"), Some((1, RelativeDateUnit::Day)));
        assert_eq!(offset("-2w"), Some((-2, RelativeDateUnit::Week)));
        assert_eq!(offset("-1m"), Some((-1, RelativeDateUnit::Month)));
        assert_eq!(offset("+1y"), Some((1, RelativeDateUnit::Year)));
        assert_eq!(offset("0d"), Some((0, RelativeDateUnit::Day)));
        assert_eq!(offset("01d"), Some((1, RelativeDateUnit::Day)));
        assert_eq!(offset("-99y"), Some((-99, RelativeDateUnit::Year)));

        // 非法：空、缺数字、多余字符、3 位、大写单位、空白、小数、重复符号。
        for invalid in [
            "", "+", "-", "d", "+d", "1x", "1D", "123d", "1dd", "1.5d", " 1d", "1d ", "--1d",
            "++1d", "+-1d", "1 d", "1W", "١d",
        ] {
            assert_eq!(offset(invalid), None, "必须拒绝非法偏移：{invalid:?}");
        }

        // 「偏移尝试」判定：符号/数字开头；格式串（YYYY）不是。
        assert!(RelativeDateOffset::looks_like_offset("+1d"));
        assert!(RelativeDateOffset::looks_like_offset("-1d"));
        assert!(RelativeDateOffset::looks_like_offset("1x"));
        assert!(!RelativeDateOffset::looks_like_offset("YYYY-MM-DD"));
        assert!(!RelativeDateOffset::looks_like_offset("周记"));
        assert!(!RelativeDateOffset::looks_like_offset(""));
    }

    #[test]
    fn relative_offsets_shift_across_months_years_and_leap_days() {
        let parts = date_parts(2026, 11, 18);
        let offset = |text: &str, parts: &DateTimeParts| {
            RelativeDateOffset::parse(text)
                .expect("偏移必须合法")
                .apply(parts)
        };
        // 天/周：跨月、跨年、闰日；时间分量保持不变。
        assert_eq!(
            offset("-1d", &parts),
            Some(date_parts(2026, 11, 17)),
            "昨天"
        );
        assert_eq!(offset("+1d", &parts), Some(date_parts(2026, 11, 19)));
        assert_eq!(offset("+1w", &parts), Some(date_parts(2026, 11, 25)));
        assert_eq!(
            offset("+1d", &date_parts(2026, 12, 31)),
            Some(date_parts(2027, 1, 1)),
            "跨年"
        );
        assert_eq!(
            offset("-1d", &date_parts(2027, 1, 1)),
            Some(date_parts(2026, 12, 31)),
            "跨年（反向）"
        );
        assert_eq!(
            offset("+1d", &date_parts(2024, 2, 29)),
            Some(date_parts(2024, 3, 1)),
            "闰日 +1d"
        );
        assert_eq!(
            offset("-1d", &date_parts(2024, 3, 1)),
            Some(date_parts(2024, 2, 29)),
            "闰日 -1d"
        );
        // 月：月末对齐（1/31 +1m → 2/29；平年 → 2/28）；-1m。
        assert_eq!(
            offset("-1m", &parts),
            Some(date_parts(2026, 10, 18)),
            "上个月同一天"
        );
        assert_eq!(
            offset("+1m", &date_parts(2024, 1, 31)),
            Some(date_parts(2024, 2, 29)),
            "闰年月末对齐"
        );
        assert_eq!(
            offset("+1m", &date_parts(2023, 1, 31)),
            Some(date_parts(2023, 2, 28)),
            "平年月末对齐"
        );
        // 年：闰日 2/29 落到 2/28；普通年份同日。
        assert_eq!(offset("+1y", &parts), Some(date_parts(2027, 11, 18)));
        assert_eq!(
            offset("+1y", &date_parts(2024, 2, 29)),
            Some(date_parts(2025, 2, 28)),
            "闰日 +1y"
        );
        assert_eq!(
            offset("-1y", &date_parts(2024, 2, 29)),
            Some(date_parts(2023, 2, 28)),
            "闰日 -1y"
        );
        // 越界/非法日期 → None（不 panic）。
        assert_eq!(
            offset(
                "+1d",
                &DateTimeParts {
                    year: i32::MAX,
                    ..parts
                }
            ),
            None
        );
        assert_eq!(offset("+1d", &DateTimeParts { month: 13, ..parts }), None);
    }

    #[test]
    fn relative_date_placeholders_render_default_and_custom_formats() {
        let parts = parts();
        // 默认格式（`YYYY-MM-DD`）。
        assert_eq!(
            render_placeholders("{{date:-1d}}", &parts, "x", formats()),
            "2026-11-17"
        );
        assert_eq!(
            render_placeholders("{{date:+1d}}", &parts, "x", formats()),
            "2026-11-19"
        );
        assert_eq!(
            render_placeholders("{{date:+1w}}", &parts, "x", formats()),
            "2026-11-25"
        );
        assert_eq!(
            render_placeholders("{{date:-1m}}", &parts, "x", formats()),
            "2026-10-18"
        );
        assert_eq!(
            render_placeholders("{{date:+1y}}", &parts, "x", formats()),
            "2027-11-18"
        );
        // 带格式：偏移后的日期按自定义格式渲染（含中文/斜杠/时间 token）。
        assert_eq!(
            render_placeholders("{{date:-1d:YYYY/MM/DD}}", &parts, "x", formats()),
            "2026/11/17"
        );
        assert_eq!(
            render_placeholders("{{date:-1m:YYYY年M月D日}}", &parts, "x", formats()),
            "2026年10月18日"
        );
        assert_eq!(
            render_placeholders("{{date:+1d:HH:mm}}", &parts, "x", formats()),
            "08:14",
            "日偏移不改时间分量"
        );
        // 空格式段回退默认；无符号/0 偏移与现有语义一致。
        assert_eq!(
            render_placeholders("{{date:-1d:}}", &parts, "x", formats()),
            "2026-11-17"
        );
        assert_eq!(
            render_placeholders("{{date:0d}} {{date:2w}}", &parts, "x", formats()),
            "2026-11-18 2026-12-02"
        );
        // 与现有占位符混用：date/time/title 与相对日期互不影响。
        assert_eq!(
            render_placeholders(
                "{{title}}|{{date}}|{{time}}|{{date:-1d}}|{{date:+1d:YYYY/MM/DD}}",
                &parts,
                "周记",
                formats()
            ),
            "周记|2026-11-18|08:14|2026-11-17|2026/11/19"
        );
        // 旧格式串兼容：冒号是字面量，不是偏移。
        assert_eq!(
            render_placeholders("{{date:YYYY:MM}} {{time:HH:mm}}", &parts, "x", formats()),
            "2026:11 08:14"
        );
    }

    #[test]
    fn invalid_relative_offsets_are_preserved_verbatim() {
        let parts = parts();
        let preserved = |text: &str| {
            assert_eq!(
                render_placeholders(text, &parts, "x", formats()),
                text,
                "非法/未知偏移必须原样保留：{text:?}"
            );
        };
        // 空偏移（多个冒号）/ 缺数字 / 非数字 / 大写 / 3 位 / 多余字符。
        preserved("{{date::YYYY}}");
        preserved("{{date::}}");
        preserved("{{date:+d}}");
        preserved("{{date:1x}}");
        preserved("{{date:1x:YYYY}}");
        preserved("{{date:-1D}}");
        preserved("{{date:123d}}");
        preserved("{{date:1.5d}}");
        preserved("{{date:++1d}}");
        preserved("{{date:2026}}");
        // 偏移越界（年份分量超出公历可表示范围）→ 原样保留。
        let extreme = DateTimeParts {
            year: i32::MAX,
            month: 12,
            day: 31,
            ..parts
        };
        assert_eq!(
            render_placeholders("{{date:+1d}}", &extreme, "x", formats()),
            "{{date:+1d}}"
        );
        // 空格式仍是旧语义（回退默认），不是「非法空偏移」。
        assert_eq!(
            render_placeholders("{{date:}}", &parts, "x", formats()),
            "2026-11-18"
        );
    }

    fn entry(
        node_id: &str,
        title: &str,
        parent: Option<&str>,
        kind: NodeKind,
        note_seq: usize,
    ) -> TreeEntry {
        TreeEntry {
            node_id: node_id.to_owned(),
            note_id: (kind == NodeKind::Note).then(|| format!("note-{note_seq}")),
            body_doc_id: (kind == NodeKind::Note).then(|| format!("note-{note_seq}-body")),
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
            entry("t-templates", "模板", None, NodeKind::Folder, 0),
            entry("t-t1", "日记模板", Some("t-templates"), NodeKind::Note, 1),
            entry("t-sub", "会议", Some("t-templates"), NodeKind::Folder, 0),
            entry("t-t2", "周记", Some("t-sub"), NodeKind::Note, 2),
            entry("t-t3", "同名", Some("t-sub"), NodeKind::Note, 3),
            entry("t-t4", "同名", Some("t-templates"), NodeKind::Note, 4),
            entry("t-note", "普通笔记", None, NodeKind::Note, 5),
        ]
    }

    #[test]
    fn collects_templates_recursively_and_keeps_relative_paths() {
        let list = collect_templates(&sample_tree(), "模板");
        assert!(list.folder_exists);
        let paths: Vec<&str> = list
            .items
            .iter()
            .map(|item| item.relative_path.as_str())
            .collect();
        // 排序是路径字符串的字典序（Unicode 码位，不是拼音）。
        assert_eq!(paths, vec!["会议/同名", "会议/周记", "同名", "日记模板"]);
        assert!(
            list.items
                .iter()
                .all(|item| item.relative_path != "普通笔记"),
            "模板文件夹外的笔记不能被当成模板"
        );

        // 多级文件夹 + 尾斜杠/空白规整。
        let nested = collect_templates(&sample_tree(), " /模板/会议/ ");
        assert_eq!(
            nested
                .items
                .iter()
                .map(|item| item.relative_path.as_str())
                .collect::<Vec<_>>(),
            vec!["同名", "周记"]
        );

        // 空文件夹 / 不存在的文件夹 / 未配置文件夹。
        let empty = collect_templates(&sample_tree(), "空目录");
        assert!(!empty.folder_exists && empty.items.is_empty());
        let blank = collect_templates(&sample_tree(), "   ");
        assert!(blank.folder.is_empty() && !blank.folder_exists);
    }

    #[test]
    fn node_paths_and_lookup_by_path() {
        let tree = sample_tree();
        let paths = node_paths(&tree);
        assert_eq!(paths["t-t2"], "模板/会议/周记");
        assert_eq!(note_index_by_path(&tree, "模板/会议/周记"), Some(3));
        assert_eq!(note_index_by_path(&tree, "普通笔记"), Some(6));
        assert_eq!(note_index_by_path(&tree, "模板/会议"), None, "目录不是笔记");
        // 目录也能按路径找到（日记要复用/创建目录）。
        assert_eq!(node_index_by_path(&tree, "模板/会议"), Some(2));
    }

    #[test]
    fn preferences_json_round_trips_and_rejects_corruption() {
        let preferences = TemplatesPreferences {
            enabled: false,
            folder: "我的/模板".to_owned(),
            date_format: "YYYY.MM.DD".to_owned(),
            time_format: "H:m".to_owned(),
        };
        let json = preferences.to_json();
        assert_eq!(TemplatesPreferences::parse(&json), preferences);
        assert!(json.contains("\"enabled\": false"));
        assert!(json.contains("\"folder\": \"我的/模板\""));

        // 缺字段 → 按字段回退默认。
        assert_eq!(
            TemplatesPreferences::parse("{\"version\": 1}"),
            TemplatesPreferences::default()
        );
        assert_eq!(
            TemplatesPreferences::parse("{\"version\": 1, \"folder\": \"x\"}"),
            TemplatesPreferences {
                folder: "x".to_owned(),
                ..TemplatesPreferences::default()
            }
        );

        for corrupt in [
            "",
            "{ broken",
            "[]",
            "{\"version\": 99, \"folder\": \"x\"}",
            "{\"folder\": \"x\"}",
        ] {
            assert_eq!(
                TemplatesPreferences::parse(corrupt),
                TemplatesPreferences::default(),
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
        let path = root.join(TEMPLATES_FILE_NAME);

        let expected = TemplatesPreferences {
            enabled: false,
            folder: "随手/模板".to_owned(),
            date_format: "YY-M-D".to_owned(),
            time_format: "H:m".to_owned(),
        };
        save_preferences(&path, &expected).expect("写偏好失败");
        guard.init(path.clone());
        assert_eq!(preferences(), expected, "重启后必须恢复盘上的偏好");

        // 盘上文件损坏 → 回退默认（不报错、不阻塞）。
        std::fs::write(&path, b"{ broken").expect("写坏文件失败");
        guard.init(path.clone());
        assert_eq!(preferences(), TemplatesPreferences::default());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn preferences_file_round_trip_and_corrupt_file_falls_back() {
        // 独立子目录：同一测试二进制里的多个用例并行，不能删共享父目录。
        let root = test_root().join("roundtrip");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("创建 target/tmp 失败");
        let path = root.join(TEMPLATES_FILE_NAME);

        assert_eq!(load_preferences(&path), TemplatesPreferences::default());

        let preferences = TemplatesPreferences {
            enabled: true,
            folder: "模板/日记".to_owned(),
            date_format: "YY-MM-DD".to_owned(),
            time_format: "HH:mm:ss".to_owned(),
        };
        save_preferences(&path, &preferences).expect("写偏好失败");
        assert_eq!(load_preferences(&path), preferences);
        assert!(
            !path.with_extension("json.tmp").exists(),
            "原子写完成后不能留下临时文件"
        );

        std::fs::write(&path, b"{ broken").expect("写坏文件失败");
        assert_eq!(load_preferences(&path), TemplatesPreferences::default());

        let _ = std::fs::remove_dir_all(&root);
    }
}
