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
use axum::extract::{DefaultBodyLimit, FromRequestParts, OriginalUri, Path, Query, State};
use axum::http::request::Parts;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post, put};
use axum::Router;

use crate::admin;
use crate::auth::{self, AuthContext, Realm};
use crate::checkpoints::{self, CheckpointRole, PublishRequest};
use crate::config::RelayConfig;
use crate::control;
use crate::db::Db;
use crate::envelope::{ObjectType, MAX_ASSET_ENVELOPE};
use crate::error::{ApiError, Error, ErrorCode, Result};
use crate::gc;
use crate::ids::{b64url_decode, hex_lower, sha256, Hash32, Id16};
use crate::management::{self, CanonicalBody, ManagedOutcome};
use crate::objects;
use crate::pairing;
use crate::ratelimit::RateLimiter;
use crate::refs::RefRole;
use crate::rotations::{self, AbortRequest, ActivateRequest, BeginRequest, GrantInput, NewManager};
use crate::scope::ScopeConn;
use crate::scope::VaultScope;
use crate::sig::SignatureHook;
use crate::slots::{self, SlotKind};
use crate::vault::{self, Vault};
use crate::wire;
use crate::wire::{bytes as cbor_bytes, uint as cbor_uint};
use rusqlite::OptionalExtension;

/// 路由共享状态。
pub struct AppState {
    pub db: Db,
    pub config: Arc<RelayConfig>,
    pub hook: Arc<dyn SignatureHook>,
    /// 唤醒通道：`/wake` 订阅它；吊销/新 head/epoch 变化时广播（消息可丢，不作为可靠投递）。
    wake: tokio::sync::broadcast::Sender<WakeEvent>,
    /// v2 `POST /v2/vaults` 入口限流（ADR 0014 §A；只影响 v2，不影响 v1）。
    v2_create_limiter: RateLimiter,
    /// v2 `/v2/vaults/{id}/auth/*` 入口限流。
    v2_auth_limiter: RateLimiter,
    /// v2 **业务**（数据面/控制面）路由入口限流（ADR 0014 §A「入口先限流」的扩展）。
    v2_data_limiter: RateLimiter,
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
        let v2_create_limiter = RateLimiter::new(config.v2_create_limit);
        let v2_auth_limiter = RateLimiter::new(config.v2_auth_limit);
        let v2_data_limiter = RateLimiter::new(config.v2_data_limit);
        Arc::new(Self {
            db,
            config,
            hook,
            wake,
            v2_create_limiter,
            v2_auth_limiter,
            v2_data_limiter,
        })
    }

    /// v2 创建入口限流（超限 → 429 `RATE_LIMITED`）。
    fn check_v2_create_rate(&self) -> Result<()> {
        self.v2_create_limiter.acquire()
    }

    /// v2 鉴权入口限流（超限 → 429 `RATE_LIMITED`）。
    fn check_v2_auth_rate(&self) -> Result<()> {
        self.v2_auth_limiter.acquire()
    }

    /// v2 **业务**路由入口限流；v1 永不受限（ADR 0014 §A：v1 行为不变）。
    fn check_v2_data_rate(&self, face: ApiFace) -> Result<()> {
        match face {
            ApiFace::V1 => Ok(()),
            ApiFace::V2 => self.v2_data_limiter.acquire(),
        }
    }

    /// 广播一次"变了"（WS 收到后自行重新鉴权并读取最新状态）。
    pub fn notify(&self) {
        let _ = self.wake.send(WakeEvent::Changed);
    }

    fn write<T, F>(&self, scope: VaultScope, f: F) -> impl std::future::Future<Output = Result<T>>
    where
        T: Send + 'static,
        F: FnOnce(&ScopeConn<'_>, &dyn crate::time::Clock) -> Result<T> + Send + 'static,
    {
        let db = self.db.clone();
        let clock = db.clock_handle();
        async move { db.write(scope, move |conn| f(conn, clock.as_ref())).await }
    }
}

/// 路由面：`/v1`（legacy 默认 vault）或 `/v2`（显式租户路由）。
///
/// ADR 0014 S2c：**业务 handler 实现只写一份**，v1/v2 两个路径注册同一个 handler；
/// 两面只在这些地方不同：
/// - 鉴权：v1 保持旧语义；v2 额外强制 `route.vault == auth.vault == signed.vault`；
/// - 入口限流：只有 v2 业务路由受限（v1 完全不受影响）。
///
/// 面由 axum 已匹配的**真实请求路径**（`OriginalUri`）推导，不由 token/subject 推断：
/// 路径同时是管理签名绑定的事实（`management::run`），因此 v2 客户端签 `/v2/...` 路径
/// 即天然绑定到该面。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApiFace {
    V1,
    V2,
}

impl ApiFace {
    /// 由请求路径前缀判定；`None` 表示不是租户路径（防御性拒绝）。
    fn of_path(path: &str) -> Option<Self> {
        if path.starts_with("/v2/") {
            Some(Self::V2)
        } else if path.starts_with("/v1/") {
            Some(Self::V1)
        } else {
            None
        }
    }

    const fn is_v2(self) -> bool {
        matches!(self, Self::V2)
    }
}

impl<S> FromRequestParts<S> for ApiFace
where
    S: Send + Sync,
{
    type Rejection = Error;

    async fn from_request_parts(
        parts: &mut Parts,
        _state: &S,
    ) -> std::result::Result<Self, Self::Rejection> {
        let path = match parts.extensions.get::<OriginalUri>() {
            Some(original) => original.0.path().to_owned(),
            None => parts.uri.path().to_owned(),
        };
        Self::of_path(&path).ok_or_else(|| Error::api(ErrorCode::NotFound))
    }
}

/// 构建路由。
///
/// 两个前缀注册**同一批业务 handler**（[`tenant_routes`]）：`/v1/vaults/{vault}/...` 是
/// legacy 默认 vault 的旧 API，`/v2/vaults/{vault}/...` 是 ADR 0014 的显式租户路由；
/// 两者只在 [`ApiFace`]（鉴权严格度 + 入口限流）上不同，业务实现不复制。
pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/v1/vaults", post(create_vault))
        .route("/v1/auth/challenges", post(create_challenge))
        .route("/v1/auth/tokens", post(issue_token))
        // v2（ADR 0014 §A）：创建票据消费 + 租户鉴权。**不开放**匿名 vault 列表。
        .route("/v2/vaults", post(create_vault_v2))
        .route(
            "/v2/vaults/{vault}/auth/challenges",
            post(create_challenge_v2),
        )
        .route("/v2/vaults/{vault}/auth/tokens", post(issue_token_v2))
        // 租户业务路由：v1 legacy 与 v2 各注册一份（同一实现，差异见 `ApiFace`）。
        .merge(tenant_routes("/v1/vaults/"))
        .merge(tenant_routes("/v2/vaults/"))
        .layer(DefaultBodyLimit::max(MAX_ASSET_ENVELOPE))
        .with_state(state)
}

/// 一个前缀下的**全部**租户业务路由（v1/v2 共用同一 handler 实现）。
///
/// 清点口径 = S2a/S2b 之后 `api.rs` 的完整路由表：`/v1/vaults`（genesis）与 `/auth/*`
/// 不在此列（它们的 v2 形态语义不同：创建票据 / 显式 scope 鉴权）。公开路由
/// `invites/{invite}/redeem` 也放在这里：它**不需要 token**（授权来自 invite token），
/// 但 v2 形态仍计入业务入口限流。
fn tenant_routes(prefix: &str) -> Router<Arc<AppState>> {
    let path = |suffix: &str| format!("{prefix}{{vault}}{suffix}");
    Router::new()
        .route(&path("/state"), get(get_state))
        .route(&path("/control"), get(get_control))
        .route(&path("/log"), get(get_log))
        .route(&path("/acks"), post(post_acks))
        .route(&path("/updates"), post(post_updates))
        .route(
            &path("/objects/{object}/parts/{part}"),
            put(put_part).get(get_part),
        )
        .route(&path("/objects/{object}"), get(get_object))
        .route(&path("/objects/{object}/complete"), post(complete_object))
        .route(&path("/objects"), get(list_objects))
        .route(&path("/checkpoints"), post(create_checkpoint))
        .route(
            &path("/checkpoints/{checkpoint}/refs"),
            post(add_checkpoint_refs),
        )
        .route(
            &path("/checkpoints/{checkpoint}/publish"),
            post(publish_checkpoint),
        )
        .route(&path("/bootstrap"), post(bootstrap))
        .route(&path("/bootstrap-leases/{lease}"), patch(renew_lease))
        .route(&path("/devices/self/grants/latest"), get(latest_grant))
        .route(&path("/invites"), post(create_invite))
        .route(&path("/invites/{invite}/redeem"), post(redeem_invite))
        .route(&path("/devices/{device}/approve"), post(approve_device))
        // ADR 0010：管理端独立吊销（只断访问，不轮换密钥；`control_rev` 不变）。
        .route(&path("/devices/{device}/revoke"), post(revoke_device))
        .route(&path("/key-slots"), get(list_key_slots))
        .route(&path("/key-slots/{slot}"), put(put_key_slot))
        .route(&path("/key-slots/{kind}/activate"), post(activate_key_slot))
        .route(&path("/rotations"), post(begin_rotation))
        .route(&path("/rotations/current"), get(get_current_rotation))
        .route(&path("/rotations/{rotation}"), get(get_rotation))
        .route(
            &path("/rotations/{rotation}/restart"),
            post(restart_rotation),
        )
        .route(
            &path("/rotations/{rotation}/revoke-more"),
            post(revoke_more_rotation),
        )
        .route(
            &path("/rotations/{rotation}/grants"),
            get(list_rotation_grants),
        )
        // §35.4：设备 realm 只读的完整 leaf manifest（B 侧 inclusion 校验）。
        // 必须注册在 `{device}` 动态段之前，避免 `leaves` 被当成设备 id。
        .route(
            &path("/rotations/{rotation}/grants/leaves"),
            get(list_rotation_grant_leaves),
        )
        .route(
            &path("/rotations/{rotation}/assets"),
            get(list_rotation_assets),
        )
        .route(&path("/checkpoints/{checkpoint}"), get(get_checkpoint))
        .route(
            &path("/rotations/{rotation}/grants/{device}"),
            put(stage_grant),
        )
        .route(
            &path("/rotations/{rotation}/activate"),
            post(activate_rotation),
        )
        .route(&path("/rotations/{rotation}/abort"), post(abort_rotation))
        .route(&path("/wake"), get(wake))
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

/// 管理写响应：首次执行用字段编码，幂等重放直接回放原响应体（字节一致）。
fn managed_response(outcome: ManagedOutcome, status: StatusCode) -> Response {
    match outcome {
        ManagedOutcome::Fresh(fields) => cbor(fields, status),
        ManagedOutcome::Replayed(body) => {
            (status, [(header::CONTENT_TYPE, wire::CONTENT_TYPE)], body).into_response()
        }
    }
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

fn authenticate(state: &AppState, headers: &HeaderMap, scope: VaultScope) -> Result<AuthContext> {
    let token = bearer(headers)?;
    authenticate_token(state, &token, scope)
}

/// token → `AuthContext`（v1 语义：token 本身有效但属于**别的** vault 时映射成 404）。
///
/// 拆分出来是为了 `/wake` 的每轮重鉴权：WS 建立后手里只有 token 字符串，没有 header。
fn authenticate_token(state: &AppState, token: &str, scope: VaultScope) -> Result<AuthContext> {
    let clock = state.db.clock_handle();
    let probe_token = token.to_string();
    let probe_clock = state.db.clock_handle();
    let token = token.to_string();
    match state.db.read(scope, move |conn| {
        auth::authenticate(conn, clock.as_ref(), &token)
    }) {
        Ok(ctx) => Ok(ctx),
        Err(err) if is_unauthorized(&err) => {
            // v1 兼容：token 本身有效但属于**别的** vault 时，旧行为是 404（路由 vault
            // 对调用者不可见），不是 401。这里显式复查，且不回显目标租户任何数据。
            let elsewhere = state.db.read_instance(move |conn| {
                auth::token_valid_in_other_vault(conn, probe_clock.as_ref(), &probe_token, scope)
            })?;
            if elsewhere {
                Err(Error::api(ErrorCode::NotFound))
            } else {
                Err(err)
            }
        }
        Err(err) => Err(err),
    }
}

/// WS 连接建立后每轮重鉴权：与 [`authenticate_face`] 同一语义（v2 仍强制 route == auth）。
fn authenticate_token_face(
    state: &AppState,
    token: &str,
    scope: VaultScope,
    face: ApiFace,
) -> Result<AuthContext> {
    let ctx = authenticate_token(state, token, scope)?;
    if face.is_v2() {
        ensure_same_vault(&ctx, scope)?;
    }
    Ok(ctx)
}

/// ADR 0014 §A：`route.vault == auth.vault == signed.vault` 的一致性断言。
///
/// token 只在签发它的 scope 内有效（`auth::authenticate` 的查询都带 `vault_id`），
/// 这里再做一次防御性断言：任何 v2 路由忘了显式校验，也会被挡在这里。
fn ensure_same_vault(ctx: &AuthContext, scope: VaultScope) -> Result<()> {
    if ctx.vault != scope {
        return Err(Error::api(ErrorCode::NotFound));
    }
    Ok(())
}

/// v2 路由的鉴权入口：在 v1 语义之上**强制** `route.vault == auth.vault`。
fn authenticate_v2(
    state: &AppState,
    headers: &HeaderMap,
    scope: VaultScope,
) -> Result<AuthContext> {
    let ctx = authenticate(state, headers, scope)?;
    ensure_same_vault(&ctx, scope)?;
    Ok(ctx)
}

/// 按**路由面**鉴权：v1 保持旧语义；v2 走 [`authenticate_v2`]（route = auth = signed）。
fn authenticate_face(
    state: &AppState,
    headers: &HeaderMap,
    scope: VaultScope,
    face: ApiFace,
) -> Result<AuthContext> {
    match face {
        ApiFace::V1 => authenticate(state, headers, scope),
        ApiFace::V2 => authenticate_v2(state, headers, scope),
    }
}

fn is_unauthorized(err: &Error) -> bool {
    matches!(err, Error::Api(api) if api.code == ErrorCode::Unauthorized)
}

/// 按路由面要求设备 realm（v2 额外断言 route == auth）。
fn require_device_face(
    state: &AppState,
    headers: &HeaderMap,
    scope: VaultScope,
    face: ApiFace,
) -> Result<AuthContext> {
    let ctx = authenticate_face(state, headers, scope, face)?;
    ctx.device_id()?;
    Ok(ctx)
}

/// ADR §22：management realm **只读**放行 `KEY_SLOT` 对象（元数据 + 分片），
/// 让恢复设备自批时能先读槽、解出真实 epoch root 再封 grant；其他对象类型仍只 device realm。
fn require_object_read(actor: &AuthContext, object_type: ObjectType) -> Result<()> {
    match (actor.realm, object_type) {
        (Realm::Device, _) => Ok(()),
        (Realm::Management, ObjectType::KeySlot) => Ok(()),
        _ => Err(Error::api(ErrorCode::ManagementRequired)),
    }
}

/// 按路由面要求管理 realm（v2 额外断言 route == auth）。
fn require_management_face(
    state: &AppState,
    headers: &HeaderMap,
    scope: VaultScope,
    face: ApiFace,
) -> Result<AuthContext> {
    let ctx = authenticate_face(state, headers, scope, face)?;
    ctx.require_management()?;
    Ok(ctx)
}

/// 按路由 `vault_id` 读该租户行：`conn` 的作用域已由调用方按同一 `vault_id` 建立，
/// 因此这里只做"路由 vault 与 scope 一致"的防御性断言（不一致绝不回退到别的租户）。
fn require_vault(conn: &ScopeConn, vault_id: &Id16) -> Result<Vault> {
    if conn.scope().vault_id() != vault_id {
        return Err(Error::api(ErrorCode::NotFound));
    }
    vault::load(conn)
}

/// v1 旧 API 的 legacy 默认 vault：显式解析（配置覆盖或 `vaults.legacy_default=1`）。
fn legacy_scope(state: &AppState) -> Result<VaultScope> {
    let cfg = state.config.clone();
    state
        .db
        .read_instance(move |conn| vault::resolve_legacy_scope(conn, &cfg))
}

// ---------------------------------------------------------------- vault init

/// v1/v2 创建成功的响应体（字段完全一致，`v1` 客户端无需感知 v2）：
/// `{0 instance, 1 log, 2 epoch, 3 control_rev, 4 auth_rev, 5 cursor}`。
fn created_vault_response(outcome: &vault::InitOutcome, vault: &Vault) -> Result<Response> {
    // 键 5：seq=0 的 HMAC 游标。客户端 genesis 后写入 `sync.relay_cursor`。
    let cursor = crate::log::cursor_at(vault, 0)?;
    Ok(cbor_created(vec![
        (0, cbor_bytes(outcome.instance_id)),
        (1, cbor_bytes(outcome.log_id)),
        (2, cbor_uint(outcome.epoch)),
        (3, cbor_uint(outcome.control_rev)),
        (4, cbor_uint(outcome.auth_rev)),
        (5, wire::text(cursor)),
    ]))
}

/// v1 genesis：显式 setup secret（ADR 0006 §4），行为不变。
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
    let scope = VaultScope::new(request.vault_id);
    let (outcome, vault) = state
        .write(scope, move |conn, clock| {
            let outcome = vault::initialize(conn, clock, &config, hook.as_ref(), &request)?;
            let vault = vault::load(conn)?;
            Ok((outcome, vault))
        })
        .await?;
    created_vault_response(&outcome, &vault)
}

/// v2 创建（ADR 0014 §A）：**一次性创建票据** + 现有 InitRequest。
///
/// 请求字段（CBOR/CDE，键升序；本端点是 v2 新增，不影响 v1 wire）：
///
/// | 键 | 字段 |
/// | --- | --- |
/// | 0 | `ticket`：运维发行的 32B 明文（库里只有 SHA256） |
/// | 1 | `vault_id`：票据绑定的 128-bit 公开路由标识 |
/// | 2 | `device_id` |
/// | 3 | `sign_pk`（32B Ed25519） |
/// | 4 | `hpke_pk`（32B X25519） |
/// | 5 | `manager_pub`（32B Ed25519） |
/// | 6 | `certificate`：`CDE(DeviceCertV1)` |
/// | 7 | `certificate_signature`：`MN/v1/device-cert\0` 域签名 |
///
/// **没有** setup secret：授权来自票据。响应与 v1 create 完全同形。
///
/// 事务：`token_hash` 查票据 → 未过期/未消费/额度/`vault_id` 匹配 → `create_vault_with_ticket`
/// → 原子扣额度/置 `consumed_at`，四步在**一个** `BEGIN IMMEDIATE` 里：并发双消费只有一个
/// 成功；创建失败（证书错误/已存在）整体回滚，票据不消费、不留半个 vault。
async fn create_vault_v2(State(state): State<Arc<AppState>>, body: Bytes) -> Result<Response> {
    // 入口先限流（ADR 0014 §A），再解析 body/查库。
    state.check_v2_create_rate()?;
    let fields = parse_body(&state, &body)?;
    let ticket = wire::bytes_field(&fields, 0)?;
    let request = vault::InitRequest {
        vault_id: wire::id16_field(&fields, 1)?,
        device_id: wire::id16_field(&fields, 2)?,
        sign_pk: wire::key32_field(&fields, 3)?,
        hpke_pk: wire::key32_field(&fields, 4)?,
        manager_pub: wire::key32_field(&fields, 5)?,
        // v2 的授权是创建票据，不是 setup secret；置空且不参与校验/消费。
        setup_secret: String::new(),
        certificate: wire::bytes_field(&fields, 6)?,
        certificate_signature: wire::bytes_field(&fields, 7)?,
    };
    let config = state.config.clone();
    let hook = state.hook.clone();
    let scope = VaultScope::new(request.vault_id);
    let (outcome, vault) = state
        .write(scope, move |conn, clock| {
            // 票据消费与 vault genesis 必须同一事务：任何一步失败都整体回滚。
            admin::consume_create_ticket(conn, clock.now_secs(), &ticket)?;
            let outcome =
                vault::create_vault_with_ticket(conn, clock, &config, hook.as_ref(), &request)?;
            let vault = vault::load(conn)?;
            Ok((outcome, vault))
        })
        .await?;
    created_vault_response(&outcome, &vault)
}

// ---------------------------------------------------------------- auth

fn realm_tag(realm: Realm) -> u64 {
    match realm {
        Realm::Device => 1,
        Realm::Management => 2,
    }
}

/// 在**显式 scope** 里创建 challenge（v1/v2 共用）。
///
/// scope 由调用方决定：v1 是 legacy 默认 vault（`legacy_scope`），v2 是路由 `vault_id`。
/// challenge 行带 `vault_id`，因此跨 vault 的 challenge 在另一个 scope 里查不到。
async fn create_challenge_in_scope(
    state: Arc<AppState>,
    scope: VaultScope,
    body: Bytes,
) -> Result<Response> {
    let fields = parse_body(&state, &body)?;
    let realm = match wire::uint_field(&fields, 0)? {
        1 => Realm::Device,
        2 => Realm::Management,
        _ => return Err(Error::api(ErrorCode::InvalidRequest)),
    };
    let subject = wire::id16_field(&fields, 1)?;
    let (challenge, vault, now) = state
        .write(scope, move |conn, clock| {
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

/// v1 challenge：显式 legacy 默认 vault（S2a 语义，不按 subject 猜租户）。
async fn create_challenge(State(state): State<Arc<AppState>>, body: Bytes) -> Result<Response> {
    let scope = legacy_scope(&state)?;
    create_challenge_in_scope(state, scope, body).await
}

/// v2 challenge：路由 `vault_id` 即鉴权作用域；未知 vault → 404（不泄露任何其它租户）。
async fn create_challenge_v2(
    State(state): State<Arc<AppState>>,
    Path(vault_id): Path<String>,
    body: Bytes,
) -> Result<Response> {
    state.check_v2_auth_rate()?;
    let vault_id = parse_id16(&vault_id)?;
    create_challenge_in_scope(state, VaultScope::new(vault_id), body).await
}

/// 在**显式 scope** 里兑现 challenge 并签发 token（v1/v2 共用）。
///
/// `auth::issue_token` 的 challenge/token 查询都带 `vault_id`（来自 scope），签名校验的
/// `SignatureCheck.vault_id` 也取自该 scope 的 vault 行——因此跨 vault 的挑战/token 必然被拒。
async fn issue_token_in_scope(
    state: Arc<AppState>,
    scope: VaultScope,
    body: Bytes,
) -> Result<Response> {
    let fields = parse_body(&state, &body)?;
    let challenge_id = wire::id16_field(&fields, 0)?;
    let message = wire::bytes_field(&fields, 1)?;
    let signature = wire::bytes_field(&fields, 2)?;
    let hook = state.hook.clone();
    let outcome = state
        .write(scope, move |conn, clock| {
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

/// v1 token：显式 legacy 默认 vault（行为不变）。
async fn issue_token(State(state): State<Arc<AppState>>, body: Bytes) -> Result<Response> {
    let scope = legacy_scope(&state)?;
    issue_token_in_scope(state, scope, body).await
}

/// v2 token：路由 `vault_id` 即鉴权作用域；跨 vault 的 challenge 在此 scope 查不到 → 401。
async fn issue_token_v2(
    State(state): State<Arc<AppState>>,
    Path(vault_id): Path<String>,
    body: Bytes,
) -> Result<Response> {
    state.check_v2_auth_rate()?;
    let vault_id = parse_id16(&vault_id)?;
    issue_token_in_scope(state, VaultScope::new(vault_id), body).await
}

// ---------------------------------------------------------------- state / control / log

/// 只读水位（v1/v2 同字段）。
///
/// v2 形态是 S2b 里第一个需要鉴权的 v2 路由，也是 `route.vault == auth.vault` 的可测入口；
/// 现在与其它 v2 业务路由一样走 [`authenticate_face`] + 业务入口限流。
async fn get_state(
    State(state): State<Arc<AppState>>,
    face: ApiFace,
    Path(vault_id): Path<String>,
    headers: HeaderMap,
) -> Result<Response> {
    let vault_id = parse_id16(&vault_id)?;
    state_payload(&state, &headers, vault_id, face).await
}

async fn state_payload(
    state: &Arc<AppState>,
    headers: &HeaderMap,
    vault_id: Id16,
    face: ApiFace,
) -> Result<Response> {
    state.check_v2_data_rate(face)?;
    let scope = VaultScope::new(vault_id);
    authenticate_face(state, headers, scope, face)?;
    let clock = state.db.clock_handle();
    let payload = state.db.read(scope, move |conn| {
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
    // §28.1 条目 10（评审 §三 1）：当前正在进行的 rotation（让响应丢失的客户端先查询）。
    if let Some(rotation) = state
        .db
        .read(scope, crate::rotations::latest)?
        .filter(|rotation| {
            matches!(
                rotation.state,
                crate::rotations::RotationState::Frozen | crate::rotations::RotationState::Staging
            )
        })
    {
        fields.push((15, cbor_bytes(rotation.rotation_id)));
    }
    fields.sort_by_key(|(key, _)| *key);
    Ok(cbor_ok(fields))
}

async fn get_control(
    State(state): State<Arc<AppState>>,
    face: ApiFace,
    Path(vault_id): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Result<Response> {
    state.check_v2_data_rate(face)?;
    let vault_id = parse_id16(&vault_id)?;
    authenticate_face(&state, &headers, vault_id.into(), face)?;
    let after_rev: u64 = query
        .get("after_rev")
        .map(|value| value.parse::<u64>())
        .transpose()
        .map_err(|_| Error::api(ErrorCode::InvalidRequest))?
        .unwrap_or(0);
    let (vault, entries, members, slot_chain, manager_chain) =
        state.db.read(vault_id.into(), move |conn| {
            let vault = require_vault(conn, &vault_id)?;
            let mut stmt = conn.prepare(
                "SELECT rev, prev_hash, record_hash, signed_record FROM control_commits
             WHERE rev > ?1 AND vault_id = ?2 ORDER BY rev ASC LIMIT 256",
            )?;
            let rows = stmt.query_map(
                (
                    i64::try_from(after_rev).map_err(|_| Error::Invariant("after_rev"))?,
                    conn.scope().as_slice(),
                ),
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
                let kind = control::kind_of(&signed).unwrap_or_else(|| "raw".to_string());
                let mut entry = vec![
                    (
                        0,
                        cbor_uint(u64::try_from(rev).map_err(|_| Error::Invariant("rev"))?),
                    ),
                    (1, cbor_bytes(prev)),
                    (2, cbor_bytes(record)),
                    (3, cbor_bytes(signed)),
                ];
                // §28.1 条目 4/10：typed 控制记录（旧记录是裸 CDE → `raw`）。
                entry.push((4, wire::text(kind)));
                entries.push(wire::map(entry));
            }
            // 成员证书：客户端验 ObjectCommit 需要生产者当前证书（按 device_id 索引）。
            let mut member_stmt = conn.prepare(
                "SELECT device_id, certificate, status, sign_pk, hpke_pk, certificate_signature
             FROM devices WHERE vault_id = ?1 ORDER BY device_id ASC",
            )?;
            let member_rows = member_stmt.query_map([conn.scope().as_slice()], |row| {
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
                let (device_id, certificate, status, sign_pk, hpke_pk, certificate_signature) =
                    row?;
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
             WHERE vault_id = ?1 ORDER BY slot_set_rev ASC, kind ASC",
            )?;
            let slot_rows = slot_stmt.query_map([conn.scope().as_slice()], |row| {
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
            // §27.2：管理权转换链（转换记录 + 双签），客户端逐级验证管理权。
            let mut manager_stmt = conn.prepare(
                "SELECT rev, pubkey, transition, old_rev, old_signature, new_signature
             FROM manager_keys WHERE vault_id = ?1 ORDER BY rev ASC",
            )?;
            let manager_rows = manager_stmt.query_map([conn.scope().as_slice()], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, Option<Vec<u8>>>(4)?,
                    row.get::<_, Option<Vec<u8>>>(5)?,
                ))
            })?;
            let mut manager_chain = Vec::new();
            for row in manager_rows {
                let (rev, pubkey, transition, old_rev, old_sig, new_sig) = row?;
                let mut entry = vec![
                    (
                        0,
                        cbor_uint(u64::try_from(rev).map_err(|_| Error::Invariant("manager rev"))?),
                    ),
                    (1, cbor_bytes(pubkey)),
                    (2, cbor_bytes(transition)),
                    (
                        3,
                        cbor_uint(u64::try_from(old_rev).map_err(|_| Error::Invariant("old rev"))?),
                    ),
                ];
                if let Some(signature) = old_sig {
                    entry.push((4, cbor_bytes(signature)));
                }
                if let Some(signature) = new_sig {
                    entry.push((5, cbor_bytes(signature)));
                }
                entry.sort_by_key(|(key, _)| *key);
                manager_chain.push(wire::map(entry));
            }
            Ok((vault, entries, members, slot_chain, manager_chain))
        })?;
    Ok(cbor_ok(vec![
        (0, wire::array(entries)),
        (1, cbor_uint(vault.control_rev)),
        (2, cbor_uint(vault.active_epoch)),
        (3, wire::array(members)),
        (4, wire::array(slot_chain)),
        (5, wire::array(manager_chain)),
    ]))
}

async fn get_log(
    State(state): State<Arc<AppState>>,
    face: ApiFace,
    Path(vault_id): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Result<Response> {
    state.check_v2_data_rate(face)?;
    let vault_id = parse_id16(&vault_id)?;
    require_device_face(&state, &headers, vault_id.into(), face)?;
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
    let page = state.db.read(vault_id.into(), move |conn| {
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
    face: ApiFace,
    Path(vault_id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response> {
    state.check_v2_data_rate(face)?;
    let vault_id = parse_id16(&vault_id)?;
    let actor = require_device_face(&state, &headers, vault_id.into(), face)?;
    let fields = parse_body(&state, &body)?;
    let received_seq = wire::uint_field(&fields, 0)?;
    let integrated_seq = wire::uint_field(&fields, 1)?;
    let epoch = wire::uint_field(&fields, 2)?;
    state
        .write(vault_id.into(), move |conn, clock| {
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
                    "SELECT received_seq, integrated_seq FROM device_acks
                     WHERE device_id = ?1 AND vault_id = ?",
                    (device_id.as_slice(), conn.scope().as_slice()),
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
                "INSERT INTO device_acks(vault_id, device_id, received_seq, integrated_seq,
                                         epoch, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(vault_id, device_id) DO UPDATE SET
                    received_seq = excluded.received_seq,
                    integrated_seq = excluded.integrated_seq,
                    epoch = excluded.epoch,
                    updated_at = excluded.updated_at",
                (
                    conn.scope().as_slice(),
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
    face: ApiFace,
    Path(vault_id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response> {
    state.check_v2_data_rate(face)?;
    let vault_id = parse_id16(&vault_id)?;
    let actor = require_device_face(&state, &headers, vault_id.into(), face)?;
    let fields = parse_body(&state, &body)?;
    let items = wire::array_field(&fields, 0)?;
    if items.len() > 256 {
        return Err(Error::api(ErrorCode::PayloadTooLarge));
    }
    state.db.read(vault_id.into(), move |conn| {
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
        let max_snapshot_bytes = state.config.max_snapshot_bytes;
        let outcome = state
            .write(vault_id.into(), move |conn, clock| {
                let object = objects::load(conn, &object_id)?;
                if object.object_type == ObjectType::Snapshot
                    && object.plain_len > max_snapshot_bytes
                {
                    return Err(Error::api(ErrorCode::PayloadTooLarge));
                }
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
    face: ApiFace,
    Path((vault_id, object_id, part_no)): Path<(String, String, u32)>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response> {
    state.check_v2_data_rate(face)?;
    let vault_id = parse_id16(&vault_id)?;
    let object_id = parse_hash32(&object_id)?;
    let actor = require_device_face(&state, &headers, vault_id.into(), face)?;
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
        .write(vault_id.into(), move |conn, clock| {
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
    face: ApiFace,
    Path((vault_id, object_id, part_no)): Path<(String, String, u32)>,
    headers: HeaderMap,
) -> Result<Response> {
    state.check_v2_data_rate(face)?;
    let vault_id = parse_id16(&vault_id)?;
    let object_id = parse_hash32(&object_id)?;
    let actor = authenticate_face(&state, &headers, vault_id.into(), face)?;
    let now = state.db.clock().now_secs();
    let (bytes, digest) = state.db.read(vault_id.into(), move |conn| {
        require_vault(conn, &vault_id)?;
        // 先确认对象类型再放行（§22：management 只能读 KEY_SLOT）。
        let object = objects::load(conn, &object_id)?;
        require_object_read(&actor, object.object_type)?;
        if object.object_type == ObjectType::KeySlot
            && !crate::slots::slot_object_readable(conn, &object_id, now)?
        {
            return Err(Error::api(ErrorCode::Retired));
        }
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
    face: ApiFace,
    Path((vault_id, object_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response> {
    state.check_v2_data_rate(face)?;
    let vault_id = parse_id16(&vault_id)?;
    let object_id = parse_hash32(&object_id)?;
    let actor = authenticate_face(&state, &headers, vault_id.into(), face)?;
    let now = state.db.clock().now_secs();
    let view = state.db.read(vault_id.into(), move |conn| {
        require_vault(conn, &vault_id)?;
        let view = objects::meta(conn, &object_id)?;
        require_object_read(&actor, view.object.object_type)?;
        if view.object.object_type == ObjectType::KeySlot
            && !crate::slots::slot_object_readable(conn, &object_id, now)?
        {
            return Err(Error::api(ErrorCode::Retired));
        }
        Ok(view)
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
    face: ApiFace,
    Path((vault_id, object_id)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response> {
    state.check_v2_data_rate(face)?;
    let vault_id = parse_id16(&vault_id)?;
    let object_id = parse_hash32(&object_id)?;
    let actor = require_device_face(&state, &headers, vault_id.into(), face)?;
    let fields = parse_body(&state, &body)?;
    let commit = wire::bytes_field(&fields, 0)?;
    let signature = wire::bytes_field(&fields, 1)?;
    let hook = state.hook.clone();
    let max_snapshot_bytes = state.config.max_snapshot_bytes;
    let outcome = state
        .write(vault_id.into(), move |conn, clock| {
            let object = objects::load(conn, &object_id)?;
            if object.object_type == ObjectType::Snapshot && object.plain_len > max_snapshot_bytes {
                return Err(Error::api(ErrorCode::PayloadTooLarge));
            }
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
    face: ApiFace,
    Path(vault_id): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Result<Response> {
    state.check_v2_data_rate(face)?;
    let vault_id = parse_id16(&vault_id)?;
    require_device_face(&state, &headers, vault_id.into(), face)?;
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
        .unwrap_or(state.db.read(vault_id.into(), vault::load)?.head_seq);
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
    let refs = state.db.read(vault_id.into(), move |conn| {
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
    face: ApiFace,
    Path(vault_id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response> {
    state.check_v2_data_rate(face)?;
    let vault_id = parse_id16(&vault_id)?;
    let actor = require_device_face(&state, &headers, vault_id.into(), face)?;
    let fields = parse_body(&state, &body)?;
    let estimated_bytes = wire::uint_field(&fields, 0)?;
    let epoch = wire::opt_uint_field(&fields, 1)?;
    // §28.1 条目 3：候选 checkpoint 的固定 role + request-id 幂等创建。
    let role = match wire::opt_field(&fields, 2) {
        None => None,
        Some(wire::CdeValue::Text(text)) => {
            Some(CheckpointRole::parse(text).ok_or_else(|| Error::api(ErrorCode::InvalidRequest))?)
        }
        Some(_) => return Err(Error::api(ErrorCode::InvalidRequest)),
    };
    let request_id = match wire::opt_field(&fields, 3) {
        None => None,
        Some(_) => Some(wire::id16_field(&fields, 3)?),
    };
    let estimated_bytes =
        i64::try_from(estimated_bytes).map_err(|_| Error::api(ErrorCode::InvalidRequest))?;
    let outcome = state
        .write(vault_id.into(), move |conn, clock| {
            require_vault(conn, &vault_id)?;
            checkpoints::create(
                conn,
                clock,
                &actor,
                estimated_bytes,
                epoch,
                role,
                request_id,
            )
        })
        .await?;
    let mut response = vec![
        (0, cbor_bytes(outcome.checkpoint_id)),
        (1, cbor_uint(outcome.epoch)),
        (2, cbor_uint(outcome.covered_seq)),
        (3, cbor_bytes(outcome.reservation_id)),
        (
            4,
            cbor_uint(u64::try_from(outcome.reservation_expires_at).unwrap_or(0)),
        ),
    ];
    if let Some(role) = outcome.role {
        response.push((5, wire::text(role.as_str())));
    }
    response.sort_by_key(|(key, _)| *key);
    Ok(cbor_created(response))
}

async fn add_checkpoint_refs(
    State(state): State<Arc<AppState>>,
    face: ApiFace,
    Path((vault_id, checkpoint_id)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response> {
    state.check_v2_data_rate(face)?;
    let vault_id = parse_id16(&vault_id)?;
    let checkpoint_id = parse_id16(&checkpoint_id)?;
    require_device_face(&state, &headers, vault_id.into(), face)?;
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
        .write(vault_id.into(), move |conn, _clock| {
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
    face: ApiFace,
    Path((vault_id, checkpoint_id)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response> {
    state.check_v2_data_rate(face)?;
    let vault_id = parse_id16(&vault_id)?;
    let checkpoint_id = parse_id16(&checkpoint_id)?;
    let actor = require_device_face(&state, &headers, vault_id.into(), face)?;
    let fields = parse_body(&state, &body)?;
    let covered_seq = wire::uint_field(&fields, 0)?;
    let manifest_oid = wire::hash32_field(&fields, 1)?;
    let refs_digest = wire::hash32_field(&fields, 2)?;
    let signed_commit = wire::bytes_field(&fields, 3)?;
    let signature = wire::bytes_field(&fields, 4)?;
    let hook = state.hook.clone();
    let outcome = state
        .write(vault_id.into(), move |conn, clock| {
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
    // §20.1：bootstrap 条目必须带 signed_commit + 生产者签名，客户端才能验 S/manifest/refs。
    // 旧 checkpoint 两者都缺失时省略键（客户端按"无签名"拒绝，不静默通过）。
    if let Some(commit) = &checkpoint.signed_commit {
        fields.push((6, cbor_bytes(commit.clone())));
    }
    if let Some(signature) = &checkpoint.commit_signature {
        fields.push((7, cbor_bytes(signature.clone())));
    }
    fields.sort_by_key(|(key, _)| *key);
    wire::map(fields)
}

async fn bootstrap(
    State(state): State<Arc<AppState>>,
    face: ApiFace,
    Path(vault_id): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Result<Response> {
    state.check_v2_data_rate(face)?;
    let vault_id = parse_id16(&vault_id)?;
    let actor = require_device_face(&state, &headers, vault_id.into(), face)?;
    // §28.1 条目 9：显式降级读回（一基线损坏时读另一基线）；GC 仍被"两份基线"门禁挡住。
    let allow_degraded = matches!(
        query.get("degraded").map(String::as_str),
        Some("1") | Some("true")
    );
    let (vault, outcome) = state
        .write(vault_id.into(), move |conn, clock| {
            let vault = require_vault(conn, &vault_id)?;
            checkpoints::bootstrap(conn, clock, &actor, allow_degraded)
                .map(|outcome| (vault, outcome))
        })
        .await?;
    let primary_cursor = crate::log::cursor_at(&vault, outcome.primary.covered_seq)?;
    let mut fields = vec![
        (0, checkpoint_fields(&outcome.primary, primary_cursor)),
        (2, cbor_uint(outcome.head_seq)),
        (3, cbor_uint(outcome.gc_floor)),
        (4, cbor_uint(outcome.epoch)),
        (5, cbor_bytes(outcome.lease.lease_id)),
        (
            6,
            cbor_uint(u64::try_from(outcome.lease.expires_at).unwrap_or(0)),
        ),
        (7, wire::boolean(outcome.degraded)),
    ];
    if let Some(fallback) = &outcome.fallback {
        let fallback_cursor = crate::log::cursor_at(&vault, fallback.covered_seq)?;
        fields.push((1, checkpoint_fields(fallback, fallback_cursor)));
    }
    fields.sort_by_key(|(key, _)| *key);
    Ok(cbor_ok(fields))
}

async fn renew_lease(
    State(state): State<Arc<AppState>>,
    face: ApiFace,
    Path((vault_id, lease_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response> {
    state.check_v2_data_rate(face)?;
    let vault_id = parse_id16(&vault_id)?;
    let lease_id = parse_id16(&lease_id)?;
    let actor = require_device_face(&state, &headers, vault_id.into(), face)?;
    let lease = state
        .write(vault_id.into(), move |conn, clock| {
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
    signature: Vec<u8>,
}

/// 设备签名的载荷包装（如 `PUT key-slots/{s}` 的槽描述符）。
///
/// §26 的管理写不走这里：它们用 [`CanonicalBody`] 把**业务字段唯一来源**限定在被签名 body。
fn signed_body(fields: &wire::Fields, key: u64) -> Result<SignedBody> {
    let signed = wire::Signed::from_fields(fields, key)?;
    Ok(SignedBody {
        payload: signed.payload,
        signature: signed.signature,
    })
}

async fn create_invite(
    State(state): State<Arc<AppState>>,
    face: ApiFace,
    Path(vault_id): Path<String>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response> {
    state.check_v2_data_rate(face)?;
    let vault_id = parse_id16(&vault_id)?;
    let actor = require_management_face(&state, &headers, vault_id.into(), face)?;
    let fields = parse_body(&state, &body)?;
    // §26：业务字段**只**从唯一 canonical body 解析（`{0: token_hash, 1: expires_at?}`）；
    // 外层 request_id/expected_control_rev 是签名输入的一部分，不接受第二份业务载荷。
    let canonical = CanonicalBody::parse(&fields, 0)?;
    let request_id = wire::id16_field(&fields, 1)?;
    let expected_control_rev = wire::uint_field(&fields, 2)?;
    let token_hash = canonical.hash32(0)?;
    let expires_at = canonical
        .opt_u64(1)?
        .map(|value| i64::try_from(value).map_err(|_| Error::api(ErrorCode::InvalidRequest)))
        .transpose()?;
    let path = uri.path().to_string();
    let hook = state.hook.clone();
    let scope = management::scope("invites");
    let outcome = state
        .write(vault_id.into(), move |conn, clock| {
            require_vault(conn, &vault_id)?;
            let call = management::ManagementCall {
                scope: &scope,
                method_tag: 2, // POST
                path: &path,
                request_id,
                expected_control_rev,
                body: &canonical,
            };
            management::run(conn, clock, hook.as_ref(), &call, || {
                let invite = pairing::create_invite(
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
                        unsigned_body: canonical.payload.clone(),
                        signature: canonical.signature.clone(),
                    },
                )?;
                Ok(vec![
                    (0, cbor_bytes(invite.invite_id)),
                    (1, cbor_uint(u64::try_from(invite.expires_at).unwrap_or(0))),
                ])
            })
        })
        .await?;
    state.notify();
    Ok(managed_response(outcome, StatusCode::CREATED))
}

async fn redeem_invite(
    State(state): State<Arc<AppState>>,
    face: ApiFace,
    Path((vault_id, invite_id)): Path<(String, String)>,
    body: Bytes,
) -> Result<Response> {
    // 公开路由：授权来自 invite token（不是 bearer），但 v2 形态仍计入业务入口限流。
    state.check_v2_data_rate(face)?;
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
        .write(vault_id.into(), move |conn, clock| {
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
    face: ApiFace,
    Path((vault_id, device_id)): Path<(String, String)>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response> {
    state.check_v2_data_rate(face)?;
    let vault_id = parse_id16(&vault_id)?;
    let device_id = parse_id16(&device_id)?;
    let actor = require_management_face(&state, &headers, vault_id.into(), face)?;
    let fields = parse_body(&state, &body)?;
    // §26：canonical body 的字段编号 0..=7 覆盖 approve 的全部业务输入；
    // 外层 3..=8 是旧 wire 的镜像，出现时必须与 body 逐字节一致（否则 403）。
    let canonical = CanonicalBody::parse(&fields, 0)?;
    let request_id = management::id16_field(&fields, 1, &canonical, 6)?;
    let expected_control_rev = management::u64_field(&fields, 2, &canonical, 7)?;
    let certificate = management::bytes_field(&fields, 3, &canonical, 0)?;
    let certificate_signature = management::bytes_field(&fields, 4, &canonical, 1)?;
    let grant_id = management::id16_field(&fields, 5, &canonical, 2)?;
    let hpke_enc = management::hash32_field(&fields, 6, &canonical, 3)?;
    let hpke_ciphertext = management::bytes_field(&fields, 7, &canonical, 4)?;
    let grant_signature = management::bytes_field(&fields, 8, &canonical, 5)?;
    let path = uri.path().to_string();
    let hook = state.hook.clone();
    let scope = management::scope("device-approve");
    let outcome = state
        .write(vault_id.into(), move |conn, clock| {
            require_vault(conn, &vault_id)?;
            let call = management::ManagementCall {
                scope: &scope,
                method_tag: 2, // POST
                path: &path,
                request_id,
                expected_control_rev,
                body: &canonical,
            };
            management::run(conn, clock, hook.as_ref(), &call, || {
                let outcome = pairing::approve_device(
                    conn,
                    clock,
                    hook.as_ref(),
                    &actor,
                    &device_id,
                    &pairing::ApproveRequest {
                        certificate,
                        certificate_signature,
                        grant_id,
                        hpke_enc,
                        hpke_ciphertext,
                        grant_signature,
                        expected_control_rev,
                        request_id,
                        path: path.clone(),
                        unsigned_body: canonical.payload.clone(),
                        management_signature: canonical.signature.clone(),
                    },
                )?;
                Ok(vec![
                    (0, cbor_bytes(outcome.device_id)),
                    (1, wire::text(outcome.fingerprint)),
                    (2, cbor_uint(outcome.epoch)),
                    (3, cbor_uint(outcome.control_rev)),
                    (4, cbor_bytes(outcome.certificate_signature)),
                ])
            })
        })
        .await?;
    state.notify();
    Ok(managed_response(outcome, StatusCode::OK))
}

/// ADR 0010：管理端**独立**吊销设备访问。
///
/// 与 approve 同构（`management::run`：验签 → 幂等 → `control_rev` CAS → 业务 → 同事务缓存
/// 响应），但业务效果只有 `devices.status/revoked_at` 与 `auth_rev`：`control_rev` 不变、
/// 不追加控制记录、不换 root/epoch、不冻结 vault（同步是 E2E，删凭证只阻断访问）。
async fn revoke_device(
    State(state): State<Arc<AppState>>,
    face: ApiFace,
    Path((vault_id, device_id)): Path<(String, String)>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response> {
    state.check_v2_data_rate(face)?;
    let vault_id = parse_id16(&vault_id)?;
    let target = parse_id16(&device_id)?;
    require_management_face(&state, &headers, vault_id.into(), face)?;
    let fields = parse_body(&state, &body)?;
    // §26/ADR 0010：canonical body = `CDE{0: target_device, 1: request_id,
    // 2: expected_control_rev}`；业务字段**只**从这份被签 body 读。外层 1/2 是同一份事实的
    // 镜像，出现时必须逐字节一致（否则 403），target 只在 body 里（防跨设备 id 碰撞）。
    let canonical = CanonicalBody::parse(&fields, 0)?;
    let request_id = management::id16_field(&fields, 1, &canonical, 1)?;
    let expected_control_rev = management::u64_field(&fields, 2, &canonical, 2)?;
    let body_target = canonical.id16(0)?;
    let path = uri.path().to_string();
    let hook = state.hook.clone();
    let scope = management::scope("device-revoke");
    let outcome = state
        .write(vault_id.into(), move |conn, clock| {
            require_vault(conn, &vault_id)?;
            let call = management::ManagementCall {
                scope: &scope,
                method_tag: 2, // POST
                path: &path,
                request_id,
                expected_control_rev,
                body: &canonical,
            };
            management::run(conn, clock, hook.as_ref(), &call, || {
                // path 的 target 也被管理签名覆盖（签名输入含完整 path）；与 body 不一致
                // 说明"签一套、执行另一套" → 按镜像语义 403。检查放在验签之后，避免给
                // 未授权调用方任何"两份载荷是否一致"的前置 oracle。
                if body_target != target {
                    return Err(Error::api(ErrorCode::Forbidden));
                }
                let outcome = crate::revocation::revoke_access(conn, clock, &target)?;
                Ok(vec![
                    (0, cbor_bytes(outcome.target)),
                    // 固定状态字面量：wire 契约（ADR 0010 §决策）。
                    (1, wire::text("revoked")),
                    (2, wire::boolean(outcome.changed)),
                    (3, cbor_uint(outcome.auth_rev)),
                    (4, cbor_uint(outcome.control_rev)),
                ])
            })
        })
        .await?;
    state.notify();
    Ok(managed_response(outcome, StatusCode::OK))
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
    face: ApiFace,
    Path(vault_id): Path<String>,
    headers: HeaderMap,
) -> Result<Response> {
    state.check_v2_data_rate(face)?;
    let vault_id = parse_id16(&vault_id)?;
    let actor = authenticate_face(&state, &headers, vault_id.into(), face)?;
    // ADR §20.2：device realm 只看 active/retiring；management realm 额外看到 staging。
    let include_staging = actor.realm == Realm::Management;
    let clock = state.db.clock_handle();
    let rows = state.db.read(vault_id.into(), move |conn| {
        require_vault(conn, &vault_id)?;
        if include_staging {
            slots::list_management(conn, clock.now_secs())
        } else {
            slots::list_visible(conn, clock.now_secs())
        }
    })?;
    Ok(cbor_ok(vec![(
        0,
        wire::array(rows.iter().map(slot_fields).collect()),
    )]))
}

async fn put_key_slot(
    State(state): State<Arc<AppState>>,
    face: ApiFace,
    Path((vault_id, slot_id)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response> {
    state.check_v2_data_rate(face)?;
    let vault_id = parse_id16(&vault_id)?;
    let slot_id = parse_hash32(&slot_id)?;
    let actor = require_device_face(&state, &headers, vault_id.into(), face)?;
    let fields = parse_body(&state, &body)?;
    let epoch = wire::uint_field(&fields, 0)?;
    let kind = SlotKind::parse(&wire::text_field(&fields, 1)?)
        .ok_or_else(|| Error::api(ErrorCode::InvalidRequest))?;
    let descriptor = wire::bytes_field(&fields, 2)?;
    let signed = signed_body(&fields, 3)?;
    let hook = state.hook.clone();
    let row = state
        .write(vault_id.into(), move |conn, clock| {
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
    face: ApiFace,
    Path((vault_id, kind)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response> {
    state.check_v2_data_rate(face)?;
    let vault_id = parse_id16(&vault_id)?;
    if kind != SlotKind::Password.as_str() {
        return Err(Error::api(ErrorCode::InvalidRequest));
    }
    let actor = require_device_face(&state, &headers, vault_id.into(), face)?;
    let fields = parse_body(&state, &body)?;
    // §26：改口令的 canonical body `{0: expected_slot_set_rev, 1: new_slot_id}` 就是
    // **设备签名覆盖的字节**；外层 0/1 是旧 wire 的镜像，不一致 → 403。
    // 此前设备签名覆盖任意 `signed_message`，而 CAS/slot 指针取自外层字段（可替换）。
    let canonical = CanonicalBody::parse(&fields, 2)?;
    let expected_slot_set_rev = management::u64_field(&fields, 0, &canonical, 0)?;
    let new_slot_id = management::hash32_field(&fields, 1, &canonical, 1)?;
    let hook = state.hook.clone();
    let rev = state
        .write(vault_id.into(), move |conn, clock| {
            require_vault(conn, &vault_id)?;
            slots::activate_password(
                conn,
                clock,
                hook.as_ref(),
                &actor,
                &slots::PasswordActivateRequest {
                    expected_slot_set_rev,
                    new_slot_id,
                    signed_message: canonical.payload.clone(),
                    signature: canonical.signature.clone(),
                },
            )
        })
        .await?;
    state.notify();
    Ok(cbor_ok(vec![(0, cbor_uint(rev))]))
}

// ---------------------------------------------------------------- rotations

/// §28.1 条目 10（评审 §三 2）：rotation view 全量字段。
///
/// 键 0–9 与旧客户端兼容；10+ 是本次增补：
/// 10 control_rev, 11 auth_rev, 12 manager_rev, 13 from_epoch_revoked_once(uint 0/1),
/// 14 freeze_asset_cutoff(=freeze_seq), 15 freeze_asset_count, 16 freeze_asset_digest,
/// 17 member_cert_digest, 18 primary_checkpoint?, 19 fallback_checkpoint?,
/// 20 activation_commit?, 21 activated_at?, 22 activated_control_rev?,
/// 23 candidates[]（0 cid,1 state,2 role?,3 covered_seq,4 manifest_oid?,5 refs_digest?,
///    6 commit_digest?,7 commit_signature?,8 health,9 readback_ready）,
/// 24 slots[]（0 slot_id,1 kind,2 state,3 descriptor_digest,4 epoch）,
/// 25 reservation?（0 reservation_id,1 owner_device,2 expires_at,3 reserved_bytes,
///    4 consumed_bytes,5 state）, 26 candidate_ready(uint 0/1)。
fn rotation_fields(
    conn: &ScopeConn,
    now: i64,
    view: &rotations::RotationView,
) -> Result<wire::CdeValue> {
    let vault = vault::load(conn)?;
    let rotation = &view.rotation;
    let mut fields = vec![
        (0, cbor_bytes(rotation.rotation_id)),
        (1, wire::text(rotation.state.as_str())),
        (2, cbor_uint(rotation.from_epoch)),
        (3, cbor_uint(rotation.to_epoch)),
        (4, cbor_uint(rotation.freeze_seq)),
        (5, cbor_uint(rotation.revoked_count)),
        (7, cbor_uint(view.grants_count)),
        (8, cbor_bytes(view.grants_digest)),
        (9, cbor_uint(view.published_checkpoints)),
        (10, cbor_uint(vault.control_rev)),
        (11, cbor_uint(vault.auth_rev)),
        (12, cbor_uint(vault.manager_rev)),
        (
            13,
            cbor_uint(u64::from(
                if vault::epoch_revoked_once(conn, rotation.from_epoch)? {
                    1u8
                } else {
                    0u8
                },
            )),
        ),
        (14, cbor_uint(rotation.freeze_seq)),
        (
            15,
            cbor_uint(objects::asset_count(
                conn,
                rotation.from_epoch,
                rotation.freeze_seq,
            )?),
        ),
    ];
    if let Some(digest) = rotation.asset_set_digest {
        fields.push((16, cbor_bytes(digest)));
    }
    fields.push((17, cbor_bytes(rotations::member_cert_digest(conn)?)));
    if let Some(hash) = rotation.member_set_hash {
        fields.push((6, cbor_bytes(hash)));
    }
    if let Some(cid) = rotation.primary_checkpoint {
        fields.push((18, cbor_bytes(cid)));
    }
    if let Some(cid) = rotation.fallback_checkpoint {
        fields.push((19, cbor_bytes(cid)));
    }
    if let Some(commit) = rotation.activation_commit {
        fields.push((20, cbor_bytes(commit)));
    }
    if let Some(at) = rotation.activated_at {
        fields.push((21, cbor_uint(u64::try_from(at).unwrap_or(0))));
    }
    if let Some(rev) = rotation.activated_control_rev {
        fields.push((22, cbor_uint(rev)));
    }
    // 候选 checkpoint 元数据（activate 前是 candidate，activate 后是被指名的 published）。
    let mut candidates = Vec::new();
    for checkpoint in checkpoints::candidates_for_epoch(conn, rotation.to_epoch)? {
        candidates.push(checkpoint_meta_value(conn, &checkpoint)?);
    }
    if candidates.is_empty() {
        for checkpoint in checkpoints::published_newest_first_for_epoch(conn, rotation.to_epoch, 2)?
        {
            candidates.push(checkpoint_meta_value(conn, &checkpoint)?);
        }
    }
    let candidate_ready = !candidates.is_empty()
        && candidates.iter().all(|value| {
            wire::value_map(value)
                .ok()
                .and_then(|map| wire::bool_field(&map, 9).ok())
                .unwrap_or(false)
        });
    fields.push((23, wire::array(candidates)));
    // 候选 epoch 的槽集合（slot ids + descriptor digest）。
    let mut slots = Vec::new();
    for slot in slots::list_management(conn, now)?
        .into_iter()
        .filter(|slot| slot.epoch == rotation.to_epoch)
    {
        slots.push(wire::map(vec![
            (0, cbor_bytes(slot.slot_id)),
            (1, wire::text(slot.kind.as_str())),
            (2, wire::text(slot.state.as_str())),
            (
                3,
                cbor_bytes(slots::descriptor_context_hash(&slot.descriptor)),
            ),
            (4, cbor_uint(slot.epoch)),
        ]));
    }
    fields.push((24, wire::array(slots)));
    // 轮换维护 reservation（owner/expiry/bytes）。
    if let Some(reservation) = rotation_reservation_value(conn, &rotation.rotation_id)? {
        fields.push((25, reservation));
    }
    fields.push((26, cbor_uint(if candidate_ready { 1 } else { 0 })));
    fields.sort_by_key(|(key, _)| *key);
    Ok(wire::map(fields))
}

/// checkpoint 元数据（candidate/GET 与 rotation view 共用）。
fn checkpoint_meta_value(
    conn: &ScopeConn,
    checkpoint: &checkpoints::CheckpointRow,
) -> Result<wire::CdeValue> {
    let mut fields = vec![
        (0, cbor_bytes(checkpoint.checkpoint_id)),
        (1, wire::text(checkpoint.state.as_str())),
        (3, cbor_uint(checkpoint.covered_seq)),
        (8, wire::text(checkpoint.health.as_str())),
        (
            9,
            wire::boolean(checkpoints::candidate_ready(conn, checkpoint)?),
        ),
    ];
    if let Some(role) = checkpoint.role {
        fields.push((2, wire::text(role.as_str())));
    }
    if let Some(manifest) = checkpoint.manifest_oid {
        fields.push((4, cbor_bytes(manifest)));
    }
    if let Some(digest) = checkpoint.refs_digest {
        fields.push((5, cbor_bytes(digest)));
    }
    if let Some(commit) = &checkpoint.signed_commit {
        fields.push((6, cbor_bytes(sha256(commit))));
    }
    if let Some(signature) = &checkpoint.commit_signature {
        fields.push((7, cbor_bytes(signature.clone())));
    }
    fields.sort_by_key(|(key, _)| *key);
    Ok(wire::map(fields))
}

/// 轮换 reservation 的视图（没有则 None）。
fn rotation_reservation_value(
    conn: &ScopeConn,
    rotation_id: &Id16,
) -> Result<Option<wire::CdeValue>> {
    let mut stmt = conn.prepare(
        "SELECT reservation_id, owner_device, expires_at, reserved_bytes, consumed_bytes, state
         FROM maintenance_reservations
         WHERE purpose = 'rotation' AND resource_id = ?1 AND vault_id = ?
         ORDER BY rowid DESC LIMIT 1",
    )?;
    let row = stmt
        .query_row((rotation_id.as_slice(), conn.scope().as_slice()), |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, String>(5)?,
            ))
        })
        .optional()?;
    let Some((id, owner, expires_at, reserved, consumed, state)) = row else {
        return Ok(None);
    };
    Ok(Some(wire::map(vec![
        (0, cbor_bytes(id)),
        (1, cbor_bytes(owner)),
        (2, cbor_uint(u64::try_from(expires_at).unwrap_or(0))),
        (3, cbor_uint(u64::try_from(reserved).unwrap_or(0))),
        (4, cbor_uint(u64::try_from(consumed).unwrap_or(0))),
        (5, wire::text(state)),
    ])))
}

async fn begin_rotation(
    State(state): State<Arc<AppState>>,
    face: ApiFace,
    Path(vault_id): Path<String>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response> {
    state.check_v2_data_rate(face)?;
    let vault_id = parse_id16(&vault_id)?;
    let actor = require_management_face(&state, &headers, vault_id.into(), face)?;
    let fields = parse_body(&state, &body)?;
    // §26：canonical body = `{0: expected_epoch, 1: expected_control_rev, 2: request_id,
    // 3: revoke[]}`（与 sync 客户端 `rotation::begin_body` 一致）。外层 1..=4 是旧 wire
    // 的镜像：出现时必须与 body 逐字节一致，任一不符 → 403；执行只用 body 的值。
    let canonical = CanonicalBody::parse(&fields, 0)?;
    let request_id = management::id16_field(&fields, 1, &canonical, 2)?;
    let expected_control_rev = management::u64_field(&fields, 2, &canonical, 1)?;
    let expected_epoch = management::u64_field(&fields, 3, &canonical, 0)?;
    let revoke: Vec<Id16> = management::array_field(&fields, 4, &canonical, 3)?
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
    let scope = management::scope("begin");
    let outcome = state
        .write(vault_id.into(), move |conn, clock| {
            require_vault(conn, &vault_id)?;
            let call = management::ManagementCall {
                scope: &scope,
                method_tag: 2, // POST
                path: &path,
                request_id,
                expected_control_rev,
                body: &canonical,
            };
            management::run(conn, clock, hook.as_ref(), &call, || {
                let view = rotations::begin(
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
                        body_sha256: canonical.digest(),
                        signed: &canonical.payload,
                        signature: &canonical.signature,
                    },
                )?;
                Ok(vec![(0, rotation_fields(conn, clock.now_secs(), &view)?)])
            })
        })
        .await?;
    state.notify();
    Ok(managed_response(outcome, StatusCode::OK))
}

async fn get_rotation(
    State(state): State<Arc<AppState>>,
    face: ApiFace,
    Path((vault_id, rotation_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response> {
    state.check_v2_data_rate(face)?;
    let vault_id = parse_id16(&vault_id)?;
    let rotation_id = parse_id16(&rotation_id)?;
    // §28.1 条目 10：management（恢复）也需要读 r view，不再限 device realm。
    authenticate_face(&state, &headers, vault_id.into(), face)?;
    let clock = state.db.clock_handle();
    let view = state.db.read(vault_id.into(), move |conn| {
        require_vault(conn, &vault_id)?;
        let view = rotations::view(conn, &rotation_id)?;
        rotation_fields(conn, clock.now_secs(), &view)
    })?;
    Ok(cbor_ok(vec![(0, view)]))
}

async fn stage_grant(
    State(state): State<Arc<AppState>>,
    face: ApiFace,
    Path((vault_id, rotation_id, device_id)): Path<(String, String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response> {
    state.check_v2_data_rate(face)?;
    let vault_id = parse_id16(&vault_id)?;
    let rotation_id = parse_id16(&rotation_id)?;
    let recipient = parse_id16(&device_id)?;
    let actor = require_management_face(&state, &headers, vault_id.into(), face)?;
    let fields = parse_body(&state, &body)?;
    // §26/§17.2：grant 的 canonical 字段就是本请求 map（0 grant_id / 1 hpke_enc /
    // 2 hpke_ciphertext / 3 signed_header / 4 signature / 5 manager_rev），**没有**第二份
    // 不透明 body；签名输入由这些字段重新构造 `GrantAadV1 || enc || SHA256(ciphertext)`，
    // 任一所消费字段被替换都会验签失败（`verify.rs` 的 `Expectation::EpochGrant`）。
    // relay 只信 devices 行里的收件人 HPKE 公钥，不收客户端传入的 AAD。
    let grant_id = wire::id16_field(&fields, 0)?;
    let hpke_enc: [u8; 32] = wire::key32_field(&fields, 1)?;
    let hpke_ciphertext = wire::bytes_field(&fields, 2)?;
    let signed_header = wire::bytes_field(&fields, 3)?;
    let signature = wire::bytes_field(&fields, 4)?;
    let manager_rev = wire::uint_field(&fields, 5)?;
    let hook = state.hook.clone();
    let view = state
        .write(vault_id.into(), move |conn, clock| {
            require_vault(conn, &vault_id)?;
            let input = GrantInput {
                grant_id,
                hpke_enc,
                hpke_ciphertext: &hpke_ciphertext,
                signed_header: &signed_header,
                signature: &signature,
                manager_rev,
            };
            let view = crate::rotations::stage_grant(
                conn,
                hook.as_ref(),
                &actor,
                &rotation_id,
                &recipient,
                &input,
                clock,
            )?;
            rotation_fields(conn, clock.now_secs(), &view)
        })
        .await?;
    state.notify();
    Ok(cbor_ok(vec![(0, view)]))
}

async fn activate_rotation(
    State(state): State<Arc<AppState>>,
    face: ApiFace,
    Path((vault_id, rotation_id)): Path<(String, String)>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response> {
    state.check_v2_data_rate(face)?;
    let vault_id = parse_id16(&vault_id)?;
    let rotation_id = parse_id16(&rotation_id)?;
    let actor = require_management_face(&state, &headers, vault_id.into(), face)?;
    let fields = parse_body(&state, &body)?;
    // §26/§27.2：canonical body = `{0: new_manager{0 transition,1 old_sig,2 new_sig},
    // 1: primary_checkpoint, 2: fallback_checkpoint}`。外层 3 是 new_manager 的旧 wire
    // 镜像，必须与 body 逐字节一致；两个 cid 只能来自被签名 body（不接受外层另行解析）。
    let canonical = CanonicalBody::parse(&fields, 0)?;
    let request_id = wire::id16_field(&fields, 1)?;
    let expected_control_rev = wire::uint_field(&fields, 2)?;
    // §34：new_manager 还要携带完整 activation T 与旧/新管理 key 的双签（3/4/5）。
    // 缺失与否由 `rotations::activate` 统一判 409；这里只拒绝类型错误（400）。
    let new_manager = match management::opt_map_field(&fields, 3, &canonical, 0)? {
        None => None,
        Some(map) => Some(NewManager {
            transition: wire::bytes_field(&map, 0)?,
            old_signature: wire::opt_bytes_field(&map, 1)?,
            new_signature: wire::opt_bytes_field(&map, 2)?,
            activation_t: wire::opt_bytes_field(&map, 3)?,
            old_t_signature: wire::opt_bytes_field(&map, 4)?,
            new_t_signature: wire::opt_bytes_field(&map, 5)?,
        }),
    };
    let primary_checkpoint = canonical.opt_id16(1)?;
    let fallback_checkpoint = canonical.opt_id16(2)?;
    // §28.1 条目 6：整套材料摘要（expected CAS），只从被签 body 解析。
    let member_cert_digest = canonical.opt_hash32(3)?;
    let grants_digest = canonical.opt_hash32(4)?;
    let slot_set_digest = canonical.opt_hash32(5)?;
    let asset_set_digest = canonical.opt_hash32(6)?;
    let path = uri.path().to_string();
    let hook = state.hook.clone();
    let scope = management::scope("activate");
    let outcome = state
        .write(vault_id.into(), move |conn, clock| {
            require_vault(conn, &vault_id)?;
            let call = management::ManagementCall {
                scope: &scope,
                method_tag: 2, // POST
                path: &path,
                request_id,
                expected_control_rev,
                body: &canonical,
            };
            management::run(conn, clock, hook.as_ref(), &call, || {
                let outcome = rotations::activate(
                    conn,
                    clock,
                    hook.as_ref(),
                    &actor,
                    &rotation_id,
                    &ActivateRequest {
                        request_id,
                        path: &path,
                        body_sha256: canonical.digest(),
                        expected_control_rev,
                        signed: &canonical.payload,
                        signature: &canonical.signature,
                        new_manager,
                        primary_checkpoint,
                        fallback_checkpoint,
                        member_cert_digest,
                        grants_digest,
                        slot_set_digest,
                        asset_set_digest,
                    },
                )?;
                Ok(vec![
                    (0, cbor_uint(outcome.epoch)),
                    (1, cbor_uint(outcome.control_rev)),
                    (2, cbor_uint(outcome.auth_rev)),
                    (3, cbor_bytes(outcome.member_set_hash)),
                    (4, cbor_bytes(outcome.primary_checkpoint)),
                    (5, cbor_bytes(outcome.fallback_checkpoint)),
                    (6, cbor_bytes(outcome.activation_commit)),
                ])
            })
        })
        .await?;
    state.notify();
    Ok(managed_response(outcome, StatusCode::OK))
}

async fn abort_rotation(
    State(state): State<Arc<AppState>>,
    face: ApiFace,
    Path((vault_id, rotation_id)): Path<(String, String)>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response> {
    state.check_v2_data_rate(face)?;
    let vault_id = parse_id16(&vault_id)?;
    let rotation_id = parse_id16(&rotation_id)?;
    let actor = require_management_face(&state, &headers, vault_id.into(), face)?;
    let fields = parse_body(&state, &body)?;
    // §26：abort 的 canonical body 是**空 map**（没有业务载荷）；request_id /
    // expected_control_rev 由管理签名输入直接绑定。非空 body 属未知键 → 400。
    let canonical = CanonicalBody::parse(&fields, 0)?;
    if !canonical.fields.is_empty() {
        return Err(Error::api(ErrorCode::InvalidRequest));
    }
    let request_id = wire::id16_field(&fields, 1)?;
    let expected_control_rev = wire::uint_field(&fields, 2)?;
    let path = uri.path().to_string();
    let hook = state.hook.clone();
    let scope = management::scope("abort");
    let outcome = state
        .write(vault_id.into(), move |conn, clock| {
            require_vault(conn, &vault_id)?;
            let call = management::ManagementCall {
                scope: &scope,
                method_tag: 2, // POST
                path: &path,
                request_id,
                expected_control_rev,
                body: &canonical,
            };
            management::run(conn, clock, hook.as_ref(), &call, || {
                let view = rotations::abort(
                    conn,
                    clock,
                    hook.as_ref(),
                    &actor,
                    &rotation_id,
                    &AbortRequest {
                        request_id,
                        path: &path,
                        body_sha256: canonical.digest(),
                        expected_control_rev,
                        signed: &canonical.payload,
                        signature: &canonical.signature,
                    },
                )?;
                Ok(vec![(0, rotation_fields(conn, clock.now_secs(), &view)?)])
            })
        })
        .await?;
    state.notify();
    Ok(managed_response(outcome, StatusCode::OK))
}

async fn get_current_rotation(
    State(state): State<Arc<AppState>>,
    face: ApiFace,
    Path(vault_id): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Result<Response> {
    state.check_v2_data_rate(face)?;
    let vault_id = parse_id16(&vault_id)?;
    // §28.1 条目 10（评审 §三 1）：management + device 都可读，用于响应丢失后按
    // `request_id` 找回结果（不盲重发 begin/activate）。
    authenticate_face(&state, &headers, vault_id.into(), face)?;
    let request_id = match query.get("request_id") {
        None => None,
        Some(value) => Some(parse_id16(value)?),
    };
    let clock = state.db.clock_handle();
    let (view, replay) = state.db.read(vault_id.into(), move |conn| {
        require_vault(conn, &vault_id)?;
        let view = match rotations::latest(conn)? {
            None => None,
            Some(latest) => Some(rotation_fields(
                conn,
                clock.now_secs(),
                &rotations::view(conn, &latest.rotation_id)?,
            )?),
        };
        let replay = match request_id {
            None => None,
            Some(request_id) => management::lookup_any(conn, &request_id)?,
        };
        Ok((view, replay))
    })?;
    let mut fields = Vec::new();
    if let Some(view) = view {
        fields.push((0, view));
    }
    if let Some((scope, response)) = replay {
        fields.push((1, cbor_bytes(response)));
        fields.push((2, wire::text(scope)));
    }
    fields.sort_by_key(|(key, _)| *key);
    Ok(cbor_ok(fields))
}

async fn restart_rotation(
    State(state): State<Arc<AppState>>,
    face: ApiFace,
    Path((vault_id, rotation_id)): Path<(String, String)>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response> {
    state.check_v2_data_rate(face)?;
    let vault_id = parse_id16(&vault_id)?;
    let rotation_id = parse_id16(&rotation_id)?;
    let actor = require_management_face(&state, &headers, vault_id.into(), face)?;
    let fields = parse_body(&state, &body)?;
    // §28.1 条目 7：restart 的 canonical body 为空（request_id/control_rev 由管理签名绑定）。
    let canonical = CanonicalBody::parse(&fields, 0)?;
    if !canonical.fields.is_empty() {
        return Err(Error::api(ErrorCode::InvalidRequest));
    }
    let request_id = wire::id16_field(&fields, 1)?;
    let expected_control_rev = wire::uint_field(&fields, 2)?;
    let path = uri.path().to_string();
    let hook = state.hook.clone();
    let scope = management::scope("restart");
    let outcome = state
        .write(vault_id.into(), move |conn, clock| {
            require_vault(conn, &vault_id)?;
            let call = management::ManagementCall {
                scope: &scope,
                method_tag: 2,
                path: &path,
                request_id,
                expected_control_rev,
                body: &canonical,
            };
            management::run(conn, clock, hook.as_ref(), &call, || {
                let view = rotations::restart(
                    conn,
                    clock,
                    hook.as_ref(),
                    &actor,
                    &rotation_id,
                    &rotations::ControlRequest {
                        request_id,
                        path: &path,
                        body_sha256: canonical.digest(),
                        expected_control_rev,
                        signed: &canonical.payload,
                        signature: &canonical.signature,
                    },
                )?;
                Ok(vec![
                    (0, rotation_fields(conn, clock.now_secs(), &view)?),
                    (1, wire::boolean(true)),
                ])
            })
        })
        .await?;
    state.notify();
    Ok(managed_response(outcome, StatusCode::OK))
}

async fn revoke_more_rotation(
    State(state): State<Arc<AppState>>,
    face: ApiFace,
    Path((vault_id, rotation_id)): Path<(String, String)>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response> {
    state.check_v2_data_rate(face)?;
    let vault_id = parse_id16(&vault_id)?;
    let rotation_id = parse_id16(&rotation_id)?;
    let actor = require_management_face(&state, &headers, vault_id.into(), face)?;
    let fields = parse_body(&state, &body)?;
    // canonical body = `{0: revoke[]}`；外层 3 是旧 wire 镜像（必须逐字节一致）。
    let canonical = CanonicalBody::parse(&fields, 0)?;
    let request_id = wire::id16_field(&fields, 1)?;
    let expected_control_rev = wire::uint_field(&fields, 2)?;
    let revoke: Vec<Id16> = management::array_field(&fields, 3, &canonical, 0)?
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
    let scope = management::scope("revoke-more");
    let outcome = state
        .write(vault_id.into(), move |conn, clock| {
            require_vault(conn, &vault_id)?;
            let call = management::ManagementCall {
                scope: &scope,
                method_tag: 2,
                path: &path,
                request_id,
                expected_control_rev,
                body: &canonical,
            };
            management::run(conn, clock, hook.as_ref(), &call, || {
                let outcome = rotations::revoke_more(
                    conn,
                    clock,
                    hook.as_ref(),
                    &actor,
                    &rotation_id,
                    &rotations::ControlRequest {
                        request_id,
                        path: &path,
                        body_sha256: canonical.digest(),
                        expected_control_rev,
                        signed: &canonical.payload,
                        signature: &canonical.signature,
                    },
                    &revoke,
                )?;
                Ok(vec![
                    (0, rotation_fields(conn, clock.now_secs(), &outcome.view)?),
                    (1, wire::boolean(outcome.restarted)),
                    (2, cbor_uint(outcome.revoked)),
                ])
            })
        })
        .await?;
    state.notify();
    Ok(managed_response(outcome, StatusCode::OK))
}

/// grant 叶子 manifest 的视图（management 专用；含完整 signed_header 与包 digest）。
fn grant_leaf_fields(grant: &rotations::GrantRow) -> wire::CdeValue {
    let mut fields = vec![
        (0, cbor_bytes(grant.device_id)),
        (1, cbor_uint(grant.epoch)),
        (2, cbor_uint(grant.manager_rev)),
        (3, cbor_bytes(grant.hpke_enc)),
        (4, cbor_bytes(sha256(&grant.hpke_ciphertext))),
        (5, cbor_bytes(grant.signed_header.clone())),
        (9, cbor_bytes(rotations::grant_package_digest(grant))),
    ];
    if let Some(signature) = &grant.signature {
        fields.push((6, cbor_bytes(signature.clone())));
    }
    if let Some(grant_id) = grant.grant_id {
        fields.push((7, cbor_bytes(grant_id)));
    }
    if let Some(activation_ref) = grant.activation_ref {
        fields.push((8, cbor_bytes(activation_ref)));
    }
    fields.sort_by_key(|(key, _)| *key);
    wire::map(fields)
}

async fn list_rotation_grants(
    State(state): State<Arc<AppState>>,
    face: ApiFace,
    Path((vault_id, rotation_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response> {
    state.check_v2_data_rate(face)?;
    let vault_id = parse_id16(&vault_id)?;
    let rotation_id = parse_id16(&rotation_id)?;
    // §28.1 条目 5 + 评审 §三 4：staged grants **仅 management** 可读。
    require_management_face(&state, &headers, vault_id.into(), face)?;
    let (view, grants) = state.db.read(vault_id.into(), move |conn| {
        require_vault(conn, &vault_id)?;
        let view = rotations::view(conn, &rotation_id)?;
        let grants = rotations::list_grants(conn, view.rotation.to_epoch)?;
        Ok((view, grants))
    })?;
    let entries: Vec<wire::CdeValue> = grants.iter().map(grant_leaf_fields).collect();
    Ok(cbor_ok(vec![
        (0, wire::array(entries)),
        (1, cbor_bytes(view.grants_digest)),
        (2, cbor_uint(view.grants_count)),
    ]))
}

/// §35.4：设备 realm 可读的**最小 leaf 证据**：`{0: device_id,1: SHA256(signed_header),
/// 2: signed_header}`。
///
/// 只暴露 inclusion 校验所需字段（不含 HPKE 密文/签名），并让设备端自己复算
/// `SHA256(signed_header)`，不必信任 relay 自报的 digest。
fn grant_leaf_evidence_fields(grant: &rotations::GrantRow) -> wire::CdeValue {
    wire::map(vec![
        (0, cbor_bytes(grant.device_id)),
        (1, cbor_bytes(sha256(&grant.signed_header))),
        (2, cbor_bytes(grant.signed_header.clone())),
    ])
}

/// §35.4：`GET /v1/vaults/{v}/rotations/{r}/grants/leaves`。
///
/// 与 `/grants` 的区别：**设备 realm** 可读，但只允许：
/// - active 设备（`authenticate` 已按 `devices.status` 拒绝吊销/未知设备）；
/// - 该 rotation 的成员（存在该 epoch 的 grant 行，防跨轮换枚举）；
/// - rotation 处于 `staging`/`activated`（frozen 尚未收齐、aborted 已删 grant，
///   都不能作为 inclusion 证据）。
///
/// 响应：`{0: leaves[], 1: grants_digest, 2: grants_count, 3: to_epoch,
/// 4: rotation_state, 5: rotation_id}`。客户端必须用返回的 leaf 复算
/// `grants_digest`（`SHA256(concat(sorted(device_id || SHA256(signed_header))))`），
/// 不得信任 relay 自报的 digest。
async fn list_rotation_grant_leaves(
    State(state): State<Arc<AppState>>,
    face: ApiFace,
    Path((vault_id, rotation_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response> {
    state.check_v2_data_rate(face)?;
    let vault_id = parse_id16(&vault_id)?;
    let rotation_id = parse_id16(&rotation_id)?;
    let actor = require_device_face(&state, &headers, vault_id.into(), face)?;
    let requester = actor.device_id()?;
    let (view, grants) = state.db.read(vault_id.into(), move |conn| {
        require_vault(conn, &vault_id)?;
        let view = rotations::view(conn, &rotation_id)?;
        if !matches!(
            view.rotation.state,
            rotations::RotationState::Staging | rotations::RotationState::Activated
        ) {
            return Err(Error::api(ErrorCode::InvalidState));
        }
        let grants = rotations::list_grants(conn, view.rotation.to_epoch)?;
        if !grants.iter().any(|grant| grant.device_id == requester) {
            return Err(Error::api(ErrorCode::Forbidden));
        }
        Ok((view, grants))
    })?;
    let entries: Vec<wire::CdeValue> = grants.iter().map(grant_leaf_evidence_fields).collect();
    Ok(cbor_ok(vec![
        (0, wire::array(entries)),
        (1, cbor_bytes(view.grants_digest)),
        (2, cbor_uint(view.grants_count)),
        (3, cbor_uint(view.rotation.to_epoch)),
        (4, wire::text(view.rotation.state.as_str())),
        (5, cbor_bytes(rotation_id)),
    ]))
}

async fn list_rotation_assets(
    State(state): State<Arc<AppState>>,
    face: ApiFace,
    Path((vault_id, rotation_id)): Path<(String, String)>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Result<Response> {
    state.check_v2_data_rate(face)?;
    let vault_id = parse_id16(&vault_id)?;
    let rotation_id = parse_id16(&rotation_id)?;
    // §28.1 条目 9（评审 §三 8）：冻结时刻的 finalized 资产分页读，不依赖可回收 log。
    authenticate_face(&state, &headers, vault_id.into(), face)?;
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
    let (rotation, refs, total) = state.db.read(vault_id.into(), move |conn| {
        require_vault(conn, &vault_id)?;
        let rotation = rotations::get(conn, &rotation_id)?;
        let refs = objects::list_assets(
            conn,
            rotation.from_epoch,
            rotation.freeze_seq,
            after.as_ref(),
            limit,
        )?;
        let total = objects::asset_count(conn, rotation.from_epoch, rotation.freeze_seq)?;
        Ok((rotation, refs, total))
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
        .collect::<Vec<_>>();
    let mut fields = vec![
        (0, wire::array(entries)),
        (1, cbor_uint(rotation.freeze_seq)),
        (3, cbor_uint(total)),
    ];
    if let Some(digest) = rotation.asset_set_digest {
        fields.push((2, cbor_bytes(digest)));
    }
    fields.sort_by_key(|(key, _)| *key);
    Ok(cbor_ok(fields))
}

async fn get_checkpoint(
    State(state): State<Arc<AppState>>,
    face: ApiFace,
    Path((vault_id, checkpoint_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response> {
    state.check_v2_data_rate(face)?;
    let vault_id = parse_id16(&vault_id)?;
    let checkpoint_id = parse_id16(&checkpoint_id)?;
    // §28.1 条目 3：候选 checkpoint 元数据/GET（readback-ready 与 published 分离）。
    authenticate_face(&state, &headers, vault_id.into(), face)?;
    let detail = state.db.read(vault_id.into(), move |conn| {
        require_vault(conn, &vault_id)?;
        let checkpoint = checkpoints::get(conn, &checkpoint_id)?;
        let mut fields = vec![
            (0, cbor_bytes(checkpoint.checkpoint_id)),
            (1, wire::text(checkpoint.state.as_str())),
            (3, cbor_uint(checkpoint.covered_seq)),
            (8, wire::text(checkpoint.health.as_str())),
            (
                9,
                wire::boolean(checkpoints::candidate_ready(conn, &checkpoint)?),
            ),
            (10, cbor_uint(checkpoint.epoch)),
        ];
        if let Some(role) = checkpoint.role {
            fields.push((2, wire::text(role.as_str())));
        }
        if let Some(manifest) = checkpoint.manifest_oid {
            fields.push((4, cbor_bytes(manifest)));
        }
        if let Some(digest) = checkpoint.refs_digest {
            fields.push((5, cbor_bytes(digest)));
        }
        if let Some(commit) = &checkpoint.signed_commit {
            fields.push((6, cbor_bytes(commit.clone())));
        }
        if let Some(signature) = &checkpoint.commit_signature {
            fields.push((7, cbor_bytes(signature.clone())));
        }
        if let Some(published_at) = checkpoint.published_at {
            fields.push((11, cbor_uint(u64::try_from(published_at).unwrap_or(0))));
        }
        fields.sort_by_key(|(key, _)| *key);
        Ok(wire::map(fields))
    })?;
    Ok(cbor_ok(vec![(0, detail)]))
}

async fn latest_grant(
    State(state): State<Arc<AppState>>,
    face: ApiFace,
    Path(vault_id): Path<String>,
    headers: HeaderMap,
) -> Result<Response> {
    state.check_v2_data_rate(face)?;
    let vault_id = parse_id16(&vault_id)?;
    let actor = require_device_face(&state, &headers, vault_id.into(), face)?;
    let device_id = actor.device_id()?;
    let grant = state.db.read(vault_id.into(), move |conn| {
        require_vault(conn, &vault_id)?;
        rotations::latest_grant(conn, &device_id)
    })?;
    let Some(grant) = grant else {
        return Err(Error::api(ErrorCode::NotFound));
    };
    let package_digest = rotations::grant_package_digest(&grant);
    let mut fields = vec![
        (0, cbor_uint(grant.epoch)),
        (1, cbor_bytes(grant.device_id)),
        (2, cbor_uint(grant.manager_rev)),
        (3, cbor_bytes(grant.hpke_enc)),
        (4, cbor_bytes(grant.hpke_ciphertext.clone())),
        (5, cbor_bytes(grant.signed_header.clone())),
        (6, cbor_bytes(grant.signature.clone().unwrap_or_default())),
        // §28.1 条目 5（评审 §三 4）：grant_id / activation_ref / 完整 leaf manifest 与包 digest。
        (9, cbor_bytes(sha256(&grant.signed_header))),
        (10, cbor_bytes(package_digest)),
    ];
    if let Some(grant_id) = grant.grant_id {
        fields.push((7, cbor_bytes(grant_id)));
    }
    if let Some(activation_ref) = grant.activation_ref {
        fields.push((8, cbor_bytes(activation_ref)));
    }
    fields.sort_by_key(|(key, _)| *key);
    Ok(cbor_ok(fields))
}

// ---------------------------------------------------------------- WS /wake

async fn wake(
    State(state): State<Arc<AppState>>,
    face: ApiFace,
    Path(vault_id): Path<String>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Result<Response> {
    state.check_v2_data_rate(face)?;
    let vault_id = parse_id16(&vault_id)?;
    let token = bearer(&headers)?;
    // 升级前先验证一次：无效/已吊销/跨 vault 的 token 直接拒绝，不建立 WS。
    match face {
        // v1 旧行为逐字节保留：这里历史上**不做**跨 vault 复查，跨 vault token → 401。
        ApiFace::V1 => {
            let clock = state.db.clock_handle();
            let probe = token.clone();
            state.db.read(vault_id.into(), move |conn| {
                auth::authenticate(conn, clock.as_ref(), &probe)?;
                Ok(())
            })?;
        }
        // v2：与其它 v2 路由同一语义（跨 vault token → 404，不泄露目标租户）。
        ApiFace::V2 => {
            authenticate_token_face(&state, &token, vault_id.into(), face)?;
        }
    }
    Ok(ws.on_upgrade(move |socket| wake_loop(state, vault_id, token, face, socket)))
}

async fn wake_loop(
    state: Arc<AppState>,
    vault_id: Id16,
    token: String,
    face: ApiFace,
    mut socket: WebSocket,
) {
    // 先订阅再推首帧：否则"首帧水位的查询"与"订阅"之间的 notify 会丢
    // （测试里也会变成依赖时序的偶发失败）。
    let mut events = state.wake.subscribe();
    if !push_wake(&state, &vault_id, &token, face, &mut socket).await {
        return;
    }
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
                if !push_wake(&state, &vault_id, &token, face, &mut socket).await {
                    return;
                }
            }
            _ = ticker.tick() => {
                if !push_wake(&state, &vault_id, &token, face, &mut socket).await {
                    return;
                }
            }
        }
    }
}

/// 重新鉴权并推送最新水位；返回 false 表示应断开（吊销/过期/跨 vault/IO 错误）。
async fn push_wake(
    state: &AppState,
    vault_id: &Id16,
    token: &str,
    face: ApiFace,
    socket: &mut WebSocket,
) -> bool {
    let vault_id = *vault_id;
    let clock = state.db.clock_handle();
    let token = token.to_string();
    let snapshot = state.db.read(vault_id.into(), move |conn| {
        let ctx = auth::authenticate(conn, clock.as_ref(), &token)?;
        if face.is_v2() {
            // v2：route vault 必须就是 token 所属 vault（防御性；查询本身已带 vault_id）。
            ensure_same_vault(&ctx, conn.scope())?;
        }
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
    // 多租户：GC 是**每 vault 独立**的（预算/日志/快照/计划都按 vault 隔离），
    // 逐个 vault 各跑一个写事务；一个 vault 的失败不影响其它 vault。
    let vault_ids = state.db.read_instance(crate::db::schema::list_vault_ids)?;
    let mut total = gc::GcOutcome::default();
    for vault_id in vault_ids {
        let scope = VaultScope::new(vault_id);
        let outcome = state
            .write(scope, move |conn, clock| gc::tick(conn, clock))
            .await?;
        total.deleted_log_rows += outcome.deleted_log_rows;
        total.retired_update_objects += outcome.retired_update_objects;
        total.deleted_staging_objects += outcome.deleted_staging_objects;
        total.purged_slots += outcome.purged_slots;
        total.pruned_checkpoints += outcome.pruned_checkpoints;
        total.retired_pruned_objects += outcome.retired_pruned_objects;
        total.blocked |= outcome.blocked;
        total.unhealthy.extend(outcome.unhealthy);
        total.floor = total.floor.max(outcome.floor);
        total.cut_seq = total.cut_seq.max(outcome.cut_seq);
    }
    Ok(total)
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
