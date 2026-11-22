//! ADR 0016 **S4d1c**：客户端 V2 管理 UI（通道 A 双签）。
//!
//! ## 范围
//!
//! 本模块是 App 侧的**管理页**（设置页里的「管理」区，不新建窗口）：
//!
//! 1. **状态概览**：当前 vault 是否 V2、control head/rev、authz/authority rev、
//!    cap 摘要、设备列表（ACTIVE/PENDING/REVOKED）、待办审批数与错误/过期/撤权提示；
//! 2. **设备/成员**：只读投影（链上事实 + relay 的 PENDING 成员提示），不在这里实现
//!    新的准入/撤销 wire（复用 S4b2 的管理能力）；
//! 3. **双签审批（通道 A）**：展示本地草稿/导入包的状态与全部公开证据（kind、目标、
//!    cap scope、租户/实例/路径/rev、A/B 槽位指纹、`h` 的 128 位比较指纹（可展开
//!    256 位）、回执/失败），支持导入/导出 `.mn-approval`（文件或带版本前缀 base64url）、
//!    A 签署、B 加签、满签提交、取消。**不自动签/不自动改 rev/不延 deadline**。
//!
//! ## 边界
//!
//! - 所有网络/签名/编解码都走既有客户端 API（[`V2ManagementClient`]、
//!   [`ApprovalDraftV1`]）；本模块不直接读写 SQLite、不读身份文件以外的密钥材料、
//!   不复制任何 crypto；
//! - 网络/身份解封/签名在专用 OS 线程（[`ManagementService`]）里执行，GPUI 线程只做
//!   非阻塞 `poll()` 与投影；
//! - **等待第二签不持 vault 编辑/同步闸**：本模块从不调用 `storage::session` 的闸门，
//!   也不碰 `AppShell` 的同步状态机；失败不清库、不换 pin、不回退 creator；
//! - 重启恢复：本地草稿/回执来自 vault 公开 meta（[`ApprovalDraftV1::load_draft`]），
//!   刷新时重新走 pin/控制链验证（`refresh_control` + `verify_against`）；
//! - **秘密纪律**：本模块不打印任何日志；错误统一经 [`sanitize_management_text`]
//!   去掉 URL/绝对路径/恢复码/长 token 后再显示；操作详情只展示**公开证据**
//!   （op/签名/公开 cap/rev），不含 root/seed/设备私钥/邀请明文 token。
//!
//! ## 固定 selector（headless 测试契约）
//!
//! | selector | 元素 |
//! | --- | --- |
//! | `mgmt-panel` | 管理区根 |
//! | `mgmt-v2-status` | V2 状态（含「当前 vault 未启用 V2」明确文案） |
//! | `mgmt-control` | control head/rev + authz/authority rev |
//! | `mgmt-policy` | PolicyV2 摘要 |
//! | `mgmt-summary` | cap/设备/待办数量摘要 |
//! | `mgmt-cap-<i>` / `mgmt-device-<i>` | 第 i 张 cap / 第 i 台设备 |
//! | `mgmt-pending` | 待办审批数 |
//! | `mgmt-refresh` | 刷新按钮 |
//! | `mgmt-approval` | 审批区根（无草稿时也显示空态） |
//! | `mgmt-approval-state` / `kind` / `target` / `scopes` / `ids` / `path` / `revs` | 操作详情 |
//! | `mgmt-approval-fingerprint-128` | `h` 的 128 位比较指纹 |
//! | `mgmt-toggle-fingerprint` / `mgmt-approval-fingerprint-256` | 展开/收起完整 256 位 |
//! | `mgmt-approval-slot-<i>` | 第 i 个冻结签名槽 |
//! | `mgmt-approval-receipt` / `mgmt-approval-failure` | 回执 / 失败信息 |
//! | `mgmt-sign` / `mgmt-submit` / `mgmt-cancel-approval` | A 签 / 满签提交 / 取消协调 |
//! | `mgmt-path-input` / `mgmt-base64-input` | 导入导出路径 / 复制文本（base64url） |
//! | `mgmt-import-file` / `mgmt-import-base64` / `mgmt-export-file` / `mgmt-copy-base64` | 四个动作 |
//! | `mgmt-notice` / `mgmt-error` | 提示 / 脱敏错误摘要 |
//!
//! selector 只是测试元数据，业务逻辑不得依赖它们。

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use gpui::prelude::*;
use gpui::{
    div, px, App, ClipboardItem, Context, Entity, IntoElement, MouseButton, Render, SharedString,
    Window,
};

use menghunote_storage::Vault;
use menghunote_sync::approval_v1::{
    ApprovalDraftV1, ApprovalEnvelopeV1, ApprovalFailureV1, ApprovalStateV1, SignerSlotV1,
};
use menghunote_sync::device::DeviceSigningKey;
use menghunote_sync::http_transport::StdTcpTransport;
use menghunote_sync::identity::LocalIdentityV1;
use menghunote_sync::management_v2::{
    AuthzStateV2, ControlStateV2, RelayMemberHintV2, V2ControlReplay, V2ManagementClient,
    V2ManagementConfig,
};
use menghunote_sync::relay_context::RelayContext;
use menghunote_sync::relay_policy::RelayMode;

use crate::dev_sync::identity::DevIdentityStore;
use crate::dev_sync::service::{ServiceConfig, TransportTimeouts};
use crate::editor::theme;
use crate::onboarding::input::FormInput;
use crate::onboarding::redact_recovery_codes;

/// 管理面板事件泵周期（与 dev-sync 面板同一节奏；headless 下用 `advance_clock` 推进）。
pub const MANAGEMENT_PUMP_INTERVAL: Duration = Duration::from_millis(40);
/// 错误摘要最大字符数（超长截断；不把整段 SQL/传输细节塞进 UI）。
pub const MAX_MANAGEMENT_ERROR_CHARS: usize = 240;
/// 未启用 V2 的固定文案（不伪造成功）。
pub const V2_DISABLED_MESSAGE: &str =
    "当前 vault 未启用 V2（没有已回放的 UpgradePolicy）：管理动作不可用，下面不会伪造成功。";

// ---------------------------------------------------------------- 公开快照（纯数据）

/// 设备在管理页上的状态（**展示**用；ACTIVE/REVOKED 来自已验证控制链）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceStatusView {
    /// 已验证控制链上的 ACTIVE 设备。
    Active,
    /// relay 投影里的 PENDING（已兑换邀请、尚未批准；不在链上）。
    Pending,
    /// 已验证控制链上的 REVOKED（永久事实）。
    Revoked,
    /// relay 报 active 但不在已验证链上（不可信，只提示）。
    Unverified,
}

impl DeviceStatusView {
    /// 固定展示文案（ACTIVE/PENDING/REVOKED）。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Active => "ACTIVE",
            Self::Pending => "PENDING",
            Self::Revoked => "REVOKED",
            Self::Unverified => "未在已验证链上",
        }
    }
}

/// 一台设备的只读投影。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceView {
    /// 设备 id。
    pub device_id: [u8; 16],
    /// 展示状态。
    pub status: DeviceStatusView,
    /// 证书 serial（链上设备）。
    pub cert_serial: Option<u64>,
    /// 数据权限位（READ=1/WRITE=2）。
    pub data_rights: Option<u32>,
    /// 证书批准的控制 rev。
    pub approved_rev: Option<u64>,
    /// 撤销控制 rev。
    pub revoked_rev: Option<u64>,
    /// 该设备在当前代下的 cap 摘要（scope/到期；仅展示）。
    pub cap_summary: Option<String>,
    /// 附加说明（仅展示）。
    pub note: Option<String>,
}

/// 一张 cap 的只读投影。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapView {
    /// cap id。
    pub cap_id: [u8; 16],
    /// 主体设备。
    pub subject_device_id: [u8; 16],
    /// scope 位。
    pub scopes: u32,
    /// 生效时刻。
    pub not_before: u64,
    /// 到期时刻。
    pub expires_at: u64,
    /// 是否已被显式撤销。
    pub revoked: bool,
    /// 此刻是否有效（`cap_is_effective_at` 的投影）。
    pub effective: bool,
    /// 授予它的控制 rev。
    pub granted_rev: u64,
}

/// `PolicyV2` 的只读投影。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PolicyView {
    /// 高风险门限。
    pub highrisk_threshold: u32,
    /// 最小 quorum。
    pub min_quorum: u32,
    /// cap 最大 TTL（秒）。
    pub max_cap_ttl: u64,
    /// 邀请最大 TTL（秒）。
    pub max_invite_ttl: u64,
}

/// 一个冻结签名槽的只读投影。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotView {
    /// 槽下标。
    pub index: usize,
    /// 角色中文标签。
    pub role_label: &'static str,
    /// 设备/PoP 设备 id。
    pub device_id: Option<[u8; 16]>,
    /// role0 绑定的 cap。
    pub cap_id: Option<[u8; 16]>,
    /// 公钥短指纹（16 hex；公开材料）。
    pub key_fingerprint: String,
    /// 是否已有签名。
    pub signed: bool,
}

/// 回执的只读投影。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReceiptView {
    /// 提交后 rev。
    pub control_rev: u64,
    /// 链尾 hash。
    pub head_hash: [u8; 32],
    /// kind。
    pub kind: u8,
    /// `authz_rev`。
    pub authz_rev: u64,
    /// `authority_rev`。
    pub authority_rev: u64,
}

/// 审批草稿的只读投影（全部是公开证据）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalView {
    /// 状态机状态。
    pub state: ApprovalStateV1,
    /// 状态中文标签。
    pub state_label: &'static str,
    /// kind（`ControlKindV2::as_u8`）。
    pub kind: u8,
    /// kind 中文标签。
    pub kind_label: &'static str,
    /// `request_id`。
    pub request_id: [u8; 16],
    /// 目标摘要（设备/cap/邀请等公开事实）。
    pub target: String,
    /// cap scope 摘要（签发证据）。
    pub scopes: String,
    /// 租户 vault id。
    pub vault_id: [u8; 16],
    /// relay 实例 id。
    pub instance_id: [u8; 16],
    /// 唯一提交路径。
    pub canonical_path: String,
    /// CAS 期望 rev。
    pub expected_control_rev: u64,
    /// 前链尾 hash。
    pub prev_control_hash: [u8; 32],
    /// 期望 authz rev。
    pub authz_rev: u64,
    /// authority rev。
    pub authority_rev: u64,
    /// 签名时刻。
    pub signed_at: u64,
    /// 短窗截止。
    pub not_after: u64,
    /// 相关到期时刻（cap/邀请；`None` = 不适用）。
    pub expires_at: Option<u64>,
    /// `h` 的 128 位比较指纹（前 16 字节的 hex）。
    pub fingerprint_128: String,
    /// `h` 的完整 256 位（hex）。
    pub fingerprint_256: String,
    /// 冻结签名槽。
    pub slots: Vec<SlotView>,
    /// 已有签名数。
    pub signature_count: usize,
    /// 槽总数。
    pub signer_count: usize,
    /// 发起者在槽位里的下标。
    pub initiator_index: u32,
    /// 本机钥是否可以签（唯一匹配未签槽；首签必须是发起者）。
    pub can_sign_local: bool,
    /// 是否满签。
    pub fully_signed: bool,
    /// 是否已导交给对方。
    pub handed_off: bool,
    /// 已确认回执。
    pub receipt: Option<ReceiptView>,
    /// 最近一次失败（分类标签 + 脱敏信息）。
    pub failure: Option<(String, String)>,
    /// 是否超出短窗（必须新建 Operation 重签）。
    pub expired: bool,
    /// 本地重验失败原因（链前进/证据缺口等；`None` = 未发现失效）。
    pub invalid_reason: Option<String>,
}

impl ApprovalView {
    /// 是否还有可执行的下一步（仅用于按钮置灰的视觉提示）。
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self.state, ApprovalStateV1::Confirmed)
    }

    /// 是否允许提交（满签且尚未确认）。
    #[must_use]
    pub fn can_submit(&self) -> bool {
        self.fully_signed && !self.is_terminal() && !self.expired && self.invalid_reason.is_none()
    }
}

/// 管理页的完整只读快照（渲染的唯一数据源）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagementSnapshot {
    /// 当前 vault 是否已回放出 V2。
    pub v2: bool,
    /// 未启用/未刷新时的明确说明。
    pub v2_message: Option<String>,
    /// relay origin（公开地址；错误文案里会被脱敏）。
    pub relay: String,
    /// 已验证控制链尾 rev。
    pub control_rev: u64,
    /// 已验证控制链尾 hash。
    pub head_hash: [u8; 32],
    /// authz rev。
    pub authz_rev: u64,
    /// authority rev。
    pub authority_rev: u64,
    /// 授权状态。
    pub authz_state: Option<AuthzStateV2>,
    /// 当前 policy。
    pub policy: Option<PolicyView>,
    /// cap 列表。
    pub caps: Vec<CapView>,
    /// 设备列表（ACTIVE/PENDING/REVOKED）。
    pub devices: Vec<DeviceView>,
    /// 待办审批数（PENDING 设备 + 本机未确认草稿）。
    pub pending_approvals: usize,
    /// 本机审批草稿投影。
    pub approval: Option<ApprovalView>,
    /// 最近一次刷新时刻（Unix 秒）。
    pub last_refresh_unix: Option<u64>,
    /// 是否有命令在途（渲染时按钮置灰）。
    pub loading: bool,
    /// 脱敏错误摘要。
    pub error: Option<String>,
    /// 提示（已排队/已导入/已复制等）。
    pub notice: Option<String>,
}

impl Default for ManagementSnapshot {
    fn default() -> Self {
        Self {
            v2: false,
            v2_message: Some("尚未刷新（打开管理区后会自动拉取已验证控制链）".to_owned()),
            relay: String::new(),
            control_rev: 0,
            head_hash: [0u8; 32],
            authz_rev: 0,
            authority_rev: 0,
            authz_state: None,
            policy: None,
            caps: Vec::new(),
            devices: Vec::new(),
            pending_approvals: 0,
            approval: None,
            last_refresh_unix: None,
            loading: false,
            error: None,
            notice: None,
        }
    }
}

// ---------------------------------------------------------------- 纯展示工具

/// 16B id 的短形式（前 8 hex）。
#[must_use]
pub fn short_id(id: &[u8; 16]) -> String {
    menghunote_sync::ids::hex_of(&id[..4])
}

/// 32B hash 的短形式（前 8 字节 = 16 hex）。
#[must_use]
pub fn short_hash(hash: &[u8]) -> String {
    let take = hash.len().min(8);
    menghunote_sync::ids::hex_of(&hash[..take])
}

/// 公钥比较指纹：前 8 字节的 hex（公开材料；双端可人工比对）。
#[must_use]
pub fn key_fingerprint(pk: &[u8; 32]) -> String {
    menghunote_sync::ids::hex_of(&pk[..8])
}

/// `h` 的 128 位比较指纹（32 hex；每 8 个字符一组便于比对）。
#[must_use]
pub fn format_fingerprint_128(bytes: &[u8; 16]) -> String {
    let hex = menghunote_sync::ids::hex_of(bytes);
    hex.as_bytes()
        .chunks(8)
        .map(|chunk| String::from_utf8_lossy(chunk).into_owned())
        .collect::<Vec<_>>()
        .join(" ")
}

/// 完整 256 位 `h`（64 hex；每 8 个字符一组）。
#[must_use]
pub fn format_fingerprint_256(bytes: &[u8; 32]) -> String {
    let hex = menghunote_sync::ids::hex_of(bytes);
    hex.as_bytes()
        .chunks(8)
        .map(|chunk| String::from_utf8_lossy(chunk).into_owned())
        .collect::<Vec<_>>()
        .join(" ")
}

/// cap scope 位的中文摘要。
#[must_use]
pub fn format_scopes(bits: u32) -> String {
    // 与 `sync-proto::CapScope` 的位定义一致（app 不直接依赖 sync-proto）：
    // INVITE=1、APPROVE=2、REVOKE=4、CAP_ADMIN=8、ROTATE=16、POLICY=32。
    let mut parts = Vec::new();
    for (bit, label) in [
        (1u32, "邀请"),
        (2u32, "批准"),
        (4u32, "撤销"),
        (8u32, "cap 管理"),
        (16u32, "轮换"),
        (32u32, "策略"),
    ] {
        if bits & bit != 0 {
            parts.push(label);
        }
    }
    if parts.is_empty() {
        "—".to_owned()
    } else {
        parts.join("/")
    }
}

/// 数据权限位摘要（READ=1/WRITE=2）。
#[must_use]
pub fn format_data_rights(bits: u32) -> String {
    match (bits & 1 != 0, bits & 2 != 0) {
        (true, true) => "读写".to_owned(),
        (true, false) => "只读".to_owned(),
        (false, true) => "只写".to_owned(),
        (false, false) => "无数据权限".to_owned(),
    }
}

/// Unix 秒 → `YYYY-MM-DD HH:MM UTC`（纯算术，不引依赖）。
#[must_use]
pub fn format_unix_utc(secs: u64) -> String {
    let days = i64::try_from(secs / 86_400).unwrap_or(0);
    let rem = secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02} UTC",
        rem / 3_600,
        (rem % 3_600) / 60
    )
}

/// Howard Hinnant 的 `civil_from_days`（1970-01-01 为 0）。
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// 错误/提示的**脱敏摘要**：遮恢复码、URL、绝对路径与长 token，再截断。
///
/// 规则（ADR 0016「错误只显示摘要」）：
/// 1. `mnrec1…` 恢复码 token → `<恢复码已隐藏>`（复用 onboarding 的 redactor）；
/// 2. `http(s)://…` / `mn-pair://…` → `<地址已隐藏>`；
/// 3. 绝对路径（`/…/…`、`X:\…`）→ `<路径已隐藏>`；
/// 4. 连续 ≥64 字符的 ASCII 字母数字/`-`/`_`/`+`/`/` 长串（base64url 审批包等）→
///    `<长文本已隐藏>`；
/// 5. 超长截断到 [`MAX_MANAGEMENT_ERROR_CHARS`]。
#[must_use]
pub fn sanitize_management_text(text: &str) -> String {
    let mut out = redact_recovery_codes(text);
    out = redact_urls(&out);
    out = redact_paths(&out);
    out = redact_long_tokens(&out);
    if out.chars().count() > MAX_MANAGEMENT_ERROR_CHARS {
        out = out
            .chars()
            .take(MAX_MANAGEMENT_ERROR_CHARS)
            .collect::<String>()
            + "…（已截断）";
    }
    out
}

fn redact_urls(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        let next = ["http://", "https://", "mn-pair://"]
            .iter()
            .filter_map(|scheme| rest.find(scheme).map(|index| (index, *scheme)))
            .min_by_key(|(index, _)| *index);
        let Some((index, _)) = next else {
            out.push_str(rest);
            return out;
        };
        out.push_str(&rest[..index]);
        let scheme_rest = &rest[index..];
        let end = scheme_rest
            .find(|c: char| {
                c.is_whitespace() || matches!(c, '"' | '\'' | ')' | '>' | '，' | '。' | '；')
            })
            .unwrap_or(scheme_rest.len());
        out.push_str("<地址已隐藏>");
        rest = &scheme_rest[end..];
    }
}

fn redact_paths(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(index) = path_start(rest) {
        out.push_str(&rest[..index]);
        let path_rest = &rest[index..];
        let end = path_rest
            .find(|c: char| {
                c.is_whitespace() || matches!(c, '"' | '\'' | ')' | '>' | '，' | '。' | '；' | '：')
            })
            .unwrap_or(path_rest.len());
        out.push_str("<路径已隐藏>");
        rest = &path_rest[end..];
    }
    out.push_str(rest);
    out
}

/// 找一个**看起来是绝对路径**的起点（`/…/…` 或 `X:\…`）。
fn path_start(text: &str) -> Option<usize> {
    let mut previous: Option<char> = None;
    for (index, c) in text.char_indices() {
        let at_boundary = previous.is_none_or(|previous| {
            previous.is_whitespace()
                || matches!(
                    previous,
                    '"' | '\'' | '(' | '（' | '=' | ':' | '：' | ',' | '，' | ';' | '；'
                )
        });
        if at_boundary {
            let rest = &text[index + c.len_utf8()..];
            if c == '/' && rest.chars().take(260).any(|next| next == '/') {
                return Some(index);
            }
            // Windows 盘符：`C:\\...` 或 `C:/...`
            if c.is_ascii_alphabetic() && (rest.starts_with(":\\") || rest.starts_with(":/")) {
                return Some(index);
            }
        }
        previous = Some(c);
    }
    None
}

fn redact_long_tokens(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut run_start = 0usize;
    let mut run_len = 0usize;
    let flush = |out: &mut String, start: usize, end: usize| {
        let token = &text[start..end];
        if token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '+' | '/'))
            && token.len() >= 64
        {
            out.push_str("<长文本已隐藏>");
        } else {
            out.push_str(token);
        }
    };
    for (index, c) in text.char_indices() {
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '+' | '/') {
            if run_len == 0 {
                run_start = index;
            }
            run_len = index + c.len_utf8() - run_start;
        } else {
            if run_len > 0 {
                flush(&mut out, run_start, index);
                run_len = 0;
            }
            out.push(c);
        }
    }
    if run_len > 0 {
        flush(&mut out, run_start, text.len());
    }
    out
}

// ---------------------------------------------------------------- 命令/事件/runner

/// 管理 worker 的命令（全部经既有客户端 API 执行）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManagementCommand {
    /// 刷新已验证控制链 + 本地草稿。
    Refresh,
    /// 从 `.mn-approval` 文件导入（限长、不联网、不自动执行）。
    ImportFile(PathBuf),
    /// 从带版本前缀的 base64url 文本导入。
    ImportBase64(String),
    /// 导出到 `.mn-approval` 文件（原子写；不自动联网）。
    ExportFile(PathBuf),
    /// 导出复制文本（base64url）。
    ExportBase64,
    /// 用本机身份钥签自己的槽（A 先签；B 审核后加签）。
    Sign,
    /// 满签提交（自证；失败不清库/不换 pin）。
    Submit,
    /// 取消协调（只清本地草稿；已外发签名不可撤回）。
    Cancel,
    /// 停止 worker（关窗）。
    Stop,
}

/// 命令的附加产物（不进入快照/布局）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManagementArtifact {
    /// 复制用的 base64url 文本（`mn-approval-v1:…`；公开证据）。
    ExportedBase64(String),
}

/// 一次命令的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagementReport {
    /// 最新快照。
    pub snapshot: ManagementSnapshot,
    /// 命令产物（例如导出文本，由面板写剪贴板）。
    pub artifact: Option<ManagementArtifact>,
}

/// 管理执行体（真实实现 = [`RealManagementRunner`]；测试可注入脚本化实现）。
pub trait ManagementRunner: Send {
    /// 执行一条命令并返回最新快照（失败也返回快照，错误放在 `snapshot.error`）。
    fn run(&mut self, command: ManagementCommand) -> ManagementReport;
}

/// worker → 面板的事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManagementEvent {
    /// 一条命令收尾（`Box` 避免 `Stopped` 被大快照撑大）。
    Updated(Box<ManagementReport>),
    /// worker 已停止。
    Stopped,
}

/// 管理 worker（专用 OS 线程 + channel；GPUI 线程只做非阻塞 poll）。
pub struct ManagementService {
    inner: Mutex<ServiceInner>,
    jobs: Mutex<Option<Sender<ManagementCommand>>>,
}

struct ServiceInner {
    snapshot: ManagementSnapshot,
    events: Receiver<ManagementEvent>,
    busy: bool,
    stopped: bool,
}

impl ManagementService {
    /// 用执行体创建服务并启动 worker（线程名固定，便于诊断）。
    #[must_use]
    pub fn new(runner: Box<dyn ManagementRunner>) -> Arc<Self> {
        let (job_tx, job_rx) = mpsc::channel::<ManagementCommand>();
        let (event_tx, event_rx) = mpsc::channel::<ManagementEvent>();
        thread::Builder::new()
            .name("menghunote-management".to_owned())
            .spawn(move || worker_loop(&job_rx, &event_tx, runner))
            .expect("启动管理 worker 线程失败（系统资源不足？）");
        Arc::new(Self {
            inner: Mutex::new(ServiceInner {
                snapshot: ManagementSnapshot::default(),
                events: event_rx,
                busy: false,
                stopped: false,
            }),
            jobs: Mutex::new(Some(job_tx)),
        })
    }

    fn lock_inner(&self) -> std::sync::MutexGuard<'_, ServiceInner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 只读快照。
    #[must_use]
    pub fn snapshot(&self) -> ManagementSnapshot {
        self.lock_inner().snapshot.clone()
    }

    /// 是否有命令在途。
    #[must_use]
    pub fn busy(&self) -> bool {
        self.lock_inner().busy
    }

    /// worker 是否已停止。
    #[must_use]
    pub fn stopped(&self) -> bool {
        self.lock_inner().stopped
    }

    /// 发一条命令（忙/已停时返回明确原因，**不排队**）。
    pub fn try_command(&self, command: ManagementCommand) -> Result<(), String> {
        let mut inner = self.lock_inner();
        if inner.stopped {
            return Err("管理 worker 已停止（请重启实例）".to_owned());
        }
        if inner.busy {
            return Err("上一管理操作仍在进行（请稍候）".to_owned());
        }
        let jobs = self
            .jobs
            .lock()
            .map_err(|_| "管理 worker 锁损坏".to_owned())?;
        let Some(sender) = jobs.as_ref() else {
            return Err("管理 worker 已关闭".to_owned());
        };
        sender
            .send(command)
            .map_err(|_| "管理 worker 已不可用（请重启实例）".to_owned())?;
        inner.busy = true;
        Ok(())
    }

    /// 请求停止（不阻塞、不 join；在途命令跑完后退出）。
    pub fn request_stop(&self) {
        if let Ok(jobs) = self.jobs.lock() {
            if let Some(sender) = jobs.as_ref() {
                let _ = sender.send(ManagementCommand::Stop);
            }
        }
    }

    /// 收事件（非阻塞）并更新快照；返回收到的事件。
    pub fn poll(&self) -> Vec<ManagementEvent> {
        let mut inner = self.lock_inner();
        let mut events = Vec::new();
        while let Ok(event) = inner.events.try_recv() {
            match &event {
                ManagementEvent::Updated(report) => {
                    inner.snapshot = report.snapshot.clone();
                    inner.busy = false;
                }
                ManagementEvent::Stopped => {
                    inner.stopped = true;
                    inner.busy = false;
                }
            }
            events.push(event);
        }
        events
    }
}

fn worker_loop(
    jobs: &Receiver<ManagementCommand>,
    events: &Sender<ManagementEvent>,
    mut runner: Box<dyn ManagementRunner>,
) {
    while let Ok(command) = jobs.recv() {
        if matches!(command, ManagementCommand::Stop) {
            break;
        }
        let mut report = runner.run(command);
        // 双保险：执行体本应已脱敏；worker 这里再脱敏一次（含提示行）。
        report.snapshot.error = report
            .snapshot
            .error
            .as_deref()
            .map(sanitize_management_text);
        report.snapshot.notice = report
            .snapshot
            .notice
            .as_deref()
            .map(sanitize_management_text);
        report.snapshot.loading = false;
        if events
            .send(ManagementEvent::Updated(Box::new(report)))
            .is_err()
        {
            break;
        }
    }
    let _ = events.send(ManagementEvent::Stopped);
}

// ---------------------------------------------------------------- GPUI 面板

/// 设置页里的「管理（V2 控制面）」面板。
pub struct ManagementPanel {
    service: Arc<ManagementService>,
    snapshot: ManagementSnapshot,
    /// 是否已经激活过（打开管理区时触发一次刷新）。
    activated: bool,
    /// 是否展开完整 256 位比较指纹。
    fingerprint_full: bool,
    /// 最近一次导出的 base64 文本（公开证据；不渲染进布局，仅供复制/测试读取）。
    exported: Option<String>,
    /// 导入/导出的本地文件路径（非秘密）。
    path_input: Entity<FormInput>,
    /// 导入用的复制文本（公开证据）。
    base64_input: Entity<FormInput>,
}

impl ManagementPanel {
    /// 用执行体创建面板（测试注入脚本化执行体）。
    pub fn with_runner(runner: Box<dyn ManagementRunner>, cx: &mut Context<Self>) -> Self {
        Self::with_service(ManagementService::new(runner), cx)
    }

    /// 用已有服务创建面板（生产 = [`RealManagementRunner`]）。
    pub fn with_service(service: Arc<ManagementService>, cx: &mut Context<Self>) -> Self {
        let snapshot = service.snapshot();
        let path_input = cx.new(|cx| {
            FormInput::new("审批包文件路径（.mn-approval）", false, cx)
                .selector("mgmt-path-input")
                .click_to_focus()
        });
        let base64_input = cx.new(|cx| {
            FormInput::new("mn-approval-v1:…（粘贴复制文本）", false, cx)
                .selector("mgmt-base64-input")
                .click_to_focus()
        });
        let panel = Self {
            service,
            snapshot,
            activated: false,
            fingerprint_full: false,
            exported: None,
            path_input,
            base64_input,
        };
        panel.spawn_pump(cx);
        panel
    }

    /// 当前快照（测试/宿主只读投影）。
    #[must_use]
    pub fn snapshot(&self) -> &ManagementSnapshot {
        &self.snapshot
    }

    /// 最近一次导出的 base64url 文本（公开证据；测试断言用）。
    #[must_use]
    pub fn exported_base64(&self) -> Option<&str> {
        self.exported.as_deref()
    }

    /// 服务句柄（宿主关停/测试注入）。
    #[must_use]
    pub fn service(&self) -> &Arc<ManagementService> {
        &self.service
    }

    /// 激活管理区（打开时调用一次：自动刷新；之后不重复）。
    pub fn activate(&mut self, cx: &mut Context<Self>) {
        if self.activated {
            return;
        }
        self.activated = true;
        self.request_refresh(cx);
    }

    /// 是否已激活。
    #[must_use]
    pub const fn is_activated(&self) -> bool {
        self.activated
    }

    /// 导入路径输入框当前文本。
    #[must_use]
    pub fn path_text(&self, cx: &App) -> String {
        self.path_input.read(cx).text().trim().to_owned()
    }

    /// base64 输入框当前文本。
    #[must_use]
    pub fn base64_text(&self, cx: &App) -> String {
        self.base64_input.read(cx).text().trim().to_owned()
    }

    /// 测试/预填：设置导入导出路径。
    pub fn set_path_text(&mut self, text: &str, cx: &mut Context<Self>) {
        self.path_input.update(cx, |input, cx| {
            input.set_text(text);
            cx.notify();
        });
    }

    /// 测试/预填：设置 base64 文本。
    pub fn set_base64_text(&mut self, text: &str, cx: &mut Context<Self>) {
        self.base64_input.update(cx, |input, cx| {
            input.set_text(text);
            cx.notify();
        });
    }

    /// 刷新（最近一次命令的提示会覆盖 `notice`）。
    pub fn request_refresh(&mut self, cx: &mut Context<Self>) {
        self.send(ManagementCommand::Refresh, "刷新", cx);
    }

    /// 从路径输入框导入。
    pub fn request_import_file(&mut self, cx: &mut Context<Self>) {
        if !self.snapshot.v2 {
            self.set_notice("当前 vault 未启用 V2：审批包无法在本机验证", cx);
            return;
        }
        let path = self.path_text(cx);
        if path.is_empty() {
            self.set_notice("请先填写 `.mn-approval` 文件路径", cx);
            return;
        }
        self.send(
            ManagementCommand::ImportFile(PathBuf::from(path)),
            "导入文件",
            cx,
        );
    }

    /// 从 base64 输入框导入。
    pub fn request_import_base64(&mut self, cx: &mut Context<Self>) {
        if !self.snapshot.v2 {
            self.set_notice("当前 vault 未启用 V2：审批包无法在本机验证", cx);
            return;
        }
        let text = self.base64_text(cx);
        if text.is_empty() {
            self.set_notice("请先粘贴带 `mn-approval-v1:` 前缀的文本", cx);
            return;
        }
        self.send(ManagementCommand::ImportBase64(text), "导入文本", cx);
    }

    /// 导出到路径输入框里的文件。
    pub fn request_export_file(&mut self, cx: &mut Context<Self>) {
        let path = self.path_text(cx);
        if path.is_empty() {
            self.set_notice("请先填写导出文件路径（.mn-approval）", cx);
            return;
        }
        self.send(
            ManagementCommand::ExportFile(PathBuf::from(path)),
            "导出文件",
            cx,
        );
    }

    /// 导出为复制文本（完成后写剪贴板）。
    pub fn request_export_base64(&mut self, cx: &mut Context<Self>) {
        self.send(ManagementCommand::ExportBase64, "复制 base64", cx);
    }

    /// A 签署（首签必须是发起者）。
    pub fn request_sign(&mut self, cx: &mut Context<Self>) {
        if !self.snapshot.v2 {
            self.set_notice("当前 vault 未启用 V2：不能签署", cx);
            return;
        }
        let Some(approval) = &self.snapshot.approval else {
            self.set_notice("没有可签署的审批草稿：先导入或等待本地草稿", cx);
            return;
        };
        if approval.expired {
            self.set_notice("审批包已超出短窗：必须新建 Operation 并全部重签", cx);
            return;
        }
        if let Some(reason) = &approval.invalid_reason {
            self.set_notice(
                format!("审批包已失效（{reason}）：必须新建 Operation 重签"),
                cx,
            );
            return;
        }
        if !approval.can_sign_local {
            self.set_notice("本机钥不能签：不是未签槽 / 首签必须是发起者 / 已满签", cx);
            return;
        }
        self.send(ManagementCommand::Sign, "签署", cx);
    }

    /// 满签提交。
    pub fn request_submit(&mut self, cx: &mut Context<Self>) {
        if !self.snapshot.v2 {
            self.set_notice("当前 vault 未启用 V2：不能提交", cx);
            return;
        }
        let Some(approval) = &self.snapshot.approval else {
            self.set_notice("没有可提交的审批草稿", cx);
            return;
        };
        if !approval.fully_signed {
            self.set_notice("包未满签：满签前禁止提交", cx);
            return;
        }
        if approval.is_terminal() {
            self.set_notice("该操作已确认（重发会幂等返回原回执）", cx);
            return;
        }
        if approval.expired {
            self.set_notice("审批包已超出短窗：必须新建 Operation 并全部重签", cx);
            return;
        }
        if let Some(reason) = &approval.invalid_reason {
            self.set_notice(
                format!("审批包已失效（{reason}）：必须新建 Operation 重签"),
                cx,
            );
            return;
        }
        self.send(ManagementCommand::Submit, "提交", cx);
    }

    /// 取消协调（只清本地草稿；已外发签名不可撤回）。
    pub fn request_cancel(&mut self, cx: &mut Context<Self>) {
        if self.snapshot.approval.is_none() {
            self.set_notice("当前没有本地审批草稿", cx);
            return;
        }
        self.send(ManagementCommand::Cancel, "取消协调", cx);
    }

    /// 展开/收起完整 256 位比较指纹。
    pub fn toggle_fingerprint(&mut self, cx: &mut Context<Self>) {
        self.fingerprint_full = !self.fingerprint_full;
        cx.notify();
    }

    fn set_notice(&mut self, notice: impl Into<String>, cx: &mut Context<Self>) {
        let notice = sanitize_management_text(&notice.into());
        self.snapshot.notice = Some(notice);
        cx.notify();
    }

    fn send(&mut self, command: ManagementCommand, label: &str, cx: &mut Context<Self>) {
        match self.service.try_command(command) {
            Ok(()) => {
                self.snapshot.loading = true;
                self.snapshot.error = None;
                self.snapshot.notice = Some(format!("已请求：{label}"));
            }
            Err(message) => {
                self.snapshot.notice = Some(format!("已忽略：{message}"));
            }
        }
        cx.notify();
    }

    /// 收一次事件并更新快照；返回是否有变化。
    pub fn pump(&mut self, cx: &mut Context<Self>) -> bool {
        let events = self.service.poll();
        if events.is_empty() {
            return false;
        }
        for event in events {
            match event {
                ManagementEvent::Updated(report) => {
                    let report = *report;
                    self.snapshot = report.snapshot;
                    if let Some(ManagementArtifact::ExportedBase64(text)) = report.artifact {
                        self.exported = Some(text.clone());
                        cx.write_to_clipboard(ClipboardItem::new_string(text));
                        self.snapshot.notice = Some(
                            "已复制审批包 base64（只写剪贴板；不自动执行、不联网）".to_owned(),
                        );
                    }
                }
                ManagementEvent::Stopped => {
                    self.snapshot.loading = false;
                    self.snapshot.error = Some(
                        "管理 worker 已停止：本地数据/草稿未改动，重启实例后可重试".to_owned(),
                    );
                }
            }
        }
        true
    }

    /// 事件泵（与 dev-sync 面板同构；headless 下显式 `advance_clock`）。
    fn spawn_pump(&self, cx: &mut Context<Self>) {
        let timers = cx.background_executor().clone();
        cx.spawn(async move |this, cx| loop {
            timers.timer(MANAGEMENT_PUMP_INTERVAL).await;
            let updated = this.update(cx, |panel, cx| {
                if panel.pump(cx) {
                    cx.notify();
                }
            });
            if updated.is_err() {
                break;
            }
        })
        .detach();
    }

    // ------------------------------------------------------------ 渲染

    fn status_line_text(&self) -> String {
        if self.snapshot.v2 {
            format!(
                "V2 已启用 · 控制链 rev {} · authz rev {} · authority rev {} · 授权状态 {}",
                self.snapshot.control_rev,
                self.snapshot.authz_rev,
                self.snapshot.authority_rev,
                match self.snapshot.authz_state {
                    Some(AuthzStateV2::Active) => "正常",
                    Some(AuthzStateV2::RecoveryOnly) => "recovery-only（管理面关闭）",
                    None => "未知",
                }
            )
        } else {
            self.snapshot
                .v2_message
                .clone()
                .unwrap_or_else(|| V2_DISABLED_MESSAGE.to_owned())
        }
    }

    fn pending_text(&self) -> String {
        let pending_devices = self
            .snapshot
            .devices
            .iter()
            .filter(|device| device.status == DeviceStatusView::Pending)
            .count();
        let local = usize::from(
            self.snapshot
                .approval
                .as_ref()
                .is_some_and(|approval| !approval.is_terminal()),
        );
        format!(
            "待办审批：{}（PENDING 设备 {pending_devices} · 本机草稿 {local}）",
            self.snapshot.pending_approvals
        )
    }

    fn action_button(
        &self,
        selector: &'static str,
        label: &'static str,
        dimmed: bool,
        primary: bool,
        cx: &mut Context<Self>,
        on_click: impl Fn(&mut ManagementPanel, &mut Context<ManagementPanel>) + 'static,
    ) -> impl IntoElement {
        let color = if dimmed {
            theme::status_bar_text()
        } else if primary {
            theme::link_color()
        } else {
            theme::text_color()
        };
        let mut element = div()
            .id(SharedString::from(selector))
            .debug_selector(move || selector.to_string())
            .px(px(8.0))
            .py(px(3.0))
            .rounded_sm()
            .border_1()
            .border_color(theme::input_border())
            .bg(theme::input_bg())
            .text_size(px(11.0))
            .text_color(color)
            .cursor_pointer()
            .child(SharedString::from(label))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |panel, _event: &gpui::MouseDownEvent, _window, cx| {
                    on_click(panel, cx);
                    cx.notify();
                    cx.stop_propagation();
                }),
            );
        if dimmed {
            element = element.opacity(0.6);
        }
        element
    }

    fn line(&self, selector: &'static str, text: impl Into<SharedString>) -> gpui::Div {
        div()
            .debug_selector(move || selector.to_string())
            .w_full()
            .text_size(px(11.0))
            .text_color(theme::text_color())
            .child(text.into())
    }

    fn small_line(&self, selector: &'static str, text: impl Into<SharedString>) -> gpui::Div {
        div()
            .debug_selector(move || selector.to_string())
            .w_full()
            .text_size(px(10.0))
            .text_color(theme::status_bar_text())
            .child(text.into())
    }

    fn render_caps(&self) -> gpui::Div {
        let mut block = div()
            .w_full()
            .flex()
            .flex_col()
            .gap_1()
            .child(self.small_line(
                "mgmt-caps",
                format!(
                    "cap 摘要（{} 张；自然到期不造记录，撤销看 revoked 标记）",
                    self.snapshot.caps.len()
                ),
            ));
        for (index, cap) in self.snapshot.caps.iter().enumerate() {
            let row = format!(
                "cap {} · 主体 {} · {} · 生效 {} → 到期 {} · 授予 rev {} · {}",
                short_id(&cap.cap_id),
                short_id(&cap.subject_device_id),
                format_scopes(cap.scopes),
                format_unix_utc(cap.not_before),
                format_unix_utc(cap.expires_at),
                cap.granted_rev,
                if cap.revoked {
                    "已撤销".to_owned()
                } else if cap.effective {
                    "有效".to_owned()
                } else {
                    "此刻无效（到期/旧代/主体非 ACTIVE）".to_owned()
                }
            );
            block = block.child(
                div()
                    .debug_selector(move || format!("mgmt-cap-{index}"))
                    .w_full()
                    .text_size(px(10.0))
                    .text_color(if cap.effective {
                        theme::text_color()
                    } else {
                        theme::status_bar_text()
                    })
                    .child(SharedString::from(row)),
            );
        }
        block
    }

    fn render_devices(&self) -> gpui::Div {
        let mut block = div()
            .w_full()
            .flex()
            .flex_col()
            .gap_1()
            .child(self.small_line(
                "mgmt-devices",
                format!(
                    "设备/成员（{} 台；ACTIVE/REVOKED 只信已验证控制链，PENDING 来自 relay 投影且不可执行）",
                    self.snapshot.devices.len()
                ),
            ));
        for (index, device) in self.snapshot.devices.iter().enumerate() {
            let mut row = format!(
                "{} · {} · 设备 {}",
                device.status.label(),
                if device.cert_serial.is_some() {
                    format!(
                        "serial {} · 权限 {}",
                        device.cert_serial.unwrap_or(0),
                        format_data_rights(device.data_rights.unwrap_or(0))
                    )
                } else {
                    "无链上证书".to_owned()
                },
                short_id(&device.device_id)
            );
            if let Some(rev) = device.approved_rev {
                row.push_str(&format!(" · 批准 rev {rev}"));
            }
            if let Some(rev) = device.revoked_rev {
                row.push_str(&format!(" · 撤销 rev {rev}"));
            }
            if let Some(caps) = &device.cap_summary {
                row.push_str(&format!(" · cap {caps}"));
            }
            if let Some(note) = &device.note {
                row.push_str(&format!(" · {note}"));
            }
            block = block.child(
                div()
                    .debug_selector(move || format!("mgmt-device-{index}"))
                    .w_full()
                    .text_size(px(10.0))
                    .text_color(match device.status {
                        DeviceStatusView::Active => theme::text_color(),
                        DeviceStatusView::Pending => theme::link_color(),
                        DeviceStatusView::Revoked => theme::danger_text(),
                        DeviceStatusView::Unverified => theme::search_warn_text(),
                    })
                    .child(SharedString::from(row)),
            );
        }
        block
    }

    fn render_approval(&self, cx: &mut Context<Self>) -> gpui::Div {
        let mut block = div()
            .debug_selector(|| "mgmt-approval".to_string())
            .w_full()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .text_size(px(13.0))
                    .text_color(theme::status_bar_text())
                    .child(SharedString::from(
                        "双签审批（通道 A：手工交换 .mn-approval）",
                    )),
            );
        let Some(approval) = &self.snapshot.approval else {
            return block.child(self.small_line(
                "mgmt-approval-empty",
                "本机没有审批草稿：从文件或复制文本导入，或等待既有本地草稿恢复。",
            ));
        };
        block = block
            .child(self.line(
                "mgmt-approval-state",
                format!(
                    "状态：{}（{}/{} 签·{}{}）",
                    approval.state_label,
                    approval.signature_count,
                    approval.signer_count,
                    if approval.fully_signed {
                        "满签"
                    } else {
                        "未满签"
                    },
                    if approval.handed_off {
                        "·已导出给 B"
                    } else {
                        ""
                    }
                ),
            ))
            .child(self.line(
                "mgmt-approval-kind",
                format!("操作：{}（kind {}）", approval.kind_label, approval.kind),
            ))
            .child(self.small_line("mgmt-approval-target", format!("目标：{}", approval.target)))
            .child(self.small_line(
                "mgmt-approval-scopes",
                format!("cap scope：{}", approval.scopes),
            ))
            .child(self.small_line(
                "mgmt-approval-ids",
                format!(
                    "租户 vault {} · 实例 {} · request {} · 发起槽 {}",
                    short_id(&approval.vault_id),
                    short_id(&approval.instance_id),
                    short_id(&approval.request_id),
                    approval.initiator_index
                ),
            ))
            .child(self.small_line(
                "mgmt-approval-path",
                format!("提交路径：{}", approval.canonical_path),
            ))
            .child(self.small_line(
                "mgmt-approval-revs",
                format!(
                    "CAS {}→{} · 前链尾 {} · authz {} · authority {} · 签名 {} · 截止 {}",
                    approval.expected_control_rev,
                    approval.expected_control_rev.saturating_add(1),
                    short_hash(&approval.prev_control_hash),
                    approval.authz_rev,
                    approval.authority_rev,
                    format_unix_utc(approval.signed_at),
                    format_unix_utc(approval.not_after)
                ),
            ))
            .child(self.line(
                "mgmt-approval-fingerprint-128",
                format!("h 128 位比较指纹：{}", approval.fingerprint_128),
            ));

        let expanded = self.fingerprint_full;
        block = block.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(self.action_button(
                    "mgmt-toggle-fingerprint",
                    if expanded {
                        "收起 256 位"
                    } else {
                        "展开 256 位"
                    },
                    false,
                    false,
                    cx,
                    |panel, cx| panel.toggle_fingerprint(cx),
                ))
                .when(expanded, |this| {
                    this.child(
                        div()
                            .debug_selector(|| "mgmt-approval-fingerprint-256".to_string())
                            .text_size(px(10.0))
                            .text_color(theme::status_bar_text())
                            .child(SharedString::from(format!(
                                "h 256 位：{}",
                                approval.fingerprint_256
                            ))),
                    )
                }),
        );

        for slot in &approval.slots {
            let row = format!(
                "槽 {} · {} · 设备 {} · cap {} · 公钥指纹 {} · {}",
                slot.index,
                slot.role_label,
                slot.device_id
                    .map_or_else(|| "—".to_owned(), |id| short_id(&id)),
                slot.cap_id
                    .map_or_else(|| "—".to_owned(), |id| short_id(&id)),
                slot.key_fingerprint,
                if slot.signed { "已签" } else { "未签" }
            );
            let index = slot.index;
            block = block.child(
                div()
                    .debug_selector(move || format!("mgmt-approval-slot-{index}"))
                    .w_full()
                    .text_size(px(10.0))
                    .text_color(if slot.signed {
                        theme::text_color()
                    } else {
                        theme::search_warn_text()
                    })
                    .child(SharedString::from(row)),
            );
        }

        if let Some(receipt) = approval.receipt {
            block = block.child(self.small_line(
                "mgmt-approval-receipt",
                format!(
                    "回执：rev {} · head {} · kind {} · authz {} · authority {}（本地链已对账）",
                    receipt.control_rev,
                    short_hash(&receipt.head_hash),
                    receipt.kind,
                    receipt.authz_rev,
                    receipt.authority_rev
                ),
            ));
        }
        if let Some((kind, message)) = &approval.failure {
            block = block.child(self.small_line(
                "mgmt-approval-failure",
                format!("最近失败：{kind} · {message}"),
            ));
        }
        if let Some(reason) = &approval.invalid_reason {
            block = block.child(self.small_line(
                "mgmt-approval-invalid",
                format!("已失效：{reason}（禁止自动改 rev/延 deadline；必须新建 Operation）"),
            ));
        }
        if approval.expired {
            block = block.child(self.small_line(
                "mgmt-approval-expired",
                "已超出短窗：提交会被拒绝；必须新建 Operation 并全部重签。",
            ));
        }

        let terminal = approval.is_terminal();
        let can_sign = approval.can_sign_local && !approval.expired;
        let can_submit = approval.can_submit();
        block = block.child(
            div()
                .flex()
                .flex_row()
                .gap_2()
                .child(self.action_button(
                    "mgmt-sign",
                    if terminal {
                        "已确认"
                    } else {
                        "A 签署 / B 加签"
                    },
                    !can_sign,
                    true,
                    cx,
                    |panel, cx| panel.request_sign(cx),
                ))
                .child(self.action_button(
                    "mgmt-submit",
                    "满签提交",
                    !can_submit,
                    true,
                    cx,
                    |panel, cx| panel.request_submit(cx),
                ))
                .child(self.action_button(
                    "mgmt-cancel-approval",
                    "取消协调（只清本地草稿）",
                    terminal,
                    false,
                    cx,
                    |panel, cx| panel.request_cancel(cx),
                )),
        );
        block
    }

    fn render_inputs(&self, cx: &mut Context<Self>) -> gpui::Div {
        let v2 = self.snapshot.v2;
        let dimmed = !v2;
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_1()
            .child(self.small_line(
                "mgmt-io-hint",
                "导入/导出（限长 1 MiB；解析不联网、不自动执行；URL/路径/秘密不进日志）",
            ))
            .child(
                div()
                    .w_full()
                    .px_2()
                    .py_1()
                    .bg(theme::window_bg())
                    .border_1()
                    .border_color(theme::input_border())
                    .rounded_sm()
                    .child(self.path_input.clone()),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .child(self.action_button(
                        "mgmt-import-file",
                        "从文件导入",
                        dimmed,
                        false,
                        cx,
                        |panel, cx| panel.request_import_file(cx),
                    ))
                    .child(self.action_button(
                        "mgmt-export-file",
                        "导出到文件",
                        dimmed || self.snapshot.approval.is_none(),
                        false,
                        cx,
                        |panel, cx| panel.request_export_file(cx),
                    )),
            )
            .child(
                div()
                    .w_full()
                    .px_2()
                    .py_1()
                    .bg(theme::window_bg())
                    .border_1()
                    .border_color(theme::input_border())
                    .rounded_sm()
                    .child(self.base64_input.clone()),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .child(self.action_button(
                        "mgmt-import-base64",
                        "从文本导入",
                        dimmed,
                        false,
                        cx,
                        |panel, cx| panel.request_import_base64(cx),
                    ))
                    .child(self.action_button(
                        "mgmt-copy-base64",
                        "复制 base64",
                        dimmed || self.snapshot.approval.is_none(),
                        false,
                        cx,
                        |panel, cx| panel.request_export_base64(cx),
                    )),
            )
    }

    fn render_body(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut body = div()
            .id("mgmt-panel")
            .debug_selector(|| "mgmt-panel".to_string())
            .w_full()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .w_full()
                    .child(
                        div()
                            .text_size(px(13.0))
                            .text_color(theme::text_color())
                            .child(SharedString::from("管理（V2 控制面 / 通道 A 双签）")),
                    )
                    .child(self.action_button(
                        "mgmt-refresh",
                        "刷新",
                        self.snapshot.loading,
                        false,
                        cx,
                        |panel, cx| panel.request_refresh(cx),
                    )),
            )
            .child(self.line("mgmt-v2-status", self.status_line_text()));

        if self.snapshot.v2 {
            body = body
                .child(self.small_line(
                    "mgmt-control",
                    format!(
                        "control rev {} · head {} · authz rev {} · authority rev {} · 刷新于 {}",
                        self.snapshot.control_rev,
                        short_hash(&self.snapshot.head_hash),
                        self.snapshot.authz_rev,
                        self.snapshot.authority_rev,
                        self.snapshot
                            .last_refresh_unix
                            .map_or_else(|| "—".to_owned(), format_unix_utc)
                    ),
                ))
                .child(self.small_line(
                    "mgmt-policy",
                    self.snapshot.policy.map_or_else(
                        || "policy：—（尚未回放 UpgradePolicy）".to_owned(),
                        |policy| {
                            format!(
                                "policy：高风险门限 {} · min quorum {} · cap TTL ≤ {}s · 邀请 TTL ≤ {}s",
                                policy.highrisk_threshold,
                                policy.min_quorum,
                                policy.max_cap_ttl,
                                policy.max_invite_ttl
                            )
                        },
                    ),
                ))
                .child(self.small_line("mgmt-pending", self.pending_text()))
                .child(self.render_caps())
                .child(self.render_devices());
        } else {
            body = body.child(self.small_line(
                "mgmt-pending",
                "待办审批：0（未启用 V2：不显示伪造的设备/cap 成功状态）",
            ));
        }

        body = body
            .child(self.render_approval(cx))
            .child(self.render_inputs(cx));

        body = body.child(
            div()
                .debug_selector(|| "mgmt-error".to_string())
                .w_full()
                .text_size(px(10.0))
                .text_color(if self.snapshot.error.is_some() {
                    theme::danger_text()
                } else {
                    theme::status_bar_text()
                })
                .child(SharedString::from(
                    self.snapshot
                        .error
                        .clone()
                        .unwrap_or_else(|| "最近错误：无".to_owned()),
                )),
        );
        body.child(
            div()
                .debug_selector(|| "mgmt-notice".to_string())
                .w_full()
                .text_size(px(10.0))
                .text_color(theme::caret_color())
                .child(SharedString::from(
                    self.snapshot.notice.clone().unwrap_or_default(),
                )),
        )
    }
}

impl Render for ManagementPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.render_body(cx)
    }
}

// ---------------------------------------------------------------- 真实执行体

/// 真实执行体的配置（来自公开配置/`ServiceConfig`；不含秘密）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagementRunConfig {
    /// canonical relay origin（公开）。
    pub relay: String,
    /// relay 接入模式（`local_dev` / `remote_tls`）。
    pub mode: RelayMode,
    /// 传输超时预算。
    pub timeouts: TransportTimeouts,
}

impl ManagementRunConfig {
    /// 从同步 `ServiceConfig` 取同一套 relay/模式/超时（唯一来源）。
    #[must_use]
    pub fn from_service(config: &ServiceConfig) -> Self {
        Self {
            relay: config.relay.clone(),
            mode: config.mode,
            timeouts: config.transport,
        }
    }
}

/// 真实执行体：身份解封/网络/签名全部在 worker 线程内，按命令重走既有客户端 API。
pub struct RealManagementRunner {
    config: ManagementRunConfig,
    vault: Vault,
    vault_path: PathBuf,
    identity: Option<LocalIdentityV1>,
    /// 已从 vault 公开 meta 读过的本地草稿（重启恢复）。
    draft: Option<ApprovalDraftV1>,
    draft_loaded: bool,
    snapshot: ManagementSnapshot,
}

impl RealManagementRunner {
    /// 构造（不碰磁盘/网络；所有 IO 在第一条命令时发生）。
    #[must_use]
    pub fn new(config: ManagementRunConfig, vault: Vault, vault_path: PathBuf) -> Self {
        let relay = config.relay.clone();
        Self {
            config,
            vault,
            vault_path,
            identity: None,
            draft: None,
            draft_loaded: false,
            snapshot: ManagementSnapshot {
                relay,
                ..ManagementSnapshot::default()
            },
        }
    }

    fn ensure_identity(&mut self) -> Result<(), String> {
        if self.identity.is_none() {
            let identity = DevIdentityStore
                .load(&self.vault_path)
                .map_err(|error| format!("读取本机身份失败：{error:#}"))?;
            self.identity = Some(identity);
        }
        Ok(())
    }

    fn local_pk(&self) -> Option<[u8; 32]> {
        self.identity
            .as_ref()
            .map(|identity| DeviceSigningKey::from_seed(identity.ed25519_seed.clone()).public_key())
    }

    fn client(&self) -> Result<V2ManagementClient<StdTcpTransport>, String> {
        let identity = self
            .identity
            .as_ref()
            .ok_or_else(|| "本机身份尚未解封".to_owned())?;
        let context =
            RelayContext::from_mode(self.config.mode, &self.config.relay, self.config.timeouts)
                .map_err(|error| format!("relay 配置无效：{error}"))?;
        let transport = context
            .transport()
            .map_err(|error| format!("构造 relay 传输失败：{error}"))?;
        let config = V2ManagementConfig {
            vault_id: identity.vault_id,
            instance_id: identity.pinned_instance,
            device_id: identity.device_id,
            manager_pub: identity.pinned_manager,
        };
        let signing_key = DeviceSigningKey::from_seed(identity.ed25519_seed.clone());
        V2ManagementClient::new(transport, self.config.relay.clone(), config, signing_key)
            .with_store(self.vault.clone())
            .map_err(|error| format!("读取本地 V2 pin/缓存失败：{error}"))
    }

    fn load_draft_once(&mut self) -> Result<(), String> {
        if self.draft_loaded {
            return Ok(());
        }
        self.draft_loaded = true;
        match ApprovalDraftV1::load_draft(&self.vault) {
            Ok(draft) => {
                self.draft = draft;
                Ok(())
            }
            Err(error) => Err(format!("读取本地审批草稿失败：{error}")),
        }
    }

    fn build(
        &self,
        state: Option<&ControlStateV2>,
        replay: Option<&V2ControlReplay>,
        hints: &[RelayMemberHintV2],
        now: u64,
        message: Option<String>,
    ) -> ManagementSnapshot {
        build_snapshot(
            &self.config.relay,
            state,
            replay,
            hints,
            self.draft.as_ref(),
            self.local_pk(),
            now,
            self.snapshot.error.clone(),
            self.snapshot.notice.clone(),
            message,
        )
    }

    fn refresh(&mut self, notice: Option<String>) -> ManagementReport {
        self.snapshot.loading = false;
        self.snapshot.notice = notice;
        let result = (|| -> Result<ManagementSnapshot, String> {
            self.ensure_identity()?;
            self.load_draft_once()?;
            let mut client = self.client()?;
            client
                .refresh_control()
                .map_err(|error| format!("刷新 V2 管理状态失败：{error}"))?;
            let now = client.now_secs();
            let message = if client.is_v2() {
                None
            } else {
                Some(V2_DISABLED_MESSAGE.to_owned())
            };
            Ok(self.build(
                Some(client.state()),
                Some(client.replay()),
                client.relay_member_hints(),
                now,
                message,
            ))
        })();
        self.finish(result)
    }

    fn import_file(&mut self, path: &Path) -> ManagementReport {
        let result = (|| -> Result<ManagementSnapshot, String> {
            self.ensure_identity()?;
            let mut client = self.client()?;
            // 既有客户端入口：refresh_control + 严格解析（限长）+ verify_import。
            let draft = client
                .import_approval_file(path)
                .map_err(|error| format!("导入审批包文件失败：{error}"))?;
            self.install_imported(
                draft,
                &mut client,
                "已导入并本地验证审批包（未执行/未联网提交）",
            )
        })();
        self.finish(result)
    }

    fn import_base64(&mut self, text: &str) -> ManagementReport {
        let result = (|| -> Result<ManagementSnapshot, String> {
            self.ensure_identity()?;
            let mut client = self.client()?;
            let draft = client
                .import_approval_base64(text)
                .map_err(|error| format!("导入审批包文本失败：{error}"))?;
            self.install_imported(
                draft,
                &mut client,
                "已导入并本地验证审批包（未执行/未联网提交）",
            )
        })();
        self.finish(result)
    }

    fn install_imported(
        &mut self,
        draft: ApprovalDraftV1,
        client: &mut V2ManagementClient<StdTcpTransport>,
        notice: &'static str,
    ) -> Result<ManagementSnapshot, String> {
        draft
            .save_draft(&self.vault)
            .map_err(|error| format!("保存本地审批草稿失败：{error}"))?;
        let now = client.now_secs();
        self.draft = Some(draft);
        let mut snapshot = self.build(
            Some(client.state()),
            Some(client.replay()),
            client.relay_member_hints(),
            now,
            None,
        );
        snapshot.notice = Some(notice.to_owned());
        snapshot.error = None;
        Ok(snapshot)
    }

    fn export_file(&mut self, path: &Path) -> ManagementReport {
        let result = (|| -> Result<ManagementSnapshot, String> {
            let Some(draft) = self.draft.as_mut() else {
                return Err("当前没有本地审批草稿可导出".to_owned());
            };
            draft
                .write_file(path)
                .map_err(|error| format!("写审批包文件失败：{error}"))?;
            draft
                .save_draft(&self.vault)
                .map_err(|error| format!("保存本地审批草稿失败：{error}"))?;
            let now = self.now_secs_offline();
            let mut snapshot = build_snapshot(
                &self.config.relay,
                None,
                None,
                &[],
                self.draft.as_ref(),
                self.local_pk(),
                now,
                None,
                Some("已导出审批包文件（不自动执行/不联网）".to_owned()),
                None,
            );
            snapshot.v2 = self.snapshot.v2;
            snapshot.v2_message = self.snapshot.v2_message.clone();
            snapshot.control_rev = self.snapshot.control_rev;
            snapshot.head_hash = self.snapshot.head_hash;
            snapshot.authz_rev = self.snapshot.authz_rev;
            snapshot.authority_rev = self.snapshot.authority_rev;
            snapshot.authz_state = self.snapshot.authz_state;
            snapshot.policy = self.snapshot.policy;
            snapshot.caps = self.snapshot.caps.clone();
            snapshot.devices = self.snapshot.devices.clone();
            snapshot.pending_approvals = self.snapshot.pending_approvals;
            snapshot.last_refresh_unix = self.snapshot.last_refresh_unix;
            Ok(snapshot)
        })();
        self.finish(result)
    }

    fn export_base64(&mut self) -> ManagementReport {
        let mut artifact = None;
        let result = (|| -> Result<ManagementSnapshot, String> {
            let Some(draft) = self.draft.as_mut() else {
                return Err("当前没有本地审批草稿可导出".to_owned());
            };
            let text = draft
                .to_base64()
                .map_err(|error| format!("编码审批包文本失败：{error}"))?;
            draft.mark_handed_off();
            draft
                .save_draft(&self.vault)
                .map_err(|error| format!("保存本地审批草稿失败：{error}"))?;
            artifact = Some(ManagementArtifact::ExportedBase64(text));
            let now = self.now_secs_offline();
            let mut snapshot = build_snapshot(
                &self.config.relay,
                None,
                None,
                &[],
                self.draft.as_ref(),
                self.local_pk(),
                now,
                None,
                Some("已生成审批包复制文本（只写剪贴板；不联网）".to_owned()),
                None,
            );
            snapshot.v2 = self.snapshot.v2;
            snapshot.v2_message = self.snapshot.v2_message.clone();
            snapshot.control_rev = self.snapshot.control_rev;
            snapshot.head_hash = self.snapshot.head_hash;
            snapshot.authz_rev = self.snapshot.authz_rev;
            snapshot.authority_rev = self.snapshot.authority_rev;
            snapshot.authz_state = self.snapshot.authz_state;
            snapshot.policy = self.snapshot.policy;
            snapshot.caps = self.snapshot.caps.clone();
            snapshot.devices = self.snapshot.devices.clone();
            snapshot.pending_approvals = self.snapshot.pending_approvals;
            snapshot.last_refresh_unix = self.snapshot.last_refresh_unix;
            Ok(snapshot)
        })();
        let mut report = self.finish(result);
        if report.snapshot.error.is_none() {
            report.artifact = artifact;
        }
        report
    }

    fn sign(&mut self) -> ManagementReport {
        let result = (|| -> Result<ManagementSnapshot, String> {
            self.ensure_identity()?;
            let seed = self
                .identity
                .as_ref()
                .ok_or_else(|| "本机身份尚未解封".to_owned())?
                .ed25519_seed
                .clone();
            let mut client = self.client()?;
            client
                .refresh_control()
                .map_err(|error| format!("签署前刷新控制链失败：{error}"))?;
            let now = client.now_secs();
            let notice = {
                let Some(draft) = self.draft.as_mut() else {
                    return Err("当前没有本地审批草稿可签署".to_owned());
                };
                // 签前重验：链前进/到期/证据缺口一律拒绝（禁止自动改 rev/延 deadline）。
                draft
                    .verify_against(client.replay(), now)
                    .map_err(|error| format!("签署前重验失败（不会自动重签）：{error}"))?;
                draft
                    .sign_with(&DeviceSigningKey::from_seed(seed))
                    .map_err(|error| format!("签署失败：{error}"))?;
                draft
                    .save_draft(&self.vault)
                    .map_err(|error| format!("保存本地审批草稿失败：{error}"))?;
                match draft.state() {
                    ApprovalStateV1::Ready => "已签署：包已满签，可提交".to_owned(),
                    ApprovalStateV1::InitiatorSigned | ApprovalStateV1::AwaitingCosign => {
                        "已签署：等待对方加签（不持编辑/同步闸）".to_owned()
                    }
                    other => format!("已签署：{}", other.label()),
                }
            };
            let mut snapshot = self.build(
                Some(client.state()),
                Some(client.replay()),
                client.relay_member_hints(),
                now,
                None,
            );
            snapshot.notice = Some(notice);
            snapshot.error = None;
            Ok(snapshot)
        })();
        self.finish(result)
    }

    fn submit(&mut self) -> ManagementReport {
        let result = (|| -> Result<ManagementSnapshot, String> {
            self.ensure_identity()?;
            let mut client = self.client()?;
            let Some(draft) = self.draft.as_mut() else {
                return Err("当前没有本地审批草稿可提交".to_owned());
            };
            let submitted = client.submit_approval(draft);
            // 无论成功失败都持久化草稿（失败分类/回执都要能在重启后恢复）。
            draft
                .save_draft(&self.vault)
                .map_err(|error| format!("保存本地审批草稿失败：{error}"))?;
            let now = client.now_secs();
            let mut snapshot = self.build(
                Some(client.state()),
                Some(client.replay()),
                client.relay_member_hints(),
                now,
                None,
            );
            match submitted {
                Ok(receipt) => {
                    snapshot.notice = Some(format!(
                        "已提交并完成本地对账：control rev {} · head {}（重发原字节幂等）",
                        receipt.control_rev,
                        short_hash(&receipt.head_hash)
                    ));
                    snapshot.error = None;
                }
                Err(error) => {
                    // 失败分类 + 脱敏信息；不清库/不换 pin/不回退 creator。
                    snapshot.error = Some(format!(
                        "提交失败（{:?}{}）：{}",
                        error.kind,
                        if error.kind.is_retryable() {
                            "，可重发原字节"
                        } else {
                            ""
                        },
                        error.message
                    ));
                }
            }
            Ok(snapshot)
        })();
        self.finish(result)
    }

    fn cancel(&mut self) -> ManagementReport {
        let result = (|| -> Result<ManagementSnapshot, String> {
            ApprovalDraftV1::clear_draft(&self.vault)
                .map_err(|error| format!("清本地审批草稿失败：{error}"))?;
            self.draft = None;
            self.draft_loaded = true;
            let now = self.now_secs_offline();
            let mut snapshot = self.snapshot.clone();
            snapshot.approval = None;
            snapshot.pending_approvals = snapshot
                .devices
                .iter()
                .filter(|device| device.status == DeviceStatusView::Pending)
                .count();
            snapshot.notice = Some(
                "已取消协调：只清本地草稿；已外发的签名不能撤回（需要时新建 Operation 重签）"
                    .to_owned(),
            );
            snapshot.error = None;
            snapshot.last_refresh_unix = Some(now);
            Ok(snapshot)
        })();
        self.finish(result)
    }

    fn now_secs_offline(&self) -> u64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(0)
    }

    fn finish(&mut self, result: Result<ManagementSnapshot, String>) -> ManagementReport {
        let snapshot = match result {
            Ok(snapshot) => snapshot,
            Err(error) => {
                let mut snapshot = self.snapshot.clone();
                snapshot.loading = false;
                snapshot.error = Some(error);
                snapshot
            }
        };
        let mut snapshot = snapshot;
        snapshot.loading = false;
        snapshot.error = snapshot.error.map(|error| sanitize_management_text(&error));
        snapshot.notice = snapshot
            .notice
            .map(|notice| sanitize_management_text(&notice));
        self.snapshot = snapshot.clone();
        ManagementReport {
            snapshot,
            artifact: None,
        }
    }
}

impl ManagementRunner for RealManagementRunner {
    fn run(&mut self, command: ManagementCommand) -> ManagementReport {
        match command {
            ManagementCommand::Refresh => self.refresh(None),
            ManagementCommand::ImportFile(path) => self.import_file(&path),
            ManagementCommand::ImportBase64(text) => self.import_base64(&text),
            ManagementCommand::ExportFile(path) => self.export_file(&path),
            ManagementCommand::ExportBase64 => self.export_base64(),
            ManagementCommand::Sign => self.sign(),
            ManagementCommand::Submit => self.submit(),
            ManagementCommand::Cancel => self.cancel(),
            ManagementCommand::Stop => self.finish(Ok(self.snapshot.clone())),
        }
    }
}

// ---------------------------------------------------------------- 快照构造（真实/测试共用）

/// 把已验证状态 + 只读成员提示 + 本地草稿投影成快照。
///
/// `state`/`replay` 来自**已验证**回放（`None` = 还没刷新/离线导出）；`hints` 只用于
/// 展示 PENDING 设备，绝不写进授权事实。
#[allow(clippy::too_many_arguments)]
pub fn build_snapshot(
    relay: &str,
    state: Option<&ControlStateV2>,
    replay: Option<&V2ControlReplay>,
    hints: &[RelayMemberHintV2],
    draft: Option<&ApprovalDraftV1>,
    local_pk: Option<[u8; 32]>,
    now: u64,
    error: Option<String>,
    notice: Option<String>,
    v2_message: Option<String>,
) -> ManagementSnapshot {
    let mut snapshot = ManagementSnapshot {
        relay: relay.to_owned(),
        control_rev: state.map_or(0, ControlStateV2::rev),
        head_hash: state.map_or([0u8; 32], ControlStateV2::head_hash),
        authz_rev: state.map_or(0, ControlStateV2::authz_rev),
        authority_rev: state.map_or(0, ControlStateV2::authority_rev),
        authz_state: state.and_then(ControlStateV2::authz_state),
        policy: state
            .and_then(ControlStateV2::policy)
            .map(|policy| PolicyView {
                highrisk_threshold: policy.highrisk_threshold,
                min_quorum: policy.min_quorum,
                max_cap_ttl: policy.max_cap_ttl,
                max_invite_ttl: policy.max_invite_ttl,
            }),
        last_refresh_unix: Some(now),
        error: error.map(|error| sanitize_management_text(&error)),
        notice: notice.map(|notice| sanitize_management_text(&notice)),
        ..ManagementSnapshot::default()
    };
    let is_v2 = state.is_some_and(ControlStateV2::is_v2);
    snapshot.v2 = is_v2;
    snapshot.v2_message = if is_v2 {
        None
    } else {
        Some(v2_message.unwrap_or_else(|| {
            if state.is_some() {
                V2_DISABLED_MESSAGE.to_owned()
            } else {
                "尚未刷新（打开管理区后会自动拉取已验证控制链）".to_owned()
            }
        }))
    };

    if !is_v2 {
        // 未启用 V2：不展示任何伪造的设备/cap 成功状态。
        snapshot.devices = hints
            .iter()
            .filter(|hint| hint.status.eq_ignore_ascii_case("pending"))
            .map(|hint| DeviceView {
                device_id: hint.device_id,
                status: DeviceStatusView::Pending,
                cert_serial: None,
                data_rights: None,
                approved_rev: None,
                revoked_rev: None,
                cap_summary: None,
                note: Some("relay 投影；未启用 V2".to_owned()),
            })
            .collect();
        snapshot.approval = draft.map(|draft| approval_view(draft, local_pk, now, None));
        snapshot.pending_approvals = snapshot.devices.len()
            + usize::from(
                snapshot
                    .approval
                    .as_ref()
                    .is_some_and(|approval| !approval.is_terminal()),
            );
        return snapshot;
    }

    let Some(state) = state else {
        return snapshot;
    };

    snapshot.caps = state
        .caps()
        .map(|entry| CapView {
            cap_id: entry.cap.cap_id,
            subject_device_id: entry.cap.subject_device_id,
            scopes: entry.cap.scopes.bits(),
            not_before: entry.cap.not_before,
            expires_at: entry.cap.expires_at,
            revoked: entry.is_revoked(),
            effective: state.cap_is_effective_at(entry, now),
            granted_rev: entry.granted_rev,
        })
        .collect();

    // 每台设备当前的 cap 摘要（scope/到期；只来自已验证控制链）。
    let mut caps_by_device: std::collections::BTreeMap<[u8; 16], Vec<String>> =
        std::collections::BTreeMap::new();
    for entry in state.caps() {
        let cap = &entry.cap;
        let status = if entry.is_revoked() {
            "已撤销".to_owned()
        } else if state.cap_is_effective_at(entry, now) {
            format!(
                "{} 到期 {}",
                format_scopes(cap.scopes.bits()),
                format_unix_utc(cap.expires_at)
            )
        } else {
            format!("{}（此刻无效）", format_scopes(cap.scopes.bits()))
        };
        caps_by_device
            .entry(cap.subject_device_id)
            .or_default()
            .push(status);
    }

    // 链上设备（ACTIVE/REVOKED 的授权事实）+ relay 的 PENDING/未验证提示（只展示）。
    let mut devices: Vec<DeviceView> = state
        .devices()
        .map(|entry| DeviceView {
            device_id: entry.cert.device_id,
            status: match entry.status {
                menghunote_sync::management_v2::DeviceStatusV2::Active => DeviceStatusView::Active,
                menghunote_sync::management_v2::DeviceStatusV2::Revoked => {
                    DeviceStatusView::Revoked
                }
            },
            cert_serial: Some(entry.cert.cert_serial),
            data_rights: Some(entry.cert.data_rights.bits()),
            approved_rev: Some(entry.approved_rev),
            revoked_rev: entry.revoked_rev,
            cap_summary: caps_by_device
                .get(&entry.cert.device_id)
                .map(|caps| caps.join("；")),
            note: None,
        })
        .collect();
    let mut seen: Vec<[u8; 16]> = devices.iter().map(|device| device.device_id).collect();
    for hint in hints {
        if seen.contains(&hint.device_id) {
            continue;
        }
        seen.push(hint.device_id);
        let (status, note) = if hint.status.eq_ignore_ascii_case("pending") {
            (
                DeviceStatusView::Pending,
                "已兑换邀请、等待批准（不在链上）",
            )
        } else if hint.status.eq_ignore_ascii_case("revoked") {
            (
                DeviceStatusView::Revoked,
                "relay 投影撤销（链上无 V2 撤销记录；仅展示）",
            )
        } else {
            (
                DeviceStatusView::Unverified,
                "relay 报 active，但缺少已验证控制链事实（不可执行）",
            )
        };
        devices.push(DeviceView {
            device_id: hint.device_id,
            status,
            cert_serial: None,
            data_rights: None,
            approved_rev: None,
            revoked_rev: None,
            cap_summary: None,
            note: Some(note.to_owned()),
        });
    }
    devices.sort_by_key(|device| device.device_id);
    snapshot.devices = devices;

    let approval = draft.map(|draft| {
        let invalid = replay.and_then(|replay| {
            if matches!(draft.state(), ApprovalStateV1::Confirmed) {
                return None;
            }
            draft
                .verify_against(replay, now)
                .err()
                .map(|error| sanitize_management_text(&error.to_string()))
        });
        approval_view(draft, local_pk, now, invalid)
    });
    snapshot.pending_approvals = snapshot
        .devices
        .iter()
        .filter(|device| device.status == DeviceStatusView::Pending)
        .count()
        + usize::from(
            approval
                .as_ref()
                .is_some_and(|approval| !approval.is_terminal()),
        );
    snapshot.approval = approval;
    snapshot
}

/// 把一个审批草稿投影成 UI 只读视图（全部公开证据）。
#[must_use]
pub fn approval_view(
    draft: &ApprovalDraftV1,
    local_pk: Option<[u8; 32]>,
    now: u64,
    invalid_reason: Option<String>,
) -> ApprovalView {
    let envelope: &ApprovalEnvelopeV1 = draft.envelope();
    let operation = envelope.operation();
    let summary = envelope.operation_summary();
    let slots: Vec<SlotView> = envelope
        .slots()
        .into_iter()
        .enumerate()
        .map(|(index, slot): (usize, SignerSlotV1)| SlotView {
            index,
            role_label: slot.role_label(),
            device_id: slot.device_id,
            cap_id: slot.cap_id,
            key_fingerprint: key_fingerprint(&slot.pk),
            signed: envelope.signature_for(index as u32).is_some(),
        })
        .collect();
    let can_sign_local = local_pk.is_some_and(|pk| {
        let matching: Vec<usize> = operation
            .signers
            .iter()
            .enumerate()
            .filter(|(_, signer)| signer.pk == pk)
            .map(|(index, _)| index)
            .collect();
        if matching.len() != 1 {
            return false;
        }
        let index = matching[0];
        if envelope.signature_for(index as u32).is_some() {
            return false;
        }
        // 首签必须是发起者（A 先签，B 审核后加签）。
        if envelope.signatures().is_empty() && index != envelope.initiator_index() as usize {
            return false;
        }
        matches!(
            draft.state(),
            ApprovalStateV1::Draft
                | ApprovalStateV1::Frozen
                | ApprovalStateV1::InitiatorSigned
                | ApprovalStateV1::AwaitingCosign
        )
    });
    ApprovalView {
        state: draft.state(),
        state_label: draft.state().label(),
        kind: summary.kind,
        kind_label: summary.kind_label,
        request_id: operation.request_id,
        target: summary.target,
        scopes: summary.scopes,
        vault_id: operation.vault_id,
        instance_id: operation.instance_id,
        canonical_path: operation.canonical_path.clone(),
        expected_control_rev: operation.expected_control_rev,
        prev_control_hash: operation.prev_control_hash,
        authz_rev: operation.expected_authz_rev,
        authority_rev: operation.authority_rev,
        signed_at: operation.signed_at,
        not_after: operation.not_after,
        expires_at: summary.expires_at,
        fingerprint_128: format_fingerprint_128(&envelope.comparison_fingerprint()),
        fingerprint_256: format_fingerprint_256(&envelope.digest()),
        slots,
        signature_count: envelope.signatures().len(),
        signer_count: operation.signers.len(),
        initiator_index: envelope.initiator_index(),
        can_sign_local,
        fully_signed: envelope.is_fully_signed(),
        handed_off: draft.is_handed_off(),
        receipt: draft.receipt().map(|receipt| ReceiptView {
            control_rev: receipt.control_rev,
            head_hash: receipt.head_hash,
            kind: receipt.kind,
            authz_rev: receipt.authz_rev,
            authority_rev: receipt.authority_rev,
        }),
        failure: draft.failure().map(|failure| {
            (
                failure_kind_label(failure.kind).to_owned(),
                sanitize_management_text(&failure.message),
            )
        }),
        expired: now > operation.not_after,
        invalid_reason: invalid_reason.map(|reason| sanitize_management_text(&reason)),
    }
}

/// 失败分类的中文标签（只用于展示）。
#[must_use]
pub fn failure_kind_label(kind: ApprovalFailureV1) -> &'static str {
    match kind {
        ApprovalFailureV1::Transport => "传输失败（可重发原字节）",
        ApprovalFailureV1::RateLimited => "限流（可稍后重发）",
        ApprovalFailureV1::Server => "relay 5xx（可稍后重发）",
        ApprovalFailureV1::Forbidden => "403 拒绝（需重建 Operation）",
        ApprovalFailureV1::NotFound => "404 目标不存在",
        ApprovalFailureV1::Stale => "409 CAS/链陈旧（必须新建 Operation）",
        ApprovalFailureV1::Conflict => "409 冲突",
        ApprovalFailureV1::Expired => "410 已过期（必须重签）",
        ApprovalFailureV1::Invalid => "400 结构/签名非法",
        ApprovalFailureV1::NotReady => "本地未就绪",
        ApprovalFailureV1::Signature => "本地签名校验失败",
        ApprovalFailureV1::Mismatch => "本地证据不符",
    }
}

// ---------------------------------------------------------------- 单测

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_formatting_is_grouped_and_stable() {
        let short = [0xABu8; 16];
        let full = [0xCDu8; 32];
        assert_eq!(
            format_fingerprint_128(&short),
            "abababab abababab abababab abababab"
        );
        assert_eq!(
            format_fingerprint_256(&full),
            "cdcdcdcd cdcdcdcd cdcdcdcd cdcdcdcd cdcdcdcd cdcdcdcd cdcdcdcd cdcdcdcd"
        );
    }

    #[test]
    fn unix_formatter_matches_known_dates() {
        assert_eq!(format_unix_utc(0), "1970-01-01 00:00 UTC");
        assert_eq!(format_unix_utc(1_700_000_000), "2023-11-14 22:13 UTC");
        assert_eq!(format_unix_utc(1_800_000_000), "2027-01-15 08:00 UTC");
    }

    #[test]
    fn scopes_and_rights_formatting_is_explicit() {
        assert_eq!(format_scopes(1 | 2), "邀请/批准");
        assert_eq!(format_scopes(0), "—");
        assert_eq!(format_data_rights(1), "只读");
        assert_eq!(format_data_rights(3), "读写");
        assert_eq!(format_data_rights(0), "无数据权限");
    }

    #[test]
    fn error_summary_redacts_urls_paths_codes_and_long_tokens() {
        let base64 = "A".repeat(200);
        let raw = format!(
            "失败：读取 /home/dev/instances/inst-a/vault.sqlite3 与 C:\\Users\\dev\\vault.db；\
             relay https://relay.example:8443/v2 ；恢复码 mnrec1abcdef0123456789 ；\
             审批包 mn-approval-v1:{base64}"
        );
        let text = sanitize_management_text(&raw);
        assert!(!text.contains("/home/dev"), "绝对路径必须隐藏：{text}");
        assert!(!text.contains("C:\\Users"), "Windows 路径必须隐藏：{text}");
        assert!(
            !text.contains("https://relay.example"),
            "URL 必须隐藏：{text}"
        );
        assert!(!text.contains("mnrec1abcdef"), "恢复码必须隐藏：{text}");
        assert!(!text.contains(&base64), "长 base64 token 必须隐藏：{text}");
        assert!(text.contains("<路径已隐藏>"), "{text}");
        assert!(text.contains("<地址已隐藏>"), "{text}");
        assert!(text.contains("<恢复码已隐藏>"), "{text}");
        assert!(text.contains("<长文本已隐藏>"), "{text}");
    }

    #[test]
    fn error_summary_keeps_harmless_text_and_truncates() {
        let text = sanitize_management_text("链已前进：必须新建 Operation 并全部重签");
        assert_eq!(text, "链已前进：必须新建 Operation 并全部重签");
        let long = "错".repeat(MAX_MANAGEMENT_ERROR_CHARS + 30);
        let text = sanitize_management_text(&long);
        assert!(text.ends_with("…（已截断）"));
        assert!(text.chars().count() <= MAX_MANAGEMENT_ERROR_CHARS + 8);
    }
}
