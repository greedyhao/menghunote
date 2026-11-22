//! relay 的错误模型。
//!
//! 分两层：
//! - [`ErrorCode`] / [`ApiError`]：**协议可见**的错误（ADR 0006 §5 的 `{code, retryable, ...}`），
//!   HTTP 层把它序列化成响应体，绝不含秘密（不含密文、密钥、token、正文）。
//! - [`Error`]：内部错误（SQLite/IO/不变量），handler 统一映射成 500，不向客户端泄漏细节。

use std::fmt;

/// 协议错误码。字符串值即 wire 上的 `code`（ADR 0006 §5），不得静默改名。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    // --- 鉴权 / 授权 ---
    /// 无 token / token 无效 / token 已过期。
    Unauthorized,
    /// 设备或管理凭据已被吊销（立即失效）。
    Revoked,
    /// challenge 不存在 / realm / subject 不匹配 / 已过期。
    ChallengeInvalid,
    /// challenge 已被消费（单用）。
    ChallengeUsed,
    /// 该端点需要管理签名（管理 token ≠ 管理签名）。
    ManagementRequired,
    /// 签名/证明校验失败。
    SignatureInvalid,
    /// 不只是"signed-in 但不允许"——该身份对该操作无权限。
    Forbidden,
    /// 单实例已经有 vault（初始化端点关闭）。
    AlreadyInitialized,
    /// 对象/分片已被 GC 剪枝（数据不可再取）。
    Retired,
    // --- 对象 / 分片 ---
    /// 同 object_id 但固定 header 上下文不同；或对象已 complete 后内容不同。
    IdConflict,
    /// 同 epoch+message_id 已被别的 producer 占用（不允许混拼）。
    ObjectBusy,
    /// key_epoch 不是当前活动 epoch（含轮换冻结期）。
    EpochMismatch,
    /// 分片没齐 / refs 未齐 / grants 未齐。
    Incomplete,
    /// 同一 (object, part) 已有不同字节。
    PartConflict,
    /// 目标不存在。
    NotFound,
    /// 分片/对象不可变（已 complete 后内容不同、已 GC 剪枝）。
    Immutable,
    // --- 游标 ---
    /// 游标结构/MAC/vault 不匹配。
    BadCursor,
    /// 游标早于 gc_floor（日志前缀已被回收）。
    CursorExpired,
    /// relay 侧日志身份/水位回退（restore 换了 incarnation），客户端必须显式 reconcile。
    RelayReset,
    /// 客户端上报的 ACK 游标回退。
    CursorBackward,
    // --- 配额 ---
    /// 正常预算耗尽（507，read/ACK/auth/维护仍可用）。
    QuotaExceeded,
    /// 维护额度不足以建立 reservation（507）。
    ReserveExceeded,
    // --- 控制面 ---
    /// 控制版本 / epoch / slot_set_rev 的 CAS 失败。
    CasConflict,
    /// 状态机不允许该转换（例如已 aborted 的候选 epoch 再 activate）。
    InvalidState,
    /// 单个 writer lease 已存在。
    Busy,
    /// 邀请已被消费。
    InviteUsed,
    /// 邀请/lease/槽已过期。
    Expired,
    /// bootstrap lease 已过期（ADR §5：410 LEASE_EXPIRED）。
    LeaseExpired,
    /// refs 冲突（同 object 不同 role / 重复 pin）。
    RefConflict,
    /// 轮换进行中，该操作暂不可用。
    Rotating,
    /// 服务端内部错误（不回显细节）。
    Internal,
    /// 没有可用的两份有效基线。
    NoValidBaseline,
    /// 请求本身格式错误（含信封结构校验失败）。
    InvalidRequest,
    /// 体积超限。
    PayloadTooLarge,
    /// 服务端尚未接入的能力（例如 sync-proto 落地前）。
    NotImplemented,
    /// 入口限流（ADR 0014 §A：auth/create 入口先限流；只用于 v2，不影响 v1）。
    RateLimited,
    /// 该 vault 处于在线删除作业状态（`DELETING`/`DELETE_FAILED`/`DELETED`）：
    /// 数据面入口一律关闭（ADR 0018 §3）。**不可重试**：删除是单向终态，
    /// 客户端应进入 `NeedsAttention`（不清 pin、不自动重建）。
    VaultDeleted,
}

impl ErrorCode {
    /// wire 上的字符串（ADR 0006 §5）。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unauthorized => "UNAUTHORIZED",
            Self::Revoked => "REVOKED",
            Self::ChallengeInvalid => "CHALLENGE_INVALID",
            Self::ChallengeUsed => "CHALLENGE_USED",
            Self::ManagementRequired => "MANAGEMENT_REQUIRED",
            Self::SignatureInvalid => "SIGNATURE_INVALID",
            Self::Forbidden => "FORBIDDEN",
            Self::AlreadyInitialized => "ALREADY_INITIALIZED",
            Self::Retired => "RETIRED",
            Self::IdConflict => "ID_CONFLICT",
            Self::ObjectBusy => "OBJECT_BUSY",
            Self::EpochMismatch => "EPOCH",
            Self::Incomplete => "INCOMPLETE",
            Self::PartConflict => "PART_CONFLICT",
            Self::NotFound => "NOT_FOUND",
            Self::Immutable => "IMMUTABLE",
            Self::BadCursor => "BAD_CURSOR",
            Self::CursorExpired => "CURSOR_EXPIRED",
            Self::RelayReset => "RELAY_RESET",
            Self::CursorBackward => "CURSOR_BACKWARD",
            Self::QuotaExceeded => "QUOTA_EXCEEDED",
            Self::ReserveExceeded => "RESERVE",
            Self::CasConflict => "CAS_CONFLICT",
            Self::InvalidState => "INVALID_STATE",
            Self::Busy => "BUSY",
            Self::InviteUsed => "INVITE_USED",
            Self::Expired => "EXPIRED",
            Self::LeaseExpired => "LEASE_EXPIRED",
            Self::RefConflict => "REF_CONFLICT",
            Self::Rotating => "ROTATING",
            Self::Internal => "INTERNAL",
            Self::NoValidBaseline => "NO_VALID_BASELINE",
            Self::InvalidRequest => "INVALID_REQUEST",
            Self::PayloadTooLarge => "REQUEST_TOO_LARGE",
            Self::NotImplemented => "NOT_IMPLEMENTED",
            Self::RateLimited => "RATE_LIMITED",
            Self::VaultDeleted => "VAULT_DELETED",
        }
    }

    /// HTTP 状态码（ADR 0006 §5 各端点的"主要错误"）。
    pub const fn status(self) -> u16 {
        match self {
            Self::Unauthorized | Self::ChallengeInvalid | Self::ChallengeUsed => 401,
            Self::Revoked | Self::ManagementRequired | Self::Forbidden => 403,
            Self::SignatureInvalid | Self::BadCursor | Self::InvalidRequest => 400,
            Self::IdConflict
            | Self::ObjectBusy
            | Self::EpochMismatch
            | Self::Incomplete
            | Self::PartConflict
            | Self::Immutable
            | Self::RelayReset
            | Self::CursorBackward
            | Self::CasConflict
            | Self::InvalidState
            | Self::Busy
            | Self::NoValidBaseline
            | Self::InviteUsed
            | Self::AlreadyInitialized
            | Self::RefConflict
            | Self::Rotating => 409,
            Self::NotFound => 404,
            Self::CursorExpired
            | Self::Expired
            | Self::LeaseExpired
            | Self::Retired
            | Self::VaultDeleted => 410,
            Self::QuotaExceeded | Self::ReserveExceeded => 507,
            Self::PayloadTooLarge => 413,
            Self::RateLimited => 429,
            Self::NotImplemented => 501,
            Self::Internal => 500,
        }
    }

    /// 该错误在 wire 上是否标 `retryable`（ADR 0006 §5；只影响客户端退避策略）。
    pub const fn default_retryable(self) -> bool {
        matches!(
            self,
            Self::Busy
                | Self::RelayReset
                | Self::CursorExpired
                | Self::NoValidBaseline
                | Self::RateLimited
        )
    }
}

/// 协议可见错误：wire 上是 `{code, retryable, current_epoch?, current_control_rev?, bootstrap_hint?}`。
#[derive(Debug, Clone)]
pub struct ApiError {
    pub code: ErrorCode,
    pub retryable: bool,
    pub current_epoch: Option<u64>,
    pub current_control_rev: Option<u64>,
    pub bootstrap_hint: Option<&'static str>,
}

impl ApiError {
    /// 用错误码的默认 `retryable` 构造。
    pub fn new(code: ErrorCode) -> Self {
        Self {
            code,
            retryable: code.default_retryable(),
            current_epoch: None,
            current_control_rev: None,
            bootstrap_hint: None,
        }
    }

    pub fn retryable(mut self, retryable: bool) -> Self {
        self.retryable = retryable;
        self
    }

    pub fn epoch(mut self, epoch: u64) -> Self {
        self.current_epoch = Some(epoch);
        self
    }

    pub fn control_rev(mut self, rev: u64) -> Self {
        self.current_control_rev = Some(rev);
        self
    }

    pub fn hint(mut self, hint: &'static str) -> Self {
        self.bootstrap_hint = Some(hint);
        self
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.code.as_str())
    }
}

impl std::error::Error for ApiError {}

/// 内部错误。
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// 协议可见错误。
    #[error(transparent)]
    Api(#[from] ApiError),
    /// SQLite 层错误（不向客户端泄漏细节）。
    #[error("sqlite error")]
    Sqlite(#[from] rusqlite::Error),
    /// IO 错误。
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// 序列化错误（仅临时 JSON 控制面需要）。
    #[error("json error")]
    Json(#[from] serde_json::Error),
    /// writer 线程已关闭（进程退出/测试收尾）。
    #[error("sqlite writer thread is closed")]
    WriterClosed,
    /// 内部不变量被破坏（例如日志前缀出现洞）；这是 bug，不是客户端错误。
    #[error("invariant violated: {0}")]
    Invariant(&'static str),
    /// 签名 hook 拒绝了请求；调用方负责映射成具体错误码。
    #[error("signature rejected")]
    SignatureRejected,
    /// base64/base64url 解码失败。
    #[error("invalid base64")]
    Base64,
    /// 输入非法（纯格式问题）。
    #[error("invalid input: {0}")]
    Invalid(&'static str),
}

impl Error {
    /// 便捷构造：`Error::Api(ApiError::new(code))`。
    pub fn api(code: ErrorCode) -> Self {
        Error::Api(ApiError::new(code))
    }

    /// 若这是协议可见错误，返回错误码。
    pub fn code(&self) -> Option<ErrorCode> {
        match self {
            Error::Api(e) => Some(e.code),
            Error::SignatureRejected => Some(ErrorCode::SignatureInvalid),
            _ => None,
        }
    }
}

/// crate 内统一 `Result`。
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_codes_match_contract() {
        assert_eq!(ErrorCode::Revoked.status(), 403);
        assert_eq!(ErrorCode::CursorExpired.status(), 410);
        assert_eq!(ErrorCode::QuotaExceeded.status(), 507);
        assert_eq!(ErrorCode::RelayReset.status(), 409);
        assert_eq!(ErrorCode::PayloadTooLarge.status(), 413);
        assert_eq!(ErrorCode::LeaseExpired.status(), 410);
        assert_eq!(ErrorCode::Incomplete.status(), 409);
        assert_eq!(ErrorCode::NotFound.status(), 404);
        assert_eq!(ErrorCode::RateLimited.status(), 429);
        // ADR 0018 §3：删除态数据面关闭 = 410 + 不可重试（客户端 → NeedsAttention）。
        assert_eq!(ErrorCode::VaultDeleted.status(), 410);
        assert!(!ErrorCode::VaultDeleted.default_retryable());
        assert!(ErrorCode::RateLimited.default_retryable());
    }

    #[test]
    fn wire_codes_are_stable() {
        // 这些字符串是 wire 契约，改名等同于破坏协议。
        assert_eq!(
            ErrorCode::ManagementRequired.as_str(),
            "MANAGEMENT_REQUIRED"
        );
        assert_eq!(ErrorCode::ObjectBusy.as_str(), "OBJECT_BUSY");
        assert_eq!(ErrorCode::NoValidBaseline.as_str(), "NO_VALID_BASELINE");
        // 与 sync-proto 的 ErrorCode 字符串对齐（wire 单一真源）。
        assert_eq!(ErrorCode::EpochMismatch.as_str(), "EPOCH");
        assert_eq!(ErrorCode::ReserveExceeded.as_str(), "RESERVE");
        assert_eq!(ErrorCode::PayloadTooLarge.as_str(), "REQUEST_TOO_LARGE");
        assert_eq!(ErrorCode::VaultDeleted.as_str(), "VAULT_DELETED");
    }

    #[test]
    fn api_error_keeps_hints() {
        let e = ApiError::new(ErrorCode::CursorExpired)
            .epoch(7)
            .control_rev(3)
            .hint("bootstrap");
        assert!(e.retryable);
        assert_eq!(e.current_epoch, Some(7));
        assert_eq!(e.current_control_rev, Some(3));
        assert_eq!(e.bootstrap_hint, Some("bootstrap"));
    }

    #[test]
    fn internal_errors_do_not_leak_codes() {
        assert_eq!(Error::Invariant("x").code(), None);
        assert_eq!(
            Error::SignatureRejected.code(),
            Some(ErrorCode::SignatureInvalid)
        );
    }
}
