//! GC：把"新 pair 产生的持久计划"落成实际日志/密文前缀回收（ADR 0006 §7 + §13.3）。
//!
//! 规则：
//! - 只有 `gc_plans.eligible_at <= now` 的 `cut_seq` 才可能生效（7 天持久宽限）；
//! - `floor = min(max 到期 cut_seq, 两有效基线中较旧的 S, 所有有效 bootstrap lease.floor_seq)`；
//! - **删除前**校验两份基线的全部 refs 存在、每片密文 SHA256 与记录的 `object_digest` 一致；
//!   任一损坏 → 标 `unhealthy` 并**不推进 gc_floor**（只告警/暂停）；
//! - 只删 `<= floor` 的日志前缀和**无根引用的 update 对象**的密文（保留幂等 tombstone 行）；
//! - v1 不做资产孤儿删除；只清 24h 未 finalize 的暂存对象。

use rusqlite::Connection;

use crate::checkpoints::{self, Health};
use crate::error::{Error, Result};
use crate::ids::Id16;
use crate::log;
use crate::objects;
use crate::quota;
use crate::time::{secs, Clock};
use crate::vault;

/// 一次 GC tick 的结果（诊断/测试断言用，不含任何密文）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GcOutcome {
    /// 本次生效的到期 `cut_seq`（0 = 没有到期计划）。
    pub cut_seq: u64,
    /// 实际计算的 floor（未执行时表示"若可执行会删到哪"）。
    pub floor: u64,
    pub deleted_log_rows: u64,
    pub retired_update_objects: u64,
    pub deleted_staging_objects: u64,
    /// 本次被标记 unhealthy 的 checkpoint。
    pub unhealthy: Vec<Id16>,
    /// 因为基线损坏/不足而未执行。
    pub blocked: bool,
}

/// 执行一次 GC（写事务内；调用方：`Db::write` 或后台 tick）。
pub fn tick(conn: &Connection, clock: &dyn Clock) -> Result<GcOutcome> {
    let now = clock.now_secs();
    let mut outcome = GcOutcome::default();
    quota::expire_stale_reservations(conn, now)?;
    let vault = vault::load(conn)?;

    // 1) 到期计划里最大的 cut_seq。
    let eligible_cut: Option<i64> = conn
        .query_row(
            "SELECT MAX(cut_seq) FROM gc_plans WHERE eligible_at <= ?1",
            [now],
            |row| row.get(0),
        )
        .unwrap_or(None);
    let Some(eligible_cut) = eligible_cut else {
        return Ok(outcome);
    };
    let eligible_cut = u64::try_from(eligible_cut).map_err(|_| Error::Invariant("gc cut_seq"))?;
    outcome.cut_seq = eligible_cut;

    // 2) 两份最新 published 基线（含 unhealthy：损坏的基线本身必须阻止前进，而不是被跳过）。
    let baselines = published_any_newest_first(conn, 2)?;
    if baselines.len() < 2 {
        outcome.blocked = true;
        return Ok(outcome);
    }
    for checkpoint in &baselines {
        if checkpoint.health == Health::Unhealthy {
            outcome.blocked = true;
            outcome.unhealthy.push(checkpoint.checkpoint_id);
            continue;
        }
        let issues = checkpoints::integrity_issues(conn, checkpoint)?;
        if !issues.is_empty() {
            checkpoints::mark_health(conn, &checkpoint.checkpoint_id, Health::Unhealthy)?;
            outcome.blocked = true;
            outcome.unhealthy.push(checkpoint.checkpoint_id);
        }
    }
    if outcome.blocked {
        // 已发布的基线损坏：绝不推进 gc_floor（§13.3）。
        return Ok(outcome);
    }

    // 3) floor = min(到期 cut, 较旧基线 S, 有效 lease floor)。
    let older_seq = baselines
        .iter()
        .map(|c| c.covered_seq)
        .min()
        .ok_or(Error::Invariant("baseline pair"))?;
    let mut floor = eligible_cut.min(older_seq);
    if let Some(lease_floor) = checkpoints::active_lease_floor(conn, now)? {
        floor = floor.min(lease_floor);
    }
    outcome.floor = floor;
    if floor <= vault.gc_floor {
        return Ok(outcome);
    }

    // 4) 删日志前缀。
    outcome.deleted_log_rows = log::delete_prefix(conn, floor)?;

    // 5) 无根引用的 update 对象：清密文、保留行（幂等 tombstone；ADR §6 末段）。
    let candidates: Vec<Vec<u8>> = {
        let mut stmt = conn.prepare(
            "SELECT o.object_id FROM objects o
             WHERE o.type = 1
               AND o.budget_class != 'retired'
               AND NOT EXISTS (SELECT 1 FROM checkpoint_refs r WHERE r.object_id = o.object_id)
               AND NOT EXISTS (SELECT 1 FROM log l WHERE l.object_id = o.object_id)",
        )?;
        let rows = stmt.query_map([], |row| row.get::<_, Vec<u8>>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for object_id in candidates {
        conn.execute(
            "DELETE FROM object_parts WHERE object_id = ?1",
            [object_id.as_slice()],
        )?;
        conn.execute(
            "UPDATE objects SET budget_class = 'retired' WHERE object_id = ?1",
            [object_id.as_slice()],
        )?;
        outcome.retired_update_objects += 1;
    }

    // 6) 从未 finalize 的暂存对象：24h 可删（ADR §7）。
    outcome.deleted_staging_objects =
        objects::cleanup_staging(conn, now - secs::STAGING_RETENTION)?;

    // 7) 账本对账 + 推进 gc_floor。
    quota::recompute(conn)?;
    conn.execute(
        "UPDATE vault SET gc_floor = ?1 WHERE singleton = 1",
        [i64::try_from(floor).map_err(|_| Error::Invariant("gc floor"))?],
    )?;
    Ok(outcome)
}

/// 含 unhealthy 的已发布基线（GC 专用；健康过滤由调用方做，否则损坏基线会被"跳过"而非阻止）。
pub fn published_any_newest_first(
    conn: &Connection,
    limit: u32,
) -> Result<Vec<checkpoints::CheckpointRow>> {
    let mut stmt = conn.prepare(
        "SELECT checkpoint_id FROM checkpoints
         WHERE state = 'published'
         ORDER BY published_at DESC, rowid DESC LIMIT ?1",
    )?;
    let ids: Vec<Vec<u8>> = stmt
        .query_map([i64::from(limit.max(1))], |row| row.get::<_, Vec<u8>>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        let id: Id16 = id
            .as_slice()
            .try_into()
            .map_err(|_| Error::Invariant("checkpoint_id"))?;
        out.push(checkpoints::get(conn, &id)?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_outcome_is_empty() {
        let outcome = GcOutcome::default();
        assert_eq!(outcome.floor, 0);
        assert!(!outcome.blocked);
        assert!(outcome.unhealthy.is_empty());
    }
}
