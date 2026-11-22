//! 控制面 wire：`application/cbor` + **CDE**（ADR 0006 §5/§15）。
//!
//! 所有控制面请求/响应体都是 CDE 的**整数键 map**：键从 0 连续编号、按键升序、无重复。
//! 这里只做"记录形状"的薄适配层：真正的 canonical 编解码在 `sync-proto::cde`（本 crate 不重写）。
//! 对象分片（`PUT/GET part`）仍是 `application/octet-stream` 原样字节，不走本模块。
//!
//! 约定（与 ADR §15 一致）：
//! - ID/hash/nonce/签名一律 byte string（UUID=16B、oid/hash=32B、sign=64B）；
//! - optional 字段**省略**而不是写 null；
//! - 布尔只用于 `retryable` 等开关。
//!
//! 签名包装（管理/设备签名的请求）：`{0: Bytes(unsigned_body), 1: Bytes(signature)}`，
//! 被签名内容作为**独立记录**再套一层 CDE（即 `unsigned_body` 自身是 CDE map 的字节）。

use menghunote_sync_proto as proto;

use crate::error::{ApiError, Error, ErrorCode, Result};
use crate::ids::{Hash32, Id16};

/// 直接暴露 proto 的 CDE 值类型（调用方构造数组/嵌套 map 时用）。
pub use menghunote_sync_proto::CdeValue;

/// 控制面 Content-Type。
pub const CONTENT_TYPE: &str = "application/cbor";

/// 解码后的整数键 map。
pub type Fields = Vec<(u64, proto::CdeValue)>;

/// CDE 值构造快捷方式。
pub fn uint(value: u64) -> proto::CdeValue {
    proto::CdeValue::UInt(value)
}

/// byte string。
pub fn bytes(value: impl Into<Vec<u8>>) -> proto::CdeValue {
    proto::CdeValue::Bytes(value.into())
}

/// UTF-8 text。
pub fn text(value: impl Into<String>) -> proto::CdeValue {
    proto::CdeValue::Text(value.into())
}

/// array。
pub fn array(value: Vec<proto::CdeValue>) -> proto::CdeValue {
    proto::CdeValue::Array(value)
}

/// 整数键 map（编码时由 proto 校验升序/无重复）。
pub fn map(value: Fields) -> proto::CdeValue {
    proto::CdeValue::Map(value)
}

/// bool。
pub fn boolean(value: bool) -> proto::CdeValue {
    proto::CdeValue::Bool(value)
}

fn invalid() -> Error {
    Error::api(ErrorCode::InvalidRequest)
}

/// 解码一个 CDE map，并校验键从 0 连续编号（v1 记录风格）。
pub fn decode(body: &[u8]) -> Result<Fields> {
    let value = proto::decode_cde(body).map_err(|_| invalid())?;
    let proto::CdeValue::Map(fields) = value else {
        return Err(invalid());
    };
    // 键序/唯一性由 `sync-proto` 的 CDE 解码保证；optional 字段省略时会保留原键号，
    // 因此**不能**要求键连续（ADR §16.4）。
    Ok(fields)
}

/// 编码一个 CDE map（编码失败说明调用方构造了非法值，属内部 bug）。
pub fn encode(fields: Fields) -> Result<Vec<u8>> {
    proto::encode_cde(&map(fields)).map_err(|_| Error::Invariant("cbor encode"))
}

/// 取字段（缺失 → 400）。
pub fn field(fields: &Fields, key: u64) -> Result<&proto::CdeValue> {
    fields
        .iter()
        .find(|(index, _)| *index == key)
        .map(|(_, value)| value)
        .ok_or_else(invalid)
}

/// 取 optional 字段。
pub fn opt_field(fields: &Fields, key: u64) -> Option<&proto::CdeValue> {
    fields
        .iter()
        .find(|(index, _)| *index == key)
        .map(|(_, value)| value)
}

/// u64 字段。
pub fn uint_field(fields: &Fields, key: u64) -> Result<u64> {
    match field(fields, key)? {
        proto::CdeValue::UInt(value) => Ok(*value),
        _ => Err(invalid()),
    }
}

/// optional u64 字段。
pub fn opt_uint_field(fields: &Fields, key: u64) -> Result<Option<u64>> {
    match opt_field(fields, key) {
        None => Ok(None),
        Some(proto::CdeValue::UInt(value)) => Ok(Some(*value)),
        Some(_) => Err(invalid()),
    }
}

/// byte string 字段。
pub fn bytes_field(fields: &Fields, key: u64) -> Result<Vec<u8>> {
    match field(fields, key)? {
        proto::CdeValue::Bytes(value) => Ok(value.clone()),
        _ => Err(invalid()),
    }
}

/// optional byte string 字段。
pub fn opt_bytes_field(fields: &Fields, key: u64) -> Result<Option<Vec<u8>>> {
    match opt_field(fields, key) {
        None => Ok(None),
        Some(proto::CdeValue::Bytes(value)) => Ok(Some(value.clone())),
        Some(_) => Err(invalid()),
    }
}

/// 16 字节 ID 字段。
pub fn id16_field(fields: &Fields, key: u64) -> Result<Id16> {
    bytes_field(fields, key)?
        .as_slice()
        .try_into()
        .map_err(|_| invalid())
}

/// 32 字节 hash 字段。
pub fn hash32_field(fields: &Fields, key: u64) -> Result<Hash32> {
    bytes_field(fields, key)?
        .as_slice()
        .try_into()
        .map_err(|_| invalid())
}

/// 32 字节定长字段（公钥等）。
pub fn key32_field(fields: &Fields, key: u64) -> Result<[u8; 32]> {
    hash32_field(fields, key)
}

/// UTF-8 text 字段。
pub fn text_field(fields: &Fields, key: u64) -> Result<String> {
    match field(fields, key)? {
        proto::CdeValue::Text(value) => Ok(value.clone()),
        _ => Err(invalid()),
    }
}

/// bool 字段。
pub fn bool_field(fields: &Fields, key: u64) -> Result<bool> {
    match field(fields, key)? {
        proto::CdeValue::Bool(value) => Ok(*value),
        _ => Err(invalid()),
    }
}

/// array 字段。
pub fn array_field(fields: &Fields, key: u64) -> Result<Vec<proto::CdeValue>> {
    match field(fields, key)? {
        proto::CdeValue::Array(value) => Ok(value.clone()),
        _ => Err(invalid()),
    }
}

/// 嵌套 map 字段。
pub fn map_field(fields: &Fields, key: u64) -> Result<Fields> {
    match field(fields, key)? {
        proto::CdeValue::Map(value) => Ok(value.clone()),
        _ => Err(invalid()),
    }
}

/// optional 嵌套 map。
pub fn opt_map_field(fields: &Fields, key: u64) -> Result<Option<Fields>> {
    match opt_field(fields, key) {
        None => Ok(None),
        Some(proto::CdeValue::Map(value)) => Ok(Some(value.clone())),
        Some(_) => Err(invalid()),
    }
}

/// 把一个 `CdeValue` 当 map 读（数组元素等）。
pub fn value_map(value: &proto::CdeValue) -> Result<Fields> {
    match value {
        proto::CdeValue::Map(value) => Ok(value.clone()),
        _ => Err(invalid()),
    }
}

/// 签名包装：`{0: unsigned_body bytes, 1: signature bytes}`。
#[derive(Debug, Clone)]
pub struct Signed {
    /// 未签名载荷（自身是 CDE 记录字节）。
    pub payload: Vec<u8>,
    /// 签名（管理/设备 key 对域+载荷的签名）。
    pub signature: Vec<u8>,
}

impl Signed {
    /// 从外层 map 的指定键读签名包装。
    pub fn from_fields(fields: &Fields, key: u64) -> Result<Self> {
        let nested = map_field(fields, key)?;
        Ok(Self {
            payload: bytes_field(&nested, 0)?,
            signature: bytes_field(&nested, 1)?,
        })
    }

    /// 解析载荷为 CDE map。
    pub fn payload_fields(&self) -> Result<Fields> {
        decode(&self.payload)
    }
}

/// 协议错误体的 CBOR：`{0: code(text), 1: retryable(bool), 2: epoch?, 3: control_rev?, 4: hint?}`。
pub fn error_body(api: &ApiError) -> Vec<u8> {
    let mut fields = vec![(0, text(api.code.as_str())), (1, boolean(api.retryable))];
    if let Some(epoch) = api.current_epoch {
        fields.push((2, uint(epoch)));
    }
    if let Some(rev) = api.current_control_rev {
        fields.push((3, uint(rev)));
    }
    if let Some(hint) = api.bootstrap_hint {
        fields.push((4, text(hint)));
    }
    fields.sort_by_key(|(key, _)| *key);
    encode(fields).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_roundtrip_is_canonical() {
        let encoded = encode(vec![
            (0, uint(7)),
            (1, bytes([1u8, 2, 3])),
            (2, text("hello")),
            (3, boolean(true)),
        ])
        .expect("encode");
        let fields = decode(&encoded).expect("decode");
        assert_eq!(uint_field(&fields, 0).expect("uint"), 7);
        assert_eq!(bytes_field(&fields, 1).expect("bytes"), vec![1, 2, 3]);
        assert_eq!(text_field(&fields, 2).expect("text"), "hello");
        assert!(bool_field(&fields, 3).expect("bool"));
        // 同一语义值重编码必须逐字节相同（CDE 唯一性）。
        assert_eq!(encode(fields).expect("re-encode"), encoded);
    }

    #[test]
    fn rejects_non_contiguous_or_non_map() {
        // 键从 1 开始 → 拒绝。
        let bad = proto::encode_cde(&map(vec![(0, uint(1))])).expect("encode");
        assert!(decode(&bad).is_ok(), "单键 0 是合法记录（optional 省略）");
        let not_map = proto::encode_cde(&uint(1)).expect("encode");
        assert!(decode(&not_map).is_err());
        // 非 map 顶层 → 拒绝。
        let bad = proto::encode_cde(&uint(1)).expect("encode");
        assert!(decode(&bad).is_err());
        // 截断 → 拒绝。
        assert!(decode(&[0xa1]).is_err());
    }

    #[test]
    fn optional_fields_are_omitted_not_null() {
        let mut fields = vec![(0, uint(1))];
        fields.push((1, boolean(false)));
        let encoded = encode(fields).expect("encode");
        assert_eq!(encoded[0], 0xa2, "2 个键的 map 头");
        // 无 optional 键时，读 optional 返回 None。
        let fields = decode(&encoded).expect("decode");
        assert_eq!(opt_uint_field(&fields, 5).expect("opt"), None);
    }

    #[test]
    fn signed_envelope_roundtrip() {
        let inner = encode(vec![(0, uint(9)), (1, bytes([7u8; 16]))]).expect("inner");
        let outer = encode(vec![(
            0,
            map(vec![
                (0, bytes(inner.clone())),
                (1, bytes(vec![0xAAu8; 64])),
            ]),
        )])
        .expect("outer");
        let fields = decode(&outer).expect("decode outer");
        let signed = Signed::from_fields(&fields, 0).expect("signed");
        assert_eq!(signed.payload, inner);
        assert_eq!(signed.signature.len(), 64);
        let payload = signed.payload_fields().expect("payload map");
        assert_eq!(uint_field(&payload, 0).expect("v"), 9);
    }

    #[test]
    fn error_body_contains_only_protocol_fields() {
        let api = ApiError::new(ErrorCode::CursorExpired)
            .epoch(3)
            .hint("bootstrap");
        let body = error_body(&api);
        let fields = decode(&body).expect("decode");
        assert_eq!(text_field(&fields, 0).expect("code"), "CURSOR_EXPIRED");
        assert!(bool_field(&fields, 1).expect("retryable"));
        assert_eq!(uint_field(&fields, 2).expect("epoch"), 3);
        assert_eq!(text_field(&fields, 4).expect("hint"), "bootstrap");
        assert!(opt_uint_field(&fields, 3).expect("rev").is_none());
    }
}
