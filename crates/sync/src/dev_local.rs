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
//! - relay 必须是 loopback 明文（[`StdTcpTransport::new_local_dev`]）；本模块不提供
//!   生产 TLS 入口，也不实现验签——测试用 `menghunote-dev-relay`，它是**放行验签**的
//!   测试脚手架，**非生产**；正式鉴权负例必须走 relay 生产 hook；
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

use crate::bootstrap::{self, BootstrapPins};
use crate::checkpoint::{CheckpointReport, CheckpointWriter, StorageAssets};
use crate::device::{DeviceSigningKey, ManagementKey};
use crate::device_management::{RevokeContext, RevokeOutcome};
use crate::engine::{
    AckReport, EngineConfig, EngineIdentity, IntegrateConfig, IntegrateReport, PullReport,
    PushReport, SyncEngine, SyncReport, RELAY_CURSOR_META,
};
use crate::error::SyncError;
use crate::genesis;
use crate::http_relay::{
    Clock, ControlPin, HttpRelayClient, HttpRelayConfig, MemberView, StdTcpTransport, SystemClock,
};
use crate::http_transport::TransportTimeouts;
use crate::identity::LocalIdentityV1;
use crate::ids::{self, PeerId};
use crate::keys::RootSecretV1;
use crate::random::{array, OsRandom};
use crate::recovery::{management_seed, RecoverySeed};
use crate::recovery_enroll;
use crate::relay::RelayClient as _;
use crate::slot::{KeySlotV1, WrappedRoot};
use crate::{DeviceId, VaultId};
use menghunote_storage::{
    ensure_structure_row, load_head, materialize_body, pending_outbox, set_sync_log_id, Vault,
    STRUCTURE_DOC_ID,
};
use menghunote_sync_proto::{SlotDescriptorV1, SlotKind};

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
    /// 顺序刻意是"先联网、后落地"：genesis 失败时本地连文件都不该出现。
    /// `vault_label` 只写本机 meta（[`VAULT_LABEL_META`]），不上 wire。
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
        if vault_path.as_os_str().is_empty() {
            return Err(SyncError::LocalCorrupt("Create 需要明确的 vault 路径"));
        }
        if vault_path.exists() {
            return Err(SyncError::LocalCorrupt(
                "Create 只接受尚不存在的 vault 路径（已有库请按其身份使用）",
            ));
        }

        // 1. 先联网：genesis 失败时本地不产生任何文件。
        let mut transport = StdTcpTransport::new_local_dev_with(relay_url, None)?;
        let throwaway_password = throwaway_password()?;
        let outcome = genesis::run(
            &mut transport,
            setup_secret,
            &throwaway_password,
            SystemClock.now_secs(),
            &mut OsRandom,
        )?;

        // 2. 再落地：打开（新建）库并写 durable 事实；任何失败都清掉刚建的文件。
        let vault = match Vault::open(vault_path) {
            Ok(vault) => vault,
            Err(error) => {
                remove_vault_files(vault_path);
                return Err(SyncError::Storage(error));
            }
        };
        let setup = (|| -> Result<(), SyncError> {
            ensure_structure_row(&vault)?;
            vault.set_meta(VAULT_LABEL_META, vault_label.as_bytes())?;
            // 初始游标：genesis 响应键 5（旧 relay 可能没有 —— 那就留空，publish 时用
            // `GET /state` 键 14 兜底，且绝不把"缺失"当成 0 号游标）。
            if let Some(cursor) = &outcome.initial_cursor {
                vault.set_meta(RELAY_CURSOR_META, cursor.as_bytes())?;
            }
            // log_id 来自 genesis 响应：先钉住 relay 日志代，pull 再复核。
            set_sync_log_id(&vault, &outcome.log_id)?;
            // 初始 recovery 槽材料（密文）留给发布/Join 复用（同一 slot_id 幂等）。
            let slot_record = encode_initial_slot(&outcome.materials.recovery_slot)?;
            vault.set_meta(INITIAL_RECOVERY_SLOT_META, &slot_record)?;
            Ok(())
        })();
        if let Err(error) = setup {
            drop(vault);
            remove_vault_files(vault_path);
            return Err(error);
        }

        // 初始 recovery 槽必须**现在**上传（ADR 0009 切片 4c-4）：KEY_SLOT 对象的 MNE1
        // 密钥 `K_slot_mne = HKDF(恢复码种子)` 不可逆，而恢复码只在这一刻还在内存里、不落盘。
        // 早传一步的收益：面板「发布基线」只凭受保护身份就能做（不需要恢复码），
        // 不再要求"关程序 → CLI publish → 重开"；失败沿用 create 的清理语义。
        if let Err(error) = Self::upload_initial_slot(
            relay_url,
            &vault,
            &outcome.identity,
            &outcome.materials.recovery_seed,
        ) {
            drop(vault);
            remove_vault_files(vault_path);
            return Err(error);
        }

        Ok(CreatedVault {
            vault,
            vault_id: outcome.identity.vault_id,
            recovery_code: outcome.materials.recovery_code.as_str().to_owned(),
            identity: outcome.identity,
            setup_secret: setup_secret.to_owned(),
        })
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
    pub fn publish_initial_pair(
        relay_url: &str,
        vault: &Vault,
        identity: &LocalIdentityV1,
        recovery_code: &str,
    ) -> Result<CheckpointReport, SyncError> {
        let seed = RecoverySeed::from_code(recovery_code)?;
        verify_recovery_matches(identity, &seed)?;
        Self::upload_initial_slot(relay_url, vault, identity, &seed)?;
        Self::publish_baseline_core_with(relay_url, vault, identity, SyncOnceOptions::default())
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
    pub fn publish_initial_pair_with_identity(
        relay_url: &str,
        vault: &Vault,
        identity: &LocalIdentityV1,
    ) -> Result<CheckpointReport, SyncError> {
        Self::publish_initial_pair_with_identity_with(
            relay_url,
            vault,
            identity,
            SyncOnceOptions::default(),
        )
    }

    /// 同 [`Self::publish_initial_pair_with_identity`]，但接收**传输预算 + 集成前守卫**
    /// （ADR 0012 S1：发布内部也跑一轮同步并 `integrate`，所以与同步共用同一条安全纪律）。
    pub fn publish_initial_pair_with_identity_with(
        relay_url: &str,
        vault: &Vault,
        identity: &LocalIdentityV1,
        options: SyncOnceOptions<'_>,
    ) -> Result<CheckpointReport, SyncError> {
        // 先拒 Join 设备（真正原因），再看槽材料/上传标记。
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
        Self::publish_baseline_core_with(relay_url, vault, identity, options)
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
        relay_url: &str,
        vault: &Vault,
        identity: &LocalIdentityV1,
        seed: &RecoverySeed,
    ) -> Result<(), SyncError> {
        let slot = load_initial_slot(vault, &identity.vault_id)?;
        verify_identity_slot_consistency(identity, &slot)?;

        let root = root_of(identity);
        let signing_key = DeviceSigningKey::from_seed(identity.ed25519_seed.clone());
        let mut client = client_for(relay_url, vault, identity, &root, signing_key.clone(), None)?;
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
        relay_url: &str,
        vault: &Vault,
        identity: &LocalIdentityV1,
        mut options: SyncOnceOptions<'_>,
    ) -> Result<CheckpointReport, SyncError> {
        let root = root_of(identity);
        let signing_key = DeviceSigningKey::from_seed(identity.ed25519_seed.clone());
        let mut client = client_for(
            relay_url,
            vault,
            identity,
            &root,
            signing_key.clone(),
            options.transport,
        )?;
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
    /// 顺序：恢复码 → 管理密钥（信任锚）→ `self_enroll`（管理签名建 invite/自批 HPKE
    /// grant，并解出根）→ **然后**才新建本地库 → `bootstrap::run_with_pins` 认证读取、
    /// 全量校验、staging 导入、单事务推进 pins/cursor。失败时清掉新建的本地文件，
    /// 不写 cursor/identity（bootstrap 只在全部导入成功后推进水位）。
    pub fn join(
        relay_url: &str,
        vault_id: &VaultId,
        recovery_code: &str,
        vault_path: &Path,
    ) -> Result<JoinedVault, SyncError> {
        if vault_path.as_os_str().is_empty() {
            return Err(SyncError::LocalCorrupt("Join 需要明确的 vault 路径"));
        }
        if vault_path.exists() {
            return Err(SyncError::LocalCorrupt(
                "Join 只接受尚不存在的 vault 路径（已有库请按其身份使用）",
            ));
        }

        // 1. 恢复码 → 恢复种子 → 管理私钥（管理信任锚）。
        let seed = RecoverySeed::from_code(recovery_code)?;
        let manager = ManagementKey::from_recovery(&seed, vault_id)?;
        let manager_pub = manager.public_key();

        // 2. 恢复码自批新设备；root 从恢复槽解出（不需要 A 在线/不需要 A 的目录）。
        let mut transport = StdTcpTransport::new_local_dev_with(relay_url, None)?;
        let enrolled = recovery_enroll::self_enroll(
            &mut transport,
            relay_url,
            *vault_id,
            recovery_code,
            SystemClock.now_secs(),
        )?;
        if enrolled.vault_id != *vault_id {
            return Err(SyncError::RelayWire("自批返回的 vault 与请求不一致"));
        }

        // 3. 本地身份先按"peer 的唯一定义"构造（bootstrap 导入文档时必须用同一个 peer）。
        let peer_bytes = peer_bytes_of(enrolled.peer_id);
        let peer_id = PeerId::from_bytes(peer_bytes);
        let device_peer = peer_id.loro_u64();
        if device_peer != enrolled.peer_id {
            return Err(SyncError::LocalCorrupt(
                "自批 peer id 无法按 ADR 0009 的映射稳定复现",
            ));
        }

        // 4. 新库（必须在包围/校验之后才创建）。
        let vault = match Vault::open(vault_path) {
            Ok(vault) => vault,
            Err(error) => {
                remove_vault_files(vault_path);
                return Err(SyncError::Storage(error));
            }
        };

        let joined = (|| -> Result<LocalIdentityV1, SyncError> {
            let transport = StdTcpTransport::new_local_dev_with(relay_url, None)?;
            let config = HttpRelayConfig {
                base_url: relay_url.to_owned(),
                vault_id: *vault_id,
                device_id: enrolled.device_id,
                manager_pub: Some(manager_pub),
            };
            // 新设备没有任何 pin：只有这条路径允许"无 pin 消费 /control"，且仍强制
            // 管理公钥信任锚 + 逐条管理签名（ADR 0009 安全边界）。
            let mut client = HttpRelayClient::new(transport, config, enrolled.signing_key.clone())
                .with_root(enrolled.root.clone())
                .with_unpinned_bootstrap_control();
            let pins = BootstrapPins::from_vault(&vault)?;
            let report = bootstrap::run_with_pins(
                &vault,
                &mut client,
                *vault_id,
                device_peer,
                &enrolled.root,
                &pins,
            )?;
            let state = client.state()?;
            if report.covered_seq > state.head_seq {
                return Err(SyncError::RelayWire("bootstrap 覆盖水位高于 relay head"));
            }
            Ok(LocalIdentityV1 {
                vault_id: *vault_id,
                device_id: enrolled.device_id,
                peer_id,
                ed25519_seed: enrolled.signing_key.seed().clone(),
                x25519_secret: enrolled.hpke_key.secret().clone(),
                current_epoch: enrolled.root.key_epoch,
                current_root: enrolled.root.root.clone(),
                // 普通设备没有管理私钥：恢复码只在 Join 时用过，不留在身份里。
                manager_seed: None,
                pinned_instance: state.instance_id,
                pinned_manager: manager_pub,
                control_rev: state.control_rev,
                pending_rotation: None,
            })
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
    pub fn sync_once(
        relay_url: &str,
        vault: &Vault,
        identity: &LocalIdentityV1,
    ) -> Result<SyncReport, SyncError> {
        Self::sync_once_with(relay_url, vault, identity, SyncOnceOptions::default())
    }

    /// 同 [`Self::sync_once`]，但由调用方注入**传输预算**与**集成前守卫**（ADR 0012 S1）。
    ///
    /// `options.before_integrate` 在 `integrate` 之前调用一次：返回 `Err` 时本轮
    /// **不会追加任何本地行**（取消/授权已失效），已推送的对象与已拉取的 inbox 保持
    /// durable 事实（重试可复用）。进入 integrate 后事务必须跑完，不再中断。
    pub fn sync_once_with(
        relay_url: &str,
        vault: &Vault,
        identity: &LocalIdentityV1,
        mut options: SyncOnceOptions<'_>,
    ) -> Result<SyncReport, SyncError> {
        let root = root_of(identity);
        let signing_key = DeviceSigningKey::from_seed(identity.ed25519_seed.clone());
        let mut client = client_for(
            relay_url,
            vault,
            identity,
            &root,
            signing_key.clone(),
            options.transport,
        )?;
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
    pub fn sync_prepare(
        relay_url: &str,
        vault: &Vault,
        identity: &LocalIdentityV1,
        transport: Option<TransportTimeouts>,
    ) -> Result<PrepareReport, SyncError> {
        let root = root_of(identity);
        let signing_key = DeviceSigningKey::from_seed(identity.ed25519_seed.clone());
        let mut client = client_for(
            relay_url,
            vault,
            identity,
            &root,
            signing_key.clone(),
            transport,
        )?;
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
        let pull = engine.pull()?;
        Ok(PrepareReport {
            encrypted,
            push,
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
    pub fn sync_integrate(
        relay_url: &str,
        vault: &Vault,
        identity: &LocalIdentityV1,
        transport: Option<TransportTimeouts>,
        expect_head: Option<(&str, u64)>,
        watch_doc: &str,
    ) -> Result<IntegratePhaseReport, SyncError> {
        Self::sync_integrate_with_config(
            relay_url,
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
    pub fn sync_integrate_with_config(
        relay_url: &str,
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
        let root = root_of(identity);
        let signing_key = DeviceSigningKey::from_seed(identity.ed25519_seed.clone());
        let mut client = client_for(
            relay_url,
            vault,
            identity,
            &root,
            signing_key.clone(),
            transport,
        )?;
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
    pub fn sync_finish(
        relay_url: &str,
        vault: &Vault,
        identity: &LocalIdentityV1,
        transport: Option<TransportTimeouts>,
    ) -> Result<FinishReport, SyncError> {
        let root = root_of(identity);
        let signing_key = DeviceSigningKey::from_seed(identity.ed25519_seed.clone());
        let mut client = client_for(
            relay_url,
            vault,
            identity,
            &root,
            signing_key.clone(),
            transport,
        )?;
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
    pub fn members(
        relay_url: &str,
        vault: &Vault,
        identity: &LocalIdentityV1,
    ) -> Result<Vec<MemberView>, SyncError> {
        let root = root_of(identity);
        let signing_key = DeviceSigningKey::from_seed(identity.ed25519_seed.clone());
        let mut client = client_for(relay_url, vault, identity, &root, signing_key, None)?;
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
    pub fn revoke_device(
        relay_url: &str,
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
        let root = root_of(identity);
        let signing_key = DeviceSigningKey::from_seed(identity.ed25519_seed.clone());
        let mut client = client_for(relay_url, vault, identity, &root, signing_key, None)?;
        let state = client.state()?;
        if state.instance_id != identity.pinned_instance {
            return Err(SyncError::RelayWire("relay instance 与本地 pin 不符"));
        }

        // ④ 每次点击一个新 request_id（去重 scope 是固定 `management/device-revoke`，
        //    target 在 canonical body 里，跨设备不会碰撞）。
        let request_id: [u8; 16] = array(&mut OsRandom)?;
        let ctx = RevokeContext::from_identity(
            relay_url,
            identity,
            state.control_rev,
            SystemClock.now_secs(),
        );
        let mut transport = StdTcpTransport::new_local_dev_with(relay_url, None)?;
        let outcome = crate::device_management::revoke_device(
            &mut transport,
            &ctx,
            &manager,
            target,
            request_id,
        )?;

        // ⑤ 只读刷新：`auth_rev` 已前进，新客户端自然读到吊销后的成员集合。
        let members = Self::members(relay_url, vault, identity)?;
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
fn throwaway_password() -> Result<String, SyncError> {
    let bytes: [u8; 32] = array(&mut OsRandom)?;
    Ok(ids::hex_of(&bytes))
}

/// 身份的当前 epoch 根。
fn root_of(identity: &LocalIdentityV1) -> RootSecretV1 {
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
fn peer_bytes_of(peer_id: u64) -> [u8; 16] {
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&peer_id.to_be_bytes());
    bytes
}

/// 构造设备 realm 的 HTTP 客户端：显式管理信任锚 + 本地控制头 pin（identity 下界，
/// 有 bootstrap pin 的哈希就一起带上，绝不借 loopback 豁免绕过已有 pin）。
///
/// `timeouts` 由调用方（ADR 0012 S1 的 `SyncOnceOptions`）传入；`None` = 历史默认预算。
fn client_for(
    relay_url: &str,
    vault: &Vault,
    identity: &LocalIdentityV1,
    root: &RootSecretV1,
    signing_key: DeviceSigningKey,
    timeouts: Option<TransportTimeouts>,
) -> Result<HttpRelayClient<StdTcpTransport>, SyncError> {
    let transport = StdTcpTransport::new_local_dev_with(relay_url, timeouts)?;
    let config = HttpRelayConfig {
        base_url: relay_url.to_owned(),
        vault_id: identity.vault_id,
        device_id: identity.device_id,
        manager_pub: Some(identity.pinned_manager),
    };
    Ok(HttpRelayClient::new(transport, config, signing_key)
        .with_root(root.clone())
        .with_control_pin(control_pin(vault, identity)?))
}

/// 本地控制头 pin：identity 的 `(instance, control_rev)` 是下界；bootstrap 落库的
/// `sync.pinned_control_hash` 提供同 rev 完整性（A 侧没有，就只做下界）。
fn control_pin(vault: &Vault, identity: &LocalIdentityV1) -> Result<ControlPin, SyncError> {
    let mut pin = ControlPin::from_identity(identity);
    let pins = BootstrapPins::from_vault(vault)?;
    if let Some(hash) = pins.control_hash {
        pin = pin.with_bootstrap_hash(hash);
    }
    Ok(pin)
}

/// 缺 `sync.relay_cursor` 时用 relay `GET /state` 键 14 兜底（genesis 键 5 的旧 relay 兼容）。
fn ensure_cursor(
    client: &mut HttpRelayClient<StdTcpTransport>,
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
fn encode_initial_slot(slot: &KeySlotV1) -> Result<Vec<u8>, SyncError> {
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

/// 清掉"本次调用刚建出来"的 vault 文件（主文件 + WAL 边车）。
///
/// 只允许在调用方已确认路径原本不存在时使用；失败路径尽力而为，不掩盖原始错误。
fn remove_vault_files(path: &Path) {
    let _ = std::fs::remove_file(path);
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut sidecar = path.as_os_str().to_os_string();
        sidecar.push(suffix);
        let _ = std::fs::remove_file(PathBuf::from(sidecar));
    }
}
