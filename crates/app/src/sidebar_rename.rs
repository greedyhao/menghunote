//! 侧栏行内重命名输入框（单行 + IME）。
//!
//! ## 为什么单独一套，不复用 `editor/`
//!
//! 编辑器那套（Rope 镜像 / ProjectionMap / 选区 / 组合闸门）是为多行 Markdown 服务的。
//! 侧栏标题只需要"一行文本 + 一个字节光标 + 一段 preedit"，照搬会引入大量无关状态，
//! 而且两边坐标语义不同（编辑器还有虚拟输入文档）。所以这里写一个**最小**实现：
//!
//! - [`RenameState`]：纯状态 + 纯函数（可单测），内部用 **byte** 保存，对外换算 **UTF-16**；
//! - [`RenameInput`]：GPUI 实体，负责键盘 / IME（`EntityInputHandler`）/ 绘制；
//! - 平台给的 UTF-16 范围一律按"显示串"（= 原文在组合起点处插入 preedit）解释。
//!
//! ## IME 的两条约定
//!
//! 1. **preedit 只显示、不进标题**：组合串存在 `marked` 里，`text()` 永远是真实标题；
//! 2. **`Esc` 优先交给 IME**：Windows 上 IME 自己在组合中吃掉 `Esc` 并回一个
//!    `replace_and_mark_text_in_range(None, "", None)`；我们只在**没有组合**时把 `Esc`
//!    当成"取消编辑"。这是平台的语义顺序，我们不去抢。
//!
//! ## 粘贴与鼠标落点
//!
//! - 标题与表格单元格都支持 `Ctrl+V` / `Cmd+V`（`Ctrl+Shift+V` 同义）：统一读
//!   `cx.read_from_clipboard()`，单行过滤在 [`RenameState::insert`] / [`RenameState::insert_cell`]
//!   里做（[`RenameMode::Title`] 丢换行/制表符，单元格换成空格），**不记录剪贴板内容**。
//! - 笔记顶部大标题**不预选**：点击进入编辑时把窗口坐标记成 `pending_click`，输入框
//!   第一次 `paint` 用当前 `shape_line` 把它换算成最近的字节索引，光标落在点击处
//!   （编辑态内再次点击同理）。`Ctrl+A` 仍是唯一的全选入口。
//! - 大标题/单元格/侧栏输入框都支持**鼠标拖动选区**：按下记 anchor，按住移动把
//!   head 更新到指针处（可反向、可拖出输入框），抬起保留选区；没拖动的按下仍是
//!   单击定位。位置→字节**复用 `paint` 的 shaping 规则**（同字体、同字号、同内边距），
//!   监听用 `Window::on_mouse_event`（每帧重建、下一帧自动清空），因此拖出元素
//!   边界也继续更新，且不会逐帧累积监听。

use std::ops::Range;
use std::time::{Duration, Instant};

use gpui::prelude::*;
use gpui::{
    div, point, px, size, App, Bounds, ClipboardItem, Context, DispatchPhase, Element, ElementId,
    ElementInputHandler, Entity, EntityInputHandler, EventEmitter, FocusHandle, Focusable,
    FontWeight, GlobalElementId, HitboxBehavior, InspectorElementId, IntoElement, KeyDownEvent,
    LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, Render,
    SharedString, Style, TextAlign, TextRun, UTF16Selection, Window,
};

use crate::editor::theme;

/// 输入框对宿主（`AppShell`）的事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenameEvent {
    /// 用户按 `Enter`，且标题非空：载荷是**trim 后**的新标题。
    ///
    /// [`RenameMode::Cell`] 下载荷是**原始**文本（允许空串，空格不 trim）。
    Commit(String),
    /// 标题为空/纯空白：宿主应提示并保持编辑（仅 [`RenameMode::Title`]）。
    Blank,
    /// 用户取消（`Esc`、外部点击等）。
    Cancel,
    /// [`RenameMode::Cell`] 下按了 `Tab` / `Shift+Tab`：提交这一格并跳到下一格/上一格。
    Advance {
        /// `true` = Tab（向后），`false` = Shift+Tab（向前）。
        forward: bool,
        /// 按键**当时**的文本（平台随后补发的 `\t` 不能再算进来）。
        text: String,
    },
    /// 文本变了（键入 / 删除 / 粘贴 / IME 上屏）。宿主用它同步草稿文本，
    /// 关闭/切换文档时不必再借 `Window` 读取输入框。
    Changed,
}

/// 输入框的用途：决定 Enter 提交语义与 `Tab` 行为。
///
/// 侧栏标题与表格单元格共用**同一套** IME/选区/光标本体；差异只在这里，避免复制
/// 第二份 `EntityInputHandler`。**初始选区由具体构造器决定**（侧栏标题全选、
/// 笔记大标题/设置字段不预选、单元格在末尾）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenameMode {
    /// 标题：`Enter` 提交 trim 后非空标题，空标题拒绝。
    Title,
    /// 表格单元格：光标默认在文本末尾；`Enter` 提交原始文本（空串也合法），
    /// `Tab`/`Shift+Tab` 提交并跳格。
    Cell,
}

/// 行内编辑的纯状态：文本 + 锚点/光标（byte 偏移）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RenameState {
    text: String,
    anchor: usize,
    caret: usize,
    /// IME 组合串（只显示，不进 `text`）。
    marked: Option<MarkedText>,
}

/// 一次组合：从 `start..end` 那段被 preedit 覆盖。
#[derive(Debug, Clone, PartialEq, Eq)]
struct MarkedText {
    start: usize,
    end: usize,
    preedit: String,
}

impl RenameState {
    /// 新建并**全选**（进入编辑时的默认状态）。
    #[must_use]
    pub fn selecting_all(text: impl Into<String>) -> Self {
        let text = text.into();
        let end = text.len();
        Self {
            text,
            anchor: 0,
            caret: end,
            marked: None,
        }
    }

    /// 新建并把光标放在**末尾**（表格单元格：单击后从格尾继续输入）。
    #[must_use]
    pub fn at_end(text: impl Into<String>) -> Self {
        let text = text.into();
        let caret = text.len();
        Self {
            text,
            anchor: caret,
            caret,
            marked: None,
        }
    }

    /// 真实标题（不含 preedit）。
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// 显示用的串：组合处插入 preedit。
    #[must_use]
    pub fn display(&self) -> String {
        match &self.marked {
            Some(marked) if !marked.preedit.is_empty() => {
                let mut out = String::with_capacity(self.text.len() + marked.preedit.len());
                out.push_str(&self.text[..marked.start]);
                out.push_str(&marked.preedit);
                out.push_str(&self.text[marked.end..]);
                out
            }
            _ => self.text.clone(),
        }
    }

    /// 选中的 byte 范围（起点 ≤ 终点）。
    #[must_use]
    pub fn selected_byte_range(&self) -> Range<usize> {
        let (start, end) = if self.anchor <= self.caret {
            (self.anchor, self.caret)
        } else {
            (self.caret, self.anchor)
        };
        start..end.min(self.text.len())
    }

    /// 光标（byte）。
    #[must_use]
    pub fn caret(&self) -> usize {
        self.caret
    }

    /// 选中范围是否反向（锚点在后）。
    #[must_use]
    pub fn is_reversed(&self) -> bool {
        self.anchor > self.caret
    }

    /// 全选。
    pub fn select_all(&mut self) {
        self.anchor = 0;
        self.caret = self.text.len();
    }

    /// 把光标放到 `byte` 并把选区折叠成空（点击定位用）。
    ///
    /// 越界夹到文本末尾；落在多字节字符中间时向前夹到最近的 UTF-8 字符边界
    /// （UTF-8 边界同时保证 UTF-16 边界，平台坐标不会出现半个代理对）。
    pub fn set_caret(&mut self, byte: usize) {
        let byte = clamp_boundary(&self.text, byte);
        self.anchor = byte;
        self.caret = byte;
    }

    /// 拖选：`anchor` 固定、`head` 跟随指针（允许反向），两端都夹到字符边界。
    ///
    /// 与 [`Self::set_caret`] 的区别只在于不折叠：head 和 anchor 可以不同，
    /// `anchor > head` 即反向选择（`is_reversed()`）。
    pub fn set_selection(&mut self, anchor: usize, head: usize) {
        self.anchor = clamp_boundary(&self.text, anchor);
        self.caret = clamp_boundary(&self.text, head);
    }

    /// 把选区（或光标处）替换成 `text`；换行/制表符会被**过滤掉**（单行标题）。
    pub fn insert(&mut self, text: &str) {
        let filtered: String = text
            .chars()
            .filter(|ch| *ch != '\n' && *ch != '\r' && *ch != '\t')
            .collect();
        if filtered.is_empty() {
            return;
        }
        self.replace_selection(&filtered);
    }

    /// [`Self::insert`] 的表格单元格版本：换行/制表符换成**一个空格**而不是丢弃
    /// （粘贴多行文本时保留词间分隔；真正的单行/转义由控制器提交时再兜一层）。
    pub fn insert_cell(&mut self, text: &str) {
        let filtered: String = text
            .chars()
            .map(|ch| {
                if matches!(ch, '\n' | '\r' | '\t') {
                    ' '
                } else {
                    ch
                }
            })
            .collect();
        self.replace_selection(&filtered);
    }

    /// 把已经清理过的单行文本替换进选区；空串 = 删除选区。
    fn replace_selection(&mut self, filtered: &str) {
        if filtered.is_empty() {
            let range = self.selected_byte_range();
            if range.start == range.end {
                return;
            }
            self.text.replace_range(range.clone(), "");
            self.anchor = range.start;
            self.caret = range.start;
            self.marked = None;
            return;
        }
        let range = self.selected_byte_range();
        self.text.replace_range(range.clone(), filtered);
        let caret = range.start + filtered.len();
        self.anchor = caret;
        self.caret = caret;
        self.marked = None;
    }

    /// 退格：有选区删选区，否则删前一个字符。
    pub fn backspace(&mut self) {
        let range = self.selected_byte_range();
        if range.start != range.end {
            self.text.replace_range(range.clone(), "");
            self.anchor = range.start;
            self.caret = range.start;
            return;
        }
        if self.caret == 0 {
            return;
        }
        let start = prev_boundary(&self.text, self.caret);
        self.text.replace_range(start..self.caret, "");
        self.anchor = start;
        self.caret = start;
    }

    /// 删除：有选区删选区，否则删后一个字符。
    pub fn delete_forward(&mut self) {
        let range = self.selected_byte_range();
        if range.start != range.end {
            self.text.replace_range(range.clone(), "");
            self.anchor = range.start;
            self.caret = range.start;
            return;
        }
        if self.caret >= self.text.len() {
            return;
        }
        let end = next_boundary(&self.text, self.caret);
        self.text.replace_range(self.caret..end, "");
        self.anchor = self.caret;
    }

    /// 左移；`extend` 表示扩展选区（Shift）。
    pub fn move_left(&mut self, extend: bool) {
        let range = self.selected_byte_range();
        let target = if !extend && range.start != range.end {
            range.start
        } else {
            prev_boundary(&self.text, self.caret.min(self.text.len()))
        };
        self.caret = target;
        if !extend {
            self.anchor = target;
        }
    }

    /// 右移；`extend` 表示扩展选区（Shift）。
    pub fn move_right(&mut self, extend: bool) {
        let range = self.selected_byte_range();
        let target = if !extend && range.start != range.end {
            range.end
        } else {
            next_boundary(&self.text, self.caret.min(self.text.len()))
        };
        self.caret = target;
        if !extend {
            self.anchor = target;
        }
    }

    /// Home。
    pub fn home(&mut self, extend: bool) {
        self.caret = 0;
        if !extend {
            self.anchor = 0;
        }
    }

    /// End。
    pub fn end(&mut self, extend: bool) {
        self.caret = self.text.len();
        if !extend {
            self.anchor = self.caret;
        }
    }

    /// 选中范围换算成 UTF-16（平台坐标）。
    #[must_use]
    pub fn selection_utf16(&self) -> Range<usize> {
        let range = self.selected_byte_range();
        byte_to_utf16(&self.text, range.start)..byte_to_utf16(&self.text, range.end)
    }

    /// UTF-16 → byte（落在代理对中间返回 `None`）。
    #[must_use]
    pub fn utf16_to_byte(&self, utf16: usize) -> Option<usize> {
        utf16_to_byte(&self.display(), utf16)
    }

    /// 组合串在"显示串"里的 UTF-16 范围。
    #[must_use]
    pub(crate) fn marked_utf16(&self) -> Option<Range<usize>> {
        let marked = self.marked.as_ref()?;
        if marked.preedit.is_empty() {
            return None;
        }
        let start = byte_to_utf16(&self.text, marked.start);
        Some(start..start + marked.preedit.encode_utf16().count())
    }

    /// trim 后的提交值；空/纯空白返回 `None`。
    #[must_use]
    pub fn trimmed_title(&self) -> Option<String> {
        let trimmed = self.text.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_owned())
        }
    }

    /// 开始/更新一次组合（`preedit` 为空 = 结束组合）。
    pub fn set_marked(&mut self, preedit: &str) {
        if preedit.is_empty() {
            self.marked = None;
            return;
        }
        let range = self.selected_byte_range();
        match &mut self.marked {
            Some(marked) => marked.preedit = preedit.to_owned(),
            None => {
                self.marked = Some(MarkedText {
                    start: range.start,
                    end: range.end,
                    preedit: preedit.to_owned(),
                });
            }
        }
    }

    /// 清掉组合（上屏/取消时）。
    pub fn clear_marked(&mut self) {
        self.marked = None;
    }

    /// 提交组合串：把组合覆盖区替换成 `text`。
    pub fn commit_marked(&mut self, text: &str) {
        let Some(marked) = self.marked.take() else {
            self.insert(text);
            return;
        };
        let filtered: String = text
            .chars()
            .filter(|ch| *ch != '\n' && *ch != '\r' && *ch != '\t')
            .collect();
        self.text.replace_range(marked.start..marked.end, &filtered);
        let caret = marked.start + filtered.len();
        self.anchor = caret;
        self.caret = caret;
    }

    /// [`Self::commit_marked`] 的表格单元格版本：换行/制表符换成空格。
    pub fn commit_marked_cell(&mut self, text: &str) {
        let Some(marked) = self.marked.take() else {
            self.insert_cell(text);
            return;
        };
        let filtered: String = text
            .chars()
            .map(|ch| {
                if matches!(ch, '\n' | '\r' | '\t') {
                    ' '
                } else {
                    ch
                }
            })
            .collect();
        self.text.replace_range(marked.start..marked.end, &filtered);
        let caret = marked.start + filtered.len();
        self.anchor = caret;
        self.caret = caret;
    }
}

/// 上一个字符边界。
fn prev_boundary(text: &str, at: usize) -> usize {
    text[..at]
        .char_indices()
        .next_back()
        .map_or(0, |(index, _)| index)
}

/// 把任意 byte 夹进 `text` 的范围内，并向前对齐到 UTF-8 字符边界。
///
/// shaping 给回来的字节索引是"最近的字符边界"，但布局可能落后一帧（刚键入/删除时
/// 文本已经变了），这里再兜一层：越界夹到末尾，落在多字节字符中间就向前夹。
fn clamp_boundary(text: &str, byte: usize) -> usize {
    let mut byte = byte.min(text.len());
    while byte > 0 && !text.is_char_boundary(byte) {
        byte -= 1;
    }
    byte
}

/// 下一个字符边界。
fn next_boundary(text: &str, at: usize) -> usize {
    text[at..]
        .char_indices()
        .nth(1)
        .map_or(text.len(), |(index, _)| at + index)
}

/// byte → UTF-16 偏移。
pub(crate) fn byte_to_utf16(text: &str, byte: usize) -> usize {
    text[..byte.min(text.len())].encode_utf16().count()
}

/// UTF-16 偏移 → byte（落点必须在字符边界上）。
pub(crate) fn utf16_to_byte(text: &str, utf16: usize) -> Option<usize> {
    let mut seen = 0;
    for (index, ch) in text.char_indices() {
        if seen == utf16 {
            return Some(index);
        }
        seen += ch.len_utf16();
        if seen > utf16 {
            return None;
        }
    }
    (seen == utf16).then_some(text.len())
}

/// 拖选判定：指针按下后移动超过这个距离才算"拖"（否则仍按单击定位处理）。
///
/// 用与侧栏行拖拽相同的 2px 口径：触控板/鼠标的轻微抖动不应该冒出一两个字符的选区。
const CLICK_DRAG_THRESHOLD: f32 = 2.0;

/// caret 闪烁半周期：先亮 [`CARET_BLINK_INTERVAL`]，再灭同样时长（与正文编辑器一致）。
///
/// 0.53s 是常见文本光标的节奏（Windows 默认约 530ms 闪烁一次）。相位由
/// [`RenameInput::spawn_caret_blink`] 的唯一 timer 循环翻转，任何输入/点击/移动
/// 都会经 [`RenameInput::mark_caret_active`] 回到"可见"并重启计时。
pub const CARET_BLINK_INTERVAL: Duration = Duration::from_millis(530);

/// `paint` 当帧的文本布局：鼠标事件用它把窗口 x 换算成**显示串**上的字节索引。
///
/// 必须与 [`RenameElement::paint`] 的 shaping 参数完全一致（字体、字号、文本左内边距），
/// 否则"看到的位置"和"选中的位置"会差几个像素；DPI 缩放/字体回退下差异会被放大。
#[derive(Debug, Clone)]
struct RenameLayout {
    /// 本元素（`RenameElement`）当帧的窗口矩形；`origin.x + pad_x` 是文本 x=0。
    bounds: Bounds<Pixels>,
    /// 当帧实际 shaping 的显示串（IME 组合时含 preedit）。
    display: String,
    font_size: Pixels,
    pad_x: Pixels,
    font: gpui::Font,
}

/// 一次按住左键的拖选手势。
#[derive(Debug, Clone)]
struct PointerDrag {
    /// 按下点的窗口坐标：布局还没就绪时用它延后换算 anchor。
    anchor_position: Point<Pixels>,
    /// 按下点对应的显示串字节索引（`None` = 按下时还没有布局，等 paint/首次 move 再算）。
    anchor_byte: Option<usize>,
    /// 是否已越过 [`CLICK_DRAG_THRESHOLD`]（没越过的抬起仍保持单击定位）。
    moved: bool,
}

/// 行内重命名输入框（一个 GPUI 实体）。
pub struct RenameInput {
    state: RenameState,
    focus_handle: FocusHandle,
    mode: RenameMode,
    /// 笔记顶部的大标题：标题语义不变，只放大字号并去掉侧栏输入框的白底/边框。
    large_title: bool,
    /// 待定位的点击（窗口坐标）：下一次 `paint` 按当前 `shape_line` 换算成光标位置。
    ///
    /// 只用于大标题的"点哪放哪"；`None` 表示没有待处理点击。paint 应用后清空，
    /// 避免每帧重复定位。
    pending_click: Option<Point<Pixels>>,
    /// 上一帧的文本布局（`paint` 更新）：鼠标按下/拖动时据此换算 anchor/head 字节。
    last_layout: Option<RenameLayout>,
    /// 按住左键的拖选状态（`None` = 没按住；抬起即清空）。
    ///
    /// 它与 `pending_click` 互斥：一旦越过拖动阈值就清掉 `pending_click`，
    /// 保证 `paint` 不会把拖出来的选区又改回单击定位。
    pointer_drag: Option<PointerDrag>,
    /// 闪烁相位：当前半周期是否**可见**（进入编辑/任何输入后先亮 530ms）。
    caret_blink_visible: bool,
    /// 本半周期的起点，时间源是 `cx.background_executor().now()`
    /// （生产 = 单调时钟；headless = 测试调度器虚拟时钟，可用 `advance_clock` 推进）。
    caret_blink_epoch: Instant,
    /// 上一帧 `paint` 是否真的画了 caret（headless 回归用，不参与业务判断）。
    caret_painted_last_frame: bool,
    /// 上一帧 `paint` 时本输入框是否持有焦点。
    ///
    /// 闪烁循环据此决定要不要 `cx.notify()`：失焦时光标本来就不画，再每 530ms
    /// 叫醒一帧只是空转（设置页字段会长期存活）。
    focused_last_frame: bool,
}

impl RenameInput {
    /// 新建标题输入框：预填 `initial` 并**全选**。
    pub fn new(initial: &str, cx: &mut Context<Self>) -> Self {
        Self::new_with_mode(initial, RenameMode::Title, cx)
    }

    /// 新建表格单元格输入框：预填 `initial`，光标在**末尾**（单击单元格的默认落点）。
    pub fn new_cell_at_end(initial: &str, cx: &mut Context<Self>) -> Self {
        Self::new_with_mode(initial, RenameMode::Cell, cx)
    }

    /// 新建表格/属性输入框：Cell 语义（允许空串），但**全选**已有值，
    /// 便于直接覆盖属性值（Obsidian 属性面板的点击即替换）。
    pub fn new_cell_selected(initial: &str, cx: &mut Context<Self>) -> Self {
        Self::new_with_state(RenameMode::Cell, RenameState::selecting_all(initial), cx)
    }

    /// 新建笔记顶部大标题输入框：标题语义（空标题拒绝、Enter 提交），
    /// 但画成正文标题而不是侧栏输入框。
    ///
    /// **不预选**：初始选区是 collapsed 的。`click` = 触发编辑的鼠标位置
    /// （窗口坐标），第一次 `paint` 时按它把光标放到点击处；`None` = 光标在末尾。
    /// 这样用户点标题不会整段变蓝，也不会在输入框第一帧闪一下全选。
    pub fn new_note_title(
        initial: &str,
        click: Option<Point<Pixels>>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut input = Self::new_with_state(RenameMode::Title, RenameState::at_end(initial), cx);
        input.large_title = true;
        input.pending_click = click;
        // 同一只按下手势可能还没抬起就开始移动：把按下点也记成拖选候选。
        // 若只是单击，抬起（或下一次在别处的按下）会清掉它，行为与原来一致。
        input.pointer_drag = click.map(|position| PointerDrag {
            anchor_position: position,
            anchor_byte: None,
            moved: false,
        });
        input
    }

    /// 新建笔记后**直接进入改名**用的大标题输入框：初始**全选**默认标题。
    ///
    /// 与 [`Self::new_note_title`] 的区别只有初始选区：新建笔记的默认名
    /// （如「未命名笔记」）不是用户想要的，全选后一次输入就能整体替换；
    /// 点击进入编辑则不能预选（用户是来补充/修改，不是来覆盖）。
    pub fn new_note_title_selected(initial: &str, cx: &mut Context<Self>) -> Self {
        let mut input =
            Self::new_with_state(RenameMode::Title, RenameState::selecting_all(initial), cx);
        input.large_title = true;
        input
    }

    /// 新建**设置页**输入框：标题语义，但光标在末尾、不预选。
    ///
    /// 设置页第一次打开时字段可能已被自动聚焦；若沿用 `new` 的全选，用户会看到
    /// 「所有文字都被选中」的蓝底。这里只放光标，用户自己点击/拖选才出现选区。
    pub fn new_at_end(initial: &str, cx: &mut Context<Self>) -> Self {
        Self::new_with_state(RenameMode::Title, RenameState::at_end(initial), cx)
    }

    fn new_with_mode(initial: &str, mode: RenameMode, cx: &mut Context<Self>) -> Self {
        let state = match mode {
            RenameMode::Title => RenameState::selecting_all(initial),
            RenameMode::Cell => RenameState::at_end(initial),
        };
        Self::new_with_state(mode, state, cx)
    }

    /// 所有构造器（侧栏标题 / 表格单元格 / 设置字段 / 大标题）都从这里出生：
    /// 初始相位是**可见**，并启动唯一那条闪烁 timer。
    fn new_with_state(mode: RenameMode, state: RenameState, cx: &mut Context<Self>) -> Self {
        let input = Self {
            state,
            focus_handle: cx.focus_handle(),
            mode,
            large_title: false,
            pending_click: None,
            last_layout: None,
            pointer_drag: None,
            caret_blink_visible: true,
            caret_blink_epoch: cx.background_executor().now(),
            caret_painted_last_frame: false,
            focused_last_frame: false,
        };
        input.spawn_caret_blink(cx);
        input
    }

    /// 启动**唯一**的闪烁计时循环：每半个周期翻转相位，实体释放即退出。
    ///
    /// 每轮都读 `caret_blink_remaining()` 而不是固定 `timer(530ms)`：
    /// 用户输入会把 [`Self::caret_blink_epoch`] 往后推，正在等待的那一拍到点后
    /// 发现"还剩 x ms"，就只续等 x，于是下一次翻转精确落在"最后一次输入 + 530ms"，
    /// 既不会刚打完字就灭，也不会把整个半周期再拖长一倍。
    ///
    /// 失焦时仍然翻转相位（无副作用，`paint` 不看它），但**不** `cx.notify()`：
    /// 设置页/侧栏里长期存活的输入框不该在没人看的时候每 530ms 空转一帧。
    fn spawn_caret_blink(&self, cx: &mut Context<Self>) {
        let timers = cx.background_executor().clone();
        cx.spawn(async move |this, cx| {
            loop {
                let Ok(wait) = this.update(cx, |input, cx| {
                    input.caret_blink_remaining(cx.background_executor().now())
                }) else {
                    // 实体已释放（窗口关闭/退出编辑）：结束循环，别继续 await。
                    break;
                };
                timers.timer(wait).await;
                let toggled = this.update(cx, |input, cx| {
                    let now = cx.background_executor().now();
                    if !input.caret_blink_remaining(now).is_zero() {
                        // 等待期间被输入/点击/选区移动重置过：这一拍不翻转，
                        // 回到循环顶部按新起点剩余时间续等。
                        return;
                    }
                    input.caret_blink_visible = !input.caret_blink_visible;
                    input.caret_blink_epoch = now;
                    if input.focused_last_frame {
                        cx.notify();
                    }
                });
                if toggled.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    /// 当前半周期还剩多久（已到点 = `Duration::ZERO`，不会返回负数）。
    fn caret_blink_remaining(&self, now: Instant) -> Duration {
        blink_remaining(now, self.caret_blink_epoch)
    }

    /// 把 caret 重置为**可见**并重启计时。
    ///
    /// 任何"用户刚动过光标"的路径都要调用：键入/粘贴/IME 组合与上屏、鼠标按下/
    /// 拖选/抬起、聚焦或进入编辑、光标键与 Home/End。闪烁循环只负责翻转，复位在
    /// 这里，因此用户不会在刚打完字或刚点完位置时看到光标熄灭。
    fn mark_caret_active(&mut self, cx: &mut Context<Self>) {
        self.caret_blink_visible = true;
        self.caret_blink_epoch = cx.background_executor().now();
        // 隐藏相位下的输入必须立刻把光标画回来，所以这里总是请求重绘。
        cx.notify();
    }

    /// caret 当前是否处于"可见"半周期（headless 回归/诊断用）。
    #[must_use]
    pub fn caret_blink_visible(&self) -> bool {
        self.caret_blink_visible
    }

    /// 上一帧 `paint` 是否真的画了 caret（headless 回归用：隐藏相位必须为 `false`）。
    #[must_use]
    pub fn caret_painted_last_frame(&self) -> bool {
        self.caret_painted_last_frame
    }

    /// 是否是笔记顶部大标题。
    #[must_use]
    pub fn is_large_title(&self) -> bool {
        self.large_title
    }

    /// 输入框用途（绘制尺寸/提交语义都从这里来）。
    #[must_use]
    pub fn mode(&self) -> RenameMode {
        self.mode
    }

    /// 当前真实标题（不含 preedit）。
    #[must_use]
    pub fn text(&self) -> &str {
        self.state.text()
    }

    /// 输入框当前是否持有键盘焦点（测试/诊断用）。
    #[must_use]
    pub fn is_focused(&self, window: &Window) -> bool {
        self.focus_handle.is_focused(window)
    }
    /// 宿主转发的文本输入（正常路径不会用到：输入框自己注册了 `EntityInputHandler`）。
    ///
    /// 只作为“平台把文本送错焦点”的兜底：仍走同一套单行清理，不产生第二套输入实现。
    pub fn insert_from_host(&mut self, text: &str, cx: &mut Context<Self>) {
        match self.mode {
            RenameMode::Title => self.state.insert(text),
            RenameMode::Cell => self.state.insert_cell(text),
        }
        self.mark_caret_active(cx);
        cx.emit(RenameEvent::Changed);
    }

    /// 状态（单测/宿主用）。
    #[must_use]
    pub fn state(&self) -> &RenameState {
        &self.state
    }

    /// 把焦点给输入框（并让 caret 立刻可见、重新计时）。
    pub fn focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus_handle, cx);
        self.mark_caret_active(cx);
    }

    /// 点击设置字段时聚焦，光标落在末尾、**不选中**。
    ///
    /// 之前这里会 `select_all`，导致设置页里点一下字段整段文字变蓝（用户多次反馈）；
    /// 设置字段是「继续编辑已有值」而不是「替换整个标题」，所以只放光标。
    /// 需要全选时用 `Ctrl+A` 或拖选。
    pub fn focus_for_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.focus(window, cx);
        self.state.end(false);
        self.mark_caret_active(cx);
    }

    /// 本帧布局里窗口 x 对应的**显示串**字节索引；布局还没画过时返回 `None`。
    ///
    /// 复用 `paint` 的 shaping 规则（同字体/字号/左内边距），所以点哪、拖到哪与画出来
    /// 的字形位置一致。`closest_index_for_x` 自己会把 x 夹到 `[0, len]`，指针拖出元素
    /// 边界也不会 panic；返回的字节随后还要过 [`clamp_boundary`]（布局可能落后一帧）。
    fn byte_at_x(&self, x: Pixels, window: &Window) -> Option<usize> {
        let layout = self.last_layout.as_ref()?;
        let shaped = shape_display_line(&layout.display, layout.font_size, &layout.font, window);
        Some(shaped.closest_index_for_x(x - layout.bounds.origin.x - layout.pad_x))
    }

    /// 左键按在输入框上：聚焦、按位置折叠光标，并开始一段拖选候选。
    ///
    /// 布局已就绪时**立即**定位（不必等下一帧）；布局未就绪（大标题输入框在这个按下的
    /// 同一帧才被创建）仍写 `pending_click`，由第一次 `paint` 用同一套 shaping 换算，
    /// 换算出的字节同时回填成拖选的 anchor —— 于是同一只按下手势也能继续拖出选区。
    fn begin_pointer_down(
        &mut self,
        position: Point<Pixels>,
        click_count: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus(window, cx);
        // IME 组合中：这一次点击交给输入法处理（平台会自己更新组合/选区），
        // 我们不接管、也不动 marked。
        if self.state.marked_utf16().is_some() {
            self.pointer_drag = None;
            return;
        }
        let byte = self.byte_at_x(position.x, window).map(|byte| {
            if click_count >= 2 {
                // 双击选词：按字符类别选连续词（中文连续字、ASCII 单词/下划线）。
                let (start, end) = word_range_at(self.state.text(), byte);
                self.state.set_selection(start, end);
                self.state.caret()
            } else {
                self.state.set_caret(byte);
                self.state.caret()
            }
        });
        self.pending_click = if byte.is_none() { Some(position) } else { None };
        self.pointer_drag = if click_count >= 2 {
            // 双击已经形成词选区；不要再让随后的轻微鼠标移动把它塌成 caret。
            None
        } else {
            Some(PointerDrag {
                anchor_position: position,
                anchor_byte: byte,
                moved: false,
            })
        };
        // 点哪放哪之后正好是"用户动过光标"：立即可见并重启计时。
        self.mark_caret_active(cx);
    }

    /// 窗口级左键按下兜底：出现新的按下，说明上一段手势（以及它的拖选候选）已经结束。
    ///
    /// 极快的单击、在窗口外释放等情况下 `mouse up` 可能落在监听注册之前而收不到；
    /// 不清理的话，之后在别处拖动会把旧 anchor 拿来扩选。输入框内部的按下随后会被
    /// 它自己的 `on_mouse_down` 重新置成新候选。
    fn on_window_mouse_down(&mut self, event: &MouseDownEvent) {
        if event.button != MouseButton::Left {
            return;
        }
        if self.pointer_drag.as_ref().is_some_and(|drag| drag.moved) {
            // 已在拖动中又收到按下（多键/异常输入）：不要破坏当前手势。
            return;
        }
        self.pointer_drag = None;
    }

    /// 按住左键移动：把选区 head 更新到指针所在字符（可反向、可拖出元素边界）。
    fn on_pointer_move(&mut self, event: &MouseMoveEvent, window: &Window, cx: &mut Context<Self>) {
        let Some(drag) = self.pointer_drag.as_ref() else {
            return;
        };
        if !event.dragging() {
            // 没按住左键：上一次手势已经结束（up 可能丢在窗口外），丢弃候选。
            self.pointer_drag = None;
            return;
        }
        // IME 组合串出现后 display 与 text 的字节索引不再一致：拖选让位给输入法。
        if self.state.marked_utf16().is_some() {
            self.pointer_drag = None;
            return;
        }
        let anchor_position = drag.anchor_position;
        let mut anchor_byte = drag.anchor_byte;
        let mut moved = drag.moved;
        if anchor_byte.is_none() {
            anchor_byte = self.byte_at_x(anchor_position.x, window);
        }
        let Some(anchor_byte) = anchor_byte else {
            return;
        };
        if !moved {
            let dx = (event.position.x - anchor_position.x).as_f32().abs();
            let dy = (event.position.y - anchor_position.y).as_f32().abs();
            if dx < CLICK_DRAG_THRESHOLD && dy < CLICK_DRAG_THRESHOLD {
                // 阈值内的抖动仍按单击处理：保持按下位置的折叠光标。
                return;
            }
            moved = true;
            // 拖选开始后和 `pending_click` 互斥：否则下一次 paint 会把选区改回单击定位。
            self.pending_click = None;
        }
        let Some(head) = self.byte_at_x(event.position.x, window) else {
            return;
        };
        if let Some(drag) = self.pointer_drag.as_mut() {
            drag.anchor_byte = Some(anchor_byte);
            drag.moved = moved;
        }
        self.state.set_selection(anchor_byte, head);
        // 拖选 = 选区/caret 在动：光标保持可见并重新计时（530ms 后才允许熄灭）。
        self.mark_caret_active(cx);
    }

    /// 左键抬起：结束拖选；**保留**已拖出的选区（没拖动过的按下仍是折叠光标）。
    fn on_pointer_up(&mut self, event: &MouseUpEvent, window: &Window, cx: &mut Context<Self>) {
        let Some(drag) = self.pointer_drag.take() else {
            return;
        };
        // 组合串出现后 display 与 text 的字节索引不一致：不再按指针改选区（交给输入法）。
        if !drag.moved || self.state.marked_utf16().is_some() {
            return;
        }
        // 抬起位置可能没有对应的 move 事件（一次快速拖拽），按抬起点再收一次。
        let Some(anchor_byte) = drag.anchor_byte else {
            return;
        };
        if let Some(head) = self.byte_at_x(event.position.x, window) {
            self.state.set_selection(anchor_byte, head);
            // 抬起是这段手势的最后一次位置更新：同样复位，用户松手后光标不会立刻灭。
            self.mark_caret_active(cx);
        }
    }

    /// `paint` 里应用一次待定位点击；返回是否真的移动了光标。
    ///
    /// 不管是否应用都清空 pending：一次点击只定位一次。组合态出现时说明输入法
    /// 接管了这次点击，直接丢弃，不能让它在组合结束后突然生效；拖选已经开始时
    /// 同样丢弃，绝不能把拖出来的选区改回单击定位。
    fn apply_pending_click(&mut self, byte: usize) -> bool {
        if self.pending_click.take().is_none() {
            return false;
        }
        if self.state.marked_utf16().is_some() {
            return false;
        }
        if self.pointer_drag.as_ref().is_some_and(|drag| drag.moved) {
            return false;
        }
        self.state.set_caret(byte);
        let caret = self.state.caret();
        // 大标题由这次按下创建时，anchor 就是这次点击的位置（同一手势可继续拖动）。
        if let Some(drag) = self.pointer_drag.as_mut() {
            drag.anchor_byte.get_or_insert(caret);
        }
        true
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        let modifiers = event.keystroke.modifiers;
        let extend = modifiers.shift;

        // 任何按键（键入、粘贴、方向键/Home/End、Enter/Esc 前的最后一次操作）都算
        // "用户刚动过光标"：先把 caret 复位成可见并重启计时，再走各自的语义。
        self.mark_caret_active(cx);

        // 复制/剪切：标题与表格单元格都支持 `Ctrl+C` / `Ctrl+X`（macOS Cmd 同义）。
        // 映射层仍把 C/X 视为“搜索框/向导不支持”，只有本输入框在映射前拦截。
        if crate::text_shortcuts::copy_shortcut(
            &event.keystroke,
            self.state.marked_utf16().is_some(),
        ) {
            let range = self.state.selected_byte_range();
            if range.start != range.end {
                let text = self.state.text()[range].to_owned();
                cx.write_to_clipboard(ClipboardItem::new_string(text));
            }
            window.prevent_default();
            return;
        }
        if crate::text_shortcuts::cut_shortcut(
            &event.keystroke,
            self.state.marked_utf16().is_some(),
        ) {
            let range = self.state.selected_byte_range();
            if range.start != range.end {
                let text = self.state.text()[range].to_owned();
                cx.write_to_clipboard(ClipboardItem::new_string(text));
                // 复用 backspace 的选区删除语义：只删一次、光标落到选区起点。
                self.state.backspace();
                cx.emit(RenameEvent::Changed);
                cx.notify();
            }
            window.prevent_default();
            return;
        }

        // 粘贴：标题与表格单元格都支持 `Ctrl+V` / `Cmd+V`（`Ctrl+Shift+V` 同义）。
        // 单行过滤在 `RenameState::insert` / `insert_cell` 里；**不记录剪贴板内容**。
        if crate::text_shortcuts::paste_shortcut(
            &event.keystroke,
            self.state.marked_utf16().is_some(),
        ) {
            if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                match self.mode {
                    RenameMode::Title => self.state.insert(&text),
                    RenameMode::Cell => self.state.insert_cell(&text),
                }
                cx.emit(RenameEvent::Changed);
                cx.notify();
            }
            window.prevent_default();
            return;
        }

        // 与搜索框共用同一套快捷键语义（`Ctrl+A` 全选；组合态忽略）。
        match crate::text_shortcuts::shortcut_action(
            &event.keystroke,
            self.state.marked_utf16().is_some(),
        ) {
            crate::text_shortcuts::ShortcutAction::SelectAll => {
                self.state.select_all();
                cx.notify();
                window.prevent_default();
                return;
            }
            crate::text_shortcuts::ShortcutAction::ClipboardUnsupported => {
                window.prevent_default();
                return;
            }
            crate::text_shortcuts::ShortcutAction::None => {}
        }

        // 调试入口（仅 debug 构建）：与编辑器正文同一条 IME 语义（组合 → 上屏）。
        // 只对表格单元格开放，侧栏重命名保持原样。
        #[cfg(debug_assertions)]
        if self.mode == RenameMode::Cell
            && self.state.marked_utf16().is_none()
            && !modifiers.control
            && !modifiers.alt
            && !modifiers.platform
            && key == "f9"
        {
            self.state.set_marked("zhongwen");
            self.state.commit_marked_cell("中文");
            cx.emit(RenameEvent::Changed);
            cx.notify();
            window.prevent_default();
            return;
        }

        match key {
            "enter" => {
                if self.state.marked.is_some() {
                    // 组合中按 Enter 是"上屏"，交给 IME，不当提交。
                    return;
                }
                match self.mode {
                    RenameMode::Title => {
                        if let Some(title) = self.state.trimmed_title() {
                            cx.emit(RenameEvent::Commit(title));
                        } else {
                            cx.emit(RenameEvent::Blank);
                        }
                    }
                    // 单元格允许提交空串（清空格子），空格原样保留。
                    RenameMode::Cell => {
                        cx.emit(RenameEvent::Commit(self.state.text().to_owned()));
                    }
                }
            }
            "tab" if self.mode == RenameMode::Cell => {
                if self.state.marked.is_some() {
                    // 组合中 Tab 属于输入法（候选选择），不跳格。
                    return;
                }
                cx.emit(RenameEvent::Advance {
                    forward: !extend,
                    text: self.state.text().to_owned(),
                });
            }
            "escape" => {
                if self.state.marked.is_some() {
                    // 组合中：先放弃组合串（平台通常会先吃掉这次 Esc，到这里说明没有组合）。
                    self.state.clear_marked();
                    cx.notify();
                    return;
                }
                cx.emit(RenameEvent::Cancel);
            }
            "backspace" => {
                self.state.backspace();
                cx.emit(RenameEvent::Changed);
                cx.notify();
            }
            "delete" => {
                self.state.delete_forward();
                cx.emit(RenameEvent::Changed);
                cx.notify();
            }
            "left" => {
                self.state.move_left(extend);
                cx.notify();
            }
            "right" => {
                self.state.move_right(extend);
                cx.notify();
            }
            "home" => {
                self.state.home(extend);
                cx.notify();
            }
            "end" => {
                self.state.end(extend);
                cx.notify();
            }
            _ => {}
        }
        window.prevent_default();
    }
}

impl Focusable for RenameInput {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<RenameEvent> for RenameInput {}

impl EntityInputHandler for RenameInput {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let display = self.state.display();
        let start = utf16_to_byte(&display, range_utf16.start)?;
        let end = utf16_to_byte(&display, range_utf16.end)?;
        *adjusted_range = Some(range_utf16);
        display.get(start..end).map(str::to_owned)
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let range = self
            .state
            .marked_utf16()
            .unwrap_or_else(|| self.state.selection_utf16());
        Some(UTF16Selection {
            range,
            reversed: self.state.is_reversed(),
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.state.marked_utf16()
    }

    fn unmark_text(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.state.clear_marked();
        // 组合被取消（Esc/点击别处）后光标要立刻回来，不能停在"组合中常亮"的残留状态。
        self.mark_caret_active(cx);
    }

    fn replace_text_in_range(
        &mut self,
        _range: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        oneline_trace("上屏", text);
        let marked = self.state.marked.is_some();
        match (self.mode, marked) {
            (RenameMode::Title, true) => self.state.commit_marked(text),
            (RenameMode::Title, false) => self.state.insert(text),
            (RenameMode::Cell, true) => self.state.commit_marked_cell(text),
            (RenameMode::Cell, false) => self.state.insert_cell(text),
        }
        // IME 上屏 / 普通文本输入：光标回到可见并重新计时。
        self.mark_caret_active(cx);
        cx.emit(RenameEvent::Changed);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _range: Option<Range<usize>>,
        new_text: &str,
        _new_selected_range: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        oneline_trace("组合", new_text);
        self.state.set_marked(new_text);
        // 组合中 caret 本来就常亮（见 `paint`），复位只为"组合结束后仍亮满 530ms"。
        self.mark_caret_active(cx);
    }

    fn bounds_for_range(
        &mut self,
        _range_utf16: Range<usize>,
        element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        // 单行：候选窗就放在这一行上（平台靠它定位 IME 窗口）。
        Some(element_bounds)
    }

    fn character_index_for_point(
        &mut self,
        _point: gpui::Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        Some(self.state.selection_utf16().start)
    }
}

/// 组合/上屏日志：只在 `MENGHUNOTE_TRACE_MENU=1` 时输出。
fn oneline_trace(kind: &str, text: &str) {
    if crate::shell::trace_menu_enabled() {
        eprintln!("[menghunote] 重命名 {kind} text={text:?}");
    }
}

impl Render for RenameInput {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::on_key_down))
            // 点输入框**内部**不是"点外部"：拦住冒泡，别让行的左键处理器把它当成确认。
            // 按下同时聚焦、按位置落光标，并开始一段拖选候选；拖动/抬起由窗口级
            // 监听继续处理（拖出输入框也要更新选区）。
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|input, event: &MouseDownEvent, window, cx| {
                    input.begin_pointer_down(event.position, event.click_count, window, cx);
                    cx.stop_propagation();
                }),
            )
            // 与正文一致的文本光标。
            .cursor_text()
            .child(RenameElement { input: cx.entity() })
    }
}

/// 只负责"画一行文本 + 选区 + 光标 + 注册输入处理器"的元素。
struct RenameElement {
    input: Entity<RenameInput>,
}

impl IntoElement for RenameElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for RenameElement {
    type RequestLayoutState = ();
    type PrepaintState = ();

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
        let mut style = Style::default();
        style.size.width = gpui::relative(1.0).into();
        // 大标题编辑态必须占满 48px 行高：0 高度会让 paint 的 top_pad=0，
        // 文字相对非编辑态向上偏 7px（用户反馈“点击后标题上移”）。单元格浮层
        // 自身有固定矩形，保持 0 高度。
        style.size.height = if self.input.read(cx).is_large_title() {
            gpui::relative(1.0).into()
        } else {
            gpui::relative(0.0).into()
        };
        (window.request_layout(style, None, cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        _cx: &mut App,
    ) -> Self::PrepaintState {
        window.insert_hitbox(bounds, HitboxBehavior::Normal);
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let mut state = self.input.read(cx).state.clone();
        let mode = self.input.read(cx).mode();
        let large_title = self.input.read(cx).is_large_title();
        let pending_click = self.input.read(cx).pending_click;
        let display = state.display();
        let mut font = theme::font();
        if large_title {
            // 非编辑态标题是 28px BOLD；编辑态必须同字重，否则点击后宽度/基线会跳。
            font.weight = FontWeight::BOLD;
        }
        // 标题与表格单元格只差尺寸：单元格更小，并在 cell 行高里垂直居中
        // （浮层矩形就是 cell 矩形，不居中会让文字贴顶）。
        let (font_size, line_height, pad_x, top_pad) = match mode {
            RenameMode::Title if large_title => (
                px(28.0),
                px(34.0),
                px(0.0),
                ((bounds.size.height - px(34.0)) / 2.0).max(px(0.0)),
            ),
            RenameMode::Title => (
                px(theme::FONT_SIZE - 1.0),
                px(theme::LINE_HEIGHT),
                px(PAD_X),
                px(0.0),
            ),
            RenameMode::Cell => {
                let line_height = px(theme::line_height_for(theme::TABLE_FONT_SIZE));
                let top_pad = ((bounds.size.height - line_height) / 2.0).max(px(0.0));
                (
                    px(theme::TABLE_FONT_SIZE),
                    line_height,
                    px(theme::TABLE_CELL_PAD_X),
                    top_pad,
                )
            }
        };

        // 背景：**不透明白底** + 中性边框。输入框必须压住行的选中底色
        // （行可能是蓝色"当前打开"态），否则两层蓝叠在一起看不出哪段是选中。
        // 笔记大标题是正文的一部分，不要白底/边框。
        if !large_title {
            let inner = Bounds::new(
                point(bounds.origin.x + px(1.0), bounds.origin.y + px(1.0)),
                size(bounds.size.width - px(2.0), bounds.size.height - px(2.0)),
            );
            window.paint_quad(gpui::fill(inner, theme::input_bg()));
            paint_border(window, bounds, theme::input_border());
        }

        let shaped = shape_display_line(&display, font_size, &font, window);
        let origin = point(bounds.origin.x + pad_x, bounds.origin.y + top_pad);

        // 记录本帧布局：鼠标事件（按下/拖动/抬起）用它、并与绘制用**完全相同**的
        // shaping 把窗口 x 换算成显示串字节。绘制与命中因此不会各算一套。
        let layout = RenameLayout {
            bounds,
            display: display.clone(),
            font_size,
            pad_x,
            font: font.clone(),
        };
        self.input
            .update(cx, |input, _cx| input.last_layout = Some(layout));

        // 窗口级鼠标监听：每帧在 paint 里重新注册，下一帧自动清空，不会逐帧累积。
        // 元素级 `on_mouse_move` 只在命中链/hover 变化时触发，拖出标题就收不到；
        // 拖选必须用窗口级监听（与 `editor/element.rs` 同一做法）。
        self.register_pointer_handlers(window);

        // 点击定位：用**当前** `shape_line` 把窗口 x 换成最近的字节索引，光标落在
        // 点击处（不是全选、不是行尾）。没有 IME 组合时才应用；组合态下 `display`
        // 与 `text` 的字节索引不一致，这次点击交给输入法。
        if let Some(click) = pending_click {
            let byte = shaped.closest_index_for_x(click.x - origin.x);
            if self
                .input
                .update(cx, |input, _cx| input.apply_pending_click(byte))
            {
                state = self.input.read(cx).state.clone();
            }
        }

        // 顺序很重要：**选区底色先画、文字后画**。
        // 反过来的话全选高亮会把文字整块盖住——用户看到的"空输入框只有 caret"
        // 就是这个顺序 bug（预填其实一直是对的）。
        let focus = self.input.read(cx).focus_handle.clone();
        let focused = focus.is_focused(window);
        // 焦点从无到有（窗口重新激活、程序性 focus）：这一帧就要可见并重新计时，
        // 不能等下一拍——否则重新激活窗口后光标可能先"黑"最多半秒。
        // 闪烁循环还据此决定要不要叫醒一帧（失焦时不 notify，但相位照常翻转）。
        let was_focused = self.input.read(cx).focused_last_frame;
        if focused != was_focused {
            let now = cx.background_executor().now();
            self.input.update(cx, |input, _cx| {
                input.focused_last_frame = focused;
                if focused {
                    input.caret_blink_visible = true;
                    input.caret_blink_epoch = now;
                }
            });
        }
        let selection = state.selected_byte_range();
        if focused && state.marked_utf16().is_none() && selection.start != selection.end {
            let start_x = shaped.x_for_index(selection.start);
            let end_x = shaped.x_for_index(selection.end);
            window.paint_quad(gpui::fill(
                Bounds::new(
                    point(origin.x + start_x, origin.y),
                    size(end_x - start_x, line_height),
                ),
                theme::input_selection(),
            ));
        }

        if let Err(error) = shaped.paint(origin, line_height, TextAlign::Left, None, window, cx) {
            eprintln!("[menghunote] 重命名文本绘制失败：{error}");
        }

        // 光标（组合中画在 preedit 末尾）。与搜索框共用 `caret_visible` 判定：
        // 空文本聚焦也要画（此前搜索框因为多了一个"非空"守卫而漏掉，这里保持一致）。
        //
        // 闪烁规则：只有"聚焦 + 非组合"才受 530ms 相位约束；IME 组合串存在时**常亮**
        // （正在拼写的人必须看得到落点，闪起来会以为输入丢了）；失焦一律不画。
        let composing = state.marked_utf16().is_some();
        let blink_visible = self.input.read(cx).caret_blink_visible();
        let show_caret = crate::search_input::caret_visible(focused, composing, display.len())
            && (composing || blink_visible);
        if !show_caret {
            // 隐藏相位或失焦：这一帧不画 caret（但仍要把输入处理器注册上，见函数末尾）。
            self.input
                .update(cx, |input, _cx| input.caret_painted_last_frame = false);
            let focus = self.input.read(cx).focus_handle.clone();
            window.handle_input(
                &focus,
                ElementInputHandler::new(bounds, self.input.clone()),
                cx,
            );
            return;
        }
        let caret_byte = match state.marked_utf16() {
            Some(_) => display.len(),
            None => state.caret(),
        };
        let caret_x = shaped.x_for_index(caret_byte.min(display.len()));
        window.paint_quad(gpui::fill(
            Bounds::new(
                point(origin.x + caret_x, origin.y),
                size(px(2.0), line_height),
            ),
            theme::input_caret(),
        ));
        self.input
            .update(cx, |input, _cx| input.caret_painted_last_frame = true);

        // 注册平台输入处理器（IME / 字符输入都走它）
        let focus = self.input.read(cx).focus_handle.clone();
        window.handle_input(
            &focus,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );
    }
}

impl RenameElement {
    /// 注册本帧的窗口级鼠标监听（按下兜底 / 拖动 / 抬起）。
    ///
    /// `Window::on_mouse_event` 的监听只活到下一帧（paint 时注册、帧结束自动清空），
    /// 所以每帧重新注册不会泄漏；`WeakEntity` 保证输入框被丢弃后监听安全失效。
    /// 拖动改用窗口级监听，是因为元素级 `on_mouse_move` 依赖 hover 命中链：
    /// 指针拖出标题（甚至拖出窗口）后就收不到，选区会停在中途。
    fn register_pointer_handlers(&self, window: &mut Window) {
        let down_input = self.input.downgrade();
        window.on_mouse_event(
            move |event: &MouseDownEvent, phase: DispatchPhase, _: &mut Window, cx: &mut App| {
                if phase != DispatchPhase::Bubble {
                    return;
                }
                down_input
                    .update(cx, |input, _cx| input.on_window_mouse_down(event))
                    .ok();
            },
        );

        let move_input = self.input.downgrade();
        window.on_mouse_event(
            move |event: &MouseMoveEvent,
                  phase: DispatchPhase,
                  window: &mut Window,
                  cx: &mut App| {
                if phase != DispatchPhase::Bubble {
                    return;
                }
                move_input
                    .update(cx, |input, cx| input.on_pointer_move(event, window, cx))
                    .ok();
            },
        );

        let up_input = self.input.downgrade();
        window.on_mouse_event(
            move |event: &MouseUpEvent, phase: DispatchPhase, window: &mut Window, cx: &mut App| {
                if phase != DispatchPhase::Bubble {
                    return;
                }
                up_input
                    .update(cx, |input, cx| input.on_pointer_up(event, window, cx))
                    .ok();
            },
        );
    }
}

/// 画一圈 1px 边框（四根细 quad；`outline` 在各版本 API 不稳）。
fn paint_border(window: &mut Window, bounds: Bounds<Pixels>, color: gpui::Rgba) {
    let thickness = px(1.0);
    let top = Bounds::new(bounds.origin, size(bounds.size.width, thickness));
    let bottom = Bounds::new(
        point(
            bounds.origin.x,
            bounds.origin.y + bounds.size.height - thickness,
        ),
        size(bounds.size.width, thickness),
    );
    let left = Bounds::new(bounds.origin, size(thickness, bounds.size.height));
    let right = Bounds::new(
        point(
            bounds.origin.x + bounds.size.width - thickness,
            bounds.origin.y,
        ),
        size(thickness, bounds.size.height),
    );
    for edge in [top, bottom, left, right] {
        window.paint_quad(gpui::fill(edge, color));
    }
}

/// 双击选词：返回 `byte` 所在连续词字符的字节区间。
///
/// 词字符 = Unicode alphanumeric 或 `_`；标点/空白/emoji 作为分隔符（emoji 不是
/// alphanumeric，双击它只会选它自己的 grapheme 边界）。只用于标题栏双击，不改变
/// 正文编辑器的分词规则。
fn word_range_at(text: &str, byte: usize) -> (usize, usize) {
    let byte = byte.min(text.len());
    // 双击位置本身不是词字符（空白/标点/emoji）：只选这一个完整字符，
    // 不要向左右吃掉相邻单词。
    if let Some(current) = text[byte..].chars().next() {
        if !is_word_char(current) {
            return (byte, byte + current.len_utf8());
        }
    }
    let mut start = byte;
    while start > 0 {
        let Some(prev) = text[..start].chars().next_back() else {
            break;
        };
        if !is_word_char(prev) {
            break;
        }
        start -= prev.len_utf8();
    }
    let mut end = byte;
    while end < text.len() {
        let Some(next) = text[end..].chars().next() else {
            break;
        };
        if !is_word_char(next) {
            break;
        }
        end += next.len_utf8();
    }
    if start == end {
        // 空文本/文件末尾：至少给一个可见字符（或保持空）。
        if let Some(prev) = text[..byte].chars().next_back() {
            start = byte - prev.len_utf8();
        }
    }
    (start, end)
}

fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

/// 闪烁半周期剩余时间（纯函数，便于单测；不读全局时钟）。
///
/// 时钟回拨或已经到点都夹到 [`Duration::ZERO`]——`Duration` 减法溢出的代价是
/// panic，而闪烁只是视觉效果，任何异常时间都不该让输入框崩掉。
fn blink_remaining(now: Instant, epoch: Instant) -> Duration {
    CARET_BLINK_INTERVAL.saturating_sub(now.saturating_duration_since(epoch))
}

/// 用与 [`RenameElement::paint`] **完全相同**的参数 shaping 一行文本。
///
/// 绘制、点击定位、拖选换算共用这一处，避免出现两套 shaping 参数导致
/// "画出来的字"和"选中的字"错位（字号/字体/文本 run 长度任一处漂移都会错）。
fn shape_display_line(
    display: &str,
    font_size: Pixels,
    font: &gpui::Font,
    window: &Window,
) -> gpui::ShapedLine {
    let runs = [TextRun {
        len: display.len(),
        font: font.clone(),
        color: theme::text_color().into(),
        background_color: None,
        underline: None,
        strikethrough: None,
    }];
    window.text_system().shape_line(
        SharedString::from(display.to_owned()),
        font_size,
        &runs,
        None,
    )
}

/// 文本左内边距。
const PAD_X: f32 = 4.0;

#[cfg(test)]
mod tests {
    use super::RenameState;

    fn state(text: &str) -> RenameState {
        RenameState::selecting_all(text)
    }

    #[test]
    fn double_click_word_range_handles_cjk_ascii_and_punctuation() {
        let text = "项目Alpha beta";
        let start = text.find("Alpha").unwrap();
        let (s, e) = super::word_range_at(text, start + 2);
        assert_eq!(
            &text[s..e],
            "项目Alpha",
            "CJK + ASCII 连续 alphanumeric 视为一个词"
        );
        let space = text.find(' ').unwrap();
        let (s, e) = super::word_range_at(text, space);
        assert_eq!(&text[s..e], " ", "空白双击至少选中一个字符");
        let (s, e) = super::word_range_at("😀x", 0);
        assert_eq!(&[s, e], &[0, 4], "emoji 不是词字符，双击只选自身");
    }

    #[test]
    fn prefill_at_end_has_no_selection() {
        let state = RenameState::at_end("YYYY-MM-DD");
        assert_eq!(state.text(), "YYYY-MM-DD");
        assert_eq!(state.caret(), "YYYY-MM-DD".len());
        assert_eq!(
            state.selected_byte_range(),
            "YYYY-MM-DD".len().."YYYY-MM-DD".len(),
            "设置字段首次打开不能预选，否则会整段显示蓝底"
        );
    }

    #[test]
    fn new_state_selects_everything() {
        let state = state("中文ab😀");
        assert_eq!(state.selected_byte_range(), 0.."中文ab😀".len());
        assert_eq!(
            state.selection_utf16(),
            0..6,
            "中文=2 + a/b=2 + 😀(代理对)=2"
        );
    }

    #[test]
    fn set_caret_clamps_to_character_boundaries() {
        let mut state = RenameState::at_end("a😀b");
        state.set_caret(2);
        assert_eq!(state.caret(), 1, "😀 中间的字节要向前夹到字符边界");
        assert_eq!(state.selected_byte_range(), 1..1);
        state.set_caret(4);
        assert_eq!(state.caret(), 1, "😀 的后一个字节同样夹到字符起点");
        state.set_caret(999);
        assert_eq!(state.caret(), "a😀b".len(), "越界夹到末尾");
    }

    #[test]
    fn set_caret_collapses_the_selection() {
        let mut state = RenameState::selecting_all("标题");
        state.set_caret(3);
        assert_eq!(
            state.selected_byte_range(),
            3..3,
            "点击定位必须折叠选区，不能保留全选"
        );
        assert_eq!(state.selection_utf16(), 1..1);
    }

    #[test]
    fn paste_replaces_the_selection_and_stays_on_one_line() {
        let mut state = RenameState::selecting_all("旧标题");
        // 粘贴与键入走同一个 `insert`：有选区替换，换行/制表符丢弃。
        state.insert("新\n标题\t");
        assert_eq!(state.text(), "新标题");
        assert_eq!(
            state.selected_byte_range(),
            state.text().len()..state.text().len()
        );
    }

    #[test]
    fn typing_replaces_the_selection() {
        let mut state = state("旧标题");
        state.insert("新名字");
        assert_eq!(state.text(), "新名字");
        assert_eq!(state.caret(), "新名字".len());
    }

    #[test]
    fn insertion_point_moves_with_utf16_and_astral_chars() {
        let mut state = state("");
        state.insert("a😀b");
        assert_eq!(state.text(), "a😀b");
        // "a😀b" = 1 + 4 + 1 = 6 字节
        assert_eq!(state.caret(), 6);
        state.move_left(false);
        assert_eq!(state.caret(), 5, "左移一个字符到 b 之前");
        state.move_left(false);
        assert_eq!(state.caret(), 1, "😀 占 4 字节，整体左移到它前面");
        // UTF-16：a=1, 😀=2 → 3
        assert_eq!(state.selection_utf16(), 1..1);
        state.insert("中");
        assert_eq!(state.text(), "a中😀b");
    }

    #[test]
    fn backspace_and_delete_stay_on_char_boundaries() {
        let mut state = state("a😀中");
        state.end(false);
        state.backspace();
        assert_eq!(state.text(), "a😀");
        state.home(false);
        state.delete_forward();
        assert_eq!(state.text(), "😀", "删除第一个字符只删 a");
        state.delete_forward();
        assert_eq!(state.text(), "", "😀 整体删掉，不留半个代理对");
    }

    #[test]
    fn selection_move_without_shift_collapses() {
        let mut state = state("abcdef");
        state.move_left(false);
        assert_eq!(
            state.selected_byte_range(),
            0..0,
            "有选区时左移收起到选区头"
        );
    }

    #[test]
    fn blank_title_is_rejected() {
        let mut state = state("  ");
        assert_eq!(state.trimmed_title(), None);
        state.insert("   ");
        assert_eq!(state.trimmed_title(), None);
        state.insert(" 真标题 ");
        assert_eq!(state.trimmed_title().as_deref(), Some("真标题"));
    }

    #[test]
    fn preedit_is_displayed_but_never_written_to_the_title() {
        let mut state = state("旧");
        state.set_marked("nihao");
        assert_eq!(state.text(), "旧", "组合串不进标题");
        assert_eq!(state.display(), "nihao");
        assert!(state.marked_utf16().is_some());
        state.commit_marked("你好");
        assert_eq!(state.text(), "你好");
        assert_eq!(state.display(), "你好");
        assert!(state.marked_utf16().is_none());
    }

    #[test]
    fn newlines_and_tabs_are_filtered_out() {
        let mut state = state("");
        state.insert("a\nb\tc");
        assert_eq!(state.text(), "abc", "单行输入不接受换行/制表符");
    }

    #[test]
    fn set_selection_keeps_direction_and_clamps_boundaries() {
        let mut state = RenameState::at_end("a😀中");
        let len = "a😀中".len();

        // 正向：anchor 在前；选区范围永远起点 ≤ 终点。
        state.set_selection(1, len);
        assert_eq!(state.selected_byte_range(), 1..len);
        assert!(!state.is_reversed());
        assert_eq!(state.caret(), len);

        // 反向：anchor 在后，范围仍规范化，但方向保留。
        state.set_selection(len, 1);
        assert_eq!(state.selected_byte_range(), 1..len);
        assert!(state.is_reversed());
        assert_eq!(state.caret(), 1);

        // 落在多字节字符中间向前夹到字符边界；越界夹到文本末尾。
        state.set_selection(3, 999);
        assert_eq!(state.selected_byte_range(), 1..len);
        assert_eq!(state.selection_utf16(), 1..4, "😀 的中间不能出现半个代理对");
    }

    #[test]
    fn set_selection_on_empty_title_stays_collapsed() {
        let mut state = RenameState::at_end("");
        state.set_selection(5, 7);
        assert_eq!(state.selected_byte_range(), 0..0, "空标题不能越界");
        assert_eq!(state.caret(), 0);
    }

    #[test]
    fn caret_blink_counts_down_the_half_period() {
        let epoch = std::time::Instant::now();
        assert_eq!(
            super::blink_remaining(epoch, epoch),
            super::CARET_BLINK_INTERVAL,
            "刚复位时整段半周期都还没走"
        );
        assert_eq!(
            super::blink_remaining(epoch + std::time::Duration::from_millis(200), epoch),
            std::time::Duration::from_millis(330),
            "200ms 后只剩 330ms"
        );
        assert_eq!(
            super::blink_remaining(epoch + super::CARET_BLINK_INTERVAL, epoch),
            std::time::Duration::ZERO,
            "到点必须归零（允许翻转）"
        );
        assert_eq!(
            super::blink_remaining(epoch + std::time::Duration::from_secs(5), epoch),
            std::time::Duration::ZERO,
            "超过半周期/时钟回拨都不能返回负数（Duration 减法会 panic）"
        );
        assert_eq!(
            super::CARET_BLINK_INTERVAL,
            std::time::Duration::from_millis(530),
            "0.53s 是与正文编辑器一致的闪烁节奏"
        );
    }
}
