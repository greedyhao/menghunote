//! 编辑器视图的视觉常量（字号 / 行高 / 颜色）。
//!
//! 切片 5a 只要求"固定字号 + 固定行高"，不引入主题系统（ADR 0003 §7 切片 5
//! 的可见验收只要肉眼能确认绘制与命中）。所有像素值都用 `f32` 存，
//! 在用到的地方再包一层 `px()`，避免常量求值依赖 GPUI 的构造器。

use gpui::{rgb, rgba, Font, Rgba};

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
/// Mermaid 渲染中/失败时的占位框高度（像素；图形通常比图片高）。
pub const MERMAID_PLACEHOLDER_HEIGHT: f32 = 120.0;
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

// ---------------------------------------------------------------------------
// 表格 Object Block（ADR 0019 S3a）
// ---------------------------------------------------------------------------

/// 表格对象与上下文字的间距（像素）：行高 = 表格高度 + 2 × 该值。
pub const TABLE_PADDING: f32 = 6.0;
/// 表头行高度（像素）：固定值，布局度量与绘制共用（不按字形高度估算）。
pub const TABLE_HEADER_HEIGHT: f32 = 28.0;
/// 数据行高度（像素）：固定值，长表格可以按行裁剪而无需逐行 shaping 定量。
pub const TABLE_ROW_HEIGHT: f32 = 26.0;
/// 单列最小宽度（像素）：窄窗口下优先保证可读，超出内容宽度的部分按裁剪处理。
pub const TABLE_MIN_COLUMN_WIDTH: f32 = 56.0;
/// 单元格左右内边距（像素）。
pub const TABLE_CELL_PAD_X: f32 = 8.0;
/// 单元格文字字号（像素）。
pub const TABLE_FONT_SIZE: f32 = 13.0;
/// 表格边框宽度（像素）。
pub const TABLE_BORDER_WIDTH: f32 = 1.0;

// ---------------------------------------------------------------------------
// 表格右下角「加行 / 加列」浮层（ADR 0019 S3b）
// ---------------------------------------------------------------------------

/// 边缘操作浮层的高度（像素）。比悬浮工具条矮一档：它是贴在表格右下角的小胶囊，
/// 不抢表头的视觉重心。
pub const TABLE_ACTIONS_HEIGHT: f32 = 22.0;
/// 单枚**插入**按钮最小宽度（像素）：`+ 行` / `+ 列` 同排。窄窗口下这两枚必须还能
/// 单独放得下（S3b1 契约：320px 窗口里浮层仍夹在编辑区内）。
pub const TABLE_ACTIONS_BUTTON_WIDTH: f32 = 34.0;
/// 单枚**删除**按钮最小宽度（像素）：`删除行` / `删除列` 是 3 个 12px 汉字，
/// 36 + 左右内边距 8 = 44，留 2px 余量。窄到放不下时整个删除组不显示（入口隐藏），
/// 而不是把浮层挤出编辑区。
pub const TABLE_ACTIONS_DELETE_BUTTON_WIDTH: f32 = 46.0;
/// 浮层与表格下边框的间距（像素）：浮层整体落在表格**外面**，不遮挡单元格文本。
pub const TABLE_ACTIONS_GAP: f32 = 2.0;
/// 浮层视觉偏移（像素）：贴住表格右边缘。
pub const TABLE_ACTIONS_PAD: f32 = 4.0;
/// 按钮圆角（像素）。
pub const TABLE_ACTIONS_RADIUS: f32 = 6.0;

/// 边缘操作浮层底色（与悬浮工具条一致，压住下方文字也看得清）。
#[must_use]
pub fn table_actions_bg() -> Rgba {
    rgb(0xffffff)
}

/// 边缘操作浮层边框色。
#[must_use]
pub fn table_actions_border() -> Rgba {
    rgb(0xd0d7de)
}

/// 边缘操作浮层按钮文字色。
#[must_use]
pub fn table_actions_text() -> Rgba {
    rgb(0x1f2328)
}

/// 边缘操作浮层**禁用**按钮的文字色（例如只剩 1 列时的「删除列」）。
///
/// 禁用是“提示”而不是隐藏：按钮画出来但明显弱化，用户能看出这里以后可能有动作，
/// 同时点击不产生任何请求（不把表格改成非法结构）。
#[must_use]
pub fn table_actions_text_disabled() -> Rgba {
    rgb(0x8c959f)
}

// ---------------------------------------------------------------------------
// 表格行列拖拽把手（ADR 0019 S3b5：拖拽重排行 / 列）
// ---------------------------------------------------------------------------

/// 列把手（表头顶部的小胶囊）宽度（像素）。
pub const TABLE_DRAG_COLUMN_HANDLE_WIDTH: f32 = 18.0;
/// 列把手高度（像素）：**压在表头上边框上**，一半在表格外、一半在表头内，不遮挡文字。
pub const TABLE_DRAG_COLUMN_HANDLE_HEIGHT: f32 = 6.0;
/// 行把手（数据行左侧的小胶囊）宽度（像素）。
pub const TABLE_DRAG_ROW_HANDLE_WIDTH: f32 = 6.0;
/// 行把手高度上限（像素）：实际高度 = `min(本值, 行高 - 8)`，压在表格左边框上。
pub const TABLE_DRAG_ROW_HANDLE_HEIGHT: f32 = 14.0;
/// 把手命中区在绘制矩形外扩的像素：更好点中，但仍不盖住单元格文字。
pub const TABLE_DRAG_HIT_PAD: f32 = 3.0;
/// 拖动时插入位置指示线宽度（像素）。
pub const TABLE_DRAG_INDICATOR_THICKNESS: f32 = 2.0;
/// 指针离开表格多远算“拖出表格 → 取消”（像素；任意方向）。
pub const TABLE_DRAG_CANCEL_MARGIN: f32 = 24.0;

/// 把手（可拖拽）底色：比边框略深，停在表头/行边框上仍然可见。
#[must_use]
pub fn table_drag_handle_bg() -> Rgba {
    rgb(0x8c959f)
}

/// 把手悬停底色（drag affordance 的高亮）。
#[must_use]
pub fn table_drag_handle_hover_bg() -> Rgba {
    rgb(0x57606a)
}

/// 拖动中的插入位置指示线颜色（主题强调色）。
#[must_use]
pub fn table_drag_indicator() -> Rgba {
    rgb(0x0969da)
}

// ---------------------------------------------------------------------------
// 表格整行/整列选中 + 拖拽 ghost（ADR 0019 S3b6：Obsidian 式选中/拖动反馈）
// ---------------------------------------------------------------------------

/// 整行/整列选中的填充色：**低透明度**（约 12%），压在表格上文字仍然清晰可读。
#[must_use]
pub fn table_axis_selection_bg() -> Rgba {
    rgba(0x0969da1f)
}

/// 整行/整列选中的边框色（强调色）。
#[must_use]
pub fn table_axis_selection_border() -> Rgba {
    rgb(0x0969da)
}

/// 整行/整列选中的边框宽度（像素）。
pub const TABLE_AXIS_SELECTION_BORDER: f32 = 2.0;

/// 被选中的行/列把手底色：强调色，与普通灰把手区分（选中反馈的一部分）。
#[must_use]
pub fn table_drag_handle_selected_bg() -> Rgba {
    rgb(0x0969da)
}

/// 拖拽 ghost（跟随指针的整行/整列半透明预览）填充色（约 20%）。
#[must_use]
pub fn table_drag_ghost_bg() -> Rgba {
    rgba(0x0969da33)
}

/// 拖拽 ghost 的边框色。
#[must_use]
pub fn table_drag_ghost_border() -> Rgba {
    rgb(0x0969da)
}

/// 拖拽 ghost 的边框宽度（像素）。
pub const TABLE_DRAG_GHOST_BORDER: f32 = 1.0;

/// 指针离开按下点超过该像素才显示 ghost：单击把手只看选中反馈，不闪 ghost。
pub const TABLE_DRAG_GHOST_MIN_MOVE: f32 = 3.0;

/// 拖动 ghost 的列块尺寸：只画表头顶线上的一个小块，不复制整列（省绘制、
/// 也不会拖到哪都铺满一屏）。`u` 轴固定贴表头顶边，`x` 跟指针。
pub const TABLE_DRAG_GHOST_COLUMN_WIDTH: f32 = 28.0;
/// 拖动 ghost 的列块高度。
pub const TABLE_DRAG_GHOST_COLUMN_HEIGHT: f32 = 8.0;
/// 拖动 ghost 的行块尺寸：只画左边线上的一个小块，`y` 跟指针。
pub const TABLE_DRAG_GHOST_ROW_WIDTH: f32 = 8.0;
/// 拖动 ghost 的行块高度。
pub const TABLE_DRAG_GHOST_ROW_HEIGHT: f32 = 22.0;

/// ghost 上“移动第 N 列/行”浮层的底色（深色半透明，压在任何表格内容上都可读）。
#[must_use]
pub fn table_drag_ghost_label_bg() -> Rgba {
    rgba(0x1f2328e6)
}

/// ghost 浮层的文字色。
#[must_use]
pub fn table_drag_ghost_label_text() -> Rgba {
    rgb(0xffffff)
}

/// ghost 浮层与指针的横向间距（像素）。
pub const TABLE_DRAG_GHOST_LABEL_OFFSET_X: f32 = 14.0;
/// ghost 浮层与指针的纵向间距（像素）。
pub const TABLE_DRAG_GHOST_LABEL_OFFSET_Y: f32 = 16.0;
/// ghost 浮层的宽度上限（像素）：靠近编辑区右边缘时按它夹住，不压到宿主其它 UI。
pub const TABLE_DRAG_GHOST_LABEL_MAX_WIDTH: f32 = 220.0;
/// ghost 浮层的高度上限（像素）：靠近编辑区下边缘时按它夹住。
pub const TABLE_DRAG_GHOST_LABEL_HEIGHT: f32 = 24.0;

// ---------------------------------------------------------------------------
// 正文右键菜单（ADR 0019 S3b4：「插入 → 表格」）
// ---------------------------------------------------------------------------

/// 右键菜单面板宽度（像素）。
pub const EDITOR_CONTEXT_MENU_WIDTH: f32 = 148.0;
/// 右键菜单面板高度（像素）：4px 内边距 ×2 + 分组标签 18 + gap 2 + 菜单项 24 + 边框 2。
pub const EDITOR_CONTEXT_MENU_HEIGHT: f32 = 54.0;
/// 链接右键菜单面板高度（像素）：4px 内边距 ×2 + 3 个菜单项 × 24 + 2 个 gap × 2 + 边框 2。
pub const EDITOR_CONTEXT_MENU_LINK_HEIGHT: f32 = 86.0;

// ---------------------------------------------------------------------------
// 外部链接确认框（file/UNC 与未知自定义 scheme）
// ---------------------------------------------------------------------------

/// 确认框面板宽度（像素）。
pub const LINK_CONFIRM_WIDTH: f32 = 340.0;
/// 确认框遮罩色（半透明黑，压住正文但不完全遮挡上下文）。
#[must_use]
pub fn link_confirm_overlay() -> Rgba {
    rgba(0x00000033)
}
/// 确认框「打开」主按钮底色。
#[must_use]
pub fn link_confirm_primary_bg() -> Rgba {
    rgb(0x0969da)
}
/// 确认框「打开」主按钮文字色。
#[must_use]
pub fn link_confirm_primary_text() -> Rgba {
    rgb(0xffffff)
}
/// 链接提示（非阻塞 toast）底色。
#[must_use]
pub fn link_notice_bg() -> Rgba {
    rgba(0x1f2328e6)
}
/// 链接提示（非阻塞 toast）文字色。
#[must_use]
pub fn link_notice_text() -> Rgba {
    rgb(0xffffff)
}

/// 表格外框 / 单元格分隔线颜色。
#[must_use]
pub fn table_border() -> Rgba {
    rgb(0xd0d7de)
}

/// 表格底色（不透明，压住正文行背景）。
#[must_use]
pub fn table_bg() -> Rgba {
    rgb(0xffffff)
}

/// 表头底色（与正文行区分）。
#[must_use]
pub fn table_header_bg() -> Rgba {
    rgb(0xf6f8fa)
}

/// 选中单元格的底色（ADR 0019 S3b2）：与文本选区同色系，压在表格底色之上、
/// 单元格文字之下，因此文字仍然清晰可读。
#[must_use]
pub fn table_cell_selected_bg() -> Rgba {
    rgb(0xdbeafe)
}

/// 选中单元格的描边色（2px，压住 1px 网格线）。
#[must_use]
pub fn table_cell_selected_border() -> Rgba {
    rgb(0x0969da)
}

/// 选中单元格的描边宽度（像素）。
pub const TABLE_CELL_SELECTED_BORDER: f32 = 2.0;

// ---------------------------------------------------------------------------
// 图谱覆盖层（G1）：全屏 canvas 自绘的背景/节点/边/标签颜色。
// 与编辑器共用同一套中性灰 + 强调蓝，避免出现第二套配色。
// ---------------------------------------------------------------------------

/// 图谱覆盖层的背景（近不透明的窗口底色：盖住编辑器但仍能透出一点层次）。
#[must_use]
pub fn graph_backdrop() -> Rgba {
    gpui::rgba(0xf6f8faf7)
}

/// 图谱顶部工具条的底色（不透明，压住下面的边线）。
#[must_use]
pub fn graph_toolbar_bg() -> Rgba {
    rgb(0xffffff)
}

/// 节点填充色（未悬停）。
#[must_use]
pub fn graph_node_bg() -> Rgba {
    rgb(0xdbeafe)
}

/// 节点描边色（未悬停）。
#[must_use]
pub fn graph_node_border() -> Rgba {
    rgb(0x0969da)
}

/// 悬停节点/邻居节点的填充色（强调态）。
#[must_use]
pub fn graph_node_active_bg() -> Rgba {
    rgb(0x0969da)
}

/// 悬停时的节点文字色（深底白字）。
#[must_use]
pub fn graph_node_active_text() -> Rgba {
    rgb(0xffffff)
}

/// 与悬停节点无关的节点被压暗后的填充色。
#[must_use]
pub fn graph_node_dim_bg() -> Rgba {
    rgb(0xf0f3f6)
}

/// 与悬停节点无关的节点被压暗后的描边色。
#[must_use]
pub fn graph_node_dim_border() -> Rgba {
    rgb(0xd0d7de)
}

/// 节点标签文字色（未悬停）。
#[must_use]
pub fn graph_label_text() -> Rgba {
    rgb(0x1f2328)
}

/// 边线颜色（未高亮）。
#[must_use]
pub fn graph_edge() -> Rgba {
    rgb(0xc3ccd6)
}

/// 悬停高亮时的边线颜色。
#[must_use]
pub fn graph_edge_active() -> Rgba {
    rgb(0x0969da)
}

/// 悬停时被压暗的边线颜色（只突出与被悬停节点相连的边）。
#[must_use]
pub fn graph_edge_dim() -> Rgba {
    rgb(0xe4e8ec)
}
