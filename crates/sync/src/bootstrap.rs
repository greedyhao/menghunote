//! 新设备 bootstrap（S7-18；ADR §3.7/§4/§7/§25.3；评审 0002 P1-4）。
//!
//! 本模块先落"数据恢复"这一段：给定已解出的 root（由槽解封或后续恢复流程提供），
//! `POST /bootstrap` 取两份基线 → 下 manifest/snapshot/asset → 全量校验 → 空库落库。
//!
//! **可恢复导入（P1-4）**：docs/structure/assets 的写入全部经过 storage 的
//! `baseline_apply` staging journal（`begin_baseline_apply` / `apply_baseline_batch` /
//! `apply_baseline_assets`）。下载/写入阶段任一步失败都**不**清 journal、**不**推进任何
//! 水位；下一次调用按同一份基线幂等续跑。只有全部实际导入并通过读回校验后，才在**一个**
//! SQLite 事务里推进 `cursor/received/integrated/confirmed_highwater`（CAS）。
//!
//! **读回校验失败（同步硬化 P1）**：`verify_imported` 失败时**不回滚**已 merge 的
//! Loro/资产数据（CRDT merge 幂等；回滚会丢本地编辑），而是写一条只本地可见的
//! `sync.baseline_verify_failed` 失败标记（storage `mark_baseline_verify_failed`）。
//! journal 仍是 `staging`（schema 的 status CHECK 只允许 `staging/completed`，见
//! `docs/notes/sync-hardening-20261121.md` §8），但下一次 `begin_baseline_apply` 会拒绝，
//! 必须先 `abort_baseline_apply` 清理（abort 同时删标记，已导入数据保留）。
//!
//! **闸外编辑**：verify 不与编辑互斥，判定用「导出点 VV ⊆ 当前 VV」而不是相等；只有
//! 本地没有快照之外的 op 时才逐字节比较正文（见 [`check_imported_doc`]）。
//!
//! **水位单调（P1-4）**：bootstrap 允许作用在已有库上，但禁止降低任何单调值：
//! `received/integrated/highwater` 取 `max(现有, baseline S)`；已有进度时保留现有 relay
//! 游标（不把已经消费过的位置回退）；`instance/log_id/epoch/control rev+hash` 只允许
//! 前进或不变。storage 的 `finish_baseline_apply` 会把 highwater 直接写成 baseline S，
//! 对"已有 highwater 高于 S"的库会降低水位，因此本模块在同一个事务里自行做
//! `max` 语义的 CAS 推进，随后清掉 staging（`abort_baseline_apply` 只清 journal，
//! 不删已导入的 Loro/asset 数据）。
//!
//! **实际 VV 校验（P1-4）**：每个 snapshot 都在独立的空 `LoroDoc` 里 import，比较
//! actual VV / frontier 与 manifest 声明（既不虚高也不缺 op），并校验 full-history、
//! 信封字段、app schema / min_reader；结构文档再重放出结构树，校验"结构引用的正文
//! 文档必须都在基线 manifest 里"。不只信声明。
//!
//! **fallback**：primary 任一阶段（下载/解密/摘要/VV/结构语义）失败才整体切 fallback；
//! 选中哪个基线就只用**它自己的** S / cursor；写入阶段失败不切 fallback（避免半库
//! 换 S），必须原样重试或人工 abort journal。
//!
//! **写库路径**：全部经 storage 公开 API（`BodyDoc`/`StructureWriter`/`baseline_apply`），
//! 不直连 SQLite；只有最终的水位推进用 `Vault::exec_transaction` 的单事务 CAS
//! （storage 没有提供 max 语义的 finish，且本任务写作用域不允许改 storage）。
//!
//! **已知边界**：relay `/bootstrap` 不返回完整签名 checkpoint 链；本模块只验证
//! `CheckpointCommit` 的生产者签名、manifest/refs 摘要、以及所有实际对象。新设备无旧
//! 水位，无法检测"有效签名的旧基线回滚"（ADR §10 已记录）；已有库用本地 pins 检测
//! `instance/log_id/epoch/control_rev/highwater` 的回退。

use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use loro::VersionVector;
use sha2::{Digest, Sha256};

use crate::checkpoint::{verify_asset_plaintext, AssetRef, KIND_BODY, KIND_STRUCTURE};
use crate::control_pin::BootstrapAnchorV1;
use crate::device::verify_signature;
use crate::engine::RELAY_CURSOR_META;
use crate::error::SyncError;
use crate::keys::RootSecretV1;
use crate::relay::{
    BootstrapBundle, BootstrapCheckpoint, ManagerChainEntry, RelayClient, RelayState,
};
use crate::rotation::ManagerTransitionV1;
use crate::upload::fetch_object;
use menghunote_storage::{
    abort_baseline_apply, apply_baseline_assets, apply_baseline_batch, asset_bytes,
    baseline_apply_state, begin_baseline_apply, load_body_doc, load_structure,
    mark_baseline_verify_failed, record_baseline_verify_time, sync_cursor_state, AffectedGuard,
    BaselineApplyStatus, BaselineAsset, BaselineExpectation, BodyDoc, Statement, StructureDoc,
    StructureWriter, Value, Vault, APP_DOC_SCHEMA, CODEC_VERSION, META_SYNC_CONFIRMED_HIGHWATER,
    META_SYNC_INTEGRATED_SEQ, META_SYNC_LOG_ID, META_SYNC_RECEIVED_SEQ, MIN_READER,
    STRUCTURE_DOC_ID,
};
use menghunote_sync_proto::{
    checkpoint_signing_input, refs_digest_input, CheckpointCommit, Envelope, EnvelopeHeader,
    ManifestDoc, ManifestV1, ObjectRefV1, ObjectType, SnapshotV1,
};

/// 每批写入的文档数（storage 的 baseline batch 是单事务；分批只为控制单事务大小）。
const BOOTSTRAP_DOC_BATCH: usize = 64;
/// bootstrap 写入 `baseline_apply.source_cursor` 的前缀：用于区分 bootstrap 的 staging
/// 与轮换（`rotation_apply` 传空串）的 staging，防止误清别人的 journal。
const JOURNAL_SOURCE_PREFIX: &str = "bootstrap:";

/// `meta` 键：bootstrap pin 的 relay instance id（16B，只允许相等）。
pub const META_BOOTSTRAP_INSTANCE: &str = "sync.pinned_instance";
/// `meta` 键：bootstrap pin 的 epoch（BE64，只允许前进）。
pub const META_BOOTSTRAP_EPOCH: &str = "sync.pinned_epoch";
/// `meta` 键：bootstrap pin 的 control_rev（BE64，只允许前进）。
pub const META_BOOTSTRAP_CONTROL_REV: &str = "sync.pinned_control_rev";
/// `meta` 键：bootstrap pin 的控制头摘要（32B，见 [`control_head_hash`]；同 rev 必须相等）。
pub const META_BOOTSTRAP_CONTROL_HASH: &str = "sync.pinned_control_hash";

/// bootstrap 结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapReport {
    /// 使用的 checkpoint id。
    pub checkpoint_id: [u8; 16],
    /// 覆盖水位 S。
    pub covered_seq: u64,
    /// 是否回退到 fallback。
    pub used_fallback: bool,
    /// 导入文档（structure 的文本为 None）。
    pub docs: Vec<(String, Option<String>)>,
    /// 导入资产数。
    pub assets: usize,
    /// S 对应的 relay 游标。
    pub cursor: String,
    /// 是否验证了 checkpoint 发布签名（relay 返回键 6/7 且验签通过）。
    pub signed_checkpoint: bool,
}

/// bootstrap 前本地已经可信的单调值（P1-4 "已有库保护"）。
///
/// 新设备全部为 `None`；已有设备由 [`BootstrapPins::from_vault`] 从本库 meta 读出，
/// 或由调用方用 identity（`pinned_instance/control_rev/current_epoch`）填充后调用
/// [`run_with_pins`]。bootstrap 只允许这些值前进/不变，绝不回退。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BootstrapPins {
    /// relay instance id（防换实例/静默回滚）。
    pub instance_id: Option<[u8; 16]>,
    /// relay log_id（restore 必须显式换新，不能静默归零）。
    pub log_id: Option<[u8; 16]>,
    /// 当前密钥代。
    pub epoch: Option<u64>,
    /// 已信任的管理控制链版本。
    pub control_rev: Option<u64>,
    /// 已信任的控制头摘要（见 [`control_head_hash`]）。
    pub control_hash: Option<[u8; 32]>,
    /// 已确认的 relay 水位。
    pub highwater: Option<u64>,
}

impl BootstrapPins {
    /// 从本库持久化的 pin（`sync.pinned_*`）与同步游标读出已有可信值。
    ///
    /// pin 缺失（新设备/首次 bootstrap）返回 `None`，不报错；pin 存在但长度非法按
    /// 本地损坏处理（不能静默忽略，否则等于绕过单调保护）。
    pub fn from_vault(vault: &Vault) -> Result<Self, SyncError> {
        let cursor = sync_cursor_state(vault)?;
        Ok(Self {
            instance_id: read_meta_array::<16>(vault, META_BOOTSTRAP_INSTANCE)?,
            log_id: cursor.log_id,
            epoch: read_meta_u64(vault, META_BOOTSTRAP_EPOCH)?,
            control_rev: read_meta_u64(vault, META_BOOTSTRAP_CONTROL_REV)?,
            control_hash: read_meta_array::<32>(vault, META_BOOTSTRAP_CONTROL_HASH)?,
            highwater: cursor.confirmed_highwater,
        })
    }

    /// 把 `sync.pinned_*` 读成一个**带自己 rev/epoch/log** 的 bootstrap 锚（ADR 0013 §2）。
    ///
    /// 语义：
    /// - `control_hash` 与 `control_rev` 必须成对：只有 hash 没有 rev 是本地损坏
    ///   （既有行为）；只有 rev 没有 hash 时返回 `None`——rev 只是下界，不构成摘要锚；
    /// - hash 存在时 `epoch` / `log_id` 必须齐全：历史视图复算需要锚**自己的**坐标，
    ///   缺任一项都不能把摘要当"同 rev 校验"用（否则旧锚会在 rev 前进后被静默忽略）。
    ///   这是 fail-closed：缺字段报 [`SyncError::LocalCorrupt`]，**不**降级成"没有锚"。
    pub fn control_anchor(&self) -> Result<Option<BootstrapAnchorV1>, SyncError> {
        let Some(hash) = self.control_hash else {
            // 只有 rev 下界（历史实例/未写摘要）不构成摘要锚；单调性仍由 control_rev 保证。
            return Ok(None);
        };
        let rev = self.control_rev.ok_or(SyncError::LocalCorrupt(
            "bootstrap pins 有 control_hash 但缺 control_rev",
        ))?;
        let epoch = self.epoch.ok_or(SyncError::LocalCorrupt(
            "bootstrap pins 有 control_hash 但缺 epoch（无法按历史视图复算摘要）",
        ))?;
        let log_id = self.log_id.ok_or(SyncError::LocalCorrupt(
            "bootstrap pins 有 control_hash 但缺 log_id（无法按历史视图复算摘要）",
        ))?;
        Ok(Some(BootstrapAnchorV1 {
            rev,
            hash,
            epoch,
            log_id,
        }))
    }
}

/// 测试用故障注入点（生产调用路径不会构造）。
///
/// 语义等价于"进程在崩溃点直接消失"：不再执行后续步骤、不清理 journal、不推进水位。
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootstrapFault {
    /// 正文批次写完后崩溃（structure / assets / 水位都未写）。
    AfterBodies,
    /// structure 文档已写入、结构投影尚未执行时崩溃。
    BeforeStructureProjection,
    /// 全部 assets 已写入、读回校验与水位推进之前崩溃。
    AfterAssets,
    /// 水位事务读取当前状态之后、提交之前，模拟另一个写者改写了 relay 游标，
    /// 迫使 CAS 失败（整笔事务回滚）。
    CursorCasConflict,
    /// 读回校验阶段注入一次「本地 VV 未覆盖快照」失败：用于验证失败标记、
    /// `begin` 拒绝续跑与 `abort` 清理路径（不改数据库里的真实数据）。
    VerifyVvNotCovered,
}

/// 故障注入伪造的 peer id（只在校验失败测试路径出现；不影响真实数据）。
const VERIFY_FAULT_PEER: u64 = 0x0BAD_5EED;

struct VerifiedDoc {
    doc_id: String,
    kind: u64,
    snapshot: SnapshotV1,
    actual_vv: VersionVector,
    /// 正文文本（structure 为 `None`）。
    text: Option<String>,
}

struct VerifiedAsset {
    object_id: [u8; 32],
    real_sha256: [u8; 32],
    byte_len: u64,
    media_type: String,
    data: Vec<u8>,
}

/// 选中的基线：manifest/docs/assets + 基线本身 + 结构语义。
struct SelectedBaseline {
    checkpoint: BootstrapCheckpoint,
    used_fallback: bool,
    docs: Vec<VerifiedDoc>,
    assets: Vec<VerifiedAsset>,
    /// structure 快照声明的 `note_id -> body_doc_id`（用于导入后的结构投影校验）。
    structure_notes: BTreeMap<String, String>,
}

/// 用已解出的 root 执行数据 bootstrap；本地 pins 从本库 meta 读取。
pub fn run<C: RelayClient>(
    vault: &Vault,
    client: &mut C,
    vault_id: [u8; 16],
    peer_id: u64,
    root: &RootSecretV1,
) -> Result<BootstrapReport, SyncError> {
    let pins = BootstrapPins::from_vault(vault)?;
    run_with_pins(vault, client, vault_id, peer_id, root, &pins)
}

/// 用调用方提供的本地 pins 执行 bootstrap（已有设备传 identity 的 pin）。
pub fn run_with_pins<C: RelayClient>(
    vault: &Vault,
    client: &mut C,
    vault_id: [u8; 16],
    peer_id: u64,
    root: &RootSecretV1,
    pins: &BootstrapPins,
) -> Result<BootstrapReport, SyncError> {
    run_inner(vault, client, vault_id, peer_id, root, pins, None)
}

/// 测试入口：在指定阶段注入崩溃/并发冲突（见 [`BootstrapFault`]）。
#[doc(hidden)]
pub fn run_with_fault<C: RelayClient>(
    vault: &Vault,
    client: &mut C,
    vault_id: [u8; 16],
    peer_id: u64,
    root: &RootSecretV1,
    fault: BootstrapFault,
) -> Result<BootstrapReport, SyncError> {
    let pins = BootstrapPins::from_vault(vault)?;
    run_inner(vault, client, vault_id, peer_id, root, &pins, Some(fault))
}

fn run_inner<C: RelayClient>(
    vault: &Vault,
    client: &mut C,
    vault_id: [u8; 16],
    peer_id: u64,
    root: &RootSecretV1,
    pins: &BootstrapPins,
    fault: Option<BootstrapFault>,
) -> Result<BootstrapReport, SyncError> {
    if root.vault_id != vault_id {
        return Err(SyncError::Object("bootstrap root 与 vault 不符"));
    }
    let bundle = client.bootstrap()?;
    let state = client.state()?;
    let chain = client.management_chain()?;
    let control_hash = control_head_hash(&state, &chain);
    validate_remote(vault_id, root, pins, &bundle, &state, &chain, &control_hash)?;

    // 选中哪个基线，就只用它的 S/cursor/manifest/docs；primary 任一阶段失败才整体切 fallback。
    // 所有下载/解密/摘要/VV/结构语义校验都在写库之前完成。
    let mut selected: Option<SelectedBaseline> = None;
    let mut last_error: Option<SyncError> = None;
    for (checkpoint, used_fallback) in [
        (bundle.primary.clone(), false),
        (bundle.fallback.clone(), true),
    ] {
        match load_baseline(
            client,
            root,
            vault_id,
            peer_id,
            &checkpoint,
            &bundle,
            &state,
        ) {
            Ok(baseline) => {
                selected = Some(SelectedBaseline {
                    used_fallback,
                    ..baseline
                });
                break;
            }
            Err(error) => last_error = Some(error),
        }
    }
    let selected = selected.ok_or_else(|| last_error.expect("bootstrap 至少尝试过一个基线"))?;

    // 实际导入（staging journal 可续跑）；失败时 journal 保留，下一次按同一基线续跑。
    let report_docs = apply_selected(vault, peer_id, &selected, fault)?;

    // 全部实际写入 + 读回校验通过后，才在一个事务里推进水位（max 语义 + CAS）。
    finalize(vault, &selected, &state, &chain, &control_hash, pins, fault)?;

    // 成功后才清 staging journal；若这里失败，重试会幂等重放并再次清理。
    let _cleared = abort_baseline_apply(vault)?;

    Ok(BootstrapReport {
        checkpoint_id: selected.checkpoint.checkpoint_id,
        covered_seq: selected.checkpoint.covered_seq,
        used_fallback: selected.used_fallback,
        docs: report_docs,
        assets: selected.assets.len(),
        cursor: selected.checkpoint.cursor,
        signed_checkpoint: true,
    })
}

/// 校验本地 durable bootstrap 锚（ADR 0013 §3；`validate_remote`/`finalize` 共用）。
///
/// 两类视图必须分开，**不能**把旧 hash 挂到 `max(rev)` 上比较：
///
/// - `anchor.rev == state.control_rev`：摘要必须逐字节等于当前视图（`epoch`/`log_id`
///   由当前 state 决定；同 rev 换 epoch/log/hash 都是 equivocation）；
/// - `anchor.rev <  state.control_rev`：**不能忽略**旧锚，也不能拿新 rev 的坐标复算；
///   按锚自己的 `rev/epoch/log` + 截断到该 rev 的管理链复算，证明当前链仍包含旧锚
///   认证过的前缀；复算不出视图 → [`SyncError::NeedsTrust`]，摘要不符 → equivocation。
fn verify_bootstrap_anchor(
    anchor: &BootstrapAnchorV1,
    state: &RelayState,
    chain: &[ManagerChainEntry],
    current_hash: &[u8; 32],
) -> Result<(), SyncError> {
    if anchor.rev > state.control_rev {
        return Err(SyncError::ControlRollback {
            expected: anchor.rev,
            actual: state.control_rev,
        });
    }
    if anchor.rev == state.control_rev {
        if anchor.hash != *current_hash {
            return Err(SyncError::Equivocation(
                "bootstrap 控制头同 rev 但 hash 变化（同 rev 历史不完整）",
            ));
        }
        return Ok(());
    }
    let computed = control_head_hash_at(state.vault_id, state.instance_id, anchor, chain).ok_or(
        SyncError::NeedsTrust("bootstrap 旧锚的 rev 不在当前管理链视图内，无法证明历史前缀"),
    )?;
    if computed != anchor.hash {
        return Err(SyncError::Equivocation(
            "bootstrap 旧锚未被当前管理链覆盖（rev 前进但前缀被重写）",
        ));
    }
    Ok(())
}

/// relay 侧与本地 pins 的写前校验：任何不满足都必须在写库前失败。
fn validate_remote(
    vault_id: [u8; 16],
    root: &RootSecretV1,
    pins: &BootstrapPins,
    bundle: &BootstrapBundle,
    state: &RelayState,
    chain: &[ManagerChainEntry],
    control_hash: &[u8; 32],
) -> Result<(), SyncError> {
    if state.vault_id != vault_id {
        return Err(SyncError::RelayWire(
            "bootstrap state.vault_id 与本地不一致",
        ));
    }
    if root.key_epoch != state.epoch {
        return Err(SyncError::Object("bootstrap root epoch 与 relay 不一致"));
    }
    if bundle.epoch != state.epoch {
        return Err(SyncError::RelayWire("bootstrap epoch 与 state 不一致"));
    }
    // bundle 先取、state 后取；head 只能前进，倒退说明 relay 换库/回滚。
    if bundle.head_seq > state.head_seq {
        return Err(SyncError::RelayWire("bootstrap/state head_seq 回退"));
    }
    if let Some(instance) = pins.instance_id {
        if instance != state.instance_id {
            return Err(SyncError::Object(
                "bootstrap relay instance 与本地 pin 不符",
            ));
        }
    }
    if let Some(log_id) = pins.log_id {
        if log_id != state.log_id {
            return Err(SyncError::Storage(
                menghunote_storage::StorageError::SyncLogMismatch {
                    expected: log_id.to_vec(),
                    found: state.log_id.to_vec(),
                },
            ));
        }
    }
    if let Some(epoch) = pins.epoch {
        if epoch > state.epoch {
            return Err(SyncError::Object("bootstrap 不允许回退 epoch"));
        }
    }
    if let Some(rev) = pins.control_rev {
        if rev > state.control_rev {
            return Err(SyncError::Object("bootstrap 不允许回退 control_rev"));
        }
        if let Some(anchor) = pins.control_anchor()? {
            verify_bootstrap_anchor(&anchor, state, chain, control_hash)?;
        }
    } else if pins.control_hash.is_some() {
        return Err(SyncError::LocalCorrupt(
            "bootstrap pins 有 control_hash 但缺 control_rev",
        ));
    }
    if let Some(highwater) = pins.highwater {
        if highwater > state.head_seq {
            return Err(SyncError::Paused(
                crate::relay::PauseReason::RollbackDetected,
            ));
        }
    }
    // 管理链必须是单调 rev 序列（内容摘要见 control_head_hash；完整链认证属于轮换阶段 A）。
    let mut previous_rev = 0u64;
    for entry in chain {
        if entry.rev <= previous_rev {
            return Err(SyncError::RelayWire("bootstrap 管理链 rev 不递增"));
        }
        previous_rev = entry.rev;
    }
    Ok(())
}

/// 控制头摘要：绑定 relay 可见的控制头坐标（instance/log/epoch/control_rev）与管理转换链。
///
/// 这是本地 pin 的摘要定义（不是 relay 内部的 `control_head` hash）：同一 `control_rev`
/// 下 relay 换 instance/log/epoch 或重写管理链都会改变摘要；rev 前进时允许摘要变化
/// （单调性由 `control_rev` 保证）。完整管理链的签名认证属于轮换阶段 A（ADR §28.2），
/// 本模块只做"同 rev 不得换内容"的回退检测。
#[must_use]
pub fn control_head_hash(state: &RelayState, chain: &[ManagerChainEntry]) -> [u8; 32] {
    control_head_hash_parts(
        state.vault_id,
        state.instance_id,
        state.log_id,
        state.epoch,
        state.control_rev,
        chain,
    )
}

/// [`control_head_hash`] 的**坐标化**输入：调用方可以显式给出历史视图的坐标
/// （ADR 0013 §3「按旧 rev/epoch/log/管理链视图验证」）。
///
/// 摘要公式与 [`control_head_hash`] 逐字节相同；`chain` 必须是**该 rev 下**的管理链
/// （即按 `entry.rev <= control_rev` 截断后的前缀）。
#[must_use]
pub fn control_head_hash_parts(
    vault_id: [u8; 16],
    instance_id: [u8; 16],
    log_id: [u8; 16],
    epoch: u64,
    control_rev: u64,
    chain: &[ManagerChainEntry],
) -> [u8; 32] {
    let mut input = Vec::new();
    input.extend_from_slice(b"menghunote/bootstrap/control-head/v1\0");
    input.extend_from_slice(&vault_id);
    input.extend_from_slice(&instance_id);
    input.extend_from_slice(&log_id);
    input.extend_from_slice(&epoch.to_be_bytes());
    input.extend_from_slice(&control_rev.to_be_bytes());
    input.extend_from_slice(&u32::try_from(chain.len()).unwrap_or(u32::MAX).to_be_bytes());
    for entry in chain {
        input.extend_from_slice(&entry.rev.to_be_bytes());
        input.extend_from_slice(&entry.pubkey);
        input.extend_from_slice(
            &u32::try_from(entry.transition.len())
                .unwrap_or(u32::MAX)
                .to_be_bytes(),
        );
        input.extend_from_slice(&entry.transition);
        push_optional_signature(&mut input, entry.old_signature);
        push_optional_signature(&mut input, entry.new_signature);
    }
    Sha256::digest(&input).into()
}

/// 按 bootstrap 锚**自己的** rev/epoch/log 视图复算控制头摘要（ADR 0013 §3）。
///
/// 为什么不能用当前 head 的坐标：摘要把 `(epoch, control_rev)` 也哈希进去，rev 前进后
/// 同一条历史管理链会算出不同摘要。历史锚只能按"它当初记录的坐标 + 当时的管理链"复算：
///
/// - `epoch` / `log_id` / `rev` 取自锚本身（本地 durable 事实，relay 不能改）；
/// - 管理链按**生效 epoch** 截断：`/control` 键 5 的 `rev` 是**管理 rev**（不是 control
///   rev），每条转换记录 `from_epoch → to_epoch`；epoch 只能由 activation 推进，因此
///   "锚当时的链" = rev1 genesis + `to_epoch <= anchor.epoch` 的转换（必须仍是链前缀）；
/// - `instance_id` 取自本地 pin/身份（换实例另有独立检查）。
///
/// 返回 `None` = 无法重建该历史视图（链不完整/非前缀/转换不可解码），调用方必须
/// fail-closed，不能把"视图不可用"当成"验证通过"。
#[must_use]
pub fn control_head_hash_at(
    vault_id: [u8; 16],
    instance_id: [u8; 16],
    anchor: &BootstrapAnchorV1,
    chain: &[ManagerChainEntry],
) -> Option<[u8; 32]> {
    if chain.first().is_none_or(|entry| entry.rev != 1) {
        return None;
    }
    // rev1 的 `transition` 是 genesis 原字节（不是 ManagerTransitionV1），不参与解码。
    let mut end = 1usize;
    for (index, entry) in chain.iter().enumerate().skip(1) {
        let transition = ManagerTransitionV1::decode(&entry.transition).ok()?;
        if transition.new_rev != entry.rev {
            return None;
        }
        if transition.to_epoch <= anchor.epoch {
            if index != end {
                // 更早的转换不属于历史视图，后面却又出现属于它的转换 = 非前缀，拒绝。
                return None;
            }
            end = index + 1;
        }
    }
    Some(control_head_hash_parts(
        vault_id,
        instance_id,
        anchor.log_id,
        anchor.epoch,
        anchor.rev,
        &chain[..end],
    ))
}

fn push_optional_signature(input: &mut Vec<u8>, signature: Option<[u8; 64]>) {
    match signature {
        None => input.push(0),
        Some(signature) => {
            input.push(1);
            input.extend_from_slice(&signature);
        }
    }
}

fn load_baseline<C: RelayClient>(
    client: &mut C,
    root: &RootSecretV1,
    vault_id: [u8; 16],
    peer_id: u64,
    checkpoint: &BootstrapCheckpoint,
    bundle: &BootstrapBundle,
    state: &RelayState,
) -> Result<SelectedBaseline, SyncError> {
    if checkpoint.epoch != state.epoch || checkpoint.epoch != bundle.epoch {
        return Err(SyncError::Object("bootstrap 基线 epoch 与 relay 不一致"));
    }
    // "≤S"：基线只能覆盖 relay 已经产生的水位；低于 GC floor 的游标已不可继续。
    if checkpoint.covered_seq > bundle.head_seq || checkpoint.covered_seq > state.head_seq {
        return Err(SyncError::Object("bootstrap 基线 S 超过 relay head"));
    }
    // 低于 GC floor 的游标已经不可继续：两次请求之间 floor 也可能前进，取较大者。
    if checkpoint.covered_seq < bundle.gc_floor || checkpoint.covered_seq < state.gc_floor {
        return Err(SyncError::Object("bootstrap 基线 S 低于 relay GC floor"));
    }
    if checkpoint.cursor.is_empty() {
        return Err(SyncError::Object("bootstrap 基线缺 relay 游标"));
    }

    let (manifest, manifest_digest) = fetch_manifest(client, root, checkpoint)?;
    verify_checkpoint_commit(client, checkpoint, &manifest, &manifest_digest)?;
    verify_manifest(&manifest, vault_id, checkpoint, state.epoch)?;
    let docs = download_docs(client, root, peer_id, &manifest)?;
    let structure_notes = verify_structure_semantics(&docs, peer_id)?;
    let assets = download_assets(client, root, &manifest)?;
    Ok(SelectedBaseline {
        checkpoint: checkpoint.clone(),
        used_fallback: false,
        docs,
        assets,
        structure_notes,
    })
}

fn verify_checkpoint_commit<C: RelayClient>(
    client: &mut C,
    checkpoint: &BootstrapCheckpoint,
    manifest: &ManifestV1,
    manifest_digest: &[u8; 32],
) -> Result<(), SyncError> {
    let signed_commit = checkpoint
        .signed_commit
        .as_deref()
        .ok_or(SyncError::Object("bootstrap 缺 CheckpointCommit"))?;
    let signature = checkpoint
        .signature
        .ok_or(SyncError::Object("bootstrap 缺 checkpoint 签名"))?;
    let commit = CheckpointCommit::decode(signed_commit)?;
    crate::checkpoint::require_current_checkpoint_validation(&commit)?;
    if commit.vault_id != manifest.vault_id
        || commit.checkpoint_id != checkpoint.checkpoint_id
        || commit.epoch != checkpoint.epoch
        || commit.covered_relay_seq != checkpoint.covered_seq
        || commit.manifest_object_id != checkpoint.manifest_oid
        || commit.manifest_digest != *manifest_digest
    {
        return Err(SyncError::Object("bootstrap checkpoint commit 与响应不符"));
    }
    // ADR §21：manifest 条目已带 key_epoch/total_plain_len/object_digest，可只凭 manifest
    // 重算 refs_digest；不再依赖 GET objects。
    let refs = manifest_refs(manifest);
    let refs_digest: [u8; 32] = Sha256::digest(refs_digest_input(&refs)?).into();
    if commit.refs_digest != refs_digest {
        return Err(SyncError::Object("bootstrap refs_digest 与 manifest 不符"));
    }
    if let Some(refs_digest) = checkpoint.refs_digest {
        if commit.refs_digest != refs_digest {
            return Err(SyncError::Object("bootstrap refs_digest 与 commit 不符"));
        }
    }
    let certificate = client
        .device_cert(&commit.producer_device_id)?
        .ok_or(SyncError::Object("bootstrap 缺 checkpoint 生产者证书"))?;
    let input = checkpoint_signing_input(&commit)?;
    verify_signature(&certificate.sign_ed25519_pub, &input, &signature)?;
    Ok(())
}

/// 先读第 0 片信封拿 part_count/total_len，再按实际片数重算 object_digest 并读全对象。
///
/// 这样多片 manifest 也能校验（`/bootstrap` 响应不直接给 manifest 的 part_count/digest，
/// 只能先看信封，再让签名 commit 绑定重算出的 digest）。
fn fetch_manifest<C: RelayClient>(
    client: &mut C,
    root: &RootSecretV1,
    checkpoint: &BootstrapCheckpoint,
) -> Result<(ManifestV1, [u8; 32]), SyncError> {
    let first = client.part(&checkpoint.manifest_oid, 0)?;
    let envelope = Envelope::decode(&first)?;
    if envelope.header.object_type != ObjectType::Manifest
        || envelope.header.object_id != checkpoint.manifest_oid
        || envelope.header.part_count == 0
    {
        return Err(SyncError::Object("bootstrap manifest 信封字段不符"));
    }
    if envelope.header.generation != 0 || envelope.header.sender_seq != 0 {
        return Err(SyncError::Object("bootstrap manifest 信封字段不符"));
    }
    let digest = object_digest_of(client, &checkpoint.manifest_oid, envelope.header.part_count)?;
    let (header, bytes) = fetch_object(
        client,
        root,
        ObjectType::Manifest,
        &checkpoint.manifest_oid,
        envelope.header.part_count,
        envelope.header.total_plaintext_len,
        &digest,
        &[],
    )?;
    if header.key_epoch != checkpoint.epoch {
        return Err(SyncError::Object(
            "bootstrap manifest epoch 与 checkpoint 不符",
        ));
    }
    Ok((ManifestV1::decode(&bytes)?, digest))
}

fn object_digest_of<C: RelayClient>(
    client: &mut C,
    object_id: &[u8; 32],
    part_count: u32,
) -> Result<[u8; 32], SyncError> {
    let mut digests = Vec::with_capacity(part_count as usize);
    for index in 0..part_count {
        let bytes = client.part(object_id, index)?;
        digests.push(Sha256::digest(&bytes).into());
    }
    let input = menghunote_sync_proto::object_digest_input(part_count, &digests)?;
    Ok(Sha256::digest(&input).into())
}

/// 由 manifest 重建 refs 集合（`refs_digest` 的唯一口径）。
///
/// `pub(crate)`：I 阶段的 checkpoint 级认证（`rotation_apply::verify_checkpoint_authorization`）
/// 必须复算 `refs_digest`，复用本函数而不是复制一份口径（ADR §21：manifest 条目已带
/// key_epoch/total_plain_len/object_digest，只凭 manifest 就能重算）。
pub(crate) fn manifest_refs(manifest: &ManifestV1) -> Vec<ObjectRefV1> {
    let mut refs = Vec::with_capacity(manifest.docs.len() + manifest.assets.len());
    refs.extend(manifest.docs.iter().map(|doc| ObjectRefV1 {
        object_id: doc.snapshot_object_id,
        object_type: ObjectType::Snapshot,
        generation: doc.generation,
        key_epoch: doc.key_epoch,
        part_count: doc.part_count,
        total_plain_len: doc.total_plain_len,
        object_digest: doc.object_digest,
    }));
    refs.extend(manifest.assets.iter().map(|asset| ObjectRefV1 {
        object_id: asset.object_id,
        object_type: ObjectType::Asset,
        generation: 0,
        key_epoch: asset.key_epoch,
        part_count: asset.part_count,
        total_plain_len: asset.total_plain_len,
        object_digest: asset.object_digest,
    }));
    refs
}

fn verify_manifest(
    manifest: &ManifestV1,
    vault_id: [u8; 16],
    checkpoint: &BootstrapCheckpoint,
    epoch: u64,
) -> Result<(), SyncError> {
    if manifest.vault_id != vault_id
        || manifest.checkpoint_id != checkpoint.checkpoint_id
        || manifest.covered_relay_seq != checkpoint.covered_seq
        || manifest.key_epoch != epoch
    {
        return Err(SyncError::Object("bootstrap manifest 与 checkpoint 不符"));
    }
    if manifest.min_reader > u32::try_from(MIN_READER).unwrap_or(u32::MAX) {
        return Err(SyncError::Object("bootstrap 基线要求更高的 reader"));
    }
    if manifest.docs.is_empty() {
        return Err(SyncError::Object("bootstrap manifest 没有文档"));
    }
    // 结构文档的 ID 是 storage 的固定常量：apply_baseline_batch 依赖它写 `kind='structure'`，
    // 允许 manifest 声明别的 ID 会让结构被当成正文落库。
    if manifest.structure_doc_id != STRUCTURE_DOC_ID {
        return Err(SyncError::Object(
            "bootstrap manifest structure_doc_id 非法",
        ));
    }
    let mut doc_ids = BTreeSet::new();
    let mut snapshot_ids = BTreeSet::new();
    let mut structure_docs = 0usize;
    for doc in &manifest.docs {
        if !doc_ids.insert(doc.doc_id.as_str()) || !snapshot_ids.insert(doc.snapshot_object_id) {
            return Err(SyncError::Object("bootstrap manifest 文档/对象重复"));
        }
        if doc.kind != KIND_BODY && doc.kind != KIND_STRUCTURE {
            return Err(SyncError::Object("bootstrap manifest 文档 kind 非法"));
        }
        if doc.key_epoch != epoch {
            return Err(SyncError::Object("bootstrap manifest 文档 epoch 不符"));
        }
        if doc.doc_id == STRUCTURE_DOC_ID {
            structure_docs += 1;
            if doc.kind != KIND_STRUCTURE {
                return Err(SyncError::Object("bootstrap 结构文档 kind 非法"));
            }
        } else if doc.kind == KIND_STRUCTURE {
            return Err(SyncError::Object("bootstrap manifest 含多余结构文档"));
        }
    }
    if structure_docs != 1 {
        return Err(SyncError::Object("bootstrap manifest 缺 structure 文档"));
    }
    let mut asset_objects = BTreeSet::new();
    let mut asset_hashes = BTreeSet::new();
    for asset in &manifest.assets {
        if !asset_objects.insert(asset.object_id) || !asset_hashes.insert(asset.real_sha256) {
            return Err(SyncError::Object("bootstrap manifest 资产重复"));
        }
        if asset.key_epoch != epoch {
            return Err(SyncError::Object("bootstrap manifest 资产 epoch 不符"));
        }
    }
    Ok(())
}

fn download_docs<C: RelayClient>(
    client: &mut C,
    root: &RootSecretV1,
    peer_id: u64,
    manifest: &ManifestV1,
) -> Result<Vec<VerifiedDoc>, SyncError> {
    let mut docs = Vec::with_capacity(manifest.docs.len());
    for doc in &manifest.docs {
        let (header, bytes) = fetch_self_described(
            client,
            root,
            ObjectType::Snapshot,
            &doc.snapshot_object_id,
            &doc.object_digest,
            &[],
        )?;
        if header.part_count != doc.part_count
            || header.generation != doc.generation
            || header.key_epoch != manifest.key_epoch
            || header.sender_seq != 0
        {
            return Err(SyncError::Object("bootstrap 快照信封字段不符"));
        }
        let snapshot = SnapshotV1::decode(&bytes)?;
        if !snapshot.full_history
            || snapshot.doc_id != doc.doc_id
            || snapshot.kind != doc.kind
            || snapshot.generation != doc.generation
            || snapshot.vv != doc.vv
            || snapshot.frontier != doc.frontier
        {
            return Err(SyncError::Object("bootstrap 快照与 manifest 不符"));
        }
        if snapshot.codec_version != CODEC_VERSION
            || snapshot.app_doc_schema != u32::try_from(APP_DOC_SCHEMA).unwrap_or(u32::MAX)
            || snapshot.min_reader > u32::try_from(MIN_READER).unwrap_or(u32::MAX)
        {
            return Err(SyncError::Object("bootstrap 快照 app schema/codec 不可读"));
        }
        // 实际 VV/frontier：独立空 LoroDoc import 后再比，不只信 manifest 声明。
        let actual_vv = verify_snapshot_history(&snapshot, doc)?;
        let text = if doc.kind == KIND_BODY {
            let body = BodyDoc::restore(
                doc.doc_id.clone(),
                peer_id,
                Some(&snapshot.loro_snapshot_bytes),
                &[],
            )?;
            Some(body.text())
        } else {
            None
        };
        docs.push(VerifiedDoc {
            doc_id: doc.doc_id.clone(),
            kind: doc.kind,
            snapshot,
            actual_vv,
            text,
        });
    }
    Ok(docs)
}

/// 把 snapshot 导入独立空 `LoroDoc`，校验 actual VV/frontier 与 manifest 声明一致。
///
/// 相等比较同时排除两种攻击：宣称了不存在的 op（虚高）与真实历史里缺 op。
fn verify_snapshot_history(
    snapshot: &SnapshotV1,
    manifest_doc: &ManifestDoc,
) -> Result<VersionVector, SyncError> {
    if !snapshot.full_history {
        return Err(SyncError::Object("bootstrap 快照不是 full-history"));
    }
    let doc = loro::LoroDoc::new();
    doc.import(&snapshot.loro_snapshot_bytes)
        .map_err(|_| SyncError::Object("bootstrap 快照 Loro 数据非法"))?;
    // shallow snapshot 的 VV/frontier 仍可能自洽，必须再问 Loro 的 shallow 状态。
    crate::checkpoint::ensure_snapshot_full_history(&doc)?;
    let actual_vv = doc.oplog_vv();
    let declared_vv = decode_vv(&manifest_doc.vv)?;
    if actual_vv != declared_vv {
        return Err(SyncError::Object(
            "bootstrap 快照实际 VV 与 manifest 声明不一致",
        ));
    }
    let actual_frontier = doc.state_frontiers();
    if manifest_doc.frontier.is_empty() {
        if actual_frontier.iter().next().is_some() {
            return Err(SyncError::Object(
                "bootstrap manifest frontier 为空但快照有实际 frontier",
            ));
        }
    } else if actual_frontier.encode() != manifest_doc.frontier {
        return Err(SyncError::Object(
            "bootstrap 快照 frontier 与 manifest 声明不一致",
        ));
    }
    Ok(actual_vv)
}

/// 结构语义校验：结构快照里的每篇笔记必须指向基线 manifest 内的正文文档。
fn verify_structure_semantics(
    docs: &[VerifiedDoc],
    peer_id: u64,
) -> Result<BTreeMap<String, String>, SyncError> {
    let structure = docs
        .iter()
        .find(|doc| doc.kind == KIND_STRUCTURE)
        .ok_or(SyncError::Object("bootstrap 缺 structure 文档"))?;
    let doc = StructureDoc::restore(peer_id, Some(&structure.snapshot.loro_snapshot_bytes), &[])?;
    let snapshot = doc.snapshot()?;
    let manifest_kinds: BTreeMap<&str, u64> = docs
        .iter()
        .map(|doc| (doc.doc_id.as_str(), doc.kind))
        .collect();
    let mut notes = BTreeMap::new();
    for node in &snapshot.nodes {
        if node.note_id.is_some() != node.body_doc_id.is_some() {
            return Err(SyncError::Object("bootstrap 结构节点笔记字段不成对"));
        }
        if !node.is_note() {
            continue;
        }
        let note_id = node.note_id.clone().unwrap_or_default();
        let body_doc_id = node.body_doc_id.clone().unwrap_or_default();
        if manifest_kinds.get(body_doc_id.as_str()) != Some(&KIND_BODY) {
            return Err(SyncError::Object(
                "bootstrap 结构引用的正文文档不在基线 manifest",
            ));
        }
        if notes.insert(note_id, body_doc_id).is_some() {
            return Err(SyncError::Object("bootstrap 结构快照 note_id 重复"));
        }
    }
    Ok(notes)
}

fn download_assets<C: RelayClient>(
    client: &mut C,
    root: &RootSecretV1,
    manifest: &ManifestV1,
) -> Result<Vec<VerifiedAsset>, SyncError> {
    let mut assets = Vec::with_capacity(manifest.assets.len());
    for manifest_asset in &manifest.assets {
        let expected = crate::checkpoint::asset_object_id(
            root,
            &manifest_asset.real_sha256,
            manifest_asset.byte_len,
            &manifest_asset.media_type,
        )?;
        if expected != manifest_asset.object_id {
            return Err(SyncError::Object("bootstrap 资产 object_id 与根不符"));
        }
        let (header, bytes) = fetch_self_described(
            client,
            root,
            ObjectType::Asset,
            &manifest_asset.object_id,
            &manifest_asset.object_digest,
            &[],
        )?;
        if header.part_count != manifest_asset.part_count || header.key_epoch != manifest.key_epoch
        {
            return Err(SyncError::Object(
                "bootstrap 资产片数/epoch 与 manifest 不符",
            ));
        }
        // ADR §31：ASSET 的 header producer 固定为 0，且 message_id 必须由 identical
        // asset header 确定性派生（跨设备/重传的密文字节才可能一致）。
        if header.producer_device_id != [0u8; 16]
            || header.generation != 0
            || header.sender_seq != 0
        {
            return Err(SyncError::Object("bootstrap 资产信封字段不符（§31）"));
        }
        if header.message_id != crate::upload::asset_message_id(root, &manifest_asset.object_id)? {
            return Err(SyncError::Object("bootstrap 资产 message_id 非确定性派生"));
        }
        let reference = AssetRef {
            real_sha256: manifest_asset.real_sha256,
            byte_len: manifest_asset.byte_len,
            media_type: manifest_asset.media_type.clone(),
            object: crate::upload::UploadedObject {
                object_id: manifest_asset.object_id,
                part_count: manifest_asset.part_count,
                total_plain_len: header.total_plaintext_len,
                object_digest: manifest_asset.object_digest,
            },
        };
        verify_asset_plaintext(&bytes, &reference)?;
        let body_start = 42 + manifest_asset.media_type.len();
        let data = bytes
            .get(body_start..)
            .ok_or(SyncError::Object("bootstrap 资产明文越界"))?
            .to_vec();
        assets.push(VerifiedAsset {
            object_id: manifest_asset.object_id,
            real_sha256: manifest_asset.real_sha256,
            byte_len: manifest_asset.byte_len,
            media_type: manifest_asset.media_type.clone(),
            data,
        });
    }
    Ok(assets)
}

/// 读第 0 片拿 header，再按 header 的 part_count/total_len 读全对象并校验摘要/解密。
fn fetch_self_described<C: RelayClient>(
    client: &mut C,
    root: &RootSecretV1,
    object_type: ObjectType,
    object_id: &[u8; 32],
    expected_digest: &[u8; 32],
    descriptor: &[u8],
) -> Result<(EnvelopeHeader, Vec<u8>), SyncError> {
    let first = client.part(object_id, 0)?;
    let envelope = Envelope::decode(&first)?;
    if envelope.header.object_type != object_type
        || envelope.header.object_id != *object_id
        || envelope.header.part_count == 0
    {
        return Err(SyncError::Object("bootstrap 对象 header 不符"));
    }
    fetch_object(
        client,
        root,
        object_type,
        object_id,
        envelope.header.part_count,
        envelope.header.total_plaintext_len,
        expected_digest,
        descriptor,
    )
}

/// 清掉"上一次 bootstrap 选了另一份基线"的 staging（数据是 CRDT merge，重放幂等）。
///
/// 轮换的 baseline apply 传空 `source_cursor`，这里用前缀区分，绝不误清轮换 journal。
fn reconcile_journal(
    vault: &Vault,
    checkpoint_id: &[u8; 16],
    baseline_s: u64,
    source_cursor: &str,
) -> Result<(), SyncError> {
    let Some(state) = baseline_apply_state(vault)? else {
        return Ok(());
    };
    if state.status != BaselineApplyStatus::Staging {
        return Ok(());
    }
    let same = state.checkpoint_id == *checkpoint_id
        && state.baseline_s == baseline_s
        && state.source_cursor == source_cursor;
    if same {
        return Ok(());
    }
    if state.source_cursor.starts_with(JOURNAL_SOURCE_PREFIX) {
        let _aborted = abort_baseline_apply(vault)?;
        return Ok(());
    }
    Err(SyncError::Storage(
        menghunote_storage::StorageError::SyncConflict {
            context: "已有非 bootstrap 的 baseline apply staging（轮换？），拒绝 bootstrap"
                .to_owned(),
        },
    ))
}

/// staging journal + 分批导入 docs/structure/assets；返回按 manifest 顺序的报告文本。
fn apply_selected(
    vault: &Vault,
    peer_id: u64,
    selected: &SelectedBaseline,
    fault: Option<BootstrapFault>,
) -> Result<Vec<(String, Option<String>)>, SyncError> {
    let checkpoint_id = selected.checkpoint.checkpoint_id;
    let baseline_s = selected.checkpoint.covered_seq;
    let source_cursor = format!("{JOURNAL_SOURCE_PREFIX}{}", selected.checkpoint.cursor);
    // 完整性基准：本 checkpoint 覆盖的全部文档与资产都必须出现在 apply 批里。
    // 集合由 begin 记录，任何"少导入一批"的续跑都不会被误当成完整
    // （rotation 的 finish 会直接 Conflict；bootstrap 的 finalize 另有逐项读回校验）。
    let expected = BaselineExpectation::new(
        selected.docs.iter().map(|doc| doc.doc_id.clone()),
        selected.assets.iter().map(|asset| asset.object_id),
    );
    reconcile_journal(vault, &checkpoint_id, baseline_s, &source_cursor)?;
    begin_baseline_apply(vault, &checkpoint_id, baseline_s, &source_cursor, &expected)?;

    // body → structure → asset（structure 投影依赖正文行已存在；与旧实现的顺序一致）。
    let bodies: Vec<&VerifiedDoc> = selected
        .docs
        .iter()
        .filter(|doc| doc.kind == KIND_BODY)
        .collect();
    for chunk in bodies.chunks(BOOTSTRAP_DOC_BATCH) {
        let batch: Vec<(String, Vec<u8>)> = chunk
            .iter()
            .map(|doc| (doc.doc_id.clone(), doc.snapshot.loro_snapshot_bytes.clone()))
            .collect();
        apply_baseline_batch(vault, &batch, &[])?;
    }
    if fault == Some(BootstrapFault::AfterBodies) {
        return Err(injected_fault());
    }

    let structure = selected
        .docs
        .iter()
        .find(|doc| doc.kind == KIND_STRUCTURE)
        .ok_or(SyncError::Object("bootstrap 缺 structure 文档"))?;
    apply_baseline_batch(
        vault,
        &[(
            structure.doc_id.clone(),
            structure.snapshot.loro_snapshot_bytes.clone(),
        )],
        &[],
    )?;
    if fault == Some(BootstrapFault::BeforeStructureProjection) {
        return Err(injected_fault());
    }
    let mut writer = StructureWriter::open(vault, peer_id)?;
    writer.commit("bootstrap")?;

    if !selected.assets.is_empty() {
        let assets: Vec<BaselineAsset> = selected
            .assets
            .iter()
            .map(|asset| BaselineAsset {
                object_id: asset.object_id,
                asset_hash: asset.real_sha256,
                byte_len: asset.byte_len,
                media_type: Some(asset.media_type.clone()),
                data: asset.data.clone(),
            })
            .collect();
        apply_baseline_assets(vault, &assets)?;
    }
    if fault == Some(BootstrapFault::AfterAssets) {
        return Err(injected_fault());
    }

    // 读回校验的耗时只作诊断指标（不参与决策）；失败也要记录。
    let verify_started = Instant::now();
    let verified = verify_imported(vault, peer_id, selected, fault);
    record_baseline_verify_time(verify_started.elapsed());
    match verified {
        Ok(report) => Ok(report),
        Err(error) => {
            // 不回滚已 merge 的 Loro/资产，也不改 journal.status（schema CHECK 只有
            // staging/completed）：只写失败标记，让下一次 begin 拒绝续跑，直到 abort。
            if let Err(mark_error) = mark_baseline_verify_failed(vault, &error.to_string()) {
                return Err(SyncError::Storage(mark_error));
            }
            Err(error)
        }
    }
}

/// 单篇文档的读回校验：导出点 VV ⊆ 当前本地 VV，且仅在无本地 extra 时比较正文。
///
/// - 方向固定为「快照/导出点 VV 必须被本地 VV 覆盖」：`actual_vv.sub_iter(local_vv)`
///   为空才通过；**不要求相等**。导出点之后本地新增的 op 是合法编辑（verify 不与编辑
///   互斥），相等比较会把它误判成失败；
/// - 只有 `local_vv ⊆ actual_vv`（本地没有快照之外的 op）时，Loro 才保证本地正文等于
///   快照正文，此时才做逐字节比较；有本地 extra 时正文必然已被合并改写，比较必然误报。
fn check_imported_doc(
    actual_vv: &VersionVector,
    local_vv: &VersionVector,
    expected_text: Option<&str>,
    actual_text: &str,
) -> Result<(), &'static str> {
    if actual_vv.sub_iter(local_vv).next().is_some() {
        return Err("bootstrap 导入后本地 VV 未覆盖快照");
    }
    if local_vv.sub_iter(actual_vv).next().is_none() {
        if let Some(expected) = expected_text {
            if expected != actual_text {
                return Err("bootstrap 后正文与快照不一致");
            }
        }
    }
    Ok(())
}

/// 读回校验：本地 VV 必须覆盖快照、正文/结构投影/资产必须与快照一致。
fn verify_imported(
    vault: &Vault,
    peer_id: u64,
    selected: &SelectedBaseline,
    fault: Option<BootstrapFault>,
) -> Result<Vec<(String, Option<String>)>, SyncError> {
    let mut body_texts: BTreeMap<String, String> = BTreeMap::new();
    for (index, doc) in selected
        .docs
        .iter()
        .filter(|doc| doc.kind == KIND_BODY)
        .enumerate()
    {
        let (body, head) = load_body_doc(vault, &doc.doc_id, peer_id, None)?;
        let local_vv = head.version_vector()?;
        // 测试故障注入：伪造一个「导出点有、本地没有」的 op，制造真实的 VV 未覆盖失败。
        let actual_vv = if fault == Some(BootstrapFault::VerifyVvNotCovered) && index == 0 {
            let mut mismatched = doc.actual_vv.clone();
            mismatched.insert(VERIFY_FAULT_PEER, 1);
            mismatched
        } else {
            doc.actual_vv.clone()
        };
        let text = body.text();
        if let Err(reason) = check_imported_doc(&actual_vv, &local_vv, doc.text.as_deref(), &text) {
            return Err(SyncError::Object(reason));
        }
        body_texts.insert(doc.doc_id.clone(), text);
    }

    // 结构重放一致性 + 基线声明的每篇笔记都已投影且正文存在。
    let (structure, _head) = load_structure(vault, peer_id)?;
    let merged = structure.snapshot()?;
    let merged_notes: BTreeMap<&str, &str> = merged
        .nodes
        .iter()
        .filter(|node| node.is_note())
        .filter_map(|node| Some((node.note_id.as_deref()?, node.body_doc_id.as_deref()?)))
        .collect();
    for (note_id, body_doc_id) in &selected.structure_notes {
        if merged_notes.get(note_id.as_str()) != Some(&body_doc_id.as_str()) {
            return Err(SyncError::Object("bootstrap 结构投影与快照不一致"));
        }
        if !body_texts.contains_key(body_doc_id) {
            return Err(SyncError::Object("bootstrap 结构引用的正文缺失"));
        }
    }

    for asset in &selected.assets {
        match asset_bytes(vault, asset.real_sha256)? {
            Some(stored) if stored == asset.data => {}
            _ => return Err(SyncError::Object("bootstrap 资产未正确落库")),
        }
    }

    Ok(selected
        .docs
        .iter()
        .map(|doc| {
            let text = if doc.kind == KIND_STRUCTURE {
                None
            } else {
                body_texts.get(&doc.doc_id).cloned()
            };
            (doc.doc_id.clone(), text)
        })
        .collect())
}

/// 原子推进水位（max 语义 + CAS）。
///
/// 只允许前进：`received/integrated = max(现有, S)`、`highwater = max(现有, pin, S)`；
/// 已有进度时保留现有 relay 游标（不把已消费的位置回退）。`instance/log_id/epoch/
/// control_rev/control_hash` 只允许前进或不变。任何一项回退都在**写事务之前**硬失败。
fn finalize(
    vault: &Vault,
    selected: &SelectedBaseline,
    state: &RelayState,
    chain: &[ManagerChainEntry],
    control_hash: &[u8; 32],
    pins: &BootstrapPins,
    fault: Option<BootstrapFault>,
) -> Result<(), SyncError> {
    let s = selected.checkpoint.covered_seq;
    let cursor_state = sync_cursor_state(vault)?;
    if let Some(log_id) = &cursor_state.log_id {
        if *log_id != state.log_id {
            return Err(SyncError::Storage(
                menghunote_storage::StorageError::SyncLogMismatch {
                    expected: log_id.to_vec(),
                    found: state.log_id.to_vec(),
                },
            ));
        }
    }
    let stored = BootstrapPins::from_vault(vault)?;
    if let Some(instance) = stored.instance_id {
        if instance != state.instance_id {
            return Err(SyncError::Object("bootstrap 不允许更换 relay instance"));
        }
    }
    if let Some(epoch) = stored.epoch {
        if epoch > state.epoch {
            return Err(SyncError::Object("bootstrap 不允许回退 epoch"));
        }
    }
    if let Some(rev) = stored.control_rev {
        if rev > state.control_rev {
            return Err(SyncError::Object("bootstrap 不允许回退 control_rev"));
        }
        // 与 `validate_remote` 同一口径：两类视图（同 rev 摘要 / 异 rev 历史锚）都验，
        // 不因 rev 前进就跳过旧摘要（ADR 0013 §3）。
        if let Some(anchor) = stored.control_anchor()? {
            verify_bootstrap_anchor(&anchor, state, chain, control_hash)?;
        }
    }
    if let Some(highwater) = pins.highwater {
        if highwater > state.head_seq {
            return Err(SyncError::Paused(
                crate::relay::PauseReason::RollbackDetected,
            ));
        }
    }

    let current_cursor = vault.get_meta(RELAY_CURSOR_META)?;
    let raw_received = vault.get_meta(META_SYNC_RECEIVED_SEQ)?;
    let raw_integrated = vault.get_meta(META_SYNC_INTEGRATED_SEQ)?;
    let raw_highwater = vault.get_meta(META_SYNC_CONFIRMED_HIGHWATER)?;

    let received_target = cursor_state.received_seq.max(s);
    let integrated_target = cursor_state.integrated_seq.max(s);
    let mut highwater_target = cursor_state.confirmed_highwater.unwrap_or(0).max(s);
    if let Some(pin) = pins.highwater {
        highwater_target = highwater_target.max(pin);
    }
    let has_progress = cursor_state.received_seq > 0
        || cursor_state.integrated_seq > 0
        || cursor_state
            .confirmed_highwater
            .is_some_and(|value| value > 0);
    if has_progress && current_cursor.is_none() {
        return Err(SyncError::LocalCorrupt("本地已有同步水位但缺 relay 游标"));
    }
    // 已有进度时保留现有游标：baseline 的 cursor 只用于"从零开始"的设备，避免回退。
    let write_cursor = !has_progress;

    let mut statements = Vec::new();
    let mut guards = Vec::new();
    if cursor_state.log_id.is_none() {
        push_meta_insert_absent(
            &mut statements,
            &mut guards,
            META_SYNC_LOG_ID,
            &state.log_id,
        );
    }
    if raw_received.is_none() || received_target != cursor_state.received_seq {
        push_meta_cas_u64(
            &mut statements,
            &mut guards,
            META_SYNC_RECEIVED_SEQ,
            cursor_state.received_seq,
            received_target,
        );
    }
    if raw_integrated.is_none() || integrated_target != cursor_state.integrated_seq {
        push_meta_cas_u64(
            &mut statements,
            &mut guards,
            META_SYNC_INTEGRATED_SEQ,
            cursor_state.integrated_seq,
            integrated_target,
        );
    }
    if raw_highwater.is_none() || cursor_state.confirmed_highwater != Some(highwater_target) {
        push_meta_cas_opt_u64(
            &mut statements,
            &mut guards,
            META_SYNC_CONFIRMED_HIGHWATER,
            cursor_state.confirmed_highwater,
            highwater_target,
        );
    }
    if write_cursor {
        push_meta_cas_opt_bytes(
            &mut statements,
            &mut guards,
            RELAY_CURSOR_META,
            current_cursor.as_deref(),
            selected.checkpoint.cursor.as_bytes(),
        );
    }
    if stored.instance_id.is_none() {
        push_meta_insert_absent(
            &mut statements,
            &mut guards,
            META_BOOTSTRAP_INSTANCE,
            &state.instance_id,
        );
    }
    if stored.epoch != Some(state.epoch) {
        push_meta_cas_opt_u64(
            &mut statements,
            &mut guards,
            META_BOOTSTRAP_EPOCH,
            stored.epoch,
            state.epoch,
        );
    }
    if stored.control_rev != Some(state.control_rev) {
        push_meta_cas_opt_u64(
            &mut statements,
            &mut guards,
            META_BOOTSTRAP_CONTROL_REV,
            stored.control_rev,
            state.control_rev,
        );
    }
    if stored.control_hash != Some(*control_hash) {
        push_meta_cas_opt_bytes(
            &mut statements,
            &mut guards,
            META_BOOTSTRAP_CONTROL_HASH,
            stored.control_hash.as_ref().map(<[u8; 32]>::as_slice),
            control_hash,
        );
    }

    // 模拟并发写者在水位事务提交前改写游标：CAS 必须失败且整笔回滚。
    if fault == Some(BootstrapFault::CursorCasConflict) {
        if !write_cursor {
            return Err(injected_fault());
        }
        vault.set_meta(RELAY_CURSOR_META, b"fault-cursor")?;
    }

    match vault.exec_transaction(&statements, &guards) {
        Ok(_) => Ok(()),
        Err(menghunote_storage::StorageError::GuardFailed { .. }) => Err(SyncError::Object(
            "bootstrap 水位 CAS 冲突（游标/水位已被并发写入），未推进",
        )),
        Err(error) => Err(error.into()),
    }
}

fn injected_fault() -> SyncError {
    SyncError::Object("bootstrap 注入故障（测试）")
}

fn push_meta_insert_absent(
    statements: &mut Vec<Statement>,
    guards: &mut Vec<AffectedGuard>,
    key: &str,
    value: &[u8],
) {
    let index = statements.len();
    statements.push(Statement::new(
        "INSERT INTO meta(key,value) SELECT ?1,?2 \
         WHERE NOT EXISTS(SELECT 1 FROM meta WHERE key=?1)",
        vec![Value::Text(key.to_owned()), Value::Blob(value.to_vec())],
    ));
    guards.push(AffectedGuard::exactly(index, 1));
}

fn push_meta_cas_u64(
    statements: &mut Vec<Statement>,
    guards: &mut Vec<AffectedGuard>,
    key: &str,
    old: u64,
    new: u64,
) {
    if old == 0 {
        // 0 与"meta 缺失"等价：插入，或 CAS 更新已存在的 0。
        let index = statements.len();
        statements.push(Statement::new(
            "INSERT INTO meta(key,value) VALUES(?1,?2) \
             ON CONFLICT(key) DO UPDATE SET value=excluded.value WHERE meta.value=?3",
            vec![
                Value::Text(key.to_owned()),
                Value::Blob(new.to_be_bytes().to_vec()),
                Value::Blob(old.to_be_bytes().to_vec()),
            ],
        ));
        guards.push(AffectedGuard::exactly(index, 1));
        return;
    }
    push_meta_cas_bytes(
        statements,
        guards,
        key,
        Some(&old.to_be_bytes()),
        &new.to_be_bytes(),
    );
}

fn push_meta_cas_opt_u64(
    statements: &mut Vec<Statement>,
    guards: &mut Vec<AffectedGuard>,
    key: &str,
    old: Option<u64>,
    new: u64,
) {
    match old {
        None => push_meta_insert_absent(statements, guards, key, &new.to_be_bytes()),
        Some(old) => push_meta_cas_u64(statements, guards, key, old, new),
    }
}

fn push_meta_cas_opt_bytes(
    statements: &mut Vec<Statement>,
    guards: &mut Vec<AffectedGuard>,
    key: &str,
    old: Option<&[u8]>,
    new: &[u8],
) {
    match old {
        None => push_meta_insert_absent(statements, guards, key, new),
        Some(old) => push_meta_cas_bytes(statements, guards, key, Some(old), new),
    }
}

fn push_meta_cas_bytes(
    statements: &mut Vec<Statement>,
    guards: &mut Vec<AffectedGuard>,
    key: &str,
    old: Option<&[u8]>,
    new: &[u8],
) {
    let Some(old) = old else {
        push_meta_insert_absent(statements, guards, key, new);
        return;
    };
    let index = statements.len();
    statements.push(Statement::new(
        "UPDATE meta SET value=?3 WHERE key=?1 AND value=?2",
        vec![
            Value::Text(key.to_owned()),
            Value::Blob(old.to_vec()),
            Value::Blob(new.to_vec()),
        ],
    ));
    guards.push(AffectedGuard::exactly(index, 1));
}

fn decode_vv(bytes: &[u8]) -> Result<VersionVector, SyncError> {
    if bytes.is_empty() {
        return Ok(VersionVector::new());
    }
    VersionVector::decode(bytes).map_err(|_| SyncError::Object("bootstrap 快照 VV 非法"))
}

fn read_meta_array<const N: usize>(vault: &Vault, key: &str) -> Result<Option<[u8; N]>, SyncError> {
    match vault.get_meta(key)? {
        None => Ok(None),
        Some(bytes) => bytes
            .try_into()
            .map(Some)
            .map_err(|_| SyncError::Object("bootstrap 本地 pin 长度非法")),
    }
}

fn read_meta_u64(vault: &Vault, key: &str) -> Result<Option<u64>, SyncError> {
    match read_meta_array::<8>(vault, key)? {
        None => Ok(None),
        Some(bytes) => Ok(Some(u64::from_be_bytes(bytes))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(key_epoch: u64, total_plain_len: u64) -> ManifestV1 {
        ManifestV1 {
            vault_id: [1; 16],
            key_epoch: 1,
            checkpoint_id: [2; 16],
            covered_relay_seq: 3,
            min_reader: 1,
            structure_doc_id: "structure".to_owned(),
            docs: vec![ManifestDoc {
                doc_id: "structure".to_owned(),
                kind: KIND_STRUCTURE,
                generation: 1,
                key_epoch,
                vv: vec![1],
                frontier: vec![2],
                snapshot_object_id: [3; 32],
                part_count: 1,
                total_plain_len,
                object_digest: [4; 32],
            }],
            assets: vec![],
        }
    }

    fn digest(value: &ManifestV1) -> [u8; 32] {
        Sha256::digest(refs_digest_input(&manifest_refs(value)).expect("refs")).into()
    }

    #[test]
    fn manifest_key_epoch_or_total_len_tamper_changes_refs_digest() {
        let base = digest(&manifest(1, 100));
        assert_ne!(
            digest(&manifest(2, 100)),
            base,
            "key_epoch 篡改必须改变 refs_digest"
        );
        assert_ne!(
            digest(&manifest(1, 101)),
            base,
            "total_plain_len 篡改必须改变 refs_digest"
        );
    }

    fn document() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let doc = loro::LoroDoc::new();
        doc.get_text("body")
            .insert(0, "hello")
            .expect("insert body text");
        doc.commit();
        (
            doc.export(loro::ExportMode::Snapshot).expect("snapshot"),
            doc.oplog_vv().encode(),
            doc.state_frontiers().encode(),
        )
    }

    fn snapshot(loro: Vec<u8>, vv: Vec<u8>, frontier: Vec<u8>) -> SnapshotV1 {
        SnapshotV1 {
            doc_id: "doc".to_owned(),
            kind: KIND_BODY,
            generation: 1,
            codec_version: CODEC_VERSION.to_owned(),
            app_doc_schema: u32::try_from(APP_DOC_SCHEMA).unwrap_or(1),
            min_reader: 1,
            full_history: true,
            vv,
            frontier,
            loro_snapshot_bytes: loro,
        }
    }

    fn manifest_doc(vv: Vec<u8>, frontier: Vec<u8>) -> ManifestDoc {
        ManifestDoc {
            doc_id: "doc".to_owned(),
            kind: KIND_BODY,
            generation: 1,
            key_epoch: 1,
            vv,
            frontier,
            snapshot_object_id: [7; 32],
            part_count: 1,
            total_plain_len: 1,
            object_digest: [8; 32],
        }
    }

    #[test]
    fn actual_vv_must_match_manifest_claim() {
        let (loro, vv, frontier) = document();
        verify_snapshot_history(
            &snapshot(loro.clone(), vv.clone(), frontier.clone()),
            &manifest_doc(vv.clone(), frontier.clone()),
        )
        .expect("真实历史必须通过");

        // manifest 宣称一个不存在的 peer 的 ops（虚高）：实际 import 必须识破。
        let mut inflated = VersionVector::decode(&vv).expect("vv");
        inflated.insert(0xDEAD, 9);
        assert!(
            verify_snapshot_history(
                &snapshot(loro.clone(), inflated.encode(), frontier.clone()),
                &manifest_doc(inflated.encode(), frontier.clone()),
            )
            .is_err(),
            "虚高 VV 必须被实际 Loro 导入拒绝"
        );

        // frontier 与 manifest 声明不一致。
        let mut wrong_frontier = VersionVector::new();
        wrong_frontier.insert(7, 1);
        assert!(
            verify_snapshot_history(
                &snapshot(loro, vv.clone(), frontier),
                &manifest_doc(vv, wrong_frontier.encode()),
            )
            .is_err(),
            "frontier 不一致必须拒绝"
        );
    }

    #[test]
    fn verify_accepts_concurrent_local_edits_after_export() {
        // 导出点（快照）VV：peer 7 有 2 个 op、peer 8 有 1 个。
        let mut exported_vv = VersionVector::new();
        exported_vv.insert(7, 2);
        exported_vv.insert(8, 1);

        // 导出点之后本地继续编辑：当前 VV 严格包含导出点，但不相等。
        let mut local_vv = exported_vv.clone();
        local_vv.insert(9, 1);
        assert_ne!(local_vv, exported_vv);
        check_imported_doc(
            &exported_vv,
            &local_vv,
            Some("导出点正文"),
            "导出点正文 + 本地新编辑",
        )
        .expect("导出点 VV ⊆ 当前 VV 时必须接受并发本地编辑（不要求相等）");

        // 没有本地 extra（VV 相等）时才逐字节比较正文：不一致必须失败。
        assert!(
            check_imported_doc(&exported_vv, &exported_vv, Some("期望正文"), "实际正文").is_err(),
            "无本地 extra 时正文不一致必须失败"
        );
        check_imported_doc(&exported_vv, &exported_vv, Some("同一正文"), "同一正文")
            .expect("无本地 extra 且正文一致必须通过");

        // 当前 VV 缺少导出点里的 op：方向不能反（不是相等/双向比较）。
        let mut missing_vv = VersionVector::new();
        missing_vv.insert(7, 2);
        assert!(
            check_imported_doc(&exported_vv, &missing_vv, Some("正文"), "正文").is_err(),
            "导出点 VV 未被本地覆盖时必须失败"
        );
    }

    #[test]
    fn full_history_is_mandatory() {
        let (loro, vv, frontier) = document();
        let mut value = snapshot(loro, vv.clone(), frontier.clone());
        value.full_history = false;
        assert!(verify_snapshot_history(&value, &manifest_doc(vv, frontier)).is_err());
    }

    #[test]
    fn structure_semantics_require_referenced_body_in_manifest() {
        let peer = 7u64;
        let structure = menghunote_storage::StructureDoc::create(peer).expect("structure");
        let seed = structure
            .create_note("标题", 1_700_000_000)
            .expect("create note");
        let bytes = structure.export_snapshot().expect("snapshot");
        let doc = VerifiedDoc {
            doc_id: STRUCTURE_DOC_ID.to_owned(),
            kind: KIND_STRUCTURE,
            snapshot: SnapshotV1 {
                doc_id: STRUCTURE_DOC_ID.to_owned(),
                kind: KIND_STRUCTURE,
                generation: 1,
                codec_version: CODEC_VERSION.to_owned(),
                app_doc_schema: 1,
                min_reader: 1,
                full_history: true,
                vv: Vec::new(),
                frontier: Vec::new(),
                loro_snapshot_bytes: bytes,
            },
            actual_vv: VersionVector::new(),
            text: None,
        };
        // 结构引用了 seed.body_doc_id，但基线 manifest 里没有它：必须拒绝。
        assert!(verify_structure_semantics(std::slice::from_ref(&doc), peer).is_err());

        // 正文在 manifest 里时通过，并给出 note_id -> body_doc_id。
        let body = VerifiedDoc {
            doc_id: seed.body_doc_id.clone(),
            kind: KIND_BODY,
            snapshot: SnapshotV1 {
                doc_id: seed.body_doc_id.clone(),
                kind: KIND_BODY,
                generation: 1,
                codec_version: CODEC_VERSION.to_owned(),
                app_doc_schema: 1,
                min_reader: 1,
                full_history: true,
                vv: Vec::new(),
                frontier: Vec::new(),
                loro_snapshot_bytes: Vec::new(),
            },
            actual_vv: VersionVector::new(),
            text: Some(String::new()),
        };
        let notes = verify_structure_semantics(&[doc, body], peer).expect("结构语义通过");
        assert_eq!(notes.get(&seed.note_id), Some(&seed.body_doc_id));
    }

    #[test]
    fn manifest_structure_doc_must_be_storage_constant() {
        let mut value = manifest(1, 100);
        value.structure_doc_id = "other".to_owned();
        let checkpoint = BootstrapCheckpoint {
            checkpoint_id: value.checkpoint_id,
            epoch: value.key_epoch,
            covered_seq: value.covered_relay_seq,
            manifest_oid: [8; 32],
            refs_digest: None,
            cursor: "c3".to_owned(),
            signed_commit: None,
            signature: None,
        };
        assert!(verify_manifest(&value, value.vault_id, &checkpoint, 1).is_err());
    }

    #[test]
    fn control_head_hash_binds_coordinates_and_chain() {
        let state = RelayState {
            vault_id: [1; 16],
            log_id: [2; 16],
            instance_id: [3; 16],
            epoch: 4,
            control_rev: 5,
            auth_rev: 1,
            head_seq: 10,
            gc_floor: 0,
            current_rotation_id: None,
        };
        let base = control_head_hash(&state, &[]);
        let mut other_instance = state.clone();
        other_instance.instance_id = [9; 16];
        assert_ne!(control_head_hash(&other_instance, &[]), base);
        let mut other_rev = state.clone();
        other_rev.control_rev = 6;
        assert_ne!(control_head_hash(&other_rev, &[]), base);
        let entry = ManagerChainEntry {
            rev: 2,
            pubkey: [7; 32],
            transition: vec![1, 2, 3],
            old_rev: 1,
            old_signature: Some([4; 64]),
            new_signature: None,
        };
        assert_ne!(control_head_hash(&state, &[entry]), base);
    }
}
