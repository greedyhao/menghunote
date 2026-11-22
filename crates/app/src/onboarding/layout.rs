//! 同步实例的**公开数据目录**与**公开配置**（ADR 0011「数据目录与公开配置」）。
//!
//! ## 为什么单独一层
//!
//! 向导要回答两个问题：实例文件放哪、上次用了哪个实例。两件事都只涉及**公开定位
//! 信息**（路径、relay、`vault_id`、`device_id`），与秘密材料（setup secret、恢复码、
//! root/私钥）严格分开：
//!
//! - 本文件里的结构体**没有**任何秘密字段，序列化后就是可以给人看的 JSON；
//! - 恢复码只在 Create 成功后的备份页内存里存在（`onboarding::create` 的 `Zeroizing`），
//!   从 `serialize` 的字段集合里根本不存在——这不是"记得别写"，而是类型上写不进去。
//!
//! ## 默认位置
//!
//! **vault（库/实例目录）默认放在用户可见的文档目录**，不再默认写
//! `%LOCALAPPDATA%`；用户可以在 Create/Join 表单里改，选择会写进公开配置的
//! `vault_path`（旧配置里的路径原样保留、不迁移）。
//!
//! | 平台 | vault 默认根 | 公开配置 |
//! | --- | --- | --- |
//! | Windows | `%USERPROFILE%\Documents\menghunote\`（Documents 不存在则 `%USERPROFILE%\menghunote\`） | `%LOCALAPPDATA%\menghunote\sync.json` |
//! | Linux/macOS | `$HOME/Documents/menghunote/`（Documents 不存在则 `$HOME/menghunote/`） | `$XDG_CONFIG_HOME/menghunote/sync.json` |
//!
//! - 实例目录默认 `<vault 根>/instances/<id>/`，本地库默认
//!   `<vault 根>/local/vault.sqlite3`；
//! - 配置 `sync.json` 仍然很小、只含公开定位信息，留在平台配置目录（LOCALAPPDATA）；
//! - 环境变量缺失时配置目录分别回退 `~/.local/share` 与 `~/.config`；**HOME 也没有
//!   就报错**，绝不回退 cwd/tmp（真实使用把库留在临时目录是数据丢失）。
//! - 测试用 [`ProfileLayout::new`] 显式注入 `target/tmp` 下的路径：显式布局**不读
//!   平台环境**，默认 vault 根就是数据根（既有测试/第三方注入语义不变）。
//!
//! 实例目录内的文件名本轮**不改名**：库仍是 `vault.sqlite3`、身份仍是
//! `dev-identity.bin`（动它们就会碰用户数据/身份，另行规划）。
//!
//! ## 旧开发目录的一次性保守迁移（P2）
//!
//! 产品目录从 `menghunote-dev` 改名成 `menghunote`、配置从 `dev-sync.json` 改名成
//! `sync.json`。默认布局解析后，`AppRoot` 启动引导会调用
//! [`ProfileLayout::migrate_legacy_profile`] 做一次性保守迁移（失败在 GUI 错误页提示）：
//!
//! - 新 `sync.json` **不存在**、旧 `menghunote-dev/dev-sync.json` 存在时，把旧配置里的
//!   **同一份公开实例列表**写到新 `sync.json`（v1 只在内存升到 v2，旧文件不回写）；
//! - `vault_path` 原样保留：**不移动/不复制** `instances` 目录（避免大文件与身份文件风险）；
//! - 旧目录/旧配置**原样保留**（不删、不改写）；
//! - 新配置已存在 → 一律以新配置为准，不再看旧配置（重复启动幂等，不改新文件）；
//! - 旧配置损坏/版本不支持/模式不被本次构建允许 → 按现有错误语义报错，不重建、不覆盖。
//!
//! ## 配置结构
//!
//! ```json
//! { "version": 2, "active_instance": "inst-...",
//!   "instances": [{ "instance_id": "...", "relay_mode": "local_dev", "relay_url": "...",
//!                   "vault_path": "...", "vault_id": "...", "device_id": "...",
//!                   "display_name": "我的笔记库" }] }
//! ```
//!
//! v1（没有 `relay_mode`）只在 [`ProfileStore::load`] 里**显式迁移**为 `local_dev`
//! 并写回 v2；未知版本/模式一律明确报错（ADR 0013 §1）。
//!
//! ## 用户自定义仓库名称（`display_name`）
//!
//! `display_name` 是**可选显示名**（[`InstanceEntry::display_name`]），只影响界面文案：
//!
//! - **不是**路径，也**不参与**实例目录/vault 文件名/身份文件的任何计算——改名不移动、
//!   不复制、不重建库（`vault_path`/`vault_id`/`device_id` 原样保留）；
//! - 字段带 `#[serde(default)]`：v2 旧配置（没有该字段）照常可读，读成 `None`；
//!   配置 `version` **仍是 2**（只增可选字段，旧版本程序忽略未知字段也能读）；
//! - 校验（[`normalize_display_name`]）：trim 后 1–64 个字符、禁止控制字符；
//!   空白/空串一律归一为 `None`（显示层回退目录名）；中文/空格/emoji 都允许；
//! - 显示顺序（[`InstanceEntry::effective_name`]）：自定义名 → 库文件所在目录名 →
//!   [`super::settings::short_public_id`]（**不是**显示 `inst-...` 全串）。
//!
//! ## 原子写
//!
//! 临时文件（同目录、`create_new`）→ write → `sync_all` → `rename` → 父目录 fsync。
//! 失败清掉临时文件、**不动**旧配置；调用方看到错误后提示「重试保存」，绝不因此重跑
//! Create/Join（ADR 0011「写失败提示重试保存，不重复 Create/Join」）。

use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::sync_gate::parse_for_build;

/// 单一权威的 relay API 面类型（ADR 0014 S3a；配置里只存数字 1/2）。
pub use menghunote_sync::api_version::RelayApiVersion;
/// 单一权威的 relay 模式类型（定义在 `menghunote-sync`；这里重新导出给 UI/配置使用）。
pub use menghunote_sync::relay_policy::RelayMode;

/// 公开配置的当前版本（ADR 0013 §1：每实例显式 `relay_mode` + `relay_url`）。
pub const PROFILE_VERSION: u32 = 2;
/// v1 旧配置（没有 `relay_mode`）：只允许**显式迁移**为 `local_dev` 并写回 v2。
pub const PROFILE_VERSION_V1: u32 = 1;

/// 实例目录里固定的库文件名（P2 不改名：避免动身份/用户数据）。
pub const VAULT_FILE_NAME: &str = "vault.sqlite3";

/// 本地（不同步）库的子目录名（与 `instances/` 同级，在产品数据根下）。
///
/// 产品路径（A 组第二切片）：
/// - Windows `%LOCALAPPDATA%\menghunote\local\vault.sqlite3`
/// - Linux `$XDG_DATA_HOME/menghunote/local/vault.sqlite3`
pub const LOCAL_DIR_NAME: &str = "local";

/// 配置文件名（产品；P2 从旧的 `dev-sync.json` 改为 `sync.json`）。
pub const PROFILE_FILE_NAME: &str = "sync.json";

/// 本机启动偏好文件名（数据根下；与 `sync.json` 分开）。
///
/// 只记"上次成功打开的是本地（不同步）库还是同步实例"，是**本机偏好**：
/// 不进 vault、不参与同步；缺失/损坏按 Welcome 处理（不报错、不阻塞启动）。
pub const STARTUP_FILE_NAME: &str = "startup.json";

/// 本机启动偏好当前版本（写入时使用；读取时 `version` 字段可缺省）。
pub const STARTUP_PREFERENCES_VERSION: u32 = 1;

/// 设置页分类记忆文件名（数据根下；本机 UI 偏好，不同步、不写正文）。
///
/// 只记上次打开设置页时停留的分类短名（与 `SettingsSection::key()` 同一套值）；
/// 缺失/损坏/未知 key 一律回退「外观」。
pub const SETTINGS_UI_FILE_NAME: &str = "settings-ui.json";

/// 快捷键偏好文件名（数据根下；本机 UI 偏好）。
///
/// 与 [`crate::shortcuts`] 的 `SHORTCUTS_FILE_NAME` 同一份约定；这里显式保留
/// 路径方法，保证测试注入的显式布局把偏好写进临时数据根而不是用户目录。
/// 不进 vault、不参与同步、不同步。
pub const SHORTCUTS_FILE_NAME: &str = "shortcuts.json";

/// 默认实例目录（`instances/`）。
const INSTANCES_DIR_NAME: &str = "instances";

/// 产品级目录名（与 relay/服务无关，只是本机目录约定）。
const APP_DIR_NAME: &str = "menghunote";

/// 旧（开发期）产品目录名：**只**用于一次性保守迁移，不删除、不改写。
pub const LEGACY_APP_DIR_NAME: &str = "menghunote-dev";

/// 旧（开发期）配置文件名：**只**用于一次性保守迁移，不删除、不改写。
pub const LEGACY_PROFILE_FILE_NAME: &str = "dev-sync.json";

/// 路径解析/配置错误。
///
/// 文案只包含**路径与公开字段**：绝不带 secret/恢复码/root。
#[derive(Debug)]
pub enum ProfileError {
    /// 平台目录环境变量缺失（HOME/LOCALAPPDATA 都没有）。
    NoHome(&'static str),
    /// 配置 JSON 损坏或字段非法；**不自动重建**。
    Corrupt { path: PathBuf, detail: String },
    /// 配置版本不支持。
    UnsupportedVersion { path: PathBuf, version: u32 },
    /// 配置里的 `active_instance` 找不到对应条目。
    UnknownActive { instance_id: String },
    /// 实例 id 非法（含路径分隔符/`.`/`..`，防止配置把路径带出 instances 目录）。
    InvalidInstanceId(String),
    /// 实例的 relay 模式/URL 不可用（未知模式、与构建能力不符、非 canonical origin）。
    InvalidRelay {
        /// 出问题的实例 id。
        instance_id: String,
        /// 原因（已脱敏；不含 userinfo/凭据）。
        detail: String,
    },
    /// 用户自定义仓库名称非法（trim 后 1–64 字符、不含控制字符）。
    InvalidDisplayName {
        /// 原因（只描述规则，不回显整段非法输入）。
        detail: String,
    },
    /// 配置里没有这个实例（显式操作只针对已登记实例，**不自动新增**）。
    UnknownInstance {
        /// 找不到的实例 id。
        instance_id: String,
    },
    /// IO 失败（读写配置/建目录）。
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl std::fmt::Display for ProfileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoHome(var) => write!(
                f,
                "无法确定用户目录（{var} 未设置）；本程序不回退当前目录/临时目录，\
                 请设置 HOME（Windows 为 LOCALAPPDATA）后重试"
            ),
            Self::Corrupt { path, detail } => write!(
                f,
                "同步配置损坏：{}（{detail}）；按 ADR 0011 不自动重建、不打开演示库，\
                 请人工检查或先备份后移除该文件",
                path.display()
            ),
            Self::UnsupportedVersion { path, version } => write!(
                f,
                "同步配置版本不支持：{} 是 v{version}，本程序只支持 v{PROFILE_VERSION}；\
                 不迁移、不覆盖",
                path.display()
            ),
            Self::UnknownActive { instance_id } => write!(
                f,
                "同步配置的 active_instance 指向不存在的实例：{instance_id}；\
                 配置索引损坏，请人工检查（不会自动改配置）"
            ),
            Self::InvalidInstanceId(id) => {
                write!(f, "实例 id 非法：{id:?}（只允许字母/数字/`-`/`_`）")
            }
            Self::InvalidRelay {
                instance_id,
                detail,
            } => write!(
                f,
                "实例 {instance_id} 的 relay 配置无效：{detail}；不自动改写配置，请人工修正"
            ),
            Self::InvalidDisplayName { detail } => {
                write!(f, "仓库名称无效：{detail}")
            }
            Self::UnknownInstance { instance_id } => write!(
                f,
                "同步配置里没有该实例：{instance_id}；这是索引操作，不自动新增条目"
            ),
            Self::Io { path, source } => {
                write!(f, "同步配置 IO 失败：{}（{source}）", path.display())
            }
        }
    }
}

impl std::error::Error for ProfileError {}

/// 本机目录布局（公开信息）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileLayout {
    /// `sync.json` 的完整路径。
    pub config_path: PathBuf,
    /// 默认实例目录的上一级（`instances/`）。
    pub instances_dir: PathBuf,
    /// 旧布局的公开配置路径（`menghunote-dev/dev-sync.json`）。
    ///
    /// 只有按平台默认解析（[`Self::discover`]）才会填；显式 [`Self::new`] 恒为
    /// `None`（测试/兼容注入的显式路径**不触发**任何迁移）。它只用于一次性迁移判定，
    /// 不是当前配置。
    pub legacy_config_path: Option<PathBuf>,
    /// 用户可见的默认 vault 根（Create/Join/本地入口的**初始值**）。
    ///
    /// - [`Self::discover`]：`<Documents>/menghunote`（Documents 不存在则
    ///   `<HOME>/menghunote`）；
    /// - [`Self::new`]：`None`——显式注入布局没有平台环境可读，默认根退化为
    ///   [`Self::data_root`]（测试/第三方注入语义，既有路径不变）。
    ///
    /// 它只是**默认值**：真正写入配置的是用户在表单里确认的绝对路径。
    default_vault_root: Option<PathBuf>,
}

impl ProfileLayout {
    /// 显式指定配置路径与实例目录（测试注入；生产走 [`Self::discover`]）。
    ///
    /// 不参与旧目录迁移：显式路径的调用方自己决定读哪个文件。
    #[must_use]
    pub fn new(config_path: impl Into<PathBuf>, instances_dir: impl Into<PathBuf>) -> Self {
        Self {
            config_path: config_path.into(),
            instances_dir: instances_dir.into(),
            legacy_config_path: None,
            default_vault_root: None,
        }
    }

    /// 按平台约定解析默认目录（env 读取闭包便于确定性测试）。
    ///
    /// 缺失 HOME/LOCALAPPDATA 是**错误**：真实使用不允许回退 cwd/tmp。
    ///
    /// 只做**路径解析**；旧目录迁移是显式的 [`Self::migrate_legacy_profile`]
    /// （启动引导 `AppRoot::new` 调用它，这样失败能在 GUI 错误页提示；测试显式布局
    /// 不受影响）。
    pub fn discover(env: impl Fn(&str) -> Option<String>) -> Result<Self, ProfileError> {
        let env = |key: &str| {
            env(key)
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        };
        // 用户可见的默认 vault 根（与配置目录分开：配置仍留平台目录）。
        // `env` 是 `Fn` 闭包（Copy）：按值传一份，下面的 cfg 分支仍能继续用同一份。
        let default_vault_root = user_default_vault_root(env);
        #[cfg(windows)]
        {
            let local = env("LOCALAPPDATA")
                .or_else(|| {
                    env("USERPROFILE").map(|home| {
                        PathBuf::from(home)
                            .join("AppData")
                            .join("Local")
                            .into_os_string()
                            .to_string_lossy()
                            .into_owned()
                    })
                })
                .ok_or(ProfileError::NoHome("LOCALAPPDATA"))?;
            let root = PathBuf::from(local);
            Ok(Self {
                config_path: root.join(APP_DIR_NAME).join(PROFILE_FILE_NAME),
                instances_dir: root.join(APP_DIR_NAME).join(INSTANCES_DIR_NAME),
                legacy_config_path: Some(
                    root.join(LEGACY_APP_DIR_NAME)
                        .join(LEGACY_PROFILE_FILE_NAME),
                ),
                default_vault_root,
            })
        }
        #[cfg(not(windows))]
        {
            let home = env("HOME").ok_or(ProfileError::NoHome("HOME"))?;
            let home = PathBuf::from(home);
            let data_home = env("XDG_DATA_HOME")
                .map_or_else(|| home.join(".local").join("share"), PathBuf::from);
            let config_home =
                env("XDG_CONFIG_HOME").map_or_else(|| home.join(".config"), PathBuf::from);
            Ok(Self {
                config_path: config_home.join(APP_DIR_NAME).join(PROFILE_FILE_NAME),
                instances_dir: data_home.join(APP_DIR_NAME).join(INSTANCES_DIR_NAME),
                legacy_config_path: Some(
                    config_home
                        .join(LEGACY_APP_DIR_NAME)
                        .join(LEGACY_PROFILE_FILE_NAME),
                ),
                default_vault_root,
            })
        }
    }

    /// 一次性保守迁移（P2）：把旧开发目录的**公开配置**复制到新配置。
    ///
    /// 只在默认布局（[`Self::discover`]）上有旧路径；显式 [`Self::new`] 恒返回
    /// `Ok(false)`。规则与顺序（任何一条不满足都**不写**）：
    ///
    /// 1. 新配置已存在 → 以新配置为准，返回 `Ok(false)`（幂等：重复启动不改新文件）；
    /// 2. 旧配置不存在 → `Ok(false)`；
    /// 3. 旧配置只做**只读解析**（v1 在内存升到 v2，旧文件不回写），用同一份公开实例
    ///    列表原子写新 `sync.json`；`vault_path` 原样保留，**不移动/不复制**库与身份；
    /// 4. 旧配置损坏/版本不支持/模式不被本构建允许 → 原样返回错误，不重建、不覆盖。
    ///
    /// 返回是否真的写了新配置。
    pub fn migrate_legacy_profile(&self) -> Result<bool, ProfileError> {
        let Some(legacy_path) = self.legacy_config_path.as_ref() else {
            return Ok(false);
        };
        // 新配置优先：只要它存在，旧配置**完全不读**（幂等的唯一判据）。
        if self.config_path.exists() || !legacy_path.exists() {
            return Ok(false);
        }
        let legacy = ProfileStore {
            path: legacy_path.clone(),
        };
        // 只读解析：v1 的写回只发生在 `load()`，这里绝不改旧文件。
        let profile = match legacy.read_only() {
            Ok(Some(profile)) => profile,
            Ok(None) => return Ok(false),
            // 旧开发配置可能是 `local_dev`（v1 默认），而产品构建不开放该模式。
            // 这不是"配置损坏"：旧文件继续原样保留，产品以空配置从 Welcome 起步，
            // 避免升级用户第一次打开就被 GUI 错误页挡住。
            Err(error @ ProfileError::InvalidRelay { .. }) => {
                eprintln!("[menghunote] 旧同步配置不适用于当前构建，跳过迁移：{error}");
                return Ok(false);
            }
            Err(error) => return Err(error),
        };
        // 原子写新配置（`save` 会再校验/ canonical 化，并用同目录临时文件 + rename）。
        self.store().save(&profile)?;
        Ok(true)
    }

    /// 某个实例 id 对应的默认目录（`instances/<id>/`）。
    pub fn instance_dir(&self, instance_id: &str) -> Result<PathBuf, ProfileError> {
        validate_instance_id(instance_id)?;
        Ok(self.instances_dir.join(instance_id))
    }

    /// 某个实例 id 对应的默认库路径（`instances/<id>/vault.sqlite3`）。
    pub fn vault_path(&self, instance_id: &str) -> Result<PathBuf, ProfileError> {
        Ok(self.instance_dir(instance_id)?.join(VAULT_FILE_NAME))
    }

    /// 用户可见的默认 vault 根（**只是默认值**；用户可在表单里改）。
    ///
    /// - 平台默认布局（[`Self::discover`]）：`<Documents>/menghunote`，Documents
    ///   不存在时退回 `<HOME>/menghunote`（Windows 用 `USERPROFILE`）；
    /// - 显式布局（[`Self::new`]）：数据根（既有注入语义，不读平台环境）。
    ///
    /// 它**不**等于配置目录：`sync.json` 仍留平台配置目录（Windows LOCALAPPDATA）。
    #[must_use]
    pub fn default_vault_root(&self) -> PathBuf {
        self.default_vault_root
            .clone()
            .unwrap_or_else(|| self.data_root())
    }

    /// Create/Join 表单的默认实例目录：`<默认 vault 根>/instances/<id>`。
    ///
    /// 与 [`Self::instance_dir`] 的区别：后者恒为配置布局的 `instances/`（旧默认，
    /// 可能落在 LOCALAPPDATA）；这里返回**用户可见目录**下的位置，作为表单初始值。
    /// 两者都只是默认值，提交时以用户输入为准（写进 `vault_path`）。
    pub fn default_instance_dir(&self, instance_id: &str) -> Result<PathBuf, ProfileError> {
        validate_instance_id(instance_id)?;
        Ok(self
            .default_vault_root()
            .join(INSTANCES_DIR_NAME)
            .join(instance_id))
    }

    /// 「仅本地使用」入口的默认库文件：`<默认 vault 根>/local/vault.sqlite3`。
    ///
    /// **不建目录、不写配置**；显式布局下与 [`Self::local_vault_path`] 相同（既有
    /// 测试/注入语义），平台默认布局下是用户文档目录（不再默认 LOCALAPPDATA）。
    #[must_use]
    pub fn default_local_vault_path(&self) -> PathBuf {
        self.default_vault_root()
            .join(LOCAL_DIR_NAME)
            .join(VAULT_FILE_NAME)
    }

    /// **数据根**：`instances/` 的上一级（产品目录 `.../menghunote`）。
    ///
    /// 本地（不同步）库与 `instances/` 同级，所以只依赖布局解析结果；显式
    /// [`Self::new`] 的路径也按同一条规则推导（`instances_dir` 的父目录），
    /// 测试/第三方注入不用再传第二个根。
    #[must_use]
    pub fn data_root(&self) -> PathBuf {
        self.instances_dir
            .parent()
            .map_or_else(|| self.instances_dir.clone(), Path::to_path_buf)
    }

    /// 本地（不同步）库目录：`<数据根>/local/`。
    #[must_use]
    pub fn local_dir(&self) -> PathBuf {
        self.data_root().join(LOCAL_DIR_NAME)
    }

    /// 本地（不同步）库路径：`<数据根>/local/vault.sqlite3`。
    ///
    /// - Windows：`%LOCALAPPDATA%\menghunote\local\vault.sqlite3`
    /// - Linux：`$XDG_DATA_HOME/menghunote/local/vault.sqlite3`
    ///
    /// 只做路径解析：**不建目录、不写配置、不碰同步实例**（打开动作在
    /// [`crate::dev_vault::open_local_vault_at`]）。
    #[must_use]
    pub fn local_vault_path(&self) -> PathBuf {
        self.local_dir().join(VAULT_FILE_NAME)
    }

    /// 本机启动偏好路径：`<数据根>/startup.json`。
    ///
    /// 与 `sync.json` 分开：它只描述"上次成功打开的是本地库还是同步实例"，
    /// 不记录实例列表、不需要迁移；缺失/损坏由启动引导按 Welcome 处理。
    #[must_use]
    pub fn startup_path(&self) -> PathBuf {
        self.data_root().join(STARTUP_FILE_NAME)
    }

    /// 设置页分类记忆路径：`<数据根>/settings-ui.json`。
    ///
    /// 本机 UI 偏好（只记分类短名），不进 vault、不参与同步。
    #[must_use]
    pub fn settings_ui_path(&self) -> PathBuf {
        self.data_root().join(SETTINGS_UI_FILE_NAME)
    }

    /// 快捷键偏好路径：`<数据根>/shortcuts.json`。
    ///
    /// 本机 UI 偏好（与 [`crate::shortcuts`] 共用文件格式）；不进 vault、
    /// 不参与同步。测试/显式布局必须走这里，不能落到平台用户目录。
    #[must_use]
    pub fn shortcuts_path(&self) -> PathBuf {
        self.data_root().join(SHORTCUTS_FILE_NAME)
    }

    /// 配置存储句柄。
    #[must_use]
    pub fn store(&self) -> ProfileStore {
        ProfileStore {
            path: self.config_path.clone(),
        }
    }
}

/// 用户可见的默认 vault 根：`<Documents>/menghunote`，Documents 不存在则
/// `<HOME>/menghunote`。
///
/// - Windows 优先 `USERPROFILE`（需求明确要求 `%USERPROFILE%\Documents\...`；
///   不跟随 OneDrive 重定向，避免把库写进用户没预期的位置）；
/// - 其它平台用 `HOME`；
/// - 两者都没有 → `None`（调用方回退数据根，保证应用仍可用）。
///
/// 只做路径解析 + `Documents` 是否存在这一项本机事实判断，不建目录。
#[must_use]
pub fn user_default_vault_root(env: impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    user_default_vault_root_for(env, cfg!(windows))
}

/// [`user_default_vault_root`] 的平台可注入版本（测试在 Linux 上也能覆盖 Windows 分支）。
#[must_use]
pub fn user_default_vault_root_for(
    env: impl Fn(&str) -> Option<String>,
    windows: bool,
) -> Option<PathBuf> {
    let var = if windows { "USERPROFILE" } else { "HOME" };
    let home = env(var)?;
    let home = PathBuf::from(home.trim());
    if home.as_os_str().is_empty() {
        return None;
    }
    let documents = home.join("Documents");
    let base = if documents.is_dir() { documents } else { home };
    Some(base.join(APP_DIR_NAME))
}

/// 上次成功打开的入口（本机启动偏好；不同步、不含秘密/正文）。
///
/// 启动引导只在**打开成功**后写它：本地库 → [`StartupMode::Local`]；
/// 同步实例 → [`StartupMode::Sync`]。文件缺失/损坏/未知值 = 没有可恢复的入口。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupMode {
    /// 本地（不同步）库（产品 `local/vault.sqlite3`；dev-sync 构建是默认 dev vault）。
    Local,
    /// 同步实例（`sync.json` 的 active）。
    Sync,
}

impl StartupMode {
    /// 稳定短名（JSON 里唯一值；也是测试/诊断字面量）。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Sync => "sync",
        }
    }

    /// 从 JSON 文本解析；缺失/不可读/损坏/未知 `mode`/未知版本一律 `None`。
    ///
    /// 只取 `mode`，忽略扩展字段（例如 [`StartupPreferences::local_vault_path`]），
    /// 这样旧客户端/旧测试读新文件仍成立。
    ///
    /// `version` 字段**可缺省**（`{"mode":"local"}` 也接受，字段最小化）；一旦给出
    /// 就必须是 [`STARTUP_PREFERENCES_VERSION`]，避免未来格式被旧程序误读。
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        StartupPreferences::parse(text).map(|preferences| preferences.mode)
    }

    /// 序列化为稳定 JSON（写入前先落临时文件再 rename）。
    ///
    /// 这是**旧格式**（只有 `version` + `mode`）；新代码写
    /// [`StartupPreferences::to_json`]（可选带 `local_vault_path`）。
    #[must_use]
    pub fn to_json(self) -> String {
        format!(
            "{{\n  \"version\": {STARTUP_PREFERENCES_VERSION},\n  \"mode\": \"{}\"\n}}\n",
            self.as_str()
        )
    }
}

/// 本机启动偏好（`startup.json`）：上次打开的入口 + 上次本地库路径。
///
/// 向后兼容：旧文件只有 `{"version":1,"mode":"local"}`（没有 `local_vault_path`），
/// [`Self::parse`] 仍能读，路径为 `None`（启动引导再按旧数据根/新用户目录回退）。
///
/// 只记"上次成功打开的入口"，**不同步、不含正文/秘密**；`version` 保持 1（新增字段
/// 是可选扩展，旧解析器忽略即可）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupPreferences {
    /// 上次成功打开的入口。
    pub mode: StartupMode,
    /// 上次本地（不同步）库的绝对路径；`None` = 旧格式/没有本地路径。
    pub local_vault_path: Option<PathBuf>,
}

impl StartupPreferences {
    /// 只有入口、没有本地路径（同步入口/旧格式）。
    #[must_use]
    pub fn new(mode: StartupMode) -> Self {
        Self {
            mode,
            local_vault_path: None,
        }
    }

    /// 带上次本地库路径（本地入口成功后写）。
    #[must_use]
    pub fn with_local_vault_path(mode: StartupMode, path: impl Into<PathBuf>) -> Self {
        Self {
            mode,
            local_vault_path: Some(path.into()),
        }
    }

    /// 从 JSON 文本解析；缺失/不可读/损坏/未知 `mode`/未知版本一律 `None`。
    ///
    /// `local_vault_path` 缺失、空串或不是字符串 → `None`（忽略该扩展，不丢 `mode`）。
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let value: serde_json::Value = serde_json::from_str(text).ok()?;
        if let Some(version) = value.get("version") {
            if version.as_u64() != Some(u64::from(STARTUP_PREFERENCES_VERSION)) {
                return None;
            }
        }
        let mode = match value.get("mode")?.as_str()? {
            "local" => StartupMode::Local,
            "sync" => StartupMode::Sync,
            _ => return None,
        };
        let local_vault_path = value
            .get("local_vault_path")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .map(PathBuf::from);
        Some(Self {
            mode,
            local_vault_path,
        })
    }

    /// 序列化为稳定 JSON（写入前先落临时文件再 rename）。
    ///
    /// 没有本地路径时不写该字段：保持与旧格式逐字节兼容（旧程序/测试仍按
    /// `{version,mode}` 读）。
    #[must_use]
    pub fn to_json(&self) -> String {
        let mut value = serde_json::json!({
            "version": STARTUP_PREFERENCES_VERSION,
            "mode": self.mode.as_str(),
        });
        if let Some(path) = &self.local_vault_path {
            value["local_vault_path"] =
                serde_json::Value::String(path.to_string_lossy().into_owned());
        }
        let mut text = serde_json::to_string_pretty(&value).unwrap_or_default();
        text.push('\n');
        text
    }
}

/// 读取本机启动偏好；文件不存在/不可读/损坏 → `None`（不报错、不阻塞启动）。
#[must_use]
pub fn load_startup_preferences(path: &Path) -> Option<StartupPreferences> {
    let text = fs::read_to_string(path).ok()?;
    StartupPreferences::parse(&text)
}

/// 兼容旧调用点：只读入口（忽略本地路径）。
#[must_use]
pub fn load_startup_mode(path: &Path) -> Option<StartupMode> {
    load_startup_preferences(path).map(|preferences| preferences.mode)
}

/// 原子写入本机启动偏好（新格式；可选带本地库路径）。
pub fn save_startup_preferences(
    path: &Path,
    preferences: &StartupPreferences,
) -> std::io::Result<()> {
    write_json_file_atomically(path, &preferences.to_json())
}

/// 原子写入本机启动偏好（旧 API）：**保留**原文件里已记录的本地库路径。
///
/// 打开同步实例时只换 `mode`，不应该忘掉用户上次选的本地库位置。
pub fn save_startup_mode(path: &Path, mode: StartupMode) -> std::io::Result<()> {
    let local_vault_path =
        load_startup_preferences(path).and_then(|preferences| preferences.local_vault_path);
    save_startup_preferences(
        path,
        &StartupPreferences {
            mode,
            local_vault_path,
        },
    )
}

/// 原子写一个小 JSON 偏好文件（同目录临时文件 → `rename`）。
///
/// 与 `theme.json` / `language.json` / `sidebar.json` 同一套纪律：失败时旧文件原样
/// 保留；Windows 上目标存在时 `rename` 可能失败，退化为直接覆盖写（并清掉临时文件）。
pub(crate) fn write_json_file_atomically(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension("json.tmp");
    fs::write(&temp, text.as_bytes())?;
    match fs::rename(&temp, path) {
        Ok(()) => Ok(()),
        Err(_) => {
            let result = fs::write(path, text.as_bytes());
            let _ = fs::remove_file(&temp);
            result
        }
    }
}

/// 实例 id 只允许字母/数字/`-`/`_`/`.`，且不能是 `.`/`..`（防止路径穿越）。
pub fn validate_instance_id(instance_id: &str) -> Result<(), ProfileError> {
    let valid = !instance_id.is_empty()
        && instance_id != "."
        && instance_id != ".."
        && instance_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if valid {
        Ok(())
    } else {
        Err(ProfileError::InvalidInstanceId(instance_id.to_owned()))
    }
}

/// 自动生成一个本机实例 id（**与服务器 `vault_id` 分离**，纯索引用途）。
///
/// 不引入随机数依赖：纳秒时间 + 进程号 + 进程内单调计数已经足够避免同日目录冲突；
/// 冲突的真实后果只是"配置里两条索引"，不影响库/身份正确性。
#[must_use]
pub fn generate_instance_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos() as u64);
    format!("inst-{nanos:x}-{:x}-{seq:x}", std::process::id())
}

/// 用户自定义仓库名称的字符上限（Unicode scalar；不按字节计）。
pub const DISPLAY_NAME_MAX_CHARS: usize = 64;

/// 归一化/校验用户输入的仓库名称。
///
/// - 前后空白先 trim；trim 后为空 → `Ok(None)`（"没有自定义名"，显示层回退目录名）；
/// - 1–[`DISPLAY_NAME_MAX_CHARS`] 个字符（中文/空格/emoji 都合法，与文件名无关）；
/// - 拒绝控制字符（`char::is_control`：C0/C1/DEL），避免把换行/制表符画进按钮；
/// - 其它一律保留原文（trim 后的样子），异常返回 [`ProfileError::InvalidDisplayName`]。
///
/// 只在**写配置/改名入口**做归一化；显示层用 [`InstanceEntry::effective_name`]。
pub fn normalize_display_name(raw: &str) -> Result<Option<String>, ProfileError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let len = trimmed.chars().count();
    if len > DISPLAY_NAME_MAX_CHARS {
        return Err(ProfileError::InvalidDisplayName {
            detail: format!(
                "最多 {DISPLAY_NAME_MAX_CHARS} 个字符（当前 {len} 个）；名称只用于显示，\
                 不会改动目录/库路径"
            ),
        });
    }
    if let Some(bad) = trimmed.chars().find(|c| c.is_control()) {
        return Err(ProfileError::InvalidDisplayName {
            detail: format!(
                "不能包含控制字符（发现 U+{:04X}）；名称只用于显示，\
                 不会改动目录/库路径",
                u32::from(bad)
            ),
        });
    }
    Ok(Some(trimmed.to_owned()))
}

/// 一个已保存实例（**只公开信息**）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceEntry {
    /// 本机实例 id（与服务器 `vault_id` 分离）。
    pub instance_id: String,
    /// 用户自定义显示名（可选；**只影响界面文案**）。
    ///
    /// 旧 v2 配置没有该字段 → `#[serde(default)]` 读成 `None`；写入前由
    /// [`normalize_display_name`] 归一化（trim/1–64 字符/无控制字符，空白 = `None`）。
    /// **不参与**目录/库路径/身份的任何计算。
    #[serde(default)]
    pub display_name: Option<String>,
    /// relay 接入模式（ADR 0013 §1）：`local_dev` / `remote_tls`。
    ///
    /// 未知模式在反序列化时就被拒绝（serde 错误进 [`ProfileError::Corrupt`]），
    /// 不做"猜一个默认值"。
    #[serde(with = "relay_mode_json")]
    pub relay_mode: RelayMode,
    /// relay API 面（ADR 0014 S3a）：`1`=v1（缺省，旧配置兼容）、`2`=v2。
    ///
    /// 旧配置没有该字段 → 显式按 **v1** 解读（不是"猜"，v1 是唯一既有 wire）；
    /// 未知数字在反序列化阶段失败（`ProfileError::Corrupt`），不回退。
    #[serde(default, with = "relay_api_version_json")]
    pub api_version: RelayApiVersion,
    /// relay **canonical origin**（不含路径/query/userinfo；公开）。
    pub relay_url: String,
    /// 库文件绝对路径。
    pub vault_path: String,
    /// 服务器 vault id（32 位十六进制；公开标识）。
    pub vault_id: String,
    /// 本机设备 id（32 位十六进制；公开标识）。
    pub device_id: String,
}

impl InstanceEntry {
    /// 库路径（`PathBuf`）。
    #[must_use]
    pub fn vault_path(&self) -> PathBuf {
        PathBuf::from(&self.vault_path)
    }

    /// 实例目录（库文件所在目录）。
    #[must_use]
    pub fn instance_dir(&self) -> PathBuf {
        let path = self.vault_path();
        path.parent()
            .map_or_else(|| path.clone(), std::path::Path::to_path_buf)
    }

    /// 状态栏短标签（目录名/文件名，不打印完整路径）。
    #[must_use]
    pub fn short_label(&self) -> String {
        let path = self.vault_path();
        let file = path.file_name().map_or_else(
            || path.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        );
        path.parent()
            .and_then(Path::file_name)
            .map_or(file.clone(), |parent| {
                format!("{}/{}", parent.to_string_lossy(), file)
            })
    }

    /// 库文件所在目录名（`…/instances/inst-a/vault.sqlite3` → `inst-a`）。
    ///
    /// 只取目录名（不显示库文件名/完整路径）；路径异常时 `None`。
    #[must_use]
    pub fn directory_name(&self) -> Option<String> {
        Path::new(&self.vault_path)
            .parent()
            .and_then(Path::file_name)
            .map(|name| name.to_string_lossy().trim().to_owned())
            .filter(|name| !name.is_empty())
    }

    /// 界面显示名（**优先自定义名**）：`display_name` → 目录名 → `short_public_id(instance_id)`。
    ///
    /// 左下角切换按钮、切换菜单当前行、管理仓库、Welcome 左栏都走这里，保证"同一份
    /// 名字"；`short_label()` 仍然专供状态栏（目录/库文件名，语义不同）。
    #[must_use]
    pub fn effective_name(&self) -> String {
        if let Some(name) = self
            .display_name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
        {
            return name.to_owned();
        }
        self.directory_name()
            .unwrap_or_else(|| super::settings::short_public_id(&self.instance_id))
    }
}

/// 公开配置（整份文件）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DevSyncProfile {
    /// 格式版本（写入时恒为 [`PROFILE_VERSION`]）。
    pub version: u32,
    /// 上次使用的实例 id。
    #[serde(default)]
    pub active_instance: Option<String>,
    /// 已登记实例（按登记顺序）。
    #[serde(default)]
    pub instances: Vec<InstanceEntry>,
}

impl Default for DevSyncProfile {
    fn default() -> Self {
        Self {
            version: PROFILE_VERSION,
            active_instance: None,
            instances: Vec::new(),
        }
    }
}

impl DevSyncProfile {
    /// 取 `active_instance` 对应的条目；`active` 指向不存在的 id 是**错误**。
    pub fn active(&self) -> Result<Option<&InstanceEntry>, ProfileError> {
        let Some(id) = self.active_instance.as_deref() else {
            return Ok(None);
        };
        self.instances
            .iter()
            .find(|entry| entry.instance_id == id)
            .map(Some)
            .ok_or_else(|| ProfileError::UnknownActive {
                instance_id: id.to_owned(),
            })
    }

    /// 按 instance_id 找条目。
    #[must_use]
    pub fn find(&self, instance_id: &str) -> Option<&InstanceEntry> {
        self.instances
            .iter()
            .find(|entry| entry.instance_id == instance_id)
    }

    /// 登记/更新一个实例，并把它设为 active（配置写入只在备份确认之后调用）。
    pub fn upsert_active(&mut self, entry: InstanceEntry) {
        self.version = PROFILE_VERSION;
        self.active_instance = Some(entry.instance_id.clone());
        match self
            .instances
            .iter_mut()
            .find(|existing| existing.instance_id == entry.instance_id)
        {
            Some(existing) => *existing = entry,
            None => self.instances.push(entry),
        }
    }

    /// 改一个已保存实例的**显示名**（`None`/空白 = 清除自定义名，回退目录名）。
    ///
    /// 只动 `display_name`：**不碰** `active_instance` / `vault_path` / `vault_id` /
    /// `device_id`，也**不移动/重建**实例目录、不碰 worker/实例锁。名称先过
    /// [`normalize_display_name`]（trim、1–64 字符、无控制字符）。
    ///
    /// 本方法只改内存；调用方用 [`ProfileStore::save`] 原子写盘（失败时磁盘原样保留）。
    /// 实例不存在是错误（[`ProfileError::UnknownInstance`]），不自动新增条目。
    pub fn rename_instance(
        &mut self,
        instance_id: &str,
        name: Option<&str>,
    ) -> Result<(), ProfileError> {
        let normalized = match name {
            Some(raw) => normalize_display_name(raw)?,
            None => None,
        };
        let Some(entry) = self
            .instances
            .iter_mut()
            .find(|entry| entry.instance_id == instance_id)
        else {
            return Err(ProfileError::UnknownInstance {
                instance_id: instance_id.to_owned(),
            });
        };
        entry.display_name = normalized;
        Ok(())
    }

    /// Welcome 列表顺序：active 在前，其余保持登记顺序。
    #[must_use]
    pub fn ordered_entries(&self) -> Vec<&InstanceEntry> {
        let active = self.active_instance.as_deref();
        let mut ordered: Vec<&InstanceEntry> = self
            .instances
            .iter()
            .filter(|entry| Some(entry.instance_id.as_str()) == active)
            .collect();
        ordered.extend(
            self.instances
                .iter()
                .filter(|entry| Some(entry.instance_id.as_str()) != active),
        );
        ordered
    }
}

/// 磁盘配置来自哪个版本（决定 [`ProfileStore::load`] 是否要把 v1 写回 v2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProfileOrigin {
    /// v1（没有 `relay_mode`）：只在内存里迁到 v2。
    V1,
    /// 当前版本 v2。
    V2,
}

/// 公开配置的读写（原子写）。
#[derive(Debug, Clone)]
pub struct ProfileStore {
    path: PathBuf,
}

impl ProfileStore {
    /// 配置路径。
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 读取配置（ADR 0013 §1 的版本/模式规则）。
    ///
    /// - 文件不存在 → `Ok(None)`（不是错误）；
    /// - v2 → 校验后返回；
    /// - v1 → **显式迁移**为 v2（每实例 `local_dev`，URL canonical 化）并**原子写回**；
    ///   写回失败返回错误（不继续用旧格式跑）；
    /// - 未知版本 / 未知模式 / 坏 JSON / 悬空 active / 非法 relay → 明确错误，
    ///   不重建、不默认回退。
    pub fn load(&self) -> Result<Option<DevSyncProfile>, ProfileError> {
        match self.read_with_origin()? {
            None => Ok(None),
            // 显式迁移：v1 只允许变成 local_dev，并立即写回 v2（失败即错误）。
            Some((profile, ProfileOrigin::V1)) => {
                self.save(&profile)?;
                Ok(Some(profile))
            }
            Some((profile, ProfileOrigin::V2)) => Ok(Some(profile)),
        }
    }

    /// **只读**解析：v1 只在内存里升到 v2，**不回写**任何文件。
    ///
    /// 旧开发目录迁移专用：旧配置必须原样保留（v1 的写回只发生在 [`Self::load`]）。
    fn read_only(&self) -> Result<Option<DevSyncProfile>, ProfileError> {
        Ok(self.read_with_origin()?.map(|(profile, _origin)| profile))
    }

    /// 解析磁盘上的配置并返回它来自哪个版本（v1 需要调用方决定是否写回）。
    fn read_with_origin(&self) -> Result<Option<(DevSyncProfile, ProfileOrigin)>, ProfileError> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(ProfileError::Io {
                    path: self.path.clone(),
                    source: error,
                })
            }
        };
        let value: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|error| ProfileError::Corrupt {
                path: self.path.clone(),
                // serde 的错误只提"第几行第几列"与期望字段，不含文件内容。
                detail: error.to_string(),
            })?;
        let Some(version) = value.get("version").and_then(serde_json::Value::as_u64) else {
            return Err(ProfileError::Corrupt {
                path: self.path.clone(),
                detail: "缺少 version 字段".to_owned(),
            });
        };
        match u32::try_from(version).unwrap_or(u32::MAX) {
            PROFILE_VERSION => {
                let profile: DevSyncProfile =
                    serde_json::from_value(value).map_err(|error| ProfileError::Corrupt {
                        path: self.path.clone(),
                        detail: error.to_string(),
                    })?;
                validate_profile(&profile)?;
                Ok(Some((profile, ProfileOrigin::V2)))
            }
            PROFILE_VERSION_V1 => {
                let legacy: DevSyncProfileV1 =
                    serde_json::from_value(value).map_err(|error| ProfileError::Corrupt {
                        path: self.path.clone(),
                        detail: error.to_string(),
                    })?;
                // 防御性再核对一次（正常走不到：上面的 match 已按 version 分派）。
                if legacy.version != PROFILE_VERSION_V1 {
                    return Err(ProfileError::UnsupportedVersion {
                        path: self.path.clone(),
                        version: legacy.version,
                    });
                }
                let profile = migrate_v1(legacy)?;
                Ok(Some((profile, ProfileOrigin::V1)))
            }
            other => Err(ProfileError::UnsupportedVersion {
                path: self.path.clone(),
                version: other,
            }),
        }
    }

    /// 原子写入配置：同目录临时文件 → write/fsync → rename → 父目录 fsync。
    ///
    /// 失败时旧配置**原样保留**，临时文件被清掉；调用方提示「重试保存」，不重跑 Create。
    pub fn save(&self, profile: &DevSyncProfile) -> Result<(), ProfileError> {
        let mut profile = profile.clone();
        profile.version = PROFILE_VERSION;
        // 写前校验 + canonical 化：配置里只存权威 parser 认可的模式/地址，
        // 手改的非法值在写回前就失败（不把坏值落盘）。显示名同样归一化
        // （trim；空白 = None；超长/控制字符拒绝）。
        for entry in &mut profile.instances {
            validate_instance_id(&entry.instance_id)?;
            entry.display_name = entry
                .display_name
                .as_deref()
                .map(normalize_display_name)
                .transpose()?
                .flatten();
            let origin = parse_for_build(&entry.relay_url, entry.relay_mode).map_err(|error| {
                ProfileError::InvalidRelay {
                    instance_id: entry.instance_id.clone(),
                    detail: error.to_string(),
                }
            })?;
            entry.relay_url = origin.as_str();
        }
        let _ = profile.active()?;
        let json = serde_json::to_vec_pretty(&profile).map_err(|error| ProfileError::Corrupt {
            path: self.path.clone(),
            detail: format!("序列化失败：{error}"),
        })?;
        let parent = self
            .path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
        fs::create_dir_all(&parent).map_err(|source| ProfileError::Io {
            path: parent.clone(),
            source,
        })?;
        let file_name = self.path.file_name().map_or_else(
            || PROFILE_FILE_NAME.to_owned(),
            |name| name.to_string_lossy().into_owned(),
        );
        let tmp = parent.join(format!(
            ".{file_name}.tmp.{}.{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |duration| duration.as_nanos() as u64)
        ));
        let result = (|| -> Result<(), ProfileError> {
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt as _;
                options.mode(0o600);
            }
            let mut file = options.open(&tmp).map_err(|source| ProfileError::Io {
                path: tmp.clone(),
                source,
            })?;
            file.write_all(&json).map_err(|source| ProfileError::Io {
                path: tmp.clone(),
                source,
            })?;
            file.sync_all().map_err(|source| ProfileError::Io {
                path: tmp.clone(),
                source,
            })?;
            drop(file);
            fs::rename(&tmp, &self.path).map_err(|source| ProfileError::Io {
                path: self.path.clone(),
                source,
            })?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
            return result;
        }
        fsync_dir(&parent).map_err(|source| ProfileError::Io {
            path: parent,
            source,
        })
    }
}

/// v1 配置里的一条实例（**没有** `relay_mode`）。
///
/// 只用于读取旧配置；迁移目标恒为 `local_dev`（ADR 0013 §1）。
#[derive(Deserialize)]
struct InstanceEntryV1 {
    instance_id: String,
    relay_url: String,
    vault_path: String,
    vault_id: String,
    device_id: String,
}

/// v1 公开配置（ADR 0011）。
#[derive(Deserialize)]
struct DevSyncProfileV1 {
    version: u32,
    #[serde(default)]
    active_instance: Option<String>,
    #[serde(default)]
    instances: Vec<InstanceEntryV1>,
}

/// v1 → v2：**只允许** `local_dev`；URL 由权威 parser canonical 化。
fn migrate_v1(legacy: DevSyncProfileV1) -> Result<DevSyncProfile, ProfileError> {
    let mut profile = DevSyncProfile {
        version: PROFILE_VERSION,
        active_instance: legacy.active_instance,
        instances: Vec::with_capacity(legacy.instances.len()),
    };
    for entry in legacy.instances {
        validate_instance_id(&entry.instance_id)?;
        let origin = parse_for_build(&entry.relay_url, RelayMode::LocalDev).map_err(|error| {
            ProfileError::InvalidRelay {
                instance_id: entry.instance_id.clone(),
                detail: format!("v1 配置只能显式迁移为 local_dev：{error}"),
            }
        })?;
        profile.instances.push(InstanceEntry {
            instance_id: entry.instance_id,
            display_name: None,
            relay_mode: RelayMode::LocalDev,
            api_version: RelayApiVersion::V1,
            relay_url: origin.as_str(),
            vault_path: entry.vault_path,
            vault_id: entry.vault_id,
            device_id: entry.device_id,
        });
    }
    let _ = profile.active()?;
    Ok(profile)
}

/// v2 配置校验：index、实例 id、模式/URL 与 canonical 形态、显示名归一化形态。
fn validate_profile(profile: &DevSyncProfile) -> Result<(), ProfileError> {
    let _ = profile.active()?;
    for entry in &profile.instances {
        validate_instance_id(&entry.instance_id)?;
        // 磁盘上的名字必须已经是归一化形态（save 会 trim；手改出的空白/超长/
        // 控制字符不静默改写，直接报错让用户修正）。
        if let Some(stored) = entry.display_name.as_deref() {
            let normalized = normalize_display_name(stored)?;
            if normalized.as_deref() != Some(stored) {
                return Err(ProfileError::InvalidDisplayName {
                    detail: format!(
                        "实例 {} 的 display_name 不是归一化形态（应为 {:?}；\
                         首尾空白/纯空白请手工修正）",
                        entry.instance_id, normalized
                    ),
                });
            }
        }
        let origin = parse_for_build(&entry.relay_url, entry.relay_mode).map_err(|error| {
            ProfileError::InvalidRelay {
                instance_id: entry.instance_id.clone(),
                detail: error.to_string(),
            }
        })?;
        // 存储只接受 canonical origin；手改出的尾斜杠/默认端口不静默改写。
        if origin.as_str() != entry.relay_url {
            return Err(ProfileError::InvalidRelay {
                instance_id: entry.instance_id.clone(),
                detail: format!(
                    "relay_url 不是 canonical origin（应为 {:?}）",
                    origin.as_str()
                ),
            });
        }
    }
    Ok(())
}

/// `RelayApiVersion` 的 serde 桥：配置里存数字 `1`/`2`（旧配置缺省 = 1）。
///
/// 未知数字在反序列化阶段失败（`ProfileError::Corrupt`），不静默降级到 v1。
mod relay_api_version_json {
    use super::RelayApiVersion;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(
        version: &RelayApiVersion,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_u32(version.as_number())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<RelayApiVersion, D::Error> {
        let value = u32::deserialize(deserializer)?;
        RelayApiVersion::from_number(value).ok_or_else(|| {
            serde::de::Error::custom(format!(
                "未知 relay API 版本 {value}：只支持 1（v1）/ 2（v2）"
            ))
        })
    }
}

/// `RelayMode` 的 serde 桥：单一权威类型在 `menghunote-sync`，这里只做字符串编解码。
///
/// 未知模式在反序列化阶段就失败（`ProfileError::Corrupt`），不会落到默认值。
mod relay_mode_json {
    use super::RelayMode;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(mode: &RelayMode, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(mode.as_str())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<RelayMode, D::Error> {
        let text = String::deserialize(deserializer)?;
        RelayMode::parse(&text).ok_or_else(|| {
            serde::de::Error::custom(format!(
                "未知 relay 模式 {text:?}：只支持 local_dev / remote_tls"
            ))
        })
    }
}

/// fsync 父目录（Windows 上 std 打不开目录，明确 no-op）。
#[cfg(unix)]
fn fsync_dir(dir: &Path) -> std::io::Result<()> {
    fs::File::open(dir)?.sync_all()
}

#[cfg(not(unix))]
fn fsync_dir(_dir: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// v2 旧配置（`remote_tls`）：两种构建都允许，用来只测"复制公开实例列表"。
    const LEGACY_REMOTE_PROFILE: &[u8] = br#"{
      "version": 2,
      "active_instance": "inst-old",
      "instances": [{
        "instance_id": "inst-old",
        "relay_mode": "remote_tls",
        "api_version": 2,
        "relay_url": "https://relay.example",
        "vault_path": "/data/menghunote-dev/instances/inst-old/vault.sqlite3",
        "vault_id": "00112233445566778899aabbccddeeff",
        "device_id": "ffeeddccbbaa99887766554433221100"
      }]
    }"#;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect();
        move |key| map.get(key).cloned()
    }

    fn temp_dir(prefix: &str) -> PathBuf {
        // lib 单测拿不到 `CARGO_TARGET_TMPDIR`（那是集成测试才有的编译期变量），
        // 因此按 AGENTS §6 的约定落到仓库 `target/tmp` 下。
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/tmp")
            .join(format!("onboarding-layout-{prefix}-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("创建测试目录失败");
        let dir = root.join(format!("case-{}", generate_instance_id()));
        std::fs::create_dir_all(&dir).expect("创建测试用例目录失败");
        dir
    }

    /// 在临时根下按平台默认布局解析（注入 env，**不碰真实用户目录**）。
    fn discover_in(root: &Path) -> ProfileLayout {
        let mut env: HashMap<String, String> = HashMap::new();
        #[cfg(not(windows))]
        {
            let home = root.join("home");
            let config = root.join("config");
            let data = root.join("data");
            for dir in [&home, &config, &data] {
                std::fs::create_dir_all(dir).expect("创建临时 XDG 目录失败");
            }
            env.insert("HOME".to_owned(), home.to_string_lossy().into_owned());
            env.insert(
                "XDG_CONFIG_HOME".to_owned(),
                config.to_string_lossy().into_owned(),
            );
            env.insert(
                "XDG_DATA_HOME".to_owned(),
                data.to_string_lossy().into_owned(),
            );
        }
        #[cfg(windows)]
        {
            let local = root.join("localappdata");
            let home = root.join("home");
            for dir in [&local, &home] {
                std::fs::create_dir_all(dir).expect("创建临时平台目录失败");
            }
            env.insert(
                "LOCALAPPDATA".to_owned(),
                local.to_string_lossy().into_owned(),
            );
            env.insert(
                "USERPROFILE".to_owned(),
                home.to_string_lossy().into_owned(),
            );
        }
        ProfileLayout::discover(move |key| env.get(key).cloned()).expect("临时默认布局解析失败")
    }

    /// 与 [`discover_in`] 对应的期望路径：`(新配置, 旧配置, 新实例目录)`。
    #[cfg(not(windows))]
    fn expected_default_paths(root: &Path) -> (PathBuf, PathBuf, PathBuf) {
        (
            root.join("config").join("menghunote").join("sync.json"),
            root.join("config")
                .join("menghunote-dev")
                .join("dev-sync.json"),
            root.join("data").join("menghunote").join("instances"),
        )
    }

    /// 与 [`discover_in`] 对应的期望路径：`(新配置, 旧配置, 新实例目录)`。
    #[cfg(windows)]
    fn expected_default_paths(root: &Path) -> (PathBuf, PathBuf, PathBuf) {
        (
            root.join("localappdata")
                .join("menghunote")
                .join("sync.json"),
            root.join("localappdata")
                .join("menghunote-dev")
                .join("dev-sync.json"),
            root.join("localappdata")
                .join("menghunote")
                .join("instances"),
        )
    }

    #[cfg(not(windows))]
    #[test]
    fn linux_defaults_follow_xdg_and_fall_back_to_home() {
        let layout = ProfileLayout::discover(env_of(&[
            ("HOME", "/home/alice"),
            ("XDG_DATA_HOME", "/tmp/data"),
            ("XDG_CONFIG_HOME", "/tmp/config"),
        ]))
        .expect("有 HOME 就能解析");
        assert_eq!(
            layout.config_path,
            PathBuf::from("/tmp/config/menghunote/sync.json")
        );
        assert_eq!(
            layout.instances_dir,
            PathBuf::from("/tmp/data/menghunote/instances")
        );
        assert_eq!(
            layout.legacy_config_path,
            Some(PathBuf::from("/tmp/config/menghunote-dev/dev-sync.json")),
            "默认布局必须知道旧开发目录的配置路径（迁移用），但不去读它"
        );
        assert_eq!(
            layout.local_vault_path(),
            PathBuf::from("/tmp/data/menghunote/local/vault.sqlite3"),
            "本地（不同步）库在数据根下：$XDG_DATA_HOME/menghunote/local/vault.sqlite3"
        );

        let layout = ProfileLayout::discover(env_of(&[("HOME", "/home/alice")]))
            .expect("只有 HOME 时用 XDG 默认回退");
        assert_eq!(
            layout.config_path,
            PathBuf::from("/home/alice/.config/menghunote/sync.json")
        );
        assert_eq!(
            layout.instances_dir,
            PathBuf::from("/home/alice/.local/share/menghunote/instances")
        );
        assert_eq!(
            layout.local_vault_path(),
            PathBuf::from("/home/alice/.local/share/menghunote/local/vault.sqlite3"),
            "本地库走 XDG_DATA_HOME 的默认回退（~/.local/share）"
        );
        assert_eq!(
            layout.legacy_config_path,
            Some(PathBuf::from(
                "/home/alice/.config/menghunote-dev/dev-sync.json"
            ))
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn missing_home_is_an_error_not_a_cwd_fallback() {
        let error = ProfileLayout::discover(env_of(&[("XDG_DATA_HOME", "/tmp/data")]))
            .expect_err("没有 HOME 必须报错");
        assert!(matches!(error, ProfileError::NoHome("HOME")), "{error}");
        // 空字符串等同于没设置。
        let error =
            ProfileLayout::discover(env_of(&[("HOME", "   ")])).expect_err("空 HOME 必须报错");
        assert!(matches!(error, ProfileError::NoHome("HOME")));
    }

    #[cfg(windows)]
    #[test]
    fn windows_defaults_use_local_appdata_not_roaming() {
        let layout =
            ProfileLayout::discover(env_of(&[("LOCALAPPDATA", r"C:\Users\alice\AppData\Local")]))
                .expect("有 LOCALAPPDATA 就能解析");
        assert_eq!(
            layout.config_path,
            PathBuf::from(r"C:\Users\alice\AppData\Local\menghunote\sync.json")
        );
        assert_eq!(
            layout.instances_dir,
            PathBuf::from(r"C:\Users\alice\AppData\Local\menghunote\instances")
        );
        assert_eq!(
            layout.legacy_config_path,
            Some(PathBuf::from(
                r"C:\Users\alice\AppData\Local\menghunote-dev\dev-sync.json"
            )),
            "旧开发目录路径只用于一次性迁移"
        );
        assert_eq!(
            layout.local_vault_path(),
            PathBuf::from(r"C:\Users\alice\AppData\Local\menghunote\local\vault.sqlite3"),
            "本地（不同步）库：%LOCALAPPDATA%\\menghunote\\local\\vault.sqlite3"
        );
    }

    /// 本地（不同步）库与 `instances/` 同级；显式布局按 `instances_dir` 父目录推导。
    #[test]
    fn local_vault_path_lives_next_to_instances() {
        let root = temp_dir("local-path");
        let layout = ProfileLayout::new(
            root.join("config/sync.json"),
            root.join("data").join("instances"),
        );
        assert_eq!(layout.data_root(), root.join("data"));
        assert_eq!(
            layout.local_dir(),
            root.join("data").join(LOCAL_DIR_NAME),
            "本地目录必须是数据根下的 local/"
        );
        assert_eq!(
            layout.local_vault_path(),
            root.join("data").join("local").join(VAULT_FILE_NAME),
            "本地库文件名沿用 vault.sqlite3"
        );
    }

    /// 显式 [`ProfileLayout::new`] 不参与迁移：第三方/测试显式路径自己决定读哪个文件。
    #[test]
    fn explicit_layout_has_no_legacy_path_and_never_migrates() {
        let dir = temp_dir("explicit-no-migrate");
        let legacy = dir.join("menghunote-dev").join(LEGACY_PROFILE_FILE_NAME);
        std::fs::create_dir_all(legacy.parent().expect("有父目录")).expect("建旧目录");
        std::fs::write(&legacy, LEGACY_REMOTE_PROFILE).expect("写旧配置");
        let layout = ProfileLayout::new(dir.join("config/sync.json"), dir.join("instances"));
        assert_eq!(layout.legacy_config_path, None);
        assert!(
            !layout.migrate_legacy_profile().expect("显式布局不迁移"),
            "显式布局不得触发迁移"
        );
        assert!(!layout.config_path.exists(), "不得写任何新配置");
    }

    /// 旧目录里的 v2 配置（`remote_tls`）：供迁移测试把 `vault_path` 指到临时库文件。
    fn legacy_remote_json(old_vault: &Path) -> serde_json::Value {
        serde_json::json!({
            "version": 2,
            "active_instance": "inst-old",
            "instances": [{
                "instance_id": "inst-old",
                "relay_mode": "remote_tls",
                "api_version": 2,
                "relay_url": "https://relay.example",
                "vault_path": old_vault.to_string_lossy(),
                "vault_id": "00112233445566778899aabbccddeeff",
                "device_id": "ffeeddccbbaa99887766554433221100"
            }]
        })
    }

    fn write_json(path: &Path, value: &serde_json::Value) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("创建 JSON 目录失败");
        }
        std::fs::write(
            path,
            serde_json::to_vec_pretty(value).expect("序列化 JSON 失败"),
        )
        .expect("写 JSON 失败");
    }

    /// P2 保守迁移：新目录默认路径 + 旧配置复制到 `sync.json`；库/身份不移动、旧文件保留。
    #[test]
    fn legacy_profile_is_migrated_to_new_sync_json_without_moving_vault() {
        let root = temp_dir("migrate-default");
        let layout = discover_in(&root);
        let (new_config, expected_legacy, expected_instances) = expected_default_paths(&root);
        assert_eq!(layout.config_path, new_config, "新配置必须叫 sync.json");
        assert_eq!(
            layout.instances_dir, expected_instances,
            "实例目录仍 instances/"
        );
        let legacy = layout
            .legacy_config_path
            .clone()
            .expect("默认布局必须带旧配置路径");
        assert_eq!(legacy, expected_legacy);

        // 旧库文件真实存在：迁移只能复制公开列表，不得移动/复制/改动库与身份。
        let old_vault = legacy
            .parent()
            .expect("旧配置有父目录")
            .join(INSTANCES_DIR_NAME)
            .join("inst-old")
            .join(VAULT_FILE_NAME);
        std::fs::create_dir_all(old_vault.parent().expect("旧实例目录")).expect("建旧实例目录");
        std::fs::write(&old_vault, b"sqlite-bytes-keep").expect("写旧库文件");
        std::fs::write(
            old_vault.parent().unwrap().join("dev-identity.bin"),
            b"identity",
        )
        .expect("写旧身份文件");
        write_json(&legacy, &legacy_remote_json(&old_vault));
        let legacy_before = std::fs::read(&legacy).expect("读旧配置");
        let identity_path = old_vault.parent().unwrap().join("dev-identity.bin");
        let identity_before = std::fs::read(&identity_path).expect("读旧身份");

        assert!(layout.migrate_legacy_profile().expect("迁移必须成功"));

        assert_eq!(
            std::fs::read(&legacy).expect("旧配置必须仍在"),
            legacy_before,
            "旧配置必须原样保留（不删、不改写）"
        );
        assert_eq!(
            std::fs::read(&old_vault).expect("旧库必须仍在"),
            b"sqlite-bytes-keep",
            "不得移动/复制/改动库文件"
        );
        assert_eq!(
            std::fs::read(&identity_path).expect("旧身份必须仍在"),
            identity_before,
            "不得移动/复制/改动身份文件"
        );
        assert!(layout.config_path.exists(), "新 sync.json 必须出现");

        // 新配置是同一份公开实例列表，vault_path 保持旧路径。
        let loaded = layout
            .store()
            .load()
            .expect("读新配置")
            .expect("新配置存在");
        assert_eq!(loaded.version, PROFILE_VERSION);
        assert_eq!(loaded.active_instance.as_deref(), Some("inst-old"));
        assert_eq!(loaded.instances.len(), 1);
        let entry = &loaded.instances[0];
        assert_eq!(entry.instance_id, "inst-old");
        assert_eq!(entry.relay_mode, RelayMode::RemoteTls);
        assert_eq!(entry.api_version, RelayApiVersion::V2);
        assert_eq!(
            entry.vault_path,
            old_vault.to_string_lossy().into_owned(),
            "vault_path 必须保持旧路径（不迁移 instances 目录）"
        );
        assert_eq!(entry.relay_url, "https://relay.example");
        let raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&layout.config_path).expect("读新配置"))
                .expect("新配置是 JSON");
        assert_eq!(raw["version"], serde_json::json!(2), "新配置必须是 v2");

        // 幂等：第二次启动不再改新配置，也不再读旧配置。
        let new_before = std::fs::read(&layout.config_path).expect("读新配置");
        assert!(
            !layout
                .migrate_legacy_profile()
                .expect("二次迁移必须是 no-op"),
            "重复启动不得再迁移"
        );
        assert_eq!(
            std::fs::read(&layout.config_path).expect("读新配置"),
            new_before,
            "重复启动不得改写新配置"
        );
    }

    /// 新配置存在时一律以新配置为准：旧配置完全不读（不覆盖、不合并）。
    #[test]
    fn existing_new_config_takes_precedence_over_legacy() {
        let root = temp_dir("migrate-precedence");
        let layout = discover_in(&root);
        let legacy = layout.legacy_config_path.clone().expect("默认布局带旧路径");
        let new_vault = root.join("new").join("inst-new").join(VAULT_FILE_NAME);
        write_json(
            &layout.config_path,
            &serde_json::json!({
                "version": 2,
                "active_instance": "inst-new",
                "instances": [{
                    "instance_id": "inst-new",
                    "relay_mode": "remote_tls",
                    "api_version": 1,
                    "relay_url": "https://relay.example",
                    "vault_path": new_vault.to_string_lossy(),
                    "vault_id": "11112222333344445555666677778888",
                    "device_id": "88887777666655554444333322221111"
                }]
            }),
        );
        write_json(
            &legacy,
            &legacy_remote_json(Path::new("/old/inst-old/vault.sqlite3")),
        );
        let new_before = std::fs::read(&layout.config_path).expect("读新配置");
        let legacy_before = std::fs::read(&legacy).expect("读旧配置");

        assert!(
            !layout.migrate_legacy_profile().expect("新配置优先，不迁移"),
            "新配置存在时不得再迁移旧配置"
        );
        assert_eq!(
            std::fs::read(&layout.config_path).expect("读新配置"),
            new_before,
            "不得覆盖/合并新配置"
        );
        assert_eq!(
            std::fs::read(&legacy).expect("读旧配置"),
            legacy_before,
            "旧配置也不得被改写"
        );
        let loaded = layout.store().load().expect("读配置").expect("配置存在");
        assert_eq!(loaded.active_instance.as_deref(), Some("inst-new"));
    }

    /// 旧配置损坏/版本不支持：不自动重建、不写新配置；错误指出旧文件（现有错误语义）。
    #[test]
    fn corrupt_or_unsupported_legacy_is_not_rebuilt() {
        let root = temp_dir("migrate-corrupt");
        let layout = discover_in(&root);
        let legacy = layout.legacy_config_path.clone().expect("默认布局带旧路径");
        std::fs::create_dir_all(legacy.parent().expect("旧配置目录")).expect("建旧配置目录");

        let corrupt = b"{ not json".to_vec();
        std::fs::write(&legacy, &corrupt).expect("写坏旧配置");
        match layout
            .migrate_legacy_profile()
            .expect_err("坏旧配置必须报错")
        {
            ProfileError::Corrupt { path, .. } => assert_eq!(path, legacy, "错误必须指出旧文件"),
            other => panic!("错误类型不对：{other}"),
        }
        assert!(!layout.config_path.exists(), "坏旧配置不得触发重建");
        assert_eq!(std::fs::read(&legacy).expect("旧文件仍在"), corrupt);

        let unsupported = br#"{ "version": 99, "instances": [] }"#.to_vec();
        std::fs::write(&legacy, &unsupported).expect("写版本不支持");
        match layout
            .migrate_legacy_profile()
            .expect_err("版本不支持必须报错")
        {
            ProfileError::UnsupportedVersion { path, version } => {
                assert_eq!(path, legacy);
                assert_eq!(version, 99);
            }
            other => panic!("错误类型不对：{other}"),
        }
        assert!(!layout.config_path.exists(), "版本不支持不得覆盖/重建");
        assert_eq!(std::fs::read(&legacy).expect("旧文件仍在"), unsupported);
    }

    /// 迁移 v1 旧配置：只读解析（旧文件保持 v1），新文件是 v2，模式与 `vault_path` 不变。
    #[cfg(feature = "dev-sync")]
    #[test]
    fn migrating_legacy_v1_keeps_old_file_and_writes_v2() {
        let root = temp_dir("migrate-v1-new-dir");
        let layout = discover_in(&root);
        let legacy = layout.legacy_config_path.clone().expect("默认布局带旧路径");
        let old_vault = root.join("old").join("inst-old").join(VAULT_FILE_NAME);
        write_json(
            &legacy,
            &serde_json::json!({
                "version": 1,
                "active_instance": "inst-old",
                "instances": [{
                    "instance_id": "inst-old",
                    // 非 canonical：若迁移把旧文件写回 v2，这个尾斜杠会被改掉。
                    "relay_url": "http://127.0.0.1:18080/",
                    "vault_path": old_vault.to_string_lossy(),
                    "vault_id": "00112233445566778899aabbccddeeff",
                    "device_id": "ffeeddccbbaa99887766554433221100"
                }]
            }),
        );
        let legacy_before = std::fs::read(&legacy).expect("读旧配置");

        assert!(layout.migrate_legacy_profile().expect("v1 必须可迁移"));
        assert_eq!(
            std::fs::read(&legacy).expect("旧配置必须仍在"),
            legacy_before,
            "v1 旧文件不得被写回 v2（迁移只读旧文件）"
        );

        let loaded = layout
            .store()
            .load()
            .expect("读新配置")
            .expect("新配置存在");
        let entry = loaded.active().expect("active 合法").expect("有 active");
        assert_eq!(
            entry.relay_mode,
            RelayMode::LocalDev,
            "迁移不得改变模式（local_dev 仍是 local_dev）"
        );
        assert_eq!(
            entry.relay_url, "http://127.0.0.1:18080",
            "新文件里的 relay_url 由权威 parser canonical 化"
        );
        assert_eq!(entry.api_version, RelayApiVersion::V1);
        assert_eq!(
            entry.vault_path,
            old_vault.to_string_lossy().into_owned(),
            "vault_path 保持旧路径"
        );
        let raw = std::fs::read_to_string(&layout.config_path).expect("读新配置");
        assert!(raw.contains("\"version\": 2"), "新配置必须是 v2：{raw}");
        assert!(
            raw.contains("\"relay_mode\": \"local_dev\""),
            "模式必须原样写入：{raw}"
        );
    }

    /// 产品构建：旧 `local_dev` 配置不再适用，**跳过迁移**（不是错误页）；不写新配置、
    /// 不改旧文件，也**不**把模式悄悄改成 `remote_tls`。
    #[cfg(not(feature = "dev-sync"))]
    #[test]
    fn product_build_skips_legacy_local_dev_without_writing() {
        let root = temp_dir("migrate-local-product");
        let layout = discover_in(&root);
        let legacy = layout.legacy_config_path.clone().expect("默认布局带旧路径");
        let old_vault = root.join("old").join("inst-old").join(VAULT_FILE_NAME);
        write_json(
            &legacy,
            &serde_json::json!({
                "version": 2,
                "active_instance": "inst-old",
                "instances": [{
                    "instance_id": "inst-old",
                    "relay_mode": "local_dev",
                    "relay_url": "http://127.0.0.1:18080",
                    "vault_path": old_vault.to_string_lossy(),
                    "vault_id": "00112233445566778899aabbccddeeff",
                    "device_id": "ffeeddccbbaa99887766554433221100"
                }]
            }),
        );
        let legacy_before = std::fs::read(&legacy).expect("读旧配置");

        let migrated = layout
            .migrate_legacy_profile()
            .expect("旧 local_dev 是已知不适用，不是配置损坏");
        assert!(!migrated, "产品构建不得迁移 local_dev 实例");
        assert!(!layout.config_path.exists(), "跳过迁移时不得写新配置");
        assert_eq!(
            std::fs::read(&legacy).expect("旧配置仍在"),
            legacy_before,
            "被拒绝时旧文件必须原样保留"
        );
        let raw = std::fs::read_to_string(&legacy).expect("读旧配置");
        assert!(
            raw.contains("\"relay_mode\": \"local_dev\""),
            "不得把模式悄悄改成 remote_tls：{raw}"
        );
    }

    #[test]
    fn instance_ids_cannot_escape_the_instances_dir() {
        let layout = ProfileLayout::new("/tmp/cfg/sync.json", "/tmp/data/instances");
        for bad in ["", ".", "..", "../escape", "a/b", "a\\b"] {
            assert!(layout.instance_dir(bad).is_err(), "{bad:?} 必须被拒绝");
        }
        assert_eq!(
            layout.instance_dir("inst-abc").expect("合法 id"),
            PathBuf::from("/tmp/data/instances/inst-abc")
        );
        assert_eq!(
            layout.vault_path("inst-abc").expect("合法 id"),
            PathBuf::from("/tmp/data/instances/inst-abc/vault.sqlite3")
        );
    }

    #[test]
    fn profile_roundtrip_is_atomic_and_has_no_secret_fields() {
        let dir = temp_dir("roundtrip");
        let layout = ProfileLayout::new(dir.join("config/sync.json"), dir.join("data/instances"));
        let store = layout.store();
        assert!(store.load().expect("缺失不算错误").is_none());

        let mut profile = DevSyncProfile::default();
        profile.upsert_active(InstanceEntry {
            instance_id: "inst-a".to_owned(),
            display_name: None,
            relay_mode: RelayMode::RemoteTls,
            api_version: RelayApiVersion::V1,
            relay_url: "https://relay.example".to_owned(),
            vault_path: "/data/instances/inst-a/vault.sqlite3".to_owned(),
            vault_id: "00112233445566778899aabbccddeeff".to_owned(),
            device_id: "ffeeddccbbaa99887766554433221100".to_owned(),
        });
        store.save(&profile).expect("写配置失败");
        // 字段集合是固定的公开信息：**没有** recovery/setup secret/root 之类。
        let raw = std::fs::read_to_string(store.path()).expect("读配置失败");
        for forbidden in ["recovery", "secret", "root", "manager", "private"] {
            assert!(
                !raw.to_ascii_lowercase().contains(forbidden),
                "配置里不得出现 {forbidden}：{raw}"
            );
        }
        let loaded = store.load().expect("读回失败").expect("必须存在");
        assert_eq!(loaded, profile);
        assert_eq!(
            loaded
                .active()
                .expect("active 合法")
                .map(|e| e.instance_id.as_str()),
            Some("inst-a")
        );

        // 再写一次（覆盖）后旧内容不应残留。
        profile.upsert_active(InstanceEntry {
            instance_id: "inst-b".to_owned(),
            display_name: None,
            relay_mode: RelayMode::RemoteTls,
            api_version: RelayApiVersion::V1,
            relay_url: "https://relay.example".to_owned(),
            vault_path: "/data/instances/inst-b/vault.sqlite3".to_owned(),
            vault_id: "11112222333344445555666677778888".to_owned(),
            device_id: "88887777666655554444333322221111".to_owned(),
        });
        store.save(&profile).expect("覆盖写失败");
        let loaded = store.load().expect("读回失败").expect("必须存在");
        assert_eq!(loaded.instances.len(), 2);
        assert_eq!(loaded.active_instance.as_deref(), Some("inst-b"));
        assert_eq!(
            loaded
                .ordered_entries()
                .first()
                .map(|e| e.instance_id.as_str()),
            Some("inst-b"),
            "Welcome 列表 active 优先"
        );
    }

    #[test]
    fn corrupt_and_unsupported_profiles_are_rejected_without_rebuild() {
        let dir = temp_dir("corrupt");
        let layout = ProfileLayout::new(dir.join("sync.json"), dir.join("instances"));
        std::fs::write(layout.config_path.clone(), b"{ not json").expect("写坏配置失败");
        let error = layout.store().load().expect_err("坏 JSON 必须报错");
        assert!(matches!(error, ProfileError::Corrupt { .. }), "{error}");

        std::fs::write(
            layout.config_path.clone(),
            br#"{ "version": 99, "instances": [] }"#,
        )
        .expect("写版本失败");
        let error = layout.store().load().expect_err("版本不符必须报错");
        assert!(
            matches!(error, ProfileError::UnsupportedVersion { version: 99, .. }),
            "{error}"
        );

        std::fs::write(
            layout.config_path.clone(),
            br#"{ "version": 1, "active_instance": "inst-x", "instances": [] }"#,
        )
        .expect("写 active 失败");
        let error = layout.store().load().expect_err("active 悬空必须报错");
        assert!(
            matches!(error, ProfileError::UnknownActive { .. }),
            "{error}"
        );
    }

    /// v1（无 relay_mode）→ 显式迁移为 local_dev，并**写回 v2**。
    ///
    /// v1 的迁移目标固定是 `local_dev`，所以只在 dev-sync 构建可用；产品构建必须
    /// 明确拒绝（见紧邻的 `v1_profile_is_rejected_in_product_build`）。
    #[cfg(feature = "dev-sync")]
    #[test]
    fn v1_profile_is_migrated_to_local_dev_and_written_back_as_v2() {
        let dir = temp_dir("migrate-v1");
        let layout = ProfileLayout::new(dir.join("sync.json"), dir.join("instances"));
        std::fs::write(
            layout.config_path.clone(),
            br#"{
              "version": 1,
              "active_instance": "inst-old",
              "instances": [{
                "instance_id": "inst-old",
                "relay_url": "http://127.0.0.1:18080/",
                "vault_path": "/data/instances/inst-old/vault.sqlite3",
                "vault_id": "00112233445566778899aabbccddeeff",
                "device_id": "ffeeddccbbaa99887766554433221100"
              }]
            }"#,
        )
        .expect("写 v1 配置");

        let loaded = layout
            .store()
            .load()
            .expect("v1 必须可显式迁移")
            .expect("配置存在");
        assert_eq!(loaded.version, PROFILE_VERSION);
        let entry = loaded.active().expect("active 合法").expect("有 active");
        assert_eq!(entry.relay_mode, RelayMode::LocalDev);
        assert_eq!(
            entry.relay_url, "http://127.0.0.1:18080",
            "迁移时 canonical 化"
        );

        // 文件已经写回 v2（不是只做内存兼容）。
        let raw = std::fs::read_to_string(layout.config_path.clone()).expect("读回");
        let value: serde_json::Value = serde_json::from_str(&raw).expect("v2 JSON");
        assert_eq!(value["version"], serde_json::json!(2));
        assert_eq!(value["instances"][0]["relay_mode"], "local_dev");
        // 再读一次走 v2 分支且稳定。
        let again = layout.store().load().expect("v2 读取").expect("存在");
        assert_eq!(again, loaded);
    }

    /// 产品构建（无 dev-sync）必须拒绝 v1 配置（迁移目标 local_dev 不可用），
    /// 而不是静默打开一个产品构建无法安全使用的实例。
    #[cfg(not(feature = "dev-sync"))]
    #[test]
    fn v1_profile_is_rejected_in_product_build() {
        let dir = temp_dir("migrate-v1-product");
        let layout = ProfileLayout::new(dir.join("sync.json"), dir.join("instances"));
        std::fs::write(
            layout.config_path.clone(),
            br#"{
              "version": 1,
              "active_instance": "inst-old",
              "instances": [{
                "instance_id": "inst-old",
                "relay_url": "http://127.0.0.1:18080/",
                "vault_path": "/data/instances/inst-old/vault.sqlite3",
                "vault_id": "00112233445566778899aabbccddeeff",
                "device_id": "ffeeddccbbaa99887766554433221100"
              }]
            }"#,
        )
        .expect("写 v1 配置");

        let error = layout.store().load().expect_err("产品构建必须拒绝 v1 配置");
        assert!(
            matches!(error, ProfileError::InvalidRelay { .. }),
            "{error}"
        );
        // 失败不改写配置（不迁移、不覆盖）。
        let raw = std::fs::read_to_string(layout.config_path.clone()).expect("读回");
        assert!(raw.contains("\"version\": 1"), "失败不得改写配置：{raw}");
    }

    /// v1 里的非 local_dev relay **不能**被静默迁移（只允许显式 local_dev）。
    #[test]
    fn v1_with_non_local_relay_is_rejected_instead_of_reinterpreted() {
        let dir = temp_dir("migrate-v1-bad");
        let layout = ProfileLayout::new(dir.join("sync.json"), dir.join("instances"));
        for relay in ["https://relay.example", "http://relay.example:18080"] {
            std::fs::write(
                layout.config_path.clone(),
                format!(
                    "{{\"version\":1,\"active_instance\":\"inst-x\",\"instances\":[\
                     {{\"instance_id\":\"inst-x\",\"relay_url\":\"{relay}\",\
                     \"vault_path\":\"/data/instances/inst-x/vault.sqlite3\",\
                     \"vault_id\":\"00112233445566778899aabbccddeeff\",\
                     \"device_id\":\"ffeeddccbbaa99887766554433221100\"}}]}}"
                ),
            )
            .expect("写 v1 配置");
            let error = layout
                .store()
                .load()
                .expect_err("非 local_dev 的 v1 必须拒绝");
            assert!(
                matches!(error, ProfileError::InvalidRelay { .. }),
                "{error}"
            );
            // 失败不改写配置（用户可人工修正）。
            let raw = std::fs::read_to_string(layout.config_path.clone()).expect("读回");
            assert!(raw.contains("\"version\":1"), "失败不得改写配置：{raw}");
        }
    }

    /// 未知版本 / 未知模式必须明确拒绝，不能默认 local_dev。
    #[test]
    fn unknown_profile_versions_and_modes_are_rejected() {
        let dir = temp_dir("unknown-v2");
        let layout = ProfileLayout::new(dir.join("sync.json"), dir.join("instances"));
        std::fs::write(
            layout.config_path.clone(),
            br#"{ "version": 3, "instances": [] }"#,
        )
        .expect("写未知版本");
        let error = layout.store().load().expect_err("未知版本必须拒绝");
        assert!(
            matches!(error, ProfileError::UnsupportedVersion { version: 3, .. }),
            "{error}"
        );

        std::fs::write(
            layout.config_path.clone(),
            br#"{
              "version": 2,
              "active_instance": "inst-x",
              "instances": [{
                "instance_id": "inst-x",
                "relay_mode": "carrier_pigeon",
                "relay_url": "http://127.0.0.1:18080",
                "vault_path": "/data/instances/inst-x/vault.sqlite3",
                "vault_id": "00112233445566778899aabbccddeeff",
                "device_id": "ffeeddccbbaa99887766554433221100"
              }]
            }"#,
        )
        .expect("写未知模式");
        let error = layout.store().load().expect_err("未知模式必须拒绝");
        match error {
            ProfileError::Corrupt { detail, .. } => {
                assert!(detail.contains("未知 relay 模式"), "{detail}");
            }
            other => panic!("错误类型不对：{other}"),
        }

        // 手改出非 canonical 的 relay_url 也要拒绝（不静默改写）。
        std::fs::write(
            layout.config_path.clone(),
            br#"{
              "version": 2,
              "active_instance": "inst-x",
              "instances": [{
                "instance_id": "inst-x",
                "relay_mode": "local_dev",
                "relay_url": "http://127.0.0.1:18080/",
                "vault_path": "/data/instances/inst-x/vault.sqlite3",
                "vault_id": "00112233445566778899aabbccddeeff",
                "device_id": "ffeeddccbbaa99887766554433221100"
              }]
            }"#,
        )
        .expect("写非 canonical");
        let error = layout.store().load().expect_err("非 canonical 必须拒绝");
        assert!(
            matches!(error, ProfileError::InvalidRelay { .. }),
            "{error}"
        );
    }

    /// save 也会校验：未知模式/非法地址不会落盘。
    #[test]
    fn save_rejects_invalid_relay_before_writing() {
        let dir = temp_dir("save-invalid");
        let layout = ProfileLayout::new(dir.join("sync.json"), dir.join("instances"));
        let store = layout.store();
        let mut profile = DevSyncProfile::default();
        profile.upsert_active(InstanceEntry {
            instance_id: "inst-bad".to_owned(),
            display_name: None,
            relay_mode: RelayMode::LocalDev,
            api_version: RelayApiVersion::V1,
            // local_dev 只允许 http + loopback 白名单。
            relay_url: "https://relay.example".to_owned(),
            vault_path: "/data/instances/inst-bad/vault.sqlite3".to_owned(),
            vault_id: "00112233445566778899aabbccddeeff".to_owned(),
            device_id: "ffeeddccbbaa99887766554433221100".to_owned(),
        });
        let error = store.save(&profile).expect_err("非法 relay 不得落盘");
        assert!(
            matches!(error, ProfileError::InvalidRelay { .. }),
            "{error}"
        );
        assert!(!store.path().exists(), "失败不能留下配置");
    }

    /// ADR 0014 S3a：旧 v2 配置没有 `api_version` → 显式按 v1 打开；新 v2 配置写 2；
    /// 未知数字是明确错误（不静默降级）。
    #[test]
    fn profile_api_version_defaults_to_v1_and_roundtrips_v2() {
        let dir = temp_dir("api-version");
        let layout = ProfileLayout::new(dir.join("sync.json"), dir.join("instances"));
        std::fs::write(
            layout.config_path.clone(),
            br#"{
              "version": 2,
              "active_instance": "inst-old",
              "instances": [{
                "instance_id": "inst-old",
                "relay_mode": "remote_tls",
                "relay_url": "https://relay.example",
                "vault_path": "/data/instances/inst-old/vault.sqlite3",
                "vault_id": "00112233445566778899aabbccddeeff",
                "device_id": "ffeeddccbbaa99887766554433221100"
              }]
            }"#,
        )
        .expect("写旧 v2 配置");
        let loaded = layout.store().load().expect("读取").expect("存在");
        assert_eq!(
            loaded.instances[0].api_version,
            RelayApiVersion::V1,
            "缺 api_version 的旧配置必须按 v1 打开（既有 wire）"
        );

        // 保存一个 v2 实例：JSON 里是数字 2，读回仍是 v2。
        let mut profile = loaded;
        profile.upsert_active(InstanceEntry {
            instance_id: "inst-v2".to_owned(),
            display_name: None,
            relay_mode: RelayMode::RemoteTls,
            api_version: RelayApiVersion::V2,
            relay_url: "https://relay.example".to_owned(),
            vault_path: "/data/instances/inst-v2/vault.sqlite3".to_owned(),
            vault_id: "11112222333344445555666677778888".to_owned(),
            device_id: "88887777666655554444333322221111".to_owned(),
        });
        layout.store().save(&profile).expect("写 v2 配置");
        let raw = std::fs::read_to_string(layout.config_path.clone()).expect("读回");
        let value: serde_json::Value = serde_json::from_str(&raw).expect("v2 JSON");
        assert_eq!(value["instances"][1]["api_version"], serde_json::json!(2));
        assert_eq!(
            layout
                .store()
                .load()
                .expect("读取")
                .expect("存在")
                .find("inst-v2")
                .expect("v2 实例")
                .api_version,
            RelayApiVersion::V2
        );

        // 未知版本：明确拒绝，不默认回退。
        let broken = raw.replace("\"api_version\": 2", "\"api_version\": 3");
        std::fs::write(layout.config_path.clone(), broken).expect("写坏版本");
        let error = layout
            .store()
            .load()
            .expect_err("未知 api_version 必须拒绝");
        assert!(matches!(error, ProfileError::Corrupt { .. }), "{error}");
    }

    #[test]
    fn generated_instance_ids_are_valid_and_distinct() {
        let first = generate_instance_id();
        let second = generate_instance_id();
        assert_ne!(first, second);
        validate_instance_id(&first).expect("生成的 id 必须合法");
        validate_instance_id(&second).expect("生成的 id 必须合法");
    }

    /// 启动偏好只认 `local`/`sync`；缺失/损坏/未知值/未知版本一律 `None`（回 Welcome）。
    #[test]
    fn startup_mode_parses_only_known_values_and_tolerates_minimal_json() {
        assert_eq!(
            StartupMode::parse("{\"mode\": \"local\"}"),
            Some(StartupMode::Local),
            "字段最小化写法（没有 version）也必须接受"
        );
        assert_eq!(
            StartupMode::parse("{\"version\": 1, \"mode\": \"sync\"}"),
            Some(StartupMode::Sync)
        );
        for corrupt in [
            "",
            "{ broken",
            "[]",
            "{\"version\": 1}",
            "{\"mode\": \"welcome\"}",
            "{\"mode\": 1}",
            "{\"version\": 99, \"mode\": \"local\"}",
            "{\"version\": 1, \"mode\": null}",
        ] {
            assert_eq!(
                StartupMode::parse(corrupt),
                None,
                "损坏/未知输入不得恢复任何入口：{corrupt:?}"
            );
        }
    }

    /// 启动偏好原子写入 → 读回一致；损坏文件回退 `None`；临时文件不残留。
    #[test]
    fn startup_mode_file_round_trips_and_corruption_falls_back() {
        let dir = temp_dir("startup-pref");
        let path = dir.join(STARTUP_FILE_NAME);
        assert_eq!(load_startup_mode(&path), None, "文件不存在 = 没有偏好");

        save_startup_mode(&path, StartupMode::Local).expect("写 local 失败");
        assert_eq!(load_startup_mode(&path), Some(StartupMode::Local));
        save_startup_mode(&path, StartupMode::Sync).expect("写 sync 失败");
        assert_eq!(load_startup_mode(&path), Some(StartupMode::Sync));
        let raw = std::fs::read_to_string(&path).expect("读回失败");
        let value: serde_json::Value = serde_json::from_str(&raw).expect("必须是 JSON");
        assert_eq!(value["version"], serde_json::json!(1));
        assert_eq!(value["mode"], "sync");
        assert!(
            !path.with_extension("json.tmp").exists(),
            "原子写完成后不能留下临时文件"
        );

        std::fs::write(&path, b"{ broken").expect("写坏文件失败");
        assert_eq!(load_startup_mode(&path), None, "损坏必须回退 None");
    }

    /// 两个本机偏好文件都在数据根下（与 `local/`、`instances/` 同级）。
    #[test]
    fn startup_and_settings_ui_paths_live_in_the_data_root() {
        let root = temp_dir("preference-paths");
        let layout = ProfileLayout::new(
            root.join("config/sync.json"),
            root.join("data").join("instances"),
        );
        assert_eq!(
            layout.startup_path(),
            root.join("data").join(STARTUP_FILE_NAME)
        );
        assert_eq!(
            layout.settings_ui_path(),
            root.join("data").join(SETTINGS_UI_FILE_NAME)
        );
        assert_eq!(
            layout.startup_path().parent(),
            Some(layout.data_root().as_path()),
            "启动偏好必须与本地库/实例目录同一数据根"
        );
    }

    /// 默认 vault 根：Documents 存在 → `<home>/Documents/menghunote`；不存在 →
    /// `<home>/menghunote`。Windows 分支优先 `USERPROFILE`（不读 `HOME`）。
    #[test]
    fn user_default_vault_root_prefers_documents_and_falls_back_to_home() {
        let root = temp_dir("user-root");
        let home = root.join("home");
        std::fs::create_dir_all(&home).expect("建 home 失败");
        let home_text = home.to_string_lossy().into_owned();

        // 没有 Documents：回退 <home>/menghunote（仍是用户目录，不是 LOCALAPPDATA）。
        let unix = user_default_vault_root_for(env_of(&[("HOME", &home_text)]), false)
            .expect("HOME 可解析");
        assert_eq!(unix, home.join(APP_DIR_NAME));

        // 有 Documents：优先 Documents/menghunote。
        let documents = home.join("Documents");
        std::fs::create_dir_all(&documents).expect("建 Documents 失败");
        let unix = user_default_vault_root_for(env_of(&[("HOME", &home_text)]), false)
            .expect("HOME 可解析");
        assert_eq!(unix, documents.join(APP_DIR_NAME));

        // Windows：只认 USERPROFILE，HOME 里即使有 Documents 也不影响。
        let windows_home = root.join("windows-home");
        std::fs::create_dir_all(windows_home.join("Documents")).expect("建 Windows Documents 失败");
        let windows_text = windows_home.to_string_lossy().into_owned();
        let win = user_default_vault_root_for(
            env_of(&[
                ("USERPROFILE", &windows_text),
                ("HOME", "/definitely-wrong"),
            ]),
            true,
        )
        .expect("USERPROFILE 可解析");
        assert_eq!(win, windows_home.join("Documents").join(APP_DIR_NAME));

        // 变量缺失 → None（调用方回退数据根，不 panic）。
        assert_eq!(user_default_vault_root_for(env_of(&[]), true), None);
        assert_eq!(user_default_vault_root_for(env_of(&[]), false), None);
    }

    /// 显式布局不读平台环境：默认 vault 根 = 数据根（既有测试/注入语义）。
    #[test]
    fn explicit_layout_keeps_data_root_defaults_and_never_reads_localappdata() {
        let root = temp_dir("explicit-defaults");
        let layout = ProfileLayout::new(
            root.join("config/sync.json"),
            root.join("data").join("instances"),
        );
        let data_root = root.join("data");
        assert_eq!(layout.default_vault_root(), data_root);
        assert_eq!(
            layout.default_instance_dir("inst-x").expect("合法 id"),
            data_root.join("instances").join("inst-x")
        );
        assert_eq!(
            layout.default_instance_dir("inst-x").expect("合法 id"),
            layout.instance_dir("inst-x").expect("合法 id"),
            "显式布局下默认实例目录与实例目录必须一致（不破坏既有测试）"
        );
        assert_eq!(
            layout.default_local_vault_path(),
            layout.local_vault_path(),
            "显式布局下本地库默认位置保持数据根（既有测试契约）"
        );
        assert!(
            !layout
                .default_local_vault_path()
                .to_string_lossy()
                .to_lowercase()
                .contains("localappdata"),
            "默认本地库路径不得落在 LOCALAPPDATA"
        );
        assert!(
            layout.default_instance_dir("..").is_err(),
            "默认实例目录仍必须校验 id（防路径穿越）"
        );
    }

    /// 平台默认布局：vault 默认根在用户文档目录（不是 LOCALAPPDATA）。
    #[cfg(not(windows))]
    #[test]
    fn discovered_layout_uses_user_documents_instead_of_localappdata() {
        let root = temp_dir("discover-user-docs");
        let home = root.join("home");
        let documents = home.join("Documents");
        std::fs::create_dir_all(&documents).expect("建 Documents 失败");
        let layout = discover_in(&root);

        assert_eq!(
            layout.default_vault_root(),
            documents.join(APP_DIR_NAME),
            "Discover 的默认 vault 根必须在用户 Documents 下"
        );
        assert_eq!(
            layout.default_instance_dir("inst-new").expect("合法 id"),
            documents
                .join(APP_DIR_NAME)
                .join("instances")
                .join("inst-new")
        );
        assert_eq!(
            layout.default_local_vault_path(),
            documents
                .join(APP_DIR_NAME)
                .join("local")
                .join(VAULT_FILE_NAME)
        );
        for path in [
            layout.default_instance_dir("inst-new").expect("合法 id"),
            layout.default_local_vault_path(),
        ] {
            let text = path.to_string_lossy().to_lowercase();
            assert!(
                !text.contains("localappdata"),
                "不得默认写 LOCALAPPDATA：{text}"
            );
            assert!(!text.contains("appdata"), "不得默认写 AppData：{text}");
        }
        // 配置仍留平台配置目录（小、公开），只有 vault 数据移动到用户目录。
        #[cfg(not(windows))]
        assert_eq!(
            layout.config_path,
            root.join("config")
                .join(APP_DIR_NAME)
                .join(PROFILE_FILE_NAME)
        );
        // 旧数据根 local/ 路径仍由 `local_vault_path()` 提供（兼容旧用户）。
        assert_eq!(
            layout.local_vault_path(),
            root.join("data")
                .join(APP_DIR_NAME)
                .join(LOCAL_DIR_NAME)
                .join(VAULT_FILE_NAME)
        );
    }

    /// `startup.json`：新格式带 `local_vault_path` 可往返；旧格式（只有
    /// `{version,mode}`）仍可读，local 路径为 `None`；`save_startup_mode` 不丢已记路径。
    #[test]
    fn startup_preferences_round_trip_and_legacy_format_is_readable() {
        let dir = temp_dir("startup-preferences");
        let path = dir.join(STARTUP_FILE_NAME);
        let local_path = dir.join("Documents/menghunote/local/vault.sqlite3");

        save_startup_preferences(
            &path,
            &StartupPreferences::with_local_vault_path(StartupMode::Local, local_path.clone()),
        )
        .expect("写新格式失败");
        let loaded = load_startup_preferences(&path).expect("新格式必须可读");
        assert_eq!(loaded.mode, StartupMode::Local);
        assert_eq!(loaded.local_vault_path, Some(local_path.clone()));
        // 旧 API 仍只读 mode。
        assert_eq!(load_startup_mode(&path), Some(StartupMode::Local));

        // 打开同步实例：只换 mode，保留上次本地路径。
        save_startup_mode(&path, StartupMode::Sync).expect("写 sync 失败");
        let loaded = load_startup_preferences(&path).expect("同步格式必须可读");
        assert_eq!(loaded.mode, StartupMode::Sync);
        assert_eq!(
            loaded.local_vault_path,
            Some(local_path),
            "切到同步入口不得忘记上次本地库路径"
        );

        // 旧格式（没有 local_vault_path）：仍能读，路径为 None。
        std::fs::write(&path, b"{\n  \"version\": 1,\n  \"mode\": \"local\"\n}\n")
            .expect("写旧格式失败");
        let legacy = load_startup_preferences(&path).expect("旧格式必须可读");
        assert_eq!(legacy.mode, StartupMode::Local);
        assert_eq!(legacy.local_vault_path, None);
        assert_eq!(
            StartupMode::parse("{\"mode\":\"local\"}"),
            Some(StartupMode::Local)
        );

        // 空串路径视为没有记录（不把空路径写进配置/启动逻辑）。
        std::fs::write(
            &path,
            b"{\"version\":1,\"mode\":\"local\",\"local_vault_path\":\"\"}",
        )
        .expect("写空路径失败");
        assert_eq!(
            load_startup_preferences(&path)
                .expect("必须可读")
                .local_vault_path,
            None
        );

        // 损坏/未知版本仍回退 None。
        for corrupt in [
            "{ broken",
            "{\"version\":99,\"mode\":\"local\"}",
            "{\"mode\":\"welcome\"}",
        ] {
            std::fs::write(&path, corrupt).expect("写坏偏好失败");
            assert_eq!(load_startup_preferences(&path), None, "{corrupt:?}");
        }
    }

    // ---- 用户自定义仓库名称（display_name，只影响显示）----

    fn entry(instance_id: &str, vault_dir: &str) -> InstanceEntry {
        InstanceEntry {
            instance_id: instance_id.to_owned(),
            display_name: None,
            relay_mode: RelayMode::RemoteTls,
            api_version: RelayApiVersion::V1,
            relay_url: "https://relay.example".to_owned(),
            vault_path: format!("{vault_dir}/vault.sqlite3"),
            vault_id: "00112233445566778899aabbccddeeff".to_owned(),
            device_id: "ffeeddccbbaa99887766554433221100".to_owned(),
        }
    }

    /// 旧 v2 配置（没有 `display_name`）照常可读；`version` 仍是 2（不升版本）。
    #[test]
    fn v2_profile_without_display_name_is_readable_and_keeps_version_two() {
        let dir = temp_dir("display-name-legacy");
        let layout = ProfileLayout::new(dir.join("sync.json"), dir.join("instances"));
        std::fs::write(
            layout.config_path.clone(),
            br#"{
              "version": 2,
              "active_instance": "inst-old",
              "instances": [{
                "instance_id": "inst-old",
                "relay_mode": "remote_tls",
                "api_version": 2,
                "relay_url": "https://relay.example",
                "vault_path": "/data/instances/inst-old/vault.sqlite3",
                "vault_id": "00112233445566778899aabbccddeeff",
                "device_id": "ffeeddccbbaa99887766554433221100"
              }]
            }"#,
        )
        .expect("写旧 v2 配置失败");

        let loaded = layout
            .store()
            .load()
            .expect("旧 v2 必须可读")
            .expect("存在");
        assert_eq!(loaded.version, PROFILE_VERSION);
        assert_eq!(loaded.version, 2, "只增可选字段，配置版本仍为 2");
        let entry = loaded.active().expect("active 合法").expect("有 active");
        assert_eq!(entry.display_name, None, "缺省字段必须是 None");
        assert_eq!(entry.effective_name(), "inst-old", "显示层回退目录名");
        assert_eq!(loaded.instances.len(), 1);
    }

    /// 写入/读回：名字落盘为 `display_name`，且**不碰**路径/身份/active/vault 目录。
    #[test]
    fn display_name_round_trips_without_touching_vault_identity_or_active() {
        let dir = temp_dir("display-name-roundtrip");
        let layout = ProfileLayout::new(dir.join("sync.json"), dir.join("instances"));
        let store = layout.store();
        let mut profile = DevSyncProfile::default();
        profile.upsert_active(entry("inst-a", "/data/instances/inst-a"));
        store.save(&profile).expect("写配置失败");

        let before = store.load().expect("读回").expect("存在");
        let mut edited = before.clone();
        edited
            .rename_instance("inst-a", Some("  研究库 / Research  "))
            .expect("改名");
        store.save(&edited).expect("写改名后的配置失败");

        let loaded = store.load().expect("读回").expect("存在");
        let a = loaded.find("inst-a").expect("条目仍在");
        assert_eq!(
            a.display_name.as_deref(),
            Some("研究库 / Research"),
            "保存前必须 trim（中文/空格/下划线都保留）"
        );
        assert_eq!(a.effective_name(), "研究库 / Research");
        assert_eq!(
            (
                a.vault_path.as_str(),
                a.vault_id.as_str(),
                a.device_id.as_str()
            ),
            (
                "/data/instances/inst-a/vault.sqlite3",
                "00112233445566778899aabbccddeeff",
                "ffeeddccbbaa99887766554433221100"
            ),
            "改名只动 display_name：路径/身份原样保留"
        );
        assert_eq!(loaded.active_instance.as_deref(), Some("inst-a"));
        assert_eq!(loaded.version, 2);
        // 原库目录没有任何写入（测试里本来就只有配置文件）。
        assert!(store.path().is_file());
        let raw = std::fs::read_to_string(store.path()).expect("读配置文本");
        assert!(
            raw.contains("\"display_name\": \"研究库 / Research\""),
            "配置里必须有 display_name：{raw}"
        );
    }

    /// 校验：trim/空白 → None；超长与超过 64 字符拒绝；控制字符拒绝；中文/emoji 合法。
    #[test]
    fn display_name_normalization_bounds_and_control_chars() {
        assert_eq!(normalize_display_name("").expect("空合法"), None);
        assert_eq!(normalize_display_name("   ").expect("纯空白合法"), None);
        assert_eq!(normalize_display_name("\t\n ").expect("纯空白合法"), None);
        assert_eq!(
            normalize_display_name("  我的库  ").expect("trim 合法"),
            Some("我的库".to_owned())
        );
        // 中文 + 空格 + emoji + 下划线都允许（与文件名无关，只做显示）。
        assert_eq!(
            normalize_display_name("工作 笔记 🚀_v2").expect("中文/emoji 合法"),
            Some("工作 笔记 🚀_v2".to_owned())
        );
        // 边界：64 个字符可以，65 个拒绝。
        let exactly = "库".repeat(DISPLAY_NAME_MAX_CHARS);
        assert_eq!(
            normalize_display_name(&exactly)
                .expect("64 字符合法")
                .map(|name| name.chars().count()),
            Some(DISPLAY_NAME_MAX_CHARS)
        );
        let too_long = "库".repeat(DISPLAY_NAME_MAX_CHARS + 1);
        let error = normalize_display_name(&too_long).expect_err("超长必须拒绝");
        assert!(
            matches!(error, ProfileError::InvalidDisplayName { .. }),
            "{error}"
        );
        assert!(error.to_string().contains("64"), "{error}");
        // 控制字符（换行/制表/ESC）拒绝，不允许把换行画进按钮。
        for bad in ["a\nb", "a\tb", "\u{7f}", "a\u{1b}b"] {
            assert!(
                matches!(
                    normalize_display_name(bad),
                    Err(ProfileError::InvalidDisplayName { .. })
                ),
                "{bad:?} 必须被拒绝"
            );
        }
    }

    /// 改名 API：只改目标实例；`None`/空白清除；未知实例报错；不自动新增条目。
    #[test]
    fn rename_instance_only_touches_the_target_entry() {
        let mut profile = DevSyncProfile::default();
        profile.upsert_active(entry("inst-a", "/data/instances/inst-a"));
        profile
            .instances
            .push(entry("inst-b", "/data/instances/inst-b"));
        let before_b = profile.find("inst-b").cloned().expect("有 B");

        profile
            .rename_instance("inst-a", Some("甲库"))
            .expect("改名成功");
        assert_eq!(
            profile.find("inst-a").and_then(|e| e.display_name.clone()),
            Some("甲库".to_owned())
        );
        assert_eq!(
            profile.find("inst-b").cloned(),
            Some(before_b.clone()),
            "不得改动其它实例"
        );
        assert_eq!(profile.active_instance.as_deref(), Some("inst-a"));
        assert_eq!(profile.instances.len(), 2);

        // 空白 = 清除自定义名（回退目录名）。
        profile
            .rename_instance("inst-a", Some("   "))
            .expect("空白合法");
        assert_eq!(
            profile.find("inst-a").and_then(|e| e.display_name.clone()),
            None
        );
        assert_eq!(
            profile.find("inst-a").map(InstanceEntry::effective_name),
            Some("inst-a".to_owned())
        );
        profile.rename_instance("inst-a", None).expect("None 合法");
        assert_eq!(
            profile.find("inst-a").and_then(|e| e.display_name.clone()),
            None
        );

        // 非法名字不改动任何东西。
        assert!(matches!(
            profile.rename_instance("inst-a", Some("a\nb")),
            Err(ProfileError::InvalidDisplayName { .. })
        ));
        assert_eq!(
            profile.find("inst-a").and_then(|e| e.display_name.clone()),
            None,
            "非法输入不得落进内存配置"
        );

        // 未知实例：明确报错，不新增条目。
        let error = profile
            .rename_instance("inst-missing", Some("x"))
            .expect_err("未知实例必须报错");
        assert!(
            matches!(
                error,
                ProfileError::UnknownInstance { ref instance_id } if instance_id == "inst-missing"
            ),
            "{error}"
        );
        assert_eq!(profile.instances.len(), 2, "不得自动新增条目");
    }

    /// 手改出的非法 display_name 在读取时明确报错（不静默改写配置）。
    #[test]
    fn stored_display_name_must_be_normalized() {
        let dir = temp_dir("display-name-invalid-stored");
        let layout = ProfileLayout::new(dir.join("sync.json"), dir.join("instances"));
        for (bad, label) in [("  前后空白  ", "首尾空白"), ("换\n行", "控制字符")] {
            let json = format!(
                r#"{{
                  "version": 2,
                  "active_instance": "inst-a",
                  "instances": [{{
                    "instance_id": "inst-a",
                    "display_name": "{bad}",
                    "relay_mode": "remote_tls",
                    "relay_url": "https://relay.example",
                    "vault_path": "/data/instances/inst-a/vault.sqlite3",
                    "vault_id": "00112233445566778899aabbccddeeff",
                    "device_id": "ffeeddccbbaa99887766554433221100"
                  }}]
                }}"#
            );
            std::fs::write(layout.config_path.clone(), json).expect("写配置失败");
            let error = layout
                .store()
                .load()
                .expect_err(&format!("{label} 必须被拒绝"));
            assert!(
                matches!(
                    error,
                    ProfileError::InvalidDisplayName { .. } | ProfileError::Corrupt { .. }
                ),
                "{label}: {error}"
            );
        }
        // 合法名字仍可读。
        std::fs::write(
            layout.config_path.clone(),
            r#"{
              "version": 2,
              "active_instance": "inst-a",
              "instances": [{
                "instance_id": "inst-a",
                "display_name": "OK 库",
                "relay_mode": "remote_tls",
                "relay_url": "https://relay.example",
                "vault_path": "/data/instances/inst-a/vault.sqlite3",
                "vault_id": "00112233445566778899aabbccddeeff",
                "device_id": "ffeeddccbbaa99887766554433221100"
              }]
            }"#,
        )
        .expect("写合法配置失败");
        let loaded = layout.store().load().expect("合法名字可读").expect("存在");
        assert_eq!(
            loaded
                .find("inst-a")
                .and_then(|e| e.display_name.as_deref()),
            Some("OK 库")
        );
    }

    /// `save` 对内存里未归一化的名字也做归一化（trim/空白 → None），不把坏值落盘。
    #[test]
    fn save_normalizes_in_memory_display_name() {
        let dir = temp_dir("display-name-save-normalize");
        let layout = ProfileLayout::new(dir.join("sync.json"), dir.join("instances"));
        let store = layout.store();
        let mut profile = DevSyncProfile::default();
        let mut a = entry("inst-a", "/data/instances/inst-a");
        a.display_name = Some("  甲库  ".to_owned());
        profile.upsert_active(a);
        store.save(&profile).expect("写配置失败");
        let loaded = store.load().expect("读回").expect("存在");
        assert_eq!(
            loaded
                .find("inst-a")
                .and_then(|e| e.display_name.as_deref()),
            Some("甲库")
        );

        // 纯空白写盘 = None（不出现空字符串的自定义名）。
        let mut profile = loaded;
        profile.instances[0].display_name = Some("   ".to_owned());
        store.save(&profile).expect("写配置失败");
        let raw = std::fs::read_to_string(store.path()).expect("读配置文本");
        assert!(
            raw.contains("\"display_name\": null"),
            "空白名字必须落成 null：{raw}"
        );
        assert_eq!(
            store
                .load()
                .expect("读回")
                .expect("存在")
                .find("inst-a")
                .and_then(|e| e.display_name.clone()),
            None
        );

        // 超长/控制字符在写前就失败（旧文件原样保留）。
        let before = std::fs::read(store.path()).expect("读旧文件");
        let mut profile = DevSyncProfile::default();
        let mut a = entry("inst-a", "/data/instances/inst-a");
        a.display_name = Some("x".repeat(DISPLAY_NAME_MAX_CHARS + 1));
        profile.upsert_active(a);
        let error = store.save(&profile).expect_err("超长必须拒绝");
        assert!(
            matches!(error, ProfileError::InvalidDisplayName { .. }),
            "{error}"
        );
        assert_eq!(
            std::fs::read(store.path()).expect("读旧文件"),
            before,
            "写失败不得动旧配置"
        );
    }
}
