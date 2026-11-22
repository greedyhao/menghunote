//! 隐藏标记下的编辑坐标（S2-5）。
//!
//! ## 为什么要单独一层
//!
//! 投影把 `**`、`` ` ``、`# ` 这类标记从 **display** 里隐藏了，但编辑请求最终要用
//! **source** 坐标。如果直接拿 source 光标算"前/后一个 grapheme"，在构造边界上就会
//! 啃到隐藏标记：caret 停在「粗体」前按 `Delete`，source 上的下一个字符正是隐藏的
//! `*` —— 结果标记被删、文字没动。
//!
//! 这里的做法是：**先在 display 上确定用户想删的那一段**（相邻可见 grapheme），
//! 再用 [`menghunote_editor::ProjectionMap::source_range_of_display_range`] 映射成
//! **最小** source 区间（起点 After 偏置、终点 Before 偏置，因此两端紧邻的隐藏标记
//! 不会被包含）。真正删除由调用方完成（把区间设成选区后走控制器的一条删除路径）。
//!
//! ## 允许的例外
//!
//! 用户显式选中了会展开的构造内部（标记已显现、已是普通文本）时，选中的就是真实
//! 文本，照常删——本模块不特殊处理，`source_range_of_display_range` 自然覆盖。

use std::ops::Range;

use menghunote_editor::units;

/// display 文本里 `caret` 之前那个**可见** grapheme 的字节区间。
///
/// `caret` 落在 grapheme 内部时先吸附到左边界；已在开头返回 `None`。
#[must_use]
pub fn grapheme_before(display: &str, caret: usize) -> Option<Range<usize>> {
    let caret = caret.min(display.len());
    let boundaries = units::grapheme_boundaries(display);
    let end = boundaries
        .iter()
        .copied()
        .take_while(|&byte| byte <= caret)
        .last()?;
    if end == 0 {
        return None;
    }
    let start = boundaries
        .iter()
        .copied()
        .take_while(|&byte| byte < end)
        .last()?;
    Some(start..end)
}

/// display 文本里 `caret` 之后那个**可见** grapheme 的字节区间。
///
/// 已在末尾返回 `None`。
#[must_use]
pub fn grapheme_after(display: &str, caret: usize) -> Option<Range<usize>> {
    let caret = caret.min(display.len());
    let boundaries = units::grapheme_boundaries(display);
    // caret 落在 grapheme 内部时，容错到它所在那个 grapheme 的起点（整块给出）。
    let start = boundaries
        .iter()
        .copied()
        .take_while(|&byte| byte <= caret)
        .last()?;
    let end = boundaries.iter().copied().find(|&byte| byte > start)?;
    Some(start..end)
}

/// 把 display 区间折叠成"要么为空、要么非空"的规范形式（空区间返回 `None`）。
///
/// 映射到 source 之后若区间为空（例如刚好落在两段隐藏标记之间），调用方必须
/// **什么都不删**，而不是退化成"删一个 source grapheme"——那会啃标记。
#[must_use]
pub fn non_empty(range: Range<usize>) -> Option<Range<usize>> {
    (range.start < range.end).then_some(range)
}

/// 光标在 display 上按方向移动一步后的目标 **display 字节**。
///
/// 返回的是"可见文本边界"：隐藏标记不在 display 里，因此**不会**成为落点。
/// 已在两端返回 `None`（调用方应保持原 caret，不要跳到 0）。
#[must_use]
pub fn step_target(
    display: &str,
    caret: usize,
    step: menghunote_editor::GraphemeStep,
) -> Option<usize> {
    match step {
        menghunote_editor::GraphemeStep::Backward => {
            grapheme_before(display, caret).map(|range| range.start)
        }
        menghunote_editor::GraphemeStep::Forward => {
            grapheme_after(display, caret).map(|range| range.end)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{grapheme_after, grapheme_before, non_empty, step_target};

    #[test]
    fn moves_over_cjk_and_astral_graphemes() {
        let text = "粗体😀x";
        // 粗 = 0..3, 体 = 3..6, 😀 = 6..10, x = 10..11
        assert_eq!(grapheme_after(text, 0), Some(0..3));
        assert_eq!(grapheme_after(text, 3), Some(3..6));
        assert_eq!(grapheme_after(text, 6), Some(6..10), "😀 整体一个 grapheme");
        assert_eq!(grapheme_before(text, 11), Some(10..11));
        assert_eq!(grapheme_before(text, 10), Some(6..10));
        assert_eq!(grapheme_before(text, 0), None);
        assert_eq!(grapheme_after(text, text.len()), None);
    }

    #[test]
    fn combining_marks_stay_inside_one_grapheme() {
        // e + U+0301 = 一个 grapheme（3 字节）
        let text = "e\u{301}x";
        assert_eq!(grapheme_after(text, 0), Some(0..3));
        assert_eq!(grapheme_before(text, 4), Some(3..4));
        assert_eq!(grapheme_before(text, 3), Some(0..3));
    }

    #[test]
    fn caret_inside_a_grapheme_snaps_to_its_bounds() {
        let text = "😀x";
        // caret 落在 😀 中间（byte 2）——前后都应给出完整 grapheme
        assert_eq!(grapheme_after(text, 2), Some(0..4));
        assert_eq!(grapheme_before(text, 2), None, "还没跨过任何完整 grapheme");
    }

    #[test]
    fn step_target_never_lands_inside_markers() {
        use menghunote_editor::GraphemeStep;
        // 隐藏态：display 里没有 `**`，只剩内容
        let display = "粗体 与 代码";
        // 在"体"之后（display 6）左移 → 落在"体"之前（display 3）
        assert_eq!(step_target(display, 6, GraphemeStep::Backward), Some(3));
        // 再左移 → 落在"粗"之前（display 0）
        assert_eq!(step_target(display, 3, GraphemeStep::Backward), Some(0));
        // 在 0 处左移 → None（保持原 caret）
        assert_eq!(step_target(display, 0, GraphemeStep::Backward), None);
        // 右移对称
        assert_eq!(step_target(display, 0, GraphemeStep::Forward), Some(3));
        assert_eq!(
            step_target(display, 6, GraphemeStep::Forward),
            Some(7),
            "空格是一个 grapheme"
        );
        let end = display.len();
        assert_eq!(step_target(display, end, GraphemeStep::Forward), None);
    }

    #[test]
    fn shown_markers_are_ordinary_text_when_inside_a_construct() {
        use menghunote_editor::GraphemeStep;
        // 标记已显现：display 里就是真实文本，按 display 移动自然经过 `*`
        let display = "**粗体**";
        assert_eq!(step_target(display, 0, GraphemeStep::Forward), Some(1));
        assert_eq!(step_target(display, 1, GraphemeStep::Forward), Some(2));
        assert_eq!(step_target(display, 2, GraphemeStep::Forward), Some(5));
        assert_eq!(step_target(display, 5, GraphemeStep::Backward), Some(2));
    }

    #[test]
    fn empty_line_and_mixed_text() {
        use menghunote_editor::GraphemeStep;
        assert_eq!(step_target("", 0, GraphemeStep::Forward), None);
        assert_eq!(step_target("", 0, GraphemeStep::Backward), None);
        // 中英混排 + emoji + 组合字符：每步都是一个 grapheme
        let display = "a中😀e\u{301}x";
        let mut at = 0;
        let mut visited = Vec::new();
        while let Some(next) = step_target(display, at, GraphemeStep::Forward) {
            visited.push(next);
            at = next;
        }
        assert_eq!(at, display.len());
        assert_eq!(visited.len(), 5, "a / 中 / 😀 / é / x");
    }

    #[test]
    fn empty_ranges_are_rejected() {
        assert_eq!(non_empty(3..3), None);
        assert_eq!(non_empty(3..5), Some(3..5));
    }
}
