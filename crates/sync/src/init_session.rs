//! ADR 0013 **S2b**：受限初始化能力（`NewGenesis` / `FreshRecovery` / `VerifiedMigration`）。
//!
//! ## 为什么需要独立入口
//!
//! 普通路径（[`crate::relay_context::RelayContext::open`]）**没有** unpin / None / reset
//! 分支：`remote_tls` 缺 durable `sync.control_pin` 直接 [`SyncError::NeedsTrust`]，一个
//! 请求都不会发。但"第一次建立信任"必然发生在还没有 pin 的时候，因此必须有一个**受限、
//! 可审计**的入口完成它——本模块就是那个入口，能力只有三种：
//!
//! | 能力 | 信任起点（不可伪造的本地材料） | 允许做的事 |
//! | --- | --- | --- |
//! | [`InitKind::NewGenesis`] | 本地**已提交**的 genesis 材料（证书 + 管理私钥） | `POST /v1/vaults` + 用本地复算的 rev1 `record_hash` 验证链前缀 + 提交强 pin |
//! | [`InitKind::FreshRecovery`] | 恢复码（派生管理锚） | `self_enroll`（唯一允许的 unpinned `/control` 消费）+ 认证 bootstrap + 提交 record/bootstrap 同视图 pin |
//! | [`InitKind::VerifiedMigration`] | 已有身份 + 旧 pin（bootstrap 摘要/身份下界） | 重新验证控制链并把旧 pin 转成 canonical `sync.control_pin`；条件不足 → `NeedsTrust`/`Unsupported` |
//!
//! 关键约束：
//!
//! - [`InitSession`] 字段全私有，构造函数**只接受**上述材料（没有 `new_unpinned()` /
//!   `reset()` / `trust_any()`）；普通客户端也构造不出"允许 unpinned 消费 `/control`"的
//!   实例（[`crate::http_relay::HttpRelayClient::with_unpinned_bootstrap_control`] 是
//!   `pub(crate)`，唯一调用点在本模块的 FreshRecovery 构造里）；
//! - 初始化前先写 durable pending（[`crate::init_journal`]），再发网络请求；阶段只进不退；
//! - 失败**不自动重试/不自动重建**：崩溃后由 [`reconcile`] 用已有材料完成 finalize，
//!   证明不了就是 `NeedsTrust`/`ConfigInvalid`，不删用户数据；
//! - 传输统一来自 [`RelayContext::transport`]（production / local_dev 由模式决定），
//!   本模块不自己拼 `StdTcpTransport`。
//!
//! ## 与 app 的关系
//!
//! app 侧：`local_dev` 的 Create/Join 用 `create/join_deferring_identity` 让 pending 停在
//! `PinCommitted`，身份"临时 + 原子 rename"落盘后调用 [`mark_identity_written`] +
//! [`finish_identity_written`]；`remote_tls`（ADR 0013 S3b）由 [`crate::remote_init`]
//! 在身份回调成功后真正推进 `GenesisRequested → IdentityWritten`，再提交 pin。
//! `local_dev` 先提交 pin 再落盘身份，`PinCommitted` 在阶段序上晚于 `IdentityWritten`，
//! 因此 mark 幂等、不回退；两条顺序都被 [`finalize_genesis`] / [`commit_recovery_pin`]
//! 的前驱阶段集合接受。

use crate::bootstrap::{self, BootstrapPins, BootstrapReport};
use crate::control_pin::{commit_verified_head, load_control_pin, PinV1};
use crate::dev_local::{peer_bytes_of, root_of};
use crate::device::{DeviceSigningKey, ManagementKey};
use crate::engine::RELAY_CURSOR_META;
use crate::error::SyncError;
use crate::genesis::{self, GenesisMaterials, GenesisReceipt};
use crate::http_relay::{ControlPin, HttpRelayClient, HttpRelayConfig, StdTcpTransport};
use crate::identity::LocalIdentityV1;
use crate::ids::PeerId;
use crate::init_journal::{self, InitKind, InitPendingV1, InitPhase};
use crate::recovery::RecoverySeed;
use crate::relay::RelayClient as _;
use crate::relay_context::RelayContext;
use crate::{DeviceId, VaultId};
use menghunote_storage::Vault;

/// NewGenesis 成功后的结果：回执 + 已建立身份 + 已提交的强 pin。
pub struct GenesisInitReport {
    /// relay 回执（公开事实）。
    pub receipt: GenesisReceipt,
    /// 新设备身份（返回给调用方按 app 的原子落盘方式持久化）。
    pub identity: LocalIdentityV1,
    /// 已 durable 提交的 canonical pin（rev1 record + 无 bootstrap 锚）。
    pub pin: PinV1,
}

/// FreshRecovery 恢复出的身份（已 bootstrap 导入基线，pins/cursor 已 durable）。
pub struct RecoveredIdentity {
    /// 新设备身份。
    pub identity: LocalIdentityV1,
    /// bootstrap 报告（覆盖水位/是否使用 fallback）。
    pub report: BootstrapReport,
}

/// VerifiedMigration 的结果。
pub struct MigrationReport {
    /// 迁移后的 canonical pin。
    pub pin: PinV1,
}

/// 受限初始化能力的会话（不可伪造；构造需要上面的材料证明）。
pub struct InitSession {
    kind: InitKind,
    context: RelayContext,
    vault_id: [u8; 16],
    manager_anchor: [u8; 32],
    /// 预期落盘的设备 id；`FreshRecovery` 在 enroll 前不知道，用全 0 表示"尚未确定"。
    expected_device: [u8; 16],
    /// FreshRecovery 的恢复种子（`Zeroizing` 语义由 `Secret32` 承担；不实现 Debug）。
    seed: Option<RecoverySeed>,
    /// **测试专用**：替换本地复算的 rev1 摘要（验证"错 rev1 必须拒绝"）。
    ///
    /// 只会让初始化**失败**（fail-closed），不能把未验证的 head 提升成信任；
    /// 生产路径不设置它（默认 `None` = 用本地证书如实复算）。
    rev1_override: Option<[u8; 32]>,
}

impl InitSession {
    /// **A. NewGenesis**：信任起点是本地已提交的 genesis 材料（证书 + 管理私钥）。
    ///
    /// 不联网：只校验材料自洽（管理公钥 = 身份锚、vault 绑定）。
    pub fn new_genesis(
        context: RelayContext,
        materials: &GenesisMaterials,
    ) -> Result<Self, SyncError> {
        let manager_anchor = materials.manager_pub();
        if manager_anchor == [0u8; 32] {
            return Err(SyncError::NeedsTrust(
                "genesis 材料缺管理公钥：无法作为控制链信任锚",
            ));
        }
        materials
            .signed_cert
            .verify(&manager_anchor)
            .map_err(|_| SyncError::Certificate("genesis 自签证书无法被本地管理公钥验证"))?;
        check_context_manager(&context, manager_anchor)?;
        Ok(Self {
            kind: InitKind::NewGenesis,
            context,
            vault_id: materials.vault_id,
            manager_anchor,
            expected_device: materials.device_id,
            seed: None,
            rev1_override: None,
        })
    }

    /// **B. FreshRecovery**：信任起点是恢复码（派生管理锚，不信任 relay 的任何公钥）。
    pub fn new_fresh_recovery(
        context: RelayContext,
        vault_id: VaultId,
        seed: &RecoverySeed,
    ) -> Result<Self, SyncError> {
        let manager = ManagementKey::from_recovery(seed, &vault_id)?;
        let manager_anchor = manager.public_key();
        check_context_manager(&context, manager_anchor)?;
        Ok(Self {
            kind: InitKind::FreshRecovery,
            context,
            vault_id,
            manager_anchor,
            expected_device: [0u8; 16],
            seed: Some(seed.clone()),
            rev1_override: None,
        })
    }

    /// **C. VerifiedMigration**：信任起点是已有受保护身份 + 旧 pin。
    ///
    /// 条件（缺任一条都明确拒绝，不降级成 unpinned）：
    ///
    /// - 身份必须带非零 `pinned_manager` / `pinned_instance` / `control_rev`；
    /// - 已有 canonical pin 且带 `record.hash` → [`SyncError::Unsupported`]（无需迁移，
    ///   不重复写）；
    /// - `remote_tls` 下必须有**同 rev 的信任锚**（旧 bootstrap 摘要或旧 pin 的 bootstrap
    ///   锚），否则 [`SyncError::NeedsTrust`]——identity 的 `(instance, rev)` 只是下界，
    ///   不能证明同 rev 历史完整性（ADR 0013 §3「旧 A」）；
    /// - `local_dev` 保持既有开发语义（身份下界即可）。
    pub fn new_verified_migration(
        context: RelayContext,
        vault: &Vault,
        identity: &LocalIdentityV1,
    ) -> Result<Self, SyncError> {
        if identity.pinned_manager == [0u8; 32] {
            return Err(SyncError::NeedsTrust(
                "身份缺管理公钥信任锚：无法迁移（拒绝以 unpinned 方式消费 /control）",
            ));
        }
        if identity.pinned_instance == [0u8; 16] || identity.control_rev == 0 {
            return Err(SyncError::NeedsTrust(
                "身份缺 (instance, control_rev) 下界：无法迁移",
            ));
        }
        if let Some(pin) = load_control_pin(vault, identity.vault_id)? {
            if pin.record.hash.is_some() {
                return Err(SyncError::Unsupported(
                    "durable pin 已是 canonical v1 且带 record hash：无需迁移",
                ));
            }
        }
        let has_anchor = BootstrapPins::from_vault(vault)?
            .control_anchor()?
            .is_some()
            || load_control_pin(vault, identity.vault_id)?
                .and_then(|pin| pin.bootstrap)
                .is_some();
        if !has_anchor && context.mode == crate::relay_policy::RelayMode::RemoteTls {
            return Err(SyncError::NeedsTrust(
                "迁移缺少同 rev 信任锚（bootstrap 摘要）：identity 的 (instance, rev) 只是下界",
            ));
        }
        check_context_manager(&context, identity.pinned_manager)?;
        Ok(Self {
            kind: InitKind::VerifiedMigration,
            context,
            vault_id: identity.vault_id,
            manager_anchor: identity.pinned_manager,
            expected_device: identity.device_id,
            seed: None,
            rev1_override: None,
        })
    }

    /// 能力种类。
    #[must_use]
    pub const fn kind(&self) -> InitKind {
        self.kind
    }

    /// 本次初始化建立的 vault id。
    #[must_use]
    pub const fn vault_id(&self) -> [u8; 16] {
        self.vault_id
    }

    /// 本次初始化使用的 relay context（模式/origin/预算）。
    #[must_use]
    pub const fn context(&self) -> &RelayContext {
        &self.context
    }

    /// 管理权信任锚。
    #[must_use]
    pub const fn manager_anchor(&self) -> [u8; 32] {
        self.manager_anchor
    }

    /// **测试专用**：把本地复算的 rev1 摘要换掉（只会让初始化 fail-closed）。
    #[doc(hidden)]
    #[must_use]
    pub fn with_test_rev1_override(mut self, hash: [u8; 32]) -> Self {
        self.rev1_override = Some(hash);
        self
    }

    /// 写 durable pending（`Prepared`）：同一 pending 重试幂等，不同 pending 明确拒绝。
    pub fn prepare_pending(
        &self,
        vault: &Vault,
        created_at: u64,
    ) -> Result<InitPendingV1, SyncError> {
        let pending = InitPendingV1::prepared_with_api_version(
            self.kind,
            self.context.mode,
            self.context.origin_str(),
            self.vault_id,
            self.expected_device,
            created_at,
            self.context.api_version(),
        );
        init_journal::begin_pending(vault, &pending)?;
        self.load_pending(vault)
    }

    // ------------------------------------------------------------------
    // A. NewGenesis
    // ------------------------------------------------------------------

    /// 发 `POST /v1/vaults`（先 durable 记录"请求已发出"）。**仅 v1 面**。
    ///
    /// 分成 request / finalize 两步的理由：崩溃点必须可构造、可对账。`request` 成功即
    /// 表示远端 vault 已存在（之后失败**不得**删本地数据/重建），`finalize` 只做本地验证
    /// 与强 pin 提交，不再需要网络写入。
    pub fn request_genesis(
        &self,
        vault: &Vault,
        materials: &GenesisMaterials,
        setup_secret: &str,
    ) -> Result<GenesisReceipt, SyncError> {
        self.request_genesis_inner(
            vault,
            materials,
            GenesisCreateAuth::SetupSecret(setup_secret),
        )
    }

    /// 发 **v2** `POST /v2/vaults`（一次性创建票据；ADR 0014 S3a）。
    ///
    /// 与 v1 的差别只有授权字段与路径；durable pending/回执校验/pin 提交完全同一套。
    /// 票据**本身**只在内存里用一次、不写 pending（崩溃后由已存在 vault 的
    /// `/state + /control` 对账补齐，不需要重发票据）。
    pub fn request_genesis_with_ticket(
        &self,
        vault: &Vault,
        materials: &GenesisMaterials,
        ticket: &crate::create_link::CreateTicket,
    ) -> Result<GenesisReceipt, SyncError> {
        self.request_genesis_inner(vault, materials, GenesisCreateAuth::Ticket(ticket))
    }

    fn request_genesis_inner(
        &self,
        vault: &Vault,
        materials: &GenesisMaterials,
        auth: GenesisCreateAuth<'_>,
    ) -> Result<GenesisReceipt, SyncError> {
        self.require_kind(InitKind::NewGenesis)?;
        // 面与授权方式必须一致：v1 只认 setup secret，v2 只认票据（不提供降级组合）。
        match (self.context.api_version(), &auth) {
            (crate::api_version::RelayApiVersion::V1, GenesisCreateAuth::SetupSecret("")) => {
                return Err(SyncError::ConfigInvalid(
                    "v1 genesis 需要非空 setup secret".to_owned(),
                ))
            }
            (crate::api_version::RelayApiVersion::V1, GenesisCreateAuth::SetupSecret(_)) => {}
            (crate::api_version::RelayApiVersion::V2, GenesisCreateAuth::Ticket(_)) => {}
            (crate::api_version::RelayApiVersion::V1, GenesisCreateAuth::Ticket(_)) => {
                return Err(SyncError::ConfigInvalid(
                    "v1 context 不接受创建票据：票据创建必须显式选择 v2 面".to_owned(),
                ))
            }
            (crate::api_version::RelayApiVersion::V2, GenesisCreateAuth::SetupSecret(_)) => {
                return Err(SyncError::ConfigInvalid(
                    "v2 context 不接受 setup secret：v2 创建必须使用一次性票据".to_owned(),
                ))
            }
        }
        if materials.vault_id != self.vault_id
            || materials.device_id != self.expected_device
            || materials.manager_pub() != self.manager_anchor
        {
            return Err(SyncError::ConfigInvalid(
                "genesis 材料与 InitSession 绑定的 vault/device/管理锚不一致".to_owned(),
            ));
        }
        let pending = self.load_pending(vault)?;
        if pending.phase != InitPhase::Prepared {
            return Err(SyncError::ConfigInvalid(format!(
                "genesis 已经请求过（阶段 {}）：不重复创建 vault",
                pending.phase.as_str()
            )));
        }
        // 写前记录：崩溃后能证明"请求可能已发出"，因此不允许当作"没发生过"。
        init_journal::advance_phase(vault, InitPhase::Prepared, InitPhase::GenesisRequested)?;
        let mut transport = self.context.transport()?;
        let receipt = match auth {
            GenesisCreateAuth::SetupSecret(secret) => {
                genesis::submit(&mut transport, materials, secret)?
            }
            GenesisCreateAuth::Ticket(ticket) => {
                genesis::submit_with_ticket(&mut transport, materials, ticket.as_bytes())?
            }
        };
        if receipt.instance_id == [0u8; 16] || receipt.log_id == [0u8; 16] {
            return Err(SyncError::RelayWire("genesis 回执的 instance/log id 为零"));
        }
        if receipt.control_rev != 1 {
            // 新库的第一条控制记录必然是 rev1（rev1 record_hash 公式的前提）。
            return Err(SyncError::RelayWire("genesis 回执的 control_rev 不是 1"));
        }
        Ok(receipt)
    }

    /// 用本地提交证书复算 rev1 `record_hash`，验证 relay 当前链/前缀后 durable 提交强 pin。
    ///
    /// 接受 `GenesisRequested`（local_dev：先提 pin 后落盘身份）与 `IdentityWritten`
    /// （remote_tls：先原子落盘身份再提 pin，ADR 0013 S3b 的规范顺序）两个前驱阶段。
    pub fn finalize_genesis(
        &self,
        vault: &Vault,
        materials: &GenesisMaterials,
        receipt: &GenesisReceipt,
    ) -> Result<GenesisInitReport, SyncError> {
        self.require_kind(InitKind::NewGenesis)?;
        if materials.vault_id != self.vault_id || materials.device_id != self.expected_device {
            return Err(SyncError::ConfigInvalid(
                "genesis 材料与 InitSession 绑定的 vault/device 不一致".to_owned(),
            ));
        }
        let pending = self.load_pending(vault)?;
        if !matches!(
            pending.phase,
            InitPhase::GenesisRequested | InitPhase::IdentityWritten
        ) {
            return Err(SyncError::ConfigInvalid(format!(
                "genesis 未请求成功或身份未落盘（阶段 {}）：不能提交 pin",
                pending.phase.as_str()
            )));
        }
        let record_hash = match self.rev1_override {
            Some(hash) => hash,
            None => genesis::rev1_record_hash(materials.certificate())?,
        };
        let pin =
            ControlPin::new(receipt.instance_id, receipt.control_rev).with_record_hash(record_hash);
        let transport = self.context.transport()?;
        let mut client = self.client(
            transport,
            materials.device_id,
            materials.signing_key.clone(),
            Some(pin),
        );
        // 全链验签 + 分页连续性 + 本地 pin 的 rev1 前缀检查（同 rev 比 hash、更高 rev 比
        // 前缀）。`VerifiedHead` 只能由已验证路径构造，外部无法伪造裸响应 hash。
        let head = client.verified_head()?;
        let expected = load_control_pin(vault, self.vault_id)?;
        commit_verified_head(vault, expected.as_ref(), head)?;
        let identity = materials.identity(receipt.instance_id, receipt.control_rev)?;
        init_journal::advance_phase(vault, pending.phase, InitPhase::PinCommitted)?;
        Ok(GenesisInitReport {
            receipt: receipt.clone(),
            identity,
            pin: self.load_pin(vault)?,
        })
    }

    // ------------------------------------------------------------------
    // B. FreshRecovery
    // ------------------------------------------------------------------

    /// 恢复码自批新设备 + 认证 bootstrap（**唯一**允许 unpinned `/control` 消费的路径）。
    ///
    /// 顺序（ADR 0013 §3「新 B」）：恢复码管理锚 → TLS/传输 → `self_enroll` → 认证后的
    /// 控制链/checkpoint/对象验证（`bootstrap::run_with_pins` 自带 pins/cursor 的单事务
    /// 推进）→ 构造身份。pin 提交在下一次调用 [`Self::commit_recovery_pin`] 里做，两者
    /// 之间崩溃由 pending + [`reconcile`] 对账。
    pub fn recover_identity(
        &self,
        vault: &Vault,
        now: u64,
    ) -> Result<RecoveredIdentity, SyncError> {
        self.require_kind(InitKind::FreshRecovery)?;
        let seed = self.seed.as_ref().ok_or(SyncError::ConfigInvalid(
            "FreshRecovery 会话缺恢复种子".to_owned(),
        ))?;
        let pending = self.load_pending(vault)?;
        if pending.phase != InitPhase::Prepared {
            return Err(SyncError::ConfigInvalid(format!(
                "fresh recovery 已经请求过（阶段 {}）：不重复 enroll",
                pending.phase.as_str()
            )));
        }
        init_journal::advance_phase(vault, InitPhase::Prepared, InitPhase::GenesisRequested)?;
        let code = seed.to_code()?;
        let mut transport = self.context.transport()?;
        let api = self.context.api();
        let enrolled =
            crate::recovery_enroll::self_enroll(&mut transport, &api, self.vault_id, &code, now)?;
        if enrolled.vault_id != self.vault_id {
            return Err(SyncError::RelayWire("自批返回的 vault 与请求不一致"));
        }
        let peer_id = PeerId::from_bytes(peer_bytes_of(enrolled.peer_id));
        if peer_id.loro_u64() != enrolled.peer_id {
            return Err(SyncError::LocalCorrupt(
                "自批 peer id 无法按 ADR 0009 的映射稳定复现",
            ));
        }
        // enroll 之后才知道新设备 id：补进 pending（只允许从"未确定"绑定一次）。
        init_journal::bind_expected_device(vault, InitPhase::GenesisRequested, enrolled.device_id)?;

        let root = enrolled.root.clone();
        let mut client = self
            .unpinned_client(enrolled.device_id, enrolled.signing_key.clone())?
            .with_root(root.clone());
        let pins = BootstrapPins::from_vault(vault)?;
        let report = bootstrap::run_with_pins(
            vault,
            &mut client,
            self.vault_id,
            peer_id.loro_u64(),
            &root,
            &pins,
        )?;
        let state = client.state()?;
        if report.covered_seq > state.head_seq {
            return Err(SyncError::RelayWire("bootstrap 覆盖水位高于 relay head"));
        }
        let identity = LocalIdentityV1 {
            vault_id: self.vault_id,
            device_id: enrolled.device_id,
            peer_id,
            ed25519_seed: enrolled.signing_key.seed().clone(),
            x25519_secret: enrolled.hpke_key.secret().clone(),
            current_epoch: root.key_epoch,
            current_root: root.root.clone(),
            // 普通设备没有管理私钥：恢复码只在 enroll 时用过，不留在身份里。
            manager_seed: None,
            pinned_instance: state.instance_id,
            pinned_manager: self.manager_anchor,
            control_rev: state.control_rev,
            pending_rotation: None,
        };
        Ok(RecoveredIdentity { identity, report })
    }

    /// 用**同一验证视图**提交 record pin 与 bootstrap 锚（ADR 0013 §3）。
    ///
    /// 锚来自 bootstrap 刚刚认证过的 `sync.pinned_*`；`verified_head_with_anchor` 会按锚
    /// 自己的 rev/epoch/log + 管理链前缀再复算一次，因此"bootstrap 成功但 record 视图
    /// 不同"会在这里被拒绝，而不是留下一对互相矛盾的 durable 事实。
    pub fn commit_recovery_pin(
        &self,
        vault: &Vault,
        identity: &LocalIdentityV1,
    ) -> Result<PinV1, SyncError> {
        self.require_kind(InitKind::FreshRecovery)?;
        let pending = self.load_pending(vault)?;
        if !matches!(
            pending.phase,
            InitPhase::GenesisRequested | InitPhase::IdentityWritten
        ) {
            return Err(SyncError::ConfigInvalid(format!(
                "fresh recovery 尚未完成 bootstrap 或身份未落盘（阶段 {}）：不能提交 pin",
                pending.phase.as_str()
            )));
        }
        self.verify_identity_binding(&pending, identity)?;
        let pin = self.verify_and_commit_pin(vault, identity)?;
        init_journal::advance_phase(vault, pending.phase, InitPhase::PinCommitted)?;
        Ok(pin)
    }

    // ------------------------------------------------------------------
    // C. VerifiedMigration
    // ------------------------------------------------------------------

    /// 重新验证控制链并把旧 pin/身份下界迁移成 canonical `sync.control_pin`。
    ///
    /// 不重复 genesis/enroll，只做"已有材料 → 已验证 head → durable pin"：
    ///
    /// - 已有完整 pin → 构造期就 [`SyncError::Unsupported`]；
    /// - 旧 pin 只有下界（无 `record.hash`）→ 用身份下界 + bootstrap 锚验证控制链，
    ///   成功后写入带 `record.hash` 的 canonical pin；
    /// - 视图证明不了（锚缺失/链前缀不符）→ `NeedsTrust`/`Equivocation`，不写 pin。
    pub fn run_migration(
        &self,
        vault: &Vault,
        identity: &LocalIdentityV1,
        now: u64,
    ) -> Result<MigrationReport, SyncError> {
        self.require_kind(InitKind::VerifiedMigration)?;
        let pending = self.prepare_pending(vault, now)?;
        if pending.phase != InitPhase::Prepared {
            return Err(SyncError::ConfigInvalid(format!(
                "verified migration 已经执行过（阶段 {}）",
                pending.phase.as_str()
            )));
        }
        self.verify_identity_binding(&pending, identity)?;
        init_journal::advance_phase(vault, InitPhase::Prepared, InitPhase::GenesisRequested)?;
        let pin = self.verify_and_commit_pin(vault, identity)?;
        init_journal::advance_phase(vault, InitPhase::GenesisRequested, InitPhase::PinCommitted)?;
        Ok(MigrationReport { pin })
    }

    // ------------------------------------------------------------------
    // 收尾
    // ------------------------------------------------------------------

    /// 初始化完成：推进到 `Done` 并清理 pending（**清理只在 Done**）。
    pub fn finish(&self, vault: &Vault) -> Result<(), SyncError> {
        let pending = self.load_pending(vault)?;
        match pending.phase {
            InitPhase::Done => {}
            InitPhase::PinCommitted | InitPhase::IdentityWritten => {
                init_journal::advance_phase(vault, pending.phase, InitPhase::Done)?;
            }
            other => {
                return Err(SyncError::ConfigInvalid(format!(
                    "初始化尚未完成（阶段 {}）：不得清理 pending",
                    other.as_str()
                )))
            }
        }
        init_journal::clear_pending(vault)?;
        Ok(())
    }

    // ------------------------------------------------------------------
    // 内部助手
    // ------------------------------------------------------------------

    // 说明：pending 的 `IdentityWritten` 推进不是 `InitSession` 的方法，而是模块级函数
    // [`mark_identity_written`] / [`finish_identity_written`]：身份落盘发生在 app 侧
    // （临时文件 + 原子 rename），本 crate 不拥有那个文件，只能由调用方在落盘后推进。

    fn require_kind(&self, expected: InitKind) -> Result<(), SyncError> {
        if self.kind != expected {
            return Err(SyncError::Unsupported(
                "该 InitSession 不支持这个初始化操作",
            ));
        }
        Ok(())
    }

    fn load_pending(&self, vault: &Vault) -> Result<InitPendingV1, SyncError> {
        init_journal::load_pending(vault, Some(self.vault_id))?.ok_or(SyncError::ConfigInvalid(
            "缺少 sync.init_pending（初始化必须先写 durable pending）".to_owned(),
        ))
    }

    fn load_pin(&self, vault: &Vault) -> Result<PinV1, SyncError> {
        load_control_pin(vault, self.vault_id)?.ok_or(SyncError::PinSaveFailed(
            "提交后读回 sync.control_pin 失败".to_owned(),
        ))
    }

    fn verify_identity_binding(
        &self,
        pending: &InitPendingV1,
        identity: &LocalIdentityV1,
    ) -> Result<(), SyncError> {
        if identity.vault_id != pending.vault_id {
            return Err(SyncError::ConfigInvalid(
                "身份与 sync.init_pending 的 vault 绑定不一致".to_owned(),
            ));
        }
        if pending.has_expected_device() && identity.device_id != pending.expected_device {
            return Err(SyncError::ConfigInvalid(
                "身份与 sync.init_pending 的预期设备不一致".to_owned(),
            ));
        }
        Ok(())
    }

    /// 普通（带 pin）客户端：pin 决议由调用方给出，本模块不做 unpinned 分支。
    fn client(
        &self,
        transport: StdTcpTransport,
        device_id: DeviceId,
        signing_key: DeviceSigningKey,
        pin: Option<ControlPin>,
    ) -> HttpRelayClient<StdTcpTransport> {
        let config = HttpRelayConfig {
            base_url: self.context.origin_str(),
            api_version: self.context.api_version(),
            vault_id: self.vault_id,
            device_id,
            manager_pub: Some(self.manager_anchor),
        };
        let mut client = HttpRelayClient::new(transport, config, signing_key);
        if let Some(pin) = pin {
            client = client.with_control_pin(pin);
        }
        client
    }

    /// **受限能力内部**的 unpinned 客户端：只在 FreshRecovery 的首次控制链消费处使用。
    ///
    /// 仍然强制管理公钥信任锚与逐条管理签名；`with_unpinned_bootstrap_control` 是
    /// `pub(crate)`，全 crate 唯一调用点就是这里。
    fn unpinned_client(
        &self,
        device_id: DeviceId,
        signing_key: DeviceSigningKey,
    ) -> Result<HttpRelayClient<StdTcpTransport>, SyncError> {
        let config = HttpRelayConfig {
            base_url: self.context.origin_str(),
            api_version: self.context.api_version(),
            vault_id: self.vault_id,
            device_id,
            manager_pub: Some(self.manager_anchor),
        };
        Ok(
            HttpRelayClient::new(self.context.transport()?, config, signing_key)
                .with_unpinned_bootstrap_control(),
        )
    }

    /// 用**已有身份**的 `(instance, rev)` 下界 + bootstrap 锚验证控制链并提交 pin。
    fn verify_and_commit_pin(
        &self,
        vault: &Vault,
        identity: &LocalIdentityV1,
    ) -> Result<PinV1, SyncError> {
        let anchor = BootstrapPins::from_vault(vault)?.control_anchor()?;
        let mut pin = ControlPin::from_identity(identity);
        if let Some(anchor) = anchor {
            pin = pin.with_bootstrap_anchor(anchor);
        }
        let signing_key = DeviceSigningKey::from_seed(identity.ed25519_seed.clone());
        let transport = self.context.transport()?;
        let mut client = self.client(transport, identity.device_id, signing_key, Some(pin));
        let head = client.verified_head_with_anchor(anchor)?;
        let expected = load_control_pin(vault, self.vault_id)?;
        match expected.as_ref() {
            // 旧实例：只有 `(instance, rev)` 下界。同 rev + 同视图锚是唯一允许补齐
            // `record.hash` 的路径（ADR 0013 §3 VerifiedMigration）。
            Some(pin) if pin.record.hash.is_none() => {
                crate::control_pin::upgrade_lower_bound_pin(vault, pin, head)?
            }
            _ => commit_verified_head(vault, expected.as_ref(), head)?,
        }
        self.load_pin(vault)
    }

    /// 检查 durable pin 是否存在（诊断/测试用）。
    pub fn current_pin(&self, vault: &Vault) -> Result<Option<PinV1>, SyncError> {
        load_control_pin(vault, self.vault_id)
    }
}

/// 初始化 pending 的对账结论（`reconcile` 返回 `None` 表示没有 pending）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitReconcileOutcome {
    /// `Prepared` 阶段（请求从未发出）：只清 pending，可安全重新开始。
    AbortedBeforeRequest,
    /// pending 已是 `Done`：清理即可。
    ClearedDone,
    /// 用已有材料补齐 finalize（未重复 genesis/enroll）后清 pending。
    Completed,
}

/// [`reconcile`] 的报告（只在实际存在 pending 时产生）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitReconcileReport {
    /// 未完成的初始化种类。
    pub kind: InitKind,
    /// 对账前的阶段。
    pub phase_before: InitPhase,
    /// 对账结论。
    pub outcome: InitReconcileOutcome,
    /// 对账后库里的 canonical pin（没有则为 `None`）。
    pub pin: Option<PinV1>,
}

/// 启动对账：发现 pending 时决定"清理 / 用已有材料完成 finalize / 停止"（ADR 0013 §3）。
///
/// 返回 `Ok(None)` = 没有 pending（普通启动，不联网、不写库）。
///
/// 规则（**不自动重建、不删用户数据**）：
///
/// | 阶段 | 有身份材料 | 动作 |
/// | --- | --- | --- |
/// | `Done` | 无所谓 | 清理 pending → [`InitReconcileOutcome::ClearedDone`] |
/// | `Prepared` | 无所谓 | 请求从未发出（写前记录保证）→ 清 pending → `AbortedBeforeRequest` |
/// | `GenesisRequested` / `IdentityWritten` / `PinCommitted` | 有 | 用身份 + bootstrap 锚重新验证控制链，提交 pin（`FreshRecovery` 若 bootstrap 未完成则先用身份材料续跑 bootstrap）→ `Done` + 清理 → `Completed` |
/// | 同上 | 没有 | [`SyncError::ConfigInvalid`]（远端可能已创建但本地材料丢失：停住，人工处理） |
///
/// 身份与 pending 的 vault/预期设备绑定不符、pin 损坏、控制链视图不符一律**停止**
/// （`ConfigInvalid`/`NeedsTrust`/`Equivocation`），绝不"清掉重来"。
pub fn reconcile(
    vault: &Vault,
    identity: Option<&LocalIdentityV1>,
) -> Result<Option<InitReconcileReport>, SyncError> {
    reconcile_inner(vault, identity, &[])
}

/// **测试/私有 CA 专用**：与 [`reconcile`] 相同，但给重建的 context 追加显式信任根
/// （DER 编码 CA）。只用于本地 TLS harness（`menghunote-dev-relay` 的 `start_tls`）：
/// 生产路径用系统信任根，不经过这里（ADR 0013 §5：不提供"一键信任"）。
#[doc(hidden)]
pub fn reconcile_with_extra_trust_roots(
    vault: &Vault,
    identity: Option<&LocalIdentityV1>,
    extra_trust_roots: &[Vec<u8>],
) -> Result<Option<InitReconcileReport>, SyncError> {
    reconcile_inner(vault, identity, extra_trust_roots)
}

fn reconcile_inner(
    vault: &Vault,
    identity: Option<&LocalIdentityV1>,
    extra_trust_roots: &[Vec<u8>],
) -> Result<Option<InitReconcileReport>, SyncError> {
    let Some(pending) = init_journal::load_pending(vault, None)? else {
        return Ok(None);
    };
    if pending.phase == InitPhase::Done {
        init_journal::clear_pending(vault)?;
        return Ok(Some(InitReconcileReport {
            kind: pending.kind,
            phase_before: pending.phase,
            outcome: InitReconcileOutcome::ClearedDone,
            pin: load_control_pin(vault, pending.vault_id)?,
        }));
    }
    if pending.phase == InitPhase::Prepared {
        // 写前记录保证：进入 GenesisRequested 之前不会发任何初始化请求。
        init_journal::abort_before_request(vault)?;
        return Ok(Some(InitReconcileReport {
            kind: pending.kind,
            phase_before: pending.phase,
            outcome: InitReconcileOutcome::AbortedBeforeRequest,
            pin: None,
        }));
    }
    let identity = identity.ok_or(SyncError::ConfigInvalid(format!(
        "初始化未完成（{} / {}）且没有本地身份材料：远端可能已创建，拒绝自动重建；请人工确认",
        pending.kind.as_str(),
        pending.phase.as_str()
    )))?;
    if identity.vault_id != pending.vault_id {
        return Err(SyncError::ConfigInvalid(
            "身份与 sync.init_pending 的 vault 绑定不一致".to_owned(),
        ));
    }
    if pending.has_expected_device() && identity.device_id != pending.expected_device {
        return Err(SyncError::ConfigInvalid(
            "身份与 sync.init_pending 的预期设备不一致".to_owned(),
        ));
    }
    if identity.pinned_manager == [0u8; 32]
        || identity.pinned_instance == [0u8; 16]
        || identity.control_rev == 0
    {
        return Err(SyncError::NeedsTrust(
            "身份缺管理锚/实例下界：无法完成初始化 finalize（拒绝以 unpinned 方式继续）",
        ));
    }
    let context = context_of(&pending)?;
    let context = if extra_trust_roots.is_empty() {
        context
    } else {
        context.with_test_extra_roots(extra_trust_roots.iter())?
    };
    let session = InitSession {
        kind: pending.kind,
        context,
        vault_id: pending.vault_id,
        manager_anchor: identity.pinned_manager,
        expected_device: identity.device_id,
        seed: None,
        rev1_override: None,
    };
    // FreshRecovery：bootstrap 没跑完时用身份里的 root 续跑（**不重复 enroll**）。
    if pending.kind == InitKind::FreshRecovery && bootstrap_incomplete(vault)? {
        session.resume_bootstrap(vault, identity)?;
    }
    let pin = session.verify_and_commit_pin(vault, identity)?;
    if pending.phase < InitPhase::PinCommitted {
        init_journal::advance_phase(vault, pending.phase, InitPhase::PinCommitted)?;
    }
    init_journal::advance_phase(vault, InitPhase::PinCommitted, InitPhase::Done)?;
    init_journal::clear_pending(vault)?;
    Ok(Some(InitReconcileReport {
        kind: pending.kind,
        phase_before: pending.phase,
        outcome: InitReconcileOutcome::Completed,
        pin: Some(pin),
    }))
}

/// app 的**身份已原子落盘**之后调用：在规范顺序里把 pending 从 `GenesisRequested`
/// 推进到 `IdentityWritten`。
///
/// 为什么需要两步（`mark` + [`finish_identity_written`]）而不是一个函数：ADR 0013 §2
/// 要求"SQLite 与身份文件不能跨文件原子，增加 journal 阶段 + 启动对账"。身份文件由
/// app 用"临时文件 + 原子 rename"写（`DevIdentityStore::persist_protected`），本 crate
/// 看不到那个提交点，只能由调用方在 rename 返回后推进；再调用
/// [`finish_identity_written`] 才允许进入 `Done` 并清理。
///
/// 语义（阶段序 `Prepared < GenesisRequested < IdentityWritten < PinCommitted < Done`）：
/// - `GenesisRequested` → `IdentityWritten`（远端 S3b 的规范顺序：先落盘身份、后提交 pin）；
/// - 已经是 `IdentityWritten` → `Ok`（幂等，重试安全）；
/// - `PinCommitted` → `Ok`（**不回退**）：`local_dev` 的 Create/Join 先提交 pin 再落盘身份，
///   `PinCommitted` 在阶段序上更晚、语义上蕴含"身份已写"；
/// - `Prepared`/`Done` → [`SyncError::ConfigInvalid`]：请求未发就声称身份已落盘，
///   或初始化已经结束，都是绑定矛盾。
///
/// **不联网、不重建、不删数据**：只推进 durable 阶段。
pub fn mark_identity_written(vault: &Vault) -> Result<(), SyncError> {
    let pending = init_journal::load_pending(vault, None)?.ok_or(SyncError::ConfigInvalid(
        "缺少 sync.init_pending：不能推进 identity_written（初始化状态不完整）".to_owned(),
    ))?;
    match pending.phase {
        // 已经到 identity_written（或按阶段序更晚的 pin_committed）：幂等。
        //
        // `local_dev` 的 Create/Join 在**提交 pin 之后**才把身份交给 app 落盘，此时
        // pending 已在 `PinCommitted`（阶段序上晚于 `IdentityWritten`，语义上蕴含
        // "身份已写"），不能为了这次推进而回退阶段；`finish_identity_written` 同样
        // 接受这两个阶段。真正的 `GenesisRequested → IdentityWritten` 推进留给远端
        // S3b 的流程（先落盘身份、再提交 pin）。
        InitPhase::IdentityWritten | InitPhase::PinCommitted => Ok(()),
        InitPhase::GenesisRequested => {
            init_journal::advance_phase(
                vault,
                InitPhase::GenesisRequested,
                InitPhase::IdentityWritten,
            )?;
            Ok(())
        }
        other => Err(SyncError::ConfigInvalid(format!(
            "初始化阶段 {} 不能推进到 identity_written（必须先发出初始化请求）",
            other.as_str()
        ))),
    }
}

/// 身份落盘的 durable 事实之后：`IdentityWritten` → `Done` → 清理 pending。
///
/// 只有 [`mark_identity_written`] 成功（或启动对账已经推进到 `IdentityWritten`）之后才
/// 应调用；`Done` 阶段同样接受（幂等重试）；没有 pending 视为已完成（`Ok`，不重建任何
/// 状态）。其它阶段明确 [`SyncError::ConfigInvalid`]：**清理只在 Done**。
pub fn finish_identity_written(vault: &Vault) -> Result<(), SyncError> {
    let Some(pending) = init_journal::load_pending(vault, None)? else {
        return Ok(());
    };
    match pending.phase {
        InitPhase::Done => {}
        // `IdentityWritten` 是正常路径；`PinCommitted`（local_dev 先提交 pin 再落盘身份）
        // 在阶段序上更晚，同样允许收尾。其它阶段（请求未发/未提交 pin）一律拒绝。
        InitPhase::IdentityWritten | InitPhase::PinCommitted => {
            init_journal::advance_phase(vault, pending.phase, InitPhase::Done)?;
        }
        other => {
            return Err(SyncError::ConfigInvalid(format!(
                "初始化尚未到身份落盘/pin 提交（当前 {}）：不得清理 pending",
                other.as_str()
            )))
        }
    }
    init_journal::clear_pending(vault)?;
    Ok(())
}

/// 从 pending 复原 context（模式 + canonical origin；预算用默认值）。
fn context_of(pending: &InitPendingV1) -> Result<RelayContext, SyncError> {
    use crate::http_transport::DEFAULT_TRANSPORT_TIMEOUTS;
    let context = match pending.mode {
        crate::relay_policy::RelayMode::LocalDev => {
            RelayContext::local_dev(&pending.origin, DEFAULT_TRANSPORT_TIMEOUTS)
        }
        crate::relay_policy::RelayMode::RemoteTls => {
            RelayContext::remote_tls(&pending.origin, DEFAULT_TRANSPORT_TIMEOUTS)
        }
    }
    .map_err(|error| {
        SyncError::ConfigInvalid(format!("sync.init_pending 的 origin 非法：{error}"))
    })?;
    // 面是 pending 的持久事实：v2 创建未完成时对账必须继续走 v2（不能静默回 v1）。
    Ok(context.with_api_version(pending.api_version))
}

/// bootstrap 是否尚未完成（`sync.pinned_instance` 与 relay 游标都没有 = finalize 没跑过）。
fn bootstrap_incomplete(vault: &Vault) -> Result<bool, SyncError> {
    let pins = BootstrapPins::from_vault(vault)?;
    let cursor = vault.get_meta(RELAY_CURSOR_META)?;
    Ok(pins.instance_id.is_none() || cursor.is_none())
}

impl InitSession {
    /// FreshRecovery 的崩溃续跑：用身份里的 root 重新做认证 bootstrap（不重复 enroll）。
    fn resume_bootstrap(&self, vault: &Vault, identity: &LocalIdentityV1) -> Result<(), SyncError> {
        let root = root_of(identity);
        let signing_key = DeviceSigningKey::from_seed(identity.ed25519_seed.clone());
        let mut client = self
            .unpinned_client(identity.device_id, signing_key)?
            .with_root(root.clone());
        let pins = BootstrapPins::from_vault(vault)?;
        bootstrap::run_with_pins(
            vault,
            &mut client,
            self.vault_id,
            identity.loro_peer_id(),
            &root,
            &pins,
        )?;
        Ok(())
    }
}

/// 创建授权方式（v1 setup secret / v2 一次性票据；由 context 的面唯一决定）。
enum GenesisCreateAuth<'a> {
    /// v1：`POST /v1/vaults` 键 5。
    SetupSecret(&'a str),
    /// v2：`POST /v2/vaults` 键 0。
    Ticket(&'a crate::create_link::CreateTicket),
}

/// context 若显式给了管理锚，必须与本地材料推导出的锚一致（fail-closed）。
fn check_context_manager(context: &RelayContext, anchor: [u8; 32]) -> Result<(), SyncError> {
    if let Some(explicit) = context.manager_anchor {
        if explicit != anchor {
            return Err(SyncError::NeedsTrust(
                "context 的管理锚与初始化材料推导出的管理锚不一致",
            ));
        }
    }
    Ok(())
}

/// 让 `InitSession` 在日志里不泄露任何秘密材料（恢复种子/私钥都不打印）。
impl std::fmt::Debug for InitSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InitSession")
            .field("kind", &self.kind)
            .field("mode", &self.context.mode)
            .field("origin", &self.context.origin_str())
            .field("vault_id", &crate::ids::hex_of(&self.vault_id))
            .field(
                "expected_device",
                &crate::ids::hex_of(&self.expected_device),
            )
            .field("manager_anchor", &"<redacted>")
            .field("has_recovery_seed", &self.seed.is_some())
            .finish_non_exhaustive()
    }
}
