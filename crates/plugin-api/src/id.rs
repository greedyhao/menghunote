//! 插件 id：格式校验与类型封装（ADR 0020 §3.3 / §10.2）。
//!
//! 规则刻意保持最小、可测试、与 UI schema 无关：
//!
//! - 只允许 **ASCII 小写字母、数字、`.`、`-`**；
//! - 长度 **1–64 字节**（允许的字符全为 ASCII，因此字节数等于字符数）；
//! - 不能为空；
//! - 首/尾不能是分隔符（`.` 或 `-`）；
//! - 本切片**不**额外禁止连续分隔符，也不要求分段非空——避免在插件 API
//!   冻结前引入未经评审的约束。
//!
//! 校验失败返回 [`PluginIdError`]，[`PluginId::parse`] 不 panic。manifest 的
//! 其它 id（命令/面板/设置项）目前只要求非空且同一集合内唯一，见
//! [`crate::manifest`]。

use std::fmt;
use std::str::FromStr;

/// 插件 id 的最大长度（字节；允许字符全为 ASCII）。
pub const PLUGIN_ID_MAX_LEN: usize = 64;

/// 已校验的插件 id。
///
/// 字段私有：唯一的构造入口是 [`PluginId::parse`]（或 `str::parse`），
/// 因此拿到本类型的值就意味着格式已通过校验，不需要在业务路径重复检查。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PluginId(String);

impl PluginId {
    /// 校验并构造插件 id。
    ///
    /// # Errors
    ///
    /// 返回 [`PluginIdError`] 的第一条命中规则（空 → 超长 → 非法字符 →
    /// 首/尾分隔符），不 panic。
    pub fn parse(raw: &str) -> Result<Self, PluginIdError> {
        if raw.is_empty() {
            return Err(PluginIdError::Empty);
        }
        if raw.len() > PLUGIN_ID_MAX_LEN {
            return Err(PluginIdError::TooLong { len: raw.len() });
        }
        for character in raw.chars() {
            if !matches!(character, 'a'..='z' | '0'..='9' | '.' | '-') {
                return Err(PluginIdError::InvalidCharacter { character });
            }
        }
        if raw.starts_with('.') || raw.starts_with('-') {
            return Err(PluginIdError::LeadingSeparator);
        }
        if raw.ends_with('.') || raw.ends_with('-') {
            return Err(PluginIdError::TrailingSeparator);
        }
        Ok(Self(raw.to_owned()))
    }

    /// 借用原始字符串。
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PluginId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl AsRef<str> for PluginId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl FromStr for PluginId {
    type Err = PluginIdError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        Self::parse(raw)
    }
}

/// 插件 id 校验失败的原因（可读、可匹配、无 panic）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PluginIdError {
    /// 空字符串。
    Empty,
    /// 超过 [`PLUGIN_ID_MAX_LEN`] 字节。
    TooLong {
        /// 实际字节长度。
        len: usize,
    },
    /// 出现不允许的字符（只允许 ASCII 小写字母/数字/`.`/`-`）。
    InvalidCharacter {
        /// 命中的第一个非法字符。
        character: char,
    },
    /// 以 `.` 或 `-` 开头。
    LeadingSeparator,
    /// 以 `.` 或 `-` 结尾。
    TrailingSeparator,
}

impl fmt::Display for PluginIdError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("插件 id 不能为空"),
            Self::TooLong { len } => write!(
                formatter,
                "插件 id 过长（{len} 字节，最多 {PLUGIN_ID_MAX_LEN} 字节）"
            ),
            Self::InvalidCharacter { character } => write!(
                formatter,
                "插件 id 含非法字符 {character:?}（只允许小写字母、数字、'.'、'-'）"
            ),
            Self::LeadingSeparator => formatter.write_str("插件 id 不能以 '.' 或 '-' 开头"),
            Self::TrailingSeparator => formatter.write_str("插件 id 不能以 '.' 或 '-' 结尾"),
        }
    }
}

impl std::error::Error for PluginIdError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_minimal_and_separator_forms() {
        for raw in [
            "a",
            "abc",
            "a1",
            "1a",
            "a.b",
            "a-b",
            "a.b-c.d",
            "notes-export",
            "com.example.notes",
        ] {
            let id = PluginId::parse(raw).unwrap_or_else(|error| panic!("{raw:?} 应合法: {error}"));
            assert_eq!(id.as_str(), raw);
            assert_eq!(id.to_string(), raw);
            assert_eq!(raw.parse::<PluginId>(), Ok(id));
        }

        let max = "a".repeat(PLUGIN_ID_MAX_LEN);
        assert!(PluginId::parse(&max).is_ok(), "64 字节必须允许");
    }

    #[test]
    fn rejects_empty_long_and_invalid_characters() {
        assert_eq!(PluginId::parse(""), Err(PluginIdError::Empty));
        assert_eq!(
            PluginId::parse(&"a".repeat(PLUGIN_ID_MAX_LEN + 1)),
            Err(PluginIdError::TooLong { len: 65 })
        );
        for (raw, character) in [
            ("Note", 'N'),
            ("my_plugin", '_'),
            ("my plugin", ' '),
            ("my/plugin", '/'),
            ("插件", '插'),
            ("a😀b", '😀'),
        ] {
            assert_eq!(
                PluginId::parse(raw),
                Err(PluginIdError::InvalidCharacter { character }),
                "{raw:?}"
            );
        }
    }

    #[test]
    fn rejects_leading_and_trailing_separators() {
        for raw in [".a", "-a", ".notes", "-notes"] {
            assert_eq!(
                PluginId::parse(raw),
                Err(PluginIdError::LeadingSeparator),
                "{raw:?}"
            );
        }
        for raw in ["a.", "a-", "notes.", "notes-"] {
            assert_eq!(
                PluginId::parse(raw),
                Err(PluginIdError::TrailingSeparator),
                "{raw:?}"
            );
        }
        // 本切片不额外禁止连续分隔符；显式锁定事实，避免以后静默收紧。
        assert!(PluginId::parse("a..b").is_ok());
        assert!(PluginId::parse("a--b").is_ok());
    }

    #[test]
    fn errors_are_readable_and_implement_error() {
        for error in [
            PluginIdError::Empty,
            PluginIdError::TooLong { len: 65 },
            PluginIdError::InvalidCharacter { character: 'X' },
            PluginIdError::LeadingSeparator,
            PluginIdError::TrailingSeparator,
        ] {
            let text = error.to_string();
            assert!(!text.is_empty(), "{error:?}");
            let _: &dyn std::error::Error = &error;
        }
    }
}
