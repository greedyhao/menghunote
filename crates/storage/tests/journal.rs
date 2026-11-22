//! 未 ACK journal 与背压：连续前缀、durable 后裁剪、慢盘合并、失败不丢已 ACK 部分。
//!
//! 另覆盖 P1-3 的 head 过期契约（见 `crates/storage/src/journal.rs` 模块头）：
//! 外部推进 head 后 flush 必须返回**可识别**的终态错误而不是裸 `HeadConflict` 或挂死；
//! 按契约重建 sink 后新条目可正常 durable；旧 journal 未 durable 条目被**显式交还**，
//! 既不写库也不静默丢弃。

mod support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use loro::VersionVector;
use support::*;

use menghunote_storage::{
    append_updates, load_body_doc, load_head, DurableSink, FlushReport, JournalEntry,
    JournalOrigin, JournalWriter, StorageError, UpdateRecord, VaultJournalSink,
};

/// 可注入慢盘/失败的 sink：记录每次 flush 的批大小，便于断言「合并」。
#[derive(Debug, Default)]
struct ScriptedSink {
    /// 每次 write_prefix 收到的条数。
    batches: Vec<usize>,
    /// 剩余几次要注入失败。
    fail_times: usize,
    /// 已 durable 的条目（按到达顺序）。
    durable: Vec<JournalEntry>,
    /// 已 durable 的条数（充作 seq）。
    seq: u64,
}

impl ScriptedSink {
    fn with_failures(fail_times: usize) -> Self {
        Self {
            fail_times,
            ..Self::default()
        }
    }
}

impl DurableSink for ScriptedSink {
    fn write_prefix(&mut self, entries: &[JournalEntry]) -> Result<u64, StorageError> {
        self.batches.push(entries.len());
        if self.fail_times > 0 {
            self.fail_times -= 1;
            // 注入失败：一个字节都不写（模拟事务回滚）
            return Err(StorageError::Corrupt {
                context: "注入的磁盘失败".to_owned(),
            });
        }
        for entry in entries {
            self.durable.push(entry.clone());
            self.seq += 1;
        }
        Ok(self.seq)
    }

    fn head_seq(&self) -> u64 {
        self.seq
    }
}

/// 计数包装：证明 stale 之后 writer **不再**触碰 sink（不重试）。
#[derive(Debug)]
struct CountingSink {
    inner: VaultJournalSink,
    attempts: Arc<AtomicUsize>,
}

impl DurableSink for CountingSink {
    fn write_prefix(&mut self, entries: &[JournalEntry]) -> Result<u64, StorageError> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        self.inner.write_prefix(entries)
    }

    fn head_seq(&self) -> u64 {
        self.inner.head_seq()
    }
}

fn entry(tag: &str) -> JournalEntry {
    JournalEntry {
        doc_id: "doc-1".to_owned(),
        batch_id: format!("batch-{tag}"),
        update: menghunote_storage::PendingUpdate {
            payload: tag.as_bytes().to_vec(),
            end_frontier: vec![0],
            end_vv: vec![0],
        },
        origin: JournalOrigin::Local,
    }
}

#[test]
fn journal_keeps_fifo_order_and_acks_contiguous_prefix() {
    let mut journal = menghunote_storage::PendingJournal::new(4);
    for tag in ["a", "b", "c"] {
        journal.append(entry(tag)).expect("append");
    }
    assert_eq!(journal.pending_len(), 3);

    // 只取前缀，不改变 journal
    let prefix: Vec<String> = journal
        .peek_prefix(2)
        .iter()
        .map(|item| item.batch_id.clone())
        .collect();
    assert_eq!(prefix, ["batch-a", "batch-b"]);
    assert_eq!(journal.pending_len(), 3, "peek 不是出队");

    journal.ack_prefix(2).expect("裁剪前缀");
    assert_eq!(journal.pending_len(), 1);
    assert_eq!(journal.peek_prefix(4)[0].batch_id, "batch-c");

    // 不能裁剪超过未 ACK 条数
    assert!(journal.ack_prefix(2).is_err());
}

#[test]
fn journal_backpressure_rejects_when_full_and_never_grows_unbounded() {
    let mut journal = menghunote_storage::PendingJournal::new(3);
    for tag in ["a", "b", "c"] {
        journal.append(entry(tag)).expect("前三条应接受");
    }
    assert!(journal.is_full());

    // 满了之后必须报背压，且**不写入**（无界增长被挡住）
    for tag in ["d", "e", "f"] {
        let error = journal.append(entry(tag)).expect_err("满容量必须背压");
        assert!(
            matches!(error, StorageError::WriteBackpressure { capacity: 3 }),
            "期望 WriteBackpressure，实际 {error}"
        );
        assert_eq!(journal.pending_len(), 3, "被拒的条目不能进 journal");
    }
    assert_eq!(journal.capacity(), 3);
}

#[test]
fn slow_io_coalesces_pending_prefix_into_one_flush() {
    let sink = ScriptedSink::default();
    let mut writer = JournalWriter::new(sink, 8, 8);

    // 慢盘时调用方会先攒着：一次 flush 应把整段前缀合并成一次写
    for tag in ["a", "b", "c", "d", "e"] {
        writer.submit(entry(tag)).expect("submit");
    }
    let report = writer.flush().expect("flush");
    assert_eq!(
        report,
        FlushReport {
            entries: 5,
            durable_seq: 5
        }
    );
    assert_eq!(writer.pending_len(), 0);
    assert_eq!(writer.durable_seq(), 5);
    assert_eq!(writer.flushed_entries(), 5);

    let sink = writer.into_sink();
    assert_eq!(sink.batches, [5], "5 条必须合并成一次 sink 写");
    assert_eq!(sink.durable.len(), 5);
    assert_eq!(sink.durable[0].batch_id, "batch-a", "顺序不能乱");
}

#[test]
fn failed_flush_loses_nothing_and_retries_to_the_same_prefix() {
    let sink = ScriptedSink::with_failures(2);
    let mut writer = JournalWriter::new(sink, 8, 8);
    for tag in ["a", "b", "c"] {
        writer.submit(entry(tag)).expect("submit");
    }

    // 连续两次注入失败：一条都不能少（sink 原子回滚）
    for _ in 0..2 {
        let error = writer.flush().expect_err("注入的失败应上报");
        assert!(matches!(error, StorageError::Corrupt { .. }));
        assert_eq!(writer.pending_len(), 3, "失败后 journal 原样保留");
        assert_eq!(writer.durable_seq(), 0, "失败不得推进 durable 水位");
    }

    // 第三次成功：仍然是从头开始的同一前缀
    let report = writer.flush().expect("第三次 flush");
    assert_eq!(report.entries, 3);
    assert_eq!(writer.pending_len(), 0);
    let sink = writer.into_sink();
    assert_eq!(sink.batches, [3, 3, 3], "每次失败也尝试了同一批前缀");
    assert_eq!(
        sink.durable
            .iter()
            .map(|e| e.batch_id.as_str())
            .collect::<Vec<_>>(),
        ["batch-a", "batch-b", "batch-c"]
    );
}

#[test]
fn max_batch_splits_prefix_without_gaps() {
    let sink = ScriptedSink::default();
    let mut writer = JournalWriter::new(sink, 8, 2);
    for tag in ["a", "b", "c", "d", "e"] {
        writer.submit(entry(tag)).expect("submit");
    }

    assert_eq!(writer.flush().expect("1").entries, 2);
    assert_eq!(writer.pending_len(), 3);
    let total = writer.flush_all().expect("排空");
    assert_eq!(total, 3);
    assert_eq!(writer.pending_len(), 0);

    let sink = writer.into_sink();
    assert_eq!(sink.batches, [2, 2, 1], "按 max_batch 切片，无空洞");
    assert_eq!(
        sink.durable
            .iter()
            .map(|e| e.batch_id.as_str())
            .collect::<Vec<_>>(),
        ["batch-a", "batch-b", "batch-c", "batch-d", "batch-e"]
    );
}

#[test]
fn unacked_suffix_is_lost_but_acked_prefix_survives_reopen() {
    // 真实落库：A 事务 + 崩溃（丢掉 writer 与 vault）
    let (dir, vault) = fresh_vault();
    let doc = seed_note(&vault, "note-1", "doc-1");

    let mut base = VersionVector::default();
    let mut entries = Vec::new();
    for (index, ch) in ["甲", "乙", "丙"].iter().enumerate() {
        doc.insert_scalars(doc.scalar_len(), ch).expect("编辑");
        let update = doc.pending_update(&base).expect("导出增量");
        base = VersionVector::decode(&update.end_vv).expect("推进基准");
        entries.push(JournalEntry {
            doc_id: "doc-1".to_owned(),
            batch_id: format!("batch-{index}"),
            update,
            origin: JournalOrigin::Local,
        });
    }

    let sink = VaultJournalSink::new(vault.clone(), "doc-1", 0);
    let mut writer = JournalWriter::new(sink, 4, 2);
    for entry in entries {
        writer.submit(entry).expect("submit");
    }
    // 只 flush 前 2 条（max_batch=2），第三条停在内存里
    let report = writer.flush().expect("flush 前缀");
    assert_eq!(report.entries, 2);
    assert_eq!(writer.pending_len(), 1, "第三条仍是未 ACK");
    assert_eq!(writer.durable_seq(), 2);

    // 崩溃：未 ACK 的第三条随内存一起丢
    drop(writer);
    drop(vault);

    let vault = open_vault(&vault_path(&dir));
    assert_eq!(
        count(
            &vault,
            "SELECT count(*) FROM loro_updates WHERE doc_id = 'doc-1'",
            &[]
        ),
        2,
        "只有已 ACK 的两条落库"
    );
    assert_eq!(load_head(&vault, "doc-1").expect("head").head_seq, 2);

    let (replayed, _) = load_body_doc(&vault, "doc-1", PEER_A, None).expect("重放");
    assert_eq!(
        replayed.text(),
        "甲乙",
        "只丢未 ACK 的连续前缀之后，已 ACK 部分完好"
    );
}

#[test]
fn sink_holds_head_watermark_and_rejects_mixed_docs() {
    let (_dir, vault) = fresh_vault();
    let doc = seed_note(&vault, "note-1", "doc-1");

    let base = VersionVector::default();
    doc.insert_scalars(0, "甲").expect("编辑");
    let update = doc.pending_update(&base).expect("增量");

    let mut sink = VaultJournalSink::new(vault.clone(), "doc-1", 0);
    assert_eq!(sink.head_seq(), 0);
    let first = JournalEntry {
        doc_id: "doc-1".to_owned(),
        batch_id: "b1".to_owned(),
        update: update.clone(),
        origin: JournalOrigin::Local,
    };
    let seq = sink
        .write_prefix(std::slice::from_ref(&first))
        .expect("写 durable");
    assert_eq!(seq, 1);
    assert_eq!(sink.head_seq(), 1, "sink 内部水位必须跟着推进");

    // 混入别的文档：必须拒绝（否则 seq 会串到别的 doc 上）
    let mixed = [
        first,
        JournalEntry {
            doc_id: "doc-other".to_owned(),
            batch_id: "b1".to_owned(),
            update,
            origin: JournalOrigin::Local,
        },
    ];
    let error = sink.write_prefix(&mixed).expect_err("混文档必须拒绝");
    assert!(matches!(error, StorageError::Corrupt { .. }));
}

/// P1-3：外部推进 head 后，flush 必须给出**可识别**的终态错误，不重试、不挂死，
/// 且旧条目既没写库也没被静默丢掉；按契约重建 sink 后新条目可正常 durable。
#[test]
fn stale_head_is_terminal_then_rebuild_hands_back_entries_and_new_entries_flush() {
    let (_dir, vault) = fresh_vault();
    let doc = seed_note(&vault, "note-1", "doc-1");

    // 本地编辑：按**空基准**导出，这就是 writer 手里的字节（追加时的 head 快照）。
    doc.insert_scalars(0, "甲").expect("编辑");
    let local = doc
        .pending_update(&VersionVector::default())
        .expect("导出本地增量");

    // writer 先按 head 0 建好 sink 并提交条目（真实路径同序：sink 先于外部推进）。
    let attempts = Arc::new(AtomicUsize::new(0));
    let sink = CountingSink {
        inner: VaultJournalSink::new(vault.clone(), "doc-1", 0),
        attempts: Arc::clone(&attempts),
    };
    let mut writer = JournalWriter::new(sink, 4, 4);
    assert_eq!(writer.durable_seq(), 0, "空 journal 时报告 sink 的真实水位");
    writer
        .submit(JournalEntry {
            doc_id: "doc-1".to_owned(),
            batch_id: "stale-batch".to_owned(),
            update: local.clone(),
            origin: JournalOrigin::Local,
        })
        .expect("submit");

    // 外部 writer（同步引擎 integrate / 另一个 Vault）在同一 doc 上推进 head。
    doc.insert_scalars(doc.scalar_len(), "乙").expect("编辑");
    let external = doc
        .pending_update(&VersionVector::default())
        .expect("导出外部增量");
    let advanced = append_updates(
        &vault,
        "doc-1",
        0,
        &[UpdateRecord::new("external-1", external)],
    )
    .expect("外部推进 head");
    assert_eq!(advanced.head_seq, 1);

    // 1) 第一次 flush：终态错误，而不是裸 HeadConflict。
    let error = writer.flush().expect_err("head 已被推进，flush 必须失败");
    match &error {
        StorageError::JournalHeadStale {
            doc_id,
            expected,
            actual,
            pending,
        } => {
            assert_eq!(doc_id, "doc-1");
            assert_eq!(*expected, 0, "错误必须带上写入用的 CAS 基准");
            assert_eq!(*actual, 1, "错误必须带上库里真实的 head");
            assert_eq!(*pending, 1, "错误必须说明有多少条未 durable 条目受影响");
        }
        other => panic!("期望 JournalHeadStale，实际 {other:?}"),
    }
    assert!(
        !matches!(&error, StorageError::HeadConflict { .. }),
        "journal 层不得把裸 HeadConflict 漏给调用方"
    );
    assert!(
        error.requires_session_rebuild(),
        "CAS 过期只能靠重建 session"
    );
    assert!(!error.is_retryable(), "CAS 过期不是可重试错误");
    assert_eq!(writer.pending_len(), 1, "失败不得丢条目");
    assert_eq!(writer.durable_seq(), 0, "失败不得推进 durable 水位");
    assert_eq!(writer.flushed_entries(), 0);
    assert!(writer.is_stale());
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        1,
        "CAS 冲突只尝试一次，不自动重试"
    );

    // 2) 再 flush / flush_all：幂等返回同一终态错误，且**不再碰数据库**（不会挂死、不刷屏）。
    for _ in 0..2 {
        let again = writer.flush().expect_err("stale 必须继续报错");
        assert!(
            matches!(again, StorageError::JournalHeadStale { pending: 1, .. }),
            "实际 {again:?}"
        );
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            1,
            "stale 之后不得重试 sink"
        );
    }
    assert!(writer.flush_all().is_err(), "排空也必须明确失败");

    // 3) stale 期间 submit 被拒绝：writer 已冻结，新条目必须交给重建后的 writer。
    let blocked = writer
        .submit(JournalEntry::local("doc-1", "later", local.clone()))
        .expect_err("stale 时 submit 必须被拒绝");
    assert!(
        matches!(blocked, StorageError::JournalHeadStale { pending: 1, .. }),
        "实际 {blocked:?}"
    );
    assert_eq!(writer.pending_len(), 1, "被拒的条目不能进 journal");

    // 4) 按契约重建 sink（新 head_seq）：旧条目**显式交还**，journal 清空，水位对齐新基准。
    let handed_back = writer
        .rebuild_sink(CountingSink {
            inner: VaultJournalSink::new(vault.clone(), "doc-1", 1),
            attempts: Arc::clone(&attempts),
        })
        .expect("stale 后重建 sink");
    assert_eq!(handed_back.len(), 1, "旧条目必须原样交还，不得静默丢弃");
    assert_eq!(handed_back[0].batch_id, "stale-batch");
    assert_eq!(handed_back[0].update, local);
    assert_eq!(writer.pending_len(), 0, "交还后 journal 清空");
    assert!(!writer.is_stale());
    assert_eq!(writer.durable_seq(), 1, "重建后水位 = 新 sink 的 CAS 基准");

    // 旧条目没有被偷偷写进库：库里仍只有外部那一条。
    assert_eq!(
        count(
            &vault,
            "SELECT count(*) FROM loro_updates WHERE doc_id = 'doc-1'",
            &[]
        ),
        1,
        "过期条目不得写入 loro_updates"
    );
    let (replayed, _) = load_body_doc(&vault, "doc-1", PEER_A, None).expect("重放");
    assert_eq!(replayed.text(), "甲乙", "只有外部那一条落库");

    // 5) 重建：用新 head 的 VV 重新导出（会话层的 ③），新条目 flush 成功且水位正确。
    let base = advanced.version_vector().expect("解码新 head 的 VV");
    doc.insert_scalars(doc.scalar_len(), "丙").expect("编辑");
    let fresh = doc.pending_update(&base).expect("按新 head 重新导出增量");
    writer
        .submit(JournalEntry {
            doc_id: "doc-1".to_owned(),
            batch_id: "fresh-1".to_owned(),
            update: fresh,
            origin: JournalOrigin::Local,
        })
        .expect("重建后可 submit");

    let report = writer.flush().expect("重建后的新条目必须能 flush");
    assert_eq!(
        report,
        FlushReport {
            entries: 1,
            durable_seq: 2
        }
    );
    assert_eq!(writer.durable_seq(), 2);
    assert_eq!(writer.flushed_entries(), 1);
    assert_eq!(writer.pending_len(), 0);
    assert_eq!(attempts.load(Ordering::SeqCst), 2, "重建后只多一次真实写入");
    assert_eq!(
        count(
            &vault,
            "SELECT count(*) FROM loro_updates WHERE doc_id = 'doc-1'",
            &[]
        ),
        2
    );
    assert_eq!(load_head(&vault, "doc-1").expect("head").head_seq, 2);
    let (replayed, _) = load_body_doc(&vault, "doc-1", PEER_A, None).expect("重放");
    assert_eq!(replayed.text(), "甲乙丙", "重建 + 重导出的内容完整");
}

/// 未过期时不允许重建 sink：那会把仍可写入的条目丢掉（契约的反向保护）。
#[test]
fn rebuild_sink_is_rejected_while_not_stale() {
    let (_dir, vault) = fresh_vault();
    let doc = seed_note(&vault, "note-1", "doc-1");
    doc.insert_scalars(0, "甲").expect("编辑");
    let update = doc.pending_update(&VersionVector::default()).expect("增量");

    let mut writer = JournalWriter::new(VaultJournalSink::new(vault.clone(), "doc-1", 0), 4, 4);
    writer
        .submit(JournalEntry::local("doc-1", "b1", update))
        .expect("submit");
    assert!(!writer.is_stale());

    let error = writer
        .rebuild_sink(VaultJournalSink::new(vault.clone(), "doc-1", 0))
        .expect_err("未 stale 时重建必须被拒绝");
    assert!(
        matches!(error, StorageError::Corrupt { .. }),
        "实际 {error:?}"
    );
    assert_eq!(writer.pending_len(), 1, "拒绝重建不得动 journal");
    assert_eq!(writer.flush().expect("正常 flush 仍可用").entries, 1);
}
