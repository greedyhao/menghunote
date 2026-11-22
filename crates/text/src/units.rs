//! `&str` 上的 Unicode 单位度量、边界校验与数值换算。
//!
//! ## 单位语义
//!
//! | 函数 | 单位 | 典型来源 |
//! | --- | --- | --- |
//! | [`byte_len`] | UTF-8 bytes | 持久化真源（Markdown 源码）的切分单位 |
//! | [`scalar_len`] | Unicode scalars | Loro / 部分 CRDT 的默认索引单位（ADR 0002 §11.1 要求实测） |
//! | [`utf16_len`] | UTF-16 code units | Windows 输入 / 剪贴板 / IME 的原生单位 |
//! | [`grapheme_len`] | 扩展 grapheme clusters | **交互边界**（光标、按字删除、命中），**不是 CRDT 索引** |
//!
//! ## 为什么是 `&str`
//!
//! 本切片**不引入 ropey**：所有换算都从 `&str` 首部线性扫描，语义上就是"参考答案"。
//! ADR 0002 §4 要求最终由 Rope 的 chunk 统计 / 前缀索引完成（不每次从文首扫描），
//! 将来 rope 版本的实现必须与本模块逐项等价，并复用同一批测试。
//!
//! ## grapheme 的定位（重要）
//!
//! [`grapheme_len`] / [`grapheme_boundaries`] 用 `unicode-segmentation` 计算，它们只是
//! **交互边界**（一次退格吃掉整个 `e` + 组合重音、光标不落在 ZWJ 两侧）。**不得**把
//! grapheme 写进持久化 op：Unicode 表版本不同会让不同机器对同一份字节切出不同边界，
//! 进而让 CRDT 索引分叉（ADR 0002 §4「grapheme 是交互边界，不是 CRDT 索引」）。
//!
//! ## 换算的边界规则
//!
//! - UTF-8 byte 偏移必须 `<= byte_len`，且必须落在 scalar 边界；
//! - UTF-16 code unit 序号必须 `<= utf16_len`，且不能落在代理对中间；
//! - Unicode scalar 序号必须 `<= scalar_len`（等于即尾边界）；
//! - 非法 UTF-16 输入（孤立半代理）不替换成 U+FFFD，直接报错。
//!
//! 所有失败都映射到 [`CoordError`]，**不做截断或四舍五入**。

use unicode_segmentation::UnicodeSegmentation;

use crate::error::CoordError;

/// UTF-8 字节长度（等价于 [`str::len`]，命名上显式区分单位）。
#[must_use]
pub fn byte_len(text: &str) -> usize {
    text.len()
}

/// Unicode scalar 数量（[`str::chars`] 的计数）。
#[must_use]
pub fn scalar_len(text: &str) -> usize {
    text.chars().count()
}

/// UTF-16 code unit 数量（非 BMP 字符算 2 个）。
#[must_use]
pub fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// 扩展 grapheme cluster 数量。
///
/// 这是**交互边界**的计数，不是 CRDT / 区间 op 的索引单位；见模块文档。
#[must_use]
pub fn grapheme_len(text: &str) -> usize {
    text.graphemes(true).count()
}

/// `offset` 是否落在合法 scalar 边界上。
///
/// `offset == byte_len` 是合法的尾边界；`offset > byte_len` 返回 `false`
/// （与 [`str::is_char_boundary`] 语义一致）。需要区分"越界"与"字符内部"时用
/// [`check_scalar_boundary`]。
#[must_use]
pub fn is_scalar_boundary(text: &str, offset: usize) -> bool {
    text.is_char_boundary(offset)
}

/// 校验 `offset` 是合法 scalar 边界，失败时给出精确原因。
pub fn check_scalar_boundary(text: &str, offset: usize) -> Result<(), CoordError> {
    let len = text.len();
    if offset > len {
        return Err(CoordError::ByteOutOfBounds { offset, len });
    }
    if !text.is_char_boundary(offset) {
        return Err(CoordError::NotScalarBoundary { offset, len });
    }
    Ok(())
}

/// 返回全部 scalar 边界的 UTF-8 字节偏移（严格递增）。
///
/// 空串返回 `[0]`；首项恒为 0，末项恒为 [`byte_len`]。
#[must_use]
pub fn scalar_boundaries(text: &str) -> Vec<usize> {
    let mut boundaries: Vec<usize> = text.char_indices().map(|(byte, _)| byte).collect();
    boundaries.push(text.len());
    boundaries
}

/// 返回全部 grapheme 边界的 UTF-8 字节偏移（严格递增）。
///
/// **交互边界，不是 CRDT 索引**；见模块文档。空串返回 `[0]`。
/// grapheme 边界必然也是 scalar 边界，因此结果是 [`scalar_boundaries`] 的子集。
#[must_use]
pub fn grapheme_boundaries(text: &str) -> Vec<usize> {
    let mut boundaries: Vec<usize> = text.grapheme_indices(true).map(|(byte, _)| byte).collect();
    boundaries.push(text.len());
    boundaries
}

/// `offset` 是否是 grapheme 边界（交互边界）。
///
/// `offset > byte_len` 或落在 grapheme 内部都返回 `false`；`offset == byte_len`
/// 是合法尾边界。每次调用 O(n)，仅用于校验 / 测试，不要放进热路径。
#[must_use]
pub fn is_grapheme_boundary(text: &str, offset: usize) -> bool {
    if offset > text.len() {
        return false;
    }
    if offset == text.len() {
        return true;
    }
    text.grapheme_indices(true).any(|(byte, _)| byte == offset)
}

/// 把原始 UTF-16 code unit 输入（Windows IME / 剪贴板 / `WM_CHAR` 的原生单位）
/// 转成 `String`。
///
/// 存在未配对的半代理时返回 [`CoordError::UnpairedSurrogate`]，**不做 U+FFFD
/// 替换兜底**——IME 提交的半个代理对是真实的数据损坏，必须让上层看见。
pub fn utf16_to_string(units: &[u16]) -> Result<String, CoordError> {
    let mut text = String::with_capacity(units.len());
    let mut index = 0usize;
    for decoded in char::decode_utf16(units.iter().copied()) {
        match decoded {
            Ok(ch) => {
                index += ch.len_utf16();
                text.push(ch);
            }
            Err(_) => {
                return Err(CoordError::UnpairedSurrogate {
                    index,
                    len: units.len(),
                });
            }
        }
    }
    Ok(text)
}

/// `&str` 上把 UTF-8 字节偏移换成 Unicode scalar 序号。
pub(crate) fn byte_to_scalar(text: &str, offset: usize) -> Result<usize, CoordError> {
    check_scalar_boundary(text, offset)?;
    // 本切片从首部扫描；ADR 0002 §4 要求后续改为 Rope 前缀索引。
    Ok(text[..offset].chars().count())
}

/// `&str` 上把 UTF-8 字节偏移换成 UTF-16 code unit 序号。
pub(crate) fn byte_to_utf16(text: &str, offset: usize) -> Result<usize, CoordError> {
    check_scalar_boundary(text, offset)?;
    Ok(text[..offset].encode_utf16().count())
}

/// `&str` 上把 Unicode scalar 序号换成 UTF-8 字节偏移。
pub(crate) fn scalar_to_byte(text: &str, index: usize) -> Result<usize, CoordError> {
    let len = scalar_len(text);
    if index > len {
        return Err(CoordError::ScalarOutOfBounds { index, len });
    }
    if index == len {
        return Ok(text.len());
    }
    match text.char_indices().nth(index) {
        Some((byte, _)) => Ok(byte),
        // `index < len` 时不可能走到这里；显式返回错误而不是 unwrap。
        None => Err(CoordError::ScalarOutOfBounds { index, len }),
    }
}

/// `&str` 上把 Unicode scalar 序号换成 UTF-16 code unit 序号。
pub(crate) fn scalar_to_utf16(text: &str, index: usize) -> Result<usize, CoordError> {
    let len = scalar_len(text);
    if index > len {
        return Err(CoordError::ScalarOutOfBounds { index, len });
    }
    Ok(text.chars().take(index).map(char::len_utf16).sum())
}

/// `&str` 上把 UTF-16 code unit 序号换成 UTF-8 字节偏移。
///
/// 序号落在代理对中间（高代理之后、低代理之前）时返回
/// [`CoordError::Utf16InsideSurrogatePair`]：那里没有对应的 scalar 边界。
pub(crate) fn utf16_to_byte(text: &str, index: usize) -> Result<usize, CoordError> {
    let len = utf16_len(text);
    if index > len {
        return Err(CoordError::Utf16OutOfBounds { index, len });
    }

    let mut consumed = 0usize;
    for (byte, ch) in text.char_indices() {
        if consumed == index {
            return Ok(byte);
        }
        let width = ch.len_utf16();
        if index < consumed + width {
            return Err(CoordError::Utf16InsideSurrogatePair { index, len });
        }
        consumed += width;
    }

    if consumed == index {
        Ok(text.len())
    } else {
        // 上面的 `index > len` 已排除这种情况；保留显式错误，不做 unwrap。
        Err(CoordError::Utf16OutOfBounds { index, len })
    }
}
