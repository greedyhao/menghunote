//! 常驻根视图 `AppRoot`（ADR 0011 决策 B′：同窗常驻根，根视图切换）。
//!
//! ## 状态机
//!
//! ```text
//! Welcome -> CreateForm -> Working -> BackupRequired -> Main
//!         \-> JoinForm  -> Working -> Main（成功即写配置，无备份页）
//!                      \-> Error（保留表单）        （保存配置失败 -> Error，可重试保存）
//! 已有有效配置 -> Main（启动直接进入，不闪 Welcome）
//! Welcome -> Main（继续本地 dev vault，明确 DEV 标注）
//! 任意 -> Error（配置损坏 / 身份缺失 / 库不匹配 / 实例目录被占用）
//! Welcome/Main -> 设置（覆盖层，不销毁 Main 的 AppShell）-> 关闭回原状态
//! ```
//!
//! **只有真实 `Vault` + 持久身份就绪后才构造 `AppShell`**：没有占位库、假 peer、
//! "先开空壳再补身份"（ADR 0011 决策）。Main 状态里 `AppShell` 是 `AppRoot` 的子视图，
//! 面板/持闸/发布按钮全部沿用 ADR 0009 的既有实现，本文件不重复造。
//!
//! Create 与 Join 共用同一套骨架：后台 worker（[`super::create`] / [`super::join`]）+
//! 非阻塞事件泵 + 终态事件；差别只有 Create 多一个"备份恢复码"确认页。
//!
//! ## 关窗分派
//!
//! | 状态 | 行为 |
//! | --- | --- |
//! | `Working`（Create/Join） | **拦截**（返回 `false`）并提示"等真实收尾"；迟到的成功仍必须展示恢复码 / 进入 Main |
//! | `Main` | 调 `AppShell::close_all`（有界排空）后允许关闭 |
//! | 其它（Welcome/表单/备份页/Error） | 直接允许关闭（无未落盘的库写入） |
//!
//! ## 固定 selector（headless 测试契约）
//!
//! | selector | 元素 |
//! | --- | --- |
//! | `app-root` | 根视图（铺满窗口） |
//! | `app-root-welcome` / `app-root-create` / `app-root-join` / `app-root-local` | Welcome 页与三个入口 |
//! | `app-root-instances` / `app-root-instance-<i>` | 已保存实例列表与第 i 行（active 优先） |
//! | `app-root-create-form` / `app-root-relay-input` / `app-root-secret-input` / `app-root-dir-input` | Create 表单与三个字段 |
//! | `app-root-create-link` | Welcome 的「粘贴创建链接（v2 票据）」入口 |
//! | `app-root-create-link-input` / `app-root-create-link-apply` / `app-root-create-link-clear` | Create 表单的创建链接字段 /「应用」/「移除链接」 |
//! | `app-root-create-link-warning` / `app-root-create-link-facts` | 票据模式的一次性警告 / 预填的 relay+vault_id+API 面 |
//! | `app-root-join-form` / `app-root-join-relay-input` / `app-root-vault-id-input` / `app-root-recovery-input` / `app-root-join-dir-input` | Join 表单与四个字段 |
//! | `app-root-mode-local_dev` / `app-root-mode-remote_tls` / `app-root-remote-consent` | Create 表单的 relay 模式/远程确认 |
//! | `app-root-consent` / `app-root-submit` / `app-root-back` / `app-root-form-error` | Create 表单的同意框/提交/返回/错误行 |
//! | `app-root-join-submit` / `app-root-join-back` / `app-root-join-error` | Join 表单的提交/返回/错误行 |
//! | `app-root-working` / `app-root-progress` / `app-root-cancel` | Working 页（Create/Join 共用） |
//! | `app-root-backup` / `app-root-vault-id` / `app-root-recovery-code` / `app-root-copy-code` / `app-root-backup-confirm` / `app-root-continue` | 备份页 |
//! | `app-root-main` | Main（`AppShell` 子视图宿主） |
//! | `app-root-settings` | 打开设置（Welcome 页与 Main 右下角各一个入口） |
//! | `dev-sync-settings` | 设置覆盖层根（打开时存在；Main 仍在其下保留） |
//! | `settings-instance-<i>` / `settings-instance-<i>-active` | 公开实例行 /「设为默认」控件（active 优先） |
//! | `settings-relay` / `settings-save` / `settings-cancel` | relay 输入框 / 保存 / 关闭（不保存） |
//! | `settings-mode-local_dev` / `settings-mode-remote_tls` / `settings-remote-consent` | relay 模式选择 / 远程确认 |
//! | `settings-fingerprint` / `settings-fingerprint-headline` / `settings-copy-fingerprint` | 安全指纹只读区 / 标题 / 复制公开值 |
//! | `settings-security-category` | 当前安全暂停类别（无则「无安全暂停」） |
//! | `settings-error` / `settings-notice` | 内联错误 / 提示（含「重启后生效」） |
//! | `settings-copy-vault-id` / `settings-copy-device-id` / `settings-copy-path` | 公开值复制按钮 |
//! | `settings-api-version` | active 实例的 API 面（v1 setup secret / v2 票据） |
//! | `app-root-error` / `app-root-error-message` / `app-root-retry` / `app-root-back-welcome` | Error 页 |
//! | `app-root-notice` | 底部提示行 |
//!
//! selector 只是测试元数据，业务逻辑**不得**依赖它们。

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::bail;
use gpui::prelude::*;
use gpui::{
    div, px, App, ClipboardItem, Context, Entity, IntoElement, MouseButton, Render, SharedString,
    Window,
};
use zeroize::Zeroizing;

use menghunote_storage::{SyncContext, Vault};
use menghunote_sync::create_link::{CreateLink, CreateTicket};
use menghunote_sync::http_relay::Clock as _;

use crate::dev_sync::gate::DEFAULT_RELAY;
use crate::dev_sync::service::ServiceConfig;
use crate::dev_sync::{DevIdentityStore, DevSyncConfig};
use crate::dev_vault;
use crate::editor::theme;
use crate::shell::AppShell;

use super::create::{
    CreateCredential, CreateEvent, CreateJob, CreateOutcome, CreateRequest, CREATE_PUMP_INTERVAL,
};
use super::fingerprint::{FingerprintState, FINGERPRINT_NOTICE};
use super::input::{FormInput, FormInputEvent};
use super::invite::{
    InviteCheckRequest, InviteEvent, InviteJob, InviteOutcome, InvitePendingFacts,
    InviteRedeemRequest, INVITE_EMPTY_HINT,
};
use super::join::{JoinEvent, JoinJob, JoinOutcome, JoinRequest};
use super::layout::{
    generate_instance_id, DevSyncProfile, InstanceEntry, ProfileError, ProfileLayout, ProfileStore,
    RelayApiVersion, RelayMode, VAULT_FILE_NAME,
};
use super::lock::{InstanceLock, LOCK_FILE_NAME};
use super::sanitize_error;
use super::settings::{
    remote_option_available, short_public_id, validate_relay_edit, SettingsState,
};

/// 操作预算（ADR 0011：沿用 120s；逻辑超时 ≠ 停止）。
pub const DEFAULT_OPERATION_BUDGET: Duration = Duration::from_secs(120);

/// `AppRoot` 的构造参数（测试注入目录/relay/instance id）。
#[derive(Debug, Clone)]
pub struct AppRootOptions {
    /// 公开配置与实例目录布局。
    pub layout: ProfileLayout,
    /// Create 表单里 relay 的默认值（生产 = [`DEFAULT_RELAY`]；测试注入进程内 dev relay）。
    pub default_relay: String,
    /// 测试注入固定实例 id；`None` = 每次进入表单自动生成。
    pub instance_id: Option<String>,
    /// 是否提供「继续本地（默认 dev vault）」入口。
    pub allow_local_vault: bool,
    /// 操作预算（Create 的等待上限；测试可缩短）。
    pub operation_budget: Duration,
}

impl AppRootOptions {
    /// 按平台默认目录构造（生产入口）。
    pub fn discover(env: impl Fn(&str) -> Option<String>) -> Result<Self, ProfileError> {
        Ok(Self {
            layout: ProfileLayout::discover(env)?,
            default_relay: DEFAULT_RELAY.to_owned(),
            instance_id: None,
            allow_local_vault: true,
            operation_budget: DEFAULT_OPERATION_BUDGET,
        })
    }

    /// 测试用：显式目录 + relay + 固定实例 id。
    #[must_use]
    pub fn for_test(layout: ProfileLayout, relay: impl Into<String>, instance_id: &str) -> Self {
        Self {
            layout,
            default_relay: relay.into(),
            instance_id: Some(instance_id.to_owned()),
            allow_local_vault: false,
            operation_budget: DEFAULT_OPERATION_BUDGET,
        }
    }
}

/// 状态的**只读标签**（测试/日志用；不含任何秘密）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootStateLabel {
    /// Welcome。
    Welcome,
    /// Create 表单。
    CreateForm,
    /// Join 表单。
    JoinForm,
    /// 邀请加入：已兑换为 PENDING，等创建者批准（ADR 0014 S1）。
    InvitePending,
    /// 后台创建/加入中。
    Working,
    /// 等待用户确认已备份恢复码。
    BackupRequired,
    /// 正在打开实例（同步路径里是瞬态）。
    Opening,
    /// 主界面（真实 `AppShell`）。
    Main,
    /// 错误页。
    Error,
}

/// 内部状态（表单/备份数据放在独立字段里，状态本身保持轻量）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Welcome,
    CreateForm,
    JoinForm,
    InvitePending,
    Working,
    BackupRequired,
    Opening,
    Main,
    Error,
}

/// Error 页的"下一步"。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetryKind {
    /// 重新读取公开配置（配置损坏/版本不符）。
    ReloadConfig,
    /// 重试打开已保存实例。
    OpenInstance,
    /// 重试保存配置（**不得**重跑 Create/Join）。
    SaveProfile,
    /// 回到 Create 表单重试（保留公开字段，secret 需重输）。
    CreateForm,
    /// 只能回 Welcome。
    None,
}

impl RetryKind {
    const fn label(self) -> &'static str {
        match self {
            Self::ReloadConfig => "重新读取配置",
            Self::OpenInstance => "重试打开",
            Self::SaveProfile => "重试保存",
            Self::CreateForm => "返回表单重试",
            Self::None => "返回欢迎页",
        }
    }
}

/// Error 页内容（公开字段 + 脱敏文案）。
struct ErrorState {
    title: String,
    message: String,
    public: Vec<(String, String)>,
    retry: RetryKind,
}

/// Create 表单状态（输入框实体 + 模式 + 同意勾选）。
struct FormState {
    instance_id: String,
    /// 本次创建要写的 relay 模式（默认 `local_dev`；票据模式由创建链接决定）。
    mode: RelayMode,
    /// `remote_tls` 的显式确认（"远程服务器/HTTPS"；ADR 0013 §1）。
    remote_consent: bool,
    relay: Entity<FormInput>,
    secret: Entity<FormInput>,
    /// 创建链接输入（**秘密**：含一次性票据；遮罩 + 禁复制 + 允许粘贴）。
    link: Entity<FormInput>,
    /// 已应用的创建链接（票据只在内存；提交时移动到 worker，不落盘）。
    link_state: Option<CreateLinkState>,
    dir: Entity<FormInput>,
    consent: bool,
    error: Option<String>,
    /// Enter 提交的订阅（随表单一起 drop）。
    _subs: Vec<gpui::Subscription>,
}

/// 已应用的 `mn-pair://create` 链接（**票据只在内存**；不实现 `Debug`/`Clone`）。
///
/// 只保留 worker/UI 需要的公开定位信息 + 票据本体；完整 URL（fragment 含票据）在
/// 应用后立刻丢弃，避免任何后续代码路径把它打印/持久化。
struct CreateLinkState {
    /// 链接 scheme 决定的模式（http-loopback / https）。
    mode: RelayMode,
    /// canonical relay origin。
    relay: String,
    /// 票据绑定的预留 vault id。
    vault_id: [u8; 16],
    /// 32 位 hex（公开展示）。
    vault_id_hex: String,
    /// 一次性票据明文（`Drop` 零化）。
    ticket: CreateTicket,
    /// 可选公开锚（relay instance）；创建回执必须一致（不自动换 pin）。
    pin: Option<[u8; 16]>,
}

/// Join 表单状态（四个输入框；**不实现 `Debug`**：恢复码就在子实体里）。
struct JoinFormState {
    instance_id: String,
    /// 本次加入要写的 relay 模式（默认 `local_dev`；`remote_tls` 初始化留 S3b）。
    mode: RelayMode,
    /// `remote_tls` 的显式确认（"远程服务器/HTTPS"；ADR 0013 §1）。
    remote_consent: bool,
    relay: Entity<FormInput>,
    vault_id: Entity<FormInput>,
    /// 恢复码（秘密；遮罩 + 禁复制）。
    recovery_code: Entity<FormInput>,
    /// 邀请链接（**可选**；含一次性 token，遮罩 + 禁复制 + 允许粘贴）。
    ///
    /// 非空时走邀请加入（不需要恢复码）：链接自带 relay/vault/iid/pin。
    invite: Entity<FormInput>,
    dir: Entity<FormInput>,
    error: Option<String>,
    _subs: Vec<gpui::Subscription>,
}

/// Working 页正在做哪种后台操作（只影响文案，不影响状态机）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkingKind {
    Create,
    Join,
    /// 邀请加入：兑换 PENDING 或检查批准状态（ADR 0014 S1）。
    Invite,
}

impl WorkingKind {
    const fn title(self) -> &'static str {
        match self {
            Self::Create => "正在创建同步实例…",
            Self::Join => "正在加入同步实例…",
            Self::Invite => "正在处理邀请加入…",
        }
    }

    const fn hint(self) -> &'static str {
        match self {
            Self::Create => {
                "genesis/身份落盘都在专用线程；UI 不冻结。关窗会被拦截，\
                 等真实收尾后成功仍会展示恢复码。"
            }
            Self::Join => {
                "恢复码自批/bootstrap/身份落盘都在专用线程；UI 不冻结。\
                 关窗会被拦截，等真实收尾（成功直接进入主界面）。"
            }
            Self::Invite => {
                "邀请兑换/检查都在专用线程；兑换只产生待批准设备（PENDING），\
                 不会自动获得数据访问权。关窗会被拦截，等真实收尾。"
            }
        }
    }
}

/// 后台操作的进度。
#[derive(Debug, Clone)]
struct WorkingState {
    kind: WorkingKind,
    /// 当前阶段的固定文案（不含秘密）。
    stage: Option<&'static str>,
    cancel_requested: bool,
    /// 本次操作的开始时刻（用于"逻辑超出 120s 预算"的提示，**不**中断 worker）。
    started: Instant,
    /// 预算提示是否已经给过（只给一次，不刷屏）。
    budget_notice_shown: bool,
}

/// Create 成功、等备份确认的数据（含一次性恢复码）。
struct PendingCreate {
    outcome: Box<CreateOutcome>,
    entry: InstanceEntry,
    confirmed: bool,
    copied: bool,
}

/// Join 成功、但公开配置还没写进去的数据（库/身份已落盘）。
///
/// 只在"配置写失败 → Error 页 → 重试保存"期间存在；**重试只重写配置**，不再 Join。
struct PendingJoin {
    outcome: Box<JoinOutcome>,
    entry: InstanceEntry,
    /// 成功是否发生在用户已请求取消之后（决定进入 Main 时的提示）。
    cancelled: bool,
}

/// 邀请加入的 PENDING 等待页状态（ADR 0014 S1）。
struct InvitePendingState {
    /// 本次加入的实例 id（写公开配置用）。
    instance_id: String,
    /// 公开事实（relay/vault/设备/指纹；不含 token）。
    facts: InvitePendingFacts,
    /// 实例目录锁（检查时移交 worker；失败后被 drop 则重新获取）。
    lock: Option<InstanceLock>,
    /// 是否有检查任务在途（禁用按钮，不并发）。
    busy: bool,
    /// 最近一次检查结果/错误（脱敏文案）。
    message: Option<String>,
}

/// 常驻根视图。
pub struct AppRoot {
    options: AppRootOptions,
    store: ProfileStore,
    profile: Option<DevSyncProfile>,
    state: State,
    form: Option<FormState>,
    join_form: Option<JoinFormState>,
    working: Option<WorkingState>,
    pending: Option<PendingCreate>,
    pending_join: Option<PendingJoin>,
    job: Option<CreateJob>,
    join_job: Option<JoinJob>,
    /// 邀请加入的后台任务（兑换或检查；二者不同时存在）。
    invite_job: Option<InviteJob>,
    /// 邀请 PENDING 等待页状态（`None` = 不在等待页）。
    invite: Option<InvitePendingState>,
    /// bootstrap 已完成、但公开配置写失败：只重试保存（不重新联网/不重新 bootstrap）。
    pending_invite: Option<(Box<InviteOutcome>, String)>,
    /// 设置面板（overlay；Main/Welcome 都可用，关闭后回到原状态）。
    settings: Option<SettingsState>,
    /// 关闭设置后要把焦点还给 Main 的编辑器（下一帧渲染时消费）。
    pending_focus_restore: bool,
    /// 进入「粘贴创建链接」表单后，下一帧把焦点给创建链接输入框。
    focus_create_link: bool,
    main: Option<Entity<AppShell>>,
    lock: Option<InstanceLock>,
    instance: Option<InstanceEntry>,
    retry_entry: Option<InstanceEntry>,
    error: Option<ErrorState>,
    notice: Option<String>,
    pending_focus: bool,
    pump_started: bool,
    /// **测试专用**：remote_tls 初始化注入的显式信任根（DER；本地 TLS harness）。
    ///
    /// 生产恒为空（系统信任根）。只增加显式信任，不关闭证书链/SAN/有效期校验。
    test_relay_roots: Vec<Vec<u8>>,
}

impl AppRoot {
    /// 构造根视图并尝试自动打开已保存实例。
    pub fn new(options: AppRootOptions, cx: &mut Context<Self>) -> Self {
        let store = options.layout.store();
        let mut root = Self {
            options,
            store,
            profile: None,
            state: State::Welcome,
            form: None,
            join_form: None,
            working: None,
            pending: None,
            pending_join: None,
            job: None,
            join_job: None,
            invite_job: None,
            invite: None,
            pending_invite: None,
            settings: None,
            pending_focus_restore: false,
            focus_create_link: false,
            main: None,
            lock: None,
            instance: None,
            retry_entry: None,
            error: None,
            notice: None,
            pending_focus: false,
            pump_started: false,
            test_relay_roots: Vec::new(),
        };
        root.boot(cx);
        root
    }

    /// 注册关窗钩子（main 与 headless 测试共用同一处分派）。
    pub fn install_close_handler(window: &Window, entity: Entity<Self>, cx: &mut App) {
        // 用弱引用：回调不能把根视图钉住（否则实体永远不析构，实例锁也不会释放）。
        let weak = entity.downgrade();
        window.on_window_should_close(cx, move |_window, cx| {
            weak.update(cx, |root, cx| root.should_close(cx))
                .unwrap_or(true)
        });
    }

    /// 启动引导：读公开配置 → 有 active 且有效就自动进 Main。
    fn boot(&mut self, cx: &mut Context<Self>) {
        self.state = State::Welcome;
        self.error = None;
        match self.store.load() {
            Err(error) => {
                let path = self.store.path().display().to_string();
                self.set_error(
                    "同步配置不可用",
                    error.to_string(),
                    vec![("配置文件".to_owned(), path)],
                    RetryKind::ReloadConfig,
                );
            }
            Ok(None) => {}
            Ok(Some(profile)) => {
                let active = match profile.active() {
                    Ok(active) => active.cloned(),
                    Err(error) => {
                        self.profile = Some(profile);
                        let path = self.store.path().display().to_string();
                        self.set_error(
                            "同步配置索引损坏",
                            error.to_string(),
                            vec![("配置文件".to_owned(), path)],
                            RetryKind::ReloadConfig,
                        );
                        return;
                    }
                };
                self.profile = Some(profile);
                if let Some(entry) = active {
                    self.try_open(entry, cx);
                }
            }
        }
    }

    /// 尝试打开一个已保存实例；失败进 Error（保留公开字段，不重建）。
    ///
    /// 例外：**实例目录被另一个进程占用**不是"实例损坏"，按 ADR 0011 回到实例选择页
    /// （Welcome）并给明确原因，用户可以先关掉另一个进程再点一次。
    fn try_open(&mut self, entry: InstanceEntry, cx: &mut Context<Self>) {
        if let Err(error) = self.open_instance(&entry, cx) {
            if let Some(lock) = error.downcast_ref::<super::lock::LockError>() {
                self.state = State::Welcome;
                self.notice = Some(format!(
                    "实例「{}」正被另一个进程使用（{}）；请先关掉那个进程再重试。",
                    entry.instance_id, lock
                ));
                cx.notify();
                return;
            }
            let message = sanitize_error(&error, &[]);
            let public = public_fields(&entry);
            self.retry_entry = Some(entry);
            self.set_error("无法打开已有实例", message, public, RetryKind::OpenInstance);
        }
    }

    /// 打开已保存实例：校验 relay/库/身份 → 取锁 → 打开库 → 构造 `AppShell`。
    fn open_instance(
        &mut self,
        entry: &InstanceEntry,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        // ADR 0013 §1：模式是公开配置里的**显式**字段，能力来自编译期 feature。
        if !crate::sync_gate::mode_allowed(entry.relay_mode) {
            bail!(
                "本次构建不开放 relay 模式 {}（local_dev 只在 dev-sync 构建中可用）",
                entry.relay_mode
            );
        }
        // 唯一权威 parser：手改的配置也要按**自己的模式**校验（local_dev 只允许明文
        // loopback，remote_tls 只允许 https + canonical origin），不降级、不改写。
        let origin = crate::sync_gate::parse_for_build(&entry.relay_url, entry.relay_mode)
            .map_err(|error| {
                anyhow::anyhow!(
                    "实例「{}」的 relay 配置无效（模式 {}）：{error}",
                    entry.instance_id,
                    entry.relay_mode
                )
            })?;
        if origin.as_str() != entry.relay_url {
            bail!(
                "实例「{}」的 relay_url 不是 canonical origin（应为 {:?}）；\
                 不自动改写配置，请人工修正",
                entry.instance_id,
                origin.as_str()
            );
        }
        let vault_path = entry.vault_path();
        if !vault_path.is_file() {
            bail!(
                "库文件不存在：{}（按 ADR 0011 不新建、不重建；请检查配置或重新创建）",
                vault_path.display()
            );
        }
        let identity_path = DevIdentityStore::path_for(&vault_path);
        // 锁在库/身份检查之后获取，避免给"不存在的配置条目"凭空建目录。
        // 保留 `LockError` 类型：`try_open` 要把"目录被占用"与"实例损坏"分开处理。
        let lock = InstanceLock::acquire(&entry.instance_dir()).map_err(anyhow::Error::new)?;
        let vault =
            Vault::open(&vault_path).map_err(|error| anyhow::anyhow!("打开库失败：{error}"))?;
        let identity = if identity_path.is_file() {
            let identity = DevIdentityStore
                .load(&vault_path)
                .map_err(|error| anyhow::anyhow!("{error:#}"))?;
            let identity_vault_id = menghunote_sync::ids::hex_of(&identity.vault_id);
            if identity_vault_id != entry.vault_id {
                bail!(
                    "库与配置不匹配：配置 vault_id={}，身份文件 vault_id={}；\
                     不覆盖、不重建（请核对配置或改用正确实例）",
                    entry.vault_id,
                    identity_vault_id
                );
            }
            Some(identity)
        } else {
            None
        };
        // ADR 0013 S3a：`AppRoot/Main` **打开实例前**调用启动对账。
        //
        // - 没有 pending → `Ok(None)`：纯 no-op（不联网、不写库）；
        // - `Prepared` → 安全放弃（请求从未发出）；`Done` → 清理；
        // - 已请求 + 有身份 → 用已有材料补 finalize（不重复 genesis/enroll）；
        // - 无身份/损坏/绑定矛盾 → 明确错误：进 Error 恢复页，**不自动重建/删库**，
        //   也**不构造 `AppShell`**（因此不会启动同步 worker）。
        let report = menghunote_sync::init_session::reconcile(&vault, identity.as_ref()).map_err(
            |error| {
                anyhow::anyhow!(
                    "初始化待办对账失败：{error}；按 ADR 0013 不自动重建、不删除库/身份/pin，请人工核对后再打开该实例"
                )
            },
        )?;
        if let Some(report) = &report {
            eprintln!(
                "[menghunote] 初始化对账完成：kind={} phase={:?} outcome={:?}",
                report.kind.as_str(),
                report.phase_before,
                report.outcome
            );
        }
        let Some(identity) = identity else {
            let detail = if report.is_some() {
                "该实例没有持久身份（未完成的初始化已按对账结论处理）"
            } else {
                "实例缺少持久身份"
            };
            bail!(
                "{detail}：{}（不自动重建、不删库；请用 Create 建新实例或人工恢复）",
                identity_path.display()
            );
        };
        let label = entry.short_label();
        let config = DevSyncConfig {
            vault: vault_path,
            relay_mode: entry.relay_mode,
            service: ServiceConfig::new_for(entry.relay_mode, entry.relay_url.clone())
                .with_api_version(entry.api_version),
        };
        let sync = SyncContext::new(identity.loro_peer_id(), identity.current_epoch);
        let shell = cx.new(move |cx| {
            AppShell::new_with_dev_sync_with_autosync(vault, label, config, sync, cx)
        });
        self.main = Some(shell);
        self.lock = Some(lock);
        self.instance = Some(entry.clone());
        self.state = State::Main;
        self.error = None;
        // active 指向被点开的实例（保存失败不影响本次打开，只提示）。
        let current = self
            .profile
            .as_ref()
            .and_then(|profile| profile.active_instance.clone());
        if current.as_deref() != Some(entry.instance_id.as_str()) {
            if let Err(error) = self.remember_active(entry) {
                self.notice = Some(format!("已打开实例，但记住选择失败：{error}"));
            }
        }
        Ok(())
    }

    /// 把 `entry` 设为 active 并原子写配置。
    fn remember_active(&mut self, entry: &InstanceEntry) -> Result<(), ProfileError> {
        let mut profile = self.profile.clone().unwrap_or_default();
        profile.upsert_active(entry.clone());
        self.store.save(&profile)?;
        self.profile = Some(profile);
        Ok(())
    }

    fn set_error(
        &mut self,
        title: impl Into<String>,
        message: impl Into<String>,
        public: Vec<(String, String)>,
        retry: RetryKind,
    ) {
        self.error = Some(ErrorState {
            title: title.into(),
            message: message.into(),
            public,
            retry,
        });
        self.state = State::Error;
        self.main = None;
    }

    // ---- 只读投影（测试/日志） ----

    /// 当前状态标签。
    #[must_use]
    pub const fn state_label(&self) -> RootStateLabel {
        match self.state {
            State::Welcome => RootStateLabel::Welcome,
            State::CreateForm => RootStateLabel::CreateForm,
            State::JoinForm => RootStateLabel::JoinForm,
            State::InvitePending => RootStateLabel::InvitePending,
            State::Working => RootStateLabel::Working,
            State::BackupRequired => RootStateLabel::BackupRequired,
            State::Opening => RootStateLabel::Opening,
            State::Main => RootStateLabel::Main,
            State::Error => RootStateLabel::Error,
        }
    }

    /// 备份页的恢复码（**只读**；Main/其它状态为 `None`）。
    #[must_use]
    pub fn recovery_code(&self) -> Option<&str> {
        self.pending
            .as_ref()
            .map(|pending| pending.outcome.recovery_code.as_str())
    }

    /// 备份页的 `vault_id`（公开标识）。
    #[must_use]
    pub fn vault_id(&self) -> Option<&str> {
        self.pending
            .as_ref()
            .map(|pending| pending.entry.vault_id.as_str())
    }

    /// Error 页标题（测试/诊断；已脱敏）。
    #[must_use]
    pub fn error_title(&self) -> Option<&str> {
        self.error.as_ref().map(|error| error.title.as_str())
    }

    /// Error 页的脱敏文案。
    #[must_use]
    pub fn error_message(&self) -> Option<&str> {
        self.error.as_ref().map(|error| error.message.as_str())
    }

    /// 最近一次提示（公开文案）。
    #[must_use]
    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    /// 已保存实例（Welcome 顺序：active 优先）。
    #[must_use]
    pub fn entries(&self) -> Vec<InstanceEntry> {
        self.profile
            .as_ref()
            .map(|profile| {
                profile
                    .ordered_entries()
                    .into_iter()
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    }

    /// 当前 active 实例（公开信息）。
    #[must_use]
    pub fn active_entry(&self) -> Option<&InstanceEntry> {
        self.instance.as_ref().or_else(|| {
            self.profile
                .as_ref()
                .and_then(|profile| profile.active().ok().flatten())
        })
    }

    /// 当前是否在后台创建/加入。
    #[must_use]
    pub const fn is_working(&self) -> bool {
        matches!(self.state, State::Working)
    }

    /// 后台操作当前阶段的固定文案（没有进度时 `None`）。
    #[must_use]
    pub fn working_label(&self) -> Option<&'static str> {
        self.working.as_ref().and_then(|working| working.stage)
    }

    /// 后台操作种类（测试断言"现在在 Join 还是 Create"）。
    #[must_use]
    pub fn working_is_join(&self) -> bool {
        self.working
            .as_ref()
            .is_some_and(|working| matches!(working.kind, WorkingKind::Join | WorkingKind::Invite))
    }

    /// 是否已持有 Main（测试用于确认 `AppShell` 子视图就绪）。
    #[must_use]
    pub const fn has_main_shell(&self) -> bool {
        self.main.is_some()
    }

    /// Main 里的 `AppShell`（**只读投影**，测试断言"自动打开了哪篇"用）。
    #[must_use]
    pub fn main_shell(&self) -> Option<&Entity<AppShell>> {
        self.main.as_ref()
    }

    /// Join 表单的公开字段（relay；测试断言失败后保留用）。
    #[must_use]
    pub fn join_form_relay(&self, cx: &App) -> Option<String> {
        self.join_form
            .as_ref()
            .map(|form| form.relay.read(cx).text().trim().to_owned())
    }

    /// Join 表单的 `vault_id` 输入（公开标识）。
    #[must_use]
    pub fn join_form_vault_id(&self, cx: &App) -> Option<String> {
        self.join_form
            .as_ref()
            .map(|form| form.vault_id.read(cx).text().trim().to_owned())
    }

    /// Join 表单的实例目录输入（公开路径）。
    #[must_use]
    pub fn join_form_dir(&self, cx: &App) -> Option<String> {
        self.join_form
            .as_ref()
            .map(|form| form.dir.read(cx).text().trim().to_owned())
    }

    /// 恢复码输入框是否已清空（**只报告是否为空，绝不暴露正文**）。
    #[must_use]
    pub fn join_form_recovery_is_empty(&self, cx: &App) -> Option<bool> {
        self.join_form
            .as_ref()
            .map(|form| form.recovery_code.read(cx).text().is_empty())
    }

    /// Join 表单当前的内联错误（脱敏文案）。
    #[must_use]
    pub fn join_form_error(&self) -> Option<&str> {
        self.join_form
            .as_ref()
            .and_then(|form| form.error.as_deref())
    }

    // ---- 设置面板（ADR 0011 S4：不改配置/命令行就能查看/修改同步配置） ----

    /// 打开设置面板（Welcome / Main 可用；Main 里 `AppShell` 保留不销毁）。
    ///
    /// 打开时重新读一次公开配置：设置面板显示的是**配置文件当前内容**（外部手改也
    /// 能看到），而不是某个缓存的旧副本。读失败只在面板内联报错，不覆盖配置、不重建
    /// 实例。
    pub fn open_settings(&mut self, cx: &mut Context<Self>) {
        if self.settings.is_some() || !matches!(self.state, State::Welcome | State::Main) {
            return;
        }
        let mut error = None;
        let profile = match self.store.load() {
            Ok(Some(profile)) => profile,
            Ok(None) => DevSyncProfile::default(),
            Err(load_error) => {
                error = Some(format!(
                    "配置读取失败：{load_error}；设置只针对已保存的公开配置，不会自动重建"
                ));
                self.profile.clone().unwrap_or_default()
            }
        };
        self.profile = Some(profile.clone());
        let active = match profile.active() {
            Ok(active) => active.cloned(),
            Err(active_error) => {
                error = Some(format!(
                    "配置的 active_instance 不可用：{active_error}；请先在列表里重新选默认实例"
                ));
                None
            }
        };
        let relay_value = active.as_ref().map_or_else(
            || self.options.default_relay.clone(),
            |entry| entry.relay_url.clone(),
        );
        // `FormInput::new` 的字符串参数是 placeholder（不是正文）：设置面板必须
        // **预填**当前 active 的 relay，避免用户看到空框后误以为配置丢了。
        let initial_relay = relay_value.clone();
        let relay = cx.new(move |cx| {
            let mut input = FormInput::new(initial_relay.clone(), false, cx)
                .selector("settings-relay")
                .click_to_focus();
            input.set_text(&initial_relay);
            input
        });
        // ADR 0013 §4：模式/指纹/错误类别都是**公开信息**；指纹只在"设置里的 active
        // 就是当前打开的实例"时读取（否则显示未打开，避免拿错库的 pin）。
        let mode = active
            .as_ref()
            .map_or(RelayMode::LocalDev, |entry| entry.relay_mode);
        let fingerprint = active
            .as_ref()
            .map(|entry| self.settings_fingerprint(entry, cx));
        let security = self
            .main
            .as_ref()
            .and_then(|shell| shell.read(cx).dev_sync_security_category());
        // Enter 与「保存」同一条路径（输入框里直接回车即可试保存；失败留在面板）。
        let subscription = cx.subscribe(&relay, |root, _input, event: &FormInputEvent, cx| {
            if matches!(event, FormInputEvent::Submit) {
                root.settings_save(cx);
            }
        });
        self.settings = Some(SettingsState {
            active,
            mode,
            remote_consent: false,
            relay,
            fingerprint,
            security,
            error,
            notice: None,
            focus_pending: true,
            _subs: vec![subscription],
        });
        cx.notify();
    }

    /// 关闭设置面板：**不写配置**；Main 的 `AppShell`/文档状态原样保留。
    pub fn close_settings(&mut self, cx: &mut Context<Self>) {
        if self.settings.take().is_none() {
            return;
        }
        if self.state == State::Main {
            // 下一帧把焦点还给编辑器（设置打开时焦点在 relay 输入框）。
            self.pending_focus_restore = true;
        }
        cx.notify();
    }

    /// 设置面板是否打开（测试/投影用；状态机本身仍停在 Welcome/Main）。
    #[must_use]
    pub fn settings_is_open(&self) -> bool {
        self.settings.is_some()
    }

    /// 设置面板 relay 输入框的当前文本（公开字段；`None` = 面板未打开）。
    #[must_use]
    pub fn settings_relay(&self, cx: &App) -> Option<String> {
        self.settings
            .as_ref()
            .map(|settings| settings.relay.read(cx).text().trim().to_owned())
    }

    /// 测试/预填：设置 relay 输入框文本。
    pub fn settings_set_relay(&mut self, text: &str, cx: &mut Context<Self>) {
        if let Some(settings) = &self.settings {
            settings.relay.update(cx, |input, cx| {
                input.set_text(text);
                cx.notify();
            });
        }
    }

    /// 设置面板里的实例列表（active 优先；公开条目）。
    #[must_use]
    pub fn settings_entries(&self) -> Vec<InstanceEntry> {
        self.entries()
    }

    /// 设置面板当前针对的 active 实例（公开信息）。
    #[must_use]
    pub fn settings_active_entry(&self) -> Option<InstanceEntry> {
        self.settings
            .as_ref()
            .and_then(|settings| settings.active.clone())
    }

    /// 设置面板内联错误（`None` = 没有错误或面板未打开）。
    #[must_use]
    pub fn settings_error(&self) -> Option<&str> {
        self.settings
            .as_ref()
            .and_then(|settings| settings.error.as_deref())
    }

    /// 设置面板内联提示（含「重启后生效」）。
    #[must_use]
    pub fn settings_notice(&self) -> Option<&str> {
        self.settings
            .as_ref()
            .and_then(|settings| settings.notice.as_deref())
    }

    /// 「保存」：只改 **active 实例**的 relay，并原子写公开配置。
    ///
    /// - 没有 active 实例 → 内联错误（先在列表里点「设为默认」）；
    /// - relay 非法 → 内联错误，**不写配置**；
    /// - 写失败 → 内联错误 + 可重试；不改库/身份、不重建实例、不跑 Create/Join；
    /// - 成功 → 提示「重启后生效」；当前 `AppShell`/worker **不热切换**。
    pub fn settings_save(&mut self, cx: &mut Context<Self>) {
        let (active, raw_relay, mode, remote_consent) = {
            let Some(settings) = self.settings.as_ref() else {
                return;
            };
            (
                settings.active.clone(),
                settings.relay.read(cx).text().trim().to_owned(),
                settings.mode,
                settings.remote_consent,
            )
        };
        let Some(active) = active else {
            self.set_settings_error(
                "当前没有 active 实例：先在实例列表里点「设为默认」，再修改 relay 模式/地址"
                    .to_owned(),
            );
            cx.notify();
            return;
        };
        // ADR 0013 §1：remote_tls 必须显式确认"远程服务器/HTTPS"（确认端点 ≠ 授权接受
        // 服务器任意 hash；pin 仍独立校验）。
        if mode == RelayMode::RemoteTls && !remote_consent {
            self.set_settings_error(
                "切换到 remote_tls 需要显式勾选「我确认要连接远程服务器（HTTPS）」；\
                 确认不会清 pin、也不会自动信任新服务器"
                    .to_owned(),
            );
            cx.notify();
            return;
        }
        let relay = match validate_relay_edit(&raw_relay, mode) {
            Ok(relay) => relay,
            Err(message) => {
                self.set_settings_error(message);
                cx.notify();
                return;
            }
        };
        let mut profile = self.profile.clone().unwrap_or_default();
        let Some(entry) = profile
            .instances
            .iter_mut()
            .find(|entry| entry.instance_id == active.instance_id)
        else {
            self.set_settings_error(format!(
                "配置里找不到 active 实例「{}」；请重新打开设置（不自动改配置）",
                active.instance_id
            ));
            cx.notify();
            return;
        };
        let changed_mode = entry.relay_mode != mode;
        entry.relay_mode = mode;
        entry.relay_url = relay.clone();
        if let Err(error) = self.store.save(&profile) {
            self.set_settings_error(format!(
                "配置保存失败：{error}；库/身份/pin 都没有被改动，也没有重建实例。\
                 修正配置目录权限后可重试保存（不要重新 Create/Join）"
            ));
            cx.notify();
            return;
        }
        self.profile = Some(profile);
        let mut updated = active;
        updated.relay_mode = mode;
        updated.relay_url = relay;
        if let Some(settings) = &mut self.settings {
            settings.active = Some(updated.clone());
            settings.error = None;
            settings.notice = Some(if changed_mode {
                format!(
                    "relay 模式已保存为 {}：重启后生效（当前窗口仍是旧模式/旧地址，不热切换）。\
                     不会清 pin、不会自动信任新服务器；换 relay instance 请建新实例或重新 Join。",
                    mode.as_str()
                )
            } else {
                "relay 已保存：重启后生效。当前窗口仍使用旧 relay（不热切换 Vault/worker）；\
                 实例没有重建，pin 没有被清。"
                    .to_owned()
            });
            // 保存后 active 已经是我们刚写的模式；指纹的 mode/origin 也跟着更新
            // （pin 本身不变，因此 rev/hash 保持不变）。
            if let Some(FingerprintState::Pin(pin)) = &mut settings.fingerprint {
                pin.mode = mode;
                pin.origin = updated.relay_url.clone();
            }
        }
        cx.notify();
    }

    /// 「设为默认」：只改公开配置的 `active_instance`，原子写；**不热切换**当前 Main。
    pub fn settings_set_active(&mut self, instance_id: &str, cx: &mut Context<Self>) {
        if self.settings.is_none() {
            return;
        }
        let mut profile = self.profile.clone().unwrap_or_default();
        let Some(entry) = profile.find(instance_id).cloned() else {
            self.set_settings_error(format!(
                "实例不存在：{instance_id}；配置没有被改动，请重读设置"
            ));
            cx.notify();
            return;
        };
        if profile.active_instance.as_deref() == Some(instance_id) {
            if let Some(settings) = &mut self.settings {
                settings.error = None;
                settings.notice = Some(format!(
                    "「{}」已经是默认实例：重启后仍会打开它（当前界面不变）。",
                    entry.instance_id
                ));
            }
            cx.notify();
            return;
        }
        profile.active_instance = Some(instance_id.to_owned());
        if let Err(error) = self.store.save(&profile) {
            self.set_settings_error(format!(
                "配置保存失败：{error}；active 没有被改动，可修正后重试（不会动实例目录）"
            ));
            cx.notify();
            return;
        }
        self.profile = Some(profile);
        let fingerprint = self.settings_fingerprint(&entry, cx);
        if let Some(settings) = &mut self.settings {
            settings.active = Some(entry.clone());
            settings.mode = entry.relay_mode;
            settings.remote_consent = false;
            settings.fingerprint = Some(fingerprint);
            settings.error = None;
            settings.notice = Some(format!(
                "已将「{}」设为默认：重启后生效；当前主界面保持不变，\
                 不热切换 Vault/worker。",
                entry.instance_id
            ));
            // 新 active 的 relay 才是可编辑对象：输入框跟着切过去，避免把 A 的
            // relay 误写到 B。
            let relay_url = entry.relay_url.clone();
            settings.relay.update(cx, |input, cx| {
                input.set_text(&relay_url);
                cx.notify();
            });
        }
        cx.notify();
    }

    /// 复制**公开值**（`vault_id` / `device_id` / 库路径）到剪贴板并提示风险。
    pub fn settings_copy_public(
        &mut self,
        value: String,
        label: &'static str,
        cx: &mut Context<Self>,
    ) {
        if self.settings.is_none() {
            return;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(value));
        if let Some(settings) = &mut self.settings {
            settings.error = None;
            settings.notice = Some(format!(
                "已复制 {label}（公开标识，可给协作者）；不要分享 setup secret、恢复码、\
                 根密钥或设备私钥。"
            ));
        }
        cx.notify();
    }

    fn set_settings_error(&mut self, message: String) {
        if let Some(settings) = &mut self.settings {
            settings.error = Some(message);
            settings.notice = None;
        }
    }

    /// 设置页的只读指纹快照（**公开**；只在设置里的 active 就是当前打开的实例时读取）。
    fn settings_fingerprint(&self, entry: &InstanceEntry, cx: &App) -> FingerprintState {
        let opened_same = self
            .instance
            .as_ref()
            .is_some_and(|current| current.instance_id == entry.instance_id);
        let Some(shell) = self.main.as_ref().filter(|_| opened_same) else {
            return FingerprintState::Unavailable(
                "该实例当前没有打开；打开后才能读取 durable pin（切换默认/保存模式只写配置，重启后生效）"
                    .to_owned(),
            );
        };
        let vault_id = match menghunote_sync::ids::parse_hex::<16>(&entry.vault_id) {
            Ok(vault_id) => vault_id,
            Err(_) => {
                return FingerprintState::Corrupt(
                    "配置里的 vault_id 不是 32 位十六进制，无法读取 pin".to_owned(),
                )
            }
        };
        let result = shell.read(cx).dev_sync_control_pin(vault_id);
        FingerprintState::from_result(entry.relay_mode, &entry.relay_url, result)
    }

    /// 设置面板当前编辑的 relay 模式（公开配置字段的投影）。
    #[must_use]
    pub fn settings_mode(&self) -> Option<RelayMode> {
        self.settings.as_ref().map(|settings| settings.mode)
    }

    /// 切换设置面板的 relay 模式（只改面板状态；保存才写配置，重启后生效）。
    pub fn settings_set_mode(&mut self, mode: RelayMode, cx: &mut Context<Self>) {
        if let Some(settings) = &mut self.settings {
            settings.mode = mode;
            settings.remote_consent = false;
            settings.error = None;
            settings.notice = None;
        }
        cx.notify();
    }

    /// 设置面板 remote_tls 的显式确认（"远程服务器/HTTPS"）。
    #[must_use]
    pub fn settings_remote_consent(&self) -> Option<bool> {
        self.settings
            .as_ref()
            .map(|settings| settings.remote_consent)
    }

    /// 勾选/取消 remote_tls 的显式确认。
    pub fn settings_set_remote_consent(&mut self, consent: bool, cx: &mut Context<Self>) {
        if let Some(settings) = &mut self.settings {
            settings.remote_consent = consent;
        }
        cx.notify();
    }

    /// 设置面板的安全指纹（只读公开投影；`None` = 面板未打开）。
    #[must_use]
    pub fn settings_fingerprint_state(&self) -> Option<FingerprintState> {
        self.settings
            .as_ref()
            .and_then(|settings| settings.fingerprint.clone())
    }

    /// 设置面板的快照错误类别（安全暂停；`None` = 没有/面板未打开）。
    #[must_use]
    pub fn settings_security_category(&self) -> Option<crate::dev_sync::SecurityCategory> {
        self.settings
            .as_ref()
            .and_then(|settings| settings.security)
    }

    /// 复制安全指纹的**完整公开文本**（无 token/秘密）。
    pub fn settings_copy_fingerprint(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.settings_fingerprint_state() else {
            self.set_settings_error(
                "安全指纹不可复制：设置面板未打开（不改配置/不联网）".to_owned(),
            );
            cx.notify();
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(state.public_text()));
        if let Some(settings) = &mut self.settings {
            settings.error = None;
            settings.notice = Some(
                "已复制安全指纹（公开值：模式/origin/instance/rev/hash）；它不含 token/恢复码/主密钥，可给协作者带外核对。换服务器/清 pin 不会自动发生。"
                    .to_owned(),
            );
        }
        cx.notify();
    }

    // ---- Welcome / 表单动作 ----

    /// 进入 Create 表单（默认 relay / 自动实例目录 / 秘密字段）。
    pub fn enter_create_form(&mut self, cx: &mut Context<Self>) {
        let instance_id = self
            .options
            .instance_id
            .clone()
            .unwrap_or_else(generate_instance_id);
        let default_dir = self
            .options
            .layout
            .instance_dir(&instance_id)
            .map(|path| path.display().to_string())
            .unwrap_or_default();
        let default_relay = self.options.default_relay.clone();
        let relay = cx.new(|cx| {
            FormInput::new(default_relay, false, cx)
                .selector("app-root-relay-input")
                .click_to_focus()
        });
        let secret = cx.new(|cx| {
            FormInput::new("setup secret", true, cx)
                .selector("app-root-secret-input")
                .click_to_focus()
        });
        // 创建链接含一次性票据：遮罩 + 禁复制 + 允许粘贴（与秘密字段同一控件）。
        let link = cx.new(|cx| {
            FormInput::new("粘贴创建链接 mn-pair://create?...", true, cx)
                .selector("app-root-create-link-input")
                .click_to_focus()
        });
        let dir = cx.new(|cx| {
            FormInput::new(default_dir, false, cx)
                .selector("app-root-dir-input")
                .click_to_focus()
        });
        // 在输入链接之后按 Enter = 应用链接；其它字段按 Enter = 提交表单。
        let mut subs = Vec::new();
        let link_subscription = cx.subscribe(&link, |root, _input, event: &FormInputEvent, cx| {
            if matches!(event, FormInputEvent::Submit) {
                root.form_apply_create_link(cx);
            }
        });
        subs.push(link_subscription);
        for input in [relay.clone(), secret.clone(), dir.clone()] {
            let subscription = cx.subscribe(&input, |root, _input, event: &FormInputEvent, cx| {
                if matches!(event, FormInputEvent::Submit) {
                    root.submit_create(cx);
                }
            });
            subs.push(subscription);
        }
        self.form = Some(FormState {
            instance_id,
            // ADR 0013 §1：默认 local_dev；remote_tls 需要显式选择 + 确认（本片不发起
            // 远端 Create 网络，S3b 才接线）。
            mode: RelayMode::LocalDev,
            remote_consent: false,
            relay,
            secret,
            link,
            link_state: None,
            dir,
            consent: false,
            error: None,
            _subs: subs,
        });
        self.state = State::CreateForm;
        self.error = None;
        self.focus_create_link = false;
        self.pending_focus = true;
        cx.notify();
    }

    /// 进入 **粘贴创建链接** 的 Create 表单（ADR 0014 S3b：`mn-pair://create` + v2 票据）。
    ///
    /// 与 [`Self::enter_create_form`] 同一个表单/同一条提交路径；差别只是初始焦点在
    /// 创建链接字段、并给出一次性票据提示。票据模式**不需要** setup secret。
    pub fn enter_create_link_form(&mut self, cx: &mut Context<Self>) {
        self.enter_create_form(cx);
        self.focus_create_link = true;
        self.notice = Some(
            "粘贴创建链接（mn-pair://create?...&vault=…#ticket=…）：票据一次性、默认 15 分钟内有效，             只从内存提交；不写配置/日志，失败也不会保存票据。"
                .to_owned(),
        );
        cx.notify();
    }

    /// 测试/预填：设置 relay 输入框文本。
    pub fn form_set_relay(&mut self, text: &str, cx: &mut Context<Self>) {
        if let Some(form) = &self.form {
            form.relay.update(cx, |input, cx| {
                input.set_text(text);
                cx.notify();
            });
        }
    }

    /// 测试/预填：设置 setup secret 输入框文本。
    pub fn form_set_secret(&mut self, text: &str, cx: &mut Context<Self>) {
        if let Some(form) = &self.form {
            form.secret.update(cx, |input, cx| {
                input.set_text(text);
                cx.notify();
            });
        }
    }

    /// 测试/预填：设置实例目录输入框文本。
    pub fn form_set_dir(&mut self, text: &str, cx: &mut Context<Self>) {
        if let Some(form) = &self.form {
            form.dir.update(cx, |input, cx| {
                input.set_text(text);
                cx.notify();
            });
        }
    }

    /// 测试/预填：设置创建链接输入框文本（秘密；只在控件里存在）。
    pub fn form_set_create_link(&mut self, text: &str, cx: &mut Context<Self>) {
        if let Some(form) = &self.form {
            form.link.update(cx, |input, cx| {
                input.set_text(text);
                cx.notify();
            });
        }
    }

    /// 测试/断言：创建链接输入框是否为空（**绝不暴露正文**）。
    pub fn form_create_link_is_empty(&self, cx: &App) -> Option<bool> {
        self.form
            .as_ref()
            .map(|form| form.link.read(cx).text().is_empty())
    }

    /// 是否已应用一条创建链接（票据模式）。
    #[must_use]
    pub fn form_has_create_link(&self) -> bool {
        self.form
            .as_ref()
            .is_some_and(|form| form.link_state.is_some())
    }

    /// 已应用链接的 vault id（公开标识；测试/展示用）。
    #[must_use]
    pub fn form_create_link_vault_id(&self) -> Option<&str> {
        self.form
            .as_ref()
            .and_then(|form| form.link_state.as_ref())
            .map(|link| link.vault_id_hex.as_str())
    }

    /// 严格解析并应用创建链接（ADR 0014 S3b）。
    ///
    /// 成功：预填 canonical relay / 模式 / vault id，切到票据模式（隐藏 setup secret），
    /// **立刻清空**链接输入框并丢弃完整 URL（fragment 里的票据只进内存状态）。
    /// 失败：内联脱敏错误（`CreateLinkError` 不含票据/URL），不联网、不建文件。
    pub fn form_apply_create_link(&mut self, cx: &mut Context<Self>) {
        let text = match &self.form {
            Some(form) => form.link.read(cx).text().trim().to_owned(),
            None => return,
        };
        let fail = |root: &mut Self, message: String, cx: &mut Context<Self>| {
            if let Some(form) = &mut root.form {
                form.error = Some(message);
            }
            cx.notify();
        };
        if text.is_empty() {
            fail(
                self,
                "请先粘贴创建链接（mn-pair://create?…&vault=…#ticket=…）".to_owned(),
                cx,
            );
            return;
        }
        let parsed = match CreateLink::parse(&text) {
            Ok(parsed) => parsed,
            Err(error) => {
                fail(
                    self,
                    format!("创建链接无效：{error}（没有联网，也没有创建任何文件）"),
                    cx,
                );
                return;
            }
        };
        // 只取公开定位信息 + 票据本体；完整 URL（含 fragment ticket）立刻丢弃。
        let CreateLink {
            mode,
            relay,
            vault_id,
            ticket,
            pin,
            url,
        } = parsed;
        drop(url);
        let vault_id_hex = menghunote_sync::ids::hex_of(&vault_id);
        let Some(form) = self.form.as_mut() else {
            return;
        };
        form.mode = mode;
        // 远端必须在票据模式下重新确认（换链接 = 换端点，不能沿用旧确认）。
        form.remote_consent = false;
        form.link_state = Some(CreateLinkState {
            mode,
            relay: relay.clone(),
            vault_id,
            vault_id_hex: vault_id_hex.clone(),
            ticket,
            pin,
        });
        form.error = None;
        // 预填 relay（vault id 以只读事实展示）；清空链接与 setup secret 输入框。
        form.relay.update(cx, |input, cx| {
            input.set_text(&relay);
            cx.notify();
        });
        form.link.update(cx, |input, cx| {
            input.clear();
            cx.notify();
        });
        form.secret.update(cx, |input, cx| {
            input.clear();
            cx.notify();
        });
        self.notice = Some(format!(
            "已载入创建链接：vault_id {vault_id_hex}（{}）；票据一次性、默认 15 分钟内有效，             提交时只使用一次，不写入配置/日志。",
            mode.as_str()
        ));
        cx.notify();
    }

    /// 清掉已应用的创建链接（回到 v1 setup secret 模式；不触发任何网络）。
    pub fn form_clear_create_link(&mut self, cx: &mut Context<Self>) {
        let default_relay = self.options.default_relay.clone();
        let Some(form) = &mut self.form else {
            return;
        };
        if form.link_state.take().is_none() {
            return;
        }
        form.mode = RelayMode::LocalDev;
        form.remote_consent = false;
        form.error = None;
        // relay 回到默认值：链接可能来自别的 origin（或 https），残留在 v1 模式会
        // 直接触发"模式/地址不匹配"的内联错误。
        form.relay.update(cx, |input, cx| {
            input.set_text(&default_relay);
            cx.notify();
        });
        self.notice = Some("已移除创建链接：回到 setup secret（v1）创建模式".to_owned());
        cx.notify();
    }

    /// 测试/断言：Create 表单当前 relay 文本。
    pub fn form_relay_text(&self, cx: &App) -> Option<String> {
        self.form
            .as_ref()
            .map(|form| form.relay.read(cx).text().to_owned())
    }

    /// 测试/断言：Create 表单当前 setup secret 文本（只用于测试，不对外展示）。
    pub fn form_secret_text(&self, cx: &App) -> Option<String> {
        self.form
            .as_ref()
            .map(|form| form.secret.read(cx).text().to_owned())
    }

    /// 测试/断言：Create 表单当前实例目录文本。
    pub fn form_dir_text(&self, cx: &App) -> Option<String> {
        self.form
            .as_ref()
            .map(|form| form.dir.read(cx).text().to_owned())
    }

    /// 勾选/取消"同意开发同步"。
    pub fn form_set_consent(&mut self, consent: bool, cx: &mut Context<Self>) {
        if let Some(form) = &mut self.form {
            form.consent = consent;
        }
        cx.notify();
    }

    /// Create 表单当前的内联错误（脱敏文案）。
    #[must_use]
    pub fn form_error(&self) -> Option<&str> {
        self.form.as_ref().and_then(|form| form.error.as_deref())
    }

    /// Create 表单当前选择的 relay 模式。
    #[must_use]
    pub fn form_mode(&self) -> Option<RelayMode> {
        self.form.as_ref().map(|form| form.mode)
    }

    /// 设置 Create 表单的 relay 模式（测试/按钮共用；不写配置、不联网）。
    pub fn form_set_mode(&mut self, mode: RelayMode, cx: &mut Context<Self>) {
        if let Some(form) = &mut self.form {
            form.mode = mode;
            form.error = None;
        }
        cx.notify();
    }

    /// Create 表单是否已勾选 remote_tls 的显式确认。
    #[must_use]
    pub fn form_remote_consent(&self) -> Option<bool> {
        self.form.as_ref().map(|form| form.remote_consent)
    }

    /// **测试专用**：给 remote_tls 的 Create/Join worker 注入显式信任根（DER）。
    ///
    /// 只用于本地 TLS harness（`menghunote-dev-relay::start_tls` 的自签 CA）；生产不调用。
    /// 只**增加**显式信任，不关闭证书链/SAN/有效期校验，也不提供 accept-invalid。
    #[doc(hidden)]
    pub fn set_test_relay_roots(&mut self, roots: Vec<Vec<u8>>) {
        self.test_relay_roots = roots;
    }

    /// 设置 Create 表单的 remote_tls 确认勾选。
    pub fn form_set_remote_consent(&mut self, consent: bool, cx: &mut Context<Self>) {
        if let Some(form) = &mut self.form {
            form.remote_consent = consent;
        }
        cx.notify();
    }

    /// 进入 Join 表单（默认 relay / 自动实例目录 / 空 vault_id / 遮罩恢复码）。
    ///
    /// 与 Create 共用目录默认值与实例锁；`vault_id` 是**公开标识**（不遮罩），
    /// 恢复码是**秘密**（遮罩、禁复制、允许粘贴）。
    pub fn enter_join_form(&mut self, cx: &mut Context<Self>) {
        let instance_id = self
            .options
            .instance_id
            .clone()
            .unwrap_or_else(generate_instance_id);
        let default_dir = self
            .options
            .layout
            .instance_dir(&instance_id)
            .map(|path| path.display().to_string())
            .unwrap_or_default();
        let default_relay = self.options.default_relay.clone();
        let relay = cx.new(|cx| {
            FormInput::new(default_relay, false, cx)
                .selector("app-root-join-relay-input")
                .click_to_focus()
        });
        let vault_id = cx.new(|cx| {
            FormInput::new("32 位十六进制 vault_id", false, cx)
                .selector("app-root-vault-id-input")
                .click_to_focus()
        });
        let recovery_code = cx.new(|cx| {
            FormInput::new("恢复码 mnrec1…", true, cx)
                .selector("app-root-recovery-input")
                .click_to_focus()
        });
        // 邀请链接含一次性 token：遮罩 + 禁复制 + 允许粘贴（与秘密字段同一控件）。
        let invite = cx.new(|cx| {
            FormInput::new(INVITE_EMPTY_HINT, true, cx)
                .selector("app-root-invite-input")
                .click_to_focus()
        });
        let dir = cx.new(|cx| {
            FormInput::new(default_dir, false, cx)
                .selector("app-root-join-dir-input")
                .click_to_focus()
        });
        // 在任意字段按 Enter 都提交（与按钮同一条路径）。
        let mut subs = Vec::new();
        for input in [
            relay.clone(),
            vault_id.clone(),
            recovery_code.clone(),
            invite.clone(),
            dir.clone(),
        ] {
            let subscription = cx.subscribe(&input, |root, _input, event: &FormInputEvent, cx| {
                if matches!(event, FormInputEvent::Submit) {
                    root.submit_join(cx);
                }
            });
            subs.push(subscription);
        }
        self.join_form = Some(JoinFormState {
            instance_id,
            mode: RelayMode::LocalDev,
            remote_consent: false,
            relay,
            vault_id,
            recovery_code,
            invite,
            dir,
            error: None,
            _subs: subs,
        });
        self.state = State::JoinForm;
        self.error = None;
        self.pending_focus = true;
        cx.notify();
    }

    /// 测试/预填：设置 Join 的 relay 输入框文本。
    pub fn join_form_set_relay(&mut self, text: &str, cx: &mut Context<Self>) {
        if let Some(form) = &self.join_form {
            form.relay.update(cx, |input, cx| {
                input.set_text(text);
                cx.notify();
            });
        }
    }

    /// 测试/预填：设置 Join 的 `vault_id` 输入框文本（公开标识）。
    pub fn join_form_set_vault_id(&mut self, text: &str, cx: &mut Context<Self>) {
        if let Some(form) = &self.join_form {
            form.vault_id.update(cx, |input, cx| {
                input.set_text(text);
                cx.notify();
            });
        }
    }

    /// 测试/预填：设置 Join 的恢复码（**秘密**；只进内存）。
    pub fn join_form_set_recovery_code(&mut self, text: &str, cx: &mut Context<Self>) {
        if let Some(form) = &self.join_form {
            form.recovery_code.update(cx, |input, cx| {
                input.set_text(text);
                cx.notify();
            });
        }
    }

    /// 测试/预填：设置 Join 的邀请链接（**秘密**：含一次性 token，只进内存）。
    pub fn join_form_set_invite(&mut self, text: &str, cx: &mut Context<Self>) {
        if let Some(form) = &self.join_form {
            form.invite.update(cx, |input, cx| {
                input.set_text(text);
                cx.notify();
            });
        }
    }

    /// Join 表单当前的邀请链接文本（测试断言；**不当日志**用）。
    #[must_use]
    pub fn join_form_invite(&self, cx: &App) -> Option<String> {
        self.join_form
            .as_ref()
            .map(|form| form.invite.read(cx).text().trim().to_owned())
    }

    /// 测试/预填：设置 Join 的实例目录输入框文本。
    pub fn join_form_set_dir(&mut self, text: &str, cx: &mut Context<Self>) {
        if let Some(form) = &self.join_form {
            form.dir.update(cx, |input, cx| {
                input.set_text(text);
                cx.notify();
            });
        }
    }

    /// Join 表单当前选择的 relay 模式。
    #[must_use]
    pub fn join_form_mode(&self) -> Option<RelayMode> {
        self.join_form.as_ref().map(|form| form.mode)
    }

    /// 设置 Join 表单的 relay 模式（不写配置、不联网）。
    pub fn join_form_set_mode(&mut self, mode: RelayMode, cx: &mut Context<Self>) {
        if let Some(form) = &mut self.join_form {
            form.mode = mode;
            form.error = None;
        }
        cx.notify();
    }

    /// Join 表单是否已勾选 remote_tls 的显式确认。
    #[must_use]
    pub fn join_form_remote_consent(&self) -> Option<bool> {
        self.join_form.as_ref().map(|form| form.remote_consent)
    }

    /// 设置 Join 表单的 remote_tls 确认勾选。
    pub fn join_form_set_remote_consent(&mut self, consent: bool, cx: &mut Context<Self>) {
        if let Some(form) = &mut self.join_form {
            form.remote_consent = consent;
        }
        cx.notify();
    }

    /// 点击实例列表里的一个已保存实例。
    pub fn open_saved_instance(&mut self, instance_id: &str, cx: &mut Context<Self>) {
        let Some(entry) = self
            .profile
            .as_ref()
            .and_then(|profile| profile.find(instance_id))
            .cloned()
        else {
            self.notice = Some(format!("实例不存在：{instance_id}"));
            cx.notify();
            return;
        };
        self.state = State::Opening;
        self.try_open(entry, cx);
        cx.notify();
    }

    /// 「继续本地」：打开默认 dev vault（**无同步**，明确 DEV 标注）。
    pub fn open_local_vault(&mut self, cx: &mut Context<Self>) {
        if !self.options.allow_local_vault {
            self.notice = Some("本地入口在本进程被禁用（测试注入）".to_owned());
            cx.notify();
            return;
        }
        match dev_vault::open_dev_vault() {
            Ok(dev) => {
                let label = dev.path.display().to_string();
                let vault = dev.vault.clone();
                let shell = cx.new(|cx| AppShell::new(vault, label, cx));
                self.main = Some(shell);
                self.instance = None;
                self.lock = None;
                self.state = State::Main;
                self.error = None;
                self.notice = Some(
                    "本地模式：默认 dev vault（DEV ONLY，无同步面板）；同步实例请用左侧创建。"
                        .to_owned(),
                );
            }
            Err(error) => {
                let message = sanitize_error(&error, &[]);
                self.set_error(
                    "无法打开本地 dev vault",
                    message,
                    vec![("目录".to_owned(), "target/tmp/menghunote-dev".to_owned())],
                    RetryKind::None,
                );
            }
        }
        cx.notify();
    }

    /// 提交 Create 表单（按钮与 Enter 的同一入口）。
    ///
    /// 已应用创建链接（ADR 0014 S3b）时走票据模式；否则是既有 v1 setup secret 模式。
    /// 两条路径互斥（类型上一个请求只带一种 [`CreateCredential`]）。
    pub fn submit_create(&mut self, cx: &mut Context<Self>) {
        if self.state != State::CreateForm {
            return;
        }
        if self.form_has_create_link() {
            self.submit_ticket_create(cx);
            return;
        }
        let Some(form) = &self.form else {
            return;
        };
        let relay = form.relay.read(cx).text().trim().to_owned();
        let secret = Zeroizing::new(form.secret.read(cx).text().trim().to_owned());
        let dir = form.dir.read(cx).text().trim().to_owned();
        let consent = form.consent;
        let mode = form.mode;
        let remote_consent = form.remote_consent;
        let form_error = |form: &mut FormState, message: String| {
            form.error = Some(message);
        };
        if let Some(form) = &mut self.form {
            form.error = None;
        }
        // 0) remote_tls 必须显式确认"远程服务器/HTTPS"（ADR 0013 §1）。确认端点 ≠ 授权
        //    接受服务器任意 hash；真正的信任只来自受限初始化的强 pin。
        if mode == RelayMode::RemoteTls && !remote_consent {
            if let Some(form) = &mut self.form {
                form_error(
                    form,
                    "请先勾选「我确认要连接远程服务器（HTTPS）」：remote_tls 不是默认模式，确认端点 ≠ 授权接受服务器任意 hash"
                        .to_owned(),
                );
            }
            cx.notify();
            return;
        }
        // 1) relay 必须是本模式允许的 canonical origin（同一套权威 parser）。
        if let Err(error) = validate_relay_edit(&relay, mode) {
            if let Some(form) = &mut self.form {
                form_error(form, error);
            }
            cx.notify();
            return;
        }
        // 2) setup secret 必填。
        if secret.is_empty() {
            if let Some(form) = &mut self.form {
                form_error(
                    form,
                    "请填写 setup secret（服务器创建凭证；只从本表单进内存，不写配置/日志）"
                        .to_owned(),
                );
            }
            cx.notify();
            return;
        }
        // 3) 实例目录：绝对路径 + 尚不存在或空目录（Create/Join 共用，见
        //    `validate_instance_directory`；"已有库/身份/别的文件"一律拒绝，不覆盖）。
        let dir_path = PathBuf::from(&dir);
        if let Err(message) = validate_instance_directory(&dir_path) {
            if let Some(form) = &mut self.form {
                form_error(form, message);
            }
            cx.notify();
            return;
        }
        // 4) 显式同意开发同步。
        if !consent {
            if let Some(form) = &mut self.form {
                form_error(
                    form,
                    "请先勾选「同意开发同步」：这是 localhost 开发验证，不是生产跨设备安全同步"
                        .to_owned(),
                );
            }
            cx.notify();
            return;
        }
        let vault_path = dir_path.join(VAULT_FILE_NAME);
        // 5) 取实例目录锁 → 启动后台 worker。
        let lock = match InstanceLock::acquire(&dir_path) {
            Ok(lock) => lock,
            Err(error) => {
                if let Some(form) = &mut self.form {
                    form_error(form, error.to_string());
                }
                cx.notify();
                return;
            }
        };
        let request = CreateRequest {
            vault_path,
            relay_url: relay,
            relay_mode: mode,
            credential: CreateCredential::SetupSecret(secret),
            lock,
            test_extra_roots: self.test_relay_roots.clone(),
        };
        let job = match CreateJob::start(request) {
            Ok(job) => job,
            Err(error) => {
                if let Some(form) = &mut self.form {
                    form_error(form, format!("无法启动后台创建线程：{error}"));
                }
                cx.notify();
                return;
            }
        };
        // secret 已交给 worker：立刻清空输入框（不在 UI 里继续持有）。
        if let Some(form) = &mut self.form {
            form.secret.update(cx, |input, cx| {
                input.clear();
                cx.notify();
            });
        }
        self.job = Some(job);
        self.working = Some(WorkingState {
            kind: WorkingKind::Create,
            stage: None,
            cancel_requested: false,
            started: Instant::now(),
            budget_notice_shown: false,
        });
        self.state = State::Working;
        self.notice = None;
        self.spawn_job_pump(cx);
        cx.notify();
    }

    /// 票据模式提交（ADR 0014 S3b：`create_vault_with_ticket`，固定 v2 面）。
    ///
    /// 校验顺序（全部在 UI 线程；任何失败都不联网、不建文件）：链接已应用 →
    /// `remote_tls` 确认 → 实例目录（与 v1 Create 共用同一套）→ 显式同意 → 目录锁。
    /// 通过后票据**只**移动到 worker（不写配置/日志）；失败后需重新粘贴同一链接
    /// （票据未被 relay 消费时可重试）。
    fn submit_ticket_create(&mut self, cx: &mut Context<Self>) {
        let Some(form) = self.form.as_ref() else {
            return;
        };
        let Some(mode) = form.link_state.as_ref().map(|link| link.mode) else {
            return;
        };
        let remote_consent = form.remote_consent;
        let consent = form.consent;
        let dir = form.dir.read(cx).text().trim().to_owned();
        if let Some(form) = &mut self.form {
            form.error = None;
        }
        let fail = |root: &mut Self, message: String, cx: &mut Context<Self>| {
            if let Some(form) = &mut root.form {
                form.error = Some(message);
            }
            cx.notify();
        };
        // 0) remote_tls 必须显式确认（链接只预填端点，不构成授权）。
        if mode == RelayMode::RemoteTls && !remote_consent {
            fail(
                self,
                "请先勾选「我确认要连接远程服务器（HTTPS）」：remote_tls 不是默认模式，确认端点 ≠ 授权接受服务器任意 hash"
                    .to_owned(),
                cx,
            );
            return;
        }
        // 1) 实例目录：绝对路径 + 尚不存在或空目录（绝不覆盖已有库/身份）。
        let dir_path = PathBuf::from(&dir);
        if let Err(message) = validate_instance_directory(&dir_path) {
            fail(self, message, cx);
            return;
        }
        // 2) 显式同意开发同步（与 v1 同一道门禁）。
        if !consent {
            fail(
                self,
                "请先勾选「同意开发同步」：这是 localhost 开发验证，不是生产跨设备安全同步"
                    .to_owned(),
                cx,
            );
            return;
        }
        let vault_path = dir_path.join(VAULT_FILE_NAME);
        // 3) 目录锁 → 票据离开 UI 内存（移动到 worker）。
        let lock = match InstanceLock::acquire(&dir_path) {
            Ok(lock) => lock,
            Err(error) => {
                fail(self, error.to_string(), cx);
                return;
            }
        };
        let Some(link) = self.form.as_mut().and_then(|form| form.link_state.take()) else {
            drop(lock);
            return;
        };
        let CreateLinkState {
            mode,
            relay,
            vault_id,
            vault_id_hex,
            ticket,
            pin,
        } = link;
        let request = CreateRequest {
            vault_path,
            relay_url: relay,
            relay_mode: mode,
            credential: CreateCredential::Ticket {
                vault_id,
                ticket,
                expected_instance: pin,
            },
            lock,
            test_extra_roots: self.test_relay_roots.clone(),
        };
        let job = match CreateJob::start(request) {
            Ok(job) => job,
            Err(error) => {
                fail(
                    self,
                    format!(
                        "无法启动后台创建线程：{error}；票据已从界面移除（不持久化），\
                         请重新粘贴创建链接"
                    ),
                    cx,
                );
                return;
            }
        };
        self.job = Some(job);
        self.working = Some(WorkingState {
            kind: WorkingKind::Create,
            stage: None,
            cancel_requested: false,
            started: Instant::now(),
            budget_notice_shown: false,
        });
        self.state = State::Working;
        self.notice = Some(format!(
            "正在用一次性票据创建 vault（vault_id {vault_id_hex}，v2 面；票据不写配置/日志）…"
        ));
        self.spawn_job_pump(cx);
        cx.notify();
    }

    /// 提交 Join 表单（按钮与 Enter 的同一入口）。
    ///
    /// 校验顺序（全部在 UI 线程，任何一步失败都**不联网**）：relay 是 loopback、
    /// `vault_id` 是 32 位十六进制、恢复码非空、实例目录可用（与 Create 共用）、
    /// 目录锁可获取。之后才把请求（含锁与恢复码）交给 worker。
    pub fn submit_join(&mut self, cx: &mut Context<Self>) {
        // Working 中不重复提交（终态前禁止重试）。
        if self.state != State::JoinForm {
            return;
        }
        let Some(form) = &self.join_form else {
            return;
        };
        let relay = form.relay.read(cx).text().trim().to_owned();
        let mode = form.mode;
        let remote_consent = form.remote_consent;
        let vault_id_text = form.vault_id.read(cx).text().trim().to_owned();
        // 恢复码是秘密：只在这个局部缓冲里存在，交给 worker 后立刻丢弃/清空输入框。
        let recovery_code = Zeroizing::new(form.recovery_code.read(cx).text().trim().to_owned());
        // 邀请链接（可选，含一次性 token）：非空时走邀请加入，忽略 relay/vault/恢复码。
        let invite_text = Zeroizing::new(form.invite.read(cx).text().trim().to_owned());
        let dir = form.dir.read(cx).text().trim().to_owned();
        if let Some(form) = &mut self.join_form {
            form.error = None;
        }
        let fail = |root: &mut Self, message: String, cx: &mut Context<Self>| {
            if let Some(form) = &mut root.join_form {
                form.error = Some(message);
            }
            cx.notify();
        };
        // 0) 邀请链接优先：它自带 relay/vault/pin/token，**不需要**恢复码。
        if !invite_text.trim().is_empty() {
            self.submit_invite(invite_text.as_str(), &dir, cx);
            return;
        }
        // 0b) remote_tls 必须显式确认"远程服务器/HTTPS"（ADR 0013 §1）。
        if mode == RelayMode::RemoteTls && !remote_consent {
            fail(
                self,
                "请先勾选「我确认要连接远程服务器（HTTPS）」：remote_tls 不是默认模式，确认端点 ≠ 授权接受服务器任意 hash"
                    .to_owned(),
                cx,
            );
            return;
        }
        // 1) relay 必须是本模式允许的 canonical origin（同一套权威 parser）。
        if let Err(error) = validate_relay_edit(&relay, mode) {
            fail(self, error, cx);
            return;
        }
        // 2) vault_id 必须是 32 位十六进制（错误只报长度，不回显输入）。
        let vault_id = match parse_vault_id_input(&vault_id_text) {
            Ok(vault_id) => vault_id,
            Err(message) => {
                fail(self, message, cx);
                return;
            }
        };
        // 3) 恢复码必填（格式由 worker 统一判：Bech32m 校验不在 UI 做，避免两套规则）。
        if recovery_code.is_empty() {
            fail(
                self,
                "请粘贴或输入恢复码（mnrec1…；遮罩输入只进内存，不写配置/日志）".to_owned(),
                cx,
            );
            return;
        }
        // 4) 实例目录：与 Create 共用同一套校验（绝对路径 + 尚不存在或空目录）。
        let dir_path = PathBuf::from(&dir);
        if let Err(message) = validate_instance_directory(&dir_path) {
            fail(self, message, cx);
            return;
        }
        let vault_path = dir_path.join(VAULT_FILE_NAME);
        // 5) 目录锁（与 Create 共用）：冲突明确报错，不删除用户目录、不偷别人的锁。
        let lock = match InstanceLock::acquire(&dir_path) {
            Ok(lock) => lock,
            Err(error) => {
                fail(self, error.to_string(), cx);
                return;
            }
        };
        let request = JoinRequest {
            vault_path,
            relay_url: relay,
            relay_mode: mode,
            vault_id,
            vault_id_hex: menghunote_sync::ids::hex_of(&vault_id),
            recovery_code,
            lock,
            test_extra_roots: self.test_relay_roots.clone(),
        };
        let job = match JoinJob::start(request) {
            Ok(job) => job,
            Err(error) => {
                fail(self, format!("无法启动后台加入线程：{error}"), cx);
                return;
            }
        };
        // 恢复码已交给 worker：立刻清空输入框（不在 UI 里继续持有正文）。
        if let Some(form) = &mut self.join_form {
            form.recovery_code.update(cx, |input, cx| {
                input.clear();
                cx.notify();
            });
        }
        self.join_job = Some(job);
        self.working = Some(WorkingState {
            kind: WorkingKind::Join,
            stage: None,
            cancel_requested: false,
            started: Instant::now(),
            budget_notice_shown: false,
        });
        self.state = State::Working;
        self.notice = None;
        self.spawn_job_pump(cx);
        cx.notify();
    }

    /// 邀请加入（ADR 0014 S1）：解析链接 → 兑换 PENDING（或 resume）→ 等待批准页。
    ///
    /// 校验顺序（全部在 UI 线程，任何一步失败都**不联网**）：链接可解析（严格 host/
    /// scheme/vault/iid/pin/token）、实例目录可用（与 Create/Join 同一套）、目录锁可获取。
    /// 链接交给 worker 后**立刻清空输入框**（含 token，不在 UI 里继续持有）。
    fn submit_invite(&mut self, text: &str, dir: &str, cx: &mut Context<Self>) {
        if let Some(form) = &mut self.join_form {
            form.error = None;
        }
        let fail = |root: &mut Self, message: String, cx: &mut Context<Self>| {
            if let Some(form) = &mut root.join_form {
                form.error = Some(message);
            }
            cx.notify();
        };
        let target = match menghunote_sync::invite_link::InviteLinkTarget::parse(text) {
            Ok(target) => target,
            Err(error) => {
                fail(
                    self,
                    format!("邀请链接无效：{error}（没有联网，也没有创建任何文件）"),
                    cx,
                );
                return;
            }
        };
        let dir_path = PathBuf::from(dir);
        let vault_path = dir_path.join(VAULT_FILE_NAME);
        // 目录校验：普通情况必须"不存在或空目录"；**唯一例外**是本邀请自己的 PENDING
        // 续跑（重启/丢回应后重贴同一链接，不重复兑换、不覆盖别的库）。
        let mut resumable = false;
        if let Err(message) = validate_instance_directory(&dir_path) {
            resumable = vault_path.exists()
                && menghunote_storage::Vault::open(&vault_path)
                    .ok()
                    .and_then(|vault| {
                        menghunote_sync::invite_link::InviteJoin::load(&vault)
                            .ok()
                            .flatten()
                    })
                    .is_some_and(|pending| {
                        pending.vault_id == target.vault_id()
                            && pending.invite_id == target.invite_id()
                            && pending.relay == target.relay()
                            && pending.pin == target.pin()
                    });
            if !resumable {
                fail(self, message, cx);
                return;
            }
        }
        let _ = resumable;
        let lock = match InstanceLock::acquire(&dir_path) {
            Ok(lock) => lock,
            Err(error) => {
                fail(self, error.to_string(), cx);
                return;
            }
        };
        let request = InviteRedeemRequest {
            target,
            vault_path,
            lock,
            test_extra_roots: self.test_relay_roots.clone(),
        };
        let job = match InviteJob::start_redeem(request) {
            Ok(job) => job,
            Err(error) => {
                fail(self, format!("无法启动后台邀请线程：{error}"), cx);
                return;
            }
        };
        // 链接（含 token）已交给 worker：立刻清空输入框，不在 UI 里继续持有。
        if let Some(form) = &mut self.join_form {
            form.invite.update(cx, |input, cx| {
                input.clear();
                cx.notify();
            });
        }
        self.invite_job = Some(job);
        self.working = Some(WorkingState {
            kind: WorkingKind::Invite,
            stage: None,
            cancel_requested: false,
            started: Instant::now(),
            budget_notice_shown: false,
        });
        self.invite = None;
        self.state = State::Working;
        self.notice = None;
        self.spawn_job_pump(cx);
        cx.notify();
    }

    /// 「检查状态」：查创建者是否已批准；批准后 bootstrap 并进 Main。
    ///
    /// 不会重复兑换（pending 已 durable）；检查失败保留等待页可重试。
    pub fn check_invite(&mut self, cx: &mut Context<Self>) {
        let Some(invite) = self.invite.as_ref() else {
            return;
        };
        if invite.busy {
            return;
        }
        let facts = invite.facts.clone();
        let lock = match self.invite.as_mut().and_then(|invite| invite.lock.take()) {
            Some(lock) => lock,
            None => {
                let instance_dir = facts.vault_path.parent().map_or_else(
                    || PathBuf::from(&facts.vault_path),
                    std::path::Path::to_path_buf,
                );
                match InstanceLock::acquire(&instance_dir) {
                    Ok(lock) => lock,
                    Err(error) => {
                        if let Some(invite) = &mut self.invite {
                            invite.message = Some(format!("无法获取实例目录锁：{error}"));
                        }
                        cx.notify();
                        return;
                    }
                }
            }
        };
        // 锁所有权交给 worker；失败路径 worker 释放它（下次检查自动重新获取）。
        let request = InviteCheckRequest {
            vault_path: PathBuf::from(&facts.vault_path),
            lock,
            relay_mode: facts.relay_mode,
            api_version: facts.api_version,
            relay_url: facts.relay_url.clone(),
            test_extra_roots: self.test_relay_roots.clone(),
        };
        match InviteJob::start_check(request) {
            Ok(job) => {
                self.invite_job = Some(job);
                if let Some(invite) = &mut self.invite {
                    invite.busy = true;
                    invite.message = Some("正在检查批准状态…".to_owned());
                }
                self.spawn_job_pump(cx);
            }
            Err(error) => {
                if let Some(invite) = &mut self.invite {
                    invite.message = Some(format!("无法启动后台检查线程：{error}"));
                }
            }
        }
        cx.notify();
    }

    /// 离开等待页（锁随状态 drop）：pending 已 durable，重贴同一链接可继续。
    pub fn back_from_invite(&mut self, cx: &mut Context<Self>) {
        self.invite_job = None;
        self.invite = None;
        self.state = State::Welcome;
        self.notice = Some(
            "已离开等待页：PENDING 已 durable 保存；再次粘贴同一邀请链接可继续（不会重复兑换）"
                .to_owned(),
        );
        cx.notify();
    }

    /// 后台 Create/Join 的事件泵（40ms；非阻塞 `try_next`）。
    fn spawn_job_pump(&mut self, cx: &mut Context<Self>) {
        if self.pump_started {
            return;
        }
        self.pump_started = true;
        let timers = cx.background_executor().clone();
        cx.spawn(async move |this, cx| loop {
            timers.timer(CREATE_PUMP_INTERVAL).await;
            let alive = this.update(cx, |root, cx| {
                root.poll_working(cx);
                root.job.is_some() || root.join_job.is_some() || root.invite_job.is_some()
            });
            match alive {
                Ok(true) => {}
                // 视图释放（窗口关闭）或本次操作已收尾：结束泵。
                _ => break,
            }
        })
        .detach();
    }

    /// 排空 channel 里当前可用的事件，并按操作预算给一次**逻辑超时提示**。
    ///
    /// 逻辑超时 ≠ 停止：worker 仍在跑（`DevLocal::create/join` 不可中断），终态前不释放
    /// 锁、不允许重复提交；提示只是告诉用户"比预期久，仍在等真实收尾"。
    fn poll_working(&mut self, cx: &mut Context<Self>) {
        let mut create_events = Vec::new();
        if let Some(job) = &self.job {
            while let Some(event) = job.try_next() {
                create_events.push(event);
            }
        }
        let mut join_events = Vec::new();
        if let Some(job) = &self.join_job {
            while let Some(event) = job.try_next() {
                join_events.push(event);
            }
        }
        let mut invite_events = Vec::new();
        if let Some(job) = &self.invite_job {
            while let Some(event) = job.try_next() {
                invite_events.push(event);
            }
        }
        for event in create_events {
            self.apply_create_event(event, cx);
        }
        for event in join_events {
            self.apply_join_event(event, cx);
        }
        for event in invite_events {
            self.apply_invite_event(event, cx);
        }
        if self.state == State::Working {
            if let Some(working) = &mut self.working {
                if !working.budget_notice_shown
                    && working.started.elapsed() >= self.options.operation_budget
                {
                    working.budget_notice_shown = true;
                    self.notice = Some(format!(
                        "已超过 {}s 操作预算：worker 仍在等真实收尾（网络/写盘不可中断），\
                         不会重复提交；成功仍会继续（Create 显示恢复码 / Join 进入主界面）。",
                        self.options.operation_budget.as_secs()
                    ));
                    cx.notify();
                }
            }
        }
    }

    /// 测试专用：**阻塞**等待 Create/Join 终态并应用（显式收敛点，不是 sleep）。
    ///
    /// 生产 UI 走定时泵（[`Self::spawn_job_pump`]）；headless 测试需要确定性收敛点，
    /// 因此这里直接阻塞在 channel 上（有超时），与面板/服务测试"等明确终态"的纪律一致。
    pub fn wait_for_working_terminal(&mut self, cx: &mut Context<Self>, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            let mut create_events = Vec::new();
            if let Some(job) = &self.job {
                while let Some(event) = job.try_next() {
                    create_events.push(event);
                }
            }
            let mut join_events = Vec::new();
            if let Some(job) = &self.join_job {
                while let Some(event) = job.try_next() {
                    join_events.push(event);
                }
            }
            if !create_events.is_empty() || !join_events.is_empty() {
                for event in create_events {
                    self.apply_create_event(event, cx);
                }
                for event in join_events {
                    self.apply_join_event(event, cx);
                }
                continue;
            }
            if self.job.is_none() && self.join_job.is_none() {
                return true;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            // 同一时刻只有一种后台操作（Create 与 Join 互斥）。
            if self.job.is_some() {
                let received = match &self.job {
                    Some(job) => job.recv(remaining),
                    None => None,
                };
                match received {
                    Some(event) => self.apply_create_event(event, cx),
                    None => return self.job.is_none() && self.join_job.is_none(),
                }
            } else if self.join_job.is_some() {
                let received = match &self.join_job {
                    Some(job) => job.recv(remaining),
                    None => None,
                };
                match received {
                    Some(event) => self.apply_join_event(event, cx),
                    None => return self.job.is_none() && self.join_job.is_none(),
                }
            } else {
                return true;
            }
        }
    }

    /// 测试专用：等 Create 终态（与 [`Self::wait_for_working_terminal`] 同一条路径）。
    pub fn wait_for_create_terminal(&mut self, cx: &mut Context<Self>, timeout: Duration) -> bool {
        self.wait_for_working_terminal(cx, timeout)
    }

    /// 测试专用：等 Join 终态（与 [`Self::wait_for_working_terminal`] 同一条路径）。
    pub fn wait_for_join_terminal(&mut self, cx: &mut Context<Self>, timeout: Duration) -> bool {
        self.wait_for_working_terminal(cx, timeout)
    }

    fn apply_create_event(&mut self, event: CreateEvent, cx: &mut Context<Self>) {
        match event {
            CreateEvent::Progress(stage) => {
                if let Some(working) = &mut self.working {
                    working.stage = Some(stage.label());
                }
            }
            CreateEvent::Finished(outcome) => {
                self.job = None;
                self.pump_started = false;
                let cancelled = self
                    .working
                    .take()
                    .is_some_and(|working| working.cancel_requested);
                let instance_id = self
                    .form
                    .as_ref()
                    .map_or_else(String::new, |form| form.instance_id.clone());
                let entry = outcome.public_facts(&instance_id);
                self.pending = Some(PendingCreate {
                    outcome,
                    entry,
                    confirmed: false,
                    copied: false,
                });
                self.state = State::BackupRequired;
                self.error = None;
                self.notice = if cancelled {
                    Some(
                        "已请求取消，但 Create 已真实成功：恢复码必须现在备份（离开后不再显示）。"
                            .to_owned(),
                    )
                } else {
                    None
                };
            }
            CreateEvent::Failed { message, security } => {
                self.job = None;
                self.pump_started = false;
                let cancelled = self
                    .working
                    .take()
                    .is_some_and(|working| working.cancel_requested);
                if cancelled {
                    self.state = State::Welcome;
                    self.notice = Some(format!("已取消；Create 未完成：{message}"));
                } else {
                    let mut public = self.form_public_fields(cx);
                    let (title, message) = security_error_text("创建失败", security, message);
                    if let Some(category) = security {
                        public.push(("安全类别".to_owned(), category.label().to_owned()));
                    }
                    self.set_error(&title, message, public, RetryKind::CreateForm);
                }
            }
        }
        cx.notify();
    }

    /// 邀请加入的事件处理（兑换 PENDING / 检查批准 / 完成）。
    fn apply_invite_event(&mut self, event: InviteEvent, cx: &mut Context<Self>) {
        match event {
            InviteEvent::Progress(stage) => {
                if let Some(working) = &mut self.working {
                    working.stage = Some(stage.label());
                }
                if let Some(invite) = &mut self.invite {
                    invite.message = Some(stage.label().to_owned());
                }
            }
            InviteEvent::Pending { facts, lock } => {
                self.invite_job = None;
                self.pump_started = false;
                self.working = None;
                let instance_id = self.invite.as_ref().map_or_else(
                    || {
                        self.options
                            .instance_id
                            .clone()
                            .unwrap_or_else(generate_instance_id)
                    },
                    |invite| invite.instance_id.clone(),
                );
                let resumed = facts.resumed;
                self.invite = Some(InvitePendingState {
                    instance_id,
                    facts: *facts,
                    lock: Some(lock),
                    busy: false,
                    message: None,
                });
                self.state = State::InvitePending;
                self.error = None;
                self.notice = Some(if resumed {
                    "已从 durable pending 恢复（没有重复兑换）：仍然等待创建者批准".to_owned()
                } else {
                    "已兑换为待批准设备：请让创建者在设备管理里核对指纹并批准".to_owned()
                });
            }
            InviteEvent::Waiting { lock } => {
                self.invite_job = None;
                self.pump_started = false;
                if let Some(invite) = &mut self.invite {
                    invite.lock = Some(lock);
                    invite.busy = false;
                    invite.message = Some(
                        "创建者尚未批准（relay 对 pending/已移除不区分）：请在创建端设备面板点「批准…」后再检查"
                            .to_owned(),
                    );
                }
                self.notice = Some("检查完成：仍未批准（pending 保留，可稍后再查）".to_owned());
            }
            InviteEvent::Finished(outcome) => {
                self.invite_job = None;
                self.pump_started = false;
                let instance_id = self.invite.as_ref().map_or_else(
                    || {
                        self.options
                            .instance_id
                            .clone()
                            .unwrap_or_else(generate_instance_id)
                    },
                    |invite| invite.instance_id.clone(),
                );
                self.commit_invite(outcome, instance_id, cx);
            }
            InviteEvent::Failed {
                kind,
                message,
                security,
            } => {
                self.invite_job = None;
                self.pump_started = false;
                let checking = self.state == State::InvitePending;
                let text = match security {
                    Some(category) => format!(
                        "安全暂停（{}）：{}；{message}",
                        category.label(),
                        category.hint()
                    ),
                    None => message,
                };
                if checking {
                    // 检查失败：保留等待页与 pending，可重试（不重新兑换）。
                    if let Some(invite) = &mut self.invite {
                        invite.busy = false;
                        invite.message = Some(text.clone());
                    }
                    self.notice = Some(format!("检查失败（未重新兑换）：{}", kind.hint()));
                } else {
                    // 兑换失败：回 Join 表单（公开字段保留；秘密需重输）。
                    self.working = None;
                    self.invite = None;
                    self.state = State::JoinForm;
                    if let Some(form) = &mut self.join_form {
                        form.error = Some(text);
                    }
                    self.pending_focus = true;
                }
            }
        }
        cx.notify();
    }

    /// 邀请 bootstrap 完成后的落库顺序：写配置（active）→ 清 pending → 进 Main。
    fn commit_invite(
        &mut self,
        outcome: Box<InviteOutcome>,
        instance_id: String,
        cx: &mut Context<Self>,
    ) {
        let entry = outcome.public_facts(&instance_id);
        let mut profile = self.profile.clone().unwrap_or_default();
        profile.upsert_active(entry.clone());
        if let Err(error) = self.store.save(&profile) {
            let message = format!(
                "配置保存失败：{error}；实例已加入（库/身份都在）。请点「重试保存」，\
                 不要重新粘贴邀请——重新兑换会失败（INVITE_USED），已完成的加入不会再跑一遍。"
            );
            self.pending_invite = Some((outcome, instance_id));
            self.set_error(
                "配置保存失败（可重试）",
                message,
                public_fields(&entry),
                RetryKind::SaveProfile,
            );
            cx.notify();
            return;
        }
        self.profile = Some(profile);
        // 配置已 durable：清掉 pending（失败只影响下次 resume，不影响本实例可用）。
        let warning = match menghunote_sync::invite_link::InviteJoin::complete(&outcome.vault) {
            Ok(()) => outcome.warning.clone(),
            Err(error) => Some(format!(
                "配置已写入，但清理邀请 pending 失败（{error}）：本实例可用，重启后不会再重复加入"
            )),
        };
        self.enter_main_invited(outcome, entry, warning, cx);
    }

    /// 「重试保存」在邀请加入之后的路径：**只重写配置**，不重新联网/不重新 bootstrap。
    fn retry_save_invite(&mut self, cx: &mut Context<Self>) {
        let Some((outcome, instance_id)) = self.pending_invite.take() else {
            self.back_to_welcome(cx);
            return;
        };
        self.commit_invite(outcome, instance_id, cx);
    }

    /// 进入 Main（邀请加入成功）。
    fn enter_main_invited(
        &mut self,
        outcome: Box<InviteOutcome>,
        entry: InstanceEntry,
        warning: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let InviteOutcome {
            vault,
            lock,
            vault_path,
            peer_id,
            key_epoch,
            ..
        } = *outcome;
        let base =
            "已通过邀请加入原实例：主界面显示的是该 vault 现有内容（不播种演示内容）。".to_owned();
        self.enter_main_with_shell(
            vault, lock, vault_path, peer_id, key_epoch, entry, base, warning, cx,
        );
    }

    /// 三个入口（Create/Join/邀请）共用的 Main 构造（不复制 AppShell 装配）。
    #[allow(clippy::too_many_arguments)]
    fn enter_main_with_shell(
        &mut self,
        vault: menghunote_storage::Vault,
        lock: InstanceLock,
        vault_path: PathBuf,
        peer_id: u64,
        key_epoch: u64,
        entry: InstanceEntry,
        base_notice: String,
        warning: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let label = entry.short_label();
        let config = DevSyncConfig {
            vault: vault_path,
            relay_mode: entry.relay_mode,
            service: ServiceConfig::new_for(entry.relay_mode, entry.relay_url.clone())
                .with_api_version(entry.api_version),
        };
        let sync = SyncContext::new(peer_id, key_epoch);
        let shell = cx.new(move |cx| {
            AppShell::new_with_dev_sync_with_autosync(vault, label, config, sync, cx)
        });
        self.main = Some(shell);
        self.lock = Some(lock);
        self.instance = Some(entry);
        self.state = State::Main;
        self.error = None;
        self.join_form = None;
        self.invite = None;
        self.pending_join = None;
        self.notice = Some(match warning {
            Some(warning) => format!("{base_notice} 注意：{warning}"),
            None => base_notice,
        });
    }

    /// 测试专用：**阻塞**等待邀请加入的下一个明确收敛点（PENDING/等待/完成/失败）。
    ///
    /// 与 [`Self::wait_for_working_terminal`] 同一纪律：阻塞在 worker channel 上（有超时），
    /// 不用 sleep、不用"事件队列为空"当判据。
    pub fn wait_for_invite_terminal(&mut self, cx: &mut Context<Self>, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            let mut events = Vec::new();
            if let Some(job) = &self.invite_job {
                while let Some(event) = job.try_next() {
                    events.push(event);
                }
            }
            if !events.is_empty() {
                for event in events {
                    self.apply_invite_event(event, cx);
                }
                if self.invite_job.is_none() && !self.invite_busy() {
                    return true;
                }
                continue;
            }
            if self.invite_job.is_none() {
                return true;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let received = match &self.invite_job {
                Some(job) => job.recv(remaining),
                None => None,
            };
            match received {
                Some(event) => self.apply_invite_event(event, cx),
                None => return self.invite_job.is_none(),
            }
        }
    }

    /// 等待页是否有检查任务在途（测试收敛判据）。
    #[must_use]
    pub fn invite_busy(&self) -> bool {
        self.invite.as_ref().is_some_and(|invite| invite.busy)
    }

    /// 等待页的公开事实（测试断言用；不含 token）。
    #[must_use]
    pub fn invite_pending_facts(&self) -> Option<&InvitePendingFacts> {
        self.invite.as_ref().map(|invite| &invite.facts)
    }

    /// 等待页最近一次检查结果/错误（测试断言用）。
    #[must_use]
    pub fn invite_message(&self) -> Option<&str> {
        self.invite
            .as_ref()
            .and_then(|invite| invite.message.as_deref())
    }

    /// Join 成功的处理：**先写公开配置**，再进 Main。
    ///
    /// 与 Create 的差别：没有恢复码需要备份，所以没有确认页；但配置写失败同样进
    /// Error（`RetryKind::SaveProfile`），**重试只重写配置、不再 Join**（重跑会再造
    /// 一个设备身份）。
    fn apply_join_event(&mut self, event: JoinEvent, cx: &mut Context<Self>) {
        match event {
            JoinEvent::Progress(stage) => {
                if let Some(working) = &mut self.working {
                    working.stage = Some(stage.label());
                }
            }
            JoinEvent::Finished(outcome) => {
                self.join_job = None;
                self.pump_started = false;
                let cancelled = self
                    .working
                    .take()
                    .is_some_and(|working| working.cancel_requested);
                let instance_id = self
                    .join_form
                    .as_ref()
                    .map_or_else(String::new, |form| form.instance_id.clone());
                let entry = outcome.public_facts(&instance_id);
                let pending = PendingJoin {
                    outcome,
                    entry,
                    cancelled,
                };
                self.commit_join(pending, cx);
            }
            JoinEvent::Failed {
                message, security, ..
            } => {
                self.join_job = None;
                self.pump_started = false;
                let cancelled = self
                    .working
                    .take()
                    .is_some_and(|working| working.cancel_requested);
                if cancelled {
                    // 取消后 worker 才失败：不再停在表单，回 Welcome 并说明"未完成"。
                    self.join_form = None;
                    self.state = State::Welcome;
                    self.notice = Some(format!("已取消；Join 未完成：{message}"));
                } else {
                    // 失败保留表单与公开字段（relay/vault_id/目录）；恢复码已清空需重输。
                    let message = match security {
                        Some(category) => format!(
                            "安全暂停（{}）：{}；{}
{message}",
                            category.label(),
                            category.hint(),
                            "不会自动重试/清 pin/信任"
                        ),
                        None => message,
                    };
                    if let Some(form) = &mut self.join_form {
                        form.error = Some(message);
                    }
                    self.state = State::JoinForm;
                    self.pending_focus = true;
                }
                self.error = None;
            }
        }
        cx.notify();
    }

    /// Join 成功后的落库顺序：写配置（active）→ 进 Main；写失败留在 Error 可重试保存。
    fn commit_join(&mut self, pending: PendingJoin, cx: &mut Context<Self>) {
        let mut profile = self.profile.clone().unwrap_or_default();
        profile.upsert_active(pending.entry.clone());
        if let Err(error) = self.store.save(&profile) {
            let message = format!(
                "配置保存失败：{error}；实例已加入（库/身份都在）。请点「重试保存」，\
                 不要重新加入——重跑 Join 会再造一个设备身份。"
            );
            let entry = pending.entry.clone();
            self.pending_join = Some(pending);
            self.set_error(
                "配置保存失败（可重试）",
                message,
                public_fields(&entry),
                RetryKind::SaveProfile,
            );
            cx.notify();
            return;
        }
        self.profile = Some(profile);
        self.enter_main_joined(pending, cx);
        cx.notify();
    }

    /// 进入 Main（Join 成功）：用 worker 交回的真实 Vault + 身份事实构造 `AppShell`。
    ///
    /// 不播种演示树；`AppShell` 构造期自动打开第一篇（A 的笔记）。
    fn enter_main_joined(&mut self, pending: PendingJoin, cx: &mut Context<Self>) {
        let PendingJoin {
            outcome,
            entry,
            cancelled,
        } = pending;
        let JoinOutcome {
            vault,
            lock,
            vault_path,
            peer_id,
            key_epoch,
            warning,
            ..
        } = *outcome;
        let label = entry.short_label();
        let config = DevSyncConfig {
            vault: vault_path,
            relay_mode: entry.relay_mode,
            service: ServiceConfig::new_for(entry.relay_mode, entry.relay_url.clone())
                .with_api_version(entry.api_version),
        };
        let sync = SyncContext::new(peer_id, key_epoch);
        let shell = cx.new(move |cx| {
            AppShell::new_with_dev_sync_with_autosync(vault, label, config, sync, cx)
        });
        self.main = Some(shell);
        self.lock = Some(lock);
        self.instance = Some(entry);
        self.state = State::Main;
        self.error = None;
        self.join_form = None;
        self.pending_join = None;
        let base = if cancelled {
            "已请求取消，但 Join 已真实成功：已写入配置并进入主界面（不丢这份实例）。".to_owned()
        } else {
            "已加入原实例：主界面显示的是该 vault 现有内容（不播种演示内容）。".to_owned()
        };
        self.notice = Some(match warning {
            Some(warning) => format!("{base} 注意：{warning}"),
            None => base,
        });
    }

    /// Error 页的「重试保存」在 Join 成功之后的分支：**只重写配置**，不重新 Join。
    fn retry_save_join(&mut self, cx: &mut Context<Self>) {
        let Some(pending) = self.pending_join.take() else {
            self.back_to_welcome(cx);
            return;
        };
        self.commit_join(pending, cx);
    }

    fn form_public_fields(&self, cx: &App) -> Vec<(String, String)> {
        let Some(form) = &self.form else {
            return Vec::new();
        };
        let mut fields = vec![
            ("实例 id".to_owned(), form.instance_id.clone()),
            (
                "relay".to_owned(),
                form.link_state.as_ref().map_or_else(
                    || form.relay.read(cx).text().trim().to_owned(),
                    |link| link.relay.clone(),
                ),
            ),
            (
                "实例目录".to_owned(),
                form.dir.read(cx).text().trim().to_owned(),
            ),
        ];
        if let Some(link) = &form.link_state {
            fields.push(("vault_id".to_owned(), link.vault_id_hex.clone()));
            fields.push(("API 面".to_owned(), "v2（一次性票据）".to_owned()));
        }
        fields
    }

    // ---- 备份页 ----

    /// 勾选/取消"已在应用之外备份恢复码"。
    pub fn set_backup_confirmed(&mut self, confirmed: bool, cx: &mut Context<Self>) {
        if let Some(pending) = &mut self.pending {
            pending.confirmed = confirmed;
        }
        cx.notify();
    }

    /// 复制恢复码（**显式动作** + 风险提示；不自动复制）。
    pub fn copy_recovery_code(&mut self, cx: &mut Context<Self>) {
        let Some(pending) = &self.pending else {
            return;
        };
        let code = pending.outcome.recovery_code.as_str().to_owned();
        cx.write_to_clipboard(ClipboardItem::new_string(code));
        if let Some(pending) = &mut self.pending {
            pending.copied = true;
        }
        self.notice = Some(
            "已复制：剪贴板历史/云同步可能保留副本；备份完请清空剪贴板。\
             恢复码拥有数据/管理恢复能力，只显示这一次。"
                .to_owned(),
        );
        cx.notify();
    }

    /// 「我已备份，继续」：写公开配置（active）→ 进入真实 Main。
    ///
    /// 配置写失败 → Error（`RetryKind::SaveProfile`）：**不重跑 Create**，重试只重写配置。
    pub fn confirm_backup(&mut self, cx: &mut Context<Self>) {
        let Some(pending) = &self.pending else {
            return;
        };
        if !pending.confirmed {
            self.notice = Some("请先勾选「已在应用之外备份恢复码」".to_owned());
            cx.notify();
            return;
        }
        let entry = pending.entry.clone();
        let mut profile = self.profile.clone().unwrap_or_default();
        profile.upsert_active(entry.clone());
        if let Err(error) = self.store.save(&profile) {
            let message = format!(
                "配置保存失败：{error}；实例已创建（库/身份都在）。请点「重试保存」，\
                 不要重新创建——重跑 Create 会生成新实例，恢复码也会不同。"
            );
            self.set_error(
                "配置保存失败（可重试）",
                message,
                public_fields(&entry),
                RetryKind::SaveProfile,
            );
            cx.notify();
            return;
        }
        self.profile = Some(profile);
        let pending = self.pending.take().expect("上面已确认 pending 存在");
        self.enter_main(pending, cx);
        cx.notify();
    }

    /// 进入 Main：用 worker 交回的真实 Vault + 身份事实构造 `AppShell`。
    fn enter_main(&mut self, pending: PendingCreate, cx: &mut Context<Self>) {
        let PendingCreate { outcome, entry, .. } = pending;
        let CreateOutcome {
            vault,
            lock,
            vault_path,
            peer_id,
            key_epoch,
            recovery_code,
            warning,
            ..
        } = *outcome;
        // 恢复码在进入 Main 时清除（Zeroizing 在 drop 时清零；Main 不再持有）。
        drop(recovery_code);
        let label = entry.short_label();
        let config = DevSyncConfig {
            vault: vault_path,
            relay_mode: entry.relay_mode,
            service: ServiceConfig::new_for(entry.relay_mode, entry.relay_url.clone())
                .with_api_version(entry.api_version),
        };
        let sync = SyncContext::new(peer_id, key_epoch);
        let shell = cx.new(move |cx| {
            AppShell::new_with_dev_sync_with_autosync(vault, label, config, sync, cx)
        });
        self.main = Some(shell);
        self.lock = Some(lock);
        self.instance = Some(entry);
        self.state = State::Main;
        self.error = None;
        self.notice = Some(match warning {
            Some(warning) => format!(
                "实例已创建。左侧「＋ 笔记」建第一篇，保存后可在右上同步面板点「发布基线」。\
                 注意：{warning}"
            ),
            None => "实例已创建。左侧「＋ 笔记」建第一篇，保存后可在右上同步面板点「发布基线」。"
                .to_owned(),
        });
    }

    // ---- Working / Error 动作 ----

    /// 「取消」后台操作：**只停止自动继续**，不 join 线程、不中断网络/写盘。
    fn cancel_working(&mut self, cx: &mut Context<Self>) {
        if self.state != State::Working {
            return;
        }
        let is_join = self.working_is_join();
        if let Some(working) = &mut self.working {
            working.cancel_requested = true;
        }
        self.notice = Some(if is_join {
            "已请求取消：worker 不能被中断，等真实收尾后退出。\
             若 Join 已成功，仍会写配置并进入主界面（不能丢这份实例）。"
                .to_owned()
        } else {
            "已请求取消：worker 不能被中断，等真实收尾后退出。\
             若 Create 已成功，恢复码仍会显示（不能丢）。"
                .to_owned()
        });
        cx.notify();
    }

    /// 「取消」后台 Create（保留给既有调用点/测试）。
    pub fn cancel_create(&mut self, cx: &mut Context<Self>) {
        self.cancel_working(cx);
    }

    /// 「取消」后台 Join。
    pub fn cancel_join(&mut self, cx: &mut Context<Self>) {
        self.cancel_working(cx);
    }

    /// Error 页的下一步（按 `RetryKind` 分派）。
    pub fn retry(&mut self, cx: &mut Context<Self>) {
        let Some(error) = &self.error else {
            return;
        };
        let (retry, message) = (error.retry, error.message.clone());
        match retry {
            RetryKind::ReloadConfig => {
                self.main = None;
                self.lock = None;
                self.instance = None;
                self.error = None;
                self.boot(cx);
            }
            RetryKind::OpenInstance => {
                let Some(entry) = self.retry_entry.clone() else {
                    self.back_to_welcome(cx);
                    return;
                };
                self.error = None;
                self.state = State::Opening;
                self.try_open(entry, cx);
            }
            RetryKind::SaveProfile => {
                if self.pending.is_some() {
                    self.confirm_backup(cx);
                } else if self.pending_join.is_some() {
                    self.retry_save_join(cx);
                } else if self.pending_invite.is_some() {
                    self.retry_save_invite(cx);
                } else {
                    self.back_to_welcome(cx);
                }
            }
            RetryKind::CreateForm => {
                self.error = None;
                if let Some(form) = &mut self.form {
                    form.error = Some(message);
                }
                self.state = State::CreateForm;
            }
            RetryKind::None => self.back_to_welcome(cx),
        }
        cx.notify();
    }

    /// 返回 Welcome（不删任何用户文件；丢弃本次未确认的恢复码）。
    pub fn back_to_welcome(&mut self, cx: &mut Context<Self>) {
        self.main = None;
        self.lock = None;
        self.instance = None;
        self.pending = None;
        self.pending_join = None;
        self.form = None;
        self.join_form = None;
        self.error = None;
        self.notice = None;
        self.state = State::Welcome;
        cx.notify();
    }

    /// 关窗分派（`main` 与测试共用）。
    ///
    /// 返回 `true` = 允许关闭；`false` = 拦截（当前只有 Working）。
    pub fn should_close(&mut self, cx: &mut Context<Self>) -> bool {
        match self.state {
            State::Working => {
                let is_join = self.working_is_join();
                self.notice = Some(if is_join {
                    "正在加入，先别关：worker 不能中断，等真实收尾（成功会写配置并进入主界面）。"
                        .to_owned()
                } else {
                    "正在创建，先别关：worker 不能中断，等真实收尾（成功会显示恢复码）。".to_owned()
                });
                cx.notify();
                false
            }
            State::Main => {
                if let Some(shell) = self.main.take() {
                    shell.update(cx, |shell, cx| shell.close_all(cx));
                }
                self.lock = None;
                self.pending = None;
                self.pending_join = None;
                self.instance = None;
                true
            }
            _ => {
                self.main = None;
                self.lock = None;
                self.pending = None;
                self.pending_join = None;
                self.pending_invite = None;
                self.form = None;
                self.join_form = None;
                self.invite = None;
                true
            }
        }
    }
}

/// 公开字段（Error 页展示；只有路径/ID/URL）。
/// 安全类初始化失败的标题/文案（与 autosync 的 `SecurityPaused` 同一措辞口径）。
///
/// 返回 `(title, message)`：`security = None` 时原样返回（普通失败）。
fn security_error_text(
    base: &str,
    security: Option<crate::dev_sync::SecurityCategory>,
    message: String,
) -> (String, String) {
    match security {
        Some(category) => (
            format!("{base}（安全暂停：{}）", category.label()),
            format!(
                "{}\n\n{}；不会自动重试/重建/清 pin/信任。",
                category.hint(),
                message
            ),
        ),
        None => (base.to_owned(), message),
    }
}

fn public_fields(entry: &InstanceEntry) -> Vec<(String, String)> {
    vec![
        ("实例 id".to_owned(), entry.instance_id.clone()),
        ("库文件".to_owned(), entry.vault_path.clone()),
        ("relay".to_owned(), entry.relay_url.clone()),
        ("vault_id".to_owned(), entry.vault_id.clone()),
    ]
}

/// Create/Join 共用的实例目录校验。
///
/// 规则（ADR 0011「数据目录与公开配置」）：
///
/// - 必须是**绝对路径**（真实使用不回退 cwd/tmp）；
/// - 目录必须**尚不存在或为空**（只忽略我们自己可能残留的实例锁文件，那种情况交给
///   [`InstanceLock`] 报"被占用"，比"目录非空"更可诊断）；
/// - 已有 `vault.sqlite3` / `dev-identity.bin` 一律拒绝：不覆盖、不迁移、不删除。
///
/// 错误文案只含路径（公开信息），绝不回显秘密。
fn validate_instance_directory(dir: &Path) -> Result<(), String> {
    if dir.as_os_str().is_empty() || !dir.is_absolute() {
        return Err("实例目录必须是绝对路径（例如 /home/you/... 或 C:\\...）".to_owned());
    }
    let vault_path = dir.join(VAULT_FILE_NAME);
    if vault_path.exists() {
        return Err(format!(
            "该目录已有库文件 {}：Create/Join 只接受全新实例（不覆盖、不迁移）",
            vault_path.display()
        ));
    }
    let identity_path = DevIdentityStore::path_for(&vault_path);
    if identity_path.exists() {
        return Err(format!(
            "该目录已有身份文件 {}：拒绝覆盖（请换新目录或人工确认）",
            identity_path.display()
        ));
    }
    if !dir.exists() {
        return Ok(());
    }
    if !dir.is_dir() {
        return Err(format!("实例目录不是目录：{}", dir.display()));
    }
    let entries = std::fs::read_dir(dir)
        .map_err(|error| format!("读取实例目录失败：{}（{error}）", dir.display()))?;
    for entry in entries {
        let entry =
            entry.map_err(|error| format!("读取实例目录失败：{}（{error}）", dir.display()))?;
        if entry.file_name().to_string_lossy() != LOCK_FILE_NAME {
            return Err(format!(
                "实例目录必须尚不存在或为空目录：{} 里已有 {}；\
                 为避免覆盖用户数据，Create/Join 只接受新目录",
                dir.display(),
                entry.path().display()
            ));
        }
    }
    Ok(())
}

/// 解析 Join 表单里的 `vault_id`（32 位十六进制，大小写皆可）。
///
/// 错误**只报长度/格式**，绝不回显输入：用户可能把恢复码误贴进这个框，回显就等于泄露。
fn parse_vault_id_input(raw: &str) -> Result<[u8; 16], String> {
    if raw.is_empty() {
        return Err("请填写 vault_id（32 位十六进制；创建者的备份页/实例信息里可看到）".to_owned());
    }
    let chars = raw.chars().count();
    if chars != 32 {
        return Err(format!(
            "vault_id 必须是 32 位十六进制（16B 公开标识；实际 {chars} 个字符）"
        ));
    }
    menghunote_sync::ids::parse_hex::<16>(raw)
        .map_err(|_| "vault_id 必须是 32 位十六进制（0-9 / a-f）".to_owned())
}

impl Render for AppRoot {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.pending_focus {
            self.pending_focus = false;
            // Join 失败后最该重输的是恢复码；Create/其它情况聚焦第一个字段。
            let focus_link = std::mem::take(&mut self.focus_create_link);
            let target = match self.state {
                State::JoinForm => self
                    .join_form
                    .as_ref()
                    .map(|form| form.recovery_code.clone()),
                State::CreateForm if focus_link => self.form.as_ref().map(|form| form.link.clone()),
                State::CreateForm => self.form.as_ref().map(|form| form.relay.clone()),
                _ => self.form.as_ref().map(|form| form.relay.clone()),
            };
            if let Some(target) = target {
                window.defer(cx, move |window, cx| {
                    target.update(cx, |input, cx| input.focus(window, cx));
                });
            }
        }
        // 设置面板：打开后把焦点放到 relay 输入框（不抢编辑器的键盘输入）。
        if let Some(settings) = &mut self.settings {
            if settings.focus_pending {
                settings.focus_pending = false;
                let target = settings.relay.clone();
                window.defer(cx, move |window, cx| {
                    target.update(cx, |input, cx| input.focus(window, cx));
                });
            }
        }
        // 关闭设置后把焦点还给 Main 的当前编辑器（同一帧不重建 `AppShell`）。
        if self.pending_focus_restore {
            self.pending_focus_restore = false;
            if let Some(shell) = self.main.clone() {
                window.defer(cx, move |window, cx| {
                    shell.update(cx, |shell, cx| shell.focus_active_editor(window, cx));
                });
            }
        }
        let body: gpui::AnyElement = match self.state {
            State::Welcome => self.render_welcome(cx).into_any_element(),
            State::CreateForm => self.render_create_form(cx).into_any_element(),
            State::JoinForm => self.render_join_form(cx).into_any_element(),
            State::InvitePending => self.render_invite_pending(cx).into_any_element(),
            State::Working => self.render_working(cx).into_any_element(),
            State::BackupRequired => self.render_backup_required(cx).into_any_element(),
            State::Opening => page_column()
                .debug_selector(|| "app-root-opening".to_string())
                .child(title_text("正在打开实例…"))
                .child(banner_text())
                .child(hint_text("校验公开配置 → 加载受保护身份 → 打开真实库。"))
                .into_any_element(),
            State::Main => {
                let shell = self.main.clone();
                // 固定的「设置」入口：右下角、编辑器状态栏上方的小按钮。
                // 只用鼠标事件，不注册焦点/快捷键，因此不会干扰编辑器的键盘输入；
                // 点开后是覆盖层，`AppShell`（文档/滚动/撤销栈）原地保留。
                let settings_entry =
                    div()
                        .absolute()
                        .right(px(12.0))
                        .bottom(px(40.0))
                        .child(action_button(
                            "app-root-settings",
                            "设置",
                            false,
                            false,
                            cx,
                            |root, cx| root.open_settings(cx),
                        ));
                div()
                    .relative()
                    .size_full()
                    .debug_selector(|| "app-root-main".to_string())
                    .when_some(shell, |this, shell| {
                        this.child(div().size_full().child(shell))
                    })
                    .child(settings_entry)
                    .into_any_element()
            }
            State::Error => self.render_error(cx).into_any_element(),
        };
        let settings = self.render_settings(cx);
        div()
            .relative()
            .size_full()
            .debug_selector(|| "app-root".to_string())
            .flex()
            .flex_col()
            .bg(theme::window_bg())
            .font_family(theme::FONT_FAMILY)
            .child(body)
            .when_some(settings, |this, settings| this.child(settings))
    }
}

// ---- 渲染辅助（纯投影；不含副作用） ----

fn page_column() -> gpui::Div {
    div()
        .size_full()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap_3()
        .p(px(32.0))
}

fn content_column() -> gpui::Div {
    div()
        .w(px(640.0))
        .max_w_full()
        .flex()
        .flex_col()
        .gap_2()
        .p(px(18.0))
        .bg(theme::input_bg())
        .border_1()
        .border_color(theme::input_border())
        .rounded_sm()
}

fn title_text(text: impl Into<SharedString>) -> gpui::Div {
    div()
        .text_size(px(20.0))
        .text_color(theme::text_color())
        .child(text.into())
}

fn section_text(text: impl Into<SharedString>) -> gpui::Div {
    div()
        .text_size(px(13.0))
        .text_color(theme::status_bar_text())
        .child(text.into())
}

fn hint_text(text: impl Into<SharedString>) -> gpui::Div {
    div()
        .text_size(px(11.0))
        .text_color(theme::status_bar_text())
        .child(text.into())
}

fn banner_text() -> gpui::Div {
    div()
        .w_full()
        .text_size(px(11.0))
        .text_color(theme::danger_text())
        .child(SharedString::from(
            "DEV ONLY · 开发同步：relay 只允许 localhost，服务端验签放行，非生产、非端到端安全验收",
        ))
}

fn label_text(text: impl Into<SharedString>) -> gpui::Div {
    div()
        .text_size(px(12.0))
        .text_color(theme::text_color())
        .child(text.into())
}

/// 统一样式的动作按钮（可点；`dimmed` 只影响视觉，真正拒绝由业务逻辑给原因）。
fn action_button<F>(
    // 实例行/公开字段的按钮 selector 带下标，动态拼接；固定入口仍传 `&'static str`。
    selector: impl Into<SharedString> + 'static,
    label: impl Into<SharedString>,
    primary: bool,
    dimmed: bool,
    cx: &mut Context<AppRoot>,
    on_click: F,
) -> gpui::Stateful<gpui::Div>
where
    F: Fn(&mut AppRoot, &mut Context<AppRoot>) + 'static,
{
    let color = if dimmed {
        theme::status_bar_text()
    } else if primary {
        theme::link_color()
    } else {
        theme::text_color()
    };
    let selector: SharedString = selector.into();
    let debug_selector = selector.clone();
    let mut element = div()
        .id(selector)
        .debug_selector(move || debug_selector.to_string())
        .px(px(10.0))
        .py(px(5.0))
        .rounded_sm()
        .border_1()
        .border_color(theme::input_border())
        .bg(theme::menu_bg())
        .text_size(px(12.0))
        .text_color(color)
        .cursor_pointer()
        .child(label.into())
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |root, _event: &gpui::MouseDownEvent, _window, cx| {
                on_click(root, cx);
                cx.notify();
                cx.stop_propagation();
            }),
        );
    if dimmed {
        element = element.opacity(0.55);
    }
    element
}

/// 一行复选框（同意 / 已备份）。
fn checkbox_row(
    selector: &'static str,
    checked: bool,
    label: &'static str,
    cx: &mut Context<AppRoot>,
    on_click: impl Fn(&mut AppRoot, &mut Context<AppRoot>) + 'static,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(SharedString::from(selector))
        .debug_selector(move || selector.to_string())
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .cursor_pointer()
        .child(
            div()
                .w(px(16.0))
                .h(px(16.0))
                .flex()
                .items_center()
                .justify_center()
                .border_1()
                .border_color(theme::input_border())
                .bg(theme::input_bg())
                .text_size(px(12.0))
                .text_color(theme::text_color())
                .child(SharedString::from(if checked { "✓" } else { " " })),
        )
        .child(
            div()
                .flex_1()
                .text_size(px(12.0))
                .text_color(theme::text_color())
                .child(SharedString::from(label)),
        )
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |root, _event: &gpui::MouseDownEvent, _window, cx| {
                on_click(root, cx);
                cx.notify();
                cx.stop_propagation();
            }),
        )
}

/// relay 模式选择行（Create/Join/设置共用；ADR 0013 §1）。
///
/// 选择只改**面板/表单状态**：Create/Join 的 remote 提交会被 S3b 边界明确拒绝；
/// 设置保存才写 ProfileV2，且重启后生效。`remote_option_available()` 为 false 时
/// remote 按钮视觉禁用（默认构建没有本 UI；保留单一判定点）。
fn mode_selector(
    prefix: &'static str,
    current: RelayMode,
    cx: &mut Context<AppRoot>,
    on_select: impl Fn(&mut AppRoot, RelayMode, &mut Context<AppRoot>) + 'static + Clone,
) -> gpui::Div {
    let mut row = div()
        .w_full()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .child(
            div()
                .text_size(px(11.0))
                .text_color(theme::status_bar_text())
                .child(SharedString::from("relay 模式：")),
        );
    let remote_available = remote_option_available();
    for mode in [RelayMode::LocalDev, RelayMode::RemoteTls] {
        let label = match mode {
            RelayMode::LocalDev => "local_dev（本机开发 / 明文 loopback）",
            RelayMode::RemoteTls => "remote_tls（远程服务器 / HTTPS）",
        };
        let chosen = current == mode;
        let disabled = mode == RelayMode::RemoteTls && !remote_available;
        let selector = format!("{prefix}-{}", mode.as_str());
        let handler = on_select.clone();
        row = row.child(action_button(
            selector,
            if chosen {
                format!("● {label}")
            } else {
                format!("○ {label}")
            },
            chosen,
            disabled,
            cx,
            move |root, cx| {
                if disabled {
                    return;
                }
                handler(root, mode, cx);
            },
        ));
    }
    row.child(
        div()
            .text_size(px(10.0))
            .text_color(theme::status_bar_text())
            .child(SharedString::from(if remote_available {
                "远端模式必须显式确认「远程服务器/HTTPS」；pin/指纹仍独立校验"
            } else {
                "本次构建不开放 remote_tls（只在含 sync 能力的构建里可见）"
            })),
    )
}

impl AppRoot {
    /// 设置面板（overlay）：实例列表 + active 切换 + relay 编辑 + 公开信息复制。
    ///
    /// 只投影公开配置；不持有 setup secret/恢复码/root/manager seed（类型上也没有）。
    /// 关闭/取消不写配置；保存只写公开配置，运行中的 `AppShell`/Vault/worker 不动。
    fn render_settings(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let (
            active_entry,
            mode,
            remote_consent,
            stored_fingerprint,
            stored_security,
            error,
            notice,
            relay,
        ) = {
            let settings = self.settings.as_ref()?;
            (
                settings.active.clone(),
                settings.mode,
                settings.remote_consent,
                settings.fingerprint.clone(),
                settings.security,
                settings.error.clone(),
                settings.notice.clone(),
                settings.relay.clone(),
            )
        };
        // 指纹与安全暂停优先读**实时**值（同步失败可能发生在设置打开之后）；
        // 只有当前打开的实例与设置里的 active 一致时才读 pin，避免拿错库。
        let fingerprint = active_entry.as_ref().map(|entry| {
            if self
                .instance
                .as_ref()
                .is_some_and(|current| current.instance_id == entry.instance_id)
            {
                self.settings_fingerprint(entry, cx)
            } else {
                stored_fingerprint.clone().unwrap_or_else(|| {
                    FingerprintState::Unavailable(
                        "该实例当前没有打开；打开后才能读取 durable pin".to_owned(),
                    )
                })
            }
        });
        let security = self
            .main
            .as_ref()
            .and_then(|shell| shell.read(cx).dev_sync_security_category())
            .or(stored_security);
        let entries = self.entries();
        let active_id = self
            .profile
            .as_ref()
            .and_then(|profile| profile.active_instance.clone());

        let mut list = div().w_full().flex().flex_col().gap_1();
        if entries.is_empty() {
            list = list.child(hint_text(
                "配置里还没有实例：先返回欢迎页创建或加入一个同步实例。",
            ));
        }
        for (index, entry) in entries.iter().enumerate() {
            let is_active = active_id.as_deref() == Some(entry.instance_id.as_str());
            let instance_id = entry.instance_id.clone();
            let set_active_id = entry.instance_id.clone();
            let row_id = entry.instance_id.clone();
            let row_selector = format!("settings-instance-{index}");
            let active_selector = format!("settings-instance-{index}-active");
            let row_label = format!(
                "{} · 本机实例 {}",
                if is_active { "当前默认" } else { "实例" },
                short_public_id(&entry.instance_id)
            );
            let ids_line = format!(
                "API {} · {} · vault_id {} · device_id {}",
                entry.api_version.as_str(),
                entry.relay_mode.as_str(),
                short_public_id(&entry.vault_id),
                short_public_id(&entry.device_id)
            );
            list = list.child(
                div()
                    .id(SharedString::from(row_id))
                    .debug_selector(move || row_selector)
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .p_2()
                    .rounded_sm()
                    .border_1()
                    .border_color(if is_active {
                        theme::link_color()
                    } else {
                        theme::sidebar_border()
                    })
                    .bg(theme::sidebar_bg())
                    .cursor_pointer()
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(theme::text_color())
                            .child(SharedString::from(row_label)),
                    )
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(theme::status_bar_text())
                            .child(SharedString::from(ids_line)),
                    )
                    .child(
                        div()
                            .text_size(px(10.0))
                            .text_color(theme::status_bar_text())
                            .child(SharedString::from(entry.vault_path.clone())),
                    )
                    .child(action_button(
                        active_selector,
                        if is_active {
                            "当前默认"
                        } else {
                            "设为默认"
                        },
                        false,
                        is_active,
                        cx,
                        move |root, cx| root.settings_set_active(&set_active_id, cx),
                    ))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |root, _event: &gpui::MouseDownEvent, _window, cx| {
                            root.settings_set_active(&instance_id, cx);
                            cx.stop_propagation();
                        }),
                    ),
            );
        }

        let public_section: gpui::AnyElement = match active_entry {
            Some(entry) => {
                let vault_id = entry.vault_id.clone();
                let device_id = entry.device_id.clone();
                let vault_path = entry.vault_path.clone();
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .pt(px(4.0))
                    .border_t_1()
                    .border_color(theme::sidebar_border())
                    .child(section_text(
                        "公开信息（可复制；不要分享 setup secret、恢复码或主密钥）",
                    ))
                    .child(
                        div()
                            .debug_selector(|| "settings-api-version".to_string())
                            .w_full()
                            .text_size(px(11.0))
                            .text_color(theme::text_color())
                            .child(SharedString::from(format!(
                                "API 面：{}（{}；面在创建时固定，v1/v2 不能混用）",
                                entry.api_version.as_str(),
                                match entry.api_version {
                                    RelayApiVersion::V1 => "setup secret 创建",
                                    RelayApiVersion::V2 => "一次性票据创建",
                                }
                            ))),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .text_size(px(11.0))
                                    .text_color(theme::text_color())
                                    .child(SharedString::from(format!("vault_id：{vault_id}"))),
                            )
                            .child(action_button(
                                "settings-copy-vault-id",
                                "复制",
                                false,
                                false,
                                cx,
                                |root, cx| {
                                    let value = root
                                        .settings_active_entry()
                                        .map_or_else(String::new, |entry| entry.vault_id);
                                    root.settings_copy_public(value, "vault_id", cx);
                                },
                            )),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .text_size(px(11.0))
                                    .text_color(theme::text_color())
                                    .child(SharedString::from(format!("device_id：{device_id}"))),
                            )
                            .child(action_button(
                                "settings-copy-device-id",
                                "复制",
                                false,
                                false,
                                cx,
                                |root, cx| {
                                    let value = root
                                        .settings_active_entry()
                                        .map_or_else(String::new, |entry| entry.device_id);
                                    root.settings_copy_public(value, "device_id", cx);
                                },
                            )),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .text_size(px(11.0))
                                    .text_color(theme::status_bar_text())
                                    .child(SharedString::from(format!("库文件：{vault_path}"))),
                            )
                            .child(action_button(
                                "settings-copy-path",
                                "复制",
                                false,
                                false,
                                cx,
                                |root, cx| {
                                    let value = root
                                        .settings_active_entry()
                                        .map_or_else(String::new, |entry| entry.vault_path);
                                    root.settings_copy_public(value, "库路径", cx);
                                },
                            )),
                    )
                    .into_any_element()
            }
            None => hint_text(
                "当前没有 active 实例：列表里点「设为默认」后才能修改 relay（保存后重启生效）。",
            )
            .into_any_element(),
        };

        let has_error = error.is_some();
        let error_text = error.unwrap_or_else(|| "无错误".to_owned());
        let notice_text = notice.unwrap_or_else(|| {
            "修改保存后需重启应用才生效；公开 id 可复制，秘密不要分享。".to_owned()
        });

        Some(
            div()
                .id("dev-sync-settings-backdrop")
                .absolute()
                .inset_0()
                .flex()
                .items_start()
                .justify_center()
                .pt(px(56.0))
                .bg(gpui::rgba(0x00000026))
                // 点空白 = 关闭（不保存）；面板内部拦冒泡。
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|root, _event: &gpui::MouseDownEvent, _window, cx| {
                        root.close_settings(cx);
                    }),
                )
                .child(
                    div()
                        .id("dev-sync-settings")
                        .debug_selector(|| "dev-sync-settings".to_string())
                        .w(px(640.0))
                        .flex()
                        .flex_col()
                        .gap_1()
                        .p(px(14.0))
                        .bg(theme::menu_bg())
                        .border_1()
                        .border_color(theme::sidebar_border())
                        .rounded_md()
                        .shadow_lg()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|_root, _event: &gpui::MouseDownEvent, _window, cx| {
                                cx.stop_propagation();
                            }),
                        )
                        .on_key_down(cx.listener(
                            |root, event: &gpui::KeyDownEvent, _window, cx| {
                                if event.keystroke.key == "escape" {
                                    root.close_settings(cx);
                                    cx.stop_propagation();
                                }
                            },
                        ))
                        .child(title_text("同步设置（DEV ONLY）"))
                        .child(banner_text())
                        .child(section_text(
                            "实例（active 优先；点「设为默认」只写配置，重启后生效）",
                        ))
                        .child(list)
                        .child(section_text(
                            "relay 模式与地址（只对 active 实例；保存后重启生效）",
                        ))
                        .child(mode_selector(
                            "settings-mode",
                            mode,
                            cx,
                            |root, mode, cx| root.settings_set_mode(mode, cx),
                        ))
                        .when(mode == RelayMode::RemoteTls, |this| {
                            this.child(checkbox_row(
                                "settings-remote-consent",
                                remote_consent,
                                "我确认要连接远程服务器（HTTPS）；确认端点 ≠ 授权接受服务器任意 hash",
                                cx,
                                |root, cx| {
                                    let next = root
                                        .settings
                                        .as_ref()
                                        .is_some_and(|settings| !settings.remote_consent);
                                    root.settings_set_remote_consent(next, cx);
                                },
                            ))
                        })
                        .child(
                            div()
                                .w_full()
                                .px_2()
                                .py_1()
                                .bg(theme::window_bg())
                                .border_1()
                                .border_color(theme::input_border())
                                .rounded_sm()
                                .child(relay),
                        )
                        .child(hint_text(match mode {
                            RelayMode::LocalDev => {
                                "local_dev：只允许 http://localhost / 127.0.0.1 / [::1]；\
                                 空值/非 loopback/https/带 userinfo/带路径一律内联报错且不写配置。"
                            }
                            RelayMode::RemoteTls => {
                                "remote_tls：只允许 https + canonical origin（DNS/IP；拒绝 \
                                 userinfo/query/fragment/非根路径）；必须勾选上面的确认。"
                            }
                        }))
                        .child(
                            div()
                                .debug_selector(|| "settings-error".to_string())
                                .w_full()
                                .text_size(px(11.0))
                                .text_color(if has_error {
                                    theme::danger_text()
                                } else {
                                    theme::status_bar_text()
                                })
                                .child(SharedString::from(error_text)),
                        )
                        .child(
                            div()
                                .debug_selector(|| "settings-notice".to_string())
                                .w_full()
                                .text_size(px(11.0))
                                .text_color(theme::caret_color())
                                .child(SharedString::from(notice_text)),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .gap_2()
                                .child(action_button(
                                    "settings-save",
                                    "保存模式/relay（重启后生效）",
                                    true,
                                    false,
                                    cx,
                                    |root, cx| root.settings_save(cx),
                                ))
                                .child(action_button(
                                    "settings-cancel",
                                    "关闭（不保存）",
                                    false,
                                    false,
                                    cx,
                                    |root, cx| root.close_settings(cx),
                                )),
                        )
                        .child(self.render_settings_fingerprint(&fingerprint, security, cx))
                        .child(public_section),
                )
                .into_any_element(),
        )
    }

    /// 设置页的「安全指纹」只读区（ADR 0013 §4）。
    ///
    /// 只投影公开值；**没有**重置 pin / 强制信任 / 接受无效证书之类的动作。
    fn render_settings_fingerprint(
        &self,
        fingerprint: &Option<FingerprintState>,
        security: Option<crate::dev_sync::SecurityCategory>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let mut section = div()
            .debug_selector(|| "settings-fingerprint".to_string())
            .w_full()
            .flex()
            .flex_col()
            .gap_1()
            .pt(px(4.0))
            .border_t_1()
            .border_color(theme::sidebar_border())
            .child(section_text(
                "安全指纹（公开、只读；不含 token/恢复码/主密钥）",
            ));
        let headline = fingerprint.as_ref().map_or_else(
            || "尚未打开实例：无法读取 pin 指纹".to_owned(),
            FingerprintState::headline,
        );
        section = section.child(
            div()
                .debug_selector(|| "settings-fingerprint-headline".to_string())
                .w_full()
                .text_size(px(11.0))
                .text_color(
                    if fingerprint.as_ref().is_some_and(FingerprintState::has_pin) {
                        theme::text_color()
                    } else {
                        theme::status_bar_text()
                    },
                )
                .child(SharedString::from(headline)),
        );
        if let Some(state) = fingerprint {
            for (label, value) in state.display_lines() {
                section = section.child(
                    div()
                        .w_full()
                        .text_size(px(10.0))
                        .text_color(theme::text_color())
                        .child(SharedString::from(format!("{label}：{value}"))),
                );
            }
        }
        section = section
            .child(
                div()
                    .debug_selector(|| "settings-security-category".to_string())
                    .w_full()
                    .text_size(px(11.0))
                    .text_color(if security.is_some() {
                        theme::danger_text()
                    } else {
                        theme::status_bar_text()
                    })
                    .child(SharedString::from(match security {
                        Some(category) => format!(
                            "错误类别：安全暂停（{}）· {}",
                            category.label(),
                            category.hint()
                        ),
                        None => "错误类别：无安全暂停（换服务器/清 pin 都不会自动发生）".to_owned(),
                    })),
            )
            .child(action_button(
                "settings-copy-fingerprint",
                "复制安全指纹（公开值）",
                false,
                false,
                cx,
                |root, cx| root.settings_copy_fingerprint(cx),
            ))
            .child(hint_text(FINGERPRINT_NOTICE));
        section.into_any_element()
    }

    fn render_welcome(&self, cx: &mut Context<Self>) -> gpui::Div {
        let entries = self.entries();
        let mut list = div().flex().flex_col().gap_1();
        if entries.is_empty() {
            list = list.child(hint_text("还没有保存的实例：点下面「创建新的同步实例」。"));
        } else {
            for (index, entry) in entries.iter().enumerate() {
                let short_vault = entry.vault_id.chars().take(8).collect::<String>();
                let active = self
                    .profile
                    .as_ref()
                    .and_then(|profile| profile.active_instance.as_deref())
                    == Some(entry.instance_id.as_str());
                let instance_id = entry.instance_id.clone();
                let label = format!(
                    "{}{} · v{} · vault {}… · {}",
                    entry.short_label(),
                    if active { "（上次使用）" } else { "" },
                    entry.api_version.as_number(),
                    short_vault,
                    entry.relay_url
                );
                list = list.child(
                    div()
                        .id(SharedString::from(format!("app-root-instance-{index}")))
                        .debug_selector(move || format!("app-root-instance-{index}"))
                        .w_full()
                        .px_2()
                        .py_1()
                        .rounded_sm()
                        .border_1()
                        .border_color(theme::sidebar_border())
                        .bg(theme::sidebar_bg())
                        .cursor_pointer()
                        .text_size(px(12.0))
                        .text_color(theme::text_color())
                        .child(SharedString::from(label))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |root, _event: &gpui::MouseDownEvent, _window, cx| {
                                root.open_saved_instance(&instance_id, cx);
                                cx.stop_propagation();
                            }),
                        ),
                );
            }
        }
        let mut column = page_column()
            .debug_selector(|| "app-root-welcome".to_string())
            .child(title_text("menghunote · 同步实例"))
            .child(banner_text())
            .child(
                content_column()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_2()
                            .child(action_button(
                                "app-root-create",
                                "创建新的同步实例",
                                true,
                                false,
                                cx,
                                |root, cx| root.enter_create_form(cx),
                            ))
                            .child(action_button(
                                "app-root-create-link",
                                "粘贴创建链接（v2 票据）",
                                false,
                                false,
                                cx,
                                |root, cx| root.enter_create_link_form(cx),
                            ))
                            .child(action_button(
                                "app-root-join",
                                "加入已有同步实例",
                                false,
                                false,
                                cx,
                                |root, cx| root.enter_join_form(cx),
                            ))
                            .child(action_button(
                                "app-root-settings",
                                "同步设置",
                                false,
                                false,
                                cx,
                                |root, cx| root.open_settings(cx),
                            )),
                    )
                    .child(section_text("已保存的实例（active 优先）"))
                    .child(
                        div()
                            .debug_selector(|| "app-root-instances".to_string())
                            .w_full()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(list),
                    ),
            );
        if self.options.allow_local_vault {
            column = column.child(
                content_column()
                    .child(section_text("没有同步需求？"))
                    .child(hint_text(
                        "「继续本地」打开默认 dev vault（target/tmp/menghunote-dev）；\
                         它不联网、没有同步面板，仅供开发。",
                    ))
                    .child(action_button(
                        "app-root-local",
                        "继续本地（DEV ONLY，无同步）",
                        false,
                        false,
                        cx,
                        |root, cx| root.open_local_vault(cx),
                    )),
            );
        }
        let notice = self.notice.clone();
        column.when_some(notice, |this, notice| {
            this.child(
                div()
                    .debug_selector(|| "app-root-notice".to_string())
                    .text_size(px(11.0))
                    .text_color(theme::caret_color())
                    .child(SharedString::from(notice)),
            )
        })
    }

    fn render_create_form(&self, cx: &mut Context<Self>) -> gpui::Div {
        let Some(form) = self.form.as_ref() else {
            return page_column()
                .debug_selector(|| "app-root-create-form".to_string())
                .child(title_text("没有待填写的创建表单"))
                .child(hint_text("内部状态不一致：请返回欢迎页重试。"));
        };
        let consent = form.consent;
        let mode = form.mode;
        let remote_consent = form.remote_consent;
        let relay = form.relay.clone();
        let secret = form.secret.clone();
        let link = form.link.clone();
        let dir = form.dir.clone();
        let form_error = form.error.clone();
        // 票据模式：链接已应用（setup secret 不再需要，隐藏该字段）。
        let link_facts = form
            .link_state
            .as_ref()
            .map(|link| (link.relay.clone(), link.vault_id_hex.clone()));
        let ticket_mode = link_facts.is_some();
        let relay_label = match mode {
            RelayMode::LocalDev => "relay 地址（local_dev 只允许明文 loopback）",
            RelayMode::RemoteTls => "relay 地址（remote_tls 只接受 HTTPS canonical origin）",
        };
        let mut column = page_column()
            .debug_selector(|| "app-root-create-form".to_string())
            .child(title_text(if ticket_mode {
                "用创建链接创建同步实例（v2 一次性票据）"
            } else {
                "创建同步实例（Create）"
            }))
            .child(banner_text())
            .child(
                content_column()
                    .child(section_text(
                        "创建链接（可选；粘贴 mn-pair://create?…&vault=…#ticket=… 后不需要 setup secret）",
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
                            .child(link),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_2()
                            .child(action_button(
                                "app-root-create-link-apply",
                                "应用创建链接",
                                false,
                                false,
                                cx,
                                |root, cx| root.form_apply_create_link(cx),
                            ))
                            .when(ticket_mode, |this| {
                                this.child(action_button(
                                    "app-root-create-link-clear",
                                    "移除链接（回到 setup secret）",
                                    false,
                                    false,
                                    cx,
                                    |root, cx| root.form_clear_create_link(cx),
                                ))
                            }),
                    )
                    .when_some(link_facts, |this, (relay, vault_id)| {
                        this.child(
                            div()
                                .debug_selector(|| "app-root-create-link-warning".to_string())
                                .w_full()
                                .text_size(px(11.0))
                                .text_color(theme::caret_color())
                                .child(SharedString::from(
                                    "一次性票据（默认 15 分钟内有效）：只在内存里提交一次；不写配置/日志，\
                                     失败不消费票据，但本次界面里的票据会随提交移除（可重新粘贴同一链接）。\
                                     创建成功后会先到备份页（恢复码只显示一次），配置写入 api_version=2。",
                                )),
                        )
                        .child(
                            div()
                                .debug_selector(|| "app-root-create-link-facts".to_string())
                                .w_full()
                                .text_size(px(11.0))
                                .text_color(theme::text_color())
                                .child(SharedString::from(format!(
                                    "relay：{relay}   ·   vault_id：{vault_id}   ·   API 面：v2"
                                ))),
                        )
                        .when(mode == RelayMode::RemoteTls && !remote_consent, |this| {
                            this.child(checkbox_row(
                                "app-root-remote-consent",
                                remote_consent,
                                "我确认要连接远程服务器（HTTPS）；确认端点 ≠ 授权接受服务器任意 hash",
                                cx,
                                |root, cx| {
                                    let next = root
                                        .form
                                        .as_ref()
                                        .is_some_and(|form| !form.remote_consent);
                                    root.form_set_remote_consent(next, cx);
                                },
                            ))
                        })
                    })
                    .when(!ticket_mode, |this| {
                        this.child(mode_selector(
                            "app-root-mode",
                            mode,
                            cx,
                            |root, mode, cx| root.form_set_mode(mode, cx),
                        ))
                        .when(mode == RelayMode::RemoteTls, |this| {
                            this.child(
                                div()
                                    .debug_selector(|| "app-root-remote-dev-notice".to_string())
                                    .w_full()
                                    .text_size(px(11.0))
                                    .text_color(theme::caret_color())
                                    .child(SharedString::from(
                                        "DEV 状态：remote_tls 初始化已接线（NewGenesis / production TLS + \
                                         受限初始化，未在真实 relay / Windows 上验证）。不会忽略证书，\
                                         也不提供私有 CA 导入（请自行安装系统根）。",
                                    )),
                            )
                            .child(checkbox_row(
                                "app-root-remote-consent",
                                remote_consent,
                                "我确认要连接远程服务器（HTTPS）；确认端点 ≠ 授权接受服务器任意 hash",
                                cx,
                                |root, cx| {
                                    let next = root
                                        .form
                                        .as_ref()
                                        .is_some_and(|form| !form.remote_consent);
                                    root.form_set_remote_consent(next, cx);
                                },
                            ))
                        })
                        .child(label_text(relay_label))
                        .child(
                            div()
                                .w_full()
                                .px_2()
                                .py_1()
                                .bg(theme::window_bg())
                                .border_1()
                                .border_color(theme::input_border())
                                .rounded_sm()
                                .child(relay),
                        )
                        .child(label_text("setup secret（服务器创建凭证；不会保存）"))
                        .child(
                            div()
                                .w_full()
                                .px_2()
                                .py_1()
                                .bg(theme::window_bg())
                                .border_1()
                                .border_color(theme::input_border())
                                .rounded_sm()
                                .child(secret),
                        )
                        .child(hint_text(
                            "这不是笔记库密码：dev-local 不接收用户口令，setup secret 只用于本次 genesis。",
                        ))
                    })
                    .child(label_text(
                        "实例目录（绝对路径；目录内固定 vault.sqlite3 / dev-identity.bin）",
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
                            .child(dir),
                    )
                    .child(checkbox_row(
                        "app-root-consent",
                        consent,
                        "我理解这是开发同步（DEV ONLY / localhost / 非生产），并同意创建",
                        cx,
                        |root, cx| {
                            let next = root.form.as_ref().is_some_and(|form| !form.consent);
                            root.form_set_consent(next, cx);
                        },
                    ))
                    .when_some(form_error, |this, error| {
                        this.child(
                            div()
                                .debug_selector(|| "app-root-form-error".to_string())
                                .text_size(px(11.0))
                                .text_color(theme::danger_text())
                                .child(SharedString::from(error)),
                        )
                    })
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_2()
                            .child(action_button(
                                "app-root-submit",
                                if ticket_mode {
                                    "用一次性票据创建"
                                } else {
                                    "创建"
                                },
                                true,
                                false,
                                cx,
                                |root, cx| root.submit_create(cx),
                            ))
                            .child(action_button(
                                "app-root-back",
                                "返回",
                                false,
                                false,
                                cx,
                                |root, cx| root.back_to_welcome(cx),
                            )),
                    ),
            );
        let notice = self.notice.clone();
        column = column.when_some(notice, |this, notice| {
            this.child(
                div()
                    .debug_selector(|| "app-root-notice".to_string())
                    .text_size(px(11.0))
                    .text_color(theme::caret_color())
                    .child(SharedString::from(notice)),
            )
        });
        column
    }

    fn render_join_form(&self, cx: &mut Context<Self>) -> gpui::Div {
        let Some(form) = self.join_form.as_ref() else {
            return page_column()
                .debug_selector(|| "app-root-join-form".to_string())
                .child(title_text("没有待填写的加入表单"))
                .child(hint_text("内部状态不一致：请返回欢迎页重试。"));
        };
        let relay = form.relay.clone();
        let vault_id = form.vault_id.clone();
        let recovery_code = form.recovery_code.clone();
        let invite = form.invite.clone();
        let dir = form.dir.clone();
        let form_error = form.error.clone();
        let mode = form.mode;
        let remote_consent = form.remote_consent;
        let relay_label = match mode {
            RelayMode::LocalDev => "relay 地址（local_dev 只允许明文 loopback）",
            RelayMode::RemoteTls => "relay 地址（remote_tls 只接受 HTTPS canonical origin）",
        };
        let column = page_column()
            .debug_selector(|| "app-root-join-form".to_string())
            .child(title_text("加入已有同步实例（Join）"))
            .child(banner_text())
            .child(
                content_column()
                    .child(mode_selector(
                        "app-root-join-mode",
                        mode,
                        cx,
                        |root, mode, cx| root.join_form_set_mode(mode, cx),
                    ))
                    .when(mode == RelayMode::RemoteTls, |this| {
                        this.child(
                            div()
                                .debug_selector(|| "app-root-remote-dev-notice".to_string())
                                .w_full()
                                .text_size(px(11.0))
                                .text_color(theme::caret_color())
                                .child(SharedString::from(
                                    "DEV 状态：remote_tls 初始化已接线（FreshRecovery / production TLS + \
                                     受限初始化，未在真实 relay / Windows 上验证）。不会忽略证书，\
                                     也不提供私有 CA 导入（请自行安装系统根）。",
                                )),
                        )
                        .child(checkbox_row(
                            "app-root-join-remote-consent",
                            remote_consent,
                            "我确认要连接远程服务器（HTTPS）；确认端点 ≠ 授权接受服务器任意 hash",
                            cx,
                            |root, cx| {
                                let next = root
                                    .join_form
                                    .as_ref()
                                    .is_some_and(|form| !form.remote_consent);
                                root.join_form_set_remote_consent(next, cx);
                            },
                        ))
                    })
                    .child(
                        div()
                            .w_full()
                            .text_size(px(12.0))
                            .text_color(theme::caret_color())
                            .child(SharedString::from(
                                "本机没有这个设备身份：这是「加入」，不是登录——会用恢复码自批一个\
                                 新的普通设备身份（没有管理私钥），不会覆盖创建者或其它设备。",
                            )),
                    )
                    .child(label_text(relay_label))
                    .child(
                        div()
                            .w_full()
                            .px_2()
                            .py_1()
                            .bg(theme::window_bg())
                            .border_1()
                            .border_color(theme::input_border())
                            .rounded_sm()
                            .child(relay),
                    )
                    .child(label_text("vault_id（32 位十六进制；创建者实例的公开标识）"))
                    .child(
                        div()
                            .w_full()
                            .px_2()
                            .py_1()
                            .bg(theme::window_bg())
                            .border_1()
                            .border_color(theme::input_border())
                            .rounded_sm()
                            .child(vault_id),
                    )
                    .child(hint_text(
                        "vault_id 是公开标识，不遮罩；恢复码才是秘密，千万不要贴进这个框。",
                    ))
                    .child(label_text("邀请链接（可选；mn-pair://join?...，粘贴后不需要恢复码）"))
                    .child(
                        div()
                            .w_full()
                            .px_2()
                            .py_1()
                            .bg(theme::window_bg())
                            .border_1()
                            .border_color(theme::input_border())
                            .rounded_sm()
                            .child(invite),
                    )
                    .child(hint_text(
                        "邀请只到「待批准」：创建者必须在设备管理里核对指纹并批准，本机才拿到数据；\
                         链接含一次性令牌，兑换后输入框立即清空、不落盘、不进日志。",
                    ))
                    .child(label_text("恢复码（走恢复码加入时填写；mnrec1…、遮罩、不落盘）"))
                    .child(
                        div()
                            .w_full()
                            .px_2()
                            .py_1()
                            .bg(theme::window_bg())
                            .border_1()
                            .border_color(theme::input_border())
                            .rounded_sm()
                            .child(recovery_code),
                    )
                    .child(hint_text(
                        "可粘贴（Ctrl+V / Shift+Insert）、可全选（Ctrl+A）、Tab 切换字段；\
                         提交后输入框立即清空，只在后台线程内存里存在。",
                    ))
                    .child(label_text(
                        "实例目录（绝对路径；必须尚不存在或空目录；目录内固定 vault.sqlite3 / dev-identity.bin）",
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
                            .child(dir),
                    )
                    .child(hint_text(
                        "与 Create 同一套目录校验与实例锁：已有库/身份/其它文件一律拒绝，不覆盖。",
                    ))
                    .when_some(form_error, |this, error| {
                        this.child(
                            div()
                                .debug_selector(|| "app-root-join-error".to_string())
                                .w_full()
                                .text_size(px(11.0))
                                .text_color(theme::danger_text())
                                .child(SharedString::from(error)),
                        )
                    })
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_2()
                            .child(action_button(
                                "app-root-join-submit",
                                "加入",
                                true,
                                false,
                                cx,
                                |root, cx| root.submit_join(cx),
                            ))
                            .child(action_button(
                                "app-root-join-back",
                                "返回",
                                false,
                                false,
                                cx,
                                |root, cx| root.back_to_welcome(cx),
                            )),
                    ),
            );
        let notice = self.notice.clone();
        column.when_some(notice, |this, notice| {
            this.child(
                div()
                    .debug_selector(|| "app-root-notice".to_string())
                    .text_size(px(11.0))
                    .text_color(theme::caret_color())
                    .child(SharedString::from(notice)),
            )
        })
    }

    /// 邀请加入的 PENDING 等待页（ADR 0014 S1）。
    ///
    /// 只显示公开事实（relay/vault/设备/指纹 + 双端比对的 sign/hpke 短 hash），
    /// 绝不显示 token/私钥/恢复码。
    fn render_invite_pending(&self, cx: &mut Context<Self>) -> gpui::Div {
        let Some(invite) = self.invite.as_ref() else {
            return page_column()
                .debug_selector(|| "app-root-invite-pending".to_string())
                .child(title_text("没有等待批准的邀请"))
                .child(hint_text("内部状态不一致：请返回欢迎页重新粘贴邀请链接。"));
        };
        let facts = &invite.facts;
        let busy = invite.busy;
        let status = invite.message.clone().unwrap_or_else(|| {
            "等待创建者批准：请在创建端设备面板核对指纹后点「批准…」".to_owned()
        });
        let expires_left = facts
            .expires_at
            .saturating_sub(menghunote_sync::http_relay::SystemClock.now_secs());
        let mut column = page_column()
            .debug_selector(|| "app-root-invite-pending".to_string())
            .child(title_text("等待创建者批准（邀请加入）"))
            .child(banner_text())
            .child(
                content_column()
                    .child(section_text(
                        "当前状态：待批准（PENDING）——本机还没有数据访问权，也没有写入身份/配置",
                    ))
                    .child(
                        div()
                            .debug_selector(|| "app-root-invite-status".to_string())
                            .w_full()
                            .text_size(px(11.0))
                            .text_color(if busy {
                                theme::caret_color()
                            } else {
                                theme::text_color()
                            })
                            .child(SharedString::from(status)),
                    )
                    .child(
                        div()
                            .debug_selector(|| "app-root-invite-facts".to_string())
                            .w_full()
                            .flex()
                            .flex_col()
                            .gap_0p5()
                            .text_size(px(10.0))
                            .text_color(theme::status_bar_text())
                            .child(SharedString::from(format!(
                                "relay：{}（{}）",
                                facts.relay_url,
                                facts.relay_mode.as_str()
                            )))
                            .child(SharedString::from(format!("vault：{}", facts.vault_id)))
                            .child(SharedString::from(format!("本机设备：{}", facts.device_id)))
                            .child(SharedString::from(format!(
                                "设备指纹：{}（与创建端确认框逐字核对）",
                                facts.fingerprint
                            )))
                            .child(SharedString::from(format!(
                                "sign 短 hash：{}｜hpke 短 hash：{}",
                                facts.sign_short, facts.hpke_short
                            )))
                            .child(SharedString::from(format!(
                                "邀请有效期：约 {} 分钟（过期只影响兑换；已经兑换的等待不受影响）",
                                expires_left / 60
                            ))),
                    )
                    .child(hint_text(
                        "PENDING 已 durable 保存：关闭窗口后再粘贴同一链接可继续等待/检查，\
                         不会重复兑换；身份与配置只会在 bootstrap 成功后写入。",
                    ))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_2()
                            .child(action_button(
                                "app-root-invite-check",
                                "检查状态",
                                true,
                                busy,
                                cx,
                                |root, cx| root.check_invite(cx),
                            ))
                            .child(action_button(
                                "app-root-invite-back",
                                "返回（保留 PENDING）",
                                false,
                                busy,
                                cx,
                                |root, cx| root.back_from_invite(cx),
                            )),
                    ),
            );
        if let Some(notice) = self.notice.clone() {
            column = column.child(
                div()
                    .debug_selector(|| "app-root-notice".to_string())
                    .text_size(px(11.0))
                    .text_color(theme::caret_color())
                    .child(SharedString::from(notice)),
            );
        }
        column
    }

    fn render_working(&self, cx: &mut Context<Self>) -> gpui::Div {
        let kind = self
            .working
            .as_ref()
            .map_or(WorkingKind::Create, |working| working.kind);
        let progress = self.working_label().unwrap_or("正在联系后台 worker…");
        let column = page_column()
            .debug_selector(|| "app-root-working".to_string())
            .child(title_text(kind.title()))
            .child(banner_text())
            .child(
                content_column()
                    .child(
                        div()
                            .debug_selector(|| "app-root-progress".to_string())
                            .text_size(px(13.0))
                            .text_color(theme::text_color())
                            .child(SharedString::from(progress)),
                    )
                    .child(hint_text(kind.hint()))
                    .child(action_button(
                        "app-root-cancel",
                        "取消（等真实收尾）",
                        false,
                        true,
                        cx,
                        |root, cx| root.cancel_working(cx),
                    )),
            );
        let notice = self.notice.clone();
        column.when_some(notice, |this, notice| {
            this.child(
                div()
                    .debug_selector(|| "app-root-notice".to_string())
                    .text_size(px(11.0))
                    .text_color(theme::caret_color())
                    .child(SharedString::from(notice)),
            )
        })
    }

    fn render_backup_required(&self, cx: &mut Context<Self>) -> gpui::Div {
        let Some(pending) = &self.pending else {
            return page_column()
                .debug_selector(|| "app-root-backup".to_string())
                .child(title_text("没有待备份的实例"))
                .child(hint_text("内部状态不一致：请返回欢迎页重试。"));
        };
        let confirmed = pending.confirmed;
        let copied = pending.copied;
        let vault_id = pending.entry.vault_id.clone();
        let recovery_code = pending.outcome.recovery_code.as_str().to_owned();
        let column = page_column()
            .debug_selector(|| "app-root-backup".to_string())
            .child(title_text("备份恢复码（只显示这一次）"))
            .child(banner_text())
            .child(
                content_column()
                    .child(
                        div()
                            .w_full()
                            .text_size(px(12.0))
                            .text_color(theme::danger_text())
                            .child(SharedString::from(
                                "恢复码拥有数据/管理恢复能力：离开本页后不再显示、不落盘、不进日志。\
                                 请抄写或复制到应用之外的离线位置。",
                            )),
                    )
                    .child(
                        div()
                            .debug_selector(|| "app-root-vault-id".to_string())
                            .text_size(px(12.0))
                            .text_color(theme::text_color())
                            .child(SharedString::from(format!("vault_id：{vault_id}"))),
                    )
                    .child(hint_text(
                        "relay 终端里的 relay_instance_id 是服务器实例标识，不是这个 vault_id；                         当前 relay 仍是单 vault。",
                    ))
                    .child(
                        div()
                            .debug_selector(|| "app-root-recovery-code".to_string())
                            .w_full()
                            .p(px(8.0))
                            .bg(theme::window_bg())
                            .border_1()
                            .border_color(theme::input_border())
                            .rounded_sm()
                            .font_family(theme::MONO_FAMILY)
                            .text_size(px(13.0))
                            .text_color(theme::caret_color())
                            .child(SharedString::from(recovery_code)),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_2()
                            .child(action_button(
                                "app-root-copy-code",
                                "复制恢复码（有风险）",
                                false,
                                copied,
                                cx,
                                |root, cx| root.copy_recovery_code(cx),
                            ))
                            .when(copied, |this| {
                                this.child(hint_text(
                                    "已复制：剪贴板历史可能保留副本，备份后请清空。",
                                ))
                            }),
                    )
                    .child(checkbox_row(
                        "app-root-backup-confirm",
                        confirmed,
                        "已在应用之外备份恢复码",
                        cx,
                        |root, cx| {
                            let next = root.pending.as_ref().is_some_and(|p| !p.confirmed);
                            root.set_backup_confirmed(next, cx);
                        },
                    ))
                    .child(action_button(
                        "app-root-continue",
                        "继续进入主界面",
                        true,
                        !confirmed,
                        cx,
                        |root, cx| root.confirm_backup(cx),
                    )),
            );
        let notice = self.notice.clone();
        column.when_some(notice, |this, notice| {
            this.child(
                div()
                    .debug_selector(|| "app-root-notice".to_string())
                    .text_size(px(11.0))
                    .text_color(theme::caret_color())
                    .child(SharedString::from(notice)),
            )
        })
    }

    fn render_error(&self, cx: &mut Context<Self>) -> gpui::Div {
        let Some(error) = &self.error else {
            return page_column()
                .debug_selector(|| "app-root-error".to_string())
                .child(title_text("错误"))
                .child(hint_text("没有错误详情"));
        };
        let mut fields = div().flex().flex_col().gap_1().w_full();
        for (key, value) in &error.public {
            fields = fields.child(
                div()
                    .w_full()
                    .text_size(px(11.0))
                    .text_color(theme::status_bar_text())
                    .child(SharedString::from(format!("{key}：{value}"))),
            );
        }
        let retry_label = error.retry.label();
        let mut column = page_column()
            .debug_selector(|| "app-root-error".to_string())
            .child(title_text(error.title.clone()))
            .child(banner_text())
            .child(
                content_column()
                    .child(
                        div()
                            .debug_selector(|| "app-root-error-message".to_string())
                            .w_full()
                            .text_size(px(12.0))
                            .text_color(theme::danger_text())
                            .child(SharedString::from(error.message.clone())),
                    )
                    .child(fields)
                    .child(action_button(
                        "app-root-retry",
                        retry_label,
                        true,
                        false,
                        cx,
                        |root, cx| root.retry(cx),
                    )),
            )
            .child(action_button(
                "app-root-back-welcome",
                "返回欢迎页",
                false,
                false,
                cx,
                |root, cx| {
                    root.back_to_welcome(cx);
                },
            ));
        // 保存失败时恢复码仍必须能看到（否则用户会以为"没生成"，重跑 Create）。
        if let Some(pending) = &self.pending {
            let code = pending.outcome.recovery_code.as_str().to_owned();
            column = column.child(
                content_column()
                    .child(label_text("本次实例的恢复码（仍未备份到配置之外）"))
                    .child(
                        div()
                            .w_full()
                            .p(px(8.0))
                            .bg(theme::window_bg())
                            .border_1()
                            .border_color(theme::input_border())
                            .rounded_sm()
                            .font_family(theme::MONO_FAMILY)
                            .text_size(px(13.0))
                            .text_color(theme::caret_color())
                            .child(SharedString::from(code)),
                    )
                    .child(action_button(
                        "app-root-copy-code",
                        "复制恢复码（有风险）",
                        false,
                        false,
                        cx,
                        |root, cx| root.copy_recovery_code(cx),
                    )),
            );
        }
        let notice = self.notice.clone();
        column.when_some(notice, |this, notice| {
            this.child(
                div()
                    .debug_selector(|| "app-root-notice".to_string())
                    .text_size(px(11.0))
                    .text_color(theme::caret_color())
                    .child(SharedString::from(notice)),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(prefix: &str) -> PathBuf {
        // lib 单测拿不到 `CARGO_TARGET_TMPDIR`（集成测试才有），按 AGENTS §6 落仓库 target/tmp。
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/tmp")
            .join(format!(
                "onboarding-app-root-{prefix}-{}-{}",
                std::process::id(),
                super::super::layout::generate_instance_id()
            ));
        std::fs::create_dir_all(&dir).expect("创建测试目录失败");
        dir
    }

    #[test]
    fn retry_labels_are_stable() {
        assert_eq!(RetryKind::SaveProfile.label(), "重试保存");
        assert_eq!(RetryKind::CreateForm.label(), "返回表单重试");
    }

    #[test]
    fn default_operation_budget_is_the_adr_value() {
        // ADR 0011：沿用 120s 操作预算；逻辑超时 ≠ 停止。
        assert_eq!(DEFAULT_OPERATION_BUDGET, Duration::from_secs(120));
        assert_eq!(CREATE_PUMP_INTERVAL, Duration::from_millis(40));
    }

    #[test]
    fn vault_id_input_requires_32_hex_without_echoing_it() {
        let valid = parse_vault_id_input("00112233445566778899aabbccddeeff").expect("32 位 hex");
        assert_eq!(
            menghunote_sync::ids::hex_of(&valid),
            "00112233445566778899aabbccddeeff"
        );
        // 大小写皆可（Create 输出小写，但粘贴来的可能大写）。
        parse_vault_id_input("00112233445566778899AABBCCDDEEFF").expect("大写 hex 也接受");

        for (raw, needle) in [
            ("", "请填写"),
            ("0011", "32 位十六进制"),
            ("00112233445566778899aabbccddeeff00", "32 位十六进制"),
            ("00112233445566778899aabbccddeeZZ", "32 位十六进制"),
        ] {
            let error = parse_vault_id_input(raw).expect_err("必须拒绝");
            assert!(error.contains(needle), "{raw:?} → {error}");
        }
        // 关键：用户可能把恢复码误贴进这个框，错误里绝不能回显输入。
        let error = parse_vault_id_input("mnrec1secretcode0123456789").expect_err("必须拒绝");
        assert!(!error.contains("mnrec1"), "不得回显输入：{error}");
        assert!(!error.contains("secret"), "不得回显输入：{error}");
    }

    #[test]
    fn instance_directory_must_be_absolute_and_empty() {
        // 相对路径直接拒绝（真实使用不回退 cwd）。
        let relative = PathBuf::from("rel/dir");
        let error = validate_instance_directory(&relative).expect_err("相对路径必须拒绝");
        assert!(error.contains("绝对路径"), "{error}");

        // 不存在的绝对目录：允许（由 worker/锁去创建）。
        let created = temp_dir("empty");
        let fresh = created.join("new-instance");
        validate_instance_directory(&fresh).expect("尚不存在的目录必须允许");

        // 空目录：允许。
        let empty = created.join("empty-instance");
        std::fs::create_dir_all(&empty).expect("建空目录失败");
        validate_instance_directory(&empty).expect("空目录必须允许");

        // 只有我们自己可能残留的锁文件：允许（真正冲突由 `InstanceLock` 报，更可诊断）。
        std::fs::write(empty.join(LOCK_FILE_NAME), "pid=1\n").expect("写锁文件失败");
        validate_instance_directory(&empty).expect("只剩锁文件时交给 InstanceLock 判冲突");
        std::fs::remove_file(empty.join(LOCK_FILE_NAME)).expect("删锁文件失败");

        // 目录里已有别的文件：拒绝（不覆盖用户数据）。
        std::fs::write(empty.join("notes.txt"), b"x").expect("写干扰文件失败");
        let error = validate_instance_directory(&empty).expect_err("非空目录必须拒绝");
        assert!(
            error.contains("空目录") && error.contains("notes.txt"),
            "{error}"
        );
        std::fs::remove_file(empty.join("notes.txt")).expect("删干扰文件失败");

        // 已有库 / 身份：拒绝（不覆盖、不迁移）。
        std::fs::write(empty.join(VAULT_FILE_NAME), b"not a db").expect("写库文件失败");
        let error = validate_instance_directory(&empty).expect_err("已有库必须拒绝");
        assert!(error.contains("已有库文件"), "{error}");
        std::fs::remove_file(empty.join(VAULT_FILE_NAME)).expect("删库文件失败");
        std::fs::write(
            DevIdentityStore::path_for(&empty.join(VAULT_FILE_NAME)),
            b"x",
        )
        .expect("写身份文件失败");
        let error = validate_instance_directory(&empty).expect_err("已有身份必须拒绝");
        assert!(error.contains("已有身份文件"), "{error}");
    }
}
