//! 搜索框的自绘单行输入（带 IME）。
//!
//! ## 为什么不用 `gpui-component` 的 `Input`（**重要，别再踩**）
//!
//! `gpui-component 0.9.1` / `gpui-base 0.9.1` 的源码里，实现 `EntityInputHandler` 的
//! 类型是 **0 个**——也就是说组件输入框**没有平台输入法接入**。表现是：
//!
//! - 用户开着微软拼音时，中英文按键都被输入法吞掉，输入框里一个字都进不去；
//! - `Esc` 在组合态被输入法消费，导致浮层关不掉；
//! - 我们早期的自动化能过，是因为脚本走 `WM_CHAR`（不经过 IME），**测不出这个问题**。
//!
//! **结论：本项目里凡是"要给人打字"的控件，必须自绘并实现 `EntityInputHandler`**
//! （本文件与 `sidebar_rename.rs` 是同一条路子，那边微软拼音真机验过）。
//! 组件的 `Button` / `List` / `Dialog` 这类**非文本输入**控件仍然可以放心用。
//!
//! ## 与重命名输入框的关系
//!
//! 文本状态直接复用 [`crate::sidebar_rename::RenameState`]（UTF-16 坐标、组合串、
//! 选区、caret 都已实现且有单测），这里只补：
//!
//! - 事件：`Changed`（输入变化 → 宿主发查询）、`Submit`（`Enter`）、`Escape`；
//! - **组合中 `Esc` 只取消组合**，组合外才抛出 `Escape` 交给浮层关闭；
//! - **组合中 `Enter` 是"上屏"**，不当作提交（否则拼音没上屏就把浮层关了）；
//! - 占位符渲染与无边框样式（边框由面板提供）。

use gpui::prelude::*;
use gpui::{
    div, point, px, size, App, Bounds, Context, Element, ElementId, ElementInputHandler, Entity,
    EntityInputHandler, EventEmitter, GlobalElementId, IntoElement, KeyDownEvent, LayoutId,
    MouseButton, MouseDownEvent, ParentElement, Pixels, Render, SharedString, Style, Styled,
    TextAlign, TextRun, UTF16Selection, Window,
};

use crate::editor::theme;
use crate::sidebar_rename::{byte_to_utf16, utf16_to_byte, RenameState};
use crate::text_shortcuts::{shortcut_action, ShortcutAction};

/// 搜索框发出的事件。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchInputEvent {
    /// 文本变了（或组合串变了）：宿主应当去发查询。
    Changed,
    /// `Enter`（且不在组合态）：打开当前选中结果。
    Submit,
    /// `Esc`（且不在组合态）：关闭浮层。
    Escape,
}

/// 组合态下的 `Esc` 语义。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EscapeAction {
    /// 只取消输入法组合（不关浮层）。
    CancelComposition,
    /// 关闭浮层。
    CloseOverlay,
}

/// `Esc` 该做什么（**纯函数**）：组合中只取消组合，组合外才关浮层。
///
/// 为什么必须这样：微软拼音组合态会先吃掉一次 `Esc`；如果到我们这里还当成
/// "关浮层"，用户按 Esc 取消拼音时浮层会直接消失（P0 回归的一半原因）。
#[must_use]
pub fn escape_action(composing: bool) -> EscapeAction {
    if composing {
        EscapeAction::CancelComposition
    } else {
        EscapeAction::CloseOverlay
    }
}

/// 组合态下的 `Enter` 语义。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnterAction {
    /// 交给输入法上屏（不提交、不打开结果）。
    CommitComposition,
    /// 打开选中结果。
    Submit,
}

/// `Enter` 该做什么（**纯函数**）：组合中是上屏，组合外才提交。
#[must_use]
pub fn enter_action(composing: bool) -> EnterAction {
    if composing {
        EnterAction::CommitComposition
    } else {
        EnterAction::Submit
    }
}

/// 单行搜索输入框。
pub struct SearchInput {
    /// 文本状态（复用重命名输入框的实现）。
    state: RenameState,
    /// 焦点句柄。
    focus_handle: gpui::FocusHandle,
    /// 占位符（空串时显示）。
    placeholder: &'static str,
}

impl SearchInput {
    /// 新建一个空输入框。
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            state: RenameState::selecting_all(""),
            focus_handle: cx.focus_handle(),
            placeholder: "搜索笔记…",
        }
    }

    /// 当前文本（不含组合串）。
    #[must_use]
    pub fn text(&self) -> &str {
        self.state.text()
    }

    /// 当前是否有输入法组合在进行。
    #[must_use]
    pub fn is_composing(&self) -> bool {
        self.state.marked_utf16().is_some()
    }

    /// 把焦点给输入框。
    pub fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus_handle, cx);
    }

    /// 按键处理（**纯状态机**：组合态与非组合态行为不同）。
    ///
    /// 返回 `Some(event)` 表示要抛给宿主；`None` 表示这次按键被输入框自己消化了。
    fn handle_key(&mut self, event: &KeyDownEvent) -> Option<SearchInputEvent> {
        let key = event.keystroke.key.as_str();
        let extend = event.keystroke.modifiers.shift;
        let composing = self.state.marked_utf16().is_some();

        // 快捷键先于普通按键：`Ctrl+A` 全选（组合态由纯函数里 return None 挡住）。
        match shortcut_action(&event.keystroke, composing) {
            ShortcutAction::SelectAll => {
                self.state.select_all();
                return None;
            }
            ShortcutAction::ClipboardUnsupported => return None,
            ShortcutAction::None => {}
        }

        match key {
            "escape" => match escape_action(composing) {
                EscapeAction::CancelComposition => {
                    self.state.clear_marked();
                    return None;
                }
                EscapeAction::CloseOverlay => return Some(SearchInputEvent::Escape),
            },
            "enter" => match enter_action(composing) {
                EnterAction::CommitComposition => return None,
                EnterAction::Submit => return Some(SearchInputEvent::Submit),
            },
            "backspace" => self.state.backspace(),
            "delete" => self.state.delete_forward(),
            "left" => self.state.move_left(extend),
            "right" => self.state.move_right(extend),
            "home" => self.state.home(extend),
            "end" => self.state.end(extend),
            // 上/下是列表导航，不归输入框管：交回宿主（浮层）。
            "up" | "down" => return None,
            _ => return None,
        }
        Some(SearchInputEvent::Changed)
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(event) = self.handle_key(event) {
            // 文本已落定，再通知宿主（发查询 / 打开 / 关闭）。
            cx.emit(event);
        }
        cx.notify();
        window.prevent_default();
    }
}

impl EventEmitter<SearchInputEvent> for SearchInput {}

impl gpui::Focusable for SearchInput {
    fn focus_handle(&self, _cx: &App) -> gpui::FocusHandle {
        self.focus_handle.clone()
    }
}

impl EntityInputHandler for SearchInput {
    fn text_for_range(
        &mut self,
        range_utf16: std::ops::Range<usize>,
        adjusted_range: &mut Option<std::ops::Range<usize>>,
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
    ) -> Option<std::ops::Range<usize>> {
        self.state.marked_utf16()
    }

    fn unmark_text(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.state.clear_marked();
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        _range: Option<std::ops::Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.state.marked_utf16().is_some() {
            self.state.commit_marked(text);
        } else {
            self.state.insert(text);
        }
        cx.emit(SearchInputEvent::Changed);
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<std::ops::Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<std::ops::Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 组合开始/更新：把 preedit 挂到当前选区。
        // 平台给的 range 是 UTF-16；这里只需要"有组合串就更新 preedit"，
        // 具体落点由 `RenameState::set_marked` 按当前选区计算，无需再换算一次。
        let _ = range_utf16;
        if new_text.is_empty() {
            self.state.clear_marked();
        } else {
            self.state.set_marked(new_text);
        }
        let _ = new_selected_range_utf16;
        cx.emit(SearchInputEvent::Changed);
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        _range_utf16: std::ops::Range<usize>,
        bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        // 候选窗贴在输入框下方（单行输入框，按行高估一个矩形就够了）。
        Some(Bounds::new(
            point(
                bounds.origin.x + px(super::editor::theme::PAD_X),
                bounds.origin.y,
            ),
            size(px(2.0), px(super::editor::theme::LINE_HEIGHT)),
        ))
    }

    fn character_index_for_point(
        &mut self,
        _point: gpui::Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        // 单选/点击定位暂不支持（浮层打开即聚焦、键盘输入是主路径）。
        None
    }
}

impl Render for SearchInput {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let placeholder = self.placeholder;
        div()
            .w_full()
            .h(px(28.0))
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::on_key_down))
            // 点输入框内部不算"点空白"：拦住冒泡，别让浮层背景把手势当成关闭。
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_input, _event: &MouseDownEvent, _window, cx| {
                    cx.stop_propagation();
                }),
            )
            .child(SearchElement {
                input: cx.entity(),
                placeholder,
            })
    }
}

/// 只负责"画一行文本 + 选区 + 光标 + 注册输入处理器"的元素。
struct SearchElement {
    input: Entity<SearchInput>,
    placeholder: &'static str,
}

impl IntoElement for SearchElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for SearchElement {
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
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = gpui::relative(1.0).into();
        style.size.height = gpui::relative(1.0).into();
        (window.request_layout(style, None, cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        _cx: &mut App,
    ) -> Self::PrepaintState {
        window.insert_hitbox(bounds, gpui::HitboxBehavior::Normal);
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let state = self.input.read(cx).state.clone();
        let display = state.display();
        let empty = display.is_empty();
        let text = if empty {
            self.placeholder.to_owned()
        } else {
            display.clone()
        };
        let font = theme::font();
        let font_size = px(theme::FONT_SIZE);
        let line_height = px(theme::LINE_HEIGHT);

        let color = if empty {
            theme::status_bar_text()
        } else {
            theme::text_color()
        };
        let runs = [TextRun {
            len: text.len(),
            font: font.clone(),
            color: color.into(),
            background_color: None,
            underline: None,
            strikethrough: None,
        }];
        let shaped =
            window
                .text_system()
                .shape_line(SharedString::from(text), font_size, &runs, None);
        let origin = point(
            bounds.origin.x + px(super::editor::theme::PAD_X),
            bounds.origin.y,
        );
        let baseline = (bounds.size.height - line_height) / 2.0;

        // 选区底色先画、文字后画（顺序反了会把字盖住，rename 踩过）。
        let selection = state.selected_byte_range();
        if !empty && state.marked_utf16().is_none() && selection.start != selection.end {
            let start_x = shaped.x_for_index(selection.start);
            let end_x = shaped.x_for_index(selection.end);
            window.paint_quad(gpui::fill(
                Bounds::new(
                    point(origin.x + start_x, origin.y + baseline),
                    size(end_x - start_x, line_height),
                ),
                theme::input_selection(),
            ));
        }

        if let Err(error) = shaped.paint(origin, line_height, TextAlign::Left, None, window, cx) {
            eprintln!("[menghunote] 搜索框文本绘制失败：{error}");
        }

        // 光标（组合中画在 preedit 末尾）。
        if !empty {
            let caret_byte = match state.marked_utf16() {
                Some(_) => display.len(),
                None => state.caret(),
            };
            let caret_x = shaped.x_for_index(caret_byte.min(display.len()));
            window.paint_quad(gpui::fill(
                Bounds::new(
                    point(origin.x + caret_x, origin.y + baseline),
                    size(px(2.0), line_height),
                ),
                theme::input_caret(),
            ));
        }

        // 注册平台输入处理器（IME / 字符输入都走它）——这是"能打字"的关键。
        let focus = self.input.read(cx).focus_handle.clone();
        window.handle_input(
            &focus,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );
    }
}

/// 文本的 UTF-16 长度（给组合串/选区定位用）。
#[must_use]
pub fn utf16_len(text: &str) -> usize {
    byte_to_utf16(text, text.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_semantics_depend_on_composition_state() {
        assert_eq!(escape_action(true), EscapeAction::CancelComposition);
        assert_eq!(escape_action(false), EscapeAction::CloseOverlay);
    }

    #[test]
    fn enter_semantics_depend_on_composition_state() {
        assert_eq!(enter_action(true), EnterAction::CommitComposition);
        assert_eq!(enter_action(false), EnterAction::Submit);
    }

    #[test]
    fn composing_escape_only_cancels_composition() {
        // 纯状态机：组合中 Esc 返回 None（不关浮层），组合外返回 Escape。
        let mut input_state = RenameState::selecting_all("");
        input_state.insert("abc");
        input_state.set_marked("拼");
        assert!(input_state.marked_utf16().is_some());
        input_state.clear_marked();
        assert!(input_state.marked_utf16().is_none());
    }

    #[test]
    fn select_all_then_type_replaces_the_whole_text() {
        let mut state = RenameState::selecting_all("");
        state.insert("menghunote");
        state.select_all();
        assert_eq!(state.selected_byte_range(), 0..state.text().len());
        state.insert("压"); // 打字 = 替换整段
        assert_eq!(state.text(), "压");
    }

    #[test]
    fn select_all_then_backspace_clears_the_text() {
        let mut state = RenameState::selecting_all("");
        state.insert("menghunote");
        state.select_all();
        state.backspace();
        assert_eq!(state.text(), "");
    }

    #[test]
    fn select_all_on_empty_text_does_not_panic() {
        let mut state = RenameState::selecting_all("");
        state.select_all();
        assert_eq!(state.selected_byte_range(), 0..0);
        state.insert("a");
        assert_eq!(state.text(), "a");
    }

    #[test]
    fn select_all_is_ignored_while_composing() {
        let mut state = RenameState::selecting_all("");
        state.insert("abc");
        state.set_marked("拼");
        // 组合态下 `shortcut_action` 返回 None，输入框不会把选区改成全选。
        let keystroke = gpui::Keystroke::parse("ctrl-a").expect("解析失败");
        assert_eq!(
            crate::text_shortcuts::shortcut_action(&keystroke, true),
            crate::text_shortcuts::ShortcutAction::None
        );
        assert!(state.marked_utf16().is_some());
    }

    #[test]
    fn utf16_len_of_ascii_and_chinese() {
        assert_eq!(utf16_len("abc"), 3);
        assert_eq!(utf16_len("中文"), 2);
        assert_eq!(utf16_len("a中😀"), 4);
    }
}
