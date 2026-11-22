//! HTTP 适配层（axum）：控制面 **`application/cbor` + CDE**，对象分片 `application/octet-stream`。
//!
//! Wire 约定（ADR 0006 §5/§15，`sync-proto::cde` 负责 canonical 编解码）：
//! - 所有控制面请求/响应 = 整数键 CDE map，键从 0 连续编号、按键升序；
//! - ID/hash/签名 = byte string（16B/32B/64B）；文本字段 = text；optional 字段**省略**；
//! - 签名包装 = `{0: Bytes(unsigned_body), 1: Bytes(signature)}`，`unsigned_body` 自身是 CDE 记录；
//! - 错误体 = `{0: code(text), 1: retryable(bool), 2: current_epoch?, 3: current_control_rev?,
//!   4: bootstrap_hint?}`；
//! - 路径/查询里的 ID 接受 32/64 位十六进制（含 UUID 连字符）或 base64url，两种都规范化到原始字节；
//! - WS `/wake` 仍用 ≤512B 小 JSON（`{v,head_seq,epoch,control_rev}`，ADR §5）。
//!
//! 事务边界：handler 只在 `db.write(|conn| 同步核心调用)` 里写，**不跨 await 持写事务**。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{DefaultBodyLimit, OriginalUri, Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post, put};
use axum::Router;

use crate::auth::{self, AuthContext, Realm};
use crate::checkpoints::{self, PublishRequest};
use crate::config::RelayConfig;
use crate::db::Db;
use crate::envelope::MAX_ASSET_ENVELOPE;
use crate::error::{ApiError, Error, ErrorCode, Result};
use crate::gc;
use crate::ids::{b64url_decode, hex_lower, sha256, Hash32, Id16};
use crate::objects;
use crate::pairing;
use crate::refs::RefRole;
use crate::rotations::{self, AbortRequest, ActivateRequest, BeginRequest, GrantInput, NewManager};
use crate::sig::SignatureHook;
use crate::slots::{self, SlotKind};
use crate::vault::{self, Vault};
use crate::wire;
use crate::wire::{bytes as cbor_bytes, uint as cbor_uint};

/// 路由共享状态。
pub struct AppState {
    pub db: Db,
    pub config: Arc<RelayConfig>,
    pub hook: Arc<dyn SignatureHook>,
    /// 唤醒通道：`/wake` 订阅它；吊销/新 head/epoch 变化时广播（消息可丢，不作为可靠投递）。
    wake: tokio::sync::broadcast::Sender<WakeEvent>,
}

/// 唤醒事件（只表示"去看一眼"，不携带业务数据）。
#[derive(Debug, Clone, Copy)]
pub enum WakeEvent {
    /// head/epoch/control_rev 或鉴权状态可能变了。
    Changed,
}

impl AppState {
    /// 组装状态。
    pub fn new(db: Db, config: Arc<RelayConfig>, hook: Arc<dyn SignatureHook>) -> Arc<Self> {
        let (wake, _) = tokio::sync::broadcast::channel(256);
        Arc::new(Self {
            db,
            config,
            hook,
            wake,
        })
    }

    /// 广播一次"变了"（WS 收到后自行重新鉴权并读取最新状态）。
    pub fn notify(&self) {
        let _ = self.wake.send(WakeEvent::Changed);
    }

    fn write<T, F>(&self, f: F) -> impl std::future::Future<Output = Result<T>>
    where
        T: Send + 'static,
        F: FnOnce(&rusqlite::Connection, &dyn crate::time::Clock) -> Result<T> + Send + 'static,
    {
        let db = self.db.clone();
        let clock = db.clock_handle();
        async move { db.write(move |conn| f(conn, clock.as_ref())).await }
    }
}

/// 构建路由。
pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/v1/vaults", post(create_vault))
        .route("/v1/auth/challenges", post(create_challenge))
        .route("/v1/auth/tokens", post(issue_token))
        .route("/v1/vaults/{vault}/state", get(get_state))
        .route("/v1/vaults/{vault}/control", get(get_control))
        .route("/v1/vaults/{vault}/log", get(get_log))
        .route("/v1/vaults/{vault}/acks", post(post_acks))
        .route("/v1/vaults/{vault}/updates", post(post_updates))
        .route(
            "/v1/vaults/{vault}/objects/{object}/parts/{part}",
            put(put_part).get(get_part),
        )
        .route("/v1/vaults/{vault}/objects/{object}", get(get_object))
        .route(
            "/v1/vaults/{vault}/objects/{object}/complete",
            post(complete_object),
        )
        .route("/v1/vaults/{vault}/objects", get(list_objects))
        .route("/v1/vaults/{vault}/checkpoints", post(create_checkpoint))
        .route(
            "/v1/vaults/{vault}/checkpoints/{checkpoint}/refs",
            post(add_checkpoint_refs),
        )
        .route(
            "/v1/vaults/{vault}/checkpoints/{checkpoint}/publish",
            post(publish_checkpoint),
        )
        .route("/v1/vaults/{vault}/bootstrap", post(bootstrap))
        .route(
            "/v1/vaults/{vault}/bootstrap-leases/{lease}",
            patch(renew_lease),
        )
        .route(
            "/v1/vaults/{vault}/devices/self/grants/latest",
            get(latest_grant),
        )
        .route("/v1/vaults/{vault}/invites", post(create_invite))
        .route(
            "/v1/vaults/{vault}/invites/{invite}/redeem",
            post(redeem_invite),
        )
        .route(
            "/v1/vaults/{vault}/devices/{device}/approve",
            post(approve_device),
        )
        .route("/v1/vaults/{vault}/key-slots", get(list_key_slots))
        .route("/v1/vaults/{vault}/key-slots/{slot}", put(put_key_slot))
        .route(
            "/v1/vaults/{vault}/key-slots/{kind}/activate",
            post(activate_key_slot),
        )
        .route("/v1/vaults/{vault}/rotations", post(begin_rotation))
        .route("/v1/vaults/{vault}/rotations/{rotation}", get(get_rotation))
        .route(
            "/v1/vaults/{vault}/rotations/{rotation}/grants/{device}",
            put(stage_grant),
        )
        .route(
            "/v1/vaults/{vault}/rotations/{rotation}/activate",
            post(activate_rotation),
        )
        .route(
            "/v1/vaults/{vault}/rotations/{rotation}/abort",
            post(abort_rotation),
        )
        .route("/v1/vaults/{vault}/wake", get(wake))
        .layer(DefaultBodyLimit::max(MAX_ASSET_ENVELOPE))
        .with_state(state)
}

// ---------------------------------------------------------------- 响应/解析辅助

fn cbor(fields: wire::Fields, status: StatusCode) -> Response {
    match wire::encode(fields) {
        Ok(body) => (status, [(header::CONTENT_TYPE, wire::CONTENT_TYPE)], body).into_response(),
        Err(err) => err.into_response(),
    }
}

fn cbor_ok(fields: wire::Fields) -> Response {
    cbor(fields, StatusCode::OK)
}

fn cbor_created(fields: wire::Fields) -> Response {
    cbor(fields, StatusCode::CREATED)
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let (status, api) = match self {
            Error::Api(api) => (
                StatusCode::from_u16(api.code.status())
                    .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                api,
            ),
            Error::WriterClosed => (
                StatusCode::SERVICE_UNAVAILABLE,
                ApiError {
                    code: ErrorCode::Internal,
                    retryable: true,
                    current_epoch: None,
                    current_control_rev: None,
                    bootstrap_hint: None,
                },
            ),
            Error::Base64 | Error::Invalid(_) => (
                StatusCode::BAD_REQUEST,
                ApiError {
                    code: ErrorCode::InvalidRequest,
                    retryable: false,
                    current_epoch: None,
                    current_control_rev: None,
                    bootstrap_hint: None,
                },
            ),
            _ => (
                StatusCode::INTERNAL_SERVER_ERROR,
                ApiError {
                    code: ErrorCode::Internal,
                    retryable: true,
                    current_epoch: None,
                    current_control_rev: None,
                    bootstrap_hint: None,
                },
            ),
        };
        (
            status,
            [(header::CONTENT_TYPE, wire::CONTENT_TYPE)],
            wire::error_body(&api),
        )
            .into_response()
    }
}

fn parse_body(state: &AppState, body: &Bytes) -> Result<wire::Fields> {
    if body.len() > state.config.max_control_body {
        return Err(Error::api(ErrorCode::PayloadTooLarge));
    }
    wire::decode(body)
}

fn is_hex(text: &str) -> bool {
    !text.is_empty() && text.len().is_multiple_of(2) && text.bytes().all(|b| b.is_ascii_hexdigit())
}

fn hex_decode(text: &str) -> Option<Vec<u8>> {
    if !is_hex(text) {
        return None;
    }
    let mut out = Vec::with_capacity(text.len() / 2);
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let high = (bytes[index] as char).to_digit(16)?;
        let low = (bytes[index + 1] as char).to_digit(16)?;
        out.push(((high << 4) | low) as u8);
        index += 2;
    }
    Some(out)
}

/// 路径里的 ID：接受十六进制（可带 UUID 连字符）或 base64url。
fn path_bytes(value: &str) -> Result<Vec<u8>> {
    let compact: String = value.chars().filter(|c| *c != '-').collect();
    if let Some(bytes) = hex_decode(&compact) {
        return Ok(bytes);
    }
    b64url_decode(value)
}

fn parse_id16(value: &str) -> Result<Id16> {
    path_bytes(value)?
        .as_slice()
        .try_into()
        .map_err(|_| Error::api(ErrorCode::InvalidRequest))
}

fn parse_hash32(value: &str) -> Result<Hash32> {
    path_bytes(value)?
        .as_slice()
        .try_into()
        .map_err(|_| Error::api(ErrorCode::InvalidRequest))
}

fn bearer(headers: &HeaderMap) -> Result<String> {
    let value = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| Error::api(ErrorCode::Unauthorized))?;
    let token = value
        .strip_prefix("Bearer ")
        .ok_or_else(|| Error::api(ErrorCode::Unauthorized))?;
    if token.is_empty() {
        return Err(Error::api(ErrorCode::Unauthorized));
    }
    Ok(token.to_string())
}

fn authenticate(state: &AppState, headers: &HeaderMap) -> Result<AuthContext> {
    let token = bearer(headers)?;
    let clock = state.db.clock_handle();
    state
        .db
        .read(move |conn| auth::authenticate(conn, clock.as_ref(), &token))
}

fn require_device(state: &AppState, headers: &HeaderMap) -> Result<AuthContext> {
    let ctx = authenticate(state, headers)?;
    ctx.device_id()?;
    Ok(ctx)
}

fn require_management_realm(state: &AppState, headers: &HeaderMap) -> Result<AuthContext> {
    let ctx = authenticate(state, headers)?;
    ctx.require_management()?;
    Ok(ctx)
}

fn require_vault(conn: &rusqlite::Connection, vault_id: &Id16) -> Result<Vault> {
    let vault = vault::load(conn)?;
    if vault.vault_id != *vault_id {
        return Err(Error::api(ErrorCode::NotFound));
    }
    Ok(vault)
}

// ---------------------------------------------------------------- vault init

async fn create_vault(State(state): State<Arc<AppState>>, body: Bytes) -> Result<Response> {
    let fields = parse_body(&state, &body)?;
    let request = vault::InitRequest {
        vault_id: wire::id16_field(&fields, 0)?,
        device_id: wire::id16_field(&fields, 1)?,
        sign_pk: wire::key32_field(&fields, 2)?,
        hpke_pk: wire::key32_field(&fields, 3)?,
        manager_pub: wire::key32_field(&fields, 4)?,
        setup_secret: wire::text_field(&fields, 5)?,
        certificate: wire::bytes_field(&fields, 6)?,
        certificate_signature: wire::bytes_field(&fields, 7)?,
    };
    let config = state.config.clone();
    let hook = state.hook.clone();
    let (outcome, vault) = state
        .write(move |conn, clock| {
            let outcome = vault::initialize(conn, clock, &config, hook.as_ref(), &request)?;
            let vault = vault::load(conn)?;
            Ok((outcome, vault))
        })
        .await?;
    // 键 5：seq=0 的 HMAC 游标。客户端 genesis 后写入 `sync.relay_cursor`。
    let cursor = crate::log::cursor_at(&vault, 0)?;
    Ok(cbor_created(vec![
        (0, cbor_bytes(outcome.instance_id)),
        (1, cbor_bytes(outcome.log_id)),
        (2, cbor_uint(outcome.epoch)),
        (3, cbor_uint(outcome.control_rev)),
        (4, cbor_uint(outcome.auth_rev)),
        (5, wire::text(cursor)),
    ]))
}

// ---------------------------------------------------------------- auth

fn realm_tag(realm: Realm) -> u64 {
    match realm {
        Realm::Device => 1,
        Realm::Management => 2,
    }
}

async fn create_challenge(State(state): State<Arc<AppState>>, body: Bytes) -> Result<Response> {
    let fields = parse_body(&state, &body)?;
    let realm = match wire::uint_field(&fields, 0)? {
        1 => Realm::Device,
        2 => Realm::Management,
        _ => return Err(Error::api(ErrorCode::InvalidRequest)),
    };
    let subject = wire::id16_field(&fields, 1)?;
    let (challenge, vault, now) = state
        .write(move |conn, clock| {
            let challenge = auth::create_challenge(conn, clock, realm, &subject)?;
            let vault = vault::load(conn)?;
            Ok((challenge, vault, clock.now_secs()))
        })
        .await?;
    Ok(cbor_ok(vec![
        (0, cbor_bytes(challenge.challenge_id)),
        (1, cbor_uint(realm_tag(realm))),
        (2, cbor_bytes(challenge.subject)),
        (3, cbor_bytes(challenge.nonce)),
        (
            4,
            cbor_uint(u64::try_from(challenge.expires_at).unwrap_or(0)),
        ),
        (5, cbor_bytes(vault.vault_id)),
        (6, cbor_bytes(vault.instance_id)),
        (7, cbor_uint(u64::try_from(now).unwrap_or(0))),
    ]))
}

async fn issue_token(State(state): State<Arc<AppState>>, body: Bytes) -> Result<Response> {
    let fields = parse_body(&state, &body)?;
    let challenge_id = wire::id16_field(&fields, 0)?;
    let message = wire::bytes_field(&fields, 1)?;
    let signature = wire::bytes_field(&fields, 2)?;
    let hook = state.hook.clone();
    let outcome = state
        .write(move |conn, clock| {
            auth::issue_token(
                conn,
                clock,
                hook.as_ref(),
                &challenge_id,
                &message,
                &signature,
            )
        })
        .await?;
    Ok(cbor_ok(vec![
        (0, wire::text(outcome.token)),
        (1, cbor_uint(u64::try_from(outcome.expires_at).unwrap_or(0))),
        (
            2,
            cbor_uint(u64::try_from(outcome.server_time).unwrap_or(0)),
        ),
        (3, cbor_uint(realm_tag(outcome.realm))),
        (4, cbor_uint(outcome.auth_rev)),
    ]))
}

// ---------------------------------------------------------------- state / control / log

async fn get_state(
    State(state): State<Arc<AppState>>,
    Path(vault_id): Path<String>,
    headers: HeaderMap,
) -> Result<Response> {
    let vault_id = parse_id16(&vault_id)?;
    authenticate(&state, &headers)?;
    let clock = state.db.clock_handle();
    let payload = state.db.read(move |conn| {
        let vault = require_vault(conn, &vault_id)?;
        let usage = crate::quota::usage(conn)?;
        let available = crate::quota::maintenance_available(conn, clock.now_secs())?;
        Ok((vault, usage, available))
    })?;
    let (vault, usage, available) = payload;
    let mut fields = vec![
        (0, cbor_bytes(vault.vault_id)),
        (1, cbor_bytes(vault.log_id)),
        (2, cbor_bytes(vault.instance_id)),
        (3, wire::text(vault.state.as_str())),
        (4, cbor_uint(vault.active_epoch)),
        (5, cbor_uint(vault.control_rev)),
        (6, cbor_uint(vault.auth_rev)),
        (7, cbor_uint(vault.head_seq)),
        (8, cbor_uint(vault.gc_floor)),
        (9, cbor_uint(u64::try_from(usage.normal_bytes).unwrap_or(0))),
        (
            10,
            cbor_uint(u64::try_from(usage.maintenance_bytes).unwrap_or(0)),
        ),
        (
            11,
            cbor_uint(u64::try_from(vault.normal_limit).unwrap_or(0)),
        ),
        (
            12,
            cbor_uint(u64::try_from(vault.maintenance_limit).unwrap_or(0)),
        ),
        (13, cbor_uint(u64::try_from(available).unwrap_or(0))),
        (14, wire::text(crate::log::cursor_at(&vault, 0)?)),
    ];
    fields.sort_by_key(|(key, _)| *key);
    Ok(cbor_ok(fields))
}

async fn get_control(
    State(state): State<Arc<AppState>>,
    Path(vault_id): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Result<Response> {
    let vault_id = parse_id16(&vault_id)?;
    authenticate(&state, &headers)?;
    let after_rev: u64 = query
        .get("after_rev")
        .map(|value| value.parse::<u64>())
        .transpose()
        .map_err(|_| Error::api(ErrorCode::InvalidRequest))?
        .unwrap_or(0);
    let (vault, entries, members, slot_chain) = state.db.read(move |conn| {
        let vault = require_vault(conn, &vault_id)?;
        let mut stmt = conn.prepare(
            "SELECT rev, prev_hash, record_hash, signed_record FROM control_commits
             WHERE rev > ?1 ORDER BY rev ASC LIMIT 256",
        )?;
        let rows = stmt.query_map(
            [i64::try_from(after_rev).map_err(|_| Error::Invariant("after_rev"))?],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                ))
            },
        )?;
        let mut entries = Vec::new();
        for row in rows {
            let (rev, prev, record, signed) = row?;
            entries.push(wire::map(vec![
                (
                    0,
                    cbor_uint(u64::try_from(rev).map_err(|_| Error::Invariant("rev"))?),
                ),
                (1, cbor_bytes(prev)),
                (2, cbor_bytes(record)),
                (3, cbor_bytes(signed)),
            ]));
        }
        // 成员证书：客户端验 ObjectCommit 需要生产者当前证书（按 device_id 索引）。
        let mut member_stmt = conn.prepare(
            "SELECT device_id, certificate, status, sign_pk, hpke_pk, certificate_signature
             FROM devices ORDER BY device_id ASC",
        )?;
        let member_rows = member_stmt.query_map([], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Vec<u8>>(3)?,
                row.get::<_, Vec<u8>>(4)?,
                row.get::<_, Option<Vec<u8>>>(5)?,
            ))
        })?;
        let mut members = Vec::new();
        for row in member_rows {
            let (device_id, certificate, status, sign_pk, hpke_pk, certificate_signature) = row?;
            let mut member = vec![
                (0, cbor_bytes(device_id)),
                (1, cbor_bytes(certificate)),
                (2, wire::text(status)),
                (3, cbor_bytes(sign_pk)),
                (4, cbor_bytes(hpke_pk)),
            ];
            if let Some(signature) = certificate_signature {
                member.push((5, cbor_bytes(signature)));
            }
            member.sort_by_key(|(key, _)| *key);
            members.push(wire::map(member));
        }
        // 槽控制链：只暴露描述符/状态/版本（密文本体走对象端点）。
        let mut slot_stmt = conn.prepare(
            "SELECT slot_id, epoch, kind, descriptor, state, slot_set_rev FROM key_slots
             ORDER BY slot_set_rev ASC, kind ASC",
        )?;
        let slot_rows = slot_stmt.query_map([], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Vec<u8>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, i64>(5)?,
            ))
        })?;
        let mut slot_chain = Vec::new();
        for row in slot_rows {
            let (slot_id, epoch, kind, descriptor, state, rev) = row?;
            slot_chain.push(wire::map(vec![
                (0, cbor_bytes(slot_id)),
                (
                    1,
                    cbor_uint(u64::try_from(epoch).map_err(|_| Error::Invariant("epoch"))?),
                ),
                (2, wire::text(kind)),
                (3, cbor_bytes(descriptor)),
                (4, wire::text(state)),
                (
                    5,
                    cbor_uint(u64::try_from(rev).map_err(|_| Error::Invariant("slot rev"))?),
                ),
            ]));
        }
        Ok((vault, entries, members, slot_chain))
    })?;
    Ok(cbor_ok(vec![
        (0, wire::array(entries)),
        (1, cbor_uint(vault.control_rev)),
        (2, cbor_uint(vault.active_epoch)),
        (3, wire::array(members)),
        (4, wire::array(slot_chain)),
    ]))
}

async fn get_log(
    State(state): State<Arc<AppState>>,
    Path(vault_id): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Result<Response> {
    let vault_id = parse_id16(&vault_id)?;
    require_device(&state, &headers)?;
    let token = query
        .get("after")
        .cloned()
        .ok_or_else(|| Error::api(ErrorCode::InvalidRequest))?;
    let limit: u32 = query
        .get("limit")
        .map(|value| value.parse::<u32>())
        .transpose()
        .map_err(|_| Error::api(ErrorCode::InvalidRequest))?
        .unwrap_or(256);
    let max_limit = state.config.max_log_page;
    let page = state.db.read(move |conn| {
        let vault = require_vault(conn, &vault_id)?;
        let cursor = crate::cursor::CursorKey::new(vault.cursor_key).decode(&token)?;
        crate::log::page(conn, &vault, &cursor, limit, max_limit)
    })?;
    let events = page
        .events
        .into_iter()
        .map(|event| {
            let mut fields = vec![
                (0, cbor_uint(event.relay_seq)),
                (1, wire::text(event.kind.as_str())),
                (3, cbor_uint(event.epoch)),
                (4, cbor_bytes(event.event)),
            ];
            if let Some(object_id) = event.object_id {
                fields.push((2, cbor_bytes(object_id)));
            }
            if let Some(signature) = &event.signature {
                fields.push((5, cbor_bytes(signature.clone())));
            }
            fields.sort_by_key(|(key, _)| *key);
            wire::map(fields)
        })
        .collect();
    Ok(cbor_ok(vec![
        (0, wire::array(events)),
        (1, wire::text(page.next_cursor)),
        (2, cbor_uint(page.through_seq)),
        (3, cbor_uint(page.query_head)),
        (4, cbor_uint(page.epoch)),
        (5, cbor_uint(page.gc_floor)),
    ]))
}

// ---------------------------------------------------------------- acks / updates

async fn post_acks(
    State(state): State<Arc<AppState>>,
    Path(vault_id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response> {
    let vault_id = parse_id16(&vault_id)?;
    let actor = require_device(&state, &headers)?;
    let fields = parse_body(&state, &body)?;
    let received_seq = wire::uint_field(&fields, 0)?;
    let integrated_seq = wire::uint_field(&fields, 1)?;
    let epoch = wire::uint_field(&fields, 2)?;
    state
        .write(move |conn, clock| {
            let vault = require_vault(conn, &vault_id)?;
            if received_seq < integrated_seq {
                return Err(Error::api(ErrorCode::InvalidRequest));
            }
            if epoch != vault.active_epoch {
                return Err(Error::Api(
                    ApiError::new(ErrorCode::EpochMismatch).epoch(vault.active_epoch),
                ));
            }
            let device_id = actor.device_id()?;
            let existing: Option<(i64, i64)> = conn
                .query_row(
                    "SELECT received_seq, integrated_seq FROM device_acks WHERE device_id = ?1",
                    [device_id.as_slice()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .ok();
            if let Some((received, integrated)) = existing {
                let received = u64::try_from(received).map_err(|_| Error::Invariant("ack"))?;
                let integrated = u64::try_from(integrated).map_err(|_| Error::Invariant("ack"))?;
                if received_seq < received || integrated_seq < integrated {
                    return Err(Error::api(ErrorCode::CursorBackward));
                }
            }
            let now = clock.now_secs();
            conn.execute(
                "INSERT INTO device_acks(device_id, received_seq, integrated_seq, epoch, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(device_id) DO UPDATE SET
                    received_seq = excluded.received_seq,
                    integrated_seq = excluded.integrated_seq,
                    epoch = excluded.epoch,
                    updated_at = excluded.updated_at",
                (
                    device_id.as_slice(),
                    i64::try_from(received_seq).map_err(|_| Error::Invariant("ack"))?,
                    i64::try_from(integrated_seq).map_err(|_| Error::Invariant("ack"))?,
                    i64::try_from(epoch).map_err(|_| Error::Invariant("epoch"))?,
                    now,
                ),
            )?;
            Ok(())
        })
        .await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn post_updates(
    State(state): State<Arc<AppState>>,
    Path(vault_id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response> {
    let vault_id = parse_id16(&vault_id)?;
    let actor = require_device(&state, &headers)?;
    let fields = parse_body(&state, &body)?;
    let items = wire::array_field(&fields, 0)?;
    if items.len() > 256 {
        return Err(Error::api(ErrorCode::PayloadTooLarge));
    }
    state.db.read(move |conn| {
        require_vault(conn, &vault_id)?;
        Ok(())
    })?;
    let mut results = Vec::with_capacity(items.len());
    let mut accepted_any = false;
    for item in items {
        let item_fields = wire::value_map(&item)?;
        let object_id = match wire::hash32_field(&item_fields, 0) {
            Ok(id) => id,
            Err(err) => {
                results.push(update_failure(&item_fields, err));
                continue;
            }
        };
        let commit = match wire::bytes_field(&item_fields, 1) {
            Ok(bytes) => bytes,
            Err(err) => {
                results.push(update_failure(&item_fields, err));
                continue;
            }
        };
        let signature = match wire::bytes_field(&item_fields, 2) {
            Ok(bytes) => bytes,
            Err(err) => {
                results.push(update_failure(&item_fields, err));
                continue;
            }
        };
        let hook = state.hook.clone();
        let outcome = state
            .write(move |conn, clock| {
                objects::complete(
                    conn,
                    clock,
                    hook.as_ref(),
                    &objects::CompleteUpload {
                        actor: &actor,
                        vault_id: &vault_id,
                        object_id: &object_id,
                        commit: &commit,
                        signature: &signature,
                    },
                )
            })
            .await;
        match outcome {
            Ok(outcome) => {
                accepted_any = true;
                let mut fields = vec![
                    (0, cbor_bytes(outcome.object_id)),
                    (
                        1,
                        wire::text(if outcome.duplicate {
                            "duplicate"
                        } else {
                            "accepted"
                        }),
                    ),
                    (4, wire::boolean(false)),
                ];
                if let Some(seq) = outcome.relay_seq {
                    fields.push((2, cbor_uint(seq)));
                }
                fields.sort_by_key(|(key, _)| *key);
                results.push(wire::map(fields));
            }
            Err(err) => {
                let (code, retryable) = error_code_and_retry(&err);
                let mut fields = vec![
                    (0, cbor_bytes(object_id)),
                    (1, wire::text("failed")),
                    (4, wire::boolean(retryable)),
                ];
                if let Some(code) = code {
                    fields.push((3, wire::text(code)));
                }
                fields.sort_by_key(|(key, _)| *key);
                results.push(wire::map(fields));
            }
        }
    }
    if accepted_any {
        state.notify();
    }
    Ok(cbor_ok(vec![(0, wire::array(results))]))
}

fn error_code_and_retry(err: &Error) -> (Option<&'static str>, bool) {
    match err {
        Error::Api(api) => (Some(api.code.as_str()), api.retryable),
        Error::SignatureRejected => (Some(ErrorCode::SignatureInvalid.as_str()), false),
        Error::Base64 | Error::Invalid(_) => (Some(ErrorCode::InvalidRequest.as_str()), false),
        _ => (Some(ErrorCode::Internal.as_str()), true),
    }
}

fn update_failure(fields: &wire::Fields, err: Error) -> wire::CdeValue {
    let (code, retryable) = error_code_and_retry(&err);
    let mut out = vec![
        (
            0,
            wire::bytes(wire::bytes_field(fields, 0).unwrap_or_default()),
        ),
        (1, wire::text("failed")),
        (4, wire::boolean(retryable)),
    ];
    if let Some(code) = code {
        out.push((3, wire::text(code)));
    }
    out.sort_by_key(|(key, _)| *key);
    wire::map(out)
}

// ---------------------------------------------------------------- objects

async fn put_part(
    State(state): State<Arc<AppState>>,
    Path((vault_id, object_id, part_no)): Path<(String, String, u32)>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response> {
    let vault_id = parse_id16(&vault_id)?;
    let object_id = parse_hash32(&object_id)?;
    let actor = require_device(&state, &headers)?;
    let reservation = match query.get("reservation") {
        None => None,
        Some(value) => Some(
            b64url_decode(value)?
                .as_slice()
                .try_into()
                .map_err(|_| Error::api(ErrorCode::InvalidRequest))?,
        ),
    };
    let envelope = body.to_vec();
    let outcome = state
        .write(move |conn, clock| {
            objects::put_part(
                conn,
                clock,
                &objects::PartUpload {
                    actor: &actor,
                    vault_id: &vault_id,
                    object_id: &object_id,
                    part_no,
                    envelope: &envelope,
                    reservation: reservation.as_ref(),
                },
            )
        })
        .await?;
    let status = if outcome.duplicate {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    Ok(cbor(
        vec![
            (0, cbor_bytes(outcome.object_id)),
            (1, cbor_uint(u64::from(outcome.part_no))),
            (2, cbor_bytes(outcome.digest)),
            (3, wire::boolean(outcome.duplicate)),
        ],
        status,
    ))
}

async fn get_part(
    State(state): State<Arc<AppState>>,
    Path((vault_id, object_id, part_no)): Path<(String, String, u32)>,
    headers: HeaderMap,
) -> Result<Response> {
    let vault_id = parse_id16(&vault_id)?;
    let object_id = parse_hash32(&object_id)?;
    require_device(&state, &headers)?;
    let (bytes, digest) = state.db.read(move |conn| {
        require_vault(conn, &vault_id)?;
        objects::read_part(conn, &object_id, part_no)
    })?;
    let etag = format!("\"{}\"", hex_lower(&digest));
    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_string()),
            (header::ETAG, etag),
        ],
        bytes,
    )
        .into_response())
}

async fn get_object(
    State(state): State<Arc<AppState>>,
    Path((vault_id, object_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response> {
    let vault_id = parse_id16(&vault_id)?;
    let object_id = parse_hash32(&object_id)?;
    require_device(&state, &headers)?;
    let view = state.db.read(move |conn| {
        require_vault(conn, &vault_id)?;
        objects::meta(conn, &object_id)
    })?;
    let parts = view
        .parts
        .iter()
        .map(|part| {
            wire::map(vec![
                (0, cbor_uint(u64::from(part.part_no))),
                (1, cbor_bytes(part.digest)),
                (2, cbor_uint(u64::try_from(part.len).unwrap_or(0))),
            ])
        })
        .collect();
    let object = view.object;
    let mut fields = vec![
        (0, cbor_bytes(object.object_id)),
        (1, cbor_bytes(object.message_id)),
        (2, cbor_bytes(object.producer)),
        (3, cbor_uint(object.epoch)),
        (4, cbor_uint(object.object_type.as_u8().into())),
        (5, cbor_uint(object.generation)),
        (6, cbor_uint(object.sender_seq)),
        (7, cbor_uint(u64::from(object.part_count))),
        (8, cbor_uint(object.plain_len)),
        (9, wire::text(object.state.as_str())),
        (10, wire::text(object.budget_class.as_str())),
        (12, wire::array(parts)),
    ];
    if let Some(digest) = object.digest {
        fields.push((11, cbor_bytes(digest)));
    }
    if let Some(completed_at) = object.completed_at {
        fields.push((13, cbor_uint(u64::try_from(completed_at).unwrap_or(0))));
    }
    if let Some(signature) = object.commit_signature {
        fields.push((14, cbor_bytes(signature)));
    }
    fields.sort_by_key(|(key, _)| *key);
    Ok(cbor_ok(fields))
}

async fn complete_object(
    State(state): State<Arc<AppState>>,
    Path((vault_id, object_id)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response> {
    let vault_id = parse_id16(&vault_id)?;
    let object_id = parse_hash32(&object_id)?;
    let actor = require_device(&state, &headers)?;
    let fields = parse_body(&state, &body)?;
    let commit = wire::bytes_field(&fields, 0)?;
    let signature = wire::bytes_field(&fields, 1)?;
    let hook = state.hook.clone();
    let outcome = state
        .write(move |conn, clock| {
            objects::complete(
                conn,
                clock,
                hook.as_ref(),
                &objects::CompleteUpload {
                    actor: &actor,
                    vault_id: &vault_id,
                    object_id: &object_id,
                    commit: &commit,
                    signature: &signature,
                },
            )
        })
        .await?;
    state.notify();
    let status = if outcome.duplicate {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    let mut fields = vec![
        (0, cbor_bytes(outcome.object_id)),
        (2, wire::boolean(outcome.duplicate)),
        (3, wire::text(outcome.state.as_str())),
    ];
    if let Some(seq) = outcome.relay_seq {
        fields.push((1, cbor_uint(seq)));
    }
    fields.sort_by_key(|(key, _)| *key);
    Ok(cbor(fields, status))
}

async fn list_objects(
    State(state): State<Arc<AppState>>,
    Path(vault_id): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Result<Response> {
    let vault_id = parse_id16(&vault_id)?;
    require_device(&state, &headers)?;
    if query.get("type").map(String::as_str) != Some("asset") {
        return Err(Error::api(ErrorCode::InvalidRequest));
    }
    let epoch: u64 = query
        .get("epoch")
        .map(|value| value.parse::<u64>())
        .transpose()
        .map_err(|_| Error::api(ErrorCode::InvalidRequest))?
        .unwrap_or(0);
    let through: u64 = query
        .get("through")
        .map(|value| value.parse::<u64>())
        .transpose()
        .map_err(|_| Error::api(ErrorCode::InvalidRequest))?
        .unwrap_or(state.db.read(vault::load)?.head_seq);
    let limit: u32 = query
        .get("limit")
        .map(|value| value.parse::<u32>())
        .transpose()
        .map_err(|_| Error::api(ErrorCode::InvalidRequest))?
        .unwrap_or(256);
    let after = match query.get("after") {
        None => None,
        Some(value) => Some(parse_hash32(value)?),
    };
    let refs = state.db.read(move |conn| {
        require_vault(conn, &vault_id)?;
        objects::list_assets(conn, epoch, through, after.as_ref(), limit)
    })?;
    let entries = refs
        .into_iter()
        .map(|meta| {
            wire::map(vec![
                (0, cbor_bytes(meta.object_id)),
                (1, cbor_uint(meta.object_type.as_u8().into())),
                (2, cbor_uint(meta.generation)),
                (3, cbor_uint(meta.key_epoch)),
                (4, cbor_uint(u64::from(meta.part_count))),
                (5, cbor_uint(meta.total_plain_len)),
                (6, cbor_bytes(meta.object_digest)),
            ])
        })
        .collect();
    Ok(cbor_ok(vec![(0, wire::array(entries))]))
}

// ---------------------------------------------------------------- checkpoints

async fn create_checkpoint(
    State(state): State<Arc<AppState>>,
    Path(vault_id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response> {
    let vault_id = parse_id16(&vault_id)?;
    let actor = require_device(&state, &headers)?;
    let fields = parse_body(&state, &body)?;
    let estimated_bytes = wire::uint_field(&fields, 0)?;
    let epoch = wire::opt_uint_field(&fields, 1)?;
    let estimated_bytes =
        i64::try_from(estimated_bytes).map_err(|_| Error::api(ErrorCode::InvalidRequest))?;
    let outcome = state
        .write(move |conn, clock| {
            require_vault(conn, &vault_id)?;
            checkpoints::create(conn, clock, &actor, estimated_bytes, epoch)
        })
        .await?;
    Ok(cbor_created(vec![
        (0, cbor_bytes(outcome.checkpoint_id)),
        (1, cbor_uint(outcome.epoch)),
        (2, cbor_uint(outcome.covered_seq)),
        (3, cbor_bytes(outcome.reservation_id)),
        (
            4,
            cbor_uint(u64::try_from(outcome.reservation_expires_at).unwrap_or(0)),
        ),
    ]))
}

async fn add_checkpoint_refs(
    State(state): State<Arc<AppState>>,
    Path((vault_id, checkpoint_id)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response> {
    let vault_id = parse_id16(&vault_id)?;
    let checkpoint_id = parse_id16(&checkpoint_id)?;
    require_device(&state, &headers)?;
    let fields = parse_body(&state, &body)?;
    let raw_refs = wire::array_field(&fields, 0)?;
    let max_refs = state.config.max_refs_per_request;
    let mut refs = Vec::with_capacity(raw_refs.len());
    for entry in raw_refs {
        let entry = wire::value_map(&entry)?;
        let role = RefRole::parse(&wire::text_field(&entry, 1)?)
            .ok_or_else(|| Error::api(ErrorCode::InvalidRequest))?;
        refs.push((wire::hash32_field(&entry, 0)?, role));
    }
    let inserted = state
        .write(move |conn, _clock| {
            require_vault(conn, &vault_id)?;
            checkpoints::add_refs(conn, max_refs, &checkpoint_id, &refs)
        })
        .await?;
    Ok(cbor_ok(vec![(
        0,
        cbor_uint(u64::try_from(inserted).unwrap_or(0)),
    )]))
}

async fn publish_checkpoint(
    State(state): State<Arc<AppState>>,
    Path((vault_id, checkpoint_id)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response> {
    let vault_id = parse_id16(&vault_id)?;
    let checkpoint_id = parse_id16(&checkpoint_id)?;
    let actor = require_device(&state, &headers)?;
    let fields = parse_body(&state, &body)?;
    let covered_seq = wire::uint_field(&fields, 0)?;
    let manifest_oid = wire::hash32_field(&fields, 1)?;
    let refs_digest = wire::hash32_field(&fields, 2)?;
    let signed_commit = wire::bytes_field(&fields, 3)?;
    let signature = wire::bytes_field(&fields, 4)?;
    let hook = state.hook.clone();
    let outcome = state
        .write(move |conn, clock| {
            require_vault(conn, &vault_id)?;
            checkpoints::publish(
                conn,
                clock,
                hook.as_ref(),
                &actor,
                &checkpoint_id,
                &PublishRequest {
                    covered_seq,
                    manifest_oid,
                    refs_digest,
                    signed_commit: &signed_commit,
                    signature: &signature,
                },
            )
        })
        .await?;
    state.notify();
    Ok(cbor_ok(vec![
        (0, cbor_bytes(outcome.checkpoint_id)),
        (1, cbor_uint(outcome.epoch)),
        (2, cbor_uint(outcome.covered_seq)),
        (
            3,
            cbor_uint(u64::try_from(outcome.published_at).unwrap_or(0)),
        ),
    ]))
}

fn checkpoint_fields(checkpoint: &checkpoints::CheckpointRow, cursor: String) -> wire::CdeValue {
    let mut fields = vec![
        (0, cbor_bytes(checkpoint.checkpoint_id)),
        (1, cbor_uint(checkpoint.epoch)),
        (2, cbor_uint(checkpoint.covered_seq)),
        (5, wire::text(cursor)),
    ];
    if let Some(manifest) = checkpoint.manifest_oid {
        fields.push((3, cbor_bytes(manifest)));
    }
    if let Some(digest) = checkpoint.refs_digest {
        fields.push((4, cbor_bytes(digest)));
    }
    fields.sort_by_key(|(key, _)| *key);
    wire::map(fields)
}

async fn bootstrap(
    State(state): State<Arc<AppState>>,
    Path(vault_id): Path<String>,
    headers: HeaderMap,
) -> Result<Response> {
    let vault_id = parse_id16(&vault_id)?;
    let actor = require_device(&state, &headers)?;
    let (vault, outcome) = state
        .write(move |conn, clock| {
            let vault = require_vault(conn, &vault_id)?;
            checkpoints::bootstrap(conn, clock, &actor).map(|outcome| (vault, outcome))
        })
        .await?;
    let primary_cursor = crate::log::cursor_at(&vault, outcome.primary.covered_seq)?;
    let fallback_cursor = crate::log::cursor_at(&vault, outcome.fallback.covered_seq)?;
    Ok(cbor_ok(vec![
        (0, checkpoint_fields(&outcome.primary, primary_cursor)),
        (1, checkpoint_fields(&outcome.fallback, fallback_cursor)),
        (2, cbor_uint(outcome.head_seq)),
        (3, cbor_uint(outcome.gc_floor)),
        (4, cbor_uint(outcome.epoch)),
        (5, cbor_bytes(outcome.lease.lease_id)),
        (
            6,
            cbor_uint(u64::try_from(outcome.lease.expires_at).unwrap_or(0)),
        ),
    ]))
}

async fn renew_lease(
    State(state): State<Arc<AppState>>,
    Path((vault_id, lease_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response> {
    let vault_id = parse_id16(&vault_id)?;
    let lease_id = parse_id16(&lease_id)?;
    let actor = require_device(&state, &headers)?;
    let lease = state
        .write(move |conn, clock| {
            require_vault(conn, &vault_id)?;
            checkpoints::renew_lease(conn, clock, &actor, &lease_id)
        })
        .await?;
    Ok(cbor_ok(vec![
        (0, cbor_bytes(lease.lease_id)),
        (1, cbor_uint(u64::try_from(lease.expires_at).unwrap_or(0))),
        (2, cbor_uint(lease.floor_seq)),
    ]))
}

// ---------------------------------------------------------------- 配对 / 设备 / key slots

#[derive(Debug)]
struct SignedBody {
    payload: Vec<u8>,
    payload_fields: wire::Fields,
    signature: Vec<u8>,
}

fn signed_body(fields: &wire::Fields, key: u64) -> Result<SignedBody> {
    let signed = wire::Signed::from_fields(fields, key)?;
    let payload_fields = signed.payload_fields()?;
    Ok(SignedBody {
        payload: signed.payload,
        payload_fields,
        signature: signed.signature,
    })
}

async fn create_invite(
    State(state): State<Arc<AppState>>,
    Path(vault_id): Path<String>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response> {
    let vault_id = parse_id16(&vault_id)?;
    let actor = require_management_realm(&state, &headers)?;
    let fields = parse_body(&state, &body)?;
    let signed = signed_body(&fields, 0)?;
    let request_id = wire::id16_field(&fields, 1)?;
    let expected_control_rev = wire::uint_field(&fields, 2)?;
    let token_hash = wire::hash32_field(&signed.payload_fields, 0)?;
    let expires_at = wire::opt_uint_field(&signed.payload_fields, 1)?
        .map(|value| i64::try_from(value).map_err(|_| Error::api(ErrorCode::InvalidRequest)))
        .transpose()?;
    let path = uri.path().to_string();
    let hook = state.hook.clone();
    let invite = state
        .write(move |conn, clock| {
            require_vault(conn, &vault_id)?;
            pairing::create_invite(
                conn,
                clock,
                hook.as_ref(),
                &actor,
                &pairing::InviteRequest {
                    token_hash,
                    requested_expires_at: expires_at,
                    expected_control_rev,
                    request_id,
                    path: path.clone(),
                    unsigned_body: signed.payload.clone(),
                    signature: signed.signature.clone(),
                },
            )
        })
        .await?;
    state.notify();
    Ok(cbor_created(vec![
        (0, cbor_bytes(invite.invite_id)),
        (1, cbor_uint(u64::try_from(invite.expires_at).unwrap_or(0))),
    ]))
}

async fn redeem_invite(
    State(state): State<Arc<AppState>>,
    Path((vault_id, invite_id)): Path<(String, String)>,
    body: Bytes,
) -> Result<Response> {
    let vault_id = parse_id16(&vault_id)?;
    let invite_id = parse_id16(&invite_id)?;
    let fields = parse_body(&state, &body)?;
    let token: [u8; 32] = wire::key32_field(&fields, 0)?;
    let device_id = wire::id16_field(&fields, 1)?;
    let sign_pk = wire::key32_field(&fields, 2)?;
    let hpke_pk = wire::key32_field(&fields, 3)?;
    let proof = wire::bytes_field(&fields, 4)?;
    let proof_signature = wire::bytes_field(&fields, 5)?;
    let device = pairing::NewDevice {
        device_id,
        sign_pk,
        hpke_pk,
        proof,
        proof_signature,
    };
    let hook = state.hook.clone();
    let outcome = state
        .write(move |conn, clock| {
            require_vault(conn, &vault_id)?;
            pairing::redeem_invite(conn, clock, hook.as_ref(), &invite_id, &token, &device)
        })
        .await?;
    state.notify();
    Ok(cbor_created(vec![
        (0, cbor_bytes(outcome.pending_device)),
        (1, wire::text(outcome.fingerprint)),
    ]))
}

async fn approve_device(
    State(state): State<Arc<AppState>>,
    Path((vault_id, device_id)): Path<(String, String)>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response> {
    let vault_id = parse_id16(&vault_id)?;
    let device_id = parse_id16(&device_id)?;
    let actor = require_management_realm(&state, &headers)?;
    let fields = parse_body(&state, &body)?;
    let signed = signed_body(&fields, 0)?;
    let request_id = wire::id16_field(&fields, 1)?;
    let expected_control_rev = wire::uint_field(&fields, 2)?;
    let certificate = wire::bytes_field(&fields, 3)?;
    let certificate_signature = wire::bytes_field(&fields, 4)?;
    let grant_id = wire::id16_field(&fields, 5)?;
    let hpke_enc = wire::key32_field(&fields, 6)?;
    let hpke_ciphertext = wire::bytes_field(&fields, 7)?;
    let grant_signature = wire::bytes_field(&fields, 8)?;
    let request = pairing::ApproveRequest {
        certificate,
        certificate_signature,
        grant_id,
        hpke_enc,
        hpke_ciphertext,
        grant_signature,
        expected_control_rev,
        request_id,
        path: uri.path().to_string(),
        unsigned_body: signed.payload,
        management_signature: signed.signature,
    };
    let hook = state.hook.clone();
    let outcome = state
        .write(move |conn, clock| {
            require_vault(conn, &vault_id)?;
            pairing::approve_device(conn, clock, hook.as_ref(), &actor, &device_id, &request)
        })
        .await?;
    state.notify();
    Ok(cbor_ok(vec![
        (0, cbor_bytes(outcome.device_id)),
        (1, wire::text(outcome.fingerprint)),
        (2, cbor_uint(outcome.epoch)),
        (3, cbor_uint(outcome.control_rev)),
        (4, cbor_bytes(outcome.certificate_signature)),
    ]))
}

fn slot_fields(slot: &slots::SlotRow) -> wire::CdeValue {
    let mut fields = vec![
        (0, cbor_bytes(slot.slot_id)),
        (1, cbor_uint(slot.epoch)),
        (2, wire::text(slot.kind.as_str())),
        (3, cbor_bytes(slot.descriptor.clone())),
        (4, wire::text(slot.state.as_str())),
        (5, cbor_uint(slot.slot_set_rev)),
    ];
    if let Some(retire_at) = slot.retire_at {
        fields.push((6, cbor_uint(u64::try_from(retire_at).unwrap_or(0))));
    }
    fields.sort_by_key(|(key, _)| *key);
    wire::map(fields)
}

async fn list_key_slots(
    State(state): State<Arc<AppState>>,
    Path(vault_id): Path<String>,
    headers: HeaderMap,
) -> Result<Response> {
    let vault_id = parse_id16(&vault_id)?;
    require_device(&state, &headers)?;
    let clock = state.db.clock_handle();
    let rows = state.db.read(move |conn| {
        require_vault(conn, &vault_id)?;
        slots::list_visible(conn, clock.now_secs())
    })?;
    Ok(cbor_ok(vec![(
        0,
        wire::array(rows.iter().map(slot_fields).collect()),
    )]))
}

async fn put_key_slot(
    State(state): State<Arc<AppState>>,
    Path((vault_id, slot_id)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response> {
    let vault_id = parse_id16(&vault_id)?;
    let slot_id = parse_hash32(&slot_id)?;
    let actor = require_device(&state, &headers)?;
    let fields = parse_body(&state, &body)?;
    let epoch = wire::uint_field(&fields, 0)?;
    let kind = SlotKind::parse(&wire::text_field(&fields, 1)?)
        .ok_or_else(|| Error::api(ErrorCode::InvalidRequest))?;
    let descriptor = wire::bytes_field(&fields, 2)?;
    let signed = signed_body(&fields, 3)?;
    let hook = state.hook.clone();
    let row = state
        .write(move |conn, clock| {
            require_vault(conn, &vault_id)?;
            slots::put(
                conn,
                clock,
                hook.as_ref(),
                &actor,
                &slots::SlotPutRequest {
                    slot_id,
                    epoch,
                    kind,
                    descriptor,
                    signed_message: signed.payload,
                    signature: signed.signature,
                },
            )
        })
        .await?;
    state.notify();
    Ok(cbor_created(vec![(0, slot_fields(&row))]))
}

async fn activate_key_slot(
    State(state): State<Arc<AppState>>,
    Path((vault_id, kind)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response> {
    let vault_id = parse_id16(&vault_id)?;
    if kind != SlotKind::Password.as_str() {
        return Err(Error::api(ErrorCode::InvalidRequest));
    }
    let actor = require_device(&state, &headers)?;
    let fields = parse_body(&state, &body)?;
    let expected_slot_set_rev = wire::uint_field(&fields, 0)?;
    let new_slot_id = wire::hash32_field(&fields, 1)?;
    let signed = signed_body(&fields, 2)?;
    let hook = state.hook.clone();
    let rev = state
        .write(move |conn, clock| {
            require_vault(conn, &vault_id)?;
            slots::activate_password(
                conn,
                clock,
                hook.as_ref(),
                &actor,
                &slots::PasswordActivateRequest {
                    expected_slot_set_rev,
                    new_slot_id,
                    signed_message: signed.payload,
                    signature: signed.signature,
                },
            )
        })
        .await?;
    state.notify();
    Ok(cbor_ok(vec![(0, cbor_uint(rev))]))
}

// ---------------------------------------------------------------- rotations

fn rotation_fields(view: &rotations::RotationView) -> wire::CdeValue {
    let mut fields = vec![
        (0, cbor_bytes(view.rotation.rotation_id)),
        (1, wire::text(view.rotation.state.as_str())),
        (2, cbor_uint(view.rotation.from_epoch)),
        (3, cbor_uint(view.rotation.to_epoch)),
        (4, cbor_uint(view.rotation.freeze_seq)),
        (5, cbor_uint(view.rotation.revoked_count)),
        (7, cbor_uint(view.grants_count)),
        (8, cbor_bytes(view.grants_digest)),
        (9, cbor_uint(view.published_checkpoints)),
    ];
    if let Some(hash) = view.rotation.member_set_hash {
        fields.push((6, cbor_bytes(hash)));
    }
    fields.sort_by_key(|(key, _)| *key);
    wire::map(fields)
}

async fn begin_rotation(
    State(state): State<Arc<AppState>>,
    Path(vault_id): Path<String>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response> {
    let vault_id = parse_id16(&vault_id)?;
    let actor = require_management_realm(&state, &headers)?;
    let fields = parse_body(&state, &body)?;
    let signed = signed_body(&fields, 0)?;
    let request_id = wire::id16_field(&fields, 1)?;
    let expected_control_rev = wire::uint_field(&fields, 2)?;
    let expected_epoch = wire::uint_field(&fields, 3)?;
    let revoke: Vec<Id16> = wire::array_field(&fields, 4)?
        .into_iter()
        .map(|value| match value {
            wire::CdeValue::Bytes(bytes) => bytes
                .as_slice()
                .try_into()
                .map_err(|_| Error::api(ErrorCode::InvalidRequest)),
            _ => Err(Error::api(ErrorCode::InvalidRequest)),
        })
        .collect::<Result<Vec<_>>>()?;
    let path = uri.path().to_string();
    let hook = state.hook.clone();
    let view = state
        .write(move |conn, clock| {
            require_vault(conn, &vault_id)?;
            rotations::begin(
                conn,
                clock,
                hook.as_ref(),
                &actor,
                &BeginRequest {
                    revoke: &revoke,
                    expected_epoch,
                    expected_control_rev,
                    request_id,
                    path: &path,
                    body_sha256: sha256(&signed.payload),
                    signed: &signed.payload,
                    signature: &signed.signature,
                },
            )
        })
        .await?;
    state.notify();
    Ok(cbor_ok(vec![(0, rotation_fields(&view))]))
}

async fn get_rotation(
    State(state): State<Arc<AppState>>,
    Path((vault_id, rotation_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response> {
    let vault_id = parse_id16(&vault_id)?;
    let rotation_id = parse_id16(&rotation_id)?;
    require_device(&state, &headers)?;
    let view = state.db.read(move |conn| {
        require_vault(conn, &vault_id)?;
        rotations::view(conn, &rotation_id)
    })?;
    Ok(cbor_ok(vec![(0, rotation_fields(&view))]))
}

async fn stage_grant(
    State(state): State<Arc<AppState>>,
    Path((vault_id, rotation_id, device_id)): Path<(String, String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response> {
    let vault_id = parse_id16(&vault_id)?;
    let rotation_id = parse_id16(&rotation_id)?;
    let recipient = parse_id16(&device_id)?;
    let actor = require_management_realm(&state, &headers)?;
    let fields = parse_body(&state, &body)?;
    let grant_id = wire::id16_field(&fields, 0)?;
    let hpke_enc: [u8; 32] = wire::key32_field(&fields, 1)?;
    let hpke_ciphertext = wire::bytes_field(&fields, 2)?;
    let signed_header = wire::bytes_field(&fields, 3)?;
    let signature = wire::bytes_field(&fields, 4)?;
    let manager_rev = wire::uint_field(&fields, 5)?;
    let hook = state.hook.clone();
    let view = state
        .write(move |conn, clock| {
            require_vault(conn, &vault_id)?;
            let input = GrantInput {
                grant_id,
                hpke_enc,
                hpke_ciphertext: &hpke_ciphertext,
                signed_header: &signed_header,
                signature: &signature,
                manager_rev,
            };
            crate::rotations::stage_grant(
                conn,
                hook.as_ref(),
                &actor,
                &rotation_id,
                &recipient,
                &input,
                clock,
            )
        })
        .await?;
    state.notify();
    Ok(cbor_ok(vec![(0, rotation_fields(&view))]))
}

async fn activate_rotation(
    State(state): State<Arc<AppState>>,
    Path((vault_id, rotation_id)): Path<(String, String)>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response> {
    let vault_id = parse_id16(&vault_id)?;
    let rotation_id = parse_id16(&rotation_id)?;
    let actor = require_management_realm(&state, &headers)?;
    let fields = parse_body(&state, &body)?;
    let signed = signed_body(&fields, 0)?;
    let request_id = wire::id16_field(&fields, 1)?;
    let expected_control_rev = wire::uint_field(&fields, 2)?;
    let new_manager = match wire::opt_map_field(&fields, 3)? {
        None => None,
        Some(map) => Some(NewManager {
            rev: wire::uint_field(&map, 0)?,
            pubkey: wire::key32_field(&map, 1)?,
            transition: wire::bytes_field(&map, 2)?,
        }),
    };
    let path = uri.path().to_string();
    let hook = state.hook.clone();
    let outcome = state
        .write(move |conn, clock| {
            require_vault(conn, &vault_id)?;
            rotations::activate(
                conn,
                clock,
                hook.as_ref(),
                &actor,
                &rotation_id,
                &ActivateRequest {
                    request_id,
                    path: &path,
                    body_sha256: sha256(&signed.payload),
                    expected_control_rev,
                    signed: &signed.payload,
                    signature: &signed.signature,
                    new_manager,
                },
            )
        })
        .await?;
    state.notify();
    Ok(cbor_ok(vec![
        (0, cbor_uint(outcome.epoch)),
        (1, cbor_uint(outcome.control_rev)),
        (2, cbor_uint(outcome.auth_rev)),
        (3, cbor_bytes(outcome.member_set_hash)),
    ]))
}

async fn abort_rotation(
    State(state): State<Arc<AppState>>,
    Path((vault_id, rotation_id)): Path<(String, String)>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response> {
    let vault_id = parse_id16(&vault_id)?;
    let rotation_id = parse_id16(&rotation_id)?;
    let actor = require_management_realm(&state, &headers)?;
    let fields = parse_body(&state, &body)?;
    let signed = signed_body(&fields, 0)?;
    let request_id = wire::id16_field(&fields, 1)?;
    let expected_control_rev = wire::uint_field(&fields, 2)?;
    let path = uri.path().to_string();
    let hook = state.hook.clone();
    let view = state
        .write(move |conn, clock| {
            require_vault(conn, &vault_id)?;
            rotations::abort(
                conn,
                clock,
                hook.as_ref(),
                &actor,
                &rotation_id,
                &AbortRequest {
                    request_id,
                    path: &path,
                    body_sha256: sha256(&signed.payload),
                    expected_control_rev,
                    signed: &signed.payload,
                    signature: &signed.signature,
                },
            )
        })
        .await?;
    state.notify();
    Ok(cbor_ok(vec![(0, rotation_fields(&view))]))
}

async fn latest_grant(
    State(state): State<Arc<AppState>>,
    Path(vault_id): Path<String>,
    headers: HeaderMap,
) -> Result<Response> {
    let vault_id = parse_id16(&vault_id)?;
    let actor = require_device(&state, &headers)?;
    let device_id = actor.device_id()?;
    let grant = state.db.read(move |conn| {
        require_vault(conn, &vault_id)?;
        rotations::latest_grant(conn, &device_id)
    })?;
    let Some(grant) = grant else {
        return Err(Error::api(ErrorCode::NotFound));
    };
    Ok(cbor_ok(vec![
        (0, cbor_uint(grant.epoch)),
        (1, cbor_bytes(grant.device_id)),
        (2, cbor_uint(grant.manager_rev)),
        (3, cbor_bytes(grant.hpke_enc)),
        (4, cbor_bytes(grant.hpke_ciphertext)),
        (5, cbor_bytes(grant.signed_header)),
    ]))
}

// ---------------------------------------------------------------- WS /wake

async fn wake(
    State(state): State<Arc<AppState>>,
    Path(vault_id): Path<String>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Result<Response> {
    let vault_id = parse_id16(&vault_id)?;
    let token = bearer(&headers)?;
    // 升级前先验证一次：无效/已吊销 token 直接 401，不建立 WS。
    {
        let clock = state.db.clock_handle();
        let token = token.clone();
        state.db.read(move |conn| {
            auth::authenticate(conn, clock.as_ref(), &token)?;
            Ok(())
        })?;
    }
    Ok(ws.on_upgrade(move |socket| wake_loop(state, vault_id, token, socket)))
}

async fn wake_loop(state: Arc<AppState>, vault_id: Id16, token: String, mut socket: WebSocket) {
    if !push_wake(&state, &vault_id, &token, &mut socket).await {
        return;
    }
    let mut events = state.wake.subscribe();
    let mut ticker = tokio::time::interval(Duration::from_secs(30));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            incoming = socket.recv() => {
                match incoming {
                    None => return,
                    Some(Err(_)) => return,
                    Some(Ok(Message::Close(_))) => return,
                    Some(Ok(_)) => {}
                }
            }
            event = events.recv() => {
                if event.is_err() {
                    return;
                }
                if !push_wake(&state, &vault_id, &token, &mut socket).await {
                    return;
                }
            }
            _ = ticker.tick() => {
                if !push_wake(&state, &vault_id, &token, &mut socket).await {
                    return;
                }
            }
        }
    }
}

/// 重新鉴权并推送最新水位；返回 false 表示应断开（吊销/过期/IO 错误）。
async fn push_wake(state: &AppState, vault_id: &Id16, token: &str, socket: &mut WebSocket) -> bool {
    let vault_id = *vault_id;
    let clock = state.db.clock_handle();
    let token = token.to_string();
    let snapshot = state.db.read(move |conn| {
        auth::authenticate(conn, clock.as_ref(), &token)?;
        let vault = require_vault(conn, &vault_id)?;
        Ok((vault.head_seq, vault.active_epoch, vault.control_rev))
    });
    let (head_seq, epoch, control_rev) = match snapshot {
        Ok(values) => values,
        Err(_) => {
            let _ = socket
                .send(Message::Close(Some(CloseFrame {
                    code: 1008,
                    reason: "unauthorized".into(),
                })))
                .await;
            return false;
        }
    };
    let payload = serde_json::json!({
        "v": 1,
        "head_seq": head_seq,
        "epoch": epoch,
        "control_rev": control_rev,
    });
    socket
        .send(Message::Text(payload.to_string().into()))
        .await
        .is_ok()
}

// ---------------------------------------------------------------- 运维入口

/// 触发一次 GC tick（部署侧由后台任务定时调用；测试直接调）。
pub async fn run_gc(state: Arc<AppState>) -> Result<gc::GcOutcome> {
    state.write(move |conn, clock| gc::tick(conn, clock)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::ids::b64url_encode;

    #[test]
    fn path_ids_accept_hex_uuid_and_base64url() {
        let id = [0xABu8; 16];
        let hex = hex_lower(&id);
        assert_eq!(parse_id16(&hex).expect("hex"), id);
        let uuid = format!(
            "{}-{}-{}-{}-{}",
            &hex[0..8],
            &hex[8..12],
            &hex[12..16],
            &hex[16..20],
            &hex[20..32]
        );
        assert_eq!(parse_id16(&uuid).expect("uuid"), id);
        assert_eq!(parse_id16(&b64url_encode(&id)).expect("b64"), id);
        assert!(parse_id16("zz").is_err());
    }

    #[test]
    fn realm_tags_match_wire() {
        assert_eq!(realm_tag(Realm::Device), 1);
        assert_eq!(realm_tag(Realm::Management), 2);
    }
}
