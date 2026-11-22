//! 签名验签的**接缝**（ADR 0006 §4/§6：relay 自己算 SHA/验签，但签名输入的 CDE 编码由
//! `sync-proto` 提供）。
//!
//! 设计约束：`sync-proto` 未落地前，relay 无法独立重建 `b"MN/v1/<domain>\0" || CDE{...}` 的被签名
//! 字节。因此把"验签"抽象成 hook：
//!
//! - relay 负责**协议结构校验**（信封字段、DB 状态、摘要重算、CAS、成员集合、refs 存在性）；
//! - hook 负责**CDE 解析 + Ed25519 验签 + 载荷内字段一致性**（例如 UPDATE 的 `origin_seq ==
//!   header.sender_seq`、`ObjectCommit` 内的 `object_digest` 必须等于 relay 重算值）。
//!
//! 生产默认是 [`RejectingHook`]（fail-closed：sync-proto 未接入时任何需要签名的写都拒绝，
//! 不可能因为"忘了接"而放行）。测试自建接受型 hook。

use ed25519_dalek::{Signature, Verifier as _, VerifyingKey};

use crate::envelope::ObjectType;
use crate::error::{Error, ErrorCode, Result};
use crate::ids::{Hash32, Id16};
use crate::refs::ObjectRefMeta;
use menghunote_sync_proto as proto;

/// 签名域（ADR 0006 §14：域 = UTF8 文本 + **一个 0x00 字节**）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignDomain {
    /// `MN/v1/auth\0`：challenge-response。
    Auth,
    /// `MN/v1/manage\0`：管理写。
    Management,
    /// `MN/v1/object-commit\0`：对象完成签名。
    ObjectCommit,
    /// `MN/v1/checkpoint\0`：checkpoint 发布签名。
    Checkpoint,
    /// `MN/v1/genesis\0`：首台设备 genesis（一次性 setup secret 之外的设备自证明）。
    Genesis,
    /// `MN/v1/device-approve\0`：管理设备批准设备证书 + HPKE grant。
    DeviceApprove,
    /// `MN/v1/invite-redeem\0`：邀请兑换。
    InviteRedeem,
    /// `MN/v1/slot-activate\0`：key slot 激活（改口令）。
    SlotActivate,
    /// `MN/v1/rotation\0`：轮换请求/激活。
    Rotation,
    /// `MN/v1/activation-t\0`：完整 activation T 的旧/新管理 key 双签（ADR §34）。
    ActivationT,
}

impl SignDomain {
    /// 完整签名域前缀（含结尾 0x00，见 ADR 0006 §14）。
    ///
    /// 结构化域直接引用 `sync-proto` 的常量（wire 单一真源，避免两处字面量漂移）；
    /// genesis/invite/slot 等域在 ADR 里没有规定前缀，客户端侧自行定义，relay 只验签名本身。
    pub const fn tag(self) -> &'static [u8] {
        match self {
            Self::Auth => proto::SIGN_AUTH_DOMAIN,
            Self::Management => proto::SIGN_MANAGE_DOMAIN,
            Self::ObjectCommit => proto::SIGN_OBJECT_COMMIT_DOMAIN,
            Self::Checkpoint => proto::SIGN_CHECKPOINT_DOMAIN,
            Self::Genesis => b"MN/v1/genesis\0",
            Self::DeviceApprove => b"MN/v1/device-approve\0",
            Self::InviteRedeem => b"MN/v1/invite-redeem\0",
            Self::SlotActivate => b"MN/v1/slot-activate\0",
            Self::Rotation => b"MN/v1/rotation\0",
            Self::ActivationT => proto::SIGN_ACTIVATION_T_DOMAIN,
        }
    }

    /// 域前缀必须以单个 0x00 结尾（§14 的 golden 断言）。
    pub const fn tag_ends_with_nul(self) -> bool {
        let tag = self.tag();
        matches!(tag.last(), Some(0))
    }

    /// 该域的签名只允许由设备密钥或管理密钥提交（用于日志诊断）。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auth => "auth",
            Self::Management => "manage",
            Self::ObjectCommit => "object-commit",
            Self::Checkpoint => "checkpoint",
            Self::Genesis => "genesis",
            Self::DeviceApprove => "device-approve",
            Self::InviteRedeem => "invite-redeem",
            Self::SlotActivate => "slot-activate",
            Self::Rotation => "rotation",
            Self::ActivationT => "activation-t",
        }
    }
}

/// 一次验签请求（relay 已知的全部上下文）。
#[derive(Debug)]
pub struct SignatureCheck<'a> {
    /// 签名域。
    pub domain: SignDomain,
    /// vault（所有签名都必须绑定 vault，防跨 vault 重放）。
    pub vault_id: &'a Id16,
    /// 设备 id（管理签名时为 None）。
    pub device_id: Option<&'a Id16>,
    /// 验签公钥（设备证书内的 Ed25519 公钥，或当前管理公钥）。
    pub public_key: &'a [u8; 32],
    /// 客户端提交的**被签名字节**（`domain || CDE{...}`，由 sync-proto 产出）。
    pub message: &'a [u8],
    /// 64 字节 Ed25519 签名。
    pub signature: &'a [u8],
    /// relay 侧可独立校验的期望值；hook 必须把它们与 `message` 内的字段逐一比对。
    pub expect: &'a Expectation<'a>,
}

/// HPKE grant 要绑定的元数据（ADR 0006 §17.2：AAD = CDE(GrantAadV1)）。
#[derive(Debug, Clone, Copy)]
pub struct EpochGrantInput<'a> {
    pub epoch: u64,
    pub recipient_device_id: &'a Id16,
    /// 收件设备在 `devices.hpke_pk` 里登记的 X25519 公钥（**不信任客户端传入**）。
    pub recipient_x25519_pub: &'a [u8; 32],
    pub manager_rev: u64,
    pub grant_id: &'a Id16,
    /// HPKE 封装公钥 `enc`。
    pub hpke_enc: &'a [u8; 32],
    /// `SHA256(hpke_ciphertext)`。
    pub ciphertext_sha256: &'a Hash32,
}

/// `ObjectCommit` 要绑定的对象元数据（全部来自 relay 自己的 `objects` 行）。
#[derive(Debug, Clone, Copy)]
pub struct ObjectCommitExpectation<'a> {
    pub object_id: &'a Hash32,
    pub object_type: ObjectType,
    pub generation: u64,
    pub key_epoch: u64,
    pub sender_seq: u64,
    pub part_count: u32,
    pub plain_len: u64,
    /// relay 从已存分片重算的 `object_digest`。
    pub computed_digest: &'a Hash32,
}

/// relay 可独立重算的期望值。hook 必须校验"签名覆盖的载荷内字段 == 这里的值"。
#[derive(Debug)]
pub enum Expectation<'a> {
    /// 客户端已给出完整签名输入（如 genesis / HPKE grant / 槽激活），relay 只验签名本身。
    None,
    /// 对象完成：commit 内全部 header 字段与 `object_digest` 必须与 relay 记录一致。
    ObjectCommit {
        object: &'a ObjectCommitExpectation<'a>,
    },
    /// checkpoint 发布：覆盖 S、数据 refs 集合 digest、manifest 绑定。
    CheckpointPublish {
        checkpoint_id: &'a Id16,
        covered_seq: u64,
        manifest_oid: &'a Hash32,
        refs_digest: &'a Hash32,
        refs: &'a [ObjectRefMeta],
    },
    /// 认证证明：challenge 的 id/nonce 必须出现在被签名载荷内。
    Challenge {
        challenge_id: &'a Id16,
        nonce: &'a [u8; 32],
    },
    /// 设备证书：`MN/v1/device-cert\0 || CDE(DeviceCertV1)`（§17.1），签名者必须是管理 key。
    DeviceCert { cert: &'a proto::DeviceCertV1 },
    /// HPKE grant：`AAD || enc32 || SHA256(hpke_ciphertext)`（§17.2），签名者必须是当前管理 key。
    EpochGrant { grant: &'a EpochGrantInput<'a> },
    /// 管理写：方法标签/路径/request_id/控制版本/实例 id（hook 自己算 body 的 CDE 摘要）。
    Management {
        method_tag: u8,
        path: &'a str,
        request_id: &'a Id16,
        expected_control_rev: u64,
        instance_id: &'a Id16,
    },
}

/// 验签 hook。生产实现必须同时负责 CDE 解析与字段一致性（见模块文档）。
pub trait SignatureHook: Send + Sync + std::fmt::Debug {
    /// 校验签名与期望值；失败返回 [`Error::SignatureRejected`]（调用方映射错误码）。
    fn verify(&self, check: &SignatureCheck<'_>) -> Result<()>;
}

/// 生产默认：fail-closed。`sync-proto` 落地并接入真实 hook 前，一切需要签名的写都拒绝。
#[derive(Debug, Default, Clone, Copy)]
pub struct RejectingHook;

impl SignatureHook for RejectingHook {
    fn verify(&self, _check: &SignatureCheck<'_>) -> Result<()> {
        Err(Error::SignatureRejected)
    }
}

/// 调用 hook 并把拒绝映射成指定错误码（其他内部错误原样透传）。
pub fn require(
    hook: &dyn SignatureHook,
    check: &SignatureCheck<'_>,
    reject_code: ErrorCode,
) -> Result<()> {
    match hook.verify(check) {
        Ok(()) => Ok(()),
        Err(Error::SignatureRejected) => Err(Error::api(reject_code)),
        Err(other) => Err(other),
    }
}

/// 对**原始消息**做一次 Ed25519 验签（供未来的真实 hook 复用，不包含 CDE 解析）。
///
/// 注意：本函数只回答"该公钥对这些字节的签名是否有效"；把它绑定到具体 wire 语义
/// （域前缀、字段一致性）是 hook 的责任，不能直接当业务校验用。
pub fn verify_ed25519(public_key: &[u8; 32], message: &[u8], signature: &[u8]) -> Result<()> {
    let key = VerifyingKey::from_bytes(public_key).map_err(|_| Error::SignatureRejected)?;
    let sig_bytes: [u8; 64] = signature.try_into().map_err(|_| Error::SignatureRejected)?;
    let sig = Signature::from_bytes(&sig_bytes);
    key.verify(message, &sig)
        .map_err(|_| Error::SignatureRejected)
}

/// 域前缀 + 消息的完整签名输入（真实 hook 用它重建 `domain || CDE`）。
pub fn signing_input(domain: SignDomain, message: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(domain.tag().len() + message.len());
    out.extend_from_slice(domain.tag());
    out.extend_from_slice(message);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    #[test]
    fn domains_end_with_a_single_nul() {
        for domain in [
            SignDomain::Auth,
            SignDomain::Management,
            SignDomain::ObjectCommit,
            SignDomain::Checkpoint,
            SignDomain::ActivationT,
        ] {
            assert!(domain.tag_ends_with_nul(), "{domain:?}");
            assert_eq!(domain.tag().last(), Some(&0u8));
            // §14：不是两个字节 "\\0"，也不是 ASCII '0'。
            assert_ne!(domain.tag().last(), Some(&b'0'));
            let len = domain.tag().len();
            assert!(!domain.tag()[..len - 1].contains(&0u8));
        }
        // golden 断言：AAD 域后缀是十六进制 00。
        assert_eq!(SignDomain::Auth.tag(), b"MN/v1/auth\x00");
    }

    #[test]
    fn rejecting_hook_is_fail_closed() {
        let hook = RejectingHook;
        let mut sig = [0u8; 64];
        let check = SignatureCheck {
            domain: SignDomain::ObjectCommit,
            vault_id: &[1u8; 16],
            device_id: Some(&[2u8; 16]),
            public_key: &[3u8; 32],
            message: b"x",
            signature: &mut sig,
            expect: &Expectation::None,
        };
        assert!(matches!(
            require(&hook, &check, ErrorCode::SignatureInvalid),
            Err(Error::Api(e)) if e.code == ErrorCode::SignatureInvalid
        ));
    }

    #[test]
    fn ed25519_roundtrip_and_tamper() {
        let seed = [7u8; 32];
        let signing = SigningKey::from_bytes(&seed);
        let verifying = signing.verifying_key();
        let message = signing_input(SignDomain::Auth, b"challenge-bytes");
        let sig = signing.sign(&message);
        verify_ed25519(verifying.as_bytes(), &message, sig.to_bytes().as_slice())
            .expect("valid signature");

        let mut tampered = message.clone();
        tampered[0] ^= 0xff;
        assert!(matches!(
            verify_ed25519(verifying.as_bytes(), &tampered, &sig.to_bytes()),
            Err(Error::SignatureRejected)
        ));
        // 截断签名必须拒绝。
        assert!(verify_ed25519(verifying.as_bytes(), &message, &[0u8; 63]).is_err());
    }

    #[test]
    fn signing_input_has_no_separator_ambiguity() {
        // 域前缀自带 NUL，因此 domain||msg 不会与 "domainX||msg" 撞。
        let a = signing_input(SignDomain::Auth, b"x");
        assert_eq!(&a[..11], b"MN/v1/auth\0");
        assert_eq!(&a[11..], b"x");
    }
}
