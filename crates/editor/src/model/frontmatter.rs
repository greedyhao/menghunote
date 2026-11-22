//! 笔记开头的 YAML frontmatter（Obsidian 风格属性面板的纯模型）。
//!
//! 只解析一个**保守子集**：文档第 0 行必须是 `---`，直到下一行 `---` / `...`
//! 结束；属性行按第一个 `:` 切成 key/value。数组、引号、日期等都只当字符串保存，
//! 面板编辑时原样写回，不做 YAML 重排/类型推断。
//!
//! 这里不依赖 GPUI，也不改正文：只给出 span 与属性列表，供 `MarkdownDocument`
//! 隐藏源码、app 画属性面板、以及把面板编辑写回 source 使用。

use std::ops::Range;

/// 打开/关闭分隔符。
pub const DELIMITER: &str = "---";

/// 一条 `key: value` 属性。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrontmatterProperty {
    /// 去掉首尾空白后的 key。
    pub key: String,
    /// 去掉首尾空白后的 value（保留引号/数组等原始写法）。
    pub value: String,
    /// 整行（含行尾换行）。
    pub line: Range<usize>,
    /// key 在 source 里的字节范围。
    pub key_range: Range<usize>,
    /// value 在 source 里的字节范围（空值 = 冒号后的空区间）。
    pub value_range: Range<usize>,
}

/// 文档开头的 frontmatter 块。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frontmatter {
    /// 从开头 `---` 到关闭行（含其行尾换行）。
    pub span: Range<usize>,
    /// 关闭分隔符所在行起点。
    pub closing_start: usize,
    /// 关闭行之后正文的起点。
    pub body_start: usize,
    /// 属性（按文档顺序；空行/注释行被跳过）。
    pub properties: Vec<FrontmatterProperty>,
}

impl Frontmatter {
    /// 空值（`[]` / `null` / `~` / 空串）在面板里显示为「没有值」。
    #[must_use]
    pub fn is_empty_value(value: &str) -> bool {
        let value = value.trim();
        value.is_empty() || value == "[]" || value == "null" || value == "~"
    }

    /// 生成一份新的 frontmatter（保留正文原样）。
    ///
    /// 只用于面板编辑：会按当前属性列表重排；手工写在 frontmatter 里的注释/空行
    /// 不保留（编辑属性本来就是“重写这段元数据”）。
    #[must_use]
    pub fn render(&self, source: &str, properties: &[FrontmatterProperty]) -> String {
        let body = &source[self.body_start..];
        let mut out = String::from("---\n");
        for property in properties {
            out.push_str(property.key.trim());
            out.push_str(": ");
            out.push_str(property.value.trim());
            out.push('\n');
        }
        out.push_str("---\n");
        out.push_str(body);
        out
    }

    /// 用新的属性列表重写整篇源码。
    #[must_use]
    pub fn with_properties(&self, source: &str, properties: &[FrontmatterProperty]) -> String {
        self.render(source, properties)
    }
}

/// 解析文档开头的 frontmatter；没有闭合行/不在第 0 行时返回 `None`。
#[must_use]
pub fn parse(source: &str) -> Option<Frontmatter> {
    let first_end = line_end(source, 0)?;
    let first = source.get(0..first_end)?;
    if first.trim_end_matches([' ', '\t', '\r']) != DELIMITER {
        return None;
    }
    let after_open = next_line_start(source, first_end)?;

    let mut cursor = after_open;
    let mut closing = None;
    while cursor < source.len() {
        let end = line_end(source, cursor)?;
        let line = source.get(cursor..end)?;
        let trimmed = line.trim_end_matches([' ', '\t', '\r']);
        if trimmed == DELIMITER || trimmed == "..." {
            closing = Some((cursor, end));
            break;
        }
        cursor = next_line_start(source, end)?;
    }
    let (closing_start, closing_end) = closing?;
    let body_start = next_line_start(source, closing_end).unwrap_or(closing_end);
    let span = 0..body_start;

    let mut properties = Vec::new();
    let mut line_start = after_open;
    while line_start < closing_start {
        let end = line_end(source, line_start)?;
        let line = source.get(line_start..end)?;
        let content = line.trim_end_matches([' ', '\t', '\r']);
        let trimmed = content.trim_start();
        if !trimmed.is_empty() && !trimmed.starts_with('#') {
            if let Some(colon) = trimmed.find(':') {
                let key = trimmed[..colon].trim();
                if !key.is_empty() {
                    let line_trim_start = line.len() - line.trim_start().len();
                    let key_start = line_start + line_trim_start;
                    let value_begin = line_start + line_trim_start + colon + 1;
                    let value_line_end = line_start + content.len();
                    let value_slice = source.get(value_begin..value_line_end)?;
                    let raw_value = value_slice.trim();
                    let value_start =
                        value_begin + (value_slice.len() - value_slice.trim_start().len());
                    let value_end = value_start + raw_value.len();
                    properties.push(FrontmatterProperty {
                        key: key.to_owned(),
                        value: raw_value.to_owned(),
                        line: line_start..next_line_start(source, end).unwrap_or(end),
                        key_range: key_start..key_start + key.len(),
                        value_range: value_start..value_end,
                    });
                }
            }
        }
        line_start = next_line_start(source, end)?;
    }

    Some(Frontmatter {
        span,
        closing_start,
        body_start,
        properties,
    })
}

fn line_end(source: &str, start: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut index = start;
    while index < bytes.len() && bytes[index] != b'\n' {
        index += 1;
    }
    Some(index)
}

fn next_line_start(source: &str, line_end: usize) -> Option<usize> {
    if line_end < source.len() && source.as_bytes()[line_end] == b'\n' {
        Some(line_end + 1)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{parse, Frontmatter};

    const SAMPLE: &str = "---\ntags: []\ndate_created: 2025-08-10T22:23:41\n---\n正文\n第二行";

    #[test]
    fn parses_obsidian_style_properties_and_body_start() {
        let frontmatter = parse(SAMPLE).expect("frontmatter");
        assert_eq!(frontmatter.properties.len(), 2);
        assert_eq!(frontmatter.properties[0].key, "tags");
        assert!(Frontmatter::is_empty_value(
            &frontmatter.properties[0].value
        ));
        assert_eq!(frontmatter.properties[1].key, "date_created");
        assert_eq!(frontmatter.properties[1].value, "2025-08-10T22:23:41");
        assert_eq!(frontmatter.body_start, SAMPLE.find("正文").unwrap());
        assert_eq!(&SAMPLE[frontmatter.body_start..], "正文\n第二行");
    }

    #[test]
    fn no_frontmatter_without_closing_line() {
        assert!(parse("---\ntags: []\n").is_none());
        assert!(parse("正文\n---\ntags: []\n---\n").is_none());
        assert!(parse("").is_none());
    }

    #[test]
    fn render_replaces_only_the_frontmatter() {
        let frontmatter = parse(SAMPLE).expect("frontmatter");
        let properties = frontmatter.properties.clone();
        let rendered = frontmatter.with_properties(SAMPLE, &properties);
        assert_eq!(rendered, SAMPLE);
    }

    #[test]
    fn empty_property_value_after_a_line_still_parses() {
        assert!(
            parse("---\ntags: []\nproperty: \n---\n正文").is_some(),
            "empty property line must not break frontmatter detection"
        );
    }

    #[test]
    fn empty_value_and_comments_are_tolerated() {
        let frontmatter = parse("---\n# comment\nkey:\n---\nbody").expect("frontmatter");
        assert_eq!(frontmatter.properties.len(), 1);
        assert_eq!(frontmatter.properties[0].key, "key");
        assert!(frontmatter.properties[0].value.is_empty());
    }
}
