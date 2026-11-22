//! 自绘输入框的快捷键映射（**纯函数**，搜索框/向导/重命名共用）。
//!
//! - [`shortcut_action`]：`Ctrl+A` 全选；`Ctrl+C` / `Ctrl+X` **v1 不做**（显式忽略，
//!   见 [`ShortcutAction::ClipboardUnsupported`]）。搜索框与向导的既有行为不变。
//! - [`paste_shortcut`]：粘贴键（`Ctrl+V` / `Cmd+V`，允许带 `Shift`）。剪贴板读取留在
//!   各输入框里（[`crate::sidebar_rename::RenameInput`] 的标题/表格单元格共享一套单行清理），
//!   这样搜索框等不需要粘贴的输入框不会被顺带改变行为。
//!
//! 组合态（IME 正在拼写）下**一律忽略**：此时按键属于输入法（`Ctrl+A` 在微软拼音里
//! 没有语义，但也不该被我们拿去全选，否则用户正在拼的词会被选中断开）。

use gpui::Keystroke;

/// 自绘输入框支持的快捷键动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShortcutAction {
    /// `Ctrl+A`：全选。
    SelectAll,
    /// `Ctrl+C` / `Ctrl+X` / `Ctrl+V`：映射层**不自己碰剪贴板**（显式忽略）。
    ///
    /// 支持粘贴的输入框（[`crate::sidebar_rename::RenameInput`]）会在调用
    /// [`shortcut_action`] **之前**用 [`paste_shortcut`] 拦截 `V`；其余输入框
    /// （搜索框/向导）保持既有行为。
    ClipboardUnsupported,
    /// 其它组合：不处理，交回给按键处理（方向键/退格等）。
    None,
}

/// 是否为粘贴快捷键：`Ctrl+V`（Windows/Linux）/ `Cmd+V`（macOS）。
///
/// 带 `Shift` 视为同义（`Ctrl+Shift+V` 粘贴纯文本）；`Alt` 组合与组合态一律不算，
/// 交回给平台/输入法。读取剪贴板由调用方负责（本函数不碰平台 API）。
/// 是否为复制快捷键：`Ctrl+C` / `Cmd+C`（不带 Alt/Shift）。
///
/// 搜索框/向导仍显式忽略 `Ctrl+C`；只有重命名/标题输入框在
/// [`shortcut_action`] 之前调用本函数，复制选区到系统剪贴板。
#[must_use]
pub fn copy_shortcut(keystroke: &Keystroke, composing: bool) -> bool {
    clipboard_chord(keystroke, composing, "c")
}

/// 是否为剪切快捷键：`Ctrl+X` / `Cmd+X`（不带 Alt/Shift）。
#[must_use]
pub fn cut_shortcut(keystroke: &Keystroke, composing: bool) -> bool {
    clipboard_chord(keystroke, composing, "x")
}

fn clipboard_chord(keystroke: &Keystroke, composing: bool, key: &str) -> bool {
    if composing {
        return false;
    }
    let modifiers = keystroke.modifiers;
    if modifiers.alt || modifiers.shift || modifiers.function {
        return false;
    }
    (modifiers.control || modifiers.platform) && keystroke.key == key
}

#[must_use]
pub fn paste_shortcut(keystroke: &Keystroke, composing: bool) -> bool {
    if composing {
        return false;
    }
    let modifiers = keystroke.modifiers;
    if modifiers.alt || modifiers.function {
        return false;
    }
    (modifiers.control || modifiers.platform) && keystroke.key == "v"
}

/// 把按键映射成快捷键动作（**纯函数**，可单测）。
///
/// 规则：
/// - 组合态（`composing == true`）→ 一律 `None`（交给输入法）；
/// - `Ctrl` + `A`（不带 Alt/Shift/Cmd）→ [`ShortcutAction::SelectAll`]；
/// - `Ctrl` + `C/X/V` → [`ShortcutAction::ClipboardUnsupported`]（粘贴需求由
///   [`paste_shortcut`] 单独判定，见模块文档）；
/// - 其它 → [`ShortcutAction::None`]。
#[must_use]
pub fn shortcut_action(keystroke: &Keystroke, composing: bool) -> ShortcutAction {
    if composing {
        return ShortcutAction::None;
    }
    let modifiers = keystroke.modifiers;
    if !modifiers.control || modifiers.alt || modifiers.shift || modifiers.platform {
        return ShortcutAction::None;
    }
    match keystroke.key.as_str() {
        "a" => ShortcutAction::SelectAll,
        "c" | "x" | "v" => ShortcutAction::ClipboardUnsupported,
        _ => ShortcutAction::None,
    }
}

#[cfg(test)]
mod tests {
    use super::{copy_shortcut, cut_shortcut, paste_shortcut, shortcut_action, ShortcutAction};
    use gpui::Keystroke;

    fn key(text: &'static str) -> Keystroke {
        Keystroke::parse(text).expect("按键解析失败")
    }

    #[test]
    fn ctrl_a_selects_all() {
        assert_eq!(
            shortcut_action(&key("ctrl-a"), false),
            ShortcutAction::SelectAll
        );
    }

    #[test]
    fn composing_ignores_every_shortcut() {
        // 组合态（IME 正在拼写）：连 Ctrl+A 也不许抢，交回输入法。
        assert_eq!(shortcut_action(&key("ctrl-a"), true), ShortcutAction::None);
        assert_eq!(shortcut_action(&key("ctrl-v"), true), ShortcutAction::None);
        assert!(!paste_shortcut(&key("ctrl-v"), true));
    }

    #[test]
    fn clipboard_keys_are_explicitly_unsupported() {
        assert_eq!(
            shortcut_action(&key("ctrl-c"), false),
            ShortcutAction::ClipboardUnsupported
        );
        assert_eq!(
            shortcut_action(&key("ctrl-x"), false),
            ShortcutAction::ClipboardUnsupported
        );
        // 映射层仍把 V 归为"剪贴板键"；支持粘贴的输入框在它之前用
        // `paste_shortcut` 拦截，不会走到这个分支。
        assert_eq!(
            shortcut_action(&key("ctrl-v"), false),
            ShortcutAction::ClipboardUnsupported
        );
    }

    #[test]
    fn paste_is_ctrl_or_cmd_v_with_optional_shift() {
        assert!(paste_shortcut(&key("ctrl-v"), false), "Ctrl+V 是粘贴");
        assert!(paste_shortcut(&key("cmd-v"), false), "Cmd+V 是粘贴");
        assert!(
            paste_shortcut(&key("ctrl-shift-v"), false),
            "Ctrl+Shift+V 同义（粘贴纯文本）"
        );
        assert!(paste_shortcut(&key("cmd-shift-v"), false));
        assert!(!paste_shortcut(&key("v"), false), "裸 V 是普通输入");
        assert!(!paste_shortcut(&key("ctrl-alt-v"), false));
        assert!(!paste_shortcut(&key("ctrl-c"), false));
        assert!(!paste_shortcut(&key("ctrl-x"), false));
    }

    #[test]
    fn copy_and_cut_are_ctrl_or_cmd_without_other_modifiers() {
        assert!(copy_shortcut(&key("ctrl-c"), false));
        assert!(copy_shortcut(&key("cmd-c"), false));
        assert!(cut_shortcut(&key("ctrl-x"), false));
        assert!(cut_shortcut(&key("cmd-x"), false));
        assert!(!copy_shortcut(&key("ctrl-shift-c"), false));
        assert!(!copy_shortcut(&key("ctrl-alt-c"), false));
        assert!(!copy_shortcut(&key("c"), false));
        assert!(!cut_shortcut(&key("ctrl-c"), false));
        assert!(!copy_shortcut(&key("ctrl-c"), true), "组合态交给 IME");
    }

    #[test]
    fn other_modifier_combinations_are_not_shortcuts() {
        assert_eq!(shortcut_action(&key("a"), false), ShortcutAction::None);
        assert_eq!(
            shortcut_action(&key("ctrl-shift-a"), false),
            ShortcutAction::None
        );
        assert_eq!(shortcut_action(&key("alt-a"), false), ShortcutAction::None);
        assert_eq!(shortcut_action(&key("ctrl-b"), false), ShortcutAction::None);
    }
}
