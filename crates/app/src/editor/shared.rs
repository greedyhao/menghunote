//! 视图与自绘元素之间共享的**非观察状态**。
//!
//! 折行结果和"上一帧几何"只影响绘制与命中，不影响 `Entity` 的语义状态。
//! 如果把它们放进 `EditorView` 再 `cx.notify()`，就会形成
//! 「绘制 → 通知 → 再绘制」的重绘环；而如果每帧重算，则 5000 字的折行每帧都要
//! 走一遍 `TextSystem`。因此这里用 `Rc<RefCell<..>>`：
//!
//! - **写入**：只在 `prepaint`（绘制阶段）里发生；
//! - **读取**：绘制逻辑与事件回调；
//! - **不触发**任何 GPUI 通知。

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use gpui::{px, Bounds, Font, IndentAdjustment, LineFragment, Pixels, Point, TextSystem};

use super::geometry::EditorGeometry;
use super::layout::{self, LayoutFrame, LineMetric};

/// 一次折行请求（参数较多，收成结构体免得调用点一排位置参数看错顺序）。
pub struct WrapRequest<'a> {
    /// 布局文本（display）。
    pub text: &'a str,
    /// 折行宽度（像素）。
    pub wrap_width: f32,
    /// 缺省（正文）度量。
    pub default_metric: LineMetric,
    /// 逐源码行度量（块级语义给出）。
    pub metrics: &'a [LineMetric],
    /// 字体系统。
    pub text_system: &'a Arc<TextSystem>,
    /// 主字体。
    pub font: &'a Font,
    /// 正文字号。
    pub font_size: Pixels,
}

/// 折行缓存：文本 / 折行宽度 / **逐行度量** 都不变时直接复用上一帧结果。
#[derive(Default)]
struct WrapCache {
    frame: Option<Rc<LayoutFrame>>,
}

impl WrapCache {
    /// 缓存命中要求：文本、折行宽度、**逐行度量（字号/行高）** 都一致。
    ///
    /// 度量必须进键：标题换级（h1↔h2）或换主题都会改变字号，只按文本比对会复用
    /// 旧度量，出现"文字变大但行高没变"这类错位。
    fn get(&self, text: &str, wrap_width: f32, metrics: &[LineMetric]) -> Option<Rc<LayoutFrame>> {
        let frame = self.frame.as_ref()?;
        let hit = frame.text == text
            && (frame.wrap_width - wrap_width).abs() < 0.5
            && frame.metrics.len() == metrics.len()
            && frame.metrics.iter().zip(metrics).all(|(a, b)| {
                (a.font_size - b.font_size).abs() < 0.01
                    && (a.line_height - b.line_height).abs() < 0.01
                    // 列表 gutter/hanging indent 也是布局输入：caret 进入 marker 会改变
                    // marker 显示宽度，度量变了但文本可能暂时相同（空 item），必须重排。
                    && (a.indent - b.indent).abs() < 0.01
                    && (a.wrap_indent - b.wrap_indent).abs() < 0.01
                    && a.mono == b.mono
            });
        hit.then(|| Rc::clone(frame))
    }
}

/// 视图 ↔ 元素共享的绘制状态。
pub struct EditorShared {
    cache: RefCell<WrapCache>,
    geometry: RefCell<Option<Rc<EditorGeometry>>>,
    viewport: Cell<(f32, f32)>,
    /// 上一次记录的"正文 hitbox 是否被悬停"（**只用于 debug 日志去重**，
    /// 指针样式与进出判定已改用 gpui 原生 hitbox 机制，不再由这里驱动）。
    hitbox_hovered: Cell<Option<bool>>,
}

impl EditorShared {
    /// 新建一份共享状态（每个窗口一份）。
    #[must_use]
    pub fn new() -> Rc<Self> {
        Rc::new(Self {
            cache: RefCell::new(WrapCache::default()),
            geometry: RefCell::new(None),
            viewport: Cell::new((0.0, 0.0)),
            hitbox_hovered: Cell::new(None),
        })
    }

    /// 取得（必要时重算）折行结果。
    ///
    /// 断行用 GPUI 的 `LineWrapper`：与 `Text` 元素同一套规则（字体回退、词边界、
    /// CJK 断行），因此"折行宽度"和"实际 shaping 宽度"来自同一个字体系统，
    /// 不需要自己按字符平均宽度估算。
    pub fn frame(&self, request: WrapRequest<'_>) -> Rc<LayoutFrame> {
        let WrapRequest {
            text,
            wrap_width,
            default_metric,
            metrics,
            text_system,
            font,
            font_size,
        } = request;
        if let Some(frame) = self.cache.borrow().get(text, wrap_width, metrics) {
            return frame;
        }

        let mut wrapper = text_system.line_wrapper(font.clone(), font_size);
        let mut boundaries = |line: &str, width: f32| -> Vec<usize> {
            wrapper
                .wrap_line(
                    &[LineFragment::text(line)],
                    px(width),
                    IndentAdjustment::NoIndent,
                )
                .map(|boundary| boundary.ix)
                .collect()
        };
        let frame = Rc::new(layout::wrap_with(
            text,
            wrap_width,
            default_metric,
            metrics,
            &mut boundaries,
        ));
        self.cache.borrow_mut().frame = Some(Rc::clone(&frame));
        frame
    }

    /// 记录本帧几何（绘制阶段写入）。
    pub fn set_geometry(&self, geometry: Rc<EditorGeometry>) {
        *self.geometry.borrow_mut() = Some(geometry);
    }

    /// 上一帧几何（事件回调 / 键盘导航读取）。
    #[must_use]
    pub fn geometry(&self) -> Option<Rc<EditorGeometry>> {
        self.geometry.borrow().clone()
    }

    /// 记录本帧绘制区域大小（像素）。
    pub fn set_viewport(&self, width: f32, height: f32) {
        self.viewport.set((width, height));
    }

    /// 上一帧绘制区域大小 `(宽, 高)`（像素）。
    #[must_use]
    pub fn viewport(&self) -> (f32, f32) {
        self.viewport.get()
    }

    /// 记录"正文 hitbox 是否被悬停"，只有**变化**时返回 `true`（debug 日志去重）。
    pub fn note_hitbox_hovered(&self, hovered: bool) -> bool {
        if self.hitbox_hovered.get() == Some(hovered) {
            return false;
        }
        self.hitbox_hovered.set(Some(hovered));
        true
    }
}

/// 点是否在正文元素的绘制矩形内（半开区间：右/下边界算外面）。
///
/// **现在只用于单测与离线诊断**：运行时不再拿它判"要不要 I-beam" ——
/// 指针样式已改由 gpui 原生 hitbox 机制管理（`window.set_cursor_style(style, &hitbox)`，
/// 命中链变化时 gpui 自己切进/切出），不需要我们再判定一次。
/// 保留它是因为边界语义（半开区间）仍然值得被单测钉住。
#[must_use]
pub fn inside_text_bounds(position: Point<Pixels>, bounds: Bounds<Pixels>) -> bool {
    bounds.contains(&position)
}

#[cfg(test)]
mod tests {
    use super::inside_text_bounds;
    use gpui::{point, px, size, Bounds};

    /// 正文元素矩形：左上 (0, 0)，100 × 50。
    fn bounds() -> Bounds<gpui::Pixels> {
        Bounds::new(point(px(0.0), px(0.0)), size(px(100.0), px(50.0)))
    }

    #[test]
    fn interior_points_are_inside() {
        assert!(inside_text_bounds(point(px(0.0), px(0.0)), bounds()));
        assert!(inside_text_bounds(point(px(50.0), px(25.0)), bounds()));
        // 贴住右下角内侧一点点：仍算里面
        assert!(inside_text_bounds(point(px(99.5), px(49.5)), bounds()));
    }

    #[test]
    fn edges_are_half_open() {
        // 上边界 / 左边界算里面（半开区间含起点）
        assert!(inside_text_bounds(point(px(0.0), px(10.0)), bounds()));
        assert!(inside_text_bounds(point(px(10.0), px(0.0)), bounds()));
        // 右边界 / 下边界上的点算**外面**：那里已经是状态栏/空白区
        assert!(!inside_text_bounds(point(px(100.0), px(25.0)), bounds()));
        assert!(!inside_text_bounds(point(px(50.0), px(50.0)), bounds()));
    }

    #[test]
    fn just_outside_is_outside() {
        assert!(!inside_text_bounds(point(px(-0.1), px(25.0)), bounds()));
        assert!(!inside_text_bounds(point(px(50.0), px(-0.1)), bounds()));
        assert!(!inside_text_bounds(point(px(100.1), px(25.0)), bounds()));
        assert!(!inside_text_bounds(point(px(50.0), px(50.1)), bounds()));
    }
}
