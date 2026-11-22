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

use std::ops::Range;

use gpui::prelude::*;
use gpui::{
    div, point, px, size, App, Bounds, Context, Element, ElementId, ElementInputHandler, Entity,
    EntityInputHandler, EventEmitter, FocusHandle, Focusable, GlobalElementId, HitboxBehavior,
    InspectorElementId, IntoElement, KeyDownEvent, LayoutId, MouseButton, MouseDownEvent, Pixels,
    Render, SharedString, Style, TextAlign, TextRun, UTF16Selection, Window,
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

/// 输入框的用途：决定预填光标、Enter 提交语义与 `Tab` 行为。
///
/// 侧栏标题与表格单元格共用**同一套** IME/选区/光标本体；差异只在这里，避免复制
/// 第二份 `EntityInputHandler`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenameMode {
    /// 侧栏标题：预填并全选；`Enter` 提交 trim 后非空标题，空标题拒绝。
    Title,
    /// 表格单元格：光标在文本末尾；`Enter` 提交原始文本（空串也合法），
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

/// 行内重命名输入框（一个 GPUI 实体）。
pub struct RenameInput {
    state: RenameState,
    focus_handle: FocusHandle,
    mode: RenameMode,
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

    fn new_with_mode(initial: &str, mode: RenameMode, cx: &mut Context<Self>) -> Self {
        let state = match mode {
            RenameMode::Title => RenameState::selecting_all(initial),
            RenameMode::Cell => RenameState::at_end(initial),
        };
        Self {
            state,
            focus_handle: cx.focus_handle(),
            mode,
        }
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
    /// 宿主转发的文本输入（正常路径不会用到：输入框自己注册了 `EntityInputHandler`）。
    ///
    /// 只作为“平台把文本送错焦点”的兜底：仍走同一套单行清理，不产生第二套输入实现。
    pub fn insert_from_host(&mut self, text: &str, cx: &mut Context<Self>) {
        match self.mode {
            RenameMode::Title => self.state.insert(text),
            RenameMode::Cell => self.state.insert_cell(text),
        }
        cx.emit(RenameEvent::Changed);
        cx.notify();
    }

    /// 状态（单测/宿主用）。
    #[must_use]
    pub fn state(&self) -> &RenameState {
        &self.state
    }

    /// 把焦点给输入框。
    pub fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus_handle, cx);
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        let modifiers = event.keystroke.modifiers;
        let extend = modifiers.shift;

        // 表格单元格允许粘贴纯文本（GFM 转义在控制器提交时做）；标题输入框仍保持
        // 既有“剪贴板 v1 不支持”的行为，避免顺手改变侧栏重命名。
        if self.mode == RenameMode::Cell
            && self.state.marked_utf16().is_none()
            && modifiers.control
            && !modifiers.alt
            && !modifiers.shift
            && !modifiers.platform
            && key == "v"
        {
            if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                self.state.insert_cell(&text);
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
        cx.notify();
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
        cx.emit(RenameEvent::Changed);
        cx.notify();
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
        cx.notify();
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
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_input, _event: &MouseDownEvent, _window, cx| {
                    cx.stop_propagation();
                }),
            )
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
        style.size.height = gpui::relative(0.0).into();
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
        let state = self.input.read(cx).state.clone();
        let mode = self.input.read(cx).mode();
        let display = state.display();
        let font = theme::font();
        // 标题与表格单元格只差尺寸：单元格更小，并在 cell 行高里垂直居中
        // （浮层矩形就是 cell 矩形，不居中会让文字贴顶）。
        let (font_size, line_height, pad_x, top_pad) = match mode {
            RenameMode::Title => (
                px(theme::FONT_SIZE - 1.0),
                px(theme::LINE_HEIGHT),
                PAD_X,
                px(0.0),
            ),
            RenameMode::Cell => {
                let line_height = px(theme::line_height_for(theme::TABLE_FONT_SIZE));
                let top_pad = ((bounds.size.height - line_height) / 2.0).max(px(0.0));
                (
                    px(theme::TABLE_FONT_SIZE),
                    line_height,
                    theme::TABLE_CELL_PAD_X,
                    top_pad,
                )
            }
        };

        // 背景：**不透明白底** + 中性边框。输入框必须压住行的选中底色
        // （行可能是蓝色"当前打开"态），否则两层蓝叠在一起看不出哪段是选中。
        let inner = Bounds::new(
            point(bounds.origin.x + px(1.0), bounds.origin.y + px(1.0)),
            size(bounds.size.width - px(2.0), bounds.size.height - px(2.0)),
        );
        window.paint_quad(gpui::fill(inner, theme::input_bg()));
        paint_border(window, bounds, theme::input_border());

        let runs = [TextRun {
            len: display.len(),
            font: font.clone(),
            color: theme::text_color().into(),
            background_color: None,
            underline: None,
            strikethrough: None,
        }];
        let shaped = window.text_system().shape_line(
            SharedString::from(display.clone()),
            font_size,
            &runs,
            None,
        );
        let origin = point(bounds.origin.x + px(pad_x), bounds.origin.y + top_pad);

        // 顺序很重要：**选区底色先画、文字后画**。
        // 反过来的话全选高亮会把文字整块盖住——用户看到的"空输入框只有 caret"
        // 就是这个顺序 bug（预填其实一直是对的）。
        let selection = state.selected_byte_range();
        if state.marked_utf16().is_none() && selection.start != selection.end {
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
        let focus = self.input.read(cx).focus_handle.clone();
        let focused = focus.is_focused(window);
        if !crate::search_input::caret_visible(
            focused,
            state.marked_utf16().is_some(),
            display.len(),
        ) {
            // 失焦不画 caret（但仍要把输入处理器注册上，见函数末尾）。
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

        // 注册平台输入处理器（IME / 字符输入都走它）
        let focus = self.input.read(cx).focus_handle.clone();
        window.handle_input(
            &focus,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
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

/// 文本左内边距。
const PAD_X: f32 = 4.0;

#[cfg(test)]
mod tests {
    use super::RenameState;

    fn state(text: &str) -> RenameState {
        RenameState::selecting_all(text)
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
}
