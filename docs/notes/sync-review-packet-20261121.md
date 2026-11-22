# 同步 Phase 1A 评审包（高级模型 / 1M 上下文）

> **只上传一份时**：用 [`sync-critical-modules-review.md`](sync-critical-modules-review.md)
> （6 个关键模块自包含版）；本包作为需要代码地图/完整证据时的展开版。

> **评审对象**：Phase 1A 缺陷收口 + `baseline_apply` 性能根因治理 + Phase 1B 计划阈值。
> **当前基线**：`5d01e9e`（Phase 1B 代码尚未开始）。
> **禁止**把本文当成 Phase 1B 已实现；本文末尾“Phase 1B 红线”只是待实现约束。

## 1. 建议的阅读顺序

1. 本包 §2–§5（先看架构、契约、关键算法）。
2. [`sync-fast-catchup-plan.md`](sync-fast-catchup-plan.md) §9（权威阈值；§9.1–§9.4）。
3. [`sync-phase0-metrics.md`](sync-phase0-metrics.md) §8.4b / §8.5（release 数据与根因）。
4. 代码第一批（§7）→ 测试第二批 → Phase 1B 上下文第三批。

旧 [`sync-phase0-review-brief.md`](sync-phase0-review-brief.md) 是 Phase 0 历史快照，
其中“Phase 1 未实现/500 阈值/128 MiB”**已过期**，只作历史对照。

## 2. 本轮范围与非范围

**已实现（Phase 1A）**

- pull 早退前 flush 已验签 inbox；批量 inbox 默认开启。
- `store_inbox_verified_batch` 单事务：既有行比对 + 新行插入 + `received` 前缀推进；
  批量入口 `BEGIN IMMEDIATE`。
- 正文按 doc 聚合：`load_head` 轻量预检 → 同 doc 多轮内存 import（物理乱序自愈）→
  一次 `pending_update` + 同一 A 事务标记多条 inbox → 整批一次 `integrated` 前缀。
- 选批不选纯 deferred 文档，已覆盖组 0 字节，避免跨文档饥饿。
- `baseline_apply`：snapshot 单次解码；仅取 VV 的路径用 `load_head`；
  本地小 delta 以 **detached oplog** 合并后导出差集；二次 snapshot import 归零。
- 每篇文档 remote-known VV（本地 `meta`）同事务维护，供 tombstone 判定。
- `skipped_received` 范围扫描加 `ORDER BY key LIMIT 4096`，跨窗口推进。

**明确未做（Phase 1B）**

- 没有 `CatchUpMode`、`catchup_prepare` / `catchup_integrate` 拆分；
- 没有真实 `/bootstrap` 选点、snapshot 下载/解密/验签、资产只补缺失；
- 没有 `prune_covered_inbox` / `finish` 同事务清理；
- 没有 Phase 1B UI/状态机；没有改 relay / `sync-proto` / schema / wire。

## 3. 契约与不变量

对照 ADR 0006 / ADR 0012：

- **网络阶段不持写闸，integrate/adopt 短持闸**；Phase 1B 的 prepare 也必须不持闸。
- checkpoint 的 `S` 是“能被快照真正抵消”的水位；触发窗口用 `S - received_seq`，
  不能用 `head - received`。
- fallback 必须绑定**实际选中的** checkpoint `S` / `cursor`；primary 的值不能复用。
- `integrated_seq` 只做单调连续推进；缺口处不得跳号 ACK。
- inbox 行是 checkpoint coverage 证据：**在 final `integrated >= S` 前不得删除**；
  尤其 `state='verified'` 不能被残留成永久假 deferred。
- `sync.skipped_asset.*` 不能因为“快照里没有资产”就删：它仍可能被 checkpoint
  coverage 证明读取；清理边界必须在 §9.3 约束内。
- `epoch`/rotation 跨越未处理控制边界 → `Paused`，**不得当损坏降级**。
- 本地 `meta` 新键（`sync.remote_known_vv.*`）非 schema、非 wire、不同步。

## 4. 数据流（代码级）

```
SyncEngine::pull
  └─ page loop → verify/decrypt → store_inbox_verified_batch (BEGIN IMMEDIATE)
       └─ received prefix (inbox + skipped_received 窗口)
  └─ integrate
       ├─ structure phase（现有逐行）
       └─ body aggregate
            ├─ load_head(doc) → head_vv
            ├─ body_group_readiness → Covered / Deferred / Ready
            ├─ Covered: mark_integrated_batch（不 load_body_doc）
            ├─ Ready: load_body_doc 一次
            │    └─ 多轮 import → doc.pending_update(before)
            │         └─ append_updates_with_integrated_marks
            │              ├─ loro_updates + head CAS
            │              ├─ inbox marks（同事务）
            │              └─ remote_known_vv = end_vv - 旧 head_vv
            └─ 整批后 advance_integrated_prefix 一次
```

`baseline_apply`：

```
begin(staging)
  └─ 回填旧 baseline_apply_docs.baseline_vv → sync.remote_known_vv.*（不降级）
apply_batch
  ├─ BodyDoc::restore(snapshot)（唯一一次）
  ├─ load_head 取 local_vv（不重放）
  ├─ no local_extra: pending_update(&local_vv)
  ├─ local_extra: load_body_doc 一次 → local_delta
  │    └─ baseline_doc.import_oplog_only(local_delta)
  │         └─ pending_update_from_oplog(&local_vv)
  ├─ tombstones: outbox / 本批快照 / remote_known / local-only 判定
  └─ 一笔事务：loro_updates + journal + remote_known + tombstone
finish: 单调 CAS 水位/cursor
```

## 5. 关键算法与正确性论证

### 5.1 聚合预检

`body_group_readiness(vault, rows)`（`engine.rs:2669` 附近）只读 `load_head` 的
`head_vv`：

- 所有行 `end_vv ⊆ head_vv` → Covered；
- 未覆盖行里没有任何 `base_vv ⊆ head_vv` → Deferred；
- 否则 Ready（解码失败也按 Ready，让原错误路径报出，避免把损坏当 deferred 永久滞留）。

这是 O(1)（每 doc 一次 head 查询），不触发 `load_body_doc`。

### 5.2 多轮合并

对 Ready 组：

- `remaining` 保存尚未处理的行；
- 每轮：`end_vv ⊆ doc.vv()` 直接记 integrated；`base_vv ⊆ doc.vv()` 则 import；
- 只要本轮有新 import，再扫剩余行（后 import 的行可能满足前行的 base）；
- 无 import 停止；remaining 保持 verified/deferred，不标记；
- 全部成功 import 的 op 只导出一次 `pending_update(&before)`，与全部标记行同事务。

正确性依赖：同一 doc 的 update 在 Loro 中按 op 依赖可合并；只要 base 已覆盖即可 import，
不要求 relay_seq 连续。物理乱序时，后到但 base 更早的行先 import，下一轮再满足前行。

### 5.3 选批防饥饿

`select_body_batch_aggregated`（`engine.rs:2562`）：

- 对每个 `(doc, log)` 组先 readiness；
- 纯 Deferred：不进 selected、不计 `batch_bytes`、不占 `batch_objects`；
- Covered：进 selected 以便标记，按 0 字节计；
- Ready：沿用软预算（首条 ready 行必放行，单行超大单独成批）；
- doc 轮转 / 结构先行不变。

### 5.4 detached oplog merge

Loro 的 attached `import` 会重算 diff/state；对“一篇一个巨大文本 op”形状，
成本随文本长度超线性增长（实测 attached 占旧 gate 97.9–99.8%）。

`BodyDoc::import_oplog_only` 只 `detach()` + `doc.import(bytes)`：
- 只合 oplog，不做 state 应用；
- 之后只允许 `pending_update_from_oplog` / `oplog_frontiers_encoded`，
  **不得**读正文；
- 合并后的 oplog frontier 与重放 state frontier 一致（各用例由 `load_body_doc`
  frontier 校验兜底）；
- 这不是通用写入路径，只给 baseline merge 用。

### 5.5 remote-known 与 tombstone

`sync.remote_known_vv.<doc_id>` = 远端已确证见过的 oplog 前缀，只由：

1. baseline snapshot 的 VV；
2. 远端 update 的 `end_vv - 旧 head_vv`；

单调并集扩大。tombstone 判定：

1. 有 `sync_outbox` → 保留；
2. 文档不存在 → 删；
3. 本批有快照：local extra=false → 删；true → 保留；
4. 有 remote-known：`local_vv ⊆ known` → 删；否则保留；
5. 旧 journal baseline VV 兜底；
6. 无证据 → 保留（local-only 永不误删）。

**已知残余**：本次改动前已由普通 pull 同步、但从未记录 remote-known 的旧库文档，
第一次追赶仍会保守保留；不能凭 `head_seq>0` 猜测（会误删 local-only）。彻底追溯需
Phase 1B 从 manifest 的 `ManifestDoc.vv` / 远端事件回填。

## 6. 实测证据（摘要）

- 聚合：单文档 5000 条 integrate `147.0 s → 275 ms`，`doc_loads 5000 → 20`；
  20 篇/5000 条 `14.9 s → 0.76 s`，`doc_loads 5000 → 400`。
- baseline gate（20 篇合成、storage merge、不含下载）：
  - 旧 5 MiB `2 718–4 143 ms`，20 MiB `44 574–61 308 ms`，5→20 `14.8–16.9×`；
  - 新 5 MiB **35 ms**，20 MiB **110 ms**，5→20 **3.1×**；
  - 闸外 verify 仍 `2 686 / 45 103 ms`（16.7×），这是当前最大残余。
- 测试：storage **239** / sync **631** / app 默认 **740** / dev-sync **891** /
  no-default **575**；fmt/clippy 全绿。
- 新增关键用例：
  - `aggregate_body_multi_round_heals_physical_out_of_order`
  - `aggregate_body_precheck_skips_large_deferred_docs_without_starving_ready_doc`
  - `received_prefix_crosses_more_than_one_skipped_window`
  - `tombstone_deletes_doc_pulled_through_normal_sync` /
    `tombstone_keeps_local_only_doc`
  - baseline 单次解码/无 local extra/detached 合并/幂等重放。

## 7. 代码文件（1M 上下文分批）

### 第一批（必看）

| 文件 | 行/入口 | 看什么 |
| --- | --- | --- |
| `crates/sync/src/engine.rs` | `integrate_body_doc_group` L1837、`select_body_batch_aggregated` L2562、`body_group_readiness` L2669 | 预检、多轮、选批防饥饿 |
| `crates/storage/src/sync_meta.rs` | `advance_consumed_prefix_on` L105、`scan_consumed_after_on` L209 | 双窗口截断/缺口证明 |
| `crates/storage/src/actor.rs` | `exec_inbox_batch` L468 | `BEGIN IMMEDIATE` 单事务 |
| `crates/storage/src/sync_bridge.rs` | `store_inbox_verified_batch` L1069、`mark_integrated_batch` L1202、`advance_integrated_prefix` L1318 | 批量/标记/前缀 API |
| `crates/storage/src/store.rs` | `append_updates_with_integrated_marks` L416、`append_updates_inner` L471、`load_head` L98 | 组合落盘/远程 VV/头部 CAS |
| `crates/storage/src/baseline_apply.rs` | `begin` L231、`apply_batch` L334、`tombstone_can_delete` L964 | 单次解码/detached/tombstone |
| `crates/storage/src/body_doc.rs` | `import_oplog_only` / `pending_update_from_oplog` | detached 语义 |
| `crates/storage/src/remote_known.rs` | 全文 | 远端已知 VV 的单调并集 |

### 第二批（证据）

| 文件 | 用途 |
| --- | --- |
| `crates/sync/tests/engine.rs` | 5 个新聚合用例 + 既有 45 个 |
| `crates/storage/tests/sync_inbox_batch.rs` | 22 个批量/前缀/BUSY 用例 |
| `crates/storage/tests/baseline_apply.rs` | 23 个 baseline/tombstone 用例 |
| `crates/sync/tests/phase1a_release.rs` | 5/20 MiB 计时探针 |
| `crates/sync/tests/phase0_catchup.rs` | 600/2000/5000 三模式矩阵 |
| `crates/storage/tests/sync_inbox_bench.rs` | inbox 逐条 vs 批量 |

### 第三批（Phase 1B 上下文，非本轮实现）

`crates/storage/src/baseline_apply.rs`、`crates/sync/src/bootstrap.rs`、
`crates/sync/src/dev_local.rs`、`crates/sync/src/rotation.rs`、
`crates/app/src/dev_sync/service.rs`。

## 8. 请高级模型重点判断

1. 聚合预检只信 `loro_docs.head_vv` 是否足够？所有写入点是否都成对更新
   head_vv/head_frontier（当前 `append_updates_inner`、baseline 是；是否还有旁路）？
2. 多轮 import 的“不要求 relay_seq 连续”是否与 Loro 的依赖语义完全一致？
   部分 deferred 后，`integrated` 标记与 `advance_integrated_prefix` 的缺口证明是否仍严格？
3. `BEGIN IMMEDIATE` 对单 writer actor 是净收益，但对“纯幂等重放”也会取写锁；
   在 WAL/慢盘/第二进程场景是否需要 BUSY 有界重试？
4. `remote_known` 的 `merge_statement` 在 A 事务外读旧值，是否可能在并发 flush 下
   丢失单调性？若只导致“多保留”是否可接受；如何加事务内断言？
5. detached oplog 合并的边界：只能导出/不得读 state；`oplog_frontiers` 与
   state frontier 的一致性是否在所有 Loro 版本/操作下成立？是否需要更强的测试
   （多 peer、删除/移动容器、并发本地编辑）？
6. tombstone 的旧库追溯方案：用 manifest `ManifestDoc.vv` 回填是否安全？
   会不会把“本地未同步编辑”误归入 remote-known？
7. 阈值：25k / 16 MiB / 3 s / 2.0× / 1.5× 是否成立？闸外 verify 16.7× 是否应先
   做 snapshot 落盘/流式重放，再启用机会性追赶？
8. Phase 1B 的 prepare/integrate 拆分、`gate_hold` 预算、`prune_covered_inbox`
   与 coverage 证明的边界是否有遗漏。

## 9. Phase 1B 红线（待实现，不可越）

- `catchup_prepare`（不持闸）：`/bootstrap` primary/fallback，实际 `S`/cursor 绑定；
  manifest + snapshot 验签；`asset_bytes` 预检只补缺失；epoch/rotation 安全暂停。
- `catchup_integrate`（持闸）：flush → 分批 `baseline_apply` → `finish` 单调 CAS；
  记录 `gate_hold_ms`；超 3 s 中止无损回退。
- `prune_covered_inbox(vault, up_to_seq)`：仅 finalize 成功且持久化
  `integrated_seq >= S` 后；保留 `skipped_asset` 证据；不删未覆盖行。
- 强制追赶：`gc_floor > received_seq` 不受机会性体积上限；失败 `Paused` + 退避，
  **绝不回退 pull**。
- 机会性：`S - received >= 25_000`，密文 ≤16 MiB，成本 2.0×/1.5×，失败清理
  staging/冷却/无损回退聚合 pull。

## 10. 建议给高级模型的提示词

> 你是同步/CRDT 评审者。先读 `sync-review-packet-20261121.md`、计划 §9、
> metrics §8.4b，再读第一批 8 个代码文件。重点审：聚合预检/多轮 import 的因果与
> 前缀证明、`BEGIN IMMEDIATE` 的锁语义、`remote_known` 的单调性、detached oplog
> 的边界、tombstone 旧库追溯、Phase 1B 阈值与写闸拆分。输出：结论、必须修正
> （文件/行号）、阈值建议、新增风险、不建议做。不要假设 Phase 1B 已实现。
