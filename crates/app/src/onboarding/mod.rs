//! 同步初始化/配置的 **UI 向导与设置**（ADR 0011；本文件对应切片 S1 + S2 + S3 + S4）。
//!
//! ## 这一层解决什么
//!
//! 在 ADR 0011 之前，真实用户要完成"创建/加入同步实例"必须：设 `MENGHUNOTE_DEV_VAULT`、
//! 设 `--dev-sync`、用 `--dev-sync-create/join` 传 secret/恢复码、再从 stdout 抄恢复码……
//! 这不是产品，只是自动化。本模块把这条路径搬进窗口：
//!
//! ```text
//! 启动（feature on、无显式 CLI 模式）
//!   ├─ 已有有效公开配置 + 身份 → 自动打开实例 → Main（不必先闪 Welcome）
//!   └─ 没有 → Welcome
//!        ├─ 创建新的同步实例 → CreateForm → Working → BackupRequired → Main
//!        ├─ 加入已有同步实例 → JoinForm  → Working → Main（成功即写配置，无备份页）
//!        ├─ 打开已保存实例 → Main
//!        └─ 继续本地（默认 dev vault，明确 DEV 标注）→ Main（无同步面板）
//! ```
//!
//! ## 各文件职责（互不越界）
//!
//! | 文件 | 职责 | 允许依赖 |
//! | --- | --- | --- |
//! | [`layout`] | 公开目录/配置：XDG/LOCALAPPDATA 解析、`dev-sync.json` 原子读写 | `serde`/`serde_json` + std |
//! | [`lock`] | 每实例目录的进程级独占锁（锁文件 + `create_new`，MVP） | std |
//! | [`create`] | Create 后台线程：一次性创建票据 → v2 创建 → 身份落盘 → channel 交回 | `menghunote-sync` + `DevIdentityStore` |
//! | [`invite_copy`] | 备份页「复制邀请链接」后台线程：读身份 → 生成 v2 邀请（token 只在内存） | 同上 |
//! | [`join`] | Join 后台线程：恢复码自批/bootstrap → 身份落盘 → channel 交回（含失败分类） | 同上 |
//! | [`input`] | 自绘输入控件（`EntityInputHandler`；秘密字段遮罩 + 允许粘贴） | GPUI + `sidebar_rename` |
//! | [`settings`] | 设置面板状态 + relay 编辑校验（与运行时门禁同一套策略） | `gate`/`service` + `layout` + `input` |
//! | [`app_root`] | 常驻根视图：状态机 + 渲染 + 关窗分派 + Create/Join 事件泵 + 设置覆盖层 | 上面六者 + `AppShell` |
//!
//! ## 安全边界（ADR 0011「表单、恢复码与敏感输入」）
//!
//! - 创建票据 / 恢复码只在内存里：**配置结构体里没有这些字段**（类型上写不进去）；
//!   App 不再收集 setup secret 或 relay 管理凭证（ADR 0018 S4b）；
//! - Create 成功且身份**受保护落盘**之后，才在备份页展示 `vault_id` + 恢复码；
//! - 恢复码只在备份页显示一次；离开备份页（进入 Main / 返回）即 drop（`Zeroizing`）；
//!   Join 的恢复码是用户自己输入的，提交后立刻从输入框清空、只在 worker 内存里存在；
//! - 错误文案统一走 [`sanitize_error`]：替换本次已知秘密、遮住恢复码形态的 token、
//!   截断超长错误；**不使用** `{error:#}` 的原始输出直接显示；
//! - 日志只打公开路径/模式，不打印任何秘密。
//!
//! ## 已知缺口（S3 记录）
//!
//! - 秘密输入框是**简化遮罩**：渲染成 `•`、不向平台报告正文、禁复制/剪切，但允许
//!   粘贴（`Ctrl+V` / `Shift+Insert`）；秘密字段里的 **IME 组合提交**仍不完善（setup
//!   secret/恢复码的真实路径是 ASCII 键入或粘贴）。完整 `EntityInputHandler` 秘密输入
//!   留给后续切片。
//! - 打开已有实例是**同步**的（锁 + 身份解封 + 打开库）：本地 IO 很快，但大库迁移会
//!   短暂阻塞窗口；真正的后台 Opening 留给产品化。
//! - 实例目录锁是"锁文件 + `create_new`"的 MVP（见 [`lock`] 的局限说明）。
//! - `local_dev` 的 `DevLocal::create/join` 自建 transport，`ServiceConfig` 的超时/预算
//!   不会传进去（ADR 0011 已记录：逻辑超时 ≠ 停止，终态前禁止重试）；`remote_tls`
//!   （S3b）用 `RelayContext::remote_tls` + production TLS，初始化的传输预算同样是
//!   历史默认（30s/30s）。
//! - `remote_tls` 初始化**未**在真实鉴权 relay / Windows 真机上验证（ADR 0013 S3b）；
//!   本地 TLS harness 通过显式注入自签根（只加信任根，不关闭证书校验）。

pub mod app_root;
pub mod create;
pub mod fingerprint;
pub mod input;
pub mod invite;
pub mod invite_copy;
pub mod join;
pub mod layout;
pub mod lock;
pub mod settings;

pub use app_root::{
    AppRoot, AppRootOptions, FormCreateMode, RootStateLabel, DEFAULT_OPERATION_BUDGET,
};
pub use create::{
    CreateEvent, CreateJob, CreateOutcome, CreateRequest, CreateStage, CREATE_PUMP_INTERVAL,
};
pub use fingerprint::{
    load as load_pin_fingerprint, BootstrapFingerprint, FingerprintState, PinFingerprint,
    FINGERPRINT_NOTICE, NOT_TRUSTED,
};
pub use input::{FormInput, FormInputEvent};
pub use invite::{
    InviteCheckRequest, InviteEvent, InviteFailureKind, InviteJob, InviteOutcome,
    InvitePendingFacts, InviteRedeemRequest, InviteStage, INVITE_EMPTY_HINT,
};
pub use invite_copy::{
    CopyInviteEvent, CopyInviteJob, CopyInviteRequest, CopyInviteStage, InviteCopyFailure,
};
pub use join::{JoinEvent, JoinFailureKind, JoinJob, JoinOutcome, JoinRequest, JoinStage};
pub use layout::{
    generate_instance_id, validate_instance_id, DevSyncProfile, InstanceEntry, ProfileError,
    ProfileLayout, ProfileStore, RelayApiVersion, RelayMode, PROFILE_VERSION, PROFILE_VERSION_V1,
    VAULT_FILE_NAME,
};
pub use lock::{InstanceLock, LockError, LOCK_FILE_NAME};

/// 错误文案的最大字符数（超长 SQL/传输错误截断；UI 不显示整段日志）。
pub const MAX_ERROR_CHARS: usize = 800;

/// 把底层错误转成**可展示的脱敏文案**。
///
/// 规则（ADR 0011「错误按类型脱敏，禁止直接显示 `{error:#}`」）：
///
/// 1. `{error:#}` 只作为**输入**：先整段替换调用方给出的秘密（如旧 CLI 的 setup secret；
///    Create 的链接票据由 `CreateTicket` 自身脱敏、不进入错误文案）；
/// 2. 恢复码形态的 token（`mnrec1…`）整段替换成 `<恢复码已隐藏>`；
/// 3. 截断到 [`MAX_ERROR_CHARS`] 个字符（按 char 边界，不切坏 UTF-8）。
///
/// `secrets` 里空串会被忽略（否则 `replace("")` 会在每个字符间塞入替换文本）。
#[must_use]
pub fn sanitize_error(error: &anyhow::Error, secrets: &[&str]) -> String {
    let mut text = format!("{error:#}");
    for secret in secrets {
        if !secret.is_empty() && text.contains(secret) {
            text = text.replace(secret, "<已隐藏>");
        }
    }
    text = redact_recovery_codes(&text);
    if text.chars().count() > MAX_ERROR_CHARS {
        text = text.chars().take(MAX_ERROR_CHARS).collect::<String>() + "…（已截断）";
    }
    text
}

/// 遮住文本里所有恢复码形态的 token（`mnrec1` 开头的连续 ASCII 字母数字/`-`/`_`）。
#[must_use]
pub fn redact_recovery_codes(text: &str) -> String {
    const PREFIX: &str = "mnrec1";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(index) = rest.find(PREFIX) {
        out.push_str(&rest[..index]);
        let token = &rest[index..];
        let end = token
            .char_indices()
            .find(|(_, c)| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_')))
            .map_or(token.len(), |(offset, _)| offset);
        out.push_str("<恢复码已隐藏>");
        rest = &token[end..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_secret_never_survives_sanitizing() {
        let error = anyhow::anyhow!("relay 拒绝 secret=SUPERSECRET 的请求");
        let text = sanitize_error(&error, &["SUPERSECRET"]);
        assert!(!text.contains("SUPERSECRET"), "秘密必须被替换：{text}");
        assert!(text.contains("<已隐藏>"), "要留下可见的替换标记：{text}");
    }

    #[test]
    fn recovery_code_tokens_are_redacted_even_without_knowing_them() {
        let error = anyhow::anyhow!(
            "bad code mnrec1abcdef0123456789 at /home/dev/instances/inst-a/vault.sqlite3"
        );
        let text = sanitize_error(&error, &[]);
        assert!(!text.contains("mnrec1abcdef"), "{text}");
        assert!(text.contains("<恢复码已隐藏>"), "{text}");
        assert!(
            text.contains("/home/dev/instances/inst-a/vault.sqlite3"),
            "公开路径应保留（诊断需要）：{text}"
        );
    }

    #[test]
    fn empty_secrets_do_not_corrupt_the_message() {
        let error = anyhow::anyhow!("普通错误");
        assert_eq!(sanitize_error(&error, &["", ""]), "普通错误");
    }

    #[test]
    fn long_errors_are_truncated_on_char_boundaries() {
        let long = "错".repeat(MAX_ERROR_CHARS + 50);
        let error = anyhow::anyhow!(long);
        let text = sanitize_error(&error, &[]);
        assert!(
            text.chars().count() <= MAX_ERROR_CHARS + 8,
            "截断后仍然过长"
        );
        assert!(text.ends_with("…（已截断）"), "{text}");
    }
}
