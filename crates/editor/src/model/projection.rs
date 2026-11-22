//! 逻辑显示投影（ADR 0002 §4）：每块一个 `Vec<Segment>`，**不用"偏移差"**。
//!
//! ## 本切片的范围
//!
//! S1 只承诺 **identity 投影**（source == display，不隐藏 Markdown 标记）：段表里
//! 恒为一条 [`SegmentKind::Copy`]。但结构与查询都按 ADR 0002 §4 的最终形态实现——
//! 源区间 / 显示 byte 区间 / kind / 边界 bias 都逐段记录，查询通过**段表定位**再
//! 段内换算，绝不使用 `display = source + 常数偏移` 这类近似。
//!
//! `Elide` / `Replace` / `Object` 的段类型与构造器先预留（供 S2 使用）；S2 的
//! 多对一映射语义（隐藏标记的边界 bias）尚未实现，查询命中这些段时返回
//! [`ProjectionError::UnsupportedSegment`]，**不猜**。
//!
//! ADR 0002 §4 里段还记录"所属构造"（construct）：该字段要等 S2 的解析器一起
//! 引入，S1 没有消费者，先不造空类型，只保留 kind / 边界 bias 两个维度。

use std::error::Error;
use std::fmt;

use menghunote_text::{units, DisplayByte, SourceByte, SourceRange};

/// 段类型（ADR 0002 §4 的最小集合）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SegmentKind {
    /// source 与 display 逐字节相同。
    Copy,
    /// source 非空、display 为空（隐藏的 Markdown 标记）。
    Elide,
    /// 转义 / 换行显示替代等：display 文本与 source 不同。
    Replace,
    /// widget / 图片等对象，对应完整源码范围。
    Object,
}

/// 多对一区间的边界取向（ADR 0002 §4：隐藏区间的内部点没有唯一逆像）。
///
/// S1 的 identity 段一律 [`BoundaryBias::After`]；真正使用它的是 S2 的 Elide/Replace。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BoundaryBias {
    /// 取靠前（左）一侧。
    Before,
    /// 取靠后（右）一侧。
    After,
}

/// 一个投影段：source 区间 ↔ 逻辑显示串的 byte 区间。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    source: SourceRange,
    display_start: DisplayByte,
    display_end: DisplayByte,
    kind: SegmentKind,
    bias: BoundaryBias,
}

impl Segment {
    /// 构造 Copy 段：显示区间宽度必须等于 source 区间宽度。
    pub fn copy(source: SourceRange, display_start: DisplayByte) -> Result<Self, ProjectionError> {
        let display_end = display_start
            .get()
            .checked_add(source.len())
            .ok_or(ProjectionError::Overflow)?;
        Ok(Self {
            source,
            display_start,
            display_end: DisplayByte::new(display_end),
            kind: SegmentKind::Copy,
            bias: BoundaryBias::After,
        })
    }

    /// 构造 Elide 段：source 非空、显示宽度为零。
    ///
    /// S1 不生成这种段（identity 没有隐藏），这里只为 S2 与结构测试预留。
    pub fn elide(source: SourceRange, display_at: DisplayByte) -> Result<Self, ProjectionError> {
        if source.is_empty() {
            return Err(ProjectionError::InvalidLayout(
                "Elide 段的 source 区间不能为空",
            ));
        }
        Ok(Self {
            source,
            display_start: display_at,
            display_end: display_at,
            kind: SegmentKind::Elide,
            bias: BoundaryBias::After,
        })
    }

    /// source 半开区间。
    #[must_use]
    pub const fn source(self) -> SourceRange {
        self.source
    }

    /// 显示串起始 byte（含）。
    #[must_use]
    pub const fn display_start(self) -> DisplayByte {
        self.display_start
    }

    /// 显示串结束 byte（不含）。
    #[must_use]
    pub const fn display_end(self) -> DisplayByte {
        self.display_end
    }

    /// 段类型。
    #[must_use]
    pub const fn kind(self) -> SegmentKind {
        self.kind
    }

    /// 边界 bias（S2 使用）。
    #[must_use]
    pub const fn bias(self) -> BoundaryBias {
        self.bias
    }

    /// 显示宽度（UTF-8 bytes）。
    #[must_use]
    pub fn display_len(self) -> usize {
        self.display_end.get() - self.display_start.get()
    }
}

/// 一段文本的投影表。
///
/// 字段私有：只能经 [`ProjectionMap::identity`] / [`ProjectionMap::from_segments`]
/// 构造，因此恒满足"段按 source 与 display 双向连续覆盖全文"的不变量。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectionMap {
    source_len: usize,
    display_len: usize,
    segments: Vec<Segment>,
}

impl ProjectionMap {
    /// S1 的 identity 投影：整篇一条 Copy 段（空文档零段）。
    pub fn identity(source: &str) -> Result<Self, ProjectionError> {
        let source_len = units::byte_len(source);
        if source_len == 0 {
            return Ok(Self {
                source_len: 0,
                display_len: 0,
                segments: Vec::new(),
            });
        }
        let range = SourceRange::within(source, SourceByte::new(0), SourceByte::new(source_len))
            .map_err(|_| {
                ProjectionError::InvalidLayout("identity 的 source 端点必须是 scalar 边界")
            })?;
        let segment = Segment::copy(range, DisplayByte::new(0))?;
        Ok(Self {
            source_len,
            display_len: source_len,
            segments: vec![segment],
        })
    }

    /// 由显式段表构造（S2 与结构测试用）。
    ///
    /// 校验：段按 source 与 display 都从 0 起连续覆盖；Copy 段两侧宽度相等；
    /// 端点落在 scalar 边界；空文档只允许空段表。
    pub fn from_segments(source: &str, segments: Vec<Segment>) -> Result<Self, ProjectionError> {
        let source_len = units::byte_len(source);
        if source_len == 0 && !segments.is_empty() {
            return Err(ProjectionError::InvalidLayout("空文档不允许非空段表"));
        }

        let mut source_cursor = 0usize;
        let mut display_cursor = 0usize;
        for segment in &segments {
            if segment.source.start().get() != source_cursor {
                return Err(ProjectionError::InvalidLayout(
                    "source 段必须从 0 起连续覆盖且互不重叠",
                ));
            }
            if segment.display_start().get() != display_cursor {
                return Err(ProjectionError::InvalidLayout("display 段必须连续覆盖"));
            }
            units::check_scalar_boundary(source, segment.source.start().get())
                .map_err(|_| ProjectionError::InvalidLayout("source 段起点不在 scalar 边界上"))?;
            units::check_scalar_boundary(source, segment.source.end().get())
                .map_err(|_| ProjectionError::InvalidLayout("source 段终点不在 scalar 边界上"))?;

            if segment.kind == SegmentKind::Copy && segment.source.len() != segment.display_len() {
                return Err(ProjectionError::InvalidLayout(
                    "Copy 段的 source / display 宽度必须相等",
                ));
            }
            if segment.kind == SegmentKind::Elide && segment.display_len() != 0 {
                return Err(ProjectionError::InvalidLayout(
                    "Elide 段的 display 宽度必须为零",
                ));
            }

            source_cursor = segment.source.end().get();
            display_cursor = segment.display_end().get();
        }

        if source_cursor != source_len {
            return Err(ProjectionError::InvalidLayout("段表未完整覆盖 source"));
        }
        Ok(Self {
            source_len,
            display_len: display_cursor,
            segments,
        })
    }

    /// source 长度（UTF-8 bytes）。
    #[must_use]
    pub const fn source_len(&self) -> usize {
        self.source_len
    }

    /// 逻辑显示串长度（UTF-8 bytes）。
    #[must_use]
    pub const fn display_len(&self) -> usize {
        self.display_len
    }

    /// 段表（只读）。
    #[must_use]
    pub fn segments(&self) -> &[Segment] {
        &self.segments
    }

    /// 是否 identity（所有段都是等宽 Copy 且 display 偏移与 source 一致）。
    #[must_use]
    pub fn is_identity(&self) -> bool {
        self.segments.iter().all(|segment| {
            segment.kind == SegmentKind::Copy
                && segment.display_start().get() == segment.source.start().get()
                && segment.display_end().get() == segment.source.end().get()
        })
    }

    /// source byte → 逻辑显示 byte。
    ///
    /// 先在**段表**里二分定位，再在段内换算；不使用整篇偏移差。
    pub fn display_byte(&self, source: SourceByte) -> Result<DisplayByte, ProjectionError> {
        let at = source.get();
        if at > self.source_len {
            return Err(ProjectionError::SourceByteOutOfBounds {
                offset: at,
                len: self.source_len,
            });
        }
        // 空文档（零段）唯一合法点。
        if self.segments.is_empty() {
            return Ok(DisplayByte::new(0));
        }
        // 尾边界直接取最后一段的 display_end（避免把"文档末尾"误当成段内点）。
        if at == self.source_len {
            let last = self.segments.last();
            return Ok(last.map_or(DisplayByte::new(0), |segment| segment.display_end()));
        }

        let index = self
            .segments
            .partition_point(|segment| segment.source.start().get() <= at);
        let segment = index
            .checked_sub(1)
            .and_then(|index| self.segments.get(index))
            .ok_or(ProjectionError::InvalidLayout("source 点没有落在任何段内"))?;
        match segment.kind {
            SegmentKind::Copy => {
                let offset = at - segment.source.start().get();
                Ok(DisplayByte::new(segment.display_start().get() + offset))
            }
            other => Err(ProjectionError::UnsupportedSegment(other)),
        }
    }

    /// 逻辑显示 byte → source byte。
    pub fn source_byte(&self, display: DisplayByte) -> Result<SourceByte, ProjectionError> {
        let at = display.get();
        if at > self.display_len {
            return Err(ProjectionError::DisplayByteOutOfBounds {
                offset: at,
                len: self.display_len,
            });
        }
        if self.segments.is_empty() {
            return Ok(SourceByte::new(0));
        }
        if at == self.display_len {
            let last = self.segments.last();
            return Ok(last.map_or(SourceByte::new(0), |segment| segment.source.end()));
        }

        let index = self
            .segments
            .partition_point(|segment| segment.display_start().get() <= at);
        let segment = index
            .checked_sub(1)
            .and_then(|index| self.segments.get(index))
            .ok_or(ProjectionError::InvalidLayout("display 点没有落在任何段内"))?;
        match segment.kind {
            SegmentKind::Copy => {
                let offset = at - segment.display_start().get();
                Ok(SourceByte::new(segment.source.start().get() + offset))
            }
            other => Err(ProjectionError::UnsupportedSegment(other)),
        }
    }
}

/// 投影表构造 / 查询失败。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectionError {
    /// source byte 越界。
    SourceByteOutOfBounds { offset: usize, len: usize },
    /// 逻辑显示 byte 越界。
    DisplayByteOutOfBounds { offset: usize, len: usize },
    /// 该段类型需要 S2 的多对一语义，S1 拒绝猜测。
    UnsupportedSegment(SegmentKind),
    /// 段表布局不合法（构造期校验）。
    InvalidLayout(&'static str),
    /// 偏移相加溢出。
    Overflow,
}

impl fmt::Display for ProjectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SourceByteOutOfBounds { offset, len } => {
                write!(f, "source byte {offset} 越界（source 长度 {len}）")
            }
            Self::DisplayByteOutOfBounds { offset, len } => {
                write!(f, "display byte {offset} 越界（display 长度 {len}）")
            }
            Self::UnsupportedSegment(kind) => {
                write!(f, "{kind:?} 段的多对一映射属于 S2，S1 不做猜测")
            }
            Self::InvalidLayout(reason) => write!(f, "投影段表布局非法：{reason}"),
            Self::Overflow => write!(f, "投影偏移相加溢出"),
        }
    }
}

impl Error for ProjectionError {}
