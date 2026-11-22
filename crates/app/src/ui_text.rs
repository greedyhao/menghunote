//! 首批迁移的固定 UI 文案（ADR 0020 P1）。
//!
//! 这一层是**渲染与测试共用的唯一取文案入口**：侧栏/状态栏/编辑器右键菜单/
//! 设置页的固定文案都从这里取，渲染代码不再出现中文字面量。这样：
//!
//! - 切换语言只需改全局 [`menghunote_i18n::set_mode`] + 重绘；
//! - headless 测试断言的就是渲染实际使用的字符串（不是另抄一份期望值）；
//! - 动态数据（笔记标题、路径、错误原文、同步状态行）**不经过这里**，原样展示。
//!
//! 只覆盖 P1 清单：设置页固定文案、侧栏固定文案、编辑器右键菜单、状态栏固定
//! 提示。未迁移清单一律写进 ADR 0020 §9，不在这里偷偷扩张。

use menghunote_i18n::{msg, t, t_count, t_with};

// ---------------------------------------------------------------------------
// 侧栏
// ---------------------------------------------------------------------------

/// 侧栏标题「笔记」。
#[must_use]
pub fn sidebar_notes() -> String {
    t(msg::sidebar::NOTES)
}

/// 侧栏「＋ 笔记」。
#[must_use]
pub fn sidebar_new_note() -> String {
    t(msg::sidebar::NEW_NOTE)
}

/// 侧栏「＋ 文件夹」。
#[must_use]
pub fn sidebar_new_folder() -> String {
    t(msg::sidebar::NEW_FOLDER)
}

/// 侧栏「导出」。
#[must_use]
pub fn sidebar_export() -> String {
    t(msg::sidebar::EXPORT)
}

/// 侧栏「导出中…」。
#[must_use]
pub fn sidebar_exporting() -> String {
    t(msg::sidebar::EXPORTING)
}

/// 侧栏「图谱」入口。
#[must_use]
pub fn sidebar_graph() -> String {
    t(msg::sidebar::GRAPH)
}

/// 侧栏底部「⚙ 设置」。
#[must_use]
pub fn sidebar_settings() -> String {
    t(msg::sidebar::SETTINGS)
}

/// 反向链接面板标题（含数量；英文 `1 backlink` / `N backlinks`）。
#[must_use]
pub fn sidebar_backlinks(count: usize) -> String {
    t_count(
        msg::sidebar::BACKLINKS_ONE,
        msg::sidebar::BACKLINKS_OTHER,
        count as u64,
    )
}

/// 反向链接面板空态「没有反向链接」。
#[must_use]
pub fn sidebar_backlinks_empty() -> String {
    t(msg::sidebar::BACKLINKS_EMPTY)
}

// ---------------------------------------------------------------------------
// 状态栏
// ---------------------------------------------------------------------------

/// 状态栏「反向链接 N」（英文单复数同 [`sidebar_backlinks`]）。
#[must_use]
pub fn status_backlinks(count: usize) -> String {
    t_count(
        msg::status::BACKLINKS_ONE,
        msg::status::BACKLINKS_OTHER,
        count as u64,
    )
}

/// 状态栏同步前缀：「同步：{status}」。
///
/// `status` 是同步 worker 产出的**动态状态行**（本切片不翻译，原样嵌入）。
#[must_use]
pub fn status_sync(status: &str) -> String {
    t_with(msg::status::SYNC, &[("status", status)])
}

/// 状态栏「同步：需处理」。
#[must_use]
pub fn status_sync_attention() -> String {
    t(msg::status::SYNC_ATTENTION)
}

// ---------------------------------------------------------------------------
// 编辑器右键菜单
// ---------------------------------------------------------------------------

/// 右键菜单分组标题「插入」。
#[must_use]
pub fn editor_menu_insert() -> String {
    t(msg::editor_menu::INSERT)
}

/// 右键菜单「表格」。
#[must_use]
pub fn editor_menu_table() -> String {
    t(msg::editor_menu::TABLE)
}

/// 链接菜单「打开链接」。
#[must_use]
pub fn editor_menu_open_link() -> String {
    t(msg::editor_menu::OPEN_LINK)
}

/// 链接菜单「复制链接」。
#[must_use]
pub fn editor_menu_copy_link() -> String {
    t(msg::editor_menu::COPY_LINK)
}

/// 链接菜单「复制 Markdown」。
#[must_use]
pub fn editor_menu_copy_markdown() -> String {
    t(msg::editor_menu::COPY_MARKDOWN)
}

// ---------------------------------------------------------------------------
// 设置页（AppShell 与 AppRoot 共用）
// ---------------------------------------------------------------------------

/// AppShell 同步设置页标题「设置 · 同步」。
#[must_use]
pub fn settings_sync_title() -> String {
    t(msg::settings::SYNC_TITLE)
}

/// AppRoot 设置页标题「同步设置（DEV ONLY）」。
#[must_use]
pub fn settings_sync_dev_title() -> String {
    t(msg::settings::SYNC_DEV_TITLE)
}

/// 同步面板已展开标题。
#[must_use]
pub fn settings_sync_panel_expanded() -> String {
    t(msg::settings::SYNC_PANEL_EXPANDED)
}

/// 同步面板默认收起标题。
#[must_use]
pub fn settings_sync_panel_collapsed() -> String {
    t(msg::settings::SYNC_PANEL_COLLAPSED)
}

/// 设置页「展开」。
#[must_use]
pub fn settings_expand() -> String {
    t(msg::settings::EXPAND)
}

/// 设置页「收起」。
#[must_use]
pub fn settings_collapse() -> String {
    t(msg::settings::COLLAPSE)
}

/// 设置页「关闭」。
#[must_use]
pub fn settings_close() -> String {
    t(msg::settings::CLOSE)
}

/// 设置页「保存模式/relay（重启后生效）」。
#[must_use]
pub fn settings_save() -> String {
    t(msg::settings::SAVE)
}

/// 设置页「关闭（不保存）」。
#[must_use]
pub fn settings_close_without_save() -> String {
    t(msg::settings::CLOSE_WITHOUT_SAVE)
}

/// 设置页错误区没有错误时的占位「无错误」。
#[must_use]
pub fn settings_error_none() -> String {
    t(msg::settings::ERROR_NONE)
}

/// 设置页默认提示（保存后重启生效/秘密不要分享）。
#[must_use]
pub fn settings_notice_default() -> String {
    t(msg::settings::NOTICE_DEFAULT)
}

/// 没有 active 实例时的内联提示。
#[must_use]
pub fn settings_no_active_hint() -> String {
    t(msg::settings::NO_ACTIVE_HINT)
}

/// 保存时没有 active 实例的内联错误。
#[must_use]
pub fn settings_no_active_save_error() -> String {
    t(msg::settings::NO_ACTIVE_SAVE_ERROR)
}

/// 配置里还没有实例时的提示。
#[must_use]
pub fn settings_no_instances_hint() -> String {
    t(msg::settings::NO_INSTANCES_HINT)
}

/// 实例列表区标题。
#[must_use]
pub fn settings_instances_title() -> String {
    t(msg::settings::INSTANCES_TITLE)
}

/// relay 模式与地址区标题。
#[must_use]
pub fn settings_relay_title() -> String {
    t(msg::settings::RELAY_TITLE)
}

/// `local_dev` 模式的约束提示。
#[must_use]
pub fn settings_relay_local_hint() -> String {
    t(msg::settings::RELAY_LOCAL_HINT)
}

/// `remote_tls` 模式的约束提示。
#[must_use]
pub fn settings_relay_remote_hint() -> String {
    t(msg::settings::RELAY_REMOTE_HINT)
}

/// 远程服务器/HTTPS 的显式确认文案。
#[must_use]
pub fn settings_remote_consent() -> String {
    t(msg::settings::REMOTE_CONSENT)
}

/// 切到 remote_tls 但没有勾选确认时的内联错误。
#[must_use]
pub fn settings_remote_tls_consent_required() -> String {
    t(msg::settings::REMOTE_TLS_CONSENT_REQUIRED)
}

/// 同步设置页顶部的 DEV ONLY 提示条。
#[must_use]
pub fn settings_sync_dev_banner() -> String {
    t(msg::settings::SYNC_DEV_BANNER)
}

/// 「设为默认」按钮。
#[must_use]
pub fn settings_set_active() -> String {
    t(msg::settings::SET_ACTIVE)
}

/// 「当前默认」按钮（已 active）。
#[must_use]
pub fn settings_active_now() -> String {
    t(msg::settings::ACTIVE_NOW)
}

/// 实例角色「实例」。
#[must_use]
pub fn settings_instance() -> String {
    t(msg::settings::INSTANCE)
}

/// 实例行标题：`{role} · 本机实例 {id}`（`role` 由上面两个函数给出）。
#[must_use]
pub fn settings_instance_row(role: &str, id: &str) -> String {
    t_with(msg::settings::INSTANCE_ROW, &[("role", role), ("id", id)])
}

/// 「复制」按钮。
#[must_use]
pub fn settings_copy() -> String {
    t(msg::settings::COPY)
}

/// 公开信息区标题。
#[must_use]
pub fn settings_public_title() -> String {
    t(msg::settings::PUBLIC_TITLE)
}

/// 安全指纹区标题。
#[must_use]
pub fn settings_fingerprint_title() -> String {
    t(msg::settings::FINGERPRINT_TITLE)
}

/// 尚未打开实例时的指纹占位。
#[must_use]
pub fn settings_fingerprint_unopened() -> String {
    t(msg::settings::FINGERPRINT_UNOPENED)
}

/// 实例未打开时的指纹占位。
#[must_use]
pub fn settings_fingerprint_instance_closed() -> String {
    t(msg::settings::FINGERPRINT_INSTANCE_CLOSED)
}

/// 实例未打开时的指纹占位（详细版：说明切换默认/保存模式只写配置）。
#[must_use]
pub fn settings_fingerprint_instance_closed_detail() -> String {
    t(msg::settings::FINGERPRINT_INSTANCE_CLOSED_DETAIL)
}

/// `vault_id` 只读行。
#[must_use]
pub fn settings_vault_id(id: &str) -> String {
    t_with(msg::settings::VAULT_ID, &[("id", id)])
}

/// `device_id` 只读行。
#[must_use]
pub fn settings_device_id(id: &str) -> String {
    t_with(msg::settings::DEVICE_ID, &[("id", id)])
}

/// 库文件路径只读行（路径本身不翻译）。
#[must_use]
pub fn settings_vault_path(path: &str) -> String {
    t_with(msg::settings::VAULT_PATH, &[("path", path)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use menghunote_i18n::{lock_for_test, Locale, LocaleMode};

    #[test]
    fn migrated_labels_follow_the_global_locale() {
        let guard = lock_for_test();

        guard.reset(Locale::ZhCn);
        assert_eq!(sidebar_notes(), "笔记");
        assert_eq!(sidebar_new_note(), "＋ 笔记");
        assert_eq!(sidebar_new_folder(), "＋ 文件夹");
        assert_eq!(sidebar_export(), "导出");
        assert_eq!(sidebar_exporting(), "导出中…");
        assert_eq!(sidebar_graph(), "图谱");
        assert_eq!(sidebar_settings(), "⚙ 设置");
        assert_eq!(editor_menu_insert(), "插入");
        assert_eq!(editor_menu_table(), "表格");
        assert_eq!(editor_menu_open_link(), "打开链接");
        assert_eq!(editor_menu_copy_markdown(), "复制 Markdown");
        assert_eq!(status_sync_attention(), "同步：需处理");
        assert_eq!(status_sync("已接收"), "同步：已接收");

        guard.reset(Locale::En);
        assert_eq!(sidebar_notes(), "Notes");
        assert_eq!(sidebar_new_note(), "+ Note");
        assert_eq!(sidebar_new_folder(), "+ Folder");
        assert_eq!(sidebar_export(), "Export");
        assert_eq!(sidebar_exporting(), "Exporting…");
        assert_eq!(sidebar_graph(), "Graph");
        assert_eq!(sidebar_settings(), "⚙ Settings");
        assert_eq!(editor_menu_insert(), "Insert");
        assert_eq!(editor_menu_table(), "Table");
        assert_eq!(editor_menu_open_link(), "Open link");
        assert_eq!(editor_menu_copy_markdown(), "Copy Markdown");
        assert_eq!(status_sync_attention(), "Sync: action needed");
        assert_eq!(status_sync("received"), "Sync: received");
    }

    #[test]
    fn backlink_labels_use_english_plural_forms() {
        let guard = lock_for_test();

        guard.reset(Locale::ZhCn);
        assert_eq!(sidebar_backlinks(1), "反向链接 1");
        assert_eq!(sidebar_backlinks(3), "反向链接 3");
        assert_eq!(status_backlinks(0), "反向链接 0");
        assert_eq!(sidebar_backlinks_empty(), "没有反向链接");

        guard.reset(Locale::En);
        assert_eq!(sidebar_backlinks(1), "1 backlink");
        assert_eq!(sidebar_backlinks(3), "3 backlinks");
        assert_eq!(status_backlinks(0), "0 backlinks");
        assert_eq!(sidebar_backlinks_empty(), "No backlinks");
    }

    #[test]
    fn settings_labels_are_templates_not_concatenations() {
        let guard = lock_for_test();

        guard.reset(Locale::ZhCn);
        assert_eq!(
            settings_instance_row(&settings_instance(), "abc"),
            "实例 · 本机实例 abc"
        );
        assert_eq!(settings_vault_path("/tmp/a.db"), "库文件：/tmp/a.db");
        assert_eq!(settings_vault_id("v1"), "vault_id：v1");
        assert_eq!(settings_sync_title(), "设置 · 同步");
        assert_eq!(settings_save(), "保存模式/relay（重启后生效）");

        guard.reset(Locale::En);
        assert_eq!(
            settings_instance_row(&settings_active_now(), "abc"),
            "Active · local instance abc"
        );
        assert_eq!(settings_vault_path("/tmp/a.db"), "Vault file: /tmp/a.db");
        assert_eq!(settings_vault_id("v1"), "vault_id: v1");
        assert_eq!(settings_sync_title(), "Settings · Sync");
        assert_eq!(
            settings_save(),
            "Save mode/relay (takes effect after restart)"
        );
    }

    #[test]
    fn reset_guard_keeps_modes_consistent() {
        let guard = lock_for_test();
        guard.reset(Locale::En);
        assert_eq!(menghunote_i18n::mode(), LocaleMode::En);
        assert_eq!(menghunote_i18n::current_locale(), Locale::En);
    }
}
