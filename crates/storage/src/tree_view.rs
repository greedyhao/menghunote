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
//!
//! ## 两种同级顺序（`list_tree` vs [`list_tree_sorted`]）
//!
//! - [`list_tree`] / [`list_notes`] 保持**结构 rank 顺序**（投影里的 `sibling_rank`）。
//!   同步/导出/基线对账都读它，语义不变；
//! - [`list_tree_sorted`] 是**侧栏展示顺序**：每个层级「目录永远在笔记前面」，
//!   同组内再按 [`TreeSort`] 排序（文件名 A-Z/Z-A、修改时间、创建时间）。
//!   父子层级/`depth` 完全保留——排序只改变**同级兄弟**的相对次序，
//!   因此「父节点 → 全部后代」的连续区间性质（UI 折叠依赖它）不变。
//!
//! 时间戳来自 `notes(created_at, updated_at)`（毫秒，投影写入）；目录在 `notes` 里没有行，
//! 因此 `created_at` / `updated_at` 为 `None`，按需求**用标题排序**。

use std::cmp::Ordering;
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

/// 侧栏排序键（本机偏好；**不同步、不写正文**）。
///
/// 排序的**第一关键字永远是「目录在前、笔记在后」**（Obsidian 风格）；
/// 这个枚举只决定同一组内（全是目录 / 全是笔记）的次序。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TreeSort {
    /// 文件名 A-Z（默认；大小写不敏感，完全相同再按原串/节点 ID 稳定排序）。
    #[default]
    NameAsc,
    /// 文件名 Z-A。
    NameDesc,
    /// 修改时间 新 → 旧（目录没有时间，组内仍按标题 A-Z）。
    UpdatedNewest,
    /// 修改时间 旧 → 新（目录没有时间，组内仍按标题 A-Z）。
    UpdatedOldest,
    /// 创建时间 新 → 旧（目录没有时间，组内仍按标题 A-Z）。
    CreatedNewest,
    /// 创建时间 旧 → 新（目录没有时间，组内仍按标题 A-Z）。
    CreatedOldest,
}

impl TreeSort {
    /// 全部排序键（菜单顺序与持久化 key 的稳定集合）。
    pub const ALL: [TreeSort; 6] = [
        TreeSort::NameAsc,
        TreeSort::NameDesc,
        TreeSort::UpdatedNewest,
        TreeSort::UpdatedOldest,
        TreeSort::CreatedNewest,
        TreeSort::CreatedOldest,
    ];

    /// 持久化 / selector 用的稳定短名（见 `sidebar-sort-<key>`）。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            TreeSort::NameAsc => "name_asc",
            TreeSort::NameDesc => "name_desc",
            TreeSort::UpdatedNewest => "updated_newest",
            TreeSort::UpdatedOldest => "updated_oldest",
            TreeSort::CreatedNewest => "created_newest",
            TreeSort::CreatedOldest => "created_oldest",
        }
    }

    /// 从稳定短名解析；未知值返回 `None`（调用方回退默认，不报错）。
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|sort| sort.as_str() == raw.trim())
    }
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
    /// 创建时间（毫秒；**目录为 `None`**，`notes` 表里没有目录行）。
    pub created_at: Option<i64>,
    /// 修改时间（毫秒；**目录为 `None`**，随正文/结构写入更新）。
    pub updated_at: Option<i64>,
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
///
/// 同级顺序 = **结构 `sibling_rank`**（投影里的真实顺序；同步/导出/基线对账用）。
/// 侧栏展示请用 [`list_tree_sorted`]（目录优先 + 用户排序）。
pub fn list_tree(vault: &Vault) -> Result<Vec<TreeEntry>, StorageError> {
    query_entries(vault, SiblingOrder::Rank)
}

/// 侧栏展示顺序的整棵树：**每个层级目录在前、笔记在后**，同组内按 `sort` 排序。
///
/// 与 [`list_tree`] 的唯一区别是**同级兄弟的次序**；`parent_id` / `depth` / 父子连续
/// 区间性质完全保留（折叠逻辑不受影响）。目录没有时间戳，时间排序下目录组按标题 A-Z。
pub fn list_tree_sorted(vault: &Vault, sort: TreeSort) -> Result<Vec<TreeEntry>, StorageError> {
    query_entries(vault, SiblingOrder::Sorted(sort))
}

/// 只要笔记（顺序与 `depth` 与 [`list_tree`] 完全一致，目录被过滤掉）。
///
/// 注意 `depth` 是**整棵树里的真实深度**（子笔记在目录下时 `depth > 0`），不是过滤后的层级。
pub fn list_notes(vault: &Vault) -> Result<Vec<TreeEntry>, StorageError> {
    let entries = query_entries(vault, SiblingOrder::Rank)?;
    Ok(entries
        .into_iter()
        .filter(|entry| entry.kind == NodeKind::Note)
        .collect())
}

/// 同级排序模式。
#[derive(Debug, Clone, Copy)]
enum SiblingOrder {
    /// 结构 rank（`sibling_rank` + `node_id` 兜底）——`list_tree` 的既有语义。
    Rank,
    /// 侧栏展示顺序：目录优先 + [`TreeSort`]。
    Sorted(TreeSort),
}

fn query_entries(vault: &Vault, order: SiblingOrder) -> Result<Vec<TreeEntry>, StorageError> {
    let rows = vault.query(
        "SELECT t.node_id, t.note_id, n.body_doc_id, t.title, t.parent_id, t.sibling_rank, \
                n.created_at, n.updated_at \
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
            // 目录没有 `notes` 行 → 两列都是 NULL → `None`（不是 0）。
            created_at: optional_integer(row, 6, "notes.created_at")?,
            updated_at: optional_integer(row, 7, "notes.updated_at")?,
        });
    }

    Ok(flatten_preorder(entries, order))
}

/// 把「parent_id + 同级顺序」的行集合组装成先序 DFS 展平列表（并填 `depth`）。
fn flatten_preorder(entries: Vec<TreeEntry>, order: SiblingOrder) -> Vec<TreeEntry> {
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

    // 同级排序：rank 或「目录优先 + 用户排序」，两者的末位兜底都是 node_id（完全确定）
    for list in children.values_mut() {
        list.sort_by(|left, right| compare_siblings(&entries[*left], &entries[*right], order));
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

/// 同级比较：`Rank` 保持既有语义；`Sorted` 先分「目录 / 笔记」，再按排序键。
fn compare_siblings(left: &TreeEntry, right: &TreeEntry, order: SiblingOrder) -> Ordering {
    match order {
        SiblingOrder::Rank => left
            .sibling_rank
            .cmp(&right.sibling_rank)
            .then_with(|| left.node_id.cmp(&right.node_id)),
        SiblingOrder::Sorted(sort) => {
            // 第一关键字：目录永远在前（每一层级都成立）。
            match (left.is_folder(), right.is_folder()) {
                (true, false) => return Ordering::Less,
                (false, true) => return Ordering::Greater,
                _ => {}
            }
            if left.is_folder() {
                // 目录没有时间戳：任何排序键下目录组都按标题 A-Z。
                return compare_title_asc(left, right);
            }
            match sort {
                TreeSort::NameAsc => compare_title_asc(left, right),
                TreeSort::NameDesc => compare_title_desc(left, right),
                TreeSort::UpdatedNewest => {
                    compare_timestamp(left, right, |entry| entry.updated_at.unwrap_or(0))
                        .then_with(|| compare_title_asc(left, right))
                }
                TreeSort::UpdatedOldest => {
                    compare_timestamp(left, right, |entry| entry.updated_at.unwrap_or(0))
                        .reverse()
                        .then_with(|| compare_title_asc(left, right))
                }
                TreeSort::CreatedNewest => {
                    compare_timestamp(left, right, |entry| entry.created_at.unwrap_or(0))
                        .then_with(|| compare_title_asc(left, right))
                }
                TreeSort::CreatedOldest => {
                    compare_timestamp(left, right, |entry| entry.created_at.unwrap_or(0))
                        .reverse()
                        .then_with(|| compare_title_asc(left, right))
                }
            }
        }
    }
}

/// 时间降序（新 → 旧）：`right.key.cmp(left.key)`，相等时由调用方按标题兜底。
fn compare_timestamp(
    left: &TreeEntry,
    right: &TreeEntry,
    key: impl Fn(&TreeEntry) -> i64,
) -> Ordering {
    key(right).cmp(&key(left))
}

/// 标题升序：大小写不敏感 → 原串 → 节点 ID（三层都相同才视为相等，保证稳定）。
fn compare_title_asc(left: &TreeEntry, right: &TreeEntry) -> Ordering {
    compare_title_ci(left, right)
        .then_with(|| left.title.cmp(&right.title))
        .then_with(|| left.node_id.cmp(&right.node_id))
}

/// 标题降序：大小写不敏感的逆序 → 原串逆序 → 节点 ID 升序（末位仍确定）。
fn compare_title_desc(left: &TreeEntry, right: &TreeEntry) -> Ordering {
    compare_title_ci(right, left)
        .then_with(|| right.title.cmp(&left.title))
        .then_with(|| left.node_id.cmp(&right.node_id))
}

/// 大小写不敏感的标题比较（逐字符 `to_lowercase`，不分配字符串）。
fn compare_title_ci(left: &TreeEntry, right: &TreeEntry) -> Ordering {
    let mut left_chars = left.title.chars().flat_map(char::to_lowercase);
    let mut right_chars = right.title.chars().flat_map(char::to_lowercase);
    loop {
        match (left_chars.next(), right_chars.next()) {
            (Some(left_char), Some(right_char)) => match left_char.cmp(&right_char) {
                Ordering::Equal => continue,
                other => return other,
            },
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
        }
    }
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

fn optional_integer(row: &[Value], index: usize, field: &str) -> Result<Option<i64>, StorageError> {
    match row.get(index) {
        Some(Value::Integer(value)) => Ok(Some(*value)),
        Some(Value::Null) | None => Ok(None),
        other => Err(StorageError::Corrupt {
            context: format!("{field} 读回异常：{other:?}"),
        }),
    }
}
