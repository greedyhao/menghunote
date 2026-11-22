//! 检索错误类型。

use thiserror::Error;

/// search crate 的统一错误。
#[derive(Debug, Error)]
pub enum SearchError {
    /// tantivy 侧错误（索引、查询、I/O 等）。
    #[error("tantivy: {0}")]
    Tantivy(#[from] tantivy::TantivyError),

    /// storage 侧错误（打开快照、读写 meta、manifest）。
    #[error("storage: {0}")]
    Storage(#[from] menghunote_storage::StorageError),

    /// 文件系统错误（fingerprint 文件等）。
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// 代目录/发布指针错误。
    #[error("generation: {0}")]
    Generation(String),

    /// 索引清单形状/内容不符合预期。
    #[error("manifest: {0}")]
    Manifest(String),

    /// fingerprint 读取/解析/不匹配。
    #[error("fingerprint: {0}")]
    Fingerprint(String),

    /// 索引目录里没有 fingerprint 文件（不是本 crate 创建的索引）。
    #[error("索引目录缺少 search.fingerprint，拒绝把它当作可复用索引")]
    MissingFingerprint,

    /// schema 与代码期望不一致（字段缺失等）。
    #[error("schema: {0}")]
    Schema(String),

    /// 用户输入无法转换成查询（目前主要保留给未来 tag/字段语法）。
    #[error("query: {0}")]
    Query(String),
}
