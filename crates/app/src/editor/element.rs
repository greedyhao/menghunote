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
    LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, ScrollEvent,
    ShapedLine, Style, TextAlign, Window,
};
use menghunote_editor::{units, Bias, Selection};

use super::geometry::EditorGeometry;
use super::layout::VisualLine;
use super::shared::EditorShared;
use super::theme;
use super::view::EditorView;

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
}

impl EditorElement {
    /// 由视图（`Render`）构造；`shared` 与视图共享折行缓存 / 上一帧几何。
    #[must_use]
    pub fn new(view: Entity<EditorView>, shared: Rc<EditorShared>) -> Self {
        Self {
            view,
            shared,
            hitbox: None,
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

    /// 画 IME 组合文本（overlay）。
    ///
    /// preedit **不在布局里**（`LayoutFrame` 只描述 source），所以这里按光标位置临时
    /// shaping 一次并画在光标右侧；组合串通常很短，超出行宽时由 content mask 裁掉。
    fn paint_preedit(
        &self,
        geometry: &EditorGeometry,
        display_caret: Option<usize>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let preedit = self.view.read(cx).preedit_text().map(str::to_owned);
        let Some(preedit) = preedit else {
            return;
        };
        // preedit 不进 display_text，但**位置**按 display 坐标算（标记可能被隐藏）。
        let Some(caret) = display_caret else {
            return;
        };
        let Some((row, x)) = geometry.caret_row_x(caret) else {
            return;
        };
        let sanitized = super::view::sanitize_single_line(&preedit);
        if sanitized.is_empty() {
            return;
        }
        let shaped = super::glyph::shape_row_styled_at(
            window,
            &sanitized,
            &[],
            geometry.frame.row_font_size(row),
            geometry.frame.lines[row].mono,
        );
        let top = geometry.row_top(row);
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
        let width = shaped.width();
        if width > px(0.0) {
            window.paint_quad(fill(
                Bounds::new(
                    point(x, top + geometry.line_height - px(1.0)),
                    size(width, px(1.0)),
                ),
                theme::caret_color(),
            ));
        }
    }

    /// 画光标竖线（最后画，保证压在文本与 preedit 之上）。
    fn paint_caret(
        &self,
        geometry: &EditorGeometry,
        display_caret: Option<usize>,
        window: &mut Window,
    ) {
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

    /// 画图片对象（ADR 0019 S2a）：已解码画图片；加载中/缺失/失败画占位框 + alt/状态文案。
    ///
    /// 矩形来自 [`EditorGeometry::objects`]（prepaint 已经按 `LayoutFrame` 的行顶/行高算好），
    /// 因此滚动、命中、选中框与文本几何是**同一套**坐标。
    fn paint_objects(
        &self,
        bounds: Bounds<Pixels>,
        geometry: &EditorGeometry,
        window: &mut Window,
        cx: &mut App,
    ) {
        for object in geometry.objects() {
            let rect = object.rect;
            if let Some(image) = &object.image {
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
                    theme::object_placeholder_bg(),
                    Edges::all(px(1.0)),
                    theme::object_placeholder_border(),
                    BorderStyle::default(),
                ));
                if let Some(label) = &object.label {
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
                // 悬浮工具条上的点击由工具条自己的按钮处理：这里必须放行，否则会同时挪光标。
                let toolbar_hit = down_view
                    .update(cx, |view, _| view.point_in_toolbar(event.position))
                    .unwrap_or(false);
                if toolbar_hit {
                    return;
                }
                // 图片对象：单击只选中块（描边），不展开源码、不产生对象内部 caret。
                if let Some(object) = down_geometry.object_at(event.position) {
                    let line = object.line;
                    down_view
                        .update(cx, |view, cx| view.select_object_at(line, Some(window), cx))
                        .ok();
                    return;
                }
                // 命中带偏置：点字形右半 → 停在隐藏标记之前（构造内），左半 → 之后。
                let (byte, bias) = down_geometry
                    .byte_at_with_bias(event.position)
                    .unwrap_or((down_geometry.byte_at(event.position), Bias::Right));
                down_view
                    .update(cx, |view, cx| {
                        view.begin_pointer_selection(byte, bias, window, cx)
                    })
                    .ok();
            },
        );

        let move_view = self.view.downgrade();
        let move_geometry = Rc::clone(&geometry);
        window.on_mouse_event(
            move |event: &MouseMoveEvent, _phase: DispatchPhase, _: &mut Window, cx: &mut App| {
                // 悬停 → 图片对象工具条（不依赖元素级 hover 命中链）。
                move_view
                    .update(cx, |view, cx| view.hover_at(event.position, cx))
                    .ok();
                if !event.dragging() {
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
            move |_event: &MouseUpEvent, _phase: DispatchPhase, _: &mut Window, cx: &mut App| {
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
        // 图片对象：几何矩形按 LayoutFrame 的行顶/行高算（元素局部坐标，下面再加 origin）。
        let objects_local =
            self.view
                .read(cx)
                .object_paints(window, &frame, scroll, content_width, first..last);
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
                let spans = super::glyph::line_spans(&styles, line.bytes.clone());
                super::glyph::shape_row_styled_at(
                    window,
                    &frame.text[line.bytes.clone()],
                    &spans,
                    line.font_size,
                    line.mono,
                )
            })
            .collect();
        // 局部坐标 → 窗口坐标（元素原点）。
        let objects = objects_local
            .into_iter()
            .map(|mut object| {
                object.rect = Bounds::new(
                    point(
                        object.rect.origin.x + bounds.origin.x,
                        object.rect.origin.y + bounds.origin.y,
                    ),
                    object.rect.size,
                );
                object
            })
            .collect();

        let geometry = Rc::new(EditorGeometry {
            frame,
            shaped,
            visible_start: first,
            scroll_y: scroll,
            origin: bounds.origin,
            line_height,
            viewport_height: bounds.size.height.as_f32(),
            styles,
            objects,
        });
        self.shared.set_geometry(Rc::clone(&geometry));
        // 正文 hitbox：prepaint 阶段插入（与绘制矩形一致），paint 阶段绑定光标样式。
        self.hitbox = Some(window.insert_hitbox(bounds, gpui::HitboxBehavior::Normal));
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
            // 顺序：选区底色 → 正文 → IME 组合串 → 光标（光标永远压在最上面）。
            self.paint_selection(&geometry, selection, display_selection, window, cx);
            self.paint_text(&geometry, window, cx);
            self.paint_objects(bounds, &geometry, window, cx);
            self.paint_preedit(&geometry, display_caret, window, cx);
            self.paint_caret(&geometry, display_caret, window);
        });

        self.register_handlers(bounds, geometry, window);

        // 指针样式：绑定到正文 hitbox，交给 gpui 原生机制。
        //
        // `set_cursor_style` 要求 paint 阶段；之后 gpui 在鼠标命中链变化时自己
        // 调 `reset_cursor_style`（进入本 hitbox → IBeam，离开 → Arrow），
        // 因此不存在"没重绘就漏判"，也不需要我们自己记状态。
        if let Some(hitbox) = &self.hitbox {
            window.set_cursor_style(CursorStyle::IBeam, hitbox);

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
