//! 已绘制的一帧几何：源码行 → 视觉行 → 字形 x 偏移。
//!
//! 命中测试**只**读这里的数据，不做任何"字符数 × 平均宽度"的估算：
//!
//! - 纵向：视觉行高度固定，`y / 行高` 直接给出视觉行下标；
//! - 横向：用该行真实的 [`ShapedLine`]（`LineLayout::closest_index_for_x` /
//!   `x_for_index`）在像素与**字节**之间换算，再交给 `menghunote_text` 换成
//!   `UnicodeScalar`。
//!
//! 这里只覆盖"当前可见（含一行 overscan）"的视觉行：屏外的行不 shaping，
//! 因此 [`EditorGeometry::byte_at_row_x`] 对屏外行返回 `None`，调用方要么放弃
//! （滚轮），要么按需 shaping（键盘上下移动，见 `glyph::shape_row`）。

use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{point, px, size, Bounds, Pixels, Point, RenderImage, ShapedLine};
use menghunote_editor::{Bias, MarkdownTableAlignment, ObjectKind, StyleKind};

use super::glyph;
use super::layout::LayoutFrame;
use super::theme;

/// 一帧的绘制几何（**只覆盖可见范围**）。
pub struct EditorGeometry {
    /// 本帧折行结果（与 `text` 同源）。
    pub frame: Rc<LayoutFrame>,
    /// 与 [`Self::visible_rows`] 一一对应的 shaping 结果。
    pub shaped: Vec<ShapedLine>,
    /// `shaped[0]` 对应的全局视觉行下标。
    pub visible_start: usize,
    /// 本帧实际使用的垂直滚动量（像素，正 = 内容向上滚过）。
    pub scroll_y: f32,
    /// 绘制区域原点（窗口坐标）。
    pub origin: Point<Pixels>,
    /// **默认**行高（正文）；逐行行高请用 [`Self::row_height`]。
    pub line_height: Pixels,
    /// 绘制区域高度（像素）。
    pub viewport_height: f32,
    /// 本帧的语义样式段（**display** 字节区间，完整覆盖全文）。
    ///
    /// 绘制与命中都从这里取"这一行怎么分段"，保证两边 shaping 完全一致
    /// （粗体会让字变宽，命中若用无样式 shaping 会偏）。
    pub styles: Rc<Vec<(Range<usize>, StyleKind)>>,
    /// 本帧可见的图片对象（按视觉行顺序；只覆盖可见范围）。
    pub objects: Vec<ObjectPaint>,
    /// 本帧可见的任务复选框（ADR 0019 S3；按视觉行顺序）。
    pub task_boxes: Vec<TaskBoxPaint>,
}

/// 图片对象的绘制状态（占位与图片共用同一块矩形，几何完全一致）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectPaintState {
    /// 正在后台解码。
    Loading,
    /// 资产不存在 / 只有元数据（待同步）。
    Missing,
    /// 解码失败（损坏 / 超预算 / 不支持的格式）。
    Failed,
    /// 已解码，可直接绘制。
    Ready,
}

/// 一个表格单元格的绘制数据（ADR 0019 S3b7：行内 Markdown）。
#[derive(Debug, Clone)]
pub struct TableCellPaint {
    /// 单元格矩形（窗口坐标，不含边框）。
    pub rect: Bounds<Pixels>,
    /// 已 shaping 的文本（行内 Markdown 的 display 文本，已按列宽截断；空 cell 为
    /// `None`）。
    pub text: Option<ShapedLine>,
    /// 对齐方式（GFM 分隔行；表头与数据行共用）。
    pub align: MarkdownTableAlignment,
    /// 单元格内的行内链接（已按列宽截断；空 = 没有可见链接）。
    ///
    /// 矩形来自**同一份** shaping（`ShapedLine::x_for_index`），因此点击与绘制逐像素
    /// 一致；截断点之后的链接不在列表里（没画出来就不可点）。
    pub links: Vec<TableCellLinkPaint>,
}

/// 一个表格单元格里的行内链接（ADR 0019 S3b7）。
///
/// 与正文链接的 `link_hitboxes` 同一套语义：**可见文本**区间可点，末尾边界不算命中，
/// 单击交给宿主按 scheme 白名单（http/https/mailto）打开。
#[derive(Debug, Clone, PartialEq)]
pub struct TableCellLinkPaint {
    /// 链接可见文本的窗口矩形（已含单元格内边距与对齐偏移）。
    pub rect: Bounds<Pixels>,
    /// 链接可见文本在该单元格 **display** 文本里的字节区间（已按列宽截断裁剪）。
    pub display: Range<usize>,
    /// 目标 URL（原样字符串；scheme 校验在宿主）。
    pub dest: String,
}

/// 一行表格的绘制数据。
#[derive(Debug, Clone)]
pub struct TableRowPaint {
    /// 整行矩形（窗口坐标；含单元格左右边界，不含行间分隔线）。
    pub rect: Bounds<Pixels>,
    /// 逐列单元格（长度 = 表格列数）。
    pub cells: Vec<TableCellPaint>,
    /// 是否表头行。
    pub header: bool,
    /// 行下标（**0 = 表头**，`1..=n` = 第 `row - 1` 条数据行）。
    ///
    /// 长表格按视口裁剪时 `rows` 只含可见行，下标不能由位置推断，必须随行一起带出来：
    /// 单元格命中/编辑都靠它寻址（ADR 0019 S3b2）。
    pub row: usize,
}

/// 一个**可见**表格对象的绘制数据（只含与视口相交的行，长表格按行裁剪）。
#[derive(Debug, Clone)]
pub struct TablePaint {
    /// 表格外框矩形（窗口坐标）。
    pub rect: Bounds<Pixels>,
    /// 列数（≥ 1）。
    pub columns: usize,
    /// **全部**数据行数（不含表头；长表格被裁剪时仍用它算行号/拖拽槽位）。
    pub row_count: usize,
    /// 表头行（可能因滚动不可见，但表头行高仍计入几何）。
    pub header: Option<TableRowPaint>,
    /// 与视口相交的数据行。
    pub rows: Vec<TableRowPaint>,
    /// 数据行高度（布局度量与绘制共用）。
    pub row_height: f32,
    /// 表头行高度。
    pub header_height: f32,
    /// 单列宽度（像素；列把手/拖拽槽位的几何来源）。
    pub column_width: f32,
    /// 当前选中的单元格 `(row, column)`（row 0 = 表头）：绘制描边/底色用。
    ///
    /// 只影响绘制；命中一律走 [`EditorGeometry::table_cell_at`]（同一份矩形）。
    pub selected_cell: Option<(usize, usize)>,
}

/// 一个被命中的表格单元格（窗口坐标 → 表格行/列）。
///
/// `row` 与 [`TableRowPaint::row`] 同一套下标（0 = 表头）；`object_line` 是表格对象行
/// （表头源码行号），控制器用它再加上行列号定位 cell span。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TableCellHit {
    /// 表格对象行（表头源码行号，0 起）。
    pub object_line: usize,
    /// 行下标（0 = 表头）。
    pub row: usize,
    /// 列下标（0 起）。
    pub column: usize,
}

/// 表格拖拽把手的轴（ADR 0019 S3b5）：列把手水平拖、行把手垂直拖。
///
/// 两者共用同一套拖拽状态机（命中 → 拖动 → 插入槽位 → 释放重排），但**行把手不包含
/// 表头行**：表头固定，不参与行重排。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TableDragAxis {
    /// 列（下标 0 起；从表头顶部的小把手水平拖动）。
    Column,
    /// 数据行（下标 0 = 第一条数据行；从行左侧的小把手垂直拖动）。
    Row,
}

/// 一个表格拖拽把手的几何（**绘制、命中、headless 断言同源**）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TableDragHandlePaint {
    /// 轴向。
    pub axis: TableDragAxis,
    /// 表格对象行（表头源码行号）。
    pub line: usize,
    /// 列把手：列下标；行把手：数据行下标（0 起、不含表头）。
    pub index: usize,
    /// 绘制矩形（窗口坐标）：只画这个小胶囊，不遮挡正文/单元格文字。
    pub paint: Bounds<Pixels>,
    /// 命中矩形（窗口坐标）：比绘制矩形略大，避免“差 1px 点不中”。
    pub hit: Bounds<Pixels>,
}

/// 图片对象的绘制载荷（已解码像素或占位框 + alt/状态文案）。
#[derive(Debug, Clone)]
pub struct ImagePaint {
    /// 已解码图片（`None` = 占位）。
    pub image: Option<Arc<RenderImage>>,
    /// 占位/alt 文本（已 shaping，单行；`None` = 图片已就绪或没有文案）。
    pub label: Option<ShapedLine>,
    /// 绘制状态。
    pub state: ObjectPaintState,
}

/// 一个 Object 的绘制载荷：图片像素 / GFM 表格 / Mermaid 光栅图。
///
/// 内嵌画布预览**复用 [`Self::Image`]**（都是“一张位图 + 可选占位文案”，绘制路径
/// 完全相同），不新增第四个分支。
///
/// 三个分支都装箱：`ShapedLine`/`TablePaint` 让 enum 本体很大，而 `ObjectPaint`
/// 每帧按可见对象存进 `Vec`，不装箱会在滚动时反复拷贝大块内存。
#[derive(Debug, Clone)]
pub enum ObjectPaintContent {
    /// 图片（已解码像素或占位框 + alt/状态文案）。
    Image(Box<ImagePaint>),
    /// GFM 表格（表格内不做富文本）。单元格原地编辑时**仍然**走这个分支：
    /// 编辑输入框是浮在这个矩形上的独立元素，表格其它行列照常绘制（ADR 0019 §12.6）。
    Table(Box<TablePaint>),
    /// Mermaid（后台渲染/光栅化后的像素，或「渲染中…/渲染失败」占位）。
    ///
    /// 与图片共用 [`ImagePaint`]：两者都是“一张位图 + 可选占位文案”，绘制路径相同，
    /// 差异只在数据来源（资产解码 / Mermaid 渲染）与状态文案。
    Mermaid(Box<ImagePaint>),
}

/// 一个**可见**的 Object 行（ADR 0019 S2a 图片 / S3a 表格）：绘制、命中、工具条共用它。
///
/// `rect` 是**窗口坐标**的块矩形；行高在布局度量里已经等于 `rect 高度 + 上下内边距`，
/// 所以滚动/命中与 `LayoutFrame` 几何天然一致。表格虽然是多行源码，但在 display 上
/// 只占**一个**视觉行（一个占位符），因此 `line` 是表头行、`row` 是那一个视觉行。
#[derive(Debug, Clone)]
pub struct ObjectPaint {
    /// 对象种类（图片 / 表格 / Mermaid / 内嵌画布）。
    pub kind: ObjectKind,
    /// 在同类集合里的下标（图片 → `images()`；表格 → `tables()`；
    /// Mermaid → `mermaid_blocks()`；画布 → `drawings()`）。
    pub index: usize,
    /// 对象行（图片所在行 / 表格表头行；0 起）。
    pub line: usize,
    /// 结束源码行（0 起，含）。
    pub end_line: usize,
    /// 视觉行号（0 起）。
    pub row: usize,
    /// 块矩形（窗口坐标）。
    pub rect: Bounds<Pixels>,
    /// 绘制载荷。
    pub content: ObjectPaintContent,
    /// 是否处于选中态（单击选中 / 文本选区相交）——画描边。
    pub selected: bool,
}

impl TableCellPaint {
    /// 平移矩形（元素局部坐标 → 窗口坐标）。
    fn translate(&mut self, origin: Point<Pixels>) {
        self.rect = Bounds::new(self.rect.origin + origin, self.rect.size);
        for link in &mut self.links {
            link.rect = Bounds::new(link.rect.origin + origin, link.rect.size);
        }
    }
}

impl TableRowPaint {
    /// 平移整行与全部单元格矩形。
    fn translate(&mut self, origin: Point<Pixels>) {
        self.rect = Bounds::new(self.rect.origin + origin, self.rect.size);
        for cell in &mut self.cells {
            cell.translate(origin);
        }
    }
}

impl TablePaint {
    /// 平移表格外框与所有可见行/单元格矩形。
    fn translate(&mut self, origin: Point<Pixels>) {
        self.rect = Bounds::new(self.rect.origin + origin, self.rect.size);
        if let Some(header) = &mut self.header {
            header.translate(origin);
        }
        for row in &mut self.rows {
            row.translate(origin);
        }
    }
}

impl TablePaint {
    /// 第 `row` 条**数据行**（0 起、不含表头）的窗口矩形；越界返回 `None`。
    ///
    /// 行高固定，因此被视口裁剪掉的行也能算出矩形（拖拽槽位/指示线要用）。
    #[must_use]
    pub fn data_row_rect(&self, row: usize) -> Option<Bounds<Pixels>> {
        if row >= self.row_count {
            return None;
        }
        let top = self.rect.origin.y.as_f32() + self.header_height + self.row_height * row as f32;
        Some(Bounds::new(
            point(self.rect.origin.x, px(top)),
            size(self.rect.size.width, px(self.row_height)),
        ))
    }

    /// 整行/整列的矩形（窗口坐标）：**选中高亮、拖拽 ghost、headless 断言同源**。
    ///
    /// - [`TableDragAxis::Column`]：整列（表头 + **全部**数据行；按固定列宽/行高算出，
    ///   被视口裁剪掉的行也算在内），`index` = 列下标；
    /// - [`TableDragAxis::Row`]：一条**数据行**（不含表头），`index` = 数据行下标
    ///   （0 起，与行把手同一套寻址）。
    ///
    /// 越界 / 几何不可用 → `None`（调用方不画）。
    #[must_use]
    pub fn axis_rect(&self, axis: TableDragAxis, index: usize) -> Option<Bounds<Pixels>> {
        match axis {
            TableDragAxis::Column => {
                if index >= self.columns || self.column_width <= 0.0 {
                    return None;
                }
                Some(Bounds::new(
                    point(
                        px(self.rect.origin.x.as_f32() + self.column_width * index as f32),
                        self.rect.origin.y,
                    ),
                    size(px(self.column_width), self.rect.size.height),
                ))
            }
            TableDragAxis::Row => self.data_row_rect(index),
        }
    }

    /// 列把手（表头顶部的小胶囊）的绘制矩形；表头不在本帧可见范围时返回 `None`。
    #[must_use]
    pub fn column_handle_paint(&self, column: usize) -> Option<Bounds<Pixels>> {
        if column >= self.columns || self.column_width <= 0.0 {
            return None;
        }
        let width = theme::TABLE_DRAG_COLUMN_HANDLE_WIDTH.min(self.column_width - 4.0);
        if width <= 0.0 {
            return None;
        }
        let height = theme::TABLE_DRAG_COLUMN_HANDLE_HEIGHT;
        let center_x = self.rect.origin.x.as_f32() + self.column_width * (column as f32 + 0.5);
        let top = self.rect.origin.y.as_f32() - height / 2.0;
        Some(Bounds::new(
            point(px(center_x - width / 2.0), px(top)),
            size(px(width), px(height)),
        ))
    }

    /// 列把手的命中矩形：绘制矩形外扩 [`theme::TABLE_DRAG_HIT_PAD`]，但**不越过该表格
    /// 对象行的上边**（越过会让悬停态在上下 1px 处抖动）。
    #[must_use]
    pub fn column_handle_hit(&self, column: usize) -> Option<Bounds<Pixels>> {
        let paint = self.column_handle_paint(column)?;
        let pad = theme::TABLE_DRAG_HIT_PAD;
        let top =
            (paint.origin.y.as_f32() - pad).max(self.rect.origin.y.as_f32() - theme::TABLE_PADDING);
        Some(Bounds::new(
            point(px(paint.origin.x.as_f32() - pad), px(top)),
            size(
                px(paint.size.width.as_f32() + pad * 2.0),
                px(paint.origin.y.as_f32() + paint.size.height.as_f32() + pad - top),
            ),
        ))
    }

    /// 行把手（数据行左侧的小胶囊）的绘制矩形；`row` 是数据行下标（0 起、不含表头）。
    #[must_use]
    pub fn row_handle_paint(&self, row: usize) -> Option<Bounds<Pixels>> {
        let row_rect = self.data_row_rect(row)?;
        let width = theme::TABLE_DRAG_ROW_HANDLE_WIDTH;
        let height = theme::TABLE_DRAG_ROW_HANDLE_HEIGHT
            .min(self.row_height - 8.0)
            .max(6.0);
        let top = row_rect.origin.y.as_f32() + (self.row_height - height) / 2.0;
        Some(Bounds::new(
            point(px(self.rect.origin.x.as_f32() - width / 2.0), px(top)),
            size(px(width), px(height)),
        ))
    }

    /// 行把手的命中矩形：绘制矩形外扩 [`theme::TABLE_DRAG_HIT_PAD`]，纵向**夹在该数据行
    /// 之内**（不越到相邻行，避免抢到相邻行的点击）。
    #[must_use]
    pub fn row_handle_hit(&self, row: usize) -> Option<Bounds<Pixels>> {
        let paint = self.row_handle_paint(row)?;
        let row_rect = self.data_row_rect(row)?;
        let pad = theme::TABLE_DRAG_HIT_PAD;
        let top = (paint.origin.y.as_f32() - pad).max(row_rect.origin.y.as_f32());
        let bottom = (paint.origin.y.as_f32() + paint.size.height.as_f32() + pad)
            .min(row_rect.origin.y.as_f32() + row_rect.size.height.as_f32());
        Some(Bounds::new(
            point(px(paint.origin.x.as_f32() - pad), px(top)),
            size(
                px(paint.size.width.as_f32() + pad * 2.0),
                px((bottom - top).max(1.0)),
            ),
        ))
    }

    /// 指针位置 → **列插入槽位**（`0..=columns`）；`None` = 已拖出表格（释放即取消）。
    ///
    /// 槽位是“缝隙”：落在某列左半 → 插到该列之前，右半 → 之后。指针允许离开表格
    /// [`theme::TABLE_DRAG_CANCEL_MARGIN`] 像素以内（把手本身就贴在边框上）。
    #[must_use]
    pub fn column_drop_slot(&self, position: Point<Pixels>) -> Option<usize> {
        if self.columns == 0 || self.column_width <= 0.0 {
            return None;
        }
        let left = self.rect.origin.x.as_f32();
        let right = left + self.rect.size.width.as_f32();
        let top = self.rect.origin.y.as_f32();
        let bottom = top + self.rect.size.height.as_f32();
        let margin = theme::TABLE_DRAG_CANCEL_MARGIN;
        let (x, y) = (position.x.as_f32(), position.y.as_f32());
        if x < left - margin || x > right + margin || y < top - margin || y > bottom + margin {
            return None;
        }
        let local = (x - left).clamp(0.0, self.rect.size.width.as_f32());
        let index = ((local / self.column_width).floor() as usize).min(self.columns - 1);
        let offset = local - self.column_width * index as f32;
        Some(if offset >= self.column_width / 2.0 {
            index + 1
        } else {
            index
        })
    }

    /// 指针位置 → **数据行插入槽位**（`0..=row_count`）；`None` = 已拖出表格。
    ///
    /// 指针在表头/分隔行上 → 槽位 0（插到第一条数据行之前）；在最后一行之下 →
    /// 槽位 `row_count`。表头行**不参与**行重排，因此槽位永远落在数据行区间里。
    #[must_use]
    pub fn row_drop_slot(&self, position: Point<Pixels>) -> Option<usize> {
        if self.row_count == 0 || self.row_height <= 0.0 {
            return None;
        }
        let left = self.rect.origin.x.as_f32();
        let right = left + self.rect.size.width.as_f32();
        let top = self.rect.origin.y.as_f32();
        let bottom = top + self.rect.size.height.as_f32();
        let margin = theme::TABLE_DRAG_CANCEL_MARGIN;
        let (x, y) = (position.x.as_f32(), position.y.as_f32());
        if x < left - margin || x > right + margin || y < top - margin || y > bottom + margin {
            return None;
        }
        let local = (y - top).max(0.0);
        let rows_top = self.header_height;
        let rows_bottom = rows_top + self.row_height * self.row_count as f32;
        if local < rows_top {
            return Some(0);
        }
        if local >= rows_bottom {
            return Some(self.row_count);
        }
        let index =
            (((local - rows_top) / self.row_height).floor() as usize).min(self.row_count - 1);
        let offset = local - rows_top - self.row_height * index as f32;
        Some(if offset >= self.row_height / 2.0 {
            index + 1
        } else {
            index
        })
    }

    /// 插入位置指示线矩形（窗口坐标）：列 = 竖线（2px），行 = 横线（2px）。
    ///
    /// 槽位越界返回 `None`（调用方不画）。
    #[must_use]
    pub fn drop_indicator_rect(&self, axis: TableDragAxis, slot: usize) -> Option<Bounds<Pixels>> {
        let thickness = theme::TABLE_DRAG_INDICATOR_THICKNESS;
        let left = self.rect.origin.x.as_f32();
        let top = self.rect.origin.y.as_f32();
        let right = left + self.rect.size.width.as_f32();
        let bottom = top + self.rect.size.height.as_f32();
        match axis {
            TableDragAxis::Column => {
                if slot > self.columns {
                    return None;
                }
                let x = if slot >= self.columns {
                    right - thickness
                } else {
                    (left + self.column_width * slot as f32 - thickness / 2.0)
                        .clamp(left, (right - thickness).max(left))
                };
                Some(Bounds::new(
                    point(px(x), px(top)),
                    size(px(thickness), px((bottom - top).max(1.0))),
                ))
            }
            TableDragAxis::Row => {
                if slot > self.row_count {
                    return None;
                }
                let rows_top = top + self.header_height;
                let rows_bottom = rows_top + self.row_height * self.row_count as f32;
                let y = if slot >= self.row_count {
                    (rows_bottom - thickness).clamp(top, (bottom - thickness).max(top))
                } else {
                    (rows_top + self.row_height * slot as f32 - thickness / 2.0)
                        .clamp(rows_top, (rows_bottom - thickness).max(rows_top))
                };
                Some(Bounds::new(
                    point(px(left), px(y)),
                    size(px((right - left).max(1.0)), px(thickness)),
                ))
            }
        }
    }
}

impl ObjectPaint {
    /// 平移整个对象（含表格内部的表头/行/单元格矩形）。
    ///
    /// `object_paints` 产出的是**元素局部坐标**（绘制、命中、单元格 shaping 都在同一
    /// 坐标系里算），元素 prepaint 在注册几何前统一加上 `bounds.origin`；表格的嵌套
    /// 矩形必须一起平移，否则表头/单元格会画在错误位置。
    pub fn translate(&mut self, origin: Point<Pixels>) {
        self.rect = Bounds::new(self.rect.origin + origin, self.rect.size);
        if let ObjectPaintContent::Table(table) = &mut self.content {
            table.translate(origin);
        }
    }

    /// 是否可直接绘制（图片/Mermaid 已解码 / 表格恒可绘制）。
    #[must_use]
    pub fn is_ready(&self) -> bool {
        match &self.content {
            ObjectPaintContent::Image(image) | ObjectPaintContent::Mermaid(image) => {
                matches!(image.state, ObjectPaintState::Ready) && image.image.is_some()
            }
            ObjectPaintContent::Table(_) => true,
        }
    }

    /// 表格绘制载荷（图片对象返回 `None`）。
    #[must_use]
    pub fn table(&self) -> Option<&TablePaint> {
        match &self.content {
            ObjectPaintContent::Image(_) | ObjectPaintContent::Mermaid(_) => None,
            ObjectPaintContent::Table(table) => Some(table),
        }
    }
}

/// 一个**可见**的任务复选框（ADR 0019 S3）：绘制、命中、headless 断言共用。
///
/// `rect` 是窗口坐标的**占位矩形**（display 上两连 NBSP 的真实 shaping 区间 ×
/// 该视觉行行高）：点击命中用整个矩形（比 13px 方框更好点），绘制在矩形内居中画方框。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TaskBoxPaint {
    /// 列表项下标（`ParsedMarkdown::items`；点击后按它切换源码）。
    pub item: usize,
    /// 任务框源码区间（点击时与当前解析结果比对，防止陈旧帧点到别的 item）。
    pub source: (usize, usize),
    /// 是否已勾选（来自解析器状态；source 是唯一真源）。
    pub checked: bool,
    /// 占位矩形（窗口坐标）。
    pub rect: Bounds<Pixels>,
}

impl EditorGeometry {
    /// 可见视觉行下标区间（半开）。
    #[must_use]
    pub fn visible_rows(&self) -> Range<usize> {
        self.visible_start..self.visible_start + self.shaped.len()
    }

    /// 文字区**基础**左边界（窗口坐标；不含块级缩进）。
    #[must_use]
    pub fn text_left(&self) -> Pixels {
        self.origin.x + px(theme::PAD_X)
    }

    /// 第 `row` 行的文字左边界 = 基础左边界 + 该行缩进（引用块）。
    ///
    /// **几何与绘制都用这一个函数**，避免 `PAD_X`/缩进被加两次
    /// （历史 bug：两处各加一次 → 选区整体右移 14px）。
    #[must_use]
    pub fn text_left_for_row(&self, row: usize) -> Pixels {
        self.origin.x + px(theme::PAD_X) + px(self.frame.row_indent(row))
    }

    /// 第 `row` 行的引用左缩进（像素）。
    #[must_use]
    pub fn row_indent(&self, row: usize) -> Pixels {
        px(self.frame.row_indent(row))
    }

    /// 第 `row` 行的行高（逐行可变：标题行更高）。
    #[must_use]
    pub fn row_height(&self, row: usize) -> Pixels {
        px(self.frame.row_height(row))
    }

    /// 第 `row` 行的字号（逐行可变）。
    #[must_use]
    pub fn row_font_size(&self, row: usize) -> Pixels {
        px(self.frame.row_font_size(row))
    }

    /// 第 `row` 行在窗口坐标里的上边（按累计行顶，**不是** `行号 × 行高`）。
    #[must_use]
    pub fn row_top(&self, row: usize) -> Pixels {
        self.origin.y + px(theme::PAD_Y) - px(self.scroll_y) + px(self.frame.row_top_offset(row))
    }

    /// 滚动后可见的视觉行区间（半开）：从 `scroll_y` 顶端开始，直到填满视口。
    ///
    /// 行高逐行不同，所以不能用 `视口高 / 行高` 估；这里按累计行顶走一遍。
    #[must_use]
    pub fn rows_in_view(&self, scroll_y: f32, viewport_height: f32) -> Range<usize> {
        let top = scroll_y.max(0.0);
        let bottom = top + viewport_height.max(0.0);
        let first = self.frame.row_at_y(top);
        let mut last = first;
        while last < self.frame.row_count() && self.frame.row_top_offset(last) < bottom {
            last += 1;
        }
        first..last.max(first + 1).min(self.frame.row_count())
    }

    /// display 字节所在视觉行，**仅当该行真的在视口内**（prepaint 多取的一行 overscan
    /// 不算）时返回。
    ///
    /// 代码块浮层（复制按钮）用它判断"首行是否可见"：屏外行既没有绘制也没有点击意义。
    #[must_use]
    pub fn visible_row_of_byte(&self, byte: usize) -> Option<usize> {
        let row = self.frame.row_of_byte(byte);
        self.rows_in_view(self.scroll_y, self.viewport_height)
            .contains(&row)
            .then_some(row)
    }

    /// display 区间 `range` 覆盖的视觉行区间（半开），**裁到真正可见的行**。
    ///
    /// 首行按 `range.start`、末行按 `range.end - 1`（不含右端点）定位：跨软折行的
    /// 代码块因此拿到首/末可见行；整块滚出视口时返回 `None`（不画、不命中）。
    #[must_use]
    pub fn visible_rows_for_display_range(&self, range: &Range<usize>) -> Option<Range<usize>> {
        if range.is_empty() {
            return None;
        }
        let first = self.frame.row_of_byte(range.start);
        let last = self
            .frame
            .row_of_byte(range.end.saturating_sub(1).max(range.start));
        let visible = self.rows_in_view(self.scroll_y, self.viewport_height);
        let start = first.max(visible.start);
        let end = (last + 1).min(visible.end);
        (start < end).then_some(start..end)
    }

    /// 窗口坐标 `y` 落在哪一视觉行，夹到可见范围内。
    #[must_use]
    pub fn row_at_y(&self, y: Pixels) -> usize {
        let visible = self.visible_rows();
        if visible.is_empty() {
            return self.visible_start;
        }
        let last_visible = visible.end - 1;
        let top = self.origin.y + px(theme::PAD_Y) - px(self.scroll_y);
        let local_y = (y - top).as_f32();
        self.frame
            .row_at_y(local_y)
            .clamp(visible.start, last_visible)
    }

    /// 窗口坐标 `position` 命中的源码字节（已夹到该视觉行的字节区间内）。
    #[must_use]
    pub fn byte_at(&self, position: Point<Pixels>) -> usize {
        let row = self.row_at_y(position.y);
        let local_x = (position.x - self.text_left_for_row(row)).max(px(0.0));
        self.byte_at_row_x(row, local_x)
            .unwrap_or_else(|| self.frame.row_bytes(row).start)
    }

    /// 在**可见**的行内把像素 x（相对行首）换成源码字节；屏外行返回 `None`。
    #[must_use]
    /// 「点击在字形左半/右半」→ 投影偏置（**纯函数**，与 UI 解耦，便于单测）。
    ///
    /// **右半 → Left、左半 → Right**：隐藏标记宽度为 0，同一个 display 位置对应两个
    /// source 位置。点字符右半时人期望"跨过这个字符"，于是取左侧偏置（停在隐藏标记
    /// 之前，即构造内）；点左半时停在字符之前，取右侧偏置。
    pub fn bias_for_half(right_half: bool) -> Bias {
        if right_half {
            Bias::Left
        } else {
            Bias::Right
        }
    }

    /// 命中：返回 `(display 字节, 偏置)`。
    ///
    /// 偏置由"点是否落在该边界的右侧字形内"决定：`closest_index_for_x` 取最近边界，
    /// 若点在该边界**左侧**（即它落在前一个字符的右半），说明点在字形右半 → `Left`。
    pub fn byte_at_with_bias(&self, position: Point<Pixels>) -> Option<(usize, Bias)> {
        let row = self.row_at_y(position.y);
        let bytes = self.frame.row_bytes(row);
        let shaped = self.shaped.get(row.checked_sub(self.visible_start)?)?;
        // 对象是原子的：拖选/命中落到对象上时一律给“占位符之后”（选区因此包含对象）。
        if self.object_row(row).is_some() {
            return Some((bytes.end, Bias::Right));
        }
        if bytes.is_empty() {
            return Some((bytes.start, Bias::Right));
        }
        let local_x = (position.x - self.text_left_for_row(row)).max(px(0.0));
        let index = shaped.closest_index_for_x(local_x);
        let index = index.min(bytes.len());
        let boundary_x = shaped.x_for_index(index);
        // 点在该边界左侧 → 处在前面字符的右半。
        let right_half = local_x < boundary_x;
        Some((bytes.start + index, Self::bias_for_half(right_half)))
    }

    pub fn byte_at_row_x(&self, row: usize, x: Pixels) -> Option<usize> {
        let bytes = self.frame.row_bytes(row);
        let shaped = self.shaped.get(row.checked_sub(self.visible_start)?)?;
        // 对象行整行是一个原子块：横向位置不影响落点（统一取占位符末尾）。
        if self.object_row(row).is_some() {
            return Some(bytes.end);
        }
        if bytes.is_empty() {
            return Some(bytes.start);
        }
        let index = shaped.closest_index_for_x(x);
        Some(bytes.start + index.min(bytes.len()))
    }

    /// 本帧可见的图片对象。
    #[must_use]
    pub fn objects(&self) -> &[ObjectPaint] {
        &self.objects
    }

    /// 第 `row` 行是否是图片对象行。
    #[must_use]
    pub fn object_row(&self, row: usize) -> Option<&ObjectPaint> {
        self.objects.iter().find(|object| object.row == row)
    }

    /// 窗口坐标命中的图片对象（整行都是对象块的可点击区域）。
    #[must_use]
    pub fn object_at(&self, position: Point<Pixels>) -> Option<&ObjectPaint> {
        let row = self.row_at_y(position.y);
        self.object_row(row)
    }

    /// 窗口坐标命中的图片对象**源码行**（工具条/点选用的轻量查询）。
    #[must_use]
    pub fn object_line_at(&self, position: Point<Pixels>) -> Option<usize> {
        self.object_at(position).map(|object| object.line)
    }

    /// 本帧可见的任务复选框（按视觉行顺序）。
    #[must_use]
    pub fn task_boxes(&self) -> &[TaskBoxPaint] {
        &self.task_boxes
    }

    /// 窗口坐标命中的表格单元格（ADR 0019 S3b2）。
    ///
    /// 只认**本帧画出来的**格子：长表格按视口裁剪后，看不见的行没有矩形，也就点不到。
    /// 矩形来自 `table_paint`（`TableCellPaint.rect`），因此点击与绘制逐像素一致，
    /// 绝不按字符宽度另算。
    #[must_use]
    pub fn table_cell_at(&self, position: Point<Pixels>) -> Option<TableCellHit> {
        let row = self.row_at_y(position.y);
        let object = self.object_row(row)?;
        let table = object.table()?;
        let rows = table.header.iter().chain(table.rows.iter());
        for paint_row in rows {
            if !paint_row.rect.contains(&position) {
                continue;
            }
            for (column, cell) in paint_row.cells.iter().enumerate() {
                if cell.rect.contains(&position) {
                    return Some(TableCellHit {
                        object_line: object.line,
                        row: paint_row.row,
                        column,
                    });
                }
            }
        }
        None
    }

    /// 窗口坐标命中的任务复选框（矩形是占位区，不是方框本身）。
    #[must_use]
    pub fn task_box_at(&self, position: Point<Pixels>) -> Option<&TaskBoxPaint> {
        self.task_boxes
            .iter()
            .find(|task| task.rect.contains(&position))
    }

    /// 窗口坐标命中的**表格单元格内行内链接**（ADR 0019 S3b7）。
    ///
    /// 只认本帧画出来的链接：矩形来自单元格 shaping（`TableCellLinkPaint.rect`），
    /// 截断点之后的链接没有矩形，也就点不到。命中优先级由调用方决定（链接先于
    /// 单元格：点链接要打开，不是进入编辑）。
    #[must_use]
    pub fn table_cell_link_at(&self, position: Point<Pixels>) -> Option<&TableCellLinkPaint> {
        let row = self.row_at_y(position.y);
        let object = self.object_row(row)?;
        let table = object.table()?;
        table
            .header
            .iter()
            .chain(table.rows.iter())
            .flat_map(|paint_row| paint_row.cells.iter())
            .flat_map(|cell| cell.links.iter())
            .find(|link| link.rect.contains(&position))
    }

    /// 第 `row` 行的样式分段 `(字节数, 种类)`（总长等于该行字节数）。
    #[must_use]
    pub fn spans_for_row(&self, row: usize) -> Vec<(usize, StyleKind)> {
        glyph::line_spans(&self.styles, self.frame.row_bytes(row))
    }

    /// 光标（源码字节 `byte`）所在行的窗口坐标 `(row, x)`；不在可见范围时返回 `None`。
    #[must_use]
    pub fn caret_row_x(&self, byte: usize) -> Option<(usize, Pixels)> {
        let row = self.frame.row_of_byte(byte);
        let bytes = self.frame.row_bytes(row);
        let shaped = self.shaped.get(row.checked_sub(self.visible_start)?)?;
        // 对象行没有字形：caret 只能贴在图片/占位框左右两侧。
        if let Some(object) = self.object_row(row) {
            let x = if byte <= bytes.start {
                object.rect.origin.x
            } else {
                object.rect.origin.x + object.rect.size.width
            };
            return Some((row, x));
        }
        if bytes.is_empty() {
            return Some((row, self.text_left_for_row(row)));
        }
        let offset = byte.saturating_sub(bytes.start).min(bytes.len());
        Some((
            row,
            self.text_left_for_row(row) + shaped.x_for_index(offset),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::point;

    fn frame(rows: usize) -> Rc<LayoutFrame> {
        let text = (0..rows)
            .map(|index| format!("{index}\n"))
            .collect::<String>();
        let mut boundaries = |_line: &str, _width: f32| Vec::new();
        Rc::new(super::super::layout::wrap_with(
            &text,
            200.0,
            super::super::layout::LineMetric::body(),
            &[],
            &mut boundaries,
        ))
    }

    /// 只验证不涉及 shaping 的几何：行定位必须夹在可见范围内。
    #[test]
    fn row_lookup_is_clamped_to_visible_rows() {
        let geometry = EditorGeometry {
            frame: frame(50),
            shaped: Vec::new(),
            visible_start: 10,
            scroll_y: 200.0,
            origin: point(px(0.0), px(0.0)),
            line_height: px(20.0),
            viewport_height: 220.0,
            styles: Rc::new(Vec::new()),
            objects: Vec::new(),
            task_boxes: Vec::new(),
        };
        assert!(geometry.visible_rows().is_empty());
        assert_eq!(geometry.row_at_y(px(0.0)), 10);
        assert_eq!(geometry.row_at_y(px(10_000.0)), 10);
        // 屏外行没有 shaping 结果，不能给出字节
        assert_eq!(geometry.byte_at_row_x(20, px(1.0)), None);
        assert_eq!(geometry.caret_row_x(0), None);
        // 第 11 行（下标 11）的上边 = 顶部内边距 - 滚动 + **累计行顶**（行高逐行可变）
        assert_eq!(
            geometry.row_top(11),
            px(0.0) + px(theme::PAD_Y) - px(200.0) + px(geometry.frame.row_top_offset(11))
        );
    }
    #[test]
    fn right_half_maps_to_left_bias() {
        use super::EditorGeometry;
        // 右半 → Left（跨过字形，停在隐藏标记之前）；左半 → Right。
        assert_eq!(
            EditorGeometry::bias_for_half(true),
            menghunote_editor::Bias::Left
        );
        assert_eq!(
            EditorGeometry::bias_for_half(false),
            menghunote_editor::Bias::Right
        );
    }
}
