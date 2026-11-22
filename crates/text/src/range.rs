//! source 上的半开区间 `[start, end)`。
//!
//! 依据 ADR 0002 §4：段记录、source selection、CRDT 区间 op 都以 source 区间为准。
//! 本切片只提供**类型 + 校验**，供后续 `ProjectionMap`（每块 `Vec<Segment>`）复用；
//! 不实现映射本身。

use crate::coords::SourceByte;
use crate::error::CoordError;
use crate::units;

/// source 上的半开区间 `[start, end)`：`start` 在内，`end` 不在内。
///
/// 空区间的表示是 `start == end`（合法）；`end == source_len` 合法（文档尾边界）。
/// 字段私有，只能经 [`SourceRange::new`] / [`SourceRange::within`] 构造，
/// 因此"已构造的 `SourceRange` 一定通过越界与顺序校验"是可依赖的不变量。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SourceRange {
    start: SourceByte,
    end: SourceByte,
}

impl SourceRange {
    /// 校验 `start <= end` 且两端都在 `source_len` 以内
    /// （`end == source_len` 合法，`end > source_len` 报错）。
    ///
    /// 任务书写的是 `new(start, end)`，但"都在 source 长度内"这条校验没有长度
    /// 就无法执行；签名显式带上 `source_len`，宁可变长一点也不静默跳过越界检查。
    /// 需要额外保证两端落在 scalar 边界时用 [`SourceRange::within`]。
    pub fn new(start: SourceByte, end: SourceByte, source_len: usize) -> Result<Self, CoordError> {
        let (start_at, end_at) = (start.get(), end.get());
        if start_at > end_at {
            return Err(CoordError::ReversedRange {
                start: start_at,
                end: end_at,
            });
        }
        if end_at > source_len {
            return Err(CoordError::RangeOutOfBounds {
                start: start_at,
                end: end_at,
                len: source_len,
            });
        }
        Ok(Self { start, end })
    }

    /// 在具体 source 上完成**全部**校验：顺序、长度，以及两端都是 scalar 边界。
    ///
    /// 这是 `ProjectionMap` 段区间推荐使用的构造方式：非 scalar 边界的区间一旦
    /// 进入映射层，后续任何 `&source[start..end]` 切分都会 panic 或错位。
    pub fn within(source: &str, start: SourceByte, end: SourceByte) -> Result<Self, CoordError> {
        let range = Self::new(start, end, units::byte_len(source))?;
        units::check_scalar_boundary(source, range.start.get())?;
        units::check_scalar_boundary(source, range.end.get())?;
        Ok(range)
    }

    /// 起点（含）。
    #[must_use]
    pub const fn start(self) -> SourceByte {
        self.start
    }

    /// 终点（不含）。
    #[must_use]
    pub const fn end(self) -> SourceByte {
        self.end
    }

    /// 区间长度（UTF-8 bytes）。
    #[must_use]
    pub const fn len(self) -> usize {
        self.end.get() - self.start.get()
    }

    /// 是否为空区间（`start == end`）。
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.start.get() == self.end.get()
    }

    /// `at` 是否落在半开区间内：`start <= at < end`（终点本身不算）。
    #[must_use]
    pub fn contains(self, at: SourceByte) -> bool {
        self.start.get() <= at.get() && at.get() < self.end.get()
    }
}
