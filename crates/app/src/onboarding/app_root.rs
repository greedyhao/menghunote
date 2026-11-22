//! 常驻根视图 `AppRoot`（ADR 0011 决策 B′：同窗常驻根，根视图切换）。
//!
//! ## 状态机
//!
//! ```text
//! Welcome -> CreateForm -> Working -> BackupRequired -> Main
//!         \-> JoinForm  -> Working -> Main（成功即写配置，无备份页）
//!                      \-> Error（保留表单）        （保存配置失败 -> Error，可重试保存）
//! 已有有效配置 -> Main（启动直接进入，不闪 Welcome）
//! 本机 startup.json = local 且本地库存在 -> Main（重启直接回到本地库，不读 sync.json）
//! Welcome -> Main（本地（不同步）库：产品 = 用户选择的库文件，默认
//!               <用户目录>/Documents/menghunote/local/vault.sqlite3（不播种 demo）；
//!               dev-sync = 默认 dev vault；都没有同步面板）
//! 任意 -> Error（配置损坏 / 身份缺失 / 库不匹配 / 实例目录被占用 / 本地库打不开）
//! Welcome/Main -> 设置（覆盖层；Main 的 AppShell 不销毁）-> 关闭回原状态
//! ```
//!
//! **不重启切换 vault（显式用户动作，绝不热切 worker）**：
//!
//! ```text
//! Main -> 左下角 vault 切换菜单 → 点另一个已保存实例
//!         = AppShell::close_all（有界排空/flush）→ 释放旧 InstanceLock → 关菜单
//!           → 打开目标实例（成功写 active + startup=sync；失败进既有 Error 页）
//!         当前实例 = no-op（不重建、不重新加载）
//! Main -> 左下角菜单 →「管理仓库…」→ 管理仓库覆盖层
//!         实例行「打开」= 同上关旧开新；「设为默认」只写 active/startup（不热切换）
//!         「返回欢迎页」 = close_all + 释放锁 → Welcome（实例列表仍在）
//!         「新建同步 vault…」 = close_all + 释放锁 → Create 表单
//!         「加入同步 vault…」 = close_all + 释放锁 → Join 表单
//! ```
//!
//! **创建 vault 的唯一入口是「管理仓库」**：设置页不再有「实例」分类
//! （旧 `settings-ui.json` 的 `instances` 记忆回退「外观」，不崩、不空白）。
//!
//! **打开成功后写本机启动偏好**（`startup.json`：`local`/`sync`）：只记入口、不同步、
//! 不含正文；下次启动按它直接恢复。偏好缺失/损坏/本地库不存在 → Welcome（不报错页）。
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
//! | `app-root-welcome` / `welcome-vault-list` | Obsidian 式仓库选择页 / 左栏滚动列表 |
//! | `welcome-vault-<i>` | 左栏第 i 个**已保存同步实例**（active 优先；点击直接打开，不经过管理页） |
//! | `welcome-vault-local` | 左栏「本地仓库」行（`startup.json` 最近本地路径 / 默认路径存在时才出现；点击直接打开） |
//! | `welcome-new-local` / `welcome-new-local-dir` / `welcome-new-local-browse` / `welcome-new-local-confirm` | 右栏「新建本地仓库」卡与展开表单 |
//! | `welcome-open-local` / `welcome-open-local-path` / `welcome-open-local-browse` / `welcome-open-local-confirm` | 右栏「打开本地仓库」卡与展开表单 |
//! | `welcome-sync-remote` / `welcome-sync-link` / `welcome-sync-dir` / `welcome-sync-dir-browse` / `welcome-sync-confirm` | 右栏「同步远程仓库」卡（**只收一条链接**，自动识别 create/join） |
//! | `welcome-sync-name` | 统一的「仓库名称」可选输入（只影响左下角/菜单/管理仓库的显示，不改目标路径；留空 = 目录名） |
//! | `welcome-sync-consent` | `dev-sync` 构建的「同意开发同步」勾选（产品构建不渲染/不要求） |
//! | `welcome-error` | Welcome 内联错误（链接解析/目录/后端兑换失败；已脱敏） |
//! | `app-root-dev-banner` | 各页顶部的开发警示条（**只有 `dev-sync` 构建**存在；产品构建不挂节点） |
//! | `app-root-create-form` / `app-root-dir-input` | **旧** Create 表单（内部/测试保留；欢迎页不再暴露）与目录字段 |
//! | `app-root-dir-browse` / `app-root-join-dir-browse` | Create/Join 的实例目录「浏览…」（系统目录选择器） |
//! | `app-root-create-link-input` / `app-root-create-link-apply` / `app-root-create-link-clear` | Create 表单的创建链接字段 /「应用」/「移除链接」 |
//! | `app-root-create-link-warning` / `app-root-create-link-facts` | 一次性票据警告 / 预填的 relay+vault_id+API 面 |
//! | `app-root-join-form` / `app-root-join-relay-input` / `app-root-vault-id-input` / `app-root-recovery-input` / `app-root-join-dir-input` | **旧** Join 表单（恢复码内部/测试路径；欢迎页与管理仓库都不再暴露手动字段） |
//! | `app-root-remote-dev-notice` | HTTPS 初始化未真机验证的 DEV 状态标注（**只有 `dev-sync` 构建**渲染） |
//! | `app-root-consent` / `app-root-submit` / `app-root-back` / `app-root-form-error` | Create 表单的「同意开发同步」勾选（**只有 `dev-sync` 构建**渲染）/提交/返回/错误行 |
//! | `app-root-join-submit` / `app-root-join-back` / `app-root-join-error` | Join 表单的提交/返回/错误行 |
//! | `app-root-working` / `app-root-progress` / `app-root-cancel` | Working 页（Create/Join 共用） |
//! | `app-root-backup` / `app-root-vault-id` / `app-root-recovery-code` / `app-root-copy-code` / `app-root-backup-confirm` / `app-root-continue` | 备份页 |
//! | `app-root-copy-invite` / `app-root-invite-ttl` / `app-root-invite-error` | 备份页「复制邀请链接」（v2、15 分钟；token 只在剪贴板）/ 剩余有效期 / 脱敏失败 |
//! | `app-root-main` | Main（`AppShell` 子视图宿主） |
//! | `app-root-local-mode` | Main 顶部的「本地模式：不同步」提示条（**只有本地库入口**出现） |
//! | `app-root-settings` | 打开设置（Welcome 页与 Main 左下角动作行右端的紧凑按钮，宽 64px） |
//! | `app-root-bottom-actions` | Main 左下角的底部动作行容器（切换器 + 设置**同一行**，同一底边/行高） |
//! | `dev-sync-settings` | 设置覆盖层根（打开时存在；Main 仍在其下保留） |
//! | `settings-nav` / `settings-nav-<section>` | 设置页左侧分类导航 / 单个分类项 |
//! | `settings-nav-divider` | 固定分类与插件分类之间的 1px 分界线（整行不可点） |
//! | `settings-section-<section>` | 右侧内容区（**只渲染当前分类**；`settings-content` 是滚动容器） |
//! | `settings-plugin-<id>` / `-toggle` / `-state` | 「插件」分类里的第一方插件行 / 开关 / 状态（模板 `templates`、日记 `daily_notes`） |
//! | `settings-toggle-sync` | 「同步」分类的同步面板**内容容器**（进入分类即渲染；没有展开/收起） |
//! | `app-root-vault-switcher` | Main 左下角动作行里的 vault 切换按钮（当前库短名 + `▾`，占满设置按钮之外的宽度） |
//! | `app-root-vault-menu` / `app-root-vault-menu-backdrop` | 切换菜单 / 打开时铺满窗口的透明遮罩（点空白关菜单） |
//! | `app-root-vault-menu-current` | 当前 vault 行（带 `✓`；点击 = no-op 并收起菜单） |
//! | `app-root-vault-menu-instance-<i>` | 第 i 个**其它**已保存实例（名字 + 简短 API/mode；点击 = 直接切换） |
//! | `app-root-vault-menu-divider` / `app-root-vault-manage` | 菜单分隔线 /「管理仓库…」（打开管理仓库覆盖层） |
//! | `app-root-vault-manager` | 「管理仓库」覆盖层面板（标题「管理仓库」+ 关闭 `vault-manager-close`） |
//! | `app-root-vault-manager-backdrop` | 管理仓库的遮罩（点空白关闭；滚轮不穿透） |
//! | `vault-manager-instance-<i>` / `vault-manager-open-<i>` / `vault-manager-default-<i>` / `vault-manager-name` | 实例行 /「打开」（关旧开新）/「设为默认」/ 行内显示名（自定义名优先） |
//! | `vault-manager-rename-<i>` / `vault-manager-rename-row` / `vault-manager-rename-input` / `vault-manager-rename-save` / `vault-manager-rename-cancel` / `vault-manager-rename-error` | 「重命名」（行内输入；保存才写 `sync.json`，Esc/取消不改；只改显示名，不移动目录） |
//! | `vault-manager-sync-remote` / `vault-manager-back-welcome` | 同步远程仓库（**唯一入口**，回到 Welcome 展开同一个统一表单）/ 返回欢迎页 |
//! | `vault-manager-notice` | 管理仓库的内联提示（设为默认结果/失败原因） |
//! | `settings-relay` / `settings-relay-mode-hint` / `settings-save` / `settings-cancel` | relay 输入框 / 只读「按地址自动识别」说明 / 保存 / 关闭（仅在「Relay」分类） |
//! | `app-root-join-relay-hint` | Join 表单 relay 的只读模式/约束说明（自动识别，不再有单选项） |
//! | `settings-about-version` / `settings-about-build` | 关于：版本 / 构建特性（只读） |
//! | `settings-fingerprint` / `settings-fingerprint-headline` / `settings-copy-fingerprint` | 安全指纹只读区 / 标题 / 复制公开值 |
//! | `settings-security-category` | 当前安全暂停类别（无则「无安全暂停」） |
//! | `settings-close` | 非 relay 分类的关闭按钮（即时设置已保存） |
//! | `settings-error` / `settings-notice` | relay 分类的内联错误 / 提示（含「重启后生效」） |
//! | `settings-public-info` | 「同步」分类底部的 active 实例**公开**信息（vault_id/device_id/路径） |
//! | `settings-copy-vault-id` / `settings-copy-device-id` / `settings-copy-path` | 公开值复制按钮 |
//! | `settings-api-version` | active 实例的 API 面（v1 setup secret / v2 票据；在「同步」分类） |
//! | `app-root-error` / `app-root-error-message` / `app-root-retry` / `app-root-back-welcome` | Error 页 |
//! | `app-root-notice` | 底部提示行 |
//!
//! selector 只是测试元数据，业务逻辑**不得**依赖它们。

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::bail;
use gpui::prelude::*;
use gpui::{
    div, px, App, ClipboardItem, Context, Entity, FocusHandle, IntoElement, MouseButton,
    PathPromptOptions, Render, SharedString, Window,
};
use zeroize::{Zeroize, Zeroizing};

use menghunote_storage::{SyncContext, Vault};
use menghunote_sync::create_link::{CreateLink, CreateTicket};
use menghunote_sync::http_relay::Clock as _;

use crate::dev_sync::gate::DEFAULT_RELAY;
use crate::dev_sync::service::ServiceConfig;
use crate::dev_sync::{DevIdentityStore, DevSyncConfig};
use crate::dev_vault;
use crate::editor::theme;
use crate::shell::{AppShell, RIBBON_WIDTH};
use crate::shortcuts::{CommandId, KeyCombo};

use super::create::{CreateEvent, CreateJob, CreateOutcome, CreateRequest, CREATE_PUMP_INTERVAL};
use super::fingerprint::{FingerprintState, FINGERPRINT_NOTICE};
use super::input::{FormInput, FormInputEvent};
use super::invite::{
    InviteCheckRequest, InviteEvent, InviteJob, InviteOutcome, InvitePendingFacts,
    InviteRedeemRequest, INVITE_EMPTY_HINT,
};
use super::invite_copy::{CopyInviteEvent, CopyInviteJob, CopyInviteRequest, CopyInviteStage};
use super::join::{JoinEvent, JoinJob, JoinOutcome, JoinRequest};
use super::layout::{
    generate_instance_id, load_startup_preferences, normalize_display_name,
    save_startup_preferences, write_json_file_atomically, DevSyncProfile, InstanceEntry,
    ProfileError, ProfileLayout, ProfileStore, RelayApiVersion, RelayMode, StartupMode,
    StartupPreferences, VAULT_FILE_NAME,
};
use super::lock::{InstanceLock, LOCK_FILE_NAME};
use super::sanitize_error;
use super::settings::{
    infer_relay_mode, local_option_available, short_public_id, validate_relay_edit, SettingsState,
};

/// 操作预算（ADR 0011：沿用 120s；逻辑超时 ≠ 停止）。
pub const DEFAULT_OPERATION_BUDGET: Duration = Duration::from_secs(120);

/// 邀请 PENDING 等待页自动检查批准状态的间隔（用户反馈：不点按钮也要自动更新）。
///
/// 生产按真实单调时钟节流；headless 测试用
/// `cx.executor().advance_clock(INVITE_AUTO_CHECK_INTERVAL)` 推进虚拟时钟，不 sleep。
pub const INVITE_AUTO_CHECK_INTERVAL: Duration = Duration::from_secs(5);

/// `AppRoot` 的构造参数（测试注入目录/relay/instance id）。
#[derive(Debug, Clone)]
pub struct AppRootOptions {
    /// 公开配置与实例目录布局。
    pub layout: ProfileLayout,
    /// Create/Join 表单里 relay 的默认值（dev 构建 = [`DEFAULT_RELAY`]；产品构建留空，
    /// 由创建/邀请链接或用户输入给出；测试注入进程内 dev relay）。
    pub default_relay: String,
    /// 测试注入固定实例 id；`None` = 每次进入表单自动生成。
    pub instance_id: Option<String>,
    /// 是否允许**启动引导**按 `startup.json` 自动恢复本机本地库。
    ///
    /// - 产品 `sync-ui` 构建 = `true`：上次打开的本地库存在时直接回 Main；
    /// - `dev-sync` 构建 = `true`：旧「继续本地」恢复默认 dev vault（含演示树）；
    /// - [`Self::for_test`] 默认 `false`：既有测试不必显式关掉恢复逻辑。
    ///
    /// Welcome 的「新建/打开本地仓库」卡与左栏本地行**不再依赖**该开关（用户可用
    /// 的仓库选择页始终完整）；它只影响启动自动恢复。
    pub allow_local_vault: bool,
    /// 操作预算（Create 的等待上限；测试可缩短）。
    pub operation_budget: Duration,
}

impl AppRootOptions {
    /// 按平台默认目录构造（生产入口）。
    ///
    /// 只做路径解析；旧目录的一次性迁移在 [`AppRoot::new`] 的启动引导里执行——这样
    /// 旧配置损坏/版本不支持时，能在**GUI 错误页**里给出明确原因（旧 `local_dev`
    /// 对产品构建不适用时只跳过迁移，不进错误页），
    /// 而不是带着错误退出、产品看起来"什么都没发生"。
    pub fn discover(env: impl Fn(&str) -> Option<String>) -> Result<Self, ProfileError> {
        Ok(Self {
            layout: ProfileLayout::discover(env)?,
            // 产品构建没有 local_dev：不要把 loopback 地址当成默认值（会被模式校验拒绝）。
            default_relay: if local_option_available() {
                DEFAULT_RELAY.to_owned()
            } else {
                String::new()
            },
            instance_id: None,
            // 本地（不同步）入口：产品构建是「仅本地使用」，dev-sync 构建保留
            // 「继续本地（DEV ONLY）」开发入口；同一 selector，文案/行为按构建区分。
            allow_local_vault: true,
            operation_budget: DEFAULT_OPERATION_BUDGET,
        })
    }

    /// 测试用：显式目录 + relay + 固定实例 id。
    ///
    /// **默认关闭启动时的本地库自动恢复**（既有测试不必先清 `startup.json`）；
    /// Welcome 的本地仓库卡仍会渲染，要测启动恢复把它显式置 `true`。
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

/// Create 表单的**创建方式**（ADR 0018 S4b：只剩粘贴创建链接）。
///
/// 管理员创建与 v1 setup secret 创建入口已移除；枚举保留单一变体，便于测试/诊断
/// 明确断言"App 不再有其它创建方式"。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormCreateMode {
    /// 粘贴创建链接（`mn-pair://create` + 预发票据）。
    Link,
}

impl FormCreateMode {
    /// 测试/诊断用的稳定字面值（对应 selector 后缀）。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Link => "link",
        }
    }
}

/// Create 表单状态（创建链接 + 实例目录 + DEV 同意勾选）。
///
/// ADR 0018 S4b：表单**只**有创建链接与实例目录；relay 地址/模式都由链接决定，
/// 不再有管理员凭证或 v1 setup secret 字段。`consent` 只在 `dev-sync` 构建渲染/校验
/// （产品构建没有 DEV 勾选，提交直接继续）。
struct FormState {
    instance_id: String,
    /// 创建方式（只剩 [`FormCreateMode::Link`]）。
    mode_kind: FormCreateMode,
    /// 链接 scheme 决定的 relay 模式（应用链接后设置；默认 `local_dev`）。
    mode: RelayMode,
    /// 创建链接输入（**秘密**：含一次性票据；遮罩 + 禁复制 + 允许粘贴）。
    link: Entity<FormInput>,
    /// 已应用的创建链接（票据只在内存；提交时移动到 worker，不落盘）。
    link_state: Option<CreateLinkState>,
    dir: Entity<FormInput>,
    /// 「同意开发同步」勾选（**只有 `dev-sync` 构建渲染并校验**；产品构建恒为 `false`
    /// 且不检查）。
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
    /// relay 输入（模式在提交时按 scheme 推断；不再有手动选择的 `mode` 字段）。
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

/// Welcome 右栏三张操作卡（Obsidian 式仓库选择页）。
///
/// 点卡片 = 展开/收起该行的小表单，不整页跳走；真正的后台任务仍在确认后进入
/// `Working`（Create）或 `InvitePending`（邀请加入）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WelcomeAction {
    /// 新建本地仓库（选择目录 → 创建空 `vault.sqlite3`）。
    NewLocal,
    /// 打开本地仓库（选择已有 `vault.sqlite3`；缺失不创建）。
    OpenLocal,
    /// 同步远程仓库（只粘贴一条 `mn-pair://create` / `mn-pair://join` 链接）。
    SyncRemote,
}

impl WelcomeAction {
    /// 测试/诊断用的稳定字面值。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NewLocal => "new-local",
            Self::OpenLocal => "open-local",
            Self::SyncRemote => "sync-remote",
        }
    }
}

/// Welcome 页的输入控件与展开状态（Obsidian 式左列表 + 右操作卡）。
///
/// 这些控件只在 Welcome 使用；进入 Main 后仍保留（返回欢迎页时不丢用户刚输入的
/// 链接/路径），但不会写盘、不会联网。
struct WelcomeForms {
    expanded: Option<WelcomeAction>,
    /// 「新建本地仓库」的目录（创建 `<dir>/vault.sqlite3`）。
    new_local_dir: Entity<FormInput>,
    /// 「打开本地仓库」的文件路径（也接受目录，提交时自动补 `vault.sqlite3`）。
    open_local_path: Entity<FormInput>,
    /// 同步远程仓库：一条链接（创建票据或邀请链接；自动识别）。
    sync_link: Entity<FormInput>,
    /// 同步远程仓库：实例目录（Create/Join 都需要）。
    sync_dir: Entity<FormInput>,
    /// 同步远程仓库：可选的「仓库名称」（只影响显示；留空 = 回退目录名）。
    sync_name: Entity<FormInput>,
    /// `dev-sync` 构建的「同意开发同步」勾选；产品构建不渲染/不校验。
    sync_consent: bool,
    /// 本次同步表单使用的实例 id（Create 写配置、Join 等待页都用它）。
    sync_instance_id: String,
    /// Welcome 内联错误（`welcome-error`；解析失败/目录不合法都走这里）。
    error: Option<String>,
    /// Enter 提交的订阅（随 Welcome 控件一起 drop）。
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
    /// 备份页最近一次「复制邀请链接」的**公开**结果（token 只在剪贴板，不在这里）。
    invite: Option<CopiedInvite>,
    /// 备份页最近一次生成邀请的脱敏失败文案（成功后清空）。
    invite_error: Option<String>,
}

/// 备份页已复制的邀请链接的公开事实（`invite_id` + 过期时刻；**不含 token/URL**）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CopiedInvite {
    invite_id: [u8; 16],
    expires_at: u64,
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
    /// 统一同步表单填的「仓库名称」（bootstrap 完成写配置时使用；只影响显示）。
    ///
    /// `None` = 留空或**重启后 resume**（durable pending 里没有名字）：写配置时回退目录名，
    /// 之后仍可在「管理仓库 → 重命名」补名字。
    display_name: Option<String>,
    /// 公开事实（relay/vault/设备/指纹；不含 token）。
    facts: InvitePendingFacts,
    /// 实例目录锁（检查时移交 worker；失败后被 drop 则重新获取）。
    lock: Option<InstanceLock>,
    /// 是否有检查任务在途（禁用按钮，不并发）。
    busy: bool,
    /// 最近一次检查结果/错误（脱敏文案）。
    message: Option<String>,
}

/// 启动时按本机启动偏好恢复本地库的结果（只影响启动引导，不改状态机语义）。
enum LocalResume {
    /// 已进入 Main（`AppShell` 就绪）。
    Opened,
    /// 本地库文件不存在：不凭空创建，回 Welcome 并给明确提示。
    Missing,
    /// 本地库存在但打开失败（Error 页已由打开路径设置）。
    Failed,
}

/// 管理仓库里正在**重命名**的实例（行内输入；保存才写 `sync.json`）。
///
/// 打开重命名不写盘、不动目录/实例锁；Esc/取消只丢弃输入（磁盘原样保留）。
struct VaultRenameState {
    /// 目标实例 id（只改这一条）。
    instance_id: String,
    /// 行内输入控件（自绘 IME；selector `vault-manager-rename-input`）。
    input: Entity<FormInput>,
    /// 行内错误（校验失败/写盘失败；成功后整块消失）。
    error: Option<String>,
    /// 首帧是否已经主动聚焦（否则点击「重命名」后要再点一次输入框）。
    focused_once: bool,
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
    /// 备份页「复制邀请链接」的后台任务（ADR 0017 S5b）。
    invite_copy_job: Option<CopyInviteJob>,
    /// 邀请 PENDING 等待页状态（`None` = 不在等待页）。
    invite: Option<InvitePendingState>,
    /// PENDING 等待页下一次自动检查批准状态的时刻（单调时钟）。
    ///
    /// 用 `cx.background_executor().now()`（生产 = 真实单调时钟；headless = 测试
    /// 调度器的虚拟时钟，`cx.executor().advance_clock(...)` 可推进），因此自动检查
    /// 在测试里是确定性的、不需要 sleep。`None` = 不在等待页/刚离开。
    invite_auto_check_due: Option<Instant>,
    /// 已经自动发出的批准检查次数（**不含**手动点「检查状态」；测试/诊断用）。
    invite_auto_checks: u32,
    /// bootstrap 已完成、但公开配置写失败：只重试保存（不重新联网/不重新 bootstrap）。
    pending_invite: Option<(Box<InviteOutcome>, String)>,
    /// 设置页「模板」区（第一方插件；首次打开设置页时懒创建）。
    templates_settings: Option<Entity<crate::templates::TemplatesSettingsPanel>>,
    /// 设置页「日记」区（同上）。
    daily_notes_settings: Option<Entity<crate::daily_notes::DailyNotesSettingsPanel>>,
    /// 日记模板笔记选择器（覆盖层；设置页「选择模板笔记…」打开）。
    daily_template_picker: Option<Entity<crate::daily_notes::DailyTemplatePicker>>,
    /// 选择器事件订阅（关闭时 drop = 取消订阅）。
    daily_template_picker_events: Option<gpui::Subscription>,
    /// 事件回调里拿不到 `&mut Window`：置位后下一帧 render 创建选择器并聚焦。
    pending_daily_template_picker: bool,
    /// 选择器关闭后下一帧把焦点还给日记模板输入框。
    pending_focus_daily_template: bool,
    /// 第一方插件设置面板的事件订阅（启用开关变化 → 左栏插件分类立即刷新）。
    plugin_settings_subs: Vec<gpui::Subscription>,
    /// 设置面板（overlay；Main/Welcome 都可用，关闭后回到原状态）。
    settings: Option<SettingsState>,
    /// 设置页里的「管理（V2 控制面）」区是否展开（默认收起；展开时才触发网络刷新）。
    settings_management_open: bool,
    /// 设置页当前分类（本机 `settings-ui.json` 记忆；缺失/损坏/未知 key 回退「外观」）。
    settings_section: SettingsSection,
    /// 设置页「快捷键」分类：正在录制新绑定的命令（`None` = 未录制）。
    ///
    /// 录制期间根视图用 **capture 相**监听按键：先于编辑器/输入框拿到事件并
    /// `stop_propagation`，保证「录制时绝不让按键同时触发原命令/正文输入」。
    shortcut_capture: Option<CommandId>,
    /// 录制态提示（非法组合/写盘失败；每次进入录制或成功改绑时清空）。
    shortcut_capture_notice: Option<String>,
    /// 设置覆盖层焦点（录制态抢焦点；`Esc`/键盘分发需要一个焦点节点）。
    settings_focus: FocusHandle,
    /// 根视图焦点：Welcome/Create/Join 没有输入框时也要能收到全局快捷键
    /// （`Ctrl+,` 打开设置）；Main 不抢，避免干扰编辑器首帧聚焦。
    root_focus: FocusHandle,
    /// Main 左下角的 **vault 切换菜单**是否展开（Obsidian 式；不是设置页）。
    vault_menu_open: bool,
    /// **管理仓库**覆盖层是否展开（Main 里新建/加入/返回欢迎页的唯一入口）。
    vault_manager_open: bool,
    /// 切换菜单的焦点句柄（接 `Esc`；关闭后把焦点还给编辑器）。
    vault_menu_focus: FocusHandle,
    /// 管理仓库覆盖层的焦点句柄（同上）。
    vault_manager_focus: FocusHandle,
    /// 菜单首帧是否已经主动抢过焦点（否则 `Esc` 收不到；每次打开复位）。
    vault_menu_focused_once: bool,
    /// 管理仓库首帧是否已经主动抢过焦点（同上）。
    vault_manager_focused_once: bool,
    /// 管理仓库的内联提示（「设为默认」结果/失败原因；每次打开复位）。
    vault_manager_notice: Option<String>,
    /// 关闭设置后要把焦点还给 Main 的编辑器（下一帧渲染时消费）。
    pending_focus_restore: bool,
    main: Option<Entity<AppShell>>,
    /// Main 的 `AppShell` 事件订阅（底栏「同步」→ 打开 AppRoot 设置页的「同步」分类）。
    ///
    /// 随 Main 生命周期：替换/关闭 `AppShell` 时 drop 旧订阅（不再往已卸载的 shell 发事件）。
    main_shell_events: Option<gpui::Subscription>,
    lock: Option<InstanceLock>,
    instance: Option<InstanceEntry>,
    /// 本机最近一次**成功打开**的本地（不同步）库路径（来自 `startup.json` 或本次打开）。
    ///
    /// Welcome 左栏的「本地仓库」行与「打开本地仓库」表单默认值都读它；
    /// `None` 时回退 [`ProfileLayout::default_local_vault_path`]。
    local_recent_path: Option<PathBuf>,
    /// 启动引导读到的上次入口（`startup.json`；Welcome 的 active 标记用）。
    ///
    /// 缓存而不是每帧读盘：Welcome 会在 hover/滚动时重绘，不能把本机偏好文件
    /// 变成每帧 IO。
    startup_mode: Option<StartupMode>,
    /// Welcome 左列表/右操作卡的控件与展开状态（首次渲染前创建）。
    welcome: Option<WelcomeForms>,
    /// 本次统一远程表单启动的 Create 实例 id（`apply_create_event` 在 `form` 为空时用）。
    pending_create_instance: Option<String>,
    /// 本次统一远程表单启动的邀请加入实例 id（`InviteEvent::Pending` 用）。
    pending_invite_instance: Option<String>,
    /// 本次统一远程表单填写的「仓库名称」（Create 完成时写入公开配置）。
    ///
    /// 只来自 Welcome 统一表单；旧 CreateForm/JoinForm 路径恒为 `None`
    /// （不会把上一次表单的名字串到内部/测试入口上）。
    pending_create_display_name: Option<String>,
    /// 本次邀请加入填写的「仓库名称」（PENDING 页保留，bootstrap 完成时写入配置）。
    pending_invite_display_name: Option<String>,
    /// 管理仓库行内重命名状态（`None` = 没有在改名；不写盘）。
    vault_rename: Option<VaultRenameState>,
    /// Main 是否来自**本地（不同步）**库入口（产品 `local/vault.sqlite3` 或
    /// dev-sync 默认 dev vault）。只影响 Main 顶部提示条与只读投影；同步实例 Main
    /// 恒为 `false`。
    local_mode: bool,
    retry_entry: Option<InstanceEntry>,
    error: Option<ErrorState>,
    notice: Option<String>,
    pending_focus: bool,
    pump_started: bool,
    /// 系统外观（浅色/深色）变化的订阅（ADR 0021 P2；只订阅一次，drop 即取消）。
    appearance_subscription: Option<gpui::Subscription>,
    /// **测试专用**：remote_tls 初始化注入的显式信任根（DER；本地 TLS harness）。
    ///
    /// 生产恒为空（系统信任根）。只增加显式信任，不关闭证书链/SAN/有效期校验。
    test_relay_roots: Vec<Vec<u8>>,
    /// **测试专用**：让 Create 的自动发布基线走故意不可达的 relay（模拟发布失败）。
    ///
    /// 生产恒为 `false`；只影响发布一步，库/身份/恢复码仍真实创建并保留。
    test_baseline_unreachable: bool,
}

impl AppRoot {
    /// 构造根视图并尝试自动打开已保存实例。
    pub fn new(options: AppRootOptions, cx: &mut Context<Self>) -> Self {
        // 本机 UI 偏好（侧栏排序）在产品根视图初始化一次；`AppShell` 只读缓存值，
        // 这样 headless 测试不会因为本机残留 `sidebar.json` 而改变默认顺序。
        crate::shell::init_sidebar_sort_from_disk();
        // 快捷键按**本布局的数据根**重新加载：显式布局（测试/便携）不会读到用户
        // 目录；缺失/损坏回退默认值，不阻塞启动。文件在数据根下，不进 vault。
        // `MENGHUNOTE_SHORTCUTS_PATH` 显式覆盖优先（测试/便携），与 `init_from_disk` 一致。
        let shortcuts_path = std::env::var_os(crate::shortcuts::SHORTCUTS_PATH_ENV)
            .map(PathBuf::from)
            .unwrap_or_else(|| options.layout.shortcuts_path());
        crate::shortcuts::init_from_path(shortcuts_path);
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
            invite_copy_job: None,
            invite: None,
            invite_auto_check_due: None,
            invite_auto_checks: 0,
            pending_invite: None,
            templates_settings: None,
            daily_notes_settings: None,
            daily_template_picker: None,
            daily_template_picker_events: None,
            pending_daily_template_picker: false,
            pending_focus_daily_template: false,
            plugin_settings_subs: Vec::new(),
            settings: None,
            settings_management_open: false,
            settings_section: SettingsSection::default(),
            shortcut_capture: None,
            shortcut_capture_notice: None,
            settings_focus: cx.focus_handle(),
            root_focus: cx.focus_handle(),
            vault_menu_open: false,
            vault_manager_open: false,
            vault_menu_focus: cx.focus_handle(),
            vault_manager_focus: cx.focus_handle(),
            vault_menu_focused_once: false,
            vault_manager_focused_once: false,
            vault_manager_notice: None,
            pending_focus_restore: false,
            main: None,
            main_shell_events: None,
            lock: None,
            instance: None,
            local_recent_path: None,
            startup_mode: None,
            welcome: None,
            pending_create_instance: None,
            pending_invite_instance: None,
            pending_create_display_name: None,
            pending_invite_display_name: None,
            vault_rename: None,
            local_mode: false,
            retry_entry: None,
            error: None,
            notice: None,
            pending_focus: false,
            pump_started: false,
            appearance_subscription: None,
            test_relay_roots: Vec::new(),
            test_baseline_unreachable: false,
        };
        root.boot(cx);
        // Welcome 的控件在首帧前就建好：启动引导已经把 `startup.json` 的本地路径
        // 读进 `local_recent_path`，表单默认值因此与恢复逻辑一致。
        root.ensure_welcome_forms(cx);
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

    /// 启动引导：本机启动偏好 → 旧目录一次性迁移 → 读公开配置 → 有 active 且有效就自动进 Main。
    fn boot(&mut self, cx: &mut Context<Self>) {
        self.state = State::Welcome;
        self.error = None;
        // 0) 本机启动偏好（不同步、不含正文）：上次成功打开的是本地库就直接回去。
        //
        //    - 本地库存在 → 直接进 Main：**不读 `sync.json`、不迁移旧配置**，本地使用
        //      不被同步配置的损坏/迁移挡住，也不联网；
        //    - 本地库不存在 → 回 Welcome + 明确提示（**不凭空创建库**）；实例列表
        //      容错读取，不把同步配置的问题升级成错误页；
        //    - 偏好缺失/损坏/`sync` → 沿用下面的 active 实例自动打开逻辑。
        if self.options.allow_local_vault {
            if let Some(preferences) = load_startup_preferences(&self.options.layout.startup_path())
            {
                self.startup_mode = Some(preferences.mode);
                if preferences.mode == StartupMode::Local {
                    // 旧格式没有 `local_vault_path`：优先打开旧数据根下仍存在的
                    // `local/vault.sqlite3`（老用户看起来不丢库），否则用新的用户目录默认。
                    let path = self.resumed_local_vault_path(&preferences);
                    self.local_recent_path = Some(path.clone());
                    match self.resume_local_vault(&path, cx) {
                        LocalResume::Opened | LocalResume::Failed => return,
                        LocalResume::Missing => {
                            self.notice = Some(crate::ui_text::local_resume_missing(
                                &path.display().to_string(),
                            ));
                            self.load_profile_for_welcome();
                            return;
                        }
                    }
                } else if let Some(path) = &preferences.local_vault_path {
                    // 上次打开的是同步实例，但本地路径仍记得：Welcome 继续显示它。
                    self.local_recent_path = Some(path.clone());
                }
            }
        }
        // P2：旧目录（`menghunote-dev/dev-sync.json`）的一次性保守迁移。显式布局
        // （`ProfileLayout::new`）没有旧路径，因此测试/第三方注入完全不受影响。
        match self.options.layout.migrate_legacy_profile() {
            Ok(true) => {
                eprintln!(
                    "[menghunote] 已迁移旧同步配置：{} → {}（库路径与旧目录保持原样）",
                    self.options
                        .layout
                        .legacy_config_path
                        .as_deref()
                        .map_or_else(|| "<旧配置>".to_owned(), |path| path.display().to_string()),
                    self.store.path().display()
                );
            }
            Ok(false) => {}
            Err(error) => {
                // 旧配置损坏/版本不支持/模式不被本构建允许：不重建、不覆盖；用现有
                // Error 页明确提示（含旧配置路径），绝不静默白屏或改写模式。
                let path = self
                    .options
                    .layout
                    .legacy_config_path
                    .as_deref()
                    .map_or_else(String::new, |path| path.display().to_string());
                self.set_error(
                    "旧同步配置无法迁移",
                    error.to_string(),
                    vec![("旧配置文件".to_owned(), path)],
                    RetryKind::ReloadConfig,
                );
                return;
            }
        }
        self.load_profile_and_auto_open(cx);
    }

    /// 读公开配置：损坏/索引损坏 → Error；有 active → 尝试自动打开。
    fn load_profile_and_auto_open(&mut self, cx: &mut Context<Self>) {
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

    /// Welcome 的容错读盘（本机偏好 `local` 但本地库不存在时用）：
    /// 只把可用的公开实例列表填进 `profile`，**不自动打开**、不升级成错误页。
    fn load_profile_for_welcome(&mut self) {
        match self.store.load() {
            Ok(Some(profile)) => self.profile = Some(profile),
            Ok(None) => {}
            Err(error) => {
                // 本次启动的主因是本地库缺失；同步配置的问题留给打开设置/重试时的
                // 明确错误，不在这里把 Welcome 换成错误页。
                eprintln!("[menghunote] 启动偏好为 local，但同步配置不可读：{error}");
            }
        }
    }

    /// 启动时按本机偏好解析要恢复的本地库路径（**只解析，不创建**）。
    ///
    /// 优先级：
    /// 1. 新格式记下的 `local_vault_path`（用户上次选的绝对路径）；
    /// 2. 旧格式（没有该字段）/dev-sync：旧数据根下仍存在的 `local/vault.sqlite3`
    ///    ——老用户升级后看起来不会"丢库"；
    /// 3. 新默认：`<用户目录>/Documents/menghunote/local/vault.sqlite3`
    ///    （Documents 不存在则 HOME 下；显式注入布局仍是数据根）。
    fn resumed_local_vault_path(&self, preferences: &StartupPreferences) -> PathBuf {
        if let Some(path) = &preferences.local_vault_path {
            return path.clone();
        }
        if cfg!(feature = "dev-sync") {
            return dev_vault::dev_vault_path();
        }
        let legacy = self.options.layout.local_vault_path();
        if legacy.is_file() {
            legacy
        } else {
            self.options.layout.default_local_vault_path()
        }
    }

    /// 按本机启动偏好恢复上次的本地库；**只检查路径，不凭空创建库**。
    ///
    /// - 文件不存在 → [`LocalResume::Missing`]（调用方回 Welcome + 明确提示）；
    /// - 打开失败 → [`LocalResume::Failed`]（打开路径已按既有语义设置 Error 页）；
    /// - 成功 → [`LocalResume::Opened`]（Main 已就绪，且已写回 `local` 偏好）。
    fn resume_local_vault(&mut self, path: &Path, cx: &mut Context<Self>) -> LocalResume {
        if !path.is_file() {
            return LocalResume::Missing;
        }
        if cfg!(feature = "dev-sync") {
            // 旧 dev 偏好没有 `local_vault_path`：继续打开默认 dev vault（含演示树）。
            // 新 Welcome「新建/打开本地仓库」会写实际路径；有路径时必须复用同一个库，
            // 不能把用户选的本地库换成 dev vault。
            let has_recorded_path = load_startup_preferences(&self.options.layout.startup_path())
                .and_then(|preferences| preferences.local_vault_path)
                .is_some();
            if has_recorded_path {
                self.open_local_only_vault_at(path.to_path_buf(), cx);
            } else {
                self.open_local_dev_vault(cx);
            }
        } else {
            self.open_local_only_vault_at(path.to_path_buf(), cx);
        }
        if self.state == State::Main {
            LocalResume::Opened
        } else {
            LocalResume::Failed
        }
    }

    /// Welcome 控件（首次渲染/首次动作前创建；幂等）。
    ///
    /// 默认值全部来自公开路径解析：
    /// - 「新建本地仓库」= `default_local_vault_path()` 的父目录（用户文档目录下）；
    /// - 「打开本地仓库」= 本机 `startup.json` 里最近的本地路径，缺失则默认路径；
    /// - 「同步远程仓库」= `default_instance_dir()`（用户文档目录下，可修改）。
    fn ensure_welcome_forms(&mut self, cx: &mut Context<Self>) {
        if self.welcome.is_some() {
            return;
        }
        let default_local = self.options.layout.default_local_vault_path();
        let new_dir = default_local
            .parent()
            .map_or_else(|| default_local.clone(), Path::to_path_buf);
        let open_path = self.welcome_open_default_path();
        let instance_id = self
            .options
            .instance_id
            .clone()
            .unwrap_or_else(generate_instance_id);
        let sync_dir = self
            .options
            .layout
            .default_instance_dir(&instance_id)
            .unwrap_or_else(|_| {
                self.options
                    .layout
                    .default_vault_root()
                    .join("instances")
                    .join(&instance_id)
            });
        let new_dir_text = new_dir.display().to_string();
        let open_path_text = open_path.display().to_string();
        let sync_dir_text = sync_dir.display().to_string();
        let new_local_dir = cx.new(|cx| {
            let mut input = FormInput::new(new_dir_text.clone(), false, cx)
                .selector("welcome-new-local-dir")
                .click_to_focus();
            input.set_text(&new_dir_text);
            input
        });
        let open_local_path = cx.new(|cx| {
            let mut input = FormInput::new(open_path_text.clone(), false, cx)
                .selector("welcome-open-local-path")
                .click_to_focus();
            input.set_text(&open_path_text);
            input
        });
        let sync_link = cx.new(|cx| {
            FormInput::new("粘贴 mn-pair://create 或 mn-pair://join 链接", true, cx)
                .selector("welcome-sync-link")
                .click_to_focus()
        });
        let sync_dir_input = cx.new(|cx| {
            let mut input = FormInput::new(sync_dir_text.clone(), false, cx)
                .selector("welcome-sync-dir")
                .click_to_focus();
            input.set_text(&sync_dir_text);
            input
        });
        // 可选「仓库名称」：空 = 回退目录名；只影响显示（不改目标路径）。
        let sync_name_input = cx.new(|cx| {
            FormInput::new(crate::ui_text::vault_name_placeholder(), false, cx)
                .selector("welcome-sync-name")
                .click_to_focus()
        });
        let mut subs = Vec::new();
        subs.push(cx.subscribe(
            &new_local_dir,
            |root, _input, event: &FormInputEvent, cx| {
                if matches!(event, FormInputEvent::Submit) {
                    root.welcome_confirm_new_local(cx);
                }
            },
        ));
        subs.push(cx.subscribe(
            &open_local_path,
            |root, _input, event: &FormInputEvent, cx| {
                if matches!(event, FormInputEvent::Submit) {
                    root.welcome_confirm_open_local(cx);
                }
            },
        ));
        subs.push(
            cx.subscribe(&sync_link, |root, _input, event: &FormInputEvent, cx| {
                if matches!(event, FormInputEvent::Submit) {
                    root.welcome_confirm_sync(cx);
                }
            }),
        );
        subs.push(cx.subscribe(
            &sync_dir_input,
            |root, _input, event: &FormInputEvent, cx| {
                if matches!(event, FormInputEvent::Submit) {
                    root.welcome_confirm_sync(cx);
                }
            },
        ));
        subs.push(cx.subscribe(
            &sync_name_input,
            |root, _input, event: &FormInputEvent, cx| {
                if matches!(event, FormInputEvent::Submit) {
                    root.welcome_confirm_sync(cx);
                }
            },
        ));
        self.welcome = Some(WelcomeForms {
            expanded: None,
            new_local_dir,
            open_local_path,
            sync_link,
            sync_dir: sync_dir_input,
            sync_name: sync_name_input,
            sync_consent: false,
            sync_instance_id: instance_id,
            error: None,
            _subs: subs,
        });
    }

    /// 「打开本地仓库」表单的默认路径：本机最近打开的本地库，否则用户目录默认。
    fn welcome_open_default_path(&self) -> PathBuf {
        self.local_recent_path
            .clone()
            .unwrap_or_else(|| self.options.layout.default_local_vault_path())
    }

    /// Welcome 左栏「本地仓库」行：只返回**真实存在**的本地库。
    ///
    /// 优先级：`startup.json` 最近路径 → `default_local_vault_path()`；两处都不存在时
    /// 不显示该行（右栏仍可新建）。
    #[must_use]
    pub fn welcome_local_row_path(&self) -> Option<PathBuf> {
        if let Some(path) = &self.local_recent_path {
            if path.is_file() {
                return Some(path.clone());
            }
        }
        let default = self.options.layout.default_local_vault_path();
        default.is_file().then_some(default)
    }

    /// 「打开本地仓库」输入框里的当前路径文本（Welcome 未创建时回退最近路径）。
    #[must_use]
    pub fn local_path_text(&self, cx: &App) -> Option<String> {
        if let Some(welcome) = &self.welcome {
            return Some(welcome.open_local_path.read(cx).text().trim().to_owned());
        }
        self.local_recent_path
            .as_ref()
            .map(|path| path.display().to_string())
    }

    /// 测试/预填：设置「打开本地仓库」的路径。
    pub fn local_set_path(&mut self, text: &str, cx: &mut Context<Self>) {
        self.ensure_welcome_forms(cx);
        let Some(welcome) = &self.welcome else {
            return;
        };
        let text = text.to_owned();
        welcome.open_local_path.update(cx, |input, cx| {
            input.set_text(&text);
            cx.notify();
        });
    }

    /// 「浏览…」：系统文件/目录选择器 → 填进「打开本地仓库」路径。
    ///
    /// 选择器只填文本，不打开、不创建；真正校验在
    /// [`Self::welcome_confirm_open_local`]（缺失明确报错、绝不新建）。
    pub fn local_browse_vault_path(&mut self, cx: &mut Context<Self>) {
        self.ensure_welcome_forms(cx);
        if self.welcome.is_none() {
            return;
        }
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: true,
            multiple: false,
            prompt: Some(SharedString::from("选择已有 vault.sqlite3（或目录）")),
        });
        cx.spawn(async move |root, cx| {
            if let Ok(Ok(Some(paths))) = receiver.await {
                if let Some(path) = paths.first() {
                    let text = path.display().to_string();
                    let _ = root.update(cx, |root, cx| root.local_set_path(&text, cx));
                }
            }
        })
        .detach();
    }

    /// 把「打开本地仓库」输入框同步成实际要打开的路径（打开成功后调用；不写盘）。
    fn set_local_path_text(&mut self, path: &Path, cx: &mut Context<Self>) {
        self.local_recent_path = Some(path.to_path_buf());
        if let Some(welcome) = &self.welcome {
            let text = path.display().to_string();
            welcome.open_local_path.update(cx, |input, cx| {
                input.set_text(&text);
                cx.notify();
            });
        }
    }

    /// 把用户选的「目录或文件」解析成真正的 `vault.sqlite3` 路径。
    ///
    /// - 已存在的目录 / 没有扩展名的输入 → 自动补 `vault.sqlite3`；
    /// - 以 `.sqlite3`（或其它扩展名）结尾 / 已存在的文件 → 原样使用。
    fn resolve_local_vault_selection(path: PathBuf) -> PathBuf {
        if path.is_dir() {
            return path.join(VAULT_FILE_NAME);
        }
        if path.is_file() {
            return path;
        }
        let looks_like_file = path
            .extension()
            .is_some_and(|extension| extension == "sqlite3");
        if looks_like_file {
            path
        } else {
            path.join(VAULT_FILE_NAME)
        }
    }

    /// 记住上次成功打开的入口（本机 `startup.json`；失败只记日志，不影响本次打开）。
    ///
    /// 打开同步实例时**保留**上次本地库路径（换入口不忘位置）；打开本地库时写入
    /// 本次实际路径，供下次启动直接恢复。
    fn remember_startup_preferences(
        &mut self,
        mode: StartupMode,
        local_vault_path: Option<PathBuf>,
    ) {
        self.startup_mode = Some(mode);
        let path = self.options.layout.startup_path();
        let local_vault_path = local_vault_path.or_else(|| {
            load_startup_preferences(&path).and_then(|preferences| preferences.local_vault_path)
        });
        let preferences = StartupPreferences {
            mode,
            local_vault_path,
        };
        if let Err(error) = save_startup_preferences(&path, &preferences) {
            eprintln!(
                "[menghunote] 保存启动偏好失败：{}（{error}）",
                path.display()
            );
        }
    }

    /// 只记入口（同步实例；保留已记的本地路径）。
    fn remember_startup_mode(&mut self, mode: StartupMode) {
        self.remember_startup_preferences(mode, None);
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
        self.set_main_shell(shell, cx);
        self.lock = Some(lock);
        self.instance = Some(entry.clone());
        self.local_mode = false;
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
        // 本机启动偏好：下次启动直接回到这个同步实例（不写进 sync.json/正文）。
        self.remember_startup_mode(StartupMode::Sync);
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
        self.main_shell_events = None;
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

    /// Main 是否来自**本地（不同步）**入口（测试/诊断；同步实例 Main 恒为 `false`）。
    #[must_use]
    pub const fn is_local_mode(&self) -> bool {
        self.local_mode
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

    /// 订阅系统外观变化（ADR 0021 P2）：只订阅一次，首帧先按当前 OS 外观解析。
    ///
    /// 回调只改全局主题并 `refresh_windows()`；不重建 `AppShell`/编辑器、不写正文。
    fn observe_appearance_once(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.appearance_subscription.is_some() {
            return;
        }
        crate::theme::apply_system_appearance(window.appearance(), cx);
        self.appearance_subscription =
            Some(cx.observe_window_appearance(window, |_root, window, cx| {
                crate::theme::apply_system_appearance(window.appearance(), cx);
            }));
    }

    /// 打开设置面板（Welcome / Main 可用；Main 里 `AppShell` 保留不销毁）。
    ///
    /// 打开时重新读一次公开配置：设置面板显示的是**配置文件当前内容**（外部手改也
    /// 能看到），而不是某个缓存的旧副本。读失败只在面板内联报错，不覆盖配置、不重建
    /// 实例。
    pub fn open_settings(&mut self, cx: &mut Context<Self>) {
        if self.settings.is_some() || !matches!(self.state, State::Welcome | State::Main) {
            return;
        }
        // 重新打开设置页不残留录制态/提示。
        self.shortcut_capture = None;
        self.shortcut_capture_notice = None;
        // 重新打开设置页不残留上一次的选择器（只在设置页内可用）。
        self.daily_template_picker = None;
        self.daily_template_picker_events = None;
        self.pending_daily_template_picker = false;
        self.pending_focus_daily_template = false;
        // 设置页/左下角切换菜单/管理仓库三选一：打开设置时收起另外两个覆盖层。
        self.clear_vault_overlays();
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
        // 第一方插件设置区（模板/日记）：首次打开设置页时创建（面板持有 IME 输入框）。
        if self.templates_settings.is_none() {
            self.templates_settings = Some(cx.new(crate::templates::TemplatesSettingsPanel::new));
        }
        if self.daily_notes_settings.is_none() {
            let panel = cx.new(crate::daily_notes::DailyNotesSettingsPanel::new);
            self.plugin_settings_subs.push(cx.subscribe(
                &panel,
                |root: &mut AppRoot,
                 _panel: Entity<crate::daily_notes::DailyNotesSettingsPanel>,
                 event: &crate::daily_notes::DailyNotesSettingsEvent,
                 cx| match *event {
                    crate::daily_notes::DailyNotesSettingsEvent::PickTemplateRequested => {
                        root.request_daily_template_picker(cx);
                    }
                },
            ));
            self.daily_notes_settings = Some(panel);
        }
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
        // 模式不再有面板状态：保存时按输入框文本推断（https → remote_tls，
        // http → local_dev），UI 只展示只读说明。
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
        self.settings_management_open = false;
        // 本机记忆：恢复到上次停留的分类（缺失/损坏/未知 key → 外观；已关闭的
        // 插件分类由下面的 clamp 回退到「插件」）。
        self.settings_section = load_settings_section(&self.options.layout.settings_ui_path());
        self.clamp_settings_section();
        self.settings = Some(SettingsState {
            active,
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

    /// 底栏「同步」点击（`AppShell` 事件）的宿主路由：打开 AppRoot 设置页并选中
    /// [`SettingsSection::Sync`]。
    ///
    /// 生产 Main 唯一走这里：不再打开 `AppShell` 的旧组合设置页，因此「同步」分类
    /// 左侧是分类导航、右侧只有同步面板内容，下方不会混入外观/语言/模板。
    pub fn open_sync_settings(&mut self, cx: &mut Context<Self>) {
        if self.settings.is_none() {
            self.open_settings(cx);
        }
        self.settings_select_section(SettingsSection::Sync, cx);
    }

    /// 挂上 Main 的 `AppShell`，并把设置入口交给宿主（AppRoot）统一路由。
    ///
    /// - `set_settings_host_managed(true)`：底栏 `sync-status` 只发
    ///   [`crate::shell::AppShellEvent::OpenSyncSettings`]，不再打开组合设置页；
    /// - 订阅随 `main_shell_events` 保存：替换/关闭 Main 时 drop（旧订阅自动取消）。
    fn set_main_shell(&mut self, shell: Entity<AppShell>, cx: &mut Context<Self>) {
        shell.update(cx, |shell, _cx| {
            shell.set_settings_host_managed(true);
            // 全局快捷键（新建笔记/图谱）由 AppRoot 统一拦截：它能看见设置/管理
            // 仓库/切换菜单这些宿主覆盖层，避免 AppShell 根上再处理一遍造成误触。
            shell.set_global_shortcut_direct(false);
        });
        let subscription = cx.subscribe(
            &shell,
            |root, _shell, event: &crate::shell::AppShellEvent, cx| match *event {
                crate::shell::AppShellEvent::OpenSyncSettings => root.open_sync_settings(cx),
            },
        );
        self.main_shell_events = Some(subscription);
        self.main = Some(shell);
        // 进入 Main 时把焦点交给 `AppShell` 的活动编辑器：Welcome 的输入框/按钮
        // 可能仍拿着焦点，否则 Ctrl+, / Ctrl+N 等宿主快捷键收不到（下一帧消费）。
        self.pending_focus_restore = true;
    }

    /// 关闭设置面板：**不写配置**；Main 的 `AppShell`/文档状态原样保留。
    pub fn close_settings(&mut self, cx: &mut Context<Self>) {
        if self.settings.take().is_none() {
            return;
        }
        self.settings_management_open = false;
        // 关闭设置 = 退出录制态（未确认的按键不落任何绑定）。
        self.shortcut_cancel_capture(cx);
        // 设置关闭 = 选择器随之卸载，不残留到下一次打开。
        self.daily_template_picker = None;
        self.daily_template_picker_events = None;
        self.pending_daily_template_picker = false;
        self.pending_focus_daily_template = false;
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

    /// 设置页「日记」区实体（测试/诊断用；`None` = 设置页还没打开过）。
    #[doc(hidden)]
    #[must_use]
    pub fn daily_notes_settings_panel(
        &self,
    ) -> Option<Entity<crate::daily_notes::DailyNotesSettingsPanel>> {
        self.daily_notes_settings.clone()
    }

    /// 测试/诊断：日记模板选择器是否打开。
    #[doc(hidden)]
    #[must_use]
    pub fn daily_template_picker_is_open(&self) -> bool {
        self.daily_template_picker.is_some()
    }

    /// 「选择模板笔记…」只发事件：这里只置位，下一帧 render 才拿得到 `&mut Window`。
    fn request_daily_template_picker(&mut self, cx: &mut Context<Self>) {
        if self.settings.is_none() {
            return;
        }
        self.pending_daily_template_picker = true;
        cx.notify();
    }

    /// 组装并打开日记模板笔记选择器（覆盖层；列出当前库**全部普通笔记**）。
    ///
    /// 没有打开的库（Welcome / 打开失败）时列表为空：选择器显示明确空态，
    /// 不发 `Picked`、不写偏好（绝不写空路径）。
    fn open_daily_template_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings.is_none() {
            return;
        }
        let current = crate::daily_notes::preferences().normalized_template();
        let choices = self.daily_template_choices(cx);
        // 上一次关闭留下的"还焦点"不能抢走新选择器的搜索焦点。
        self.pending_focus_daily_template = false;
        let picker =
            cx.new(|cx| crate::daily_notes::DailyTemplatePicker::new(choices, current, cx));
        let events = cx.subscribe(
            &picker,
            |root: &mut AppRoot,
             _picker,
             event: &crate::daily_notes::DailyTemplatePickerEvent,
             cx| match *event {
                crate::daily_notes::DailyTemplatePickerEvent::Picked { index } => {
                    root.on_daily_template_picked(index, cx);
                }
                crate::daily_notes::DailyTemplatePickerEvent::Closed => {
                    root.close_daily_template_picker(cx);
                }
            },
        );
        picker.update(cx, |picker, cx| picker.focus(window, cx));
        self.daily_template_picker = Some(picker);
        self.daily_template_picker_events = Some(events);
        self.notice = None;
        cx.notify();
    }

    /// 当前库的全部普通笔记（只读快照；`AppShell` 未打开 = 空）。
    fn daily_template_choices(&self, cx: &App) -> Vec<crate::daily_notes::DailyTemplateChoice> {
        let Some(shell) = self.main.as_ref() else {
            return Vec::new();
        };
        crate::daily_notes::collect_template_choices(shell.read(cx).tree_snapshot())
    }

    /// 选中一篇笔记：写回日记模板字段并立即保存（原子写），关闭选择器。
    fn on_daily_template_picked(&mut self, index: usize, cx: &mut Context<Self>) {
        let path = self.daily_template_picker.as_ref().and_then(|picker| {
            picker
                .read(cx)
                .choice(index)
                .map(|choice| choice.path.clone())
        });
        self.close_daily_template_picker(cx);
        let Some(path) = path else {
            return;
        };
        match self.daily_notes_settings.clone() {
            Some(panel) => {
                panel.update(cx, |panel, cx| panel.apply_template_path(&path, cx));
            }
            None => {
                // 防御路径：设置面板尚未创建时只写偏好（不 panic、不写空路径）。
                let mut preferences = crate::daily_notes::preferences();
                preferences.template = crate::daily_notes::normalize_note_path(&path);
                crate::daily_notes::set_preferences(preferences);
            }
        }
        self.notice = Some(crate::ui_text::daily_notes_saved());
        cx.notify();
    }

    /// 关闭选择器并把焦点还给日记模板输入框（下一帧，等覆盖层真正卸下）。
    fn close_daily_template_picker(&mut self, cx: &mut Context<Self>) {
        if self.daily_template_picker.take().is_none() {
            return;
        }
        self.daily_template_picker_events = None;
        if self.settings.is_some() {
            self.pending_focus_daily_template = true;
        }
        cx.notify();
    }

    /// 设置页当前分类（默认 [`SettingsSection::Appearance`]；本机 `settings-ui.json` 记忆）。
    ///
    /// 打开设置页时从磁盘恢复（缺失/损坏/未知 key → 外观；已关闭插件分类 → 插件）。
    #[must_use]
    pub const fn settings_section(&self) -> SettingsSection {
        self.settings_section
    }

    /// 切换设置页分类（点左侧导航；只重绘 + 写本机分类记忆，不做网络/配置 IO）。
    ///
    /// 设置没打开时是 no-op（导航不存在）；同一分类重复点击只重绘一次、不重复写盘。
    /// 已关闭的插件分类不可选中：目标回退到「插件」页（它没有导航项）。
    pub fn settings_select_section(&mut self, section: SettingsSection, cx: &mut Context<Self>) {
        if self.settings.is_none() {
            return;
        }
        let section = if section.is_plugin() && !section.is_enabled() {
            SettingsSection::Plugins
        } else {
            section
        };
        if self.settings_section != section {
            // 切换分类 = 离开快捷键页：录制态必须取消（不能把按键录到看不见的行上）。
            self.shortcut_cancel_capture(cx);
            self.settings_section = section;
            // 本机 `settings-ui.json`（只存分类短名；失败只记日志，本会话仍然生效）。
            save_settings_section(&self.options.layout.settings_ui_path(), section);
            cx.notify();
        }
    }

    /// 切换第一方插件总开关（「插件」分类里的行；固定分类传入时 no-op）。
    ///
    /// 直接复用插件自己的持久化（`templates.json` / `daily-notes.json` 的 `enabled`），
    /// **不新建第二份开关状态**；关闭当下若正停在那个插件分类，立即回退到「插件」。
    /// 侧栏 ribbon 由 `AppShell` 按同一份 `enabled` 渲染，这里只触发一次重绘。
    pub fn settings_toggle_plugin(&mut self, section: SettingsSection, cx: &mut Context<Self>) {
        if self.settings.is_none() || !section.is_plugin() {
            return;
        }
        match section {
            SettingsSection::Templates => {
                let mut preferences = crate::templates::preferences();
                preferences.enabled = !preferences.enabled;
                crate::templates::set_preferences(preferences);
            }
            SettingsSection::DailyNotes => {
                let mut preferences = crate::daily_notes::preferences();
                preferences.enabled = !preferences.enabled;
                crate::daily_notes::set_preferences(preferences);
            }
            _ => return,
        }
        self.after_plugin_enabled_changed(cx);
    }

    fn after_plugin_enabled_changed(&mut self, cx: &mut Context<Self>) {
        self.clamp_settings_section();
        // 侧栏 ribbon 的入口按同一份 `enabled` 渲染：通知子视图重绘（不重建 AppShell）。
        if let Some(shell) = self.main.clone() {
            shell.update(cx, |_shell, cx| cx.notify());
        }
        cx.notify();
    }

    /// 当前分类是已关闭的插件分类时回退到「插件」（避免空白页）。
    fn clamp_settings_section(&mut self) {
        if self.settings_section.is_plugin() && !self.settings_section.is_enabled() {
            self.settings_section = SettingsSection::Plugins;
        }
    }

    /// 展开/收起设置页里的「管理（V2 控制面）」区（ADR 0016 S4d1c）。
    ///
    /// 展开时对管理面板调一次 `activate`（触发首次刷新）；收起不改变任何状态。
    /// V2 管理是 dev-only 实验：产品 `sync-ui` 构建不编译本入口。
    #[cfg(feature = "dev-sync")]
    pub fn settings_toggle_management(&mut self, cx: &mut Context<Self>) {
        if self.settings.is_none() {
            return;
        }
        self.settings_management_open = !self.settings_management_open;
        if self.settings_management_open {
            if let Some(panel) = self
                .main
                .as_ref()
                .and_then(|shell| shell.read(cx).management_panel())
            {
                panel.update(cx, |panel: &mut crate::dev_sync::ManagementPanel, cx| {
                    panel.activate(cx)
                });
            }
        }
        cx.notify();
    }

    /// 设置页「管理」区是否展开（测试/投影用）。
    #[must_use]
    pub const fn settings_management_open(&self) -> bool {
        self.settings_management_open
    }

    /// V2 管理区（dev-sync 实验）：产品 `sync-ui` 构建恒为 `None`。
    #[cfg(feature = "sync-ui")]
    fn v2_management_block_if_enabled(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        #[cfg(feature = "dev-sync")]
        {
            if !crate::dev_sync::experimental_v2_ui() {
                return None;
            }
            let management_panel = self
                .main
                .as_ref()
                .and_then(|shell| shell.read(cx).management_panel());
            let mut block = div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .w_full()
                .pt(px(4.0))
                .border_t_1()
                .border_color(theme::sidebar_border())
                .child(section_text(if self.settings_management_open {
                    "管理（实验：V2 控制面 / 通道 A 双签，已展开）"
                } else {
                    "管理（实验：V2 控制面 / 通道 A 双签，默认收起）"
                }))
                .child(action_button(
                    "settings-toggle-management",
                    if self.settings_management_open {
                        "收起"
                    } else {
                        "展开"
                    },
                    false,
                    false,
                    cx,
                    |root, cx| root.settings_toggle_management(cx),
                ));
            if self.settings_management_open {
                if let Some(panel) = management_panel {
                    block = block.child(div().w_full().child(panel));
                } else {
                    block = block.child(hint_text(
                        "管理面板不可用：当前实例没有真实库路径/管理执行体",
                    ));
                }
            }
            Some(block.into_any_element())
        }
        #[cfg(not(feature = "dev-sync"))]
        {
            let _ = cx;
            None
        }
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

    /// 公开配置里的已保存实例（active 优先；设置页/管理仓库/测试共用的只读投影）。
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
    /// - 模式按输入地址推断（https → remote_tls，http → local_dev），再交给权威
    ///   parser 做模式/URL 校验；非法 → 内联错误，**不写配置**；
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
            self.set_settings_error(crate::ui_text::settings_no_active_save_error());
            cx.notify();
            return;
        };
        // relay 模式不再由用户选择：从地址推断（不复制 URL 校验，最终仍走权威 parser）。
        let mode = infer_relay_mode(&raw_relay);
        // remote_tls 不需要额外"确认连接 HTTPS"勾选：模式 + canonical HTTPS origin
        // 只由权威 parser 校验（ADR 0013 §1 的旧显式确认已按用户反馈移除；确认端点
        // 仍不等于授权接受服务器任意 hash，pin 依旧独立校验）。
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

    // ---- Main 左下角：vault 切换（Obsidian 式） + 管理仓库覆盖层 ----

    /// 左下角切换菜单：打开（只在 Main 可用；不是设置页）。
    pub fn open_vault_menu(&mut self, cx: &mut Context<Self>) {
        if self.state != State::Main || self.vault_menu_open {
            return;
        }
        // 菜单与管理仓库互斥；打开菜单不碰主界面。
        self.vault_manager_open = false;
        self.vault_manager_focused_once = false;
        self.vault_manager_notice = None;
        self.vault_menu_open = true;
        self.vault_menu_focused_once = false;
        cx.notify();
    }

    /// 关闭左下角切换菜单；Main 里把焦点还给编辑器（不重建 `AppShell`）。
    pub fn close_vault_menu(&mut self, cx: &mut Context<Self>) {
        if !self.vault_menu_open {
            return;
        }
        self.vault_menu_open = false;
        self.vault_menu_focused_once = false;
        if self.state == State::Main && !self.vault_manager_open {
            self.pending_focus_restore = true;
        }
        cx.notify();
    }

    /// 左下角 vault 按钮：菜单 开 ↔ 关。
    pub fn toggle_vault_menu(&mut self, cx: &mut Context<Self>) {
        if self.vault_menu_open {
            self.close_vault_menu(cx);
        } else {
            self.open_vault_menu(cx);
        }
    }

    /// 切换菜单是否展开（测试/诊断；不是设置页）。
    #[must_use]
    pub fn vault_menu_is_open(&self) -> bool {
        self.vault_menu_open
    }

    /// 打开「管理仓库」覆盖层（Main 里新建/加入/返回欢迎页的唯一入口）。
    ///
    /// 打开时重读一次公开配置（与设置页一致：显示磁盘当前内容，外部手改也能看到）。
    pub fn open_vault_manager(&mut self, cx: &mut Context<Self>) {
        if self.state != State::Main {
            return;
        }
        // 菜单 → 管理仓库：不还焦点给编辑器（下一帧焦点进管理仓库）。
        self.vault_menu_open = false;
        self.vault_menu_focused_once = false;
        self.vault_manager_notice = None;
        // 重新打开 = 从磁盘当前内容开始：不保留上一次未保存的重命名输入。
        self.vault_rename = None;
        match self.store.load() {
            Ok(Some(profile)) => self.profile = Some(profile),
            // 没有配置文件时保留内存里的列表（不把刚打开实例的公开信息清掉）。
            Ok(None) => {}
            Err(error) => {
                self.vault_manager_notice = Some(format!(
                    "配置读取失败：{error}；先按内存里的列表显示，不自动重建、不改写配置"
                ));
            }
        }
        self.vault_manager_open = true;
        self.vault_manager_focused_once = false;
        cx.notify();
    }

    /// 关闭「管理仓库」；Main 里把焦点还给编辑器。
    pub fn close_vault_manager(&mut self, cx: &mut Context<Self>) {
        if !self.vault_manager_open {
            return;
        }
        self.vault_manager_open = false;
        self.vault_manager_focused_once = false;
        self.vault_manager_notice = None;
        self.vault_rename = None;
        if self.state == State::Main {
            self.pending_focus_restore = true;
        }
        cx.notify();
    }

    /// 「管理仓库」是否展开（测试/诊断）。
    #[must_use]
    pub fn vault_manager_is_open(&self) -> bool {
        self.vault_manager_open
    }

    /// 管理仓库的内联提示（「设为默认」结果/失败原因；重新打开时清空）。
    #[must_use]
    pub fn vault_manager_notice(&self) -> Option<&str> {
        self.vault_manager_notice.as_deref()
    }

    /// 管理仓库的行投影（**渲染与测试共用**：名字/路径/角色只有一份）。
    #[must_use]
    pub fn vault_manager_rows(&self) -> Vec<VaultManagerRow> {
        let active_id = self
            .profile
            .as_ref()
            .and_then(|profile| profile.active_instance.clone());
        let opened_id = self
            .instance
            .as_ref()
            .map(|entry| entry.instance_id.clone());
        self.entries()
            .into_iter()
            .map(|entry| VaultManagerRow {
                name: entry.effective_name(),
                meta: format!(
                    "API {} · {} · vault_id {}",
                    entry.api_version.as_str(),
                    entry.relay_mode.as_str(),
                    short_public_id(&entry.vault_id)
                ),
                instance_id: entry.instance_id.clone(),
                path: entry.vault_path.clone(),
                is_active: active_id.as_deref() == Some(entry.instance_id.as_str()),
                opened: opened_id.as_deref() == Some(entry.instance_id.as_str()),
            })
            .collect()
    }

    /// 是否正在管理仓库里行内重命名（`None` = 不在改名；不写盘）。
    #[must_use]
    pub fn vault_rename_is_open(&self) -> bool {
        self.vault_rename.is_some()
    }

    /// 正在改名的实例 id（测试/诊断）。
    #[must_use]
    pub fn vault_rename_instance_id(&self) -> Option<&str> {
        self.vault_rename
            .as_ref()
            .map(|rename| rename.instance_id.as_str())
    }

    /// 行内重命名输入框的当前文本（trim 后原文）。
    #[must_use]
    pub fn vault_rename_text(&self, cx: &App) -> Option<String> {
        self.vault_rename
            .as_ref()
            .map(|rename| rename.input.read(cx).text().trim().to_owned())
    }

    /// 行内重命名输入框的错误（校验失败/写盘失败；没有则 `None`）。
    #[must_use]
    pub fn vault_rename_error(&self) -> Option<&str> {
        self.vault_rename
            .as_ref()
            .and_then(|rename| rename.error.as_deref())
    }

    /// 测试/预填：改行内重命名输入框的文本（不写盘）。
    pub fn vault_rename_set_text(&mut self, text: &str, cx: &mut Context<Self>) {
        if let Some(rename) = &self.vault_rename {
            let text = text.to_owned();
            rename.input.update(cx, |input, cx| {
                input.set_text(&text);
                cx.notify();
            });
            cx.notify();
        }
    }

    /// 开始重命名一个已保存实例：打开**行内**输入（预填当前自定义名，空 = 目录名）。
    ///
    /// 只准备输入：不写 `sync.json`、不移动目录、不碰实例锁/worker。
    pub fn begin_vault_rename(&mut self, instance_id: &str, cx: &mut Context<Self>) {
        if !self.vault_manager_open {
            return;
        }
        let Some(entry) = self
            .profile
            .as_ref()
            .and_then(|profile| profile.find(instance_id))
            .cloned()
        else {
            self.vault_manager_notice = Some(format!("实例不存在：{instance_id}；配置没有被改动"));
            cx.notify();
            return;
        };
        let initial = entry.display_name.clone().unwrap_or_default();
        let input = cx.new(|cx| {
            let mut input = FormInput::new(crate::ui_text::vault_name_placeholder(), false, cx)
                .selector("vault-manager-rename-input")
                .click_to_focus();
            input.set_text(&initial);
            input
        });
        self.vault_manager_notice = None;
        self.vault_rename = Some(VaultRenameState {
            instance_id: instance_id.to_owned(),
            input,
            error: None,
            focused_once: false,
        });
        cx.notify();
    }

    /// 取消重命名（Esc/「取消」）：只丢弃输入，磁盘/内存配置都不变。
    pub fn cancel_vault_rename(&mut self, cx: &mut Context<Self>) {
        if self.vault_rename.take().is_some() {
            cx.notify();
        }
    }

    /// 保存重命名：校验 → 只改 `display_name` → `sync.json` 原子写 → 刷新三处显示。
    ///
    /// 成功条件（全部满足才关闭行内输入）：
    /// 1. 名称通过 [`normalize_display_name`]（trim/1–64 字符/无控制字符；空白 = 清除）；
    /// 2. 实例仍在公开配置里（不自动新增条目）；
    /// 3. `sync.json` 写成功（失败时磁盘原样保留，输入与错误留在行内）。
    ///
    /// **不移动目录、不改 `active_instance`/`vault_path`/`vault_id`、不碰实例锁**。
    pub fn save_vault_rename(&mut self, cx: &mut Context<Self>) {
        let Some(rename) = self.vault_rename.as_ref() else {
            return;
        };
        let instance_id = rename.instance_id.clone();
        let raw = rename.input.read(cx).text().trim().to_owned();
        let normalized = match normalize_display_name(&raw) {
            Ok(name) => name,
            Err(error) => {
                self.set_vault_rename_error(error.to_string(), cx);
                return;
            }
        };
        let mut profile = self.profile.clone().unwrap_or_default();
        if let Err(error) = profile.rename_instance(&instance_id, normalized.as_deref()) {
            self.set_vault_rename_error(error.to_string(), cx);
            return;
        }
        if let Err(error) = self.store.save(&profile) {
            self.set_vault_rename_error(
                format!(
                    "配置保存失败：{error}；名称没有被改动（磁盘原样保留），\
                     可修正后重试（不会动实例目录/库/实例锁）"
                ),
                cx,
            );
            return;
        }
        self.profile = Some(profile);
        // 当前打开的实例也要立刻换名字（左下角按钮/菜单当前行/管理仓库同步刷新）。
        if let Some(current) = &mut self.instance {
            if current.instance_id == instance_id {
                current.display_name = normalized.clone();
            }
        }
        self.vault_rename = None;
        let display = self
            .profile
            .as_ref()
            .and_then(|profile| profile.find(&instance_id))
            .map(InstanceEntry::effective_name)
            .unwrap_or_else(|| instance_id.clone());
        self.vault_manager_notice = Some(match &normalized {
            Some(_) => format!(
                "已保存仓库名称「{display}」到 sync.json：只改显示，\
                 目录/库路径/实例锁/同步 worker 都没有变化。"
            ),
            None => format!(
                "已清除自定义名称：显示回退为目录名「{display}」（只改显示，目录/路径没有变化）。"
            ),
        });
        cx.notify();
    }

    /// 行内重命名的错误（保留输入，可修正后重试）。
    fn set_vault_rename_error(&mut self, message: String, cx: &mut Context<Self>) {
        if let Some(rename) = &mut self.vault_rename {
            rename.error = Some(message);
        }
        cx.notify();
    }

    /// 当前 vault 的显示名（左下角按钮文案）：**自定义名优先**，其次库文件父目录名
    /// （不显示 `vault.sqlite3`/完整路径）；本地库用本地路径的目录名，没有则「本地库」。
    ///
    /// 与切换菜单当前行、管理仓库行共用 [`InstanceEntry::effective_name`]：三处同一个名字。
    #[must_use]
    pub fn current_vault_name(&self, _cx: &App) -> String {
        if let Some(entry) = self.instance.as_ref() {
            // 自定义名优先，其次目录名，最后短 id（与菜单/管理仓库同一份名字）。
            return entry.effective_name();
        }
        if self.local_mode {
            let path = self
                .local_recent_path
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_default();
            return vault_directory_name(Path::new(&path)).unwrap_or_else(|| "本地库".to_owned());
        }
        "本库".to_owned()
    }

    /// 左下角切换菜单的行模型（当前 vault 带 `✓`；其它按已保存顺序，active 优先）。
    ///
    /// 渲染与测试共用这一份投影：菜单文案不会出现"测试断言的和画出来的不一致"。
    #[must_use]
    pub fn vault_menu_rows(&self, cx: &App) -> Vec<VaultMenuRow> {
        let current_id = self
            .instance
            .as_ref()
            .filter(|_| !self.local_mode)
            .map(|entry| entry.instance_id.clone());
        // 当前行与左下角按钮用同一个短名字（目录名），避免两处显示不一致。
        let current_name = self.current_vault_name(cx);
        self.entries()
            .into_iter()
            .map(|entry| {
                let current = current_id.as_deref() == Some(entry.instance_id.as_str());
                // 与左下角按钮/管理仓库同一套显示名（自定义名 → 目录名 → 短 id）。
                let name = entry.effective_name();
                VaultMenuRow {
                    instance_id: entry.instance_id.clone(),
                    label: if current {
                        format!("✓ {current_name}")
                    } else {
                        name
                    },
                    meta: if current {
                        "当前 vault".to_owned()
                    } else {
                        format!(
                            "API {} · {}",
                            entry.api_version.as_str(),
                            entry.relay_mode.as_str()
                        )
                    },
                    current,
                }
            })
            .collect()
    }

    /// 切到另一个已保存实例：**关旧开新**，不重启应用、不热切 worker。
    ///
    /// 顺序（数据安全优先）：
    /// 1. 目标就是当前打开的实例 → **no-op**（只收菜单/管理器），不重建 `AppShell`；
    /// 2. 先 `AppShell::close_all`（有界排空/flush）→ drop 旧 `AppShell`；
    /// 3. 释放旧 `InstanceLock`（Drop 删除锁文件）；
    /// 4. 打开目标实例（成功写 active/`startup.json` = sync；失败进既有 Error 页，
    ///    **绝不**把两个 `AppShell`/worker 同时留着）。
    ///
    /// 这是**显式用户动作**：切换 = 关旧开新；锁逻辑只有这一份。
    pub fn switch_to_instance(&mut self, instance_id: &str, cx: &mut Context<Self>) {
        if self.state != State::Main {
            return;
        }
        let Some(entry) = self
            .profile
            .as_ref()
            .and_then(|profile| profile.find(instance_id))
            .cloned()
        else {
            self.vault_manager_notice = Some(format!(
                "实例不存在：{instance_id}；配置没有被改动，请重新打开管理仓库"
            ));
            cx.notify();
            return;
        };
        // 目标就是当前实例：no-op（不 flush/不重新加载），只收起菜单/管理器。
        if !self.local_mode
            && self
                .instance
                .as_ref()
                .is_some_and(|current| current.instance_id == instance_id)
        {
            let from_manager = self.vault_manager_open;
            self.vault_menu_open = false;
            self.vault_menu_focused_once = false;
            if from_manager {
                self.vault_manager_notice = Some(format!(
                    "「{}」已经是当前打开的 vault：不重建、不重新加载。",
                    entry.instance_id
                ));
            } else {
                self.pending_focus_restore = self.main.is_some();
                self.notice = Some(format!(
                    "「{}」已经是当前打开的 vault：不重建、不重新加载（当前库保持不变）。",
                    entry.instance_id
                ));
            }
            cx.notify();
            return;
        }
        // 关旧开新：菜单/管理器先收起，再 flush 旧 Main + 释放锁。
        self.clear_vault_overlays();
        self.close_main(cx);
        self.state = State::Opening;
        self.error = None;
        self.pending_focus_restore = false;
        self.close_settings(cx);
        self.open_entry_after_close(entry, cx);
        cx.notify();
    }

    /// 管理仓库 →「设为默认」：只改公开配置 `active_instance` + 本机启动入口，
    /// **不热切换**当前 Main（当前库/worker 不变）。
    pub fn vault_manager_set_active(&mut self, instance_id: &str, cx: &mut Context<Self>) {
        if !self.vault_manager_open {
            return;
        }
        let mut profile = self.profile.clone().unwrap_or_default();
        let Some(entry) = profile.find(instance_id).cloned() else {
            self.vault_manager_notice = Some(format!("实例不存在：{instance_id}；配置没有被改动"));
            cx.notify();
            return;
        };
        let already = profile.active_instance.as_deref() == Some(instance_id);
        if !already {
            profile.active_instance = Some(instance_id.to_owned());
            if let Err(error) = self.store.save(&profile) {
                self.vault_manager_notice = Some(format!(
                    "配置保存失败：{error}；active 没有被改动，可修正后重试（不会动实例目录）"
                ));
                cx.notify();
                return;
            }
            self.profile = Some(profile);
        }
        // 兑现「重启后生效」：下次启动走同步入口（即使当前停在本地模式）。
        self.remember_startup_mode(StartupMode::Sync);
        self.vault_manager_notice = Some(if already {
            format!(
                "「{}」已经是默认实例：重启后生效（仍会打开它；当前界面不变）。\
                 要立刻切换请点同一行的「打开」（关旧开新，不重启）。",
                entry.instance_id
            )
        } else {
            format!(
                "已将「{}」设为默认：重启后生效；当前主界面保持不变，不热切换 Vault/worker。\
                 要立刻切换请点同一行的「打开」（关旧开新，不重启）。",
                entry.instance_id
            )
        });
        cx.notify();
    }

    /// 管理仓库 →「同步远程仓库…」：关 Main + 回 Welcome → 展开**同一个**统一表单。
    ///
    /// 创建与加入不再分两个入口：表单只收一条 `mn-pair://create` / `mn-pair://join`
    /// 链接，由 [`Self::welcome_confirm_sync`] 自动识别（还有 DEV 同意勾选与实例目录）。
    pub fn vault_manager_sync_remote(&mut self, cx: &mut Context<Self>) {
        if !self.vault_manager_open {
            return;
        }
        self.leave_main_to_welcome(cx);
        self.ensure_welcome_forms(cx);
        self.reset_welcome_sync_form(cx);
        self.welcome_open_action(WelcomeAction::SyncRemote, cx);
        self.notice = Some(
            "同步远程仓库：粘贴 mn-pair://create（创建票据）或 mn-pair://join（邀请加入）             链接，自动识别；不再需要手填 relay/vault_id/恢复码。"
                .to_owned(),
        );
        cx.notify();
    }

    /// 重置统一同步表单：新实例 id、默认目录、空链接、未勾选（不写盘/不联网）。
    fn reset_welcome_sync_form(&mut self, cx: &mut Context<Self>) {
        self.ensure_welcome_forms(cx);
        let instance_id = self
            .options
            .instance_id
            .clone()
            .unwrap_or_else(generate_instance_id);
        let dir_text = self
            .options
            .layout
            .default_instance_dir(&instance_id)
            .map(|path| path.display().to_string())
            .unwrap_or_default();
        let Some(welcome) = &mut self.welcome else {
            return;
        };
        welcome.sync_instance_id = instance_id;
        welcome.sync_consent = false;
        welcome.error = None;
        welcome.sync_link.update(cx, |input, cx| {
            input.clear();
            cx.notify();
        });
        welcome.sync_dir.update(cx, |input, cx| {
            input.set_text(&dir_text);
            cx.notify();
        });
        // 名称属于上一次操作：新表单从「留空则使用目录名」开始。
        welcome.sync_name.update(cx, |input, cx| {
            input.clear();
            cx.notify();
        });
    }

    /// 管理仓库 →「返回欢迎页」：关闭当前 Main（flush）并释放实例锁，回到 Welcome。
    ///
    /// 同步实例列表仍在（`sync.json` 不改），可以重新打开；本地模式提示清掉。
    /// 不删除任何用户文件、不改 active、不重建实例。
    pub fn vault_manager_back_to_welcome(&mut self, cx: &mut Context<Self>) {
        if !self.vault_manager_open {
            return;
        }
        let had_main = self.main.is_some() || self.state == State::Main;
        self.leave_main_to_welcome(cx);
        if let Some(welcome) = &mut self.welcome {
            welcome.expanded = None;
            welcome.error = None;
        }
        self.notice = had_main.then(|| {
            "已返回欢迎页：当前笔记库已关闭（已 flush），实例列表仍在；可重新打开、新建或加入 vault。"
                .to_owned()
        });
        cx.notify();
    }

    /// 收起左下角菜单/管理仓库（不碰焦点；`close_main`/`open_settings` 内部用）。
    fn clear_vault_overlays(&mut self) {
        self.vault_menu_open = false;
        self.vault_manager_open = false;
        self.vault_menu_focused_once = false;
        self.vault_manager_focused_once = false;
        self.vault_manager_notice = None;
        self.vault_rename = None;
    }

    /// 关闭当前 Main：`AppShell::close_all`（有界排空）→ drop shell → 释放实例锁。
    ///
    /// 不触碰公开配置；本地模式标记一并清除。
    fn close_main(&mut self, cx: &mut Context<Self>) {
        // 离开 Main：左下角菜单/管理仓库一并收起（不残留到 Welcome/Error）。
        self.clear_vault_overlays();
        self.main_shell_events = None;
        if let Some(shell) = self.main.take() {
            shell.update(cx, |shell, cx| shell.close_all(cx));
        }
        self.lock = None;
        self.instance = None;
        self.local_mode = false;
    }

    /// 清掉 Welcome 同步表单里的**一次性链接**与 DEV 同意（离开入口/返回时调用）。
    fn clear_welcome_sync_secret(&mut self, cx: &mut Context<Self>) {
        let Some(welcome) = &mut self.welcome else {
            return;
        };
        welcome.sync_consent = false;
        welcome.error = None;
        welcome.sync_link.update(cx, |input, cx| {
            input.clear();
            cx.notify();
        });
    }

    /// 关 Main + 关设置 + 清掉所有临时页面状态，回到 Welcome（不删文件/不改配置）。
    fn leave_main_to_welcome(&mut self, cx: &mut Context<Self>) {
        self.close_main(cx);
        self.close_settings(cx);
        self.pending_focus_restore = false;
        self.pending = None;
        self.pending_join = None;
        self.pending_invite = None;
        self.form = None;
        self.join_form = None;
        self.invite = None;
        self.working = None;
        self.retry_entry = None;
        self.error = None;
        self.pending_create_display_name = None;
        self.pending_invite_display_name = None;
        self.vault_rename = None;
        self.state = State::Welcome;
        if let Some(welcome) = &mut self.welcome {
            welcome.expanded = None;
        }
        self.clear_welcome_sync_secret(cx);
    }

    /// 打开目标实例；失败进既有 Error 页（可重试/返回欢迎页）。
    fn open_entry_after_close(&mut self, entry: InstanceEntry, cx: &mut Context<Self>) {
        if let Err(error) = self.open_instance(&entry, cx) {
            let message = sanitize_error(&error, &[]);
            self.retry_entry = Some(entry.clone());
            let public = public_fields(&entry);
            self.set_error("无法打开实例", message, public, RetryKind::OpenInstance);
        }
    }

    /// 当前打开的实例（`None` = Welcome/本地模式/未成功打开）。
    #[must_use]
    pub fn opened_instance(&self) -> Option<&InstanceEntry> {
        self.instance.as_ref()
    }

    /// Welcome 本地入口按钮的**动态**文案（产品）：文件已存在 =「打开本地 vault」，
    /// 不存在 =「新建本地 vault」（点击创建空库，不播种 demo）。
    ///
    /// dev-sync 构建保持旧「继续本地（DEV ONLY）」文案。
    #[must_use]
    pub fn local_action_label(&self, cx: &App) -> String {
        if cfg!(feature = "dev-sync") {
            return Self::local_entry_label();
        }
        let path = self
            .welcome
            .as_ref()
            .map(|welcome| welcome.open_local_path.read(cx).text().trim().to_owned())
            .unwrap_or_default();
        if !path.is_empty() && Path::new(&path).is_file() {
            "打开本地 vault".to_owned()
        } else {
            "新建本地 vault".to_owned()
        }
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
                crate::ui_text::settings_fingerprint_instance_closed_detail(),
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

    /// 进入 Create 表单（ADR 0018 S4b：只有创建链接 + 实例目录）。
    pub fn enter_create_form(&mut self, cx: &mut Context<Self>) {
        let instance_id = self
            .options
            .instance_id
            .clone()
            .unwrap_or_else(generate_instance_id);
        // 默认放在用户可见目录（Documents/menghunote/instances/<id>），不再默认
        // LOCALAPPDATA；用户可改，实际路径提交时写进公开配置的 `vault_path`。
        let default_dir = self
            .options
            .layout
            .default_instance_dir(&instance_id)
            .map(|path| path.display().to_string())
            .unwrap_or_default();
        // 创建链接含一次性票据：遮罩 + 禁复制 + 允许粘贴（与秘密字段同一控件）。
        let link = cx.new(|cx| {
            FormInput::new("粘贴创建链接 mn-pair://create?...", true, cx)
                .selector("app-root-create-link-input")
                .click_to_focus()
        });
        // 预填真实值（不是 placeholder）：用户不点「浏览」也能直接提交默认目录。
        let dir_default = default_dir.clone();
        let dir = cx.new(|cx| {
            let mut input = FormInput::new(dir_default.clone(), false, cx)
                .selector("app-root-dir-input")
                .click_to_focus();
            input.set_text(&dir_default);
            input
        });
        // 链接输入框按 Enter = 应用链接；其它字段按 Enter = 提交表单。
        let mut subs = Vec::new();
        let link_subscription = cx.subscribe(&link, |root, _input, event: &FormInputEvent, cx| {
            if matches!(event, FormInputEvent::Submit) {
                root.form_apply_create_link(cx);
            }
        });
        subs.push(link_subscription);
        let dir_subscription = cx.subscribe(&dir, |root, _input, event: &FormInputEvent, cx| {
            if matches!(event, FormInputEvent::Submit) {
                root.submit_create(cx);
            }
        });
        subs.push(dir_subscription);
        self.form = Some(FormState {
            instance_id,
            mode_kind: FormCreateMode::Link,
            // 尚未应用链接：按本构建可用的默认模式展示，应用链接后由 scheme 覆盖。
            mode: default_form_mode(),
            link,
            link_state: None,
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

    /// 进入 **粘贴创建链接** 的 Create 表单（`mn-pair://create` + v2 票据）。
    ///
    /// 与 [`Self::enter_create_form`] 同一个表单/同一条提交路径；差别只是初始焦点在
    /// 创建链接字段、并给出一次性票据提示。
    pub fn enter_create_link_form(&mut self, cx: &mut Context<Self>) {
        self.enter_create_form(cx);
        self.notice = Some(
            "粘贴创建链接（mn-pair://create?...&vault=…#ticket=…）：票据一次性、默认 15 分钟内有效，             只从内存提交；不写配置/日志，失败也不会保存票据。"
                .to_owned(),
        );
        cx.notify();
    }

    /// 当前创建方式（测试/诊断；ADR 0018 S4b 只可能是链接）。
    #[must_use]
    pub fn form_create_mode(&self) -> Option<FormCreateMode> {
        self.form.as_ref().map(|form| form.mode_kind)
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

    /// 「浏览…」：系统目录选择器 → 选中目录写进 Create 表单的实例目录。
    ///
    /// 只填文本、不提交；方向/存在性校验仍在 [`Self::submit_create`] 的
    /// `validate_instance_directory`（绝对路径 + 不存在/空目录），选择器不构成授权。
    /// headless 环境拿不到真实对话框，测试直接调 [`Self::form_set_dir`]。
    pub fn form_browse_dir(&mut self, cx: &mut Context<Self>) {
        if self.form.is_none() {
            return;
        }
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(SharedString::from("选择同步实例目录")),
        });
        cx.spawn(async move |root, cx| {
            if let Ok(Ok(Some(paths))) = receiver.await {
                if let Some(path) = paths.first() {
                    let text = path.display().to_string();
                    let _ = root.update(cx, |root, cx| root.form_set_dir(&text, cx));
                }
            }
        })
        .detach();
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
        // 产品构建（无 dev-sync）只允许 remote_tls：链接的 http scheme 在应用阶段就
        // 明确拒绝，不把票据带进后续流程（票据未被消费，可人工核对链接）。
        if !crate::sync_gate::mode_allowed(mode) {
            fail(
                self,
                format!(
                    "本次构建不开放 relay 模式 {}：产品构建只允许 remote_tls（local_dev 需要 dev-sync 构建）",
                    mode.as_str()
                ),
                cx,
            );
            return;
        }
        let vault_id_hex = menghunote_sync::ids::hex_of(&vault_id);
        let Some(form) = self.form.as_mut() else {
            return;
        };
        // 应用链接 = 唯一创建方式；模式由链接 scheme 决定（HTTPS 只由权威 parser 校验，
        // 不需要额外的"确认连接 HTTPS"勾选）。
        form.mode = mode;
        form.link_state = Some(CreateLinkState {
            mode,
            relay: relay.clone(),
            vault_id,
            vault_id_hex: vault_id_hex.clone(),
            ticket,
            pin,
        });
        form.error = None;
        // 链接（含票据）只留在内存状态；输入框立刻清空，完整 URL 已丢弃。
        form.link.update(cx, |input, cx| {
            input.clear();
            cx.notify();
        });
        self.notice = Some(format!(
            "已载入创建链接：vault_id {vault_id_hex}（{}）；票据一次性、默认 15 分钟内有效，             提交时只使用一次，不写入配置/日志。",
            mode.as_str()
        ));
        cx.notify();
    }

    /// 清掉已应用的创建链接（不触发任何网络）。
    pub fn form_clear_create_link(&mut self, cx: &mut Context<Self>) {
        let Some(form) = &mut self.form else {
            return;
        };
        if form.link_state.take().is_none() {
            return;
        }
        form.mode = default_form_mode();
        form.error = None;
        self.notice = Some(
            "已移除创建链接：粘贴新的 mn-pair://create 链接后可重新创建（旧票据已从界面丢弃，             未提交时不会被消费）"
                .to_owned(),
        );
        cx.notify();
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

    /// **测试专用**：给 remote_tls 的 Create/Join worker 注入显式信任根（DER）。
    ///
    /// 只用于本地 TLS harness（`menghunote-dev-relay::start_tls` 的自签 CA）；生产不调用。
    /// 只**增加**显式信任，不关闭证书链/SAN/有效期校验，也不提供 accept-invalid。
    #[doc(hidden)]
    pub fn set_test_relay_roots(&mut self, roots: Vec<Vec<u8>>) {
        self.test_relay_roots = roots;
    }

    /// **测试专用**：让下次 Create 的自动发布基线走故意不可达的 loopback（模拟发布失败）。
    ///
    /// 只用于验证"Create 成功但基线未发布"的 UX 与重试路径；生产不调用。
    #[doc(hidden)]
    pub fn set_test_baseline_unreachable(&mut self, unreachable: bool) {
        self.test_baseline_unreachable = unreachable;
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
        // 与 Create 同一默认根（用户可见目录，可改；不再默认 LOCALAPPDATA）。
        let default_dir = self
            .options
            .layout
            .default_instance_dir(&instance_id)
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
        // 预填真实值（不是 placeholder）：与 Create 同一默认根，可直接提交或再浏览。
        let dir_default = default_dir.clone();
        let dir = cx.new(|cx| {
            let mut input = FormInput::new(dir_default.clone(), false, cx)
                .selector("app-root-join-dir-input")
                .click_to_focus();
            input.set_text(&dir_default);
            input
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

    /// 「浏览…」：系统目录选择器 → 选中目录写进 Join 表单的实例目录。
    ///
    /// 与 Create 同一套校验（绝对路径 + 不存在/空目录），选择器不构成授权。
    pub fn join_form_browse_dir(&mut self, cx: &mut Context<Self>) {
        if self.join_form.is_none() {
            return;
        }
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(SharedString::from("选择同步实例目录")),
        });
        cx.spawn(async move |root, cx| {
            if let Ok(Ok(Some(paths))) = receiver.await {
                if let Some(path) = paths.first() {
                    let text = path.display().to_string();
                    let _ = root.update(cx, |root, cx| root.join_form_set_dir(&text, cx));
                }
            }
        })
        .detach();
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

    /// Welcome 的**本地（不同步）**入口文案（同一 selector，按构建区分）。
    ///
    /// 渲染与测试共用这唯一来源：产品 =「仅本地使用（不同步）」；
    /// `dev-sync` =「继续本地（DEV ONLY，无同步）」。
    #[must_use]
    pub fn local_entry_label() -> String {
        if cfg!(feature = "dev-sync") {
            crate::ui_text::local_dev_action()
        } else {
            crate::ui_text::local_action()
        }
    }

    // ---- Welcome 左列表 / 右操作卡（Obsidian 式仓库选择页） ----

    /// 展开/收起右栏某张操作卡（不整页跳走；当前展开项再点一次 = 收起）。
    pub fn welcome_toggle_action(&mut self, action: WelcomeAction, cx: &mut Context<Self>) {
        self.ensure_welcome_forms(cx);
        let Some(welcome) = &mut self.welcome else {
            return;
        };
        welcome.expanded = if welcome.expanded == Some(action) {
            None
        } else {
            Some(action)
        };
        welcome.error = None;
        self.pending_focus = true;
        cx.notify();
    }

    /// 打开指定操作卡（测试/管理仓库入口使用；幂等）。
    pub fn welcome_open_action(&mut self, action: WelcomeAction, cx: &mut Context<Self>) {
        self.ensure_welcome_forms(cx);
        let Some(welcome) = &mut self.welcome else {
            return;
        };
        welcome.expanded = Some(action);
        welcome.error = None;
        self.pending_focus = true;
        cx.notify();
    }

    /// 当前展开的操作卡（测试/诊断）。
    #[must_use]
    pub fn welcome_expanded(&self) -> Option<WelcomeAction> {
        self.welcome.as_ref().and_then(|welcome| welcome.expanded)
    }

    /// Welcome 内联错误（`welcome-error`；已脱敏）。
    #[must_use]
    pub fn welcome_error(&self) -> Option<&str> {
        self.welcome
            .as_ref()
            .and_then(|welcome| welcome.error.as_deref())
    }

    fn set_welcome_error(&mut self, message: impl Into<String>, cx: &mut Context<Self>) {
        self.ensure_welcome_forms(cx);
        if let Some(welcome) = &mut self.welcome {
            welcome.error = Some(message.into());
        }
        cx.notify();
    }

    /// 「新建本地仓库」的目录文本（测试/预填）。
    #[must_use]
    pub fn welcome_new_local_dir_text(&self, cx: &App) -> Option<String> {
        self.welcome
            .as_ref()
            .map(|welcome| welcome.new_local_dir.read(cx).text().trim().to_owned())
    }

    /// 测试/预填：「新建本地仓库」目录。
    pub fn welcome_set_new_local_dir(&mut self, text: &str, cx: &mut Context<Self>) {
        self.ensure_welcome_forms(cx);
        if let Some(welcome) = &self.welcome {
            let text = text.to_owned();
            welcome.new_local_dir.update(cx, |input, cx| {
                input.set_text(&text);
                cx.notify();
            });
        }
    }

    /// 「打开本地仓库」输入框文本（测试/预填）。
    #[must_use]
    pub fn welcome_open_local_path_text(&self, cx: &App) -> Option<String> {
        self.welcome
            .as_ref()
            .map(|welcome| welcome.open_local_path.read(cx).text().trim().to_owned())
    }

    /// 测试/预填：「打开本地仓库」路径。
    pub fn welcome_set_open_local_path(&mut self, text: &str, cx: &mut Context<Self>) {
        self.local_set_path(text, cx);
    }

    /// 同步表单的链接输入框（**秘密**：只报告是否为空/测试预填，不打印正文）。
    #[must_use]
    pub fn welcome_sync_link_is_empty(&self, cx: &App) -> Option<bool> {
        self.welcome
            .as_ref()
            .map(|welcome| welcome.sync_link.read(cx).text().is_empty())
    }

    /// 测试/预填：同步远程仓库的链接（截图/日志不得输出正文）。
    pub fn welcome_set_sync_link(&mut self, text: &str, cx: &mut Context<Self>) {
        self.ensure_welcome_forms(cx);
        if let Some(welcome) = &self.welcome {
            let text = text.to_owned();
            welcome.sync_link.update(cx, |input, cx| {
                input.set_text(&text);
                cx.notify();
            });
        }
    }

    /// 同步远程仓库的实例目录文本（测试/预填）。
    #[must_use]
    pub fn welcome_sync_dir_text(&self, cx: &App) -> Option<String> {
        self.welcome
            .as_ref()
            .map(|welcome| welcome.sync_dir.read(cx).text().trim().to_owned())
    }

    /// 测试/预填：同步远程仓库的实例目录。
    pub fn welcome_set_sync_dir(&mut self, text: &str, cx: &mut Context<Self>) {
        self.ensure_welcome_forms(cx);
        if let Some(welcome) = &self.welcome {
            let text = text.to_owned();
            welcome.sync_dir.update(cx, |input, cx| {
                input.set_text(&text);
                cx.notify();
            });
        }
    }

    /// 统一同步表单里的「仓库名称」输入（trim 后的原文；空 = 留空）。
    #[must_use]
    pub fn welcome_sync_name_text(&self, cx: &App) -> Option<String> {
        self.welcome
            .as_ref()
            .map(|welcome| welcome.sync_name.read(cx).text().trim().to_owned())
    }

    /// 测试/预填：统一同步表单的「仓库名称」（只影响显示，不写盘）。
    pub fn welcome_set_sync_name(&mut self, text: &str, cx: &mut Context<Self>) {
        self.ensure_welcome_forms(cx);
        if let Some(welcome) = &self.welcome {
            let text = text.to_owned();
            welcome.sync_name.update(cx, |input, cx| {
                input.set_text(&text);
                cx.notify();
            });
        }
    }

    /// `dev-sync` 的「同意开发同步」当前是否勾选（产品构建恒为 false）。
    #[must_use]
    pub fn welcome_sync_consent(&self) -> bool {
        self.welcome
            .as_ref()
            .is_some_and(|welcome| welcome.sync_consent)
    }

    /// 勾选/取消「同意开发同步」（**只有 `dev-sync` 构建渲染/校验**）。
    pub fn welcome_set_sync_consent(&mut self, consent: bool, cx: &mut Context<Self>) {
        self.ensure_welcome_forms(cx);
        if let Some(welcome) = &mut self.welcome {
            welcome.sync_consent = consent;
        }
        cx.notify();
    }

    /// 「新建本地仓库」→ 选择目录 → 创建空 `vault.sqlite3`（**不播种 demo**）。
    ///
    /// 目录内已有库时明确提示改用「打开本地仓库」，绝不覆盖；成功后写
    /// `startup.json`（`mode=local` + 实际路径）并进入 Main。
    pub fn welcome_confirm_new_local(&mut self, cx: &mut Context<Self>) {
        if self.state != State::Welcome {
            return;
        }
        self.ensure_welcome_forms(cx);
        let text = self
            .welcome
            .as_ref()
            .map(|welcome| welcome.new_local_dir.read(cx).text().trim().to_owned())
            .unwrap_or_default();
        if text.is_empty() {
            self.set_welcome_error("请填写要创建仓库的目录（绝对路径）", cx);
            return;
        }
        let dir = PathBuf::from(&text);
        if !dir.is_absolute() {
            self.set_welcome_error(
                "本地仓库目录必须是绝对路径（例如 /home/you/... 或 C:\\...）",
                cx,
            );
            return;
        }
        let path = if dir.file_name().is_some_and(|name| name == VAULT_FILE_NAME)
            || dir
                .extension()
                .is_some_and(|extension| extension == "sqlite3")
            || dir.is_file()
        {
            dir
        } else {
            dir.join(VAULT_FILE_NAME)
        };
        if path.exists() {
            self.set_welcome_error(
                format!(
                    "该位置已有库文件 {}：请改用「打开本地仓库」（新建不会覆盖已有库）",
                    path.display()
                ),
                cx,
            );
            return;
        }
        if let Some(welcome) = &mut self.welcome {
            welcome.error = None;
        }
        self.open_local_only_vault_at(path, cx);
        cx.notify();
    }

    /// 「打开本地仓库」→ 打开已有 `vault.sqlite3`；**缺失/损坏明确报错，不新建**。
    pub fn welcome_confirm_open_local(&mut self, cx: &mut Context<Self>) {
        if self.state != State::Welcome {
            return;
        }
        self.ensure_welcome_forms(cx);
        let text = self
            .welcome
            .as_ref()
            .map(|welcome| welcome.open_local_path.read(cx).text().trim().to_owned())
            .unwrap_or_default();
        if text.is_empty() {
            self.set_welcome_error("请选择已有 vault.sqlite3（或包含它的目录）", cx);
            return;
        }
        let selected = PathBuf::from(&text);
        if !selected.is_absolute() {
            self.set_welcome_error(
                "本地仓库路径必须是绝对路径（例如 /home/you/... 或 C:\\...）",
                cx,
            );
            return;
        }
        let path = Self::resolve_local_vault_selection(selected);
        if !path.is_file() {
            self.set_welcome_error(
                format!(
                    "本地库不存在：{}（打开不会新建；需要新库请用「新建本地仓库」）",
                    path.display()
                ),
                cx,
            );
            return;
        }
        if let Some(welcome) = &mut self.welcome {
            welcome.error = None;
        }
        self.open_local_only_vault_at(path, cx);
        cx.notify();
    }

    /// 「浏览…」：新建本地仓库的目录选择器（只填文本，不创建）。
    pub fn welcome_browse_new_local(&mut self, cx: &mut Context<Self>) {
        self.ensure_welcome_forms(cx);
        if self.welcome.is_none() {
            return;
        }
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(SharedString::from("选择新建仓库的目录")),
        });
        cx.spawn(async move |root, cx| {
            if let Ok(Ok(Some(paths))) = receiver.await {
                if let Some(path) = paths.first() {
                    let text = path.display().to_string();
                    let _ = root.update(cx, |root, cx| root.welcome_set_new_local_dir(&text, cx));
                }
            }
        })
        .detach();
    }

    /// 「浏览…」：打开本地仓库的文件/目录选择器（只填文本，不打开）。
    pub fn welcome_browse_open_local(&mut self, cx: &mut Context<Self>) {
        self.local_browse_vault_path(cx);
    }

    /// 「浏览…」：同步远程仓库的实例目录选择器（只填文本，不建目录）。
    pub fn welcome_browse_sync_dir(&mut self, cx: &mut Context<Self>) {
        self.ensure_welcome_forms(cx);
        if self.welcome.is_none() {
            return;
        }
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(SharedString::from("选择同步实例目录")),
        });
        cx.spawn(async move |root, cx| {
            if let Ok(Ok(Some(paths))) = receiver.await {
                if let Some(path) = paths.first() {
                    let text = path.display().to_string();
                    let _ = root.update(cx, |root, cx| root.welcome_set_sync_dir(&text, cx));
                }
            }
        })
        .detach();
    }

    /// 统一「同步远程仓库」入口：**只收一条链接**，自动识别创建票据 / 邀请链接。
    ///
    /// 严格解析：不认识的 scheme/host、坏票据、坏目录都**不联网、不建文件**，
    /// 错误内联到 `welcome-error`。识别成功后才走既有 Create / Invite worker。
    pub fn welcome_confirm_sync(&mut self, cx: &mut Context<Self>) {
        if self.state != State::Welcome {
            return;
        }
        self.ensure_welcome_forms(cx);
        let (link, dir, name, consent, instance_id) = {
            let Some(welcome) = &self.welcome else {
                return;
            };
            (
                welcome.sync_link.read(cx).text().trim().to_owned(),
                welcome.sync_dir.read(cx).text().trim().to_owned(),
                welcome.sync_name.read(cx).text().trim().to_owned(),
                welcome.sync_consent,
                welcome.sync_instance_id.clone(),
            )
        };
        if link.is_empty() {
            self.set_welcome_error(
                "请粘贴一条链接：mn-pair://create（创建票据）或 mn-pair://join（邀请加入）",
                cx,
            );
            return;
        }
        let lower = link.to_ascii_lowercase();
        let is_create = lower.starts_with("mn-pair://create");
        let is_join = lower.starts_with("mn-pair://join");
        if !is_create && !is_join {
            self.set_welcome_error(
                "无法识别的链接：只接受 mn-pair://create 或 mn-pair://join（没有联网，也没有创建文件）",
                cx,
            );
            return;
        }
        // 产品构建没有 DEV 勾选；dev-sync 构建必须先明确同意。
        #[cfg(feature = "dev-sync")]
        if !consent {
            self.set_welcome_error(
                "请先勾选「同意开发同步」：这是 localhost 开发验证，不是生产跨设备安全同步",
                cx,
            );
            return;
        }
        #[cfg(not(feature = "dev-sync"))]
        let _ = consent;
        if dir.is_empty() {
            self.set_welcome_error("请填写实例目录（绝对路径；默认在用户文档目录）", cx);
            return;
        }
        // 「仓库名称」可选：trim 后 1–64 字符、无控制字符；空白 = 回退目录名。
        // **只影响显示**，不参与目录/路径计算。
        let display_name = match normalize_display_name(&name) {
            Ok(name) => name,
            Err(error) => {
                self.set_welcome_error(
                    format!("{error}（名称只用于显示；没有改动目录，也没有创建任何文件）"),
                    cx,
                );
                return;
            }
        };
        if is_create {
            let parsed = match CreateLink::parse(&link) {
                Ok(parsed) => parsed,
                Err(error) => {
                    self.set_welcome_error(
                        format!("创建链接无效：{error}（没有联网，也没有创建任何文件）"),
                        cx,
                    );
                    return;
                }
            };
            if !crate::sync_gate::mode_allowed(parsed.mode) {
                self.set_welcome_error(
                    format!(
                        "本次构建不开放 relay 模式 {}：产品构建只允许 remote_tls（local_dev 需要 dev-sync 构建）",
                        parsed.mode.as_str()
                    ),
                    cx,
                );
                return;
            }
            let dir_path = PathBuf::from(&dir);
            if let Err(message) = validate_instance_directory(&dir_path) {
                self.set_welcome_error(message, cx);
                return;
            }
            let CreateLink {
                mode,
                relay,
                vault_id,
                ticket,
                pin,
                url,
            } = parsed;
            drop(url);
            let state = CreateLinkState {
                mode,
                relay,
                vault_id,
                vault_id_hex: menghunote_sync::ids::hex_of(&vault_id),
                ticket,
                pin,
            };
            match self.begin_ticket_create(state, dir_path, instance_id, display_name, cx) {
                Ok(()) => {
                    if let Some(welcome) = &mut self.welcome {
                        welcome.error = None;
                        welcome.sync_link.update(cx, |input, cx| {
                            input.clear();
                            cx.notify();
                        });
                    }
                    cx.notify();
                }
                Err(message) => self.set_welcome_error(message, cx),
            }
        } else {
            let target = match menghunote_sync::invite_link::InviteLinkTarget::parse(&link) {
                Ok(target) => target,
                Err(error) => {
                    self.set_welcome_error(
                        format!("邀请链接无效：{error}（没有联网，也没有创建任何文件）"),
                        cx,
                    );
                    return;
                }
            };
            if !crate::sync_gate::mode_allowed(target.mode()) {
                self.set_welcome_error(
                    format!(
                        "本次构建不开放 relay 模式 {}：产品构建只允许 remote_tls（local_dev 需要 dev-sync 构建）",
                        target.mode().as_str()
                    ),
                    cx,
                );
                return;
            }
            let dir_path = PathBuf::from(&dir);
            let vault_path = dir_path.join(VAULT_FILE_NAME);
            if let Err(message) = validate_invite_directory(&target, &vault_path, &dir_path) {
                self.set_welcome_error(message, cx);
                return;
            }
            match self.begin_invite_redeem(target, vault_path, Some(instance_id), display_name, cx)
            {
                Ok(()) => {
                    if let Some(welcome) = &mut self.welcome {
                        welcome.error = None;
                        welcome.sync_link.update(cx, |input, cx| {
                            input.clear();
                            cx.notify();
                        });
                    }
                    cx.notify();
                }
                Err(message) => self.set_welcome_error(message, cx),
            }
        }
    }

    /// 「本地（不同步）」：打开当前选择的已有本地库（旧公开入口；也供测试调用）。
    ///
    /// Welcome 的新入口是 [`Self::welcome_confirm_open_local`]（只打开已有库）；
    /// 这里保持旧 API 存在，但同样不新建缺失的库。
    pub fn open_local_vault(&mut self, cx: &mut Context<Self>) {
        self.welcome_confirm_open_local(cx);
    }

    /// 产品构建的「仅本地使用」：用户选择的库文件，空库不播种。
    ///
    /// 数据安全：
    /// - 不写 `sync.json`（不登记实例）、不建 instance 目录、不写身份文件；
    /// - 已有库直接复用（`Vault::open`，不重置内容）；不存在则创建空库；
    /// - 不取实例目录锁（不影响同步实例的锁；本地库与 instances 目录分离）。
    /// - 成功后把**实际路径**写进 `startup.json`，下次启动恢复同一路径。
    fn open_local_only_vault_at(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        match dev_vault::open_local_vault_at(&path) {
            Ok(dev) => {
                let label = dev.path.display().to_string();
                let vault = dev.vault.clone();
                let shell = cx.new(|cx| AppShell::new(vault, label, cx));
                self.set_main_shell(shell, cx);
                self.instance = None;
                self.lock = None;
                self.local_mode = true;
                self.state = State::Main;
                self.error = None;
                self.notice = Some(crate::ui_text::local_mode_notice());
                self.set_local_path_text(&path, cx);
                // 本机启动偏好：下次启动直接回到这个本地库（不写 sync.json/身份/正文）。
                self.remember_startup_preferences(StartupMode::Local, Some(path));
            }
            Err(error) => {
                let message = sanitize_error(&error, &[]);
                let dir = path.parent().map_or_else(
                    || path.display().to_string(),
                    |dir| dir.display().to_string(),
                );
                self.local_mode = false;
                self.set_error(
                    crate::ui_text::local_open_failed_title(),
                    message,
                    vec![(crate::ui_text::local_dir_label(), dir)],
                    RetryKind::None,
                );
            }
        }
    }

    /// `dev-sync` 构建的旧「继续本地」：默认 dev vault + 演示树（行为不变）。
    fn open_local_dev_vault(&mut self, cx: &mut Context<Self>) {
        match dev_vault::open_dev_vault() {
            Ok(dev) => {
                let label = dev.path.display().to_string();
                let vault = dev.vault.clone();
                let shell = cx.new(|cx| AppShell::new(vault, label, cx));
                self.set_main_shell(shell, cx);
                self.instance = None;
                self.lock = None;
                self.local_mode = true;
                self.state = State::Main;
                self.error = None;
                self.notice = Some(
                    "本地模式：默认 dev vault（DEV ONLY，无同步面板）；同步实例请用左侧创建。"
                        .to_owned(),
                );
                // dev 构建的「继续本地」同样记本机启动偏好（下次启动回到同一 dev vault），
                // 并让 Welcome 的「打开本地仓库」显示实际路径。
                self.set_local_path_text(&dev.path, cx);
                self.remember_startup_mode(StartupMode::Local);
            }
            Err(error) => {
                let message = sanitize_error(&error, &[]);
                self.local_mode = false;
                self.set_error(
                    crate::ui_text::local_dev_open_failed_title(),
                    message,
                    vec![(
                        crate::ui_text::local_dir_label(),
                        "target/tmp/menghunote-dev".to_owned(),
                    )],
                    RetryKind::None,
                );
            }
        }
    }

    /// 提交 Create 表单（按钮与 Enter 的同一入口）。
    ///
    /// ADR 0018 S4b：App 只有「粘贴创建链接」一种创建方式；未应用链接时给出内联错误，
    /// 不联网、不建文件。真正的校验/提交见 [`Self::submit_ticket_create`]。
    pub fn submit_create(&mut self, cx: &mut Context<Self>) {
        if self.state != State::CreateForm {
            return;
        }
        if self.form_has_create_link() {
            self.submit_ticket_create(cx);
        } else if let Some(form) = &mut self.form {
            form.error = Some(
                "请先粘贴创建链接并点「应用创建链接」：App 不再使用管理凭证或 setup secret，\
                 创建授权只来自链接 fragment 里的一次性票据"
                    .to_owned(),
            );
            cx.notify();
        }
    }

    /// 票据模式提交（ADR 0014 S3b / ADR 0018 S4b：App 唯一创建路径）。
    ///
    /// 校验顺序（全部在 UI 线程；任何失败都不联网、不建文件）：链接已应用 →
    /// 实例目录 → DEV 显式同意（**只有 `dev-sync` 构建**）→ 目录锁。
    /// HTTPS 只由链接 scheme + 权威 parser 决定，不再有"确认连接 HTTPS"勾选。
    /// 通过后票据**只**移动到 worker（不写配置/日志）；失败后需重新粘贴同一链接
    /// （票据未被 relay 消费时可重试）。
    fn submit_ticket_create(&mut self, cx: &mut Context<Self>) {
        let Some(form) = self.form.as_ref() else {
            return;
        };
        let Some(mode) = form.link_state.as_ref().map(|link| link.mode) else {
            return;
        };
        let dir = form.dir.read(cx).text().trim().to_owned();
        let instance_id = form.instance_id.clone();
        if let Some(form) = &mut self.form {
            form.error = None;
        }
        let fail = |root: &mut Self, message: String, cx: &mut Context<Self>| {
            if let Some(form) = &mut root.form {
                form.error = Some(message);
            }
            cx.notify();
        };
        // 0) 产品构建（无 dev-sync）在提交前再拒绝一次 local_dev（防御纵深；应用链接时
        //    已经拒绝过，但测试/未来入口可能直接构造成状态）。
        if !crate::sync_gate::mode_allowed(mode) {
            fail(
                self,
                format!(
                    "本次构建不开放 relay 模式 {}：产品构建只允许 remote_tls（local_dev 需要 dev-sync 构建）",
                    mode.as_str()
                ),
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
        // 2) 「同意开发同步」只在 dev-sync 构建要求/渲染（产品构建没有这个 DEV 勾选，
        //    提交直接继续；产品安全说明仍由 TLS/pin 与恢复码承担）。
        #[cfg(feature = "dev-sync")]
        {
            let consent = self.form.as_ref().is_some_and(|form| form.consent);
            if !consent {
                fail(
                    self,
                    "请先勾选「同意开发同步」：这是 localhost 开发验证，不是生产跨设备安全同步"
                        .to_owned(),
                    cx,
                );
                return;
            }
        }
        let Some(link) = self.form.as_mut().and_then(|form| form.link_state.take()) else {
            return;
        };
        match self.begin_ticket_create(link, dir_path, instance_id, None, cx) {
            Ok(()) => cx.notify(),
            Err(message) => fail(self, message, cx),
        }
    }

    /// 启动 Create worker（统一远程表单与旧 Create 表单共用）。
    ///
    /// 目录锁、票据移动、Working 状态都在这里；失败时票据随 `CreateLinkState` drop
    /// （未被 relay 消费，可重新粘贴同一链接），不写配置/日志。
    fn begin_ticket_create(
        &mut self,
        link: CreateLinkState,
        dir_path: PathBuf,
        instance_id: String,
        display_name: Option<String>,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let vault_path = dir_path.join(VAULT_FILE_NAME);
        let lock = InstanceLock::acquire(&dir_path).map_err(|error| error.to_string())?;
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
            vault_id,
            ticket,
            expected_instance: pin,
            lock,
            test_extra_roots: self.test_relay_roots.clone(),
            test_baseline_unreachable: self.test_baseline_unreachable,
        };
        let job = CreateJob::start(request).map_err(|error| {
            format!(
                "无法启动后台创建线程：{error}；票据已从界面移除（不持久化），\
                 请重新粘贴创建链接"
            )
        })?;
        self.pending_create_instance = Some(instance_id);
        // 名字只来自 Welcome 统一表单；Create 成功时写进公开配置（不改路径）。
        self.pending_create_display_name = display_name;
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
        Ok(())
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
        // 0b) 模式按 relay 地址推断（https → remote_tls，http → local_dev），再交给
        //     同一套权威 parser：remote_tls 只接受 HTTPS，不需要额外的"确认连接
        //     HTTPS"勾选；产品构建的 local_dev 会在这里被能力闸明确拒绝。
        let mode = infer_relay_mode(&relay);
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
        if !crate::sync_gate::mode_allowed(target.mode()) {
            fail(
                self,
                format!(
                    "本次构建不开放 relay 模式 {}：产品构建只允许 remote_tls（local_dev 需要 dev-sync 构建）",
                    target.mode().as_str()
                ),
                cx,
            );
            return;
        }
        let dir_path = PathBuf::from(dir);
        let vault_path = dir_path.join(VAULT_FILE_NAME);
        if let Err(message) = validate_invite_directory(&target, &vault_path, &dir_path) {
            fail(self, message, cx);
            return;
        }
        match self.begin_invite_redeem(target, vault_path, None, None, cx) {
            Ok(()) => {
                // 链接（含 token）已交给 worker：立刻清空输入框，不在 UI 里继续持有。
                if let Some(form) = &mut self.join_form {
                    form.invite.update(cx, |input, cx| {
                        input.clear();
                        cx.notify();
                    });
                }
                cx.notify();
            }
            Err(message) => fail(self, message, cx),
        }
    }

    /// 启动邀请兑换 worker（统一远程表单与旧 Join 表单共用）。
    ///
    /// 目录校验由调用方完成（旧 Join 表单与新欢迎页的入口一致）；这里只取锁、
    /// 把 token 移交给 worker，并进入 Working。失败不写库/不写配置。
    fn begin_invite_redeem(
        &mut self,
        target: menghunote_sync::invite_link::InviteLinkTarget,
        vault_path: PathBuf,
        instance_id: Option<String>,
        display_name: Option<String>,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let instance_dir = vault_path
            .parent()
            .map_or_else(|| vault_path.clone(), std::path::Path::to_path_buf);
        let lock = InstanceLock::acquire(&instance_dir).map_err(|error| error.to_string())?;
        let request = InviteRedeemRequest {
            target,
            vault_path,
            lock,
            test_extra_roots: self.test_relay_roots.clone(),
        };
        let job = InviteJob::start_redeem(request)
            .map_err(|error| format!("无法启动后台邀请线程：{error}"))?;
        self.pending_invite_instance = instance_id;
        // 名字在 PENDING 等待页保留，bootstrap 完成写配置时才落盘（只影响显示）。
        self.pending_invite_display_name = display_name;
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
        Ok(())
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
                // 手动检查同样重置自动检查的节拍：下一次自动 CHECK 在 ~5s 后，
                // 不会紧跟着手动点击再发一次（busy 期间也不会重叠）。
                self.invite_auto_check_due =
                    Some(cx.background_executor().now() + INVITE_AUTO_CHECK_INTERVAL);
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
        self.invite_auto_check_due = None;
        self.state = State::Welcome;
        self.ensure_welcome_forms(cx);
        self.welcome_open_action(WelcomeAction::SyncRemote, cx);
        self.notice = Some(
            "已离开等待页：PENDING 已 durable 保存；再次粘贴同一邀请链接可继续（不会重复兑换）"
                .to_owned(),
        );
        cx.notify();
    }

    /// 后台 Create/Join/邀请的事件泵（40ms；非阻塞 `try_next`）。
    ///
    /// **等待页保持存活**：进入 `State::InvitePending` 后即使没有在途 job 也继续泵，
    /// 这样 [`Self::poll_invite_auto_check`] 才能每 ~5s 自动发一次 CHECK；离开等待页
    /// （Main/JoinForm/Welcome）后的下一拍发现没有活可干就清 `pump_started` 并退出，
    /// 后续操作仍能重新拉起泵。
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
                root.poll_invite_auto_check(cx);
                let keep = root.job.is_some()
                    || root.join_job.is_some()
                    || root.invite_job.is_some()
                    || root.invite_copy_job.is_some()
                    || root.state == State::InvitePending;
                if !keep {
                    // 泵决定退出时由它自己清标志：否则"离开等待页→立刻重新粘贴邀请"
                    // 会看到 `pump_started == true` 而不启动新泵，页面就再也不更新了。
                    root.pump_started = false;
                }
                keep
            });
            match alive {
                Ok(true) => {}
                // 视图释放（窗口关闭）或本次操作已收尾：结束泵。
                _ => break,
            }
        })
        .detach();
    }

    /// PENDING 等待页的自动批准检查（每 [`INVITE_AUTO_CHECK_INTERVAL`] 一次）。
    ///
    /// - 只在 `State::InvitePending` 时工作；离开页面清掉下一次到点时刻；
    /// - 到点而 `busy`/已有检查在途 → **不重复发**，只把节拍推到下一个间隔；
    /// - 检查失败由 [`Self::apply_invite_event`] 保留等待页，下一拍自动重试；
    /// - 时间源使用 `cx.background_executor().now()`：生产是真实单调时钟，
    ///   headless 是测试调度器的虚拟时钟（`advance_clock` 可推进，不 sleep）。
    fn poll_invite_auto_check(&mut self, cx: &mut Context<Self>) {
        if self.state != State::InvitePending {
            self.invite_auto_check_due = None;
            return;
        }
        let now = cx.background_executor().now();
        let Some(due) = self.invite_auto_check_due else {
            // 防御：等待页状态存在但没登记节拍（例如未来新入口）→ 从现在起算一个间隔。
            self.invite_auto_check_due = Some(now + INVITE_AUTO_CHECK_INTERVAL);
            return;
        };
        if now < due {
            return;
        }
        // 先推进节拍再尝试派发：busy 时本轮跳过，下一间隔再试（不会每 40ms 空转）。
        self.invite_auto_check_due = Some(now + INVITE_AUTO_CHECK_INTERVAL);
        let busy =
            self.invite.as_ref().is_some_and(|invite| invite.busy) || self.invite_job.is_some();
        if busy {
            return;
        }
        self.invite_auto_checks = self.invite_auto_checks.saturating_add(1);
        self.check_invite(cx);
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
        let mut copy_invite_events = Vec::new();
        if let Some(job) = &self.invite_copy_job {
            while let Some(event) = job.try_next() {
                copy_invite_events.push(event);
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
        for event in copy_invite_events {
            self.apply_copy_invite_event(event, cx);
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
                    .pending_create_instance
                    .take()
                    .or_else(|| self.form.as_ref().map(|form| form.instance_id.clone()))
                    .unwrap_or_default();
                let mut entry = outcome.public_facts(&instance_id);
                // 统一表单填的「仓库名称」在这里落进公开事实（只影响显示）。
                entry.display_name = self.pending_create_display_name.take();
                self.pending = Some(PendingCreate {
                    outcome,
                    entry,
                    confirmed: false,
                    copied: false,
                    invite: None,
                    invite_error: None,
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
                self.pending_create_instance = None;
                self.pending_create_display_name = None;
                if cancelled {
                    self.state = State::Welcome;
                    self.ensure_welcome_forms(cx);
                    self.welcome_open_action(WelcomeAction::SyncRemote, cx);
                    self.notice = Some(format!("已取消；Create 未完成：{message}"));
                } else if self.form.is_some() {
                    // 旧 Create 表单（内部/测试路径）保留既有「回到表单重试」语义。
                    let mut public = self.form_public_fields(cx);
                    let (title, message) = security_error_text("创建失败", security, message);
                    if let Some(category) = security {
                        public.push(("安全类别".to_owned(), category.label().to_owned()));
                    }
                    self.set_error(&title, message, public, RetryKind::CreateForm);
                } else {
                    // 统一远程表单：错误内联回 Welcome（不构造不存在的 CreateForm）。
                    let (_, text) = security_error_text("创建失败", security, message);
                    self.state = State::Welcome;
                    self.ensure_welcome_forms(cx);
                    self.welcome_open_action(WelcomeAction::SyncRemote, cx);
                    self.set_welcome_error(text, cx);
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
                // 不在这里停泵：等待页要靠同一个泵每 ~5s 自动检查批准状态
                // （泵在 `State::InvitePending` 时保持存活，离开页面后自己退出）。
                self.working = None;
                let instance_id = self
                    .pending_invite_instance
                    .take()
                    .or_else(|| {
                        self.invite
                            .as_ref()
                            .map(|invite| invite.instance_id.clone())
                    })
                    .or_else(|| self.options.instance_id.clone())
                    .unwrap_or_else(generate_instance_id);
                let resumed = facts.resumed;
                let display_name = self.pending_invite_display_name.take();
                self.invite = Some(InvitePendingState {
                    instance_id,
                    display_name,
                    facts: *facts,
                    lock: Some(lock),
                    busy: false,
                    message: None,
                });
                self.state = State::InvitePending;
                // 第一拍自动检查在进入等待页 ~5s 后（不是立刻），之后每 5s 一次。
                self.invite_auto_check_due =
                    Some(cx.background_executor().now() + INVITE_AUTO_CHECK_INTERVAL);
                self.invite_auto_checks = 0;
                self.error = None;
                self.notice = Some(if resumed {
                    "已从 durable pending 恢复（没有重复兑换）：仍然等待创建者批准".to_owned()
                } else {
                    "已兑换为待批准设备：请让创建者在设备管理里核对指纹并批准".to_owned()
                });
            }
            InviteEvent::Waiting { lock } => {
                self.invite_job = None;
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
                let instance_id = self
                    .invite
                    .as_ref()
                    .map(|invite| invite.instance_id.clone())
                    .or_else(|| self.pending_invite_instance.take())
                    .or_else(|| self.options.instance_id.clone())
                    .unwrap_or_else(generate_instance_id);
                self.pending_invite_instance = None;
                self.commit_invite(outcome, instance_id, cx);
            }
            InviteEvent::Failed {
                kind,
                message,
                security,
            } => {
                self.invite_job = None;
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
                } else if self.join_form.is_some() {
                    // 旧 Join 表单：公开字段保留；秘密需重输。
                    self.working = None;
                    self.invite = None;
                    self.pending_invite_instance = None;
                    self.pending_invite_display_name = None;
                    self.state = State::JoinForm;
                    if let Some(form) = &mut self.join_form {
                        form.error = Some(text);
                    }
                    self.pending_focus = true;
                } else {
                    // 统一远程表单：回 Welcome，在对应卡片内联显示错误。
                    self.working = None;
                    self.invite = None;
                    self.pending_invite_instance = None;
                    self.pending_invite_display_name = None;
                    self.state = State::Welcome;
                    self.ensure_welcome_forms(cx);
                    self.welcome_open_action(WelcomeAction::SyncRemote, cx);
                    self.notice = Some(format!("邀请加入失败：{}", kind.hint()));
                    self.set_welcome_error(text, cx);
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
        let mut entry = outcome.public_facts(&instance_id);
        // 统一表单填的「仓库名称」：bootstrap 成功后才写进公开配置（只影响显示）。
        entry.display_name = self
            .invite
            .as_ref()
            .and_then(|invite| invite.display_name.clone());
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
        self.set_main_shell(shell, cx);
        self.lock = Some(lock);
        self.instance = Some(entry);
        self.local_mode = false;
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

    /// 等待页**自动**发出的批准检查次数（不含手动点「检查状态」；测试/诊断用）。
    #[must_use]
    pub const fn invite_auto_check_count(&self) -> u32 {
        self.invite_auto_checks
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
        self.set_main_shell(shell, cx);
        self.lock = Some(lock);
        self.instance = Some(entry);
        self.local_mode = false;
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
                "创建方式".to_owned(),
                "link（粘贴创建链接；v2 一次性票据）".to_owned(),
            ),
            (
                "relay".to_owned(),
                form.link_state.as_ref().map_or_else(
                    || "（尚未应用创建链接）".to_owned(),
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

    /// 备份页「复制邀请链接给其他设备」（ADR 0017 S5b）。
    ///
    /// 生成 v2 邀请链接（`api=2`、默认 15 分钟）→ 复制到剪贴板 → 提示风险。
    /// **不要求活动文档**（邀请是管理操作，只需要 vault + 身份 + 管理私钥）；
    /// 网络/签名在后台线程，token 不写日志/配置、也不在备份页长期展示。
    pub fn copy_invite(&mut self, cx: &mut Context<Self>) {
        if self.state != State::BackupRequired {
            return;
        }
        if self.invite_copy_job.is_some() {
            self.notice = Some("正在生成邀请链接：请等这次收尾（不并发）。".to_owned());
            cx.notify();
            return;
        }
        let Some(pending) = &self.pending else {
            return;
        };
        let request = CopyInviteRequest {
            vault: pending.outcome.vault.clone(),
            vault_path: pending.outcome.vault_path.clone(),
            relay_mode: pending.outcome.relay_mode,
            relay_url: pending.outcome.relay_url.clone(),
            api_version: pending.outcome.api_version,
            test_extra_roots: self.test_relay_roots.clone(),
        };
        let job = match CopyInviteJob::start(request) {
            Ok(job) => job,
            Err(error) => {
                if let Some(pending) = &mut self.pending {
                    pending.invite_error = Some(format!("无法启动后台邀请任务：{error}"));
                }
                cx.notify();
                return;
            }
        };
        if let Some(pending) = &mut self.pending {
            pending.invite_error = None;
        }
        self.invite_copy_job = Some(job);
        self.notice = Some(CopyInviteStage::Requesting.label().to_owned());
        self.spawn_job_pump(cx);
        cx.notify();
    }

    /// 备份页是否正在生成邀请链接（按钮禁用/测试用）。
    #[must_use]
    pub fn copy_invite_busy(&self) -> bool {
        self.invite_copy_job.is_some()
    }

    /// 备份页最近一次成功生成的邀请 id（32 位 hex；**公开**，不含 token）。
    #[must_use]
    pub fn backup_invite_id(&self) -> Option<String> {
        self.pending
            .as_ref()
            .and_then(|pending| pending.invite)
            .map(|invite| menghunote_sync::ids::hex_of(&invite.invite_id))
    }

    /// 备份页实例的初始基线是否已自动发布（`None` = 不在备份页）。
    ///
    /// `false` = Create 的自动发布失败：恢复码/实例仍可用，但邀请会被本地前置检查挡住，
    /// 需进主界面后显式「重试发布基线」。测试用它断言失败语义没有被吞掉。
    #[must_use]
    pub fn backup_baseline_published(&self) -> Option<bool> {
        self.pending
            .as_ref()
            .map(|pending| pending.outcome.baseline_published)
    }

    /// 备份页最近一次生成邀请的脱敏失败文案（成功/未生成时为 `None`）。
    #[must_use]
    pub fn backup_invite_error(&self) -> Option<&str> {
        self.pending
            .as_ref()
            .and_then(|pending| pending.invite_error.as_deref())
    }

    /// 备份页邀请链接的剩余有效期文案（不含 token）。
    #[must_use]
    pub fn backup_invite_ttl_text(&self) -> Option<String> {
        let invite = self.pending.as_ref().and_then(|pending| pending.invite)?;
        let now = menghunote_sync::http_relay::SystemClock.now_secs();
        let left = invite.expires_at.saturating_sub(now);
        Some(if left == 0 {
            "邀请已过期：请重新点「复制邀请链接」生成新的".to_owned()
        } else {
            format!(
                "邀请 {} 分 {} 秒内有效（一次性；链接含 token，只私发给要加入的设备）",
                left / 60,
                left % 60
            )
        })
    }

    /// 测试专用：**阻塞**等待备份页邀请链接的终态（显式收敛点，不是 sleep）。
    pub fn wait_for_invite_copy_terminal(
        &mut self,
        cx: &mut Context<Self>,
        timeout: Duration,
    ) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            let mut events = Vec::new();
            if let Some(job) = &self.invite_copy_job {
                while let Some(event) = job.try_next() {
                    events.push(event);
                }
            }
            if !events.is_empty() {
                for event in events {
                    self.apply_copy_invite_event(event, cx);
                }
                if self.invite_copy_job.is_none() {
                    return true;
                }
                continue;
            }
            let Some(job) = &self.invite_copy_job else {
                return true;
            };
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            match job.recv(remaining) {
                Some(event) => self.apply_copy_invite_event(event, cx),
                None => return self.invite_copy_job.is_none(),
            }
        }
    }

    /// 备份页邀请链接事件（成功即复制剪贴板；失败只显示脱敏原因）。
    fn apply_copy_invite_event(&mut self, event: CopyInviteEvent, cx: &mut Context<Self>) {
        match event {
            CopyInviteEvent::Progress(stage) => {
                self.notice = Some(stage.label().to_owned());
            }
            CopyInviteEvent::Finished(link) => {
                self.invite_copy_job = None;
                self.pump_started = false;
                let mut link = *link;
                // 复制完立刻擦除本地 URL 副本（token 只在剪贴板里留一份）。
                cx.write_to_clipboard(ClipboardItem::new_string(link.url.clone()));
                link.url.zeroize();
                if let Some(pending) = &mut self.pending {
                    pending.invite = Some(CopiedInvite {
                        invite_id: link.invite_id,
                        expires_at: link.expires_at,
                    });
                    pending.invite_error = None;
                }
                self.notice = Some(
                    "已复制邀请链接（公开链接含一次性 token，默认 15 分钟内有效）：\
                     只私发给要加入的那台设备；不要截图/公开发布，剪贴板历史可能保留副本。\
                     其他设备粘贴链接后是 PENDING，需在本机设备面板批准。"
                        .to_owned(),
                );
            }
            CopyInviteEvent::Failed { message, kind } => {
                self.invite_copy_job = None;
                self.pump_started = false;
                if let Some(pending) = &mut self.pending {
                    pending.invite = None;
                    pending.invite_error = Some(format!("{}：{message}", kind.hint()));
                }
                self.notice = None;
            }
        }
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
        self.set_main_shell(shell, cx);
        self.lock = Some(lock);
        self.instance = Some(entry);
        self.local_mode = false;
        self.state = State::Main;
        self.error = None;
        self.notice = Some(match warning {
            Some(warning) => format!(
                "实例已创建（初始基线在 Create 内自动发布；未发布时同步面板会显示\
                 「重试发布基线」）。其他设备需要加入时，在右上同步面板「邀请设备…」→\
                 「复制邀请链接」（v2 链接含一次性 token，默认 15 分钟）。注意：{warning}"
            ),
            None => "实例已创建，初始基线已自动发布：其他设备可直接在右上同步面板\
                 「邀请设备…」→「复制邀请链接」（v2 链接含一次性 token，默认 15 分钟）。"
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
                self.main_shell_events = None;
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
    ///
    /// 若当前有 Main，先走 [`Self::close_main`]（`close_all` 有界排空 + 释放实例锁），
    /// 不留后台 `AppShell`/worker。
    pub fn back_to_welcome(&mut self, cx: &mut Context<Self>) {
        self.close_main(cx);
        self.pending = None;
        self.pending_join = None;
        self.pending_invite = None;
        self.form = None;
        self.join_form = None;
        self.invite = None;
        self.working = None;
        self.retry_entry = None;
        self.error = None;
        self.notice = None;
        self.state = State::Welcome;
        if let Some(welcome) = &mut self.welcome {
            welcome.expanded = None;
        }
        self.clear_welcome_sync_secret(cx);
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
                self.close_main(cx);
                self.pending = None;
                self.pending_join = None;
                true
            }
            _ => {
                self.main_shell_events = None;
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

/// 邀请加入的目录校验：普通情况必须"不存在或空目录"；**唯一例外**是本邀请
/// 自己的 PENDING 续跑（重启/丢回应后重贴同一链接，不重复兑换、不覆盖别的库）。
fn validate_invite_directory(
    target: &menghunote_sync::invite_link::InviteLinkTarget,
    vault_path: &Path,
    dir_path: &Path,
) -> Result<(), String> {
    if let Err(message) = validate_instance_directory(dir_path) {
        let resumable = vault_path.exists()
            && menghunote_storage::Vault::open(vault_path)
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
            return Err(message);
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
        // ADR 0021 P2：系统外观（浅色/深色）变化 → 全局主题 + 重绘；只订阅一次。
        self.observe_appearance_once(window, cx);
        // Welcome/Create/Join 没有输入框时没有焦点节点，按键事件不会分发到根上
        // （`Ctrl+,` 就打不开设置）。只在非 Main 且当前没有焦点时补一个根焦点；
        // Main 由编辑器/输入框自己聚焦，不能在这里抢。
        if !matches!(self.state, State::Main) && window.focused(cx).is_none() {
            let focus = self.root_focus.clone();
            window.defer(cx, move |window, cx| {
                window.focus(&focus, cx);
            });
        }
        if self.pending_focus {
            self.pending_focus = false;
            // Join 失败后最该重输的是恢复码；Create 聚焦创建链接（唯一入口）；
            // Welcome 展开哪张卡就聚焦那张卡的主输入框。
            let target = match self.state {
                State::JoinForm => self
                    .join_form
                    .as_ref()
                    .map(|form| form.recovery_code.clone()),
                State::CreateForm => self.form.as_ref().map(|form| form.link.clone()),
                State::Welcome => {
                    self.welcome
                        .as_ref()
                        .and_then(|welcome| match welcome.expanded {
                            Some(WelcomeAction::NewLocal) => Some(welcome.new_local_dir.clone()),
                            Some(WelcomeAction::OpenLocal) => Some(welcome.open_local_path.clone()),
                            Some(WelcomeAction::SyncRemote) => Some(welcome.sync_link.clone()),
                            None => None,
                        })
                }
                _ => None,
            };
            if let Some(target) = target {
                window.defer(cx, move |window, cx| {
                    target.update(cx, |input, cx| input.focus(window, cx));
                });
            }
        }
        // 设置面板：打开后把焦点放到 relay 输入框（不抢编辑器的键盘输入）。
        // 分类导航之后 relay 输入框可能不在当前分类里：等到「Relay」分类被打开时再聚焦
        // （焦点不会落到一个没渲染的输入框上，Esc/键盘输入行为可预期）。
        if let Some(settings) = &mut self.settings {
            if settings.focus_pending && self.settings_section == SettingsSection::Relay {
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
            let root_focus = self.root_focus.clone();
            if let Some(shell) = self.main.clone() {
                // 空库没有活动编辑器（刚新建的本地库）：焦点退回根视图，
                // 否则欢迎页输入框的旧焦点会让 Ctrl+, / Ctrl+N 收不到。
                let has_editor = shell.read(cx).active_editor_view().is_some();
                window.defer(cx, move |window, cx| {
                    if has_editor {
                        shell.update(cx, |shell, cx| shell.focus_active_editor(window, cx));
                    } else {
                        window.focus(&root_focus, cx);
                    }
                });
            }
        }
        // 「选择模板笔记…」在事件回调里置位（没有 `&mut Window`）：这一帧创建并聚焦。
        if std::mem::take(&mut self.pending_daily_template_picker) {
            self.open_daily_template_picker(window, cx);
        }
        // 选择器关闭后把焦点还给日记模板输入框（下一帧，等覆盖层卸下）。
        if std::mem::take(&mut self.pending_focus_daily_template) {
            if let Some(panel) = self.daily_notes_settings.clone() {
                if let Some(input) = panel
                    .read(cx)
                    .field_input(crate::daily_notes::DailyNotesField::Template)
                {
                    window.defer(cx, move |window, cx| {
                        input.update(cx, |input, cx| input.focus(window, cx));
                    });
                }
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
                .when_some(banner_text(), |this, banner| this.child(banner))
                .child(hint_text("校验公开配置 → 加载受保护身份 → 打开真实库。"))
                .into_any_element(),
            State::Main => {
                let shell = self.main.clone();
                // 左下角菜单/管理仓库打开后的**首帧**抢焦点（`Esc` 才能收到）。
                self.focus_vault_overlay_once(window, cx);
                // 左下角**一行**动作：vault 切换器（左，占满剩余宽度）+ 紧凑「设置」。
                // 两按钮同一底边/同一行高，设置按钮不撑满剩余宽度（`flex_shrink_0` + 定宽）。
                let vault_name = self.current_vault_name(cx);
                let bottom_actions_w = bottom_actions_width(window.bounds().size.width.as_f32());
                let vault_switcher = div()
                    .id("app-root-vault-switcher")
                    .debug_selector(|| "app-root-vault-switcher".to_string())
                    // 占满设置按钮之外的剩余宽度：长名字只在自身内部省略。
                    .flex_1()
                    .min_w(px(0.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .px(px(10.0))
                    .py(px(5.0))
                    .rounded_sm()
                    .border_1()
                    .border_color(theme::input_border())
                    .bg(theme::menu_bg())
                    .text_size(px(12.0))
                    .text_color(theme::text_color())
                    .cursor_pointer()
                    .whitespace_nowrap()
                    // 长名字省略：不让目录名把 ▾ 挤出侧栏。
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(SharedString::from(vault_name)),
                    )
                    .child(div().flex_shrink_0().child("▾"))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|root, _event: &gpui::MouseDownEvent, _window, cx| {
                            root.toggle_vault_menu(cx);
                            cx.stop_propagation();
                        }),
                    );
                // 唯一的「设置」入口：仍是左侧栏底部，但和切换器**同一行**，且缩短成
                // 紧凑按钮（56–64px），不再单独占一行。覆盖在 `AppShell` 侧栏底部；
                // 点在按钮上不会漏给编辑器。
                let settings_entry = compact_action_button(
                    "app-root-settings",
                    crate::ui_text::sidebar_settings(),
                    SETTINGS_ENTRY_WIDTH,
                    cx,
                    |root, cx| root.open_settings(cx),
                );
                let bottom_actions = div()
                    .debug_selector(|| "app-root-bottom-actions".to_string())
                    .absolute()
                    // 跳过最左侧图标栏（ribbon）：这一行仍然在侧栏底部，
                    // 但整体不压在 ribbon 按钮上（`RIBBON_WIDTH` 是唯一口径）。
                    .left(px(8.0 + RIBBON_WIDTH))
                    .bottom(px(SETTINGS_ENTRY_BOTTOM))
                    // 窄窗口时夹住，不把设置按钮挤出窗口右边界。
                    .w(px(bottom_actions_w))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(BOTTOM_ACTIONS_GAP))
                    .child(vault_switcher)
                    .child(settings_entry);
                let shell_body = div()
                    .size_full()
                    .when_some(shell, |this, shell| this.child(shell));
                // 本地（不同步）模式：顶部一条固定提示（不覆盖编辑器，也不挤压
                // `AppShell` 内部状态栏）；同步实例 Main 不渲染这一行。
                let mut main = div()
                    .relative()
                    .size_full()
                    .debug_selector(|| "app-root-main".to_string());
                if self.local_mode {
                    main = main.child(
                        div()
                            .flex()
                            .flex_col()
                            .size_full()
                            .child(
                                div()
                                    .debug_selector(|| "app-root-local-mode".to_string())
                                    .w_full()
                                    .flex_shrink_0()
                                    .px_2()
                                    .py_1()
                                    .border_b(px(1.0))
                                    .border_color(theme::sidebar_border())
                                    .bg(theme::sidebar_bg())
                                    .text_size(px(11.0))
                                    .text_color(theme::status_bar_text())
                                    .child(SharedString::from(crate::ui_text::local_mode_notice())),
                            )
                            .child(div().flex_1().min_h(px(0.0)).child(shell_body)),
                    );
                } else {
                    main = main.child(shell_body);
                }
                // 菜单打开时铺一层透明遮罩：点空白关菜单，且这次点击不漏给编辑器；
                // 遮罩在切换器/设置按钮**下面**（后面再加），按钮仍可点（再点 = 收起）。
                if self.vault_menu_open {
                    main = main.child(
                        div()
                            .debug_selector(|| "app-root-vault-menu-backdrop".to_string())
                            .absolute()
                            .inset_0()
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|root, _event: &gpui::MouseDownEvent, _window, cx| {
                                    root.close_vault_menu(cx);
                                    cx.stop_propagation();
                                }),
                            )
                            .on_scroll(cx.listener(
                                |_root, _event: &gpui::ScrollEvent, _window, cx| {
                                    cx.stop_propagation();
                                },
                            )),
                    );
                }
                main = main.child(bottom_actions);
                if self.vault_menu_open {
                    main = main.child(self.render_vault_menu(window, cx));
                }
                main.into_any_element()
            }
            State::Error => self.render_error(cx).into_any_element(),
        };
        let settings = self.render_settings(window, cx);
        // 「管理仓库」覆盖层（Main 里新建/加入/返回欢迎页的唯一入口；与设置互斥）。
        let vault_manager = self.render_vault_manager(window, cx);
        // 日记模板选择器：覆盖层，挂在设置页**之上**（设置页关闭时会被清理）。
        let daily_template_picker = self.daily_template_picker.clone();
        div()
            .relative()
            .size_full()
            .debug_selector(|| "app-root".to_string())
            .flex()
            .flex_col()
            .bg(theme::window_bg())
            .font_family(theme::FONT_FAMILY)
            .track_focus(&self.root_focus)
            // 快捷键录制：**capture 相**在根上先于编辑器/输入框拿到按键，吞掉并
            // stop_propagation —— 录制时绝不能让按键同时触发原命令或落进正文。
            .capture_key_down(cx.listener(|root, event: &gpui::KeyDownEvent, window, cx| {
                if root.handle_shortcut_capture_key(event, cx) {
                    window.prevent_default();
                    cx.stop_propagation();
                }
            }))
            // 全局命令（默认 Ctrl+, 打开设置 / Ctrl+N 新建笔记 / Ctrl+G 图谱）。
            // Main 的 `AppShell` 已被设为 `global_shortcut_direct = false`，由这里
            // 统一拦截并检查宿主覆盖层；Welcome 只响应「打开设置」。
            .on_key_down(cx.listener(|root, event: &gpui::KeyDownEvent, window, cx| {
                if crate::shortcuts::matches(CommandId::OpenSettings, &event.keystroke) {
                    window.prevent_default();
                    if root.settings.is_none()
                        && root.shortcut_capture.is_none()
                        && matches!(root.state, State::Welcome | State::Main)
                        && root.daily_template_picker.is_none()
                        && !root.vault_menu_open
                        && !root.vault_manager_open
                    {
                        root.open_settings(cx);
                    }
                    cx.stop_propagation();
                    return;
                }
                // 设置/录制态/非 Main：新笔记与图谱不得穿透（它们由宿主统一处理）。
                if root.settings.is_some() || root.shortcut_capture.is_some() {
                    return;
                }
                if !matches!(root.state, State::Main) {
                    return;
                }
                if root.vault_menu_open
                    || root.vault_manager_open
                    || root.daily_template_picker.is_some()
                {
                    return;
                }
                let Some(shell) = root.main.clone() else {
                    return;
                };
                if shell.read(cx).shortcuts_blocked(cx) {
                    return;
                }
                if crate::shortcuts::matches(CommandId::NewNote, &event.keystroke) {
                    window.prevent_default();
                    shell.update(cx, |shell, cx| shell.shortcut_create_note(window, cx));
                    cx.stop_propagation();
                } else if crate::shortcuts::matches(CommandId::ToggleGraph, &event.keystroke) {
                    window.prevent_default();
                    shell.update(cx, |shell, cx| shell.shortcut_open_graph(window, cx));
                    cx.stop_propagation();
                }
            }))
            .child(body)
            .when_some(vault_manager, |this, manager| this.child(manager))
            .when_some(settings, |this, settings| this.child(settings))
            .when_some(daily_template_picker, |this, picker| this.child(picker))
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

/// 各页顶部的**开发警示条**：`DEV ONLY · 开发同步…`。
///
/// 只有 `dev-sync` 构建渲染；产品 `sync-ui` 构建返回 `None`（调用方用 `when_some`，
/// 不挂任何节点，避免留下空白行/额外 gap）。文案走 `ui_text` + catalog，不硬编码。
#[cfg(feature = "dev-sync")]
fn banner_text() -> Option<gpui::Div> {
    Some(
        div()
            .debug_selector(|| "app-root-dev-banner".to_string())
            .w_full()
            .text_size(px(11.0))
            .text_color(theme::danger_text())
            .child(SharedString::from(
                crate::ui_text::settings_sync_dev_banner(),
            )),
    )
}

/// 产品构建：没有开发警示条。
#[cfg(not(feature = "dev-sync"))]
fn banner_text() -> Option<gpui::Div> {
    None
}

/// Join 表单的 DEV 状态标注（**只有 `dev-sync` 构建**渲染）。
///
/// 模式不再手选（按地址自动推断），因此这里不再按模式分支：dev-sync 构建里
/// local_dev / remote_tls 两条初始化路径都未在真实 relay / Windows 上验证。
/// 产品构建返回 `None`（调用方用 `when_some`，不挂节点、也不把 DEV 文案编进产品二进制）。
#[cfg(feature = "dev-sync")]
fn join_dev_notice() -> Option<gpui::Div> {
    Some(
        div()
            .debug_selector(|| "app-root-remote-dev-notice".to_string())
            .w_full()
            .text_size(px(11.0))
            .text_color(theme::caret_color())
            .child(SharedString::from(
                "DEV 状态：relay 初始化已接线（local_dev / remote_tls；production TLS + \
                 受限初始化，未在真实 relay / Windows 上验证）。不会忽略证书，\
                 也不提供私有 CA 导入（请自行安装系统根）。",
            )),
    )
}

/// 产品构建：Join 表单不出现任何 DEV 文案。
#[cfg(not(feature = "dev-sync"))]
fn join_dev_notice() -> Option<gpui::Div> {
    None
}

/// 左侧导航的一项（固定分类与插件分类共用同一外观/命中）。
fn settings_nav_item(
    section: SettingsSection,
    selected: bool,
    cx: &mut Context<AppRoot>,
) -> gpui::Stateful<gpui::Div> {
    let key = section.key();
    div()
        .id(SharedString::from(format!("settings-nav-{key}")))
        .debug_selector(move || format!("settings-nav-{key}"))
        .w_full()
        .h(px(26.0))
        .px_2()
        .flex()
        .flex_row()
        .items_center()
        .rounded_sm()
        .cursor_pointer()
        .text_size(px(12.0))
        .text_color(if selected {
            theme::link_color()
        } else {
            theme::text_color()
        })
        .bg(if selected {
            theme::sidebar_selected_bg()
        } else {
            theme::menu_bg()
        })
        .hover(|style| style.bg(theme::sidebar_selected_bg()))
        .child(SharedString::from(section.label()))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |root, _event: &gpui::MouseDownEvent, _window, cx| {
                root.settings_select_section(section, cx);
                cx.stop_propagation();
            }),
        )
}

/// 「插件」分类的一行：名称 + 简短说明 + 启用开关（开关读写插件自己的持久化）。
///
/// `settings-plugin-<key>` / `-toggle` / `-state` 是固定 selector；只有开关区域可点，
/// 整行不可点，避免误触。
fn plugin_switch_row(
    section: SettingsSection,
    enabled: bool,
    cx: &mut Context<AppRoot>,
) -> gpui::AnyElement {
    let key = section.key();
    let row_selector = format!("settings-plugin-{key}");
    let toggle_selector = format!("settings-plugin-{key}-toggle");
    let state_selector = format!("settings-plugin-{key}-state");
    let title = section.label();
    let description = match section {
        SettingsSection::Templates => crate::ui_text::settings_plugins_templates_description(),
        SettingsSection::DailyNotes => crate::ui_text::settings_plugins_daily_notes_description(),
        _ => String::new(),
    };
    let row_id = row_selector.clone();
    let toggle_id = toggle_selector.clone();
    // 开关底色：开 = accent，关 = 灰色轨道；滑块永远白色。
    let track_bg = if enabled {
        theme::search_toggle_active()
    } else {
        theme::input_border()
    };
    let knob_bg = theme::search_toggle_active_text();
    div()
        .id(SharedString::from(row_id))
        .debug_selector(move || row_selector)
        .w_full()
        .flex()
        .flex_row()
        .items_center()
        .gap_3()
        .py(px(10.0))
        .border_b_1()
        .border_color(theme::sidebar_border())
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_size(px(13.0))
                        .text_color(theme::text_color())
                        .child(SharedString::from(title)),
                )
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(theme::status_bar_text())
                        .child(SharedString::from(description)),
                ),
        )
        .child(
            // 视觉上是 iOS/Obsidian 风格的胶囊开关；`-state` 挂在轨道上只作测试锚点，
            // 不再重复显示「已启用/已停用」文字。
            div()
                .id(SharedString::from(toggle_id))
                .debug_selector(move || toggle_selector)
                .flex_shrink_0()
                .w(px(36.0))
                .h(px(20.0))
                .p(px(2.0))
                .flex()
                .items_center()
                .cursor_pointer()
                .rounded(px(10.0))
                .bg(track_bg)
                .child(
                    div()
                        .debug_selector(move || state_selector)
                        .w(px(16.0))
                        .h(px(16.0))
                        .rounded_full()
                        .bg(knob_bg)
                        .when(enabled, |this| this.ml_auto()),
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |root, _event, _window, cx| {
                        root.settings_toggle_plugin(section, cx);
                        cx.stop_propagation();
                    }),
                ),
        )
        .into_any_element()
}

/// 库文件路径 → **父目录名**（左下角切换按钮/菜单里的短名字）。
///
/// 只取目录名：不显示 `vault.sqlite3`、也不显示完整路径（长路径会把侧栏挤坏）。
fn vault_directory_name(path: &Path) -> Option<String> {
    path.parent()
        .and_then(Path::file_name)
        .map(|name| name.to_string_lossy().trim().to_owned())
        .filter(|name| !name.is_empty())
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

/// 底部动作行里的**紧凑**按钮（定宽 + `flex_shrink_0` + 文案不换行/省略）。
///
/// 复用 [`action_button`] 的样式与点击语义（selector 完全不变），只额外收紧宽度：
/// `action_button` 有几十处调用方，宽度是**局部**需求，不能全局改。
fn compact_action_button<F>(
    selector: impl Into<SharedString> + 'static,
    label: impl Into<SharedString>,
    width: f32,
    cx: &mut Context<AppRoot>,
    on_click: F,
) -> gpui::Stateful<gpui::Div>
where
    F: Fn(&mut AppRoot, &mut Context<AppRoot>) + 'static,
{
    action_button(selector, label, false, false, cx, on_click)
        .flex()
        .flex_row()
        .items_center()
        .justify_center()
        .flex_shrink_0()
        .w(px(width))
        // 比通用按钮更紧的内边距：定宽下也要能放下「⚙ 设置」（英文超长时省略）。
        .px(px(6.0))
        .whitespace_nowrap()
        .overflow_hidden()
        .text_ellipsis()
}

/// Welcome 右栏的一张操作卡（点整行展开/收起，不整页跳走）。
fn welcome_action_card(
    selector: &'static str,
    title: &'static str,
    hint: &'static str,
    expanded: bool,
    action: WelcomeAction,
    cx: &mut Context<AppRoot>,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(selector)
        .debug_selector(move || selector.to_string())
        .w_full()
        .min_w(px(0.0))
        .flex()
        .flex_col()
        .gap_1()
        .p(px(14.0))
        .rounded_md()
        .border_1()
        .border_color(if expanded {
            theme::link_color()
        } else {
            theme::sidebar_border()
        })
        .bg(if expanded {
            theme::sidebar_selected_bg()
        } else {
            theme::menu_bg()
        })
        .cursor_pointer()
        .hover(|style| style.bg(theme::sidebar_selected_bg()))
        .child(
            div()
                .w_full()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .text_size(px(14.0))
                        .text_color(theme::text_color())
                        .child(SharedString::from(title)),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .text_size(px(11.0))
                        .text_color(theme::status_bar_text())
                        .child(SharedString::from(if expanded { "▾" } else { "▸" })),
                ),
        )
        .child(
            div()
                .w_full()
                .min_w(px(0.0))
                .text_size(px(11.0))
                .text_color(theme::status_bar_text())
                .child(SharedString::from(hint)),
        )
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |root, _event: &gpui::MouseDownEvent, _window, cx| {
                root.welcome_toggle_action(action, cx);
                cx.stop_propagation();
            }),
        )
}

/// 一行复选框（同意 / 已备份）。
fn checkbox_row(
    selector: &'static str,
    checked: bool,
    label: impl Into<SharedString>,
    cx: &mut Context<AppRoot>,
    on_click: impl Fn(&mut AppRoot, &mut Context<AppRoot>) + 'static,
) -> gpui::Stateful<gpui::Div> {
    let label: SharedString = label.into();
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
                .child(label),
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

/// Create 表单在**尚未应用创建链接**时的默认 relay 模式：产品 `sync-ui` 构建没有
/// `local_dev`，显示 `remote_tls`；应用链接后由链接 scheme 覆盖。Join 不再使用它
/// （Join 提交时按 relay 地址推断）。
fn default_form_mode() -> RelayMode {
    if local_option_available() {
        RelayMode::LocalDev
    } else {
        RelayMode::RemoteTls
    }
}

/// 设置页左侧分类（Obsidian「选项」式导航；**只在本次会话记住**，不持久化）。
///
/// 导航顺序 = 固定分类（[`SettingsSection::FIXED`]）→ 分界线 →
/// **已启用**的插件分类（[`SettingsSection::PLUGIN_SECTIONS`]，模板/日记）。
/// 插件关闭后对应分类整项隐藏；停在该分类时由 `AppRoot` 回退到「插件」页。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SettingsSection {
    /// 外观（主题）。
    #[default]
    Appearance,
    /// 语言 / Language。
    Language,
    /// 快捷键（查看/修改命令绑定；冲突红标 + 顶部汇总；本机 `shortcuts.json`）。
    Shortcuts,
    /// 同步（同步面板：进入分类即渲染，没有展开/收起；底部有 active 实例的公开信息）。
    Sync,
    /// Relay（地址 + 只读的自动模式说明；模式按 scheme 推断）。
    Relay,
    /// 安全（指纹 + V2 实验入口）。
    Security,
    /// 插件（第一方核心插件总开关：模板/日记）。
    Plugins,
    /// 关于（版本/构建信息）。
    About,
    /// 模板（第一方插件设置；仅在模板插件启用时显示）。
    Templates,
    /// 日记（第一方插件设置；仅在日记插件启用时显示）。
    DailyNotes,
}

impl SettingsSection {
    /// 固定分类（永远显示；顺序 = 左侧导航顺序，分界线之前）。
    pub const FIXED: [SettingsSection; 8] = [
        SettingsSection::Appearance,
        SettingsSection::Language,
        SettingsSection::Shortcuts,
        SettingsSection::Sync,
        SettingsSection::Relay,
        SettingsSection::Security,
        SettingsSection::Plugins,
        SettingsSection::About,
    ];

    /// 插件分类（只在对应插件启用时显示；顺序 = 左侧导航顺序，分界线之后）。
    pub const PLUGIN_SECTIONS: [SettingsSection; 2] =
        [SettingsSection::Templates, SettingsSection::DailyNotes];

    /// 全部分类（固定在前、插件在后；仅供枚举/测试，导航本身按固定/插件分组渲染）。
    pub const ALL: [SettingsSection; 10] = [
        SettingsSection::Appearance,
        SettingsSection::Language,
        SettingsSection::Shortcuts,
        SettingsSection::Sync,
        SettingsSection::Relay,
        SettingsSection::Security,
        SettingsSection::Plugins,
        SettingsSection::About,
        SettingsSection::Templates,
        SettingsSection::DailyNotes,
    ];

    /// 稳定短名：固定 selector `settings-nav-<key>` / `settings-section-<key>` 用。
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            SettingsSection::Appearance => "appearance",
            SettingsSection::Language => "language",
            SettingsSection::Shortcuts => "shortcuts",
            SettingsSection::Sync => "sync",
            SettingsSection::Relay => "relay",
            SettingsSection::Security => "security",
            SettingsSection::Plugins => "plugins",
            SettingsSection::About => "about",
            SettingsSection::Templates => "templates",
            SettingsSection::DailyNotes => "daily_notes",
        }
    }

    /// 稳定短名 → 分类；未知 key 返回 `None`（调用方回退「外观」）。
    ///
    /// 与 [`Self::key`] 是同一套值：设置页分类记忆（`settings-ui.json`）只存短名，
    /// 不存枚举序号（插分类不会读错）。
    #[must_use]
    pub fn parse(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|section| section.key() == key)
    }

    /// 是否是插件分类（模板/日记；只在启用时出现在导航里）。
    #[must_use]
    pub const fn is_plugin(self) -> bool {
        matches!(
            self,
            SettingsSection::Templates | SettingsSection::DailyNotes
        )
    }

    /// 是否是固定分类（永远显示）。
    #[must_use]
    pub const fn is_fixed(self) -> bool {
        !self.is_plugin()
    }

    /// 分类当前是否可用：固定分类恒 `true`；插件分类读插件自己的 `enabled` 偏好。
    ///
    /// **不缓存、不复制**开关状态：与侧栏 ribbon 读的是同一份
    /// `templates.json` / `daily-notes.json`。
    #[must_use]
    pub fn is_enabled(self) -> bool {
        match self {
            SettingsSection::Templates => crate::templates::preferences().enabled,
            SettingsSection::DailyNotes => crate::daily_notes::preferences().enabled,
            _ => true,
        }
    }

    /// 导航项文案（走 `ui_text` 的中英文案，不在渲染里写字面量）。
    #[must_use]
    pub fn label(self) -> String {
        match self {
            SettingsSection::Appearance => crate::ui_text::settings_nav_appearance(),
            SettingsSection::Language => crate::ui_text::settings_nav_language(),
            SettingsSection::Shortcuts => crate::ui_text::settings_nav_shortcuts(),
            SettingsSection::Sync => crate::ui_text::settings_nav_sync(),
            SettingsSection::Relay => crate::ui_text::settings_nav_relay(),
            SettingsSection::Security => crate::ui_text::settings_nav_security(),
            SettingsSection::Plugins => crate::ui_text::settings_nav_plugins(),
            SettingsSection::About => crate::ui_text::settings_nav_about(),
            SettingsSection::Templates => crate::ui_text::settings_nav_templates(),
            SettingsSection::DailyNotes => crate::ui_text::settings_nav_daily_notes(),
        }
    }
}

/// 设置页分类偏好的版本（写入时使用；读取时 `version` 字段可缺省）。
pub const SETTINGS_UI_PREFERENCES_VERSION: u32 = 1;

/// 读取本机设置页分类记忆；缺失/不可读/损坏/未知 key → 外观。
///
/// 文件路径 = [`ProfileLayout::settings_ui_path`]（`<数据根>/settings-ui.json`）。
#[must_use]
pub fn load_settings_section(path: &Path) -> SettingsSection {
    let Ok(text) = std::fs::read_to_string(path) else {
        return SettingsSection::Appearance;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return SettingsSection::Appearance;
    };
    // `version` 可缺省（字段最小化写法也接受）；给出就必须是当前版本。
    if let Some(version) = value.get("version") {
        if version.as_u64() != Some(u64::from(SETTINGS_UI_PREFERENCES_VERSION)) {
            return SettingsSection::Appearance;
        }
    }
    value
        .get("section")
        .and_then(serde_json::Value::as_str)
        .and_then(SettingsSection::parse)
        .unwrap_or_default()
}

/// 原子写入本机设置页分类记忆；失败只记日志（本会话仍然生效）。
///
/// 不写正文、不进 vault、不参与同步。
pub fn save_settings_section(path: &Path, section: SettingsSection) {
    let text = format!(
        "{{\n  \"version\": {SETTINGS_UI_PREFERENCES_VERSION},\n  \"section\": \"{}\"\n}}\n",
        section.key()
    );
    if let Err(error) = write_json_file_atomically(path, &text) {
        eprintln!(
            "[menghunote] 保存设置页分类失败：{}（{error}）",
            path.display()
        );
    }
}

/// 当前构建的 feature 列表（「关于」区只读展示；`--no-default-features` 等构建不同）。
fn build_features() -> String {
    const FEATURES: [(&str, bool); 3] = [
        ("sync", cfg!(feature = "sync")),
        ("sync-ui", cfg!(feature = "sync-ui")),
        ("dev-sync", cfg!(feature = "dev-sync")),
    ];
    let compiled: Vec<&str> = FEATURES
        .into_iter()
        .filter_map(|(name, enabled)| enabled.then_some(name))
        .collect();
    if compiled.is_empty() {
        "none".to_owned()
    } else {
        compiled.join(" + ")
    }
}

/// Main 左下角：底部动作行（切换器 + 设置）的底边距窗口底部（避开 `AppShell` 24px 状态栏）。
const SETTINGS_ENTRY_BOTTOM: f32 = 30.0;
/// 动作行内两个按钮之间的水平间隙。
const BOTTOM_ACTIONS_GAP: f32 = 6.0;
/// 「设置」紧凑按钮宽度（56–64px；不再撑满剩余宽度，也不再单独占一行）。
const SETTINGS_ENTRY_WIDTH: f32 = 64.0;
/// 动作行的行高（按钮 py5 + 12px 文案 + 1px 边框 ≈ 31.5；向上取整用于菜单底边）。
const BOTTOM_ACTIONS_ROW_HEIGHT: f32 = 32.0;
/// vault 切换菜单的底边：在动作行**上方** 4px 处向上展开（不盖住按钮）。
const VAULT_MENU_BOTTOM: f32 = SETTINGS_ENTRY_BOTTOM + BOTTOM_ACTIONS_ROW_HEIGHT + 4.0;
/// 左下角动作行宽度（跳过 ribbon 后的侧栏宽度；窄窗口会被 [`bottom_actions_width`] 夹小）。
const VAULT_SIDEBAR_ACTION_WIDTH: f32 = 196.0;
/// 动作行宽度的下限：再窄就放弃夹取（和旧实现一样允许溢出，不把按钮压成 0 宽）。
const BOTTOM_ACTIONS_MIN_WIDTH: f32 = 140.0;

/// 底部动作行的实际宽度：默认 196（侧栏宽度），窄窗口时夹进窗口（右侧留 8px）。
fn bottom_actions_width(window_width: f32) -> f32 {
    (window_width - (8.0 + RIBBON_WIDTH) - 8.0)
        .clamp(BOTTOM_ACTIONS_MIN_WIDTH, VAULT_SIDEBAR_ACTION_WIDTH)
}

/// 「管理仓库」一行（**渲染与测试共用**的投影：名字/路径/角色只有一份）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultManagerRow {
    /// 实例 id（「打开」/「设为默认」/「重命名」的目标）。
    pub instance_id: String,
    /// 显示名：自定义名 → 库目录名 → 短 id（**不再只显示 `inst-...`**）。
    pub name: String,
    /// 次要信息：`API v2 · remote_tls · vault_id xxxxxxxx…`。
    pub meta: String,
    /// 库文件完整路径（渲染时省略；测试断言"路径仍在"）。
    pub path: String,
    /// 是否是公开配置里的 active（默认实例；行边框高亮）。
    pub is_active: bool,
    /// 是否是当前真正打开的实例（「打开」= no-op）。
    pub opened: bool,
}

/// 左下角切换菜单的一行（**渲染与测试共用**的投影：文案/✓/次要信息只有一份）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultMenuRow {
    /// 该行对应的实例 id（当前 vault 行也带上：点击 = no-op）。
    pub instance_id: String,
    /// 显示文案：当前 vault 行带 `✓ ` 前缀，其它行是短名字（目录名）。
    pub label: String,
    /// 次要信息：当前行「当前 vault」，其它行 `API v2 · remote_tls`。
    pub meta: String,
    /// 是否当前打开的 vault（渲染为高亮 + `✓`；点击 = no-op）。
    pub current: bool,
}

/// 左下角切换菜单/管理仓库里的一行（名字省略 + 可选次要信息）。
///
/// 行本身可点（菜单 = 直接切换；管理仓库由按钮承载动作，行只高亮当前项），
/// 长名字/长路径一律 `overflow_hidden + text_ellipsis`：不把菜单撑出窗口。
fn vault_list_row(
    selector: String,
    label: String,
    meta: Option<String>,
    accent: bool,
    on_click: impl Fn(&mut AppRoot, &mut Context<AppRoot>) + 'static,
    cx: &mut Context<AppRoot>,
) -> gpui::Stateful<gpui::Div> {
    let mut row = div()
        .id(SharedString::from(selector.clone()))
        .debug_selector(move || selector)
        .w_full()
        .min_w(px(0.0))
        .flex()
        .flex_col()
        .gap_1()
        .px_2()
        .py_1()
        .rounded_sm()
        .cursor_pointer()
        .hover(|style| style.bg(theme::sidebar_selected_bg()))
        .child(
            div()
                .w_full()
                .min_w(px(0.0))
                .overflow_hidden()
                .text_ellipsis()
                .text_size(px(12.0))
                .text_color(if accent {
                    theme::link_color()
                } else {
                    theme::text_color()
                })
                .child(SharedString::from(label)),
        );
    if let Some(meta) = meta {
        row = row.child(
            div()
                .w_full()
                .min_w(px(0.0))
                .overflow_hidden()
                .text_ellipsis()
                .text_size(px(10.0))
                .text_color(theme::status_bar_text())
                .child(SharedString::from(meta)),
        );
    }
    row.on_mouse_down(
        MouseButton::Left,
        cx.listener(move |root, _event: &gpui::MouseDownEvent, _window, cx| {
            on_click(root, cx);
            cx.stop_propagation();
        }),
    )
}

impl AppRoot {
    /// 左下角菜单/管理仓库打开后的**首帧**抢焦点：否则 `Esc` 由编辑器收，覆盖层收不到。
    ///
    /// 只在打开后的第一帧做一次（`*_focused_once`），关闭时复位；不重复抢焦点。
    fn focus_vault_overlay_once(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.vault_menu_open && !self.vault_menu_focused_once {
            self.vault_menu_focused_once = true;
            let focus = self.vault_menu_focus.clone();
            window.defer(cx, move |window, cx| {
                window.focus(&focus, cx);
            });
        }
        if self.vault_manager_open && !self.vault_manager_focused_once {
            self.vault_manager_focused_once = true;
            let focus = self.vault_manager_focus.clone();
            window.defer(cx, move |window, cx| {
                window.focus(&focus, cx);
            });
        }
    }

    /// Main 左下角的 vault 切换菜单（Obsidian 式下拉；在按钮上方向上展开、限高可滚）。
    ///
    /// 当前 vault 行带 `✓`（点击 = no-op 并收起菜单）；其它已保存实例逐行（名字 +
    /// 简短 API/mode），点击 = 直接切换（关旧开新，不经过设置页）；底部「管理仓库…」。
    fn render_vault_menu(&self, window: &Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let window_size = window.bounds().size;
        let left = 8.0 + RIBBON_WIDTH;
        // 不超出窗口：宽度/高度按当前窗口夹住；实例很多时菜单内部滚动。
        let menu_w = (window_size.width.as_f32() - left - 8.0).clamp(160.0, 248.0);
        let menu_max_h =
            (window_size.height.as_f32() - VAULT_MENU_BOTTOM - 12.0).clamp(120.0, 420.0);
        let rows = self.vault_menu_rows(cx);
        let mut items = div().w_full().flex().flex_col().gap_1();
        for (index, row) in rows.iter().enumerate() {
            // 当前行固定 selector（唯一）；其它行用**已保存顺序**下标（active 优先）。
            let selector = if row.current {
                "app-root-vault-menu-current".to_owned()
            } else {
                format!("app-root-vault-menu-instance-{index}")
            };
            let select_id = row.instance_id.clone();
            items = items.child(vault_list_row(
                selector,
                row.label.clone(),
                Some(row.meta.clone()),
                row.current,
                move |root, cx| root.switch_to_instance(&select_id, cx),
                cx,
            ));
        }
        if rows.is_empty() {
            items = items.child(hint_text("还没有已保存的同步 vault。").w_full());
        }
        let menu = div()
            .id("app-root-vault-menu")
            .debug_selector(|| "app-root-vault-menu".to_string())
            .absolute()
            .left(px(left))
            .bottom(px(VAULT_MENU_BOTTOM))
            .w(px(menu_w))
            .max_h(px(menu_max_h))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_1()
            .p_1()
            .bg(theme::menu_bg())
            .border_1()
            .border_color(theme::sidebar_border())
            .rounded_md()
            .shadow_lg()
            .track_focus(&self.vault_menu_focus)
            .on_key_down(
                cx.listener(|root, event: &gpui::KeyDownEvent, _window, cx| {
                    if event.keystroke.key == "escape" {
                        root.close_vault_menu(cx);
                        cx.stop_propagation();
                    }
                }),
            )
            // 菜单内部的点击不能冒泡到遮罩（否则先关菜单再点空）。
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_root, _event: &gpui::MouseDownEvent, _window, cx| {
                    cx.stop_propagation();
                }),
            )
            .child(items)
            .child(
                div()
                    .debug_selector(|| "app-root-vault-menu-divider".to_string())
                    .w_full()
                    .h(px(1.0))
                    .my_1()
                    .flex_shrink_0()
                    .bg(theme::sidebar_border()),
            )
            .child(action_button(
                "app-root-vault-manage",
                "管理仓库…",
                false,
                false,
                cx,
                |root, cx| root.open_vault_manager(cx),
            ));
        menu.into_any_element()
    }

    /// 「管理仓库」覆盖层：实例列表（打开/设为默认）+ 新建/加入/返回欢迎页。
    ///
    /// 这是 Main 里创建 vault 的**唯一入口**（设置页不再有「实例」分类）。
    /// 每个实例一行：名字/API/mode/vault_id/路径（省略）+「打开」+「设为默认」。
    fn render_vault_manager(
        &mut self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        // 只在 Main 里渲染：离开 Main（关窗/返回欢迎页/进 Error）时管理仓库必须消失。
        if !self.vault_manager_open || self.state != State::Main {
            return None;
        }
        // 重命名行内输入：打开后的**首帧**抢焦点（否则要先点一下输入框才能打字）。
        if let Some(rename) = &mut self.vault_rename {
            if !rename.focused_once {
                rename.focused_once = true;
                let input = rename.input.clone();
                window.defer(cx, move |window, cx| {
                    input.update(cx, |input, cx| input.focus(window, cx));
                });
            }
        }
        let rows = self.vault_manager_rows();
        let rename_target = self
            .vault_rename
            .as_ref()
            .map(|rename| rename.instance_id.clone());
        let rename_input = self
            .vault_rename
            .as_ref()
            .map(|rename| rename.input.clone());
        let rename_error = self
            .vault_rename
            .as_ref()
            .and_then(|rename| rename.error.clone());
        let notice = self.vault_manager_notice.clone();
        let window_size = window.bounds().size;
        let panel_w = (window_size.width.as_f32() - 40.0).clamp(320.0, 680.0);
        let panel_h = (window_size.height.as_f32() - 112.0).clamp(240.0, 720.0);

        let mut list = div()
            .id("vault-manager-list")
            .debug_selector(|| "vault-manager-list".to_string())
            .w_full()
            .flex()
            .flex_col()
            .gap_1();
        if rows.is_empty() {
            list = list.child(
                hint_text("还没有已保存的 vault；可以点「同步远程仓库…」粘贴创建/邀请链接。")
                    .w_full(),
            );
        }
        for (index, row) in rows.iter().enumerate() {
            let row_selector = format!("vault-manager-instance-{index}");
            let open_selector = format!("vault-manager-open-{index}");
            let default_selector = format!("vault-manager-default-{index}");
            let rename_selector = format!("vault-manager-rename-{index}");
            let open_id = row.instance_id.clone();
            let default_id = row.instance_id.clone();
            let rename_id = row.instance_id.clone();
            let renaming = rename_target.as_deref() == Some(row.instance_id.as_str());
            let mut actions = div()
                .w_full()
                .flex()
                .flex_row()
                .flex_wrap()
                .items_center()
                .gap_2()
                // 「打开」= 关旧开新（不重启）；当前实例 = no-op。
                .child(action_button(
                    open_selector,
                    if row.opened { "已打开" } else { "打开" },
                    row.opened,
                    false,
                    cx,
                    move |root, cx| root.switch_to_instance(&open_id, cx),
                ))
                // 「设为默认」只写公开配置 + 本机启动入口，不热切换。
                .child(action_button(
                    default_selector,
                    if row.is_active {
                        "当前默认"
                    } else {
                        "设为默认"
                    },
                    false,
                    row.is_active,
                    cx,
                    move |root, cx| root.vault_manager_set_active(&default_id, cx),
                ))
                // 「重命名」= 行内输入（只改显示名，不移动目录/不碰实例锁）。
                .child(action_button(
                    rename_selector,
                    crate::ui_text::vault_rename(),
                    false,
                    renaming,
                    cx,
                    move |root, cx| root.begin_vault_rename(&rename_id, cx),
                ));
            if renaming {
                if let Some(input) = rename_input.clone() {
                    let rename_row = div()
                        .debug_selector(|| "vault-manager-rename-row".to_string())
                        .w_full()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.0))
                                .px_2()
                                .py_1()
                                .bg(theme::window_bg())
                                .border_1()
                                .border_color(theme::input_border())
                                .rounded_sm()
                                .child(input),
                        )
                        .child(action_button(
                            "vault-manager-rename-save",
                            crate::ui_text::vault_rename_save(),
                            true,
                            false,
                            cx,
                            |root, cx| root.save_vault_rename(cx),
                        ))
                        .child(action_button(
                            "vault-manager-rename-cancel",
                            crate::ui_text::vault_rename_cancel(),
                            false,
                            false,
                            cx,
                            |root, cx| root.cancel_vault_rename(cx),
                        ));
                    actions = actions.child(rename_row);
                    actions = actions.child(
                        div()
                            .debug_selector(|| "vault-manager-rename-hint".to_string())
                            .w_full()
                            .text_size(px(10.0))
                            .text_color(theme::status_bar_text())
                            .child(SharedString::from(crate::ui_text::vault_name_placeholder())),
                    );
                    if let Some(error) = rename_error.clone() {
                        actions = actions.child(
                            div()
                                .debug_selector(|| "vault-manager-rename-error".to_string())
                                .w_full()
                                .text_size(px(11.0))
                                .text_color(theme::danger_text())
                                .child(SharedString::from(error)),
                        );
                    }
                }
            }
            list = list.child(
                div()
                    .id(SharedString::from(row_selector.clone()))
                    .debug_selector(move || row_selector)
                    .w_full()
                    .min_w(px(0.0))
                    .flex()
                    .flex_col()
                    .gap_1()
                    .p_2()
                    .rounded_sm()
                    .border_1()
                    .border_color(if row.is_active {
                        theme::link_color()
                    } else {
                        theme::sidebar_border()
                    })
                    .bg(theme::sidebar_bg())
                    // 名字：自定义名优先（回退目录名/短 id），不再只显示 `inst-...`。
                    .child(
                        div()
                            .debug_selector(|| "vault-manager-name".to_string())
                            .w_full()
                            .min_w(px(0.0))
                            .overflow_hidden()
                            .text_ellipsis()
                            .text_size(px(12.0))
                            .text_color(if row.is_active {
                                theme::link_color()
                            } else {
                                theme::text_color()
                            })
                            .child(SharedString::from(if row.is_active {
                                format!("✓ {}", row.name)
                            } else {
                                row.name.clone()
                            })),
                    )
                    .child(
                        div()
                            .w_full()
                            .min_w(px(0.0))
                            .overflow_hidden()
                            .text_ellipsis()
                            .text_size(px(11.0))
                            .text_color(theme::status_bar_text())
                            .child(SharedString::from(row.meta.clone())),
                    )
                    .child(
                        // 长路径省略：不把下面的按钮挤乱（内容区仍可滚动）。
                        div()
                            .w_full()
                            .min_w(px(0.0))
                            .overflow_hidden()
                            .text_ellipsis()
                            .text_size(px(10.0))
                            .text_color(theme::status_bar_text())
                            .child(SharedString::from(row.path.clone())),
                    )
                    .child(actions),
            );
        }

        let panel = div()
            .id("app-root-vault-manager")
            .debug_selector(|| "app-root-vault-manager".to_string())
            .w(px(panel_w))
            .h(px(panel_h))
            .overflow_hidden()
            .flex()
            .flex_col()
            .gap_2()
            .p(px(14.0))
            .bg(theme::menu_bg())
            .border_1()
            .border_color(theme::sidebar_border())
            .rounded_md()
            .shadow_lg()
            .track_focus(&self.vault_manager_focus)
            .on_key_down(
                cx.listener(|root, event: &gpui::KeyDownEvent, _window, cx| {
                    if event.keystroke.key == "escape" {
                        // 重命名进行中：Esc 只取消行内输入（不关管理仓库、不改配置）。
                        if root.vault_rename_is_open() {
                            root.cancel_vault_rename(cx);
                        } else {
                            root.close_vault_manager(cx);
                        }
                        cx.stop_propagation();
                    }
                }),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_root, _event: &gpui::MouseDownEvent, _window, cx| {
                    cx.stop_propagation();
                }),
            )
            .child(
                div()
                    .w_full()
                    .flex_shrink_0()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(title_text("管理仓库"))
                    .child(div().flex_1())
                    .child(action_button(
                        "vault-manager-close",
                        crate::ui_text::settings_close(),
                        false,
                        false,
                        cx,
                        |root, cx| root.close_vault_manager(cx),
                    )),
            )
            .child(
                hint_text(
                    "切换 = 先 flush 关闭当前库并释放实例锁，再打开目标 vault；\
                     不重启应用、不热切换 worker。「重命名」只改显示名（sync.json），\
                     不移动目录、不改库路径、不碰实例锁。",
                )
                .w_full(),
            )
            .child(
                div()
                    .debug_selector(|| "vault-manager-actions".to_string())
                    .w_full()
                    .flex_shrink_0()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(action_button(
                        "vault-manager-sync-remote",
                        "同步远程仓库…",
                        true,
                        false,
                        cx,
                        |root, cx| root.vault_manager_sync_remote(cx),
                    ))
                    .child(action_button(
                        "vault-manager-back-welcome",
                        "返回欢迎页",
                        false,
                        false,
                        cx,
                        |root, cx| root.vault_manager_back_to_welcome(cx),
                    )),
            )
            .child(
                div()
                    .id("vault-manager-content")
                    .debug_selector(|| "vault-manager-content".to_string())
                    .w_full()
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(list)
                    .when_some(notice, |this, notice| {
                        this.child(
                            div()
                                .debug_selector(|| "vault-manager-notice".to_string())
                                .w_full()
                                .text_size(px(11.0))
                                .text_color(theme::caret_color())
                                .child(SharedString::from(notice)),
                        )
                    }),
            );

        Some(
            div()
                .debug_selector(|| "app-root-vault-manager-backdrop".to_string())
                .absolute()
                .inset_0()
                .flex()
                .items_start()
                .justify_center()
                .pt(px(56.0))
                .bg(theme::overlay_scrim())
                // 覆盖层拦住滚轮（不把滚动漏给下面的编辑器）。
                .on_scroll(
                    cx.listener(|_root, _event: &gpui::ScrollEvent, _window, cx| {
                        cx.stop_propagation();
                    }),
                )
                // 点空白 = 关闭管理仓库（未保存的重命名一并丢弃，磁盘不变）。
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|root, _event: &gpui::MouseDownEvent, _window, cx| {
                        root.close_vault_manager(cx);
                        cx.stop_propagation();
                    }),
                )
                .child(panel)
                .into_any_element(),
        )
    }

    /// 设置面板（overlay）：左侧分类导航 + 右侧**只渲染当前分类**的内容区。
    ///
    /// 只投影公开配置；不持有 setup secret/恢复码/root/manager seed（类型上也没有）。
    /// 关闭/取消不写配置；保存只写公开配置，运行中的 `AppShell`/Vault/worker 不动。
    ///
    /// 布局：标题 + 两栏（左侧固定导航 / 右侧独立滚动内容）+ 底部固定（错误/提示/保存/关闭）。
    /// 覆盖层仍然限高、拦截滚轮（不穿透到底下的编辑器）。
    fn render_settings(&self, window: &Window, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let (active_entry, stored_fingerprint, stored_security, error, notice, relay) = {
            let settings = self.settings.as_ref()?;
            (
                settings.active.clone(),
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
                        crate::ui_text::settings_fingerprint_instance_closed(),
                    )
                })
            }
        });
        let security = self
            .main
            .as_ref()
            .and_then(|shell| shell.read(cx).dev_sync_security_category())
            .or(stored_security);
        let public_section: gpui::AnyElement = match active_entry {
            Some(entry) => {
                let vault_id = entry.vault_id.clone();
                let device_id = entry.device_id.clone();
                let vault_path = entry.vault_path.clone();
                div()
                    .debug_selector(|| "settings-public-info".to_string())
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .pt(px(4.0))
                    .border_t_1()
                    .border_color(theme::sidebar_border())
                    .child(section_text(crate::ui_text::settings_public_title()))
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
                            .flex_wrap()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.0))
                                    .text_size(px(11.0))
                                    .text_color(theme::text_color())
                                    .child(SharedString::from(crate::ui_text::settings_vault_id(
                                        &vault_id,
                                    ))),
                            )
                            .child(action_button(
                                "settings-copy-vault-id",
                                crate::ui_text::settings_copy(),
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
                            .flex_wrap()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.0))
                                    .text_size(px(11.0))
                                    .text_color(theme::text_color())
                                    .child(SharedString::from(crate::ui_text::settings_device_id(
                                        &device_id,
                                    ))),
                            )
                            .child(action_button(
                                "settings-copy-device-id",
                                crate::ui_text::settings_copy(),
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
                            .flex_wrap()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.0))
                                    .text_size(px(11.0))
                                    .text_color(theme::status_bar_text())
                                    .child(SharedString::from(
                                        crate::ui_text::settings_vault_path(&vault_path),
                                    )),
                            )
                            .child(action_button(
                                "settings-copy-path",
                                crate::ui_text::settings_copy(),
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
            None => hint_text(crate::ui_text::settings_no_active_hint()).into_any_element(),
        };

        let has_error = error.is_some();
        let error_text = error.unwrap_or_default();
        let notice_text = notice.unwrap_or_else(crate::ui_text::settings_notice_default);
        // relay 的保存/重启提示只属于「Relay」分类；其它分类的字段都是即时生效的
        // 本机设置（外观/语言/插件字段），底栏不能显示「保存 relay」造成误解。
        let relay_section = self.settings_section == SettingsSection::Relay;

        // 同步面板作为「同步」分类的内容（唯一设置入口里同时有连接配置与同步操作）；
        // 默认构建/未启用时 `None`，整块不存在。
        #[cfg(feature = "sync-ui")]
        let sync_panel = self
            .main
            .as_ref()
            .and_then(|shell| shell.read(cx).dev_sync_panel());

        // 当前分类可能是一个刚被关掉的插件分类（例如偏好被外部改过）：回退到「插件」，
        // 保证内容区永远不是空白页；状态本身由所有开关路径即时同步。
        let section = if self.settings_section.is_plugin() && !self.settings_section.is_enabled() {
            SettingsSection::Plugins
        } else {
            self.settings_section
        };

        // ---- 左侧分类导航（固定宽度，独立滚动）----
        let mut nav = div()
            .id("settings-nav")
            .debug_selector(|| "settings-nav".to_string())
            .w(px(152.0))
            .flex_shrink_0()
            .h_full()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_1()
            .px_2()
            .py_2()
            .border_r(px(1.0))
            .border_color(theme::sidebar_border())
            .child(
                div()
                    .px_1()
                    .pb_1()
                    .text_size(px(11.0))
                    .text_color(theme::status_bar_text())
                    .child(SharedString::from(crate::ui_text::settings_nav_title())),
            );
        // 固定分类永远显示。
        for item in SettingsSection::FIXED {
            nav = nav.child(settings_nav_item(item, item == section, cx));
        }
        // 分界线：固定项之后、插件项之前（1px 主题线 + 上下间距；整行不可点）。
        nav = nav.child(
            div()
                .debug_selector(|| "settings-nav-divider".to_string())
                .w_full()
                .h(px(1.0))
                .my_1()
                .flex_shrink_0()
                .bg(theme::sidebar_border()),
        );
        // 插件分类只显示已启用的（关闭后整项消失；ribbon 用同一份 enabled）。
        for item in SettingsSection::PLUGIN_SECTIONS {
            if !item.is_enabled() {
                continue;
            }
            nav = nav.child(settings_nav_item(item, item == section, cx));
        }

        // ---- 右侧内容：只渲染当前分类 ----
        let section_key = section.key();
        let mut content = div()
            .id(SharedString::from(format!(
                "settings-section-{section_key}"
            )))
            .debug_selector(move || format!("settings-section-{section_key}"))
            .w_full()
            .flex()
            .flex_col()
            .gap_2();
        match section {
            SettingsSection::Appearance => {
                content = content.child(crate::theme_settings::appearance_section());
            }
            SettingsSection::Language => {
                content = content.child(crate::language_settings::language_section());
            }
            SettingsSection::Shortcuts => {
                content = content.child(self.render_shortcuts_section(cx));
            }
            SettingsSection::Templates => {
                content = match self.templates_settings.clone() {
                    Some(panel) => content.child(div().w_full().child(panel)),
                    None => content.child(hint_text("模板设置尚未初始化（重新打开设置页）")),
                };
            }
            SettingsSection::DailyNotes => {
                content = match self.daily_notes_settings.clone() {
                    Some(panel) => content.child(div().w_full().child(panel)),
                    None => content.child(hint_text("日记设置尚未初始化（重新打开设置页）")),
                };
            }
            SettingsSection::Sync => {
                // 进入「同步」分类就直接渲染面板内容：不再有展开/收起状态与标题行。
                // `settings-toggle-sync` 保留为**同步内容容器**（兼容测试/selector 契约）。
                content = content.child(section_text(crate::ui_text::settings_nav_sync()));
                let mut sync_content = div()
                    .id("settings-toggle-sync")
                    .debug_selector(|| "settings-toggle-sync".to_string())
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap_2();
                match sync_panel {
                    Some(panel) => {
                        sync_content = sync_content.child(div().w_full().child(panel));
                    }
                    None => {
                        sync_content = sync_content
                            .child(hint_text(crate::ui_text::settings_sync_unavailable()));
                    }
                }
                content = content.child(sync_content);
                // active 实例的**公开**信息（vault_id/device_id/API 面/库路径）：
                // 「实例」分类已移除（用户反馈没有存在感），但公开信息仍要能看，
                // 因此贴在同步内容下方（只读、可复制；不含任何秘密）。
                content = content.child(public_section);
            }
            SettingsSection::Plugins => {
                // Obsidian「核心插件」式总开关页：只列**有运行时开关**的第一方插件
                // （模板/日记）；Mermaid/图谱没有可切换的开关，不列进来。
                content = content
                    .child(section_text(crate::ui_text::settings_plugins_title()))
                    .child(hint_text(crate::ui_text::settings_plugins_hint()))
                    .child(
                        div()
                            .id("settings-plugins")
                            .debug_selector(|| "settings-plugins".to_string())
                            .w_full()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(plugin_switch_row(
                                SettingsSection::Templates,
                                crate::templates::preferences().enabled,
                                cx,
                            ))
                            .child(plugin_switch_row(
                                SettingsSection::DailyNotes,
                                crate::daily_notes::preferences().enabled,
                                cx,
                            )),
                    );
            }
            SettingsSection::Relay => {
                content = content
                    .child(section_text(crate::ui_text::settings_relay_title()))
                    // 模式不再提供可点选项：只读说明按地址自动识别。
                    .child(
                        hint_text(crate::ui_text::settings_relay_auto_mode())
                            .debug_selector(|| "settings-relay-mode-hint".to_string())
                            .w_full(),
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
                            .child(relay),
                    )
                    .child(hint_text(crate::ui_text::settings_relay_remote_hint()).w_full())
                    .when(local_option_available(), |this| {
                        this.child(hint_text(crate::ui_text::settings_relay_local_hint()).w_full())
                    });
            }
            SettingsSection::Security => {
                content =
                    content.child(self.render_settings_fingerprint(&fingerprint, security, cx));
                // V2 管理是实验功能：个人用户只需要 V1 设备列表/邀请/移除。
                // 只在 `dev-sync` 构建 + `MENGHUNOTE_EXPERIMENTAL_V2=1` 时渲染（安全分类下）。
                content = content
                    .when_some(self.v2_management_block_if_enabled(cx), |this, block| {
                        this.child(block)
                    });
            }
            SettingsSection::About => {
                content = content
                    .child(section_text(crate::ui_text::settings_about_title()))
                    .child(
                        div()
                            .debug_selector(|| "settings-about-version".to_string())
                            .w_full()
                            .text_size(px(12.0))
                            .text_color(theme::text_color())
                            .child(SharedString::from(crate::ui_text::settings_about_version(
                                env!("CARGO_PKG_VERSION"),
                            ))),
                    )
                    .child(
                        div()
                            .debug_selector(|| "settings-about-build".to_string())
                            .w_full()
                            .text_size(px(11.0))
                            .text_color(theme::status_bar_text())
                            .child(SharedString::from(crate::ui_text::settings_about_build(
                                &build_features(),
                            ))),
                    )
                    .child(hint_text(crate::ui_text::settings_about_hint()));
            }
        }

        Some(
            div()
                .id("dev-sync-settings-backdrop")
                .absolute()
                .inset_0()
                .flex()
                .items_start()
                .justify_center()
                .pt(px(56.0))
                .bg(theme::overlay_scrim())
                // 设置页是覆盖层：滚轮/触控板滚动到这里就停住，不能穿透到下面的
                // 编辑器（真机反馈：滚设置页时底部笔记也跟着滚）。
                .on_scroll(
                    cx.listener(|_root, _event: &gpui::ScrollEvent, _window, cx| {
                        cx.stop_propagation();
                    }),
                )
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
                        // 快捷键录制需要一个焦点节点（点击「修改」后我们把焦点放到
                        // 面板本身；根上的 capture 监听器先于所有子节点收按键）。
                        .track_focus(&self.settings_focus)
                        .w(px(760.0))
                        .h(px(
                            (window.bounds().size.height.as_f32() - 112.0).clamp(240.0, 900.0)
                        ))
                        // 外层不滚：标题、左导航、右内容、底栏各自约束（内容区独立滚动）。
                        .overflow_hidden()
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
                        // 标题用通用「设置」：这是一张带分类导航的产品设置页，
                        // 不再是只讲同步的卡片（DEV banner 仍然单独一行）。
                        .child(title_text(crate::ui_text::settings_nav_title()))
                        .when_some(banner_text(), |this, banner| this.child(banner))
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .flex_1()
                                .min_h(px(0.0))
                                .w_full()
                                .child(nav)
                                .child(
                                    div()
                                        .id("settings-content")
                                        .debug_selector(|| "settings-content".to_string())
                                        .flex_1()
                                        .min_w(px(0.0))
                                        .min_h(px(0.0))
                                        .h_full()
                                        .overflow_y_scroll()
                                        .px_3()
                                        .py_1()
                                        .child(content),
                                ),
                        )
                        .child(
                            div()
                                .w_full()
                                .flex_shrink_0()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .pt(px(4.0))
                                .border_t_1()
                                .border_color(theme::sidebar_border())
                                .child(
                                    div()
                                        .w_full()
                                        .when(relay_section, |this| {
                                            this.child(
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
                                                    .debug_selector(|| {
                                                        "settings-notice".to_string()
                                                    })
                                                    .w_full()
                                                    .text_size(px(11.0))
                                                    .text_color(theme::caret_color())
                                                    .child(SharedString::from(notice_text)),
                                            )
                                        })
                                        .child(
                                            div()
                                                .flex()
                                                .flex_row()
                                                .gap_2()
                                                .when(relay_section, |this| {
                                                    this.child(action_button(
                                                        "settings-save",
                                                        crate::ui_text::settings_save(),
                                                        true,
                                                        false,
                                                        cx,
                                                        |root, cx| root.settings_save(cx),
                                                    ))
                                                    .child(action_button(
                                                        "settings-cancel",
                                                        crate::ui_text::settings_close_without_save(
                                                        ),
                                                        false,
                                                        false,
                                                        cx,
                                                        |root, cx| root.close_settings(cx),
                                                    ))
                                                })
                                                .when(!relay_section, |this| {
                                                    this.child(action_button(
                                                        "settings-close",
                                                        crate::ui_text::settings_close(),
                                                        false,
                                                        false,
                                                        cx,
                                                        |root, cx| root.close_settings(cx),
                                                    ))
                                                }),
                                        ),
                                ),
                        ),
                )
                .into_any_element(),
        )
    }

    /// 设置页「快捷键」分类：标题 + 说明 + 顶部冲突汇总 + 每个命令一行。
    ///
    /// 固定 selector（headless 测试点击/断言用）：
    /// `settings-shortcuts` / `shortcut-row-<id>` / `shortcut-value-<id>` /
    /// `shortcut-edit-<id>` / `shortcut-reset-<id>` / `shortcut-conflict-<id>` /
    /// `shortcut-conflicts-summary` / `shortcut-capture`。
    ///
    /// 冲突**只提醒、不禁止保存**：行上红标 + 顶部汇总；录制成功后立即写盘并生效。
    fn render_shortcuts_section(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let snapshot = crate::shortcuts::snapshot();
        let capture = self.shortcut_capture;
        let notice = self.shortcut_capture_notice.clone();
        let has_conflicts = snapshot.conflict_count() > 0;
        let mut section = div()
            .id("settings-shortcuts")
            .debug_selector(|| "settings-shortcuts".to_string())
            .w_full()
            .flex()
            .flex_col()
            .gap_1()
            .child(section_text(crate::shortcuts::title()))
            .child(hint_text(crate::shortcuts::hint()).w_full())
            .child(
                div()
                    .debug_selector(|| "shortcut-conflicts-summary".to_string())
                    .w_full()
                    .text_size(px(11.0))
                    .text_color(if has_conflicts {
                        theme::danger_text()
                    } else {
                        theme::status_bar_text()
                    })
                    .child(SharedString::from(snapshot.summary())),
            );
        // 非录制态的提示（恢复默认写盘失败等）：只在有内容时挂一行，避免空白。
        if let Some(notice) = notice.as_ref().filter(|_| capture.is_none()) {
            section = section.child(
                div()
                    .debug_selector(|| "shortcut-notice".to_string())
                    .w_full()
                    .text_size(px(11.0))
                    .text_color(theme::danger_text())
                    .child(SharedString::from(notice.clone())),
            );
        }
        for row in &snapshot.rows {
            let command = row.command;
            let id = command.as_str();
            let row_selector = format!("shortcut-row-{id}");
            let value_selector = format!("shortcut-value-{id}");
            let edit_selector = format!("shortcut-edit-{id}");
            let reset_selector = format!("shortcut-reset-{id}");
            let conflict_selector = format!("shortcut-conflict-{id}");
            let is_capturing = capture == Some(command);
            let mut line = div()
                .id(SharedString::from(row_selector.clone()))
                .debug_selector(move || row_selector)
                .w_full()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .py(px(6.0))
                .border_b_1()
                .border_color(theme::sidebar_border())
                .when(is_capturing, |this| this.bg(theme::sidebar_selected_bg()))
                .child(
                    div()
                        .w(px(104.0))
                        .flex_shrink_0()
                        .text_size(px(12.0))
                        .text_color(theme::text_color())
                        .child(SharedString::from(command.label())),
                );
            if is_capturing {
                let capture_text = notice
                    .clone()
                    .unwrap_or_else(crate::shortcuts::capture_prompt);
                line = line.child(
                    div()
                        .id("shortcut-capture")
                        .debug_selector(|| "shortcut-capture".to_string())
                        .flex_1()
                        .min_w(px(0.0))
                        .text_size(px(12.0))
                        .text_color(if notice.is_some() {
                            theme::danger_text()
                        } else {
                            theme::link_color()
                        })
                        .child(SharedString::from(capture_text)),
                );
            } else {
                line = line.child(
                    div()
                        .debug_selector(move || value_selector)
                        .flex_1()
                        .min_w(px(0.0))
                        .text_size(px(12.0))
                        .text_color(theme::text_color())
                        .child(SharedString::from(row.value_text())),
                );
            }
            if row.conflicted {
                line = line.child(
                    div()
                        .debug_selector(move || conflict_selector)
                        .flex_shrink_0()
                        .px_1()
                        .py(px(1.0))
                        .rounded_sm()
                        .border_1()
                        .border_color(theme::danger_text())
                        .text_size(px(10.0))
                        .text_color(theme::danger_text())
                        .child(SharedString::from(crate::shortcuts::conflict_badge())),
                );
            }
            let edit_id = SharedString::from(edit_selector);
            let edit_debug = edit_id.clone();
            line = line.child(
                div()
                    .id(edit_id)
                    .debug_selector(move || edit_debug.to_string())
                    .flex_shrink_0()
                    .px_2()
                    .py(px(3.0))
                    .rounded_sm()
                    .border_1()
                    .border_color(theme::input_border())
                    .bg(theme::menu_bg())
                    .text_size(px(11.0))
                    .text_color(theme::text_color())
                    .cursor_pointer()
                    .hover(|style| style.bg(theme::sidebar_selected_bg()))
                    .child(SharedString::from(crate::shortcuts::modify_label()))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |root, _event: &gpui::MouseDownEvent, window, cx| {
                            root.shortcut_begin_capture(command, window, cx);
                            cx.stop_propagation();
                        }),
                    ),
            );
            let reset_id = SharedString::from(reset_selector);
            let reset_debug = reset_id.clone();
            line = line.child(
                div()
                    .id(reset_id)
                    .debug_selector(move || reset_debug.to_string())
                    .flex_shrink_0()
                    .px_2()
                    .py(px(3.0))
                    .rounded_sm()
                    .border_1()
                    .border_color(theme::input_border())
                    .bg(theme::menu_bg())
                    .text_size(px(11.0))
                    .text_color(theme::text_color())
                    .cursor_pointer()
                    .hover(|style| style.bg(theme::sidebar_selected_bg()))
                    .child(SharedString::from(crate::shortcuts::reset_label()))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |root, _event: &gpui::MouseDownEvent, _window, cx| {
                            root.shortcut_reset(command, cx);
                            cx.stop_propagation();
                        }),
                    ),
            );
            section = section.child(line);
        }
        section.into_any_element()
    }

    /// 点击「修改」进入录制态：下一次按键成为该命令的新绑定。
    ///
    /// 先把焦点交给设置面板：根视图的 **capture 相**监听器先于编辑器/输入框
    /// 拿到按键并吞掉，保证录制时按键不会同时触发原命令或落进正文。
    fn shortcut_begin_capture(
        &mut self,
        command: CommandId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.settings.is_none() {
            return;
        }
        self.shortcut_capture = Some(command);
        self.shortcut_capture_notice = None;
        self.settings_focus.focus(window, cx);
        cx.notify();
    }

    /// 取消录制（Esc / 关闭设置 / 切换分类）。
    fn shortcut_cancel_capture(&mut self, cx: &mut Context<Self>) {
        if self.shortcut_capture.take().is_none() {
            return;
        }
        self.shortcut_capture_notice = None;
        cx.notify();
    }

    /// 「恢复默认」：只恢复该命令的内置绑定并立即写盘（冲突随之消失）。
    fn shortcut_reset(&mut self, command: CommandId, cx: &mut Context<Self>) {
        if self.shortcut_capture == Some(command) {
            self.shortcut_capture = None;
        }
        self.shortcut_capture_notice = match crate::shortcuts::reset_binding(command) {
            Ok(()) => None,
            Err(error) => Some(crate::shortcuts::capture_failed(&error.to_string())),
        };
        cx.notify();
    }

    /// 录制态按键处理（返回 `true` = 本次按键已被吞掉，绝不能再传给编辑器/输入框）。
    ///
    /// 规则与 [`KeyCombo::is_bindable`] 同源：必须有 Ctrl/Cmd 或 Alt，或功能键
    /// F1–F24；裸字母/数字/Enter/Tab/Backspace 不接受。`Esc` 取消录制。
    fn handle_shortcut_capture_key(
        &mut self,
        event: &gpui::KeyDownEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(command) = self.shortcut_capture else {
            return false;
        };
        let keystroke = &event.keystroke;
        let modifiers = keystroke.modifiers;
        // 裸 Esc = 取消；带修饰键的 Esc 允许被录成快捷键（与"取消键"区分）。
        if keystroke.key.eq_ignore_ascii_case("escape")
            && !modifiers.control
            && !modifiers.alt
            && !modifiers.platform
        {
            self.shortcut_cancel_capture(cx);
            return true;
        }
        let Some(combo) = KeyCombo::from_keystroke(keystroke) else {
            self.shortcut_capture_notice = Some(crate::shortcuts::capture_invalid());
            cx.notify();
            return true;
        };
        if !combo.is_bindable() {
            self.shortcut_capture_notice = Some(crate::shortcuts::capture_invalid());
            cx.notify();
            return true;
        }
        match crate::shortcuts::assign(command, combo) {
            Ok(()) => {
                self.shortcut_capture = None;
                self.shortcut_capture_notice = None;
            }
            Err(error) => {
                // 内存里已经生效（下一次按键即用新绑定），只有写盘失败要提示。
                self.shortcut_capture = None;
                self.shortcut_capture_notice =
                    Some(crate::shortcuts::capture_failed(&error.to_string()));
            }
        }
        cx.notify();
        true
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
            .child(section_text(crate::ui_text::settings_fingerprint_title()));
        let headline = fingerprint.as_ref().map_or_else(
            crate::ui_text::settings_fingerprint_unopened,
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

    /// Welcome（Obsidian 式仓库选择页）：左栏已保存仓库列表，右栏新建/打开/同步。
    ///
    /// 右栏三张卡各自展开小表单（不整页跳走）；真正的创建/加入在确认后进入
    /// `Working`，成功仍是既有备份页 / PENDING 等待页。
    fn render_welcome(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        self.ensure_welcome_forms(cx);
        let entries = self.entries();
        let active_id = self
            .profile
            .as_ref()
            .and_then(|profile| profile.active_instance.clone());
        let local_row = self.welcome_local_row_path();
        let has_local_row = local_row.is_some();
        let startup_mode = self.startup_mode;
        let (
            expanded,
            new_local_dir,
            open_local_path,
            sync_link,
            sync_dir,
            sync_name,
            form_error,
            sync_consent,
        ) = {
            let Some(welcome) = &self.welcome else {
                return page_column()
                    .debug_selector(|| "app-root-welcome".to_string())
                    .child(title_text("Welcome 控件未就绪"))
                    .child(hint_text("内部状态不一致：请返回欢迎页重试。"));
            };
            (
                welcome.expanded,
                welcome.new_local_dir.clone(),
                welcome.open_local_path.clone(),
                welcome.sync_link.clone(),
                welcome.sync_dir.clone(),
                welcome.sync_name.clone(),
                welcome.error.clone(),
                welcome.sync_consent,
            )
        };

        // ---- 左栏：已保存的仓库（同步实例 active 优先 + 本地库）----
        let mut list = div()
            .debug_selector(|| "welcome-vault-list".to_string())
            .w_full()
            .flex()
            .flex_col()
            .gap_1();
        for (index, entry) in entries.iter().enumerate() {
            let is_active = active_id.as_deref() == Some(entry.instance_id.as_str());
            let name = entry.effective_name();
            let path = entry.vault_path.clone();
            let instance_id = entry.instance_id.clone();
            let selector = format!("welcome-vault-{index}");
            let row_id = selector.clone();
            list = list.child(
                div()
                    .id(SharedString::from(row_id))
                    .debug_selector(move || selector)
                    .w_full()
                    .min_w(px(0.0))
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    .p_2()
                    .rounded_md()
                    .border_1()
                    .border_color(if is_active {
                        theme::link_color()
                    } else {
                        theme::sidebar_border()
                    })
                    .bg(if is_active {
                        theme::sidebar_selected_bg()
                    } else {
                        theme::menu_bg()
                    })
                    .cursor_pointer()
                    .hover(|style| style.bg(theme::sidebar_selected_bg()))
                    .child(
                        div()
                            .w_full()
                            .min_w(px(0.0))
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_1()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.0))
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .text_size(px(13.0))
                                    .text_color(if is_active {
                                        theme::link_color()
                                    } else {
                                        theme::text_color()
                                    })
                                    .child(SharedString::from(name)),
                            )
                            .when(is_active, |this| {
                                this.child(
                                    div()
                                        .flex_shrink_0()
                                        .text_size(px(10.0))
                                        .text_color(theme::status_bar_text())
                                        .child("上次使用"),
                                )
                            }),
                    )
                    .child(
                        div()
                            .w_full()
                            .min_w(px(0.0))
                            .overflow_hidden()
                            .text_ellipsis()
                            .text_size(px(10.0))
                            .text_color(theme::status_bar_text())
                            .child(SharedString::from(path)),
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |root, _event: &gpui::MouseDownEvent, _window, cx| {
                            root.open_saved_instance(&instance_id, cx);
                            cx.stop_propagation();
                        }),
                    ),
            );
        }
        if let Some(path) = local_row {
            let is_active = startup_mode == Some(StartupMode::Local);
            let name = vault_directory_name(&path).unwrap_or_else(|| "本地仓库".to_owned());
            let path_text = path.display().to_string();
            list = list.child(
                div()
                    .id("welcome-vault-local")
                    .debug_selector(|| "welcome-vault-local".to_string())
                    .w_full()
                    .min_w(px(0.0))
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    .p_2()
                    .rounded_md()
                    .border_1()
                    .border_color(if is_active {
                        theme::link_color()
                    } else {
                        theme::sidebar_border()
                    })
                    .bg(if is_active {
                        theme::sidebar_selected_bg()
                    } else {
                        theme::menu_bg()
                    })
                    .cursor_pointer()
                    .hover(|style| style.bg(theme::sidebar_selected_bg()))
                    .child(
                        div()
                            .w_full()
                            .min_w(px(0.0))
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_1()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.0))
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .text_size(px(13.0))
                                    .text_color(if is_active {
                                        theme::link_color()
                                    } else {
                                        theme::text_color()
                                    })
                                    .child(SharedString::from(format!("本地仓库 · {name}"))),
                            )
                            .when(is_active, |this| {
                                this.child(
                                    div()
                                        .flex_shrink_0()
                                        .text_size(px(10.0))
                                        .text_color(theme::status_bar_text())
                                        .child("上次使用"),
                                )
                            }),
                    )
                    .child(
                        div()
                            .w_full()
                            .min_w(px(0.0))
                            .overflow_hidden()
                            .text_ellipsis()
                            .text_size(px(10.0))
                            .text_color(theme::status_bar_text())
                            .child(SharedString::from(path_text)),
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |root, _event: &gpui::MouseDownEvent, _window, cx| {
                            // 左栏点击 = 直接打开该库（不要求先「设为默认」、不经过管理页）。
                            root.open_local_only_vault_at(path.clone(), cx);
                            cx.stop_propagation();
                        }),
                    ),
            );
        }
        if entries.is_empty() && !has_local_row {
            list =
                list.child(hint_text("还没有仓库：从右侧新建本地仓库，或同步远程仓库。").w_full());
        }
        let left = div()
            .id("welcome-vault-scroll")
            .w(px(280.0))
            .flex_shrink_0()
            .h_full()
            .min_h(px(0.0))
            .overflow_y_scroll()
            .p(px(18.0))
            .border_r_1()
            .border_color(theme::sidebar_border())
            .bg(theme::sidebar_bg())
            .flex()
            .flex_col()
            .gap_2()
            .child(section_text("已保存的仓库"))
            .child(list);

        // ---- 右栏：大标题 + 三行操作卡 ----
        let mut right = div()
            .id("welcome-content-scroll")
            .flex_1()
            .min_w(px(0.0))
            .h_full()
            .min_h(px(0.0))
            .overflow_y_scroll()
            .p(px(28.0))
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .text_size(px(26.0))
                    .text_color(theme::text_color())
                    .child("menghunote"),
            )
            .child(section_text("仓库"))
            .when_some(banner_text(), |this, banner| this.child(banner));

        // 1) 新建本地仓库
        right = right.child(welcome_action_card(
            "welcome-new-local",
            "新建本地仓库",
            "选择一个目录，在其中创建空的 vault.sqlite3（不播种演示内容）",
            expanded == Some(WelcomeAction::NewLocal),
            WelcomeAction::NewLocal,
            cx,
        ));
        if expanded == Some(WelcomeAction::NewLocal) {
            right = right.child(
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p(px(12.0))
                    .rounded_md()
                    .bg(theme::input_bg())
                    .border_1()
                    .border_color(theme::input_border())
                    .child(label_text(
                        "目录（绝对路径；将在其中创建 vault.sqlite3；已有库请改用「打开本地仓库」）",
                    ))
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.0))
                                    .px_2()
                                    .py_1()
                                    .bg(theme::window_bg())
                                    .border_1()
                                    .border_color(theme::input_border())
                                    .rounded_sm()
                                    .child(new_local_dir),
                            )
                            .child(action_button(
                                "welcome-new-local-browse",
                                "浏览…",
                                false,
                                false,
                                cx,
                                |root, cx| root.welcome_browse_new_local(cx),
                            )),
                    )
                    .child(
                        div().flex().flex_row().gap_2().child(action_button(
                            "welcome-new-local-confirm",
                            "创建并打开",
                            true,
                            false,
                            cx,
                            |root, cx| root.welcome_confirm_new_local(cx),
                        )),
                    ),
            );
        }

        // 2) 打开本地仓库
        right = right.child(welcome_action_card(
            "welcome-open-local",
            "打开本地仓库",
            "打开已有的 vault.sqlite3；缺失时明确报错，不会新建",
            expanded == Some(WelcomeAction::OpenLocal),
            WelcomeAction::OpenLocal,
            cx,
        ));
        if expanded == Some(WelcomeAction::OpenLocal) {
            right = right.child(
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p(px(12.0))
                    .rounded_md()
                    .bg(theme::input_bg())
                    .border_1()
                    .border_color(theme::input_border())
                    .child(label_text(
                        "库文件（vault.sqlite3；也可以选目录，提交时自动补 vault.sqlite3）",
                    ))
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.0))
                                    .px_2()
                                    .py_1()
                                    .bg(theme::window_bg())
                                    .border_1()
                                    .border_color(theme::input_border())
                                    .rounded_sm()
                                    .child(open_local_path),
                            )
                            .child(action_button(
                                "welcome-open-local-browse",
                                "浏览…",
                                false,
                                false,
                                cx,
                                |root, cx| root.welcome_browse_open_local(cx),
                            )),
                    )
                    .child(div().flex().flex_row().gap_2().child(action_button(
                        "welcome-open-local-confirm",
                        "打开",
                        true,
                        false,
                        cx,
                        |root, cx| root.welcome_confirm_open_local(cx),
                    ))),
            );
        }

        // 3) 同步远程仓库（只收一条链接；自动识别 create/join）
        right = right.child(welcome_action_card(
            "welcome-sync-remote",
            "同步远程仓库",
            "粘贴 mn-pair://create 或 mn-pair://join 链接；自动识别创建/加入，不用手填 relay/vault/恢复码",
            expanded == Some(WelcomeAction::SyncRemote),
            WelcomeAction::SyncRemote,
            cx,
        ));
        if expanded == Some(WelcomeAction::SyncRemote) {
            let mut form = div()
                .w_full()
                .flex()
                .flex_col()
                .gap_2()
                .p(px(12.0))
                .rounded_md()
                .bg(theme::input_bg())
                .border_1()
                .border_color(theme::input_border())
                .child(label_text(
                    "链接（只粘贴一条；创建票据 mn-pair://create 或邀请链接 mn-pair://join）",
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
                        .child(sync_link),
                )
                .child(hint_text(
                    "链接严格解析：不是 mn-pair://create / mn-pair://join 的形式、票据/令牌不合法时会在本页内联报错，不联网、不建文件。",
                ))
                .child(label_text(
                    "实例目录（绝对路径；默认在用户文档目录，可修改/浏览；目录内固定 vault.sqlite3）",
                ))
                .child(
                    div()
                        .w_full()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.0))
                                .px_2()
                                .py_1()
                                .bg(theme::window_bg())
                                .border_1()
                                .border_color(theme::input_border())
                                .rounded_sm()
                                .child(sync_dir),
                        )
                        .child(action_button(
                            "welcome-sync-dir-browse",
                            "浏览…",
                            false,
                            false,
                            cx,
                            |root, cx| root.welcome_browse_sync_dir(cx),
                        )),
                )
                // 可选「仓库名称」：只影响左下角/菜单/管理仓库的显示，不改路径。
                .child(label_text(format!(
                    "{}（可选；只影响显示，不改变实例目录）",
                    crate::ui_text::vault_name_label()
                )))
                .child(
                    div()
                        .w_full()
                        .px_2()
                        .py_1()
                        .bg(theme::window_bg())
                        .border_1()
                        .border_color(theme::input_border())
                        .rounded_sm()
                        .child(sync_name),
                )
                .child(hint_text(crate::ui_text::vault_name_placeholder()));
            #[cfg(feature = "dev-sync")]
            {
                form = form.child(checkbox_row(
                    "welcome-sync-consent",
                    sync_consent,
                    "我理解这是开发同步（DEV ONLY / localhost / 非生产），并同意继续",
                    cx,
                    |root, cx| {
                        let next = !root.welcome_sync_consent();
                        root.welcome_set_sync_consent(next, cx);
                    },
                ));
            }
            #[cfg(not(feature = "dev-sync"))]
            let _ = sync_consent;
            form = form.child(div().flex().flex_row().gap_2().child(action_button(
                "welcome-sync-confirm",
                "继续",
                true,
                false,
                cx,
                |root, cx| root.welcome_confirm_sync(cx),
            )));
            right = right.child(form);
        }

        if let Some(error) = form_error {
            right = right.child(
                div()
                    .debug_selector(|| "welcome-error".to_string())
                    .w_full()
                    .text_size(px(11.0))
                    .text_color(theme::danger_text())
                    .child(SharedString::from(error)),
            );
        }
        if let Some(notice) = self.notice.clone() {
            right = right.child(
                div()
                    .debug_selector(|| "app-root-notice".to_string())
                    .w_full()
                    .text_size(px(11.0))
                    .text_color(theme::caret_color())
                    .child(SharedString::from(notice)),
            );
        }

        div()
            .debug_selector(|| "app-root-welcome".to_string())
            .w_full()
            .flex_1()
            .min_h(px(0.0))
            .flex()
            .flex_row()
            .child(left)
            .child(right)
    }

    fn render_create_form(&self, cx: &mut Context<Self>) -> gpui::Div {
        let Some(form) = self.form.as_ref() else {
            return page_column()
                .debug_selector(|| "app-root-create-form".to_string())
                .child(title_text("没有待填写的创建表单"))
                .child(hint_text("内部状态不一致：请返回欢迎页重试。"));
        };
        // 「同意开发同步」只在 dev-sync 构建渲染/校验；产品构建没有这个勾选。
        #[cfg(feature = "dev-sync")]
        let consent = form.consent;
        #[cfg(feature = "dev-sync")]
        let mode = form.mode;
        let link = form.link.clone();
        let dir = form.dir.clone();
        let form_error = form.error.clone();
        // 创建链接已应用：票据在内存，页面只展示公开事实。
        let link_facts = form
            .link_state
            .as_ref()
            .map(|link| (link.relay.clone(), link.vault_id_hex.clone()));
        let ticket_mode = link_facts.is_some();
        let title = "用创建链接创建同步实例（v2 一次性票据）";
        let mut content = content_column()
            .child(section_text(
                "创建链接（粘贴 mn-pair://create?…&vault=…#ticket=… 后点「应用创建链接」）",
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
                            "移除链接",
                            false,
                            false,
                            cx,
                            |root, cx| root.form_clear_create_link(cx),
                        ))
                    }),
            );
        if let Some((relay, vault_id)) = link_facts {
            content = content
                .child(
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
                );
            // DEV 状态标注只在 dev-sync 构建出现；产品构建不渲染任何 DEV 文案。
            #[cfg(feature = "dev-sync")]
            if mode == RelayMode::RemoteTls {
                content = content.child(
                    div()
                        .debug_selector(|| "app-root-remote-dev-notice".to_string())
                        .w_full()
                        .text_size(px(11.0))
                        .text_color(theme::caret_color())
                        .child(SharedString::from(
                            "DEV 状态：remote_tls 初始化已接线（production TLS + 受限初始化，\
                             未在真实 relay / Windows 上验证）。不会忽略证书，也不提供私有 \
                             CA 导入（请自行安装系统根）。",
                        )),
                );
            }
        } else {
            content = content.child(hint_text(
                "尚未应用创建链接：创建授权只来自链接 fragment 里的一次性票据；App 不接收\
                 管理凭证/setup secret，也不需要填 vault_id/TTL。",
            ));
        }

        content = content
            .child(label_text(
                "实例目录（绝对路径；默认在用户文档目录，可修改/浏览；目录内固定 vault.sqlite3 / dev-identity.bin）",
            ))
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .px_2()
                            .py_1()
                            .bg(theme::window_bg())
                            .border_1()
                            .border_color(theme::input_border())
                            .rounded_sm()
                            .child(dir),
                    )
                    .child(action_button(
                        "app-root-dir-browse",
                        "浏览…",
                        false,
                        false,
                        cx,
                        |root, cx| root.form_browse_dir(cx),
                    )),
            );
        // 「同意开发同步」只在 dev-sync 构建出现；产品构建没有 DEV 勾选，提交直接继续。
        #[cfg(feature = "dev-sync")]
        {
            content = content.child(checkbox_row(
                "app-root-consent",
                consent,
                "我理解这是开发同步（DEV ONLY / localhost / 非生产），并同意创建",
                cx,
                |root, cx| {
                    let next = root.form.as_ref().is_some_and(|form| !form.consent);
                    root.form_set_consent(next, cx);
                },
            ));
        }
        content = content
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
                        "用一次性票据创建",
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
            );
        let mut column = page_column()
            .debug_selector(|| "app-root-create-form".to_string())
            .child(title_text(title))
            .when_some(banner_text(), |this, banner| this.child(banner))
            .child(content);
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
        // 模式不再手选：提交时按 relay 地址推断；这里只展示只读约束。
        let relay_label =
            "relay 地址（按地址自动识别：https → remote_tls；http loopback → local_dev，仅 dev-sync）";
        let column = page_column()
            .debug_selector(|| "app-root-join-form".to_string())
            .child(title_text("加入已有同步实例（Join）"))
            .when_some(banner_text(), |this, banner| this.child(banner))
            .child(
                content_column()
                    .when_some(join_dev_notice(), |this, notice| this.child(notice))
                    .child(
                        div()
                            .w_full()
                            .text_size(px(12.0))
                            .text_color(theme::caret_color())
                            .child(SharedString::from(
                                "本机没有这个设备身份：这是「加入」，不是登录——会用恢复码/邀请\
                                 自批一个新的普通设备身份（没有管理私钥），不会覆盖创建者或其它设备。",
                            )),
                    )
                    .child(label_text("① 粘贴邀请链接（推荐；创建者「复制邀请链接」得到）"))
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
                         链接含一次性令牌，兑换后输入框立即清空、不落盘、不进日志。\
                         粘贴邀请后只需要再确认实例目录，relay/vault_id/恢复码可留空。",
                    ))
                    .child(hint_text("—— 或者手动输入以下信息（恢复码加入）——"))
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
                    .child(
                        hint_text(
                            "remote_tls 只接受 HTTPS canonical origin；local_dev 只允许明文 loopback（仅 dev-sync）。\
                             非法地址内联报错，不联网。",
                        )
                        .debug_selector(|| "app-root-join-relay-hint".to_string())
                        .w_full(),
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
                        "实例目录（绝对路径；默认在用户文档目录，可修改/浏览；必须尚不存在或空目录；目录内固定 vault.sqlite3 / dev-identity.bin）",
                    ))
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.0))
                                    .px_2()
                                    .py_1()
                                    .bg(theme::window_bg())
                                    .border_1()
                                    .border_color(theme::input_border())
                                    .rounded_sm()
                                    .child(dir),
                            )
                            .child(action_button(
                                "app-root-join-dir-browse",
                                "浏览…",
                                false,
                                false,
                                cx,
                                |root, cx| root.join_form_browse_dir(cx),
                            )),
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
            .when_some(banner_text(), |this, banner| this.child(banner))
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
            .when_some(banner_text(), |this, banner| this.child(banner))
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
        let baseline_published = pending.outcome.baseline_published;
        let warning = pending.outcome.warning.clone();
        let invite_copied = pending.invite.is_some();
        let invite_busy = self.invite_copy_job.is_some();
        let invite_ttl = self.backup_invite_ttl_text();
        let invite_error = self.backup_invite_error().map(str::to_owned);
        let column = page_column()
            .debug_selector(|| "app-root-backup".to_string())
            .child(title_text("备份恢复码（只显示这一次）"))
            .when_some(banner_text(), |this, banner| this.child(banner))
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
                            .debug_selector(|| "app-root-baseline-status".to_string())
                            .text_size(px(11.0))
                            .text_color(if baseline_published {
                                theme::caret_color()
                            } else {
                                theme::danger_text()
                            })
                            .child(SharedString::from(if baseline_published {
                                "初始基线已自动发布：可直接复制邀请链接给其他设备（加入端不再缺基线）。"
                            } else {
                                "基线未发布（创建时自动发布失败）：实例/恢复码已保留。请先点\
                                 「继续进入主界面」，新建或打开一篇笔记后在同步面板点\
                                 「重试发布基线」；发布成功前邀请链接不会生成（本地前置检查会挡住）。"
                            })),
                    )
                    .when_some(warning, |this, warning| {
                        this.child(
                            div()
                                .debug_selector(|| "app-root-create-warning".to_string())
                                .text_size(px(11.0))
                                .text_color(theme::status_bar_text())
                                .child(SharedString::from(format!("创建告警：{warning}"))),
                        )
                    })
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
                    .child(section_text(
                        "邀请其他设备（初始基线在 Create 内自动发布；不要求先发布笔记）",
                    ))
                    .child(hint_text(
                        "「复制邀请链接」生成 v2 一次性邀请（默认 15 分钟），其他设备粘贴链接后是 \
                         PENDING，需在本机设备面板批准。链接含 token：只在剪贴板里，UI 不显示明文；\
                         只私发，不要截图/公开发布。",
                    ))
                    .child(action_button(
                        "app-root-copy-invite",
                        if invite_busy {
                            "正在生成邀请链接…"
                        } else if invite_copied {
                            "重新生成并复制邀请链接"
                        } else {
                            "复制邀请链接给其他设备"
                        },
                        false,
                        invite_busy,
                        cx,
                        |root, cx| root.copy_invite(cx),
                    ))
                    .when(invite_copied, |this| {
                        this.child(
                            div()
                                .debug_selector(|| "app-root-invite-copied".to_string())
                                .text_size(px(11.0))
                                .text_color(theme::caret_color())
                                .child(SharedString::from(
                                    "已复制邀请链接：token 只在剪贴板，本页不显示明文；\
                                     再次点击会生成并复制一张新链接（旧的作废）。",
                                )),
                        )
                    })
                    .when_some(invite_ttl, |this, text| {
                        this.child(
                            div()
                                .debug_selector(|| "app-root-invite-ttl".to_string())
                                .text_size(px(11.0))
                                .text_color(theme::caret_color())
                                .child(SharedString::from(text)),
                        )
                    })
                    .when_some(invite_error, |this, text| {
                        this.child(
                            div()
                                .debug_selector(|| "app-root-invite-error".to_string())
                                .text_size(px(11.0))
                                .text_color(theme::danger_text())
                                .child(SharedString::from(text)),
                        )
                    })
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
            .when_some(banner_text(), |this, banner| this.child(banner))
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

    /// 分类分组契约：固定分类在前、插件分类在后；两组拼起来就是 `ALL`。
    #[test]
    fn settings_sections_split_into_fixed_and_plugin_groups() {
        assert_eq!(
            SettingsSection::FIXED.len() + SettingsSection::PLUGIN_SECTIONS.len(),
            SettingsSection::ALL.len()
        );
        assert_eq!(
            &SettingsSection::ALL[..SettingsSection::FIXED.len()],
            &SettingsSection::FIXED[..]
        );
        assert_eq!(
            &SettingsSection::ALL[SettingsSection::FIXED.len()..],
            &SettingsSection::PLUGIN_SECTIONS[..]
        );
        assert!(SettingsSection::FIXED
            .iter()
            .all(|section| section.is_fixed() && !section.is_plugin()));
        assert!(SettingsSection::PLUGIN_SECTIONS
            .iter()
            .all(|section| section.is_plugin() && !section.is_fixed()));
        // 默认分类是「外观」（固定组第一项）。
        assert_eq!(SettingsSection::default(), SettingsSection::Appearance);
        assert_eq!(SettingsSection::default(), SettingsSection::FIXED[0]);
    }

    /// 分类短名 → 枚举往返；未知 key 明确返回 `None`（调用方回退「外观」）。
    #[test]
    fn settings_section_keys_round_trip_and_unknown_key_is_rejected() {
        for section in SettingsSection::ALL {
            assert_eq!(SettingsSection::parse(section.key()), Some(section));
        }
        // `instances` 是**已移除**分类的旧 key：必须明确不可达（回退「外观」）。
        for unknown in ["", "nope", "Appearance", "templates ", "0", "instances"] {
            assert_eq!(SettingsSection::parse(unknown), None, "{unknown:?}");
        }
        // 短名唯一（导航 selector / 偏好文件都靠它）。
        let mut seen = std::collections::HashSet::new();
        for section in SettingsSection::ALL {
            assert!(
                seen.insert(section.key()),
                "分类短名重复：{}",
                section.key()
            );
        }
    }

    /// 设置分类偏好：原子写 → 读回；损坏/未知 key/未知版本 → 外观；最小字段也接受。
    #[test]
    fn settings_section_preference_file_round_trips_and_corruption_falls_back() {
        let dir = temp_dir("settings-ui-pref");
        let path = dir.join(crate::onboarding::layout::SETTINGS_UI_FILE_NAME);
        assert_eq!(
            load_settings_section(&path),
            SettingsSection::Appearance,
            "文件不存在 = 外观"
        );

        save_settings_section(&path, SettingsSection::DailyNotes);
        assert_eq!(load_settings_section(&path), SettingsSection::DailyNotes);
        let raw = std::fs::read_to_string(&path).expect("读回失败");
        let value: serde_json::Value = serde_json::from_str(&raw).expect("必须是 JSON");
        assert_eq!(value["version"], serde_json::json!(1));
        assert_eq!(value["section"], "daily_notes");
        assert!(
            !path.with_extension("json.tmp").exists(),
            "原子写完成后不能留下临时文件"
        );

        for corrupt in [
            "",
            "{ broken",
            "{}",
            "{\"section\":\"nope\"}",
            "{\"version\":99,\"section\":\"language\"}",
            "{\"version\":1}",
            "{\"version\":1,\"section\":2}",
            // 旧版本可能记住已移除的「实例」分类：回退「外观」，不崩/不空白。
            "{\"version\":1,\"section\":\"instances\"}",
        ] {
            std::fs::write(&path, corrupt).expect("写坏文件失败");
            assert_eq!(
                load_settings_section(&path),
                SettingsSection::Appearance,
                "损坏/未知输入必须回退外观：{corrupt:?}"
            );
        }
        // 字段最小化写法（没有 version）也接受。
        std::fs::write(&path, "{\"section\":\"language\"}").expect("写最小 JSON 失败");
        assert_eq!(load_settings_section(&path), SettingsSection::Language);
    }
}
