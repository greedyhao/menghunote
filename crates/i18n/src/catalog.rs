//! 静态 key-value catalog（ADR 0020 P1）。
//!
//! 为什么先用**静态数组**而不是 fluent/JSON 资源文件：
//!
//! - 本切片只需要 `t(id)` / `{name}` 插值 / 单复数两条 key，静态数组零依赖、
//!   零启动成本、编译期就能做 key 集合一致性测试；
//! - ADR 0020 §8 的第 1 个待决问题（fluent vs 轻量 key-value）要等插件 API
//!   冻结前才评估，过早引入重依赖会绑死后续格式；
//! - 两个 catalog 的 key 集合**必须完全一致**（[`missing_translations`] 为空），
//!   测试会拦住"只加了中文/只加了英文"的改动。
//!
//! 找不到 key 的运行时回退规则在 [`crate::translate`]（当前语言 → 英文 →
//! 中文 → key 本身）。

use crate::locale::Locale;

/// 简体中文 catalog。
///
/// 复数规则：中文单复数同形，`*.one` / `*.other` 两条 key 的文案相同
/// （仍然带 `{count}`），由 [`crate::t_count`] 选择 key。
pub const ZH_CN: &[(&str, &str)] = &[
    // ---- settings.nav（设置页左侧分类导航）----
    ("settings.about.build", "构建：{build}"),
    ("settings.about.hint", "版本号来自 Cargo 清单；构建特性只读展示，不改任何运行行为"),
    ("settings.about.title", "关于"),
    (
        "settings.about.version",
        "menghunote {version}（GPUI Kit 0.9.1；SQLite 单库为持久化真源）",
    ),
    ("settings.nav.about", "关于"),
    ("settings.nav.appearance", "外观"),
    ("settings.nav.daily_notes", "日记"),
    ("settings.nav.instances", "实例"),
    ("settings.nav.language", "语言"),
    ("settings.nav.plugins", "插件"),
    ("settings.nav.relay", "Relay"),
    ("settings.nav.security", "安全"),
    ("settings.nav.sync", "同步"),
    ("settings.nav.templates", "模板"),
    ("settings.nav.title", "设置"),
    // ---- settings.appearance ----
    ("settings.appearance.current", "当前：{mode}（解析为 {theme}）"),
    ("settings.appearance.dark", "深色"),
    ("settings.appearance.hint", "本机偏好；不同步、不写正文；切换立即生效"),
    ("settings.appearance.light", "浅色"),
    ("settings.appearance.system", "跟随系统"),
    ("settings.appearance.title", "外观"),
    // ---- settings.language ----
    ("settings.language.current", "当前：{language}"),
    ("settings.language.current_system", "当前：{language}（跟随系统）"),
    ("settings.language.en", "English"),
    ("settings.language.hint", "本机偏好；不同步、不写正文；切换立即生效"),
    ("settings.language.system", "跟随系统"),
    ("settings.language.title", "语言"),
    ("settings.language.zh", "中文"),
    // ---- settings（同步区/按钮/错误提示）----
    ("settings.active_now", "当前默认"),
    ("settings.close", "关闭"),
    ("settings.close_without_save", "关闭（不保存 relay 修改）"),
    ("settings.copy", "复制"),
    ("settings.device_id", "device_id：{id}"),
    ("settings.error_none", "无错误"),
    (
        "settings.fingerprint.instance_closed",
        "该实例当前没有打开；打开后才能读取 durable pin",
    ),
    (
        "settings.fingerprint.instance_closed_detail",
        "该实例当前没有打开；打开后才能读取 durable pin（切换默认/保存模式只写配置，重启后生效）",
    ),
    (
        "settings.fingerprint.title",
        "安全指纹（公开、只读；不含 token/恢复码/主密钥）",
    ),
    (
        "settings.fingerprint.unopened",
        "尚未打开实例：无法读取 pin 指纹",
    ),
    ("settings.instance", "实例"),
    ("settings.instance_row", "{role} · 本机实例 {id}"),
    (
        "settings.instances.title",
        "实例（active 优先；点「设为默认」只写配置，重启后生效）",
    ),
    (
        "settings.no_active_hint",
        "当前没有 active 实例：列表里点「设为默认」后才能修改 relay（保存后重启生效）。",
    ),
    (
        "settings.no_active_save_error",
        "当前没有 active 实例：先在实例列表里点「设为默认」，再修改 relay 模式/地址",
    ),
    (
        "settings.no_instances_hint",
        "配置里还没有实例：先返回欢迎页创建或加入一个同步实例。",
    ),
    (
        "settings.notice.default",
        "relay 设置保存后需重启应用才生效；公开 id 可复制，秘密不要分享。",
    ),
    (
        "settings.plugins.daily_notes.description",
        "按日期格式创建/打开今日日记，可用模板渲染正文。",
    ),
    ("settings.plugins.disabled", "已停用"),
    ("settings.plugins.enabled", "已启用"),
    (
        "settings.plugins.hint",
        "关闭后立即隐藏对应设置分类与侧栏入口；开关保存在本机偏好，不同步、不写正文",
    ),
    (
        "settings.plugins.templates.description",
        "在普通文件夹里管理可复用正文，支持插入模板与从模板新建下一条笔记。",
    ),
    ("settings.plugins.title", "核心插件"),
    (
        "settings.public.title",
        "公开信息（可复制；不要分享 setup secret、恢复码或主密钥）",
    ),
    (
        "settings.relay.local_hint",
        "local_dev：只允许 http://localhost / 127.0.0.1 / [::1]；空值/非 loopback/https/带 userinfo/带路径一律内联报错且不写配置。",
    ),
    (
        "settings.relay.remote_hint",
        "remote_tls：只允许 https + canonical origin（DNS/IP；拒绝 userinfo/query/fragment/非根路径）；必须勾选上面的确认。",
    ),
    (
        "settings.relay.title",
        "relay 模式与地址（只对 active 实例；保存后重启生效）",
    ),
    (
        "settings.remote_consent",
        "我确认要连接远程服务器（HTTPS）；确认端点 ≠ 授权接受服务器任意 hash",
    ),
    (
        "settings.remote_tls_consent_required",
        "切换到 remote_tls 需要显式勾选「我确认要连接远程服务器（HTTPS）」；确认不会清 pin、也不会自动信任新服务器",
    ),
    ("settings.save", "保存 relay 设置（重启后生效）"),
    ("settings.set_active", "设为默认"),
    (
        "settings.sync.dev_banner",
        "DEV ONLY · 开发同步：relay 只允许 localhost，服务端验签放行，非生产、非端到端安全验收",
    ),
    ("settings.sync.panel_title", "同步面板"),
    ("settings.sync.title", "设置 · 同步"),
    (
        "settings.sync.unavailable",
        "当前没有打开的实例：同步面板只对已打开的实例可用。",
    ),
    ("settings.vault_id", "vault_id：{id}"),
    ("settings.vault_path", "库文件：{path}"),
    // ---- sidebar ----
    ("sidebar.backlinks.one", "反向链接 {count}"),
    ("sidebar.backlinks.other", "反向链接 {count}"),
    ("sidebar.backlinks_empty", "没有反向链接"),
    ("sidebar.export", "导出"),
    ("sidebar.exporting", "导出中…"),
    ("sidebar.graph", "图谱"),
    ("sidebar.new_drawing", "画布"),
    ("sidebar.new_folder", "＋ 文件夹"),
    ("sidebar.new_note", "＋ 笔记"),
    ("sidebar.notes", "笔记"),
    ("sidebar.settings", "⚙ 设置"),
    // ---- sidebar（排序 / 最左侧图标栏）----
    ("sidebar.ribbon.daily", "今日日记"),
    ("sidebar.ribbon.graph", "图谱"),
    ("sidebar.ribbon.search", "搜索（Ctrl+K）"),
    ("sidebar.ribbon.template", "从模板新建"),
    ("sidebar.sort.created_newest", "创建时间 新→旧"),
    ("sidebar.sort.created_oldest", "创建时间 旧→新"),
    ("sidebar.sort.name_asc", "文件名 A-Z"),
    ("sidebar.sort.name_desc", "文件名 Z-A"),
    ("sidebar.sort.title", "排序"),
    ("sidebar.sort.updated_newest", "修改时间 新→旧"),
    ("sidebar.sort.updated_oldest", "修改时间 旧→新"),
    // ---- editor.menu ----
    ("editor.menu.copy_link", "复制链接"),
    ("editor.menu.copy_markdown", "复制 Markdown"),
    ("editor.menu.insert", "插入"),
    ("editor.menu.open_link", "打开链接"),
    ("editor.menu.table", "表格"),
    // ---- drawing（画布笔记 / Excalidraw 覆盖层）----
    ("drawing.bar_title", "画布 · {title}"),
    ("drawing.close", "关闭（不保存）"),
    ("drawing.save", "保存并关闭"),
    ("drawing.save_failed", "画布保存失败：{reason}"),
    ("drawing.saved", "画布已保存"),
    ("drawing.placeholder", "画布笔记"),
    ("drawing.placeholder_hint", "画布内容已隐藏；点「打开画布」继续编辑"),
    ("drawing.open", "打开画布"),
    // ---- properties ----
    ("properties.title", "笔记属性"),
    ("properties.add", "添加笔记属性"),
    ("properties.no_value", "没有值"),
    // ---- status ----
    ("status.backlinks.one", "反向链接 {count}"),
    ("status.backlinks.other", "反向链接 {count}"),
    ("status.sync", "同步：{status}"),
    ("status.sync_attention", "同步：需处理"),
    // ---- templates ----
    ("templates.title", "模板"),
    (
        "templates.hint",
        "模板就是普通笔记：不建独立表、不隐藏文件夹；本机设置不同步、不写正文。相对日期：{{date:-1d}}（昨天）、{{date:+1w}}（7 天后）、{{date:-1m:YYYY/MM/DD}}（上个月同一天，自定义格式）",
    ),
    ("templates.disabled_hint", "模板插件未启用（在设置页「插件」里开启）"),
    ("templates.folder_label", "模板文件夹（普通目录，支持多级）"),
    ("templates.date_format_label", "日期格式"),
    ("templates.time_format_label", "时间格式"),
    ("templates.example", "示例：{example}（当前设置）"),
    ("templates.saved", "模板设置已保存"),
    ("templates.rejected", "字段不能为空，未保存"),
    ("templates.insert_menu", "模板…"),
    ("templates.new_from_template", "从模板新建…"),
    ("templates.picker_title_insert", "插入模板"),
    ("templates.picker_title_new", "从模板新建"),
    (
        "templates.picker_hint",
        "输入过滤 · ↑/↓ 选择 · Enter 确认 · Esc 关闭 · 点击插入",
    ),
    ("templates.picker_search", "搜索模板…"),
    (
        "templates.empty_missing",
        "模板文件夹「{folder}」不存在（不会自动创建）",
    ),
    ("templates.empty_folder", "模板文件夹「{folder}」里还没有笔记"),
    ("templates.empty_no_match", "没有匹配的模板"),
    ("templates.empty_no_folder", "未配置模板文件夹（设置页「模板」）"),
    ("templates.empty_no_note", "先打开一篇笔记才能插入模板"),
    ("templates.inserted", "已插入模板「{name}」"),
    ("templates.created", "已从模板新建「{title}」"),
    (
        "templates.insert_failed",
        "当前无法插入模板（同步持闸/输入法组合/单元格编辑中，或没有打开的笔记）",
    ),
    ("templates.empty_body", "模板正文为空，未插入"),
    (
        "templates.write_failed",
        "新建失败（结构写入被拒绝或出错，详见 stderr）",
    ),
    (
        "templates.created_insert_failed",
        "已从模板新建「{title}」，但正文写入失败（笔记为空）",
    ),
    ("templates.read_failed", "读取模板正文失败：{reason}"),
    // ---- daily_notes ----
    ("daily_notes.title", "日记"),
    (
        "daily_notes.hint",
        "本机设置；不同步、不写正文；日期格式里的 “/” 会生成子目录。相对日期：{{date:-1d}}（昨天）、{{date:+1d}}（明天），单位 d/w/m/y",
    ),
    ("daily_notes.disabled_hint", "日记插件未启用（在设置页「插件」里开启）"),
    ("daily_notes.folder_label", "日记文件夹（新笔记位置）"),
    ("daily_notes.date_format_label", "日期格式（“/” 生成子目录）"),
    ("daily_notes.template_label", "日记模板路径（可选）"),
    (
        "daily_notes.template_hint",
        "留空 = 无模板；可以是任意普通笔记路径，如 模板/日记模板",
    ),
    ("daily_notes.example", "今天的日记：{path}"),
    ("daily_notes.saved", "日记设置已保存"),
    ("daily_notes.rejected", "字段不能为空，未保存"),
    ("daily_notes.open_today", "今日日记"),
    ("daily_notes.opened", "已打开今日日记「{path}」"),
    ("daily_notes.created", "已创建今日日记「{path}」"),
    ("daily_notes.create_failed", "创建今日日记失败：{reason}"),
    (
        "daily_notes.template_missing",
        "日记模板「{path}」不存在：已创建空日记（可在设置页修正路径）",
    ),
    (
        "daily_notes.invalid_path",
        "日期格式没有生成有效笔记名（检查「日记」设置）",
    ),
    (
        "daily_notes.out_of_range",
        "相对日期偏移超出范围（检查日历/日期设置）",
    ),
    (
        "daily_notes.write_failed",
        "结构写入被拒绝或出错（详见 stderr）",
    ),
    ("daily_notes.pick_template", "选择模板笔记…"),
    ("daily_notes.template_picker_title", "选择日记模板"),
    (
        "daily_notes.template_picker_hint",
        "↑/↓ 选择 · Enter 确认 · Esc 关闭",
    ),
    ("daily_notes.template_picker_search", "搜索"),
    ("daily_notes.template_picker_current", "当前"),
    (
        "daily_notes.template_picker_empty_library",
        "库中还没有笔记；先新建一篇笔记，再选择日记模板",
    ),
    (
        "daily_notes.template_picker_empty_match",
        "没有匹配的笔记",
    ),
    // ---- local（仅本地使用，不同步）----
    (
        "local.action",
        "仅本地使用（不同步）",
    ),
    (
        "local.dev_action",
        "继续本地（DEV ONLY，无同步）",
    ),
    (
        "local.dev_hint",
        "「继续本地」打开默认 dev vault（target/tmp/menghunote-dev）；它不联网、没有同步面板，仅供开发。",
    ),
    (
        "local.dev_open_failed_title",
        "无法打开本地 dev vault",
    ),
    ("local.dir_label", "目录"),
    ("local.mode_notice", "本地模式：不同步"),
    ("local.open_failed_title", "无法打开本地笔记库"),
    ("local.path_hint", "本地库：{path}"),
    (
        "local.resume_missing",
        "上次使用的本地笔记库不存在：{path}；已回到欢迎页（不会自动新建库）",
    ),
    (
        "local.welcome_hint",
        "在本机打开一个独立笔记库：不联网、不创建同步实例、不参与同步；空库由你自己新建笔记。",
    ),
    ("local.welcome_title", "没有同步需求？"),
];

/// 英文 catalog。
pub const EN: &[(&str, &str)] = &[
    // ---- settings.nav（settings page category navigation）----
    ("settings.about.build", "Build: {build}"),
    (
        "settings.about.hint",
        "Version comes from the Cargo manifest; build features are shown read-only and change no behavior",
    ),
    ("settings.about.title", "About"),
    (
        "settings.about.version",
        "menghunote {version} (GPUI Kit 0.9.1; single SQLite database is the source of truth)",
    ),
    ("settings.nav.about", "About"),
    ("settings.nav.appearance", "Appearance"),
    ("settings.nav.daily_notes", "Daily notes"),
    ("settings.nav.instances", "Instances"),
    ("settings.nav.language", "Language"),
    ("settings.nav.plugins", "Plugins"),
    ("settings.nav.relay", "Relay"),
    ("settings.nav.security", "Security"),
    ("settings.nav.sync", "Sync"),
    ("settings.nav.templates", "Templates"),
    ("settings.nav.title", "Settings"),
    // ---- settings.appearance ----
    (
        "settings.appearance.current",
        "Current: {mode} (resolved: {theme})",
    ),
    ("settings.appearance.dark", "Dark"),
    (
        "settings.appearance.hint",
        "Local preference; not synced and not written to notes; applies immediately",
    ),
    ("settings.appearance.light", "Light"),
    ("settings.appearance.system", "Follow system"),
    ("settings.appearance.title", "Appearance"),
    // ---- settings.language ----
    ("settings.language.current", "Current: {language}"),
    (
        "settings.language.current_system",
        "Current: {language} (follow system)",
    ),
    ("settings.language.en", "English"),
    (
        "settings.language.hint",
        "Local preference; not synced and not written to notes; applies immediately",
    ),
    ("settings.language.system", "Follow system"),
    ("settings.language.title", "Language"),
    ("settings.language.zh", "Chinese"),
    // ---- settings（同步区/按钮/错误提示）----
    ("settings.active_now", "Active"),
    ("settings.close", "Close"),
    ("settings.close_without_save", "Close (discard relay changes)"),
    ("settings.copy", "Copy"),
    ("settings.device_id", "device_id: {id}"),
    ("settings.error_none", "No errors"),
    (
        "settings.fingerprint.instance_closed",
        "This instance is not open; the durable pin can be read only after opening it",
    ),
    (
        "settings.fingerprint.instance_closed_detail",
        "This instance is not open; the durable pin can be read only after opening it (changing the active instance or saving the mode only writes config and takes effect after restart)",
    ),
    (
        "settings.fingerprint.title",
        "Security fingerprint (public, read-only; no tokens, recovery codes or master keys)",
    ),
    (
        "settings.fingerprint.unopened",
        "No instance opened yet: the pin fingerprint is unavailable",
    ),
    ("settings.instance", "Instance"),
    ("settings.instance_row", "{role} · local instance {id}"),
    (
        "settings.instances.title",
        "Instances (active first; Set as active only writes config and takes effect after restart)",
    ),
    (
        "settings.no_active_hint",
        "No active instance: choose Set as active in the list before editing the relay (takes effect after restart).",
    ),
    (
        "settings.no_active_save_error",
        "No active instance: choose Set as active in the list before editing the relay mode/address",
    ),
    (
        "settings.no_instances_hint",
        "No instances in the config yet: return to the welcome page to create or join a sync instance.",
    ),
    (
        "settings.notice.default",
        "Relay changes take effect after restarting the app; public ids can be copied, secrets must not be shared.",
    ),
    (
        "settings.plugins.daily_notes.description",
        "Create or open today's daily note from a date format, optionally rendering a template.",
    ),
    ("settings.plugins.disabled", "Disabled"),
    ("settings.plugins.enabled", "Enabled"),
    (
        "settings.plugins.hint",
        "Disabling immediately hides the settings category and sidebar entry; toggles are local preferences, not synced and never written to notes",
    ),
    (
        "settings.plugins.templates.description",
        "Manage reusable note bodies in an ordinary folder and insert them or start a new note from one.",
    ),
    ("settings.plugins.title", "Core plugins"),
    (
        "settings.public.title",
        "Public information (copyable; never share setup secrets, recovery codes or the master key)",
    ),
    (
        "settings.relay.local_hint",
        "local_dev: only http://localhost / 127.0.0.1 / [::1]; empty, non-loopback, https, userinfo or path values fail inline and never modify the config.",
    ),
    (
        "settings.relay.remote_hint",
        "remote_tls: only https + canonical origin (DNS/IP; userinfo/query/fragment/non-root paths rejected); the consent above is required.",
    ),
    (
        "settings.relay.title",
        "Relay mode and address (active instance only; takes effect after restart)",
    ),
    (
        "settings.remote_consent",
        "I confirm connecting to a remote server (HTTPS); confirming the endpoint does not accept any server hash",
    ),
    (
        "settings.remote_tls_consent_required",
        "Switching to remote_tls requires the explicit \"remote server (HTTPS)\" consent; it never clears the pin and never auto-trusts a new server",
    ),
    (
        "settings.save",
        "Save relay settings (takes effect after restart)",
    ),
    ("settings.set_active", "Set as active"),
    (
        "settings.sync.dev_banner",
        "DEV ONLY · dev sync: relay only allows localhost, server signature checks are bypassed; not production, not end-to-end security acceptance",
    ),
    ("settings.sync.panel_title", "Sync panel"),
    ("settings.sync.title", "Settings · Sync"),
    (
        "settings.sync.unavailable",
        "No open instance: the sync panel is only available for an opened instance.",
    ),
    ("settings.vault_id", "vault_id: {id}"),
    ("settings.vault_path", "Vault file: {path}"),
    // ---- sidebar ----
    ("sidebar.backlinks.one", "{count} backlink"),
    ("sidebar.backlinks.other", "{count} backlinks"),
    ("sidebar.backlinks_empty", "No backlinks"),
    ("sidebar.export", "Export"),
    ("sidebar.exporting", "Exporting…"),
    ("sidebar.graph", "Graph"),
    ("sidebar.new_drawing", "Canvas"),
    ("sidebar.new_folder", "+ Folder"),
    ("sidebar.new_note", "+ Note"),
    ("sidebar.notes", "Notes"),
    ("sidebar.settings", "⚙ Settings"),
    // ---- sidebar（sorting / leftmost ribbon）----
    ("sidebar.ribbon.daily", "Today's daily note"),
    ("sidebar.ribbon.graph", "Graph"),
    ("sidebar.ribbon.search", "Search (Ctrl+K)"),
    ("sidebar.ribbon.template", "New from template"),
    ("sidebar.sort.created_newest", "Created (newest first)"),
    ("sidebar.sort.created_oldest", "Created (oldest first)"),
    ("sidebar.sort.name_asc", "File name A-Z"),
    ("sidebar.sort.name_desc", "File name Z-A"),
    ("sidebar.sort.title", "Sort"),
    ("sidebar.sort.updated_newest", "Modified (newest first)"),
    ("sidebar.sort.updated_oldest", "Modified (oldest first)"),
    // ---- editor.menu ----
    ("editor.menu.copy_link", "Copy link"),
    ("editor.menu.copy_markdown", "Copy Markdown"),
    ("editor.menu.insert", "Insert"),
    ("editor.menu.open_link", "Open link"),
    ("editor.menu.table", "Table"),
    // ---- drawing（canvas notes / Excalidraw overlay）----
    ("drawing.bar_title", "Canvas · {title}"),
    ("drawing.close", "Close without saving"),
    ("drawing.save", "Save & close"),
    ("drawing.save_failed", "Saving the canvas failed: {reason}"),
    ("drawing.saved", "Canvas saved"),
    ("drawing.placeholder", "Canvas note"),
    ("drawing.placeholder_hint", "The canvas is hidden; choose Open canvas to continue."),
    ("drawing.open", "Open canvas"),
    // ---- properties ----
    ("properties.title", "Note properties"),
    ("properties.add", "Add property"),
    ("properties.no_value", "No value"),
    // ---- status ----
    ("status.backlinks.one", "{count} backlink"),
    ("status.backlinks.other", "{count} backlinks"),
    ("status.sync", "Sync: {status}"),
    ("status.sync_attention", "Sync: action needed"),
    // ---- templates ----
    ("templates.title", "Templates"),
    (
        "templates.hint",
        "Templates are ordinary notes: no separate table, no hidden folder; local settings are not synced and never written into note bodies. Relative dates: {{date:-1d}} (yesterday), {{date:+1w}} (7 days later), {{date:-1m:YYYY/MM/DD}} (same day last month, custom format)",
    ),
    (
        "templates.disabled_hint",
        "Templates are disabled (enable them in Settings → Templates)",
    ),
    (
        "templates.folder_label",
        "Template folder (ordinary folder, multiple levels allowed)",
    ),
    ("templates.date_format_label", "Date format"),
    ("templates.time_format_label", "Time format"),
    ("templates.example", "Example: {example} (current settings)"),
    ("templates.saved", "Template settings saved"),
    ("templates.rejected", "Field must not be empty; not saved"),
    ("templates.insert_menu", "Template…"),
    ("templates.new_from_template", "New from template…"),
    ("templates.picker_title_insert", "Insert template"),
    ("templates.picker_title_new", "New from template"),
    (
        "templates.picker_hint",
        "Type to filter · ↑/↓ to move · Enter to confirm · Esc to close · click to insert",
    ),
    ("templates.picker_search", "Search templates…"),
    (
        "templates.empty_missing",
        "Template folder \"{folder}\" does not exist (never created automatically)",
    ),
    (
        "templates.empty_folder",
        "Template folder \"{folder}\" has no notes yet",
    ),
    ("templates.empty_no_match", "No matching templates"),
    (
        "templates.empty_no_folder",
        "No template folder configured (Settings → Templates)",
    ),
    (
        "templates.empty_no_note",
        "Open a note before inserting a template",
    ),
    ("templates.inserted", "Inserted template \"{name}\""),
    ("templates.created", "Created \"{title}\" from a template"),
    (
        "templates.insert_failed",
        "Cannot insert a template now (sync gate / IME composition / cell editing, or no note is open)",
    ),
    ("templates.empty_body", "The template body is empty; nothing was inserted"),
    (
        "templates.write_failed",
        "Failed to create the note (structural write rejected or failed; see stderr)",
    ),
    (
        "templates.created_insert_failed",
        "Created \"{title}\" from a template, but writing the body failed (the note is empty)",
    ),
    (
        "templates.read_failed",
        "Failed to read the template body: {reason}",
    ),
    // ---- daily_notes ----
    ("daily_notes.title", "Daily notes"),
    (
        "daily_notes.hint",
        "Local settings; not synced and never written into note bodies; \"/\" in the date format creates subfolders. Relative dates: {{date:-1d}} (yesterday), {{date:+1d}} (tomorrow); units d/w/m/y",
    ),
    (
        "daily_notes.disabled_hint",
        "Daily notes are disabled (enable them in Settings → Daily notes)",
    ),
    (
        "daily_notes.folder_label",
        "Daily note folder (location of new notes)",
    ),
    (
        "daily_notes.date_format_label",
        "Date format (\"/\" creates subfolders)",
    ),
    ("daily_notes.template_label", "Daily template path (optional)"),
    (
        "daily_notes.template_hint",
        "Empty = no template; any ordinary note path works, e.g. Templates/Daily-template",
    ),
    ("daily_notes.example", "Today's daily note: {path}"),
    ("daily_notes.saved", "Daily note settings saved"),
    ("daily_notes.rejected", "Field must not be empty; not saved"),
    ("daily_notes.open_today", "Today"),
    ("daily_notes.opened", "Opened today's daily note \"{path}\""),
    ("daily_notes.created", "Created today's daily note \"{path}\""),
    (
        "daily_notes.create_failed",
        "Failed to create today's daily note: {reason}",
    ),
    (
        "daily_notes.template_missing",
        "Daily template \"{path}\" does not exist: created an empty daily note (fix the path in Settings)",
    ),
    (
        "daily_notes.invalid_path",
        "The date format produced no valid note name (check Settings → Daily notes)",
    ),
    (
        "daily_notes.out_of_range",
        "The relative date offset is out of range (check the calendar/date settings)",
    ),
    (
        "daily_notes.write_failed",
        "Structural write rejected or failed (see stderr)",
    ),
    ("daily_notes.pick_template", "Choose template note…"),
    (
        "daily_notes.template_picker_title",
        "Choose daily template",
    ),
    (
        "daily_notes.template_picker_hint",
        "↑/↓ to move · Enter to choose · Esc to close",
    ),
    ("daily_notes.template_picker_search", "Search"),
    ("daily_notes.template_picker_current", "Current"),
    (
        "daily_notes.template_picker_empty_library",
        "No notes in the library yet; create a note first, then choose a daily template",
    ),
    (
        "daily_notes.template_picker_empty_match",
        "No matching notes",
    ),
    // ---- local（local-only, no sync）----
    ("local.action", "Use locally only (no sync)"),
    (
        "local.dev_action",
        "Continue locally (DEV ONLY, no sync)",
    ),
    (
        "local.dev_hint",
        "\"Continue locally\" opens the default dev vault (target/tmp/menghunote-dev); no network, no sync panel, development only.",
    ),
    (
        "local.dev_open_failed_title",
        "Unable to open the local dev vault",
    ),
    ("local.dir_label", "Directory"),
    ("local.mode_notice", "Local mode: not synced"),
    (
        "local.open_failed_title",
        "Unable to open the local vault",
    ),
    ("local.path_hint", "Local vault: {path}"),
    (
        "local.resume_missing",
        "The local vault from last time is missing: {path}; returned to Welcome (no vault was created)",
    ),
    (
        "local.welcome_hint",
        "Open a standalone notebook vault on this machine: no network, no sync instance, no syncing; the empty vault is yours to fill.",
    ),
    ("local.welcome_title", "Don't need sync?"),
];

#[must_use]
pub const fn entries(locale: Locale) -> &'static [(&'static str, &'static str)] {
    match locale {
        Locale::ZhCn => ZH_CN,
        Locale::En => EN,
    }
}

/// 在**指定语言**里查 key（不做回退；回退在 [`crate::t`]）。
#[must_use]
pub fn lookup(locale: Locale, id: &str) -> Option<&'static str> {
    entries(locale)
        .iter()
        .find_map(|(key, text)| (*key == id).then_some(*text))
}

/// 指定语言是否包含该 key（缺 key/未翻译检测用）。
#[must_use]
pub fn has_key(locale: Locale, id: &str) -> bool {
    lookup(locale, id).is_some()
}

/// 某个 catalog 的全部 key（测试/诊断用）。
pub fn catalog_keys(locale: Locale) -> impl Iterator<Item = &'static str> {
    entries(locale).iter().map(|(key, _)| *key)
}

/// catalog 规模（状态/测试短报告用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CatalogStats {
    /// 中文 key 数。
    pub zh_cn: usize,
    /// 英文 key 数。
    pub en: usize,
}

/// 两个语言的 catalog 规模。
#[must_use]
pub fn stats() -> CatalogStats {
    CatalogStats {
        zh_cn: ZH_CN.len(),
        en: EN.len(),
    }
}

/// 缺翻译清单：`(语言, 只在另一边存在的 key)`。
///
/// 两个 catalog 完全一致时返回空；测试直接断言为空。运行时**不**调用它
/// （避免绘制路径分配），只在测试/开发期诊断用。
#[must_use]
pub fn missing_translations() -> Vec<(Locale, &'static str)> {
    let mut missing = Vec::new();
    for locale in Locale::ALL {
        let other = match locale {
            Locale::ZhCn => Locale::En,
            Locale::En => Locale::ZhCn,
        };
        for key in catalog_keys(other) {
            if !has_key(locale, key) {
                missing.push((locale, key));
            }
        }
    }
    missing
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// 两个 catalog 的 key 集合必须一致，且等于 `msg::ALL`。
    #[test]
    fn catalogs_have_identical_key_sets_matching_message_ids() {
        let zh: HashSet<&str> = catalog_keys(Locale::ZhCn).collect();
        let en: HashSet<&str> = catalog_keys(Locale::En).collect();
        let declared: HashSet<&str> = crate::msg::ALL.iter().copied().collect();

        assert!(
            missing_translations().is_empty(),
            "缺翻译：{:?}",
            missing_translations()
        );
        assert_eq!(zh, en, "中英 catalog 的 key 集合必须一致");
        assert_eq!(zh, declared, "catalog key 必须与 msg 常量一一对应");
        assert_eq!(stats().zh_cn, stats().en, "两个 catalog 的条目数必须一致");
    }

    /// 没有重复 key、没有空文案。
    #[test]
    fn catalogs_have_no_duplicates_or_empty_values() {
        for locale in Locale::ALL {
            let mut seen = HashSet::new();
            for (key, text) in entries(locale) {
                assert!(seen.insert(*key), "{locale:?} 重复 key：{key}");
                assert!(!text.trim().is_empty(), "{locale:?} 的 {key} 文案为空");
            }
        }
    }

    /// 占位符必须成对、只允许已知名字（防止模板拼写错误静默留在 UI 上）。
    #[test]
    fn placeholders_are_balanced_and_known() {
        const ALLOWED: [&str; 16] = [
            "count", "mode", "theme", "language", "status", "role", "id", "path", "folder", "name",
            "title", "example", "reason", "format", "build", "version",
        ];
        for locale in Locale::ALL {
            for (key, text) in entries(locale) {
                let mut rest = *text;
                while let Some(start) = rest.find('{') {
                    let after = &rest[start + 1..];
                    // `{{...}}` 是**字面量**（模板/日记占位符语法，直接展示给用户），
                    // 不是 `{name}` 插值：整段跳过，不参与 ALLOWED 校验。
                    if after.starts_with('{') {
                        let close = after[1..].find("}}").unwrap_or_else(|| {
                            panic!("{locale:?} 的 {key} 字面量花括号未闭合：{text}")
                        });
                        rest = &after[1 + close + 2..];
                        continue;
                    }
                    let end = after
                        .find('}')
                        .unwrap_or_else(|| panic!("{locale:?} 的 {key} 占位符未闭合：{text}"));
                    let name = &after[..end];
                    assert!(
                        ALLOWED.contains(&name),
                        "{locale:?} 的 {key} 含未知占位符 {{{name}}}"
                    );
                    rest = &after[end + 1..];
                }
                assert!(
                    !rest.contains('}'),
                    "{locale:?} 的 {key} 有多余的 '}}'：{text}"
                );
            }
        }
    }

    #[test]
    fn lookups_return_translated_text_and_none_for_unknown_ids() {
        assert_eq!(
            lookup(Locale::ZhCn, crate::msg::sidebar::NOTES),
            Some("笔记")
        );
        assert_eq!(
            lookup(Locale::En, crate::msg::sidebar::NOTES),
            Some("Notes")
        );
        assert_eq!(lookup(Locale::En, "sidebar.missing"), None);
        assert!(has_key(Locale::En, crate::msg::settings::SYNC_TITLE));
        assert!(!has_key(Locale::ZhCn, "status.missing"));
    }
}
