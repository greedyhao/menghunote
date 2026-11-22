//! `mn-pair://create` 链接的 **relay 生产端渲染**（ADR 0018 S4a Web 管理面）。
//!
//! 链接格式的唯一权威实现仍是 `menghunote-sync::create_link`（客户端解析/编码）。
//! 但按 ADR 0003/0006 的依赖方向，`relay` **只依赖 `sync-proto`**，不能反向依赖
//! `sync`（即使 dev-dependency 也不引入，避免边界倒挂与编译膨胀）。因此这里只实现
//! Web 管理面领取链接所需的最小 canonical 子集：
//!
//! - origin **只**来自配置 `public_url`（canonical origin；绝不从 `Host`/`listen` 推导；
//!   见 [`crate::config::PublicUrl`]）；
//! - query 值用 RFC 3986 unreserved 之外的字节全部**大写**百分号编码（与 `sync` 相同）；
//! - `vault` = 32 位小写 hex；`ticket` = fragment 里的无 padding base64url。
//!
//! **一致性由测试锁死**：`crates/sync/tests/create_link_relay_output.rs` 用
//! `CreateLink::parse` / `CreateLink::encode` 对同一输入逐字节比对（relay 的测试
//! 依赖方向是 sync → relay，允许这样验证）。任何一侧改格式都会立刻红。
//!
//! 本模块**不打印/不落盘** ticket；只把调用方传入的字节拼进返回的 String。

use crate::admin::CREATE_TICKET_BYTES;
use crate::ids::{b64url_encode, hex_lower, Id16};

/// 创建链接的 scheme/host（`mn-pair://create`）。
pub const CREATE_LINK_PREFIX: &str = "mn-pair://create";

/// 创建链接格式版本（与 `sync::create_link` 的 `v=1` 一致）。
pub const CREATE_LINK_VERSION: &str = "1";

/// 渲染 `mn-pair://create?v=1&relay=…&vault=…#ticket=…`（无 `pin`）。
///
/// `origin` 必须是 canonical origin（来自配置 `public_url`）；本函数只做 query 编码，
/// 不校验/改写 origin（校验属于 [`crate::config::PublicUrl`] 与客户端唯一 URL parser）。
pub fn create_link_url(origin: &str, vault_id: Id16, ticket: &[u8; CREATE_TICKET_BYTES]) -> String {
    format!(
        "{CREATE_LINK_PREFIX}?v={CREATE_LINK_VERSION}&relay={}&vault={}#ticket={}",
        percent_encode(origin),
        hex_lower(&vault_id),
        b64url_encode(ticket)
    )
}

/// 只保留 RFC 3986 unreserved 字符；`:` `/` `[` `]` 等一律百分号编码（大写 hex）。
///
/// 与 `sync::create_link` 的 `percent_encode` 逐字节一致（见模块级测试）。
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_is_canonical_and_ticket_is_base64url_in_fragment() {
        let vault = [0x5au8; 16];
        let ticket = [0x11u8; 32];
        let link = create_link_url("http://127.0.0.1:18080", vault, &ticket);
        assert_eq!(
            link,
            format!(
                "mn-pair://create?v=1&relay=http%3A%2F%2F127.0.0.1%3A18080&vault={}#ticket={}",
                "5a".repeat(16),
                b64url_encode(&ticket)
            )
        );
        // ticket 只在 fragment；query 里没有明文/hex 票据。
        let (head, fragment) = link.split_once('#').expect("fragment");
        assert!(!head.contains(&hex_lower(&ticket)));
        assert_eq!(
            fragment,
            format!("ticket={}", b64url_encode(&ticket)),
            "base64url 与 hex 必须来自同一 ticket"
        );
        let encoded = fragment.strip_prefix("ticket=").expect("fragment 字段名");
        assert!(!encoded.contains('='), "base64url 无 padding");
    }

    #[test]
    fn percent_encoding_matches_unreserved_rules() {
        assert_eq!(percent_encode("aZ0-._~"), "aZ0-._~");
        assert_eq!(
            percent_encode("http://[::1]:80/x?y#z @"),
            "http%3A%2F%2F%5B%3A%3A1%5D%3A80%2Fx%3Fy%23z%20%40"
        );
    }
}
