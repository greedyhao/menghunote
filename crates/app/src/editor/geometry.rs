//! 已绘制的一帧几何：源码行 → 视觉行 → 字形 x 偏移。
//!
//! 命中测试**只**读这里的数据，不做任何"字符数 × 平均宽度"的估算：
//!
//! - 纵向：视觉行高度固定，`y / 行高` 直接给出视觉行下标；
//! - 横向：用该行真实的 [`ShapedLine`]（`LineLayout::closest_index_for_x` /
//!   `x_for_index`）在像素与**字节**之间换算，再交给 `menghunote_text` 换成
//!   `UnicodeScalar`。
//!
//! 这里只覆盖"当前可见（含一行 overscan）"的视觉行：屏外的行不 shaping，
//! 因此 [`EditorGeometry::byte_at_row_x`] 对屏外行返回 `None`，调用方要么放弃
//! （滚轮），要么按需 shaping（键盘上下移动，见 `glyph::shape_row`）。

use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{px, Bounds, Pixels, Point, RenderImage, ShapedLine};
use menghunote_editor::{Bias, StyleKind};

use super::glyph;
use super::layout::LayoutFrame;
use super::theme;

/// 一帧的绘制几何（**只覆盖可见范围**）。
pub struct EditorGeometry {
    /// 本帧折行结果（与 `text` 同源）。
    pub frame: Rc<LayoutFrame>,
    /// 与 [`Self::visible_rows`] 一一对应的 shaping 结果。
    pub shaped: Vec<ShapedLine>,
    /// `shaped[0]` 对应的全局视觉行下标。
    pub visible_start: usize,
    /// 本帧实际使用的垂直滚动量（像素，正 = 内容向上滚过）。
    pub scroll_y: f32,
    /// 绘制区域原点（窗口坐标）。
    pub origin: Point<Pixels>,
    /// **默认**行高（正文）；逐行行高请用 [`Self::row_height`]。
    pub line_height: Pixels,
    /// 绘制区域高度（像素）。
    pub viewport_height: f32,
    /// 本帧的语义样式段（**display** 字节区间，完整覆盖全文）。
    ///
    /// 绘制与命中都从这里取"这一行怎么分段"，保证两边 shaping 完全一致
    /// （粗体会让字变宽，命中若用无样式 shaping 会偏）。
    pub styles: Rc<Vec<(Range<usize>, StyleKind)>>,
    /// 本帧可见的图片对象（按视觉行顺序；只覆盖可见范围）。
    pub objects: Vec<ObjectPaint>,
    /// 本帧可见的任务复选框（ADR 0019 S3；按视觉行顺序）。
    pub task_boxes: Vec<TaskBoxPaint>,
}

/// 图片对象的绘制状态（占位与图片共用同一块矩形，几何完全一致）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectPaintState {
    /// 正在后台解码。
    Loading,
    /// 资产不存在 / 只有元数据（待同步）。
    Missing,
    /// 解码失败（损坏 / 超预算 / 不支持的格式）。
    Failed,
    /// 已解码，可直接绘制。
    Ready,
}

/// 一个**可见**的图片 Object 行（ADR 0019 S2a）：绘制、命中、工具条共用它。
///
/// `rect` 是**窗口坐标**的图片/占位框矩形；行高在布局度量里已经等于
/// `rect 高度 + 上下内边距`，所以滚动/命中与 `LayoutFrame` 几何天然一致。
#[derive(Debug, Clone)]
pub struct ObjectPaint {
    /// 该图片在 `MarkdownDocument::images()` 里的下标（整个文档顺序）。
    pub index: usize,
    /// 源码行号（0 起）。
    pub line: usize,
    /// 视觉行号（0 起）。
    pub row: usize,
    /// 图片/占位框矩形（窗口坐标）。
    pub rect: Bounds<Pixels>,
    /// 已解码图片（`None` = 占位）。
    pub image: Option<Arc<RenderImage>>,
    /// 占位/alt 文本（已 shaping，单行；`None` = 图片已就绪或没有文案）。
    pub label: Option<ShapedLine>,
    /// 绘制状态。
    pub state: ObjectPaintState,
    /// 是否处于选中态（单击选中 / 文本选区相交）——画描边。
    pub selected: bool,
}

impl ObjectPaint {
    /// 是否已解码（可画图片）。
    #[must_use]
    pub const fn is_ready(&self) -> bool {
        matches!(self.state, ObjectPaintState::Ready) && self.image.is_some()
    }
}

/// 一个**可见**的任务复选框（ADR 0019 S3）：绘制、命中、headless 断言共用。
///
/// `rect` 是窗口坐标的**占位矩形**（display 上两连 NBSP 的真实 shaping 区间 ×
/// 该视觉行行高）：点击命中用整个矩形（比 13px 方框更好点），绘制在矩形内居中画方框。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TaskBoxPaint {
    /// 列表项下标（`ParsedMarkdown::items`；点击后按它切换源码）。
    pub item: usize,
    /// 任务框源码区间（点击时与当前解析结果比对，防止陈旧帧点到别的 item）。
    pub source: (usize, usize),
    /// 是否已勾选（来自解析器状态；source 是唯一真源）。
    pub checked: bool,
    /// 占位矩形（窗口坐标）。
    pub rect: Bounds<Pixels>,
}

impl EditorGeometry {
    /// 可见视觉行下标区间（半开）。
    #[must_use]
    pub fn visible_rows(&self) -> Range<usize> {
        self.visible_start..self.visible_start + self.shaped.len()
    }

    /// 文字区**基础**左边界（窗口坐标；不含块级缩进）。
    #[must_use]
    pub fn text_left(&self) -> Pixels {
        self.origin.x + px(theme::PAD_X)
    }

    /// 第 `row` 行的文字左边界 = 基础左边界 + 该行缩进（引用块）。
    ///
    /// **几何与绘制都用这一个函数**，避免 `PAD_X`/缩进被加两次
    /// （历史 bug：两处各加一次 → 选区整体右移 14px）。
    #[must_use]
    pub fn text_left_for_row(&self, row: usize) -> Pixels {
        self.origin.x + px(theme::PAD_X) + px(self.frame.row_indent(row))
    }

    /// 第 `row` 行的引用左缩进（像素）。
    #[must_use]
    pub fn row_indent(&self, row: usize) -> Pixels {
        px(self.frame.row_indent(row))
    }

    /// 第 `row` 行的行高（逐行可变：标题行更高）。
    #[must_use]
    pub fn row_height(&self, row: usize) -> Pixels {
        px(self.frame.row_height(row))
    }

    /// 第 `row` 行的字号（逐行可变）。
    #[must_use]
    pub fn row_font_size(&self, row: usize) -> Pixels {
        px(self.frame.row_font_size(row))
    }

    /// 第 `row` 行在窗口坐标里的上边（按累计行顶，**不是** `行号 × 行高`）。
    #[must_use]
    pub fn row_top(&self, row: usize) -> Pixels {
        self.origin.y + px(theme::PAD_Y) - px(self.scroll_y) + px(self.frame.row_top_offset(row))
    }

    /// 滚动后可见的视觉行区间（半开）：从 `scroll_y` 顶端开始，直到填满视口。
    ///
    /// 行高逐行不同，所以不能用 `视口高 / 行高` 估；这里按累计行顶走一遍。
    #[must_use]
    pub fn rows_in_view(&self, scroll_y: f32, viewport_height: f32) -> Range<usize> {
        let top = scroll_y.max(0.0);
        let bottom = top + viewport_height.max(0.0);
        let first = self.frame.row_at_y(top);
        let mut last = first;
        while last < self.frame.row_count() && self.frame.row_top_offset(last) < bottom {
            last += 1;
        }
        first..last.max(first + 1).min(self.frame.row_count())
    }

    /// 窗口坐标 `y` 落在哪一视觉行，夹到可见范围内。
    #[must_use]
    pub fn row_at_y(&self, y: Pixels) -> usize {
        let visible = self.visible_rows();
        if visible.is_empty() {
            return self.visible_start;
        }
        let last_visible = visible.end - 1;
        let top = self.origin.y + px(theme::PAD_Y) - px(self.scroll_y);
        let local_y = (y - top).as_f32();
        self.frame
            .row_at_y(local_y)
            .clamp(visible.start, last_visible)
    }

    /// 窗口坐标 `position` 命中的源码字节（已夹到该视觉行的字节区间内）。
    #[must_use]
    pub fn byte_at(&self, position: Point<Pixels>) -> usize {
        let row = self.row_at_y(position.y);
        let local_x = (position.x - self.text_left_for_row(row)).max(px(0.0));
        self.byte_at_row_x(row, local_x)
            .unwrap_or_else(|| self.frame.row_bytes(row).start)
    }

    /// 在**可见**的行内把像素 x（相对行首）换成源码字节；屏外行返回 `None`。
    #[must_use]
    /// 「点击在字形左半/右半」→ 投影偏置（**纯函数**，与 UI 解耦，便于单测）。
    ///
    /// **右半 → Left、左半 → Right**：隐藏标记宽度为 0，同一个 display 位置对应两个
    /// source 位置。点字符右半时人期望"跨过这个字符"，于是取左侧偏置（停在隐藏标记
    /// 之前，即构造内）；点左半时停在字符之前，取右侧偏置。
    pub fn bias_for_half(right_half: bool) -> Bias {
        if right_half {
            Bias::Left
        } else {
            Bias::Right
        }
    }

    /// 命中：返回 `(display 字节, 偏置)`。
    ///
    /// 偏置由"点是否落在该边界的右侧字形内"决定：`closest_index_for_x` 取最近边界，
    /// 若点在该边界**左侧**（即它落在前一个字符的右半），说明点在字形右半 → `Left`。
    pub fn byte_at_with_bias(&self, position: Point<Pixels>) -> Option<(usize, Bias)> {
        let row = self.row_at_y(position.y);
        let bytes = self.frame.row_bytes(row);
        let shaped = self.shaped.get(row.checked_sub(self.visible_start)?)?;
        // 对象是原子的：拖选/命中落到对象上时一律给“占位符之后”（选区因此包含对象）。
        if self.object_row(row).is_some() {
            return Some((bytes.end, Bias::Right));
        }
        if bytes.is_empty() {
            return Some((bytes.start, Bias::Right));
        }
        let local_x = (position.x - self.text_left_for_row(row)).max(px(0.0));
        let index = shaped.closest_index_for_x(local_x);
        let index = index.min(bytes.len());
        let boundary_x = shaped.x_for_index(index);
        // 点在该边界左侧 → 处在前面字符的右半。
        let right_half = local_x < boundary_x;
        Some((bytes.start + index, Self::bias_for_half(right_half)))
    }

    pub fn byte_at_row_x(&self, row: usize, x: Pixels) -> Option<usize> {
        let bytes = self.frame.row_bytes(row);
        let shaped = self.shaped.get(row.checked_sub(self.visible_start)?)?;
        // 对象行整行是一个原子块：横向位置不影响落点（统一取占位符末尾）。
        if self.object_row(row).is_some() {
            return Some(bytes.end);
        }
        if bytes.is_empty() {
            return Some(bytes.start);
        }
        let index = shaped.closest_index_for_x(x);
        Some(bytes.start + index.min(bytes.len()))
    }

    /// 本帧可见的图片对象。
    #[must_use]
    pub fn objects(&self) -> &[ObjectPaint] {
        &self.objects
    }

    /// 第 `row` 行是否是图片对象行。
    #[must_use]
    pub fn object_row(&self, row: usize) -> Option<&ObjectPaint> {
        self.objects.iter().find(|object| object.row == row)
    }

    /// 窗口坐标命中的图片对象（整行都是对象块的可点击区域）。
    #[must_use]
    pub fn object_at(&self, position: Point<Pixels>) -> Option<&ObjectPaint> {
        let row = self.row_at_y(position.y);
        self.object_row(row)
    }

    /// 窗口坐标命中的图片对象**源码行**（工具条/点选用的轻量查询）。
    #[must_use]
    pub fn object_line_at(&self, position: Point<Pixels>) -> Option<usize> {
        self.object_at(position).map(|object| object.line)
    }

    /// 本帧可见的任务复选框（按视觉行顺序）。
    #[must_use]
    pub fn task_boxes(&self) -> &[TaskBoxPaint] {
        &self.task_boxes
    }

    /// 窗口坐标命中的任务复选框（矩形是占位区，不是方框本身）。
    #[must_use]
    pub fn task_box_at(&self, position: Point<Pixels>) -> Option<&TaskBoxPaint> {
        self.task_boxes
            .iter()
            .find(|task| task.rect.contains(&position))
    }

    /// 第 `row` 行的样式分段 `(字节数, 种类)`（总长等于该行字节数）。
    #[must_use]
    pub fn spans_for_row(&self, row: usize) -> Vec<(usize, StyleKind)> {
        glyph::line_spans(&self.styles, self.frame.row_bytes(row))
    }

    /// 光标（源码字节 `byte`）所在行的窗口坐标 `(row, x)`；不在可见范围时返回 `None`。
    #[must_use]
    pub fn caret_row_x(&self, byte: usize) -> Option<(usize, Pixels)> {
        let row = self.frame.row_of_byte(byte);
        let bytes = self.frame.row_bytes(row);
        let shaped = self.shaped.get(row.checked_sub(self.visible_start)?)?;
        // 对象行没有字形：caret 只能贴在图片/占位框左右两侧。
        if let Some(object) = self.object_row(row) {
            let x = if byte <= bytes.start {
                object.rect.origin.x
            } else {
                object.rect.origin.x + object.rect.size.width
            };
            return Some((row, x));
        }
        if bytes.is_empty() {
            return Some((row, self.text_left_for_row(row)));
        }
        let offset = byte.saturating_sub(bytes.start).min(bytes.len());
        Some((
            row,
            self.text_left_for_row(row) + shaped.x_for_index(offset),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::point;

    fn frame(rows: usize) -> Rc<LayoutFrame> {
        let text = (0..rows)
            .map(|index| format!("{index}\n"))
            .collect::<String>();
        let mut boundaries = |_line: &str, _width: f32| Vec::new();
        Rc::new(super::super::layout::wrap_with(
            &text,
            200.0,
            super::super::layout::LineMetric::body(),
            &[],
            &mut boundaries,
        ))
    }

    /// 只验证不涉及 shaping 的几何：行定位必须夹在可见范围内。
    #[test]
    fn row_lookup_is_clamped_to_visible_rows() {
        let geometry = EditorGeometry {
            frame: frame(50),
            shaped: Vec::new(),
            visible_start: 10,
            scroll_y: 200.0,
            origin: point(px(0.0), px(0.0)),
            line_height: px(20.0),
            viewport_height: 220.0,
            styles: Rc::new(Vec::new()),
            objects: Vec::new(),
            task_boxes: Vec::new(),
        };
        assert!(geometry.visible_rows().is_empty());
        assert_eq!(geometry.row_at_y(px(0.0)), 10);
        assert_eq!(geometry.row_at_y(px(10_000.0)), 10);
        // 屏外行没有 shaping 结果，不能给出字节
        assert_eq!(geometry.byte_at_row_x(20, px(1.0)), None);
        assert_eq!(geometry.caret_row_x(0), None);
        // 第 11 行（下标 11）的上边 = 顶部内边距 - 滚动 + **累计行顶**（行高逐行可变）
        assert_eq!(
            geometry.row_top(11),
            px(0.0) + px(theme::PAD_Y) - px(200.0) + px(geometry.frame.row_top_offset(11))
        );
    }
    #[test]
    fn right_half_maps_to_left_bias() {
        use super::EditorGeometry;
        // 右半 → Left（跨过字形，停在隐藏标记之前）；左半 → Right。
        assert_eq!(
            EditorGeometry::bias_for_half(true),
            menghunote_editor::Bias::Left
        );
        assert_eq!(
            EditorGeometry::bias_for_half(false),
            menghunote_editor::Bias::Right
        );
    }
}
