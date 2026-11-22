//! 结构 CRDT：每 vault 一个结构 `LoroDoc`（`loro_docs.kind='structure'`）。
//!
//! 权威归属（ADR 0001 §3.1）：
//!
//! - 结构 `LoroTree` 只承载**笔记导航树**，不是 Markdown 块树；
//! - 节点 `meta`（`LoroMap`）保存 `title` / `note_id` / `body_doc_id` / `created_at` /
//!   标签成员 / 收藏 / 置顶 / 删除与恢复事件；
//! - 全局标签字典是根上另一个 `LoroMap`（tag_id → {name, normalized_name}）；
//! - 删除语义（ADR 0001 §3.3）：**grow-only 删除事件 ID** + **已观察到的恢复确认 ID**，
//!   有效删除 = 存在未被确认的删除事件。**不用** `LoroTree::delete`，也不用 SQL `DELETE`。
//!
//! 本模块只做 CRDT 侧；落库与投影见 [`crate::structure_store`] / [`crate::projector`]。

use loro::{ExportMode, Frontiers, LoroDoc, LoroMap, LoroTree, LoroValue, TreeID, VersionVector};

use crate::body_doc::PendingUpdate;
use crate::error::StorageError;

/// 结构文档在 `loro_docs` 里的固定 doc_id（每 vault 一个）。
pub const STRUCTURE_DOC_ID: &str = "structure";
/// 导航树容器名。
pub const TREE_CONTAINER: &str = "tree";
/// 全局标签字典容器名。
pub const TAG_DICT_CONTAINER: &str = "tag_dict";
/// 根级元数据（本机计数器等）容器名。
pub const ROOT_META_CONTAINER: &str = "meta";

/// 节点 meta：标题。
pub const KEY_TITLE: &str = "title";
/// 节点 meta：稳定 note_id。
pub const KEY_NOTE_ID: &str = "note_id";
/// 节点 meta：正文文档 ID。
pub const KEY_BODY_DOC_ID: &str = "body_doc_id";
/// 节点 meta：创建时间（客户端编辑戳）。
pub const KEY_CREATED_AT: &str = "created_at";
/// 节点 meta：最后一次结构改动戳。
pub const KEY_EDITED_AT: &str = "edited_at";
/// 节点 meta：收藏。
pub const KEY_FAVORITE: &str = "is_favorite";
/// 节点 meta：置顶。
pub const KEY_PINNED: &str = "is_pinned";
/// 节点 meta：标签成员集合（子 `LoroMap`，key = tag_id）。
pub const KEY_TAGS: &str = "tags";
/// 节点 meta：grow-only 删除事件列表（子 `LoroList`）。
pub const KEY_DELETE_EVENTS: &str = "delete_events";
/// 节点 meta：已确认（已见过）的删除事件集合（子 `LoroMap`）。
pub const KEY_RESTORE_ACKS: &str = "restore_acks";
/// 标签字典项：显示名。
const KEY_TAG_NAME: &str = "name";
/// 标签字典项：归一化名。
const KEY_TAG_NORMALIZED: &str = "normalized_name";

/// 新建笔记时返回的三个稳定 ID。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteSeed {
    /// 树节点 ID（`TreeID` 的字符串形式）。
    pub node_id: String,
    /// 永久稳定的笔记 ID。
    pub note_id: String,
    /// 对应的正文文档 ID。
    pub body_doc_id: String,
}

/// 一个标签字典项。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagSnapshot {
    /// 标签 ID。
    pub tag_id: String,
    /// 原始显示名。
    pub name: String,
    /// 归一化名（用于查重，不设 UNIQUE）。
    pub normalized_name: String,
}

/// 一个树节点的有效状态（投影器直接消费）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeSnapshot {
    /// `TreeID` 字符串。
    pub node_id: String,
    /// 父节点（根级为 `None`）。
    pub parent: Option<String>,
    /// 同级 dense rank（从遍历顺序生成，不用 native fractional index）。
    pub index: usize,
    /// 标题。
    pub title: String,
    /// 笔记 ID（纯目录节点为 `None`）。
    pub note_id: Option<String>,
    /// 正文文档 ID。
    pub body_doc_id: Option<String>,
    /// 创建时间。
    pub created_at: i64,
    /// 最后结构改动时间。
    pub edited_at: i64,
    /// 收藏。
    pub is_favorite: bool,
    /// 置顶。
    pub is_pinned: bool,
    /// 有效删除（存在未被确认的删除事件）。
    pub deleted: bool,
    /// 全部删除事件（grow-only）。
    pub delete_events: Vec<String>,
    /// 已被恢复确认的事件。
    pub confirmed_events: Vec<String>,
    /// 标签 ID 列表（顺序稳定：按 tag_id 排序）。
    pub tags: Vec<String>,
}

impl NodeSnapshot {
    /// 节点是否是笔记（有 note_id 与 body_doc_id）。
    pub fn is_note(&self) -> bool {
        self.note_id.is_some() && self.body_doc_id.is_some()
    }
}

/// 整棵结构树的快照。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructureSnapshot {
    /// 节点（先序 DFS 顺序）。
    pub nodes: Vec<NodeSnapshot>,
    /// 标签字典。
    pub tags: Vec<TagSnapshot>,
}

/// 结构 CRDT 文档。
pub struct StructureDoc {
    doc: LoroDoc,
    tree: LoroTree,
    tag_dict: LoroMap,
    root_meta: LoroMap,
}

impl std::fmt::Debug for StructureDoc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StructureDoc")
            .field("peer_id", &self.doc.peer_id())
            .field("nodes", &self.tree.nodes().len())
            .finish()
    }
}

impl StructureDoc {
    /// 新建结构文档并设置本机 peer id。
    pub fn create(peer_id: u64) -> Result<Self, StorageError> {
        let doc = LoroDoc::new();
        doc.set_peer_id(peer_id)?;
        let this = Self {
            tree: doc.get_tree(TREE_CONTAINER),
            tag_dict: doc.get_map(TAG_DICT_CONTAINER),
            root_meta: doc.get_map(ROOT_META_CONTAINER),
            doc,
        };
        // 启用 fractional index：本切片移动只需 append，但将来要插到指定位置时不必改协议
        this.tree.enable_fractional_index(0);
        Ok(this)
    }

    /// 从快照 + 后续 update 重放。
    pub fn restore(
        peer_id: u64,
        snapshot: Option<&[u8]>,
        updates: &[Vec<u8>],
    ) -> Result<Self, StorageError> {
        let this = Self::create(peer_id)?;
        if let Some(snapshot) = snapshot {
            this.doc.import(snapshot)?;
        }
        for payload in updates {
            this.doc.import(payload)?;
        }
        // fractional index 配置不在 oplog 里，重放后重新打开，保证后续 move 可用
        this.tree.enable_fractional_index(0);
        Ok(this)
    }

    /// 本机 peer id。
    pub fn peer_id(&self) -> u64 {
        self.doc.peer_id()
    }

    /// 当前 frontier。
    pub fn frontiers(&self) -> Frontiers {
        self.doc.state_frontiers()
    }

    /// 当前 oplog 版本向量。
    pub fn vv(&self) -> VersionVector {
        self.doc.oplog_vv()
    }

    /// 自 `from` 之后的新增 ops。
    pub fn pending_update(&self, from: &VersionVector) -> Result<PendingUpdate, StorageError> {
        let payload = self.doc.export(ExportMode::updates(from))?;
        Ok(PendingUpdate {
            payload,
            end_frontier: self.frontiers().encode(),
            end_vv: self.vv().encode(),
        })
    }

    /// 全量历史 update。
    pub fn export_all_updates(&self) -> Result<Vec<u8>, StorageError> {
        Ok(self.doc.export(ExportMode::all_updates())?)
    }

    /// 完整快照。
    pub fn export_snapshot(&self) -> Result<Vec<u8>, StorageError> {
        Ok(self.doc.export(ExportMode::Snapshot)?)
    }

    /// 导入 update / snapshot（幂等）。
    pub fn import(&self, bytes: &[u8]) -> Result<(), StorageError> {
        self.doc.import(bytes)?;
        Ok(())
    }

    /// 新建一篇笔记：根级树节点 + 全部 meta 字段 + 预先建好的子容器。
    ///
    /// 子容器先建好，是为了避免两个 peer 并发「首次打标签 / 首次删除」时各建一个同名容器。
    pub fn create_note(&self, title: &str, now: i64) -> Result<NoteSeed, StorageError> {
        let node = self.tree.create(None::<TreeID>)?;
        let note_seq = self.next_seq("note")?;
        let note_id = format!("note-{}-{note_seq:06}", self.doc.peer_id());
        let body_doc_id = format!("{note_id}-body");

        let meta = self.tree.get_meta(node)?;
        meta.insert(KEY_TITLE, title)?;
        meta.insert(KEY_NOTE_ID, note_id.as_str())?;
        meta.insert(KEY_BODY_DOC_ID, body_doc_id.as_str())?;
        meta.insert(KEY_CREATED_AT, now)?;
        meta.insert(KEY_EDITED_AT, now)?;
        meta.insert(KEY_FAVORITE, false)?;
        meta.insert(KEY_PINNED, false)?;
        meta.ensure_mergeable_map(KEY_TAGS)?;
        meta.ensure_mergeable_list(KEY_DELETE_EVENTS)?;
        meta.ensure_mergeable_map(KEY_RESTORE_ACKS)?;
        self.doc.commit();

        Ok(NoteSeed {
            node_id: node.to_string(),
            note_id,
            body_doc_id,
        })
    }

    /// 新建一个**目录**节点：与 [`StructureDoc::create_note`] 同一条写入路径，只是不写
    /// `note_id` / `body_doc_id`（目录没有正文文档，投影时落在那张 `notes` 表之外）。
    ///
    /// 返回 `TreeID` 字符串（移动/改名/嵌套都用它）。
    pub fn create_folder(&self, title: &str, now: i64) -> Result<String, StorageError> {
        let node = self.tree.create(None::<TreeID>)?;
        let meta = self.tree.get_meta(node)?;
        meta.insert(KEY_TITLE, title)?;
        meta.insert(KEY_CREATED_AT, now)?;
        meta.insert(KEY_EDITED_AT, now)?;
        meta.insert(KEY_FAVORITE, false)?;
        meta.insert(KEY_PINNED, false)?;
        meta.ensure_mergeable_map(KEY_TAGS)?;
        meta.ensure_mergeable_list(KEY_DELETE_EVENTS)?;
        meta.ensure_mergeable_map(KEY_RESTORE_ACKS)?;
        self.doc.commit();
        Ok(node.to_string())
    }

    /// 改名。
    pub fn rename(&self, node_id: &str, title: &str, now: i64) -> Result<(), StorageError> {
        let meta = self.node_meta(node_id)?;
        meta.insert(KEY_TITLE, title)?;
        meta.insert(KEY_EDITED_AT, now)?;
        self.doc.commit();
        Ok(())
    }

    /// 设置收藏。
    pub fn set_favorite(&self, node_id: &str, value: bool, now: i64) -> Result<(), StorageError> {
        let meta = self.node_meta(node_id)?;
        meta.insert(KEY_FAVORITE, value)?;
        meta.insert(KEY_EDITED_AT, now)?;
        self.doc.commit();
        Ok(())
    }

    /// 设置置顶。
    pub fn set_pinned(&self, node_id: &str, value: bool, now: i64) -> Result<(), StorageError> {
        let meta = self.node_meta(node_id)?;
        meta.insert(KEY_PINNED, value)?;
        meta.insert(KEY_EDITED_AT, now)?;
        self.doc.commit();
        Ok(())
    }

    /// 移动节点（挂到 `parent` 末尾；`parent = None` 表示移到根级）。
    pub fn move_node(
        &self,
        node_id: &str,
        parent: Option<&str>,
        now: i64,
    ) -> Result<(), StorageError> {
        let target = self.parse_node(node_id)?;
        let parent_id = match parent {
            Some(parent) => Some(self.parse_node(parent)?),
            None => None,
        };
        self.tree.mov(target, parent_id)?;
        let meta = self.tree.get_meta(target)?;
        meta.insert(KEY_EDITED_AT, now)?;
        self.doc.commit();
        Ok(())
    }

    /// 确保标签存在并返回 tag_id。
    ///
    /// tag_id 由**归一化名派生**（`tag-<normalized>`），不依赖本地计数器：
    /// 两副本并发创建同名标签会得到同一个 ID，于是成员集合天然去重、字典也不会出现两份。
    /// 这与 DDL 里 `tags.normalized_name` 不设 UNIQUE 不冲突（那只是兜底）。
    pub fn ensure_tag(&self, name: &str) -> Result<String, StorageError> {
        let normalized = normalize_tag(name);
        if normalized.is_empty() {
            return Err(StorageError::InvalidTagName {
                name: name.to_owned(),
            });
        }
        // 已有标签（含远端并发创建的同一个 ID）直接复用
        if let Some(existing) = self.find_tag(&normalized) {
            return Ok(existing);
        }

        let tag_id = format!("tag-{normalized}");
        let entry = self.tag_dict.ensure_mergeable_map(tag_id.as_str())?;
        entry.insert(KEY_TAG_NAME, name.trim())?;
        entry.insert(KEY_TAG_NORMALIZED, normalized.as_str())?;
        self.doc.commit();
        Ok(tag_id)
    }

    /// 给节点加标签（集合语义，重复加不产生重复项）。
    pub fn add_tag(&self, node_id: &str, name: &str, now: i64) -> Result<String, StorageError> {
        let tag_id = self.ensure_tag(name)?;
        let meta = self.node_meta(node_id)?;
        let tags = meta.ensure_mergeable_map(KEY_TAGS)?;
        tags.insert(tag_id.as_str(), true)?;
        meta.insert(KEY_EDITED_AT, now)?;
        self.doc.commit();
        Ok(tag_id)
    }

    /// 从节点移除标签（只删成员，不删字典项）。
    pub fn remove_tag(&self, node_id: &str, tag_id: &str, now: i64) -> Result<(), StorageError> {
        let meta = self.node_meta(node_id)?;
        let tags = meta.ensure_mergeable_map(KEY_TAGS)?;
        tags.delete(tag_id)?;
        meta.insert(KEY_EDITED_AT, now)?;
        self.doc.commit();
        Ok(())
    }

    /// 删除笔记：追加一个 **grow-only** 删除事件，返回事件 ID。
    ///
    /// 注意：这里**不删树节点**（ADR 0001 §3.3 禁止用 `LoroTree::delete` 当删除真源）。
    pub fn delete_note(&self, node_id: &str, now: i64) -> Result<String, StorageError> {
        let meta = self.node_meta(node_id)?;
        let events = meta.ensure_mergeable_list(KEY_DELETE_EVENTS)?;
        let event_id = format!("del-{}-{}", self.doc.peer_id(), self.next_seq("event")?);
        events.push(event_id.as_str())?;
        meta.insert(KEY_EDITED_AT, now)?;
        self.doc.commit();
        Ok(event_id)
    }

    /// 恢复笔记：只确认**当前已观察到**的删除事件，返回本次新确认的条数。
    ///
    /// 并发的新删除事件不在确认集合里，所以恢复之后它仍然生效（ADR 0001 §3.3）。
    pub fn restore_note(&self, node_id: &str, now: i64) -> Result<usize, StorageError> {
        let meta = self.node_meta(node_id)?;
        let events = meta.ensure_mergeable_list(KEY_DELETE_EVENTS)?;
        let acks = meta.ensure_mergeable_map(KEY_RESTORE_ACKS)?;

        let mut confirmed = 0usize;
        for index in 0..events.len() {
            let Some(value) = events.get(index) else {
                continue;
            };
            let LoroValue::String(event_id) = value.get_deep_value() else {
                continue;
            };
            if acks.get(event_id.as_str()).is_none() {
                acks.insert(event_id.as_str(), true)?;
                confirmed += 1;
            }
        }
        meta.insert(KEY_EDITED_AT, now)?;
        self.doc.commit();
        Ok(confirmed)
    }

    /// 节点当前是否有效删除。
    pub fn is_deleted(&self, node_id: &str) -> Result<bool, StorageError> {
        let meta = self.node_meta(node_id)?;
        let deep = meta.get_deep_value();
        Ok(effective_deleted(&deep))
    }

    /// 节点快照（含同级 dense rank）。
    pub fn node_snapshot(&self, node_id: &str) -> Result<NodeSnapshot, StorageError> {
        let node = self.parse_node(node_id)?;
        let parent = match self.tree.parent(node) {
            Some(loro::TreeParentId::Node(parent)) => Some(parent),
            _ => None,
        };
        let index = self
            .tree
            .children(parent)
            .unwrap_or_default()
            .iter()
            .position(|sibling| *sibling == node)
            .unwrap_or(0);
        self.build_snapshot(node, parent, index)
    }

    /// 整棵树 + 标签字典的快照（先序 DFS）。
    pub fn snapshot(&self) -> Result<StructureSnapshot, StorageError> {
        let mut nodes = Vec::new();
        self.walk(None, &mut nodes)?;
        Ok(StructureSnapshot {
            nodes,
            tags: self.tag_snapshots(),
        })
    }

    /// 标签字典快照（按 tag_id 排序，保证可比）。
    pub fn tag_snapshots(&self) -> Vec<TagSnapshot> {
        let deep = self.tag_dict.get_deep_value();
        let mut tags = Vec::new();
        if let LoroValue::Map(entries) = &deep {
            for (tag_id, value) in entries.iter() {
                let LoroValue::Map(fields) = value else {
                    continue;
                };
                let name = fields
                    .get(KEY_TAG_NAME)
                    .and_then(as_string)
                    .unwrap_or_default();
                let normalized_name = fields
                    .get(KEY_TAG_NORMALIZED)
                    .and_then(as_string)
                    .unwrap_or_else(|| normalize_tag(&name));
                tags.push(TagSnapshot {
                    tag_id: tag_id.clone(),
                    name,
                    normalized_name,
                });
            }
        }
        tags.sort_by(|a, b| a.tag_id.cmp(&b.tag_id));
        tags
    }

    fn walk(
        &self,
        parent: Option<TreeID>,
        out: &mut Vec<NodeSnapshot>,
    ) -> Result<(), StorageError> {
        let Some(children) = self.tree.children(parent) else {
            return Ok(());
        };
        for (index, node) in children.into_iter().enumerate() {
            out.push(self.build_snapshot(node, parent, index)?);
            self.walk(Some(node), out)?;
        }
        Ok(())
    }

    fn build_snapshot(
        &self,
        node: TreeID,
        parent: Option<TreeID>,
        index: usize,
    ) -> Result<NodeSnapshot, StorageError> {
        let meta = self.tree.get_meta(node)?;
        let deep = meta.get_deep_value();

        Ok(NodeSnapshot {
            node_id: node.to_string(),
            parent: parent.map(|parent| parent.to_string()),
            index,
            title: string_field(&deep, KEY_TITLE).unwrap_or_default(),
            note_id: string_field(&deep, KEY_NOTE_ID),
            body_doc_id: string_field(&deep, KEY_BODY_DOC_ID),
            created_at: i64_field(&deep, KEY_CREATED_AT).unwrap_or(0),
            edited_at: i64_field(&deep, KEY_EDITED_AT).unwrap_or(0),
            is_favorite: bool_field(&deep, KEY_FAVORITE).unwrap_or(false),
            is_pinned: bool_field(&deep, KEY_PINNED).unwrap_or(false),
            deleted: effective_deleted(&deep),
            delete_events: string_list_field(&deep, KEY_DELETE_EVENTS),
            confirmed_events: {
                let mut confirmed = map_keys_field(&deep, KEY_RESTORE_ACKS);
                confirmed.sort();
                confirmed
            },
            tags: {
                let mut tags = map_keys_field(&deep, KEY_TAGS);
                tags.sort();
                tags
            },
        })
    }

    fn node_meta(&self, node_id: &str) -> Result<LoroMap, StorageError> {
        let node = self.parse_node(node_id)?;
        self.tree.get_meta(node).map_err(StorageError::from)
    }

    fn parse_node(&self, node_id: &str) -> Result<TreeID, StorageError> {
        let node = TreeID::try_from(node_id).map_err(StorageError::from)?;
        if !self.tree.contains(node) {
            return Err(StorageError::UnknownNode {
                node_id: node_id.to_owned(),
            });
        }
        Ok(node)
    }

    fn find_tag(&self, normalized: &str) -> Option<String> {
        let deep = self.tag_dict.get_deep_value();
        let LoroValue::Map(entries) = &deep else {
            return None;
        };
        for (tag_id, value) in entries.iter() {
            let LoroValue::Map(fields) = value else {
                continue;
            };
            if fields
                .get(KEY_TAG_NORMALIZED)
                .and_then(as_string)
                .is_some_and(|existing| existing == normalized)
            {
                return Some(tag_id.clone());
            }
        }
        None
    }

    fn next_seq(&self, kind: &str) -> Result<u64, StorageError> {
        let key = format!("{kind}_seq_{}", self.doc.peer_id());
        let current = self
            .root_meta
            .get(&key)
            .and_then(|value| match value.get_deep_value() {
                LoroValue::I64(value) if value >= 0 => Some(value as u64),
                _ => None,
            })
            .unwrap_or(0);
        let next = current + 1;
        self.root_meta
            .insert(key.as_str(), i64::try_from(next).unwrap_or(i64::MAX))?;
        Ok(next)
    }
}

/// 标签归一化：去首尾空白、小写、内部空白折叠成单空格。
pub fn normalize_tag(name: &str) -> String {
    name.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// 有效删除 = 存在未被恢复确认的删除事件。
fn effective_deleted(deep: &LoroValue) -> bool {
    let events = string_list_field(deep, KEY_DELETE_EVENTS);
    let confirmed: std::collections::HashSet<String> =
        map_keys_field(deep, KEY_RESTORE_ACKS).into_iter().collect();
    events.iter().any(|event| !confirmed.contains(event))
}

fn as_string(value: &LoroValue) -> Option<String> {
    match value {
        LoroValue::String(value) => Some(value.to_string()),
        _ => None,
    }
}

fn field<'a>(deep: &'a LoroValue, key: &str) -> Option<&'a LoroValue> {
    match deep {
        LoroValue::Map(entries) => entries.get(key),
        _ => None,
    }
}

fn string_field(deep: &LoroValue, key: &str) -> Option<String> {
    field(deep, key).and_then(as_string)
}

fn i64_field(deep: &LoroValue, key: &str) -> Option<i64> {
    match field(deep, key) {
        Some(LoroValue::I64(value)) => Some(*value),
        Some(LoroValue::Double(value)) => Some(*value as i64),
        _ => None,
    }
}

fn bool_field(deep: &LoroValue, key: &str) -> Option<bool> {
    match field(deep, key) {
        Some(LoroValue::Bool(value)) => Some(*value),
        _ => None,
    }
}

fn string_list_field(deep: &LoroValue, key: &str) -> Vec<String> {
    match field(deep, key) {
        Some(LoroValue::List(items)) => items.iter().filter_map(as_string).collect(),
        _ => Vec::new(),
    }
}

fn map_keys_field(deep: &LoroValue, key: &str) -> Vec<String> {
    match field(deep, key) {
        Some(LoroValue::Map(entries)) => entries.keys().cloned().collect(),
        _ => Vec::new(),
    }
}
