//! Phase 1B：checkpoint 快速追赶（双模式 + prepare/integrate 拆分）。
//!
//! 约束见 `docs/notes/sync-fast-catchup-plan.md` §9：
//! - **Mandatory**：`gc_floor > received_seq`，普通 pull 必然 `CursorExpired`；不受
//!   机会性快照上限拦截，失败进入明确 `Paused`，禁止回退普通 pull。
//! - **Opportunistic**：`S - received_seq >= N_min` 且快照密文估算不超过 16 MiB；
//!   任一不满足走普通聚合 pull。
//! - 网络/下载/验签在 [`prepare`]（不持写闸）；写库/水位/游标在 [`integrate`]（调用方
//!   持外部提交闸）。本模块不碰 wire/schema。

use menghunote_storage::{sync_cursor_state, Vault};

use crate::bootstrap::{self, BootstrapPins, PreparedBootstrap};
use crate::error::SyncError;
use crate::relay::{RelayClient, RelayState};

/// 机会性追赶的最小事件差（plan §9.1）。
pub const DEFAULT_N_MIN: u64 = 25_000;
/// 机会性追赶的快照密文硬上限（plan §9.1）。
pub const DEFAULT_OPPORTUNISTIC_MAX_SNAPSHOT_BYTES: u64 = 16 * 1024 * 1024;
/// 明文快照 → 密文的保守安全系数（信封/分片/编码开销）。
const SNAPSHOT_CIPHERTEXT_FACTOR: u64 = 2;

/// 追赶模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatchUpMode {
    /// 游标已过期，必须用 checkpoint 才能继续。
    Mandatory,
    /// 落后足够多且快照成本可接受，主动追赶。
    Opportunistic,
}

/// 追赶阈值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CatchUpConfig {
    /// 机会性追赶的最小事件差。
    pub n_min: u64,
    /// 机会性追赶允许的快照密文字节上限。
    pub opportunistic_max_snapshot_bytes: u64,
}

impl Default for CatchUpConfig {
    fn default() -> Self {
        Self {
            n_min: DEFAULT_N_MIN,
            opportunistic_max_snapshot_bytes: DEFAULT_OPPORTUNISTIC_MAX_SNAPSHOT_BYTES,
        }
    }
}

/// 一次追赶决策（只读，不含明文）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CatchUpDecision {
    /// 强制或机会性。
    pub mode: CatchUpMode,
    /// 实际选中 checkpoint 的覆盖水位 S。
    pub checkpoint_s: u64,
    /// `S - received_seq`（饱和）。
    pub gap_events: u64,
    /// 快照明文字节估算（已乘安全系数）。
    pub estimated_snapshot_bytes: u64,
}

/// 是否值得进入网络准备（避免正常近端同步无条件下载整份快照）。
///
/// 强制条件用 `gc_floor > received`；机会性先用 `head_seq - received` 做上界，
/// 真正的 `S - received` 与快照上限在 [`prepare`] 拿到实际 checkpoint 后再判。
#[must_use]
pub fn should_attempt(state: &RelayState, local_received_seq: u64, config: &CatchUpConfig) -> bool {
    state.gc_floor > local_received_seq
        || state.head_seq.saturating_sub(local_received_seq) >= config.n_min
}

/// 决策：只依赖 relay state / 本地 received / 实际选中 checkpoint S / 快照估算。
///
/// 强制模式不受快照上限拦截；机会性必须同时满足事件差和快照上限。
#[must_use]
pub fn decide(
    state: &RelayState,
    local_received_seq: u64,
    checkpoint_s: u64,
    estimated_snapshot_bytes: u64,
    config: &CatchUpConfig,
) -> Option<CatchUpDecision> {
    let gap_events = checkpoint_s.saturating_sub(local_received_seq);
    if state.gc_floor > local_received_seq {
        return Some(CatchUpDecision {
            mode: CatchUpMode::Mandatory,
            checkpoint_s,
            gap_events,
            estimated_snapshot_bytes,
        });
    }
    if gap_events >= config.n_min
        && estimated_snapshot_bytes <= config.opportunistic_max_snapshot_bytes
    {
        return Some(CatchUpDecision {
            mode: CatchUpMode::Opportunistic,
            checkpoint_s,
            gap_events,
            estimated_snapshot_bytes,
        });
    }
    None
}

/// 已下载/校验、尚未写库的追赶候选。
#[derive(Debug)]
pub struct CatchUpPrepared {
    prepared: PreparedBootstrap,
    decision: CatchUpDecision,
}

impl CatchUpPrepared {
    /// 决策。
    #[must_use]
    pub fn decision(&self) -> CatchUpDecision {
        self.decision
    }

    /// 实际选中 checkpoint 的 S。
    #[must_use]
    pub fn checkpoint_s(&self) -> u64 {
        self.prepared.covered_seq()
    }

    /// 实际选中 checkpoint 的 cursor。
    #[must_use]
    pub fn checkpoint_cursor(&self) -> &str {
        self.prepared.checkpoint_cursor()
    }

    /// 是否使用 fallback。
    #[must_use]
    pub fn used_fallback(&self) -> bool {
        self.prepared.used_fallback()
    }

    /// 已选中文档数。
    #[must_use]
    pub fn doc_count(&self) -> usize {
        self.prepared.doc_count()
    }

    /// 已选中资产数。
    #[must_use]
    pub fn asset_count(&self) -> usize {
        self.prepared.asset_count()
    }
}

/// 追赶写库报告（不含明文）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatchUpReport {
    /// 模式。
    pub mode: CatchUpMode,
    /// 实际选中 checkpoint 的 S。
    pub checkpoint_s: u64,
    /// 是否使用 fallback。
    pub used_fallback: bool,
    /// 导入文档 id（用于追赶后物化；不含明文）。
    pub docs: Vec<String>,
    /// 导入资产数。
    pub assets: usize,
    /// 清理的 inbox 行数（仅在 integrated>=S 后）。
    pub pruned_inbox: usize,
    /// 写闸内耗时（毫秒）；只记录，不自动中止。
    pub gate_hold_ms: u64,
}

/// 网络准备：选 checkpoint、下载/验签/校验，**不写本地库**。
///
/// 返回 `None` 表示不需要追赶（普通 pull 更便宜）；调用方应继续普通 pull。
pub fn prepare<C: RelayClient>(
    vault: &Vault,
    client: &mut C,
    vault_id: [u8; 16],
    peer_id: u64,
    root: &crate::keys::RootSecretV1,
    pins: &BootstrapPins,
    config: &CatchUpConfig,
) -> Result<Option<CatchUpPrepared>, SyncError> {
    let local = sync_cursor_state(vault)?;
    let state = client.state()?;
    if state.vault_id != vault_id {
        return Err(SyncError::RelayWire("state.vault_id 与本地不一致"));
    }
    if let Some(log_id) = local.log_id {
        if log_id != state.log_id {
            return Err(SyncError::Paused(crate::relay::PauseReason::LogIdChanged));
        }
    }
    if state.epoch != root.key_epoch {
        return Err(SyncError::Paused(crate::relay::PauseReason::EpochMismatch));
    }

    // 1) 只拉 manifest 预检：先在真正下载快照之前用实际选中 checkpoint 的 S/明文字节
    //    判断机会性是否值得；不满足直接回普通 pull，不浪费带宽。
    let probe = match bootstrap::probe(client, vault_id, root, pins) {
        Ok(probe) => probe,
        Err(error) => {
            if state.gc_floor > local.received_seq {
                eprintln!("[sync] mandatory catchup probe failed: {error}");
                return Err(SyncError::Paused(crate::relay::PauseReason::CatchupFailed));
            }
            eprintln!("[sync] opportunistic catchup probe failed, fallback to pull: {error}");
            return Ok(None);
        }
    };
    let probe_estimated = probe
        .estimated_plain_bytes()
        .saturating_mul(SNAPSHOT_CIPHERTEXT_FACTOR);
    if decide(
        &state,
        local.received_seq,
        probe.covered_seq(),
        probe_estimated,
        config,
    )
    .is_none()
    {
        return Ok(None);
    }

    // 2) 预检通过后才下载 snapshot/缺失资产。fallback 可能和预检不同，实际 S/字节
    //    必须在下载完成后再判一次；mandatory 仍不受机会性上限拦截。
    let prepared = match bootstrap::prepare(vault, client, vault_id, peer_id, root, pins) {
        Ok(prepared) => prepared,
        Err(error) => {
            if state.gc_floor > local.received_seq {
                eprintln!("[sync] mandatory catchup prepare failed: {error}");
                return Err(SyncError::Paused(crate::relay::PauseReason::CatchupFailed));
            }
            eprintln!("[sync] opportunistic catchup prepare failed, fallback to pull: {error}");
            return Ok(None);
        }
    };
    let actual_estimated = prepared
        .estimated_snapshot_bytes()
        .saturating_mul(SNAPSHOT_CIPHERTEXT_FACTOR);
    let Some(actual_decision) = decide(
        &state,
        local.received_seq,
        prepared.covered_seq(),
        actual_estimated,
        config,
    ) else {
        // 预检主基线可接受，实际 fallback 超限：丢弃已下载快照，走普通 pull。
        return Ok(None);
    };
    Ok(Some(CatchUpPrepared {
        prepared,
        decision: actual_decision,
    }))
}

/// 写库：baseline_apply → finish/游标 CAS → 清理已覆盖 inbox。
///
/// 调用方必须持有外部提交写闸；网络已在 [`prepare`] 完成。
pub fn integrate(
    vault: &Vault,
    peer_id: u64,
    prepared: &CatchUpPrepared,
) -> Result<CatchUpReport, SyncError> {
    let started = std::time::Instant::now();
    let report = bootstrap::integrate(
        vault,
        peer_id,
        &prepared.prepared,
        bootstrap::BootstrapAdvancement::CatchUp,
        None,
    )?;
    let cursor = sync_cursor_state(vault)?;
    if cursor.integrated_seq < report.covered_seq {
        return Err(SyncError::LocalCorrupt(
            "catchup integrate 返回成功但 integrated_seq 未覆盖 checkpoint S",
        ));
    }
    let pruned = menghunote_storage::prune_covered_inbox(
        vault,
        &prepared.prepared.log_id(),
        report.covered_seq,
    )?;
    let gate_hold_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    if gate_hold_ms > 3_000 {
        eprintln!("[sync] catchup gate_hold_ms={gate_hold_ms} 超过 3s 预算（只记录，未自动中止）");
    }
    Ok(CatchUpReport {
        mode: prepared.decision.mode,
        checkpoint_s: report.covered_seq,
        used_fallback: report.used_fallback,
        docs: report.docs.into_iter().map(|(doc_id, _)| doc_id).collect(),
        assets: report.assets,
        pruned_inbox: pruned,
        gate_hold_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(head_seq: u64, gc_floor: u64) -> RelayState {
        RelayState {
            vault_id: [1; 16],
            log_id: [2; 16],
            instance_id: [3; 16],
            epoch: 1,
            control_rev: 0,
            auth_rev: 0,
            head_seq,
            gc_floor,
            current_rotation_id: None,
        }
    }

    #[test]
    fn mandatory_wins_even_when_snapshot_exceeds_opportunistic_cap() {
        let decision = decide(
            &state(100_000, 11_000),
            10_000,
            90_000,
            u64::MAX,
            &CatchUpConfig::default(),
        )
        .expect("过期游标必须强制追赶");
        assert_eq!(decision.mode, CatchUpMode::Mandatory);
        assert_eq!(decision.gap_events, 80_000);
    }

    #[test]
    fn opportunistic_requires_n_min_and_snapshot_cap() {
        let config = CatchUpConfig::default();
        let below_min = decide(&state(100_000, 0), 99_000, 99_000, 1, &config);
        assert!(below_min.is_none(), "事件差不足不得机会性追赶");

        let ok = decide(
            &state(100_000, 0),
            70_000,
            95_000,
            16 * 1024 * 1024,
            &config,
        )
        .expect("25k 且快照正好 16MiB 应允许");
        assert_eq!(ok.mode, CatchUpMode::Opportunistic);

        let too_big = decide(
            &state(100_000, 0),
            70_000,
            95_000,
            16 * 1024 * 1024 + 1,
            &config,
        );
        assert!(too_big.is_none(), "超过机会性快照上限必须回退普通 pull");
    }

    #[test]
    fn should_attempt_only_for_expired_or_large_gap() {
        let config = CatchUpConfig::default();
        assert!(should_attempt(&state(100_000, 0), 75_000, &config));
        assert!(!should_attempt(&state(100_000, 0), 90_000, &config));
        assert!(should_attempt(&state(100_000, 20_000), 10_000, &config));
    }
}
