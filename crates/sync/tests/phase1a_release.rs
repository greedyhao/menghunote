//! Phase 1A release 探针：`baseline_apply` 持闸 merge 成本（手动运行，不 assert 时间）。
//!
//! 只测 **storage 的 baseline merge**：在已有库上准备 20 篇带本地未上传 delta 的正文，
//! 构造 5 MiB / 20 MiB 两档 Loro snapshot（20 篇合计明文），走
//! `begin_baseline_apply → apply_baseline_batch × N → finish_baseline_apply`，
//! 记录总墙钟 / 各段 / `gate_hold_ms`（begin + 各批 apply + finish 的墙钟和，
//! 对应 Phase 1A 计划里"持写闸 merge"的时间口径），以及 storage 内置的分段计时
//! 与计数（restore / head 查询 / 本地重放 / 本地差集 / 二次 snapshot import /
//! detached oplog import / pending_update / SQLite 事务），用于区分
//! 「Loro 单大文本 op 的超线性」与「重复解码/二次导入」两类成本。
//!
//! 不涉及 relay / checkpoint 签名 / 真实下载：`S` 与 cursor 是自造的合法值；
//! 每次都用 fresh vault，结束后 journal 一律 `completed`，不会给下一次留 staging。
//!
//! 第二个探针 [`checkpoint_validate_scan_cost`] 测 **checkpoint validate 的 coverage
//! 日志扫描 O(S) 成本**：FakeRelay 构造 S = 1k / 5k / 25k / 1M 条混合日志（85.7% UPDATE +
//! 14.3% ASSET），走真实 `CheckpointWriter::prepare → validate`（不复制 scan 实现），
//! 只打印页数/墙钟，不 assert 时间。1M 档 = 4096 页，是生产
//! `COVERAGE_SCAN_MAX_PAGES` 的上限，只跑 1 次（常规档 3 次取中位数）。
//!
//! 第三个探针 [`large_doc_per_page_replay_cost`]（M3 标定）测 **一篇 1 MiB 大正文的
//! per-page replay 成本**：每次 aggregate 页都会为每个 touched doc 全量
//! `load_body_doc`；该探针连续重放 5 次并打印 raw/中位数，另测 snapshot 导出/恢复与
//! 全量 `pending_update` 作参考。同样只打印、不 assert 时间。
//!
//! ```bash
//! cargo test --release -p menghunote-sync --test phase1a_release -- --ignored --nocapture
//! ```

mod support;

use std::time::Instant;

use support::{
    commit_local, commit_with_intent, fresh_vault, publish_real_asset, seed_doc, uuid_like,
    FakeRelay, TestRandom, EPOCH, VAULT_ID,
};

use menghunote_storage::{
    apply_baseline_batch, baseline_apply_debug_counters, baseline_apply_debug_timings,
    baseline_apply_state, begin_baseline_apply, create_note_slot, ensure_structure_row,
    finish_baseline_apply, load_body_doc, reset_baseline_apply_debug, sync_cursor_state,
    BaselineApplyDebugCounters, BaselineApplyDebugTimings, BaselineApplyFinish,
    BaselineApplyStatus, BaselineExpectation, BodyDoc, Value, Vault, BASELINE_MERGE_BATCH,
    META_SYNC_CONFIRMED_HIGHWATER, META_SYNC_INTEGRATED_SEQ, META_SYNC_LOG_ID,
    META_SYNC_RECEIVED_SEQ, META_SYNC_RELAY_CURSOR,
};
use menghunote_sync::checkpoint::{CheckpointWriter, EmptyAssets, PreparedCheckpoint};
use menghunote_sync::engine::RELAY_CURSOR_META;
use menghunote_sync::genesis;
use menghunote_sync::relay::{
    BootstrapBundle, BootstrapLease, CheckpointLease, CheckpointPublish, CheckpointRef,
    ObjectSubmission, PublishedCheckpoint, RelayClient, RelayLogKind, RelayLogPage, RelayState,
    SlotRecord, SlotUpload, UpdateOutcome, UpdateSubmission,
};
use menghunote_sync::{DeviceId, ObjectId, SyncError};
use menghunote_sync_proto::DeviceCertV1;

/// 基线文档数（真实信任链里 docs 数量更多；这里固定 20 便于两档总量均分）。
const DOCS: usize = 20;
/// 本地设备 peer。
const LOCAL_PEER: u64 = 0x41;
/// 远端基线写者 peer（只用于生成 snapshot；与本地 peer 不同）。
const REMOTE_PEER: u64 = 0x42;
/// 本地未上传 delta 的标记（大写 + 下划线，不会出现在远端随机字母表里）。
const LOCAL_PREFIX: &str = "LOCAL_UNSYNCED_";
/// 远端基线内容标记（同上）。
const REMOTE_PREFIX: &str = "REMOTE_BASELINE_";
/// 每批 apply 的文档数（20 篇 → 4 批；生产 `BOOTSTRAP_DOC_BATCH=64`，这里分小批
/// 是为了能看到分段耗时，总 merge 工作量相同）。
const APPLY_CHUNK: usize = 5;
/// 模拟的 checkpoint 覆盖水位（自造合法值）。
const BASELINE_S: u64 = 500;
/// finish 选定的 cursor。
const BASELINE_CURSOR: &str = "c500";
/// 本地已有水位（finish 后必须单调前进到 `BASELINE_S`）。
const SEED_RECEIVED: u64 = 100;
const SEED_INTEGRATED: u64 = 80;
const SEED_HIGHWATER: u64 = 90;
/// 本地已有 relay cursor。
const SEED_CURSOR: &str = "c80";

/// 远端正文随机字母表（只用小写字母 + 数字，标记里的大写/下划线不可能偶然出现）。
const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";

fn pseudo_text(seed: u64, chars: usize) -> String {
    let mut state = seed
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(0xD1B5_4A32_D192_ED03);
    let mut out = String::with_capacity(chars + 32);
    while out.len() < chars {
        // xorshift64：确定性、跨平台一致，避免为基准引入随机依赖。
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        let value = state.wrapping_mul(0x2545_F491_4F6C_DD1D);
        for byte in value.to_le_bytes() {
            out.push(ALPHABET[(byte as usize) % ALPHABET.len()] as char);
            if out.len() >= chars {
                break;
            }
        }
    }
    out
}

fn remote_text(index: usize, chars: usize) -> String {
    let mut text = format!("{REMOTE_PREFIX}{index:02}|");
    text.push_str(&pseudo_text(0x5eed_0000 + index as u64, chars));
    text
}

fn local_text(index: usize) -> String {
    format!("{LOCAL_PREFIX}{index:02}|{}", "L".repeat(2048))
}

fn build_snapshot(index: usize, chars: usize) -> Vec<u8> {
    let doc = BodyDoc::create(format!("bench-doc-{index:02}"), REMOTE_PEER)
        .expect("创建远端基线 BodyDoc");
    doc.insert_scalars(0, &remote_text(index, chars))
        .expect("写远端基线正文");
    doc.export_snapshot().expect("导出 baseline snapshot")
}

/// 生成 20 篇 snapshot，使总字节数尽量贴近 `target_bytes`。
///
/// 先用 16 KiB 文本量出"snapshot 字节 / 文本字符"的比例，再按比例放大；若实测
/// 总字节偏离目标 >2%，按实测比例再重建一次（Loro snapshot 有固定的块/字典开销，
/// 超出这个范围说明比例外推不准）。
fn build_snapshot_set(target_bytes: usize) -> (Vec<Vec<u8>>, usize, f64) {
    let per_doc = target_bytes / DOCS;
    let pilot_chars = 16 * 1024;
    let pilot = build_snapshot(0x3ff, pilot_chars);
    let ratio = pilot.len() as f64 / pilot_chars as f64;
    assert!(ratio > 0.0, "snapshot 比例必须为正");

    let mut chars = ((per_doc as f64) / ratio).ceil() as usize;
    let mut snapshots: Vec<Vec<u8>> = (0..DOCS)
        .map(|index| build_snapshot(index, chars))
        .collect();
    let total: usize = snapshots.iter().map(Vec::len).sum();
    if (total as f64 - target_bytes as f64).abs() / target_bytes as f64 > 0.02 {
        let scale = target_bytes as f64 / total as f64;
        chars = ((chars as f64) * scale).ceil() as usize;
        snapshots = (0..DOCS)
            .map(|index| build_snapshot(index, chars))
            .collect();
    }
    (snapshots, chars, ratio)
}

fn count(vault: &Vault, sql: &str) -> i64 {
    let rows = vault.query(sql, &[]).expect("count 查询");
    match rows.first().and_then(|row| row.first()) {
        Some(Value::Integer(value)) => *value,
        other => panic!("count 结果异常：{other:?}"),
    }
}

fn seed_watermarks(vault: &Vault) {
    vault
        .set_meta(META_SYNC_RECEIVED_SEQ, &SEED_RECEIVED.to_be_bytes())
        .expect("seed received");
    vault
        .set_meta(META_SYNC_INTEGRATED_SEQ, &SEED_INTEGRATED.to_be_bytes())
        .expect("seed integrated");
    vault
        .set_meta(META_SYNC_CONFIRMED_HIGHWATER, &SEED_HIGHWATER.to_be_bytes())
        .expect("seed highwater");
    vault
        .set_meta(META_SYNC_RELAY_CURSOR, SEED_CURSOR.as_bytes())
        .expect("seed relay cursor");
}

/// 在 fresh vault 上准备 20 篇"已有本地正文 + 本地未上传 delta"的文档。
fn prepare_vault() -> (tempfile::TempDir, Vault, Vec<String>) {
    let (dir, vault) = fresh_vault();
    let mut doc_ids = Vec::with_capacity(DOCS);
    for index in 0..DOCS {
        let doc_id = format!("bench-doc-{index:02}");
        let doc = seed_doc(&vault, &doc_id, LOCAL_PEER);
        doc.insert_scalars(0, &local_text(index))
            .expect("写本地正文");
        // 不带同步意图：这是一条只在本机、从未上传的 delta。
        commit_local(&vault, &doc);
        doc_ids.push(doc_id);
    }
    seed_watermarks(&vault);
    (dir, vault, doc_ids)
}

#[derive(Debug)]
struct ProbeStats {
    label: &'static str,
    target_bytes: usize,
    snapshot_total: usize,
    snapshot_min: usize,
    snapshot_max: usize,
    chars_per_doc: usize,
    ratio: f64,
    build_snapshot_ms: u128,
    vault_prep_ms: u128,
    begin_ms: u128,
    apply_batch_ms: Vec<u128>,
    apply_total_ms: u128,
    finish_ms: u128,
    gate_hold_ms: u128,
    verify_ms: u128,
    total_wall_ms: u128,
    timings: BaselineApplyDebugTimings,
    counters: BaselineApplyDebugCounters,
}

#[allow(clippy::too_many_lines)]
fn run_probe(label: &'static str, target_bytes: usize) -> ProbeStats {
    let started = Instant::now();

    // 1) 生成快照（不持闸；真实实现里这段是下载 + 解密 + 校验）。
    let build_started = Instant::now();
    let (snapshots, chars_per_doc, ratio) = build_snapshot_set(target_bytes);
    let build_snapshot_ms = build_started.elapsed().as_millis();
    let snapshot_total: usize = snapshots.iter().map(Vec::len).sum();
    let snapshot_min = snapshots.iter().map(Vec::len).min().unwrap_or(0);
    let snapshot_max = snapshots.iter().map(Vec::len).max().unwrap_or(0);
    assert_eq!(snapshots.len(), DOCS);

    // 2) 已有库：20 篇本地正文 + 未上传 delta + 旧水位。
    let prep_started = Instant::now();
    let (_dir, vault, doc_ids) = prepare_vault();
    let vault_prep_ms = prep_started.elapsed().as_millis();
    assert_eq!(
        count(&vault, "SELECT count(*) FROM sync_outbox"),
        0,
        "本地 delta 不得有 outbox 行"
    );
    let before = sync_cursor_state(&vault).expect("seeded cursor");
    assert_eq!(before.received_seq, SEED_RECEIVED);
    assert_eq!(before.integrated_seq, SEED_INTEGRATED);
    assert_eq!(before.confirmed_highwater, Some(SEED_HIGHWATER));

    // 3) 持闸段：begin → apply 各批 → finish。
    reset_baseline_apply_debug();
    let begin_started = Instant::now();
    let expected =
        BaselineExpectation::new(doc_ids.iter().cloned(), std::iter::empty::<[u8; 32]>());
    let begin_state = begin_baseline_apply(&vault, &[0x51; 16], BASELINE_S, "c0-source", &expected)
        .expect("begin baseline");
    let begin_ms = begin_started.elapsed().as_millis();
    assert_eq!(begin_state.status, BaselineApplyStatus::Staging);
    assert_eq!(begin_state.expected_docs, Some(DOCS as u64));
    assert!(
        !begin_state.finishable,
        "begin 时预期 docs 尚未应用，finishable 必须为 false"
    );

    let entries: Vec<(String, Vec<u8>)> = doc_ids
        .iter()
        .cloned()
        .zip(snapshots.iter().cloned())
        .collect();
    let mut apply_batch_ms = Vec::new();
    let mut apply_total_ms = 0u128;
    for (batch_index, chunk) in entries.chunks(APPLY_CHUNK).enumerate() {
        let batch_started = Instant::now();
        let state = apply_baseline_batch(&vault, chunk, &[]).expect("apply baseline batch");
        let batch_ms = batch_started.elapsed().as_millis();
        apply_total_ms += batch_ms;
        apply_batch_ms.push(batch_ms);
        eprintln!(
            "[phase1a] baseline_apply label={label} batch={batch_index} docs={} apply_ms={batch_ms} applied_docs={}",
            chunk.len(),
            state.applied_docs
        );
    }

    let finish_started = Instant::now();
    let finish = finish_baseline_apply(&vault, BASELINE_S, BASELINE_CURSOR).expect("finish");
    let finish_ms = finish_started.elapsed().as_millis();
    let gate_hold_ms = begin_ms + apply_total_ms + finish_ms;
    let BaselineApplyFinish::Applied(finish_state) = finish else {
        panic!("finish 必须 Applied，实际 {finish:?}");
    };
    assert_eq!(finish_state.status, BaselineApplyStatus::Completed);
    assert_eq!(finish_state.applied_docs, DOCS as u64);
    assert!(finish_state.chosen_cursor.as_deref() == Some(BASELINE_CURSOR));
    let timings = baseline_apply_debug_timings();
    let counters = baseline_apply_debug_counters();

    // 4) 闸外校验：本地 delta 保留、正文可重放、水位单调、无残留 staging。
    let verify_started = Instant::now();
    let after = sync_cursor_state(&vault).expect("cursor after finish");
    assert!(
        after.received_seq >= before.received_seq,
        "received 不得回退"
    );
    assert!(
        after.integrated_seq >= before.integrated_seq,
        "integrated 不得回退"
    );
    assert!(
        after.confirmed_highwater.unwrap_or(0) >= before.confirmed_highwater.unwrap_or(0),
        "highwater 不得回退"
    );
    assert_eq!(after.received_seq, BASELINE_S);
    assert_eq!(after.integrated_seq, BASELINE_S);
    assert_eq!(after.confirmed_highwater, Some(BASELINE_S));
    assert_eq!(
        vault
            .get_meta(META_SYNC_RELAY_CURSOR)
            .expect("relay cursor")
            .as_deref(),
        Some(BASELINE_CURSOR.as_bytes())
    );
    for (index, doc_id) in doc_ids.iter().enumerate() {
        let (doc, head) = load_body_doc(&vault, doc_id, LOCAL_PEER, None).expect("merged doc");
        let text = doc.text();
        assert!(
            text.contains(&format!("{LOCAL_PREFIX}{index:02}")),
            "第 {index} 篇本地未上传 delta 丢失"
        );
        assert!(
            text.contains(&format!("{REMOTE_PREFIX}{index:02}")),
            "第 {index} 篇基线正文未导入"
        );
        assert!(
            head.head_seq >= 2,
            "第 {index} 篇应至少有本地 + baseline 两条 update"
        );
    }
    assert_eq!(
        count(
            &vault,
            "SELECT count(*) FROM loro_updates WHERE batch_id='local'",
        ),
        DOCS as i64,
        "本地 delta 的 update 行必须保留"
    );
    assert_eq!(
        count(
            &vault,
            &format!("SELECT count(*) FROM loro_updates WHERE batch_id='{BASELINE_MERGE_BATCH}'"),
        ),
        DOCS as i64,
        "每篇应恰好追加一条 baseline_merge 组合 update"
    );
    assert_eq!(count(&vault, "SELECT count(*) FROM sync_outbox"), 0);
    let journal = baseline_apply_state(&vault)
        .expect("state")
        .expect("journal");
    assert_eq!(journal.status, BaselineApplyStatus::Completed);
    assert!(!journal.finishable);
    let verify_ms = verify_started.elapsed().as_millis();

    let total_wall_ms = started.elapsed().as_millis();
    ProbeStats {
        label,
        target_bytes,
        snapshot_total,
        snapshot_min,
        snapshot_max,
        chars_per_doc,
        ratio,
        build_snapshot_ms,
        vault_prep_ms,
        begin_ms,
        apply_batch_ms,
        apply_total_ms,
        finish_ms,
        gate_hold_ms,
        verify_ms,
        total_wall_ms,
        timings,
        counters,
    }
}

fn print_probe(stats: &ProbeStats) {
    let mib = |bytes: usize| bytes as f64 / (1024.0 * 1024.0);
    eprintln!(
        "[phase1a] baseline_apply label={} target_mib={:.2} snapshot_total={} snapshot_mib={:.2} \
         per_doc_min={} per_doc_max={} chars_per_doc={} snapshot_ratio={:.4} docs={DOCS}",
        stats.label,
        mib(stats.target_bytes),
        stats.snapshot_total,
        mib(stats.snapshot_total),
        stats.snapshot_min,
        stats.snapshot_max,
        stats.chars_per_doc,
        stats.ratio,
    );
    eprintln!(
        "[phase1a] baseline_apply label={} build_snapshot_ms={} vault_prep_ms={} begin_ms={} \
         apply_batches_ms={:?} apply_total_ms={} finish_ms={} gate_hold_ms={} verify_ms={} \
         total_wall_ms={}",
        stats.label,
        stats.build_snapshot_ms,
        stats.vault_prep_ms,
        stats.begin_ms,
        stats.apply_batch_ms,
        stats.apply_total_ms,
        stats.finish_ms,
        stats.gate_hold_ms,
        stats.verify_ms,
        stats.total_wall_ms,
    );
    let t = &stats.timings;
    let ns_to_ms = |ns: u64| ns as f64 / 1e6;
    eprintln!(
        "[phase1a] baseline_apply label={} phases_ms restore={:.1} head_lookup={:.1} \
         load_body_doc={:.1} local_delta_export={:.1} local_delta_import={:.1} \
         snapshot_reimport={:.1} new_doc_import={:.1} pending_update={:.1} \
         transaction={:.1} batch_total_sum={:.1}",
        stats.label,
        ns_to_ms(t.restore_ns),
        ns_to_ms(t.head_lookup_ns),
        ns_to_ms(t.load_body_doc_ns),
        ns_to_ms(t.local_delta_export_ns),
        ns_to_ms(t.local_delta_import_ns),
        ns_to_ms(t.snapshot_reimport_ns),
        ns_to_ms(t.new_doc_import_ns),
        ns_to_ms(t.pending_update_ns),
        ns_to_ms(t.transaction_ns),
        ns_to_ms(t.batch_total_ns),
    );
    let c = &stats.counters;
    eprintln!(
        "[phase1a] baseline_apply label={} counters batches={} docs={} new={} \
         existing_no_extra={} existing_local_extra={} replayed={} snapshot_imports={} \
         snapshot_mib={:.2} load_body_doc_calls={} snapshot_reimport_mib={:.2} \
         new_doc_import_mib={:.2} pending_update_calls={} pending_update_mib={:.2} \
         local_delta_kib={:.2}",
        stats.label,
        c.batches,
        c.docs,
        c.new_docs,
        c.existing_no_extra,
        c.existing_local_extra,
        c.replayed_same_digest,
        c.snapshot_imports,
        mib(c.snapshot_bytes as usize),
        c.load_body_doc_calls,
        mib(c.snapshot_reimport_bytes as usize),
        mib(c.new_doc_import_bytes as usize),
        c.pending_update_calls,
        mib(c.pending_update_bytes as usize),
        c.local_delta_bytes as f64 / 1024.0,
    );
}

#[test]
#[ignore = "manual benchmark"]
fn phase1a_baseline_apply_gate_hold() {
    for (label, bytes) in [
        ("5MiB", 5usize * 1024 * 1024),
        ("20MiB", 20usize * 1024 * 1024),
    ] {
        let stats = run_probe(label, bytes);
        print_probe(&stats);
    }
}

// ============================================================ checkpoint validate 覆盖日志扫描成本
//
// 任务 4 P1：只测真实 `CheckpointWriter::validate` 里 coverage 日志扫描的 O(S) 成本，
// 不改生产 `scan_coverage_log`，也不复制它的实现。S 条日志是合成的（FakeRelay）：
// `1..=S-1` 落在本地 integrated 连续前缀（生产代码信任的"已消费"不变量），尾部第 S 条
// 是真实 ASSET（prepare 落 pending receipt，validate 必须下载实证）；前缀里按 1/7
// 混入 ASSET 事件（≈14.3%，任务要求的 10–20%）。
//
// 口径：
// - `pages` = validate 期间真实 `RelayClient::log` 调用次数（应从 c0 扫到 S，S/256 页）。
// - `scan_ms` = 第一次 log 开始到最后一次 log 返回的墙钟跨度（含页间逐事件核对；
//   不含最后一页返回之后的 256 条 visitor，误差上界 = 一页）。
// - `validate_ms` = 整个 validate 墙钟（含 manifest/快照读回、尾部 ASSET 实证）。
// 只打印不 assert 时间；硬断言只校验覆盖到了 S、尾部 ASSET 被真实验证、页数符合 S/256。

/// 探针档位（事件总数 S）。
///
/// 最后一档 `1_048_576 = 4096 × 256` 正好对应生产 `COVERAGE_SCAN_MAX_PAGES = 4096`
/// 的页数上限（`crates/sync/src/checkpoint.rs`，私有常量）；这一档只跑 1 次，
/// 避免 3 × 百万事件（FakeRelay 的逐页扫描本身还有 O(S²/256) 的夹具 artifact）。
const SCAN_COST_S: [usize; 4] = [1_000, 5_000, 25_000, SCAN_COST_MAX_S];
/// 4096 页上限对应的事件总数。
const SCAN_COST_MAX_S: usize = 1_048_576;
/// 常规档位重复次数（取中位数）。
const SCAN_COST_RUNS: usize = 3;
/// 上限档位重复次数（只跑 1 次，不取中位数）。
const SCAN_COST_MAX_RUNS: usize = 1;
/// 生产 `COVERAGE_SCAN_PAGE_LIMIT`（`crates/sync/src/checkpoint.rs` 私有常量 = 256）。
/// 只用于校验探针确实按 S/256 页扫完整段；若生产改页大小，这里应同步更新。
const SCAN_COST_PAGE_LIMIT: usize = 256;
/// ASSET 事件间隔（每 7 条一条 → 14.3%）。
const SCAN_COST_ASSET_EVERY: usize = 7;
/// 探针正文文档 id（与 coverage 无关，只给 prepare 提供可导出的本地快照）。
const SCAN_COST_NOTE_ID: &str = "scan-cost-note";

/// 计数/计时包装：所有方法委托给 FakeRelay；只额外统计 `log()`（真实 coverage scan
/// 发起的页请求）与 `part()`（manifest/snapshot/ASSET 读回）的调用次数与墙钟。
///
/// 这是**探针侧 instrumentation**，不是 scan 实现的拷贝：validate 仍走生产
/// `CheckpointWriter::validate` → `scan_coverage_log`，wrapper 只是它调用的客户端。
struct CountingRelay {
    inner: FakeRelay,
    log_calls: u64,
    log_busy_ns: u128,
    scan_first: Option<Instant>,
    scan_last: Option<Instant>,
    part_calls: u64,
    part_busy_ns: u128,
}

impl CountingRelay {
    fn new(inner: FakeRelay) -> Self {
        Self {
            inner,
            log_calls: 0,
            log_busy_ns: 0,
            scan_first: None,
            scan_last: None,
            part_calls: 0,
            part_busy_ns: 0,
        }
    }

    fn reset_counters(&mut self) {
        self.log_calls = 0;
        self.log_busy_ns = 0;
        self.scan_first = None;
        self.scan_last = None;
        self.part_calls = 0;
        self.part_busy_ns = 0;
    }

    /// 第一次到最后一次 `log()` 返回的墙钟跨度（页扫描口径）。
    fn scan_span_ms(&self) -> f64 {
        match (self.scan_first, self.scan_last) {
            (Some(first), Some(last)) => last.duration_since(first).as_secs_f64() * 1e3,
            _ => 0.0,
        }
    }

    /// 仅 `log()` 内部（FakeRelay 取页/克隆事件）的墙钟合计。
    fn log_busy_ms(&self) -> f64 {
        self.log_busy_ns as f64 / 1e6
    }

    /// `part()` 读回墙钟合计（manifest/snapshot/ASSET）。
    fn part_busy_ms(&self) -> f64 {
        self.part_busy_ns as f64 / 1e6
    }
}

impl RelayClient for CountingRelay {
    fn state(&mut self) -> Result<RelayState, SyncError> {
        self.inner.state()
    }

    fn log(&mut self, cursor: Option<&str>, limit: u32) -> Result<RelayLogPage, SyncError> {
        if self.scan_first.is_none() {
            self.scan_first = Some(Instant::now());
        }
        let started = Instant::now();
        let result = self.inner.log(cursor, limit);
        self.log_busy_ns += started.elapsed().as_nanos();
        self.log_calls += 1;
        self.scan_last = Some(Instant::now());
        result
    }

    fn part(&mut self, object_id: &ObjectId, part_no: u32) -> Result<Vec<u8>, SyncError> {
        let started = Instant::now();
        let result = self.inner.part(object_id, part_no);
        self.part_busy_ns += started.elapsed().as_nanos();
        self.part_calls += 1;
        result
    }

    fn put_part(
        &mut self,
        object_id: &ObjectId,
        part_no: u32,
        envelope: &[u8],
    ) -> Result<(), SyncError> {
        self.inner.put_part(object_id, part_no, envelope)
    }

    fn submit_updates(
        &mut self,
        items: Vec<UpdateSubmission>,
    ) -> Result<Vec<UpdateOutcome>, SyncError> {
        self.inner.submit_updates(items)
    }

    fn submit_object(&mut self, item: ObjectSubmission) -> Result<UpdateOutcome, SyncError> {
        self.inner.submit_object(item)
    }

    fn ack(&mut self, received_seq: u64, integrated_seq: u64, epoch: u64) -> Result<(), SyncError> {
        self.inner.ack(received_seq, integrated_seq, epoch)
    }

    fn device_cert(&mut self, device_id: &DeviceId) -> Result<Option<DeviceCertV1>, SyncError> {
        self.inner.device_cert(device_id)
    }

    fn put_key_slot(&mut self, upload: SlotUpload) -> Result<SlotRecord, SyncError> {
        self.inner.put_key_slot(upload)
    }

    fn list_key_slots(&mut self) -> Result<Vec<SlotRecord>, SyncError> {
        self.inner.list_key_slots()
    }

    fn activate_password_slot(
        &mut self,
        expected_slot_set_rev: u64,
        slot_id: ObjectId,
        signed_message: Vec<u8>,
        signature: [u8; 64],
    ) -> Result<u64, SyncError> {
        self.inner
            .activate_password_slot(expected_slot_set_rev, slot_id, signed_message, signature)
    }

    fn create_checkpoint(
        &mut self,
        estimated_bytes: u64,
        epoch: Option<u64>,
    ) -> Result<CheckpointLease, SyncError> {
        self.inner.create_checkpoint(estimated_bytes, epoch)
    }

    fn add_checkpoint_refs(
        &mut self,
        checkpoint_id: [u8; 16],
        refs: Vec<CheckpointRef>,
    ) -> Result<u64, SyncError> {
        self.inner.add_checkpoint_refs(checkpoint_id, refs)
    }

    fn publish_checkpoint(
        &mut self,
        publish: CheckpointPublish,
    ) -> Result<PublishedCheckpoint, SyncError> {
        self.inner.publish_checkpoint(publish)
    }

    fn bootstrap(&mut self) -> Result<BootstrapBundle, SyncError> {
        self.inner.bootstrap()
    }

    fn renew_bootstrap_lease(&mut self, lease_id: [u8; 16]) -> Result<BootstrapLease, SyncError> {
        self.inner.renew_bootstrap_lease(lease_id)
    }
}

/// 一次性夹具：真实 SQLite vault + 合成 S 条日志 + 已 prepare 的 checkpoint。
struct ScanCostFixture {
    _dir: tempfile::TempDir,
    vault: Vault,
    materials: genesis::GenesisMaterials,
    relay: CountingRelay,
    prepared: PreparedCheckpoint,
    asset_events: usize,
    update_events: usize,
    prepare_pages: u64,
    prepare_scan_ms: f64,
    prepare_log_ms: f64,
}

fn build_scan_cost_fixture(total_events: usize) -> ScanCostFixture {
    assert!(
        total_events > 1,
        "S 至少需要一条前缀 UPDATE 与一条尾部 ASSET"
    );
    let (dir, vault) = fresh_vault();
    ensure_structure_row(&vault).expect("structure 行");
    vault
        .set_meta(RELAY_CURSOR_META, b"c0")
        .expect("写 relay 游标");
    let materials = genesis::build_materials(
        VAULT_ID,
        [0x44; 16],
        "pw",
        1_700_000_000,
        &mut TestRandom::new(0x88_14),
    )
    .expect("genesis 材料");
    let mut relay = FakeRelay::new(&[]);
    relay
        .certs
        .insert(materials.device_id, materials.certificate().clone());
    vault
        .set_meta(META_SYNC_LOG_ID, &relay.log_id)
        .expect("写 log_id");

    // 一篇真实正文：prepare 需要可导出的本地快照/head VV（与 coverage 无关）。
    let doc = seed_doc(&vault, SCAN_COST_NOTE_ID, materials.peer_id_u64());
    create_note_slot(
        &vault,
        SCAN_COST_NOTE_ID,
        SCAN_COST_NOTE_ID,
        SCAN_COST_NOTE_ID,
        1_700_000_000,
    )
    .expect("notes 行");
    doc.insert_scalars(0, "checkpoint coverage scan cost probe")
        .expect("正文");
    commit_with_intent(&vault, &doc, &uuid_like(9_001), [0x91; 32], EPOCH);

    // 1..S-1 按 1/7 混入 ASSET；第 S 条是真 ASSET。前 S-1 条都属于本地
    // integrated 连续前缀（只借生产代码信任的"已消费"不变量，不读资产），
    // 唯一缺口是尾部 ASSET → prepare 落 pending receipt，validate 必须下载实证。
    let mut asset_events = 0usize;
    for seq in 1..total_events {
        if seq % SCAN_COST_ASSET_EVERY == 0 {
            relay.publish_event(RelayLogKind::Asset, [0x2a; 32]);
            asset_events += 1;
        } else {
            relay.publish_event(RelayLogKind::Update, [0x15; 32]);
        }
    }
    let (_, tail_seq) = publish_real_asset(
        &mut relay,
        VAULT_ID,
        materials.device_id,
        &materials.signing_key,
        &materials.root,
        b"coverage scan cost tail asset",
        "text/plain",
    );
    assert_eq!(
        tail_seq,
        u64::try_from(total_events).expect("S 溢出 u64"),
        "尾部 ASSET 必须恰为第 S 条日志"
    );
    asset_events += 1;
    let update_events = total_events - asset_events;
    assert!(
        (update_events) > 0,
        "混合日志必须至少有一条 UPDATE（实际 S={total_events}）"
    );

    let prefix = u64::try_from(total_events - 1).expect("S 溢出 u64");
    vault
        .set_meta(META_SYNC_INTEGRATED_SEQ, &prefix.to_be_bytes())
        .expect("写 integrated 前缀");
    vault
        .set_meta(META_SYNC_RECEIVED_SEQ, &prefix.to_be_bytes())
        .expect("写 received 前缀");

    // 从这里开始的页请求才算探针成本（prepare 的 pending receipt 扫描也是 O(S)）。
    let mut relay = CountingRelay::new(relay);
    let prepared = {
        let mut writer = CheckpointWriter::new(
            &vault,
            &mut relay,
            VAULT_ID,
            materials.device_id,
            materials.signing_key.clone(),
            materials.root.clone(),
            materials.peer_id_u64(),
        );
        writer.prepare(&EmptyAssets).expect("prepare")
    };
    assert_eq!(
        prepared.report.covered_seq,
        u64::try_from(total_events).expect("S 溢出 u64")
    );
    assert_eq!(
        relay.log_calls,
        u64::try_from(total_events.div_ceil(SCAN_COST_PAGE_LIMIT)).expect("页数溢出 u64"),
        "prepare 的 receipt 扫描应从 c0 扫满 S/256 页"
    );
    let prepare_pages = relay.log_calls;
    let prepare_scan_ms = relay.scan_span_ms();
    let prepare_log_ms = relay.log_busy_ms();
    relay.reset_counters();

    ScanCostFixture {
        _dir: dir,
        vault,
        materials,
        relay,
        prepared,
        asset_events,
        update_events,
        prepare_pages,
        prepare_scan_ms,
        prepare_log_ms,
    }
}

fn median_ms(values: &[f64]) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).expect("时间不能是 NaN"));
    sorted[sorted.len() / 2]
}

/// 该档位的 validate 重复次数：4096 页上限档只跑 1 次，其余 3 次取中位数。
fn scan_cost_runs(total_events: usize) -> usize {
    if total_events >= SCAN_COST_MAX_S {
        SCAN_COST_MAX_RUNS
    } else {
        SCAN_COST_RUNS
    }
}

/// 进程峰值常驻内存（Linux `/proc/self/status` 的 `VmHWM`，MiB）。
///
/// 只用于给 4096 页档的夹具内存占用一个量级参考，不参与断言；其它平台返回 `None`。
fn peak_resident_mib() -> Option<f64> {
    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        let line = status.lines().find(|line| line.starts_with("VmHWM:"))?;
        let kib: f64 = line.split_whitespace().nth(1)?.parse().ok()?;
        Some(kib / 1024.0)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

fn run_scan_cost(total_events: usize) {
    let runs = scan_cost_runs(total_events);
    let setup_started = Instant::now();
    let mut fixture = build_scan_cost_fixture(total_events);
    let setup_ms = setup_started.elapsed().as_secs_f64() * 1e3;
    let peak_rss = peak_resident_mib().map_or_else(|| "n/a".to_owned(), |mib| format!("{mib:.1}"));
    let asset_pct = fixture.asset_events as f64 * 100.0 / total_events as f64;
    eprintln!(
        "[phase1a] scan_cost setup S={total_events} asset_events={} update_events={} \
         asset_pct={asset_pct:.1} prepare_pages={} prepare_scan_ms={:.3} prepare_log_ms={:.3} \
         setup_ms={setup_ms:.3} runs={runs} peak_rss_mib={peak_rss}",
        fixture.asset_events,
        fixture.update_events,
        fixture.prepare_pages,
        fixture.prepare_scan_ms,
        fixture.prepare_log_ms,
    );

    let mut pages = 0u64;
    let mut part_calls = 0u64;
    let mut raw_scan = Vec::with_capacity(runs);
    let mut raw_validate = Vec::with_capacity(runs);
    let mut raw_log = Vec::with_capacity(runs);
    let mut raw_part = Vec::with_capacity(runs);
    for run in 0..runs {
        fixture.relay.reset_counters();
        let started = Instant::now();
        let baseline = {
            let mut writer = CheckpointWriter::new(
                &fixture.vault,
                &mut fixture.relay,
                VAULT_ID,
                fixture.materials.device_id,
                fixture.materials.signing_key.clone(),
                fixture.materials.root.clone(),
                fixture.materials.peer_id_u64(),
            );
            writer.validate(&fixture.prepared).expect("validate")
        };
        let validate_ms = started.elapsed().as_secs_f64() * 1e3;
        assert_eq!(
            baseline.covered_seq(),
            u64::try_from(total_events).expect("S 溢出 u64")
        );
        assert_eq!(
            baseline.coverage_verified_candidates(),
            1,
            "尾部 ASSET 必须被真实下载验证（receipt 路径）"
        );
        assert_eq!(baseline.coverage_durable_assets(), 0);

        let run_pages = fixture.relay.log_calls;
        let scan_ms = fixture.relay.scan_span_ms();
        let log_ms = fixture.relay.log_busy_ms();
        let run_parts = fixture.relay.part_calls;
        let part_ms = fixture.relay.part_busy_ms();
        assert_eq!(
            run_pages,
            u64::try_from(total_events.div_ceil(SCAN_COST_PAGE_LIMIT)).expect("页数溢出 u64"),
            "validate 的 coverage scan 应从 c0 扫满 S/256 页"
        );
        assert!(run_parts >= 1, "validate 至少读回 manifest/快照/尾部 ASSET");
        pages = run_pages;
        part_calls = run_parts;
        eprintln!(
            "[phase1a] scan_cost S={total_events} run={run} pages={run_pages} \
             scan_ms={scan_ms:.3} log_ms={log_ms:.3} validate_ms={validate_ms:.3} \
             part_calls={run_parts} part_ms={part_ms:.3}",
        );
        raw_scan.push(scan_ms);
        raw_validate.push(validate_ms);
        raw_log.push(log_ms);
        raw_part.push(part_ms);
    }

    let scan_median = median_ms(&raw_scan);
    let validate_median = median_ms(&raw_validate);
    let log_median = median_ms(&raw_log);
    eprintln!(
        "[phase1a] scan_cost S={total_events} pages={pages} scan_ms={scan_median:.3} \
         log_ms={log_median:.3} validate_ms={validate_median:.3} runs={runs} \
         raw_scan_ms={raw_scan:?} raw_log_ms={raw_log:?} raw_validate_ms={raw_validate:?} \
         part_calls={part_calls} part_ms_raw={raw_part:?}"
    );
}

#[test]
#[ignore = "manual benchmark"]
fn fragmented_doc_per_page_replay_cost() {
    // 同一篇 1 MiB 正文，拆成 256 条 4 KiB update：模拟真实库中“大文档 + 多批增量”
    // 的形状，验证 per-page `load_body_doc` 的成本是否被单条大 op 低估。
    const FRAG_CHUNKS: usize = 256;
    const FRAG_CHUNK_CHARS: usize = 4096;
    const DOC_ID: &str = "large-doc-frag";
    let (_dir, vault) = fresh_vault();
    let doc = seed_doc(&vault, DOC_ID, LOCAL_PEER);
    for index in 0..FRAG_CHUNKS {
        let chunk = pseudo_text(0xF00D_0000 + index as u64, FRAG_CHUNK_CHARS);
        let at = doc.scalar_len();
        doc.insert_scalars(at, &chunk).expect("追加分片");
        commit_local(&vault, &doc);
    }
    assert_eq!(doc.scalar_len(), FRAG_CHUNKS * FRAG_CHUNK_CHARS);
    let update_rows = count(
        &vault,
        &format!("SELECT count(*) FROM loro_updates WHERE doc_id='{DOC_ID}'"),
    );
    assert_eq!(update_rows, FRAG_CHUNKS as i64, "必须有 256 条 update");

    let mut replay_ms = Vec::with_capacity(LARGE_DOC_REPLAY_RUNS);
    for run in 0..LARGE_DOC_REPLAY_RUNS {
        let started = Instant::now();
        let (loaded, head) =
            load_body_doc(&vault, DOC_ID, LOCAL_PEER, None).expect("完整重放分片大文档");
        let ms = started.elapsed().as_secs_f64() * 1e3;
        assert_eq!(loaded.scalar_len(), FRAG_CHUNKS * FRAG_CHUNK_CHARS);
        assert_eq!(head.head_seq, FRAG_CHUNKS as u64);
        replay_ms.push(ms);
        eprintln!("[phase1a] frag_doc_replay run={run} replay_ms={ms:.3}");
    }
    eprintln!(
        "[phase1a] frag_doc_replay doc_mib=1.000 update_rows={update_rows}          chunks={FRAG_CHUNKS} chunk_bytes={FRAG_CHUNK_CHARS}          replay_ms_raw={replay_ms:?} replay_ms_median={:.3}",
        median_ms(&replay_ms),
    );
}

#[test]
#[ignore = "manual benchmark"]
fn checkpoint_validate_scan_cost() {
    for total_events in SCAN_COST_S {
        run_scan_cost(total_events);
    }
}

// ============================================================ M3 大文档 per-page replay 成本
//
// M3 标定：aggregate 每页（每批 pull）会为每个 **touched doc** 全量 `load_body_doc`
// （快照 + 该 doc 全部 `loro_updates` 重放）。§8.3.2 的 20 篇 × 107 KiB 夹具没有覆盖
// “单篇 1 MiB”这个形状；本探针造一篇确定性 1 MiB ASCII 正文（单条 insert op、
// 单条 `loro_updates` 行），连续 5 次 `load_body_doc`，打印 raw + 中位数（首轮含
// 冷页缓存，另给去掉首轮的 warm 中位数）；再各测一次 `pending_update(空 VV)` /
// `export_snapshot` / 从 snapshot `BodyDoc::restore` 作为旁路参考。只打印，
// 不 assert 时间。
//
// 注意：实测该形状的纯 replay ≈2 ms，远低于 §10.4 由 verify 折算的 ~2 s/篇；
// 后者含 baseline merge 的 attached import 成本，不是本探针的纯 replay 口径。

/// 大文档正文 id。
const LARGE_DOC_ID: &str = "large-doc";
/// 1 MiB 正文（ASCII：Loro scalar 数 == UTF-8 字节数）。
const LARGE_DOC_CHARS: usize = 1024 * 1024;
/// 完整重放次数（每次都是新的 `load_body_doc`，不共享 BodyDoc）。
const LARGE_DOC_REPLAY_RUNS: usize = 5;

#[test]
#[ignore = "manual benchmark"]
fn large_doc_per_page_replay_cost() {
    let (_dir, vault) = fresh_vault();
    let doc = seed_doc(&vault, LARGE_DOC_ID, LOCAL_PEER);
    let text = pseudo_text(0x1a2b_3c4d_5e6f_7081, LARGE_DOC_CHARS);
    assert_eq!(
        text.len(),
        LARGE_DOC_CHARS,
        "ASCII 正文的字节数必须正好 1 MiB"
    );
    doc.insert_scalars(0, &text).expect("写 1 MiB 正文");
    let head_seq = commit_local(&vault, &doc);
    let update_rows = count(
        &vault,
        &format!("SELECT count(*) FROM loro_updates WHERE doc_id='{LARGE_DOC_ID}'"),
    );
    assert_eq!(update_rows, 1, "per-page replay 要的是单条大文本 op 的形状");
    let payload_rows = vault
        .query(
            &format!("SELECT length(payload) FROM loro_updates WHERE doc_id='{LARGE_DOC_ID}'"),
            &[],
        )
        .expect("查询 update payload 长度");
    let payload_bytes = match payload_rows.first().and_then(|row| row.first()) {
        Some(Value::Integer(bytes)) => *bytes,
        other => panic!("payload 长度结果异常：{other:?}"),
    };
    assert!(payload_bytes > 0, "update payload 不能为空");

    // 5 次完整重放：每次都是一次新的 SQLite 读取 + snapshot/update import。
    let mut replay_ms = Vec::with_capacity(LARGE_DOC_REPLAY_RUNS);
    let mut last_loaded: Option<BodyDoc> = None;
    for run in 0..LARGE_DOC_REPLAY_RUNS {
        let started = Instant::now();
        let (loaded, head) =
            load_body_doc(&vault, LARGE_DOC_ID, LOCAL_PEER, None).expect("完整重放大文档");
        let ms = started.elapsed().as_secs_f64() * 1e3;
        assert_eq!(loaded.scalar_len(), LARGE_DOC_CHARS);
        assert_eq!(loaded.byte_len(), LARGE_DOC_CHARS);
        assert_eq!(head.head_seq, head_seq, "重放 head 必须与提交时一致");
        replay_ms.push(ms);
        last_loaded = Some(loaded);
        eprintln!("[phase1a] large_doc_replay run={run} replay_ms={ms:.3}");
    }
    let loaded = last_loaded.expect("至少重放一次");
    let replay_median = median_ms(&replay_ms);
    let warm_median = if replay_ms.len() > 1 {
        median_ms(&replay_ms[1..])
    } else {
        replay_median
    };

    // 参考项：不代表 per-page replay，只给出 snapshot / 全量 update 路径的量级。
    let pending_started = Instant::now();
    let pending = loaded
        .pending_update(&loro::VersionVector::default())
        .expect("全量 pending_update");
    let pending_ms = pending_started.elapsed().as_secs_f64() * 1e3;
    let snapshot_started = Instant::now();
    let snapshot = loaded.export_snapshot().expect("导出 1 MiB 快照");
    let snapshot_ms = snapshot_started.elapsed().as_secs_f64() * 1e3;
    let restore_started = Instant::now();
    let restored =
        BodyDoc::restore(LARGE_DOC_ID, LOCAL_PEER, Some(&snapshot), &[]).expect("从快照 restore");
    let restore_ms = restore_started.elapsed().as_secs_f64() * 1e3;
    assert_eq!(restored.scalar_len(), LARGE_DOC_CHARS);

    eprintln!(
        "[phase1a] large_doc_replay doc_mib={:.3} text_bytes={LARGE_DOC_CHARS} \
         loro_payload_bytes={payload_bytes} update_rows={update_rows} \
         replay_runs={LARGE_DOC_REPLAY_RUNS} replay_ms_raw={replay_ms:?} \
         replay_ms_median={replay_median:.3} replay_ms_warm_median={warm_median:.3}",
        LARGE_DOC_CHARS as f64 / (1024.0 * 1024.0),
    );
    eprintln!(
        "[phase1a] large_doc_replay_ref pending_update_ms={pending_ms:.3} \
         pending_bytes={} export_snapshot_ms={snapshot_ms:.3} snapshot_bytes={} \
         restore_from_snapshot_ms={restore_ms:.3}",
        pending.payload.len(),
        snapshot.len(),
    );
}
