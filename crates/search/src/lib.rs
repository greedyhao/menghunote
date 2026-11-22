//! `menghunote-search`：笔记全文检索内核（S4-1b）。
//!
//! 范围（v1）：
//! - tantivy 0.27 + cang-jie 0.22 的**固定 analyzer 管线**
//!   （ForSearch → 丢纯空白 token → LowerCaser）；
//! - `title` + `body` 两个字段，`WithFreqsAndPositions`；
//! - 显式查询语义：引号 = 短语，空格 = AND；
//! - 索引生命周期、snippet 与 UTF-8 区间、版本落盘。
//!
//! 本 crate **不做**：UI、真实性能压测。

mod analyzer;
mod error;
mod fingerprint;
mod index;
mod manager;
mod markdown;

pub use analyzer::{DropWhitespaceFilter, TOKENIZER_NAME};
pub use error::SearchError;
pub use fingerprint::{
    Fingerprint, ANALYZER_VERSION, DICTIONARY_HASH_PLACEHOLDER, FINGERPRINT_FILE, MARKDOWN_VERSION,
    SCHEMA_VERSION,
};
pub use index::{
    fast_snippet, DocVersion, Hit, IndexedDocVersion, QueryOptions, QueryResult, SearchDoc,
    SearchIndex, SnippetLine, GRAM_TOKENIZER_NAME, SNIPPET_WINDOW_BYTES,
};
pub use manager::{
    BuildProgress, FlushOutcome, GenerationPointer, IndexManager, ProgressPhase, ReconcileReport,
    ACTIVE_GENERATION_META_KEY, DEBOUNCE, VAULT_UUID_META_KEY,
};
pub use markdown::{markdown_to_plain, MARKDOWN_TO_PLAIN_VERSION};
