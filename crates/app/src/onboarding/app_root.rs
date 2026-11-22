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
//! | `app-root-join-form` / `app-root-join-relay-input` / `app-root-vault-id-input` / `app-root-recovery-input` / `app-root-join-dir-input` | Join 表单与四个字段 |
//! | `app-root-consent` / `app-root-submit` / `app-root-back` / `app-root-form-error` | Create 表单的同意框/提交/返回/错误行 |
//! | `app-root-join-submit` / `app-root-join-back` / `app-root-join-error` | Join 表单的提交/返回/错误行 |
//! | `app-root-working` / `app-root-progress` / `app-root-cancel` | Working 页（Create/Join 共用） |
//! | `app-root-backup` / `app-root-vault-id` / `app-root-recovery-code` / `app-root-copy-code` / `app-root-backup-confirm` / `app-root-continue` | 备份页 |
//! | `app-root-main` | Main（`AppShell` 子视图宿主） |
//! | `app-root-settings` | 打开设置（Welcome 页与 Main 右下角各一个入口） |
//! | `dev-sync-settings` | 设置覆盖层根（打开时存在；Main 仍在其下保留） |
//! | `settings-instance-<i>` / `settings-instance-<i>-active` | 公开实例行 /「设为默认」控件（active 优先） |
//! | `settings-relay` / `settings-save` / `settings-cancel` | relay 输入框 / 保存 / 关闭（不保存） |
//! | `settings-error` / `settings-notice` | 内联错误 / 提示（含「重启后生效」） |
//! | `settings-copy-vault-id` / `settings-copy-device-id` / `settings-copy-path` | 公开值复制按钮 |
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

use crate::dev_sync::gate::DEFAULT_RELAY;
use crate::dev_sync::service::ServiceConfig;
use crate::dev_sync::{DevIdentityStore, DevSyncConfig};
use crate::dev_vault;
use crate::editor::theme;
use crate::shell::AppShell;

use super::create::{CreateEvent, CreateJob, CreateOutcome, CreateRequest, CREATE_PUMP_INTERVAL};
use super::input::{FormInput, FormInputEvent};
use super::join::{JoinEvent, JoinJob, JoinOutcome, JoinRequest};
use super::layout::{
    generate_instance_id, DevSyncProfile, InstanceEntry, ProfileError, ProfileLayout, ProfileStore,
    VAULT_FILE_NAME,
};
use super::lock::{InstanceLock, LOCK_FILE_NAME};
use super::sanitize_error;
use super::settings::{short_public_id, validate_relay_edit, SettingsState};

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

/// Create 表单状态（输入框实体 + 同意勾选）。
struct FormState {
    instance_id: String,
    relay: Entity<FormInput>,
    secret: Entity<FormInput>,
    dir: Entity<FormInput>,
    consent: bool,
    error: Option<String>,
    /// Enter 提交的订阅（随表单一起 drop）。
    _subs: Vec<gpui::Subscription>,
}

/// Join 表单状态（四个输入框；**不实现 `Debug`**：恢复码就在子实体里）。
struct JoinFormState {
    instance_id: String,
    relay: Entity<FormInput>,
    vault_id: Entity<FormInput>,
    /// 恢复码（秘密；遮罩 + 禁复制）。
    recovery_code: Entity<FormInput>,
    dir: Entity<FormInput>,
    error: Option<String>,
    _subs: Vec<gpui::Subscription>,
}

/// Working 页正在做哪种后台操作（只影响文案，不影响状态机）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkingKind {
    Create,
    Join,
}

impl WorkingKind {
    const fn title(self) -> &'static str {
        match self {
            Self::Create => "正在创建同步实例…",
            Self::Join => "正在加入同步实例…",
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
    relay_url: String,
    confirmed: bool,
    copied: bool,
}

/// Join 成功、但公开配置还没写进去的数据（库/身份已落盘）。
///
/// 只在"配置写失败 → Error 页 → 重试保存"期间存在；**重试只重写配置**，不再 Join。
struct PendingJoin {
    outcome: Box<JoinOutcome>,
    entry: InstanceEntry,
    relay_url: String,
    /// 成功是否发生在用户已请求取消之后（决定进入 Main 时的提示）。
    cancelled: bool,
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
    /// 设置面板（overlay；Main/Welcome 都可用，关闭后回到原状态）。
    settings: Option<SettingsState>,
    /// 关闭设置后要把焦点还给 Main 的编辑器（下一帧渲染时消费）。
    pending_focus_restore: bool,
    main: Option<Entity<AppShell>>,
    lock: Option<InstanceLock>,
    instance: Option<InstanceEntry>,
    retry_entry: Option<InstanceEntry>,
    error: Option<ErrorState>,
    notice: Option<String>,
    pending_focus: bool,
    pump_started: bool,
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
            settings: None,
            pending_focus_restore: false,
            main: None,
            lock: None,
            instance: None,
            retry_entry: None,
            error: None,
            notice: None,
            pending_focus: false,
            pump_started: false,
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
        // relay 可能是手改过的配置：非 loopback 一律拒绝（ADR 0009/0011 门禁）。
        crate::dev_sync::gate::validate_relay_url(&entry.relay_url)
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        let vault_path = entry.vault_path();
        if !vault_path.is_file() {
            bail!(
                "库文件不存在：{}（按 ADR 0011 不新建、不重建；请检查配置或重新创建）",
                vault_path.display()
            );
        }
        let identity_path = DevIdentityStore::path_for(&vault_path);
        if !identity_path.is_file() {
            bail!(
                "实例缺少持久身份：{}（不自动重建；请用 Create 建新实例或人工恢复）",
                identity_path.display()
            );
        }
        // 锁在身份/库检查之后获取，避免给"不存在的配置条目"凭空建目录。
        // 保留 `LockError` 类型：`try_open` 要把"目录被占用"与"实例损坏"分开处理。
        let lock = InstanceLock::acquire(&entry.instance_dir()).map_err(anyhow::Error::new)?;
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
        let vault =
            Vault::open(&vault_path).map_err(|error| anyhow::anyhow!("打开库失败：{error}"))?;
        let label = entry.short_label();
        let config = DevSyncConfig {
            vault: vault_path,
            service: ServiceConfig::new(entry.relay_url.clone()),
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
            .is_some_and(|working| working.kind == WorkingKind::Join)
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
            let mut input =
                FormInput::new(initial_relay.clone(), false, cx).selector("settings-relay");
            input.set_text(&initial_relay);
            input
        });
        // Enter 与「保存」同一条路径（输入框里直接回车即可试保存；失败留在面板）。
        let subscription = cx.subscribe(&relay, |root, _input, event: &FormInputEvent, cx| {
            if matches!(event, FormInputEvent::Submit) {
                root.settings_save(cx);
            }
        });
        self.settings = Some(SettingsState {
            active,
            relay,
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
        let (active, raw_relay) = {
            let Some(settings) = self.settings.as_ref() else {
                return;
            };
            (
                settings.active.clone(),
                settings.relay.read(cx).text().trim().to_owned(),
            )
        };
        let Some(active) = active else {
            self.set_settings_error(
                "当前没有 active 实例：先在实例列表里点「设为默认」，再修改 relay".to_owned(),
            );
            cx.notify();
            return;
        };
        let relay = match validate_relay_edit(&raw_relay) {
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
        entry.relay_url = relay.clone();
        if let Err(error) = self.store.save(&profile) {
            self.set_settings_error(format!(
                "配置保存失败：{error}；库/身份没有被改动，也没有重建实例。\
                 修正配置目录权限后可重试保存（不要重新 Create/Join）"
            ));
            cx.notify();
            return;
        }
        self.profile = Some(profile);
        let mut updated = active;
        updated.relay_url = relay;
        if let Some(settings) = &mut self.settings {
            settings.active = Some(updated);
            settings.error = None;
            settings.notice = Some(
                "relay 已保存：重启后生效。当前窗口仍使用旧 relay（不热切换 \
                 Vault/worker）；实例没有重建。"
                    .to_owned(),
            );
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
        if let Some(settings) = &mut self.settings {
            settings.active = Some(entry.clone());
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
        let relay =
            cx.new(|cx| FormInput::new(default_relay, false, cx).selector("app-root-relay-input"));
        let secret =
            cx.new(|cx| FormInput::new("setup secret", true, cx).selector("app-root-secret-input"));
        let dir =
            cx.new(|cx| FormInput::new(default_dir, false, cx).selector("app-root-dir-input"));
        // 在任意字段按 Enter 都提交表单（与按钮同一条路径）。
        let mut subs = Vec::new();
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
            relay,
            secret,
            dir,
            consent: false,
            error: None,
            _subs: subs,
        });
        self.state = State::CreateForm;
        self.error = None;
        self.pending_focus = true;
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

    /// 勾选/取消"同意开发同步"。
    pub fn form_set_consent(&mut self, consent: bool, cx: &mut Context<Self>) {
        if let Some(form) = &mut self.form {
            form.consent = consent;
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
            FormInput::new(default_relay, false, cx).selector("app-root-join-relay-input")
        });
        let vault_id = cx.new(|cx| {
            FormInput::new("32 位十六进制 vault_id", false, cx).selector("app-root-vault-id-input")
        });
        let recovery_code = cx.new(|cx| {
            FormInput::new("恢复码 mnrec1…", true, cx).selector("app-root-recovery-input")
        });
        let dir =
            cx.new(|cx| FormInput::new(default_dir, false, cx).selector("app-root-join-dir-input"));
        // 在任意字段按 Enter 都提交（与按钮同一条路径）。
        let mut subs = Vec::new();
        for input in [
            relay.clone(),
            vault_id.clone(),
            recovery_code.clone(),
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
            relay,
            vault_id,
            recovery_code,
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

    /// 测试/预填：设置 Join 的实例目录输入框文本。
    pub fn join_form_set_dir(&mut self, text: &str, cx: &mut Context<Self>) {
        if let Some(form) = &self.join_form {
            form.dir.update(cx, |input, cx| {
                input.set_text(text);
                cx.notify();
            });
        }
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
    pub fn submit_create(&mut self, cx: &mut Context<Self>) {
        if self.state != State::CreateForm {
            return;
        }
        let Some(form) = &self.form else {
            return;
        };
        let relay = form.relay.read(cx).text().trim().to_owned();
        let secret = Zeroizing::new(form.secret.read(cx).text().trim().to_owned());
        let dir = form.dir.read(cx).text().trim().to_owned();
        let consent = form.consent;
        let form_error = |form: &mut FormState, message: String| {
            form.error = Some(message);
        };
        if let Some(form) = &mut self.form {
            form.error = None;
        }
        // 1) relay 必须是 loopback（与运行时门禁同一套校验）。
        if let Err(error) = crate::dev_sync::gate::validate_relay_url(&relay) {
            if let Some(form) = &mut self.form {
                form_error(form, error.to_string());
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
            setup_secret: secret,
            lock,
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
        let vault_id_text = form.vault_id.read(cx).text().trim().to_owned();
        // 恢复码是秘密：只在这个局部缓冲里存在，交给 worker 后立刻丢弃/清空输入框。
        let recovery_code = Zeroizing::new(form.recovery_code.read(cx).text().trim().to_owned());
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
        // 1) relay 必须是 loopback 明文（与运行时门禁同一套校验）。
        if let Err(error) = crate::dev_sync::gate::validate_relay_url(&relay) {
            fail(self, error.to_string(), cx);
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
            vault_id,
            vault_id_hex: menghunote_sync::ids::hex_of(&vault_id),
            recovery_code,
            lock,
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
                root.job.is_some() || root.join_job.is_some()
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
        for event in create_events {
            self.apply_create_event(event, cx);
        }
        for event in join_events {
            self.apply_join_event(event, cx);
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
                let (instance_id, relay_url) = self.form_public_identity(cx);
                let entry = outcome.public_facts(&instance_id, &relay_url);
                self.pending = Some(PendingCreate {
                    outcome,
                    entry,
                    relay_url,
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
            CreateEvent::Failed(message) => {
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
                    let public = self.form_public_fields(cx);
                    self.set_error("创建失败", message, public, RetryKind::CreateForm);
                }
            }
        }
        cx.notify();
    }

    fn form_public_identity(&self, cx: &App) -> (String, String) {
        self.form.as_ref().map_or_else(
            || (String::new(), self.options.default_relay.clone()),
            |form| {
                (
                    form.instance_id.clone(),
                    form.relay.read(cx).text().trim().to_owned(),
                )
            },
        )
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
                let (instance_id, relay_url) = self.join_form_public_identity(cx);
                let entry = outcome.public_facts(&instance_id, &relay_url);
                let pending = PendingJoin {
                    outcome,
                    entry,
                    relay_url,
                    cancelled,
                };
                self.commit_join(pending, cx);
            }
            JoinEvent::Failed { message, .. } => {
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
            relay_url,
            cancelled,
        } = pending;
        let JoinOutcome {
            vault,
            lock,
            vault_path,
            peer_id,
            key_epoch,
            ..
        } = *outcome;
        let label = entry.short_label();
        let config = DevSyncConfig {
            vault: vault_path,
            service: ServiceConfig::new(relay_url),
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
        self.notice = Some(if cancelled {
            "已请求取消，但 Join 已真实成功：已写入配置并进入主界面（不丢这份实例）。".to_owned()
        } else {
            "已加入原实例：主界面显示的是该 vault 现有内容（不播种演示内容）。".to_owned()
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

    fn join_form_public_identity(&self, cx: &App) -> (String, String) {
        self.join_form.as_ref().map_or_else(
            || (String::new(), self.options.default_relay.clone()),
            |form| {
                (
                    form.instance_id.clone(),
                    form.relay.read(cx).text().trim().to_owned(),
                )
            },
        )
    }

    fn form_public_fields(&self, cx: &App) -> Vec<(String, String)> {
        let Some(form) = &self.form else {
            return Vec::new();
        };
        vec![
            ("实例 id".to_owned(), form.instance_id.clone()),
            (
                "relay".to_owned(),
                form.relay.read(cx).text().trim().to_owned(),
            ),
            (
                "实例目录".to_owned(),
                form.dir.read(cx).text().trim().to_owned(),
            ),
        ]
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
        let PendingCreate {
            outcome,
            entry,
            relay_url,
            ..
        } = pending;
        let CreateOutcome {
            vault,
            lock,
            vault_path,
            peer_id,
            key_epoch,
            recovery_code,
            ..
        } = *outcome;
        // 恢复码在进入 Main 时清除（Zeroizing 在 drop 时清零；Main 不再持有）。
        drop(recovery_code);
        let label = entry.short_label();
        let config = DevSyncConfig {
            vault: vault_path,
            service: ServiceConfig::new(relay_url),
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
        self.notice = Some(
            "实例已创建。左侧「＋ 笔记」建第一篇，保存后可在右上同步面板点「发布基线」。"
                .to_owned(),
        );
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
                self.form = None;
                self.join_form = None;
                true
            }
        }
    }
}

/// 公开字段（Error 页展示；只有路径/ID/URL）。
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
            let target = match self.state {
                State::JoinForm => self
                    .join_form
                    .as_ref()
                    .map(|form| form.recovery_code.clone()),
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

impl AppRoot {
    /// 设置面板（overlay）：实例列表 + active 切换 + relay 编辑 + 公开信息复制。
    ///
    /// 只投影公开配置；不持有 setup secret/恢复码/root/manager seed（类型上也没有）。
    /// 关闭/取消不写配置；保存只写公开配置，运行中的 `AppShell`/Vault/worker 不动。
    fn render_settings(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let (active_entry, error, notice, relay) = {
            let settings = self.settings.as_ref()?;
            (
                settings.active.clone(),
                settings.error.clone(),
                settings.notice.clone(),
                settings.relay.clone(),
            )
        };
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
                "vault_id {} · device_id {}",
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
                            "relay（只对 active 实例；只允许明文 loopback）",
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
                                .child(relay),
                        )
                        .child(hint_text(
                            "空值/非 loopback/https/带 userinfo/带路径一律内联报错且不写配置；\
                             保存成功提示「重启后生效」，不热切换 Vault/worker。",
                        ))
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
                                    "保存 relay（重启后生效）",
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
                        .child(public_section),
                )
                .into_any_element(),
        )
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
                    "{}{} · vault {}… · {}",
                    entry.short_label(),
                    if active { "（上次使用）" } else { "" },
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
        let form = self.form.as_ref().expect("CreateForm 状态必须有表单实例");
        let consent = form.consent;
        let relay = form.relay.clone();
        let secret = form.secret.clone();
        let dir = form.dir.clone();
        let form_error = form.error.clone();
        let column = page_column()
            .debug_selector(|| "app-root-create-form".to_string())
            .child(title_text("创建同步实例（Create）"))
            .child(banner_text())
            .child(
                content_column()
                    .child(label_text("relay 地址（只允许 localhost）"))
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
                    .child(label_text("实例目录（绝对路径；目录内固定 vault.sqlite3 / dev-identity.bin）"))
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
                                "创建",
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
        let dir = form.dir.clone();
        let form_error = form.error.clone();
        let column = page_column()
            .debug_selector(|| "app-root-join-form".to_string())
            .child(title_text("加入已有同步实例（Join）"))
            .child(banner_text())
            .child(
                content_column()
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
                    .child(label_text("relay 地址（只允许 localhost）"))
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
                    .child(label_text("恢复码（mnrec1…；遮罩、不落盘、不进日志）"))
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
