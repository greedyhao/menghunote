//! 固定 analyzer 管线。
//!
//! 顺序是硬性的（探针 task-55 / ADR 0005 §4）：
//! 1. `CangJieTokenizer { ForSearch }`：中文按词切分；
//! 2. [`DropWhitespaceFilter`]：丢掉 cang-jie 产生的纯空白 token
//!    （原样会把 `" "` 写进词表，污染 BM25 长度与短语位置）；
//! 3. `LowerCaser`：英文统一小写，`gpui` 才能命中 `GPUI`。
//!
//! 索引与查询必须共用同一个 `TextAnalyzer`（[`build`]），否则两边 token 形态不一致。

use std::sync::Arc;

use cang_jie::jieba_rs::Jieba;
use cang_jie::{CangJieTokenizer, TokenizerOption};
use tantivy::tokenizer::{LowerCaser, TextAnalyzer, Token, TokenFilter, TokenStream, Tokenizer};

/// 注册进 tantivy `TokenizerManager` 的名字。
/// 改名/改管线时必须同时升级 [`crate::fingerprint::ANALYZER_VERSION`]。
pub const TOKENIZER_NAME: &str = "cangjie_for_search";
/// 大小写敏感版 analyzer 的注册名（同一 cang-jie 分词，不挂 `LowerCaser`）。
pub const CS_TOKENIZER_NAME: &str = "cangjie_for_search_cs";

/// 丢弃纯空白 token 的过滤器。
#[derive(Debug, Clone, Copy, Default)]
pub struct DropWhitespaceFilter;

impl TokenFilter for DropWhitespaceFilter {
    type Tokenizer<T: Tokenizer> = DropWhitespaceWrapper<T>;

    fn transform<T: Tokenizer>(self, tokenizer: T) -> Self::Tokenizer<T> {
        DropWhitespaceWrapper { inner: tokenizer }
    }
}

/// [`DropWhitespaceFilter`] 的包装 tokenizer。
#[derive(Debug, Clone)]
pub struct DropWhitespaceWrapper<T: Tokenizer> {
    inner: T,
}

impl<T: Tokenizer> Tokenizer for DropWhitespaceWrapper<T> {
    type TokenStream<'a> = DropWhitespaceStream<T::TokenStream<'a>>;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> Self::TokenStream<'a> {
        DropWhitespaceStream {
            inner: self.inner.token_stream(text),
        }
    }
}

/// [`DropWhitespaceFilter`] 的 token stream。
pub struct DropWhitespaceStream<T> {
    inner: T,
}

impl<T: TokenStream> TokenStream for DropWhitespaceStream<T> {
    fn advance(&mut self) -> bool {
        while self.inner.advance() {
            if !self.inner.token().text.trim().is_empty() {
                return true;
            }
        }
        false
    }

    fn token(&self) -> &Token {
        self.inner.token()
    }

    fn token_mut(&mut self) -> &mut Token {
        self.inner.token_mut()
    }
}

/// 用共享的 Jieba 词典实例构建 analyzer。
///
/// `Jieba::new()` 会加载默认词典（秒级），不要在每次查询时重建；
/// [`crate::SearchIndex`] 内部持有一个 `Arc<Jieba>` 并复用。
pub fn build(worker: Arc<Jieba>) -> TextAnalyzer {
    TextAnalyzer::builder(CangJieTokenizer {
        worker,
        option: TokenizerOption::ForSearch { hmm: true },
    })
    .filter(DropWhitespaceFilter)
    .filter(LowerCaser)
    .build()
}

/// 大小写敏感版：同一 cang-jie 分词与空白过滤，但**不挂** `LowerCaser`。
pub fn build_case_sensitive(worker: Arc<Jieba>) -> TextAnalyzer {
    TextAnalyzer::builder(CangJieTokenizer {
        worker,
        option: TokenizerOption::ForSearch { hmm: true },
    })
    .filter(DropWhitespaceFilter)
    .build()
}

/// 用共享 Jieba 实例把文本切成 token（单测辅助）。
#[cfg(test)]
pub fn tokens(worker: Arc<Jieba>, text: &str) -> Vec<String> {
    let mut analyzer = build(worker);
    tokens_with(&mut analyzer, text)
}

/// 用已构建的 analyzer 切词；一次查询应复用它，避免每个 clause 重建。
pub fn tokens_with(analyzer: &mut TextAnalyzer, text: &str) -> Vec<String> {
    let mut stream = analyzer.token_stream(text);
    let mut out = Vec::new();
    while stream.advance() {
        out.push(stream.token().text.clone());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn worker() -> Arc<Jieba> {
        Arc::new(Jieba::new())
    }

    #[test]
    fn chinese_is_split_into_words() {
        assert_eq!(tokens(worker(), "笔记流程"), vec!["笔记", "流程"]);
    }

    #[test]
    fn whitespace_tokens_are_dropped() {
        let tokens = tokens(worker(), "GPUI 编辑器");
        assert!(
            tokens.iter().all(|token| !token.trim().is_empty()),
            "不应出现纯空白 token: {tokens:?}"
        );
        assert!(!tokens.iter().any(|token| token == " "));
    }

    #[test]
    fn english_is_lowercased_for_index_and_query() {
        assert_eq!(tokens(worker(), "GPUI"), vec!["gpui"]);
    }

    #[test]
    fn overlap_tokens_are_kept() {
        // cang-jie ForSearch 会同时产出「编辑」「编辑器」这类重叠词；
        // 保留它们是有意的（召回优先），不要在 analyzer 里做去重。
        let tokens = tokens(worker(), "编辑器");
        assert!(tokens.contains(&"编辑器".to_string()), "{tokens:?}");
    }
}
