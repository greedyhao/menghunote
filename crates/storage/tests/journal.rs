//! 未 ACK journal 与背压：连续前缀、durable 后裁剪、慢盘合并、失败不丢已 ACK 部分。

mod support;

use loro::VersionVector;
use support::*;

use menghunote_storage::{
    load_body_doc, load_head, DurableSink, FlushReport, JournalEntry, JournalWriter, StorageError,
    VaultJournalSink,
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
        },
    ];
    let error = sink.write_prefix(&mixed).expect_err("混文档必须拒绝");
    assert!(matches!(error, StorageError::Corrupt { .. }));
}
