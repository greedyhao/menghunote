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
//! ## 默认位置（ADR 0011）
//!
//! | 平台 | 数据 | 配置 |
//! | --- | --- | --- |
//! | Windows | `%LOCALAPPDATA%\menghunote-dev\instances\<id>\` | `%LOCALAPPDATA%\menghunote-dev\dev-sync.json` |
//! | Linux | `$XDG_DATA_HOME/menghunote-dev/instances/<id>/` | `$XDG_CONFIG_HOME/menghunote-dev/dev-sync.json` |
//!
//! 环境变量缺失时分别回退 `~/.local/share` 与 `~/.config`；**HOME 也没有就报错**，
//! 绝不回退 cwd/tmp（真实使用把库留在临时目录是数据丢失）。测试用
//! [`ProfileLayout::new`] 显式注入 `target/tmp` 下的路径。
//!
//! ## 配置结构
//!
//! ```json
//! { "version": 2, "active_instance": "inst-...",
//!   "instances": [{ "instance_id": "...", "relay_mode": "local_dev", "relay_url": "...",
//!                   "vault_path": "...", "vault_id": "...", "device_id": "..." }] }
//! ```
//!
//! v1（没有 `relay_mode`）只在 [`ProfileStore::load`] 里**显式迁移**为 `local_dev`
//! 并写回 v2；未知版本/模式一律明确报错（ADR 0013 §1）。
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

/// 实例目录里固定的库文件名。
pub const VAULT_FILE_NAME: &str = "vault.sqlite3";

/// 配置文件名。
pub const PROFILE_FILE_NAME: &str = "dev-sync.json";

/// 默认实例目录（`instances/`）。
const INSTANCES_DIR_NAME: &str = "instances";

/// 产品级目录名（与 relay/服务无关，只是本机目录约定）。
const APP_DIR_NAME: &str = "menghunote-dev";

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
    /// `dev-sync.json` 的完整路径。
    pub config_path: PathBuf,
    /// 默认实例目录的上一级（`instances/`）。
    pub instances_dir: PathBuf,
}

impl ProfileLayout {
    /// 显式指定配置路径与实例目录（测试注入；生产走 [`Self::discover`]）。
    #[must_use]
    pub fn new(config_path: impl Into<PathBuf>, instances_dir: impl Into<PathBuf>) -> Self {
        Self {
            config_path: config_path.into(),
            instances_dir: instances_dir.into(),
        }
    }

    /// 按平台约定解析默认目录（env 读取闭包便于确定性测试）。
    ///
    /// 缺失 HOME/LOCALAPPDATA 是**错误**：真实使用不允许回退 cwd/tmp。
    pub fn discover(env: impl Fn(&str) -> Option<String>) -> Result<Self, ProfileError> {
        let env = |key: &str| {
            env(key)
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        };
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
            let root = PathBuf::from(local).join(APP_DIR_NAME);
            Ok(Self::new(
                root.join(PROFILE_FILE_NAME),
                root.join(INSTANCES_DIR_NAME),
            ))
        }
        #[cfg(not(windows))]
        {
            let home = env("HOME").ok_or(ProfileError::NoHome("HOME"))?;
            let home = PathBuf::from(home);
            let data_home = env("XDG_DATA_HOME")
                .map_or_else(|| home.join(".local").join("share"), PathBuf::from);
            let config_home =
                env("XDG_CONFIG_HOME").map_or_else(|| home.join(".config"), PathBuf::from);
            Ok(Self::new(
                config_home.join(APP_DIR_NAME).join(PROFILE_FILE_NAME),
                data_home.join(APP_DIR_NAME).join(INSTANCES_DIR_NAME),
            ))
        }
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

    /// 配置存储句柄。
    #[must_use]
    pub fn store(&self) -> ProfileStore {
        ProfileStore {
            path: self.config_path.clone(),
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

/// 一个已保存实例（**只公开信息**）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceEntry {
    /// 本机实例 id（与服务器 `vault_id` 分离）。
    pub instance_id: String,
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
                Ok(Some(profile))
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
                // 显式迁移：v1 只允许变成 local_dev，并立即写回 v2（失败即错误）。
                self.save(&profile)?;
                Ok(Some(profile))
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
        // 手改的非法值在写回前就失败（不把坏值落盘）。
        for entry in &mut profile.instances {
            validate_instance_id(&entry.instance_id)?;
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
            || "dev-sync.json".to_owned(),
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

/// v2 配置校验：index、实例 id、模式/URL 与 canonical 形态。
fn validate_profile(profile: &DevSyncProfile) -> Result<(), ProfileError> {
    let _ = profile.active()?;
    for entry in &profile.instances {
        validate_instance_id(&entry.instance_id)?;
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
            PathBuf::from("/tmp/config/menghunote-dev/dev-sync.json")
        );
        assert_eq!(
            layout.instances_dir,
            PathBuf::from("/tmp/data/menghunote-dev/instances")
        );

        let layout = ProfileLayout::discover(env_of(&[("HOME", "/home/alice")]))
            .expect("只有 HOME 时用 XDG 默认回退");
        assert_eq!(
            layout.config_path,
            PathBuf::from("/home/alice/.config/menghunote-dev/dev-sync.json")
        );
        assert_eq!(
            layout.instances_dir,
            PathBuf::from("/home/alice/.local/share/menghunote-dev/instances")
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
            PathBuf::from(r"C:\Users\alice\AppData\Local\menghunote-dev\dev-sync.json")
        );
        assert_eq!(
            layout.instances_dir,
            PathBuf::from(r"C:\Users\alice\AppData\Local\menghunote-dev\instances")
        );
    }

    #[test]
    fn instance_ids_cannot_escape_the_instances_dir() {
        let layout = ProfileLayout::new("/tmp/cfg/dev-sync.json", "/tmp/data/instances");
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
        let layout =
            ProfileLayout::new(dir.join("config/dev-sync.json"), dir.join("data/instances"));
        let store = layout.store();
        assert!(store.load().expect("缺失不算错误").is_none());

        let mut profile = DevSyncProfile::default();
        profile.upsert_active(InstanceEntry {
            instance_id: "inst-a".to_owned(),
            relay_mode: RelayMode::LocalDev,
            api_version: RelayApiVersion::V1,
            relay_url: "http://127.0.0.1:18080".to_owned(),
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
            relay_mode: RelayMode::LocalDev,
            api_version: RelayApiVersion::V1,
            relay_url: "http://localhost:18080".to_owned(),
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
        let layout = ProfileLayout::new(dir.join("dev-sync.json"), dir.join("instances"));
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
    #[test]
    fn v1_profile_is_migrated_to_local_dev_and_written_back_as_v2() {
        let dir = temp_dir("migrate-v1");
        let layout = ProfileLayout::new(dir.join("dev-sync.json"), dir.join("instances"));
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

    /// v1 里的非 local_dev relay **不能**被静默迁移（只允许显式 local_dev）。
    #[test]
    fn v1_with_non_local_relay_is_rejected_instead_of_reinterpreted() {
        let dir = temp_dir("migrate-v1-bad");
        let layout = ProfileLayout::new(dir.join("dev-sync.json"), dir.join("instances"));
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
        let layout = ProfileLayout::new(dir.join("dev-sync.json"), dir.join("instances"));
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
        let layout = ProfileLayout::new(dir.join("dev-sync.json"), dir.join("instances"));
        let store = layout.store();
        let mut profile = DevSyncProfile::default();
        profile.upsert_active(InstanceEntry {
            instance_id: "inst-bad".to_owned(),
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
        let layout = ProfileLayout::new(dir.join("dev-sync.json"), dir.join("instances"));
        std::fs::write(
            layout.config_path.clone(),
            br#"{
              "version": 2,
              "active_instance": "inst-old",
              "instances": [{
                "instance_id": "inst-old",
                "relay_mode": "local_dev",
                "relay_url": "http://127.0.0.1:18080",
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
            relay_mode: RelayMode::LocalDev,
            api_version: RelayApiVersion::V2,
            relay_url: "http://127.0.0.1:18080".to_owned(),
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
}
