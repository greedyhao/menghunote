//! 客户端同步引擎（ADR 0006 §5/§7/§16；S7-5）。
//!
//! 四个阶段，全部**幂等、可续传、失败不推进游标**：
//!
//! 1. [`SyncEngine::encrypt_pending`]：`pending_encryption` 意图 → 组 `UpdateV1` →
//!    MNE1 UPDATE 信封（固定切片/AAD/context_hash/key_epoch）→ `store_encrypted_parts` CAS；
//! 2. [`SyncEngine::push`]：`pending_send` → 逐片幂等 PUT → `mark_sent` → 批量
//!    `POST /updates`（≤256）→ 逐项 accepted/duplicate **durable ACK**（`mark_acked` CAS）；
//! 3. [`SyncEngine::pull`]：`GET /log?after=cursor` → 校验 `ObjectCommit` 签名/object_digest →
//!    AEAD 解密 → 解码 `UpdateV1` → `store_inbox_verified`（依赖未齐也 durable）→ 推进游标；
//! 4. [`SyncEngine::integrate`]：`take_inbox_ready` → 依赖（base_vv ⊆ 本地 VV）就绪 →
//!    Loro import → 断言本地 seq = `head_seq+1` → `mark_integrated`；随后
//!    [`SyncEngine::ack`] 上报 received/integrated 游标。
//!
//! **重试语义**：加密失败/进程崩溃后重试必须复用已持久化密文（storage 的
//! `store_encrypted_parts` 对同字节幂等、对不同字节冲突）。因此一旦加密成功，重试路径
//! 只走推送，不会重新生成 nonce。
//!
//! **不推进游标的失败**：任何事件校验/解密/签名失败直接返回错误，`pull` 不写游标；
//! 回滚/log_id 变化/epoch 不匹配返回 [`SyncError::Paused`]，调用方必须停止自动同步。

use crate::asset_sync;
use crate::device::{verify_signature, DeviceSigningKey};
use crate::error::SyncError;
use crate::keys::{derive_object_key, RootSecretV1};
use crate::relay::{
    PauseReason, RelayClient, RelayLogEvent, RelayLogKind, UpdateOutcome, UpdateStatus,
    UpdateSubmission,
};
use crate::storage_view;
use crate::{DeviceId, ObjectId, VaultId};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use hmac::{Hmac, Mac};
use loro::VersionVector;
use menghunote_storage::{
    advance_integrated_prefix, append_updates, append_updates_with_integrated_marks, asset_bytes,
    count_inbox_ready, count_unreferenced_assets, list_referenced_assets, load_body_doc, load_head,
    mark_acked, mark_integrated, mark_integrated_batch, mark_sent, mark_skipped_event_received,
    outbox_parts, outbox_rows, pending_encryption, repack_required, set_confirmed_highwater,
    set_sync_log_id, store_encrypted_parts, store_inbox_verified, store_inbox_verified_batch,
    sync_cursor_state, take_inbox_ready, SyncContext, SyncEncryptedPart, SyncInboxRow,
    SyncOutboxState, UpdateRecord, Vault, VerifiedInboxRow, STRUCTURE_DOC_ID,
};
use menghunote_sync_proto::{
    cap_plain, expected_part_plain_len, object_commit_signing_input, object_digest_input,
    part_count_for, Envelope, EnvelopeHeader, ObjectCommit, ObjectType, UpdateV1,
    MAX_ASSET_MIME_BYTES, MAX_ASSET_RAW_BYTES,
};
use sha2::{Digest, Sha256};
use std::time::Instant;

/// `meta` 键：本设备持久化的 relay 游标（bootstrap 或上一次 pull 写入）。
pub const RELAY_CURSOR_META: &str = "sync.relay_cursor";

/// durable skipped-asset 记录的 `meta` 键前缀（沿用现有 `meta` 表，不新增 schema）。
///
/// 完整键：`sync.skipped_asset.<log_id base64url>.<relay_seq 20 位十进制>`。
pub(crate) const SKIPPED_ASSET_META_PREFIX: &str = "sync.skipped_asset.";

/// skipped-asset 记录值格式版本。
///
/// v1 只绑定 relay 的声明（seq/object_id/epoch/cursor），不证明资产真实存在；
/// v2 追加本机**实际下载并完成密码学验证**得到的资产事实（real_sha256/byte_len/
/// media_type/object_digest）。
///
/// v3 追加**完整 ObjectCommit 字节**：checkpoint validate 重扫日志时必须逐字节
/// 绑定 `event/commit`（而不是只认同一个 object_id 的 ASSET 事件）。v1/v2 一律
/// 拒绝——旧开发 vault 的 durable verified 不得当作新链的证据（独立修订号，
/// 禁止把旧字节改号升级）。
const SKIPPED_ASSET_MAGIC: &[u8] = b"MN/skipped-asset/3\0";
/// 记录里的日志事件类型：1 = ASSET（只允许这一种，UPDATE 必须写 inbox）。
const SKIPPED_ASSET_KIND_ASSET: u8 = 1;
/// 记录 HMAC 的 HKDF info（与设备签名密钥做域分离，不直接复用 Ed25519 种子）。
const SKIPPED_ASSET_MAC_INFO: &[u8] = b"MN/v1/skipped-asset\0";
/// 记录里允许保存的 relay 游标最大字节数（relay 游标是给 `/log` 回显的 token）。
const SKIPPED_ASSET_MAX_CURSOR: usize = 4096;
/// 记录里允许保存的 ObjectCommit 编码最大字节数（当前编码 ≤ 160B，留余量）。
const SKIPPED_ASSET_MAX_COMMIT: usize = 512;

const MESSAGE_ID_LEN: usize = 16;
/// 单批 `POST /updates` 上限（ADR §5）。
const MAX_UPDATES_PER_BATCH: usize = 256;
/// 集成批次的对象上限（ADR 0012 S2b：每批最多 32 对象）。
pub const INTEGRATE_BATCH_OBJECTS: usize = 32;
/// 集成批次的明文负载软上限（约 1 MiB；单个超大对象仍允许单独成批）。
pub const INTEGRATE_BATCH_BYTES: usize = 1024 * 1024;
/// 集成批次的选取时间软预算（50ms；到点先停手，下一批继续）。
pub const INTEGRATE_BATCH_TIME: std::time::Duration = std::time::Duration::from_millis(50);
/// 一次 integrate 最多跑多少批（防止病态库把持闸窗口拖到不可控）。
pub const INTEGRATE_MAX_BATCHES: usize = 32;
/// `pull` 批量写 inbox 的单批明文上限（只防一页大 update 在内存里堆太多；不改变语义）。
const PULL_INBOX_BATCH_BYTES: usize = 8 * 1024 * 1024;
const NONCE_LEN: usize = 24;
const HEADER_LEN: u16 = 196;
const PROTOCOL_VERSION: u16 = 1;
const CIPHER_SUITE: u8 = 1;

/// 引擎配置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EngineConfig {
    /// 每批推送上限（ADR §5：`POST /updates` ≤256）。
    pub send_batch: usize,
    /// 每页日志上限。
    pub page_limit: u32,
    /// 每次 integrate 最多处理的行数。
    pub integrate_limit: usize,
    /// `pull` 是否把一页的 verified 行用**一个事务**批量写入 inbox（默认真）。
    ///
    /// Phase 0 的实测开关，Phase 1A-2 起默认开启：整页 256 条 UPDATE 只产生一次
    /// `sync_inbox` 提交（`received` 前缀与行同一事务推进，见
    /// [`menghunote_storage::store_inbox_verified_batch`]）。结果与逐条路径等价
    /// （重复字节幂等、冲突 fail-closed、游标只在整页验收成功后推进）；差别只是
    /// durability 粒度从"条"变成"页内 flush"，以及页内提前返回（例如遇到
    /// checkpoint/control 的 `Paused(UnsupportedEvent)`）时会先 flush 已验收行再返回。
    ///
    /// 保留 `false` 分支给基准对照（Phase 0 的逐条事务路径）。
    pub inbox_batch: bool,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            send_batch: 256,
            page_limit: 256,
            integrate_limit: 256,
            inbox_batch: true,
        }
    }
}

/// 设备身份（引擎运行所需的最小集合）。
#[derive(Clone)]
pub struct EngineIdentity {
    /// 本机设备 ID。
    pub device_id: DeviceId,
    /// 设备签名私钥。
    pub signing_key: DeviceSigningKey,
    /// Loro peer id（本地 seq 分配用的 peer，永不复用 sender_seq）。
    pub peer_id: u64,
}

impl core::fmt::Debug for EngineIdentity {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("EngineIdentity")
            .field("device_id", &crate::ids::hex_of(&self.device_id))
            .field("signing_key", &"<redacted>")
            .field("peer_id", &self.peer_id)
            .finish()
    }
}

/// 推送报告。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PushReport {
    /// 逐片上传成功的对象数。
    pub uploaded: usize,
    /// durable ACK 的对象数。
    pub acked: usize,
    /// relay 逐项失败（保留 outbox 待重试）。
    pub failed: usize,
}

/// 增量资产上传报告（普通同步的"正文之后"阶段）。
///
/// 语义与顺序：
/// - 只处理 **`asset_refs` 引用得到**的本地资产（有明文）；历史孤儿跳过并计入
///   [`AssetSyncReport::orphans_skipped`]，不把整库历史资产重新上传。
/// - 已有 durable "已上传" 标记的资产直接跳过（不读字节、不发请求）。
/// - relay 上对象已完整（例如 checkpoint/轮换/另一台设备传过）时按确定性密文逐片
///   校验后只做提交/落标记，不重传分片；分片缺失时只补缺失片（已 durable 的片不重传）。
/// - 对象被其它 reservation 持有时整条资产**延后**（`deferred`），不阻塞正文/结构同步，
///   下一轮再试；relay/对象校验失败计入 `failed`，同样不阻塞本轮。
/// - **本地库损坏**（[`SyncError::LocalCorrupt`]，例如字节长度/哈希与元数据不一致）不是
///   可重试的网络失败：继续扫完其余健康资产后，把首个损坏原样上抛，见
///   [`SyncEngine::upload_pending_assets`]。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AssetSyncReport {
    /// 被正文引用、有明文的资产数（本轮扫描口径）。
    pub referenced: usize,
    /// 本轮成功上传并完成对象提交的资产数（含补齐缺失分片）。
    pub uploaded: usize,
    /// 本轮真正 PUT 的分片数（已存在/已 durable 的片不计）。
    pub parts_uploaded: usize,
    /// 本轮 PUT 的明文字节数。
    pub bytes_uploaded: u64,
    /// 命中本地 durable 标记、直接跳过的资产数。
    pub already_uploaded: usize,
    /// relay 上对象已完整（checkpoint/轮换/其他设备传过），本轮只校验+提交/落标记的数量。
    pub reused_existing: usize,
    /// 被其它 reservation 持有、需下一轮再试的资产数。
    pub deferred: usize,
    /// 单资产 relay/对象校验失败数（不阻塞正文同步，下一轮重试）。
    ///
    /// **本地库损坏不计入这里**：`LocalCorrupt` 每轮重试都不会自愈，会掩盖真正的
    /// "需要本地修复"状态，因此整轮上抛而不是伪装成可重试失败。
    pub failed: usize,
    /// 元数据不可用或空数据而跳过的资产数（不上传空资产）。
    pub skipped_invalid: usize,
    /// 有明文但没有任何正文引用的孤儿数（**不上传**；GC 不在本任务范围）。
    pub orphans_skipped: u64,
}

/// 单条资产的增量上传结果（内部用）。
enum AssetUploadOutcome {
    /// 命中 durable 标记，跳过。
    AlreadyUploaded,
    /// 本轮上传（含补齐缺失分片）并完成提交。
    Uploaded {
        parts_uploaded: usize,
        bytes_uploaded: u64,
    },
    /// relay 上对象已完整，只校验/提交/落标记。
    ReusedExisting {
        parts_uploaded: usize,
        bytes_uploaded: u64,
    },
    /// 被其它 reservation 持有，下一轮再试。
    Deferred,
    /// 元数据/明文不可用，跳空资产。
    SkippedInvalid,
}

/// 拉取报告。
///
/// 除事件计数外还带**只读性能指标**（Phase 0：先量清楚瓶颈，再决定要不要批量/快照）。
/// 所有字节/耗时字段都不含路径、密钥或明文内容，可以直接打日志。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PullReport {
    /// 本页事件数。
    pub events: usize,
    /// 成功验收并 durable 写入 inbox 的事件数。
    pub stored: usize,
    /// 已收过（`relay_seq <= received_seq`）而跳过的事件数。
    pub skipped: usize,
    /// 新游标（已持久化）。
    pub next_cursor: String,
    /// 本次成功验收的 UPDATE **明文负载**字节合计。
    ///
    /// 口径：`UpdateV1` 载荷的明文长度之和（= durable 写进 `sync_inbox.verified_payload`、
    /// 后续 integrate 要 import 的字节）。**不含**每片 196B 信封头与 16B AEAD tag，
    /// 因此它是网络拉取字节的下界；要估算 wire 体积需再加"分片数 × 212B"。
    pub update_bytes: u64,
    /// 本次 pull 实际下载 + 验证 + 落本地内容寻址表的 ASSET **明文字节**合计。
    ///
    /// 含幂等重放时对已消费 ASSET 的重新下载/校验（那是真实发生的网络与磁盘成本）。
    pub asset_bytes: u64,
    /// 本次 pull 的墙钟耗时（`Duration::as_millis` 截断成 `u64`；<1ms 记 0）。
    pub elapsed_ms: u64,
    /// 本次 pull 请求/消费的日志页数。
    ///
    /// 当前实现每次调用只拉一页，成功返回的报告恒为 `1`（空页也算 1）；字段保留给
    /// Phase 1 的多页/快照路径复用。
    pub pages: usize,
    /// 本页 verified 行是否用**一个事务**批量写入 inbox
    /// （[`EngineConfig::inbox_batch`]；默认真，`false` = 基准对照的逐条路径）。
    pub inbox_batch: bool,
}

/// 集成（Integrate）阶段的批次预算与上限（ADR 0012 S2b）。
///
/// 语义：
/// - 结构文档优先，正文按 **doc 轮转** 公平取批（不循环刷最早一批）；
/// - 每批最多 [`IntegrateConfig::batch_objects`] 个对象 / 约
///   [`IntegrateConfig::batch_bytes`] 字节 / [`IntegrateConfig::batch_time`] 的**软**预算；
/// - 到事务边界交付，**不跨批提前 ACK**：逐行路径是每个对象自己的 A 事务 +
///   `mark_integrated`；正文聚合路径是每篇文档一个 A 事务（组合 update + 批量标记），
///   整批正文结束后统一推进一次 integrated 前缀；
/// - 有进展才继续下一批；连续无进展就停，并把剩余 deferred 如实上报。
///
/// [`IntegrateConfig::aggregate_body`] 打开时正文的 `batch_objects` 语义不同：它限制
/// 的是**每批参与的文档数**（而不是行数），同一文档的所有待处理行尽量进同一批，
/// 只受 `batch_bytes` / `batch_time` 与扫描窗口约束。这样一页 256 条同一文档的 update
/// 只需一次 `load_body_doc`（旧路径是 32 行一批 = 8 次全量重放）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IntegrateConfig {
    /// 单批对象上限（1 个超大对象仍允许单独成批）。
    ///
    /// `aggregate_body = true` 时正文阶段把它解释成"单批最多多少篇文档"（结构阶段不变）。
    pub batch_objects: usize,
    /// 单批明文负载软上限（字节）。
    pub batch_bytes: usize,
    /// 单批选取时间软预算。
    pub batch_time: std::time::Duration,
    /// 一次 integrate 的批数上限（结构批 + 正文批）。
    pub max_batches: usize,
    /// 正文阶段是否按文档聚合（默认真）。
    ///
    /// - `true`：同一文档的待处理行按 `relay_seq` 升序在一批内处理；一次
    ///   `load_body_doc` + 内存逐条 import + **一条**组合 update 落盘，inbox 标记与
    ///   update 在同一 A 事务里（见 [`Self::batch_objects`] 的语义差异）；
    /// - `false`：Phase 0 的逐行路径（每条 update 一次全量重放 + 一次 A 事务），
    ///   保留给基准对照；聚合路径的多轮合并已能自愈同文档物理乱序。
    pub aggregate_body: bool,
}

impl Default for IntegrateConfig {
    fn default() -> Self {
        Self {
            batch_objects: INTEGRATE_BATCH_OBJECTS,
            batch_bytes: INTEGRATE_BATCH_BYTES,
            batch_time: INTEGRATE_BATCH_TIME,
            max_batches: INTEGRATE_MAX_BATCHES,
            aggregate_body: true,
        }
    }
}

impl IntegrateConfig {
    /// 按"本次最多集成多少个对象"折算批次（旧 `integrate(limit)` 入口的兼容语义）。
    ///
    /// `limit` 是**本次调用的对象预算**，不是单批上限：批次大小仍受 32 对象/1MiB/50ms
    /// 约束，批数按需折算（至少一批）。
    ///
    /// 注意：`aggregate_body = true`（默认）时正文按文档聚合，`batch_objects` 被解释成
    /// "每批参与的文档数"，因此一次调用可能处理**超过 `limit` 个正文对象**（同一文档
    /// 一页 256 行会一次集成完）。结构阶段仍严格按对象预算；需要严格逐对象预算的基准
    /// 对照请设 `aggregate_body = false`。
    #[must_use]
    pub fn with_object_budget(limit: usize) -> Self {
        let batch_objects = INTEGRATE_BATCH_OBJECTS.min(limit.max(1));
        let max_batches = limit
            .saturating_add(batch_objects.saturating_sub(1))
            .checked_div(batch_objects)
            .unwrap_or(usize::MAX)
            .max(1);
        Self {
            batch_objects,
            max_batches,
            ..Self::default()
        }
    }
}

/// 集成（Integrate）阶段的可见事实（ADR 0012 S2b）。
///
/// `deferred` 是**本轮结束时仍 durable verified、且落在已接收连续前缀内**的对象数：
/// 它们没有被丢弃，依赖就绪/下一次同步会继续。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IntegrateReport {
    /// 已 import 并推进本地 seq 的对象数（含"已应用过、只标记"的幂等对象）。
    pub integrated: usize,
    /// 依赖未就绪/未知文档，保持 durable verified 的对象数。
    pub deferred: usize,
    /// 其中结构文档的对象数。
    pub structure_integrated: usize,
    /// 其中正文文档的对象数。
    pub body_integrated: usize,
    /// 本轮实际跑了多少批（结构批 + 正文批）。
    pub batches: usize,
    /// 结构 head 真的前进了、并且**只做投影**重建了 body 行 / notes 视图。
    pub projected: bool,
    /// 整个 integrate 阶段的墙钟耗时（`Duration::as_millis` 截断成 `u64`）。
    ///
    /// 包含结构阶段、只投影、正文轮转与 `deferred` 计数；不含网络（pull 已单独计时）。
    pub elapsed_ms: u64,
    /// 本阶段调用 `load_body_doc`（全量重放历史）的次数。
    ///
    /// 聚合正文（[`IntegrateConfig::aggregate_body`]）下同一文档一批只算一次，且选批
    /// 预检判定"全部已覆盖/纯 deferred"的组为 0 次（只读 O(1) 的 head_vv）；结构阶段
    /// 仍是逐行路径（每行一次全量重放），因此本字段是"整个 integrate 的全量重放次数"，
    /// 供 Phase 1A 的 release 基准对比旧逐行路径（256 行 = 256 次）。
    pub doc_loads: usize,
}

/// 依赖有序集成的完整结果（`changed_heads` 供 UI 判断要不要交付/刷新）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IntegrateOutcome {
    /// 集成计数（含 deferred / 批次 / 是否投影）。
    pub report: IntegrateReport,
    /// 本轮 head **真的前进**的文档（结构 + 正文，按首次出现稳定序；去重）。
    pub changed_heads: Vec<String>,
}

/// ACK 报告。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AckReport {
    /// 已上传的本地上报 received 游标。
    pub received_seq: u64,
    /// 已上报 integrated 游标。
    pub integrated_seq: u64,
}

/// 一次完整同步报告。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncReport {
    /// 本轮新加密的对象数。
    pub encrypted: usize,
    /// 推送结果。
    pub push: PushReport,
    /// 增量资产上传结果（正文推送之后）。
    pub assets: AssetSyncReport,
    /// 拉取结果。
    pub pull: PullReport,
    /// 集成结果。
    pub integrate: IntegrateReport,
    /// ACK 结果。
    pub ack: AckReport,
}

/// durable skipped-asset 记录（评审 0003 P0-B 残余）。
///
/// `SyncEngine::pull` 消费 ASSET 事件时**不写** `sync_inbox` 行；为了让 checkpoint 的
/// `ensure_coverage_proven` 不再靠"无 inbox 行 + 在游标之前"推断该序号是 ASSET，
/// pull 在推进游标之前把"本机确实消费了这条 ASSET 事件"落成 durable 事实。
///
/// 记录值受设备签名密钥派生的 HMAC 保护：seq/object_id/log_id/kind/epoch/cursor 任一
/// 字段被改写（哪怕是本地 SQLite 里的行）都会校验失败，checkpoint 一律 fail-closed。
///
/// v3 还绑定 [`VerifiedAsset`] 的四个字段与**完整 ObjectCommit 字节**：前者来自本机
/// 真实下载全部 parts 并完成 AEAD/摘要/资产头/OID 校验的结果，不是 relay 的声明；
/// 后者让 checkpoint 重扫日志时能逐字节核对"这次看到的就是验证时那条声明"。HMAC
/// 因此证明的是"本机验证过这个真实资产与这条完整声明"。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SkippedAssetRecord {
    /// 该事件所属的 relay 日志。
    pub(crate) log_id: [u8; 16],
    /// relay 日志序号。
    pub(crate) relay_seq: u64,
    /// 日志事件里的 ASSET object_id。
    pub(crate) object_id: ObjectId,
    /// 记录类型（只允许 [`SKIPPED_ASSET_KIND_ASSET`]）。
    pub(crate) kind: u8,
    /// 事件所属 key epoch。
    pub(crate) epoch: u64,
    /// 资产数据的真实 SHA256（从已解密明文头读出并校验过）。
    pub(crate) real_sha256: [u8; 32],
    /// 资产数据的真实字节数（与明文头、密文总长一致）。
    pub(crate) byte_len: u64,
    /// 资产 MIME（canonical 头的一部分，参与 object_id 重算）。
    pub(crate) media_type: String,
    /// 已由实际 parts 重算并核对的密文对象摘要。
    pub(crate) object_digest: [u8; 32],
    /// 验证时那条日志事件的**完整** `ObjectCommit` 编码（v3 起必填）。
    pub(crate) commit: Vec<u8>,
    /// 消费该事件时的 pull 游标（`page.next_cursor`，原样保存供审计）。
    pub(crate) cursor: String,
}

/// 记录 HMAC 的密钥：`HKDF(设备签名种子, salt=vault_id, info=MN/v1/skipped-asset\0)`。
///
/// 用设备签名密钥而不是 epoch 根，是为了让跨 epoch 轮换的旧记录仍能被本机验证
/// （根每个 epoch 都会重新随机）。HKDF 做了域分离，不直接把 Ed25519 种子当 HMAC 密钥。
fn skipped_asset_mac_key(
    signing_key: &DeviceSigningKey,
    vault_id: &VaultId,
) -> Result<crate::keys::Secret32, SyncError> {
    crate::kdf::hkdf32(
        signing_key.seed().expose_slice(),
        vault_id,
        SKIPPED_ASSET_MAC_INFO,
    )
}

fn skipped_asset_mac(
    material: &[u8],
    mac_key: &crate::keys::Secret32,
) -> Result<[u8; 32], SyncError> {
    type HmacSha256 = Hmac<Sha256>;
    let mut mac = <HmacSha256 as Mac>::new_from_slice(mac_key.expose_slice())
        .map_err(|_| SyncError::LocalCorrupt("skipped-asset HMAC 密钥长度非法"))?;
    mac.update(material);
    Ok(mac.finalize().into_bytes().into())
}

impl SkippedAssetRecord {
    fn encode(&self, mac_key: &crate::keys::Secret32) -> Result<Vec<u8>, SyncError> {
        if self.relay_seq == 0 {
            return Err(SyncError::LocalCorrupt("skipped-asset relay_seq 必须 >0"));
        }
        if self.kind != SKIPPED_ASSET_KIND_ASSET {
            return Err(SyncError::LocalCorrupt("skipped-asset kind 必须是 ASSET"));
        }
        if self.epoch == 0 {
            return Err(SyncError::LocalCorrupt("skipped-asset epoch 必须 >0"));
        }
        if self.cursor.is_empty() || self.cursor.len() > SKIPPED_ASSET_MAX_CURSOR {
            return Err(SyncError::LocalCorrupt("skipped-asset 游标长度非法"));
        }
        if self.media_type.is_empty() || self.media_type.len() > MAX_ASSET_MIME_BYTES {
            return Err(SyncError::LocalCorrupt(
                "skipped-asset MIME 长度非法（必须 1..=255）",
            ));
        }
        if self.commit.is_empty() || self.commit.len() > SKIPPED_ASSET_MAX_COMMIT {
            return Err(SyncError::LocalCorrupt("skipped-asset commit 长度非法"));
        }
        let cursor_len = u32::try_from(self.cursor.len())
            .map_err(|_| SyncError::LocalCorrupt("skipped-asset 游标超长"))?;
        let commit_len = u32::try_from(self.commit.len())
            .map_err(|_| SyncError::LocalCorrupt("skipped-asset commit 超长"))?;
        let mut out = Vec::with_capacity(
            SKIPPED_ASSET_MAGIC.len()
                + 16
                + 8
                + 32
                + 1
                + 8
                + 32
                + 8
                + 2
                + self.media_type.len()
                + 32
                + 4
                + self.commit.len()
                + 4
                + self.cursor.len()
                + 32,
        );
        out.extend_from_slice(SKIPPED_ASSET_MAGIC);
        out.extend_from_slice(&self.log_id);
        out.extend_from_slice(&self.relay_seq.to_be_bytes());
        out.extend_from_slice(&self.object_id);
        out.push(self.kind);
        out.extend_from_slice(&self.epoch.to_be_bytes());
        out.extend_from_slice(&self.real_sha256);
        out.extend_from_slice(&self.byte_len.to_be_bytes());
        out.extend_from_slice(&(self.media_type.len() as u16).to_be_bytes());
        out.extend_from_slice(self.media_type.as_bytes());
        out.extend_from_slice(&self.object_digest);
        out.extend_from_slice(&commit_len.to_be_bytes());
        out.extend_from_slice(&self.commit);
        out.extend_from_slice(&cursor_len.to_be_bytes());
        out.extend_from_slice(self.cursor.as_bytes());
        let tag = skipped_asset_mac(&out, mac_key)?;
        out.extend_from_slice(&tag);
        Ok(out)
    }

    fn decode(bytes: &[u8], mac_key: &crate::keys::Secret32) -> Result<Self, SyncError> {
        // 最小长度：magic + 定长字段 + MIME ≥1B + commit ≥1B + cursor ≥1B + tag。
        let min_len = SKIPPED_ASSET_MAGIC.len()
            + 16
            + 8
            + 32
            + 1
            + 8
            + 32
            + 8
            + 2
            + 1
            + 32
            + 4
            + 1
            + 4
            + 1
            + 32;
        if bytes.len() < min_len {
            return Err(SyncError::LocalCorrupt("skipped-asset 记录过短"));
        }
        let (body, tag) = bytes.split_at(bytes.len() - 32);
        let expected = skipped_asset_mac(body, mac_key)?;
        if tag != expected {
            return Err(SyncError::LocalCorrupt(
                "skipped-asset 记录认证失败（被篡改）",
            ));
        }
        if !body.starts_with(SKIPPED_ASSET_MAGIC) {
            return Err(SyncError::LocalCorrupt("skipped-asset 记录版本非法"));
        }
        let mut input = &body[SKIPPED_ASSET_MAGIC.len()..];
        let log_id = skipped_take_array::<16>(&mut input)?;
        let relay_seq = skipped_take_u64(&mut input)?;
        let object_id = skipped_take_array::<32>(&mut input)?;
        let kind = skipped_take(&mut input, 1)?[0];
        let epoch = skipped_take_u64(&mut input)?;
        let real_sha256 = skipped_take_array::<32>(&mut input)?;
        let byte_len = skipped_take_u64(&mut input)?;
        let mime_len = usize::from(skipped_take_u16(&mut input)?);
        if !(1..=MAX_ASSET_MIME_BYTES).contains(&mime_len) {
            return Err(SyncError::LocalCorrupt(
                "skipped-asset MIME 长度非法（必须 1..=255）",
            ));
        }
        let media_type = String::from_utf8(skipped_take(&mut input, mime_len)?.to_vec())
            .map_err(|_| SyncError::LocalCorrupt("skipped-asset MIME 不是 UTF-8"))?;
        let object_digest = skipped_take_array::<32>(&mut input)?;
        let commit_len = skipped_take_u32(&mut input)? as usize;
        if commit_len == 0 || commit_len > SKIPPED_ASSET_MAX_COMMIT {
            return Err(SyncError::LocalCorrupt("skipped-asset commit 长度非法"));
        }
        let commit = skipped_take(&mut input, commit_len)?.to_vec();
        let cursor_len = skipped_take_u32(&mut input)? as usize;
        if cursor_len == 0 || cursor_len > SKIPPED_ASSET_MAX_CURSOR {
            return Err(SyncError::LocalCorrupt("skipped-asset 游标长度非法"));
        }
        let cursor = String::from_utf8(skipped_take(&mut input, cursor_len)?.to_vec())
            .map_err(|_| SyncError::LocalCorrupt("skipped-asset 游标不是 UTF-8"))?;
        if !input.is_empty() {
            return Err(SyncError::LocalCorrupt("skipped-asset 记录尾部有多余字节"));
        }
        if relay_seq == 0 || epoch == 0 || kind != SKIPPED_ASSET_KIND_ASSET {
            return Err(SyncError::LocalCorrupt("skipped-asset 记录字段非法"));
        }
        // 完整 commit 必须与记录里的身份字段自洽（防止 HMAC 记录本身字段错配）。
        let commit_decoded = ObjectCommit::decode(&commit)
            .map_err(|_| SyncError::LocalCorrupt("skipped-asset commit 无法解码"))?;
        if commit_decoded.object_id != object_id
            || commit_decoded.object_type != ObjectType::Asset
            || commit_decoded.key_epoch != epoch
            || commit_decoded.generation != 0
            || commit_decoded.sender_seq != 0
            || commit_decoded.total_len == 0
            || commit_decoded.total_len > MAX_ASSET_RAW_BYTES as u64
        {
            return Err(SyncError::LocalCorrupt(
                "skipped-asset commit 与记录声明不一致",
            ));
        }
        Ok(Self {
            log_id,
            relay_seq,
            object_id,
            kind,
            epoch,
            real_sha256,
            byte_len,
            media_type,
            object_digest,
            commit,
            cursor,
        })
    }
}

fn skipped_take<'a>(input: &mut &'a [u8], len: usize) -> Result<&'a [u8], SyncError> {
    if input.len() < len {
        return Err(SyncError::LocalCorrupt("skipped-asset 记录长度不足"));
    }
    let (head, tail) = input.split_at(len);
    *input = tail;
    Ok(head)
}

fn skipped_take_array<const N: usize>(input: &mut &[u8]) -> Result<[u8; N], SyncError> {
    let bytes = skipped_take(input, N)?;
    bytes
        .try_into()
        .map_err(|_| SyncError::LocalCorrupt("skipped-asset 字段长度非法"))
}

fn skipped_take_u16(input: &mut &[u8]) -> Result<u16, SyncError> {
    let bytes: [u8; 2] = skipped_take(input, 2)?
        .try_into()
        .map_err(|_| SyncError::LocalCorrupt("skipped-asset 字段长度非法"))?;
    Ok(u16::from_be_bytes(bytes))
}

fn skipped_take_u32(input: &mut &[u8]) -> Result<u32, SyncError> {
    let bytes: [u8; 4] = skipped_take(input, 4)?
        .try_into()
        .map_err(|_| SyncError::LocalCorrupt("skipped-asset 字段长度非法"))?;
    Ok(u32::from_be_bytes(bytes))
}

fn skipped_take_u64(input: &mut &[u8]) -> Result<u64, SyncError> {
    let bytes: [u8; 8] = skipped_take(input, 8)?
        .try_into()
        .map_err(|_| SyncError::LocalCorrupt("skipped-asset 字段长度非法"))?;
    Ok(u64::from_be_bytes(bytes))
}

/// skipped-asset 记录的 `meta` 键（seq 固定 20 位十进制，便于稳定排序/诊断）。
pub(crate) fn skipped_asset_meta_key(log_id: &[u8; 16], relay_seq: u64) -> String {
    format!(
        "{SKIPPED_ASSET_META_PREFIX}{}.{relay_seq:020}",
        crate::base64url::encode(log_id)
    )
}

/// 读取并**验证**本机为某个 `log_id` 保存的全部 skipped-asset 记录。
///
/// 任何一条记录结构/HMAC/键序号不自洽都返回错误（fail-closed）：checkpoint 不能
/// 把被改写的记录当成覆盖证据。
pub(crate) fn load_skipped_asset_records(
    vault: &Vault,
    signing_key: &DeviceSigningKey,
    vault_id: &VaultId,
    log_id: &[u8; 16],
) -> Result<Vec<SkippedAssetRecord>, SyncError> {
    let prefix = format!(
        "{SKIPPED_ASSET_META_PREFIX}{}.",
        crate::base64url::encode(log_id)
    );
    let prefix_len = i64::try_from(prefix.len())
        .map_err(|_| SyncError::LocalCorrupt("skipped-asset 前缀超长"))?;
    let rows = vault.query(
        "SELECT key,value FROM meta WHERE substr(key,1,?1)=?2 ORDER BY key",
        &[
            menghunote_storage::Value::Integer(prefix_len),
            menghunote_storage::Value::Text(prefix.clone()),
        ],
    )?;
    let mac_key = skipped_asset_mac_key(signing_key, vault_id)?;
    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        let key = match row.first() {
            Some(menghunote_storage::Value::Text(key)) => key,
            _ => return Err(SyncError::LocalCorrupt("skipped-asset 键类型非法")),
        };
        let bytes = match row.get(1) {
            Some(menghunote_storage::Value::Blob(bytes)) => bytes,
            _ => return Err(SyncError::LocalCorrupt("skipped-asset 值类型非法")),
        };
        let seq_text = key
            .strip_prefix(&prefix)
            .ok_or(SyncError::LocalCorrupt("skipped-asset 键前缀不符"))?;
        if seq_text.len() != 20 || !seq_text.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(SyncError::LocalCorrupt("skipped-asset 键序号非法"));
        }
        let seq: u64 = seq_text
            .parse()
            .map_err(|_| SyncError::LocalCorrupt("skipped-asset 键序号非法"))?;
        let record = SkippedAssetRecord::decode(bytes, &mac_key)?;
        if record.log_id != *log_id || record.relay_seq != seq {
            return Err(SyncError::LocalCorrupt(
                "skipped-asset 键与记录内容不一致（被篡改）",
            ));
        }
        out.push(record);
    }
    Ok(out)
}

/// 同步引擎。
pub struct SyncEngine<'a, C: RelayClient> {
    vault: &'a Vault,
    client: &'a mut C,
    vault_id: VaultId,
    identity: EngineIdentity,
    root: RootSecretV1,
    config: EngineConfig,
    /// 轮换阶段 A–C 冻结：禁止 push/encrypt，但 pull/integrate 仍可继续。
    rotation_frozen: bool,
}

impl<'a, C: RelayClient> core::fmt::Debug for SyncEngine<'a, C> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SyncEngine")
            .field("vault_id", &crate::ids::hex_of(&self.vault_id))
            .field("identity", &self.identity)
            .field("key_epoch", &self.root.key_epoch)
            .field("config", &self.config)
            .finish()
    }
}

/// 从持久事实重建轮换冻结屏障（不依赖 RAM）。
///
/// 满足任意一条即保持冻结：
/// - `sync.repack_required=true`（旧 epoch outbox 尚未重包）；
/// - `sync.rotation_apply_phase` 存在且不是 `applied`（`applying`/`paused`/`conflict`/
///   `needs_restart`/未知值都属于"apply 未完成"，包括 merge 后、promotion 前崩溃）；
/// - rotation journal 存在且 phase 不是明确的终态 `Completed`/`Promoted`
///   （`Frozen`…`Activated`、`Aborted`、restart 中间态都属于"激活中"）。
///
/// 读取失败一律 **fail closed**：无法证明轮换已完成时，宁可继续冻结旧 epoch 的
/// encrypt/push，也不能把旧 epoch 密文推给 relay。
fn durable_rotation_barrier(vault: &Vault) -> bool {
    match repack_required(vault) {
        Ok(true) => return true,
        Ok(false) => {}
        Err(_) => return true,
    }
    if crate::rotation_journal::apply_phase_barrier(vault) {
        return true;
    }
    match crate::rotation_journal::RotationJournal::load(vault) {
        Ok(None) => false,
        Ok(Some(journal)) => !matches!(
            journal.phase,
            crate::rotation_journal::RotationPhase::Completed
                | crate::rotation_journal::RotationPhase::Promoted
        ),
        Err(_) => true,
    }
}

impl<'a, C: RelayClient> SyncEngine<'a, C> {
    /// 默认配置构造。
    pub fn new(
        vault: &'a Vault,
        client: &'a mut C,
        identity: EngineIdentity,
        root: RootSecretV1,
    ) -> Self {
        Self::with_config(vault, client, identity, root, EngineConfig::default())
    }

    /// 指定配置构造。
    pub fn with_config(
        vault: &'a Vault,
        client: &'a mut C,
        identity: EngineIdentity,
        root: RootSecretV1,
        config: EngineConfig,
    ) -> Self {
        Self {
            vault,
            client,
            vault_id: root.vault_id,
            identity,
            root,
            config,
            // 冻结屏障必须从持久事实重建：进程重启后不能因为 RAM 归零就解冻。
            rotation_frozen: durable_rotation_barrier(vault),
        }
    }

    /// 轮换阶段 A：停止后续 push/encrypt（本地 A 事务编辑仍可继续）。
    ///
    /// 这里只置 RAM 标志；阶段 A 的持久事实由 rotation journal /
    /// `sync.repack_required` 落盘，重启后由 [`durable_rotation_barrier`] 重建。
    pub fn freeze_rotation(&mut self) {
        self.rotation_frozen = true;
    }

    /// 轮换 abort/restart 后按持久事实重新解冻。
    ///
    /// 只有 journal 明确 `Completed`/`Promoted` 且 `repack_required=false` 时才真正
    /// 解冻；否则保持冻结，避免用一次 RAM 调用绕过 durable barrier。
    pub fn thaw_rotation(&mut self) {
        self.rotation_frozen = durable_rotation_barrier(self.vault);
    }

    /// 当前是否处于轮换冻结（push/encrypt 被拒绝）。
    ///
    /// 该值在构造期由 [`durable_rotation_barrier`] 从持久事实重建，`thaw_rotation`
    /// 也只能按持久事实重新计算，不能用一次 RAM 调用绕过。
    #[must_use]
    pub fn is_rotation_frozen(&self) -> bool {
        self.rotation_frozen
    }

    /// same-epoch 启动收尾：消费 durable typed apply receipt（评审 0003 P1-C/P1-D）。
    ///
    /// 崩溃窗口：身份 rename 已提交、`sync.rotation_apply_phase='applied'` 尚未写入。
    /// 此时 relay 与本地身份已是同一 epoch，[`Self::apply_epoch_change`] 的 epoch
    /// 检测不会触发；如果没人处理 `applying`，engine 会永久冻结。
    ///
    /// 本方法只接受**可认证**的事实：receipt + 内存/磁盘身份 + authenticated pin +
    /// completed baseline journal + 重包屏障全部一致才写 `applied`；否则返回 `Paused`
    /// 并保持冻结（fail-closed）。收尾**不会**清 `sync.repack_required`。
    ///
    /// 返回 `None` 表示当前 phase 不是 `applying`（没有需要收尾的 apply）。
    pub fn reconcile_rotation_apply(
        &mut self,
        identity_path: &std::path::Path,
        identity: &crate::identity::LocalIdentityV1,
    ) -> Result<Option<crate::rotation_apply::ApplyOutcome>, SyncError> {
        let outcome =
            crate::rotation_apply::reconcile_same_epoch_apply(self.vault, identity_path, identity)?;
        if matches!(
            outcome,
            Some(crate::rotation_apply::ApplyOutcome::Applied { .. })
        ) {
            // receipt 已认证身份 epoch/pin；换根后按 durable 事实重算屏障
            // （重包未完成时仍然冻结）。
            self.set_root(RootSecretV1::new(
                self.vault_id,
                identity.current_epoch,
                identity.current_root.clone(),
            ))?;
            self.thaw_rotation();
        }
        Ok(outcome)
    }

    /// B 侧应用 epoch 变化：进入时冻结 push，只有 Applied 才换根并解冻。
    ///
    /// activation T 由 `C: RelayClient` 的 [`RelayClient::verified_activation_t`] 提供
    /// （`HttpRelayClient` 委托 `/control` 缓存复算双签；未覆盖的客户端拿默认 `None`
    /// → 一律 `Paused`），因此本方法不需要为 T 改签名就能走真实授权路径。
    ///
    /// **阶段 I 的持久屏障由 apply 入口自己落盘**（[`crate::rotation_apply::
    /// apply_epoch_change_with_client`] 在第一次写库/merge 前写 `sync.rotation_apply_phase`
    /// = `applying`，成功后转 `applied`，失败保持 `paused`/`conflict`）。本方法只负责
    /// RAM 侧的进入冻结与成功换根；**不再**用 `staging` 之类的前置值代替持久 phase。
    #[allow(clippy::too_many_arguments)]
    pub fn apply_epoch_change(
        &mut self,
        vault: &menghunote_storage::Vault,
        identity_path: &std::path::Path,
        identity: &crate::identity::LocalIdentityV1,
        checkpoint_id: [u8; 16],
        baseline_s: u64,
        baseline_cursor: &str,
        candidate_root: &RootSecretV1,
        manager_public: [u8; 32],
        new_manager_public: [u8; 32],
        new_manager_rev: u64,
        restart_reason: Option<crate::rotation_apply::RestartReason>,
    ) -> Result<crate::rotation_apply::ApplyOutcome, SyncError> {
        self.freeze_rotation();
        let outcome = crate::rotation_apply::apply_epoch_change_with_client(
            self.client,
            vault,
            identity_path,
            identity,
            checkpoint_id,
            baseline_s,
            baseline_cursor,
            candidate_root,
            manager_public,
            new_manager_public,
            new_manager_rev,
            restart_reason,
        )?;
        if matches!(outcome, crate::rotation_apply::ApplyOutcome::Applied { .. }) {
            // same-epoch receipt 收尾时身份可能已被 rename 到更高 epoch（调用方也许已
            // 没有候选根）。若传入的根落后于已认证身份，以身份文件中的当前根为准；
            // 正常 epoch change 仍用本次候选根。
            if identity.current_epoch > candidate_root.key_epoch {
                self.set_root(RootSecretV1::new(
                    self.vault_id,
                    identity.current_epoch,
                    identity.current_root.clone(),
                ))?;
            } else {
                self.set_root(candidate_root.clone())?;
            }
            self.thaw_rotation();
        }
        Ok(outcome)
    }

    /// 当前使用的 epoch 根（rotation 后由调用方替换）。
    pub fn root(&self) -> &RootSecretV1 {
        &self.root
    }

    /// 替换当前 epoch 根（rotation/解锁后）。
    pub fn set_root(&mut self, root: RootSecretV1) -> Result<(), SyncError> {
        if root.vault_id != self.vault_id {
            return Err(SyncError::Descriptor("根归属 vault 与引擎不一致"));
        }
        self.root = root;
        Ok(())
    }

    /// 阶段 1：把 `pending_encryption` 意图加密成 MNE1 parts。
    pub fn encrypt_pending(&mut self, limit: usize) -> Result<usize, SyncError> {
        if self.rotation_frozen {
            return Err(SyncError::Paused(PauseReason::RotationFrozen));
        }
        let rows = pending_encryption(self.vault, limit)?;
        let mut encrypted = 0usize;
        for row in rows {
            if row.key_epoch != self.root.key_epoch {
                return Err(SyncError::Paused(PauseReason::EpochMismatch));
            }
            let object_id = row
                .object_id
                .ok_or(SyncError::LocalCorrupt("outbox intent 缺少 object_id"))?;
            let meta = storage_view::doc_meta(self.vault, &row.doc_id)?
                .ok_or(SyncError::LocalCorrupt("outbox 指向不存在的文档"))?;
            let stored = storage_view::stored_update(self.vault, &row.doc_id, row.seq)?;
            let base_vv =
                normalize_vv(&storage_view::base_vv_at(self.vault, &row.doc_id, row.seq)?);
            let message_id = decode_message_id(&row.message_id)?;
            let update = UpdateV1 {
                doc_id: row.doc_id.clone(),
                generation: meta.generation,
                codec_version: meta.codec_version,
                app_doc_schema: meta.app_doc_schema,
                min_reader: meta.min_reader,
                update_id: stored.update_id,
                batch_id: row.batch_id.clone(),
                origin_device_id: self.identity.device_id,
                origin_seq: row.seq,
                base_vv,
                end_vv: stored.end_vv,
                end_frontier: stored.end_frontier,
                loro_update_bytes: stored.payload,
            };
            let payload = update.encode()?;
            let parts = self.encrypt_update(
                &object_id,
                &message_id,
                meta.generation,
                row.key_epoch,
                row.seq,
                &payload,
            )?;
            let digest = object_digest_of(&parts)?;
            // CAS：同 message/epoch 已存在不同字节会失败，重试因此不会换 nonce。
            store_encrypted_parts(self.vault, &row.message_id, row.key_epoch, &digest, &parts)?;
            encrypted += 1;
        }
        Ok(encrypted)
    }

    /// 阶段 2：上传 parts + 批量提交 + durable ACK。
    pub fn push(&mut self) -> Result<PushReport, SyncError> {
        if self.rotation_frozen {
            return Err(SyncError::Paused(PauseReason::RotationFrozen));
        }
        // `encrypted` = 还没上传完；`sent` = 上次已上传但未获 ACK（必须继续提交）。
        let batch = self.config.send_batch.clamp(1, MAX_UPDATES_PER_BATCH);
        let mut rows = outbox_rows(self.vault, SyncOutboxState::Encrypted, batch)?;
        rows.extend(outbox_rows(self.vault, SyncOutboxState::Sent, batch)?);
        rows.truncate(batch);
        let mut report = PushReport::default();
        let mut submissions = Vec::with_capacity(rows.len());
        let mut expected: Vec<(ObjectId, String, u64, [u8; 32])> = Vec::with_capacity(rows.len());
        for row in rows {
            if row.key_epoch != self.root.key_epoch {
                return Err(SyncError::Paused(PauseReason::EpochMismatch));
            }
            let object_id = row
                .object_id
                .ok_or(SyncError::LocalCorrupt("outbox intent 缺少 object_id"))?;
            let digest = row
                .cipher_digest
                .ok_or(SyncError::LocalCorrupt("encrypted outbox 缺 cipher_digest"))?;
            let parts = outbox_parts(self.vault, &row.message_id, row.key_epoch)?;
            if parts.is_empty() {
                return Err(SyncError::LocalCorrupt("encrypted outbox 没有 parts"));
            }
            let computed = object_digest_of_parts(&parts)?;
            if computed != digest {
                return Err(SyncError::LocalCorrupt(
                    "outbox parts 与 cipher_digest 不一致（疑似被改写）",
                ));
            }
            if row.state == SyncOutboxState::Encrypted {
                for part in &parts {
                    self.client
                        .put_part(&object_id, part.part_no, &part.envelope)?;
                }
                // 上传成功后才推进 sent；submit 失败时保持 sent，下一轮继续提交。
                if !mark_sent(self.vault, &row.message_id, row.key_epoch, &digest)? {
                    return Err(SyncError::LocalCorrupt("mark_sent CAS 失败"));
                }
                report.uploaded += 1;
            }

            let first = Envelope::decode(&parts[0].envelope)?;
            if first.header.object_type != ObjectType::Update {
                return Err(SyncError::LocalCorrupt("outbox 里不是 UPDATE 对象"));
            }
            let commit = ObjectCommit {
                vault_id: self.vault_id,
                object_id,
                message_id: first.header.message_id,
                object_type: first.header.object_type,
                generation: first.header.generation,
                key_epoch: first.header.key_epoch,
                sender_seq: first.header.sender_seq,
                part_count: first.header.part_count,
                total_len: first.header.total_plaintext_len,
                object_digest: digest,
            };
            let signed_commit = commit.encode()?;
            let signature = self
                .identity
                .signing_key
                .sign(&object_commit_signing_input(&commit)?);
            expected.push((object_id, row.message_id.clone(), row.key_epoch, digest));
            submissions.push(UpdateSubmission {
                object_id,
                commit,
                signed_commit,
                signature,
            });
        }

        if submissions.is_empty() {
            return Ok(report);
        }
        let outcomes = self.client.submit_updates(submissions)?;
        let mut by_object: std::collections::HashMap<ObjectId, UpdateOutcome> = outcomes
            .into_iter()
            .map(|outcome| (outcome.object_id, outcome))
            .collect();
        for (object_id, message_id, epoch, digest) in expected {
            match by_object.remove(&object_id) {
                Some(UpdateOutcome {
                    status: UpdateStatus::Accepted { .. } | UpdateStatus::Duplicate,
                    ..
                }) => {
                    // durable ACK：CAS 成功才删除 outbox（旧 ACK 不会删新行）。
                    if mark_acked(self.vault, &message_id, epoch, &digest)? {
                        report.acked += 1;
                    }
                }
                Some(UpdateOutcome {
                    status: UpdateStatus::Failed { .. },
                    ..
                }) => report.failed += 1,
                None => return Err(SyncError::RelayWire("updates 响应缺少对象结果")),
            }
        }
        Ok(report)
    }

    /// 阶段 2b：**增量上传被正文引用的本地资产**（正文推送之后、拉取之前）。
    ///
    /// 顺序是刻意的（ADR 0012 的两阶段路径也复用本方法）：
    /// - 正文/结构更新先 `push`，资产后置；单条资产 relay/对象校验失败只计入
    ///   [`AssetSyncReport::failed`]，**不阻塞**本轮正文交付；
    /// - 资产在 `pull` 之前完成，远端设备下一次 pull 就能同时拿到正文与 ASSET 事件；
    /// - 已有 durable 标记的资产不读字节、不发请求；relay 上已完整的对象只校验+提交，
    ///   被其它 reservation 持有的对象延后（`deferred`），下一轮重试。
    ///
    /// **本地库损坏上抛**：`upload_one_asset` 发现本地字节长度/哈希与元数据不一致、
    /// 或确定性分片事实与元数据不符时返回 [`SyncError::LocalCorrupt`]。这类错误每轮
    /// 重试都不可能自愈，必须让调用方看到（否则就是"永远重试成功、永不报错"）。
    /// 处理方式是**继续扫完其余资产**（健康资产照常上传/落标记），最后把第一个本地
    /// 损坏原样上抛；[`SyncError::Paused`]（全局暂停）与 [`SyncError::Storage`]
    /// （本地库读写失败）仍**立即**上抛，语义不变。
    ///
    /// 只处理 `asset_refs` 引用得到的资产：历史孤儿不重传（`orphans_skipped` 如实上报）。
    pub fn upload_pending_assets(&mut self) -> Result<AssetSyncReport, SyncError> {
        if self.rotation_frozen {
            return Err(SyncError::Paused(PauseReason::RotationFrozen));
        }
        let mut report = AssetSyncReport {
            orphans_skipped: count_unreferenced_assets(self.vault)?,
            ..AssetSyncReport::default()
        };
        let mut first_local_corrupt: Option<SyncError> = None;
        let assets = list_referenced_assets(self.vault)?;
        report.referenced = assets.len();
        for meta in assets {
            let (Some(byte_len), Some(media_type)) = (meta.byte_len, meta.media_type.as_deref())
            else {
                report.skipped_invalid += 1;
                continue;
            };
            if byte_len <= 0 {
                // 空资产/无长度元数据不上传（storage 的 insert_asset 已拒绝空明文，
                // 这里兜底历史占位/损坏行）。
                report.skipped_invalid += 1;
                continue;
            }
            let Ok(byte_len) = u64::try_from(byte_len) else {
                report.skipped_invalid += 1;
                continue;
            };
            match self.upload_one_asset(&meta.asset_hash, byte_len, media_type) {
                Ok(AssetUploadOutcome::AlreadyUploaded) => report.already_uploaded += 1,
                Ok(AssetUploadOutcome::ReusedExisting {
                    parts_uploaded,
                    bytes_uploaded,
                }) => {
                    report.reused_existing += 1;
                    report.parts_uploaded += parts_uploaded;
                    report.bytes_uploaded += bytes_uploaded;
                }
                Ok(AssetUploadOutcome::Uploaded {
                    parts_uploaded,
                    bytes_uploaded,
                }) => {
                    report.uploaded += 1;
                    report.parts_uploaded += parts_uploaded;
                    report.bytes_uploaded += bytes_uploaded;
                }
                Ok(AssetUploadOutcome::Deferred) => report.deferred += 1,
                Ok(AssetUploadOutcome::SkippedInvalid) => report.skipped_invalid += 1,
                // 单资产失败不阻塞正文：但全局暂停（epoch/轮换）与本地库读写失败
                // 必须上抛，否则会把"整库不可用"伪装成一次成功的同步。
                Err(error) if matches!(error, SyncError::Paused(_) | SyncError::Storage(_)) => {
                    return Err(error)
                }
                // 本地库损坏不是网络失败：先继续上传其余健康资产（不让单个损坏拖住
                // 整库），最后统一上抛。计成 `failed` 会让它每轮"重试成功"、永不报错。
                Err(error) if matches!(error, SyncError::LocalCorrupt(_)) => {
                    if first_local_corrupt.is_none() {
                        first_local_corrupt = Some(error);
                    }
                }
                // 其余（relay/传输/对象校验）沿用既有语义：单资产失败，下一轮重试。
                Err(_) => report.failed += 1,
            }
        }
        if let Some(error) = first_local_corrupt {
            return Err(error);
        }
        Ok(report)
    }

    /// 单条资产：标记 → 探测 relay → 只补缺失分片 → 提交 → durable 标记。
    fn upload_one_asset(
        &mut self,
        hash: &[u8; 32],
        byte_len: u64,
        media_type: &str,
    ) -> Result<AssetUploadOutcome, SyncError> {
        asset_sync::ensure_vault(&self.root, self.vault_id)?;
        let object = asset_sync::local_asset_object(&self.root, hash, byte_len, media_type)?;
        let total = asset_sync::asset_plain_len(byte_len, media_type)?;
        let part_count = u32::try_from(part_count_for(total, ObjectType::Asset))
            .map_err(|_| SyncError::LocalCorrupt("资产分片数超出 u32"))?;
        if let Some(fact) = asset_sync::read_uploaded(self.vault, &object.object_id)? {
            if fact.key_epoch == object.key_epoch
                && fact.part_count == part_count
                && fact.total_plain_len == total
            {
                return Ok(AssetUploadOutcome::AlreadyUploaded);
            }
        }
        let Some(data) = asset_bytes(self.vault, hash)? else {
            return Ok(AssetUploadOutcome::SkippedInvalid);
        };
        if data.is_empty() {
            return Ok(AssetUploadOutcome::SkippedInvalid);
        }
        if data.len() as u64 != byte_len {
            return Err(SyncError::LocalCorrupt("资产字节长度与元数据不一致"));
        }
        let real: [u8; 32] = Sha256::digest(&data).into();
        if real != *hash {
            return Err(SyncError::LocalCorrupt("资产字节哈希与元数据不一致"));
        }
        let plaintext = crate::checkpoint::asset_plaintext(hash, byte_len, media_type, &data)?;
        let encrypted = crate::upload::encrypt_asset_parts(
            &self.root,
            self.vault_id,
            object.object_id,
            &plaintext,
        )?;
        if encrypted.part_count != part_count || encrypted.total_plain_len != total {
            return Err(SyncError::LocalCorrupt("资产确定性分片事实与元数据不一致"));
        }

        let mut parts_uploaded = 0usize;
        let mut bytes_uploaded = 0u64;
        let reused_complete = match asset_sync::read_part(self.client, &object.object_id, 0)? {
            None => {
                // relay 上完全没有该对象：普通预算完整上传。
                for (part_no, envelope, _) in &encrypted.parts {
                    self.client
                        .put_part(&object.object_id, *part_no, envelope)?;
                    parts_uploaded += 1;
                    bytes_uploaded += expected_part_plain_len(
                        ObjectType::Asset,
                        encrypted.total_plain_len,
                        *part_no,
                    )? as u64;
                }
                false
            }
            Some(existing) => {
                // 确定性加密下，同一 object_id 的分片字节必须一致；不一致只能是
                // 本地根/代码版本变化或 relay 被篡改，必须 fail-closed（本轮不计成功）。
                if existing != encrypted.parts[0].1 {
                    return Err(SyncError::Object("资产对象已有密文与本地确定性密文不一致"));
                }
                let mut missing = Vec::new();
                for (part_no, envelope, _) in encrypted.parts.iter().skip(1) {
                    match asset_sync::read_part(self.client, &object.object_id, *part_no)? {
                        Some(stored) => {
                            if stored != *envelope {
                                return Err(SyncError::Object(
                                    "资产对象已有分片与本地确定性密文不一致",
                                ));
                            }
                        }
                        None => missing.push((*part_no, envelope)),
                    }
                }
                if missing.is_empty() {
                    // checkpoint/轮换/其他设备已完整上传：只做提交（幂等）并落标记。
                    true
                } else {
                    // 上次普通上传中断：只补缺失片，已 durable 的片不重传。
                    for (part_no, envelope) in missing {
                        match self.client.put_part(&object.object_id, part_no, envelope) {
                            Ok(()) => {
                                parts_uploaded += 1;
                                bytes_uploaded += expected_part_plain_len(
                                    ObjectType::Asset,
                                    encrypted.total_plain_len,
                                    part_no,
                                )? as u64;
                            }
                            // 对象已被 checkpoint 的维护 reservation 持有：普通预算
                            // 不能续传，也不能改用别人的 reservation；延后到下一轮。
                            Err(error) if asset_sync::is_id_conflict(&error) => {
                                return Ok(AssetUploadOutcome::Deferred)
                            }
                            Err(error) => return Err(error),
                        }
                    }
                    false
                }
            }
        };

        crate::upload::submit_prepared_object(
            self.client,
            self.vault_id,
            &self.identity.signing_key,
            ObjectType::Asset,
            object.object_id,
            object.message_id,
            0,
            self.root.key_epoch,
            0,
            encrypted.part_count,
            encrypted.total_plain_len,
            &encrypted.object_digest,
        )?;
        let fact = asset_sync::uploaded_fact_from_object(
            &object,
            encrypted.part_count,
            encrypted.total_plain_len,
            encrypted.object_digest,
        );
        asset_sync::mark_uploaded(self.vault, &fact)?;
        if reused_complete {
            Ok(AssetUploadOutcome::ReusedExisting {
                parts_uploaded,
                bytes_uploaded,
            })
        } else {
            Ok(AssetUploadOutcome::Uploaded {
                parts_uploaded,
                bytes_uploaded,
            })
        }
    }

    /// 阶段 3：拉取一页日志并验收。
    ///
    /// 除游标/计数外还会填写 [`PullReport`] 的只读指标（事件/明文字节/资产字节/耗时/
    /// 页数/是否批量），并在成功返回前打一行脱敏 `eprintln!`（事件、字节、耗时，不含
    /// 路径/密钥/明文）。
    pub fn pull(&mut self) -> Result<PullReport, SyncError> {
        let started = Instant::now();
        let state = self.client.state()?;
        if state.vault_id != self.vault_id {
            return Err(SyncError::RelayWire("state.vault_id 与本地不一致"));
        }
        let local = sync_cursor_state(self.vault)?;
        match local.log_id {
            Some(log_id) if log_id != state.log_id => {
                return Err(SyncError::Paused(PauseReason::LogIdChanged));
            }
            None => set_sync_log_id(self.vault, &state.log_id)?,
            _ => {}
        }
        if let Some(highwater) = local.confirmed_highwater {
            if state.head_seq < highwater {
                return Err(SyncError::Paused(PauseReason::RollbackDetected));
            }
        }
        if state.gc_floor > local.received_seq {
            return Err(SyncError::Paused(PauseReason::CursorExpired));
        }
        if state.epoch != self.root.key_epoch {
            return Err(SyncError::Paused(PauseReason::EpochMismatch));
        }
        let Some(cursor) = self.read_cursor()? else {
            return Err(SyncError::Unsupported("缺少 relay 游标，必须先 bootstrap"));
        };
        let page = self.client.log(Some(&cursor), self.config.page_limit)?;
        if page.epoch != state.epoch {
            return Err(SyncError::Paused(PauseReason::EpochMismatch));
        }
        let mut report = PullReport {
            events: page.events.len(),
            pages: 1,
            ..PullReport::default()
        };
        // Phase 1A-2：`inbox_batch`（默认真）把一页的 verified 行攒起来，用一个事务写入。
        // 关键纪律：**任何提前返回都必须先 flush 已验收的行**——逐条路径在每条事件后
        // 就 durable，批量路径不能在 `Paused(UnsupportedEvent)` 之类的早退时把已验证行
        // 留在内存里丢掉。用立即执行的闭包承载事件循环，所有 `?` 早退都会落到闭包外的
        // 统一 flush（flush 也失败时保留原错误优先，只记录）。
        let mut buffered: Vec<VerifiedInboxRow> = Vec::new();
        let mut buffered_bytes = 0usize;
        let consume_result: Result<(), SyncError> = (|| {
            for event in &page.events {
                if event.relay_seq <= local.received_seq {
                    // ASSET 已推进 received 前缀后仍要核对 durable skipped 记录：relay 把
                    // 同一 seq 重写成另一条资产时必须 Paused(RelayHistoryRewritten)，不能
                    // 因为"水位已覆盖"就静默跳过（记录本身还绑定完整 commit 字节）。
                    if event.kind == RelayLogKind::Asset {
                        let bytes =
                            self.persist_skipped_asset(&state.log_id, event, &page.next_cursor)?;
                        report.asset_bytes = report.asset_bytes.saturating_add(bytes);
                    }
                    report.skipped += 1;
                    continue;
                }
                match event.kind {
                    RelayLogKind::Update => {
                        let row = self.verify_update_event(&state.log_id, event)?;
                        // 指标口径：成功验收的 Update **明文负载**字节（见 PullReport 注释）。
                        report.update_bytes = report.update_bytes.saturating_add(
                            u64::try_from(row.verified_payload.len()).unwrap_or(u64::MAX),
                        );
                        if self.config.inbox_batch {
                            buffered_bytes =
                                buffered_bytes.saturating_add(row.verified_payload.len());
                            buffered.push(row);
                            if buffered_bytes >= PULL_INBOX_BATCH_BYTES {
                                // 返回的"新插入行数"只用于区分幂等命中；`stored` 口径是
                                // "成功验收并 durable"，与逐条路径一致（重放行也算已 durable）。
                                store_inbox_verified_batch(self.vault, &buffered)?;
                                report.stored += buffered.len();
                                report.inbox_batch = true;
                                buffered.clear();
                                buffered_bytes = 0;
                            }
                        } else {
                            store_inbox_verified(
                                self.vault,
                                &row.log_id,
                                row.relay_seq,
                                &row.object_id,
                                row.epoch,
                                &row.verified_payload,
                            )?;
                            report.stored += 1;
                        }
                    }
                    // 资产事件不是文档水位事件：其密文本体已由 checkpoint refs 恢复，
                    // 新设备从 S 补齐日志时直接跳过，不阻塞后续 update。
                    //
                    // 评审 0003 P0-B 残余：**推进游标之前**先把"本机消费了这条 ASSET"
                    // 落成 durable 事实。否则 checkpoint 只能把"游标之前、无 inbox 行"
                    // 的缺口推断成 ASSET——恶意 relay 只要漏发一条 UPDATE 并推进 cursor
                    // 就能伪造覆盖。写记录前必须真正下载全部分片并完成密码学验证
                    // （见 `persist_skipped_asset`）；记录写入失败时整个 pull 返回错误，
                    // 游标不前进。
                    RelayLogKind::Asset => {
                        let bytes =
                            self.persist_skipped_asset(&state.log_id, event, &page.next_cursor)?;
                        report.asset_bytes = report.asset_bytes.saturating_add(bytes);
                        // ASSET 不写 inbox，但必须在 durable 消费后推进 received 连续前缀，
                        // 否则同一页里"ASSET 在前、UPDATE 在后"会让后续 UPDATE 永远不 ready。
                        mark_skipped_event_received(self.vault, &state.log_id, event.relay_seq)?;
                        report.skipped += 1;
                    }
                    // checkpoint/control 由 bootstrap/checkpoint 任务处理；静默跳过会
                    // 让 received 前缀出现洞，所以这里明确暂停。
                    _ => return Err(SyncError::Paused(PauseReason::UnsupportedEvent)),
                }
            }
            Ok(())
        })();
        // 先 flush 已验收行（`stored` 口径仍是"成功验收并 durable"；重放行幂等）。
        if !buffered.is_empty() {
            match store_inbox_verified_batch(self.vault, &buffered) {
                Ok(_) => {
                    report.stored += buffered.len();
                    report.inbox_batch = true;
                    buffered.clear();
                }
                Err(flush_error) => {
                    if let Err(original) = &consume_result {
                        // 原错误优先：flush 失败只记录，不掩盖触发早退的真实原因。
                        eprintln!(
                            "[sync] pull: 早退前 flush {} 条已验证 inbox 行失败（保留原错误）：\
                             {flush_error}; original={original}",
                            buffered.len()
                        );
                    } else {
                        return Err(SyncError::from(flush_error));
                    }
                }
            }
        }
        consume_result?;
        // 只有全部事件验收成功才推进游标（失败已在上面返回）。
        self.write_cursor(&page.next_cursor)?;
        report.next_cursor = page.next_cursor.clone();
        if page.query_head > local.confirmed_highwater.unwrap_or(0) {
            set_confirmed_highwater(self.vault, page.query_head)?;
        }
        report.elapsed_ms = elapsed_millis(started);
        // 脱敏日志：只有数量/字节/耗时，不含路径、密钥或明文（Phase 0 观测用）。
        eprintln!(
            "[sync] pull events={} stored={} skipped={} update_bytes={} asset_bytes={} \
             pages={} inbox_batch={} elapsed_ms={}",
            report.events,
            report.stored,
            report.skipped,
            report.update_bytes,
            report.asset_bytes,
            report.pages,
            report.inbox_batch,
            report.elapsed_ms
        );
        Ok(report)
    }

    /// 阶段 4：把 verified inbox 中依赖就绪的对象集成进本地库（旧入口）。
    ///
    /// 等价于 [`Self::integrate_dependency_ordered`] + [`IntegrateConfig::with_object_budget`]：
    /// 保留"一次调用最多处理 `limit` 个对象"的兼容语义，但内部已按 ADR 0012 S2b 的
    /// 结构→投影→正文顺序收敛（不再需要调用方再发一轮网络）。
    ///
    /// Phase 1A-2 起正文默认按文档聚合：`limit` 仍决定批数与结构对象预算，但同一文档
    /// 的待处理行会在一批内一起集成（可能超过 `limit` 行）；需要严格逐对象预算时用
    /// [`IntegrateConfig::aggregate_body`] 显式关闭聚合。
    pub fn integrate(&mut self, limit: usize) -> Result<IntegrateReport, SyncError> {
        let config = IntegrateConfig::with_object_budget(limit);
        self.integrate_dependency_ordered(config)
            .map(|outcome| outcome.report)
    }

    /// 阶段 4（ADR 0012 S2b）：**依赖有序**地把 verified inbox 集成进本地库。
    ///
    /// 顺序是硬约束：
    ///
    /// 1. 先集成 [`STRUCTURE_DOC_ID`] 的 durable inbox 行；
    /// 2. 只有结构 head **真的前进**才用 [`menghunote_storage::StructureWriter`] 做一次
    ///    **纯投影**（`commit` 在没有新 op 时只重建 `tree/notes` 与 body 行）；
    /// 3. 再按 doc 轮转公平集成正文；若仍有 deferred，在**同一次调用**内重试一批
    ///    （不要求调用方再发一轮网络）。
    ///
    /// 批预算见 [`IntegrateConfig`]；无进展就停（不循环刷最早一批），`deferred` 如实上报。
    pub fn integrate_dependency_ordered(
        &mut self,
        config: IntegrateConfig,
    ) -> Result<IntegrateOutcome, SyncError> {
        let started = Instant::now();
        // 结构行是 vault 的长期不变量；但 engine 的单元测试会用"没有结构文档"的裸库，
        // 因此这里把它当**可选**前置：没有结构行 → 不投影（结构 update 会按未知文档
        // deferred，和旧行为一致）。
        let structure_before = self.structure_head_seq()?;
        let mut outcome = IntegrateOutcome::default();
        // 扫描窗口要明显大于单批上限：否则 `(log_id, relay_seq)` 顺序会让后面的 doc
        // 永远进不了候选（公平取批就无从谈起）。
        let scan_limit = config
            .batch_objects
            .saturating_mul(16)
            .clamp(64, 1024)
            .max(1);

        // 1) 结构阶段：结构行优先，绝不因正文 backlog 饿死。
        let mut batches = 0usize;
        while batches < config.max_batches {
            let pending = self.scan_ready_rows(scan_limit)?;
            let selected = select_structure_batch(&pending, &config);
            if selected.is_empty() {
                break;
            }
            batches += 1;
            if self.integrate_selected(&pending, &selected, &mut outcome)? == 0 {
                // 结构文档行一定存在；真无进展就停（不循环刷同一批）。
                break;
            }
        }

        // 2) 投影：只有结构 head 真变才做，且**只做投影**（无新 op）。
        //    失败 → 明确错误（调用方必须保持写禁用/不假刷新）；结构 CRDT 已 durable。
        if let Some(before) = structure_before {
            let after = self.structure_head_seq()?.unwrap_or(before);
            if after != before {
                self.project_structure_only()?;
                outcome.report.projected = true;
                outcome.changed_heads.push(STRUCTURE_DOC_ID.to_owned());
            }
        }

        // 3) 正文阶段。
        if config.aggregate_body {
            // Phase 1A-2：按文档聚合。每篇文档一批只做一次 `load_body_doc`，行内按
            // relay_seq 升序 import，最后把增量合成**一条** update 落盘、同事务标记
            // inbox；整轮正文结束后统一推进一次 integrated 前缀。
            let body_result =
                self.integrate_body_aggregated(&config, scan_limit, &mut outcome, &mut batches);
            // 即使正文中途报错，也要推进已经提交的 integrated 行前缀（deferred/错误不是
            // 丢数据）；前缀推进失败不得掩盖正文的原错误。
            let prefix_result = advance_integrated_prefix(self.vault).map_err(SyncError::from);
            match (body_result, prefix_result) {
                (Ok(()), Ok(())) => {}
                (Ok(()), Err(error)) => return Err(error),
                (Err(error), Ok(())) => return Err(error),
                (Err(error), Err(prefix_error)) => {
                    eprintln!(
                        "[sync] integrate: 正文报错后推进 integrated 前缀失败（保留原错误）：\
                         {prefix_error}; original={error}"
                    );
                    return Err(error);
                }
            }
        } else {
            // 旧逐行路径（基准对照）：每条 update 一次全量重放 + 一次 A 事务 + 一次标记。
            let mut retried_without_progress = false;
            while batches < config.max_batches {
                let pending = self.scan_ready_rows(scan_limit)?;
                let selected = select_body_batch(&pending, &config, batches);
                if selected.is_empty() {
                    break;
                }
                batches += 1;
                let progressed = self.integrate_selected(&pending, &selected, &mut outcome)?;
                if progressed == 0 {
                    if retried_without_progress {
                        // 连续两批零进展：依赖确实没就绪，停止并上报剩余 deferred。
                        break;
                    }
                    // 同一次 integrate 内对剩余 deferred 重试一次（结构投影可能刚建立 body 行）。
                    retried_without_progress = true;
                } else {
                    retried_without_progress = false;
                }
            }
        }

        outcome.report.batches = batches;
        // 剩余 verified 行：依赖未就绪或被本次批数上限留下，都不是丢失。
        let remaining = count_inbox_ready(self.vault)?;
        outcome.report.deferred = usize::try_from(remaining).unwrap_or(usize::MAX);
        outcome.report.elapsed_ms = elapsed_millis(started);
        // 脱敏日志：只有计数/耗时（Phase 0 观测用）。
        eprintln!(
            "[sync] integrate integrated={} structure={} body={} deferred={} batches={} \
             projected={} doc_loads={} elapsed_ms={}",
            outcome.report.integrated,
            outcome.report.structure_integrated,
            outcome.report.body_integrated,
            outcome.report.deferred,
            outcome.report.batches,
            outcome.report.projected,
            outcome.report.doc_loads,
            outcome.report.elapsed_ms
        );
        Ok(outcome)
    }

    /// 结构文档的当前 head seq（没有结构行 = `None`）。
    fn structure_head_seq(&self) -> Result<Option<u64>, SyncError> {
        if !storage_view::doc_exists(self.vault, STRUCTURE_DOC_ID)? {
            return Ok(None);
        }
        Ok(Some(load_head(self.vault, STRUCTURE_DOC_ID)?.head_seq))
    }

    /// 扫一批「已接收连续前缀内、verified、未集成」的 inbox 行，并解码出 doc id。
    fn scan_ready_rows(&self, limit: usize) -> Result<Vec<PendingInboxRow>, SyncError> {
        let rows = take_inbox_ready(self.vault, limit)?;
        let mut pending = Vec::with_capacity(rows.len());
        for row in rows {
            let payload = row
                .verified_payload
                .as_deref()
                .ok_or(SyncError::LocalCorrupt("verified inbox 缺 payload"))?;
            let update = UpdateV1::decode(payload)?;
            pending.push(PendingInboxRow {
                doc_id: update.doc_id,
                size: payload.len(),
                row,
            });
        }
        Ok(pending)
    }

    /// 集成选中的行（逐行路径：结构阶段与 `aggregate_body = false` 的正文阶段）；
    /// 返回**真的有进展**（integrated）的对象数。
    fn integrate_selected(
        &self,
        pending: &[PendingInboxRow],
        selected: &[usize],
        outcome: &mut IntegrateOutcome,
    ) -> Result<usize, SyncError> {
        let mut progressed = 0usize;
        for &index in selected {
            let item = &pending[index];
            match self.integrate_inbox_row(&item.row, &mut outcome.report.doc_loads)? {
                InboxRowOutcome::Integrated { changed } => {
                    outcome.report.integrated += 1;
                    if item.doc_id == STRUCTURE_DOC_ID {
                        outcome.report.structure_integrated += 1;
                    } else {
                        outcome.report.body_integrated += 1;
                    }
                    if changed
                        && !outcome
                            .changed_heads
                            .iter()
                            .any(|head| head == &item.doc_id)
                    {
                        outcome.changed_heads.push(item.doc_id.clone());
                    }
                    progressed += 1;
                }
                // 依赖未就绪：保持 durable verified，最终由 `count_inbox_ready` 统一上报。
                InboxRowOutcome::Deferred => {}
            }
        }
        Ok(progressed)
    }

    /// 集成**一个** inbox 行（每个对象自己的 A 事务 + `mark_integrated`）。
    ///
    /// `HeadConflict` 是可重放的并发冲突（别的 writer 把 head 推进了）：不覆盖 head、
    /// 不丢 inbox/outbox，重读 head 后按新基准**有界重放一次**；再冲突就如实报错。
    ///
    /// `doc_loads` 累计本阶段 `load_body_doc`（全量重放）次数：逐行路径每 import 一行
    /// 就 +1（Phase 0 证明的 O(历史 × 事件) 就在这里）；聚合路径按文档 +1。
    fn integrate_inbox_row(
        &self,
        row: &SyncInboxRow,
        doc_loads: &mut usize,
    ) -> Result<InboxRowOutcome, SyncError> {
        let payload = row
            .verified_payload
            .as_deref()
            .ok_or(SyncError::LocalCorrupt("verified inbox 缺 payload"))?;
        let update = UpdateV1::decode(payload)?;
        if !storage_view::doc_exists(self.vault, &update.doc_id)? {
            return Ok(InboxRowOutcome::Deferred);
        }
        let head = load_head(self.vault, &update.doc_id)?;
        let local_vv = head.version_vector()?;
        // 本地已覆盖 end_vv → 这个 update 已经应用过（典型是自己产生的 update 从 relay
        // 回环回来，或同 ops 的重复投递）。不 import、不分配新 seq，只标记已集成。
        let end_vv = if update.end_vv.is_empty() {
            VersionVector::new()
        } else {
            VersionVector::decode(&update.end_vv)
                .map_err(|_| SyncError::Object("end_vv 解码失败"))?
        };
        if vv_includes(&local_vv, &end_vv) {
            mark_integrated(self.vault, &row.log_id, row.relay_seq)?;
            return Ok(InboxRowOutcome::Integrated { changed: false });
        }
        let base_vv = if update.base_vv.is_empty() {
            VersionVector::new()
        } else {
            VersionVector::decode(&update.base_vv)
                .map_err(|_| SyncError::Object("base_vv 解码失败"))?
        };
        if !vv_includes(&local_vv, &base_vv) {
            // 依赖未齐：保持 durable verified，等前面的对象到达。
            return Ok(InboxRowOutcome::Deferred);
        }
        let mut conflict_retries = 0usize;
        loop {
            let (doc, current) =
                load_body_doc(self.vault, &update.doc_id, self.identity.peer_id, None)?;
            *doc_loads = doc_loads.saturating_add(1);
            let before = doc.vv();
            doc.import(&update.loro_update_bytes)?;
            let pending = doc.pending_update(&before)?;
            if pending.payload.is_empty() {
                break;
            }
            match append_updates(
                self.vault,
                &update.doc_id,
                current.head_seq,
                &[UpdateRecord::new(update.batch_id.clone(), pending)],
            ) {
                Ok(head_after) => {
                    // 本地 seq 只能由 head+1 分配（禁止 sender_seq）。
                    if head_after.head_seq != current.head_seq.saturating_add(1) {
                        return Err(SyncError::LocalCorrupt("本地 seq 未按 head+1 分配"));
                    }
                    mark_integrated(self.vault, &row.log_id, row.relay_seq)?;
                    return Ok(InboxRowOutcome::Integrated { changed: true });
                }
                // ADR 0012 S2b：并发本地写入把 head 推进了 → 重读 head 后重放一次。
                Err(menghunote_storage::StorageError::HeadConflict { .. })
                    if conflict_retries == 0 =>
                {
                    conflict_retries += 1;
                    continue;
                }
                Err(error) => return Err(storage_view::map_storage_error(error)),
            }
        }
        mark_integrated(self.vault, &row.log_id, row.relay_seq)?;
        Ok(InboxRowOutcome::Integrated { changed: false })
    }

    // ---------------------------------------------------------------- 正文聚合（Phase 1A-2）

    /// 正文聚合路径的批循环。
    ///
    /// 与逐行路径的区别只在选择与交付粒度：每批最多 `batch_objects` **篇文档**
    /// （而不是 32 行），同一文档的候选行一次处理；批之间仍按 `batches` 轮转起点，
    /// 多文档时不会总从第一篇开始。选批前先用 `head_vv` 做轻量预检：纯 deferred 组
    /// 直接跳过、不占正文预算（避免大 deferred 文档饿死后面的就绪文档）。
    /// 有进展才继续，连续两批零进展就停。
    fn integrate_body_aggregated(
        &self,
        config: &IntegrateConfig,
        scan_limit: usize,
        outcome: &mut IntegrateOutcome,
        batches: &mut usize,
    ) -> Result<(), SyncError> {
        let mut retried_without_progress = false;
        while *batches < config.max_batches {
            let pending = self.scan_ready_rows(scan_limit)?;
            let selected = select_body_batch_aggregated(self.vault, &pending, config, *batches)?;
            if selected.is_empty() {
                break;
            }
            *batches += 1;
            let progressed = self.integrate_body_selected(&pending, &selected, outcome)?;
            if progressed == 0 {
                if retried_without_progress {
                    // 连续两批零进展：依赖确实没就绪，停止并上报剩余 deferred。
                    break;
                }
                // 同一次 integrate 内对剩余 deferred 重试一次（结构投影可能刚建立 body 行）。
                retried_without_progress = true;
            } else {
                retried_without_progress = false;
            }
        }
        Ok(())
    }

    /// 处理一批正文行：按 `(doc_id, log_id)` 分组；同一文档的候选行在 `pending` 里
    /// 已按 relay_seq 升序，因此每个 doc 组内部天然有序。
    fn integrate_body_selected(
        &self,
        pending: &[PendingInboxRow],
        selected: &[usize],
        outcome: &mut IntegrateOutcome,
    ) -> Result<usize, SyncError> {
        let mut groups: Vec<(&str, [u8; 16], Vec<usize>)> = Vec::new();
        for &index in selected {
            let item = &pending[index];
            match groups
                .iter_mut()
                .find(|group| group.0 == item.doc_id.as_str() && group.1 == item.row.log_id)
            {
                Some((_, _, rows)) => rows.push(index),
                None => groups.push((item.doc_id.as_str(), item.row.log_id, vec![index])),
            }
        }
        let mut progressed = 0usize;
        for (doc_id, log_id, indices) in groups {
            progressed = progressed.saturating_add(
                self.integrate_body_doc_group(pending, doc_id, &log_id, &indices, outcome)?,
            );
        }
        Ok(progressed)
    }

    /// 聚合处理**一篇文档**的候选行；返回真正标记为 integrated 的行数。
    ///
    /// 顺序与失败语义（Phase 1A-2 的硬约束 + Phase 1A engine 聚合缺陷收口）：
    ///
    /// 1. **轻量预检**（不 `load_body_doc`）：只 [`load_head`] 取 O(1) 的 `head_vv`；
    ///    - 全部行的 `end_vv` 都已被覆盖 → [`mark_integrated_batch`] 一次标记后返回，
    ///      零重放；
    ///    - 未覆盖行里没有任何一行的 `base_vv` 满足 → 纯 deferred，直接返回、不重放；
    ///    - 只有至少一行需要 import 时才 `load_body_doc`（`HeadConflict` 重试才再读）；
    /// 2. 进入内存后对组内行做**多轮合并**：每轮把已覆盖的行标记、把 `base_vv` 已满足
    ///    的行 import，剩下的留到下一轮；只要本轮有新的 import 就继续（后 import 的 op
    ///    可能刚好满足前面行的依赖 → 物理乱序自愈），没有新 import 就停。真正缺外部
    ///    依赖的行保持 verified，**不会因为中间某行 deferred 而丢弃已成功的前缀**；
    /// 3. 所有成功 import 的 ops 用**一次** `pending_update(&before)` 合成一条
    ///    `UpdateRecord`，与所有可标记行的 `state='integrated'` 标记进同一个 A 事务
    ///    （inbox 行不满足 `verified` 则整笔回滚，组内不落盘）；
    /// 4. 只有已覆盖行时用 [`mark_integrated_batch`] 一次标记；
    /// 5. 任何 import 错误都原样返回：整组不落盘、不标记，inbox 保持 verified；
    /// 6. `HeadConflict` 重读 head 后**有界重试一次**，再冲突就报错。
    ///
    /// 本函数**不推进** integrated 前缀；调用方在整批正文结束后统一推进一次。
    fn integrate_body_doc_group(
        &self,
        pending: &[PendingInboxRow],
        doc_id: &str,
        log_id: &[u8; 16],
        indices: &[usize],
        outcome: &mut IntegrateOutcome,
    ) -> Result<usize, SyncError> {
        if indices.is_empty() || !storage_view::doc_exists(self.vault, doc_id)? {
            return Ok(0);
        }
        // 1) 轻量预检：只读 O(1) 的 head_vv，不重放历史。
        let head_vv = load_head(self.vault, doc_id)?.version_vector()?;
        match body_group_readiness(pending, indices, &head_vv) {
            // 纯 deferred：整组保持 durable verified，不 load_body_doc。
            BodyGroupReadiness::Deferred => return Ok(0),
            BodyGroupReadiness::Covered => {
                let seqs = relay_seqs_of(pending, indices);
                mark_integrated_batch(self.vault, log_id, &seqs)?;
                let count = seqs.len();
                outcome.report.integrated += count;
                outcome.report.body_integrated += count;
                return Ok(count);
            }
            BodyGroupReadiness::Ready => {}
        }
        let mut conflict_retries = 0usize;
        loop {
            let (doc, current) = load_body_doc(self.vault, doc_id, self.identity.peer_id, None)?;
            outcome.report.doc_loads = outcome.report.doc_loads.saturating_add(1);
            let before = doc.vv();
            let mut integrated_seqs = Vec::with_capacity(indices.len());
            let mut imported = 0usize;
            let mut last_batch_id: Option<String> = None;
            // 2) 同文档内存多轮合并：只有本轮真的 import 了新 op 才再扫一轮。
            let mut remaining: Vec<usize> = indices.to_vec();
            while !remaining.is_empty() {
                let mut deferred: Vec<usize> = Vec::new();
                let mut round_imported = 0usize;
                for &index in &remaining {
                    let item = &pending[index];
                    let payload = item
                        .row
                        .verified_payload
                        .as_deref()
                        .ok_or(SyncError::LocalCorrupt("verified inbox 缺 payload"))?;
                    let update = UpdateV1::decode(payload)?;
                    // 先看 end_vv：本地已覆盖 = 这个 update 应用过（自己产生的回环/重复
                    // 投递，或本轮前面的 import 已把它包含的 ops 带进来），只标记。
                    let end_vv = decode_vv(&update.end_vv, "end_vv 解码失败")?;
                    if vv_includes(&doc.vv(), &end_vv) {
                        integrated_seqs.push(item.row.relay_seq);
                        continue;
                    }
                    // 依赖未齐：留到下一轮（本轮后面的 import 可能刚好满足它）。
                    let base_vv = decode_vv(&update.base_vv, "base_vv 解码失败")?;
                    if !vv_includes(&doc.vv(), &base_vv) {
                        deferred.push(index);
                        continue;
                    }
                    doc.import(&update.loro_update_bytes)?;
                    imported += 1;
                    round_imported += 1;
                    last_batch_id = Some(update.batch_id);
                    integrated_seqs.push(item.row.relay_seq);
                }
                remaining = deferred;
                // 本轮没有任何新 import：剩下的行缺的是组外依赖，多扫也扫不出来。
                if round_imported == 0 {
                    break;
                }
            }
            if integrated_seqs.is_empty() {
                // 预检说 Ready、重放后却一行都动不了（并发/损坏）：整组保持 verified。
                return Ok(0);
            }
            let changed = imported > 0;
            if imported == 0 {
                // 只有已覆盖行：一次事务标记，不写 update。
                mark_integrated_batch(self.vault, log_id, &integrated_seqs)?;
            } else {
                let combined = doc.pending_update(&before)?;
                if combined.payload.is_empty() {
                    // 防御性分支：没有新 ops 时不能追加空 update，只标记。
                    mark_integrated_batch(self.vault, log_id, &integrated_seqs)?;
                } else {
                    let batch_id = last_batch_id.unwrap_or_else(|| "aggregate".to_owned());
                    let record = UpdateRecord::new(batch_id, combined);
                    match append_updates_with_integrated_marks(
                        self.vault,
                        doc_id,
                        current.head_seq,
                        &record,
                        log_id,
                        &integrated_seqs,
                    ) {
                        Ok(head_after) => {
                            // 本地 seq 只能由 head+1 分配（一次组合落盘 = 一个 seq）。
                            if head_after.head_seq != current.head_seq.saturating_add(1) {
                                return Err(SyncError::LocalCorrupt("本地 seq 未按 head+1 分配"));
                            }
                        }
                        // 并发 writer 推进了 head：整组没有落盘/标记，重读后重放一次。
                        Err(menghunote_storage::StorageError::HeadConflict { .. })
                            if conflict_retries == 0 =>
                        {
                            conflict_retries += 1;
                            continue;
                        }
                        Err(error) => return Err(storage_view::map_storage_error(error)),
                    }
                }
            }
            let count = integrated_seqs.len();
            outcome.report.integrated += count;
            outcome.report.body_integrated += count;
            if changed && !outcome.changed_heads.iter().any(|head| head == doc_id) {
                outcome.changed_heads.push(doc_id.to_owned());
            }
            return Ok(count);
        }
    }

    /// 只做投影：没有新结构 op 时 `commit` 仅重建 `tree/notes` 与 body 行（幂等）。
    ///
    /// 失败返回 [`SyncError::StructureProjection`]：结构 CRDT 已 durable，但本地投影
    /// **没有**追上，调用方必须明确报错并保持写禁用（不得假刷新）。
    fn project_structure_only(&self) -> Result<(), SyncError> {
        let sync = SyncContext::new(self.identity.peer_id, self.root.key_epoch);
        let mut writer = menghunote_storage::StructureWriter::open_with_sync(
            self.vault,
            self.identity.peer_id,
            Some(sync),
        )
        .map_err(|error| SyncError::StructureProjection(error.to_string()))?;
        writer
            .commit("s2b-project")
            .map_err(|error| SyncError::StructureProjection(error.to_string()))?;
        Ok(())
    }

    /// 上报 received/integrated 游标。
    pub fn ack(&mut self) -> Result<AckReport, SyncError> {
        let state = self.client.state()?;
        if state.epoch != self.root.key_epoch {
            return Err(SyncError::Paused(PauseReason::EpochMismatch));
        }
        let local = sync_cursor_state(self.vault)?;
        self.client
            .ack(local.received_seq, local.integrated_seq, state.epoch)?;
        if local
            .confirmed_highwater
            .is_none_or(|high| state.head_seq > high)
        {
            set_confirmed_highwater(self.vault, state.head_seq)?;
        }
        Ok(AckReport {
            received_seq: local.received_seq,
            integrated_seq: local.integrated_seq,
        })
    }

    /// 一轮完整同步（加密 → 推送 → 拉取 → 集成 → ACK）。
    pub fn sync_once(&mut self) -> Result<SyncReport, SyncError> {
        self.sync_once_with_guard(&mut || Ok(()))
    }

    /// 同 [`Self::sync_once`]，但在**首次外部集成之前**调用一次 `guard`（ADR 0012 S1）。
    ///
    /// 语义（安全优先）：
    ///
    /// - `guard` 返回 `Err` → 本轮**不 append**：在此之前只做了加密/推送/拉取（这些不改
    ///   本地正文 head），错误原样返回（取消用 [`SyncError::CancelledBeforeIntegrate`]）；
    /// - `guard` 返回 `Ok` → 正常集成；**进入 integrate 后不再检查**，真实事务必须跑完并
    ///   交付（已提交的 durable 变化不能被"取消"吞掉）；
    /// - 只在 `integrate` 之前调用一次，不是每对象回调（S2 才做可暂停的分批握手）。
    pub fn sync_once_with_guard(
        &mut self,
        guard: &mut dyn FnMut() -> Result<(), SyncError>,
    ) -> Result<SyncReport, SyncError> {
        let encrypted = self.encrypt_pending(self.config.send_batch)?;
        let push = self.push()?;
        // 正文优先：正文 push 之后才上传资产；单资产失败不阻塞后续 pull/integrate。
        let assets = self.upload_pending_assets()?;
        let pull = self.pull()?;
        guard()?;
        let integrate = self.integrate(self.config.integrate_limit)?;
        let ack = self.ack()?;
        Ok(SyncReport {
            encrypted,
            push,
            assets,
            pull,
            integrate,
            ack,
        })
    }

    // ---------------------------------------------------------------- 内部

    fn encrypt_update(
        &self,
        object_id: &ObjectId,
        message_id: &[u8; MESSAGE_ID_LEN],
        generation: u64,
        key_epoch: u64,
        sender_seq: u64,
        plaintext: &[u8],
    ) -> Result<Vec<SyncEncryptedPart>, SyncError> {
        if plaintext.is_empty() {
            return Err(SyncError::LocalCorrupt("update 载荷为空"));
        }
        let object_type = ObjectType::Update;
        let total = plaintext.len() as u64;
        let part_count = u32::try_from(part_count_for(total, object_type))
            .map_err(|_| SyncError::LocalCorrupt("分片数超出 u32"))?;
        let key = derive_object_key(&self.root, object_type, object_id)?;
        let cipher = XChaCha20Poly1305::new_from_slice(key.expose_slice())
            .map_err(|_| SyncError::Object("数据密钥长度非法"))?;
        let cap = cap_plain(object_type) as usize;
        let mut parts = Vec::with_capacity(part_count as usize);
        let mut rng = crate::random::OsRandom;
        for index in 0..part_count {
            let this_len = expected_part_plain_len(object_type, total, index)?;
            let start = index as usize * cap;
            let end = start
                .checked_add(this_len)
                .ok_or(SyncError::LocalCorrupt("分片偏移溢出"))?;
            let chunk = plaintext
                .get(start..end)
                .ok_or(SyncError::LocalCorrupt("分片超出载荷长度"))?;
            let nonce = crate::random::array::<NONCE_LEN>(&mut rng)?;
            let header = EnvelopeHeader {
                protocol_version: PROTOCOL_VERSION,
                header_len: HEADER_LEN,
                object_type,
                cipher_suite: CIPHER_SUITE,
                flags: 0,
                vault_id: self.vault_id,
                object_id: *object_id,
                message_id: *message_id,
                producer_device_id: self.identity.device_id,
                generation,
                key_epoch,
                sender_seq,
                part_index: index,
                part_count,
                total_plaintext_len: total,
                this_plaintext_len: u32::try_from(chunk.len())
                    .map_err(|_| SyncError::LocalCorrupt("分片明文超出 u32"))?,
                ciphertext_len: u32::try_from(chunk.len() + 16)
                    .map_err(|_| SyncError::LocalCorrupt("分片密文超出 u32"))?,
                context_hash: [0u8; 32],
                nonce,
            };
            let aad = header.aad(&[])?;
            let ciphertext = cipher
                .encrypt(
                    XNonce::from_slice(&nonce),
                    Payload {
                        msg: chunk,
                        aad: &aad,
                    },
                )
                .map_err(|_| SyncError::Object("数据对象加密失败"))?;
            let envelope = Envelope::new(header, ciphertext)?.encode()?;
            let digest: [u8; 32] = Sha256::digest(&envelope).into();
            parts.push(SyncEncryptedPart {
                part_no: index,
                envelope,
                digest,
            });
        }
        Ok(parts)
    }

    /// 只做**验收**（验签/摘要/解密/载荷校验），不写库；返回可批量写入的 inbox 行。
    ///
    /// 拆出这一步是为了让 `pull` 能在 `EngineConfig::inbox_batch` 打开时把整页行用
    /// 一个事务写入（指标/实验路径）；默认路径仍是"验收一条、立刻 durable 一条"。
    fn verify_update_event(
        &mut self,
        log_id: &[u8; 16],
        event: &RelayLogEvent,
    ) -> Result<VerifiedInboxRow, SyncError> {
        let object_id = event
            .object_id
            .ok_or(SyncError::Object("update 事件缺少 object_id"))?;
        if event.epoch != self.root.key_epoch {
            return Err(SyncError::Paused(PauseReason::EpochMismatch));
        }
        let signature = event
            .signature
            .ok_or(SyncError::Paused(PauseReason::MissingSignature))?;
        let commit = ObjectCommit::decode(&event.event)?;
        if commit.vault_id != self.vault_id
            || commit.object_id != object_id
            || commit.key_epoch != event.epoch
            || commit.object_type != ObjectType::Update
        {
            return Err(SyncError::Object("ObjectCommit 与日志事件不一致"));
        }

        let mut raw_parts = Vec::with_capacity(commit.part_count as usize);
        let mut envelopes = Vec::with_capacity(commit.part_count as usize);
        for part_no in 0..commit.part_count {
            let bytes = self.client.part(&object_id, part_no)?;
            let envelope =
                Envelope::decode(&bytes).map_err(|_| SyncError::Object("信封解码失败"))?;
            let header = &envelope.header;
            if header.object_type != ObjectType::Update
                || header.vault_id != self.vault_id
                || header.object_id != object_id
                || header.part_index != part_no
                || header.part_count != commit.part_count
                || header.key_epoch != commit.key_epoch
                || header.generation != commit.generation
                || header.sender_seq != commit.sender_seq
                || header.total_plaintext_len != commit.total_len
            {
                return Err(SyncError::Object("信封 header 与 ObjectCommit 不一致"));
            }
            raw_parts.push(bytes);
            envelopes.push(envelope);
        }

        // object_digest = SHA256(BE32(part_count) || concat(BE32(no) || SHA256(part)))
        let hashes: Vec<[u8; 32]> = raw_parts
            .iter()
            .map(|bytes| {
                let digest: [u8; 32] = Sha256::digest(bytes).into();
                digest
            })
            .collect();
        let computed: [u8; 32] =
            Sha256::digest(object_digest_input(commit.part_count, &hashes)?).into();
        if computed != commit.object_digest {
            return Err(SyncError::Object("object_digest 不符"));
        }

        // 生产者一致性 + 证书验签。
        let producer = envelopes[0].header.producer_device_id;
        let message_id = envelopes[0].header.message_id;
        if envelopes.iter().any(|env| {
            env.header.producer_device_id != producer || env.header.message_id != message_id
        }) {
            return Err(SyncError::Object(
                "各片 header 的 producer/message_id 不一致",
            ));
        }
        let cert = self
            .client
            .device_cert(&producer)?
            .ok_or(SyncError::Object("缺少生产者设备证书"))?;
        if cert.vault_id != self.vault_id || cert.device_id != producer {
            return Err(SyncError::Object("设备证书归属不符"));
        }
        verify_signature(
            &cert.sign_ed25519_pub,
            &object_commit_signing_input(&commit)?,
            &signature,
        )?;

        // AEAD 解密 + §16 载荷校验。
        let key = derive_object_key(&self.root, ObjectType::Update, &object_id)?;
        let cipher = XChaCha20Poly1305::new_from_slice(key.expose_slice())
            .map_err(|_| SyncError::Object("数据密钥长度非法"))?;
        let mut plaintext = Vec::with_capacity(commit.total_len as usize);
        for (index, envelope) in envelopes.iter().enumerate() {
            let expected = expected_part_plain_len(
                ObjectType::Update,
                commit.total_len,
                u32::try_from(index).map_err(|_| SyncError::Object("分片下标溢出"))?,
            )?;
            if envelope.header.this_plaintext_len as usize != expected {
                return Err(SyncError::Object("分片明文长度不符合固定切片"));
            }
            let aad = envelope.header.aad(&[])?;
            let chunk = cipher
                .decrypt(
                    XNonce::from_slice(&envelope.header.nonce),
                    Payload {
                        msg: &envelope.ciphertext,
                        aad: &aad,
                    },
                )
                .map_err(|_| SyncError::Object("AEAD 解密失败"))?;
            plaintext.extend_from_slice(&chunk);
        }
        if plaintext.len() as u64 != commit.total_len {
            return Err(SyncError::Object("解密后的明文长度与 commit 不符"));
        }
        let update = UpdateV1::decode(&plaintext)?;
        if update.generation != commit.generation || update.origin_seq != commit.sender_seq {
            return Err(SyncError::Object("UpdateV1 与 ObjectCommit 不一致"));
        }
        envelopes[0].header.validate_update_payload(&update)?;

        Ok(VerifiedInboxRow {
            log_id: *log_id,
            relay_seq: event.relay_seq,
            object_id,
            epoch: event.epoch,
            verified_payload: plaintext,
        })
    }

    /// 从 relay 完整读回并验证一个 ASSET 对象（评审 0003 P0-B：ASSET 来源认证）。
    ///
    /// 日志事件里的 `ObjectCommit` 只是 relay 的声明。只有真正取回全部分片、用本
    /// epoch 根解密成功，并且资产明文头能重算出同一个 `object_id`，才能证明该事件
    /// 对应一个真实存在的密文资产；任何一步失败都返回错误，调用方不得写记录/推游标。
    ///
    /// 验证口径与 checkpoint writer 完全一致：统一走
    /// [`crate::upload::fetch_verified_asset`]（分片上限/分配保护/固定切片/每片
    /// header 上下文/AEAD/摘要/明文头/OID 重算），这里只把事件里的完整声明传进去。
    ///
    /// 返回值额外带上**已验证的资产数据字节**：pull 消费 ASSET 事件时要把它们落进
    /// 本地内容寻址表（否则"另一台设备同步后取不到资产字节"——ASSET 事件是更新日志
    /// 里唯一携带资产数据的载体）。
    fn download_verified_asset(
        &mut self,
        object_id: &ObjectId,
        commit: &ObjectCommit,
    ) -> Result<(crate::upload::VerifiedAssetFacts, Vec<u8>), SyncError> {
        crate::upload::fetch_verified_asset_data(
            &mut *self.client,
            &self.root,
            self.vault_id,
            MAX_ASSET_RAW_BYTES as u64,
            &crate::upload::AssetExpectation {
                object_id,
                object_type: commit.object_type,
                generation: commit.generation,
                message_id: Some(commit.message_id),
                sender_seq: commit.sender_seq,
                part_count: commit.part_count,
                total_len: commit.total_len,
                object_digest: &commit.object_digest,
                key_epoch: commit.key_epoch,
            },
        )
    }

    /// 把已验证的资产明文写进本地内容寻址表（`assets`）。
    ///
    /// 幂等语义由 storage 保证：同 hash + 同字节 → `Ok(())`；同 hash 不同字节
    /// （本地已损坏或哈希碰撞）→ `Corrupt`，这里 fail-closed（不写记录、不推游标）。
    fn store_verified_asset(
        &self,
        verified: &crate::upload::VerifiedAssetFacts,
        data: &[u8],
    ) -> Result<(), SyncError> {
        menghunote_storage::insert_asset(
            self.vault,
            &verified.real_sha256,
            verified.byte_len,
            Some(&verified.media_type),
            data,
        )?;
        Ok(())
    }

    /// 持久写入一条 skipped-asset 记录；同一 seq 重放必须幂等，内容冲突 fail-closed。
    ///
    /// 冲突分两类：HMAC/结构/认证失败是 `LocalCorrupt`（本地记录被篡改或旧修订）；
    /// HMAC 校验通过但同一 seq 的身份/资产事实不同，是 relay 重写已消费历史，
    /// 返回 `Paused(RelayHistoryRewritten)`，**不覆盖**已有记录、不推进游标。
    ///
    /// **先做来源认证，再落资产字节**（[`Self::download_verified_asset`]）：只有真实下载
    /// 并验证通过的 ASSET 才允许落记录，记录里的资产事实全部来自本机验证结果。验证通过后
    /// 把明文写进本地内容寻址表（[`Self::store_verified_asset`]，幂等），这样**已存在的
    /// 设备**同步后也能用 `asset_bytes` 取到资产；结构校验/下载/验证/落库任一失败都不写
    /// 记录，调用方（pull）因此不会推进游标。
    ///
    /// 结构校验（不验签，因为 ASSET 信封的 producer 被规范化为 0，事件里拿不到
    /// 生产者公钥）：事件必须是可解码的 `ObjectCommit`，类型 ASSET、object_id /
    /// vault / epoch 与日志事件一致、generation=0、sender_seq=0。
    ///
    /// 返回值为本次**实际下载并验证通过的资产明文字节**（`PullReport::asset_bytes` 口径；
    /// 幂等重放也会真实下载一次，因此同样计入）。
    fn persist_skipped_asset(
        &mut self,
        log_id: &[u8; 16],
        event: &RelayLogEvent,
        cursor: &str,
    ) -> Result<u64, SyncError> {
        let object_id = event
            .object_id
            .ok_or(SyncError::Object("ASSET 日志事件缺少 object_id"))?;
        // 与 UPDATE 路径同口径：事件 epoch 必须**等于**本地根。
        // 旧实现只拒绝"更新"的 relay（`>`），epoch 落后会走到 AEAD 失败并报成
        // `Object`；语义上这属于"本地根/relay 不同代"，应暂停而不是当成对象损坏。
        if event.epoch != self.root.key_epoch {
            return Err(SyncError::Paused(PauseReason::EpochMismatch));
        }
        let commit = ObjectCommit::decode(&event.event)?;
        if commit.vault_id != self.vault_id
            || commit.object_id != object_id
            || commit.key_epoch != event.epoch
            || commit.object_type != ObjectType::Asset
            || commit.generation != 0
            || commit.sender_seq != 0
        {
            return Err(SyncError::Object(
                "ASSET 日志事件的 ObjectCommit 与事件不符",
            ));
        }
        // 入口先验后下载：即便是 wire 层已收紧（`ObjectCommit::validate` 的 ASSET
        // 64MiB 上限），这里也显式挡一次，保证 `commit.total_len` 不会以无界值进入
        // 读回层的容量分配。
        if commit.total_len > MAX_ASSET_RAW_BYTES as u64 {
            return Err(SyncError::Object("ASSET 总长超出 64MiB 上限"));
        }
        // 评审 0003 P0-B 残余：HMAC 只能证明本机记录过 relay 的声明；写记录之前必须
        // 真正取回并验证资产。失败直接 `?`，不写记录、不写 inbox、不推游标。
        let (verified, data) = self.download_verified_asset(&object_id, &commit)?;
        let asset_bytes = verified.byte_len;
        // 资产字节落本地（内容寻址、幂等）：只有这一步成功，别的设备同步后
        // `asset_bytes` 才真的取得到；记录只表示"本机消费了这条 ASSET"。
        self.store_verified_asset(&verified, &data)?;
        // 本机刚完成"下载 + 全部分片校验 + 落盘"，relay 上该对象必然完整：直接落
        // durable 已上传标记，避免下一轮普通同步再为这个对象发探测/重传请求。
        asset_sync::mark_uploaded(
            self.vault,
            &asset_sync::AssetUploadedFact {
                object_id,
                part_count: commit.part_count,
                total_plain_len: commit.total_len,
                object_digest: commit.object_digest,
                key_epoch: commit.key_epoch,
            },
        )?;
        let record = SkippedAssetRecord {
            log_id: *log_id,
            relay_seq: event.relay_seq,
            object_id,
            kind: SKIPPED_ASSET_KIND_ASSET,
            epoch: event.epoch,
            real_sha256: verified.real_sha256,
            byte_len: verified.byte_len,
            media_type: verified.media_type,
            object_digest: verified.object_digest,
            commit: event.event.clone(),
            cursor: cursor.to_owned(),
        };
        let mac_key = skipped_asset_mac_key(&self.identity.signing_key, &self.vault_id)?;
        let value = record.encode(&mac_key)?;
        let key = skipped_asset_meta_key(log_id, event.relay_seq);
        // 先 insert-do-nothing，再回读比较：并发/重放时先写者赢，后写者只做一致性校验，
        // 绝不覆盖成不同事实。
        let inserted = self.vault.exec(
            "INSERT INTO meta(key, value) VALUES(?1, ?2) ON CONFLICT(key) DO NOTHING",
            &[
                menghunote_storage::Value::Text(key.clone()),
                menghunote_storage::Value::Blob(value),
            ],
        )?;
        if inserted == 1 {
            return Ok(asset_bytes);
        }
        let existing = self
            .vault
            .get_meta(&key)?
            .ok_or(SyncError::LocalCorrupt("skipped-asset 记录缺失"))?;
        let existing = SkippedAssetRecord::decode(&existing, &mac_key)?;
        // cursor 允许因页边界不同而变化（重试可能拿到不同 next_cursor），但身份与已验证
        // 资产字段必须完全一致。HMAC 已通过，说明本地记录没有被篡改；此时同一 seq
        // 解析出不同事实，只可能是 relay 重写了历史（或对同一 OID 返回了不同密文）——
        // 映射成暂停而不是 `LocalCorrupt`，避免让本地库永久表现为"本地损坏"。
        if existing.log_id != record.log_id
            || existing.relay_seq != record.relay_seq
            || existing.object_id != record.object_id
            || existing.kind != record.kind
            || existing.epoch != record.epoch
            || existing.real_sha256 != record.real_sha256
            || existing.byte_len != record.byte_len
            || existing.media_type != record.media_type
            || existing.object_digest != record.object_digest
            || existing.commit != record.commit
        {
            return Err(SyncError::Paused(PauseReason::RelayHistoryRewritten));
        }
        Ok(asset_bytes)
    }

    fn read_cursor(&self) -> Result<Option<String>, SyncError> {
        match self.vault.get_meta(RELAY_CURSOR_META)? {
            Some(bytes) if bytes.is_empty() => Ok(None),
            Some(bytes) => String::from_utf8(bytes)
                .map(Some)
                .map_err(|_| SyncError::LocalCorrupt("relay 游标不是 UTF-8")),
            None => Ok(None),
        }
    }

    fn write_cursor(&self, cursor: &str) -> Result<(), SyncError> {
        self.vault.set_meta(RELAY_CURSOR_META, cursor.as_bytes())?;
        Ok(())
    }
}

/// `Instant::elapsed` 的毫秒截断（`Duration` 转 `u64`，超出范围饱和到 `u64::MAX`）。
fn elapsed_millis(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// 一条待集成的 inbox 行 + 取出后缓存的 doc id / 负载大小（供公平取批复用）。
struct PendingInboxRow {
    row: SyncInboxRow,
    doc_id: String,
    size: usize,
}

/// 单行集成的结果（deferred 时保持 durable verified，不推进任何水位）。
enum InboxRowOutcome {
    /// 已标记 integrated；`changed` = 该文档 head 真的前进了。
    Integrated { changed: bool },
    /// 依赖未就绪（未知文档 / base_vv 未覆盖）。
    Deferred,
}

/// 结构批选取：结构文档优先，最多 `batch_objects` / 约 `batch_bytes` / `batch_time`。
fn select_structure_batch(pending: &[PendingInboxRow], config: &IntegrateConfig) -> Vec<usize> {
    let started = std::time::Instant::now();
    let mut selected = Vec::new();
    let mut bytes = 0usize;
    for (index, item) in pending.iter().enumerate() {
        if item.doc_id != STRUCTURE_DOC_ID {
            continue;
        }
        if !selected.is_empty() {
            if selected.len() >= config.batch_objects {
                break;
            }
            if bytes.saturating_add(item.size) > config.batch_bytes {
                break;
            }
            if started.elapsed() >= config.batch_time {
                break;
            }
        }
        bytes = bytes.saturating_add(item.size);
        selected.push(index);
    }
    selected
}

/// 正文批选取：按 doc 轮转（`rotation` 决定起点），避免只刷最早的一篇。
fn select_body_batch(
    pending: &[PendingInboxRow],
    config: &IntegrateConfig,
    rotation: usize,
) -> Vec<usize> {
    let started = std::time::Instant::now();
    let mut selected = Vec::new();
    let mut bytes = 0usize;
    // (doc_id, 该 doc 的候选下标)。doc 数量很少（参与方按笔记数），线性查找足够。
    let mut docs: Vec<(&str, Vec<usize>)> = Vec::new();
    for (index, item) in pending.iter().enumerate() {
        if item.doc_id == STRUCTURE_DOC_ID {
            continue;
        }
        match docs
            .iter_mut()
            .find(|(doc_id, _)| *doc_id == item.doc_id.as_str())
        {
            Some((_, indices)) => indices.push(index),
            None => docs.push((item.doc_id.as_str(), vec![index])),
        }
    }
    if docs.is_empty() {
        return selected;
    }
    let offset = rotation % docs.len();
    let mut cursors = vec![0usize; docs.len()];
    'outer: loop {
        let mut took_any = false;
        for step in 0..docs.len() {
            let doc = (offset + step) % docs.len();
            let Some(&index) = docs[doc].1.get(cursors[doc]) else {
                continue;
            };
            cursors[doc] += 1;
            if !selected.is_empty() {
                if selected.len() >= config.batch_objects {
                    break 'outer;
                }
                if bytes.saturating_add(pending[index].size) > config.batch_bytes {
                    break 'outer;
                }
                if started.elapsed() >= config.batch_time {
                    break 'outer;
                }
            }
            bytes = bytes.saturating_add(pending[index].size);
            selected.push(index);
            took_any = true;
        }
        if !took_any {
            break;
        }
    }
    selected
}

/// 正文聚合批选取（[`IntegrateConfig::aggregate_body`]）：按 doc 轮转，**每篇文档**的
/// 候选行尽量全部进同一批。
///
/// 与 [`select_body_batch`] 的语义差异（Phase 1A-2 有意为之）：
/// - 旧路径把 `batch_objects` 解释为**行数**（32 行一批），一页 256 行同一文档会被切成
///   8 批 = 8 次 `load_body_doc` 全量重放；
/// - 聚合路径把 `batch_objects` 解释为**每批参与的文档数**（默认 32 篇），单篇文档的
///   行数只受 `batch_bytes` / `batch_time` 与扫描窗口约束。目标：单文档一页 256 行
///   一次 load；多文档仍按 `rotation` 轮转起点，避免饿死靠后的文档。
/// - 预算仍是**软**的：单行超大时允许它单独成批；遇到字节/时间上限就停这一批，
///   已选中的行全部交付，剩余行留在 durable verified 等下一批。
///
/// Phase 1A 缺陷收口起，选批前先按 doc 读一次 O(1) 的 `head_vv` 做与组处理**同一套**
/// 预检（[`body_group_readiness`]，不 `load_body_doc`）：
/// - 纯 deferred 组不进 selected，也不消耗 `batch_bytes` —— 否则两个大 deferred 文档
///   会把后面的就绪文档挤出本批，连续零进展后整轮提前停止（饿死）；
/// - 全部已覆盖组进 selected 以便标记，但按 0 字节计，不占就绪正文预算；
/// - 未知文档（没有 `loro_docs` 行）无法预检，按 deferred 处理（保持 durable verified，
///   等结构投影/依赖到达）。
fn select_body_batch_aggregated(
    vault: &Vault,
    pending: &[PendingInboxRow],
    config: &IntegrateConfig,
    rotation: usize,
) -> Result<Vec<usize>, SyncError> {
    let started = std::time::Instant::now();
    let mut selected = Vec::new();
    let mut bytes = 0usize;
    // 需要 import 的行数（软预算只看这些行，已覆盖的标记行是 0 字节）。
    let mut ready_rows = 0usize;
    // (doc_id, 该 doc 的候选下标)。doc 数量很少（参与方按笔记数），线性查找足够。
    let mut docs: Vec<(&str, Vec<usize>)> = Vec::new();
    for (index, item) in pending.iter().enumerate() {
        if item.doc_id == STRUCTURE_DOC_ID {
            continue;
        }
        match docs
            .iter_mut()
            .find(|(doc_id, _)| *doc_id == item.doc_id.as_str())
        {
            Some((_, indices)) => indices.push(index),
            None => docs.push((item.doc_id.as_str(), vec![index])),
        }
    }
    if docs.is_empty() {
        return Ok(selected);
    }
    let offset = rotation % docs.len();
    let max_docs = config.batch_objects.max(1);
    let mut groups = 0usize;
    'outer: for step in 0..docs.len() {
        if groups >= max_docs {
            break;
        }
        let doc = (offset + step) % docs.len();
        let doc_id = docs[doc].0;
        // 未知文档无法预检：按 deferred 跳过（组处理也不会 load；等结构投影建立文档行）。
        if !storage_view::doc_exists(vault, doc_id)? {
            continue;
        }
        let head_vv = load_head(vault, doc_id)?.version_vector()?;
        // 同一文档可能跨 log（日志轮换）：标记必须在同 log 内一次事务完成，因此按 log
        // 拆子组分别预检；文档级轮转与预算口径不变。
        let mut subgroups: Vec<([u8; 16], Vec<usize>)> = Vec::new();
        for &index in &docs[doc].1 {
            let log_id = pending[index].row.log_id;
            match subgroups.iter_mut().find(|(id, _)| *id == log_id) {
                Some((_, rows)) => rows.push(index),
                None => subgroups.push((log_id, vec![index])),
            }
        }
        let mut took_any = false;
        for (_, indices) in &subgroups {
            match body_group_readiness(pending, indices, &head_vv) {
                // 纯 deferred：不进入 selected、不消耗批次预算。
                BodyGroupReadiness::Deferred => continue,
                // 全部已覆盖：需要标记但不 load，按 0 字节计。
                BodyGroupReadiness::Covered => {
                    selected.extend(indices.iter().copied());
                    took_any = true;
                }
                BodyGroupReadiness::Ready => {
                    for &index in indices {
                        // 软预算：第一条 ready 行永远放行（单行超大时单独成批）。
                        if ready_rows > 0 {
                            if bytes.saturating_add(pending[index].size) > config.batch_bytes {
                                break 'outer;
                            }
                            if started.elapsed() >= config.batch_time {
                                break 'outer;
                            }
                        }
                        bytes = bytes.saturating_add(pending[index].size);
                        ready_rows += 1;
                        selected.push(index);
                        took_any = true;
                    }
                }
            }
        }
        if took_any {
            groups += 1;
        }
    }
    Ok(selected)
}

/// 一组（同一 `doc_id` + `log_id`，按 relay_seq 升序）inbox 行的预检结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BodyGroupReadiness {
    /// 所有行的 `end_vv` 都已被 `head_vv` 覆盖：只需标记，不需要 import。
    Covered,
    /// 至少一行未被覆盖且 `base_vv` 已被 `head_vv` 覆盖：至少有一行要 import。
    Ready,
    /// 未覆盖行的 `base_vv` 全都不满足：纯 deferred，本轮跳过（不重放、不占预算）。
    Deferred,
}

/// 用 `head_vv`（O(1)）对一组行做轻量预检，**不** `load_body_doc`。
///
/// 语义与组内逐行重放后的检查一致：`end_vv` 已被覆盖 = 该 update 应用过；未覆盖行的
/// `base_vv` 全不满足 = 真正缺外部依赖。空 `base_vv/end_vv` 由 [`decode_vv`] 归一化成
/// 空版本向量（与逐行路径一致），空 `head_vv` 由 `HeadState::version_vector` 归一化。
///
/// 解码失败时返回 [`BodyGroupReadiness::Ready`]：把错误留给真正的组处理路径原样报出，
/// 预检不能把损坏数据误判成 deferred 而让它永远滞留。
fn body_group_readiness(
    pending: &[PendingInboxRow],
    indices: &[usize],
    head_vv: &VersionVector,
) -> BodyGroupReadiness {
    let mut uncovered = false;
    for &index in indices {
        let Some(payload) = pending[index].row.verified_payload.as_deref() else {
            return BodyGroupReadiness::Ready;
        };
        let Ok(update) = UpdateV1::decode(payload) else {
            return BodyGroupReadiness::Ready;
        };
        let Ok(end_vv) = decode_vv(&update.end_vv, "end_vv 解码失败") else {
            return BodyGroupReadiness::Ready;
        };
        if vv_includes(head_vv, &end_vv) {
            continue;
        }
        uncovered = true;
        let Ok(base_vv) = decode_vv(&update.base_vv, "base_vv 解码失败") else {
            return BodyGroupReadiness::Ready;
        };
        if vv_includes(head_vv, &base_vv) {
            return BodyGroupReadiness::Ready;
        }
    }
    if uncovered {
        BodyGroupReadiness::Deferred
    } else {
        BodyGroupReadiness::Covered
    }
}

/// 组内各行的 relay_seq（批量标记用；顺序不影响标记语义）。
fn relay_seqs_of(pending: &[PendingInboxRow], indices: &[usize]) -> Vec<u64> {
    indices
        .iter()
        .map(|&index| pending[index].row.relay_seq)
        .collect()
}

/// 解码 `UpdateV1.base_vv/end_vv`；空字节等价于空版本向量（seq=1 的规范形式）。
fn decode_vv(bytes: &[u8], error: &'static str) -> Result<VersionVector, SyncError> {
    if bytes.is_empty() {
        return Ok(VersionVector::new());
    }
    VersionVector::decode(bytes).map_err(|_| SyncError::Object(error))
}

/// 空的 `end_vv`（seq=1 时）规范化为空版本向量的编码，避免接收端解码失败。
fn normalize_vv(bytes: &[u8]) -> Vec<u8> {
    if bytes.is_empty() {
        VersionVector::new().encode()
    } else {
        bytes.to_vec()
    }
}

/// 版本向量包含关系：`local` 覆盖 `required` 的全部区间。
fn vv_includes(local: &VersionVector, required: &VersionVector) -> bool {
    // `VersionVector` 是 peer -> 已见最大 counter（不含边界之外）。
    for (peer, required_end) in required.iter() {
        match local.get(peer) {
            Some(local_end) if local_end >= required_end => {}
            _ => return false,
        }
    }
    true
}

fn object_digest_of(parts: &[SyncEncryptedPart]) -> Result<[u8; 32], SyncError> {
    let hashes: Vec<[u8; 32]> = parts.iter().map(|part| part.digest).collect();
    let input = object_digest_input(parts.len() as u32, &hashes)?;
    Ok(Sha256::digest(&input).into())
}

fn object_digest_of_parts(
    parts: &[menghunote_storage::SyncOutboxPart],
) -> Result<[u8; 32], SyncError> {
    let hashes: Vec<[u8; 32]> = parts.iter().map(|part| part.digest).collect();
    let input = object_digest_input(parts.len() as u32, &hashes)?;
    Ok(Sha256::digest(&input).into())
}

/// 把 outbox 里的 UUID 文本转成 16B wire message_id。
fn decode_message_id(text: &str) -> Result<[u8; MESSAGE_ID_LEN], SyncError> {
    let cleaned: String = text.chars().filter(|ch| *ch != '-').collect();
    if cleaned.len() != MESSAGE_ID_LEN * 2 {
        return Err(SyncError::LocalCorrupt("outbox message_id 不是 UUID"));
    }
    crate::ids::parse_hex::<MESSAGE_ID_LEN>(&cleaned)
        .map_err(|_| SyncError::LocalCorrupt("outbox message_id 非法"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_id_accepts_uuid_and_plain_hex() {
        let expected = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ];
        assert_eq!(
            decode_message_id("00112233-4455-6677-8899-aabbccddeeff").unwrap(),
            expected
        );
        assert_eq!(
            decode_message_id("00112233445566778899aabbccddeeff").unwrap(),
            expected
        );
        assert!(decode_message_id("msg-1").is_err());
        assert!(decode_message_id("00112233-4455-6677-8899-aabbccddee").is_err());
    }

    #[test]
    fn vv_containment() {
        let mut local = VersionVector::new();
        local.insert(1, 3);
        let mut required = VersionVector::new();
        required.insert(1, 2);
        assert!(vv_includes(&local, &required));
        let mut ahead = VersionVector::new();
        ahead.insert(1, 4);
        assert!(!vv_includes(&local, &ahead));
        let mut other_peer = VersionVector::new();
        other_peer.insert(2, 1);
        assert!(!vv_includes(&local, &other_peer));
        // 空 required 永远满足
        assert!(vv_includes(&local, &VersionVector::new()));
    }

    #[test]
    fn skipped_asset_record_rejects_old_revision_bytes() {
        let mac_key = crate::keys::Secret32::from_array([0x42; 32]);
        let object_id = [0x11; 32];
        let commit = ObjectCommit {
            vault_id: [0x01; 16],
            object_id,
            message_id: [0x22; 16],
            object_type: ObjectType::Asset,
            generation: 0,
            key_epoch: 1,
            sender_seq: 0,
            part_count: 1,
            total_len: 67,
            object_digest: [0x33; 32],
        };
        let record = SkippedAssetRecord {
            log_id: [0x44; 16],
            relay_seq: 1,
            object_id,
            kind: SKIPPED_ASSET_KIND_ASSET,
            epoch: 1,
            real_sha256: [0x55; 32],
            byte_len: 25,
            media_type: "text/plain".to_owned(),
            object_digest: [0x33; 32],
            commit: commit.encode().expect("commit"),
            cursor: "c1".to_owned(),
        };
        let bytes = record.encode(&mac_key).expect("encode");
        SkippedAssetRecord::decode(&bytes, &mac_key).expect("当前修订必须可解码");

        // 旧 v2 记录（magic 旧、HMAC 自洽）必须被独立修订号拒绝，不得改号升级。
        let mut old = bytes;
        let magic_len = SKIPPED_ASSET_MAGIC.len();
        old[..magic_len].copy_from_slice(b"MN/skipped-asset/2\0");
        let body_len = old.len() - 32;
        let tag = skipped_asset_mac(&old[..body_len], &mac_key).expect("mac");
        old[body_len..].copy_from_slice(&tag);
        let error = SkippedAssetRecord::decode(&old, &mac_key).expect_err("旧修订必须拒绝");
        assert!(format!("{error}").contains("版本非法"), "{error}");
    }
}
