//! 搜索浮层（S4-2a 骨架 → S4-2b 接真实索引）。
//!
//! 数据来自 [`crate::search_service::SearchService`]：后台线程持有 `IndexManager`，
//! UI 只发请求、收结果。本模块负责：
//!
//! - 输入框（`gpui-component` 的 `Input`）+ 结果列表（`List`/`ListItem`）；
//! - **latest-wins**：只有最新 `request_id` 的响应允许上屏（旧响应直接丢）；
//! - 高亮：用 `Hit::snippet_ranges`（`snippet_text` 内的 UTF-8 字节区间）切段上色，
//!   **不碰编辑器坐标**；
//! - 索引状态：准备中 / 就绪 / 降级 / 失败，直接显示在列表上方；
//! - 键盘：`↑/↓` 选择、`Enter` 打开、`Esc` 关闭；点击结果也打开。
//!
//! 打开笔记的动作由宿主实现（[`OpenHandler`]）：浮层只把 `note_id` 交出去，
//! 不关心树怎么找、文档怎么排空。

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use gpui::component::list::{List, ListDelegate, ListItem, ListState};
use gpui::component::IndexPath;
use gpui::prelude::*;
use gpui::{
    div, px, App, Context, Entity, FocusHandle, Focusable, IntoElement, ParentElement, Render,
    SharedString, Styled, Window,
};
use menghunote_search::Hit;

use super::editor::theme;
use super::search_input::{SearchInput, SearchInputEvent};
use super::search_service::{is_latest, SearchReply, SearchService, SearchStatus, DEFAULT_LIMIT};

/// 打开一条搜索结果：`(note_id, window, app)`。
pub type OpenHandler = Rc<dyn Fn(&str, &mut Window, &mut App) + 'static>;

/// 请求关闭浮层的回调。
pub type CloseHandler = Rc<dyn Fn(&mut Window, &mut App) + 'static>;

/// 浮层里共享的展示状态（overlay 写、delegate 读）。
#[derive(Debug, Default)]
pub struct OverlayState {
    /// 当前结果。
    pub hits: Vec<Hit>,
    /// 当前选中行。
    pub selected: usize,
    /// 索引状态。
    pub status: SearchStatus,
    /// 最近一次查询失败的可诊断信息。
    pub error: Option<String>,
}

impl OverlayState {
    /// 选中行（越界返回 `None`）。
    #[must_use]
    pub fn selected_hit(&self) -> Option<&Hit> {
        self.hits.get(self.selected)
    }

    /// 移动选中行（夹在合法区间内）。
    pub fn move_selection(&mut self, delta: isize) {
        if self.hits.is_empty() {
            self.selected = 0;
            return;
        }
        let max = self.hits.len() - 1;
        let next = self.selected as isize + delta;
        self.selected = next.clamp(0, max as isize) as usize;
    }
}

/// 高亮区间 → 渲染用的分段（**纯函数**，单独测）。
///
/// 输入是 `snippet_text` 与它内部的高亮字节区间（可能重叠/乱序），输出是
/// `(文本片段, 是否高亮)` 的有序序列，覆盖整段文本。
#[must_use]
pub fn highlight_segments(text: &str, ranges: &[std::ops::Range<usize>]) -> Vec<(String, bool)> {
    // 先把区间夹到合法范围并排序合并（Tantivy 的区间通常已排序，但不保证）。
    let mut spans: Vec<(usize, usize)> = ranges
        .iter()
        .map(|range| (range.start.min(text.len()), range.end.min(text.len())))
        .filter(|(start, end)| {
            start < end && text.is_char_boundary(*start) && text.is_char_boundary(*end)
        })
        .collect();
    spans.sort_by_key(|(start, _)| *start);
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (start, end) in spans {
        match merged.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }

    let mut out = Vec::new();
    let mut cursor = 0usize;
    for (start, end) in merged {
        if cursor < start {
            out.push((text[cursor..start].to_owned(), false));
        }
        out.push((text[start..end].to_owned(), true));
        cursor = end;
    }
    if cursor < text.len() {
        out.push((text[cursor..].to_owned(), false));
    }
    out
}

/// 列表委托：读共享状态渲染真实结果。
pub struct SearchDelegate {
    state: Rc<RefCell<OverlayState>>,
    on_open: OpenHandler,
}

impl SearchDelegate {
    /// 新建委托。
    pub fn new(state: Rc<RefCell<OverlayState>>, on_open: OpenHandler) -> Self {
        Self { state, on_open }
    }
}

impl ListDelegate for SearchDelegate {
    type Item = ListItem;

    fn items_count(&self, _section: usize, _cx: &App) -> usize {
        self.state.borrow().hits.len()
    }

    fn set_selected_index(
        &mut self,
        ix: Option<IndexPath>,
        _window: &mut Window,
        _cx: &mut Context<ListState<Self>>,
    ) {
        if let Some(path) = ix {
            self.state.borrow_mut().selected = path.row;
        }
    }

    fn render_item(
        &mut self,
        ix: IndexPath,
        _window: &mut Window,
        _cx: &mut Context<ListState<Self>>,
    ) -> Option<Self::Item> {
        let (hit, selected) = {
            let state = self.state.borrow();
            let hit = state.hits.get(ix.row)?.clone();
            (hit, state.selected == ix.row)
        };
        let note_id = hit.note_id.clone();
        let on_open = Rc::clone(&self.on_open);

        // 标题一行 + snippet 一行（snippet 内按 `snippet_ranges` 高亮）。
        let mut snippet = div().flex().flex_row().text_size(px(12.0));
        for (text, highlighted) in highlight_segments(&hit.snippet_text, &hit.snippet_ranges) {
            let mut piece = div().child(SharedString::from(text));
            if highlighted {
                piece = piece
                    .bg(theme::search_highlight_bg())
                    .text_color(theme::search_highlight_text());
            } else {
                piece = piece.text_color(theme::status_bar_text());
            }
            snippet = snippet.child(piece);
        }

        Some(
            ListItem::new(("search-hit", ix.row))
                .selected(selected)
                .px_3()
                .py_2()
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(
                            div()
                                .text_size(px(14.0))
                                .child(SharedString::from(hit.title)),
                        )
                        .child(snippet),
                )
                .on_click(move |_event, window, cx| {
                    on_open(&note_id, window, cx);
                    cx.stop_propagation();
                }),
        )
    }
}

/// 搜索浮层。
pub struct SearchOverlay {
    input: Entity<SearchInput>,
    list: Entity<ListState<SearchDelegate>>,
    state: Rc<RefCell<OverlayState>>,
    service: SearchService,
    /// 是否已把焦点交给输入框（只做一次）。
    focused_once: bool,
    focus_handle: FocusHandle,
    on_close: CloseHandler,
    on_open: OpenHandler,
    /// 已发出但尚未返回的最新请求号。
    latest_request: Rc<RefCell<u64>>,
}

impl SearchOverlay {
    /// 新建浮层并启动后台检索服务。
    pub fn new(
        vault: menghunote_storage::Vault,
        window: &mut Window,
        cx: &mut Context<Self>,
        on_open: OpenHandler,
        on_close: CloseHandler,
    ) -> Self {
        let on_open_for_list = Rc::clone(&on_open);
        // 自绘输入（带 `EntityInputHandler`）：组件 `Input` **没有 IME**，见 search_input.rs 文档。
        let input = cx.new(SearchInput::new);
        let state = Rc::new(RefCell::new(OverlayState::default()));
        let list = cx.new(|cx| {
            ListState::new(
                SearchDelegate::new(Rc::clone(&state), on_open_for_list),
                window,
                cx,
            )
        });
        let service = SearchService::start(vault);
        let latest_request = Rc::new(RefCell::new(0u64));

        // 输入变化 → 发查询（debounce 在后台做；这里只发，不做节流）。
        // 输入框自己处理 IME 组合：组合中 `Esc` 只取消组合、`Enter` 只上屏，
        // 只有"组合外"的这两个键才会作为事件抛到这里。
        let on_close_for_input = Rc::clone(&on_close);
        cx.subscribe_in(
            &input,
            window,
            move |this, _input, event: &SearchInputEvent, window, cx| match event {
                SearchInputEvent::Changed => this.request_query(cx),
                SearchInputEvent::Submit => this.open_selected(window, cx),
                SearchInputEvent::Escape => on_close_for_input(window, cx),
            },
        )
        .detach();

        // 定时把后台结果取回来；UI 线程只读 channel，不碰索引。
        cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(super::search_service::TICK_MS))
                .await;
            let Some(this) = this.upgrade() else { break };
            this.update(cx, |this, cx| {
                this.drain_replies(cx);
            });
        })
        .detach();

        Self {
            input,
            list,
            state,
            service,
            focused_once: false,
            focus_handle: cx.focus_handle(),
            on_close,
            on_open,
            latest_request,
        }
    }

    /// 把焦点交给输入框（打开浮层时调用一次）。
    pub fn focus_input(&self, window: &mut Window, cx: &mut App) {
        self.input.update(cx, |input, cx| input.focus(window, cx));
    }

    /// 发一次查询（空查询直接清空结果，不打后台）。
    fn request_query(&mut self, cx: &mut Context<Self>) {
        let query = self.input.read(cx).text().to_owned();
        if query.trim().is_empty() {
            let mut state = self.state.borrow_mut();
            state.hits.clear();
            state.selected = 0;
            state.error = None;
            drop(state);
            self.list.update(cx, |_list, cx| cx.notify());
            cx.notify();
            return;
        }
        let request_id = self.service.request(query, DEFAULT_LIMIT);
        *self.latest_request.borrow_mut() = request_id;
        cx.notify();
    }

    /// 取回后台消息：状态、结果、错误。
    fn drain_replies(&mut self, cx: &mut Context<Self>) {
        let latest = self.service.latest_request();
        let replies = self.service.poll();
        if replies.is_empty() {
            return;
        }
        let mut changed = false;
        for reply in replies {
            match reply {
                SearchReply::Status(status) => {
                    changed = true;
                    self.state.borrow_mut().status = status;
                }
                SearchReply::Hits { request_id, hits } => {
                    // **latest-wins**：旧请求的响应必须丢弃（否则会闪回旧结果）。
                    if !is_latest(latest, request_id) {
                        continue;
                    }
                    changed = true;
                    let mut state = self.state.borrow_mut();
                    state.hits = hits;
                    state.selected = 0;
                    state.error = None;
                }
                SearchReply::Failed {
                    request_id,
                    message,
                } => {
                    if !is_latest(latest, request_id) {
                        continue;
                    }
                    changed = true;
                    let mut state = self.state.borrow_mut();
                    state.hits.clear();
                    state.selected = 0;
                    state.error = Some(message);
                }
            }
        }
        if changed {
            self.list.update(cx, |_list, cx| cx.notify());
            cx.notify();
        }
    }

    /// 打开当前选中（没有选中就第一项）；没有结果时什么都不做。
    fn open_selected(&mut self, window: &mut Window, cx: &mut App) {
        let note_id = {
            let state = self.state.borrow();
            state.selected_hit().map(|hit| hit.note_id.clone())
        };
        if let Some(note_id) = note_id {
            // 宿主负责跳转；它决定成功后是否关浮层。
            let handler = Rc::clone(&self.on_open);
            handler(&note_id, window, cx);
        }
    }

    /// 状态行文案（准备中/降级/失败）。
    fn status_line(&self) -> Option<(String, bool)> {
        let state = self.state.borrow();
        match &state.status {
            SearchStatus::Preparing => Some(("索引准备中…".to_owned(), false)),
            SearchStatus::Ready => None,
            SearchStatus::Degraded(reason) => Some((format!("索引降级：{reason}"), true)),
            SearchStatus::Failed(reason) => Some((reason.clone(), true)),
        }
    }
}

impl Focusable for SearchOverlay {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for SearchOverlay {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.focused_once {
            self.focused_once = true;
            let handle = self.input.read(cx).focus_handle(cx);
            window.defer(cx, move |window, cx| {
                handle.focus(window, cx);
            });
        }

        let on_close = Rc::clone(&self.on_close);
        let error = self.state.borrow().error.clone();
        let status = self.status_line();
        let empty = self.state.borrow().hits.is_empty();

        div()
            .id("search-overlay")
            .absolute()
            .inset_0()
            .flex()
            .items_start()
            .justify_center()
            .pt(px(96.0))
            .bg(gpui::rgba(0x00000026))
            .track_focus(&self.focus_handle)
            // 点空白（遮罩）= 关闭浮层；面板内部自己拦冒泡。
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener({
                    let on_close = Rc::clone(&self.on_close);
                    move |_this, _event: &gpui::MouseDownEvent, window, cx| {
                        on_close(window, cx);
                    }
                }),
            )
            .on_key_down(
                cx.listener(move |this, event: &gpui::KeyDownEvent, window, cx| {
                    match event.keystroke.key.as_str() {
                        "escape" => {
                            on_close(window, cx);
                            cx.stop_propagation();
                        }
                        "up" => {
                            this.state.borrow_mut().move_selection(-1);
                            this.list.update(cx, |_list, cx| cx.notify());
                            cx.notify();
                            cx.stop_propagation();
                        }
                        "down" => {
                            this.state.borrow_mut().move_selection(1);
                            this.list.update(cx, |_list, cx| cx.notify());
                            cx.notify();
                            cx.stop_propagation();
                        }
                        "enter" => {
                            this.open_selected(window, cx);
                            cx.stop_propagation();
                        }
                        _ => {}
                    }
                }),
            )
            .child(
                div()
                    .w(px(560.0))
                    .rounded_md()
                    .bg(theme::menu_bg())
                    .border_1()
                    .border_color(theme::sidebar_border())
                    .shadow_lg()
                    .p_3()
                    .flex()
                    .flex_col()
                    .gap_2()
                    // 点面板内部不算"点空白"。
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(|_this, _event: &gpui::MouseDownEvent, _window, cx| {
                            cx.stop_propagation();
                        }),
                    )
                    .child(
                        div()
                            .rounded_sm()
                            .border_1()
                            .border_color(theme::input_border())
                            .bg(theme::input_bg())
                            .px_2()
                            .child(self.input.clone()),
                    )
                    .when_some(status, |this, (text, warn)| {
                        this.child(
                            div()
                                .text_size(px(12.0))
                                .text_color(if warn {
                                    theme::search_warn_text()
                                } else {
                                    theme::status_bar_text()
                                })
                                .child(SharedString::from(text)),
                        )
                    })
                    .when_some(error, |this, message| {
                        this.child(
                            div()
                                .text_size(px(12.0))
                                .text_color(theme::search_warn_text())
                                .child(SharedString::from(message)),
                        )
                    })
                    .child(
                        // 列表固定高度；空状态**叠在**列表上，避免把卡片撑高。
                        div()
                            .relative()
                            .h(px(320.0))
                            .child(List::new(&self.list))
                            .when(empty, |this| {
                                this.child(
                                    div()
                                        .absolute()
                                        .top(px(8.0))
                                        .left(px(12.0))
                                        .text_size(px(12.0))
                                        .text_color(theme::status_bar_text())
                                        .child(SharedString::from("没有匹配的笔记")),
                                )
                            }),
                    ),
            )
            .into_any_element()
    }
}

#[cfg(test)]
// 这些用例就要传"只有一个高亮区间"，clippy 的 single_range_in_vec_init 在这里是噪音。
#[allow(clippy::single_range_in_vec_init)]
mod tests {
    use super::highlight_segments;

    #[test]
    fn no_ranges_returns_single_plain_segment() {
        let segments = highlight_segments("纯文本片段", &[]);
        assert_eq!(segments, vec![("纯文本片段".to_owned(), false)]);
    }

    #[test]
    fn ranges_are_highlighted_and_cover_the_whole_text() {
        // "hello world"：hello(0..5) 与 world(6..11) 都高亮
        let segments = highlight_segments("hello world", &[0..5, 6..11]);
        assert_eq!(
            segments,
            vec![
                ("hello".to_owned(), true),
                (" ".to_owned(), false),
                ("world".to_owned(), true),
            ]
        );
        let joined: String = segments.into_iter().map(|(text, _)| text).collect();
        assert_eq!(joined, "hello world", "分段必须完整覆盖原文");
    }

    #[test]
    fn overlapping_and_unordered_ranges_are_merged() {
        let segments = highlight_segments("abcdef", &[3..6, 1..3]);
        assert_eq!(
            segments,
            vec![("a".to_owned(), false), ("bcdef".to_owned(), true)]
        );
    }

    #[test]
    fn out_of_bounds_ranges_are_clamped_and_broken_ones_dropped() {
        // 越界区间夹到文本末尾（不是丢弃）；切在多字节字符中间的区间则直接丢掉。
        let wide = [0..99usize];
        let clamped = highlight_segments("中文abc", &wide);
        assert_eq!(clamped, vec![("中文abc".to_owned(), true)]);

        let inside_char = [1..2usize];
        let dropped = highlight_segments("中文abc", &inside_char);
        assert_eq!(dropped, vec![("中文abc".to_owned(), false)]);
    }

    #[test]
    fn chinese_range_highlights_whole_char() {
        // "中文" 是 0..6 字节；只高亮这两个字
        let whole_prefix = [0..6usize];
        let segments = highlight_segments("中文abc", &whole_prefix);
        assert_eq!(
            segments,
            vec![("中文".to_owned(), true), ("abc".to_owned(), false)]
        );
    }
}
