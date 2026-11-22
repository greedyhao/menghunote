//! sync-proto 的基础错误与错误码。

use std::fmt;

/// 纯协议层错误；不做 IO/网络描述。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtoError {
    /// 数据太短（header/part 截断）。
    Truncated,
    /// 文本/字节长度或字段值越界。
    Invalid(String),
    /// 非 canonical CBOR（整数过长、键乱序、重复键、浮点/null/不定长等）。
    NonCanonical(String),
    /// 未知字段/键。
    UnknownField(u64),
    /// 字段缺失。
    MissingField(u64),
    /// 不允许的版本。
    UnsupportedVersion(u16),
    /// 超过协议上限。
    TooLarge,
}

impl fmt::Display for ProtoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated => f.write_str("截断的协议数据"),
            Self::Invalid(message) => write!(f, "协议字段非法: {message}"),
            Self::NonCanonical(message) => write!(f, "非 canonical CDE: {message}"),
            Self::UnknownField(key) => write!(f, "未知字段键 {key}"),
            Self::MissingField(key) => write!(f, "缺少字段键 {key}"),
            Self::UnsupportedVersion(version) => write!(f, "不支持的协议版本 {version}"),
            Self::TooLarge => f.write_str("超过协议大小上限"),
        }
    }
}

impl std::error::Error for ProtoError {}

/// ADR 0006 §5 的统一错误码（wire 用字符串）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    AlreadyInitialized,
    Revoked,
    ChallengeInvalid,
    ChallengeUsed,
    ManagementRequired,
    InviteUsed,
    Expired,
    Rotating,
    IdConflict,
    Epoch,
    ObjectBusy,
    Incomplete,
    BadCursor,
    CursorExpired,
    Reset,
    NoValidBaseline,
    LeaseExpired,
    Busy,
    Reserve,
    RefConflict,
    NonMonotone,
    RecipientMismatch,
    CursorBackward,
    RollbackDetected,
    RelayReset,
    Forbidden,
    Unauthorized,
    NotFound,
    Gone,
    MethodNotAllowed,
    Conflict,
    RequestTooLarge,
    TooManyRequests,
    Internal,
}

impl ErrorCode {
    /// wire 字符串（全大写，稳定）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AlreadyInitialized => "ALREADY_INITIALIZED",
            Self::Revoked => "REVOKED",
            Self::ChallengeInvalid => "CHALLENGE_INVALID",
            Self::ChallengeUsed => "CHALLENGE_USED",
            Self::ManagementRequired => "MANAGEMENT_REQUIRED",
            Self::InviteUsed => "INVITE_USED",
            Self::Expired => "EXPIRED",
            Self::Rotating => "ROTATING",
            Self::IdConflict => "ID_CONFLICT",
            Self::Epoch => "EPOCH",
            Self::ObjectBusy => "OBJECT_BUSY",
            Self::Incomplete => "INCOMPLETE",
            Self::BadCursor => "BAD_CURSOR",
            Self::CursorExpired => "CURSOR_EXPIRED",
            Self::Reset => "RESET",
            Self::NoValidBaseline => "NO_VALID_BASELINE",
            Self::LeaseExpired => "LEASE_EXPIRED",
            Self::Busy => "BUSY",
            Self::Reserve => "RESERVE",
            Self::RefConflict => "REF_CONFLICT",
            Self::NonMonotone => "NON_MONOTONE",
            Self::RecipientMismatch => "RECIPIENT_MISMATCH",
            Self::CursorBackward => "CURSOR_BACKWARD",
            Self::RollbackDetected => "ROLLBACK_DETECTED",
            Self::RelayReset => "RELAY_RESET",
            Self::Forbidden => "FORBIDDEN",
            Self::Unauthorized => "UNAUTHORIZED",
            Self::NotFound => "NOT_FOUND",
            Self::Gone => "GONE",
            Self::MethodNotAllowed => "METHOD_NOT_ALLOWED",
            Self::Conflict => "CONFLICT",
            Self::RequestTooLarge => "REQUEST_TOO_LARGE",
            Self::TooManyRequests => "TOO_MANY_REQUESTS",
            Self::Internal => "INTERNAL",
        }
    }

    /// 客户端是否可重试（保守口径）。
    pub fn retryable(self) -> bool {
        matches!(
            self,
            Self::Busy | Self::ObjectBusy | Self::TooManyRequests | Self::Internal
        )
    }
}

/// 错误响应体；不含秘密。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorBody {
    pub code: ErrorCode,
    pub retryable: bool,
    pub current_epoch: Option<u64>,
    pub current_control_rev: Option<u64>,
    pub bootstrap_hint: Option<String>,
}

impl ErrorBody {
    pub fn new(code: ErrorCode) -> Self {
        Self {
            code,
            retryable: code.retryable(),
            current_epoch: None,
            current_control_rev: None,
            bootstrap_hint: None,
        }
    }
}
