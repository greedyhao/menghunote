//! 真实签名校验实现（`sync-proto` 落地后接入）。
//!
//! 分工（ADR 0006 §15 边界）：`sync-proto` 只产出**确定性的待编码字节**，不做 SHA/验签；
//! 本模块用 `sync-proto` 解码 CDE 记录，再由 relay 计算摘要并做 Ed25519 验签。
//!
//! 覆盖：
//! - `ObjectCommit`：解码后逐字段绑定 relay 已知的对象元数据（vault/object/type/generation/
//!   epoch/sender_seq/part_count/total_len/object_digest），再验签；**摘要必须等于 relay 重算值**。
//! - `CheckpointCommit`：解码后绑定 checkpoint_id/covering S/manifest_oid/refs_digest，
//!   并用 `refs_digest_input` 重算公开 data refs 的 digest（§15：manifest 自身不纳入）。
//! - `Challenge`：绑定 vault/challenge_id/nonce/（设备 realm 时）subject。
//! - 管理写：`MN/v1/manage\0 || instance || vault || request_id || expected_control_rev ||
//!   SHA256(CDE{method,path,unsigned_body})`。
//! - 其余（genesis / HPKE grant / invite redeem / slot activate）：客户端提交的
//!   `message` 已是完整签名输入，relay 只验签名本身（其余绑定由管理控制链校验）。

use menghunote_sync_proto as proto;

use crate::error::{ApiError, Error, ErrorCode, Result};
use crate::ids::{sha256, Id16};
use crate::refs::ObjectRefMeta;
use crate::sig::{Expectation, SignatureCheck, SignatureHook};

/// 生产用验签实现：CDE 解码 + Ed25519。
#[derive(Debug, Default, Clone, Copy)]
pub struct Ed25519Hook;

fn reject() -> Error {
    // 统一成"签名拒绝"，调用方再映射成具体错误码（不给攻击者细分 oracle）。
    Error::SignatureRejected
}

fn verify_raw(check: &SignatureCheck<'_>, input: &[u8]) -> Result<()> {
    crate::sig::verify_ed25519(check.public_key, input, check.signature)
}

fn proto_ref(meta: &ObjectRefMeta) -> Result<proto::ObjectRefV1> {
    Ok(proto::ObjectRefV1 {
        object_id: meta.object_id,
        object_type: proto::ObjectType::try_from(meta.object_type.as_u8())
            .map_err(|_| Error::SignatureRejected)?,
        generation: meta.generation,
        key_epoch: meta.key_epoch,
        part_count: meta.part_count,
        total_plain_len: meta.total_plain_len,
        object_digest: meta.object_digest,
    })
}

impl SignatureHook for Ed25519Hook {
    fn verify(&self, check: &SignatureCheck<'_>) -> Result<()> {
        match check.expect {
            // 客户端已经给出完整签名输入（domain 由客户端/协议层拼接）。
            Expectation::None => verify_raw(check, check.message),

            Expectation::ObjectCommit { object } => {
                let commit = proto::ObjectCommit::decode(check.message).map_err(|_| reject())?;
                let matches = commit.vault_id == *check.vault_id
                    && commit.object_id == *object.object_id
                    && commit.object_type
                        == proto::ObjectType::try_from(object.object_type.as_u8())
                            .map_err(|_| reject())?
                    && commit.generation == object.generation
                    && commit.key_epoch == object.key_epoch
                    && commit.sender_seq == object.sender_seq
                    && commit.part_count == object.part_count
                    && commit.total_len == object.plain_len
                    && commit.object_digest == *object.computed_digest;
                if !matches {
                    return Err(reject());
                }
                let input = proto::object_commit_signing_input(&commit).map_err(|_| reject())?;
                verify_raw(check, &input)
            }

            Expectation::CheckpointPublish {
                checkpoint_id,
                covered_seq,
                manifest_oid,
                refs_digest,
                refs,
            } => {
                let commit =
                    proto::CheckpointCommit::decode(check.message).map_err(|_| reject())?;
                if commit.vault_id != *check.vault_id
                    || commit.checkpoint_id.as_slice() != checkpoint_id.as_slice()
                    || commit.covered_relay_seq != *covered_seq
                    || commit.manifest_object_id.as_slice() != manifest_oid.as_slice()
                {
                    return Err(reject());
                }
                // 公开 data refs 的 digest：relay 侧重算，必须同时等于 commit 与请求声明的值。
                let refs_v1 = refs
                    .iter()
                    .map(proto_ref)
                    .collect::<Result<Vec<proto::ObjectRefV1>>>()?;
                let digest_input = proto::refs_digest_input(&refs_v1).map_err(|_| reject())?;
                let recomputed = sha256(&digest_input);
                if recomputed.as_slice() != commit.refs_digest.as_slice()
                    || recomputed.as_slice() != refs_digest.as_slice()
                {
                    return Err(reject());
                }
                if let Some(device_id) = check.device_id {
                    if commit.producer_device_id != *device_id {
                        return Err(reject());
                    }
                }
                let input = proto::checkpoint_signing_input(&commit).map_err(|_| reject())?;
                verify_raw(check, &input)
            }

            Expectation::Challenge {
                challenge_id,
                nonce,
            } => {
                let challenge = proto::Challenge::decode(check.message).map_err(|_| reject())?;
                if challenge.vault_id != *check.vault_id
                    || challenge.challenge_id.as_slice() != challenge_id.as_slice()
                    || challenge.nonce.as_slice() != nonce.as_slice()
                {
                    return Err(reject());
                }
                if let Some(device_id) = check.device_id {
                    if challenge.subject != *device_id {
                        return Err(reject());
                    }
                }
                let input = proto::auth_signing_input(&challenge).map_err(|_| reject())?;
                verify_raw(check, &input)
            }

            Expectation::DeviceCert { cert } => {
                if cert.vault_id != *check.vault_id {
                    return Err(reject());
                }
                if let Some(device_id) = check.device_id {
                    if cert.device_id != *device_id {
                        return Err(reject());
                    }
                }
                let input = proto::device_cert_signed_bytes(cert).map_err(|_| reject())?;
                if !check.message.is_empty() && check.message != input.as_slice() {
                    return Err(reject());
                }
                verify_raw(check, &input)
            }

            Expectation::EpochGrant { grant } => {
                // §17.2：AAD 的字段全部由 relay 侧事实重建（收件人/epoch/manager_rev/enc/密文摘要），
                // 客户端不能只签一个自造的 AAD 就把 grant 塞进来。
                let aad = proto::GrantAadV1 {
                    pv: 1,
                    vault_id: *check.vault_id,
                    epoch: grant.epoch,
                    recipient_device_id: *grant.recipient_device_id,
                    recipient_x25519_pub: *grant.recipient_x25519_pub,
                    manager_rev: grant.manager_rev,
                    grant_id: *grant.grant_id,
                };
                let input =
                    proto::grant_signed_bytes(&aad, grant.hpke_enc, grant.ciphertext_sha256)
                        .map_err(|_| reject())?;
                if !check.message.is_empty() && check.message != input.as_slice() {
                    // 客户端提交的"signed_header"必须与 relay 重建的待签字节完全一致。
                    return Err(reject());
                }
                verify_raw(check, &input)
            }

            Expectation::Management {
                method_tag,
                path,
                request_id,
                expected_control_rev,
                instance_id,
            } => {
                let method = proto::HttpMethod::try_from(*method_tag).map_err(|_| reject())?;
                let request_cde = proto::management_request_cde(method, path, check.message)
                    .map_err(|_| reject())?;
                let body_digest = sha256(&request_cde);
                let input = proto::management_signing_input(
                    instance_id,
                    check.vault_id,
                    request_id,
                    *expected_control_rev,
                    &body_digest,
                )
                .map_err(|_| reject())?;
                verify_raw(check, &input)
            }
        }
    }
}

/// 构造 `DeviceCertV1`（不含签名；调用方用管理私钥签 `device_cert_signed_bytes`）。
pub fn new_certificate(
    vault_id: &Id16,
    device_id: &Id16,
    sign_pk: &[u8; 32],
    hpke_pk: &[u8; 32],
    manager_rev: u64,
) -> proto::DeviceCertV1 {
    proto::DeviceCertV1 {
        vault_id: *vault_id,
        device_id: *device_id,
        sign_ed25519_pub: *sign_pk,
        hpke_x25519_pub: *hpke_pk,
        cert_serial: 1,
        issued_manager_rev: manager_rev,
        rights: proto::Rights(proto::Rights::ALL),
    }
}

/// 便捷：验签失败映射成协议错误（auth/管理/对象完成各自的错误码不同）。
pub fn map_reject(err: Error, code: ErrorCode) -> Error {
    match err {
        Error::SignatureRejected => Error::Api(ApiError::new(code)),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{Id16, ZERO_HASH32};
    use crate::sig::ObjectCommitExpectation;
    use ed25519_dalek::{Signer, SigningKey};

    fn keypair() -> (SigningKey, [u8; 32]) {
        let signing = SigningKey::from_bytes(&[9u8; 32]);
        let public = signing.verifying_key().to_bytes();
        (signing, public)
    }

    fn commit(key_epoch: u64, sender_seq: u64, digest: [u8; 32]) -> proto::ObjectCommit {
        proto::ObjectCommit {
            vault_id: [1u8; 16],
            object_id: [2u8; 32],
            message_id: [3u8; 16],
            object_type: proto::ObjectType::Update,
            generation: 7,
            key_epoch,
            sender_seq,
            part_count: 1,
            total_len: 10,
            object_digest: digest,
        }
    }

    #[test]
    fn object_commit_binds_every_header_field() {
        let (signing, public) = keypair();
        let digest = [4u8; 32];
        let signed = commit(3, 11, digest);
        let message = signed.encode().expect("encode");
        let input = proto::object_commit_signing_input(&signed).expect("input");
        let signature = signing.sign(&input).to_bytes().to_vec();

        let expect = ObjectCommitExpectation {
            object_id: &[2u8; 32],
            object_type: crate::envelope::ObjectType::Update,
            generation: 7,
            key_epoch: 3,
            sender_seq: 11,
            part_count: 1,
            plain_len: 10,
            computed_digest: &digest,
        };
        let check = SignatureCheck {
            domain: crate::sig::SignDomain::ObjectCommit,
            vault_id: &[1u8; 16],
            device_id: Some(&[5u8; 16]),
            public_key: &public,
            message: &message,
            signature: &signature,
            expect: &Expectation::ObjectCommit { object: &expect },
        };
        Ed25519Hook.verify(&check).expect("valid commit");

        // 摘要不一致必须拒绝（防"签名对但内容被换"）。
        let wrong_digest = [6u8; 32];
        let expect_bad = ObjectCommitExpectation {
            computed_digest: &wrong_digest,
            ..expect
        };
        let check_bad = SignatureCheck {
            expect: &Expectation::ObjectCommit {
                object: &expect_bad,
            },
            ..check
        };
        assert!(matches!(
            Ed25519Hook.verify(&check_bad),
            Err(Error::SignatureRejected)
        ));
    }

    #[test]
    fn object_commit_rejects_wrong_key_epoch_and_tamper() {
        let (signing, public) = keypair();
        let digest = [4u8; 32];
        let signed = commit(3, 11, digest);
        let message = signed.encode().expect("encode");
        let input = proto::object_commit_signing_input(&signed).expect("input");
        let signature = signing.sign(&input).to_bytes().to_vec();
        let wrong_epoch = ObjectCommitExpectation {
            object_id: &[2u8; 32],
            object_type: crate::envelope::ObjectType::Update,
            generation: 7,
            key_epoch: 4, // 与 commit 的 3 不同
            sender_seq: 11,
            part_count: 1,
            plain_len: 10,
            computed_digest: &digest,
        };
        let check_wrong_epoch = SignatureCheck {
            domain: crate::sig::SignDomain::ObjectCommit,
            vault_id: &[1u8; 16],
            device_id: Some(&[5u8; 16]),
            public_key: &public,
            message: &message,
            signature: &signature,
            expect: &Expectation::ObjectCommit {
                object: &wrong_epoch,
            },
        };
        assert!(Ed25519Hook.verify(&check_wrong_epoch).is_err());
        // 篡改签名：拒绝。
        let mut bad_sig = signature.clone();
        bad_sig[0] ^= 1;
        let expect = ObjectCommitExpectation {
            object_id: &[2u8; 32],
            object_type: crate::envelope::ObjectType::Update,
            generation: 7,
            key_epoch: 3,
            sender_seq: 11,
            part_count: 1,
            plain_len: 10,
            computed_digest: &digest,
        };
        let check_bad = SignatureCheck {
            domain: crate::sig::SignDomain::ObjectCommit,
            vault_id: &[1u8; 16],
            device_id: Some(&[5u8; 16]),
            public_key: &public,
            message: &message,
            signature: &bad_sig,
            expect: &Expectation::ObjectCommit { object: &expect },
        };
        assert!(Ed25519Hook.verify(&check_bad).is_err());
    }

    #[test]
    fn checkpoint_refs_digest_is_recomputed() {
        let (signing, public) = keypair();
        let refs = vec![ObjectRefMeta {
            object_id: [8u8; 32],
            object_type: crate::envelope::ObjectType::Snapshot,
            generation: 2,
            key_epoch: 1,
            part_count: 1,
            total_plain_len: 100,
            object_digest: [7u8; 32],
        }];
        let refs_v1 = vec![proto_ref(&refs[0]).expect("ref")];
        let refs_input = proto::refs_digest_input(&refs_v1).expect("input");
        let refs_digest = sha256(&refs_input);

        let commit = proto::CheckpointCommit {
            vault_id: [1u8; 16],
            checkpoint_id: [9u8; 16],
            epoch: 1,
            covered_relay_seq: 42,
            manifest_object_id: [3u8; 32],
            manifest_digest: [5u8; 32],
            refs_digest,
            producer_device_id: [6u8; 16],
            validation_version: 2,
            validation_digest: [0x5Au8; 32],
        };
        let message = commit.encode().expect("encode");
        let input = proto::checkpoint_signing_input(&commit).expect("input");
        let signature = signing.sign(&input).to_bytes().to_vec();
        let check = SignatureCheck {
            domain: crate::sig::SignDomain::Checkpoint,
            vault_id: &[1u8; 16],
            device_id: Some(&[6u8; 16]),
            public_key: &public,
            message: &message,
            signature: &signature,
            expect: &Expectation::CheckpointPublish {
                checkpoint_id: &[9u8; 16],
                covered_seq: 42,
                manifest_oid: &[3u8; 32],
                refs_digest: &refs_digest,
                refs: &refs,
            },
        };
        Ed25519Hook.verify(&check).expect("valid checkpoint");

        // refs 被换 → refs_digest 重算不一致 → 拒绝。
        let other_refs = vec![ObjectRefMeta {
            object_digest: [0u8; 32],
            ..refs[0].clone()
        }];
        let check_bad = SignatureCheck {
            expect: &Expectation::CheckpointPublish {
                checkpoint_id: &[9u8; 16],
                covered_seq: 42,
                manifest_oid: &[3u8; 32],
                refs_digest: &refs_digest,
                refs: &other_refs,
            },
            ..check
        };
        assert!(Ed25519Hook.verify(&check_bad).is_err());
    }

    #[test]
    fn challenge_binds_nonce_and_vault() {
        let (signing, public) = keypair();
        let challenge = proto::Challenge {
            pv: 1,
            relay_instance_id: [1u8; 16],
            origin_sha256: ZERO_HASH32,
            vault_id: [2u8; 16],
            realm: proto::AuthRealm::Device,
            subject: [3u8; 16],
            challenge_id: [4u8; 16],
            nonce: [5u8; 32],
            expires_at: 100,
        };
        let message = challenge.encode().expect("encode");
        let input = proto::auth_signing_input(&challenge).expect("input");
        let signature = signing.sign(&input).to_bytes().to_vec();
        let check = SignatureCheck {
            domain: crate::sig::SignDomain::Auth,
            vault_id: &[2u8; 16],
            device_id: Some(&[3u8; 16]),
            public_key: &public,
            message: &message,
            signature: &signature,
            expect: &Expectation::Challenge {
                challenge_id: &[4u8; 16],
                nonce: &[5u8; 32],
            },
        };
        Ed25519Hook.verify(&check).expect("valid challenge");

        // 换一个 nonce 的期望 → 拒绝（防重放到别的 challenge）。
        let other_nonce = [6u8; 32];
        let check_bad = SignatureCheck {
            expect: &Expectation::Challenge {
                challenge_id: &[4u8; 16],
                nonce: &other_nonce,
            },
            ..check
        };
        assert!(Ed25519Hook.verify(&check_bad).is_err());
    }

    #[test]
    fn management_input_uses_adr_domain_and_binding() {
        let (signing, public) = keypair();
        let instance: Id16 = [1u8; 16];
        let vault: Id16 = [2u8; 16];
        let request_id: Id16 = [3u8; 16];
        let body = b"cde-body";
        let method = proto::HttpMethod::Post;
        let cde =
            proto::management_request_cde(method, "/v1/vaults/x/rotations", body).expect("cde");
        let digest = sha256(&cde);
        let input = proto::management_signing_input(&instance, &vault, &request_id, 7, &digest)
            .expect("input");
        // §14：域以单个 0x00 结尾。
        assert_eq!(&input[..13], b"MN/v1/manage\0");
        let signature = signing.sign(&input).to_bytes().to_vec();

        let check = SignatureCheck {
            domain: crate::sig::SignDomain::Management,
            vault_id: &vault,
            device_id: None,
            public_key: &public,
            message: body,
            signature: &signature,
            expect: &Expectation::Management {
                method_tag: 2,
                path: "/v1/vaults/x/rotations",
                request_id: &request_id,
                expected_control_rev: 7,
                instance_id: &instance,
            },
        };
        Ed25519Hook
            .verify(&check)
            .expect("valid management signature");

        // path 不同 → 输入不同 → 拒绝（防跨端点重放）。
        let check_bad = SignatureCheck {
            expect: &Expectation::Management {
                method_tag: 2,
                path: "/v1/vaults/x/objects",
                request_id: &request_id,
                expected_control_rev: 7,
                instance_id: &instance,
            },
            ..check
        };
        assert!(Ed25519Hook.verify(&check_bad).is_err());
    }
    #[test]
    fn epoch_grant_binds_recipient_epoch_and_ciphertext() {
        // §17.2：AAD = CDE(GrantAadV1)，外层签名 = AAD || enc32 || SHA256(hpke_ciphertext)。
        let (signing, public) = keypair();
        let enc = [7u8; 32];
        let ciphertext = b"hpke-ciphertext";
        let ciphertext_sha256 = sha256(ciphertext);
        let aad = proto::GrantAadV1 {
            pv: 1,
            vault_id: [2u8; 16],
            epoch: 3,
            recipient_device_id: [4u8; 16],
            recipient_x25519_pub: [5u8; 32],
            manager_rev: 2,
            grant_id: [6u8; 16],
        };
        let input = proto::grant_signed_bytes(&aad, &enc, &ciphertext_sha256).expect("input");
        let signature = signing.sign(&input).to_bytes().to_vec();
        let grant = crate::sig::EpochGrantInput {
            epoch: 3,
            recipient_device_id: &[4u8; 16],
            recipient_x25519_pub: &[5u8; 32],
            manager_rev: 2,
            grant_id: &[6u8; 16],
            hpke_enc: &enc,
            ciphertext_sha256: &ciphertext_sha256,
        };
        let check = SignatureCheck {
            domain: crate::sig::SignDomain::DeviceApprove,
            vault_id: &[2u8; 16],
            device_id: Some(&[4u8; 16]),
            public_key: &public,
            message: &input,
            signature: &signature,
            expect: &Expectation::EpochGrant { grant: &grant },
        };
        Ed25519Hook.verify(&check).expect("valid grant");

        // 收件人换了 → AAD 变 → 拒绝（防把 grant 转给别的设备）。
        let other_recipient = [9u8; 16];
        let other_grant = crate::sig::EpochGrantInput {
            recipient_device_id: &other_recipient,
            ..grant
        };
        let check_bad = SignatureCheck {
            expect: &Expectation::EpochGrant {
                grant: &other_grant,
            },
            ..check
        };
        assert!(Ed25519Hook.verify(&check_bad).is_err());
    }
    #[test]
    fn device_certificate_binds_domain_vault_and_device() {
        // §17.1：MN/v1/device-cert\0 || CDE(DeviceCertV1)。
        let (signing, public) = keypair();
        let cert = new_certificate(&[1u8; 16], &[2u8; 16], &[3u8; 32], &[4u8; 32], 1);
        let input = proto::device_cert_signed_bytes(&cert).expect("signed bytes");
        assert_eq!(&input[..18], b"MN/v1/device-cert\0");
        let signature = signing.sign(&input).to_bytes().to_vec();
        let check = SignatureCheck {
            domain: crate::sig::SignDomain::DeviceApprove,
            vault_id: &[1u8; 16],
            device_id: Some(&[2u8; 16]),
            public_key: &public,
            message: &input,
            signature: &signature,
            expect: &Expectation::DeviceCert { cert: &cert },
        };
        Ed25519Hook.verify(&check).expect("valid certificate");

        // 换设备（证书内容变）→ 待签字节变 → 签名不再有效。
        let other = new_certificate(&[1u8; 16], &[9u8; 16], &[3u8; 32], &[4u8; 32], 1);
        let check_bad = SignatureCheck {
            expect: &Expectation::DeviceCert { cert: &other },
            ..check
        };
        assert!(Ed25519Hook.verify(&check_bad).is_err());
    }
}
