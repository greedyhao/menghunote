//! Phase 0 手动基准：逐条 `store_inbox_verified` vs 批量 `store_inbox_verified_batch`。
//!
//! 只打印数字，**不 assert 时间**（机器/磁盘/并发负载都会影响结果；断言时间只会带来
//! 偶发失败）。运行方式：
//!
//! ```bash
//! cargo test -p menghunote-storage --test sync_inbox_bench -- --ignored --nocapture
//! ```
//!
//! 临时 Vault 用**显式路径**放在 `CARGO_TARGET_TMPDIR` 下（本机测试进程不能写 `%TEMP%`；
//! 也不用 `tempfile::tempdir`，便于失败后保留现场）。每轮都新建库并删除旧文件，避免
//! 上一轮的行/前缀影响本轮计时。

use std::path::{Path, PathBuf};
use std::time::Instant;

use menghunote_storage::{
    count_inbox_ready, set_sync_log_id, store_inbox_verified, store_inbox_verified_batch,
    sync_cursor_state, Vault, VerifiedInboxRow,
};

const LOG_ID: [u8; 16] = [0x3a; 16];
/// 单行 payload 大小（贴近真实 UpdateV1：小编辑几 KB、大编辑几十 KB，这里取 4 KiB）。
const PAYLOAD_BYTES: usize = 4096;
/// 每个 N 的测量轮数（取中位数报告，单轮数字也原样打印）。
const ROUNDS: usize = 3;

fn bench_vault(tag: &str) -> (PathBuf, Vault) {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("phase0-inbox-bench");
    std::fs::create_dir_all(&dir).expect("创建基准目录");
    let path = dir.join(format!("{tag}.sqlite3"));
    for suffix in ["", "-wal", "-shm"] {
        let candidate = PathBuf::from(format!("{}{suffix}", path.display()));
        let _ = std::fs::remove_file(candidate);
    }
    let vault = Vault::open(&path).expect("打开基准 vault");
    set_sync_log_id(&vault, &LOG_ID).expect("绑定 log_id");
    (path, vault)
}

fn make_rows(n: u64) -> Vec<VerifiedInboxRow> {
    (1..=n)
        .map(|seq| VerifiedInboxRow {
            log_id: LOG_ID,
            relay_seq: seq,
            object_id: [u8::try_from(seq % 251).expect("seed 在 u8 内"); 32],
            epoch: 1,
            verified_payload: vec![
                u8::try_from(seq % 251).expect("seed 在 u8 内");
                PAYLOAD_BYTES
            ],
        })
        .collect()
}

fn received(vault: &Vault) -> u64 {
    sync_cursor_state(vault).expect("游标状态").received_seq
}

/// 跑一轮逐条写入；返回耗时。
fn run_per_row(tag: &str, rows: &[VerifiedInboxRow]) -> std::time::Duration {
    let (path, vault) = bench_vault(tag);
    let started = Instant::now();
    for row in rows {
        store_inbox_verified(
            &vault,
            &row.log_id,
            row.relay_seq,
            &row.object_id,
            row.epoch,
            &row.verified_payload,
        )
        .expect("逐条写入");
    }
    let elapsed = started.elapsed();
    assert_eq!(received(&vault), rows.len() as u64);
    assert_eq!(count_inbox_ready(&vault).unwrap(), rows.len() as u64);
    drop(vault);
    cleanup(&path);
    elapsed
}

/// 跑一轮批量写入；返回耗时。
fn run_batch(tag: &str, rows: &[VerifiedInboxRow]) -> std::time::Duration {
    let (path, vault) = bench_vault(tag);
    let started = Instant::now();
    let inserted = store_inbox_verified_batch(&vault, rows).expect("批量写入");
    let elapsed = started.elapsed();
    assert_eq!(inserted, rows.len());
    assert_eq!(received(&vault), rows.len() as u64);
    assert_eq!(count_inbox_ready(&vault).unwrap(), rows.len() as u64);
    drop(vault);
    cleanup(&path);
    elapsed
}

fn cleanup(path: &Path) {
    for suffix in ["", "-wal", "-shm"] {
        let candidate = PathBuf::from(format!("{}{suffix}", path.display()));
        let _ = std::fs::remove_file(candidate);
    }
}

fn median(values: &mut [u128]) -> u128 {
    values.sort_unstable();
    values[values.len() / 2]
}

#[test]
#[ignore = "manual benchmark"]
fn bench_inbox_store_per_row_vs_batch() {
    let base = env!("CARGO_TARGET_TMPDIR");
    eprintln!(
        "[bench] target_tmp={base} payload_bytes={PAYLOAD_BYTES} rounds={ROUNDS} (每轮新建 vault)"
    );
    for n in [256u64, 1024] {
        let rows = make_rows(n);
        // 预热一轮（排除首轮 page cache / SQLite 首次 prepare 的影响），不计入统计。
        let _ = run_per_row(&format!("warmup-per-row-{n}"), &rows);
        let _ = run_batch(&format!("warmup-batch-{n}"), &rows);

        let mut per_row = Vec::with_capacity(ROUNDS);
        let mut batch = Vec::with_capacity(ROUNDS);
        for round in 0..ROUNDS {
            let row_elapsed = run_per_row(&format!("per-row-{n}-{round}"), &rows);
            let batch_elapsed = run_batch(&format!("batch-{n}-{round}"), &rows);
            eprintln!(
                "[bench] n={n} round={round} per_row={:?} batch={:?}",
                row_elapsed, batch_elapsed
            );
            per_row.push(row_elapsed.as_nanos());
            batch.push(batch_elapsed.as_nanos());
        }
        let per_row_median = median(&mut per_row);
        let batch_median = median(&mut batch);
        // 加速比用 f64 打印（仅展示，不参与断言）。
        let speedup = per_row_median as f64 / batch_median.max(1) as f64;
        eprintln!(
            "[bench] n={n} per_row_median={}us batch_median={}us speedup={speedup:.2}x rows={n} payload={PAYLOAD_BYTES}B",
            per_row_median / 1000,
            batch_median / 1000
        );
    }
}
