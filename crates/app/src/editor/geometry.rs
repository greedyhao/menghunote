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

use gpui::{px, Pixels, Point, ShapedLine};

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
    /// 行高（等于 `frame.line_height`，存成 `Pixels` 少一次转换）。
    pub line_height: Pixels,
    /// 绘制区域高度（像素）。
    pub viewport_height: f32,
}

impl EditorGeometry {
    /// 可见视觉行下标区间（半开）。
    #[must_use]
    pub fn visible_rows(&self) -> Range<usize> {
        self.visible_start..self.visible_start + self.shaped.len()
    }

    /// 文字区左边界（窗口坐标）。
    #[must_use]
    pub fn text_left(&self) -> Pixels {
        self.origin.x + px(theme::PAD_X)
    }

    /// 第 `row` 行在窗口坐标里的上边。
    #[must_use]
    pub fn row_top(&self, row: usize) -> Pixels {
        self.origin.y + px(theme::PAD_Y) - px(self.scroll_y) + self.line_height * row as f32
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
        let local_x = (position.x - self.text_left()).max(px(0.0));
        self.byte_at_row_x(row, local_x)
            .unwrap_or_else(|| self.frame.row_bytes(row).start)
    }

    /// 在**可见**的行内把像素 x（相对行首）换成源码字节；屏外行返回 `None`。
    #[must_use]
    pub fn byte_at_row_x(&self, row: usize, x: Pixels) -> Option<usize> {
        let bytes = self.frame.row_bytes(row);
        let shaped = self.shaped.get(row.checked_sub(self.visible_start)?)?;
        if bytes.is_empty() {
            return Some(bytes.start);
        }
        let index = shaped.closest_index_for_x(x);
        Some(bytes.start + index.min(bytes.len()))
    }

    /// 光标（源码字节 `byte`）所在行的窗口坐标 `(row, x)`；不在可见范围时返回 `None`。
    #[must_use]
    pub fn caret_row_x(&self, byte: usize) -> Option<(usize, Pixels)> {
        let row = self.frame.row_of_byte(byte);
        let bytes = self.frame.row_bytes(row);
        let shaped = self.shaped.get(row.checked_sub(self.visible_start)?)?;
        if bytes.is_empty() {
            return Some((row, self.text_left()));
        }
        let offset = byte.saturating_sub(bytes.start).min(bytes.len());
        Some((row, self.text_left() + shaped.x_for_index(offset)))
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
            20.0,
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
        };
        assert!(geometry.visible_rows().is_empty());
        assert_eq!(geometry.row_at_y(px(0.0)), 10);
        assert_eq!(geometry.row_at_y(px(10_000.0)), 10);
        // 屏外行没有 shaping 结果，不能给出字节
        assert_eq!(geometry.byte_at_row_x(20, px(1.0)), None);
        assert_eq!(geometry.caret_row_x(0), None);
        // 第 11 行（下标 11）的上边 = 顶部内边距 - 滚动 + 11 × 行高
        assert_eq!(
            geometry.row_top(11),
            px(0.0) + px(theme::PAD_Y) - px(200.0) + px(220.0)
        );
    }
}
