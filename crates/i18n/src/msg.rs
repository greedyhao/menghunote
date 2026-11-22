//! 消息 id 常量与命名空间（ADR 0020 P1）。
//!
//! 每条 UI 固定文案都有一个**稳定 id**（点分命名空间），业务代码只允许写
//! `t(msg::sidebar::NOTES)` 这类调用，不允许把中文/英文直接写进渲染逻辑。
//!
//! 命名空间约定（P1 只落地前四个）：
//!
//! | 前缀 | 用途 |
//! | --- | --- |
//! | `settings.*` | 设置页固定文案（外观/语言/同步区/按钮/错误提示） |
//! | `sidebar.*` | 侧栏与反向链接面板固定文案 |
//! | `editor.menu.*` | 编辑器右键菜单固定文案 |
//! | `status.*` | 底栏固定提示 |
//! | `plugin.<plugin_id>.*` | **后置**：第三方插件命名空间（插件 API 冻结后再定） |
//!
//! [`ALL`] 由宏一并生成，供 catalog 一致性测试断言"常量集合 == 两个语言的
//! catalog key 集合"，避免加了常量却漏翻/加了 key 却没常量。

macro_rules! define_messages {
    ($($module:ident { $($name:ident = $id:literal;)* })*) => {
        $(
            pub mod $module {
                $(
                    #[doc = concat!("消息 id：`", $id, "`")]
                    pub const $name: &str = $id;
                )*
            }
        )*

        /// 全部消息 id（顺序与定义顺序一致；catalog 测试按集合比较）。
        pub const ALL: &[&str] = &[$($($id,)*)*];
    };
}

define_messages! {
    settings {
        APPEARANCE_TITLE = "settings.appearance.title";
        APPEARANCE_HINT = "settings.appearance.hint";
        APPEARANCE_LIGHT = "settings.appearance.light";
        APPEARANCE_DARK = "settings.appearance.dark";
        APPEARANCE_SYSTEM = "settings.appearance.system";
        APPEARANCE_CURRENT = "settings.appearance.current";

        LANGUAGE_TITLE = "settings.language.title";
        LANGUAGE_HINT = "settings.language.hint";
        LANGUAGE_ZH = "settings.language.zh";
        LANGUAGE_EN = "settings.language.en";
        LANGUAGE_SYSTEM = "settings.language.system";
        LANGUAGE_CURRENT = "settings.language.current";
        LANGUAGE_CURRENT_SYSTEM = "settings.language.current_system";

        SYNC_TITLE = "settings.sync.title";
        SYNC_DEV_TITLE = "settings.sync.dev_title";
        SYNC_DEV_BANNER = "settings.sync.dev_banner";
        SYNC_PANEL_EXPANDED = "settings.sync.panel_expanded";
        SYNC_PANEL_COLLAPSED = "settings.sync.panel_collapsed";

        EXPAND = "settings.expand";
        COLLAPSE = "settings.collapse";
        CLOSE = "settings.close";
        SAVE = "settings.save";
        CLOSE_WITHOUT_SAVE = "settings.close_without_save";
        ERROR_NONE = "settings.error_none";
        NOTICE_DEFAULT = "settings.notice.default";
        NO_ACTIVE_HINT = "settings.no_active_hint";
        NO_ACTIVE_SAVE_ERROR = "settings.no_active_save_error";
        NO_INSTANCES_HINT = "settings.no_instances_hint";
        INSTANCES_TITLE = "settings.instances.title";
        RELAY_TITLE = "settings.relay.title";
        RELAY_LOCAL_HINT = "settings.relay.local_hint";
        RELAY_REMOTE_HINT = "settings.relay.remote_hint";
        REMOTE_CONSENT = "settings.remote_consent";
        REMOTE_TLS_CONSENT_REQUIRED = "settings.remote_tls_consent_required";
        SET_ACTIVE = "settings.set_active";
        ACTIVE_NOW = "settings.active_now";
        INSTANCE = "settings.instance";
        INSTANCE_ROW = "settings.instance_row";
        COPY = "settings.copy";
        PUBLIC_TITLE = "settings.public.title";
        FINGERPRINT_TITLE = "settings.fingerprint.title";
        FINGERPRINT_UNOPENED = "settings.fingerprint.unopened";
        FINGERPRINT_INSTANCE_CLOSED = "settings.fingerprint.instance_closed";
        FINGERPRINT_INSTANCE_CLOSED_DETAIL = "settings.fingerprint.instance_closed_detail";
        VAULT_ID = "settings.vault_id";
        DEVICE_ID = "settings.device_id";
        VAULT_PATH = "settings.vault_path";
    }

    sidebar {
        NOTES = "sidebar.notes";
        NEW_NOTE = "sidebar.new_note";
        NEW_FOLDER = "sidebar.new_folder";
        EXPORT = "sidebar.export";
        EXPORTING = "sidebar.exporting";
        GRAPH = "sidebar.graph";
        SETTINGS = "sidebar.settings";
        BACKLINKS_ONE = "sidebar.backlinks.one";
        BACKLINKS_OTHER = "sidebar.backlinks.other";
        BACKLINKS_EMPTY = "sidebar.backlinks_empty";
    }

    editor_menu {
        INSERT = "editor.menu.insert";
        TABLE = "editor.menu.table";
        OPEN_LINK = "editor.menu.open_link";
        COPY_LINK = "editor.menu.copy_link";
        COPY_MARKDOWN = "editor.menu.copy_markdown";
    }

    status {
        BACKLINKS_ONE = "status.backlinks.one";
        BACKLINKS_OTHER = "status.backlinks.other";
        SYNC = "status.sync";
        SYNC_ATTENTION = "status.sync_attention";
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn all_ids_are_unique_and_namespaced() {
        let mut seen = HashSet::new();
        for id in ALL {
            assert!(seen.insert(*id), "重复的消息 id：{id}");
            assert!(id.contains('.'), "消息 id 必须带命名空间：{id}");
            assert!(
                [
                    "settings.",
                    "sidebar.",
                    "editor.menu.",
                    "status.",
                    "plugin."
                ]
                .iter()
                .any(|prefix| id.starts_with(prefix)),
                "未知命名空间：{id}"
            );
            assert!(
                id.chars()
                    .all(|character| character.is_ascii_alphanumeric() || "._".contains(character)),
                "消息 id 只允许 ASCII 字母/数字/./_：{id}"
            );
        }
        assert!(!ALL.is_empty());
    }

    #[test]
    fn namespaced_constants_match_all() {
        // 抽查各命名空间至少有一个常量被登记，防止宏展开后 ALL 与模块脱节。
        assert!(ALL.contains(&settings::SYNC_TITLE));
        assert!(ALL.contains(&sidebar::NOTES));
        assert!(ALL.contains(&editor_menu::TABLE));
        assert!(ALL.contains(&status::SYNC));
    }
}
