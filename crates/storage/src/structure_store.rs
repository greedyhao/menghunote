//! 结构文档的落库与提交驱动：`loro_docs.kind='structure'` + A 事务 + 投影。

use crate::body_doc::{APP_DOC_SCHEMA, CODEC_VERSION, MIN_READER};
use crate::error::StorageError;
use crate::projector::{project, ProjectionReport};
use crate::store::{self, HeadState, UpdateRecord};
use crate::structure::{StructureDoc, STRUCTURE_DOC_ID};
use crate::Vault;
use rusqlite::types::Value;

/// 确保结构文档行存在（每 vault 一行，`kind = 'structure'`）。
pub fn ensure_structure_row(vault: &Vault) -> Result<(), StorageError> {
    vault.exec(
        "INSERT INTO loro_docs(doc_id, kind, generation, app_doc_schema, codec_version, min_reader) \
         VALUES(?1, 'structure', 1, ?2, ?3, ?4) ON CONFLICT(doc_id) DO NOTHING",
        &[
            Value::Text(STRUCTURE_DOC_ID.to_owned()),
            Value::Integer(APP_DOC_SCHEMA),
            Value::Text(CODEC_VERSION.to_owned()),
            Value::Integer(MIN_READER),
        ],
    )?;
    Ok(())
}

/// 从库里重放结构文档；不存在结构行时报 [`StorageError::Corrupt`]。
pub fn load_structure(
    vault: &Vault,
    peer_id: u64,
) -> Result<(StructureDoc, HeadState), StorageError> {
    let parts = store::load_replay_parts(vault, STRUCTURE_DOC_ID, None)?;
    let doc = StructureDoc::restore(peer_id, parts.snapshot.as_deref(), &parts.payloads)?;

    // 启动校验：重放 frontier 必须与库里一致（NULL 等价于空 frontier）
    let replayed = doc.frontiers().encode();
    let expected = store::encoded_frontier(&parts.head);
    if replayed != expected {
        return Err(StorageError::Corrupt {
            context: format!(
                "重放结构文档得到的 frontier 与库中记录不一致（len {} vs {}）",
                replayed.len(),
                expected.len()
            ),
        });
    }
    Ok((doc, parts.head))
}

/// 结构写入口：持有一份已重放的结构 doc 与它已 durable 的 head。
///
/// `commit` 做两件事：A 事务（结构 update + head + outbox），随后一次投影。
/// 投影幂等，所以即使中途崩溃，下一次 `commit`/[`crate::projector::project`] 也能追平。
#[derive(Debug)]
pub struct StructureWriter {
    vault: Vault,
    doc: StructureDoc,
    head: HeadState,
}

impl StructureWriter {
    /// 打开（或创建）结构文档。
    pub fn open(vault: &Vault, peer_id: u64) -> Result<Self, StorageError> {
        ensure_structure_row(vault)?;
        let (doc, head) = load_structure(vault, peer_id)?;
        Ok(Self {
            vault: vault.clone(),
            doc,
            head,
        })
    }

    /// 当前结构 doc（就地编辑后调用 [`StructureWriter::commit`]）。
    pub fn doc(&self) -> &StructureDoc {
        &self.doc
    }

    /// 已 durable 的 head。
    pub fn head(&self) -> &HeadState {
        &self.head
    }

    /// 提交结构改动：A 事务 + 投影。没有任何新增 op 时只补投影。
    pub fn commit(&mut self, batch_id: &str) -> Result<ProjectionReport, StorageError> {
        let base = self.head.version_vector()?;

        // 用 VV 判断「有没有新 op」：空的 frontier 编码不是零字节，直接比 bytes 会误判
        if self.doc.vv() != base {
            let update = self.doc.pending_update(&base)?;
            let new_head = store::append_updates(
                &self.vault,
                STRUCTURE_DOC_ID,
                self.head.head_seq,
                &[UpdateRecord::new(batch_id, update)],
            )?;
            self.head = new_head;
        }

        let report = project(&self.vault, &self.doc, self.head.head_seq)?;
        // 整棵树投完再发一条"结构变了"：按单篇发会让订阅者重复全量对账，
        // 而结构提交本来就是整棵树投影，`note_id = None` 更诚实。
        self.vault
            .publish_event(crate::StorageEvent::StructureProjected { note_id: None });
        Ok(report)
    }
}
