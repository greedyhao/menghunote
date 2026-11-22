//! 编辑器视图的视觉常量（字号 / 行高 / 颜色）。
//!
//! ## 颜色：迁移期兼容包装（ADR 0021）
//!
//! 本文件的**颜色函数**全部是 [`crate::theme`] 的兼容包装：它们读**全局当前主题**
//! （浅色 / 深色 / 跟随系统），因此 600+ 现有调用点不需要改签名就能跟着切主题。
//! 新代码应优先直接接收 `&Theme`（或读 [`crate::theme::current_theme`]），
//! 逐步把语义 token 显式化；迁移完成后这些自由函数可以删除或降为 `light()` 测试辅助。
//!
//! **浅色主题的每个值都与 ADR 0021 迁移前的硬编码颜色逐色一致**，避免视觉回归；
//! 深色主题是新增值。组件只允许读语义 token，不允许在绘制代码里写 `0x...` 颜色。
//!
//! ## 排版常量
//!
//! 字体族/字号/行高/间距属于**排版**，首版不进入主题（ADR 0021 §3.2：主题不得改变
//! 布局命中契约）。所有像素值都用 `f32` 存，在用到的地方再包一层 `px()`，避免常量
//! 求值依赖 GPUI 的构造器。

use gpui::{Font, Rgba};

use super::code_highlight::CodeHighlightClass;

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

/// 引用块左侧竖条宽度（像素）。
pub const QUOTE_BAR_WIDTH: f32 = 3.0;
/// 竖条与文字之间的距离（像素）。
pub const QUOTE_BAR_GAP: f32 = 6.0;

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

/// 悬浮工具条高度（像素，Obsidian 式圆形/胶囊按钮）。
pub const OBJECT_TOOLBAR_HEIGHT: f32 = 28.0;
/// 悬浮工具条按钮（单个）宽度（像素）。
pub const OBJECT_TOOLBAR_BUTTON_WIDTH: f32 = 28.0;
/// 悬浮工具条与图片顶部的间距（像素）。
pub const OBJECT_TOOLBAR_GAP: f32 = 4.0;

// ---------------------------------------------------------------------------
// 代码块灰底 + 右上角「复制」按钮
// ---------------------------------------------------------------------------

/// 代码块灰底的圆角半径（像素）。
pub const CODE_BLOCK_CORNER_RADIUS: f32 = 4.0;
/// 代码块「复制」按钮宽度（像素）。刻意做小：只在光标离开块时浮出。
pub const CODE_COPY_BUTTON_WIDTH: f32 = 26.0;
/// 代码块「复制」按钮高度（像素）。
pub const CODE_COPY_BUTTON_HEIGHT: f32 = 22.0;
/// 「复制」按钮与代码块首行行顶的间距（像素）。
pub const CODE_COPY_BUTTON_TOP_GAP: f32 = 4.0;
/// 「复制」按钮图标尺寸（像素）。
pub const CODE_COPY_ICON_SIZE: f32 = 14.0;

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

/// 整行/整列选中的边框宽度（像素）。
pub const TABLE_AXIS_SELECTION_BORDER: f32 = 2.0;

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

/// 选中单元格的描边宽度（像素）。
pub const TABLE_CELL_SELECTED_BORDER: f32 = 2.0;

// ---------------------------------------------------------------------------
// 颜色：语义 token 兼容包装（ADR 0021）
// ---------------------------------------------------------------------------
//
// 每个函数都读**全局当前主题**。浅色主题的值与迁移前硬编码逐色一致；深色主题
// 由 `crate::theme` 提供。绘制代码不要新增 `rgb(...)` 字面量。

macro_rules! theme_token_fns {
    ($($(#[$doc:meta])* $name:ident => $field:ident),* $(,)?) => {
        $(
            $(#[$doc])*
            #[must_use]
            pub fn $name() -> Rgba {
                crate::theme::current_theme().$field
            }
        )*
    };
}

theme_token_fns! {
    /// 窗口背景。
    window_bg => window_bg,
    /// 正文颜色。
    text_color => text,
    /// 选区底色。
    selection_bg => selection_bg,
    /// 光标颜色。
    caret_color => caret,
    /// 状态栏背景。
    status_bar_bg => status_bar_bg,
    /// 状态栏文字。
    status_bar_text => status_bar_text,
    /// 侧栏背景。
    sidebar_bg => sidebar_bg,
    /// 侧栏与正文之间的分隔线。
    sidebar_border => sidebar_border,
    /// 侧栏"已选中但未打开"的浅色底（与"已打开"的蓝色区分开）。
    sidebar_selected_bg => sidebar_selected_bg,
    /// 危险操作（删除）的文字色。
    danger_text => danger,
    /// 右键菜单 / popup 的面板底色。
    menu_bg => surface,
    /// 行内输入框的表面色（不透明，压住行背景的蓝/浅色）。
    input_bg => input_bg,
    /// 行内输入框边框（中性灰，和行底色分层）。
    input_border => input_border,
    /// 行内输入框的"选中"高亮：比行选中底色明显更深，深色文字压在上面仍有足够对比。
    input_selection => input_selection,
    /// 行内输入框的 caret（深色，在白色底色与深蓝高亮上都清楚）。
    input_caret => input_caret,
    /// 行内代码的底色（浅灰，压在窗口底色上仍可读）。
    code_bg => code_bg,
    /// 链接文字色（与正文区分，再配合下划线）。
    link_color => link,
    /// 任务复选框（未勾选）底色。
    task_box_bg => task_box_bg,
    /// 任务复选框（未勾选）描边色。
    task_box_border => task_box_border,
    /// 任务复选框（已勾选）底色 / 描边色（主题强调色）。
    task_box_checked_bg => task_box_checked_bg,
    /// 任务复选框（已勾选）描边色。
    task_box_checked_border => task_box_checked_border,
    /// 已勾选时的勾形颜色（压在强调色上）。
    task_box_check => task_box_check,
    /// 引用块左侧竖条的颜色（比正文弱，用于"这是个引用块"的视觉锚点）。
    quote_bar_color => quote_border,
    /// 引用块正文颜色（略弱化，字号不变）。
    quote_text_color => quote_text,
    /// 搜索结果里高亮片段的底色（浅黄，和正文区分开）。
    search_highlight_bg => search_highlight_bg,
    /// 高亮片段的文字色（保持与正文一致的深浅，避免"高亮处反而看不清"）。
    search_highlight_text => search_highlight_text,
    /// 索引降级/查询失败的提示色。
    search_warn_text => warning,
    /// 进度条底槽色（浅灰，和面板背景区分）。
    search_progress_track => search_progress_track,
    /// 进度条填充色（低饱和蓝，与选区色同族）。
    search_progress_fill => search_progress_fill,
    /// 开关「打开」态底色。
    search_toggle_active => accent,
    /// 开关「打开」态文字色。
    search_toggle_active_text => on_accent,
    /// 搜索结果里"当前选中分组"的底色（浅蓝，保证正文与统计行都可读）。
    search_group_selected => search_group_selected,
    /// 占位框底色（浅灰，与正文区分但不抢眼）。
    object_placeholder_bg => object_placeholder_bg,
    /// 占位框边框色。
    object_placeholder_border => object_placeholder_border,
    /// 占位文案颜色。
    object_label_color => object_label_color,
    /// 选中态描边色（与光标同族的蓝，明确"选中了整块"）。
    object_selected_border => object_selected_border,
    /// 悬浮工具条底色（不透明，压住图片也看得清）。
    object_toolbar_bg => object_toolbar_bg,
    /// 悬浮工具条边框色。
    object_toolbar_border => object_toolbar_border,
    /// 悬浮工具条按钮文字色。
    object_toolbar_text => object_toolbar_text,
    /// 边缘操作浮层底色（与悬浮工具条一致，压住下方文字也看得清）。
    table_actions_bg => table_actions_bg,
    /// 边缘操作浮层边框色。
    table_actions_border => table_actions_border,
    /// 边缘操作浮层按钮文字色。
    table_actions_text => table_actions_text,
    /// 边缘操作浮层**禁用**按钮的文字色（例如只剩 1 列时的「删除列」）。
    table_actions_text_disabled => table_actions_text_disabled,
    /// 把手（可拖拽）底色：比边框略深，停在表头/行边框上仍然可见。
    table_drag_handle_bg => table_drag_handle_bg,
    /// 把手悬停底色（drag affordance 的高亮）。
    table_drag_handle_hover_bg => table_drag_handle_hover_bg,
    /// 拖动中的插入位置指示线颜色（主题强调色）。
    table_drag_indicator => table_drag_indicator,
    /// 整行/整列选中的填充色：低透明度，压在表格上文字仍然清晰可读。
    table_axis_selection_bg => table_axis_selection_bg,
    /// 整行/整列选中的边框色（强调色）。
    table_axis_selection_border => table_axis_selection_border,
    /// 被选中的行/列把手底色：强调色，与普通灰把手区分（选中反馈的一部分）。
    table_drag_handle_selected_bg => table_drag_handle_selected_bg,
    /// 拖拽 ghost（跟随指针的整行/整列半透明预览）填充色。
    table_drag_ghost_bg => table_drag_ghost_bg,
    /// 拖拽 ghost 的边框色。
    table_drag_ghost_border => table_drag_ghost_border,
    /// ghost 上"移动第 N 列/行"浮层的底色（压在任何表格内容上都可读）。
    table_drag_ghost_label_bg => table_drag_ghost_label_bg,
    /// ghost 浮层的文字色。
    table_drag_ghost_label_text => table_drag_ghost_label_text,
    /// 确认框遮罩色（半透明黑，压住正文但不完全遮挡上下文）。
    link_confirm_overlay => link_confirm_overlay,
    /// 确认框「打开」主按钮底色。
    link_confirm_primary_bg => link_confirm_primary_bg,
    /// 确认框「打开」主按钮文字色。
    link_confirm_primary_text => link_confirm_primary_text,
    /// 链接提示（非阻塞 toast）底色。
    link_notice_bg => link_notice_bg,
    /// 链接提示（非阻塞 toast）文字色。
    link_notice_text => link_notice_text,
    /// 表格外框 / 单元格分隔线颜色。
    table_border => table_border,
    /// 表格底色（不透明，压住正文行背景）。
    table_bg => table_bg,
    /// 表头底色（与正文行区分）。
    table_header_bg => table_header_bg,
    /// 选中单元格的底色：与文本选区同色系，压在表格底色之上、单元格文字之下。
    table_cell_selected_bg => table_cell_selected_bg,
    /// 选中单元格的描边色（2px，压住 1px 网格线）。
    table_cell_selected_border => table_cell_selected_border,
    /// 图谱覆盖层的背景（近不透明的窗口底色：盖住编辑器但仍能透出一点层次）。
    graph_backdrop => graph_backdrop,
    /// 图谱顶部工具条的底色（不透明，压住下面的边线）。
    graph_toolbar_bg => graph_toolbar_bg,
    /// 节点填充色（未悬停）。
    graph_node_bg => graph_node_bg,
    /// 节点描边色（未悬停）。
    graph_node_border => graph_node_border,
    /// 悬停节点/邻居节点的填充色（强调态）。
    graph_node_active_bg => graph_node_active_bg,
    /// 悬停时的节点文字色（深底白字）。
    graph_node_active_text => graph_node_active_text,
    /// 与悬停节点无关的节点被压暗后的填充色。
    graph_node_dim_bg => graph_node_dim_bg,
    /// 与悬停节点无关的节点被压暗后的描边色。
    graph_node_dim_border => graph_node_dim_border,
    /// 节点标签文字色（未悬停）。
    graph_label_text => graph_label_text,
    /// 边线颜色（未高亮）。
    graph_edge => graph_edge,
    /// 悬停高亮时的边线颜色。
    graph_edge_active => graph_edge_active,
    /// 悬停时被压暗的边线颜色（只突出与被悬停节点相连的边）。
    graph_edge_dim => graph_edge_dim,
    /// Mermaid 占位框底色。
    mermaid_placeholder_bg => mermaid_placeholder_bg,
    /// Mermaid 占位文案颜色。
    mermaid_placeholder_text => mermaid_placeholder_text,
    /// Mermaid 渲染失败文字色。
    mermaid_error => mermaid_error,
    /// 图片放大遮罩色。
    image_overlay_bg => image_overlay_bg,
    /// 设置/搜索浮层遮罩。
    overlay_scrim => overlay_scrim,
    /// 设置页浅遮罩。
    overlay_scrim_soft => overlay_scrim_soft,
    /// 代码块语法高亮：关键字。
    syntax_keyword => syntax_keyword,
    /// 代码块语法高亮：字符串。
    syntax_string => syntax_string,
    /// 代码块语法高亮：注释。
    syntax_comment => syntax_comment,
    /// 代码块语法高亮：数值。
    syntax_number => syntax_number,
    /// 代码块语法高亮：类型名。
    syntax_type => syntax_type,
    /// 代码块语法高亮：函数名。
    syntax_function => syntax_function,
    /// 代码块语法高亮：变量名。
    syntax_variable => syntax_variable,
    /// 代码块语法高亮：运算符。
    syntax_operator => syntax_operator,
    /// 代码块语法高亮：标点/分隔符。
    syntax_punctuation => syntax_punctuation,
    /// 代码块语法高亮：属性/注解/键名。
    syntax_attribute => syntax_attribute,
}

/// 语法类别 → 当前主题的颜色（绘制层只调这一个入口）。
///
/// `CodeHighlightClass` 与主题 token 一一对应；新增类别时这里编译期报错，
/// 不会出现“有类别但没颜色”的静默退化。
#[must_use]
pub fn syntax_color(class: CodeHighlightClass) -> Rgba {
    match class {
        CodeHighlightClass::Keyword => syntax_keyword(),
        CodeHighlightClass::String => syntax_string(),
        CodeHighlightClass::Comment => syntax_comment(),
        CodeHighlightClass::Number => syntax_number(),
        CodeHighlightClass::Type => syntax_type(),
        CodeHighlightClass::Function => syntax_function(),
        CodeHighlightClass::Variable => syntax_variable(),
        CodeHighlightClass::Operator => syntax_operator(),
        CodeHighlightClass::Punctuation => syntax_punctuation(),
        CodeHighlightClass::Attribute => syntax_attribute(),
    }
}
