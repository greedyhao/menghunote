//! `EntityInputHandler`：把平台 IME / 文本输入接到 `EditorView`（ADR 0002 §7，切片 6b）。
//!
//! ## 两份文本，绝不能混
//!
//! | 文本 | 内容 | 谁持有 | 去向 |
//! | --- | --- | --- | --- |
//! | **source** | Markdown 源码（CRDT 真源） | `EditorController` 的 Rope 镜像 | 只有 commit 才写 |
//! | **虚拟输入文档** | `source` 在选区处替换成 preedit | `InputDocument`（派生） | 平台查询用，永不落库 |
//!
//! 平台给的所有 UTF-16 范围都按**虚拟输入文档**解释；本模块负责把它们换算回
//! source 坐标（`virtual_utf16_to_source_byte` 等），或直接把 preedit 当作 overlay
//! 处理。**preedit 不产生 `EditRequest`**，也不进 CRDT / journal。
//!
//! ## 平台调用序列（`gpui-windows` 实测，见 `events.rs` 的 `handle_ime_composition`）
//!
//! ```text
//! 组合中：replace_and_mark_text_in_range(None, "nihao", Some(caret..caret))
//! 修改：  replace_and_mark_text_in_range(None, "ni'hao", ...)
//! 取消：  replace_and_mark_text_in_range(None, "", None)      // Esc 或 IME 自行取消
//! 上屏：  replace_text_in_range(None, "你好")                  // GCS_RESULTSTR
//! 打字：  replace_text_in_range(None, "a")                     // 非 IME 的普通输入
//! ```
//!
//! 注意 Windows 后端**一律传 `range = None`**：范围由我们自己按当前选区 / preedit
//! 推断，不要依赖平台给的 range。

use std::ops::Range;

use gpui::{px, Context, EntityInputHandler, Pixels, Point, UTF16Selection, Window};
use menghunote_editor::InputUtf16;

use super::view::EditorView;

impl EntityInputHandler for EditorView {
    /// 虚拟输入文档里的任意 UTF-16 片段。
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let document = self.input_document().ok()?;
        let start = document
            .utf16_to_byte_offset(InputUtf16::new(range_utf16.start))
            .ok()?;
        let end = document
            .utf16_to_byte_offset(InputUtf16::new(range_utf16.end))
            .ok()?;
        if start > end {
            return None;
        }
        let text = document.text().get(start..end)?;
        // 平台给的范围若落在代理对中间，我们已经返回 None；能走到这里就是精确命中。
        *adjusted_range = Some(range_utf16);
        Some(text.to_owned())
    }

    /// 选区（虚拟输入文档的 UTF-16 坐标）。
    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let selection = self.selection();
        let range = self.selection_utf16_in_input_document()?;
        Some(UTF16Selection {
            range,
            reversed: selection.anchor() > selection.head(),
        })
    }

    /// 正在组合的范围（虚拟输入文档的 UTF-16 坐标）；没有组合返回 `None`。
    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        let document = self.input_document().ok()?;
        if document.preedit().is_empty() {
            return None;
        }
        let (start, end) = document.preedit_utf16_range().ok()?;
        Some(start.get()..end.get())
    }

    /// 平台要求取消组合（Esc / 切换焦点）：丢弃 overlay，**不发请求**。
    fn unmark_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.cancel_composition(cx);
        window.invalidate_character_coordinates();
        cx.notify();
    }

    /// 提交 / 普通输入：**一次调用只产生一个 `EditRequest`**。
    ///
    /// - 有组合：先把组合文本作为一次替换提交（等价于 IME 上屏）；
    /// - 没有组合：在光标处插入 / 替换选区（普通打字、粘贴走同一条路径）；
    /// - **Held（外部提交闸门持有）**：整段忽略。这是 IME 上屏与粘贴（含 Windows 的
    ///   `Ctrl+V` 走平台文本输入）的唯一入口，冻结在这里就能保证一次都不落到驱动。
    fn replace_text_in_range(
        &mut self,
        _range_utf16: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.is_sync_held() {
            eprintln!(
                "[menghunote] Held 期间收到平台文本输入（{} 字）：已忽略",
                text.chars().count()
            );
            return;
        }
        if self.is_composing() {
            self.commit_composition(text, cx);
        } else {
            self.insert_text(text, cx);
        }
        cx.notify();
    }

    /// 组合更新：**只改 overlay**，绝不产生 `EditRequest`。
    ///
    /// `new_text` 为空表示取消（Windows 上 Esc 与"组合自然结束"都会走这条）。
    fn replace_and_mark_text_in_range(
        &mut self,
        _range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if new_text.is_empty() {
            self.cancel_composition(cx);
        } else {
            self.update_preedit(new_text, new_selected_range, cx);
        }
        // 候选窗要跟着光标走：GPUI 会在下一帧取 `selected_bounds` 交给 IME。
        window.invalidate_character_coordinates();
        cx.notify();
    }

    /// 候选窗 / 输入法 UI 用的屏幕矩形。
    ///
    /// 组合中：组合串在布局里没有占位（我们把它当 overlay 画在光标处），因此
    /// 返回值 = 光标位置 + preedit 前缀宽度；组合外：就是该位置的光标矩形。
    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        _element_bounds: gpui::Bounds<Pixels>,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<gpui::Bounds<Pixels>> {
        let position = self.point_for_input_utf16(range_utf16.start, window)?;
        let line_height = px(super::theme::LINE_HEIGHT);
        Some(gpui::Bounds::new(
            position,
            gpui::size(px(0.0), line_height),
        ))
    }

    /// 屏幕上一点 → 虚拟输入文档的 UTF-16 序号（触摸 / IME 定位用）。
    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let byte = self.source_byte_at_point(point)?;
        self.source_byte_to_input_utf16(byte)
    }

    /// 平台设置选区（如 ATM/触摸选择）：落到 source 的 grapheme 边界上。
    fn set_selected_text_range(
        &mut self,
        range_utf16: Range<usize>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(byte) = self.virtual_utf16_to_source_byte(range_utf16.start) else {
            return;
        };
        let byte = self.snap_source_byte_to_grapheme(byte);
        self.place_caret_at_byte(byte, cx);
    }

    /// 虚拟输入文档的 UTF-16 长度（平台用它判断文档边界）。
    fn text_length_utf16(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        Some(self.input_document().ok()?.utf16_len())
    }

    /// 编辑器接受文本输入（`gpui-windows` 用它决定是否关联 IME 上下文）。
    fn accepts_text_input(&self, _window: &mut Window, _cx: &mut Context<Self>) -> bool {
        true
    }
}
