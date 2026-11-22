//! 结构式投影器：结构 `LoroDoc` → `tree` / `notes` / `tags` / `note_tags`。
//!
//! 规则（ADR 0001 §3.1）：
//!
//! - `tree` / `notes` / `tags` / `note_tags` **只是投影**，权威在结构 CRDT；
//! - 投影可幂等重建：同一结构 doc 跑两遍，结果必须完全一致（除了 `updated_at` 取 MAX 不倒退）；
//! - 笔记行**永不删除**（墓碑在 `deleted` 列），默认查询按 `deleted = 0` 过滤；
//! - 不与 B 物化抢列：本模块只写结构归属的列（title / created_at / updated_at / deleted /
//!   is_favorite / is_pinned / meta_seq），**绝不碰** `body_md` / `body_seq` / `body_frontier`。

use rusqlite::types::Value;

use crate::actor::Statement;
use crate::body_doc::{APP_DOC_SCHEMA, CODEC_VERSION, MIN_READER};
use crate::error::StorageError;
use crate::structure::StructureDoc;
use crate::Vault;

/// 一次投影的统计。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProjectionReport {
    /// 树节点数（含非笔记节点）。
    pub nodes: usize,
    /// 笔记数。
    pub notes: usize,
    /// 标签字典项数。
    pub tags: usize,
    /// 笔记 × 标签成员关系数。
    pub tag_links: usize,
}

/// 一条可见笔记（默认查询用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteView {
    /// 笔记 ID。
    pub note_id: String,
    /// 标题。
    pub title: String,
    /// 正文文档 ID。
    pub body_doc_id: String,
    /// 更新时间。
    pub updated_at: i64,
    /// 收藏。
    pub is_favorite: bool,
    /// 置顶。
    pub is_pinned: bool,
    /// 父节点（根级为 None）。
    pub parent_id: Option<String>,
    /// 同级 rank。
    pub sibling_rank: i64,
}

/// 把结构 doc 投影到 SQL。
///
/// 整次投影在**一个 SQLite 事务**里完成：一方面原子，另一方面 `tree.parent_id` 是
/// `DEFERRABLE INITIALLY DEFERRED`，同一事务内先插子后插父也能在提交时通过外键检查。
pub fn project(
    vault: &Vault,
    doc: &StructureDoc,
    meta_seq: u64,
) -> Result<ProjectionReport, StorageError> {
    let snapshot = doc.snapshot()?;
    let mut statements: Vec<Statement> = Vec::new();
    let mut notes = 0usize;
    let mut tag_links = 0usize;

    // 1. 每篇笔记的正文文档行（kind='body'），幂等
    for node in &snapshot.nodes {
        let (Some(_note_id), Some(body_doc_id)) = (&node.note_id, &node.body_doc_id) else {
            continue;
        };
        notes += 1;
        statements.push(Statement::new(
            "INSERT INTO loro_docs(doc_id, kind, generation, app_doc_schema, codec_version, min_reader) \
             VALUES(?1, 'body', 1, ?2, ?3, ?4) ON CONFLICT(doc_id) DO NOTHING",
            vec![
                Value::Text(body_doc_id.clone()),
                Value::Integer(APP_DOC_SCHEMA),
                Value::Text(CODEC_VERSION.to_owned()),
                Value::Integer(MIN_READER),
            ],
        ));
    }

    // 2. 全局标签字典
    for tag in &snapshot.tags {
        statements.push(Statement::new(
            "INSERT INTO tags(tag_id, name, normalized_name) VALUES(?1, ?2, ?3) \
             ON CONFLICT(tag_id) DO UPDATE SET name = excluded.name, normalized_name = excluded.normalized_name",
            vec![
                Value::Text(tag.tag_id.clone()),
                Value::Text(tag.name.clone()),
                Value::Text(tag.normalized_name.clone()),
            ],
        ));
    }

    // 3. notes：只写结构归属列；updated_at 取 MAX 不倒退
    for node in &snapshot.nodes {
        let (Some(note_id), Some(body_doc_id)) = (&node.note_id, &node.body_doc_id) else {
            continue;
        };
        statements.push(Statement::new(
            "INSERT INTO notes(note_id, body_doc_id, title, created_at, updated_at, deleted, is_favorite, is_pinned, meta_seq) \
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) \
             ON CONFLICT(note_id) DO UPDATE SET \
               body_doc_id = excluded.body_doc_id, \
               title = excluded.title, \
               updated_at = MAX(notes.updated_at, excluded.updated_at), \
               deleted = excluded.deleted, \
               is_favorite = excluded.is_favorite, \
               is_pinned = excluded.is_pinned, \
               meta_seq = MAX(notes.meta_seq, excluded.meta_seq)",
            vec![
                Value::Text(note_id.clone()),
                Value::Text(body_doc_id.clone()),
                Value::Text(node.title.clone()),
                Value::Integer(node.created_at),
                Value::Integer(node.edited_at.max(node.created_at)),
                Value::Integer(if node.deleted { 1 } else { 0 }),
                Value::Integer(if node.is_favorite { 1 } else { 0 }),
                Value::Integer(if node.is_pinned { 1 } else { 0 }),
                Value::Integer(i64_of(meta_seq)?),
            ],
        ));
    }

    // 4. tree：所有节点（含非笔记节点），node_id / parent / rank / 墓碑
    for node in &snapshot.nodes {
        statements.push(Statement::new(
            "INSERT INTO tree(node_id, parent_id, note_id, title, sibling_rank, deleted, meta_seq) \
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7) \
             ON CONFLICT(node_id) DO UPDATE SET \
               parent_id = excluded.parent_id, \
               note_id = excluded.note_id, \
               title = excluded.title, \
               sibling_rank = excluded.sibling_rank, \
               deleted = excluded.deleted, \
               meta_seq = excluded.meta_seq",
            vec![
                Value::Text(node.node_id.clone()),
                match &node.parent {
                    Some(parent) => Value::Text(parent.clone()),
                    None => Value::Null,
                },
                match &node.note_id {
                    Some(note_id) => Value::Text(note_id.clone()),
                    None => Value::Null,
                },
                Value::Text(node.title.clone()),
                Value::Integer(i64::try_from(node.index).map_err(|_| StorageError::Corrupt {
                    context: "sibling rank 超出 i64 范围".to_owned(),
                })?),
                Value::Integer(if node.deleted { 1 } else { 0 }),
                Value::Integer(i64_of(meta_seq)?),
            ],
        ));
    }

    // 5. note_tags：只重建**派生**成员关系（权威在 CRDT 的标签集合里；笔记行永不删除）
    for node in &snapshot.nodes {
        let Some(note_id) = &node.note_id else {
            continue;
        };
        statements.push(Statement::new(
            "DELETE FROM note_tags WHERE note_id = ?1",
            vec![Value::Text(note_id.clone())],
        ));
        for tag_id in &node.tags {
            tag_links += 1;
            statements.push(Statement::new(
                "INSERT INTO note_tags(note_id, tag_id) VALUES(?1, ?2) ON CONFLICT DO NOTHING",
                vec![Value::Text(note_id.clone()), Value::Text(tag_id.clone())],
            ));
        }
    }

    vault.exec_transaction(&statements, &[])?;

    Ok(ProjectionReport {
        nodes: snapshot.nodes.len(),
        notes,
        tags: snapshot.tags.len(),
        tag_links,
    })
}

/// 默认查询：只返回未删除的笔记（`notes_recent` 部分索引的语义）。
pub fn visible_notes(vault: &Vault) -> Result<Vec<NoteView>, StorageError> {
    query_notes(vault, false)
}

/// 回收站查询：只返回已删除的笔记。
pub fn trashed_notes(vault: &Vault) -> Result<Vec<NoteView>, StorageError> {
    query_notes(vault, true)
}

fn query_notes(vault: &Vault, deleted: bool) -> Result<Vec<NoteView>, StorageError> {
    let rows = vault.query(
        "SELECT n.note_id, n.title, n.body_doc_id, n.updated_at, n.is_favorite, n.is_pinned, \
                t.parent_id, t.sibling_rank \
         FROM notes n LEFT JOIN tree t ON t.note_id = n.note_id \
         WHERE n.deleted = ?1 \
         ORDER BY n.is_pinned DESC, n.is_favorite DESC, n.updated_at DESC, n.note_id",
        &[Value::Integer(if deleted { 1 } else { 0 })],
    )?;

    let mut notes = Vec::with_capacity(rows.len());
    for row in &rows {
        notes.push(NoteView {
            note_id: text_of(row, 0, "notes.note_id")?,
            title: text_of(row, 1, "notes.title")?,
            body_doc_id: text_of(row, 2, "notes.body_doc_id")?,
            updated_at: integer_of(row, 3, "notes.updated_at")?,
            is_favorite: flag_of(row, 4, "notes.is_favorite")?,
            is_pinned: flag_of(row, 5, "notes.is_pinned")?,
            parent_id: match row.get(6) {
                Some(Value::Text(parent)) => Some(parent.clone()),
                Some(Value::Null) | None => None,
                other => {
                    return Err(StorageError::Corrupt {
                        context: format!("tree.parent_id 读回异常：{other:?}"),
                    });
                }
            },
            sibling_rank: integer_of(row, 7, "tree.sibling_rank")?,
        });
    }
    Ok(notes)
}

fn i64_of(value: u64) -> Result<i64, StorageError> {
    i64::try_from(value).map_err(|_| StorageError::Corrupt {
        context: format!("数值 {value} 超出 SQLite INTEGER 范围"),
    })
}

fn text_of(row: &[Value], index: usize, field: &str) -> Result<String, StorageError> {
    match row.get(index) {
        Some(Value::Text(text)) => Ok(text.clone()),
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

fn flag_of(row: &[Value], index: usize, field: &str) -> Result<bool, StorageError> {
    Ok(integer_of(row, index, field)? != 0)
}
