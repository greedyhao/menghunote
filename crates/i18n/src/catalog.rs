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
    ("settings.close_without_save", "关闭（不保存）"),
    ("settings.collapse", "收起"),
    ("settings.copy", "复制"),
    ("settings.device_id", "device_id：{id}"),
    ("settings.error_none", "无错误"),
    ("settings.expand", "展开"),
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
        "修改保存后需重启应用才生效；公开 id 可复制，秘密不要分享。",
    ),
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
    ("settings.save", "保存模式/relay（重启后生效）"),
    ("settings.set_active", "设为默认"),
    (
        "settings.sync.dev_banner",
        "DEV ONLY · 开发同步：relay 只允许 localhost，服务端验签放行，非生产、非端到端安全验收",
    ),
    ("settings.sync.dev_title", "同步设置（DEV ONLY）"),
    (
        "settings.sync.panel_collapsed",
        "同步面板（默认收起，展开后查看设备/邀请/轮换）",
    ),
    ("settings.sync.panel_expanded", "同步面板（已展开）"),
    ("settings.sync.title", "设置 · 同步"),
    ("settings.vault_id", "vault_id：{id}"),
    ("settings.vault_path", "库文件：{path}"),
    // ---- sidebar ----
    ("sidebar.backlinks.one", "反向链接 {count}"),
    ("sidebar.backlinks.other", "反向链接 {count}"),
    ("sidebar.backlinks_empty", "没有反向链接"),
    ("sidebar.export", "导出"),
    ("sidebar.exporting", "导出中…"),
    ("sidebar.graph", "图谱"),
    ("sidebar.new_folder", "＋ 文件夹"),
    ("sidebar.new_note", "＋ 笔记"),
    ("sidebar.notes", "笔记"),
    ("sidebar.settings", "⚙ 设置"),
    // ---- editor.menu ----
    ("editor.menu.copy_link", "复制链接"),
    ("editor.menu.copy_markdown", "复制 Markdown"),
    ("editor.menu.insert", "插入"),
    ("editor.menu.open_link", "打开链接"),
    ("editor.menu.table", "表格"),
    // ---- status ----
    ("status.backlinks.one", "反向链接 {count}"),
    ("status.backlinks.other", "反向链接 {count}"),
    ("status.sync", "同步：{status}"),
    ("status.sync_attention", "同步：需处理"),
];

/// 英文 catalog。
pub const EN: &[(&str, &str)] = &[
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
    ("settings.close_without_save", "Close (without saving)"),
    ("settings.collapse", "Collapse"),
    ("settings.copy", "Copy"),
    ("settings.device_id", "device_id: {id}"),
    ("settings.error_none", "No errors"),
    ("settings.expand", "Expand"),
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
        "Changes take effect after restarting the app; public ids can be copied, secrets must not be shared.",
    ),
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
        "Save mode/relay (takes effect after restart)",
    ),
    ("settings.set_active", "Set as active"),
    (
        "settings.sync.dev_banner",
        "DEV ONLY · dev sync: relay only allows localhost, server signature checks are bypassed; not production, not end-to-end security acceptance",
    ),
    ("settings.sync.dev_title", "Sync settings (DEV ONLY)"),
    (
        "settings.sync.panel_collapsed",
        "Sync panel (collapsed by default; expand for devices, invites and rotation)",
    ),
    ("settings.sync.panel_expanded", "Sync panel (expanded)"),
    ("settings.sync.title", "Settings · Sync"),
    ("settings.vault_id", "vault_id: {id}"),
    ("settings.vault_path", "Vault file: {path}"),
    // ---- sidebar ----
    ("sidebar.backlinks.one", "{count} backlink"),
    ("sidebar.backlinks.other", "{count} backlinks"),
    ("sidebar.backlinks_empty", "No backlinks"),
    ("sidebar.export", "Export"),
    ("sidebar.exporting", "Exporting…"),
    ("sidebar.graph", "Graph"),
    ("sidebar.new_folder", "+ Folder"),
    ("sidebar.new_note", "+ Note"),
    ("sidebar.notes", "Notes"),
    ("sidebar.settings", "⚙ Settings"),
    // ---- editor.menu ----
    ("editor.menu.copy_link", "Copy link"),
    ("editor.menu.copy_markdown", "Copy Markdown"),
    ("editor.menu.insert", "Insert"),
    ("editor.menu.open_link", "Open link"),
    ("editor.menu.table", "Table"),
    // ---- status ----
    ("status.backlinks.one", "{count} backlink"),
    ("status.backlinks.other", "{count} backlinks"),
    ("status.sync", "Sync: {status}"),
    ("status.sync_attention", "Sync: action needed"),
];

/// 某个语言的 catalog。
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
        const ALLOWED: [&str; 8] = [
            "count", "mode", "theme", "language", "status", "role", "id", "path",
        ];
        for locale in Locale::ALL {
            for (key, text) in entries(locale) {
                let mut rest = *text;
                while let Some(start) = rest.find('{') {
                    let after = &rest[start + 1..];
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
