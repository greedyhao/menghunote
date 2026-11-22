//! CDE 桥：本 crate 私有结构（DPAPI 身份 BLOB、HPKE grant AAD）复用 `sync-proto` 的
//! canonical CBOR 编解码器，不另写一套编码。
//!
//! 这里只补三件 proto 没暴露给外部记录解析器的小工具：
//!
//! - 严格取字段（字段数、连续整数键、optional 省略）；
//! - 定长字节 / 秘密字节的搬运（秘密字节在拷贝后立即零化临时缓冲）；
//! - 编码后用 [`zeroize_value`] 擦掉 `CdeValue` 里的明文副本。

use crate::error::SyncError;
use crate::keys::Secret32;
use menghunote_sync_proto::{decode_cde, encode_cde, CdeValue};
use zeroize::{Zeroize, Zeroizing};

/// 解码为"整数键 → 值"序列；canonical 校验（最短整数/键升序/无重复）由 proto 负责。
pub fn decode_entries(bytes: &[u8]) -> Result<Vec<(u64, CdeValue)>, SyncError> {
    match decode_cde(bytes)? {
        CdeValue::Map(entries) => Ok(entries),
        _ => Err(SyncError::Cde("期望整数键 map")),
    }
}

/// 断言一个 `CdeValue` 是整数键 map 并取出条目（用于嵌套记录）。
pub fn entries_of(value: CdeValue) -> Result<Vec<(u64, CdeValue)>, SyncError> {
    match value {
        CdeValue::Map(entries) => Ok(entries),
        _ => Err(SyncError::Cde("期望整数键 map")),
    }
}

/// 断言字段数恰好 `N` 且键为 `0..N`，返回按序的值。
pub fn expect_exact<const N: usize>(
    entries: Vec<(u64, CdeValue)>,
) -> Result<Vec<CdeValue>, SyncError> {
    if entries.len() != N {
        return Err(SyncError::Cde("记录字段数不符"));
    }
    let mut out = Vec::with_capacity(N);
    for (expected, (key, value)) in entries.into_iter().enumerate() {
        if key != expected as u64 {
            return Err(SyncError::Cde("记录字段键不连续或乱序"));
        }
        out.push(value);
    }
    Ok(out)
}

/// 取 UInt。
pub fn expect_uint(value: CdeValue, field: &'static str) -> Result<u64, SyncError> {
    match value {
        CdeValue::UInt(v) => Ok(v),
        _ => Err(SyncError::Cde(field)),
    }
}

/// 取 u8。
pub fn expect_u8(value: CdeValue, field: &'static str) -> Result<u8, SyncError> {
    u8::try_from(expect_uint(value, field)?).map_err(|_| SyncError::Cde(field))
}

/// 取 u16。
pub fn expect_u16(value: CdeValue, field: &'static str) -> Result<u16, SyncError> {
    u16::try_from(expect_uint(value, field)?).map_err(|_| SyncError::Cde(field))
}

/// 取定长公开字节（ID/公钥/随机 ID 等）。
pub fn expect_bytes<const N: usize>(
    value: CdeValue,
    field: &'static str,
) -> Result<[u8; N], SyncError> {
    match value {
        CdeValue::Bytes(bytes) if bytes.len() == N => {
            let mut out = [0u8; N];
            out.copy_from_slice(&bytes);
            Ok(out)
        }
        _ => Err(SyncError::Cde(field)),
    }
}

/// 取 32 字节秘密：拷贝进 [`Secret32`] 后立刻零化临时 `Vec`。
pub fn expect_secret32(value: CdeValue, field: &'static str) -> Result<Secret32, SyncError> {
    match value {
        CdeValue::Bytes(bytes) if bytes.len() == 32 => {
            let mut out = [0u8; 32];
            out.copy_from_slice(&bytes);
            // 临时明文缓冲在离开作用域时清零（CdeValue 自己不做零化）。
            let _wiped = Zeroizing::new(bytes);
            Ok(Secret32::from_array(out))
        }
        _ => Err(SyncError::Cde(field)),
    }
}

/// 编码为 CDE 字节。
pub fn encode(value: &CdeValue) -> Result<Vec<u8>, SyncError> {
    Ok(encode_cde(value)?)
}

/// 递归擦除 `CdeValue` 里的明文（Bytes/Text）。用于"编码完后立刻销毁中间副本"。
pub fn zeroize_value(value: &mut CdeValue) {
    match value {
        CdeValue::Bytes(bytes) => bytes.zeroize(),
        CdeValue::Text(text) => text.zeroize(),
        CdeValue::Array(items) => {
            for item in items {
                zeroize_value(item);
            }
        }
        CdeValue::Map(entries) => {
            for (_, item) in entries {
                zeroize_value(item);
            }
        }
        CdeValue::UInt(_) | CdeValue::Bool(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_record_roundtrip() {
        let value = CdeValue::Map(vec![
            (0, CdeValue::UInt(7)),
            (1, CdeValue::Bytes(vec![0xaa; 16])),
        ]);
        let bytes = encode(&value).unwrap();
        let entries = decode_entries(&bytes).unwrap();
        let fields = expect_exact::<2>(entries).unwrap();
        assert_eq!(expect_uint(fields[0].clone(), "n").unwrap(), 7);
        assert_eq!(
            expect_bytes::<16>(fields[1].clone(), "id").unwrap(),
            [0xaa; 16]
        );
        // 字段数不符
        let entries = decode_entries(&bytes).unwrap();
        assert!(expect_exact::<1>(entries).is_err());
        // 类型不符
        assert!(expect_uint(CdeValue::Bytes(vec![1]), "n").is_err());
        assert!(expect_bytes::<32>(CdeValue::Bytes(vec![1; 16]), "id").is_err());
    }

    #[test]
    fn secrets_are_wiped_from_temporary_value() {
        let mut value = CdeValue::Map(vec![(0, CdeValue::Bytes(vec![0x77; 32]))]);
        let bytes = encode(&value).unwrap();
        zeroize_value(&mut value);
        match &value {
            CdeValue::Map(entries) => match &entries[0].1 {
                CdeValue::Bytes(b) => assert!(b.iter().all(|x| *x == 0), "中间副本必须被擦除"),
                _ => panic!("结构不符"),
            },
            _ => panic!("结构不符"),
        }
        // 编码结果仍在（由调用方负责 Zeroizing）
        assert!(bytes.contains(&0x77));
        let mut decoded = decode_entries(&bytes).unwrap();
        let secret = expect_secret32(decoded.pop().unwrap().1, "root").unwrap();
        assert_eq!(secret.expose(), &[0x77; 32]);
    }

    #[test]
    fn non_canonical_bytes_rejected_by_proto() {
        // 0x18 0x05 = UInt(5) 的非最短编码
        assert!(decode_entries(&[0x18, 0x05]).is_err());
        // 乱序键
        assert!(decode_entries(&[0xa2, 0x01, 0x00, 0x00, 0x00]).is_err());
    }
}
