//! ADR 0017 S5a / ADR 0018 S4a：relay 渲染的 `mn-pair://create` 领取链接与
//! `sync::create_link` 唯一权威编码**逐字节一致**。
//!
//! 依赖方向：`sync` 的 `[dev-dependencies]` 已包含 `menghunote-relay`（e2e harness 用），
//! 因此这里可以反向校验 relay CLI 的渲染；relay 生产代码**不**依赖 sync。
//!
//! 这不是"再实现一份解析"：解析、canonical 重建、vault/ticket/relay 断言全部走
//! `CreateLink::parse`，并与 `CreateLink::encode` 的输出做字节比较。

use menghunote_relay::config::PublicUrl;
use menghunote_relay::ticket_link::create_link_url;
use menghunote_sync::create_link::{CreateLink, CreateTicket};
use menghunote_sync::relay_policy::RelayMode;

const VAULT: [u8; 16] = [0x5au8; 16];
const TICKET: [u8; 32] = [0x11u8; 32];

fn assert_same_link(origin: &str) {
    let printed = create_link_url(origin, VAULT, &TICKET);
    let canonical = CreateLink::encode(
        RelayMode::LocalDev,
        origin,
        VAULT,
        CreateTicket::from_bytes(TICKET),
        None,
    )
    .expect("sync canonical encode");
    assert_eq!(
        printed, canonical.url,
        "relay CLI 与 sync 的链接必须逐字节一致（origin={origin}）"
    );

    let parsed = CreateLink::parse(&printed).expect("relay 输出必须可被 CreateLink::parse 解析");
    assert_eq!(parsed.mode, RelayMode::LocalDev);
    assert_eq!(
        parsed.relay, origin,
        "relay 必须重新 canonical 化到同一 origin"
    );
    assert_eq!(parsed.vault_id, VAULT);
    assert!(parsed.ticket.matches(&CreateTicket::from_bytes(TICKET)));
    assert_eq!(parsed.url, printed, "canonical 重建必须字节一致");

    // ticket 只在 fragment；hex 与 base64url 是同一 32B 的两种视图。
    let (head, fragment) = printed.split_once('#').expect("fragment");
    assert!(!head.contains("ticket="), "ticket 不得进 query/referrer");
    let encoded = fragment.strip_prefix("ticket=").expect("ticket 字段");
    assert_eq!(
        menghunote_sync::base64url::decode(encoded).expect("base64url"),
        TICKET,
        "fragment 的 base64url 必须还原成同一 ticket"
    );
    assert_eq!(
        menghunote_sync::ids::hex_of(&VAULT),
        "5a".repeat(16),
        "vault 是 32 位小写 hex（公开路由标识）"
    );
    assert!(
        head.ends_with(&format!("vault={}", menghunote_sync::ids::hex_of(&VAULT))),
        "{head}"
    );
}

/// ADR 0018 S4a：配置里的 canonical `public_url` 必须与客户端唯一 URL parser 的
/// canonical origin **逐字节一致**，且 relay 生成的领取链接可被 `CreateLink::parse` 解析。
/// （反向依赖在这里是允许的：sync 的 dev-dependency 已包含 relay。）
fn assert_public_url_matches_sync_parser(raw: &str, expected: &str, mode: RelayMode) {
    let public = PublicUrl::parse(raw).unwrap_or_else(|err| panic!("{raw}: {err}"));
    assert_eq!(public.as_str(), expected, "raw={raw}");
    let link = create_link_url(public.as_str(), VAULT, &TICKET);
    let parsed = CreateLink::parse(&link).expect("sync 必须能解析 relay 生成的领取链接");
    assert_eq!(
        parsed.relay, expected,
        "链接里的 relay 必须与配置的 public_url canonical 严格一致"
    );
    assert_eq!(parsed.mode, mode);
    assert_eq!(parsed.vault_id, VAULT);
    assert!(parsed.ticket.matches(&CreateTicket::from_bytes(TICKET)));
}

#[test]
fn relay_link_matches_sync_create_link_ipv4() {
    assert_same_link("http://127.0.0.1:18080");

    // ADR 0018 S4a：claim 领取链接的 relay origin 只来自配置 public_url（不是 listen/Host）。
    assert_public_url_matches_sync_parser(
        "http://127.0.0.1:18080",
        "http://127.0.0.1:18080",
        RelayMode::LocalDev,
    );
    assert_public_url_matches_sync_parser(
        "HTTP://LOCALHOST:80/",
        "http://localhost",
        RelayMode::LocalDev,
    );
    assert_public_url_matches_sync_parser(
        "http://[::1]:18080",
        "http://[::1]:18080",
        RelayMode::LocalDev,
    );
    assert_public_url_matches_sync_parser(
        "https://Relay.Example.com:443/",
        "https://relay.example.com",
        RelayMode::RemoteTls,
    );
    assert_public_url_matches_sync_parser(
        "https://[2001:DB8::1]:8443",
        "https://[2001:db8::1]:8443",
        RelayMode::RemoteTls,
    );
}

#[test]
fn relay_link_matches_sync_create_link_ipv6_and_default_port() {
    assert_same_link("http://[::1]:18080");
    // 默认端口 80 省略、IPv6 方括号（sync 的 canonical origin 规则）。
    assert_same_link("http://127.0.0.1");
    assert_same_link("http://[::1]");
}

/// 非 loopback 明文 origin：relay 的链接渲染不拦（它只做 canonical 编码），但客户端
/// `local_dev` 白名单**故意**拒绝它（生产必须 TLS）。这证明 dev-only 例外没有被静默放宽。
#[test]
fn non_loopback_origin_is_rendered_but_client_policy_still_rejects_it() {
    let printed = create_link_url("http://192.168.1.10:8080", VAULT, &TICKET);
    assert!(
        printed.contains("relay=http%3A%2F%2F192.168.1.10%3A8080"),
        "{printed}"
    );
    assert!(
        CreateLink::parse(&printed).is_err(),
        "生产 local_dev 策略必须拒绝非 loopback http（此链接仅 dev 人工使用）"
    );
}
