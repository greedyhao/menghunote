//! ADR 0013 **S2b**：受限初始化的 durable pending / journal（`sync.init_pending`）。
//!
//! ## 为什么需要它
//!
//! 初始化要跨三类事实：**远端**（relay 上新建的 vault/设备）、**SQLite meta**
//! （cursor/pins/control pin）与**身份文件**（app 侧 `临时 + 原子 rename`）。三者不能放在
//! 一个事务里，任何一步之后崩溃都会留下"远端已建、本地未完成"或"本地半初始化"的状态。
//! 如果崩溃后直接重跑 Create/Join，就会在 relay 上留下不可用的孤儿 vault，或把已有材料
//! 覆盖掉。因此初始化前先在 SQLite meta 写一条 **canonical CDE v1** 的 pending：
//!
//! ```text
//! {v:1, kind:uint, mode:uint, origin:text, vault_id:b16,
//!  expected_device:b16, phase:uint, created_at:uint}
//! ```
//!
//! - `kind`：`NewGenesis=1` / `FreshRecovery=2` / `VerifiedMigration=3`；
//! - `mode`：`local_dev=1` / `remote_tls=2`（与 [`RelayMode`] 一一对应，未知值拒绝）；
//! - `origin`：canonical origin 文本（`scheme://host[:port]`，不含 userinfo/query）；
//! - `expected_device`：本项目**预期**落盘的设备 id；全 0 表示"尚未确定"（FreshRecovery
//!   在 enroll 之前还不知道新设备 id，enroll 成功后用 [`bind_expected_device`] 补上）；
//! - `phase`：只进不退的阶段阶梯，见 [`InitPhase`]；
//! - `created_at`：Unix 秒（只用于诊断/过期判断，不参与信任判定）。
//!
//! ## 阶段（每一步都 durable）
//!
//! ```text
//! Prepared → GenesisRequested → IdentityWritten → PinCommitted → Done
//! ```
//!
//! | 阶段 | 含义 |
//! | --- | --- |
//! | `Prepared` | 本地材料/write-ahead 已落盘；**初始化请求尚未发出**（写前记录保证） |
//! | `GenesisRequested` | 初始化请求**已发出**（genesis/enroll）；远端可能已建立 vault/设备，崩溃后必须对账、不得重建 |
//! | `IdentityWritten` | 身份材料已用"临时 + 原子 rename"落盘（当前由 app 侧写入；sync 只在能证明时推进） |
//! | `PinCommitted` | durable 强 pin（`sync.control_pin`）已提交 |
//! | `Done` | 本地初始化全部完成；**只有**这个阶段允许清理 pending |
//!
//! `Prepared` 与 `GenesisRequested` 的先后是**写前记录**：先写"请求将发出"，再发请求。
//! 因此 `Prepared` 崩溃可以由 [`abort_before_request`] 安全放弃（远端不可能有本次创建），
//! 而 `GenesisRequested` 崩溃必须 fail-closed（远端可能已创建）。唯一的例外是
//! `abort_before_identity`（crate 内受限）：远端初始化在**拿到回执/身份材料之前**失败时，本地没有任何
//! 可对账的材料，保留这个新库只会让路径永久不可用（细节见该函数的文档）。
//!
//! ## 纪律
//!
//! - 阶段**只进不退**（[`advance_phase`] 用 CAS 比较 canonical 字节，落后/超前都拒绝）；
//! - pending 损坏/版本未知/字段残缺/vault 绑定不符都是**明确错误**
//!   （[`SyncError::ConfigInvalid`]），**不得**当成"没有 pending"（否则就等于绕过对账）；
//! - 清理只在 `Done`（[`clear_pending`]）；未完成时[`crate::relay_context::RelayContext::open`]
//!   会拒绝普通同步/管理操作（fail-closed，见 [`ensure_no_pending_sync`]）；
//! - pending **不含任何秘密**：没有根、私钥、恢复种子、恢复码；只有公开 id/模式/origin/阶段。

use menghunote_storage::{AffectedGuard, Statement, StorageError, Value, Vault};
use menghunote_sync_proto::{decode_cde, encode_cde, CdeValue};

use crate::api_version::RelayApiVersion;
use crate::error::SyncError;
use crate::relay_policy::RelayMode;

/// meta 键：初始化 pending（canonical CDE v1）。
pub const META_INIT_PENDING: &str = "sync.init_pending";

/// pending 结构版本（canonical CDE 键 0）。
pub const INIT_PENDING_VERSION: u64 = 1;

const KEY_VERSION: u64 = 0;
const KEY_KIND: u64 = 1;
const KEY_MODE: u64 = 2;
const KEY_ORIGIN: u64 = 3;
const KEY_VAULT_ID: u64 = 4;
const KEY_EXPECTED_DEVICE: u64 = 5;
const KEY_PHASE: u64 = 6;
const KEY_CREATED_AT: u64 = 7;
/// ADR 0014 S3a：relay API 面。**可选**字段：v1 pending 不写（保持既有字节完全不变），
/// 缺省按 v1 解读；v2 创建（票据）写 2。旧二进制读到 v2 pending 会按"未知字段"拒绝，
/// 这正是单向升级的 fail-closed 语义（不静默降级）。
const KEY_API_VERSION: u64 = 8;

/// 初始化能力种类（ADR 0013 §3：只有这三种受限能力，没有 None/unpin/reset）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum InitKind {
    /// 新 vault：本地提交证书复算 rev1 `record_hash`，TLS 后验证链前缀并提交强 pin。
    NewGenesis,
    /// 恢复码加入：恢复码管理锚 + TLS + 认证后的控制链/checkpoint/对象验证。
    FreshRecovery,
    /// 已有库/身份与旧 pin 的受限迁移（转 canonical v1；不具备条件明确拒绝）。
    VerifiedMigration,
}

impl InitKind {
    /// CDE 里的稳定取值（**不得**重排；未知值读取时拒绝）。
    #[must_use]
    pub const fn as_code(self) -> u64 {
        match self {
            Self::NewGenesis => 1,
            Self::FreshRecovery => 2,
            Self::VerifiedMigration => 3,
        }
    }

    /// 从 CDE 取值解析；未知值返回 `None`（调用方必须拒绝，不做默认回退）。
    #[must_use]
    pub const fn from_code(code: u64) -> Option<Self> {
        match code {
            1 => Some(Self::NewGenesis),
            2 => Some(Self::FreshRecovery),
            3 => Some(Self::VerifiedMigration),
            _ => None,
        }
    }

    /// 诊断/日志用的稳定短标签。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NewGenesis => "new_genesis",
            Self::FreshRecovery => "fresh_recovery",
            Self::VerifiedMigration => "verified_migration",
        }
    }
}

/// 初始化阶段（只进不退；整理见模块文档）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum InitPhase {
    /// write-ahead 已落盘，远端结果未知。
    Prepared,
    /// 远端初始化已确认成功（vault/设备已在 relay 建立）。
    GenesisRequested,
    /// 身份材料已原子落盘（app 侧写入；sync 只在能证明时推进）。
    IdentityWritten,
    /// durable 强 pin 已提交。
    PinCommitted,
    /// 本地初始化完成（**唯一**允许清理 pending 的阶段）。
    Done,
}

impl InitPhase {
    /// CDE 里的稳定取值。
    #[must_use]
    pub const fn as_code(self) -> u64 {
        match self {
            Self::Prepared => 1,
            Self::GenesisRequested => 2,
            Self::IdentityWritten => 3,
            Self::PinCommitted => 4,
            Self::Done => 5,
        }
    }

    /// 从 CDE 取值解析；未知值返回 `None`。
    #[must_use]
    pub const fn from_code(code: u64) -> Option<Self> {
        match code {
            1 => Some(Self::Prepared),
            2 => Some(Self::GenesisRequested),
            3 => Some(Self::IdentityWritten),
            4 => Some(Self::PinCommitted),
            5 => Some(Self::Done),
            _ => None,
        }
    }

    /// 诊断/日志用的稳定短标签。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Prepared => "prepared",
            Self::GenesisRequested => "genesis_requested",
            Self::IdentityWritten => "identity_written",
            Self::PinCommitted => "pin_committed",
            Self::Done => "done",
        }
    }

    /// 初始化请求是否**已经发出**（`GenesisRequested` 及之后）。
    ///
    /// `Prepared` = 写前记录已落盘但请求未发出：此时远端不可能有本次创建的事实。
    /// 一旦为 `true`，崩溃后既不能重跑 genesis/enroll（可能建出孤儿 vault/设备），
    /// 也不能假定远端不存在：只能按已有材料 finalize 或 fail-closed。
    #[must_use]
    pub const fn request_sent(self) -> bool {
        matches!(
            self,
            Self::GenesisRequested | Self::IdentityWritten | Self::PinCommitted | Self::Done
        )
    }
}

/// 初始化 pending（`sync.init_pending` 的语义结构）。
///
/// **不含秘密**：只有公开 id、模式、origin、阶段与时间戳。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitPendingV1 {
    /// 受限初始化能力种类。
    pub kind: InitKind,
    /// relay 接入模式。
    pub mode: RelayMode,
    /// canonical origin（`scheme://host[:port]`）。
    pub origin: String,
    /// relay API 面（ADR 0014 S3a）：v2 票据创建写 v2；旧 pending/缺省 = v1。
    pub api_version: RelayApiVersion,
    /// 本次初始化要建立的 vault id。
    pub vault_id: [u8; 16],
    /// 预期落盘的设备 id；全 0 = 尚未确定（FreshRecovery 在 enroll 前）。
    pub expected_device: [u8; 16],
    /// 当前阶段。
    pub phase: InitPhase,
    /// 创建时刻（Unix 秒；诊断用，不参与信任判定）。
    pub created_at: u64,
}

impl InitPendingV1 {
    /// 构造一条 `Prepared` pending。
    #[must_use]
    pub fn prepared(
        kind: InitKind,
        mode: RelayMode,
        origin: impl Into<String>,
        vault_id: [u8; 16],
        expected_device: [u8; 16],
        created_at: u64,
    ) -> Self {
        // 兼容入口：历史调用点（local_dev / remote v1）固定 v1 面。
        Self::prepared_with_api_version(
            kind,
            mode,
            origin,
            vault_id,
            expected_device,
            created_at,
            RelayApiVersion::V1,
        )
    }

    /// 显式 API 面的构造（ADR 0014 S3a：v2 票据创建/新配置用）。
    #[must_use]
    pub fn prepared_with_api_version(
        kind: InitKind,
        mode: RelayMode,
        origin: impl Into<String>,
        vault_id: [u8; 16],
        expected_device: [u8; 16],
        created_at: u64,
        api_version: RelayApiVersion,
    ) -> Self {
        Self {
            kind,
            mode,
            origin: origin.into(),
            api_version,
            vault_id,
            expected_device,
            phase: InitPhase::Prepared,
            created_at,
        }
    }

    /// 设备 id 是否已确定。
    #[must_use]
    pub fn has_expected_device(&self) -> bool {
        self.expected_device != [0u8; 16]
    }
}

fn mode_code(mode: RelayMode) -> u64 {
    match mode {
        RelayMode::LocalDev => 1,
        RelayMode::RemoteTls => 2,
    }
}

fn mode_from_code(code: u64) -> Option<RelayMode> {
    match code {
        1 => Some(RelayMode::LocalDev),
        2 => Some(RelayMode::RemoteTls),
        _ => None,
    }
}

fn corrupt(message: &'static str) -> SyncError {
    SyncError::ConfigInvalid(format!("sync.init_pending 损坏：{message}"))
}

/// canonical CDE 编码（唯一编码器；写库/测试夹具共用）。
pub fn encode_pending_v1(pending: &InitPendingV1) -> Result<Vec<u8>, SyncError> {
    let mut fields = vec![
        (KEY_VERSION, CdeValue::UInt(INIT_PENDING_VERSION)),
        (KEY_KIND, CdeValue::UInt(pending.kind.as_code())),
        (KEY_MODE, CdeValue::UInt(mode_code(pending.mode))),
        (KEY_ORIGIN, CdeValue::Text(pending.origin.clone())),
        (KEY_VAULT_ID, CdeValue::Bytes(pending.vault_id.to_vec())),
        (
            KEY_EXPECTED_DEVICE,
            CdeValue::Bytes(pending.expected_device.to_vec()),
        ),
        (KEY_PHASE, CdeValue::UInt(pending.phase.as_code())),
        (KEY_CREATED_AT, CdeValue::UInt(pending.created_at)),
    ];
    // v1 面保持既有编码逐字节不变（旧 golden/旧库不迁移）；v2 才追加键 8。
    if pending.api_version != RelayApiVersion::V1 {
        fields.push((
            KEY_API_VERSION,
            CdeValue::UInt(pending.api_version.as_code()),
        ));
    }
    encode_cde(&CdeValue::Map(fields))
        .map_err(|_| SyncError::ConfigInvalid("sync.init_pending 无法 canonical 编码".to_owned()))
}

/// canonical CDE 解码 + 严格字段/版本/长度校验。
///
/// 任何结构问题都返回 [`SyncError::ConfigInvalid`]，**不**返回 `None`（fail-closed）。
pub fn decode_pending_v1(bytes: &[u8]) -> Result<InitPendingV1, SyncError> {
    let CdeValue::Map(fields) = decode_cde(bytes).map_err(|_| corrupt("不是合法 canonical CDE"))?
    else {
        return Err(corrupt("不是 CDE map"));
    };
    for (key, _) in &fields {
        if *key > KEY_API_VERSION {
            return Err(corrupt("含未知字段"));
        }
    }
    let version = uint_field(&fields, KEY_VERSION)?;
    if version != INIT_PENDING_VERSION {
        return Err(SyncError::ConfigInvalid(format!(
            "sync.init_pending 版本不支持：{version}"
        )));
    }
    let kind_code = uint_field(&fields, KEY_KIND)?;
    let kind = InitKind::from_code(kind_code).ok_or(corrupt("kind 未知"))?;
    let mode_code = uint_field(&fields, KEY_MODE)?;
    let mode = mode_from_code(mode_code).ok_or(corrupt("relay_mode 未知"))?;
    let origin = match field(&fields, KEY_ORIGIN)? {
        CdeValue::Text(text) => text.clone(),
        _ => return Err(corrupt("origin 不是文本")),
    };
    if origin.is_empty() {
        return Err(corrupt("origin 为空"));
    }
    let vault_id = fixed_field::<16>(&fields, KEY_VAULT_ID)?;
    let expected_device = fixed_field::<16>(&fields, KEY_EXPECTED_DEVICE)?;
    let phase_code = uint_field(&fields, KEY_PHASE)?;
    let phase = InitPhase::from_code(phase_code).ok_or(corrupt("phase 未知"))?;
    let created_at = uint_field(&fields, KEY_CREATED_AT)?;
    // 缺省 = v1（ADR 0014 S3a 的旧 pending 兼容）；未知数字 fail-closed。
    let api_version = match fields.iter().find(|(key, _)| *key == KEY_API_VERSION) {
        Some((_, CdeValue::UInt(code))) => {
            RelayApiVersion::from_code(*code).ok_or(corrupt("api_version 未知"))?
        }
        Some(_) => return Err(corrupt("api_version 类型不符")),
        None => RelayApiVersion::V1,
    };
    Ok(InitPendingV1 {
        kind,
        mode,
        origin,
        api_version,
        vault_id,
        expected_device,
        phase,
        created_at,
    })
}

fn field(fields: &[(u64, CdeValue)], key: u64) -> Result<&CdeValue, SyncError> {
    fields
        .iter()
        .find(|(index, _)| *index == key)
        .map(|(_, value)| value)
        .ok_or(corrupt("缺字段"))
}

fn uint_field(fields: &[(u64, CdeValue)], key: u64) -> Result<u64, SyncError> {
    match field(fields, key)? {
        CdeValue::UInt(value) => Ok(*value),
        _ => Err(corrupt("整数字段类型不符")),
    }
}

fn fixed_field<const N: usize>(fields: &[(u64, CdeValue)], key: u64) -> Result<[u8; N], SyncError> {
    let CdeValue::Bytes(bytes) = field(fields, key)? else {
        return Err(corrupt("定长字节字段类型不符"));
    };
    bytes
        .as_slice()
        .try_into()
        .map_err(|_| corrupt("定长字节字段长度不符"))
}

/// 读取 pending：不存在 → `Ok(None)`；损坏/绑定不符 → 明确错误。
///
/// `expected_vault` 为 `Some` 时要求 pending 的 `vault_id` 与本地 vault 一致
/// （复制/错配的 pending 不能被当成"本次初始化"）。
pub fn load_pending(
    vault: &Vault,
    expected_vault: Option<[u8; 16]>,
) -> Result<Option<InitPendingV1>, SyncError> {
    let Some(bytes) = vault.get_meta(META_INIT_PENDING)? else {
        return Ok(None);
    };
    let pending = decode_pending_v1(&bytes)?;
    if let Some(expected) = expected_vault {
        if pending.vault_id != expected {
            return Err(SyncError::ConfigInvalid(
                "sync.init_pending 的 vault 绑定与本地 vault 不一致".to_owned(),
            ));
        }
    }
    Ok(Some(pending))
}

/// 开始一次初始化：写入 `Prepared` pending（**只在不存在时**插入）。
///
/// 语义（"重复 Create/Join 的幂等"）：
///
/// - 没有 pending → 写入并返回 `Ok`；
/// - 已有**完全相同**的 pending → `Ok`（崩溃/重试的续跑，不重复写、不覆盖）；
/// - 已有**不同**的 pending → [`SyncError::ConfigInvalid`]（未完成的初始化必须先对账，
///   不得用新初始化覆盖，否则会丢失"远端已建"的线索）。
pub fn begin_pending(vault: &Vault, pending: &InitPendingV1) -> Result<(), SyncError> {
    if pending.phase != InitPhase::Prepared {
        return Err(SyncError::ConfigInvalid(
            "初始 pending 必须是 prepared 阶段".to_owned(),
        ));
    }
    let bytes = encode_pending_v1(pending)?;
    let key = Value::Text(META_INIT_PENDING.to_owned());
    let statement = Statement::new(
        "INSERT INTO meta(key, value) SELECT ?1, ?2 \
         WHERE NOT EXISTS (SELECT 1 FROM meta WHERE key = ?1)",
        vec![key, Value::Blob(bytes)],
    );
    match vault.exec_transaction(&[statement], &[AffectedGuard::exactly(0, 1)]) {
        Ok(_) => Ok(()),
        Err(StorageError::GuardFailed { .. }) => {
            // 已存在：只有逐字节相同才算幂等续跑。
            let existing = load_pending(vault, Some(pending.vault_id))?.ok_or_else(|| {
                SyncError::ConfigInvalid(
                    "sync.init_pending 写入竞争后消失（不自动重建）".to_owned(),
                )
            })?;
            if existing == *pending {
                Ok(())
            } else {
                Err(SyncError::ConfigInvalid(format!(
                    "已有未完成的初始化 pending（{} / {}）：必须先启动对账，不得覆盖",
                    existing.kind.as_str(),
                    existing.phase.as_str()
                )))
            }
        }
        Err(error) => Err(SyncError::ConfigInvalid(format!(
            "sync.init_pending 写入失败：{error}"
        ))),
    }
}

/// 用 CAS 推进一次阶段（只进不退）。
pub fn advance_phase(
    vault: &Vault,
    expected: InitPhase,
    next: InitPhase,
) -> Result<InitPendingV1, SyncError> {
    if next <= expected {
        return Err(SyncError::ConfigInvalid(
            "初始化阶段只能前进（不允许回退/原地）".to_owned(),
        ));
    }
    persist_transition(vault, expected, |pending| {
        pending.phase = next;
    })
}

/// 绑定预期设备 id（只允许从"未确定"变为"已确定"）。
pub fn bind_expected_device(
    vault: &Vault,
    expected: InitPhase,
    device_id: [u8; 16],
) -> Result<InitPendingV1, SyncError> {
    if device_id == [0u8; 16] {
        return Err(SyncError::ConfigInvalid(
            "预期设备 id 不能是全 0".to_owned(),
        ));
    }
    let current = load_pending(vault, None)?.ok_or(SyncError::ConfigInvalid(
        "缺少 sync.init_pending".to_owned(),
    ))?;
    if current.has_expected_device() {
        return Err(SyncError::ConfigInvalid(
            "sync.init_pending 已绑定预期设备：不允许改写（可能换错设备）".to_owned(),
        ));
    }
    persist_transition(vault, expected, |pending| {
        pending.expected_device = device_id;
    })
}

/// 用 CAS 改写一个阶段内的字段（`expected` 是调用方读到的阶段）。
///
/// CAS 对象是**存储里的 canonical 字节**：并发写者先推进了阶段时这里失败，不覆盖赢家。
fn persist_transition(
    vault: &Vault,
    expected_phase: InitPhase,
    mutate: impl FnOnce(&mut InitPendingV1),
) -> Result<InitPendingV1, SyncError> {
    let current = load_pending(vault, None)?.ok_or(SyncError::ConfigInvalid(
        "缺少 sync.init_pending".to_owned(),
    ))?;
    if current.phase != expected_phase {
        return Err(SyncError::ConfigInvalid(format!(
            "sync.init_pending 阶段不符：期望 {}，实际 {}",
            expected_phase.as_str(),
            current.phase.as_str()
        )));
    }
    let old_bytes = encode_pending_v1(&current)?;
    let mut next = current;
    mutate(&mut next);
    let new_bytes = encode_pending_v1(&next)?;
    vault.exec_transaction(
        &[Statement::new(
            "UPDATE meta SET value = ?2 WHERE key = ?1 AND typeof(value) = 'blob' AND value = ?3",
            vec![
                Value::Text(META_INIT_PENDING.to_owned()),
                Value::Blob(new_bytes),
                Value::Blob(old_bytes),
            ],
        )],
        &[AffectedGuard::exactly(0, 1)],
    )
    .map_err(|error| match error {
        StorageError::GuardFailed { .. } => SyncError::ConfigInvalid(
            "sync.init_pending 被并发修改（不覆盖赢家，需重新对账）".to_owned(),
        ),
        other => SyncError::ConfigInvalid(format!("sync.init_pending 更新失败：{other}")),
    })?;
    Ok(next)
}

/// 初始化请求**尚未发出**时的安全清理（唯一的非 `Done` 清理入口）。
///
/// 为什么允许：`Prepared` 是写前记录，进入 `GenesisRequested` 之前不会发任何初始化
/// 请求（见模块文档的阶段表），因此此时远端**不可能**有本次创建的事实；继续卡在
/// "未完成初始化"只会让这个新路径永久不可用。它**只删 pending 记录**，不碰库数据/
/// 身份文件/用户目录；请求一旦发出（`GenesisRequested` 及之后）就只能走
/// [`crate::init_session::reconcile`] 的 finalize 路径。
pub fn abort_before_request(vault: &Vault) -> Result<(), SyncError> {
    let current = load_pending(vault, None)?.ok_or(SyncError::ConfigInvalid(
        "缺少 sync.init_pending".to_owned(),
    ))?;
    if current.phase != InitPhase::Prepared {
        return Err(SyncError::ConfigInvalid(format!(
            "初始化已进入 {} 阶段（请求可能已发出）：只允许在 prepared 阶段安全放弃",
            current.phase.as_str()
        )));
    }
    delete_pending_matching(vault, &current)
}

/// **远端初始化专用**：请求已发出、但**身份材料从未落盘**时的安全清理
/// （`Prepared` / `GenesisRequested` 两个阶段）。
///
/// 为什么允许在 `GenesisRequested` 清理：ADR 0013 §3 的"请求已发就必须对账"针对的是
/// **可继续的初始化**——远端可能已建立 vault/设备，但本地已经拿到了回执与身份材料，
/// 对账能补 finalize。若在拿到回执之前失败（TLS/策略拒绝、relay 明确拒绝、传输中断），
/// 本地**没有任何可用身份材料**：这个新库永远无法被 finalize，继续保留只会让这条
/// 新路径永久不可用（`RelayContext::open` 被 pending 挡住）。此时：
///
/// - 远端最多留下一个**没有身份、没有槽/checkpoint 的空 vault/孤儿设备**（不可用，
///   与 ADR 0009 既有失败语义一致），下次初始化会建新的；
/// - 本地只清 pending（不碰库数据/身份文件），随后由调用方删除本次新建的库文件——它
///   是本操作自己刚建出来的，不是用户数据。
///
/// `IdentityWritten`/`PinCommitted`/`Done` 一律拒绝：一旦身份落盘，初始化就**可以**
/// 被对账继续，删 pending 等于丢掉"远端可能已建"的线索。
///
/// `pub(crate)`：能力只给 `crate::remote_init` 的受限编排，不是应用层可随手调用的开关。
pub(crate) fn abort_before_identity(vault: &Vault) -> Result<(), SyncError> {
    let current = load_pending(vault, None)?.ok_or(SyncError::ConfigInvalid(
        "缺少 sync.init_pending".to_owned(),
    ))?;
    if current.phase >= InitPhase::IdentityWritten {
        return Err(SyncError::ConfigInvalid(format!(
            "初始化已进入 {} 阶段（身份已落盘）：只能走启动对账，不得放弃 pending",
            current.phase.as_str()
        )));
    }
    delete_pending_matching(vault, &current)
}

/// 删除**逐字节匹配**当前 pending 的那一行（CAS：并发修改时拒绝，不删赢家）。
fn delete_pending_matching(vault: &Vault, current: &InitPendingV1) -> Result<(), SyncError> {
    let bytes = encode_pending_v1(current)?;
    vault
        .exec_transaction(
            &[Statement::new(
                "DELETE FROM meta WHERE key = ?1 AND typeof(value) = 'blob' AND value = ?2",
                vec![
                    Value::Text(META_INIT_PENDING.to_owned()),
                    Value::Blob(bytes),
                ],
            )],
            &[AffectedGuard::exactly(0, 1)],
        )
        .map_err(|error| match error {
            StorageError::GuardFailed { .. } => SyncError::ConfigInvalid(
                "sync.init_pending 在放弃时被并发修改（不删赢家的状态）".to_owned(),
            ),
            other => SyncError::ConfigInvalid(format!("sync.init_pending 放弃失败：{other}")),
        })?;
    Ok(())
}

/// 清理 pending：**只允许** `Done`（ADR 0013 §3「清理只在 Done」）。
pub fn clear_pending(vault: &Vault) -> Result<(), SyncError> {
    let current = load_pending(vault, None)?.ok_or(SyncError::ConfigInvalid(
        "缺少 sync.init_pending".to_owned(),
    ))?;
    if current.phase != InitPhase::Done {
        return Err(SyncError::ConfigInvalid(format!(
            "初始化尚未完成（阶段 {}）：只在 done 阶段允许清理 pending",
            current.phase.as_str()
        )));
    }
    let bytes = encode_pending_v1(&current)?;
    vault
        .exec_transaction(
            &[Statement::new(
                "DELETE FROM meta WHERE key = ?1 AND typeof(value) = 'blob' AND value = ?2",
                vec![
                    Value::Text(META_INIT_PENDING.to_owned()),
                    Value::Blob(bytes),
                ],
            )],
            &[AffectedGuard::exactly(0, 1)],
        )
        .map_err(|error| match error {
            StorageError::GuardFailed { .. } => SyncError::ConfigInvalid(
                "sync.init_pending 在清理时被并发修改（不删赢家的状态）".to_owned(),
            ),
            other => SyncError::ConfigInvalid(format!("sync.init_pending 清理失败：{other}")),
        })?;
    Ok(())
}

/// 普通同步/管理操作的守卫：pending 未完成时**明确阻止**（ADR 0013 §3）。
///
/// `Done` 是"已完成、等清理"，允许放行（对账会清理它）。
pub fn ensure_no_pending_sync(vault: &Vault, vault_id: [u8; 16]) -> Result<(), SyncError> {
    let Some(pending) = load_pending(vault, Some(vault_id))? else {
        return Ok(());
    };
    if pending.phase == InitPhase::Done {
        return Ok(());
    }
    Err(SyncError::NeedsTrust(
        "本地初始化未完成（sync.init_pending）：拒绝启动普通同步/管理操作，请先完成或对账初始化",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const VAULT: [u8; 16] = [0x5a; 16];
    const DEVICE: [u8; 16] = [0x04; 16];

    fn fresh() -> (tempfile::TempDir, Vault) {
        let base = std::env::var("CARGO_TARGET_TMPDIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from("target/tmp"));
        std::fs::create_dir_all(&base).expect("target/tmp");
        let dir = tempfile::Builder::new()
            .prefix("init-journal-")
            .tempdir_in(&base)
            .expect("临时目录");
        let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("vault");
        (dir, vault)
    }

    fn pending(kind: InitKind) -> InitPendingV1 {
        InitPendingV1::prepared(
            kind,
            RelayMode::LocalDev,
            "http://127.0.0.1:18080",
            VAULT,
            DEVICE,
            1_700_000_000,
        )
    }

    fn raw(vault: &Vault) -> Option<Vec<u8>> {
        vault.get_meta(META_INIT_PENDING).expect("读 meta")
    }

    #[test]
    fn pending_roundtrips_through_canonical_cde() {
        for kind in [
            InitKind::NewGenesis,
            InitKind::FreshRecovery,
            InitKind::VerifiedMigration,
        ] {
            for mode in [RelayMode::LocalDev, RelayMode::RemoteTls] {
                for phase in [
                    InitPhase::Prepared,
                    InitPhase::GenesisRequested,
                    InitPhase::IdentityWritten,
                    InitPhase::PinCommitted,
                    InitPhase::Done,
                ] {
                    let mut value = pending(kind);
                    value.mode = mode;
                    value.phase = phase;
                    let bytes = encode_pending_v1(&value).expect("编码");
                    assert_eq!(decode_pending_v1(&bytes).expect("解码"), value);
                    // 编码是 canonical 的：同值同字节。
                    assert_eq!(encode_pending_v1(&value).expect("再编码"), bytes);
                }
            }
        }
    }

    /// ADR 0014 S3a：旧 pending（无键 8）按 v1 解读且编码逐字节不变；v2 才写键 8；
    /// 未知 api_version 明确拒绝（不静默回退）。
    #[test]
    fn pending_api_version_is_optional_and_v2_roundtrips() {
        let v1 = pending(InitKind::NewGenesis);
        assert_eq!(v1.api_version, RelayApiVersion::V1);
        let bytes_v1 = encode_pending_v1(&v1).expect("编码");
        // 历史 v1 编码（键 0..=7）：不含键 8（旧二进制/旧 golden 仍可读）。
        let fields = match decode_cde(&bytes_v1).expect("cde") {
            CdeValue::Map(fields) => fields,
            other => panic!("不是 map：{other:?}"),
        };
        assert!(fields.iter().all(|(key, _)| *key != KEY_API_VERSION));

        let v2 = InitPendingV1::prepared_with_api_version(
            InitKind::NewGenesis,
            RelayMode::LocalDev,
            "http://127.0.0.1:18080",
            VAULT,
            DEVICE,
            1_700_000_000,
            RelayApiVersion::V2,
        );
        let bytes_v2 = encode_pending_v1(&v2).expect("编码 v2");
        assert_ne!(bytes_v1, bytes_v2);
        assert_eq!(decode_pending_v1(&bytes_v2).expect("解码 v2"), v2);
        assert_eq!(
            decode_pending_v1(&bytes_v1).expect("解码旧").api_version,
            RelayApiVersion::V1
        );

        // 未知 api_version 值：明确错误。
        let mut fields = match decode_cde(&bytes_v2).expect("cde") {
            CdeValue::Map(fields) => fields,
            other => panic!("不是 map：{other:?}"),
        };
        for (key, value) in &mut fields {
            if *key == KEY_API_VERSION {
                *value = CdeValue::UInt(9);
            }
        }
        let broken = encode_cde(&CdeValue::Map(fields)).expect("cde");
        assert!(
            decode_pending_v1(&broken).is_err(),
            "未知 api_version 必须拒绝"
        );
    }

    #[test]
    fn corrupt_or_unknown_pending_is_an_error_not_missing() {
        let (_dir, vault) = fresh();
        let cde = |fields: Vec<(u64, CdeValue)>| encode_cde(&CdeValue::Map(fields)).expect("cde");
        let blobs: Vec<Vec<u8>> = vec![
            Vec::new(),
            vec![0xFF, 0x00],
            // 版本 2。
            cde(vec![
                (KEY_VERSION, CdeValue::UInt(2)),
                (KEY_KIND, CdeValue::UInt(1)),
                (KEY_MODE, CdeValue::UInt(1)),
                (KEY_ORIGIN, CdeValue::Text("http://127.0.0.1".to_owned())),
                (KEY_VAULT_ID, CdeValue::Bytes(VAULT.to_vec())),
                (KEY_EXPECTED_DEVICE, CdeValue::Bytes(DEVICE.to_vec())),
                (KEY_PHASE, CdeValue::UInt(1)),
                (KEY_CREATED_AT, CdeValue::UInt(1)),
            ]),
            // 未知 kind。
            cde(vec![
                (KEY_VERSION, CdeValue::UInt(INIT_PENDING_VERSION)),
                (KEY_KIND, CdeValue::UInt(9)),
                (KEY_MODE, CdeValue::UInt(1)),
                (KEY_ORIGIN, CdeValue::Text("http://127.0.0.1".to_owned())),
                (KEY_VAULT_ID, CdeValue::Bytes(VAULT.to_vec())),
                (KEY_EXPECTED_DEVICE, CdeValue::Bytes(DEVICE.to_vec())),
                (KEY_PHASE, CdeValue::UInt(1)),
                (KEY_CREATED_AT, CdeValue::UInt(1)),
            ]),
            // 未知 mode。
            cde(vec![
                (KEY_VERSION, CdeValue::UInt(INIT_PENDING_VERSION)),
                (KEY_KIND, CdeValue::UInt(1)),
                (KEY_MODE, CdeValue::UInt(7)),
                (KEY_ORIGIN, CdeValue::Text("http://127.0.0.1".to_owned())),
                (KEY_VAULT_ID, CdeValue::Bytes(VAULT.to_vec())),
                (KEY_EXPECTED_DEVICE, CdeValue::Bytes(DEVICE.to_vec())),
                (KEY_PHASE, CdeValue::UInt(1)),
                (KEY_CREATED_AT, CdeValue::UInt(1)),
            ]),
            // 未知 phase。
            cde(vec![
                (KEY_VERSION, CdeValue::UInt(INIT_PENDING_VERSION)),
                (KEY_KIND, CdeValue::UInt(1)),
                (KEY_MODE, CdeValue::UInt(1)),
                (KEY_ORIGIN, CdeValue::Text("http://127.0.0.1".to_owned())),
                (KEY_VAULT_ID, CdeValue::Bytes(VAULT.to_vec())),
                (KEY_EXPECTED_DEVICE, CdeValue::Bytes(DEVICE.to_vec())),
                (KEY_PHASE, CdeValue::UInt(17)),
                (KEY_CREATED_AT, CdeValue::UInt(1)),
            ]),
            // vault_id 长度错。
            cde(vec![
                (KEY_VERSION, CdeValue::UInt(INIT_PENDING_VERSION)),
                (KEY_KIND, CdeValue::UInt(1)),
                (KEY_MODE, CdeValue::UInt(1)),
                (KEY_ORIGIN, CdeValue::Text("http://127.0.0.1".to_owned())),
                (KEY_VAULT_ID, CdeValue::Bytes(vec![0; 15])),
                (KEY_EXPECTED_DEVICE, CdeValue::Bytes(DEVICE.to_vec())),
                (KEY_PHASE, CdeValue::UInt(1)),
                (KEY_CREATED_AT, CdeValue::UInt(1)),
            ]),
            // 未知字段。
            cde(vec![
                (KEY_VERSION, CdeValue::UInt(INIT_PENDING_VERSION)),
                (KEY_KIND, CdeValue::UInt(1)),
                (KEY_MODE, CdeValue::UInt(1)),
                (KEY_ORIGIN, CdeValue::Text("http://127.0.0.1".to_owned())),
                (KEY_VAULT_ID, CdeValue::Bytes(VAULT.to_vec())),
                (KEY_EXPECTED_DEVICE, CdeValue::Bytes(DEVICE.to_vec())),
                (KEY_PHASE, CdeValue::UInt(1)),
                (KEY_CREATED_AT, CdeValue::UInt(1)),
                (99, CdeValue::UInt(1)),
            ]),
        ];
        for blob in blobs {
            vault
                .set_meta(META_INIT_PENDING, &blob)
                .expect("写坏 pending");
            let error = load_pending(&vault, Some(VAULT)).expect_err("损坏必须报错，不能当缺失");
            assert!(
                matches!(error, SyncError::ConfigInvalid(_)),
                "必须是 ConfigInvalid：{error:?}"
            );
        }
    }

    #[test]
    fn begin_is_idempotent_only_for_the_same_pending() {
        let (_dir, vault) = fresh();
        let first = pending(InitKind::NewGenesis);
        begin_pending(&vault, &first).expect("首次");
        begin_pending(&vault, &first).expect("同一 pending 的重试必须幂等");
        let other = pending(InitKind::FreshRecovery);
        let error = begin_pending(&vault, &other).expect_err("不同 pending 必须拒绝");
        assert!(matches!(error, SyncError::ConfigInvalid(_)), "{error:?}");
        assert_eq!(load_pending(&vault, Some(VAULT)).expect("读"), Some(first));
        // 非 Prepared 阶段不得作为"初始 pending"写入。
        let mut not_prepared = pending(InitKind::FreshRecovery);
        not_prepared.phase = InitPhase::Done;
        assert!(begin_pending(&vault, &not_prepared).is_err());
    }

    #[test]
    fn vault_binding_mismatch_is_rejected() {
        let (_dir, vault) = fresh();
        begin_pending(&vault, &pending(InitKind::NewGenesis)).expect("写入");
        let error = load_pending(&vault, Some([0x99; 16])).expect_err("错绑必须拒绝");
        assert!(matches!(error, SyncError::ConfigInvalid(_)), "{error:?}");
    }

    #[test]
    fn phases_only_move_forward_and_bind_device_once() {
        let (_dir, vault) = fresh();
        begin_pending(
            &vault,
            &InitPendingV1::prepared(
                InitKind::FreshRecovery,
                RelayMode::LocalDev,
                "http://127.0.0.1:18080",
                VAULT,
                [0u8; 16],
                7,
            ),
        )
        .expect("写入");
        // 回退/原地都拒绝。
        assert!(advance_phase(&vault, InitPhase::Prepared, InitPhase::Prepared).is_err());
        let bound = bind_expected_device(&vault, InitPhase::Prepared, DEVICE).expect("绑定设备");
        assert_eq!(bound.expected_device, DEVICE);
        assert_eq!(bound.phase, InitPhase::Prepared, "绑定设备不推进阶段");
        // 已绑定后再改设备 = 矛盾，拒绝（不能让 pending 悄悄指向另一台设备）。
        assert!(bind_expected_device(&vault, InitPhase::Prepared, [0x77; 16]).is_err());
        // 阶段期望不符（调用方读到的是旧阶段）→ 拒绝。
        let error = advance_phase(&vault, InitPhase::GenesisRequested, InitPhase::PinCommitted)
            .expect_err("阶段期望不符必须拒绝");
        assert!(matches!(error, SyncError::ConfigInvalid(_)), "{error:?}");
        // 正常前进。
        advance_phase(&vault, InitPhase::Prepared, InitPhase::GenesisRequested).expect("前进");
        advance_phase(
            &vault,
            InitPhase::GenesisRequested,
            InitPhase::IdentityWritten,
        )
        .expect("前进");
        advance_phase(&vault, InitPhase::IdentityWritten, InitPhase::PinCommitted).expect("前进");
        advance_phase(&vault, InitPhase::PinCommitted, InitPhase::Done).expect("前进");
        assert_eq!(
            load_pending(&vault, Some(VAULT))
                .expect("读")
                .expect("在")
                .phase,
            InitPhase::Done
        );
    }

    #[test]
    fn abort_before_request_only_allows_prepared() {
        let (_dir, vault) = fresh();
        begin_pending(&vault, &pending(InitKind::NewGenesis)).expect("写入");
        abort_before_request(&vault).expect("prepared 阶段允许安全放弃");
        assert!(raw(&vault).is_none(), "放弃后不得留下 pending");

        begin_pending(&vault, &pending(InitKind::NewGenesis)).expect("再次写入");
        advance_phase(&vault, InitPhase::Prepared, InitPhase::GenesisRequested).expect("前进");
        let error = abort_before_request(&vault).expect_err("请求已发出后不得放弃");
        assert!(matches!(error, SyncError::ConfigInvalid(_)), "{error:?}");
        assert!(raw(&vault).is_some(), "拒绝路径不得删 pending");
    }

    #[test]
    fn clear_is_allowed_only_at_done() {
        let (_dir, vault) = fresh();
        begin_pending(&vault, &pending(InitKind::NewGenesis)).expect("写入");
        let error = clear_pending(&vault).expect_err("未完成不得清理");
        assert!(matches!(error, SyncError::ConfigInvalid(_)), "{error:?}");
        assert!(raw(&vault).is_some(), "拒绝路径不得删 pending");

        advance_phase(&vault, InitPhase::Prepared, InitPhase::GenesisRequested).expect("前进");
        advance_phase(
            &vault,
            InitPhase::GenesisRequested,
            InitPhase::IdentityWritten,
        )
        .expect("前进");
        advance_phase(&vault, InitPhase::IdentityWritten, InitPhase::PinCommitted).expect("前进");
        assert!(clear_pending(&vault).is_err(), "PinCommitted 仍不得清理");
        advance_phase(&vault, InitPhase::PinCommitted, InitPhase::Done).expect("前进");
        clear_pending(&vault).expect("done 阶段清理");
        assert!(raw(&vault).is_none(), "清理后必须没有 pending");
        // 第二次清理是明确错误（不能把"没有 pending"当成"清理成功"）。
        assert!(clear_pending(&vault).is_err());
    }

    #[test]
    fn sync_guard_blocks_unfinished_and_allows_done() {
        let (_dir, vault) = fresh();
        // 没有 pending：放行。
        ensure_no_pending_sync(&vault, VAULT).expect("无 pending");
        begin_pending(&vault, &pending(InitKind::NewGenesis)).expect("写入");
        for phase in [
            InitPhase::Prepared,
            InitPhase::GenesisRequested,
            InitPhase::IdentityWritten,
            InitPhase::PinCommitted,
        ] {
            if phase.as_code() > 1 {
                let previous = InitPhase::from_code(phase.as_code() - 1).expect("上一阶段");
                advance_phase(&vault, previous, phase).expect("前进");
            }
            let error = ensure_no_pending_sync(&vault, VAULT).expect_err("未完成必须阻止普通同步");
            assert!(matches!(error, SyncError::NeedsTrust(_)), "{error:?}");
        }
        advance_phase(&vault, InitPhase::PinCommitted, InitPhase::Done).expect("前进");
        ensure_no_pending_sync(&vault, VAULT).expect("done 阶段放行（等清理）");
    }

    /// 崩溃矩阵的"存储层"部分：每个阶段落盘后重开库，pending 逐字节可读、阶段不回退。
    #[test]
    fn every_phase_survives_a_reopen() {
        let (_dir, vault) = fresh();
        let path = vault.path().to_path_buf();
        begin_pending(&vault, &pending(InitKind::NewGenesis)).expect("写入");
        let phases = [
            InitPhase::Prepared,
            InitPhase::GenesisRequested,
            InitPhase::IdentityWritten,
            InitPhase::PinCommitted,
            InitPhase::Done,
        ];
        let mut vault = vault;
        for (index, phase) in phases.iter().enumerate() {
            if index > 0 {
                advance_phase(&vault, phases[index - 1], *phase).expect("前进");
            }
            let bytes = raw(&vault).expect("持久化");
            drop(vault);
            vault = Vault::open(&path).expect("重开");
            assert_eq!(
                raw(&vault).expect("读"),
                bytes,
                "阶段 {phase:?} 必须逐字节持久"
            );
            assert_eq!(
                load_pending(&vault, Some(VAULT))
                    .expect("读")
                    .expect("在")
                    .phase,
                *phase
            );
            if *phase == InitPhase::Done {
                clear_pending(&vault).expect("清理");
            }
        }
    }
}
