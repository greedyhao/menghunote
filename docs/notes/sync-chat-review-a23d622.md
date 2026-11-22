# 同步 Phase 1A 收尾硬化：单文件 Chat 评审稿

> **使用方法（chat 模式，无 agent/无工具）**
> 1. 把本文件作为**唯一附件**上传给高级模型。
> 2. 先把下面「提示词」整段发给它，再让它读附件。
> 3. 若平台对附件有大小限制，按文末「分段上传」处理；不要让它自己执行命令。
>
> 本文自带：范围/不变量、P1/P2 状态、关键代码全文、完整代码 diff、测试证据。
> 代码基线 `5d01e9e`，当前 HEAD `a23d622`；**Phase 1B 未实现**。

---

## 提示词（复制给模型）

你是 Rust / CRDT / 本地存储与同步的评审者。请只依据我上传的 Markdown 做代码评审，
不要假设你能运行命令、访问仓库或调用工具。先确认你收到了全文（列出你看到的章节），
再按下面顺序审查：

1. `P1-1` baseline finish 完整性：预期 docs/assets 记录、finish 前逐项校验、
   `finishable` 语义、begin 幂等；有没有绕过或 TOCTOU。
2. `P1-2` tombstone TOCTOU：事务外预判 + 事务内 outbox 重判、notes 软删条件，
   安全方向是否只能退化为 kept_local_edit；是否还缺 head CAS。
3. `P1-3` Journal HeadConflict：`JournalHeadStale`/`rebuild_sink` 契约是否足够；
   生产 `session/flush.rs` 未接自动重建，这个缺口是否阻断。
4. `P1-4` ready/count 按 `sync.log_id` 过滤：旧 incarnation 是否还会混入、log 缺失/损坏语义。
5. `P2-5` remote_known 值 CAS：读改写、4 次重试、baseline 批次冲突不内重试；
   是否可能写入更小的 known、是否可能误删。
6. `P2-6/P2-7` meta 只认 BLOB、三入口一致；`BEGIN IMMEDIATE` 共享开启点、BUSY 语义。
7. `P2-8/P2-9` `OriginMarks`、LocalCorrupt 上抛/整轮中止的 UX 与重试。
8. 明确区分：**已关 / 未关 / 不能判断**。每条必须引用文件/函数/测试名。
   特别确认：是否仍有可能在 `integrated >= S` 前删 inbox/skipped_asset；
   是否违反 head 单调、seq=head+1、received/integrated 连续前缀、remote/adopt 不产生 outbox。
   最后回答：是否允许进入 Phase 1B。

输出格式：
- 结论（P1/P2 每项一行）
- 必须修正（文件/函数/测试，按严重度）
- 不能判断/需要补充的材料
- 未关风险与是否阻断 Phase 1B
- 不要重复贴大段代码。

---

## 1. 评审范围与硬边界

**代码基线**：`5d01e9e`；**当前 HEAD**：`a23d622`（本轮硬化提交）。
**Phase 1B 未实现**：没有 `CatchUpMode`、`catchup_prepare`/`catchup_integrate`、prune、真实
snapshot 下载/验签；计划 §9 只是待确认约束。

**本轮未改**：relay 服务端、`sync-proto` wire/schema、SQLite DDL/迁移、checkpoint 签名、
`integrated`/`received` 连续推进语义、任何第二写连接。

**不可违反的不变量**：
- `loro_updates.seq` 是接收端本地序号；remote import 只能用 `head_seq + 1`，不得写
  `sender_seq`/`relay_seq`。
- `received_seq` / `integrated_seq` 各自只沿连续前缀推进，缺口不跳号。
- `integrated >= S` 未持久化前，不得删除/标记覆盖 `relay_seq <= S` 的 inbox 行；
  durable `skipped_asset` 记录不得误删。
- epoch/rotation/log 边界异常 → `Paused`/显式错误，不得当损坏降级。
- `HeadConflict` 必须停外部写 → 验证/对账 → 有界重放，禁止改 CAS/覆盖 head/丢 inbox。
- remote/adopt 导入不得产生 `sync_outbox`（禁止 echo）。

## 2. 本轮状态

| 项 | 状态 | 关键文件 |
|---|---|---|
| P1-1 finish 完整性 | 完成 | `baseline_apply.rs`（`BaselineExpectation`、`finish_baseline_apply`、`expected_is_complete`） |
| P1-2 tombstone TOCTOU | 完成（SQL 事务内重判） | `baseline_apply.rs`（`tombstone_can_delete`、prepared tombstone/notes UPDATE） |
| P1-3 Journal HeadConflict | **部分**：JournalWriter 契约完成，生产 session/flush 未接线 | `journal.rs`、`error.rs` |
| P1-4 ready/count log_id | 完成 | `sync_bridge.rs` |
| P2-5 remote_known 单调性 | 完成（值 CAS） | `remote_known.rs`、`store.rs`、`baseline_apply.rs` |
| P2-6 meta BLOB-only | 完成 | `sync_meta.rs`、`actor.rs`、`sync_bridge.rs` |
| P2-7 exec_transaction Immediate | 完成 | `actor.rs` |
| P2-8 OriginMarks | 完成 | `store.rs`、`journal.rs`、`sync_bridge.rs` |
| P2-9 资产本地损坏 | 完成 | `engine.rs`（`upload_pending_assets`） |
| P3 | 未做 | 见 §5 |

**明确未关**：`session/flush.rs` 仍直接使用 `VaultJournalSink`，没有接入
`JournalHeadStale`/`rebuild_sink` 的自动恢复；当前只把 `HeadConflict` 记入
`last_write_error`（非静默，但不会自愈）。这是 P1-3 的生产闭环缺口。

## 3. 验证证据（本机）

```text
cargo fmt --all -- --check                                  EXIT 0
cargo clippy --workspace --all-targets   --features menghunote-app/dev-sync -- -D warnings         EXIT 0
cargo test -p menghunote-storage                            274 passed / 0 failed (1 ignored bench)
cargo test -p menghunote-sync                               634 passed / 0 failed (3 ignored manual)
cargo test -p menghunote-app                                740 / 0
cargo test -p menghunote-app --features dev-sync            891 / 0
cargo test --workspace                                     2884 / 0
cargo test --workspace --features menghunote-app/dev-sync  3035 / 0
cargo test --release phase1a_release -- --ignored           1 passed (63.9 s)
```

新增关键回归：
- P1-1 `finish_without_all_docs_is_conflict`、`finish_after_full_batches_is_applied`、
  `begin_idempotent_replay_keeps_expected_set`、`finish_without_all_assets_is_conflict`
- P1-2 `tombstone_outbox_race_between_decision_and_transaction_keeps_note`
- P1-3 `stale_head_is_terminal_then_rebuild_hands_back_entries_and_new_entries_flush`
- P1-4 `ready_and_count_only_include_current_log_incarnation` 等
- P2-5 `concurrent_writers_from_same_old_converge_to_union` 等
- P2-6/7 `text_meta_is_corrupt_from_all_three_entry_points` 等
- P2-8 `per_record_origin_only_extends_for_remote_records` 等
- P2-9 `corrupt_local_asset_raises_instead_of_counting_retryable_failure` 等

## 4. 评审重点（不需要重复代码）

1. P1-1 的 expected meta 编码/校验能否被跳过；旧 journal 无记录时是否一律 fail-closed。
2. P1-2 事务内 `CASE WHEN outbox` 是否覆盖“判定→写入”窗口；软删条件是否可能删 local-only。
3. P1-3 契约是否足够；生产 flusher 不接线是否阻断 Phase 1B。
4. P2-5 CAS 不命中时整批回滚/重试 4 次；`baseline_apply` 批次冲突返回 `SyncConflict`
   由调用方重跑，是否会造成无限重试/可见错误。
5. P2-6 BLOB-only 对旧库/外部写入的兼容性；P2-7 Immediate 对 BUSY/吞吐的影响。
6. P2-9 本地损坏中止整轮同步是否合理；健康资产已上传的幂等性。

## 5. 未做 / 残余

- P3：`peek_prefix` 拷贝、`new(0)` 语义、ASSET 每页一次前缀推进、UpdateV1 缓存、
  预检/执行共用纯函数、push 查询顺序、journal 混合来源单测。
- `session/flush.rs` 生产接线（见上）。
- Phase 1B 的阈值/snapshot/prune 都没做。
- 未改 relay/wire/schema/checkpoint；`remote_known`、`baseline_expected` 都是本地 `meta` 键。

---

# 附录 A：关键代码全文

下面每个文件是当前 HEAD 的**完整源码**，按评审顺序排列；diff 在附录 B。


## `crates/storage/src/baseline_apply.rs`

baseline_apply.rs：P1-1/P1-2、baseline merge、remote_known 写入

````rust
//! S7-34：已验证基线的分批 merge / 原子推进 cursor（P3-I 前置）。
//!
//! 语义：
//! - 下载/解密/摘要验证在 sync 层完成；本模块只接收已验证的 Loro snapshot 字节。
//! - 每批 docs/tombstones 在**一个 SQLite 事务**里落库；任何失败整批回滚。
//! - snapshot 每篇只解码/导入**一次**（`BodyDoc::restore`），之后的派生全部基于这个副本：
//!   - 新文档：直接 `pending_update(&VersionVector::default())`；
//!   - 已有且无本地额外 op：只读 `load_head` 的 VV，等价（VV 相同）则不写 update；
//!   - 已有且有本地额外 op：只重放一次本地副本，导出**极小**的本地差集并反向合进
//!     基线副本的 **oplog**（detached，不重算 state/diff），再导出差集。
//!     绝不二次 `import(snapshot)`、也不用 `load_body_doc` 只为取 VV。
//! - update 的 `batch_id` 固定为 [`BASELINE_MERGE_BATCH`]（origin 标记）。
//! - 每篇应用的快照 VV（远端已知 ops）写入非破坏性本地 meta
//!   `sync.remote_known_vv.<doc_id>`（见 [`crate::remote_known`]），跨 journal 保留，
//!   供后续 tombstone 判定区分「远端已删的已同步文档」与「local-only 文档」。
//! - `finish` 只做**单调**水位/cursor CAS 与 journal 状态迁移；失败不推进任何 cursor。
//! - `begin` 记录本 checkpoint 的**预期参与集合**（覆盖到的 doc 与资产，存于既有 `meta`
//!   单行、只本地可见）：`finish` 必须先证明集合里每一项都已 `applied=1`，才允许写任何
//!   meta；少跑一批（或跑错集合）返回 [`BaselineApplyFinish::Conflict`]，而不是推进 S
//!   之后让 pull 永久跳过 relay 事件。
//! - tombstone 的 outbox 判定会在**同一写事务内**用 SQL 重判一次：判定完成后被并发写入的
//!   outbox 行只会把 `deleted` 退化为 `kept_local_edit`，绝不误删本地编辑。
//! - `abort` 只清 staging，不删已导入的 Loro/asset 数据，也不回滚远端已知 VV。

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use loro::VersionVector;
use rusqlite::types::Value;
use sha2::{Digest, Sha256};

use crate::actor::{AffectedGuard, Statement};
use crate::body_doc::{BodyDoc, PendingUpdate, APP_DOC_SCHEMA, CODEC_VERSION, MIN_READER};
use crate::error::StorageError;
use crate::remote_known;
use crate::store::{load_body_doc, load_head, HeadState};
use crate::sync_bridge::{
    sync_cursor_state, META_SYNC_CONFIRMED_HIGHWATER, META_SYNC_INTEGRATED_SEQ,
    META_SYNC_RECEIVED_SEQ,
};
use crate::Vault;

/// baseline merge 写入 `loro_updates` 的 batch_id / origin 标记。
pub const BASELINE_MERGE_BATCH: &str = "baseline_merge";
/// relay 页面 cursor 的 meta 键（与 sync `engine::RELAY_CURSOR_META` 对齐）。
pub const META_SYNC_RELAY_CURSOR: &str = "sync.relay_cursor";
/// [`BaselineExpectation`] 的持久化 meta 键（**只本地可见**；不是 schema/wire）。
pub const META_BASELINE_EXPECTED: &str = "sync.baseline_expected";
/// 预期集合编码的魔数（防止误读旧值/损坏值）。
const EXPECTED_MAGIC: &[u8] = b"MNBASEEXP1";
/// storage 内部构造 BodyDoc 时使用的 peer id（只影响新文档的本地 op id，不影响 import）。
const BASELINE_PEER_ID: u64 = 1;

/// 基线 apply 的分段计时（**仅用于基准/诊断**；按线程累计，不持久化、不参与语义）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BaselineApplyDebugTimings {
    /// `BodyDoc::restore`（含 snapshot 解码/导入）。
    pub restore_ns: u64,
    /// `doc_exists` / `load_head` / 本地 VV 比较。
    pub head_lookup_ns: u64,
    /// `load_body_doc`（本地重放）。
    pub load_body_doc_ns: u64,
    /// 本地极小增量导出（`local_delta = doc.pending_update(&baseline_vv)`）。
    pub local_delta_export_ns: u64,
    /// 本地极小增量反向导入 `baseline_doc`。
    pub local_delta_import_ns: u64,
    /// 旧实现：对本地 doc 二次 `import(snapshot)` 的耗时（优化后应恒为 0）。
    pub snapshot_reimport_ns: u64,
    /// 新文档：`BodyDoc::create` + `import(snapshot)` 的耗时（优化后应恒为 0）。
    pub new_doc_import_ns: u64,
    /// 落盘用的 `pending_update` 导出。
    pub pending_update_ns: u64,
    /// `exec_transaction`。
    pub transaction_ns: u64,
    /// 整个 `apply_baseline_batch`。
    pub batch_total_ns: u64,
}

/// 基线 apply 的计数（**仅用于基准/诊断**；按线程累计，不持久化）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BaselineApplyDebugCounters {
    pub batches: u64,
    pub docs: u64,
    pub new_docs: u64,
    pub existing_no_extra: u64,
    pub existing_local_extra: u64,
    pub replayed_same_digest: u64,
    pub snapshot_imports: u64,
    pub snapshot_bytes: u64,
    pub load_body_doc_calls: u64,
    pub snapshot_reimport_bytes: u64,
    pub new_doc_import_bytes: u64,
    pub pending_update_calls: u64,
    pub pending_update_bytes: u64,
    pub local_delta_bytes: u64,
}

#[derive(Default)]
struct BaselineDebugAcc {
    timings: BaselineApplyDebugTimings,
    counters: BaselineApplyDebugCounters,
}

thread_local! {
    static BASELINE_DEBUG: RefCell<BaselineDebugAcc> = RefCell::new(BaselineDebugAcc::default());
}

/// 清零当前线程的基线 apply 诊断计数（基准/测试用）。
#[doc(hidden)]
pub fn reset_baseline_apply_debug() {
    BASELINE_DEBUG.with(|acc| *acc.borrow_mut() = BaselineDebugAcc::default());
}

/// 读当前线程的分段计时（基准/测试用）。
#[doc(hidden)]
pub fn baseline_apply_debug_timings() -> BaselineApplyDebugTimings {
    BASELINE_DEBUG.with(|acc| acc.borrow().timings)
}

/// 读当前线程的计数（基准/测试用）。
#[doc(hidden)]
pub fn baseline_apply_debug_counters() -> BaselineApplyDebugCounters {
    BASELINE_DEBUG.with(|acc| acc.borrow().counters)
}

fn debug_add_time<F>(field: F, started: Instant)
where
    F: FnOnce(&mut BaselineApplyDebugTimings) -> &mut u64,
{
    let elapsed = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
    BASELINE_DEBUG.with(|acc| *field(&mut acc.borrow_mut().timings) += elapsed);
}

fn debug_add_count<F>(field: F, delta: u64)
where
    F: FnOnce(&mut BaselineApplyDebugCounters) -> &mut u64,
{
    BASELINE_DEBUG.with(|acc| *field(&mut acc.borrow_mut().counters) += delta);
}

/// staging journal 状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaselineApplyStatus {
    Staging,
    Completed,
}

impl BaselineApplyStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Staging => "staging",
            Self::Completed => "completed",
        }
    }

    fn parse(value: &str) -> Result<Self, StorageError> {
        match value {
            "staging" => Ok(Self::Staging),
            "completed" => Ok(Self::Completed),
            other => Err(StorageError::Corrupt {
                context: format!("未知 baseline_apply.status {other:?}"),
            }),
        }
    }
}

/// `baseline_apply_state` 的只读快照。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaselineApplyState {
    pub checkpoint_id: [u8; 16],
    pub baseline_s: u64,
    pub source_cursor: String,
    pub status: BaselineApplyStatus,
    pub applied_docs: u64,
    pub applied_assets: u64,
    pub kept_local_edit: u64,
    /// begin 记录的预期文档数；没有记录（旧 journal/损坏）时为 `None`，此时不可 finish。
    pub expected_docs: Option<u64>,
    /// begin 记录的预期资产数；同上。
    pub expected_assets: Option<u64>,
    /// staging 且 begin 记录的预期集合已全部 `applied=1` 时为 true；finish 还要求 cursor CAS 通过。
    pub finishable: bool,
    /// 所选基线自带的 relay cursor；是否真正写进 `meta` 取决于 finish 的单调判断。
    pub chosen_cursor: Option<String>,
}

/// `finish_baseline_apply` 的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BaselineApplyFinish {
    /// 成功推进 cursor；携带 completed 状态。
    Applied(BaselineApplyState),
    NotFound,
    /// begin 记录的水位/游标已被并发改写、S 不符、或预期参与集合未全部应用；
    /// 未修改任何行。
    Conflict,
}

/// `begin_baseline_apply` 记录的**预期参与集合**：本 checkpoint 覆盖的全部文档与资产。
///
/// 为什么需要：journal 只记录「已应用了多少」，不记录「本来应该应用哪些」。少跑一批
/// （进程被杀、调用方漏批、fallback 选错集）时旧实现仍能 finish 并推进 S，随后普通 pull
/// 会把这段前缀当成已消费，**永久跳过其中所有 relay 事件**。
///
/// 集合以单条 [`META_BASELINE_EXPECTED`] `meta` 行持久化（不新增列/表、不参与同步、不是
/// wire）；`new` 会排序去重，因此同一集合的不同传入顺序被视为同一份预期。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BaselineExpectation {
    docs: Vec<String>,
    assets: Vec<[u8; 32]>,
}

impl BaselineExpectation {
    /// 规范化（排序 + 去重）后构造。
    pub fn new(
        docs: impl IntoIterator<Item = String>,
        assets: impl IntoIterator<Item = [u8; 32]>,
    ) -> Self {
        let mut docs: Vec<String> = docs.into_iter().collect();
        docs.sort_unstable();
        docs.dedup();
        let mut assets: Vec<[u8; 32]> = assets.into_iter().collect();
        assets.sort_unstable();
        assets.dedup();
        Self { docs, assets }
    }

    /// 预期的文档 id（已排序去重）。
    pub fn docs(&self) -> &[String] {
        &self.docs
    }

    /// 预期的资产 object_id（已排序去重）。
    pub fn assets(&self) -> &[[u8; 32]] {
        &self.assets
    }

    /// 预期文档数。
    pub fn doc_count(&self) -> u64 {
        u64::try_from(self.docs.len()).unwrap_or(u64::MAX)
    }

    /// 预期资产数。
    pub fn asset_count(&self) -> u64 {
        u64::try_from(self.assets.len()).unwrap_or(u64::MAX)
    }

    /// 集合是否为空（空集合视为"begin 已显式声明无需应用任何项"）。
    pub fn is_empty(&self) -> bool {
        self.docs.is_empty() && self.assets.is_empty()
    }

    /// 编码成单条 `meta` 值：`magic | doc_count | (len,doc)* | asset_count | oid*`（全部 BE）。
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(
            EXPECTED_MAGIC.len()
                + 16
                + self.docs.iter().map(|doc| 8 + doc.len()).sum::<usize>()
                + self.assets.len() * 32,
        );
        out.extend_from_slice(EXPECTED_MAGIC);
        out.extend_from_slice(&self.doc_count().to_be_bytes());
        for doc in &self.docs {
            let len = u64::try_from(doc.len()).unwrap_or(u64::MAX);
            out.extend_from_slice(&len.to_be_bytes());
            out.extend_from_slice(doc.as_bytes());
        }
        out.extend_from_slice(&self.asset_count().to_be_bytes());
        for asset in &self.assets {
            out.extend_from_slice(asset);
        }
        out
    }

    /// 解码并校验 `encode` 的输出；任何不合法都视为本地 meta 损坏（fail-closed）。
    fn decode(bytes: &[u8]) -> Result<Self, StorageError> {
        let mut offset = 0usize;
        let magic = take_expected_bytes(bytes, &mut offset, EXPECTED_MAGIC.len())?;
        if magic != EXPECTED_MAGIC {
            return Err(corrupt_expected("魔数不符"));
        }
        let doc_count = read_expected_u64(bytes, &mut offset)?;
        let doc_count =
            usize::try_from(doc_count).map_err(|_| corrupt_expected("doc 数超过 usize"))?;
        // 损坏数据不得预分配过大内存。
        let mut docs = Vec::with_capacity(doc_count.min(64));
        for _ in 0..doc_count {
            let len = read_expected_u64(bytes, &mut offset)?;
            let len = usize::try_from(len).map_err(|_| corrupt_expected("doc 长度超过 usize"))?;
            let raw = take_expected_bytes(bytes, &mut offset, len)?;
            let doc = std::str::from_utf8(raw)
                .map_err(|_| corrupt_expected("doc_id 不是 UTF-8"))?
                .to_owned();
            docs.push(doc);
        }
        let asset_count = read_expected_u64(bytes, &mut offset)?;
        let asset_count =
            usize::try_from(asset_count).map_err(|_| corrupt_expected("资产数超过 usize"))?;
        let mut assets = Vec::with_capacity(asset_count.min(64));
        for _ in 0..asset_count {
            let raw = take_expected_bytes(bytes, &mut offset, 32)?;
            let object_id: [u8; 32] = raw
                .try_into()
                .map_err(|_| corrupt_expected("object_id 长度非法"))?;
            assets.push(object_id);
        }
        if offset != bytes.len() {
            return Err(corrupt_expected("尾部有多余字节"));
        }
        Ok(Self { docs, assets })
    }
}

/// 一个已验证的基线资产（HMAC oid 是 relay 对象名；明文 hash 是真实 sha256）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaselineAsset {
    pub object_id: [u8; 32],
    pub asset_hash: [u8; 32],
    pub byte_len: u64,
    pub media_type: Option<String>,
    pub data: Vec<u8>,
}

struct ApplyJournal {
    checkpoint_id: [u8; 16],
    baseline_s: u64,
    source_cursor: String,
    status: BaselineApplyStatus,
    expected_received_seq: u64,
    expected_integrated_seq: u64,
    expected_confirmed_highwater: Option<u64>,
    expected_relay_cursor: Option<Vec<u8>>,
    applied_docs: u64,
    applied_assets: u64,
    kept_local_edit: u64,
    chosen_cursor: Option<String>,
}

struct PreparedDoc {
    doc_id: String,
    snapshot_digest: [u8; 32],
    baseline_vv: Vec<u8>,
    expected_head: Option<HeadState>,
    update: Option<PendingUpdate>,
    /// 读到的旧「远端已知 VV」（值 CAS 的期望值；`None` = 读时不存在）。
    ///
    /// 写入必须走 CAS（P2-5）：本批准备期间，普通 pull 的 A 事务完全可能已经把同一个
    /// `sync.remote_known_vv.<doc_id>` 扩大；无条件覆盖会把它退回到本批的并集（丢失
    /// 远端已知证据 → tombstone 误判为 local-only，只会保守保留，但证据丢了）。
    remote_known_expected: Option<VersionVector>,
    /// 合并后的远端已知 VV（写入 `sync.remote_known_vv.<doc_id>`）。
    remote_known_merged: VersionVector,
}
struct PreparedTombstone {
    doc_id: String,
    action: &'static str,
}

struct PreparedAsset {
    object_id: [u8; 32],
    asset_hash: [u8; 32],
    byte_len: u64,
    statement: Option<Statement>,
}

/// 开始一次 baseline apply staging；同参数（含预期参与集合）重复调用幂等，冲突参数返回
/// SyncConflict。
///
/// `expected` 必须覆盖**本 checkpoint 的全部** docs 与资产：finish 前会拿它逐项核对
/// `baseline_apply_docs/applied=1` 与 `baseline_apply_assets/applied=1`（见
/// [`BaselineExpectation`]）。同一 checkpoint/S/cursor 但 `expected` 不同 → 冲突，
/// 不会静默沿用旧集合。
pub fn begin_baseline_apply(
    vault: &Vault,
    checkpoint_id: &[u8; 16],
    baseline_s: u64,
    source_cursor: &str,
    expected: &BaselineExpectation,
) -> Result<BaselineApplyState, StorageError> {
    let now = now_secs()?;
    let expected_bytes = expected.encode();
    let existing = load_journal(vault)?;
    if let Some(existing) = &existing {
        if existing.status == BaselineApplyStatus::Staging
            && existing.checkpoint_id == *checkpoint_id
            && existing.baseline_s == baseline_s
            && existing.source_cursor == source_cursor
        {
            // 同参数重入必须幂等（崩溃续跑路径）；但预期集合必须对得上：
            // 旧 journal 没有集合记录、或调用方换了集合，都不能静默继续。
            let stored = vault.get_meta(META_BASELINE_EXPECTED)?;
            if stored.as_deref() != Some(expected_bytes.as_slice()) {
                return Err(StorageError::SyncConflict {
                    context: "同一 checkpoint 的 baseline apply staging 已有另一份预期参与集合"
                        .to_owned(),
                });
            }
            return state_from_journal(vault, existing);
        }
        if existing.status == BaselineApplyStatus::Staging {
            return Err(StorageError::SyncConflict {
                context: "已有另一份 baseline apply staging".to_owned(),
            });
        }
    }

    let cursor = sync_cursor_state(vault)?;
    let relay_cursor = vault.get_meta(META_SYNC_RELAY_CURSOR)?;

    let mut statements = Vec::new();
    let mut guards = Vec::new();
    // 旧 journal 子表的「远端已知 VV」证据回填 + 清理。
    //
    // `baseline_apply_docs.baseline_vv` 是**跨 journal 仍然有效**的「远端已知 VV」
    // 证据：必须在删除前落到非破坏性本地 meta 里，否则已有设备下一次快照追赶时会把
    // 远端已删文档误判成 local-only（只会保守保留，但证据丢了）。
    //
    // **无条件执行**（P2-5）：走到这里只可能是「没有 journal」或「completed journal」
    // （staging 同参已幂等早退、异参已 SyncConflict 返回）。completed journal 的
    // baseline_vv 要回填；没有 journal 但残留子表（旧版本/异常写留下的孤儿行）也必须
    // 先回填再清掉，否则下面无条件 `DELETE` 会把证据静默丢弃。
    //
    // `DO NOTHING` 是单调断言：已有 meta（普通 pull/integrate 记录的更新的远端已知 VV）
    // 永远不降级；缺失时才由旧 baseline_vv 补上。
    statements.push(Statement::new(
        format!(
            "INSERT INTO meta(key,value) \
             SELECT '{}' || doc_id, baseline_vv FROM baseline_apply_docs WHERE applied=1 \
             ON CONFLICT(key) DO NOTHING",
            remote_known::META_REMOTE_KNOWN_VV_PREFIX
        ),
        Vec::new(),
    ));
    // 旧 journal 子表只属于旧 journal（completed 或孤儿残留），一律清掉，
    // 避免它们被下一轮 `apply_baseline_batch` 当成「已应用」。
    statements.push(Statement::new(
        "DELETE FROM baseline_apply_docs",
        Vec::new(),
    ));
    statements.push(Statement::new(
        "DELETE FROM baseline_apply_tombstones",
        Vec::new(),
    ));
    statements.push(Statement::new(
        "DELETE FROM baseline_apply_assets",
        Vec::new(),
    ));
    statements.push(Statement::new("DELETE FROM baseline_apply", Vec::new()));
    let insert_index = statements.len();
    statements.push(Statement::new(
        "INSERT INTO baseline_apply(\
           singleton,checkpoint_id,baseline_s,source_cursor,status,\
           expected_received_seq,expected_integrated_seq,expected_confirmed_highwater,\
           expected_relay_cursor,applied_docs,applied_assets,kept_local_edit,chosen_cursor,\
           created_at,updated_at\
         ) VALUES(1,?1,?2,?3,'staging',?4,?5,?6,?7,0,0,0,NULL,?8,?8)",
        vec![
            Value::Blob(checkpoint_id.to_vec()),
            Value::Integer(i64_of(baseline_s)?),
            Value::Text(source_cursor.to_owned()),
            Value::Integer(i64_of(cursor.received_seq)?),
            Value::Integer(i64_of(cursor.integrated_seq)?),
            opt_u64(cursor.confirmed_highwater)?,
            opt_blob(relay_cursor),
            Value::Integer(now),
        ],
    ));
    guards.push(AffectedGuard::exactly(insert_index, 1));

    // 预期参与集合随 journal 同事务落 meta（replace 语义：completed journal 被替换时
    // 旧集合必须一起被覆盖，否则 finish 会拿上一轮的集合核对本轮进度）。
    let expected_index = statements.len();
    statements.push(Statement::new(
        "INSERT INTO meta(key,value) VALUES(?1,?2) \
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        vec![
            Value::Text(META_BASELINE_EXPECTED.to_owned()),
            Value::Blob(expected_bytes),
        ],
    ));
    guards.push(AffectedGuard::exactly(expected_index, 1));

    match vault.exec_transaction(&statements, &guards) {
        Ok(_) => {}
        Err(StorageError::GuardFailed { .. }) => {
            return Err(StorageError::SyncConflict {
                context: "baseline apply staging 已被其它写入占用".to_owned(),
            });
        }
        Err(error) => return Err(error),
    }
    baseline_apply_state(vault)?.ok_or_else(|| StorageError::Corrupt {
        context: "begin_baseline_apply 后 journal 不存在".to_owned(),
    })
}

/// 当前 staging journal 的只读状态；没有 journal 返回 `None`。
pub fn baseline_apply_state(vault: &Vault) -> Result<Option<BaselineApplyState>, StorageError> {
    match load_journal(vault)? {
        None => Ok(None),
        Some(journal) => state_from_journal(vault, &journal).map(Some),
    }
}

/// 应用一批已验证的快照文档和远端 tombstone。
pub fn apply_baseline_batch(
    vault: &Vault,
    docs: &[(String, Vec<u8>)],
    tombstones: &[String],
) -> Result<BaselineApplyState, StorageError> {
    let journal = load_active_journal(vault)?;
    if docs.is_empty() && tombstones.is_empty() {
        return state_from_journal(vault, &journal);
    }

    let mut seen_docs = BTreeSet::new();
    for (doc_id, _) in docs {
        if !seen_docs.insert(doc_id.clone()) {
            return Err(StorageError::Corrupt {
                context: format!("baseline batch 重复 doc {doc_id}"),
            });
        }
    }
    let mut seen_tombstones = BTreeSet::new();
    for doc_id in tombstones {
        if !seen_tombstones.insert(doc_id.clone()) {
            return Err(StorageError::Corrupt {
                context: format!("baseline batch 重复 tombstone {doc_id}"),
            });
        }
    }

    let applied_docs = load_applied_doc_digests(vault)?;
    let mut prepared_docs = Vec::new();
    let mut baseline_by_doc: BTreeMap<String, bool> = BTreeMap::new();
    let batch_started = Instant::now();
    debug_add_count(|c| &mut c.batches, 1);
    for (doc_id, snapshot) in docs {
        debug_add_count(|c| &mut c.docs, 1);
        if snapshot.is_empty() {
            return Err(StorageError::Corrupt {
                context: format!("baseline 文档 {doc_id} 快照为空"),
            });
        }
        let digest: [u8; 32] = Sha256::digest(snapshot).into();

        // 幂等重放：同一快照已应用过。**不再解码/导入 snapshot**，直接用本批已记录的
        // baseline VV 判断 tombstone 需要的 local_extra。
        if let Some(old_digest) = applied_docs.get(doc_id) {
            if old_digest != &digest {
                return Err(StorageError::SyncConflict {
                    context: format!("baseline 文档 {doc_id} 已用另一份快照应用"),
                });
            }
            let baseline_vv = load_applied_baseline_vv(vault, doc_id)?;
            let lookup_started = Instant::now();
            let local_extra = local_has_extra(vault, doc_id, &baseline_vv)?;
            debug_add_time(|t| &mut t.head_lookup_ns, lookup_started);
            debug_add_count(|c| &mut c.replayed_same_digest, 1);
            baseline_by_doc.insert(doc_id.clone(), local_extra);
            continue;
        }

        // snapshot 只解码/导入这一遍：后续所有派生都基于这个 `baseline_doc`。
        let restore_started = Instant::now();
        let baseline_doc = BodyDoc::restore(doc_id.clone(), BASELINE_PEER_ID, Some(snapshot), &[])?;
        debug_add_time(|t| &mut t.restore_ns, restore_started);
        debug_add_count(|c| &mut c.snapshot_imports, 1);
        debug_add_count(|c| &mut c.snapshot_bytes, snapshot.len() as u64);
        let baseline_vv = baseline_doc.vv();

        // 仅取 VV 的地方一律用 O(1) 的 load_head，不重放整篇文档。
        let lookup_started = Instant::now();
        let exists = crate::read_api::doc_exists(vault, doc_id)?;
        let (local_extra, head, local_vv) = if exists {
            let head = load_head(vault, doc_id)?;
            let local_vv = head.version_vector()?;
            let local_extra = local_vv.sub_iter(&baseline_vv).next().is_some();
            (local_extra, Some(head), Some(local_vv))
        } else {
            (false, None, None)
        };
        debug_add_time(|t| &mut t.head_lookup_ns, lookup_started);

        let mut update = None;
        let mut expected_head = None;
        if let (Some(head), Some(local_vv)) = (head, local_vv) {
            if !local_extra {
                debug_add_count(|c| &mut c.existing_no_extra, 1);
                if baseline_vv != local_vv {
                    let export_started = Instant::now();
                    let pending = baseline_doc.pending_update(&local_vv)?;
                    debug_add_time(|t| &mut t.pending_update_ns, export_started);
                    debug_add_count(|c| &mut c.pending_update_calls, 1);
                    debug_add_count(
                        |c| &mut c.pending_update_bytes,
                        pending.payload.len() as u64,
                    );
                    if !pending.payload.is_empty() {
                        update = Some(pending);
                    }
                }
                expected_head = Some(head);
            } else {
                debug_add_count(|c| &mut c.existing_local_extra, 1);
                // 只重放一次本地副本，导出**极小**的本地增量；绝不把大 snapshot 再导入一遍。
                let load_started = Instant::now();
                let (local_doc, loaded_head) =
                    load_body_doc(vault, doc_id, BASELINE_PEER_ID, None)?;
                debug_add_time(|t| &mut t.load_body_doc_ns, load_started);
                debug_add_count(|c| &mut c.load_body_doc_calls, 1);
                // 以本次重放读到的 head/VV 为准：若 load_head 之后有并发写，CAS 与导出基准
                // 都跟着最新值走，不 panic、也不写错 head。
                let local_vv = loaded_head.version_vector()?;
                let local_delta_started = Instant::now();
                let local_delta = local_doc.pending_update(&baseline_vv)?;
                debug_add_time(|t| &mut t.local_delta_export_ns, local_delta_started);
                debug_add_count(
                    |c| &mut c.local_delta_bytes,
                    local_delta.payload.len() as u64,
                );
                let import_started = Instant::now();
                if !local_delta.payload.is_empty() {
                    // 把本地极小增量反向合进基线副本，得到 merged 状态。
                    // 只合 oplog（detached）：attached import 会把大文本重新应用到 state，
                    // 成本随总文本长度超线性增长；这里只需要 oplog 并集。
                    baseline_doc.import_oplog_only(&local_delta.payload)?;
                }
                debug_add_time(|t| &mut t.local_delta_import_ns, import_started);
                let export_started = Instant::now();
                let pending = baseline_doc.pending_update_from_oplog(&local_vv)?;
                debug_add_time(|t| &mut t.pending_update_ns, export_started);
                debug_add_count(|c| &mut c.pending_update_calls, 1);
                debug_add_count(
                    |c| &mut c.pending_update_bytes,
                    pending.payload.len() as u64,
                );
                if !pending.payload.is_empty() {
                    update = Some(pending);
                }
                expected_head = Some(loaded_head);
            }
        } else {
            // 新文档：直接用已 restore 的 baseline_doc 导出相对空 VV 的全量历史。
            debug_add_count(|c| &mut c.new_docs, 1);
            let export_started = Instant::now();
            let pending = baseline_doc.pending_update(&VersionVector::default())?;
            debug_add_time(|t| &mut t.pending_update_ns, export_started);
            debug_add_count(|c| &mut c.pending_update_calls, 1);
            debug_add_count(
                |c| &mut c.pending_update_bytes,
                pending.payload.len() as u64,
            );
            if !pending.payload.is_empty() {
                update = Some(pending);
            }
        }

        let remote_known_expected = remote_known::load(vault, doc_id)?;
        let remote_known_merged = match &remote_known_expected {
            Some(existing) => remote_known::union(existing, &baseline_vv),
            None => baseline_vv.clone(),
        };
        prepared_docs.push(PreparedDoc {
            doc_id: doc_id.clone(),
            snapshot_digest: digest,
            baseline_vv: baseline_vv.encode(),
            expected_head,
            update,
            remote_known_expected,
            remote_known_merged,
        });
        baseline_by_doc.insert(doc_id.clone(), local_extra);
    }

    let mut prepared_tombstones = Vec::new();
    for doc_id in tombstones {
        let action = if tombstone_can_delete(vault, doc_id, baseline_by_doc.get(doc_id).copied())? {
            "deleted"
        } else {
            "kept_local_edit"
        };
        prepared_tombstones.push(PreparedTombstone {
            doc_id: doc_id.clone(),
            action,
        });
    }

    let now = now_secs()?;
    let mut statements = Vec::new();
    let mut guards = Vec::new();
    // 远端已知 VV 的 CAS 守卫下标 → doc_id：GuardFailed 时用它给出明确冲突错误。
    let mut remote_known_guards: Vec<(usize, String)> = Vec::new();
    for prepared in &prepared_docs {
        let journal_index = statements.len();
        statements.push(Statement::new(
            "INSERT INTO baseline_apply_docs(doc_id,snapshot_digest,baseline_vv,applied,updated_at) \
             VALUES(?1,?2,?3,1,?4) \
             ON CONFLICT(doc_id) DO UPDATE SET \
               snapshot_digest=excluded.snapshot_digest,baseline_vv=excluded.baseline_vv,applied=1,updated_at=excluded.updated_at",
            vec![
                Value::Text(prepared.doc_id.clone()),
                Value::Blob(prepared.snapshot_digest.to_vec()),
                Value::Blob(prepared.baseline_vv.clone()),
                Value::Integer(now),
            ],
        ));
        guards.push(AffectedGuard::exactly(journal_index, 1));

        // 跨 journal 保留的「远端已知 VV」：本批快照的 VV 就是远端已见过的 ops。
        // 与旧值取单调并集（旧值可能来自普通 pull/integrate 的更新远端已知集），
        // 但写入必须是**值 CAS**：只有库中值仍是准备阶段读到的旧值才更新。
        // 并发写者（普通 pull 的 A 事务）已经扩大过 → 0 行 → 整批回滚并报明确冲突，
        // 由调用方重试；绝不把更大的值覆盖回本批的并集（P2-5）。
        let remote_index = statements.len();
        statements.push(remote_known::cas_statement(
            &prepared.doc_id,
            prepared.remote_known_expected.as_ref(),
            &prepared.remote_known_merged,
        ));
        guards.push(AffectedGuard::exactly(remote_index, 1));
        remote_known_guards.push((remote_index, prepared.doc_id.clone()));

        let Some(update) = &prepared.update else {
            if prepared.expected_head.is_none() {
                // 新文档但快照没有 op：仍建行，保持空文档语义。
                statements.push(Statement::new(
                    "INSERT INTO loro_docs(doc_id,kind,generation,app_doc_schema,codec_version,min_reader,head_seq) \
                     VALUES(?1,'body',1,?2,?3,?4,0)",
                    vec![
                        Value::Text(prepared.doc_id.clone()),
                        Value::Integer(APP_DOC_SCHEMA),
                        Value::Text(CODEC_VERSION.to_owned()),
                        Value::Integer(MIN_READER),
                    ],
                ));
                guards.push(AffectedGuard::exactly(statements.len() - 1, 1));
            }
            continue;
        };

        if let Some(head) = &prepared.expected_head {
            let seq = head.head_seq.saturating_add(1);
            let cas_index = statements.len();
            statements.push(Statement::new(
                "UPDATE loro_docs SET head_seq=?2,head_frontier=?3,head_vv=?4 \
                 WHERE doc_id=?1 AND head_seq=?5",
                vec![
                    Value::Text(prepared.doc_id.clone()),
                    Value::Integer(i64_of(seq)?),
                    Value::Blob(update.end_frontier.clone()),
                    Value::Blob(update.end_vv.clone()),
                    Value::Integer(i64_of(head.head_seq)?),
                ],
            ));
            guards.push(AffectedGuard::exactly(cas_index, 1));
            let insert_index = statements.len();
            statements.push(Statement::new(
                "INSERT INTO loro_updates(doc_id,seq,update_id,batch_id,codec_version,payload,end_frontier,end_vv) \
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                vec![
                    Value::Text(prepared.doc_id.clone()),
                    Value::Integer(i64_of(seq)?),
                    Value::Text(format!("{}#{seq}", prepared.doc_id)),
                    Value::Text(BASELINE_MERGE_BATCH.to_owned()),
                    Value::Text(CODEC_VERSION.to_owned()),
                    Value::Blob(update.payload.clone()),
                    Value::Blob(update.end_frontier.clone()),
                    Value::Blob(update.end_vv.clone()),
                ],
            ));
            guards.push(AffectedGuard::exactly(insert_index, 1));
            let outbox_index = statements.len();
            statements.push(Statement::new(
                "INSERT INTO projection_outbox(doc_id,target_seq) VALUES(?1,?2) \
                 ON CONFLICT(doc_id) DO UPDATE SET target_seq=MAX(target_seq,excluded.target_seq)",
                vec![
                    Value::Text(prepared.doc_id.clone()),
                    Value::Integer(i64_of(seq)?),
                ],
            ));
            guards.push(AffectedGuard::exactly(outbox_index, 1));
        } else {
            let kind = if prepared.doc_id == crate::structure::STRUCTURE_DOC_ID {
                "structure"
            } else {
                "body"
            };
            let insert_index = statements.len();
            statements.push(Statement::new(
                "INSERT INTO loro_docs(doc_id,kind,generation,app_doc_schema,codec_version,min_reader,head_seq,head_frontier,head_vv) \
                 VALUES(?1,?2,1,?3,?4,?5,1,?6,?7)",
                vec![
                    Value::Text(prepared.doc_id.clone()),
                    Value::Text(kind.to_owned()),
                    Value::Integer(APP_DOC_SCHEMA),
                    Value::Text(CODEC_VERSION.to_owned()),
                    Value::Integer(MIN_READER),
                    Value::Blob(update.end_frontier.clone()),
                    Value::Blob(update.end_vv.clone()),
                ],
            ));
            guards.push(AffectedGuard::exactly(insert_index, 1));
            let update_index = statements.len();
            statements.push(Statement::new(
                "INSERT INTO loro_updates(doc_id,seq,update_id,batch_id,codec_version,payload,end_frontier,end_vv) \
                 VALUES(?1,1,?2,?3,?4,?5,?6,?7)",
                vec![
                    Value::Text(prepared.doc_id.clone()),
                    Value::Text(format!("{}#1", prepared.doc_id)),
                    Value::Text(BASELINE_MERGE_BATCH.to_owned()),
                    Value::Text(CODEC_VERSION.to_owned()),
                    Value::Blob(update.payload.clone()),
                    Value::Blob(update.end_frontier.clone()),
                    Value::Blob(update.end_vv.clone()),
                ],
            ));
            guards.push(AffectedGuard::exactly(update_index, 1));
            let outbox_index = statements.len();
            statements.push(Statement::new(
                "INSERT INTO projection_outbox(doc_id,target_seq) VALUES(?1,1) \
                 ON CONFLICT(doc_id) DO UPDATE SET target_seq=MAX(target_seq,excluded.target_seq)",
                vec![Value::Text(prepared.doc_id.clone())],
            ));
            guards.push(AffectedGuard::exactly(outbox_index, 1));
        }
    }

    for tombstone in &prepared_tombstones {
        // P1-2（TOCTOU）：`tombstone_can_delete` 在事务外读 outbox，判定完成后本地编辑的
        // A 事务完全可能插进一条 `sync_outbox` 行；若仍按旧判定无条件软删，就会把刚编辑
        // 的笔记删掉。因此最终 action 必须在**同一个写事务里**用 SQL 重判一次：
        // 只允许 `deleted → kept_local_edit` 的降级，绝不允许 `kept → deleted` 升级。
        let index = statements.len();
        statements.push(Statement::new(
            "INSERT INTO baseline_apply_tombstones(doc_id,action,updated_at) \
             VALUES(?1, CASE WHEN EXISTS(SELECT 1 FROM sync_outbox WHERE doc_id=?1) \
                             THEN 'kept_local_edit' ELSE ?2 END, ?3) \
             ON CONFLICT(doc_id) DO UPDATE SET \
               action=CASE WHEN EXISTS(SELECT 1 FROM sync_outbox WHERE doc_id=?1) \
                           THEN 'kept_local_edit' ELSE excluded.action END, \
               updated_at=excluded.updated_at",
            vec![
                Value::Text(tombstone.doc_id.clone()),
                Value::Text(tombstone.action.to_owned()),
                Value::Integer(now),
            ],
        ));
        // 这一句永远恰好写入 1 行（INSERT 或 DO UPDATE），是"判定已落库"的守卫：
        // 说明事务内重判真的执行了，而不是被别的写入/约束挡掉。
        guards.push(AffectedGuard::exactly(index, 1));

        if tombstone.action == "deleted" {
            // 软删与 outbox 判定在同一事务内：只有重判仍为 `deleted`（且此刻没有
            // outbox）才置 deleted=1。0 行是合法结果（该文档没有 notes 行，或已降级为
            // kept_local_edit），所以这句本身不设 exactly 守卫；守卫语义在上一条。
            statements.push(Statement::new(
                "UPDATE notes SET deleted=1 \
                 WHERE body_doc_id=?1 \
                   AND NOT EXISTS(SELECT 1 FROM sync_outbox WHERE doc_id=?1) \
                   AND EXISTS(SELECT 1 FROM baseline_apply_tombstones \
                              WHERE doc_id=?1 AND action='deleted')",
                vec![Value::Text(tombstone.doc_id.clone())],
            ));
        }
    }

    let journal_index = statements.len();
    statements.push(Statement::new(
        "UPDATE baseline_apply SET \
           applied_docs=(SELECT COUNT(*) FROM baseline_apply_docs WHERE applied=1), \
           applied_assets=(SELECT COUNT(*) FROM baseline_apply_assets WHERE applied=1), \
           kept_local_edit=(SELECT COUNT(*) FROM baseline_apply_tombstones WHERE action='kept_local_edit'), \
           updated_at=?1 \
         WHERE singleton=1 AND status='staging'",
        vec![Value::Integer(now)],
    ));
    guards.push(AffectedGuard::exactly(journal_index, 1));

    let transaction_started = Instant::now();
    match vault.exec_transaction(&statements, &guards) {
        Ok(_) => {}
        Err(StorageError::GuardFailed { statement, .. }) => {
            if let Some((_, doc_id)) = remote_known_guards
                .iter()
                .find(|(index, _)| *index == statement)
            {
                // 值 CAS 不命中：准备阶段读到的旧值已被普通 pull 的 A 事务改写。
                // 整批已回滚（update/head/journal 都没有落库），重跑本批即可读到并集。
                return Err(StorageError::SyncConflict {
                    context: format!(
                        "baseline 文档 {doc_id} 的远端已知 VV 值 CAS 失败：\
                         并发写者已改写旧值；整批已回滚，可重试"
                    ),
                });
            }
            return Err(StorageError::SyncConflict {
                context: "baseline batch 期间 journal/doc head 已变化".to_owned(),
            });
        }
        Err(error) => return Err(error),
    }
    debug_add_time(|t| &mut t.transaction_ns, transaction_started);
    debug_add_time(|t| &mut t.batch_total_ns, batch_started);
    baseline_apply_state(vault)?.ok_or_else(|| StorageError::Corrupt {
        context: "apply_baseline_batch 后 journal 不存在".to_owned(),
    })
}

/// 应用一批已验证的基线资产；同一 HMAC oid/真实 hash 重复应用幂等。
pub fn apply_baseline_assets(
    vault: &Vault,
    assets: &[BaselineAsset],
) -> Result<BaselineApplyState, StorageError> {
    let journal = load_active_journal(vault)?;
    if assets.is_empty() {
        return state_from_journal(vault, &journal);
    }

    let mut seen_oid = BTreeSet::new();
    let mut seen_hash = BTreeSet::new();
    let mut prepared = Vec::with_capacity(assets.len());
    for asset in assets {
        if asset.data.is_empty() {
            return Err(StorageError::Corrupt {
                context: "baseline 资产明文为空".to_owned(),
            });
        }
        if asset.byte_len != asset.data.len() as u64 {
            return Err(StorageError::Corrupt {
                context: format!(
                    "baseline 资产 {} byte_len={} 与 data.len={} 不一致",
                    hex32(&asset.object_id),
                    asset.byte_len,
                    asset.data.len()
                ),
            });
        }
        if !seen_oid.insert(asset.object_id) {
            return Err(StorageError::Corrupt {
                context: format!("baseline batch 重复 object_id {}", hex32(&asset.object_id)),
            });
        }
        if !seen_hash.insert(asset.asset_hash) {
            return Err(StorageError::Corrupt {
                context: format!(
                    "baseline batch 重复 asset_hash {}",
                    hex32(&asset.asset_hash)
                ),
            });
        }

        if let Some((old_hash, old_len)) = load_applied_asset(vault, &asset.object_id)? {
            if old_hash != asset.asset_hash || old_len != asset.byte_len {
                return Err(StorageError::SyncConflict {
                    context: format!(
                        "baseline 资产 object {} 已绑定另一份资产",
                        hex32(&asset.object_id)
                    ),
                });
            }
            continue;
        }

        let statement = prepare_asset_statement(vault, asset)?;
        prepared.push(PreparedAsset {
            object_id: asset.object_id,
            asset_hash: asset.asset_hash,
            byte_len: asset.byte_len,
            statement,
        });
    }

    let now = now_secs()?;
    let mut statements = Vec::new();
    let mut guards = Vec::new();
    for asset in &prepared {
        if let Some(statement) = &asset.statement {
            let index = statements.len();
            statements.push(statement.clone());
            guards.push(AffectedGuard::exactly(index, 1));
        }
        let index = statements.len();
        statements.push(Statement::new(
            "INSERT INTO baseline_apply_assets(object_id,asset_hash,byte_len,applied,updated_at) \
             VALUES(?1,?2,?3,1,?4) \
             ON CONFLICT(object_id) DO UPDATE SET \
               asset_hash=excluded.asset_hash,byte_len=excluded.byte_len,applied=1,updated_at=excluded.updated_at",
            vec![
                Value::Blob(asset.object_id.to_vec()),
                Value::Blob(asset.asset_hash.to_vec()),
                Value::Integer(i64_of(asset.byte_len)?),
                Value::Integer(now),
            ],
        ));
        guards.push(AffectedGuard::exactly(index, 1));
    }
    let journal_index = statements.len();
    statements.push(Statement::new(
        "UPDATE baseline_apply SET \
           applied_docs=(SELECT COUNT(*) FROM baseline_apply_docs WHERE applied=1), \
           applied_assets=(SELECT COUNT(*) FROM baseline_apply_assets WHERE applied=1), \
           kept_local_edit=(SELECT COUNT(*) FROM baseline_apply_tombstones WHERE action='kept_local_edit'), \
           updated_at=?1 \
         WHERE singleton=1 AND status='staging'",
        vec![Value::Integer(now)],
    ));
    guards.push(AffectedGuard::exactly(journal_index, 1));

    match vault.exec_transaction(&statements, &guards) {
        Ok(_) => {}
        Err(StorageError::GuardFailed { .. }) => {
            return Err(StorageError::SyncConflict {
                context: "baseline assets 写入期间 journal 已变化".to_owned(),
            });
        }
        Err(error) => return Err(error),
    }
    baseline_apply_state(vault)?.ok_or_else(|| StorageError::Corrupt {
        context: "apply_baseline_assets 后 journal 不存在".to_owned(),
    })
}

/// 全部批次完成后按**单调语义**推进水位与 relay cursor；CAS 失败返回 Conflict 且不写任何行。
///
/// 评审 0002 P1-4：bootstrap/rotation 会作用在**已有库**上，所选基线 S 可能低于本库已有
/// 水位。因此这里只允许前进（[`sync_cursor_state`] 的字段语义）：
///
/// - `received/integrated/confirmed_highwater = max(现有, S)`；
/// - relay cursor 只在 `S >= 现有 integrated`（基线覆盖了已消费前缀）时切换为基线 cursor，
///   否则保持原游标，绝不把已消费位置回退；
/// - 本函数只写上述四个 `meta` 键与 `baseline_apply` journal。sync 侧的 pin
///   （instance/log_id/epoch/control rev+hash）由 sync 模块维护，这里不读不写，
///   所以不可能被降低；不能把它们搬进本函数。
///
/// CAS 仍是"begin 时记录、finish 时校验"：任一字段在 finish 前被并发改写即返回
/// [`BaselineApplyFinish::Conflict`]，整笔事务不写。
///
/// **完整性前提**（P1-1）：begin 记录在 [`META_BASELINE_EXPECTED`] 的预期参与集合必须
/// 全部 `applied=1`。少跑一批（哪怕 status 仍是 staging）返回 Conflict；旧 journal
/// 没有集合记录同样 Conflict（拿不到证明就不允许推进 S），绝不明知不完整还写水位。
pub fn finish_baseline_apply(
    vault: &Vault,
    chosen_baseline_s: u64,
    cursor: &str,
) -> Result<BaselineApplyFinish, StorageError> {
    let journal = match load_journal(vault)? {
        None => return Ok(BaselineApplyFinish::NotFound),
        Some(journal) => journal,
    };
    if journal.status != BaselineApplyStatus::Staging || journal.baseline_s != chosen_baseline_s {
        return Ok(BaselineApplyFinish::Conflict);
    }

    // 完整性校验在任何写语句之前：拿不到 begin 的预期集合，或集合未全齐，一律 Conflict。
    let Some(expected) = load_expected_set(vault)? else {
        return Ok(BaselineApplyFinish::Conflict);
    };
    if !expected_is_complete(vault, &journal, &expected)? {
        return Ok(BaselineApplyFinish::Conflict);
    }

    // finish 前先校验 begin 时记录的旧值；任何一项被并发改写都按 Conflict 处理，不写任何行。
    let current = sync_cursor_state(vault)?;
    let current_relay_cursor = vault.get_meta(META_SYNC_RELAY_CURSOR)?;
    if current.received_seq != journal.expected_received_seq
        || current.integrated_seq != journal.expected_integrated_seq
        || current.confirmed_highwater != journal.expected_confirmed_highwater
        || current_relay_cursor != journal.expected_relay_cursor
    {
        return Ok(BaselineApplyFinish::Conflict);
    }

    // 单调目标：已有水位高于基线 S 时保留已有值。
    let received_target = current.received_seq.max(chosen_baseline_s);
    let integrated_target = current.integrated_seq.max(chosen_baseline_s);
    let highwater_target = current
        .confirmed_highwater
        .unwrap_or(0)
        .max(chosen_baseline_s);
    // 基线 cursor 指向位置 S；只有它覆盖了已 integrate 的前缀才允许切换。
    let advance_cursor = chosen_baseline_s >= current.integrated_seq;

    let now = now_secs()?;
    let mut statements = Vec::new();
    let mut guards = Vec::new();
    push_meta_cas(
        &mut statements,
        &mut guards,
        META_SYNC_RECEIVED_SEQ,
        current.received_seq,
        &received_target.to_be_bytes(),
    );
    push_meta_cas(
        &mut statements,
        &mut guards,
        META_SYNC_INTEGRATED_SEQ,
        current.integrated_seq,
        &integrated_target.to_be_bytes(),
    );
    push_meta_cas(
        &mut statements,
        &mut guards,
        META_SYNC_CONFIRMED_HIGHWATER,
        current.confirmed_highwater.unwrap_or(0),
        &highwater_target.to_be_bytes(),
    );
    match (&current_relay_cursor, advance_cursor) {
        // 基线覆盖已消费前缀：切到基线 cursor。
        (Some(old), true) => push_meta_cas_bytes(
            &mut statements,
            &mut guards,
            META_SYNC_RELAY_CURSOR,
            old,
            cursor.as_bytes(),
        ),
        // 低 S：原样写回旧游标，既保持语义也保留 CAS（并发改写 → Conflict）。
        (Some(old), false) => push_meta_cas_bytes(
            &mut statements,
            &mut guards,
            META_SYNC_RELAY_CURSOR,
            old,
            old,
        ),
        (None, true) => push_meta_absent_insert(
            &mut statements,
            &mut guards,
            META_SYNC_RELAY_CURSOR,
            cursor.as_bytes(),
        ),
        // 低 S 且原本没有游标：保持缺失，并发插入同样按 Conflict 处理。
        (None, false) => {
            let index = statements.len();
            statements.push(Statement::new(
                "UPDATE meta SET value=value WHERE key=?1",
                vec![Value::Text(META_SYNC_RELAY_CURSOR.to_owned())],
            ));
            guards.push(AffectedGuard::exactly(index, 0));
        }
    }

    let journal_index = statements.len();
    statements.push(Statement::new(
        "UPDATE baseline_apply SET status='completed',chosen_cursor=?1,updated_at=?2 \
         WHERE singleton=1 AND status='staging'",
        vec![Value::Text(cursor.to_owned()), Value::Integer(now)],
    ));
    guards.push(AffectedGuard::exactly(journal_index, 1));

    match vault.exec_transaction(&statements, &guards) {
        Ok(_) => {}
        Err(StorageError::GuardFailed { .. }) => return Ok(BaselineApplyFinish::Conflict),
        Err(error) => return Err(error),
    }
    let state = baseline_apply_state(vault)?.ok_or_else(|| StorageError::Corrupt {
        context: "finish_baseline_apply 后 journal 不存在".to_owned(),
    })?;
    Ok(BaselineApplyFinish::Applied(state))
}

/// 只清 staging journal（含子表和预期参与集合）；已导入的 Loro update/资产保留。
pub fn abort_baseline_apply(vault: &Vault) -> Result<bool, StorageError> {
    let Some(journal) = load_journal(vault)? else {
        return Ok(false);
    };
    if journal.status != BaselineApplyStatus::Staging {
        return Ok(false);
    }
    let statements = [
        Statement::new("DELETE FROM baseline_apply_docs", Vec::new()),
        Statement::new("DELETE FROM baseline_apply_tombstones", Vec::new()),
        Statement::new("DELETE FROM baseline_apply_assets", Vec::new()),
        Statement::new(
            "DELETE FROM baseline_apply WHERE singleton=1 AND status='staging'",
            Vec::new(),
        ),
        // 预期集合只属于被清掉的 journal；留着会被下一次 begin 的 replace 覆盖，
        // 但清掉可以避免"没有 journal 却有预期集合"的中间态被误读。
        Statement::new(
            "DELETE FROM meta WHERE key=?1",
            vec![Value::Text(META_BASELINE_EXPECTED.to_owned())],
        ),
    ];
    match vault.exec_transaction(&statements, &[AffectedGuard::exactly(3, 1)]) {
        Ok(_) => Ok(true),
        Err(StorageError::GuardFailed { .. }) => Ok(false),
        Err(error) => Err(error),
    }
}

fn tombstone_can_delete(
    vault: &Vault,
    doc_id: &str,
    baseline_in_batch: Option<bool>,
) -> Result<bool, StorageError> {
    // 本地仍待同步的 outbox 行：无条件保留（可能是用户尚未上传的编辑）。
    let outbox_rows = vault.query(
        "SELECT count(*) FROM sync_outbox WHERE doc_id=?1",
        &[Value::Text(doc_id.to_owned())],
    )?;
    let has_outbox = outbox_rows
        .first()
        .and_then(|row| row.first())
        .and_then(|value| match value {
            Value::Integer(value) => Some(*value > 0),
            _ => None,
        })
        .unwrap_or(false);
    if has_outbox {
        return Ok(false);
    }

    if !crate::read_api::doc_exists(vault, doc_id)? {
        return Ok(true);
    }
    match baseline_in_batch {
        // 有快照且本地没有快照之外的 ops：可安全删除。
        Some(false) => return Ok(true),
        // 有快照且本地有快照之外的 ops：保留。
        Some(true) => return Ok(false),
        None => {}
    }

    // 没有本批快照：用**跨 journal 保留**的「远端已知 VV」判断本地有没有远端未知的 op。
    //
    // - 普通 pull/integrate 集成过、`head_seq>0`、`sync_outbox` 为空 → 本地 ops 全被
    //   远端已知 VV 覆盖 → `deleted`；
    // - local-only 文档（从未远程同步、无任何证据）→ 没有记录 → 保守保留；
    // - 有本地未上传/未被远端确认的 op → `local_vv` 超出已知集 → 保留。
    let head = load_head(vault, doc_id)?;
    let local_vv = head.version_vector()?;
    if let Some(known) = remote_known::load(vault, doc_id)? {
        let known_all = local_vv.sub_iter(&known).next().is_none();
        return Ok(known_all);
    }

    // 兜底：当前 journal 里已经应用过该 doc（例如 meta 写入之前的历史数据）。
    // 只读取 VV，不再重放整篇文档。
    if let Some(baseline) = load_applied_baseline_vv_opt(vault, doc_id)? {
        let known = local_vv.sub_iter(&baseline).next().is_none();
        return Ok(known);
    }

    // 完全没有证据：local-only 内容必须保留；只有从未有过 op 的空文档才可删除。
    Ok(head.head_seq == 0)
}

/// 读当前 journal 中某篇已应用文档的 baseline VV（幂等重放分支用；缺失视为损坏）。
fn load_applied_baseline_vv(vault: &Vault, doc_id: &str) -> Result<VersionVector, StorageError> {
    load_applied_baseline_vv_opt(vault, doc_id)?.ok_or_else(|| StorageError::Corrupt {
        context: format!("baseline_apply_docs 缺少已应用文档 {doc_id} 的 baseline_vv"),
    })
}

/// 读当前 journal 中某篇已应用文档的 baseline VV；没有该行返回 `None`。
fn load_applied_baseline_vv_opt(
    vault: &Vault,
    doc_id: &str,
) -> Result<Option<VersionVector>, StorageError> {
    let rows = vault.query(
        "SELECT baseline_vv FROM baseline_apply_docs WHERE doc_id=?1 AND applied=1",
        &[Value::Text(doc_id.to_owned())],
    )?;
    match rows.first().and_then(|row| row.first()) {
        Some(Value::Blob(baseline_vv)) => VersionVector::decode(baseline_vv)
            .map(Some)
            .map_err(StorageError::from),
        Some(other) => Err(StorageError::Corrupt {
            context: format!("baseline_apply_docs.baseline_vv 读回异常：{other:?}"),
        }),
        None => Ok(None),
    }
}

/// 本地文档是否有 baseline 之外的 op（只看 head VV，O(1)，不重放正文）。
fn local_has_extra(
    vault: &Vault,
    doc_id: &str,
    baseline_vv: &VersionVector,
) -> Result<bool, StorageError> {
    if !crate::read_api::doc_exists(vault, doc_id)? {
        return Ok(false);
    }
    let local_vv = load_head(vault, doc_id)?.version_vector()?;
    let extra = local_vv.sub_iter(baseline_vv).next().is_some();
    Ok(extra)
}

fn prepare_asset_statement(
    vault: &Vault,
    asset: &BaselineAsset,
) -> Result<Option<Statement>, StorageError> {
    let rows = vault.query(
        "SELECT byte_len,data,media_type FROM assets WHERE asset_hash=?1",
        &[Value::Blob(asset.asset_hash.to_vec())],
    )?;
    let Some(row) = rows.first() else {
        return Ok(Some(Statement::new(
            "INSERT INTO assets(asset_hash,byte_len,media_type,data,orphan_since) \
             VALUES(?1,?2,?3,?4,NULL)",
            vec![
                Value::Blob(asset.asset_hash.to_vec()),
                Value::Integer(i64_of(asset.byte_len)?),
                opt_text(asset.media_type.clone()),
                Value::Blob(asset.data.clone()),
            ],
        )));
    };
    let old_len = u64_of(row, 0, "assets.byte_len")?;
    let old_data = optional_blob(row, 1)?;
    if old_len != asset.byte_len {
        return Err(StorageError::SyncConflict {
            context: format!("baseline 资产 {} byte_len 不一致", hex32(&asset.asset_hash)),
        });
    }
    match old_data {
        Some(bytes) if bytes == asset.data => Ok(None),
        Some(_) => Err(StorageError::SyncConflict {
            context: format!("baseline 资产 {} 明文不一致", hex32(&asset.asset_hash)),
        }),
        None => Ok(Some(Statement::new(
            "UPDATE assets SET data=?2 WHERE asset_hash=?1 AND data IS NULL",
            vec![
                Value::Blob(asset.asset_hash.to_vec()),
                Value::Blob(asset.data.clone()),
            ],
        ))),
    }
}

fn load_applied_asset(
    vault: &Vault,
    object_id: &[u8; 32],
) -> Result<Option<([u8; 32], u64)>, StorageError> {
    let rows = vault.query(
        "SELECT asset_hash,byte_len FROM baseline_apply_assets WHERE object_id=?1",
        &[Value::Blob(object_id.to_vec())],
    )?;
    rows.first()
        .map(|row| {
            Ok((
                array32(row, 0, "baseline_apply_assets.asset_hash")?,
                u64_of(row, 1, "baseline_apply_assets.byte_len")?,
            ))
        })
        .transpose()
}

fn load_applied_doc_digests(vault: &Vault) -> Result<BTreeMap<String, [u8; 32]>, StorageError> {
    let rows = vault.query(
        "SELECT doc_id,snapshot_digest FROM baseline_apply_docs WHERE applied=1",
        &[],
    )?;
    rows.iter()
        .map(|row| {
            Ok((
                text_of(row, 0, "baseline_apply_docs.doc_id")?,
                array32(row, 1, "baseline_apply_docs.snapshot_digest")?,
            ))
        })
        .collect()
}

/// 读 begin 记录的预期参与集合；缺失返回 `None`（旧 journal/异常，按不可 finish 处理）。
fn load_expected_set(vault: &Vault) -> Result<Option<BaselineExpectation>, StorageError> {
    match vault.get_meta(META_BASELINE_EXPECTED)? {
        None => Ok(None),
        Some(bytes) => BaselineExpectation::decode(&bytes).map(Some),
    }
}

/// 预期集合是否已全部 `applied=1`。
///
/// 先比计数（每批都会更新 `applied_docs/applied_assets`，因此未齐时 O(1) 就能拒绝），
/// 计数相等后再逐项确认集合身份：光比数量会放过"漏一篇 + 多跑一篇"的情况。计数相等
/// 且预期项全部在已应用集合中 ⇒ 两个集合相等。
fn expected_is_complete(
    vault: &Vault,
    journal: &ApplyJournal,
    expected: &BaselineExpectation,
) -> Result<bool, StorageError> {
    if journal.applied_docs != expected.doc_count()
        || journal.applied_assets != expected.asset_count()
    {
        return Ok(false);
    }
    if expected.is_empty() {
        return Ok(true);
    }
    let applied_docs: BTreeSet<String> = load_applied_doc_digests(vault)?.into_keys().collect();
    let docs_complete = expected
        .docs()
        .iter()
        .all(|doc_id| applied_docs.contains(doc_id));
    if !docs_complete {
        return Ok(false);
    }
    let applied_assets = load_applied_asset_ids(vault)?;
    Ok(expected
        .assets()
        .iter()
        .all(|object_id| applied_assets.contains(object_id)))
}

/// 当前 journal 中 `applied=1` 的资产 object_id 集合。
fn load_applied_asset_ids(vault: &Vault) -> Result<BTreeSet<[u8; 32]>, StorageError> {
    let rows = vault.query(
        "SELECT object_id FROM baseline_apply_assets WHERE applied=1",
        &[],
    )?;
    rows.iter()
        .map(|row| array32(row, 0, "baseline_apply_assets.object_id"))
        .collect()
}

fn corrupt_expected(reason: &str) -> StorageError {
    StorageError::Corrupt {
        context: format!("meta {META_BASELINE_EXPECTED} 非法：{reason}"),
    }
}

fn take_expected_bytes<'a>(
    bytes: &'a [u8],
    offset: &mut usize,
    len: usize,
) -> Result<&'a [u8], StorageError> {
    let end = offset
        .checked_add(len)
        .ok_or_else(|| corrupt_expected("长度溢出"))?;
    let slice = bytes
        .get(*offset..end)
        .ok_or_else(|| corrupt_expected("长度越界"))?;
    *offset = end;
    Ok(slice)
}

fn read_expected_u64(bytes: &[u8], offset: &mut usize) -> Result<u64, StorageError> {
    let raw = take_expected_bytes(bytes, offset, 8)?;
    let mut value = [0u8; 8];
    value.copy_from_slice(raw);
    Ok(u64::from_be_bytes(value))
}

fn load_active_journal(vault: &Vault) -> Result<ApplyJournal, StorageError> {
    let journal = load_journal(vault)?.ok_or_else(|| StorageError::SyncConflict {
        context: "没有 active baseline apply".to_owned(),
    })?;
    if journal.status != BaselineApplyStatus::Staging {
        return Err(StorageError::SyncConflict {
            context: "baseline apply 已完成，不能再应用批次".to_owned(),
        });
    }
    Ok(journal)
}

fn load_journal(vault: &Vault) -> Result<Option<ApplyJournal>, StorageError> {
    let rows = vault.query(
        "SELECT checkpoint_id,baseline_s,source_cursor,status,\
                expected_received_seq,expected_integrated_seq,expected_confirmed_highwater,\
                expected_relay_cursor,applied_docs,applied_assets,kept_local_edit,chosen_cursor \
         FROM baseline_apply WHERE singleton=1",
        &[],
    )?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    Ok(Some(ApplyJournal {
        checkpoint_id: array16(row, 0, "baseline_apply.checkpoint_id")?,
        baseline_s: u64_of(row, 1, "baseline_apply.baseline_s")?,
        source_cursor: text_of(row, 2, "baseline_apply.source_cursor")?,
        status: BaselineApplyStatus::parse(&text_of(row, 3, "baseline_apply.status")?)?,
        expected_received_seq: u64_of(row, 4, "baseline_apply.expected_received_seq")?,
        expected_integrated_seq: u64_of(row, 5, "baseline_apply.expected_integrated_seq")?,
        expected_confirmed_highwater: optional_u64(
            row,
            6,
            "baseline_apply.expected_confirmed_highwater",
        )?,
        expected_relay_cursor: optional_blob(row, 7)?,
        applied_docs: u64_of(row, 8, "baseline_apply.applied_docs")?,
        applied_assets: u64_of(row, 9, "baseline_apply.applied_assets")?,
        kept_local_edit: u64_of(row, 10, "baseline_apply.kept_local_edit")?,
        chosen_cursor: match row.get(11) {
            Some(Value::Text(value)) => Some(value.clone()),
            Some(Value::Null) | None => None,
            other => {
                return Err(StorageError::Corrupt {
                    context: format!("baseline_apply.chosen_cursor 读回异常：{other:?}"),
                });
            }
        },
    }))
}

fn state_from_journal(
    vault: &Vault,
    journal: &ApplyJournal,
) -> Result<BaselineApplyState, StorageError> {
    let expected = load_expected_set(vault)?;
    let (expected_docs, expected_assets) = match &expected {
        Some(expected) => (Some(expected.doc_count()), Some(expected.asset_count())),
        None => (None, None),
    };
    // finishable 必须反映**真实就绪**：仅 status=staging 不够，还要 begin 记录的预期
    // 集合全部 applied=1（否则 finish 会 Conflict，不能对 UI/续跑逻辑谎报可完成）。
    let finishable = journal.status == BaselineApplyStatus::Staging
        && match &expected {
            Some(expected) => expected_is_complete(vault, journal, expected)?,
            None => false,
        };
    Ok(BaselineApplyState {
        checkpoint_id: journal.checkpoint_id,
        baseline_s: journal.baseline_s,
        source_cursor: journal.source_cursor.clone(),
        status: journal.status,
        applied_docs: journal.applied_docs,
        applied_assets: journal.applied_assets,
        kept_local_edit: journal.kept_local_edit,
        expected_docs,
        expected_assets,
        finishable,
        chosen_cursor: journal.chosen_cursor.clone(),
    })
}

fn push_meta_cas(
    statements: &mut Vec<Statement>,
    guards: &mut Vec<AffectedGuard>,
    key: &str,
    old: u64,
    new: &[u8],
) {
    let index = statements.len();
    if old == 0 {
        // 初始 cursor 在 meta 中可能不存在；0 与“缺失”语义等价。
        statements.push(Statement::new(
            "INSERT INTO meta(key,value) VALUES(?1,?2) \
             ON CONFLICT(key) DO UPDATE SET value=excluded.value \
             WHERE meta.value=?3",
            vec![
                Value::Text(key.to_owned()),
                Value::Blob(new.to_vec()),
                Value::Blob(old.to_be_bytes().to_vec()),
            ],
        ));
    } else {
        statements.push(Statement::new(
            "UPDATE meta SET value=?3 WHERE key=?1 AND value=?2",
            vec![
                Value::Text(key.to_owned()),
                Value::Blob(old.to_be_bytes().to_vec()),
                Value::Blob(new.to_vec()),
            ],
        ));
    }
    guards.push(AffectedGuard::exactly(index, 1));
}

fn push_meta_cas_bytes(
    statements: &mut Vec<Statement>,
    guards: &mut Vec<AffectedGuard>,
    key: &str,
    old: &[u8],
    new: &[u8],
) {
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

fn push_meta_absent_insert(
    statements: &mut Vec<Statement>,
    guards: &mut Vec<AffectedGuard>,
    key: &str,
    new: &[u8],
) {
    let index = statements.len();
    statements.push(Statement::new(
        "INSERT INTO meta(key,value) SELECT ?1,?2 \
         WHERE NOT EXISTS(SELECT 1 FROM meta WHERE key=?1)",
        vec![Value::Text(key.to_owned()), Value::Blob(new.to_vec())],
    ));
    guards.push(AffectedGuard::exactly(index, 1));
}

fn now_secs() -> Result<i64, StorageError> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| StorageError::Corrupt {
            context: format!("系统时间早于 UNIX_EPOCH：{error}"),
        })?;
    i64::try_from(now.as_secs()).map_err(|_| StorageError::Corrupt {
        context: "系统时间超过 i64".to_owned(),
    })
}

fn i64_of(value: u64) -> Result<i64, StorageError> {
    i64::try_from(value).map_err(|_| StorageError::Corrupt {
        context: format!("{value} 超过 i64::MAX"),
    })
}

fn opt_u64(value: Option<u64>) -> Result<Value, StorageError> {
    match value {
        None => Ok(Value::Null),
        Some(value) => Ok(Value::Integer(i64_of(value)?)),
    }
}

fn opt_blob(value: Option<Vec<u8>>) -> Value {
    match value {
        None => Value::Null,
        Some(value) => Value::Blob(value),
    }
}

fn opt_text(value: Option<String>) -> Value {
    match value {
        None => Value::Null,
        Some(value) => Value::Text(value),
    }
}

fn optional_u64(row: &crate::Row, index: usize, field: &str) -> Result<Option<u64>, StorageError> {
    match row.get(index) {
        Some(Value::Null) | None => Ok(None),
        Some(Value::Integer(value)) => {
            u64::try_from(*value)
                .map(Some)
                .map_err(|_| StorageError::Corrupt {
                    context: format!("{field} 为负：{value}"),
                })
        }
        other => Err(StorageError::Corrupt {
            context: format!("{field} 读回异常：{other:?}"),
        }),
    }
}

fn optional_blob(row: &crate::Row, index: usize) -> Result<Option<Vec<u8>>, StorageError> {
    match row.get(index) {
        Some(Value::Null) | None => Ok(None),
        Some(Value::Blob(value)) => Ok(Some(value.clone())),
        other => Err(StorageError::Corrupt {
            context: format!("blob 列读回异常：{other:?}"),
        }),
    }
}

fn text_of(row: &crate::Row, index: usize, field: &str) -> Result<String, StorageError> {
    match row.get(index) {
        Some(Value::Text(value)) => Ok(value.clone()),
        other => Err(StorageError::Corrupt {
            context: format!("{field} 应为 TEXT，实际 {other:?}"),
        }),
    }
}

fn u64_of(row: &crate::Row, index: usize, field: &str) -> Result<u64, StorageError> {
    match row.get(index) {
        Some(Value::Integer(value)) => u64::try_from(*value).map_err(|_| StorageError::Corrupt {
            context: format!("{field} 为负：{value}"),
        }),
        other => Err(StorageError::Corrupt {
            context: format!("{field} 应为 INTEGER，实际 {other:?}"),
        }),
    }
}

fn array16(row: &crate::Row, index: usize, field: &str) -> Result<[u8; 16], StorageError> {
    let bytes = blob_at(row, index, field)?;
    bytes
        .try_into()
        .map_err(|bytes: Vec<u8>| StorageError::Corrupt {
            context: format!("{field} 长度应为 16，实际 {}", bytes.len()),
        })
}

fn array32(row: &crate::Row, index: usize, field: &str) -> Result<[u8; 32], StorageError> {
    let bytes = blob_at(row, index, field)?;
    bytes
        .try_into()
        .map_err(|bytes: Vec<u8>| StorageError::Corrupt {
            context: format!("{field} 长度应为 32，实际 {}", bytes.len()),
        })
}

fn blob_at(row: &crate::Row, index: usize, field: &str) -> Result<Vec<u8>, StorageError> {
    match row.get(index) {
        Some(Value::Blob(value)) => Ok(value.clone()),
        other => Err(StorageError::Corrupt {
            context: format!("{field} 应为 BLOB，实际 {other:?}"),
        }),
    }
}

fn hex32(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

````


## `crates/storage/src/journal.rs`

journal.rs：JournalWriter、VaultJournalSink、HeadConflict 契约

````rust
//! 未 ACK 的写入 journal + 背压（ADR 0001 §3.4「A 已提交而 B 未完成」）。
//!
//! 模型：
//!
//! - **单份可追加 journal**：已产生但**尚未 durable** 的 update 按到达顺序排成一条 FIFO；
//! - **writer 取连续前缀**：flush 永远从头取 `min(pending, max_batch)` 条，
//!   不会跳过更早的条目（skip 就会造成 seq 空洞）；
//! - **durable 后裁剪**：只有 sink 成功返回后，才把这一段从 journal 里去掉并推进水位；
//! - **慢 I/O 合并 + 背压**：一批 flush 把当前所有 pending 前缀**一次性**写进同一个
//!   SQLite 事务；pending 达到 `capacity` 时 `submit` 直接返回
//!   [`StorageError::WriteBackpressure`]，**不 append**，调用方必须先 flush（拒绝无界增长）。
//!
//! 崩溃语义：sink 成功 = 已 durable；未 flush 的条目只在内存里，重启后自然丢掉。
//! 因此「已 ACK 的不丢、未 ACK 的只丢连续前缀之后」由本模块与 [`crate::store`] 的 A 事务共同保证。
//!
//! # head 过期：可重试 vs 必须重建 session（P1-3 契约）
//!
//! journal 条目里的 `PendingUpdate` 是**追加那一刻**按当时 head 导出的字节
//! （`end_frontier` / `end_vv` 是那次导出的快照），**不可重算**。如果同一文档的 head
//! 被别的 writer 推进（同步引擎 integrate、`SessionDriver` 的 adopt、另一个 Vault
//! 进程……），前缀的 CAS 基准（`loro_docs.head_seq`）就永远对不上：`append_updates`
//! 每次都返回 [`StorageError::HeadConflict`]，但**同一份字节重试多少次都不会成功**。
//! 这正是旧实现「flush 一直 HeadConflict、水位永远不前进」的卡死来源。
//!
//! 本模块用一条明确的二分法代替无限重试：
//!
//! - [`JournalWriter::flush`] 捕获 sink 的 [`StorageError::HeadConflict`]，**不重试**
//!   （0 次），立即转成终态错误 [`StorageError::JournalHeadStale`]
//!   （`doc_id` / `expected` / `actual` / `pending` 齐全，
//!   [`StorageError::requires_session_rebuild`] 为 `true`）。其它错误（磁盘失败等）
//!   原样透传 —— 那才是可重试的，journal 原样保留。
//! - 进入 stale 之后，`flush` / `flush_all` **不再触碰数据库**，幂等地返回同一个
//!   stale 错误；`submit` 也返回 `JournalHeadStale`（writer 已冻结，新条目必须交给
//!   重建后的 writer）。因此调用方无论怎么轮询，都不会挂死、也不会刷屏重试。
//! - **谁负责重建**：持有 live `BodyDoc` 的会话层（`session::JournalHandle` /
//!   `SessionDriver`，实现在 `crate::session::flush`）。重建顺序：
//!   ① 先停外部写并按 ADR 0012 对账（`HeadConflict` 先停外部写、验证授权、再对账，
//!   然后才允许有界重放）；② 把 durable journal 重放到**新 head**（`load_body_doc` /
//!   `load_head`），让内存文档与库一致；③ 用新 head 的 VV 重新导出本地意图
//!   （`BodyDoc::pending_update`）——只有这一步得到的字节才能写；④ 用新 `head_seq`
//!   造一个 [`VaultJournalSink`]，调用 [`JournalWriter::rebuild_sink`] 装回去。
//! - **未 durable 的旧条目怎么办**：`rebuild_sink` 把它们**原样交还**调用方（返回值）
//!   并从 writer 的 journal 里清空。它们**没有被写入、也没有被静默丢弃** —— 调用方
//!   必须用 ③ 重新导出的增量覆盖这段内容；若调用方决定不要，也必须显式消费返回值
//!   并在上层报告，不能假装成功。重建后 `durable_seq` 等于新 sink 的 CAS 基准。
//! - **有界重试规则**：CAS 冲突在 journal 层的自动重试是 **0** 次（基准是常量，
//!   重试不可能成功）；需要重放时由 session 层按 ADR 0012 做**有界**重放（一次），
//!   再失败就上报告需要人工处理。本模块不提供、也不会退化成无限重试。

use std::collections::VecDeque;

use crate::body_doc::PendingUpdate;
use crate::error::StorageError;
use crate::store::{OriginMarks, UpdateRecord};
use crate::sync_bridge::{self, SyncContext, SyncIntent};
use crate::Vault;

/// 一条 update 的**来源**（ADR 0009：只有本地提交才产生出站意图）。
///
/// - [`JournalOrigin::Local`]：真实本地写入 —— 用户输入 / IME / undo / redo / 结构提交，
///   启用同步上下文时在同一个 A 事务里写一条 `sync_outbox` 意图；
/// - [`JournalOrigin::Remote`]：远端导入 / adopt，**必须不产生任何意图**，否则会把刚
///   收到的数据原样推回 relay（自激循环，且破坏 epoch/ACK 语义）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JournalOrigin {
    /// 本机产生的更新（本地输入、undo/redo）。
    Local,
    /// 从远端导入的更新（`DocumentSession::apply_remote`）。
    Remote,
}

impl JournalOrigin {
    /// 是否应该产生出站同步意图。
    #[must_use]
    pub const fn produces_sync_intent(self) -> bool {
        matches!(self, Self::Local)
    }
}

/// 一条待落库的 update。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalEntry {
    /// 所属文档。
    pub doc_id: String,
    /// 批次 ID（同一次 flush 共用一个 batch_id）。
    pub batch_id: String,
    /// 增量本身。
    pub update: PendingUpdate,
    /// 来源（决定 flush 时是否产生出站意图）。
    pub origin: JournalOrigin,
}

impl JournalEntry {
    /// 本地提交（会产生同步意图）。
    #[must_use]
    pub fn local(
        doc_id: impl Into<String>,
        batch_id: impl Into<String>,
        update: PendingUpdate,
    ) -> Self {
        Self {
            doc_id: doc_id.into(),
            batch_id: batch_id.into(),
            update,
            origin: JournalOrigin::Local,
        }
    }

    /// 远端导入（**不**产生同步意图）。
    #[must_use]
    pub fn remote(
        doc_id: impl Into<String>,
        batch_id: impl Into<String>,
        update: PendingUpdate,
    ) -> Self {
        Self {
            doc_id: doc_id.into(),
            batch_id: batch_id.into(),
            update,
            origin: JournalOrigin::Remote,
        }
    }
}

/// 只可追加的未 ACK journal。
#[derive(Debug)]
pub struct PendingJournal {
    capacity: usize,
    entries: VecDeque<JournalEntry>,
    next_id: u64,
}

impl PendingJournal {
    /// 创建容量为 `capacity` 条的 journal；容量必须 > 0，否则永远只能报背压。
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            entries: VecDeque::new(),
            next_id: 0,
        }
    }

    /// 追加一条。已满时返回 [`StorageError::WriteBackpressure`] 且**不写入**。
    pub fn append(&mut self, entry: JournalEntry) -> Result<u64, StorageError> {
        if self.entries.len() >= self.capacity {
            return Err(StorageError::WriteBackpressure {
                capacity: self.capacity,
            });
        }
        let id = self.next_id;
        self.next_id += 1;
        self.entries.push_back(entry);
        Ok(id)
    }

    /// 当前未 ACK 条数。
    pub fn pending_len(&self) -> usize {
        self.entries.len()
    }

    /// 容量上限。
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// journal 是否已满（调用方应先 flush）。
    pub fn is_full(&self) -> bool {
        self.entries.len() >= self.capacity
    }

    /// 取从头开始的连续前缀（最多 `max` 条），只读不改。
    ///
    /// 取的一定是 FIFO 的最前面若干条：跳过更早的条目会造成 seq 空洞，
    /// 所以 flush 只能「从头开始」而不能挑着写。
    pub fn peek_prefix(&mut self, max: usize) -> &[JournalEntry] {
        let take = max.min(self.entries.len());
        if take == 0 {
            return &[];
        }
        // VecDeque 可能是环形，先整理成连续内存再切片
        self.entries.make_contiguous();
        let (front, _) = self.entries.as_slices();
        &front[..take]
    }

    /// durable 之后裁剪前缀。
    pub fn ack_prefix(&mut self, count: usize) -> Result<(), StorageError> {
        if count > self.entries.len() {
            return Err(StorageError::Corrupt {
                context: format!("ack_prefix({count}) 超过未 ACK 条数 {}", self.entries.len()),
            });
        }
        for _ in 0..count {
            self.entries.pop_front();
        }
        Ok(())
    }

    /// 取走全部未 ACK 条目。
    ///
    /// **只**供 [`JournalWriter::rebuild_sink`] 在 head 过期后显式交还给调用方使用：
    /// 正常路径一律走 [`Self::ack_prefix`]，这里既不写库也不推进任何水位。
    fn drain_all(&mut self) -> Vec<JournalEntry> {
        self.entries.drain(..).collect()
    }
}

/// durable 落点（生产实现见 [`VaultJournalSink`]，测试可注入慢盘/失败实现）。
pub trait DurableSink {
    /// 把一段**连续前缀**写 durable；成功返回这段里最后一条的 `seq`。
    ///
    /// 实现必须是原子的：要么整段 durable，要么什么都没写（否则会丢已 ACK 数据）。
    ///
    /// CAS 基准冲突必须返回 [`StorageError::HeadConflict`]（不能自己改基准硬写）：
    /// [`JournalWriter`] 据此把 writer 判成 head 过期，而不是无限重试同一份字节。
    fn write_prefix(&mut self, entries: &[JournalEntry]) -> Result<u64, StorageError>;

    /// 当前 sink 使用的 CAS 基准（= 它认为已 durable 的 `head_seq`）。
    ///
    /// writer 用它给 `durable_seq` 定初值，并在 [`JournalWriter::rebuild_sink`] 之后重新对齐
    /// 水位。实现必须返回**真正用于 CAS 的**那个值，否则重建后的水位会骗人。
    fn head_seq(&self) -> u64;
}

/// 一次 flush 的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlushReport {
    /// 本次写 durable 的条数。
    pub entries: usize,
    /// 写完后的 durable seq（sink 返回）。
    pub durable_seq: u64,
}

/// head 过期的终态记录（见模块头契约）。
///
/// 只要它非空，writer 就冻结：`submit` / `flush` 一律返回同一个
/// [`StorageError::JournalHeadStale`]，直到调用方 [`JournalWriter::rebuild_sink`]。
#[derive(Debug, Clone, PartialEq, Eq)]
struct StaleHead {
    doc_id: String,
    expected: u64,
    actual: u64,
}

impl StaleHead {
    fn to_error(&self, pending: usize) -> StorageError {
        StorageError::JournalHeadStale {
            doc_id: self.doc_id.clone(),
            expected: self.expected,
            actual: self.actual,
            pending,
        }
    }
}

/// journal writer：持有 journal 与 sink，串行 flush。
#[derive(Debug)]
pub struct JournalWriter<S: DurableSink> {
    journal: PendingJournal,
    sink: S,
    max_batch: usize,
    durable_seq: u64,
    flushed_entries: u64,
    /// 非空 = CAS 基准已过期，writer 已冻结（见模块头契约）。
    stale: Option<StaleHead>,
}

impl<S: DurableSink> JournalWriter<S> {
    /// `max_batch` 是一次 flush 最多合并多少条（慢 I/O 时 pending 会堆到这里为止）。
    ///
    /// `durable_seq` 初值取 sink 的 CAS 基准（[`DurableSink::head_seq`]），
    /// 因此空 journal 上 flush 报告的是真实水位，而不是 0。
    pub fn new(sink: S, capacity: usize, max_batch: usize) -> Self {
        let durable_seq = sink.head_seq();
        Self {
            journal: PendingJournal::new(capacity),
            sink,
            max_batch: max_batch.max(1),
            durable_seq,
            flushed_entries: 0,
            stale: None,
        }
    }

    /// 追加一条待落库 update；满了就是背压，调用方必须 flush 后重试。
    ///
    /// head 过期后拒绝（返回 [`StorageError::JournalHeadStale`]）：writer 已冻结，
    /// 新条目必须交给重建后的 writer，否则只会堆在一条永远写不动的前缀后面。
    pub fn submit(&mut self, entry: JournalEntry) -> Result<u64, StorageError> {
        if let Some(stale) = self.stale.as_ref() {
            return Err(stale.to_error(self.journal.pending_len()));
        }
        self.journal.append(entry)
    }

    /// 把当前前缀合并成一次 sink 写；成功后裁剪 journal 并推进水位。
    ///
    /// 失败时 journal **原样保留**（sink 承诺原子），下次重试不会丢已成功部分。
    ///
    /// 唯一的例外是 CAS 冲突：sink 返回 [`StorageError::HeadConflict`] 时前缀字节已不可
    /// 重放，本方法**不重试**（0 次），标记 stale 并返回
    /// [`StorageError::JournalHeadStale`] —— 调用方必须按模块头的契约重建 session，
    /// 而不是再 flush 一次。
    pub fn flush(&mut self) -> Result<FlushReport, StorageError> {
        // 终态优先：stale 之后连数据库都不碰（有界重试 = 0 次）。
        if let Some(stale) = self.stale.as_ref() {
            return Err(stale.to_error(self.journal.pending_len()));
        }

        // 先拷出前缀，避免同时借用 journal 与 sink
        let prefix: Vec<JournalEntry> = self.journal.peek_prefix(self.max_batch).to_vec();
        if prefix.is_empty() {
            return Ok(FlushReport {
                entries: 0,
                durable_seq: self.durable_seq,
            });
        }

        let entries = prefix.len();
        let durable_seq = match self.sink.write_prefix(&prefix) {
            Ok(seq) => seq,
            // CAS 冲突是**终态**：前缀的 end_vv/end_frontier 是按旧 head 导出的快照，
            // 重新导出只能由持有 live 文档的会话层做；同一份字节重试不可能成功。
            Err(StorageError::HeadConflict {
                doc_id,
                expected,
                actual,
            }) => {
                let stale = StaleHead {
                    doc_id,
                    expected,
                    actual,
                };
                let error = stale.to_error(self.journal.pending_len());
                self.stale = Some(stale);
                return Err(error);
            }
            // 其它错误（磁盘失败等）原样透传：journal 原样保留，同一前缀可以重试。
            Err(error) => return Err(error),
        };
        self.journal.ack_prefix(entries)?;
        self.durable_seq = durable_seq;
        self.flushed_entries += entries as u64;

        Ok(FlushReport {
            entries,
            durable_seq,
        })
    }

    /// flush 直到 journal 空或没有进展（一次性排空，供关闭/手动保存用）。
    ///
    /// head 过期时第一拍就返回 [`StorageError::JournalHeadStale`]，不会空转。
    pub fn flush_all(&mut self) -> Result<usize, StorageError> {
        let mut total = 0;
        while self.journal.pending_len() > 0 {
            let report = self.flush()?;
            if report.entries == 0 {
                break;
            }
            total += report.entries;
        }
        Ok(total)
    }

    /// writer 是否已进入「head 过期、必须重建」的终态。
    pub fn is_stale(&self) -> bool {
        self.stale.is_some()
    }

    /// 重建 sink：head 过期后**唯一**合法的恢复入口（见模块头契约）。
    ///
    /// 调用方必须先用新 head 重放 live 文档并重新导出本地意图；本方法只负责把 writer
    /// 从 stale 拉回可用状态：
    ///
    /// - 把旧 journal 里未写入的条目**原样交还**（返回值），并从 journal 里清空 ——
    ///   它们不写库、也**不被静默丢弃**；调用方要么用重新导出的增量覆盖其内容，
    ///   要么显式报告这次丢弃；
    /// - `durable_seq` 对齐新 sink 的 CAS 基准（[`DurableSink::head_seq`]）；
    /// - 未处于 stale 时拒绝：还有能写的前缀就换 sink，等于把本可落库的条目丢掉。
    pub fn rebuild_sink(&mut self, sink: S) -> Result<Vec<JournalEntry>, StorageError> {
        if self.stale.is_none() {
            return Err(StorageError::Corrupt {
                context: "journal 未处于 head 过期状态，拒绝重建 sink（会丢弃仍可写入的条目）"
                    .to_owned(),
            });
        }
        let handed_back = self.journal.drain_all();
        self.sink = sink;
        self.stale = None;
        self.durable_seq = self.sink.head_seq();
        Ok(handed_back)
    }

    /// 未 ACK 条数。
    pub fn pending_len(&self) -> usize {
        self.journal.pending_len()
    }

    /// journal 容量。
    pub fn capacity(&self) -> usize {
        self.journal.capacity()
    }

    /// 已 durable 的最后 seq。
    pub fn durable_seq(&self) -> u64 {
        self.durable_seq
    }

    /// 累计 flush 成功的条数。
    pub fn flushed_entries(&self) -> u64 {
        self.flushed_entries
    }

    /// 取回 sink（关闭/测试用）。
    pub fn into_sink(self) -> S {
        self.sink
    }
}

/// 生产用 sink：把前缀写进 [`crate::store::append_updates`] 的 A 事务。
///
/// 带 [`SyncContext`] 时（`with_sync_context`），**本地**条目在同一 A 事务里各写一条
/// `sync_outbox` 意图（与 records 一一对应）；未启用时行为与切片 2a 之前完全一致。
#[derive(Debug, Clone)]
pub struct VaultJournalSink {
    vault: Vault,
    doc_id: String,
    head_seq: u64,
    sync: Option<SyncContext>,
}

impl VaultJournalSink {
    /// 以库中当前 head_seq 作为 CAS 基准（默认**不**产生同步意图）。
    pub fn new(vault: Vault, doc_id: impl Into<String>, head_seq: u64) -> Self {
        Self {
            vault,
            doc_id: doc_id.into(),
            head_seq,
            sync: None,
        }
    }

    /// 启用本地意图：每次 flush 为本地条目生成确定性 `SyncIntent`（同事务落库）。
    #[must_use]
    pub fn with_sync_context(mut self, sync: SyncContext) -> Self {
        self.sync = Some(sync);
        self
    }

    /// 当前已 durable 的 seq。
    pub fn head_seq(&self) -> u64 {
        self.head_seq
    }
}

impl DurableSink for VaultJournalSink {
    fn write_prefix(&mut self, entries: &[JournalEntry]) -> Result<u64, StorageError> {
        if entries.is_empty() {
            return Err(StorageError::Corrupt {
                context: "sink 收到空前缀".to_owned(),
            });
        }
        for entry in entries {
            if entry.doc_id != self.doc_id {
                return Err(StorageError::Corrupt {
                    context: format!(
                        "一次 flush 里混了多个文档：期望 {}，实际 {}",
                        self.doc_id, entry.doc_id
                    ),
                });
            }
        }
        // batch_id 逐条保留：合并 flush 可能跨多个逻辑批次
        let records: Vec<UpdateRecord> = entries
            .iter()
            .map(|entry| UpdateRecord::new(entry.batch_id.clone(), entry.update.clone()))
            .collect();
        // 远端导入条目：其 `end_vv - 上一条 end_vv` 是远端真正带来的 ops，
        // 同事务记入「远端已知 VV」（baseline tombstone 判定用）。
        let remote_marks: Vec<bool> = entries
            .iter()
            .map(|entry| entry.origin == JournalOrigin::Remote)
            .collect();
        let head = match self.sync {
            // 默认路径（未启用同步）：行为与切片 2a 之前一致（只是多记远端已知 VV）。
            None => sync_bridge::append_updates_with_intent_slots(
                &self.vault,
                &self.doc_id,
                self.head_seq,
                &records,
                &[],
                OriginMarks::PerRecord(&remote_marks),
            )?,
            Some(ctx) => {
                // 一 update 一意图（remote 条目为空位）；seq 必须与 A 事务写进
                // `loro_updates.seq` 的值完全一致，否则意图会指向别人的 update。
                let mut slots: Vec<Option<SyncIntent>> = Vec::with_capacity(entries.len());
                for (index, entry) in entries.iter().enumerate() {
                    if !entry.origin.produces_sync_intent() {
                        slots.push(None);
                        continue;
                    }
                    let seq = self
                        .head_seq
                        .checked_add(1)
                        .and_then(|value| value.checked_add(index as u64))
                        .ok_or_else(|| StorageError::Corrupt {
                            context: format!("{} 本地 seq 溢出", self.doc_id),
                        })?;
                    slots.push(Some(sync_bridge::local_sync_intent(
                        &self.doc_id,
                        seq,
                        &entry.batch_id,
                        ctx,
                    )?));
                }
                sync_bridge::append_updates_with_intent_slots(
                    &self.vault,
                    &self.doc_id,
                    self.head_seq,
                    &records,
                    &slots,
                    OriginMarks::PerRecord(&remote_marks),
                )?
            }
        };
        self.head_seq = head.head_seq;
        Ok(head.head_seq)
    }

    /// CAS 基准 = 本 sink 已 durable 的 head_seq（与固有方法 `head_seq` 同一个值）。
    fn head_seq(&self) -> u64 {
        self.head_seq
    }
}

````


## `crates/storage/src/error.rs`

error.rs：JournalHeadStale / requires_session_rebuild

````rust
//! 存储层统一错误类型。
//!
//! 库层只暴露 [`StorageError`]，应用层再用 `anyhow` 之类的汇总类型包装。
//! 业务路径禁止 `unwrap()`；需要上下文时用 `?` 或 `map_err` 转成带上下文的变体。

use thiserror::Error;

/// 存储层错误。
#[derive(Debug, Error)]
pub enum StorageError {
    /// 文件系统错误（建父目录、启动工作线程等）。
    #[error("文件系统操作失败：{0}")]
    Io(#[from] std::io::Error),

    /// SQLite 自身返回的错误（SQL 语法、约束冲突、类型不匹配、锁等待超时等）。
    #[error("SQLite 操作失败：{0}")]
    Sqlite(#[from] rusqlite::Error),

    /// 只读快照在 `busy_timeout` 内没拿到读锁（写事务长时间占用）。
    ///
    /// 读路径**不做无限重试**：调用方（search）应当退避后重开快照，或改用
    /// `reconcile_required` 走全量对账，而不是在这里阻塞写线程。
    #[error("只读快照在 {busy_timeout_ms}ms 内未获得读锁（{context}）：请退避后重试或走全量对账")]
    SnapshotBusy {
        /// 出错的具体动作（打开/查询/解码）。
        context: &'static str,
        /// 该连接实际使用的 `busy_timeout`。
        busy_timeout_ms: u32,
    },

    /// 索引清单里某列的类型不符合 schema 预期（说明库被外部改坏或 schema 漂移）。
    #[error("索引清单列 [{column}] 类型异常：{found}")]
    ManifestShape {
        /// 列名。
        column: &'static str,
        /// 实际读到的 SQLite 类型。
        found: &'static str,
    },

    /// 迁移失败：带上目标版本与迁移名称，便于定位是哪一级迁移脚本出错。
    #[error("迁移到 schema v{version}（{name}）失败：{source}")]
    Migration {
        /// 目标 schema 版本。
        version: i64,
        /// 迁移的可读名称。
        name: &'static str,
        /// 底层 SQLite 错误。
        #[source]
        source: rusqlite::Error,
    },

    /// PRAGMA 设置后读回校验不一致。
    #[error("PRAGMA 校验失败：[{name}] 期望 {expected}，实际 {actual}")]
    PragmaMismatch {
        /// PRAGMA 名称。
        name: &'static str,
        /// 期望值（统一字符串化，便于比较）。
        expected: String,
        /// 实际读回值。
        actual: String,
    },

    /// 数据库 schema 版本高于本程序支持的版本：拒绝以可写方式打开。
    ///
    /// 对应 ADR 0001 §3.9「新 schema 下旧程序只读拒写」。
    #[error("数据库 schema 版本为 v{found}，本程序最高支持 v{supported}；拒绝以可写方式打开")]
    UnsupportedSchemaVersion {
        /// 数据库中记录的 `user_version`。
        found: i64,
        /// 本程序支持的最高 schema 版本。
        supported: i64,
    },

    /// 工作线程通道不可用（线程已退出，或初始化阶段就失败了）。
    #[error("存储工作线程不可用（{context}）")]
    ChannelClosed {
        /// 出错位置的可读描述。
        context: &'static str,
    },

    /// Loro 返回的错误（非法 update、peer id 非法、解码失败等）。
    #[error("Loro 操作失败：{0}")]
    Loro(#[from] loro::LoroError),

    /// Loro 导出/编码失败。
    #[error("Loro 导出失败：{0}")]
    LoroEncode(#[from] loro::LoroEncodeError),

    /// 坐标 / 单位换算失败（越界、半截字符、半截代理对）。
    ///
    /// 全仓只有 `menghunote_text::CoordError` 一份定义（ADR 0003 §4），这里只做包装。
    #[error("坐标换算失败：{0}")]
    Coord(#[from] menghunote_text::CoordError),

    /// A 事务的乐观并发冲突：head 已被别人推进，本批 update 未落库。
    #[error("文档 {doc_id} 的 head_seq 冲突：期望 {expected}，实际 {actual}；本批 update 已回滚")]
    HeadConflict {
        /// 文档 ID。
        doc_id: String,
        /// 调用方看到并用作 CAS 基准的 seq。
        expected: u64,
        /// 库里实际的 seq。
        actual: u64,
    },

    /// 未 ACK journal 的 CAS 基准已过期：head 被别的 writer 推进了。
    ///
    /// journal 条目的 `PendingUpdate` 是**追加那一刻**按当时 head 导出的字节
    /// （`end_frontier` / `end_vv` 都是那次导出的快照），**不可重算**。head 一旦前进，
    /// 同一份字节写一万次也只会再得到一次 CAS 冲突 —— 所以这是**终态**错误，
    /// 由 [`StorageError::requires_session_rebuild`] 显式标记：
    ///
    /// - **谁负责重建**：持有 live `BodyDoc` 的会话层（`session::JournalHandle` /
    ///   `SessionDriver`）。它必须先把内存文档重放到新 head，再按新 head 重新导出增量，
    ///   并用新 `head_seq` 造 sink（见 `crate::journal` 模块头的契约）；
    /// - **旧条目**：不会被写入，也**不会**被静默丢弃 —— `JournalWriter::rebuild_sink`
    ///   把它们原样交还给调用方；
    /// - **重试**：本层自动重试次数为 0，`flush` 之后不再触碰数据库。
    #[error(
        "文档 {doc_id} 的未 ACK journal 基准已过期（期望 head {expected}，实际 {actual}）：\
         {pending} 条已导出增量不可重算，必须重建 session 后重新导出；已禁止重试"
    )]
    JournalHeadStale {
        /// 文档 ID。
        doc_id: String,
        /// 写入时使用的 CAS 基准（`loro_docs.head_seq`）。
        expected: u64,
        /// 库里实际看到的 head。
        actual: u64,
        /// 过期时 journal 里未 durable 的条数。
        pending: usize,
    },

    /// 未 ACK journal 已满：必须先 flush 再重试（背压，不允许无界增长）。
    #[error("未 ACK journal 已满（容量 {capacity} 条），拒绝新写入；请先 flush")]
    WriteBackpressure {
        /// journal 容量。
        capacity: usize,
    },

    /// 事务守卫不满足：某条语句没有影响预期行数，整笔事务已回滚。
    #[error("事务守卫失败：第 {statement} 条语句应影响 {expected} 行，实际 {actual} 行；已回滚")]
    GuardFailed {
        /// 语句下标（0 起）。
        statement: usize,
        /// 期望影响行数。
        expected: usize,
        /// 实际影响行数。
        actual: usize,
    },

    /// 相对光标无法解析成当前位置（历史不可读或已丢失）。
    #[error("光标无法解析到位置：{0}")]
    CursorUnresolved(String),

    /// 标签名为空（归一化后没有任何字符）。
    #[error("标签名非法：{name:?}（归一化后为空）")]
    InvalidTagName {
        /// 传入的原始标签名。
        name: String,
    },

    /// 结构树里找不到指定节点（节点 ID 非法或已被移除）。
    #[error("结构树里找不到节点 {node_id}")]
    UnknownNode {
        /// 节点 ID。
        node_id: String,
    },

    /// 同步桥 CAS/幂等冲突：同一 message/epoch 已存在不同字节，或旧 ACK/旧 digest。
    #[error("同步状态冲突：{context}")]
    SyncConflict {
        /// 具体上下文（message/epoch/digest）。
        context: String,
    },

    /// 同步 log_id 与本地已记录的不一致（relay reset 必须显式迁移，不能静默归零）。
    #[error("同步 log_id 不一致：本地 {expected:?}，收到 {found:?}")]
    SyncLogMismatch {
        /// 本地记录的 log_id。
        expected: Vec<u8>,
        /// 新收到的 log_id。
        found: Vec<u8>,
    },

    /// 历史版本正文的 zstd 压缩/解压失败（库里的历史快照损坏，或编码阶段内存不足）。
    ///
    /// 与 [`Self::Io`] 区分：这不是文件系统错误，调用方只该提示"历史版本读取失败"，
    /// 不应把它当成库路径/权限问题。
    #[error("历史版本编解码失败（{context}）：{source}")]
    HistoryCodec {
        /// 具体动作（压缩/解压）。
        context: &'static str,
        /// 底层 `std::io::Error`（zstd 流）。
        #[source]
        source: std::io::Error,
    },

    /// 数据库中出现不自洽状态（缺行、seq 断档、重放结果与记录不一致等）。
    #[error("存储状态不自洽：{context}")]
    Corrupt {
        /// 具体上下文。
        context: String,
    },
}

impl StorageError {
    /// 是否必须**重建 session**（而不是重试）。
    ///
    /// 目前只有 [`Self::JournalHeadStale`]：那批 `PendingUpdate` 是按旧 head 导出的字节，
    /// 没有任何 rebase 可能，原样重试永远不会成功。调用方必须先重建 session、重新导出
    /// 增量（见 `crate::journal` 模块头），否则只会把失败换个地方再报一次。
    #[must_use]
    pub fn requires_session_rebuild(&self) -> bool {
        matches!(self, Self::JournalHeadStale { .. })
    }

    /// 是否值得**原样重试**（只表示"条件变化后可能成功"，不表示"一定要重试"）。
    ///
    /// - [`Self::HeadConflict`]：store 层 CAS 冲突 —— 重读 head 后重放（sync engine 语义，
    ///   与 journal 层的[`Self::JournalHeadStale`]不同：那里的字节可以重新导出）；
    /// - [`Self::SnapshotBusy`]：读锁超时 —— 退避后重开快照；
    /// - [`Self::WriteBackpressure`]：journal 满 —— 先 flush 再重试。
    ///
    /// 其余变体一律 `false`：盲目重试只会挂死，或把真正的损坏/配置错误掩盖成"暂时失败"。
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::HeadConflict { .. } | Self::SnapshotBusy { .. } | Self::WriteBackpressure { .. }
        )
    }
}

````


## `crates/storage/src/remote_known.rs`

remote_known.rs：值 CAS、单调并集

````rust
//! 每篇文档「远端已知 VV」的本地元数据（**非破坏性本地 meta**，不新增表/列、不参与 wire）。
//!
//! ## 为什么需要它
//!
//! `baseline_apply` 的远端 tombstone 需要回答：**这篇本地文档里有没有远端从未见过的 op？**
//! 有就必须保留（可能是 local-only 内容或尚未同步的本地编辑）；全都见过则可以按远端
//! tombstone 标记删除。仅凭 `loro_docs.head_seq>0` 或 `sync_outbox` 为空都不够：
//!
//! - 已有设备普通 pull/integrate 同步过的文档，`head_seq>0`、outbox 为空，
//!   但它的内容确实全部来自远端；
//! - 启用同步前就存在的 local-only 文档同样 `head_seq>0`（且没有 outbox 行），
//!   却从未离开本机。
//!
//! 因此这里持久化一个**只能单调扩大**的「远端已确证见过的 oplog 前缀」：
//! `meta` 键 `sync.remote_known_vv.<doc_id>`，值为 `VersionVector::encode()`。
//!
//! ## 谁会扩大它
//!
//! 1. 应用已验证基线快照：快照自带的 VV 就是远端在该 checkpoint 上已知的 ops
//!    （`baseline_apply` 在同一事务里写入）；
//! 2. 集成远端 update：`after_vv - before_vv` 就是这条远端消息实际带来的 ops
//!    （`store::append_updates_inner` 在同一 A 事务里写入）。
//!
//! 本地写入**不会**扩大它——即使已经 ACK 上传也不扩大。这是刻意的保守取舍：
//! tombstone 判定只会更倾向于 `kept_local_edit`，绝不会把本地未确证同步过的内容删掉。
//!
//! ## 单调性怎么强制（P2-5）
//!
//! 「只读旧值、在事务外算并集、再无条件覆盖」有两个写者互相覆盖的窗口（普通 pull 的
//! A 事务与 baseline apply 批次都可能写同一个键）。因此所有写入都改成**值 CAS**：
//!
//! - 生成语句时带上读到的旧值 `expected_old`，SQL 是
//!   `INSERT ... ON CONFLICT(key) DO UPDATE SET value=excluded.value WHERE meta.value=?old`；
//!   旧值不存在时用 `INSERT ... SELECT ... WHERE NOT EXISTS`；
//! - 语句带 `AffectedGuard::exactly(index, 1)`：CAS 不命中就是 0 行 → 整笔事务回滚，
//!   调用方拿到明确的冲突错误（重读后重试即收敛到并集），**绝不会写入更小的值**。
//!
//! ## 判定
//!
//! `local_vv.sub_iter(remote_known).next().is_none()` 表示本地没有任何远端未知的 op，
//! 才允许按 tombstone 删除。没有这条记录时一律视为「无证据」，保守保留。

use loro::VersionVector;
use rusqlite::types::Value;

use crate::actor::Statement;
use crate::error::StorageError;
use crate::Vault;

/// 每篇文档「远端已知 VV」的 meta 键前缀。
pub const META_REMOTE_KNOWN_VV_PREFIX: &str = "sync.remote_known_vv.";

/// 某篇文档的 meta 键（doc_id 由仓库生成，形如 `note-<peer>-<seq>`，不含前缀歧义）。
pub(crate) fn meta_key(doc_id: &str) -> String {
    format!("{META_REMOTE_KNOWN_VV_PREFIX}{doc_id}")
}

/// 读某篇文档的远端已知 VV；没有记录返回 `None`（= 无证据）。
pub(crate) fn load(vault: &Vault, doc_id: &str) -> Result<Option<VersionVector>, StorageError> {
    let key = meta_key(doc_id);
    let rows = vault.query(
        "SELECT value FROM meta WHERE key=?1",
        &[Value::Text(key.clone())],
    )?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    match row.first() {
        Some(Value::Blob(bytes)) => Ok(Some(decode(bytes)?)),
        // meta 值的统一口径（P2-6）：只认 BLOB；NULL/INTEGER/TEXT 都视为库被改坏。
        other => Err(StorageError::Corrupt {
            context: format!("{key} 读回异常：{other:?}"),
        }),
    }
}

/// 解码 VV；空字节等价于空版本向量（与 `HeadState::version_vector` 同口径）。
pub(crate) fn decode(bytes: &[u8]) -> Result<VersionVector, StorageError> {
    if bytes.is_empty() {
        return Ok(VersionVector::default());
    }
    VersionVector::decode(bytes).map_err(StorageError::from)
}

/// 逐 peer 取 max（单调并集）。
pub(crate) fn union(existing: &VersionVector, added: &VersionVector) -> VersionVector {
    let mut merged = existing.clone();
    for (peer, &counter) in added.iter() {
        let counter = counter.max(0);
        if counter <= 0 {
            continue;
        }
        let current = merged.get(peer).copied().unwrap_or(0).max(0);
        if counter > current {
            merged.insert(*peer, counter);
        }
    }
    merged
}

/// `after` 中不在 `before` 里的 ops（远端消息真正带来的那一段）。
///
/// 结果 VV 对某个 peer 的值是 `after[peer]`，即「远端已知 [0, after)」，与 `before`
/// 里已有的前缀合并是单调的。
pub(crate) fn added_since(after: &VersionVector, before: &VersionVector) -> VersionVector {
    let mut added = VersionVector::new();
    for span in after.sub_iter(before) {
        added.insert(span.peer, span.counter.end);
    }
    added
}

/// 生成一条**值 CAS** upsert 语句（调用方负责把它放进 A 事务，并加
/// `AffectedGuard::exactly(语句下标, 1)`）。
///
/// - `expected = Some(old)`：`INSERT ... ON CONFLICT(key) DO UPDATE ... WHERE meta.value=?old`。
///   只有库中值仍是读到的旧值时才写入；被并发写者改过就是 0 行 → 守卫失败 → 整笔回滚；
/// - `expected = None`：`INSERT ... SELECT ... WHERE NOT EXISTS`。只有键仍不存在时才插入；
///   并发插入过就是 0 行 → 同样回滚。
///
/// 两种写法都不会用旧并集覆盖（更小的）并发结果，因此值只能单调扩大。
pub(crate) fn cas_statement(
    doc_id: &str,
    expected: Option<&VersionVector>,
    merged: &VersionVector,
) -> Statement {
    assert_monotone(expected, merged);
    let key = meta_key(doc_id);
    let encoded = merged.encode();
    match expected {
        Some(old) => Statement::new(
            "INSERT INTO meta(key,value) VALUES(?1,?2) \
             ON CONFLICT(key) DO UPDATE SET value=excluded.value \
             WHERE meta.value=?3",
            vec![
                Value::Text(key),
                Value::Blob(encoded),
                Value::Blob(old.encode()),
            ],
        ),
        None => Statement::new(
            "INSERT INTO meta(key,value) SELECT ?1,?2 \
             WHERE NOT EXISTS(SELECT 1 FROM meta WHERE key=?1)",
            vec![Value::Text(key), Value::Blob(encoded)],
        ),
    }
}

/// 调试断言：CAS 的目标值必须包含读到的旧值（`old ⊆ merged`）。
///
/// 生产路径由 [`union`] 构造 merged，这里只是把「只能扩大」的契约钉在生成语句处；
/// 一旦将来有人改成直接写入更小的值，debug/test 构建会立刻失败。
fn assert_monotone(expected: Option<&VersionVector>, merged: &VersionVector) {
    if let Some(old) = expected {
        debug_assert!(
            old.sub_iter(merged).next().is_none(),
            "remote known VV 只能单调扩大：旧值必须是 merged 的子集"
        );
    }
}

/// 在 A 事务**之外**读旧值并返回合并后的 **CAS** 语句；`added` 为空时返回 `None`。
///
/// 旧值只作为 CAS 的期望值；若事务执行时值已被并发写者扩大，语句影响 0 行、守卫失败，
/// 调用方必须重读（重新调用本函数）后重试。这里**不会**用读到的旧并集覆盖库里的新值。
pub(crate) fn merge_statement(
    vault: &Vault,
    doc_id: &str,
    added: &VersionVector,
) -> Result<Option<Statement>, StorageError> {
    if added.is_empty() {
        return Ok(None);
    }
    let existing = load(vault, doc_id)?;
    let merged = match &existing {
        Some(existing) => union(existing, added),
        None => added.clone(),
    };
    Ok(Some(cas_statement(doc_id, existing.as_ref(), &merged)))
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::mpsc;
    use std::sync::{Arc, Barrier};
    use std::thread;

    use super::*;
    use crate::actor::AffectedGuard;

    /// 临时 vault：目录建在 `target/tmp` 下（AGENTS §6：不用 `%TEMP%`）。
    ///
    /// lib 单元测试拿不到 `CARGO_TARGET_TMPDIR`（只有集成测试有），所以从
    /// `CARGO_MANIFEST_DIR` 拼出同一个位置。
    fn fresh_vault(prefix: &str) -> (tempfile::TempDir, Vault) {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/tmp")
            .join(format!("remote-known-{prefix}"));
        std::fs::create_dir_all(&root).expect("创建 target/tmp 下的测试根目录");
        let dir = tempfile::Builder::new()
            .prefix("vault-")
            .tempdir_in(&root)
            .expect("创建临时目录");
        let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
        (dir, vault)
    }

    fn vv(peer: u64, counter: i32) -> VersionVector {
        let mut vv = VersionVector::new();
        vv.insert(peer, counter);
        vv
    }

    fn read(vault: &Vault, doc_id: &str) -> Option<VersionVector> {
        load(vault, doc_id).expect("读取远端已知 VV")
    }

    fn exec_cas(
        vault: &Vault,
        doc_id: &str,
        expected: Option<&VersionVector>,
        merged: &VersionVector,
    ) -> Result<(), StorageError> {
        vault
            .exec_transaction(
                &[cas_statement(doc_id, expected, merged)],
                &[AffectedGuard::exactly(0, 1)],
            )
            .map(|_| ())
    }

    /// 值 CAS 的三种状态：不存在时插入 → 陈旧期望被拒绝且不缩小 → 读到新值后并集成功。
    #[test]
    fn cas_statement_rejects_stale_expected_and_never_shrinks() {
        let (_dir, vault) = fresh_vault("stale");
        let doc_id = "doc-cas";
        let a = vv(0xA1, 2);
        let b = vv(0xB2, 3);

        // 读时不存在：直接插入。
        exec_cas(&vault, doc_id, None, &a).expect("首次插入");
        assert_eq!(read(&vault, doc_id), Some(a.clone()));

        // 另一个写者仍以为「不存在」，拿着旧的 None 来写 B：必须被 CAS 拒绝。
        let error = exec_cas(&vault, doc_id, None, &b).expect_err("陈旧期望必须失败");
        assert!(
            matches!(error, StorageError::GuardFailed { .. }),
            "CAS 不命中应当是 GuardFailed（调用方映射成明确冲突），实际 {error:?}"
        );
        assert_eq!(
            read(&vault, doc_id),
            Some(a.clone()),
            "CAS 失败绝不能把已有的 A 覆盖成只有 B"
        );

        // 重读后按并集重试：成功且值单调扩大。
        let current = read(&vault, doc_id).expect("已存在");
        let merged = union(&current, &b);
        exec_cas(&vault, doc_id, Some(&current), &merged).expect("并集重试");

        // 再拿一个更旧、且值更小的期望来写：必须继续被拒绝，值不得缩小。
        let stale = vv(0xA1, 1);
        let smaller = vv(0xA1, 1);
        let error = exec_cas(&vault, doc_id, Some(&stale), &smaller).expect_err("更小值必须失败");
        assert!(matches!(error, StorageError::GuardFailed { .. }));
        assert_eq!(
            read(&vault, doc_id),
            Some(merged),
            "失败后必须保持并集，绝不能写入更小的值"
        );
    }

    /// 两路并发写者从**同一旧值**出发写不同 `added`：barrier 对齐后同时 CAS。
    ///
    /// 结果只能是：恰好一方首次成功、另一方拿到明确的 `GuardFailed`；失败方重读后按
    /// 并集重试成功。最终值 = 两方新增的并集（不丢证据、不静默变小）。
    #[test]
    fn concurrent_writers_from_same_old_converge_to_union() {
        let (_dir, vault) = fresh_vault("concurrent");
        let doc_id = "doc-concurrent";
        let base = vv(0x10, 1);
        exec_cas(&vault, doc_id, None, &base).expect("初始值");

        let barrier = Arc::new(Barrier::new(2));
        let (tx, rx) = mpsc::channel();
        let mut handles = Vec::new();
        for peer in [0xA1_u64, 0xB2_u64] {
            let vault = vault.clone();
            let barrier = Arc::clone(&barrier);
            let tx = tx.clone();
            handles.push(thread::spawn(move || {
                let added = vv(peer, 5);
                // 两方都在 barrier 之前读到同一个旧值。
                let old = load(&vault, doc_id).expect("读旧值").expect("已存在");
                let merged = union(&old, &added);
                let statement = cas_statement(doc_id, Some(&old), &merged);
                barrier.wait();
                let first = vault.exec_transaction(&[statement], &[AffectedGuard::exactly(0, 1)]);
                // 失败方（陈旧旧值）重读后重试一次：必须成功并收敛到并集。
                let (first_failed, retried) = match first {
                    Ok(_) => (false, true),
                    Err(StorageError::GuardFailed { .. }) => {
                        let current = load(&vault, doc_id).expect("重读").expect("已存在");
                        let merged = union(&current, &added);
                        (
                            true,
                            exec_cas(&vault, doc_id, Some(&current), &merged).is_ok(),
                        )
                    }
                    Err(other) => panic!("并发 CAS 只允许守卫失败，实际 {other:?}"),
                };
                tx.send((peer, first_failed, retried)).expect("回传结果");
            }));
        }
        drop(tx);
        let results: Vec<(u64, bool, bool)> = rx.iter().take(2).collect();
        for handle in handles {
            handle.join().expect("写者线程不应 panic");
        }
        assert_eq!(results.len(), 2, "两路写者都必须有明确结果");
        assert_eq!(
            results.iter().filter(|(_, failed, _)| *failed).count(),
            1,
            "同一旧值下只允许一方首次成功，另一方必须拿到明确的 CAS 冲突"
        );
        for (peer, _, retried) in &results {
            assert!(*retried, "peer {peer:#x} 重读重试后必须成功");
        }

        let final_vv = read(&vault, doc_id).expect("最终值");
        let expected = union(&union(&base, &vv(0xA1, 5)), &vv(0xB2, 5));
        assert_eq!(final_vv, expected, "最终值必须包含两路 added 的并集");
        assert_eq!(final_vv.get(&0x10).copied(), Some(1), "旧前缀不得丢失");
    }
}

````


## `crates/storage/src/store.rs`

store.rs：append_updates_inner、OriginMarks、remote_meta

````rust
//! `loro_docs` / `loro_updates` 的读写与 A 事务。
//!
//! A 事务（ADR 0001 §3.4）在一次 SQLite 事务里完成三件事：
//!
//! 1. 逐个 append `loro_updates`（seq 连续，`update_id` 唯一）；
//! 2. 以 CAS 方式推进 `loro_docs.head_seq / head_frontier / head_vv`；
//! 3. 写 `projection_outbox.target_seq`（提醒 B 物化器）。
//!
//! CAS 用「该语句必须影响 1 行」的事务守卫实现：一旦 head 已被别人推进，
//! 整笔事务回滚，**不会出现「update 落库了但 head 没动」的半截状态**。

use loro::{Frontiers, VersionVector};
use rusqlite::types::Value;

use crate::actor::{AffectedGuard, Statement};
use crate::body_doc::{BodyDoc, PendingUpdate, APP_DOC_SCHEMA, CODEC_VERSION, MIN_READER};
use crate::error::StorageError;
use crate::Vault;

/// 文档在 `loro_docs` 里的权威行摘要。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadState {
    /// 文档 ID。
    pub doc_id: String,
    /// 已持久化的最后一个 batch seq（0 表示还没有任何 update）。
    pub head_seq: u64,
    /// 已持久化的 frontier（`Frontiers::encode`）。
    pub head_frontier: Vec<u8>,
    /// 已持久化的 VV（`VersionVector::encode`）。
    pub head_vv: Vec<u8>,
}

impl HeadState {
    /// 解码 frontier。
    pub fn frontiers(&self) -> Result<Frontiers, StorageError> {
        if self.head_frontier.is_empty() {
            return Ok(Frontiers::default());
        }
        Frontiers::decode(&self.head_frontier).map_err(StorageError::from)
    }

    /// 解码 VV。全新的文档 `head_vv` 是 NULL/空，等价于空版本向量。
    pub fn version_vector(&self) -> Result<loro::VersionVector, StorageError> {
        if self.head_vv.is_empty() {
            return Ok(loro::VersionVector::default());
        }
        loro::VersionVector::decode(&self.head_vv).map_err(StorageError::from)
    }
}

/// `loro_docs` 的一行（含快照）。
struct StoredDoc {
    snapshot: Option<Vec<u8>>,
    snapshot_seq: u64,
    head: HeadState,
}

/// 新建一个正文文档行（`kind = 'body'`）。
pub fn create_body_doc(vault: &Vault, doc_id: &str) -> Result<(), StorageError> {
    vault.exec(
        "INSERT INTO loro_docs(doc_id, kind, generation, app_doc_schema, codec_version, min_reader) \
         VALUES(?1, 'body', 1, ?2, ?3, ?4)",
        &[
            Value::Text(doc_id.to_owned()),
            Value::Integer(APP_DOC_SCHEMA),
            Value::Text(CODEC_VERSION.to_owned()),
            Value::Integer(MIN_READER),
        ],
    )?;
    Ok(())
}

/// 为一篇笔记准备好投影行，关联它的正文文档。
///
/// 注意：`notes` 是**投影**（ADR 0001 §3.1），最终由结构 CRDT 的投影器写入；
/// 本函数只是 S0 阶段的脚手架，让两阶段物化有落点，后续切片会由投影器接管。
pub fn create_note_slot(
    vault: &Vault,
    note_id: &str,
    body_doc_id: &str,
    title: &str,
    created_at: i64,
) -> Result<(), StorageError> {
    vault.exec(
        "INSERT INTO notes(note_id, body_doc_id, title, created_at, updated_at, deleted, is_favorite, is_pinned) \
         VALUES(?1, ?2, ?3, ?4, ?4, 0, 0, 0)",
        &[
            Value::Text(note_id.to_owned()),
            Value::Text(body_doc_id.to_owned()),
            Value::Text(title.to_owned()),
            Value::Integer(created_at),
        ],
    )?;
    Ok(())
}

/// 读 `loro_docs` 的 head 三件套；行不存在时报 [`StorageError::Corrupt`]。
pub fn load_head(vault: &Vault, doc_id: &str) -> Result<HeadState, StorageError> {
    let rows = vault.query(
        "SELECT head_seq, head_frontier, head_vv FROM loro_docs WHERE doc_id = ?1",
        &[Value::Text(doc_id.to_owned())],
    )?;
    let row = rows
        .into_iter()
        .next()
        .ok_or_else(|| StorageError::Corrupt {
            context: format!("loro_docs 缺少文档 {doc_id}"),
        })?;

    Ok(HeadState {
        doc_id: doc_id.to_owned(),
        head_seq: u64_of(&row, 0, "loro_docs.head_seq")?,
        head_frontier: blob_of(&row, 1, "loro_docs.head_frontier")?,
        head_vv: blob_of(&row, 2, "loro_docs.head_vv")?,
    })
}

/// 读 `loro_docs.head_seq` 仍等于 `expected_head_seq` 时的 `head_vv`（远端已知 VV 的起点）。
///
/// 若 head 已被并发推进（CAS 必然失败、整笔事务回滚），返回空 VV 即可：它不会落库。
fn load_head_vv_at(
    vault: &Vault,
    doc_id: &str,
    expected_head_seq: u64,
) -> Result<VersionVector, StorageError> {
    let rows = vault.query(
        "SELECT head_seq, head_vv FROM loro_docs WHERE doc_id = ?1",
        &[Value::Text(doc_id.to_owned())],
    )?;
    let row = rows
        .into_iter()
        .next()
        .ok_or_else(|| StorageError::Corrupt {
            context: format!("loro_docs 缺少文档 {doc_id}"),
        })?;
    if u64_of(&row, 0, "loro_docs.head_seq")? != expected_head_seq {
        return Ok(VersionVector::default());
    }
    match row.get(1) {
        Some(Value::Null) | None => Ok(VersionVector::default()),
        Some(Value::Blob(bytes)) => VersionVector::decode(bytes).map_err(StorageError::from),
        Some(other) => Err(StorageError::Corrupt {
            context: format!("loro_docs.head_vv 读回异常：{other:?}"),
        }),
    }
}

/// 读文档行 + 快照元信息。
fn load_stored(vault: &Vault, doc_id: &str) -> Result<StoredDoc, StorageError> {
    let rows = vault.query(
        "SELECT snapshot, snapshot_seq, head_seq, head_frontier, head_vv FROM loro_docs WHERE doc_id = ?1",
        &[Value::Text(doc_id.to_owned())],
    )?;
    let row = rows
        .into_iter()
        .next()
        .ok_or_else(|| StorageError::Corrupt {
            context: format!("loro_docs 缺少文档 {doc_id}"),
        })?;

    Ok(StoredDoc {
        snapshot: optional_blob(&row, 0)?,
        snapshot_seq: u64_of(&row, 1, "loro_docs.snapshot_seq")?,
        head: HeadState {
            doc_id: doc_id.to_owned(),
            head_seq: u64_of(&row, 2, "loro_docs.head_seq")?,
            head_frontier: blob_of(&row, 3, "loro_docs.head_frontier")?,
            head_vv: blob_of(&row, 4, "loro_docs.head_vv")?,
        },
    })
}

/// 库里的 frontier 可能是 NULL（从未提交），语义等价于空 frontier。
///
/// 注意：`Frontiers::default().encode()` **不是**空字节，所以不能直接比较原始 bytes。
pub(crate) fn encoded_frontier(head: &HeadState) -> Vec<u8> {
    if head.head_frontier.is_empty() {
        Frontiers::default().encode()
    } else {
        head.head_frontier.clone()
    }
}

/// 重放所需材料：快照 + 后续 update payload + 目标 head。
pub(crate) struct ReplayParts {
    pub(crate) snapshot: Option<Vec<u8>>,
    pub(crate) payloads: Vec<Vec<u8>>,
    pub(crate) head: HeadState,
}

/// 读取重放材料（快照 + 连续 update 前缀），并校验 seq 连续、不越过快照边界。
///
/// `up_to_seq = Some(n)` 只重放到第 n 个 batch（固定 frontier）；`None` 重放到 head。
pub(crate) fn load_replay_parts(
    vault: &Vault,
    doc_id: &str,
    up_to_seq: Option<u64>,
) -> Result<ReplayParts, StorageError> {
    let stored = load_stored(vault, doc_id)?;
    let limit = match up_to_seq {
        Some(seq) => {
            if seq > stored.head.head_seq {
                return Err(StorageError::Corrupt {
                    context: format!(
                        "请求重放到 seq {seq}，但 {doc_id} 的 head_seq 只有 {}",
                        stored.head.head_seq
                    ),
                });
            }
            seq
        }
        None => stored.head.head_seq,
    };
    if limit < stored.snapshot_seq {
        return Err(StorageError::Corrupt {
            context: format!(
                "请求重放到 seq {limit}，但快照已经覆盖到 seq {}；不能倒着物化",
                stored.snapshot_seq
            ),
        });
    }

    let rows = vault.query(
        "SELECT seq, payload FROM loro_updates WHERE doc_id = ?1 AND seq > ?2 AND seq <= ?3 ORDER BY seq",
        &[
            Value::Text(doc_id.to_owned()),
            Value::Integer(i64_of(stored.snapshot_seq)?),
            Value::Integer(i64_of(limit)?),
        ],
    )?;

    let mut payloads = Vec::with_capacity(rows.len());
    for (expected_seq, row) in (stored.snapshot_seq + 1..).zip(rows.iter()) {
        let seq = u64_of(row, 0, "loro_updates.seq")?;
        if seq != expected_seq {
            return Err(StorageError::Corrupt {
                context: format!(
                    "{doc_id} 的 loro_updates 出现断档：期望 seq {expected_seq}，实际 {seq}"
                ),
            });
        }
        payloads.push(blob_of(row, 1, "loro_updates.payload")?);
    }

    let snapshot = if stored.snapshot_seq == 0 {
        None
    } else {
        stored.snapshot.clone()
    };

    let mut head = stored.head;
    if let Some(limit) = up_to_seq {
        head = HeadState {
            doc_id: doc_id.to_owned(),
            head_seq: limit,
            head_frontier: end_frontier_at(vault, doc_id, limit)?,
            head_vv: end_vv_at(vault, doc_id, limit)?,
        };
    }

    Ok(ReplayParts {
        snapshot,
        payloads,
        head,
    })
}

/// 从库里重放出一个 [`BodyDoc`]。
///
/// `up_to_seq = Some(n)` 时只重放到第 n 个 batch，用于**固定 frontier** 的 B 物化；
/// `None` 表示重放到当前 head，并在结尾校验 frontier 与库中记录一致（不一致说明数据损坏）。
pub fn load_body_doc(
    vault: &Vault,
    doc_id: &str,
    peer_id: u64,
    up_to_seq: Option<u64>,
) -> Result<(BodyDoc, HeadState), StorageError> {
    let parts = load_replay_parts(vault, doc_id, up_to_seq)?;
    let doc = BodyDoc::restore(doc_id, peer_id, parts.snapshot.as_deref(), &parts.payloads)?;

    if up_to_seq.is_none() {
        // 启动校验：重放结果必须与库中 head_frontier 一致，否则视为损坏（不回写、不猜）
        let replayed = doc.frontiers().encode();
        let expected = encoded_frontier(&parts.head);
        if replayed != expected {
            return Err(StorageError::Corrupt {
                context: format!(
                    "重放 {doc_id} 得到的 frontier 与 loro_docs.head_frontier 不一致（len {} vs {}）",
                    replayed.len(),
                    expected.len()
                ),
            });
        }
    }

    Ok((doc, parts.head))
}

/// 读某个 batch 落库时的 `end_vv`。
pub fn end_vv_at(vault: &Vault, doc_id: &str, seq: u64) -> Result<Vec<u8>, StorageError> {
    let rows = vault.query(
        "SELECT end_vv FROM loro_updates WHERE doc_id = ?1 AND seq = ?2",
        &[Value::Text(doc_id.to_owned()), Value::Integer(i64_of(seq)?)],
    )?;
    let row = rows
        .into_iter()
        .next()
        .ok_or_else(|| StorageError::Corrupt {
            context: format!("{doc_id} 缺少 seq {seq} 的 loro_updates 行"),
        })?;
    blob_of(&row, 0, "loro_updates.end_vv")
}

/// 读某个 batch 落库时的 `end_frontier`（B 物化的固定 frontier）。
pub fn end_frontier_at(vault: &Vault, doc_id: &str, seq: u64) -> Result<Vec<u8>, StorageError> {
    let rows = vault.query(
        "SELECT end_frontier FROM loro_updates WHERE doc_id = ?1 AND seq = ?2",
        &[Value::Text(doc_id.to_owned()), Value::Integer(i64_of(seq)?)],
    )?;
    let row = rows
        .into_iter()
        .next()
        .ok_or_else(|| StorageError::Corrupt {
            context: format!("{doc_id} 缺少 seq {seq} 的 loro_updates 行"),
        })?;
    blob_of(&row, 0, "loro_updates.end_frontier")
}

/// A 事务里要写入的一条 update（带它自己的 `batch_id`）。
///
/// 一次 flush 的连续前缀可能来自不同逻辑批次（例如慢盘合并了两次提交），
/// 所以 `batch_id` 是**逐行**的，不能整批共用一个。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateRecord {
    /// 该 update 所属的逻辑批次。
    pub batch_id: String,
    /// 增量内容。
    pub update: PendingUpdate,
}

impl UpdateRecord {
    /// 构造一条记录。
    pub fn new(batch_id: impl Into<String>, update: PendingUpdate) -> Self {
        Self {
            batch_id: batch_id.into(),
            update,
        }
    }
}

/// 把一份快照落库，记录它覆盖到的 seq 与 frontier。
///
/// 本切片只提供落库与「snapshot + 后续 update」重放路径；快照的生成节奏与旧 update 的
/// 真正 GC（ADR 0001 §3.5）留给后续切片。快照不能倒退覆盖更早的 seq。
pub fn store_snapshot(
    vault: &Vault,
    doc_id: &str,
    seq: u64,
    frontier: &[u8],
    snapshot: &[u8],
) -> Result<(), StorageError> {
    if seq == 0 || seq > load_head(vault, doc_id)?.head_seq {
        return Err(StorageError::Corrupt {
            context: format!("快照 seq {seq} 不在 (0, head] 区间内"),
        });
    }
    let affected = vault.exec(
        "UPDATE loro_docs SET snapshot = ?2, snapshot_seq = ?3, snapshot_frontier = ?4 \
         WHERE doc_id = ?1 AND snapshot_seq <= ?3",
        &[
            Value::Text(doc_id.to_owned()),
            Value::Blob(snapshot.to_vec()),
            Value::Integer(i64_of(seq)?),
            Value::Blob(frontier.to_vec()),
        ],
    )?;
    if affected != 1 {
        return Err(StorageError::Corrupt {
            context: format!("{doc_id} 的快照 seq 倒退，拒绝覆盖"),
        });
    }
    Ok(())
}

/// A 事务：追加一批 update + 推进 head + 写 projection_outbox。
///
/// `expected_head_seq` 是调用方看到的 head（CAS 基准）；不匹配时整笔回滚并返回
/// [`StorageError::HeadConflict`]，绝不覆盖别人的新结果。
pub fn append_updates(
    vault: &Vault,
    doc_id: &str,
    expected_head_seq: u64,
    records: &[UpdateRecord],
) -> Result<HeadState, StorageError> {
    append_updates_inner(
        vault,
        doc_id,
        expected_head_seq,
        records,
        &[],
        &[],
        OriginMarks::AllLocal,
    )
}

/// A 事务：追加**一条**组合 update，并把同一批 remote inbox 行标为 `integrated`。
///
/// Phase 1A-2 的聚合 integrate 入口：同一文档的多条 inbox 行先在内存里逐条
/// `BodyDoc::import`，再把 `before..end` 的增量合成**一条** `UpdateRecord` 落盘；
/// 「追加 update + 推进 head + projection_outbox + 标记 inbox 行」全部在**同一个**
/// A 事务里（守卫要求每条 `UPDATE sync_inbox` 恰好影响 1 行，任何一行不存在/已不是
/// `verified` 都会整笔回滚，不会出现「update 写了但 inbox 没标」的半份状态）。
///
/// 与 [`append_updates`] 的差别只有同事务的批量标记：
/// - `relay_seqs` 为空时行为与 [`append_updates`] 逐字节一致（仍然只有一条记录）；
/// - 本函数**不推进** `sync.integrated_seq`；调用方在整批正文都完成后统一调用
///   [`crate::advance_integrated_prefix`]，避免每个 doc 组各写一次前缀；
/// - 校验失败映射：head CAS 失败仍是 [`StorageError::HeadConflict`]；inbox 标记失败
///   （行不存在 / 已 integrated / 已 quarantine）映射为 [`StorageError::SyncConflict`]，
///   两种情况下 update 都没有落库；
/// - 同事务把 `end_vv - 旧 head_vv`（= 这条远端消息实际带来的 ops）并入本机的
///   「远端已知 VV」（`sync.remote_known_vv.<doc_id>`，non-destructive meta），
///   供 `baseline_apply` 的 tombstone 判定区分已同步文档与 local-only 文档。
pub fn append_updates_with_integrated_marks(
    vault: &Vault,
    doc_id: &str,
    expected_head_seq: u64,
    record: &UpdateRecord,
    log_id: &[u8; 16],
    relay_seqs: &[u64],
) -> Result<HeadState, StorageError> {
    let marks: Vec<(&[u8; 16], u64)> = relay_seqs
        .iter()
        .map(|relay_seq| (log_id, *relay_seq))
        .collect();
    append_updates_inner(
        vault,
        doc_id,
        expected_head_seq,
        std::slice::from_ref(record),
        &[],
        &marks,
        // engine integrate 的这条组合 update 全部来自远端消息：其 `end_vv - 旧 head_vv`
        // 就是远端实际带来的 ops，记入「远端已知 VV」（tombstone 判定用）。
        OriginMarks::PerRecord(&[true]),
    )
}

/// 一次 A 事务里每条 update 的**来源标记**。
///
/// 这是 P2-8 修掉的类型陷阱：旧接口用 `&[bool]` 表示来源，**空切片 = 全部本地**；
/// 而同一次调用的 `slots` 也是切片，**空切片 = 没有任何出站意图**。两个空切片含义
/// 相反、类型相同，调用点很容易把「没有意图」写成「全部本地」或反之。这里把来源标记
/// 收成枚举，让「空」不再是两种含义：
///
/// - [`OriginMarks::AllLocal`]：本批记录全部是本地写入，**不**扩大远端已知 VV；
/// - [`OriginMarks::PerRecord`]：与 `records` **一一对应**（长度不匹配直接报
///   [`StorageError::Corrupt`]），`true` = 该条来自远端消息，其
///   `end_vv - 前一条 end_vv` 并入「远端已知 VV」。
///
/// 本地写入即使已经 ACK 也不会扩大远端已知 VV（tombstone 判定只允许更保守）。
#[derive(Debug, Clone, Copy)]
pub enum OriginMarks<'a> {
    /// 全部记录都是本地写入。
    AllLocal,
    /// 逐条标记（必须与 records 等长）。
    PerRecord(&'a [bool]),
}

impl<'a> OriginMarks<'a> {
    /// 本批是否有远端条目。
    fn any_remote(self) -> bool {
        match self {
            Self::AllLocal => false,
            Self::PerRecord(marks) => marks.iter().any(|flag| *flag),
        }
    }

    /// 第 `index` 条是否来自远端（`AllLocal` 恒为 false）。
    fn is_remote(self, index: usize) -> bool {
        match self {
            Self::AllLocal => false,
            Self::PerRecord(marks) => marks.get(index).copied().unwrap_or(false),
        }
    }

    /// 校验来源标记与 records 的对应关系。
    ///
    /// `AllLocal` 不需要长度；`PerRecord` 必须与 records 等长——**空 records 配非空
    /// marks、或 records 配空 marks 都会在这里被拒绝**，不会退化成「全部本地」。
    fn validate(self, records_len: usize) -> Result<(), StorageError> {
        if let Self::PerRecord(marks) = self {
            if marks.len() != records_len {
                return Err(StorageError::Corrupt {
                    context: format!(
                        "append_updates: records={records_len} \
                         OriginMarks::PerRecord={} 必须一对一",
                        marks.len()
                    ),
                });
            }
        }
        Ok(())
    }
}

/// A 事务内同时写出站同步意图（按位置一一对应；`None` = 该条不产生意图）。
///
/// 与 [`append_updates_with_sync_intents`] 相比，这里允许"某些 update 没有意图"：
/// 一次 flush 的前缀可能混本地编辑与 remote 导入，而 remote 导入**绝不能**产生
/// 出站意图（ADR 0009）。仍然只有**一条** A 事务，失败即整笔回滚。
///
/// 参数语义（P2-8 钉死）：
/// - `slots` 空 = 本批**没有任何**出站意图（不是"每条都要一条意图"）；`None` 位 = 该条
///   不产生意图；
/// - `origin` 见 [`OriginMarks`]：只有远端条目会扩大远端已知 VV，本地条目永远不扩大。
pub(crate) fn append_updates_with_intent_slots(
    vault: &Vault,
    doc_id: &str,
    expected_head_seq: u64,
    records: &[UpdateRecord],
    slots: &[Option<crate::sync_bridge::SyncIntent>],
    origin: OriginMarks<'_>,
) -> Result<HeadState, StorageError> {
    append_updates_inner(
        vault,
        doc_id,
        expected_head_seq,
        records,
        slots,
        &[],
        origin,
    )
}

/// 同事务附加的 inbox 标记：`(log_id, relay_seq)`，每个都要求恰好命中一行 `verified`。
type IntegratedMarks<'a> = &'a [(&'a [u8; 16], u64)];

fn append_updates_inner(
    vault: &Vault,
    doc_id: &str,
    expected_head_seq: u64,
    records: &[UpdateRecord],
    slots: &[Option<crate::sync_bridge::SyncIntent>],
    integrated_marks: IntegratedMarks<'_>,
    origin: OriginMarks<'_>,
) -> Result<HeadState, StorageError> {
    if records.is_empty() {
        if !slots.is_empty() {
            return Err(StorageError::Corrupt {
                context: "空 update 批次不能带 sync intent".to_owned(),
            });
        }
        if !integrated_marks.is_empty() {
            return Err(StorageError::Corrupt {
                context: "空 update 批次不能带 inbox 标记".to_owned(),
            });
        }
        // `PerRecord` 配空 records 时，非空标记同样算长度不匹配（不会退化成全部本地）。
        origin.validate(0)?;
        return load_head(vault, doc_id);
    }
    if !slots.is_empty() && slots.len() != records.len() {
        return Err(StorageError::Corrupt {
            context: format!(
                "append_updates: records={} intents={} 必须一对一",
                records.len(),
                slots.len()
            ),
        });
    }
    origin.validate(records.len())?;

    // 远端已知 VV 的增量：只有 remote 条目会扩大它（`after_vv - running_vv`）。
    //
    // 这里只算「本批新增了哪些远端 ops」；旧值读取与 CAS 语句在构造语句阶段生成，
    // 重试时按最新旧值重算并集，但 `added` 本身不变（head 基准没变）。
    let remote_added = if origin.any_remote() {
        let mut running = load_head_vv_at(vault, doc_id, expected_head_seq)?;
        let mut known = VersionVector::new();
        for (index, record) in records.iter().enumerate() {
            let after = VersionVector::decode(&record.update.end_vv).map_err(StorageError::from)?;
            if origin.is_remote(index) {
                let added = crate::remote_known::added_since(&after, &running);
                known = crate::remote_known::union(&known, &added);
            }
            running = after;
        }
        if known.is_empty() {
            None
        } else {
            Some(known)
        }
    } else {
        None
    };

    let last = &records
        .last()
        .ok_or_else(|| StorageError::Corrupt {
            context: "append_updates 收到空批次".to_owned(),
        })?
        .update;
    let head_seq = expected_head_seq + records.len() as u64;

    let mut statements = Vec::with_capacity(records.len() + 2 + slots.len());

    // 第 0 条就是 CAS：head_seq 必须还是调用方看到的那个值。
    // 放在最前面，过期基准会在碰到任何唯一约束之前就被守卫拦下，报 HeadConflict 而不是 UNIQUE。
    statements.push(Statement::new(
        "UPDATE loro_docs SET head_seq = ?2, head_frontier = ?3, head_vv = ?4 \
         WHERE doc_id = ?1 AND head_seq = ?5",
        vec![
            Value::Text(doc_id.to_owned()),
            Value::Integer(i64_of(head_seq)?),
            Value::Blob(last.end_frontier.clone()),
            Value::Blob(last.end_vv.clone()),
            Value::Integer(i64_of(expected_head_seq)?),
        ],
    ));

    for (index, record) in records.iter().enumerate() {
        let seq = expected_head_seq + 1 + index as u64;
        statements.push(Statement::new(
            "INSERT INTO loro_updates(doc_id, seq, update_id, batch_id, codec_version, payload, end_frontier, end_vv) \
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            vec![
                Value::Text(doc_id.to_owned()),
                Value::Integer(i64_of(seq)?),
                Value::Text(format!("{doc_id}#{seq}")),
                Value::Text(record.batch_id.clone()),
                Value::Text(CODEC_VERSION.to_owned()),
                Value::Blob(record.update.payload.clone()),
                Value::Blob(record.update.end_frontier.clone()),
                Value::Blob(record.update.end_vv.clone()),
            ],
        ));
    }

    statements.push(Statement::new(
        "INSERT INTO projection_outbox(doc_id, target_seq) VALUES(?1, ?2) \
         ON CONFLICT(doc_id) DO UPDATE SET target_seq = MAX(target_seq, excluded.target_seq)",
        vec![
            Value::Text(doc_id.to_owned()),
            Value::Integer(i64_of(head_seq)?),
        ],
    ));

    for (index, slot) in slots.iter().enumerate() {
        // `None`：这条 update 不产生出站意图（remote 导入 / adopt 等），同事务里跳过。
        let Some(intent) = slot else {
            continue;
        };
        if intent.message_id.is_empty() || intent.batch_id.is_empty() || intent.key_epoch == 0 {
            return Err(StorageError::Corrupt {
                context: "sync intent message/batch/epoch 非法".to_owned(),
            });
        }
        let seq = expected_head_seq + 1 + index as u64;
        statements.push(Statement::new(
            "INSERT INTO sync_outbox(message_id,batch_id,doc_id,seq,key_epoch,object_id,state,ciphertext) \
             VALUES(?1,?2,?3,?4,?5,?6,'pending',NULL)",
            vec![
                Value::Text(intent.message_id.clone()),
                Value::Text(intent.batch_id.clone()),
                Value::Text(doc_id.to_owned()),
                Value::Integer(i64_of(seq)?),
                Value::Integer(i64_of(intent.key_epoch)?),
                Value::Blob(intent.object_id.to_vec()),
            ],
        ));
    }

    // 远端已知 VV 的 meta 写入（与 update/head 同一 A 事务；失败一起回滚）。
    //
    // **值 CAS**：语句里带读到的旧值，只有库中值仍是旧值才会更新（`WHERE meta.value=?old`，
    // 旧值不存在时用 `INSERT ... WHERE NOT EXISTS`）。这样两个写者（普通 pull 的 A 事务
    // 与 baseline apply 批次）即使都拿着同一份旧并集，后到的那个也不会把先到的扩大值
    // 覆盖回更小的并集；CAS 不命中 → 0 行 → 守卫失败 → 整笔回滚后重读重试。
    let remote_slot = match &remote_added {
        Some(added) => {
            let index = statements.len();
            statements.push(cas_statement_or_corrupt(
                crate::remote_known::merge_statement(vault, doc_id, added)?,
                doc_id,
            )?);
            Some(index)
        }
        None => None,
    };

    // 同事务标记 inbox 行：每条都要求恰好影响 1 行（不存在 / 已 integrated /
    // 已 quarantine 都会让整笔 A 事务回滚）。标记语句放在最后，避免它们的守卫先于
    // head CAS 暴露，保证并发冲突仍然报 HeadConflict。
    let marks_start = statements.len();
    for (mark_log_id, relay_seq) in integrated_marks {
        statements.push(Statement::new(
            "UPDATE sync_inbox SET state='integrated' \
             WHERE log_id=?1 AND relay_seq=?2 AND state='verified'",
            vec![
                Value::Blob(mark_log_id.to_vec()),
                Value::Integer(i64_of(*relay_seq)?),
            ],
        ));
    }
    let mut guards = Vec::with_capacity(2 + integrated_marks.len());
    guards.push(AffectedGuard::exactly(0, 1));
    if let Some(index) = remote_slot {
        guards.push(AffectedGuard::exactly(index, 1));
    }
    guards.extend(
        integrated_marks
            .iter()
            .enumerate()
            .map(|(index, _)| AffectedGuard::exactly(marks_start + index, 1)),
    );

    // 远端已知 VV 的 CAS 失败是**可重试**的：并发写者只是先扩大了一步，重读旧值后
    // 把两边并集再写一次即可。重试期间 head CAS 仍在 index 0：若真的有人推进了 head，
    // 依旧按 HeadConflict 返回，不会把「基准过期」误当成 meta 冲突。
    const REMOTE_KNOWN_CAS_ATTEMPTS: usize = 4;
    let mut attempt = 0usize;
    loop {
        attempt += 1;
        match vault.exec_transaction(&statements, &guards) {
            Ok(_) => {
                return Ok(HeadState {
                    doc_id: doc_id.to_owned(),
                    head_seq,
                    head_frontier: last.end_frontier.clone(),
                    head_vv: last.end_vv.clone(),
                });
            }
            // 第 0 条守卫 = head CAS。
            Err(StorageError::GuardFailed { statement: 0, .. }) => {
                let actual = load_head(vault, doc_id)
                    .map(|head| head.head_seq)
                    .unwrap_or(0);
                return Err(StorageError::HeadConflict {
                    doc_id: doc_id.to_owned(),
                    expected: expected_head_seq,
                    actual,
                });
            }
            // 远端已知 VV 的值 CAS 不命中：指定下标只有它这一条守卫。
            Err(StorageError::GuardFailed { statement, .. }) if remote_slot == Some(statement) => {
                let Some(added) = remote_added.as_ref() else {
                    return Err(StorageError::Corrupt {
                        context: format!("文档 {doc_id} 的远端已知 VV CAS 守卫缺少重算依据"),
                    });
                };
                if attempt >= REMOTE_KNOWN_CAS_ATTEMPTS {
                    return Err(StorageError::SyncConflict {
                        context: format!(
                            "文档 {doc_id} 的远端已知 VV 值 CAS 连续 {attempt} 次被并发写者改写；\
                             整笔 A 事务（update+head+meta）已回滚，可重试"
                        ),
                    });
                }
                let slot = remote_slot.expect("守卫命中说明 remote_slot 存在");
                statements[slot] = cas_statement_or_corrupt(
                    crate::remote_known::merge_statement(vault, doc_id, added)?,
                    doc_id,
                )?;
            }
            // 标记失败：update 与所有标记一起回滚；调用方必须保持 inbox verified 原样。
            Err(StorageError::GuardFailed { statement, .. }) if statement >= marks_start => {
                return Err(StorageError::SyncConflict {
                    context: format!(
                        "inbox 标记 integrated 失败（第 {} 个标记行不存在或当前不是 verified）；\
                         整笔 A 事务（update+标记）已回滚",
                        statement - marks_start
                    ),
                });
            }
            Err(error) => return Err(error),
        }
    }
}

/// [`crate::remote_known::merge_statement`] 在 `remote_added` 非空时必然返回语句；
/// 这里把它转成带文档上下文的损坏错误，避免业务路径 `unwrap`。
fn cas_statement_or_corrupt(
    statement: Option<Statement>,
    doc_id: &str,
) -> Result<Statement, StorageError> {
    statement.ok_or_else(|| StorageError::Corrupt {
        context: format!("文档 {doc_id} 的远端已知 VV 增量非空，但没有生成 CAS 语句"),
    })
}

/// 笔记当前的物化水位（供 B 事务与测试读取）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BodyState {
    /// 笔记 ID。
    pub note_id: String,
    /// 已物化的 body_md。
    pub body_md: Option<String>,
    /// 已物化的 batch seq。
    pub body_seq: u64,
    /// 已物化的 frontier。
    pub body_frontier: Option<Vec<u8>>,
}

/// 读一篇笔记的物化状态。
pub fn load_body_state(vault: &Vault, note_id: &str) -> Result<BodyState, StorageError> {
    let rows = vault.query(
        "SELECT body_md, body_seq, body_frontier FROM notes WHERE note_id = ?1",
        &[Value::Text(note_id.to_owned())],
    )?;
    let row = rows
        .into_iter()
        .next()
        .ok_or_else(|| StorageError::Corrupt {
            context: format!("notes 缺少 {note_id}"),
        })?;

    let body_md = match row.first() {
        Some(Value::Text(text)) => Some(text.clone()),
        Some(Value::Null) | None => None,
        Some(other) => {
            return Err(StorageError::Corrupt {
                context: format!("notes.body_md 类型异常：{other:?}"),
            });
        }
    };

    Ok(BodyState {
        note_id: note_id.to_owned(),
        body_md,
        body_seq: u64_of(&row, 1, "notes.body_seq")?,
        body_frontier: optional_blob(&row, 2)?,
    })
}

fn i64_of(value: u64) -> Result<i64, StorageError> {
    i64::try_from(value).map_err(|_| StorageError::Corrupt {
        context: format!("数值 {value} 超出 SQLite INTEGER 范围"),
    })
}

fn u64_of(row: &[Value], index: usize, field: &str) -> Result<u64, StorageError> {
    match row.get(index) {
        Some(Value::Integer(value)) if *value >= 0 => Ok(*value as u64),
        other => Err(StorageError::Corrupt {
            context: format!("{field} 读回异常：{other:?}"),
        }),
    }
}

fn blob_of(row: &[Value], index: usize, field: &str) -> Result<Vec<u8>, StorageError> {
    match row.get(index) {
        Some(Value::Blob(bytes)) => Ok(bytes.clone()),
        Some(Value::Null) | None => Ok(Vec::new()),
        other => Err(StorageError::Corrupt {
            context: format!("{field} 读回异常：{other:?}"),
        }),
    }
}

fn optional_blob(row: &[Value], index: usize) -> Result<Option<Vec<u8>>, StorageError> {
    match row.get(index) {
        Some(Value::Blob(bytes)) => Ok(Some(bytes.clone())),
        Some(Value::Null) | None => Ok(None),
        other => Err(StorageError::Corrupt {
            context: format!("期望 BLOB/NULL，实际 {other:?}"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use loro::VersionVector;

    use super::*;
    use crate::body_doc::BodyDoc;
    use crate::remote_known;

    /// 临时 vault：目录建在 `target/tmp` 下（AGENTS §6：不用 `%TEMP%`）。
    ///
    /// lib 单元测试拿不到 `CARGO_TARGET_TMPDIR`（只有集成测试有），所以从
    /// `CARGO_MANIFEST_DIR` 拼出同一个位置。
    fn fresh_vault(prefix: &str) -> (tempfile::TempDir, Vault) {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/tmp")
            .join(format!("store-origin-{prefix}"));
        std::fs::create_dir_all(&root).expect("创建 target/tmp 下的测试根目录");
        let dir = tempfile::Builder::new()
            .prefix("vault-")
            .tempdir_in(&root)
            .expect("创建临时目录");
        let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
        (dir, vault)
    }

    fn scalar_i64(vault: &Vault, sql: &str) -> i64 {
        let rows = vault.query(sql, &[]).expect("查询");
        match rows.first().and_then(|row| row.first()) {
            Some(Value::Integer(value)) => *value,
            other => panic!("期望整数结果，实际 {other:?}"),
        }
    }

    fn local_update(doc: &BodyDoc, base: &VersionVector) -> PendingUpdate {
        doc.pending_update(base).expect("导出增量")
    }

    /// `OriginMarks::AllLocal`（显式类型）不扩大远端已知 VV，也不产生 outbox。
    #[test]
    fn all_local_origin_does_not_extend_remote_known() {
        let (_dir, vault) = fresh_vault("all-local");
        create_body_doc(&vault, "doc-local").expect("loro_docs 行");
        let doc = BodyDoc::create("doc-local", 0xA1).expect("创建 BodyDoc");
        doc.insert_scalars(0, "local").expect("本地编辑");
        let update = local_update(&doc, &VersionVector::default());

        let head = append_updates_inner(
            &vault,
            "doc-local",
            0,
            &[UpdateRecord::new("b0", update)],
            &[],
            &[],
            OriginMarks::AllLocal,
        )
        .expect("A 事务");

        assert_eq!(head.head_seq, 1);
        assert!(
            remote_known::load(&vault, "doc-local")
                .expect("读远端已知")
                .is_none(),
            "全部本地的批次绝不能创建/扩大远端已知 VV"
        );
        assert_eq!(
            scalar_i64(&vault, "SELECT count(*) FROM sync_outbox"),
            0,
            "slots 空 = 本批没有任何出站意图"
        );
    }

    /// `PerRecord` 混排：只有 remote 条目的 `after - running` 记入远端已知 VV。
    #[test]
    fn per_record_origin_only_extends_for_remote_records() {
        let (_dir, vault) = fresh_vault("mixed");
        create_body_doc(&vault, "doc-mixed").expect("loro_docs 行");

        let doc = BodyDoc::create("doc-mixed", 0xA1).expect("创建 BodyDoc");
        doc.insert_scalars(0, "local").expect("本地编辑");
        let local = local_update(&doc, &VersionVector::default());
        let remote_doc = BodyDoc::create("doc-mixed", 0xB2).expect("创建 BodyDoc");
        remote_doc.insert_scalars(0, "remote").expect("远端编辑");
        doc.import(&remote_doc.export_all_updates().expect("远端 update"))
            .expect("本地副本导入远端");
        let local_end = VersionVector::decode(&local.end_vv).expect("本地 VV");
        let remote = local_update(&doc, &local_end);
        let remote_end = VersionVector::decode(&remote.end_vv).expect("远端 VV");
        let expected = remote_known::added_since(&remote_end, &local_end);

        let head = append_updates_inner(
            &vault,
            "doc-mixed",
            0,
            &[
                UpdateRecord::new("b0", local),
                UpdateRecord::new("b1", remote),
            ],
            &[],
            &[],
            OriginMarks::PerRecord(&[false, true]),
        )
        .expect("A 事务");

        assert_eq!(head.head_seq, 2, "两条记录都必须落库");
        let known = remote_known::load(&vault, "doc-mixed")
            .expect("读远端已知")
            .expect("有 remote 条目就必须写 meta");
        assert_eq!(
            known, expected,
            "远端条目带来的 ops 必须记入，且只记入远端那一小段"
        );
        assert_eq!(
            known.get(&0xA1).copied().unwrap_or(0),
            0,
            "本地条目（哪怕同批先落库）绝不能记入远端已知 VV"
        );
        assert_eq!(
            scalar_i64(&vault, "SELECT count(*) FROM sync_outbox"),
            0,
            "没有传 slots 就不得写出站意图"
        );
    }

    /// 空 `PerRecord` 不再表示「全部本地」：长度不匹配直接拒绝，且不写任何行。
    #[test]
    fn per_record_origin_length_mismatch_is_rejected() {
        let (_dir, vault) = fresh_vault("mismatch");
        create_body_doc(&vault, "doc-mismatch").expect("loro_docs 行");
        let doc = BodyDoc::create("doc-mismatch", 0xA1).expect("创建 BodyDoc");
        doc.insert_scalars(0, "x").expect("编辑");
        let update = local_update(&doc, &VersionVector::default());
        let record = UpdateRecord::new("b0", update);

        let error = append_updates_inner(
            &vault,
            "doc-mismatch",
            0,
            std::slice::from_ref(&record),
            &[],
            &[],
            OriginMarks::PerRecord(&[]),
        )
        .expect_err("空 PerRecord 配非空 records 必须报错");
        assert!(
            matches!(error, StorageError::Corrupt { .. }),
            "必须是显式损坏/参数错误，实际 {error:?}"
        );
        assert_eq!(
            scalar_i64(&vault, "SELECT count(*) FROM loro_updates"),
            0,
            "参数错误不得留下半写"
        );

        let error = append_updates_inner(
            &vault,
            "doc-mismatch",
            0,
            &[record],
            &[],
            &[],
            OriginMarks::PerRecord(&[true, false]),
        )
        .expect_err("标记多于 records 也必须报错");
        assert!(matches!(error, StorageError::Corrupt { .. }));
    }

    /// 值 CAS 不命中（并发写者改了 meta）→ 有界重试后返回明确 SyncConflict，
    /// 整笔 A 事务回滚：head/update/outbox/meta 都不留半写，也绝不写入更小值。
    ///
    /// 注入方式：`loro_updates` 的 BEFORE INSERT 触发器在**同一事务里**把 meta 改成
    /// 别的值（模拟「读旧值之后、CAS 之前的并发提交」）；每次重试都会再次命中，
    /// 因此有界重试必然耗尽。
    #[test]
    fn stale_remote_known_cas_fails_closed_and_rolls_back() {
        let (_dir, vault) = fresh_vault("cas");
        create_body_doc(&vault, "doc-cas").expect("loro_docs 行");
        let remote_doc = BodyDoc::create("doc-cas", 0xB2).expect("创建 BodyDoc");
        remote_doc.insert_scalars(0, "remote").expect("远端编辑");
        let update = local_update(&remote_doc, &VersionVector::default());
        vault
            .exec(
                "CREATE TRIGGER remote_known_race BEFORE INSERT ON loro_updates BEGIN \
                   INSERT OR REPLACE INTO meta(key,value) \
                   VALUES('sync.remote_known_vv.doc-cas', X'DEADBEEF'); \
                 END;",
                &[],
            )
            .expect("安装竞态触发器");

        let error = append_updates_inner(
            &vault,
            "doc-cas",
            0,
            &[UpdateRecord::new("b0", update)],
            &[],
            &[],
            OriginMarks::PerRecord(&[true]),
        )
        .expect_err("CAS 必须失败");

        match error {
            StorageError::SyncConflict { context } => {
                assert!(
                    context.contains("远端已知 VV"),
                    "错误必须点明远端已知 VV CAS：{context}"
                );
                assert!(context.contains("可重试"), "错误必须说明可重试：{context}");
            }
            other => panic!("期望 SyncConflict，实际 {other:?}"),
        }
        assert_eq!(load_head(&vault, "doc-cas").expect("head").head_seq, 0);
        assert_eq!(
            scalar_i64(&vault, "SELECT count(*) FROM loro_updates"),
            0,
            "整笔回滚：update 不得落库"
        );
        assert_eq!(
            scalar_i64(&vault, "SELECT count(*) FROM projection_outbox"),
            0
        );
        assert_eq!(
            scalar_i64(
                &vault,
                "SELECT count(*) FROM meta WHERE key='sync.remote_known_vv.doc-cas'"
            ),
            0,
            "触发器写入随整笔回滚，失败后不残留"
        );
        assert!(
            remote_known::load(&vault, "doc-cas")
                .expect("读远端已知")
                .is_none(),
            "失败路径绝不能写入更小/错误的值"
        );
    }
}

````


## `crates/storage/src/sync_bridge.rs`

sync_bridge.rs：ready/count log_id、批量 inbox、integrated 前缀

````rust
//! S7 同步桥：出站意图 → 加密 parts → 推送/ACK；入站 verified inbox → 本地 seq → integrate。
//!
//! 本模块只碰 SQLite；不做 Argon、AEAD、HTTP、Loro 解码。ADR 0006 §5 的
//! 关键约束：
//!
//! - A 事务只写「出站意图」，worker 在事务外加密后 CAS 落 parts；
//! - 同一 message/epoch 的既有 parts 必须与再次提交的字节完全一致，否则冲突；
//! - ACK 用 `(message_id, epoch, object_digest)` CAS，旧 ACK 不能删除新行；
//! - 入站对象先 durable 写 inbox（依赖未齐也保存），再由 sync 层决定何时可 integrate；
//! - `allocate_local_seq_for_import` 只返回 `head_seq+1`，**绝不把 sender_seq 写进 loro_updates.seq`。

use std::collections::BTreeMap;

use rusqlite::types::Value;

use crate::actor::{AffectedGuard, Statement};
use crate::error::StorageError;
use crate::store;
use crate::Vault;

// 同步 `meta` 键与「连续前缀」的低层实现放在 `sync_meta`（直接跑在 `Connection` 上，
// 这样批量 inbox 的插入与前缀推进能共用一个事务）；这里保持原有的公开路径。
use crate::sync_meta::{meta_bytes_of, skipped_received_key, ConsumedPrefix};
pub use crate::sync_meta::{
    META_SYNC_CONFIRMED_HIGHWATER, META_SYNC_INTEGRATED_SEQ, META_SYNC_LOG_ID,
    META_SYNC_RECEIVED_SEQ, META_SYNC_SKIPPED_RECEIVED_PREFIX,
};

/// 出站意图（A 事务内与 update 一起落库）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncIntent {
    /// 稳定 message id（UUID 文本；parts 表以 UTF-8 字节存同一值）。
    pub message_id: String,
    /// 逻辑 batch，通常与 `UpdateRecord.batch_id` 相同。
    pub batch_id: String,
    /// 加密 epoch。
    pub key_epoch: u64,
    /// 对象名（32B；asset 为 HMAC 对象名，其他为随机 32B）。
    pub object_id: [u8; 32],
}

/// 本地写入的**同步上下文**：本机 peer + 当前加密 epoch。
///
/// ADR 0009 切片 2b：真实本地写入（正文 session flush、结构提交）在 A 事务里
/// 顺手写 `sync_outbox` 意图。这里只放"派生意图所需的常量"：
///
/// - `peer_id`：来自持久设备身份（`LocalIdentityV1::loro_peer_id()`），**不是**常量 1；
/// - `key_epoch`：Phase 1 恒为 `1`（ADR 0006 genesis 初始代），**禁止**写成 0
///   （0 在 `validate_intent` 里就是非法值，会让整笔 A 事务失败）。
///
/// `None` 表示不产生意图（默认路径 / remote 导入路径），行为与切片 2a 之前逐字节一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SyncContext {
    /// 本机 Loro peer id（`0` 非法：Loro 不接受 0 peer）。
    pub peer_id: u64,
    /// 当前加密 epoch（Phase 1 = 1；必须 > 0）。
    pub key_epoch: u64,
}

impl SyncContext {
    /// 构造上下文（调用方保证两个值都 > 0；不做隐式回退）。
    #[must_use]
    pub const fn new(peer_id: u64, key_epoch: u64) -> Self {
        Self { peer_id, key_epoch }
    }

    /// 校验：peer / epoch 都必须 > 0，否则明确报错（不静默替换成常量）。
    pub fn validate(self) -> Result<(), StorageError> {
        if self.peer_id == 0 {
            return Err(StorageError::Corrupt {
                context: "SyncContext.peer_id 必须 >0（0 不是合法 Loro peer）".to_owned(),
            });
        }
        if self.key_epoch == 0 {
            return Err(StorageError::Corrupt {
                context: "SyncContext.key_epoch 必须 >0（Phase 1 = 1，禁止常量 0）".to_owned(),
            });
        }
        Ok(())
    }
}

/// 本地意图 ID 的确定性派生域（版本化；改算法必须换常量）。
///
/// 三个派生都从这里起头：message id / object id 必须**纯函数**地由
/// `(doc_id, seq, peer, epoch)` 决定，重试才不会产生新 ID（ADR 0009）。
pub const SYNC_INTENT_ID_DOMAIN: &[u8] = b"MN/dev-sync-intent/v1\0";

/// 确定性 message id（32 位小写 hex = 16B wire id）。
///
/// **为什么不是可读串**：`crates/sync` 的 `engine::decode_message_id` 只接受
/// 16 字节的 hax/UUID 文本（32 个 hex 字符，允许带 `-`），可读的
/// `doc#seq@epoch` 会让 engine 直接报 `LocalCorrupt`。所以这里取
/// `sha256(domain‖"message"‖doc_id‖NUL‖seq_be‖peer_be‖epoch_be)` 的前 16B。
///
/// 纯函数语义：同一 `(doc_id, seq, peer_id, key_epoch)` 永远得到同一个 id；
/// 不同 seq / epoch 必然不同（hash 输入不同）。
#[must_use]
pub fn intent_message_id(doc_id: &str, seq: u64, ctx: SyncContext) -> String {
    let digest = intent_digest(
        b"message\0",
        &[
            doc_id.as_bytes(),
            &[0],
            &seq.to_be_bytes(),
            &ctx.peer_id.to_be_bytes(),
            &ctx.key_epoch.to_be_bytes(),
        ],
    );
    hex_lower(&digest[..16])
}

/// 确定性 object id（32B）。
///
/// `sha256(domain‖"object"‖message_id‖peer_be‖epoch_be)`：对象名与 message 一一对应，
/// 重试/重打包时同样的 message 得到同样的对象名。
#[must_use]
pub fn intent_object_id(message_id: &str, ctx: SyncContext) -> [u8; 32] {
    intent_digest(
        b"object\0",
        &[
            message_id.as_bytes(),
            &ctx.peer_id.to_be_bytes(),
            &ctx.key_epoch.to_be_bytes(),
        ],
    )
}

/// 为一条**本地** update 派生一条出站意图。
///
/// `seq` 必须是这次 A 事务里该 update 真正落库的 `loro_updates.seq`
/// （= `expected_head_seq + 1 + index`），否则意图与数据对不上。
pub fn local_sync_intent(
    doc_id: &str,
    seq: u64,
    batch_id: &str,
    ctx: SyncContext,
) -> Result<SyncIntent, StorageError> {
    ctx.validate()?;
    if doc_id.is_empty() {
        return Err(StorageError::Corrupt {
            context: "sync intent doc_id 不能为空".to_owned(),
        });
    }
    let message_id = intent_message_id(doc_id, seq, ctx);
    validate_intent(&message_id, batch_id, seq, ctx.key_epoch)?;
    Ok(SyncIntent {
        batch_id: batch_id.to_owned(),
        object_id: intent_object_id(&message_id, ctx),
        message_id,
        key_epoch: ctx.key_epoch,
    })
}

/// 同事务写入「一 update 一**可选**意图」：`None` = 该条不产生意图（remote 导入）。
///
/// 一次 flush 的连续前缀可能**混**本地编辑与远端导入（`JournalOrigin`），而
/// remote/import/adopt **绝不能**产生出站意图（否则会把刚收到的数据再推回 relay）。
/// 这里仍是一条 A 事务、按位置一一对应，只是允许某些位置为空。
///
/// `slots` 空 = 本批没有**任何**出站意图；来源标记用 [`store::OriginMarks`]
/// （`AllLocal` / 与 records 等长的 `PerRecord`），**不再用空切片**表示「全部本地」。
pub(crate) fn append_updates_with_intent_slots(
    vault: &Vault,
    doc_id: &str,
    expected_head_seq: u64,
    records: &[store::UpdateRecord],
    slots: &[Option<SyncIntent>],
    origin: store::OriginMarks<'_>,
) -> Result<store::HeadState, StorageError> {
    if !slots.is_empty() && slots.len() != records.len() {
        return Err(StorageError::Corrupt {
            context: format!(
                "append_updates_with_intent_slots: records={} slots={} 必须一对一",
                records.len(),
                slots.len()
            ),
        });
    }
    store::append_updates_with_intent_slots(
        vault,
        doc_id,
        expected_head_seq,
        records,
        slots,
        origin,
    )
}

fn intent_digest(tag: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    use sha2::{Digest as _, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(SYNC_INTENT_ID_DOMAIN);
    hasher.update(tag);
    for part in parts {
        hasher.update(part);
    }
    hasher.finalize().into()
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(HEX[usize::from(byte >> 4)]));
        out.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    out
}

/// outbox 状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SyncOutboxState {
    Pending,
    Encrypted,
    Sent,
    Acked,
}

impl SyncOutboxState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Encrypted => "encrypted",
            Self::Sent => "sent",
            Self::Acked => "acked",
        }
    }

    fn parse(value: &str) -> Result<Self, StorageError> {
        match value {
            "pending" => Ok(Self::Pending),
            "encrypted" => Ok(Self::Encrypted),
            "sent" => Ok(Self::Sent),
            "acked" => Ok(Self::Acked),
            other => Err(StorageError::Corrupt {
                context: format!("未知 sync_outbox.state {other:?}"),
            }),
        }
    }
}

/// 一条待处理出站消息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncOutboxRow {
    pub message_id: String,
    pub batch_id: String,
    pub doc_id: String,
    pub seq: u64,
    pub key_epoch: u64,
    pub object_id: Option<[u8; 32]>,
    pub state: SyncOutboxState,
    pub cipher_digest: Option<[u8; 32]>,
}

/// 一个已加密的 MNE1 part。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncEncryptedPart {
    /// 0-based 连续下标（0..part_count-1）。
    pub part_no: u32,
    /// exact MNE1 envelope bytes（header+ciphertext）。
    pub envelope: Vec<u8>,
    /// 该 part envelope 的 SHA256（sync 计算，storage 不验哈希）。
    pub digest: [u8; 32],
}

/// 从数据库读出的 part。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncOutboxPart {
    pub message_id: String,
    pub epoch: u64,
    pub part_no: u32,
    pub envelope: Vec<u8>,
    pub digest: [u8; 32],
}

/// inbox 状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SyncInboxState {
    Received,
    Verified,
    Integrated,
    Quarantine,
}

impl SyncInboxState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Received => "received",
            Self::Verified => "verified",
            Self::Integrated => "integrated",
            Self::Quarantine => "quarantine",
        }
    }

    fn parse(value: &str) -> Result<Self, StorageError> {
        match value {
            "received" => Ok(Self::Received),
            "verified" => Ok(Self::Verified),
            "integrated" => Ok(Self::Integrated),
            "quarantine" => Ok(Self::Quarantine),
            other => Err(StorageError::Corrupt {
                context: format!("未知 sync_inbox.state {other:?}"),
            }),
        }
    }
}

/// 一条入站对象。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncInboxRow {
    pub log_id: [u8; 16],
    pub relay_seq: u64,
    pub object_id: [u8; 32],
    pub epoch: u64,
    pub state: SyncInboxState,
    pub verified_payload: Option<Vec<u8>>,
}

/// 批量写入的一行**已验签 / 已解密**入站事实（[`store_inbox_verified_batch`] 的输入）。
///
/// 字段与 [`store_inbox_verified`] 的参数一一对应；依赖是否齐仍由 sync 层判断，
/// storage 只负责 durable 落行与推进 `received` 连续前缀。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedInboxRow {
    /// relay 日志 incarnation（同一批必须一致）。
    pub log_id: [u8; 16],
    /// relay 日志序号（必须 > 0；同一批内不得重复）。
    pub relay_seq: u64,
    /// 密文对象名（32B）。
    pub object_id: [u8; 32],
    /// 加密 epoch（必须 > 0）。
    pub epoch: u64,
    /// 已解密并完成载荷校验的 `UpdateV1` 明文（写入 `sync_inbox.verified_payload`）。
    pub verified_payload: Vec<u8>,
}

/// 本地同步水位（`meta` 约定）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SyncCursorState {
    pub received_seq: u64,
    pub integrated_seq: u64,
    pub log_id: Option<[u8; 16]>,
    pub confirmed_highwater: Option<u64>,
}

/// 在 A 事务内追加 update 的同时写出站意图。
///
/// `intents.len()` 必须与 `records.len()` 相同（一 update 一意图）。
pub fn append_updates_with_sync_intents(
    vault: &Vault,
    doc_id: &str,
    expected_head_seq: u64,
    records: &[store::UpdateRecord],
    intents: &[SyncIntent],
) -> Result<store::HeadState, StorageError> {
    if intents.len() != records.len() {
        return Err(StorageError::Corrupt {
            context: format!(
                "append_updates_with_sync_intents: records={} intents={} 必须一对一",
                records.len(),
                intents.len()
            ),
        });
    }
    let slots: Vec<Option<SyncIntent>> = intents.iter().cloned().map(Some).collect();
    store::append_updates_with_intent_slots(
        vault,
        doc_id,
        expected_head_seq,
        records,
        &slots,
        store::OriginMarks::AllLocal,
    )
}

/// 单独记录一条出站意图（调用方已确认 update 落库）。
///
/// 若希望意图与 update 同事务，请用 [`append_updates_with_sync_intents`]。
pub fn enqueue_sync_intent(
    vault: &Vault,
    message_id: &str,
    batch_id: &str,
    doc_id: &str,
    seq: u64,
    key_epoch: u64,
    object_id: &[u8; 32],
) -> Result<(), StorageError> {
    validate_intent(message_id, batch_id, seq, key_epoch)?;
    let affected = vault.exec(
        "INSERT INTO sync_outbox(message_id,batch_id,doc_id,seq,key_epoch,object_id,state,ciphertext) \
         VALUES(?1,?2,?3,?4,?5,?6,'pending',NULL)",
        &[
            Value::Text(message_id.to_owned()),
            Value::Text(batch_id.to_owned()),
            Value::Text(doc_id.to_owned()),
            Value::Integer(i64_of(seq)?),
            Value::Integer(i64_of(key_epoch)?),
            Value::Blob(object_id.to_vec()),
        ],
    )?;
    if affected != 1 {
        return Err(StorageError::SyncConflict {
            context: format!("message {message_id} 已存在"),
        });
    }
    Ok(())
}

/// 读指定状态的 outbox 行。
pub fn outbox_rows(
    vault: &Vault,
    state: SyncOutboxState,
    limit: usize,
) -> Result<Vec<SyncOutboxRow>, StorageError> {
    let rows = vault.query(
        "SELECT message_id,batch_id,doc_id,seq,key_epoch,object_id,state,cipher_digest \
         FROM sync_outbox WHERE state=?1 ORDER BY doc_id,seq,message_id LIMIT ?2",
        &[
            Value::Text(state.as_str().to_owned()),
            Value::Integer(i64::try_from(limit).unwrap_or(i64::MAX)),
        ],
    )?;
    rows.iter().map(row_to_outbox).collect()
}

/// 待加密（state=pending）的意图。
pub fn pending_encryption(vault: &Vault, limit: usize) -> Result<Vec<SyncOutboxRow>, StorageError> {
    outbox_rows(vault, SyncOutboxState::Pending, limit)
}

/// 待发送（state=encrypted）的批次。
pub fn pending_send(vault: &Vault, limit: usize) -> Result<Vec<SyncOutboxRow>, StorageError> {
    outbox_rows(vault, SyncOutboxState::Encrypted, limit)
}

/// 按 message_id 读一行。
pub fn outbox_row(vault: &Vault, message_id: &str) -> Result<Option<SyncOutboxRow>, StorageError> {
    let rows = vault.query(
        "SELECT message_id,batch_id,doc_id,seq,key_epoch,object_id,state,cipher_digest \
         FROM sync_outbox WHERE message_id=?1",
        &[Value::Text(message_id.to_owned())],
    )?;
    rows.first().map(row_to_outbox).transpose()
}

/// 读某 message/epoch 的全部 parts，按 part_no 升序。
pub fn outbox_parts(
    vault: &Vault,
    message_id: &str,
    epoch: u64,
) -> Result<Vec<SyncOutboxPart>, StorageError> {
    let rows = vault.query(
        "SELECT message_id,epoch,part_no,envelope,digest FROM sync_outbox_parts \
         WHERE message_id=?1 AND epoch=?2 ORDER BY part_no",
        &[
            Value::Blob(message_id.as_bytes().to_vec()),
            Value::Integer(i64_of(epoch)?),
        ],
    )?;
    rows.iter()
        .map(|row| {
            Ok(SyncOutboxPart {
                message_id: text_from_blob(row, 0, "sync_outbox_parts.message_id")?,
                epoch: u64_of(row, 1, "sync_outbox_parts.epoch")?,
                part_no: u32_of(row, 2, "sync_outbox_parts.part_no")?,
                envelope: blob_of(row, 3, "sync_outbox_parts.envelope")?,
                digest: array32(row, 4, "sync_outbox_parts.digest")?,
            })
        })
        .collect()
}

/// CAS 写入一批加密 parts，并把 outbox 置为 encrypted。
///
/// 同一 `(message_id, epoch)` 已有 parts 时，必须与新提交的整批字节完全一致；
/// 否则返回 [`StorageError::SyncConflict`]（不会覆盖旧 parts）。
pub fn store_encrypted_parts(
    vault: &Vault,
    message_id: &str,
    epoch: u64,
    object_digest: &[u8; 32],
    parts: &[SyncEncryptedPart],
) -> Result<(), StorageError> {
    if message_id.is_empty() {
        return Err(StorageError::Corrupt {
            context: "message_id 不能为空".to_owned(),
        });
    }
    if parts.is_empty() {
        return Err(StorageError::Corrupt {
            context: format!("message {message_id} 没有 parts"),
        });
    }
    for (index, part) in parts.iter().enumerate() {
        if part.part_no != index as u32 {
            return Err(StorageError::Corrupt {
                context: format!(
                    "message {message_id} parts 必须从 0 连续：第 {index} 项是 {}",
                    part.part_no
                ),
            });
        }
        if part.envelope.is_empty() {
            return Err(StorageError::Corrupt {
                context: format!("message {message_id} 第 {index} 片信封为空"),
            });
        }
    }

    let outbox = outbox_row(vault, message_id)?.ok_or_else(|| StorageError::Corrupt {
        context: format!("sync_outbox 缺少 message {message_id}"),
    })?;
    if outbox.key_epoch != epoch {
        return Err(StorageError::SyncConflict {
            context: format!(
                "message {message_id} epoch={epoch}，outbox 为 {}",
                outbox.key_epoch
            ),
        });
    }
    if outbox.state == SyncOutboxState::Acked {
        return Err(StorageError::SyncConflict {
            context: format!("message {message_id} 已 ACK，拒绝重加密"),
        });
    }

    let existing = outbox_parts(vault, message_id, epoch)?;
    let mut missing = Vec::new();
    if !existing.is_empty() {
        if let Some(old_digest) = outbox.cipher_digest {
            if old_digest != *object_digest {
                return Err(StorageError::SyncConflict {
                    context: format!("message {message_id}/{epoch} 整体 digest 不一致"),
                });
            }
        }
        if existing.len() != parts.len() {
            return Err(StorageError::SyncConflict {
                context: format!(
                    "message {message_id}/{epoch} 已有 {} parts，本次 {} parts",
                    existing.len(),
                    parts.len()
                ),
            });
        }
        for (old, new) in existing.iter().zip(parts) {
            if old.part_no != new.part_no
                || old.envelope != new.envelope
                || old.digest != new.digest
            {
                return Err(StorageError::SyncConflict {
                    context: format!(
                        "message {message_id}/{epoch} 第 {} 片字节不一致",
                        new.part_no
                    ),
                });
            }
        }
    } else {
        missing.extend(parts.iter());
    }

    let mut statements = Vec::with_capacity(1 + missing.len());
    statements.push(Statement::new(
        "UPDATE sync_outbox SET state='encrypted', cipher_digest=?2 \
         WHERE message_id=?1 AND key_epoch=?3 AND state IN('pending','encrypted','sent')",
        vec![
            Value::Text(message_id.to_owned()),
            Value::Blob(object_digest.to_vec()),
            Value::Integer(i64_of(epoch)?),
        ],
    ));
    for part in missing {
        statements.push(Statement::new(
            "INSERT INTO sync_outbox_parts(message_id,epoch,part_no,envelope,digest) \
             VALUES(?1,?2,?3,?4,?5)",
            vec![
                Value::Blob(message_id.as_bytes().to_vec()),
                Value::Integer(i64_of(epoch)?),
                Value::Integer(i64::from(part.part_no)),
                Value::Blob(part.envelope.clone()),
                Value::Blob(part.digest.to_vec()),
            ],
        ));
    }

    match vault.exec_transaction(&statements, &[AffectedGuard::exactly(0, 1)]) {
        Ok(_) => Ok(()),
        Err(StorageError::GuardFailed { .. }) => Err(StorageError::SyncConflict {
            context: format!("message {message_id}/{epoch} 状态已变化，未写入 parts"),
        }),
        Err(error) => Err(error),
    }
}

/// 把已加密批次标记为已发送（CAS digest）。
pub fn mark_sent(
    vault: &Vault,
    message_id: &str,
    epoch: u64,
    object_digest: &[u8; 32],
) -> Result<bool, StorageError> {
    let affected = vault.exec(
        "UPDATE sync_outbox SET state='sent' \
         WHERE message_id=?1 AND key_epoch=?2 AND cipher_digest=?3 \
           AND state IN('encrypted','sent')",
        &[
            Value::Text(message_id.to_owned()),
            Value::Integer(i64_of(epoch)?),
            Value::Blob(object_digest.to_vec()),
        ],
    )?;
    Ok(affected == 1)
}

/// durable ACK：CAS 成功后删除该 outbox 行及其 parts。
///
/// 旧 ACK 的 digest 对不上时只返回 `false`，**不会**删除同 doc/seq 的新 message。
pub fn mark_acked(
    vault: &Vault,
    message_id: &str,
    epoch: u64,
    object_digest: &[u8; 32],
) -> Result<bool, StorageError> {
    let statements = [
        Statement::new(
            "DELETE FROM sync_outbox_parts WHERE message_id=?1 AND epoch=?2",
            vec![
                Value::Blob(message_id.as_bytes().to_vec()),
                Value::Integer(i64_of(epoch)?),
            ],
        ),
        Statement::new(
            "DELETE FROM sync_outbox \
             WHERE message_id=?1 AND key_epoch=?2 AND cipher_digest=?3 \
               AND state IN('encrypted','sent')",
            vec![
                Value::Text(message_id.to_owned()),
                Value::Integer(i64_of(epoch)?),
                Value::Blob(object_digest.to_vec()),
            ],
        ),
    ];
    match vault.exec_transaction(&statements, &[AffectedGuard::exactly(1, 1)]) {
        Ok(_) => Ok(true),
        Err(StorageError::GuardFailed { .. }) => Ok(false),
        Err(error) => Err(error),
    }
}

/// `doc_id/seq` 是否仍被未 ACK 的 outbox 引用。
///
/// 只有返回 `false` 时才允许回收 `loro_updates`（经验证 snapshot VV 覆盖
/// 属于 sync 层的独立条件，本函数不解析 Loro）。
pub fn update_referenced_by_outbox(
    vault: &Vault,
    doc_id: &str,
    seq: u64,
) -> Result<bool, StorageError> {
    let rows = vault.query(
        "SELECT count(*) FROM sync_outbox WHERE doc_id=?1 AND seq=?2",
        &[Value::Text(doc_id.to_owned()), Value::Integer(i64_of(seq)?)],
    )?;
    Ok(rows
        .first()
        .and_then(|row| row.first())
        .and_then(|value| match value {
            Value::Integer(value) => Some(*value > 0),
            _ => None,
        })
        .unwrap_or(false))
}

/// `doc_id/seq` 的 update 是否已无未 ACK outbox 引用、可回收。
///
/// 这只是**必要**条件；调用方还必须确认“经验证 snapshot VV 已覆盖该 seq”。
pub fn can_recycle_update(vault: &Vault, doc_id: &str, seq: u64) -> Result<bool, StorageError> {
    Ok(!update_referenced_by_outbox(vault, doc_id, seq)?)
}

/// `meta` 键：轮换后必须 repack outbox 的持久屏障（sync 阶段 H 写入 `b"1"`）。
pub const META_SYNC_REPACK_REQUIRED: &str = "sync.repack_required";

/// 旧 outbox 行的 CAS 键：message/epoch/整体 digest 必须同时匹配。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxKey {
    pub message_id: String,
    pub epoch: u64,
    pub cipher_digest: [u8; 32],
}

/// repack 后要写入的一个 MNE1 part。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepackPart {
    pub part_no: u32,
    pub envelope: Vec<u8>,
    pub digest: [u8; 32],
}

/// 一条 outbox 重打包目标（保留旧行的 doc_id/seq/batch_id，只换消息元数据）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepackSpec {
    pub message_id: String,
    pub object_id: [u8; 32],
    pub epoch: u64,
    pub parts: Vec<RepackPart>,
    pub cipher_digest: [u8; 32],
}

/// repack 结果；`NotFound`/`Conflict` 都保证**没有改动任何行**。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepackOutcome {
    Repacked,
    NotFound,
    Conflict,
}

/// candidate 里的 parts 摘要（不把 envelope 全量搬进列表）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepackPartSummary {
    pub part_no: u32,
    pub envelope_len: usize,
    pub digest: [u8; 32],
}

/// 一条待 repack 的旧 outbox 及其 parts 摘要。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxRepackCandidate {
    pub message_id: String,
    pub batch_id: String,
    pub doc_id: String,
    pub seq: u64,
    pub key_epoch: u64,
    pub object_id: Option<[u8; 32]>,
    pub state: SyncOutboxState,
    pub cipher_digest: Option<[u8; 32]>,
    pub parts: Vec<RepackPartSummary>,
}

/// `sync.repack_required` 屏障读取；未知值按损坏处理，避免静默漏 repack。
pub fn repack_required(vault: &Vault) -> Result<bool, StorageError> {
    match vault.get_meta(META_SYNC_REPACK_REQUIRED)? {
        None => Ok(false),
        Some(value) if value == b"0" || value.is_empty() => Ok(false),
        Some(value) if value == b"1" => Ok(true),
        Some(value) => Err(StorageError::Corrupt {
            context: format!(
                "meta {META_SYNC_REPACK_REQUIRED} 非法值：{}",
                String::from_utf8_lossy(&value)
            ),
        }),
    }
}

/// 写 `sync.repack_required` 屏障；与 sync 现有 `vault.set_meta` 使用同一 meta 键。
pub fn set_repack_required(vault: &Vault, required: bool) -> Result<(), StorageError> {
    vault.set_meta(
        META_SYNC_REPACK_REQUIRED,
        if required { b"1" } else { b"0" },
    )
}

/// 列出仍未 ACK（pending/encrypted/sent）、且 key_epoch 不是给定当前 epoch 的 outbox 行。
///
/// `encrypted` 表示 parts 已 durable 但尚未发送；换 epoch 后同样必须重包，不能漏掉。
pub fn outbox_repack_candidates(
    vault: &Vault,
    current_epoch: u64,
) -> Result<Vec<OutboxRepackCandidate>, StorageError> {
    let rows = vault.query(
        "SELECT message_id,batch_id,doc_id,seq,key_epoch,object_id,state,cipher_digest \
         FROM sync_outbox WHERE state IN('pending','encrypted','sent') AND key_epoch<>?1 \
         ORDER BY doc_id,seq,message_id",
        &[Value::Integer(i64_of(current_epoch)?)],
    )?;
    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        let row = row_to_outbox(row)?;
        let parts = outbox_parts(vault, &row.message_id, row.key_epoch)?
            .into_iter()
            .map(|part| RepackPartSummary {
                part_no: part.part_no,
                envelope_len: part.envelope.len(),
                digest: part.digest,
            })
            .collect();
        out.push(OutboxRepackCandidate {
            message_id: row.message_id,
            batch_id: row.batch_id,
            doc_id: row.doc_id,
            seq: row.seq,
            key_epoch: row.key_epoch,
            object_id: row.object_id,
            state: row.state,
            cipher_digest: row.cipher_digest,
            parts,
        });
    }
    Ok(out)
}

/// 原子地把一条旧 outbox 重打包为新的 message/object/epoch/parts。
///
/// 先做无写入的旧行 CAS 校验；校验通过后在**同一个事务**里：
/// 删除旧 parts → 删除旧 outbox 行 → 插入新 outbox 行 → 插入新 parts。
/// 旧行已被 ACK、epoch/digest 不匹配或事务中途失败时，旧行与旧 parts 都保持不变。
pub fn repack_outbox(
    vault: &Vault,
    old: &OutboxKey,
    new: &RepackSpec,
) -> Result<RepackOutcome, StorageError> {
    validate_repack_spec(new)?;
    if old.message_id.is_empty() {
        return Err(StorageError::Corrupt {
            context: "repack old.message_id 不能为空".to_owned(),
        });
    }

    let old_row = outbox_row(vault, &old.message_id)?;
    let Some(old_row) = old_row else {
        return Ok(RepackOutcome::NotFound);
    };
    if old_row.state == SyncOutboxState::Acked
        || old_row.key_epoch != old.epoch
        || old_row.cipher_digest != Some(old.cipher_digest)
    {
        return Ok(RepackOutcome::Conflict);
    }
    // 若新旧 CAS 键完全相同，迟到的旧 ACK 仍可能命中新行；这不是一次有效的重包。
    if new.message_id == old.message_id
        && new.epoch == old.epoch
        && new.cipher_digest == old.cipher_digest
    {
        return Ok(RepackOutcome::Conflict);
    }

    // 旧 parts 必须是连续 0..n-1；数据库损坏时不能静默搬走。
    let old_parts = outbox_parts(vault, &old.message_id, old.epoch)?;
    for (index, part) in old_parts.iter().enumerate() {
        if part.part_no != index as u32 {
            return Err(StorageError::Corrupt {
                context: format!(
                    "旧 outbox {}/{} parts 不连续：第 {index} 项是 {}",
                    old.message_id, old.epoch, part.part_no
                ),
            });
        }
    }

    // 同一 message_id 只能有一行；复用旧 message 时由事务内“先删后插”保证。
    if new.message_id != old.message_id && outbox_row(vault, &new.message_id)?.is_some() {
        return Ok(RepackOutcome::Conflict);
    }

    let mut statements = Vec::with_capacity(2 + 1 + new.parts.len());
    statements.push(Statement::new(
        "DELETE FROM sync_outbox_parts WHERE message_id=?1 AND epoch=?2",
        vec![
            Value::Blob(old.message_id.as_bytes().to_vec()),
            Value::Integer(i64_of(old.epoch)?),
        ],
    ));
    statements.push(Statement::new(
        "DELETE FROM sync_outbox \
         WHERE message_id=?1 AND key_epoch=?2 AND cipher_digest=?3 \
           AND state IN('pending','encrypted','sent')",
        vec![
            Value::Text(old.message_id.clone()),
            Value::Integer(i64_of(old.epoch)?),
            Value::Blob(old.cipher_digest.to_vec()),
        ],
    ));
    statements.push(Statement::new(
        "INSERT INTO sync_outbox(message_id,batch_id,doc_id,seq,key_epoch,ciphertext,object_id,state,cipher_digest) \
         VALUES(?1,?2,?3,?4,?5,NULL,?6,'encrypted',?7)",
        vec![
            Value::Text(new.message_id.clone()),
            Value::Text(old_row.batch_id.clone()),
            Value::Text(old_row.doc_id.clone()),
            Value::Integer(i64_of(old_row.seq)?),
            Value::Integer(i64_of(new.epoch)?),
            Value::Blob(new.object_id.to_vec()),
            Value::Blob(new.cipher_digest.to_vec()),
        ],
    ));
    for part in &new.parts {
        statements.push(Statement::new(
            "INSERT INTO sync_outbox_parts(message_id,epoch,part_no,envelope,digest) \
             VALUES(?1,?2,?3,?4,?5)",
            vec![
                Value::Blob(new.message_id.as_bytes().to_vec()),
                Value::Integer(i64_of(new.epoch)?),
                Value::Integer(i64::from(part.part_no)),
                Value::Blob(part.envelope.clone()),
                Value::Blob(part.digest.to_vec()),
            ],
        ));
    }

    let mut guards = vec![
        AffectedGuard::exactly(0, old_parts.len()),
        AffectedGuard::exactly(1, 1),
        AffectedGuard::exactly(2, 1),
    ];
    guards.extend((0..new.parts.len()).map(|index| AffectedGuard::exactly(3 + index, 1)));

    match vault.exec_transaction(&statements, &guards) {
        Ok(_) => Ok(RepackOutcome::Repacked),
        Err(StorageError::GuardFailed { .. }) => Ok(RepackOutcome::Conflict),
        Err(error) => Err(error),
    }
}

fn validate_repack_spec(spec: &RepackSpec) -> Result<(), StorageError> {
    if spec.message_id.is_empty() {
        return Err(StorageError::Corrupt {
            context: "repack new.message_id 不能为空".to_owned(),
        });
    }
    if spec.epoch == 0 {
        return Err(StorageError::Corrupt {
            context: "repack new.epoch 必须 >0".to_owned(),
        });
    }
    if spec.parts.is_empty() {
        return Err(StorageError::Corrupt {
            context: "repack new.parts 不能为空".to_owned(),
        });
    }
    for (index, part) in spec.parts.iter().enumerate() {
        if part.part_no != index as u32 {
            return Err(StorageError::Corrupt {
                context: format!(
                    "repack new.parts 必须从 0 连续：第 {index} 项是 {}",
                    part.part_no
                ),
            });
        }
        if part.envelope.is_empty() {
            return Err(StorageError::Corrupt {
                context: format!("repack new.parts 第 {index} 片信封为空"),
            });
        }
        if part.digest.len() != 32 {
            return Err(StorageError::Corrupt {
                context: format!("repack new.parts 第 {index} 片 digest 不是 32B"),
            });
        }
    }
    Ok(())
}

fn validate_intent(
    message_id: &str,
    batch_id: &str,
    seq: u64,
    key_epoch: u64,
) -> Result<(), StorageError> {
    if message_id.is_empty() || batch_id.is_empty() {
        return Err(StorageError::Corrupt {
            context: "sync intent message_id/batch_id 不能为空".to_owned(),
        });
    }
    if seq == 0 || key_epoch == 0 {
        return Err(StorageError::Corrupt {
            context: "sync intent seq/key_epoch 必须 >0".to_owned(),
        });
    }
    Ok(())
}

fn row_to_outbox(row: &crate::Row) -> Result<SyncOutboxRow, StorageError> {
    let object_id = row
        .get(5)
        .and_then(blob_value)
        .map(|bytes| array32_bytes(bytes, "sync_outbox.object_id"))
        .transpose()?;
    let cipher_digest = row
        .get(7)
        .and_then(blob_value)
        .map(|bytes| array32_bytes(bytes, "sync_outbox.cipher_digest"))
        .transpose()?;
    Ok(SyncOutboxRow {
        message_id: text_of(row, 0, "sync_outbox.message_id")?,
        batch_id: text_of(row, 1, "sync_outbox.batch_id")?,
        doc_id: text_of(row, 2, "sync_outbox.doc_id")?,
        seq: u64_of(row, 3, "sync_outbox.seq")?,
        key_epoch: u64_of(row, 4, "sync_outbox.key_epoch")?,
        object_id,
        state: SyncOutboxState::parse(&text_of(row, 6, "sync_outbox.state")?)?,
        cipher_digest,
    })
}

/// 入站：durable 保存已验签/已解密对象（依赖是否齐由 sync 层决定）。
pub fn store_inbox_verified(
    vault: &Vault,
    log_id: &[u8; 16],
    relay_seq: u64,
    object_id: &[u8; 32],
    epoch: u64,
    verified_payload: &[u8],
) -> Result<(), StorageError> {
    if relay_seq == 0 || epoch == 0 {
        return Err(StorageError::Corrupt {
            context: "inbox relay_seq/epoch 必须 >0".to_owned(),
        });
    }
    ensure_log_id(vault, log_id)?;

    let rows = vault.query(
        "SELECT object_id,epoch,state,verified_payload FROM sync_inbox \
         WHERE log_id=?1 AND relay_seq=?2",
        &[
            Value::Blob(log_id.to_vec()),
            Value::Integer(i64_of(relay_seq)?),
        ],
    )?;
    if let Some(row) = rows.first() {
        let old_object = array32(row, 0, "sync_inbox.object_id")?;
        let old_epoch = u64_of(row, 1, "sync_inbox.epoch")?;
        let old_payload = row.get(3).and_then(blob_value).unwrap_or_default();
        if old_object != *object_id || old_epoch != epoch || old_payload != verified_payload {
            return Err(StorageError::SyncConflict {
                context: format!("inbox {log_id:?}/{relay_seq} 字节不一致"),
            });
        }
        advance_received_prefix(vault)?;
        return Ok(());
    }

    let changed = vault.exec(
        "INSERT INTO sync_inbox(log_id,relay_seq,object_id,epoch,state,verified_payload) \
         VALUES(?1,?2,?3,?4,'verified',?5)",
        &[
            Value::Blob(log_id.to_vec()),
            Value::Integer(i64_of(relay_seq)?),
            Value::Blob(object_id.to_vec()),
            Value::Integer(i64_of(epoch)?),
            Value::Blob(verified_payload.to_vec()),
        ],
    )?;
    if changed != 1 {
        return Err(StorageError::SyncConflict {
            context: format!("inbox {log_id:?}/{relay_seq} 写入冲突"),
        });
    }
    advance_received_prefix(vault)?;
    Ok(())
}

/// 入站批量入口：**一个事务**完成「既有行比对 + 新行插入 + `received` 前缀推进」。
///
/// 语义与逐条入口**完全一致**：
///
/// - 已存在同 `(log_id, relay_seq)` 行、且 `object_id/epoch/verified_payload` 完全相同 →
///   幂等跳过（不重写、不报错，也不触发任何写入守卫）；
/// - 已存在同行但字节不同 → [`StorageError::SyncConflict`]，**整批一行都不写**；
/// - 新行插入与 `received` 连续前缀推进在**同一个** SQLite 事务里（Phase 1A-1），
///   一次提交只产生一次 fsync；冲突/崩溃只会整批回滚，不存在"行写了、前缀没推进"的
///   半提交状态；前缀只覆盖已消费的连续序号（含 `sync.skipped_received` 的 ASSET 标记）。
///
/// 输入先按 `relay_seq` 升序**稳定排序**（不要求调用方预排序；允许序号不连续，例如一页里
/// 混有已消费的 ASSET 序号）；同一批里出现重复 `relay_seq` 直接拒绝——一条日志不可能有
/// 两个同序号事件，静默去重会掩盖上游错误。全部校验/排序在 worker 的事务入口
/// （`actor` 模块的 `ExecInboxBatch` 命令）再做一遍，这里只负责搬运数据。
///
/// 返回**本次新插入**的行数（幂等跳过的行不计）。
pub fn store_inbox_verified_batch(
    vault: &Vault,
    rows: &[VerifiedInboxRow],
) -> Result<usize, StorageError> {
    if rows.is_empty() {
        return Ok(0);
    }
    let batch = rows
        .iter()
        .map(|row| crate::actor::InboxBatchRow {
            log_id: row.log_id,
            relay_seq: row.relay_seq,
            object_id: row.object_id,
            epoch: row.epoch,
            verified_payload: row.verified_payload.clone(),
        })
        .collect();
    vault.exec_inbox_batch(batch)
}

/// 读当前已验证、且落在 durable 连续接收前缀内的 inbox 行。
///
/// 依赖完整性由 sync 层判断（本 crate 不解析 Loro VV）；这里的 "ready"
/// 指 cryptographically verified + durable + **属于当前 `sync.log_id`** +
/// `relay_seq <= received_seq`，不在连续前缀内的行保持 durable，等前面的缺口补齐。
/// 排序为 `(log_id, relay_seq)`。
///
/// **incarnation 过滤**（ADR 0006 §5）：`sync_inbox` 的 `log_id` 是写入时那代 relay
/// 日志的 incarnation。restore 迁移 `sync.log_id` 后，旧 incarnation 的行仍然 durable
/// （不能在迁移时静默删数据），但绝不能进入 ready——`relay_seq` 只在同一 incarnation
/// 内有序，跨代比较没有意义。过滤条件里的 `log_id` 由 [`read_current_log_id`] 读出后
/// 作为 SQL 参数绑定，不做子查询以便调用方/测试都看到同一个显式值。
///
/// `sync.log_id` 不存在（新库尚未绑定 relay 日志代）时返回空：没有当前 incarnation
/// 就没有可匹配的行；新库此时 `received` 也是 0，所以与旧实现（不按 log 过滤）的结果
/// 一致，不会把其它 incarnation 的遗留行漏出来。`sync.log_id` 存在但不是 16B →
/// [`StorageError::Corrupt`]（与 [`sync_cursor_state`] 同口径），不静默当作"没有 log"。
pub fn take_inbox_ready(vault: &Vault, limit: usize) -> Result<Vec<SyncInboxRow>, StorageError> {
    let Some(log_id) = read_current_log_id(vault)? else {
        return Ok(Vec::new());
    };
    let received = read_meta_u64(vault, META_SYNC_RECEIVED_SEQ)?.unwrap_or(0);
    let rows = vault.query(
        "SELECT log_id,relay_seq,object_id,epoch,state,verified_payload \
         FROM sync_inbox WHERE log_id=?1 AND state='verified' AND relay_seq<=?2 \
         ORDER BY log_id,relay_seq LIMIT ?3",
        &[
            Value::Blob(log_id.to_vec()),
            Value::Integer(i64_of(received)?),
            Value::Integer(i64::try_from(limit).unwrap_or(i64::MAX)),
        ],
    )?;
    rows.iter().map(row_to_inbox).collect()
}

/// 统计「已验证、属于当前 incarnation、落在 durable 连续接收前缀内、尚未集成」的行数。
///
/// 语义（ADR 0012 S2b）：这些行不是"丢失"——它们要么依赖未就绪（deferred），要么被
/// 批次预算留在本轮之后。调用方用它上报"已接收·待应用 N"，并据此判断"还有没有
/// 可继续的工作"，而不是循环刷最早的一批。
///
/// 与 [`take_inbox_ready`] 同口径：只计当前 `sync.log_id` 的行，旧 incarnation 的
/// verified 行即使 `relay_seq <= received` 也不计。`sync.log_id` 不存在返回 0
/// （没有当前 incarnation 就没有 ready 行）；存在但长度非法 → [`StorageError::Corrupt`]。
pub fn count_inbox_ready(vault: &Vault) -> Result<u64, StorageError> {
    let Some(log_id) = read_current_log_id(vault)? else {
        return Ok(0);
    };
    let received = read_meta_u64(vault, META_SYNC_RECEIVED_SEQ)?.unwrap_or(0);
    let rows = vault.query(
        "SELECT COUNT(*) FROM sync_inbox \
         WHERE log_id=?1 AND state='verified' AND relay_seq<=?2",
        &[
            Value::Blob(log_id.to_vec()),
            Value::Integer(i64_of(received)?),
        ],
    )?;
    rows.first()
        .map(|row| u64_of(row, 0, "sync_inbox.count"))
        .transpose()?
        .ok_or_else(|| StorageError::Corrupt {
            context: "sync_inbox 计数缺失".to_owned(),
        })
}

/// 带调用方依赖判断的 ready 选择器；不满足的项保持 durable verified。
///
/// 底层走 [`take_inbox_ready`]，因此同样只选择当前 `sync.log_id` 的行。
pub fn take_inbox_ready_where(
    vault: &Vault,
    limit: usize,
    mut is_ready: impl FnMut(&SyncInboxRow) -> bool,
) -> Result<Vec<SyncInboxRow>, StorageError> {
    let rows = take_inbox_ready(vault, limit.saturating_mul(16).max(limit))?;
    Ok(rows
        .into_iter()
        .filter(|row| is_ready(row))
        .take(limit)
        .collect())
}

/// 为 remote import 分配本地 `loro_updates.seq = head_seq + 1`。
///
/// **只分配，不写库**；调用方必须在同一 A 事务里把该 seq 用在
/// `loro_updates.seq`，禁止直接使用 sender_seq。
pub fn allocate_local_seq_for_import(vault: &Vault, doc_id: &str) -> Result<u64, StorageError> {
    let head = store::load_head(vault, doc_id)?;
    head.head_seq
        .checked_add(1)
        .ok_or_else(|| StorageError::Corrupt {
            context: format!("{doc_id} 本地 seq 溢出"),
        })
}

/// 把一个 verified inbox 行标为 integrated，并推进连续 integrated 前缀。
pub fn mark_integrated(
    vault: &Vault,
    log_id: &[u8; 16],
    relay_seq: u64,
) -> Result<(), StorageError> {
    let changed = vault.exec(
        "UPDATE sync_inbox SET state='integrated' \
         WHERE log_id=?1 AND relay_seq=?2 AND state='verified'",
        &[
            Value::Blob(log_id.to_vec()),
            Value::Integer(i64_of(relay_seq)?),
        ],
    )?;
    if changed == 0 {
        let rows = vault.query(
            "SELECT state FROM sync_inbox WHERE log_id=?1 AND relay_seq=?2",
            &[
                Value::Blob(log_id.to_vec()),
                Value::Integer(i64_of(relay_seq)?),
            ],
        )?;
        let Some(row) = rows.first() else {
            return Err(StorageError::Corrupt {
                context: format!("inbox {log_id:?}/{relay_seq} 不存在"),
            });
        };
        let state = text_of(row, 0, "sync_inbox.state")?;
        if state != SyncInboxState::Integrated.as_str() {
            return Err(StorageError::SyncConflict {
                context: format!("inbox {log_id:?}/{relay_seq} 当前 state={state}，不能 integrate"),
            });
        }
    }
    advance_integrated_prefix(vault)?;
    Ok(())
}

/// 批量把同一 log 的多条 verified inbox 行标为 integrated（**一个事务**）。
///
/// Phase 1A-1 为聚合 integrate 准备的 API：调用方在整批正文/依赖都落库后一次性标记，
/// 再统一调用 [`advance_integrated_prefix`]（本函数**不推进**前缀）。
///
/// 严格语义：`relay_seqs` 中每一行都必须存在且当前 `state='verified'`；任何一行
/// 不存在 / 已 integrated / 其它 state → 整批回滚并返回 [`StorageError::SyncConflict`]，
/// 不会留下半批已 integrated 的行。重复序号同理（第二次更新影响 0 行）整批回滚。
///
/// 返回标记的行数（= 输入行数；空输入返回 0）。
pub fn mark_integrated_batch(
    vault: &Vault,
    log_id: &[u8; 16],
    relay_seqs: &[u64],
) -> Result<usize, StorageError> {
    if relay_seqs.is_empty() {
        return Ok(0);
    }
    let mut statements = Vec::with_capacity(relay_seqs.len());
    let mut guards = Vec::with_capacity(relay_seqs.len());
    for (index, relay_seq) in relay_seqs.iter().enumerate() {
        statements.push(Statement::new(
            "UPDATE sync_inbox SET state='integrated' \
             WHERE log_id=?1 AND relay_seq=?2 AND state='verified'",
            vec![
                Value::Blob(log_id.to_vec()),
                Value::Integer(i64_of(*relay_seq)?),
            ],
        ));
        guards.push(AffectedGuard::exactly(index, 1));
    }
    match vault.exec_transaction(&statements, &guards) {
        Ok(_) => Ok(relay_seqs.len()),
        // 守卫不满足 = 某行不存在或已不是 verified；exec_transaction 已整笔回滚。
        Err(StorageError::GuardFailed { .. }) => Err(StorageError::SyncConflict {
            context: format!(
                "inbox {log_id:?} 批量 integrate 失败：某行不存在或当前不是 verified（整批已回滚）"
            ),
        }),
        Err(error) => Err(error),
    }
}

/// 把 inbox 行标为 quarantine（完整性失败/依赖永久缺失）。
pub fn mark_inbox_quarantine(
    vault: &Vault,
    log_id: &[u8; 16],
    relay_seq: u64,
) -> Result<(), StorageError> {
    let changed = vault.exec(
        "UPDATE sync_inbox SET state='quarantine' \
         WHERE log_id=?1 AND relay_seq=?2 AND state IN('received','verified')",
        &[
            Value::Blob(log_id.to_vec()),
            Value::Integer(i64_of(relay_seq)?),
        ],
    )?;
    if changed == 0 {
        return Err(StorageError::SyncConflict {
            context: format!("inbox {log_id:?}/{relay_seq} 不能进入 quarantine"),
        });
    }
    Ok(())
}

fn row_to_inbox(row: &crate::Row) -> Result<SyncInboxRow, StorageError> {
    Ok(SyncInboxRow {
        log_id: array16(row, 0, "sync_inbox.log_id")?,
        relay_seq: u64_of(row, 1, "sync_inbox.relay_seq")?,
        object_id: array32(row, 2, "sync_inbox.object_id")?,
        epoch: u64_of(row, 3, "sync_inbox.epoch")?,
        state: SyncInboxState::parse(&text_of(row, 4, "sync_inbox.state")?)?,
        verified_payload: row.get(5).and_then(blob_value),
    })
}

/// 读本地同步水位。
pub fn sync_cursor_state(vault: &Vault) -> Result<SyncCursorState, StorageError> {
    Ok(SyncCursorState {
        received_seq: read_meta_u64(vault, META_SYNC_RECEIVED_SEQ)?.unwrap_or(0),
        integrated_seq: read_meta_u64(vault, META_SYNC_INTEGRATED_SEQ)?.unwrap_or(0),
        log_id: read_current_log_id(vault)?,
        confirmed_highwater: read_meta_u64(vault, META_SYNC_CONFIRMED_HIGHWATER)?,
    })
}

/// 读当前 relay 日志 incarnation（`sync.log_id`，BLOB 16B）。
///
/// - 不存在（新库/尚未绑定）：`Ok(None)`，调用方按"没有当前 incarnation"处理
///   （ready 视为空，绝不回退成"不过滤 log"）；
/// - 存在但不是 16B：[`StorageError::Corrupt`]，与 [`sync_cursor_state`] 的既有口径一致。
fn read_current_log_id(vault: &Vault) -> Result<Option<[u8; 16]>, StorageError> {
    read_meta(vault, META_SYNC_LOG_ID)?
        .map(|bytes| array16_bytes(bytes, "sync.log_id"))
        .transpose()
}

/// 写入 `sync.log_id`；已有不同值则拒绝（restore 必须显式迁移）。
pub fn set_sync_log_id(vault: &Vault, log_id: &[u8; 16]) -> Result<(), StorageError> {
    ensure_log_id(vault, log_id)
}

/// 写 confirmed highwater（BE64 BLOB）。
pub fn set_confirmed_highwater(vault: &Vault, value: u64) -> Result<(), StorageError> {
    write_meta(vault, META_SYNC_CONFIRMED_HIGHWATER, &value.to_be_bytes())
}

/// 单条入口的 `sync.log_id` 校验（走 Vault 命令，独立事务）。
///
/// 批量 inbox 事务内的同语义校验在 `sync_meta::ensure_log_id_on`（直接跑在连接/事务上）；
/// 两处必须保持一致：不存在则写，存在不同则 `SyncLogMismatch`。
fn ensure_log_id(vault: &Vault, log_id: &[u8; 16]) -> Result<(), StorageError> {
    match read_meta(vault, META_SYNC_LOG_ID)? {
        Some(existing) => {
            if existing.as_slice() != log_id {
                return Err(StorageError::SyncLogMismatch {
                    expected: existing,
                    found: log_id.to_vec(),
                });
            }
        }
        None => write_meta(vault, META_SYNC_LOG_ID, log_id)?,
    }
    Ok(())
}

fn advance_received_prefix(vault: &Vault) -> Result<(), StorageError> {
    vault.advance_consumed_prefix(ConsumedPrefix::Received)
}

/// 把已集成连续前缀推进到最大已消费序号（**一次**写最终值）。
///
/// 供聚合 integrate 的调用方在整批正文都落库后调用一次（Phase 1A-1）。语义与旧实现
/// 一致：只认 inbox `state='integrated'` 或 `sync.skipped_received` 标记，扫窗口后
/// 一次写最终值；崩溃只会少推进，下次调用重算。
pub fn advance_integrated_prefix(vault: &Vault) -> Result<(), StorageError> {
    vault.advance_consumed_prefix(ConsumedPrefix::Integrated)
}

/// 登记一个**不写 inbox**、但已 durable 消费的事件（ASSET），并推进连续接收前缀。
///
/// 调用方必须先把该事件的事实（对 ASSET 是 skipped-asset HMAC 记录）durable 化，
/// 再调用本函数；顺序保证"received 只覆盖已消费的事件"。
pub fn mark_skipped_event_received(
    vault: &Vault,
    log_id: &[u8; 16],
    relay_seq: u64,
) -> Result<(), StorageError> {
    let key = skipped_received_key(log_id, relay_seq)?;
    write_meta(vault, &key, b"1")?;
    advance_received_prefix(vault)
}

/// 读一条 meta（同步桥经 Vault 命令的读取路径）。
///
/// 类型判定与 [`Vault::get_meta`] / `sync_meta::read_meta_on` 共用同一个 helper
/// [`meta_bytes_of`]：**只认 BLOB**，其它类型是 [`StorageError::Corrupt`]。
/// 这里以前把 TEXT 当"缺失"，会让 `sync.log_id` / `sync.repack_required` /
/// `sync.received_seq` 这些持久屏障与游标被静默绕过。
fn read_meta(vault: &Vault, key: &str) -> Result<Option<Vec<u8>>, StorageError> {
    let rows = vault.query(
        "SELECT value FROM meta WHERE key=?1",
        &[Value::Text(key.to_owned())],
    )?;
    let Some(value) = rows.first().and_then(|row| row.first()) else {
        return Ok(None);
    };
    Ok(Some(meta_bytes_of(key, value.clone())?))
}

fn read_meta_u64(vault: &Vault, key: &str) -> Result<Option<u64>, StorageError> {
    let Some(bytes) = read_meta(vault, key)? else {
        return Ok(None);
    };
    if bytes.len() != 8 {
        return Err(StorageError::Corrupt {
            context: format!("{key} 应为 BE64 8B，实际 {}B", bytes.len()),
        });
    }
    Ok(Some(u64::from_be_bytes(bytes.try_into().expect("长度8"))))
}

fn write_meta(vault: &Vault, key: &str, value: &[u8]) -> Result<(), StorageError> {
    vault.exec(
        "INSERT INTO meta(key,value) VALUES(?1,?2) \
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        &[Value::Text(key.to_owned()), Value::Blob(value.to_vec())],
    )?;
    Ok(())
}

fn text_of(row: &crate::Row, index: usize, field: &str) -> Result<String, StorageError> {
    match row.get(index) {
        Some(Value::Text(value)) => Ok(value.clone()),
        other => Err(StorageError::Corrupt {
            context: format!("{field} 类型异常：{other:?}"),
        }),
    }
}

fn text_from_blob(row: &crate::Row, index: usize, field: &str) -> Result<String, StorageError> {
    let bytes = blob_of(row, index, field)?;
    String::from_utf8(bytes).map_err(|_| StorageError::Corrupt {
        context: format!("{field} 不是 UTF-8 message id"),
    })
}

fn blob_of(row: &crate::Row, index: usize, field: &str) -> Result<Vec<u8>, StorageError> {
    row.get(index)
        .and_then(blob_value)
        .ok_or_else(|| StorageError::Corrupt {
            context: format!("{field} 缺失或类型异常"),
        })
}

fn blob_value(value: &Value) -> Option<Vec<u8>> {
    match value {
        Value::Blob(bytes) => Some(bytes.clone()),
        _ => None,
    }
}

fn array16(row: &crate::Row, index: usize, field: &str) -> Result<[u8; 16], StorageError> {
    array16_bytes(blob_of(row, index, field)?, field)
}

fn array32(row: &crate::Row, index: usize, field: &str) -> Result<[u8; 32], StorageError> {
    array32_bytes(blob_of(row, index, field)?, field)
}

fn array16_bytes(bytes: Vec<u8>, field: &str) -> Result<[u8; 16], StorageError> {
    bytes
        .try_into()
        .map_err(|bytes: Vec<u8>| StorageError::Corrupt {
            context: format!("{field} 应为 16B，实际 {}B", bytes.len()),
        })
}

fn array32_bytes(bytes: Vec<u8>, field: &str) -> Result<[u8; 32], StorageError> {
    bytes
        .try_into()
        .map_err(|bytes: Vec<u8>| StorageError::Corrupt {
            context: format!("{field} 应为 32B，实际 {}B", bytes.len()),
        })
}

fn u64_of(row: &crate::Row, index: usize, field: &str) -> Result<u64, StorageError> {
    match row.get(index) {
        Some(Value::Integer(value)) => u64::try_from(*value).map_err(|_| StorageError::Corrupt {
            context: format!("{field} 为负数"),
        }),
        other => Err(StorageError::Corrupt {
            context: format!("{field} 类型异常：{other:?}"),
        }),
    }
}

fn u32_of(row: &crate::Row, index: usize, field: &str) -> Result<u32, StorageError> {
    let value = u64_of(row, index, field)?;
    u32::try_from(value).map_err(|_| StorageError::Corrupt {
        context: format!("{field} 超出 u32"),
    })
}

fn i64_of(value: u64) -> Result<i64, StorageError> {
    i64::try_from(value).map_err(|_| StorageError::Corrupt {
        context: format!("数值 {value} 超出 SQLite INTEGER"),
    })
}

/// 出站 digest 的 map 形式；用于按 message 聚合时去重（本模块内部）。
#[allow(dead_code)]
fn group_parts_by_message(parts: Vec<SyncOutboxPart>) -> BTreeMap<String, Vec<SyncOutboxPart>> {
    let mut grouped = BTreeMap::new();
    for part in parts {
        grouped
            .entry(part.message_id.clone())
            .or_insert_with(Vec::new)
            .push(part);
    }
    grouped
}

````


## `crates/storage/src/sync_meta.rs`

sync_meta.rs：meta BLOB-only、前缀扫描

````rust
//! 同步元数据（`meta` 键）与「已消费连续前缀」的低层实现。
//!
//! 这里直接跑在 `rusqlite::Connection` 上，而不是走 [`crate::Vault`] 命令：
//! `rusqlite::Transaction` 通过 `Deref` 也是 `&Connection`，因此同一套扫描/写入既能被
//! 普通 Vault 命令复用（worker 线程的连接），也能被
//! `store_inbox_verified_batch` 放进**同一个**事务里执行——这是 Phase 1A-1
//! 「批量 inbox 原子化」的关键：新行插入与 `sync.received_seq` 推进一起提交/回滚。
//!
//! 语义与旧实现逐字等价：前缀只覆盖连续、已 durable 消费的序号；崩溃只会少推进
//! （下次扫描幂等重算），绝不会把未消费的事件算成已消费。

use rusqlite::types::Value;
use rusqlite::Connection;

use crate::error::StorageError;

/// `meta` 键：已 durable 连续接收前缀（BLOB BE64）。
pub const META_SYNC_RECEIVED_SEQ: &str = "sync.received_seq";
/// `meta` 键：已 integrate 连续前缀（BLOB BE64）。
pub const META_SYNC_INTEGRATED_SEQ: &str = "sync.integrated_seq";
/// `meta` 键：当前 relay log incarnation（BLOB 16B）。
pub const META_SYNC_LOG_ID: &str = "sync.log_id";
/// `meta` 键：已确认高水位（BLOB BE64）。
pub const META_SYNC_CONFIRMED_HIGHWATER: &str = "sync.confirmed_highwater";

/// `meta` 键前缀：**非 inbox 事件**（当前只有 ASSET）已 durable 消费的标记。
///
/// 完整键：`sync.skipped_received.<log_id 32 位十六进制>.<relay_seq 20 位十进制>`。
/// ASSET 事件不写 `sync_inbox`（它不是文档水位事件），但 `received_seq` 是"连续前缀"：
/// 若直接把 ASSET 当缺口，`received_seq` 会永久卡在该序号之前，后续 UPDATE 永远无法
/// 集成。pull 在把 ASSET 落成 durable skipped 记录后写本标记，前缀推进同时接受
/// inbox 行与这类标记，保证"已消费"的事件不会把前缀卡住。
pub const META_SYNC_SKIPPED_RECEIVED_PREFIX: &str = "sync.skipped_received.";

/// 一次前缀扫描中**每个来源**（inbox 行 / `skipped_received` 标记）各自的窗口大小。
/// 正常一页 256 条；4096 已远超单页，只作上限保护。
const PREFIX_SCAN_LIMIT: usize = 4096;

/// 要推进的连续前缀及其"已消费"判定。
///
/// 两个变体的条件与旧实现完全一致：
/// - `Received`：inbox `state IN('verified','integrated')`（读者尚未集成也算已接收）；
/// - `Integrated`：inbox `state='integrated'`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConsumedPrefix {
    /// `sync.received_seq`。
    Received,
    /// `sync.integrated_seq`。
    Integrated,
}

impl ConsumedPrefix {
    /// 对应的 `meta` 键。
    pub(crate) fn meta_key(self) -> &'static str {
        match self {
            Self::Received => META_SYNC_RECEIVED_SEQ,
            Self::Integrated => META_SYNC_INTEGRATED_SEQ,
        }
    }

    /// inbox 行算作「已消费」的 SQL 条件。
    pub(crate) fn inbox_condition(self) -> &'static str {
        match self {
            Self::Received => "state IN('verified','integrated')",
            Self::Integrated => "state='integrated'",
        }
    }
}

/// 校验 `sync.log_id`：不存在则写入，已存在不同值则 [`StorageError::SyncLogMismatch`]。
///
/// 与 Vault 版 [`crate::sync_bridge::set_sync_log_id`] 同语义，区别只是直接在给定连接
/// （或事务）上执行，供批量 inbox 在同一事务里完成校验。
///
/// 既有值不是 BLOB 时 [`read_meta_on`] 返回 [`StorageError::Corrupt`]，本函数直接传播：
/// 损坏的 incarnation 记录**不能**当成"尚未绑定"覆盖掉（那会让旧传代的 inbox 行混进
/// 新一代）。
pub(crate) fn ensure_log_id_on(conn: &Connection, log_id: &[u8; 16]) -> Result<(), StorageError> {
    match read_meta_on(conn, META_SYNC_LOG_ID)? {
        Some(existing) => {
            if existing.as_slice() != log_id {
                return Err(StorageError::SyncLogMismatch {
                    expected: existing,
                    found: log_id.to_vec(),
                });
            }
        }
        None => write_meta_on(conn, META_SYNC_LOG_ID, log_id)?,
    }
    Ok(())
}

/// 把连续前缀推进到最大已消费序号（**一次**写最终值）。
///
/// 语义与"从 `current+1` 起逐条检查、遇到第一个缺口停下"逐字等价；差别只在写盘次数：
/// 旧实现对每个前进的序号各写一次 `meta`（每次都是独立事务，WAL + `synchronous=FULL`
/// 下就是一次 fsync）。现在的做法：先用两次读（inbox 升序窗口 + 同 log 的
/// `skipped_received` 键范围扫描，**两个窗口各自**有 [`PREFIX_SCAN_LIMIT`] 条上限）
/// 在内存里走到第一个缺口，再**一次**写入最终值。
///
/// 两个来源的窗口可能分别被截断：只要还有来源没看到"缺口之后"的序号，缺口就不算
/// 证明完毕，必须从新的 `current` 继续下一窗口（见循环内注释）。
///
/// 崩溃语义没有变差：inbox 行与 skipped 标记都先 durable；前缀少推进只会让下一次调用
/// 重算（扫描幂等），绝不会把未消费的事件算成已消费。
///
/// 在 [`crate::Vault`] 命令里传入 worker 独占连接时是自动提交读 + 单条写；
/// 在批量 inbox 事务里传入 `&Transaction` 时，读/写都与新行插入处于同一事务。
pub(crate) fn advance_consumed_prefix_on(
    conn: &Connection,
    prefix: ConsumedPrefix,
) -> Result<(), StorageError> {
    let Some(log_id) = read_meta_on(conn, META_SYNC_LOG_ID)? else {
        return Ok(());
    };
    let log_id = array16_bytes(log_id, "sync.log_id")?;
    let meta_key = prefix.meta_key();
    let mut current = read_meta_u64_on(conn, meta_key)?.unwrap_or(0);
    loop {
        // 与旧实现同口径：前缀已经是 u64::MAX 时无法再前进（保留溢出错误）。
        current
            .checked_add(1)
            .ok_or_else(|| StorageError::Corrupt {
                context: format!("{meta_key} 溢出"),
            })?;
        let window = scan_consumed_after_on(conn, &log_id, current, prefix)?;
        if window.seqs.is_empty() {
            return Ok(());
        }
        // 在内存里从 current+1 走到第一个缺口（seqs 已升序去重）。
        let mut next = current;
        for &seq in &window.seqs {
            if Some(seq) == next.checked_add(1) {
                next = seq;
            } else {
                break;
            }
        }
        if next == current {
            // 首条观测序号就大于 current+1：缺口就在 current+1。
            // 两个窗口的下界都恰好是 current+1（inbox 用 relay_seq>current，skipped 用
            // key(current+1) 起扫），窗口又总是按升序取最小的若干条：若 current+1 确实
            // 已消费，它必然是窗口里最小的那一条，不可能被截断漏掉。缺口已证明，停止。
            return Ok(());
        }
        current = next;
        write_meta_on(conn, meta_key, &current.to_be_bytes())?;
        let gap = current
            .checked_add(1)
            .ok_or_else(|| StorageError::Corrupt {
                context: format!("{meta_key} 溢出"),
            })?;
        // 两个来源都没被截断 → 已看到 current 之后的全部已消费序号，缺口是真的。
        // 被截断但每个被截断来源都已观测到 ≥ 缺口位置的序号 → 缺口同样是真的
        // （未观测序号都大于各自的窗口最大值）。两种情况都不必再扫。
        if !window.truncated() || window.proves_gap_at(gap) {
            return Ok(());
        }
        // 否则继续下一窗口。典型场景：`skipped_received` 窗口被截断在 4096，而 inbox
        // 窗口里还有一条更大的序号（例如 10000）—— 若沿用「合并后的 truncated 且
        // current < last」判断，这里会因为 current(4096) < last(10000) 提前 return，
        // 把下一个窗口里 ≤ 缺口的 skipped 序号永久漏掉、前缀卡死。
        // 下一轮从新的 current+1 起扫；每个窗口只可能看到更大的序号，所以要么推进、
        // 要么在「首条观测序号 > current+1」处判定缺口并退出，不会死循环。
    }
}

/// 一次前缀扫描的窗口观测结果。
///
/// inbox 行与 `skipped_received` 标记是**两个独立的 `LIMIT` 窗口**：某个来源被截断只
/// 说明该来源还有更大的序号没读到（未观测序号一定大于它自己的最大观测序号），不能拿
/// 另一个来源的窗口去推断缺口是否成立。因此截断状态与最大观测序号都按来源记录。
struct ConsumedScanWindow {
    /// 两个来源合并、升序去重后的已消费序号。
    seqs: Vec<u64>,
    /// inbox 窗口是否被 `LIMIT` 截断。
    inbox_truncated: bool,
    /// `skipped_received` 窗口是否被 `LIMIT` 截断。
    skipped_truncated: bool,
    /// inbox 窗口观测到的最大序号（窗口按序号升序，末条即最大）。
    inbox_last: Option<u64>,
    /// `skipped_received` 窗口观测到的最大序号（键定宽，字典序即序号序）。
    skipped_last: Option<u64>,
}

impl ConsumedScanWindow {
    /// 任一来源还有未观测的已消费序号（需要继续下一个窗口）。
    fn truncated(&self) -> bool {
        self.inbox_truncated || self.skipped_truncated
    }

    /// `gap` 处的缺口是否已被本窗口证明。
    ///
    /// 对每个被截断的来源，未观测序号都严格大于该来源窗口的最大观测序号：只要该最大值
    /// ≥ `gap`，未观测序号就不可能落在 `gap` 上。任一被截断来源不满足这条，缺口就没有
    /// 证明完毕，必须继续下一窗口。
    fn proves_gap_at(&self, gap: u64) -> bool {
        (!self.inbox_truncated || self.inbox_last.is_some_and(|last| last >= gap))
            && (!self.skipped_truncated || self.skipped_last.is_some_and(|last| last >= gap))
    }
}

/// 扫出 `current` 之后**已消费**的序号（升序去重），以及两个来源窗口的截断状态。
///
/// "已消费"= inbox 行满足 [`ConsumedPrefix::inbox_condition`] **或**存在
/// `sync.skipped_received` 标记（ASSET 这类不写 inbox 的事件）。`skipped_received`
/// 键是固定宽度的 `<prefix><hex log_id>.<20 位序号>`，字典序等于序号序，因此可以用
/// `meta` 主键做**范围扫描**，不需要 `LIKE` 全表匹配。
///
/// 两个来源各自带 [`PREFIX_SCAN_LIMIT`] 上界并各自报告截断状态：一次把某条日志的全部
/// 标记读进内存会在长日志上无界增长，而只合并成一个 `truncated` 标志又无法区分"哪个
/// 来源还有更大的序号没读到"，缺口证明就会出错（见 [`ConsumedScanWindow::proves_gap_at`]）。
fn scan_consumed_after_on(
    conn: &Connection,
    log_id: &[u8; 16],
    current: u64,
    prefix: ConsumedPrefix,
) -> Result<ConsumedScanWindow, StorageError> {
    let sql = format!(
        "SELECT relay_seq FROM sync_inbox \
         WHERE log_id=?1 AND relay_seq>?2 AND {} \
         ORDER BY relay_seq LIMIT ?3",
        prefix.inbox_condition()
    );
    let window_limit = i64::try_from(PREFIX_SCAN_LIMIT).unwrap_or(i64::MAX);
    let (mut seqs, inbox_truncated) = {
        let mut statement = conn.prepare(&sql)?;
        let mut rows = statement.query(rusqlite::params_from_iter([
            Value::Blob(log_id.to_vec()),
            Value::Integer(i64_of(current)?),
            Value::Integer(window_limit),
        ]))?;
        let mut seqs = Vec::new();
        while let Some(row) = rows.next()? {
            let value: i64 = row.get(0)?;
            seqs.push(u64::try_from(value).map_err(|_| StorageError::Corrupt {
                context: "sync_inbox.relay_seq 为负数".to_owned(),
            })?);
        }
        let truncated = seqs.len() == PREFIX_SCAN_LIMIT;
        (seqs, truncated)
    };
    let inbox_last = seqs.last().copied();

    let lower = skipped_received_key(log_id, current.saturating_add(1))?;
    let upper = skipped_received_key(log_id, u64::MAX)?;
    let mut skipped_last: Option<u64> = None;
    let skipped_truncated = {
        let mut statement = conn
            .prepare("SELECT key FROM meta WHERE key>=?1 AND key<=?2 ORDER BY key ASC LIMIT ?3")?;
        let mut rows = statement.query(rusqlite::params_from_iter([
            Value::Text(lower),
            Value::Text(upper),
            Value::Integer(window_limit),
        ]))?;
        let mut scanned = 0usize;
        while let Some(row) = rows.next()? {
            scanned = scanned.saturating_add(1);
            let key: String = row.get(0)?;
            // 格式不认识的键不参与前缀推进（fail-safe）：它既不算已消费，也不提供
            // 截断边界。可解析的未观测序号仍严格大于这里见过的最大可解析序号，所以
            // 用已解析最大值做下界是安全的。
            let Some(seq) = parse_skipped_received_seq(&key) else {
                continue;
            };
            skipped_last = Some(skipped_last.map_or(seq, |last| last.max(seq)));
            if seq > current {
                seqs.push(seq);
            }
        }
        scanned == PREFIX_SCAN_LIMIT
    };

    seqs.sort_unstable();
    seqs.dedup();
    Ok(ConsumedScanWindow {
        seqs,
        inbox_truncated,
        skipped_truncated,
        inbox_last,
        skipped_last,
    })
}

/// 解析 `sync.skipped_received.<hex log_id>.<20 位序号>` 的序号。
///
/// 格式不认识就返回 `None`：不是本函数写下的键不参与前缀推进，宁可让前缀停在这里
/// （fail-safe），也不把来路不明的 meta 键当成"已消费"。
fn parse_skipped_received_seq(key: &str) -> Option<u64> {
    let rest = key.strip_prefix(META_SYNC_SKIPPED_RECEIVED_PREFIX)?;
    let (hex, digits) = rest.split_once('.')?;
    if hex.len() != 32 || digits.len() != 20 {
        return None;
    }
    if !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// `sync.skipped_received.<hex log_id>.<seq 20 位>`。
pub(crate) fn skipped_received_key(log_id: &[u8], relay_seq: u64) -> Result<String, StorageError> {
    if log_id.len() != 16 {
        return Err(StorageError::Corrupt {
            context: format!("sync.log_id 长度不是 16：{}", log_id.len()),
        });
    }
    let mut hex = String::with_capacity(32);
    for byte in log_id {
        use core::fmt::Write as _;
        write!(hex, "{byte:02x}").map_err(|_| StorageError::Corrupt {
            context: "sync.log_id 十六进制编码失败".to_owned(),
        })?;
    }
    Ok(format!(
        "{META_SYNC_SKIPPED_RECEIVED_PREFIX}{hex}.{relay_seq:020}"
    ))
}

/// `meta.value` 的**唯一**解读口径：只认 BLOB，其它 SQLite 类型一律是损坏。
///
/// 三个读取入口共享本函数，避免"同一份库读出两套结论"漂移：
///
/// - [`crate::Vault::get_meta`]（actor 线程上的命令）；
/// - `sync_bridge::read_meta`（同步桥经 Vault 命令读取）；
/// - 本模块的 [`read_meta_on`]（worker 独占连接 / 事务内的批量 inbox 校验）。
///
/// 为什么不把其它类型「当字节」或「当缺失」：
///
/// - **当缺失会静默绕过持久屏障**：`sync.repack_required`（轮换后必须重打包）、
///   `sync.log_id`（incarnation 绑定）、`sync.received_seq` 这类键"读不到"就等价于
///   "没有屏障/没有游标"，把一处损坏放大成绕过安全前置条件；
/// - **当字节与 schema 相悖**：`meta.value` 是 `BLOB NOT NULL`（STRICT 表），能读出
///   TEXT/INTEGER/REAL 本身就说明库被外部改坏或 schema 漂移，继续按字节解释只会让
///   损坏值流入上层语义。
///
/// 正常写入路径写不出非 BLOB（STRICT 表会直接拒绝），所以这里命中的都是
/// "库被外部改坏"的场景：宁可 fail-closed 报损坏，也不猜。
pub(crate) fn meta_bytes_of(key: &str, value: Value) -> Result<Vec<u8>, StorageError> {
    match value {
        Value::Blob(bytes) => Ok(bytes),
        other => Err(StorageError::Corrupt {
            context: format!(
                "meta[{key}] 应为 BLOB，实际 {}（库被外部改坏或 schema 漂移）",
                sqlite_type_name(&other)
            ),
        }),
    }
}

/// SQLite 值的类型名（只用于错误信息）。
fn sqlite_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "NULL",
        Value::Integer(_) => "INTEGER",
        Value::Real(_) => "REAL",
        Value::Text(_) => "TEXT",
        Value::Blob(_) => "BLOB",
    }
}

/// 在给定连接（或事务）上读一条 meta；键不存在返回 `None`。
///
/// 类型判定走 [`meta_bytes_of`]：只认 BLOB，其它类型是 [`StorageError::Corrupt`]
/// （不再静默当作缺失）。
pub(crate) fn read_meta_on(conn: &Connection, key: &str) -> Result<Option<Vec<u8>>, StorageError> {
    let mut statement = conn.prepare("SELECT value FROM meta WHERE key=?1")?;
    let mut rows = statement.query([key])?;
    match rows.next()? {
        None => Ok(None),
        Some(row) => Ok(Some(meta_bytes_of(key, row.get::<usize, Value>(0)?)?)),
    }
}

fn read_meta_u64_on(conn: &Connection, key: &str) -> Result<Option<u64>, StorageError> {
    let Some(bytes) = read_meta_on(conn, key)? else {
        return Ok(None);
    };
    if bytes.len() != 8 {
        return Err(StorageError::Corrupt {
            context: format!("{key} 应为 BE64 8B，实际 {}B", bytes.len()),
        });
    }
    Ok(Some(u64::from_be_bytes(bytes.try_into().expect("长度8"))))
}

fn write_meta_on(conn: &Connection, key: &str, value: &[u8]) -> Result<(), StorageError> {
    conn.execute(
        "INSERT INTO meta(key,value) VALUES(?1,?2) \
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        rusqlite::params![key, value],
    )?;
    Ok(())
}

fn array16_bytes(bytes: Vec<u8>, field: &str) -> Result<[u8; 16], StorageError> {
    bytes
        .try_into()
        .map_err(|bytes: Vec<u8>| StorageError::Corrupt {
            context: format!("{field} 应为 16B，实际 {}B", bytes.len()),
        })
}

fn i64_of(value: u64) -> Result<i64, StorageError> {
    i64::try_from(value).map_err(|_| StorageError::Corrupt {
        context: format!("数值 {value} 超出 SQLite INTEGER"),
    })
}

````


## `crates/storage/src/actor.rs`

actor.rs：begin_immediate、exec_inbox_batch/exec_transaction

````rust
//! 单写者 actor：一个专用线程独占 `rusqlite::Connection`。
//!
//! 为什么需要它：
//!
//! - `rusqlite::Connection` 不是 `Sync`，多个线程各持一个连接写同一个文件时，
//!   SQLite 的写锁会退化成 `SQLITE_BUSY` 竞争；
//! - ADR 0001 §3.9 要求「每 vault 单 writer actor」。
//!
//! 实现方式：所有读写都变成消息排到同一个线程上顺序执行，外部通过 `mpsc` 发命令、
//! 通过容量为 1 的 `sync_channel` 收结果（等价 oneshot）。`Connection` 在工作线程内
//! 创建、也在工作线程内析构，**绝不跨线程移动**，因此关闭连接时的 WAL checkpoint
//! 也发生在它自己的线程上。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};

use rusqlite::types::Value;
use rusqlite::{Connection, Transaction, TransactionBehavior};

use crate::connection;
use crate::error::StorageError;
use crate::migrate;
use crate::sync_meta::{self, ConsumedPrefix};

/// 查询结果中的一行：按 SQL 的列顺序排列的值。
pub type Row = Vec<Value>;

/// 单次请求的回复通道：容量 1，等价 oneshot。
type Reply<T> = SyncSender<Result<T, StorageError>>;

/// 事务里的一条语句。
#[derive(Debug, Clone)]
pub struct Statement {
    /// SQL 文本。
    pub sql: String,
    /// 绑定参数。
    pub params: Vec<Value>,
}

impl Statement {
    /// 构造一条语句。
    pub fn new(sql: impl Into<String>, params: Vec<Value>) -> Self {
        Self {
            sql: sql.into(),
            params,
        }
    }
}

/// 「这条语句必须影响 N 行」的事务守卫；不满足则整笔事务回滚。
///
/// 用来把 CAS 变成原子语义：例如 `UPDATE loro_docs ... WHERE head_seq = ?` 必须影响 1 行，
/// 否则说明 head 已被别人推进，此时**不能**提交本批 update。
#[derive(Debug, Clone, Copy)]
pub struct AffectedGuard {
    /// 语句下标（0 起）。
    pub statement: usize,
    /// 期望影响的行数。
    pub expected: usize,
}

impl AffectedGuard {
    /// 要求第 `statement` 条语句恰好影响 `expected` 行。
    pub fn exactly(statement: usize, expected: usize) -> Self {
        Self {
            statement,
            expected,
        }
    }
}

/// 批量 inbox 原子写入的一行。
///
/// 由 `sync_bridge::store_inbox_verified_batch` 构造，送到 worker 线程后在同一事务里
/// 完成「既有行比对 + 新行插入 + `received` 连续前缀推进」；字段含义与
/// [`crate::VerifiedInboxRow`] 一一对应，只是在 actor 层不依赖 sync 语义。
#[derive(Debug, Clone)]
pub(crate) struct InboxBatchRow {
    /// relay 日志 incarnation（同一批必须一致）。
    pub(crate) log_id: [u8; 16],
    /// relay 日志序号（必须 > 0；同一批内不得重复）。
    pub(crate) relay_seq: u64,
    /// 密文对象名（32B）。
    pub(crate) object_id: [u8; 32],
    /// 加密 epoch（必须 > 0）。
    pub(crate) epoch: u64,
    /// 已解密并完成载荷校验的 `UpdateV1` 明文。
    pub(crate) verified_payload: Vec<u8>,
}

/// 工作线程命令。
enum Command {
    /// 执行单条写语句，返回受影响行数。
    Exec {
        /// SQL 文本。
        sql: String,
        /// 绑定参数。
        params: Vec<Value>,
        /// 结果回传通道。
        reply: Reply<usize>,
    },
    /// 执行多条语句（DDL / PRAGMA / 测试夹具），不关心行数。
    ExecBatch {
        /// SQL 文本。
        sql: String,
        /// 结果回传通道。
        reply: Reply<()>,
    },
    /// 执行单条查询，返回全部行。
    Query {
        /// SQL 文本。
        sql: String,
        /// 绑定参数。
        params: Vec<Value>,
        /// 结果回传通道。
        reply: Reply<Vec<Row>>,
    },
    /// 在**同一个事务**里执行多条语句，并按守卫校验影响行数。
    ExecTransaction {
        /// 语句列表。
        statements: Vec<Statement>,
        /// 守卫列表。
        guards: Vec<AffectedGuard>,
        /// 结果回传通道：每条语句的影响行数。
        reply: Reply<Vec<usize>>,
    },
    /// 读取一条 meta（键值，BLOB）。
    GetMeta {
        /// 键（命名空间由调用方约定，storage 不理解语义）。
        key: String,
        /// 结果回传通道。
        reply: Reply<Option<Vec<u8>>>,
    },
    /// 读取 `PRAGMA user_version`。
    UserVersion {
        /// 结果回传通道。
        reply: Reply<i64>,
    },
    /// 原子批量写 inbox：校验、既有行冲突检测、新行插入、`received` 连续前缀推进
    /// 全部在**同一个事务**里完成。
    ExecInboxBatch {
        /// 待写入行（worker 内重新校验/排序）。
        rows: Vec<InboxBatchRow>,
        /// 结果回传通道：本次真正新插入的行数（幂等命中不计）。
        reply: Reply<usize>,
    },
    /// 在同一条命令里推进连续前缀（读窗口 + 一次写最终值都在 worker 连接上完成）。
    AdvanceConsumedPrefix {
        /// 要推进的前缀（received / integrated）。
        prefix: ConsumedPrefix,
        /// 结果回传通道。
        reply: Reply<()>,
    },
}

/// vault 数据库句柄，克隆后共享同一个工作线程。
///
/// 它是 `Send + Sync`，可以跨线程共享；所有操作串行落在工作线程上。
#[derive(Debug, Clone)]
pub struct Vault {
    /// 共享状态，最后一个句柄析构时触发优雅关闭。
    inner: Arc<Shared>,
}

/// 句柄之间共享的状态。
#[derive(Debug)]
pub(crate) struct Shared {
    /// 命令发送端；关闭时取走并丢弃，工作线程的 `recv` 随之返回错误。
    tx: Mutex<Option<Sender<Command>>>,
    /// 工作线程句柄，只在最后一个句柄析构时 join。
    worker: Mutex<Option<JoinHandle<()>>>,
    /// 数据库文件路径。
    path: PathBuf,
    /// 变更通知的订阅者槽位（`(id, sender)`）。
    pub(crate) event_slots: Mutex<Vec<crate::events::EventSlot>>,
    /// 分配订阅者 id 用。
    pub(crate) next_event_slot: std::sync::atomic::AtomicU64,
    /// 「事件送不出去、请做全量对账」的粘性标记。
    pub(crate) reconcile_required: std::sync::atomic::AtomicBool,
}

impl Vault {
    /// 打开（或创建）给定路径的 vault 数据库。
    ///
    /// 内部依次完成：
    ///
    /// 1. 父目录不存在则创建；
    /// 2. 在工作线程打开连接；
    /// 3. 应用并读回校验 PRAGMA（WAL / FULL / foreign_keys / busy_timeout）；
    /// 4. 迁移到最新 schema。
    ///
    /// 只有全部成功才返回 `Ok`；失败时工作线程已被回收，不会泄漏。
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }

        let (tx, rx) = mpsc::channel::<Command>();
        // 初始化握手：工作线程把「打开 + 配置 + 迁移」的结果回传，成功后才交出句柄。
        let (init_tx, init_rx) = mpsc::sync_channel::<Result<(), StorageError>>(1);

        let worker_path = path.clone();
        let handle = thread::Builder::new()
            .name("menghunote-storage".to_owned())
            .spawn(move || worker_main(&worker_path, &rx, init_tx))?;

        match init_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                inner: Arc::new(Shared {
                    tx: Mutex::new(Some(tx)),
                    worker: Mutex::new(Some(handle)),
                    path,
                    event_slots: crate::events::new_slots(),
                    next_event_slot: std::sync::atomic::AtomicU64::new(0),
                    reconcile_required: std::sync::atomic::AtomicBool::new(false),
                }),
            }),
            Ok(Err(error)) => {
                // 初始化失败：关掉通道并等线程退出，再把手上的错误原样上报。
                drop(tx);
                let _ = handle.join();
                Err(error)
            }
            Err(_) => {
                // 工作线程没回消息就没了（panic 或连接都放不下），按通道失效处理。
                drop(tx);
                let _ = handle.join();
                Err(StorageError::ChannelClosed {
                    context: "等待工作线程初始化",
                })
            }
        }
    }

    /// vault 数据库文件路径。
    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    /// 打开一条**只读快照**连接（WAL 下与写路径并发，不占写 actor）。
    ///
    /// 语义见 [`crate::ReadSnapshot`]：只保证读到"已提交"的数据；忙则返回
    /// [`StorageError::SnapshotBusy`]，不做无限重试。
    pub fn open_read_snapshot(&self) -> Result<crate::ReadSnapshot, StorageError> {
        crate::ReadSnapshot::open(&self.inner.path)
    }

    /// 订阅轻量变更事件（容量 = 队列上限）。
    ///
    /// 发布走 `try_send`：队列满 / 无订阅者时不会阻塞写，而是置"需要全量对账"标记。
    pub fn subscribe_events(&self, capacity: usize) -> crate::StorageEventSubscriber {
        self.inner.subscribe_events(capacity)
    }

    /// 取走并清除"事件没送到、请做全量对账"的标记。
    pub fn take_reconcile_required(&self) -> bool {
        self.inner.take_reconcile_required()
    }

    /// 读一条 meta（键值 BLOB）；键不存在返回 `None`。
    ///
    /// 判定与另两个入口共享同一个 helper（`sync_meta::meta_bytes_of`）：**只认 BLOB**，
    /// TEXT/INTEGER/REAL 都是 [`StorageError::Corrupt`]，不会静默当成"没有这条 meta"
    /// ——否则 `sync.repack_required` / `sync.log_id` 之类持久屏障会被绕过。
    pub fn get_meta(&self, key: &str) -> Result<Option<Vec<u8>>, StorageError> {
        self.call(|reply| Command::GetMeta {
            key: key.to_owned(),
            reply,
        })
    }

    /// 写一条 meta：**走写 actor 的小事务**（`INSERT ... ON CONFLICT`），不绕过 actor。
    pub fn set_meta(&self, key: &str, value: &[u8]) -> Result<(), StorageError> {
        self.exec_transaction(
            &[Statement::new(
                "INSERT INTO meta(key, value) VALUES(?1, ?2) \
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                vec![Value::Text(key.to_owned()), Value::Blob(value.to_vec())],
            )],
            &[AffectedGuard::exactly(0, 1)],
        )?;
        Ok(())
    }

    /// 发布一条变更事件（内部用；B 物化与结构投影提交后调用）。
    ///
    /// 返回成功投递的订阅者数量；`0` 说明没人收到，已置"需要全量对账"标记。
    pub(crate) fn publish_event(&self, event: crate::StorageEvent) -> usize {
        self.inner.publish_event(event)
    }

    /// 当前 schema 版本（`PRAGMA user_version`）。
    pub fn user_version(&self) -> Result<i64, StorageError> {
        self.call(|reply| Command::UserVersion { reply })
    }

    /// 低层写命令：执行单条 SQL，返回受影响行数。
    ///
    /// 只用于写语句；会把结果集返回给调用方的语句请用 [`Vault::query`]。
    /// 参数用 [`Value`] 传递，调用方负责别把用户输入拼进 SQL。
    pub fn exec(&self, sql: &str, params: &[Value]) -> Result<usize, StorageError> {
        self.call(|reply| Command::Exec {
            sql: sql.to_owned(),
            params: params.to_vec(),
            reply,
        })
    }

    /// 低层批处理：执行多条语句（DDL、PRAGMA、测试夹具等），全部成功才返回 `Ok`。
    pub fn exec_batch(&self, sql: &str) -> Result<(), StorageError> {
        self.call(|reply| Command::ExecBatch {
            sql: sql.to_owned(),
            reply,
        })
    }

    /// 低层读命令：执行单条查询，返回全部行。
    pub fn query(&self, sql: &str, params: &[Value]) -> Result<Vec<Row>, StorageError> {
        self.call(|reply| Command::Query {
            sql: sql.to_owned(),
            params: params.to_vec(),
            reply,
        })
    }

    /// 低层事务命令：多条语句在同一事务里执行，`guards` 不满足则整笔回滚。
    ///
    /// 返回每条语句的影响行数（顺序与 `statements` 一致）。
    ///
    /// 事务以 `BEGIN IMMEDIATE` 显式开启（与批量 inbox 入口共用
    /// [`begin_immediate`]）：写锁在事务开始就取，并发写者按 `busy_timeout` 排队，
    /// 而不是在第一条语句处撞上"锁升级不走 busy handler"的立即失败。
    pub fn exec_transaction(
        &self,
        statements: &[Statement],
        guards: &[AffectedGuard],
    ) -> Result<Vec<usize>, StorageError> {
        self.call(|reply| Command::ExecTransaction {
            statements: statements.to_vec(),
            guards: guards.to_vec(),
            reply,
        })
    }

    /// 原子批量写 inbox（storage 内部入口；外部请走
    /// [`crate::sync_bridge::store_inbox_verified_batch`]）。
    ///
    /// 空批次是合法 no-op（与逐条入口一致），不会打开事务。
    pub(crate) fn exec_inbox_batch(&self, rows: Vec<InboxBatchRow>) -> Result<usize, StorageError> {
        if rows.is_empty() {
            return Ok(0);
        }
        self.call(|reply| Command::ExecInboxBatch { rows, reply })
    }

    /// 推进连续前缀（`received` / `integrated`），同一条命令内完成读窗口与写最终值。
    pub(crate) fn advance_consumed_prefix(
        &self,
        prefix: ConsumedPrefix,
    ) -> Result<(), StorageError> {
        self.call(|reply| Command::AdvanceConsumedPrefix { prefix, reply })
    }

    /// 把一条命令发给工作线程并等待回复。
    fn call<T>(&self, make: impl FnOnce(Reply<T>) -> Command) -> Result<T, StorageError> {
        let (reply_tx, reply_rx) = mpsc::sync_channel::<Result<T, StorageError>>(1);
        {
            // 只锁住"取发送端 + 投递命令"这一段，等回复时不持锁。
            let guard = lock(&self.inner.tx);
            let Some(tx) = guard.as_ref() else {
                return Err(StorageError::ChannelClosed {
                    context: "提交命令，工作线程已关闭",
                });
            };
            tx.send(make(reply_tx))
                .map_err(|_| StorageError::ChannelClosed {
                    context: "提交命令，工作线程已退出",
                })?;
        }
        reply_rx.recv().map_err(|_| StorageError::ChannelClosed {
            context: "等待命令回复，工作线程已退出",
        })?
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        // 丢弃发送端：工作线程的 recv 立刻返回错误，退出循环并在自己的线程上关闭连接
        // （关闭时 SQLite 会做 WAL checkpoint）。Drop 里只做清理，绝不 panic。
        drop(lock(&self.tx).take());
        if let Some(worker) = lock(&self.worker).take() {
            // 工作线程若已 panic，join 返回 Err；这里吞掉，避免析构路径上二次 panic。
            let _ = worker.join();
        }
    }
}

/// 工作线程入口：独占连接，顺序处理命令。
fn worker_main(path: &Path, rx: &Receiver<Command>, init: Reply<()>) {
    let conn = match connection::open_and_migrate(path) {
        Ok(conn) => conn,
        Err(error) => {
            let _ = init.send(Err(error));
            return;
        }
    };

    if init.send(Ok(())).is_err() {
        // 主线程已放弃初始化；连接在这里随函数返回于本线程析构。
        return;
    }

    while let Ok(command) = rx.recv() {
        handle_command(&conn, command);
    }
}

/// 执行一条命令并回传结果；调用方可能已经走了，回复失败只忽略。
fn handle_command(conn: &Connection, command: Command) {
    match command {
        Command::Exec { sql, params, reply } => {
            let _ = reply.send(exec(conn, &sql, &params));
        }
        Command::ExecBatch { sql, reply } => {
            let _ = reply.send(conn.execute_batch(&sql).map_err(StorageError::from));
        }
        Command::Query { sql, params, reply } => {
            let _ = reply.send(query(conn, &sql, &params));
        }
        Command::ExecTransaction {
            statements,
            guards,
            reply,
        } => {
            let _ = reply.send(exec_transaction(conn, &statements, &guards));
        }
        Command::GetMeta { key, reply } => {
            // 与 sync_bridge / sync_meta 同一个读取 helper（只认 BLOB，其它类型报损坏）。
            let _ = reply.send(sync_meta::read_meta_on(conn, &key));
        }
        Command::UserVersion { reply } => {
            let _ = reply.send(migrate::user_version(conn));
        }
        Command::ExecInboxBatch { rows, reply } => {
            let _ = reply.send(exec_inbox_batch(conn, rows));
        }
        Command::AdvanceConsumedPrefix { prefix, reply } => {
            let _ = reply.send(sync_meta::advance_consumed_prefix_on(conn, prefix));
        }
    }
}

/// 在连接上显式开启一个 **Immediate** 写事务（`BEGIN IMMEDIATE`）。
///
/// 为什么不用 deferred（`conn.unchecked_transaction()` 的默认行为）：
///
/// - deferred 的 `BEGIN` 只是登记，第一条 SELECT 拿共享锁，等到第一条写语句才尝试把
///   共享锁升级成写锁；而 SQLite 的锁升级**不走 busy handler**——此刻若已有别的写事务
///   在途，不会用 `busy_timeout` 等待，而是直接返回 `SQLITE_BUSY`，让调用方在"最多等
///   5s"的预期之外立刻失败；
/// - `BEGIN IMMEDIATE` 在事务开始就取写锁，这一步**照常**受 `busy_timeout` 约束，与
///   ADR 0001 §3.9「每 vault 单 writer actor」的模型一致：写事务不该和别的写事务撞在
///   升级点上。
///
/// 两个写事务入口都走这里，论证只写在这一处（避免注释在两个调用点各改一半而漂移）：
///
/// - [`exec_inbox_batch`]：先读后写，deferred 会在锁升级处失败，且读到的旧快照还会先
///   报出误导性的 `SyncConflict`；
/// - [`exec_transaction`]：承载 A 事务/资产/repack/finish，调用方习惯把写语句放最前，
///   但**没有任何断言**保证这一点（以后多一条只读语句就退化成升级路径），所以同样
///   显式 Immediate。
fn begin_immediate(conn: &Connection) -> Result<Transaction<'_>, StorageError> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    Ok(tx)
}

/// 批量 inbox 的原子执行。
///
/// 全部动作在 worker 持有独占连接上的**同一个事务**里：
///
/// 1. 校验批内行：同 `log_id`、`relay_seq/epoch > 0`、`relay_seq` 不重复；按序号排序；
/// 2. 校验 `sync.log_id`（不存在则写，存在不同则 [`StorageError::SyncLogMismatch`]）；
/// 3. 读该序号范围既有行，逐行比较 `object_id/epoch/verified_payload`：
///    - 完全相同 → 幂等跳过（不插入、不触发任何守卫）；
///    - 任意不同 → [`StorageError::SyncConflict`]，`tx` 在此 drop，整笔回滚（一行不写）；
/// 4. 新行在同一事务插入（每行要求恰好影响 1 行）；
/// 5. **同一事务内**推进 `received` 连续前缀（与普通路径同一套扫描语义）；
/// 6. 提交后返回新插入行数（幂等行不计）。
///
/// 先读后写的竞态因此被消除：快照与插入属于同一个写事务（SQLite 的单写者锁），
/// 且一次提交只产生一次 fsync；崩溃/冲突只会留下「什么都没发生」或「行+前缀一起提交」。
///
/// 事务用 [`begin_immediate`] 显式开启（而不是 deferred）：本事务先读后写，deferred
/// 的锁升级不走 busy handler，完整论证见该函数。
fn exec_inbox_batch(
    conn: &Connection,
    mut rows: Vec<InboxBatchRow>,
) -> Result<usize, StorageError> {
    if rows.is_empty() {
        return Ok(0);
    }
    let log_id = rows[0].log_id;
    for row in &rows {
        if row.log_id != log_id {
            return Err(StorageError::Corrupt {
                context: "批量 inbox 行必须属于同一 log_id".to_owned(),
            });
        }
        if row.relay_seq == 0 || row.epoch == 0 {
            return Err(StorageError::Corrupt {
                context: "inbox relay_seq/epoch 必须 >0".to_owned(),
            });
        }
    }
    rows.sort_by_key(|row| row.relay_seq);
    if rows
        .windows(2)
        .any(|pair| pair[0].relay_seq == pair[1].relay_seq)
    {
        return Err(StorageError::Corrupt {
            context: "批量 inbox 出现重复 relay_seq".to_owned(),
        });
    }

    // 事务模式与锁升级论证收在 [`begin_immediate`]：本事务先读后写，deferred 会在锁
    // 升级处立即 SQLITE_BUSY（见该函数的注释）。
    let tx = begin_immediate(conn)?;
    sync_meta::ensure_log_id_on(&tx, &log_id)?;

    let first_seq = rows.first().map_or(0, |row| row.relay_seq);
    let last_seq = rows.last().map_or(0, |row| row.relay_seq);
    let existing = read_existing_inbox(&tx, &log_id, first_seq, last_seq)?;
    let mut fresh: Vec<&InboxBatchRow> = Vec::with_capacity(rows.len());
    for row in &rows {
        match existing.get(&row.relay_seq) {
            Some((object_id, epoch, payload)) => {
                if *object_id != row.object_id
                    || *epoch != row.epoch
                    || *payload != row.verified_payload
                {
                    // 不 commit：tx 在此 drop，SQLite 自动回滚整笔事务（包括已插入的新行）。
                    return Err(StorageError::SyncConflict {
                        context: format!("inbox {log_id:?}/{} 字节不一致", row.relay_seq),
                    });
                }
            }
            None => fresh.push(row),
        }
    }

    if !fresh.is_empty() {
        insert_inbox_rows(&tx, &log_id, &fresh)?;
    }

    // 与插入处于同一事务：提交后 received 一定覆盖本批新行；崩溃/冲突则一起回滚。
    sync_meta::advance_consumed_prefix_on(&tx, ConsumedPrefix::Received)?;
    tx.commit()?;
    Ok(fresh.len())
}

/// 既有 inbox 行的比较字段：`(object_id, epoch, verified_payload)`。
type ExistingInboxFacts = ([u8; 32], u64, Vec<u8>);

/// 读 `[first_seq, last_seq]` 范围内的既有 inbox 行，按序号建索引。
fn read_existing_inbox(
    conn: &Connection,
    log_id: &[u8; 16],
    first_seq: u64,
    last_seq: u64,
) -> Result<BTreeMap<u64, ExistingInboxFacts>, StorageError> {
    let mut statement = conn.prepare(
        "SELECT relay_seq,object_id,epoch,verified_payload FROM sync_inbox \
         WHERE log_id=?1 AND relay_seq>=?2 AND relay_seq<=?3",
    )?;
    let mut rows = statement.query(rusqlite::params_from_iter([
        Value::Blob(log_id.to_vec()),
        Value::Integer(i64_of(first_seq)?),
        Value::Integer(i64_of(last_seq)?),
    ]))?;
    let mut existing = BTreeMap::new();
    while let Some(row) = rows.next()? {
        let seq: i64 = row.get(0)?;
        let seq = u64::try_from(seq).map_err(|_| StorageError::Corrupt {
            context: "sync_inbox.relay_seq 为负数".to_owned(),
        })?;
        let object_id: Vec<u8> = row.get(1)?;
        let object_id = object_id
            .try_into()
            .map_err(|bytes: Vec<u8>| StorageError::Corrupt {
                context: format!("sync_inbox.object_id 应为 32B，实际 {}B", bytes.len()),
            })?;
        let epoch: i64 = row.get(2)?;
        let epoch = u64::try_from(epoch).map_err(|_| StorageError::Corrupt {
            context: "sync_inbox.epoch 为负数".to_owned(),
        })?;
        let payload: Option<Vec<u8>> = row.get(3)?;
        existing.insert(seq, (object_id, epoch, payload.unwrap_or_default()));
    }
    Ok(existing)
}

/// 在同一事务里插入新行；每行都要求恰好影响 1 行，否则整笔回滚。
fn insert_inbox_rows(
    conn: &Connection,
    log_id: &[u8; 16],
    rows: &[&InboxBatchRow],
) -> Result<(), StorageError> {
    let mut statement = conn.prepare(
        "INSERT INTO sync_inbox(log_id,relay_seq,object_id,epoch,state,verified_payload) \
         VALUES(?1,?2,?3,?4,'verified',?5)",
    )?;
    for row in rows {
        let affected = statement.execute(rusqlite::params_from_iter([
            Value::Blob(log_id.to_vec()),
            Value::Integer(i64_of(row.relay_seq)?),
            Value::Blob(row.object_id.to_vec()),
            Value::Integer(i64_of(row.epoch)?),
            Value::Blob(row.verified_payload.clone()),
        ]))?;
        if affected != 1 {
            return Err(StorageError::SyncConflict {
                context: format!("inbox {log_id:?}/{} 写入冲突", row.relay_seq),
            });
        }
    }
    Ok(())
}

/// SQLite INTEGER（有符号 64 位）到 u64 的转换；负数说明库/入参不自洽。
fn i64_of(value: u64) -> Result<i64, StorageError> {
    i64::try_from(value).map_err(|_| StorageError::Corrupt {
        context: format!("数值 {value} 超出 SQLite INTEGER"),
    })
}

/// 执行单条写语句。
fn exec(conn: &Connection, sql: &str, params: &[Value]) -> Result<usize, StorageError> {
    let mut statement = conn.prepare(sql)?;
    Ok(statement.execute(rusqlite::params_from_iter(params))?)
}

/// 执行单条查询，把每一行转成 [`Row`]。
fn query(conn: &Connection, sql: &str, params: &[Value]) -> Result<Vec<Row>, StorageError> {
    let mut statement = conn.prepare(sql)?;
    let column_count = statement.column_count();
    let mut rows = statement.query(rusqlite::params_from_iter(params))?;

    let mut result = Vec::new();
    while let Some(row) = rows.next()? {
        let mut values = Vec::with_capacity(column_count);
        for index in 0..column_count {
            values.push(row.get::<_, Value>(index)?);
        }
        result.push(values);
    }
    Ok(result)
}

/// 在一个事务里执行多条语句；任一语句报错或守卫不满足 → 整笔回滚。
fn exec_transaction(
    conn: &Connection,
    statements: &[Statement],
    guards: &[AffectedGuard],
) -> Result<Vec<usize>, StorageError> {
    // 连接由工作线程独占，不会有嵌套事务；事务模式与批量 inbox 共用 begin_immediate
    // （锁升级论证见该函数），语句顺序/守卫/CAS 语义都不变。
    let tx = begin_immediate(conn)?;

    // 守卫必须在**该语句执行后立刻**校验：否则后续语句可能先撞 UNIQUE/FK，
    // 把「head 被人推进了」这个真正原因盖掉（CAS 语义要求先于任何写入冲突暴露）。
    for guard in guards {
        if guard.statement >= statements.len() {
            return Err(StorageError::GuardFailed {
                statement: guard.statement,
                expected: guard.expected,
                actual: 0,
            });
        }
    }

    let mut affected = Vec::with_capacity(statements.len());
    for (index, statement) in statements.iter().enumerate() {
        let mut prepared = tx.prepare(&statement.sql)?;
        let rows = prepared.execute(rusqlite::params_from_iter(&statement.params))?;
        affected.push(rows);

        for guard in guards.iter().filter(|guard| guard.statement == index) {
            if rows != guard.expected {
                // 不 commit：tx 在此 drop，SQLite 自动回滚整笔事务
                return Err(StorageError::GuardFailed {
                    statement: index,
                    expected: guard.expected,
                    actual: rows,
                });
            }
        }
    }

    tx.commit()?;
    Ok(affected)
}

/// 获取互斥锁；即使锁被毒化也继续使用内部数据（清理路径不允许 panic）。
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

````


## `crates/storage/src/body_doc.rs`

body_doc.rs：BodyDoc、import_oplog_only、pending_update_from_oplog

````rust
//! 每个正文 Doc 一个 `LoroDoc`（ADR 0001 §3.1 / §3.4）。
//!
//! 约定：
//!
//! - 正文只存**原始 Markdown**（LF 归一），不用 Loro Rich Text，不把块树落库；
//! - 对外编辑接口的 `pos` / `len` 一律是 **Unicode scalar**（Loro 实测单位；
//!   通用换算的唯一实现在 [`menghunote_text`]，实测结论见 `tests/loro_units.rs`）；
//!   `convert_pos` 对半截索引**向下取整**、`to_delta()` **只产出 Insert** 这两个陷阱也记在那里，
//!   适配逻辑留在本文件，不搬进 text（ADR 0003：text 只管通用单位，不管 Loro API 语义）；
//!   IME 需要 UTF-16 时用 [`BodyDoc::insert_utf16`] / [`BodyDoc::delete_utf16`] 的显式接口；
//! - 编辑即映射：一次改动就是 `delete(range) + insert(text)`（ADR 0001 §3.4），
//!   过期基准不许直接应用（基准校验由调用方的 `command`/frontier 负责，本层只校验范围）；
//! - 导出增量用「上次 durable 的 VV」作起点，**不在本层推进任何 watermark**。

use loro::cursor::{Cursor, Side};
use loro::{ExportMode, Frontiers, LoroDoc, LoroText, TextDelta, UndoManager, VersionVector};

use crate::error::StorageError;
use menghunote_text::CoordError;

/// 正文容器在 `LoroDoc` 里的名字。
pub const BODY_CONTAINER: &str = "body";

/// 落库时记录的 codec 标识（ADR 0001 §3.9「每个 snapshot/update 携带实际 codec 标识」）。
pub const CODEC_VERSION: &str = "loro-1.17.1";

/// 本切片写入的 `app_doc_schema`。
pub const APP_DOC_SCHEMA: i64 = 1;

/// 本切片要求的最低读取方版本。
pub const MIN_READER: i64 = 1;

/// 一次本地提交导出出的增量。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingUpdate {
    /// Loro update 字节（相对导出基准的增量）。
    pub payload: Vec<u8>,
    /// 该增量应用后的 frontier（`Frontiers::encode`）。
    pub end_frontier: Vec<u8>,
    /// 该增量应用后的 VV（`VersionVector::encode`）。
    pub end_vv: Vec<u8>,
}

/// 一个正文 CRDT 文档。
pub struct BodyDoc {
    doc_id: String,
    doc: LoroDoc,
    text: LoroText,
}

impl std::fmt::Debug for BodyDoc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BodyDoc")
            .field("doc_id", &self.doc_id)
            .field("scalars", &self.scalar_len())
            .finish()
    }
}

impl BodyDoc {
    /// 新建文档并设置本机 peer id。
    ///
    /// peer id 必须每设备唯一（ADR 0001 §3.2）；克隆旧备份当新实例时必须换新 peer id。
    pub fn create(doc_id: impl Into<String>, peer_id: u64) -> Result<Self, StorageError> {
        let doc = LoroDoc::new();
        doc.set_peer_id(peer_id)?;
        let text = doc.get_text(BODY_CONTAINER);
        Ok(Self {
            doc_id: doc_id.into(),
            doc,
            text,
        })
    }

    /// 从快照 + 后续 update 重放出一个文档（启动恢复路径）。
    pub fn restore(
        doc_id: impl Into<String>,
        peer_id: u64,
        snapshot: Option<&[u8]>,
        updates: &[Vec<u8>],
    ) -> Result<Self, StorageError> {
        let this = Self::create(doc_id, peer_id)?;
        if let Some(snapshot) = snapshot {
            this.doc.import(snapshot)?;
        }
        for payload in updates {
            this.doc.import(payload)?;
        }
        Ok(this)
    }

    /// 文档 ID。
    pub fn doc_id(&self) -> &str {
        &self.doc_id
    }

    /// 当前正文（原始 Markdown）。
    pub fn text(&self) -> String {
        self.text.to_string()
    }

    /// 以 Unicode scalar 计的正文长度（Loro 实测单位）。
    pub fn scalar_len(&self) -> usize {
        self.text.len_unicode()
    }

    /// 以 UTF-16 code unit 计的正文长度。
    pub fn utf16_len(&self) -> usize {
        self.text.len_utf16()
    }

    /// 以 UTF-8 字节计的正文长度。
    pub fn byte_len(&self) -> usize {
        self.text.len_utf8()
    }

    /// 按 **Unicode scalar** 位置插入文本。越界报错，不截断。
    pub fn insert_scalars(&self, pos: usize, text: &str) -> Result<(), StorageError> {
        self.check_range(pos, 0)?;
        self.text.insert(pos, text)?;
        self.doc.commit();
        Ok(())
    }

    /// 按 **Unicode scalar** 位置删除 `len` 个字符。
    pub fn delete_scalars(&self, pos: usize, len: usize) -> Result<(), StorageError> {
        self.check_range(pos, len)?;
        self.text.delete(pos, len)?;
        self.doc.commit();
        Ok(())
    }

    /// 替换 `[pos, pos + len)` 的 scalar 区间（IME 确认与格式替换都走这一个入口）。
    pub fn replace_scalars(&self, pos: usize, len: usize, text: &str) -> Result<(), StorageError> {
        self.check_range(pos, len)?;
        if len > 0 {
            self.text.delete(pos, len)?;
        }
        if !text.is_empty() {
            self.text.insert(pos, text)?;
        }
        self.doc.commit();
        Ok(())
    }

    /// 应用一段 delta（`Retain` / `Insert` / `Delete` 的计数单位实测为 Unicode scalar）。
    pub fn apply_delta_scalars(&self, delta: &[TextDelta]) -> Result<(), StorageError> {
        self.text.apply_delta(delta)?;
        self.doc.commit();
        Ok(())
    }

    /// 按 **UTF-16** 位置插入；落在代理对中间会报错（非法半代理对位置）。
    pub fn insert_utf16(&self, pos: usize, text: &str) -> Result<(), StorageError> {
        self.text.insert_utf16(pos, text)?;
        self.doc.commit();
        Ok(())
    }

    /// 按 **UTF-16** 位置删除；落在代理对中间会报错。
    pub fn delete_utf16(&self, pos: usize, len: usize) -> Result<(), StorageError> {
        self.text.delete_utf16(pos, len)?;
        self.doc.commit();
        Ok(())
    }

    /// 取 `pos` 处的相对光标；`pos` 单位实测为 Unicode scalar。
    pub fn cursor_at_scalar(&self, pos: usize) -> Option<Cursor> {
        self.text.get_cursor(pos, Side::Middle)
    }

    /// 把光标解析成当前版本的 scalar 位置（会随并发编辑移动）。
    pub fn resolve_cursor_scalar(&self, cursor: &Cursor) -> Result<usize, StorageError> {
        self.doc
            .get_cursor_pos(cursor)
            .map(|pos| pos.current.pos)
            .map_err(|error| StorageError::CursorUnresolved(error.to_string()))
    }

    /// 设置**下一次提交**的 origin（撤销分组 / 后台变更排除用）。
    pub fn set_next_commit_origin(&self, origin: &str) {
        self.doc.set_next_commit_origin(origin);
    }

    /// 撤销管理器（本 peer 栈，ADR 0002 §8）。
    pub fn undo_manager(&self) -> UndoManager {
        UndoManager::new(&self.doc)
    }

    /// 当前 frontier。
    pub fn frontiers(&self) -> Frontiers {
        self.doc.state_frontiers()
    }

    /// 当前 oplog 版本向量。
    pub fn vv(&self) -> VersionVector {
        self.doc.oplog_vv()
    }

    /// 自 `from` 之后的新增 ops（A 事务要落库的增量）。
    pub fn pending_update(&self, from: &VersionVector) -> Result<PendingUpdate, StorageError> {
        let payload = self.doc.export(ExportMode::updates(from))?;
        Ok(PendingUpdate {
            payload,
            end_frontier: self.frontiers().encode(),
            end_vv: self.vv().encode(),
        })
    }

    /// 全量历史 update（对端首次同步用）。
    pub fn export_all_updates(&self) -> Result<Vec<u8>, StorageError> {
        Ok(self.doc.export(ExportMode::all_updates())?)
    }

    /// 完整快照。
    pub fn export_snapshot(&self) -> Result<Vec<u8>, StorageError> {
        Ok(self.doc.export(ExportMode::Snapshot)?)
    }

    /// 导入一个 update 或 snapshot（幂等；重复导入不会产生新 ops）。
    pub fn import(&self, bytes: &[u8]) -> Result<(), StorageError> {
        self.doc.import(bytes)?;
        Ok(())
    }

    /// 基线 merge 专用：把增量只合进 **oplog**（detached），不触发 DocState 的
    /// diff 计算与状态应用。
    ///
    /// 语义等价性：CRDT 的 oplog 合并与 state 应用是同一组 op；detached 只跳过
    /// 「把新 op 应用到容器状态」这一步，oplog 的 VV/frontier 与 attached 完全一致。
    /// 调用方只能使用 [`Self::pending_update_from_oplog`] / [`Self::oplog_frontiers_encoded`]
    /// 这类纯 oplog 接口，**不得**在 detached 之后读正文。
    ///
    /// 为什么需要它：对「已有本地内容」的文档，Loro 的 attached import 会重算
    /// diff 并把大文本重新应用到 state，成本随总文本长度超线性增长
    /// （5→20 MiB 实测 ≈15–17×，见 `phase1a_release` 探针）；baseline merge 只需要
    /// 「oplog 并集 + 导出差集」，detached 路径把这部分成本降到 O(增量)。
    pub(crate) fn import_oplog_only(&self, bytes: &[u8]) -> Result<(), StorageError> {
        if !self.doc.is_detached() {
            self.doc.detach();
        }
        self.doc.import(bytes)?;
        Ok(())
    }

    /// detached 模式下合并后的 oplog frontier（attached 等价物 = [`Self::frontiers`]）。
    pub(crate) fn oplog_frontiers_encoded(&self) -> Vec<u8> {
        self.doc.oplog_frontiers().encode()
    }

    /// 从 oplog 导出 `from` 之后的增量；`end_frontier`/`end_vv` 取 **oplog** 版本。
    ///
    /// 仅用于已 [`Self::import_oplog_only`] 的文档；对 attached 文档它与
    /// [`Self::pending_update`] 结果一致（state 在 latest 时两者相同）。
    pub(crate) fn pending_update_from_oplog(
        &self,
        from: &VersionVector,
    ) -> Result<PendingUpdate, StorageError> {
        let payload = self.doc.export(ExportMode::updates(from))?;
        Ok(PendingUpdate {
            payload,
            end_frontier: self.oplog_frontiers_encoded(),
            end_vv: self.doc.oplog_vv().encode(),
        })
    }

    /// 校验 `[pos, pos + len)` 在当前文本内（scalar 口径），避免 Loro 抛出的越界信息丢上下文。
    ///
    /// 坐标错误直接复用 `menghunote_text::CoordError`（ADR 0003：全仓只有一份）。
    fn check_range(&self, pos: usize, len: usize) -> Result<(), StorageError> {
        let total = self.scalar_len();
        let end = pos.checked_add(len).ok_or(CoordError::ScalarOutOfBounds {
            index: usize::MAX,
            len: total,
        })?;
        if end > total {
            return Err(CoordError::ScalarOutOfBounds {
                index: end,
                len: total,
            }
            .into());
        }
        Ok(())
    }
}

````


## `crates/sync/src/engine.rs`

engine.rs：pull/integrate、聚合预检/多轮、资产上传

````rust
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

````


---

# 附录 B：完整代码 diff（`5d01e9e..HEAD`，同步相关）


````diff
diff --git a/crates/storage/src/actor.rs b/crates/storage/src/actor.rs
index 6824e6c..0b4f53a 100644
--- a/crates/storage/src/actor.rs
+++ b/crates/storage/src/actor.rs
@@ -263,7 +263,11 @@ impl Vault {
         self.inner.take_reconcile_required()
     }
 
-    /// 读一条 meta（键值 BLOB）；不存在返回 `None`。
+    /// 读一条 meta（键值 BLOB）；键不存在返回 `None`。
+    ///
+    /// 判定与另两个入口共享同一个 helper（`sync_meta::meta_bytes_of`）：**只认 BLOB**，
+    /// TEXT/INTEGER/REAL 都是 [`StorageError::Corrupt`]，不会静默当成"没有这条 meta"
+    /// ——否则 `sync.repack_required` / `sync.log_id` 之类持久屏障会被绕过。
     pub fn get_meta(&self, key: &str) -> Result<Option<Vec<u8>>, StorageError> {
         self.call(|reply| Command::GetMeta {
             key: key.to_owned(),
@@ -328,6 +332,10 @@ impl Vault {
     /// 低层事务命令：多条语句在同一事务里执行，`guards` 不满足则整笔回滚。
     ///
     /// 返回每条语句的影响行数（顺序与 `statements` 一致）。
+    ///
+    /// 事务以 `BEGIN IMMEDIATE` 显式开启（与批量 inbox 入口共用
+    /// [`begin_immediate`]）：写锁在事务开始就取，并发写者按 `busy_timeout` 排队，
+    /// 而不是在第一条语句处撞上"锁升级不走 busy handler"的立即失败。
     pub fn exec_transaction(
         &self,
         statements: &[Statement],
@@ -433,7 +441,8 @@ fn handle_command(conn: &Connection, command: Command) {
             let _ = reply.send(exec_transaction(conn, &statements, &guards));
         }
         Command::GetMeta { key, reply } => {
-            let _ = reply.send(get_meta(conn, &key));
+            // 与 sync_bridge / sync_meta 同一个读取 helper（只认 BLOB，其它类型报损坏）。
+            let _ = reply.send(sync_meta::read_meta_on(conn, &key));
         }
         Command::UserVersion { reply } => {
             let _ = reply.send(migrate::user_version(conn));
@@ -447,6 +456,30 @@ fn handle_command(conn: &Connection, command: Command) {
     }
 }
 
+/// 在连接上显式开启一个 **Immediate** 写事务（`BEGIN IMMEDIATE`）。
+///
+/// 为什么不用 deferred（`conn.unchecked_transaction()` 的默认行为）：
+///
+/// - deferred 的 `BEGIN` 只是登记，第一条 SELECT 拿共享锁，等到第一条写语句才尝试把
+///   共享锁升级成写锁；而 SQLite 的锁升级**不走 busy handler**——此刻若已有别的写事务
+///   在途，不会用 `busy_timeout` 等待，而是直接返回 `SQLITE_BUSY`，让调用方在"最多等
+///   5s"的预期之外立刻失败；
+/// - `BEGIN IMMEDIATE` 在事务开始就取写锁，这一步**照常**受 `busy_timeout` 约束，与
+///   ADR 0001 §3.9「每 vault 单 writer actor」的模型一致：写事务不该和别的写事务撞在
+///   升级点上。
+///
+/// 两个写事务入口都走这里，论证只写在这一处（避免注释在两个调用点各改一半而漂移）：
+///
+/// - [`exec_inbox_batch`]：先读后写，deferred 会在锁升级处失败，且读到的旧快照还会先
+///   报出误导性的 `SyncConflict`；
+/// - [`exec_transaction`]：承载 A 事务/资产/repack/finish，调用方习惯把写语句放最前，
+///   但**没有任何断言**保证这一点（以后多一条只读语句就退化成升级路径），所以同样
+///   显式 Immediate。
+fn begin_immediate(conn: &Connection) -> Result<Transaction<'_>, StorageError> {
+    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
+    Ok(tx)
+}
+
 /// 批量 inbox 的原子执行。
 ///
 /// 全部动作在 worker 持有独占连接上的**同一个事务**里：
@@ -463,8 +496,8 @@ fn handle_command(conn: &Connection, command: Command) {
 /// 先读后写的竞态因此被消除：快照与插入属于同一个写事务（SQLite 的单写者锁），
 /// 且一次提交只产生一次 fsync；崩溃/冲突只会留下「什么都没发生」或「行+前缀一起提交」。
 ///
-/// 事务用 **Immediate** 显式开启（而不是 deferred）：本事务先读后写，deferred 的锁升级
-/// 详见下方注释。
+/// 事务用 [`begin_immediate`] 显式开启（而不是 deferred）：本事务先读后写，deferred
+/// 的锁升级不走 busy handler，完整论证见该函数。
 fn exec_inbox_batch(
     conn: &Connection,
     mut rows: Vec<InboxBatchRow>,
@@ -495,16 +528,9 @@ fn exec_inbox_batch(
         });
     }
 
-    // 显式 **Immediate** 事务（不是 deferred）：
-    //
-    // 本事务里先读（校验 `sync.log_id`、比对 `[first_seq,last_seq]` 既有行）再写
-    // （插新行、推进 received 前缀）。Deferred 事务的 `BEGIN` 只是登记，第一条 SELECT
-    // 拿共享锁，等到第一条写语句才尝试把共享锁升级成写锁；而 SQLite 的锁升级**不走
-    // busy handler**——此刻若已有别的写事务在途，不会用 `busy_timeout` 等待，而是直接
-    // 返回 `SQLITE_BUSY`，让调用方在"最多等 5s"的预期之外立刻失败。`BEGIN IMMEDIATE`
-    // 在事务开始就取写锁（这一步照常受 `busy_timeout` 约束），与 ADR 0001 §3.9
-    // 「每 vault 单 writer actor」的模型一致：写事务不该和别的写事务撞在升级点上。
-    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
+    // 事务模式与锁升级论证收在 [`begin_immediate`]：本事务先读后写，deferred 会在锁
+    // 升级处立即 SQLITE_BUSY（见该函数的注释）。
+    let tx = begin_immediate(conn)?;
     sync_meta::ensure_log_id_on(&tx, &log_id)?;
 
     let first_seq = rows.first().map_or(0, |row| row.relay_seq);
@@ -613,29 +639,6 @@ fn i64_of(value: u64) -> Result<i64, StorageError> {
     })
 }
 
-/// 读一条 meta。
-fn get_meta(conn: &Connection, key: &str) -> Result<Option<Vec<u8>>, StorageError> {
-    let mut statement = conn.prepare("SELECT value FROM meta WHERE key = ?1")?;
-    let mut rows = statement.query([key])?;
-    match rows.next()? {
-        None => Ok(None),
-        Some(row) => match row.get::<usize, Value>(0)? {
-            Value::Blob(blob) => Ok(Some(blob)),
-            Value::Text(text) => Ok(Some(text.into_bytes())),
-            other => Err(StorageError::ManifestShape {
-                column: "meta.value",
-                found: match other {
-                    Value::Null => "NULL",
-                    Value::Integer(_) => "INTEGER",
-                    Value::Real(_) => "REAL",
-                    Value::Text(_) => "TEXT",
-                    Value::Blob(_) => "BLOB",
-                },
-            }),
-        },
-    }
-}
-
 /// 执行单条写语句。
 fn exec(conn: &Connection, sql: &str, params: &[Value]) -> Result<usize, StorageError> {
     let mut statement = conn.prepare(sql)?;
@@ -665,8 +668,9 @@ fn exec_transaction(
     statements: &[Statement],
     guards: &[AffectedGuard],
 ) -> Result<Vec<usize>, StorageError> {
-    // 连接由工作线程独占，不会有嵌套事务，所以用 &self 版本的 unchecked_transaction
-    let tx = conn.unchecked_transaction()?;
+    // 连接由工作线程独占，不会有嵌套事务；事务模式与批量 inbox 共用 begin_immediate
+    // （锁升级论证见该函数），语句顺序/守卫/CAS 语义都不变。
+    let tx = begin_immediate(conn)?;
 
     // 守卫必须在**该语句执行后立刻**校验：否则后续语句可能先撞 UNIQUE/FK，
     // 把「head 被人推进了」这个真正原因盖掉（CAS 语义要求先于任何写入冲突暴露）。
diff --git a/crates/storage/src/baseline_apply.rs b/crates/storage/src/baseline_apply.rs
index b44412d..38e8732 100644
--- a/crates/storage/src/baseline_apply.rs
+++ b/crates/storage/src/baseline_apply.rs
@@ -14,6 +14,12 @@
 //!   `sync.remote_known_vv.<doc_id>`（见 [`crate::remote_known`]），跨 journal 保留，
 //!   供后续 tombstone 判定区分「远端已删的已同步文档」与「local-only 文档」。
 //! - `finish` 只做**单调**水位/cursor CAS 与 journal 状态迁移；失败不推进任何 cursor。
+//! - `begin` 记录本 checkpoint 的**预期参与集合**（覆盖到的 doc 与资产，存于既有 `meta`
+//!   单行、只本地可见）：`finish` 必须先证明集合里每一项都已 `applied=1`，才允许写任何
+//!   meta；少跑一批（或跑错集合）返回 [`BaselineApplyFinish::Conflict`]，而不是推进 S
+//!   之后让 pull 永久跳过 relay 事件。
+//! - tombstone 的 outbox 判定会在**同一写事务内**用 SQL 重判一次：判定完成后被并发写入的
+//!   outbox 行只会把 `deleted` 退化为 `kept_local_edit`，绝不误删本地编辑。
 //! - `abort` 只清 staging，不删已导入的 Loro/asset 数据，也不回滚远端已知 VV。
 
 use std::cell::RefCell;
@@ -39,6 +45,10 @@ use crate::Vault;
 pub const BASELINE_MERGE_BATCH: &str = "baseline_merge";
 /// relay 页面 cursor 的 meta 键（与 sync `engine::RELAY_CURSOR_META` 对齐）。
 pub const META_SYNC_RELAY_CURSOR: &str = "sync.relay_cursor";
+/// [`BaselineExpectation`] 的持久化 meta 键（**只本地可见**；不是 schema/wire）。
+pub const META_BASELINE_EXPECTED: &str = "sync.baseline_expected";
+/// 预期集合编码的魔数（防止误读旧值/损坏值）。
+const EXPECTED_MAGIC: &[u8] = b"MNBASEEXP1";
 /// storage 内部构造 BodyDoc 时使用的 peer id（只影响新文档的本地 op id，不影响 import）。
 const BASELINE_PEER_ID: u64 = 1;
 
@@ -165,7 +175,11 @@ pub struct BaselineApplyState {
     pub applied_docs: u64,
     pub applied_assets: u64,
     pub kept_local_edit: u64,
-    /// staging 状态且没有未完成子步骤时为 true；finish 还要求 cursor CAS 通过。
+    /// begin 记录的预期文档数；没有记录（旧 journal/损坏）时为 `None`，此时不可 finish。
+    pub expected_docs: Option<u64>,
+    /// begin 记录的预期资产数；同上。
+    pub expected_assets: Option<u64>,
+    /// staging 且 begin 记录的预期集合已全部 `applied=1` 时为 true；finish 还要求 cursor CAS 通过。
     pub finishable: bool,
     /// 所选基线自带的 relay cursor；是否真正写进 `meta` 取决于 finish 的单调判断。
     pub chosen_cursor: Option<String>,
@@ -177,10 +191,126 @@ pub enum BaselineApplyFinish {
     /// 成功推进 cursor；携带 completed 状态。
     Applied(BaselineApplyState),
     NotFound,
-    /// begin 记录的水位/游标已被并发改写、S 不符或批次未完成；未修改任何行。
+    /// begin 记录的水位/游标已被并发改写、S 不符、或预期参与集合未全部应用；
+    /// 未修改任何行。
     Conflict,
 }
 
+/// `begin_baseline_apply` 记录的**预期参与集合**：本 checkpoint 覆盖的全部文档与资产。
+///
+/// 为什么需要：journal 只记录「已应用了多少」，不记录「本来应该应用哪些」。少跑一批
+/// （进程被杀、调用方漏批、fallback 选错集）时旧实现仍能 finish 并推进 S，随后普通 pull
+/// 会把这段前缀当成已消费，**永久跳过其中所有 relay 事件**。
+///
+/// 集合以单条 [`META_BASELINE_EXPECTED`] `meta` 行持久化（不新增列/表、不参与同步、不是
+/// wire）；`new` 会排序去重，因此同一集合的不同传入顺序被视为同一份预期。
+#[derive(Debug, Clone, Default, PartialEq, Eq)]
+pub struct BaselineExpectation {
+    docs: Vec<String>,
+    assets: Vec<[u8; 32]>,
+}
+
+impl BaselineExpectation {
+    /// 规范化（排序 + 去重）后构造。
+    pub fn new(
+        docs: impl IntoIterator<Item = String>,
+        assets: impl IntoIterator<Item = [u8; 32]>,
+    ) -> Self {
+        let mut docs: Vec<String> = docs.into_iter().collect();
+        docs.sort_unstable();
+        docs.dedup();
+        let mut assets: Vec<[u8; 32]> = assets.into_iter().collect();
+        assets.sort_unstable();
+        assets.dedup();
+        Self { docs, assets }
+    }
+
+    /// 预期的文档 id（已排序去重）。
+    pub fn docs(&self) -> &[String] {
+        &self.docs
+    }
+
+    /// 预期的资产 object_id（已排序去重）。
+    pub fn assets(&self) -> &[[u8; 32]] {
+        &self.assets
+    }
+
+    /// 预期文档数。
+    pub fn doc_count(&self) -> u64 {
+        u64::try_from(self.docs.len()).unwrap_or(u64::MAX)
+    }
+
+    /// 预期资产数。
+    pub fn asset_count(&self) -> u64 {
+        u64::try_from(self.assets.len()).unwrap_or(u64::MAX)
+    }
+
+    /// 集合是否为空（空集合视为"begin 已显式声明无需应用任何项"）。
+    pub fn is_empty(&self) -> bool {
+        self.docs.is_empty() && self.assets.is_empty()
+    }
+
+    /// 编码成单条 `meta` 值：`magic | doc_count | (len,doc)* | asset_count | oid*`（全部 BE）。
+    fn encode(&self) -> Vec<u8> {
+        let mut out = Vec::with_capacity(
+            EXPECTED_MAGIC.len()
+                + 16
+                + self.docs.iter().map(|doc| 8 + doc.len()).sum::<usize>()
+                + self.assets.len() * 32,
+        );
+        out.extend_from_slice(EXPECTED_MAGIC);
+        out.extend_from_slice(&self.doc_count().to_be_bytes());
+        for doc in &self.docs {
+            let len = u64::try_from(doc.len()).unwrap_or(u64::MAX);
+            out.extend_from_slice(&len.to_be_bytes());
+            out.extend_from_slice(doc.as_bytes());
+        }
+        out.extend_from_slice(&self.asset_count().to_be_bytes());
+        for asset in &self.assets {
+            out.extend_from_slice(asset);
+        }
+        out
+    }
+
+    /// 解码并校验 `encode` 的输出；任何不合法都视为本地 meta 损坏（fail-closed）。
+    fn decode(bytes: &[u8]) -> Result<Self, StorageError> {
+        let mut offset = 0usize;
+        let magic = take_expected_bytes(bytes, &mut offset, EXPECTED_MAGIC.len())?;
+        if magic != EXPECTED_MAGIC {
+            return Err(corrupt_expected("魔数不符"));
+        }
+        let doc_count = read_expected_u64(bytes, &mut offset)?;
+        let doc_count =
+            usize::try_from(doc_count).map_err(|_| corrupt_expected("doc 数超过 usize"))?;
+        // 损坏数据不得预分配过大内存。
+        let mut docs = Vec::with_capacity(doc_count.min(64));
+        for _ in 0..doc_count {
+            let len = read_expected_u64(bytes, &mut offset)?;
+            let len = usize::try_from(len).map_err(|_| corrupt_expected("doc 长度超过 usize"))?;
+            let raw = take_expected_bytes(bytes, &mut offset, len)?;
+            let doc = std::str::from_utf8(raw)
+                .map_err(|_| corrupt_expected("doc_id 不是 UTF-8"))?
+                .to_owned();
+            docs.push(doc);
+        }
+        let asset_count = read_expected_u64(bytes, &mut offset)?;
+        let asset_count =
+            usize::try_from(asset_count).map_err(|_| corrupt_expected("资产数超过 usize"))?;
+        let mut assets = Vec::with_capacity(asset_count.min(64));
+        for _ in 0..asset_count {
+            let raw = take_expected_bytes(bytes, &mut offset, 32)?;
+            let object_id: [u8; 32] = raw
+                .try_into()
+                .map_err(|_| corrupt_expected("object_id 长度非法"))?;
+            assets.push(object_id);
+        }
+        if offset != bytes.len() {
+            return Err(corrupt_expected("尾部有多余字节"));
+        }
+        Ok(Self { docs, assets })
+    }
+}
+
 /// 一个已验证的基线资产（HMAC oid 是 relay 对象名；明文 hash 是真实 sha256）。
 #[derive(Debug, Clone, PartialEq, Eq)]
 pub struct BaselineAsset {
@@ -212,8 +342,14 @@ struct PreparedDoc {
     baseline_vv: Vec<u8>,
     expected_head: Option<HeadState>,
     update: Option<PendingUpdate>,
+    /// 读到的旧「远端已知 VV」（值 CAS 的期望值；`None` = 读时不存在）。
+    ///
+    /// 写入必须走 CAS（P2-5）：本批准备期间，普通 pull 的 A 事务完全可能已经把同一个
+    /// `sync.remote_known_vv.<doc_id>` 扩大；无条件覆盖会把它退回到本批的并集（丢失
+    /// 远端已知证据 → tombstone 误判为 local-only，只会保守保留，但证据丢了）。
+    remote_known_expected: Option<VersionVector>,
     /// 合并后的远端已知 VV（写入 `sync.remote_known_vv.<doc_id>`）。
-    remote_known: Vec<u8>,
+    remote_known_merged: VersionVector,
 }
 struct PreparedTombstone {
     doc_id: String,
@@ -227,14 +363,22 @@ struct PreparedAsset {
     statement: Option<Statement>,
 }
 
-/// 开始一次 baseline apply staging；同参数重复调用幂等，冲突参数返回 SyncConflict。
+/// 开始一次 baseline apply staging；同参数（含预期参与集合）重复调用幂等，冲突参数返回
+/// SyncConflict。
+///
+/// `expected` 必须覆盖**本 checkpoint 的全部** docs 与资产：finish 前会拿它逐项核对
+/// `baseline_apply_docs/applied=1` 与 `baseline_apply_assets/applied=1`（见
+/// [`BaselineExpectation`]）。同一 checkpoint/S/cursor 但 `expected` 不同 → 冲突，
+/// 不会静默沿用旧集合。
 pub fn begin_baseline_apply(
     vault: &Vault,
     checkpoint_id: &[u8; 16],
     baseline_s: u64,
     source_cursor: &str,
+    expected: &BaselineExpectation,
 ) -> Result<BaselineApplyState, StorageError> {
     let now = now_secs()?;
+    let expected_bytes = expected.encode();
     let existing = load_journal(vault)?;
     if let Some(existing) = &existing {
         if existing.status == BaselineApplyStatus::Staging
@@ -242,7 +386,16 @@ pub fn begin_baseline_apply(
             && existing.baseline_s == baseline_s
             && existing.source_cursor == source_cursor
         {
-            return state_from_journal(existing);
+            // 同参数重入必须幂等（崩溃续跑路径）；但预期集合必须对得上：
+            // 旧 journal 没有集合记录、或调用方换了集合，都不能静默继续。
+            let stored = vault.get_meta(META_BASELINE_EXPECTED)?;
+            if stored.as_deref() != Some(expected_bytes.as_slice()) {
+                return Err(StorageError::SyncConflict {
+                    context: "同一 checkpoint 的 baseline apply staging 已有另一份预期参与集合"
+                        .to_owned(),
+                });
+            }
+            return state_from_journal(vault, existing);
         }
         if existing.status == BaselineApplyStatus::Staging {
             return Err(StorageError::SyncConflict {
@@ -256,38 +409,43 @@ pub fn begin_baseline_apply(
 
     let mut statements = Vec::new();
     let mut guards = Vec::new();
-    if existing.is_some() {
-        // completed 的 journal 可以安全替换；旧 journal 子表只属于它。
-        //
-        // 但 `baseline_apply_docs.baseline_vv` 是**跨 journal 仍然有效**的
-        // 「远端已知 VV」证据：必须在删除前落到非破坏性本地 meta 里，
-        // 否则已有设备下一次快照追赶时会把远端已删文档误判成 local-only。
-        //
-        // `DO NOTHING`：已有 meta（普通 pull/integrate 记录的更新的远端已知 VV）
-        // 永远不降级；缺失时才由旧 baseline_vv 补上。
-        statements.push(Statement::new(
-            format!(
-                "INSERT INTO meta(key,value) \
-                 SELECT '{}' || doc_id, baseline_vv FROM baseline_apply_docs WHERE applied=1 \
-                 ON CONFLICT(key) DO NOTHING",
-                remote_known::META_REMOTE_KNOWN_VV_PREFIX
-            ),
-            Vec::new(),
-        ));
-        statements.push(Statement::new(
-            "DELETE FROM baseline_apply_docs",
-            Vec::new(),
-        ));
-        statements.push(Statement::new(
-            "DELETE FROM baseline_apply_tombstones",
-            Vec::new(),
-        ));
-        statements.push(Statement::new(
-            "DELETE FROM baseline_apply_assets",
-            Vec::new(),
-        ));
-        statements.push(Statement::new("DELETE FROM baseline_apply", Vec::new()));
-    }
+    // 旧 journal 子表的「远端已知 VV」证据回填 + 清理。
+    //
+    // `baseline_apply_docs.baseline_vv` 是**跨 journal 仍然有效**的「远端已知 VV」
+    // 证据：必须在删除前落到非破坏性本地 meta 里，否则已有设备下一次快照追赶时会把
+    // 远端已删文档误判成 local-only（只会保守保留，但证据丢了）。
+    //
+    // **无条件执行**（P2-5）：走到这里只可能是「没有 journal」或「completed journal」
+    // （staging 同参已幂等早退、异参已 SyncConflict 返回）。completed journal 的
+    // baseline_vv 要回填；没有 journal 但残留子表（旧版本/异常写留下的孤儿行）也必须
+    // 先回填再清掉，否则下面无条件 `DELETE` 会把证据静默丢弃。
+    //
+    // `DO NOTHING` 是单调断言：已有 meta（普通 pull/integrate 记录的更新的远端已知 VV）
+    // 永远不降级；缺失时才由旧 baseline_vv 补上。
+    statements.push(Statement::new(
+        format!(
+            "INSERT INTO meta(key,value) \
+             SELECT '{}' || doc_id, baseline_vv FROM baseline_apply_docs WHERE applied=1 \
+             ON CONFLICT(key) DO NOTHING",
+            remote_known::META_REMOTE_KNOWN_VV_PREFIX
+        ),
+        Vec::new(),
+    ));
+    // 旧 journal 子表只属于旧 journal（completed 或孤儿残留），一律清掉，
+    // 避免它们被下一轮 `apply_baseline_batch` 当成「已应用」。
+    statements.push(Statement::new(
+        "DELETE FROM baseline_apply_docs",
+        Vec::new(),
+    ));
+    statements.push(Statement::new(
+        "DELETE FROM baseline_apply_tombstones",
+        Vec::new(),
+    ));
+    statements.push(Statement::new(
+        "DELETE FROM baseline_apply_assets",
+        Vec::new(),
+    ));
+    statements.push(Statement::new("DELETE FROM baseline_apply", Vec::new()));
     let insert_index = statements.len();
     statements.push(Statement::new(
         "INSERT INTO baseline_apply(\
@@ -309,6 +467,19 @@ pub fn begin_baseline_apply(
     ));
     guards.push(AffectedGuard::exactly(insert_index, 1));
 
+    // 预期参与集合随 journal 同事务落 meta（replace 语义：completed journal 被替换时
+    // 旧集合必须一起被覆盖，否则 finish 会拿上一轮的集合核对本轮进度）。
+    let expected_index = statements.len();
+    statements.push(Statement::new(
+        "INSERT INTO meta(key,value) VALUES(?1,?2) \
+         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
+        vec![
+            Value::Text(META_BASELINE_EXPECTED.to_owned()),
+            Value::Blob(expected_bytes),
+        ],
+    ));
+    guards.push(AffectedGuard::exactly(expected_index, 1));
+
     match vault.exec_transaction(&statements, &guards) {
         Ok(_) => {}
         Err(StorageError::GuardFailed { .. }) => {
@@ -325,9 +496,10 @@ pub fn begin_baseline_apply(
 
 /// 当前 staging journal 的只读状态；没有 journal 返回 `None`。
 pub fn baseline_apply_state(vault: &Vault) -> Result<Option<BaselineApplyState>, StorageError> {
-    load_journal(vault)?
-        .map(|journal| state_from_journal(&journal))
-        .transpose()
+    match load_journal(vault)? {
+        None => Ok(None),
+        Some(journal) => state_from_journal(vault, &journal).map(Some),
+    }
 }
 
 /// 应用一批已验证的快照文档和远端 tombstone。
@@ -338,7 +510,7 @@ pub fn apply_baseline_batch(
 ) -> Result<BaselineApplyState, StorageError> {
     let journal = load_active_journal(vault)?;
     if docs.is_empty() && tombstones.is_empty() {
-        return state_from_journal(&journal);
+        return state_from_journal(vault, &journal);
     }
 
     let mut seen_docs = BTreeSet::new();
@@ -484,9 +656,10 @@ pub fn apply_baseline_batch(
             }
         }
 
-        let remote_known = match remote_known::load(vault, doc_id)? {
-            Some(existing) => remote_known::union(&existing, &baseline_vv).encode(),
-            None => baseline_vv.encode(),
+        let remote_known_expected = remote_known::load(vault, doc_id)?;
+        let remote_known_merged = match &remote_known_expected {
+            Some(existing) => remote_known::union(existing, &baseline_vv),
+            None => baseline_vv.clone(),
         };
         prepared_docs.push(PreparedDoc {
             doc_id: doc_id.clone(),
@@ -494,7 +667,8 @@ pub fn apply_baseline_batch(
             baseline_vv: baseline_vv.encode(),
             expected_head,
             update,
-            remote_known,
+            remote_known_expected,
+            remote_known_merged,
         });
         baseline_by_doc.insert(doc_id.clone(), local_extra);
     }
@@ -515,6 +689,8 @@ pub fn apply_baseline_batch(
     let now = now_secs()?;
     let mut statements = Vec::new();
     let mut guards = Vec::new();
+    // 远端已知 VV 的 CAS 守卫下标 → doc_id：GuardFailed 时用它给出明确冲突错误。
+    let mut remote_known_guards: Vec<(usize, String)> = Vec::new();
     for prepared in &prepared_docs {
         let journal_index = statements.len();
         statements.push(Statement::new(
@@ -532,17 +708,18 @@ pub fn apply_baseline_batch(
         guards.push(AffectedGuard::exactly(journal_index, 1));
 
         // 跨 journal 保留的「远端已知 VV」：本批快照的 VV 就是远端已见过的 ops。
-        // 与旧值取单调并集（旧值可能来自普通 pull/integrate 的更新远端已知集）。
+        // 与旧值取单调并集（旧值可能来自普通 pull/integrate 的更新远端已知集），
+        // 但写入必须是**值 CAS**：只有库中值仍是准备阶段读到的旧值才更新。
+        // 并发写者（普通 pull 的 A 事务）已经扩大过 → 0 行 → 整批回滚并报明确冲突，
+        // 由调用方重试；绝不把更大的值覆盖回本批的并集（P2-5）。
         let remote_index = statements.len();
-        statements.push(Statement::new(
-            "INSERT INTO meta(key,value) VALUES(?1,?2) \
-             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
-            vec![
-                Value::Text(remote_known::meta_key(&prepared.doc_id)),
-                Value::Blob(prepared.remote_known.clone()),
-            ],
+        statements.push(remote_known::cas_statement(
+            &prepared.doc_id,
+            prepared.remote_known_expected.as_ref(),
+            &prepared.remote_known_merged,
         ));
         guards.push(AffectedGuard::exactly(remote_index, 1));
+        remote_known_guards.push((remote_index, prepared.doc_id.clone()));
 
         let Some(update) = &prepared.update else {
             if prepared.expected_head.is_none() {
@@ -650,23 +827,42 @@ pub fn apply_baseline_batch(
     }
 
     for tombstone in &prepared_tombstones {
-        if tombstone.action == "deleted" {
-            statements.push(Statement::new(
-                "UPDATE notes SET deleted=1 WHERE body_doc_id=?1",
-                vec![Value::Text(tombstone.doc_id.clone())],
-            ));
-        }
+        // P1-2（TOCTOU）：`tombstone_can_delete` 在事务外读 outbox，判定完成后本地编辑的
+        // A 事务完全可能插进一条 `sync_outbox` 行；若仍按旧判定无条件软删，就会把刚编辑
+        // 的笔记删掉。因此最终 action 必须在**同一个写事务里**用 SQL 重判一次：
+        // 只允许 `deleted → kept_local_edit` 的降级，绝不允许 `kept → deleted` 升级。
         let index = statements.len();
         statements.push(Statement::new(
-            "INSERT INTO baseline_apply_tombstones(doc_id,action,updated_at) VALUES(?1,?2,?3) \
-             ON CONFLICT(doc_id) DO UPDATE SET action=excluded.action,updated_at=excluded.updated_at",
+            "INSERT INTO baseline_apply_tombstones(doc_id,action,updated_at) \
+             VALUES(?1, CASE WHEN EXISTS(SELECT 1 FROM sync_outbox WHERE doc_id=?1) \
+                             THEN 'kept_local_edit' ELSE ?2 END, ?3) \
+             ON CONFLICT(doc_id) DO UPDATE SET \
+               action=CASE WHEN EXISTS(SELECT 1 FROM sync_outbox WHERE doc_id=?1) \
+                           THEN 'kept_local_edit' ELSE excluded.action END, \
+               updated_at=excluded.updated_at",
             vec![
                 Value::Text(tombstone.doc_id.clone()),
                 Value::Text(tombstone.action.to_owned()),
                 Value::Integer(now),
             ],
         ));
+        // 这一句永远恰好写入 1 行（INSERT 或 DO UPDATE），是"判定已落库"的守卫：
+        // 说明事务内重判真的执行了，而不是被别的写入/约束挡掉。
         guards.push(AffectedGuard::exactly(index, 1));
+
+        if tombstone.action == "deleted" {
+            // 软删与 outbox 判定在同一事务内：只有重判仍为 `deleted`（且此刻没有
+            // outbox）才置 deleted=1。0 行是合法结果（该文档没有 notes 行，或已降级为
+            // kept_local_edit），所以这句本身不设 exactly 守卫；守卫语义在上一条。
+            statements.push(Statement::new(
+                "UPDATE notes SET deleted=1 \
+                 WHERE body_doc_id=?1 \
+                   AND NOT EXISTS(SELECT 1 FROM sync_outbox WHERE doc_id=?1) \
+                   AND EXISTS(SELECT 1 FROM baseline_apply_tombstones \
+                              WHERE doc_id=?1 AND action='deleted')",
+                vec![Value::Text(tombstone.doc_id.clone())],
+            ));
+        }
     }
 
     let journal_index = statements.len();
@@ -684,7 +880,20 @@ pub fn apply_baseline_batch(
     let transaction_started = Instant::now();
     match vault.exec_transaction(&statements, &guards) {
         Ok(_) => {}
-        Err(StorageError::GuardFailed { .. }) => {
+        Err(StorageError::GuardFailed { statement, .. }) => {
+            if let Some((_, doc_id)) = remote_known_guards
+                .iter()
+                .find(|(index, _)| *index == statement)
+            {
+                // 值 CAS 不命中：准备阶段读到的旧值已被普通 pull 的 A 事务改写。
+                // 整批已回滚（update/head/journal 都没有落库），重跑本批即可读到并集。
+                return Err(StorageError::SyncConflict {
+                    context: format!(
+                        "baseline 文档 {doc_id} 的远端已知 VV 值 CAS 失败：\
+                         并发写者已改写旧值；整批已回滚，可重试"
+                    ),
+                });
+            }
             return Err(StorageError::SyncConflict {
                 context: "baseline batch 期间 journal/doc head 已变化".to_owned(),
             });
@@ -705,7 +914,7 @@ pub fn apply_baseline_assets(
 ) -> Result<BaselineApplyState, StorageError> {
     let journal = load_active_journal(vault)?;
     if assets.is_empty() {
-        return state_from_journal(&journal);
+        return state_from_journal(vault, &journal);
     }
 
     let mut seen_oid = BTreeSet::new();
@@ -826,6 +1035,10 @@ pub fn apply_baseline_assets(
 ///
 /// CAS 仍是"begin 时记录、finish 时校验"：任一字段在 finish 前被并发改写即返回
 /// [`BaselineApplyFinish::Conflict`]，整笔事务不写。
+///
+/// **完整性前提**（P1-1）：begin 记录在 [`META_BASELINE_EXPECTED`] 的预期参与集合必须
+/// 全部 `applied=1`。少跑一批（哪怕 status 仍是 staging）返回 Conflict；旧 journal
+/// 没有集合记录同样 Conflict（拿不到证明就不允许推进 S），绝不明知不完整还写水位。
 pub fn finish_baseline_apply(
     vault: &Vault,
     chosen_baseline_s: u64,
@@ -839,6 +1052,14 @@ pub fn finish_baseline_apply(
         return Ok(BaselineApplyFinish::Conflict);
     }
 
+    // 完整性校验在任何写语句之前：拿不到 begin 的预期集合，或集合未全齐，一律 Conflict。
+    let Some(expected) = load_expected_set(vault)? else {
+        return Ok(BaselineApplyFinish::Conflict);
+    };
+    if !expected_is_complete(vault, &journal, &expected)? {
+        return Ok(BaselineApplyFinish::Conflict);
+    }
+
     // finish 前先校验 begin 时记录的旧值；任何一项被并发改写都按 Conflict 处理，不写任何行。
     let current = sync_cursor_state(vault)?;
     let current_relay_cursor = vault.get_meta(META_SYNC_RELAY_CURSOR)?;
@@ -937,7 +1158,7 @@ pub fn finish_baseline_apply(
     Ok(BaselineApplyFinish::Applied(state))
 }
 
-/// 只清 staging journal（含子表）；已导入的 Loro update/资产保留。
+/// 只清 staging journal（含子表和预期参与集合）；已导入的 Loro update/资产保留。
 pub fn abort_baseline_apply(vault: &Vault) -> Result<bool, StorageError> {
     let Some(journal) = load_journal(vault)? else {
         return Ok(false);
@@ -953,6 +1174,12 @@ pub fn abort_baseline_apply(vault: &Vault) -> Result<bool, StorageError> {
             "DELETE FROM baseline_apply WHERE singleton=1 AND status='staging'",
             Vec::new(),
         ),
+        // 预期集合只属于被清掉的 journal；留着会被下一次 begin 的 replace 覆盖，
+        // 但清掉可以避免"没有 journal 却有预期集合"的中间态被误读。
+        Statement::new(
+            "DELETE FROM meta WHERE key=?1",
+            vec![Value::Text(META_BASELINE_EXPECTED.to_owned())],
+        ),
     ];
     match vault.exec_transaction(&statements, &[AffectedGuard::exactly(3, 1)]) {
         Ok(_) => Ok(true),
@@ -1134,6 +1361,86 @@ fn load_applied_doc_digests(vault: &Vault) -> Result<BTreeMap<String, [u8; 32]>,
         .collect()
 }
 
+/// 读 begin 记录的预期参与集合；缺失返回 `None`（旧 journal/异常，按不可 finish 处理）。
+fn load_expected_set(vault: &Vault) -> Result<Option<BaselineExpectation>, StorageError> {
+    match vault.get_meta(META_BASELINE_EXPECTED)? {
+        None => Ok(None),
+        Some(bytes) => BaselineExpectation::decode(&bytes).map(Some),
+    }
+}
+
+/// 预期集合是否已全部 `applied=1`。
+///
+/// 先比计数（每批都会更新 `applied_docs/applied_assets`，因此未齐时 O(1) 就能拒绝），
+/// 计数相等后再逐项确认集合身份：光比数量会放过"漏一篇 + 多跑一篇"的情况。计数相等
+/// 且预期项全部在已应用集合中 ⇒ 两个集合相等。
+fn expected_is_complete(
+    vault: &Vault,
+    journal: &ApplyJournal,
+    expected: &BaselineExpectation,
+) -> Result<bool, StorageError> {
+    if journal.applied_docs != expected.doc_count()
+        || journal.applied_assets != expected.asset_count()
+    {
+        return Ok(false);
+    }
+    if expected.is_empty() {
+        return Ok(true);
+    }
+    let applied_docs: BTreeSet<String> = load_applied_doc_digests(vault)?.into_keys().collect();
+    let docs_complete = expected
+        .docs()
+        .iter()
+        .all(|doc_id| applied_docs.contains(doc_id));
+    if !docs_complete {
+        return Ok(false);
+    }
+    let applied_assets = load_applied_asset_ids(vault)?;
+    Ok(expected
+        .assets()
+        .iter()
+        .all(|object_id| applied_assets.contains(object_id)))
+}
+
+/// 当前 journal 中 `applied=1` 的资产 object_id 集合。
+fn load_applied_asset_ids(vault: &Vault) -> Result<BTreeSet<[u8; 32]>, StorageError> {
+    let rows = vault.query(
+        "SELECT object_id FROM baseline_apply_assets WHERE applied=1",
+        &[],
+    )?;
+    rows.iter()
+        .map(|row| array32(row, 0, "baseline_apply_assets.object_id"))
+        .collect()
+}
+
+fn corrupt_expected(reason: &str) -> StorageError {
+    StorageError::Corrupt {
+        context: format!("meta {META_BASELINE_EXPECTED} 非法：{reason}"),
+    }
+}
+
+fn take_expected_bytes<'a>(
+    bytes: &'a [u8],
+    offset: &mut usize,
+    len: usize,
+) -> Result<&'a [u8], StorageError> {
+    let end = offset
+        .checked_add(len)
+        .ok_or_else(|| corrupt_expected("长度溢出"))?;
+    let slice = bytes
+        .get(*offset..end)
+        .ok_or_else(|| corrupt_expected("长度越界"))?;
+    *offset = end;
+    Ok(slice)
+}
+
+fn read_expected_u64(bytes: &[u8], offset: &mut usize) -> Result<u64, StorageError> {
+    let raw = take_expected_bytes(bytes, offset, 8)?;
+    let mut value = [0u8; 8];
+    value.copy_from_slice(raw);
+    Ok(u64::from_be_bytes(value))
+}
+
 fn load_active_journal(vault: &Vault) -> Result<ApplyJournal, StorageError> {
     let journal = load_journal(vault)?.ok_or_else(|| StorageError::SyncConflict {
         context: "没有 active baseline apply".to_owned(),
@@ -1185,7 +1492,22 @@ fn load_journal(vault: &Vault) -> Result<Option<ApplyJournal>, StorageError> {
     }))
 }
 
-fn state_from_journal(journal: &ApplyJournal) -> Result<BaselineApplyState, StorageError> {
+fn state_from_journal(
+    vault: &Vault,
+    journal: &ApplyJournal,
+) -> Result<BaselineApplyState, StorageError> {
+    let expected = load_expected_set(vault)?;
+    let (expected_docs, expected_assets) = match &expected {
+        Some(expected) => (Some(expected.doc_count()), Some(expected.asset_count())),
+        None => (None, None),
+    };
+    // finishable 必须反映**真实就绪**：仅 status=staging 不够，还要 begin 记录的预期
+    // 集合全部 applied=1（否则 finish 会 Conflict，不能对 UI/续跑逻辑谎报可完成）。
+    let finishable = journal.status == BaselineApplyStatus::Staging
+        && match &expected {
+            Some(expected) => expected_is_complete(vault, journal, expected)?,
+            None => false,
+        };
     Ok(BaselineApplyState {
         checkpoint_id: journal.checkpoint_id,
         baseline_s: journal.baseline_s,
@@ -1194,7 +1516,9 @@ fn state_from_journal(journal: &ApplyJournal) -> Result<BaselineApplyState, Stor
         applied_docs: journal.applied_docs,
         applied_assets: journal.applied_assets,
         kept_local_edit: journal.kept_local_edit,
-        finishable: journal.status == BaselineApplyStatus::Staging,
+        expected_docs,
+        expected_assets,
+        finishable,
         chosen_cursor: journal.chosen_cursor.clone(),
     })
 }
diff --git a/crates/storage/src/error.rs b/crates/storage/src/error.rs
index 569c8d1..02c8aaa 100644
--- a/crates/storage/src/error.rs
+++ b/crates/storage/src/error.rs
@@ -103,6 +103,34 @@ pub enum StorageError {
         actual: u64,
     },
 
+    /// 未 ACK journal 的 CAS 基准已过期：head 被别的 writer 推进了。
+    ///
+    /// journal 条目的 `PendingUpdate` 是**追加那一刻**按当时 head 导出的字节
+    /// （`end_frontier` / `end_vv` 都是那次导出的快照），**不可重算**。head 一旦前进，
+    /// 同一份字节写一万次也只会再得到一次 CAS 冲突 —— 所以这是**终态**错误，
+    /// 由 [`StorageError::requires_session_rebuild`] 显式标记：
+    ///
+    /// - **谁负责重建**：持有 live `BodyDoc` 的会话层（`session::JournalHandle` /
+    ///   `SessionDriver`）。它必须先把内存文档重放到新 head，再按新 head 重新导出增量，
+    ///   并用新 `head_seq` 造 sink（见 `crate::journal` 模块头的契约）；
+    /// - **旧条目**：不会被写入，也**不会**被静默丢弃 —— `JournalWriter::rebuild_sink`
+    ///   把它们原样交还给调用方；
+    /// - **重试**：本层自动重试次数为 0，`flush` 之后不再触碰数据库。
+    #[error(
+        "文档 {doc_id} 的未 ACK journal 基准已过期（期望 head {expected}，实际 {actual}）：\
+         {pending} 条已导出增量不可重算，必须重建 session 后重新导出；已禁止重试"
+    )]
+    JournalHeadStale {
+        /// 文档 ID。
+        doc_id: String,
+        /// 写入时使用的 CAS 基准（`loro_docs.head_seq`）。
+        expected: u64,
+        /// 库里实际看到的 head。
+        actual: u64,
+        /// 过期时 journal 里未 durable 的条数。
+        pending: usize,
+    },
+
     /// 未 ACK journal 已满：必须先 flush 再重试（背压，不允许无界增长）。
     #[error("未 ACK journal 已满（容量 {capacity} 条），拒绝新写入；请先 flush")]
     WriteBackpressure {
@@ -175,3 +203,31 @@ pub enum StorageError {
         context: String,
     },
 }
+
+impl StorageError {
+    /// 是否必须**重建 session**（而不是重试）。
+    ///
+    /// 目前只有 [`Self::JournalHeadStale`]：那批 `PendingUpdate` 是按旧 head 导出的字节，
+    /// 没有任何 rebase 可能，原样重试永远不会成功。调用方必须先重建 session、重新导出
+    /// 增量（见 `crate::journal` 模块头），否则只会把失败换个地方再报一次。
+    #[must_use]
+    pub fn requires_session_rebuild(&self) -> bool {
+        matches!(self, Self::JournalHeadStale { .. })
+    }
+
+    /// 是否值得**原样重试**（只表示"条件变化后可能成功"，不表示"一定要重试"）。
+    ///
+    /// - [`Self::HeadConflict`]：store 层 CAS 冲突 —— 重读 head 后重放（sync engine 语义，
+    ///   与 journal 层的[`Self::JournalHeadStale`]不同：那里的字节可以重新导出）；
+    /// - [`Self::SnapshotBusy`]：读锁超时 —— 退避后重开快照；
+    /// - [`Self::WriteBackpressure`]：journal 满 —— 先 flush 再重试。
+    ///
+    /// 其余变体一律 `false`：盲目重试只会挂死，或把真正的损坏/配置错误掩盖成"暂时失败"。
+    #[must_use]
+    pub fn is_retryable(&self) -> bool {
+        matches!(
+            self,
+            Self::HeadConflict { .. } | Self::SnapshotBusy { .. } | Self::WriteBackpressure { .. }
+        )
+    }
+}
diff --git a/crates/storage/src/journal.rs b/crates/storage/src/journal.rs
index 4aea6e6..f2d36cb 100644
--- a/crates/storage/src/journal.rs
+++ b/crates/storage/src/journal.rs
@@ -12,12 +12,46 @@
 //!
 //! 崩溃语义：sink 成功 = 已 durable；未 flush 的条目只在内存里，重启后自然丢掉。
 //! 因此「已 ACK 的不丢、未 ACK 的只丢连续前缀之后」由本模块与 [`crate::store`] 的 A 事务共同保证。
+//!
+//! # head 过期：可重试 vs 必须重建 session（P1-3 契约）
+//!
+//! journal 条目里的 `PendingUpdate` 是**追加那一刻**按当时 head 导出的字节
+//! （`end_frontier` / `end_vv` 是那次导出的快照），**不可重算**。如果同一文档的 head
+//! 被别的 writer 推进（同步引擎 integrate、`SessionDriver` 的 adopt、另一个 Vault
+//! 进程……），前缀的 CAS 基准（`loro_docs.head_seq`）就永远对不上：`append_updates`
+//! 每次都返回 [`StorageError::HeadConflict`]，但**同一份字节重试多少次都不会成功**。
+//! 这正是旧实现「flush 一直 HeadConflict、水位永远不前进」的卡死来源。
+//!
+//! 本模块用一条明确的二分法代替无限重试：
+//!
+//! - [`JournalWriter::flush`] 捕获 sink 的 [`StorageError::HeadConflict`]，**不重试**
+//!   （0 次），立即转成终态错误 [`StorageError::JournalHeadStale`]
+//!   （`doc_id` / `expected` / `actual` / `pending` 齐全，
+//!   [`StorageError::requires_session_rebuild`] 为 `true`）。其它错误（磁盘失败等）
+//!   原样透传 —— 那才是可重试的，journal 原样保留。
+//! - 进入 stale 之后，`flush` / `flush_all` **不再触碰数据库**，幂等地返回同一个
+//!   stale 错误；`submit` 也返回 `JournalHeadStale`（writer 已冻结，新条目必须交给
+//!   重建后的 writer）。因此调用方无论怎么轮询，都不会挂死、也不会刷屏重试。
+//! - **谁负责重建**：持有 live `BodyDoc` 的会话层（`session::JournalHandle` /
+//!   `SessionDriver`，实现在 `crate::session::flush`）。重建顺序：
+//!   ① 先停外部写并按 ADR 0012 对账（`HeadConflict` 先停外部写、验证授权、再对账，
+//!   然后才允许有界重放）；② 把 durable journal 重放到**新 head**（`load_body_doc` /
+//!   `load_head`），让内存文档与库一致；③ 用新 head 的 VV 重新导出本地意图
+//!   （`BodyDoc::pending_update`）——只有这一步得到的字节才能写；④ 用新 `head_seq`
+//!   造一个 [`VaultJournalSink`]，调用 [`JournalWriter::rebuild_sink`] 装回去。
+//! - **未 durable 的旧条目怎么办**：`rebuild_sink` 把它们**原样交还**调用方（返回值）
+//!   并从 writer 的 journal 里清空。它们**没有被写入、也没有被静默丢弃** —— 调用方
+//!   必须用 ③ 重新导出的增量覆盖这段内容；若调用方决定不要，也必须显式消费返回值
+//!   并在上层报告，不能假装成功。重建后 `durable_seq` 等于新 sink 的 CAS 基准。
+//! - **有界重试规则**：CAS 冲突在 journal 层的自动重试是 **0** 次（基准是常量，
+//!   重试不可能成功）；需要重放时由 session 层按 ADR 0012 做**有界**重放（一次），
+//!   再失败就上报告需要人工处理。本模块不提供、也不会退化成无限重试。
 
 use std::collections::VecDeque;
 
 use crate::body_doc::PendingUpdate;
 use crate::error::StorageError;
-use crate::store::UpdateRecord;
+use crate::store::{OriginMarks, UpdateRecord};
 use crate::sync_bridge::{self, SyncContext, SyncIntent};
 use crate::Vault;
 
@@ -161,6 +195,14 @@ impl PendingJournal {
         }
         Ok(())
     }
+
+    /// 取走全部未 ACK 条目。
+    ///
+    /// **只**供 [`JournalWriter::rebuild_sink`] 在 head 过期后显式交还给调用方使用：
+    /// 正常路径一律走 [`Self::ack_prefix`]，这里既不写库也不推进任何水位。
+    fn drain_all(&mut self) -> Vec<JournalEntry> {
+        self.entries.drain(..).collect()
+    }
 }
 
 /// durable 落点（生产实现见 [`VaultJournalSink`]，测试可注入慢盘/失败实现）。
@@ -168,7 +210,16 @@ pub trait DurableSink {
     /// 把一段**连续前缀**写 durable；成功返回这段里最后一条的 `seq`。
     ///
     /// 实现必须是原子的：要么整段 durable，要么什么都没写（否则会丢已 ACK 数据）。
+    ///
+    /// CAS 基准冲突必须返回 [`StorageError::HeadConflict`]（不能自己改基准硬写）：
+    /// [`JournalWriter`] 据此把 writer 判成 head 过期，而不是无限重试同一份字节。
     fn write_prefix(&mut self, entries: &[JournalEntry]) -> Result<u64, StorageError>;
+
+    /// 当前 sink 使用的 CAS 基准（= 它认为已 durable 的 `head_seq`）。
+    ///
+    /// writer 用它给 `durable_seq` 定初值，并在 [`JournalWriter::rebuild_sink`] 之后重新对齐
+    /// 水位。实现必须返回**真正用于 CAS 的**那个值，否则重建后的水位会骗人。
+    fn head_seq(&self) -> u64;
 }
 
 /// 一次 flush 的结果。
@@ -180,6 +231,28 @@ pub struct FlushReport {
     pub durable_seq: u64,
 }
 
+/// head 过期的终态记录（见模块头契约）。
+///
+/// 只要它非空，writer 就冻结：`submit` / `flush` 一律返回同一个
+/// [`StorageError::JournalHeadStale`]，直到调用方 [`JournalWriter::rebuild_sink`]。
+#[derive(Debug, Clone, PartialEq, Eq)]
+struct StaleHead {
+    doc_id: String,
+    expected: u64,
+    actual: u64,
+}
+
+impl StaleHead {
+    fn to_error(&self, pending: usize) -> StorageError {
+        StorageError::JournalHeadStale {
+            doc_id: self.doc_id.clone(),
+            expected: self.expected,
+            actual: self.actual,
+            pending,
+        }
+    }
+}
+
 /// journal writer：持有 journal 与 sink，串行 flush。
 #[derive(Debug)]
 pub struct JournalWriter<S: DurableSink> {
@@ -188,29 +261,52 @@ pub struct JournalWriter<S: DurableSink> {
     max_batch: usize,
     durable_seq: u64,
     flushed_entries: u64,
+    /// 非空 = CAS 基准已过期，writer 已冻结（见模块头契约）。
+    stale: Option<StaleHead>,
 }
 
 impl<S: DurableSink> JournalWriter<S> {
     /// `max_batch` 是一次 flush 最多合并多少条（慢 I/O 时 pending 会堆到这里为止）。
+    ///
+    /// `durable_seq` 初值取 sink 的 CAS 基准（[`DurableSink::head_seq`]），
+    /// 因此空 journal 上 flush 报告的是真实水位，而不是 0。
     pub fn new(sink: S, capacity: usize, max_batch: usize) -> Self {
+        let durable_seq = sink.head_seq();
         Self {
             journal: PendingJournal::new(capacity),
             sink,
             max_batch: max_batch.max(1),
-            durable_seq: 0,
+            durable_seq,
             flushed_entries: 0,
+            stale: None,
         }
     }
 
     /// 追加一条待落库 update；满了就是背压，调用方必须 flush 后重试。
+    ///
+    /// head 过期后拒绝（返回 [`StorageError::JournalHeadStale`]）：writer 已冻结，
+    /// 新条目必须交给重建后的 writer，否则只会堆在一条永远写不动的前缀后面。
     pub fn submit(&mut self, entry: JournalEntry) -> Result<u64, StorageError> {
+        if let Some(stale) = self.stale.as_ref() {
+            return Err(stale.to_error(self.journal.pending_len()));
+        }
         self.journal.append(entry)
     }
 
     /// 把当前前缀合并成一次 sink 写；成功后裁剪 journal 并推进水位。
     ///
     /// 失败时 journal **原样保留**（sink 承诺原子），下次重试不会丢已成功部分。
+    ///
+    /// 唯一的例外是 CAS 冲突：sink 返回 [`StorageError::HeadConflict`] 时前缀字节已不可
+    /// 重放，本方法**不重试**（0 次），标记 stale 并返回
+    /// [`StorageError::JournalHeadStale`] —— 调用方必须按模块头的契约重建 session，
+    /// 而不是再 flush 一次。
     pub fn flush(&mut self) -> Result<FlushReport, StorageError> {
+        // 终态优先：stale 之后连数据库都不碰（有界重试 = 0 次）。
+        if let Some(stale) = self.stale.as_ref() {
+            return Err(stale.to_error(self.journal.pending_len()));
+        }
+
         // 先拷出前缀，避免同时借用 journal 与 sink
         let prefix: Vec<JournalEntry> = self.journal.peek_prefix(self.max_batch).to_vec();
         if prefix.is_empty() {
@@ -221,7 +317,27 @@ impl<S: DurableSink> JournalWriter<S> {
         }
 
         let entries = prefix.len();
-        let durable_seq = self.sink.write_prefix(&prefix)?;
+        let durable_seq = match self.sink.write_prefix(&prefix) {
+            Ok(seq) => seq,
+            // CAS 冲突是**终态**：前缀的 end_vv/end_frontier 是按旧 head 导出的快照，
+            // 重新导出只能由持有 live 文档的会话层做；同一份字节重试不可能成功。
+            Err(StorageError::HeadConflict {
+                doc_id,
+                expected,
+                actual,
+            }) => {
+                let stale = StaleHead {
+                    doc_id,
+                    expected,
+                    actual,
+                };
+                let error = stale.to_error(self.journal.pending_len());
+                self.stale = Some(stale);
+                return Err(error);
+            }
+            // 其它错误（磁盘失败等）原样透传：journal 原样保留，同一前缀可以重试。
+            Err(error) => return Err(error),
+        };
         self.journal.ack_prefix(entries)?;
         self.durable_seq = durable_seq;
         self.flushed_entries += entries as u64;
@@ -233,6 +349,8 @@ impl<S: DurableSink> JournalWriter<S> {
     }
 
     /// flush 直到 journal 空或没有进展（一次性排空，供关闭/手动保存用）。
+    ///
+    /// head 过期时第一拍就返回 [`StorageError::JournalHeadStale`]，不会空转。
     pub fn flush_all(&mut self) -> Result<usize, StorageError> {
         let mut total = 0;
         while self.journal.pending_len() > 0 {
@@ -245,6 +363,35 @@ impl<S: DurableSink> JournalWriter<S> {
         Ok(total)
     }
 
+    /// writer 是否已进入「head 过期、必须重建」的终态。
+    pub fn is_stale(&self) -> bool {
+        self.stale.is_some()
+    }
+
+    /// 重建 sink：head 过期后**唯一**合法的恢复入口（见模块头契约）。
+    ///
+    /// 调用方必须先用新 head 重放 live 文档并重新导出本地意图；本方法只负责把 writer
+    /// 从 stale 拉回可用状态：
+    ///
+    /// - 把旧 journal 里未写入的条目**原样交还**（返回值），并从 journal 里清空 ——
+    ///   它们不写库、也**不被静默丢弃**；调用方要么用重新导出的增量覆盖其内容，
+    ///   要么显式报告这次丢弃；
+    /// - `durable_seq` 对齐新 sink 的 CAS 基准（[`DurableSink::head_seq`]）；
+    /// - 未处于 stale 时拒绝：还有能写的前缀就换 sink，等于把本可落库的条目丢掉。
+    pub fn rebuild_sink(&mut self, sink: S) -> Result<Vec<JournalEntry>, StorageError> {
+        if self.stale.is_none() {
+            return Err(StorageError::Corrupt {
+                context: "journal 未处于 head 过期状态，拒绝重建 sink（会丢弃仍可写入的条目）"
+                    .to_owned(),
+            });
+        }
+        let handed_back = self.journal.drain_all();
+        self.sink = sink;
+        self.stale = None;
+        self.durable_seq = self.sink.head_seq();
+        Ok(handed_back)
+    }
+
     /// 未 ACK 条数。
     pub fn pending_len(&self) -> usize {
         self.journal.pending_len()
@@ -343,7 +490,7 @@ impl DurableSink for VaultJournalSink {
                 self.head_seq,
                 &records,
                 &[],
-                &remote_marks,
+                OriginMarks::PerRecord(&remote_marks),
             )?,
             Some(ctx) => {
                 // 一 update 一意图（remote 条目为空位）；seq 必须与 A 事务写进
@@ -374,11 +521,16 @@ impl DurableSink for VaultJournalSink {
                     self.head_seq,
                     &records,
                     &slots,
-                    &remote_marks,
+                    OriginMarks::PerRecord(&remote_marks),
                 )?
             }
         };
         self.head_seq = head.head_seq;
         Ok(head.head_seq)
     }
+
+    /// CAS 基准 = 本 sink 已 durable 的 head_seq（与固有方法 `head_seq` 同一个值）。
+    fn head_seq(&self) -> u64 {
+        self.head_seq
+    }
 }
diff --git a/crates/storage/src/lib.rs b/crates/storage/src/lib.rs
index 884e5eb..f591603 100644
--- a/crates/storage/src/lib.rs
+++ b/crates/storage/src/lib.rs
@@ -72,7 +72,8 @@ pub use baseline_apply::{
     baseline_apply_debug_counters, baseline_apply_debug_timings, baseline_apply_state,
     begin_baseline_apply, finish_baseline_apply, reset_baseline_apply_debug,
     BaselineApplyDebugCounters, BaselineApplyDebugTimings, BaselineApplyFinish, BaselineApplyState,
-    BaselineApplyStatus, BaselineAsset, BASELINE_MERGE_BATCH, META_SYNC_RELAY_CURSOR,
+    BaselineApplyStatus, BaselineAsset, BaselineExpectation, BASELINE_MERGE_BATCH,
+    META_BASELINE_EXPECTED, META_SYNC_RELAY_CURSOR,
 };
 pub use body_doc::{
     BodyDoc, PendingUpdate, APP_DOC_SCHEMA, BODY_CONTAINER, CODEC_VERSION, MIN_READER,
diff --git a/crates/storage/src/remote_known.rs b/crates/storage/src/remote_known.rs
index 467b080..3f9bbf2 100644
--- a/crates/storage/src/remote_known.rs
+++ b/crates/storage/src/remote_known.rs
@@ -24,6 +24,17 @@
 //! 本地写入**不会**扩大它——即使已经 ACK 上传也不扩大。这是刻意的保守取舍：
 //! tombstone 判定只会更倾向于 `kept_local_edit`，绝不会把本地未确证同步过的内容删掉。
 //!
+//! ## 单调性怎么强制（P2-5）
+//!
+//! 「只读旧值、在事务外算并集、再无条件覆盖」有两个写者互相覆盖的窗口（普通 pull 的
+//! A 事务与 baseline apply 批次都可能写同一个键）。因此所有写入都改成**值 CAS**：
+//!
+//! - 生成语句时带上读到的旧值 `expected_old`，SQL 是
+//!   `INSERT ... ON CONFLICT(key) DO UPDATE SET value=excluded.value WHERE meta.value=?old`；
+//!   旧值不存在时用 `INSERT ... SELECT ... WHERE NOT EXISTS`；
+//! - 语句带 `AffectedGuard::exactly(index, 1)`：CAS 不命中就是 0 行 → 整笔事务回滚，
+//!   调用方拿到明确的冲突错误（重读后重试即收敛到并集），**绝不会写入更小的值**。
+//!
 //! ## 判定
 //!
 //! `local_vv.sub_iter(remote_known).next().is_none()` 表示本地没有任何远端未知的 op，
@@ -56,7 +67,7 @@ pub(crate) fn load(vault: &Vault, doc_id: &str) -> Result<Option<VersionVector>,
     };
     match row.first() {
         Some(Value::Blob(bytes)) => Ok(Some(decode(bytes)?)),
-        Some(Value::Null) | None => Ok(None),
+        // meta 值的统一口径（P2-6）：只认 BLOB；NULL/INTEGER/TEXT 都视为库被改坏。
         other => Err(StorageError::Corrupt {
             context: format!("{key} 读回异常：{other:?}"),
         }),
@@ -99,18 +110,59 @@ pub(crate) fn added_since(after: &VersionVector, before: &VersionVector) -> Vers
     added
 }
 
-/// 纯 SQL 语句：把某篇文档的远端已知 VV 覆盖为 `merged`（调用方负责事务与单调并集）。
-pub(crate) fn upsert_statement(doc_id: &str, merged: &VersionVector) -> Statement {
-    Statement::new(
-        "INSERT INTO meta(key,value) VALUES(?1,?2) \
-         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
-        vec![Value::Text(meta_key(doc_id)), Value::Blob(merged.encode())],
-    )
+/// 生成一条**值 CAS** upsert 语句（调用方负责把它放进 A 事务，并加
+/// `AffectedGuard::exactly(语句下标, 1)`）。
+///
+/// - `expected = Some(old)`：`INSERT ... ON CONFLICT(key) DO UPDATE ... WHERE meta.value=?old`。
+///   只有库中值仍是读到的旧值时才写入；被并发写者改过就是 0 行 → 守卫失败 → 整笔回滚；
+/// - `expected = None`：`INSERT ... SELECT ... WHERE NOT EXISTS`。只有键仍不存在时才插入；
+///   并发插入过就是 0 行 → 同样回滚。
+///
+/// 两种写法都不会用旧并集覆盖（更小的）并发结果，因此值只能单调扩大。
+pub(crate) fn cas_statement(
+    doc_id: &str,
+    expected: Option<&VersionVector>,
+    merged: &VersionVector,
+) -> Statement {
+    assert_monotone(expected, merged);
+    let key = meta_key(doc_id);
+    let encoded = merged.encode();
+    match expected {
+        Some(old) => Statement::new(
+            "INSERT INTO meta(key,value) VALUES(?1,?2) \
+             ON CONFLICT(key) DO UPDATE SET value=excluded.value \
+             WHERE meta.value=?3",
+            vec![
+                Value::Text(key),
+                Value::Blob(encoded),
+                Value::Blob(old.encode()),
+            ],
+        ),
+        None => Statement::new(
+            "INSERT INTO meta(key,value) SELECT ?1,?2 \
+             WHERE NOT EXISTS(SELECT 1 FROM meta WHERE key=?1)",
+            vec![Value::Text(key), Value::Blob(encoded)],
+        ),
+    }
+}
+
+/// 调试断言：CAS 的目标值必须包含读到的旧值（`old ⊆ merged`）。
+///
+/// 生产路径由 [`union`] 构造 merged，这里只是把「只能扩大」的契约钉在生成语句处；
+/// 一旦将来有人改成直接写入更小的值，debug/test 构建会立刻失败。
+fn assert_monotone(expected: Option<&VersionVector>, merged: &VersionVector) {
+    if let Some(old) = expected {
+        debug_assert!(
+            old.sub_iter(merged).next().is_none(),
+            "remote known VV 只能单调扩大：旧值必须是 merged 的子集"
+        );
+    }
 }
 
-/// 在 A 事务**之外**读旧值并返回合并后的 upsert 语句；`added` 为空时返回 `None`。
+/// 在 A 事务**之外**读旧值并返回合并后的 **CAS** 语句；`added` 为空时返回 `None`。
 ///
-/// 事务内的单调性由「先读后写 + 单写者」保证：所有写都经 Vault actor 串行执行。
+/// 旧值只作为 CAS 的期望值；若事务执行时值已被并发写者扩大，语句影响 0 行、守卫失败，
+/// 调用方必须重读（重新调用本函数）后重试。这里**不会**用读到的旧并集覆盖库里的新值。
 pub(crate) fn merge_statement(
     vault: &Vault,
     doc_id: &str,
@@ -119,9 +171,166 @@ pub(crate) fn merge_statement(
     if added.is_empty() {
         return Ok(None);
     }
-    let merged = match load(vault, doc_id)? {
-        Some(existing) => union(&existing, added),
+    let existing = load(vault, doc_id)?;
+    let merged = match &existing {
+        Some(existing) => union(existing, added),
         None => added.clone(),
     };
-    Ok(Some(upsert_statement(doc_id, &merged)))
+    Ok(Some(cas_statement(doc_id, existing.as_ref(), &merged)))
+}
+
+#[cfg(test)]
+mod tests {
+    use std::path::Path;
+    use std::sync::mpsc;
+    use std::sync::{Arc, Barrier};
+    use std::thread;
+
+    use super::*;
+    use crate::actor::AffectedGuard;
+
+    /// 临时 vault：目录建在 `target/tmp` 下（AGENTS §6：不用 `%TEMP%`）。
+    ///
+    /// lib 单元测试拿不到 `CARGO_TARGET_TMPDIR`（只有集成测试有），所以从
+    /// `CARGO_MANIFEST_DIR` 拼出同一个位置。
+    fn fresh_vault(prefix: &str) -> (tempfile::TempDir, Vault) {
+        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
+            .join("../../target/tmp")
+            .join(format!("remote-known-{prefix}"));
+        std::fs::create_dir_all(&root).expect("创建 target/tmp 下的测试根目录");
+        let dir = tempfile::Builder::new()
+            .prefix("vault-")
+            .tempdir_in(&root)
+            .expect("创建临时目录");
+        let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
+        (dir, vault)
+    }
+
+    fn vv(peer: u64, counter: i32) -> VersionVector {
+        let mut vv = VersionVector::new();
+        vv.insert(peer, counter);
+        vv
+    }
+
+    fn read(vault: &Vault, doc_id: &str) -> Option<VersionVector> {
+        load(vault, doc_id).expect("读取远端已知 VV")
+    }
+
+    fn exec_cas(
+        vault: &Vault,
+        doc_id: &str,
+        expected: Option<&VersionVector>,
+        merged: &VersionVector,
+    ) -> Result<(), StorageError> {
+        vault
+            .exec_transaction(
+                &[cas_statement(doc_id, expected, merged)],
+                &[AffectedGuard::exactly(0, 1)],
+            )
+            .map(|_| ())
+    }
+
+    /// 值 CAS 的三种状态：不存在时插入 → 陈旧期望被拒绝且不缩小 → 读到新值后并集成功。
+    #[test]
+    fn cas_statement_rejects_stale_expected_and_never_shrinks() {
+        let (_dir, vault) = fresh_vault("stale");
+        let doc_id = "doc-cas";
+        let a = vv(0xA1, 2);
+        let b = vv(0xB2, 3);
+
+        // 读时不存在：直接插入。
+        exec_cas(&vault, doc_id, None, &a).expect("首次插入");
+        assert_eq!(read(&vault, doc_id), Some(a.clone()));
+
+        // 另一个写者仍以为「不存在」，拿着旧的 None 来写 B：必须被 CAS 拒绝。
+        let error = exec_cas(&vault, doc_id, None, &b).expect_err("陈旧期望必须失败");
+        assert!(
+            matches!(error, StorageError::GuardFailed { .. }),
+            "CAS 不命中应当是 GuardFailed（调用方映射成明确冲突），实际 {error:?}"
+        );
+        assert_eq!(
+            read(&vault, doc_id),
+            Some(a.clone()),
+            "CAS 失败绝不能把已有的 A 覆盖成只有 B"
+        );
+
+        // 重读后按并集重试：成功且值单调扩大。
+        let current = read(&vault, doc_id).expect("已存在");
+        let merged = union(&current, &b);
+        exec_cas(&vault, doc_id, Some(&current), &merged).expect("并集重试");
+
+        // 再拿一个更旧、且值更小的期望来写：必须继续被拒绝，值不得缩小。
+        let stale = vv(0xA1, 1);
+        let smaller = vv(0xA1, 1);
+        let error = exec_cas(&vault, doc_id, Some(&stale), &smaller).expect_err("更小值必须失败");
+        assert!(matches!(error, StorageError::GuardFailed { .. }));
+        assert_eq!(
+            read(&vault, doc_id),
+            Some(merged),
+            "失败后必须保持并集，绝不能写入更小的值"
+        );
+    }
+
+    /// 两路并发写者从**同一旧值**出发写不同 `added`：barrier 对齐后同时 CAS。
+    ///
+    /// 结果只能是：恰好一方首次成功、另一方拿到明确的 `GuardFailed`；失败方重读后按
+    /// 并集重试成功。最终值 = 两方新增的并集（不丢证据、不静默变小）。
+    #[test]
+    fn concurrent_writers_from_same_old_converge_to_union() {
+        let (_dir, vault) = fresh_vault("concurrent");
+        let doc_id = "doc-concurrent";
+        let base = vv(0x10, 1);
+        exec_cas(&vault, doc_id, None, &base).expect("初始值");
+
+        let barrier = Arc::new(Barrier::new(2));
+        let (tx, rx) = mpsc::channel();
+        let mut handles = Vec::new();
+        for peer in [0xA1_u64, 0xB2_u64] {
+            let vault = vault.clone();
+            let barrier = Arc::clone(&barrier);
+            let tx = tx.clone();
+            handles.push(thread::spawn(move || {
+                let added = vv(peer, 5);
+                // 两方都在 barrier 之前读到同一个旧值。
+                let old = load(&vault, doc_id).expect("读旧值").expect("已存在");
+                let merged = union(&old, &added);
+                let statement = cas_statement(doc_id, Some(&old), &merged);
+                barrier.wait();
+                let first = vault.exec_transaction(&[statement], &[AffectedGuard::exactly(0, 1)]);
+                // 失败方（陈旧旧值）重读后重试一次：必须成功并收敛到并集。
+                let (first_failed, retried) = match first {
+                    Ok(_) => (false, true),
+                    Err(StorageError::GuardFailed { .. }) => {
+                        let current = load(&vault, doc_id).expect("重读").expect("已存在");
+                        let merged = union(&current, &added);
+                        (
+                            true,
+                            exec_cas(&vault, doc_id, Some(&current), &merged).is_ok(),
+                        )
+                    }
+                    Err(other) => panic!("并发 CAS 只允许守卫失败，实际 {other:?}"),
+                };
+                tx.send((peer, first_failed, retried)).expect("回传结果");
+            }));
+        }
+        drop(tx);
+        let results: Vec<(u64, bool, bool)> = rx.iter().take(2).collect();
+        for handle in handles {
+            handle.join().expect("写者线程不应 panic");
+        }
+        assert_eq!(results.len(), 2, "两路写者都必须有明确结果");
+        assert_eq!(
+            results.iter().filter(|(_, failed, _)| *failed).count(),
+            1,
+            "同一旧值下只允许一方首次成功，另一方必须拿到明确的 CAS 冲突"
+        );
+        for (peer, _, retried) in &results {
+            assert!(*retried, "peer {peer:#x} 重读重试后必须成功");
+        }
+
+        let final_vv = read(&vault, doc_id).expect("最终值");
+        let expected = union(&union(&base, &vv(0xA1, 5)), &vv(0xB2, 5));
+        assert_eq!(final_vv, expected, "最终值必须包含两路 added 的并集");
+        assert_eq!(final_vv.get(&0x10).copied(), Some(1), "旧前缀不得丢失");
+    }
 }
diff --git a/crates/storage/src/store.rs b/crates/storage/src/store.rs
index 3e04674..69e1fad 100644
--- a/crates/storage/src/store.rs
+++ b/crates/storage/src/store.rs
@@ -392,7 +392,15 @@ pub fn append_updates(
     expected_head_seq: u64,
     records: &[UpdateRecord],
 ) -> Result<HeadState, StorageError> {
-    append_updates_inner(vault, doc_id, expected_head_seq, records, &[], &[], &[])
+    append_updates_inner(
+        vault,
+        doc_id,
+        expected_head_seq,
+        records,
+        &[],
+        &[],
+        OriginMarks::AllLocal,
+    )
 }
 
 /// A 事务：追加**一条**组合 update，并把同一批 remote inbox 行标为 `integrated`。
@@ -434,25 +442,85 @@ pub fn append_updates_with_integrated_marks(
         &marks,
         // engine integrate 的这条组合 update 全部来自远端消息：其 `end_vv - 旧 head_vv`
         // 就是远端实际带来的 ops，记入「远端已知 VV」（tombstone 判定用）。
-        &[true],
+        OriginMarks::PerRecord(&[true]),
     )
 }
 
+/// 一次 A 事务里每条 update 的**来源标记**。
+///
+/// 这是 P2-8 修掉的类型陷阱：旧接口用 `&[bool]` 表示来源，**空切片 = 全部本地**；
+/// 而同一次调用的 `slots` 也是切片，**空切片 = 没有任何出站意图**。两个空切片含义
+/// 相反、类型相同，调用点很容易把「没有意图」写成「全部本地」或反之。这里把来源标记
+/// 收成枚举，让「空」不再是两种含义：
+///
+/// - [`OriginMarks::AllLocal`]：本批记录全部是本地写入，**不**扩大远端已知 VV；
+/// - [`OriginMarks::PerRecord`]：与 `records` **一一对应**（长度不匹配直接报
+///   [`StorageError::Corrupt`]），`true` = 该条来自远端消息，其
+///   `end_vv - 前一条 end_vv` 并入「远端已知 VV」。
+///
+/// 本地写入即使已经 ACK 也不会扩大远端已知 VV（tombstone 判定只允许更保守）。
+#[derive(Debug, Clone, Copy)]
+pub enum OriginMarks<'a> {
+    /// 全部记录都是本地写入。
+    AllLocal,
+    /// 逐条标记（必须与 records 等长）。
+    PerRecord(&'a [bool]),
+}
+
+impl<'a> OriginMarks<'a> {
+    /// 本批是否有远端条目。
+    fn any_remote(self) -> bool {
+        match self {
+            Self::AllLocal => false,
+            Self::PerRecord(marks) => marks.iter().any(|flag| *flag),
+        }
+    }
+
+    /// 第 `index` 条是否来自远端（`AllLocal` 恒为 false）。
+    fn is_remote(self, index: usize) -> bool {
+        match self {
+            Self::AllLocal => false,
+            Self::PerRecord(marks) => marks.get(index).copied().unwrap_or(false),
+        }
+    }
+
+    /// 校验来源标记与 records 的对应关系。
+    ///
+    /// `AllLocal` 不需要长度；`PerRecord` 必须与 records 等长——**空 records 配非空
+    /// marks、或 records 配空 marks 都会在这里被拒绝**，不会退化成「全部本地」。
+    fn validate(self, records_len: usize) -> Result<(), StorageError> {
+        if let Self::PerRecord(marks) = self {
+            if marks.len() != records_len {
+                return Err(StorageError::Corrupt {
+                    context: format!(
+                        "append_updates: records={records_len} \
+                         OriginMarks::PerRecord={} 必须一对一",
+                        marks.len()
+                    ),
+                });
+            }
+        }
+        Ok(())
+    }
+}
+
 /// A 事务内同时写出站同步意图（按位置一一对应；`None` = 该条不产生意图）。
 ///
 /// 与 [`append_updates_with_sync_intents`] 相比，这里允许"某些 update 没有意图"：
 /// 一次 flush 的前缀可能混本地编辑与 remote 导入，而 remote 导入**绝不能**产生
 /// 出站意图（ADR 0009）。仍然只有**一条** A 事务，失败即整笔回滚。
 ///
-/// `remote_marks` 与 records 一一对应（空切片 = 全部本地）：标为 remote 的记录会在
-/// 同一事务里把 `end_vv - 上一条 end_vv` 并入「远端已知 VV」，供 tombstone 判定。
+/// 参数语义（P2-8 钉死）：
+/// - `slots` 空 = 本批**没有任何**出站意图（不是"每条都要一条意图"）；`None` 位 = 该条
+///   不产生意图；
+/// - `origin` 见 [`OriginMarks`]：只有远端条目会扩大远端已知 VV，本地条目永远不扩大。
 pub(crate) fn append_updates_with_intent_slots(
     vault: &Vault,
     doc_id: &str,
     expected_head_seq: u64,
     records: &[UpdateRecord],
     slots: &[Option<crate::sync_bridge::SyncIntent>],
-    remote_marks: &[bool],
+    origin: OriginMarks<'_>,
 ) -> Result<HeadState, StorageError> {
     append_updates_inner(
         vault,
@@ -461,7 +529,7 @@ pub(crate) fn append_updates_with_intent_slots(
         records,
         slots,
         &[],
-        remote_marks,
+        origin,
     )
 }
 
@@ -475,7 +543,7 @@ fn append_updates_inner(
     records: &[UpdateRecord],
     slots: &[Option<crate::sync_bridge::SyncIntent>],
     integrated_marks: IntegratedMarks<'_>,
-    remote_marks: &[bool],
+    origin: OriginMarks<'_>,
 ) -> Result<HeadState, StorageError> {
     if records.is_empty() {
         if !slots.is_empty() {
@@ -488,6 +556,8 @@ fn append_updates_inner(
                 context: "空 update 批次不能带 inbox 标记".to_owned(),
             });
         }
+        // `PerRecord` 配空 records 时，非空标记同样算长度不匹配（不会退化成全部本地）。
+        origin.validate(0)?;
         return load_head(vault, doc_id);
     }
     if !slots.is_empty() && slots.len() != records.len() {
@@ -499,29 +569,28 @@ fn append_updates_inner(
             ),
         });
     }
-    if !remote_marks.is_empty() && remote_marks.len() != records.len() {
-        return Err(StorageError::Corrupt {
-            context: format!(
-                "append_updates: records={} remote_marks={} 必须一对一",
-                records.len(),
-                remote_marks.len()
-            ),
-        });
-    }
+    origin.validate(records.len())?;
 
     // 远端已知 VV 的增量：只有 remote 条目会扩大它（`after_vv - running_vv`）。
-    let remote_meta = if remote_marks.iter().any(|flag| *flag) {
+    //
+    // 这里只算「本批新增了哪些远端 ops」；旧值读取与 CAS 语句在构造语句阶段生成，
+    // 重试时按最新旧值重算并集，但 `added` 本身不变（head 基准没变）。
+    let remote_added = if origin.any_remote() {
         let mut running = load_head_vv_at(vault, doc_id, expected_head_seq)?;
         let mut known = VersionVector::new();
         for (index, record) in records.iter().enumerate() {
             let after = VersionVector::decode(&record.update.end_vv).map_err(StorageError::from)?;
-            if remote_marks.get(index).copied().unwrap_or(false) {
+            if origin.is_remote(index) {
                 let added = crate::remote_known::added_since(&after, &running);
                 known = crate::remote_known::union(&known, &added);
             }
             running = after;
         }
-        crate::remote_known::merge_statement(vault, doc_id, &known)?
+        if known.is_empty() {
+            None
+        } else {
+            Some(known)
+        }
     } else {
         None
     };
@@ -603,11 +672,22 @@ fn append_updates_inner(
     }
 
     // 远端已知 VV 的 meta 写入（与 update/head 同一 A 事务；失败一起回滚）。
-    let remote_guard = remote_meta.as_ref().map(|statement| {
-        let index = statements.len();
-        statements.push(statement.clone());
-        AffectedGuard::exactly(index, 1)
-    });
+    //
+    // **值 CAS**：语句里带读到的旧值，只有库中值仍是旧值才会更新（`WHERE meta.value=?old`，
+    // 旧值不存在时用 `INSERT ... WHERE NOT EXISTS`）。这样两个写者（普通 pull 的 A 事务
+    // 与 baseline apply 批次）即使都拿着同一份旧并集，后到的那个也不会把先到的扩大值
+    // 覆盖回更小的并集；CAS 不命中 → 0 行 → 守卫失败 → 整笔回滚后重读重试。
+    let remote_slot = match &remote_added {
+        Some(added) => {
+            let index = statements.len();
+            statements.push(cas_statement_or_corrupt(
+                crate::remote_known::merge_statement(vault, doc_id, added)?,
+                doc_id,
+            )?);
+            Some(index)
+        }
+        None => None,
+    };
 
     // 同事务标记 inbox 行：每条都要求恰好影响 1 行（不存在 / 已 integrated /
     // 已 quarantine 都会让整笔 A 事务回滚）。标记语句放在最后，避免它们的守卫先于
@@ -625,7 +705,9 @@ fn append_updates_inner(
     }
     let mut guards = Vec::with_capacity(2 + integrated_marks.len());
     guards.push(AffectedGuard::exactly(0, 1));
-    guards.extend(remote_guard);
+    if let Some(index) = remote_slot {
+        guards.push(AffectedGuard::exactly(index, 1));
+    }
     guards.extend(
         integrated_marks
             .iter()
@@ -633,38 +715,80 @@ fn append_updates_inner(
             .map(|(index, _)| AffectedGuard::exactly(marks_start + index, 1)),
     );
 
-    match vault.exec_transaction(&statements, &guards) {
-        Ok(_) => Ok(HeadState {
-            doc_id: doc_id.to_owned(),
-            head_seq,
-            head_frontier: last.end_frontier.clone(),
-            head_vv: last.end_vv.clone(),
-        }),
-        // 第 0 条守卫 = head CAS。
-        Err(StorageError::GuardFailed { statement: 0, .. }) => {
-            let actual = load_head(vault, doc_id)
-                .map(|head| head.head_seq)
-                .unwrap_or(0);
-            Err(StorageError::HeadConflict {
-                doc_id: doc_id.to_owned(),
-                expected: expected_head_seq,
-                actual,
-            })
-        }
-        // 标记失败：update 与所有标记一起回滚；调用方必须保持 inbox verified 原样。
-        Err(StorageError::GuardFailed { statement, .. }) if statement >= marks_start => {
-            Err(StorageError::SyncConflict {
-                context: format!(
-                    "inbox 标记 integrated 失败（第 {} 个标记行不存在或当前不是 verified）；\
-                     整笔 A 事务（update+标记）已回滚",
-                    statement - marks_start
-                ),
-            })
+    // 远端已知 VV 的 CAS 失败是**可重试**的：并发写者只是先扩大了一步，重读旧值后
+    // 把两边并集再写一次即可。重试期间 head CAS 仍在 index 0：若真的有人推进了 head，
+    // 依旧按 HeadConflict 返回，不会把「基准过期」误当成 meta 冲突。
+    const REMOTE_KNOWN_CAS_ATTEMPTS: usize = 4;
+    let mut attempt = 0usize;
+    loop {
+        attempt += 1;
+        match vault.exec_transaction(&statements, &guards) {
+            Ok(_) => {
+                return Ok(HeadState {
+                    doc_id: doc_id.to_owned(),
+                    head_seq,
+                    head_frontier: last.end_frontier.clone(),
+                    head_vv: last.end_vv.clone(),
+                });
+            }
+            // 第 0 条守卫 = head CAS。
+            Err(StorageError::GuardFailed { statement: 0, .. }) => {
+                let actual = load_head(vault, doc_id)
+                    .map(|head| head.head_seq)
+                    .unwrap_or(0);
+                return Err(StorageError::HeadConflict {
+                    doc_id: doc_id.to_owned(),
+                    expected: expected_head_seq,
+                    actual,
+                });
+            }
+            // 远端已知 VV 的值 CAS 不命中：指定下标只有它这一条守卫。
+            Err(StorageError::GuardFailed { statement, .. }) if remote_slot == Some(statement) => {
+                let Some(added) = remote_added.as_ref() else {
+                    return Err(StorageError::Corrupt {
+                        context: format!("文档 {doc_id} 的远端已知 VV CAS 守卫缺少重算依据"),
+                    });
+                };
+                if attempt >= REMOTE_KNOWN_CAS_ATTEMPTS {
+                    return Err(StorageError::SyncConflict {
+                        context: format!(
+                            "文档 {doc_id} 的远端已知 VV 值 CAS 连续 {attempt} 次被并发写者改写；\
+                             整笔 A 事务（update+head+meta）已回滚，可重试"
+                        ),
+                    });
+                }
+                let slot = remote_slot.expect("守卫命中说明 remote_slot 存在");
+                statements[slot] = cas_statement_or_corrupt(
+                    crate::remote_known::merge_statement(vault, doc_id, added)?,
+                    doc_id,
+                )?;
+            }
+            // 标记失败：update 与所有标记一起回滚；调用方必须保持 inbox verified 原样。
+            Err(StorageError::GuardFailed { statement, .. }) if statement >= marks_start => {
+                return Err(StorageError::SyncConflict {
+                    context: format!(
+                        "inbox 标记 integrated 失败（第 {} 个标记行不存在或当前不是 verified）；\
+                         整笔 A 事务（update+标记）已回滚",
+                        statement - marks_start
+                    ),
+                });
+            }
+            Err(error) => return Err(error),
         }
-        Err(error) => Err(error),
     }
 }
 
+/// [`crate::remote_known::merge_statement`] 在 `remote_added` 非空时必然返回语句；
+/// 这里把它转成带文档上下文的损坏错误，避免业务路径 `unwrap`。
+fn cas_statement_or_corrupt(
+    statement: Option<Statement>,
+    doc_id: &str,
+) -> Result<Statement, StorageError> {
+    statement.ok_or_else(|| StorageError::Corrupt {
+        context: format!("文档 {doc_id} 的远端已知 VV 增量非空，但没有生成 CAS 语句"),
+    })
+}
+
 /// 笔记当前的物化水位（供 B 事务与测试读取）。
 #[derive(Debug, Clone, PartialEq, Eq)]
 pub struct BodyState {
@@ -743,3 +867,242 @@ fn optional_blob(row: &[Value], index: usize) -> Result<Option<Vec<u8>>, Storage
         }),
     }
 }
+
+#[cfg(test)]
+mod tests {
+    use std::path::Path;
+
+    use loro::VersionVector;
+
+    use super::*;
+    use crate::body_doc::BodyDoc;
+    use crate::remote_known;
+
+    /// 临时 vault：目录建在 `target/tmp` 下（AGENTS §6：不用 `%TEMP%`）。
+    ///
+    /// lib 单元测试拿不到 `CARGO_TARGET_TMPDIR`（只有集成测试有），所以从
+    /// `CARGO_MANIFEST_DIR` 拼出同一个位置。
+    fn fresh_vault(prefix: &str) -> (tempfile::TempDir, Vault) {
+        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
+            .join("../../target/tmp")
+            .join(format!("store-origin-{prefix}"));
+        std::fs::create_dir_all(&root).expect("创建 target/tmp 下的测试根目录");
+        let dir = tempfile::Builder::new()
+            .prefix("vault-")
+            .tempdir_in(&root)
+            .expect("创建临时目录");
+        let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
+        (dir, vault)
+    }
+
+    fn scalar_i64(vault: &Vault, sql: &str) -> i64 {
+        let rows = vault.query(sql, &[]).expect("查询");
+        match rows.first().and_then(|row| row.first()) {
+            Some(Value::Integer(value)) => *value,
+            other => panic!("期望整数结果，实际 {other:?}"),
+        }
+    }
+
+    fn local_update(doc: &BodyDoc, base: &VersionVector) -> PendingUpdate {
+        doc.pending_update(base).expect("导出增量")
+    }
+
+    /// `OriginMarks::AllLocal`（显式类型）不扩大远端已知 VV，也不产生 outbox。
+    #[test]
+    fn all_local_origin_does_not_extend_remote_known() {
+        let (_dir, vault) = fresh_vault("all-local");
+        create_body_doc(&vault, "doc-local").expect("loro_docs 行");
+        let doc = BodyDoc::create("doc-local", 0xA1).expect("创建 BodyDoc");
+        doc.insert_scalars(0, "local").expect("本地编辑");
+        let update = local_update(&doc, &VersionVector::default());
+
+        let head = append_updates_inner(
+            &vault,
+            "doc-local",
+            0,
+            &[UpdateRecord::new("b0", update)],
+            &[],
+            &[],
+            OriginMarks::AllLocal,
+        )
+        .expect("A 事务");
+
+        assert_eq!(head.head_seq, 1);
+        assert!(
+            remote_known::load(&vault, "doc-local")
+                .expect("读远端已知")
+                .is_none(),
+            "全部本地的批次绝不能创建/扩大远端已知 VV"
+        );
+        assert_eq!(
+            scalar_i64(&vault, "SELECT count(*) FROM sync_outbox"),
+            0,
+            "slots 空 = 本批没有任何出站意图"
+        );
+    }
+
+    /// `PerRecord` 混排：只有 remote 条目的 `after - running` 记入远端已知 VV。
+    #[test]
+    fn per_record_origin_only_extends_for_remote_records() {
+        let (_dir, vault) = fresh_vault("mixed");
+        create_body_doc(&vault, "doc-mixed").expect("loro_docs 行");
+
+        let doc = BodyDoc::create("doc-mixed", 0xA1).expect("创建 BodyDoc");
+        doc.insert_scalars(0, "local").expect("本地编辑");
+        let local = local_update(&doc, &VersionVector::default());
+        let remote_doc = BodyDoc::create("doc-mixed", 0xB2).expect("创建 BodyDoc");
+        remote_doc.insert_scalars(0, "remote").expect("远端编辑");
+        doc.import(&remote_doc.export_all_updates().expect("远端 update"))
+            .expect("本地副本导入远端");
+        let local_end = VersionVector::decode(&local.end_vv).expect("本地 VV");
+        let remote = local_update(&doc, &local_end);
+        let remote_end = VersionVector::decode(&remote.end_vv).expect("远端 VV");
+        let expected = remote_known::added_since(&remote_end, &local_end);
+
+        let head = append_updates_inner(
+            &vault,
+            "doc-mixed",
+            0,
+            &[
+                UpdateRecord::new("b0", local),
+                UpdateRecord::new("b1", remote),
+            ],
+            &[],
+            &[],
+            OriginMarks::PerRecord(&[false, true]),
+        )
+        .expect("A 事务");
+
+        assert_eq!(head.head_seq, 2, "两条记录都必须落库");
+        let known = remote_known::load(&vault, "doc-mixed")
+            .expect("读远端已知")
+            .expect("有 remote 条目就必须写 meta");
+        assert_eq!(
+            known, expected,
+            "远端条目带来的 ops 必须记入，且只记入远端那一小段"
+        );
+        assert_eq!(
+            known.get(&0xA1).copied().unwrap_or(0),
+            0,
+            "本地条目（哪怕同批先落库）绝不能记入远端已知 VV"
+        );
+        assert_eq!(
+            scalar_i64(&vault, "SELECT count(*) FROM sync_outbox"),
+            0,
+            "没有传 slots 就不得写出站意图"
+        );
+    }
+
+    /// 空 `PerRecord` 不再表示「全部本地」：长度不匹配直接拒绝，且不写任何行。
+    #[test]
+    fn per_record_origin_length_mismatch_is_rejected() {
+        let (_dir, vault) = fresh_vault("mismatch");
+        create_body_doc(&vault, "doc-mismatch").expect("loro_docs 行");
+        let doc = BodyDoc::create("doc-mismatch", 0xA1).expect("创建 BodyDoc");
+        doc.insert_scalars(0, "x").expect("编辑");
+        let update = local_update(&doc, &VersionVector::default());
+        let record = UpdateRecord::new("b0", update);
+
+        let error = append_updates_inner(
+            &vault,
+            "doc-mismatch",
+            0,
+            std::slice::from_ref(&record),
+            &[],
+            &[],
+            OriginMarks::PerRecord(&[]),
+        )
+        .expect_err("空 PerRecord 配非空 records 必须报错");
+        assert!(
+            matches!(error, StorageError::Corrupt { .. }),
+            "必须是显式损坏/参数错误，实际 {error:?}"
+        );
+        assert_eq!(
+            scalar_i64(&vault, "SELECT count(*) FROM loro_updates"),
+            0,
+            "参数错误不得留下半写"
+        );
+
+        let error = append_updates_inner(
+            &vault,
+            "doc-mismatch",
+            0,
+            &[record],
+            &[],
+            &[],
+            OriginMarks::PerRecord(&[true, false]),
+        )
+        .expect_err("标记多于 records 也必须报错");
+        assert!(matches!(error, StorageError::Corrupt { .. }));
+    }
+
+    /// 值 CAS 不命中（并发写者改了 meta）→ 有界重试后返回明确 SyncConflict，
+    /// 整笔 A 事务回滚：head/update/outbox/meta 都不留半写，也绝不写入更小值。
+    ///
+    /// 注入方式：`loro_updates` 的 BEFORE INSERT 触发器在**同一事务里**把 meta 改成
+    /// 别的值（模拟「读旧值之后、CAS 之前的并发提交」）；每次重试都会再次命中，
+    /// 因此有界重试必然耗尽。
+    #[test]
+    fn stale_remote_known_cas_fails_closed_and_rolls_back() {
+        let (_dir, vault) = fresh_vault("cas");
+        create_body_doc(&vault, "doc-cas").expect("loro_docs 行");
+        let remote_doc = BodyDoc::create("doc-cas", 0xB2).expect("创建 BodyDoc");
+        remote_doc.insert_scalars(0, "remote").expect("远端编辑");
+        let update = local_update(&remote_doc, &VersionVector::default());
+        vault
+            .exec(
+                "CREATE TRIGGER remote_known_race BEFORE INSERT ON loro_updates BEGIN \
+                   INSERT OR REPLACE INTO meta(key,value) \
+                   VALUES('sync.remote_known_vv.doc-cas', X'DEADBEEF'); \
+                 END;",
+                &[],
+            )
+            .expect("安装竞态触发器");
+
+        let error = append_updates_inner(
+            &vault,
+            "doc-cas",
+            0,
+            &[UpdateRecord::new("b0", update)],
+            &[],
+            &[],
+            OriginMarks::PerRecord(&[true]),
+        )
+        .expect_err("CAS 必须失败");
+
+        match error {
+            StorageError::SyncConflict { context } => {
+                assert!(
+                    context.contains("远端已知 VV"),
+                    "错误必须点明远端已知 VV CAS：{context}"
+                );
+                assert!(context.contains("可重试"), "错误必须说明可重试：{context}");
+            }
+            other => panic!("期望 SyncConflict，实际 {other:?}"),
+        }
+        assert_eq!(load_head(&vault, "doc-cas").expect("head").head_seq, 0);
+        assert_eq!(
+            scalar_i64(&vault, "SELECT count(*) FROM loro_updates"),
+            0,
+            "整笔回滚：update 不得落库"
+        );
+        assert_eq!(
+            scalar_i64(&vault, "SELECT count(*) FROM projection_outbox"),
+            0
+        );
+        assert_eq!(
+            scalar_i64(
+                &vault,
+                "SELECT count(*) FROM meta WHERE key='sync.remote_known_vv.doc-cas'"
+            ),
+            0,
+            "触发器写入随整笔回滚，失败后不残留"
+        );
+        assert!(
+            remote_known::load(&vault, "doc-cas")
+                .expect("读远端已知")
+                .is_none(),
+            "失败路径绝不能写入更小/错误的值"
+        );
+    }
+}
diff --git a/crates/storage/src/sync_bridge.rs b/crates/storage/src/sync_bridge.rs
index c9e2b44..a831dea 100644
--- a/crates/storage/src/sync_bridge.rs
+++ b/crates/storage/src/sync_bridge.rs
@@ -20,7 +20,7 @@ use crate::Vault;
 
 // 同步 `meta` 键与「连续前缀」的低层实现放在 `sync_meta`（直接跑在 `Connection` 上，
 // 这样批量 inbox 的插入与前缀推进能共用一个事务）；这里保持原有的公开路径。
-use crate::sync_meta::{skipped_received_key, ConsumedPrefix};
+use crate::sync_meta::{meta_bytes_of, skipped_received_key, ConsumedPrefix};
 pub use crate::sync_meta::{
     META_SYNC_CONFIRMED_HIGHWATER, META_SYNC_INTEGRATED_SEQ, META_SYNC_LOG_ID,
     META_SYNC_RECEIVED_SEQ, META_SYNC_SKIPPED_RECEIVED_PREFIX,
@@ -157,13 +157,16 @@ pub fn local_sync_intent(
 /// 一次 flush 的连续前缀可能**混**本地编辑与远端导入（`JournalOrigin`），而
 /// remote/import/adopt **绝不能**产生出站意图（否则会把刚收到的数据再推回 relay）。
 /// 这里仍是一条 A 事务、按位置一一对应，只是允许某些位置为空。
+///
+/// `slots` 空 = 本批没有**任何**出站意图；来源标记用 [`store::OriginMarks`]
+/// （`AllLocal` / 与 records 等长的 `PerRecord`），**不再用空切片**表示「全部本地」。
 pub(crate) fn append_updates_with_intent_slots(
     vault: &Vault,
     doc_id: &str,
     expected_head_seq: u64,
     records: &[store::UpdateRecord],
     slots: &[Option<SyncIntent>],
-    remote_marks: &[bool],
+    origin: store::OriginMarks<'_>,
 ) -> Result<store::HeadState, StorageError> {
     if !slots.is_empty() && slots.len() != records.len() {
         return Err(StorageError::Corrupt {
@@ -174,22 +177,13 @@ pub(crate) fn append_updates_with_intent_slots(
             ),
         });
     }
-    if !remote_marks.is_empty() && remote_marks.len() != records.len() {
-        return Err(StorageError::Corrupt {
-            context: format!(
-                "append_updates_with_intent_slots: records={} remote_marks={} 必须一对一",
-                records.len(),
-                remote_marks.len()
-            ),
-        });
-    }
     store::append_updates_with_intent_slots(
         vault,
         doc_id,
         expected_head_seq,
         records,
         slots,
-        remote_marks,
+        origin,
     )
 }
 
@@ -370,7 +364,14 @@ pub fn append_updates_with_sync_intents(
         });
     }
     let slots: Vec<Option<SyncIntent>> = intents.iter().cloned().map(Some).collect();
-    store::append_updates_with_intent_slots(vault, doc_id, expected_head_seq, records, &slots, &[])
+    store::append_updates_with_intent_slots(
+        vault,
+        doc_id,
+        expected_head_seq,
+        records,
+        &slots,
+        store::OriginMarks::AllLocal,
+    )
 }
 
 /// 单独记录一条出站意图（调用方已确认 update 落库）。
@@ -1089,15 +1090,31 @@ pub fn store_inbox_verified_batch(
 /// 读当前已验证、且落在 durable 连续接收前缀内的 inbox 行。
 ///
 /// 依赖完整性由 sync 层判断（本 crate 不解析 Loro VV）；这里的 "ready"
-/// 指 cryptographically verified + durable + `relay_seq <= received_seq`，
-/// 不在连续前缀内的行保持 durable，等前面的缺口补齐。排序为 `(log_id, relay_seq)`。
+/// 指 cryptographically verified + durable + **属于当前 `sync.log_id`** +
+/// `relay_seq <= received_seq`，不在连续前缀内的行保持 durable，等前面的缺口补齐。
+/// 排序为 `(log_id, relay_seq)`。
+///
+/// **incarnation 过滤**（ADR 0006 §5）：`sync_inbox` 的 `log_id` 是写入时那代 relay
+/// 日志的 incarnation。restore 迁移 `sync.log_id` 后，旧 incarnation 的行仍然 durable
+/// （不能在迁移时静默删数据），但绝不能进入 ready——`relay_seq` 只在同一 incarnation
+/// 内有序，跨代比较没有意义。过滤条件里的 `log_id` 由 [`read_current_log_id`] 读出后
+/// 作为 SQL 参数绑定，不做子查询以便调用方/测试都看到同一个显式值。
+///
+/// `sync.log_id` 不存在（新库尚未绑定 relay 日志代）时返回空：没有当前 incarnation
+/// 就没有可匹配的行；新库此时 `received` 也是 0，所以与旧实现（不按 log 过滤）的结果
+/// 一致，不会把其它 incarnation 的遗留行漏出来。`sync.log_id` 存在但不是 16B →
+/// [`StorageError::Corrupt`]（与 [`sync_cursor_state`] 同口径），不静默当作"没有 log"。
 pub fn take_inbox_ready(vault: &Vault, limit: usize) -> Result<Vec<SyncInboxRow>, StorageError> {
+    let Some(log_id) = read_current_log_id(vault)? else {
+        return Ok(Vec::new());
+    };
     let received = read_meta_u64(vault, META_SYNC_RECEIVED_SEQ)?.unwrap_or(0);
     let rows = vault.query(
         "SELECT log_id,relay_seq,object_id,epoch,state,verified_payload \
-         FROM sync_inbox WHERE state='verified' AND relay_seq<=?1 \
-         ORDER BY log_id,relay_seq LIMIT ?2",
+         FROM sync_inbox WHERE log_id=?1 AND state='verified' AND relay_seq<=?2 \
+         ORDER BY log_id,relay_seq LIMIT ?3",
         &[
+            Value::Blob(log_id.to_vec()),
             Value::Integer(i64_of(received)?),
             Value::Integer(i64::try_from(limit).unwrap_or(i64::MAX)),
         ],
@@ -1105,16 +1122,27 @@ pub fn take_inbox_ready(vault: &Vault, limit: usize) -> Result<Vec<SyncInboxRow>
     rows.iter().map(row_to_inbox).collect()
 }
 
-/// 统计「已验证、落在 durable 连续接收前缀内、尚未集成」的 inbox 行数。
+/// 统计「已验证、属于当前 incarnation、落在 durable 连续接收前缀内、尚未集成」的行数。
 ///
 /// 语义（ADR 0012 S2b）：这些行不是"丢失"——它们要么依赖未就绪（deferred），要么被
 /// 批次预算留在本轮之后。调用方用它上报"已接收·待应用 N"，并据此判断"还有没有
 /// 可继续的工作"，而不是循环刷最早的一批。
+///
+/// 与 [`take_inbox_ready`] 同口径：只计当前 `sync.log_id` 的行，旧 incarnation 的
+/// verified 行即使 `relay_seq <= received` 也不计。`sync.log_id` 不存在返回 0
+/// （没有当前 incarnation 就没有 ready 行）；存在但长度非法 → [`StorageError::Corrupt`]。
 pub fn count_inbox_ready(vault: &Vault) -> Result<u64, StorageError> {
+    let Some(log_id) = read_current_log_id(vault)? else {
+        return Ok(0);
+    };
     let received = read_meta_u64(vault, META_SYNC_RECEIVED_SEQ)?.unwrap_or(0);
     let rows = vault.query(
-        "SELECT COUNT(*) FROM sync_inbox WHERE state='verified' AND relay_seq<=?1",
-        &[Value::Integer(i64_of(received)?)],
+        "SELECT COUNT(*) FROM sync_inbox \
+         WHERE log_id=?1 AND state='verified' AND relay_seq<=?2",
+        &[
+            Value::Blob(log_id.to_vec()),
+            Value::Integer(i64_of(received)?),
+        ],
     )?;
     rows.first()
         .map(|row| u64_of(row, 0, "sync_inbox.count"))
@@ -1125,6 +1153,8 @@ pub fn count_inbox_ready(vault: &Vault) -> Result<u64, StorageError> {
 }
 
 /// 带调用方依赖判断的 ready 选择器；不满足的项保持 durable verified。
+///
+/// 底层走 [`take_inbox_ready`]，因此同样只选择当前 `sync.log_id` 的行。
 pub fn take_inbox_ready_where(
     vault: &Vault,
     limit: usize,
@@ -1270,13 +1300,22 @@ pub fn sync_cursor_state(vault: &Vault) -> Result<SyncCursorState, StorageError>
     Ok(SyncCursorState {
         received_seq: read_meta_u64(vault, META_SYNC_RECEIVED_SEQ)?.unwrap_or(0),
         integrated_seq: read_meta_u64(vault, META_SYNC_INTEGRATED_SEQ)?.unwrap_or(0),
-        log_id: read_meta(vault, META_SYNC_LOG_ID)?
-            .map(|bytes| array16_bytes(bytes, "sync.log_id"))
-            .transpose()?,
+        log_id: read_current_log_id(vault)?,
         confirmed_highwater: read_meta_u64(vault, META_SYNC_CONFIRMED_HIGHWATER)?,
     })
 }
 
+/// 读当前 relay 日志 incarnation（`sync.log_id`，BLOB 16B）。
+///
+/// - 不存在（新库/尚未绑定）：`Ok(None)`，调用方按"没有当前 incarnation"处理
+///   （ready 视为空，绝不回退成"不过滤 log"）；
+/// - 存在但不是 16B：[`StorageError::Corrupt`]，与 [`sync_cursor_state`] 的既有口径一致。
+fn read_current_log_id(vault: &Vault) -> Result<Option<[u8; 16]>, StorageError> {
+    read_meta(vault, META_SYNC_LOG_ID)?
+        .map(|bytes| array16_bytes(bytes, "sync.log_id"))
+        .transpose()
+}
+
 /// 写入 `sync.log_id`；已有不同值则拒绝（restore 必须显式迁移）。
 pub fn set_sync_log_id(vault: &Vault, log_id: &[u8; 16]) -> Result<(), StorageError> {
     ensure_log_id(vault, log_id)
@@ -1333,15 +1372,21 @@ pub fn mark_skipped_event_received(
     advance_received_prefix(vault)
 }
 
+/// 读一条 meta（同步桥经 Vault 命令的读取路径）。
+///
+/// 类型判定与 [`Vault::get_meta`] / `sync_meta::read_meta_on` 共用同一个 helper
+/// [`meta_bytes_of`]：**只认 BLOB**，其它类型是 [`StorageError::Corrupt`]。
+/// 这里以前把 TEXT 当"缺失"，会让 `sync.log_id` / `sync.repack_required` /
+/// `sync.received_seq` 这些持久屏障与游标被静默绕过。
 fn read_meta(vault: &Vault, key: &str) -> Result<Option<Vec<u8>>, StorageError> {
     let rows = vault.query(
         "SELECT value FROM meta WHERE key=?1",
         &[Value::Text(key.to_owned())],
     )?;
-    Ok(rows
-        .first()
-        .and_then(|row| row.first())
-        .and_then(blob_value))
+    let Some(value) = rows.first().and_then(|row| row.first()) else {
+        return Ok(None);
+    };
+    Ok(Some(meta_bytes_of(key, value.clone())?))
 }
 
 fn read_meta_u64(vault: &Vault, key: &str) -> Result<Option<u64>, StorageError> {
diff --git a/crates/storage/src/sync_meta.rs b/crates/storage/src/sync_meta.rs
index 82db021..4ab5086 100644
--- a/crates/storage/src/sync_meta.rs
+++ b/crates/storage/src/sync_meta.rs
@@ -71,6 +71,10 @@ impl ConsumedPrefix {
 ///
 /// 与 Vault 版 [`crate::sync_bridge::set_sync_log_id`] 同语义，区别只是直接在给定连接
 /// （或事务）上执行，供批量 inbox 在同一事务里完成校验。
+///
+/// 既有值不是 BLOB 时 [`read_meta_on`] 返回 [`StorageError::Corrupt`]，本函数直接传播：
+/// 损坏的 incarnation 记录**不能**当成"尚未绑定"覆盖掉（那会让旧传代的 inbox 行混进
+/// 新一代）。
 pub(crate) fn ensure_log_id_on(conn: &Connection, log_id: &[u8; 16]) -> Result<(), StorageError> {
     match read_meta_on(conn, META_SYNC_LOG_ID)? {
         Some(existing) => {
@@ -313,16 +317,58 @@ pub(crate) fn skipped_received_key(log_id: &[u8], relay_seq: u64) -> Result<Stri
     ))
 }
 
-fn read_meta_on(conn: &Connection, key: &str) -> Result<Option<Vec<u8>>, StorageError> {
+/// `meta.value` 的**唯一**解读口径：只认 BLOB，其它 SQLite 类型一律是损坏。
+///
+/// 三个读取入口共享本函数，避免"同一份库读出两套结论"漂移：
+///
+/// - [`crate::Vault::get_meta`]（actor 线程上的命令）；
+/// - `sync_bridge::read_meta`（同步桥经 Vault 命令读取）；
+/// - 本模块的 [`read_meta_on`]（worker 独占连接 / 事务内的批量 inbox 校验）。
+///
+/// 为什么不把其它类型「当字节」或「当缺失」：
+///
+/// - **当缺失会静默绕过持久屏障**：`sync.repack_required`（轮换后必须重打包）、
+///   `sync.log_id`（incarnation 绑定）、`sync.received_seq` 这类键"读不到"就等价于
+///   "没有屏障/没有游标"，把一处损坏放大成绕过安全前置条件；
+/// - **当字节与 schema 相悖**：`meta.value` 是 `BLOB NOT NULL`（STRICT 表），能读出
+///   TEXT/INTEGER/REAL 本身就说明库被外部改坏或 schema 漂移，继续按字节解释只会让
+///   损坏值流入上层语义。
+///
+/// 正常写入路径写不出非 BLOB（STRICT 表会直接拒绝），所以这里命中的都是
+/// "库被外部改坏"的场景：宁可 fail-closed 报损坏，也不猜。
+pub(crate) fn meta_bytes_of(key: &str, value: Value) -> Result<Vec<u8>, StorageError> {
+    match value {
+        Value::Blob(bytes) => Ok(bytes),
+        other => Err(StorageError::Corrupt {
+            context: format!(
+                "meta[{key}] 应为 BLOB，实际 {}（库被外部改坏或 schema 漂移）",
+                sqlite_type_name(&other)
+            ),
+        }),
+    }
+}
+
+/// SQLite 值的类型名（只用于错误信息）。
+fn sqlite_type_name(value: &Value) -> &'static str {
+    match value {
+        Value::Null => "NULL",
+        Value::Integer(_) => "INTEGER",
+        Value::Real(_) => "REAL",
+        Value::Text(_) => "TEXT",
+        Value::Blob(_) => "BLOB",
+    }
+}
+
+/// 在给定连接（或事务）上读一条 meta；键不存在返回 `None`。
+///
+/// 类型判定走 [`meta_bytes_of`]：只认 BLOB，其它类型是 [`StorageError::Corrupt`]
+/// （不再静默当作缺失）。
+pub(crate) fn read_meta_on(conn: &Connection, key: &str) -> Result<Option<Vec<u8>>, StorageError> {
     let mut statement = conn.prepare("SELECT value FROM meta WHERE key=?1")?;
     let mut rows = statement.query([key])?;
     match rows.next()? {
         None => Ok(None),
-        // 与旧实现同口径：只认 BLOB；其它类型视为没有该 meta。
-        Some(row) => match row.get::<usize, Value>(0)? {
-            Value::Blob(blob) => Ok(Some(blob)),
-            _ => Ok(None),
-        },
+        Some(row) => Ok(Some(meta_bytes_of(key, row.get::<usize, Value>(0)?)?)),
     }
 }
 
diff --git a/crates/storage/tests/baseline_apply.rs b/crates/storage/tests/baseline_apply.rs
index 5aac713..f312f17 100644
--- a/crates/storage/tests/baseline_apply.rs
+++ b/crates/storage/tests/baseline_apply.rs
@@ -4,6 +4,7 @@ mod support;
 
 use std::time::Duration;
 
+use loro::VersionVector;
 use support::*;
 
 use menghunote_storage::session::{
@@ -15,9 +16,10 @@ use menghunote_storage::{
     apply_baseline_batch, baseline_apply_debug_counters, baseline_apply_state,
     begin_baseline_apply, create_note_slot, finish_baseline_apply, load_body_doc, load_head,
     reset_baseline_apply_debug, set_sync_log_id, store_inbox_verified_batch, sync_cursor_state,
-    BaselineApplyFinish, BaselineApplyStatus, BaselineAsset, BodyDoc, SyncContext, UpdateRecord,
-    Value, Vault, VerifiedInboxRow, META_SYNC_CONFIRMED_HIGHWATER, META_SYNC_INTEGRATED_SEQ,
-    META_SYNC_RECEIVED_SEQ, META_SYNC_RELAY_CURSOR, SCHEMA_VERSION,
+    BaselineApplyFinish, BaselineApplyStatus, BaselineAsset, BaselineExpectation, BodyDoc,
+    StorageError, SyncContext, UpdateRecord, Value, Vault, VerifiedInboxRow,
+    META_SYNC_CONFIRMED_HIGHWATER, META_SYNC_INTEGRATED_SEQ, META_SYNC_RECEIVED_SEQ,
+    META_SYNC_RELAY_CURSOR, SCHEMA_VERSION,
 };
 use menghunote_text::UnicodeScalar;
 
@@ -85,14 +87,89 @@ fn meta_remote_known_count(vault: &Vault, doc_id: &str) -> i64 {
     )
 }
 
+/// 读某篇文档的远端已知 VV；没有记录返回 `None`。
+fn meta_remote_known(vault: &Vault, doc_id: &str) -> Option<VersionVector> {
+    meta_remote_known_bytes(vault, doc_id).map(|bytes| VersionVector::decode(&bytes).expect("解码"))
+}
+
+/// 读某篇文档的远端已知 VV 原始字节（用于断言「不降级」）。
+fn meta_remote_known_bytes(vault: &Vault, doc_id: &str) -> Option<Vec<u8>> {
+    let rows = vault
+        .query(
+            "SELECT value FROM meta WHERE key=?1",
+            &[Value::Text(format!("sync.remote_known_vv.{doc_id}"))],
+        )
+        .expect("meta 查询");
+    match rows.first().and_then(|row| row.first()) {
+        Some(Value::Blob(bytes)) => Some(bytes.clone()),
+        None => None,
+        other => panic!("remote_known 值异常：{other:?}"),
+    }
+}
+
+/// 注入「读旧值之后、CAS 之前 meta 被并发改写」：在 baseline_apply_docs 首次写入时
+/// 把该文档的远端已知 VV 改成别的字节（同一事务内可见，CAS 必然不命中）。
+fn install_remote_known_clobber_trigger(vault: &Vault, doc_id: &str) {
+    let sql = format!(
+        "CREATE TRIGGER remote_known_cas_race BEFORE INSERT ON baseline_apply_docs \
+         BEGIN \
+           INSERT OR REPLACE INTO meta(key,value) \
+           VALUES('sync.remote_known_vv.{doc_id}', X'DEADBEEF'); \
+         END;"
+    );
+    vault.exec(&sql, &[]).expect("安装竞态触发器");
+}
+
 fn snapshot(doc_id: &str, peer: u64, text: &str) -> Vec<u8> {
     let doc = BodyDoc::create(doc_id, peer).expect("创建基线 LoroDoc");
     doc.insert_scalars(0, text).expect("写基线正文");
     doc.export_snapshot().expect("导出基线 snapshot")
 }
 
-fn begin(vault: &Vault, s: u64, cursor: &str) {
-    begin_baseline_apply(vault, &[7; 16], s, cursor).expect("begin baseline");
+/// 预期参与集合：只声明文档（本文件里大多数用例没有资产）。
+fn expectation(docs: &[&str]) -> BaselineExpectation {
+    BaselineExpectation::new(
+        docs.iter().map(|doc_id| (*doc_id).to_owned()),
+        std::iter::empty::<[u8; 32]>(),
+    )
+}
+
+/// 预期参与集合：文档 + 资产。
+fn expectation_with_assets(docs: &[&str], assets: &[[u8; 32]]) -> BaselineExpectation {
+    BaselineExpectation::new(
+        docs.iter().map(|doc_id| (*doc_id).to_owned()),
+        assets.iter().copied(),
+    )
+}
+
+/// begin 一份**空预期集合**的 staging（只关心 cursor/journal 语义的用例）。
+fn begin(vault: &Vault, s: u64, cursor: &str) -> menghunote_storage::BaselineApplyState {
+    begin_with(vault, s, cursor, &BaselineExpectation::default())
+}
+
+/// begin 一份显式声明预期参与集合的 staging。
+fn begin_with(
+    vault: &Vault,
+    s: u64,
+    cursor: &str,
+    expected: &BaselineExpectation,
+) -> menghunote_storage::BaselineApplyState {
+    begin_baseline_apply(vault, &[7; 16], s, cursor, expected).expect("begin baseline")
+}
+
+/// 读某篇文档的 tombstone action（当前 journal）。
+fn tombstone_action(vault: &Vault, doc_id: &str) -> Option<String> {
+    let rows = vault
+        .query(
+            "SELECT action FROM baseline_apply_tombstones WHERE doc_id=?1",
+            &[Value::Text(doc_id.to_owned())],
+        )
+        .expect("tombstone 查询");
+    match rows.first().and_then(|row| row.first()) {
+        Some(Value::Text(value)) => Some(value.clone()),
+        Some(other) => panic!("tombstone action 异常：{other:?}"),
+        None => None,
+    }
 }
 
 /// 模拟"已有库"的同步水位；`None` 表示 meta 缺省。
@@ -153,7 +230,7 @@ fn baseline_merge_preserves_local_edit_and_advances_cursor() {
     let local = local_doc(&vault, "doc-1", "local");
     let snap = snapshot("doc-1", PEER_B, "remote");
 
-    begin(&vault, 5, "source-cursor");
+    begin_with(&vault, 5, "source-cursor", &expectation(&["doc-1"]));
     assert_eq!(
         baseline_apply_state(&vault)
             .expect("state")
@@ -273,7 +350,7 @@ fn batch_is_resumable_after_reopen_and_idempotent() {
     let snap_a = snapshot("doc-a", PEER_B, "a-remote");
     let snap_b = snapshot("doc-b", PEER_B, "b-remote");
 
-    begin(&vault, 9, "src");
+    begin_with(&vault, 9, "src", &expectation(&["doc-a", "doc-b"]));
     apply_baseline_batch(&vault, &[("doc-a".to_owned(), snap_a.clone())], &[]).expect("batch a");
     let cursor = sync_cursor_state(&vault).expect("cursor before finish");
     assert_eq!(cursor.received_seq, 0);
@@ -317,7 +394,7 @@ fn batch_is_resumable_after_reopen_and_idempotent() {
 fn finish_cursor_cas_conflict_does_not_advance() {
     let (_dir, vault) = fresh_vault();
     local_doc(&vault, "doc-1", "local");
-    begin(&vault, 4, "src");
+    begin_with(&vault, 4, "src", &expectation(&["doc-1"]));
     apply_baseline_batch(
         &vault,
         &[("doc-1".to_owned(), snapshot("doc-1", PEER_B, "remote"))],
@@ -881,7 +958,7 @@ fn replay_same_digest_skips_snapshot_decode() {
 #[test]
 fn tombstone_deletes_previous_baseline_doc_after_begin_clears_staging_tables() {
     let (_dir, vault) = fresh_vault();
-    begin(&vault, 10, "src1");
+    begin_with(&vault, 10, "src1", &expectation(&["doc-x"]));
     apply_baseline_batch(
         &vault,
         &[("doc-x".to_owned(), snapshot("doc-x", PEER_B, "remote body"))],
@@ -1080,3 +1157,620 @@ fn tombstone_deletes_doc_integrated_through_engine_path() {
     );
     assert_eq!(state.kept_local_edit, 0);
 }
+
+// ---------------------------------------------------------------------------
+// P1-1：finish 完整性（begin 记录预期参与集合，finish 前逐项证明）
+// ---------------------------------------------------------------------------
+
+/// 少跑一批就 finish：必须 Conflict 且一行都不写（旧实现会 Applied 并推进 S，
+/// pull 之后永久跳过这批 relay 事件）。
+#[test]
+fn finish_without_all_docs_is_conflict() {
+    let (_dir, vault) = fresh_vault();
+    local_doc(&vault, "doc-a", "a-local");
+    local_doc(&vault, "doc-b", "b-local");
+    begin_with(&vault, 5, "src", &expectation(&["doc-a", "doc-b"]));
+
+    let state = apply_baseline_batch(
+        &vault,
+        &[("doc-a".to_owned(), snapshot("doc-a", PEER_B, "a-remote"))],
+        &[],
+    )
+    .expect("batch a");
+    assert_eq!(state.applied_docs, 1);
+    assert_eq!(state.expected_docs, Some(2));
+    assert!(
+        !state.finishable,
+        "预期集合未齐（doc-b 缺失）时不得声称可 finish"
+    );
+
+    assert_eq!(
+        finish_baseline_apply(&vault, 5, "baseline-cursor").expect("finish"),
+        BaselineApplyFinish::Conflict,
+        "预期集合未齐时 finish 必须 Conflict"
+    );
+
+    // 冲突时一行都不写：水位/meta/游标/journal 全部保持 begin 前状态。
+    let cursor = sync_cursor_state(&vault).expect("cursor");
+    assert_eq!(cursor.received_seq, 0, "冲突时不得推进 received");
+    assert_eq!(cursor.integrated_seq, 0, "冲突时不得推进 integrated");
+    assert_eq!(cursor.confirmed_highwater, None, "冲突时不得写 highwater");
+    assert_eq!(relay_cursor(&vault), None, "冲突时不得写 relay cursor");
+    let journal = baseline_apply_state(&vault)
+        .expect("state")
+        .expect("journal");
+    assert_eq!(journal.status, BaselineApplyStatus::Staging);
+    assert_eq!(journal.chosen_cursor, None);
+    assert_eq!(journal.applied_docs, 1, "冲突不得回滚已提交批次");
+
+    // 补齐缺失批次后必须能正常 finish（Conflict 不是永久卡死）。
+    let state = apply_baseline_batch(
+        &vault,
+        &[("doc-b".to_owned(), snapshot("doc-b", PEER_B, "b-remote"))],
+        &[],
+    )
+    .expect("batch b");
+    assert!(state.finishable, "两篇都到齐后必须可 finish");
+    let BaselineApplyFinish::Applied(state) =
+        finish_baseline_apply(&vault, 5, "baseline-cursor").expect("finish")
+    else {
+        panic!("补齐后 finish 必须 Applied");
+    };
+    assert_eq!(state.status, BaselineApplyStatus::Completed);
+    assert_eq!(state.applied_docs, 2);
+    let cursor = sync_cursor_state(&vault).expect("cursor");
+    assert_eq!(cursor.received_seq, 5);
+    assert_eq!(cursor.integrated_seq, 5);
+    assert_eq!(cursor.confirmed_highwater, Some(5));
+}
+
+/// 全部分批到齐：finish 正常 Applied。
+#[test]
+fn finish_after_full_batches_is_applied() {
+    let (_dir, vault) = fresh_vault();
+    local_doc(&vault, "doc-a", "a-local");
+    local_doc(&vault, "doc-b", "b-local");
+    begin_with(&vault, 9, "src", &expectation(&["doc-a", "doc-b"]));
+
+    let first = apply_baseline_batch(
+        &vault,
+        &[("doc-a".to_owned(), snapshot("doc-a", PEER_B, "a-remote"))],
+        &[],
+    )
+    .expect("batch a");
+    assert_eq!(first.expected_docs, Some(2));
+    assert!(!first.finishable);
+
+    let second = apply_baseline_batch(
+        &vault,
+        &[("doc-b".to_owned(), snapshot("doc-b", PEER_B, "b-remote"))],
+        &[],
+    )
+    .expect("batch b");
+    assert!(second.finishable);
+
+    let BaselineApplyFinish::Applied(state) =
+        finish_baseline_apply(&vault, 9, "baseline-cursor").expect("finish")
+    else {
+        panic!("完整批次 finish 必须 Applied");
+    };
+    assert_eq!(state.status, BaselineApplyStatus::Completed);
+    assert_eq!(state.applied_docs, 2);
+    assert_eq!(state.expected_docs, Some(2));
+    assert_eq!(state.expected_assets, Some(0));
+    assert!(!state.finishable, "completed 后不再是 finishable");
+    let cursor = sync_cursor_state(&vault).expect("cursor");
+    assert_eq!(cursor.received_seq, 9);
+    assert_eq!(cursor.integrated_seq, 9);
+    assert_eq!(cursor.confirmed_highwater, Some(9));
+    assert_eq!(
+        relay_cursor(&vault).as_deref(),
+        Some(b"baseline-cursor".as_slice())
+    );
+}
+
+/// begin 同参数重入幂等（含顺序无关的同一集合）；同 checkpoint 换集合必须冲突，
+/// 且不得清掉已应用进度或预期集合。
+#[test]
+fn begin_idempotent_replay_keeps_expected_set() {
+    let (_dir, vault) = fresh_vault();
+    local_doc(&vault, "doc-a", "a-local");
+    local_doc(&vault, "doc-b", "b-local");
+    begin_with(&vault, 5, "src", &expectation(&["doc-a", "doc-b"]));
+    apply_baseline_batch(
+        &vault,
+        &[("doc-a".to_owned(), snapshot("doc-a", PEER_B, "a-remote"))],
+        &[],
+    )
+    .expect("batch a");
+
+    // 同参数 + 同一集合（顺序不同）：幂等重入，保留进度与集合。
+    let replayed = begin_with(&vault, 5, "src", &expectation(&["doc-b", "doc-a"]));
+    assert_eq!(replayed.applied_docs, 1, "重入不得清空已应用批次");
+    assert_eq!(replayed.expected_docs, Some(2), "重入必须保留预期集合");
+    assert_eq!(replayed.source_cursor, "src");
+    assert!(!replayed.finishable);
+
+    // 同 checkpoint/S/cursor 但预期集合不同：必须冲突（不能静默沿用新集合）。
+    let error = begin_baseline_apply(&vault, &[7; 16], 5, "src", &expectation(&["doc-a"]))
+        .expect_err("同一 checkpoint 换预期集合必须冲突");
+    assert!(
+        matches!(error, StorageError::SyncConflict { .. }),
+        "应当是 SyncConflict，实际 {error:?}"
+    );
+    let state = baseline_apply_state(&vault)
+        .expect("state")
+        .expect("journal");
+    assert_eq!(state.applied_docs, 1, "冲突不得清空已应用批次");
+    assert_eq!(state.expected_docs, Some(2), "冲突不得改写预期集合");
+
+    // 补齐后仍能 finish。
+    apply_baseline_batch(
+        &vault,
+        &[("doc-b".to_owned(), snapshot("doc-b", PEER_B, "b-remote"))],
+        &[],
+    )
+    .expect("batch b");
+    assert!(matches!(
+        finish_baseline_apply(&vault, 5, "baseline-cursor").expect("finish"),
+        BaselineApplyFinish::Applied(_)
+    ));
+}
+
+/// 资产少一批同样必须 Conflict（bootstrap 的资产也只能在全部到齐后 finish）。
+#[test]
+fn finish_without_all_assets_is_conflict() {
+    let (_dir, vault) = fresh_vault();
+    local_doc(&vault, "doc-1", "local");
+    let asset_a = BaselineAsset {
+        object_id: [1; 32],
+        asset_hash: [11; 32],
+        byte_len: 3,
+        media_type: Some("text/plain".to_owned()),
+        data: b"aaa".to_vec(),
+    };
+    let asset_b = BaselineAsset {
+        object_id: [2; 32],
+        asset_hash: [12; 32],
+        byte_len: 4,
+        media_type: Some("text/plain".to_owned()),
+        data: b"bbbb".to_vec(),
+    };
+    begin_with(
+        &vault,
+        4,
+        "src",
+        &expectation_with_assets(&["doc-1"], &[[1; 32], [2; 32]]),
+    );
+    apply_baseline_batch(
+        &vault,
+        &[("doc-1".to_owned(), snapshot("doc-1", PEER_B, "remote"))],
+        &[],
+    )
+    .expect("doc batch");
+    let state = apply_baseline_assets(&vault, std::slice::from_ref(&asset_a)).expect("asset a");
+    assert_eq!(state.applied_assets, 1);
+    assert_eq!(state.expected_assets, Some(2));
+    assert!(!state.finishable, "资产少一批时不得声称可 finish");
+
+    assert_eq!(
+        finish_baseline_apply(&vault, 4, "baseline-cursor").expect("finish"),
+        BaselineApplyFinish::Conflict,
+        "预期资产未齐时 finish 必须 Conflict"
+    );
+    // 一行都不写：水位/游标/journal 保持 staging。
+    let cursor = sync_cursor_state(&vault).expect("cursor");
+    assert_eq!(cursor.received_seq, 0);
+    assert_eq!(cursor.integrated_seq, 0);
+    assert_eq!(cursor.confirmed_highwater, None);
+    assert_eq!(relay_cursor(&vault), None);
+    let journal = baseline_apply_state(&vault)
+        .expect("state")
+        .expect("journal");
+    assert_eq!(journal.status, BaselineApplyStatus::Staging);
+    assert_eq!(journal.chosen_cursor, None);
+    assert_eq!(journal.applied_assets, 1, "冲突不得回滚已应用资产");
+
+    let state = apply_baseline_assets(&vault, std::slice::from_ref(&asset_b)).expect("asset b");
+    assert!(state.finishable);
+    assert!(matches!(
+        finish_baseline_apply(&vault, 4, "baseline-cursor").expect("finish"),
+        BaselineApplyFinish::Applied(_)
+    ));
+}
+
+// ---------------------------------------------------------------------------
+// P1-2：tombstone 判定与 notes 软删之间的 TOCTOU
+// ---------------------------------------------------------------------------
+
+/// 判定（事务外看到无 outbox）之后、事务执行期间插进一条 outbox：事务内重判必须把
+/// action 降级为 kept_local_edit，`notes.deleted` 保持 0。
+#[test]
+fn tombstone_outbox_race_between_decision_and_transaction_keeps_note() {
+    let (_dir, vault) = fresh_vault();
+    // doc-a：本批要 merge 的基线文档；触发器挂在它的 journal 写入上。
+    local_doc(&vault, "doc-a", "a-local");
+
+    // doc-pull：普通 pull 集成来的文档（远端已知、无 outbox），判定本来会删除它。
+    seed_note(&vault, "note-pull", "doc-pull");
+    let mut session =
+        DocumentSession::open(&vault, "doc-pull", session_options(Some(SYNC_CTX))).expect("会话");
+    let delta = session
+        .apply_remote(&remote_update("doc-pull", PEER_B, "pulled body"))
+        .expect("导入远端")
+        .expect("应当产生 LiveDelta");
+    assert_eq!(delta.source, ChangeSource::Remote);
+    session.flush(FlushReason::Manual);
+    session
+        .wait_for_durable(LiveRev(1), Duration::from_secs(5))
+        .expect("远端导入必须 durable");
+    drop(session);
+    assert_eq!(count(&vault, "SELECT count(*) FROM sync_outbox", &[]), 0);
+    assert_eq!(meta_remote_known_count(&vault, "doc-pull"), 1);
+
+    // 注入竞态：docs 批的 journal 写入（先于同批 tombstone 语句）触发一条 outbox 行，
+    // 模拟"判定完成后本地编辑的 A 事务刚好提交"。
+    vault
+        .exec(
+            "CREATE TRIGGER baseline_race_outbox BEFORE INSERT ON baseline_apply_docs \
+             WHEN NEW.doc_id='doc-a' BEGIN \
+               INSERT INTO sync_outbox(message_id,batch_id,doc_id,seq,key_epoch,ciphertext) \
+               VALUES('race-outbox','race','doc-pull',1,1,NULL); \
+             END;",
+            &[],
+        )
+        .expect("注入竞态触发器");
+
+    begin_with(&vault, 7, "src", &expectation(&["doc-a"]));
+    let state = apply_baseline_batch(
+        &vault,
+        &[("doc-a".to_owned(), snapshot("doc-a", PEER_B, "a-remote"))],
+        &["doc-pull".to_owned()],
+    )
+    .expect("apply with raced tombstone");
+
+    assert_eq!(
+        count(
+            &vault,
+            "SELECT count(*) FROM sync_outbox WHERE doc_id='doc-pull'",
+            &[],
+        ),
+        1,
+        "触发器必须真的插入了 outbox（证明竞态窗口被覆盖）"
+    );
+    assert_eq!(
+        notes_deleted(&vault, "doc-pull"),
+        0,
+        "判定后被并发插入 outbox 的笔记绝不能软删"
+    );
+    assert_eq!(
+        tombstone_action(&vault, "doc-pull").as_deref(),
+        Some("kept_local_edit"),
+        "事务内重判必须把 action 降级为 kept_local_edit"
+    );
+    assert_eq!(state.kept_local_edit, 1);
+}
+
+// ---------------------------------------------------------------------------
+// P2-5：远端已知 VV 的单调性（值 CAS）与 begin 回填覆盖面
+// ---------------------------------------------------------------------------
+
+/// completed journal（finish 后 status='completed'、子表保留）在 begin 替换时
+/// 必须先回填 baseline_vv；这是旧库升级后 tombstone 不误判的关键证据。
+#[test]
+fn begin_backfills_remote_known_for_completed_journal() {
+    let (_dir, vault) = fresh_vault();
+    begin_with(&vault, 10, "src1", &expectation(&["doc-completed"]));
+    apply_baseline_batch(
+        &vault,
+        &[(
+            "doc-completed".to_owned(),
+            snapshot("doc-completed", PEER_B, "remote body"),
+        )],
+        &[],
+    )
+    .expect("first baseline");
+    assert!(matches!(
+        finish_baseline_apply(&vault, 10, "c10").expect("finish"),
+        BaselineApplyFinish::Applied(_)
+    ));
+    let baseline_vv = meta_remote_known(&vault, "doc-completed").expect("已有远端已知 VV");
+
+    // 模拟旧版本数据：没有跨 journal 的 meta，只剩 completed journal + 子表。
+    vault
+        .exec(
+            "DELETE FROM meta WHERE key=?1",
+            &[Value::Text("sync.remote_known_vv.doc-completed".to_owned())],
+        )
+        .expect("删 meta");
+    assert_eq!(
+        count(
+            &vault,
+            "SELECT count(*) FROM baseline_apply_docs WHERE doc_id='doc-completed'",
+            &[]
+        ),
+        1
+    );
+
+    begin(&vault, 20, "src2");
+
+    assert_eq!(
+        meta_remote_known(&vault, "doc-completed"),
+        Some(baseline_vv),
+        "completed journal 的 baseline_vv 必须回填到 meta"
+    );
+    assert_eq!(
+        count(&vault, "SELECT count(*) FROM baseline_apply_docs", &[]),
+        0,
+        "旧 journal 子表必须清掉"
+    );
+    let state = baseline_apply_state(&vault)
+        .expect("state")
+        .expect("journal");
+    assert_eq!(state.status, BaselineApplyStatus::Staging);
+    assert_eq!(state.baseline_s, 20);
+}
+
+/// 没有 journal 行、但残留 `baseline_apply_docs`（旧版本/异常写的孤儿行）时，
+/// begin 也必须先把 `baseline_vv` 回填到 meta，再清掉残留子表。
+#[test]
+fn begin_backfills_remote_known_without_journal_but_with_residual_docs() {
+    let (_dir, vault) = fresh_vault();
+    begin_with(&vault, 10, "src1", &expectation(&["doc-residual"]));
+    apply_baseline_batch(
+        &vault,
+        &[(
+            "doc-residual".to_owned(),
+            snapshot("doc-residual", PEER_B, "remote body"),
+        )],
+        &[],
+    )
+    .expect("first baseline");
+    assert!(matches!(
+        finish_baseline_apply(&vault, 10, "c10").expect("finish"),
+        BaselineApplyFinish::Applied(_)
+    ));
+
+    // 模拟旧版本/异常写：journal 行丢失，但子表仍在；meta 也缺失。
+    vault
+        .exec("DELETE FROM baseline_apply", &[])
+        .expect("删 journal");
+    vault
+        .exec(
+            "DELETE FROM meta WHERE key=?1",
+            &[Value::Text("sync.remote_known_vv.doc-residual".to_owned())],
+        )
+        .expect("删 meta");
+    assert_eq!(
+        count(
+            &vault,
+            "SELECT count(*) FROM baseline_apply_docs WHERE applied=1",
+            &[]
+        ),
+        1,
+        "前置：残留一行已应用的 baseline_apply_docs"
+    );
+
+    begin(&vault, 20, "src2");
+
+    assert_eq!(
+        meta_remote_known_count(&vault, "doc-residual"),
+        1,
+        "没有 journal 的残留 baseline_vv 也必须回填（否则证据会被 DELETE 静默丢弃）"
+    );
+    assert_eq!(
+        meta_remote_known(&vault, "doc-residual"),
+        Some(
+            VersionVector::decode(
+                &meta_remote_known_bytes(&vault, "doc-residual").expect("回填值")
+            )
+            .expect("解码")
+        ),
+        "回填的是旧 journal 的 baseline VV"
+    );
+    assert_eq!(
+        count(&vault, "SELECT count(*) FROM baseline_apply_docs", &[]),
+        0,
+        "残留子表必须被清掉"
+    );
+    let state = baseline_apply_state(&vault)
+        .expect("state")
+        .expect("journal");
+    assert_eq!(state.status, BaselineApplyStatus::Staging);
+    assert_eq!(state.baseline_s, 20);
+}
+
+/// begin 的回填是单调的：已有 meta（普通 pull/integrate 记录的更新值）绝不被
+/// 旧 journal 的 baseline_vv 降级（`DO NOTHING`）。
+#[test]
+fn begin_backfill_never_downgrades_existing_remote_known() {
+    let (_dir, vault) = fresh_vault();
+    begin_with(&vault, 10, "src1", &expectation(&["doc-keep"]));
+    apply_baseline_batch(
+        &vault,
+        &[(
+            "doc-keep".to_owned(),
+            snapshot("doc-keep", PEER_B, "baseline body"),
+        )],
+        &[],
+    )
+    .expect("first baseline");
+    assert!(matches!(
+        finish_baseline_apply(&vault, 10, "c10").expect("finish"),
+        BaselineApplyFinish::Applied(_)
+    ));
+
+    // 已有值比旧 journal 的 baseline VV 更大（模拟后续普通 pull 扩大了它）。
+    let mut superset = meta_remote_known(&vault, "doc-keep").expect("baseline 后已有值");
+    superset.insert(0xA1, 9);
+    vault
+        .exec(
+            "INSERT INTO meta(key,value) VALUES(?1,?2) \
+             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
+            &[
+                Value::Text("sync.remote_known_vv.doc-keep".to_owned()),
+                Value::Blob(superset.encode()),
+            ],
+        )
+        .expect("写入更大的旧值");
+    let before = meta_remote_known_bytes(&vault, "doc-keep").expect("旧值");
+
+    vault
+        .exec("DELETE FROM baseline_apply", &[])
+        .expect("模拟无 journal 的残留子表");
+    begin(&vault, 20, "src2");
+
+    assert_eq!(
+        meta_remote_known_bytes(&vault, "doc-keep"),
+        Some(before),
+        "begin 回填绝不能把已有（更大的）远端已知 VV 降级"
+    );
+}
+
+/// 新文档（读旧值时不存在）的 CAS：事务内被并发写入抢占 → 明确 SyncConflict，
+/// 整批回滚，不覆盖任何值。
+#[test]
+fn baseline_remote_known_cas_fails_closed_for_new_doc() {
+    let (_dir, vault) = fresh_vault();
+    begin_with(&vault, 5, "src", &expectation(&["doc-cas-new"]));
+    install_remote_known_clobber_trigger(&vault, "doc-cas-new");
+
+    let error = apply_baseline_batch(
+        &vault,
+        &[(
+            "doc-cas-new".to_owned(),
+            snapshot("doc-cas-new", PEER_B, "remote"),
+        )],
+        &[],
+    )
+    .expect_err("CAS 必须失败");
+
+    match &error {
+        StorageError::SyncConflict { context } => {
+            assert!(
+                context.contains("远端已知 VV"),
+                "错误必须点明远端已知 VV CAS：{context}"
+            );
+        }
+        other => panic!("期望 SyncConflict，实际 {other:?}"),
+    }
+    assert_eq!(
+        meta_remote_known_bytes(&vault, "doc-cas-new"),
+        None,
+        "触发器写入的 meta 必须随整批回滚"
+    );
+    assert_eq!(
+        count(
+            &vault,
+            "SELECT count(*) FROM loro_docs WHERE doc_id='doc-cas-new'",
+            &[]
+        ),
+        0,
+        "整批回滚：文档行不得落库"
+    );
+    assert_eq!(
+        count(
+            &vault,
+            "SELECT count(*) FROM loro_updates WHERE doc_id='doc-cas-new'",
+            &[]
+        ),
+        0
+    );
+}
+
+/// 已存在文档（读旧值 `Some`）的 CAS：库中值被并发改写 → 明确 SyncConflict，
+/// 队头与远端已知 VV 都保持原值（绝不回退到本批并集）。
+#[test]
+fn baseline_remote_known_cas_fails_closed_for_existing_doc() {
+    let (_dir, vault) = fresh_vault();
+    begin_with(&vault, 5, "src1", &expectation(&["doc-cas-old"]));
+    apply_baseline_batch(
+        &vault,
+        &[(
+            "doc-cas-old".to_owned(),
+            snapshot("doc-cas-old", PEER_B, "first"),
+        )],
+        &[],
+    )
+    .expect("first baseline");
+    assert!(matches!(
+        finish_baseline_apply(&vault, 5, "c5").expect("finish"),
+        BaselineApplyFinish::Applied(_)
+    ));
+    let before_known = meta_remote_known_bytes(&vault, "doc-cas-old").expect("已有远端已知 VV");
+    let before_head = load_head(&vault, "doc-cas-old").expect("head");
+
+    begin_with(&vault, 6, "src2", &expectation(&["doc-cas-old"]));
+    install_remote_known_clobber_trigger(&vault, "doc-cas-old");
+    let error = apply_baseline_batch(
+        &vault,
+        &[(
+            "doc-cas-old".to_owned(),
+            snapshot("doc-cas-old", PEER_B, "second"),
+        )],
+        &[],
+    )
+    .expect_err("陈旧旧值的 CAS 必须失败");
+
+    assert!(
+        matches!(&error, StorageError::SyncConflict { context } if context.contains("doc-cas-old")),
+        "必须是带文档上下文的 SyncConflict，实际 {error:?}"
+    );
+    assert_eq!(
+        meta_remote_known_bytes(&vault, "doc-cas-old"),
+        Some(before_known),
+        "CAS 失败后已有值必须保持原样（不能变小，也不能被本批并集覆盖）"
+    );
+    assert_eq!(
+        load_head(&vault, "doc-cas-old").expect("head").head_seq,
+        before_head.head_seq,
+        "整批回滚：head 不得推进"
+    );
+}
+
+/// 正常路径是单调并集：普通 pull 已记录的远端 ops（本批快照里没有）必须保留。
+#[test]
+fn baseline_remote_known_write_unions_with_normal_pull_evidence() {
+    let (_dir, vault) = fresh_vault();
+    let mut pulled = VersionVector::new();
+    pulled.insert(0xA1, 2);
+    vault
+        .exec(
+            "INSERT INTO meta(key,value) VALUES(?1,?2)",
+            &[
+                Value::Text("sync.remote_known_vv.doc-union".to_owned()),
+                Value::Blob(pulled.encode()),
+            ],
+        )
+        .expect("预置普通 pull 的远端已知 VV");
+
+    begin_with(&vault, 7, "src", &expectation(&["doc-union"]));
+    apply_baseline_batch(
+        &vault,
+        &[(
+            "doc-union".to_owned(),
+            snapshot("doc-union", PEER_B, "remote body"),
+        )],
+        &[],
+    )
+    .expect("baseline apply");
+
+    let known = meta_remote_known(&vault, "doc-union").expect("应用后必须有远端已知 VV");
+    assert_eq!(
+        known.get(&0xA1).copied(),
+        Some(2),
+        "普通 pull 的旧证据必须保留"
+    );
+    assert!(
+        known.get(&PEER_B).copied().unwrap_or(0) >= 1,
+        "本批快照的 VV 必须并入"
+    );
+    assert_eq!(
+        count(
+            &vault,
+            "SELECT count(*) FROM meta WHERE key='sync.remote_known_vv.doc-union'",
+            &[]
+        ),
+        1
+    );
+}
diff --git a/crates/storage/tests/exec_transaction_busy.rs b/crates/storage/tests/exec_transaction_busy.rs
new file mode 100644
index 0000000..21877f7
--- /dev/null
+++ b/crates/storage/tests/exec_transaction_busy.rs
@@ -0,0 +1,170 @@
+//! P2-7：`exec_transaction` 用 `BEGIN IMMEDIATE` 开启事务（与批量 inbox 共用
+//! `begin_immediate`）。
+//!
+//! 为什么必须是 Immediate：`exec_transaction` 承载 A 事务/资产/repack/finish，调用方
+//! 习惯把写语句放在最前面，但**没有任何断言**保证这一点；只要事务里出现"先读后写"，
+//! deferred 的锁升级就不走 `busy_timeout`（SQLite 的锁升级不调用 busy handler），会在
+//! 预期之外立即 `SQLITE_BUSY`。
+//!
+//! 这些用例锁住外部可观测的后果：并发写者占着写锁时整笔事务失败、零半写、锁释放后
+//! 同一批语句可重放且守卫照旧生效。
+
+mod support;
+
+use support::*;
+
+use menghunote_storage::{AffectedGuard, Statement, StorageError, Value, Vault};
+
+/// 断言错误是 `SQLITE_BUSY`（而不是守卫失败 / 约束冲突）。
+fn assert_busy(error: &StorageError) {
+    assert!(
+        matches!(
+            error,
+            StorageError::Sqlite(rusqlite::Error::SqliteFailure(code, _))
+                if code.code == rusqlite::ErrorCode::DatabaseBusy
+        ),
+        "并发写事务在场时必须整笔 SQLITE_BUSY，实际 {error:?}"
+    );
+}
+
+/// 普通 `meta` 键的 BLOB 值（测试断言用）。
+fn meta_blob(vault: &Vault, key: &str) -> Option<Vec<u8>> {
+    vault.get_meta(key).expect("读 meta")
+}
+
+/// 外部写事务占着写锁时，`exec_transaction` 必须整笔 `SQLITE_BUSY` 且零半写；
+/// 释放后同一批语句重放成功，语句顺序与守卫语义都不变。
+#[test]
+fn exec_transaction_under_foreign_write_lock_fails_busy_without_partial_write() {
+    let (dir, vault) = fresh_vault();
+    vault.set_meta("tx.cas", b"old").expect("预置 CAS 目标行");
+
+    // 本连接不等锁（生产是 5s）：让 BEGIN IMMEDIATE 的失败立刻返回，测试无需 sleep。
+    vault
+        .exec_batch("PRAGMA busy_timeout=0")
+        .expect("busy_timeout=0");
+
+    // 第二个 Vault（= 第二个连接）先占住写锁。
+    let holder = open_vault(&vault_path(&dir));
+    holder
+        .exec_batch("BEGIN IMMEDIATE")
+        .expect("外部连接占写锁");
+
+    // 第一条插入一行（半写的证据），第二条 CAS 更新既有行（守卫要求恰好 1 行）。
+    let statements = [
+        Statement::new(
+            "INSERT INTO meta(key,value) VALUES(?1,?2)",
+            vec![
+                Value::Text("tx.half".to_owned()),
+                Value::Blob(b"1".to_vec()),
+            ],
+        ),
+        Statement::new(
+            "UPDATE meta SET value=?2 WHERE key=?1",
+            vec![
+                Value::Text("tx.cas".to_owned()),
+                Value::Blob(b"new".to_vec()),
+            ],
+        ),
+    ];
+    let guards = [AffectedGuard::exactly(0, 1), AffectedGuard::exactly(1, 1)];
+
+    let error = vault
+        .exec_transaction(&statements, &guards)
+        .expect_err("写锁被占时必须整笔失败");
+    assert_busy(&error);
+
+    holder.exec_batch("ROLLBACK").expect("释放外部写锁");
+    drop(holder);
+
+    assert_eq!(meta_blob(&vault, "tx.half"), None, "失败事务不得留下半写");
+    assert_eq!(
+        meta_blob(&vault, "tx.cas"),
+        Some(b"old".to_vec()),
+        "失败事务不得改动 CAS 目标"
+    );
+
+    // 锁释放后同一批重放：整笔提交，每句影响行数与顺序都不变。
+    let affected = vault
+        .exec_transaction(&statements, &guards)
+        .expect("锁释放后应成功");
+    assert_eq!(affected, vec![1, 1], "语句顺序与每句影响行数必须保持原样");
+    assert_eq!(meta_blob(&vault, "tx.half"), Some(b"1".to_vec()));
+    assert_eq!(meta_blob(&vault, "tx.cas"), Some(b"new".to_vec()));
+}
+
+/// `exec_transaction` 在 `BEGIN` 阶段就取写锁（`BEGIN IMMEDIATE`），不是等第一条语句。
+///
+/// 有写语句时 deferred 也会在第一条语句撞上同样的 `SQLITE_BUSY`（`busy_timeout=0`），
+/// 两种事务模式的结果无法区分；**空语句事务**是唯一能从外部观测到的差异：
+///
+/// - deferred：`BEGIN` 不碰写锁，`COMMIT` 空事务也成功 → `Ok(vec![])`；
+/// - Immediate：`BEGIN` 就要写锁 → 占锁期间直接 `SQLITE_BUSY`。
+///
+/// 这也解释了为什么上面的"零半写"用例本身不足以证明模式切换。
+#[test]
+fn exec_transaction_takes_write_lock_at_begin() {
+    let (dir, vault) = fresh_vault();
+    vault
+        .exec_batch("PRAGMA busy_timeout=0")
+        .expect("busy_timeout=0");
+
+    let holder = open_vault(&vault_path(&dir));
+    holder
+        .exec_batch("BEGIN IMMEDIATE")
+        .expect("外部连接占写锁");
+
+    let error = vault
+        .exec_transaction(&[], &[])
+        .expect_err("占锁期间 BEGIN IMMEDIATE 必须失败");
+    assert_busy(&error);
+
+    holder.exec_batch("ROLLBACK").expect("释放外部写锁");
+    drop(holder);
+
+    assert_eq!(
+        vault
+            .exec_transaction(&[], &[])
+            .expect("释放后空事务应成功"),
+        Vec::<usize>::new(),
+        "空事务返回空的受影响行数列表"
+    );
+}
+
+/// 占锁期间守卫错误也不该抢先暴露：整笔事务在 `BEGIN` 就失败，不会执行任何语句。
+#[test]
+fn exec_transaction_lock_failure_precedes_statement_effects() {
+    let (dir, vault) = fresh_vault();
+    vault.set_meta("tx.cas", b"old").expect("预置 CAS 目标行");
+    vault
+        .exec_batch("PRAGMA busy_timeout=0")
+        .expect("busy_timeout=0");
+
+    let holder = open_vault(&vault_path(&dir));
+    holder
+        .exec_batch("BEGIN IMMEDIATE")
+        .expect("外部连接占写锁");
+
+    // 守卫期望 0 行，但语句实际会影响 1 行：若事务能开始，会先写入再报 GuardFailed。
+    let statements = [Statement::new(
+        "UPDATE meta SET value=?2 WHERE key=?1",
+        vec![
+            Value::Text("tx.cas".to_owned()),
+            Value::Blob(b"new".to_vec()),
+        ],
+    )];
+    let guards = [AffectedGuard::exactly(0, 0)];
+    let error = vault
+        .exec_transaction(&statements, &guards)
+        .expect_err("写锁被占时必须整笔失败");
+    assert_busy(&error);
+
+    holder.exec_batch("ROLLBACK").expect("释放外部写锁");
+    drop(holder);
+
+    assert_eq!(
+        meta_blob(&vault, "tx.cas"),
+        Some(b"old".to_vec()),
+        "失败事务不得写入任何字节"
+    );
+}
diff --git a/crates/storage/tests/journal.rs b/crates/storage/tests/journal.rs
index 2c2d03a..f2dcceb 100644
--- a/crates/storage/tests/journal.rs
+++ b/crates/storage/tests/journal.rs
@@ -1,13 +1,21 @@
 //! 未 ACK journal 与背压：连续前缀、durable 后裁剪、慢盘合并、失败不丢已 ACK 部分。
+//!
+//! 另覆盖 P1-3 的 head 过期契约（见 `crates/storage/src/journal.rs` 模块头）：
+//! 外部推进 head 后 flush 必须返回**可识别**的终态错误而不是裸 `HeadConflict` 或挂死；
+//! 按契约重建 sink 后新条目可正常 durable；旧 journal 未 durable 条目被**显式交还**，
+//! 既不写库也不静默丢弃。
 
 mod support;
 
+use std::sync::atomic::{AtomicUsize, Ordering};
+use std::sync::Arc;
+
 use loro::VersionVector;
 use support::*;
 
 use menghunote_storage::{
-    load_body_doc, load_head, DurableSink, FlushReport, JournalEntry, JournalOrigin, JournalWriter,
-    StorageError, VaultJournalSink,
+    append_updates, load_body_doc, load_head, DurableSink, FlushReport, JournalEntry,
+    JournalOrigin, JournalWriter, StorageError, UpdateRecord, VaultJournalSink,
 };
 
 /// 可注入慢盘/失败的 sink：记录每次 flush 的批大小，便于断言「合并」。
@@ -48,6 +56,28 @@ impl DurableSink for ScriptedSink {
         }
         Ok(self.seq)
     }
+
+    fn head_seq(&self) -> u64 {
+        self.seq
+    }
+}
+
+/// 计数包装：证明 stale 之后 writer **不再**触碰 sink（不重试）。
+#[derive(Debug)]
+struct CountingSink {
+    inner: VaultJournalSink,
+    attempts: Arc<AtomicUsize>,
+}
+
+impl DurableSink for CountingSink {
+    fn write_prefix(&mut self, entries: &[JournalEntry]) -> Result<u64, StorageError> {
+        self.attempts.fetch_add(1, Ordering::SeqCst);
+        self.inner.write_prefix(entries)
+    }
+
+    fn head_seq(&self) -> u64 {
+        self.inner.head_seq()
+    }
 }
 
 fn entry(tag: &str) -> JournalEntry {
@@ -282,3 +312,197 @@ fn sink_holds_head_watermark_and_rejects_mixed_docs() {
     let error = sink.write_prefix(&mixed).expect_err("混文档必须拒绝");
     assert!(matches!(error, StorageError::Corrupt { .. }));
 }
+
+/// P1-3：外部推进 head 后，flush 必须给出**可识别**的终态错误，不重试、不挂死，
+/// 且旧条目既没写库也没被静默丢掉；按契约重建 sink 后新条目可正常 durable。
+#[test]
+fn stale_head_is_terminal_then_rebuild_hands_back_entries_and_new_entries_flush() {
+    let (_dir, vault) = fresh_vault();
+    let doc = seed_note(&vault, "note-1", "doc-1");
+
+    // 本地编辑：按**空基准**导出，这就是 writer 手里的字节（追加时的 head 快照）。
+    doc.insert_scalars(0, "甲").expect("编辑");
+    let local = doc
+        .pending_update(&VersionVector::default())
+        .expect("导出本地增量");
+
+    // writer 先按 head 0 建好 sink 并提交条目（真实路径同序：sink 先于外部推进）。
+    let attempts = Arc::new(AtomicUsize::new(0));
+    let sink = CountingSink {
+        inner: VaultJournalSink::new(vault.clone(), "doc-1", 0),
+        attempts: Arc::clone(&attempts),
+    };
+    let mut writer = JournalWriter::new(sink, 4, 4);
+    assert_eq!(writer.durable_seq(), 0, "空 journal 时报告 sink 的真实水位");
+    writer
+        .submit(JournalEntry {
+            doc_id: "doc-1".to_owned(),
+            batch_id: "stale-batch".to_owned(),
+            update: local.clone(),
+            origin: JournalOrigin::Local,
+        })
+        .expect("submit");
+
+    // 外部 writer（同步引擎 integrate / 另一个 Vault）在同一 doc 上推进 head。
+    doc.insert_scalars(doc.scalar_len(), "乙").expect("编辑");
+    let external = doc
+        .pending_update(&VersionVector::default())
+        .expect("导出外部增量");
+    let advanced = append_updates(
+        &vault,
+        "doc-1",
+        0,
+        &[UpdateRecord::new("external-1", external)],
+    )
+    .expect("外部推进 head");
+    assert_eq!(advanced.head_seq, 1);
+
+    // 1) 第一次 flush：终态错误，而不是裸 HeadConflict。
+    let error = writer.flush().expect_err("head 已被推进，flush 必须失败");
+    match &error {
+        StorageError::JournalHeadStale {
+            doc_id,
+            expected,
+            actual,
+            pending,
+        } => {
+            assert_eq!(doc_id, "doc-1");
+            assert_eq!(*expected, 0, "错误必须带上写入用的 CAS 基准");
+            assert_eq!(*actual, 1, "错误必须带上库里真实的 head");
+            assert_eq!(*pending, 1, "错误必须说明有多少条未 durable 条目受影响");
+        }
+        other => panic!("期望 JournalHeadStale，实际 {other:?}"),
+    }
+    assert!(
+        !matches!(&error, StorageError::HeadConflict { .. }),
+        "journal 层不得把裸 HeadConflict 漏给调用方"
+    );
+    assert!(
+        error.requires_session_rebuild(),
+        "CAS 过期只能靠重建 session"
+    );
+    assert!(!error.is_retryable(), "CAS 过期不是可重试错误");
+    assert_eq!(writer.pending_len(), 1, "失败不得丢条目");
+    assert_eq!(writer.durable_seq(), 0, "失败不得推进 durable 水位");
+    assert_eq!(writer.flushed_entries(), 0);
+    assert!(writer.is_stale());
+    assert_eq!(
+        attempts.load(Ordering::SeqCst),
+        1,
+        "CAS 冲突只尝试一次，不自动重试"
+    );
+
+    // 2) 再 flush / flush_all：幂等返回同一终态错误，且**不再碰数据库**（不会挂死、不刷屏）。
+    for _ in 0..2 {
+        let again = writer.flush().expect_err("stale 必须继续报错");
+        assert!(
+            matches!(again, StorageError::JournalHeadStale { pending: 1, .. }),
+            "实际 {again:?}"
+        );
+        assert_eq!(
+            attempts.load(Ordering::SeqCst),
+            1,
+            "stale 之后不得重试 sink"
+        );
+    }
+    assert!(writer.flush_all().is_err(), "排空也必须明确失败");
+
+    // 3) stale 期间 submit 被拒绝：writer 已冻结，新条目必须交给重建后的 writer。
+    let blocked = writer
+        .submit(JournalEntry::local("doc-1", "later", local.clone()))
+        .expect_err("stale 时 submit 必须被拒绝");
+    assert!(
+        matches!(blocked, StorageError::JournalHeadStale { pending: 1, .. }),
+        "实际 {blocked:?}"
+    );
+    assert_eq!(writer.pending_len(), 1, "被拒的条目不能进 journal");
+
+    // 4) 按契约重建 sink（新 head_seq）：旧条目**显式交还**，journal 清空，水位对齐新基准。
+    let handed_back = writer
+        .rebuild_sink(CountingSink {
+            inner: VaultJournalSink::new(vault.clone(), "doc-1", 1),
+            attempts: Arc::clone(&attempts),
+        })
+        .expect("stale 后重建 sink");
+    assert_eq!(handed_back.len(), 1, "旧条目必须原样交还，不得静默丢弃");
+    assert_eq!(handed_back[0].batch_id, "stale-batch");
+    assert_eq!(handed_back[0].update, local);
+    assert_eq!(writer.pending_len(), 0, "交还后 journal 清空");
+    assert!(!writer.is_stale());
+    assert_eq!(writer.durable_seq(), 1, "重建后水位 = 新 sink 的 CAS 基准");
+
+    // 旧条目没有被偷偷写进库：库里仍只有外部那一条。
+    assert_eq!(
+        count(
+            &vault,
+            "SELECT count(*) FROM loro_updates WHERE doc_id = 'doc-1'",
+            &[]
+        ),
+        1,
+        "过期条目不得写入 loro_updates"
+    );
+    let (replayed, _) = load_body_doc(&vault, "doc-1", PEER_A, None).expect("重放");
+    assert_eq!(replayed.text(), "甲乙", "只有外部那一条落库");
+
+    // 5) 重建：用新 head 的 VV 重新导出（会话层的 ③），新条目 flush 成功且水位正确。
+    let base = advanced.version_vector().expect("解码新 head 的 VV");
+    doc.insert_scalars(doc.scalar_len(), "丙").expect("编辑");
+    let fresh = doc.pending_update(&base).expect("按新 head 重新导出增量");
+    writer
+        .submit(JournalEntry {
+            doc_id: "doc-1".to_owned(),
+            batch_id: "fresh-1".to_owned(),
+            update: fresh,
+            origin: JournalOrigin::Local,
+        })
+        .expect("重建后可 submit");
+
+    let report = writer.flush().expect("重建后的新条目必须能 flush");
+    assert_eq!(
+        report,
+        FlushReport {
+            entries: 1,
+            durable_seq: 2
+        }
+    );
+    assert_eq!(writer.durable_seq(), 2);
+    assert_eq!(writer.flushed_entries(), 1);
+    assert_eq!(writer.pending_len(), 0);
+    assert_eq!(attempts.load(Ordering::SeqCst), 2, "重建后只多一次真实写入");
+    assert_eq!(
+        count(
+            &vault,
+            "SELECT count(*) FROM loro_updates WHERE doc_id = 'doc-1'",
+            &[]
+        ),
+        2
+    );
+    assert_eq!(load_head(&vault, "doc-1").expect("head").head_seq, 2);
+    let (replayed, _) = load_body_doc(&vault, "doc-1", PEER_A, None).expect("重放");
+    assert_eq!(replayed.text(), "甲乙丙", "重建 + 重导出的内容完整");
+}
+
+/// 未过期时不允许重建 sink：那会把仍可写入的条目丢掉（契约的反向保护）。
+#[test]
+fn rebuild_sink_is_rejected_while_not_stale() {
+    let (_dir, vault) = fresh_vault();
+    let doc = seed_note(&vault, "note-1", "doc-1");
+    doc.insert_scalars(0, "甲").expect("编辑");
+    let update = doc.pending_update(&VersionVector::default()).expect("增量");
+
+    let mut writer = JournalWriter::new(VaultJournalSink::new(vault.clone(), "doc-1", 0), 4, 4);
+    writer
+        .submit(JournalEntry::local("doc-1", "b1", update))
+        .expect("submit");
+    assert!(!writer.is_stale());
+
+    let error = writer
+        .rebuild_sink(VaultJournalSink::new(vault.clone(), "doc-1", 0))
+        .expect_err("未 stale 时重建必须被拒绝");
+    assert!(
+        matches!(error, StorageError::Corrupt { .. }),
+        "实际 {error:?}"
+    );
+    assert_eq!(writer.pending_len(), 1, "拒绝重建不得动 journal");
+    assert_eq!(writer.flush().expect("正常 flush 仍可用").entries, 1);
+}
diff --git a/crates/storage/tests/meta_value_types.rs b/crates/storage/tests/meta_value_types.rs
new file mode 100644
index 0000000..aabdc95
--- /dev/null
+++ b/crates/storage/tests/meta_value_types.rs
@@ -0,0 +1,283 @@
+//! P2-6：`meta.value` 的三个读取入口必须只有**一套**语义 —— 只认 BLOB。
+//!
+//! | 入口 | 实际代码路径 |
+//! | --- | --- |
+//! | [`menghunote_storage::Vault::get_meta`] | actor 线程上的 `GetMeta` 命令 |
+//! | `sync_bridge::read_meta` | `sync_cursor_state` / `take_inbox_ready` / `set_sync_log_id` 等同步桥读取 |
+//! | `sync_meta::read_meta_on` | worker 独占连接 / 批量 inbox 事务内的读取 |
+//!
+//! 为什么专门测非 BLOB：`meta.value` 是 `BLOB NOT NULL`（`schema/v1.sql` 里的 STRICT
+//! 表），正常写入路径**写不出** TEXT/INTEGER/REAL。能读出来只可能是"库被外部工具改坏
+//! 或 schema 漂移"，而旧行为对它有两种互相矛盾的解读：
+//!
+//! - `Vault::get_meta` 把 TEXT 当字节返回；
+//! - `sync_bridge::read_meta` / `sync_meta::read_meta_on` 把 TEXT 当**缺失**。
+//!
+//! 后者会静默绕过持久屏障与游标：`sync.log_id` 读不到就当"还没绑定 incarnation"
+//! （`take_inbox_ready` 静默返回空，`set_sync_log_id` 还会把损坏值覆盖掉，等于悄悄
+//! 换绑传代），`sync.repack_required` 读不到就当"不需要 repack"。
+//!
+//! 测试库用 `PRAGMA writable_schema` 把 `meta` 表改回非 STRICT，从而如实构造出这种
+//! 损坏状态（STRICT 表拒绝非 BLOB 写入，测试里没有别的办法造出 TEXT）。
+
+mod support;
+
+use std::path::Path;
+
+use support::*;
+
+use menghunote_storage::{
+    advance_integrated_prefix, repack_required, set_repack_required, set_sync_log_id,
+    sync_cursor_state, take_inbox_ready, StorageError, Value, Vault, META_SYNC_LOG_ID,
+    META_SYNC_REPACK_REQUIRED,
+};
+
+/// 测试用的合法 relay log incarnation（16B BLOB）。
+const LOG_ID: [u8; 16] = [0x2f; 16];
+
+/// 把测试库的 `meta` 表从 STRICT 改成非 STRICT。
+///
+/// 只改 `sqlite_master` 里那条 DDL（去掉唯一的 `STRICT` 后缀），其余列定义逐字保留；
+/// 之后的连接会按非 STRICT 解析这张表，于是能写入 TEXT/INTEGER/REAL —— 正是"库被外部
+/// 改坏"的形状。
+fn relax_meta_table(path: &Path) {
+    let conn = rusqlite::Connection::open(path).expect("打开原始连接");
+    conn.execute_batch("PRAGMA writable_schema=ON")
+        .expect("打开 writable_schema");
+    let ddl: String = conn
+        .query_row(
+            "SELECT sql FROM sqlite_master WHERE name='meta'",
+            [],
+            |row| row.get(0),
+        )
+        .expect("读 meta 表 DDL");
+    let relaxed = ddl.replacen(" STRICT", "", 1);
+    assert_ne!(relaxed, ddl, "meta 表 DDL 应带 STRICT：{ddl}");
+    conn.execute(
+        "UPDATE sqlite_master SET sql=?1 WHERE name='meta'",
+        [relaxed],
+    )
+    .expect("改写 meta 表 DDL");
+    conn.execute_batch("PRAGMA writable_schema=OFF")
+        .expect("关闭 writable_schema");
+    drop(conn);
+}
+
+/// 建一个 vault，把 `meta` 表改成非 STRICT 后重新打开。
+///
+/// 重新打开走 [`Vault::open`]：`user_version` 已是 `SCHEMA_VERSION`，迁移不会重跑，
+/// 所以非 STRICT 的 `meta` 表被原样沿用。
+fn vault_with_typed_meta() -> (tempfile::TempDir, Vault) {
+    let dir = temp_dir();
+    let path = vault_path(&dir);
+    {
+        let vault = open_vault(&path);
+        // 先写一版合法 BLOB，确保 schema 与 WAL 都已落盘，再改表结构。
+        set_sync_log_id(&vault, &LOG_ID).expect("预置合法 BLOB");
+    }
+    relax_meta_table(&path);
+    let vault = open_vault(&path);
+    (dir, vault)
+}
+
+/// 把一个 meta 键写成指定 SQLite 类型（绕过 Vault 的 BLOB 写入路径）。
+fn write_typed_meta(vault: &Vault, key: &str, value: Value) {
+    vault
+        .exec(
+            "INSERT INTO meta(key,value) VALUES(?1,?2) \
+             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
+            &[Value::Text(key.to_owned()), value],
+        )
+        .expect("写入指定类型的 meta");
+}
+
+/// `meta.value` 的实际 SQLite 类型名（`typeof()`，用于确认构造出的损坏状态）。
+fn value_type(vault: &Vault, key: &str) -> String {
+    let rows = vault
+        .query(
+            "SELECT typeof(value) FROM meta WHERE key=?1",
+            &[Value::Text(key.to_owned())],
+        )
+        .expect("读 typeof(value)");
+    match rows.first().and_then(|row| row.first()) {
+        Some(Value::Text(name)) => name.clone(),
+        other => panic!("typeof(value) 类型异常：{other:?}"),
+    }
+}
+
+/// 断言结果是 [`StorageError::Corrupt`]，返回错误上下文（便于继续断言 key 与实际类型）。
+fn corrupt<T: std::fmt::Debug>(result: Result<T, StorageError>, label: &str) -> String {
+    match result {
+        Ok(value) => panic!("{label} 必须报损坏，实际 Ok({value:?})"),
+        Err(StorageError::Corrupt { context }) => context,
+        Err(other) => panic!("{label} 必须是 Corrupt，实际 {other:?}"),
+    }
+}
+
+/// 同一 key 写成 TEXT：三个入口必须给出**同一个**损坏结论。
+#[test]
+fn text_meta_is_corrupt_from_all_three_entry_points() {
+    let (_dir, vault) = vault_with_typed_meta();
+    write_typed_meta(&vault, META_SYNC_LOG_ID, Value::Text("2f".repeat(16)));
+    assert_eq!(
+        value_type(&vault, META_SYNC_LOG_ID),
+        "text",
+        "构造出的值必须是 TEXT"
+    );
+
+    // 入口 1：Vault::get_meta（actor 线程）。旧行为把 TEXT 直接当字节返回。
+    let from_actor = corrupt(vault.get_meta(META_SYNC_LOG_ID), "Vault::get_meta");
+    // 入口 2：sync_bridge::read_meta（sync_cursor_state 读 sync.log_id）。旧行为当缺失。
+    let from_bridge = corrupt(sync_cursor_state(&vault), "sync_bridge::read_meta");
+    // 入口 3：sync_meta::read_meta_on（前缀推进在 worker 连接上读 sync.log_id）。
+    let from_meta = corrupt(advance_integrated_prefix(&vault), "sync_meta::read_meta_on");
+
+    for (label, context) in [
+        ("Vault::get_meta", &from_actor),
+        ("sync_bridge::read_meta", &from_bridge),
+        ("sync_meta::read_meta_on", &from_meta),
+    ] {
+        assert!(
+            context.contains(META_SYNC_LOG_ID),
+            "{label} 的错误信息必须点名 key：{context}"
+        );
+        assert!(
+            context.contains("TEXT") && context.contains("BLOB"),
+            "{label} 的错误信息必须同时给出实际类型与期望类型：{context}"
+        );
+    }
+    // 三处共用同一判定函数，连错误文案都不该有差别。
+    assert_eq!(
+        from_actor, from_bridge,
+        "actor 与 sync_bridge 的结论必须一致"
+    );
+    assert_eq!(from_actor, from_meta, "actor 与 sync_meta 的结论必须一致");
+}
+
+/// BLOB（包括空 BLOB）继续按原语义读出，前缀推进不受影响。
+#[test]
+fn blob_meta_reads_consistently_from_all_three_entry_points() {
+    let (_dir, vault) = fresh_vault();
+    set_sync_log_id(&vault, &LOG_ID).expect("写 BLOB sync.log_id");
+
+    assert_eq!(
+        vault.get_meta(META_SYNC_LOG_ID).expect("读 BLOB meta"),
+        Some(LOG_ID.to_vec()),
+        "BLOB 必须逐字节读出"
+    );
+    let state = sync_cursor_state(&vault).expect("读游标状态");
+    assert_eq!(
+        state.log_id,
+        Some(LOG_ID),
+        "sync_bridge 必须读到同一个 BLOB"
+    );
+    assert_eq!(state.received_seq, 0);
+    assert_eq!(state.integrated_seq, 0);
+    assert_eq!(state.confirmed_highwater, None);
+
+    advance_integrated_prefix(&vault).expect("BLOB 下前缀推进是合法 no-op");
+
+    set_repack_required(&vault, true).expect("写 repack 屏障");
+    assert!(
+        repack_required(&vault).expect("读 repack 屏障"),
+        "BLOB 屏障必须生效"
+    );
+    set_repack_required(&vault, false).expect("清 repack 屏障");
+    assert!(!repack_required(&vault).expect("读 repack 屏障"));
+}
+
+/// 缺失的键在三个入口都是"没有"，不是损坏。
+#[test]
+fn missing_meta_reads_as_absent_from_all_three_entry_points() {
+    let (_dir, vault) = fresh_vault();
+
+    assert_eq!(vault.get_meta(META_SYNC_LOG_ID).expect("读缺失 key"), None);
+    let state = sync_cursor_state(&vault).expect("读游标状态");
+    assert_eq!(state.log_id, None);
+    assert_eq!(state.received_seq, 0);
+    assert_eq!(state.integrated_seq, 0);
+    assert_eq!(state.confirmed_highwater, None);
+    advance_integrated_prefix(&vault).expect("缺失时前缀推进是 no-op");
+    assert!(!repack_required(&vault).expect("缺失屏障读成 false"));
+    assert_eq!(
+        vault
+            .get_meta(META_SYNC_REPACK_REQUIRED)
+            .expect("读缺失屏障"),
+        None
+    );
+    assert!(
+        take_inbox_ready(&vault, 8)
+            .expect("没有 incarnation 时 ready 为空")
+            .is_empty(),
+        "缺失 sync.log_id 时 ready 必须为空（不回退成不过滤 log）"
+    );
+}
+
+/// 损坏的 `sync.log_id` 不能被当成"还没绑定 incarnation"静默吞掉。
+///
+/// 这是 P2-6 的实际风险点：旧行为下 `take_inbox_ready` 会静默返回空（pull 永久卡住），
+/// `set_sync_log_id` 会把损坏值覆盖成新值（悄悄换绑传代，旧行混进新代）。
+#[test]
+fn text_log_id_blocks_ready_and_rebinding_instead_of_silently_passing() {
+    let (_dir, vault) = vault_with_typed_meta();
+    write_typed_meta(&vault, META_SYNC_LOG_ID, Value::Text("2f".repeat(16)));
+
+    let context = corrupt(take_inbox_ready(&vault, 8), "take_inbox_ready");
+    assert!(
+        context.contains(META_SYNC_LOG_ID),
+        "错误必须点名 sync.log_id：{context}"
+    );
+    let context = corrupt(set_sync_log_id(&vault, &LOG_ID), "set_sync_log_id");
+    assert!(
+        context.contains(META_SYNC_LOG_ID),
+        "错误必须点名 sync.log_id：{context}"
+    );
+
+    // 损坏值仍在库里：没有被"缺失 → 写入新版"的路径覆盖掉。
+    assert_eq!(value_type(&vault, META_SYNC_LOG_ID), "text");
+}
+
+/// INTEGER / REAL / TEXT 全部按损坏处理（不是只有 TEXT 特殊）。
+#[test]
+fn non_blob_meta_types_are_corrupt() {
+    let (_dir, vault) = vault_with_typed_meta();
+    // NULL 造不出来：`meta.value` 是 NOT NULL，非 STRICT 表同样拒绝。
+    for (value, expected_type) in [
+        (Value::Integer(7), "INTEGER"),
+        (Value::Real(1.5), "REAL"),
+        (Value::Text("7".to_owned()), "TEXT"),
+    ] {
+        write_typed_meta(&vault, META_SYNC_LOG_ID, value);
+        assert_eq!(
+            value_type(&vault, META_SYNC_LOG_ID),
+            expected_type.to_lowercase()
+        );
+        let context = corrupt(vault.get_meta(META_SYNC_LOG_ID), "Vault::get_meta");
+        assert!(
+            context.contains(expected_type),
+            "错误必须报实际类型 {expected_type}：{context}"
+        );
+        let context = corrupt(sync_cursor_state(&vault), "sync_bridge::read_meta");
+        assert!(
+            context.contains(expected_type),
+            "sync_bridge 也必须报实际类型 {expected_type}：{context}"
+        );
+    }
+}
+
+/// 屏障键本身：损坏的 `sync.repack_required` 不得读成"不需要 repack"。
+#[test]
+fn repack_barrier_is_not_silently_false_when_corrupt() {
+    let (_dir, vault) = vault_with_typed_meta();
+    write_typed_meta(
+        &vault,
+        META_SYNC_REPACK_REQUIRED,
+        Value::Text("1".to_owned()),
+    );
+
+    let context = corrupt(repack_required(&vault), "repack_required");
+    assert!(
+        context.contains(META_SYNC_REPACK_REQUIRED),
+        "错误必须点名屏障 key：{context}"
+    );
+}
diff --git a/crates/storage/tests/remote_known.rs b/crates/storage/tests/remote_known.rs
new file mode 100644
index 0000000..26b0ad7
--- /dev/null
+++ b/crates/storage/tests/remote_known.rs
@@ -0,0 +1,191 @@
+//! P2-5 / P2-8：远端已知 VV 的单调写入与「来源标记」语义。
+//!
+//! 这里只走**公开**入口：`VaultJournalSink`（每次 flush 的 JournalOrigin 决定来源）
+//! 与 `append_updates`（全部本地）。远端已知 VV 是 `meta` 里的非破坏性本地证据：
+//!
+//! - 本地写入（含已 ACK 的本地编辑）**不扩大**它；
+//! - 只有远端条目把 `end_vv - 上一条 end_vv` 记进去；
+//! - 没有出站意图的 flush（slots 空）不产生任何 outbox 行。
+
+mod support;
+
+use support::*;
+
+use loro::VersionVector;
+
+use menghunote_storage::{
+    append_updates, create_body_doc, load_head, sync_cursor_state, BodyDoc, DurableSink,
+    JournalEntry, SyncContext, UpdateRecord, Value, Vault, VaultJournalSink,
+};
+
+const CTX: SyncContext = SyncContext {
+    peer_id: 0xA1CE_0001,
+    key_epoch: 1,
+};
+
+fn remote_known(vault: &Vault, doc_id: &str) -> Option<VersionVector> {
+    let rows = vault
+        .query(
+            "SELECT value FROM meta WHERE key=?1",
+            &[Value::Text(format!("sync.remote_known_vv.{doc_id}"))],
+        )
+        .expect("meta 查询");
+    match rows.first().and_then(|row| row.first()) {
+        Some(Value::Blob(bytes)) => Some(VersionVector::decode(bytes).expect("解码远端已知 VV")),
+        None => None,
+        other => panic!("remote_known 值异常：{other:?}"),
+    }
+}
+
+fn scalar(vault: &Vault, sql: &str) -> i64 {
+    let rows = vault.query(sql, &[]).expect("查询");
+    match rows.first().and_then(|row| row.first()) {
+        Some(Value::Integer(value)) => *value,
+        other => panic!("期望整数结果，实际 {other:?}"),
+    }
+}
+
+/// 本地编辑的 `end_vv`（用于断言「本地写入不扩大远端已知 VV」）。
+fn vv_of(update: &menghunote_storage::PendingUpdate) -> VersionVector {
+    VersionVector::decode(&update.end_vv).expect("解码 end_vv")
+}
+
+fn local_entry(doc_id: &str, peer: u64, text: &str) -> (JournalEntry, VersionVector) {
+    let doc = BodyDoc::create(doc_id, peer).expect("创建 BodyDoc");
+    doc.insert_scalars(0, text).expect("本地编辑");
+    let update = doc.pending_update(&VersionVector::default()).expect("导出");
+    let vv = vv_of(&update);
+    (JournalEntry::local(doc_id, "local-batch", update), vv)
+}
+
+fn remote_entry(
+    doc_id: &str,
+    peer: u64,
+    base: &VersionVector,
+    text: &str,
+) -> (JournalEntry, VersionVector) {
+    let doc = BodyDoc::create(doc_id, peer).expect("创建 BodyDoc");
+    doc.insert_scalars(0, text).expect("远端编辑");
+    let update = doc.pending_update(base).expect("导出远端增量");
+    let vv = vv_of(&update);
+    (JournalEntry::remote(doc_id, "remote-batch", update), vv)
+}
+
+/// 纯本地 flush（显式 `OriginMarks::AllLocal` 对应的公开路径）：不创建远端已知 VV。
+#[test]
+fn local_append_never_creates_remote_known() {
+    let (_dir, vault) = fresh_vault();
+    create_body_doc(&vault, "doc-local").expect("loro_docs 行");
+    let doc = BodyDoc::create("doc-local", PEER_A).expect("创建 BodyDoc");
+    doc.insert_scalars(0, "local only").expect("本地编辑");
+    let current = load_head(&vault, "doc-local").expect("head");
+    let update = doc
+        .pending_update(&current.version_vector().expect("head vv"))
+        .expect("导出");
+
+    append_updates(
+        &vault,
+        "doc-local",
+        current.head_seq,
+        &[UpdateRecord::new("local-batch", update)],
+    )
+    .expect("A 事务");
+
+    assert_eq!(
+        remote_known(&vault, "doc-local"),
+        None,
+        "本地写入即使已经进 outbox 也不能扩大远端已知 VV"
+    );
+    assert_eq!(scalar(&vault, "SELECT count(*) FROM sync_outbox"), 0);
+}
+
+/// 远端条目（PerRecord 中 `true` 的那条）只把真正的远端增量记入远端已知 VV；
+/// 同批的本地条目不记入。
+#[test]
+fn mixed_flush_extends_remote_known_only_for_remote_entries() {
+    let (_dir, vault) = fresh_vault();
+    create_body_doc(&vault, "doc-mixed").expect("loro_docs 行");
+
+    let (local, local_vv) = local_entry("doc-mixed", PEER_A, "local");
+    let (remote, remote_vv) = remote_entry("doc-mixed", PEER_B, &local_vv, "remote");
+    let expected_remote = {
+        // 远端记录带来的 ops = remote_vv - local_vv。
+        let mut added = VersionVector::new();
+        for span in remote_vv.sub_iter(&local_vv) {
+            added.insert(span.peer, span.counter.end);
+        }
+        added
+    };
+
+    let mut sink = VaultJournalSink::new(vault.clone(), "doc-mixed", 0).with_sync_context(CTX);
+    let head = sink.write_prefix(&[local, remote]).expect("混合 flush");
+    assert_eq!(head, 2);
+
+    let known = remote_known(&vault, "doc-mixed").expect("有远端条目就必须写");
+    assert_eq!(known, expected_remote, "只能记入远端那一小段");
+    assert_eq!(
+        scalar(&vault, "SELECT count(*) FROM sync_outbox"),
+        1,
+        "本地条目产生 1 条意图，远端条目绝不能产生意图"
+    );
+    assert_eq!(sync_cursor_state(&vault).expect("cursor").received_seq, 0);
+}
+
+/// 纯远端 flush（`slots` 为空 = 没有出站意图）：扩展远端已知 VV，且不产生 outbox。
+#[test]
+fn remote_only_flush_with_empty_slots_has_no_outbox() {
+    let (_dir, vault) = fresh_vault();
+    create_body_doc(&vault, "doc-remote").expect("loro_docs 行");
+    let (remote, remote_vv) =
+        remote_entry("doc-remote", PEER_B, &VersionVector::default(), "remote");
+
+    // 不带 sync context：slots 就是空切片（= 没有任何意图），过去与
+    // 「remote_marks 空 = 全部本地」共用一个形状；现在来源由 OriginMarks 显式表达。
+    let mut sink = VaultJournalSink::new(vault.clone(), "doc-remote", 0);
+    let head = sink.write_prefix(&[remote]).expect("远端 flush");
+    assert_eq!(head, 1);
+
+    assert_eq!(remote_known(&vault, "doc-remote"), Some(remote_vv));
+    assert_eq!(
+        scalar(&vault, "SELECT count(*) FROM sync_outbox"),
+        0,
+        "slots 空 = 不产生任何出站意图"
+    );
+}
+
+/// 同一 flush 连续两条远端记录：记入的是整段远端增量（`end - 前一条 end`），
+/// 不是只记最后一条的绝对 VV。
+#[test]
+fn consecutive_remote_entries_record_the_whole_remote_delta() {
+    let (_dir, vault) = fresh_vault();
+    create_body_doc(&vault, "doc-remote-2").expect("loro_docs 行");
+
+    let first_doc = BodyDoc::create("doc-remote-2", PEER_B).expect("创建 BodyDoc");
+    first_doc.insert_scalars(0, "one").expect("第一次远端编辑");
+    let first = first_doc
+        .pending_update(&VersionVector::default())
+        .expect("第一段");
+    let first_vv = vv_of(&first);
+
+    first_doc
+        .insert_scalars(first_doc.scalar_len(), " two")
+        .expect("第二次远端编辑");
+    let second = first_doc.pending_update(&first_vv).expect("第二段");
+    let end_vv = vv_of(&second);
+
+    let mut sink = VaultJournalSink::new(vault.clone(), "doc-remote-2", 0);
+    sink.write_prefix(&[
+        JournalEntry::remote("doc-remote-2", "remote-1", first),
+        JournalEntry::remote("doc-remote-2", "remote-2", second),
+    ])
+    .expect("两段远端 flush");
+
+    assert_eq!(
+        remote_known(&vault, "doc-remote-2"),
+        Some(end_vv),
+        "整段远端增量都必须记入，不能只记最后一条"
+    );
+    // 显式断言两条都写了（来源只有 remote，没有任何本地产物）。
+    assert_eq!(scalar(&vault, "SELECT count(*) FROM loro_updates"), 2);
+    assert_eq!(scalar(&vault, "SELECT count(*) FROM sync_outbox"), 0);
+}
diff --git a/crates/storage/tests/sync_inbox_batch.rs b/crates/storage/tests/sync_inbox_batch.rs
index 9c04bc9..dc0352f 100644
--- a/crates/storage/tests/sync_inbox_batch.rs
+++ b/crates/storage/tests/sync_inbox_batch.rs
@@ -17,12 +17,15 @@ use menghunote_storage::{
     advance_integrated_prefix, append_updates_with_integrated_marks, count_inbox_ready, load_head,
     mark_integrated, mark_integrated_batch, mark_skipped_event_received, set_sync_log_id,
     store_inbox_verified, store_inbox_verified_batch, sync_cursor_state, take_inbox_ready,
-    StorageError, UpdateRecord, Value, Vault, VerifiedInboxRow, META_SYNC_LOG_ID,
-    META_SYNC_RECEIVED_SEQ, META_SYNC_SKIPPED_RECEIVED_PREFIX,
+    take_inbox_ready_where, StorageError, UpdateRecord, Value, Vault, VerifiedInboxRow,
+    META_SYNC_LOG_ID, META_SYNC_RECEIVED_SEQ, META_SYNC_SKIPPED_RECEIVED_PREFIX,
 };
 
 const LOG_ID: [u8; 16] = [0x3a; 16];
 
+/// 另一个 incarnation（restore 迁移后当前 `sync.log_id` 可能变成它，旧行仍然 durable）。
+const OTHER_LOG_ID: [u8; 16] = [0x5b; 16];
+
 fn bind_log(vault: &Vault) {
     set_sync_log_id(vault, &LOG_ID).expect("写 sync.log_id");
 }
@@ -855,3 +858,175 @@ fn batch_under_foreign_write_lock_fails_busy_without_partial_write() {
     assert_eq!(inbox_rows(&vault).len(), 1);
     assert_eq!(received(&vault), 1);
 }
+
+// ================================ incarnation 过滤（restore 迁移 log_id 后的旧行）
+
+/// 当前 incarnation 的 ready `relay_seq`（升序；`take_inbox_ready` 已按 log/seq 排序）。
+fn ready_seqs(vault: &Vault, limit: usize) -> Vec<u64> {
+    take_inbox_ready(vault, limit)
+        .expect("读 ready")
+        .into_iter()
+        .map(|row| row.relay_seq)
+        .collect()
+}
+
+/// 直接写 `meta` 原始字节（模拟 restore 的显式迁移 / 损坏状态，绕过 Vault 校验）。
+fn write_raw_meta(vault: &Vault, key: &str, value: &[u8]) {
+    vault
+        .exec(
+            "INSERT INTO meta(key,value) VALUES(?1,?2) \
+             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
+            &[Value::Text(key.to_owned()), Value::Blob(value.to_vec())],
+        )
+        .expect("写原始 meta");
+}
+
+/// 直接插入一条**其它 incarnation** 的 verified 行。
+///
+/// 生产入口 `store_inbox_verified*` 都先 `ensure_log_id`，跨代写入会被
+/// `SyncLogMismatch` 拒绝；这种 durable 状态只能由 restore 显式迁移 `sync.log_id`
+/// 后留下，所以测试直接用 SQL 准备，不经过被测试的过滤逻辑。
+fn insert_stale_incarnation_row(vault: &Vault, log_id: &[u8; 16], relay_seq: u64, seed: u8) {
+    vault
+        .exec(
+            "INSERT INTO sync_inbox(log_id,relay_seq,object_id,epoch,state,verified_payload) \
+             VALUES(?1,?2,?3,?4,'verified',?5)",
+            &[
+                Value::Blob(log_id.to_vec()),
+                Value::Integer(i64::try_from(relay_seq).expect("relay_seq 在 i64 内")),
+                Value::Blob(vec![seed; 32]),
+                Value::Integer(1),
+                Value::Blob(vec![seed; 16]),
+            ],
+        )
+        .expect("插入旧 incarnation 的 verified 行");
+}
+
+/// 当前 log A 有 verified 行、库里另有 log B 的 verified 行时：
+/// ready/count 只计 A；B 即使 `relay_seq <= received` 也不计入；received 前缀只按 A 推进。
+#[test]
+fn ready_and_count_only_include_current_log_incarnation() {
+    let (_dir, vault) = fresh_vault();
+    bind_log(&vault);
+    assert_eq!(store_inbox_verified_batch(&vault, &rows(2, 0)).unwrap(), 2);
+    assert_eq!(received(&vault), 2);
+
+    // 旧 incarnation B 的 1..=5 仍在库里（restore 迁移 log_id 后旧行不能被静默删除）。
+    for seq in 1..=5u64 {
+        insert_stale_incarnation_row(
+            &vault,
+            &OTHER_LOG_ID,
+            seq,
+            u8::try_from(seq).expect("seq 在 u8 内"),
+        );
+    }
+    // `set_sync_log_id` 的语义未变：生产入口仍拒绝跨代写入（restore 走显式迁移）。
+    assert!(
+        matches!(
+            set_sync_log_id(&vault, &OTHER_LOG_ID),
+            Err(StorageError::SyncLogMismatch { .. })
+        ),
+        "set_sync_log_id 必须继续拒绝跨代变更"
+    );
+
+    // B 的 seq1/seq2 都 <= received(2)，但 ready/count 只能看到 A 的 1/2。
+    assert_eq!(ready_seqs(&vault, 16), vec![1, 2]);
+    assert_eq!(
+        count_inbox_ready(&vault).unwrap(),
+        2,
+        "旧 incarnation 的行即使 relay_seq<=received 也不得计入"
+    );
+
+    // A 的 seq3 落库 → received 只推进到 3，不跨 B 的 1..=5。
+    assert_eq!(
+        store_inbox_verified_batch(&vault, &[row(3, 0x33, 8)]).unwrap(),
+        1
+    );
+    assert_eq!(received(&vault), 3, "received 前缀推进不跨 incarnation");
+    assert_eq!(ready_seqs(&vault, 16), vec![1, 2, 3]);
+    assert_eq!(count_inbox_ready(&vault).unwrap(), 3);
+
+    // 状态变化同样只作用于当前 log：集成 A 的 seq1 后 B 的行仍不 ready、不影响计数。
+    mark_integrated(&vault, &LOG_ID, 1).expect("集成 A seq1");
+    assert_eq!(ready_seqs(&vault, 16), vec![2, 3]);
+    assert_eq!(count_inbox_ready(&vault).unwrap(), 2);
+}
+
+/// `take_inbox_ready_where` 走同一条底层路径：谓词只能看到当前 log 的行。
+#[test]
+fn ready_where_path_filters_other_log_incarnation() {
+    let (_dir, vault) = fresh_vault();
+    bind_log(&vault);
+    assert_eq!(store_inbox_verified_batch(&vault, &rows(3, 0)).unwrap(), 3);
+    for seq in 1..=3u64 {
+        insert_stale_incarnation_row(
+            &vault,
+            &OTHER_LOG_ID,
+            seq,
+            0xb0 + u8::try_from(seq).unwrap_or(0),
+        );
+    }
+
+    // 恒真谓词 + 足够大的 limit：旧实现会把 B 的 3 行也交进来（6 条），现在必须只有 3 条。
+    let mut observed_log_ids = Vec::new();
+    let picked = take_inbox_ready_where(&vault, 16, |row| {
+        observed_log_ids.push(row.log_id);
+        true
+    })
+    .expect("where 路径读 ready");
+    assert_eq!(
+        picked.iter().map(|row| row.relay_seq).collect::<Vec<_>>(),
+        vec![1, 2, 3]
+    );
+    assert!(
+        observed_log_ids.iter().all(|log_id| *log_id == LOG_ID),
+        "谓词只应看到当前 incarnation 的行，实际 {observed_log_ids:?}"
+    );
+
+    // 谓词本身的过滤仍然生效（不是把 where 语义吞掉）。
+    let picked: Vec<u64> = take_inbox_ready_where(&vault, 16, |row| row.relay_seq != 2)
+        .expect("where 过滤")
+        .into_iter()
+        .map(|row| row.relay_seq)
+        .collect();
+    assert_eq!(picked, vec![1, 3]);
+}
+
+/// `sync.log_id` 缺失 / 长度非法时的语义：
+/// - 缺失 → 空（`take`）/ 0（`count`），即新库旧行为；即使残留其它 log 的行与 received，
+///   也不能回退成"不过滤 log"；
+/// - 长度非法 → 明确 `Corrupt`（与 `sync_cursor_state` 同口径），不静默当作没有 log。
+#[test]
+fn ready_without_log_id_is_empty_and_invalid_length_is_corrupt() {
+    let (_dir, vault) = fresh_vault();
+    // 新库：没有 sync.log_id，也没有 inbox 行 → 空/0（旧实现同样返回空）。
+    assert!(take_inbox_ready(&vault, 8)
+        .expect("无 log 读 ready")
+        .is_empty());
+    assert_eq!(count_inbox_ready(&vault).expect("无 log 计数"), 0);
+
+    // 不一致的持久状态：有遗留 verified 行与 received，却没有 sync.log_id。
+    // 没有当前 incarnation 就没有可匹配的 log，ready 必须为空。
+    insert_stale_incarnation_row(&vault, &OTHER_LOG_ID, 1, 0x5b);
+    write_raw_meta(&vault, META_SYNC_RECEIVED_SEQ, &1u64.to_be_bytes());
+    assert!(
+        take_inbox_ready(&vault, 8)
+            .expect("无 log 读 ready")
+            .is_empty(),
+        "缺 sync.log_id 时不能漏出其它 incarnation 的遗留行"
+    );
+    assert_eq!(count_inbox_ready(&vault).expect("无 log 计数"), 0);
+
+    // 长度不是 16B = 损坏：显式错误，不静默当成"没有 log"。
+    write_raw_meta(&vault, META_SYNC_LOG_ID, b"short");
+    let error = take_inbox_ready(&vault, 8).expect_err("非法 log_id 长度必须报错");
+    assert!(
+        matches!(error, StorageError::Corrupt { .. }),
+        "实际 {error:?}"
+    );
+    let error = count_inbox_ready(&vault).expect_err("非法 log_id 长度必须报错");
+    assert!(
+        matches!(error, StorageError::Corrupt { .. }),
+        "实际 {error:?}"
+    );
+}
diff --git a/crates/storage/tests/sync_intent.rs b/crates/storage/tests/sync_intent.rs
index 2242567..7a7a4f1 100644
--- a/crates/storage/tests/sync_intent.rs
+++ b/crates/storage/tests/sync_intent.rs
@@ -25,9 +25,9 @@ use menghunote_storage::session::{
 use menghunote_storage::{
     append_updates, apply_baseline_batch, begin_baseline_apply, enqueue_sync_intent,
     finish_baseline_apply, intent_message_id, intent_object_id, load_body_doc, load_head,
-    local_sync_intent, outbox_row, pending_encryption, BodyDoc, DurableSink, JournalEntry,
-    JournalOrigin, StorageError, StructureWriter, SyncContext, SyncIntent, SyncOutboxState,
-    UpdateRecord, Value, Vault, VaultJournalSink, STRUCTURE_DOC_ID,
+    local_sync_intent, outbox_row, pending_encryption, BaselineExpectation, BodyDoc, DurableSink,
+    JournalEntry, JournalOrigin, StorageError, StructureWriter, SyncContext, SyncIntent,
+    SyncOutboxState, UpdateRecord, Value, Vault, VaultJournalSink, STRUCTURE_DOC_ID,
 };
 use menghunote_text::UnicodeScalar;
 
@@ -504,7 +504,17 @@ fn remote_import_and_adopt_never_produce_intents() {
         doc.insert_scalars(0, "基线").expect("写基线");
         doc.export_snapshot().expect("导出 snapshot")
     };
-    begin_baseline_apply(&vault3, &[7; 16], 3, "cursor-1").expect("begin");
+    begin_baseline_apply(
+        &vault3,
+        &[7; 16],
+        3,
+        "cursor-1",
+        &BaselineExpectation::new(
+            std::iter::once("doc-1".to_owned()),
+            std::iter::empty::<[u8; 32]>(),
+        ),
+    )
+    .expect("begin");
     apply_baseline_batch(&vault3, &[("doc-1".to_owned(), baseline)], &[]).expect("apply");
     let _ = finish_baseline_apply(&vault3, 3, "cursor-2").expect("finish");
     assert_eq!(outbox_count(&vault3), 0, "adopt/baseline 绝不能产生 intent");
diff --git a/crates/sync/src/bootstrap.rs b/crates/sync/src/bootstrap.rs
index 4dc768c..0d0593c 100644
--- a/crates/sync/src/bootstrap.rs
+++ b/crates/sync/src/bootstrap.rs
@@ -54,10 +54,10 @@ use crate::upload::fetch_object;
 use menghunote_storage::{
     abort_baseline_apply, apply_baseline_assets, apply_baseline_batch, asset_bytes,
     baseline_apply_state, begin_baseline_apply, load_body_doc, load_structure, sync_cursor_state,
-    AffectedGuard, BaselineApplyStatus, BaselineAsset, BodyDoc, Statement, StructureDoc,
-    StructureWriter, Value, Vault, APP_DOC_SCHEMA, CODEC_VERSION, META_SYNC_CONFIRMED_HIGHWATER,
-    META_SYNC_INTEGRATED_SEQ, META_SYNC_LOG_ID, META_SYNC_RECEIVED_SEQ, MIN_READER,
-    STRUCTURE_DOC_ID,
+    AffectedGuard, BaselineApplyStatus, BaselineAsset, BaselineExpectation, BodyDoc, Statement,
+    StructureDoc, StructureWriter, Value, Vault, APP_DOC_SCHEMA, CODEC_VERSION,
+    META_SYNC_CONFIRMED_HIGHWATER, META_SYNC_INTEGRATED_SEQ, META_SYNC_LOG_ID,
+    META_SYNC_RECEIVED_SEQ, MIN_READER, STRUCTURE_DOC_ID,
 };
 use menghunote_sync_proto::{
     checkpoint_signing_input, refs_digest_input, CheckpointCommit, Envelope, EnvelopeHeader,
@@ -1052,8 +1052,15 @@ fn apply_selected(
     let checkpoint_id = selected.checkpoint.checkpoint_id;
     let baseline_s = selected.checkpoint.covered_seq;
     let source_cursor = format!("{JOURNAL_SOURCE_PREFIX}{}", selected.checkpoint.cursor);
+    // 完整性基准：本 checkpoint 覆盖的全部文档与资产都必须出现在 apply 批里。
+    // 集合由 begin 记录，任何"少导入一批"的续跑都不会被误当成完整
+    // （rotation 的 finish 会直接 Conflict；bootstrap 的 finalize 另有逐项读回校验）。
+    let expected = BaselineExpectation::new(
+        selected.docs.iter().map(|doc| doc.doc_id.clone()),
+        selected.assets.iter().map(|asset| asset.object_id),
+    );
     reconcile_journal(vault, &checkpoint_id, baseline_s, &source_cursor)?;
-    begin_baseline_apply(vault, &checkpoint_id, baseline_s, &source_cursor)?;
+    begin_baseline_apply(vault, &checkpoint_id, baseline_s, &source_cursor, &expected)?;
 
     // body → structure → asset（structure 投影依赖正文行已存在；与旧实现的顺序一致）。
     let bodies: Vec<&VerifiedDoc> = selected
diff --git a/crates/sync/src/engine.rs b/crates/sync/src/engine.rs
index 67ebbc8..75fcadd 100644
--- a/crates/sync/src/engine.rs
+++ b/crates/sync/src/engine.rs
@@ -170,7 +170,10 @@ pub struct PushReport {
 /// - relay 上对象已完整（例如 checkpoint/轮换/另一台设备传过）时按确定性密文逐片
 ///   校验后只做提交/落标记，不重传分片；分片缺失时只补缺失片（已 durable 的片不重传）。
 /// - 对象被其它 reservation 持有时整条资产**延后**（`deferred`），不阻塞正文/结构同步，
-///   下一轮再试；网络/单资产损坏计入 `failed`，同样不阻塞本轮。
+///   下一轮再试；relay/对象校验失败计入 `failed`，同样不阻塞本轮。
+/// - **本地库损坏**（[`SyncError::LocalCorrupt`]，例如字节长度/哈希与元数据不一致）不是
+///   可重试的网络失败：继续扫完其余健康资产后，把首个损坏原样上抛，见
+///   [`SyncEngine::upload_pending_assets`]。
 #[derive(Debug, Clone, Default, PartialEq, Eq)]
 pub struct AssetSyncReport {
     /// 被正文引用、有明文的资产数（本轮扫描口径）。
@@ -187,7 +190,10 @@ pub struct AssetSyncReport {
     pub reused_existing: usize,
     /// 被其它 reservation 持有、需下一轮再试的资产数。
     pub deferred: usize,
-    /// 单资产失败数（网络/校验失败；不阻塞正文同步，下一轮重试）。
+    /// 单资产 relay/对象校验失败数（不阻塞正文同步，下一轮重试）。
+    ///
+    /// **本地库损坏不计入这里**：`LocalCorrupt` 每轮重试都不会自愈，会掩盖真正的
+    /// "需要本地修复"状态，因此整轮上抛而不是伪装成可重试失败。
     pub failed: usize,
     /// 元数据不可用或空数据而跳过的资产数（不上传空资产）。
     pub skipped_invalid: usize,
@@ -1091,12 +1097,19 @@ impl<'a, C: RelayClient> SyncEngine<'a, C> {
     /// 阶段 2b：**增量上传被正文引用的本地资产**（正文推送之后、拉取之前）。
     ///
     /// 顺序是刻意的（ADR 0012 的两阶段路径也复用本方法）：
-    /// - 正文/结构更新先 `push`，资产后置；单条资产失败只计入
+    /// - 正文/结构更新先 `push`，资产后置；单条资产 relay/对象校验失败只计入
     ///   [`AssetSyncReport::failed`]，**不阻塞**本轮正文交付；
     /// - 资产在 `pull` 之前完成，远端设备下一次 pull 就能同时拿到正文与 ASSET 事件；
     /// - 已有 durable 标记的资产不读字节、不发请求；relay 上已完整的对象只校验+提交，
     ///   被其它 reservation 持有的对象延后（`deferred`），下一轮重试。
     ///
+    /// **本地库损坏上抛**：`upload_one_asset` 发现本地字节长度/哈希与元数据不一致、
+    /// 或确定性分片事实与元数据不符时返回 [`SyncError::LocalCorrupt`]。这类错误每轮
+    /// 重试都不可能自愈，必须让调用方看到（否则就是"永远重试成功、永不报错"）。
+    /// 处理方式是**继续扫完其余资产**（健康资产照常上传/落标记），最后把第一个本地
+    /// 损坏原样上抛；[`SyncError::Paused`]（全局暂停）与 [`SyncError::Storage`]
+    /// （本地库读写失败）仍**立即**上抛，语义不变。
+    ///
     /// 只处理 `asset_refs` 引用得到的资产：历史孤儿不重传（`orphans_skipped` 如实上报）。
     pub fn upload_pending_assets(&mut self) -> Result<AssetSyncReport, SyncError> {
         if self.rotation_frozen {
@@ -1106,6 +1119,7 @@ impl<'a, C: RelayClient> SyncEngine<'a, C> {
             orphans_skipped: count_unreferenced_assets(self.vault)?,
             ..AssetSyncReport::default()
         };
+        let mut first_local_corrupt: Option<SyncError> = None;
         let assets = list_referenced_assets(self.vault)?;
         report.referenced = assets.len();
         for meta in assets {
@@ -1149,9 +1163,20 @@ impl<'a, C: RelayClient> SyncEngine<'a, C> {
                 Err(error) if matches!(error, SyncError::Paused(_) | SyncError::Storage(_)) => {
                     return Err(error)
                 }
+                // 本地库损坏不是网络失败：先继续上传其余健康资产（不让单个损坏拖住
+                // 整库），最后统一上抛。计成 `failed` 会让它每轮"重试成功"、永不报错。
+                Err(error) if matches!(error, SyncError::LocalCorrupt(_)) => {
+                    if first_local_corrupt.is_none() {
+                        first_local_corrupt = Some(error);
+                    }
+                }
+                // 其余（relay/传输/对象校验）沿用既有语义：单资产失败，下一轮重试。
                 Err(_) => report.failed += 1,
             }
         }
+        if let Some(error) = first_local_corrupt {
+            return Err(error);
+        }
         Ok(report)
     }
 
diff --git a/crates/sync/src/rotation_apply.rs b/crates/sync/src/rotation_apply.rs
index 9dabfdb..b47ce45 100644
--- a/crates/sync/src/rotation_apply.rs
+++ b/crates/sync/src/rotation_apply.rs
@@ -28,7 +28,7 @@ use crate::{DeviceId, ObjectId, VaultId};
 use menghunote_storage::{
     abort_baseline_apply, apply_baseline_assets, apply_baseline_batch, baseline_apply_state,
     begin_baseline_apply, finish_baseline_apply, repack_required, BaselineApplyFinish,
-    BaselineApplyState, BaselineApplyStatus, BaselineAsset,
+    BaselineApplyState, BaselineApplyStatus, BaselineAsset, BaselineExpectation,
 };
 use menghunote_sync_proto::{
     activation_t_signing_input, checkpoint_signing_input, grant_package_digest, ActivationCommitV1,
@@ -307,7 +307,19 @@ pub fn apply_verified_baseline(
     payload: &BaselinePayload,
     baseline_cursor: &str,
 ) -> Result<BaselineApplyState, SyncError> {
-    begin_baseline_apply(vault, &payload.checkpoint_id, payload.baseline_s, "")?;
+    // 完整性基准：候选 checkpoint 覆盖的 docs/资产必须全部写进 journal，少一批就
+    // finish 不了（不会推进 cursor）。
+    let expected = BaselineExpectation::new(
+        payload.docs.iter().map(|(doc_id, _)| doc_id.clone()),
+        payload.assets.iter().map(|asset| asset.object_id),
+    );
+    begin_baseline_apply(
+        vault,
+        &payload.checkpoint_id,
+        payload.baseline_s,
+        "",
+        &expected,
+    )?;
     let result = (|| {
         for chunk in payload.docs.chunks(64) {
             apply_baseline_batch(vault, chunk, &[])?;
diff --git a/crates/sync/tests/asset_sync.rs b/crates/sync/tests/asset_sync.rs
index 5eea795..0f43745 100644
--- a/crates/sync/tests/asset_sync.rs
+++ b/crates/sync/tests/asset_sync.rs
@@ -3,7 +3,8 @@
 //! 覆盖 attachment-insert 缺口：A 插入并引用新资产后，普通 `sync_once`（非发布基线）
 //! 就应把 ASSET 对象增量上传，B 能取到与 A 一致的字节；第二个同步轮次不重传；
 //! 多片资产传到一半失败后从缺失片续传；checkpoint 已上传的对象被普通同步直接复用/标记；
-//! 被维护 reservation 持有的对象延后（ID_CONFLICT 不阻塞正文）。
+//! 被维护 reservation 持有的对象延后（ID_CONFLICT 不阻塞正文）；本地字节与元数据 hash
+//! 不一致时必须上抛 `LocalCorrupt`（不能伪装成可重试失败）。
 
 mod support;
 
@@ -16,6 +17,7 @@ use menghunote_storage::{
 };
 use menghunote_sync::checkpoint::asset_object_id;
 use menghunote_sync::engine::{EngineIdentity, SyncEngine};
+use menghunote_sync::SyncError;
 use menghunote_sync_proto::{cap_plain, part_count_for, ObjectType};
 
 const ASSET_MEDIA: &str = "text/plain";
@@ -401,7 +403,7 @@ fn missing_media_or_data_skips_asset_without_upload() {
 }
 
 #[test]
-fn asset_hash_mismatch_is_failed_without_uploading_wrong_bytes() {
+fn asset_hash_mismatch_raises_local_corrupt_without_uploading_wrong_bytes() {
     ensure_test_identity_wrap();
     let (_dir, vault) = fresh_vault();
     let device = Device::new(0x91);
@@ -431,12 +433,18 @@ fn asset_hash_mismatch_is_failed_without_uploading_wrong_bytes() {
     link_asset_ref(&vault, "note-tamper", &claimed, 0, 1).expect("link");
     seed_pull_state(&vault, &relay);
 
-    let report = {
+    // P2-9：本地库损坏不是可重试的网络失败，必须**上抛**让调用方看到；原实现把它
+    // 计入 `failed` 后继续，于是每轮都"重试成功"、永不报错。
+    let error = {
         let mut sync = engine(&vault, &mut relay, &device);
-        sync.sync_once().expect("正文同步不得被坏资产阻塞")
+        sync.sync_once()
+            .expect_err("本地资产 hash 与字节不一致必须上抛 LocalCorrupt")
     };
-    assert_eq!(report.assets.failed, 1);
-    assert_eq!(report.assets.uploaded, 0);
+    assert!(
+        matches!(error, SyncError::LocalCorrupt("资产字节哈希与元数据不一致")),
+        "实际：{error:?}"
+    );
+    // 整轮报错也不能把错误字节传给 relay、不能落 durable 标记。
     assert!(relay.put_history.is_empty());
     assert!(marker_rows(&vault).is_empty());
 }
diff --git a/crates/sync/tests/engine.rs b/crates/sync/tests/engine.rs
index 922a57f..bd40809 100644
--- a/crates/sync/tests/engine.rs
+++ b/crates/sync/tests/engine.rs
@@ -11,10 +11,11 @@ mod support;
 use support::*;
 
 use menghunote_storage::{
-    append_updates, count_inbox_ready, load_body_doc, load_head, outbox_parts, outbox_row,
-    store_inbox_verified_batch, sync_cursor_state, take_inbox_ready, BodyDoc, StorageError,
-    SyncOutboxState, UpdateRecord, Vault, VerifiedInboxRow, APP_DOC_SCHEMA, CODEC_VERSION,
-    META_SYNC_LOG_ID, MIN_READER,
+    append_updates, asset_bytes, asset_content_hash, count_inbox_ready, create_note_slot,
+    insert_asset, link_asset_ref, load_body_doc, load_head, outbox_parts, outbox_row,
+    replace_asset_refs, store_inbox_verified_batch, sync_cursor_state, take_inbox_ready, BodyDoc,
+    StorageError, SyncOutboxState, UpdateRecord, Vault, VerifiedInboxRow, APP_DOC_SCHEMA,
+    CODEC_VERSION, META_SYNC_LOG_ID, MIN_READER,
 };
 use menghunote_sync::engine::{
     EngineConfig, EngineIdentity, IntegrateConfig, SyncEngine, RELAY_CURSOR_META,
@@ -2207,6 +2208,206 @@ fn pull_pauses_on_lagging_asset_event_epoch_before_download() {
     );
 }
 
+// ============================================================ P2-9：本地资产损坏
+//
+// `upload_one_asset` 在**本地字节与元数据不一致**时返回 `LocalCorrupt`。原实现把它
+// 和网络失败一样 `report.failed += 1` 后继续，于是每轮都"重试成功"、永不报错。
+// 约定：本地库损坏不是可重试失败——扫完其余资产后**上抛**；健康资产仍要完整上传。
+
+const ASSET_MEDIA: &str = "text/plain";
+
+/// 建一篇资产测试用的 note（`loro_docs` 行 + `notes` 行；`asset_refs` 的外键要求）。
+fn seed_asset_note(vault: &Vault, note_id: &str, peer_id: u64) {
+    seed_doc(vault, note_id, peer_id);
+    create_note_slot(vault, note_id, note_id, note_id, 1_700_000_000).expect("notes 行");
+}
+
+/// 用 storage **公开 API** 构造"元数据 hash 与本地字节不一致"的损坏资产。
+///
+/// 路径：`replace_asset_refs` 先按引用建 `data IS NULL` 的元数据占位（`byte_len`/
+/// `media_type` 也是 NULL），再用 `insert_asset` 的"补全占位明文"分支写入**另一份
+/// 字节**——该分支只校验长度、不重算内容哈希。这正是真实损坏（外部写坏/恢复错
+/// 字节）在库里的形态，且不需要测试直接改 SQL。
+fn seed_corrupt_asset(vault: &Vault, note_id: &str, declared: [u8; 32], actual: &[u8]) {
+    replace_asset_refs(vault, note_id, 1, &[(declared, 0)]).expect("元数据占位 + 引用");
+    insert_asset(
+        vault,
+        &declared,
+        actual.len() as u64,
+        Some(ASSET_MEDIA),
+        actual,
+    )
+    .expect("按占位补全明文");
+}
+
+/// 健康资产：元数据与字节自洽，并被 `asset_refs` 引用（可被增量上传）。
+fn seed_healthy_asset(vault: &Vault, note_id: &str, data: &[u8]) -> [u8; 32] {
+    let hash = asset_content_hash(data);
+    insert_asset(vault, &hash, data.len() as u64, Some(ASSET_MEDIA), data).expect("insert_asset");
+    link_asset_ref(vault, note_id, &hash, 0, 1).expect("link_asset_ref");
+    hash
+}
+
+/// durable "已上传"标记（`sync.asset_uploaded.<hex object_id>`）的键列表。
+fn asset_marker_keys(vault: &Vault) -> Vec<String> {
+    vault
+        .query(
+            "SELECT key FROM meta WHERE substr(key,1,?1)=?2 ORDER BY key",
+            &[
+                menghunote_storage::Value::Integer("sync.asset_uploaded.".len() as i64),
+                menghunote_storage::Value::Text("sync.asset_uploaded.".to_owned()),
+            ],
+        )
+        .expect("读 asset 标记")
+        .iter()
+        .map(|row| match row.first() {
+            Some(menghunote_storage::Value::Text(key)) => key.clone(),
+            _ => panic!("asset 标记 key 类型非法"),
+        })
+        .collect()
+}
+
+/// 本地损坏（hash 与字节不一致）必须**上抛**，不能被计入 `failed` 后静默继续。
+#[test]
+fn corrupt_local_asset_raises_instead_of_counting_retryable_failure() {
+    ensure_test_identity_wrap();
+    let (_dir, vault) = fresh_vault();
+    let device = Device::new(0x7c);
+    let mut relay = FakeRelay::new(&[&device]);
+    seed_asset_note(&vault, "note-corrupt", device.identity.peer_id);
+
+    // 元数据声明的是 `expected-bytes-A` 的哈希，库里实际存的是 `tampered-bytes-A`
+    // （同长、不同内容），`insert_asset` 的占位补全路径不会拦住它。
+    let declared = asset_content_hash(b"expected-bytes-A");
+    let actual = b"tampered-bytes-A";
+    seed_corrupt_asset(&vault, "note-corrupt", declared, actual);
+    assert_eq!(
+        asset_bytes(&vault, declared).expect("读本地资产"),
+        Some(actual.to_vec()),
+        "夹具前提：本地确实存着与元数据 hash 不一致的字节"
+    );
+    assert_ne!(asset_content_hash(actual), declared, "夹具必须是损坏的");
+
+    let error = {
+        let mut sync = engine(&vault, &mut relay, &device);
+        sync.upload_pending_assets()
+            .expect_err("本地库损坏必须上抛，不能计入 failed 后静默继续")
+    };
+    assert!(
+        matches!(error, SyncError::LocalCorrupt("资产字节哈希与元数据不一致")),
+        "必须原样上抛 LocalCorrupt（不是可重试网络失败）：{error:?}"
+    );
+    assert!(relay.put_history.is_empty(), "损坏资产不得上传任何分片");
+    assert!(relay.objects.is_empty(), "损坏资产不得提交对象");
+    assert!(
+        asset_marker_keys(&vault).is_empty(),
+        "损坏资产不得落 durable 标记"
+    );
+}
+
+/// 单个损坏资产不得阻止其余健康资产上传（扫描完再上抛），且下一轮不得静默"重试成功"。
+#[test]
+fn corrupt_asset_does_not_block_healthy_upload_but_still_raises() {
+    ensure_test_identity_wrap();
+    let (_dir, vault) = fresh_vault();
+    let device = Device::new(0x7d);
+    let mut relay = FakeRelay::new(&[&device]);
+
+    // 健康资产放在**后面**：`list_referenced_assets` 按 hash 升序，4ba7… < d6cc…，
+    // 损坏资产先被扫到——若实现改成 fail-fast，这条用例就会丢掉健康资产。
+    seed_asset_note(&vault, "note-corrupt", device.identity.peer_id);
+    seed_asset_note(&vault, "note-mixed", device.identity.peer_id);
+    let declared = asset_content_hash(b"expected-bytes-A");
+    seed_corrupt_asset(&vault, "note-corrupt", declared, b"tampered-bytes-A");
+    let healthy_data = b"healthy asset bytes";
+    let healthy = seed_healthy_asset(&vault, "note-mixed", healthy_data);
+    assert!(
+        declared < healthy,
+        "夹具顺序前提：损坏资产必须先于健康资产被扫描"
+    );
+
+    let error = {
+        let mut sync = engine(&vault, &mut relay, &device);
+        sync.upload_pending_assets()
+            .expect_err("存在本地损坏时整轮必须报错")
+    };
+    assert!(
+        matches!(error, SyncError::LocalCorrupt("资产字节哈希与元数据不一致")),
+        "必须原样上抛 LocalCorrupt：{error:?}"
+    );
+
+    // 健康资产仍然完整上传：分片 + 对象提交 + durable 标记。
+    let healthy_object = menghunote_sync::checkpoint::asset_object_id(
+        &device.root,
+        &healthy,
+        healthy_data.len() as u64,
+        ASSET_MEDIA,
+    )
+    .expect("健康资产 object_id");
+    assert!(
+        relay.parts.contains_key(&(healthy_object, 0)),
+        "健康资产分片必须已上传（不能被单个损坏拦住）"
+    );
+    assert_eq!(
+        relay
+            .objects
+            .get(&healthy_object)
+            .map(|object| object.commit.object_type),
+        Some(ObjectType::Asset),
+        "健康资产必须完成对象提交"
+    );
+    let markers = asset_marker_keys(&vault);
+    assert_eq!(markers.len(), 1, "只有健康资产落 durable 标记");
+    assert!(
+        markers[0].ends_with(&menghunote_sync::ids::hex_of(&healthy_object)),
+        "标记必须属于健康资产"
+    );
+
+    // 下一轮：健康资产命中标记（不重传、不再探测 relay），损坏资产仍如实上抛。
+    let puts = relay.put_history.len();
+    let gets = relay.part_gets.len();
+    let again = {
+        let mut sync = engine(&vault, &mut relay, &device);
+        sync.upload_pending_assets()
+            .expect_err("损坏仍在，下一轮必须继续报错")
+    };
+    assert!(
+        matches!(again, SyncError::LocalCorrupt("资产字节哈希与元数据不一致")),
+        "下一轮仍必须是 LocalCorrupt：{again:?}"
+    );
+    assert_eq!(
+        relay.put_history.len(),
+        puts,
+        "健康资产命中标记后不得重传分片"
+    );
+    assert_eq!(
+        relay.part_gets.len(),
+        gets,
+        "健康资产命中标记后不得再探测 relay"
+    );
+}
+
+/// 正常路径不受影响：健康的被引用资产仍计入 `uploaded`。
+#[test]
+fn healthy_referenced_asset_is_still_counted_as_uploaded() {
+    ensure_test_identity_wrap();
+    let (_dir, vault) = fresh_vault();
+    let device = Device::new(0x7e);
+    let mut relay = FakeRelay::new(&[&device]);
+    seed_asset_note(&vault, "note-healthy", device.identity.peer_id);
+    seed_healthy_asset(&vault, "note-healthy", b"healthy asset bytes");
+
+    let report = {
+        let mut sync = engine(&vault, &mut relay, &device);
+        sync.upload_pending_assets().expect("健康资产必须正常上传")
+    };
+    assert_eq!(report.referenced, 1);
+    assert_eq!(report.uploaded, 1, "正常资产仍必须计入 uploaded");
+    assert_eq!(report.failed, 0, "不得莫名计成失败");
+    assert_eq!(report.skipped_invalid, 0);
+    assert_eq!(asset_marker_keys(&vault).len(), 1);
+}
+
 // ============================================================ Phase 0：指标 + 批量 inbox
 // 只加**只读**度量与默认关闭的实验开关；不改变追赶策略、不动 relay/wire/schema。
 
diff --git a/crates/sync/tests/phase1a_release.rs b/crates/sync/tests/phase1a_release.rs
index 975e6af..e293fc7 100644
--- a/crates/sync/tests/phase1a_release.rs
+++ b/crates/sync/tests/phase1a_release.rs
@@ -26,9 +26,9 @@ use menghunote_storage::{
     apply_baseline_batch, baseline_apply_debug_counters, baseline_apply_debug_timings,
     baseline_apply_state, begin_baseline_apply, finish_baseline_apply, load_body_doc,
     reset_baseline_apply_debug, sync_cursor_state, BaselineApplyDebugCounters,
-    BaselineApplyDebugTimings, BaselineApplyFinish, BaselineApplyStatus, BodyDoc, Value, Vault,
-    BASELINE_MERGE_BATCH, META_SYNC_CONFIRMED_HIGHWATER, META_SYNC_INTEGRATED_SEQ,
-    META_SYNC_RECEIVED_SEQ, META_SYNC_RELAY_CURSOR,
+    BaselineApplyDebugTimings, BaselineApplyFinish, BaselineApplyStatus, BaselineExpectation,
+    BodyDoc, Value, Vault, BASELINE_MERGE_BATCH, META_SYNC_CONFIRMED_HIGHWATER,
+    META_SYNC_INTEGRATED_SEQ, META_SYNC_RECEIVED_SEQ, META_SYNC_RELAY_CURSOR,
 };
 
 /// 基线文档数（真实信任链里 docs 数量更多；这里固定 20 便于两档总量均分）。
@@ -216,11 +216,17 @@ fn run_probe(label: &'static str, target_bytes: usize) -> ProbeStats {
     // 3) 持闸段：begin → apply 各批 → finish。
     reset_baseline_apply_debug();
     let begin_started = Instant::now();
-    let begin_state =
-        begin_baseline_apply(&vault, &[0x51; 16], BASELINE_S, "c0-source").expect("begin baseline");
+    let expected =
+        BaselineExpectation::new(doc_ids.iter().cloned(), std::iter::empty::<[u8; 32]>());
+    let begin_state = begin_baseline_apply(&vault, &[0x51; 16], BASELINE_S, "c0-source", &expected)
+        .expect("begin baseline");
     let begin_ms = begin_started.elapsed().as_millis();
     assert_eq!(begin_state.status, BaselineApplyStatus::Staging);
-    assert!(begin_state.finishable);
+    assert_eq!(begin_state.expected_docs, Some(DOCS as u64));
+    assert!(
+        !begin_state.finishable,
+        "begin 时预期 docs 尚未应用，finishable 必须为 false"
+    );
 
     let entries: Vec<(String, Vec<u8>)> = doc_ids
         .iter()

````


---

## 附录 C：分段上传建议（平台限制附件大小/上下文时）

1. 先上传本文件 §1–§5 + 附录 A 中的 `baseline_apply.rs`/`journal.rs`，让模型评审 P1-1–P1-3。
2. 再上传 `sync_bridge.rs`/`sync_meta.rs`/`actor.rs`，评审 P1-4/P2-6/P2-7。
3. 最后上传 `remote_known.rs`/`store.rs`/`engine.rs` + 附录 B diff，评审 P2-5/P2-8/P2-9。
4. 每段都先问“是否收到完整函数”，再让它给结论；不要让它假设能执行 cargo。
