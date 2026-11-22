//! relay 控制面的 CBOR（CDE）编解码辅助（S7-9）。
//!
//! relay 的控制面是 `application/cbor` + 整数键 CDE map（ADR §5/§15），可选字段**省略**。
//! 本模块只做"记录形状"的薄封装：真正的 canonical 编解码由 `sync-proto::cde` 保证
//! （键升序、无重复、拒绝尾随字节），这里不重写编码规则。

use crate::error::SyncError;
pub use menghunote_sync_proto::CdeValue;
use menghunote_sync_proto::{decode_cde, encode_cde};

/// 解码后的整数键 map。
pub type Fields = Vec<(u64, CdeValue)>;

/// 解码一个 CDE map；不是 map/非法 CBOR 都算 relay wire 错误。
pub fn decode(body: &[u8]) -> Result<Fields, SyncError> {
    match decode_cde(body) {
        Ok(CdeValue::Map(fields)) => Ok(fields),
        Ok(_) => Err(SyncError::RelayWire("控制面响应不是 CDE map")),
        Err(_) => Err(SyncError::RelayWire("控制面响应不是合法 CDE")),
    }
}

/// 编码一个整数键 map（编码失败说明调用方构造了非法值）。
pub fn encode(fields: Fields) -> Result<Vec<u8>, SyncError> {
    encode_cde(&CdeValue::Map(fields)).map_err(|_| SyncError::RelayWire("控制面请求无法编码"))
}

/// 编码任意 CDE 值（嵌套数组/记录用）。
pub fn encode_value(value: &CdeValue) -> Result<Vec<u8>, SyncError> {
    encode_cde(value).map_err(|_| SyncError::RelayWire("控制面请求无法编码"))
}

/// u64 → `CdeValue`。
pub fn uint(value: u64) -> CdeValue {
    CdeValue::UInt(value)
}

/// 字节串 → `CdeValue`。
pub fn bytes(value: impl Into<Vec<u8>>) -> CdeValue {
    CdeValue::Bytes(value.into())
}

/// 文本 → `CdeValue`。
pub fn text(value: impl Into<String>) -> CdeValue {
    CdeValue::Text(value.into())
}

/// 数组 → `CdeValue`。
pub fn array(values: Vec<CdeValue>) -> CdeValue {
    CdeValue::Array(values)
}

/// 布尔 → `CdeValue`。
pub fn boolean(value: bool) -> CdeValue {
    CdeValue::Bool(value)
}

fn invalid() -> SyncError {
    SyncError::RelayWire("控制面字段缺失或类型不符")
}

/// 取字段；缺失即 wire 错误。
pub fn field(fields: &Fields, key: u64) -> Result<&CdeValue, SyncError> {
    fields
        .iter()
        .find(|(index, _)| *index == key)
        .map(|(_, value)| value)
        .ok_or_else(invalid)
}

/// 取 optional 字段。
pub fn opt_field(fields: &Fields, key: u64) -> Option<&CdeValue> {
    fields
        .iter()
        .find(|(index, _)| *index == key)
        .map(|(_, value)| value)
}

/// u64 字段。
pub fn uint_field(fields: &Fields, key: u64) -> Result<u64, SyncError> {
    match field(fields, key)? {
        CdeValue::UInt(value) => Ok(*value),
        _ => Err(invalid()),
    }
}

/// optional u64 字段。
pub fn opt_uint_field(fields: &Fields, key: u64) -> Result<Option<u64>, SyncError> {
    match opt_field(fields, key) {
        None => Ok(None),
        Some(CdeValue::UInt(value)) => Ok(Some(*value)),
        Some(_) => Err(invalid()),
    }
}

/// 字节串字段。
pub fn bytes_field(fields: &Fields, key: u64) -> Result<Vec<u8>, SyncError> {
    match field(fields, key)? {
        CdeValue::Bytes(value) => Ok(value.clone()),
        _ => Err(invalid()),
    }
}

/// optional 字节串字段。
pub fn opt_bytes_field(fields: &Fields, key: u64) -> Result<Option<Vec<u8>>, SyncError> {
    match opt_field(fields, key) {
        None => Ok(None),
        Some(CdeValue::Bytes(value)) => Ok(Some(value.clone())),
        Some(_) => Err(invalid()),
    }
}

/// 定长字节串字段（16B/32B/64B）。
pub fn bytes_fixed<const N: usize>(fields: &Fields, key: u64) -> Result<[u8; N], SyncError> {
    let bytes = bytes_field(fields, key)?;
    bytes
        .as_slice()
        .try_into()
        .map_err(|_| SyncError::RelayWire("控制面字段长度不符"))
}

/// 文本字段。
pub fn text_field(fields: &Fields, key: u64) -> Result<String, SyncError> {
    match field(fields, key)? {
        CdeValue::Text(value) => Ok(value.clone()),
        _ => Err(invalid()),
    }
}

/// optional 文本字段。
pub fn opt_text_field(fields: &Fields, key: u64) -> Result<Option<String>, SyncError> {
    match opt_field(fields, key) {
        None => Ok(None),
        Some(CdeValue::Text(value)) => Ok(Some(value.clone())),
        Some(_) => Err(invalid()),
    }
}

/// 布尔字段。
pub fn bool_field(fields: &Fields, key: u64) -> Result<bool, SyncError> {
    match field(fields, key)? {
        CdeValue::Bool(value) => Ok(*value),
        _ => Err(invalid()),
    }
}

/// 数组字段。
pub fn array_field(fields: &Fields, key: u64) -> Result<Vec<CdeValue>, SyncError> {
    match field(fields, key)? {
        CdeValue::Array(values) => Ok(values.clone()),
        _ => Err(invalid()),
    }
}

/// 把一个 `CdeValue` 当整数键 map 读。
pub fn value_map(value: &CdeValue) -> Result<Fields, SyncError> {
    match value {
        CdeValue::Map(fields) => Ok(fields.clone()),
        _ => Err(invalid()),
    }
}

/// 把数组元素当 map 读。
pub fn value_map_field(fields: &Fields, key: u64, index: usize) -> Result<Fields, SyncError> {
    let values = array_field(fields, key)?;
    let value = values.get(index).ok_or_else(invalid)?;
    value_map(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_encodes_sorted_by_key() {
        let encoded = encode(vec![(1, uint(2)), (0, uint(1))]).expect("编码");
        // 0x02: map(2)；0x00/0x01 键；0x01/0x02 值（CDE 的 uint 是 1 字节小整数）。
        assert_eq!(encoded, vec![0xa2, 0x00, 0x01, 0x01, 0x02]);
        let decoded = decode(&encoded).expect("解码");
        assert_eq!(decoded, vec![(0, uint(1)), (1, uint(2))]);
    }

    #[test]
    fn optional_field_roundtrip() {
        let fields = vec![(0, bytes([0xaa; 16])), (1, text("active"))];
        assert_eq!(bytes_fixed::<16>(&fields, 0).unwrap(), [0xaa; 16]);
        assert_eq!(text_field(&fields, 1).unwrap(), "active");
        assert_eq!(opt_uint_field(&fields, 9).unwrap(), None);
        assert!(bytes_fixed::<32>(&fields, 0).is_err());
        assert!(bool_field(&fields, 0).is_err());
    }
}
