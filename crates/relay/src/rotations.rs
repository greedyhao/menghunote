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
use crate::error::{ApiError, Error, ErrorCode, Result};
use crate::ids::{sha256, Hash32, Id16};
use crate::log::{self, LogKind};
use crate::sig::{require, Expectation, SignDomain, SignatureCheck, SignatureHook};
use crate::time::{secs, Clock};
use crate::vault;

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
    pub revoked_count: u64,
    pub state: RotationState,
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
    pub rev: u64,
    pub pubkey: [u8; 32],
    pub transition: Vec<u8>,
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
}

/// activate 结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivateOutcome {
    pub epoch: u64,
    pub control_rev: u64,
    pub auth_rev: u64,
    pub member_set_hash: Hash32,
}

/// 读 rotation 行。
pub fn get(conn: &Connection, rotation_id: &Id16) -> Result<RotationRow> {
    let mut stmt = conn.prepare(
        "SELECT rotation_id, from_epoch, to_epoch, freeze_seq, member_set_hash, revoked_count,
                state, created_at, updated_at
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
                row.get::<_, i64>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, i64>(7)?,
                row.get::<_, i64>(8)?,
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
        member_set_hash: match row.4 {
            None => None,
            Some(bytes) => Some(
                bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| Error::Invariant("member_set_hash"))?,
            ),
        },
        revoked_count: u64::try_from(row.5).map_err(|_| Error::Invariant("revoked_count"))?,
        state: RotationState::parse(&row.6).ok_or(Error::Invariant("rotations.state"))?,
        created_at: row.7,
        updated_at: row.8,
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

/// 视图（含 staged grants 摘要）。
pub fn view(conn: &Connection, rotation_id: &Id16) -> Result<RotationView> {
    let rotation = get(conn, rotation_id)?;
    let mut stmt = conn.prepare(
        "SELECT device_id, signed_header FROM device_grants WHERE epoch = ?1 ORDER BY device_id ASC",
    )?;
    let rows = stmt.query_map(
        [i64::try_from(rotation.to_epoch).map_err(|_| Error::Invariant("epoch"))?],
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
    let published = checkpoints::published_for_epoch(conn, rotation.to_epoch)?;
    Ok(RotationView {
        rotation,
        grants_count,
        grants_digest: sha256(&buf),
        published_checkpoints: published.0,
    })
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

    // 立即吊销 + 失效旧 token（§8 步 2）。每吊销一台都会推进 auth_rev。
    let mut revoked_count = 0u64;
    for device_id in request.revoke {
        if auth::revoke_device(conn, now, device_id)? {
            revoked_count += 1;
        }
    }
    conn.execute(
        "INSERT INTO rotations(rotation_id, from_epoch, to_epoch, freeze_seq, member_set_hash,
                               revoked_count, state, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, NULL, ?5, 'frozen', ?6, ?6)",
        (
            rotation_id.as_slice(),
            i64::try_from(vault.active_epoch).map_err(|_| Error::Invariant("epoch"))?,
            i64::try_from(to_epoch).map_err(|_| Error::Invariant("epoch"))?,
            i64::try_from(freeze_seq).map_err(|_| Error::Invariant("freeze_seq"))?,
            i64::try_from(revoked_count).map_err(|_| Error::Invariant("revoked_count"))?,
            now,
        ),
    )?;
    vault::set_state(conn, vault::VaultState::Rotating)?;
    // 控制链记录本次 freeze/吊销（管理签名后的原始字节）。
    vault::append_control(conn, now, request.signed)?;
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
    let manager_pub =
        vault::manager_pubkey(conn)?.ok_or_else(|| Error::api(ErrorCode::ManagementRequired))?;
    let check = SignatureCheck {
        domain: SignDomain::DeviceApprove,
        vault_id: &vault.vault_id,
        device_id: Some(recipient),
        public_key: &manager_pub,
        message: input.signed_header,
        signature: input.signature,
        expect: &Expectation::None,
    };
    require(hook, &check, ErrorCode::ManagementRequired)?;

    let existing: Option<(Vec<u8>, Vec<u8>)> = conn
        .query_row(
            "SELECT hpke_ciphertext, signed_header FROM device_grants
             WHERE epoch = ?1 AND device_id = ?2",
            (
                i64::try_from(rotation.to_epoch).map_err(|_| Error::Invariant("epoch"))?,
                recipient.as_slice(),
            ),
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    match existing {
        Some((ciphertext, header))
            if ciphertext == input.hpke_ciphertext && header == input.signed_header =>
        {
            // 幂等重放。
        }
        Some(_) => return Err(Error::api(ErrorCode::CasConflict)),
        None => {
            conn.execute(
                "INSERT INTO device_grants(epoch, device_id, manager_rev, hpke_enc, hpke_ciphertext,
                                           signed_header)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                (
                    i64::try_from(rotation.to_epoch).map_err(|_| Error::Invariant("epoch"))?,
                    recipient.as_slice(),
                    i64::try_from(input.manager_rev).map_err(|_| Error::Invariant("manager_rev"))?,
                    input.hpke_enc.as_slice(),
                    input.hpke_ciphertext,
                    input.signed_header,
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

    // 两份覆盖 S 的候选基线。
    let (published, min_covered) = checkpoints::published_for_epoch(conn, rotation.to_epoch)?;
    if published < 2 || min_covered < rotation.freeze_seq {
        return Err(Error::api(ErrorCode::Incomplete));
    }
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

    let now = clock.now_secs();
    // 管理公钥转换（旧/新双签在 `transition` 里；这里只做 rev 连续性与发布）。
    if let Some(new_manager) = &request.new_manager {
        if new_manager.rev != vault.manager_rev + 1 {
            return Err(Error::api(ErrorCode::CasConflict));
        }
        conn.execute(
            "INSERT INTO manager_keys(rev, pubkey, transition, created_at) VALUES (?1, ?2, ?3, ?4)",
            (
                i64::try_from(new_manager.rev).map_err(|_| Error::Invariant("manager rev"))?,
                new_manager.pubkey.as_slice(),
                new_manager.transition.as_slice(),
                now,
            ),
        )?;
        conn.execute(
            "UPDATE vault SET manager_rev = ?1 WHERE singleton = 1",
            [i64::try_from(new_manager.rev).map_err(|_| Error::Invariant("manager rev"))?],
        )?;
    }

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
    let control_rev = vault::append_control(conn, now, request.signed)?;
    log::append(
        conn,
        now,
        LogKind::Control,
        None,
        rotation.to_epoch,
        request.signed,
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
    if rotation.revoked_count == 0 {
        vault::set_state(conn, vault::VaultState::Active)?;
    }
    vault::append_control(conn, now, request.signed)?;
    let _ = hook;
    let _ = vault;
    view(conn, rotation_id)
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
}

/// 读设备在活动 epoch 的 grant；staged（候选 epoch）的 grant 不可见（§8 步 5）。
pub fn latest_grant(conn: &Connection, device_id: &Id16) -> Result<Option<GrantRow>> {
    let vault = vault::load(conn)?;
    let mut stmt = conn.prepare(
        "SELECT epoch, device_id, manager_rev, hpke_enc, hpke_ciphertext, signed_header
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
                ))
            },
        )
        .optional()?;
    let Some((epoch, device, manager_rev, hpke_enc, ciphertext, header)) = row else {
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
    }))
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
