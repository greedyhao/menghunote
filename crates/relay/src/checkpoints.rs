//! checkpoint 生命周期：writer lease → 固定 S → refs → publish → 两基线 → GC 计划
//! （ADR 0006 §5/§7/§13.3）。
//!
//! 关键语义：
//! - 每次 checkpoint 只有一个 writer lease（未 publish 时再申请 → 409 BUSY）；
//! - `S = vault.head_seq` 在创建时**固定**；`publish` 时不得改 S；
//! - refs 只接受**已 complete**且属于该 checkpoint epoch 的对象；manifest 本体单独 pin；
//! - publish 前服务端按 §13.3 校验全部 refs 存在、每片密文 SHA256 与记录的 `object_digest` 一致；
//! - 新 pair 产生**持久** GC 计划 `{cut_seq = S_old, eligible_at = C_new.published_at + 7d}`，
//!   后来的 checkpoint **不能重置已有计划 deadline**（`INSERT OR IGNORE`，键为 `(epoch, cut_seq)`）。

use rusqlite::{Connection, OptionalExtension};

use crate::auth::AuthContext;
use crate::error::{ApiError, Error, ErrorCode, Result};
use crate::ids::{be64, sha256, sha256_parts, Hash32, Id16};
use crate::objects;
use crate::quota::{self, ReservationPurpose};
use crate::refs::RefRole;
use crate::sig::{require, Expectation, SignDomain, SignatureCheck, SignatureHook};
use crate::time::{secs, Clock};
use crate::vault;

/// checkpoint 状态（ADR §6）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckpointState {
    Staging,
    Published,
    Pruned,
}

impl CheckpointState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Staging => "staging",
            Self::Published => "published",
            Self::Pruned => "pruned",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "staging" => Some(Self::Staging),
            "published" => Some(Self::Published),
            "pruned" => Some(Self::Pruned),
            _ => None,
        }
    }
}

/// 健康度（§13.3：损坏的基线必须标 `unhealthy` 并阻止 gc_floor 越过可恢复 floor）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    Unknown,
    Healthy,
    Unhealthy,
}

impl Health {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Healthy => "healthy",
            Self::Unhealthy => "unhealthy",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "unknown" => Some(Self::Unknown),
            "healthy" => Some(Self::Healthy),
            "unhealthy" => Some(Self::Unhealthy),
            _ => None,
        }
    }
}

/// `checkpoints` 行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointRow {
    pub checkpoint_id: Id16,
    pub epoch: u64,
    pub covered_seq: u64,
    pub manifest_oid: Option<Hash32>,
    pub refs_digest: Option<Hash32>,
    pub signed_commit: Option<Vec<u8>>,
    pub state: CheckpointState,
    pub validation_version: u64,
    pub health: Health,
    pub created_at: i64,
    pub published_at: Option<i64>,
}

/// 创建 checkpoint 的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateOutcome {
    pub checkpoint_id: Id16,
    pub epoch: u64,
    pub covered_seq: u64,
    pub reservation_id: Id16,
    pub reservation_expires_at: i64,
}

/// publish 请求（`covered_seq`/`manifest_oid`/`refs_digest` 由 hook 与被签名 commit 交叉校验）。
#[derive(Debug)]
pub struct PublishRequest<'a> {
    pub covered_seq: u64,
    pub manifest_oid: Hash32,
    pub refs_digest: Hash32,
    pub signed_commit: &'a [u8],
    pub signature: &'a [u8],
}

/// publish 结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishOutcome {
    pub checkpoint_id: Id16,
    pub epoch: u64,
    pub covered_seq: u64,
    pub published_at: i64,
}

fn i64_of(value: u64, field: &'static str) -> Result<i64> {
    i64::try_from(value).map_err(|_| Error::Invariant(field))
}

fn u64_of(value: i64, field: &'static str) -> Result<u64> {
    u64::try_from(value).map_err(|_| Error::Invariant(field))
}

/// 读 checkpoint 行；不存在 404。
pub fn get(conn: &Connection, checkpoint_id: &Id16) -> Result<CheckpointRow> {
    let mut stmt = conn.prepare(
        "SELECT checkpoint_id, epoch, covered_seq, manifest_oid, refs_digest, signed_commit,
                state, validation_version, health, created_at, published_at
         FROM checkpoints WHERE checkpoint_id = ?1",
    )?;
    let row = stmt
        .query_row([checkpoint_id.as_slice()], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Option<Vec<u8>>>(3)?,
                row.get::<_, Option<Vec<u8>>>(4)?,
                row.get::<_, Option<Vec<u8>>>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, i64>(7)?,
                row.get::<_, String>(8)?,
                row.get::<_, i64>(9)?,
                row.get::<_, Option<i64>>(10)?,
            ))
        })
        .optional()?
        .ok_or_else(|| Error::api(ErrorCode::NotFound))?;
    Ok(CheckpointRow {
        checkpoint_id: row
            .0
            .as_slice()
            .try_into()
            .map_err(|_| Error::Invariant("checkpoint_id"))?,
        epoch: u64_of(row.1, "checkpoints.epoch")?,
        covered_seq: u64_of(row.2, "checkpoints.covered_seq")?,
        manifest_oid: match row.3 {
            None => None,
            Some(bytes) => Some(
                bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| Error::Invariant("manifest_oid"))?,
            ),
        },
        refs_digest: match row.4 {
            None => None,
            Some(bytes) => Some(
                bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| Error::Invariant("refs_digest"))?,
            ),
        },
        signed_commit: row.5,
        state: CheckpointState::parse(&row.6).ok_or(Error::Invariant("checkpoints.state"))?,
        validation_version: u64_of(row.7, "validation_version")?,
        health: Health::parse(&row.8).ok_or(Error::Invariant("checkpoints.health"))?,
        created_at: row.9,
        published_at: row.10,
    })
}

/// refs 列表（按 object_id 升序）。
pub fn refs(conn: &Connection, checkpoint_id: &Id16) -> Result<Vec<(Hash32, RefRole)>> {
    let mut stmt = conn.prepare(
        "SELECT object_id, role FROM checkpoint_refs WHERE checkpoint_id = ?1
         ORDER BY object_id ASC",
    )?;
    let rows = stmt.query_map([checkpoint_id.as_slice()], |row| {
        Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (object_id, role) = row?;
        out.push((
            object_id
                .as_slice()
                .try_into()
                .map_err(|_| Error::Invariant("checkpoint_refs.object_id"))?,
            RefRole::parse(&role).ok_or(Error::Invariant("checkpoint_refs.role"))?,
        ));
    }
    Ok(out)
}

/// 候选 epoch 的轮换维护 reservation：同一 rotation 复用一份（槽与两份基线共享预算）。
fn rotation_reservation(
    conn: &Connection,
    now: i64,
    device_id: &Id16,
    to_epoch: u64,
    estimated_bytes: i64,
) -> Result<Id16> {
    let rotation_id = crate::rotations::rotation_id_for_epoch(conn, to_epoch)?
        .ok_or_else(|| Error::api(ErrorCode::InvalidState))?;
    let existing: Option<Vec<u8>> = conn
        .query_row(
            "SELECT reservation_id FROM maintenance_reservations
             WHERE purpose = 'rotation' AND resource_id = ?1 AND state = 'active'",
            [rotation_id.as_slice()],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(bytes) = existing {
        return bytes
            .as_slice()
            .try_into()
            .map_err(|_| Error::Invariant("reservation_id"));
    }
    quota::create_reservation(
        conn,
        now,
        ReservationPurpose::Rotation,
        rotation_id.as_slice(),
        device_id,
        estimated_bytes.max(1),
        now + secs::CHECKPOINT_LEASE_TTL,
    )
}

/// 创建 checkpoint（staging）+ 维护 reservation + 固定 S。
pub fn create(
    conn: &Connection,
    clock: &dyn Clock,
    actor: &AuthContext,
    estimated_bytes: i64,
    epoch: Option<u64>,
) -> Result<CreateOutcome> {
    let device_id = actor.device_id()?;
    let vault = vault::load(conn)?;
    let now = clock.now_secs();
    let target_epoch = epoch.unwrap_or(vault.active_epoch);
    let epoch_state = vault::epoch_state(conn, target_epoch)?.ok_or_else(|| {
        Error::Api(ApiError::new(ErrorCode::EpochMismatch).epoch(vault.active_epoch))
    })?;
    let covered_seq = if target_epoch == vault.active_epoch {
        if epoch_state != vault::EpochState::Active {
            return Err(Error::Api(
                ApiError::new(ErrorCode::EpochMismatch).epoch(vault.active_epoch),
            ));
        }
        // 单一 checkpoint writer lease：未 publish 的上一个 lease 还在 → 409 BUSY。
        if quota::active_checkpoint_lease(conn, now)?.is_some() {
            return Err(Error::Api(
                ApiError::new(ErrorCode::Busy).epoch(vault.active_epoch),
            ));
        }
        vault.head_seq
    } else {
        // 候选 epoch：只能来自正在进行的轮换，且 S 必须覆盖 freeze_seq。
        if vault.state != vault::VaultState::Rotating || epoch_state != vault::EpochState::Staged {
            return Err(Error::Api(
                ApiError::new(ErrorCode::EpochMismatch).epoch(vault.active_epoch),
            ));
        }
        let freeze_seq = crate::rotations::freeze_seq_for_epoch(conn, target_epoch)?
            .ok_or_else(|| Error::api(ErrorCode::InvalidState))?;
        if vault.head_seq < freeze_seq {
            return Err(Error::Invariant("head_seq below rotation freeze"));
        }
        vault.head_seq
    };
    if estimated_bytes < 0 {
        return Err(Error::api(ErrorCode::InvalidRequest));
    }

    let checkpoint_id = crate::ids::random_id16()?;
    conn.execute(
        "INSERT INTO checkpoints(checkpoint_id, epoch, covered_seq, manifest_oid, refs_digest,
                                signed_commit, state, validation_version, health, created_at,
                                published_at)
         VALUES (?1, ?2, ?3, NULL, NULL, NULL, 'staging', 1, 'unknown', ?4, NULL)",
        (
            checkpoint_id.as_slice(),
            i64_of(target_epoch, "epoch")?,
            i64_of(covered_seq, "covered_seq")?,
            now,
        ),
    )?;
    let reservation_id = if target_epoch == vault.active_epoch {
        quota::create_reservation(
            conn,
            now,
            ReservationPurpose::Checkpoint,
            checkpoint_id.as_slice(),
            &device_id,
            estimated_bytes,
            now + secs::CHECKPOINT_LEASE_TTL,
        )?
    } else {
        rotation_reservation(conn, now, &device_id, target_epoch, estimated_bytes)?
    };
    Ok(CreateOutcome {
        checkpoint_id,
        epoch: target_epoch,
        covered_seq,
        reservation_id,
        reservation_expires_at: now + secs::CHECKPOINT_LEASE_TTL,
    })
}

/// 追加 refs（≤1024/请求；只接受 complete 且同 epoch 的对象）。
pub fn add_refs(
    conn: &Connection,
    max_refs: usize,
    checkpoint_id: &Id16,
    refs_in: &[(Hash32, RefRole)],
) -> Result<usize> {
    if refs_in.is_empty() || refs_in.len() > max_refs {
        return Err(Error::api(ErrorCode::InvalidRequest));
    }
    let checkpoint = get(conn, checkpoint_id)?;
    if checkpoint.state != CheckpointState::Staging {
        return Err(Error::api(ErrorCode::InvalidState));
    }
    let mut inserted = 0usize;
    for (object_id, role) in refs_in {
        let object = objects::load(conn, object_id)?;
        if object.epoch != checkpoint.epoch {
            return Err(Error::Api(
                ApiError::new(ErrorCode::EpochMismatch).epoch(checkpoint.epoch),
            ));
        }
        if object.state != objects::ObjectState::Complete {
            return Err(Error::api(ErrorCode::Incomplete));
        }
        match role {
            RefRole::Data => {
                if !matches!(
                    object.object_type,
                    crate::envelope::ObjectType::Snapshot | crate::envelope::ObjectType::Asset
                ) {
                    return Err(Error::api(ErrorCode::InvalidRequest));
                }
            }
            RefRole::Manifest => {
                if object.object_type != crate::envelope::ObjectType::Manifest {
                    return Err(Error::api(ErrorCode::InvalidRequest));
                }
            }
        }
        let existing: Option<String> = conn
            .query_row(
                "SELECT role FROM checkpoint_refs WHERE checkpoint_id = ?1 AND object_id = ?2",
                (checkpoint_id.as_slice(), object_id.as_slice()),
                |row| row.get(0),
            )
            .optional()?;
        match existing {
            Some(existing_role) if RefRole::parse(&existing_role) == Some(*role) => continue,
            Some(_) => return Err(Error::api(ErrorCode::CasConflict)),
            None => {
                // 只允许一个 manifest 本体。
                if *role == RefRole::Manifest {
                    let manifests: i64 = conn.query_row(
                        "SELECT COUNT(*) FROM checkpoint_refs
                         WHERE checkpoint_id = ?1 AND role = 'manifest'",
                        [checkpoint_id.as_slice()],
                        |row| row.get(0),
                    )?;
                    if manifests > 0 {
                        return Err(Error::api(ErrorCode::CasConflict));
                    }
                }
                conn.execute(
                    "INSERT INTO checkpoint_refs(checkpoint_id, object_id, role) VALUES (?1, ?2, ?3)",
                    (checkpoint_id.as_slice(), object_id.as_slice(), role.as_str()),
                )?;
                inserted += 1;
            }
        }
    }
    Ok(inserted)
}

/// 完整性校验（§13.3）：全部 refs 存在、对象 complete、分片密文摘要与记录一致。
pub fn integrity_issues(
    conn: &Connection,
    checkpoint: &CheckpointRow,
) -> Result<Vec<&'static str>> {
    let refs = refs(conn, &checkpoint.checkpoint_id)?;
    if refs.is_empty() {
        return Ok(vec!["no_refs"]);
    }
    let mut issues = Vec::new();
    let mut manifest_seen = 0usize;
    for (object_id, role) in refs {
        if role == RefRole::Manifest {
            manifest_seen += 1;
        }
        match objects::load_opt(conn, &object_id)? {
            None => issues.push("ref_missing"),
            Some(object) => {
                if object.epoch != checkpoint.epoch {
                    issues.push("ref_epoch_mismatch");
                }
                issues.extend(objects::integrity_issues(conn, &object)?);
            }
        }
    }
    if manifest_seen != 1 {
        issues.push("manifest_ref_count");
    }
    if let Some(manifest_oid) = checkpoint.manifest_oid {
        let pinned: Option<i64> = conn
            .query_row(
                "SELECT COUNT(*) FROM checkpoint_refs
                 WHERE checkpoint_id = ?1 AND object_id = ?2 AND role = 'manifest'",
                (checkpoint.checkpoint_id.as_slice(), manifest_oid.as_slice()),
                |row| row.get(0),
            )
            .optional()?;
        if pinned.unwrap_or(0) != 1 {
            issues.push("manifest_not_pinned");
        }
    }
    Ok(issues)
}

/// 发布 checkpoint：校验 refs/摘要/签名，原子标 published 并生成持久 GC 计划。
pub fn publish(
    conn: &Connection,
    clock: &dyn Clock,
    hook: &dyn SignatureHook,
    actor: &AuthContext,
    checkpoint_id: &Id16,
    request: &PublishRequest<'_>,
) -> Result<PublishOutcome> {
    let vault = vault::load(conn)?;
    let device_id = actor.device_id()?;
    let checkpoint = get(conn, checkpoint_id)?;
    let now = clock.now_secs();

    if checkpoint.state == CheckpointState::Published {
        // 幂等：同 commit 字节 → 返回原 published；不同 → CAS 冲突。
        if checkpoint.signed_commit.as_deref() == Some(request.signed_commit) {
            return Ok(PublishOutcome {
                checkpoint_id: checkpoint.checkpoint_id,
                epoch: checkpoint.epoch,
                covered_seq: checkpoint.covered_seq,
                published_at: checkpoint.published_at.unwrap_or(now),
            });
        }
        return Err(Error::api(ErrorCode::CasConflict));
    }
    if checkpoint.state != CheckpointState::Staging {
        return Err(Error::api(ErrorCode::Immutable));
    }
    // epoch 屏障：活动 epoch，或正在轮换的候选 epoch。
    if checkpoint.epoch != vault.active_epoch {
        let candidate = vault.state == vault::VaultState::Rotating
            && vault::epoch_state(conn, checkpoint.epoch)? == Some(vault::EpochState::Staged);
        if !candidate {
            return Err(Error::Api(
                ApiError::new(ErrorCode::EpochMismatch).epoch(vault.active_epoch),
            ));
        }
    }
    if request.covered_seq != checkpoint.covered_seq {
        // S 在创建时固定，publish 不得改。
        return Err(Error::api(ErrorCode::CasConflict));
    }

    let refs = refs(conn, checkpoint_id)?;
    let data_refs: Vec<(Hash32, RefRole)> = refs
        .iter()
        .filter(|(_, role)| *role == RefRole::Data)
        .copied()
        .collect();
    if data_refs.is_empty() {
        return Err(Error::api(ErrorCode::Incomplete));
    }
    // 服务端完整性门禁：任何损坏都不得 publish（也防止日后 GC 误删）。
    let issues = integrity_issues(conn, &checkpoint)?;
    if !issues.is_empty() {
        mark_health(conn, checkpoint_id, Health::Unhealthy)?;
        return Err(Error::api(ErrorCode::Incomplete));
    }
    // manifest 绑定：publish 请求声明 manifest_oid，必须与 pin 的 manifest ref 一致。
    let manifest_refs: Vec<Hash32> = refs
        .iter()
        .filter(|(_, role)| *role == RefRole::Manifest)
        .map(|(id, _)| *id)
        .collect();
    if manifest_refs.len() != 1 || manifest_refs[0] != request.manifest_oid {
        return Err(Error::api(ErrorCode::CasConflict));
    }

    // §15：公开 refs 按 object_id 升序；refs_digest 只覆盖 snapshot/asset 数据对象。
    let mut metas = Vec::with_capacity(data_refs.len());
    for (object_id, _) in &data_refs {
        metas.push(objects::object_ref_meta(conn, object_id)?);
    }
    metas.sort_by_key(|meta| meta.object_id);

    let public_key = vault::device_sign_pk(conn, &device_id)?
        .ok_or_else(|| Error::Api(ApiError::new(ErrorCode::Revoked).epoch(vault.active_epoch)))?;
    let check = SignatureCheck {
        domain: SignDomain::Checkpoint,
        vault_id: &vault.vault_id,
        device_id: Some(&device_id),
        public_key: &public_key,
        message: request.signed_commit,
        signature: request.signature,
        expect: &Expectation::CheckpointPublish {
            checkpoint_id,
            covered_seq: checkpoint.covered_seq,
            manifest_oid: &request.manifest_oid,
            refs_digest: &request.refs_digest,
            refs: &metas,
        },
    };
    require(hook, &check, ErrorCode::SignatureInvalid)?;

    conn.execute(
        "UPDATE checkpoints SET state = 'published', published_at = ?1, signed_commit = ?2,
                                manifest_oid = ?3, refs_digest = ?4, health = 'healthy'
         WHERE checkpoint_id = ?5 AND state = 'staging'",
        (
            now,
            request.signed_commit,
            request.manifest_oid.as_slice(),
            request.refs_digest.as_slice(),
            checkpoint_id.as_slice(),
        ),
    )?;
    // 释放该 checkpoint 的维护 reservation（剩余额度回到 R_available；不删数据）。
    conn.execute(
        "UPDATE maintenance_reservations SET state = 'complete'
         WHERE resource_id = ?1 AND purpose = 'checkpoint' AND state = 'active'",
        [checkpoint_id.as_slice()],
    )?;
    schedule_gc_plan(conn, now, checkpoint_id)?;
    Ok(PublishOutcome {
        checkpoint_id: *checkpoint_id,
        epoch: checkpoint.epoch,
        covered_seq: checkpoint.covered_seq,
        published_at: now,
    })
}

/// 新 pair 产生持久 GC 计划；**已有计划不重置 deadline**（ADR §7）。
fn schedule_gc_plan(conn: &Connection, now: i64, new_checkpoint: &Id16) -> Result<()> {
    let published = published_newest_first(conn, 2)?;
    if published.len() < 2 {
        return Ok(());
    }
    let newer = &published[0];
    let older = &published[1];
    if newer.checkpoint_id != *new_checkpoint {
        return Ok(());
    }
    let mut proof = Vec::with_capacity(64);
    proof.extend_from_slice(&newer.checkpoint_id);
    proof.extend_from_slice(&older.checkpoint_id);
    proof.extend_from_slice(
        newer
            .refs_digest
            .as_ref()
            .map(|d| d.as_slice())
            .unwrap_or(&[]),
    );
    proof.extend_from_slice(
        older
            .refs_digest
            .as_ref()
            .map(|d| d.as_slice())
            .unwrap_or(&[]),
    );
    conn.execute(
        "INSERT INTO gc_plans(epoch, cut_seq, eligible_at, proof, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(epoch, cut_seq) DO NOTHING",
        (
            i64_of(older.epoch, "epoch")?,
            i64_of(older.covered_seq, "cut_seq")?,
            now + secs::GC_GRACE,
            proof,
            now,
        ),
    )?;
    Ok(())
}

/// 已发布（且未被标 unhealthy）的 checkpoint，按 published_at 倒序。
pub fn published_newest_first(conn: &Connection, limit: u32) -> Result<Vec<CheckpointRow>> {
    let mut stmt = conn.prepare(
        "SELECT checkpoint_id FROM checkpoints
         WHERE state = 'published' AND health != 'unhealthy'
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
        out.push(get(conn, &id)?);
    }
    Ok(out)
}

/// bootstrap 用的两份基线：必须属于当前活动 epoch，且恰好两份可独立导入。
pub fn active_baselines(conn: &Connection) -> Result<Vec<CheckpointRow>> {
    let vault = vault::load(conn)?;
    let all = published_newest_first(conn, 8)?;
    let mut out: Vec<CheckpointRow> = all
        .into_iter()
        .filter(|cp| cp.epoch == vault.active_epoch && cp.health != Health::Unhealthy)
        .collect();
    out.truncate(2);
    Ok(out)
}

/// 标记健康度。
pub fn mark_health(conn: &Connection, checkpoint_id: &Id16, health: Health) -> Result<()> {
    conn.execute(
        "UPDATE checkpoints SET health = ?1 WHERE checkpoint_id = ?2",
        (health.as_str(), checkpoint_id.as_slice()),
    )?;
    Ok(())
}

/// 某 epoch 已发布 checkpoint 的 `(数量, 最小 covered_seq)`。
pub fn published_for_epoch(conn: &Connection, epoch: u64) -> Result<(u64, u64)> {
    let (count, min_seq): (i64, Option<i64>) = conn.query_row(
        "SELECT COUNT(*), MIN(covered_seq) FROM checkpoints
         WHERE epoch = ?1 AND state = 'published' AND health != 'unhealthy'",
        [i64_of(epoch, "epoch")?],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    Ok((
        u64_of(count.max(0), "checkpoint count")?,
        match min_seq {
            None => 0,
            Some(value) => u64_of(value, "covered_seq")?,
        },
    ))
}

// ---------------- bootstrap lease ----------------

/// bootstrap read lease（ADR §7：默认 24h，从创建起最长 7 天）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseRow {
    pub lease_id: Id16,
    pub device_id: Id16,
    pub primary_checkpoint: Id16,
    pub fallback_checkpoint: Id16,
    pub floor_seq: u64,
    pub created_at: i64,
    pub expires_at: i64,
}

/// bootstrap 结果：两基线 + 各自游标 + head + lease。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapOutcome {
    pub lease: LeaseRow,
    pub primary: CheckpointRow,
    pub fallback: CheckpointRow,
    pub head_seq: u64,
    pub gc_floor: u64,
    pub epoch: u64,
}

/// 建立 bootstrap lease：没有两份有效基线 → 409 NO_VALID_BASELINE。
pub fn bootstrap(
    conn: &Connection,
    clock: &dyn Clock,
    actor: &AuthContext,
) -> Result<BootstrapOutcome> {
    let device_id = actor.device_id()?;
    let vault = vault::load(conn)?;
    let baselines = active_baselines(conn)?;
    if baselines.len() < 2 {
        return Err(Error::Api(
            ApiError::new(ErrorCode::NoValidBaseline).epoch(vault.active_epoch),
        ));
    }
    let primary = baselines[0].clone();
    let fallback = baselines[1].clone();
    // 两份基线必须都健康（integrity 校验在 GC 侧强制；这里做只读预检，避免给坏基线发 lease）。
    for checkpoint in [&primary, &fallback] {
        let issues = integrity_issues(conn, checkpoint)?;
        if !issues.is_empty() {
            mark_health(conn, &checkpoint.checkpoint_id, Health::Unhealthy)?;
            return Err(Error::Api(
                ApiError::new(ErrorCode::NoValidBaseline).epoch(vault.active_epoch),
            ));
        }
    }
    let now = clock.now_secs();
    let lease = LeaseRow {
        lease_id: crate::ids::random_id16()?,
        device_id,
        primary_checkpoint: primary.checkpoint_id,
        fallback_checkpoint: fallback.checkpoint_id,
        floor_seq: fallback.covered_seq,
        created_at: now,
        expires_at: now + secs::BOOTSTRAP_LEASE_TTL,
    };
    conn.execute(
        "INSERT INTO bootstrap_leases(lease_id, device_id, primary_checkpoint, fallback_checkpoint,
                                      floor_seq, created_at, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        (
            lease.lease_id.as_slice(),
            lease.device_id.as_slice(),
            lease.primary_checkpoint.as_slice(),
            lease.fallback_checkpoint.as_slice(),
            i64_of(lease.floor_seq, "floor_seq")?,
            lease.created_at,
            lease.expires_at,
        ),
    )?;
    Ok(BootstrapOutcome {
        lease,
        primary,
        fallback,
        head_seq: vault.head_seq,
        gc_floor: vault.gc_floor,
        epoch: vault.active_epoch,
    })
}

/// 续租：只能延长到 `created_at + 7d`（ADR §7 上限），过期 lease → 410 LEASE_EXPIRED。
pub fn renew_lease(
    conn: &Connection,
    clock: &dyn Clock,
    actor: &AuthContext,
    lease_id: &Id16,
) -> Result<LeaseRow> {
    let device_id = actor.device_id()?;
    let now = clock.now_secs();
    let mut stmt = conn.prepare(
        "SELECT lease_id, device_id, primary_checkpoint, fallback_checkpoint, floor_seq,
                created_at, expires_at
         FROM bootstrap_leases WHERE lease_id = ?1",
    )?;
    let row = stmt
        .query_row([lease_id.as_slice()], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, Vec<u8>>(2)?,
                row.get::<_, Vec<u8>>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, i64>(6)?,
            ))
        })
        .optional()?
        .ok_or_else(|| Error::api(ErrorCode::NotFound))?;
    drop(stmt);
    let lease = LeaseRow {
        lease_id: row
            .0
            .as_slice()
            .try_into()
            .map_err(|_| Error::Invariant("lease_id"))?,
        device_id: row
            .1
            .as_slice()
            .try_into()
            .map_err(|_| Error::Invariant("lease device"))?,
        primary_checkpoint: row
            .2
            .as_slice()
            .try_into()
            .map_err(|_| Error::Invariant("lease primary"))?,
        fallback_checkpoint: row
            .3
            .as_slice()
            .try_into()
            .map_err(|_| Error::Invariant("lease fallback"))?,
        floor_seq: u64_of(row.4, "lease floor_seq")?,
        created_at: row.5,
        expires_at: row.6,
    };
    if lease.device_id != device_id {
        return Err(Error::api(ErrorCode::Forbidden));
    }
    if lease.expires_at <= now {
        return Err(Error::Api(ApiError::new(ErrorCode::LeaseExpired).epoch(0)));
    }
    let max_expiry = lease.created_at + secs::BOOTSTRAP_LEASE_MAX;
    let expires_at = (now + secs::BOOTSTRAP_LEASE_TTL).min(max_expiry);
    conn.execute(
        "UPDATE bootstrap_leases SET expires_at = ?1 WHERE lease_id = ?2",
        (expires_at, lease_id.as_slice()),
    )?;
    Ok(LeaseRow {
        expires_at,
        ..lease
    })
}

/// 当前有效（未过期）的 lease 最小 floor_seq（GC 用）。
pub fn active_lease_floor(conn: &Connection, now: i64) -> Result<Option<u64>> {
    let value: Option<Option<i64>> = conn
        .query_row(
            "SELECT MIN(floor_seq) FROM bootstrap_leases WHERE expires_at > ?1",
            [now],
            |row| row.get(0),
        )
        .optional()?;
    match value.flatten() {
        None => Ok(None),
        Some(value) => Ok(Some(u64_of(value, "lease floor_seq")?)),
    }
}

/// checkpoint 的两基线 S 与其证明摘要（诊断）。
pub fn pair_digest(primary: &CheckpointRow, fallback: &CheckpointRow) -> Hash32 {
    let mut buf = Vec::new();
    buf.extend_from_slice(&primary.checkpoint_id);
    buf.extend_from_slice(be64(primary.covered_seq).as_slice());
    buf.extend_from_slice(&fallback.checkpoint_id);
    buf.extend_from_slice(be64(fallback.covered_seq).as_slice());
    sha256_parts(&[&buf])
}

/// refs 集合的摘要（诊断用，不用于 wire；wire 的 `refs_digest` 由 hook 按 CDE 计算）。
pub fn refs_fingerprint(conn: &Connection, checkpoint_id: &Id16) -> Result<Hash32> {
    let refs = refs(conn, checkpoint_id)?;
    let mut buf = Vec::new();
    for (object_id, role) in refs {
        buf.extend_from_slice(&object_id);
        buf.push(match role {
            RefRole::Data => 0,
            RefRole::Manifest => 1,
        });
    }
    Ok(sha256(&buf))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkpoint_state_and_health_roundtrip() {
        assert_eq!(
            CheckpointState::parse(CheckpointState::Published.as_str()),
            Some(CheckpointState::Published)
        );
        assert_eq!(Health::parse("unhealthy"), Some(Health::Unhealthy));
        assert_eq!(Health::parse("bad"), None);
    }

    #[test]
    fn pair_digest_binds_both_baselines() {
        let mut a = CheckpointRow {
            checkpoint_id: [1u8; 16],
            epoch: 1,
            covered_seq: 10,
            manifest_oid: None,
            refs_digest: None,
            signed_commit: None,
            state: CheckpointState::Published,
            validation_version: 1,
            health: Health::Healthy,
            created_at: 0,
            published_at: Some(0),
        };
        let b = CheckpointRow {
            checkpoint_id: [2u8; 16],
            covered_seq: 20,
            ..a.clone()
        };
        let d1 = pair_digest(&a, &b);
        a.covered_seq = 11;
        assert_ne!(d1, pair_digest(&a, &b));
    }
}
