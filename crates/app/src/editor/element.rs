//! 单个 `Element`：软折行绘制 + 点击命中 + 拖拽选区 + 滚轮滚动。
//!
//! 为什么不用 `StyledText` / `Text` 元素拼：编辑器的命中必须精确到
//! "视觉行内的字形 x 偏移 → 源码字节 → UnicodeScalar"，而现成文本元素只暴露
//! "整段文本的相对偏移"，且一个视觉行要同时参与选区、光标、命中三套几何。
//! 自己实现 `Element` 可以把这三件事绑定在**同一份** [`EditorGeometry`] 上：
//!
//! - `prepaint`：算折行（带缓存）+ shaping 可见行 + 记录几何；
//! - `paint`：画选区底色 → 画正文 → 画光标，然后注册本帧的鼠标监听。
//!
//! 事件监听用 `Window::on_mouse_event` 而不用 `div().on_mouse_down`：前者拿到的
//! 是**窗口坐标**加上本元素自己的 `bounds`，可以做出精确的"行内命中"；
//! 而且监听器每帧重新注册，天然跟着最新的 `bounds` / 几何走（窗口 resize 后
//! 第一帧就换掉，不需要额外的失效逻辑）。

use std::ops::Range;
use std::rc::Rc;

use gpui::{
    fill, point, px, quad, relative, size, transparent_black, App, BorderStyle, Bounds,
    ContentMask, Corners, CursorStyle, DispatchPhase, Edges, Element, ElementId,
    ElementInputHandler, Entity, Focusable as _, GlobalElementId, InspectorElementId, IntoElement,
    LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PathBuilder, Pixels,
    ScrollEvent, ShapedLine, Style, TextAlign, Window,
};
use menghunote_editor::{units, Bias, Selection};

use super::geometry::{
    EditorGeometry, ObjectPaintContent, TableCellPaint, TablePaint, TaskBoxPaint,
};
use super::layout::VisualLine;
use super::shared::EditorShared;
use super::theme;
use super::view::{EditorView, TaskCheckbox};

/// 光标竖线宽度（像素）。
const CARET_WIDTH: f32 = 2.0;
/// 空行被选中时的高亮宽度（像素）：没有宽度就没有"选中了"的视觉反馈。
const EMPTY_LINE_SELECTION_WIDTH: f32 = 6.0;

/// 编辑器画布元素。
pub struct EditorElement {
    view: Entity<EditorView>,
    shared: Rc<EditorShared>,
    /// 正文区的 hitbox：`insert_hitbox` 只能在 prepaint 调用、`set_cursor_style`
    /// 只能在 paint 调用，所以要把 prepaint 拿到的句柄带到 paint。
    ///
    /// 光标样式交给 gpui 原生机制：注册一次之后，命中链变化时
    /// `dispatch_mouse_event` → `reset_cursor_style` 会自动在 IBeam / Arrow 之间切换，
    /// 不需要我们自己判进出、也不依赖"有没有重绘"。
    hitbox: Option<gpui::Hitbox>,
    /// wiki/普通链接的独立 hitbox（悬停时切手形；只覆盖可见 display 内容）。
    link_hitboxes: Vec<gpui::Hitbox>,
    /// 图片对象 hitbox（悬停显示手形/可放大）。
    object_hitboxes: Vec<gpui::Hitbox>,
    /// 任务复选框 hitbox（ADR 0019 S3：悬停手形；点击只切换 source，不落 caret）。
    task_hitboxes: Vec<gpui::Hitbox>,
}

impl EditorElement {
    /// 由视图（`Render`）构造；`shared` 与视图共享折行缓存 / 上一帧几何。
    #[must_use]
    pub fn new(view: Entity<EditorView>, shared: Rc<EditorShared>) -> Self {
        Self {
            view,
            shared,
            hitbox: None,
            link_hitboxes: Vec::new(),
            object_hitboxes: Vec::new(),
            task_hitboxes: Vec::new(),
        }
    }

    /// 画选区底色。
    fn paint_selection(
        &self,
        geometry: &EditorGeometry,
        selection: Selection,
        display_selection: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut App,
    ) {
        // display 坐标的选区直接来自投影表；换算失败就不画（不静默回退 0）。
        let Some(selected) = display_selection else {
            return;
        };
        // 只在被请求的那一帧 dump（`update` 需要可变借用）
        let dump = self.view.update(cx, |view, _| view.take_selection_dump());
        if dump {
            eprintln!(
                "[menghunote][dump] 选区 anchor=#{:?} head=#{:?} → start=#{:?} end=#{:?}；字节 {:?}；文本 {} 字节 / {} scalar；可见行 {}..{}",
                selection.anchor().get(),
                selection.head().get(),
                selection.start().get(),
                selection.end().get(),
                selected,
                geometry.frame.text.len(),
                units::scalar_len(&geometry.frame.text),
                geometry.visible_start,
                geometry.visible_start + geometry.shaped.len().saturating_sub(1)
            );
        }
        for (offset, shaped) in geometry.shaped.iter().enumerate() {
            let row = geometry.visible_start + offset;
            // 对象行由 `paint_objects` 画整块描边/高亮，不在这里画文本选区（占位符不是文字）。
            if geometry.object_row(row).is_some() {
                continue;
            }
            let line = &geometry.frame.lines[row];
            let span = selection_span(line, shaped, selected.clone());
            if dump {
                let inter_start = selected.start.max(line.bytes.start);
                let inter_end = selected.end.min(line.bytes.end);
                let inter = if inter_end > inter_start {
                    inter_start..inter_end
                } else {
                    inter_start..inter_start
                };
                eprintln!(
                    "[menghunote][dump]   row={row} line.bytes={:?} 交集={:?} → span={:?}（left裁={} right裁={}）shape_width={:?} text_left={:?} scale={} elem_w={:?}",
                    line.bytes,
                    inter,
                    span,
                    selected.start <= line.bytes.start,
                    selected.end >= line.bytes.end,
                    shaped.width(),
                    geometry.text_left_for_row(row),
                    window.scale_factor(),
                    window.bounds().size.width
                );
            }
            let Some((x1, x2)) = span else {
                continue;
            };
            let top = geometry.row_top(row);
            // 引用行的选区要从该行缩进后的左边界起算（缩进只在几何里加一次）。
            window.paint_quad(fill(
                Bounds::new(
                    point(geometry.text_left_for_row(row) + x1, top),
                    size(x2 - x1, geometry.row_height(row)),
                ),
                theme::selection_bg(),
            ));
        }
    }

    /// 画 IME 组合文本（overlay），返回本帧是否真的画了 preedit。
    ///
    /// preedit **不在布局里**（`LayoutFrame` 只描述 source），所以这里按光标位置临时
    /// shaping 一次并画在光标右侧；组合串通常很短，超出行宽时由 content mask 裁掉。
    ///
    /// **caret 归属**：preedit 非空时，这一帧**唯一**的 caret 画在组合串末尾
    /// （`x + shaped.width()`），调用方必须据此跳过 [`Self::paint_caret`]；否则组合串
    /// 起点还会再画一条竖线（真机反馈：caret 停在 `biao'p` 开头）。
    ///
    /// `caret_visible` 是闪烁相位：隐藏时不画串尾 caret（preedit 文本与下划线保留）。
    /// 组合期间 `pump` 会强制常亮，所以实际使用中组合时不会闪。
    fn paint_preedit(
        &self,
        geometry: &EditorGeometry,
        display_caret: Option<usize>,
        focused: bool,
        caret_visible: bool,
        window: &mut Window,
        cx: &mut App,
    ) -> bool {
        let preedit = self.view.read(cx).preedit_text().map(str::to_owned);
        let Some(preedit) = preedit else {
            // 没有 preedit：清掉上一帧组合串 caret/底纸的痕迹，走原来的 caret 路径。
            self.record_preedit_caret(cx, None, None);
            return false;
        };
        // preedit 不进 display_text，但**位置**按 display 坐标算（标记可能被隐藏）。
        // 兜底：极少数帧里 source→display 的 head 映射会拿不到（例如选区正跨过
        // 刚被投影隐藏的标记）；此时仍应把组合串画在选区起点，不能静默消失。
        let caret = display_caret.or_else(|| {
            self.view
                .read(cx)
                .selection_in_display_range()
                .map(|range| range.start)
        });
        let Some(caret) = caret else {
            self.record_preedit_caret(cx, None, None);
            return false;
        };
        let Some((row, x)) = geometry.caret_row_x(caret) else {
            self.record_preedit_caret(cx, None, None);
            return false;
        };
        let sanitized = super::view::sanitize_single_line(&preedit);
        if sanitized.is_empty() {
            self.record_preedit_caret(cx, None, None);
            return false;
        }
        self.view.update(cx, |view, _| {
            view.preedit_paint_count += 1;
        });
        // 宽度按**实际绘制用的 sanitized 文本** shaping（反引号/制表符/换行都已处理），
        // 不能拿原始 preedit 的长度或估算宽度来定 caret 位置。
        let shaped = super::glyph::shape_row_styled_at(
            window,
            &sanitized,
            &[],
            geometry.frame.row_font_size(row),
            geometry.frame.lines[row].mono,
        );
        let top = geometry.row_top(row);
        let width = shaped.width();
        // **先擦底纸**：组合串不在布局里，只画在 caret 处；如果不清掉它下面的正文，
        // 光标后面的已有文字会和组合串叠在一起（真机反馈的“输入时重叠”）。
        // 只覆盖组合串实际宽度，后续文字虽然不会右移（布局没变），但不会被压字。
        if width > px(0.0) {
            window.paint_quad(fill(
                Bounds::new(point(x, top), size(width, geometry.row_height(row))),
                theme::window_bg(),
            ));
        }
        // 组合串用主色 + 下划线，和已提交文本区分开。
        if let Err(error) = shaped.paint(
            point(x, top),
            geometry.row_height(row),
            TextAlign::Left,
            None,
            window,
            cx,
        ) {
            eprintln!("[menghunote] 绘制 preedit 失败：{error}");
        }
        if width > px(0.0) {
            window.paint_quad(fill(
                Bounds::new(
                    point(x, top + geometry.line_height - px(1.0)),
                    size(width, px(1.0)),
                ),
                theme::caret_color(),
            ));
        }
        // 组合串末尾画这一帧唯一的 caret：同一视觉行、同一行高、同一颜色。
        // 正文未聚焦或闪烁相位隐藏时不画（与 `paint_caret` 的口径一致）。
        let preedit_caret_x = if focused && caret_visible {
            let caret_x = x + width;
            window.paint_quad(fill(
                Bounds::new(
                    point(caret_x, top),
                    size(px(CARET_WIDTH), geometry.row_height(row)),
                ),
                theme::caret_color(),
            ));
            Some(caret_x)
        } else {
            None
        };
        self.record_preedit_caret(cx, preedit_caret_x, (width > px(0.0)).then_some(width));
        true
    }

    /// 记录本帧组合串末尾 caret 的绘制结果（headless 回归用；不参与业务判断）。
    fn record_preedit_caret(&self, cx: &mut App, x: Option<Pixels>, mask_width: Option<Pixels>) {
        self.view.update(cx, |view, _| {
            view.preedit_caret_painted_last_frame = x.is_some();
            view.preedit_caret_x_last_frame = x;
            view.preedit_mask_width_last_frame = mask_width;
        });
    }

    /// 画光标竖线（最后画，保证压在文本与 preedit 之上）。
    fn paint_caret(
        &self,
        geometry: &EditorGeometry,
        display_caret: Option<usize>,
        focused: bool,
        window: &mut Window,
    ) {
        if !focused {
            return;
        }
        let Some(caret) = display_caret else {
            return;
        };
        if let Some((row, x)) = geometry.caret_row_x(caret) {
            let top = geometry.row_top(row);
            // 标题行的 caret 要和该行行高一致，不能再用统一行高。
            window.paint_quad(fill(
                Bounds::new(
                    point(x, top),
                    size(px(CARET_WIDTH), geometry.row_height(row)),
                ),
                theme::caret_color(),
            ));
        }
    }

    /// 画正文（逐可见视觉行，跳过屏外行）。
    /// 画分隔线：语义行被投影成空行，这里在行高中心画一条横线。
    fn paint_thematic_breaks(
        &self,
        bounds: Bounds<Pixels>,
        geometry: &EditorGeometry,
        window: &mut Window,
        cx: &mut App,
    ) {
        for offset in 0..geometry.shaped.len() {
            let row = geometry.visible_start + offset;
            let Some(line) = geometry.frame.lines.get(row) else {
                continue;
            };
            if !self.view.read(cx).is_thematic_display_row(line.bytes.start) {
                continue;
            }
            let top = geometry.row_top(row);
            let y = top + geometry.row_height(row) * 0.5;
            let left = geometry.text_left_for_row(row);
            let right = bounds.origin.x + bounds.size.width - px(theme::PAD_X);
            if right <= left {
                continue;
            }
            window.paint_quad(fill(
                Bounds::new(point(left, y), size(right - left, px(1.0))),
                theme::sidebar_border(),
            ));
        }
    }

    fn paint_text(&self, geometry: &EditorGeometry, window: &mut Window, cx: &mut App) {
        for (offset, shaped) in geometry.shaped.iter().enumerate() {
            let row = geometry.visible_start + offset;
            // 图片对象行不画字形（display 里只有占位符）：整行由 `paint_objects` 覆盖。
            if geometry.object_row(row).is_some() {
                continue;
            }
            // 引用行有缩进：文字原点用该行自己的左边界（PAD_X 与 indent 只在这里加一次）。
            let origin = point(geometry.text_left_for_row(row), geometry.row_top(row));
            // 引用块左侧竖条：只按 quote depth 画；列表 gutter 不是引用。
            if geometry
                .frame
                .lines
                .get(row)
                .is_some_and(|line| line.quote_bar)
            {
                let bar_x = origin.x - px(theme::QUOTE_BAR_WIDTH + theme::QUOTE_BAR_GAP);
                window.paint_quad(fill(
                    Bounds::new(
                        point(bar_x, geometry.row_top(row)),
                        size(px(theme::QUOTE_BAR_WIDTH), geometry.row_height(row)),
                    ),
                    theme::quote_bar_color(),
                ));
            }
            // 行内代码的底色是 `TextRun::background_color`，必须**单独**刷一遍；
            // `paint` 只画字形与下划线/删除线，不会画 run 背景。
            let row_height = geometry.row_height(row);
            if let Err(error) =
                shaped.paint_background(origin, row_height, TextAlign::Left, None, window, cx)
            {
                eprintln!("[menghunote] 绘制样式底色失败：{error}");
            }
            // 单行绘制失败只影响这一行：记录到 stderr，不 panic、不 unwrap。
            if let Err(error) = shaped.paint(origin, row_height, TextAlign::Left, None, window, cx)
            {
                eprintln!("[menghunote] 绘制视觉行 {row} 失败：{error}");
            }
        }
    }

    /// 画 Object（ADR 0019 S2a 图片 / S3a 表格）。
    ///
    /// 矩形来自 [`EditorGeometry::objects`]（prepaint 已经按 `LayoutFrame` 的行顶/行高
    /// 算好），因此滚动、命中、选中框与文本几何是**同一套**坐标。图片加载中/缺失/失败
    /// 画占位框 + alt/状态文案；表格画只读 GFM 网格（表头底色/边框/单元格文本）。
    fn paint_objects(
        &self,
        bounds: Bounds<Pixels>,
        geometry: &EditorGeometry,
        window: &mut Window,
        cx: &mut App,
    ) {
        for object in geometry.objects() {
            let rect = object.rect;
            match &object.content {
                ObjectPaintContent::Image(paint) | ObjectPaintContent::Mermaid(paint) => {
                    // 图片与 Mermaid 共用几何，但占位框 token 分开（深色主题下
                    // Mermaid 占位需要独立可读的背景/文案色，ADR 0021 §3.2）。
                    let (placeholder_bg, placeholder_border) = match &object.content {
                        ObjectPaintContent::Image(_) => (
                            theme::object_placeholder_bg(),
                            theme::object_placeholder_border(),
                        ),
                        _ => (
                            theme::mermaid_placeholder_bg(),
                            theme::object_placeholder_border(),
                        ),
                    };
                    if let Some(image) = &paint.image {
                        // `bounds` 是视口裁剪框：图片可能被滚出视口，交给 gpui 裁剪。
                        if let Err(error) = window.paint_image(
                            bounds,
                            rect,
                            Corners::default(),
                            std::sync::Arc::clone(image),
                            0,
                            false,
                        ) {
                            eprintln!("[menghunote] 绘制图片失败（降级为空白）：{error}");
                        }
                    } else {
                        window.paint_quad(quad(
                            rect,
                            Corners::default(),
                            placeholder_bg,
                            Edges::all(px(1.0)),
                            placeholder_border,
                            BorderStyle::default(),
                        ));
                        if let Some(label) = &paint.label {
                            let line_height = px(theme::OBJECT_LABEL_FONT_SIZE * 1.5);
                            let origin = point(
                                rect.origin.x + px(theme::OBJECT_LABEL_PAD_X),
                                rect.origin.y + (rect.size.height - line_height) / 2.0,
                            );
                            if let Err(error) =
                                label.paint(origin, line_height, TextAlign::Left, None, window, cx)
                            {
                                eprintln!("[menghunote] 绘制图片占位文案失败：{error}");
                            }
                        }
                    }
                }
                ObjectPaintContent::Table(table) => {
                    Self::paint_table(table, window, cx);
                }
            }
            if object.selected {
                // 选中态 = 整块描边（不展开源码、不显示文本 caret）。
                window.paint_quad(quad(
                    rect,
                    Corners::default(),
                    transparent_black(),
                    Edges::all(px(theme::OBJECT_SELECTED_BORDER)),
                    theme::object_selected_border(),
                    BorderStyle::default(),
                ));
            }
        }
    }

    /// 画只读表格（ADR 0019 S3a）：外框底色 → 表头底色 → 单元格文字 → 网格线。
    ///
    /// `table.rows` 只含与视口相交的行（长表格按行裁剪，滚出视口的部分不绘制）；
    /// 单元格文字已按列宽截断并补省略号，不会溢出到相邻列。
    fn paint_table(table: &TablePaint, window: &mut Window, cx: &mut App) {
        window.paint_quad(fill(table.rect, theme::table_bg()));
        if let Some(header) = &table.header {
            window.paint_quad(fill(header.rect, theme::table_header_bg()));
        }
        // 选中单元格的底色：画在单元格文字**之前**（文字仍然压在底色上，清晰可读）。
        let selected = table
            .selected_cell
            .and_then(|(row, column)| table_cell_paint_at(table, row, column));
        if let Some(cell) = selected {
            window.paint_quad(fill(cell.rect, theme::table_cell_selected_bg()));
        }
        // 单元格文字：对齐方式来自 GFM 分隔行；`align_width` 用单元格内宽，
        // 于是 Center/Right 由 GPUI 的 shaping 排版完成（不做像素估算）。
        for row in table.header.iter().chain(table.rows.iter()) {
            for cell in &row.cells {
                let Some(text) = &cell.text else {
                    continue;
                };
                let inner = Bounds::new(
                    point(
                        cell.rect.origin.x + px(theme::TABLE_CELL_PAD_X),
                        cell.rect.origin.y,
                    ),
                    size(
                        px(
                            (cell.rect.size.width.as_f32() - theme::TABLE_CELL_PAD_X * 2.0)
                                .max(0.0),
                        ),
                        cell.rect.size.height,
                    ),
                );
                let align = match cell.align {
                    menghunote_editor::MarkdownTableAlignment::Center => TextAlign::Center,
                    menghunote_editor::MarkdownTableAlignment::Right => TextAlign::Right,
                    menghunote_editor::MarkdownTableAlignment::None
                    | menghunote_editor::MarkdownTableAlignment::Left => TextAlign::Left,
                };
                if let Err(error) = text.paint(
                    inner.origin,
                    inner.size.height,
                    align,
                    Some(inner.size.width),
                    window,
                    cx,
                ) {
                    eprintln!("[menghunote] 绘制表格单元格失败：{error}");
                }
            }
        }
        // 网格线：列分隔线（每行每列右侧）+ 行底线。边框宽度固定 1px。
        let line = px(theme::TABLE_BORDER_WIDTH);
        for row in table.header.iter().chain(table.rows.iter()) {
            for (index, cell) in row.cells.iter().enumerate() {
                if index == 0 {
                    continue;
                }
                window.paint_quad(fill(
                    Bounds::new(
                        point(cell.rect.origin.x, cell.rect.origin.y),
                        size(line, cell.rect.size.height),
                    ),
                    theme::table_border(),
                ));
            }
            let bottom = row.rect.origin.y + row.rect.size.height - line;
            window.paint_quad(fill(
                Bounds::new(
                    point(row.rect.origin.x, bottom),
                    size(row.rect.size.width, line),
                ),
                theme::table_border(),
            ));
        }
        // 外框最后画：表头底色/网格线不得盖住 1px 边框。
        window.paint_quad(quad(
            table.rect,
            Corners::default(),
            transparent_black(),
            Edges::all(px(theme::TABLE_BORDER_WIDTH)),
            theme::table_border(),
            BorderStyle::default(),
        ));
        // 选中单元格描边压在最上层（网格线不得盖住它）；矩形与命中/编辑共用一份。
        if let Some(cell) = selected {
            window.paint_quad(quad(
                cell.rect,
                Corners::default(),
                transparent_black(),
                Edges::all(px(theme::TABLE_CELL_SELECTED_BORDER)),
                theme::table_cell_selected_border(),
                BorderStyle::default(),
            ));
        }
    }

    /// 画任务复选框（ADR 0019 S3）。
    ///
    /// 矩形来自本帧 shaping 的占位区间（两连 NBSP），方框在矩形内**居中**绘制：
    /// 勾选态是主题强调色底 + 白色勾形，未勾选是白底灰描边。绘制在正文之后、
    /// caret 之前，压在占位空白上；点击命中与指针样式由同一矩形驱动。
    fn paint_task_boxes(&self, geometry: &EditorGeometry, window: &mut Window) {
        for task in geometry.task_boxes() {
            let rect = task.rect;
            let side = (rect.size.height.as_f32() - 8.0)
                .min(rect.size.width.as_f32() - 2.0)
                .clamp(theme::TASK_BOX_MIN_SIZE, theme::TASK_BOX_MAX_SIZE);
            let side = px(side);
            let bounds = Bounds::new(
                point(
                    rect.origin.x + (rect.size.width - side) / 2.0,
                    rect.origin.y + (rect.size.height - side) / 2.0,
                ),
                size(side, side),
            );
            if task.checked {
                window.paint_quad(quad(
                    bounds,
                    Corners::all(px(theme::TASK_BOX_RADIUS)),
                    theme::task_box_checked_bg(),
                    Edges::all(px(1.0)),
                    theme::task_box_checked_border(),
                    BorderStyle::default(),
                ));
                let (x, y, edge) = (
                    bounds.origin.x.as_f32(),
                    bounds.origin.y.as_f32(),
                    bounds.size.width.as_f32(),
                );
                let mut path = PathBuilder::stroke(px(1.5));
                path.move_to(point(px(x + edge * 0.24), px(y + edge * 0.52)));
                path.line_to(point(px(x + edge * 0.42), px(y + edge * 0.72)));
                path.line_to(point(px(x + edge * 0.78), px(y + edge * 0.30)));
                match path.build() {
                    Ok(path) => window.paint_path(path, theme::task_box_check()),
                    Err(error) => eprintln!("[menghunote] 绘制任务勾形失败：{error}"),
                }
            } else {
                window.paint_quad(quad(
                    bounds,
                    Corners::all(px(theme::TASK_BOX_RADIUS)),
                    theme::task_box_bg(),
                    Edges::all(px(1.0)),
                    theme::task_box_border(),
                    BorderStyle::default(),
                ));
            }
        }
    }

    /// 注册本帧的鼠标监听（每帧重建，跟随最新 `bounds` 与几何）。
    fn register_handlers(
        &self,
        bounds: Bounds<Pixels>,
        geometry: Rc<EditorGeometry>,
        window: &mut Window,
    ) {
        let down_view = self.view.downgrade();
        let down_geometry = Rc::clone(&geometry);
        window.on_mouse_event(
            move |event: &MouseDownEvent,
                  phase: DispatchPhase,
                  window: &mut Window,
                  cx: &mut App| {
                if !phase.bubble()
                    || event.button != MouseButton::Left
                    || !bounds.contains(&event.position)
                {
                    return;
                }
                // 鼠标点击是"活动"：闪烁立刻回到可见并重新计时（点击同一位置也一样）。
                down_view
                    .update(cx, |view, _| view.reset_caret_blink())
                    .ok();
                // 外部链接确认框是模态的：点击只由遮罩/按钮处理，绝不落到正文
                // （正常情况下遮罩已经 `stop_propagation`，这里是兜底）。
                if down_view
                    .update(cx, |view, _| view.link_confirm_open())
                    .unwrap_or(false)
                {
                    return;
                }
                // 非阻塞链接提示：任何正文点击都清掉（它只用于一次性反馈）。
                down_view
                    .update(cx, |view, _| view.clear_link_notice())
                    .ok();
                // 悬浮工具条上的点击由工具条自己的按钮处理：这里必须放行，否则会同时挪光标。
                let toolbar_hit = down_view
                    .update(cx, |view, _| view.point_in_toolbar(event.position))
                    .unwrap_or(false);
                // 正文右键菜单（ADR 0019 S3b4）：点菜单外 → 先关菜单，再照常处理这次
                // 左键（落 caret / 选区）。菜单内的点击由菜单自己的按钮处理（上面已放行）。
                let menu_hit = down_view
                    .update(cx, |view, _| view.context_menu_contains(event.position))
                    .unwrap_or(false);
                if !menu_hit
                    && down_view
                        .update(cx, |view, _| view.context_menu_open())
                        .unwrap_or(false)
                {
                    down_view
                        .update(cx, |view, cx| view.close_context_menu(cx))
                        .ok();
                }
                if toolbar_hit {
                    return;
                }
                // 表格行列拖拽（ADR 0019 S3b5）：把手是浮层 div（元素级监听先跑，拖拽已经
                // 开始）；这里再兜一层 —— 拖拽中/按在把手上都不落 caret、不选单元格。
                //
                // 拖拽中又收到一次“按下”说明上一次手势的抬起丢了（例如在窗口外释放）：
                // 直接取消，绝不让脏状态把这次点击吞掉、更不会在别处误重排。
                if down_view
                    .update(cx, |view, cx| view.cancel_table_drag(cx))
                    .unwrap_or(false)
                {
                    return;
                }
                // 单元格原地编辑浮层（ADR 0019 S3b2）：点在浮层**外** → 先提交这次编辑
                // （一次 `EditRequest` = 一个撤销组），随后继续按正常命中处理这次点击
                // （选别的格 / 落正文 caret）。浮层内部点击由输入框自己的监听
                // `stop_propagation` 拦住，不会走到这里。
                //
                // 提交后表格仍是 Object、display 长度不变，所以本帧（上一帧）几何算出的
                // display 字节仍然有效，不需要再做坐标换算。
                if down_view
                    .update(cx, |view, _| view.is_editing_table_cell())
                    .unwrap_or(false)
                {
                    down_view
                        .update(cx, |view, cx| {
                            view.commit_table_cell_edit(None, cx);
                            // 这次点击可能落在正文/别的对象上；选中态先清掉，若命中
                            // 另一个单元格，下面的 `click_table_cell` 会重新设置。
                            view.clear_table_cell_selection(cx);
                        })
                        .ok();
                }
                // 任务复选框（ADR 0019 S3）：只切换 source，不落正文 caret、不开始拖选。
                if let Some((item, source)) = down_geometry
                    .task_box_at(event.position)
                    .map(|task| (task.item, task.source))
                {
                    down_view
                        .update(cx, |view, cx| {
                            view.toggle_task_checkbox(item, source, Some(window), cx)
                        })
                        .ok();
                    return;
                }
                // 表格单元格里的行内链接（ADR 0019 S3b7）：**链接优先于单元格**。
                // 单击只打开链接（与正文普通链接同一套语义：不移动 caret、不进入编辑），
                // 否则 cell 永远先被选中，链接就再也点不开了。
                if let Some(dest) = down_geometry
                    .table_cell_link_at(event.position)
                    .map(|link| link.dest.clone())
                {
                    down_view
                        .update(cx, |view, cx| view.request_external_link(dest, cx))
                        .ok();
                    return;
                }
                // Shift+单击正文文本：**扩选**而不是重新落 caret，更不能打开链接。
                //
                // 必须在链接/对象分支**之前**处理：Shift+单击 wiki/外链是要扩选，
                // 不是跳转。落在对象/表格单元格上的按压保持原行为（不扩选），
                // 因此对象分支放在它之后；任务框在上面已按原行为处理。
                if event.modifiers.shift && down_geometry.object_at(event.position).is_none() {
                    let (byte, bias) = down_geometry
                        .byte_at_with_bias(event.position)
                        .unwrap_or((down_geometry.byte_at(event.position), Bias::Right));
                    down_view
                        .update(cx, |view, cx| {
                            view.begin_shift_pointer_selection(
                                byte,
                                bias,
                                event.position,
                                window,
                                cx,
                            )
                        })
                        .ok();
                    return;
                }
                // 表格单元格（ADR 0019 S3b2）：单击只选中该格（不移动正文 caret）；
                // 再次单击同一格 / 双击才进入就地编辑。点到表格的非单元格区域（内边距/
                // 边框）仍然走 S3a 的整块选中。
                if let Some(object) = down_geometry.object_at(event.position) {
                    let line = object.line;
                    if object.kind.is_table() {
                        if let Some(cell) = down_geometry.table_cell_at(event.position) {
                            let click_count = event.click_count;
                            down_view
                                .update(cx, |view, cx| {
                                    view.click_table_cell(
                                        line,
                                        cell.row,
                                        cell.column,
                                        click_count,
                                        Some(window),
                                        cx,
                                    )
                                })
                                .ok();
                            return;
                        }
                    }
                    down_view
                        .update(cx, |view, cx| view.select_object_at(line, Some(window), cx))
                        .ok();
                    return;
                }
                // 命中带偏置：点字形右半 → 停在隐藏标记之前（构造内），左半 → 之后。
                let (byte, bias) = down_geometry
                    .byte_at_with_bias(event.position)
                    .unwrap_or((down_geometry.byte_at(event.position), Bias::Right));
                // 普通链接（外部 / 内部）：**单击即打开**（Ctrl/Cmd+单击同样打开）。
                // 末尾边界（`display == end`）不算命中，因此点链接末尾仍会正常落 caret。
                let external = down_view
                    .update(cx, |view, _| view.external_link_hit_at_display(byte))
                    .ok()
                    .flatten();
                if let Some(hit) = external {
                    down_view
                        .update(cx, |view, cx| view.request_external_link(hit.dest, cx))
                        .ok();
                    return;
                }
                // wiki 链接：单击打开（不先移动 caret；未解析由宿主提示）。
                let wiki = down_view
                    .update(cx, |view, _| view.wiki_link_at_display(byte))
                    .ok()
                    .flatten();
                if let Some((target, heading)) = wiki {
                    down_view
                        .update(cx, |view, cx| view.request_wiki_link(target, heading, cx))
                        .ok();
                    return;
                }
                down_view
                    .update(cx, |view, cx| {
                        view.begin_pointer_selection(byte, bias, event.position, window, cx)
                    })
                    .ok();
            },
        );

        // 右键：普通链接上是「打开链接 / 复制链接 / 复制 Markdown」，其它位置是
        // 「插入 → 表格」（ADR 0019 S3b4）。右键本身**不移动 caret**；菜单里的动作
        // 使用打开菜单时的 caret/选区。
        let right_view = self.view.downgrade();
        let right_geometry = Rc::clone(&geometry);
        window.on_mouse_event(
            move |event: &MouseDownEvent,
                  phase: DispatchPhase,
                  _window: &mut Window,
                  cx: &mut App| {
                if !phase.bubble()
                    || event.button != MouseButton::Right
                    || !bounds.contains(&event.position)
                {
                    return;
                }
                // 右键点击同样是活动：闪烁回到可见并重新计时。
                right_view
                    .update(cx, |view, _| view.reset_caret_blink())
                    .ok();
                // 确认框是模态的：右键也不得在它下面开菜单。
                if right_view
                    .update(cx, |view, _| view.link_confirm_open())
                    .unwrap_or(false)
                {
                    return;
                }
                let toolbar_hit = right_view
                    .update(cx, |view, _| view.point_in_toolbar(event.position))
                    .unwrap_or(false);
                if toolbar_hit {
                    // 右键落在浮层/菜单本身上：不重开菜单，也不打扰当前菜单。
                    return;
                }
                // 右键落点 → display 字节 → 命中普通链接（没有就退回「插入表格」）。
                let display = right_geometry.byte_at(event.position);
                let link = right_view
                    .update(cx, |view, _| view.external_link_hit_at_display(display))
                    .ok()
                    .flatten();
                right_view
                    .update(cx, |view, cx| {
                        view.open_context_menu_at(event.position, link, cx)
                    })
                    .ok();
            },
        );

        let move_view = self.view.downgrade();
        let move_geometry = Rc::clone(&geometry);
        window.on_mouse_event(
            move |event: &MouseMoveEvent, _phase: DispatchPhase, _: &mut Window, cx: &mut App| {
                // 表格行列拖拽（ADR 0019 S3b5）：拖拽期间只更新插入槽位，**不动正文
                // caret/选区**、也不改悬停目标（把手/工具条必须稳定）。
                if move_view
                    .update(cx, |view, _| view.table_drag_active())
                    .unwrap_or(false)
                {
                    move_view
                        .update(cx, |view, cx| view.update_table_drag(event.position, cx))
                        .ok();
                    return;
                }
                // 悬停 → 图片对象工具条（不依赖元素级 hover 命中链）。
                move_view
                    .update(cx, |view, cx| view.hover_at(event.position, cx))
                    .ok();
                if !event.dragging() {
                    return;
                }
                // 正文文本单击：按下后只有真正拖动（越过点击阈值）才实时扩选；
                // 阈值内的抖动保持原选区，等 mouse-up 再落 caret（不提前显现隐藏语法）。
                if !move_view
                    .update(cx, |view, _| view.note_pointer_moved(event.position))
                    .unwrap_or(true)
                {
                    return;
                }
                // 拖选与点击用同一套偏置，否则跨构造拖动会跳。
                let (byte, bias) = move_geometry
                    .byte_at_with_bias(event.position)
                    .unwrap_or((move_geometry.byte_at(event.position), Bias::Right));
                move_view
                    .update(cx, |view, cx| {
                        view.extend_selection_to_biased(byte, bias, cx)
                    })
                    .ok();
            },
        );

        let up_view = self.view.downgrade();
        window.on_mouse_event(
            move |event: &MouseUpEvent,
                  _phase: DispatchPhase,
                  window: &mut Window,
                  cx: &mut App| {
                // 链接菜单/确认框动作后**吞掉同一次手势的 mouse-up**：动作在
                // mouse-down 上执行，菜单随即关闭；抬起若继续走下面的 `end_drag`
                // 会清掉拖拽锚点（并可能让宿主把这次点击当成正文点击）。
                if up_view
                    .update(cx, |view, _| view.take_swallow_mouse_up())
                    .unwrap_or(false)
                {
                    return;
                }
                // 表格行列拖拽（ADR 0019 S3b5）：释放位置决定插入槽位 → 一次
                // `EditRequest`（原地/表格外则取消，零请求）。必须在 `end_drag`
                // 之前处理：它读的是拖拽状态，不是文本拖选锚点。
                let dragging_table = up_view
                    .update(cx, |view, _| view.table_drag_active())
                    .unwrap_or(false);
                if dragging_table {
                    up_view
                        .update(cx, |view, cx| {
                            view.finish_table_drag(event.position, Some(window), cx)
                        })
                        .ok();
                }
                // 正文文本单击：mouse-down 只记了锚点，抬起且**没有拖动**才真正落
                // caret（Shift+单击在这里扩选）。必须在 `end_drag` 之前（扩选锚点
                // 还在拖拽状态里）；拖动过的手势不会留下待定点击，保持既有选区。
                up_view
                    .update(cx, |view, cx| view.finish_pointer_click(window, cx))
                    .ok();
                up_view.update(cx, |view, _cx| view.end_drag()).ok();
            },
        );

        let scroll_view = self.view.downgrade();
        let scroll_geometry = Rc::clone(&geometry);
        window.on_mouse_event(
            move |event: &ScrollEvent, phase: DispatchPhase, _: &mut Window, cx: &mut App| {
                if !phase.bubble() || !bounds.contains(&event.position) {
                    return;
                }
                // 平台给的是"内容位移"（正 = 滚轮向上）；`scroll_y` 正 = 已向下滚，
                // 所以取反。见 `Window::with_element_offset` 的 `origin += offset` 约定。
                let delta = event
                    .delta
                    .pixel_delta(scroll_geometry.line_height)
                    .y
                    .as_f32();
                if delta == 0.0 {
                    return;
                }
                scroll_view
                    .update(cx, |view, cx| view.scroll_by(-delta, cx))
                    .ok();
            },
        );
    }
}

/// 把任务框的 display 占位区间换算成本帧窗口矩形。
///
/// 与链接 hitbox 共用 `EditorGeometry::caret_row_x`（同一份 shaping），
/// 因此绘制、命中、headless 断言看到的矩形完全一致；占位被折行拆开（跨视觉行）时
/// 返回 `None`（不画半个复选框，也不产生半行点击区）。
fn task_box_paint(
    geometry: &EditorGeometry,
    checkbox: &TaskCheckbox,
    bounds: Bounds<Pixels>,
) -> Option<TaskBoxPaint> {
    let (row, start_x) = geometry.caret_row_x(checkbox.display.0)?;
    let (end_row, end_x) = geometry.caret_row_x(checkbox.display.1)?;
    if row != end_row || end_x <= start_x {
        return None;
    }
    let rect = Bounds::new(
        point(start_x, geometry.row_top(row)),
        size(end_x - start_x, geometry.row_height(row)),
    );
    if !bounds.intersects(&rect) {
        return None;
    }
    Some(TaskBoxPaint {
        item: checkbox.item,
        source: checkbox.source,
        checked: checkbox.checked,
        rect,
    })
}

impl IntoElement for EditorElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for EditorElement {
    type RequestLayoutState = ();
    type PrepaintState = Option<Rc<EditorGeometry>>;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        // 铺满父容器：尺寸由外层 flex 布局决定，折行宽度在 prepaint 里按实际
        // bounds 计算，因此窗口 resize 会自动重排（不需要额外失效逻辑）。
        let mut style = Style::default();
        style.size.width = relative(1.0).into();
        style.size.height = relative(1.0).into();
        (window.request_layout(style, None, cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        // 布局用 **display 文本**（Markdown 标记按当前 selection 隐藏/显现）。
        // `WrapCache` 按文本比对，因此"caret 进入/离开构造导致 display 变长变短"
        // 会自动让缓存失效并重排——不需要再单独记 `display_len`。
        let text = self.view.read(cx).display_text();
        let scroll_y = self.view.read(cx).scroll_y();
        let font = theme::font();
        let font_size = px(theme::FONT_SIZE);
        let line_height = px(theme::DEFAULT_LINE_HEIGHT);
        let wrap_width =
            (bounds.size.width.as_f32() - theme::PAD_X * 2.0).max(super::layout::MIN_WRAP_WIDTH);
        let content_width = wrap_width;
        // 块级度量（标题字号 / 代码块等宽 / 图片对象行高）：连同文本一起进 `WrapCache` 的键。
        let metrics = self.view.read(cx).block_metrics(window, content_width);

        let frame = self.shared.frame(super::shared::WrapRequest {
            text: &text,
            wrap_width,
            default_metric: super::layout::LineMetric::body(),
            metrics: &metrics,
            text_system: cx.text_system(),
            font: &font,
            font_size,
        });
        // 笔记属性面板是正文最上面的一个块：把整篇正文下移面板高度，面板自身由
        // `EditorView::render` 用 `-scroll_y` 平移，于是它随正文一起滚走。
        // 只改这一帧的副本，不污染 `WrapCache`（缓存键仍是纯折行结果）。
        let top_inset = self.view.read(cx).properties_inset(cx);
        let frame = if top_inset > 0.0 {
            let mut shifted = (*frame).clone();
            for top in &mut shifted.row_tops {
                *top += top_inset;
            }
            shifted.content_height += top_inset;
            Rc::new(shifted)
        } else {
            frame
        };
        self.shared
            .set_viewport(bounds.size.width.as_f32(), bounds.size.height.as_f32());

        let row_count = frame.row_count();
        let scroll = scroll_y.clamp(
            0.0,
            (frame.content_height() - bounds.size.height.as_f32()).max(0.0),
        );
        // 行高逐行不同：可见范围按累计行顶算，再多取一行 overscan。
        let first = frame.row_at_y(scroll).min(row_count.saturating_sub(1));
        let viewport_bottom = scroll + bounds.size.height.as_f32() + frame.row_height(first);
        let mut last = first;
        while last < row_count && frame.row_top_offset(last) < viewport_bottom {
            last += 1;
        }
        let last = last.max(first + 1).min(row_count);

        // 语义样式（display 坐标，完整覆盖）：绘制与命中都用它分段 shaping。
        // 注意：样式随 caret 变化（进入/离开构造），所以**每帧**都要按当前 runs 重新
        // shaping；`WrapCache` 只缓存折行（纯文本函数），不会把样式缓存住。
        let styles = Rc::new(self.view.read(cx).style_runs_display());
        // 代码块只读语法高亮：display 坐标快照（syntect 只在 source 变化时重跑；
        // 这里每帧只克隆一个 `Rc`）。颜色只在 shaping 时生效，不影响折行/行高/命中。
        let code_highlight = self
            .view
            .update(cx, |view, _| view.code_highlight_display());
        // 图片对象：几何矩形按 LayoutFrame 的行顶/行高算（元素局部坐标，下面再加 origin）。
        let objects_local = self.view.read(cx).object_paints(
            window,
            &frame,
            scroll,
            content_width,
            bounds.size.height.as_f32(),
            first..last,
        );
        let object_rows: std::collections::HashSet<usize> =
            objects_local.iter().map(|object| object.row).collect();
        let shaped = frame.lines[first..last]
            .iter()
            .enumerate()
            .map(|(offset, line)| {
                // 对象行只含 display 占位符：不 shaping 字形（占位符永不绘制成豆腐块）。
                if object_rows.contains(&(first + offset)) {
                    return super::glyph::shape_row_styled_at(
                        window,
                        "",
                        &[],
                        line.font_size,
                        line.mono,
                    );
                }
                let text = &frame.text[line.bytes.clone()];
                // 代码行（等宽且落在代码块内）：用语法 spans 着色。字体/字号与普通
                // shaping 完全一致（只是 TextRun 颜色不同），行宽与命中几何不变；
                // 未命中 token 的文本仍是正文色。代码块里没有语义样式（粗体/斜体）。
                if line.mono && code_highlight.is_code_range(&line.bytes) {
                    let syntax = code_highlight.spans_for_line(&line.bytes);
                    return super::glyph::shape_row_code(
                        window,
                        text,
                        &syntax,
                        line.font_size,
                        line.mono,
                    );
                }
                let spans = super::glyph::line_spans(&styles, line.bytes.clone());
                super::glyph::shape_row_styled_at(window, text, &spans, line.font_size, line.mono)
            })
            .collect();
        // 局部坐标 → 窗口坐标（元素原点；表格内部的表头/行/单元格矩形一起平移）。
        let objects = objects_local
            .into_iter()
            .map(|mut object| {
                object.translate(bounds.origin);
                object
            })
            .collect();

        let mut geometry = EditorGeometry {
            frame,
            shaped,
            visible_start: first,
            scroll_y: scroll,
            origin: bounds.origin,
            line_height,
            viewport_height: bounds.size.height.as_f32(),
            styles,
            objects,
            task_boxes: Vec::new(),
        };
        // 任务复选框（ADR 0019 S3）：display 上的占位区间 → 本帧窗口矩形。
        // 与链接 hitbox 一样用 `caret_row_x`（同一份 shaping 几何），不按字符宽度估算。
        geometry.task_boxes = self
            .view
            .read(cx)
            .task_checkboxes()
            .into_iter()
            .filter_map(|checkbox| task_box_paint(&geometry, &checkbox, bounds))
            .collect();
        let geometry = Rc::new(geometry);
        self.shared.set_geometry(Rc::clone(&geometry));
        // 正文 hitbox：prepaint 阶段插入（与绘制矩形一致），paint 阶段绑定光标样式。
        self.hitbox = Some(window.insert_hitbox(bounds, gpui::HitboxBehavior::Normal));
        // wiki/普通链接：可见 display 区间各插一个 hitbox；悬停时由 gpui 切 PointingHand。
        self.link_hitboxes.clear();
        let mut link_ranges = self.view.read(cx).wiki_display_ranges();
        for (start, end, target) in self.view.read(cx).external_link_display_ranges() {
            // 只复用 start/end；target/heading 在这里不用。
            link_ranges.push((start, end, target, None));
        }
        for (start, end, _, _) in link_ranges {
            let Some((row, start_x)) = geometry.caret_row_x(start) else {
                continue;
            };
            let Some((end_row, end_x)) = geometry.caret_row_x(end) else {
                continue;
            };
            if row != end_row || end_x <= start_x {
                continue;
            }
            let rect = Bounds::new(
                point(start_x, geometry.row_top(row)),
                size(end_x - start_x, geometry.row_height(row)),
            );
            if !bounds.intersects(&rect) {
                continue;
            }
            self.link_hitboxes
                .push(window.insert_hitbox(rect, gpui::HitboxBehavior::Normal));
        }
        // 表格单元格里的行内链接（ADR 0019 S3b7）：矩形来自单元格 shaping（截断后的
        // 可见前缀），与绘制逐像素一致；悬停同样是手形。命中优先级在鼠标按下时处理
        // （链接先于单元格：点链接要打开，不是进入编辑）。
        for object in geometry.objects() {
            let Some(table) = object.table() else {
                continue;
            };
            for cell in table
                .header
                .iter()
                .chain(table.rows.iter())
                .flat_map(|paint_row| paint_row.cells.iter())
            {
                for link in &cell.links {
                    if !bounds.intersects(&link.rect) {
                        continue;
                    }
                    self.link_hitboxes
                        .push(window.insert_hitbox(link.rect, gpui::HitboxBehavior::Normal));
                }
            }
        }
        self.object_hitboxes.clear();
        for object in geometry.objects() {
            if bounds.intersects(&object.rect) {
                self.object_hitboxes
                    .push(window.insert_hitbox(object.rect, gpui::HitboxBehavior::Normal));
            }
        }
        self.task_hitboxes.clear();
        for task in geometry.task_boxes() {
            self.task_hitboxes
                .push(window.insert_hitbox(task.rect, gpui::HitboxBehavior::Normal));
        }
        Some(geometry)
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        // UI 自动化的"可以开始点击"信号：paint 才是真正往窗口提交绘制。
        // 只在第一次打印（内部有 Once 判断），不污染日志。
        crate::platform::log_first_frame_ready();
        let Some(geometry) = prepaint.clone() else {
            return;
        };
        // 布局坐标系是 **display**（隐藏标记后变短），所以绘制一律用 display 坐标：
        // source 选区/光标先经 `ProjectionMap` 映射（可能跨多个省略段，
        // 不能假设 display 与 source 线性对应）。
        let (selection, display_selection, display_caret) = {
            let view = self.view.read(cx);
            (
                view.selection(),
                view.selection_in_display_range(),
                view.display_caret(),
            )
        };

        // 文字不能画到画布外面（最后一个视觉行可能只有半行高）。
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            // 顺序：选区底色 → 正文 → IME 组合串（含组合串末尾的 caret）→ 光标。
            // 有 preedit 时组合串末尾那条竖线就是本帧唯一 caret，不能再从起点画。
            self.paint_selection(&geometry, selection, display_selection, window, cx);
            self.paint_text(&geometry, window, cx);
            self.paint_thematic_breaks(bounds, &geometry, window, cx);
            self.paint_task_boxes(&geometry, window);
            self.paint_objects(bounds, &geometry, window, cx);
            let body_focused = self.view.read(cx).focus_handle(cx).is_focused(window);
            // 聚焦/失焦：闪烁立刻回到可见并重新计时（只在绘制帧观察，不额外注册监听）。
            self.view
                .update(cx, |view, _| view.note_caret_focus(body_focused));
            let caret_visible = self.view.read(cx).caret_blink_visible();
            let preedit_painted = self.paint_preedit(
                &geometry,
                display_caret,
                body_focused,
                caret_visible,
                window,
                cx,
            );
            if !preedit_painted {
                // preedit 为空/取消/定位失败：保持原 caret 路径（起点竖线）。
                // 闪烁隐藏时整条都不画（下划线/组合串文本不受影响）。
                self.paint_caret(
                    &geometry,
                    display_caret,
                    body_focused && caret_visible,
                    window,
                );
            }
            self.view.update(cx, |view, _| {
                view.caret_painted_last_frame = body_focused && caret_visible;
            });
        });

        self.register_handlers(bounds, geometry, window);

        // 指针样式：绑定到正文 hitbox，交给 gpui 原生机制。
        //
        // `set_cursor_style` 要求 paint 阶段；之后 gpui 在鼠标命中链变化时自己
        // 调 `reset_cursor_style`（进入本 hitbox → IBeam，离开 → Arrow），
        // 因此不存在"没重绘就漏判"，也不需要我们自己记状态。
        // gpui 的 `cursor_style` 从后往前取第一个 hover 的 hitbox；主 hitbox 覆盖
        // 正文，必须在**前**注册，链接的 PointingHand 后注册才能压过 IBeam。
        if let Some(hitbox) = &self.hitbox {
            // 表格行列拖拽中（ADR 0019 S3b6）：指针可能已经离开把手、落在正文上，
            // 这时再显示 I-beam 会误导；换成“抓紧”指针。drag 状态是可选交互提示，
            // 不改变任何命中/caret 行为。
            let cursor = if self.view.read(cx).table_drag_active() {
                CursorStyle::ClosedHand
            } else {
                CursorStyle::IBeam
            };
            window.set_cursor_style(cursor, hitbox);
        }
        for hitbox in &self.link_hitboxes {
            window.set_cursor_style(CursorStyle::PointingHand, hitbox);
        }
        for hitbox in &self.object_hitboxes {
            // 图片默认就是可放大/可点选的；选中后同样保持可点击的指针（平台无 zoom-in 枚举）。
            window.set_cursor_style(CursorStyle::PointingHand, hitbox);
        }
        for hitbox in &self.task_hitboxes {
            // 任务复选框悬停手形；命中矩形的注册顺序在正文 IBeam 之后 → 手形压过 IBeam。
            window.set_cursor_style(CursorStyle::PointingHand, hitbox);
        }
        if let Some(hitbox) = &self.hitbox {
            // 证据（截图抓不到系统光标）：用 gpui 自己的 `is_hovered` 打一行变化日志。
            // 它读的就是 `mouse_hit_test`，与 `reset_cursor_style` 同源。
            #[cfg(debug_assertions)]
            {
                let hovered = hitbox.id.is_hovered(window);
                if self.shared.note_hitbox_hovered(hovered) {
                    eprintln!("[menghunote][debug] 正文 hitbox 悬停 → hovered={hovered}");
                }
            }
        }

        // 整篇一个输入处理器：IME 组合与普通文本输入都发给同一个 Entity。
        // 平台侧（gpui-windows）通过它调用 `EntityInputHandler`，见 `editor::input`。
        let focus_handle = self.view.focus_handle(cx);
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.view.clone()),
            cx,
        );
    }
}

/// 表格里第 `row` 行（0 = 表头）第 `column` 列单元格的绘制数据。
///
/// 长表格按视口裁剪后 `rows` 只含可见行：滚出视口的单元格返回 `None`（不画、不命中）。
fn table_cell_paint_at(table: &TablePaint, row: usize, column: usize) -> Option<&TableCellPaint> {
    table
        .header
        .iter()
        .chain(table.rows.iter())
        .find(|paint_row| paint_row.row == row)
        .and_then(|paint_row| paint_row.cells.get(column))
}

/// 空行是否该画一条"选区窄高亮"。
///
/// 三个条件缺一不可：
///
/// 1. **选区非零宽**（`sel_start < sel_end`）：折叠光标不是选区，它由
///    [`EditorElement::paint_caret`] 单独画，不该有任何底色 —— 这正是
///    "光标停在空行时右侧冒出浅蓝块"的根因（空行 `start == end`，零宽选区同时满足旧的覆盖判定）。
/// 2. 该视觉行**确实是空行**（`line_start == line_end`）。
/// 3. 选区**覆盖**它，按半开区间判定：`sel_start <= line_start && sel_end > line_start`。
fn empty_line_highlight(
    sel_start: usize,
    sel_end: usize,
    line_start: usize,
    line_end: usize,
) -> bool {
    sel_start < sel_end && line_start == line_end && sel_start <= line_start && sel_end > line_start
}

/// 一条视觉行上被选区覆盖的横向区间（相对该行文字起点，即 `geometry.text_left_for_row(row)`）。
fn selection_span(
    line: &VisualLine,
    shaped: &ShapedLine,
    selected: Range<usize>,
) -> Option<(Pixels, Pixels)> {
    selection_span_with(
        line.bytes.start,
        line.bytes.end,
        selected,
        shaped.width(),
        |index| shaped.x_for_index(index),
    )
}

/// 计算一条视觉行上要画的选区区间（**纯函数**：横向位置由 `x_for_index` 注入）。
///
/// 返回值是**行内相对像素**（0 = 该行文字起点）。绘制方再加 `geometry.text_left_for_row(row)`：
/// 早期版本在这里就把 `PAD_X` 加进返回值、绘制处又加了一次，导致每条选区带整体
/// 右移 14px（真机截图里"末行铺到右边界"就是这个）。
///
/// 返回 `None` 表示**不画任何选区底色**。参数一律是 **display 字节**（布局坐标系）。
fn selection_span_with(
    line_start: usize,
    line_end: usize,
    selected: Range<usize>,
    width: Pixels,
    x_for_index: impl Fn(usize) -> Pixels,
) -> Option<(Pixels, Pixels)> {
    let (sel_start, sel_end) = (
        selected.start.min(selected.end),
        selected.start.max(selected.end),
    );

    // 折叠光标：不画任何选区底色（否则空行上会多出一块"光标后面的阴影"）。
    if sel_start >= sel_end {
        return None;
    }

    // 空行没有字形宽度，是否画完全由覆盖判定决定；显式分流，不依赖 width 恰好为 0。
    if line_start == line_end {
        return empty_line_highlight(sel_start, sel_end, line_start, line_end)
            .then_some((px(0.0), px(EMPTY_LINE_SELECTION_WIDTH)));
    }

    if sel_end <= line_start || sel_start >= line_end {
        return None;
    }

    let rel_start = sel_start.clamp(line_start, line_end) - line_start;
    let rel_end = sel_end.clamp(line_start, line_end) - line_start;
    let x1 = if sel_start <= line_start {
        px(0.0)
    } else {
        x_for_index(rel_start)
    };
    let x2 = if sel_end >= line_end {
        width
    } else {
        x_for_index(rel_end)
    };
    (x2 > x1).then_some((x1, x2))
}

#[cfg(test)]
mod tests {
    use super::empty_line_highlight;

    /// 空行的字节区间恒为 `p..p`（这里取 p = 5）。
    const EMPTY_LINE_AT: usize = 5;

    #[test]
    fn collapsed_caret_on_an_empty_line_draws_nothing() {
        // 光标停在空行：start == end == 5（旧实现会画浅蓝块）
        assert!(!empty_line_highlight(5, 5, EMPTY_LINE_AT, EMPTY_LINE_AT));
        assert!(!empty_line_highlight(0, 0, 0, 0));
        assert!(!empty_line_highlight(99, 99, 99, 99));
    }

    #[test]
    fn selection_covering_an_empty_line_draws_the_narrow_highlight() {
        assert!(empty_line_highlight(3, 9, EMPTY_LINE_AT, EMPTY_LINE_AT));
        assert!(empty_line_highlight(5, 6, EMPTY_LINE_AT, EMPTY_LINE_AT));
    }

    #[test]
    fn selection_only_touching_the_empty_line_boundary_draws_nothing() {
        // 选区停在空行之前：[2, 5) 不包含位置 5
        assert!(!empty_line_highlight(2, 5, EMPTY_LINE_AT, EMPTY_LINE_AT));
        assert!(!empty_line_highlight(6, 7, EMPTY_LINE_AT, EMPTY_LINE_AT));
        assert!(!empty_line_highlight(
            EMPTY_LINE_AT,
            EMPTY_LINE_AT,
            EMPTY_LINE_AT,
            EMPTY_LINE_AT
        ));
    }

    #[test]
    fn non_empty_lines_are_never_treated_as_empty_line_highlights() {
        assert!(!empty_line_highlight(0, 10, 3, 7));
        assert!(!empty_line_highlight(3, 7, 3, 7));
        assert!(!empty_line_highlight(3, 8, 3, 7));
    }

    /// 注入用的假形状：第 i 个字节在行内 `i * 10px` 处，整行宽 300px。
    fn fake_span(
        line_start: usize,
        line_end: usize,
        sel: std::ops::Range<usize>,
    ) -> Option<(gpui::Pixels, gpui::Pixels)> {
        super::selection_span_with(line_start, line_end, sel, gpui::px(300.0), |index| {
            gpui::px(index as f32 * 10.0)
        })
    }

    #[test]
    fn collapsed_caret_on_an_empty_line_paints_no_selection_block() {
        assert_eq!(fake_span(5, 5, 5..5), None);
        assert_eq!(fake_span(0, 10, 5..5), None);
        assert_eq!(fake_span(0, 10, 0..0), None);
    }

    #[test]
    fn selection_covering_an_empty_line_paints_the_narrow_block_at_the_text_origin() {
        // 行内坐标：从 0 开始，宽 6px（绘制处再加 text_left）
        assert_eq!(
            fake_span(5, 5, 3..9),
            Some((gpui::px(0.0), gpui::px(super::EMPTY_LINE_SELECTION_WIDTH)))
        );
    }

    #[test]
    fn selection_touching_only_the_empty_line_boundary_paints_nothing() {
        assert_eq!(fake_span(5, 5, 2..5), None);
        assert_eq!(fake_span(5, 5, 6..7), None);
    }

    #[test]
    fn normal_lines_use_the_real_shaped_positions() {
        // 行 0..10、选中 2..5：x1 = 20，x2 = 50（行内坐标）
        assert_eq!(
            fake_span(0, 10, 2..5),
            Some((gpui::px(20.0), gpui::px(50.0)))
        );
        // 整行被选中：从 0 到整行宽度（绘制处加 text_left，不再重复加 PAD_X）
        assert_eq!(
            fake_span(0, 10, 0..10),
            Some((gpui::px(0.0), gpui::px(300.0)))
        );
        // 选区完全在行之前：不画
        assert_eq!(fake_span(10, 20, 0..5), None);
    }

    /// 真机 bug 的回归用例（task-19）：同一逻辑行被软折行切成 3 段，
    /// 选区从**中段中间**到**末段中间** —— 首段不画、中段半开、末段半闭。
    #[test]
    fn soft_wrapped_selection_highlights_only_the_covered_part_of_each_row() {
        // 三段：0..30、30..60、60..90；选区 42..75（中段中间 → 末段中间）
        assert_eq!(fake_span(0, 30, 42..75), None, "首段完全在选区之外");
        // 中段：从选区起点（42 → 行内 12 → 120px）到行尾（300px）
        assert_eq!(
            fake_span(30, 60, 42..75),
            Some((gpui::px(120.0), gpui::px(300.0))),
            "中段向右开口"
        );
        // 末段：从行首（0）到选区终点（75 → 行内 15 → 150px）
        assert_eq!(
            fake_span(60, 90, 42..75),
            Some((gpui::px(0.0), gpui::px(150.0))),
            "末段向左开口"
        );
    }
}
