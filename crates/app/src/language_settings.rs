//! 设置页的「语言」选择与本机 `language.json` 偏好（ADR 0020 P1）。
//!
//! 中文 / English / 跟随系统三选一；点击**立即生效**（更新进程级语言 +
//! `refresh_windows()`），不重建编辑器、不丢选区/滚动/撤销栈、不产生
//! `EditRequest`。偏好只写本机 `language.json`，不同步、不写正文、不写进 vault。
//!
//! ## 文件格式
//!
//! ```json
//! {
//!   "version": 1,
//!   "locale": "system" | "zh-CN" | "en"
//! }
//! ```
//!
//! **只存 locale**（这里的 `locale` 是"语言选择"，`system` 表示跟随系统）；
//! 解析后的输出语言不落盘。文件缺失/损坏/未知版本/未知值一律回退 `system`，
//! 不阻塞启动。原子写：同目录临时文件 → rename，失败退化为直接覆盖。
//!
//! 平台配置目录与主题一致（[`settings_path`]）：
//!
//! 1. `MENGHUNOTE_LANGUAGE_PATH`（测试/便携显式覆盖）；
//! 2. Windows：`%LOCALAPPDATA%\menghunote\language.json`；
//! 3. macOS：`~/Library/Application Support/menghunote/language.json`；
//! 4. 其它：`$XDG_CONFIG_HOME/menghunote/language.json`（缺省 `~/.config`）。
//!
//! ## 组件 selector（headless 测试点击/断言用）
//!
//! | selector | 含义 |
//! | --- | --- |
//! | `settings-language` | 语言区根 |
//! | `settings-language-zh-CN` / `settings-language-en` / `settings-language-system` | 三个模式按钮 |
//! | `settings-language-current` | 当前输出语言（+ 是否跟随系统） |

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

use gpui::{
    div, px, App, InteractiveElement as _, IntoElement, MouseButton, ParentElement as _,
    SharedString, Styled as _,
};
use menghunote_i18n::{msg, t, t_with, Locale, LocaleMode, LocaleTestGuard};

use crate::editor::theme;

/// 偏好文件当前版本。
pub const LANGUAGE_PREFERENCES_VERSION: u32 = 1;

/// 偏好文件名（平台配置目录下）。
pub const LANGUAGE_FILE_NAME: &str = "language.json";

/// 测试/便携用的路径覆盖环境变量。
pub const LANGUAGE_PATH_ENV: &str = "MENGHUNOTE_LANGUAGE_PATH";

// ---------------------------------------------------------------------------
// 偏好模型与持久化
// ---------------------------------------------------------------------------

/// 本机语言偏好（只含语言选择，不含解析结果）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LanguagePreferences {
    /// 用户选择：固定中文 / 固定英文 / 跟随系统。
    pub locale: LocaleMode,
}

impl LanguagePreferences {
    /// 从 JSON 文本解析；损坏/未知版本/未知语言一律回退默认（`System`）。
    ///
    /// 手写解析（不把 `serde_json` 拉进默认构建）：格式只有一个字段，容忍
    /// 未知字段与空白，不执行任何代码。
    #[must_use]
    pub fn parse(text: &str) -> Self {
        let Some(version) = json_number_field(text, "version") else {
            return Self::default();
        };
        if version != LANGUAGE_PREFERENCES_VERSION {
            return Self::default();
        }
        let locale = json_string_field(text, "locale")
            .and_then(|value| LocaleMode::parse(&value))
            .unwrap_or_default();
        Self { locale }
    }

    /// 序列化为稳定 JSON（写入前先落临时文件再 rename）。
    #[must_use]
    pub fn to_json(self) -> String {
        format!(
            "{{\n  \"version\": {LANGUAGE_PREFERENCES_VERSION},\n  \"locale\": \"{}\"\n}}\n",
            self.locale.as_str()
        )
    }
}

/// 平台配置目录下的语言偏好文件路径；解析不到（没有 HOME/LOCALAPPDATA）时返回 `None`。
#[must_use]
pub fn settings_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(LANGUAGE_PATH_ENV) {
        return Some(PathBuf::from(path));
    }
    #[cfg(target_os = "windows")]
    {
        std::env::var_os("LOCALAPPDATA").map(|base| {
            PathBuf::from(base)
                .join("menghunote")
                .join(LANGUAGE_FILE_NAME)
        })
    }
    #[cfg(target_os = "macos")]
    {
        std::env::var_os("HOME").map(|home| {
            PathBuf::from(home)
                .join("Library/Application Support/menghunote")
                .join(LANGUAGE_FILE_NAME)
        })
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
        Some(base.join("menghunote").join(LANGUAGE_FILE_NAME))
    }
}

/// 读取偏好；文件不存在/不可读/损坏都回退默认（**不**报错、不阻塞启动）。
#[must_use]
pub fn load_preferences(path: &Path) -> LanguagePreferences {
    let Ok(text) = std::fs::read_to_string(path) else {
        return LanguagePreferences::default();
    };
    LanguagePreferences::parse(&text)
}

/// 原子写入偏好：同目录临时文件 → rename；失败时旧文件原样保留。
pub fn save_preferences(path: &Path, preferences: LanguagePreferences) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = preferences.to_json();
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

// ---------------------------------------------------------------------------
// 运行时状态（偏好文件路径）
// ---------------------------------------------------------------------------

/// 语言偏好文件路径（`None` = 不持久化；测试/未初始化）。
///
/// 当前语言本身在 `menghunote_i18n` 的进程级状态里；本模块只额外记住写盘目标，
/// 避免把文件路径塞进纯 i18n crate。
static STORE_PATH: RwLock<Option<PathBuf>> = RwLock::new(None);

fn read_store_path() -> RwLockReadGuard<'static, Option<PathBuf>> {
    STORE_PATH.read().unwrap_or_else(|error| error.into_inner())
}

fn write_store_path() -> RwLockWriteGuard<'static, Option<PathBuf>> {
    STORE_PATH
        .write()
        .unwrap_or_else(|error| error.into_inner())
}

/// 启动时从平台配置目录加载偏好（生产入口在 `main.rs` 调一次）。
///
/// 没有偏好文件时保持默认 `System`；`System` 在这里解析成系统语言
/// （`LC_ALL`/`LANG`，没有则中文）。
pub fn init_from_disk() {
    let Some(path) = settings_path() else {
        return;
    };
    init_from_path(path);
}

/// 从**显式路径**加载偏好（测试/便携入口；生产走 [`init_from_disk`]）。
///
/// 文件缺失/损坏回退 `System`，并把该路径记为后续 [`set_mode`] 的持久化目标。
#[doc(hidden)]
pub fn init_from_path(path: PathBuf) {
    let preferences = load_preferences(&path);
    *write_store_path() = Some(path);
    // 启动阶段没有窗口，只需更新进程级语言；首帧渲染自然使用新值。
    menghunote_i18n::set_mode(preferences.locale);
}

/// 设置语言模式并立即生效（持久化 + 所有窗口重绘）。
///
/// 返回 `true` 表示模式或输出语言真的变了。切换不重建编辑器、不写正文、
/// 不产生 `EditRequest`——语言只是全局 `Copy` 状态 + 重绘。
pub fn set_mode(mode: LocaleMode, cx: &mut App) -> bool {
    let changed = menghunote_i18n::set_mode(mode);
    if changed {
        persist(mode);
        cx.refresh_windows();
    }
    changed
}

fn persist(mode: LocaleMode) {
    let path = read_store_path().clone();
    let Some(path) = path else {
        return;
    };
    if let Err(error) = save_preferences(&path, LanguagePreferences { locale: mode }) {
        // 写失败只影响下次启动；当前会话仍然生效（不阻塞、不 panic）。
        eprintln!("[menghunote] 保存语言偏好失败：{error}");
    }
}

// ---------------------------------------------------------------------------
// 设置页 UI
// ---------------------------------------------------------------------------

/// 设置页的「外观 + 语言」行：两区并排（窄卡片自动换行），避免语言区被
/// 同步面板挤出可视区；两者互不依赖，走同一设置页。
#[must_use]
pub fn appearance_and_language_row() -> impl IntoElement {
    div()
        .w_full()
        .flex()
        .flex_row()
        .flex_wrap()
        .gap_3()
        .child(
            div()
                .flex_1()
                .min_w(px(240.0))
                .child(crate::theme_settings::appearance_section()),
        )
        .child(div().flex_1().min_w(px(240.0)).child(language_section()))
}

/// 语言设置区（AppShell 与 AppRoot 的设置页共用，与「外观」并排）。
#[must_use]
pub fn language_section() -> impl IntoElement {
    let mode = menghunote_i18n::mode();
    let locale = menghunote_i18n::current_locale();
    div()
        .id("settings-language")
        .debug_selector(|| "settings-language".to_string())
        .w_full()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_size(px(12.0))
                .text_color(theme::text_color())
                .child(SharedString::from(language_title())),
        )
        .child(
            div()
                .text_size(px(11.0))
                .text_color(theme::status_bar_text())
                .child(SharedString::from(language_hint())),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .gap_2()
                .child(mode_button(
                    "settings-language-zh-CN",
                    locale_label(Locale::ZhCn),
                    LocaleMode::ZhCn,
                    mode,
                ))
                .child(mode_button(
                    "settings-language-en",
                    locale_label(Locale::En),
                    LocaleMode::En,
                    mode,
                ))
                .child(mode_button(
                    "settings-language-system",
                    language_mode_label(LocaleMode::System),
                    LocaleMode::System,
                    mode,
                )),
        )
        .child(
            div()
                .debug_selector(|| "settings-language-current".to_string())
                .text_size(px(11.0))
                .text_color(theme::status_bar_text())
                .child(SharedString::from(current_line(mode, locale))),
        )
}

/// 单个语言按钮：选中态用强调色，未选中用表面色 + 边框。
fn mode_button(
    selector: &'static str,
    label: String,
    button_mode: LocaleMode,
    current: LocaleMode,
) -> impl IntoElement {
    let active = button_mode == current;
    let (background, text, border) = if active {
        (
            theme::search_toggle_active(),
            theme::search_toggle_active_text(),
            theme::search_toggle_active(),
        )
    } else {
        (theme::menu_bg(), theme::text_color(), theme::input_border())
    };
    div()
        .id(selector)
        .debug_selector(move || selector.to_string())
        .px_3()
        .py_1()
        .rounded_sm()
        .border_1()
        .border_color(border)
        .bg(background)
        .text_size(px(12.0))
        .text_color(text)
        .hover(move |style| {
            // 选中态保持强调色（白字压在浅灰 hover 上会看不清）。
            if active {
                style.bg(background)
            } else {
                style.bg(theme::sidebar_selected_bg())
            }
        })
        .child(SharedString::from(label))
        .on_mouse_down(MouseButton::Left, move |_event, _window, cx| {
            // 立即生效：只改全局语言 + 重绘，不触碰文档/选区/撤销栈。
            set_mode(button_mode, cx);
        })
}

/// 语言区标题「语言」/「Language」。
#[must_use]
pub fn language_title() -> String {
    t(msg::settings::LANGUAGE_TITLE)
}

/// 语言区提示（本机偏好/不同步/立即生效）。
#[must_use]
pub fn language_hint() -> String {
    t(msg::settings::LANGUAGE_HINT)
}

/// 语言模式的中文/英文标签（只用于展示；持久化用 [`LocaleMode::as_str`]）。
#[must_use]
pub fn language_mode_label(mode: LocaleMode) -> String {
    match mode {
        LocaleMode::ZhCn => locale_label(Locale::ZhCn),
        LocaleMode::En => locale_label(Locale::En),
        LocaleMode::System => t(msg::settings::LANGUAGE_SYSTEM),
    }
}

/// 输出语言的展示名（中文 / English；按当前语言本地化）。
#[must_use]
pub fn locale_label(locale: Locale) -> String {
    match locale {
        Locale::ZhCn => t(msg::settings::LANGUAGE_ZH),
        Locale::En => t(msg::settings::LANGUAGE_EN),
    }
}

/// 当前语言行：`当前：中文（跟随系统）` / `Current: Chinese (follow system)`。
#[must_use]
pub fn current_line(mode: LocaleMode, locale: Locale) -> String {
    let language = locale_label(locale);
    let id = if mode == LocaleMode::System {
        msg::settings::LANGUAGE_CURRENT_SYSTEM
    } else {
        msg::settings::LANGUAGE_CURRENT
    };
    t_with(id, &[("language", language.as_str())])
}

// ---------------------------------------------------------------------------
// 测试钩子
// ---------------------------------------------------------------------------

/// 语言偏好测试锁：偏好路径 + 进程级语言都是全局状态，改它们的测试必须串行。
static TEST_LOCK: Mutex<()> = Mutex::new(());

/// 语言偏好测试守卫：`reset` 固定语言并把持久化目标指向给定路径；
/// `Drop` 清空持久化目标并把语言复位到默认（`System` + 中文）。
pub struct LanguageTestGuard {
    _lock: MutexGuard<'static, ()>,
    locale: LocaleTestGuard,
}

impl LanguageTestGuard {
    /// 固定到中文（避免系统语言影响断言），并设置偏好文件路径。
    pub fn reset(&self, store_path: Option<PathBuf>) {
        self.locale.reset(Locale::ZhCn);
        *write_store_path() = store_path;
    }
}

impl Drop for LanguageTestGuard {
    fn drop(&mut self) {
        *write_store_path() = None;
    }
}

/// 串行化语言偏好测试并自动复位（只给 `crates/app/tests/**` 与 crate 内测试用）。
#[doc(hidden)]
#[must_use]
pub fn lock_for_test() -> LanguageTestGuard {
    let lock = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let locale = menghunote_i18n::lock_for_test();
    LanguageTestGuard {
        _lock: lock,
        locale,
    }
}

// ---------------------------------------------------------------------------
// 手写 JSON 解析（只支持本模块写出的扁平对象）
// ---------------------------------------------------------------------------

/// 取 `"key": "value"` 的字符串值；找不到/不是字符串返回 `None`。
fn json_string_field(text: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let start = text.find(&needle)? + needle.len();
    let rest = &text[start..];
    let rest = rest.trim_start().strip_prefix(':')?;
    let rest = rest.trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_owned())
}

/// 取 `"key": 123` 的数字值；找不到/不是数字返回 `None`。
fn json_number_field(text: &str, key: &str) -> Option<u32> {
    let needle = format!("\"{key}\"");
    let start = text.find(&needle)? + needle.len();
    let rest = &text[start..];
    let rest = rest.trim_start().strip_prefix(':')?;
    let rest = rest.trim_start();
    let end = rest
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn test_root() -> PathBuf {
        // lib 单测没有 `CARGO_TARGET_TMPDIR`（只有集成测试有），统一落仓库 target/tmp。
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/tmp")
            .join(format!("language-preferences-{}", std::process::id()))
    }

    #[test]
    fn preferences_json_round_trips_and_rejects_corruption() {
        for mode in LocaleMode::ALL {
            let preferences = LanguagePreferences { locale: mode };
            let json = preferences.to_json();
            assert!(json.contains(&format!("\"locale\": \"{}\"", mode.as_str())));
            assert_eq!(LanguagePreferences::parse(&json), preferences);
        }

        for corrupt in [
            "",
            "{ broken",
            "[]",
            "{\"version\": 99, \"locale\": \"en\"}",
            "{\"version\": 1, \"locale\": \"neon\"}",
            "{\"version\": 1}",
            "{\"locale\": \"en\"}",
        ] {
            assert_eq!(
                LanguagePreferences::parse(corrupt),
                LanguagePreferences::default(),
                "损坏输入必须回退 System：{corrupt:?}"
            );
        }

        // 未知字段被容忍。
        assert_eq!(
            LanguagePreferences::parse("{\"version\": 1, \"locale\": \"en\", \"x\": 1}"),
            LanguagePreferences {
                locale: LocaleMode::En
            }
        );
    }

    #[test]
    fn preferences_file_round_trip_and_corrupt_file_falls_back() {
        let root = test_root();
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("创建 target/tmp 失败");
        let path = root.join(LANGUAGE_FILE_NAME);

        assert_eq!(load_preferences(&path), LanguagePreferences::default());

        let preferences = LanguagePreferences {
            locale: LocaleMode::En,
        };
        save_preferences(&path, preferences).expect("写偏好失败");
        assert_eq!(load_preferences(&path), preferences);
        let raw = std::fs::read_to_string(&path).expect("读回偏好失败");
        assert!(raw.contains("\"locale\": \"en\""));
        assert!(raw.contains("\"version\": 1"));
        assert!(
            !path.with_extension("json.tmp").exists(),
            "原子写完成后不能留下临时文件"
        );

        std::fs::write(&path, b"{ broken").expect("写坏文件失败");
        assert_eq!(load_preferences(&path), LanguagePreferences::default());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn labels_exist_in_both_languages_and_are_not_concatenated() {
        let guard = lock_for_test();
        guard.reset(None);

        guard.locale.reset(Locale::ZhCn);
        assert_eq!(language_title(), "语言");
        assert_eq!(locale_label(Locale::ZhCn), "中文");
        assert_eq!(locale_label(Locale::En), "English");
        assert_eq!(language_mode_label(LocaleMode::System), "跟随系统");
        assert_eq!(
            current_line(LocaleMode::System, Locale::ZhCn),
            "当前：中文（跟随系统）"
        );

        guard.locale.reset(Locale::En);
        assert_eq!(language_title(), "Language");
        assert_eq!(locale_label(Locale::ZhCn), "Chinese");
        assert_eq!(language_mode_label(LocaleMode::En), "English");
        assert_eq!(current_line(LocaleMode::En, Locale::En), "Current: English");
    }

    #[test]
    fn init_restores_persisted_mode_and_handles_corruption() {
        let guard = lock_for_test();
        guard.reset(None);

        let root = test_root().join("init");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("创建 target/tmp 失败");
        let path = root.join(LANGUAGE_FILE_NAME);

        save_preferences(
            &path,
            LanguagePreferences {
                locale: LocaleMode::En,
            },
        )
        .expect("写偏好失败");
        init_from_path(path.clone());
        assert_eq!(menghunote_i18n::mode(), LocaleMode::En);
        assert_eq!(menghunote_i18n::current_locale(), Locale::En);

        // 改盘上文件后重新初始化：模拟重启。
        std::fs::write(&path, b"{ broken").expect("写坏文件失败");
        init_from_path(path.clone());
        assert_eq!(menghunote_i18n::mode(), LocaleMode::System);
        assert_eq!(
            menghunote_i18n::current_locale(),
            Locale::detect(),
            "损坏回退 System 后按系统语言解析"
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
