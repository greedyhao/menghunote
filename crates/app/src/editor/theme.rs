//! 编辑器视图的视觉常量（字号 / 行高 / 颜色）。
//!
//! 切片 5a 只要求"固定字号 + 固定行高"，不引入主题系统（ADR 0003 §7 切片 5
//! 的可见验收只要肉眼能确认绘制与命中）。所有像素值都用 `f32` 存，
//! 在用到的地方再包一层 `px()`，避免常量求值依赖 GPUI 的构造器。

use gpui::{rgb, Font, Rgba};

/// 等宽字体族（Windows 自带 Consolas；缺失时由 GPUI 的平台回退链接管）。
pub const FONT_FAMILY: &str = "Consolas";
/// 正文字号（像素）。
pub const FONT_SIZE: f32 = 15.0;
/// 行高（像素）：固定值，视觉行高度一律等于它，命中可以直接用 `y / 行高` 定位。
pub const LINE_HEIGHT: f32 = 22.0;
/// 文字区左右内边距（像素）。
pub const PAD_X: f32 = 14.0;
/// 文字区顶部内边距（像素）。
pub const PAD_Y: f32 = 8.0;

/// 编辑器字体。
#[must_use]
pub fn font() -> Font {
    gpui::font(FONT_FAMILY)
}

/// 窗口背景。
#[must_use]
pub fn window_bg() -> Rgba {
    rgb(0xffffff)
}

/// 正文颜色。
#[must_use]
pub fn text_color() -> Rgba {
    rgb(0x1f2328)
}

/// 选区底色。
#[must_use]
pub fn selection_bg() -> Rgba {
    rgb(0xcfe4fd)
}

/// 光标颜色。
#[must_use]
pub fn caret_color() -> Rgba {
    rgb(0x0969da)
}

/// 状态栏背景。
#[must_use]
pub fn status_bar_bg() -> Rgba {
    rgb(0xf6f8fa)
}

/// 状态栏文字。
#[must_use]
pub fn status_bar_text() -> Rgba {
    rgb(0x59636e)
}

/// 侧栏背景。
#[must_use]
pub fn sidebar_bg() -> Rgba {
    rgb(0xf6f8fa)
}

/// 侧栏与正文之间的分隔线。
#[must_use]
pub fn sidebar_border() -> Rgba {
    rgb(0xd0d7de)
}

/// 侧栏"已选中但未打开"的浅色底（与"已打开"的蓝色区分开）。
#[must_use]
pub fn sidebar_selected_bg() -> Rgba {
    rgb(0xeaeef6)
}

/// 危险操作（删除）的文字色。
#[must_use]
pub fn danger_text() -> Rgba {
    rgb(0xcf222e)
}

/// 右键菜单 / popup 的面板底色。
#[must_use]
pub fn menu_bg() -> Rgba {
    rgb(0xffffff)
}

/// 行内输入框的表面色（不透明，压住行背景的蓝/浅色）。
#[must_use]
pub fn input_bg() -> Rgba {
    rgb(0xffffff)
}

/// 行内输入框边框（中性灰，和行底色分层）。
#[must_use]
pub fn input_border() -> Rgba {
    rgb(0x57606a)
}

/// 行内输入框的"选中"高亮：**比行选中底色（0xcfe4fd）明显更深**的蓝，
/// 深色文字压在上面仍有足够对比（约 7:1）。
#[must_use]
pub fn input_selection() -> Rgba {
    rgb(0x7aa7e8)
}

/// 行内输入框的 caret（深色，在白色底色与深蓝高亮上都清楚）。
#[must_use]
pub fn input_caret() -> Rgba {
    rgb(0x1f2328)
}

/// 行内代码的底色（浅灰，压在窗口底色上仍可读）。
#[must_use]
pub fn code_bg() -> Rgba {
    rgb(0xeff1f3)
}

/// 链接文字色（与正文区分，再配合下划线）。
#[must_use]
pub fn link_color() -> Rgba {
    rgb(0x0969da)
}

/// 等宽字体族（行内代码用）；找不到时退回编辑器主字体。
pub const MONO_FAMILY: &str = "Consolas";

/// 行高系数：`line_height = font_size * LINE_HEIGHT_FACTOR`（四舍五入到整像素）。
pub const LINE_HEIGHT_FACTOR: f32 = 1.5;

/// 正文行高（= [`FONT_SIZE`] × 系数，保持早先的 22px 视觉）。
pub const DEFAULT_LINE_HEIGHT: f32 = 22.0;

/// 标题各级字号（h1..h6）。块级渲染用它，**不要**在业务代码里写字号。
#[must_use]
pub fn heading_font_size(level: u8) -> f32 {
    match level {
        1 => 26.0,
        2 => 22.0,
        3 => 19.0,
        4 => 17.0,
        _ => 16.0,
    }
}

/// 某一字号对应的行高（固定系数，四舍五入）。
#[must_use]
pub fn line_height_for(font_size: f32) -> f32 {
    (font_size * LINE_HEIGHT_FACTOR).round().max(1.0)
}

/// 围栏代码块里用的字体（等宽）。
#[must_use]
pub fn code_block_font() -> Font {
    let mut font = font();
    font.family = MONO_FAMILY.into();
    font
}
