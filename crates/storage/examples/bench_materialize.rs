//! S6-1：批量物化正文基准（不启 UI）。
//!
//! 用法：
//! ```text
//! cargo run --release -p menghunote-storage --example bench_materialize
//! ```
//!
//! 造 2000 篇（结构 + A 正文，**不物化**），先逐篇 `materialize_body_profiled`
//! 测阶段分布，再重置后跑 `reconcile_batched(batch=200)`；报告写
//! `target/tmp/materialize-bench.txt`。

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use menghunote_storage::{
    append_updates, load_body_doc, materialize_body_profiled, reconcile_batched, scan_stale_bodies,
    MaterializeOutcome, MaterializeProgress, MaterializeStageTimings, UpdateRecord, Vault,
};

const NOTES: usize = 2_000;
const BATCH_SIZE: usize = 200;
const PEER: u64 = 9_700_001;

fn main() -> Result<(), Box<dyn Error>> {
    let tmp = target_tmp();
    fs::create_dir_all(&tmp)?;
    let vault_dir = tmp.join("materialize-bench-vault");
    let _ = fs::remove_dir_all(&vault_dir);
    fs::create_dir_all(&vault_dir)?;
    let vault = Vault::open(vault_dir.join("vault.sqlite3"))?;

    let seed_started = Instant::now();
    let (note_ids, bodies) = seed_notes(&vault, NOTES)?;
    let seed_ms = elapsed_ms(seed_started.elapsed());

    let stale = scan_stale_bodies(&vault)?;
    if stale.len() != NOTES {
        return Err(format!("播种后 stale={}，应为 {NOTES}", stale.len()).into());
    }

    // 1) 逐篇：阶段分解。
    let mut stage_samples = Vec::with_capacity(NOTES);
    let per_note_started = Instant::now();
    for item in &stale {
        let (outcome, timings) =
            materialize_body_profiled(&vault, &item.note_id, &item.doc_id, item.head_seq, PEER)?;
        if !matches!(outcome, MaterializeOutcome::Applied { .. }) {
            return Err(format!("{} 未应用：{outcome:?}", item.note_id).into());
        }
        stage_samples.push(timings);
    }
    let per_note_ms = elapsed_ms(per_note_started.elapsed());
    verify_all(&vault, &note_ids, &bodies)?;

    // 2) 重置到未物化状态，跑批量。
    reset_materialization(&vault)?;
    let stale = scan_stale_bodies(&vault)?;
    if stale.len() != NOTES {
        return Err(format!("重置后 stale={}，应为 {NOTES}", stale.len()).into());
    }

    let mut progress = Vec::new();
    let batch_started = Instant::now();
    let summary = reconcile_batched(&vault, PEER, BATCH_SIZE, &mut |item| progress.push(item))?;
    let batch_ms = elapsed_ms(batch_started.elapsed());
    verify_all(&vault, &note_ids, &bodies)?;

    let report = render_report(
        seed_ms,
        per_note_ms,
        &stage_samples,
        batch_ms,
        summary.materialized,
        summary.stale,
        summary.failed,
        &progress,
    );
    println!("{report}");
    let report_path = tmp.join("materialize-bench.txt");
    fs::write(&report_path, &report)?;
    println!("报告已写入: {}", report_path.display());
    Ok(())
}

fn seed_notes(vault: &Vault, count: usize) -> Result<(Vec<String>, Vec<String>), Box<dyn Error>> {
    let mut writer = menghunote_storage::StructureWriter::open(vault, PEER)?;
    let mut nodes = Vec::with_capacity(count);
    for index in 0..count {
        let title = format!("批量物化 {index:04}");
        let node = writer
            .doc()
            .create_note(&title, 1_800_000_000_000 + index as i64)?;
        nodes.push(node);
    }
    writer.commit("materialize-bench-structure")?;

    let mut note_ids = Vec::with_capacity(count);
    let mut bodies = Vec::with_capacity(count);
    for (index, node) in nodes.iter().enumerate() {
        let body = format!(
            "# 批量物化 {index:04}\n\n这是第 {index} 篇压测正文，包含中英混排 rust gpui sqlite。\n\
             第二行用于验证多行物化内容。\n"
        );
        let (doc, head) = load_body_doc(vault, &node.body_doc_id, PEER, None)?;
        doc.insert_scalars(0, &body)?;
        let base = head.version_vector()?;
        let update = doc.pending_update(&base)?;
        append_updates(
            vault,
            &node.body_doc_id,
            head.head_seq,
            &[UpdateRecord::new("materialize-bench-body", update)],
        )?;
        note_ids.push(node.note_id.clone());
        bodies.push(body);
    }
    Ok((note_ids, bodies))
}

fn reset_materialization(vault: &Vault) -> Result<(), Box<dyn Error>> {
    vault.exec(
        "UPDATE notes SET body_md = NULL, body_seq = 0, body_frontier = NULL",
        &[],
    )?;
    Ok(())
}

fn verify_all(vault: &Vault, note_ids: &[String], bodies: &[String]) -> Result<(), Box<dyn Error>> {
    for (note_id, expected) in note_ids.iter().zip(bodies) {
        let state = menghunote_storage::load_body_state(vault, note_id)?;
        if state.body_md.as_deref() != Some(expected.as_str()) {
            return Err(format!("{note_id} body_md 与 A 内容不一致").into());
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn render_report(
    seed_ms: f64,
    per_note_ms: f64,
    stages: &[MaterializeStageTimings],
    batch_ms: f64,
    materialized: usize,
    stale: usize,
    failed: usize,
    progress: &[MaterializeProgress],
) -> String {
    let frontier = stage_durations(stages, |s| s.frontier);
    let loro = stage_durations(stages, |s| s.loro_load);
    let text = stage_durations(stages, |s| s.text);
    let sql = stage_durations(stages, |s| s.sql);
    let event = stage_durations(stages, |s| s.event);
    let total = stage_durations(stages, |s| s.total());

    let mut out = String::new();
    out.push_str("menghunote materialize bench (S6-1)\n");
    out.push_str("====================================\n");
    out.push_str(&format!(
        "corpus: notes={NOTES}, batch_size={BATCH_SIZE}, seed={seed_ms:.1}ms\n"
    ));
    out.push_str(&format!(
        "stale_after_seed={}, per_note_wall={per_note_ms:.1}ms\n\n",
        stages.len()
    ));
    out.push_str("per-note stage distribution (ms):\n");
    out.push_str("  stage              total      mean       p50       p95\n");
    for (name, stats) in [
        ("frontier lookup", &frontier),
        ("loro load/apply", &loro),
        ("text+scalar", &text),
        ("sql update+commit", &sql),
        ("publish event", &event),
        ("stage total", &total),
    ] {
        out.push_str(&format!(
            "  {:<18} {:>8.1} {:>8.2} {:>8.2} {:>8.2}\n",
            name, stats.total_ms, stats.mean_ms, stats.p50_ms, stats.p95_ms
        ));
    }
    out.push_str(&format!(
        "\nper-note wall: {per_note_ms:.1}ms ({:.1}ms/note)\n",
        per_note_ms / NOTES as f64
    ));
    out.push_str(&format!(
        "batched wall: {batch_ms:.1}ms ({:.1}ms/note), speedup={:.2}x\n",
        batch_ms / NOTES as f64,
        per_note_ms / batch_ms.max(0.001)
    ));
    out.push_str(&format!(
        "batch summary: materialized={materialized}, stale={stale}, failed={failed}\n"
    ));
    out.push_str(&format!(
        "progress callbacks: {}, last={:?}\n",
        progress.len(),
        progress.last()
    ));
    out.push_str("note: 'loro load/apply' 包含 loro_updates 读取 + Loro 重放/解码；\n");
    out.push_str("      'sql update+commit' 是整批/单篇 UPDATE + outbox DELETE + commit。\n");
    out
}

struct DurationStats {
    total_ms: f64,
    mean_ms: f64,
    p50_ms: f64,
    p95_ms: f64,
}

fn stage_durations(
    stages: &[MaterializeStageTimings],
    get: impl Fn(&MaterializeStageTimings) -> Duration,
) -> DurationStats {
    let mut values: Vec<f64> = stages
        .iter()
        .map(|stage| get(stage).as_secs_f64() * 1_000.0)
        .collect();
    let total_ms: f64 = values.iter().sum();
    if values.is_empty() {
        return DurationStats {
            total_ms: 0.0,
            mean_ms: 0.0,
            p50_ms: 0.0,
            p95_ms: 0.0,
        };
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let at = |p: f64| values[((values.len() - 1) as f64 * p).round() as usize];
    DurationStats {
        total_ms,
        mean_ms: total_ms / values.len() as f64,
        p50_ms: at(0.50),
        p95_ms: at(0.95),
    }
}

fn elapsed_ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

fn target_tmp() -> PathBuf {
    if let Some(dir) = std::env::var_os("CARGO_TARGET_TMPDIR") {
        return PathBuf::from(dir);
    }
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("target/tmp")
}
