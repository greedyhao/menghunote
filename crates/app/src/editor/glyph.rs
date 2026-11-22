//! 行 shaping 的唯一入口：绘制与键盘导航共用同一套字体 / 字号。
//!
//! 之所以抽出来：上下移动光标时要在**目标视觉行**里把"像素 x"换回"源码字节"，
//! 而那一行可能当时不在绘制范围内（PageUp/PageDown 就会这样）。如果键盘
//! 路径拿不到 shaping 结果，就只能用"字符数 × 平均宽度"估算 —— 这正是任务里
//! 明令禁止的做法。所以这里提供一个按需 shaping 的函数，两条路径共用。

use gpui::{px, ShapedLine, SharedString, TextRun, Window};

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
