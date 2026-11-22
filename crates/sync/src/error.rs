//! 统一错误类型。
//!
//! **秘密纪律**：任何变体都不得携带口令、根密钥、恢复种子、私钥或它们的派生值；
//! 只允许定位信息（字段名、长度、期望/实际的小整数、Win32 错误码）。
//! 这样即使错误被 `{:?}`/`{}` 打进日志，也不会泄露可用秘密。

use thiserror::Error;

/// `crates/sync` 的错误集合。
#[derive(Debug, Error)]
pub enum SyncError {
    /// `sync-proto` 的 wire 编解码失败（字段越界/非 canonical/未知字段等）。
    ///
    /// `ProtoError` 只带结构性描述，不含秘密。
    #[error("wire 契约失败：{0}")]
    Proto(#[from] menghunote_sync_proto::ProtoError),

    /// 本地文件操作失败（身份原子推广等）。
    #[error("本地 IO 失败：{0}")]
    Io(String),

    /// 口令长度越界（ADR 0006 §3：原样 UTF-8、不 trim、不规范化，1..=1024 字节）。
    #[error("口令长度必须在 {min}..={max} 字节之间（实际 {actual}）")]
    PasswordLength {
        /// 实际字节数。
        actual: usize,
        /// 允许下界（含）。
        min: usize,
        /// 允许上界（含）。
        max: usize,
    },

    /// Argon2id 参数越界（必须在调用 Argon 之前拒绝，ADR 0006 §3）。
    #[error("Argon2id 参数越界：{field}={value}")]
    Argon2Param {
        /// 字段名。
        field: &'static str,
        /// 实际取值。
        value: u64,
    },

    /// Argon2id 计算本身失败（参数已通过校验后仍失败，通常是实现/内存问题）。
    #[error("Argon2id 计算失败：{0}")]
    Argon2(&'static str),

    /// HKDF 派生失败（只可能是输出长度超过 255×HashLen）。
    #[error("HKDF-SHA256 派生失败（输出长度非法）")]
    Hkdf,

    /// 槽类型与 KDF/操作不匹配。
    #[error("密钥槽类型不匹配：期望 {expected}，实际 {actual}")]
    SlotKind {
        /// 期望类型。
        expected: &'static str,
        /// 实际类型。
        actual: &'static str,
    },

    /// `SlotDescriptorV1` 校验失败。
    #[error("密钥槽描述无效：{0}")]
    Descriptor(&'static str),

    /// 解封失败（口令/恢复码错误，或槽体被篡改/换错槽）。
    #[error("解封失败：口令/恢复码不匹配或槽体损坏")]
    Unwrap,

    /// 恢复码编解码失败。
    #[error("恢复码无效：{0}")]
    RecoveryCode(&'static str),

    /// HPKE 失败。
    #[error("HPKE 失败：{0}")]
    Hpke(&'static str),

    /// HPKE 收件人公钥与本机身份不一致（错 recipient）。
    #[error("HPKE 收件人不匹配")]
    RecipientMismatch,

    /// Ed25519 签名验证失败。
    #[error("签名验证失败")]
    Signature,

    /// `DeviceCertV1` 校验失败。
    #[error("设备证书无效：{0}")]
    Certificate(&'static str),

    /// 邀请令牌无效（已用/过期/哈希不匹配）。
    #[error("邀请令牌无效：{0}")]
    Invite(&'static str),

    /// 槽集合 CAS 冲突。
    #[error("槽集合 CAS 冲突：期望 rev={expected}，实际 rev={actual}")]
    Cas {
        /// 期望的槽集合版本。
        expected: u64,
        /// 存储侧当前版本。
        actual: u64,
    },

    /// 改口令流程失败。
    #[error("改口令失败：{0}")]
    PasswordChange(&'static str),

    /// 确定性 CBOR 编解码失败。
    #[error("CDE 编解码失败：{0}")]
    Cde(&'static str),

    /// 本地身份 BLOB 结构无效。
    #[error("本地身份结构无效：{0}")]
    Identity(&'static str),

    /// DPAPI 不可用（非 Windows 平台明确拒绝，不假装可用）。
    #[error("DPAPI 不可用：仅 Windows CurrentUser 实现")]
    DpapiUnavailable,

    /// DPAPI 调用失败，携带 Win32 错误码（不含明文）。
    #[error("DPAPI 调用失败：Win32 错误码 {0}")]
    Dpapi(u32),

    /// 系统随机源失败。
    #[error("系统随机源失败")]
    Random,

    /// 本地存储错误（经 `menghunote-storage` 暴露）。
    #[error("本地存储失败：{0}")]
    Storage(#[from] menghunote_storage::StorageError),

    /// 本地存储里的同步相关数据不一致（schema 不认、outbox 缺字段等）。
    #[error("本地同步数据异常：{0}")]
    LocalCorrupt(&'static str),

    /// relay 传输层失败（连接断开、响应被截断等）。
    #[error("relay 传输失败：{0}")]
    Transport(&'static str),

    /// relay 返回业务错误（CBOR 错误体 `{0:code,1:retryable,2:epoch?,3:rev?,4:hint?}`）。
    #[error("relay 拒绝：{code}")]
    RelayApi {
        /// 统一错误码（wire 字符串）。
        code: String,
        /// 是否可重试。
        retryable: bool,
        /// relay 当前 epoch（`EPOCH` 类错误携带）。
        current_epoch: Option<u64>,
        /// relay 当前 control_rev（`CAS_CONFLICT` 类错误携带）。
        current_control_rev: Option<u64>,
        /// 需要重新 bootstrap 时的提示（`RELAY_RESET`/`CURSOR_EXPIRED`）。
        bootstrap_hint: Option<String>,
    },

    /// relay 响应格式非法（缺字段/ID 不是 16/32 字节等）。
    #[error("relay 响应非法：{0}")]
    RelayWire(&'static str),

    /// HTTP 状态码非预期。
    #[error("relay HTTP 状态 {0}")]
    HttpStatus(u16),

    /// 端点/能力尚未实现（例如真实 relay 还没有设备证书查询）。
    #[error("尚未实现：{0}")]
    Unsupported(&'static str),

    /// 对象级校验失败（digest/签名/解密）。
    #[error("同步对象无效：{0}")]
    Object(&'static str),

    /// 同步必须暂停（游标回滚/epoch 变化等）；暂停时**不得**推进游标或 ACK。
    #[error("同步暂停：{0}")]
    Paused(crate::relay::PauseReason),

    /// head CAS 冲突（并发本地写入把 head 推进了）。**可重试**：重读 head 后重放，
    /// 不得把它当成"数据丢了"而丢弃 outbox/inbox 行。
    #[error("head CAS 冲突：{doc_id} 期望 {expected}，实际 {actual}")]
    HeadConflict {
        /// 文档 ID。
        doc_id: String,
        /// 调用方用作 CAS 基准的 seq。
        expected: u64,
        /// 库里实际的 seq。
        actual: u64,
    },
}

impl SyncError {
    /// 是否可安全重试（不丢数据、可重复提交）。
    ///
    /// 只把明确的"并发/瞬时"错误标为可重试；校验/解密/签名失败一律不可重试，
    /// 以免把损坏数据当成可恢复状态反复重放。
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::HeadConflict { .. } | Self::Transport(_) | Self::Random => true,
            Self::RelayApi { retryable, .. } => *retryable,
            Self::Storage(menghunote_storage::StorageError::SnapshotBusy { .. }) => true,
            _ => false,
        }
    }
}
