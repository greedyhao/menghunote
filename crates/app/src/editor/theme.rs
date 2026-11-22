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

/// 引用块每层缩进（像素）。嵌套引用按层数累加。
pub const QUOTE_INDENT_PER_LEVEL: f32 = 16.0;

/// 列表每层缩进（像素）。bullet/number 的 gutter 由该层级缩进 + marker 显示宽度组成，
/// 续行 hanging indent 与 marker 宽度**同一来源**（ADR 0019 S1：度量/绘制/命中共用几何）。
pub const LIST_INDENT_PER_LEVEL: f32 = 24.0;

/// 任务复选框在 22px 行高内的最大边长（像素；ADR 0019 S3）。
///
/// 实际绘制边长 = `min(占位宽度, 行高 - 8, 本值)`，最小 [`TASK_BOX_MIN_SIZE`]：
/// 宽度来自真实 shaping 的两连 NBSP，字号/主题变了也不会画到行外。
pub const TASK_BOX_MAX_SIZE: f32 = 15.0;
/// 任务复选框最小边长（像素）：再小就没有可读的勾形，且点击命中由整个占位矩形决定。
pub const TASK_BOX_MIN_SIZE: f32 = 9.0;
/// 任务复选框圆角（像素）。
pub const TASK_BOX_RADIUS: f32 = 3.0;

/// 任务复选框（未勾选）底色。
#[must_use]
pub fn task_box_bg() -> Rgba {
    rgb(0xffffff)
}

/// 任务复选框（未勾选）描边色。
#[must_use]
pub fn task_box_border() -> Rgba {
    rgb(0x8c959f)
}

/// 任务复选框（已勾选）底色 / 描边色（主题强调色）。
#[must_use]
pub fn task_box_checked_bg() -> Rgba {
    rgb(0x0969da)
}

/// 任务复选框（已勾选）描边色。
#[must_use]
pub fn task_box_checked_border() -> Rgba {
    rgb(0x0969da)
}

/// 已勾选时的勾形颜色（压在强调色上）。
#[must_use]
pub fn task_box_check() -> Rgba {
    rgb(0xffffff)
}

/// 引用块左侧竖条宽度（像素）。
pub const QUOTE_BAR_WIDTH: f32 = 3.0;
/// 竖条与文字之间的距离（像素）。
pub const QUOTE_BAR_GAP: f32 = 6.0;

/// 引用块左侧竖条的颜色（比正文弱，用于"这是个引用块"的视觉锚点）。
#[must_use]
pub fn quote_bar_color() -> Rgba {
    rgb(0xc4c9d0)
}

/// 引用块正文颜色（略弱化，字号不变）。
#[must_use]
pub fn quote_text_color() -> Rgba {
    rgb(0x57606a)
}

/// 搜索结果里高亮片段的底色（浅黄，和正文区分开）。
#[must_use]
pub fn search_highlight_bg() -> Rgba {
    rgb(0xfff1b8)
}

/// 高亮片段的文字色（保持与正文一致的深浅，避免"高亮处反而看不清"）。
#[must_use]
pub fn search_highlight_text() -> Rgba {
    rgb(0x24292f)
}

/// 索引降级/查询失败的提示色。
#[must_use]
pub fn search_warn_text() -> Rgba {
    rgb(0xb35900)
}

/// 进度条底槽色（浅灰，和面板背景区分）。
#[must_use]
pub fn search_progress_track() -> Rgba {
    rgb(0xe6e9ed)
}

/// 进度条填充色（低饱和蓝，与选区色同族）。
#[must_use]
pub fn search_progress_fill() -> Rgba {
    rgb(0x7aa7e8)
}

/// 开关「打开」态底色。
#[must_use]
pub fn search_toggle_active() -> Rgba {
    rgb(0x0969da)
}

/// 开关「打开」态文字色。
#[must_use]
pub fn search_toggle_active_text() -> Rgba {
    rgb(0xffffff)
}

/// 搜索结果里"当前选中分组"的底色（浅蓝，保证正文与统计行都可读）。
#[must_use]
pub fn search_group_selected() -> Rgba {
    rgb(0xe8f0fb)
}

// ---------------------------------------------------------------------------
// 图片 Object Block（ADR 0019 S2a）
// ---------------------------------------------------------------------------

/// 图片/占位框的上下内边距（像素）：行高 = 内容高度 + 2 × 该值。
pub const OBJECT_PADDING: f32 = 6.0;
/// 图片尚未就绪（加载中 / 缺失 / 失败）时的占位框高度（像素）。
pub const OBJECT_PLACEHOLDER_HEIGHT: f32 = 34.0;
/// 占位框里 alt/状态文案的字号（像素）。
pub const OBJECT_LABEL_FONT_SIZE: f32 = 12.0;
/// 占位文案与框左边界的内边距（像素）。
pub const OBJECT_LABEL_PAD_X: f32 = 10.0;
/// 图片/占位框的圆角（像素）。
pub const OBJECT_CORNER_RADIUS: f32 = 4.0;
/// 选中态描边宽度（像素）。
pub const OBJECT_SELECTED_BORDER: f32 = 2.0;
/// 未选中时图片边框宽度（像素；0 = 不描边）。
pub const OBJECT_BORDER: f32 = 0.0;

/// 占位框底色（浅灰，与正文区分但不抢眼）。
#[must_use]
pub fn object_placeholder_bg() -> Rgba {
    rgb(0xf3f4f6)
}

/// 占位框边框色。
#[must_use]
pub fn object_placeholder_border() -> Rgba {
    rgb(0xd0d7de)
}

/// 占位文案颜色。
#[must_use]
pub fn object_label_color() -> Rgba {
    rgb(0x59636e)
}

/// 选中态描边色（与光标同族的蓝，明确"选中了整块"）。
#[must_use]
pub fn object_selected_border() -> Rgba {
    rgb(0x0969da)
}

/// 悬浮工具条高度（像素，Obsidian 式圆形/胶囊按钮）。
pub const OBJECT_TOOLBAR_HEIGHT: f32 = 28.0;
/// 悬浮工具条按钮（单个）宽度（像素）。
pub const OBJECT_TOOLBAR_BUTTON_WIDTH: f32 = 28.0;
/// 悬浮工具条与图片顶部的间距（像素）。
pub const OBJECT_TOOLBAR_GAP: f32 = 4.0;

/// 悬浮工具条底色（不透明，压住图片也看得清）。
#[must_use]
pub fn object_toolbar_bg() -> Rgba {
    rgb(0xffffff)
}

/// 悬浮工具条边框色。
#[must_use]
pub fn object_toolbar_border() -> Rgba {
    rgb(0xd0d7de)
}

/// 悬浮工具条按钮文字色。
#[must_use]
pub fn object_toolbar_text() -> Rgba {
    rgb(0x1f2328)
}
