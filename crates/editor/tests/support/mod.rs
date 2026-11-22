//! 协议事件夹具：直接构造 `storage::session` 的冻结类型，**不启动真实 storage**
//! （ADR 0003 §7 切片 3 的边界；端到端在切片 4）。
//!
//! 各测试按需使用其中一部分函数，因此对未使用项关闭 dead_code 提示。

#![allow(dead_code)]

use menghunote_editor::UnicodeScalar;
use menghunote_storage::session::{
    Applied, BatchId, ChangeSource, DeltaSplice, EditOutcome, Generation, LiveDelta, LiveRev,
    MergeInfo, OpenSnapshot, SessionEpoch,
};

/// 夹具文档 ID。
pub const DOC_ID: &str = "body-note-1";
/// 夹具打开时的 live 修订号。
pub const BASE_REV: u64 = 7;

/// 打开快照：LF 归一的 Markdown 原文 + 单调起点。
pub fn snapshot(markdown: &str) -> OpenSnapshot {
    OpenSnapshot {
        doc_id: DOC_ID.to_owned(),
        generation: Generation(3),
        session_epoch: SessionEpoch(9),
        live_rev: LiveRev(BASE_REV),
        live_frontier: vec![0x10, 0x20],
        live_vv: vec![0x30],
        durable_batch: Some(BatchId(2)),
        durable_head_seq: 5,
        markdown: markdown.to_owned(),
    }
}

/// 一条 scalar 单位的 splice。
pub fn splice(at: usize, delete_scalars: u64, insert: &str) -> DeltaSplice {
    DeltaSplice {
        at: UnicodeScalar::new(at),
        delete_scalars,
        insert: insert.to_owned(),
    }
}

/// 一次本地命令的确认（`local_echo` 决定镜像是否应再应用 splice）。
pub fn applied(
    command_id: &str,
    live_rev: u64,
    local_echo: bool,
    splice: DeltaSplice,
) -> EditOutcome {
    EditOutcome::Applied(Applied {
        command_id: command_id.to_owned(),
        live_rev: LiveRev(live_rev),
        live_frontier: vec![live_rev as u8],
        splice,
        local_echo,
        source: ChangeSource::Local,
        merge: MergeInfo::default(),
    })
}

/// 远端 / 后台的一条 live 变更。
pub fn live_delta(from_rev: u64, to_rev: u64, splice: DeltaSplice) -> LiveDelta {
    LiveDelta {
        doc_id: DOC_ID.to_owned(),
        from_rev: LiveRev(from_rev),
        to_rev: LiveRev(to_rev),
        live_frontier: vec![to_rev as u8],
        splice,
        cause: None,
        source: ChangeSource::Remote,
        merge: MergeInfo {
            concurrent: true,
            remote_changes: 1,
        },
    }
}
