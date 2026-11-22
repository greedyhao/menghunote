//! 语言与语言偏好（ADR 0020 P1）。
//!
//! [`Locale`] 是**已解析**的输出语言；[`LocaleMode`] 是用户的持久化选择
//! （固定中文 / 固定英文 / 跟随系统）。两者分开，和主题的 `ThemeMode` /
//! `ThemeId` 一一对应：偏好文件只存 mode，运行时再解析。

use std::fmt;

/// 系统语言探测的两个来源（POSIX 优先级：`LC_ALL` 高于 `LANG`）。
const SYSTEM_LOCALE_ENV: [&str; 2] = ["LC_ALL", "LANG"];

/// 输出语言。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Locale {
    /// 简体中文（默认回退）。
    #[default]
    ZhCn,
    /// 英文。
    En,
}

impl Locale {
    /// 全部受支持的语言（`catalog` 一致性测试/语言选择器共用）。
    pub const ALL: [Locale; 2] = [Locale::ZhCn, Locale::En];

    /// 稳定标签（进偏好文件/日志，**不是** UI 文案）。
    #[must_use]
    pub const fn tag(self) -> &'static str {
        match self {
            Self::ZhCn => "zh-CN",
            Self::En => "en",
        }
    }

    /// 解析语言标签；未知/不支持的语言返回 `None`。
    ///
    /// 接受 `zh` / `zh-CN` / `zh_CN` / `zh-Hans` / `en` / `en-US` 等常见写法
    /// （大小写不敏感，忽略 `.UTF-8` 编码后缀与 `@variant`），只取主语言子标签。
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match primary_subtag(raw)?.as_str() {
            "zh" => Some(Self::ZhCn),
            "en" => Some(Self::En),
            _ => None,
        }
    }

    /// 按系统环境解析当前语言（`LC_ALL` 优先，其次 `LANG`）。
    ///
    /// 规则：
    /// - 空值视为未设置，继续看下一个变量；
    /// - 第一个非空值决定结果（遵循 POSIX 优先级，**不**再回看 `LANG`）；
    /// - `C` / `POSIX` / 其它不支持的语言一律回退 [`Locale::default`]（中文）；
    /// - 两个变量都没有时同样回退中文（Windows 上通常没有 `LANG`，等价于
    ///   "Windows 默认 zh-CN"）。
    #[must_use]
    pub fn detect() -> Self {
        Self::detect_with(|key| std::env::var(key).ok())
    }

    /// [`Locale::detect`] 的可注入版本（测试用；不读真实环境变量）。
    #[must_use]
    pub fn detect_with(get: impl Fn(&str) -> Option<String>) -> Self {
        for key in SYSTEM_LOCALE_ENV {
            let Some(raw) = get(key) else {
                continue;
            };
            let raw = raw.trim();
            if raw.is_empty() {
                continue;
            }
            // 第一个非空值就是结论：未知语言按产品约定回退中文，而不是继续
            // 读取低优先级的 `LANG`（避免 `LC_ALL=fr_FR` + `LANG=en_US` 时
            // 意外切英文）。
            return Self::parse_system_value(raw).unwrap_or_default();
        }
        Self::default()
    }

    /// 解析系统变量值；`C` / `POSIX` / 未知语言返回 `None`。
    fn parse_system_value(raw: &str) -> Option<Self> {
        let normalized = normalize_system_value(raw);
        match normalized.as_str() {
            "" | "c" | "posix" => None,
            _ => Self::parse(&normalized),
        }
    }
}

impl fmt::Display for Locale {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.tag())
    }
}

/// 用户选择的语言模式（持久化到本机 `language.json`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum LocaleMode {
    /// 跟随系统（默认）。
    #[default]
    System,
    /// 固定简体中文。
    ZhCn,
    /// 固定英文。
    En,
}

impl LocaleMode {
    /// 全部模式（设置页按这个顺序渲染）。
    pub const ALL: [LocaleMode; 3] = [LocaleMode::ZhCn, LocaleMode::En, LocaleMode::System];

    /// 持久化字符串（`"system"` / `"zh-CN"` / `"en"`）。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::ZhCn => "zh-CN",
            Self::En => "en",
        }
    }

    /// 解析持久化字符串；未知/损坏的值返回 `None`（调用方回退 [`Self::System`]）。
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "system" | "auto" => Some(Self::System),
            other => match Locale::parse(other) {
                Some(Locale::ZhCn) => Some(Self::ZhCn),
                Some(Locale::En) => Some(Self::En),
                None => None,
            },
        }
    }

    /// 固定的语言（`System` 返回 `None`）。
    #[must_use]
    pub const fn fixed_locale(self) -> Option<Locale> {
        match self {
            Self::System => None,
            Self::ZhCn => Some(Locale::ZhCn),
            Self::En => Some(Locale::En),
        }
    }

    /// 由固定语言构造模式。
    #[must_use]
    pub const fn from_locale(locale: Locale) -> Self {
        match locale {
            Locale::ZhCn => Self::ZhCn,
            Locale::En => Self::En,
        }
    }

    /// 解析成实际输出语言：`System` 用调用方探测到的系统语言。
    #[must_use]
    pub fn resolve(self, system: Locale) -> Locale {
        self.fixed_locale().unwrap_or(system)
    }
}

impl fmt::Display for LocaleMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// 去掉 `.UTF-8` 编码后缀、`@variant` 与首尾空白。
fn normalize_system_value(raw: &str) -> String {
    let raw = raw.trim();
    let raw = raw.split('.').next().unwrap_or(raw);
    let raw = raw.split('@').next().unwrap_or(raw);
    raw.trim().to_ascii_lowercase()
}

/// 语言标签的主语言子标签（`zh_CN.UTF-8` → `zh`）。
fn primary_subtag(raw: &str) -> Option<String> {
    let normalized = normalize_system_value(raw);
    if normalized.is_empty() {
        return None;
    }
    let primary = normalized
        .split(['-', '_'])
        .next()
        .unwrap_or(normalized.as_str());
    if primary.is_empty() {
        None
    } else {
        Some(primary.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_tags_parse_to_supported_locales() {
        for (raw, expected) in [
            ("zh", Locale::ZhCn),
            ("zh-CN", Locale::ZhCn),
            ("zh_CN", Locale::ZhCn),
            ("zh-Hans", Locale::ZhCn),
            ("ZH-cn.UTF-8", Locale::ZhCn),
            ("en", Locale::En),
            ("en-US", Locale::En),
            ("en_US.utf8", Locale::En),
            (" en-GB ", Locale::En),
        ] {
            assert_eq!(Locale::parse(raw), Some(expected), "{raw}");
        }
    }

    #[test]
    fn unsupported_or_empty_tags_are_rejected() {
        for raw in ["", "  ", "fr", "fr-FR", "C", "POSIX", "zhx", "e"] {
            assert_eq!(Locale::parse(raw), None, "{raw}");
        }
    }

    #[test]
    fn system_detection_prefers_lc_all_and_handles_unknown() {
        assert_eq!(
            Locale::detect_with(|key| match key {
                "LC_ALL" => Some("en_US.UTF-8".to_owned()),
                "LANG" => Some("zh_CN.UTF-8".to_owned()),
                _ => None,
            }),
            Locale::En,
            "LC_ALL 必须优先于 LANG"
        );
        assert_eq!(
            Locale::detect_with(|key| match key {
                "LC_ALL" => Some(String::new()),
                "LANG" => Some("en_GB.UTF-8".to_owned()),
                _ => None,
            }),
            Locale::En,
            "空的 LC_ALL 视为未设置"
        );
        assert_eq!(
            Locale::detect_with(|key| match key {
                "LC_ALL" => Some("fr_FR.UTF-8".to_owned()),
                "LANG" => Some("en_US.UTF-8".to_owned()),
                _ => None,
            }),
            Locale::ZhCn,
            "未知语言按产品约定回退中文，不继续看 LANG"
        );
        assert_eq!(
            Locale::detect_with(|_| None),
            Locale::ZhCn,
            "没有环境变量时回退中文（Windows 默认 zh-CN）"
        );
        assert_eq!(
            Locale::detect_with(|key| (key == "LANG").then(|| "C.UTF-8".to_owned())),
            Locale::ZhCn,
            "C/POSIX 回退中文"
        );
    }

    #[test]
    fn locale_modes_round_trip_and_resolve() {
        for mode in LocaleMode::ALL {
            assert_eq!(LocaleMode::parse(mode.as_str()), Some(mode));
        }
        assert_eq!(LocaleMode::parse("zh_CN"), Some(LocaleMode::ZhCn));
        assert_eq!(LocaleMode::parse("en-US"), Some(LocaleMode::En));
        assert_eq!(LocaleMode::parse("neon"), None);
        assert_eq!(LocaleMode::parse(""), None);

        assert_eq!(LocaleMode::System.resolve(Locale::En), Locale::En);
        assert_eq!(LocaleMode::ZhCn.resolve(Locale::En), Locale::ZhCn);
        assert_eq!(LocaleMode::En.resolve(Locale::ZhCn), Locale::En);
        assert_eq!(
            LocaleMode::from_locale(Locale::ZhCn).fixed_locale(),
            Some(Locale::ZhCn)
        );
        assert_eq!(LocaleMode::System.fixed_locale(), None);
    }
}
