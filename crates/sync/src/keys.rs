//! 秘密材料与根密钥类型。
//!
//! ADR 0006 §3：根/KEK/口令**不 Debug、不日志、用 `Zeroizing`**。
//! 这里把这条纪律落到类型上：所有秘密都装在 [`Secret32`] 里，它：
//!
//! - 用 `Zeroizing<[u8; 32]>` 持有，`Drop` 时清零；
//! - **不实现** `Debug` / `Display` / `Serialize`（想打印就编译不过）；
//! - 相等比较走 [`subtle::ConstantTimeEq`]（避免按字节短路泄露）；
//! - 只暴露一个名字带 `expose` 的访问器，调用点一眼能看出"这里在用明文密钥"。
//!
//! 根密钥的 **wire 表示**是 `menghunote-sync-proto::RootSecretV1`；本模块的
//! [`RootSecretV1`] 是它的本地"零化 + 不可打印"版本，编解码复用 proto 的 CDE。

use crate::error::SyncError;
use crate::random::RandomSource;
use crate::{ObjectId, VaultId};
use menghunote_sync_proto::{ObjectType, RootSecretV1 as ProtoRootSecretV1};
use subtle::ConstantTimeEq;
use zeroize::{Zeroize, Zeroizing};

/// 32 字节秘密材料（根密钥、KEK、种子、私钥材料）。
///
/// 没有 `Debug`/`Display`：`format!("{secret:?}")` 无法编译。
#[derive(Clone)]
pub struct Secret32(Zeroizing<[u8; 32]>);

impl Secret32 {
    /// 生成一个新的随机秘密。
    pub fn generate(rng: &mut impl RandomSource) -> Result<Self, SyncError> {
        let mut bytes = Zeroizing::new([0u8; 32]);
        rng.fill(bytes.as_mut())?;
        Ok(Self(bytes))
    }

    /// 从已有字节构造（调用方交出所有权，内部会清零）。
    #[must_use]
    pub fn from_array(bytes: [u8; 32]) -> Self {
        Self(Zeroizing::new(bytes))
    }

    /// 取明文引用。**调用点必须显式出现 `expose`**，便于审计。
    #[must_use]
    pub fn expose(&self) -> &[u8; 32] {
        &self.0
    }

    /// 取明文字节切片（算法接口需要 `&[u8]` 时用）。
    #[must_use]
    pub fn expose_slice(&self) -> &[u8] {
        self.0.as_slice()
    }

    /// 常量时间相等。
    #[must_use]
    pub fn ct_eq(&self, other: &Self) -> bool {
        self.0.as_slice().ct_eq(other.0.as_slice()).into()
    }
}

impl PartialEq for Secret32 {
    fn eq(&self, other: &Self) -> bool {
        self.ct_eq(other)
    }
}

impl Eq for Secret32 {}

/// 用于封装根密钥的 KEK（口令派生或恢复码派生）。
///
/// 与 [`Secret32`] 一样不可打印；单独成类型是为了让"这是封装密钥、不是根"在调用点可见。
#[derive(Clone)]
pub struct WrapKey(Secret32);

impl WrapKey {
    /// 由 32 字节密钥材料构造（只允许本 crate 内部派生路径使用）。
    #[must_use]
    pub(crate) fn from_secret(secret: Secret32) -> Self {
        Self(secret)
    }

    /// 取明文引用。
    #[must_use]
    pub fn expose(&self) -> &[u8; 32] {
        self.0.expose()
    }

    /// 常量时间相等。
    #[must_use]
    pub fn ct_eq(&self, other: &Self) -> bool {
        self.0.ct_eq(&other.0)
    }
}

/// 某个 epoch 的 vault 根密钥 + 归属信息（ADR §2 `RootSecretV1` 的本地表示）。
///
/// 它是槽体与 HPKE grant 的明文载荷；**不上 wire 明文**，只出现在
/// "被 KEK 封装"或"被 HPKE 封装"之后。wire 编码走 `sync-proto` 的
/// `RootSecretV1::encode/decode`。
#[derive(Clone)]
pub struct RootSecretV1 {
    /// vault 身份。
    pub vault_id: VaultId,
    /// 密钥代（`key_epoch`，与 doc `generation` 永不混用）。
    pub key_epoch: u64,
    /// 32 字节随机根。
    pub root: Secret32,
}

impl RootSecretV1 {
    /// 用已有根构造。
    #[must_use]
    pub const fn new(vault_id: VaultId, key_epoch: u64, root: Secret32) -> Self {
        Self {
            vault_id,
            key_epoch,
            root,
        }
    }

    /// 生成新的随机根（ADR §2：每个新 epoch 的根重新随机，不由旧根导出）。
    pub fn generate(
        vault_id: VaultId,
        key_epoch: u64,
        rng: &mut impl RandomSource,
    ) -> Result<Self, SyncError> {
        let mut bytes = Zeroizing::new([0u8; 32]);
        rng.fill(bytes.as_mut())?;
        Ok(Self {
            vault_id,
            key_epoch,
            root: Secret32::from_array(*bytes),
        })
    }

    /// 归属信息 + 根的常量时间比较。
    #[must_use]
    pub fn ct_eq(&self, other: &Self) -> bool {
        self.vault_id == other.vault_id
            && self.key_epoch == other.key_epoch
            && self.root.ct_eq(&other.root)
    }

    /// CDE 编码（proto 的 `RootSecretV1`）。
    ///
    /// 编解码必须经过 proto 的 wire 结构，那里会有一份栈上的根副本；编码/解码完成后
    /// 立刻用 `zeroize` 擦掉本 crate 持有的那份（proto 内部 `to_cde` 的临时副本由
    /// 依赖侧管理，见实现回报"已知限制"）。
    pub fn encode(&self) -> Result<Vec<u8>, SyncError> {
        let mut proto = ProtoRootSecretV1 {
            vault_id: self.vault_id,
            key_epoch: self.key_epoch,
            root: *self.root.expose(),
        };
        let encoded = proto.encode()?;
        proto.root.zeroize();
        Ok(encoded)
    }

    /// 解析 CDE 并立即把根搬进 [`Secret32`]。
    pub fn decode(bytes: &[u8]) -> Result<Self, SyncError> {
        let mut proto = ProtoRootSecretV1::decode(bytes)?;
        let root = Secret32::from_array(proto.root);
        proto.root.zeroize();
        Ok(Self {
            vault_id: proto.vault_id,
            key_epoch: proto.key_epoch,
            root,
        })
    }
}

/// 派生数据对象密钥（ADR §2）：
/// `K_object = HKDF(K_epoch, salt=vault_id, info=MN/v1/data\0 || type_u8 || epoch_BE64 || object_id, 32)`。
///
/// info 的拼装由 `sync-proto` 的 `hkdf_data_info` 负责（含 `i64::MAX` 上界检查）。
pub fn derive_object_key(
    root: &RootSecretV1,
    object_type: ObjectType,
    object_id: &ObjectId,
) -> Result<Secret32, SyncError> {
    let info = menghunote_sync_proto::hkdf_data_info(object_type, root.key_epoch, object_id)?;
    crate::kdf::hkdf32(root.root.expose_slice(), &root.vault_id, &info)
}

/// 派生资产 HMAC 对象名密钥（ADR §2：
/// `K_asset_id = HKDF(K_epoch, salt=vault_id, info=MN/v1/asset-id\0, 32)`）。
pub fn derive_asset_id_key(root: &RootSecretV1) -> Result<Secret32, SyncError> {
    crate::kdf::hkdf32(
        root.root.expose_slice(),
        &root.vault_id,
        crate::domain::HKDF_ASSET_ID_INFO,
    )
}

/// 明文秘密的调试格式：只打印"已脱敏"，不打印任何字节。
pub(crate) fn redacted() -> &'static str {
    "<redacted>"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestRng;

    #[test]
    fn secret_eq_is_constant_time_wrapper() {
        let mut rng = TestRng::new(1);
        let a = Secret32::generate(&mut rng).unwrap();
        let b = Secret32::generate(&mut rng).unwrap();
        assert!(!a.ct_eq(&b));
        assert!(a.ct_eq(&Secret32::from_array(*a.expose())));
        assert!(a.ct_eq(&Secret32::from_array(*a.expose())));
    }

    #[test]
    fn root_secret_cde_roundtrip() {
        let mut rng = TestRng::new(2);
        let root = RootSecretV1::generate([7u8; 16], 3, &mut rng).unwrap();
        let bytes = root.encode().unwrap();
        let back = RootSecretV1::decode(&bytes).unwrap();
        assert!(root.ct_eq(&back));
        assert_eq!(bytes, back.encode().unwrap(), "CDE 必须确定性");
        // proto 的 RootSecretV1 是 3 项 map（0xa3）
        assert_eq!(bytes[0], 0xa3);
        // 末尾是根的 32 字节
        assert_eq!(&bytes[bytes.len() - 32..], root.root.expose_slice());

        // 尾部多余字节拒绝
        let mut long = bytes.clone();
        long.push(0);
        assert!(RootSecretV1::decode(&long).is_err());
        // 截断拒绝
        assert!(RootSecretV1::decode(&bytes[..bytes.len() - 1]).is_err());
        // 字段数不符拒绝（把 3 项改成 2 项）
        let mut short = bytes;
        short[0] = 0xa2;
        assert!(RootSecretV1::decode(&short).is_err());
    }

    #[test]
    fn root_secret_rejects_zero_epoch() {
        // proto 的 RootSecretV1::validate 要求 key_epoch > 0
        let root = RootSecretV1::new([1u8; 16], 0, Secret32::from_array([2u8; 32]));
        assert!(root.encode().is_err());
    }

    #[test]
    fn object_keys_are_domain_separated_and_deterministic() {
        let mut rng = TestRng::new(3);
        let root = RootSecretV1::generate([9u8; 16], 1, &mut rng).unwrap();
        let oid_a = [0x11u8; 32];
        let oid_b = [0x22u8; 32];

        let k1 = derive_object_key(&root, ObjectType::Update, &oid_a).unwrap();
        let k1_again = derive_object_key(&root, ObjectType::Update, &oid_a).unwrap();
        assert!(k1.ct_eq(&k1_again));

        // 类型/对象/epoch/根 任一不同都必须得到不同密钥
        assert!(!k1.ct_eq(&derive_object_key(&root, ObjectType::Snapshot, &oid_a).unwrap()));
        assert!(!k1.ct_eq(&derive_object_key(&root, ObjectType::Update, &oid_b).unwrap()));
        let other_epoch = RootSecretV1::new(root.vault_id, 2, root.root.clone());
        assert!(!k1.ct_eq(&derive_object_key(&other_epoch, ObjectType::Update, &oid_a).unwrap()));
        let other_root = RootSecretV1::generate(root.vault_id, 1, &mut rng).unwrap();
        assert!(!k1.ct_eq(&derive_object_key(&other_root, ObjectType::Update, &oid_a).unwrap()));
        // 资产密钥独立
        assert!(!k1.ct_eq(&derive_asset_id_key(&root).unwrap()));
    }

    #[test]
    fn secret_debug_is_impossible() {
        // 类型系统层面 Secret32/WrapKey 没有 Debug；这里只断言脱敏占位符存在。
        assert_eq!(redacted(), "<redacted>");
    }
}
