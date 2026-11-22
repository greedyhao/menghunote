//! 版本类型：插件版本与 host API 版本（ADR 0020 §3.3 / §10.2）。
//!
//! 本切片刻意用两套最简单、可测试的方案：
//!
//! - [`PluginVersion`]：**`major.minor.patch`**（三段 `u32`，严格 ASCII 数字、
//!   不允许前导零、不支持预发布/构建元数据）。插件自己的版本需要给人看，
//!   三段足够表达破坏性变更/特性/修复，且不需要引入 semver crate。
//! - [`ApiVersion`]：**单个 `u16`**（host API 代）。API 兼容性由 host 精确
//!   枚举支持集合（[`SUPPORTED_API_VERSIONS`]），当前只支持 `1`；这比解析
//!   比较区间更不容易在插件 API 冻结前把兼容规则焊死。
//!
//! 两者都实现 `serde` 的字符串/整数表示；JSON 里 `version` 是字符串
//! `"1.2.3"`，`api_version` 是整数 `1`。解析失败返回可读错误，不 panic。

use std::fmt;
use std::str::FromStr;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// 插件版本：`major.minor.patch`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PluginVersion {
    /// 主版本号。
    pub major: u32,
    /// 次版本号。
    pub minor: u32,
    /// 修订号。
    pub patch: u32,
}

impl PluginVersion {
    /// 由三段数字构造。
    #[must_use]
    pub const fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }

    /// 解析 `major.minor.patch`。
    ///
    /// # Errors
    ///
    /// 段数不是 3、段为空、含非 ASCII 数字、有前导零或超出 `u32` 时返回
    /// [`PluginVersionError`]。
    pub fn parse(raw: &str) -> Result<Self, PluginVersionError> {
        let mut parts = raw.split('.');
        let major = parse_component(parts.next())?;
        let minor = parse_component(parts.next())?;
        let patch = parse_component(parts.next())?;
        if parts.next().is_some() {
            return Err(PluginVersionError::ComponentCount);
        }
        Ok(Self {
            major,
            minor,
            patch,
        })
    }
}

impl FromStr for PluginVersion {
    type Err = PluginVersionError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        Self::parse(raw)
    }
}

impl fmt::Display for PluginVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

impl Serialize for PluginVersion {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for PluginVersion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_str(PluginVersionVisitor)
    }
}

struct PluginVersionVisitor;

impl Visitor<'_> for PluginVersionVisitor {
    type Value = PluginVersion;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("版本字符串 \"major.minor.patch\"")
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        PluginVersion::parse(value).map_err(E::custom)
    }
}

/// 解析单个版本段；`None` 表示段数不足。
fn parse_component(part: Option<&str>) -> Result<u32, PluginVersionError> {
    let Some(part) = part else {
        return Err(PluginVersionError::ComponentCount);
    };
    if part.is_empty() {
        return Err(PluginVersionError::EmptyComponent);
    }
    if !part.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(PluginVersionError::NonNumeric);
    }
    if part.len() > 1 && part.starts_with('0') {
        return Err(PluginVersionError::LeadingZero);
    }
    part.parse().map_err(|_| PluginVersionError::Overflow)
}

/// 插件版本解析错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PluginVersionError {
    /// 不是恰好三段。
    ComponentCount,
    /// 某一段为空。
    EmptyComponent,
    /// 某一段含非 ASCII 数字。
    NonNumeric,
    /// 某一段有前导零（semver 不允许）。
    LeadingZero,
    /// 某一段超出 `u32`。
    Overflow,
}

impl fmt::Display for PluginVersionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ComponentCount => formatter.write_str("版本必须是 major.minor.patch 三段"),
            Self::EmptyComponent => formatter.write_str("版本段不能为空"),
            Self::NonNumeric => formatter.write_str("版本段只能是 ASCII 数字"),
            Self::LeadingZero => formatter.write_str("版本段不能有前导零"),
            Self::Overflow => formatter.write_str("版本段超出 u32 范围"),
        }
    }
}

impl std::error::Error for PluginVersionError {}

/// host 插件 API 的代号（单个 `u16`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ApiVersion(u16);

impl ApiVersion {
    /// 本 host 当前实现的 API 版本。
    pub const CURRENT: Self = Self(1);

    /// 本 host 接受的唯一 API 版本。
    ///
    /// 这是**精确集合**而不是区间：插件 API 尚未冻结，先把“只支持 1”写成
    /// 可测试的事实；将来扩展时在这里加值并补兼容测试。
    pub const SUPPORTED: &'static [Self] = &[Self::CURRENT];

    /// 由原始整数构造（不校验；校验发生在 [`crate::PluginManifest::validate`]）。
    #[must_use]
    pub const fn new(value: u16) -> Self {
        Self(value)
    }

    /// 原始整数。
    #[must_use]
    pub const fn get(self) -> u16 {
        self.0
    }
}

impl fmt::Display for ApiVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

/// host 支持的 API 版本集合（与 [`ApiVersion::SUPPORTED`] 同一份数据）。
pub const SUPPORTED_API_VERSIONS: &[ApiVersion] = ApiVersion::SUPPORTED;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_version_parses_and_formats() {
        for (raw, expected) in [
            ("0.0.0", PluginVersion::new(0, 0, 0)),
            ("1.0.0", PluginVersion::new(1, 0, 0)),
            ("1.2.3", PluginVersion::new(1, 2, 3)),
            ("10.20.30", PluginVersion::new(10, 20, 30)),
            ("4294967295.0.1", PluginVersion::new(u32::MAX, 0, 1)),
        ] {
            assert_eq!(PluginVersion::parse(raw), Ok(expected), "{raw}");
            assert_eq!(expected.to_string(), raw);
            assert_eq!(raw.parse::<PluginVersion>(), Ok(expected));
        }
    }

    #[test]
    fn plugin_version_rejects_non_semver_forms() {
        for (raw, expected) in [
            ("", PluginVersionError::EmptyComponent),
            ("1", PluginVersionError::ComponentCount),
            ("1.2", PluginVersionError::ComponentCount),
            ("1.2.3.4", PluginVersionError::ComponentCount),
            ("1..3", PluginVersionError::EmptyComponent),
            (".2.3", PluginVersionError::EmptyComponent),
            ("1.2.", PluginVersionError::EmptyComponent),
            ("1.2.x", PluginVersionError::NonNumeric),
            ("-1.2.3", PluginVersionError::NonNumeric),
            ("1.02.3", PluginVersionError::LeadingZero),
            ("1.2.003", PluginVersionError::LeadingZero),
            ("4294967296.0.0", PluginVersionError::Overflow),
        ] {
            assert_eq!(PluginVersion::parse(raw), Err(expected), "{raw}");
        }
        // 单段 "0" 是合法的零，不触发前导零规则。
        assert_eq!(
            PluginVersion::parse("0.0.0"),
            Ok(PluginVersion::new(0, 0, 0))
        );
    }

    #[test]
    fn api_version_supports_only_current_generation() {
        assert_eq!(ApiVersion::CURRENT.get(), 1);
        assert_eq!(SUPPORTED_API_VERSIONS.len(), 1);
        assert_eq!(SUPPORTED_API_VERSIONS[0], ApiVersion::new(1));
        assert!(ApiVersion::SUPPORTED.contains(&ApiVersion::CURRENT));
        assert!(!ApiVersion::SUPPORTED.contains(&ApiVersion::new(2)));
        assert_eq!(ApiVersion::new(7).to_string(), "7");
    }
}
