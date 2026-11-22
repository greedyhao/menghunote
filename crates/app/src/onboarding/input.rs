//! 向导用的单行输入控件（自绘，注册平台输入处理器 = 能打字/能粘贴）。
//!
//! ## 为什么自绘
//!
//! 与搜索框/重命名输入框同一个原因（见 `crate::search_input` 的模块文档）：
//! `gpui-component 0.9.1` 里**没有**任何类型实现 `EntityInputHandler`，用它做输入框在
//! 真机上开 IME 会一个字都进不去。本项目里"要给人打字"的控件都必须自绘并注册
//! `ElementInputHandler`。
//!
//! ## 与搜索框的区别
//!
//! | 能力 | 本控件（S1/S2/S3） | 说明 |
//! | --- | --- | --- |
//! | 直接输入 / 粘贴（`Ctrl+V`、`Shift+Insert`） | ✅ | 粘贴读 `cx.read_from_clipboard()` |
//! | `Ctrl+A` 全选 / 方向键 / Home/End / Backspace/Delete | ✅ | 复用 [`RenameState`] |
//! | `Tab` / `Shift+Tab` | ✅ | 走 `window.focus_next/prev` |
//! | IME 组合（preedit） | ⚠️ 非秘密字段同搜索框；**秘密字段不向平台报告正文** | 见下 |
//! | `Ctrl+C` / `Ctrl+X` | ❌ 明确不做 | 秘密字段**禁止**复制；普通字段 v1 也没接剪贴板写出 |
//! | 鼠标点击定位 caret | ❌ 不做 | 键盘是主路径（与搜索框一致） |
//!
//! ## 秘密字段（`secret = true`）
//!
//! ADR 0011 要求 setup secret / 恢复码遮罩、禁复制/剪切、不得落日志/Debug。本实现保证：
//!
//! - 渲染只画 `•`（组合串也画成 `•`，不画 preedit 原文）；
//! - `EntityInputHandler::text_for_range` 与 `marked_text_range` 对平台返回 `None`：
//!   不把正文交给系统输入法/候选窗；
//! - 本类型**不实现 `Debug`/`Clone`**，宿主（`onboarding::app_root`）也不实现——正文
//!   没有任何一条 Debug 输出路径能带出来；
//! - 允许**粘贴**（`Ctrl+V` / `Shift+Insert`）：恢复码/setup secret 的真实操作路径就是
//!   "从密码管理器或备份处粘贴"，禁粘贴等于把用户逼去手抄。
//!
//! **已知缺口（S3 明确记录，未在本切片完成）**：秘密字段里的 IME 组合不会被平台正确
//! 提交（因为我们不报告 marked range），所以 setup secret/恢复码必须直接键入 ASCII 或
//! 粘贴；完整 `EntityInputHandler` 秘密输入（组合中 Enter 上屏、Esc 取消、UTF-16 选区）
//! 留给后续切片。非秘密字段（relay/实例目录/vault_id）的 IME 组合走同一套
//! `RenameState`，与搜索框一致。

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

/// `Shift+Insert` 是否等价于粘贴（传统终端/输入法常用键位）。
///
/// 单独成纯函数是为了可单测：它**不进** [`crate::text_shortcuts::shortcut_action`]，
/// 因为搜索框/重命名的既有行为（`Ctrl+V` 明确不支持）不应被改；向导里的秘密输入
/// 必须能粘贴（恢复码/setup secret 的真实来源是密码管理器）。
///
/// 规则：`Insert` + `Shift`，且**不带** Ctrl/Alt/Cmd；组合态（IME 正在拼写）由调用方
/// 先行排除（此时按键属于输入法）。
#[must_use]
pub fn is_paste_keystroke(keystroke: &gpui::Keystroke) -> bool {
    let modifiers = keystroke.modifiers;
    keystroke.key == "insert"
        && modifiers.shift
        && !modifiers.control
        && !modifiers.alt
        && !modifiers.platform
}

/// 输入控件事件。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormInputEvent {
    /// 文本变了（含粘贴/组合串）。
    Changed,
    /// `Enter`（非组合态）：表单提交。
    Submit,
}

/// 单行输入控件。
pub struct FormInput {
    state: RenameState,
    focus_handle: gpui::FocusHandle,
    placeholder: SharedString,
    /// 是否秘密字段（遮罩 + 不向平台报告正文）。
    secret: bool,
    /// 测试元数据 selector（宿主传入，便于 headless 断言）。
    selector: SharedString,
    /// 点击输入框时把焦点给它（默认关：向导由宿主决定焦点；面板里的秘密输入需要）。
    click_to_focus: bool,
}

impl FormInput {
    /// 新建一个空输入控件。
    pub fn new(placeholder: impl Into<SharedString>, secret: bool, cx: &mut Context<Self>) -> Self {
        Self {
            state: RenameState::selecting_all(""),
            // 必须设置在 FocusHandle 上；div 的 `.tab_stop(true)` 只影响交互元数据，
            // 不会把 handle 登记成键盘可达的 tab stop。
            focus_handle: cx.focus_handle().tab_stop(true),
            placeholder: placeholder.into(),
            secret,
            selector: SharedString::from("onboarding-input"),
            click_to_focus: false,
        }
    }

    /// 设置测试 selector（链式，便于构造时一行写完）。
    #[must_use]
    pub fn selector(mut self, selector: impl Into<SharedString>) -> Self {
        self.selector = selector.into();
        self
    }

    /// 点击输入框时把焦点给它（链式；默认关）。
    ///
    /// 向导的输入框由宿主在进入表单时统一聚焦，不需要点击聚焦；dev-sync 面板里的
    /// 恢复码输入是"展开了确认区再点进去粘贴"的形态，必须有这条路径。
    #[must_use]
    pub fn click_to_focus(mut self) -> Self {
        self.click_to_focus = true;
        self
    }

    /// 当前文本（不含 preedit）。
    #[must_use]
    pub fn text(&self) -> &str {
        self.state.text()
    }

    /// 直接设置文本（测试/预填用；会清空组合并全选）。
    pub fn set_text(&mut self, text: &str) {
        self.state = RenameState::selecting_all(text);
    }

    /// 清空文本（秘密字段提交后调用）。
    pub fn clear(&mut self) {
        self.set_text("");
    }

    /// 是否是秘密字段。
    #[must_use]
    pub const fn is_secret(&self) -> bool {
        self.secret
    }

    /// 把焦点给输入控件。
    pub fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus_handle, cx);
    }

    /// 按键处理（返回 `Some` 表示要抛给宿主；`None` = 自己消化了）。
    fn handle_key(
        &mut self,
        event: &KeyDownEvent,
        cx: &mut Context<Self>,
    ) -> Option<FormInputEvent> {
        let key = event.keystroke.key.as_str();
        let extend = event.keystroke.modifiers.shift;
        let composing = self.state.marked_utf16().is_some();

        // `Shift+Insert`：与 `Ctrl+V` 同义的粘贴（组合态下按键属于输入法，不抢）。
        if !composing && is_paste_keystroke(&event.keystroke) {
            return self.paste_from_clipboard(cx);
        }

        match shortcut_action(&event.keystroke, composing) {
            ShortcutAction::SelectAll => {
                self.state.select_all();
                return None;
            }
            // `Ctrl+C/X/V`：只有粘贴实现；秘密字段也允许粘贴（用户需要把一次性 secret
            // 贴进来），但**绝不**实现复制/剪切。
            ShortcutAction::ClipboardUnsupported => {
                if key == "v" {
                    return self.paste_from_clipboard(cx);
                }
                return None;
            }
            ShortcutAction::None => {}
        }

        match key {
            "escape" => {
                if composing {
                    self.state.clear_marked();
                    return None;
                }
                return None;
            }
            "enter" => {
                if composing {
                    // 组合中 Enter 是上屏，不提交（与搜索框一致）。
                    return None;
                }
                return Some(FormInputEvent::Submit);
            }
            "backspace" => self.state.backspace(),
            "delete" => self.state.delete_forward(),
            "left" => self.state.move_left(extend),
            "right" => self.state.move_right(extend),
            "home" => self.state.home(extend),
            "end" => self.state.end(extend),
            _ => return None,
        }
        Some(FormInputEvent::Changed)
    }

    /// 把剪贴板文本插入当前选区（秘密字段同样允许粘贴）。
    ///
    /// 换行/制表符由 [`RenameState::insert`] 过滤成空（单行控件），所以粘贴一整行
    /// 恢复码不会带入换行。
    fn paste_from_clipboard(&mut self, cx: &mut Context<Self>) -> Option<FormInputEvent> {
        let text = cx.read_from_clipboard().and_then(|item| item.text())?;
        self.state.insert(&text);
        Some(FormInputEvent::Changed)
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        // Tab 在 `handle_key` 之外处理：聚焦导航需要 `&mut Window`。
        if event.keystroke.key == "tab" && self.state.marked_utf16().is_none() {
            if event.keystroke.modifiers.shift {
                window.focus_prev(cx);
            } else {
                window.focus_next(cx);
            }
            window.prevent_default();
            return;
        }
        if let Some(event) = self.handle_key(event, cx) {
            cx.emit(event);
        }
        cx.notify();
        window.prevent_default();
    }
}

impl EventEmitter<FormInputEvent> for FormInput {}

impl gpui::Focusable for FormInput {
    fn focus_handle(&self, _cx: &App) -> gpui::FocusHandle {
        self.focus_handle.clone()
    }
}

impl EntityInputHandler for FormInput {
    fn text_for_range(
        &mut self,
        range_utf16: std::ops::Range<usize>,
        adjusted_range: &mut Option<std::ops::Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        // 秘密字段：绝不把正文交给平台（输入法/候选窗/系统）。
        if self.secret {
            return None;
        }
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
        // 秘密字段不报告组合范围（同理：不向平台暴露正文）。
        if self.secret {
            return None;
        }
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
        cx.emit(FormInputEvent::Changed);
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _range_utf16: Option<std::ops::Range<usize>>,
        new_text: &str,
        _new_selected_range_utf16: Option<std::ops::Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 组合开始/更新：把 preedit 挂到当前选区（真正的落点由 RenameState 计算）。
        if new_text.is_empty() {
            self.state.clear_marked();
        } else {
            self.state.set_marked(new_text);
        }
        cx.emit(FormInputEvent::Changed);
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        _range_utf16: std::ops::Range<usize>,
        bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        // 候选窗贴在输入框左下（单行控件按行高估一个矩形）。
        Some(Bounds::new(
            point(bounds.origin.x + px(theme::PAD_X), bounds.origin.y),
            size(px(2.0), px(theme::LINE_HEIGHT)),
        ))
    }

    fn character_index_for_point(
        &mut self,
        _point: gpui::Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        // 不做点击定位（与搜索框一致）。
        None
    }
}

impl Render for FormInput {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let placeholder = self.placeholder.clone();
        let selector = self.selector.clone();
        let secret = self.secret;
        div()
            .w_full()
            .h(px(28.0))
            .debug_selector(move || selector.to_string())
            .track_focus(&self.focus_handle)
            // 没有 tab_stop，`window.focus_next/prev` 找不到这个输入框（Tab 会原地不动）。
            .tab_stop(true)
            .on_key_down(cx.listener(Self::on_key_down))
            // 点输入框内部不算"点空白"：拦住冒泡。
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|input, _event: &MouseDownEvent, window, cx| {
                    if input.click_to_focus {
                        input.focus(window, cx);
                    }
                    cx.stop_propagation();
                }),
            )
            .child(FormInputElement {
                input: cx.entity(),
                placeholder,
                secret,
            })
    }
}

/// 只负责"画一行文本/遮罩 + 选区 + 光标 + 注册输入处理器"的元素。
struct FormInputElement {
    input: Entity<FormInput>,
    placeholder: SharedString,
    secret: bool,
}

impl IntoElement for FormInputElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for FormInputElement {
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
        let raw = state.display();
        // 秘密字段：显示遮罩（按字符数，不按字节，CJK 也一个点）。
        let display = if self.secret {
            "•".repeat(raw.chars().count())
        } else {
            raw.clone()
        };
        let empty = display.is_empty();
        let text = if empty {
            self.placeholder.to_string()
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
        let focus = self.input.read(cx).focus_handle.clone();
        let focused = focus.is_focused(window);
        let composing = state.marked_utf16().is_some();
        let show_caret = crate::search_input::caret_visible(focused, composing, display.len());
        let placeholder_shift = if empty && show_caret {
            px(4.0)
        } else {
            px(0.0)
        };
        let caret_origin_x = bounds.origin.x + px(theme::PAD_X);
        let origin = point(caret_origin_x + placeholder_shift, bounds.origin.y);
        let baseline = (bounds.size.height - line_height) / 2.0;

        // 选区底色先画、文字后画。
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
            eprintln!("[menghunote] 向导输入框文本绘制失败：{error}");
        }

        if show_caret {
            let caret_byte = match state.marked_utf16() {
                Some(_) => display.len(),
                None => state.caret().min(display.len()),
            };
            let caret_local_x = shaped.x_for_index(caret_byte);
            window.paint_quad(gpui::fill(
                Bounds::new(
                    point(caret_origin_x + caret_local_x, origin.y + baseline),
                    size(px(2.0), line_height),
                ),
                theme::input_caret(),
            ));
        }

        // 注册平台输入处理器（IME / 字符输入 / 粘贴都走它）——"能打字"的关键。
        let focus = self.input.read(cx).focus_handle.clone();
        window.handle_input(
            &focus,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );
    }
}

/// 文本的 UTF-16 长度（组合串/选区定位用；与搜索框同义）。
#[must_use]
pub fn utf16_len(text: &str) -> usize {
    byte_to_utf16(text, text.len())
}

#[cfg(test)]
mod tests {
    use super::{is_paste_keystroke, utf16_len};
    use gpui::Keystroke;

    fn key(text: &'static str) -> Keystroke {
        Keystroke::parse(text).expect("按键解析失败")
    }

    #[test]
    fn utf16_len_matches_platform_units() {
        assert_eq!(utf16_len("abc"), 3);
        assert_eq!(utf16_len("中文"), 2);
        assert_eq!(utf16_len("a中😀"), 4);
    }

    #[test]
    fn paste_keystrokes_cover_ctrl_v_and_shift_insert() {
        assert!(is_paste_keystroke(&key("shift-insert")));
        // 不带 Shift 的 Insert 不是粘贴（会覆盖字符，本控件不实现）；组合键必须干净。
        assert!(!is_paste_keystroke(&key("insert")));
        assert!(!is_paste_keystroke(&key("ctrl-shift-insert")));
        assert!(!is_paste_keystroke(&key("alt-shift-insert")));
        assert!(!is_paste_keystroke(&key("ctrl-v")));
        assert!(!is_paste_keystroke(&key("shift-a")));
    }
}
