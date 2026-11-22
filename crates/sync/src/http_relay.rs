//! 真实 relay 的 HTTP/CBOR 客户端（ADR 0006 §5；S7-9）。
//!
//! 控制面形状（与 `crates/relay/src/api.rs` 顶部 wire 约定一致）：
//!
//! - 所有控制面请求/响应 = `application/cbor` 的整数键 CDE map（`sync-proto::cde` 保证
//!   canonical：键升序、无重复、可选字段省略）；
//! - ID/hash/签名 = byte string；文本字段 = text；
//! - 签名包装 = `{0: Bytes(unsigned_body), 1: Bytes(signature)}`；
//! - 错误体 = `{0: code, 1: retryable, 2: current_epoch?, 3: current_control_rev?,
//!   4: bootstrap_hint?}`；
//! - 对象分片仍是 `application/octet-stream` 原样字节。
//!
//! 客户端职责：
//!
//! 1. challenge→token 认证（600s，提前 60s 刷新）；
//! 2. state/log/objects/updates/acks 全走 CBOR；
//! 3. **log 事件读签名**（事件 map 键 5），交给引擎按 ADR §2 验 `ObjectCommit`；
//! 4. `GET P/control?after_rev` 拉控制链 + 成员证书，**按 `control_rev` 缓存/失效**；
//!    成员证书按 §17.1 用管理公钥验签（relay 若未带 `certificate_signature`，
//!    见 [`parse_control`] 的文档化降级：只能信任已鉴权控制面 + 链哈希）。
//!
//! **TLS**：本文件只实现 `http://`（ADR §7 localhost 开发路径）。生产走反向代理 TLS 终止，
//! 桌面直连 HTTPS 需要单独的 TLS transport（未实现）。

use crate::base64url;
use crate::cbor;
use crate::device::{verify_signature, DeviceSigningKey};
use crate::error::SyncError;
use crate::relay::{
    BootstrapBundle, BootstrapCheckpoint, BootstrapLease, CheckpointLease, CheckpointPublish,
    CheckpointRef, CheckpointStatus, FrozenAssetPage, FrozenAssetRef, ManagerChainEntry,
    ObjectSubmission, PublishedCheckpoint, RelayClient, RelayLogEvent, RelayLogKind, RelayLogPage,
    RelayState, RotationBegin, RotationView, SlotRecord, SlotUpload, UpdateOutcome, UpdateStatus,
    UpdateSubmission,
};
use crate::{DeviceId, ObjectId, VaultId};
use menghunote_sync_proto::{
    auth_signing_input, device_cert_signed_bytes, AuthRealm, Challenge, DeviceCertV1,
};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// 控制面 Content-Type。
pub const CONTENT_TYPE_CBOR: &str = "application/cbor";
/// 对象分片 Content-Type。
pub const CONTENT_TYPE_OCTET: &str = "application/octet-stream";

/// 时间源（token 刷新用；测试可注入 FakeClock）。
pub trait Clock {
    /// 当前 Unix 秒。
    fn now_secs(&self) -> u64;
}

/// 系统时钟。
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_secs(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs())
    }
}

/// 一个 HTTP 请求。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRequest {
    /// 方法（GET/PUT/POST）。
    pub method: &'static str,
    /// 路径 + query（以 `/` 开头）。
    pub path: String,
    /// 头（小写名）。
    pub headers: Vec<(String, String)>,
    /// 请求体。
    pub body: Vec<u8>,
}

/// 一个 HTTP 响应。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    /// 状态码。
    pub status: u16,
    /// 响应体。
    pub body: Vec<u8>,
}

/// HTTP 传输抽象（测试注入点）。
pub trait HttpTransport {
    /// 发送请求并返回响应；网络/读截断返回 [`SyncError::Transport`]。
    fn send(&mut self, request: &HttpRequest) -> Result<HttpResponse, SyncError>;
}

/// 使用 `std::net::TcpStream` 的极简 HTTP/1.1 传输（**只支持 http://**）。
pub struct StdTcpTransport {
    host: String,
    port: u16,
    timeout: Duration,
}

impl StdTcpTransport {
    /// 从 `http://host:port` 构造。
    pub fn new(base_url: &str) -> Result<Self, SyncError> {
        let rest = base_url
            .strip_prefix("http://")
            .ok_or(SyncError::Unsupported(
                "HTTPS 传输尚未实现（请用反向代理 TLS 或 http://localhost 开发）",
            ))?;
        let rest = rest.trim_end_matches('/');
        let (host, port) = match rest.rsplit_once(':') {
            Some((host, port)) => (
                host.to_owned(),
                port.parse::<u16>()
                    .map_err(|_| SyncError::Transport("base_url 端口非法"))?,
            ),
            None => (rest.to_owned(), 80),
        };
        if host.is_empty() {
            return Err(SyncError::Transport("base_url 缺少主机名"));
        }
        Ok(Self {
            host,
            port,
            timeout: Duration::from_secs(30),
        })
    }
}

impl HttpTransport for StdTcpTransport {
    fn send(&mut self, request: &HttpRequest) -> Result<HttpResponse, SyncError> {
        let mut stream = TcpStream::connect((self.host.as_str(), self.port))
            .map_err(|_| SyncError::Transport("连接 relay 失败"))?;
        stream
            .set_read_timeout(Some(self.timeout))
            .map_err(|_| SyncError::Transport("设置读超时失败"))?;
        stream
            .set_write_timeout(Some(self.timeout))
            .map_err(|_| SyncError::Transport("设置写超时失败"))?;

        let mut head = format!(
            "{} {} HTTP/1.1\r\nHost: {}:{}\r\nConnection: close\r\nContent-Length: {}\r\n",
            request.method,
            request.path,
            self.host,
            self.port,
            request.body.len()
        );
        for (name, value) in &request.headers {
            head.push_str(name);
            head.push_str(": ");
            head.push_str(value);
            head.push_str("\r\n");
        }
        head.push_str("\r\n");
        stream
            .write_all(head.as_bytes())
            .and_then(|()| stream.write_all(&request.body))
            .map_err(|_| SyncError::Transport("写请求失败"))?;

        let mut raw = Vec::new();
        stream
            .read_to_end(&mut raw)
            .map_err(|_| SyncError::Transport("读响应失败/被截断"))?;
        parse_http_response(&raw)
    }
}

fn parse_http_response(raw: &[u8]) -> Result<HttpResponse, SyncError> {
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or(SyncError::Transport("响应头不完整"))?;
    let head =
        std::str::from_utf8(&raw[..split]).map_err(|_| SyncError::Transport("响应头不是 ASCII"))?;
    let mut lines = head.split("\r\n");
    let status_line = lines.next().ok_or(SyncError::Transport("缺少状态行"))?;
    let status = status_line
        .split_whitespace()
        .nth(1)
        .ok_or(SyncError::Transport("状态行非法"))?
        .parse::<u16>()
        .map_err(|_| SyncError::Transport("状态码非法"))?;
    let mut chunked = false;
    let mut content_length: Option<usize> = None;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            let name = name.trim().to_ascii_lowercase();
            let value = value.trim();
            if name == "content-length" {
                content_length = value.parse().ok();
            } else if name == "transfer-encoding" && value.eq_ignore_ascii_case("chunked") {
                chunked = true;
            }
        }
    }
    if chunked {
        return Err(SyncError::Transport("不支持 chunked 响应"));
    }
    let body = &raw[split + 4..];
    if let Some(length) = content_length {
        if body.len() < length {
            return Err(SyncError::Transport("响应体被截断"));
        }
        return Ok(HttpResponse {
            status,
            body: body[..length].to_vec(),
        });
    }
    Ok(HttpResponse {
        status,
        body: body.to_vec(),
    })
}

/// HTTP 客户端配置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRelayConfig {
    /// relay 基址（`http://host:port`，可带路径前缀）。
    pub base_url: String,
    /// vault 身份。
    pub vault_id: VaultId,
    /// 本机设备身份。
    pub device_id: DeviceId,
    /// 控制链信任锚：管理公钥（§17.1 验设备证书用）。
    ///
    /// genesis/bootstrapping 设备从恢复码派生后填入；`None` 时只能解析证书、
    /// 无法完成 §17.1 验签（会降级为"信任已鉴权控制面"）。
    pub manager_pub: Option<[u8; 32]>,
}

#[derive(Debug, Clone)]
struct TokenState {
    token: String,
    expires_at: u64,
}

/// `/control` 的验证后缓存。
#[derive(Debug, Clone)]
struct ControlState {
    control_rev: u64,
    epoch: u64,
    /// 只缓存 active 成员的证书。
    certs: HashMap<DeviceId, DeviceCertV1>,
    /// §30.3 键 4：typed 控制记录 kind（旧记录为 `raw`）。
    kinds: Vec<(u64, String)>,
    /// active 成员证书的 digest 输入（device_id/cert/signature）。
    member_cert_inputs: Vec<menghunote_sync_proto::MemberCertDigestInputV1>,
}

/// relay 的 HTTP 实现。
pub struct HttpRelayClient<T: HttpTransport, C: Clock = SystemClock> {
    transport: T,
    config: HttpRelayConfig,
    signing_key: DeviceSigningKey,
    clock: C,
    token: Option<TokenState>,
    control: Option<ControlState>,
}

impl<T: HttpTransport> HttpRelayClient<T, SystemClock> {
    /// 用系统时钟构造。
    pub fn new(transport: T, config: HttpRelayConfig, signing_key: DeviceSigningKey) -> Self {
        Self::with_clock(transport, config, signing_key, SystemClock)
    }
}

impl<T: HttpTransport, C: Clock> HttpRelayClient<T, C> {
    /// 指定时钟构造（测试 token 刷新）。
    pub fn with_clock(
        transport: T,
        config: HttpRelayConfig,
        signing_key: DeviceSigningKey,
        clock: C,
    ) -> Self {
        Self {
            transport,
            config,
            signing_key,
            clock,
            token: None,
            control: None,
        }
    }

    /// 已缓存的 control_rev（测试/诊断用）。
    pub fn cached_control_rev(&self) -> Option<u64> {
        self.control.as_ref().map(|control| control.control_rev)
    }

    /// §28.1：当前 active 成员证书集合 digest（SHA256(proto 字节输入)）。
    pub fn active_member_cert_digest(&mut self) -> Result<[u8; 32], SyncError> {
        if self.control.is_none() {
            self.refresh_control()?;
        }
        let Some(control) = &self.control else {
            return Err(SyncError::RelayWire("control 缓存为空"));
        };
        let input = menghunote_sync_proto::member_cert_digest(&control.member_cert_inputs);
        Ok(Sha256::digest(&input).into())
    }

    /// rotation begin 会 bump `auth_rev` 使全部旧 token 失效；调用方必须清缓存重认证。
    pub fn clear_auth_cache(&mut self) {
        self.token = None;
        self.control = None;
    }

    /// §30.3 键 4：已缓存的控制记录 `(rev, kind)`（旧记录为 `raw`）。
    #[must_use]
    pub fn control_kinds(&self) -> Vec<(u64, String)> {
        self.control
            .as_ref()
            .map(|control| control.kinds.clone())
            .unwrap_or_default()
    }

    /// 从 `GET /state` 键 14 读初始游标（relay task-86 的兜底；旧版没有该字段）。
    ///
    /// genesis 响应键 5 是首选来源；两者都没有时必须走 bootstrap，不能把它当成
    /// 网络错误反复重试。
    pub fn initial_cursor(&mut self) -> Result<String, SyncError> {
        let fields = self.send_cbor("GET", self.vault_path("/state"), None)?;
        cbor::opt_text_field(&fields, 14)?.ok_or(SyncError::Unsupported(
            "relay 未提供初始游标（genesis 键 5 / state 键 14）；必须先 bootstrap",
        ))
    }

    fn vault_path(&self, suffix: &str) -> String {
        format!(
            "/v1/vaults/{}{suffix}",
            base64url::encode(&self.config.vault_id)
        )
    }

    fn origin(&self) -> &str {
        self.config.base_url.trim_end_matches('/')
    }

    // ------------------------------------------------------------ 认证

    fn ensure_token(&mut self) -> Result<String, SyncError> {
        let now = self.clock.now_secs();
        if let Some(token) = &self.token {
            // 提前 60s 刷新（ADR §5：600s 有效）。
            if token.expires_at > now.saturating_add(60) {
                return Ok(token.token.clone());
            }
        }
        self.authenticate(now)?;
        self.token
            .as_ref()
            .map(|token| token.token.clone())
            .ok_or(SyncError::RelayWire("token 刷新后仍为空"))
    }

    fn authenticate(&mut self, now: u64) -> Result<(), SyncError> {
        let body = cbor::encode(vec![
            (0, cbor::uint(1)), // realm = device
            (1, cbor::bytes(self.config.device_id)),
        ])?;
        let response = self.transport.send(&HttpRequest {
            method: "POST",
            path: "/v1/auth/challenges".to_owned(),
            headers: cbor_headers(),
            body,
        })?;
        let fields = decode_ok(&response)?;
        let challenge_id: [u8; 16] = cbor::bytes_fixed(&fields, 0)?;
        let realm = cbor::uint_field(&fields, 1)?;
        if realm != u64::from(AuthRealm::Device as u8) {
            return Err(SyncError::Unsupported("relay 只支持 device realm"));
        }
        let subject: [u8; 16] = cbor::bytes_fixed(&fields, 2)?;
        if subject != self.config.device_id {
            return Err(SyncError::RelayWire("challenge subject 与设备不一致"));
        }
        let nonce: [u8; 32] = cbor::bytes_fixed(&fields, 3)?;
        let expires_at = cbor::uint_field(&fields, 4)?;
        let vault_id: [u8; 16] = cbor::bytes_fixed(&fields, 5)?;
        let instance_id: [u8; 16] = cbor::bytes_fixed(&fields, 6)?;
        let _server_time = cbor::uint_field(&fields, 7)?;
        if vault_id != self.config.vault_id {
            return Err(SyncError::RelayWire("challenge.vault_id 不符"));
        }
        if expires_at <= now {
            return Err(SyncError::RelayApi {
                code: "CHALLENGE_INVALID".to_owned(),
                retryable: true,
                current_epoch: None,
                current_control_rev: None,
                bootstrap_hint: None,
            });
        }
        let origin_sha256: [u8; 32] = Sha256::digest(self.origin().as_bytes()).into();
        let message = Challenge {
            pv: 1,
            relay_instance_id: instance_id,
            origin_sha256,
            vault_id,
            realm: AuthRealm::Device,
            subject: self.config.device_id,
            challenge_id,
            nonce,
            expires_at,
        };
        let signed = auth_signing_input(&message)?;
        let signature = self.signing_key.sign(&signed);
        let body = cbor::encode(vec![
            (0, cbor::bytes(challenge_id.to_vec())),
            (1, cbor::bytes(message.encode()?)),
            (2, cbor::bytes(signature)),
        ])?;
        let response = self.transport.send(&HttpRequest {
            method: "POST",
            path: "/v1/auth/tokens".to_owned(),
            headers: cbor_headers(),
            body,
        })?;
        let fields = decode_ok(&response)?;
        let token = cbor::text_field(&fields, 0)?;
        let expires_at = cbor::uint_field(&fields, 1)?;
        let _server_time = cbor::uint_field(&fields, 2)?;
        let realm = cbor::uint_field(&fields, 3)?;
        let _auth_rev = cbor::uint_field(&fields, 4)?;
        if realm != u64::from(AuthRealm::Device as u8) {
            return Err(SyncError::RelayWire("token realm 不符"));
        }
        if token.is_empty() {
            return Err(SyncError::RelayWire("token 为空"));
        }
        self.token = Some(TokenState { token, expires_at });
        Ok(())
    }

    // ------------------------------------------------------------ 请求

    fn send(
        &mut self,
        method: &'static str,
        path: String,
        content_type: Option<&str>,
        body: Vec<u8>,
    ) -> Result<HttpResponse, SyncError> {
        let token = self.ensure_token()?;
        let mut headers = vec![("authorization".to_owned(), format!("Bearer {token}"))];
        if let Some(content_type) = content_type {
            headers.push(("content-type".to_owned(), content_type.to_owned()));
        }
        self.transport.send(&HttpRequest {
            method,
            path,
            headers,
            body,
        })
    }

    fn send_cbor(
        &mut self,
        method: &'static str,
        path: String,
        fields: Option<cbor::Fields>,
    ) -> Result<cbor::Fields, SyncError> {
        let body = fields.map(cbor::encode).transpose()?.unwrap_or_default();
        let response = self.send(method, path, Some(CONTENT_TYPE_CBOR), body)?;
        decode_ok(&response)
    }

    fn read_state(&mut self) -> Result<RelayState, SyncError> {
        let fields = self.send_cbor("GET", self.vault_path("/state"), None)?;
        let vault_id: [u8; 16] = cbor::bytes_fixed(&fields, 0)?;
        let log_id: [u8; 16] = cbor::bytes_fixed(&fields, 1)?;
        let instance_id: [u8; 16] = cbor::bytes_fixed(&fields, 2)?;
        let _state = cbor::text_field(&fields, 3)?;
        let epoch = cbor::uint_field(&fields, 4)?;
        let control_rev = cbor::uint_field(&fields, 5)?;
        let auth_rev = cbor::uint_field(&fields, 6)?;
        let head_seq = cbor::uint_field(&fields, 7)?;
        let gc_floor = cbor::uint_field(&fields, 8)?;
        let current_rotation_id = cbor::opt_bytes_field(&fields, 15)?
            .map(|bytes| {
                bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| SyncError::RelayWire("state.current_rotation_id 长度非法"))
            })
            .transpose()?;
        if vault_id != self.config.vault_id {
            return Err(SyncError::RelayWire("state.vault_id 与本地不一致"));
        }
        Ok(RelayState {
            vault_id,
            log_id,
            instance_id,
            epoch,
            control_rev,
            auth_rev,
            head_seq,
            gc_floor,
            current_rotation_id,
        })
    }

    fn refresh_control(&mut self) -> Result<(), SyncError> {
        let path = format!("{}/control?after_rev=0", self.vault_path(""));
        let fields = self.send_cbor("GET", path, None)?;
        let control = parse_control(
            &fields,
            &self.config.vault_id,
            self.config.manager_pub.as_ref(),
        )?;
        self.control = Some(control);
        Ok(())
    }
}

impl<T: HttpTransport, C: Clock> RelayClient for HttpRelayClient<T, C> {
    fn state(&mut self) -> Result<RelayState, SyncError> {
        let state = self.read_state()?;
        // control_rev 前进 → 旧成员证书缓存失效。
        if let Some(control) = &self.control {
            if state.control_rev > control.control_rev || state.epoch != control.epoch {
                self.control = None;
            }
        }
        Ok(state)
    }

    fn log(&mut self, cursor: Option<&str>, limit: u32) -> Result<RelayLogPage, SyncError> {
        let cursor = cursor.ok_or(SyncError::Unsupported(
            "relay 需要 bootstrap 提供的初始游标",
        ))?;
        let path = format!(
            "{}?after={}&limit={limit}",
            self.vault_path("/log"),
            percent_encode(cursor)
        );
        let fields = self.send_cbor("GET", path, None)?;
        let events = cbor::array_field(&fields, 0)?;
        let mut parsed = Vec::with_capacity(events.len());
        for event in events {
            let event = cbor::value_map(&event)?;
            let relay_seq = cbor::uint_field(&event, 0)?;
            let kind_text = cbor::text_field(&event, 1)?;
            let kind =
                RelayLogKind::parse(&kind_text).ok_or(SyncError::RelayWire("未知日志事件类型"))?;
            let object_id = match cbor::opt_bytes_field(&event, 2)? {
                Some(bytes) => Some(
                    bytes
                        .as_slice()
                        .try_into()
                        .map_err(|_| SyncError::RelayWire("日志 object_id 不是 32 字节"))?,
                ),
                None => None,
            };
            let epoch = cbor::uint_field(&event, 3)?;
            let payload = cbor::bytes_field(&event, 4)?;
            let signature = match cbor::opt_bytes_field(&event, 5)? {
                Some(bytes) => Some(
                    bytes
                        .as_slice()
                        .try_into()
                        .map_err(|_| SyncError::RelayWire("日志签名不是 64 字节"))?,
                ),
                None => None,
            };
            parsed.push(RelayLogEvent {
                relay_seq,
                kind,
                object_id,
                epoch,
                event: payload,
                signature,
            });
        }
        Ok(RelayLogPage {
            events: parsed,
            next_cursor: cbor::text_field(&fields, 1)?,
            through_seq: cbor::uint_field(&fields, 2)?,
            query_head: cbor::uint_field(&fields, 3)?,
            epoch: cbor::uint_field(&fields, 4)?,
            gc_floor: cbor::uint_field(&fields, 5)?,
        })
    }

    fn part(&mut self, object_id: &ObjectId, part_no: u32) -> Result<Vec<u8>, SyncError> {
        let path = format!(
            "/v1/vaults/{}/objects/{}/parts/{part_no}",
            base64url::encode(&self.config.vault_id),
            base64url::encode(object_id)
        );
        let response = self.send("GET", path, None, Vec::new())?;
        match response.status {
            200 => Ok(response.body),
            _ => Err(error_from_response(&response)),
        }
    }

    fn put_part(
        &mut self,
        object_id: &ObjectId,
        part_no: u32,
        envelope: &[u8],
    ) -> Result<(), SyncError> {
        let path = format!(
            "/v1/vaults/{}/objects/{}/parts/{part_no}",
            base64url::encode(&self.config.vault_id),
            base64url::encode(object_id)
        );
        let response = self.send("PUT", path, Some(CONTENT_TYPE_OCTET), envelope.to_vec())?;
        match response.status {
            200 | 201 => Ok(()),
            _ => Err(error_from_response(&response)),
        }
    }

    fn put_part_with_reservation(
        &mut self,
        object_id: &ObjectId,
        part_no: u32,
        envelope: &[u8],
        reservation: Option<&[u8; 16]>,
    ) -> Result<(), SyncError> {
        let Some(reservation) = reservation else {
            return self.put_part(object_id, part_no, envelope);
        };
        let path = format!(
            "/v1/vaults/{}/objects/{}/parts/{part_no}?reservation={}",
            base64url::encode(&self.config.vault_id),
            base64url::encode(object_id),
            base64url::encode(reservation)
        );
        let response = self.send("PUT", path, Some(CONTENT_TYPE_OCTET), envelope.to_vec())?;
        match response.status {
            200 | 201 => Ok(()),
            _ => Err(error_from_response(&response)),
        }
    }

    fn submit_updates(
        &mut self,
        items: Vec<UpdateSubmission>,
    ) -> Result<Vec<UpdateOutcome>, SyncError> {
        if items.is_empty() {
            return Ok(Vec::new());
        }
        if items.len() > 256 {
            return Err(SyncError::Unsupported("单批 update 超过 256"));
        }
        let entries = items
            .iter()
            .map(|item| {
                cbor::CdeValue::Map(vec![
                    (0, cbor::bytes(item.object_id)),
                    (1, cbor::bytes(item.signed_commit.clone())),
                    (2, cbor::bytes(item.signature)),
                ])
            })
            .collect();
        let fields = self.send_cbor(
            "POST",
            self.vault_path("/updates"),
            Some(vec![(0, cbor::array(entries))]),
        )?;
        let results = cbor::array_field(&fields, 0)?;
        let mut outcomes = Vec::with_capacity(results.len());
        for result in results {
            let result = cbor::value_map(&result)?;
            let object_id: [u8; 32] = cbor::bytes_fixed(&result, 0)?;
            let status_text = cbor::text_field(&result, 1)?;
            let relay_seq = cbor::opt_uint_field(&result, 2)?;
            let code = cbor::opt_text_field(&result, 3)?;
            let retryable = cbor::bool_field(&result, 4)?;
            let status = match status_text.as_str() {
                "accepted" => UpdateStatus::Accepted { relay_seq },
                "duplicate" => UpdateStatus::Duplicate,
                "failed" => UpdateStatus::Failed { code, retryable },
                _ => return Err(SyncError::RelayWire("未知 update 结果状态")),
            };
            outcomes.push(UpdateOutcome { object_id, status });
        }
        Ok(outcomes)
    }

    fn ack(&mut self, received_seq: u64, integrated_seq: u64, epoch: u64) -> Result<(), SyncError> {
        let body = cbor::encode(vec![
            (0, cbor::uint(received_seq)),
            (1, cbor::uint(integrated_seq)),
            (2, cbor::uint(epoch)),
        ])?;
        let response = self.send(
            "POST",
            self.vault_path("/acks"),
            Some(CONTENT_TYPE_CBOR),
            body,
        )?;
        match response.status {
            200 | 204 => Ok(()),
            _ => Err(error_from_response(&response)),
        }
    }

    fn device_cert(&mut self, device_id: &DeviceId) -> Result<Option<DeviceCertV1>, SyncError> {
        if self.control.is_none() {
            self.refresh_control()?;
        }
        Ok(self
            .control
            .as_ref()
            .and_then(|control| control.certs.get(device_id))
            .cloned())
    }

    fn submit_object(&mut self, item: ObjectSubmission) -> Result<UpdateOutcome, SyncError> {
        let entry = cbor::CdeValue::Map(vec![
            (0, cbor::bytes(item.object_id)),
            (1, cbor::bytes(item.signed_commit)),
            (2, cbor::bytes(item.signature)),
        ]);
        let fields = self.send_cbor(
            "POST",
            self.vault_path("/updates"),
            Some(vec![(0, cbor::array(vec![entry]))]),
        )?;
        let results = cbor::array_field(&fields, 0)?;
        let result = results
            .first()
            .ok_or(SyncError::RelayWire("submit_object 响应缺少结果"))?;
        parse_update_outcome(&cbor::value_map(result)?)
    }

    fn put_key_slot(&mut self, upload: SlotUpload) -> Result<SlotRecord, SyncError> {
        let path = format!(
            "{}/key-slots/{}",
            self.vault_path(""),
            base64url::encode(&upload.slot_id)
        );
        let signed = cbor::CdeValue::Map(vec![
            (0, cbor::bytes(upload.signed_message)),
            (1, cbor::bytes(upload.signature)),
        ]);
        let fields = self.send_cbor(
            "PUT",
            path,
            Some(vec![
                (0, cbor::uint(upload.epoch)),
                (1, cbor::text(slot_kind_text(upload.kind))),
                (2, cbor::bytes(upload.descriptor)),
                (3, signed),
            ]),
        )?;
        let slot = cbor::value_map(&cbor::field(&fields, 0)?.clone())?;
        parse_slot_record(&slot)
    }

    fn list_key_slots(&mut self) -> Result<Vec<SlotRecord>, SyncError> {
        let fields = self.send_cbor("GET", format!("{}/key-slots", self.vault_path("")), None)?;
        let mut slots = Vec::new();
        for value in cbor::array_field(&fields, 0)? {
            slots.push(parse_slot_record(&cbor::value_map(&value)?)?);
        }
        Ok(slots)
    }

    fn activate_password_slot(
        &mut self,
        expected_slot_set_rev: u64,
        slot_id: ObjectId,
        signed_message: Vec<u8>,
        signature: [u8; 64],
    ) -> Result<u64, SyncError> {
        let signed = cbor::CdeValue::Map(vec![
            (0, cbor::bytes(signed_message)),
            (1, cbor::bytes(signature)),
        ]);
        let fields = self.send_cbor(
            "POST",
            format!("{}/key-slots/password/activate", self.vault_path("")),
            Some(vec![
                (0, cbor::uint(expected_slot_set_rev)),
                (1, cbor::bytes(slot_id)),
                (2, signed),
            ]),
        )?;
        cbor::uint_field(&fields, 0)
    }

    fn create_checkpoint(
        &mut self,
        estimated_bytes: u64,
        epoch: Option<u64>,
    ) -> Result<CheckpointLease, SyncError> {
        self.create_checkpoint_with_role(estimated_bytes, epoch, None)
    }

    fn create_checkpoint_with_role(
        &mut self,
        estimated_bytes: u64,
        epoch: Option<u64>,
        role: Option<&str>,
    ) -> Result<CheckpointLease, SyncError> {
        let mut body = vec![(0, cbor::uint(estimated_bytes))];
        if let Some(epoch) = epoch {
            body.push((1, cbor::uint(epoch)));
        }
        if let Some(role) = role {
            body.push((2, cbor::text(role)));
        }
        let fields = self.send_cbor(
            "POST",
            format!("{}/checkpoints", self.vault_path("")),
            Some(body),
        )?;
        Ok(CheckpointLease {
            checkpoint_id: cbor::bytes_fixed(&fields, 0)?,
            epoch: cbor::uint_field(&fields, 1)?,
            covered_seq: cbor::uint_field(&fields, 2)?,
            reservation_id: cbor::bytes_fixed(&fields, 3)?,
            expires_at: cbor::uint_field(&fields, 4)?,
        })
    }

    fn add_checkpoint_refs(
        &mut self,
        checkpoint_id: menghunote_sync_proto::CheckpointId,
        refs: Vec<CheckpointRef>,
    ) -> Result<u64, SyncError> {
        let entries = refs
            .into_iter()
            .map(|reference| {
                cbor::CdeValue::Map(vec![
                    (0, cbor::bytes(reference.object_id)),
                    (1, cbor::text(reference.role.as_str())),
                ])
            })
            .collect();
        let path = format!(
            "{}/checkpoints/{}/refs",
            self.vault_path(""),
            base64url::encode(&checkpoint_id)
        );
        let fields = self.send_cbor("POST", path, Some(vec![(0, cbor::array(entries))]))?;
        cbor::uint_field(&fields, 0)
    }

    fn publish_checkpoint(
        &mut self,
        publish: CheckpointPublish,
    ) -> Result<PublishedCheckpoint, SyncError> {
        let path = format!(
            "{}/checkpoints/{}/publish",
            self.vault_path(""),
            base64url::encode(&publish.checkpoint_id)
        );
        let fields = self.send_cbor(
            "POST",
            path,
            Some(vec![
                (0, cbor::uint(publish.covered_seq)),
                (1, cbor::bytes(publish.manifest_oid)),
                (2, cbor::bytes(publish.refs_digest)),
                (3, cbor::bytes(publish.signed_commit)),
                (4, cbor::bytes(publish.signature)),
            ]),
        )?;
        Ok(PublishedCheckpoint {
            checkpoint_id: cbor::bytes_fixed(&fields, 0)?,
            epoch: cbor::uint_field(&fields, 1)?,
            covered_seq: cbor::uint_field(&fields, 2)?,
            published_at: cbor::uint_field(&fields, 3)?,
        })
    }

    fn bootstrap(&mut self) -> Result<BootstrapBundle, SyncError> {
        bootstrap_impl(self, false)
    }

    fn bootstrap_degraded(&mut self) -> Result<BootstrapBundle, SyncError> {
        bootstrap_impl(self, true)
    }

    fn renew_bootstrap_lease(&mut self, lease_id: [u8; 16]) -> Result<BootstrapLease, SyncError> {
        let fields = self.send_cbor(
            "PATCH",
            format!(
                "{}/bootstrap-leases/{}",
                self.vault_path(""),
                base64url::encode(&lease_id)
            ),
            None,
        )?;
        Ok(BootstrapLease {
            lease_id: cbor::bytes_fixed(&fields, 0)?,
            expires_at: cbor::uint_field(&fields, 1)?,
            floor_seq: cbor::uint_field(&fields, 2)?,
        })
    }

    fn begin_rotation(&mut self, begin: RotationBegin) -> Result<RotationView, SyncError> {
        let signed = cbor::CdeValue::Map(vec![
            (0, cbor::bytes(begin.signed_body)),
            (1, cbor::bytes(begin.signature.to_vec())),
        ]);
        let fields = self.send_cbor(
            "POST",
            self.vault_path("/rotations"),
            Some(vec![
                (0, signed),
                (1, cbor::bytes(begin.request_id.to_vec())),
                (2, cbor::uint(begin.expected_control_rev)),
                (3, cbor::uint(begin.expected_epoch)),
                (
                    4,
                    cbor::array(
                        begin
                            .revoke
                            .iter()
                            .map(|device| cbor::bytes(device.to_vec()))
                            .collect(),
                    ),
                ),
            ]),
        )?;
        parse_rotation_view(&fields)
    }

    fn rotation_for_request(
        &mut self,
        request_id: [u8; 16],
    ) -> Result<Option<RotationView>, SyncError> {
        let fields = self.send_cbor(
            "GET",
            format!(
                "{}/rotations/current?request_id={}",
                self.vault_path(""),
                base64url::encode(&request_id)
            ),
            None,
        )?;
        match cbor::field(&fields, 0) {
            Ok(_) => parse_rotation_view(&fields).map(Some),
            Err(_) => Ok(None),
        }
    }

    fn current_rotation(&mut self) -> Result<Option<RotationView>, SyncError> {
        let fields = self.send_cbor(
            "GET",
            format!("{}/rotations/current", self.vault_path("")),
            None,
        )?;
        match cbor::field(&fields, 0) {
            Ok(_) => parse_rotation_view(&fields).map(Some),
            Err(_) => Ok(None),
        }
    }

    fn rotation(&mut self, rotation_id: [u8; 16]) -> Result<RotationView, SyncError> {
        let fields = self.send_cbor(
            "GET",
            format!(
                "{}/rotations/{}",
                self.vault_path(""),
                base64url::encode(&rotation_id)
            ),
            None,
        )?;
        parse_rotation_view(&fields)
    }

    fn management_chain(&mut self) -> Result<Vec<ManagerChainEntry>, SyncError> {
        let fields = self.send_cbor(
            "GET",
            format!("{}/control?after_rev=0", self.vault_path("")),
            None,
        )?;
        cbor::array_field(&fields, 5)?
            .iter()
            .map(parse_manager_chain_entry)
            .collect()
    }

    fn active_member_hpke_keys(&mut self) -> Result<Vec<(DeviceId, [u8; 32])>, SyncError> {
        if self.control.is_none() {
            self.refresh_control()?;
        }
        let mut keys = self
            .control
            .as_ref()
            .map(|control| {
                control
                    .certs
                    .iter()
                    .map(|(device_id, cert)| (*device_id, cert.hpke_x25519_pub))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        keys.sort_by_key(|(device_id, _)| *device_id);
        Ok(keys)
    }

    fn rotation_assets(
        &mut self,
        rotation_id: [u8; 16],
        after: Option<[u8; 32]>,
        limit: u32,
    ) -> Result<FrozenAssetPage, SyncError> {
        let mut path = format!(
            "{}/rotations/{}/assets?limit={limit}",
            self.vault_path(""),
            base64url::encode(&rotation_id)
        );
        if let Some(after) = after {
            path.push_str(&format!("&after={}", base64url::encode(&after)));
        }
        let fields = self.send_cbor("GET", path, None)?;
        let assets = cbor::array_field(&fields, 0)?
            .iter()
            .map(parse_frozen_asset)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(FrozenAssetPage {
            assets,
            freeze_seq: cbor::uint_field(&fields, 1)?,
            asset_set_digest: cbor::opt_bytes_field(&fields, 2)?
                .map(|bytes| {
                    bytes
                        .as_slice()
                        .try_into()
                        .map_err(|_| SyncError::RelayWire("asset_set_digest 长度非法"))
                })
                .transpose()?,
            total: cbor::uint_field(&fields, 3)?,
        })
    }

    fn checkpoint_status(
        &mut self,
        checkpoint_id: [u8; 16],
    ) -> Result<CheckpointStatus, SyncError> {
        let outer = self.send_cbor(
            "GET",
            format!(
                "{}/checkpoints/{}",
                self.vault_path(""),
                base64url::encode(&checkpoint_id)
            ),
            None,
        )?;
        let fields = cbor::value_map(cbor::field(&outer, 0)?)?;
        Ok(CheckpointStatus {
            checkpoint_id: cbor::bytes_fixed(&fields, 0)
                .map_err(|_| SyncError::RelayWire("checkpoint id 字段"))?,
            state: cbor::text_field(&fields, 1)
                .map_err(|_| SyncError::RelayWire("checkpoint state 字段"))?,
            role: cbor::opt_text_field(&fields, 2)
                .map_err(|_| SyncError::RelayWire("checkpoint role 字段"))?,
            covered_seq: cbor::uint_field(&fields, 3)
                .map_err(|_| SyncError::RelayWire("checkpoint covered 字段"))?,
            manifest_object_id: cbor::opt_bytes_field(&fields, 4)?
                .map(|bytes| {
                    bytes
                        .as_slice()
                        .try_into()
                        .map_err(|_| SyncError::RelayWire("manifest oid 长度非法"))
                })
                .transpose()?,
            refs_digest: cbor::opt_bytes_field(&fields, 5)?
                .map(|bytes| {
                    bytes
                        .as_slice()
                        .try_into()
                        .map_err(|_| SyncError::RelayWire("refs_digest 长度非法"))
                })
                .transpose()?,
            candidate_ready: cbor::bool_field(&fields, 9).unwrap_or(false),
            epoch: cbor::uint_field(&fields, 10).unwrap_or(0),
        })
    }
}

fn bootstrap_impl<T: HttpTransport, C: Clock>(
    client: &mut HttpRelayClient<T, C>,
    degraded: bool,
) -> Result<BootstrapBundle, SyncError> {
    let path = if degraded {
        format!("{}/bootstrap?degraded=1", client.vault_path(""))
    } else {
        format!("{}/bootstrap", client.vault_path(""))
    };
    let fields = client.send_cbor("POST", path, None)?;
    let primary = parse_bootstrap_checkpoint(cbor::field(&fields, 0)?)?;
    let fallback = parse_bootstrap_checkpoint(cbor::field(&fields, 1)?)?;
    let lease_id: [u8; 16] = cbor::bytes_fixed(&fields, 5)?;
    Ok(BootstrapBundle {
        primary,
        fallback,
        head_seq: cbor::uint_field(&fields, 2)?,
        gc_floor: cbor::uint_field(&fields, 3)?,
        epoch: cbor::uint_field(&fields, 4)?,
        degraded: cbor::bool_field(&fields, 7).unwrap_or(false),
        lease: BootstrapLease {
            lease_id,
            expires_at: cbor::uint_field(&fields, 6)?,
            floor_seq: 0,
        },
    })
}

fn parse_frozen_asset(value: &cbor::CdeValue) -> Result<FrozenAssetRef, SyncError> {
    let fields = cbor::value_map(value)?;
    Ok(FrozenAssetRef {
        object_id: cbor::bytes_fixed(&fields, 0)?,
        generation: cbor::uint_field(&fields, 2)?,
        key_epoch: cbor::uint_field(&fields, 3)?,
        part_count: u32::try_from(cbor::uint_field(&fields, 4)?)
            .map_err(|_| SyncError::RelayWire("frozen asset part_count 越界"))?,
        total_plain_len: cbor::uint_field(&fields, 5)?,
        object_digest: cbor::bytes_fixed(&fields, 6)?,
    })
}

fn parse_manager_chain_entry(value: &cbor::CdeValue) -> Result<ManagerChainEntry, SyncError> {
    let fields = cbor::value_map(value)?;
    Ok(ManagerChainEntry {
        rev: cbor::uint_field(&fields, 0)?,
        pubkey: cbor::bytes_fixed(&fields, 1)
            .map_err(|_| SyncError::RelayWire("management chain pubkey 长度非法"))?,
        transition: cbor::bytes_field(&fields, 2)?,
        old_rev: cbor::uint_field(&fields, 3)?,
        old_signature: cbor::opt_bytes_field(&fields, 4)?
            .map(|bytes| {
                bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| SyncError::RelayWire("management old sig 长度非法"))
            })
            .transpose()?,
        new_signature: cbor::opt_bytes_field(&fields, 5)?
            .map(|bytes| {
                bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| SyncError::RelayWire("management new sig 长度非法"))
            })
            .transpose()?,
    })
}

fn parse_rotation_view(fields: &cbor::Fields) -> Result<RotationView, SyncError> {
    let view = cbor::value_map(cbor::field(fields, 0)?)?;
    Ok(RotationView {
        rotation_id: cbor::bytes_fixed(&view, 0)?,
        state: cbor::text_field(&view, 1)?,
        from_epoch: cbor::uint_field(&view, 2)?,
        to_epoch: cbor::uint_field(&view, 3)?,
        freeze_seq: cbor::uint_field(&view, 4)?,
        revoked_count: cbor::uint_field(&view, 5)?,
        member_set_hash: cbor::opt_bytes_field(&view, 6)?
            .map(|bytes| {
                bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| SyncError::RelayWire("rotation member_set_hash 长度非法"))
            })
            .transpose()?,
        grants_count: cbor::uint_field(&view, 7)?,
        grants_digest: cbor::opt_bytes_field(&view, 8)?
            .map(|bytes| {
                bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| SyncError::RelayWire("rotation grants_digest 长度非法"))
            })
            .transpose()?,
        published_checkpoints: cbor::uint_field(&view, 9)?,
        control_rev: cbor::uint_field(&view, 10)?,
        auth_rev: cbor::uint_field(&view, 11)?,
        manager_rev: cbor::uint_field(&view, 12)?,
        from_epoch_revoked_once: cbor::uint_field(&view, 13)? != 0,
        freeze_asset_count: cbor::uint_field(&view, 15)?,
        freeze_asset_digest: opt_fixed32(&view, 16, "rotation asset_set_digest")?,
        member_cert_digest: opt_fixed32(&view, 17, "rotation member_cert_digest")?,
        primary_checkpoint: opt_fixed16(&view, 18, "rotation primary_checkpoint")?,
        fallback_checkpoint: opt_fixed16(&view, 19, "rotation fallback_checkpoint")?,
        activation_commit: cbor::opt_bytes_field(&view, 20)?,
        activated_at: cbor::opt_uint_field(&view, 21)?,
        activated_control_rev: cbor::opt_uint_field(&view, 22)?,
        candidate_count: cbor::array_field(&view, 23)?.len() as u64,
        candidate_ready: cbor::uint_field(&view, 26)? != 0,
        reservation_id: match view.iter().find(|(key, _)| *key == 25) {
            Some((_, cbor::CdeValue::Map(fields))) => Some(cbor::bytes_fixed(fields, 0)?),
            Some(_) => return Err(SyncError::RelayWire("rotation reservation 不是 map")),
            None => None,
        },
    })
}

fn opt_fixed32(
    fields: &cbor::Fields,
    key: u64,
    what: &'static str,
) -> Result<Option<[u8; 32]>, SyncError> {
    cbor::opt_bytes_field(fields, key)?
        .map(|bytes| {
            bytes
                .as_slice()
                .try_into()
                .map_err(|_| SyncError::RelayWire(what))
        })
        .transpose()
}

fn opt_fixed16(
    fields: &cbor::Fields,
    key: u64,
    what: &'static str,
) -> Result<Option<[u8; 16]>, SyncError> {
    cbor::opt_bytes_field(fields, key)?
        .map(|bytes| {
            bytes
                .as_slice()
                .try_into()
                .map_err(|_| SyncError::RelayWire(what))
        })
        .transpose()
}

fn parse_bootstrap_checkpoint(value: &cbor::CdeValue) -> Result<BootstrapCheckpoint, SyncError> {
    let fields = cbor::value_map(value)?;
    let manifest = cbor::opt_bytes_field(&fields, 3)?
        .ok_or(SyncError::RelayWire("bootstrap checkpoint 缺 manifest_oid"))?;
    let manifest_oid: ObjectId = manifest
        .as_slice()
        .try_into()
        .map_err(|_| SyncError::RelayWire("bootstrap manifest_oid 长度非法"))?;
    let refs_digest = match cbor::opt_bytes_field(&fields, 4)? {
        None => None,
        Some(bytes) => Some(
            bytes
                .as_slice()
                .try_into()
                .map_err(|_| SyncError::RelayWire("bootstrap refs_digest 长度非法"))?,
        ),
    };
    let signature = match cbor::opt_bytes_field(&fields, 7)? {
        None => None,
        Some(bytes) => Some(
            bytes
                .as_slice()
                .try_into()
                .map_err(|_| SyncError::RelayWire("bootstrap checkpoint 签名长度非法"))?,
        ),
    };
    Ok(BootstrapCheckpoint {
        checkpoint_id: cbor::bytes_fixed(&fields, 0)?,
        epoch: cbor::uint_field(&fields, 1)?,
        covered_seq: cbor::uint_field(&fields, 2)?,
        manifest_oid,
        refs_digest,
        cursor: cbor::text_field(&fields, 5)?,
        signed_commit: cbor::opt_bytes_field(&fields, 6)?,
        signature,
    })
}

fn slot_kind_text(kind: menghunote_sync_proto::SlotKind) -> &'static str {
    match kind {
        menghunote_sync_proto::SlotKind::Password => "password",
        menghunote_sync_proto::SlotKind::Recovery => "recovery",
    }
}

fn parse_slot_kind(text: &str) -> Result<menghunote_sync_proto::SlotKind, SyncError> {
    match text {
        "password" => Ok(menghunote_sync_proto::SlotKind::Password),
        "recovery" => Ok(menghunote_sync_proto::SlotKind::Recovery),
        _ => Err(SyncError::RelayWire("未知槽类型")),
    }
}

fn parse_slot_record(fields: &cbor::Fields) -> Result<SlotRecord, SyncError> {
    Ok(SlotRecord {
        slot_id: cbor::bytes_fixed(fields, 0)?,
        epoch: cbor::uint_field(fields, 1)?,
        kind: parse_slot_kind(&cbor::text_field(fields, 2)?)?,
        descriptor: cbor::bytes_field(fields, 3)?,
        state: cbor::text_field(fields, 4)?,
        slot_set_rev: cbor::uint_field(fields, 5)?,
        retire_at: cbor::opt_uint_field(fields, 6)?,
    })
}

fn parse_update_outcome(fields: &cbor::Fields) -> Result<UpdateOutcome, SyncError> {
    let object_id: [u8; 32] = cbor::bytes_fixed(fields, 0)?;
    let status_text = cbor::text_field(fields, 1)?;
    let relay_seq = cbor::opt_uint_field(fields, 2)?;
    let code = cbor::opt_text_field(fields, 3)?;
    let retryable = cbor::bool_field(fields, 4)?;
    let status = match status_text.as_str() {
        "accepted" => UpdateStatus::Accepted { relay_seq },
        "duplicate" => UpdateStatus::Duplicate,
        "failed" => UpdateStatus::Failed { code, retryable },
        _ => return Err(SyncError::RelayWire("未知 update 结果状态")),
    };
    Ok(UpdateOutcome { object_id, status })
}

fn cbor_headers() -> Vec<(String, String)> {
    vec![("content-type".to_owned(), CONTENT_TYPE_CBOR.to_owned())]
}

/// 2xx 响应解 CBOR；非 2xx 解析错误体。
fn decode_ok(response: &HttpResponse) -> Result<cbor::Fields, SyncError> {
    if !(200..300).contains(&response.status) {
        return Err(error_from_response(response));
    }
    cbor::decode(&response.body)
}

/// 把 relay 的错误体映射成 [`SyncError::RelayApi`]。
pub(crate) fn error_from_response(response: &HttpResponse) -> SyncError {
    if let Ok(fields) = cbor::decode(&response.body) {
        let code = cbor::text_field(&fields, 0);
        let retryable = cbor::bool_field(&fields, 1);
        if let (Ok(code), Ok(retryable)) = (code, retryable) {
            return SyncError::RelayApi {
                code,
                retryable,
                current_epoch: cbor::opt_uint_field(&fields, 2).ok().flatten(),
                current_control_rev: cbor::opt_uint_field(&fields, 3).ok().flatten(),
                bootstrap_hint: cbor::opt_text_field(&fields, 4).ok().flatten(),
            };
        }
    }
    if response.status >= 500 {
        return SyncError::RelayApi {
            code: format!("HTTP_{}", response.status),
            retryable: true,
            current_epoch: None,
            current_control_rev: None,
            bootstrap_hint: None,
        };
    }
    SyncError::HttpStatus(response.status)
}

/// 解析 `/control`：验证控制链哈希连续性 + 成员证书的一致性/可选 §17.1 签名。
///
/// 说明（relay `2accf2c` 的 wire）：`members[].certificate` 只返回 `CDE(DeviceCertV1)`，
/// **没有** `certificate_signature`。若 relay 后续在成员 map 加键 5（签名），这里会
/// 按 §17.1（`MN/v1/device-cert\0 || CDE(cert)`）用管理公钥验签；缺失时只能降级为
/// "信任已鉴权的控制面 + 链哈希"，这是当前 wire 的已知缺口。
fn parse_control(
    fields: &cbor::Fields,
    vault_id: &VaultId,
    manager_pub: Option<&[u8; 32]>,
) -> Result<ControlState, SyncError> {
    let commits =
        cbor::array_field(fields, 0).map_err(|_| SyncError::RelayWire("control commits"))?;
    let control_rev =
        cbor::uint_field(fields, 1).map_err(|_| SyncError::RelayWire("control rev"))?;
    let epoch = cbor::uint_field(fields, 2).map_err(|_| SyncError::RelayWire("control epoch"))?;

    let mut prev_hash = [0u8; 32];
    let mut last_rev = 0u64;
    let mut kinds = Vec::new();
    for commit in &commits {
        let commit =
            cbor::value_map(commit).map_err(|_| SyncError::RelayWire("control commit map"))?;
        let rev =
            cbor::uint_field(&commit, 0).map_err(|_| SyncError::RelayWire("control commit rev"))?;
        let stored_prev: [u8; 32] = cbor::bytes_fixed(&commit, 1)
            .map_err(|_| SyncError::RelayWire("control commit prev"))?;
        let stored_hash: [u8; 32] = cbor::bytes_fixed(&commit, 2)
            .map_err(|_| SyncError::RelayWire("control commit hash"))?;
        let signed_record = cbor::bytes_field(&commit, 3)
            .map_err(|_| SyncError::RelayWire("control commit signed"))?;
        if rev != last_rev.saturating_add(1) {
            return Err(SyncError::RelayWire("控制链 rev 不连续"));
        }
        if stored_prev != prev_hash {
            return Err(SyncError::RelayWire("控制链 prev_hash 不连续"));
        }
        let mut input = Vec::with_capacity(64 + signed_record.len());
        input.extend_from_slice(&prev_hash);
        input.extend_from_slice(&signed_record);
        let computed: [u8; 32] = Sha256::digest(&input).into();
        // relay 的 genesis 记录（rev=1）用 SHA256(signed_record)（relay vault::initialize），
        // 后续 append_control 用 SHA256(prev_hash || signed_record)。两者都是链哈希的合法
        // 形式（当前 relay wire 的不对称，已在回报中标注），不接受其它形式。
        let genesis_formula: [u8; 32] = Sha256::digest(&signed_record).into();
        let valid = computed == stored_hash
            || (rev == 1 && prev_hash == [0u8; 32] && genesis_formula == stored_hash);
        if !valid {
            return Err(SyncError::RelayWire("控制链 record_hash 不符"));
        }
        prev_hash = stored_hash;
        last_rev = rev;
        kinds.push((
            rev,
            cbor::opt_text_field(&commit, 4)?.unwrap_or_else(|| "raw".to_owned()),
        ));
    }
    if commits.is_empty() {
        if control_rev != 0 {
            return Err(SyncError::RelayWire("控制链为空但 control_rev 非 0"));
        }
    } else if control_rev != last_rev {
        return Err(SyncError::RelayWire("control_rev 与链尾不一致"));
    }

    let mut certs = HashMap::new();
    let mut member_cert_inputs = Vec::new();
    for member in
        cbor::array_field(fields, 3).map_err(|_| SyncError::RelayWire("control members"))?
    {
        let member =
            cbor::value_map(&member).map_err(|_| SyncError::RelayWire("control member map"))?;
        let device_id: [u8; 16] =
            cbor::bytes_fixed(&member, 0).map_err(|_| SyncError::RelayWire("control member id"))?;
        let certificate_bytes = cbor::bytes_field(&member, 1)
            .map_err(|_| SyncError::RelayWire("control member cert"))?;
        let status = cbor::text_field(&member, 2)
            .map_err(|_| SyncError::RelayWire("control member status"))?;
        let sign_pk: [u8; 32] = cbor::bytes_fixed(&member, 3)
            .map_err(|_| SyncError::RelayWire("control member sign_pk"))?;
        let hpke_pk: [u8; 32] = cbor::bytes_fixed(&member, 4)
            .map_err(|_| SyncError::RelayWire("control member hpke_pk"))?;
        let signature = cbor::opt_bytes_field(&member, 5)?;
        let cert = DeviceCertV1::decode(&certificate_bytes)
            .map_err(|_| SyncError::RelayWire("成员证书不是合法 DeviceCertV1"))?;
        if cert.vault_id != *vault_id
            || cert.device_id != device_id
            || cert.sign_ed25519_pub != sign_pk
            || cert.hpke_x25519_pub != hpke_pk
        {
            return Err(SyncError::RelayWire("成员证书与成员字段不一致"));
        }
        if let Some(signature) = &signature {
            let manager_pub = manager_pub.ok_or(SyncError::Unsupported(
                "未配置管理公钥，无法按 §17.1 校验证书",
            ))?;
            let signature_bytes: [u8; 64] = signature
                .as_slice()
                .try_into()
                .map_err(|_| SyncError::RelayWire("证书签名不是 64 字节"))?;
            let signed_input = device_cert_signed_bytes(&cert)?;
            verify_signature(manager_pub, &signed_input, &signature_bytes)?;
        }
        if status == "active" {
            member_cert_inputs.push(menghunote_sync_proto::MemberCertDigestInputV1 {
                device_id,
                certificate: certificate_bytes,
                certificate_signature: signature,
            });
            certs.insert(device_id, cert);
        }
    }
    Ok(ControlState {
        control_rev,
        epoch,
        certs,
        kinds,
        member_cert_inputs,
    })
}

fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestRng;
    use std::cell::Cell;
    use std::collections::VecDeque;

    #[derive(Default)]
    struct MockTransport {
        responses: VecDeque<HttpResponse>,
        requests: Vec<HttpRequest>,
    }

    impl MockTransport {
        fn with_responses(responses: Vec<HttpResponse>) -> Self {
            Self {
                responses: responses.into(),
                requests: Vec::new(),
            }
        }
    }

    impl HttpTransport for MockTransport {
        fn send(&mut self, request: &HttpRequest) -> Result<HttpResponse, SyncError> {
            self.requests.push(request.clone());
            self.responses
                .pop_front()
                .ok_or(SyncError::Transport("测试没有更多预设响应"))
        }
    }

    struct FakeClock(Cell<u64>);

    impl FakeClock {
        fn new(now: u64) -> Self {
            Self(Cell::new(now))
        }

        fn set(&self, now: u64) {
            self.0.set(now);
        }
    }

    impl Clock for FakeClock {
        fn now_secs(&self) -> u64 {
            self.0.get()
        }
    }

    fn cbor_response(fields: cbor::Fields) -> HttpResponse {
        HttpResponse {
            status: 200,
            body: cbor::encode(fields).expect("测试响应编码"),
        }
    }

    fn auth_responses(expires_at: u64) -> Vec<HttpResponse> {
        vec![
            cbor_response(vec![
                (0, cbor::bytes([0x01; 16])),
                (1, cbor::uint(1)),
                (2, cbor::bytes([0x04; 16])),
                (3, cbor::bytes([0x02; 32])),
                (4, cbor::uint(expires_at - 100)),
                (5, cbor::bytes([0x5a; 16])),
                (6, cbor::bytes([0x03; 16])),
                (7, cbor::uint(expires_at - 600)),
            ]),
            cbor_response(vec![
                (0, cbor::text("token-1")),
                (1, cbor::uint(expires_at)),
                (2, cbor::uint(expires_at - 600)),
                (3, cbor::uint(1)),
                (4, cbor::uint(1)),
            ]),
        ]
    }

    fn state_fields(control_rev: u64, head_seq: u64) -> cbor::Fields {
        vec![
            (0, cbor::bytes([0x5a; 16])),
            (1, cbor::bytes([0x11; 16])),
            (2, cbor::bytes([0x22; 16])),
            (3, cbor::text("active")),
            (4, cbor::uint(1)),
            (5, cbor::uint(control_rev)),
            (6, cbor::uint(1)),
            (7, cbor::uint(head_seq)),
            (8, cbor::uint(0)),
            (9, cbor::uint(0)),
            (10, cbor::uint(0)),
            (11, cbor::uint(1024)),
            (12, cbor::uint(2048)),
            (13, cbor::uint(2048)),
        ]
    }

    struct TestDevice {
        id: DeviceId,
        cert: DeviceCertV1,
    }

    fn test_device(seed: u8) -> TestDevice {
        let id = [seed; 16];
        let sign = DeviceSigningKey::generate(&mut TestRng::new(u64::from(seed))).expect("密钥");
        let hpke = crate::device::DeviceHpkeKey::generate(&mut TestRng::new(u64::from(seed)))
            .expect("hpke");
        let cert = DeviceCertV1 {
            vault_id: [0x5a; 16],
            device_id: id,
            sign_ed25519_pub: sign.public_key(),
            hpke_x25519_pub: hpke.public_key(),
            cert_serial: 1,
            issued_manager_rev: 1,
            rights: menghunote_sync_proto::Rights(menghunote_sync_proto::Rights::ALL),
        };
        TestDevice { id, cert }
    }

    fn member_fields(
        device: &TestDevice,
        signed: bool,
        manager: &DeviceSigningKey,
    ) -> cbor::CdeValue {
        let mut fields = vec![
            (0, cbor::bytes(device.id)),
            (1, cbor::bytes(device.cert.encode().expect("证书编码"))),
            (2, cbor::text("active")),
            (3, cbor::bytes(device.cert.sign_ed25519_pub)),
            (4, cbor::bytes(device.cert.hpke_x25519_pub)),
        ];
        if signed {
            let signature = manager.sign(&device_cert_signed_bytes(&device.cert).expect("待签"));
            fields.push((5, cbor::bytes(signature)));
        }
        cbor::CdeValue::Map(fields)
    }

    fn control_fields(
        rev: u64,
        members: &[&TestDevice],
        manager: Option<&DeviceSigningKey>,
        signed_certs: bool,
        corrupt_hash: bool,
    ) -> cbor::Fields {
        let mut commits = Vec::new();
        let mut prev = [0u8; 32];
        for index in 1..=rev {
            let record = vec![0xaa, index as u8];
            let mut input = Vec::new();
            input.extend_from_slice(&prev);
            input.extend_from_slice(&record);
            let mut hash: [u8; 32] = Sha256::digest(&input).into();
            if corrupt_hash && index == rev {
                hash[0] ^= 0x01;
            }
            commits.push(cbor::CdeValue::Map(vec![
                (0, cbor::uint(index)),
                (1, cbor::bytes(prev)),
                (2, cbor::bytes(hash)),
                (3, cbor::bytes(record)),
            ]));
            prev = hash;
        }
        let mut member_values = Vec::new();
        for device in members {
            let manager = manager.expect("测试需要管理密钥");
            member_values.push(member_fields(device, signed_certs, manager));
        }
        vec![
            (0, cbor::array(commits)),
            (1, cbor::uint(rev)),
            (2, cbor::uint(1)),
            (3, cbor::array(member_values)),
            (4, cbor::array(Vec::new())),
        ]
    }

    fn config(manager_pub: Option<[u8; 32]>) -> HttpRelayConfig {
        HttpRelayConfig {
            base_url: "http://127.0.0.1:8080".to_owned(),
            vault_id: [0x5a; 16],
            device_id: [0x04; 16],
            manager_pub,
        }
    }

    fn client_with(
        responses: Vec<HttpResponse>,
        clock: FakeClock,
        manager_pub: Option<[u8; 32]>,
    ) -> HttpRelayClient<MockTransport, FakeClock> {
        let signing = DeviceSigningKey::generate(&mut TestRng::new(9)).expect("签名密钥");
        HttpRelayClient::with_clock(
            MockTransport::with_responses(responses),
            config(manager_pub),
            signing,
            clock,
        )
    }

    #[test]
    fn percent_encodes_query() {
        assert_eq!(percent_encode("abc-_."), "abc-_.");
        assert_eq!(percent_encode("a b"), "a%20b");
        assert_eq!(percent_encode("a+b/c="), "a%2Bb%2Fc%3D");
    }

    #[test]
    fn parses_http_response_with_content_length() {
        let raw = b"HTTP/1.1 201 Created\r\nContent-Length: 3\r\n\r\nabc";
        let response = parse_http_response(raw).unwrap();
        assert_eq!(response.status, 201);
        assert_eq!(response.body, b"abc");
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nabc";
        assert!(parse_http_response(raw).is_err());
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n";
        assert!(parse_http_response(raw).is_err());
    }

    #[test]
    fn std_transport_rejects_https() {
        assert!(StdTcpTransport::new("https://relay.example").is_err());
        let transport = StdTcpTransport::new("http://127.0.0.1:8080").unwrap();
        assert_eq!(transport.port, 8080);
        assert_eq!(transport.host, "127.0.0.1");
    }

    #[test]
    fn rotation_view_reads_section_30_3_fields() {
        let view = cbor::CdeValue::Map(vec![
            (0, cbor::bytes([0x41; 16].to_vec())),
            (1, cbor::text("frozen")),
            (2, cbor::uint(1)),
            (3, cbor::uint(2)),
            (4, cbor::uint(17)),
            (5, cbor::uint(1)),
            (7, cbor::uint(2)),
            (8, cbor::bytes([0x42; 32].to_vec())),
            (9, cbor::uint(0)),
            (10, cbor::uint(3)),
            (11, cbor::uint(4)),
            (12, cbor::uint(5)),
            (13, cbor::uint(0)),
            (15, cbor::uint(7)),
            (16, cbor::bytes([0x43; 32].to_vec())),
            (17, cbor::bytes([0x44; 32].to_vec())),
            (18, cbor::bytes([0x45; 16].to_vec())),
            (19, cbor::bytes([0x46; 16].to_vec())),
            (20, cbor::bytes(vec![0x47])),
            (21, cbor::uint(9)),
            (22, cbor::uint(10)),
            (23, cbor::array(vec![cbor::CdeValue::Map(vec![]); 2])),
            (26, cbor::uint(1)),
        ]);
        let fields = match cbor::CdeValue::Map(vec![(0, view)]) {
            cbor::CdeValue::Map(fields) => fields,
            _ => unreachable!(),
        };
        let parsed = parse_rotation_view(&fields).expect("rotation view");
        assert_eq!(parsed.control_rev, 3);
        assert_eq!(parsed.auth_rev, 4);
        assert_eq!(parsed.manager_rev, 5);
        assert_eq!(parsed.freeze_asset_count, 7);
        assert_eq!(parsed.freeze_asset_digest, Some([0x43; 32]));
        assert_eq!(parsed.member_cert_digest, Some([0x44; 32]));
        assert_eq!(parsed.primary_checkpoint, Some([0x45; 16]));
        assert_eq!(parsed.fallback_checkpoint, Some([0x46; 16]));
        assert_eq!(parsed.activated_at, Some(9));
        assert_eq!(parsed.activated_control_rev, Some(10));
        assert_eq!(parsed.candidate_count, 2);
        assert!(parsed.candidate_ready);
    }

    #[test]
    fn state_authenticates_reuses_token_and_refreshes_before_expiry() {
        let clock = FakeClock::new(900);
        let mut responses = auth_responses(1200);
        responses.push(cbor_response(state_fields(1, 9)));
        responses.push(cbor_response(state_fields(1, 9)));
        responses.extend(auth_responses(1500));
        responses.push(cbor_response(state_fields(1, 9)));
        let mut client = client_with(responses, clock, Some([0x33; 32]));

        let state = client.state().unwrap();
        assert_eq!(state.head_seq, 9);
        assert_eq!(state.control_rev, 1);
        assert_eq!(client.transport.requests.len(), 3);
        assert_eq!(client.transport.requests[0].path, "/v1/auth/challenges");
        assert_eq!(client.transport.requests[1].path, "/v1/auth/tokens");
        let challenge_body = cbor::decode(&client.transport.requests[0].body).unwrap();
        assert_eq!(challenge_body[0], (0, cbor::uint(1)));
        assert_eq!(challenge_body[1], (1, cbor::bytes([0x04; 16])));
        assert!(client.transport.requests[2].path.ends_with("/state"));
        assert!(client.transport.requests[2]
            .headers
            .iter()
            .any(|(name, value)| name == "authorization" && value == "Bearer token-1"));
        assert!(client.transport.requests[2]
            .headers
            .iter()
            .any(|(name, _)| name == "content-type"));

        // 未到期：不重新认证。
        client.state().unwrap();
        assert_eq!(client.transport.requests.len(), 4);

        // 剩余 <60s：刷新（challenge/token 各一次）。
        client.clock.set(1150);
        client.state().unwrap();
        assert_eq!(client.transport.requests.len(), 7);
        assert_eq!(client.transport.requests[4].path, "/v1/auth/challenges");
        assert_eq!(client.transport.requests[5].path, "/v1/auth/tokens");
    }

    #[test]
    fn log_reads_signature_and_control_cache_serves_certs() {
        let manager = DeviceSigningKey::generate(&mut TestRng::new(77)).expect("管理密钥");
        let device_a = test_device(0x0a);
        let device_b = test_device(0x0b);
        let object_id = [0x77; 32];
        let commit = menghunote_sync_proto::ObjectCommit {
            vault_id: [0x5a; 16],
            object_id,
            message_id: [0x01; 16],
            object_type: menghunote_sync_proto::ObjectType::Update,
            generation: 1,
            key_epoch: 1,
            sender_seq: 1,
            part_count: 1,
            total_len: 10,
            object_digest: [0x02; 32],
        };
        let log_fields = vec![
            (
                0,
                cbor::array(vec![cbor::CdeValue::Map(vec![
                    (0, cbor::uint(1)),
                    (1, cbor::text("update")),
                    (2, cbor::bytes(object_id)),
                    (3, cbor::uint(1)),
                    (4, cbor::bytes(commit.encode().unwrap())),
                    (5, cbor::bytes([0x44; 64])),
                ])]),
            ),
            (1, cbor::text("cursor-1")),
            (2, cbor::uint(1)),
            (3, cbor::uint(1)),
            (4, cbor::uint(1)),
            (5, cbor::uint(0)),
        ];

        let mut responses = auth_responses(1200);
        responses.push(cbor_response(state_fields(1, 1)));
        responses.push(cbor_response(log_fields));
        responses.push(cbor_response(control_fields(
            1,
            &[&device_a],
            Some(&manager),
            true,
            false,
        )));
        let mut client = client_with(responses, FakeClock::new(900), Some(manager.public_key()));

        client.state().unwrap();
        let page = client.log(Some("cursor-0"), 10).unwrap();
        assert_eq!(page.events.len(), 1);
        assert_eq!(page.events[0].signature, Some([0x44; 64]));
        assert_eq!(page.events[0].event, commit.encode().unwrap());
        assert_eq!(page.next_cursor, "cursor-1");

        let cert = client.device_cert(&device_a.id).unwrap().expect("证书");
        assert_eq!(cert.sign_ed25519_pub, device_a.cert.sign_ed25519_pub);
        assert_eq!(client.cached_control_rev(), Some(1));
        assert!(client.device_cert(&device_b.id).unwrap().is_none());

        // 控制链哈希、§17.1 证书签名都已校验通过（否则上面的 device_cert 会失败）。
    }

    #[test]
    fn control_cache_invalidates_when_control_rev_moves() {
        let manager = DeviceSigningKey::generate(&mut TestRng::new(78)).expect("管理密钥");
        let device_a = test_device(0x1a);
        let device_b = test_device(0x1b);

        let mut responses = auth_responses(1200);
        responses.push(cbor_response(state_fields(1, 0)));
        responses.push(cbor_response(control_fields(
            1,
            &[&device_a],
            Some(&manager),
            true,
            false,
        )));
        responses.push(cbor_response(state_fields(2, 0)));
        responses.push(cbor_response(control_fields(
            2,
            &[&device_a, &device_b],
            Some(&manager),
            true,
            false,
        )));
        let mut client = client_with(responses, FakeClock::new(900), Some(manager.public_key()));

        client.state().unwrap();
        assert!(client.device_cert(&device_a.id).unwrap().is_some());
        assert_eq!(client.transport.requests.len(), 4); // auth×2 + state + control
        client.state().unwrap();
        assert_eq!(client.transport.requests.len(), 5);
        assert_eq!(
            client.cached_control_rev(),
            None,
            "control_rev 前进必须让缓存失效"
        );
        assert!(client.device_cert(&device_b.id).unwrap().is_some());
        assert_eq!(client.cached_control_rev(), Some(2));
    }

    #[test]
    fn control_chain_hash_and_certificate_signature_are_verified() {
        let manager = DeviceSigningKey::generate(&mut TestRng::new(79)).expect("管理密钥");
        let device = test_device(0x2a);

        // record_hash 被篡改 → RelayWire。
        let mut responses = auth_responses(1200);
        responses.push(cbor_response(control_fields(
            1,
            &[&device],
            Some(&manager),
            true,
            true,
        )));
        let mut client = client_with(responses, FakeClock::new(900), Some(manager.public_key()));
        assert!(matches!(
            client.device_cert(&device.id),
            Err(SyncError::RelayWire("控制链 record_hash 不符"))
        ));

        // 证书签名被篡改 → Signature。
        let mut responses = auth_responses(1200);
        let mut fields = control_fields(1, &[&device], Some(&manager), true, false);
        let members = cbor::array_field(&fields, 3).unwrap();
        let mut member = cbor::value_map(&members[0]).unwrap();
        let mut signature = cbor::bytes_field(&member, 5).unwrap();
        signature[0] ^= 0x01;
        member[5].1 = cbor::bytes(signature);
        fields[3].1 = cbor::array(vec![cbor::CdeValue::Map(member)]);
        responses.push(cbor_response(fields));
        let mut client = client_with(responses, FakeClock::new(900), Some(manager.public_key()));
        assert!(matches!(
            client.device_cert(&device.id),
            Err(SyncError::Signature)
        ));

        // 没有证书签名（当前 relay wire）：仍能缓存证书（文档化降级）。
        let mut responses = auth_responses(1200);
        responses.push(cbor_response(control_fields(
            1,
            &[&device],
            Some(&manager),
            false,
            false,
        )));
        let mut client = client_with(responses, FakeClock::new(900), Some(manager.public_key()));
        assert!(client.device_cert(&device.id).unwrap().is_some());
    }

    #[test]
    fn error_body_maps_code_epoch_and_retryable() {
        let error = HttpResponse {
            status: 409,
            body: cbor::encode(vec![
                (0, cbor::text("EPOCH")),
                (1, cbor::boolean(false)),
                (2, cbor::uint(3)),
                (3, cbor::uint(7)),
                (4, cbor::text("bootstrap")),
            ])
            .unwrap(),
        };
        let err = error_from_response(&error);
        match err {
            SyncError::RelayApi {
                code,
                retryable,
                current_epoch,
                current_control_rev,
                bootstrap_hint,
            } => {
                assert_eq!(code, "EPOCH");
                assert!(!retryable);
                assert_eq!(current_epoch, Some(3));
                assert_eq!(current_control_rev, Some(7));
                assert_eq!(bootstrap_hint.as_deref(), Some("bootstrap"));
            }
            other => panic!("期望 RelayApi，实际 {other:?}"),
        }

        // 5xx 无错误体 → 可重试内部错误。
        let err = error_from_response(&HttpResponse {
            status: 503,
            body: Vec::new(),
        });
        assert!(matches!(
            err,
            SyncError::RelayApi {
                retryable: true,
                ..
            }
        ));
        // 4xx 无错误体 → HTTP 状态。
        assert!(matches!(
            error_from_response(&HttpResponse {
                status: 404,
                body: b"not cbor".to_vec()
            }),
            SyncError::HttpStatus(404)
        ));
    }

    #[test]
    fn updates_put_part_and_ack_use_cbor_layout() {
        let object_id = [0x77; 32];
        let commit = menghunote_sync_proto::ObjectCommit {
            vault_id: [0x5a; 16],
            object_id,
            message_id: [0x01; 16],
            object_type: menghunote_sync_proto::ObjectType::Update,
            generation: 1,
            key_epoch: 1,
            sender_seq: 1,
            part_count: 1,
            total_len: 10,
            object_digest: [0x02; 32],
        };
        let mut responses = auth_responses(1200);
        responses.push(HttpResponse {
            status: 201,
            body: cbor::encode(vec![
                (0, cbor::bytes(object_id)),
                (1, cbor::uint(0)),
                (2, cbor::bytes([0x09; 32])),
                (3, cbor::boolean(false)),
            ])
            .unwrap(),
        });
        responses.push(cbor_response(vec![(
            0,
            cbor::array(vec![
                cbor::CdeValue::Map(vec![
                    (0, cbor::bytes(object_id)),
                    (1, cbor::text("accepted")),
                    (2, cbor::uint(5)),
                    (4, cbor::boolean(false)),
                ]),
                cbor::CdeValue::Map(vec![
                    (0, cbor::bytes([0x78; 32])),
                    (1, cbor::text("failed")),
                    (3, cbor::text("EPOCH")),
                    (4, cbor::boolean(false)),
                ]),
            ]),
        )]));
        responses.push(HttpResponse {
            status: 204,
            body: Vec::new(),
        });
        let signed_commit = commit.encode().unwrap();
        let mut client = client_with(responses, FakeClock::new(900), None);

        client.put_part(&object_id, 0, &[0xaa; 4]).unwrap();
        let outcomes = client
            .submit_updates(vec![UpdateSubmission {
                object_id,
                signed_commit: signed_commit.clone(),
                commit,
                signature: [0u8; 64],
            }])
            .unwrap();
        assert_eq!(
            outcomes[0].status,
            UpdateStatus::Accepted { relay_seq: Some(5) }
        );
        assert!(matches!(
            outcomes[1].status,
            UpdateStatus::Failed {
                code: Some(_),
                retryable: false
            }
        ));
        client.ack(1, 1, 1).unwrap();

        // 请求布局与 relay 一致：CBOR map；update item = {0,1,2}。
        assert_eq!(
            client.transport.requests[2]
                .headers
                .iter()
                .find(|(name, _)| name == "content-type")
                .map(|(_, value)| value.as_str()),
            Some(CONTENT_TYPE_OCTET)
        );
        let update_body = cbor::decode(&client.transport.requests[3].body).unwrap();
        let items = cbor::array_field(&update_body, 0).unwrap();
        let item = cbor::value_map(&items[0]).unwrap();
        assert_eq!(item.len(), 3);
        assert_eq!(cbor::bytes_field(&item, 0).unwrap(), object_id.to_vec());
        assert_eq!(cbor::bytes_field(&item, 1).unwrap(), signed_commit);
        assert_eq!(cbor::bytes_field(&item, 2).unwrap(), vec![0u8; 64]);
        let ack_body = cbor::decode(&client.transport.requests[4].body).unwrap();
        assert_eq!(ack_body[0], (0, cbor::uint(1)));
        assert_eq!(ack_body[1], (1, cbor::uint(1)));
        assert_eq!(ack_body[2], (2, cbor::uint(1)));
    }
}
