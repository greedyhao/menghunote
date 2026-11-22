//! 行 shaping 的唯一入口：绘制与键盘导航共用同一套字体 / 字号。
//!
//! 之所以抽出来：上下移动光标时要在**目标视觉行**里把"像素 x"换回"源码字节"，
//! 而那一行可能当时不在绘制范围内（PageUp/PageDown 就会这样）。如果键盘
//! 路径拿不到 shaping 结果，就只能用"字符数 × 平均宽度"估算 —— 这正是任务里
//! 明令禁止的做法。所以这里提供一个按需 shaping 的函数，两条路径共用。

use std::ops::Range;

use gpui::{px, FontStyle, FontWeight, ShapedLine, SharedString, TextRun, Window};
use menghunote_editor::StyleKind;

use super::theme;

/// 用编辑器统一字体 shaping 一行文本。
///
/// `text` 必须是**单行**（不含 `\n`）：`TextSystem::shape_line` 对它断言。
#[must_use]
pub fn shape_row(window: &Window, text: &str) -> ShapedLine {
    let font = theme::font();
    let font_size = px(theme::FONT_SIZE);
    let shared = SharedString::from(text);
    let mut runs: Vec<TextRun> = Vec::new();
    if !shared.is_empty() {
        runs.push(TextRun {
            len: shared.len(),
            font,
            color: theme::text_color().into(),
            background_color: None,
            underline: None,
            strikethrough: None,
        });
    }
    window
        .text_system()
        .shape_line(shared, font_size, &runs, None)
}

/// 一条视觉行内的一段样式：`(字节数, 种类)`。
///
/// **纯函数**产出的中间表示（不含字体/颜色），便于单测；[`text_runs`] 再把它
/// 变成 GPUI 的 [`TextRun`]。
#[must_use]
pub fn line_spans(
    styles: &[(Range<usize>, StyleKind)],
    line: Range<usize>,
) -> Vec<(usize, StyleKind)> {
    let line_len = line.end.saturating_sub(line.start);
    if line_len == 0 {
        return Vec::new();
    }
    let mut spans: Vec<(usize, StyleKind)> = Vec::new();
    let mut cursor = line.start;
    for (range, kind) in styles {
        if range.end <= cursor {
            continue;
        }
        if range.start >= line.end {
            break;
        }
        let start = range.start.max(cursor).max(line.start);
        let end = range.end.min(line.end);
        if start > cursor {
            // styles 声称"完整覆盖"，但真出现空洞也补 Plain，绝不留缝隙。
            push_span(&mut spans, start - cursor, StyleKind::Plain);
        }
        if end > start {
            push_span(&mut spans, end - start, *kind);
            cursor = end;
        }
    }
    if cursor < line.end {
        push_span(&mut spans, line.end - cursor, StyleKind::Plain);
    }
    spans
}

/// 合并同类相邻段，避免生成一堆等价的 `TextRun`。
fn push_span(spans: &mut Vec<(usize, StyleKind)>, len: usize, kind: StyleKind) {
    if len == 0 {
        return;
    }
    match spans.last_mut() {
        Some((last_len, last_kind)) if *last_kind == kind => *last_len += len,
        _ => spans.push((len, kind)),
    }
}

/// `(字节数, 种类)` 列表 → GPUI 的 [`TextRun`] 列表（字体/颜色在这一步决定）。
#[must_use]
pub fn text_runs(spans: &[(usize, StyleKind)]) -> Vec<TextRun> {
    let base = theme::font();
    let text_color = theme::text_color().into();
    spans
        .iter()
        .map(|(len, kind)| {
            let mut font = base.clone();
            let mut run = TextRun {
                len: *len,
                font: base.clone(),
                color: text_color,
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            match kind {
                StyleKind::Plain => {}
                StyleKind::Bold => {
                    font.weight = FontWeight::BOLD;
                    run.font = font;
                }
                StyleKind::Emphasis => {
                    font.style = FontStyle::Italic;
                    run.font = font;
                }
                StyleKind::InlineCode => {
                    font.family = theme::MONO_FAMILY.into();
                    run.font = font;
                    run.background_color = Some(theme::code_bg().into());
                }
                StyleKind::Strike => {
                    run.strikethrough = Some(gpui::StrikethroughStyle {
                        thickness: px(1.0),
                        color: Some(text_color),
                    });
                }
                StyleKind::Link => {
                    run.color = theme::link_color().into();
                    run.underline = Some(gpui::UnderlineStyle {
                        thickness: px(1.0),
                        color: Some(theme::link_color().into()),
                        wavy: false,
                    });
                }
            }
            run
        })
        .collect()
}

/// 用**带样式**的 runs shaping 一行文本。
///
/// 绘制与命中必须共用它：粗体会让字变宽，如果命中用无样式 shaping，点击就会偏。
#[must_use]
pub fn shape_row_styled(window: &Window, text: &str, spans: &[(usize, StyleKind)]) -> ShapedLine {
    shape_row_styled_at(window, text, spans, theme::FONT_SIZE, false)
}

/// 同上，但显式给字号与"是否等宽字体"（块级渲染：标题放大 / 代码块等宽）。
#[must_use]
pub fn shape_row_styled_at(
    window: &Window,
    text: &str,
    spans: &[(usize, StyleKind)],
    font_size: f32,
    mono: bool,
) -> ShapedLine {
    let shared = SharedString::from(text);
    let runs = if shared.is_empty() {
        Vec::new()
    } else {
        let mut runs = text_runs(spans);
        if mono {
            let font = theme::code_block_font();
            for run in &mut runs {
                run.font = font.clone();
            }
        }
        runs
    };
    window
        .text_system()
        .shape_line(shared, px(font_size), &runs, None)
}

#[cfg(test)]
mod tests {
    use super::{line_spans, text_runs};
    use menghunote_editor::StyleKind;

    fn styles(items: &[(usize, usize, StyleKind)]) -> Vec<(std::ops::Range<usize>, StyleKind)> {
        items.iter().map(|(a, b, k)| (*a..*b, *k)).collect()
    }

    #[test]
    fn spans_cover_the_line_exactly() {
        let s = styles(&[(0, 20, StyleKind::Bold)]);
        let spans = line_spans(&s, 3..12);
        assert_eq!(spans.iter().map(|(len, _)| len).sum::<usize>(), 9);
        assert_eq!(spans, vec![(9, StyleKind::Bold)]);
    }

    #[test]
    fn plain_gaps_are_filled() {
        // 覆盖不全（理论上不会发生）也要补 Plain，绝不丢字节
        let s = styles(&[(0, 2, StyleKind::Bold), (5, 8, StyleKind::InlineCode)]);
        let spans = line_spans(&s, 0..10);
        assert_eq!(spans.iter().map(|(len, _)| len).sum::<usize>(), 10);
        assert_eq!(
            spans,
            vec![
                (2, StyleKind::Bold),
                (3, StyleKind::Plain),
                (3, StyleKind::InlineCode),
                (2, StyleKind::Plain),
            ]
        );
    }

    #[test]
    fn spans_are_clipped_to_the_line_and_split_at_boundaries() {
        let s = styles(&[(0, 10, StyleKind::Plain), (10, 20, StyleKind::Link)]);
        let spans = line_spans(&s, 5..15);
        assert_eq!(spans, vec![(5, StyleKind::Plain), (5, StyleKind::Link)]);
    }

    #[test]
    fn adjacent_same_kind_runs_are_merged() {
        let s = styles(&[(0, 4, StyleKind::Bold), (4, 8, StyleKind::Bold)]);
        assert_eq!(line_spans(&s, 0..8), vec![(8, StyleKind::Bold)]);
    }

    #[test]
    fn empty_line_has_no_spans() {
        assert!(line_spans(&styles(&[(0, 10, StyleKind::Bold)]), 4..4).is_empty());
    }

    #[test]
    fn text_runs_lengths_match_the_spans() {
        let spans = vec![
            (3, StyleKind::Plain),
            (4, StyleKind::Bold),
            (5, StyleKind::InlineCode),
        ];
        let runs = text_runs(&spans);
        assert_eq!(runs.len(), 3);
        assert_eq!(runs.iter().map(|r| r.len).sum::<usize>(), 12);
        // 粗体真的改字重；代码有底色
        assert_ne!(runs[1].font.weight, runs[0].font.weight);
        assert!(runs[2].background_color.is_some());
    }
}
