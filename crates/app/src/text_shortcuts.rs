//! 自绘输入框的快捷键映射（**纯函数**，两个输入框共用）。
//!
//! 目前只有 `Ctrl+A`（全选）。`Ctrl+C/X/V` **v1 不做**，原因写在
//! [`ShortcutAction::CopyUnsupported`] 上：剪贴板要走平台 API（gpui 有 `ClipboardItem`），
//! 而现在的输入框连"选区复制"的产品需求都还没定；与其做半截，不如显式忽略并记录。
//!
//! 组合态（IME 正在拼写）下**一律忽略**：此时按键属于输入法（`Ctrl+A` 在微软拼音里
//! 没有语义，但也不该被我们拿去全选，否则用户正在拼的词会被选中断开）。

use gpui::Keystroke;

/// 自绘输入框支持的快捷键动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShortcutAction {
    /// `Ctrl+A`：全选。
    SelectAll,
    /// `Ctrl+C` / `Ctrl+X` / `Ctrl+V`：**v1 未实现**（剪贴板产品需求未定，显式忽略）。
    ClipboardUnsupported,
    /// 其它组合：不处理，交回给按键处理（方向键/退格等）。
    None,
}

/// 把按键映射成快捷键动作（**纯函数**，可单测）。
///
/// 规则：
/// - 组合态（`composing == true`）→ 一律 `None`（交给输入法）；
/// - `Ctrl` + `A`（不带 Alt/Shift/Cmd）→ [`ShortcutAction::SelectAll`]；
/// - `Ctrl` + `C/X/V` → [`ShortcutAction::ClipboardUnsupported`]（占位：明确不做）；
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
    use super::{shortcut_action, ShortcutAction};
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
        assert_eq!(
            shortcut_action(&key("ctrl-v"), false),
            ShortcutAction::ClipboardUnsupported
        );
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
