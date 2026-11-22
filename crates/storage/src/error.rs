//! 存储层统一错误类型。
//!
//! 库层只暴露 [`StorageError`]，应用层再用 `anyhow` 之类的汇总类型包装。
//! 业务路径禁止 `unwrap()`；需要上下文时用 `?` 或 `map_err` 转成带上下文的变体。

use thiserror::Error;

/// 存储层错误。
#[derive(Debug, Error)]
pub enum StorageError {
    /// 文件系统错误（建父目录、启动工作线程等）。
    #[error("文件系统操作失败：{0}")]
    Io(#[from] std::io::Error),

    /// SQLite 自身返回的错误（SQL 语法、约束冲突、类型不匹配、锁等待超时等）。
    #[error("SQLite 操作失败：{0}")]
    Sqlite(#[from] rusqlite::Error),

    /// 只读快照在 `busy_timeout` 内没拿到读锁（写事务长时间占用）。
    ///
    /// 读路径**不做无限重试**：调用方（search）应当退避后重开快照，或改用
    /// `reconcile_required` 走全量对账，而不是在这里阻塞写线程。
    #[error("只读快照在 {busy_timeout_ms}ms 内未获得读锁（{context}）：请退避后重试或走全量对账")]
    SnapshotBusy {
        /// 出错的具体动作（打开/查询/解码）。
        context: &'static str,
        /// 该连接实际使用的 `busy_timeout`。
        busy_timeout_ms: u32,
    },

    /// 索引清单里某列的类型不符合 schema 预期（说明库被外部改坏或 schema 漂移）。
    #[error("索引清单列 [{column}] 类型异常：{found}")]
    ManifestShape {
        /// 列名。
        column: &'static str,
        /// 实际读到的 SQLite 类型。
        found: &'static str,
    },

    /// 迁移失败：带上目标版本与迁移名称，便于定位是哪一级迁移脚本出错。
    #[error("迁移到 schema v{version}（{name}）失败：{source}")]
    Migration {
        /// 目标 schema 版本。
        version: i64,
        /// 迁移的可读名称。
        name: &'static str,
        /// 底层 SQLite 错误。
        #[source]
        source: rusqlite::Error,
    },

    /// PRAGMA 设置后读回校验不一致。
    #[error("PRAGMA 校验失败：[{name}] 期望 {expected}，实际 {actual}")]
    PragmaMismatch {
        /// PRAGMA 名称。
        name: &'static str,
        /// 期望值（统一字符串化，便于比较）。
        expected: String,
        /// 实际读回值。
        actual: String,
    },

    /// 数据库 schema 版本高于本程序支持的版本：拒绝以可写方式打开。
    ///
    /// 对应 ADR 0001 §3.9「新 schema 下旧程序只读拒写」。
    #[error("数据库 schema 版本为 v{found}，本程序最高支持 v{supported}；拒绝以可写方式打开")]
    UnsupportedSchemaVersion {
        /// 数据库中记录的 `user_version`。
        found: i64,
        /// 本程序支持的最高 schema 版本。
        supported: i64,
    },

    /// 工作线程通道不可用（线程已退出，或初始化阶段就失败了）。
    #[error("存储工作线程不可用（{context}）")]
    ChannelClosed {
        /// 出错位置的可读描述。
        context: &'static str,
    },

    /// Loro 返回的错误（非法 update、peer id 非法、解码失败等）。
    #[error("Loro 操作失败：{0}")]
    Loro(#[from] loro::LoroError),

    /// Loro 导出/编码失败。
    #[error("Loro 导出失败：{0}")]
    LoroEncode(#[from] loro::LoroEncodeError),

    /// 坐标 / 单位换算失败（越界、半截字符、半截代理对）。
    ///
    /// 全仓只有 `menghunote_text::CoordError` 一份定义（ADR 0003 §4），这里只做包装。
    #[error("坐标换算失败：{0}")]
    Coord(#[from] menghunote_text::CoordError),

    /// A 事务的乐观并发冲突：head 已被别人推进，本批 update 未落库。
    #[error("文档 {doc_id} 的 head_seq 冲突：期望 {expected}，实际 {actual}；本批 update 已回滚")]
    HeadConflict {
        /// 文档 ID。
        doc_id: String,
        /// 调用方看到并用作 CAS 基准的 seq。
        expected: u64,
        /// 库里实际的 seq。
        actual: u64,
    },

    /// 未 ACK journal 已满：必须先 flush 再重试（背压，不允许无界增长）。
    #[error("未 ACK journal 已满（容量 {capacity} 条），拒绝新写入；请先 flush")]
    WriteBackpressure {
        /// journal 容量。
        capacity: usize,
    },

    /// 事务守卫不满足：某条语句没有影响预期行数，整笔事务已回滚。
    #[error("事务守卫失败：第 {statement} 条语句应影响 {expected} 行，实际 {actual} 行；已回滚")]
    GuardFailed {
        /// 语句下标（0 起）。
        statement: usize,
        /// 期望影响行数。
        expected: usize,
        /// 实际影响行数。
        actual: usize,
    },

    /// 相对光标无法解析成当前位置（历史不可读或已丢失）。
    #[error("光标无法解析到位置：{0}")]
    CursorUnresolved(String),

    /// 标签名为空（归一化后没有任何字符）。
    #[error("标签名非法：{name:?}（归一化后为空）")]
    InvalidTagName {
        /// 传入的原始标签名。
        name: String,
    },

    /// 结构树里找不到指定节点（节点 ID 非法或已被移除）。
    #[error("结构树里找不到节点 {node_id}")]
    UnknownNode {
        /// 节点 ID。
        node_id: String,
    },

    /// 同步桥 CAS/幂等冲突：同一 message/epoch 已存在不同字节，或旧 ACK/旧 digest。
    #[error("同步状态冲突：{context}")]
    SyncConflict {
        /// 具体上下文（message/epoch/digest）。
        context: String,
    },

    /// 同步 log_id 与本地已记录的不一致（relay reset 必须显式迁移，不能静默归零）。
    #[error("同步 log_id 不一致：本地 {expected:?}，收到 {found:?}")]
    SyncLogMismatch {
        /// 本地记录的 log_id。
        expected: Vec<u8>,
        /// 新收到的 log_id。
        found: Vec<u8>,
    },

    /// 数据库中出现不自洽状态（缺行、seq 断档、重放结果与记录不一致等）。
    #[error("存储状态不自洽：{context}")]
    Corrupt {
        /// 具体上下文。
        context: String,
    },
}
