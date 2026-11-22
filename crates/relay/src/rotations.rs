//! key epoch 轮换状态机：freeze → 候选 epoch → staged grants → atomic activate
//! （ADR 0006 §8 + §13.2）。
//!
//! 屏障语义（§9 epoch 屏障）：
//! - `BEGIN` 事务里完成"吊销 + 失效旧 token + 固定旧 epoch 截止 S + 进入 ROTATING"，
//!   因此 freeze 之前接受的写全部 `<= S`，freeze 之后任何旧 epoch 追加一律 409；
//! - 候选根/epoch **不自动回退**：abort 只是"废弃候选"，吊销过的旧 epoch 不会被解冻继续写；
//!   要继续只能新分配更高 epoch 重做。
//! - staged grants 在 activate 前对普通成员不可读（查询只认 `active_epoch`）。

use rusqlite::{Connection, OptionalExtension};

use crate::auth::{self, AuthContext, ManagementProof};
use crate::checkpoints;
use crate::control::{
    self, ControlKind, EpochActivated, RevocationsAdded, RotationRestarted, SignedControlRecord,
};
use crate::error::{ApiError, Error, ErrorCode, Result};
use crate::ids::{sha256, Hash32, Id16};
use crate::log::{self, LogKind};
use crate::objects;
use crate::sig::{
    require, EpochGrantInput, Expectation, SignDomain, SignatureCheck, SignatureHook,
};
use crate::time::{secs, Clock};
use crate::vault;
use crate::wire;
use menghunote_sync_proto as proto;

/// 轮换状态（ADR §6 `rotations.state`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RotationState {
    /// 旧 epoch 已冻结，等候选基线与 grants。
    Frozen,
    /// 候选材料齐了（两基线 + 全成员 grant），等 activate。
    Staging,
    Activated,
    Aborted,
}

impl RotationState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Frozen => "frozen",
            Self::Staging => "staging",
            Self::Activated => "activated",
            Self::Aborted => "aborted",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "frozen" => Some(Self::Frozen),
            "staging" => Some(Self::Staging),
            "activated" => Some(Self::Activated),
            "aborted" => Some(Self::Aborted),
            _ => None,
        }
    }
}

/// `rotations` 行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RotationRow {
    pub rotation_id: Id16,
    pub from_epoch: u64,
    pub to_epoch: u64,
    pub freeze_seq: u64,
    pub member_set_hash: Option<Hash32>,
    /// §25.1：freeze 时刻 active epoch 的不透明资产集合摘要（ObjectRefV1[ASSET]）。
    pub asset_set_digest: Option<Hash32>,
    pub revoked_count: u64,
    pub state: RotationState,
    /// §28.1 条目 3：activate 指名并 promote 的两个候选 cid（激活前 NULL）。
    pub primary_checkpoint: Option<Id16>,
    pub fallback_checkpoint: Option<Id16>,
    /// 激活 commit（CDE `ActivationCommitV1` 的 SHA256；激活前 NULL）。
    pub activation_commit: Option<Hash32>,
    pub activated_at: Option<i64>,
    /// 激活事件的控制链 rev（wire 的 activated commit "seq"）。
    pub activated_control_rev: Option<u64>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// `GET rotations/{r}` 视图。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RotationView {
    pub rotation: RotationRow,
    pub grants_count: u64,
    /// 已 staged grants 的集合摘要（staged HPKE 头 + 收件设备）。
    pub grants_digest: Hash32,
    /// 候选 epoch 已发布的 checkpoint 数量。
    pub published_checkpoints: u64,
}

/// begin 请求。
#[derive(Debug)]
pub struct BeginRequest<'a> {
    pub revoke: &'a [Id16],
    pub expected_epoch: u64,
    /// 管理签名里承诺的 control_rev（CAS）。
    pub expected_control_rev: u64,
    pub request_id: Id16,
    pub path: &'a str,
    pub body_sha256: Hash32,
    pub signed: &'a [u8],
    pub signature: &'a [u8],
}

/// 一个 HPKE grant（管理签名覆盖 AAD || enc || SHA256(ciphertext)，AAD 的 CDE 由 hook 校验）。
#[derive(Debug)]
pub struct GrantInput<'a> {
    /// 客户端生成的 grant id（进入 AAD，防跨 grant 重放）。
    pub grant_id: Id16,
    pub hpke_enc: [u8; 32],
    pub hpke_ciphertext: &'a [u8],
    pub signed_header: &'a [u8],
    pub signature: &'a [u8],
    pub manager_rev: u64,
}

/// 管理公钥转换（旧/新管理 key 双签记录）。
#[derive(Debug)]
pub struct NewManager {
    /// §27.2 的 `ManagerTransitionV1` CDE 字节（绑定 vault/instance/rotation/epoch/revs/pubkey/S）。
    pub transition: Vec<u8>,
    pub old_signature: Option<Vec<u8>>,
    pub new_signature: Option<Vec<u8>>,
}

/// §27.2：管理权转换记录（CDE 整数键 map，旧/新管理 key 双签同一份字节）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagerTransition {
    pub vault_id: Id16,
    pub instance_id: Id16,
    pub rotation_id: Id16,
    pub from_epoch: u64,
    pub to_epoch: u64,
    pub old_rev: u64,
    pub new_rev: u64,
    pub new_pubkey: [u8; 32],
    /// 冻结截止 S（必须等于 `rotation.freeze_seq`）。
    pub covered_seq: u64,
}

impl ManagerTransition {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let fields = vec![
            (0, wire::bytes(self.vault_id)),
            (1, wire::bytes(self.instance_id)),
            (2, wire::bytes(self.rotation_id)),
            (3, wire::uint(self.from_epoch)),
            (4, wire::uint(self.to_epoch)),
            (5, wire::uint(self.old_rev)),
            (6, wire::uint(self.new_rev)),
            (7, wire::bytes(self.new_pubkey)),
            (8, wire::uint(self.covered_seq)),
        ];
        proto::encode_cde(&wire::map(fields))
            .map_err(|_| Error::Invariant("manager transition encode"))
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let value = proto::decode_cde(bytes).map_err(|_| Error::api(ErrorCode::InvalidRequest))?;
        let fields = wire::value_map(&value)?;
        if fields.len() != 9 {
            return Err(Error::api(ErrorCode::InvalidRequest));
        }
        let id16 = |key: u64| -> Result<Id16> {
            wire::bytes_field(&fields, key)?
                .as_slice()
                .try_into()
                .map_err(|_| Error::api(ErrorCode::InvalidRequest))
        };
        Ok(Self {
            vault_id: id16(0)?,
            instance_id: id16(1)?,
            rotation_id: id16(2)?,
            from_epoch: wire::uint_field(&fields, 3)?,
            to_epoch: wire::uint_field(&fields, 4)?,
            old_rev: wire::uint_field(&fields, 5)?,
            new_rev: wire::uint_field(&fields, 6)?,
            new_pubkey: wire::key32_field(&fields, 7)?,
            covered_seq: wire::uint_field(&fields, 8)?,
        })
    }

    /// 绑定到本次 activate 的事实；任一不符 → 409。
    fn bind(&self, vault: &vault::Vault, rotation: &RotationRow, rotation_id: &Id16) -> Result<()> {
        if self.vault_id != vault.vault_id
            || self.instance_id != vault.instance_id
            || self.rotation_id != *rotation_id
            || self.from_epoch != rotation.from_epoch
            || self.to_epoch != rotation.to_epoch
            || self.old_rev != vault.manager_rev
            || self.new_rev != vault.manager_rev + 1
            || self.covered_seq != rotation.freeze_seq
        {
            return Err(Error::api(ErrorCode::CasConflict));
        }
        Ok(())
    }
}

/// activate 请求。
#[derive(Debug)]
pub struct ActivateRequest<'a> {
    pub request_id: Id16,
    pub path: &'a str,
    pub body_sha256: Hash32,
    pub expected_control_rev: u64,
    pub signed: &'a [u8],
    pub signature: &'a [u8],
    pub new_manager: Option<NewManager>,
    /// §28.1 条目 3 / 评审 §三 6：activate 必须**指名**要 promote 的两个候选 cid。
    ///
    /// task-99 先接入 canonical body（字段只从被签名 body 解析）；独立性校验与
    /// "只 promote 被指名的两个 cid" 在 task-100 生效。
    pub primary_checkpoint: Option<Id16>,
    pub fallback_checkpoint: Option<Id16>,
    /// §28.1 条目 6：activate 必须锁定整套材料摘要（expected CAS）。
    pub member_cert_digest: Option<Hash32>,
    pub grants_digest: Option<Hash32>,
    pub slot_set_digest: Option<Hash32>,
    pub asset_set_digest: Option<Hash32>,
}

/// activate 结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivateOutcome {
    pub epoch: u64,
    pub control_rev: u64,
    pub auth_rev: u64,
    pub member_set_hash: Hash32,
    /// §28.1 条目 3/6：被指名的两个 cid 与精确激活 commit。
    pub primary_checkpoint: Id16,
    pub fallback_checkpoint: Id16,
    pub activation_commit: Hash32,
}

/// 读 rotation 行。
pub fn get(conn: &Connection, rotation_id: &Id16) -> Result<RotationRow> {
    let mut stmt = conn.prepare(
        "SELECT rotation_id, from_epoch, to_epoch, freeze_seq, member_set_hash, asset_set_digest,
                revoked_count, state, created_at, updated_at,
                primary_checkpoint, fallback_checkpoint, activation_commit, activated_at,
                activated_control_rev
         FROM rotations WHERE rotation_id = ?1",
    )?;
    let row = stmt
        .query_row([rotation_id.as_slice()], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, Option<Vec<u8>>>(4)?,
                row.get::<_, Option<Vec<u8>>>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, i64>(8)?,
                row.get::<_, i64>(9)?,
                row.get::<_, Option<Vec<u8>>>(10)?,
                row.get::<_, Option<Vec<u8>>>(11)?,
                row.get::<_, Option<Vec<u8>>>(12)?,
                row.get::<_, Option<i64>>(13)?,
                row.get::<_, Option<i64>>(14)?,
            ))
        })
        .optional()?
        .ok_or_else(|| Error::api(ErrorCode::NotFound))?;
    Ok(RotationRow {
        rotation_id: row
            .0
            .as_slice()
            .try_into()
            .map_err(|_| Error::Invariant("rotation_id"))?,
        from_epoch: u64::try_from(row.1).map_err(|_| Error::Invariant("from_epoch"))?,
        to_epoch: u64::try_from(row.2).map_err(|_| Error::Invariant("to_epoch"))?,
        freeze_seq: u64::try_from(row.3).map_err(|_| Error::Invariant("freeze_seq"))?,
        asset_set_digest: row
            .5
            .map(|bytes| {
                bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| Error::Invariant("asset_set_digest"))
            })
            .transpose()?,
        member_set_hash: match row.4 {
            None => None,
            Some(bytes) => Some(
                bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| Error::Invariant("member_set_hash"))?,
            ),
        },
        revoked_count: u64::try_from(row.6).map_err(|_| Error::Invariant("revoked_count"))?,
        state: RotationState::parse(&row.7).ok_or(Error::Invariant("rotations.state"))?,
        primary_checkpoint: match row.10 {
            None => None,
            Some(bytes) => Some(
                bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| Error::Invariant("primary_checkpoint"))?,
            ),
        },
        fallback_checkpoint: match row.11 {
            None => None,
            Some(bytes) => Some(
                bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| Error::Invariant("fallback_checkpoint"))?,
            ),
        },
        activation_commit: match row.12 {
            None => None,
            Some(bytes) => Some(
                bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| Error::Invariant("activation_commit"))?,
            ),
        },
        activated_at: row.13,
        activated_control_rev: match row.14 {
            None => None,
            Some(value) => {
                Some(u64::try_from(value).map_err(|_| Error::Invariant("activated_control_rev"))?)
            }
        },
        created_at: row.8,
        updated_at: row.9,
    })
}

/// 最新一条 rotation。
pub fn latest(conn: &Connection) -> Result<Option<RotationRow>> {
    let mut stmt = conn.prepare(
        "SELECT rotation_id FROM rotations ORDER BY created_at DESC, rowid DESC LIMIT 1",
    )?;
    let row = stmt
        .query_row([], |row| row.get::<_, Vec<u8>>(0))
        .optional()?;
    match row {
        None => Ok(None),
        Some(bytes) => {
            let id: Id16 = bytes
                .as_slice()
                .try_into()
                .map_err(|_| Error::Invariant("rotation_id"))?;
            Ok(Some(get(conn, &id)?))
        }
    }
}

/// 候选 epoch 对应的 rotation id（轮换维护 reservation 以它为 resource_id）。
pub fn rotation_id_for_epoch(conn: &Connection, epoch: u64) -> Result<Option<Id16>> {
    let value: Option<Vec<u8>> = conn
        .query_row(
            "SELECT rotation_id FROM rotations
             WHERE to_epoch = ?1 AND state IN ('frozen','staging') LIMIT 1",
            [i64::try_from(epoch).map_err(|_| Error::Invariant("epoch"))?],
            |row| row.get(0),
        )
        .optional()?;
    match value {
        None => Ok(None),
        Some(bytes) => Ok(Some(
            bytes
                .as_slice()
                .try_into()
                .map_err(|_| Error::Invariant("rotation_id"))?,
        )),
    }
}

/// 候选 epoch 的冻结水位（checkpoint 创建时校验 `head_seq >= freeze_seq`）。
pub fn freeze_seq_for_epoch(conn: &Connection, epoch: u64) -> Result<Option<u64>> {
    let value: Option<i64> = conn
        .query_row(
            "SELECT freeze_seq FROM rotations
             WHERE to_epoch = ?1 AND state IN ('frozen','staging') LIMIT 1",
            [i64::try_from(epoch).map_err(|_| Error::Invariant("epoch"))?],
            |row| row.get(0),
        )
        .optional()?;
    match value {
        None => Ok(None),
        Some(value) => Ok(Some(
            u64::try_from(value).map_err(|_| Error::Invariant("freeze_seq"))?,
        )),
    }
}

/// 某 epoch 的 grants 集合摘要：`(数量, SHA256(concat(device_id || SHA256(header))))`。
///
/// 只覆盖 staged/最新 grant 的签名头，不含密文；与 activate 的 `ActivationCommitV1`
/// 和 rotation view 使用同一口径（客户端可复算）。
pub fn grants_digest(conn: &Connection, epoch: u64) -> Result<(u64, Hash32)> {
    let mut stmt = conn.prepare(
        "SELECT device_id, signed_header FROM device_grants WHERE epoch = ?1 ORDER BY device_id ASC",
    )?;
    let rows = stmt.query_map(
        [i64::try_from(epoch).map_err(|_| Error::Invariant("epoch"))?],
        |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?)),
    )?;
    let mut buf = Vec::new();
    let mut grants_count = 0u64;
    for row in rows {
        let (device_id, signed_header) = row?;
        buf.extend_from_slice(&device_id);
        buf.extend_from_slice(&sha256(&signed_header));
        grants_count += 1;
    }
    Ok((grants_count, sha256(&buf)))
}

/// 追加一条 typed `SignedControlRecord`（控制链永久保留，不在密文 GC 范围内）。
#[allow(clippy::too_many_arguments)]
fn append_control_record(
    conn: &Connection,
    now: i64,
    kind: ControlKind,
    request_id: Id16,
    manager_rev: u64,
    expected_control_rev: u64,
    signed_body: &[u8],
    signature: &[u8],
    event: Option<Vec<u8>>,
) -> Result<u64> {
    control::append(
        conn,
        now,
        &SignedControlRecord {
            kind,
            request_id,
            manager_rev,
            expected_control_rev,
            signed_body: signed_body.to_vec(),
            signature: signature.to_vec(),
            event,
        },
    )
}

/// 成员证书集合摘要：按 device_id 升序 `SHA256(concat(device_id || certificate ||
/// certificate_signature?))`。activate 必须锁定它（证书集合不能在 activate 时被掉包）。
pub fn member_cert_digest(conn: &Connection) -> Result<Hash32> {
    let mut stmt = conn.prepare(
        "SELECT device_id, certificate, certificate_signature FROM devices
         WHERE status = 'active' ORDER BY device_id ASC",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, Vec<u8>>(0)?,
            row.get::<_, Vec<u8>>(1)?,
            row.get::<_, Option<Vec<u8>>>(2)?,
        ))
    })?;
    let mut buf = Vec::new();
    for row in rows {
        let (device_id, certificate, certificate_signature) = row?;
        buf.extend_from_slice(&device_id);
        buf.extend_from_slice(&certificate);
        if let Some(signature) = certificate_signature {
            buf.extend_from_slice(&signature);
        }
    }
    Ok(sha256(&buf))
}

/// 候选 epoch 的槽集合摘要：按 slot_id 升序 `SHA256(concat(slot_id || SHA256(descriptor)))`。
pub fn slot_set_digest(conn: &Connection, epoch: u64) -> Result<Hash32> {
    let mut stmt = conn.prepare(
        "SELECT slot_id, descriptor FROM key_slots
         WHERE epoch = ?1 AND state = 'staging' ORDER BY slot_id ASC",
    )?;
    let rows = stmt.query_map(
        [i64::try_from(epoch).map_err(|_| Error::Invariant("epoch"))?],
        |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?)),
    )?;
    let mut buf = Vec::new();
    for row in rows {
        let (slot_id, descriptor) = row?;
        buf.extend_from_slice(&slot_id);
        buf.extend_from_slice(&sha256(&descriptor));
    }
    Ok(sha256(&buf))
}

/// 视图（含 staged grants 摘要）。
pub fn view(conn: &Connection, rotation_id: &Id16) -> Result<RotationView> {
    let rotation = get(conn, rotation_id)?;
    let (grants_count, grants_digest) = grants_digest(conn, rotation.to_epoch)?;
    let published = checkpoints::published_for_epoch(conn, rotation.to_epoch)?;
    Ok(RotationView {
        rotation,
        grants_count,
        grants_digest,
        published_checkpoints: published.0,
    })
}

/// §28.1 条目 3/5：激活 commit（relay 侧 CDE 记录；客户端据此比对"激活了什么"）。
///
/// 字段编号（键从 0 连续）：
/// 0 vault_id, 1 instance_id, 2 rotation_id, 3 from_epoch, 4 to_epoch, 5 covered_seq,
/// 6 primary_checkpoint, 7 fallback_checkpoint, 8 primary_covered_seq,
/// 9 fallback_covered_seq, 10 primary_manifest, 11 fallback_manifest,
/// 12 member_set_hash, 13 grants_digest, 14 asset_set_digest?, 15 control_rev。
///
/// 说明：放在 relay 而不是 `sync-proto`——本任务禁止私改 `sync-proto`；协议化需 Lead 分派。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivationCommit {
    pub vault_id: Id16,
    pub instance_id: Id16,
    pub rotation_id: Id16,
    pub from_epoch: u64,
    pub to_epoch: u64,
    pub covered_seq: u64,
    pub primary_checkpoint: Id16,
    pub fallback_checkpoint: Id16,
    pub primary_covered_seq: u64,
    pub fallback_covered_seq: u64,
    pub primary_manifest: Hash32,
    pub fallback_manifest: Hash32,
    pub member_set_hash: Hash32,
    pub grants_digest: Hash32,
    pub asset_set_digest: Option<Hash32>,
    pub control_rev: u64,
}

impl ActivationCommit {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut fields = vec![
            (0, wire::bytes(self.vault_id)),
            (1, wire::bytes(self.instance_id)),
            (2, wire::bytes(self.rotation_id)),
            (3, wire::uint(self.from_epoch)),
            (4, wire::uint(self.to_epoch)),
            (5, wire::uint(self.covered_seq)),
            (6, wire::bytes(self.primary_checkpoint)),
            (7, wire::bytes(self.fallback_checkpoint)),
            (8, wire::uint(self.primary_covered_seq)),
            (9, wire::uint(self.fallback_covered_seq)),
            (10, wire::bytes(self.primary_manifest)),
            (11, wire::bytes(self.fallback_manifest)),
            (12, wire::bytes(self.member_set_hash)),
            (13, wire::bytes(self.grants_digest)),
            (15, wire::uint(self.control_rev)),
        ];
        if let Some(digest) = self.asset_set_digest {
            fields.push((14, wire::bytes(digest)));
        }
        proto::encode_cde(&wire::map(fields))
            .map_err(|_| Error::Invariant("activation commit encode"))
    }

    /// 激活 commit hash（wire 的 `activation_commit`）。
    pub fn digest(&self) -> Result<Hash32> {
        Ok(sha256(&self.encode()?))
    }
}

/// 开始轮换（**一个事务内完成吊销/冻结/候选 epoch/控制记录**）。
pub fn begin(
    conn: &Connection,
    clock: &dyn Clock,
    hook: &dyn SignatureHook,
    actor: &AuthContext,
    request: &BeginRequest<'_>,
) -> Result<RotationView> {
    actor.require_management()?;
    let vault = vault::load(conn)?;
    if request.expected_epoch != vault.active_epoch {
        return Err(Error::Api(
            ApiError::new(ErrorCode::CasConflict).epoch(vault.active_epoch),
        ));
    }
    if vault.state == vault::VaultState::Rotating {
        // 只允许"上一个候选已经废弃"后重新开始；否则请 resume 现有 rotation。
        match latest(conn)? {
            Some(rotation) if rotation.state == RotationState::Aborted => {}
            _ => return Err(Error::api(ErrorCode::InvalidState)),
        }
    }
    auth::require_management(
        conn,
        hook,
        &ManagementProof {
            method_tag: 2, // POST
            path: request.path,
            request_id: request.request_id,
            expected_control_rev: request.expected_control_rev,
            signature: request.signature,
        },
        request.signed,
    )?;

    let now = clock.now_secs();
    let rotation_id = crate::ids::random_id16()?;
    let to_epoch = vault::allocate_epoch(conn, now)?;
    let freeze_seq = vault.head_seq;
    // §25.1：冻结时刻的资产集合摘要（opaque ObjectRefV1；候选复制以此为准）。
    let asset_set_digest = objects::asset_set_digest(conn, vault.active_epoch, freeze_seq)?;

    // §24.1①：freeze 本身必须让全部旧 token 失效（即使 revoke 为空），客户端必须重新认证。
    vault::bump_auth_rev(conn)?;
    // 立即吊销（§8 步 2）。每吊销一台会再推进 auth_rev（同上，全部 token 失效）。
    let mut revoked_count = 0u64;
    for device_id in request.revoke {
        if auth::revoke_device(conn, now, device_id)? {
            revoked_count += 1;
        }
    }
    // §23.1：该旧 epoch 冻结期间发生过吊销 → 永久事实，abort 绝不能解冻。
    if revoked_count > 0 {
        vault::mark_epoch_revoked_once(conn, vault.active_epoch)?;
    }
    conn.execute(
        "INSERT INTO rotations(rotation_id, from_epoch, to_epoch, freeze_seq, member_set_hash,
                               asset_set_digest, revoked_count, state, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?6, 'frozen', ?7, ?7)",
        (
            rotation_id.as_slice(),
            i64::try_from(vault.active_epoch).map_err(|_| Error::Invariant("epoch"))?,
            i64::try_from(to_epoch).map_err(|_| Error::Invariant("epoch"))?,
            i64::try_from(freeze_seq).map_err(|_| Error::Invariant("freeze_seq"))?,
            asset_set_digest.as_slice(),
            i64::try_from(revoked_count).map_err(|_| Error::Invariant("revoked_count"))?,
            now,
        ),
    )?;
    vault::set_state(conn, vault::VaultState::Rotating)?;
    // 控制链记录本次 freeze/吊销（typed SignedControlRecord：含 kind/request_id/管理签名）。
    append_control_record(
        conn,
        now,
        ControlKind::RotationBegin,
        request.request_id,
        vault.manager_rev,
        request.expected_control_rev,
        request.signed,
        request.signature,
        None,
    )?;
    view(conn, &rotation_id)
}

/// staged 一份 HPKE grant（activate 前不可被普通成员读取）。
pub fn stage_grant(
    conn: &Connection,
    hook: &dyn SignatureHook,
    actor: &AuthContext,
    rotation_id: &Id16,
    recipient: &Id16,
    input: &GrantInput<'_>,
    clock: &dyn Clock,
) -> Result<RotationView> {
    actor.require_management()?;
    let vault = vault::load(conn)?;
    let rotation = get(conn, rotation_id)?;
    if !matches!(
        rotation.state,
        RotationState::Frozen | RotationState::Staging
    ) {
        return Err(Error::api(ErrorCode::InvalidState));
    }
    // 收件人必须是当前未吊销成员；吊销了就是 RECIPIENT_MISMATCH/REVOKED。
    if !auth::device_active(conn, recipient)? {
        return Err(Error::api(ErrorCode::Revoked));
    }
    if input.manager_rev != vault.manager_rev {
        return Err(Error::api(ErrorCode::CasConflict));
    }
    // §24.2：缺外层签名直接拒绝（不能用 Expectation::None 只验一个自造 header）。
    if input.signature.is_empty() {
        return Err(Error::api(ErrorCode::ManagementRequired));
    }
    let manager_pub =
        vault::manager_pubkey(conn)?.ok_or_else(|| Error::api(ErrorCode::ManagementRequired))?;
    // 收件人 X25519 公钥只认 relay 的 devices 行，不信任客户端传入。
    let hpke_pk: Vec<u8> = conn.query_row(
        "SELECT hpke_pk FROM devices WHERE device_id = ?1",
        [recipient.as_slice()],
        |row| row.get(0),
    )?;
    let recipient_x25519_pub: [u8; 32] = hpke_pk
        .as_slice()
        .try_into()
        .map_err(|_| Error::Invariant("devices.hpke_pk"))?;
    let ciphertext_sha256 = sha256(input.hpke_ciphertext);
    let grant = EpochGrantInput {
        epoch: rotation.to_epoch,
        recipient_device_id: recipient,
        recipient_x25519_pub: &recipient_x25519_pub,
        manager_rev: input.manager_rev,
        grant_id: &input.grant_id,
        hpke_enc: &input.hpke_enc,
        ciphertext_sha256: &ciphertext_sha256,
    };
    let check = SignatureCheck {
        domain: SignDomain::DeviceApprove,
        vault_id: &vault.vault_id,
        device_id: Some(recipient),
        public_key: &manager_pub,
        message: input.signed_header,
        signature: input.signature,
        expect: &Expectation::EpochGrant { grant: &grant },
    };
    require(hook, &check, ErrorCode::ManagementRequired)?;

    type StoredGrant = (Vec<u8>, Vec<u8>, Option<Vec<u8>>, Option<Vec<u8>>);
    let existing: Option<StoredGrant> = conn
        .query_row(
            "SELECT hpke_ciphertext, signed_header, signature, grant_id FROM device_grants
             WHERE epoch = ?1 AND device_id = ?2",
            (
                i64::try_from(rotation.to_epoch).map_err(|_| Error::Invariant("epoch"))?,
                recipient.as_slice(),
            ),
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
    match existing {
        Some((ciphertext, header, signature, existing_grant))
            if ciphertext == input.hpke_ciphertext
                && header == input.signed_header
                && signature.as_deref() == Some(input.signature)
                && existing_grant.as_deref() == Some(input.grant_id.as_slice()) =>
        {
            // 幂等重放。
        }
        Some(_) => return Err(Error::api(ErrorCode::CasConflict)),
        None => {
            conn.execute(
                "INSERT INTO device_grants(epoch, device_id, manager_rev, hpke_enc, hpke_ciphertext,
                                           signed_header, signature, grant_id, activation_ref)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                (
                    i64::try_from(rotation.to_epoch).map_err(|_| Error::Invariant("epoch"))?,
                    recipient.as_slice(),
                    i64::try_from(input.manager_rev).map_err(|_| Error::Invariant("manager_rev"))?,
                    input.hpke_enc.as_slice(),
                    input.hpke_ciphertext,
                    input.signed_header,
                    input.signature,
                    input.grant_id.as_slice(),
                    rotation_id.as_slice(),
                ),
            )?;
        }
    }
    // 材料齐了就进入 staging；否则保持 frozen（等更多 grant/基线）。
    maybe_advance_to_staging(conn, &rotation, clock.now_secs())?;
    view(conn, rotation_id)
}

fn maybe_advance_to_staging(conn: &Connection, rotation: &RotationRow, now: i64) -> Result<()> {
    if rotation.state != RotationState::Frozen {
        return Ok(());
    }
    let (published, min_covered) = checkpoints::published_for_epoch(conn, rotation.to_epoch)?;
    if published < 2 || min_covered < rotation.freeze_seq {
        return Ok(());
    }
    let missing: i64 = conn.query_row(
        "SELECT COUNT(*) FROM devices d
         WHERE d.status = 'active'
           AND NOT EXISTS (SELECT 1 FROM device_grants g
                           WHERE g.epoch = ?1 AND g.device_id = d.device_id)",
        [i64::try_from(rotation.to_epoch).map_err(|_| Error::Invariant("epoch"))?],
        |row| row.get(0),
    )?;
    if missing == 0 {
        conn.execute(
            "UPDATE rotations SET state = 'staging', updated_at = ?1
             WHERE rotation_id = ?2 AND state = 'frozen'",
            (now, rotation.rotation_id.as_slice()),
        )?;
    }
    Ok(())
}

/// 成员集合摘要：排序后的 active device_id 的 SHA256。
pub fn member_set_hash(conn: &Connection) -> Result<Hash32> {
    let mut stmt = conn
        .prepare("SELECT device_id FROM devices WHERE status = 'active' ORDER BY device_id ASC")?;
    let ids: Vec<Vec<u8>> = stmt
        .query_map([], |row| row.get::<_, Vec<u8>>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut buf = Vec::with_capacity(ids.len() * 16);
    for id in ids {
        buf.extend_from_slice(&id);
    }
    Ok(sha256(&buf))
}

/// 原子激活：发布新槽、切 active_epoch、解冻新 epoch、控制链 + 事件。
pub fn activate(
    conn: &Connection,
    clock: &dyn Clock,
    hook: &dyn SignatureHook,
    actor: &AuthContext,
    rotation_id: &Id16,
    request: &ActivateRequest<'_>,
) -> Result<ActivateOutcome> {
    actor.require_management()?;
    let vault = vault::load(conn)?;
    let rotation = get(conn, rotation_id)?;
    if !matches!(
        rotation.state,
        RotationState::Frozen | RotationState::Staging
    ) {
        // 已 aborted 的候选 epoch 绝不能激活（§8 步 8：不得复用已泄露的候选根）。
        return Err(Error::api(ErrorCode::InvalidState));
    }
    if vault::epoch_state(conn, rotation.to_epoch)? != Some(vault::EpochState::Staged) {
        return Err(Error::api(ErrorCode::InvalidState));
    }
    auth::require_management(
        conn,
        hook,
        &ManagementProof {
            method_tag: 2, // POST
            path: request.path,
            request_id: request.request_id,
            expected_control_rev: request.expected_control_rev,
            signature: request.signature,
        },
        request.signed,
    )?;

    // §28.1 条目 2–3：activate **只** promote 被指名的两个 cid（并校验独立性）。
    // 未指名的候选一律不收口（缺 cid → 409 INCOMPLETE，客户端必须重新准备）。
    let now = clock.now_secs();
    let primary_id = request
        .primary_checkpoint
        .ok_or_else(|| Error::api(ErrorCode::Incomplete))?;
    let fallback_id = request
        .fallback_checkpoint
        .ok_or_else(|| Error::api(ErrorCode::Incomplete))?;
    let pair = checkpoints::promote_named_candidates(
        conn,
        rotation.to_epoch,
        &primary_id,
        &fallback_id,
        rotation.freeze_seq,
        now,
    )?;
    checkpoints::schedule_plan_for_pair(conn, now, rotation.to_epoch, pair.cut_seq, &pair.proof)?;
    // 全成员 grant（管理签名控制链里的成员集合，不信任裸 relay 公钥列表）。
    let missing: i64 = conn.query_row(
        "SELECT COUNT(*) FROM devices d
         WHERE d.status = 'active'
           AND NOT EXISTS (SELECT 1 FROM device_grants g
                           WHERE g.epoch = ?1 AND g.device_id = d.device_id)",
        [i64::try_from(rotation.to_epoch).map_err(|_| Error::Invariant("epoch"))?],
        |row| row.get(0),
    )?;
    if missing != 0 {
        return Err(Error::api(ErrorCode::Incomplete));
    }
    // §8 步 5：activate 事务必须发布**新槽**（password + recovery 的 staging 各至少一个）。
    let slot_issues = crate::slots::rotation_slots_ready(conn, rotation.to_epoch)?;
    if !slot_issues.is_empty() {
        return Err(Error::api(ErrorCode::Incomplete));
    }
    // §28.1 条目 6：activate 必须锁定整套材料的摘要（证书集合/槽集合/grants/assets）。
    let actual_member = member_cert_digest(conn)?;
    let (_, actual_grants) = grants_digest(conn, rotation.to_epoch)?;
    let actual_slots = slot_set_digest(conn, rotation.to_epoch)?;
    let actual_assets = rotation.asset_set_digest;
    let expected = [
        (
            request.member_cert_digest,
            Some(actual_member),
            "member_cert_digest",
        ),
        (request.grants_digest, Some(actual_grants), "grants_digest"),
        (
            request.slot_set_digest,
            Some(actual_slots),
            "slot_set_digest",
        ),
        (request.asset_set_digest, actual_assets, "asset_set_digest"),
    ];
    for (declared, actual, _label) in expected {
        if declared != actual {
            return Err(Error::api(ErrorCode::CasConflict));
        }
    }

    // 管理公钥转换（旧/新双签在 `transition` 里；这里只做 rev 连续性与发布）。
    // §27.2：v1 每次轮换必须携带管理权转换（不允许传 None 绕过新管理权要求）。
    let new_manager = request
        .new_manager
        .as_ref()
        .ok_or_else(|| Error::api(ErrorCode::CasConflict))?;
    let transition = ManagerTransition::decode(&new_manager.transition)?;
    transition.bind(&vault, &rotation, rotation_id)?;
    let old_pub =
        vault::manager_pubkey(conn)?.ok_or_else(|| Error::api(ErrorCode::ManagementRequired))?;
    verify_manager_transition(
        hook,
        &vault.vault_id,
        &old_pub,
        &transition.new_pubkey,
        &new_manager.transition,
        new_manager.old_signature.as_deref(),
        new_manager.new_signature.as_deref(),
    )?;
    conn.execute(
        "INSERT INTO manager_keys(rev, pubkey, transition, old_rev, old_signature,
                                  new_signature, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        (
            i64::try_from(transition.new_rev).map_err(|_| Error::Invariant("manager rev"))?,
            transition.new_pubkey.as_slice(),
            new_manager.transition.as_slice(),
            i64::try_from(transition.old_rev).map_err(|_| Error::Invariant("manager rev"))?,
            new_manager.old_signature.as_deref(),
            new_manager.new_signature.as_deref(),
            now,
        ),
    )?;
    conn.execute(
        "UPDATE vault SET manager_rev = ?1 WHERE singleton = 1",
        [i64::try_from(transition.new_rev).map_err(|_| Error::Invariant("manager rev"))?],
    )?;

    // key slot：旧 active 进入 retiring（14 天），候选 epoch 的 staging 槽上线。
    conn.execute(
        "UPDATE key_slots SET state = 'retiring', retire_at = ?1 WHERE state = 'active'",
        [now + secs::OLD_SLOT_RETENTION],
    )?;
    conn.execute(
        "UPDATE key_slots SET state = 'active', slot_set_rev = slot_set_rev + 1
         WHERE state = 'staging' AND epoch = ?1",
        [i64::try_from(rotation.to_epoch).map_err(|_| Error::Invariant("epoch"))?],
    )?;

    // epoch 切换（同一事务：旧 retired、新 active）。
    vault::set_epoch_state(conn, rotation.from_epoch, vault::EpochState::Retired)?;
    vault::set_epoch_retire_at(conn, rotation.from_epoch, now + secs::OLD_SLOT_RETENTION)?;
    vault::set_epoch_state(conn, rotation.to_epoch, vault::EpochState::Active)?;

    let member_set_hash = member_set_hash(conn)?;
    conn.execute(
        "UPDATE rotations SET state = 'activated', member_set_hash = ?1, updated_at = ?2
         WHERE rotation_id = ?3",
        (member_set_hash.as_slice(), now, rotation_id.as_slice()),
    )?;
    conn.execute(
        "UPDATE vault SET active_epoch = ?1, state = 'active' WHERE singleton = 1",
        [i64::try_from(rotation.to_epoch).map_err(|_| Error::Invariant("epoch"))?],
    )?;
    // 激活后立即让全部旧 token 失效：客户端必须重新认证并重新取 grant。
    let auth_rev = vault::bump_auth_rev(conn)?;
    // §28.1 条目 3/5/10：激活事实 + typed `EpochActivated` 控制事件（不是裸 signed payload）。
    let (_, grants_digest) = grants_digest(conn, rotation.to_epoch)?;
    let control_rev = vault.control_rev + 1;
    let activation = ActivationCommit {
        vault_id: vault.vault_id,
        instance_id: vault.instance_id,
        rotation_id: *rotation_id,
        from_epoch: rotation.from_epoch,
        to_epoch: rotation.to_epoch,
        covered_seq: rotation.freeze_seq,
        primary_checkpoint: pair.primary.checkpoint_id,
        fallback_checkpoint: pair.fallback.checkpoint_id,
        primary_covered_seq: pair.primary.covered_seq,
        fallback_covered_seq: pair.fallback.covered_seq,
        primary_manifest: pair
            .primary
            .manifest_oid
            .ok_or(Error::Invariant("primary manifest"))?,
        fallback_manifest: pair
            .fallback
            .manifest_oid
            .ok_or(Error::Invariant("fallback manifest"))?,
        member_set_hash,
        grants_digest,
        asset_set_digest: rotation.asset_set_digest,
        control_rev,
    };
    let activation_commit = activation.digest()?;
    let event = EpochActivated {
        vault_id: vault.vault_id,
        instance_id: vault.instance_id,
        rotation_id: *rotation_id,
        from_epoch: rotation.from_epoch,
        to_epoch: rotation.to_epoch,
        covered_seq: rotation.freeze_seq,
        primary_checkpoint: pair.primary.checkpoint_id,
        fallback_checkpoint: pair.fallback.checkpoint_id,
        activation_commit,
        member_set_hash,
        grants_digest,
        control_rev,
        auth_rev,
    }
    .encode()?;
    let rev = append_control_record(
        conn,
        now,
        ControlKind::EpochActivated,
        request.request_id,
        transition.old_rev,
        request.expected_control_rev,
        request.signed,
        request.signature,
        Some(event.clone()),
    )?;
    if rev != control_rev {
        return Err(Error::Invariant("activation control rev mismatch"));
    }
    conn.execute(
        "UPDATE rotations SET primary_checkpoint = ?1, fallback_checkpoint = ?2,
                             activation_commit = ?3, activated_at = ?4,
                             activated_control_rev = ?5
         WHERE rotation_id = ?6",
        (
            pair.primary.checkpoint_id.as_slice(),
            pair.fallback.checkpoint_id.as_slice(),
            activation_commit.as_slice(),
            now,
            i64::try_from(control_rev).map_err(|_| Error::Invariant("control rev"))?,
            rotation_id.as_slice(),
        ),
    )?;
    // log 的 control 事件也带 typed `EpochActivated`（客户端从日志首读即可识别）。
    log::append(
        conn,
        now,
        LogKind::Control,
        None,
        rotation.to_epoch,
        &event,
        Some(request.signature),
    )?;
    // 轮换 reservation 完成（剩余维护额度归还；已写数据保留）。
    conn.execute(
        "UPDATE maintenance_reservations SET state = 'complete'
         WHERE purpose = 'rotation' AND resource_id = ?1 AND state = 'active'",
        [rotation_id.as_slice()],
    )?;
    Ok(ActivateOutcome {
        epoch: rotation.to_epoch,
        control_rev,
        auth_rev,
        member_set_hash,
        primary_checkpoint: pair.primary.checkpoint_id,
        fallback_checkpoint: pair.fallback.checkpoint_id,
        activation_commit,
    })
}

/// 废弃候选 epoch：删 staged grants、标 aborted；**吊销过的旧 epoch 不解冻**。
pub fn abort(
    conn: &Connection,
    clock: &dyn Clock,
    hook: &dyn SignatureHook,
    actor: &AuthContext,
    rotation_id: &Id16,
    request: &AbortRequest<'_>,
) -> Result<RotationView> {
    actor.require_management()?;
    let vault = vault::load(conn)?;
    let rotation = get(conn, rotation_id)?;
    if !matches!(
        rotation.state,
        RotationState::Frozen | RotationState::Staging
    ) {
        return Err(Error::api(ErrorCode::InvalidState));
    }
    auth::require_management(
        conn,
        hook,
        &ManagementProof {
            method_tag: 2, // POST
            path: request.path,
            request_id: request.request_id,
            expected_control_rev: request.expected_control_rev,
            signature: request.signature,
        },
        request.signed,
    )?;
    let now = clock.now_secs();
    // 废弃候选 epoch 与它的全部 staged grants（根已可能泄露 → 永不复用该 epoch）。
    conn.execute(
        "DELETE FROM device_grants WHERE epoch = ?1",
        [i64::try_from(rotation.to_epoch).map_err(|_| Error::Invariant("epoch"))?],
    )?;
    // §24.1：候选 checkpoint 一并作废（永不 bootstrap/参与 GC，object/cid 不复用）。
    checkpoints::abort_candidates(conn, rotation.to_epoch)?;
    vault::set_epoch_state(conn, rotation.to_epoch, vault::EpochState::Aborted)?;
    conn.execute(
        "UPDATE rotations SET state = 'aborted', updated_at = ?1 WHERE rotation_id = ?2",
        (now, rotation_id.as_slice()),
    )?;
    conn.execute(
        "UPDATE maintenance_reservations SET state = 'expired'
         WHERE purpose = 'rotation' AND resource_id = ?1 AND state = 'active'",
        [rotation_id.as_slice()],
    )?;
    // 关键：吊销过设备的轮换失败后**不能**回旧 epoch 继续写（§10）；只有未吊销才能解冻。
    // §23.1：只看目标旧 epoch 的持久事实，不看"本次 rotation 的 revoked_count"；
    // 带过任何吊销的 epoch 永不解冻（只能 activate 新 epoch 或显式恢复）。
    if !vault::epoch_revoked_once(conn, rotation.from_epoch)? {
        vault::set_state(conn, vault::VaultState::Active)?;
    }
    append_control_record(
        conn,
        now,
        ControlKind::RotationAbort,
        request.request_id,
        vault.manager_rev,
        request.expected_control_rev,
        request.signed,
        request.signature,
        None,
    )?;
    let _ = hook;
    let _ = vault;
    view(conn, rotation_id)
}

/// 控制面请求（`restart` / `revoke-more` 与 abort 同构）。
#[derive(Debug)]
pub struct ControlRequest<'a> {
    pub request_id: Id16,
    pub path: &'a str,
    pub body_sha256: Hash32,
    pub expected_control_rev: u64,
    pub signed: &'a [u8],
    pub signature: &'a [u8],
}

/// `revoke-more` 结果。
#[derive(Debug, Clone)]
pub struct RevokeMoreOutcome {
    pub view: RotationView,
    pub restarted: bool,
    pub revoked: u64,
}

/// §8 步 8 / 评审 §二 resume：废弃当前候选 epoch 并分配更高 epoch（保持冻结）。
///
/// **原子**：删 staged grants、作废候选 checkpoint、旧候选 epoch 标 aborted、旧 rotation
/// 标 aborted、写新 rotation 行（沿用 from_epoch/freeze_seq，state=frozen），旧轮换
/// reservation 归还。返回新 rotation 行；vault 保持 `rotating`（旧写不因失败解冻）。
fn discard_candidate_and_allocate(
    conn: &Connection,
    now: i64,
    rotation: &RotationRow,
) -> Result<(Id16, u64)> {
    let to_epoch = rotation.to_epoch;
    let to_epoch_i64 = i64::try_from(to_epoch).map_err(|_| Error::Invariant("epoch"))?;
    conn.execute("DELETE FROM device_grants WHERE epoch = ?1", [to_epoch_i64])?;
    checkpoints::abort_candidates(conn, to_epoch)?;
    vault::set_epoch_state(conn, to_epoch, vault::EpochState::Aborted)?;
    conn.execute(
        "UPDATE rotations SET state = 'aborted', updated_at = ?1
         WHERE rotation_id = ?2 AND state IN ('frozen','staging')",
        (now, rotation.rotation_id.as_slice()),
    )?;
    let new_to_epoch = vault::allocate_epoch(conn, now)?;
    let new_rotation_id = crate::ids::random_id16()?;
    conn.execute(
        "INSERT INTO rotations(rotation_id, from_epoch, to_epoch, freeze_seq, member_set_hash,
                               asset_set_digest, revoked_count, state, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, NULL, ?5, 0, 'frozen', ?6, ?6)",
        (
            new_rotation_id.as_slice(),
            i64::try_from(rotation.from_epoch).map_err(|_| Error::Invariant("from_epoch"))?,
            i64::try_from(new_to_epoch).map_err(|_| Error::Invariant("to_epoch"))?,
            i64::try_from(rotation.freeze_seq).map_err(|_| Error::Invariant("freeze_seq"))?,
            rotation
                .asset_set_digest
                .as_ref()
                .map(|digest| digest.as_slice()),
            now,
        ),
    )?;
    conn.execute(
        "UPDATE maintenance_reservations SET state = 'expired'
         WHERE purpose = 'rotation' AND resource_id = ?1 AND state = 'active'",
        [rotation.rotation_id.as_slice()],
    )?;
    Ok((new_rotation_id, new_to_epoch))
}

/// `POST rotations/{r}/restart`：原子重启候选 epoch（更高 epoch、fresh 候选根），保持冻结。
pub fn restart(
    conn: &Connection,
    clock: &dyn Clock,
    hook: &dyn SignatureHook,
    actor: &AuthContext,
    rotation_id: &Id16,
    request: &ControlRequest<'_>,
) -> Result<RotationView> {
    actor.require_management()?;
    let vault = vault::load(conn)?;
    if vault.state != vault::VaultState::Rotating {
        return Err(Error::api(ErrorCode::InvalidState));
    }
    let rotation = get(conn, rotation_id)?;
    if !matches!(
        rotation.state,
        RotationState::Frozen | RotationState::Staging
    ) {
        return Err(Error::api(ErrorCode::InvalidState));
    }
    auth::require_management(
        conn,
        hook,
        &ManagementProof {
            method_tag: 2, // POST
            path: request.path,
            request_id: request.request_id,
            expected_control_rev: request.expected_control_rev,
            signature: request.signature,
        },
        request.signed,
    )?;
    let now = clock.now_secs();
    let (new_rotation_id, new_to_epoch) = discard_candidate_and_allocate(conn, now, &rotation)?;
    let event = RotationRestarted {
        old_rotation: rotation.rotation_id,
        new_rotation: new_rotation_id,
        from_epoch: rotation.from_epoch,
        old_to_epoch: rotation.to_epoch,
        new_to_epoch,
        freeze_seq: rotation.freeze_seq,
    }
    .encode()?;
    append_control_record(
        conn,
        now,
        ControlKind::RotationRestarted,
        request.request_id,
        vault.manager_rev,
        request.expected_control_rev,
        request.signed,
        request.signature,
        Some(event),
    )?;
    // 保持 ROTATING：绝不能因为 restart 解冻旧 epoch。
    view(conn, &new_rotation_id)
}

/// `POST rotations/{r}/revoke-more`：轮换期间追加吊销，保持冻结；若候选 grant/材料已为
/// 被吊销设备生成，则按 §8 步 8 **原子废弃候选 + 分配更高 epoch**。
pub fn revoke_more(
    conn: &Connection,
    clock: &dyn Clock,
    hook: &dyn SignatureHook,
    actor: &AuthContext,
    rotation_id: &Id16,
    request: &ControlRequest<'_>,
    revoke: &[Id16],
) -> Result<RevokeMoreOutcome> {
    actor.require_management()?;
    if revoke.is_empty() {
        return Err(Error::api(ErrorCode::InvalidRequest));
    }
    let vault = vault::load(conn)?;
    if vault.state != vault::VaultState::Rotating {
        return Err(Error::api(ErrorCode::InvalidState));
    }
    let rotation = get(conn, rotation_id)?;
    if !matches!(
        rotation.state,
        RotationState::Frozen | RotationState::Staging
    ) {
        return Err(Error::api(ErrorCode::InvalidState));
    }
    auth::require_management(
        conn,
        hook,
        &ManagementProof {
            method_tag: 2, // POST
            path: request.path,
            request_id: request.request_id,
            expected_control_rev: request.expected_control_rev,
            signature: request.signature,
        },
        request.signed,
    )?;
    let now = clock.now_secs();
    let to_epoch = i64::try_from(rotation.to_epoch).map_err(|_| Error::Invariant("epoch"))?;
    let mut revoked = 0u64;
    for device_id in revoke {
        if auth::revoke_device(conn, now, device_id)? {
            revoked += 1;
        }
        // 候选 grant / 候选材料一旦已为被吊销设备生成，就不能继续用同一候选根。
    }
    if revoked > 0 {
        vault::mark_epoch_revoked_once(conn, rotation.from_epoch)?;
    }
    conn.execute(
        "UPDATE rotations SET revoked_count = revoked_count + ?1, updated_at = ?2
         WHERE rotation_id = ?3",
        (
            i64::try_from(revoked).map_err(|_| Error::Invariant("revoked_count"))?,
            now,
            rotation_id.as_slice(),
        ),
    )?;
    let mut candidate_grant = 0i64;
    for device_id in revoke {
        candidate_grant += conn.query_row(
            "SELECT COUNT(*) FROM device_grants WHERE epoch = ?1 AND device_id = ?2",
            (to_epoch, device_id.as_slice()),
            |row| row.get::<_, i64>(0),
        )?;
    }
    let candidate_material: i64 = conn.query_row(
        "SELECT
           (SELECT COUNT(*) FROM checkpoints WHERE epoch = ?1 AND state IN ('staging','candidate'))
         + (SELECT COUNT(*) FROM key_slots WHERE epoch = ?1 AND state = 'staging')",
        [to_epoch],
        |row| row.get(0),
    )?;
    let must_restart = revoked > 0 && (candidate_grant > 0 || candidate_material > 0);
    if must_restart {
        let (new_rotation_id, _new_epoch) = discard_candidate_and_allocate(conn, now, &rotation)?;
        let new_view = view(conn, &new_rotation_id)?;
        let event = RevocationsAdded {
            rotation_id: rotation.rotation_id,
            devices: revoke.to_vec(),
            restarted: true,
            new_rotation: Some(new_rotation_id),
        }
        .encode()?;
        append_control_record(
            conn,
            now,
            ControlKind::RotationRevokedMore,
            request.request_id,
            vault.manager_rev,
            request.expected_control_rev,
            request.signed,
            request.signature,
            Some(event),
        )?;
        return Ok(RevokeMoreOutcome {
            view: new_view,
            restarted: true,
            revoked,
        });
    }
    let current = view(conn, rotation_id)?;
    let event = RevocationsAdded {
        rotation_id: rotation.rotation_id,
        devices: revoke.to_vec(),
        restarted: false,
        new_rotation: None,
    }
    .encode()?;
    append_control_record(
        conn,
        now,
        ControlKind::RotationRevokedMore,
        request.request_id,
        vault.manager_rev,
        request.expected_control_rev,
        request.signed,
        request.signature,
        Some(event),
    )?;
    Ok(RevokeMoreOutcome {
        view: current,
        restarted: false,
        revoked,
    })
}

/// §23.2：用给定管理公钥验证一段转换字节的签名（缺签名/长度错/验签失败 → 409）。
fn verify_manager_signature(
    hook: &dyn SignatureHook,
    vault_id: &Id16,
    public_key: &[u8; 32],
    transition: &[u8],
    signature: Option<&[u8]>,
) -> Result<()> {
    let signature = signature.ok_or_else(|| Error::api(ErrorCode::CasConflict))?;
    if signature.len() != 64 {
        return Err(Error::api(ErrorCode::CasConflict));
    }
    let check = SignatureCheck {
        domain: SignDomain::Management,
        vault_id,
        device_id: None,
        public_key,
        message: transition,
        signature,
        expect: &Expectation::None,
    };
    require(hook, &check, ErrorCode::CasConflict)
}

/// §23.2：旧管理 key 与新管理 key 都必须对同一段 `transition` 字节给出有效签名。
fn verify_manager_transition(
    hook: &dyn SignatureHook,
    vault_id: &Id16,
    old_public_key: &[u8; 32],
    new_public_key: &[u8; 32],
    transition: &[u8],
    old_signature: Option<&[u8]>,
    new_signature: Option<&[u8]>,
) -> Result<()> {
    verify_manager_signature(hook, vault_id, old_public_key, transition, old_signature)?;
    verify_manager_signature(hook, vault_id, new_public_key, transition, new_signature)
}

/// abort 请求。
#[derive(Debug)]
pub struct AbortRequest<'a> {
    pub request_id: Id16,
    pub path: &'a str,
    pub body_sha256: Hash32,
    pub expected_control_rev: u64,
    pub signed: &'a [u8],
    pub signature: &'a [u8],
}

/// 设备在当前活动 epoch 的最新 grant（`GET devices/self/grants/latest`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantRow {
    pub epoch: u64,
    pub device_id: Id16,
    pub manager_rev: u64,
    pub hpke_enc: [u8; 32],
    pub hpke_ciphertext: Vec<u8>,
    pub signed_header: Vec<u8>,
    /// §24.2：外层管理签名（客户端验 grant 来源）。
    pub signature: Option<Vec<u8>>,
    /// 客户端生成的 grant id（进入 AAD，防跨 grant 重放）。
    pub grant_id: Option<Id16>,
    /// §28.1 条目 3：候选 grant 所属 rotation；active epoch 的 grant 为 None。
    pub activation_ref: Option<Id16>,
}

/// 读设备在活动 epoch 的 grant；staged（候选 epoch）的 grant 不可见（§8 步 5）。
pub fn latest_grant(conn: &Connection, device_id: &Id16) -> Result<Option<GrantRow>> {
    let vault = vault::load(conn)?;
    let mut stmt = conn.prepare(
        "SELECT epoch, device_id, manager_rev, hpke_enc, hpke_ciphertext, signed_header, signature,
                grant_id, activation_ref
         FROM device_grants WHERE epoch = ?1 AND device_id = ?2",
    )?;
    let row = stmt
        .query_row(
            (
                i64::try_from(vault.active_epoch).map_err(|_| Error::Invariant("epoch"))?,
                device_id.as_slice(),
            ),
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                    row.get::<_, Vec<u8>>(4)?,
                    row.get::<_, Vec<u8>>(5)?,
                    row.get::<_, Option<Vec<u8>>>(6)?,
                    row.get::<_, Option<Vec<u8>>>(7)?,
                    row.get::<_, Option<Vec<u8>>>(8)?,
                ))
            },
        )
        .optional()?;
    let Some((
        epoch,
        device,
        manager_rev,
        hpke_enc,
        ciphertext,
        header,
        signature,
        grant_id,
        activation_ref,
    )) = row
    else {
        return Ok(None);
    };
    Ok(Some(GrantRow {
        epoch: u64::try_from(epoch).map_err(|_| Error::Invariant("epoch"))?,
        device_id: device
            .as_slice()
            .try_into()
            .map_err(|_| Error::Invariant("device_id"))?,
        manager_rev: u64::try_from(manager_rev).map_err(|_| Error::Invariant("manager_rev"))?,
        hpke_enc: hpke_enc
            .as_slice()
            .try_into()
            .map_err(|_| Error::Invariant("hpke_enc"))?,
        hpke_ciphertext: ciphertext,
        signed_header: header,
        signature,
        grant_id: match grant_id {
            None => None,
            Some(bytes) => Some(
                bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| Error::Invariant("grant_id"))?,
            ),
        },
        activation_ref: match activation_ref {
            None => None,
            Some(bytes) => Some(
                bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| Error::Invariant("activation_ref"))?,
            ),
        },
    }))
}

/// 某 epoch 的全部 grant（management 才可读的 staged 列表用）。
pub fn list_grants(conn: &Connection, epoch: u64) -> Result<Vec<GrantRow>> {
    let mut stmt = conn.prepare(
        "SELECT epoch, device_id, manager_rev, hpke_enc, hpke_ciphertext, signed_header, signature,
                grant_id, activation_ref
         FROM device_grants WHERE epoch = ?1 ORDER BY device_id ASC",
    )?;
    let rows = stmt.query_map(
        [i64::try_from(epoch).map_err(|_| Error::Invariant("epoch"))?],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Vec<u8>>(3)?,
                row.get::<_, Vec<u8>>(4)?,
                row.get::<_, Vec<u8>>(5)?,
                row.get::<_, Option<Vec<u8>>>(6)?,
                row.get::<_, Option<Vec<u8>>>(7)?,
                row.get::<_, Option<Vec<u8>>>(8)?,
            ))
        },
    )?;
    let mut out = Vec::new();
    for row in rows {
        let (
            epoch,
            device,
            manager_rev,
            hpke_enc,
            ciphertext,
            header,
            signature,
            grant_id,
            activation_ref,
        ) = row?;
        out.push(GrantRow {
            epoch: u64::try_from(epoch).map_err(|_| Error::Invariant("epoch"))?,
            device_id: device
                .as_slice()
                .try_into()
                .map_err(|_| Error::Invariant("device_id"))?,
            manager_rev: u64::try_from(manager_rev).map_err(|_| Error::Invariant("manager_rev"))?,
            hpke_enc: hpke_enc
                .as_slice()
                .try_into()
                .map_err(|_| Error::Invariant("hpke_enc"))?,
            hpke_ciphertext: ciphertext,
            signed_header: header,
            signature,
            grant_id: match grant_id {
                None => None,
                Some(bytes) => Some(
                    bytes
                        .as_slice()
                        .try_into()
                        .map_err(|_| Error::Invariant("grant_id"))?,
                ),
            },
            activation_ref: match activation_ref {
                None => None,
                Some(bytes) => Some(
                    bytes
                        .as_slice()
                        .try_into()
                        .map_err(|_| Error::Invariant("activation_ref"))?,
                ),
            },
        });
    }
    Ok(out)
}

/// grant 包摘要：`SHA256(hpke_enc || SHA256(ciphertext) || SHA256(signed_header) || signature)`。
/// 覆盖密文/封装/签名三者（评审 §三 4：包 digest 不能只有 header）。
pub fn grant_package_digest(grant: &GrantRow) -> Hash32 {
    sha256(
        &[
            grant.hpke_enc.as_slice(),
            sha256(&grant.hpke_ciphertext).as_slice(),
            sha256(&grant.signed_header).as_slice(),
            grant.signature.as_deref().unwrap_or(&[]),
        ]
        .concat(),
    )
}

/// 诊断：grant 数量（不做健康/顺序判断）。
pub fn grant_count(conn: &Connection, epoch: u64) -> Result<u64> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM device_grants WHERE epoch = ?1",
        [i64::try_from(epoch).map_err(|_| Error::Invariant("epoch"))?],
        |row| row.get(0),
    )?;
    u64::try_from(count).map_err(|_| Error::Invariant("grant count"))
}

/// 陈旧槽清理：`retire_at <= now` 的 retiring 槽删除（只删槽，不删数据；ADR §8 步 7）。
pub fn purge_retired_slots(conn: &Connection, now: i64) -> Result<u64> {
    let changed = conn.execute(
        "DELETE FROM key_slots WHERE state = 'retiring' AND retire_at IS NOT NULL AND retire_at <= ?1",
        [now],
    )?;
    Ok(changed as u64)
}

/// 成员集合的诊断摘要（轮换视图里 `member_set_hash` 的来源）。
pub fn member_digest(conn: &Connection) -> Result<Hash32> {
    member_set_hash(conn)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manager_transition_cde_roundtrip_and_tamper() {
        let record = ManagerTransition {
            vault_id: [1u8; 16],
            instance_id: [2u8; 16],
            rotation_id: [3u8; 16],
            from_epoch: 1,
            to_epoch: 2,
            old_rev: 1,
            new_rev: 2,
            new_pubkey: [4u8; 32],
            covered_seq: 7,
        };
        let bytes = record.encode().expect("encode");
        assert_eq!(ManagerTransition::decode(&bytes).expect("decode"), record);
        // 任意字段篡改都会被解码/绑定发现（这里验证 CDE 篡改至少不会静默通过）。
        let mut tampered = bytes.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 0xFF;
        assert!(ManagerTransition::decode(&tampered).is_err());
    }

    #[test]
    fn manager_transition_requires_both_valid_signatures() {
        use ed25519_dalek::{Signer, SigningKey};
        let old = SigningKey::from_bytes(&[1u8; 32]);
        let new = SigningKey::from_bytes(&[2u8; 32]);
        let imposter = SigningKey::from_bytes(&[3u8; 32]);
        let transition = b"MN/v1/manager-transition|old=1|new=2|pub=new";
        let old_sig = old.sign(transition).to_bytes().to_vec();
        let new_sig = new.sign(transition).to_bytes().to_vec();
        let hook = crate::verify::Ed25519Hook;
        let vault = [9u8; 16];
        let old_pub = old.verifying_key().to_bytes();
        let new_pub = new.verifying_key().to_bytes();

        verify_manager_transition(
            &hook,
            &vault,
            &old_pub,
            &new_pub,
            transition,
            Some(&old_sig),
            Some(&new_sig),
        )
        .expect("双签有效");

        // 单签（缺旧/缺新）→ 409。
        for (old_s, new_s) in [
            (None, Some(new_sig.as_slice())),
            (Some(old_sig.as_slice()), None),
        ] {
            let err = verify_manager_transition(
                &hook, &vault, &old_pub, &new_pub, transition, old_s, new_s,
            )
            .expect_err("缺一签必须拒绝");
            assert!(matches!(err, Error::Api(api) if api.code == ErrorCode::CasConflict));
        }

        // 错 key（新 key 的签名冒充旧 key / 第三方签名）→ 409。
        let bad_old = new.sign(transition).to_bytes().to_vec();
        let err = verify_manager_transition(
            &hook,
            &vault,
            &old_pub,
            &new_pub,
            transition,
            Some(&bad_old),
            Some(&new_sig),
        )
        .expect_err("旧 key 错");
        assert!(matches!(err, Error::Api(api) if api.code == ErrorCode::CasConflict));
        let bad_new = imposter.sign(transition).to_bytes().to_vec();
        let err = verify_manager_transition(
            &hook,
            &vault,
            &old_pub,
            &new_pub,
            transition,
            Some(&old_sig),
            Some(&bad_new),
        )
        .expect_err("新 key 错");
        assert!(matches!(err, Error::Api(api) if api.code == ErrorCode::CasConflict));
    }

    #[test]
    fn rotation_state_roundtrip() {
        for state in [
            RotationState::Frozen,
            RotationState::Staging,
            RotationState::Activated,
            RotationState::Aborted,
        ] {
            assert_eq!(RotationState::parse(state.as_str()), Some(state));
        }
        assert_eq!(RotationState::parse("done"), None);
    }

    #[test]
    fn grant_digest_changes_with_membership() {
        // 与 view 内联算法保持同一口径：sha256(concat(device_id || sha256(header)))。
        let mut buf = Vec::new();
        buf.extend_from_slice(&[1u8; 16]);
        buf.extend_from_slice(&sha256(b"header"));
        let digest = sha256(&buf);
        buf.extend_from_slice(&[2u8; 16]);
        assert_ne!(digest, sha256(&buf));
    }
}
