//! 回收站覆盖层（P0：查看 + 恢复已删除笔记）。
//!
//! ## 语义边界
//!
//! - 删除是 storage 的 **grow-only 删除事件**（`notes.deleted = 1`），恢复只是
//!   `StructureWriter::restore_note(node_id, now)` 确认**当前已观察到的**删除事件；
//! - 本覆盖层**不移动节点**：恢复只 ack，原父级（`NoteView.parent_id`）原样保留，
//!   所以文件夹里的笔记恢复后仍在原文件夹下；
//! - **不提供「彻底删除」**：数据是 append-only + 多设备，永久删除语义未定；
//!   这里既不用 `LoroTree::delete`，也不删 SQL 行（ADR 0001 §3.3）。
//!
//! ## 数据来源
//!
//! 打开面板时只读一次本地投影：[`menghunote_storage::trashed_notes`]（已删除笔记，
//! 按收藏/置顶/更新时间倒序）+ `tree` 投影（`node_id` 与父级路径；`list_tree` 会过滤
//! 已删除节点，不能用来恢复，也不保证能解析已删除父级）。
//! 恢复本身由宿主 `AppShell` 走 `crate::shell::AppShell` 的 `write_structure` 公共路径
//! （同步冻结/失败提示与其它结构写入完全一致），本模块只发事件。
//!
//! ## selector（headless 测试点击/断言用）
//!
//! | selector | 含义 |
//! | --- | --- |
//! | `trash-overlay` | 覆盖层根（遮罩） |
//! | `trash-panel` | 面板本体 |
//! | `trash-close` | 「关闭」按钮 |
//! | `trash-empty` | 空态「回收站是空的」 |
//! | `trash-error` | inline 错误行（恢复失败时保留列表） |
//! | `trash-row-<N>` | 第 N 行（0 = `trashed_notes` 的第一条） |
//! | `trash-restore-<N>` | 第 N 行的「恢复」按钮 |
//!
//! ## 已知限制
//!
//! - 目录删除不在本切片范围（当前删除入口只对笔记开放）；
//! - 父级在 `tree` 投影里缺失时路径回退显示「根目录」（投影一致时不会发生；
//!   含已删除的父级仍能解析出路径）；
//! - 像素/DPI/真实窗口观感仍需 Windows 真机验收（headless 只证明状态与命中）。

use std::collections::HashMap;

use chrono::{Local, TimeZone};
use gpui::prelude::*;
use gpui::{
    div, px, relative, AnyElement, Context, EventEmitter, FocusHandle, IntoElement, KeyDownEvent,
    MouseButton, Render, SharedString, Window,
};
use menghunote_storage::{trashed_notes, NoteView, Value, Vault};

use crate::editor::theme;

/// 面板标题（ribbon 悬停提示复用同一文案）。
pub const PANEL_TITLE: &str = "回收站";
/// 空态文案。
pub const EMPTY_TEXT: &str = "回收站是空的";
/// 根级笔记的父级展示文案。
pub const ROOT_LABEL: &str = "根目录";
/// 行内「恢复」按钮文案。
pub const RESTORE_LABEL: &str = "恢复";
/// 「关闭」按钮文案。
pub const CLOSE_LABEL: &str = "关闭";
/// 恢复成功提示（宿主写到侧栏 notice）。
pub const RESTORED_NOTICE: &str = "已恢复";
/// 恢复失败兜底提示（`write_structure` 已经给出更具体文案时优先用它）。
pub const RESTORE_FAILED_NOTICE: &str = "恢复失败（详见 stderr）";
/// 同步在途时恢复被冻结的提示（与 `write_structure` 同一条门禁，文案针对恢复）。
pub const RESTORE_SYNC_FROZEN_NOTICE: &str = "同步进行中，稍后再恢复";
/// 结构提交成功但投影里找不到该节点（极少见；保留列表并显示错误）。
pub const RESTORE_LIST_FAILED_NOTICE: &str = "恢复失败：笔记没有出现在侧栏";
/// 读取删除列表失败时的前置提示（错误详情跟在后面）。
pub const LOAD_FAILED_NOTICE: &str = "回收站加载失败";

/// 回收站里的一行（打开面板时由投影数据 + 树路径快照组成）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrashRow {
    /// 结构树节点 ID（恢复时作为 `restore_note` 的参数）。
    pub node_id: String,
    /// 稳定笔记 ID（只读诊断/测试用）。
    pub note_id: String,
    /// 标题。
    pub title: String,
    /// 最后更新时间（毫秒）。
    pub updated_at: i64,
    /// 父级路径（`文件夹/子文件夹`；根级为 [`ROOT_LABEL`]）。
    pub parent_label: String,
}

/// 覆盖层事件：宿主负责真正恢复/关闭。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrashEvent {
    /// 用户要求关闭覆盖层（Esc / 关闭按钮 / 点遮罩）。
    Closed,
    /// 用户点了某一行的「恢复」。
    RestoreRequested {
        /// 结构树节点 ID。
        node_id: String,
        /// 行的标题（日志/提示用）。
        title: String,
    },
}

/// 回收站覆盖层。
pub struct TrashPanel {
    rows: Vec<TrashRow>,
    /// inline 错误（恢复失败时保留列表；`None` = 正常）。
    ///
    /// 由宿主通过 [`TrashPanel::set_error`] 写入——恢复要经过 `write_structure`
    /// （同步冻结/提交失败都在宿主侧），面板不自己写库。
    error: Option<String>,
    focus: FocusHandle,
    /// 首帧是否已经主动抢过焦点（打开后 `Esc` 要立刻可用）。
    focused_once: bool,
}

impl TrashPanel {
    /// 打开面板时同步加载删除列表（只读、小查询；不加载正文）。
    pub fn new(vault: Vault, cx: &mut Context<Self>) -> Self {
        let mut error = None;
        let rows = match load_rows(&vault) {
            Ok(rows) => rows,
            Err(message) => {
                eprintln!("[menghunote] 读取回收站失败：{message}");
                error = Some(format!("{LOAD_FAILED_NOTICE}：{message}"));
                Vec::new()
            }
        };
        Self {
            rows,
            error,
            focus: cx.focus_handle(),
            focused_once: false,
        }
    }

    /// 面板焦点（接 `Esc`）。
    pub fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.focus.focus(window, cx);
    }

    /// 当前行数（headless 断言用）。
    #[must_use]
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// 第 `index` 行（`None` = 越界；headless 断言用）。
    #[must_use]
    pub fn row(&self, index: usize) -> Option<&TrashRow> {
        self.rows.get(index)
    }

    /// 第 `index` 行的标题（headless 断言用）。
    #[must_use]
    pub fn row_title(&self, index: usize) -> Option<&str> {
        self.rows.get(index).map(|row| row.title.as_str())
    }

    /// 第 `index` 行的父级路径（headless 断言用）。
    #[must_use]
    pub fn row_parent_label(&self, index: usize) -> Option<&str> {
        self.rows.get(index).map(|row| row.parent_label.as_str())
    }

    /// 当前 inline 错误（`None` = 正常；headless 断言用）。
    #[must_use]
    pub fn error_text(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// 写入 inline 错误：**不清空列表**（恢复失败后用户可以再点一次）。
    pub fn set_error(&mut self, message: impl Into<String>, cx: &mut Context<Self>) {
        self.error = Some(message.into());
        cx.notify();
    }

    /// 清除 inline 错误。
    pub fn clear_error(&mut self, cx: &mut Context<Self>) {
        if self.error.take().is_some() {
            cx.notify();
        }
    }

    fn request_restore(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(row) = self.rows.get(index).cloned() else {
            return;
        };
        cx.emit(TrashEvent::RestoreRequested {
            node_id: row.node_id,
            title: row.title,
        });
        cx.stop_propagation();
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if event.keystroke.key.as_str() == "escape" {
            cx.emit(TrashEvent::Closed);
            cx.stop_propagation();
        }
    }

    fn close_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("trash-close")
            .debug_selector(|| "trash-close".to_string())
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
                    cx.emit(TrashEvent::Closed);
                    cx.stop_propagation();
                }),
            )
    }

    fn render_rows(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        self.rows
            .iter()
            .enumerate()
            .map(|(index, row)| {
                let meta = format!(
                    "{} · {}",
                    format_updated_at(row.updated_at),
                    row.parent_label
                );
                div()
                    .id(SharedString::from(format!("trash-row-{index}")))
                    .debug_selector(move || format!("trash-row-{index}"))
                    .w_full()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .hover(|style| style.bg(theme::sidebar_selected_bg()))
                    // 点击行**不**打开笔记：被删除的笔记本来就不该被打开；恢复只走按钮。
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .flex()
                            .flex_col()
                            .gap(px(1.0))
                            .child(
                                div()
                                    .min_w(px(0.0))
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .text_size(px(13.0))
                                    .text_color(theme::text_color())
                                    .child(SharedString::from(row.title.clone())),
                            )
                            .child(
                                div()
                                    .min_w(px(0.0))
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .text_size(px(11.0))
                                    .text_color(theme::status_bar_text())
                                    .child(SharedString::from(meta)),
                            ),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("trash-restore-{index}")))
                            .debug_selector(move || format!("trash-restore-{index}"))
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
                            .text_color(theme::text_color())
                            .hover(|style| style.bg(theme::sidebar_selected_bg()))
                            .child(SharedString::from(RESTORE_LABEL))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |panel, _event, _window, cx| {
                                    panel.request_restore(index, cx);
                                }),
                            ),
                    )
                    .into_any_element()
            })
            .collect()
    }
}

impl EventEmitter<TrashEvent> for TrashPanel {}

impl Render for TrashPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 覆盖层创建时 `focus()` 早于元素挂载；首帧再抢一次，保证 `Esc` 立刻可用。
        if !self.focused_once {
            self.focused_once = true;
            self.focus.focus(window, cx);
        }
        let focused = self.focus.clone();
        let rows = self.render_rows(cx);
        let empty = self.rows.is_empty() && self.error.is_none();
        let mut list = div()
            .id("trash-list")
            .debug_selector(|| "trash-list".to_string())
            .w_full()
            .flex_1()
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .gap(px(1.0))
            .p_1()
            .overflow_y_scroll()
            .children(rows);
        if empty {
            list = list.child(
                div()
                    .debug_selector(|| "trash-empty".to_string())
                    .w_full()
                    .p_2()
                    .text_size(px(12.0))
                    .text_color(theme::status_bar_text())
                    .child(SharedString::from(EMPTY_TEXT)),
            );
        }

        let mut card = div()
            .id("trash-panel")
            .debug_selector(|| "trash-panel".to_string())
            .w(px(560.0))
            .max_w(relative(0.92))
            .max_h(relative(0.82))
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
                            .child(SharedString::from(PANEL_TITLE)),
                    )
                    .child(self.close_button(cx)),
            );
        if let Some(error) = self.error.clone() {
            card = card.child(
                div()
                    .debug_selector(|| "trash-error".to_string())
                    .flex_shrink_0()
                    .w_full()
                    .px_3()
                    .py_1()
                    .border_b_1()
                    .border_color(theme::sidebar_border())
                    .text_size(px(11.0))
                    .text_color(theme::danger_text())
                    .child(SharedString::from(error)),
            );
        }

        div()
            .id("trash-overlay")
            .debug_selector(|| "trash-overlay".to_string())
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
                    cx.emit(TrashEvent::Closed);
                    cx.stop_propagation();
                }),
            )
            .child(card.child(list))
    }
}

/// `tree` 投影里的一行（只读；回收站只需要 node_id / parent_id / title）。
struct TreePathRow {
    node_id: String,
    parent_id: Option<String>,
    title: String,
}

/// 删除列表 + 父级路径快照（只读投影）。
///
/// 为什么直接读 `tree` 表：`trashed_notes` 的 `NoteView` 只有 `note_id`，没有恢复所需的
/// 结构树 `node_id`；而 `list_tree` 会过滤掉已删除节点（父级也可能已删除）。`tree` 是
/// `projector` 的派生投影（ADR 0001），只读查询与 `list_tree` 是同一份事实来源。
fn load_rows(vault: &Vault) -> Result<Vec<TrashRow>, String> {
    let notes = trashed_notes(vault).map_err(|error| error.to_string())?;
    // 读不到 `tree` 时整次加载失败：没有 `node_id` 的行无法恢复，宁可明确报错，
    // 也不要把"看起来能点、点了一定失败"的列表交给用户。
    let (node_ids, tree) = load_tree_index(vault)?;
    Ok(notes
        .into_iter()
        .map(|note| row_from_note(note, &node_ids, &tree))
        .collect())
}

/// 一次扫描 `tree` 投影：`note_id → node_id` + 全量路径行（含已删除的父级）。
fn load_tree_index(vault: &Vault) -> Result<(HashMap<String, String>, Vec<TreePathRow>), String> {
    let rows = vault
        .query("SELECT node_id, parent_id, title, note_id FROM tree", &[])
        .map_err(|error| error.to_string())?;
    let mut node_ids = HashMap::with_capacity(rows.len());
    let mut tree = Vec::with_capacity(rows.len());
    for row in &rows {
        let node_id = match row.first() {
            Some(Value::Text(value)) => value.clone(),
            other => return Err(format!("tree.node_id 读回异常：{other:?}")),
        };
        let parent_id = match row.get(1) {
            Some(Value::Text(value)) => Some(value.clone()),
            Some(Value::Null) | None => None,
            other => return Err(format!("tree.parent_id 读回异常：{other:?}")),
        };
        let title = match row.get(2) {
            Some(Value::Text(value)) => value.clone(),
            other => return Err(format!("tree.title 读回异常：{other:?}")),
        };
        if let Some(Value::Text(note_id)) = row.get(3) {
            node_ids.insert(note_id.clone(), node_id.clone());
        }
        tree.push(TreePathRow {
            node_id,
            parent_id,
            title,
        });
    }
    Ok((node_ids, tree))
}

fn row_from_note(
    note: NoteView,
    node_ids: &HashMap<String, String>,
    tree: &[TreePathRow],
) -> TrashRow {
    TrashRow {
        parent_label: parent_label(note.parent_id.as_deref(), tree),
        // 投影一致时必然存在；缺失时保留列表（恢复会走失败提示，不静默丢行）。
        node_id: node_ids.get(&note.note_id).cloned().unwrap_or_default(),
        note_id: note.note_id,
        title: note.title,
        updated_at: note.updated_at,
    }
}

/// 父节点 ID → 展示路径（`文件夹/子文件夹`；根级/缺失父级 → [`ROOT_LABEL`]）。
fn parent_label(parent_id: Option<&str>, tree: &[TreePathRow]) -> String {
    let Some(parent_id) = parent_id else {
        return ROOT_LABEL.to_owned();
    };
    let Some(index) = tree.iter().position(|entry| entry.node_id == parent_id) else {
        return ROOT_LABEL.to_owned();
    };
    let mut titles = Vec::new();
    let mut cursor = Some(index);
    // 上界防御脏数据（与 `tree_path_for_entry` 的 64 层一致）。
    for _ in 0..64 {
        let Some(entry) = cursor.and_then(|current| tree.get(current)) else {
            break;
        };
        titles.push(entry.title.clone());
        cursor = entry
            .parent_id
            .as_deref()
            .and_then(|parent| tree.iter().position(|entry| entry.node_id == parent));
    }
    if titles.is_empty() {
        return ROOT_LABEL.to_owned();
    }
    titles.reverse();
    titles.join("/")
}

/// 毫秒时间戳 → 本地时间（`YYYY-MM-DD HH:MM`）；非法时间回退毫秒数。
fn format_updated_at(ms: i64) -> String {
    let seconds = ms.div_euclid(1_000);
    let nanos = (ms.rem_euclid(1_000) * 1_000_000) as u32;
    match Local.timestamp_opt(seconds, nanos) {
        chrono::LocalResult::Single(time) => time.format("%Y-%m-%d %H:%M").to_string(),
        _ => format!("{ms} ms"),
    }
}

#[cfg(test)]
mod tests {
    use super::{format_updated_at, parent_label, TreePathRow, ROOT_LABEL};

    fn entry(node_id: &str, title: &str, parent_id: Option<&str>) -> TreePathRow {
        TreePathRow {
            node_id: node_id.to_owned(),
            parent_id: parent_id.map(str::to_owned),
            title: title.to_owned(),
        }
    }

    #[test]
    fn root_parent_uses_root_label() {
        assert_eq!(parent_label(None, &[]), ROOT_LABEL);
    }

    #[test]
    fn missing_parent_falls_back_to_root_label() {
        let tree = [entry("a", "A", None)];
        assert_eq!(parent_label(Some("missing"), &tree), ROOT_LABEL);
    }

    #[test]
    fn nested_parent_shows_full_path() {
        let tree = [
            entry("a", "A", None),
            entry("b", "B", Some("a")),
            entry("c", "C", Some("b")),
        ];
        assert_eq!(parent_label(Some("a"), &tree), "A");
        assert_eq!(parent_label(Some("c"), &tree), "A/B/C");
    }

    #[test]
    fn invalid_timestamp_does_not_panic() {
        assert!(!format_updated_at(i64::MAX).is_empty());
        assert!(!format_updated_at(0).is_empty());
    }
}
