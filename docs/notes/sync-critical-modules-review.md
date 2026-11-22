# 同步 Phase 1A 关键模块评审（单文件版）

> **2026-11-22 更新**：Phase 1B MVP 已实现（bootstrap prepare/integrate、CatchUpMode、
> catchup、prune、两阶段/sync_once 接入）；本文以下“Phase 1B 未实现”是当时评审上下文，
> 现状见 [`sync-fast-catchup-plan.md`](sync-fast-catchup-plan.md) §11。

> **用途**：只上传这一份，评审 Phase 1A 的 6 个关键模块与 Phase 1B 阈值，不用再带
> 多个代码文件。代码基线 `5d01e9e`；Phase 1B **未实现**。
> 需要逐行看完整代码时，用文中 `路径:行号` 回仓库取即可。

## 0. 请评审者回答的问题

1. 聚合预检 + 同文档多轮 import，是否严格保持 Loro 因果与 `integrated` 连续前缀？
2. `BEGIN IMMEDIATE` 在单 writer actor 下是否足够；纯幂等重放也取写锁是否可接受？
3. `remote_known_vv` 的单调性、旧库追溯与 local-only 保护是否成立？
4. detached oplog 合并是否在所有 Loro 操作下与 attached 状态等价？
5. 25k / 16 MiB / 3 s / 2.0× / 1.5× 的 Phase 1B 阈值是否成立？
6. 还有哪些必须修/必须补测的阻断？

## 1. 状态与边界

**已实现（Phase 1A）**

- pull 早退前 flush；批量 inbox 单事务；正文按 doc 聚合；`load_head` 预检；
  多轮 import 自愈物理乱序；选批防饥饿；baseline 单次解码 + detached oplog 合并；
  remote-known VV；skipped 窗口有界扫描。

**未实现（Phase 1B）**

- 没有 `CatchUpMode`、`catchup_prepare`/`catchup_integrate` 拆分、真实 `/bootstrap`
  下载/验签、资产只补缺失、`prune_covered_inbox`、UI/状态机。

**本轮未改**：relay 服务端、`sync-proto` wire、schema、checkpoint 签名、游标语义。
只新增本地 `meta` 键 `sync.remote_known_vv.<doc_id>`（非 schema、不同步）。

**不可违反的契约（ADR 0006 / 0012）**

- 网络阶段不持写闸；integrate/adopt 短持闸。
- 触发窗口只能用 `S - received_seq`，不能用 `head - received`。
- fallback 必须绑定实际选中的 `S` / `cursor`。
- `integrated_seq` 只连续推进，缺口处不跳号 ACK。
- final `integrated >= S` 前不得删 inbox / 覆盖证明；`skipped_asset` 不能误删。
- epoch/rotation 控制边界未处理 → `Paused`，不得当损坏降级。

## 2. 模块 A：聚合预检与同文档多轮 import

文件：`crates/sync/src/engine.rs:1837`（`integrate_body_doc_group`）、
`crates/sync/src/engine.rs:2669`（`body_group_readiness`）。

### 2.1 预检（不 load_body_doc）

```rust
enum BodyGroupReadiness {
    Covered,  // 所有行 end_vv ⊆ head_vv：只标记
    Ready,    // 至少一行 base_vv ⊆ head_vv：需要 import
    Deferred, // 未覆盖行的 base_vv 全不满足：纯缺外部依赖，跳过
}

fn body_group_readiness(
    pending: &[PendingInboxRow],
    indices: &[usize],
    head_vv: &VersionVector,
) -> BodyGroupReadiness {
    let mut uncovered = false;
    for &index in indices {
        let Some(payload) = pending[index].row.verified_payload.as_deref() else {
            return BodyGroupReadiness::Ready; // 解码失败交给真正路径报错
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
    if uncovered { BodyGroupReadiness::Deferred } else { BodyGroupReadiness::Covered }
}
```

关键点：`Covered` 直接 `mark_integrated_batch`、`doc_loads=0`；`Deferred` 直接返回，
不读正文、不占选批预算；只有 `Ready` 才 `load_body_doc`。

### 2.2 多轮 import

```text
remaining = indices
loop:
  next = []
  round_imported = 0
  for row in remaining:
    if doc.vv() ⊇ row.end_vv:      // 已应用/被本轮覆盖
      mark row
    else if doc.vv() ⊇ row.base_vv:
      doc.import(row.payload)
      mark row; round_imported += 1
    else:
      next.push(row)               // 真缺外部依赖
  remaining = next
  if round_imported == 0: break    // 多扫无意义
```

所有成功 import 的 op 只用一次 `doc.pending_update(&before)` 导出，和全部可标记行
（含已覆盖行）进入同一笔 `append_updates_with_integrated_marks`；整批之后才推进一次
`integrated` 前缀。不能因为中间某行 deferred 就丢弃已成功子集。

### 2.3 选批防饥饿

`crates/sync/src/engine.rs:2562`：

- 每个 `(doc, log)` 子组先 readiness；
- `Deferred` 不进 selected、不占 `batch_bytes`、不占 `batch_objects`；
- `Covered` 进 selected 以便标记，按 0 字节计；
- `Ready` 才计入软预算；首条 ready 行永远放行（单行超大单独成批）。

### 2.4 测试与证据

- `aggregate_body_multi_round_heals_physical_out_of_order`
- `aggregate_body_precheck_skips_large_deferred_docs_without_starving_ready_doc`
- 纯已覆盖 `doc_loads=0`；256 行 `doc_loads=1`；物理乱序 `doc_loads=1`；
  `aggregate_body=false` 旧路径仍通过。
- `crates/sync/tests/engine.rs` 45 个用例。

### 2.5 请确认

1. 预检只信 `loro_docs.head_vv` 是否足够？是否还有旁路只改 history 不改 head？
2. 多轮 import 不要求 relay_seq 连续，是否与 Loro 的 op 依赖语义完全一致？
3. 部分行 deferred 后，`integrated` 标记与前缀缺口证明是否仍严格？
4. `Covered` 标记路径没有 head CAS（只标记 inbox），在并发 writer 下是否安全？

## 3. 模块 B：baseline_apply 单次解码 + detached oplog

文件：`crates/storage/src/baseline_apply.rs:334`、`crates/storage/src/body_doc.rs`。

### 3.1 分支逻辑

```text
baseline_doc = BodyDoc::restore(snapshot)     // 唯一一次 snapshot 解码
head = load_head(doc_id); local_vv = head.vv()
if !exists:
    update = baseline_doc.pending_update(default)
else if local_vv ⊆ baseline_vv:              // no local_extra
    update = (baseline_vv == local_vv) ? none
             : baseline_doc.pending_update(&local_vv)
else:                                        // local_extra
    local_doc = load_body_doc(doc_id)         // 仅这里一次
    local_delta = local_doc.pending_update(&baseline_vv)
    baseline_doc.import_oplog_only(local_delta)  // detached: 只合 oplog
    update = baseline_doc.pending_update_from_oplog(&local_vv)
```

- 旧实现：`BodyDoc::restore` 后再 `doc.import(snapshot)`，二次解码/导入大快照；
  已有本地文档走 attached import，成本占旧 gate 97.9–99.8%。
- 新实现：snapshot 只 decode 一次；仅取 VV 用 `load_head`；本地小 delta 只合
  **oplog**，不重算 state/diff；再导出差集。
- `import_oplog_only` 之后只允许 `pending_update_from_oplog` /
  `oplog_frontiers_encoded`，**不得读正文**。

### 3.2 实测

| 档 | 旧 gate_hold | 新 gate_hold | 闸外 verify |
|---|---:|---:|---:|
| 5 MiB | 2 718–4 143 ms | **35 ms** | 2 686 ms |
| 20 MiB | 44 574–61 308 ms | **110 ms** | 45 103 ms |
| 5→20 | 14.8–16.9× | **3.1×** | 16.7× |

根因：不是 decode（3.4×）、不是 pending_update（3.35×），是 Loro attached import
的 diff/state 应用（15.1–16.6×）。detached 反向导入 0.2 ms。

### 3.3 请确认

1. `oplog_frontiers` 与 state frontier 在所有 Loro 操作（多 peer、容器删除/移动、
   并发本地编辑）下是否总能一致？
2. detached 文档只导出、不读 state 的约束是否足够，还是需要类型层隔离？
3. 闸外 verify 仍 16.7×；是否必须先落 `loro_docs.snapshot`/流式重放，才启用机会性
   追赶？还是可以先接受 20 MiB 形状的 45 s 首次重放？

## 4. 模块 C：remote-known VV 与 tombstone

文件：`crates/storage/src/remote_known.rs`、`baseline_apply.rs:964`、
`store.rs:416`、`journal.rs`、`sync_bridge.rs`。

### 4.1 语义

`sync.remote_known_vv.<doc_id>` = 远端已确证见过的 oplog 前缀，只由两条路径扩大：

1. 应用 baseline snapshot：写入快照 VV；
2. 集成远端 update：`end_vv - 旧 head_vv` 并入。

本地写入不扩大（即使已 ACK）。tombstone 判定：

```text
if sync_outbox 有行: keep
if !doc_exists: deleted
if 本批有 snapshot:
    local_extra ? keep : deleted
if remote_known:
    local_vv ⊆ known ? deleted : keep
if 旧 journal baseline_vv:
    local_vv ⊆ baseline ? deleted : keep
else:
    keep   // local-only，绝不误删
```

### 4.2 已知残余（重要）

**旧库**（本次改动前普通 pull 同步过、之后再无远端 update）没有 remote-known 记录，
第一次追赶仍 `kept_local_edit`。这是保守行为，不会误删 local-only；要彻底追溯需
Phase 1B 从 manifest `ManifestDoc.vv` / 远端事件回填。**不能**凭 `head_seq>0` 猜。

### 4.3 请确认

1. `merge_statement` 在 A 事务外读旧值，并发 flush 下是否只导致“多保留”，不会少保留？
2. begin 回填旧 `baseline_apply_docs.baseline_vv` 的 SQL 是否覆盖所有旧 journal 状态？
3. manifest VV 回填方案会不会把本地未同步编辑误归入 remote-known？需要什么测试？

## 5. 模块 D：BEGIN IMMEDIATE

文件：`crates/storage/src/actor.rs:468`（`exec_inbox_batch`）。

```rust
// 先读（log_id/既有行）后写（插新行/推进 received）。
// Deferred 的锁升级不走 busy_timeout，可能直接 SQLITE_BUSY；
// Immediate 在事务开始取写锁，这一步仍受 5s busy_timeout 约束。
let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
```

测试：第二个 Vault 先 `BEGIN IMMEDIATE` 占写锁；批量入口设 `busy_timeout=0`，
断言整笔 `SQLITE_BUSY`、零半写；释放后重放得到真实 `SyncConflict`。回归证明退回
deferred 后该用例会失败（先读旧快照）。

**问题**：纯幂等重放也会取写锁；单 writer actor 下可接受吗？是否需要 BUSY 有界重试？

## 6. 模块 E：skipped_received 有界扫描

文件：`crates/storage/src/sync_meta.rs:209`。

```sql
SELECT relay_seq FROM sync_inbox
 WHERE log_id=?1 AND relay_seq>?2 AND <condition>
 ORDER BY relay_seq LIMIT ?3
```

```sql
SELECT key FROM meta WHERE key>=?1 AND key<=?2
 ORDER BY key ASC LIMIT ?3
```

- 两个窗口各自 `LIMIT PREFIX_SCAN_LIMIT=4096`，各自记录 `inbox_truncated` /
  `skipped_truncated` 与窗口内最大序号。
- `truncated()` 为两者并集。
- 缺口证明 `proves_gap_at(gap)`：每个被截断来源的最大观测序号必须 ≥ gap；
  否则继续下一窗口。修复了“skipped 截断在 4096、inbox 有 5001 时提前 return”
  导致前缀卡死的问题。
- 测试：1..=5000 skipped + inbox 5001 → received=5001；缺口停在 4096；
  两窗口同时截断推进到 8200。

**问题**：`proves_gap_at` 的边界是否严密？格式不可解析的 meta 键占满窗口时
fail-safe 会卡住前缀，是否需要在真实 corruption 场景中报警？

## 7. 模块 F：Phase 1B 阈值（待确认）

| 项 | 当前默认 | 依据 |
|---|---|---|
| 强制追赶 | `gc_floor > received_seq` | 普通 pull 会 CursorExpired |
| 强制失败 | `Paused` + 指数退避，**不回退 pull** | 防死循环 |
| 强制体积 | 不受机会性上限；仅本地磁盘预算 | 必须离开过期游标 |
| 机会性事件窗口 | `S - received_seq >= 25_000` | 5000 条尾部仅 0.28–0.76 s，不足以摊平全链路 |
| 机会性快照上限 | **16 MiB** 密文（可配 8–16） | 优化后 20 MiB gate 110 ms；3 s 预算 |
| 成本交叉 | replay ≥ max(8 MiB, 2.0×plain_len) 且 增量耗时 > 1.5×快照总耗时 | 防小增量拉大快照 |
| 持闸预算 | `gate_hold_ms < 3 000`；估算超限不启动，实测超限中止回退 | ADR 0012 短持闸 |
| 失败 | 清 staging、冷却、无损回退普通聚合 pull | 不能卡死 |

`catchup_prepare` 不持闸：primary/fallback 选点并绑定实际 `S`/cursor；manifest +
snapshot 验签；`asset_bytes` 只补缺失；epoch/rotation 安全暂停。

`catchup_integrate` 持闸：flush → 分批 `baseline_apply`（每批 5 篇）→ `finish`
单调 CAS；`verify_imported` 放闸外或单独预算。

`prune_covered_inbox` 仅在 finalize 成功且持久化 `integrated_seq >= S` 后；保留
`skipped_asset` 证据，不删未覆盖行。

## 8. 测试/运行证据（摘要）

- storage **239** / sync **631** / app 默认 **740** / dev-sync **891** /
  no-default **575**；fmt/clippy 全绿。
- release `phase1a_release` 1 个 ignored 探针通过。
- 聚合：单文档 5000 条 `147.0 s → 275 ms`，`doc_loads 5000 → 20`；
  20 篇/5000 条 `14.9 s → 0.76 s`，`doc_loads 5000 → 400`。
- baseline 数据见 §3.2；闸外 verify 是最大残余。

## 9. 代码锚点（需要全文时）

| 文件 | 行 |
|---|---|
| `crates/sync/src/engine.rs` | 1837 / 2562 / 2669 |
| `crates/storage/src/baseline_apply.rs` | 231 / 334 / 964 |
| `crates/storage/src/body_doc.rs` | `import_oplog_only` / `pending_update_from_oplog` |
| `crates/storage/src/remote_known.rs` | 全文 |
| `crates/storage/src/actor.rs` | 468 |
| `crates/storage/src/sync_meta.rs` | 105 / 209 |
| `crates/storage/src/store.rs` | 98 / 416 / 471 |
| `crates/storage/src/sync_bridge.rs` | 1069 / 1202 / 1318 |

## 10. 结论建议格式

请回：

- **结论**：Phase 1A 是否可接受；6 个模块哪些必须先改。
- **必须修正**：文件/行号 + 为什么。
- **阈值**：25k / 16 MiB / 3 s / 2.0× / 1.5× 是否调整；旧库 tombstone 追溯方案。
- **新增风险**：并发、崩溃、coverage 证明、detached 边界。
- **不建议做**：明确后置项，避免扩大 Phase 1B。
