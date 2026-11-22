//! S4-4a：检索性能 bench（不启 UI）。
//!
//! 运行：
//! ```text
//! cargo run --release --example bench_search
//! cargo run --release --example bench_search -- --notes 200 --incremental 5 --save 20
//! ```
//!
//! 数据在 `target/tmp/search-bench-vault`（`CARGO_TARGET_TMPDIR` 可用时优先用它），
//! 报告写到 `target/tmp/search-bench.txt`。
//!
//! 门槛来自 ADR 0005 §5，**不在这份 bench 里改门槛**：
//! - 全量重建 wall < 3s；
//! - 增量可见 p95 < 1s；
//! - 热查询 p95 < 50ms。

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use menghunote_search::{FlushOutcome, IndexManager, DEBOUNCE};
use menghunote_storage::{
    append_updates, load_body_doc, materialize_body, Statement, StructureWriter, UpdateRecord,
    Value, Vault, APP_DOC_SCHEMA, CODEC_VERSION, MIN_READER,
};

const DEFAULT_NOTES: usize = 2_000;
const DEFAULT_LONG_DOCS: usize = 3;
const DEFAULT_INCREMENTAL: usize = 20;
const DEFAULT_SAVE: usize = 100;
const QUERY_ITERATIONS: usize = 20;
const PEER: u64 = 9_400_001;

const REBUILD_BUDGET_MS: f64 = 3_000.0;
const INCREMENTAL_BUDGET_MS: f64 = 1_000.0;
const QUERY_BUDGET_MS: f64 = 50.0;

fn main() -> Result<(), Box<dyn Error>> {
    let args = Args::parse();
    let tmp = target_tmp();
    fs::create_dir_all(&tmp)?;

    let vault_dir = tmp.join("search-bench-vault");
    let _ = fs::remove_dir_all(&vault_dir);
    fs::create_dir_all(&vault_dir)?;
    let vault = Vault::open(vault_dir.join("vault.sqlite3"))?;

    // 真实 storage 路径的一篇短文：增量可见性与保存回归都用它。
    let real_started = Instant::now();
    let real = seed_real_note(&vault, "增量目标", "初始内容：笔记流程 rust 压测。", 1)?;
    let real_ms = real_started.elapsed().as_secs_f64() * 1_000.0;

    let corpus_started = Instant::now();
    let corpus = seed_bulk(&vault, args.notes, args.long_docs)?;
    let corpus_ms = corpus_started.elapsed().as_secs_f64() * 1_000.0;
    let vault_bytes = dir_size(&vault_dir);

    // 保存基线：无索引时连续 A 事务（DurableAck 的代理；不等待搜索）。
    let save_without_index = measure_save(&vault, &real, args.save)?;

    // 全量重建：IndexManager 首次 open（代构建 + 启动对账）。
    let rebuild_started = Instant::now();
    let mut manager = IndexManager::open(&vault)?;
    let rebuild_ms = rebuild_started.elapsed().as_secs_f64() * 1_000.0;
    let index_size_after_build = dir_size(manager.generation_dir());

    let queries = measure_queries(&mut manager)?;
    let incremental = measure_incremental(&vault, &mut manager, &real, args.incremental)?;
    let save_with_index = measure_save(&vault, &real, args.save)?;
    let index_size_final = dir_size(manager.generation_dir());
    let peak_rss = peak_rss_mib();

    let query_p95_worst = queries
        .iter()
        .map(|query| query.stats.p95_ms)
        .fold(0.0_f64, f64::max);
    let report = render_report(
        &args,
        &corpus,
        real_ms,
        corpus_ms,
        rebuild_ms,
        index_size_after_build,
        index_size_final,
        vault_bytes,
        &queries,
        &incremental,
        &save_without_index,
        &save_with_index,
        peak_rss,
        query_p95_worst,
    );

    println!("{report}");
    let report_path = target_tmp().join("search-bench.txt");
    fs::write(&report_path, &report)?;
    println!("报告已写入: {}", report_path.display());
    Ok(())
}

#[derive(Debug, Clone)]
struct Args {
    notes: usize,
    long_docs: usize,
    incremental: usize,
    save: usize,
}

impl Args {
    fn parse() -> Self {
        let mut args = Self {
            notes: DEFAULT_NOTES,
            long_docs: DEFAULT_LONG_DOCS,
            incremental: DEFAULT_INCREMENTAL,
            save: DEFAULT_SAVE,
        };
        let mut iter = std::env::args().skip(1);
        while let Some(arg) = iter.next() {
            let (key, inline) = match arg.split_once('=') {
                Some((key, value)) => (key, Some(value.to_owned())),
                None => (arg.as_str(), None),
            };
            match key {
                "--notes" => args.notes = next_usize(&mut iter, inline, "--notes"),
                "--long-docs" => args.long_docs = next_usize(&mut iter, inline, "--long-docs"),
                "--incremental" => {
                    args.incremental = next_usize(&mut iter, inline, "--incremental")
                }
                "--save" => args.save = next_usize(&mut iter, inline, "--save"),
                "--help" | "-h" => {
                    println!(
                        "用法: bench_search [--notes N] [--long-docs N] [--incremental N] [--save N]"
                    );
                    std::process::exit(0);
                }
                other => {
                    eprintln!("未知参数: {other}（--help 查看）");
                    std::process::exit(2);
                }
            }
        }
        args.notes = args.notes.max(1);
        args
    }
}

fn next_usize(
    iter: &mut impl Iterator<Item = String>,
    inline: Option<String>,
    name: &str,
) -> usize {
    let raw = inline.or_else(|| iter.next()).unwrap_or_else(|| {
        eprintln!("{name} 需要一个整数");
        std::process::exit(2);
    });
    raw.parse().unwrap_or_else(|_| {
        eprintln!("{name} 需要整数，实际 {raw:?}");
        std::process::exit(2);
    })
}

#[derive(Debug, Clone)]
struct Corpus {
    shorts: usize,
    long_docs: usize,
    short_bytes: usize,
    long_bytes: usize,
}

#[derive(Debug, Clone)]
struct RealNote {
    note_id: String,
    body_doc_id: String,
}

fn seed_real_note(
    vault: &Vault,
    title: &str,
    body: &str,
    now: i64,
) -> Result<RealNote, Box<dyn Error>> {
    let mut writer = StructureWriter::open(vault, PEER)?;
    let node = writer.doc().create_note(title, now)?;
    writer.commit("bench-seed-real")?;
    let (doc, head) = load_body_doc(vault, &node.body_doc_id, PEER, None)?;
    doc.insert_scalars(0, body)?;
    let base = head.version_vector()?;
    let update = doc.pending_update(&base)?;
    let head = append_updates(
        vault,
        &node.body_doc_id,
        head.head_seq,
        &[UpdateRecord::new("bench-seed-real", update)],
    )?;
    materialize_body(vault, &node.note_id, &node.body_doc_id, head.head_seq, PEER)?;
    Ok(RealNote {
        note_id: node.note_id,
        body_doc_id: node.body_doc_id,
    })
}

fn seed_bulk(vault: &Vault, notes: usize, long_docs: usize) -> Result<Corpus, Box<dyn Error>> {
    let now = now_ms();
    let shorts = notes.saturating_sub(1); // 真实 note 占掉一篇短文
    let mut statements = Vec::with_capacity((shorts + long_docs) * 2);
    let mut short_bytes = 0usize;
    let mut long_bytes = 0usize;

    for index in 0..shorts {
        let body = short_body(index);
        short_bytes += body.len();
        push_insert(
            &mut statements,
            &format!("bench-body-short-{index}"),
            &format!("bench-note-short-{index}"),
            &format!("压测短文 {index}"),
            &body,
            now + index as i64,
        );
    }
    for index in 0..long_docs {
        let body = long_body(index);
        long_bytes += body.len();
        push_insert(
            &mut statements,
            &format!("bench-body-long-{index}"),
            &format!("bench-note-long-{index}"),
            &format!("长文压测 {index}"),
            &body,
            now + 10_000 + index as i64,
        );
    }

    if !statements.is_empty() {
        vault.exec_transaction(&statements, &[])?;
    }
    Ok(Corpus {
        shorts: shorts + 1,
        long_docs,
        short_bytes,
        long_bytes,
    })
}

fn push_insert(
    statements: &mut Vec<Statement>,
    body_doc_id: &str,
    note_id: &str,
    title: &str,
    body_md: &str,
    updated_at: i64,
) {
    statements.push(Statement::new(
        "INSERT INTO loro_docs(doc_id, kind, generation, app_doc_schema, codec_version, min_reader) \
         VALUES(?1, 'body', 1, ?2, ?3, ?4)",
        vec![
            Value::Text(body_doc_id.to_owned()),
            Value::Integer(APP_DOC_SCHEMA),
            Value::Text(CODEC_VERSION.to_owned()),
            Value::Integer(MIN_READER),
        ],
    ));
    statements.push(Statement::new(
        "INSERT INTO notes(note_id, body_doc_id, title, body_md, body_seq, body_frontier, \
         meta_seq, created_at, updated_at, deleted, is_favorite, is_pinned) \
         VALUES(?1, ?2, ?3, ?4, 1, NULL, 1, ?5, ?5, 0, 0, 0)",
        vec![
            Value::Text(note_id.to_owned()),
            Value::Text(body_doc_id.to_owned()),
            Value::Text(title.to_owned()),
            Value::Text(body_md.to_owned()),
            Value::Integer(updated_at),
        ],
    ));
}

fn short_body(index: usize) -> String {
    let mut body = format!(
        "第 {index} 篇压测笔记。这是一段中英混排的正文，用来验证检索性能与中文分词。\
         rust async gpui sqlite tantivy；笔记流程、压测、性能、搜索、索引。"
    );
    while body.chars().count() < 360 {
        body.push_str("这里补一些中文内容，让短文落在三百到六百字之间，并保持可搜索的笔记流程。");
    }
    if body.chars().count() > 600 {
        body = body.chars().take(600).collect();
    }
    body.push_str(&format!("\n\n尾部唯一标记 short-marker-{index}。"));
    body
}

fn long_body(index: usize) -> String {
    let mut body = String::with_capacity(1_100_000);
    let block = format!(
        "## 长文压测 {index}\n\n\
         这是约 1MB 的中英混排长文，包含代码块与 emoji 🚀😀，用于测试 snippet/查询延迟。\n\
         rust async gpui sqlite tantivy 笔记流程 压测 性能。\n\n\
         ```rust\n\
         fn bench_{index}() {{ let note = \"笔记流程\"; let word = \"rust\"; }}\n\
         ```\n\n\
         > 引用块也要能转换成纯文本。\n\
         - 列表项一：搜索索引\n\
         - 列表项二：中文分词\n\n"
    );
    while body.len() < 1_000_000 {
        body.push_str(&block);
    }
    body
}

#[derive(Debug, Clone)]
struct Stats {
    n: usize,
    p50_ms: f64,
    p95_ms: f64,
    mean_ms: f64,
}

fn stats(durations: &[Duration]) -> Stats {
    if durations.is_empty() {
        return Stats {
            n: 0,
            p50_ms: 0.0,
            p95_ms: 0.0,
            mean_ms: 0.0,
        };
    }
    let mut values: Vec<f64> = durations
        .iter()
        .map(|duration| duration.as_secs_f64() * 1_000.0)
        .collect();
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let index = |p: f64| ((values.len() - 1) as f64 * p).round() as usize;
    Stats {
        n: values.len(),
        p50_ms: values[index(0.50)],
        p95_ms: values[index(0.95)],
        mean_ms: values.iter().sum::<f64>() / values.len() as f64,
    }
}

#[derive(Debug, Clone)]
struct QueryBench {
    name: &'static str,
    query: &'static str,
    stats: Stats,
    hits: usize,
}

fn measure_queries(manager: &mut IndexManager) -> Result<Vec<QueryBench>, Box<dyn Error>> {
    let queries = [
        ("chinese", "笔记"),
        ("english", "rust"),
        ("phrase", "\"笔记流程\""),
        ("multi_token", "笔记 rust"),
    ];
    let mut out = Vec::new();
    for (name, query) in queries {
        // 预热（不计入统计）。
        for _ in 0..3 {
            let hits = manager.query(query, 10)?;
            if hits.is_empty() {
                return Err(format!("query {query:?} 预热无命中，bench 数据不满足假设").into());
            }
        }
        let mut durations = Vec::with_capacity(QUERY_ITERATIONS);
        let mut hits = 0usize;
        for _ in 0..QUERY_ITERATIONS {
            let started = Instant::now();
            let result = manager.query(query, 10)?;
            durations.push(started.elapsed());
            hits = result.len();
        }
        out.push(QueryBench {
            name,
            query,
            stats: stats(&durations),
            hits,
        });
    }
    Ok(out)
}

#[derive(Debug, Clone)]
struct IncrementalBench {
    stats: Stats,
    event_misses: usize,
    final_hits: usize,
}

fn measure_incremental(
    vault: &Vault,
    manager: &mut IndexManager,
    real: &RealNote,
    iterations: usize,
) -> Result<IncrementalBench, Box<dyn Error>> {
    let (doc, mut head) = load_body_doc(vault, &real.body_doc_id, PEER, None)?;
    let mut durations = Vec::with_capacity(iterations);
    let mut event_misses = 0usize;
    let mut final_hits = 0usize;

    for index in 0..iterations {
        let started = Instant::now();
        let marker = format!(" inc{index}");
        doc.insert_scalars(doc.scalar_len(), &marker)?;
        let base = head.version_vector()?;
        let update = doc.pending_update(&base)?;
        head = append_updates(
            vault,
            &real.body_doc_id,
            head.head_seq,
            &[UpdateRecord::new(format!("bench-inc-{index}"), update)],
        )?;
        materialize_body(vault, &real.note_id, &real.body_doc_id, head.head_seq, PEER)?;

        let now = Instant::now();
        let drained = manager.poll_events(now)?;
        if drained == 0 {
            // 正常路径不该发生；为了 bench 可重复，显式标记（并计数），
            // 不能把事件丢失误算成索引延迟。
            event_misses += 1;
            manager.mark_dirty_at(&real.note_id, now);
        }
        let due = now + DEBOUNCE + Duration::from_millis(1);
        let outcome = manager.flush_if_due(due)?;
        if matches!(outcome, FlushOutcome::Deferred { .. }) {
            manager.flush_now()?;
        }
        let hits = manager.query(&format!("inc{index}"), 10)?;
        if hits.is_empty() || hits.iter().all(|hit| hit.note_id != real.note_id) {
            return Err(format!("增量第 {index} 次查询未命中 {}", real.note_id).into());
        }
        final_hits = hits.len();
        durations.push(started.elapsed());
    }

    Ok(IncrementalBench {
        stats: stats(&durations),
        event_misses,
        final_hits,
    })
}

fn measure_save(
    vault: &Vault,
    real: &RealNote,
    iterations: usize,
) -> Result<Stats, Box<dyn Error>> {
    let (doc, mut head) = load_body_doc(vault, &real.body_doc_id, PEER, None)?;
    let mut durations = Vec::with_capacity(iterations);
    for index in 0..iterations {
        let started = Instant::now();
        doc.insert_scalars(doc.scalar_len(), "x")?;
        let base = head.version_vector()?;
        let update = doc.pending_update(&base)?;
        head = append_updates(
            vault,
            &real.body_doc_id,
            head.head_seq,
            &[UpdateRecord::new(format!("bench-save-{index}"), update)],
        )?;
        durations.push(started.elapsed());
    }
    Ok(stats(&durations))
}

#[allow(clippy::too_many_arguments)]
fn render_report(
    args: &Args,
    corpus: &Corpus,
    real_ms: f64,
    corpus_ms: f64,
    rebuild_ms: f64,
    index_bytes_build: u64,
    index_bytes_final: u64,
    vault_bytes: u64,
    queries: &[QueryBench],
    incremental: &IncrementalBench,
    save_without_index: &Stats,
    save_with_index: &Stats,
    peak_rss_mib: Option<f64>,
    query_p95_worst: f64,
) -> String {
    let rebuild_pass = rebuild_ms < REBUILD_BUDGET_MS;
    let incremental_pass = incremental.stats.p95_ms < INCREMENTAL_BUDGET_MS;
    let query_pass = query_p95_worst < QUERY_BUDGET_MS;
    let mut out = String::new();
    out.push_str("menghunote search bench (ADR 0005 §5)\n");
    out.push_str("=====================================\n");
    out.push_str(&format!(
        "corpus: notes={} (short={} incl. real note, long={}), short_bytes={}, long_bytes={}\n",
        corpus.shorts + corpus.long_docs,
        corpus.shorts,
        corpus.long_docs,
        corpus.short_bytes,
        corpus.long_bytes
    ));
    out.push_str(&format!(
        "params: notes={} long_docs={} incremental={} save={}\n",
        args.notes, args.long_docs, args.incremental, args.save
    ));
    out.push_str(&format!(
        "setup: real_note={real_ms:.1}ms, bulk_corpus={corpus_ms:.1}ms, vault={:.2}MiB\n",
        bytes_mib(vault_bytes)
    ));
    out.push_str(&format!(
        "rebuild: {rebuild_ms:.1}ms  (budget {REBUILD_BUDGET_MS:.0}ms, {})\n",
        verdict(rebuild_pass)
    ));
    if !rebuild_pass {
        out.push_str(&format!(
            "  over budget by {:.1}ms\n",
            rebuild_ms - REBUILD_BUDGET_MS
        ));
    }
    out.push_str(&format!(
        "index_disk: build={:.2}MiB, final={:.2}MiB\n",
        bytes_mib(index_bytes_build),
        bytes_mib(index_bytes_final)
    ));
    match peak_rss_mib {
        Some(mib) => out.push_str(&format!(
            "peak_rss: {mib:.1}MiB (Windows PeakWorkingSet64)\n"
        )),
        None => out.push_str("peak_rss: N/A（无法读取 PeakWorkingSet64）\n"),
    }
    out.push_str("\nqueries (10 hits, warmup 3, measured 20):\n");
    out.push_str("  name         query                 p50       p95       mean      hits\n");
    for query in queries {
        out.push_str(&format!(
            "  {:<11} {:<20} {:>7.2}ms {:>7.2}ms {:>7.2}ms {:>4}\n",
            query.name,
            format!("{:?}", query.query),
            query.stats.p50_ms,
            query.stats.p95_ms,
            query.stats.mean_ms,
            query.hits
        ));
    }
    out.push_str(&format!(
        "query p95 worst: {query_p95_worst:.2}ms (budget {QUERY_BUDGET_MS:.0}ms, {})\n",
        verdict(query_pass)
    ));

    out.push_str(&format!(
        "\nincremental: n={}, p50={:.1}ms, p95={:.1}ms, mean={:.1}ms, event_misses={}, final_hits={}\n",
        incremental.stats.n,
        incremental.stats.p50_ms,
        incremental.stats.p95_ms,
        incremental.stats.mean_ms,
        incremental.event_misses,
        incremental.final_hits
    ));
    out.push_str(&format!(
        "incremental p95: {:.1}ms (budget {:.0}ms, {})\n",
        incremental.stats.p95_ms,
        INCREMENTAL_BUDGET_MS,
        verdict(incremental_pass)
    ));
    if !incremental_pass {
        out.push_str(&format!(
            "  over budget by {:.1}ms\n",
            incremental.stats.p95_ms - INCREMENTAL_BUDGET_MS
        ));
    }

    out.push_str("\nsave regression (A-commit proxy; not a full DurableAck harness):\n");
    out.push_str(&format!(
        "  no_index:   p50={:.2}ms p95={:.2}ms mean={:.2}ms n={}\n",
        save_without_index.p50_ms,
        save_without_index.p95_ms,
        save_without_index.mean_ms,
        save_without_index.n
    ));
    out.push_str(&format!(
        "  with_index: p50={:.2}ms p95={:.2}ms mean={:.2}ms n={}\n",
        save_with_index.p50_ms, save_with_index.p95_ms, save_with_index.mean_ms, save_with_index.n
    ));
    let delta = save_with_index.p95_ms - save_without_index.p95_ms;
    out.push_str(&format!(
        "  p95 delta: {delta:+.2}ms (no numeric gate in this bench)\n"
    ));

    out.push_str("\nverdict:\n");
    out.push_str(&format!(
        "  rebuild <3s: {}{}\n",
        verdict(rebuild_pass),
        if rebuild_pass {
            String::new()
        } else {
            format!(" ({:.1}ms over)", rebuild_ms - REBUILD_BUDGET_MS)
        }
    ));
    out.push_str(&format!(
        "  incremental p95 <1s: {}{}\n",
        verdict(incremental_pass),
        if incremental_pass {
            String::new()
        } else {
            format!(
                " ({:.1}ms over)",
                incremental.stats.p95_ms - INCREMENTAL_BUDGET_MS
            )
        }
    ));
    out.push_str(&format!(
        "  hot query p95 <50ms: {}{}\n",
        verdict(query_pass),
        if query_pass {
            String::new()
        } else {
            format!(" ({:.2}ms over)", query_p95_worst - QUERY_BUDGET_MS)
        }
    ));
    out
}

fn verdict(pass: bool) -> &'static str {
    if pass {
        "PASS"
    } else {
        "FAIL"
    }
}

fn bytes_mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

fn target_tmp() -> PathBuf {
    if let Some(dir) = std::env::var_os("CARGO_TARGET_TMPDIR") {
        return PathBuf::from(dir);
    }
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("target/tmp")
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

fn dir_size(path: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(path) else {
        return 0;
    };
    let mut total = 0u64;
    for entry in entries.flatten() {
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if metadata.is_dir() {
            total = total.saturating_add(dir_size(&entry.path()));
        } else {
            total = total.saturating_add(metadata.len());
        }
    }
    total
}

#[cfg(windows)]
fn peak_rss_mib() -> Option<f64> {
    let pid = std::process::id();
    let script = format!("(Get-Process -Id {pid}).PeakWorkingSet64");
    let output = Command::new("powershell")
        .args(["-NoProfile", "-Command", &script])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let bytes = text.trim().parse::<u64>().ok()?;
    Some(bytes as f64 / (1024.0 * 1024.0))
}

#[cfg(not(windows))]
fn peak_rss_mib() -> Option<f64> {
    let status = fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(value) = line.strip_prefix("VmHWM:") {
            let kb = value.split_whitespace().next()?.parse::<f64>().ok()?;
            return Some(kb / 1024.0);
        }
    }
    None
}
