//! 评审 0002 P1-1.4 / ADR 0006 §23.2、§27.2：完整 activation T 记录（`ActivationTV1`）。
//!
//! 九字段 `ManagerTransitionV1`（relay `rotations::ManagerTransition`）只保护
//! “管理权从旧 key 转到新 key”这一事实；它**不能**替代完整 activation 授权对象，
//! 否则旧管理私钥退役后，仍可能用一份合法的 new-key 转换签名去拼接**另一次**
//! activation 的基线/槽/证书/grant/资产材料。本记录把这次 activation 真正消费的
//! 全部材料收进同一份 canonical CDE，并要求旧、新两把管理 key 对
//!
//! ```text
//! MN/v1/activation-t\0 || CDE(ActivationTV1)
//! ```
//!
//! 双签（域分隔符是一个**真实的 0x00 字节**，与 `MN/v1/manage\0` 等域互不复用）。
//!
//! # 字段表（整数键、不可重排、不可增删；golden vector 锁定）
//!
//! | key | 字段 | wire 类型 | 语义 |
//! | --- | --- | --- | --- |
//! | 0 | `vault_id` | 16B | 库标识 |
//! | 1 | `instance_id` | 16B | relay 实例标识 |
//! | 2 | `rotation_id` | 16B | 本次轮换标识 |
//! | 3 | `from_epoch` | uint | 旧 active epoch |
//! | 4 | `to_epoch` | uint | 待激活候选 epoch |
//! | 5 | `freeze_seq` | uint | 冻结截止 S（= `rotation.freeze_seq`） |
//! | 6 | `old_manager_rev` | uint | 旧管理 key rev |
//! | 7 | `new_manager_rev` | uint | 新管理 key rev（v1 恒为旧 rev + 1） |
//! | 8 | `new_manager_pubkey` | 32B | 新管理 Ed25519 公钥 |
//! | 9 | `prev_control_hash` | 32B | 激活前控制链链尾 `record_hash`（pin，防分叉拼接） |
//! | 10 | `expected_control_rev` | uint | 期望的激活后控制链 rev（CAS） |
//! | 11 | `primary_cid` | 16B | 主基线 checkpoint id |
//! | 12 | `primary_commit_digest` | 32B | 主基线 `CheckpointCommit` 摘要 |
//! | 13 | `fallback_cid` | 16B | 备基线 checkpoint id |
//! | 14 | `fallback_commit_digest` | 32B | 备基线 `CheckpointCommit` 摘要 |
//! | 15 | `slot_set_digest` | 32B | 槽集合 digest（`slot_set_digest` 口径） |
//! | 16 | `member_cert_digest` | 32B | 成员证书集合 digest（`member_cert_digest` 口径） |
//! | 17 | `grants_digest` | 32B | **完整** grants 包 digest（`grant_package_digest` 口径） |
//! | 18 | `old_asset_set_digest` | 32B | 旧 epoch 资产集合 digest |
//! | 19 | `new_asset_set_digest` | 32B | 新 epoch 资产集合 digest |
//!
//! 本模块只做 canonical 编解码与校验，不做 SHA-256、不碰 IO/密钥/存储。

use crate::cde::{
    decode, encode, expect_bytes_n, expect_i64_max, expect_u32, into_record_fields, CdeValue,
};
use crate::constants::{signature_input, SIGN_ACTIVATION_T_DOMAIN};
use crate::error::ProtoError;
use crate::records::{CheckpointId, Ed25519PublicKey, RelayInstanceId, Sha256, VaultId};

/// 字段个数；键必须恰好是 `0..20`，不得增删或重排。
pub const ACTIVATION_T_FIELD_COUNT: usize = 20;

/// 完整 activation T：新旧管理 key 双签的授权对象（评审 0002 P1-1.4）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivationTV1 {
    pub vault_id: VaultId,
    pub instance_id: RelayInstanceId,
    pub rotation_id: [u8; 16],
    pub from_epoch: u64,
    pub to_epoch: u64,
    /// 冻结截止 S；必须等于 `rotation.freeze_seq`。
    pub freeze_seq: u64,
    pub old_manager_rev: u32,
    /// v1 激活后恒为 `old_manager_rev + 1`（relay `bind` 同口径）。
    pub new_manager_rev: u32,
    pub new_manager_pubkey: Ed25519PublicKey,
    /// 激活前控制链链尾 `record_hash`；把 activation 钉在一条确定的控制链上。
    pub prev_control_hash: Sha256,
    pub expected_control_rev: u64,
    pub primary_cid: CheckpointId,
    pub primary_commit_digest: Sha256,
    pub fallback_cid: CheckpointId,
    pub fallback_commit_digest: Sha256,
    pub slot_set_digest: Sha256,
    pub member_cert_digest: Sha256,
    pub grants_digest: Sha256,
    pub old_asset_set_digest: Sha256,
    pub new_asset_set_digest: Sha256,
}

impl ActivationTV1 {
    /// 字段未全零、长度正确、u64 不超 SQLite INTEGER 上界、epoch/rev 单调。
    ///
    /// 刻意**不**要求 `to_epoch == from_epoch + 1`：restart 会保留原 from 并分配更高的
    /// 已废弃候选之后的新 epoch（ADR 0006 §33），因此只要求 `to_epoch > from_epoch`。
    /// 也不单独要求 `freeze_seq > 0`：空库首次激活时 S 可以为 0。
    pub fn validate(&self) -> Result<(), ProtoError> {
        for (name, value) in [
            ("ActivationTV1.vault_id", &self.vault_id[..]),
            ("ActivationTV1.instance_id", &self.instance_id[..]),
            ("ActivationTV1.rotation_id", &self.rotation_id[..]),
            (
                "ActivationTV1.new_manager_pubkey",
                &self.new_manager_pubkey[..],
            ),
            (
                "ActivationTV1.prev_control_hash",
                &self.prev_control_hash[..],
            ),
            ("ActivationTV1.primary_cid", &self.primary_cid[..]),
            (
                "ActivationTV1.primary_commit_digest",
                &self.primary_commit_digest[..],
            ),
            ("ActivationTV1.fallback_cid", &self.fallback_cid[..]),
            (
                "ActivationTV1.fallback_commit_digest",
                &self.fallback_commit_digest[..],
            ),
            ("ActivationTV1.slot_set_digest", &self.slot_set_digest[..]),
            (
                "ActivationTV1.member_cert_digest",
                &self.member_cert_digest[..],
            ),
            ("ActivationTV1.grants_digest", &self.grants_digest[..]),
            (
                "ActivationTV1.old_asset_set_digest",
                &self.old_asset_set_digest[..],
            ),
            (
                "ActivationTV1.new_asset_set_digest",
                &self.new_asset_set_digest[..],
            ),
        ] {
            if value.iter().all(|byte| *byte == 0) {
                return Err(ProtoError::Invalid(format!("{name} 不能全零")));
            }
        }

        for (name, value) in [
            ("ActivationTV1.from_epoch", self.from_epoch),
            ("ActivationTV1.to_epoch", self.to_epoch),
            ("ActivationTV1.freeze_seq", self.freeze_seq),
            (
                "ActivationTV1.expected_control_rev",
                self.expected_control_rev,
            ),
        ] {
            if value > i64::MAX as u64 {
                return Err(ProtoError::Invalid(format!("{name} 超过 i64::MAX")));
            }
        }

        if self.from_epoch == 0 {
            return Err(ProtoError::Invalid(
                "ActivationTV1.from_epoch 必须 >0".to_owned(),
            ));
        }
        if self.to_epoch <= self.from_epoch {
            return Err(ProtoError::Invalid(format!(
                "ActivationTV1.to_epoch={} 必须大于 from_epoch={}",
                self.to_epoch, self.from_epoch
            )));
        }
        if self.old_manager_rev == 0 {
            return Err(ProtoError::Invalid(
                "ActivationTV1.old_manager_rev 必须 >0".to_owned(),
            ));
        }
        let Some(expected_new_manager_rev) = self.old_manager_rev.checked_add(1) else {
            return Err(ProtoError::Invalid(format!(
                "ActivationTV1.old_manager_rev={} 无法加 1",
                self.old_manager_rev
            )));
        };
        if self.new_manager_rev != expected_new_manager_rev {
            return Err(ProtoError::Invalid(format!(
                "ActivationTV1.new_manager_rev={} 必须等于 old_manager_rev+1={expected_new_manager_rev}",
                self.new_manager_rev
            )));
        }
        if self.expected_control_rev == 0 {
            return Err(ProtoError::Invalid(
                "ActivationTV1.expected_control_rev 必须 >0".to_owned(),
            ));
        }
        Ok(())
    }

    /// 映射到整数键 map（键 `0..20`）。
    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.validate()?;
        Ok(CdeValue::Map(vec![
            (0, CdeValue::Bytes(self.vault_id.to_vec())),
            (1, CdeValue::Bytes(self.instance_id.to_vec())),
            (2, CdeValue::Bytes(self.rotation_id.to_vec())),
            (3, CdeValue::UInt(self.from_epoch)),
            (4, CdeValue::UInt(self.to_epoch)),
            (5, CdeValue::UInt(self.freeze_seq)),
            (6, CdeValue::UInt(u64::from(self.old_manager_rev))),
            (7, CdeValue::UInt(u64::from(self.new_manager_rev))),
            (8, CdeValue::Bytes(self.new_manager_pubkey.to_vec())),
            (9, CdeValue::Bytes(self.prev_control_hash.to_vec())),
            (10, CdeValue::UInt(self.expected_control_rev)),
            (11, CdeValue::Bytes(self.primary_cid.to_vec())),
            (12, CdeValue::Bytes(self.primary_commit_digest.to_vec())),
            (13, CdeValue::Bytes(self.fallback_cid.to_vec())),
            (14, CdeValue::Bytes(self.fallback_commit_digest.to_vec())),
            (15, CdeValue::Bytes(self.slot_set_digest.to_vec())),
            (16, CdeValue::Bytes(self.member_cert_digest.to_vec())),
            (17, CdeValue::Bytes(self.grants_digest.to_vec())),
            (18, CdeValue::Bytes(self.old_asset_set_digest.to_vec())),
            (19, CdeValue::Bytes(self.new_asset_set_digest.to_vec())),
        ]))
    }

    /// 从 CDE map 解码；字段数必须恰好 20，键必须是 `0..20` 的连续升序。
    pub fn from_cde(value: CdeValue) -> Result<Self, ProtoError> {
        let fields = into_record_fields(value, ACTIVATION_T_FIELD_COUNT)?;
        let record = Self {
            vault_id: expect_bytes_n::<16>(fields[0].clone(), "ActivationTV1.vault_id")?,
            instance_id: expect_bytes_n::<16>(fields[1].clone(), "ActivationTV1.instance_id")?,
            rotation_id: expect_bytes_n::<16>(fields[2].clone(), "ActivationTV1.rotation_id")?,
            from_epoch: expect_i64_max(fields[3].clone(), "ActivationTV1.from_epoch")?,
            to_epoch: expect_i64_max(fields[4].clone(), "ActivationTV1.to_epoch")?,
            freeze_seq: expect_i64_max(fields[5].clone(), "ActivationTV1.freeze_seq")?,
            old_manager_rev: expect_u32(fields[6].clone(), "ActivationTV1.old_manager_rev")?,
            new_manager_rev: expect_u32(fields[7].clone(), "ActivationTV1.new_manager_rev")?,
            new_manager_pubkey: expect_bytes_n::<32>(
                fields[8].clone(),
                "ActivationTV1.new_manager_pubkey",
            )?,
            prev_control_hash: expect_bytes_n::<32>(
                fields[9].clone(),
                "ActivationTV1.prev_control_hash",
            )?,
            expected_control_rev: expect_i64_max(
                fields[10].clone(),
                "ActivationTV1.expected_control_rev",
            )?,
            primary_cid: expect_bytes_n::<16>(fields[11].clone(), "ActivationTV1.primary_cid")?,
            primary_commit_digest: expect_bytes_n::<32>(
                fields[12].clone(),
                "ActivationTV1.primary_commit_digest",
            )?,
            fallback_cid: expect_bytes_n::<16>(fields[13].clone(), "ActivationTV1.fallback_cid")?,
            fallback_commit_digest: expect_bytes_n::<32>(
                fields[14].clone(),
                "ActivationTV1.fallback_commit_digest",
            )?,
            slot_set_digest: expect_bytes_n::<32>(
                fields[15].clone(),
                "ActivationTV1.slot_set_digest",
            )?,
            member_cert_digest: expect_bytes_n::<32>(
                fields[16].clone(),
                "ActivationTV1.member_cert_digest",
            )?,
            grants_digest: expect_bytes_n::<32>(fields[17].clone(), "ActivationTV1.grants_digest")?,
            old_asset_set_digest: expect_bytes_n::<32>(
                fields[18].clone(),
                "ActivationTV1.old_asset_set_digest",
            )?,
            new_asset_set_digest: expect_bytes_n::<32>(
                fields[19].clone(),
                "ActivationTV1.new_asset_set_digest",
            )?,
        };
        record.validate()?;
        Ok(record)
    }

    /// canonical 编码（唯一字节串）。
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    /// canonical 解码；拒绝未知键、缺键、错序、错误长度与尾部多余字节。
    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        Self::from_cde(decode(bytes)?)
    }

    /// `MN/v1/activation-t\0 || CDE(self)`。
    pub fn signing_input(&self) -> Result<Vec<u8>, ProtoError> {
        activation_t_signing_input(self)
    }
}

/// 完整 activation T 的双签输入：`MN/v1/activation-t\0 || CDE(T)`。
///
/// 专用域，不与 `MN/v1/manage\0` / `MN/v1/device-cert\0` / `MN/v1/checkpoint\0`
/// 等任何既有签名域复用（AAD/域分离是硬约束，见 ADR 0006 §14）。
pub fn activation_t_signing_input(record: &ActivationTV1) -> Result<Vec<u8>, ProtoError> {
    Ok(signature_input(SIGN_ACTIVATION_T_DOMAIN, &record.encode()?))
}
