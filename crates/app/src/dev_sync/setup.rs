//! dev-sync 的**启动生命周期**（ADR 0009 Phase 1 / 切片 4c-1）。
//!
//! ## 为什么要有这一层
//!
//! 切片 2a 的启动路径在 `ensure_instance_vault_ready → load_or_create` 时**自动造占位
//! 身份**：它能保证 peer 稳定，但身份先于 Create/Join 出现，vault_id / 管理私钥与真实
//! relay 身份无关，Join 也就无从谈起。切片 4c-1 改成三种**显式**模式：
//!
//! | 模式 | 触发 | 行为 |
//! | --- | --- | --- |
//! | `Create` | `--dev-sync-create` | 对**不存在**的库路径跑 `DevLocal::create`（genesis）→ 持久化真实身份 |
//! | `Join` | `--dev-sync-join` | 只凭 `vault_id + 恢复码` 跑 `DevLocal::join` → 持久化本设备身份 |
//! | `Existing` | 普通 `--dev-sync` | **只** `DevIdentityStore::load`，身份缺失明确报错 |
//!
//! 三种模式互斥（Create/Join 两个 flag 同时出现直接拒绝）；Create/Join 隐含启用 dev-sync。
//!
//! ## 顺序（硬约束）
//!
//! `prepare` 内部固定是：新路径检查 → `DevLocal::{create,join}`（真实 HTTP，先联网后落地）
//! → `DevIdentityStore::persist_protected` → 返回库句柄。`main` 随后才
//! [`open_vault_at_with_peer`] 重建窗口句柄并开窗——**身份没落盘就绝不开窗**。
//!
//! ## 不开窗的两个 CLI 子命令（切片 4c-3）
//!
//! `--dev-sync-info` / `--dev-sync-publish` 与三种启动模式**同层互斥**，但它们不走
//! [`prepare`]：`main` 在开窗前把它们交给 [`super::cli`]，打印完即退出（不开窗、不初始化
//! GPUI）。`--dev-sync-info` 纯只读、不联网；`--dev-sync-publish` 复用已有实例的
//! 身份 + 库调 `DevLocal::publish_initial_pair`（见 [`super::cli::publish_baseline`]）。
//!
//! ## 失败清理策略
//!
//! Create/Join 只接受**全新路径**（库文件与同目录 `dev-identity.bin` 都不存在），
//! 所以任何一步失败都不会破坏旧身份/旧库：
//!
//! - `DevLocal::{create,join}` 自己会清掉它刚建出的库文件（见 `crates/sync/src/dev_local.rs`）；
//! - 库已建出、但 `persist_protected` 失败时，这里 drop 库句柄后**再清一次**库文件
//!   （含 WAL/SHM/journal 边车）与本次可能已 rename 出来的身份文件，然后 fail-fast；
//! - 实例目录本身**不删**（可能是用户已有的空目录），错误信息里给出目录路径。
//!
//! relay 侧可能留下一个没有槽/没有 checkpoint 的空 vault（不可用），重试会建新的；
//! 这与 `DevLocal::create` 的既有失败语义一致。
//!
//! ## 安全
//!
//! - setup secret 与恢复码**只从环境变量读**（绝不进 argv），本模块不打印它们，
//!   错误文案也不回显（恢复码只报长度，不报内容）；
//! - 日志只报模式/路径/是否已存在（由 `main` 打印）。

use std::path::{Path, PathBuf};

use anyhow::{bail, Context as _};
use menghunote_storage::Vault;
use menghunote_sync::dev_local::DevLocal;
use menghunote_sync::identity::LocalIdentityV1;
use menghunote_sync::ids;
use menghunote_sync::init_journal::load_pending;
use menghunote_sync::init_session::{self, reconcile};
use menghunote_sync::VaultId;

use super::gate::{DevSyncConfig, CREATE_FLAG, ENABLE_FLAG, INFO_FLAG, JOIN_FLAG, PUBLISH_FLAG};
use super::identity::{ensure_instance_vault_ready, DevIdentityStore};

/// Create 模式的一次性 setup secret（genesis 用；**不得**放进 argv）。
pub const SETUP_SECRET_ENV: &str = "MENGHUNOTE_DEV_SETUP_SECRET";
/// Join 模式的 vault id（32 位十六进制；公开标识，不是秘密）。
pub const VAULT_ID_ENV: &str = "MENGHUNOTE_DEV_VAULT_ID";
/// Join 模式的恢复码（`mnrec1...`；**不得**放进 argv/日志）。
pub const RECOVERY_CODE_ENV: &str = "MENGHUNOTE_DEV_RECOVERY_CODE";

/// Create 写进本机 meta 的实例标签（非 wire，仅本机展示；Join 不导入）。
pub(crate) const INSTANCE_LABEL: &str = "menghunote dev-sync 实例";

/// 启动生命周期错误：app 层统一用 `anyhow` 汇总，底层 `SyncError`/io 错误链保留在
/// `Display` 的 `{:#}` 输出里（`main` 就是这样打印的）。
pub type SetupError = anyhow::Error;

/// 启动模式（Create/Join/复用已有身份 + 两个不开窗子命令，五者互斥）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupMode {
    /// 普通 `--dev-sync`：库与身份都必须已经存在，**只读身份、不创建**。
    Existing,
    /// `--dev-sync-create`：新建 vault + 真实 genesis 身份。
    Create,
    /// `--dev-sync-join`：只用 vault_id + 恢复码加入已有 vault。
    Join,
    /// `--dev-sync-info`：只打印公开标识（不开窗、不联网、不创建）。
    Info,
    /// `--dev-sync-publish`：对已有实例发布初始基线（不开窗）。
    Publish,
}

/// 不开窗的 CLI 子命令（切片 4c-3）。
///
/// 单独一个枚举的原因：它把"需要 `prepare` 建窗口生命周期"与"打印完就退出"在**类型上**
/// 分开，`main` 不会因为漏写一个 `match` 分支把 `Info` 当成启动模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliMode {
    /// `--dev-sync-info`：只读身份/库，打印公开标识。
    Info,
    /// `--dev-sync-publish`：发布初始基线。
    Publish,
}

impl SetupMode {
    /// 启动日志用的稳定短标签（不含任何秘密）。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Existing => "existing（复用已有身份）",
            Self::Create => "create（新建 vault + 身份）",
            Self::Join => "join（凭 vault_id + 恢复码加入）",
            Self::Info => "info（打印公开标识，不开窗/不联网）",
            Self::Publish => "publish（发布初始基线，不开窗）",
        }
    }

    /// 该模式是否是不开窗的 CLI 子命令；`Some` 时 `main` **不得**调用 [`prepare`]。
    #[must_use]
    pub const fn cli_mode(self) -> Option<CliMode> {
        match self {
            Self::Existing | Self::Create | Self::Join => None,
            Self::Info => Some(CliMode::Info),
            Self::Publish => Some(CliMode::Publish),
        }
    }
}

/// 从环境变量读出的启动材料。
///
/// **不实现 `Debug`**：`setup_secret` / `recovery_code` 都是秘密材料，避免被顺手打印。
#[derive(Default)]
pub struct SetupSecrets {
    /// `MENGHUNOTE_DEV_SETUP_SECRET`（Create 必需）。
    pub setup_secret: Option<String>,
    /// `MENGHUNOTE_DEV_VAULT_ID`（Join 必需，公开标识）。
    pub vault_id: Option<String>,
    /// `MENGHUNOTE_DEV_RECOVERY_CODE`（Join 必需，秘密）。
    pub recovery_code: Option<String>,
}

/// prepare 的产物：已落盘身份的库句柄 + 身份。
///
/// **不实现 `Debug`**：`identity` 是秘密材料。
pub struct SetupOutcome {
    /// 已打开的本地库（Create/Join 由 `DevLocal` 建出；Existing 是只读打开后的句柄）。
    ///
    /// 它只用于启动阶段的身份/数据面；窗口句柄由 `main` 用
    /// [`menghunote_app::dev_vault::open_vault_at_with_peer`] 重建（负责播种演示树）。
    pub vault: Vault,
    /// 持久身份（peer/epoch/root 的唯一来源）。
    pub identity: LocalIdentityV1,
    /// 本次库的 vault id（Create 新建 / Join 校验 / Existing 从身份读出）。
    pub vault_id: VaultId,
    /// Create 时 genesis 返回的一次性恢复码；Join/Existing 为 `None`。
    ///
    /// **不打印、不落盘**：切片 4c-2 的面板负责临时展示并在提交后清除；本切片只把它
    /// 交回进程内（测试/后续 worker 用）。
    pub recovery_code: Option<String>,
}

/// 解析五种模式（纯函数，便于确定性测试）。
///
/// **五者互斥**：任意两个模式 flag 同时出现 → [`SetupError`]；都没出现 →
/// [`SetupMode::Existing`]。普通 [`ENABLE_FLAG`] 与任何模式 flag 并存不算冲突
/// （前者只是总开关）。
pub fn resolve_mode<I, S>(args: I) -> Result<SetupMode, SetupError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut create = false;
    let mut join = false;
    let mut info = false;
    let mut publish = false;
    for arg in args {
        match arg.as_ref() {
            CREATE_FLAG => create = true,
            JOIN_FLAG => join = true,
            INFO_FLAG => info = true,
            PUBLISH_FLAG => publish = true,
            _ => {}
        }
    }
    let selected: Vec<&str> = [
        (create, CREATE_FLAG),
        (join, JOIN_FLAG),
        (info, INFO_FLAG),
        (publish, PUBLISH_FLAG),
    ]
    .into_iter()
    .filter_map(|(is_set, flag)| is_set.then_some(flag))
    .collect();
    match selected.as_slice() {
        [one] => Ok(match *one {
            CREATE_FLAG => SetupMode::Create,
            JOIN_FLAG => SetupMode::Join,
            INFO_FLAG => SetupMode::Info,
            PUBLISH_FLAG => SetupMode::Publish,
            other => bail!("未预期的 dev-sync 模式 flag：{other}"),
        }),
        [] => Ok(SetupMode::Existing),
        many => bail!(
            "{} 互斥：一次启动只能选一种初始化模式或子命令\
             （复用已有实例请只用 {ENABLE_FLAG}）",
            many.join(" / ")
        ),
    }
}

/// 从环境读取启动材料（纯函数：测试传固定表，不改进程环境）。
///
/// 只取值，不做校验；缺失/空值由 [`prepare`] 按模式明确报错。
#[must_use]
pub fn secrets_from_env(env: impl Fn(&str) -> Option<String>) -> SetupSecrets {
    SetupSecrets {
        setup_secret: env(SETUP_SECRET_ENV),
        vault_id: env(VAULT_ID_ENV),
        recovery_code: env(RECOVERY_CODE_ENV),
    }
}

/// 启动生命周期主入口：按模式把"身份/库就绪"这件事做完，然后返回库与身份。
///
/// 顺序与失败清理见模块文档。调用方在成功后**必须**用返回身份的 peer 打开窗口
/// （`open_vault_at_with_peer` + `AppShell::new_with_dev_sync`），不得再用常量 1。
///
/// `Info` / `Publish` 是**不开窗**的 CLI 子命令（[`SetupMode::cli_mode`]）：调用方必须
/// 在开窗前分支到 [`super::cli`]，本函数遇到这两个模式会明确报错，避免"能开窗但什么
/// 也没做"的静默路径。
pub fn prepare(
    config: &DevSyncConfig,
    mode: SetupMode,
    secrets: SetupSecrets,
) -> Result<SetupOutcome, SetupError> {
    match mode {
        SetupMode::Existing => prepare_existing(config),
        SetupMode::Create => prepare_create(config, secrets),
        SetupMode::Join => prepare_join(config, secrets),
        SetupMode::Info | SetupMode::Publish => bail!(
            "{} 是不开窗的 CLI 子命令，必须由 main 在开窗前分支处理（不应进入启动生命周期）",
            mode.label()
        ),
    }
}

/// 普通 `--dev-sync`：**只**加载已有身份，绝不创建库/身份。
fn prepare_existing(config: &DevSyncConfig) -> Result<SetupOutcome, SetupError> {
    // 沿用切片 2a 的旧/非空库判据：身份存在时放行；不存在且库有内容时明确拒绝，
    // 避免给来历不明的旧库凭空造身份（ADR 0009「只接受修复后新库」）。
    ensure_instance_vault_ready(&config.vault).with_context(|| {
        format!(
            "dev-sync 实例库不可用（不迁移、不删除）：{}",
            config.vault.display()
        )
    })?;

    let store = DevIdentityStore;
    let identity = store.load(&config.vault).with_context(|| {
        format!(
            "dev-sync 实例缺少持久身份：{}；{ENABLE_FLAG} 现在只复用已有身份，不再自动创建。\
             请用 {CREATE_FLAG} 初始化新库，或用 {JOIN_FLAG} 加入已有库",
            DevIdentityStore::path_for(&config.vault).display()
        )
    })?;
    let vault_id = identity.vault_id;
    let vault = Vault::open(&config.vault)
        .with_context(|| format!("打开 dev-sync 实例库失败：{}", config.vault.display()))?;
    // ADR 0013 S2b：启动对账。没有 `sync.init_pending` 时是纯 no-op（不联网、不写库）；
    // 有未完成的初始化时，用已有身份材料补齐 finalize（不重复 genesis/enroll）或
    // fail-closed 停住（不自动重建、不删用户数据）。
    reconcile_pending_init(&vault, &identity).with_context(|| {
        format!(
            "dev-sync 初始化对账失败（未完成的初始化必须人工处理，不自动重建）：{}",
            config.vault.display()
        )
    })?;
    Ok(SetupOutcome {
        vault,
        identity,
        vault_id,
        recovery_code: None,
    })
}

/// 启动对账（ADR 0013 S2b）：只有存在 `sync.init_pending` 时才动作。
fn reconcile_pending_init(vault: &Vault, identity: &LocalIdentityV1) -> Result<(), SetupError> {
    if load_pending(vault, Some(identity.vault_id))?.is_none() {
        return Ok(());
    }
    let Some(report) = reconcile(vault, Some(identity))? else {
        bail!("初始化对账报告与 pending 状态不一致（不静默继续）");
    };
    eprintln!(
        "[menghunote] dev-sync 初始化对账完成：kind={} phase={:?} outcome={:?}",
        report.kind.as_str(),
        report.phase_before,
        report.outcome
    );
    Ok(())
}

/// `--dev-sync-create`：新路径 + setup secret → genesis → 受保护身份落盘。
fn prepare_create(
    config: &DevSyncConfig,
    secrets: SetupSecrets,
) -> Result<SetupOutcome, SetupError> {
    // 先校验材料与路径，再联网：缺 secret / 路径已存在都不该触发任何网络请求。
    let setup_secret = required_secret(secrets.setup_secret, SETUP_SECRET_ENV, CREATE_FLAG)?;
    require_fresh_paths(&config.vault, "Create")?;

    // ADR 0013 S3a：身份 rename 成功后再推进 pending（IdentityWritten → Done）。
    let created = DevLocal::create_deferring_identity(
        config.relay(),
        &setup_secret,
        &config.vault,
        INSTANCE_LABEL,
    )
    .with_context(|| {
        format!(
            "Create 失败（relay = {}，路径 = {}）",
            config.relay(),
            config.vault.display()
        )
    })?;
    // setup secret 只在这一步用；`CreatedVault.setup_secret` 是回显字段，这里显式丢弃，
    // 不随 outcome 继续持有。
    drop(created.setup_secret);
    finish(
        created.vault,
        created.identity,
        created.vault_id,
        Some(created.recovery_code),
        &config.vault,
    )
}

/// `--dev-sync-join`：只接收 vault_id + 恢复码 + 新路径（**不读 A 的目录/身份/root**）。
fn prepare_join(config: &DevSyncConfig, secrets: SetupSecrets) -> Result<SetupOutcome, SetupError> {
    let vault_id = parse_vault_id(secrets.vault_id)?;
    let recovery_code = required_secret(secrets.recovery_code, RECOVERY_CODE_ENV, JOIN_FLAG)?;
    require_fresh_paths(&config.vault, "Join")?;

    let joined =
        DevLocal::join_deferring_identity(config.relay(), &vault_id, &recovery_code, &config.vault)
            .with_context(|| {
                format!(
                    "Join 失败（relay = {}，路径 = {}）",
                    config.relay(),
                    config.vault.display()
                )
            })?;
    finish(
        joined.vault,
        joined.identity,
        joined.vault_id,
        None,
        &config.vault,
    )
}

/// 收尾：身份必须**受保护落盘**后才算成功；失败则清掉本次新建的库/身份文件。
fn finish(
    vault: Vault,
    identity: LocalIdentityV1,
    vault_id: VaultId,
    recovery_code: Option<String>,
    vault_path: &Path,
) -> Result<SetupOutcome, SetupError> {
    let store = DevIdentityStore;
    if let Err(error) = store.persist_protected(vault_path, &identity) {
        // 先关库再删文件（Windows 上句柄没释放时删不掉）。
        drop(vault);
        cleanup_new_vault(vault_path);
        return Err(error).with_context(|| {
            format!(
                "dev-sync 身份持久化失败：已清理本次新建的库文件/身份文件（目录 {} 可能保留为空），\
                 不留下“能开窗但身份没落盘”的半状态",
                parent_dir(vault_path).display()
            )
        });
    }
    // 身份已原子落盘：推进 pending（IdentityWritten → Done）。失败**不删文件**：
    // 实例已可恢复，下一次启动的初始化对账会用身份补齐 finalize（ADR 0013 §2/S3a）。
    if let Err(error) = init_session::mark_identity_written(&vault)
        .and_then(|()| init_session::finish_identity_written(&vault))
    {
        drop(vault);
        return Err(error).context(
            "dev-sync 身份已落盘，但初始化 pending 收尾失败；未删除任何文件，\
             下次启动会做初始化对账（不自动重建）",
        );
    }
    Ok(SetupOutcome {
        vault,
        identity,
        vault_id,
        recovery_code,
    })
}

/// Create/Join 只接受全新路径：库文件与同目录身份文件都必须不存在。
///
/// 身份文件单独检查：库文件不存在但 `dev-identity.bin` 仍在，说明上一次初始化留下了
/// 可疑状态（或用户误指了别人的实例目录）；按 ADR 0009「不覆盖、不迁移、不删除」明确拒绝。
fn require_fresh_paths(vault_path: &Path, verb: &str) -> Result<(), SetupError> {
    if vault_path.as_os_str().is_empty() {
        bail!("{verb} 需要明确的 vault 路径（MENGHUNOTE_DEV_VAULT）");
    }
    if vault_path.exists() {
        bail!(
            "{verb} 只接受尚不存在的 vault 路径：{}（已有库请用普通 {ENABLE_FLAG} 复用其身份；\
             不覆盖、不迁移、不删除）",
            vault_path.display()
        );
    }
    let identity_path = DevIdentityStore::path_for(vault_path);
    if identity_path.exists() {
        bail!(
            "dev identity 文件已存在，拒绝 {verb}：{}（不覆盖已有身份；请换新目录或人工确认）",
            identity_path.display()
        );
    }
    Ok(())
}

/// 必填环境变量：去首尾空白后为空等同于缺失；错误只报变量名，不回显值。
///
/// 对 `dev_sync` 子模块可见：切片 4c-3 的 `--dev-sync-publish` 也用它读恢复码
/// （同样是"只从 env 读、错误只点名变量"）。
pub(super) fn required_secret(
    value: Option<String>,
    env_name: &str,
    flag: &str,
) -> Result<String, SetupError> {
    match value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
    {
        Some(value) => Ok(value),
        None => bail!("{flag} 需要 {env_name}（只从环境变量读取，禁止放进 argv/日志）"),
    }
}

/// 解析 `MENGHUNOTE_DEV_VAULT_ID`（16B 小写/大写十六进制皆可）。
///
/// 错误**只报长度**，绝不回显内容：该变量若被误填成恢复码，回显就等于泄露秘密。
fn parse_vault_id(raw: Option<String>) -> Result<VaultId, SetupError> {
    let raw = required_secret(raw, VAULT_ID_ENV, JOIN_FLAG)?;
    ids::parse_hex::<16>(&raw).with_context(|| {
        format!(
            "{VAULT_ID_ENV} 必须是 32 位十六进制（16B vault id，公开标识；实际 {} 个字符）",
            raw.chars().count()
        )
    })
}

/// 清掉本次调用刚建出的 vault 文件与身份文件（未成功发布身份时的回滚）。
///
/// 只允许在"两个路径启动前都不存在"的前提下调用；best-effort，不掩盖原始错误。
/// 实例目录本身保留（可能是用户已存在的目录）。
fn cleanup_new_vault(vault_path: &Path) {
    let _ = std::fs::remove_file(vault_path);
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut sidecar = vault_path.as_os_str().to_os_string();
        sidecar.push(suffix);
        let _ = std::fs::remove_file(PathBuf::from(sidecar));
    }
    let _ = std::fs::remove_file(DevIdentityStore::path_for(vault_path));
}

/// 出错提示里用的目录（相对文件名退化为 `.`，与 [`DevIdentityStore::path_for`] 一致）。
fn parent_dir(path: &Path) -> PathBuf {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_are_explicit_and_create_join_are_mutually_exclusive() {
        assert_eq!(
            resolve_mode(["--dev-sync"]).expect("普通启用 = 复用已有身份"),
            SetupMode::Existing
        );
        assert_eq!(
            resolve_mode(Vec::<String>::new()).expect("无 flag = Existing"),
            SetupMode::Existing
        );
        assert_eq!(
            resolve_mode(["--dev-sync-create"]).expect("Create"),
            SetupMode::Create
        );
        assert_eq!(
            resolve_mode(["--dev-sync", "--dev-sync-join"]).expect("Join"),
            SetupMode::Join
        );
        // 切片 4c-3：两个不开窗子命令同样是显式模式，且 `cli_mode()` 只对它们返回 Some。
        assert_eq!(
            resolve_mode(["--dev-sync-info"]).expect("Info"),
            SetupMode::Info
        );
        assert_eq!(
            resolve_mode(["--dev-sync-publish"]).expect("Publish"),
            SetupMode::Publish
        );
        assert_eq!(SetupMode::Info.cli_mode(), Some(CliMode::Info));
        assert_eq!(SetupMode::Publish.cli_mode(), Some(CliMode::Publish));
        for mode in [SetupMode::Existing, SetupMode::Create, SetupMode::Join] {
            assert_eq!(mode.cli_mode(), None, "{mode:?} 是开窗模式");
        }
        let error = resolve_mode(["--dev-sync-create", "--dev-sync-join"])
            .expect_err("两个初始化模式必须互斥");
        assert!(
            error.to_string().contains("互斥"),
            "互斥错误必须说清楚：{error}"
        );
        // Create/Join 与 Info/Publish 也互斥（一次启动只能做一件事）。
        let error = resolve_mode(["--dev-sync-create", "--dev-sync-publish"])
            .expect_err("Create 与 Publish 必须互斥");
        assert!(
            error.to_string().contains(PUBLISH_FLAG),
            "必须点名冲突的 flag"
        );
        let error = resolve_mode(["--dev-sync-info", "--dev-sync-publish"])
            .expect_err("两个子命令必须互斥");
        let message = error.to_string();
        assert!(message.contains(INFO_FLAG) && message.contains(PUBLISH_FLAG));
    }

    #[test]
    fn prepare_refuses_windowless_cli_modes() {
        // 这两个模式不得进入启动生命周期：main 必须先分支，否则会"开窗但什么都没做"。
        let config = DevSyncConfig {
            vault: PathBuf::from("/tmp/menghunote-dev-sync-cli-guard/vault.sqlite3"),
            relay_mode: menghunote_sync::relay_policy::RelayMode::LocalDev,
            service: crate::dev_sync::service::ServiceConfig::new("http://127.0.0.1:18080"),
        };
        for mode in [SetupMode::Info, SetupMode::Publish] {
            let error = prepare(&config, mode, SetupSecrets::default())
                .err()
                .unwrap_or_else(|| panic!("{mode:?} 不得进入 prepare"));
            assert!(
                error.to_string().contains("开窗前"),
                "错误必须说明要由 main 分支：{error}"
            );
        }
    }

    #[test]
    fn missing_and_blank_secrets_are_reported_by_name_only() {
        let error = required_secret(Some("   ".to_owned()), SETUP_SECRET_ENV, CREATE_FLAG)
            .expect_err("空值等同缺失");
        let message = error.to_string();
        assert!(
            message.contains(SETUP_SECRET_ENV),
            "必须点名变量：{message}"
        );
        assert!(message.contains(CREATE_FLAG), "必须点明模式：{message}");

        // 恢复码不能被回显（用户可能把恢复码误填进 vault id 变量）。
        let error = parse_vault_id(Some("mnrec1secretrecoverycode".to_owned()))
            .expect_err("长度不符必须是明确错误");
        let message = format!("{error:#}");
        assert!(
            !message.contains("mnrec1secretrecoverycode"),
            "错误不得回显内容：{message}"
        );
        assert!(
            message.contains(VAULT_ID_ENV) && message.contains("32 位"),
            "必须说清期望格式：{message}"
        );
    }

    #[test]
    fn secrets_are_read_from_the_expected_environment_keys() {
        let secrets = secrets_from_env(|key| match key {
            SETUP_SECRET_ENV => Some("setup".to_owned()),
            VAULT_ID_ENV => Some("00112233445566778899aabbccddeeff".to_owned()),
            RECOVERY_CODE_ENV => Some("mnrec1code".to_owned()),
            _ => None,
        });
        assert_eq!(secrets.setup_secret.as_deref(), Some("setup"));
        assert_eq!(
            secrets.vault_id.as_deref(),
            Some("00112233445566778899aabbccddeeff")
        );
        assert_eq!(secrets.recovery_code.as_deref(), Some("mnrec1code"));
    }
}
