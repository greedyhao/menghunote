//! 设置页的「外观」选择（ADR 0021 P2）。
//!
//! 浅色 / 深色 / 跟随系统三选一；点击**立即生效**（更新全局主题 + 所有窗口重绘），
//! 不重建编辑器、不丢选区/滚动/撤销栈、不产生 `EditRequest`。偏好只写本机
//! `theme.json`（[`crate::theme`]），不同步、不写正文。
//!
//! 组件本身只读语义 token；固定 selector 供 headless 测试点击：
//!
//! | selector | 含义 |
//! | --- | --- |
//! | `settings-appearance` | 外观区根 |
//! | `settings-theme-light` / `settings-theme-dark` / `settings-theme-system` | 三个模式按钮 |
//! | `settings-theme-current` | 当前模式 + 解析后的主题 id |

use gpui::{
    div, px, InteractiveElement as _, IntoElement, MouseButton, ParentElement as _, SharedString,
    Styled as _,
};

use menghunote_i18n::{msg, t, t_with};

use crate::editor::theme;
use crate::theme::{mode, set_mode, theme_id, ThemeId, ThemeMode};

/// 外观设置区（AppShell 与 AppRoot 的设置页共用）。
#[must_use]
pub fn appearance_section() -> impl IntoElement {
    let mode = mode();
    let resolved = theme_id();
    div()
        .id("settings-appearance")
        .debug_selector(|| "settings-appearance".to_string())
        .w_full()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_size(px(12.0))
                .text_color(theme::text_color())
                .child(SharedString::from(t(msg::settings::APPEARANCE_TITLE))),
        )
        .child(
            div()
                .text_size(px(11.0))
                .text_color(theme::status_bar_text())
                .child(SharedString::from(t(msg::settings::APPEARANCE_HINT))),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .gap_2()
                .child(mode_button(
                    "settings-theme-light",
                    mode_label(ThemeMode::Light),
                    ThemeMode::Light,
                    mode,
                ))
                .child(mode_button(
                    "settings-theme-dark",
                    mode_label(ThemeMode::Dark),
                    ThemeMode::Dark,
                    mode,
                ))
                .child(mode_button(
                    "settings-theme-system",
                    mode_label(ThemeMode::System),
                    ThemeMode::System,
                    mode,
                )),
        )
        .child(
            div()
                .debug_selector(|| "settings-theme-current".to_string())
                .text_size(px(11.0))
                .text_color(theme::status_bar_text())
                .child(SharedString::from(current_line(mode, resolved))),
        )
}

/// 单个模式按钮：选中态用强调色，未选中用表面色 + 边框。
fn mode_button(
    selector: &'static str,
    label: String,
    button_mode: ThemeMode,
    current: ThemeMode,
) -> impl IntoElement {
    let active = button_mode == current;
    let (background, text, border) = if active {
        (
            theme::search_toggle_active(),
            theme::search_toggle_active_text(),
            theme::search_toggle_active(),
        )
    } else {
        (theme::menu_bg(), theme::text_color(), theme::input_border())
    };
    div()
        .id(selector)
        .debug_selector(move || selector.to_string())
        .px_3()
        .py_1()
        .rounded_sm()
        .border_1()
        .border_color(border)
        .bg(background)
        .text_size(px(12.0))
        .text_color(text)
        .hover(move |style| {
            // 选中态保持强调色（白字压在浅灰 hover 上会看不清）。
            if active {
                style.bg(background)
            } else {
                style.bg(theme::sidebar_selected_bg())
            }
        })
        .child(SharedString::from(label))
        .on_mouse_down(MouseButton::Left, move |_event, window, cx| {
            // 立即生效：只改全局主题 + 重绘，不触碰文档/选区/撤销栈。
            set_mode(button_mode, window.appearance(), cx);
        })
}

/// 当前模式行：`当前：浅色（解析为浅色）` / `Current: Light (resolved: Light)`。
#[must_use]
pub fn current_line(mode: ThemeMode, resolved: ThemeId) -> String {
    let mode_text = mode_label(mode);
    let theme_text = resolved_label(resolved);
    t_with(
        msg::settings::APPEARANCE_CURRENT,
        &[("mode", mode_text.as_str()), ("theme", theme_text.as_str())],
    )
}

/// 模式的展示名（按当前语言翻译；持久化用 [`ThemeMode::as_str`]）。
#[must_use]
pub fn mode_label(mode: ThemeMode) -> String {
    match mode {
        ThemeMode::Light => t(msg::settings::APPEARANCE_LIGHT),
        ThemeMode::Dark => t(msg::settings::APPEARANCE_DARK),
        ThemeMode::System => t(msg::settings::APPEARANCE_SYSTEM),
    }
}

/// 解析后主题 id 的展示名（按当前语言翻译）。
#[must_use]
pub fn resolved_label(id: ThemeId) -> String {
    match id.as_str() {
        "dark" => t(msg::settings::APPEARANCE_DARK),
        _ => t(msg::settings::APPEARANCE_LIGHT),
    }
}
