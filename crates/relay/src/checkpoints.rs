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
use crate::envelope::ObjectType;
use crate::error::{ApiError, Error, ErrorCode, Result};
use crate::ids::{be64, sha256, sha256_parts, Hash32, Id16};
use crate::objects;
use crate::quota::{self, ReservationPurpose};
use crate::refs::RefRole;
use crate::sig::{require, Expectation, SignDomain, SignatureCheck, SignatureHook};
use crate::time::{secs, Clock};
use crate::vault;
use crate::wire;
use menghunote_sync_proto as proto;

/// checkpoint 状态（ADR §6）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckpointState {
    Staging,
    /// §24.1：非 active epoch 发布后只能处于 candidate，不参与 bootstrap/GC。
    Candidate,
    Published,
    /// §24.1：候选 epoch 被 abort 后其 checkpoint 作废，永不复用。
    Aborted,
    Pruned,
}

impl CheckpointState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Staging => "staging",
            Self::Candidate => "candidate",
            Self::Published => "published",
            Self::Aborted => "aborted",
            Self::Pruned => "pruned",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "staging" => Some(Self::Staging),
            "candidate" => Some(Self::Candidate),
            "published" => Some(Self::Published),
            "aborted" => Some(Self::Aborted),
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

/// 候选 checkpoint 的固定角色（activate 时落定；wire 输出 primary/fallback）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckpointRole {
    Primary,
    Fallback,
}

impl CheckpointRole {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Fallback => "fallback",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "primary" => Some(Self::Primary),
            "fallback" => Some(Self::Fallback),
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
    /// §20.1：生产者对 `signed_commit` 的 Ed25519 签名（bootstrap 返回给客户端验签）。
    pub commit_signature: Option<Vec<u8>>,
    pub state: CheckpointState,
    /// §28.1 条目 3：候选基线在 activate 时被钉成 primary/fallback；旧行为 NULL。
    pub role: Option<CheckpointRole>,
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
    /// 候选 epoch 的固定角色（active epoch 为 None）。
    pub role: Option<CheckpointRole>,
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
                commit_signature, state, validation_version, health, created_at, published_at,
                role
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
                row.get::<_, Option<Vec<u8>>>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, i64>(8)?,
                row.get::<_, String>(9)?,
                row.get::<_, i64>(10)?,
                row.get::<_, Option<i64>>(11)?,
                row.get::<_, Option<String>>(12)?,
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
        commit_signature: row.6,
        state: CheckpointState::parse(&row.7).ok_or(Error::Invariant("checkpoints.state"))?,
        role: match row.12 {
            None => None,
            Some(role) => {
                Some(CheckpointRole::parse(&role).ok_or(Error::Invariant("checkpoints.role"))?)
            }
        },
        validation_version: u64_of(row.8, "validation_version")?,
        health: Health::parse(&row.9).ok_or(Error::Invariant("checkpoints.health"))?,
        created_at: row.10,
        published_at: row.11,
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

/// checkpoint 创建的幂等 scope（§28.1 条目 3：request-id 创建 + 固定 role）。
const DEDUP_CHECKPOINT_CREATE: &str = "checkpoint/create";

fn dedup_lookup(conn: &Connection, scope: &str, id: &[u8]) -> Result<Option<(Hash32, Vec<u8>)>> {
    let mut stmt =
        conn.prepare("SELECT request_hash, response FROM dedup WHERE scope = ?1 AND id = ?2")?;
    let row = stmt
        .query_row((scope, id), |row| {
            Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?))
        })
        .optional()?;
    match row {
        None => Ok(None),
        Some((hash, response)) => Ok(Some((
            hash.as_slice()
                .try_into()
                .map_err(|_| Error::Invariant("dedup.request_hash"))?,
            response,
        ))),
    }
}

/// 候选 checkpoint 是否 **readback-ready**（candidate + 覆盖冻结 S + refs 完整性通过）。
/// ready 与 published 是两件事：ready 前不能 activate，published 只由 activate 产生。
pub fn candidate_ready(conn: &Connection, checkpoint: &CheckpointRow) -> Result<bool> {
    if checkpoint.state != CheckpointState::Candidate || checkpoint.health == Health::Unhealthy {
        return Ok(false);
    }
    let Some(freeze_seq) = crate::rotations::freeze_seq_for_epoch(conn, checkpoint.epoch)? else {
        return Ok(false);
    };
    if checkpoint.covered_seq < freeze_seq {
        return Ok(false);
    }
    Ok(integrity_issues(conn, checkpoint)?.is_empty())
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
///
/// §28.1 条目 3：候选 epoch 的 `role` 在**创建时固定**（primary/fallback）；带
/// `request_id` 时创建幂等（同 id 同请求 → 回原结果，同 id 不同请求 → 409）。
#[allow(clippy::too_many_arguments)]
pub fn create(
    conn: &Connection,
    clock: &dyn Clock,
    actor: &AuthContext,
    estimated_bytes: i64,
    epoch: Option<u64>,
    role: Option<CheckpointRole>,
    request_id: Option<Id16>,
) -> Result<CreateOutcome> {
    let device_id = actor.device_id()?;
    let vault = vault::load(conn)?;
    let now = clock.now_secs();
    let request_hash = request_id.map(|_| {
        let mut fields = vec![(
            0,
            wire::uint(u64::try_from(estimated_bytes.max(0)).unwrap_or(0)),
        )];
        if let Some(epoch) = epoch {
            fields.push((1, wire::uint(epoch)));
        }
        if let Some(role) = role {
            fields.push((2, wire::text(role.as_str())));
        }
        sha256(&wire::encode(fields).unwrap_or_default())
    });
    if let (Some(request_id), Some(request_hash)) = (request_id, request_hash) {
        if let Some((stored_hash, response)) =
            dedup_lookup(conn, DEDUP_CHECKPOINT_CREATE, &request_id)?
        {
            if stored_hash != request_hash {
                return Err(Error::api(ErrorCode::IdConflict));
            }
            let fields = wire::decode(&response)?;
            let role = match wire::opt_field(&fields, 5) {
                None => None,
                Some(wire::CdeValue::Text(text)) => {
                    Some(CheckpointRole::parse(text).ok_or(Error::Invariant("create role"))?)
                }
                Some(_) => return Err(Error::Invariant("create role type")),
            };
            return Ok(CreateOutcome {
                checkpoint_id: wire::id16_field(&fields, 0)?,
                epoch: wire::uint_field(&fields, 1)?,
                covered_seq: wire::uint_field(&fields, 2)?,
                reservation_id: wire::id16_field(&fields, 3)?,
                reservation_expires_at: i64::try_from(wire::uint_field(&fields, 4)?)
                    .map_err(|_| Error::Invariant("reservation expires"))?,
                role,
            });
        }
    }
    let target_epoch = epoch.unwrap_or(vault.active_epoch);
    if role.is_some() && target_epoch == vault.active_epoch {
        // 角色只用于"候选 epoch 的两份基线"；active epoch 的基线由 published_at 决定。
        return Err(Error::api(ErrorCode::InvalidRequest));
    }
    if let Some(role) = role {
        let existing: i64 = conn.query_row(
            "SELECT COUNT(*) FROM checkpoints
             WHERE epoch = ?1 AND role = ?2 AND state IN ('staging','candidate','published')",
            (i64_of(target_epoch, "epoch")?, role.as_str()),
            |row| row.get(0),
        )?;
        if existing != 0 {
            return Err(Error::api(ErrorCode::CasConflict));
        }
    }
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
                                signed_commit, state, role, validation_version, health, created_at,
                                published_at)
         VALUES (?1, ?2, ?3, NULL, NULL, NULL, 'staging', ?4, 1, 'unknown', ?5, NULL)",
        (
            checkpoint_id.as_slice(),
            i64_of(target_epoch, "epoch")?,
            i64_of(covered_seq, "covered_seq")?,
            role.map(|role| role.as_str()),
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
    let reservation_expires_at = now + secs::CHECKPOINT_LEASE_TTL;
    if let (Some(request_id), Some(request_hash)) = (request_id, request_hash) {
        let mut fields = vec![
            (0, wire::bytes(checkpoint_id)),
            (1, wire::uint(target_epoch)),
            (2, wire::uint(covered_seq)),
            (3, wire::bytes(reservation_id)),
            (
                4,
                wire::uint(u64::try_from(reservation_expires_at).unwrap_or(0)),
            ),
        ];
        if let Some(role) = role {
            fields.push((5, wire::text(role.as_str())));
        }
        let response = wire::encode(fields)?;
        conn.execute(
            "INSERT INTO dedup(scope, id, request_hash, accepted_seq, response, created_at)
             VALUES (?1, ?2, ?3, NULL, ?4, ?5)",
            (
                DEDUP_CHECKPOINT_CREATE,
                request_id.as_slice(),
                request_hash.as_slice(),
                response.as_slice(),
                now,
            ),
        )?;
    }
    Ok(CreateOutcome {
        checkpoint_id,
        epoch: target_epoch,
        covered_seq,
        reservation_id,
        reservation_expires_at,
        role,
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

/// 发布 checkpoint：校验 refs/摘要/签名/未来 live，原子标 published 并生成持久 GC 计划。
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

    // §24.1：只有 active epoch 的 checkpoint 才算 published；候选 epoch 只能 candidate。
    let target_state = if checkpoint.epoch == vault.active_epoch {
        "published"
    } else {
        "candidate"
    };
    // ADR §7 / 评审 0002 P1-5 + T20 + P1-E：发布前校验"未来 live ≤ Q"。
    //
    // "未来 live" = 发布后最新两份 published 基线引用的 DATA + MANIFEST 对象、`log` 仍保留的
    // UPDATE、以及同 epoch 未 retired 且未被 pair 引用的 finalized ASSET（口径见
    // `quota::future_live_bytes`；共享对象只计一次）。超预算只拒绝（507），
    // **不删任何安全根、不建 GC 计划**：失败发生在所有写操作之前，外层写事务整体回滚，
    // staging checkpoint / refs / 对象 / 日志保持原样，扩容后可重试。
    if target_state == "published" {
        let live_after = quota::future_live_bytes(conn, checkpoint.epoch, &[*checkpoint_id])?;
        if live_after > vault.normal_limit {
            return Err(Error::Api(
                ApiError::new(ErrorCode::QuotaExceeded).epoch(vault.active_epoch),
            ));
        }
    }
    conn.execute(
        "UPDATE checkpoints SET state = ?1, published_at = ?2, signed_commit = ?3,
                                commit_signature = ?4, manifest_oid = ?5, refs_digest = ?6,
                                health = 'healthy'
         WHERE checkpoint_id = ?7 AND state = 'staging'",
        (
            target_state,
            now,
            request.signed_commit,
            request.signature,
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
    if target_state == "published" {
        schedule_gc_plan(conn, now, checkpoint.epoch, checkpoint_id)?;
        // 评审 0002 P1-E：发布后按真实 parts 对账——新 pair 的活对象（原维护 reservation
        // 写入）升为 normal，旧基线/宽限副本降为 maintenance，账本 = 真实 parts。
        quota::recompute(conn)?;
    }
    Ok(PublishOutcome {
        checkpoint_id: *checkpoint_id,
        epoch: checkpoint.epoch,
        covered_seq: checkpoint.covered_seq,
        published_at: now,
    })
}

/// 新 pair 产生持久 GC 计划；**已有计划不重置 deadline**（ADR §7）。
fn schedule_gc_plan(conn: &Connection, now: i64, epoch: u64, new_checkpoint: &Id16) -> Result<()> {
    let published = published_newest_first_for_epoch(conn, epoch, 2)?;
    if published.len() < 2 {
        return Ok(());
    }
    let newer = &published[0];
    let older = &published[1];
    if newer.checkpoint_id != *new_checkpoint {
        return Ok(());
    }
    schedule_plan_for_pair(
        conn,
        now,
        older.epoch,
        older.covered_seq,
        &pair_proof(newer, older),
    )
}

/// activate 指名并 promote 的一对候选基线（§28.1 条目 3）。
#[derive(Debug, Clone)]
pub struct CandidatePair {
    pub primary: CheckpointRow,
    pub fallback: CheckpointRow,
    /// GC 计划的 `cut_seq = min(S_primary, S_fallback)`。
    pub cut_seq: u64,
    pub proof: Vec<u8>,
}

/// 候选 epoch 的全部候选 checkpoint（诊断/自动提名；按角色、S、创建序排序）。
pub fn candidates_for_epoch(conn: &Connection, epoch: u64) -> Result<Vec<CheckpointRow>> {
    let mut stmt = conn.prepare(
        "SELECT checkpoint_id FROM checkpoints
         WHERE epoch = ?1 AND state = 'candidate'
         ORDER BY role IS NULL, role ASC, covered_seq DESC, rowid DESC",
    )?;
    let ids: Vec<Vec<u8>> = stmt
        .query_map([i64_of(epoch, "epoch")?], |row| row.get::<_, Vec<u8>>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut rows = Vec::with_capacity(ids.len());
    for id in ids {
        let id: Id16 = id
            .as_slice()
            .try_into()
            .map_err(|_| Error::Invariant("checkpoint_id"))?;
        rows.push(get(conn, &id)?);
    }
    Ok(rows)
}

/// 某 checkpoint 引用的 SNAPSHOT 数据对象（§28.1 条目 3：两个候选基线的 snapshot
/// refs 必须不相交；同 epoch ASSET 允许共享，因为资产复制是同一份已校验对象）。
fn snapshot_refs(conn: &Connection, checkpoint: &CheckpointRow) -> Result<Vec<Hash32>> {
    let mut out = Vec::new();
    for (object_id, role) in refs(conn, &checkpoint.checkpoint_id)? {
        if role != RefRole::Data {
            continue;
        }
        if let Some(object) = objects::load_opt(conn, &object_id)? {
            if object.object_type == ObjectType::Snapshot {
                out.push(object_id);
            }
        }
    }
    Ok(out)
}

/// 被签名的 `CheckpointCommit` 必须与 checkpoint 行逐字段绑定（commit/摘要绑定）。
fn commit_binding_issues(checkpoint: &CheckpointRow) -> Vec<&'static str> {
    let mut issues = Vec::new();
    let Some(signed) = checkpoint.signed_commit.as_deref() else {
        issues.push("commit_missing");
        return issues;
    };
    if checkpoint.commit_signature.is_none() {
        issues.push("commit_signature_missing");
    }
    match proto::CheckpointCommit::decode(signed) {
        Err(_) => issues.push("commit_undecodable"),
        Ok(commit) => {
            if commit.checkpoint_id.as_slice() != checkpoint.checkpoint_id.as_slice() {
                issues.push("commit_checkpoint_mismatch");
            }
            if commit.covered_relay_seq != checkpoint.covered_seq {
                issues.push("commit_seq_mismatch");
            }
            if Some(commit.manifest_object_id) != checkpoint.manifest_oid {
                issues.push("commit_manifest_mismatch");
            }
            if Some(commit.refs_digest) != checkpoint.refs_digest {
                issues.push("commit_refs_digest_mismatch");
            }
        }
    }
    issues
}

/// §24.1 / §28.1 条目 2–3：activate **只** promote 被指名的两份候选基线。
///
/// 校验（任一不满足 → 409，客户端必须重新准备/restart）：
/// - 两个 cid 不同、都属于该候选 epoch 且 state=candidate（固定 role 若已存在必须匹配）；
/// - 都覆盖冻结 S（`covered_seq >= freeze_seq`）、健康且 refs 完整性通过；
/// - `manifest_oid` 不同、`SNAPSHOT` refs 不相交（同 epoch ASSET 可共享）、
///   `signed_commit`/`refs_digest` 与行逐字段绑定；
/// - 其余候选 checkpoint 标 `aborted`（永不复用），只有指名的两份变 published。
pub fn promote_named_candidates(
    conn: &Connection,
    epoch: u64,
    primary_id: &Id16,
    fallback_id: &Id16,
    freeze_seq: u64,
    now: i64,
) -> Result<CandidatePair> {
    if primary_id == fallback_id {
        // 复用同一个 cid 不是"两份独立基线"。
        return Err(Error::api(ErrorCode::CasConflict));
    }
    let mut rows = Vec::with_capacity(2);
    for (id, expected_role) in [
        (primary_id, CheckpointRole::Primary),
        (fallback_id, CheckpointRole::Fallback),
    ] {
        let checkpoint = get(conn, id)?;
        if checkpoint.epoch != epoch || checkpoint.state != CheckpointState::Candidate {
            return Err(Error::api(ErrorCode::InvalidState));
        }
        if checkpoint.role.is_some_and(|role| role != expected_role) {
            return Err(Error::api(ErrorCode::CasConflict));
        }
        if checkpoint.covered_seq < freeze_seq {
            return Err(Error::api(ErrorCode::Incomplete));
        }
        if checkpoint.health == Health::Unhealthy {
            return Err(Error::api(ErrorCode::Incomplete));
        }
        if !integrity_issues(conn, &checkpoint)?.is_empty() {
            mark_health(conn, id, Health::Unhealthy)?;
            return Err(Error::api(ErrorCode::Incomplete));
        }
        if !commit_binding_issues(&checkpoint).is_empty() {
            return Err(Error::api(ErrorCode::CasConflict));
        }
        rows.push(checkpoint);
    }
    let primary = rows.remove(0);
    let fallback = rows.remove(0);
    // 独立性：manifest 不同、SNAPSHOT refs 不相交（同 epoch ASSET 可共享）。
    match (primary.manifest_oid, fallback.manifest_oid) {
        (Some(left), Some(right)) if left != right => {}
        _ => return Err(Error::api(ErrorCode::CasConflict)),
    }
    if primary.refs_digest.is_none() || fallback.refs_digest.is_none() {
        return Err(Error::api(ErrorCode::CasConflict));
    }
    let primary_snapshots = snapshot_refs(conn, &primary)?;
    let fallback_snapshots = snapshot_refs(conn, &fallback)?;
    if primary_snapshots
        .iter()
        .any(|object_id| fallback_snapshots.contains(object_id))
    {
        return Err(Error::api(ErrorCode::CasConflict));
    }
    // 评审 0002 P1-5 / P1-E：rotation candidate activate 的 Q 门禁。此前只有 active
    // publish 校验 future_live≤Q，activate 可把维护 reservation 写出的候选 pair 直接
    // promote 成 live，令 activate 后 live 超 Q、账本永久高于 Q。这里在**任何 promotion
    // 效果之前**（abort 未指名候选、改 state、建 GC 计划都由调用方在同一事务后续执行）
    // 用与 publish 同一 `quota::future_live_bytes` 计算 prospect pair；超 Q → 507、
    // 不删任何安全根，事务整体回滚后可扩容重试。
    let prospective = quota::future_live_bytes(conn, epoch, &[*primary_id, *fallback_id])?;
    let vault = vault::load(conn)?;
    if prospective > vault.normal_limit {
        return Err(Error::Api(
            ApiError::new(ErrorCode::QuotaExceeded).epoch(vault.active_epoch),
        ));
    }
    // 未指名的候选作废（永不 promote/复用）。
    conn.execute(
        "UPDATE checkpoints SET state = 'aborted'
         WHERE epoch = ?1 AND state = 'candidate'
           AND checkpoint_id NOT IN (?2, ?3)",
        (
            i64_of(epoch, "epoch")?,
            primary.checkpoint_id.as_slice(),
            fallback.checkpoint_id.as_slice(),
        ),
    )?;
    for (checkpoint, role) in [
        (&primary, CheckpointRole::Primary),
        (&fallback, CheckpointRole::Fallback),
    ] {
        conn.execute(
            "UPDATE checkpoints SET state = 'published', role = ?1, published_at = ?2
             WHERE checkpoint_id = ?3 AND state = 'candidate'",
            (role.as_str(), now, checkpoint.checkpoint_id.as_slice()),
        )?;
    }
    let cut_seq = primary.covered_seq.min(fallback.covered_seq);
    let proof = pair_proof(&primary, &fallback);
    Ok(CandidatePair {
        primary,
        fallback,
        cut_seq,
        proof,
    })
}

/// §28.1 条目 9 / 评审 §一 10：旧 checkpoint 无根/lease 后 prune —— 清 refs、保留签名骨架。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PruneOutcome {
    pub pruned_checkpoints: u64,
    pub retired_objects: u64,
}

/// 旧 epoch / 候选垃圾的**破坏性**回收（§S7-35）。
///
/// 调用前提：调用方（`gc::tick`）已确认 active epoch 的两份保留根健康；本函数不再
/// 自行判断"能不能删"，只负责"删什么"。degraded/不足两份健康基线时不得调用。
///
/// 规则：
/// - `keep` = active epoch 当前两份健康基线，永不剪；
/// - 有效 bootstrap lease 引用的 checkpoint 不剪（客户端可能正在读）；
/// - 进行中的轮换（`rotation_id_for_epoch` 命中）的候选 epoch 不剪；
/// - 其余 `published/staging/candidate/aborted` 在 7 天密文宽限后一律剪（**不再**对每个
///   retired epoch 无条件保留最后两份 full 基线）；
/// - 剪枝 = 清 `checkpoint_refs` + state='pruned'，**保留** checkpoint 行与
///   signed_commit/commit_signature/manifest_oid/refs_digest 签名骨架（可审计，不参与
///   bootstrap——bootstrap 只查 state='published'）；
/// - 对象回收：无任何 checkpoint refs、无 log 行、完成超过 7 天的对象删 parts 并标
///   `budget_class='retired'`（行保留 tombstone）。只回收**旧 epoch** 的对象（含
///   **ASSET 副本** 与 update/snapshot/manifest）；active epoch 的孤儿对象一律后置，
///   不在这里回收（同 epoch 孤儿删除仍按评审"可后置"处理）。
/// - ASSET 对象被回收时同步把 `asset_objects.state` 标 `retired`（历史事实保留，但
///   枚举不再声称密文可用）。
pub fn prune_retired(conn: &Connection, now: i64, keep: &[Id16]) -> Result<PruneOutcome> {
    let vault = vault::load(conn)?;
    let cutoff = now - secs::GC_GRACE;
    let mut leased: Vec<Id16> = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT primary_checkpoint, fallback_checkpoint FROM bootstrap_leases
             WHERE expires_at > ?1",
        )?;
        let rows = stmt.query_map([now], |row| {
            Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?;
        for row in rows {
            let (primary, fallback) = row?;
            for bytes in [primary, fallback] {
                leased.push(
                    bytes
                        .as_slice()
                        .try_into()
                        .map_err(|_| Error::Invariant("lease checkpoint"))?,
                );
            }
        }
    }
    let rows: Vec<(Id16, u64, String, Option<i64>, i64)> = {
        let mut stmt = conn.prepare(
            "SELECT checkpoint_id, epoch, state, published_at, created_at FROM checkpoints
             WHERE state IN ('published','staging','candidate','aborted')
             ORDER BY epoch ASC, rowid ASC",
        )?;
        let mapped = stmt.query_map([], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<i64>>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in mapped {
            let (id, epoch, state, published_at, created_at) = row?;
            out.push((
                id.as_slice()
                    .try_into()
                    .map_err(|_| Error::Invariant("checkpoint_id"))?,
                u64_of(epoch, "checkpoints.epoch")?,
                state,
                published_at,
                created_at,
            ));
        }
        out
    };
    let mut outcome = PruneOutcome::default();
    for (checkpoint_id, epoch, _state, published_at, created_at) in rows {
        if keep.contains(&checkpoint_id) || leased.contains(&checkpoint_id) {
            continue;
        }
        if epoch != vault.active_epoch {
            // 轮换进行中的候选 epoch 可能是客户端还在 prepare/validate 的材料。
            if crate::rotations::rotation_id_for_epoch(conn, epoch)?.is_some() {
                continue;
            }
            match vault::epoch_state(conn, epoch)? {
                Some(vault::EpochState::Retired) | Some(vault::EpochState::Aborted) => {}
                _ => continue,
            }
        }
        let stamp = published_at.unwrap_or(created_at);
        if stamp > cutoff {
            continue;
        }
        conn.execute(
            "DELETE FROM checkpoint_refs WHERE checkpoint_id = ?1",
            [checkpoint_id.as_slice()],
        )?;
        conn.execute(
            "UPDATE checkpoints SET state = 'pruned' WHERE checkpoint_id = ?1",
            [checkpoint_id.as_slice()],
        )?;
        outcome.pruned_checkpoints += 1;
    }
    outcome.retired_objects = retire_unreferenced_objects(conn, vault.active_epoch, cutoff)?;
    Ok(outcome)
}

/// 回收"无 refs、无 log、完成超过 7 天"的对象 parts；见 [`prune_retired`] 的规则说明。
fn retire_unreferenced_objects(conn: &Connection, active_epoch: u64, cutoff: i64) -> Result<u64> {
    let mut stmt = conn.prepare(
        "SELECT o.object_id, o.epoch, o.type FROM objects o
         WHERE o.state = 'complete'
           AND o.budget_class != 'retired'
           AND o.type BETWEEN 1 AND 4
           AND o.completed_at IS NOT NULL
           AND o.completed_at <= ?1
           AND NOT EXISTS (SELECT 1 FROM checkpoint_refs r WHERE r.object_id = o.object_id)
           AND NOT EXISTS (SELECT 1 FROM log l WHERE l.object_id = o.object_id)
           AND o.epoch NOT IN (SELECT to_epoch FROM rotations WHERE state IN ('frozen','staging'))
           AND o.epoch != ?2",
    )?;
    let rows = stmt.query_map((cutoff, i64_of(active_epoch, "epoch")?), |row| {
        Ok((
            row.get::<_, Vec<u8>>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
        ))
    })?;
    let mut collected: Vec<(Hash32, u64, i64)> = Vec::new();
    for row in rows {
        let (object_id, epoch, object_type) = row?;
        collected.push((
            object_id
                .as_slice()
                .try_into()
                .map_err(|_| Error::Invariant("objects.object_id"))?,
            u64_of(epoch, "objects.epoch")?,
            object_type,
        ));
    }
    let mut retired = 0u64;
    for (object_id, epoch, object_type) in collected {
        conn.execute(
            "DELETE FROM object_parts WHERE object_id = ?1",
            [object_id.as_slice()],
        )?;
        conn.execute(
            "UPDATE objects SET budget_class = 'retired' WHERE object_id = ?1",
            [object_id.as_slice()],
        )?;
        // ASSET 副本：登记行保留为历史事实，但不再声称密文可用。
        if object_type == ObjectType::Asset.as_i64() {
            conn.execute(
                "UPDATE asset_objects SET state = 'retired'
                 WHERE epoch = ?1 AND object_id = ?2",
                (i64_of(epoch, "epoch")?, object_id.as_slice()),
            )?;
        }
        retired += 1;
    }
    Ok(retired)
}

pub fn abort_candidates(conn: &Connection, epoch: u64) -> Result<u64> {
    let changed = conn.execute(
        "UPDATE checkpoints SET state = 'aborted'
         WHERE epoch = ?1 AND state IN ('staging','candidate')",
        [i64_of(epoch, "epoch")?],
    )?;
    u64::try_from(changed).map_err(|_| Error::Invariant("aborted checkpoints"))
}

/// 为已 promote 的 pair 写持久 GC 计划（幂等：deadline 不重置）。
pub fn schedule_plan_for_pair(
    conn: &Connection,
    now: i64,
    epoch: u64,
    cut_seq: u64,
    proof: &[u8],
) -> Result<()> {
    conn.execute(
        "INSERT INTO gc_plans(epoch, cut_seq, eligible_at, proof, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(epoch, cut_seq) DO NOTHING",
        (
            i64_of(epoch, "epoch")?,
            i64_of(cut_seq, "cut_seq")?,
            now + secs::GC_GRACE,
            proof,
            now,
        ),
    )?;
    Ok(())
}

fn pair_proof(newer: &CheckpointRow, older: &CheckpointRow) -> Vec<u8> {
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
    proof
}

/// active epoch 下已发布（且未被标 unhealthy）的 checkpoint，按 published_at 倒序。
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

/// 指定 epoch 的已发布基线（GC 计划用），按 published_at 倒序。
pub fn published_newest_first_for_epoch(
    conn: &Connection,
    epoch: u64,
    limit: u32,
) -> Result<Vec<CheckpointRow>> {
    let mut stmt = conn.prepare(
        "SELECT checkpoint_id FROM checkpoints
         WHERE state = 'published' AND health != 'unhealthy' AND epoch = ?1
         ORDER BY published_at DESC, rowid DESC LIMIT ?2",
    )?;
    let ids: Vec<Vec<u8>> = stmt
        .query_map((i64_of(epoch, "epoch")?, i64::from(limit.max(1))), |row| {
            row.get::<_, Vec<u8>>(0)
        })?
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
         WHERE epoch = ?1 AND state IN ('published','candidate') AND health != 'unhealthy'",
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
///
/// §28.1 条目 9 / 评审 §三 9：`degraded=true` 时只有 primary 一份基线可用（另一份损坏），
/// 客户端仍可用它读回，但 GC 在补齐两份健康基线前**不得前进**。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapOutcome {
    pub lease: LeaseRow,
    pub primary: CheckpointRow,
    pub fallback: Option<CheckpointRow>,
    pub degraded: bool,
    pub head_seq: u64,
    pub gc_floor: u64,
    pub epoch: u64,
}

/// 建立 bootstrap lease：没有两份有效基线 → 409 NO_VALID_BASELINE。
///
/// `allow_degraded=true` 时允许"只有一份健康基线"的降级读回（另一份损坏/缺失）。
pub fn bootstrap(
    conn: &Connection,
    clock: &dyn Clock,
    actor: &AuthContext,
    allow_degraded: bool,
) -> Result<BootstrapOutcome> {
    let device_id = actor.device_id()?;
    let vault = vault::load(conn)?;
    // 不依赖持久化的 health：每次按当前密文重算完整性，才能让"一份损坏"变成
    // `degraded` 读回而不是永远的 409（标记 unhealthy 的写会随失败事务回滚）。
    let all = published_newest_first_for_epoch(conn, vault.active_epoch, 8)?;
    let mut healthy = Vec::new();
    for checkpoint in &all {
        if checkpoint.health == Health::Unhealthy {
            continue;
        }
        if integrity_issues(conn, checkpoint)?.is_empty() {
            healthy.push(checkpoint.clone());
        } else {
            mark_health(conn, &checkpoint.checkpoint_id, Health::Unhealthy)?;
        }
    }
    let (primary, fallback, degraded) = match healthy.len() {
        0 => {
            return Err(Error::Api(
                ApiError::new(ErrorCode::NoValidBaseline).epoch(vault.active_epoch),
            ));
        }
        1 if allow_degraded => (healthy[0].clone(), None, true),
        1 => {
            return Err(Error::Api(
                ApiError::new(ErrorCode::NoValidBaseline).epoch(vault.active_epoch),
            ));
        }
        _ => (healthy[0].clone(), Some(healthy[1].clone()), false),
    };
    let now = clock.now_secs();
    let lease_floor = fallback
        .as_ref()
        .map(|checkpoint| checkpoint.covered_seq)
        .unwrap_or(primary.covered_seq);
    let lease = LeaseRow {
        lease_id: crate::ids::random_id16()?,
        device_id,
        primary_checkpoint: primary.checkpoint_id,
        // 降级时 fallback 列复用 primary（wire 不返回 fallback 条目 + degraded=true）。
        fallback_checkpoint: fallback
            .as_ref()
            .map(|checkpoint| checkpoint.checkpoint_id)
            .unwrap_or(primary.checkpoint_id),
        floor_seq: lease_floor,
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
        degraded,
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
            commit_signature: None,
            state: CheckpointState::Published,
            role: None,
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
