//! 侧栏用的**笔记树读接口**：从 SQL 投影表（`tree` + `notes`）读取，按先序 DFS 展平。
//!
//! ## 为什么读投影，而不是每次重放结构文档（证据）
//!
//! - `projector::project` 第 4 段明确写「**tree：所有节点（含非笔记节点）**」，且写入
//!   `title` / `parent_id` / `sibling_rank` / `deleted` —— 侧栏需要的字段投影里全都有；
//! - 目录节点在 `notes` 表里**没有行**（投影只对 `note_id` 存在的节点写 `notes`），
//!   所以标题必须取 `tree.title`，不能取 `notes.title`；
//! - 结构写入的唯一路径 `StructureWriter::commit` 是「A 事务 → 投影」，因此读投影与读
//!   CRDT 快照等价，但省掉一次 snapshot + 全量 update 重放（侧栏会频繁刷新）。
//!
//! ## 顺序与形状
//!
//! `sibling_rank` 是结构快照里的**同级 dense rank**（先序 DFS 生成），配合
//! `tree_parent_order(parent_id, sibling_rank, node_id)` 索引保证稳定顺序。本模块把它组装成
//! **展平的先序 DFS 列表**，并给出 `depth`（根 = 0）供 UI 缩进；同一树状态两次调用结果一致。
//!
//! - 已删除（`deleted = 1`）的节点不返回（回收站是另一条查询）；
//! - 父节点被删除或缺失时，其子树按根级处理（不丢节点，也不制造环）；
//! - 空 vault 返回空 `Vec`（不报错）。

use std::collections::HashMap;

use rusqlite::types::Value;

use crate::error::StorageError;
use crate::Vault;

/// 树节点种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    /// 笔记（有 `note_id` / `body_doc_id`）。
    Note,
    /// 目录（纯结构节点，没有正文）。
    Folder,
}

/// 侧栏的一项：先序 DFS 顺序、已展平。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    /// 树节点 ID（结构文档里的 `TreeID` 字符串；移动/删除都用它）。
    pub node_id: String,
    /// 笔记 ID（目录为 `None`）。
    pub note_id: Option<String>,
    /// 正文文档 ID（目录为 `None`；打开编辑器用）。
    pub body_doc_id: Option<String>,
    /// 标题（结构里的最新值；rename 后立即反映）。
    pub title: String,
    /// 父节点 ID（根级为 `None`）。
    pub parent_id: Option<String>,
    /// 节点种类。
    pub kind: NodeKind,
    /// 同级排序键（dense rank，越小越靠前）。
    pub sibling_rank: i64,
    /// 缩进深度（根级 = 0）。
    pub depth: usize,
}

impl TreeEntry {
    /// 是否笔记。
    pub fn is_note(&self) -> bool {
        self.kind == NodeKind::Note
    }

    /// 是否目录。
    pub fn is_folder(&self) -> bool {
        self.kind == NodeKind::Folder
    }
}

/// 整棵树（笔记 + 目录），按先序 DFS 展平，已删除节点不返回。
pub fn list_tree(vault: &Vault) -> Result<Vec<TreeEntry>, StorageError> {
    query_entries(vault)
}

/// 只要笔记（顺序与 `depth` 与 [`list_tree`] 完全一致，目录被过滤掉）。
///
/// 注意 `depth` 是**整棵树里的真实深度**（子笔记在目录下时 `depth > 0`），不是过滤后的层级。
pub fn list_notes(vault: &Vault) -> Result<Vec<TreeEntry>, StorageError> {
    let entries = query_entries(vault)?;
    Ok(entries
        .into_iter()
        .filter(|entry| entry.kind == NodeKind::Note)
        .collect())
}

fn query_entries(vault: &Vault) -> Result<Vec<TreeEntry>, StorageError> {
    let rows = vault.query(
        "SELECT t.node_id, t.note_id, n.body_doc_id, t.title, t.parent_id, t.sibling_rank \
         FROM tree t LEFT JOIN notes n ON n.note_id = t.note_id \
         WHERE t.deleted = 0",
        &[],
    )?;

    let mut entries = Vec::with_capacity(rows.len());
    for row in &rows {
        let node_id = text_of(row, 0, "tree.node_id")?;
        let note_id = optional_text(row, 1, "tree.note_id")?;
        let body_doc_id = optional_text(row, 2, "notes.body_doc_id")?;
        let kind = if note_id.is_some() {
            NodeKind::Note
        } else {
            NodeKind::Folder
        };
        entries.push(TreeEntry {
            node_id,
            note_id,
            body_doc_id,
            title: text_of(row, 3, "tree.title")?,
            parent_id: optional_text(row, 4, "tree.parent_id")?,
            kind,
            sibling_rank: integer_of(row, 5, "tree.sibling_rank")?,
            depth: 0,
        });
    }

    Ok(flatten_preorder(entries))
}

/// 把「parent_id + sibling_rank」的行集合组装成先序 DFS 展平列表（并填 `depth`）。
fn flatten_preorder(entries: Vec<TreeEntry>) -> Vec<TreeEntry> {
    // 已知节点：用来判定「父节点缺失」（被删或脏数据）→ 当根处理
    let known: HashMap<String, usize> = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| (entry.node_id.clone(), index))
        .collect();

    let mut children: HashMap<Option<usize>, Vec<usize>> = HashMap::new();
    for (index, entry) in entries.iter().enumerate() {
        let parent = entry
            .parent_id
            .as_ref()
            .and_then(|parent| known.get(parent).copied());
        children.entry(parent).or_default().push(index);
    }

    // 同级排序：rank 优先，node_id 兜底保证完全确定（与投影索引一致）
    for list in children.values_mut() {
        list.sort_by(|left, right| {
            entries[*left]
                .sibling_rank
                .cmp(&entries[*right].sibling_rank)
                .then_with(|| entries[*left].node_id.cmp(&entries[*right].node_id))
        });
    }

    let mut ordered: Vec<(usize, usize)> = Vec::with_capacity(entries.len());
    let mut visited = vec![false; entries.len()];
    let mut stack: Vec<(usize, usize)> = children
        .get(&None)
        .map(|roots| {
            roots
                .iter()
                .rev()
                .map(|index| (*index, 0usize))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    while let Some((index, depth)) = stack.pop() {
        if visited[index] {
            continue; // 防御：脏数据成环时不重复、不死循环
        }
        visited[index] = true;
        ordered.push((index, depth));

        if let Some(kids) = children.get(&Some(index)) {
            for kid in kids.iter().rev() {
                stack.push((*kid, depth + 1));
            }
        }
    }

    // 不可达节点（理论上只有成环脏数据）按根级追加，保证「不丢节点」
    for (index, visited) in visited.iter().enumerate() {
        if !visited {
            ordered.push((index, 0));
        }
    }

    let mut slots: Vec<Option<TreeEntry>> = entries.into_iter().map(Some).collect();
    ordered
        .into_iter()
        .filter_map(|(index, depth)| {
            slots
                .get_mut(index)
                .and_then(Option::take)
                .map(|mut entry| {
                    entry.depth = depth;
                    entry
                })
        })
        .collect()
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

fn integer_of(row: &[Value], index: usize, field: &str) -> Result<i64, StorageError> {
    match row.get(index) {
        Some(Value::Integer(value)) => Ok(*value),
        other => Err(StorageError::Corrupt {
            context: format!("{field} 读回异常：{other:?}"),
        }),
    }
}
