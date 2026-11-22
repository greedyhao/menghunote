//! 送达日志（ADR 0006 §5 `GET P/log`、§7 GC 前缀）。
//!
//! `relay_seq` **只表示送达顺序**：它在一个 writer 事务内单调分配，`>= gc_floor` 的前缀不能有洞。
//! 任何 log 事件都必须在对象 `complete` 的同一事务里产生（§13.1：片没齐禁止进 log）。

use rusqlite::OptionalExtension;

use crate::cursor::{self, Cursor, CursorKey};
use crate::error::{ApiError, Error, ErrorCode, Result};
use crate::ids::{be64, Hash32};
use crate::scope::ScopeConn;
use crate::vault::Vault;

/// log 事件类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogKind {
    Update,
    Asset,
    Checkpoint,
    Control,
}

impl LogKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Update => "update",
            Self::Asset => "asset",
            Self::Checkpoint => "checkpoint",
            Self::Control => "control",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "update" => Some(Self::Update),
            "asset" => Some(Self::Asset),
            "checkpoint" => Some(Self::Checkpoint),
            "control" => Some(Self::Control),
            _ => None,
        }
    }
}

/// 一条日志事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEvent {
    pub relay_seq: u64,
    pub kind: LogKind,
    pub object_id: Option<Hash32>,
    pub epoch: u64,
    pub event: Vec<u8>,
    /// 事件签名（update/asset = 设备对 ObjectCommit 的签名；control = 管理签名）。
    pub signature: Option<Vec<u8>>,
}

/// 一页日志。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogPage {
    pub events: Vec<LogEvent>,
    /// `next_cursor` 指**最后返回事件**（无事件时回显请求游标），不是 head（ADR §5）。
    pub next_cursor: String,
    /// 本页覆盖到的水位（同上）。
    pub through_seq: u64,
    /// 查询时的 head（客户端据此知道还有多少）。
    pub query_head: u64,
    /// 当前活动 epoch。
    pub epoch: u64,
    /// 当前 gc_floor。
    pub gc_floor: u64,
}

/// 追加一条日志并分配 `relay_seq`（**同一写事务内**调用）。
pub fn append(
    conn: &ScopeConn,
    now: i64,
    kind: LogKind,
    object_id: Option<&Hash32>,
    epoch: u64,
    event: &[u8],
    signature: Option<&[u8]>,
) -> Result<u64> {
    let head: i64 = conn.query_row(
        "SELECT head_seq FROM vaults WHERE vault_id = ?1",
        [conn.scope().as_slice()],
        |row| row.get(0),
    )?;
    let next = u64::try_from(head)
        .map_err(|_| Error::Invariant("vault.head_seq"))?
        .checked_add(1)
        .ok_or(Error::Invariant("relay_seq overflow"))?;
    let next_i64 = i64::try_from(next).map_err(|_| Error::Invariant("relay_seq overflow"))?;
    conn.execute(
        "INSERT INTO log(vault_id, relay_seq, kind, object_id, epoch, event, signature,
                         created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        (
            conn.scope().as_slice(),
            next_i64,
            kind.as_str(),
            object_id.map(|id| id.as_slice()),
            i64::try_from(epoch).map_err(|_| Error::Invariant("epoch overflow"))?,
            event,
            signature,
            now,
        ),
    )?;
    conn.execute(
        "UPDATE vaults SET head_seq = ?1 WHERE vault_id = ?",
        (next_i64, conn.scope().as_slice()),
    )?;
    Ok(next)
}

/// 该 `relay_seq` 是否已存在（诊断/幂等测试）。
pub fn contains(conn: &ScopeConn, relay_seq: u64) -> Result<bool> {
    let value: Option<i64> = conn
        .query_row(
            "SELECT relay_seq FROM log WHERE relay_seq = ?1 AND vault_id = ?",
            (
                i64::try_from(relay_seq).map_err(|_| Error::Invariant("relay_seq"))?,
                conn.scope().as_slice(),
            ),
            |row| row.get(0),
        )
        .optional()?;
    Ok(value.is_some())
}

/// 日志行数（测试/诊断）。
pub fn count(conn: &ScopeConn) -> Result<u64> {
    let total: i64 = conn.query_row(
        "SELECT COUNT(*) FROM log WHERE vault_id = ?1",
        [conn.scope().as_slice()],
        |row| row.get(0),
    )?;
    u64::try_from(total).map_err(|_| Error::Invariant("log count"))
}

/// 某对象在 log 中的 `relay_seq`（GC 需要"无根引用且已被 GC 前缀覆盖"的判定）。
pub fn seq_of_object(conn: &ScopeConn, object_id: &Hash32) -> Result<Option<u64>> {
    let value: Option<i64> = conn
        .query_row(
            "SELECT relay_seq FROM log
             WHERE object_id = ?1 AND vault_id = ? ORDER BY relay_seq LIMIT 1",
            (object_id.as_slice(), conn.scope().as_slice()),
            |row| row.get(0),
        )
        .optional()?;
    match value {
        None => Ok(None),
        Some(value) => Ok(Some(
            u64::try_from(value).map_err(|_| Error::Invariant("relay_seq"))?,
        )),
    }
}

/// 读一页日志：`after` 排他、升序、无洞；游标过期/回退分别 410/409。
pub fn page(
    conn: &ScopeConn,
    vault: &Vault,
    after: &Cursor,
    limit: u32,
    max_limit: u32,
) -> Result<LogPage> {
    let limit = limit.clamp(1, max_limit.max(1));
    let cursor_key = CursorKey::new(vault.cursor_key);
    cursor_key.check_against(after, vault)?;
    if after.last_seq < vault.gc_floor {
        return Err(Error::Api(
            ApiError::new(ErrorCode::CursorExpired)
                .epoch(vault.active_epoch)
                .hint("bootstrap"),
        ));
    }
    if after.last_seq > vault.head_seq {
        // head 低于已确认水位 = relay 侧日志回退；客户端必须显式 reconcile（ADR §5）。
        return Err(Error::Api(
            ApiError::new(ErrorCode::RelayReset)
                .epoch(vault.active_epoch)
                .hint("head_below_cursor"),
        ));
    }

    let mut stmt = conn.prepare(
        "SELECT relay_seq, kind, object_id, epoch, event, signature
         FROM log WHERE relay_seq > ?1 AND vault_id = ?3 ORDER BY relay_seq ASC LIMIT ?2",
    )?;
    let rows = stmt.query_map(
        (
            i64::try_from(after.last_seq).map_err(|_| Error::Invariant("cursor seq"))?,
            i64::from(limit),
            conn.scope().as_slice(),
        ),
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<Vec<u8>>>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, Vec<u8>>(4)?,
                row.get::<_, Option<Vec<u8>>>(5)?,
            ))
        },
    )?;
    let mut events = Vec::new();
    let mut expected = after.last_seq + 1;
    for row in rows {
        let (seq, kind, object_id, epoch, event, signature) = row?;
        let seq = u64::try_from(seq).map_err(|_| Error::Invariant("log relay_seq"))?;
        // 洞检测：`>= gc_floor` 的日志前缀必须连续。出现洞说明 writer/GC 不变量被破坏。
        if seq != expected {
            return Err(Error::Invariant("log prefix has a gap"));
        }
        expected = seq + 1;
        events.push(LogEvent {
            relay_seq: seq,
            kind: LogKind::parse(&kind).ok_or(Error::Invariant("log.kind"))?,
            object_id: match object_id {
                None => None,
                Some(bytes) => Some(
                    bytes
                        .as_slice()
                        .try_into()
                        .map_err(|_| Error::Invariant("log.object_id"))?,
                ),
            },
            epoch: u64::try_from(epoch).map_err(|_| Error::Invariant("log.epoch"))?,
            event,
            signature,
        });
    }

    let through_seq = events.last().map(|e| e.relay_seq).unwrap_or(after.last_seq);
    let next_cursor = cursor_key.encode(&cursor::for_vault(vault, through_seq)?)?;
    Ok(LogPage {
        events,
        next_cursor,
        through_seq,
        query_head: vault.head_seq,
        epoch: vault.active_epoch,
        gc_floor: vault.gc_floor,
    })
}

/// 删除 `relay_seq <= floor` 的日志前缀（GC 内部使用；调用方负责 gc_floor 的推进）。
pub fn delete_prefix(conn: &ScopeConn, floor: u64) -> Result<u64> {
    let changed = conn.execute(
        "DELETE FROM log WHERE relay_seq <= ?1 AND vault_id = ?",
        (
            i64::try_from(floor).map_err(|_| Error::Invariant("gc floor"))?,
            conn.scope().as_slice(),
        ),
    )?;
    Ok(changed as u64)
}

/// GC 后对象是否还被日志引用（用于判断"无根引用的 update 对象"）。
pub fn object_logged(conn: &ScopeConn, object_id: &Hash32) -> Result<bool> {
    let value: Option<i64> = conn
        .query_row(
            "SELECT relay_seq FROM log WHERE object_id = ?1 AND vault_id = ? LIMIT 1",
            (object_id.as_slice(), conn.scope().as_slice()),
            |row| row.get(0),
        )
        .optional()?;
    Ok(value.is_some())
}

/// 编码一个只含 seq 的游标（测试/诊断）。
pub fn cursor_at(vault: &Vault, seq: u64) -> Result<String> {
    let cursor = cursor::for_vault(vault, seq)?;
    CursorKey::new(vault.cursor_key).encode(&cursor)
}

/// 未使用但保持 wire 稳定：BE64 的唯一入口（避免散落 `to_be_bytes`）。
pub fn encode_seq(seq: u64) -> [u8; 8] {
    be64(seq)
}

/// 诊断：全部事件的 `(seq, kind)`。
pub fn timeline(conn: &ScopeConn) -> Result<Vec<(u64, LogKind)>> {
    let mut stmt =
        conn.prepare("SELECT relay_seq, kind FROM log WHERE vault_id = ?1 ORDER BY relay_seq")?;
    let rows = stmt.query_map([conn.scope().as_slice()], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (seq, kind) = row?;
        out.push((
            u64::try_from(seq).map_err(|_| Error::Invariant("relay_seq"))?,
            LogKind::parse(&kind).ok_or(Error::Invariant("log.kind"))?,
        ));
    }
    Ok(out)
}
