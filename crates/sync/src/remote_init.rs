//! ADR 0013 **S3b**：app 侧的远端 `remote_tls` Create / Join 编排（受限初始化 + 身份落盘回调）。
//!
//! ## 与 `dev_local` / `init_session` 的分工
//!
//! - [`crate::init_session::InitSession`] 是**受限能力**：NewGenesis / FreshRecovery，
//!   没有 unpin/reset 分支，pin 决议只能由本地材料（genesis 证书 / 恢复码）证明；
//! - [`crate::dev_local::DevLocal`] 是 `local_dev` 的既有编排（明文 loopback）；
//! - 本模块把同一套受限能力接到 **remote_tls**（production TLS + HTTPS canonical origin）：
//!   远端 Create/Join 的**顺序**与本地不同——身份文件由 app 用"临时 + 原子 rename"写，
//!   sync 看不到那个提交点，所以 `create_remote` / `join_remote` 接受一个**落盘回调**
//!   （`persist_identity`），在正确时序里调用它，然后由本模块推进 durable pending 阶段。
//!
//! ```text
//! Create: pending(Prepared) → POST /v1/vaults(GenesisRequested)
//!         → 本地事实（structure/label/cursor/log_id/初始槽材料）
//!         → 身份原子落盘（回调）→ IdentityWritten
//!         → /state + /control 全链验签 + rev1 前缀 → commit_verified_head(PinCommitted)
//!         → 上传初始 recovery 槽 → Done（清理 pending）
//!
//! Join:   恢复码 → pending(Prepared) → self_enroll(GenesisRequested) → 认证 bootstrap
//!         → 身份原子落盘（回调）→ IdentityWritten
//!         → record pin + bootstrap 锚同一验证视图 → commit_recovery_pin(PinCommitted)
//!         → Done（清理 pending）
//! ```
//!
//! ## 失败语义（ADR 0013 §3/§4；与 local_dev 的差别是有意的）
//!
//! - **拿到回执/身份材料之前**失败（TLS/策略拒绝、relay 明确拒绝、传输中断）：
//!   本次新库没有任何可用身份，永远无法 finalize。清理 pending（`init_journal::abort_before_identity`
//!   的受限入口）+ 删掉本次新建的库文件，返回错误；远端最多留下不可用的空 vault/孤儿
//!   设备（与 ADR 0009 既有失败语义一致）。调用方可以换新目录重试。
//! - **拿到回执/身份材料之后**失败（含身份落盘失败）：**保留** pending/库与已落盘的
//!   事实（不删用户数据、不回退 local_dev、不 unpin），错误交回 app 显示；`Done` 之前的
//!   pending 由启动对账继续（能证明安全才推进，证明不了就是 `NeedsTrust`/
//!   `ConfigInvalid`，不自动重建）。身份落盘失败时本地没有可对账的身份材料，保留现场
//!   是为了人工诊断（**不**删库、**不**重跑 genesis/enroll 覆盖远端事实）。
//! - 收尾（`finish`）失败与初始 recovery 槽上传失败**不**当致命错误：身份与强 pin 都已
//!   durable，实例可用；以 `warning` 交回 app（启动对账补 pending；槽上传失败时其他设备
//!   用恢复码 Join 可能失败，恢复码仍在 app 的备份页）。
//!
//! ## 安全
//!
//! - 传输只来自 [`RelayContext::transport`]（`remote_tls` → 显式 production TLS policy，
//!   系统信任根；测试 harness 可用 `with_test_extra_roots` 显式追加自签根，仍校验证书链/
//!   SAN/有效期，**没有** accept-invalid）；
//! - pending/身份/配置**不含秘密**；恢复码只在内存里传给初始槽上传，不落盘、不进日志；
//! - 本模块不写 app 的受保护身份文件：落盘是回调的责任（`DevIdentityStore`），
//!   sync 只在回调返回成功后推进 `IdentityWritten`。

use std::path::Path;

use menghunote_storage::{ensure_structure_row, set_sync_log_id, Vault};

use crate::create_link::CreateTicket;
use crate::dev_local::{
    encode_initial_slot, remove_vault_files, throwaway_password, DevLocal,
    INITIAL_RECOVERY_SLOT_META, VAULT_LABEL_META,
};
use crate::engine::RELAY_CURSOR_META;
use crate::error::SyncError;
use crate::genesis::{self, GenesisReceipt};
use crate::http_relay::{Clock, SystemClock};
use crate::identity::LocalIdentityV1;
use crate::init_journal;
use crate::init_session::{finish_identity_written, mark_identity_written, InitSession};
use crate::random::{array, OsRandom};
use crate::recovery::RecoverySeed;
use crate::relay_context::RelayContext;
use crate::relay_policy::RelayMode;
use crate::{DeviceId, VaultId};

/// Create 成功后的产物（**不实现 `Debug`**：`identity`/`recovery_code` 是秘密材料）。
///
/// v1（`create_remote`）与 v2 票据（`create_vault_with_ticket`）返回**同一组事实**：
/// 库、身份、vault id、一次性恢复码、非致命告警。写身份/公开配置仍由调用方决定。
pub struct CreatedVault {
    /// 已打开的真实库（结构行/标签/游标/log_id/初始槽材料都已落库）。
    pub vault: Vault,
    /// 新设备身份（已由调用方回调原子落盘）。
    pub identity: LocalIdentityV1,
    /// 新 vault id（公开标识）。
    pub vault_id: VaultId,
    /// 一次性恢复码（只在 app 备份页展示；不落盘、不进日志）。
    pub recovery_code: String,
    /// 非致命告警（初始恢复槽上传/ pending 收尾失败；不影响实例可用）。
    pub warning: Option<String>,
}

/// 兼容旧名（v1 `create_remote` 的历史返回类型）。
pub type RemoteCreatedVault = CreatedVault;

/// 远端 Join 成功后的产物（**不实现 `Debug`**：`identity` 是秘密材料）。
pub struct RemoteJoinedVault {
    /// 已由 bootstrap 导入基线的库（pins/cursor 已 durable）。
    pub vault: Vault,
    /// 新设备身份（已由调用方回调原子落盘）。
    pub identity: LocalIdentityV1,
    /// 加入的 vault id（公开标识）。
    pub vault_id: VaultId,
    /// 非致命告警（pending 收尾失败；不影响实例可用）。
    pub warning: Option<String>,
}

/// 远端 Create：genesis + 身份落盘回调 + rev1 强 pin + 初始恢复槽 + `Done`。
///
/// `persist_identity` 必须在**返回前**完成身份的"临时文件 + 原子 rename"落盘
/// （app 的 `DevIdentityStore::persist_protected`）；失败=本次身份未落盘，本模块保留
/// 库/pending 并返回错误（远端可能已建 vault，**不得**重跑 genesis 覆盖）。
///
/// `context` 必须是 `remote_tls`（HTTPS canonical origin）；`local_dev` 走
/// [`crate::dev_local::DevLocal::create`]，不在本入口（防止生产路径误用明文）。
pub fn create_remote(
    context: RelayContext,
    setup_secret: &str,
    vault_path: &Path,
    vault_label: &str,
    persist_identity: impl FnOnce(&LocalIdentityV1) -> Result<(), SyncError>,
) -> Result<CreatedVault, SyncError> {
    require_remote_mode(&context)?;
    if setup_secret.is_empty() {
        return Err(SyncError::ConfigInvalid(
            "remote Create 需要非空 setup secret（服务器创建凭证）".to_owned(),
        ));
    }
    // v1 面（remote_tls 的既有 Create）；票据创建走 create_vault_with_ticket。
    if context.api_version() != crate::api_version::RelayApiVersion::V1 {
        return Err(SyncError::ConfigInvalid(
            "create_remote 只接受 v1 面；v2 创建请用一次性票据（create_vault_with_ticket）"
                .to_owned(),
        ));
    }
    let vault_id: VaultId = array(&mut OsRandom)?;
    create_with_auth(
        context,
        vault_id,
        GenesisCreateAuth::SetupSecret(setup_secret),
        None,
        vault_path,
        vault_label,
        persist_identity,
    )
}

/// **库级票据创建**（ADR 0014 S3a）：`POST /v2/vaults` + 本地材料 + 身份/pin/本地库初始化。
///
/// 与 [`create_remote`] 的主体**完全复用**（同一个 durable pending/回执校验/强 pin/
/// 初始恢复槽/收尾），只把"创建路由"从 v1 setup secret 换成 v2 一次性票据：
///
/// - `context` 必须显式选择 [`crate::api_version::RelayApiVersion::V2`]
///   （`local_dev` 或 `remote_tls` 均可；测试用 loopback dev relay）；
/// - `vault_id` 必须与票据绑定的预留 id 一致（relay 统一以 403 拒绝错 vault；客户端
///   拿不到票据的绑定信息，因此这一步只能由 relay 判定）；
/// - 票据只在内存里用一次：不写 pending/配置/日志，失败也不消费票据（relay 事务回滚）；
/// - 失败语义与 v1 相同：拿到回执前失败会清理本次新库；拿到回执后失败保留现场。
///
/// 返回事实：[`CreatedVault`]（写身份/公开配置由调用方决定，库不替 app 落盘）。
pub fn create_vault_with_ticket(
    context: RelayContext,
    vault_id: VaultId,
    ticket: &CreateTicket,
    expected_instance: Option<[u8; 16]>,
    vault_path: &Path,
    vault_label: &str,
    persist_identity: impl FnOnce(&LocalIdentityV1) -> Result<(), SyncError>,
) -> Result<CreatedVault, SyncError> {
    if context.api_version() != crate::api_version::RelayApiVersion::V2 {
        return Err(SyncError::ConfigInvalid(
            "票创建必须显式使用 v2 面（RelayApiVersion::V2）".to_owned(),
        ));
    }
    create_with_auth(
        context,
        vault_id,
        GenesisCreateAuth::Ticket(ticket),
        expected_instance,
        vault_path,
        vault_label,
        persist_identity,
    )
}

/// 创建授权方式（唯一差别；其余流程逐字节共用）。
enum GenesisCreateAuth<'a> {
    /// v1 `POST /v1/vaults`（setup secret）。
    SetupSecret(&'a str),
    /// v2 `POST /v2/vaults`（一次性票据）。
    Ticket(&'a CreateTicket),
}

/// v1/v2 共用的 Create 主体（顺序与失败语义见模块文档）。
fn create_with_auth(
    context: RelayContext,
    vault_id: VaultId,
    auth: GenesisCreateAuth<'_>,
    expected_instance: Option<[u8; 16]>,
    vault_path: &Path,
    vault_label: &str,
    persist_identity: impl FnOnce(&LocalIdentityV1) -> Result<(), SyncError>,
) -> Result<CreatedVault, SyncError> {
    if vault_path.as_os_str().is_empty() {
        return Err(SyncError::LocalCorrupt("Create 需要明确的 vault 路径"));
    }
    if vault_path.exists() {
        return Err(SyncError::LocalCorrupt(
            "Create 只接受尚不存在的 vault 路径（已有库请按其身份使用）",
        ));
    }

    // 1) 本地材料 + 会话（不联网）：vault/device id 必须先进 pending，之后才发请求。
    let device_id: DeviceId = array(&mut OsRandom)?;
    let throwaway_password = throwaway_password()?;
    let materials = genesis::build_materials(
        vault_id,
        device_id,
        &throwaway_password,
        SystemClock.now_secs(),
        &mut OsRandom,
    )?;
    let session = InitSession::new_genesis(context, &materials)?;

    // 2) 新库 + 写前 pending（`Prepared`）。
    let vault = match Vault::open(vault_path) {
        Ok(vault) => vault,
        Err(error) => {
            remove_vault_files(vault_path);
            return Err(SyncError::Storage(error));
        }
    };
    if let Err(error) = session.prepare_pending(&vault, SystemClock.now_secs()) {
        drop(vault);
        remove_vault_files(vault_path);
        return Err(error);
    }

    // 3) 请求 genesis/genesis-with-ticket。失败 = 没有回执/身份，本次新库无法继续
    //    （见模块文档）。
    let receipt = match auth {
        GenesisCreateAuth::SetupSecret(setup_secret) => {
            session.request_genesis(&vault, &materials, setup_secret)
        }
        GenesisCreateAuth::Ticket(ticket) => {
            session.request_genesis_with_ticket(&vault, &materials, ticket)
        }
    };
    let receipt = match receipt {
        Ok(receipt) => receipt,
        Err(error) => {
            let _ = init_journal::abort_before_identity(&vault);
            drop(vault);
            remove_vault_files(vault_path);
            return Err(error);
        }
    };

    // 3b) 公开锚（链接里的 `pin`）：必须与创建回执的 relay instance 一致。
    //     不自动换 pin；不一致时这次新库没有可证明的身份，按"回执前失败"的同一套
    //     清理语义处理（远端可能已建 vault/消费票据，本地不留任何半初始化材料）。
    if let Some(expected) = expected_instance {
        if receipt.instance_id != expected {
            let _ = init_journal::abort_before_identity(&vault);
            drop(vault);
            remove_vault_files(vault_path);
            return Err(SyncError::InstanceMismatch);
        }
    }

    // 4) 本地事实（不依赖身份/pin）：回执里的 cursor/log_id + 展示标签 + 初始槽材料。
    //    先落这些，`IdentityWritten` 的崩溃窗口里库至少是"带游标/槽材料"的完整事实。
    write_created_local_facts(&vault, vault_label, &materials, &receipt)?;

    // 5) 身份落盘（app 的原子 rename）→ durable `IdentityWritten`。
    //    失败保留库/pending（远端已建 vault，不得删/重建）。
    let identity = materials.identity(receipt.instance_id, receipt.control_rev)?;
    persist_identity(&identity)?;
    drop(identity);
    mark_identity_written(&vault)?;

    // 6) 验证 `/state + /control` 全链与 rev1 `SHA256(零32||cert)` 前缀 → 强 pin。
    let report = session.finalize_genesis(&vault, &materials, &receipt)?;

    // 7) 初始 recovery 槽必须现在上传（`K_slot_mne = HKDF(恢复码种子)` 不可逆，恢复码不落盘）。
    //    这是收尾步骤：身份/强 pin 已 durable，失败降级为告警而不是"创建失败"。
    let mut warning = match DevLocal::upload_initial_slot_during_init(
        session.context(),
        &vault,
        &report.identity,
        &materials.recovery_seed,
    ) {
        Ok(()) => None,
        Err(error) => Some(format!(
            "初始恢复码槽上传失败（{error}）：实例可继续使用，但其他设备用恢复码 Join 可能失败；\
             恢复码仍须备份（可稍后用恢复码补传）"
        )),
    };

    // 8) pending → Done（清理只在 Done）。失败留给启动对账，不删数据。
    if let Err(error) = session.finish(&vault) {
        warning = Some(append_warning(
            warning,
            format!("初始化 pending 收尾失败（{error}）：重启后会做启动对账补齐"),
        ));
    }

    Ok(CreatedVault {
        vault,
        identity: report.identity,
        vault_id,
        recovery_code: materials.recovery_code.as_str().to_owned(),
        warning,
    })
}

/// 远端 Join：恢复码 → self_enroll → 认证 bootstrap → 身份落盘回调 → 强 pin + 锚 → `Done`。
///
/// 与 [`create_remote`] 对称：`persist_identity` 负责身份的原子落盘，本模块只在其成功后
/// 推进 `IdentityWritten`；pin 必须在 bootstrap 的**同一验证视图**上提交
/// （`record` 与 `bootstrap` 锚各带 rev，见 [`crate::init_session`]）。
pub fn join_remote(
    context: RelayContext,
    vault_id: VaultId,
    recovery_code: &str,
    vault_path: &Path,
    persist_identity: impl FnOnce(&LocalIdentityV1) -> Result<(), SyncError>,
) -> Result<RemoteJoinedVault, SyncError> {
    require_remote_mode(&context)?;
    if vault_path.as_os_str().is_empty() {
        return Err(SyncError::LocalCorrupt("Join 需要明确的 vault 路径"));
    }
    if vault_path.exists() {
        return Err(SyncError::LocalCorrupt(
            "Join 只接受尚不存在的 vault 路径（已有库请按其身份使用）",
        ));
    }

    // 1) 恢复码 → 恢复种子（格式错误在**建库之前**返回：不留下任何文件）。
    let seed = RecoverySeed::from_code(recovery_code)?;
    let session = InitSession::new_fresh_recovery(context, vault_id, &seed)?;

    // 2) 新库 + 写前 pending（`Prepared`）。
    let vault = match Vault::open(vault_path) {
        Ok(vault) => vault,
        Err(error) => {
            remove_vault_files(vault_path);
            return Err(SyncError::Storage(error));
        }
    };
    if let Err(error) = session.prepare_pending(&vault, SystemClock.now_secs()) {
        drop(vault);
        remove_vault_files(vault_path);
        return Err(error);
    }

    // 3) self_enroll + 认证 bootstrap（远端可能已登记设备 → 之后的失败不得自动重建）。
    let recovered = match session.recover_identity(&vault, SystemClock.now_secs()) {
        Ok(recovered) => recovered,
        Err(error) => {
            // 没有身份材料：本次新库无法 finalize（远端最多留下孤儿设备），清理后允许重试。
            let _ = init_journal::abort_before_identity(&vault);
            drop(vault);
            remove_vault_files(vault_path);
            return Err(error);
        }
    };

    // 4) 身份落盘（app 原子 rename）→ `IdentityWritten`。
    persist_identity(&recovered.identity)?;
    mark_identity_written(&vault)?;

    // 5) 用 bootstrap 的同一验证视图提交 record pin + bootstrap 锚（`PinCommitted`）。
    session.commit_recovery_pin(&vault, &recovered.identity)?;

    // 6) pending → Done（清理只在 Done）；失败留给启动对账。
    let warning = match finish_identity_written(&vault) {
        Ok(()) => None,
        Err(error) => Some(format!(
            "初始化 pending 收尾失败（{error}）：实例保留可用，重启后会做启动对账补齐"
        )),
    };

    Ok(RemoteJoinedVault {
        vault,
        identity: recovered.identity,
        vault_id,
        warning,
    })
}

/// 只允许 `remote_tls`：防止生产路径把明文/开发语义误接进远端入口。
fn require_remote_mode(context: &RelayContext) -> Result<(), SyncError> {
    if context.mode != RelayMode::RemoteTls {
        return Err(SyncError::ConfigInvalid(
            "远端 Create/Join 只接受 remote_tls（https）context；local_dev 走 DevLocal 入口"
                .to_owned(),
        ));
    }
    Ok(())
}

/// genesis 回执里的本地事实（不依赖身份/pin）：标签、初始游标、log_id、初始 slot 材料。
fn write_created_local_facts(
    vault: &Vault,
    vault_label: &str,
    materials: &genesis::GenesisMaterials,
    receipt: &GenesisReceipt,
) -> Result<(), SyncError> {
    ensure_structure_row(vault)?;
    vault.set_meta(VAULT_LABEL_META, vault_label.as_bytes())?;
    if let Some(cursor) = &receipt.initial_cursor {
        vault.set_meta(RELAY_CURSOR_META, cursor.as_bytes())?;
    }
    // log_id 来自 genesis 响应：先钉住 relay 日志代，pull 再复核。
    set_sync_log_id(vault, &receipt.log_id)?;
    // 初始 recovery 槽材料（密文）留给上传/Join 复用（同一 slot_id 幂等）。
    let slot_record = encode_initial_slot(&materials.recovery_slot)?;
    vault.set_meta(INITIAL_RECOVERY_SLOT_META, &slot_record)?;
    Ok(())
}

/// 合并告警（两条非致命收尾失败时都保留，避免后一条覆盖前一条）。
fn append_warning(existing: Option<String>, next: String) -> String {
    match existing {
        Some(existing) => format!("{existing}；{next}"),
        None => next,
    }
}
