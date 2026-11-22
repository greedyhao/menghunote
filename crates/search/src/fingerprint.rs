//! 索引 fingerprint：决定"这个索引目录还能不能继续用"。
//!
//! 任何会影响倒排内容/查询结果的东西都必须进 fingerprint：
//! schema 版本、tokenizer 名、analyzer 版本、Markdown 转换版本、词典哈希。
//!
//! 改其中任一项 → `open()` 必须拒绝旧索引，由上层重建 generation
//! （generation 目录切换在后续任务实现；本 crate 只负责拒绝与报错）。

use crate::analyzer::TOKENIZER_NAME;
use crate::error::SearchError;
use crate::markdown::MARKDOWN_TO_PLAIN_VERSION;

/// 索引 schema 结构版本。字段集合/存储选项变化时 +1。
pub const SCHEMA_VERSION: u32 = 1;
/// analyzer 管线版本。分词器或过滤器顺序变化时 +1。
pub const ANALYZER_VERSION: u32 = 1;
/// Markdown 转换算法版本；与 [`crate::markdown::MARKDOWN_TO_PLAIN_VERSION`] 保持一致。
pub const MARKDOWN_VERSION: u32 = MARKDOWN_TO_PLAIN_VERSION;

/// 索引目录里的 fingerprint 文件名。
pub const FINGERPRINT_FILE: &str = "search.fingerprint";

/// 词典哈希占位。
///
/// TODO(S4-2)：cang-jie 目前只暴露默认词典，没有稳定词典摘要。
/// 先把 0 作为占位；真正接入自定义词典时，必须改成"词典文件内容/版本"的
/// 稳定哈希，并升级 [`ANALYZER_VERSION`]。
pub const DICTIONARY_HASH_PLACEHOLDER: u64 = 0;

/// 索引格式指纹。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fingerprint {
    /// schema 结构版本（字段、stored/indexed 选项变化时 +1）。
    pub schema_version: u32,
    /// 注册进 tantivy 的 tokenizer 名。
    pub tokenizer: String,
    /// analyzer 管线版本（分词/过滤顺序变化时 +1）。
    pub analyzer_version: u32,
    /// Markdown → 纯文本算法版本。
    pub markdown_version: u32,
    /// 中文词典哈希（当前为占位 0）。
    pub dictionary_hash: u64,
}

impl Fingerprint {
    /// 当前代码期望的 fingerprint。
    pub fn current() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            tokenizer: TOKENIZER_NAME.to_owned(),
            analyzer_version: ANALYZER_VERSION,
            markdown_version: MARKDOWN_VERSION,
            dictionary_hash: DICTIONARY_HASH_PLACEHOLDER,
        }
    }

    /// 序列化为可读的 `key=value` 文本（便于人工排查）。
    pub fn encode(&self) -> String {
        format!(
            "schema_version={}\ntokenizer={}\nanalyzer_version={}\nmarkdown_version={}\ndictionary_hash={:016x}\n",
            self.schema_version,
            self.tokenizer,
            self.analyzer_version,
            self.markdown_version,
            self.dictionary_hash
        )
    }

    /// 解析 [`Self::encode`] 的输出；缺字段/未知字段都报错。
    pub fn parse(text: &str) -> Result<Self, SearchError> {
        let mut schema_version = None;
        let mut tokenizer = None;
        let mut analyzer_version = None;
        let mut markdown_version = None;
        let mut dictionary_hash = None;

        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                return Err(SearchError::Fingerprint(format!("无法解析行: {line:?}")));
            };
            match key {
                "schema_version" => schema_version = Some(parse_u32(key, value)?),
                "tokenizer" => tokenizer = Some(value.to_owned()),
                "analyzer_version" => analyzer_version = Some(parse_u32(key, value)?),
                "markdown_version" => markdown_version = Some(parse_u32(key, value)?),
                "dictionary_hash" => {
                    dictionary_hash = Some(u64::from_str_radix(value, 16).map_err(|error| {
                        SearchError::Fingerprint(format!(
                            "dictionary_hash 不是十六进制 {value:?}: {error}"
                        ))
                    })?)
                }
                _ => {
                    return Err(SearchError::Fingerprint(format!(
                        "未知 fingerprint 字段: {key:?}"
                    )))
                }
            }
        }

        Ok(Self {
            schema_version: required(schema_version, "schema_version")?,
            tokenizer: required(tokenizer, "tokenizer")?,
            analyzer_version: required(analyzer_version, "analyzer_version")?,
            markdown_version: required(markdown_version, "markdown_version")?,
            dictionary_hash: required(dictionary_hash, "dictionary_hash")?,
        })
    }
}

fn parse_u32(key: &str, value: &str) -> Result<u32, SearchError> {
    value
        .parse()
        .map_err(|error| SearchError::Fingerprint(format!("{key} 不是 u32 {value:?}: {error}")))
}

fn required<T>(value: Option<T>, key: &str) -> Result<T, SearchError> {
    value.ok_or_else(|| SearchError::Fingerprint(format!("缺少字段 {key}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips() {
        let fingerprint = Fingerprint::current();
        assert_eq!(
            Fingerprint::parse(&fingerprint.encode()).expect("解析 fingerprint 失败"),
            fingerprint
        );
    }

    #[test]
    fn rejects_unknown_and_missing_fields() {
        assert!(Fingerprint::parse("schema_version=1\nbogus=x\n").is_err());
        assert!(Fingerprint::parse("schema_version=1\n").is_err());
    }
}
