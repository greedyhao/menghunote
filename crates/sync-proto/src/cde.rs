//! 确定性 CBOR（CDE，RFC 8949）最小实现。
//!
//! 只接受 v1 用到的类型：uint、byte string、text、array、map（uint 键）、bool。
//! 禁止：浮点、null、undefined、tag、不定长、重复键、乱序键、非最短整数。
//!
//! map 的键在编码时按整数升序输出；解码时要求严格升序，因此同一语义值只有
//! 一种字节串。

use crate::error::ProtoError;

/// 单次 CDE 解码/编码的硬上限（与 ADR 0006 的 64MiB 对象上限一致）。
pub const MAX_CDE_BYTES: usize = 64 * 1024 * 1024;
/// 嵌套深度上限，防止恶意输入打爆栈。
pub const MAX_CDE_DEPTH: usize = 64;
/// 单层 array/map 项数上限；v1 最大的数组是 manifest refs（≤1024）。
pub const MAX_CDE_CONTAINER_ITEMS: usize = 65_536;

/// CDE 值。记录类型在 `records` 中映射到它。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CdeValue {
    UInt(u64),
    Bytes(Vec<u8>),
    Text(String),
    Array(Vec<CdeValue>),
    /// 整数键 map；编码时按 key 升序，解码时拒绝乱序/重复。
    Map(Vec<(u64, CdeValue)>),
    Bool(bool),
}

impl CdeValue {
    pub fn uint(value: u64) -> Self {
        Self::UInt(value)
    }

    pub fn bytes(value: impl Into<Vec<u8>>) -> Self {
        Self::Bytes(value.into())
    }

    pub fn text(value: impl Into<String>) -> Self {
        Self::Text(value.into())
    }

    pub fn array(value: Vec<CdeValue>) -> Self {
        Self::Array(value)
    }

    pub fn map(value: Vec<(u64, CdeValue)>) -> Self {
        Self::Map(value)
    }

    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(self)
    }
}

/// canonical 编码。map 会排序并拒绝重复键。
pub fn encode(value: &CdeValue) -> Result<Vec<u8>, ProtoError> {
    let mut out = Vec::new();
    write_value(value, &mut out, 0)?;
    if out.len() > MAX_CDE_BYTES {
        return Err(ProtoError::TooLarge);
    }
    Ok(out)
}

/// canonical 解码；必须恰好消费全部输入。
pub fn decode(bytes: &[u8]) -> Result<CdeValue, ProtoError> {
    if bytes.len() > MAX_CDE_BYTES {
        return Err(ProtoError::TooLarge);
    }
    let (value, used) = read_value(bytes, 0, 0)?;
    if used != bytes.len() {
        return Err(ProtoError::NonCanonical(format!(
            "尾部还有 {} 字节未消费",
            bytes.len() - used
        )));
    }
    Ok(value)
}

fn write_value(value: &CdeValue, out: &mut Vec<u8>, depth: usize) -> Result<(), ProtoError> {
    if depth > MAX_CDE_DEPTH {
        return Err(ProtoError::Invalid("CDE 嵌套过深".to_owned()));
    }
    match value {
        CdeValue::UInt(value) => write_head(0, *value, out),
        CdeValue::Bytes(bytes) => {
            write_head(2, bytes.len() as u64, out);
            out.extend_from_slice(bytes);
        }
        CdeValue::Text(text) => {
            write_head(3, text.len() as u64, out);
            out.extend_from_slice(text.as_bytes());
        }
        CdeValue::Array(items) => {
            if items.len() > MAX_CDE_CONTAINER_ITEMS {
                return Err(ProtoError::TooLarge);
            }
            write_head(4, items.len() as u64, out);
            for item in items {
                write_value(item, out, depth + 1)?;
            }
        }
        CdeValue::Map(entries) => {
            if entries.len() > MAX_CDE_CONTAINER_ITEMS {
                return Err(ProtoError::TooLarge);
            }
            let mut sorted = entries.clone();
            sorted.sort_by_key(|(key, _)| *key);
            if sorted.windows(2).any(|pair| pair[0].0 == pair[1].0) {
                return Err(ProtoError::NonCanonical("map 含重复键".to_owned()));
            }
            write_head(5, sorted.len() as u64, out);
            for (key, value) in sorted {
                write_head(0, key, out);
                write_value(&value, out, depth + 1)?;
            }
        }
        CdeValue::Bool(value) => out.push(if *value { 0xf5 } else { 0xf4 }),
    }
    Ok(())
}

fn write_head(major: u8, value: u64, out: &mut Vec<u8>) {
    let major = major << 5;
    if value < 24 {
        out.push(major | value as u8);
    } else if value <= u8::MAX as u64 {
        out.push(major | 24);
        out.push(value as u8);
    } else if value <= u16::MAX as u64 {
        out.push(major | 25);
        out.extend_from_slice(&(value as u16).to_be_bytes());
    } else if value <= u32::MAX as u64 {
        out.push(major | 26);
        out.extend_from_slice(&(value as u32).to_be_bytes());
    } else {
        out.push(major | 27);
        out.extend_from_slice(&value.to_be_bytes());
    }
}

fn read_value(bytes: &[u8], offset: usize, depth: usize) -> Result<(CdeValue, usize), ProtoError> {
    if depth > MAX_CDE_DEPTH {
        return Err(ProtoError::Invalid("CDE 嵌套过深".to_owned()));
    }
    let (major, argument, mut position) = read_head(bytes, offset)?;

    let value = match major {
        0 => CdeValue::UInt(argument),
        2 => {
            let length = usize_of(argument)?;
            let end = position.checked_add(length).ok_or(ProtoError::Truncated)?;
            if end > bytes.len() {
                return Err(ProtoError::Truncated);
            }
            let value = CdeValue::Bytes(bytes[position..end].to_vec());
            position = end;
            value
        }
        3 => {
            let length = usize_of(argument)?;
            let end = position.checked_add(length).ok_or(ProtoError::Truncated)?;
            if end > bytes.len() {
                return Err(ProtoError::Truncated);
            }
            let text = std::str::from_utf8(&bytes[position..end])
                .map_err(|_| ProtoError::Invalid("text 不是合法 UTF-8".to_owned()))?;
            position = end;
            CdeValue::Text(text.to_owned())
        }
        4 => {
            let length = usize_of(argument)?;
            if length > MAX_CDE_CONTAINER_ITEMS || length > bytes.len() {
                return Err(ProtoError::TooLarge);
            }
            let mut items = Vec::with_capacity(length);
            for _ in 0..length {
                let (item, used) = read_value(bytes, position, depth + 1)?;
                items.push(item);
                position = used;
            }
            CdeValue::Array(items)
        }
        5 => {
            let length = usize_of(argument)?;
            if length > MAX_CDE_CONTAINER_ITEMS || length > bytes.len() {
                return Err(ProtoError::TooLarge);
            }
            let mut entries = Vec::with_capacity(length);
            let mut previous = None;
            for _ in 0..length {
                let (key, used) = read_value(bytes, position, depth + 1)?;
                let CdeValue::UInt(key) = key else {
                    return Err(ProtoError::NonCanonical("map 的键必须是整数".to_owned()));
                };
                if let Some(previous) = previous {
                    if key <= previous {
                        return Err(ProtoError::NonCanonical("map 的键必须严格升序".to_owned()));
                    }
                }
                previous = Some(key);
                position = used;
                let (value, used) = read_value(bytes, position, depth + 1)?;
                entries.push((key, value));
                position = used;
            }
            CdeValue::Map(entries)
        }
        7 => match argument {
            20 => CdeValue::Bool(false),
            21 => CdeValue::Bool(true),
            _ => {
                return Err(ProtoError::NonCanonical(
                    "只允许 bool，拒绝 null/undefined/float/simple".to_owned(),
                ))
            }
        },
        _ => {
            return Err(ProtoError::NonCanonical(format!(
                "不允许的 CBOR major type {major}"
            )))
        }
    };
    Ok((value, position))
}

fn read_head(bytes: &[u8], offset: usize) -> Result<(u8, u64, usize), ProtoError> {
    let first = *bytes.get(offset).ok_or(ProtoError::Truncated)?;
    let major = first >> 5;
    let additional = first & 0x1f;
    let (argument, position) = match additional {
        0..=23 => (u64::from(additional), offset + 1),
        24 => {
            let value = u64::from(*bytes.get(offset + 1).ok_or(ProtoError::Truncated)?);
            if value < 24 {
                return Err(ProtoError::NonCanonical("整数未用最短形式".to_owned()));
            }
            (value, offset + 2)
        }
        25 => {
            let slice = bytes
                .get(offset + 1..offset + 3)
                .ok_or(ProtoError::Truncated)?;
            let value = u64::from(u16::from_be_bytes([slice[0], slice[1]]));
            if value <= u8::MAX as u64 {
                return Err(ProtoError::NonCanonical("整数未用最短形式".to_owned()));
            }
            (value, offset + 3)
        }
        26 => {
            let slice = bytes
                .get(offset + 1..offset + 5)
                .ok_or(ProtoError::Truncated)?;
            let value = u64::from(u32::from_be_bytes([slice[0], slice[1], slice[2], slice[3]]));
            if value <= u16::MAX as u64 {
                return Err(ProtoError::NonCanonical("整数未用最短形式".to_owned()));
            }
            (value, offset + 5)
        }
        27 => {
            let slice = bytes
                .get(offset + 1..offset + 9)
                .ok_or(ProtoError::Truncated)?;
            let value = u64::from_be_bytes([
                slice[0], slice[1], slice[2], slice[3], slice[4], slice[5], slice[6], slice[7],
            ]);
            if value <= u32::MAX as u64 {
                return Err(ProtoError::NonCanonical("整数未用最短形式".to_owned()));
            }
            (value, offset + 9)
        }
        _ => {
            return Err(ProtoError::NonCanonical(
                "不允许不定长/保留的 additional info".to_owned(),
            ))
        }
    };
    Ok((major, argument, position))
}

fn usize_of(value: u64) -> Result<usize, ProtoError> {
    usize::try_from(value).map_err(|_| ProtoError::TooLarge)
}

/// 把 CDE 值按“恰好 n 个连续整数键”的 record map 拆开。
pub(crate) fn into_record_fields(
    value: CdeValue,
    expected_count: usize,
) -> Result<Vec<CdeValue>, ProtoError> {
    let CdeValue::Map(entries) = value else {
        return Err(ProtoError::Invalid("record 必须是 map".to_owned()));
    };
    if entries.len() != expected_count {
        return Err(ProtoError::Invalid(format!(
            "record 字段数 {}，期望 {expected_count}",
            entries.len()
        )));
    }
    let mut fields = Vec::with_capacity(expected_count);
    for (expected, (key, value)) in entries.into_iter().enumerate() {
        let expected = expected as u64;
        if key != expected {
            if key > expected && key < expected_count as u64 {
                return Err(ProtoError::MissingField(expected));
            }
            return Err(ProtoError::UnknownField(key));
        }
        fields.push(value);
    }
    Ok(fields)
}

pub(crate) fn expect_uint(value: CdeValue, field: &str) -> Result<u64, ProtoError> {
    match value {
        CdeValue::UInt(value) => Ok(value),
        other => Err(ProtoError::Invalid(format!(
            "{field} 必须是 uint，实际 {other:?}"
        ))),
    }
}

pub(crate) fn expect_u32(value: CdeValue, field: &str) -> Result<u32, ProtoError> {
    let value = expect_uint(value, field)?;
    u32::try_from(value).map_err(|_| ProtoError::Invalid(format!("{field} 超出 u32")))
}

pub(crate) fn expect_u16(value: CdeValue, field: &str) -> Result<u16, ProtoError> {
    let value = expect_uint(value, field)?;
    u16::try_from(value).map_err(|_| ProtoError::Invalid(format!("{field} 超出 u16")))
}

pub(crate) fn expect_u8(value: CdeValue, field: &str) -> Result<u8, ProtoError> {
    let value = expect_uint(value, field)?;
    u8::try_from(value).map_err(|_| ProtoError::Invalid(format!("{field} 超出 u8 标签范围")))
}

/// 协议内所有 u64 逻辑值必须 ≤ i64::MAX（SQLite INTEGER）。
pub(crate) fn expect_i64_max(value: CdeValue, field: &str) -> Result<u64, ProtoError> {
    let value = expect_uint(value, field)?;
    if value > i64::MAX as u64 {
        return Err(ProtoError::Invalid(format!("{field} 超过 i64::MAX")));
    }
    Ok(value)
}

pub(crate) fn expect_bool(value: CdeValue, field: &str) -> Result<bool, ProtoError> {
    match value {
        CdeValue::Bool(value) => Ok(value),
        other => Err(ProtoError::Invalid(format!(
            "{field} 必须是 bool，实际 {other:?}"
        ))),
    }
}

pub(crate) fn expect_bytes(value: CdeValue, field: &str) -> Result<Vec<u8>, ProtoError> {
    match value {
        CdeValue::Bytes(value) => Ok(value),
        other => Err(ProtoError::Invalid(format!(
            "{field} 必须是 byte string，实际 {other:?}"
        ))),
    }
}

pub(crate) fn expect_text(value: CdeValue, field: &str) -> Result<String, ProtoError> {
    match value {
        CdeValue::Text(value) => Ok(value),
        other => Err(ProtoError::Invalid(format!(
            "{field} 必须是 text，实际 {other:?}"
        ))),
    }
}

pub(crate) fn expect_array(value: CdeValue, field: &str) -> Result<Vec<CdeValue>, ProtoError> {
    match value {
        CdeValue::Array(value) => Ok(value),
        other => Err(ProtoError::Invalid(format!(
            "{field} 必须是 array，实际 {other:?}"
        ))),
    }
}

pub(crate) fn expect_bytes_n<const N: usize>(
    value: CdeValue,
    field: &str,
) -> Result<[u8; N], ProtoError> {
    let bytes = expect_bytes(value, field)?;
    bytes.try_into().map_err(|bytes: Vec<u8>| {
        ProtoError::Invalid(format!("{field} 长度应为 {N}，实际 {}", bytes.len()))
    })
}
