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
//! ```bash
//! cargo test --release -p menghunote-sync --test phase1a_release -- --ignored --nocapture
//! ```

mod support;

use std::time::Instant;

use support::{commit_local, fresh_vault, seed_doc};

use menghunote_storage::{
    apply_baseline_batch, baseline_apply_debug_counters, baseline_apply_debug_timings,
    baseline_apply_state, begin_baseline_apply, finish_baseline_apply, load_body_doc,
    reset_baseline_apply_debug, sync_cursor_state, BaselineApplyDebugCounters,
    BaselineApplyDebugTimings, BaselineApplyFinish, BaselineApplyStatus, BodyDoc, Value, Vault,
    BASELINE_MERGE_BATCH, META_SYNC_CONFIRMED_HIGHWATER, META_SYNC_INTEGRATED_SEQ,
    META_SYNC_RECEIVED_SEQ, META_SYNC_RELAY_CURSOR,
};

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
    let begin_state =
        begin_baseline_apply(&vault, &[0x51; 16], BASELINE_S, "c0-source").expect("begin baseline");
    let begin_ms = begin_started.elapsed().as_millis();
    assert_eq!(begin_state.status, BaselineApplyStatus::Staging);
    assert!(begin_state.finishable);

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
