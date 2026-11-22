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
//! | `templates.*` | 模板插件（第一方）：侧栏/菜单/选择器/设置页固定文案 |
//! | `daily_notes.*` | 日记插件（第一方）：侧栏/设置页/提示固定文案 |
//! | `local.*` | 「仅本地使用（不同步）」入口：Welcome 文案、主界面提示、错误页 |
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
        NAV_TITLE = "settings.nav.title";
        NAV_APPEARANCE = "settings.nav.appearance";
        NAV_LANGUAGE = "settings.nav.language";
        NAV_TEMPLATES = "settings.nav.templates";
        NAV_DAILY_NOTES = "settings.nav.daily_notes";
        NAV_SYNC = "settings.nav.sync";
        NAV_INSTANCES = "settings.nav.instances";
        NAV_RELAY = "settings.nav.relay";
        NAV_SECURITY = "settings.nav.security";
        NAV_PLUGINS = "settings.nav.plugins";
        NAV_ABOUT = "settings.nav.about";

        PLUGINS_TITLE = "settings.plugins.title";
        PLUGINS_HINT = "settings.plugins.hint";
        PLUGINS_TEMPLATES_DESCRIPTION = "settings.plugins.templates.description";
        PLUGINS_DAILY_NOTES_DESCRIPTION = "settings.plugins.daily_notes.description";
        PLUGINS_ENABLED = "settings.plugins.enabled";
        PLUGINS_DISABLED = "settings.plugins.disabled";

        ABOUT_TITLE = "settings.about.title";
        ABOUT_VERSION = "settings.about.version";
        ABOUT_BUILD = "settings.about.build";
        ABOUT_HINT = "settings.about.hint";

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
        SYNC_PANEL_TITLE = "settings.sync.panel_title";
        SYNC_DEV_BANNER = "settings.sync.dev_banner";
        SYNC_UNAVAILABLE = "settings.sync.unavailable";

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
        // 画布笔记（`.excalidraw`）：侧栏动作行的「画布」按钮。
        NEW_DRAWING = "sidebar.new_drawing";
        EXPORT = "sidebar.export";
        EXPORTING = "sidebar.exporting";
        GRAPH = "sidebar.graph";
        SETTINGS = "sidebar.settings";
        BACKLINKS_ONE = "sidebar.backlinks.one";
        BACKLINKS_OTHER = "sidebar.backlinks.other";
        BACKLINKS_EMPTY = "sidebar.backlinks_empty";
        SORT_TITLE = "sidebar.sort.title";
        SORT_NAME_ASC = "sidebar.sort.name_asc";
        SORT_NAME_DESC = "sidebar.sort.name_desc";
        SORT_UPDATED_NEWEST = "sidebar.sort.updated_newest";
        SORT_UPDATED_OLDEST = "sidebar.sort.updated_oldest";
        SORT_CREATED_NEWEST = "sidebar.sort.created_newest";
        SORT_CREATED_OLDEST = "sidebar.sort.created_oldest";
        RIBBON_DAILY = "sidebar.ribbon.daily";
        RIBBON_TEMPLATE = "sidebar.ribbon.template";
        RIBBON_GRAPH = "sidebar.ribbon.graph";
        RIBBON_SEARCH = "sidebar.ribbon.search";
    }

    // 画布笔记（`.excalidraw`）+ Excalidraw 覆盖层：bar 标题与两个按钮。
    // 标题是**文件名语义**（`.excalidraw` 后缀），动态部分走 `{title}` 插值。
    drawing {
        BAR_TITLE = "drawing.bar_title";
        SAVE = "drawing.save";
        CLOSE = "drawing.close";
        SAVED = "drawing.saved";
        SAVE_FAILED = "drawing.save_failed";
        PLACEHOLDER = "drawing.placeholder";
        PLACEHOLDER_HINT = "drawing.placeholder_hint";
        OPEN = "drawing.open";
    }

    // 笔记属性（YAML frontmatter 面板）。
    properties {
        TITLE = "properties.title";
        ADD = "properties.add";
        NO_VALUE = "properties.no_value";
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

    templates {
        TITLE = "templates.title";
        HINT = "templates.hint";
        DISABLED_HINT = "templates.disabled_hint";
        FOLDER_LABEL = "templates.folder_label";
        DATE_FORMAT_LABEL = "templates.date_format_label";
        TIME_FORMAT_LABEL = "templates.time_format_label";
        EXAMPLE = "templates.example";
        SAVED = "templates.saved";
        REJECTED = "templates.rejected";
        INSERT_MENU = "templates.insert_menu";
        NEW_FROM_TEMPLATE = "templates.new_from_template";
        PICKER_TITLE_INSERT = "templates.picker_title_insert";
        PICKER_TITLE_NEW = "templates.picker_title_new";
        PICKER_HINT = "templates.picker_hint";
        PICKER_SEARCH = "templates.picker_search";
        EMPTY_MISSING = "templates.empty_missing";
        EMPTY_FOLDER = "templates.empty_folder";
        EMPTY_NO_MATCH = "templates.empty_no_match";
        EMPTY_NO_FOLDER = "templates.empty_no_folder";
        EMPTY_NO_NOTE = "templates.empty_no_note";
        INSERTED = "templates.inserted";
        CREATED = "templates.created";
        INSERT_FAILED = "templates.insert_failed";
        EMPTY_BODY = "templates.empty_body";
        CREATED_INSERT_FAILED = "templates.created_insert_failed";
        WRITE_FAILED = "templates.write_failed";
        READ_FAILED = "templates.read_failed";
    }

    daily_notes {
        TITLE = "daily_notes.title";
        HINT = "daily_notes.hint";
        DISABLED_HINT = "daily_notes.disabled_hint";
        FOLDER_LABEL = "daily_notes.folder_label";
        DATE_FORMAT_LABEL = "daily_notes.date_format_label";
        TEMPLATE_LABEL = "daily_notes.template_label";
        TEMPLATE_HINT = "daily_notes.template_hint";
        EXAMPLE = "daily_notes.example";
        SAVED = "daily_notes.saved";
        REJECTED = "daily_notes.rejected";
        OPEN_TODAY = "daily_notes.open_today";
        OPENED = "daily_notes.opened";
        CREATED = "daily_notes.created";
        CREATE_FAILED = "daily_notes.create_failed";
        TEMPLATE_MISSING = "daily_notes.template_missing";
        INVALID_PATH = "daily_notes.invalid_path";
        OUT_OF_RANGE = "daily_notes.out_of_range";
        WRITE_FAILED = "daily_notes.write_failed";
        PICK_TEMPLATE = "daily_notes.pick_template";
        PICKER_TITLE = "daily_notes.template_picker_title";
        PICKER_HINT = "daily_notes.template_picker_hint";
        PICKER_SEARCH = "daily_notes.template_picker_search";
        PICKER_CURRENT = "daily_notes.template_picker_current";
        PICKER_EMPTY_LIBRARY = "daily_notes.template_picker_empty_library";
        PICKER_EMPTY_MATCH = "daily_notes.template_picker_empty_match";
    }

    // 仅本地使用（不同步）：产品入口 + dev-sync 开发入口 + 主界面提示/错误页。
    // `*_dev_*` 只在 `dev-sync` 构建出现（旧「继续本地（DEV ONLY）」行为），
    // 产品构建用的是不带 `dev` 的一组。
    local {
        WELCOME_TITLE = "local.welcome_title";
        WELCOME_HINT = "local.welcome_hint";
        PATH_HINT = "local.path_hint";
        ACTION = "local.action";
        MODE_NOTICE = "local.mode_notice";
        RESUME_MISSING = "local.resume_missing";
        OPEN_FAILED_TITLE = "local.open_failed_title";
        DIR_LABEL = "local.dir_label";
        DEV_ACTION = "local.dev_action";
        DEV_HINT = "local.dev_hint";
        DEV_OPEN_FAILED_TITLE = "local.dev_open_failed_title";
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
                    "drawing.",
                    "properties.",
                    "status.",
                    "templates.",
                    "daily_notes.",
                    "local.",
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
        assert!(ALL.contains(&templates::TITLE));
        assert!(ALL.contains(&daily_notes::TITLE));
        assert!(ALL.contains(&local::MODE_NOTICE));
    }
}
