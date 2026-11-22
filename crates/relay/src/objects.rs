//! 对象/分片流水线：`PUT part` → `:complete` → log → 幂等 ACK（ADR 0006 §5/§6/§13.1）。
//!
//! 事务边界是硬要求：
//! - **每个 `PUT part` 是一个独立短事务**：只提交该片 + 计费，绝不跨片持锁；
//! - `complete` 是**另一个短事务**：校验既有全部分片、签名与 `object_digest`，然后**原子地**
//!   标记 complete、分配 `relay_seq`、写 log 与幂等 ACK；
//! - **片没齐禁止进 log**：log/ACK 只在 complete 事务内产生。
//!
//! relay 不解析密文（不解 AEAD/CDE 载荷），只验：信封结构（§16）、固定 header 上下文、
//! 分片齐全、密文 SHA256 聚合摘要（§2）、以及签名 hook 的结论。

use rusqlite::OptionalExtension;

use crate::auth::AuthContext;
use crate::envelope::{self, Header, ObjectType};
use crate::error::{ApiError, Error, ErrorCode, Result};
use crate::ids::{be64, sha256, Hash32, Id16};
use crate::log::{self, LogKind};
use crate::quota::{self, ReservationPurpose};
use crate::refs::ObjectRefMeta;
use crate::scope::ScopeConn;
use crate::sig::{require, Expectation, SignDomain, SignatureCheck, SignatureHook};
use crate::time::Clock;
use crate::vault;
use menghunote_sync_proto as proto;

/// dedup 作用域：对象完成。
const DEDUP_OBJECT_COMPLETE: &str = "object-complete";

/// 对象状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectState {
    Staging,
    Complete,
}

impl ObjectState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Staging => "staging",
            Self::Complete => "complete",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "staging" => Some(Self::Staging),
            "complete" => Some(Self::Complete),
            _ => None,
        }
    }
}

/// 预算归类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetClass {
    /// 正常预算 `Q`。
    Normal,
    /// 维护预算（checkpoint/轮换 reservation 覆盖）。
    Maintenance,
    /// 已 GC：分片已删，行保留为幂等 tombstone。
    Retired,
}

impl BudgetClass {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Maintenance => "maintenance",
            Self::Retired => "retired",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "normal" => Some(Self::Normal),
            "maintenance" => Some(Self::Maintenance),
            "retired" => Some(Self::Retired),
            _ => None,
        }
    }
}

/// `objects` 行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectRow {
    pub object_id: Hash32,
    pub message_id: Id16,
    /// 对象生产者；ASSET 固定为 0（vault 内共享，AAD 不含 producer，§31.1）。
    pub producer: Id16,
    pub epoch: u64,
    pub object_type: ObjectType,
    pub generation: u64,
    pub sender_seq: u64,
    pub part_count: u32,
    pub plain_len: u64,
    pub state: ObjectState,
    pub budget_class: BudgetClass,
    pub digest: Option<Hash32>,
    pub signed_commit: Option<Vec<u8>>,
    pub commit_signature: Option<Vec<u8>>,
    pub reservation_id: Option<Id16>,
    pub created_at: i64,
    pub touched_at: i64,
    pub completed_at: Option<i64>,
}

/// 分片元数据（不含密文）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PartView {
    pub part_no: u32,
    pub digest: Hash32,
    pub len: usize,
}

/// `PUT part` 结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartPutOutcome {
    pub object_id: Hash32,
    pub part_no: u32,
    pub digest: Hash32,
    /// 同字节重复上传（幂等 200，不重复计费）。
    pub duplicate: bool,
    pub object_state: ObjectState,
}

/// `:complete` 结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompleteOutcome {
    pub object_id: Hash32,
    /// 进 log 的对象（update/asset）才有 relay_seq。
    pub relay_seq: Option<u64>,
    /// 幂等重放（返回原 seq，不新分配）。
    pub duplicate: bool,
    pub state: ObjectState,
}

/// `GET objects/{o}` 的响应视图。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectMetaView {
    pub object: ObjectRow,
    pub parts: Vec<PartView>,
}

fn i64_of(value: u64, field: &'static str) -> Result<i64> {
    i64::try_from(value).map_err(|_| Error::Invariant(field))
}

fn u64_of(value: i64, field: &'static str) -> Result<u64> {
    u64::try_from(value).map_err(|_| Error::Invariant(field))
}

/// 读对象行；不存在 404。
pub fn load(conn: &ScopeConn, object_id: &Hash32) -> Result<ObjectRow> {
    load_opt(conn, object_id)?.ok_or_else(|| Error::api(ErrorCode::NotFound))
}

/// 读对象行（可不存在）。
pub fn load_opt(conn: &ScopeConn, object_id: &Hash32) -> Result<Option<ObjectRow>> {
    let mut stmt = conn.prepare(
        "SELECT object_id, message_id, producer, epoch, type, generation, sender_seq,
                part_count, plain_len, state, budget_class, digest, signed_commit,
                commit_signature, reservation_id, created_at, touched_at, completed_at
         FROM objects WHERE object_id = ?1 AND vault_id = ?",
    )?;
    let row = stmt
        .query_row((object_id.as_slice(), conn.scope().as_slice()), |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, Vec<u8>>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, i64>(7)?,
                row.get::<_, i64>(8)?,
                row.get::<_, String>(9)?,
                row.get::<_, String>(10)?,
                row.get::<_, Option<Vec<u8>>>(11)?,
                row.get::<_, Option<Vec<u8>>>(12)?,
                row.get::<_, Option<Vec<u8>>>(13)?,
                row.get::<_, Option<Vec<u8>>>(14)?,
                row.get::<_, i64>(15)?,
                row.get::<_, i64>(16)?,
                row.get::<_, Option<i64>>(17)?,
            ))
        })
        .optional()?;
    let Some((
        object_id,
        message_id,
        producer,
        epoch,
        object_type,
        generation,
        sender_seq,
        part_count,
        plain_len,
        state,
        budget_class,
        digest,
        signed_commit,
        commit_signature,
        reservation_id,
        created_at,
        touched_at,
        completed_at,
    )) = row
    else {
        return Ok(None);
    };
    Ok(Some(ObjectRow {
        object_id: object_id
            .as_slice()
            .try_into()
            .map_err(|_| Error::Invariant("objects.object_id"))?,
        message_id: message_id
            .as_slice()
            .try_into()
            .map_err(|_| Error::Invariant("objects.message_id"))?,
        producer: producer
            .as_slice()
            .try_into()
            .map_err(|_| Error::Invariant("objects.producer"))?,
        epoch: u64_of(epoch, "objects.epoch")?,
        object_type: ObjectType::from_i64(object_type).ok_or(Error::Invariant("objects.type"))?,
        generation: u64_of(generation, "objects.generation")?,
        sender_seq: u64_of(sender_seq, "objects.sender_seq")?,
        part_count: u32::try_from(part_count)
            .map_err(|_| Error::Invariant("objects.part_count"))?,
        plain_len: u64_of(plain_len, "objects.plain_len")?,
        state: ObjectState::parse(&state).ok_or(Error::Invariant("objects.state"))?,
        budget_class: BudgetClass::parse(&budget_class)
            .ok_or(Error::Invariant("objects.budget_class"))?,
        digest: match digest {
            None => None,
            Some(bytes) => Some(
                bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| Error::Invariant("objects.digest"))?,
            ),
        },
        signed_commit,
        commit_signature,
        reservation_id: match reservation_id {
            None => None,
            Some(bytes) => Some(
                bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| Error::Invariant("objects.reservation_id"))?,
            ),
        },
        created_at,
        touched_at,
        completed_at,
    }))
}

/// 读全部分片元数据（按片号升序）。
pub fn parts(conn: &ScopeConn, object_id: &Hash32) -> Result<Vec<PartView>> {
    let mut stmt = conn.prepare(
        "SELECT part_no, digest, length(envelope) FROM object_parts
         WHERE object_id = ?1 AND vault_id = ? ORDER BY part_no ASC",
    )?;
    let rows = stmt.query_map((object_id.as_slice(), conn.scope().as_slice()), |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, Vec<u8>>(1)?,
            row.get::<_, i64>(2)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (part_no, digest, len) = row?;
        out.push(PartView {
            part_no: u32::try_from(part_no).map_err(|_| Error::Invariant("part_no"))?,
            digest: digest
                .as_slice()
                .try_into()
                .map_err(|_| Error::Invariant("part digest"))?,
            len: usize::try_from(len).map_err(|_| Error::Invariant("part len"))?,
        });
    }
    Ok(out)
}

/// 单片摘要（不存在则 None）。
fn part_digest(conn: &ScopeConn, object_id: &Hash32, part_no: u32) -> Result<Option<Hash32>> {
    let mut stmt = conn.prepare(
        "SELECT digest FROM object_parts
             WHERE object_id = ?1 AND part_no = ?2 AND vault_id = ?",
    )?;
    let row = stmt
        .query_row(
            (
                object_id.as_slice(),
                i64::from(part_no),
                conn.scope().as_slice(),
            ),
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()?;
    match row {
        None => Ok(None),
        Some(bytes) => Ok(Some(
            bytes
                .as_slice()
                .try_into()
                .map_err(|_| Error::Invariant("part digest"))?,
        )),
    }
}

/// 重算 `object_digest`（缺片 → 409 INCOMPLETE）。
pub fn recompute_digest(conn: &ScopeConn, object: &ObjectRow) -> Result<Hash32> {
    let parts = parts(conn, &object.object_id)?;
    if parts.len() != object.part_count as usize {
        return Err(Error::api(ErrorCode::Incomplete));
    }
    let ordered: Vec<(u32, Hash32)> = parts.iter().map(|p| (p.part_no, p.digest)).collect();
    envelope::object_digest(object.part_count, &ordered)
}

/// 公开 ref（§2/§15）。
pub fn object_ref_meta(conn: &ScopeConn, object_id: &Hash32) -> Result<ObjectRefMeta> {
    let object = load(conn, object_id)?;
    let digest = object
        .digest
        .ok_or_else(|| Error::api(ErrorCode::Incomplete))?;
    Ok(ObjectRefMeta {
        object_id: object.object_id,
        object_type: object.object_type,
        generation: object.generation,
        key_epoch: object.epoch,
        part_count: object.part_count,
        total_plain_len: object.plain_len,
        object_digest: digest,
    })
}

/// reservation 对应的目标 epoch（checkpoint 的 epoch / rotation 的 to_epoch）。
fn reservation_target_epoch(
    conn: &ScopeConn,
    purpose: ReservationPurpose,
    resource_id: &[u8],
) -> Result<Option<u64>> {
    let value: Option<i64> = match purpose {
        ReservationPurpose::Checkpoint => conn
            .query_row(
                "SELECT epoch FROM checkpoints WHERE checkpoint_id = ?1 AND vault_id = ?",
                (resource_id, conn.scope().as_slice()),
                |row| row.get(0),
            )
            .optional()?,
        ReservationPurpose::Rotation => conn
            .query_row(
                "SELECT to_epoch FROM rotations
                 WHERE rotation_id = ?1 AND vault_id = ?",
                (resource_id, conn.scope().as_slice()),
                |row| row.get(0),
            )
            .optional()?,
    };
    match value {
        None => Ok(None),
        Some(value) => Ok(Some(u64_of(value, "reservation target epoch")?)),
    }
}

/// 当前可写的 epoch：活动 epoch，或轮换中的候选 epoch（只允许维护对象）。
fn check_writable_epoch(conn: &ScopeConn, vault: &vault::Vault, header: &Header) -> Result<bool> {
    if vault.state == vault::VaultState::Rotating {
        // 冻结期：旧 epoch（= 当前 active_epoch）一律 409；只有候选 epoch 可写（走维护预算）。
        if vault::candidate_epoch(conn)? == Some(header.key_epoch) {
            return Ok(true);
        }
        return Err(Error::Api(
            ApiError::new(ErrorCode::EpochMismatch).epoch(vault.active_epoch),
        ));
    }
    if header.key_epoch == vault.active_epoch {
        return Ok(false);
    }
    Err(Error::Api(
        ApiError::new(ErrorCode::EpochMismatch).epoch(vault.active_epoch),
    ))
}

/// 校验维护 reservation 可用于该完整 header。
fn validate_reservation(
    conn: &ScopeConn,
    now: i64,
    actor_device: &Id16,
    reservation_id: &Id16,
    header: &Header,
) -> Result<()> {
    let reservation = quota::reservation(conn, reservation_id)?
        .ok_or_else(|| Error::api(ErrorCode::ReserveExceeded))?;
    if reservation.state != quota::ReservationState::Active {
        return Err(Error::api(ErrorCode::ReserveExceeded));
    }
    if reservation.expires_at <= now {
        quota::expire_reservation(conn, reservation_id)?;
        return Err(Error::api(ErrorCode::ReserveExceeded));
    }
    if &reservation.owner_device != actor_device {
        return Err(Error::api(ErrorCode::Forbidden));
    }
    let target_epoch =
        reservation_target_epoch(conn, reservation.purpose, &reservation.resource_id)?
            .ok_or_else(|| Error::api(ErrorCode::NotFound))?;
    if target_epoch != header.key_epoch {
        return Err(Error::api(ErrorCode::EpochMismatch));
    }
    Ok(())
}

/// `PUT part` 的请求参数（打包成一个结构，避免长参数列表掩盖调用顺序）。
#[derive(Debug)]
pub struct PartUpload<'a> {
    /// 已鉴权的设备（非 ASSET 必须等于 header 的 producer；ASSET 见 §31.1 共享语义）。
    pub actor: &'a AuthContext,
    /// 路径上的 vault id（必须等于 header.vault_id）。
    pub vault_id: &'a Id16,
    /// 路径上的 object id（必须等于 header.object_id）。
    pub object_id: &'a Hash32,
    /// 路径上的分片号（必须等于 header.part_index）。
    pub part_no: u32,
    /// **原样**的 MNE1 信封字节（relay 不解密）。
    pub envelope: &'a [u8],
    /// checkpoint/轮换的维护 reservation（非空表示走 `R` 预算）。
    pub reservation: Option<&'a Id16>,
}

/// 写一个分片（**独立短事务**；调用方把它放进 `Db::write`）。
pub fn put_part(
    conn: &ScopeConn,
    clock: &dyn Clock,
    upload: &PartUpload<'_>,
) -> Result<PartPutOutcome> {
    let actor = upload.actor;
    let path_vault = upload.vault_id;
    let path_object = upload.object_id;
    let path_part = upload.part_no;
    let envelope = upload.envelope;
    let reservation = upload.reservation;
    // 1) 信封结构 + §16 固定切片（纯格式，先于任何 DB 写）。
    let header = Header::parse(envelope)?;
    if &header.vault_id != path_vault
        || &header.object_id != path_object
        || header.part_index != path_part
    {
        return Err(Error::api(ErrorCode::InvalidRequest));
    }

    // 2) vault / 设备 / epoch 屏障。
    let vault = vault::load(conn)?;
    if vault.vault_id != *path_vault {
        return Err(Error::api(ErrorCode::NotFound));
    }
    let actor_device = actor.device_id()?;
    // §31/§31.1：ASSET 的 AAD 不含 `producer_device_id`，同步客户端把 header 里的
    // producer 固定为 0（保证两设备上传同一资产的信封逐字节相同）。relay 对 ASSET
    // 接受 0 或上传者本人；其他非零且不等于 actor 的值仍 403。非 ASSET 保持严格。
    let is_asset = header.object_type == ObjectType::Asset;
    let shared_asset_producer = is_asset && header.producer_device_id == [0u8; 16];
    if header.producer_device_id != actor_device && !shared_asset_producer {
        return Err(Error::api(ErrorCode::Forbidden));
    }
    // 资产由 vault 内设备共享，不记录"上传者所有权"：对象行 producer 固定 0。
    let stored_producer = if is_asset {
        [0u8; 16]
    } else {
        header.producer_device_id
    };
    let is_candidate = check_writable_epoch(conn, &vault, &header)?;
    if is_candidate && reservation.is_none() {
        // 候选 epoch 的密文必须由轮换/checkpoint 的维护 reservation 覆盖（否则无预算归属）。
        return Err(Error::api(ErrorCode::ReserveExceeded));
    }

    let now = clock.now_secs();
    let part_hash = sha256(envelope);

    // 3) 同 object_id 的固定上下文必须完全一致，否则 409（不可混拼两个生产者/两次尝试）。
    let existing = load_opt(conn, path_object)?;
    let (budget_class, stored_reservation, state) = match &existing {
        Some(obj) => {
            // ASSET 行 producer 恒为 0；旧/兼容行（producer=上传者）仍允许同一上传者续传。
            let producer_matches = obj.producer == header.producer_device_id
                || (header.object_type == ObjectType::Asset && obj.producer == [0u8; 16]);
            let mismatch = obj.epoch != header.key_epoch
                || obj.message_id != header.message_id
                || !producer_matches
                || obj.object_type != header.object_type
                || obj.generation != header.generation
                || obj.sender_seq != header.sender_seq
                || obj.part_count != header.part_count
                || obj.plain_len != header.total_plaintext_len;
            if mismatch {
                return Err(Error::api(ErrorCode::IdConflict));
            }
            if obj.reservation_id != reservation.copied() {
                // 要么全部维护片、要么全部正常片，不允许中途换预算来源。
                return Err(Error::api(ErrorCode::IdConflict));
            }
            (obj.budget_class, obj.reservation_id, obj.state)
        }
        None => {
            let budget = match reservation {
                None => BudgetClass::Normal,
                Some(rid) => {
                    validate_reservation(conn, now, &actor_device, rid, &header)?;
                    BudgetClass::Maintenance
                }
            };
            conn.execute(
                "INSERT INTO objects(vault_id, object_id, message_id, producer, epoch, type,
                                     generation, sender_seq, part_count, plain_len, state,
                                     budget_class, digest, signed_commit, reservation_id,
                                     created_at, touched_at, completed_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'staging', ?11, NULL, NULL, ?12,
                         ?13, ?13, NULL)",
                (
                    conn.scope().as_slice(),
                    header.object_id.as_slice(),
                    header.message_id.as_slice(),
                    stored_producer.as_slice(),
                    i64_of(header.key_epoch, "key_epoch")?,
                    header.object_type.as_i64(),
                    i64_of(header.generation, "generation")?,
                    i64_of(header.sender_seq, "sender_seq")?,
                    i64::from(header.part_count),
                    i64_of(header.total_plaintext_len, "plain_len")?,
                    budget.as_str(),
                    reservation.map(|id| id.as_slice()),
                    now,
                ),
            )?;
            (budget, reservation.copied(), ObjectState::Staging)
        }
    };

    // 4) 重复分片：同字节幂等 200（不重复计费），不同字节 409。
    if let Some(existing_digest) = part_digest(conn, path_object, path_part)? {
        if existing_digest == part_hash {
            return Ok(PartPutOutcome {
                object_id: header.object_id,
                part_no: path_part,
                digest: part_hash,
                duplicate: true,
                object_state: state,
            });
        }
        return Err(Error::api(ErrorCode::PartConflict));
    }
    if state == ObjectState::Complete {
        // 已完成对象不可再补片（内容不同）；幂等重放走上面的同字节分支。
        return Err(Error::api(ErrorCode::IdConflict));
    }

    // 5) 计费 + 落片必须同一事务（逐片原子扣减，§13.2）。
    let bytes = i64::try_from(envelope.len()).map_err(|_| Error::Invariant("envelope len"))?;
    match budget_class {
        BudgetClass::Normal => quota::charge_normal(conn, bytes)?,
        BudgetClass::Maintenance => {
            let rid = stored_reservation
                .ok_or(Error::Invariant("maintenance object without reservation"))?;
            quota::charge_maintenance(conn, now, &rid, bytes)?;
        }
        BudgetClass::Retired => return Err(Error::api(ErrorCode::Retired)),
    }
    conn.execute(
        "INSERT INTO object_parts(vault_id, object_id, part_no, envelope, digest)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        (
            conn.scope().as_slice(),
            header.object_id.as_slice(),
            i64::from(path_part),
            envelope,
            part_hash.as_slice(),
        ),
    )?;
    conn.execute(
        "UPDATE objects SET touched_at = ?1 WHERE object_id = ?2 AND vault_id = ?",
        (now, header.object_id.as_slice(), conn.scope().as_slice()),
    )?;
    Ok(PartPutOutcome {
        object_id: header.object_id,
        part_no: path_part,
        digest: part_hash,
        duplicate: false,
        object_state: ObjectState::Staging,
    })
}

/// `:complete` 的请求参数。
#[derive(Debug)]
pub struct CompleteUpload<'a> {
    /// 已鉴权的设备（非 ASSET 必须等于对象 producer；ASSET 由该 actor 的 key 签名）。
    pub actor: &'a AuthContext,
    /// 路径上的 vault id。
    pub vault_id: &'a Id16,
    /// 路径上的 object id。
    pub object_id: &'a Hash32,
    /// `ObjectCommit` 的 CDE 字节（被签名载荷）。
    pub commit: &'a [u8],
    /// 设备对 commit 的 Ed25519 签名。
    pub signature: &'a [u8],
}

/// 完成对象：校验全部分片 + 签名 + digest，然后**原子**标 complete / 分配 seq / 写 log / 幂等 ACK。
pub fn complete(
    conn: &ScopeConn,
    clock: &dyn Clock,
    hook: &dyn SignatureHook,
    upload: &CompleteUpload<'_>,
) -> Result<CompleteOutcome> {
    let actor = upload.actor;
    let path_vault = upload.vault_id;
    let object_id = upload.object_id;
    let commit = upload.commit;
    let signature = upload.signature;
    let vault = vault::load(conn)?;
    if vault.vault_id != *path_vault {
        return Err(Error::api(ErrorCode::NotFound));
    }
    // 一片都没上传时对象行还不存在：语义上就是 INCOMPLETE（而不是 404，避免暴露"上传进度"）。
    let object = load_opt(conn, object_id)?.ok_or_else(|| Error::api(ErrorCode::Incomplete))?;
    let actor_device = actor.device_id()?;
    // §31.1：ASSET 是 vault 内共享对象（producer=0）；complete 允许任一有效成员设备
    // 提交，但签名仍必须由**该 actor** 的有效设备 key 给出（见下方 signer 选择）。
    let shared_asset = object.object_type == ObjectType::Asset && object.producer == [0u8; 16];
    if !shared_asset && actor_device != object.producer {
        return Err(Error::api(ErrorCode::Forbidden));
    }
    let signer = if shared_asset {
        actor_device
    } else {
        object.producer
    };

    // 幂等：同 commit 字节 → 返回原 ACK（即使响应丢了也能重试）。
    let request_hash = sha256(commit);
    if let Some((stored_hash, stored_seq)) = dedup_lookup(conn, DEDUP_OBJECT_COMPLETE, object_id)? {
        if stored_hash == request_hash {
            return Ok(CompleteOutcome {
                object_id: *object_id,
                relay_seq: stored_seq,
                duplicate: true,
                state: object.state,
            });
        }
        // 同 object 不同 commit：不可变，409。
        return Err(Error::api(ErrorCode::IdConflict));
    }
    if object.state == ObjectState::Complete {
        // complete 事务是原子的：有 complete 行就一定有 dedup tombstone。走到这里说明内部不变量坏了。
        return Err(Error::Invariant("completed object without dedup tombstone"));
    }
    // epoch 屏障：活动 epoch，或轮换中的候选 epoch（候选对象不进 log，避免污染冻结的日志）。
    // 冻结期旧 epoch 连 complete 也不再接受：`S` 之后不得再产生旧 epoch 日志/ACK。
    let is_candidate = object.epoch != vault.active_epoch;
    if is_candidate {
        let allowed = vault.state == vault::VaultState::Rotating
            && vault::candidate_epoch(conn)? == Some(object.epoch);
        if !allowed {
            return Err(Error::Api(
                ApiError::new(ErrorCode::EpochMismatch).epoch(vault.active_epoch),
            ));
        }
    } else if vault.state == vault::VaultState::Rotating {
        return Err(Error::Api(
            ApiError::new(ErrorCode::EpochMismatch).epoch(vault.active_epoch),
        ));
    }

    // 全部分片必须齐（否则 409，绝不能进 log）。
    let digest = recompute_digest(conn, &object)?;
    let public_key = vault::device_sign_pk(conn, &signer)?
        .ok_or_else(|| Error::Api(ApiError::new(ErrorCode::Revoked).epoch(vault.active_epoch)))?;
    let expectation = crate::sig::ObjectCommitExpectation {
        object_id,
        object_type: object.object_type,
        generation: object.generation,
        key_epoch: object.epoch,
        sender_seq: object.sender_seq,
        part_count: object.part_count,
        plain_len: object.plain_len,
        computed_digest: &digest,
    };
    let check = SignatureCheck {
        domain: SignDomain::ObjectCommit,
        vault_id: &vault.vault_id,
        device_id: Some(&signer),
        public_key: &public_key,
        message: commit,
        signature,
        expect: &Expectation::ObjectCommit {
            object: &expectation,
        },
    };
    // hook 同时负责"载荷内 origin_seq == header.sender_seq / doc generation 一致"等
    // 只有 CDE 解析才能校验的字段（`sync-proto` 落地后实现）。
    require(hook, &check, ErrorCode::SignatureInvalid)?;

    let now = clock.now_secs();
    let log_kind = if is_candidate {
        // 候选 epoch 的任何对象都不进旧日志（日志在 activate 后才切换 epoch）。
        None
    } else {
        match object.object_type {
            ObjectType::Update => Some(LogKind::Update),
            ObjectType::Asset => Some(LogKind::Asset),
            // 非事件流对象（快照/manifest/槽）不进 log：它们只通过 checkpoint refs 被引用。
            ObjectType::Snapshot | ObjectType::Manifest | ObjectType::KeySlot => None,
        }
    };
    let relay_seq = match log_kind {
        Some(kind) => Some(log::append(
            conn,
            now,
            kind,
            Some(object_id),
            object.epoch,
            commit,
            Some(signature),
        )?),
        None => None,
    };
    conn.execute(
        "UPDATE objects SET state = 'complete', digest = ?1, signed_commit = ?2,
                            completed_at = ?3, touched_at = ?3
         WHERE object_id = ?4 AND vault_id = ?",
        (
            digest.as_slice(),
            commit,
            now,
            object_id.as_slice(),
            conn.scope().as_slice(),
        ),
    )?;
    // §25.1：ASSET complete 同事务写不透明登记表（log 只做唤醒；GC 后枚举仍完整）。
    if object.object_type == ObjectType::Asset {
        let rotation_id: Option<Vec<u8>> = if is_candidate {
            conn.query_row(
                "SELECT rotation_id FROM rotations
                 WHERE to_epoch = ?1 AND state IN ('frozen','staging') AND vault_id = ?
                 LIMIT 1",
                (i64_of(object.epoch, "epoch")?, conn.scope().as_slice()),
                |row| row.get(0),
            )
            .optional()?
        } else {
            None
        };
        conn.execute(
            "INSERT INTO asset_objects(vault_id, epoch, object_id, state, finalized_seq,
                                       rotation_id)
             VALUES (?1, ?2, ?3, 'active', ?4, ?5)
             ON CONFLICT(vault_id, epoch, object_id) DO NOTHING",
            (
                conn.scope().as_slice(),
                i64_of(object.epoch, "epoch")?,
                object_id.as_slice(),
                i64::try_from(relay_seq.unwrap_or(0)).map_err(|_| Error::Invariant("relay_seq"))?,
                rotation_id.as_deref(),
            ),
        )?;
    }
    let response = match relay_seq {
        Some(seq) => be64(seq).to_vec(),
        None => Vec::new(),
    };
    dedup_put(
        conn,
        now,
        DEDUP_OBJECT_COMPLETE,
        object_id.as_slice(),
        &request_hash,
        relay_seq,
        &response,
    )?;
    Ok(CompleteOutcome {
        object_id: *object_id,
        relay_seq,
        duplicate: false,
        state: ObjectState::Complete,
    })
}

/// 读分片密文（`ETag` 用 digest 的十六进制）。
pub fn read_part(conn: &ScopeConn, object_id: &Hash32, part_no: u32) -> Result<(Vec<u8>, Hash32)> {
    let object = load(conn, object_id)?;
    if object.budget_class == BudgetClass::Retired {
        return Err(Error::api(ErrorCode::Retired));
    }
    let mut stmt = conn.prepare(
        "SELECT envelope, digest FROM object_parts
         WHERE object_id = ?1 AND part_no = ?2 AND vault_id = ?",
    )?;
    let row = stmt
        .query_row(
            (
                object_id.as_slice(),
                i64::from(part_no),
                conn.scope().as_slice(),
            ),
            |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?)),
        )
        .optional()?;
    let Some((envelope, digest)) = row else {
        return Err(Error::api(ErrorCode::NotFound));
    };
    let digest: Hash32 = digest
        .as_slice()
        .try_into()
        .map_err(|_| Error::Invariant("part digest"))?;
    Ok((envelope, digest))
}

/// `GET objects/{o}`：不可变元数据 + 各片 digest（已上传 bitmap 由 parts 列表隐含）。
pub fn meta(conn: &ScopeConn, object_id: &Hash32) -> Result<ObjectMetaView> {
    Ok(ObjectMetaView {
        object: load(conn, object_id)?,
        parts: parts(conn, object_id)?,
    })
}

/// §13.3 的服务端可校验项：分片齐全、每片密文 SHA256 与列一致、聚合 digest 与记录一致、
/// 每片固定 header 上下文与对象行一致。
///
/// 返回**不含密文**的问题列表（空 = 健康）。这是 GC 删除前的前置门禁。
pub fn integrity_issues(conn: &ScopeConn, object: &ObjectRow) -> Result<Vec<&'static str>> {
    let mut issues = Vec::new();
    if object.state != ObjectState::Complete {
        issues.push("object_not_complete");
        return Ok(issues);
    }
    let Some(recorded_digest) = object.digest else {
        issues.push("object_digest_missing");
        return Ok(issues);
    };
    let mut stmt = conn.prepare(
        "SELECT part_no, envelope, digest FROM object_parts
         WHERE object_id = ?1 AND vault_id = ?
         ORDER BY part_no ASC",
    )?;
    let rows = stmt.query_map(
        (object.object_id.as_slice(), conn.scope().as_slice()),
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, Vec<u8>>(2)?,
            ))
        },
    )?;
    let mut digests: Vec<(u32, Hash32)> = Vec::new();
    for row in rows {
        let (part_no, envelope, digest) = row?;
        let part_no = match u32::try_from(part_no) {
            Ok(value) => value,
            Err(_) => {
                issues.push("part_no_out_of_range");
                continue;
            }
        };
        if part_no != digests.len() as u32 {
            issues.push("part_numbering_gap");
        }
        let stored: Hash32 = match digest.as_slice().try_into() {
            Ok(value) => value,
            Err(_) => {
                issues.push("part_digest_size");
                continue;
            }
        };
        if sha256(&envelope) != stored {
            issues.push("part_ciphertext_digest_mismatch");
        }
        match Header::parse(&envelope) {
            Ok(header) => {
                // §31.1：ASSET 的 producer 不在 AAD 内，确定性 header 为 0；不参与
                // 完整性判定（envelope 字节本身已由 part/object digest 绑定）。
                let producer_consistent = object.object_type == ObjectType::Asset
                    || header.producer_device_id == object.producer;
                let consistent = header.object_id == object.object_id
                    && header.key_epoch == object.epoch
                    && header.message_id == object.message_id
                    && producer_consistent
                    && header.object_type == object.object_type
                    && header.generation == object.generation
                    && header.sender_seq == object.sender_seq
                    && header.part_count == object.part_count
                    && header.part_index == part_no
                    && header.total_plaintext_len == object.plain_len;
                if !consistent {
                    issues.push("part_header_mismatch");
                }
            }
            Err(_) => issues.push("part_envelope_invalid"),
        }
        digests.push((part_no, stored));
    }
    if digests.len() != object.part_count as usize {
        issues.push("parts_missing");
        return Ok(issues);
    }
    match envelope::object_digest(object.part_count, &digests) {
        Ok(recomputed) if recomputed == recorded_digest => {}
        Ok(_) => issues.push("object_digest_mismatch"),
        Err(_) => issues.push("object_digest_uncomputable"),
    }
    Ok(issues)
}

/// §25.1/§34：把 `asset_objects JOIN objects` 的行转成 `ObjectRefV1`（按 object_id 升序）。
fn collect_asset_refs<T>(rows: T) -> Result<Vec<proto::ObjectRefV1>>
where
    T: Iterator<Item = rusqlite::Result<(Vec<u8>, i64, i64, i64, i64, i64, Vec<u8>)>>,
{
    let mut refs = Vec::new();
    for row in rows {
        let (object_id, object_type, generation, object_epoch, part_count, plain_len, digest) =
            row?;
        refs.push(proto::ObjectRefV1 {
            object_id: object_id
                .as_slice()
                .try_into()
                .map_err(|_| Error::Invariant("objects.object_id"))?,
            object_type: proto::ObjectType::try_from(
                u8::try_from(object_type).map_err(|_| Error::Invariant("objects.type"))?,
            )
            .map_err(|_| Error::Invariant("objects.type"))?,
            generation: u64_of(generation, "objects.generation")?,
            key_epoch: u64_of(object_epoch, "objects.epoch")?,
            part_count: u32::try_from(part_count).map_err(|_| Error::Invariant("part_count"))?,
            total_plain_len: u64_of(plain_len, "objects.plain_len")?,
            object_digest: digest
                .as_slice()
                .try_into()
                .map_err(|_| Error::Invariant("objects.digest"))?,
        });
    }
    Ok(refs)
}

/// §25.1：冻结时刻的资产集合摘要：`SHA256(CDE(sorted ObjectRefV1[ASSET]))`。
/// 只使用不透明登记 + ObjectRefV1 元数据；真实文件 hash/MIME/长度不在 relay。
pub fn asset_set_digest(conn: &ScopeConn, epoch: u64, through_seq: u64) -> Result<Hash32> {
    let mut stmt = conn.prepare(
        "SELECT o.object_id, o.type, o.generation, o.epoch, o.part_count, o.plain_len, o.digest
         FROM asset_objects a JOIN objects o ON o.object_id = a.object_id
         WHERE a.epoch = ?1 AND a.state = 'active' AND a.finalized_seq <= ?2
           AND o.state = 'complete' AND o.digest IS NOT NULL
           AND a.vault_id = ?3 AND o.vault_id = ?3
         ORDER BY o.object_id ASC",
    )?;
    let rows = stmt.query_map(
        (
            i64_of(epoch, "epoch")?,
            i64_of(through_seq, "through_seq")?,
            conn.scope().as_slice(),
        ),
        |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, Vec<u8>>(6)?,
            ))
        },
    )?;
    let refs = collect_asset_refs(rows)?;
    let input =
        proto::refs_digest_input(&refs).map_err(|_| Error::Invariant("asset_set_digest input"))?;
    Ok(sha256(&input))
}

/// §34：候选 epoch（重加密）资产集合摘要：`SHA256(CDE(sorted ObjectRefV1[ASSET]))`。
///
/// 与冻结时的 [`asset_set_digest`] 同口径，但只取本次 rotation 在候选 epoch 下已
/// complete 的资产（`asset_objects.rotation_id` 绑定），供 activation T 的
/// `new_asset_set_digest` 与 relay DB 事实逐项比对。
pub fn candidate_asset_set_digest(
    conn: &ScopeConn,
    epoch: u64,
    rotation_id: &Id16,
) -> Result<Hash32> {
    let mut stmt = conn.prepare(
        "SELECT o.object_id, o.type, o.generation, o.epoch, o.part_count, o.plain_len, o.digest
         FROM asset_objects a JOIN objects o ON o.object_id = a.object_id
         WHERE a.epoch = ?1 AND a.state = 'active' AND a.rotation_id = ?2
           AND o.state = 'complete' AND o.digest IS NOT NULL
           AND a.vault_id = ?3 AND o.vault_id = ?3
         ORDER BY o.object_id ASC",
    )?;
    let rows = stmt.query_map(
        (
            i64_of(epoch, "epoch")?,
            rotation_id.as_slice(),
            conn.scope().as_slice(),
        ),
        |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, Vec<u8>>(6)?,
            ))
        },
    )?;
    let refs = collect_asset_refs(rows)?;
    let input = proto::refs_digest_input(&refs)
        .map_err(|_| Error::Invariant("candidate_asset_set_digest input"))?;
    Ok(sha256(&input))
}

/// §25.1：某 epoch 在 `through_seq` 之前已 finalize 的资产数量（opaque 登记表口径）。
pub fn asset_count(conn: &ScopeConn, epoch: u64, through_seq: u64) -> Result<u64> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM asset_objects a JOIN objects o ON o.object_id = a.object_id
         WHERE a.epoch = ?1 AND a.state = 'active' AND a.finalized_seq <= ?2
           AND o.state = 'complete' AND o.digest IS NOT NULL
           AND a.vault_id = ?3 AND o.vault_id = ?3",
        (
            i64_of(epoch, "epoch")?,
            i64_of(through_seq, "through_seq")?,
            conn.scope().as_slice(),
        ),
        |row| row.get(0),
    )?;
    u64_of(count, "asset count")
}

/// 分页列出某 epoch 在 `through_seq` 之前完成的资产（轮换复制基线用）。
pub fn list_assets(
    conn: &ScopeConn,
    epoch: u64,
    through_seq: u64,
    after_object_id: Option<&Hash32>,
    limit: u32,
) -> Result<Vec<ObjectRefMeta>> {
    // §25.1：枚举只查不透明登记表，不依赖可回收的 log（log GC 后仍完整）。
    let mut stmt = conn.prepare(
        "SELECT o.object_id, o.type, o.generation, o.epoch, o.part_count, o.plain_len, o.digest
         FROM asset_objects a JOIN objects o ON o.object_id = a.object_id
         WHERE a.epoch = ?1 AND a.state = 'active' AND a.finalized_seq <= ?2
           AND o.state = 'complete' AND o.digest IS NOT NULL
           AND (?3 IS NULL OR o.object_id > ?3)
           AND a.vault_id = ?5 AND o.vault_id = ?5
         ORDER BY o.object_id ASC LIMIT ?4",
    )?;
    let rows = stmt.query_map(
        (
            i64_of(epoch, "epoch")?,
            i64_of(through_seq, "through_seq")?,
            after_object_id.map(|id| id.as_slice()),
            i64::from(limit.clamp(1, 1024)),
            conn.scope().as_slice(),
        ),
        |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, Vec<u8>>(6)?,
            ))
        },
    )?;
    let mut out = Vec::new();
    for row in rows {
        let (object_id, object_type, generation, epoch, part_count, plain_len, digest) = row?;
        out.push(ObjectRefMeta {
            object_id: object_id
                .as_slice()
                .try_into()
                .map_err(|_| Error::Invariant("objects.object_id"))?,
            object_type: ObjectType::from_i64(object_type)
                .ok_or(Error::Invariant("objects.type"))?,
            generation: u64_of(generation, "objects.generation")?,
            key_epoch: u64_of(epoch, "objects.epoch")?,
            part_count: u32::try_from(part_count).map_err(|_| Error::Invariant("part_count"))?,
            total_plain_len: u64_of(plain_len, "objects.plain_len")?,
            object_digest: digest
                .as_slice()
                .try_into()
                .map_err(|_| Error::Invariant("objects.digest"))?,
        });
    }
    Ok(out)
}

/// GC：删除从未 finalize 的暂存对象（ADR §7：24h 可删）与它们的密文。
pub fn cleanup_staging(conn: &ScopeConn, older_than: i64) -> Result<u64> {
    let staged: Vec<Vec<u8>> = {
        let mut stmt = conn.prepare(
            "SELECT object_id FROM objects
             WHERE state = 'staging' AND touched_at < ?1 AND vault_id = ?",
        )?;
        let rows = stmt.query_map((older_than, conn.scope().as_slice()), |row| {
            row.get::<_, Vec<u8>>(0)
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut deleted = 0u64;
    for object_id in staged {
        conn.execute(
            "DELETE FROM object_parts WHERE object_id = ?1 AND vault_id = ?",
            (object_id.as_slice(), conn.scope().as_slice()),
        )?;
        conn.execute(
            "DELETE FROM objects WHERE object_id = ?1 AND vault_id = ?",
            (object_id.as_slice(), conn.scope().as_slice()),
        )?;
        deleted += 1;
    }
    Ok(deleted)
}

// ---------- dedup ----------

fn dedup_lookup(conn: &ScopeConn, scope: &str, id: &[u8]) -> Result<Option<(Hash32, Option<u64>)>> {
    let mut stmt = conn.prepare(
        "SELECT request_hash, accepted_seq FROM dedup
         WHERE scope = ?1 AND id = ?2 AND vault_id = ?3",
    )?;
    let row = stmt
        .query_row((scope, id, conn.scope().as_slice()), |row| {
            Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Option<i64>>(1)?))
        })
        .optional()?;
    let Some((request_hash, accepted_seq)) = row else {
        return Ok(None);
    };
    let accepted_seq = match accepted_seq {
        None => None,
        Some(value) => Some(u64_of(value, "dedup.accepted_seq")?),
    };
    Ok(Some((
        request_hash
            .as_slice()
            .try_into()
            .map_err(|_| Error::Invariant("dedup.request_hash"))?,
        accepted_seq,
    )))
}

fn dedup_put(
    conn: &ScopeConn,
    now: i64,
    scope: &str,
    id: &[u8],
    request_hash: &Hash32,
    accepted_seq: Option<u64>,
    response: &[u8],
) -> Result<()> {
    conn.execute(
        "INSERT INTO dedup(vault_id, scope, id, request_hash, accepted_seq, response, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        (
            conn.scope().as_slice(),
            scope,
            id,
            request_hash.as_slice(),
            match accepted_seq {
                None => None,
                Some(seq) => Some(i64_of(seq, "accepted_seq")?),
            },
            response,
            now,
        ),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enums_roundtrip() {
        assert_eq!(ObjectState::parse("staging"), Some(ObjectState::Staging));
        assert_eq!(BudgetClass::parse("retired"), Some(BudgetClass::Retired));
        assert_eq!(BudgetClass::parse("other"), None);
        assert_eq!(ObjectState::Complete.as_str(), "complete");
    }
}
