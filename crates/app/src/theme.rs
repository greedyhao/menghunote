//! 应用级主题（ADR 0021 P1/P2）：语义 token 集 + 浅色/深色/跟随系统。
//!
//! ## 为什么是"应用级"而不是"编辑器级"
//!
//! 主题是**整个窗口**的偏好：侧栏、状态栏、编辑器、表格、图片遮罩、图谱、Mermaid
//! 占位框必须共用同一套语义 token，否则同一语义在不同模块会出现不同颜色。ADR 0021
//! 明确拒绝"每篇笔记单独主题"（`theme_id` 不写进正文/同步），本模块也不碰
//! vault/schema/wire。
//!
//! ## 模型
//!
//! - [`ThemeMode`]：用户选择的模式（浅色 / 深色 / 跟随系统）；
//! - [`ThemeId`]：**解析后**的主题 id（内置 `light` / `dark`；自定义主题包后置）；
//! - [`Theme`]：语义 token 集。组件只允许读 token，不允许写 `0x...` 颜色；
//! - 全局 [`current_theme()`]：绘制函数（含迁移期的 `editor::theme::*` 兼容包装）
//!   统一从这里取当前主题，避免改 600+ 调用点。
//!
//! ## 系统外观
//!
//! `ThemeMode::System` 用 GPUI 的 [`WindowAppearance`] 解析；根视图（`AppShell` /
//! `AppRoot`）订阅一次 `observe_window_appearance`，OS 切换时更新全局主题并
//! `refresh_windows()` 重绘。启动时没有窗口，回退**浅色**（[`WindowAppearance::Light`]）。
//!
//! ## 持久化
//!
//! 主题偏好是**本机应用设置**：只存 `mode`（+ 可选 `theme_id`），不同步、不写正文、
//! 不写同步 schema。文件是平台配置目录下的 `theme.json`（见 [`settings_path`]）；
//! 损坏/未知版本/未知模式一律回退 [`ThemeMode::System`]（解析为浅色），**不阻塞启动**。
//!
//! ## 默认值
//!
//! 首次启动（没有偏好文件）默认 **`System`**：跟随操作系统外观；OS 外观不可用时
//! 回退浅色。这样浅色用户看到的仍是现有外观（逐色保持），深色用户不会被强制闪白。
//!
//! ## 与 Mermaid/图谱的关系
//!
//! Mermaid 预览缓存 key 里带真实 `theme_id`（见 [`crate::editor::mermaid_preview`]），
//! 切主题后旧 key 自然失效并重新渲染一次；SVG 内容本身的暗色配色属于 M3，本切片
//! 只保证容器/占位/工具条从 token 取色。图谱所有颜色也来自 token。
//!
//! ## 已知限制
//!
//! - 自定义主题包（manifest/校验/加载）后置到 P5；本模块的 [`ThemeId`] 目前只认
//!   内置 `light` / `dark`；
//! - 字体族/字号/行高仍是排版常量（ADR 0021 §3.2：首版不进入主题）；
//! - 真实 OS 外观切换必须真机验证（headless 测试只能验证解析与状态机）。

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock, RwLock, RwLockReadGuard, RwLockWriteGuard};

use gpui::{rgb, rgba, App, Rgba, WindowAppearance};

/// 偏好文件当前版本。
pub const THEME_PREFERENCES_VERSION: u32 = 1;

/// 偏好文件名（平台配置目录下）。
pub const THEME_FILE_NAME: &str = "theme.json";

/// 测试/便携用的路径覆盖环境变量。
pub const THEME_PATH_ENV: &str = "MENGHUNOTE_THEME_PATH";

// ---------------------------------------------------------------------------
// 模式与 id
// ---------------------------------------------------------------------------

/// 用户在设置页选择的主题模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThemeMode {
    /// 固定浅色。
    Light,
    /// 固定深色。
    Dark,
    /// 跟随操作系统外观（默认）。
    #[default]
    System,
}

impl ThemeMode {
    /// 持久化/日志用的稳定字符串（**不是** i18n 文案）。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Light => "light",
            Self::Dark => "dark",
            Self::System => "system",
        }
    }

    /// 解析持久化字符串；未知值返回 `None`（调用方回退默认模式）。
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "light" => Some(Self::Light),
            "dark" => Some(Self::Dark),
            "system" => Some(Self::System),
            _ => None,
        }
    }

    /// 把模式解析成**实际主题 id**。
    ///
    /// `System` 只认深色外观为 `Dark`（含 macOS 的 `VibrantDark`）；其它一律浅色，
    /// 包括启动时拿不到窗口外观的 `Light` 回退。
    #[must_use]
    pub const fn resolve(self, appearance: WindowAppearance) -> ThemeId {
        match self {
            Self::Light => ThemeId::LIGHT,
            Self::Dark => ThemeId::DARK,
            Self::System => match appearance {
                WindowAppearance::Dark | WindowAppearance::VibrantDark => ThemeId::DARK,
                WindowAppearance::Light | WindowAppearance::VibrantLight => ThemeId::LIGHT,
            },
        }
    }
}

impl std::fmt::Display for ThemeMode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// 解析后的主题 id。
///
/// 首版只有内置 `light` / `dark`；自定义主题包（P5）会扩展成带 `String` 的
/// manifest id，但**必须**走同一套 token 校验。这里用 `&'static str` 让
/// [`crate::editor::mermaid_preview::MermaidCacheKey`] 保持 `Copy`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ThemeId(&'static str);

impl ThemeId {
    /// 内置浅色（= 现有外观，逐色保持）。
    pub const LIGHT: Self = Self("light");
    /// 内置深色。
    pub const DARK: Self = Self("dark");

    /// 稳定字符串（进 Mermaid 缓存 key / 日志 / 偏好文件）。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        self.0
    }

    /// 解析持久化字符串；未知 id 返回 `None`（调用方回退内置浅色）。
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "light" => Some(Self::LIGHT),
            "dark" => Some(Self::DARK),
            _ => None,
        }
    }
}

impl Default for ThemeId {
    fn default() -> Self {
        Self::LIGHT
    }
}

impl std::fmt::Display for ThemeId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.0)
    }
}

// ---------------------------------------------------------------------------
// 语义 token 集
// ---------------------------------------------------------------------------

/// 语义 token 集。
///
/// 字段名就是 token 名；组件只允许读这里，不允许在绘制代码里写 `0x...`。
/// 浅色主题的每个值都与 ADR 0021 迁移前的硬编码颜色**逐色一致**（避免视觉回归）；
/// 深色主题是新增值。字体/圆角/间距**不**进入主题（ADR 0021 §3.2）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Theme {
    /// 窗口底色。
    pub window_bg: Rgba,
    /// 面板/菜单/输入框/表格/工具条的表面色。
    pub surface: Rgba,
    /// 弱化表面（状态栏/侧栏/表头）。
    pub surface_muted: Rgba,
    /// 正文文字。
    pub text: Rgba,
    /// 弱化文字（状态栏/占位文案/引用正文）。
    pub text_muted: Rgba,
    /// 禁用文字（禁用按钮）。
    pub text_disabled: Rgba,
    /// 分隔线/边框（装饰性）。
    pub border: Rgba,
    /// 强调色（选区边框/链接/任务勾选/焦点环）。
    pub accent: Rgba,
    /// 压在强调色上的文字/勾形。
    pub on_accent: Rgba,
    /// 文本选区底色。
    pub selection_bg: Rgba,
    /// 光标颜色。
    pub caret: Rgba,
    /// 悬停底色（侧栏选中/工具条按钮 hover）。
    pub hover_bg: Rgba,
    /// 焦点环（与 accent 同值，语义独立）。
    pub focus_ring: Rgba,
    /// 危险操作文字。
    pub danger: Rgba,
    /// 警告文字。
    pub warning: Rgba,
    /// 成功状态（预留：插件/未来状态条）。
    pub success: Rgba,
    /// 信息状态（预留）。
    pub info: Rgba,
    /// 编辑器底色（与窗口同色，语义独立）。
    pub editor_bg: Rgba,
    /// 编辑器正文（与 text 同色，语义独立）。
    pub editor_text: Rgba,
    /// 行内/围栏代码底色。
    pub code_bg: Rgba,
    /// 引用块左侧竖条。
    pub quote_border: Rgba,
    /// 引用块正文。
    pub quote_text: Rgba,
    /// 链接文字。
    pub link: Rgba,
    /// 状态栏底色。
    pub status_bar_bg: Rgba,
    /// 状态栏文字。
    pub status_bar_text: Rgba,
    /// 侧栏底色。
    pub sidebar_bg: Rgba,
    /// 侧栏分隔线。
    pub sidebar_border: Rgba,
    /// 侧栏"已选中但未打开"的底色。
    pub sidebar_selected_bg: Rgba,
    /// 行内输入框底色。
    pub input_bg: Rgba,
    /// 行内输入框边框。
    pub input_border: Rgba,
    /// 行内输入框选区。
    pub input_selection: Rgba,
    /// 行内输入框光标。
    pub input_caret: Rgba,
    /// 搜索命中片段底色。
    pub search_highlight_bg: Rgba,
    /// 搜索命中片段文字。
    pub search_highlight_text: Rgba,
    /// 搜索进度条底槽。
    pub search_progress_track: Rgba,
    /// 搜索进度条填充。
    pub search_progress_fill: Rgba,
    /// 搜索结果"当前分组"底色。
    pub search_group_selected: Rgba,
    /// 任务复选框（未勾选）底色。
    pub task_box_bg: Rgba,
    /// 任务复选框（未勾选）描边。
    pub task_box_border: Rgba,
    /// 任务复选框（已勾选）底色。
    pub task_box_checked_bg: Rgba,
    /// 任务复选框（已勾选）描边。
    pub task_box_checked_border: Rgba,
    /// 任务复选框勾形。
    pub task_box_check: Rgba,
    /// 图片/表格/Mermaid 占位框底色。
    pub object_placeholder_bg: Rgba,
    /// 占位框边框。
    pub object_placeholder_border: Rgba,
    /// 占位文案。
    pub object_label_color: Rgba,
    /// 整块选中描边。
    pub object_selected_border: Rgba,
    /// 悬浮工具条底色。
    pub object_toolbar_bg: Rgba,
    /// 悬浮工具条边框。
    pub object_toolbar_border: Rgba,
    /// 悬浮工具条文字。
    pub object_toolbar_text: Rgba,
    /// 表格外框/网格线。
    pub table_border: Rgba,
    /// 表格底色。
    pub table_bg: Rgba,
    /// 表头底色。
    pub table_header_bg: Rgba,
    /// 选中单元格底色。
    pub table_cell_selected_bg: Rgba,
    /// 选中单元格描边。
    pub table_cell_selected_border: Rgba,
    /// 表格右下角浮层底色。
    pub table_actions_bg: Rgba,
    /// 表格浮层边框。
    pub table_actions_border: Rgba,
    /// 表格浮层文字。
    pub table_actions_text: Rgba,
    /// 表格浮层禁用文字。
    pub table_actions_text_disabled: Rgba,
    /// 行列拖拽把手底色。
    pub table_drag_handle_bg: Rgba,
    /// 把手悬停底色。
    pub table_drag_handle_hover_bg: Rgba,
    /// 拖动插入线。
    pub table_drag_indicator: Rgba,
    /// 整行/整列选中填充（半透明）。
    pub table_axis_selection_bg: Rgba,
    /// 整行/整列选中边框。
    pub table_axis_selection_border: Rgba,
    /// 被选中行/列把手底色。
    pub table_drag_handle_selected_bg: Rgba,
    /// 拖拽 ghost 填充（半透明）。
    pub table_drag_ghost_bg: Rgba,
    /// 拖拽 ghost 边框。
    pub table_drag_ghost_border: Rgba,
    /// ghost 上"第 N → 第 M"浮层底色。
    pub table_drag_ghost_label_bg: Rgba,
    /// ghost 浮层文字。
    pub table_drag_ghost_label_text: Rgba,
    /// 外部链接确认框遮罩。
    pub link_confirm_overlay: Rgba,
    /// 确认框主按钮底色。
    pub link_confirm_primary_bg: Rgba,
    /// 确认框主按钮文字。
    pub link_confirm_primary_text: Rgba,
    /// 链接提示 toast 底色。
    pub link_notice_bg: Rgba,
    /// 链接提示 toast 文字。
    pub link_notice_text: Rgba,
    /// 图谱覆盖层背景（近不透明）。
    pub graph_backdrop: Rgba,
    /// 图谱工具条底色。
    pub graph_toolbar_bg: Rgba,
    /// 节点填充（未悬停）。
    pub graph_node_bg: Rgba,
    /// 节点描边（未悬停）。
    pub graph_node_border: Rgba,
    /// 悬停/邻居节点填充。
    pub graph_node_active_bg: Rgba,
    /// 悬停节点文字。
    pub graph_node_active_text: Rgba,
    /// 被压暗节点填充。
    pub graph_node_dim_bg: Rgba,
    /// 被压暗节点描边。
    pub graph_node_dim_border: Rgba,
    /// 节点标签文字。
    pub graph_label_text: Rgba,
    /// 边线（未高亮）。
    pub graph_edge: Rgba,
    /// 高亮边线。
    pub graph_edge_active: Rgba,
    /// 被压暗边线。
    pub graph_edge_dim: Rgba,
    /// Mermaid 占位框底色。
    pub mermaid_placeholder_bg: Rgba,
    /// Mermaid 占位文案。
    pub mermaid_placeholder_text: Rgba,
    /// Mermaid 渲染失败文字。
    pub mermaid_error: Rgba,
    /// 图片放大遮罩。
    pub image_overlay_bg: Rgba,
    /// 设置/搜索浮层遮罩。
    pub overlay_scrim: Rgba,
    /// 设置页浅遮罩（与搜索浮层区分）。
    pub overlay_scrim_soft: Rgba,
}

macro_rules! theme_token_names {
    ($($field:ident),* $(,)?) => {
        /// 全部 token 名（token 完整性测试的**唯一**清单）。
        pub const TOKEN_NAMES: &[&str] = &[$(stringify!($field)),*];

        impl Theme {
            /// 按名字取 token；未知名字返回 `None`。
            ///
            /// 只给测试/插件只读投影用；绘制代码应直接用字段（编译期检查）。
            #[must_use]
            pub fn token(&self, name: &str) -> Option<Rgba> {
                match name {
                    $(stringify!($field) => Some(self.$field),)*
                    _ => None,
                }
            }
        }
    };
}

theme_token_names!(
    window_bg,
    surface,
    surface_muted,
    text,
    text_muted,
    text_disabled,
    border,
    accent,
    on_accent,
    selection_bg,
    caret,
    hover_bg,
    focus_ring,
    danger,
    warning,
    success,
    info,
    editor_bg,
    editor_text,
    code_bg,
    quote_border,
    quote_text,
    link,
    status_bar_bg,
    status_bar_text,
    sidebar_bg,
    sidebar_border,
    sidebar_selected_bg,
    input_bg,
    input_border,
    input_selection,
    input_caret,
    search_highlight_bg,
    search_highlight_text,
    search_progress_track,
    search_progress_fill,
    search_group_selected,
    task_box_bg,
    task_box_border,
    task_box_checked_bg,
    task_box_checked_border,
    task_box_check,
    object_placeholder_bg,
    object_placeholder_border,
    object_label_color,
    object_selected_border,
    object_toolbar_bg,
    object_toolbar_border,
    object_toolbar_text,
    table_border,
    table_bg,
    table_header_bg,
    table_cell_selected_bg,
    table_cell_selected_border,
    table_actions_bg,
    table_actions_border,
    table_actions_text,
    table_actions_text_disabled,
    table_drag_handle_bg,
    table_drag_handle_hover_bg,
    table_drag_indicator,
    table_axis_selection_bg,
    table_axis_selection_border,
    table_drag_handle_selected_bg,
    table_drag_ghost_bg,
    table_drag_ghost_border,
    table_drag_ghost_label_bg,
    table_drag_ghost_label_text,
    link_confirm_overlay,
    link_confirm_primary_bg,
    link_confirm_primary_text,
    link_notice_bg,
    link_notice_text,
    graph_backdrop,
    graph_toolbar_bg,
    graph_node_bg,
    graph_node_border,
    graph_node_active_bg,
    graph_node_active_text,
    graph_node_dim_bg,
    graph_node_dim_border,
    graph_label_text,
    graph_edge,
    graph_edge_active,
    graph_edge_dim,
    mermaid_placeholder_bg,
    mermaid_placeholder_text,
    mermaid_error,
    image_overlay_bg,
    overlay_scrim,
    overlay_scrim_soft,
);

/// 内置浅色主题：**逐色等于** ADR 0021 迁移前的硬编码颜色。
static LIGHT_THEME: OnceLock<Theme> = OnceLock::new();
/// 内置深色主题。
static DARK_THEME: OnceLock<Theme> = OnceLock::new();

/// 内置浅色主题。
#[must_use]
pub fn light_theme() -> &'static Theme {
    LIGHT_THEME.get_or_init(|| Theme {
        window_bg: rgb(0xffffff),
        surface: rgb(0xffffff),
        surface_muted: rgb(0xf6f8fa),
        text: rgb(0x1f2328),
        text_muted: rgb(0x59636e),
        text_disabled: rgb(0x8c959f),
        border: rgb(0xd0d7de),
        accent: rgb(0x0969da),
        on_accent: rgb(0xffffff),
        selection_bg: rgb(0xcfe4fd),
        caret: rgb(0x0969da),
        hover_bg: rgb(0xeaeef6),
        focus_ring: rgb(0x0969da),
        danger: rgb(0xcf222e),
        warning: rgb(0xb35900),
        success: rgb(0x1a7f37),
        info: rgb(0x0969da),
        editor_bg: rgb(0xffffff),
        editor_text: rgb(0x1f2328),
        code_bg: rgb(0xeff1f3),
        quote_border: rgb(0xc4c9d0),
        quote_text: rgb(0x57606a),
        link: rgb(0x0969da),
        status_bar_bg: rgb(0xf6f8fa),
        status_bar_text: rgb(0x59636e),
        sidebar_bg: rgb(0xf6f8fa),
        sidebar_border: rgb(0xd0d7de),
        sidebar_selected_bg: rgb(0xeaeef6),
        input_bg: rgb(0xffffff),
        input_border: rgb(0x57606a),
        input_selection: rgb(0x7aa7e8),
        input_caret: rgb(0x1f2328),
        search_highlight_bg: rgb(0xfff1b8),
        search_highlight_text: rgb(0x24292f),
        search_progress_track: rgb(0xe6e9ed),
        search_progress_fill: rgb(0x7aa7e8),
        search_group_selected: rgb(0xe8f0fb),
        task_box_bg: rgb(0xffffff),
        task_box_border: rgb(0x8c959f),
        task_box_checked_bg: rgb(0x0969da),
        task_box_checked_border: rgb(0x0969da),
        task_box_check: rgb(0xffffff),
        object_placeholder_bg: rgb(0xf3f4f6),
        object_placeholder_border: rgb(0xd0d7de),
        object_label_color: rgb(0x59636e),
        object_selected_border: rgb(0x0969da),
        object_toolbar_bg: rgb(0xffffff),
        object_toolbar_border: rgb(0xd0d7de),
        object_toolbar_text: rgb(0x1f2328),
        table_border: rgb(0xd0d7de),
        table_bg: rgb(0xffffff),
        table_header_bg: rgb(0xf6f8fa),
        table_cell_selected_bg: rgb(0xdbeafe),
        table_cell_selected_border: rgb(0x0969da),
        table_actions_bg: rgb(0xffffff),
        table_actions_border: rgb(0xd0d7de),
        table_actions_text: rgb(0x1f2328),
        table_actions_text_disabled: rgb(0x8c959f),
        table_drag_handle_bg: rgb(0x8c959f),
        table_drag_handle_hover_bg: rgb(0x57606a),
        table_drag_indicator: rgb(0x0969da),
        table_axis_selection_bg: rgba(0x0969da1f),
        table_axis_selection_border: rgb(0x0969da),
        table_drag_handle_selected_bg: rgb(0x0969da),
        table_drag_ghost_bg: rgba(0x0969da33),
        table_drag_ghost_border: rgb(0x0969da),
        table_drag_ghost_label_bg: rgba(0x1f2328e6),
        table_drag_ghost_label_text: rgb(0xffffff),
        link_confirm_overlay: rgba(0x00000033),
        link_confirm_primary_bg: rgb(0x0969da),
        link_confirm_primary_text: rgb(0xffffff),
        link_notice_bg: rgba(0x1f2328e6),
        link_notice_text: rgb(0xffffff),
        graph_backdrop: rgba(0xf6f8faf7),
        graph_toolbar_bg: rgb(0xffffff),
        graph_node_bg: rgb(0xdbeafe),
        graph_node_border: rgb(0x0969da),
        graph_node_active_bg: rgb(0x0969da),
        graph_node_active_text: rgb(0xffffff),
        graph_node_dim_bg: rgb(0xf0f3f6),
        graph_node_dim_border: rgb(0xd0d7de),
        graph_label_text: rgb(0x1f2328),
        graph_edge: rgb(0xc3ccd6),
        graph_edge_active: rgb(0x0969da),
        graph_edge_dim: rgb(0xe4e8ec),
        mermaid_placeholder_bg: rgb(0xf3f4f6),
        mermaid_placeholder_text: rgb(0x59636e),
        mermaid_error: rgb(0xcf222e),
        image_overlay_bg: rgba(0x000000cc),
        overlay_scrim: rgba(0x00000026),
        overlay_scrim_soft: rgba(0x0000001a),
    })
}

/// 内置深色主题。
#[must_use]
pub fn dark_theme() -> &'static Theme {
    DARK_THEME.get_or_init(|| Theme {
        window_bg: rgb(0x0d1117),
        surface: rgb(0x161b22),
        surface_muted: rgb(0x0d1117),
        text: rgb(0xe6edf3),
        text_muted: rgb(0x8b949e),
        text_disabled: rgb(0x6e7681),
        border: rgb(0x6e7681),
        accent: rgb(0x58a6ff),
        on_accent: rgb(0x0d1117),
        selection_bg: rgb(0x264f78),
        caret: rgb(0x58a6ff),
        hover_bg: rgb(0x21262d),
        focus_ring: rgb(0x58a6ff),
        danger: rgb(0xff7b72),
        warning: rgb(0xd29922),
        success: rgb(0x3fb950),
        info: rgb(0x58a6ff),
        editor_bg: rgb(0x0d1117),
        editor_text: rgb(0xe6edf3),
        code_bg: rgb(0x161b22),
        quote_border: rgb(0x6e7681),
        quote_text: rgb(0x8b949e),
        link: rgb(0x58a6ff),
        status_bar_bg: rgb(0x0d1117),
        status_bar_text: rgb(0x8b949e),
        sidebar_bg: rgb(0x0d1117),
        sidebar_border: rgb(0x6e7681),
        sidebar_selected_bg: rgb(0x21262d),
        input_bg: rgb(0x161b22),
        input_border: rgb(0x8b949e),
        input_selection: rgb(0x1a5fb4),
        input_caret: rgb(0xe6edf3),
        search_highlight_bg: rgb(0x5c4b00),
        search_highlight_text: rgb(0xfff8c5),
        search_progress_track: rgb(0x21262d),
        search_progress_fill: rgb(0x1f6feb),
        search_group_selected: rgb(0x1c2b3a),
        task_box_bg: rgb(0x161b22),
        task_box_border: rgb(0x6e7681),
        task_box_checked_bg: rgb(0x58a6ff),
        task_box_checked_border: rgb(0x58a6ff),
        task_box_check: rgb(0x0d1117),
        object_placeholder_bg: rgb(0x1c2128),
        object_placeholder_border: rgb(0x6e7681),
        object_label_color: rgb(0x8b949e),
        object_selected_border: rgb(0x58a6ff),
        object_toolbar_bg: rgb(0x161b22),
        object_toolbar_border: rgb(0x6e7681),
        object_toolbar_text: rgb(0xe6edf3),
        table_border: rgb(0x6e7681),
        table_bg: rgb(0x161b22),
        table_header_bg: rgb(0x1c2128),
        table_cell_selected_bg: rgb(0x1f3a5f),
        table_cell_selected_border: rgb(0x58a6ff),
        table_actions_bg: rgb(0x161b22),
        table_actions_border: rgb(0x6e7681),
        table_actions_text: rgb(0xe6edf3),
        table_actions_text_disabled: rgb(0x6e7681),
        table_drag_handle_bg: rgb(0x6e7681),
        table_drag_handle_hover_bg: rgb(0x8b949e),
        table_drag_indicator: rgb(0x58a6ff),
        table_axis_selection_bg: rgba(0x58a6ff2e),
        table_axis_selection_border: rgb(0x58a6ff),
        table_drag_handle_selected_bg: rgb(0x58a6ff),
        table_drag_ghost_bg: rgba(0x58a6ff40),
        table_drag_ghost_border: rgb(0x58a6ff),
        table_drag_ghost_label_bg: rgba(0xe6edf3e6),
        table_drag_ghost_label_text: rgb(0x0d1117),
        link_confirm_overlay: rgba(0x00000066),
        link_confirm_primary_bg: rgb(0x58a6ff),
        link_confirm_primary_text: rgb(0x0d1117),
        link_notice_bg: rgba(0xe6edf3e6),
        link_notice_text: rgb(0x0d1117),
        graph_backdrop: rgba(0x0d1117f7),
        graph_toolbar_bg: rgb(0x161b22),
        graph_node_bg: rgb(0x1f3a5f),
        graph_node_border: rgb(0x58a6ff),
        graph_node_active_bg: rgb(0x58a6ff),
        graph_node_active_text: rgb(0x0d1117),
        graph_node_dim_bg: rgb(0x161b22),
        graph_node_dim_border: rgb(0x30363d),
        graph_label_text: rgb(0xe6edf3),
        graph_edge: rgb(0x6e7681),
        graph_edge_active: rgb(0x58a6ff),
        graph_edge_dim: rgb(0x30363d),
        mermaid_placeholder_bg: rgb(0x1c2128),
        mermaid_placeholder_text: rgb(0x8b949e),
        mermaid_error: rgb(0xff7b72),
        image_overlay_bg: rgba(0x000000cc),
        overlay_scrim: rgba(0x00000066),
        overlay_scrim_soft: rgba(0x00000044),
    })
}

/// 按 id 取内置主题。
#[must_use]
pub fn theme_for(id: ThemeId) -> &'static Theme {
    // `ThemeId` 是 `&'static str` 的 newtype，`&str` 不能穷尽匹配；未知 id 一律
    // 回退内置浅色（ADR 0021 §3.1：找不到/损坏的主题回退浅色）。
    match id.as_str() {
        "dark" => dark_theme(),
        _ => light_theme(),
    }
}

// ---------------------------------------------------------------------------
// 全局当前主题
// ---------------------------------------------------------------------------

/// 进程级主题状态。
///
/// `RwLock` 只护几个 `Copy` 字段，不跨渲染/IO 持锁；绘制路径每帧读多次也够快
/// （读锁 + 一次 `Copy`）。测试用 [`lock_for_test`] 串行化并自动复位。
struct ThemeState {
    /// 用户选择的模式。
    mode: ThemeMode,
    /// 解析后的主题 id。
    resolved: ThemeId,
    /// 偏好文件路径（`None` = 不持久化；测试/未初始化）。
    store_path: Option<PathBuf>,
}

static STATE: RwLock<ThemeState> = RwLock::new(ThemeState {
    mode: ThemeMode::System,
    resolved: ThemeId::LIGHT,
    store_path: None,
});

fn read_state() -> RwLockReadGuard<'static, ThemeState> {
    STATE.read().unwrap_or_else(|error| error.into_inner())
}

fn write_state() -> RwLockWriteGuard<'static, ThemeState> {
    STATE.write().unwrap_or_else(|error| error.into_inner())
}

/// 当前解析后的主题（绘制函数的唯一取色入口）。
#[must_use]
pub fn current_theme() -> &'static Theme {
    theme_for(theme_id())
}

/// 当前解析后的主题 id（Mermaid 缓存 key / 日志用）。
#[must_use]
pub fn theme_id() -> ThemeId {
    read_state().resolved
}

/// 当前用户选择的模式。
#[must_use]
pub fn mode() -> ThemeMode {
    read_state().mode
}

/// 设置模式并立即生效（持久化 + 所有窗口重绘）。
///
/// `appearance` 是**当前窗口**的 OS 外观：`System` 模式需要它解析。切换不重建
/// 编辑器、不写正文、不产生 `EditRequest`——主题只是全局 `Copy` 状态 + 重绘。
///
/// 返回 `true` 表示解析结果真的变了（调用方不必再自己判断）。
pub fn set_mode(mode: ThemeMode, appearance: WindowAppearance, cx: &mut App) -> bool {
    let resolved = mode.resolve(appearance);
    let changed = {
        let mut state = write_state();
        let changed = state.mode != mode || state.resolved != resolved;
        state.mode = mode;
        state.resolved = resolved;
        changed
    };
    if changed {
        persist(ThemePreferences {
            mode,
            theme_id: None,
        });
        cx.refresh_windows();
    }
    changed
}

/// 系统外观变化时调用（只对 `System` 模式生效）。
///
/// 返回 `true` 表示解析结果变了（已请求重绘）。
pub fn apply_system_appearance(appearance: WindowAppearance, cx: &mut App) -> bool {
    let changed = {
        let mut state = write_state();
        if state.mode != ThemeMode::System {
            false
        } else {
            let resolved = ThemeMode::System.resolve(appearance);
            if state.resolved == resolved {
                false
            } else {
                state.resolved = resolved;
                true
            }
        }
    };
    if changed {
        cx.refresh_windows();
    }
    changed
}

fn persist(preferences: ThemePreferences) {
    let path = read_state().store_path.clone();
    let Some(path) = path else {
        return;
    };
    if let Err(error) = save_preferences(&path, preferences) {
        // 写失败只影响下次启动；当前会话仍然生效（不阻塞、不 panic）。
        eprintln!("[menghunote] 保存主题偏好失败：{error}");
    }
}

// ---------------------------------------------------------------------------
// 持久化
// ---------------------------------------------------------------------------

/// 本机主题偏好（只含 `mode` + 可选 `theme_id`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ThemePreferences {
    /// 用户选择的模式。
    pub mode: ThemeMode,
    /// 自定义主题包 id（首版恒为 `None`；内置 light/dark 由 `mode` 决定）。
    pub theme_id: Option<ThemeId>,
}

impl ThemePreferences {
    /// 从 JSON 文本解析；损坏/未知版本/未知模式一律回退默认值。
    ///
    /// 手写解析（不引入 `serde_json` 到默认构建）：格式只有两个字段，容忍未知
    /// 字段与空白，**不**执行任何代码。
    #[must_use]
    pub fn parse(text: &str) -> Self {
        let Some(version) = json_number_field(text, "version") else {
            return Self::default();
        };
        if version != THEME_PREFERENCES_VERSION {
            return Self::default();
        }
        let mode = json_string_field(text, "mode")
            .and_then(|value| ThemeMode::parse(&value))
            .unwrap_or_default();
        let theme_id = json_string_field(text, "theme_id").and_then(|value| ThemeId::parse(&value));
        Self { mode, theme_id }
    }

    /// 序列化为稳定 JSON（写入前先落临时文件再 rename）。
    #[must_use]
    pub fn to_json(self) -> String {
        let theme_id = self
            .theme_id
            .map_or_else(|| "null".to_owned(), |id| format!("\"{}\"", id.as_str()));
        format!(
            "{{\n  \"version\": {THEME_PREFERENCES_VERSION},\n  \"mode\": \"{}\",\n  \"theme_id\": {theme_id}\n}}\n",
            self.mode.as_str()
        )
    }
}

/// 平台配置目录下的主题偏好文件路径；解析不到（没有 HOME/LOCALAPPDATA）时返回 `None`。
///
/// 1. `MENGHUNOTE_THEME_PATH`（测试/便携显式覆盖）；
/// 2. Windows：`%LOCALAPPDATA%\menghunote\theme.json`；
/// 3. macOS：`~/Library/Application Support/menghunote/theme.json`；
/// 4. 其它（Linux）：`$XDG_CONFIG_HOME/menghunote/theme.json`，缺省
///    `~/.config/menghunote/theme.json`。
#[must_use]
pub fn settings_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(THEME_PATH_ENV) {
        return Some(PathBuf::from(path));
    }
    #[cfg(target_os = "windows")]
    {
        std::env::var_os("LOCALAPPDATA")
            .map(|base| PathBuf::from(base).join("menghunote").join(THEME_FILE_NAME))
    }
    #[cfg(target_os = "macos")]
    {
        std::env::var_os("HOME").map(|home| {
            PathBuf::from(home)
                .join("Library/Application Support/menghunote")
                .join(THEME_FILE_NAME)
        })
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
        Some(base.join("menghunote").join(THEME_FILE_NAME))
    }
}

/// 读取偏好；文件不存在/不可读/损坏都回退默认值（**不**报错、不阻塞启动）。
#[must_use]
pub fn load_preferences(path: &Path) -> ThemePreferences {
    let Ok(text) = std::fs::read_to_string(path) else {
        return ThemePreferences::default();
    };
    ThemePreferences::parse(&text)
}

/// 原子写入偏好：同目录临时文件 → rename；失败时旧文件原样保留。
pub fn save_preferences(path: &Path, preferences: ThemePreferences) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = preferences.to_json();
    let temp = path.with_extension("json.tmp");
    std::fs::write(&temp, text.as_bytes())?;
    match std::fs::rename(&temp, path) {
        Ok(()) => Ok(()),
        Err(_) => {
            // Windows 上目标存在时 rename 可能失败：退化为直接覆盖写。
            let result = std::fs::write(path, text.as_bytes());
            let _ = std::fs::remove_file(&temp);
            result
        }
    }
}

/// 启动时从平台配置目录加载偏好（生产入口在 `main.rs` 调一次）。
///
/// 没有偏好文件时保持默认 `System`；解析结果在**没有窗口**时按浅色回退，
/// 根视图第一次渲染后由 `observe_window_appearance` 校正。
pub fn init_from_disk() {
    let Some(path) = settings_path() else {
        return;
    };
    init_from_path(path);
}

/// 从**显式路径**加载偏好（测试/便携入口；生产走 [`init_from_disk`]）。
///
/// 文件缺失/损坏回退默认 `System`（无窗口时解析为浅色），并把该路径记为后续
/// [`set_mode`] 的持久化目标。
#[doc(hidden)]
pub fn init_from_path(path: PathBuf) {
    let preferences = load_preferences(&path);
    let mut state = write_state();
    state.mode = preferences.mode;
    state.resolved = preferences.mode.resolve(WindowAppearance::Light);
    state.store_path = Some(path);
}

// ---------------------------------------------------------------------------
// 测试钩子
// ---------------------------------------------------------------------------

/// 进程级主题测试锁：主题是全局状态，改它的测试必须串行。
static TEST_LOCK: Mutex<()> = Mutex::new(());

/// 主题测试守卫：持有期间串行，`Drop` 时自动复位到默认（System/Light、无持久化）。
pub struct ThemeTestGuard {
    _lock: MutexGuard<'static, ()>,
}

impl ThemeTestGuard {
    /// 把全局主题指向一个确定的测试状态（`None` = 不持久化）。
    pub fn reset(&self, store_path: Option<PathBuf>) {
        let mut state = write_state();
        state.mode = ThemeMode::default();
        state.resolved = ThemeId::LIGHT;
        state.store_path = store_path;
    }
}

impl Drop for ThemeTestGuard {
    fn drop(&mut self) {
        let mut state = write_state();
        state.mode = ThemeMode::default();
        state.resolved = ThemeId::LIGHT;
        state.store_path = None;
    }
}

/// 串行化主题测试并自动复位（只给 `crates/app/tests/**` 用）。
#[doc(hidden)]
pub fn lock_for_test() -> ThemeTestGuard {
    ThemeTestGuard {
        _lock: TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner()),
    }
}

// ---------------------------------------------------------------------------
// 手写 JSON 解析（只支持本模块写出的扁平对象）
// ---------------------------------------------------------------------------

/// 取 `"key": "value"` 的字符串值；找不到/不是字符串返回 `None`。
fn json_string_field(text: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let start = text.find(&needle)? + needle.len();
    let rest = &text[start..];
    let rest = rest.trim_start().strip_prefix(':')?;
    let rest = rest.trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_owned())
}

/// 取 `"key": 123` 的数字值；找不到/不是数字返回 `None`。
fn json_number_field(text: &str, key: &str) -> Option<u32> {
    let needle = format!("\"{key}\"");
    let start = text.find(&needle)? + needle.len();
    let rest = &text[start..];
    let rest = rest.trim_start().strip_prefix(':')?;
    let rest = rest.trim_start();
    let end = rest
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// WCAG 2.x 相对亮度（sRGB）。
    fn relative_luminance(color: Rgba) -> f32 {
        fn channel(value: f32) -> f32 {
            if value <= 0.04045 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        }
        0.2126 * channel(color.r) + 0.7152 * channel(color.g) + 0.0722 * channel(color.b)
    }

    /// 把半透明前景合成到不透明背景上。
    fn composite(foreground: Rgba, background: Rgba) -> Rgba {
        background.blend(foreground)
    }

    /// WCAG 对比度（1.0–21.0）。
    fn contrast_ratio(foreground: Rgba, background: Rgba) -> f32 {
        let foreground = composite(foreground, background);
        let (lighter, darker) = {
            let a = relative_luminance(foreground);
            let b = relative_luminance(background);
            if a >= b {
                (a, b)
            } else {
                (b, a)
            }
        };
        (lighter + 0.05) / (darker + 0.05)
    }

    fn assert_contrast(id: ThemeId, foreground: &str, background: &str, minimum: f32) {
        let theme = theme_for(id);
        let foreground_color = theme.token(foreground).expect("前景 token 必须存在");
        let background_color = theme.token(background).expect("背景 token 必须存在");
        let ratio = contrast_ratio(foreground_color, background_color);
        assert!(
            ratio >= minimum,
            "{} 的 {foreground}/{background} 对比度 {ratio:.2} < {minimum:.1}",
            id.as_str()
        );
    }

    #[test]
    fn token_names_are_unique_and_resolve() {
        let mut seen = std::collections::BTreeSet::new();
        for name in TOKEN_NAMES {
            assert!(seen.insert(*name), "token 名重复：{name}");
        }
        for theme in [light_theme(), dark_theme()] {
            for name in TOKEN_NAMES {
                assert!(theme.token(name).is_some(), "主题缺少必需 token：{name}");
            }
            assert!(theme.token("no-such-token").is_none());
        }
        // ADR 0021 §3.2 的类别必须全部有 token（防止未来删字段时静默丢类别）。
        for required in [
            "window_bg",
            "surface",
            "surface_muted",
            "text",
            "text_muted",
            "border",
            "accent",
            "on_accent",
            "selection_bg",
            "caret",
            "hover_bg",
            "focus_ring",
            "danger",
            "warning",
            "success",
            "info",
            "editor_bg",
            "editor_text",
            "code_bg",
            "quote_border",
            "link",
            "table_border",
            "table_header_bg",
            "table_cell_selected_bg",
            "graph_backdrop",
            "graph_node_bg",
            "graph_edge",
            "mermaid_placeholder_bg",
            "mermaid_placeholder_text",
            "mermaid_error",
            "image_overlay_bg",
        ] {
            assert!(TOKEN_NAMES.contains(&required), "缺少语义类别：{required}");
        }
    }

    #[test]
    fn light_theme_keeps_the_legacy_palette() {
        let light = light_theme();
        // 迁移前的硬编码值（ADR 0021 §1）：抽查每个语义组的代表色。
        assert_eq!(light.window_bg, rgb(0xffffff));
        assert_eq!(light.text, rgb(0x1f2328));
        assert_eq!(light.text_muted, rgb(0x59636e));
        assert_eq!(light.border, rgb(0xd0d7de));
        assert_eq!(light.accent, rgb(0x0969da));
        assert_eq!(light.selection_bg, rgb(0xcfe4fd));
        assert_eq!(light.code_bg, rgb(0xeff1f3));
        assert_eq!(light.quote_border, rgb(0xc4c9d0));
        assert_eq!(light.table_header_bg, rgb(0xf6f8fa));
        assert_eq!(light.table_cell_selected_bg, rgb(0xdbeafe));
        assert_eq!(light.graph_backdrop, rgba(0xf6f8faf7));
        assert_eq!(light.graph_edge, rgb(0xc3ccd6));
        assert_eq!(light.image_overlay_bg, rgba(0x000000cc));
    }

    #[test]
    fn text_tokens_meet_wcag_aa() {
        for theme in [ThemeId::LIGHT, ThemeId::DARK] {
            for background in ["window_bg", "surface", "surface_muted"] {
                assert_contrast(theme, "text", background, 4.5);
                assert_contrast(theme, "text_muted", background, 4.5);
            }
            assert_contrast(theme, "editor_text", "editor_bg", 4.5);
            assert_contrast(theme, "quote_text", "window_bg", 4.5);
            assert_contrast(theme, "link", "window_bg", 4.5);
            assert_contrast(theme, "danger", "surface", 4.5);
            assert_contrast(theme, "warning", "surface", 4.5);
            assert_contrast(theme, "success", "surface", 4.5);
            assert_contrast(theme, "info", "surface", 4.5);
            assert_contrast(theme, "on_accent", "accent", 4.5);
            assert_contrast(theme, "search_highlight_text", "search_highlight_bg", 4.5);
            assert_contrast(
                theme,
                "table_drag_ghost_label_text",
                "table_drag_ghost_label_bg",
                4.5,
            );
            assert_contrast(theme, "graph_label_text", "graph_backdrop", 4.5);
            assert_contrast(theme, "graph_node_active_text", "graph_node_active_bg", 4.5);
            assert_contrast(
                theme,
                "mermaid_placeholder_text",
                "mermaid_placeholder_bg",
                4.5,
            );
            assert_contrast(theme, "mermaid_error", "mermaid_placeholder_bg", 4.5);
            assert_contrast(theme, "object_label_color", "object_placeholder_bg", 4.5);
            // 选区上的正文：选中不能把文字吃掉。
            assert_contrast(theme, "text", "selection_bg", 4.5);
            assert_contrast(theme, "input_caret", "input_selection", 4.5);
            assert_contrast(theme, "text", "table_cell_selected_bg", 4.5);
        }
    }

    #[test]
    fn ui_boundaries_meet_wcag_non_text_contrast() {
        for theme in [ThemeId::LIGHT, ThemeId::DARK] {
            // 输入框/按钮等**交互控件**边界：AA 非文本 3:1。
            assert_contrast(theme, "input_border", "surface", 3.0);
            assert_contrast(theme, "accent", "surface", 3.0);
            assert_contrast(theme, "table_cell_selected_border", "table_bg", 3.0);
            assert_contrast(theme, "table_axis_selection_border", "table_bg", 3.0);
            assert_contrast(theme, "task_box_border", "task_box_bg", 3.0);
        }
        // 浅色的分隔线/图谱边线是**装饰性**的既有值（0xd0d7de/白 ≈ 1.45:1、
        // 0xc3ccd6/浅灰 ≈ 1.53:1），ADR 0021 要求浅色逐色保持，因此只设回归下限；
        // 深色的分隔线/边线必须达到 3:1。
        assert_contrast(ThemeId::LIGHT, "border", "surface", 1.4);
        assert_contrast(ThemeId::LIGHT, "graph_edge", "graph_backdrop", 1.5);
        assert_contrast(ThemeId::DARK, "border", "surface", 3.0);
        assert_contrast(ThemeId::DARK, "graph_edge", "graph_backdrop", 3.0);
        assert_contrast(ThemeId::DARK, "graph_edge_dim", "graph_backdrop", 1.4);
    }

    #[test]
    fn mode_round_trips_and_resolves_system() {
        for mode in [ThemeMode::Light, ThemeMode::Dark, ThemeMode::System] {
            assert_eq!(ThemeMode::parse(mode.as_str()), Some(mode));
        }
        assert_eq!(ThemeMode::parse("LIGHT"), None);
        assert_eq!(ThemeMode::parse(""), None);
        assert_eq!(ThemeMode::default(), ThemeMode::System);

        assert_eq!(
            ThemeMode::Light.resolve(WindowAppearance::Dark),
            ThemeId::LIGHT
        );
        assert_eq!(
            ThemeMode::Dark.resolve(WindowAppearance::Light),
            ThemeId::DARK
        );
        assert_eq!(
            ThemeMode::System.resolve(WindowAppearance::Dark),
            ThemeId::DARK
        );
        assert_eq!(
            ThemeMode::System.resolve(WindowAppearance::VibrantDark),
            ThemeId::DARK
        );
        assert_eq!(
            ThemeMode::System.resolve(WindowAppearance::Light),
            ThemeId::LIGHT
        );
        assert_eq!(
            ThemeMode::System.resolve(WindowAppearance::VibrantLight),
            ThemeId::LIGHT
        );
    }

    #[test]
    fn preferences_round_trip_and_corrupt_fallback() {
        for mode in [ThemeMode::Light, ThemeMode::Dark, ThemeMode::System] {
            let preferences = ThemePreferences {
                mode,
                theme_id: None,
            };
            let text = preferences.to_json();
            assert_eq!(ThemePreferences::parse(&text), preferences);
        }
        // 未知模式 / 未知版本 / 非 JSON / 空串都回退默认（System）。
        for corrupt in [
            "",
            "not json",
            "{\"version\": 1}",
            "{\"version\": 1, \"mode\": \"neon\"}",
            "{\"version\": 99, \"mode\": \"dark\"}",
            "{\"version\": \"1\", \"mode\": \"dark\"}",
        ] {
            assert_eq!(
                ThemePreferences::parse(corrupt),
                ThemePreferences::default(),
                "损坏输入必须回退默认：{corrupt:?}"
            );
        }
        // 未知 theme_id 被忽略（内置模式不依赖 id）。
        assert_eq!(
            ThemePreferences::parse("{\"version\": 1, \"mode\": \"dark\", \"theme_id\": \"neon\"}"),
            ThemePreferences {
                mode: ThemeMode::Dark,
                theme_id: None,
            }
        );
    }

    #[test]
    fn preferences_file_round_trip_and_corrupt_file_fallback() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/tmp")
            .join(format!("theme-preferences-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("创建 target/tmp 失败");
        let path = root.join(THEME_FILE_NAME);

        assert_eq!(load_preferences(&path), ThemePreferences::default());
        let preferences = ThemePreferences {
            mode: ThemeMode::Dark,
            theme_id: None,
        };
        save_preferences(&path, preferences).expect("写偏好失败");
        assert_eq!(load_preferences(&path), preferences);
        let raw = std::fs::read_to_string(&path).expect("读回偏好失败");
        assert!(raw.contains("\"mode\": \"dark\""));
        assert!(raw.contains("\"version\": 1"));

        std::fs::write(&path, b"{ broken").expect("写坏文件失败");
        assert_eq!(load_preferences(&path), ThemePreferences::default());
        let _ = std::fs::remove_dir_all(&root);
    }
}
