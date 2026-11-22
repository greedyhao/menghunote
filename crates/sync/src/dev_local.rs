//! ADR 0009 切片 4b：**GPUI-free** 的 dev-local 同步编排（Create / 发布初始基线 / Join / SyncOnce）。
//!
//! 本模块只做"把 `crates/sync` 已有的真实步骤按 ADR 0009 的顺序串起来"，不新增协议、
//! 不改 wire、不碰 UI/worker（切片 4c 才接 `SyncService`）。它存在的理由是：同样的
//! 编排写在 `examples/e2e_common.rs` 里只能给 example 用，而 app 的 dev-sync worker
//! 需要一个**可测、无 GPUI**的库入口。
//!
//! ## 编排
//!
//! 1. [`DevLocal::create`]：`POST /v1/vaults`（genesis）→ 新建本地 SQLite vault →
//!    落 `sync.relay_cursor`（relay 提供时）/ `sync.log_id` / 展示标签 /
//!    **初始 recovery 槽材料** → **上传初始 recovery 槽**（KEY_SLOT staging）并落
//!    「已上传」标记。槽必须在恢复码还在内存里的这一刻上传：KEY_SLOT 对象的 MNE1
//!    密钥 `K_slot_mne = HKDF(恢复码种子)` 不可逆，恢复码又不落盘，事后无法补传；
//! 2. 发布初始基线有两条入口，共用同一条核心路径（sync + 两份 checkpoint）：
//!    - [`DevLocal::publish_initial_pair`]（CLI）：校验恢复码与本地管理身份一致 →
//!      上传/重传初始 recovery 槽（幂等）→ 核心；
//!    - [`DevLocal::publish_initial_pair_with_identity`]（面板 worker）：**不接收恢复码**，
//!      信任锚是受保护身份里的 `manager_seed`（Join 设备没有 → 明确拒绝），初始槽以
//!      Create 落下的「已上传」标记为前提（旧实例明确失败并指向 CLI 路径）；
//!
//!    两条入口随后都跑一轮完整 `SyncOnce`（让覆盖证明只认 durable 事实）→ 用
//!    [`CheckpointWriter`] 发布**两份**真实同 epoch checkpoint（manifest/snapshot/
//!    asset/refs），并把最新一份落成 durable 事实；
//! 3. [`DevLocal::join`]：只接收 `vault_id + 恢复码 + 新路径`。`RecoverySeed →
//!    ManagementKey::from_recovery` → `recovery_enroll::self_enroll` 建管理信任锚并
//!    自批新设备 → `bootstrap::run_with_pins` 认证读取并导入基线 → pins/cursor 由
//!    bootstrap 落库，identity 只返回给调用方（**不读 A 的目录/identity/root**，
//!    也不在 vault 里写任何私钥或恢复码）；
//! 4. [`DevLocal::sync_once`]：构造借用型 [`SyncEngine`]，按 `encrypt_pending → push →
//!    pull → integrate → ack` 跑一轮，返回 [`SyncReport`]（游标由 engine 自己推进）；
//! 5. ADR 0010 切片 2a-2 的两个**只读/管理**入口，给 dev-only 设备列表 UI 用：
//!    [`DevLocal::members`]（已验证控制链的只读成员视图）与 [`DevLocal::revoke_device`]
//!    （管理密钥 + 管理端 helper 移除一台设备的访问，**不轮换密钥**、不删本地身份）。
//!
//! ## 凭据模型（dev-only）
//!
//! dev-local 只有**恢复码**一种凭据：`create` 不接收口令，因此 password 槽材料只存在于
//! 进程内、不上传、不落盘（口令槽 onboarding 属于切片 4c 的正式流程）。所以
//! `create` / [`DevLocal::publish_initial_pair`] 上传的是 **genesis 生成的那一个 recovery
//! 槽**（staging），Join 全程只靠恢复码解出根。
//!
//! 面板的「发布基线」走 [`DevLocal::publish_initial_pair_with_identity`]：它**不需要恢复码**
//! （恢复码在 Create 之后就不在进程里了），只要求身份里有管理私钥，且初始槽已由 Create
//! 上传过（[`INITIAL_SLOT_UPLOADED_META`]）。这是 ADR 0009 切片 4c-4 的取舍：为了让面板
//! 能一键发布，把"只有恢复码能做的事"（上传初始槽）前移到 Create，发布本身只做
//! "同步 + 两份 checkpoint"。
//!
//! ## 安全与非目标
//!
//! - relay 接入统一走 [`crate::relay_context::RelayContext`]：本模块的每个操作都从
//!   **同一个** context 取客户端/传输（不再各自拼 `StdTcpTransport`）；URL 兼容入口
//!   只是先构造一个 `local_dev` context。`remote_tls` 的 Create/Join 初始化在
//!   [`crate::remote_init`]（S3b），本模块保持 local_dev 语义。普通同步的
//!   生产 TLS 由 app 按公开配置的模式用 `remote_tls` context 打开；本模块不实现验签——测试用
//!   `menghunote-dev-relay`，它是**放行验签**的测试脚手架，**非生产**；正式鉴权负例
//!   必须走 relay 生产 hook；
//! - 不在这里写 app 的受保护身份文件（`DevIdentityStore` 负责 DPAPI/Linux 主钥封装）：
//!   identity 只经返回值交给调用方，绝不进 vault meta/明文配置/日志；
//! - 恢复码只以 `String` 形式交给调用方（ADR 0009：仅临时显示，提交后清除），本模块
//!   自身不打印、不落盘；`setup_secret` 只回显、不持久化；
//! - 初始 recovery 槽材料（描述符 + 包裹根密文）会写入 vault meta
//!   [`INITIAL_RECOVERY_SLOT_META`]：那是**已经要发到 relay 的密文**，不含根/恢复码明文；
//!   它让"发布基线"可重复且幂等（同一 slot_id/描述符，relay 只登记一次）。
//!
//! ## 失败可恢复性
//!
//! - `create` 是新建库：路径已存在直接拒绝；任何一步失败都**不留下**可被误用的本地
//!   vault（已创建的文件会被清掉）。relay 侧可能留下一个"没有槽、没有 checkpoint"
//!   的空 vault，它无法被任何设备使用，重试 `create` 会建新的；
//! - `publish_initial_pair` 先做恢复码一致性校验（错了在**任何上传之前**失败），再做
//!   槽上传/同步/两份 checkpoint；它是幂等的，失败后重跑会补齐（relay 只把最新的两份
//!   published 作为 `/bootstrap` 的候选）；
//! - `join` 失败不写 cursor/identity：bootstrap 只在全部导入通过后一个事务推进水位；
//!   失败时本模块清掉新建的本地文件（不在新目录留半初始化库），调用方重新 Join 即可。

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::checkpoint::{CheckpointReport, CheckpointWriter, StorageAssets};
use crate::device::{DeviceSigningKey, ManagementKey};
use crate::device_management::{RevokeContext, RevokeOutcome};
use crate::engine::{
    AckReport, AssetSyncReport, EngineConfig, EngineIdentity, IntegrateConfig, IntegrateReport,
    PullReport, PushReport, SyncEngine, SyncReport, RELAY_CURSOR_META,
};
use crate::error::SyncError;
use crate::genesis;
use crate::http_relay::{Clock, HttpRelayClient, MemberView, SystemClock};
use crate::http_transport::{HttpTransport, TransportTimeouts, DEFAULT_TRANSPORT_TIMEOUTS};
use crate::identity::LocalIdentityV1;
use crate::ids;
use crate::init_session::InitSession;
use crate::keys::RootSecretV1;
use crate::random::{array, OsRandom};
use crate::recovery::{management_seed, RecoverySeed};
use crate::relay::RelayClient as _;
use crate::relay_context::{RelayContext, RelayTarget};
use crate::slot::{KeySlotV1, WrappedRoot};
use crate::{DeviceId, VaultId};
use menghunote_storage::{
    ensure_structure_row, load_head, materialize_body, pending_outbox, set_sync_log_id, Vault,
    STRUCTURE_DOC_ID,
};
use menghunote_sync_proto::{SlotDescriptorV1, SlotKind};

// ADR 0009 Phase 2（切片 5）：轮换的库级入口与结果类型，与 Create/Join/SyncOnce 同层导出。
pub use crate::dev_local_rotation::{ApplyOutcome, RotationOutcome};

/// `meta` 键：dev-local 实例的展示标签（仅本机，非 wire；Join 不导入）。
pub const VAULT_LABEL_META: &str = "dev.local.vault_label";
/// `meta` 键：genesis 生成的初始 recovery 槽材料（描述符 CDE + `nonce||ciphertext`）。
///
/// **只含密文**：包裹根用 `HKDF(恢复码)` 派生 KEK 加 AEAD，槽体与上传到 relay 的
/// KEY_SLOT 明文逐字节相同；恢复码本身不落盘。写入后只由
/// [`DevLocal::upload_initial_slot`] 读取（复用同一 slot_id，relay 幂等）。
pub const INITIAL_RECOVERY_SLOT_META: &str = "dev.local.initial_recovery_slot.v1";
/// `meta` 键：初始 recovery 槽**已上传到 relay** 的 durable 事实（ADR 0009 切片 4c-4）。
///
/// 记录 `slot_id`（32B）+ 确定性 `message_id`（16B），两者都是公开事实，不含恢复码/根。
/// 它是"只凭身份发布基线"（[`DevLocal::publish_initial_pair_with_identity`]）的前置条件：
/// 上传 KEY_SLOT 需要的 `K_slot_mne` 只能由恢复码种子派生，恢复码不落盘 → 事后无法补传，
/// 所以缺这条事实的旧实例必须明确失败并指向 CLI 的恢复码路径（不静默发布一个 B 无法
/// Join 的假基线）。
pub const INITIAL_SLOT_UPLOADED_META: &str = "dev.local.initial_slot_uploaded.v1";
/// `meta` 键：最近一次发布基线的**较新**那份 checkpoint。
pub const PUBLISHED_BASELINE_META: &str = "dev.local.published_baseline.v1";

/// 初始 recovery 槽记录 magic（"MenghuNote Dev Local Slot v1"）。
const INITIAL_SLOT_MAGIC: &[u8; 8] = b"MN/DLS1\0";
/// 初始 recovery 槽「已上传」记录 magic。
const INITIAL_SLOT_UPLOADED_MAGIC: &[u8; 8] = b"MN/DLS2\0";
/// 已发布基线记录 magic。
const PUBLISHED_BASELINE_MAGIC: &[u8; 8] = b"MN/DLB1\0";
/// 槽描述符 CDE 的合理上限（`SlotDescriptorV1` 现在是 ~70B，留余量防止坏长度读爆内存）。
const MAX_SLOT_DESCRIPTOR_LEN: usize = 4096;
/// KEY_SLOT `message_id` 派生的域分隔（非秘密；换它等于换对象字节，会导致 409）。
const KEY_SLOT_MESSAGE_DOMAIN: &[u8] = b"MN/v1/dev-local/key-slot-message\0";

/// 一轮同步的可选注入项（ADR 0012 S1）：**传输预算** + **集成前守卫**。
///
/// 为什么需要它：`DevLocal` 过去自己构造 transport（超时预算接不进来），而且
/// `integrate` 一旦开始就没有"取消/授权复核"的边界。两者都必须由调用方（app 的
/// worker）决定，所以收进一个显式结构，而不是在 `DevLocal` 内部硬编。
///
/// - `transport = None`：保持历史默认（连接/读写 30s、无整次请求预算）；
/// - `before_integrate = None`：不做额外复核（CLI/测试路径）。
#[derive(Default)]
pub struct SyncOnceOptions<'a> {
    /// 传输超时预算；`None` = 历史默认（向后兼容）。
    pub transport: Option<TransportTimeouts>,
    /// `integrate` 之前最后一道门：返回 `Err` 即**不 append**（取消/授权已失效）。
    pub before_integrate: Option<&'a mut (dyn FnMut() -> Result<(), SyncError> + 'a)>,
}

/// dev-local 编排入口（纯函数集合，无状态）。
pub struct DevLocal;

/// `create` 的结果：本地 vault + 正式身份 + 恢复码。
///
/// **不实现 `Debug`/`Display`**：`identity` 与 `recovery_code` 都是秘密材料
/// （`LocalIdentityV1` 本身也不实现，避免误打印）。
pub struct CreatedVault {
    /// 新开的本地库（已建 structure 行、游标、log_id 与标签）。
    pub vault: Vault,
    /// genesis 身份（含管理私钥；app 由 `DevIdentityStore` 受保护落盘）。
    pub identity: LocalIdentityV1,
    /// 新 vault 的 id（Join 时展示/输入的公开标识）。
    pub vault_id: VaultId,
    /// 一次性展示给用户的恢复码（`mnrec1...`，**不得**写日志/配置）。
    pub recovery_code: String,
    /// 回显调用方传入的 setup secret（dev-only；**不得**落盘/写日志）。
    pub setup_secret: String,
}

/// `join` 的结果：新设备自己的 vault + 设备身份。
pub struct JoinedVault {
    /// 新开的本地库（已由 bootstrap 导入基线并落 pins/cursor）。
    pub vault: Vault,
    /// 新设备身份（`manager_seed = None`：普通设备没有管理私钥）。
    pub identity: LocalIdentityV1,
    /// 加入的 vault id。
    pub vault_id: VaultId,
}

/// [`DevLocal::published_baseline`] 读出的 durable 事实（本地 meta，非 wire）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublishedBaseline {
    /// 较新那份 checkpoint 的 id。
    pub checkpoint_id: [u8; 16],
    /// 该 checkpoint 的 epoch。
    pub epoch: u64,
    /// 该 checkpoint 固定的 S。
    pub covered_seq: u64,
    /// relay 记录的发布时间（Unix 秒）。
    pub published_at: u64,
}

/// [`DevLocal::revoke_device`] 的结果：relay 事实 + **刷新后**的成员视图（ADR 0010 切片 2a-2）。
///
/// 两个字段都不含秘密：`outcome` 只有目标设备/`changed`/`auth_rev`/`control_rev`；
/// `members` 是只读投影（`MemberView` 的 `Debug` 只打印 id/status/fingerprint）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevokedDevice {
    /// 管理端请求返回的事实（`control_rev` 不变、`auth_rev` 可能已前进）。
    pub outcome: RevokeOutcome,
    /// 吊销**之后**重新拉取的成员视图（已按 `device_id` 稳定升序）。
    pub members: Vec<MemberView>,
}

/// 两阶段同步 **Prepare** 阶段的报告（encrypt/push/pull；不含 integrate/ack）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PrepareReport {
    /// 本轮新加密的本地意图数。
    pub encrypted: usize,
    /// 推送结果。
    pub push: PushReport,
    /// 增量资产上传结果（正文 push 之后、pull 之前）。
    pub assets: AssetSyncReport,
    /// 拉取结果（游标已 durable 推进）。
    pub pull: PullReport,
}

/// 两阶段同步 **Integrate** 阶段的报告。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IntegratePhaseReport {
    /// 集成结果（integrated/deferred/批次/是否投影）。
    pub integrate: IntegrateReport,
    /// 本阶段 head **真的前进**的文档（结构 + 正文；UI 据此判断交付/刷新）。
    pub changed_heads: Vec<String>,
    /// `watch_doc` 是否真的前进（= 需要交付）。
    pub watch_doc_changed: bool,
    /// 本阶段按 durable outbox 触发物化的正文篇数（changed heads → 物化/索引更新）。
    pub materialized: usize,
    /// 物化失败篇数（失败只记日志，不影响已 durable 的 CRDT 集成结论）。
    pub materialize_failed: usize,
}

/// 两阶段同步 **Finish** 阶段的报告（push tail + ack）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FinishReport {
    /// 上报的 received/integrated 游标。
    pub ack: AckReport,
    /// tail 阶段新加密的本地意图数。
    pub encrypted: usize,
    /// tail 推送结果。
    pub push: PushReport,
}

impl DevLocal {
    /// Create：对真实 relay 走 genesis，并新建（**必须是新路径**）本地 vault。
    ///
    /// 初始化走 ADR 0013 S2b 的受限能力 [`InitSession::new_genesis`]：
    ///
    /// 1. 先在本地生成 vault/device **材料**（不联网），再开新库并写 durable pending
    ///    （`sync.init_pending`）——顺序是"先记录意图、再联网"；
    /// 2. `POST /v1/vaults`（写前先推进 `GenesisRequested`）；成功后**不重复**创建；
    /// 3. 用本地提交证书复算 rev1 `record_hash`，验证 relay 当前链/前缀，再 durable
    ///    提交强 pin（`sync.control_pin`，rev1 record 锚）；
    /// 4. 写本地事实（structure/label/cursor/log_id/初始槽材料）→ 上传初始 recovery 槽
    ///    → pending 进入 `Done` 并清理（app 走 [`Self::create_deferring_identity`]：
    ///    停在 `PinCommitted`，身份 rename 成功后再推进 `IdentityWritten → Done`）。
    ///
    /// 失败语义沿用 ADR 0009（app 验收依赖它）：**任何返回错误都清掉本次新建的库文件**
    /// （relay 侧可能留下一个没有槽/没有 checkpoint 的空 vault，重试会建新的）；
    /// pending 的用途是**崩溃**（进程消失、清理代码没跑）之后的对账，见
    /// [`crate::init_session::reconcile`]。
    ///
    /// 口令：dev-local 不接收口令，genesis 的 password 槽材料用一次随机口令生成后**丢弃**
    /// （不上传、不落盘），因此该 vault 只能靠恢复码加入——这是 ADR 0009 Phase 1 的
    /// dev-only 取舍，口令 onboarding 在切片 4c。
    pub fn create(
        relay_url: &str,
        setup_secret: &str,
        vault_path: &Path,
        vault_label: &str,
    ) -> Result<CreatedVault, SyncError> {
        Self::create_inner(relay_url, setup_secret, vault_path, vault_label, true)
    }

    /// **app 专用**：与 [`Self::create`] 完全相同，但 pending 停在 `PinCommitted`。
    ///
    /// 调用方必须在身份**原子落盘**（`DevIdentityStore::persist_protected` 的临时文件 +
    /// rename）之后调用 [`crate::init_session::mark_identity_written`] 与
    /// `finish_identity_written`。这样"relay 已建 vault、本地身份还没落盘"的崩溃窗口
    /// 有 durable pending 可对账（ADR 0013 §2/S3），而不是靠重跑 genesis。
    ///
    /// 失败语义与 [`Self::create`] 一致：返回错误时清掉本次新建的本地文件。
    pub fn create_deferring_identity(
        relay_url: &str,
        setup_secret: &str,
        vault_path: &Path,
        vault_label: &str,
    ) -> Result<CreatedVault, SyncError> {
        Self::create_inner(relay_url, setup_secret, vault_path, vault_label, false)
    }

    fn create_inner(
        relay_url: &str,
        setup_secret: &str,
        vault_path: &Path,
        vault_label: &str,
        finish_identity: bool,
    ) -> Result<CreatedVault, SyncError> {
        if vault_path.as_os_str().is_empty() {
            return Err(SyncError::LocalCorrupt("Create 需要明确的 vault 路径"));
        }
        if vault_path.exists() {
            return Err(SyncError::LocalCorrupt(
                "Create 只接受尚不存在的 vault 路径（已有库请按其身份使用）",
            ));
        }

        // 0. 按唯一权威策略构造 context（local_dev 只接受 http + loopback 白名单）。
        let context = RelayContext::local_dev(relay_url, DEFAULT_TRANSPORT_TIMEOUTS)?;

        // 1. 本地材料：vault_id/device_id 必须**先进 pending**，之后才能发请求。
        let vault_id: VaultId = array(&mut OsRandom)?;
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
        let context = session.context().clone();

        // 2. 新库（pending 需要 SQLite；genesis 失败时会连同 pending 一起清掉）。
        let vault = match Vault::open(vault_path) {
            Ok(vault) => vault,
            Err(error) => {
                remove_vault_files(vault_path);
                return Err(SyncError::Storage(error));
            }
        };
        let created = (|| -> Result<(LocalIdentityV1, String), SyncError> {
            session.prepare_pending(&vault, SystemClock.now_secs())?;
            let receipt = session.request_genesis(&vault, &materials, setup_secret)?;
            let report = session.finalize_genesis(&vault, &materials, &receipt)?;

            ensure_structure_row(&vault)?;
            vault.set_meta(VAULT_LABEL_META, vault_label.as_bytes())?;
            // 初始游标：genesis 响应键 5（旧 relay 可能没有 —— 那就留空，publish 时用
            // `GET /state` 键 14 兜底，且绝不把"缺失"当成 0 号游标）。
            if let Some(cursor) = &receipt.initial_cursor {
                vault.set_meta(RELAY_CURSOR_META, cursor.as_bytes())?;
            }
            // log_id 来自 genesis 响应：先钉住 relay 日志代，pull 再复核。
            set_sync_log_id(&vault, &receipt.log_id)?;
            // 初始 recovery 槽材料（密文）留给发布/Join 复用（同一 slot_id 幂等）。
            let slot_record = encode_initial_slot(&materials.recovery_slot)?;
            vault.set_meta(INITIAL_RECOVERY_SLOT_META, &slot_record)?;

            // 初始 recovery 槽必须**现在**上传（ADR 0009 切片 4c-4）：KEY_SLOT 对象的 MNE1
            // 密钥 `K_slot_mne = HKDF(恢复码种子)` 不可逆，而恢复码只在这一刻还在内存里、不落盘。
            // 这一步是初始化内部操作（`open_for_init` 跳过 pending 守卫）。
            Self::upload_initial_slot_during_init(
                &context,
                &vault,
                &report.identity,
                &materials.recovery_seed,
            )?;

            // 本地初始化完成：pending → Done → 清理（清理只在 Done）。
            // app 的延迟路径停在 PinCommitted，等身份 rename 成功后由调用方推进。
            if finish_identity {
                session.finish(&vault)?;
            }
            Ok((report.identity, materials.recovery_code.as_str().to_owned()))
        })();
        match created {
            Ok((identity, recovery_code)) => Ok(CreatedVault {
                vault,
                vault_id: identity.vault_id,
                recovery_code,
                identity,
                setup_secret: setup_secret.to_owned(),
            }),
            Err(error) => {
                drop(vault);
                remove_vault_files(vault_path);
                Err(error)
            }
        }
    }

    /// CLI 路径的发布初始基线：**用恢复码**确认管理身份，然后与身份路径共用同一条核心。
    ///
    /// 步骤（顺序不可交换）：
    /// 1. 恢复码必须与本地管理身份一致（`HKDF(R) == identity.manager_seed`）——错了在
    ///    任何上传之前失败，避免用一个错码把 KEY_SLOT 密文写坏；
    /// 2. 上传 genesis 的 recovery 槽（staging；`PUT key-slots` 对同 slot_id 同描述符幂等；
    ///    新实例在 Create 时已上传过一次，这里只是补齐旧实例或灾后重传）并落 durable 标记；
    /// 3. 核心：一轮完整 `SyncOnce`（覆盖证明只认 durable 事实）+ 连续两份同 epoch
    ///    checkpoint（见 [`Self::publish_baseline_core_with`]）。
    ///
    /// 这条入口保留给 CLI（`--dev-sync-publish`）、自动化与恢复场景；面板按钮走
    /// [`Self::publish_initial_pair_with_identity`]（不需要恢复码）。
    pub fn publish_initial_pair<T: RelayTarget + ?Sized>(
        target: &T,
        vault: &Vault,
        identity: &LocalIdentityV1,
        recovery_code: &str,
    ) -> Result<CheckpointReport, SyncError> {
        let seed = RecoverySeed::from_code(recovery_code)?;
        verify_recovery_matches(identity, &seed)?;
        // 预算取历史默认（本入口没有 per-call 预算参数）；显式 context 用自己的预算。
        let context = target.relay_context(None)?;
        Self::upload_initial_slot(&context, vault, identity, &seed)?;
        Self::publish_baseline_core_with(&context, vault, identity, SyncOnceOptions::default())
    }

    /// **只凭受保护身份**发布初始基线（ADR 0009 切片 4c-4；面板「发布基线」的库入口）。
    ///
    /// 与 [`Self::publish_initial_pair`] 的差别只有"信任锚 + 初始槽"两步：
    ///
    /// - **不接收恢复码**：管理私钥（`identity.manager_seed`）必须存在，且派生出的管理公钥
    ///   必须等于身份里 pin 住的 `pinned_manager`。Join 设备没有管理私钥 → 明确错误，
    ///   绝不退化成"用设备 key 冒充管理"；
    /// - 初始 recovery 槽由 Create 上传（恢复码当时还在内存里），这里只做 fail-closed
    ///   校验：槽材料与身份/库自洽 + [`INITIAL_SLOT_UPLOADED_META`] 存在。旧实例（建库时
    ///   没传过槽）明确失败并指向 CLI 的恢复码路径——**绝不**静默发布一份 B 无法 Join 的
    ///   假基线；
    /// - 其余（sync + 两份 checkpoint + durable 基线事实）与 CLI 路径**逐字节同一条**
    ///   核心路径（[`Self::publish_baseline_core_with`]），不复制逻辑。
    ///
    /// 幂等语义同 4b：重跑会多一对真实 checkpoint，`/bootstrap` 只取最新两份 published。
    pub fn publish_initial_pair_with_identity<T: RelayTarget + ?Sized>(
        target: &T,
        vault: &Vault,
        identity: &LocalIdentityV1,
    ) -> Result<CheckpointReport, SyncError> {
        Self::publish_initial_pair_with_identity_with(
            target,
            vault,
            identity,
            SyncOnceOptions::default(),
        )
    }

    /// 同 [`Self::publish_initial_pair_with_identity`]，但接收**传输预算 + 集成前守卫**
    /// （ADR 0012 S1：发布内部也跑一轮同步并 `integrate`，所以与同步共用同一条安全纪律）。
    pub fn publish_initial_pair_with_identity_with<T: RelayTarget + ?Sized>(
        target: &T,
        vault: &Vault,
        identity: &LocalIdentityV1,
        options: SyncOnceOptions<'_>,
    ) -> Result<CheckpointReport, SyncError> {
        // 先拒 Join 设备（真正原因），再看槽材料/上传标记（保持既有错误优先级：
        // 本地可判定原因先于 URL/网络配置错误）。
        require_manager_identity(identity)?;
        let slot = load_initial_slot(vault, &identity.vault_id)?;
        verify_identity_slot_consistency(identity, &slot)?;
        if !initial_slot_uploaded(vault, &slot)? {
            return Err(SyncError::Unsupported(
                "本实例建库时没有上传初始 recovery 槽（旧版本 Create）：面板无法补传\
                 （K_slot_mne 只能由恢复码派生）；请先用 --dev-sync-publish 与恢复码补一次，\
                 之后面板「发布基线」一直可用",
            ));
        }
        let context = target.relay_context(options.transport)?;
        Self::publish_baseline_core_with(&context, vault, identity, options)
    }

    /// 上传 genesis 的初始 recovery 槽（staging）并落 durable 标记。
    ///
    /// **只有恢复码在内存里的调用点能用**：`create`（genesis 刚返回）与
    /// [`Self::publish_initial_pair`]（CLI，恢复码来自环境变量）。KEY_SLOT 对象的 MNE1
    /// 密钥 `K_slot_mne = HKDF(恢复码种子)` 不可逆，恢复码又不落盘，所以事后（只有
    /// `manager_seed` 时）无法补传——这是 ADR 0009 切片 4c-4 把槽上传前移的根因。
    ///
    /// message id 确定性派生，整份 KEY_SLOT 对象逐字节可复现，重传 relay 幂等接受。
    fn upload_initial_slot(
        context: &RelayContext,
        vault: &Vault,
        identity: &LocalIdentityV1,
        seed: &RecoverySeed,
    ) -> Result<(), SyncError> {
        Self::upload_initial_slot_with(context, vault, identity, seed, false)
    }

    /// Create 内部的初始槽上传：pending 尚未收尾，走 `open_for_init`（跳过 pending 守卫）。
    pub(crate) fn upload_initial_slot_during_init(
        context: &RelayContext,
        vault: &Vault,
        identity: &LocalIdentityV1,
        seed: &RecoverySeed,
    ) -> Result<(), SyncError> {
        Self::upload_initial_slot_with(context, vault, identity, seed, true)
    }

    fn upload_initial_slot_with(
        context: &RelayContext,
        vault: &Vault,
        identity: &LocalIdentityV1,
        seed: &RecoverySeed,
        during_init: bool,
    ) -> Result<(), SyncError> {
        let slot = load_initial_slot(vault, &identity.vault_id)?;
        verify_identity_slot_consistency(identity, &slot)?;

        let root = root_of(identity);
        let signing_key = DeviceSigningKey::from_seed(identity.ed25519_seed.clone());
        let pinned = if during_init {
            context.open_for_init(vault, identity)?
        } else {
            context.open(vault, identity)?
        };
        let mut client = pinned.into_client();
        ensure_cursor(&mut client, vault)?;
        let message_id = key_slot_message_id(&slot)?;
        {
            let mut writer = CheckpointWriter::new(
                vault,
                &mut client,
                identity.vault_id,
                identity.device_id,
                signing_key,
                root,
                identity.loro_peer_id(),
            );
            writer.upload_recovery_slot(&slot, seed, message_id)?;
        }
        write_initial_slot_uploaded(vault, &slot.descriptor.slot_id, &message_id)
    }

    /// 发布核心：一轮完整 `SyncOnce` + 两份真实同 epoch checkpoint（两条入口共用）。
    ///
    /// - 一轮完整 `SyncOnce`：覆盖证明只认 durable 事实（integrated 前缀/skipped-asset
    ///   记录），所以本地刚建好的笔记必须先推送到 relay 并拉回 integrate；
    /// - 连续两份同 epoch checkpoint（`CheckpointWriter` 的 prepare→validate→promote）：
    ///   `/bootstrap` 只按 `published_at DESC` 取最新两份 published，所以**必须**成对发布。
    ///
    /// 资产源用 [`StorageAssets`]（真实 storage 资产；空库自然是 0 个资产），不用合成
    /// fixture。返回值是**较新那份** checkpoint 的报告（pair = 前一份 + 这一份）。
    fn publish_baseline_core_with(
        context: &RelayContext,
        vault: &Vault,
        identity: &LocalIdentityV1,
        mut options: SyncOnceOptions<'_>,
    ) -> Result<CheckpointReport, SyncError> {
        let root = root_of(identity);
        let signing_key = DeviceSigningKey::from_seed(identity.ed25519_seed.clone());
        let mut client = context.open(vault, identity)?.into_client();
        // 缺 `sync.relay_cursor` 时用 `GET /state` 键 14 兜底（旧 relay 兼容）。
        ensure_cursor(&mut client, vault)?;

        // 1) 发布前把 durable 数据推/拉平（错误一律向上传播：暂停=不得发布基线）。
        //    集成前守卫与同步共用一条：取消/授权已失效时**不 append**。
        {
            let engine_identity = EngineIdentity {
                device_id: identity.device_id,
                signing_key: signing_key.clone(),
                peer_id: identity.loro_peer_id(),
            };
            let mut engine = SyncEngine::new(vault, &mut client, engine_identity, root.clone());
            match options.before_integrate.as_mut() {
                Some(guard) => engine.sync_once_with_guard(&mut **guard)?,
                None => engine.sync_once()?,
            };
        }

        // 2) 两份真实 checkpoint（prepare→validate→promote 全部真实读回/导入）。
        let mut writer = CheckpointWriter::new(
            vault,
            &mut client,
            identity.vault_id,
            identity.device_id,
            signing_key,
            root,
            identity.loro_peer_id(),
        );
        let _first = writer.create_checkpoint(&StorageAssets)?;
        let second = writer.create_checkpoint(&StorageAssets)?;
        write_published_baseline(vault, &second)?;
        Ok(second)
    }

    /// Join：只给 `vault_id + 恢复码 + 新路径`，不读 A 的目录/identity/root。
    ///
    /// 初始化走 ADR 0013 S2b 的受限能力 [`InitSession::new_fresh_recovery`]：
    ///
    /// 1. 恢复码 → 恢复种子 →（会话内）管理私钥（管理信任锚）；
    /// 2. 新库 + durable pending（`Prepared`）→ 写前推进 `GenesisRequested` → `self_enroll`
    ///    自批新设备（唯一允许 unpinned 消费 `/control` 的路径，仍强制管理锚 + 逐条签名）；
    /// 3. 认证 `bootstrap::run_with_pins` 导入基线并单事务推进 pins/cursor；
    /// 4. 用**同一验证视图**提交 record pin + bootstrap 锚（`commit_recovery_pin`）；
    /// 5. pending → `Done` → 清理（清理只在 Done）；app 走
    ///    [`Self::join_deferring_identity`] 时停在 `PinCommitted`，身份落盘后再推进。
    ///
    /// 失败语义沿用 ADR 0009（app 验收依赖它）：**返回错误时清掉本次新建的本地文件**，
    /// 不写 cursor/identity（bootstrap 只在全部导入成功后推进水位）；pending 的用途是
    /// **崩溃**之后的启动对账（[`crate::init_session::reconcile`]）。
    pub fn join(
        relay_url: &str,
        vault_id: &VaultId,
        recovery_code: &str,
        vault_path: &Path,
    ) -> Result<JoinedVault, SyncError> {
        Self::join_inner(relay_url, vault_id, recovery_code, vault_path, true)
    }

    /// **app 专用**：与 [`Self::join`] 相同，但 pending 停在 `PinCommitted`。
    ///
    /// 与 [`Self::create_deferring_identity`] 对称：身份原子落盘后由调用方调用
    /// [`crate::init_session::mark_identity_written`] + `finish_identity_written`，
    /// 崩溃窗口由启动对账（[`crate::init_session::reconcile`]）补 finalize。
    pub fn join_deferring_identity(
        relay_url: &str,
        vault_id: &VaultId,
        recovery_code: &str,
        vault_path: &Path,
    ) -> Result<JoinedVault, SyncError> {
        Self::join_inner(relay_url, vault_id, recovery_code, vault_path, false)
    }

    fn join_inner(
        relay_url: &str,
        vault_id: &VaultId,
        recovery_code: &str,
        vault_path: &Path,
        finish_identity: bool,
    ) -> Result<JoinedVault, SyncError> {
        if vault_path.as_os_str().is_empty() {
            return Err(SyncError::LocalCorrupt("Join 需要明确的 vault 路径"));
        }
        if vault_path.exists() {
            return Err(SyncError::LocalCorrupt(
                "Join 只接受尚不存在的 vault 路径（已有库请按其身份使用）",
            ));
        }

        // 1. 恢复码 → 恢复种子；管理锚由 InitSession 从种子推导（不信任 relay 的公钥）。
        let seed = RecoverySeed::from_code(recovery_code)?;
        let context = RelayContext::local_dev(relay_url, DEFAULT_TRANSPORT_TIMEOUTS)?;
        let session = InitSession::new_fresh_recovery(context, *vault_id, &seed)?;

        // 2. 新库（pending 需要 SQLite；任何失败都清掉刚建的文件）。
        let vault = match Vault::open(vault_path) {
            Ok(vault) => vault,
            Err(error) => {
                remove_vault_files(vault_path);
                return Err(SyncError::Storage(error));
            }
        };
        let joined = (|| -> Result<LocalIdentityV1, SyncError> {
            session.prepare_pending(&vault, SystemClock.now_secs())?;
            let recovered = session.recover_identity(&vault, SystemClock.now_secs())?;
            session.commit_recovery_pin(&vault, &recovered.identity)?;
            if finish_identity {
                session.finish(&vault)?;
            }
            Ok(recovered.identity)
        })();

        match joined {
            Ok(identity) => Ok(JoinedVault {
                vault,
                identity,
                vault_id: *vault_id,
            }),
            Err(error) => {
                // 失败不留下半初始化库（cursor/identity 都没写；文件也清掉，重试从头来）。
                drop(vault);
                remove_vault_files(vault_path);
                Err(error)
            }
        }
    }

    /// SyncOnce：一轮完整同步（`encrypt_pending → push → pull → integrate → ack`）。
    ///
    /// [`SyncEngine`] 是借用型的，每次调用内构造；游标/水位由 engine 自己按 durable 事实推进。
    ///
    /// 这是**向后兼容入口**（CLI/测试/旧调用）：传输预算与集成前守卫都用历史默认
    /// （ADR 0012 S1）。需要可配预算或取消复核时用 [`Self::sync_once_with`]。
    pub fn sync_once<T: RelayTarget + ?Sized>(
        target: &T,
        vault: &Vault,
        identity: &LocalIdentityV1,
    ) -> Result<SyncReport, SyncError> {
        Self::sync_once_with(target, vault, identity, SyncOnceOptions::default())
    }

    /// 同 [`Self::sync_once`]，但由调用方注入**传输预算**与**集成前守卫**（ADR 0012 S1）。
    ///
    /// `options.before_integrate` 在 `integrate` 之前调用一次：返回 `Err` 时本轮
    /// **不会追加任何本地行**（取消/授权已失效），已推送的对象与已拉取的 inbox 保持
    /// durable 事实（重试可复用）。进入 integrate 后事务必须跑完，不再中断。
    pub fn sync_once_with<T: RelayTarget + ?Sized>(
        target: &T,
        vault: &Vault,
        identity: &LocalIdentityV1,
        mut options: SyncOnceOptions<'_>,
    ) -> Result<SyncReport, SyncError> {
        let context = target.relay_context(options.transport)?;
        let root = root_of(identity);
        let signing_key = DeviceSigningKey::from_seed(identity.ed25519_seed.clone());
        let mut client = context.open(vault, identity)?.into_client();
        // pull 必须有游标：B 由 bootstrap 落库，A 由 genesis 落库；旧 relay 两者都没有时
        // 用 `GET /state` 键 14 兜底（绝不把"缺失"当成 0 号游标硬编）。
        ensure_cursor(&mut client, vault)?;
        let engine_identity = EngineIdentity {
            device_id: identity.device_id,
            signing_key,
            peer_id: identity.loro_peer_id(),
        };
        let mut engine = SyncEngine::new(vault, &mut client, engine_identity, root);
        match options.before_integrate.as_mut() {
            Some(guard) => engine.sync_once_with_guard(&mut **guard),
            None => engine.sync_once(),
        }
    }

    // ------------------------------------------------------------------
    // ADR 0012 S2a：可暂停的两阶段同步（Prepare / Integrate / Finish）
    // ------------------------------------------------------------------
    //
    // 三个入口各自**重新构造** client/engine：真正的事实只存在 DB（outbox/inbox/
    // cursor/pins），不依赖进程内 engine。调用方（app 的 worker）负责在阶段之间等待
    // 授权/交付回执；本模块只保证每一步"只做这一阶段的事"：
    //
    // - `sync_prepare`：encrypt → push → pull（**不** integrate、**不** ack）；
    // - `sync_integrate`：integrate（外加授权 head 复核），**不** ack；
    // - `sync_finish`：push tail（若有）→ ack。

    /// 两阶段同步的 **Prepare** 阶段：加密待发送意图 → 推送 → 拉取一页。
    ///
    /// 明确**不**做 `integrate`/`ack`：本阶段只把远端变化落成 durable inbox 并推进
    /// pull 游标，正文/结构 head 一律不动。因此 UI 在网络阶段不需要持闸、可以继续编辑。
    pub fn sync_prepare<T: RelayTarget + ?Sized>(
        target: &T,
        vault: &Vault,
        identity: &LocalIdentityV1,
        transport: Option<TransportTimeouts>,
    ) -> Result<PrepareReport, SyncError> {
        let context = target.relay_context(transport)?;
        let root = root_of(identity);
        let signing_key = DeviceSigningKey::from_seed(identity.ed25519_seed.clone());
        let mut client = context.open(vault, identity)?.into_client();
        ensure_cursor(&mut client, vault)?;
        let engine_identity = EngineIdentity {
            device_id: identity.device_id,
            signing_key,
            peer_id: identity.loro_peer_id(),
        };
        let mut engine = SyncEngine::new(vault, &mut client, engine_identity, root);
        let config = EngineConfig::default();
        let encrypted = engine.encrypt_pending(config.send_batch)?;
        let push = engine.push()?;
        // ADR 0012 两阶段路径的资产上传点：与完整 `sync_once` 同一阶段（正文之后、
        // pull 之前），保证自动同步（S3）也会增量上传新资产。
        let assets = engine.upload_pending_assets()?;
        let pull = engine.pull()?;
        Ok(PrepareReport {
            encrypted,
            push,
            assets,
            pull,
        })
    }

    /// 两阶段同步的 **Integrate** 阶段：把 durable verified inbox 集成进本地库。
    ///
    /// `expect_head` 是调用方（UI 持闸）给出的授权基准：`(doc_id, durable_head_seq)`。
    /// 任一条不满足就返回 [`SyncError::CancelledBeforeIntegrate`]，**不 append**：
    ///
    /// - 库中该文档 head 已被移动（授权失效 / 本地还有未持闸写入）；
    /// - 调用方在进入本阶段前已复核取消标志（见 app worker：先查 `OpControl`，再调本函数）。
    ///
    /// `watch_doc` 是 UI 准备 adopt 的活动文档：报告里会给出它的 head 是否在本阶段前进了，
    /// 供 UI 判定"是否需要交付"。**不** ack（ack 属于 Finish，等 UI adopt/end 之后）。
    ///
    /// 集成本身按 ADR 0012 S2b 的**依赖顺序**收敛（结构 → 只投影 → 正文公平轮转），
    /// 因此调用方不需要"再联网补一轮"来修结构/正文依赖。
    pub fn sync_integrate<T: RelayTarget + ?Sized>(
        target: &T,
        vault: &Vault,
        identity: &LocalIdentityV1,
        transport: Option<TransportTimeouts>,
        expect_head: Option<(&str, u64)>,
        watch_doc: &str,
    ) -> Result<IntegratePhaseReport, SyncError> {
        Self::sync_integrate_with_config(
            target,
            vault,
            identity,
            transport,
            expect_head,
            watch_doc,
            IntegrateConfig::default(),
        )
    }

    /// 同 [`Self::sync_integrate`]，但显式指定集成批次预算（测试/诊断用）。
    #[allow(clippy::too_many_arguments)]
    pub fn sync_integrate_with_config<T: RelayTarget + ?Sized>(
        target: &T,
        vault: &Vault,
        identity: &LocalIdentityV1,
        transport: Option<TransportTimeouts>,
        expect_head: Option<(&str, u64)>,
        watch_doc: &str,
        integrate_config: IntegrateConfig,
    ) -> Result<IntegratePhaseReport, SyncError> {
        // Integrate 前复核授权基准（ADR 0012 S2b）：head 被推进 = 授权已失效，
        // 一律不 append；inbox/outbox 原样保留，调用方可按新授权重试。
        if let Some((doc_id, expected)) = expect_head {
            let head = load_head(vault, doc_id)?;
            if head.head_seq != expected {
                return Err(SyncError::CancelledBeforeIntegrate);
            }
        }
        let before = load_head(vault, watch_doc)?.head_seq;
        let context = target.relay_context(transport)?;
        let root = root_of(identity);
        let signing_key = DeviceSigningKey::from_seed(identity.ed25519_seed.clone());
        let mut client = context.open(vault, identity)?.into_client();
        ensure_cursor(&mut client, vault)?;
        let peer_id = identity.loro_peer_id();
        let engine_identity = EngineIdentity {
            device_id: identity.device_id,
            signing_key,
            peer_id,
        };
        let mut engine = SyncEngine::new(vault, &mut client, engine_identity, root);
        let outcome = engine.integrate_dependency_ordered(integrate_config)?;
        drop(engine);
        // changed heads（正文）触发一次**现有接口**的物化：B 事务写 `notes.body_md`
        // 并发布 `BodyMaterialized` 事件（索引器据此增量更新）。结构投影的事件由
        // `StructureWriter::commit` 自己发布（`StructureProjected`）。
        let (materialized, materialize_failed) =
            materialize_changed_bodies(vault, peer_id, &outcome.changed_heads);
        let after = load_head(vault, watch_doc)?.head_seq;
        let watch_doc_changed = after != before;
        Ok(IntegratePhaseReport {
            integrate: outcome.report,
            changed_heads: outcome.changed_heads,
            watch_doc_changed,
            materialized,
            materialize_failed,
        })
    }

    /// 两阶段同步的 **Finish** 阶段：推送本阶段新落库的本地意图（若有）→ 上报 ACK。
    ///
    /// 调用方契约：UI 已经把 integrate 的 durable 变化 adopt 进 live、并 `end` 释放闸门。
    /// 本阶段不再 `pull`/`integrate`：它只做 "让 relay 知道本机已经收到/整合到哪" 以及
    /// 清掉本轮 Held 期间新产生的本地意图。
    pub fn sync_finish<T: RelayTarget + ?Sized>(
        target: &T,
        vault: &Vault,
        identity: &LocalIdentityV1,
        transport: Option<TransportTimeouts>,
    ) -> Result<FinishReport, SyncError> {
        let context = target.relay_context(transport)?;
        let root = root_of(identity);
        let signing_key = DeviceSigningKey::from_seed(identity.ed25519_seed.clone());
        let mut client = context.open(vault, identity)?.into_client();
        ensure_cursor(&mut client, vault)?;
        let engine_identity = EngineIdentity {
            device_id: identity.device_id,
            signing_key,
            peer_id: identity.loro_peer_id(),
        };
        let mut engine = SyncEngine::new(vault, &mut client, engine_identity, root);
        let config = EngineConfig::default();
        // 先推 tail（Held 窗口内新落库的本地意图），再上报 received/integrated 游标。
        let encrypted = engine.encrypt_pending(config.send_batch)?;
        let push = engine.push()?;
        let ack = engine.ack()?;
        Ok(FinishReport {
            ack,
            encrypted,
            push,
        })
    }

    /// ADR 0010 切片 2a-2：读一次**已验证控制链的只读成员视图**（设备列表 UI 的数据源）。
    ///
    /// 只构造一个**临时** `HttpRelayClient`（用完即丢，不缓存 token/控制链）：调用方拿到的
    /// 只有 [`MemberView`]——它不含 token/密钥，`Debug` 也只打印 id/status/fingerprint。
    /// 缓存失效语义由 2a-1 的 [`HttpRelayClient::members`] 负责（先比对 `/state.auth_rev`
    /// 再决定是否重新拉控制链）；每次新客户端天然读到最新的 `auth_rev`。
    ///
    /// 失败即真实失败（relay 不可达 / 被吊销 `REVOKED` / 控制链 pin 不符），不返回旧快照。
    pub fn members<T: RelayTarget + ?Sized>(
        target: &T,
        vault: &Vault,
        identity: &LocalIdentityV1,
    ) -> Result<Vec<MemberView>, SyncError> {
        let context = target.relay_context(None)?;
        let mut client = context.open(vault, identity)?.into_client();
        client.members()
    }

    /// ADR 0010 切片 1b + 2a-2：**移除一台设备对 relay 的访问**（不轮换密钥）。
    ///
    /// 语义与安全边界严格照 ADR 0010：
    ///
    /// - 只是诚实 relay 上的**访问撤销**（`auth_rev` 前进、目标 `status=revoked`），
    ///   不是签名成员移除、也不是密码学撤销：被移除设备仍可能用它已获得的旧 root 解密
    ///   旧/同 epoch 密文；管理材料（`manager_seed`/恢复码）**不会被撤销**；
    /// - **禁止移除本机**（这里在任何网络动作之前拒绝；helper 里还有第二道）；
    /// - **必须有管理密钥**：只有 `manager_seed` 在身份里的设备（Create/恢复码路径）能管理；
    ///   Join 设备明确失败，绝不退化成"用设备 key 冒充管理"；
    /// - 不删本地身份 / `manager_seed` / 恢复材料 / 笔记，不自动轮换、不把 revoked 翻回 active；
    /// - `request_id` 每次调用随机 16B（`OsRandom`）：响应丢失就整次重试（新 id），
    ///   不拿旧 id 去撞 relay；`expected_control_rev` 取刚读到的 `/state.control_rev`
    ///   （独立吊销**不推进** `control_rev`，CAS 冲突时应刷新后重试）。
    ///
    /// 成功后**重新拉一次成员视图**再返回，调用方（worker）可直接把最新列表投影到面板，
    /// 不需要另发一条列表命令。
    pub fn revoke_device<T: RelayTarget + ?Sized>(
        relay: &T,
        vault: &Vault,
        identity: &LocalIdentityV1,
        target: DeviceId,
    ) -> Result<RevokedDevice, SyncError> {
        // ① 本机守卫：先于任何 IO（打开库/读身份以外）拒绝。
        if target == identity.device_id {
            return Err(SyncError::Unsupported(
                "禁止移除本机设备：会立即自断同步与管理访问（ADR 0010）；请用另一台管理设备或恢复码",
            ));
        }
        // ② 管理密钥必须来自受保护身份（Join 设备没有）：缺它就直接失败。
        let Some(seed) = identity.manager_seed.clone() else {
            return Err(SyncError::Unsupported(
                "本机没有管理密钥（Join 设备）：无法移除设备；请用 Create 设备或最新恢复码操作",
            ));
        };
        let manager = ManagementKey::from_seed(seed);

        // ③ CAS 基准取当前 `/state.control_rev`（独立吊销不推进它；不盲用 identity 里的旧值）。
        let context = relay.relay_context(None)?;
        let mut client = context.open(vault, identity)?.into_client();
        let state = client.state()?;
        if state.instance_id != identity.pinned_instance {
            return Err(SyncError::RelayWire("relay instance 与本地 pin 不符"));
        }

        // ④ 每次点击一个新 request_id（去重 scope 是固定 `management/device-revoke`，
        //    target 在 canonical body 里，跨设备不会碰撞）。
        let request_id: [u8; 16] = array(&mut OsRandom)?;
        let ctx = RevokeContext::from_identity(
            context.origin_str().as_str(),
            identity,
            state.control_rev,
            SystemClock.now_secs(),
        )
        .with_api_version(context.api_version());
        let mut transport = context.transport()?;
        let outcome = crate::device_management::revoke_device(
            &mut transport,
            &ctx,
            &manager,
            target,
            request_id,
        )?;

        // ⑤ 只读刷新：`auth_rev` 已前进，新客户端自然读到吊销后的成员集合。
        let members = Self::members(&context, vault, identity)?;
        Ok(RevokedDevice { outcome, members })
    }

    /// 读 dev-local 实例标签（[`VAULT_LABEL_META`]；Join 的库没有，返回 `None`）。
    pub fn vault_label(vault: &Vault) -> Result<Option<String>, SyncError> {
        match vault.get_meta(VAULT_LABEL_META)? {
            None => Ok(None),
            Some(bytes) => String::from_utf8(bytes)
                .map(Some)
                .map_err(|_| SyncError::LocalCorrupt("vault 标签不是 UTF-8")),
        }
    }

    /// 读最近一次发布基线的 durable 事实（[`PUBLISHED_BASELINE_META`]）。
    pub fn published_baseline(vault: &Vault) -> Result<Option<PublishedBaseline>, SyncError> {
        let Some(bytes) = vault.get_meta(PUBLISHED_BASELINE_META)? else {
            return Ok(None);
        };
        decode_published_baseline(&bytes).map(Some)
    }
}

// ---------------------------------------------------------------- 内部助手

/// genesis 的 password 槽材料只用于凑齐 `GenesisMaterials`（本模块不上传口令槽）：
/// 用 32B 随机数的十六进制当一次性口令，构造完立即丢弃。
pub(crate) fn throwaway_password() -> Result<String, SyncError> {
    let bytes: [u8; 32] = array(&mut OsRandom)?;
    Ok(ids::hex_of(&bytes))
}

/// 身份的当前 epoch 根。
pub(crate) fn root_of(identity: &LocalIdentityV1) -> RootSecretV1 {
    RootSecretV1::new(
        identity.vault_id,
        identity.current_epoch,
        identity.current_root.clone(),
    )
}

/// 对 changed heads 里的**正文**文档触发一次现有接口的物化（`projection_outbox` →
/// `notes.body_md`，成功时发布 `BodyMaterialized` → 索引增量）。
///
/// 只处理派生数据：失败计入返回值并打日志，不改 CRDT 事实、也不改变集成结论
/// （搜索索引还有周期对账兜底）。结构文档的投影事件由 `StructureWriter::commit`
/// 自己发布。
fn materialize_changed_bodies(
    vault: &Vault,
    peer_id: u64,
    changed_heads: &[String],
) -> (usize, usize) {
    if changed_heads.is_empty() {
        return (0, 0);
    }
    let pending = match pending_outbox(vault) {
        Ok(pending) => pending,
        Err(error) => {
            eprintln!("[dev-sync] 读取 projection_outbox 失败（跳过本轮物化）：{error}");
            return (0, changed_heads.len());
        }
    };
    let mut materialized = 0usize;
    let mut failed = 0usize;
    for item in pending {
        if item.doc_id == STRUCTURE_DOC_ID || !changed_heads.iter().any(|head| head == &item.doc_id)
        {
            continue;
        }
        let Some(note_id) = item.note_id else {
            // 结构投影还没建立 notes 行：不是失败，等下一轮结构/索引对账。
            continue;
        };
        match materialize_body(vault, &note_id, &item.doc_id, item.target_seq, peer_id) {
            Ok(_) => materialized += 1,
            Err(error) => {
                failed += 1;
                eprintln!(
                    "[dev-sync] 正文物化失败（{}，目标 seq {}）：{error}",
                    item.doc_id, item.target_seq
                );
            }
        }
    }
    (materialized, failed)
}

/// 把 `PeerId` 的 u64 映射回 16B（`identity.peer_id` 的存储形态）。
///
/// 受限初始化入口（[`crate::init_session`]）也用同一条映射：Join 的 peer 口径只能有
/// 一处，不允许各写一套。
pub(crate) fn peer_bytes_of(peer_id: u64) -> [u8; 16] {
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&peer_id.to_be_bytes());
    bytes
}

/// 缺 `sync.relay_cursor` 时用 relay `GET /state` 键 14 兜底（genesis 键 5 的旧 relay 兼容）。
fn ensure_cursor<T: HttpTransport>(
    client: &mut HttpRelayClient<T>,
    vault: &Vault,
) -> Result<(), SyncError> {
    if vault.get_meta(RELAY_CURSOR_META)?.is_none() {
        let cursor = client.initial_cursor()?;
        vault.set_meta(RELAY_CURSOR_META, cursor.as_bytes())?;
    }
    Ok(())
}

/// 恢复码必须与本地管理身份一致：`HKDF(R, vault_id, "MN/v1/management-ed25519\0")`
/// 逐字节等于 identity 里的 `manager_seed`。错码在这里失败，**不必先上传任何对象**。
fn verify_recovery_matches(
    identity: &LocalIdentityV1,
    seed: &RecoverySeed,
) -> Result<(), SyncError> {
    let Some(stored) = &identity.manager_seed else {
        return Err(SyncError::Unsupported(
            "发布初始基线需要 genesis 身份（含管理私钥）：Join 设备不能发布",
        ));
    };
    let derived = management_seed(seed, &identity.vault_id)?;
    if !derived.ct_eq(stored) {
        return Err(SyncError::RecoveryCode("恢复码与本地管理身份不匹配"));
    }
    Ok(())
}

/// 身份 ↔ 初始 recovery 槽的自洽校验（不联网、不碰秘密输出）。
///
/// 三条都对应真实故障模式，缺一不可：
/// 1. 槽归属 = 身份 vault（加载时已校验，这里再兜一次防"换库/换身份"的错配）；
/// 2. 槽 epoch = 身份当前 epoch：轮换过 epoch 的实例不能再用"初始基线"发布（否则会拿
///    旧 epoch 的槽配新 root，Join 端解出的根对不上）；
/// 3. 管理私钥必须存在，且派生公钥 == `pinned_manager`（管理 pin 自洽）：Join 设备没有
///    管理私钥 → 明确拒绝，绝不退化成"用设备 key 冒充管理"。
fn verify_identity_slot_consistency(
    identity: &LocalIdentityV1,
    slot: &KeySlotV1,
) -> Result<(), SyncError> {
    if slot.descriptor.vault_id != identity.vault_id {
        return Err(SyncError::LocalCorrupt(
            "初始 recovery 槽的 vault 归属与身份不一致",
        ));
    }
    if slot.descriptor.key_epoch != identity.current_epoch {
        return Err(SyncError::Unsupported(
            "初始基线只能在 genesis epoch 发布：初始槽的 epoch 与身份当前 epoch 不一致\
             （轮换之后不能用初始基线发布）",
        ));
    }
    require_manager_identity(identity)
}

/// 管理密钥（信任锚）必须来自受保护身份，且与 `pinned_manager` 自洽。
///
/// 这条校验**先于**读槽/任何上传：Join 设备（`manager_seed = None`）必须在拿到"缺初始槽"
/// 之类无关错误之前就看到真正原因，也绝不退化成"用设备 key 冒充管理"。
fn require_manager_identity(identity: &LocalIdentityV1) -> Result<(), SyncError> {
    let Some(seed) = identity.manager_seed.clone() else {
        return Err(SyncError::Unsupported(
            "发布初始基线需要 genesis 身份（含管理私钥）：Join 设备没有管理密钥，不能发布",
        ));
    };
    if ManagementKey::from_seed(seed).public_key() != identity.pinned_manager {
        return Err(SyncError::LocalCorrupt(
            "身份的管理私钥与 pinned_manager 不一致（身份文件被换过或损坏）",
        ));
    }
    Ok(())
}

/// 初始 recovery 槽（KEY_SLOT 对象）的确定性 `message_id`。
///
/// `message_id` 进 MNE1 header/AAD；relay 对同 `object_id` 换了 `message_id`（=不同信封
/// 字节）会返回 409 ID_CONFLICT，而"上传初始槽 / 发布基线"都是**可重跑**的动作（Create
/// 传过一次、CLI 发布补传、checkpoint 失败后重试）。所以这里只用槽材料（slot_id + 描述符
/// CDE，都是非秘密）派生一个稳定 id：重试/重启/跨入口都得到逐字节相同的 KEY_SLOT 对象，
/// relay 按幂等接受。
fn key_slot_message_id(slot: &KeySlotV1) -> Result<[u8; 16], SyncError> {
    let descriptor = slot.descriptor.encode()?;
    let mut hasher = Sha256::new();
    hasher.update(KEY_SLOT_MESSAGE_DOMAIN);
    hasher.update(slot.descriptor.slot_id);
    hasher.update(&descriptor);
    let digest = hasher.finalize();
    let mut message_id = [0u8; 16];
    message_id.copy_from_slice(&digest[..16]);
    Ok(message_id)
}

/// 从 `meta` 读回 genesis 的 recovery 槽（描述符 + 包裹根），并校验归属。
fn load_initial_slot(vault: &Vault, vault_id: &VaultId) -> Result<KeySlotV1, SyncError> {
    let bytes = vault
        .get_meta(INITIAL_RECOVERY_SLOT_META)?
        .ok_or(SyncError::LocalCorrupt(
            "vault 缺 genesis 初始 recovery 槽记录（只有 DevLocal::create 的库能发布基线）",
        ))?;
    decode_initial_slot(&bytes, vault_id)
}

/// `magic || u32 BE(descriptor_len) || CDE(descriptor) || nonce||ciphertext`。
pub(crate) fn encode_initial_slot(slot: &KeySlotV1) -> Result<Vec<u8>, SyncError> {
    let descriptor = slot.descriptor.encode()?;
    let descriptor_len = u32::try_from(descriptor.len())
        .map_err(|_| SyncError::LocalCorrupt("初始槽描述符长度超出 u32"))?;
    let wrapped = slot.wrapped_bytes();
    let mut out =
        Vec::with_capacity(INITIAL_SLOT_MAGIC.len() + 4 + descriptor.len() + wrapped.len());
    out.extend_from_slice(INITIAL_SLOT_MAGIC);
    out.extend_from_slice(&descriptor_len.to_be_bytes());
    out.extend_from_slice(&descriptor);
    out.extend_from_slice(&wrapped);
    Ok(out)
}

/// 解析 [`encode_initial_slot`] 的记录；任何不自洽都按本地损坏处理（不猜、不修复）。
fn decode_initial_slot(bytes: &[u8], vault_id: &VaultId) -> Result<KeySlotV1, SyncError> {
    let header_len = INITIAL_SLOT_MAGIC.len() + 4;
    if bytes.len() < header_len || !bytes.starts_with(INITIAL_SLOT_MAGIC) {
        return Err(SyncError::LocalCorrupt(
            "初始 recovery 槽记录 magic/长度不符",
        ));
    }
    let mut raw_len = [0u8; 4];
    raw_len.copy_from_slice(&bytes[INITIAL_SLOT_MAGIC.len()..header_len]);
    let descriptor_len = usize::try_from(u32::from_be_bytes(raw_len))
        .map_err(|_| SyncError::LocalCorrupt("初始 recovery 槽描述符长度非法"))?;
    let descriptor_end = header_len
        .checked_add(descriptor_len)
        .ok_or(SyncError::LocalCorrupt("初始 recovery 槽描述符长度溢出"))?;
    if descriptor_len == 0
        || descriptor_len > MAX_SLOT_DESCRIPTOR_LEN
        || descriptor_end >= bytes.len()
    {
        return Err(SyncError::LocalCorrupt(
            "初始 recovery 槽记录的分段长度非法",
        ));
    }
    let descriptor = SlotDescriptorV1::decode(&bytes[header_len..descriptor_end])?;
    if descriptor.vault_id != *vault_id {
        return Err(SyncError::LocalCorrupt("初始 recovery 槽的 vault 归属不符"));
    }
    if descriptor.kind != SlotKind::Recovery {
        return Err(SyncError::LocalCorrupt("初始槽记录不是 recovery 槽"));
    }
    let wrapped = WrappedRoot::from_bytes(&bytes[descriptor_end..])
        .map_err(|_| SyncError::LocalCorrupt("初始 recovery 槽的槽体损坏"))?;
    Ok(KeySlotV1 {
        descriptor,
        wrapped,
    })
}

/// `magic || slot_id(32) || message_id(16)`（初始 recovery 槽「已上传」记录）。
fn encode_initial_slot_uploaded(slot_id: &[u8; 32], message_id: &[u8; 16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(INITIAL_SLOT_UPLOADED_MAGIC.len() + 32 + 16);
    out.extend_from_slice(INITIAL_SLOT_UPLOADED_MAGIC);
    out.extend_from_slice(slot_id);
    out.extend_from_slice(message_id);
    out
}

/// 解析 [`encode_initial_slot_uploaded`] 的记录（长度/magic 不符按本地损坏处理）。
fn decode_initial_slot_uploaded(bytes: &[u8]) -> Result<([u8; 32], [u8; 16]), SyncError> {
    let expected = INITIAL_SLOT_UPLOADED_MAGIC.len() + 32 + 16;
    if bytes.len() != expected || !bytes.starts_with(INITIAL_SLOT_UPLOADED_MAGIC) {
        return Err(SyncError::LocalCorrupt(
            "初始 recovery 槽上传记录 magic/长度不符",
        ));
    }
    let mut slot_id = [0u8; 32];
    let mut message_id = [0u8; 16];
    slot_id.copy_from_slice(&bytes[INITIAL_SLOT_UPLOADED_MAGIC.len()..][..32]);
    message_id.copy_from_slice(&bytes[INITIAL_SLOT_UPLOADED_MAGIC.len() + 32..]);
    Ok((slot_id, message_id))
}

/// 上传成功后写 durable 标记（`create` / CLI 发布都会走这里；重传幂等）。
fn write_initial_slot_uploaded(
    vault: &Vault,
    slot_id: &[u8; 32],
    message_id: &[u8; 16],
) -> Result<(), SyncError> {
    vault.set_meta(
        INITIAL_SLOT_UPLOADED_META,
        &encode_initial_slot_uploaded(slot_id, message_id),
    )?;
    Ok(())
}

/// 初始 recovery 槽是否**当前这份槽材料**已经上传过（`false` = 旧版本 Create 的实例）。
///
/// 记录存在但 `slot_id` 对不上 → 本地事实自相矛盾（换过库/槽材料），按损坏 fail-closed，
/// 不猜、不重写。
fn initial_slot_uploaded(vault: &Vault, slot: &KeySlotV1) -> Result<bool, SyncError> {
    let Some(bytes) = vault.get_meta(INITIAL_SLOT_UPLOADED_META)? else {
        return Ok(false);
    };
    let (slot_id, _message_id) = decode_initial_slot_uploaded(&bytes)?;
    if slot_id != slot.descriptor.slot_id {
        return Err(SyncError::LocalCorrupt(
            "初始 recovery 槽上传记录与本库槽材料不一致（不重写、不猜）",
        ));
    }
    Ok(true)
}

/// 把"最新一份已发布 checkpoint"落成本地 durable 事实（不覆盖更旧的两份，只记较新的）。
fn write_published_baseline(vault: &Vault, report: &CheckpointReport) -> Result<(), SyncError> {
    let published_at = report
        .published_at
        .ok_or(SyncError::LocalCorrupt("checkpoint report 缺发布时间"))?;
    let mut out = Vec::with_capacity(PUBLISHED_BASELINE_MAGIC.len() + 16 + 24);
    out.extend_from_slice(PUBLISHED_BASELINE_MAGIC);
    out.extend_from_slice(&report.checkpoint_id);
    out.extend_from_slice(&report.epoch.to_be_bytes());
    out.extend_from_slice(&report.covered_seq.to_be_bytes());
    out.extend_from_slice(&published_at.to_be_bytes());
    vault.set_meta(PUBLISHED_BASELINE_META, &out)?;
    Ok(())
}

/// 解析 [`write_published_baseline`] 的记录。
fn decode_published_baseline(bytes: &[u8]) -> Result<PublishedBaseline, SyncError> {
    let expected = PUBLISHED_BASELINE_MAGIC.len() + 16 + 8 * 3;
    if bytes.len() != expected || !bytes.starts_with(PUBLISHED_BASELINE_MAGIC) {
        return Err(SyncError::LocalCorrupt("已发布基线记录 magic/长度不符"));
    }
    let mut checkpoint_id = [0u8; 16];
    let mut offset = PUBLISHED_BASELINE_MAGIC.len();
    checkpoint_id.copy_from_slice(&bytes[offset..offset + 16]);
    offset += 16;
    let mut field = |name: &'static str| -> Result<u64, SyncError> {
        let end = offset
            .checked_add(8)
            .ok_or(SyncError::LocalCorrupt("已发布基线记录越界"))?;
        let raw = bytes
            .get(offset..end)
            .ok_or(SyncError::LocalCorrupt(name))?;
        offset = end;
        let mut buf = [0u8; 8];
        buf.copy_from_slice(raw);
        Ok(u64::from_be_bytes(buf))
    };
    let epoch = field("已发布基线记录缺 epoch")?;
    let covered_seq = field("已发布基线记录缺 covered_seq")?;
    let published_at = field("已发布基线记录缺 published_at")?;
    Ok(PublishedBaseline {
        checkpoint_id,
        epoch,
        covered_seq,
        published_at,
    })
}

/// 清掉"本次调用刚建出来"的 vault 文件（主文件 + WAL 边车）与**空**实例目录。
///
/// 只允许在调用方已确认路径原本不存在时使用；失败路径尽力而为，不掩盖原始错误。
///
/// 目录只在"刚被本次初始化创建且为空"时删除（`remove_dir` 对非空目录失败，因此用户
/// 已有内容的目录不会被删）：Create 现在必须**先**建库写 pending、再联网（ADR 0013
/// S2b 的写前记录），所以"联网失败不该留下实例目录"这条既有验收要靠这里兜住。
pub(crate) fn remove_vault_files(path: &Path) {
    let _ = std::fs::remove_file(path);
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut sidecar = path.as_os_str().to_os_string();
        sidecar.push(suffix);
        let _ = std::fs::remove_file(PathBuf::from(sidecar));
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            let _ = std::fs::remove_dir(parent);
        }
    }
}
