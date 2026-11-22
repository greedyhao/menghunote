//! Markdown 单向导出（S3-1）。
//!
//! ## 产品取舍
//!
//! SQLite 是**持久化真源**，Markdown 只是**单向导出**（ADR 0001）：不做导入、不监听
//! 外部编辑、不把文件当成第二份真源。因此这里只负责"把树 + 正文写成文件"，写完就
//! 不管了。
//!
//! ## 为什么导出正文而不加标题行
//!
//! 导出文件名叫 `<标题>.md`，正文按 `BodyDoc::text()` **原样**写出。**不**自动在正文
//! 前面补一行 `# 标题`：很多笔记正文自己就有 `#` 标题，自动补会造成重复标题；用文件名
//! 表达标题已经足够（Obsidian/Typora 的惯例也是这样）。
//!
//! ## 数据来源必须是源文
//!
//! 正文一律取 `BodyDoc::text()`（CRDT 源文）。**不要**用 UI 的 display 文本——那是投影
//! 之后的结果（隐藏标记已消失），导出出去就成了"改过的"内容。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Context;
use menghunote_storage::{list_tree, load_body_doc, NodeKind, TreeEntry, Vault};

/// Windows 文件名里非法的字符（ANSI + 控制字符）。
const ILLEGAL_CHARS: &[char] = &['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

/// 标题为空时的兜底文件名（不含扩展名）。
pub const UNTITLED: &str = "未命名";

/// 把标题清洗成**安全的文件/目录名**（不含扩展名）。
///
/// 规则：
/// - Windows 非法字符与控制字符 → `_`；
/// - 去掉尾部空格与点（Windows 会静默丢弃，导致"文件找不到"）；
/// - 清洗后为空 / 全是空白 → [`UNTITLED`]。
#[must_use]
pub fn sanitize_name(title: &str) -> String {
    let mut cleaned: String = title
        .chars()
        .map(|ch| {
            if ILLEGAL_CHARS.contains(&ch) || ch.is_control() {
                '_'
            } else {
                ch
            }
        })
        .collect();
    cleaned = cleaned.trim_end_matches([' ', '.']).to_owned();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        return UNTITLED.to_owned();
    }
    // 只保留"去掉首尾空白"的主体（内部空格保留）。
    trimmed.to_owned()
}

/// 大小写不敏感的重名登记表（Windows 上 `A.md` 与 `a.md` 是同一个文件）。
#[derive(Default)]
pub struct NameRegistry {
    used: HashMap<String, u32>,
}

impl NameRegistry {
    /// 登记一个 `.md` 文件的主名（不含扩展名），重名时把 ` (2)` 加在**扩展名之前**。
    pub fn insert_markdown(&mut self, stem: &str) -> String {
        self.insert_inner(stem, ".md")
    }

    /// 通用版：`extension` 为空表示目录名。
    fn insert_inner(&mut self, stem: &str, extension: &str) -> String {
        let desired = format!("{stem}{extension}");
        let key = desired.to_lowercase();
        let count = self.used.entry(key.clone()).or_insert(0);
        *count += 1;
        if *count == 1 {
            return stem.to_owned();
        }
        let mut index = *count;
        loop {
            let candidate = format!("{stem} ({index}){extension}");
            let candidate_key = candidate.to_lowercase();
            if let std::collections::hash_map::Entry::Vacant(slot) = self.used.entry(candidate_key)
            {
                slot.insert(1);
                return format!("{stem} ({index})");
            }
            index += 1;
        }
    }

    /// 登记一个期望名字，返回**可直接使用**的名字（必要时加 ` (2)`、` (3)`…）。
    pub fn insert(&mut self, desired: &str) -> String {
        let key = desired.to_lowercase();
        let count = self.used.entry(key.clone()).or_insert(0);
        *count += 1;
        if *count == 1 {
            return desired.to_owned();
        }
        // 冲突：从 2 开始试，直到大小写不敏感地唯一。
        let mut index = *count;
        loop {
            let candidate = format!("{desired} ({index})");
            let candidate_key = candidate.to_lowercase();
            if let std::collections::hash_map::Entry::Vacant(slot) = self.used.entry(candidate_key)
            {
                slot.insert(1);
                return candidate;
            }
            index += 1;
        }
    }
}

/// 一篇待写出的笔记（相对导出根的路径 + 正文）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportItem {
    /// 相对导出根的路径（`目录/子目录/标题.md`）。
    pub relative: PathBuf,
    /// 正文（源文，可能已补尾部换行）。
    pub text: String,
}

/// 导出目录名用的时间戳：`YYYYMMDD-HHMMSS`（**UTC**）。
///
/// 用 UTC 而不是本地时间：免得为了时区再引一个依赖；目录名只要求可排序、可区分。
#[must_use]
pub fn timestamp_label(unix_millis: u128) -> String {
    let secs = (unix_millis / 1000) as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (hour, minute, second) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // 以 1970-01-01 为原点的民用历换算（Howard Hinnant 的算法，无需 chrono）
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    format!("{year:04}{month:02}{day:02}-{hour:02}{minute:02}{second:02}")
}

/// 正文尾换行补全：Markdown 文件按惯例以 `\n` 结尾。
#[must_use]
pub fn ensure_trailing_newline(text: &str) -> String {
    if text.is_empty() || text.ends_with('\n') {
        text.to_owned()
    } else {
        format!("{text}\n")
    }
}

/// 把结构树规划成导出清单（**纯函数**：不碰磁盘，便于单测）。
///
/// `bodies` 是 `node_id → 正文`；缺正文的笔记会产出一个 `Err`，由调用方计入失败。
/// 目录与文件共享"同一父目录下的命名空间"，重名按大小写不敏感去重。
pub fn plan_export(
    tree: &[TreeEntry],
    bodies: &HashMap<String, String>,
) -> Vec<Result<ExportItem, String>> {
    // node_id → 该节点在导出树里的目录路径
    let mut dirs: HashMap<String, PathBuf> = HashMap::new();
    // 每个父目录（根用空串）一个命名登记表
    let mut registries: HashMap<String, NameRegistry> = HashMap::new();
    let mut items = Vec::new();

    for entry in tree {
        let parent_key = entry.parent_id.clone().unwrap_or_default();
        let parent_path = entry
            .parent_id
            .as_ref()
            .and_then(|parent| dirs.get(parent).cloned())
            .unwrap_or_default();
        let registry = registries.entry(parent_key).or_default();

        match entry.kind {
            NodeKind::Folder => {
                let name = registry.insert(&sanitize_name(&entry.title));
                dirs.insert(entry.node_id.clone(), parent_path.join(name));
            }
            NodeKind::Note => {
                let Some(body) = bodies.get(&entry.node_id) else {
                    items.push(Err(format!("笔记「{}」没有正文", entry.title)));
                    continue;
                };
                // 带扩展名登记：目录「A」与文件「A.md」不会互相挤占，且 ` (2)` 加在
                // 扩展名之前（`重名 (2).md`），与常见工具一致。
                let stem = sanitize_name(&entry.title);
                let file_name = format!("{}.md", registry.insert_markdown(&stem));
                items.push(Ok(ExportItem {
                    relative: parent_path.join(file_name),
                    text: ensure_trailing_newline(body),
                }));
            }
        }
    }
    items
}

/// 执行导出：建目录 → 逐篇写文件 → 汇总结果。
pub fn export_vault(vault: &Vault, root: &Path) -> anyhow::Result<ExportReport> {
    let tree = list_tree(vault).context("读取结构树失败")?;

    // 先把正文全部读出来（失败的单篇记账，不中断整体）。
    let mut bodies: HashMap<String, String> = HashMap::new();
    let mut failures: Vec<String> = Vec::new();
    for entry in &tree {
        if entry.kind != NodeKind::Note {
            continue;
        }
        let Some(doc_id) = &entry.body_doc_id else {
            failures.push(format!("「{}」没有正文文档 ID", entry.title));
            continue;
        };
        match load_body_doc(vault, doc_id, 0, None) {
            Ok((doc, _head)) => {
                bodies.insert(entry.node_id.clone(), doc.text());
            }
            Err(error) => failures.push(format!("「{}」加载失败：{error}", entry.title)),
        }
    }

    let mut succeeded = 0usize;
    for planned in plan_export(&tree, &bodies) {
        match planned {
            Ok(item) => {
                let target = root.join(&item.relative);
                if let Some(parent) = target.parent() {
                    if let Err(error) = std::fs::create_dir_all(parent) {
                        failures.push(format!("建目录失败 {}：{error}", parent.display()));
                        continue;
                    }
                }
                match std::fs::write(&target, item.text.as_bytes()) {
                    Ok(()) => succeeded += 1,
                    Err(error) => {
                        failures.push(format!("写文件失败 {}：{error}", target.display()))
                    }
                }
            }
            Err(message) => failures.push(message),
        }
    }

    Ok(ExportReport {
        root: root.to_path_buf(),
        succeeded,
        failures,
    })
}

/// 一次导出的结果。
#[derive(Debug, Clone)]
pub struct ExportReport {
    /// 导出根目录。
    pub root: PathBuf,
    /// 成功写出的笔记数。
    pub succeeded: usize,
    /// 失败条目（最多保留前若干条，供界面提示）。
    pub failures: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::{ensure_trailing_newline, sanitize_name, NameRegistry, UNTITLED};

    #[test]
    fn illegal_windows_chars_become_underscore() {
        assert_eq!(
            sanitize_name("a<b>c:d\"e/f\\g|h?i*j"),
            "a_b_c_d_e_f_g_h_i_j"
        );
        assert_eq!(sanitize_name("制表\t符"), "制表_符");
    }

    #[test]
    fn trailing_spaces_and_dots_are_trimmed() {
        assert_eq!(sanitize_name("标题..."), "标题");
        assert_eq!(sanitize_name("标题   "), "标题");
        assert_eq!(sanitize_name(" 标题 "), "标题");
    }

    #[test]
    fn blank_titles_fall_back_to_untitled() {
        assert_eq!(sanitize_name(""), UNTITLED);
        assert_eq!(sanitize_name("   "), UNTITLED);
        assert_eq!(sanitize_name("..."), UNTITLED);
    }

    #[test]
    fn duplicates_get_numeric_suffix_case_insensitively() {
        let mut registry = NameRegistry::default();
        assert_eq!(registry.insert("笔记"), "笔记");
        assert_eq!(registry.insert("笔记"), "笔记 (2)");
        assert_eq!(registry.insert("笔记"), "笔记 (3)");
        // 大小写不同也算同一个名字（Windows 上 A.md / a.md 会互相覆盖）
        assert_eq!(registry.insert("Note"), "Note");
        assert_eq!(registry.insert("note"), "note (2)");
        // 文件主名：后缀加在扩展名之前
        let mut files = NameRegistry::default();
        assert_eq!(files.insert_markdown("重名"), "重名");
        assert_eq!(files.insert_markdown("重名"), "重名 (2)");
        assert_eq!(files.insert_markdown("重名"), "重名 (3)");
    }

    #[test]
    fn tree_maps_to_nested_relative_paths() {
        use super::{plan_export, ExportItem};
        use menghunote_storage::{NodeKind, TreeEntry};
        use std::collections::HashMap;

        let entry = |node_id: &str, title: &str, kind: NodeKind, parent: Option<&str>| TreeEntry {
            node_id: node_id.to_owned(),
            note_id: None,
            body_doc_id: None,
            title: title.to_owned(),
            parent_id: parent.map(str::to_owned),
            kind,
            sibling_rank: 0,
            depth: 0,
        };
        let tree = vec![
            entry("f1", "示例目录", NodeKind::Folder, None),
            entry("n1", "根级笔记", NodeKind::Note, None),
            entry("n2", "目录里的笔记", NodeKind::Note, Some("f1")),
            entry("f2", "子目录", NodeKind::Folder, Some("f1")),
            entry("n3", "更深一层", NodeKind::Note, Some("f2")),
            entry("n4", "重名", NodeKind::Note, None),
            entry("n5", "重名", NodeKind::Note, None),
        ];
        let mut bodies = HashMap::new();
        for id in ["n1", "n2", "n3", "n4", "n5"] {
            bodies.insert(id.to_owned(), format!("{id} 的正文"));
        }

        let items: Vec<ExportItem> = plan_export(&tree, &bodies)
            .into_iter()
            .map(|item| item.expect("规划不应失败"))
            .collect();
        let paths: Vec<String> = items
            .iter()
            .map(|item| item.relative.to_string_lossy().replace('\\', "/"))
            .collect();
        assert_eq!(
            paths,
            vec![
                "根级笔记.md",
                "示例目录/目录里的笔记.md",
                "示例目录/子目录/更深一层.md",
                "重名.md",
                "重名 (2).md",
            ]
        );
        assert_eq!(items[0].text, "n1 的正文\n", "缺尾换行要补上");
    }

    #[test]
    fn missing_body_becomes_a_failure_not_a_panic() {
        use super::plan_export;
        use menghunote_storage::{NodeKind, TreeEntry};
        use std::collections::HashMap;

        let tree = vec![TreeEntry {
            node_id: "n1".to_owned(),
            note_id: None,
            body_doc_id: None,
            title: "没有正文".to_owned(),
            parent_id: None,
            kind: NodeKind::Note,
            sibling_rank: 0,
            depth: 0,
        }];
        let results = plan_export(&tree, &HashMap::new());
        assert_eq!(results.len(), 1);
        assert!(results[0].is_err());
    }

    #[test]
    fn timestamp_is_sortable_and_fixed_width() {
        // 1970-01-01T00:00:00Z
        assert_eq!(super::timestamp_label(0), "19700101-000000");
        // 2026-11-10T12:34:56Z
        assert_eq!(super::timestamp_label(1_794_314_096_000), "20261110-123456");
    }

    #[test]
    fn trailing_newline_is_added_only_when_missing() {
        assert_eq!(ensure_trailing_newline("abc"), "abc\n");
        assert_eq!(ensure_trailing_newline("abc\n"), "abc\n");
        assert_eq!(ensure_trailing_newline(""), "");
    }
}
