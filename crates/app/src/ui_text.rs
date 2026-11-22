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

/// 侧栏「画布」（新建 `.excalidraw` 画布笔记）。
#[must_use]
pub fn sidebar_new_drawing() -> String {
    t(msg::sidebar::NEW_DRAWING)
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

// ---- 最左侧图标栏（ribbon）/ 侧栏排序 ----

/// ribbon「今日日记」。
#[must_use]
pub fn sidebar_ribbon_daily() -> String {
    t(msg::sidebar::RIBBON_DAILY)
}

/// ribbon「从模板新建」。
#[must_use]
pub fn sidebar_ribbon_template() -> String {
    t(msg::sidebar::RIBBON_TEMPLATE)
}

/// ribbon「图谱」。
#[must_use]
pub fn sidebar_ribbon_graph() -> String {
    t(msg::sidebar::RIBBON_GRAPH)
}

/// ribbon「搜索（Ctrl+K）」。
#[must_use]
pub fn sidebar_ribbon_search() -> String {
    t(msg::sidebar::RIBBON_SEARCH)
}

/// 侧栏排序按钮文字「排序」。
#[must_use]
pub fn sidebar_sort_title() -> String {
    t(msg::sidebar::SORT_TITLE)
}

/// 排序键展示名（菜单项与测试共用同一份文案）。
///
/// 参数是存储层的稳定短名（[`menghunote_storage::TreeSort::as_str`]），未知值原样返回，
/// 保证"加了排序键却没加翻译"不会 panic。
#[must_use]
pub fn sidebar_sort_label(key: &str) -> String {
    match key {
        "name_asc" => t(msg::sidebar::SORT_NAME_ASC),
        "name_desc" => t(msg::sidebar::SORT_NAME_DESC),
        "updated_newest" => t(msg::sidebar::SORT_UPDATED_NEWEST),
        "updated_oldest" => t(msg::sidebar::SORT_UPDATED_OLDEST),
        "created_newest" => t(msg::sidebar::SORT_CREATED_NEWEST),
        "created_oldest" => t(msg::sidebar::SORT_CREATED_OLDEST),
        other => other.to_owned(),
    }
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
// 画布笔记（`.excalidraw`）+ Excalidraw 覆盖层
// ---------------------------------------------------------------------------

/// 覆盖层顶部 bar 标题「画布 · <标题>」。
#[must_use]
pub fn drawing_bar_title(title: &str) -> String {
    t_with(msg::drawing::BAR_TITLE, &[("title", title)])
}

/// 覆盖层「保存并关闭」。
#[must_use]
pub fn drawing_save() -> String {
    t(msg::drawing::SAVE)
}

/// 覆盖层「关闭（不保存）」。
#[must_use]
pub fn drawing_close() -> String {
    t(msg::drawing::CLOSE)
}

/// 保存成功提示。
#[must_use]
pub fn drawing_saved() -> String {
    t(msg::drawing::SAVED)
}

/// 保存失败提示（`reason` 是错误原文，不翻译）。
#[must_use]
pub fn drawing_save_failed(reason: &str) -> String {
    t_with(msg::drawing::SAVE_FAILED, &[("reason", reason)])
}

/// 画布关闭后的占位标题（**不显示 JSON 源码**）。
#[must_use]
pub fn drawing_placeholder() -> String {
    t(msg::drawing::PLACEHOLDER)
}

/// 画布关闭后的占位说明。
#[must_use]
pub fn drawing_placeholder_hint() -> String {
    t(msg::drawing::PLACEHOLDER_HINT)
}

/// 占位页「打开画布」。
#[must_use]
pub fn drawing_open() -> String {
    t(msg::drawing::OPEN)
}

// ---------------------------------------------------------------------------
// 设置页（AppShell 与 AppRoot 共用）
// ---------------------------------------------------------------------------

/// AppShell 同步设置页标题「设置 · 同步」。
#[must_use]
pub fn settings_sync_title() -> String {
    t(msg::settings::SYNC_TITLE)
}

/// 产品构建里同步面板的中性标题「同步面板」（**不含 DEV 标注**）。
#[must_use]
pub fn settings_sync_panel_title() -> String {
    t(msg::settings::SYNC_PANEL_TITLE)
}

/// 同步分类里没有可用面板时的说明。
#[must_use]
pub fn settings_sync_unavailable() -> String {
    t(msg::settings::SYNC_UNAVAILABLE)
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

/// relay 模式的只读自动识别说明（设置页顶部；不再有可点选项）。
#[must_use]
pub fn settings_relay_auto_mode() -> String {
    t(msg::settings::RELAY_AUTO_MODE)
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

/// 开发警示条（`DEV ONLY · 开发同步…`）：**只在 `dev-sync` 构建渲染**
///（AppRoot 各页顶部；产品构建不挂这个节点）。
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

// ---------------------------------------------------------------------------
// 模板插件（第一方，ADR 0020 扩展：正文/路径不翻译）
// ---------------------------------------------------------------------------

/// 侧栏「从模板新建…」。
#[must_use]
pub fn sidebar_new_from_template() -> String {
    t(msg::templates::NEW_FROM_TEMPLATE)
}

/// 编辑器右键菜单「插入 → 模板…」。
#[must_use]
pub fn editor_menu_template() -> String {
    t(msg::templates::INSERT_MENU)
}

/// 设置页「模板」区标题。
#[must_use]
pub fn templates_title() -> String {
    t(msg::templates::TITLE)
}

/// 设置页「模板」区提示（普通笔记/本机设置）。
#[must_use]
pub fn templates_hint() -> String {
    t(msg::templates::HINT)
}

/// 模板插件未启用时的提示。
#[must_use]
pub fn templates_disabled_hint() -> String {
    t(msg::templates::DISABLED_HINT)
}

/// 「模板文件夹」字段名。
#[must_use]
pub fn templates_folder_label() -> String {
    t(msg::templates::FOLDER_LABEL)
}

/// 「日期格式」字段名。
#[must_use]
pub fn templates_date_format_label() -> String {
    t(msg::templates::DATE_FORMAT_LABEL)
}

/// 「时间格式」字段名。
#[must_use]
pub fn templates_time_format_label() -> String {
    t(msg::templates::TIME_FORMAT_LABEL)
}

/// 日期/时间示例行。
#[must_use]
pub fn templates_example(example: &str) -> String {
    t_with(msg::templates::EXAMPLE, &[("example", example)])
}

/// 模板设置已保存。
#[must_use]
pub fn templates_saved() -> String {
    t(msg::templates::SAVED)
}

/// 字段为空/无效，未保存。
#[must_use]
pub fn templates_rejected() -> String {
    t(msg::templates::REJECTED)
}

/// 选择器标题「插入模板」。
#[must_use]
pub fn templates_picker_title_insert() -> String {
    t(msg::templates::PICKER_TITLE_INSERT)
}

/// 选择器标题「从模板新建」。
#[must_use]
pub fn templates_picker_title_new() -> String {
    t(msg::templates::PICKER_TITLE_NEW)
}

/// 选择器操作提示（↑/↓/Enter/Esc）。
#[must_use]
pub fn templates_picker_hint() -> String {
    t(msg::templates::PICKER_HINT)
}

/// 选择器搜索框标签。
#[must_use]
pub fn templates_picker_search() -> String {
    t(msg::templates::PICKER_SEARCH)
}

/// 空态：模板文件夹不存在。
#[must_use]
pub fn templates_empty_missing(folder: &str) -> String {
    t_with(msg::templates::EMPTY_MISSING, &[("folder", folder)])
}

/// 空态：模板文件夹为空。
#[must_use]
pub fn templates_empty_folder(folder: &str) -> String {
    t_with(msg::templates::EMPTY_FOLDER, &[("folder", folder)])
}

/// 空态：没有匹配项。
#[must_use]
pub fn templates_empty_no_match() -> String {
    t(msg::templates::EMPTY_NO_MATCH)
}

/// 空态：未配置模板文件夹。
#[must_use]
pub fn templates_empty_no_folder() -> String {
    t(msg::templates::EMPTY_NO_FOLDER)
}

/// 空态：没有打开的笔记（不能插入）。
#[must_use]
pub fn templates_empty_no_note() -> String {
    t(msg::templates::EMPTY_NO_NOTE)
}

/// 已插入模板（`name` 是相对路径）。
#[must_use]
pub fn templates_inserted(name: &str) -> String {
    t_with(msg::templates::INSERTED, &[("name", name)])
}

/// 已从模板新建（`title` 是新标题）。
#[must_use]
pub fn templates_created(title: &str) -> String {
    t_with(msg::templates::CREATED, &[("title", title)])
}

/// 新建成功但正文写入失败。
#[must_use]
pub fn templates_created_insert_failed(title: &str) -> String {
    t_with(msg::templates::CREATED_INSERT_FAILED, &[("title", title)])
}

/// 当前无法插入模板。
#[must_use]
pub fn templates_insert_failed() -> String {
    t(msg::templates::INSERT_FAILED)
}

/// 模板正文为空，未插入。
#[must_use]
pub fn templates_empty_body() -> String {
    t(msg::templates::EMPTY_BODY)
}

/// 从模板新建失败（结构写入被拒绝/出错）。
#[must_use]
pub fn templates_write_failed() -> String {
    t(msg::templates::WRITE_FAILED)
}

/// 读取模板正文失败（`reason` 是错误原文，不翻译）。
#[must_use]
pub fn templates_read_failed(reason: &str) -> String {
    t_with(msg::templates::READ_FAILED, &[("reason", reason)])
}

// ---------------------------------------------------------------------------
// 日记插件（第一方）
// ---------------------------------------------------------------------------

/// 侧栏「今日日记」。
#[must_use]
pub fn sidebar_daily_note() -> String {
    t(msg::daily_notes::OPEN_TODAY)
}

/// 设置页「日记」区标题。
#[must_use]
pub fn daily_notes_title() -> String {
    t(msg::daily_notes::TITLE)
}

/// 设置页「日记」区提示。
#[must_use]
pub fn daily_notes_hint() -> String {
    t(msg::daily_notes::HINT)
}

/// 日记插件未启用时的提示。
#[must_use]
pub fn daily_notes_disabled_hint() -> String {
    t(msg::daily_notes::DISABLED_HINT)
}

/// 「日记文件夹」字段名。
#[must_use]
pub fn daily_notes_folder_label() -> String {
    t(msg::daily_notes::FOLDER_LABEL)
}

/// 「日期格式」字段名。
#[must_use]
pub fn daily_notes_date_format_label() -> String {
    t(msg::daily_notes::DATE_FORMAT_LABEL)
}

/// 「日记模板路径」字段名。
#[must_use]
pub fn daily_notes_template_label() -> String {
    t(msg::daily_notes::TEMPLATE_LABEL)
}

/// 模板路径提示（可空/任意普通笔记）。
#[must_use]
pub fn daily_notes_template_hint() -> String {
    t(msg::daily_notes::TEMPLATE_HINT)
}

/// 今天路径示例。
#[must_use]
pub fn daily_notes_example(path: &str) -> String {
    t_with(msg::daily_notes::EXAMPLE, &[("path", path)])
}

/// 日记设置已保存。
#[must_use]
pub fn daily_notes_saved() -> String {
    t(msg::daily_notes::SAVED)
}

/// 字段为空/无效，未保存。
#[must_use]
pub fn daily_notes_rejected() -> String {
    t(msg::daily_notes::REJECTED)
}

/// 已打开今日日记（`path` 是完整路径）。
#[must_use]
pub fn daily_notes_opened(path: &str) -> String {
    t_with(msg::daily_notes::OPENED, &[("path", path)])
}

/// 已创建今日日记。
#[must_use]
pub fn daily_notes_created(path: &str) -> String {
    t_with(msg::daily_notes::CREATED, &[("path", path)])
}

/// 创建今日日记失败（`reason` 不翻译）。
#[must_use]
pub fn daily_notes_create_failed(reason: &str) -> String {
    t_with(msg::daily_notes::CREATE_FAILED, &[("reason", reason)])
}

/// 日记模板丢失（已创建空日记）。
#[must_use]
pub fn daily_notes_template_missing(path: &str) -> String {
    t_with(msg::daily_notes::TEMPLATE_MISSING, &[("path", path)])
}

/// 日期格式没有产出有效笔记名。
#[must_use]
pub fn daily_notes_invalid_path() -> String {
    t(msg::daily_notes::INVALID_PATH)
}

/// 相对日期偏移越界（未来日历 UI 用；当前侧栏入口不会触发）。
#[must_use]
pub fn daily_notes_out_of_range() -> String {
    t(msg::daily_notes::OUT_OF_RANGE)
}

/// 日记结构写入被拒绝/出错（通用原因，具体见 stderr）。
#[must_use]
pub fn daily_notes_write_failed() -> String {
    t(msg::daily_notes::WRITE_FAILED)
}

/// 设置页「选择模板笔记…」按钮。
#[must_use]
pub fn daily_notes_pick_template() -> String {
    t(msg::daily_notes::PICK_TEMPLATE)
}

/// 笔记选择器标题「选择日记模板」。
#[must_use]
pub fn daily_notes_picker_title() -> String {
    t(msg::daily_notes::PICKER_TITLE)
}

/// 笔记选择器操作提示（↑/↓/Enter/Esc）。
#[must_use]
pub fn daily_notes_picker_hint() -> String {
    t(msg::daily_notes::PICKER_HINT)
}

/// 笔记选择器搜索框标签。
#[must_use]
pub fn daily_notes_picker_search() -> String {
    t(msg::daily_notes::PICKER_SEARCH)
}

/// 「当前」（标记当前已选模板）。
#[must_use]
pub fn daily_notes_picker_current() -> String {
    t(msg::daily_notes::PICKER_CURRENT)
}

/// 空态：库里还没有任何笔记。
#[must_use]
pub fn daily_notes_picker_empty_library() -> String {
    t(msg::daily_notes::PICKER_EMPTY_LIBRARY)
}

/// 空态：搜索没有匹配的笔记。
#[must_use]
pub fn daily_notes_picker_empty_match() -> String {
    t(msg::daily_notes::PICKER_EMPTY_MATCH)
}

// ---------------------------------------------------------------------------
// 仅本地使用（不同步）：产品入口 + dev-sync 开发入口 + 主界面提示/错误页
// ---------------------------------------------------------------------------

/// Welcome 本地区块标题「没有同步需求？」。
#[must_use]
pub fn local_welcome_title() -> String {
    t(msg::local::WELCOME_TITLE)
}

/// Welcome：本地库说明（不联网、不创建同步实例、不参与同步）。
#[must_use]
pub fn local_welcome_hint() -> String {
    t(msg::local::WELCOME_HINT)
}

/// Welcome：本地库实际路径（`{path}` 是公开路径，不经过翻译）。
#[must_use]
pub fn local_path_hint(path: &str) -> String {
    t_with(msg::local::PATH_HINT, &[("path", path)])
}

/// **产品构建**入口文案「仅本地使用（不同步）」。
#[must_use]
pub fn local_action() -> String {
    t(msg::local::ACTION)
}

/// **dev-sync 构建**开发入口文案「继续本地（DEV ONLY，无同步）」。
#[must_use]
pub fn local_dev_action() -> String {
    t(msg::local::DEV_ACTION)
}

/// dev-sync 开发入口说明（打开默认 dev vault；仅供开发）。
#[must_use]
pub fn local_dev_hint() -> String {
    t(msg::local::DEV_HINT)
}

/// 主界面提示「本地模式：不同步」。
#[must_use]
pub fn local_mode_notice() -> String {
    t(msg::local::MODE_NOTICE)
}

/// 启动时上次的本地库已不存在（`{path}` 是公开路径）：回 Welcome 的明确提示。
#[must_use]
pub fn local_resume_missing(path: &str) -> String {
    t_with(msg::local::RESUME_MISSING, &[("path", path)])
}

/// 产品构建打开本地库失败的 Error 页标题。
#[must_use]
pub fn local_open_failed_title() -> String {
    t(msg::local::OPEN_FAILED_TITLE)
}

/// dev-sync 构建打开 dev vault 失败的 Error 页标题（保留开发措辞）。
#[must_use]
pub fn local_dev_open_failed_title() -> String {
    t(msg::local::DEV_OPEN_FAILED_TITLE)
}

/// Error 页公开字段标签「目录」。
#[must_use]
pub fn local_dir_label() -> String {
    t(msg::local::DIR_LABEL)
}

// ---------------------------------------------------------------------------
// 用户自定义仓库名称（Welcome 统一同步表单 + 管理仓库「重命名」）
// ---------------------------------------------------------------------------

/// Welcome 同步表单 / 管理仓库重命名的字段标签「仓库名称」。
#[must_use]
pub fn vault_name_label() -> String {
    t(msg::vault::NAME_LABEL)
}

/// 「仓库名称」输入框占位：留空则使用目录名。
#[must_use]
pub fn vault_name_placeholder() -> String {
    t(msg::vault::NAME_PLACEHOLDER)
}

/// 管理仓库每行的「重命名」按钮/标题。
#[must_use]
pub fn vault_rename() -> String {
    t(msg::vault::RENAME)
}

/// 重命名行内输入的「保存」。
#[must_use]
pub fn vault_rename_save() -> String {
    t(msg::vault::RENAME_SAVE)
}

/// 重命名行内输入的「取消」（Esc 等价）。
#[must_use]
pub fn vault_rename_cancel() -> String {
    t(msg::vault::RENAME_CANCEL)
}

// ---------------------------------------------------------------------------
// 设置页左侧分类导航 / 关于（AppRoot 产品设置）
// ---------------------------------------------------------------------------

/// 设置导航标题「设置」。
#[must_use]
pub fn settings_nav_title() -> String {
    t(msg::settings::NAV_TITLE)
}

/// 导航项「外观」。
#[must_use]
pub fn settings_nav_appearance() -> String {
    t(msg::settings::NAV_APPEARANCE)
}

/// 导航项「语言」。
#[must_use]
pub fn settings_nav_language() -> String {
    t(msg::settings::NAV_LANGUAGE)
}

/// 导航项「快捷键」。
#[must_use]
pub fn settings_nav_shortcuts() -> String {
    t(msg::settings::NAV_SHORTCUTS)
}

/// 导航项「模板」。
#[must_use]
pub fn settings_nav_templates() -> String {
    t(msg::settings::NAV_TEMPLATES)
}

/// 导航项「日记」。
#[must_use]
pub fn settings_nav_daily_notes() -> String {
    t(msg::settings::NAV_DAILY_NOTES)
}

/// 导航项「同步」。
#[must_use]
pub fn settings_nav_sync() -> String {
    t(msg::settings::NAV_SYNC)
}

/// 导航项「实例」。
#[must_use]
pub fn settings_nav_instances() -> String {
    t(msg::settings::NAV_INSTANCES)
}

/// 导航项「Relay」。
#[must_use]
pub fn settings_nav_relay() -> String {
    t(msg::settings::NAV_RELAY)
}

/// 导航项「安全」。
#[must_use]
pub fn settings_nav_security() -> String {
    t(msg::settings::NAV_SECURITY)
}

/// 导航项「插件」（固定分类；只列第一方核心插件）。
#[must_use]
pub fn settings_nav_plugins() -> String {
    t(msg::settings::NAV_PLUGINS)
}

/// 导航项「关于」。
#[must_use]
pub fn settings_nav_about() -> String {
    t(msg::settings::NAV_ABOUT)
}

// ---- 「插件」分类：第一方核心插件总开关 ----

/// 「插件」分类标题（核心插件）。
#[must_use]
pub fn settings_plugins_title() -> String {
    t(msg::settings::PLUGINS_TITLE)
}

/// 「插件」分类说明（关闭后立即隐藏分类与侧栏入口；本机偏好）。
#[must_use]
pub fn settings_plugins_hint() -> String {
    t(msg::settings::PLUGINS_HINT)
}

/// 模板插件说明。
#[must_use]
pub fn settings_plugins_templates_description() -> String {
    t(msg::settings::PLUGINS_TEMPLATES_DESCRIPTION)
}

/// 日记插件说明。
#[must_use]
pub fn settings_plugins_daily_notes_description() -> String {
    t(msg::settings::PLUGINS_DAILY_NOTES_DESCRIPTION)
}

/// 插件开关状态「已启用」。
#[must_use]
pub fn settings_plugins_enabled() -> String {
    t(msg::settings::PLUGINS_ENABLED)
}

/// 插件开关状态「已停用」。
#[must_use]
pub fn settings_plugins_disabled() -> String {
    t(msg::settings::PLUGINS_DISABLED)
}

/// 「关于」区标题。
#[must_use]
pub fn settings_about_title() -> String {
    t(msg::settings::ABOUT_TITLE)
}

/// 「关于」区版本行（`{version}` = Cargo 包版本）。
#[must_use]
pub fn settings_about_version(version: &str) -> String {
    t_with(msg::settings::ABOUT_VERSION, &[("version", version)])
}

/// 「关于」区构建行（`{build}` = 编译期 feature 列表）。
#[must_use]
pub fn settings_about_build(build: &str) -> String {
    t_with(msg::settings::ABOUT_BUILD, &[("build", build)])
}

/// 「关于」区说明（只读、不改行为）。
#[must_use]
pub fn settings_about_hint() -> String {
    t(msg::settings::ABOUT_HINT)
}

// ---------------------------------------------------------------------------
// 笔记属性（YAML frontmatter 面板）
// ---------------------------------------------------------------------------

/// 属性面板标题「笔记属性」。
#[must_use]
pub fn properties_title() -> String {
    t(msg::properties::TITLE)
}

/// 「添加笔记属性」。
#[must_use]
pub fn properties_add() -> String {
    t(msg::properties::ADD)
}

/// 空值占位「没有值」。
#[must_use]
pub fn properties_no_value() -> String {
    t(msg::properties::NO_VALUE)
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
    fn notes_plugin_labels_follow_the_global_locale() {
        let guard = lock_for_test();

        guard.reset(Locale::ZhCn);
        assert_eq!(sidebar_new_from_template(), "从模板新建…");
        assert_eq!(sidebar_daily_note(), "今日日记");
        assert_eq!(settings_nav_plugins(), "插件");
        assert_eq!(settings_plugins_title(), "核心插件");
        assert_eq!(settings_plugins_enabled(), "已启用");
        assert_eq!(settings_plugins_disabled(), "已停用");
        assert_eq!(
            settings_sync_unavailable(),
            "当前没有打开的实例：同步面板只对已打开的实例可用。"
        );
        assert_eq!(editor_menu_template(), "模板…");
        assert_eq!(templates_title(), "模板");
        assert!(
            templates_hint().contains("{{date:-1d}}")
                && templates_hint().contains("{{date:-1m:YYYY/MM/DD}}"),
            "模板设置 hint 必须说明相对日期语法"
        );
        assert_eq!(
            templates_example("2026-11-18 08:14"),
            "示例：2026-11-18 08:14（当前设置）"
        );
        assert_eq!(
            templates_empty_missing("模板"),
            "模板文件夹「模板」不存在（不会自动创建）"
        );
        assert_eq!(templates_empty_no_match(), "没有匹配的模板");
        assert_eq!(templates_inserted("日记模板"), "已插入模板「日记模板」");
        assert_eq!(templates_created("周记 2"), "已从模板新建「周记 2」");
        assert_eq!(templates_read_failed("io"), "读取模板正文失败：io");
        assert_eq!(daily_notes_title(), "日记");
        assert!(
            daily_notes_hint().contains("{{date:-1d}}")
                && daily_notes_hint().contains("{{date:+1d}}"),
            "日记设置 hint 必须说明相对日期语法"
        );
        assert_eq!(
            daily_notes_out_of_range(),
            "相对日期偏移超出范围（检查日历/日期设置）"
        );
        assert_eq!(
            daily_notes_example("日记/2026-11-18"),
            "今天的日记：日记/2026-11-18"
        );
        assert_eq!(
            daily_notes_created("日记/2026-11-18"),
            "已创建今日日记「日记/2026-11-18」"
        );
        assert_eq!(
            daily_notes_template_missing("模板/日记模板"),
            "日记模板「模板/日记模板」不存在：已创建空日记（可在设置页修正路径）"
        );
        assert_eq!(daily_notes_pick_template(), "选择模板笔记…");
        assert_eq!(daily_notes_picker_title(), "选择日记模板");
        assert_eq!(daily_notes_picker_search(), "搜索");
        assert_eq!(daily_notes_picker_current(), "当前");
        assert_eq!(
            daily_notes_picker_empty_library(),
            "库中还没有笔记；先新建一篇笔记，再选择日记模板"
        );
        assert_eq!(daily_notes_picker_empty_match(), "没有匹配的笔记");

        guard.reset(Locale::En);
        assert_eq!(sidebar_new_from_template(), "New from template…");
        assert_eq!(sidebar_daily_note(), "Today");
        assert_eq!(settings_nav_plugins(), "Plugins");
        assert_eq!(settings_plugins_title(), "Core plugins");
        assert_eq!(settings_plugins_enabled(), "Enabled");
        assert_eq!(settings_plugins_disabled(), "Disabled");
        assert_eq!(editor_menu_template(), "Template…");
        assert_eq!(templates_title(), "Templates");
        assert!(
            templates_hint().contains("{{date:-1d}}") && templates_hint().contains("{{date:+1w}}"),
            "English template hint must document relative dates"
        );
        assert_eq!(
            templates_example("2026-11-18 08:14"),
            "Example: 2026-11-18 08:14 (current settings)"
        );
        assert_eq!(
            templates_empty_missing("Templates"),
            "Template folder \"Templates\" does not exist (never created automatically)"
        );
        assert_eq!(templates_empty_no_match(), "No matching templates");
        assert_eq!(templates_inserted("Weekly"), "Inserted template \"Weekly\"");
        assert_eq!(
            templates_created("Weekly 2"),
            "Created \"Weekly 2\" from a template"
        );
        assert_eq!(daily_notes_title(), "Daily notes");
        assert!(
            daily_notes_hint().contains("{{date:-1d}}")
                && daily_notes_hint().contains("{{date:+1d}}"),
            "English daily-notes hint must document relative dates"
        );
        assert_eq!(
            daily_notes_out_of_range(),
            "The relative date offset is out of range (check the calendar/date settings)"
        );
        assert_eq!(
            daily_notes_example("Daily/2026-11-18"),
            "Today's daily note: Daily/2026-11-18"
        );
        assert_eq!(daily_notes_pick_template(), "Choose template note…");
        assert_eq!(daily_notes_picker_title(), "Choose daily template");
        assert_eq!(daily_notes_picker_search(), "Search");
        assert_eq!(daily_notes_picker_current(), "Current");
        assert_eq!(
            daily_notes_picker_empty_library(),
            "No notes in the library yet; create a note first, then choose a daily template"
        );
        assert_eq!(daily_notes_picker_empty_match(), "No matching notes");
    }

    #[test]
    fn local_only_labels_follow_the_global_locale() {
        let guard = lock_for_test();

        guard.reset(Locale::ZhCn);
        assert_eq!(local_action(), "仅本地使用（不同步）");
        assert_eq!(local_dev_action(), "继续本地（DEV ONLY，无同步）");
        assert_eq!(local_mode_notice(), "本地模式：不同步");
        assert_eq!(local_open_failed_title(), "无法打开本地笔记库");
        assert_eq!(local_dir_label(), "目录");
        assert_eq!(local_welcome_title(), "没有同步需求？");
        assert_eq!(
            local_path_hint("/data/menghunote/local/vault.sqlite3"),
            "本地库：/data/menghunote/local/vault.sqlite3"
        );
        assert_eq!(
            local_resume_missing("/data/menghunote/local/vault.sqlite3"),
            "上次使用的本地笔记库不存在：/data/menghunote/local/vault.sqlite3；已回到欢迎页（不会自动新建库）"
        );

        guard.reset(Locale::En);
        assert_eq!(local_action(), "Use locally only (no sync)");
        assert_eq!(local_dev_action(), "Continue locally (DEV ONLY, no sync)");
        assert_eq!(local_mode_notice(), "Local mode: not synced");
        assert_eq!(local_open_failed_title(), "Unable to open the local vault");
        assert_eq!(local_dir_label(), "Directory");
        assert_eq!(local_welcome_title(), "Don't need sync?");
        assert_eq!(
            local_path_hint("/data/menghunote/local/vault.sqlite3"),
            "Local vault: /data/menghunote/local/vault.sqlite3"
        );
        assert_eq!(
            local_resume_missing("/data/menghunote/local/vault.sqlite3"),
            "The local vault from last time is missing: /data/menghunote/local/vault.sqlite3; returned to Welcome (no vault was created)"
        );
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
        assert_eq!(settings_sync_panel_title(), "同步面板");
        assert_eq!(settings_save(), "保存 relay 设置（重启后生效）");

        guard.reset(Locale::En);
        assert_eq!(
            settings_instance_row(&settings_active_now(), "abc"),
            "Active · local instance abc"
        );
        assert_eq!(settings_vault_path("/tmp/a.db"), "Vault file: /tmp/a.db");
        assert_eq!(settings_vault_id("v1"), "vault_id: v1");
        assert_eq!(settings_sync_title(), "Settings · Sync");
        assert_eq!(settings_sync_panel_title(), "Sync panel");
        assert_eq!(
            settings_save(),
            "Save relay settings (takes effect after restart)"
        );
    }

    /// 用户自定义仓库名称：中文默认 + 英文 key 都有（Welcome 表单/管理仓库共用）。
    #[test]
    fn vault_name_labels_follow_the_global_locale() {
        let guard = lock_for_test();

        guard.reset(Locale::ZhCn);
        assert_eq!(vault_name_label(), "仓库名称");
        assert_eq!(vault_name_placeholder(), "留空则使用目录名");
        assert_eq!(vault_rename(), "重命名");
        assert_eq!(vault_rename_save(), "保存");
        assert_eq!(vault_rename_cancel(), "取消");

        guard.reset(Locale::En);
        assert_eq!(vault_name_label(), "Vault name");
        assert_eq!(
            vault_name_placeholder(),
            "Leave blank to use the folder name"
        );
        assert_eq!(vault_rename(), "Rename");
        assert_eq!(vault_rename_save(), "Save");
        assert_eq!(vault_rename_cancel(), "Cancel");
    }

    #[test]
    fn reset_guard_keeps_modes_consistent() {
        let guard = lock_for_test();
        guard.reset(Locale::En);
        assert_eq!(menghunote_i18n::mode(), LocaleMode::En);
        assert_eq!(menghunote_i18n::current_locale(), Locale::En);
    }
}
