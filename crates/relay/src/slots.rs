//! key slot 端点适配：`GET` / `PUT` / `password:activate`（ADR 0006 §5 key-slots、§3 改口令）。
//!
//! 语义：
//! - slot 本体是一个已 `complete` 的 KEY_SLOT 对象（密文信封由客户端上传）；本模块只登记
//!   描述符与状态，**不解密**；
//! - 写入前用 `SlotDescriptorV1` 校验 slot_id/vault/epoch/kind 与路径/header 一致（§6"写入约束"）；
//! - `password:activate` 用 `slot_set_rev` 做 CAS：成功则旧 password 槽**立即删除**、新槽激活
//!   （ADR §3 改口令：commit 后立即移除旧槽）；失败 409，不留长期双 active；
//! - recovery 槽不参与 password 激活（v1 只做 password 重包）。

use rusqlite::{Connection, OptionalExtension};

use crate::auth::AuthContext;
use crate::envelope::ObjectType;
use crate::error::{ApiError, Error, ErrorCode, Result};
use crate::ids::{sha256, Id16};
use crate::objects;
use crate::sig::{require, Expectation, SignDomain, SignatureCheck, SignatureHook};
use crate::time::Clock;
use crate::vault;
use menghunote_sync_proto as proto;

/// slot 种类（wire 用 text）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotKind {
    Password,
    Recovery,
}

impl SlotKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Password => "password",
            Self::Recovery => "recovery",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "password" => Some(Self::Password),
            "recovery" => Some(Self::Recovery),
            _ => None,
        }
    }

    fn as_proto(self) -> proto::SlotKind {
        match self {
            Self::Password => proto::SlotKind::Password,
            Self::Recovery => proto::SlotKind::Recovery,
        }
    }

    fn from_proto(kind: proto::SlotKind) -> Self {
        match kind {
            proto::SlotKind::Password => Self::Password,
            proto::SlotKind::Recovery => Self::Recovery,
        }
    }
}

/// slot 状态（ADR §6）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotState {
    Staging,
    Active,
    Retiring,
}

impl SlotState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Staging => "staging",
            Self::Active => "active",
            Self::Retiring => "retiring",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "staging" => Some(Self::Staging),
            "active" => Some(Self::Active),
            "retiring" => Some(Self::Retiring),
            _ => None,
        }
    }
}

/// `key_slots` 行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotRow {
    pub slot_id: [u8; 32],
    pub epoch: u64,
    pub kind: SlotKind,
    pub descriptor: Vec<u8>,
    pub state: SlotState,
    pub slot_set_rev: u64,
    pub retire_at: Option<i64>,
}

impl SlotRow {
    /// 更新 64 字节定长值（避免调用方到处 try_into）。
    fn try_id(bytes: Vec<u8>) -> Result<[u8; 32]> {
        bytes
            .as_slice()
            .try_into()
            .map_err(|_| Error::Invariant("slot_id"))
    }
}

fn read_row(
    slot_id: Vec<u8>,
    epoch: i64,
    kind: String,
    descriptor: Vec<u8>,
    state: String,
    slot_set_rev: i64,
    retire_at: Option<i64>,
) -> Result<SlotRow> {
    Ok(SlotRow {
        slot_id: SlotRow::try_id(slot_id)?,
        epoch: u64::try_from(epoch).map_err(|_| Error::Invariant("key_slots.epoch"))?,
        kind: SlotKind::parse(&kind).ok_or(Error::Invariant("key_slots.kind"))?,
        descriptor,
        state: SlotState::parse(&state).ok_or(Error::Invariant("key_slots.state"))?,
        slot_set_rev: u64::try_from(slot_set_rev).map_err(|_| Error::Invariant("slot_set_rev"))?,
        retire_at,
    })
}

/// `GET key-slots`：返回 active 槽 + 未过期 retiring 槽（旧槽仅对迁移授权）。
pub fn list(conn: &Connection, now: i64) -> Result<Vec<SlotRow>> {
    let mut stmt = conn.prepare(
        "SELECT slot_id, epoch, kind, descriptor, state, slot_set_rev, retire_at
         FROM key_slots
         WHERE state = 'active' OR (state = 'retiring' AND (retire_at IS NULL OR retire_at > ?1))
         ORDER BY kind ASC, slot_set_rev DESC",
    )?;
    let rows = stmt.query_map([now], |row| {
        Ok((
            row.get::<_, Vec<u8>>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Vec<u8>>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, i64>(5)?,
            row.get::<_, Option<i64>>(6)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (slot_id, epoch, kind, descriptor, state, rev, retire_at) = row?;
        out.push(read_row(
            slot_id, epoch, kind, descriptor, state, rev, retire_at,
        )?);
    }
    Ok(out)
}

/// `PUT key-slots/{s}` 的请求。
#[derive(Debug)]
pub struct SlotPutRequest {
    pub slot_id: [u8; 32],
    pub epoch: u64,
    pub kind: SlotKind,
    pub descriptor: Vec<u8>,
    pub signed_message: Vec<u8>,
    pub signature: Vec<u8>,
}

/// `PUT key-slots/{s}`：登记候选槽（staging），对象必须已 `complete`。
pub fn put(
    conn: &Connection,
    clock: &dyn Clock,
    hook: &dyn SignatureHook,
    actor: &AuthContext,
    request: &SlotPutRequest,
) -> Result<SlotRow> {
    let slot_id = &request.slot_id;
    let epoch = request.epoch;
    let kind = request.kind;
    let descriptor = &request.descriptor;
    let device_id = actor.device_id()?;
    let vault = vault::load(conn)?;
    let now = clock.now_secs();
    // 活动 epoch，或轮换中的候选 epoch（ADR §8 步 5：候选基线要发布新槽）。
    let is_candidate =
        vault.state == vault::VaultState::Rotating && vault::candidate_epoch(conn)? == Some(epoch);
    if epoch != vault.active_epoch && !is_candidate {
        return Err(Error::Api(
            ApiError::new(ErrorCode::EpochMismatch).epoch(vault.active_epoch),
        ));
    }
    // 描述符必须与路径/对象/epoch 一致（写入前一致性校验）。
    let descriptor_record = proto::SlotDescriptorV1::decode(descriptor)
        .map_err(|_| Error::api(ErrorCode::InvalidRequest))?;
    if descriptor_record.slot_id != *slot_id
        || descriptor_record.vault_id != vault.vault_id
        || descriptor_record.key_epoch != epoch
        || descriptor_record.kind != kind.as_proto()
    {
        return Err(Error::api(ErrorCode::InvalidRequest));
    }
    // slot 本体必须是已完成的 KEY_SLOT 对象，且 epoch 匹配。
    let object = objects::load(conn, slot_id)?;
    if object.object_type != ObjectType::KeySlot
        || object.state != objects::ObjectState::Complete
        || object.epoch != epoch
    {
        return Err(Error::api(ErrorCode::Incomplete));
    }
    let object_meta = objects::meta(conn, slot_id)?;
    let _ = object_meta;
    // 已注册设备签名（槽指针/新增槽都需要该设备签名，ADR §3）。
    let sign_pk = vault::device_sign_pk(conn, &device_id)?
        .ok_or_else(|| Error::Api(ApiError::new(ErrorCode::Revoked).epoch(epoch)))?;
    let check = SignatureCheck {
        domain: SignDomain::SlotActivate,
        vault_id: &vault.vault_id,
        device_id: Some(&device_id),
        public_key: &sign_pk,
        message: &request.signed_message,
        signature: &request.signature,
        expect: &Expectation::None,
    };
    require(hook, &check, ErrorCode::SignatureInvalid)?;

    // 已有同 id：同内容幂等，不同内容 409（槽不可变）。
    if let Some(existing) = get(conn, slot_id)? {
        if existing.epoch == epoch
            && existing.kind == kind
            && existing.descriptor == request.descriptor
        {
            return Ok(existing);
        }
        return Err(Error::api(ErrorCode::IdConflict));
    }
    let next_rev = next_slot_set_rev(conn, kind)?;
    conn.execute(
        "INSERT INTO key_slots(slot_id, epoch, kind, descriptor, state, slot_set_rev, retire_at,
                               created_at)
         VALUES (?1, ?2, ?3, ?4, 'staging', ?5, NULL, ?6)",
        (
            slot_id.as_slice(),
            i64::try_from(epoch).map_err(|_| Error::Invariant("epoch"))?,
            kind.as_str(),
            request.descriptor.as_slice(),
            i64::try_from(next_rev).map_err(|_| Error::Invariant("slot rev"))?,
            now,
        ),
    )?;
    get(conn, slot_id)?.ok_or(Error::Invariant("slot vanished"))
}

/// 读单个 slot。
pub fn get(conn: &Connection, slot_id: &[u8; 32]) -> Result<Option<SlotRow>> {
    let mut stmt = conn.prepare(
        "SELECT slot_id, epoch, kind, descriptor, state, slot_set_rev, retire_at
         FROM key_slots WHERE slot_id = ?1",
    )?;
    let row = stmt
        .query_row([slot_id.as_slice()], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Vec<u8>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, Option<i64>>(6)?,
            ))
        })
        .optional()?;
    let Some((slot_id, epoch, kind, descriptor, state, rev, retire_at)) = row else {
        return Ok(None);
    };
    Ok(Some(read_row(
        slot_id, epoch, kind, descriptor, state, rev, retire_at,
    )?))
}

fn next_slot_set_rev(conn: &Connection, kind: SlotKind) -> Result<u64> {
    let value: i64 = conn.query_row(
        "SELECT COALESCE(MAX(slot_set_rev), 0) FROM key_slots WHERE kind = ?1",
        [kind.as_str()],
        |row| row.get(0),
    )?;
    Ok(u64::try_from(value)
        .map_err(|_| Error::Invariant("slot_set_rev"))?
        .saturating_add(1))
}

/// 当前 active password 槽的 `slot_set_rev`（没有则为 0）。
pub fn active_password_rev(conn: &Connection) -> Result<u64> {
    let value: i64 = conn.query_row(
        "SELECT COALESCE(MAX(slot_set_rev), 0) FROM key_slots
         WHERE kind = 'password' AND state = 'active'",
        [],
        |row| row.get(0),
    )?;
    u64::try_from(value).map_err(|_| Error::Invariant("slot_set_rev"))
}

/// `POST key-slots/password:activate` 的请求。
#[derive(Debug)]
pub struct PasswordActivateRequest {
    /// CAS 期望的当前 password 槽集版本。
    pub expected_slot_set_rev: u64,
    pub new_slot_id: [u8; 32],
    pub signed_message: Vec<u8>,
    pub signature: Vec<u8>,
}

/// `POST key-slots/password:activate`：CAS `slot_set_rev` 并在同一事务里切换 active 指针。
pub fn activate_password(
    conn: &Connection,
    clock: &dyn Clock,
    hook: &dyn SignatureHook,
    actor: &AuthContext,
    request: &PasswordActivateRequest,
) -> Result<u64> {
    let expected_slot_set_rev = request.expected_slot_set_rev;
    let new_slot_id = &request.new_slot_id;
    let device_id = actor.device_id()?;
    let vault = vault::load(conn)?;
    let now = clock.now_secs();
    let sign_pk = vault::device_sign_pk(conn, &device_id)?
        .ok_or_else(|| Error::Api(ApiError::new(ErrorCode::Revoked).epoch(vault.active_epoch)))?;
    let check = SignatureCheck {
        domain: SignDomain::SlotActivate,
        vault_id: &vault.vault_id,
        device_id: Some(&device_id),
        public_key: &sign_pk,
        message: &request.signed_message,
        signature: &request.signature,
        expect: &Expectation::None,
    };
    require(hook, &check, ErrorCode::SignatureInvalid)?;

    let staged = get(conn, new_slot_id)?.ok_or_else(|| Error::api(ErrorCode::NotFound))?;
    if staged.kind != SlotKind::Password
        || staged.state != SlotState::Staging
        || staged.epoch != vault.active_epoch
    {
        return Err(Error::api(ErrorCode::Incomplete));
    }
    let current = active_password_rev(conn)?;
    if current != expected_slot_set_rev {
        return Err(Error::Api(
            ApiError::new(ErrorCode::CasConflict).control_rev(vault.control_rev),
        ));
    }
    // 改口令 commit 后旧槽立即移除（ADR §3：不存在长期双 active）。
    conn.execute(
        "DELETE FROM key_slots WHERE kind = 'password' AND state = 'active'",
        [],
    )?;
    let next_rev = expected_slot_set_rev.saturating_add(1);
    conn.execute(
        "UPDATE key_slots SET state = 'active', slot_set_rev = ?1, retire_at = NULL
         WHERE slot_id = ?2",
        (
            i64::try_from(next_rev).map_err(|_| Error::Invariant("slot rev"))?,
            new_slot_id.as_slice(),
        ),
    )?;
    // 槽指针更新进控制链（永久保留）。
    vault::append_control(conn, now, &request.signed_message)?;
    Ok(next_rev)
}

/// 轮换 activate 前置条件：候选 epoch 必须已发布**新槽**（password + recovery 各至少一个
/// staging），ADR §8 步 5。返回缺失原因（空 = 齐备）。
pub fn rotation_slots_ready(conn: &Connection, epoch: u64) -> Result<Vec<&'static str>> {
    let mut issues = Vec::new();
    let epoch = i64::try_from(epoch).map_err(|_| Error::Invariant("epoch"))?;
    let password: i64 = conn.query_row(
        "SELECT COUNT(*) FROM key_slots WHERE epoch = ?1 AND kind = 'password' AND state = 'staging'",
        [epoch],
        |row| row.get(0),
    )?;
    if password == 0 {
        issues.push("missing_password_slot");
    }
    let recovery: i64 = conn.query_row(
        "SELECT COUNT(*) FROM key_slots WHERE epoch = ?1 AND kind = 'recovery' AND state = 'staging'",
        [epoch],
        |row| row.get(0),
    )?;
    if recovery == 0 {
        issues.push("missing_recovery_slot");
    }
    Ok(issues)
}

/// 清理过期 retiring 槽（旧槽 14 天保留期到点后删除；只删槽，不删数据）。
pub fn purge_retired_slots(conn: &Connection, now: i64) -> Result<u64> {
    let changed = conn.execute(
        "DELETE FROM key_slots WHERE state = 'retiring' AND retire_at IS NOT NULL AND retire_at <= ?1",
        [now],
    )?;
    Ok(changed as u64)
}

/// slot 描述符的 context hash（KEY_SLOT 信封要求 `SHA256(CDE(descriptor))`，供测试/诊断）。
pub fn descriptor_context_hash(descriptor: &[u8]) -> [u8; 32] {
    sha256(descriptor)
}

/// 槽诊断时间线：`(slot_id, kind, state, slot_set_rev)`。
pub type SlotTimeline = Vec<(Vec<u8>, String, String, u64)>;

/// 诊断：所有槽的 `(slot_id, kind, state, rev)`。
pub fn timeline(conn: &Connection) -> Result<SlotTimeline> {
    let mut stmt =
        conn.prepare("SELECT slot_id, kind, state, slot_set_rev FROM key_slots ORDER BY slot_id")?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, Vec<u8>>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, i64>(3)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (id, kind, state, rev) = row?;
        out.push((
            id,
            kind,
            state,
            u64::try_from(rev).map_err(|_| Error::Invariant("slot rev"))?,
        ));
    }
    Ok(out)
}

/// 便捷：当前 active 的 password/recovery slot（诊断/配对客户端用）。
pub fn active_slots(conn: &Connection) -> Result<Vec<SlotRow>> {
    let mut stmt = conn.prepare(
        "SELECT slot_id, epoch, kind, descriptor, state, slot_set_rev, retire_at
         FROM key_slots WHERE state = 'active' ORDER BY kind",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, Vec<u8>>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Vec<u8>>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, i64>(5)?,
            row.get::<_, Option<i64>>(6)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (slot_id, epoch, kind, descriptor, state, rev, retire_at) = row?;
        out.push(read_row(
            slot_id, epoch, kind, descriptor, state, rev, retire_at,
        )?);
    }
    Ok(out)
}

/// 设备可见的 slot 列表（recovery 槽对普通成员也可见——客户端需要找到自己的槽描述符；
/// 密文本体只能解密旧根，泄露描述符不泄露密钥）。
pub fn list_visible(conn: &Connection, now: i64) -> Result<Vec<SlotRow>> {
    list(conn, now)
}

/// `SlotKind` 与 proto 互转（测试/客户端用）。
pub fn kind_from_proto(kind: proto::SlotKind) -> SlotKind {
    SlotKind::from_proto(kind)
}

/// 设备 id 的辅助（避免调用方重复 try_into）。
pub fn device_id(value: &Id16) -> Id16 {
    *value
}
