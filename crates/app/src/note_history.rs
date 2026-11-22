//! 笔记「历史记录…」时间线覆盖层（ADR 0024 H1，本机历史）。
//!
//! ## 用户路径
//!
//! 1. 侧栏笔记右键 →「历史记录…」（[`MENU_HISTORY_LABEL`]）→ 打开本覆盖层；
//! 2. 左侧按时间倒序列出本机捕获的版本（时间、标题、来源，可选设备）；
//! 3. 点击一行 → 右侧只读预览该版本正文（[`NoteHistoryPanel::preview_body`]）；
//! 4. 「恢复为新笔记」→ 由宿主创建**根级新笔记**并把历史正文写进去；本覆盖层不碰
//!    当前笔记、不改写 CRDT 历史。
//!
//! ## 数据来源
//!
//! 只读本地 SQLite（[`menghunote_storage::list_note_versions`] /
//! [`menghunote_storage::load_note_version_for_note`]）：不查询 relay、不改同步 wire。
//! 打开面板**不会**捕获新版本（捕获时机在 `AppShell` 的 2s 定时器/切换/关窗）。
//!
//! ## selector（headless 测试点击/断言用）
//!
//! | selector | 含义 |
//! | --- | --- |
//! | `note-history` | 覆盖层根（遮罩） |
//! | `note-history-panel` | 面板本体 |
//! | `note-history-row-<N>` | 左侧第 N 行（0 = 最新） |
//! | `note-history-preview` | 右侧只读预览容器 |
//! | `note-history-restore` |「恢复为新笔记」按钮 |
//! | `note-history-close` |「关闭」按钮 |
//! | `note-history-empty` | 空态/加载失败提示 |
//!
//! ## 已知限制（v1）
//!
//! - 没有 diff/分页/搜索；一次最多展示最近
//!   [`menghunote_storage::HISTORY_MAX_VERSIONS_PER_NOTE`] 条；
//! - 不展示跨设备时间线（`device_id` 当前为 `None`）；
//! - 像素/DPI/真实窗口交互仍需 Windows 真机验收（headless 只证明状态与 selector）。

use chrono::{Local, TimeZone};
use gpui::prelude::*;
use gpui::{
    div, px, relative, AnyElement, Context, EventEmitter, FocusHandle, IntoElement, KeyDownEvent,
    MouseButton, Render, SharedString, Window,
};
use menghunote_storage::{
    list_note_versions, load_note_version_for_note, HistoryVersion, HistoryVersionSummary, Vault,
    HISTORY_MAX_VERSIONS_PER_NOTE, HISTORY_SOURCE_LOCAL,
};

use crate::editor::theme;

/// 右键菜单项文案（shell 渲染与测试共用一份）。
pub const MENU_HISTORY_LABEL: &str = "历史记录…";

/// 捕获失败提示（正文保存不受影响；app 侧写入 notice）。
pub const CAPTURE_FAILED_NOTICE: &str = "历史版本写入失败（正文保存不受影响）";
/// 恢复时结构新建成功但投影里看不到（极少见）。
pub const RESTORE_LIST_FAILED_NOTICE: &str = "恢复失败：新笔记没有出现在列表里";
/// 恢复时新笔记打不开。
pub const RESTORE_OPEN_FAILED_NOTICE: &str = "恢复失败：无法打开新笔记";
/// 捕获时标题回退（树里暂时找不到该笔记）。
pub const NOTE_FALLBACK_TITLE: &str = "笔记";
/// 恢复时历史标题为空（旧数据/异常）的回退基名。
pub const EMPTY_HISTORY_TITLE_FALLBACK: &str = "历史笔记";

/// 历史版本恢复后的新标题：普通笔记 `原标题（历史）`；画布笔记保留 `.excalidraw`
/// 后缀（否则恢复出的 JSON 会被当普通正文显示，而不是画布）。
#[must_use]
pub fn restore_title(historical_title: &str) -> String {
    let title = historical_title.trim();
    if crate::drawing::is_drawing_title(title) {
        let stem = &title[..title
            .len()
            .saturating_sub(crate::drawing::DRAWING_SUFFIX.len())];
        format!("{stem}（历史）{}", crate::drawing::DRAWING_SUFFIX)
    } else if title.is_empty() {
        format!("{EMPTY_HISTORY_TITLE_FALLBACK}（历史）")
    } else {
        format!("{title}（历史）")
    }
}

/// 恢复成功提示（动态标题）。
#[must_use]
pub fn restored_notice(title: &str) -> String {
    format!("已恢复为新笔记「{title}」")
}

/// 恢复失败提示：编辑器拒绝整篇替换（动态标题）。
#[must_use]
pub fn restore_rejected_notice(title: &str) -> String {
    format!("恢复失败：「{title}」正文写入被拒绝")
}

/// 面板标题前缀。
const PANEL_TITLE: &str = "历史记录";
/// 空态：本机还没有任何版本。
const EMPTY_TEXT: &str = "还没有本机历史版本（编辑停止约 2 秒后会自动记录）";
/// 没选中版本时的预览提示。
const PREVIEW_HINT: &str = "选择左侧版本以只读预览";
/// 「恢复为新笔记」按钮。
const RESTORE_LABEL: &str = "恢复为新笔记";
/// 「关闭」按钮。
const CLOSE_LABEL: &str = "关闭";
/// 来源展示名（未知来源原样展示）。
fn source_label(source: &str) -> &str {
    match source {
        HISTORY_SOURCE_LOCAL => "本机",
        "sync" => "同步",
        "checkpoint" => "基线",
        other => other,
    }
}

/// 覆盖层事件：宿主负责真正的新建笔记/聚焦。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoteHistoryEvent {
    /// 用户要求关闭覆盖层（Esc / 关闭按钮 / 点遮罩）。
    Closed,
    ///「恢复为新笔记」：带选中版本的标题与正文（宿主创建新笔记并写入）。
    RestoreRequested {
        /// 历史版本捕获时的标题。
        title: String,
        /// 历史版本正文（解压后的 Markdown 源文）。
        body: String,
    },
}

/// 一篇笔记的历史时间线覆盖层。
pub struct NoteHistoryPanel {
    note_id: String,
    note_title: String,
    vault: Vault,
    versions: Vec<HistoryVersionSummary>,
    selected: Option<usize>,
    preview: Option<HistoryVersion>,
    error: Option<String>,
    focus: FocusHandle,
    /// 首帧是否已经主动抢过焦点（打开后 `Esc`/方向键要立刻可用）。
    focused_once: bool,
}

impl NoteHistoryPanel {
    /// 打开面板时同步加载当前笔记的版本列表（只读、小查询；不加载正文）。
    pub fn new(note_id: String, note_title: String, vault: Vault, cx: &mut Context<Self>) -> Self {
        let (versions, error) =
            match list_note_versions(&vault, &note_id, HISTORY_MAX_VERSIONS_PER_NOTE) {
                Ok(versions) => (versions, None),
                Err(error) => {
                    eprintln!("[menghunote] 读取历史版本失败 note={note_id}：{error}");
                    (Vec::new(), Some(format!("历史记录加载失败：{error}")))
                }
            };
        Self {
            note_id,
            note_title,
            vault,
            versions,
            selected: None,
            preview: None,
            error,
            focus: cx.focus_handle(),
            focused_once: false,
        }
    }

    /// 面板焦点（接 `Esc` / 方向键）。
    pub fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.focus.focus(window, cx);
    }

    /// 版本数量（headless 断言用）。
    #[must_use]
    pub fn version_count(&self) -> usize {
        self.versions.len()
    }

    /// 当前选中行（0 = 最新；headless 断言用）。
    #[must_use]
    pub fn selected_index(&self) -> Option<usize> {
        self.selected
    }

    /// 当前预览正文（headless 断言用；`None` = 还没选中/加载失败）。
    #[must_use]
    pub fn preview_body(&self) -> Option<&str> {
        self.preview.as_ref().map(|version| version.body.as_str())
    }

    /// 加载失败提示（`None` = 正常）。
    #[must_use]
    pub fn error_text(&self) -> Option<&str> {
        self.error.as_deref()
    }

    fn select_row(&mut self, row: usize, cx: &mut Context<Self>) {
        let Some(summary) = self.versions.get(row).cloned() else {
            return;
        };
        self.selected = Some(row);
        self.error = None;
        match load_note_version_for_note(&self.vault, &self.note_id, summary.version_id) {
            Ok(Some(version)) => {
                self.preview = Some(version);
            }
            Ok(None) => {
                self.preview = None;
                self.error = Some("该历史版本已不存在".to_owned());
            }
            Err(error) => {
                eprintln!(
                    "[menghunote] 读取历史版本正文失败 note={} version={}：{error}",
                    self.note_id, summary.version_id
                );
                self.preview = None;
                self.error = Some(format!("读取历史版本失败：{error}"));
            }
        }
        cx.notify();
    }

    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.versions.is_empty() {
            return;
        }
        let current = self.selected.unwrap_or(0) as isize;
        let next = (current + delta).clamp(0, self.versions.len() as isize - 1) as usize;
        self.select_row(next, cx);
    }

    fn restore_selected(&mut self, cx: &mut Context<Self>) {
        let Some(version) = &self.preview else {
            return;
        };
        cx.emit(NoteHistoryEvent::RestoreRequested {
            title: version.title.clone(),
            body: version.body.clone(),
        });
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        match event.keystroke.key.as_str() {
            "escape" => {
                cx.emit(NoteHistoryEvent::Closed);
                cx.stop_propagation();
            }
            "up" => {
                self.move_selection(-1, cx);
                cx.stop_propagation();
            }
            "down" => {
                self.move_selection(1, cx);
                cx.stop_propagation();
            }
            "enter" => {
                self.restore_selected(cx);
                cx.stop_propagation();
            }
            _ => {}
        }
    }

    fn close_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("note-history-close")
            .debug_selector(|| "note-history-close".to_string())
            .flex_shrink_0()
            .h(px(24.0))
            .px_2()
            .flex()
            .items_center()
            .justify_center()
            .rounded_sm()
            .cursor_pointer()
            .border_1()
            .border_color(theme::sidebar_border())
            .text_size(px(11.0))
            .text_color(theme::status_bar_text())
            .hover(|style| style.bg(theme::sidebar_selected_bg()))
            .child(SharedString::from(CLOSE_LABEL))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_panel, _event, _window, cx| {
                    cx.emit(NoteHistoryEvent::Closed);
                    cx.stop_propagation();
                }),
            )
    }

    fn render_rows(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        self.versions
            .iter()
            .enumerate()
            .map(|(row, version)| {
                let selected = self.selected == Some(row);
                let time = format_captured_at(version.captured_at);
                let title = version.title.clone();
                let mut meta = source_label(&version.source).to_owned();
                if let Some(device) = &version.device_id {
                    meta.push_str(" · ");
                    meta.push_str(device);
                }
                div()
                    .id(SharedString::from(format!("note-history-row-{row}")))
                    .debug_selector(move || format!("note-history-row-{row}"))
                    .w_full()
                    .h(px(46.0))
                    .flex_shrink_0()
                    .px_2()
                    .flex()
                    .flex_col()
                    .justify_center()
                    .gap(px(1.0))
                    .rounded_sm()
                    .cursor_pointer()
                    .when(selected, |this| this.bg(theme::sidebar_selected_bg()))
                    .hover(|style| style.bg(theme::sidebar_selected_bg()))
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .flex_row()
                            .items_center()
                            .justify_between()
                            .gap_1()
                            .child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(theme::text_color())
                                    .child(SharedString::from(time)),
                            )
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .text_size(px(10.0))
                                    .text_color(theme::status_bar_text())
                                    .child(SharedString::from(meta)),
                            ),
                    )
                    .child(
                        div()
                            .w_full()
                            .overflow_hidden()
                            .text_ellipsis()
                            .text_size(px(11.0))
                            .text_color(theme::status_bar_text())
                            .child(SharedString::from(title)),
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |panel, _event, _window, cx| {
                            panel.select_row(row, cx);
                            cx.stop_propagation();
                        }),
                    )
                    .into_any_element()
            })
            .collect()
    }

    fn preview_pane(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let header = self.preview.as_ref().map(|version| {
            format!(
                "{} · {} · {}",
                format_captured_at(version.captured_at),
                version.title,
                source_label(&version.source)
            )
        });
        let body_text = match (&self.preview, &self.error) {
            (_, Some(error)) => error.clone(),
            (Some(version), None) if version.body.is_empty() => "（该版本正文为空）".to_owned(),
            (Some(version), None) => version.body.clone(),
            (None, None) => PREVIEW_HINT.to_owned(),
        };
        let can_restore = self.preview.is_some();
        div()
            .flex_1()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .gap_2()
            .p_2()
            .child(
                div()
                    .flex_shrink_0()
                    .w_full()
                    .overflow_hidden()
                    .text_ellipsis()
                    .text_size(px(11.0))
                    .text_color(theme::status_bar_text())
                    .child(SharedString::from(
                        header.unwrap_or_else(|| "只读预览".to_owned()),
                    )),
            )
            .child(
                div()
                    .id("note-history-preview")
                    .debug_selector(|| "note-history-preview".to_string())
                    .flex_1()
                    .min_h(px(0.0))
                    .w_full()
                    .p_2()
                    .overflow_y_scroll()
                    .rounded_sm()
                    .border_1()
                    .border_color(theme::sidebar_border())
                    .bg(theme::window_bg())
                    .text_size(px(12.0))
                    .text_color(theme::text_color())
                    .child(SharedString::from(body_text)),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_end()
                    .child(
                        div()
                            .id("note-history-restore")
                            .debug_selector(|| "note-history-restore".to_string())
                            .h(px(26.0))
                            .px_3()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_sm()
                            .border_1()
                            .border_color(theme::sidebar_border())
                            .text_size(px(11.0))
                            .text_color(if can_restore {
                                theme::text_color()
                            } else {
                                theme::status_bar_text()
                            })
                            .when(can_restore, |this| this.cursor_pointer())
                            .hover(|style| style.bg(theme::sidebar_selected_bg()))
                            .child(SharedString::from(RESTORE_LABEL))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |panel, _event, _window, cx| {
                                    panel.restore_selected(cx);
                                    cx.stop_propagation();
                                }),
                            ),
                    ),
            )
    }
}

impl EventEmitter<NoteHistoryEvent> for NoteHistoryPanel {}

impl Render for NoteHistoryPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 覆盖层创建时 `focus()` 早于元素挂载；首帧再抢一次，保证 `Esc` 立刻可用。
        if !self.focused_once {
            self.focused_once = true;
            self.focus.focus(window, cx);
        }
        let focused = self.focus.clone();
        let title = format!("{PANEL_TITLE} · {}", self.note_title);
        let rows = self.render_rows(cx);
        let mut list = div()
            .id("note-history-list")
            .w(px(300.0))
            .flex_shrink_0()
            .h_full()
            .flex()
            .flex_col()
            .gap(px(1.0))
            .p_1()
            .border_r_1()
            .border_color(theme::sidebar_border())
            .overflow_y_scroll()
            .children(rows);
        if self.versions.is_empty() {
            list = list.child(
                div()
                    .debug_selector(|| "note-history-empty".to_string())
                    .w_full()
                    .p_2()
                    .text_size(px(11.0))
                    .text_color(theme::status_bar_text())
                    .child(SharedString::from(
                        self.error.clone().unwrap_or_else(|| EMPTY_TEXT.to_owned()),
                    )),
            );
        }

        let panel = div()
            .id("note-history-panel")
            .debug_selector(|| "note-history-panel".to_string())
            .w(px(760.0))
            .h(px(460.0))
            .max_w(relative(0.92))
            .max_h(relative(0.88))
            .flex()
            .flex_col()
            .bg(theme::menu_bg())
            .border_1()
            .border_color(theme::sidebar_border())
            .rounded_md()
            .shadow_lg()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_panel, _event, _window, cx| cx.stop_propagation()),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .h(px(40.0))
                    .px_3()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .border_b_1()
                    .border_color(theme::sidebar_border())
                    .child(
                        div()
                            .min_w(px(0.0))
                            .overflow_hidden()
                            .text_ellipsis()
                            .text_size(px(13.0))
                            .text_color(theme::text_color())
                            .child(SharedString::from(title)),
                    )
                    .child(self.close_button(cx)),
            )
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .flex()
                    .flex_row()
                    .child(list)
                    .child(self.preview_pane(cx)),
            );

        div()
            .id("note-history")
            .debug_selector(|| "note-history".to_string())
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(theme::overlay_scrim())
            .track_focus(&focused)
            .on_key_down(cx.listener(Self::on_key_down))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_panel, _event, _window, cx| {
                    cx.emit(NoteHistoryEvent::Closed);
                    cx.stop_propagation();
                }),
            )
            .child(panel)
    }
}

/// 毫秒时间戳 → 本地时间（`YYYY-MM-DD HH:MM`）；非法时间回退毫秒数。
fn format_captured_at(ms: i64) -> String {
    let seconds = ms.div_euclid(1_000);
    let nanos = (ms.rem_euclid(1_000) * 1_000_000) as u32;
    match Local.timestamp_opt(seconds, nanos) {
        chrono::LocalResult::Single(time) => time.format("%Y-%m-%d %H:%M").to_string(),
        _ => format!("{ms} ms"),
    }
}

#[cfg(test)]
mod restore_title_tests {
    use super::restore_title;

    #[test]
    fn drawing_history_restore_keeps_the_drawing_suffix() {
        assert_eq!(
            restore_title("流程图.excalidraw"),
            "流程图（历史）.excalidraw"
        );
        assert_eq!(restore_title("普通笔记"), "普通笔记（历史）");
    }
}
