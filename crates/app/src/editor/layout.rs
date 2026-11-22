//! 软折行与视觉行几何（**不碰 GPUI 绘制**）。
//!
//! 切片 5a 只有一层绘制：`EditorController::source_text()` 按软折行铺开。
//! S1 是 identity 投影（源码原样显示，`#` / `**` 都不隐藏），所以这里只做
//! 「源码字节 ↔ 视觉行」的几何，不做任何语法 / 标记处理。
//!
//! ## 折行位置由调用方注入
//!
//! [`wrap_with`] 的 `boundaries` 参数负责回答"这一行要切成哪几段"：
//!
//! - 生产路径用 GPUI `TextSystem` 的 `LineWrapper`（与 `Text` 元素同一套断行规则，
//!   含字体回退、词边界、CJK 规则）；
//! - 单元测试注入假实现，所以在**没有窗口**的情况下也能验证边界处理
//!   （空行、行尾换行、超长不可断词、grapheme 吸附）。
//!
//! 这样拆分的好处：命中 / 光标 / 选区这些容易写错的纯逻辑可以脱离 GPUI 测试，
//! 而断行宽度仍然用真实字形宽度，不需要"字符数 × 平均宽度"估算。

use std::ops::Range;

use menghunote_editor::units;

/// 视觉行最小折行宽度（防止窗口被拖到极窄时出现除零 / 死循环）。
pub const MIN_WRAP_WIDTH: f32 = 64.0;

/// 一个视觉行：源码字节区间 + 它属于哪一条源码行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisualLine {
    /// 源码行序号（按 `\n` 切分，0 起）。
    pub source_line: usize,
    /// 该视觉行是这条源码行的第几段（软折行下标，0 起）。
    pub row_in_line: usize,
    /// 该视觉行显示的源码字节区间（**不含**行尾换行符）。
    pub bytes: Range<usize>,
}

impl VisualLine {
    /// 该视觉行是否为空行（没有可显示字符）。
    #[must_use]
    pub fn is_empty_line(&self) -> bool {
        self.bytes.is_empty()
    }
}

/// 一整篇文档的折行结果（可整帧缓存）。
#[derive(Debug, Clone)]
pub struct LayoutFrame {
    /// 折行依据的 source 快照（与 `EditorController::source_text()` 同源）。
    pub text: String,
    /// 视觉行表（至少一行；空文档也有一个空行，光标才有落点）。
    pub lines: Vec<VisualLine>,
    /// 全文 grapheme 起始字节（升序，含 `0` 与 `text.len()`）。
    pub graphemes: Vec<usize>,
    /// 固定行高（像素）。
    pub line_height: f32,
    /// 折行宽度（像素），用于缓存命中判断。
    pub wrap_width: f32,
}

impl LayoutFrame {
    /// 视觉行数。
    #[must_use]
    pub fn row_count(&self) -> usize {
        self.lines.len()
    }

    /// 文档内容总高度（像素）。
    #[must_use]
    pub fn content_height(&self) -> f32 {
        self.row_count() as f32 * self.line_height
    }

    /// 第 `row` 行的源码字节区间。
    #[must_use]
    pub fn row_bytes(&self, row: usize) -> Range<usize> {
        self.lines[row].bytes.clone()
    }

    /// 文档纵坐标 `y`（相对内容顶部）落在哪一视觉行。
    #[must_use]
    pub fn row_at_y(&self, y: f32) -> usize {
        let last = self.row_count().saturating_sub(1);
        if !y.is_finite() || y <= 0.0 {
            return 0;
        }
        let row = (y / self.line_height.max(1.0)).floor();
        // `as usize` 对 NaN / 负数 / 超大值都是饱和的，这里只需再夹上界。
        (row as usize).min(last)
    }

    /// 源码字节 `byte` 落在哪一视觉行。
    ///
    /// 软折行折断处（`byte` 同时是上一行末尾与下一行开头）**归下一行**，
    /// 与主流编辑器一致：光标显示在换行后的行首。
    #[must_use]
    pub fn row_of_byte(&self, byte: usize) -> usize {
        let byte = byte.min(self.text.len());
        let index = self
            .lines
            .partition_point(|line| line.bytes.start <= byte)
            .saturating_sub(1);
        let Some(next) = self.lines.get(index + 1) else {
            return index;
        };
        let current = &self.lines[index];
        if next.bytes.start == byte
            && current.bytes.end == byte
            && next.source_line == current.source_line
        {
            index + 1
        } else {
            index
        }
    }

    /// 把字节位置吸附到**前一个 grapheme 边界**。
    ///
    /// 点击命中给出的是字形位置，可能落在 grapheme 中间（例如 `e` 与 U+0301
    /// 之间）。光标不允许停在 grapheme 中间，因此统一向前吸附。
    #[must_use]
    pub fn snap_byte_to_grapheme(&self, byte: usize) -> usize {
        let byte = byte.min(self.text.len());
        let index = self.graphemes.partition_point(|&boundary| boundary <= byte);
        if index == 0 {
            0
        } else {
            self.graphemes[index - 1]
        }
    }
}

/// 按注入的断行函数折行。
///
/// `boundaries(line, wrap_width)` 返回**该源码行内部**每段视觉行的结束字节
/// （相对 `line` 起点、升序、可少于 `line.len()`——剩余部分自然成为最后一段）。
/// 传入空向量表示"整行不折"。
#[must_use]
pub fn wrap_with(
    text: &str,
    wrap_width: f32,
    line_height: f32,
    boundaries: &mut dyn FnMut(&str, f32) -> Vec<usize>,
) -> LayoutFrame {
    let wrap_width = if wrap_width.is_finite() {
        wrap_width.max(MIN_WRAP_WIDTH)
    } else {
        MIN_WRAP_WIDTH
    };
    let line_height = if line_height.is_finite() {
        line_height.max(1.0)
    } else {
        1.0
    };
    let graphemes = units::grapheme_boundaries(text);
    let mut lines = Vec::new();
    let mut source_line = 0usize;
    let mut offset = 0usize;

    for raw in text.split_inclusive('\n') {
        let content_end = if raw.ends_with('\n') {
            offset + raw.len() - 1
        } else {
            offset + raw.len()
        };
        let content = &text[offset..content_end];
        let mut start = offset;
        let mut row_in_line = 0usize;
        for end in boundaries(content, wrap_width) {
            let end = offset + end.min(content.len());
            // 断行函数给出退化边界（<= 当前起点）时跳过，避免产生空视觉行或死循环。
            if end <= start {
                continue;
            }
            lines.push(VisualLine {
                source_line,
                row_in_line,
                bytes: start..end,
            });
            start = end;
            row_in_line += 1;
        }
        // 断行函数可能把整行吃干净（最后一个边界正好等于行尾），此时不再补空行；
        // 空源码行（`row_in_line == 0`）必须补，否则空行没有行高、光标无处可落。
        if start < content_end || row_in_line == 0 {
            lines.push(VisualLine {
                source_line,
                row_in_line,
                bytes: start..content_end,
            });
        }
        offset += raw.len();
        source_line += 1;
    }

    // 空文档 / 以换行结尾时补最后一行，光标才有一个可落的空行
    // （与编辑器惯例一致：`"a\n"` 有 2 个视觉行）。
    if text.is_empty() || text.ends_with('\n') {
        lines.push(VisualLine {
            source_line,
            row_in_line: 0,
            bytes: text.len()..text.len(),
        });
    }

    LayoutFrame {
        text: text.to_owned(),
        lines,
        graphemes,
        line_height,
        wrap_width,
    }
}

/// 滚动量夹取：`scroll` 想滚到哪，就夹到 `[0, 内容高度 - 视口高度]`。
///
/// 内容比视口矮时上界为 0（不能滚）；`NaN` / 无穷按 0 处理（滚动量只来自
/// 滚轮与键盘，出现非有限值说明上游算错了，宁可回到顶部也不要把画面甩飞）。
#[must_use]
pub fn clamp_scroll(scroll: f32, content_height: f32, viewport_height: f32) -> f32 {
    if !scroll.is_finite() || !content_height.is_finite() || !viewport_height.is_finite() {
        return 0.0;
    }
    let max = (content_height - viewport_height).max(0.0);
    scroll.clamp(0.0, max)
}

/// 让第 `row` 行完整落进视口所需的滚动量（只做**最小**滚动）。
///
/// 已经在视口内就不动；在上方就顶到视口顶部；在下方就刚好贴住视口底部。
/// 上下方向键 / 翻页之后用它把光标带回来，因此不能"居中式"滚动。
#[must_use]
pub fn scroll_to_reveal(scroll: f32, row: usize, line_height: f32, viewport_height: f32) -> f32 {
    if !line_height.is_finite() || line_height <= 0.0 || !viewport_height.is_finite() {
        return scroll.max(0.0);
    }
    let current = if scroll.is_finite() {
        scroll.max(0.0)
    } else {
        0.0
    };
    let top = row as f32 * line_height;
    let bottom = top + line_height;
    if top < current {
        top
    } else if bottom > current + viewport_height {
        bottom - viewport_height
    } else {
        current
    }
}

/// 上下移动 / 翻页的目标视觉行（夹在 `[0, last_row]`，不会越界也不会回绕）。
#[must_use]
pub fn vertical_target(row: usize, delta: i64, last_row: usize) -> usize {
    let last = i64::try_from(last_row).unwrap_or(i64::MAX);
    let row = i64::try_from(row).unwrap_or(0);
    let target = row.saturating_add(delta).clamp(0, last);
    usize::try_from(target).unwrap_or(0)
}

/// 一页包含多少个视觉行（至少 1，视口不足一行时也不会退化成 0）。
#[must_use]
pub fn rows_per_page(viewport_height: f32, line_height: f32) -> i64 {
    if !viewport_height.is_finite() || !line_height.is_finite() || line_height <= 0.0 {
        return 1;
    }
    let rows = (viewport_height / line_height).floor();
    if rows < 1.0 {
        1
    } else {
        rows as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 假断行：每 `step` 个字节切一段（只在 char 边界切，避免造出非法 UTF-8 区间）。
    fn fake_boundaries(step: usize) -> impl FnMut(&str, f32) -> Vec<usize> {
        move |line: &str, _width: f32| {
            let mut ends = Vec::new();
            let mut cursor = 0usize;
            for (index, ch) in line.char_indices() {
                if index > 0 && index - cursor >= step {
                    ends.push(index);
                    cursor = index;
                }
                let _ = ch;
            }
            ends
        }
    }

    fn wrap(text: &str, step: usize) -> LayoutFrame {
        let mut boundaries = fake_boundaries(step);
        wrap_with(text, 200.0, 20.0, &mut boundaries)
    }

    #[test]
    fn empty_document_has_one_row() {
        let frame = wrap("", 8);
        assert_eq!(frame.row_count(), 1);
        assert!(frame.lines[0].is_empty_line());
        assert_eq!(frame.row_of_byte(0), 0);
        assert_eq!(frame.snap_byte_to_grapheme(0), 0);
    }

    #[test]
    fn hard_line_breaks_produce_one_row_each() {
        let frame = wrap("ab\ncd", 8);
        assert_eq!(frame.row_count(), 2);
        assert_eq!(frame.lines[0].bytes, 0..2);
        assert_eq!(frame.lines[1].bytes, 3..5);
        assert_eq!(frame.row_at_y(0.0), 0);
        assert_eq!(frame.row_at_y(19.9), 0);
        assert_eq!(frame.row_at_y(20.0), 1);
        assert_eq!(frame.row_at_y(10_000.0), 1);
        assert_eq!(frame.row_at_y(f32::NAN), 0);
    }

    #[test]
    fn trailing_newline_adds_an_empty_row_for_the_caret() {
        let frame = wrap("a\n", 8);
        assert_eq!(frame.row_count(), 2);
        assert_eq!(frame.lines[1].bytes, 2..2);
        // 光标在最后一个字节之后 → 落在空行
        assert_eq!(frame.row_of_byte(2), 1);
    }

    #[test]
    fn blank_line_between_paragraphs_is_its_own_row() {
        let frame = wrap("a\n\nb", 8);
        assert_eq!(frame.row_count(), 3);
        assert!(frame.lines[1].is_empty_line());
        assert_eq!(frame.lines[1].bytes, 2..2);
    }

    #[test]
    fn long_line_is_split_into_visual_rows_with_contiguous_bytes() {
        let text = "abcdefghijklmnopqrstuvwxyz";
        let frame = wrap(text, 8);
        assert!(frame.row_count() >= 4, "26 字节 / 8 → 至少 4 段");
        // 视觉行必须首尾相接、不重叠、覆盖整行
        let mut expected = 0usize;
        for line in &frame.lines {
            assert_eq!(line.bytes.start, expected);
            assert_eq!(line.source_line, 0);
            expected = line.bytes.end;
        }
        assert_eq!(expected, text.len());
        // 软折行折断处的字节归下一行
        let first_end = frame.lines[0].bytes.end;
        assert_eq!(frame.row_of_byte(first_end), 1);
        assert_eq!(frame.row_of_byte(first_end - 1), 0);
    }

    #[test]
    fn click_byte_inside_a_grapheme_snaps_back_to_its_start() {
        // `é` 是分解形式：e(1 byte) + U+0301(2 bytes) = 3 bytes / 1 grapheme / 2 scalar。
        // 因此文本布局是：[0]='a'、[1..4]="e"+U+0301、[4]='b'，总长 5 字节。
        let frame = wrap("ae\u{301}b", 64);
        assert_eq!(
            frame.graphemes,
            vec![0, 1, 4, 5],
            "grapheme 边界必须来自 text"
        );
        // 落在 U+0301 内部（byte 2）→ 吸附到 e（byte 1）
        assert_eq!(frame.snap_byte_to_grapheme(2), 1);
        // 落在 b（byte 4）→ 原样
        assert_eq!(frame.snap_byte_to_grapheme(4), 4);
        assert_eq!(frame.snap_byte_to_grapheme(0), 0);
        // 越界夹到文本末尾（合法尾边界）
        assert_eq!(frame.snap_byte_to_grapheme(usize::MAX), 5);
    }

    #[test]
    fn degenerate_boundaries_are_ignored() {
        // 断行函数返回 0 与越界值：不能产生空视觉行或越界切片
        let mut boundaries = |_line: &str, _width: f32| vec![0usize, 2, usize::MAX];
        let frame = wrap_with("abcdef", 200.0, 20.0, &mut boundaries);
        assert_eq!(frame.row_count(), 2);
        assert_eq!(frame.lines[0].bytes, 0..2);
        assert_eq!(frame.lines[1].bytes, 2..6);
    }

    #[test]
    fn scroll_is_clamped_to_the_content_extent() {
        // 内容 500、视口 200 → 只能在 [0, 300] 里滚
        assert_eq!(clamp_scroll(0.0, 500.0, 200.0), 0.0);
        assert_eq!(clamp_scroll(123.0, 500.0, 200.0), 123.0);
        assert_eq!(clamp_scroll(9999.0, 500.0, 200.0), 300.0);
        assert_eq!(clamp_scroll(-50.0, 500.0, 200.0), 0.0);
        // 内容比视口矮：上界为 0
        assert_eq!(clamp_scroll(10.0, 100.0, 200.0), 0.0);
        // 非有限值：回顶部
        assert_eq!(clamp_scroll(f32::NAN, 500.0, 200.0), 0.0);
        assert_eq!(clamp_scroll(f32::INFINITY, 500.0, 200.0), 0.0);
        assert_eq!(clamp_scroll(10.0, f32::NAN, 200.0), 0.0);
    }

    #[test]
    fn reveal_keeps_visible_rows_still_and_minimally_scrolls_otherwise() {
        let line_height = 20.0;
        let viewport = 200.0;
        // 第 5 行（100..120）已经在 [0, 200) 里 → 不动
        assert_eq!(
            scroll_to_reveal(0.0, 5, line_height, viewport),
            0.0,
            "可见就不该滚动"
        );
        // 第 1 行在滚动位置 100 之上 → 顶到视口顶部
        assert_eq!(scroll_to_reveal(100.0, 1, line_height, viewport), 20.0);
        // 第 20 行（400..420）在下方 → 刚好贴住视口底部：420 - 200 = 220
        assert_eq!(scroll_to_reveal(100.0, 20, line_height, viewport), 220.0);
        // 非法行高：保持原样（只做非负夹取）
        assert_eq!(scroll_to_reveal(50.0, 3, 0.0, viewport), 50.0);
        assert_eq!(scroll_to_reveal(-5.0, 3, 0.0, viewport), 0.0);
    }

    #[test]
    fn vertical_target_clamps_without_wrapping() {
        assert_eq!(vertical_target(5, 1, 10), 6);
        assert_eq!(vertical_target(5, -1, 10), 4);
        assert_eq!(vertical_target(0, -1, 10), 0, "顶部不能回绕到末尾");
        assert_eq!(vertical_target(10, 1, 10), 10, "底部不能回绕到开头");
        // 翻一页（典型 34 行）越过底部时夹到最后一行
        assert_eq!(vertical_target(30, 34, 33), 33);
        assert_eq!(vertical_target(2, -34, 33), 0);
        // 饱和：超大 delta 不会溢出
        assert_eq!(vertical_target(3, i64::MAX, 33), 33);
        assert_eq!(vertical_target(3, i64::MIN, 33), 0);
    }

    #[test]
    fn rows_per_page_never_returns_zero() {
        assert_eq!(rows_per_page(220.0, 20.0), 11);
        assert_eq!(rows_per_page(5.0, 20.0), 1, "视口不足一行也要能翻页");
        assert_eq!(rows_per_page(220.0, 0.0), 1);
        assert_eq!(rows_per_page(f32::NAN, 20.0), 1);
    }

    #[test]
    fn scroll_and_reveal_compose_to_a_stable_viewport() {
        // 内容 66 行 × 20px，视口 200px：从顶部往下翻两页，滚动量始终在合法区间，
        // 且光标行始终可见（这正是一次 PageDown 的完整几何行为）。
        let line_height = 20.0;
        let viewport = 200.0;
        let content = 66.0 * line_height;
        let page = rows_per_page(viewport, line_height);
        let mut scroll = 0.0_f32;
        let mut row = 0usize;
        for _ in 0..3 {
            row = vertical_target(row, page, 65);
            scroll = clamp_scroll(
                scroll_to_reveal(scroll, row, line_height, viewport),
                content,
                viewport,
            );
            let top = row as f32 * line_height;
            assert!(
                top >= scroll - 0.01 && top + line_height <= scroll + viewport + 0.01,
                "第 {row} 行必须可见（scroll = {scroll}）"
            );
            assert!((0.0..=content - viewport).contains(&scroll));
        }
    }

    #[test]
    fn wrap_width_is_floored_to_keep_geometry_finite() {
        let mut boundaries = |_line: &str, width: f32| {
            assert!(width >= MIN_WRAP_WIDTH);
            Vec::new()
        };
        let frame = wrap_with("abc", 1.0, 20.0, &mut boundaries);
        assert_eq!(frame.wrap_width, MIN_WRAP_WIDTH);
        assert_eq!(frame.row_count(), 1);
    }
}
