//! Phase 0：两设备「落后追赶」的计数/耗时基准（手动运行，不 assert 时间）。
//!
//! 场景：设备 A 先产生 N 条正文 update 并推到 relay；设备 B 从 c0（很早的游标）开始
//! 逐页 `pull → integrate`，直到 relay 日志被追平。对同一份日志跑两遍：
//!
//! - `inbox_batch=false`：生产默认（每条事件一次 `store_inbox_verified` 事务）；
//! - `inbox_batch=true`：Phase 0 实验开关（整页 verified 行一个事务）。
//!
//! 只比较**结果一致**（事件数、字节、游标、水位、最终正文）与耗时，不对时间做断言。
//!
//! ```bash
//! cargo test -p menghunote-sync --test phase0_catchup -- --ignored --nocapture
//! ```
//!
//! 不 sleep、不读真实网络：`FakeRelay` 是纯内存 relay，耗时反映本地 SQLite/Loro 成本。

mod support;

use std::time::Instant;

use support::*;

use menghunote_storage::{load_body_doc, sync_cursor_state, Vault};
use menghunote_sync::engine::{EngineConfig, EngineIdentity, PullReport, SyncEngine};

/// 每个 update 的正文片段（约 64B，贴近"小编辑"；不是 4KB 大块）。
fn edit_text(index: usize) -> String {
    format!("-{index:05}-{}", "x".repeat(56))
}

/// 设备 A 把所有未发送意图加密并推完（本地 seq 顺序 = relay 日志顺序）。
fn publish_all(relay: &mut FakeRelay, vault: &Vault, device: &Device) -> usize {
    let mut published = 0usize;
    loop {
        let identity = EngineIdentity {
            device_id: device.identity.device_id,
            signing_key: device.identity.signing_key.clone(),
            peer_id: device.identity.peer_id,
        };
        let mut sync = SyncEngine::new(vault, relay, identity, device.root.clone());
        let encrypted = sync.encrypt_pending(256).expect("加密");
        let push = sync.push().expect("推送");
        assert_eq!(push.acked, encrypted, "本轮推送必须全部 durable ACK");
        published += encrypted;
        if encrypted == 0 {
            return published;
        }
    }
}

/// 单个配置下的一次追赶统计。
#[derive(Debug, Default)]
struct CaseStats {
    rounds: usize,
    events: usize,
    stored: usize,
    skipped: usize,
    update_bytes: u64,
    asset_bytes: u64,
    pull_elapsed_ms: u64,
    integrate_elapsed_ms: u64,
    pull_wall_ms: u128,
    integrate_wall_ms: u128,
    max_page_events: usize,
}

/// 跑一次完整追赶，返回统计（内部断言结果正确，只不测时间）。
fn run_case(edits: usize, inbox_batch: bool) -> CaseStats {
    ensure_test_identity_wrap();
    let (_a_dir, vault_a) = fresh_vault();
    let (_b_dir, vault_b) = fresh_vault();
    let device_a = Device::new(0x61);
    let device_b = Device::new(0x62);
    let mut relay = FakeRelay::new(&[&device_a, &device_b]);

    // ---- A：edits 条正文 update ----
    let doc_a = seed_doc(&vault_a, "doc-1", device_a.identity.peer_id);
    let mut pos = 0usize;
    for index in 0..edits {
        let text = edit_text(index);
        doc_a.insert_scalars(pos, &text).expect("A 写正文");
        pos += text.chars().count();
        let mut object_id = [0u8; 32];
        object_id[..8].copy_from_slice(&(index as u64 + 1).to_be_bytes());
        object_id[31] = 0x61;
        commit_with_intent(
            &vault_a,
            &doc_a,
            &uuid_like(index as u64 + 1),
            object_id,
            EPOCH,
        );
    }
    let published = publish_all(&mut relay, &vault_a, &device_a);
    assert_eq!(published, edits, "A 必须推完所有 update");
    let head_seq = relay.head_seq;
    assert_eq!(head_seq, edits as u64);

    // ---- B：从 c0 逐页追赶 ----
    seed_doc(&vault_b, "doc-1", device_b.identity.peer_id);
    seed_cursor(&vault_b);
    let mut stats = CaseStats::default();
    loop {
        // 本轮开始前的历史行数（探针重放的就是这段历史）。
        let history_before = sync_cursor_state(&vault_b)
            .map(|state| state.integrated_seq)
            .unwrap_or(0);
        // 探针（不计入下面两段计时）：一次完整重放该文档历史的成本。
        // integrate 的每个 remote import 都会走 `load_body_doc` → 全量重放，
        // 这个数字应当随轮次增长，用来解释 integrate 的超线性。
        let replay_started = Instant::now();
        let _ =
            load_body_doc(&vault_b, "doc-1", device_b.identity.peer_id, None).expect("重放探针");
        let replay_ms = replay_started.elapsed().as_millis();

        let pull_started = Instant::now();
        let pull: PullReport = {
            let identity = EngineIdentity {
                device_id: device_b.identity.device_id,
                signing_key: device_b.identity.signing_key.clone(),
                peer_id: device_b.identity.peer_id,
            };
            let config = EngineConfig {
                inbox_batch,
                ..EngineConfig::default()
            };
            let mut sync = SyncEngine::with_config(
                &vault_b,
                &mut relay,
                identity,
                device_b.root.clone(),
                config,
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
            let identity = EngineIdentity {
                device_id: device_b.identity.device_id,
                signing_key: device_b.identity.signing_key.clone(),
                peer_id: device_b.identity.peer_id,
            };
            let config = EngineConfig {
                inbox_batch,
                ..EngineConfig::default()
            };
            let mut sync = SyncEngine::with_config(
                &vault_b,
                &mut relay,
                identity,
                device_b.root.clone(),
                config,
            );
            sync.integrate(1_000_000).expect("B integrate")
        };
        let integrate_wall = integrate_started.elapsed();
        stats.integrate_wall_ms += integrate_wall.as_millis();
        stats.integrate_elapsed_ms += integrate.elapsed_ms;
        eprintln!(
            "[phase0] batch={inbox_batch} round={} history_rows={} replay_probe_ms={} events={} stored={} skipped={} update_bytes={} pull_ms={} integrate_ms={} integrated={} deferred={}",
            stats.rounds,
            history_before,
            replay_ms,
            pull.events,
            pull.stored,
            pull.skipped,
            pull.update_bytes,
            pull.elapsed_ms,
            integrate.elapsed_ms,
            integrate.integrated,
            integrate.deferred
        );
        if pull.events == 0 {
            assert_eq!(integrate.integrated, 0, "空页之后不应再有可集成行");
            break;
        }
    }

    // ---- 结果正确性（与时间无关的硬断言）----
    let state = sync_cursor_state(&vault_b).expect("B 游标状态");
    assert_eq!(state.received_seq, head_seq, "B 必须收到全部事件");
    assert_eq!(state.integrated_seq, head_seq, "B 必须集成全部事件");
    assert!(stats.events >= edits, "事件数至少等于 edits");
    assert_eq!(stats.events, edits, "本场景没有 ASSET，事件数应等于 edits");
    assert_eq!(stats.stored, edits);
    assert_eq!(stats.skipped, 0);
    assert!(stats.update_bytes > 0, "Update 明文字节必须 > 0");
    assert_eq!(stats.asset_bytes, 0, "本场景没有 ASSET");

    let (body_a, _) =
        load_body_doc(&vault_a, "doc-1", device_a.identity.peer_id, None).expect("A 正文重放");
    let (body_b, _) =
        load_body_doc(&vault_b, "doc-1", device_b.identity.peer_id, None).expect("B 正文重放");
    assert_eq!(body_a.text(), body_b.text(), "追赶后 B 正文必须与 A 一致");
    stats
}

#[test]
#[ignore = "manual benchmark"]
fn phase0_two_device_catchup() {
    for edits in [600usize, 2_000] {
        let per_row = run_case(edits, false);
        let batch = run_case(edits, true);
        eprintln!(
            "[phase0] edits={edits} per_row: rounds={} events={} update_bytes={} pull_ms(report)={} pull_ms(wall)={} integrate_ms(report)={} integrate_ms(wall)={} max_page_events={}",
            per_row.rounds,
            per_row.events,
            per_row.update_bytes,
            per_row.pull_elapsed_ms,
            per_row.pull_wall_ms,
            per_row.integrate_elapsed_ms,
            per_row.integrate_wall_ms,
            per_row.max_page_events
        );
        eprintln!(
            "[phase0] edits={edits} batch:   rounds={} events={} update_bytes={} pull_ms(report)={} pull_ms(wall)={} integrate_ms(report)={} integrate_ms(wall)={} max_page_events={}",
            batch.rounds,
            batch.events,
            batch.update_bytes,
            batch.pull_elapsed_ms,
            batch.pull_wall_ms,
            batch.integrate_elapsed_ms,
            batch.integrate_wall_ms,
            batch.max_page_events
        );
        // 只展示比例，不做断言（不同机器/负载会变）。
        let pull_speedup = per_row.pull_wall_ms as f64 / batch.pull_wall_ms.max(1) as f64;
        let total_per_row = per_row.pull_wall_ms + per_row.integrate_wall_ms;
        let total_batch = batch.pull_wall_ms + batch.integrate_wall_ms;
        let total_speedup = total_per_row as f64 / total_batch.max(1) as f64;
        eprintln!(
            "[phase0] edits={edits} pull_speedup={pull_speedup:.2}x total_speedup={total_speedup:.2}x (per_row_total={total_per_row}ms batch_total={total_batch}ms)"
        );
    }
}
