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

use gpui::prelude::*;
use gpui::{
    div, px, App, Context, Entity, FocusHandle, Focusable, IntoElement, ParentElement, Render,
    SharedString, Styled, Window,
};
use menghunote_search::Hit;

use super::editor::theme;
use super::search_input::{SearchInput, SearchInputEvent};
use super::search_service::{
    is_latest, QueryOptions, SearchReply, SearchService, SearchStatus, DEFAULT_LIMIT,
};

/// 打开一条搜索结果：`(note_id, window, app)`。
pub type OpenHandler = Rc<dyn Fn(&str, &mut Window, &mut App) + 'static>;

/// 请求关闭浮层的回调。
pub type CloseHandler = Rc<dyn Fn(&mut Window, &mut App) + 'static>;

/// 浮层里共享的展示状态（overlay 写、delegate 读）。
#[derive(Debug, Default)]
pub struct OverlayState {
    /// 当前结果（按笔记分组）。
    pub groups: Vec<HitGroup>,
    /// 当前选中的分组。
    pub selected: usize,
    /// 索引状态。
    pub status: SearchStatus,
    /// 最近一次查询失败的可诊断信息。
    pub error: Option<String>,
    /// 命中总数（`hits` 只保留 limit 条）。
    pub total: usize,
    /// 当前查询选项（大小写 / 整词）。
    pub options: QueryOptions,
    /// 上一帧的状态（用于"构建完成 → 自动重查一次"）。
    pub previous_status: Option<SearchStatus>,
    /// 已展开的笔记（按 note_id；会话内保持，重查后仍在）。
    pub expanded: std::collections::HashSet<String>,
}

impl OverlayState {
    /// 选中的分组（越界返回 `None`）。
    #[must_use]
    pub fn selected_group(&self) -> Option<&HitGroup> {
        self.groups.get(self.selected)
    }

    /// 移动选中行（夹在合法区间内）。
    pub fn move_selection(&mut self, delta: isize) {
        if self.groups.is_empty() {
            self.selected = 0;
            return;
        }
        let max = self.groups.len() - 1;
        let next = self.selected as isize + delta;
        self.selected = next.clamp(0, max as isize) as usize;
    }
}

/// 状态行/总数行的文案（**纯函数**，可单测）。
///
/// 优先级：失败 > 降级 > 构建中 > 结果总数。构建中给「阶段 + 进度」，
/// 总数只在就绪时才有意义（0 条显示"没有匹配的笔记"）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatusLine {
    /// 什么都不显示。
    Hidden,
    /// 一行文字（失败/降级/总数）。
    Text(String),
    /// 进度条 + 阶段文案。
    Progress {
        /// 阶段 + 数量，例如 `建立索引 320/2000`。
        label: String,
        /// 百分比；`None` = 总数未知（不确定态，**不要画成 0%**）。
        percent: Option<u32>,
    },
}

/// 根据索引状态与命中总数算状态行（**纯函数**）。
#[must_use]
pub fn status_line_for(status: &SearchStatus, total: usize) -> StatusLine {
    match status {
        SearchStatus::Failed(message) => StatusLine::Text(message.clone()),
        SearchStatus::Degraded(reason) => StatusLine::Text(format!("索引降级：{reason}")),
        SearchStatus::Preparing => StatusLine::Progress {
            label: "索引准备中…".to_owned(),
            percent: None,
        },
        SearchStatus::Progress {
            phase,
            done,
            total: stage_total,
        } => {
            let percent = super::search_service::progress_percent(*done, *stage_total);
            let label = if *stage_total == 0 {
                format!("{}…", phase.label())
            } else {
                format!("{} {done}/{stage_total}", phase.label())
            };
            StatusLine::Progress { label, percent }
        }
        SearchStatus::Ready => StatusLine::Text(count_text(total)),
    }
}

/// 结果总数文案（**纯函数**）。
#[must_use]
pub fn count_text(total: usize) -> String {
    if total == 0 {
        "没有匹配的笔记".to_owned()
    } else {
        format!("{total} 条结果")
    }
}

/// 点 `Aa` 开关后的选项（**纯函数**）。
#[must_use]
pub fn toggle_case_sensitive(options: QueryOptions) -> QueryOptions {
    QueryOptions {
        case_sensitive: !options.case_sensitive,
        ..options
    }
}

/// 点 `整词` 开关后的选项（**纯函数**）。
#[must_use]
pub fn toggle_whole_word(options: QueryOptions) -> QueryOptions {
    QueryOptions {
        whole_word: !options.whole_word,
        ..options
    }
}

/// 一条命中行（单行展示，`ranges` 绑定 `text` 的字节区间）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnippetLine {
    /// 行文本（纯文本）。
    pub text: String,
    /// 高亮区间（UTF-8 字节，绑定 `text`）。
    pub ranges: Vec<std::ops::Range<usize>>,
}

/// 一个笔记分组：标题 + 全部命中行（折叠态只渲染前 [`COLLAPSED_LINES`] 行）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HitGroup {
    /// 笔记 ID（打开用）。
    pub note_id: String,
    /// 标题（单行）。
    pub title: String,
    /// 该笔记的命中行（全量；折叠只发生在渲染层）。
    pub lines: Vec<SnippetLine>,
    /// 这篇笔记一共有多少处命中（可能大于 `lines.len()`）。
    pub matched_lines: usize,
}

/// 折叠态下每组默认显示几行（产品口径：3 行）；展开后显示**全部**命中行。
pub const COLLAPSED_LINES: usize = 3;

/// 折叠态显示前 3 行；展开态显示全部（**纯函数**，可单测）。
#[must_use]
pub fn visible_line_count(total_lines: usize, expanded: bool) -> usize {
    if expanded {
        total_lines
    } else {
        total_lines.min(COLLAPSED_LINES)
    }
}

/// 头部右侧的命中计数文案（**纯函数**）。
#[must_use]
pub fn header_count_text(matched_lines: usize) -> String {
    format!("{matched_lines} 处命中")
}

/// 折叠按钮文案（**纯函数**）。
#[must_use]
pub fn fold_button_text(expanded: bool) -> &'static str {
    if expanded {
        "收起"
    } else {
        "展开"
    }
}

/// 折叠提示文案（**纯函数**）：折叠态且还有没显示的行时，给一句"还有多少"。
///
/// 展开态（或行数本来就 ≤ 3）返回 `None`——全部都在屏幕上了，不再提示。
#[must_use]
pub fn collapsed_hint_text(matched_lines: usize, shown: usize, expanded: bool) -> Option<String> {
    if expanded || matched_lines <= shown {
        return None;
    }
    Some(format!("还有 {} 行，点标题展开", matched_lines - shown))
}

/// 把一条 `Hit` 收敛成展示用分组（**纯函数**）。
///
/// **不截断**：`snippets` 全部收下（S5-5 起 search 侧不再限制上限），
/// 折叠只发生在渲染层（[`visible_line_count`]），展开后能看到所有命中行。
#[must_use]
pub fn group_from_hit(hit: &Hit) -> HitGroup {
    let lines: Vec<SnippetLine> = hit
        .snippets
        .iter()
        .map(|line| SnippetLine {
            text: line.text.clone(),
            ranges: line.ranges.clone(),
        })
        .collect();
    HitGroup {
        note_id: hit.note_id.clone(),
        title: hit.title.clone(),
        lines,
        matched_lines: hit.matched_lines.max(hit.snippets.len()),
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

/// 搜索浮层。
pub struct SearchOverlay {
    input: Entity<SearchInput>,
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
        peer_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
        on_open: OpenHandler,
        on_close: CloseHandler,
    ) -> Self {
        // 自绘输入（带 `EntityInputHandler`）：组件 `Input` **没有 IME**，见 search_input.rs 文档。
        let input = cx.new(SearchInput::new);
        let state = Rc::new(RefCell::new(OverlayState::default()));

        let service = SearchService::start(vault, peer_id);
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

        let mut overlay = Self {
            input,
            state,
            service,
            focused_once: false,
            focus_handle: cx.focus_handle(),
            on_close,
            on_open,
            latest_request,
        };

        // 开发开关：`MENGHUNOTE_SEARCH_QUERY=软` 预填查询并发起一次检索，
        // 给截图/取证用（自动化注入不了中文输入法）。
        if let Some(query) = std::env::var_os("MENGHUNOTE_SEARCH_QUERY") {
            let query = query.to_string_lossy().to_string();
            if !query.is_empty() {
                overlay.input.update(cx, |input, cx| {
                    input.set_text(&query);
                    cx.notify();
                });
                overlay.request_query(cx);
            }
        }
        overlay
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
            state.groups.clear();
            state.selected = 0;
            state.error = None;
            drop(state);
            cx.notify();
            return;
        }
        let options = self.state.borrow().options;
        let request_id = self.service.request(query, DEFAULT_LIMIT, options);
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
                    // 构建进度可能来自后台线程，**晚于 Ready 到达**（实测过：界面停在
                    // "建立索引 2/2（100%）"不退）。就绪之后忽略一切过期进度。
                    let stale_progress = {
                        let state = self.state.borrow();
                        matches!(status, SearchStatus::Progress { .. })
                            && matches!(state.status, SearchStatus::Ready)
                    };
                    if stale_progress {
                        continue;
                    }
                    changed = true;
                    self.state.borrow_mut().status = status;
                }
                SearchReply::Hits {
                    request_id,
                    total,
                    hits,
                } => {
                    // **latest-wins**：旧请求的响应必须丢弃（否则会闪回旧结果）。
                    if !is_latest(latest, request_id) {
                        continue;
                    }
                    changed = true;
                    let mut state = self.state.borrow_mut();
                    state.groups = hits.iter().map(group_from_hit).collect();
                    state.total = total;
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
                    state.groups.clear();
                    state.selected = 0;
                    state.error = Some(message);
                }
            }
        }
        // 构建完成（非 Ready → Ready）时对当前输入**自动重查一次**：
        // 建索引期间发出去的查询可能还没结果，完成后不重查会一直显示空。
        let became_ready = {
            let mut state = self.state.borrow_mut();
            let current = state.status.clone();
            let previous = state.previous_status.replace(current);
            matches!(state.status, SearchStatus::Ready)
                && !matches!(previous, Some(SearchStatus::Ready) | None)
        };
        if became_ready {
            changed = true;
            self.request_query(cx);
        }

        if changed {
            cx.notify();
        }
    }

    /// 打开当前选中（没有选中就第一项）；没有结果时什么都不做。
    fn open_selected(&mut self, window: &mut Window, cx: &mut App) {
        let note_id = {
            let state = self.state.borrow();
            state.selected_group().map(|group| group.note_id.clone())
        };
        if let Some(note_id) = note_id {
            // 宿主负责跳转；它决定成功后是否关浮层。
            let handler = Rc::clone(&self.on_open);
            handler(&note_id, window, cx);
        }
    }

    /// 渲染一个笔记分组。
    ///
    /// - 标题行右侧：`N 处命中` + `展开/收起`；**点标题行 = 折叠切换**（不打开笔记）；
    /// - 命中行：折叠态前 3 行、展开态全部（单行省略 + `snippet_ranges` 高亮）；
    /// - 点命中行 = 打开该笔记（保留"点结果即打开"的既有行为）。
    fn render_group(
        &self,
        index: usize,
        group: &HitGroup,
        selected: bool,
        expanded: bool,
        on_open: OpenHandler,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let note_id_for_body = group.note_id.clone();
        let on_open_for_body = Rc::clone(&on_open);
        let note_id_for_toggle = group.note_id.clone();
        let shown = visible_line_count(group.lines.len(), expanded);

        let header = div()
            .id(("search-group-header", index))
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap_2()
            .cursor_pointer()
            .child(
                div()
                    .flex_1()
                    .text_size(px(14.0))
                    .whitespace_nowrap()
                    .child(SharedString::from(group.title.clone())),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(theme::status_bar_text())
                            .child(SharedString::from(header_count_text(group.matched_lines))),
                    )
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(theme::search_toggle_active())
                            .child(SharedString::from(fold_button_text(expanded))),
                    ),
            )
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, _event: &gpui::MouseDownEvent, _window, cx| {
                    if crate::shell::trace_mouse_enabled() {
                        eprintln!("[menghunote] 点分组标题 #{index}（切换折叠）");
                    }
                    this.state.borrow_mut().selected = index;
                    let note_id = note_id_for_toggle.clone();
                    {
                        let mut state = this.state.borrow_mut();
                        if !state.expanded.remove(&note_id) {
                            state.expanded.insert(note_id);
                        }
                    }
                    cx.notify();
                    cx.stop_propagation();
                }),
            );

        let mut card = div()
            .id(("search-group", index))
            .px_3()
            .py_2()
            .rounded_sm()
            .flex()
            .flex_col()
            .gap_1()
            .cursor_pointer()
            .when(selected, |this| this.bg(theme::search_group_selected()))
            .child(header);

        // 命中行放在**独立的容器**里接点击（与标题行是兄弟而非祖先关系），
        // 这样不依赖冒泡顺序：点标题=折叠，点命中行=打开笔记。
        let mut body = div()
            .id(("search-group-body", index))
            .flex()
            .flex_col()
            .gap_1()
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, _event: &gpui::MouseDownEvent, window, cx| {
                    if crate::shell::trace_mouse_enabled() {
                        eprintln!("[menghunote] 点分组正文 #{index}（打开笔记）");
                    }
                    this.state.borrow_mut().selected = index;
                    let handler = Rc::clone(&on_open_for_body);
                    handler(&note_id_for_body, window, cx);
                    cx.stop_propagation();
                }),
            );
        for line in group.lines.iter().take(shown) {
            let mut row = div()
                .flex()
                .flex_row()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_size(px(12.0));
            for (text, highlighted) in highlight_segments(&line.text, &line.ranges) {
                let mut piece = div().child(SharedString::from(text));
                if highlighted {
                    piece = piece
                        .bg(theme::search_highlight_bg())
                        .text_color(theme::search_highlight_text());
                } else {
                    piece = piece.text_color(theme::status_bar_text());
                }
                row = row.child(piece);
            }
            body = body.child(row);
        }
        card = card.child(body);

        if let Some(hint) = collapsed_hint_text(group.matched_lines, shown, expanded) {
            card = card.child(
                div()
                    .text_size(px(11.0))
                    .text_color(theme::status_bar_text())
                    .child(SharedString::from(hint)),
            );
        }

        card.into_any_element()
    }

    /// 渲染状态行（文字或进度条）。
    fn render_status_line(&self, status: &StatusLine) -> gpui::AnyElement {
        match status {
            StatusLine::Hidden => div().into_any_element(),
            StatusLine::Text(text) => div()
                .text_size(px(12.0))
                .text_color(theme::status_bar_text())
                .child(SharedString::from(text.clone()))
                .into_any_element(),
            StatusLine::Progress { label, percent } => {
                let fill = match percent {
                    // 确定态：按百分比填充。
                    Some(pct) => (f64::from(*pct) / 100.0).clamp(0.0, 1.0),
                    // 不确定态：固定一段中等长度，避免被读成 0%。
                    None => 0.35,
                };
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(theme::status_bar_text())
                            .child(SharedString::from(match percent {
                                Some(pct) => format!("{label}（{pct}%）"),
                                None => label.clone(),
                            })),
                    )
                    .child(
                        div()
                            .h(px(4.0))
                            .w_full()
                            .rounded_full()
                            .bg(theme::search_progress_track())
                            .child(
                                div()
                                    .h(px(4.0))
                                    .rounded_full()
                                    .bg(theme::search_progress_fill())
                                    .w(gpui::relative(fill as f32)),
                            ),
                    )
                    .into_any_element()
            }
        }
    }

    /// 渲染两个开关（`Aa` / `整词`）。
    fn render_toggles(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let options = self.state.borrow().options;
        let mut row = div().flex().flex_row().gap_2();
        for (label, active, is_case) in [
            ("Aa", options.case_sensitive, true),
            ("整词", options.whole_word, false),
        ] {
            let mut button = div()
                .id(if is_case {
                    "toggle-case"
                } else {
                    "toggle-whole-word"
                })
                .px_2()
                .h(px(22.0))
                .rounded_sm()
                .border_1()
                .text_size(px(12.0))
                .flex()
                .items_center()
                .cursor_pointer()
                .child(SharedString::from(label));
            if active {
                button = button
                    .bg(theme::search_toggle_active())
                    .border_color(theme::search_toggle_active())
                    .text_color(theme::search_toggle_active_text());
            } else {
                button = button
                    .bg(theme::input_bg())
                    .border_color(theme::input_border())
                    .text_color(theme::status_bar_text());
            }
            row = row.child(button.on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, _event: &gpui::MouseDownEvent, _window, cx| {
                    // 切开关 = 立即重查（request_id 递增，旧响应按 latest-wins 丢弃）。
                    {
                        let mut state = this.state.borrow_mut();
                        state.options = if is_case {
                            toggle_case_sensitive(state.options)
                        } else {
                            toggle_whole_word(state.options)
                        };
                    }
                    this.request_query(cx);
                    cx.notify();
                    cx.stop_propagation();
                }),
            ));
        }
        row.into_any_element()
    }

    /// 状态行（准备中/构建进度/降级/失败/总数）。
    fn status_line(&self) -> StatusLine {
        let state = self.state.borrow();
        status_line_for(&state.status, state.total)
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
        let empty = self.state.borrow().groups.is_empty();

        div()
            .id("search-overlay")
            // 测试元数据（只读断言用）：浮层存在 = 打开；`debug_bounds` 为 `None` = 关闭。
            .debug_selector(|| "search-overlay".to_string())
            .absolute()
            .inset_0()
            .flex()
            .items_start()
            .justify_center()
            .pt(px(96.0))
            .bg(theme::overlay_scrim())
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
                            cx.notify();
                            cx.stop_propagation();
                        }
                        "down" => {
                            this.state.borrow_mut().move_selection(1);
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
                    // 两个开关：Aa（大小写敏感）/ 整词（关 = 子串）。
                    .child(self.render_toggles(cx))
                    .child(self.render_status_line(&status))
                    .when_some(error, |this, message| {
                        this.child(
                            div()
                                .text_size(px(12.0))
                                .text_color(theme::search_warn_text())
                                .child(SharedString::from(message)),
                        )
                    })
                    .child({
                        // 结果区：**自己用 div 列表 + 滚动**，不用 `gpui-component` 的 `List`。
                        //
                        // 取舍：`List` 是虚拟列表，要求"每一项等高"（其文档里明确写了），
                        // 而这里一个笔记分组的高度是 1–3 行摘要 + 可选统计行，天然不等高。
                        // 硬凑成固定 4 行会在多数分组上留大片空白。结果最多 20 个分组，
                        // 不需要虚拟化，所以直接 `overflow_y_scroll` + 每项一个 div。
                        let selected = self.state.borrow().selected;
                        let mut column = div()
                            .id("search-results")
                            // 测试元数据（只读断言用）：结果列表容器（空态也在）。
                            .debug_selector(|| "search-results".to_string())
                            .h(px(320.0))
                            .overflow_y_scroll()
                            .flex()
                            .flex_col()
                            .gap_2();
                        if empty {
                            column = column.child(
                                div()
                                    // 测试元数据（只读断言用）：空态占位。
                                    .debug_selector(|| "search-empty".to_string())
                                    .text_size(px(12.0))
                                    .text_color(theme::status_bar_text())
                                    .child(SharedString::from("")),
                            );
                        } else {
                            let on_open = Rc::clone(&self.on_open);
                            let groups: Vec<HitGroup> = self.state.borrow().groups.clone();
                            for (index, group) in groups.iter().enumerate() {
                                let expanded =
                                    self.state.borrow().expanded.contains(&group.note_id);
                                column = column.child(self.render_group(
                                    index,
                                    group,
                                    index == selected,
                                    expanded,
                                    Rc::clone(&on_open),
                                    cx,
                                ));
                            }
                        }
                        column
                    }),
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

#[cfg(test)]
mod status_tests {
    use super::{
        count_text, status_line_for, toggle_case_sensitive, toggle_whole_word, StatusLine,
    };
    use crate::search_service::{BuildPhase, QueryOptions, SearchStatus};

    #[test]
    fn count_text_says_n_results_or_empty() {
        assert_eq!(count_text(0), "没有匹配的笔记");
        assert_eq!(count_text(1), "1 条结果");
        assert_eq!(count_text(42), "42 条结果");
    }

    #[test]
    fn status_takes_priority_over_count() {
        // 准备中 → 进度行，不显示"0 条结果"
        let preparing = status_line_for(&SearchStatus::Preparing, 0);
        assert!(matches!(
            preparing,
            StatusLine::Progress { percent: None, .. }
        ));

        // 构建中带确定性进度
        let building = status_line_for(
            &SearchStatus::Progress {
                phase: BuildPhase::Index,
                done: 320,
                total: 2000,
            },
            0,
        );
        match building {
            StatusLine::Progress { label, percent } => {
                assert!(label.contains("320/2000"), "阶段文案应带数量：{label}");
                assert_eq!(percent, Some(16));
            }
            other => panic!("期望进度行，实际 {other:?}"),
        }

        // 降级 / 失败优先于总数
        assert!(matches!(
            status_line_for(&SearchStatus::Degraded("缺 3 篇".to_owned()), 5),
            StatusLine::Text(text) if text.contains("索引降级")
        ));
        assert!(matches!(
            status_line_for(&SearchStatus::Failed("炸了".to_owned()), 5),
            StatusLine::Text(text) if text == "炸了"
        ));

        // 就绪 → 总数
        assert_eq!(
            status_line_for(&SearchStatus::Ready, 12),
            StatusLine::Text("12 条结果".to_owned())
        );
    }

    #[test]
    fn unknown_total_never_shows_zero_percent() {
        let line = status_line_for(
            &SearchStatus::Progress {
                phase: BuildPhase::Materialize,
                done: 0,
                total: 0,
            },
            0,
        );
        match line {
            StatusLine::Progress { label, percent } => {
                assert_eq!(percent, None, "未知总量必须走不确定态，不能显示 0%");
                assert!(label.contains("补齐正文"), "阶段名要对：{label}");
            }
            other => panic!("期望进度行，实际 {other:?}"),
        }
    }

    #[test]
    fn toggles_flip_only_their_own_option() {
        let base = QueryOptions::default();
        let case = toggle_case_sensitive(base);
        assert!(case.case_sensitive);
        assert_eq!(case.whole_word, base.whole_word);

        let word = toggle_whole_word(base);
        assert!(word.whole_word);
        assert_eq!(word.case_sensitive, base.case_sensitive);

        // 再点一次回到原状
        assert_eq!(toggle_case_sensitive(case), base);
        assert_eq!(toggle_whole_word(word), base);
    }
}

#[cfg(test)]
// 单条高亮区间在用例里是常态，single_range_in_vec_init 这里是噪音。
#[allow(clippy::single_range_in_vec_init)]
mod grouping_tests {
    use super::{
        collapsed_hint_text, fold_button_text, group_from_hit, header_count_text,
        visible_line_count, HitGroup, SnippetLine, COLLAPSED_LINES,
    };
    use menghunote_search::{Hit, SnippetLine as SearchLine};

    fn hit_with(lines: usize, matched: usize) -> Hit {
        Hit {
            note_id: "note-1".to_owned(),
            title: "一篇笔记".to_owned(),
            snippets: (0..lines)
                .map(|index| SearchLine {
                    text: format!("第 {index} 行命中"),
                    ranges: vec![0..1],
                    line_index: index,
                })
                .collect(),
            matched_lines: matched,
            score: 1.0,
        }
    }

    #[test]
    fn group_keeps_every_line_from_search() {
        // S5-5 取消上限后，分组必须保留**全部**命中行（折叠只在渲染层做）。
        let group = group_from_hit(&hit_with(6, 6));
        assert_eq!(group.lines.len(), 6);
        assert_eq!(group.matched_lines, 6);
    }

    #[test]
    fn collapsed_shows_first_three_and_expanded_shows_all() {
        assert_eq!(visible_line_count(6, false), COLLAPSED_LINES);
        assert_eq!(visible_line_count(6, true), 6, "展开后显示全部");
        assert_eq!(visible_line_count(2, false), 2, "不足 3 行就全显示");
        assert_eq!(visible_line_count(0, true), 0);
    }

    #[test]
    fn collapse_hint_only_when_lines_are_hidden() {
        assert_eq!(
            collapsed_hint_text(6, 3, false),
            Some("还有 3 行，点标题展开".to_owned())
        );
        assert_eq!(collapsed_hint_text(6, 3, true), None, "展开后不再提示");
        assert_eq!(collapsed_hint_text(2, 2, false), None, "都显示完了不提示");
    }

    #[test]
    fn header_text_reports_total_hits_and_fold_button() {
        assert_eq!(header_count_text(6), "6 处命中");
        assert_eq!(header_count_text(1), "1 处命中");
        assert_eq!(fold_button_text(false), "展开");
        assert_eq!(fold_button_text(true), "收起");
    }

    #[test]
    fn selection_moves_by_group_not_by_line() {
        let mut state = super::OverlayState {
            groups: vec![
                HitGroup {
                    note_id: "a".to_owned(),
                    title: "a".to_owned(),
                    lines: vec![SnippetLine {
                        text: "x".to_owned(),
                        ranges: Vec::new(),
                    }],
                    matched_lines: 1,
                },
                HitGroup {
                    note_id: "b".to_owned(),
                    title: "b".to_owned(),
                    lines: (0..5)
                        .map(|index| SnippetLine {
                            text: format!("y{index}"),
                            ranges: Vec::new(),
                        })
                        .collect(),
                    matched_lines: 5,
                },
            ],
            ..Default::default()
        };
        assert_eq!(
            state.selected_group().map(|g| g.note_id.as_str()),
            Some("a")
        );
        state.move_selection(1);
        assert_eq!(
            state.selected_group().map(|g| g.note_id.as_str()),
            Some("b"),
            "下一组是第 2 个笔记，不是它的第 2 行"
        );
        state.move_selection(10);
        assert_eq!(state.selected, 1);
    }
}
