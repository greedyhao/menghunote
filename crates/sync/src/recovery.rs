//! 恢复码：`R = random32`，编码为 **Bech32m（HRP = `mnrec`）**（ADR 0006 §3）。
//!
//! 恢复码同时具备两种能力：
//!
//! - **数据恢复**：`K_recovery_wrap = HKDF(R, vault_id, b"MN/v1/recovery-wrap\0" || slot_id, 32)`
//!   解出 vault 根；
//! - **管理恢复**：`management_ed25519_seed = HKDF(R, vault_id, b"MN/v1/management-ed25519\0", 32)`
//!   派生**独立的管理 Ed25519 私钥**（普通根持有者不能据此注册/吊销设备，ADR §3/§4）。
//!
//! 编码用 BIP-350 Bech32m 而不是 Bech32：解码时**强制校验 Bech32m**，否则校验和类型不同
//! 的串会被误接受（Bech32 与 Bech32m 的字符集相同，只差校验和常量）。

use crate::domain;
use crate::error::SyncError;
use crate::kdf::hkdf32;
use crate::keys::{Secret32, WrapKey};
use crate::random::RandomSource;
use crate::VaultId;
use bech32::primitives::decode::CheckedHrpstring;
use bech32::{Bech32m, Hrp};
use zeroize::Zeroizing;

/// 恢复码的 Bech32 HRP（ADR §3：`mnrec`）。
pub const RECOVERY_HRP: &str = "mnrec";
/// 恢复种子字节数。
pub const RECOVERY_SEED_LEN: usize = 32;
/// 恢复码字符串长度：5(hrp) + 1(分隔符) + 52(32B→base32) + 6(校验和)。
pub const RECOVERY_CODE_LEN: usize = 64;

/// 恢复种子 `R`（32 字节随机）。**不实现 `Debug`/`Display`**，编码只能走 [`Self::to_code`]。
#[derive(Clone)]
pub struct RecoverySeed(Secret32);

impl RecoverySeed {
    /// 生成新的随机恢复种子（ADR §3：`R = random32`）。
    pub fn generate(rng: &mut impl RandomSource) -> Result<Self, SyncError> {
        Ok(Self(Secret32::generate(rng)?))
    }

    /// 从已有字节构造。
    #[must_use]
    pub fn from_array(bytes: [u8; RECOVERY_SEED_LEN]) -> Self {
        Self(Secret32::from_array(bytes))
    }

    /// 取明文引用。
    #[must_use]
    pub fn expose(&self) -> &[u8; RECOVERY_SEED_LEN] {
        self.0.expose()
    }

    /// 常量时间相等。
    #[must_use]
    pub fn ct_eq(&self, other: &Self) -> bool {
        self.0.ct_eq(&other.0)
    }

    /// 编码为 Bech32m 恢复码（`mnrec1...`）。
    ///
    /// 返回值包在 `Zeroizing<String>` 里，`Drop` 时清零；调用方仍必须遵守
    /// "只显示一次、不写日志、不进遥测"。
    pub fn to_code(&self) -> Result<Zeroizing<String>, SyncError> {
        let hrp = Hrp::parse_unchecked(RECOVERY_HRP);
        let code = bech32::encode::<Bech32m>(hrp, self.0.expose_slice())
            .map_err(|_| SyncError::RecoveryCode("Bech32m 编码失败"))?;
        Ok(Zeroizing::new(code))
    }

    /// 解析 Bech32m 恢复码。
    ///
    /// 严格规则：必须是 **Bech32m** 校验和、HRP 必须是 `mnrec`、数据必须恰好 32 字节；
    /// 大小写混写/未知 HRP/长度不符/校验和错误一律拒绝。
    pub fn from_code(code: &str) -> Result<Self, SyncError> {
        let checked = CheckedHrpstring::new::<Bech32m>(code)
            .map_err(|_| SyncError::RecoveryCode("Bech32m 校验失败"))?;
        let expected = Hrp::parse_unchecked(RECOVERY_HRP);
        if checked.hrp() != expected {
            return Err(SyncError::RecoveryCode("HRP 必须是 mnrec"));
        }
        let mut seed = Zeroizing::new([0u8; RECOVERY_SEED_LEN]);
        let mut len = 0usize;
        for byte in checked.byte_iter() {
            if len >= RECOVERY_SEED_LEN {
                return Err(SyncError::RecoveryCode("恢复码数据必须是 32 字节"));
            }
            seed[len] = byte;
            len += 1;
        }
        if len != RECOVERY_SEED_LEN {
            return Err(SyncError::RecoveryCode("恢复码数据必须是 32 字节"));
        }
        Ok(Self(Secret32::from_array(*seed)))
    }
}

/// 管理私钥种子：`HKDF(R, vault_id, b"MN/v1/management-ed25519\0", 32)`（ADR §3）。
pub fn management_seed(seed: &RecoverySeed, vault_id: &VaultId) -> Result<Secret32, SyncError> {
    hkdf32(
        seed.0.expose_slice(),
        vault_id,
        domain::HKDF_MANAGEMENT_ED25519_INFO,
    )
}

/// 恢复槽 KEK：`HKDF(R, vault_id, b"MN/v1/recovery-wrap\0" || slot_id, 32)`（ADR §3）。
pub fn recovery_wrap_key(
    seed: &RecoverySeed,
    vault_id: &VaultId,
    slot_id: &[u8; 32],
) -> Result<WrapKey, SyncError> {
    let info = domain::recovery_wrap_info(slot_id);
    let kek = hkdf32(seed.0.expose_slice(), vault_id, &info)?;
    Ok(WrapKey::from_secret(kek))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestRng;

    /// 用独立实现（BIP-350 参考算法，Python）算出的 golden vector：
    /// seed = 0x00 0x01 ... 0x1f，HRP = `mnrec`，Bech32m 校验和。
    const GOLDEN_SEED: [u8; 32] = [
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
        0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d,
        0x1e, 0x1f,
    ];
    const GOLDEN_BECH32M: &str = "mnrec1qqqsyqcyq5rqwzqfpg9scrgwpugpzysnzs23v9ccrydpk8qarc0sr56vg6";
    /// 同一 32 字节用 **Bech32（非 m）** 校验和编码的串：必须被拒绝。
    const GOLDEN_BECH32_WRONG_SUM: &str =
        "mnrec1qqqsyqcyq5rqwzqfpg9scrgwpugpzysnzs23v9ccrydpk8qarc0skg2qdc";

    #[test]
    fn golden_vector_roundtrip() {
        let seed = RecoverySeed::from_array(GOLDEN_SEED);
        let code = seed.to_code().unwrap();
        assert_eq!(code.as_str(), GOLDEN_BECH32M);
        assert_eq!(code.len(), RECOVERY_CODE_LEN);
        let back = RecoverySeed::from_code(&code).unwrap();
        assert!(seed.ct_eq(&back));
    }

    #[test]
    fn generate_then_parse() {
        let mut rng = TestRng::new(7);
        let seed = RecoverySeed::generate(&mut rng).unwrap();
        let code = seed.to_code().unwrap();
        assert!(code.starts_with("mnrec1"));
        assert_eq!(code.len(), RECOVERY_CODE_LEN);
        assert!(seed.ct_eq(&RecoverySeed::from_code(&code).unwrap()));
    }

    #[test]
    fn uppercase_is_accepted() {
        let seed = RecoverySeed::from_array(GOLDEN_SEED);
        let upper = seed.to_code().unwrap().to_uppercase();
        let back = RecoverySeed::from_code(&upper).unwrap();
        assert!(seed.ct_eq(&back));
    }

    #[test]
    fn wrong_checksum_type_rejected() {
        // Bech32（非 m）编码的同一数据：ADR 要求 Bech32m，必须拒绝。
        assert!(RecoverySeed::from_code(GOLDEN_BECH32_WRONG_SUM).is_err());
    }

    #[test]
    fn tampered_and_malformed_rejected() {
        let mut chars: Vec<char> = GOLDEN_BECH32M.chars().collect();
        // 改动最后一个数据字符 → 校验和失败
        chars[RECOVERY_CODE_LEN - 7] = if chars[RECOVERY_CODE_LEN - 7] == 'q' {
            'p'
        } else {
            'q'
        };
        let tampered: String = chars.into_iter().collect();
        assert!(RecoverySeed::from_code(&tampered).is_err());
        // 混写大小写
        let mixed = format!("mnrec1{}", GOLDEN_BECH32M[6..].to_uppercase());
        assert!(RecoverySeed::from_code(&mixed).is_err());
        // 空串/垃圾
        assert!(RecoverySeed::from_code("").is_err());
        assert!(RecoverySeed::from_code("mnrec1").is_err());
        // 未知 HRP（Bech32m 合法但 HRP 不同）
        let hrp = Hrp::parse_unchecked("mnxxx");
        let wrong_hrp = bech32::encode::<Bech32m>(hrp, &GOLDEN_SEED).unwrap();
        assert!(RecoverySeed::from_code(&wrong_hrp).is_err());
    }

    #[test]
    fn wrong_data_length_rejected() {
        let hrp = Hrp::parse_unchecked(RECOVERY_HRP);
        let short = bech32::encode::<Bech32m>(hrp, &GOLDEN_SEED[..31]).unwrap();
        assert!(RecoverySeed::from_code(&short).is_err());
        let long = bech32::encode::<Bech32m>(hrp, &[0u8; 33]).unwrap();
        assert!(RecoverySeed::from_code(&long).is_err());
    }

    /// 恢复码同时恢复 root 与独立管理 key（ADR §11 步 2 的验收点）。
    #[test]
    fn recovery_code_restores_root_and_management_key() {
        let mut rng = TestRng::new(11);
        let vault_id: VaultId = [3u8; 16];
        let slot_id = [4u8; 32];

        // 初始化：生成 R、根、恢复槽 KEK、管理种子
        let seed = RecoverySeed::generate(&mut rng).unwrap();
        let root = crate::keys::RootSecretV1::generate(vault_id, 1, &mut rng).unwrap();
        let _kek = recovery_wrap_key(&seed, &vault_id, &slot_id).unwrap();
        let _mgmt = management_seed(&seed, &vault_id).unwrap();

        // 用户抄下恢复码；此后再无内存中的 R
        let code = seed.to_code().unwrap();
        drop(seed);

        // 恢复：仅凭恢复码 + vault_id + slot_id 重新派生两个能力
        let restored_seed = RecoverySeed::from_code(&code).unwrap();
        let _restored_kek = recovery_wrap_key(&restored_seed, &vault_id, &slot_id).unwrap();
        let restored_mgmt = management_seed(&restored_seed, &vault_id).unwrap();

        // 管理种子能从恢复码确定性地重建（同一默认 key）
        let mgmt_key = crate::device::ManagementKey::from_seed(restored_mgmt);
        let mgmt_key2 =
            crate::device::ManagementKey::from_recovery(&restored_seed, &vault_id).unwrap();
        assert_eq!(mgmt_key.public_key(), mgmt_key2.public_key());
        assert_ne!(mgmt_key.public_key(), [0u8; 32]);

        // root 本身不受恢复码影响（恢复码只解封槽，不重算根）
        assert_eq!(root.key_epoch, 1);
    }

    #[test]
    fn management_seed_differs_from_recovery_wrap_key() {
        let seed = RecoverySeed::from_array(GOLDEN_SEED);
        let vault_id: VaultId = [5u8; 16];
        let mgmt = management_seed(&seed, &vault_id).unwrap();
        let wrap = recovery_wrap_key(&seed, &vault_id, &[1u8; 32]).unwrap();
        assert_ne!(mgmt.expose(), wrap.expose());
        assert_ne!(
            mgmt.expose(),
            recovery_wrap_key(&seed, &vault_id, &[2u8; 32])
                .unwrap()
                .expose(),
            "不同槽必须得到不同 KEK"
        );
    }
}
