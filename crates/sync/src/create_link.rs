//! ADR 0014 **S3a**：一次性创建票据 / `mn-pair://create` 链接。
//!
//! ## 为什么需要它
//!
//! 多 vault 的 `POST /v2/vaults` 授权来自运维发行的一次性 **32B 创建票据**
//! （relay 只存 `SHA256`，ADR 0014 §A / S2b）。客户端需要一条**可粘贴**的创建链接，
//! 让新实例在不接触 setup secret 的情况下建库：
//!
//! ```text
//! mn-pair://create?v=1&relay=<percent-encoded canonical origin>&vault=<hex32>[&pin=<hex32>]#ticket=<base64url>
//! ```
//!
//! ## 安全边界
//!
//! - **ticket 只在 fragment**：不进 query、不进 referrer/服务端日志；本模块的 `Debug`
//!   与全部错误文案都不回显 ticket/完整 URL；
//! - **唯一 URL parser**：`relay` 百分号解码后交给
//!   [`crate::relay_policy::parse_relay_url`]——`https` 只接受远端 canonical origin，
//!   `http` 只接受显式 loopback 白名单；userinfo/query/fragment/非根 path 一律拒绝；
//! - **不自动换 pin**：可选的 `pin` 是邀请方已保存的 relay instance 公开锚，调用方
//!   必须在 bootstrap/`/state` 回执里逐字节比对；本模块只解析、不信任、不改写；
//! - **ticket 不落盘**：本模块不提供任何持久化 API；调用方只在内存里把它交给
//!   `POST /v2/vaults`（[`crate::remote_init::create_vault_with_ticket`]）。
//!
//! ## 非目标（S3a 明确）
//!
//! 不做 Windows scheme 注册/QR/剪贴板；不做 App 表单（S3b）；不自动创建票据
//! （票据由运维 `dev_server --issue-ticket` 发行）。

use std::fmt;

use zeroize::Zeroize;

use crate::api_version::RelayApiVersion;
use crate::error::SyncError;
use crate::http_transport::TransportTimeouts;
use crate::relay_context::RelayContext;
use crate::relay_policy::{parse_relay_url, RelayMode};
use crate::VaultId;

/// `mn-pair://` 的创建 host。
pub const CREATE_LINK_HOST: &str = "create";
/// 创建链接格式版本（`v=1`；与 relay API 面无关，面由票据端点固定为 v2）。
pub const CREATE_LINK_VERSION: &str = "1";
/// 票据明文长度（relay 发行/消费契约，ADR 0014 S2b）。
pub const CREATE_TICKET_LEN: usize = 32;

// ---------------------------------------------------------------------------
// 票据
// ---------------------------------------------------------------------------

/// 一次性创建票据（32B 明文；**只活内存**，不落库/日志/配置）。
///
/// - `Debug` 脱敏；`Drop` 擦除（`zeroize`），避免进程内长时间残留；
/// - 不可 `Clone`：多一份副本就多一次泄漏面（需要重试时由调用方保留同一实例）；
/// - `PartialEq` 走常量时间比较（只用于"同一票据"的断言，不做授权判断）。
pub struct CreateTicket {
    bytes: [u8; CREATE_TICKET_LEN],
}

impl CreateTicket {
    /// 从 relay 发行的 32B 明文构造。
    #[must_use]
    pub const fn from_bytes(bytes: [u8; CREATE_TICKET_LEN]) -> Self {
        Self { bytes }
    }

    /// 从 slice 构造（长度必须恰好 32，否则拒绝；错误只含长度）。
    pub fn from_slice(bytes: &[u8]) -> Result<Self, CreateLinkError> {
        let bytes: [u8; CREATE_TICKET_LEN] = bytes
            .try_into()
            .map_err(|_| CreateLinkError::TicketLength)?;
        Ok(Self { bytes })
    }

    /// 请求字段只在该 crate 内读取（`POST /v2/vaults` 的键 0）。
    pub(crate) const fn as_bytes(&self) -> &[u8; CREATE_TICKET_LEN] {
        &self.bytes
    }

    /// 常量时间的相等比较（测试/诊断；不代表票据已被授权）。
    #[must_use]
    pub fn matches(&self, other: &Self) -> bool {
        use subtle::ConstantTimeEq as _;
        bool::from(self.bytes.ct_eq(&other.bytes))
    }
}

impl PartialEq for CreateTicket {
    fn eq(&self, other: &Self) -> bool {
        self.matches(other)
    }
}

impl Eq for CreateTicket {}

impl fmt::Debug for CreateTicket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CreateTicket")
            .field("bytes", &"<redacted: one-time create ticket>")
            .finish()
    }
}

impl Drop for CreateTicket {
    fn drop(&mut self) {
        self.bytes.zeroize();
    }
}

// ---------------------------------------------------------------------------
// 链接
// ---------------------------------------------------------------------------

/// 解析后的创建链接（ticket 仍在内存里；`Debug` 脱敏）。
///
/// `url` 是**由 canonical 组件重新生成**的完整链接（含 fragment ticket）：解析时不保留
/// 任意原始文本，避免把夹带内容原样带给 UI/日志。
pub struct CreateLink {
    /// 链接来源的 relay 模式（由 scheme 决定：`https`=remote_tls，`http`=local_dev）。
    pub mode: RelayMode,
    /// canonical relay origin（公开）。
    pub relay: String,
    /// 预留的 vault id（票据绑定；创建必须使用同一个 id）。
    pub vault_id: VaultId,
    /// 一次性票据（秘密；只在内存）。
    pub ticket: CreateTicket,
    /// 可选公开锚：创建端已保存的 relay instance（16B），用于要求同实例。
    pub pin: Option<[u8; 16]>,
    /// canonical 完整 URL（含 fragment；**不得**打印/入库）。
    pub url: String,
}

impl CreateLink {
    /// 严格解析创建链接（唯一入口）。
    ///
    /// 拒绝：非 `mn-pair://create`、userinfo/额外 path/端口、缺/重复/未知 query 字段、
    /// `v=1` 之外版本、非 32 位 hex 的 vault/pin、fragment 里非 `ticket=` 的内容、
    /// 非 32B base64url ticket、非法百分号编码、以及唯一权威 relay parser 拒绝的 origin。
    pub fn parse(raw: &str) -> Result<Self, CreateLinkError> {
        let text = raw.trim();
        if text.is_empty() {
            return Err(CreateLinkError::Empty);
        }
        let rest = text
            .strip_prefix("mn-pair://")
            .or_else(|| text.strip_prefix("MN-PAIR://"))
            .ok_or(CreateLinkError::Scheme)?;
        // fragment 只允许一个 `#`：ticket 只从 fragment 取。
        let (head, fragment) = rest.split_once('#').ok_or(CreateLinkError::Fragment)?;
        let (authority, query) = head.split_once('?').ok_or(CreateLinkError::Query)?;
        // host 必须精确是 create：这同时拒绝 userinfo（`@`）、端口和额外 path。
        if !authority.eq_ignore_ascii_case(CREATE_LINK_HOST) {
            return Err(CreateLinkError::Host);
        }
        let ticket = parse_fragment(fragment)?;
        let fields = parse_query(query)?;
        let version = fields.version.ok_or(CreateLinkError::Missing("v"))?;
        if version != CREATE_LINK_VERSION {
            return Err(CreateLinkError::Version);
        }
        let relay_text = fields.relay.ok_or(CreateLinkError::Missing("relay"))?;
        let relay_plain = percent_decode(relay_text)?;
        let mode = if relay_plain.starts_with("https://") {
            RelayMode::RemoteTls
        } else if relay_plain.starts_with("http://") {
            RelayMode::LocalDev
        } else {
            return Err(CreateLinkError::RelayScheme);
        };
        let origin = parse_relay_url(&relay_plain, mode)
            .map_err(|error| CreateLinkError::Relay(error.to_string()))?;
        let vault_id = parse_hex32(fields.vault.ok_or(CreateLinkError::Missing("vault"))?)?;
        let pin = match fields.pin {
            Some(text) => Some(parse_hex32(text)?),
            None => None,
        };
        let relay = origin.as_str();
        // 用 canonical 组件重建（不保留任意原始文本）。
        let url = build_url(&relay, &vault_id, &ticket, pin.as_ref());
        Ok(Self {
            mode,
            relay,
            vault_id,
            ticket,
            pin,
            url,
        })
    }

    /// 由 canonical 组件生成创建链接（唯一编码入口；ticket 只进 fragment）。
    pub fn encode(
        mode: RelayMode,
        relay: &str,
        vault_id: VaultId,
        ticket: CreateTicket,
        pin: Option<[u8; 16]>,
    ) -> Result<Self, CreateLinkError> {
        let origin = parse_relay_url(relay, mode)
            .map_err(|error| CreateLinkError::Relay(error.to_string()))?;
        let relay = origin.as_str();
        let url = build_url(&relay, &vault_id, &ticket, pin.as_ref());
        Ok(Self {
            mode,
            relay,
            vault_id,
            ticket,
            pin,
            url,
        })
    }

    /// 本链接对应的 relay context：**固定 v2 面**（票据端点就是 `/v2/vaults`）。
    ///
    /// 解析/编码时已保证 origin 被唯一 parser 接受；这里仍按模式重新构造，
    /// 因此不会把 `http` 当成远端或把 `https` 当成本地开发。
    pub fn relay_context(&self, timeouts: TransportTimeouts) -> Result<RelayContext, SyncError> {
        Ok(RelayContext::from_mode(self.mode, &self.relay, timeouts)?
            .with_api_version(RelayApiVersion::V2))
    }

    /// 是否要求同 relay instance（公开锚存在）。
    #[must_use]
    pub const fn requires_instance(&self) -> bool {
        self.pin.is_some()
    }

    /// vault id 的 32 位 hex（公开；UI 预填/展示用）。
    #[must_use]
    pub fn vault_id_hex(&self) -> String {
        crate::ids::hex_of(&self.vault_id)
    }
}

impl fmt::Debug for CreateLink {
    /// ticket 与 URL 都不打印；只给公开定位信息。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CreateLink")
            .field("mode", &self.mode)
            .field("relay", &self.relay)
            .field("vault_id", &crate::ids::hex_of(&self.vault_id))
            .field("pin", &self.pin.as_ref().map(|pin| crate::ids::hex_of(pin)))
            .field("ticket", &"<redacted: one-time create ticket>")
            .field("url", &"<redacted: contains ticket>")
            .finish()
    }
}

/// 创建链接解析/编码失败原因（**不含 ticket/完整 URL**）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreateLinkError {
    /// 空输入。
    Empty,
    /// 不是 `mn-pair://`。
    Scheme,
    /// host 不是 `create`（或带了 userinfo/端口/额外 path）。
    Host,
    /// 缺 fragment / 多余 `#`。
    Fragment,
    /// 缺 query。
    Query,
    /// 缺必需字段。
    Missing(&'static str),
    /// 重复字段。
    Duplicate(&'static str),
    /// 未知字段（拒绝 `?next=` 之类的恶意跳转）。
    Unknown(&'static str),
    /// 版本不是 1。
    Version,
    /// fragment ticket 非法（长度/编码）。
    Ticket,
    /// 票据长度不是 32B。
    TicketLength,
    /// 百分号编码非法。
    Percent,
    /// `vault`/`pin` 不是 32 位 hex。
    Hex(&'static str),
    /// relay 不是受支持的 scheme（http/https）。
    RelayScheme,
    /// relay 被唯一权威 URL parser 拒绝（文案只含公开定位信息）。
    Relay(String),
}

impl fmt::Display for CreateLinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "创建链接为空"),
            Self::Scheme => write!(f, "不是 mn-pair:// 创建链接"),
            Self::Host => write!(
                f,
                "创建链接的 host 必须是 create（不接受 userinfo/端口/path）"
            ),
            Self::Fragment => write!(f, "创建链接缺 fragment（#ticket=…）或含多余 #"),
            Self::Query => write!(f, "创建链接缺 query（?v=1&relay=…&vault=…）"),
            Self::Missing(field) => write!(f, "创建链接缺字段 {field}=…"),
            Self::Duplicate(field) => write!(f, "创建链接字段重复：{field}"),
            Self::Unknown(field) => write!(f, "创建链接含未知字段 {field}（拒绝跳转/夹带）"),
            Self::Version => write!(f, "创建链接版本不是 1"),
            Self::Ticket => write!(f, "创建链接票据格式非法（应为 32 字节 base64url）"),
            Self::TicketLength => write!(f, "创建票据长度必须是 32 字节"),
            Self::Percent => write!(f, "创建链接含非法百分号编码"),
            Self::Hex(field) => write!(f, "创建链接字段 {field} 不是 32 位十六进制"),
            Self::RelayScheme => write!(f, "创建链接的 relay 必须是 http:// 或 https://"),
            Self::Relay(message) => write!(f, "创建链接的 relay 不合法：{message}"),
        }
    }
}

impl std::error::Error for CreateLinkError {}

impl From<CreateLinkError> for SyncError {
    fn from(error: CreateLinkError) -> Self {
        Self::InviteLink(error.to_string())
    }
}

// ---------------------------------------------------------------------------
// 内部编解码（严格，唯一实现）
// ---------------------------------------------------------------------------

fn build_url(
    relay: &str,
    vault_id: &VaultId,
    ticket: &CreateTicket,
    pin: Option<&[u8; 16]>,
) -> String {
    let relay = percent_encode(relay);
    let vault = crate::ids::hex_of(vault_id);
    let mut url =
        format!("mn-pair://{CREATE_LINK_HOST}?v={CREATE_LINK_VERSION}&relay={relay}&vault={vault}");
    if let Some(pin) = pin {
        url.push_str("&pin=");
        url.push_str(&crate::ids::hex_of(pin));
    }
    url.push_str("#ticket=");
    url.push_str(&crate::base64url::encode(ticket.as_bytes()));
    url
}

fn parse_fragment(fragment: &str) -> Result<CreateTicket, CreateLinkError> {
    if fragment.contains('#') {
        return Err(CreateLinkError::Fragment);
    }
    let (key, value) = fragment.split_once('=').ok_or(CreateLinkError::Ticket)?;
    if key != "ticket" {
        return Err(CreateLinkError::Ticket);
    }
    let decoded = percent_decode(value)?;
    let bytes = crate::base64url::decode(&decoded).map_err(|_| CreateLinkError::Ticket)?;
    CreateTicket::from_slice(&bytes)
}

fn percent_decode(value: &str) -> Result<String, CreateLinkError> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' => {
                let high = *bytes.get(index + 1).ok_or(CreateLinkError::Percent)?;
                let low = *bytes.get(index + 2).ok_or(CreateLinkError::Percent)?;
                let high = hex_value(high).ok_or(CreateLinkError::Percent)?;
                let low = hex_value(low).ok_or(CreateLinkError::Percent)?;
                out.push(high << 4 | low);
                index += 3;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(out).map_err(|_| CreateLinkError::Percent)
}

/// 只保留 RFC 3986 unreserved 字符；`:` `/` 等一律百分号编码（query 值里必须转义）。
fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push(hex_digit(byte >> 4));
            out.push(hex_digit(byte & 0x0f));
        }
    }
    out
}

fn hex_digit(value: u8) -> char {
    match value {
        0..=9 => (b'0' + value) as char,
        _ => (b'A' + (value - 10)) as char,
    }
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn parse_hex32(text: &str) -> Result<[u8; 16], CreateLinkError> {
    if text.len() != 32 {
        return Err(CreateLinkError::Hex("vault/pin"));
    }
    let mut out = [0u8; 16];
    for (index, chunk) in text.as_bytes().chunks(2).enumerate() {
        let high = hex_value(chunk[0]).ok_or(CreateLinkError::Hex("vault/pin"))?;
        let low = hex_value(chunk[1]).ok_or(CreateLinkError::Hex("vault/pin"))?;
        out[index] = high << 4 | low;
    }
    Ok(out)
}

struct QueryFields<'a> {
    version: Option<&'a str>,
    relay: Option<&'a str>,
    vault: Option<&'a str>,
    pin: Option<&'a str>,
}

fn parse_query(query: &str) -> Result<QueryFields<'_>, CreateLinkError> {
    let mut fields = QueryFields {
        version: None,
        relay: None,
        vault: None,
        pin: None,
    };
    for pair in query.split('&') {
        if pair.is_empty() {
            return Err(CreateLinkError::Query);
        }
        let (key, value) = pair.split_once('=').ok_or(CreateLinkError::Query)?;
        if value.is_empty() {
            return Err(CreateLinkError::Missing("query 字段值"));
        }
        let slot = match key {
            "v" => &mut fields.version,
            "relay" => &mut fields.relay,
            "vault" => &mut fields.vault,
            "pin" => &mut fields.pin,
            // 未知字段一律拒绝（拒绝恶意跳转/夹带）；不把未知字段名回显给用户。
            _other => return Err(CreateLinkError::Unknown("未知字段")),
        };
        if slot.is_some() {
            return Err(CreateLinkError::Duplicate("query 字段"));
        }
        *slot = Some(value);
    }
    Ok(fields)
}

#[cfg(test)]
mod tests {
    use super::*;

    const VAULT: [u8; 16] = [0x5a; 16];
    const PIN: [u8; 16] = [0x22; 16];
    const TICKET: [u8; 32] = [0x11; 32];

    fn local_link(pin: Option<[u8; 16]>) -> CreateLink {
        CreateLink::encode(
            RelayMode::LocalDev,
            "http://127.0.0.1:18080",
            VAULT,
            CreateTicket::from_bytes(TICKET),
            pin,
        )
        .expect("local link")
    }

    #[test]
    fn create_link_roundtrips_and_ticket_stays_in_fragment() {
        let link = local_link(Some(PIN));
        assert!(link
            .url
            .starts_with("mn-pair://create?v=1&relay=http%3A%2F%2F127.0.0.1%3A18080&vault="));
        assert!(link.url.contains("#ticket="));
        assert!(!link.url[..link.url.find('#').expect("fragment")].contains("ticket="));
        let parsed = CreateLink::parse(&link.url).expect("parse");
        assert_eq!(parsed.vault_id, VAULT);
        assert_eq!(parsed.pin, Some(PIN));
        assert_eq!(parsed.relay, "http://127.0.0.1:18080");
        assert_eq!(parsed.mode, RelayMode::LocalDev);
        assert!(parsed.ticket.matches(&CreateTicket::from_bytes(TICKET)));
        // canonical 重建：再次解析/编码得到逐字节相同的 URL。
        assert_eq!(parsed.url, link.url);
        let again = CreateLink::parse(&parsed.url).expect("reparse");
        assert_eq!(again.url, link.url);
    }

    #[test]
    fn remote_link_requires_https_and_canonicalizes() {
        let link = CreateLink::encode(
            RelayMode::RemoteTls,
            "https://Relay.Example:443/",
            VAULT,
            CreateTicket::from_bytes(TICKET),
            None,
        )
        .expect("remote link");
        assert_eq!(link.relay, "https://relay.example");
        assert_eq!(link.pin, None);
        // 明文 http 不能编码成 remote 链接；localhost 也不能冒充远端。
        assert!(CreateLink::encode(
            RelayMode::RemoteTls,
            "http://127.0.0.1:18080",
            VAULT,
            CreateTicket::from_bytes(TICKET),
            None,
        )
        .is_err());
        // 远端解析同样只接受 https。
        let parsed = CreateLink::parse(&link.url).expect("remote parse");
        assert_eq!(parsed.mode, RelayMode::RemoteTls);
    }

    #[test]
    fn parse_rejects_bad_shape_without_echoing_the_ticket() {
        let good = local_link(None).url;
        let ticket = good
            .split_once("#ticket=")
            .map(|(_, value)| value.to_owned())
            .expect("ticket");
        for bad in [
            String::new(),
            format!("mn-pair://join?v=1&relay=http%3A%2F%2F127.0.0.1%3A18080&vault={}#ticket={ticket}", crate::ids::hex_of(&VAULT)),
            format!("mn-pair://create?relay=http%3A%2F%2F127.0.0.1%3A18080&vault={}#ticket={ticket}", crate::ids::hex_of(&VAULT)),
            // 版本
            format!("mn-pair://create?v=2&relay=http%3A%2F%2F127.0.0.1%3A18080&vault={}#ticket={ticket}", crate::ids::hex_of(&VAULT)),
            // 未知字段 / 额外 path / userinfo / 端口
            format!("mn-pair://create?v=1&relay=http%3A%2F%2F127.0.0.1%3A18080&vault={}&next=x#ticket={ticket}", crate::ids::hex_of(&VAULT)),
            format!("mn-pair://create/extra?v=1&relay=http%3A%2F%2F127.0.0.1%3A18080&vault={}#ticket={ticket}", crate::ids::hex_of(&VAULT)),
            format!("mn-pair://user@create?v=1&relay=http%3A%2F%2F127.0.0.1%3A18080&vault={}#ticket={ticket}", crate::ids::hex_of(&VAULT)),
            format!("mn-pair://create:8080?v=1&relay=http%3A%2F%2F127.0.0.1%3A18080&vault={}#ticket={ticket}", crate::ids::hex_of(&VAULT)),
            // relay 非 https / 非 loopback http
            format!("mn-pair://create?v=1&relay=http%3A%2F%2Frelay.example&vault={}#ticket={ticket}", crate::ids::hex_of(&VAULT)),
            format!("mn-pair://create?v=1&relay=https%3A%2F%2Frelay.example%2Fpath&vault={}#ticket={ticket}", crate::ids::hex_of(&VAULT)),
            // ticket 非 32B / 非法 base64url
            format!("mn-pair://create?v=1&relay=http%3A%2F%2F127.0.0.1%3A18080&vault={}#ticket=abc", crate::ids::hex_of(&VAULT)),
            format!("mn-pair://create?v=1&relay=http%3A%2F%2F127.0.0.1%3A18080&vault={}#ticket={ticket}%20", crate::ids::hex_of(&VAULT)),
            // vault 非 32 位 hex
            "mn-pair://create?v=1&relay=http%3A%2F%2F127.0.0.1%3A18080&vault=zz#ticket=x".to_owned(),
        ] {
            let error = CreateLink::parse(&bad).expect_err("必须拒绝");
            let message = format!("{error}");
            assert!(!message.contains(&ticket), "错误文案不得回显 ticket：{bad}");
            assert!(!format!("{error:?}").contains(&ticket));
        }
    }

    #[test]
    fn debug_and_errors_never_contain_the_ticket_or_url() {
        let link = local_link(Some(PIN));
        let parsed = CreateLink::parse(&link.url).expect("parse");
        let ticket = crate::base64url::encode(&TICKET);
        let debug = format!("{parsed:?}");
        assert!(debug.contains("<redacted: one-time create ticket>"));
        assert!(!debug.contains(&ticket));
        assert!(!debug.contains(&parsed.url));
        assert!(!format!("{:?}", parsed.ticket).contains(&ticket));
        assert!(!format!("{:?}", CreateTicket::from_bytes(TICKET)).contains(&ticket));
    }

    /// fragment 的 ticket 允许百分号编码（解析时必须严格解码；非法编码拒绝）。
    #[test]
    fn ticket_fragment_accepts_percent_encoding() {
        let link = local_link(None);
        let (head, value) = link.url.split_once("#ticket=").expect("fragment");
        // 把 base64url 的第一个字符编码成百分号形式（值本身不变）。
        let mut chars = value.chars();
        let first = chars.next().expect("非空");
        let rest = chars.as_str();
        let encoded = format!("{head}#ticket=%{:02X}{rest}", first as u32);
        let parsed = CreateLink::parse(&encoded).expect("百分号编码的 ticket 应可解析");
        assert!(parsed.ticket.matches(&CreateTicket::from_bytes(TICKET)));
        assert_eq!(
            parsed.url, link.url,
            "解析后按 canonical 重建（不保留原始文本）"
        );

        // 非法百分号编码明确拒绝，且不回显。
        let broken = format!("{head}#ticket=%ZZ{rest}");
        let error = CreateLink::parse(&broken).expect_err("非法百分号必须拒绝");
        assert!(!format!("{error}").contains(value));
    }

    #[test]
    fn link_context_is_always_v2_and_mode_matched() {
        let link = local_link(None);
        let context = link
            .relay_context(crate::http_transport::DEFAULT_TRANSPORT_TIMEOUTS)
            .expect("context");
        assert_eq!(context.mode, RelayMode::LocalDev);
        assert_eq!(context.api_version(), RelayApiVersion::V2);
        assert_eq!(context.origin_str(), "http://127.0.0.1:18080");
        assert_eq!(
            context.api().create_vault_path(),
            "/v2/vaults",
            "票据端点必须走 v2 面"
        );
    }
}
