//! 坐标 / Unicode 单位换算的错误类型。
//!
//! 依据 ADR 0002 §4（坐标强类型、互不隐式转换）与 §11.1（单位夹具）。
//!
//! **错误优先原则**：越界、落在非 scalar 边界、落在代理对中间、非法半代理，
//! 一律返回 [`CoordError`]，绝不静默截断、钳制或四舍五入。编辑器内核里任何
//! "顺手纠偏"都会在下游变成错位的光标、错删的字或损坏的区间 op。
//!
//! 错误里携带的 `offset` / `index` 是**调用方传入的原始值**（不是钳制后的值），
//! `len` 是对应单位下该字符串的总长度；这样报错能直接定位调用点。

use std::error::Error;
use std::fmt;

/// 坐标换算与区间校验失败的全部原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoordError {
    /// UTF-8 字节偏移超出字符串长度（`offset == len` 是合法的尾边界）。
    ByteOutOfBounds { offset: usize, len: usize },
    /// 字节偏移落在 UTF-8 多字节字符内部，不是 scalar 边界。
    NotScalarBoundary { offset: usize, len: usize },
    /// Unicode scalar 序号超出长度（`index == len` 是合法的尾边界）。
    ScalarOutOfBounds { index: usize, len: usize },
    /// UTF-16 code unit 序号超出长度（`index == len` 是合法的尾边界）。
    Utf16OutOfBounds { index: usize, len: usize },
    /// UTF-16 code unit 序号落在代理对中间（高代理与低代理之间），
    /// 该位置不对应任何 Unicode scalar 边界。
    Utf16InsideSurrogatePair { index: usize, len: usize },
    /// UTF-16 输入里存在未配对的半代理（孤立的高代理或低代理）。
    UnpairedSurrogate { index: usize, len: usize },
    /// 区间起点晚于终点（半开区间要求 `start <= end`）。
    ReversedRange { start: usize, end: usize },
    /// 区间端点超出 source 长度。
    RangeOutOfBounds {
        start: usize,
        end: usize,
        len: usize,
    },
}

impl fmt::Display for CoordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ByteOutOfBounds { offset, len } => {
                write!(f, "UTF-8 字节偏移 {offset} 越界（source 长度 {len}）")
            }
            Self::NotScalarBoundary { offset, len } => write!(
                f,
                "UTF-8 字节偏移 {offset} 不在 scalar 边界上（source 长度 {len}）"
            ),
            Self::ScalarOutOfBounds { index, len } => write!(
                f,
                "Unicode scalar 序号 {index} 越界（source 共 {len} 个 scalar）"
            ),
            Self::Utf16OutOfBounds { index, len } => write!(
                f,
                "UTF-16 code unit 序号 {index} 越界（source 共 {len} 个 code unit）"
            ),
            Self::Utf16InsideSurrogatePair { index, len } => write!(
                f,
                "UTF-16 code unit 序号 {index} 落在代理对中间（source 共 {len} 个 code unit）"
            ),
            Self::UnpairedSurrogate { index, len } => write!(
                f,
                "UTF-16 输入在序号 {index} 处有未配对的半代理（共 {len} 个 code unit）"
            ),
            Self::ReversedRange { start, end } => {
                write!(f, "区间起止倒序：start={start} 晚于 end={end}")
            }
            Self::RangeOutOfBounds { start, end, len } => {
                write!(f, "区间 [{start}, {end}) 超出 source 长度 {len}")
            }
        }
    }
}

impl Error for CoordError {}
