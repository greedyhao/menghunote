//! 行 shaping 的唯一入口：绘制与键盘导航共用同一套字体 / 字号。
//!
//! 之所以抽出来：上下移动光标时要在**目标视觉行**里把"像素 x"换回"源码字节"，
//! 而那一行可能当时不在绘制范围内（PageUp/PageDown 就会这样）。如果键盘
//! 路径拿不到 shaping 结果，就只能用"字符数 × 平均宽度"估算 —— 这正是任务里
//! 明令禁止的做法。所以这里提供一个按需 shaping 的函数，两条路径共用。

use std::ops::Range;

use gpui::{px, FontStyle, FontWeight, ShapedLine, SharedString, TextRun, Window};
use menghunote_editor::StyleKind;

use super::code_highlight::CodeHighlightClass;
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
    text_runs_with_color(spans, theme::text_color())
}

/// 同 [`text_runs`]，但显式指定 `Plain` 段的基础色。
///
/// 占位文案（图片/Mermaid）压在浅灰/深灰占位框上，必须用 `object_label_color` /
/// `mermaid_placeholder_text` 而不是正文色，否则深色主题下会看不见。
#[must_use]
pub fn text_runs_with_color(spans: &[(usize, StyleKind)], plain_color: gpui::Rgba) -> Vec<TextRun> {
    let base = theme::font();
    let text_color = plain_color.into();
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
                StyleKind::WikiLink => {
                    run.color = theme::link_color().into();
                    run.underline = Some(gpui::UnderlineStyle {
                        thickness: px(1.0),
                        color: Some(theme::link_color().into()),
                        wavy: false,
                    });
                }
                StyleKind::WikiLinkUnresolved => {
                    run.color = theme::status_bar_text().into();
                    run.underline = Some(gpui::UnderlineStyle {
                        thickness: px(1.0),
                        color: Some(theme::status_bar_text().into()),
                        wavy: true,
                    });
                }
            }
            run
        })
        .collect()
}

/// 把样式段变成 runs；空段时生成一个覆盖全文的 Plain run。
///
/// 这是 preedit 可见性的最小保证：GPUI 0.10 的 `shape_line` 对空 runs 不兜底，
/// 直接传 `&[]` 会得到零字形（标题输入框自己构造了 run，所以正常）。
fn text_runs_for_line(
    spans: &[(usize, StyleKind)],
    text_len: usize,
    plain_color: gpui::Rgba,
) -> Vec<TextRun> {
    if spans.is_empty() && text_len > 0 {
        text_runs_with_color(&[(text_len, StyleKind::Plain)], plain_color)
    } else {
        text_runs_with_color(spans, plain_color)
    }
}

/// 用**代码块语法高亮** spans shaping 一行代码（只改颜色，不改布局/编辑）。
///
/// `syntax_spans` 是**相对 `text`** 的字节区间（调用方已把 display 坐标裁剪到本
/// 视觉行）。未命中的区间用正文色（`theme::text_color()`），字体由 `mono` 决定
/// （代码行恒为等宽）；`TextRun` 的字体/字号与普通行 shaping 完全一致，所以行宽、
/// caret 位置、命中几何都不变。
#[must_use]
pub fn shape_row_code(
    window: &Window,
    text: &str,
    syntax_spans: &[(Range<usize>, CodeHighlightClass)],
    font_size: f32,
    mono: bool,
) -> ShapedLine {
    let shared = SharedString::from(text);
    let runs = if shared.is_empty() {
        Vec::new()
    } else {
        code_text_runs(text, syntax_spans, mono)
    };
    window
        .text_system()
        .shape_line(shared, px(font_size), &runs, None)
}

/// 代码高亮 spans → 覆盖全文的 `TextRun`（空洞补正文色）。
///
/// 防御性裁剪：spans 越界、重叠、切在 UTF-8 中间都被修正或丢弃；任何情况下
/// runs 的 `len` 之和都等于 `text.len()`，否则 GPUI 的 shaping 会丢字/错位。
fn code_text_runs(
    text: &str,
    syntax_spans: &[(Range<usize>, CodeHighlightClass)],
    mono: bool,
) -> Vec<TextRun> {
    let font = if mono {
        theme::code_block_font()
    } else {
        theme::font()
    };
    let plain_color = theme::text_color().into();
    let mut runs: Vec<TextRun> = Vec::new();
    let mut cursor = 0usize;
    let push = |runs: &mut Vec<TextRun>, len: usize, color: gpui::Hsla| {
        if len == 0 {
            return;
        }
        // 相邻同色合并，避免一行几十个等价 run。
        if let Some(last) = runs.last_mut() {
            if last.color == color {
                last.len += len;
                return;
            }
        }
        runs.push(TextRun {
            len,
            font: font.clone(),
            color,
            background_color: None,
            underline: None,
            strikethrough: None,
        });
    };
    for (range, class) in syntax_spans {
        let mut start = range.start.max(cursor).min(text.len());
        let mut end = range.end.min(text.len());
        while start < end && !text.is_char_boundary(start) {
            start += 1;
        }
        while end > start && !text.is_char_boundary(end) {
            end -= 1;
        }
        if end <= start {
            continue;
        }
        if cursor < start {
            push(&mut runs, start - cursor, plain_color);
        }
        push(&mut runs, end - start, theme::syntax_color(*class).into());
        cursor = end;
    }
    if cursor < text.len() {
        push(&mut runs, text.len() - cursor, plain_color);
    }
    if runs.is_empty() && !text.is_empty() {
        push(&mut runs, text.len(), plain_color);
    }
    runs
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
    shape_row_styled_at_with_color(window, text, spans, font_size, mono, theme::text_color())
}

/// 同 [`shape_row_styled_at`]，但显式指定 `Plain` 段的基础色（占位文案用）。
#[must_use]
pub fn shape_row_styled_at_with_color(
    window: &Window,
    text: &str,
    spans: &[(usize, StyleKind)],
    font_size: f32,
    mono: bool,
    plain_color: gpui::Rgba,
) -> ShapedLine {
    let shared = SharedString::from(text);
    let runs = if shared.is_empty() {
        Vec::new()
    } else {
        // **必须有覆盖全文的 run**：GPUI 0.10 的 shape_line 在 runs 为空时不会给
        // 纯文本兜底，绘制结果是零字形。IME preedit / 占位文案都曾因此“只剩候选窗”。
        let mut runs = text_runs_for_line(spans, text.len(), plain_color);
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

/// 用带样式 runs shaping 一行文本，并在超过 `max_width` 时**按真实字形宽度截断**。
///
/// 表格单元格用它：GPUI 的文字绘制没有“按矩形裁剪”的参数，超宽文本会溢出到相邻
/// 单元格，所以必须在这里按 shaping 宽度截断并补 `…`（不做“字符数 × 平均宽度”估算）。
///
/// 返回 `None`：文本为空 / 宽度非正 / 连一个省略号都放不下（该格不画字，但边框与
/// 背景仍按几何绘制）。
#[must_use]
pub fn shape_row_truncated_at(
    window: &Window,
    text: &str,
    spans: &[(usize, StyleKind)],
    font_size: f32,
    mono: bool,
    max_width: f32,
) -> Option<TruncatedLine> {
    if text.is_empty() || !max_width.is_finite() || max_width <= 0.0 {
        return None;
    }
    let full = shape_row_styled_at(window, text, spans, font_size, mono);
    if full.width().as_f32() <= max_width {
        return Some(TruncatedLine {
            line: full,
            visible_len: text.len(),
        });
    }
    let ellipsis = "…";
    let ellipsis_spans = [(ellipsis.len(), StyleKind::Plain)];
    let ellipsis = shape_row_styled_at(window, ellipsis, &ellipsis_spans, font_size, mono);
    let ellipsis_width = ellipsis.width().as_f32();
    let budget = (max_width - ellipsis_width).max(0.0);
    if budget <= 0.0 {
        return Some(TruncatedLine {
            line: ellipsis,
            visible_len: 0,
        });
    }
    // `closest_index_for_x` 返回字节下标；吸附到 char 边界后再切（不切坏 UTF-8）。
    let mut cut = full.closest_index_for_x(px(budget)).min(text.len());
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    if cut == 0 {
        return Some(TruncatedLine {
            line: ellipsis,
            visible_len: 0,
        });
    }
    let truncated = format!("{}…", &text[..cut]);
    let truncated_spans = [(truncated.len(), StyleKind::Plain)];
    Some(TruncatedLine {
        line: shape_row_styled_at(window, &truncated, &truncated_spans, font_size, mono),
        visible_len: cut,
    })
}

/// [`shape_row_truncated_at`] 的结果：shaping 后的行 + **可见前缀**的字节长度。
///
/// `visible_len` 是截断点（未截断 = 全文长度）。表格单元格里的链接命中必须按它裁剪：
/// 省略号之后的内容没有画出来，也就不该可点。
#[derive(Debug, Clone)]
pub struct TruncatedLine {
    /// 实际 shaping（并绘制）的行：`display[..visible_len]` + 可选省略号。
    pub line: ShapedLine,
    /// 可见 display 前缀的字节长度（未截断 = `display.len()`）。
    pub visible_len: usize,
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

#[cfg(test)]
mod code_run_tests {
    use super::code_text_runs;
    use crate::editor::code_highlight::CodeHighlightClass;

    #[test]
    fn code_runs_cover_the_whole_line_and_use_token_colors() {
        let text = "fn main() {";
        let spans = vec![
            (0..2, CodeHighlightClass::Keyword),
            (3..7, CodeHighlightClass::Function),
            (10..11, CodeHighlightClass::Punctuation),
        ];
        let runs = code_text_runs(text, &spans, true);
        assert_eq!(
            runs.iter().map(|run| run.len).sum::<usize>(),
            text.len(),
            "runs 必须完整覆盖一行（shaping 不允许丢字节）"
        );
        let plain = crate::editor::theme::text_color().into();
        let keyword = crate::editor::theme::syntax_color(CodeHighlightClass::Keyword).into();
        let function = crate::editor::theme::syntax_color(CodeHighlightClass::Function).into();
        assert_eq!(runs[0].len, 2);
        assert_eq!(runs[0].color, keyword, "关键字 run 用关键字 token 色");
        assert_ne!(runs[0].color, plain);
        // `main` 前面有一个空格空洞 → 正文色 run；随后才是函数色。
        assert_eq!(runs[1].len, 1);
        assert_eq!(runs[1].color, plain);
        assert_eq!(runs[2].len, 4);
        assert_eq!(runs[2].color, function);
        assert_ne!(keyword, function);
    }

    #[test]
    fn code_runs_clip_invalid_utf8_boundaries_instead_of_panicking() {
        let text = "中文代码";
        // 1 不是 char 边界（中 占 0..3）；必须吸附到 3，不能切坏字符。
        let spans = vec![(1..6, CodeHighlightClass::String)];
        let runs = code_text_runs(text, &spans, true);
        assert_eq!(runs.iter().map(|run| run.len).sum::<usize>(), text.len());
        assert_eq!(runs.len(), 3, "非法区间只能覆盖它真正覆盖到的完整字符");
        assert_eq!(runs[0].len, 3, "非法起点吸附到 3（`中` 用正文色）");
        assert_eq!(runs[1].len, 3, "`文` 用字符串色");
        assert_eq!(runs[2].len, 6, "尾部 `代码` 仍是正文色");
    }

    #[test]
    fn code_runs_ignore_out_of_range_and_empty_spans() {
        let text = "abc";
        let spans = vec![
            (2..2, CodeHighlightClass::Keyword),
            (9..12, CodeHighlightClass::String),
        ];
        let runs = code_text_runs(text, &spans, true);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].len, 3);
    }
}

#[cfg(test)]
mod default_run_tests {
    use super::*;

    #[test]
    fn empty_spans_get_one_plain_run_covering_the_text() {
        let text = "组合";
        let runs = text_runs_for_line(&[], text.len(), theme::text_color());
        assert_eq!(
            runs.len(),
            1,
            "空 spans 必须兜底一个 run，否则 preedit 零字形"
        );
        assert_eq!(runs[0].len, text.len());
        assert_eq!(runs[0].color, theme::text_color().into());
    }
}
