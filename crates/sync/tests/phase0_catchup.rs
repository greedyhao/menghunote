//! Phase 0 → 1A：两设备「落后追赶」的计数/耗时基准（手动运行，不 assert 时间）。
//!
//! 场景：设备 A 先产生 N 条正文 update 并推到 relay；设备 B 从 c0（很早的游标）开始
//! 逐页 `pull → integrate`，直到 relay 日志被追平。对同一份日志跑三种模式：
//!
//! - `per_row`：`inbox_batch=false` + `IntegrateConfig { aggregate_body: false }`
//!   （Phase 0 的逐行路径：每条 update 一次 `store_inbox_verified` / 一次
//!   `load_body_doc` 全量重放 / 一次 A 事务 + `mark_integrated`）；
//! - `batch`：`inbox_batch=true` + `aggregate_body: false`
//!   （Phase 0 的批量 inbox + 逐行集成）；
//! - `aggregate`：`inbox_batch=true` + `aggregate_body: true`
//!   （Phase 1A 目标：整页 verified 行一个事务 + 同一文档一批只全量重放一次）。
//!
//! 只比较**结果一致**（事件数、字节、游标、水位、最终正文）与耗时，不对时间做断言。
//! 集成一律走 [`SyncEngine::integrate_dependency_ordered`]，否则无法关闭聚合
//! （`integrate(limit)` 默认 `aggregate_body = true`）。
//!
//! ```bash
//! cargo test --release -p menghunote-sync --test phase0_catchup -- --ignored --nocapture
//! ```
//!
//! 不 sleep、不读真实网络：`FakeRelay` 是纯内存 relay，耗时反映本地 SQLite/Loro 成本。

mod support;

use std::time::Instant;

use support::*;

use menghunote_storage::{load_body_doc, sync_cursor_state, BodyDoc};
use menghunote_sync::engine::{
    EngineConfig, EngineIdentity, IntegrateConfig, PullReport, SyncEngine,
};

/// 单文档场景每个 update 的正文片段（约 64B，贴近"小编辑"；不是 4KB 大块）。
fn edit_text(index: usize) -> String {
    format!("-{index:05}-{}", "x".repeat(56))
}

/// 多文档场景每个 update 的正文片段（439B）。
///
/// 20 篇 × 250 条 × 439B ≈ 107 KiB/篇（> 100 KB），同时保持 update 条数 5000 不变。
fn edit_text_long(index: usize) -> String {
    format!("-{index:05}-{}", "y".repeat(432))
}

/// 集成预算：沿用旧 `integrate(1_000_000)` 的语义（`limit` 只折算批次上限，
/// 本场景每页最多 256 行）。
const INTEGRATE_LIMIT: usize = 1_000_000;

fn doc_id(index: usize) -> String {
    format!("doc-{index:02}")
}

/// 三种追赶模式。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    PerRow,
    Batch,
    Aggregate,
}

impl Mode {
    const ALL: [Mode; 3] = [Mode::PerRow, Mode::Batch, Mode::Aggregate];

    fn name(self) -> &'static str {
        match self {
            Self::PerRow => "per_row",
            Self::Batch => "batch",
            Self::Aggregate => "aggregate",
        }
    }

    fn inbox_batch(self) -> bool {
        !matches!(self, Self::PerRow)
    }

    fn aggregate_body(self) -> bool {
        matches!(self, Self::Aggregate)
    }

    fn engine_config(self) -> EngineConfig {
        EngineConfig {
            inbox_batch: self.inbox_batch(),
            ..EngineConfig::default()
        }
    }

    /// `integrate_dependency_ordered` 的配置；用旧的 limit 折算批数上限，再显式
    /// 打开/关闭正文聚合。
    fn integrate_config(self) -> IntegrateConfig {
        let mut config = IntegrateConfig::with_object_budget(INTEGRATE_LIMIT);
        config.aggregate_body = self.aggregate_body();
        config
    }
}

/// 单个配置下的一次追赶统计。
#[derive(Debug, Default)]
struct CaseStats {
    rounds: usize,
    events: usize,
    stored: usize,
    skipped: usize,
    integrated: usize,
    update_bytes: u64,
    asset_bytes: u64,
    pull_elapsed_ms: u64,
    integrate_elapsed_ms: u64,
    pull_wall_ms: u128,
    integrate_wall_ms: u128,
    doc_loads: usize,
    batches: usize,
    max_deferred: usize,
    max_page_events: usize,
}

impl CaseStats {
    fn total_wall_ms(&self) -> u128 {
        self.pull_wall_ms + self.integrate_wall_ms
    }
}

fn engine_identity(device: &Device) -> EngineIdentity {
    EngineIdentity {
        device_id: device.identity.device_id,
        signing_key: device.identity.signing_key.clone(),
        peer_id: device.identity.peer_id,
    }
}

/// 跑一次完整追赶，返回统计（内部断言结果正确，只不测时间）。
///
/// `docs` 篇文档、每篇 `updates_per_doc` 条 update（总计 `docs * updates_per_doc`）。
/// A 按 **doc 轮转** 提交（每轮每篇一条），并且**每轮提交后立刻推送**：生产
/// `outbox_rows` 按 `doc_id,seq` 排序，只有"每篇每轮恰好一条 pending"时 relay 日志
/// 才会保持逐轮、跨文档交错的顺序（否则会先集中推完第一篇）。
fn run_case(
    mode: Mode,
    docs: usize,
    updates_per_doc: usize,
    text_for: fn(usize) -> String,
    scenario: &str,
) -> CaseStats {
    ensure_test_identity_wrap();
    let (_a_dir, vault_a) = fresh_vault();
    let (_b_dir, vault_b) = fresh_vault();
    let device_a = Device::new(0x61);
    let device_b = Device::new(0x62);
    let mut relay = FakeRelay::new(&[&device_a, &device_b]);
    let updates = docs * updates_per_doc;

    // ---- A：docs 篇文档，按 doc 轮转提交 + 每轮推送 ----
    let docs_a: Vec<BodyDoc> = (0..docs)
        .map(|doc| seed_doc(&vault_a, &doc_id(doc), device_a.identity.peer_id))
        .collect();
    let mut positions = vec![0usize; docs];
    let mut published = 0usize;
    let mut commit_index = 0usize;
    {
        // 引擎持有 `&mut relay`；本轮内只做加密/推送，不再借 relay。
        let mut engine = SyncEngine::new(
            &vault_a,
            &mut relay,
            engine_identity(&device_a),
            device_a.root.clone(),
        );
        for _round in 0..updates_per_doc {
            for doc in 0..docs {
                commit_index += 1;
                let text = text_for(commit_index);
                docs_a[doc]
                    .insert_scalars(positions[doc], &text)
                    .expect("A 写正文");
                positions[doc] += text.chars().count();
                let mut object_id = [0u8; 32];
                object_id[..8].copy_from_slice(&(commit_index as u64).to_be_bytes());
                object_id[31] = 0x61;
                commit_with_intent(
                    &vault_a,
                    &docs_a[doc],
                    &uuid_like(commit_index as u64),
                    object_id,
                    EPOCH,
                );
            }
            // 本轮恰好 docs 条 pending（每篇一条），推送顺序 = doc_id 升序 = 轮转顺序。
            let encrypted = engine.encrypt_pending(256).expect("A 加密");
            let push = engine.push().expect("A 推送");
            assert_eq!(push.acked, encrypted, "本轮推送必须全部 durable ACK");
            assert_eq!(encrypted, docs, "每轮必须恰好加密本轮 {docs} 条");
            published += encrypted;
        }
    }
    assert_eq!(published, updates, "A 必须推完所有 update");
    let head_seq = relay.head_seq;
    assert_eq!(head_seq, updates as u64);

    // ---- B：从 c0 逐页追赶 ----
    for doc in 0..docs {
        seed_doc(&vault_b, &doc_id(doc), device_b.identity.peer_id);
    }
    seed_cursor(&vault_b);
    let mut stats = CaseStats::default();
    loop {
        // 本轮开始前的历史行数（探针重放的就是这段历史）。
        let history_before = sync_cursor_state(&vault_b)
            .map(|state| state.integrated_seq)
            .unwrap_or(0);
        // 探针（不计入下面两段计时）：一次完整重放第 0 篇文档历史的成本。
        // integrate 的每个 remote import 都会走 `load_body_doc` → 全量重放，
        // 这个数字应当随轮次增长，用来解释 integrate 的超线性。
        let replay_started = Instant::now();
        let _ =
            load_body_doc(&vault_b, &doc_id(0), device_b.identity.peer_id, None).expect("重放探针");
        let replay_ms = replay_started.elapsed().as_millis();

        let pull_started = Instant::now();
        let pull: PullReport = {
            let mut sync = SyncEngine::with_config(
                &vault_b,
                &mut relay,
                engine_identity(&device_b),
                device_b.root.clone(),
                mode.engine_config(),
            );
            sync.pull().expect("B pull")
        };
        let pull_wall = pull_started.elapsed();
        stats.rounds += 1;
        stats.pull_wall_ms += pull_wall.as_millis();
        stats.pull_elapsed_ms += pull.elapsed_ms;
        stats.events += pull.events;
        stats.stored += pull.stored;
        stats.skipped += pull.skipped;
        stats.update_bytes += pull.update_bytes;
        stats.asset_bytes += pull.asset_bytes;
        stats.max_page_events = stats.max_page_events.max(pull.events);

        let integrate_started = Instant::now();
        let integrate = {
            let mut sync = SyncEngine::with_config(
                &vault_b,
                &mut relay,
                engine_identity(&device_b),
                device_b.root.clone(),
                mode.engine_config(),
            );
            // Phase 1A：必须走 dependency_ordered + 显式 IntegrateConfig，
            // 否则无法关闭正文聚合，per_row/batch 就不是旧路径。
            sync.integrate_dependency_ordered(mode.integrate_config())
                .expect("B integrate")
        };
        let integrate_wall = integrate_started.elapsed();
        let report = integrate.report;
        stats.integrate_wall_ms += integrate_wall.as_millis();
        stats.integrate_elapsed_ms += report.elapsed_ms;
        stats.integrated += report.integrated;
        stats.doc_loads += report.doc_loads;
        stats.batches += report.batches;
        stats.max_deferred = stats.max_deferred.max(report.deferred);
        eprintln!(
            "[phase1a] scenario={scenario} mode={} docs={} updates={} round={} history_rows={} \
             replay_probe_ms={} events={} stored={} skipped={} update_bytes={} pull_ms={} \
             integrate_ms={} doc_loads={} batches={} integrated={} deferred={}",
            mode.name(),
            docs,
            updates,
            stats.rounds,
            history_before,
            replay_ms,
            pull.events,
            pull.stored,
            pull.skipped,
            pull.update_bytes,
            pull.elapsed_ms,
            report.elapsed_ms,
            report.doc_loads,
            report.batches,
            report.integrated,
            report.deferred
        );
        if pull.events == 0 {
            assert_eq!(report.integrated, 0, "空页之后不应再有可集成行");
            break;
        }
    }

    // ---- 结果正确性（与时间无关的硬断言）----
    let state = sync_cursor_state(&vault_b).expect("B 游标状态");
    assert_eq!(state.received_seq, head_seq, "B 必须收到全部事件");
    assert_eq!(state.integrated_seq, head_seq, "B 必须集成全部事件");
    assert!(stats.events >= updates, "事件数至少等于 updates");
    assert_eq!(
        stats.events, updates,
        "本场景没有 ASSET，事件数应等于 updates"
    );
    assert_eq!(stats.stored, updates);
    assert_eq!(stats.skipped, 0);
    assert!(stats.update_bytes > 0, "Update 明文字节必须 > 0");
    assert_eq!(stats.asset_bytes, 0, "本场景没有 ASSET");
    assert_eq!(stats.integrated, updates, "所有事件都必须被集成");
    assert_eq!(
        stats.max_deferred, 0,
        "每轮 integrate 后都不得留下 deferred"
    );

    for doc in 0..docs {
        let (body_a, _) = load_body_doc(&vault_a, &doc_id(doc), device_a.identity.peer_id, None)
            .expect("A 正文重放");
        let (body_b, _) = load_body_doc(&vault_b, &doc_id(doc), device_b.identity.peer_id, None)
            .expect("B 正文重放");
        assert_eq!(
            body_a.text(),
            body_b.text(),
            "追赶后 B 第 {doc} 篇正文必须与 A 一致"
        );
    }
    stats
}

fn print_summary(scenario: &str, mode: Mode, docs: usize, updates: usize, stats: &CaseStats) {
    eprintln!(
        "[phase1a] summary scenario={scenario} mode={} docs={} updates={} rounds={} events={} \
         update_bytes={} pull_ms(report)={} pull_ms(wall)={} integrate_ms(report)={} \
         integrate_ms(wall)={} doc_loads={} batches={} deferred={} max_page_events={} total_ms={}",
        mode.name(),
        docs,
        updates,
        stats.rounds,
        stats.events,
        stats.update_bytes,
        stats.pull_elapsed_ms,
        stats.pull_wall_ms,
        stats.integrate_elapsed_ms,
        stats.integrate_wall_ms,
        stats.doc_loads,
        stats.batches,
        stats.max_deferred,
        stats.max_page_events,
        stats.total_wall_ms()
    );
}

/// 以 per_row 为基准打印同一场景下其它模式的加速比（只展示，不断言）。
fn print_speedups(scenario: &str, updates: usize, results: &[(Mode, CaseStats)]) {
    let Some((_, baseline)) = results.iter().find(|(mode, _)| *mode == Mode::PerRow) else {
        return;
    };
    for (mode, stats) in results {
        if *mode == Mode::PerRow {
            continue;
        }
        let pull_speedup = baseline.pull_wall_ms as f64 / stats.pull_wall_ms.max(1) as f64;
        let integrate_speedup =
            baseline.integrate_wall_ms as f64 / stats.integrate_wall_ms.max(1) as f64;
        let total_speedup = baseline.total_wall_ms() as f64 / stats.total_wall_ms().max(1) as f64;
        let doc_load_ratio = stats.doc_loads as f64 / baseline.doc_loads.max(1) as f64;
        eprintln!(
            "[phase1a] speedup scenario={scenario} updates={updates} mode={} \
             baseline=per_row pull_speedup={pull_speedup:.2}x \
             integrate_speedup={integrate_speedup:.2}x total_speedup={total_speedup:.2}x \
             doc_loads_ratio={doc_load_ratio:.4} (baseline_total={}ms mode_total={}ms)",
            mode.name(),
            baseline.total_wall_ms(),
            stats.total_wall_ms()
        );
    }
}

fn run_scenario(
    scenario: &str,
    docs: usize,
    updates_per_doc: usize,
    text_for: fn(usize) -> String,
) {
    let updates = docs * updates_per_doc;
    eprintln!(
        "[phase1a] scenario={scenario} begin docs={docs} updates={updates} \
         updates_per_doc={updates_per_doc}"
    );
    let mut results = Vec::with_capacity(Mode::ALL.len());
    for mode in Mode::ALL {
        let stats = run_case(mode, docs, updates_per_doc, text_for, scenario);
        print_summary(scenario, mode, docs, updates, &stats);
        results.push((mode, stats));
    }
    print_speedups(scenario, updates, &results);
}

#[test]
#[ignore = "manual benchmark"]
fn phase1a_two_device_catchup_matrix() {
    // 1) 单文档 600 / 2000（沿用 Phase 0 的 ~64B 小编辑）。
    for edits in [600usize, 2_000] {
        run_scenario("single", 1, edits, edit_text);
    }

    // 2) 5000 条 / 20 篇多文档（每篇 250 条 × 439B ≈ 107 KiB 正文）。
    assert_eq!(edit_text_long(1).chars().count(), 439);
    assert!(edit_text_long(1).chars().count() * 250 >= 100 * 1024);
    run_scenario("multi", 20, 250, edit_text_long);

    // 3) 可选：5000 条单文档（同一文档的 O(历史 × 事件) 最坏情形）。
    run_scenario("single", 1, 5_000, edit_text);
}
