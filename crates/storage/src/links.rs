//! 正文 wiki 链接的本地派生投影（L2）。
//!
//! `links` 表是**本地派生**：正文 durable 之后的 B 事务里按解析结果重建，
//! 不同步、不参与裁决。`to_note_id` 用当前结构树解析，解析不到写 `NULL`。

use std::collections::HashMap;

use crate::error::StorageError;
use crate::Vault;
use rusqlite::types::Value;

/// 一条已解析（或未解析）的 wiki 链接。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedWikiLink {
    /// 完整源码区间。
    pub span_start: u64,
    /// 完整源码区间终点。
    pub span_end: u64,
    /// 目标原文（方括号内、`#heading` 之前；可能为空）。
    pub target: String,
    /// 解析到的笔记（未解析 = None）。
    pub to_note_id: Option<String>,
}

/// 反向链接查询结果（展示用公开投影）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backlink {
    /// 来源笔记 ID。
    pub from_note_id: String,
    /// 来源笔记标题（结构树当前标题）。
    pub from_title: String,
    /// 链接在来源正文里的起始字节。
    pub start_byte: u64,
    /// 目标原文。
    pub target: String,
}

#[derive(Debug, Clone)]
struct Node {
    parent: Option<String>,
    note_id: Option<String>,
    title: String,
}

/// 解析正文里的全部 wiki 链接（按 source 顺序；空 target 指向当前笔记）。
pub(crate) fn resolve_wiki_links(
    vault: &Vault,
    current_note_id: &str,
    source: &str,
) -> Result<Vec<ResolvedWikiLink>, StorageError> {
    let parsed = menghunote_markdown::parse(source);
    if parsed.wiki_links.is_empty() {
        return Ok(Vec::new());
    }
    let tree = load_tree(vault)?;
    let notes = note_paths(&tree);
    let mut out = Vec::with_capacity(parsed.wiki_links.len());
    for link in &parsed.wiki_links {
        let target = source
            .get(link.target.start..link.target.end)
            .unwrap_or("")
            .trim()
            .to_owned();
        let normalized = normalize_target(&target);
        let to_note_id = if normalized.is_empty() {
            Some(current_note_id.to_owned())
        } else {
            resolve_target(&notes, &normalized)
        };
        out.push(ResolvedWikiLink {
            span_start: link.span.start as u64,
            span_end: link.span.end as u64,
            target,
            to_note_id,
        });
    }
    Ok(out)
}

/// 查某篇笔记的反向链接（来源标题取结构树当前值）。
pub fn list_backlinks(vault: &Vault, to_note_id: &str) -> Result<Vec<Backlink>, StorageError> {
    let rows = vault.query(
        "SELECT l.from_note_id, t.title, l.start_byte, l.target \
         FROM links l JOIN tree t ON t.note_id = l.from_note_id AND t.deleted = 0 \
         WHERE l.to_note_id = ?1 AND l.kind = 'wiki' \
         ORDER BY t.title, l.start_byte, l.target",
        &[Value::Text(to_note_id.to_owned())],
    )?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        out.push(Backlink {
            from_note_id: text_of(&row, 0, "links.from_note_id")?,
            from_title: text_of(&row, 1, "tree.title")?,
            start_byte: u64_of(&row, 2, "links.start_byte")?,
            target: text_of(&row, 3, "links.target")?,
        });
    }
    Ok(out)
}

fn load_tree(vault: &Vault) -> Result<HashMap<String, Node>, StorageError> {
    let rows = vault.query(
        "SELECT t.node_id, t.parent_id, t.note_id, t.title \
         FROM tree t WHERE t.deleted = 0",
        &[],
    )?;
    let mut nodes = HashMap::with_capacity(rows.len());
    for row in rows {
        let node_id = text_of(&row, 0, "tree.node_id")?;
        let parent = optional_text(&row, 1, "tree.parent_id")?;
        let note_id = optional_text(&row, 2, "tree.note_id")?;
        let title = text_of(&row, 3, "tree.title")?;
        nodes.insert(
            node_id,
            Node {
                parent,
                note_id,
                title,
            },
        );
    }
    Ok(nodes)
}

fn note_paths(nodes: &HashMap<String, Node>) -> Vec<(String, String, String)> {
    let mut notes = Vec::new();
    for node in nodes.values() {
        let Some(note_id) = &node.note_id else {
            continue;
        };
        let mut path = vec![node.title.clone()];
        let mut current = node.parent.as_deref();
        let mut guard = 0usize;
        while let Some(parent_id) = current {
            guard += 1;
            if guard > 64 {
                break;
            }
            let Some(parent) = nodes.get(parent_id) else {
                break;
            };
            path.push(parent.title.clone());
            current = parent.parent.as_deref();
        }
        path.reverse();
        notes.push((
            note_id.clone(),
            path.join("/").to_lowercase(),
            node.title.to_lowercase(),
        ));
    }
    notes
}

fn resolve_target(notes: &[(String, String, String)], normalized: &str) -> Option<String> {
    let mut best: Option<(u8, usize, &str)> = None;
    let mut ambiguous = false;
    for (note_id, path, title) in notes {
        let score = if path == normalized {
            0
        } else if !normalized.contains('/') && title == normalized {
            1
        } else if path.ends_with(&format!("/{normalized}")) {
            2
        } else {
            continue;
        };
        let length = path.chars().count();
        match best {
            None => {
                best = Some((score, length, note_id));
                ambiguous = false;
            }
            Some((best_score, best_len, best_id)) => {
                if (score, length) < (best_score, best_len) {
                    best = Some((score, length, note_id));
                    ambiguous = false;
                } else if (score, length) == (best_score, best_len) && note_id != best_id {
                    ambiguous = true;
                }
            }
        }
    }
    if ambiguous {
        return None;
    }
    best.map(|(_, _, note_id)| note_id.to_owned())
}

fn normalize_target(target: &str) -> String {
    target
        .trim()
        .trim_end_matches(".md")
        .trim_matches('/')
        .to_lowercase()
}

fn text_of(row: &[Value], index: usize, field: &str) -> Result<String, StorageError> {
    match row.get(index) {
        Some(Value::Text(text)) => Ok(text.clone()),
        other => Err(StorageError::Corrupt {
            context: format!("{field} 读回异常：{other:?}"),
        }),
    }
}

fn optional_text(row: &[Value], index: usize, field: &str) -> Result<Option<String>, StorageError> {
    match row.get(index) {
        Some(Value::Text(text)) => Ok(Some(text.clone())),
        Some(Value::Null) | None => Ok(None),
        other => Err(StorageError::Corrupt {
            context: format!("{field} 读回异常：{other:?}"),
        }),
    }
}

fn u64_of(row: &[Value], index: usize, field: &str) -> Result<u64, StorageError> {
    match row.get(index) {
        Some(Value::Integer(value)) => u64::try_from(*value).map_err(|_| StorageError::Corrupt {
            context: format!("{field} 必须是非负整数：{value}"),
        }),
        other => Err(StorageError::Corrupt {
            context: format!("{field} 读回异常：{other:?}"),
        }),
    }
}
