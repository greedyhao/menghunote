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

    /// relay 配置/URL/模式非法（ADR 0013 §4 `ConfigInvalid`）。
    ///
    /// 只含公开定位信息（scheme/host/模式/字段名），不含任何凭据。
    #[error("同步配置无效：{0}")]
    ConfigInvalid(String),

    /// 需要重新建立信任（ADR 0013 §3/§4 `NeedsTrust`）。
    ///
    /// 出现在"本地没有可证明的信任锚"的路径上：缺失 durable pin、旧 pin 只有
    /// `(instance, rev)` 下界（同 rev 无 hash）、高 rev 前进缺少旧前缀 anchor。
    /// **不得**在调用方降级成 unpinned/bootstrap-only 继续。
    #[error("需要建立信任（不盲信继续）：{0}")]
    NeedsTrust(&'static str),

    /// durable 控制 pin 损坏/版本不支持/vault 绑定不符（ADR 0013 §2/§4 `PinCorrupt`）。
    ///
    /// **不得**当成"没有 pin"：`None` 只表示从未建立过。
    #[error("控制 pin 损坏：{0}")]
    PinCorrupt(&'static str),

    /// durable 控制 pin 保存失败（写事务失败或 CAS 未命中后的明确失败）。
    #[error("控制 pin 保存失败：{0}")]
    PinSaveFailed(String),

    /// relay instance 与本地 pin/身份不一致（换实例/静默回滚）。
    #[error("relay 实例与本地 pin 不一致（疑似换实例或静默回滚）")]
    InstanceMismatch,

    /// 控制链回退：新 head 的 rev 低于已提交 pin（ADR 0006 §35）。
    #[error("控制链回退：本地 pin rev={expected}，收到 rev={actual}")]
    ControlRollback {
        /// 已提交 pin 的 rev。
        expected: u64,
        /// 新 head 的 rev。
        actual: u64,
    },

    /// 控制链内容不一致（equivocation：换 genesis/重写链/同 rev 换 bootstrap 视图/
    /// 旧锚未被当前链覆盖）。消息只含公开定位信息。
    #[error("控制链内容不一致（equivocation）：{0}")]
    Equivocation(&'static str),

    /// durable pin 的并发 CAS 冲突：重新验证后重试，**不覆盖**赢家。
    #[error("控制 pin CAS 冲突：已被并发修改，不覆盖赢家")]
    PinConflict,

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

    /// 当前平台没有本地身份封装实现（Windows/Linux 之外明确拒绝，不回退明文）。
    #[error("本地身份封装在当前平台不可用")]
    LocalWrapUnsupportedPlatform,

    /// Linux 本地封装 BLOB 结构非法（magic/长度/编码不符）。
    #[error("本地封装格式非法：{0}")]
    LocalWrapFormat(&'static str),

    /// 未知的本地封装外层版本号。
    #[error("本地封装版本未知：{0}")]
    LocalWrapVersion(u8),

    /// 未知的本地封装外层 kind。
    #[error("本地封装类型未知：{0}")]
    LocalWrapKind(u8),

    /// 本地封装认证失败（头/nonce/密文被篡改，或 AAD 绑定的 vault/device/记录不符）。
    #[error("本地封装认证失败：密文损坏或 entropy 不匹配")]
    LocalWrapAuth,

    /// 本地身份封装路径无法解析（环境变量缺失/相对路径/非目录等）。
    #[error("本地主密钥路径无效：{0}")]
    LocalKeyPath(&'static str),

    /// 主密钥或其私有目录不存在（保护/解封只加载，不创建）。
    #[error("本地主密钥缺失：{0}")]
    LocalKeyMissing(&'static str),

    /// 主密钥内容损坏（长度/尾随字节/非 32B 主钥等）。
    #[error("本地主密钥损坏：{0}")]
    LocalKeyCorrupt(&'static str),

    /// 主密钥或目录的权限/属主/类型不安全（**不自动修复**）。
    #[error("本地主密钥权限不安全：{0}")]
    LocalKeyPermissions(&'static str),

    /// 主密钥文件 IO 失败（创建/打开/读写/fsync/原子发布）。
    #[error("本地主密钥 IO 失败：{0}")]
    LocalKeyIo(String),

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

    /// TLS 校验/握手失败（证书链、主机名、有效期、信任根装配）。**不可重试**：
    /// 重试不会让一张过期或错配的证书变合法。
    #[error("TLS 校验失败：{0}")]
    Tls(String),

    /// 传输安全策略拒绝（明文 http、跨 origin 重定向、https→http 降级、非法 base_url）。
    /// **不可重试**：策略是确定性的，重试只会重复被拒。
    #[error("传输策略拒绝：{0}")]
    TransportPolicy(String),

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

    /// 邀请链接非法/不可用（ADR 0014 S1）。
    ///
    /// 只含**脱敏**定位信息：不含 token、不含完整 URL；对端可据此区分格式/TTL/pin/状态。
    #[error("邀请链接无效：{0}")]
    InviteLink(String),

    /// relay 响应格式非法（缺字段/ID 不是 16/32 字节等）。
    #[error("relay 响应非法：{0}")]
    RelayWire(&'static str),

    /// `ApprovalEnvelopeV1`（ADR 0016 S4d1b）编解码/导入验证失败。
    ///
    /// 只含**公开定位信息**（字段/槽位/角色/次数）：包内证据本就是公开的
    /// cap/控制链事实，本变体**不**携带私钥、root、seed、邀请 token。
    #[error("审批包无效：{0}")]
    Approval(String),

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

    /// 取消/授权复核在**首次外部集成前**赢得竞争：本轮**没有**追加任何外部行（ADR 0012 S1）。
    ///
    /// 只在 `integrate` 之前的那道门返回；一旦进入 integrate 就不会再用它中断事务
    /// （已开始的真实事务必须跑完并交付）。**不可重试**：重试是用户/调度器的决定，
    /// 不能让自动路径悄悄再来一遍。
    #[error("同步已取消：在外部集成前停止（未追加任何本地行）")]
    CancelledBeforeIntegrate,

    /// 结构投影失败（ADR 0012 S2b）。
    ///
    /// 结构 CRDT 与 inbox 水位都已 durable；失败的是本地 `tree/notes` 投影。调用方必须
    /// **明确报错并保持写禁用**（不得假装侧栏已刷新），并保留重试入口。
    #[error("结构投影失败：{0}")]
    StructureProjection(String),

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
