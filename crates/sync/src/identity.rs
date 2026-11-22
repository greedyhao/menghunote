//! 本地身份 BLOB（ADR 0006 §3），只以 DPAPI 保护后落盘。
//!
//! 结构（整数键 0..11，optional 省略，编码用 `sync-proto` 的 CDE）：
//!
//! ```text
//! 0 vault_id           16B
//! 1 device_id          16B
//! 2 peer_id            16B   （Loro peer；克隆/恢复入口必须换新）
//! 3 ed25519_seed       32B   （设备签名私钥种子）
//! 4 x25519_secret      32B   （HPKE 私钥材料）
//! 5 current_epoch      UInt
//! 6 current_root       32B   （当前 epoch 根）
//! 7 manager_seed?      32B   （仅管理设备）
//! 8 pinned_instance    16B   （relay instance id，防换实例/回滚）
//! 9 pinned_manager     32B   （管理公钥）
//! 10 control_rev       UInt
//! 11 pending_rotation? map   （轮换中的候选根材料，ADR §8 步 1）
//! ```
//!
//! 这是**本地**格式（DPAPI 内层，不上 wire），但沿用 CDE 编码规则：确定性、整数键升序、
//! 定长字节用 byte string。秘密字段在编码后立即擦除中间 `CdeValue` 副本；比 wire 记录
//! 多一道零化，因为这里装的都是长期私钥。
//!
//! 跨用户/跨 SID 的 DPAPI 失败是预期行为，见 [`crate::dpapi`] 模块文档。

use crate::dpapi;
use crate::error::SyncError;
use crate::ids::PeerId;
use crate::keys::Secret32;
use crate::{DeviceId, VaultId};
use core::iter::Peekable;
use menghunote_sync_proto::CdeValue;
use std::vec::IntoIter;
use zeroize::Zeroizing;

/// 轮换中的候选材料（ADR §8 步 1：先 durable 保存 pending root/槽/grant 材料）。
///
/// 本地结构，字段可扩展；`candidate_root` 与正式根一样按秘密处理。
#[derive(Clone)]
pub struct PendingRotationV1 {
    /// 目标 epoch（单调递增，废弃 epoch 永不复用）。
    pub to_epoch: u64,
    /// relay 侧的 rotation ID（16B）。
    pub rotation_id: [u8; 16],
    /// 候选 epoch 的随机根（**绝不能用旧根/旧 password/旧 R 封装**）。
    pub candidate_root: Secret32,
    /// 落盘时刻（Unix 秒），用于过期回收。
    pub staged_at: u64,
}

impl PendingRotationV1 {
    fn to_cde(&self) -> CdeValue {
        CdeValue::Map(vec![
            (0, CdeValue::UInt(self.to_epoch)),
            (1, CdeValue::Bytes(self.rotation_id.to_vec())),
            (
                2,
                CdeValue::Bytes(self.candidate_root.expose_slice().to_vec()),
            ),
            (3, CdeValue::UInt(self.staged_at)),
        ])
    }

    fn from_cde(value: CdeValue) -> Result<Self, SyncError> {
        let mut fields = crate::wire::expect_exact::<4>(crate::wire::entries_of(value)?)?;
        Ok(Self {
            to_epoch: crate::wire::expect_uint(fields.remove(0), "pending.to_epoch")?,
            rotation_id: crate::wire::expect_bytes::<16>(fields.remove(0), "pending.rotation_id")?,
            candidate_root: crate::wire::expect_secret32(fields.remove(0), "pending.root")?,
            staged_at: crate::wire::expect_uint(fields.remove(0), "pending.staged_at")?,
        })
    }
}

/// DPAPI 包裹的本地身份（ADR §3 的字段清单）。
///
/// 不含 `Debug` 派生：手写 `Debug` 只打印非秘密字段，秘密一律 `<redacted>`。
pub struct LocalIdentityV1 {
    /// vault 身份。
    pub vault_id: VaultId,
    /// 设备身份。
    pub device_id: DeviceId,
    /// Loro peer id。
    pub peer_id: PeerId,
    /// Ed25519 私钥种子。
    pub ed25519_seed: Secret32,
    /// X25519 HPKE 私钥材料。
    pub x25519_secret: Secret32,
    /// 当前密钥代。
    pub current_epoch: u64,
    /// 当前 epoch 根。
    pub current_root: Secret32,
    /// 管理私钥种子（仅管理设备有）。
    pub manager_seed: Option<Secret32>,
    /// pin 住的 relay instance（防换实例/静默回滚）。
    pub pinned_instance: [u8; 16],
    /// pin 住的管理公钥。
    pub pinned_manager: [u8; 32],
    /// 管理控制链版本。
    pub control_rev: u64,
    /// 轮换进行中的候选材料。
    pub pending_rotation: Option<PendingRotationV1>,
}

impl LocalIdentityV1 {
    /// DPAPI 可选 entropy：`b"MN/dpapi/v1\0" || vault_id || device_id`。
    #[must_use]
    pub fn entropy(&self) -> Vec<u8> {
        dpapi::entropy_for(&self.vault_id, &self.device_id)
    }

    /// 编码为 CDE；编码后用 [`crate::wire::zeroize_value`] 擦掉中间副本。
    pub fn encode(&self) -> Result<Vec<u8>, SyncError> {
        let mut entries: Vec<(u64, CdeValue)> = vec![
            (0, CdeValue::Bytes(self.vault_id.to_vec())),
            (1, CdeValue::Bytes(self.device_id.to_vec())),
            (2, CdeValue::Bytes(self.peer_id.as_bytes().to_vec())),
            (
                3,
                CdeValue::Bytes(self.ed25519_seed.expose_slice().to_vec()),
            ),
            (
                4,
                CdeValue::Bytes(self.x25519_secret.expose_slice().to_vec()),
            ),
            (5, CdeValue::UInt(self.current_epoch)),
            (
                6,
                CdeValue::Bytes(self.current_root.expose_slice().to_vec()),
            ),
        ];
        if let Some(manager_seed) = &self.manager_seed {
            entries.push((7, CdeValue::Bytes(manager_seed.expose_slice().to_vec())));
        }
        entries.push((8, CdeValue::Bytes(self.pinned_instance.to_vec())));
        entries.push((9, CdeValue::Bytes(self.pinned_manager.to_vec())));
        entries.push((10, CdeValue::UInt(self.control_rev)));
        if let Some(pending) = &self.pending_rotation {
            entries.push((11, pending.to_cde()));
        }
        let mut value = CdeValue::Map(entries);
        let encoded = crate::wire::encode(&value)?;
        // 中间 CdeValue 里是长期私钥：编码完立刻擦除。
        crate::wire::zeroize_value(&mut value);
        Ok(encoded)
    }

    /// 解析 CDE；未知键/缺字段/归属不符一律拒绝。
    pub fn decode(bytes: &[u8]) -> Result<Self, SyncError> {
        let entries = crate::wire::decode_entries(bytes)?;
        let mut it: Peekable<IntoIter<(u64, CdeValue)>> = entries.into_iter().peekable();

        let vault_id =
            crate::wire::expect_bytes::<16>(take_required(&mut it, 0)?, "identity.vault_id")?;
        let device_id =
            crate::wire::expect_bytes::<16>(take_required(&mut it, 1)?, "identity.device_id")?;
        let peer_id = PeerId::from_bytes(crate::wire::expect_bytes::<16>(
            take_required(&mut it, 2)?,
            "identity.peer_id",
        )?);
        let ed25519_seed =
            crate::wire::expect_secret32(take_required(&mut it, 3)?, "identity.ed25519_seed")?;
        let x25519_secret =
            crate::wire::expect_secret32(take_required(&mut it, 4)?, "identity.x25519_secret")?;
        let current_epoch =
            crate::wire::expect_uint(take_required(&mut it, 5)?, "identity.current_epoch")?;
        let current_root =
            crate::wire::expect_secret32(take_required(&mut it, 6)?, "identity.current_root")?;
        let manager_seed = match take_optional(&mut it, 7)? {
            Some(value) => Some(crate::wire::expect_secret32(
                value,
                "identity.manager_seed",
            )?),
            None => None,
        };
        let pinned_instance = crate::wire::expect_bytes::<16>(
            take_required(&mut it, 8)?,
            "identity.pinned_instance",
        )?;
        let pinned_manager =
            crate::wire::expect_bytes::<32>(take_required(&mut it, 9)?, "identity.pinned_manager")?;
        let control_rev =
            crate::wire::expect_uint(take_required(&mut it, 10)?, "identity.control_rev")?;
        let pending_rotation = match take_optional(&mut it, 11)? {
            Some(value) => Some(PendingRotationV1::from_cde(value)?),
            None => None,
        };
        if let Some((key, _)) = it.next() {
            return Err(SyncError::Cde(match key {
                7 => "可选字段 7 位置错误",
                11 => "可选字段 11 位置错误",
                _ => "身份 BLOB 含未知字段",
            }));
        }
        Ok(Self {
            vault_id,
            device_id,
            peer_id,
            ed25519_seed,
            x25519_secret,
            current_epoch,
            current_root,
            manager_seed,
            pinned_instance,
            pinned_manager,
            control_rev,
            pending_rotation,
        })
    }

    /// 用 DPAPI（CurrentUser）保护整份身份。
    ///
    /// 明文序列化缓冲包在 `Zeroizing` 里，加密后立即清零。
    pub fn protect(&self) -> Result<Vec<u8>, SyncError> {
        let plaintext = Zeroizing::new(self.encode()?);
        dpapi::protect(&plaintext, &self.entropy())
    }

    /// 解封 DPAPI BLOB 并解析；`vault_id`/`device_id` 必须与 BLOB 内容一致
    /// （entropy 已经绑定它们，这里再校验一次结构，防错配）。
    pub fn unprotect(
        blob: &[u8],
        vault_id: &VaultId,
        device_id: &DeviceId,
    ) -> Result<Self, SyncError> {
        let plaintext = dpapi::unprotect(blob, &dpapi::entropy_for(vault_id, device_id))?;
        let identity = Self::decode(&plaintext)?;
        if identity.vault_id != *vault_id || identity.device_id != *device_id {
            return Err(SyncError::Identity(
                "身份 BLOB 的 vault/device 与期望不一致",
            ));
        }
        Ok(identity)
    }
}

impl core::fmt::Debug for LocalIdentityV1 {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("LocalIdentityV1")
            .field("vault_id", &crate::ids::hex_of(&self.vault_id))
            .field("device_id", &crate::ids::hex_of(&self.device_id))
            .field("peer_id", &self.peer_id)
            .field("ed25519_seed", &crate::keys::redacted())
            .field("x25519_secret", &crate::keys::redacted())
            .field("current_epoch", &self.current_epoch)
            .field("current_root", &crate::keys::redacted())
            .field(
                "manager_seed",
                &self.manager_seed.as_ref().map(|_| crate::keys::redacted()),
            )
            .field(
                "pinned_instance",
                &crate::ids::hex_of(&self.pinned_instance),
            )
            .field("pinned_manager", &crate::ids::hex_of(&self.pinned_manager))
            .field("control_rev", &self.control_rev)
            .field(
                "pending_rotation",
                &self.pending_rotation.as_ref().map(|p| p.to_epoch),
            )
            .finish()
    }
}

fn take_required(
    it: &mut Peekable<IntoIter<(u64, CdeValue)>>,
    key: u64,
) -> Result<CdeValue, SyncError> {
    take_optional(it, key)?.ok_or(SyncError::Cde("身份 BLOB 缺少必需字段"))
}

fn take_optional(
    it: &mut Peekable<IntoIter<(u64, CdeValue)>>,
    key: u64,
) -> Result<Option<CdeValue>, SyncError> {
    match it.peek() {
        Some((k, _)) if *k == key => Ok(it.next().map(|(_, value)| value)),
        Some((k, _)) if *k < key => Err(SyncError::Cde("身份 BLOB 字段乱序")),
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestRng;

    fn fixture(with_manager: bool, with_pending: bool, seed: u64) -> LocalIdentityV1 {
        let mut rng = TestRng::new(seed);
        LocalIdentityV1 {
            vault_id: [0x10; 16],
            device_id: [0x20; 16],
            peer_id: PeerId::from_bytes([0x30; 16]),
            ed25519_seed: Secret32::generate(&mut rng).unwrap(),
            x25519_secret: Secret32::generate(&mut rng).unwrap(),
            current_epoch: 7,
            current_root: Secret32::generate(&mut rng).unwrap(),
            manager_seed: if with_manager {
                Some(Secret32::generate(&mut rng).unwrap())
            } else {
                None
            },
            pinned_instance: [0x40; 16],
            pinned_manager: [0x50; 32],
            control_rev: 11,
            pending_rotation: if with_pending {
                Some(PendingRotationV1 {
                    to_epoch: 8,
                    rotation_id: [0x60; 16],
                    candidate_root: Secret32::generate(&mut rng).unwrap(),
                    staged_at: 1_700_000_000,
                })
            } else {
                None
            },
        }
    }

    #[test]
    fn roundtrip_all_optional_shapes() {
        for (with_manager, with_pending) in
            [(false, false), (true, false), (false, true), (true, true)]
        {
            let identity = fixture(with_manager, with_pending, 71);
            let bytes = identity.encode().unwrap();
            let back = LocalIdentityV1::decode(&bytes).unwrap();
            assert_eq!(back.vault_id, identity.vault_id);
            assert_eq!(back.device_id, identity.device_id);
            assert_eq!(back.peer_id, identity.peer_id);
            assert_eq!(back.current_epoch, 7);
            assert_eq!(back.control_rev, 11);
            assert!(back.ed25519_seed.ct_eq(&identity.ed25519_seed));
            assert!(back.x25519_secret.ct_eq(&identity.x25519_secret));
            assert!(back.current_root.ct_eq(&identity.current_root));
            assert_eq!(back.manager_seed.is_some(), with_manager);
            assert_eq!(back.pending_rotation.is_some(), with_pending);
            if let (Some(a), Some(b)) = (&back.pending_rotation, &identity.pending_rotation) {
                assert_eq!(a.to_epoch, b.to_epoch);
                assert_eq!(a.rotation_id, b.rotation_id);
                assert_eq!(a.staged_at, b.staged_at);
                assert!(a.candidate_root.ct_eq(&b.candidate_root));
            }
            // 编码确定性
            assert_eq!(bytes, back.encode().unwrap());
        }
    }

    #[test]
    fn field_count_matches_optionals() {
        // 10 项（无 optional）
        let bytes = fixture(false, false, 72).encode().unwrap();
        assert_eq!(bytes[0], 0xaa);
        // 11 项（一个 optional）
        let bytes = fixture(true, false, 73).encode().unwrap();
        assert_eq!(bytes[0], 0xab);
        let bytes = fixture(false, true, 74).encode().unwrap();
        assert_eq!(bytes[0], 0xab);
        // 12 项（两个 optional）
        let bytes = fixture(true, true, 75).encode().unwrap();
        assert_eq!(bytes[0], 0xac);
    }

    #[test]
    fn unknown_or_truncated_rejected() {
        let identity = fixture(true, true, 76);
        let bytes = identity.encode().unwrap();
        // 截断
        assert!(LocalIdentityV1::decode(&bytes[..bytes.len() - 1]).is_err());
        // 尾部多余字节
        let mut long = bytes.clone();
        long.push(0);
        assert!(LocalIdentityV1::decode(&long).is_err());
        // 未知字段：在合法记录末尾追加键 12，必须被拒绝（v1 严格解码）
        let mut entries = crate::wire::decode_entries(&bytes).unwrap();
        entries.push((12, CdeValue::UInt(0)));
        let bad = crate::wire::encode(&CdeValue::Map(entries)).unwrap();
        assert!(LocalIdentityV1::decode(&bad).is_err());
    }

    #[test]
    fn required_field_missing_rejected() {
        // 手工构造只有 6 项（缺 manager/pinned/control）的 map
        let value = CdeValue::Map(vec![
            (0, CdeValue::Bytes(vec![0x10; 16])),
            (1, CdeValue::Bytes(vec![0x20; 16])),
            (2, CdeValue::Bytes(vec![0x30; 16])),
            (3, CdeValue::Bytes(vec![0x40; 32])),
            (4, CdeValue::Bytes(vec![0x50; 32])),
            (5, CdeValue::UInt(7)),
            (6, CdeValue::Bytes(vec![0x60; 32])),
        ]);
        let bytes = crate::wire::encode(&value).unwrap();
        assert!(LocalIdentityV1::decode(&bytes).is_err());
    }

    #[test]
    fn debug_redacts_all_secrets() {
        let identity = fixture(true, true, 77);
        let text = format!("{identity:?}");
        assert!(text.contains("<redacted>"));
        // 任何秘密的十六进制都不能出现在 Debug 输出里
        for secret in [
            &identity.ed25519_seed,
            &identity.x25519_secret,
            &identity.current_root,
            identity.manager_seed.as_ref().unwrap(),
            &identity.pending_rotation.as_ref().unwrap().candidate_root,
        ] {
            assert!(
                !text.contains(&crate::ids::hex_of(secret.expose_slice())),
                "Debug 输出泄露了秘密: {text}"
            );
        }
        // 非秘密字段可见（便于排障）
        assert!(text.contains(&crate::ids::hex_of(&identity.device_id)));
        assert!(text.contains("current_epoch"));
    }

    #[test]
    fn entropy_binds_vault_and_device() {
        let identity = fixture(false, false, 78);
        let entropy = identity.entropy();
        assert_eq!(
            entropy,
            dpapi::entropy_for(&identity.vault_id, &identity.device_id)
        );
        assert_ne!(
            entropy,
            dpapi::entropy_for(&[0x11; 16], &identity.device_id)
        );
    }

    #[cfg(windows)]
    #[test]
    fn dpapi_identity_roundtrip() {
        let identity = fixture(true, true, 79);
        let blob = identity.protect().unwrap();
        assert!(!blob.is_empty());
        let opened =
            LocalIdentityV1::unprotect(&blob, &identity.vault_id, &identity.device_id).unwrap();
        assert_eq!(opened.device_id, identity.device_id);
        assert!(opened.current_root.ct_eq(&identity.current_root));
        assert!(opened
            .manager_seed
            .as_ref()
            .unwrap()
            .ct_eq(identity.manager_seed.as_ref().unwrap()));
        // 换 device_id（entropy 不同）必须失败
        assert!(LocalIdentityV1::unprotect(&blob, &identity.vault_id, &[0x99; 16]).is_err());
        // 换 vault_id 必须失败
        assert!(LocalIdentityV1::unprotect(&blob, &[0x98; 16], &identity.device_id).is_err());
    }

    #[cfg(not(windows))]
    #[test]
    fn dpapi_identity_is_unavailable_off_windows() {
        let identity = fixture(false, false, 80);
        assert!(matches!(
            identity.protect(),
            Err(SyncError::DpapiUnavailable)
        ));
        assert!(matches!(
            LocalIdentityV1::unprotect(b"blob", &identity.vault_id, &identity.device_id),
            Err(SyncError::DpapiUnavailable)
        ));
    }
}
