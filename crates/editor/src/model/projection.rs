//! 逻辑显示投影（ADR 0002 §4）：每块一个 `Vec<Segment>`，**不用"偏移差"**。
//!
//! ## 段模型
//!
//! - [`SegmentKind::Copy`]：可见段，source 与 display 逐字节相同（等宽）；
//! - [`SegmentKind::Elide`]：省略段，source 非空、display 宽度为 0
//!   （Markdown 标记被隐藏）；
//! - `Replace` / `Object` 仍为后续切片预留，本层不生成，查询遇到时返回 `None`
//!   （构造器 `from_segments` 也会拒绝它们）。
//!
//! 段表**完整、有序、无重叠**地覆盖 `[0, source_len)`，且 display 区间连续；
//! 查询一律先在段表里定位、再段内换算，绝不用"display = source + 常数偏移"。
//!
//! ## 偏置规则（写死，测试钉住）
//!
//! 多对一必然有歧义，这里明确选择：
//!
//! 1. **source → display**（[`ProjectionMap::display_byte_at_source`]）：
//!    - source 落在 `Elide` 段**内部**（或正好在段首）→ 取该省略段的**起点**
//!      （也就是左侧/Before，映射到隐藏内容之前的 display 位置）；
//!    - source 正好在省略段**末尾**（下一可见段起点）→ 取下一可见段起点
//!      （右侧/After，display 位置相同）；
//!    - source == `source_len`（文档尾）→ display 末尾；
//!    - 越界 → `None`（不静默返回 0）。
//! 2. **display → source**（[`ProjectionMap::source_byte_at_display`]）：
//!    - display 落在 `Copy` 段内部 → 段内线性换算；
//!    - display 正好在"隐藏内容被抹掉"的接缝（前一可见段末尾 == 后一可见段开头）→
//!      取**右侧**（After）：下一可见段的 source 起点；
//!    - display == `display_len`（显示末尾端点）→ `source_len`（**定义**为文档尾）；
//!    - 越界 → `None`。
//! 3. **display 区间 → source 区间**（[`ProjectionMap::source_range_of_display_range`]）：
//!    - 起点用 After 偏置、终点用 Before 偏置，得到**最小** source 区间：
//!      只包含显示范围对应的可见文字，以及**严格位于其内部**的省略段；
//!      两端紧邻的隐藏标记不包含；
//!    - 空 display 区间 → 折叠到 After 偏置的 source 位置；
//!    - 端点倒序 / 越界 → `None`。
//!
//! ## 可见性不变量
//!
//! display 串 = 所有 `Copy` 段 source 内容的拼接（用 [`ProjectionMap::display_text`]
//! 直接取）；`Elide` 段不贡献任何 display 长度。可见位置满足
//! display → source → display 往返一致（source → display 在省略段边界是多对一，
//! 不承诺反解）。

use std::borrow::Cow;
use std::error::Error;
use std::fmt;
use std::ops::Range;

use menghunote_text::{units, DisplayByte, SourceByte, SourceRange};

use crate::model::markdown::MarkdownDocument;
use crate::model::selection::Selection;

/// 命中时的**左右偏置**（点击落点在字形哪一半）。
///
/// 隐藏标记宽度为 0，同一个 display 位置可能对应两个 source 位置：
/// - [`Bias::Right`] = 该位置**之后**（右侧，原行为：`source_byte_at_display`）；
/// - [`Bias::Left`] = 该位置**之前**（左侧，跨过零宽隐藏标记取前一个 source 位置）。
///
/// 命中时由"点在字形左半/右半"决定：右半取 `Left`、左半取 `Right`，
/// 这样点 `` `abc` `` 末尾字母的右半，光标落在**构造内**（闭合反引号之前），
/// 点左半则落在字母之间——与用户在行首/行尾的直觉一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Bias {
    /// 取靠前（左）一侧：跳过零宽隐藏标记，落在标记**之前**。
    Left,
    /// 取靠后（右）一侧：原 `source_byte_at_display` 的行为。
    Right,
}

/// 段类型（ADR 0002 §4 的最小集合）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SegmentKind {
    /// source 与 display 逐字节相同。
    Copy,
    /// source 非空、display 为空（隐藏的 Markdown 标记）。
    Elide,
    /// 转义 / 换行显示替代等：display 文本与 source 不同（后续切片）。
    Replace,
    /// widget / 图片等对象，对应完整源码范围（后续切片）。
    Object,
}

/// 多对一区间的边界取向（ADR 0002 §4：隐藏区间的内部点没有唯一逆像）。
///
/// `Copy` 段内部无歧义；只有省略段边界用它区分左右侧。规则见模块文档。
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

    /// 边界 bias（历史预留字段；真实偏置规则见模块文档）。
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
/// 字段私有：只能经 [`ProjectionMap::identity`] /
/// [`ProjectionMap::from_markdown`] / [`ProjectionMap::from_segments`] 构造，
/// 因此恒满足"段双向连续覆盖 + display 拼接一致"的不变量。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectionMap {
    source_len: usize,
    display_byte_len: usize,
    /// 逻辑显示串（所有 Copy 段拼接；Elide 不出现）。缓存在这里供渲染直接借用。
    display: String,
    segments: Vec<Segment>,
}

impl ProjectionMap {
    /// identity 投影：整篇一条 Copy 段（空文档零段）。
    pub fn identity(source: &str) -> Result<Self, ProjectionError> {
        let source_len = units::byte_len(source);
        let mut segments = Vec::new();
        if source_len > 0 {
            let range =
                SourceRange::within(source, SourceByte::new(0), SourceByte::new(source_len))
                    .map_err(|_| {
                        ProjectionError::InvalidLayout("identity 的 source 端点必须是 scalar 边界")
                    })?;
            segments.push(Segment::copy(range, DisplayByte::new(0))?);
        }
        Self::from_segments(source, segments)
    }

    /// **由 Markdown 语义扫描结果构造真实投影**（S2-2）。
    ///
    /// 隐藏区间来自 [`MarkdownDocument::hidden_ranges_for_selection`]：选区（含
    /// 折叠 caret）相交/覆盖的构造会显现，其余构造的标记进入 `Elide` 段。
    ///
    /// 选区坐标非法（越界）时**保守地不隐藏任何内容**（等价 identity），
    /// 不会 panic；需要显式错误的调用方可用 [`ProjectionMap::try_from_markdown`]。
    #[must_use]
    pub fn from_markdown(doc: &MarkdownDocument, selection: Selection) -> Self {
        match Self::try_from_markdown(doc, selection) {
            Ok(projection) => projection,
            // 防御：正常选区下不可达（Markdown 隐藏区间必然在 source 内且有序）。
            // 万一发生，退回 identity（宁可不隐藏，也不隐藏错误内容）。
            Err(_) => Self::identity(doc.source()).unwrap_or_else(|_| Self {
                source_len: doc.source_len(),
                display_byte_len: 0,
                display: String::new(),
                segments: Vec::new(),
            }),
        }
    }

    /// 严格版本：选区坐标非法或隐藏区间布局异常时返回错误。
    pub fn try_from_markdown(
        doc: &MarkdownDocument,
        selection: Selection,
    ) -> Result<Self, ProjectionError> {
        let hidden = doc
            .hidden_ranges_for_selection(selection)
            .map_err(|_| ProjectionError::InvalidLayout("选区坐标无法换算到 source byte"))?;
        Self::project(doc.source(), &hidden)
    }

    /// 由显式段表构造（结构测试 / 后续 Replace 用）。
    ///
    /// 校验：段按 source 与 display 都从 0 起连续覆盖；Copy 段两侧宽度相等；
    /// Elide 段 display 宽度为零；端点落在 scalar 边界；空文档只允许空段表。
    /// 本层只接受 `Copy` / `Elide`（`Replace` / `Object` 尚未实现）。
    pub fn from_segments(source: &str, segments: Vec<Segment>) -> Result<Self, ProjectionError> {
        let source_len = units::byte_len(source);
        if source_len == 0 && !segments.is_empty() {
            return Err(ProjectionError::InvalidLayout("空文档不允许非空段表"));
        }

        let mut source_cursor = 0usize;
        let mut display_cursor = 0usize;
        let mut display = String::new();

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

            match segment.kind {
                SegmentKind::Copy => {
                    if segment.source.len() != segment.display_len() {
                        return Err(ProjectionError::InvalidLayout(
                            "Copy 段的 source / display 宽度必须相等",
                        ));
                    }
                    display.push_str(
                        &source[segment.source.start().get()..segment.source.end().get()],
                    );
                }
                SegmentKind::Elide => {
                    if segment.display_len() != 0 {
                        return Err(ProjectionError::InvalidLayout(
                            "Elide 段的 display 宽度必须为零",
                        ));
                    }
                }
                SegmentKind::Replace | SegmentKind::Object => {
                    return Err(ProjectionError::InvalidLayout(
                        "本层只支持 Copy / Elide 段（Replace / Object 尚未实现）",
                    ));
                }
            }

            source_cursor = segment.source.end().get();
            display_cursor = segment.display_end().get();
        }

        if source_cursor != source_len {
            return Err(ProjectionError::InvalidLayout("段表未完整覆盖 source"));
        }
        if display.len() != display_cursor {
            return Err(ProjectionError::InvalidLayout(
                "display 拼接长度与段表游标不一致",
            ));
        }
        Ok(Self {
            source_len,
            display_byte_len: display_cursor,
            display,
            segments,
        })
    }

    /// 由 source + 隐藏区间构造段表；隐藏区间必须有序、非空、在 source 内。
    fn project(source: &str, hidden: &[SourceRange]) -> Result<Self, ProjectionError> {
        let source_len = units::byte_len(source);
        let mut segments: Vec<Segment> = Vec::new();
        let mut source_cursor = 0usize;
        let mut display_cursor = 0usize;

        for range in hidden {
            let start = range.start().get();
            let end = range.end().get();
            if start < source_cursor || start >= end || end > source_len {
                return Err(ProjectionError::InvalidLayout(
                    "隐藏区间必须有序、非空、互不重叠且在 source 范围内",
                ));
            }
            if start > source_cursor {
                let visible = Self::source_range(source, source_cursor, start)?;
                let segment = Segment::copy(visible, DisplayByte::new(display_cursor))?;
                display_cursor = segment.display_end().get();
                segments.push(segment);
            }
            let elided = Self::source_range(source, start, end)?;
            segments.push(Segment::elide(elided, DisplayByte::new(display_cursor))?);
            source_cursor = end;
        }

        if source_cursor < source_len {
            let visible = Self::source_range(source, source_cursor, source_len)?;
            let segment = Segment::copy(visible, DisplayByte::new(display_cursor))?;
            segments.push(segment);
        }

        Self::from_segments(source, segments)
    }

    fn source_range(
        source: &str,
        start: usize,
        end: usize,
    ) -> Result<SourceRange, ProjectionError> {
        SourceRange::within(source, SourceByte::new(start), SourceByte::new(end))
            .map_err(|_| ProjectionError::InvalidLayout("投影段端点不在 scalar 边界上"))
    }

    /// source 长度（UTF-8 bytes）。
    #[must_use]
    pub const fn source_len(&self) -> usize {
        self.source_len
    }

    /// 逻辑显示串长度，强类型 [`DisplayByte`]。
    #[must_use]
    pub const fn display_len(&self) -> DisplayByte {
        DisplayByte::new(self.display_byte_len)
    }

    /// 逻辑显示串（省略段不出现）。
    ///
    /// 直接借用内部缓存，渲染层无需重新拼接。
    #[must_use]
    pub fn display_text(&self) -> Cow<'_, str> {
        Cow::Borrowed(self.display.as_str())
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

    /// source byte → 逻辑显示 byte（Option 版）。
    ///
    /// 偏置规则见模块文档：省略段内部取段起点；文档尾取 display 末尾；越界 `None`。
    #[must_use]
    pub fn display_byte_at_source(&self, source: SourceByte) -> Option<DisplayByte> {
        let at = source.get();
        if at > self.source_len {
            return None;
        }
        if at == self.source_len {
            return Some(DisplayByte::new(self.display_byte_len));
        }
        if self.segments.is_empty() {
            // 空文档只有 (0, 0) 一个点。
            return Some(DisplayByte::new(0));
        }
        let index = self
            .segments
            .partition_point(|segment| segment.source.start().get() <= at);
        let segment = self.segments.get(index.checked_sub(1)?)?;
        match segment.kind {
            SegmentKind::Copy => Some(DisplayByte::new(
                segment.display_start().get() + (at - segment.source.start().get()),
            )),
            // 省略段内部（含段首）：Before 偏置 → 段起点对应的 display 位置。
            SegmentKind::Elide => Some(segment.display_start()),
            _ => None,
        }
    }

    /// 逻辑显示 byte → source byte，带**左右偏置**（命中用）。
    ///
    /// - [`Bias::Right`]：原行为（等价于 [`Self::source_byte_at_display`]）：
    ///   接缝取右侧、`display_len` 取 `source_len`；
    /// - [`Bias::Left`]：跳过该位置上的零宽隐藏标记，取标记**之前**的 source 位置：
    ///   - `display_len` 且有尾部隐藏标记（如 `` `abc` `` 的闭合反引号）→ 取标记起点；
    ///   - 中间接缝（如 `**粗**文字` 中 "粗" 之后）→ 取隐藏标记起点；
    ///   - 普通可见字符内部 → 与 `Right` 相同（无歧义）。
    ///
    /// 越界返回 `None`。
    #[must_use]
    pub fn source_byte_at_display_biased(
        &self,
        display: DisplayByte,
        bias: Bias,
    ) -> Option<SourceByte> {
        match bias {
            Bias::Right => self.source_byte_at_display(display),
            Bias::Left => self.source_byte_at_display_left(display),
        }
    }

    /// `Left` 偏置的实现：该 display 位置上若有零宽隐藏段，取段起点。
    fn source_byte_at_display_left(&self, display: DisplayByte) -> Option<SourceByte> {
        let at = display.get();
        if at > self.display_byte_len {
            return None;
        }
        // 该 display 位置上的（可能连续多段）零宽隐藏段。
        let mut start: Option<usize> = None;
        let mut end: Option<usize> = None;
        for segment in &self.segments {
            if segment.kind == SegmentKind::Elide && segment.display_start().get() == at {
                start.get_or_insert(segment.source.start().get());
                end = Some(segment.source.end().get());
            }
        }
        if let (Some(start), Some(end)) = (start, end) {
            debug_assert!(start <= end, "隐藏段起点不应晚于终点");
            return Some(SourceByte::new(start));
        }
        // 没有隐藏段：与 Right 等价（可见字符内部无歧义）。
        self.source_byte_at_display(display)
    }

    /// 逻辑显示 byte → source byte（Option 版）。
    ///
    /// 接缝处取右侧（After）；`display_len` 端点定义为 `source_len`；越界 `None`。
    #[must_use]
    pub fn source_byte_at_display(&self, display: DisplayByte) -> Option<SourceByte> {
        let at = display.get();
        if at > self.display_byte_len {
            return None;
        }
        if at == self.display_byte_len {
            return Some(SourceByte::new(self.source_len));
        }
        if self.segments.is_empty() {
            return Some(SourceByte::new(0));
        }
        let index = self
            .segments
            .partition_point(|segment| segment.display_start().get() <= at);
        let segment = self.segments.get(index.checked_sub(1)?)?;
        match segment.kind {
            SegmentKind::Copy => Some(SourceByte::new(
                segment.source.start().get() + (at - segment.display_start().get()),
            )),
            // 只有尾部零宽段会走到这里；保守取段起点（正常不可达）。
            SegmentKind::Elide => Some(segment.source.start()),
            _ => None,
        }
    }

    /// display 区间的**左端**在 source 上的位置（Before 偏置）。
    fn source_byte_before_display(&self, at: usize) -> Option<SourceByte> {
        let at = at.min(self.display_byte_len);
        if at == 0 {
            return Some(SourceByte::new(0));
        }
        if self.segments.is_empty() {
            return None;
        }
        if at == self.display_byte_len {
            // 末尾端点：取最后一个可见段末尾（不吞掉尾部隐藏标记）。
            return self
                .segments
                .iter()
                .rev()
                .find(|segment| segment.kind == SegmentKind::Copy)
                .map(|segment| segment.source.end())
                .or_else(|| Some(SourceByte::new(0)));
        }
        // 第一个 display_end > at 的段。
        let index = self
            .segments
            .partition_point(|segment| segment.display_end().get() <= at);
        let segment = self.segments.get(index)?;
        match segment.kind {
            SegmentKind::Copy => {
                if at <= segment.display_start().get() {
                    // 接缝：取前一个可见段的末尾。
                    self.segments[..index]
                        .iter()
                        .rev()
                        .find(|previous| previous.kind == SegmentKind::Copy)
                        .map(|previous| previous.source.end())
                        .or_else(|| Some(SourceByte::new(0)))
                } else {
                    Some(SourceByte::new(
                        segment.source.start().get() + (at - segment.display_start().get()),
                    ))
                }
            }
            _ => self.segments[index..]
                .iter()
                .find(|next| next.kind == SegmentKind::Copy)
                .map(|next| next.source.start()),
        }
    }

    /// display 区间 → 最小 source 区间。
    ///
    /// 起点用 After 偏置、终点用 Before 偏置：严格位于区间内部的省略段会被包含，
    /// 两端紧邻的隐藏标记不包含。空区间折叠为 After 偏置的 source 点；端点倒序 /
    /// 越界返回 `None`。
    #[must_use]
    pub fn source_range_of_display_range(&self, range: Range<DisplayByte>) -> Option<SourceRange> {
        let start_at = range.start.get();
        let end_at = range.end.get();
        if start_at > end_at || end_at > self.display_byte_len {
            return None;
        }
        if start_at == end_at {
            let at = if start_at == self.display_byte_len {
                SourceByte::new(self.source_len)
            } else {
                self.source_byte_at_display(DisplayByte::new(start_at))?
            };
            return SourceRange::new(at, at, self.source_len).ok();
        }
        let start = self.source_byte_at_display(DisplayByte::new(start_at))?;
        let end = self.source_byte_before_display(end_at)?;
        if start.get() > end.get() {
            return None;
        }
        SourceRange::new(start, end, self.source_len).ok()
    }
}

/// 投影表构造 / 查询失败。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectionError {
    /// source byte 越界。
    SourceByteOutOfBounds { offset: usize, len: usize },
    /// 逻辑显示 byte 越界。
    DisplayByteOutOfBounds { offset: usize, len: usize },
    /// 该段类型需要尚未实现的语义，拒绝猜测。
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
                write!(f, "{kind:?} 段的映射尚未实现，拒绝猜测")
            }
            Self::InvalidLayout(reason) => write!(f, "投影段表布局非法：{reason}"),
            Self::Overflow => write!(f, "投影偏移相加溢出"),
        }
    }
}

impl Error for ProjectionError {}

#[cfg(test)]
mod bias_tests {
    use super::*;

    /// 造一个 `` `abc` `` 的投影：开头/结尾各一段 Elide，中间 Copy。
    fn inline_code() -> ProjectionMap {
        let source = "`abc`";
        let segments = vec![
            Segment::elide(
                SourceRange::new(SourceByte::new(0), SourceByte::new(1), 5).expect("起点"),
                DisplayByte::new(0),
            )
            .expect("elide"),
            Segment::copy(
                SourceRange::new(SourceByte::new(1), SourceByte::new(4), 5).expect("copy"),
                DisplayByte::new(0),
            )
            .expect("copy"),
            Segment::elide(
                SourceRange::new(SourceByte::new(4), SourceByte::new(5), 5).expect("终点"),
                DisplayByte::new(3),
            )
            .expect("elide"),
        ];
        ProjectionMap::from_segments(source, segments).expect("构造投影失败")
    }

    #[test]
    fn left_bias_stays_inside_the_construct_at_display_end() {
        let map = inline_code();
        // display 末尾 = "abc" 之后；Right 落在闭合反引号之后（构造外），Left 落在之前（构造内）。
        let right = map
            .source_byte_at_display_biased(DisplayByte::new(3), Bias::Right)
            .expect("Right 应有值");
        let left = map
            .source_byte_at_display_biased(DisplayByte::new(3), Bias::Left)
            .expect("Left 应有值");
        assert_eq!(right.get(), 5, "Right = 文档尾（闭合反引号之后）");
        assert_eq!(left.get(), 4, "Left = 闭合反引号之前（构造内）");
    }

    #[test]
    fn left_bias_is_symmetric_at_the_display_start() {
        let map = inline_code();
        let left = map
            .source_byte_at_display_biased(DisplayByte::new(0), Bias::Left)
            .expect("Left 应有值");
        assert_eq!(left.get(), 0, "起点 Left = 0（开头反引号之前）");
        // 起点 Right 仍是原行为（本实现保留 legacy），不强制改。
        let right = map
            .source_byte_at_display_biased(DisplayByte::new(0), Bias::Right)
            .expect("Right 应有值");
        assert!(right.get() <= 1, "起点 Right 不应越界到构造外");
    }

    #[test]
    fn biases_agree_inside_visible_text() {
        let map = inline_code();
        // 只比较可见字符**内部**的边界（1..3）；0 与 3 是隐藏标记所在接缝，
        // 两种偏置本来就应当不同（0：开头反引号前 vs 后；3：闭合反引号前 vs 后）。
        for display in 1..3usize {
            let left = map
                .source_byte_at_display_biased(DisplayByte::new(display), Bias::Left)
                .expect("Left");
            let right = map
                .source_byte_at_display_biased(DisplayByte::new(display), Bias::Right)
                .expect("Right");
            if display < 3 {
                assert_eq!(
                    left, right,
                    "可见字符内部两种偏置必须一致（display={display}）"
                );
            }
        }
    }

    #[test]
    fn biases_agree_without_hidden_markers() {
        let source = "plain text";
        let map = ProjectionMap::from_segments(
            source,
            vec![Segment::copy(
                SourceRange::new(
                    SourceByte::new(0),
                    SourceByte::new(source.len()),
                    source.len(),
                )
                .expect("copy"),
                DisplayByte::new(0),
            )
            .expect("copy")],
        )
        .expect("构造投影失败");
        for display in 0..=source.len() {
            assert_eq!(
                map.source_byte_at_display_biased(DisplayByte::new(display), Bias::Left),
                map.source_byte_at_display_biased(DisplayByte::new(display), Bias::Right),
                "无隐藏标记时两种偏置必须等价（display={display}）"
            );
        }
    }

    #[test]
    fn out_of_bounds_returns_none() {
        let map = inline_code();
        assert!(map
            .source_byte_at_display_biased(DisplayByte::new(4), Bias::Left)
            .is_none());
        assert!(map
            .source_byte_at_display_biased(DisplayByte::new(99), Bias::Right)
            .is_none());
    }
}
