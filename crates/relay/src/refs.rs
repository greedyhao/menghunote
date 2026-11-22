//! 不透明对象引用（ADR 0006 §2 公开 refs / §15 `ObjectRefV1` 字段集）。
//!
//! relay 只用它做：refs 存在性校验、`refs_digest` 重建（交给 hook）、GC 完整性校验。
//! 这些字段**都是密文元数据**（object id 是不透明 HMAC/随机量，digest 是密文 SHA256），
//! 不含真实资产 hash、不含 VV、不含明文。

use crate::envelope::ObjectType;
use crate::ids::Hash32;

/// checkpoint ref 的角色（§15：manifest 自身不纳入 `refs_digest`，但要单独 pin 本体）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefRole {
    /// snapshot / asset 数据对象（纳入 refs_digest）。
    Data,
    /// manifest 对象本体（不纳入 refs_digest）。
    Manifest,
}

impl RefRole {
    /// SQLite 文本值。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Data => "data",
            Self::Manifest => "manifest",
        }
    }

    /// 从 SQLite 文本值还原。
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "data" => Some(Self::Data),
            "manifest" => Some(Self::Manifest),
            _ => None,
        }
    }
}

/// 公开对象引用（`ObjectRefV1` 的字段集，整数键 0..6 的顺序即此结构声明顺序）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectRefMeta {
    /// 0：不透明对象 id。
    pub object_id: Hash32,
    /// 1：对象类型。
    pub object_type: ObjectType,
    /// 2：doc generation（MANIFEST/ASSET 恒为 0）。
    pub generation: u64,
    /// 3：key epoch。
    pub key_epoch: u64,
    /// 4：分片数。
    pub part_count: u32,
    /// 5：明文总长。
    pub total_plain_len: u64,
    /// 6：对象摘要（密文 SHA256 聚合）。
    pub object_digest: Hash32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ref_role_roundtrips() {
        for role in [RefRole::Data, RefRole::Manifest] {
            assert_eq!(RefRole::parse(role.as_str()), Some(role));
        }
        assert_eq!(RefRole::parse("other"), None);
    }
}
