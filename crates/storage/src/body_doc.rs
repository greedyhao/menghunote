//! 每个正文 Doc 一个 `LoroDoc`（ADR 0001 §3.1 / §3.4）。
//!
//! 约定：
//!
//! - 正文只存**原始 Markdown**（LF 归一），不用 Loro Rich Text，不把块树落库；
//! - 对外编辑接口的 `pos` / `len` 一律是 **Unicode scalar**（Loro 实测单位；
//!   通用换算的唯一实现在 [`menghunote_text`]，实测结论见 `tests/loro_units.rs`）；
//!   `convert_pos` 对半截索引**向下取整**、`to_delta()` **只产出 Insert** 这两个陷阱也记在那里，
//!   适配逻辑留在本文件，不搬进 text（ADR 0003：text 只管通用单位，不管 Loro API 语义）；
//!   IME 需要 UTF-16 时用 [`BodyDoc::insert_utf16`] / [`BodyDoc::delete_utf16`] 的显式接口；
//! - 编辑即映射：一次改动就是 `delete(range) + insert(text)`（ADR 0001 §3.4），
//!   过期基准不许直接应用（基准校验由调用方的 `command`/frontier 负责，本层只校验范围）；
//! - 导出增量用「上次 durable 的 VV」作起点，**不在本层推进任何 watermark**。

use loro::cursor::{Cursor, Side};
use loro::{ExportMode, Frontiers, LoroDoc, LoroText, TextDelta, UndoManager, VersionVector};

use crate::error::StorageError;
use menghunote_text::CoordError;

/// 正文容器在 `LoroDoc` 里的名字。
pub const BODY_CONTAINER: &str = "body";

/// 落库时记录的 codec 标识（ADR 0001 §3.9「每个 snapshot/update 携带实际 codec 标识」）。
pub const CODEC_VERSION: &str = "loro-1.17.1";

/// 本切片写入的 `app_doc_schema`。
pub const APP_DOC_SCHEMA: i64 = 1;

/// 本切片要求的最低读取方版本。
pub const MIN_READER: i64 = 1;

/// 一次本地提交导出出的增量。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingUpdate {
    /// Loro update 字节（相对导出基准的增量）。
    pub payload: Vec<u8>,
    /// 该增量应用后的 frontier（`Frontiers::encode`）。
    pub end_frontier: Vec<u8>,
    /// 该增量应用后的 VV（`VersionVector::encode`）。
    pub end_vv: Vec<u8>,
}

/// 一个正文 CRDT 文档。
pub struct BodyDoc {
    doc_id: String,
    doc: LoroDoc,
    text: LoroText,
}

impl std::fmt::Debug for BodyDoc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BodyDoc")
            .field("doc_id", &self.doc_id)
            .field("scalars", &self.scalar_len())
            .finish()
    }
}

impl BodyDoc {
    /// 新建文档并设置本机 peer id。
    ///
    /// peer id 必须每设备唯一（ADR 0001 §3.2）；克隆旧备份当新实例时必须换新 peer id。
    pub fn create(doc_id: impl Into<String>, peer_id: u64) -> Result<Self, StorageError> {
        let doc = LoroDoc::new();
        doc.set_peer_id(peer_id)?;
        let text = doc.get_text(BODY_CONTAINER);
        Ok(Self {
            doc_id: doc_id.into(),
            doc,
            text,
        })
    }

    /// 从快照 + 后续 update 重放出一个文档（启动恢复路径）。
    pub fn restore(
        doc_id: impl Into<String>,
        peer_id: u64,
        snapshot: Option<&[u8]>,
        updates: &[Vec<u8>],
    ) -> Result<Self, StorageError> {
        let this = Self::create(doc_id, peer_id)?;
        if let Some(snapshot) = snapshot {
            this.doc.import(snapshot)?;
        }
        for payload in updates {
            this.doc.import(payload)?;
        }
        Ok(this)
    }

    /// 文档 ID。
    pub fn doc_id(&self) -> &str {
        &self.doc_id
    }

    /// 当前正文（原始 Markdown）。
    pub fn text(&self) -> String {
        self.text.to_string()
    }

    /// 以 Unicode scalar 计的正文长度（Loro 实测单位）。
    pub fn scalar_len(&self) -> usize {
        self.text.len_unicode()
    }

    /// 以 UTF-16 code unit 计的正文长度。
    pub fn utf16_len(&self) -> usize {
        self.text.len_utf16()
    }

    /// 以 UTF-8 字节计的正文长度。
    pub fn byte_len(&self) -> usize {
        self.text.len_utf8()
    }

    /// 按 **Unicode scalar** 位置插入文本。越界报错，不截断。
    pub fn insert_scalars(&self, pos: usize, text: &str) -> Result<(), StorageError> {
        self.check_range(pos, 0)?;
        self.text.insert(pos, text)?;
        self.doc.commit();
        Ok(())
    }

    /// 按 **Unicode scalar** 位置删除 `len` 个字符。
    pub fn delete_scalars(&self, pos: usize, len: usize) -> Result<(), StorageError> {
        self.check_range(pos, len)?;
        self.text.delete(pos, len)?;
        self.doc.commit();
        Ok(())
    }

    /// 替换 `[pos, pos + len)` 的 scalar 区间（IME 确认与格式替换都走这一个入口）。
    pub fn replace_scalars(&self, pos: usize, len: usize, text: &str) -> Result<(), StorageError> {
        self.check_range(pos, len)?;
        if len > 0 {
            self.text.delete(pos, len)?;
        }
        if !text.is_empty() {
            self.text.insert(pos, text)?;
        }
        self.doc.commit();
        Ok(())
    }

    /// 应用一段 delta（`Retain` / `Insert` / `Delete` 的计数单位实测为 Unicode scalar）。
    pub fn apply_delta_scalars(&self, delta: &[TextDelta]) -> Result<(), StorageError> {
        self.text.apply_delta(delta)?;
        self.doc.commit();
        Ok(())
    }

    /// 按 **UTF-16** 位置插入；落在代理对中间会报错（非法半代理对位置）。
    pub fn insert_utf16(&self, pos: usize, text: &str) -> Result<(), StorageError> {
        self.text.insert_utf16(pos, text)?;
        self.doc.commit();
        Ok(())
    }

    /// 按 **UTF-16** 位置删除；落在代理对中间会报错。
    pub fn delete_utf16(&self, pos: usize, len: usize) -> Result<(), StorageError> {
        self.text.delete_utf16(pos, len)?;
        self.doc.commit();
        Ok(())
    }

    /// 取 `pos` 处的相对光标；`pos` 单位实测为 Unicode scalar。
    pub fn cursor_at_scalar(&self, pos: usize) -> Option<Cursor> {
        self.text.get_cursor(pos, Side::Middle)
    }

    /// 把光标解析成当前版本的 scalar 位置（会随并发编辑移动）。
    pub fn resolve_cursor_scalar(&self, cursor: &Cursor) -> Result<usize, StorageError> {
        self.doc
            .get_cursor_pos(cursor)
            .map(|pos| pos.current.pos)
            .map_err(|error| StorageError::CursorUnresolved(error.to_string()))
    }

    /// 设置**下一次提交**的 origin（撤销分组 / 后台变更排除用）。
    pub fn set_next_commit_origin(&self, origin: &str) {
        self.doc.set_next_commit_origin(origin);
    }

    /// 撤销管理器（本 peer 栈，ADR 0002 §8）。
    pub fn undo_manager(&self) -> UndoManager {
        UndoManager::new(&self.doc)
    }

    /// 当前 frontier。
    pub fn frontiers(&self) -> Frontiers {
        self.doc.state_frontiers()
    }

    /// 当前 oplog 版本向量。
    pub fn vv(&self) -> VersionVector {
        self.doc.oplog_vv()
    }

    /// 自 `from` 之后的新增 ops（A 事务要落库的增量）。
    pub fn pending_update(&self, from: &VersionVector) -> Result<PendingUpdate, StorageError> {
        let payload = self.doc.export(ExportMode::updates(from))?;
        Ok(PendingUpdate {
            payload,
            end_frontier: self.frontiers().encode(),
            end_vv: self.vv().encode(),
        })
    }

    /// 全量历史 update（对端首次同步用）。
    pub fn export_all_updates(&self) -> Result<Vec<u8>, StorageError> {
        Ok(self.doc.export(ExportMode::all_updates())?)
    }

    /// 完整快照。
    pub fn export_snapshot(&self) -> Result<Vec<u8>, StorageError> {
        Ok(self.doc.export(ExportMode::Snapshot)?)
    }

    /// 导入一个 update 或 snapshot（幂等；重复导入不会产生新 ops）。
    pub fn import(&self, bytes: &[u8]) -> Result<(), StorageError> {
        self.doc.import(bytes)?;
        Ok(())
    }

    /// 基线 merge 专用：把增量只合进 **oplog**（detached），不触发 DocState 的
    /// diff 计算与状态应用。
    ///
    /// 语义等价性：CRDT 的 oplog 合并与 state 应用是同一组 op；detached 只跳过
    /// 「把新 op 应用到容器状态」这一步，oplog 的 VV/frontier 与 attached 完全一致。
    /// 调用方只能使用 [`Self::pending_update_from_oplog`] / [`Self::oplog_frontiers_encoded`]
    /// 这类纯 oplog 接口，**不得**在 detached 之后读正文。
    ///
    /// 为什么需要它：对「已有本地内容」的文档，Loro 的 attached import 会重算
    /// diff 并把大文本重新应用到 state，成本随总文本长度超线性增长
    /// （5→20 MiB 实测 ≈15–17×，见 `phase1a_release` 探针）；baseline merge 只需要
    /// 「oplog 并集 + 导出差集」，detached 路径把这部分成本降到 O(增量)。
    pub(crate) fn import_oplog_only(&self, bytes: &[u8]) -> Result<(), StorageError> {
        if !self.doc.is_detached() {
            self.doc.detach();
        }
        self.doc.import(bytes)?;
        Ok(())
    }

    /// detached 模式下合并后的 oplog frontier（attached 等价物 = [`Self::frontiers`]）。
    pub(crate) fn oplog_frontiers_encoded(&self) -> Vec<u8> {
        self.doc.oplog_frontiers().encode()
    }

    /// 从 oplog 导出 `from` 之后的增量；`end_frontier`/`end_vv` 取 **oplog** 版本。
    ///
    /// 仅用于已 [`Self::import_oplog_only`] 的文档；对 attached 文档它与
    /// [`Self::pending_update`] 结果一致（state 在 latest 时两者相同）。
    pub(crate) fn pending_update_from_oplog(
        &self,
        from: &VersionVector,
    ) -> Result<PendingUpdate, StorageError> {
        let payload = self.doc.export(ExportMode::updates(from))?;
        Ok(PendingUpdate {
            payload,
            end_frontier: self.oplog_frontiers_encoded(),
            end_vv: self.doc.oplog_vv().encode(),
        })
    }

    /// 校验 `[pos, pos + len)` 在当前文本内（scalar 口径），避免 Loro 抛出的越界信息丢上下文。
    ///
    /// 坐标错误直接复用 `menghunote_text::CoordError`（ADR 0003：全仓只有一份）。
    fn check_range(&self, pos: usize, len: usize) -> Result<(), StorageError> {
        let total = self.scalar_len();
        let end = pos.checked_add(len).ok_or(CoordError::ScalarOutOfBounds {
            index: usize::MAX,
            len: total,
        })?;
        if end > total {
            return Err(CoordError::ScalarOutOfBounds {
                index: end,
                len: total,
            }
            .into());
        }
        Ok(())
    }
}
