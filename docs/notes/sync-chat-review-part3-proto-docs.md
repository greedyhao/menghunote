# 同步评审 Part 3/6：sync-proto 与其余文档

@@PART 3/6

> 这是 **Part 3/6**。请先只确认收到，不要开始评审。
> 本部分约 417754 字符；应低于平台约 72 万字符的截断点。
> 内容与 Part 2 连续；全部到齐后再集中评审。代码截止 `1bededa`。

## 收到确认（请只回复这三行）

1. `PART 3/6 RECEIVED`
2. 本部分 `@@SECTION` 数量（应为 21）
3. 最后一个完整文件（若可见）

---

# 本部分附录

@@SECTION docs/03 `docs/notes/sync-review-packet-20261121.md`


`````markdown
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

`````



@@SECTION docs/04 `docs/notes/sync-fast-catchup-plan.md`


`````markdown
# 同步快速追赶（历史链压缩）计划

> 2026-11-20。针对“两台设备差异大时同步慢”的方案评估；**本文件只是计划，未改代码/wire**。
> 关联：ADR 0006（同步 v1）、ADR 0012（自动同步两阶段）、ADR 0013（生产 relay）。

## 1. 结论先行

- 现在慢的主要不是“relay 不会压缩”，而是**已有设备追赶时逐条回放 relay update 日志**：
  `SyncEngine::pull` 按页取日志（默认 256 条/页）→ 每条 `ingest_update` 落 inbox →
  `integrate_dependency_ordered` 再按对象集成。差异越大，事件数、解密/验签、写 inbox、
  Loro import 的次数都线性增长。
- **不建议**按“r1→r2→…→r7 只留 id、最后留 r7”直接改 relay：relay 只持有密文，
  不能解析/合并 Loro op；中间 update 在“还没有 checkpoint 覆盖、或有设备仍需要”时
  不能只留 id，否则会破坏因果/合并与回滚保护。Loro 也没有“单一获胜版本 r7”的概念，
  多设备是 op 合并，不是覆盖式版本链。
- **正确等价物**：relay 已有的 **checkpoint（全历史快照）+ 日志 GC** 就是“最终状态 + 可裁剪旧段”。
  当前缺的是**客户端在落后很多时主动用 checkpoint 快速追赶**，而不是逐条回放。
- 第一版可以**不改 relay wire/schema**：复用现有 `/bootstrap`（它已支持作用在已有库上，
  通过 `baseline_apply` 把快照 merge 进现有 Loro 文档，并保留本地离线 ops），
  追赶后从 checkpoint 覆盖位置继续拉增量。

## 2. 现状（代码依据）

- `crates/sync/src/engine.rs::pull`：
  - 只有 `state.gc_floor > local.received_seq` 才 `Paused(CursorExpired)`；否则从本地游标
    一页页拉日志；
  - `Update` 逐条进 inbox，`Asset` 做持久 skipped 记录；`checkpoint/control` 在此路径暂停。
- `crates/sync/src/bootstrap.rs`：
  - `/bootstrap` 取最新两份 published checkpoint，下载 manifest/snapshot/asset，全量校验后
    通过 `menghunote_storage::baseline_apply` 写入；
  - **允许已有库**：docs 先 `import(snapshot)`，再以旧 head VV 导出本地增量，保留本地离线 op；
    水位/游标只允许前进（`max` 语义 + CAS），不覆盖已有进度；
  - 失败走 staging journal，可续跑；fallback checkpoint 只在下载/校验阶段切换。
- `crates/storage/src/baseline_apply.rs`：快照导入 + 本地增量保留 + 资产 staging 的既有实现。
- relay `checkpoints`/`gc`：保留最近两份有效 checkpoint，日志前缀按覆盖/租约 GC；
  checkpoint 是密文快照，relay 不解析明文。

## 3. 方案对比

### A. 客户端 checkpoint 快速追赶（推荐，第一版）

**触发**（任一）：
- `head_seq - received_seq` 超过事件阈值（例如 2000 条）；
- 估算待拉字节超过阈值（例如 32 MiB）；
- `CursorExpired`（无可用日志窗口）；
- 或距离上次成功同步超过一定时间且差异大（谨慎，先用显式指标）。

**流程**：
1. flush 本地编辑（保证本地 op 已 durable、可导出为 baseline delta）；
2. 取 relay state/最新 checkpoint（/bootstrap 现有返回即可）；
3. 下载/解密/验签/校验 snapshot，`baseline_apply` merge 到现有 docs（保留本地未上传 op）；
4. 将 `received/integrated` 推进到 checkpoint S；**把 relay cursor 前移到 checkpoint cursor**
   （现有 bootstrap 为了“不回退”而保留旧 cursor；追赶场景需要在确认 S 覆盖后允许前进，
   否则会反复拉取已被快照覆盖的旧事件）；
5. 从 S 之后拉剩余 update，按正常路径集成；
6. 本地未上传 op 照常上传，最终双向收敛。

**优点**：不改 wire/schema；复用已验证的 bootstrap/baseline_apply；失败 fail-closed；
对“差异极大/游标过期”效果最好。
**代价**：checkpoint 是全历史快照，下载体积可能大于增量；必须按事件数/字节估算选择，
小差异仍走增量。

### B. relay 更积极地生成/裁剪 checkpoint（第二版）

- 当前 checkpoint 触发是 16 MiB 新增 update 或 24h（ADR 0006 的维护策略）。
- 可评估改为“事件数/字节/时间”三选一（例如 2–4 MiB 或 5–15 分钟或 1000 事件），
  让旧日志更早可 GC；代价是客户端上传快照更频繁、relay 存储/CPU 增加。
- 这不是“中间只留 id”，而是**更早把中间段替换成一份有签名的最终快照**；
  GC 仍必须保留两份有效基线。
- 需要单独压测 checkpoint 生成/上传成本，不能只看同步端。

### C. 浅快照 / 增量快照 / 文档级快照（wire v2，暂不做）

- 当前 `SnapshotV1` 强制 `full_history=true`（ADR 0006），不允许 state-only/shallow；
  这是为了保证任意设备可独立导入、CRDT 因果完整。
- 若要进一步压缩快照，需要协议 v2：base checkpoint + delta ops、每文档快照、
  或 shallow + 明确不可合并边界；涉及 wire/签名/GC/回滚，风险高，等 A/B 测量后再说。
- 不能把“只留 r1..r7 的 id”当低成本优化：没有 op 就无法重建/合并，ID 本身不携带变更。

### D. 点对点/局域网直连（长期）

- 如果两台设备在同一局域网，直接交换 Loro snapshot/update 可绕开 relay 往返；
  但这与当前“relay 为唯一传输”模型不同，涉及发现/鉴权/NAT，单独立项。

## 4. 分阶段计划

### Phase 0：度量与基线（必须先做）

- 增加同步计数（只读、脱敏）：`events_fetched`、`bytes_fetched`、`updates_integrated`、
  `snapshot_bytes`、`catchup_used`、各阶段耗时；面板/日志可见（与用户要的同步日志功能共用）。
- 构造两设备压测：A 生成 N 次编辑（例如 500/2000/10000 个 update，正文 1–10 MB），
  B 从不同落后位置追赶；记录时间、内存、DB 增长、relay 流量。
- 先找瓶颈：网络/解密验签/inbox 写入/Loro import/投影。没有数字不做“压缩”。

### Phase 1：checkpoint 快速追赶（MVP）

- 新增 `SyncCatchUp` 决策：根据 `head_seq`、`received_seq`、页面大小、checkpoint 覆盖 S
  估算 `replay` vs `snapshot+delta` 成本；默认保守阈值，可配置/可关闭。
- 复用 `bootstrap::run`（已有库路径）；bootstrap 后增加“追赶专用”游标前进：
  仅当写后 watermark `>= S` 且 checkpoint 校验通过，把 cursor CAS 到 checkpoint cursor。
- 失败路径：任何快照校验/合并/游标 CAS 失败 → 不前进、保留 journal/旧游标，回退普通 pull；
  不能把快照失败变成“卡在暂停”。
- 保留本地离线 op：用现有 `baseline_apply` 的“旧 head VV 导出增量”测试；
  追赶后本地编辑不能丢/不能重复。
- 验收：
  - 5k update、落后远端的设备追赶时间显著下降（目标 ≥50%，按实测调）；
  - 本地未上传编辑完整保留；
  - 快照损坏/签名失败/epoch 不符/预算不足时不前进、不丢数据；
  - 两张 checkpoint（primary/fallback）与 gc_floor 边界正确；
  - 新设备 bootstrap、已有设备追赶、近端增量三种路径都回归。

### Phase 2：relay 压缩策略调优

- 基于 Phase 0 数据决定 checkpoint 触发阈值（事件/字节/时间），只改策略不改 wire；
- 验证 GC 仍保留两份有效基线、读取租约、回滚防护；
- 目标：让“old log”更快被 checkpoint 覆盖，减少下一次追赶的 replay 量；
- 观察 relay 存储峰值/上传耗时，不接受“客户端快、服务器爆”的方案。

### Phase 3：仅在有数据支撑时做 wire v2

- 如果 Phase 1 的快照下载仍是主要成本（大库/多文档），再设计增量快照/文档级快照；
- 必须新 ADR、版本协商、签名/GC/回滚/兼容旧客户端；不做半成品。

## 5. 关键决策点（需确认）

1. **第一版是否接受“全历史快照”体积**？接受则 Phase 1 不需改 relay；不接受则要直接进 Phase 3（wire v2）。
2. **阈值策略**：按落后事件数、估算字节、还是固定“超过 N 分钟/游标将过期”？
   默认建议：`events > 2000 || estimated_bytes > 32 MiB || cursor_expired`，实测后调。
3. **本地未上传编辑**：快照 merge 后继续上传（推荐，现有 baseline_apply 已支持）还是先强制上传再追赶？
4. **relay 自动 checkpoint**：是否允许更高频维护（更多 CPU/带宽换更快追赶）？
5. **“中间版本只留 id”**：如果坚持，需要先定义“谁保证所有设备都已覆盖、谁有权丢弃密文 op”，
   否则会破坏离线设备/回滚保护；建议不接受。

## 6. 风险与边界

- Loro 不可“改写历史”；压缩只能是快照 + 日志裁剪，不能把 op 合并成单一 r7。
- checkpoint 是全历史快照，可能比增量更大；必须成本估算，不能无条件使用。
- bootstrap 在已有库上会 merge 快照；本地 op 保留逻辑依赖 `baseline_apply` 的旧 VV 导出，
  要为崩溃/resume/并发编辑补测试。
- 游标前进必须 CAS + 单调；不能因为追赶把 confirmed_highwater 回退。
- 同步中结构/正文写入持闸；追赶期间 UI 必须明确“正在快速追赶/全量重建”，不能假装空闲。
- relay 只存密文；任何“服务端按 revision 合并”的方案都需要明文或额外元数据，涉及隐私边界。

## 7. 建议下一步

先做 **Phase 0 + Phase 1**（不改 relay）：
1. 加同步计量与两设备压测脚本；
2. 用现有 `/bootstrap` 在已有库上做 checkpoint 追赶，补游标前进/本地 op 保留测试；
3. 拿实测数据决定是否调 checkpoint 频率，再决定是否上 wire v2。

这样能先回答“到底慢在哪”，避免为一个未验证的“只留 id”方案改 relay 协议。

---

## 8. 高级模型评审后的修正（2026-11-20）

评审同意 Phase 0/1、否定“服务端只留 id”，以下 6 点按代码结构逐条核对：

1. **触发公式**：必须用 `S - received_seq`（能被 checkpoint 真正抵消的窗口），不能只看
   `head - received`。`/bootstrap` 响应没有快照字节数，决策前要额外取一次 manifest
   （体积小、已签名）读 `docs.total_plain_len`；估算 replay 字节不足时用事件数 × 平均
   update 大小，另加单次快照硬上限（§9 已收紧为机会性 16 MiB；强制追赶只受本地磁盘预算）。
2. **Asset 不能“全部跳过”**：当前 `pull` 对 ASSET 并不是只留 skipped 记录，而是
   `persist_skipped_asset` **真实下载、验签、解密并落库**（否则同步后图片取不到）。
   `bootstrap::load_baseline` 也是无条件 `download_assets`，`verify_imported` 还要求
   每个资产字节存在。正确做法：用 `asset_bytes(vault, hash)` 预检，**只下载本地缺失的
   资产**；如果要做“完全不下载、按需拉取”，必须先补一个网络按需资产服务，否则会留下
   永久缺失的图片。
3. **CursorExpired**：当前 `pull` 直接 `Paused(CursorExpired)`，没有自动恢复；强制追赶失败
   不能回退普通 `pull`（会立即再次过期）。机会性失败可降级并冷却，强制失败必须进入
   明确的 Paused/指数退避。
4. **Inbox 清理**：`bootstrap::finalize` 把 `integrated` 推进到 S 后，≤S 的 inbox 行对
   coverage 证明可由 `integrated` 前缀覆盖；但**只能在 finalize 成功且 integrated ≥ S
   之后**清（或标记 covered），否则会破坏 checkpoint 的本地 coverage 证明。`sync_inbox`
   目前没有这个清理 API，需要新增。
5. **写闸临界区**：当前自动同步已有 `sync_prepare`（网络、不持闸）/ `sync_integrate`
   （持闸）两阶段，这是正确骨架；但 `bootstrap::run` 是“下载+校验+写库”一体的，
   **不能直接塞进 sync_once 持闸**。需要拆成：
   - `catchup_prepare`：不持闸，选 checkpoint、下 manifest/snapshot（+缺失 asset）、
     验签/验 VV，结果暂存在 worker（内存/临时文件，受上限保护）；
   - `catchup_integrate`：持闸，flush 本地、读旧 VV、`baseline_apply`、清 inbox、
     CAS 游标；随后普通 pull 拉 S 之后增量。
   导入本身仍可能占闸数秒，但网络绝不占闸；需要记录 `gate_hold_ms` 实测。
6. **Phase 0 批处理**：当前 `store_inbox_verified` 每条事件一次 `vault.exec`（独立事务），
   确有逐条 fsync/逐条 Loro import 的放大效应；压测必须包含“单页 256 条合并事务 /
   按文档聚合 import / 页面级一次投影”对照，不能只测快照方案。

另外两个评审点确认成立：fallback 必须用**实际选中的** checkpoint S 推进游标；
epoch/轮换跨越时先安全暂停/等待控制帧，不能把校验失败当损坏回退。

---

## 9. Phase 1A/1B 架构评审修订（2026-11-21）

> 本节是**权威约束**；§6 的“500 条”初值已被本节取代。Phase 1B 代码尚未开始，
> 需先完成 Stage 1（Phase 1A）的 release 标定并确认后再实现。

### 9.1 双模式触发与双向成本模型

**强制追赶（Mandatory）**
- 条件：`state.gc_floor > local.received_seq`（普通 `pull` 会 `CursorExpired`）。
- **不受任何机会性快照体积上限拦截**；只受本地磁盘安全预算保护（基线快照 + 临时
  staging + 资产解压的峰值需预留）。
- 失败必须进入明确的 `Paused` + 指数退避，**严禁回退普通 `pull`**（会立即再次过期，
  形成死循环）。成功后才允许恢复普通 pull。
- 强制追赶也不使用 `N_min`/成本交叉：只要能拿到合法 checkpoint，就必须尽快离开
  过期游标。

**机会性追赶（Opportunistic）**
- 先取已签名的小体积 `manifest`，读**实际选定** checkpoint 的 `S` 与
  `docs.total_plain_len`；fallback 时使用 fallback 的 `S`，不能拿 `head` 或 primary
  的 `S` 代替。
- 仅当同时满足才启动：
  1. `S.saturating_sub(local.received_seq) >= N_min`，默认 **`N_min = 25_000`**
     （可配置）。不得用 `head - received_seq`；
  2. **快照密文字节硬上限默认 `16 MiB`**（可配置范围 8–16 MiB）。这是机会性上限，
     不是 128 MiB 那种无分批保护的上限；超过即不启动机会性追赶，走普通聚合 pull。
     下限 8 MiB 用于更保守的 rollout；
  3. **双向成本交叉**：`estimated_replay_bytes >= max(8 MiB, 2.0 *
     manifest.docs.total_plain_len)`，**且**预估增量集成时间 >
     `1.5 *` 预估快照总耗时（下载 + 解密/验签 + merge + verify，按实测常数估）。
     防止为几百 KB 增量去拉十几 MB 全库快照。
- **单次持闸预算 3 000 ms**：`gate_hold_ms` 必须 < 3 s。选点时按当前 release 实测
  （16 MiB 优化后 storage merge ≈0.1 s，见 §9.4）估算；估算超预算就不进机会性追赶。
  实际持闸若超预算，`catchup_integrate` 必须中止并走无损回退，不能继续持闸。
- 任一环节失败：清理临时 staging、记录冷却窗口（指数退避）、**无损回退普通聚合
  pull**；不能卡死同步，也不能把失败当作 checkpoint 损坏而降级。

### 9.2 写闸拆分与 gate_hold_ms

- `catchup_prepare`（**不持写闸**）：`/bootstrap` 选 primary/fallback；fallback 必须
  把**实际选中的** `S` 与 `checkpoint_cursor` 一起绑定到后续 integrate，不能沿用
  primary 的值。验签 manifest 与 snapshot；用 `asset_bytes(vault, hash)` 预检
  （长度 + SHA256），**只下载本地缺失的资产**；全部校验（含 epoch/轮换）通过后
  暂存到有容量上限的 staging。
- epoch/rotation 跨越未处理控制边界时：安全暂停等待控制帧，**不得当作快照损坏降级**。
- `catchup_integrate`（**持写闸**）：先 flush 本地未落盘改动 → 分批/紧凑执行优化后的
  `baseline_apply`（每批 5 篇；复用 detached oplog 合并，避免 attached 大文本重放）→
  `finish_baseline_apply` 单调 CAS 推进水位与 cursor 到**实际选定且成功导入的**
  checkpoint 的 `S`/cursor（fallback 时误用 primary S 是硬错误）→ 记录 `gate_hold_ms`。
  `verify_imported` 放在闸外或单独预算内，不计入 3 s 持闸；若必须放入持闸，16 MiB
  上限需按 §9.4 的 verify 实测重新收紧。
- 复用 ADR 0012 现有 `sync_prepare`/`sync_integrate` 骨架，但 `bootstrap::run` 单体
  不能直接塞进 `sync_once` 的持闸段。

### 9.3 Inbox 清理与 Coverage 证明边界

- 新增 `prune_covered_inbox(vault, up_to_seq)`：**仅在 `finalize` 事务成功且持久化
  `integrated_seq >= S` 之后**清理（或标记覆盖）`relay_seq <= S` 的 inbox 记录。
- 清理前必须让 coverage 证明可由 `integrated` 前缀（及 durable skipped-asset/
  pending receipt 证据）覆盖；任何时刻崩溃都不能破坏 ADR 0006 的本地 checkpoint
  coverage 证明。
- 禁止在 finalize 之前按游标直接删除 inbox 行。

### 9.4 Stage 1A Release 标定结论与缺陷收口（2026-11-21）

`--release` 重跑后（详见 `sync-phase0-metrics.md` §8）：

- 聚合正文把 `doc_loads` 从“每条一次”降到“每文档每页一次”：单文档 2000 条
  `2000 → 8`、5000 条 `5000 → 20`；5000 条/20 篇 `5000 → 400`。
  integrate 墙钟：2000 单文档 `28.9 s → 86 ms`，5000 单文档 `147.0 s → 275 ms`，
  5000/20 篇 `14.9 s → 0.76 s`；**O(历史 × 事件) 已消除**。
- Phase 1A 缺陷收口：聚合路径先用 `load_head` 预检，纯已覆盖组不再重放（`doc_loads=0`）、
  纯 deferred 组不占预算（防跨文档饥饿）；同文档内存多轮 import 自愈物理乱序；
  `baseline_apply` 去掉二次 snapshot import，detached oplog 合并把 5/20 MiB
  storage merge 从 `2.7–4.1 s / 44.6–61.3 s` 降到 **35 ms / 110 ms**；
  `actor` 批量写锁改 `BEGIN IMMEDIATE`，`skipped_received` 扫描加
  `ORDER BY key LIMIT 4096` 并支持跨窗口推进。
- `baseline_apply` 5→20 MiB 旧实现的 **14.8–16.9×** 超线性根因：**Loro attached
  import 对“大文本 op + 非空本地文档”的 diff/state 应用**，占 gate 97.9–99.8%；
  snapshot decode 3.4×、pending_update 3.35×，都接近线性。detached oplog 合并绕开
  state/diff 后 gate 5→20 为 3.1–3.7×（4× 数据，亚线性）。
- **但闸外 `verify_imported` 仍 2.7 s / 45.1 s（16.7×）**：它仍走 attached
  `load_body_doc` 重放大 update。这是 Phase 1B 前的残余风险：`gate_hold` 小不代表
  首次打开/B 物化小；后续应把合并结果落 `loro_docs.snapshot`（`store_snapshot`
  已有）或等 Loro 侧优化，不能把 16 MiB 机会性上限当成“全链路 <3 s”。
- **`N_min=25 000`、`16 MiB` 机会性硬上限** 是本次 release 数据 + 3 s 持闸预算的
  默认值；最终值仍需用户确认。强制追赶不使用这两个阈值。
- Phase 1B **尚未实现**；上述阈值确认后再编码。
- Phase 1A 代码已落地：批量 inbox 原子化、按文档聚合 integrate、release 标定、
  precheck/乱序/防饥饿、baseline detached merge；完整评审入口见
  [`sync-review-packet-20261121.md`](sync-review-packet-20261121.md)，
  文件清单见 [`sync-phase1a-review-index.md`](sync-phase1a-review-index.md)。

`````



@@SECTION docs/05 `docs/notes/sync-phase0-metrics.md`


`````markdown
# 同步 Phase 0：度量与压测

> 2026-11-20。对应计划 [`sync-fast-catchup-plan.md`](sync-fast-catchup-plan.md) 的 Phase 0
> （§4 + §8 评审修正第 6 点）。**本阶段只加只读指标、默认关闭的实验开关与基准测试**；
> 没有实现 checkpoint 快速追赶（Phase 1），没有改 relay / `sync-proto` / schema / 游标语义。

## 1. 结论先行

- **集成（integrate）才是瓶颈，不是 relay、也不是 inbox 写入**。每个 remote update 都会
  `load_body_doc` → `load_replay_parts`（[`store.rs`](../../crates/storage/src/store.rs)）
  **完整重放**该文档历史后再 import 这一条；单文档热路径下成本近似
  `O(文档历史 × 本页事件)`。实测同一台机器、同 256 条/页：
  文档历史 0 行时 integrate ≈ **0.90 s**，历史 1792 行时 ≈ **6.8 s**（约 7.5 倍）。
- **inbox 逐条写确实很贵**：每行 2 次事务（`INSERT` + `received` 前缀推进各一次
  `write_meta`），WAL + `synchronous=FULL` 下每笔都是一次 fsync。实测 256 行
  **270 ms**（≈1.05 ms/行），1024 行 **799 ms**。
- 合并成 1 个事务后：256 行 **7.6 ms**（**35.6×**）、1024 行 **35 ms**（**22.7×**）。
  `pull` 阶段端到端（含 FakeRelay 网络/验签/解密）**10–11×**。
- 但两设备追赶的**总时间只快 1.06–1.10×**——integrate 占 83 %（600 条）到 94 %（2000 条）。
- 因此 Phase 1 的优先级建议是：**① 按文档聚合 import（最便宜、收益最大）→ ② inbox 批量写
  默认开启 → ③ checkpoint 快速追赶**。详见 §6。

## 2. 环境与命令

- Linux 6.8.0（Ubuntu），Intel Xeon Gold 6330 @ 2.00 GHz，8 vCPU，ext4（`/dev/sda1`），
  运行时空闲 72 G；`rustc`/`cargo` **1.100.0**。
- workspace `[profile.dev] opt-level = 1`（依赖 `opt-level = 2`）；SQLite
  `journal_mode=WAL`、`synchronous=FULL`、`busy_timeout=5000`（ADR 0001 §3.9）。
- 无真实 relay / TLS / 网络：`FakeRelay` 是纯内存 relay（不 sleep），所以**网络字节未测**，
  下面的 `update_bytes` 是**明文**口径（见 §3.1）。

```bash
export LIBRARY_PATH=/home/dev/xkb-sysroot/root/usr/lib/x86_64-linux-gnu
export PKG_CONFIG_PATH=/home/dev/xkb-sysroot/root/usr/lib/x86_64-linux-gnu/pkgconfig

# 基准 A：storage 层 逐条 vs 批量写 inbox（N=256/1024，每轮新建 vault，取中位数）
cargo test -p menghunote-storage --test sync_inbox_bench -- --ignored --nocapture

# 基准 B：两设备落后追赶（A 产生 600/2000 条 update，B 从 c0 逐页 pull+integrate）
cargo test -p menghunote-sync --test phase0_catchup -- --ignored --nocapture

# 指标/批量一致性的常规回归（非 ignored，进 CI）
cargo test -p menghunote-sync --test engine
cargo test -p menghunote-storage --test sync_inbox_batch
```

## 3. 本阶段新增的度量与 API

### 3.1 `PullReport`（只读，`Default + PartialEq + Eq` 保持）

| 字段 | 口径 |
| --- | --- |
| `update_bytes: u64` | 成功验收的 `UpdateV1` **明文负载**之和（= inbox `verified_payload` 长度）；不含每片 196 B 信封头 + 16 B AEAD tag，是网络字节的**下界** |
| `asset_bytes: u64` | 本次真实下载 + 验证 + 落内容寻址表的 ASSET **明文字节**（含幂等重放时的重新下载） |
| `elapsed_ms: u64` | `pull` 墙钟耗时（`as_millis` 截断；<1 ms 记 0） |
| `pages: usize` | 本次请求/消费的日志页数（当前实现每次只拉一页，成功返回恒为 1；留给 Phase 1 多页路径复用） |
| `inbox_batch: bool` | 本页 verified 行是否用**一个事务**批量写入（`EngineConfig::inbox_batch`，默认 `false`） |

`IntegrateReport` 增加 `elapsed_ms: u64`（整个 integrate 阶段，含结构/投影/正文轮转）。
`PrepareReport` / `SyncReport` 因嵌套自动携带。`pull` / integrate 成功返回前各打一行
**脱敏** `eprintln!`（事件、字节、耗时；不含路径/密钥/明文）：

```text
[sync] pull events=256 stored=256 skipped=0 update_bytes=72438 asset_bytes=0 pages=1 inbox_batch=false elapsed_ms=223
[sync] integrate integrated=256 structure=0 body=256 deferred=0 batches=8 projected=false elapsed_ms=2191
```

worker 摘要（`OpResult::Ok.summary`，用于发布基线 notice、dev 日志与
`SyncOnceReport.worker_summary`）也追加同一组指标，例如
`推送 0 / 拉取 256 / 整合 256 / 延后 0（UPDATE 72438 B / ASSET 0 B / 拉取 223 ms / 整合 2191 ms）`；
只做展示，不参与任何同步决策。

**常规同步的面板行没有改**（仍是「完成（N 条变化）」）：要让面板行显示字节/耗时，需要
把指标穿到 `SyncOnceReport`/面板行（`shell.rs` 的 finish 路径 + `state.rs` 的报告结构），
属于 UI 改动而不是 Phase 0 的度量目标。Phase 0 的可见入口是上面两行 `[sync]` 日志和
报告字段；面板展示留到确实需要时再做（任务书允许的取舍）。

### 3.2 批量 inbox API（storage）

`store_inbox_verified_batch(vault, &[VerifiedInboxRow]) -> Result<usize, StorageError>`：
先按 `relay_seq` 稳定排序（重复 seq 拒绝），一次读既有行做冲突检测（**冲突整批不写**），
新行在**同一 `exec_transaction`** 插入（每条语句有 `AffectedGuard::exactly(1, 1)`），
事务后一次性推进 `received` 连续前缀。幂等/冲突语义与逐条入口一致；返回新插入行数。

同时把 `advance_received_prefix` / `advance_integrated_prefix` 从"每前进一个序号写一次
`meta`"改成"扫窗口 → 内存走连续段 → **一次**写最终值"（语义逐字等价，崩溃只会少推进、
下次重算）。这是批量 API 能体现收益的前提，也直接降低逐条路径的前缀写盘次数。

生产 `pull` 默认仍逐条写；`EngineConfig::inbox_batch = true` 时才整页攒行、一个事务写入
（`PULL_INBOX_BATCH_BYTES = 8 MiB` 分批上限，防一页大 update 占内存）。

## 4. 基准 A：storage 层 inbox 逐条 vs 批量

每行 payload 4 KiB，每轮新建 vault，预热 1 轮后测 3 轮取中位数；原始输出：

```text
[bench] target_tmp=/home/dev/menghunote/target/tmp payload_bytes=4096 rounds=3 (每轮新建 vault)
[bench] n=256 round=0 per_row=269.306527ms batch=7.337516ms
[bench] n=256 round=1 per_row=272.31969ms batch=7.591259ms
[bench] n=256 round=2 per_row=270.095254ms batch=7.759615ms
[bench] n=256 per_row_median=270095us batch_median=7591us speedup=35.58x rows=256 payload=4096B
[bench] n=1024 round=0 per_row=799.252944ms batch=34.274916ms
[bench] n=1024 round=1 per_row=789.769334ms batch=35.242095ms
[bench] n=1024 round=2 per_row=829.437318ms batch=37.65859ms
[bench] n=1024 per_row_median=799252us batch_median=35242us speedup=22.68x rows=1024 payload=4096B
```

| N | 逐条中位数 | 批量中位数 | 加速比 | 逐条 ≈ /行 |
| --- | --- | --- | --- | --- |
| 256 | 270.1 ms | 7.6 ms | **35.6×** | 1.06 ms |
| 1024 | 799.3 ms | 35.2 ms | **22.7×** | 0.78 ms |

同一份代码在另一时段复跑一次（机器上还有其它任务在跑，绝对时间整体偏高）：
`n=256 per_row=304.4ms batch=8.0ms speedup=37.92x`、
`n=1024 per_row=971.8ms batch=37.8ms speedup=25.70x` —— 加速比同量级。

读法：逐条路径每行 = 1 次 insert 事务 + 1 次前缀写事务（2 次 fsync）；批量路径整批 =
1 次 insert 事务 + 1 次前缀写事务。N 越大加速比回落到 20× 左右，是批量路径里
`exec_transaction` 自身的 CPU/绑定开销开始显现。

## 5. 基准 B：两设备落后追赶（A 先产生 N 条 update，B 从 c0 追）

A 每条 update ≈ 64 B 正文（`-00001-xxxx…`），顺序提交并推完；B 每页 256 条
`pull → integrate` 直到 `events == 0`。原始摘要（同一台机器；数字取自带 replay 探针的
那次运行）：

```text
[phase0] edits=600 per_row: rounds=4 events=600 update_bytes=168224 pull_ms(wall)=674  integrate_ms(wall)=3426
[phase0] edits=600 batch:   rounds=4 events=600 update_bytes=168224 pull_ms(wall)=60   integrate_ms(wall)=3679
[phase0] edits=600 pull_speedup=11.23x total_speedup=1.10x (per_row_total=4100ms batch_total=3739ms)

[phase0] edits=2000 per_row: rounds=9 events=2000 update_bytes=565425 pull_ms(wall)=1981 integrate_ms(wall)=31753
[phase0] edits=2000 batch:   rounds=9 events=2000 update_bytes=565425 pull_ms(wall)=189  integrate_ms(wall)=31735
[phase0] edits=2000 pull_speedup=10.48x total_speedup=1.06x (per_row_total=33734ms batch_total=31924ms)
```

逐轮数据（batch 配置；`history_rows` = 本轮开始前已集成行数，`replay_probe_ms` =
对本轮文档做一次完整 `load_body_doc` 重放的实测耗时，**不计入** pull/integrate 计时）：

```text
[phase0] batch=true round=1 history_rows=0    replay_probe_ms=0  events=256 pull_ms=25 integrate_ms=904  integrated=256 deferred=0
[phase0] batch=true round=2 history_rows=256  replay_probe_ms=3  events=256 pull_ms=24 integrate_ms=1754 integrated=256 deferred=0
[phase0] batch=true round=3 history_rows=512  replay_probe_ms=7  events=256 pull_ms=24 integrate_ms=2724 integrated=256 deferred=0
[phase0] batch=true round=4 history_rows=768  replay_probe_ms=10 events=256 pull_ms=23 integrate_ms=3605 integrated=256 deferred=0
[phase0] batch=true round=5 history_rows=1024 replay_probe_ms=14 events=256 pull_ms=24 integrate_ms=4556 integrated=256 deferred=0
[phase0] batch=true round=6 history_rows=1280 replay_probe_ms=17 events=256 pull_ms=26 integrate_ms=5557 integrated=256 deferred=0
[phase0] batch=true round=7 history_rows=1536 replay_probe_ms=21 events=256 pull_ms=24 integrate_ms=6764 integrated=256 deferred=0
[phase0] batch=true round=8 history_rows=1792 replay_probe_ms=25 events=208 pull_ms=19 integrate_ms=5870 integrated=208 deferred=0
```

读法：

- `replay_probe_ms` 随历史**线性**增长（0 → 3 → 7 → 10 → 14 → 17 → 21 → 25 ms / 256 行）。
- 每轮 integrate ≈ `本页事件数 × 单次全量重放`（256 × 25 ms ≈ 6.4 s，与实测 6.8 s 吻合），
  即单文档热路径下 integrate 是**二次**成本。
- pull 侧：逐条 170–425 ms/页，批量 19–30 ms/页（≈10×，低于 storage 层的 35×，
  因为这里也含 FakeRelay 取片、签名校验、AEAD 解密等固定成本）。
- 同一场景在不同时段跑了多次（机器上还有其它负载），绝对时间相差约 5–20 %；
  结论按量级/比值读，不要按单次毫秒数做阈值。

## 6. 结论与 Phase 1 建议

**是否先做批量 inbox？** 值得做（改动小、语义等价、pull 阶段约 10–11×），但**不是追赶的主解**：
integrate 占 83–94 %，批量 inbox 只把总数改善 1.06–1.10×。

**真正便宜的下一步（建议排在 checkpoint 追赶之前）**：按文档聚合 import——
一页/一批里同一文档的行只 `load_body_doc` 一次、批量 `import`、一次 A 事务追加、
一次 `mark_integrated`。按上面的实测，这能直接消掉 integrate 里最大的 `O(历史 × 事件)`
项（本轮数据里它相当于 256 行 × 每行重放，6.8 s → 预估与"历史长度无关"的一次重放量级）。
这不是 Phase 1 的方案变更，但它会改变快照追赶的性价比，建议 Phase 1 先量它再定阈值。

**checkpoint 快速追赶阈值（旧初值，已被 §8.5 / 计划 §9.1 取代，仅留作历史）**：

```text
S - received_seq >= 500 事件
或 estimated_replay_bytes >= 8 MiB
或 Pull 返回 Paused(CursorExpired)
且 snapshot 明文总量 <= 128 MiB（§8.1 的硬上限）
```

依据：本场景单事件成本 ≈ 1.1 ms（inbox 写）+ 3.5–26 ms（integrate 重放，随文档历史增长：
历史 0 时 3.5 ms/条，历史 1792 时 26 ms/条），500 事件 ≈ 2–13 s 的 replay；2000 条 update 的
明文合计只有 565 KB，本机快照下载/AEAD 成本可忽略（**merge 成本未测**，见 §7）。所以 500 事件
这个量级已经值得走快照，而不是等 2000。`S` 必须取**实际选中的** checkpoint（§8 第 7 点），
不能用 head。

## 7. 未测 / 限制

- **没有**真实 relay/TLS/网络：`FakeRelay` 内存日志，网络字节、丢包/重试、真实 relay GC
  未测；`update_bytes` 是明文口径（wire 体积需另加"分片数 × 212 B"）。
- **没有**快照（`/bootstrap` + `baseline_apply`）的下载/merge 耗时、`gate_hold_ms`、
  inbox 清理 API —— 这些属于 Phase 1，本阶段只有阈值建议，没有实测。
- 场景是**单文档热路径**（对重放是最坏情形）；多文档、正文 1–10 MB、附件资产在追赶中的
  占比未测。
- Windows / DPAPI / 真机磁盘未测（本机为 Linux + ext4）。
- 常规回归（非 ignored）已覆盖：`pull_and_integrate_reports_phase0_metrics`（口径对账）、
  `pull_inbox_batch_flag_matches_per_event_results`（批量开关结果等价）、
  `crates/storage/tests/sync_inbox_batch.rs`（8 个批量语义/冲突/前缀用例）。

## 8. Phase 1A Release 实测（2026-11-21）

> 本节全部数字来自 **`--release`** 重跑（默认 `opt-level=3`、`debug=0`、无 LTO），
> **不要与 §4/§5 的 dev（workspace `opt-level=1`）数字直接比较**。
> 本节只补基准与文档：不改生产代码、不改 plan、未 commit/打包。
>
> Phase 1A 生产改动（未提交，HEAD `ec9ecff` 之上）：`EngineConfig.inbox_batch` 默认 true、
> 整页 inbox 单事务、`IntegrateConfig.aggregate_body` 默认 true（聚合路径下
> `batch_objects` = 每批**文档数**）、`IntegrateReport.doc_loads`、storage
> `append_updates_with_integrated_marks` / `mark_integrated_batch` /
> `advance_integrated_prefix`。

### 8.1 环境与命令

- Linux 6.8.0-149（Ubuntu），Intel Xeon Gold 6330 @ 2.00 GHz，8 vCPU，15 GiB RAM，
  ext4（`/dev/sda1`，运行时空闲 28 G）；`rustc` / `cargo` **1.100.0**。
- 运行前后机器 load average 约 0.3–1.2（8 vCPU），未见常驻构建任务；矩阵测试
  单进程串行（同一进程内三种模式依次跑）。
- SQLite 仍是生产配置：`journal_mode=WAL`、`synchronous=FULL`、`busy_timeout=5000`。
- 时间口径：**墙钟** `Instant`；`pull_ms` / `integrate_ms` 也会同时记录生产
  `PullReport` / `IntegrateReport` 的内部 `elapsed_ms`（两者相差 ≤2 ms，表格用墙钟）。
  `gate_hold_ms` = `begin` + 各批 `apply` + `finish` 的墙钟和（Phase 1B 持闸 merge 的
  估算口径；不含 snapshot 下载/解密/验签，也不含 flush 本地改动与 inbox 清理）。

```bash
export CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 \
  LIBRARY_PATH=/home/dev/xkb-sysroot/root/usr/lib/x86_64-linux-gnu \
  PKG_CONFIG_PATH=/home/dev/xkb-sysroot/root/usr/lib/x86_64-linux-gnu/pkgconfig

# dry compile
cargo test --release -p menghunote-sync --test phase0_catchup --test phase1a_release --no-run
cargo test --release -p menghunote-storage --test sync_inbox_bench --no-run

# 正式运行（全部 --release + --ignored）
cargo test --release -p menghunote-storage --test sync_inbox_bench -- --ignored --nocapture
cargo test --release -p menghunote-sync --test phase0_catchup -- --ignored --nocapture
cargo test --release -p menghunote-sync --test phase1a_release -- --ignored --nocapture
```

三次运行的进程内耗时（含 warmup / 校验，仅作记录）：storage bench **4.77 s**，
phase0 矩阵 **478.47 s**，baseline_apply 探针 **95.11 s**（跑了两遍确认可重复）。
原始输出保存在 `target/tmp/phase1a-logs/`（`run-*.log`；`target/` 不入库）。

### 8.2 storage 批量 inbox（4 KiB/行，3 轮中位数）

```text
[bench] n=256  round=0 per_row=269.297234ms batch=5.158311ms
[bench] n=256  round=1 per_row=282.411711ms batch=5.337196ms
[bench] n=256  round=2 per_row=283.711569ms batch=5.457038ms
[bench] n=256  per_row_median=282411us batch_median=5337us speedup=52.91x
[bench] n=1024 round=0 per_row=819.404747ms batch=26.534073ms
[bench] n=1024 round=1 per_row=757.321859ms batch=23.285371ms
[bench] n=1024 round=2 per_row=941.195746ms batch=25.157710ms
[bench] n=1024 per_row_median=819404us batch_median=25157us speedup=32.57x
```

| N | 逐条中位数 | 批量中位数 | 加速比 | 逐条 ≈ /行 |
| --- | --- | --- | --- | --- |
| 256 | 282.4 ms | **5.34 ms** | **52.9×** | 1.10 ms |
| 1024 | 819.4 ms | **25.16 ms** | **32.6×** | 0.80 ms |

读法与 Phase 0 相同：逐条路径每行一次 insert 事务 + 一次前缀写事务（`synchronous=FULL`
下都是 fsync），所以 release 优化对逐条几乎无效；批量路径整批一次事务，收益主要来自
fsync 次数减少。N=1024 时批量本身的事务 CPU 开始显现，加速比从 52.9× 回落到 32.6×。

### 8.3 两设备落后追赶（三种模式）

三种模式（`crates/sync/tests/phase0_catchup.rs`）：

| 模式 | `inbox_batch` | `IntegrateConfig.aggregate_body` | 含义 |
| --- | --- | --- | --- |
| `per_row` | `false` | `false` | Phase 0 逐行：每条 update 一次 inbox 事务 + 一次 `load_body_doc` 全量重放 |
| `batch` | `true` | `false` | Phase 0 批量 inbox + 逐行集成 |
| `aggregate` | `true` | `true` | Phase 1A 目标：整页 inbox 一事务 + 同一文档一批只全量重放一次 |

集成一律走 `integrate_dependency_ordered(config)`（不是 `integrate(limit)`），否则
`aggregate_body` 无法显式关闭。**所有模式的结果硬断言都通过**：B 最终正文 = A、
`received_seq == integrated_seq == relay.head_seq`、事件数/字节对账、
`stored == updates`、`skipped == 0`、`asset_bytes == 0`、每轮 `deferred == 0`。

#### 8.3.1 单文档 600 / 2000（每条 update ≈ 64 B，正文总量 38 KB / 128 KB）

| 场景 | 模式 | pull 墙钟 | integrate 墙钟 | doc_loads | batches | 总墙钟 | integrate 加速 | 总加速 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 600 | `per_row` | 548 ms | 2 965 ms | 600 | 19 | 3 513 ms | 1.00× | 1.00× |
| 600 | `batch` | **94 ms** | 5 213 ms | 600 | 19 | 5 307 ms | 0.57× | 0.66× |
| 600 | `aggregate` | **52 ms** | **31 ms** | **3** | 3 | **83 ms** | **95.7×** | **42.3×** |
| 2000 | `per_row` | 2 253 ms | 28 891 ms | 2000 | 63 | 31 144 ms | 1.00× | 1.00× |
| 2000 | `batch` | **162 ms** | 25 741 ms | 2000 | 63 | 25 903 ms | 1.12× | 1.20× |
| 2000 | `aggregate` | **158 ms** | **86 ms** | **8** | 8 | **244 ms** | **335.9×** | **127.6×** |

`aggregate` 每页（256 事件）的 `integrate` 稳定在 **10–13 ms**，`doc_loads` 恒为 1
（600 → 3 页 3 次；2000 → 8 页 8 次）；`per_row`/`batch` 每页 8 批、`doc_loads = 本页事件数`，
且随文档历史增长（2000 条时单页 0.7 s → 5.1 s）。这直接复现并消除了 Phase 0 的
`O(历史 × 事件)`：聚合前三页/八页都要为每条事件重放一遍历史，聚合后每页只重放一次。

> `batch` 与 `per_row` 的 **integrate 是同一段代码**（`aggregate_body=false`），
> 两者 integrate 的差异应视为磁盘 fsync 抖动：600 的 batch integrate 偏高（0.57×）
> 是单次运行波动；2000 / 5000 的 integrate 比值回落到 0.98–1.12×。每轮原始行见
> `run-phase0-catchup.log`。

#### 8.3.2 多文档 5000 条 / 20 篇（每篇 250 条 × 439 B = 109 750 B ≈ 107 KiB）

A 在 20 篇文档上**按 doc 轮转**提交（每轮每篇一条，共 250 轮），且**每轮提交后立刻推送**：
生产 `outbox_rows` 按 `doc_id,seq` 排序，只有每篇每轮恰好一条 pending 时，relay 日志
才是"逐轮、跨文档交错"的（否则会先把第一篇的 250 条推完，改变每页多文档分布）。
B 先 `seed_doc` 20 篇 + cursor，再逐页 `pull → integrate` 到追平（21 轮含末尾空页）。

| 模式 | pull 墙钟 | integrate 墙钟 | doc_loads | batches | 总墙钟 | integrate 加速 | 总加速 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `per_row` | 4 142 ms | 14 897 ms | 5000 | 157 | 19 039 ms | 1.00× | 1.00× |
| `batch` | **421 ms** | 14 736 ms | 5000 | 157 | 15 157 ms | 1.01× | 1.26× |
| `aggregate` | **406 ms** | **763 ms** | **400** | 20 | **1 169 ms** | **19.5×** | **16.3×** |

- 事件对账：`events = stored = 5000`、`skipped = 0`、`asset_bytes = 0`、
  Update 明文合计 `3 324 240 B`（≈3.17 MiB）、`update_bytes > 0`；
  `received_seq = integrated_seq = 5000`；20 篇正文逐篇 `B.text() == A.text()`。
- `aggregate` 的 `doc_loads = 20 × 20 = 400`：每页 20 篇各重放一次；`per_row` 是 5000 次
  （每条一次）。per-page integrate 维持在 28–50 ms（本页文档历史从 0 长到 110 KB），
  但仍与"每页 256 条"解耦。
- 同一份日志下，多文档比"5000 条全压一篇"便宜得多（见 8.3.3）：per_row 14.9 s vs 147 s，
  因为重放成本是 `每篇历史 × 处理次数`，不是"总事件数"。这也说明多文档分布本身能摊薄
  重放成本。

#### 8.3.3 补充：5000 条单文档（可选场景，同一篇 64 B 小编辑）

| 模式 | pull 墙钟 | integrate 墙钟 | doc_loads | batches | 总墙钟 | integrate 加速 | 总加速 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `per_row` | 3 768 ms | 147 022 ms | 5000 | 157 | 150 790 ms | 1.00× | 1.00× |
| `batch` | **406 ms** | 149 664 ms | 5000 | 157 | 150 070 ms | 0.98× | 1.00× |
| `aggregate` | **401 ms** | **275 ms** | **20** | 20 | **676 ms** | **534.6×** | **223.1×** |

这是单文档最坏情形：per_row 每页处理 256 条时重放的历史从 0 长到 4 864 行，
单页 integrate 从 0.77 s 单调升到 14.1 s（末页 136 条 7.8 s）——就是 `O(历史 × 事件)`；
`aggregate` 每页只重放一次（20 页 = 20 次），单页 10–20 ms，不随历史增长。
这一档也说明"多文档/单文档"必须分开看：同样 5000 条，多文档 aggregate 总耗时 1.17 s，
单文档 0.68 s，而 per_row 分别是 19.0 s / 150.8 s。

### 8.4 `baseline_apply` 持闸 merge（旧实现，优化前；5 MiB / 20 MiB）

> 评审包/代码地图见 [`sync-review-packet-20261121.md`](sync-review-packet-20261121.md)。
> 本节是 **Phase 1A 优化前**（HEAD `87a6c5b`）的数据，保留作根因对照；优化后的
> detached oplog merge 数据见 §8.4b。两次测量受 fsync/机器负载影响，但量级一致。

探针（`crates/sync/tests/phase1a_release.rs`）只测 **storage merge**，不测 relay/checkpoint
签名/真实下载：

- fresh vault 上准备 20 篇文档，每篇已有本地正文（`LOCAL_UNSYNCED_xx` + 2 KiB 文本）和
  一条**未上传** delta（`append_updates`，无 outbox 意图）；旧水位 `received=100 /
  integrated=80 / highwater=90 / cursor=c80`。
- 用 `BodyDoc::export_snapshot()` 构造 20 篇远端 snapshot，合计目标 5 MiB / 20 MiB
  （实际 4.96 / 19.83 MiB，偏差 −0.8% / −0.9%）。每篇文本是确定性伪随机的
  `REMOTE_BASELINE_xx|…`，snapshot/文本 ≈ **2.0×**（约等于每字符 2 字节；一篇一个
  "一次性插入"op，不是真实编辑历史）。
- `begin_baseline_apply(checkpoint_id, S=500, "c0-source")` →
  `apply_baseline_batch`（每批 5 篇，共 4 批）→ `finish_baseline_apply(500, "c500")`。
  只打印，不 assert 时间；`S`/cursor 是自造合法值，没有伪造 checkpoint 签名。

| 档 | 实际快照总量 | 每篇 snapshot | 每篇文本 chars | begin | apply 4 批（5 篇/批） | apply 合计 | finish | **gate_hold_ms** | 闸外 verify | 总墙钟 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 5 MiB | 5 202 500 B（4.96 MiB） | 260 125 B | 129 919 | 0 ms | 676 / 682 / 671 / 689 ms | 2 718 ms | 0 ms | **2 718 ms** | 2 653 ms | 5 450 ms |
| 20 MiB | 20 792 660 B（19.83 MiB） | 1 039 633 B | 519 673 | 2 ms | 11 039 / 11 259 / 10 928 / 11 345 ms | 44 571 ms | 1 ms | **44 574 ms** | 45 052 ms | 89 774 ms |

第二次跑（同一二进制、fresh vault）复核：5 MiB `gate_hold=2 692 ms`（verify 2 630 ms，
总 5 395 ms）；20 MiB `gate_hold=45 907 ms`（verify 43 630 ms，总 89 689 ms）。
5→20 MiB 是 4× 数据、**≈16.9×** gate 时间——本探针的"一篇一个巨大文本 op"形状下
merge 是超线性的；**不能**把该倍率外推到 128 MiB 上限。

正确性（两种档位都通过，且都在闸外校验）：

- 每篇 `merged.text()` 同时含本地 `LOCAL_UNSYNCED_xx` 与远端 `REMOTE_BASELINE_xx`；
  `loro_updates` 里 `batch_id='local'` 20 行、`batch_id='baseline_merge'` 20 行；
  `sync_outbox` 仍为 0（本地 delta 未被上传、也未被覆盖）。
- 水位单调：`100/80/90 → 500/500/500`（received/integrated/confirmed_highwater），
  relay cursor `c80 → c500`；`finish` 返回 `BaselineApplyFinish::Applied`，
  `status=Completed`、`finishable=false`，没有残留 staging journal。
- `gate_hold_ms` 不含 `apply_baseline_assets`、inbox 清理、本地 flush；真实 Phase 1B
  的持闸还会叠加这些成本。

### 8.4b `baseline_apply` 优化后（detached oplog merge，5 MiB / 20 MiB）

同一探针、同一 20 篇 × 每篇一个一次性长文本 op 的合成形状；改动是“每篇 snapshot
只 restore 一次、本地小 delta 只合并进 **oplog**（detached，不重算 state/diff）、
用 `pending_update_from_oplog` 导出差集”。

| 档 | gate_hold（Lead 复跑） | restore | head 查询 | 本地重放 | 本地 delta 导出 | delta 反向导入 | pending_update | SQLite 事务 | 闸外 verify | 5→20 gate |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 5 MiB | **35 ms**（agent 31 ms） | 4.3 | 1.5 | 2.7 | 0.4 | 0.2 | 4.9 | 14.8 | 2.69 s | — |
| 20 MiB | **110 ms**（agent 114 ms） | 16.3 | 2.1 | 3.0 | 0.5 | 0.2 | 18.0 | 45.2 | 45.10 s | **3.1×**（4× 数据） |

同机 agent 对照（优化前 → 优化后）：5 MiB `4 143 → 31 ms`（≈134×），
20 MiB `61 308 → 114 ms`（≈538×），5→20 倍率 `14.8× → 3.7×`。
本机早先旧实现为 `2 718 / 44 574 ms`（§8.4）；机器/负载不同，用**倍率**读。

**根因（实测拆分，不是猜测）**

- 不是文档数/ops 数：两档都是 20 篇、每篇 1 op。
- 不是 snapshot decode：`restore` 6.3 → 21.5 ms（3.4×，≈线性）。
- 不是 pending_update 导出：5.7 → 19.1 ms（3.35×，≈线性）。
- **是 Loro attached import**：旧路径把大 snapshot 二次 import 进非空本地文档，
  `4 058 → 61 170 ms`（15.1×），占 gate_hold **97.9% / 99.8%**；把它换成“小本地
  delta 导入大基线文档”仍 `3 598 → 59 887 ms`（16.6×）。同一文本 op，无论大侧在
  导入方还是已有方，只要 attached 就超线性。
- detached oplog 只合并 op，完全绕开 state/diff：反向导入 0.2 ms；合并后的
  oplog frontier 与重放 state frontier 一致（每个用例都由 `load_body_doc` 的
  frontier 校验兜底）。
- **残余**：闸外 `verify` 仍 `2 857 → 45 720 ms`（16.0×）；它走 attached
  `load_body_doc` 重放合并后的大 update。真实系统 B 物化/首次打开仍会付这个成本；
  本次只治理了持闸内的 merge，不能宣称全链路线性。

### 8.5 结论

1. **聚合确实消除了 `O(历史 × 事件)`，不是只换了个循环。** 直接证据是
   `doc_loads`：单文档 2000 条 `2000 → 8`、5000 条 `5000 → 20`；多文档 5000 条
   `5000 → 400`（= 20 篇 × 20 页，每篇每页一次）。时间同步下降：2000 条
   integrate `28.9 s → 86 ms`（336×）、5000 条单文档 `147.0 s → 275 ms`（535×）、
   5000 条/20 篇 `14.9 s → 0.76 s`（19.5×）；per-page integrate 不再随历史增长
   （单文档 10–13 ms/页；20 篇文档 28–50 ms/页）。聚合后的剩余成本是
   "每页页内文档数 × 每篇一次重放 + 每篇一次组合 A 事务"，不再是"每条事件 × 整段历史"。
2. **批量 inbox 仍是独立的必要优化。** pull 侧 release 实测 5.8–14.3×（单文档）
   和 9.8–10.2×（5000/20 篇、5000 单篇）。在逐行/批量集成下它只把总时间改善
   1.2–1.26×；聚合后 integrate 被压到很小，pull 变成多文档场景的 ~35%（406 / 1 169 ms）
   和单文档 5000 的 ~59%（401 / 676 ms），批量 inbox 的占比明显上升。
3. **release 重跑后的量级判断。** `per_row`/`batch` 的 integrate 仍由 SQLite 事务与
   `synchronous=FULL` fsync 主导，release 编译器优化空间有限；真正的量级变化来自
   Phase 1A 的结构性改动（聚合 + 批量标记），而不是 profile。因此 Phase 0 的
   "integrate 是瓶颈"在逐行路径上仍成立，而聚合路径把瓶颈移到了 pull/网络与
   snapshot merge。
4. **`N_min = 25 000`，且必须做双向成本交叉。** 聚合处理 5000 条尾部只要
   **0.28 s（单文档）/ 0.76 s（20 篇）**；而优化后 5 MiB merge 的 `gate_hold`
   已降到 **35 ms**、20 MiB **110 ms**。阈值不应再按“merge 很慢”外推，而应按
   “下载/解密/验签 + 闸外 verify + 首次重放”的总成本：16 MiB 的闸外 verify 仍约
   45 s（20 MiB 形状），所以 5 000 条尾部远不足以摊平全链路。计划 §9.1 定为
   **25 000 事件 + 2.0× 重放/正文比 + 1.5× 时间交叉 + 16 MiB 机会性硬上限**；
   强制追赶（`gc_floor > received_seq`）不受 `N_min`/体积上限限制，仍直接走快照。
   最终值需要用户确认，并用真实 snapshot 形状复测。
5. **16 MiB 机会性上限 + 3 s 持闸预算，不是 128 MiB 无保护上限。** 优化后 storage
   merge 的 5→20 MiB 已回到 3.1×（4× 数据，亚线性），16 MiB 的 gate 估算远低于
   3 s；但闸外 verify 仍是 16×，且真实网络/解密不在探针内。因此机会性上限锁在
   **8–16 MiB**（默认 16 MiB），并保留“估算超 3 s 不启动 / 实测超 3 s 中止回退”；
   128 MiB 只能在强制追赶且本地磁盘预算允许时考虑，不能走机会性路径。

### 8.6 未测 / 限制

- **没有真实 relay/TLS/网络**：`FakeRelay` 纯内存，`pull` 的数字不含真实 RTT、TLS、
  丢包/重试；`update_bytes` 仍是 `UpdateV1` 明文口径（wire 还要加每片 196 B 信封头 +
  16 B AEAD tag）。
- **baseline_apply 探针是 storage merge，不是真实下载**：没有 `/bootstrap` 选点、
  manifest 验签、snapshot 解密/摘要校验、资产下载、inbox 清理、本地 flush；`S`/cursor
  是自造值，无 checkpoint 签名。`gate_hold_ms` 只覆盖 begin/apply/finish；优化后
  **闸外 `verify` 仍 2.7 / 45.1 s（16×）**，本探针没有把它算进 gate。
- **快照形状是合成的**：20 篇、每篇一个"一次性插入长文本"op、伪随机 ASCII，
  snapshot/文本 ≈2.0×；真实笔记的 Loro op 历史粒度不同，merge 成本可能明显不同。
  5→20 MiB 的超线性倍率尤其不能直接外推。
- **场景未覆盖**：结构文档（本次 0 个，`structure_integrated=0`）、附件/ASSET、
  真实多文档里的大正文（>1 MB/篇）、Windows/DPAPI、真实磁盘（本机 ext4）、
  低内存/高负载。时间随机器的 fsync 抖动明显（单次 ±20–30% 常见），本节的绝对值
  只用于量级/比值，不要当阈值断言。
- **测量次数**：storage bench 3 轮取中位数；追赶矩阵一次完整运行（三种模式 ×
  4 个场景，478 s）；baseline_apply 两次完整运行（结论一致）。未做多进程/并发同步
  竞争下的测量。
- 常规回归（非 ignored）未改：`crates/sync/tests/engine.rs`、
  `crates/storage/tests/sync_inbox_batch.rs` 仍覆盖批量 ack/冲突/前缀语义；
  本次只新增/扩展 ignored 的 release 基准。

### 8.7 `checkpoint validate` 覆盖日志扫描（S = 1k / 5k / 25k，2026-11-21）

> 任务 4 P1 探针：`crates/sync/tests/phase1a_release.rs::checkpoint_validate_scan_cost`
> （`#[ignore = "manual benchmark"]`）。只加基准与文档：**不改**生产
> `scan_coverage_log`，不改 relay / `sync-proto` / schema，未 commit。

#### 8.7.1 目的与口径

`CheckpointWriter::prepare` 在 coverage 有缺口时会把 pending receipt 落盘，
`validate` 的 `ensure_coverage_proven` 再从持久游标沿 relay 日志**连续扫到 S**
（每页 256 条，`COVERAGE_SCAN_MAX_PAGES = 4096`）。本探针只量这段**真实扫描**随 S 的
墙钟与页数：不复制 scan 实现，而是在测试侧用 `CountingRelay` 包装 FakeRelay，
统计真实 `scan_coverage_log` 发起的 `RelayClient::log` 调用（validate 仍走生产
`CheckpointWriter::validate`）。

| 指标 | 口径 |
| --- | --- |
| `S` | relay 日志事件总数 = checkpoint 的 `covered_seq` |
| `pages` | validate 期间真实 `log()` 调用次数（从持久游标 `c0` 扫到 S，应 = `ceil(S/256)`） |
| `scan_ms` | 第一次 `log()` 开始到最后一次 `log()` 返回的墙钟跨度；含页间逐事件核对（visitor），**不含**最后一页返回之后的 visitor（≤256 条） |
| `log_ms` | 仅 `log()` 内部（FakeRelay 取页 + 克隆事件）的墙钟 |
| `visitor ≈` | `scan_ms - log_ms`：逐事件序号/证据核对开销 |
| `validate_ms` | 整个 `validate` 墙钟（含 manifest/快照读回导入、尾部 ASSET 下载实证、coverage scan） |
| `prepare_*` | prepare 阶段 `try_record_coverage_receipt` 的同口径扫描（单次、含首扫冷启动，不是中位数） |

#### 8.7.2 夹具与测量方式（FakeRelay + 真 SQLite）

- 每个 S 新建 fresh vault（`CARGO_TARGET_TMPDIR` 下真 SQLite）+ `ensure_structure_row`
  + 1 篇小正文（`prepare` 需要可导出的本地快照/head VV，与 coverage 无关）；
  `RELAY_CURSOR_META = "c0"`、`sync.log_id` 与 FakeRelay 一致。
- relay 日志合成 `1..=S` 条：
  - `1..=S-1`：`seq % 7 == 0` 的发布为 ASSET（`publish_event` 的 dummy 事件），
    其余为 UPDATE（`publish_event`）；这一段都在本地 **integrated 连续前缀**内
    （直接写 `sync.integrated_seq = S-1`，合成值）。
  - 第 S 条是 `publish_real_asset` 发出的**真实 ASSET**（1 part、可解密、
    明文 `coverage scan cost tail asset`）。
  - 整体 ASSET 占比 **14.3%**（143 / 715 / 3 572 条），落在 10–20% 要求内。
- 因此 `local_coverage` 把 `1..=S-1` 视为已消费前缀，唯一缺口是第 S 条 ASSET：
  `prepare` 会扫一遍日志并落 pending receipt；`validate` 再从 `c0` 扫一遍、
  逐条核对 receipt 候选，并对尾部 ASSET 真实下载/解密/AEAD 验证。
- 每档同一夹具连续 `validate` 3 次（`prepare` 只做一次；每次 validate 前清零计数器），
  取中位数；一次进程内 3 档串行（1k → 5k → 25k）。
- 硬断言（与时间无关）：`covered_seq == S`、`coverage_verified_candidates == 1`、
  `coverage_durable_assets == 0`、每次 validate 的 `pages == ceil(S/256)`。
  **只打印不 assert 时间。**

环境：Linux 6.8.0-149（Ubuntu），Intel Xeon Gold 6330 @ 2.00 GHz，8 vCPU，
15 GiB RAM，ext4（`/dev/sda1`，空闲 16 G）；`rustc`/`cargo` **1.100.0**，
`--release`（`opt-level=3`、`debug=0`、无 LTO）。测量时机器为共享状态
（其它 agent 的构建/测试同时跑），load average 约 **0.1–2.4**（8 vCPU），
所以绝对值只看量级/线性度（同一探针隔几分钟复跑 4 次，25k 的 `scan_ms` 中位数
3.796 / 3.830 / 3.895 / 3.965 ms，最大偏差 <5%）。

```bash
export LIBRARY_PATH=/home/dev/xkb-sysroot/root/usr/lib/x86_64-linux-gnu
export PKG_CONFIG_PATH=/home/dev/xkb-sysroot/root/usr/lib/x86_64-linux-gnu/pkgconfig

# 只跑本探针（首次无编译 1.18 s；探针本身约 0.6 s）
cargo test --release -p menghunote-sync --test phase1a_release \
  checkpoint_validate_scan_cost -- --ignored --nocapture

# 整个 ignored 文件（另有 baseline_apply 探针；本次 2 passed、测试 66.23 s，
# 含 storage/sync 增量重编译的整条命令 88.44 s）
cargo test --release -p menghunote-sync --test phase1a_release -- --ignored --nocapture
```

> 注意：`cargo test ... -- --ignored` 默认按 test harness 并行执行**两个** ignored 探针，
> `scan_ms` 会被另一探针的 CPU 竞争抬高（实测 1k：0.153 vs 单跑的 0.076 ms；
> 25k：3.878 vs 3.796 ms）。本节表格取**单独运行本探针**的数据。

#### 8.7.3 原始输出（`target/tmp/phase1a-logs/scan-cost-run.log`）

```text
[phase1a] scan_cost setup S=1000 asset_events=143 update_events=857 asset_pct=14.3 prepare_pages=4 prepare_scan_ms=0.114 prepare_log_ms=0.096
[phase1a] scan_cost S=1000 run=0 pages=4 scan_ms=0.083 validate_ms=0.710 log_ms=0.062 part_calls=4 part_ms=0.002
[phase1a] scan_cost S=1000 run=1 pages=4 scan_ms=0.076 validate_ms=0.542 log_ms=0.057 part_calls=4 part_ms=0.001
[phase1a] scan_cost S=1000 run=2 pages=4 scan_ms=0.074 validate_ms=0.514 log_ms=0.055 part_calls=4 part_ms=0.002
[phase1a] scan_cost S=1000 pages=4 scan_ms=0.076 (median of 3; raw_scan_ms=[0.08260899999999999, 0.076162, 0.073966] log_ms_median=0.057 validate_ms_median=0.542 validate_ms_raw=[0.710135, 0.542165, 0.514101] part_calls=4 part_ms_raw=[0.001698, 0.001321, 0.001602])
[phase1a] scan_cost setup S=5000 asset_events=715 update_events=4285 asset_pct=14.3 prepare_pages=20 prepare_scan_ms=0.450 prepare_log_ms=0.342
[phase1a] scan_cost S=5000 run=0 pages=20 scan_ms=0.479 validate_ms=0.919 log_ms=0.344 part_calls=4 part_ms=0.002
[phase1a] scan_cost S=5000 run=1 pages=20 scan_ms=0.440 validate_ms=0.823 log_ms=0.316 part_calls=4 part_ms=0.002
[phase1a] scan_cost S=5000 run=2 pages=20 scan_ms=0.617 validate_ms=1.003 log_ms=0.439 part_calls=4 part_ms=0.002
[phase1a] scan_cost S=5000 pages=20 scan_ms=0.479 (median of 3; raw_scan_ms=[0.478646, 0.440245, 0.617419] log_ms_median=0.344 validate_ms_median=0.919 validate_ms_raw=[0.918995, 0.823261, 1.003386] part_calls=4 part_ms_raw=[0.00223, 0.001847, 0.001587])
[phase1a] scan_cost setup S=25000 asset_events=3572 update_events=21428 asset_pct=14.3 prepare_pages=98 prepare_scan_ms=3.660 prepare_log_ms=3.141
[phase1a] scan_cost S=25000 run=0 pages=98 scan_ms=3.796 validate_ms=4.376 log_ms=3.177 part_calls=4 part_ms=0.003
[phase1a] scan_cost S=25000 run=1 pages=98 scan_ms=3.779 validate_ms=4.356 log_ms=3.183 part_calls=4 part_ms=0.002
[phase1a] scan_cost S=25000 run=2 pages=98 scan_ms=3.829 validate_ms=4.468 log_ms=3.197 part_calls=4 part_ms=0.002
[phase1a] scan_cost S=25000 pages=98 scan_ms=3.796 (median of 3; raw_scan_ms=[3.796107, 3.7794470000000002, 3.829243] log_ms_median=3.183 validate_ms_median=4.376 validate_ms_raw=[4.376248, 4.35638, 4.46814] part_calls=4 part_ms_raw=[0.002683, 0.001617, 0.001777])
```

#### 8.7.4 结果

| S | 事件（ASSET / UPDATE） | pages | `scan_ms` 中位数 | raw `scan_ms` | `log_ms` 中位数 | visitor ≈ | `validate_ms` 中位数 | prepare pages / `prepare_scan_ms` |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 1 000 | 143 / 857 | 4 | **0.076 ms** | 0.074–0.083 | 0.057 ms | 0.019 ms | **0.542 ms** | 4 / 0.114 ms |
| 5 000 | 715 / 4 285 | 20 | **0.479 ms** | 0.440–0.617 | 0.344 ms | 0.135 ms | **0.919 ms** | 20 / 0.450 ms |
| 25 000 | 3 572 / 21 428 | 98 | **3.796 ms** | 3.779–3.829 | 3.183 ms | 0.613 ms | **4.376 ms** | 98 / 3.660 ms |

读法：

1. **逐事件 visitor 是线性的、常数很小**：`scan_ms - log_ms` ≈ 0.019 / 0.135 / 0.613 ms，
   折合 **19–27 ns/事件**（25 000 条 ≈ 0.6 ms）。页数严格 = `ceil(S/256)`
   （4 / 20 / 98）。
2. **本探针的 `scan_ms` 主要来自 FakeRelay 取页，而不是生产 scan 的 visitor**：
   FakeRelay 的 `log()` 每页都从 `Vec` 头 `iter().filter(seq > cursor)` 再 `take(256)`，
   跳过的前缀随页号增长。25k / 98 页合计约 `256 × (0+1+…+97)` ≈ **1.22M 次跳过**，
   这就是 `log_ms` 3.18 ms 的主要来源（1k/5k 分别只有约 1.5k/49k 次）。
   真实 relay 用游标/索引取页，理想成本 O(page)，**本表的 `scan_ms` 是上界**，
   不能按它外推真实 relay 的扫描成本。
3. **生产口径下真正要紧的是页数 × 网络 RTT**：S = 25 000 时是 **98 次串行页请求**
   （每页 256 条）；visitor 本身只有约 0.6 ms。FakeRelay 无网络，所以本表只能说明
   “CPU/扫描逻辑不是瓶颈”，不能给出真实端到端延迟。
4. **prepare 也会扫一遍同一区间**（写 pending receipt）：25k 时 `prepare_scan_ms` 3.66 ms
   （单次），即一次 “prepare → validate” 最多访问 **2 × S** 条事件；
   若本地账簿已完整覆盖（`missing` 为空），`ensure_coverage_proven` 直接返回、
   不扫描（本探针未测这一档，按代码路径）。

#### 8.7.5 限制（不外推）

- **FakeRelay 不是真实 relay**：纯内存、无 TLS/网络/RTT/重试/真实 relay GC；
  `log()` 每页从 `Vec` 头线性 filter（见读法 2），使 `scan_ms` 含
  ≈`O(S²/256)` 的夹具 artifact。真实 relay 的游标分页成本未测。
- **合成日志**：85.7% 的 UPDATE 是不含真实负载的 `publish_event`；integrated 前缀是
  直接写 meta 的合成值（不是 engine 真实 pull/integrate 推进）；前缀内的 ASSET
  事件不下载验证——生产里它们应由 engine 的 durable skipped-asset 记录证明，
  本探针只借用生产代码信任的“连续前缀 = 已消费”不变量。只有尾部第 S 条 ASSET
  是真实密文并走完整 receipt + 下载/解密/AEAD 验证。
- **夹具是单文档小正文**：manifest/snapshot 固定成本（`validate_ms - scan_ms` ≈ 0.44–0.58 ms）
  不随 S 缩放；真实 checkpoint 的快照导入/资产实证成本本探针只测了 1 条尾部 ASSET。
- **没有覆盖的形状**：缺口在持久游标之前（durable skipped-asset 路径）、多个缺口、
  ASSET 占 10–20% 之外、S < 1k / > 1M（`COVERAGE_SCAN_MAX_PAGES = 4096` 上限）、
  relay GC 后日志不可读、真实网络抖动；均未测。
- **页大小耦合**：256 是生产私有常量，探针里用 `ceil(S/256)` 校验页数；生产改页大小
  时探针与本节需同步。
- **时间噪声**：共享 8 vCPU 机器、load 0.1–2.4；5k 的第 3 轮 0.617 ms（比中位数高 29%），
  25k 的 prepare 曾出现一次 8.8 ms 冷启动离群值。绝对值只用于量级/线性度判断。
- 探针**未 commit**；写作用域仅 `crates/sync/tests/phase1a_release.rs` 与本文件。

`````



@@SECTION docs/07 `docs/adr/0001-storage-format.md`


`````markdown
# ADR 0001 — 存储与同步架构：SQLite 单库真源 + 分区 CRDT + 密文中继

- **状态**：已采纳（2026-11-08，`bluetrum/gpt-6.1-sol`，`reasoning_effort: max`）
- **输入**：[`0003-storage-sync-consolidated.md`](../briefs/0003-storage-sync-consolidated.md)（取代 `0001-storage-format.md`）
- **影响范围**：`crates/storage`、`crates/sync`、`crates/core`、MCP、导出、备份
- **阻塞性**：本文第 8 节第 2 步通过前，禁止并行业务写入实现

---

## 1. 背景

menghunote 是基于 GPUI 的桌面笔记软件（Windows 首要平台，单人 + AI 协作），
编辑器对标 **Typora 式原位所见即所得**：编辑器内的文本就是 Markdown 源码，`#`、`**` 等标记
真实存在于文本中，渲染层只是不绘制（见 [`0002-editor-core.md`](../briefs/0002-editor-core.md)）。

两条产品级前置决策（用户 2026-11-08 拍定）：

1. **不支持外部编辑器编辑 `.md`** → 导出目录是单向产物；因此磁盘真源可以是单一数据库，
   文件监听 / 回声抑制 / 内容哈希比对 / 文件级三级冲突整块消失。
2. **同步走服务端加密（E2E），本地明文** → 中继只见密文；本地安全依赖 OS 全盘加密（BitLocker）。

因此本 ADR 要解决的是：**在一个 SQLite 库里，如何同时承载"字符级 Markdown CRDT"、
"笔记导航树"、"可重建的检索投影"与"逐设备密文同步"，并且崩溃、并发、换代都不丢数据。**

---

## 2. 决策摘要

加粗项为**不可逆**决策（一旦写入用户数据，改动需要迁移或换代）。

1. **一个结构 `LoroDoc`（`LoroTree` + 元数据 Map）+ 每篇笔记一个正文 `LoroDoc`（`LoroText`）**。
2. **结构树只是笔记导航树**，不是 Markdown 块树；**块永远是派生**，不落库。
3. 正文 CRDT 只存原始 Markdown（LF 归一），**不用 Rich Text，不做正文 CRDT 分片**。
4. **编辑即刻映射**（delete(range)+insert），**不做**周期性整篇 diff；提交用"首次 dirty 起固定 250ms 窗口"，
   **不用**会被连续输入无限延期的尾缘 debounce。
5. **两阶段物化**：A 事务持久化 CRDT op 与 outbox；B 事务后台从固定 frontier 物化 `body_md` 与投影。
   **禁止**每 250ms 同事务重写整篇 `body_md`。
6. **同步因果历史与产品版本回看分离**：v1 保留当前代完整因果历史；版本回看用有配额的本机压缩检查点。
7. **relay 是持久密文中继**（密文 update + 完整密文 snapshot + 密文资产 + 加密 manifest），
   **不是纯转发**，也不运行 Loro 或搜索。
8. **附件进 SQLite BLOB**（SHA-256 主键去重），**不放外部 `blobs/` 目录**。
9. **随机 256-bit vault 根密钥 + Argon2id 口令封装 + DPAPI 本地封装**；
   **口令不直接作数据密钥**。
10. 删除语义：**grow-only 删除事件 + 恢复确认事件**，**不用** `LoroTree::delete`，也**不用** SQL `DELETE`。

---

## 3. 详细决策

### 3.1 文档划分与"同一事实只有一处权威"

| 事实 | 权威 | SQL 投影（可重建） |
| --- | --- | --- |
| 正文 | 正文 `LoroDoc` 的 `LoroText`（原始 Markdown） | `notes.body_md` |
| 标题 / 树 parent 与顺序 / 标签成员 / 删除状态 / `created_at` / 收藏 / 置顶 | 结构 `LoroDoc`：`LoroTree` 节点 `get_meta()` 的 `LoroMap` + 全局 Map（标签字典） | `notes`、`tree`、`tags`、`note_tags` |
| `updated_at` | **SQL 派生值**：取正文 Doc 与结构元数据中**显式存储的客户端编辑戳**的 `max` | `notes.updated_at` |
| 导出根目录与策略 | **SQL 本机配置**（不同设备可不同） | — |
| 实际导出文件名/路径 | 导出期派生：树 + 标题 + 稳定 `note_id` | — |
| 光标、展开态、本机同步游标、密钥槽 | **SQL 本地事实**（不可从 CRDT 重建） | — |
| 资产字节 | `assets.data` BLOB | — |
| 资产引用 | Markdown 内 `asset://<SHA-256>` 才是引用事实 | `asset_refs` |

要点：

- **结构 `LoroTree` 仅承载笔记导航树**；节点 `meta` 保存标题、`note_id → body_doc_id`、
  创建时间、标签成员、收藏/置顶、删除/恢复事件。
- **正文编辑戳不写结构 Doc**——否则每次正文提交都要触碰结构文档，制造无谓的串行化。
- **时间戳不参与 CRDT 冲突排序**，`updated_at` 也不用落库时间或接收时间。
- **正文首个 H1 与笔记标题是两个独立事实**，不做双向自动绑定。
- `SQL` 里的 `notes/tree/tags/note_tags/body_md/links/chunks/asset_refs` **只由投影器写入**，
  是纯投影；权威在 CRDT 或本地配置。

### 3.2 身份与代际

- `note_id` **永久稳定**（改名、移动、导出都不变）。
- 每台设备的 `device_id` 与 Loro `peer_id` 必须唯一；**克隆/恢复旧备份作为新实例时必须生成新 `peer_id`**，
  绝不能复制活动 peer 身份。
- 协议预留 **doc generation**：将来截断历史必须"新物理 Doc / 新代"，
  **不能把旧代 update 直接导入新代**。
- `key_epoch` 与 doc generation 是两个维度，不得混用。

### 3.3 删除与恢复

- **不用** `LoroTree::delete`，**不用** SQL `DELETE`。
- 记录 **grow-only 的删除事件 ID** 与**观察到的恢复确认 ID**：
  有效删除 = 存在未被恢复确认的删除事件。
- 恢复只确认"它已见过的"删除；**并发的新删除仍然获胜**。
- 并发正文编辑仍持久化到回收站，**不使笔记复活**。

### 3.4 编辑与持久化协议

**坐标与映射**

- 源码统一 **LF**。三套坐标各司其职、**不得混用**：
  **Rope 源码定位用 UTF-8 byte**；**编辑核 ↔ CRDT 桥的逻辑字符坐标用 Unicode scalar**；
  **IME 另接 UTF-16**，视图用 grapheme。
- ⚠️ **不得**仅凭"`LoroText` 是字符级"就断定 Loro 1.17.1 某个 Rust API 的默认索引单位。
  适配层必须用**中文 / emoji / 组合字符**的固定夹具验证其**实际 API 单位**，
  若与 Unicode scalar 不同，则在适配边界显式转换。
  这条是 v1 的**兼容性验收项**，不是注释——单位判断错 = 中文与 emoji 光标全体错位。
- 每次真实编辑按**当前基准版本**执行 `delete(range) + insert(text)`；
  本地与远端对同一文档的修改**串行化**；**过期偏移不得直接应用**。
- 远端 CRDT delta **增量驱动** Rope；**IME preedit 不写 CRDT**，composition commit 一次提交；
  合并时保留 CRDT 相对光标锚点。

**提交窗口**

- 首次 dirty 起**固定 250ms 窗口**（不是尾缘 debounce）；64 KiB 待提交、`Ctrl+S`、切换/关闭立即冲刷。
- UI **不同步等待** SQLite；"已保存"只在 WAL durable commit 之后显示。

**两阶段物化（不可逆）**

| 阶段 | 内容 |
| --- | --- |
| **A 事务** | 追加 `loro_updates`；更新持久 head frontier / VV / 本机 seq；写 `projection_outbox`、`sync_outbox`。结构类小投影（标题、墓碑等）及其 `index_outbox` 可随结构 op 同事务，确保已删除笔记立即从查询过滤 |
| **B 事务** | 后台从**已持久化的固定 frontier** 物化 `body_md`，原子更新 `body_frontier`/`body_seq`、`chunks`/`links`/`asset_refs`、`index_outbox`。重物化与解析在事务外完成；**旧任务只能清除自己版本的 outbox，不能覆盖新任务** |

节奏：正文 ≤1 MiB 每约 1s 合并物化，较大篇约 2s；**持续输入也不能无限推迟**（数值可调）。

**恢复与校验**

- 启动：snapshot + 后续 update 重放；`body_frontier ≠ head` 就重算。
- **首次打开笔记额外直接比较 CRDT 文本与 `body_md`**——`frontier` 相同**不是**文本正确的证明；
  热路径不做 Blake3 比对。
- A 已提交而 B 未完成：只丢缓存进度。
- **未 ACK 写契约（不可含糊，实现前必须对齐）**：内存中只有**一份可追加的未 ACK 操作日志**，
  **不是**"一个在途批 + 一个新的待提交批"。writer 每次取**连续前缀**提交；durable 后按 ACK **裁剪前缀**；
  过期 flush 请求**合并**；慢 I/O 时用**背压**（暂停接受新写 / 拒绝新的提交），
  而不是假装还能无阻塞吸收输入。
  **持久事务批号与未 ACK 内存日志不是同一概念**：前者可重放，后者丢了就是真丢。
  因此"最多丢一个未 ACK 批"**只在"批"= 连续前缀时成立**；若把"批"理解成固定 250 ms 封口的不可变批次，
  慢 I/O 下就只能拒绝/暂停新写——两者不可能同时成立。本文取**前者**（可追加日志 + 连续前缀 + ACK 裁剪 + 背压）。
  **不承诺**硬件断电下的绝对 250 ms。
- CRDT 权威解码失败 → 恢复模式；**禁止 `body_md` 反向覆盖 CRDT**。
- 索引：commit 后才确认 outbox；重复处理按 `note_id` 删除后重建；可重放。

### 3.5 历史策略：因果历史 ≠ 版本回看

- **v1 保留当前代完整因果历史**，以支持无限天数的未吊销离线设备。
- **普通完整 snapshot 只合并/回收重复 update 行**，**不等于**删除历史或墓碑。
- **产品版本回看**另存压缩 Markdown 检查点：5 分钟一份，AI 调用 / 大替换 / 删除前补份；
  保留 30 天、每篇最多 100 份或 32 MiB 压缩数据、全 vault 最多 1 GiB，先到限先淘汰最旧；
  **v1 仅本机归档，不承诺跨设备复制**。
- 恢复某个版本 = 产生**新的区间 CRDT 编辑**，**不回退 VV**。
- 配额：因果数据默认 **4 GiB/vault**；64 MiB/Doc 或 1 GiB/vault 预警；
  达到配额暂停新增写入/同步并允许调高，**不暗删未同步数据**。
- 真正截断可后补，但必须先满足：全部未退役设备停写、上传、确认**换代屏障**；
  未确认的长期离线设备必须由用户**明确退役**；迟到旧代编辑进入人工差异恢复。
- **不分片正文 CRDT**，只分 Rope / 解析 / 布局。v1 正文 ≤10 MiB、单行 ≤1 MiB 字节可编辑，
  超出转只读 / 源码降级，**不截断**。
  性能门槛：1 MiB 笔记 + 10 万字符单行 + 中文 emoji IME，Windows 输入到视图 **p95 ≤16 ms**
  ——这是**目标，尚未实测**。

### 3.6 同步与中继

- **持久密文中继**（不是纯转发、不是服务端合并）：HTTPS 拉取，WebSocket 仅作唤醒。
  relay 存：鉴权设备公钥与吊销状态、密文 updates、完整密文 snapshots、密文资产、
  加密 manifest / 密钥封装槽、不透明游标。**不运行 Loro、不做搜索、不见明文。**
- 更新单位 = 每个 Doc **已落库批次**的增量 update；多字符 op 合并；
  网络约 1s 或 256 KiB 批量；单传输片 ≤1 MiB；资产 4 MiB 片。
  **这是传输切片，不是 CRDT 分片。**
- VV / base / target frontier 在 **E2E envelope 内**交换；`relay_seq` 只表示送达顺序，
  **不能当因果版本**。
- 断线：按游标补日志 + 按 VV 求缺失；若依赖缺失或日志已回收，
  取**全因果 snapshot** 与本地待同步更新合并，**绝不覆盖本地离线状态**；导入幂等；durable 后才 ACK。
- 跨 Doc 用 `batch_id` + 缺失依赖占位；**不声称**跨文档分布式原子合并。
- **新设备 bootstrap 无需任何对端在线**：先经认证获得 vault key，
  再从 relay 加密 manifest 取结构 + 正文完整快照、所需资产与统一水位 S，最后补 S 之后的日志；
  完成后得到当前代全量因果历史 + 当前状态（**不含**已被淘汰的本机版本归档）。
- **relay GC（v1 即做）**：客户端累计 16 MiB 增量或每 24h 产生**可独立导入**的完整 checkpoint；
  必须先拉齐所声明水位 S、重新解密导入验证覆盖，再发布签名 manifest；
  保留最近两份有效 checkpoint，只删较老那份覆盖的日志，并保留 **7 天宽限**；
  checkpoint **不截断因果历史**，因此**不需要按每台离线设备永久留日志，也不能按 `last_seen` 静默删设备**；
  vault 数据保留至用户显式删除或配额处置；达配额拒绝新增、保留已有数据，并为 checkpoint 替换预留空间。
- **E2E 不防恶意 relay 删数据**：客户端记录已确认水位以检测回退；
  新设备在无可信旧水位时**无法独立识别"有效签名的旧快照回滚"** → 已写入第 6 节已知边界。
- **UI**：非阻断并发合并提示。VV 并发编辑时提示来源并展示增量；删除 + 编辑并发提示
  "回收站有新内容"；语义误合并由有界版本回看恢复；**不建立文件三级冲突机制**。

### 3.7 密钥与 E2E

- **随机 256-bit vault 根密钥**；`Argon2id`（64 MiB、t=3、p=1、16-byte 随机 salt；参数随槽存、可向上校准）
  **只派生 KEK 用于封装根密钥**；改口令仅重包，不重加密历史。
- ⚠️ **口令只在客户端进入 Argon2id，永远不得发送给 relay**；尤其**不得**复用为会明文提交给服务端的
  "登录口令"。relay 的公钥挑战 / 短期设备 token / 独立管理凭证属于**另一个控制面**。
  否则服务端一旦拿到登录口令，配上公开 salt 与口令封装槽即可解开根密钥，**E2E 名存实亡**。
  新设备从 relay 取回密钥槽后，也**必须本地解封**，不得把口令或根密钥交给服务端代解。
- ⚠️ **"改口令" ≠ "撤销已泄露的解密能力"**。日常改口令只重包随机根密钥，成本低；
  但一旦怀疑**口令或旧封装槽已泄露**，仅重包无效——攻击者可留着旧槽解出同一个根密钥。
  此时必须走**设备吊销流程：轮换随机 `key_epoch` 并发布新加密基线**，
  旧槽只在迁移窗口内保留给仍授权的设备。
  产品文案**不得**宣传"改口令即可收回攻击者已拿到的解密能力"。
- Windows 默认用 **DPAPI CurrentUser 包裹本机根密钥**，不要求每次口令解锁；
  **不改变本地明文选择**。
- 算法：XChaCha20-Poly1305、随机 24-byte nonce、HKDF-SHA256 分用途子密钥；
  AAD 绑定 vault / object / type / generation / key_epoch / msg / 片号 / 协议。
- VV 与真实资产 hash 留在密文内；relay 侧资产名用 **HMAC 派生的不透明 ID**，避免明文 hash 的已知图片探测。
- 配对：已授权设备扫描**高熵一次性邀请**并核对公钥指纹，成熟 HPKE 封装给新设备公钥；
  **不自造 6 位码当密钥**。新设备也可凭口令槽 / 独立恢复码解封后从 relay bootstrap。
- relay 设备鉴权：公钥证明 + 短期 token；新增/吊销需**独立账户管理权**，
  **不能仅凭知道 vault key**。吊销立即拒绝旧设备认证并轮换随机 `key_epoch`，
  为当前 bootstrap checkpoint 与所需资产重新加密发布到新 epoch，
  旧解密槽仅供仍授权设备迁移，旧密文按 GC 收尾。
  **已被旧设备得到的明文与旧历史不可撤回。**
- **纠正"忘口令必丢"**：有 DPAPI 授权设备或恢复码即可重包新口令；
  只有 relay 密文 + 口令 / 恢复码 / 授权设备全失才永久不可恢复。
  本地明文 DB 备份仍可恢复笔记并建立新 vault。创建时强制确认"无服务器后门"并校验恢复码已备份。

### 3.8 附件

- **SQLite BLOB + SHA-256 主键去重**；不选外部 `blobs/<hash>`：
  外部 CAS 会让真源变成"DB + 目录"，跨资源提交与备份都要额外协议；
  BLOB 是单事务、一次 SQLite 在线备份即恢复全部。
- 代价（接受）：DB 与 WAL 变大；导出必须复制为 `assets/` 目录。
- v1 单附件 ≤64 MiB；不把大视频放主路径。
- 本地新增资产字节与引用它的正文 op **同 A 事务**；
  远端可先有 missing 资产占位（`assets.data IS NULL`），下载并校验 hash 后再填字节，
  **不假装已离线可用**。
- 导出固定已提交 frontier（不盲读旧 `body_md`）；`asset://hash` 与 `note://uuid`
  只在单向导出时重写为相对路径，并随导出 manifest 保存对应关系；导入是显式导入，不回写、不监听。
- 孤儿 GC：活笔记 / 回收站 / 保留版本 / 保留 bootstrap checkpoint / 待同步任务均无引用，
  且满 30 天、所有未吊销设备确认稳定 GC 水位，才可删；否则**只标记不删**。
  从未引用或发布的暂存资产 24h 可删。
- 备份：**不需要** `blobs/` 目录；必须用 SQLite backup API / 一致备份，
  **不能直接拷贝运行中的主文件**（会丢 WAL）。Tantivy 不需备份，可重建。

### 3.9 迁移、兼容与平台约束

- 每 vault：Windows 命名 mutex + **单 writer actor**；`journal_mode=WAL`、`synchronous=FULL`、
  `busy_timeout=5000`。
- 库存 `%LOCALAPPDATA%`；**禁止** OneDrive / SMB / 网络盘；备份与导出可放别处。
- `user_version` 逐级事务迁移；变更前做 SQLite 一致备份；新 schema 下旧程序**只读拒写**；
  **不是**每次启动自动重建权威。
- 每个 Loro snapshot/update 携带实际 codec 标识、`app_doc_schema`、`min_reader`；
  升级用固定 1.17.1 测试夹具证明；**不把 crate semver 当编码兼容承诺**；
  双读旧写，所有未退役设备兼容后才切新写。
- 检索：**Tantivy 0.27 + cang-jie 0.22 `ForSearch`** 做中文 BM25 主路径；
  schema / tokenizer / 词典变更 → **新索引代**从 SQL 全量重建并原子切换；
  `chunks` 是可删派生区域，无持久块树；向量检索后补。

---

## 4. 被否方案

| 方案 | 否决理由 |
| --- | --- |
| vault 单体 `LoroDoc` | 正文无法按需加载 / 独立同步与恢复；结构会承受全部正文历史 |
| 正文用 Loro Rich Text | 与 Typora 模型冲突：`**` 根本不存在于数据里 |
| 持久化块树 | 真源分裂 + 往返保真成本；且不是 Typora 编辑模型 |
| 周期性整篇 diff 同步到 CRDT | 写放大，与 1 MiB 正文目标冲突 |
| 尾缘 debounce 提交 | 连续输入时可无限延期，永远不落盘 |
| 每 250ms 同事务重写 `body_md` | 大篇持续全文写放大 |
| 用完整 snapshot 当"历史压缩/截断" | 只合并重复日志，不截断因果历史 |
| `LoroTree::delete` / SQL `DELETE` 当删除真源 | 无法表达"删除 vs 恢复"的并发语义 |
| 6 位配对码当密钥 | 弱密钥，可暴力 |
| 口令直接派生数据密钥 | 改口令要重加密全库 |
| 附件走外部 `blobs/<hash>` CAS | 真源变 DB+目录；跨资源提交与备份失去原子性 |
| relay 纯转发 | 新设备 bootstrap 依赖在线对端；离线设备日志无处存放 |
| 直接拷贝运行中的 sqlite 主文件备份 | 丢 WAL，备份不一致 |
| 文件监听 / 回声抑制 / 文件级三级冲突 | 场景已不存在（不做外部编辑），整块删除 |

---

## 5. 后果

**正面**：单一事实链；投影全部可重建；崩溃只丢未 ACK 的内存批次；
支持无限天数离线设备；可自建纯密文中继，服务器不可信也不泄内容。

**代价（接受）**：
- 库与备份体积随附件增长（BLOB 入了库）；
- 正文与结构两套 frontier / outbox，状态机复杂度上升；
- 版本回看 v1 仅本机；
- 配额与"换代屏障"是产品级运维负担；
- relay 存储成本与 GC 正确性要求高。

---

## 6. 已知边界（安全，必须写进 ADR）

- SQLite / WAL / SHM、Tantivy 索引、临时文件、导出目录、本地备份**均可能明文**。
- **DPAPI 只护钥不护笔记**；同一 Windows 用户下的恶意进程 / 内存读取不可抵御；
  安全依赖 BitLocker / OS。
- relay 可见对象数量、大小、时序、IP。
- E2E **不防**恶意 relay 删数据；新设备无可信旧水位时无法独立识别"有效签名的旧快照回滚"。
- 已被吊销设备**已经获得**的明文与历史不可撤回。

---

## 7. 附录：DDL 草图（SQLite STRICT 表，要求 ≥3.37；非实际建库代码）

```sql
PRAGMA journal_mode=WAL;
PRAGMA synchronous=FULL;
PRAGMA foreign_keys=ON;
PRAGMA busy_timeout=5000;

CREATE TABLE meta(key TEXT PRIMARY KEY, value BLOB NOT NULL) STRICT;

CREATE TABLE loro_docs(
  doc_id TEXT PRIMARY KEY, kind TEXT NOT NULL CHECK(kind IN('structure','body')),
  generation INTEGER NOT NULL DEFAULT 1, app_doc_schema INTEGER NOT NULL,
  codec_version TEXT NOT NULL, min_reader INTEGER NOT NULL,
  snapshot BLOB, snapshot_seq INTEGER NOT NULL DEFAULT 0, snapshot_frontier BLOB,
  head_seq INTEGER NOT NULL DEFAULT 0, head_frontier BLOB, head_vv BLOB,
  CHECK(head_seq>=snapshot_seq)
) STRICT;

CREATE TABLE loro_updates(
  doc_id TEXT NOT NULL REFERENCES loro_docs(doc_id), seq INTEGER NOT NULL,
  update_id TEXT NOT NULL UNIQUE, batch_id TEXT NOT NULL,
  codec_version TEXT NOT NULL, payload BLOB NOT NULL,
  end_frontier BLOB NOT NULL, end_vv BLOB NOT NULL,
  PRIMARY KEY(doc_id,seq)
) STRICT;

CREATE TABLE notes(
  note_id TEXT PRIMARY KEY,
  body_doc_id TEXT NOT NULL UNIQUE REFERENCES loro_docs(doc_id),
  title TEXT NOT NULL, body_md TEXT,
  body_seq INTEGER NOT NULL DEFAULT 0, body_frontier BLOB,
  meta_seq INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL,
  deleted INTEGER NOT NULL CHECK(deleted IN(0,1)),
  is_favorite INTEGER NOT NULL CHECK(is_favorite IN(0,1)),
  is_pinned INTEGER NOT NULL CHECK(is_pinned IN(0,1))
) STRICT;
CREATE INDEX notes_recent ON notes(updated_at DESC,note_id) WHERE deleted=0;
CREATE INDEX notes_flags ON notes(is_favorite,is_pinned,updated_at DESC) WHERE deleted=0;

CREATE TABLE tree(
  node_id TEXT PRIMARY KEY, parent_id TEXT REFERENCES tree(node_id) DEFERRABLE INITIALLY DEFERRED,
  note_id TEXT UNIQUE REFERENCES notes(note_id), title TEXT NOT NULL,
  sibling_rank INTEGER NOT NULL, deleted INTEGER NOT NULL CHECK(deleted IN(0,1)),
  meta_seq INTEGER NOT NULL
) STRICT;
CREATE INDEX tree_parent_order ON tree(parent_id,sibling_rank,node_id);

CREATE TABLE tags(tag_id TEXT PRIMARY KEY,name TEXT NOT NULL,normalized_name TEXT NOT NULL) STRICT;
CREATE INDEX tags_name ON tags(normalized_name);

CREATE TABLE note_tags(
  note_id TEXT NOT NULL REFERENCES notes(note_id),tag_id TEXT NOT NULL REFERENCES tags(tag_id),
  PRIMARY KEY(note_id,tag_id)
) STRICT;
CREATE INDEX note_tags_reverse ON note_tags(tag_id,note_id);

CREATE TABLE chunks(
  chunk_id INTEGER PRIMARY KEY,note_id TEXT NOT NULL REFERENCES notes(note_id),ordinal INTEGER NOT NULL,
  start_byte INTEGER NOT NULL,end_byte INTEGER NOT NULL,body_seq INTEGER NOT NULL,
  parse_version INTEGER NOT NULL,plain_text TEXT NOT NULL,
  UNIQUE(note_id,ordinal),CHECK(end_byte>=start_byte)
) STRICT;
CREATE INDEX chunks_note_seq ON chunks(note_id,body_seq);

CREATE TABLE links(
  from_note_id TEXT NOT NULL REFERENCES notes(note_id),start_byte INTEGER NOT NULL,
  end_byte INTEGER NOT NULL,to_note_id TEXT,target TEXT NOT NULL,kind TEXT NOT NULL,body_seq INTEGER NOT NULL,
  PRIMARY KEY(from_note_id,start_byte,target)
) STRICT;
CREATE INDEX links_backlinks ON links(to_note_id,from_note_id);

CREATE TABLE assets(
  asset_hash BLOB PRIMARY KEY CHECK(length(asset_hash)=32),byte_len INTEGER,media_type TEXT,data BLOB,
  orphan_since INTEGER,CHECK(data IS NULL OR (byte_len IS NOT NULL AND length(data)=byte_len))
) STRICT;

CREATE TABLE asset_refs(
  note_id TEXT NOT NULL REFERENCES notes(note_id),asset_hash BLOB NOT NULL REFERENCES assets(asset_hash),
  start_byte INTEGER NOT NULL,body_seq INTEGER NOT NULL,
  PRIMARY KEY(note_id,asset_hash,start_byte)
) STRICT;
CREATE INDEX asset_refs_reverse ON asset_refs(asset_hash,note_id);

CREATE TABLE versions(
  version_id INTEGER PRIMARY KEY,note_id TEXT NOT NULL REFERENCES notes(note_id),
  captured_at INTEGER NOT NULL,generation INTEGER NOT NULL,frontier BLOB NOT NULL,
  title TEXT NOT NULL,body_zstd BLOB NOT NULL,asset_hashes BLOB NOT NULL
) STRICT;
CREATE INDEX versions_retention ON versions(note_id,captured_at DESC);

CREATE TABLE projection_outbox(
  doc_id TEXT PRIMARY KEY REFERENCES loro_docs(doc_id),target_seq INTEGER NOT NULL
) STRICT;

CREATE TABLE index_outbox(
  note_id TEXT PRIMARY KEY REFERENCES notes(note_id),target_body_seq INTEGER NOT NULL,
  target_meta_seq INTEGER NOT NULL,index_version INTEGER NOT NULL,
  action TEXT NOT NULL CHECK(action IN('upsert','delete'))
) STRICT;

CREATE TABLE sync_outbox(
  message_id TEXT PRIMARY KEY,batch_id TEXT NOT NULL,doc_id TEXT NOT NULL,seq INTEGER NOT NULL,
  key_epoch INTEGER NOT NULL,ciphertext BLOB,
  FOREIGN KEY(doc_id,seq) REFERENCES loro_updates(doc_id,seq)
) STRICT;
CREATE INDEX sync_batches ON sync_outbox(batch_id);

-- 完整迁移事务最后设置 PRAGMA user_version=1。
```

DDL 说明（实现时勿丢）：

- `notes/tree/tags/note_tags/chunks/links/asset_refs` 是投影，**仅投影器写**。
- `tree.sibling_rank` 是从 `LoroTree` 遍历生成的 **dense rank**，**别猜** native fractional-index 编码
  在 SQLite 里可直接字典排序。
- `tags.normalized_name` **不设 UNIQUE**，避免并发创建同名标签导致 SQL 投影失败。
- `assets.data IS NULL` 表示**未下载**，不是空内容。
- outbox 任务可合并，但须**按版本 CAS 确认**；`sync_outbox` 指向的 update 不得被 checkpoint GC 删除。
- `versions.asset_hashes` 必须参与 **GC 根扫描**。
- 正式 schema 可加状态 / 错误列，**不改变权威归属**。

---

## 8. 落地顺序（8 步）

| 步 | 内容 | 验收 |
| --- | --- | --- |
| 1 | ADR + 契约：ID、权威归属、编辑坐标、两阶段 frontier、envelope/`min_reader`、删除恢复语义 | 完整数据流**无双权威** |
| 2 | 存储最小链路：单文档编辑 / 两副本并发 / 快照重放 / 两事务故障注入 | 崩溃后只丢未 ACK 内存批；head/body 不同必重建；peer 复制被阻止。**此步前禁止并行业务写入实现** |
| 3 | 编辑器垂直切片：真实标记、局部 op、增量解析、IME、光标锚点 | Windows Unicode / 1 MiB / 长行性能门槛通过 |
| 4 | 领域与资产：树移动、标签、删除恢复、收藏置顶、BLOB、单向导出、SQLite 备份恢复 | 两副本"删除 × 编辑"、"移动 × 移动"并发不丢内容；备份复原含图片 |
| 5 | 派生检索：`chunks`/`links`/outbox、Tantivy + cang-jie | 中英混搜；重复消费与索引损坏可全重建；已删笔记不返回 |
| 6 | 密钥与配对：DPAPI、口令/恢复槽、认证 envelope、轮换 | 改口令不用重加密历史；篡改被拒；换 Windows 可凭恢复材料恢复 |
| 7 | 自建 relay：持久日志、两 checkpoint GC、manifest bootstrap、VV 补偿、设备吊销 | 所有其他客户端关闭时新设备仍能完整恢复；长离线编辑合并；被吊销设备拿不到新 epoch；配额不暗删 |
| 8 | Windows 集成：两设备端到端、断网/断电故障矩阵、IME 大笔记、备份恢复、打包 | `fmt`/`clippy`/`test` + 实际 Windows 验收，由 Lead 亲验 |

并行门槛：**第 2 步通过后**，3 / 4 / 6 可在冻结接口与不重叠写域下并行；5 等 3、4；7 等 6；8 集成串行。
只读 `rmcp`、向量检索等**不是**核心验收阻塞项。

---

## 9. 一句话

**先完成"单篇笔记的区间编辑 → CRDT → durable op → 崩溃重放 → 两副本合并 → 带 frontier 物化"的契约验收；
不先堆 UI，不先写 relay。**

`````



@@SECTION docs/08 `docs/adr/0003-module-boundaries.md`


`````markdown
# ADR 0003 — 模块边界与依赖方向（`crates/text` / `storage::session` / `crates/editor`）

- **状态**：已采纳（2026-11-09，`bluetrum/gpt-6.1-sol`，`reasoning_effort: max`）
- **前置**：[ADR 0001](0001-storage-format.md)、[ADR 0002](0002-editor-core.md) 的磁盘/CRDT 协议**不变**；
  本 ADR 只定"代码放哪、谁依赖谁"，不借机改协议。
- **影响范围**：`crates/text`（新增）、`crates/storage`、`crates/editor`、`crates/app`、根 `Cargo.toml`

---

## 1. 背景与问题

实现推进到 S0 末尾时暴露出两个结构问题：

1. **单位/坐标有两份实现**：`crates/storage/src/units.rs`（174 行）与 `crates/editor/src/units.rs`（213 行）
   都在做 UTF-8 byte ↔ Unicode scalar ↔ UTF-16 换算，语义重叠、错误类型不同 → 必然漂移。
2. **文档会话 bridge 无归属**：`EditorDocumentPort`（`OpenSnapshot` / `EditRequest` / `Applied` / `StaleBase` /
   `DurableAck` / `Flush` …）要持有 live `LoroDoc`、未 ACK journal、A/B 两阶段状态；放错位置会导致
   "把会话塞进 GPUI 层（不可测）"或"形成 `storage → editor` 的环"。

约束：`crates/editor` 必须**不依赖 GPUI**（无窗口可测）；`crates/storage` 已有单写者 actor + loro；
单人开发，不为"整洁"引入无收益的 crate。

---

## 2. 决策

**选 B + ①**：

- **B**：新建**薄的 `crates/text`**，只收拢坐标与单位原语；
- **①**：bridge / 具体 `EditorDocumentPort` 与协议类型放 **`storage::session`**；
- **`crates/editor` 允许静态依赖 `crates/storage`**（"无窗口可测" ≠ "不得依赖 SQLite/Loro"）。

为什么不是其他组合：

| 被否方案 | 否决理由 |
| --- | --- |
| A：`storage` 依赖 `editor` 持有单位 | 具体端口又要 `editor → storage`，两者成环 |
| ②：bridge 放 `editor` | 等于把 journal/durable 协调搬进 editor，无必要的依赖倒置 |
| ③：新建 `crates/doc` | 没有真实收益，只是多一层 |
| C：保留两份 units，靠测试对齐 | 两份实现必然漂移，测试对齐是事后补救 |
| D：`storage` 拥有通用单位 | 把通用 Unicode 契约绑死在数据库包里 |

---

## 3. 依赖 DAG（不可有环，含 dev-dependency）

```text
app  →  editor / storage / text / gpui
editor →  storage / text
storage →  text / loro / rusqlite
text →  unicode-segmentation（及错误实现所需的轻依赖）
```

硬约束：

- **禁止任何循环**，包括用 dev-dependency 绕行；
- `storage` **不依赖** `editor`；
- `editor` / `storage` / `text` **不依赖** GPUI 或 `app`（`editor` 也不许加 GPUI dev-dependency）；
- 不另建 `ui` / `doc` crate。

---

## 4. 类型与职责归属

### `crates/text`（唯一来源）

- `SourceByte` / `UnicodeScalar` / `InputUtf16` / `DisplayByte`（四个 newtype）；
- `SourceRange`；
- **共同的 `CoordError`**（以及保留时的 `TextUnit`）；
- byte / scalar / UTF-16 的严格转换与校验、grapheme 原语。

**不在 text**：`DisplayByte ↔ SourceByte` 映射（那是 `ProjectionMap` 的事，属于隐藏标记的多对一语义，
不是单位换算）；grapheme 的移动/删除策略（属于 editor 的交互策略）；
Loro、SQL、协议类型、解析器、GPUI。

### `crates/storage`

- `BodyDoc` 与**Loro 实际 API 单位适配**（只调用 text，不再实现第二套通用换算）；
- 具体 `EditorDocumentPort` / `DocumentSession` 串行 reducer；
- 会话协议：`doc/generation/session_epoch/live_rev/command`、`OpenSnapshot`、`EditRequest`、`Applied`、
  `LiveDelta`、`StaleBase`、`DurableAck`、`FlushTicket`、`HistoryState`、`AnchorToken`、`SaveError`；
- 已有的 `Vault`、journal、A/B 事务、replay、snapshot。

### `crates/editor`

- `EditorController`（持**具体端口**）；
- Rope source 镜像、选择、ProjectionMap、临时输入文档 / preedit；
- **不含 GPUI 类型**的 `LayoutFrame` / `VisualCaret`。

### `crates/app`

- 仅：GPUI `Element`、native 输入 / shape / executor 适配；
- **不实现**文档协议、Undo 或保存状态机。

---

## 5. bridge 必须守住的运行时边界

1. **每个打开的正文只有一个 live owner**；生产调用**不得**拿到可绕过 session 的 live `LoroDoc` 可变句柄。
2. 受理顺序固定：校验 `doc/generation/epoch/base/command` 幂等与区间 → **journal 容量准入/预留** →
   才允许改 Loro。**背压拒绝必须发生在 Loro 变更之前**。
3. `local / remote / undo / redo` **都过同一个 reducer 与 composition 闸门**；Rope 只按确认 delta **更新一次**
   （local echo 不得重复 apply）。
4. **唯一 journal**。现有 `journal.rs` 的 `JournalWriter::flush` 是同步调用 `sink.write_prefix` 后裁剪，
   **不能直接在 GPUI 回调里调用**：需要"连续前缀任务 → 后台 actor → 完成消息回 reducer"，
   两者只能择一封装，**不可出现两份 journal**。
5. **A 成功后才发 `DurableAck` 并裁剪**；"已保存"只由**通过当前 `live_rev` 的 durable 水位**决定；
   **B（物化）不参与保存条件**。
6. **B 是独立重放任务**：从已持久化的固定 frontier 物化，写回仍走同一 `Vault` actor + CAS，
   **不读取/不改动正在编辑的 live Doc**。
7. live reducer 可在 UI owner 线程做**短内存操作**；I/O 与重物化在后台。
   **crate 放在 storage 不决定执行线程**——位置是源码归属，不是线程模型。

---

## 6. 迁移步骤

1. 先冻结本 ADR 与端口/事件类型签名；
2. 以**现有 editor 的强类型 + 严格错误契约**为基线建立 `text`，补齐 storage 需要的函数；
3. storage 侧改为调用同一实现；`CoordError` **只有一份**，`StorageError` 包装它；
4. 旧路径可用 `pub use` **别名**过渡，但**只能别名，不得第二定义**；
5. 通用 Unicode 测试移入/复用 `text`；**Loro API 实测测试留在 storage**；
6. 现有 A/B SQL、schema、`exec_transaction` 守卫、snapshot/replay/actor 的**位置与语义不动**；`app` 空窗口暂不动。

验收（切片 1 的门禁）：原单位/范围行为全部保留；NFC/NFD、代理对中间位、多字节内部、孤立半代理、
空/尾边界等统一严格测试；**无重复通用转换或第二坐标定义**；依赖图无环。

> **不算完成的信号**：只创建空 `text`、或只共享测试但仍保留双实现。

---

## 7. 六个交付切片

| 片 | 内容 | 依赖 | 验收要点 |
| --- | --- | --- | --- |
| **1** | `text` + 双消费者迁移 | 必须先完成 | 原行为保留；无重复实现/第二坐标；无环 |
| **2** | `storage` 会话核 | 端口签名固定后可 ‖3 | stale base / 旧 epoch / 重复 command；同 request 原始基准半开区间、**逆序**应用；单 live owner；背压先于 Loro 变更；唯一 UndoManager + composition 闸门 |
| **3** | `editor` 纯模型 | 可 ‖2 | delta 单次更新、不重复 local echo；UTF-16/byte/scalar 与 source/input/display 不混用；preedit 不成为真实 source |
| **4** | 无头端到端集成 | 等 2+3 | 慢盘/保存失败/持续输入背压；旧 ACK 不标新输入已保存；A 提交 B 未完成重启恢复；旧 B 任务 CAS 不覆盖新版；两 peer + Undo/redo/remote；切换/关闭异步等待；复跑 S0 全链路门禁 |
| **5** | `app` 单 `Element` 源码 UI | 等 4（S0 通过） | **首次 GPUI 接入**；选择/命中/折行/滚动/绘制与 caret/IME geometry 统一同一 `LayoutFrame`；真实 Windows 用 `中😀é`（NFD）与长行，当场回归 |
| **6** | Windows IME + Undo + durable 恢复闭环 | 等 5 geometry 稳定 | 微软拼音 + 至少一种常用 IME；preedit 不写 CRDT/journal/Undo，commit 一次请求/单组；composition 闸门不阻塞 writer/窗口；Undo 仅本 peer 且生成新 op；Ctrl+S/切换/关闭 ticket/水位/错误/重启正文一致；**重启不恢复 Undo 栈** |

**序列：1 → (2 ‖ 3) → 4 → 5 → 6。**
2/3 必须先锁**端口签名与事件类型**，且写域（文件、Cargo 根清单）不得重叠。

**S1 只承诺"源码可见 + identity mapping"**；真实隐藏/显现属于 ADR 0002 的 **S2**，不能把 S1 当作 v1 终点。

---

## 8. v1 冻结 vs 可后补

**冻结（v1 内不再调整）**：

- `text` 的坐标/单位唯一归属；
- `storage::session` 拥有会话/协议、`editor → storage` 的依赖方向；
- 每文档一个 live owner、一份 journal、一个 Undo 栈；
- 基准校验、composition 闸门、A durable 与 B 派生的边界、GPUI 与内核的隔离。

**可后补**：O(n) 参考换算的索引加速（同单位、同错误契约）、Rope/解析器实现、容量与前缀批大小、
B 的计算调度、缓存优化、UI 细节、旧路径别名。
**不得借优化另生第二套语义实现。**

---

## 9. 后果

- **正面**：单位语义只有一处；依赖图无环且可解释；会话核可无头测试；`editor` 永不接触 GPUI；
  第 5 片之前不存在"UI 先写、协议后补"的返工。
- **代价**：`editor` 编译期会拉入 `loro` + `rusqlite`；`text` 多一个 crate 需要维护；
  第 1 片是纯重构，没有用户可见产出（必须先做完）。
- **风险**：`journal` 的同步 `flush` 若被直接搬进 GPUI 回调，会造成 UI 卡顿 + 保存语义错乱；
  第 4 片正是把这条边界测出来。

---

## 10. 一句话

**先把两套单位/坐标收成 `text` 的唯一实现（含 storage 侧迁移），这个门禁过了，才开 session 与 UI。**

---

## 11. 追加（2026-11-10）：第 5 个成员 `crates/search`

- 新增 **`crates/search`（package `menghunote-search`）**：检索索引与查询（Tantivy 0.27 + cang-jie 0.22）。
- 依赖方向：**`app → search → storage`**；`search` 不得依赖 `app`/`editor`/GPUI；`storage`/`editor`/`text`
  **不得依赖** `search`（含 dev-dependency）。
- `storage` 对 search 只暴露**只读投影快照**、`meta` 指针读写、以及轻量变更通知；
  **storage 不得知道 Tantivy / IndexWriter**。
- 将来 MCP/CLI → `search`/`storage`，不反向依赖 `app`。
- 细节（索引代、fingerprint、新鲜度协议、删除硬保证、v1 范围）见 [ADR 0005](0005-search-index.md)。

`````



@@SECTION docs/09 `docs/adr/0006-sync-v1.md`


`````markdown
# ADR 0006 — 同步/加密 v1：可执行契约

- 状态：已决（2026-11-11，`gpt-6.1-sol`，max）
- 输入：`docs/briefs/0004-sync-v1.md`；约束来自 [ADR 0001](0001-storage-format.md) §3.6/§3.7
- 结论：按本 ADR 的 wire 格式、密钥语义、API、GC 与轮换流程实现；**wire 一旦落盘不得静默变更**，升级走 v2/双读。

---

## 1. 范围（v1）

**必做**：

| 项 | 说明 |
| --- | --- |
| 随机 epoch 根、口令/恢复槽、DPAPI CurrentUser | 不做本地 DB/索引加密；Argon 调参 UI 后置 |
| 设备 Ed25519 鉴权、X25519/HPKE、独立管理凭据、一次性邀请 | 单 relay/单 vault、最多 32 个活动设备；无 SaaS 账号 |
| 持久 update 推拉、不可伪造游标、幂等、durable inbox/outbox | WS 只唤醒；无 P2P/服务端因果合并 |
| 新设备全因果快照 + manifest + 资产 + 水位 S bootstrap | 其他客户端全部关闭也能恢复；不复制本机版本归档 |
| 改口令仅重包 | 不重新加密数据；**不声称**撤销旧解密能力 |
| 吊销、冻结旧 epoch、轮换、给未吊销设备发新 grant | 不跨代截断历史；吊销可暂停同步，但**不得暂停本地保存** |
| 附件最小 BLOB 分片传输/校验/缺失标记 | 附件 UI/预览/缩略图后置 |
| 两 checkpoint GC、7 天日志宽限、维护预算 | 实际资产孤儿删除后置；v1 只标记，完整资产不按 `last_seen` 删除 |
| 强制确认备份恢复码 | 恢复码不是短数字码；同时具备数据恢复 + 管理恢复能力 |
| 配额、背压、低磁盘拒绝 | 配额管理用配置/CLI；运维面板后置 |

**明确延期**：附件 UI/预览；资产孤儿删除协议（需 30 天 + 全部未吊销设备稳定水位）；Argon 自动校准/平台扩展；
checkpoint 复用未变 doc；配额面板；HA/P2P/团队权限；跨代因果截断/版本归档跨设备。
v1 只建最小 Windows 连接/配对/恢复/改口令/吊销状态 UI，不做通用账号产品。

---

## 2. 密文信封（MNE1）

所有整数 BE、无 padding；u64 写 SQLite 前须 ≤ `i64::MAX`；UUID 用标准网络字节序。
header 固定 **196 字节**：

| offset | 字段 | 长度 |
| --: | --- | --: |
| 0 | magic = `b"MNE1"` | 4 |
| 4 | protocol_version = 1 | u16 |
| 6 | header_len = 196 | u16 |
| 8 | type: UPDATE=1, SNAPSHOT=2, MANIFEST=3, ASSET=4, KEY_SLOT=5 | u8 |
| 9 | cipher_suite = 1（XChaCha20-Poly1305） | u8 |
| 10 | flags = 0 | u16 |
| 12 | vault_id | 16 |
| 28 | object_id | 32 |
| 60 | message_id | 16 |
| 76 | producer_device_id | 16 |
| 92 | generation | u64 |
| 100 | key_epoch | u64 |
| 108 | sender_seq | u64 |
| 116 | part_index（0 起） | u32 |
| 120 | part_count（≥1） | u32 |
| 124 | total_plaintext_len | u64 |
| 132 | this_plaintext_len | u32 |
| 136 | ciphertext_len = this_plaintext_len + 16 | u32 |
| 140 | context_hash：KEY_SLOT 为 `SHA256(CDE(descriptor))`，其他全零 | 32 |
| 172 | CSPRNG nonce | 24 |
| 196 | ciphertext ‖ tag | ciphertext_len |

**AAD**：`b"MN/e2e/v1\0" || header[0..196] || BE32(descriptor_len) || descriptor_bytes`。
非 KEY_SLOT `descriptor_len = 0`；KEY_SLOT 的 descriptor 为确定性 CBOR 槽描述（不能只认证 salt 而漏掉 m/t/p/version/kind）。
接收前必须把 header 的 vault/object/part 与**请求路径/manifest 引用的预期值**比对；不能只从攻击者 header 重建 AAD 就相信路由。

**HKDF-SHA256 域**：

- `K_object = HKDF(K_epoch, salt=vault_id, info=b"MN/v1/data\0" || type_u8 || epoch_BE64 || object_id, 32)`
- `K_asset_id = HKDF(K_epoch, salt=vault_id, info=b"MN/v1/asset-id\0", 32)`
- asset `object_id = HMAC-SHA256(K_asset_id, real_sha256)`；其他 `object_id = random32`
- 每个新 epoch 的根**重新随机生成**，不由旧根导出。

`message_id = random UUID`；同一对象所有片固定 message_id/producer/epoch/type/count/total_len，nonce 每片随机。
**重试必须复用已持久化的原始密文**，禁止重试时改 nonce 又沿用幂等键。

**载荷**：RFC 8949 确定性 CBOR（CDE：最短整数、确定键序、无浮点/重复键/不定长）；资产例外为固定前缀 + 原始字节。

- `UpdateV1 = {doc_id:String, generation, codec_version, app_doc_schema, min_reader, update_id, batch_id,
  origin_device_id, origin_seq, base_vv, end_vv, end_frontier, loro_update_bytes}`；VV/frontier/逻辑 doc_id 在密文内；`origin_seq == header.sender_seq`。
- `SnapshotV1 = {doc_id, kind, generation, codec_version, app_doc_schema, min_reader, full_history=true, vv, frontier, loro_snapshot_bytes}`；**禁止 shallow/state-only snapshot**。
- `ManifestV1 = {vault_id, key_epoch, checkpoint_id, covered_relay_seq:S, min_reader, structure_doc_id,
  docs:[{doc_id,kind,generation,vv,frontier,snapshot_object_id,part_count,object_digest}],
  assets:[{real_sha256,byte_len,media_type,object_id,part_count,object_digest}]}`；
  须含当前代全部 docs（含回收站/墓碑/因果历史），资产集合不能只扫可见正文。
- `Asset 明文 = sha256[32] || BE64(byte_len) || BE16(mime_len) || mime_utf8 || data`；mime ≤255B，原始资产 ≤64MiB；
  首片可建 missing 占位，完整下载后校验真实 SHA256 与 HMAC 对象名再落 `assets.data`。
- `RootSecretV1 = {vault_id, key_epoch, root:[u8;32]}`，仅用于槽/HPKE 内部。

**对象摘要**：`object_digest = SHA256(BE32(part_count) || concat(BE32(part_index) || SHA256(exact_envelope_part)))`，按片号排序。

**签名**：

- `ObjectCommit = Ed25519(device_sk, b"MN/v1/object-commit\0" || CDE{vault_id,object_id,message_id,type,generation,
  key_epoch,sender_seq,part_count,total_len,object_digest})`
- checkpoint 另签 `b"MN/v1/checkpoint\0" || CDE{vault_id,checkpoint_id,epoch,S,manifest_oid,manifest_digest,
  refs_digest,producer_device_id,validation_version=1}`
- 公开 refs 只含 opaque IDs/密文 digest/类型/长度，不含真实 hash/VV；manifest 内 S/refs 必须与公开签名元数据一致。

---

## 3. 密钥槽与本地身份

`SlotDescriptorV1 = {protocol_version, slot_id(=object_id32), vault_id, key_epoch, kind(password|recovery),
wrap_suite=1, kdf, created_at, manager_rev}`。

- password KDF：`argon2id, version=0x13, m=65536 KiB, t=3, p=1, salt=random16, output_len=32`；
  v1 写者固定 64MiB/t3/p1；读者允许 m=64..256MiB、t=3..6、p=1..4，越界在 Argon 前拒绝。
- 口令为原样 UTF-8、不 trim、不做隐式 Unicode 规范化，1..1024B；只在客户端 KDF。
- `K_password_wrap = HKDF(Argon2id(password,params), vault_id, b"MN/v1/password-wrap\0" || slot_id, 32)`
- 恢复种子 `R = random32`；编码 **Bech32m HRP = `mnrec`**（32B 数据；不是 6 位码/问答）；
  恢复材料还须打印 vault UUID、relay HTTPS 地址、`instance_id`。
  `K_recovery_wrap = HKDF(R, vault_id, b"MN/v1/recovery-wrap\0" || slot_id, 32)`；
  `management_ed25519_seed = HKDF(R, vault_id, b"MN/v1/management-ed25519\0", 32)`。
- 管理密钥独立于 vault root：**普通根持有者不能据此注册/吊销**；恢复码则具备独立管理能力。R 不发服务器、不长期存 DB；服务端只存管理公钥。
- **DPAPI**（Windows）包裹：`{vault_id, device_id, peer_id, ed25519_seed, x25519_secret, current_epoch, current_root,
  manager_seed?(仅管理设备), pinned_instance, pinned_manager, control_rev, pending_rotation?}`，
  存 `meta` 受保护 BLOB；optional entropy = `b"MN/dpapi/v1\0" || vault_id || device_id`，**不用 LOCAL_MACHINE**。
  - **平台范围（2026-11-13 补充）**：上述 DPAPI 契约只适用于 Windows 且**冻结不变**；Linux 的
    本地身份封装（用户主密钥文件 + XChaCha20-Poly1305、`MNKW` 外层格式）见
    [ADR 0008](0008-linux-local-identity-wrap.md)，安全性弱于 DPAPI、不宣称等价；
    其它平台明确不可用。两者内层 CDE 类型与 entropy 口径相同，由 `identity_wrap` 门面分派。
- 根/KEK/口令不 Debug、不日志、用 `Zeroizing`；这不加密本地正文/索引/备份。
- 复用现有 `search.vault_uuid` 为 vault 身份；不得把 `search.active_generation` 当 doc generation 或 key_epoch。
  恢复/克隆入口强制新 device_id/鉴权密钥/Loro peer_id；生产不接受直接复制 DB 冒充在线身份。
  正常重启保持设备 peer；轮换不改 peer/doc generation。

**改口令**：当前未吊销、已解锁设备生成新的不可变 password 槽 → 从本地/relay 读回试解密并比较根 →
带 `expected_slot_set_rev` CAS 原子切 active 指针 → 删旧 password 槽。只有暂存双槽，不存在长期双 active；
旧槽 commit 前一直有效，commit 后立即移除，暂存失败 24h 回收。数据、epoch、恢复槽、其他设备 DPAPI 都不动。
槽指针更新需该已注册设备签名；新增/吊销/改管理公钥仍必须 management 签名。丢口令但仍有 DPAPI 授权设备可走同一重包路径。

---

## 4. 设备鉴权

- `DeviceCertV1 = {vault_id, device_id, sign_ed25519_pub32, hpke_x25519_pub32, cert_serial,
  issued_manager_rev, rights=READ|WRITE}`，由管理 key 签名；管理权不是 rights 里的 root 派生 flag。
- **首台**：relay init 生成一次性 256-bit setup secret（哈希存储，由服务器控制台/权限受限文件交给本人）；
  客户端用 setup secret 创建唯一 vault、公钥、初始证书/槽，再完成两基线发布；setup 端点成功后关闭。
  设备和 vault root 都在客户端生成，没有登录口令。
- **邀请**：管理设备生成 `T = random32`，relay 只存 `SHA256(T)`，24h、一次性；
  用 QR 或 `mn-pair://...#token=...&vault=...&manager_pub=...` **fragment** 传递，不放 HTTP query/日志；
  新设备本地生成两对 key，HTTPS 正文发送 T 及签名自证明，原子消费 T 仅创建 PENDING 注册；
  管理设备比对 `SHA256(vault_id||device_id||sign_pk||hpke_pk)` 前 128bit 指纹（32 hex，双端显示），
  签证书 + HPKE grant 后 activate。**知道口令不能绕过邀请/独立管理授权注册**；
  无在线旧设备时，当前恢复码可本地派生管理 key 自助授权。
- **HPKE**：RFC 9180 base mode，KEM=0x0020 X25519-HKDF-SHA256、KDF=0x0001 HKDF-SHA256、AEAD=0x0003 ChaCha20Poly1305；
  `info=b"MN/v1/epoch-grant\0"`；AAD=`CDE{pv,vault_id,epoch,recipient_device_id,recipient_x25519_pub,manager_rev,grant_id}`；
  外层管理签名覆盖 `AAD || enc32 || SHA256(hpke_ciphertext)`；内部 `RootSecretV1`。标准 HPKE 内部 nonce 不是自造 24B 格式。
- **Challenge**：`{pv, relay_instance_id, origin_sha256, vault_id, realm(device|management), subject, challenge_id16,
  nonce32, expires_at}`，60 秒、单用；客户端核对 audience/instance/realm 后签 `b"MN/v1/auth\0" || CDE(challenge)`。
  验证/消费 challenge/建 token 一个事务。
- **Token**：`random32` 的 base64url（43 字符），有效 **600 秒**，DB 只存 `SHA256(token)`；不用 JWT/永久 refresh。
  到期前 60 秒走新 challenge 刷新；WS 用 `Authorization` header，token 不入 query。
  管理 token ≠ 管理签名：管理写另签 `b"MN/v1/manage\0" || instance || vault || request_id ||
  expected_control_rev_BE64 || SHA256(CDE{method,path,unsigned_body})`。
- 每请求检查当前 device.status/auth_rev/vault.state/expected epoch；吊销 commit 起旧 token 立即失效，后续 challenge 也 403，WS 关闭；
  已发出的旧密文不能网络撤回。设备 ID 永不复用，新注册用新 keys/peer。

---

## 5. Relay API

全 HTTPS：控制面 `application/cbor`、对象 part `application/octet-stream`；WS 仅小 JSON 唤醒。
`P = /v1/vaults/{vault_uuid}`。错误统一 `{code, retryable, current_epoch?, current_control_rev?, bootstrap_hint?}`，不含秘密。

| 方法/路径 | 请求 → 响应 | 主要错误 |
| --- | --- | --- |
| POST `/v1/vaults` | setup secret + Genesis/证书/槽 → 201 rev/epoch | 403, 409 ALREADY_INITIALIZED |
| POST `/v1/auth/challenges` | realm/subject → challenge | 403 REVOKED, 429 |
| POST `/v1/auth/tokens` | challenge/proof → token/expires/server_time | 401 CHALLENGE_INVALID/USED |
| GET `P/state` | → epoch/control_rev/head_seq/gc_floor/配额 | 401, 403 |
| GET `P/control?after_rev` | → 签名管理转换/成员/槽控制链 | 409 RELAY_RESET |
| POST `P/invites` | 管理签名 + hash(T)/expiry → invite_id | 403 MANAGEMENT_REQUIRED, 409 CAS |
| POST `P/invites/{i}/redeem` | T + new keys + proof → pending_device_id | 409 INVITE_USED, 410 EXPIRED |
| POST `P/devices/{d}/approve` | 管理证书 + signed HPKE grant → ACTIVE | 403, 409 CAS |
| GET `P/devices/self/grants/latest` | → 当前 epoch 本人 grant + 管理转换 | 403 REVOKED, 409 ROTATING |
| GET `P/key-slots` | → 当前 active 槽描述/密文 | 403；旧槽仅迁移授权 |
| PUT `P/key-slots/{s}` | 描述 + MNE1 → staged | 409 ID_CONFLICT, 413 |
| POST `P/key-slots/password:activate` | device 签名 + expected 槽 rev/newid → 新槽 rev | 409 CAS/EPOCH |
| PUT `P/objects/{o}/parts/{n}` | exact MNE1 bytes → 201 或 200 同字节重复 | 409 ID_CONFLICT/OBJECT_BUSY/EPOCH, 413, 507 |
| GET `P/objects/{o}` | → 不可变元数据/各片 digest/已上传 bitmap | 404, 403 |
| GET `P/objects/{o}/parts/{n}` | → bytes，ETag = 密文 SHA256 | 404, 410 RETIRED |
| POST `P/objects/{o}:complete` | ObjectCommit 签名 → complete；asset 追加事件 | 409 INCOMPLETE/EPOCH/ID_CONFLICT |
| POST `P/updates` | ≤256 个 complete update 提交 → 每项 accepted/duplicate/failed + seq | 整个鉴权/epoch 错误 403/409；业务错误逐项 |
| GET `P/log?after={cursor}&limit` | → 升序 events, next_cursor, through_seq, query_head, epoch | 400 BAD_CURSOR, 410 CURSOR_EXPIRED, 409 RESET |
| POST `P/acks` | durable received/integrated cursor + checkpoint → 204 | 409 CURSOR_BACKWARD |
| POST `P/bootstrap` | → 两 checkpoint、各自 Cursor(S)、head、read lease | 409 NO_VALID_BASELINE |
| PATCH `P/bootstrap-leases/{l}` | 续租 → expiry | 410 LEASE_EXPIRED |
| POST `P/checkpoints` | epoch/预计维护 bytes → cid、固定 S、writer lease | 409 BUSY/EPOCH, 507 RESERVE |
| POST `P/checkpoints/{c}/refs` | ≤1024 条 opaque refs → durable refs | 409 REF_CONFLICT, 413 |
| POST `P/checkpoints/{c}:publish` | signed commit + 验证版本 → published | 409 INCOMPLETE/NON_MONOTONE/CAS |
| POST `P/rotations` | 管理签名、revoke IDs、expected_epoch/rev → 立即吊销/冻结 S/候选 epoch | 403, 409 CAS |
| GET `P/rotations/{r}` | → 阶段/S/候选 epoch/grant 集合 digest | 404 |
| GET `P/objects?type=asset&epoch=e&through=S` | 分页 opaque 完整资产集合（轮换用） | 403 |
| PUT `P/rotations/{r}/grants/{d}` | manager 签名 HPKE → staged | 409 REVOKED/RECIPIENT_MISMATCH |
| POST `P/rotations/{r}:activate` | 新槽/两基线/成员 digest/双签转换 → 新 epoch | 409 INCOMPLETE/CAS；旧写不解冻 |
| GET `P/wake`（WS） | Authorization → `{v:1,head_seq,epoch,control_rev}` | 403 后断开；消息可丢/重复 |

**limits**：非 asset 的 MNE1 片总长 ≤1MiB（明文 ≤1MiB−212）；asset 片 ≤4MiB（明文 ≤4MiB−212）；
完整 update CDE ≤64MiB、manifest ≤64MiB、单 snapshot 受配置 `max_snapshot_bytes` 限制（默认 4GiB，可随配额提高）、
KEY_SLOT ≤4KiB 明文；控制 HTTP ≤1MiB、refs 请求 ≤1024 条；WS ≤512B。
不以 64MiB/doc 预警当因果数据硬截断。同 epoch 确定性 asset ID 碰撞时，完成对象只能下载/解密/hash 验证后复用；
另一上传者未完成返回 OBJECT_BUSY，不可混拼两个生产者的 parts。

**cursor**（仅 relay 解读）：`base64url(version1 || instance16 || vault16 || log_incarnation16 || last_seq_BE64 ||
HMAC-SHA256(server_cursor_key, domain || 前 57B))`，共 89 字节。restart 保留 incarnation；
restore 备份必须显式换 incarnation 并报告 RELAY_RESET，禁止偷偷归零。`after` 排他；`next_cursor` 指最后返回事件而非 head。
`relay_seq` 只送达顺序。`>= floor` 的日志前缀不能有洞。旧 cursor < `gc_floor` 返回 410 + 当前两基线，不静默跳水位。

**客户端持久化**：`received_seq`（连续前缀所有对象验签/解密后写 inbox，依赖未齐也须 durable 保存）与
`integrated_seq`（连续前缀都真正导入 + A 事务提交），以及 `confirmed_highwater/log_id`。
AEAD/签名失败不 ACK、不推进 cursor。checkpoint 只允许 `integrated >= S` 且 ≤S 无 pending/隔离。
head 低于已确认水位或 log_id 意外改变即 ROLLBACK_DETECTED，暂停自动同步；新设备无旧可信水位无法检测有效旧 checkpoint 回滚，这是边界。

**本地迁移（新增/变更，全部经现有 storage writer）**：

- `sync_outbox` 新增 `object_id / state / cipher_digest`；ciphertext 可保持 NULL；
- 新表 `sync_outbox_parts(message_id, epoch, part_no, envelope, digest)`；
- 新表 `sync_inbox(log_id, relay_seq, object_id, epoch, state, verified_payload)`；
- A 事务只记录出站意图，不做 Argon/网络/重加密；worker 事务外加密后 CAS 落全部 parts；
- FK 所指 updates 只有明确 durable ACK 或经验证 snapshot VV 覆盖后才能回收；
- `loro_updates.seq` 是**接收端本地序号**：remote import 必须分配 `local head+1`，不能用 `sender_seq` 写现有 PK；
  post-import head/frontier/VV 由本地 Loro 重新计算。入站不再生成 sync_outbox，禁止 echo。
  相同 update_id/Loro 字节重复幂等，不同内容同 ID 硬失败。

---

## 6. Relay 存储

relay 只有两个 crate 依赖：`sync-proto` + rusqlite/HTTP/公钥签名与 MAC；**不依赖 storage/Loro/Tantivy/GPUI**。
所有表 STRICT、`journal_mode=WAL`、`synchronous=FULL`、`foreign_keys=ON`、`busy_timeout=5000`；单实例只允许一个 vault。
核心表：

- `relay_meta(key,value)`
- `vault(singleton=1, vault_id16, state initializing|active|rotating, active_epoch, next_epoch, manager_rev,
  control_rev, auth_rev, log_id16, head_seq, gc_floor, normal_limit, maintenance_limit)`
- `manager_keys(rev, pubkey32, transition)`
- `control_commits(rev, prev_hash32, signed_record)`
- `epochs(epoch, state staged|active|retired|aborted, created_at, retire_at)`
- `devices(device_id16, sign_pk32, hpke_pk32, certificate, status pending|active|revoked, created_at, revoked_at, last_seen)`
- `challenges(challenge_id16, realm, subject, nonce32, expires_at, used_at)`
- `tokens(token_hash32, realm, subject, auth_rev, expires_at)`
- `invites(invite_id16, token_hash32 UNIQUE, expires_at, pending_device, used_at)`
- `objects(object_id32 PK, message_id16, producer, epoch, type 1..5, generation, sender_seq, part_count, plain_len,
  state staging|complete, budget_class normal|maintenance|retired, digest32?, signed_commit, created_at, touched_at, UNIQUE(epoch,message_id))`
- `object_parts(object_id, part_no, envelope (212..4194304 bytes), digest32, PK(object_id,part_no))`
- `log(relay_seq>0 PK, kind update|asset|checkpoint|control, object_id, event, created_at)`
- `checkpoints(checkpoint_id16, epoch, covered_seq, manifest_oid, refs_digest32, signed_commit, state staging|published|pruned, created_at, published_at)`
- `checkpoint_refs(checkpoint_id, object_id, PK(checkpoint_id,object_id))`
- `key_slots(slot_id PK→objects, epoch, kind password|recovery, descriptor, state staging|active|retiring,
  slot_set_rev, retire_at)` + `UNIQUE INDEX active_slot_kind ON key_slots(kind) WHERE state='active'`
- `device_grants(epoch, device_id, manager_rev, hpke_enc32, hpke_ciphertext, signed_header, PK(epoch,device_id))`
- `rotations(rotation_id16, from_epoch, to_epoch, freeze_seq, member_set_hash32, state frozen|staging|activated|aborted, created_at)`
- `bootstrap_leases(lease_id16, device_id, primary_checkpoint, fallback_checkpoint, floor_seq, created_at, expires_at)`
- `device_acks(device_id PK, received_seq, integrated_seq, epoch, updated_at, CHECK(received_seq>=integrated_seq))`
- `dedup(scope, id, request_hash32, accepted_seq, response, PK(scope,id))`
- `gc_plans(epoch, cut_seq, eligible_at, proof, PK(epoch,cut_seq))`

写入约束：所有 header/descriptor 与冗余列写入前做一致性校验；relay 只验格式/公钥签名/引用存在，
**不运行 Argon/Loro/AEAD 解密**；`last_seen` 仅诊断。published checkpoint 剪枝保留 `signed_commit` 骨架、清 refs、
`manifest_oid` 置 NULL（避免 FK 阻止删旧密文）；control 链、manager/设备历史公钥与 update 幂等 tombstone **不随密文 GC 消失**。
对象 body/parts/log seq/预算 ledger/幂等响应**一个 writer 事务提交**；ciphertext 与对象引用不能跨资源 commit。

---

## 7. Checkpoint 与 GC

- 任一在线客户端累计 **16MiB** 新增 update 或 **24h**，申请**单一 checkpoint writer lease**，取固定 S，
  拉齐并 integrate 所有 `<=S` 事件，再导出完整全因果快照与完整资产清单；
  上传并从 relay 重新解密/import 校验后签 manifest 发布。relay 不能证明"VV 覆盖 S"，验证必须在合规客户端完成。
- 保留最新两份有效独立 checkpoint `C_old(S0)`、`C_new(S1)`，`S1>=S0`（相等允许）。
  每次新 pair 产生**持久计划** `{cut_seq=S0, eligible_at=C_new.published_at+604800}`；
  后来的 checkpoint **不能重置已有计划 deadline**，否则每 24h 发布会造成永不 GC。
- 执行 `floor = min(max 到期 cut_seq, 当前较老有效 checkpoint.S, 所有有效 bootstrap lease.floor_seq)`；
  只删 `<= floor` 的日志前缀及无根引用的 update 对象；事务更新 `gc_floor`。
  不按 device ACK/`last_seen` 永远留日志，也不删 Loro 历史/墓碑。旧 checkpoint 不再是最新两份且无 lease 时可剪枝；
  7 天是**日志宽限**，不能每个 24h checkpoint 都额外留 7 天完整副本。
- bootstrap lease 默认 24h、每设备一个、从创建起最长 7 天，保护 primary/fallback 及 fallback 之后日志；
  到期续传取新 checkpoint 并 merge 已有数据，绝不覆盖。完整 assets v1 不做孤儿删除；
  仅从未 finalize/未发布暂存对象 24h 可删。跨 epoch 旧资产只有新基线验证复制完整、旧根无引用且 GC 安全时清理。
- 配额：`Q` 默认 4GiB（当前两 checkpoint/资产/日志/元数据的 live 密文预算），维护 `R` 默认 2Q，
  仅用于 checkpoint 替换、轮换和宽限保留；共享 objects 只计一次。上传前必须有维护 reservation，写每片都计预算；
  正常达到 Q 返回 507 但 read/ACK/auth/维护仍可用，不能先删旧数据腾空间。维护阶段峰值上限 Q+R；
  发布前校验未来 live ≤ Q 与实际磁盘剩余空间，不足拒绝/暂停并允许显式提高配置，不能暗删/强退离线设备。

**部署**：一个 Rust relay 进程/一个 vault/SQLite WAL FULL，axum+tokio，单有界 SQLite writer 线程，
HTTP handler 不跨 await 持写事务；Linux 单容器/非 root、持久目录、反代 TLS/WSS；HTTP 仅显式 localhost 开发。
正常备份 SQLite backup API；恢复时换 `log_incarnation`、让客户端显式 reconcile，不偷偷回滚游标。无任意对象 DELETE API；
整 vault 删除用显式服务器 CLI。

---

## 8. 轮换（吊销 / key_epoch）

v1 默认每次新 password + 新 R + 新管理 key，不复用旧设备可能知道的 KEK/恢复码。

1. 先在本地 durable 保存 pending root/槽/grant 材料，强制确认新恢复码备份。
2. 管理请求 `BEGIN IMMEDIATE`：吊销目标、失效旧 token、固定旧 epoch 全部 data append 截止 S；vault 进入 `ROTATING`，
   任何旧 epoch 新追加一律 409。**本地编辑/SQLite A 事务继续，只暂停同步**；旧 epoch 只读仍允许未吊销设备。
3. 发起者拉齐并真正 integrate 至 S，合并本地离线状态；创建新随机根及新 epoch（单调递增，废弃 epoch 永不复用）。
   重加密当前完整基线中所有 doc 快照、manifest、所有已完成资产（v1 资产不做孤儿删除，所以轮换要复制旧 epoch
   所有已 finalize 资产，不能只复制可见引用）。不改本地明文、不改 doc generation；无需逐条重加密 `<=S`
   已被全因果基线覆盖的 update 日志。
4. 产生**两份可独立从空库导入**的新 epoch checkpoint，均覆盖 S，文档快照独立导出/加密（资产可共享同 epoch
   已校验对象）；分别从 relay 读回解密/import 验证。把新根**只通过未吊销设备各自的 X25519 HPKE 公钥**封装；
   设备集合来自管理签名控制链，不信任裸 relay 公钥列表。**新根绝不能用旧根/旧 password/旧 R 封装**。
5. 一个 activate 事务发布新槽、两 checkpoint、管理公钥转换（旧/新管理 key 双签）、当前成员证书与 grants，
   切 `active_epoch`，解冻新 epoch 追加，发 `EpochActivated` 控制事件。staged grants 激活前不可对普通成员读取。
6. 客户端收到/获取自己的最新 grant，先校验证书/转换链再解封；full snapshot 与本地状态合并，不覆盖离线 state；
   旧 outbox 只有在 VV 证明其已包含时才能移除，否则保持原 update_id/原 Loro 字节，用新 message_id/object_id/epoch 重包。
   旧 ACK 必须按 `(message_id,epoch,object_digest)` CAS，不能删除新版本 outbox。
7. 旧槽只对仍授权迁移设备提供最多 **14 天**，然后删除；旧密文按两 checkpoint + 日志 7 天 GC 清理。
   当前成员的"最新 epoch grant"**无 last_seen/7 天 TTL**，直至吊销或被下一 epoch 最新 grant 替代，
   保证离线多年仍可直接拿最新根。旧管理私钥立即退役，公钥/签名控制链永久保留。
8. `ROTATING` 中若追加吊销且候选 grant 已为其生成，**废弃候选 epoch/根**，分配新 epoch 重新随机密钥，
   保持旧写入冻结；不得仅删 grant 行后继续用同一新根（恶意 relay 可能已泄露那份 HPKE 包）。
   已吊销设备已取得的明文/旧历史不能撤回；其未同步编辑只能手工导出后以新身份/新 op 恢复，不自动纳入轮换。
   失败只能 resume/restart 新候选 epoch，**不能回滚到已吊销旧 epoch 继续写**。

---

## 9. 测试矩阵（不接受 sleep；用 fault injection / FakeClock / 明确 DurableAck 收敛点）

| 类 | 注入 | 必须结果 |
| --- | --- | --- |
| 信封篡改 | header 每个 AAD 字段/nonce/tag/body/slot salt,m,t,p/version 修改 | Format/AEAD 失败，CRDT/ACK/cursor 不动；超大 KDF 先拒绝 |
| 错 key/口令/recipient | 错误 epoch 根、KEK、HPKE 私钥 | 本地统一解封失败，不泄露秘密，不尝试 server 代解 |
| 重放/冲突 | 同 message exact bytes、同 ID 不同 bytes、GC 后重试 | 重复返回原 seq；冲突 409；tombstone 不被 GC 误删 |
| 游标 | 回退 head、跨 vault cursor、篡改 MAC、分页乱序/洞、restore log_id 变更 | 400/410/RESET；本地 ROLLBACK 暂停，不默默归零/跳过 |
| epoch 屏障 | 旧 push 与 revoke 并发、旧 token、staged grant 后追加 revocation | freeze 前接受的全在 S，freeze 后一律拒绝；废弃已泄露候选根；旧根/旧 password/R 不能解新密文 |
| bootstrap | 所有其他 client 关闭；仅恢复材料 + relay；结构/body/trash/assets | 空库全因果导入，正文/结构/VV/hash 一致，真正离线可读；索引本机重建 |
| 长离线 | 离线 >7d 日志 GC、>14d 旧槽、跨多次轮换后带未上传编辑回来 | 取得最新 grant/基线并合并，离线编辑保留；无 last_seen 隐退役 |
| 续传 | 片乱序/重复/缺失、半片断网、两个 asset 上传者、lease 过期 | 只重传缺片；exact 重复不加计费；不同 producer 不混片；换新基线 merge 缓存 |
| 部分成功 | push 批 256 项第 k 项失败/网络截断响应 | 已 commit 项唯一且可重试取原 ACK，剩余 outbox 保留，无跨 doc 原子假象 |
| 依赖/codec | 缺 base VV、过新 min_reader、wrong generation | durable pending 但不可 integrated/发布 checkpoint；需基线/升级，禁止旧代直接导入新代 |
| 崩溃 | A 后 parts 前、parts 入库前后、relay commit ACK 前、inbox A 后 ACK 前、refs 后 publish 前、GC 前后、freeze 后 activate 前、activate 后 DPAPI 更新前 | 每个点可重放/续传，无未 ACK 丢失，无新旧 epoch 混用；旧 ACK 不清新 outbox |
| GC | 每 24h 继续新 checkpoint、满 Q、invalid manifest、C_new 损坏、read lease 中 GC | 7 天计划仍到期；两有效基线在发布前不可删；fallback 重放；空间不足只拒绝不删 |
| 控制面 | invite 并发 redeem/过期、challenge 重放/过期、token 600s、吊销已建 WS | 一次消费、立即拒绝 token/挑战、管理权不来自 root、WS 关闭 |
| 秘密审计/Windows | 服务端 DB/HTTP 日志 canary、跨 SID DPAPI、克隆 peer、真实 IME+remote import | 无正文/真实资产 hash/口令/root；DPAPI 跨用户失败；新身份无 peer 复用；远端走同 reducer/composition 闸门 |

---

## 10. 风险与不可逆点

- wire 字节布局/CDE/AAD/口令 UTF-8 规则/vault UUID 命名空间一旦落盘不可静默改，升级须 v2/双读；
- `generation` 与 `key_epoch` 不能混用；
- 日常改口令**无法**废除旧槽解出的同一根；
- 轮换改 HMAC 资产 ID 并重传所有基线，成本大且冻结同步；
- 管理/恢复材料全失时，即使 root 仍在也不能冒充管理（能读/改 password/导出建新 vault）；
- 本地 SQLite/WAL/Tantivy/导出为明文，DPAPI 不抵御同用户进程，relay 可删数据/拒服务/看大小时序；
- 新 device 无旧水位无法抗"有效签名的旧 manifest 回滚"；
- 回滚实现只能关闭 sync 保留本地真源和 outbox，迁移前一致备份；安全吊销已提交**绝不回**旧 epoch 重新写；
  失败 resume 或废弃候选后更高 epoch 重做；不退 VV、不删未同步 updates。

---

## 11. 落地顺序（crate 与验收）

依赖 DAG：`app → sync → storage + sync-proto`；`relay → sync-proto`；`storage` 不反向依赖 `sync`；`relay` 不依赖 storage/search。

| 步 | crate | 验收 |
| --- | --- | --- |
| 1 | `sync-proto` | 固定 header/enum/CDE schemas/签名输入/ObjectDigest/cursor 类型/错误码/limits；无 IO/crypto/Loro/SQLite/GPUI 依赖。offset 196 golden vectors、恶意/截断/超量解析、编码唯一性、未知版本拒绝 |
| 2 | `sync`（密钥） | Root/KEK/R 零化、槽、DPAPI Windows 模块、HPKE、独立管理 key、设备证书/control 链。改 password 数据 cipher 完全不变；跨 SID 失败；恢复码能本地恢复管理+root；篡改全拒绝 |
| 3 | `relay` | DDL/auth/invite/parts/complete/dedup/有界 writer/配额；无 Loro 依赖。commit 后断响应重试同 seq；撤销旧 token 立即失败；服务端 canary 审计不见明文 |
| 4 | `sync` + storage 桥 | A 意图、parts CAS、inbox/local seq、received/integrated、VV 缺失恢复；app 不绕过 live reducer。两 peer 并发/断线/重启，无 echo、无错本地 seq、旧 ACK 不删新行；继承现有 A/B 保存契约 |
| 5 | relay + sync | bootstrap/read lease/checkpoint 读回验证/两份 GC/gc_plan。无其他在线 client 全量恢复；FakeClock 持续 24h 发布仍在 7 天 GC；满配额维护可提交或安全拒绝 |
| 6 | relay + sync | 完整 freeze → 两新 epoch 基线 → HPKE grants → atomic activate → 离线 merge → 旧槽 GC（含 candidate abort）。所有 rotation crash 点可 resume；旧私钥/旧 password/R 即使拿到新密文也不能解密；离线 90 天仍可最新 grant 合并 |
| 7 | app/集成 | 最小连接/配对/恢复确认/吊销状态；真实两 Windows 设备、真实 IME/断网/杀进程。Lead 亲跑三绿 + Windows 真机验收 |

---

## 12. 一句话

**relay 只做不可信的密文仓库，客户端自己保根、自己验对方、自己证明水位；`key_epoch` 是安全边界，`generation` 是数据边界，两者永不相混。**

---

## 13. 实现精度补充（2026-11-11，GPT 复核）

### 13.1 multipart 不是"整个逻辑对象一个长事务"

- **每个 `PUT part` 独立短事务**：只提交该片（`object_parts`）+ 计费，不持有跨片长事务；
- `complete` / `updates` 提交是**另一个短事务**：校验**既有全部片**、`ObjectCommit` 签名与 `object_digest`，
  然后原子地标记 `objects.state='complete'`、分配 `relay_seq`、写日志与幂等 ACK；
- **禁止"片还没齐就进 log"**：任何日志/ACK/事件都必须在 complete 事务内产生；
- 重试同片同字节返回 200 原样，不同字节 409。

### 13.2 维护 reservation 必须持久化

新增 STRICT 表（可与 checkpoint writer lease 统一）：

```sql
CREATE TABLE maintenance_reservations(
  reservation_id BLOB PRIMARY KEY CHECK(length(reservation_id)=16),
  purpose TEXT NOT NULL CHECK(purpose IN('checkpoint','rotation')),
  resource_id BLOB NOT NULL UNIQUE,
  owner_device BLOB NOT NULL REFERENCES devices(device_id),
  reserved_bytes INTEGER NOT NULL CHECK(reserved_bytes>=0),
  consumed_bytes INTEGER NOT NULL CHECK(consumed_bytes>=0 AND consumed_bytes<=reserved_bytes),
  state TEXT NOT NULL CHECK(state IN('active','complete','expired')),
  expires_at INTEGER NOT NULL
) STRICT;
```

- `objects` 增加可空 `reservation_id BLOB REFERENCES maintenance_reservations`；
- 账本放 `relay_meta` 或独立 `usage(singleton=1, normal_bytes, maintenance_bytes)` STRICT；
- 可用维护额度：`R_available = R − maintenance_used − SUM(active.reserved_bytes − consumed_bytes)`；
- **逐片原子扣减**（与 part 写入同一事务）；lease/reservation 过期**不能删除已发布数据**；
- rotation 候选根/epoch **不自动回退**：过期只允许 resume 或废弃候选后更高 epoch 重做。

### 13.3 GC 删除前校验两个 checkpoint 的引用与密文摘要

- 服务端在删除日志前缀前，校验**当前两 checkpoint 的全部 refs 存在**，且各片密文字节
  `SHA256(exact_envelope)` 与签名记录中的 `object_digest` 一致（服务端可校验 SHA256，**不需要解密**）；
- 任一项损坏 → 把该 checkpoint 标为 `unhealthy`，**阻止 gc_floor 越过可恢复的 floor**，只允许告警/暂停；
- "客户端 decrypt/import 验证"仍是**因果覆盖的唯一门槛**；服务端摘要校验只是防止删掉已损坏基线之后的数据；
- "旧私钥不能解新密文"的测试口径明确为：**被吊销设备的**私钥 + 旧 root/password/R；
  未吊销设备保留其原 HPKE 私钥，当然可以解自己的新 grant——不要把"未吊销设备能解"误判成轮换失败。

---

## 14. wire 精度澄清：域分隔符是真正的 NUL（0x00）

本 ADR 中所有 `b"MN/...\0"` 均为 **Rust 字面量写法**：源码里是单个反斜杠 + `0`，编译后是 **一个 0x00 字节**。
**不是** `\\0`（反斜杠字符 + `0` 两个字节），也不是结尾两个 ASCII 字节。

- 正式表述：每个域 = `UTF8("MN/v1/<domain>")` **后接 0x00**；AAD 域为 `UTF8("MN/e2e/v1") || 0x00`；
- 适用于：AAD、HKDF info、Ed25519 签名域（`MN/v1/object-commit\0`、`MN/v1/checkpoint\0`、`MN/v1/auth\0`、
  `MN/v1/manage\0`）、HPKE info（`MN/v1/epoch-grant\0`）、DPAPI entropy（`MN/dpapi/v1\0`）；
- **golden vector 必须用十六进制断言后缀 `00`**（例如 AAD 前缀 `4d4e 2f65 3265 2f76 3100` = `MN/e2e/v1\0`）；
- 实现者遇到"`\\0` vs `\0`"时，一律以本节的 `0x00` 为准，不得自行选择。

---

## 15. CDE 落地规则（冻结，sync-proto golden vector 门禁）

本契约每条 Record 使用**整数键 map**；字段按该 Record 声明顺序从 **0 连续编号**，编码时按整数键**升序**输出。
所有 ID/nonce/key/signature/VV/frontier/Loro bytes 一律 **CBOR byte string**，不用整数数组：
UUID=16B、oid/hash=32B、sign=64B；`update_id`/`batch_id` 仍 UTF-8 String（兼容现有 SQLite）；
`generation`/`epoch`/`seq` 为 UInt 且 ≤ `i64::MAX`；`codec` 为 UTF-8 String；`schema`/`min_reader` 为 UInt32。

**Slot**：`kind` 标签 `PASSWORD=1 / RECOVERY=2`；`Kdf` union 编码为 `[tag, params_map]`：

- tag=1 = Argon2id：`params` 整数键 0..5 依序 `version, m_kib, t, p, salt, output_len`；
- tag=2 = RecoveryHKDF：`params` 为空 map（算法/域固定）。

**optional**：不存在的 optional 字段**省略**；禁止 null 替代、浮点、重复键、不定长（RFC 8949 CDE）。

**排序**：

- `ManifestV1.docs` 按 `doc_id` 的 UTF-8 字节升序；`assets` 按 `real_sha256` 字节升序；
- 公开 refs 按 `object_id` 原始字节升序；
- `ObjectRefV1` 整数键 0..6 依序 `object_id, type, generation, key_epoch, part_count, total_plain_len, object_digest`；
- `refs_digest = SHA256(CDE(array<ObjectRefV1>))`。

**refs_digest 口径**：只覆盖 **snapshot/asset 数据对象**；**manifest 自身不纳入**该 digest（避免自引用循环）。
`manifest_oid/manifest_digest` 已由 checkpoint 签名另行绑定；数据库 `checkpoint_refs` 仍额外 pin manifest 本体。
manifest 解密后 `docs/assets` 的 ref 并集必须与该公开 data refs **恰好一致**。

**通用 enum**（如 auth realm `DEVICE=1 / MANAGEMENT=2`）用 UInt 标签；管理签名中 method 固定
`GET=1 / POST=2 / PUT=3 / PATCH=4 / DELETE=5`，path 用规范化实际路径 UTF-8，`unsigned_body` 按其类型 CDE。

**边界**：`sync-proto` 是纯类型包，**不做 SHA/crypto**，只提供确定的"待编码字节输入"；
SHA/签名由 `sync`/`relay` 计算。

---

## 16. 信封规范化（冻结）

### 16.1 generation 口径

- **UPDATE / SNAPSHOT**：`header.generation` 必须等于**密文载荷内 doc 的 generation**，且 **> 0**；
- **MANIFEST / ASSET / KEY_SLOT**：`generation = 0`。

### 16.2 sender_seq 口径

- **仅 UPDATE**：`sender_seq = UpdateV1.origin_seq`；
- 其他类型（SNAPSHOT/MANIFEST/ASSET/KEY_SLOT）**一律 0**；
  Snapshot **不得**把生产者本地 `head_seq` 当接收端 `sender_seq`。

### 16.3 固定大小传输切片

```
cap_plain = ASSET ? (4MiB - 212) : (1MiB - 212)
part_count = ceil(total_plain_len / cap_plain)
```

- 除最后一片外，每片 `plaintext_len` **恰好等于** `cap_plain`；最后一片为剩余正长度；
- 载荷均**非空**（`total_plain_len >= 1`）；
- KEY_SLOT 额外约束 `明文 <= 4KiB`，因此**必然单片**；
- 接收端严格检查：`part_count`、各片长度、`total_plain_len`、以及固定 header 上下文（vault/object/message/producer/type/epoch）一致；不一致即格式错误。

### 16.4 CDE optional 编号

CDE 省略 optional 字段时，**保留其预分配整数键编号，不重排其余键**；
v1 解码**拒绝未知键与未知 tag**（向前兼容靠 protocol_version 升级，不靠忽略字段）。

以上只落协议规范与测试，不新开架构分支。

---

## 17. 补充冻结：设备证书签名域与 HPKE grant AAD

### 17.1 `DeviceCertV1` 管理签名输入

管理密钥对设备证书的签名待签字节：

```
b"MN/v1/device-cert\0" || CDE(DeviceCertV1)
```

- 域名 `MN/v1/device-cert\0`（单反斜杠、结尾 0x00，见 §14）；
- `CDE(DeviceCertV1)` 使用 §15 规则（整数键 map，证书字段按声明顺序）；
- relay 验设备证书**必须**用这一前缀，不得自行拼 `manage` 域或裸 CDE；
- 该域与 `MN/v1/auth\0`、`MN/v1/manage\0`、`MN/v1/object-commit\0`、`MN/v1/checkpoint\0` 并列，互不复用。

### 17.2 HPKE grant 的 AAD 与签名

新增共享 Record（**放入 `sync-proto`**，避免 sync 与 relay 各写一套）：

```
GrantAadV1 {
  0: pv                  u16
  1: vault_id            16B
  2: epoch               u64
  3: recipient_device_id 16B
  4: recipient_x25519_pub 32B
  5: manager_rev         u64
  6: grant_id            16B
}
```

- AAD 字节 = `CDE(GrantAadV1)`（整数键 0..6、byte string）；
- HPKE 使用 RFC 9180 base mode（KEM 0x0020 / KDF 0x0001 / AEAD 0x0003），
  `info = UTF8("MN/v1/epoch-grant") || 0x00`，`aad = CDE(GrantAadV1)`；
- 外层管理签名待签字节 = `AAD || enc32 || SHA256(hpke_ciphertext)`；
- 内部明文 = `CDE(RootSecretV1)`；
- relay **不校验** HPKE 密文内容，但若需要验管理签名，必须按本节的 AAD/签名输入重建；客户端验签后解封。

`sync-proto` 需提供 `GrantAadV1` 的 encode/decode 与 `device_cert_signed_bytes()` / `grant_signed_bytes()` 纯函数
（只产生字节，不做 SHA/签名）。

---

## 18. 控制链哈希口径（统一）

`control_commits.record_hash` 对**所有 rev（含 genesis rev=1）**统一为：

```
record_hash = SHA256(prev_hash || signed_record)
```

- `rev=1` 的 `prev_hash` 为 **32 字节全零**；
- 不得对 genesis 特判成 `SHA256(signed_record)`（客户端原本为兼容两种都验，但这会让第三方实现踩坑）；
- `prev_hash` 恒等于上一条的 `record_hash`，`control_rev` 必须等于链尾 rev。

---

## 19. ASSET 对象的确定性 nonce（跨进程复用）

ASSET 的 `object_id = HMAC(K_asset_id, real_sha256)` 是确定性的；若每次 checkpoint 用随机 nonce 重加密同一资产，
密文字节不同 → relay 会按同 object_id 判 `ID_CONFLICT`，导致进程重启后无法发布新 checkpoint。

因此 v1 规定：**ASSET 对象使用确定性 nonce**：

```
nonce = HKDF-SHA256(K_object,
                    salt = vault_id,
                    info = UTF8("MN/v1/asset-nonce\0") || object_id32 || BE32(part_index),
                    len  = 24)
```

- **`message_id` 也必须确定性派生**（`message_id` 参与 AAD，只固定 nonce 不足以让密文字节一致）：
  `message_id = HKDF-SHA256(K_object, salt=vault_id, info=b"MN/v1/asset-nonce\0" || object_id32 || BE32(0xFFFFFFFF), 24)[0..16]`；
  同一资产的三元组 `(object_id, message_id, nonce)` 全部确定，密文字节才可复现；
- 仅用于 ASSET；UPDATE/SNAPSHOT/MANIFEST/KEY_SLOT 仍用 CSPRNG 随机 nonce 与随机 message_id；
- 同一 epoch、同一 `real_sha256` → 同一 `object_id`、同一密文字节，重传幂等；
- 该做法安全，因为 nonce 复用只发生在**明文完全相同**时（object_id 由明文 SHA-256 决定）；
- 客户端不得为"图省事"把确定性 nonce 推广到其他类型。

替代方案（持久化 asset→object 映射）也能工作，但会引入"映射丢失即无法发布"的运维负担，v1 不采用。

---

## 20. 恢复路径修正：KEY_SLOT 的 MNE1 密钥来自 KEK（不是 root）

§2 规定 `K_object = HKDF(K_epoch, ...)`，但 KEY_SLOT 的**明文就是被 KEK 包裹的 root**：
恢复设备只有恢复码/口令（→ KEK），没有 root，若 KEY_SLOT 也用 `K_epoch` 加密就会形成"要先有 root 才能解出 root"的死锁。

**修正（仅 KEY_SLOT）**：

```
IKM          = password ? Argon2id(password, params) : recovery_seed R
K_slot_mne   = HKDF-SHA256(IKM,
                           salt = vault_id,
                           info = UTF8("MN/v1/key-slot\0") || kind_u8 || BE64(epoch) || slot_id32,
                           len  = 32)
```

- `kind_u8`：PASSWORD=1、RECOVERY=2；`slot_id32` = KEY_SLOT 的 object_id；
- `nonce`/`message_id` 仍随机（与 §19 的 ASSET 无关）；
- AAD 不变（含 header + descriptor，`context_hash=SHA256(CDE(descriptor))`）；
- 明文仍是 `WrappedRoot`（KEK 包裹的 root），外层 MNE1 与内层 wrap 使用**不同 HKDF 域**；
- 其他对象类型（UPDATE/SNAPSHOT/MANIFEST/ASSET）仍用 `K_object`，不受影响。

### 20.1 `/bootstrap` 必须带 checkpoint 签名

`POST P/bootstrap` 的 primary/fallback 每个条目新增：

- **键 6 = `signed_commit`（CheckpointCommit 的 CDE 字节）**
- **键 7 = `signature`（64B，生产者设备签名）**

客户端必须验证：`S`、`manifest_oid`、`refs_digest` 与 `signed_commit` 内容一致，签名对应当前控制链中的 active 设备。
否则"客户端从 relay 读回并验证 S"这条 ADR 要求无法成立。

### 20.2 key-slots 可见性按 realm

- **device realm**：`GET P/key-slots` 只返回 `active` / `retiring`；
- **management realm**：额外返回 `staging` 槽（含 descriptor/slot_id），供恢复设备在自批流程里读取。

### 20.3 恢复码自批新设备（v1 流程）

使用现有端点，不新增 wire：

1. 恢复码 → root + management key（§3.7）；
2. 管理 realm challenge/token；
3. `POST P/invites`（管理签名，token_hash=SHA256(T)，T 本地生成）；
4. `POST P/invites/{i}/redeem`（新设备公钥 + 自证明，用 T）→ pending device；
5. `POST P/devices/{d}/approve`（管理签名 + §17.1 证书 + 用新设备 HPKE 公钥封装的 grant）→ ACTIVE；
6. device token → `GET P/key-slots`（管理 realm 已能看到 recovery staging）→ 解封 root → `/bootstrap`。

邀请仅作为**传输机制**在本地闭环使用，不要求旧设备在线。

---

## 21. Manifest 必须携带 refs_digest 的全部输入

§15 规定 `refs_digest = SHA256(CDE(array<ObjectRefV1>))`，`ObjectRefV1` 含
`object_id, type, generation, key_epoch, part_count, total_plain_len, object_digest`。
但 §2 的 `ManifestV1` 条目缺 `key_epoch` 与 `total_plain_len` —— 客户端无法仅凭 manifest 重算 `refs_digest`，
只能"信任 relay 返回的 refs_digest"，违背"客户端验证覆盖"的原则。

**修正（v1 wire 冻结）**：`ManifestV1` 的条目补齐这两个字段：

```
docs:   { doc_id, kind, generation, key_epoch, vv, frontier,
          snapshot_object_id, part_count, total_plain_len, object_digest }
assets: { real_sha256, byte_len, media_type, object_id, key_epoch,
          part_count, total_plain_len, object_digest }
```

- `total_plain_len` 是**载荷明文**总长（资产为 `32+8+2+mime+data`，不是 `byte_len`）；
- `key_epoch` 为该对象发布时的 epoch；
- 客户端 bootstrap 时必须用 manifest 条目**重算** `refs_digest` 并与 `CheckpointCommit` 中的值比对；
  不一致即拒绝（不得只比较 relay 返回值）；
- 该字段集与 `ObjectRefV1` 一一对应，`refs_digest` 的输入不再依赖任何外部查询。

---

## 22. 自批的 grant 必须封真实 root（管理 realm 可读 KEY_SLOT 对象）

§20.3 的自批流程若在 approve 阶段还不知道 root，就只能把"占位 root"封进 HPKE grant；
但 grant 的语义是"把 epoch root 交给已授权设备"，占位 grant 等于假凭证（设备解出来是错的，且会被误判为已授权）。

**修正**：允许 **management realm 读取 `type=KEY_SLOT` 的对象字节**（只限 KEY_SLOT，不放开 UPDATE/SNAPSHOT/MANIFEST/ASSET）。

自批顺序改为：

1. 恢复码 → recovery seed `R` → `K_slot_mne`（§20）与 `management_ed25519`（§3.7）；
2. 管理 realm challenge/token；
3. **管理 token 读 `GET P/key-slots`（含 staging）并 `GET P/objects/{slot_id}/parts/{n}` 取槽对象字节**；
4. 用 `K_slot_mne` 解出 `WrappedRoot` → 用 KEK 解封得到**真实 root**；
5. 新设备本地生成 sign/HPKE 密钥，管理 key 签 `DeviceCertV1`（§17.1）；
6. **用新设备 HPKE 公钥封真实 root** 得 grant（§17.2）；
7. invite → redeem → approve（携带真实 grant）→ 设备 ACTIVE；
8. device token 走 `/bootstrap`。

- relay 只放行 management realm 读 `type=KEY_SLOT` 的对象；其他类型仍要求 device realm；
- 客户端对 grant 的验收改为：从 `/devices/self/grants/latest` 取回 → 用新设备 HPKE 私钥解封 → **结果必须等于槽里解出的 root**；
- 占位 root 的 grant 只允许在旧测试夹具中出现，生产代码不得生成。

---

## 23. 轮换安全补丁：吊销后旧 epoch 永久不可解冻；管理权转换必须双签

### 23.1 旧 epoch 的"不可解冻"必须是持久事实

现实现只用"本次 rotation 的 `revoked_count` 决定 abort 是否解冻"，存在绕过：

```
begin(revoke=[D])  -> 冻结，revoked_count=1
abort              -> 仍冻结（正确）
begin([])          -> 新 rotation 行，revoked_count=0（被允许）
abort              -> 错误地解冻旧 epoch，旧根/旧 epoch 继续写入
```

**规则（v1 必须）**：

- 某个 epoch 一旦在其冻结期间发生过**任何**吊销（不论来自哪次 rotation 尝试），该 epoch 就是**永久不可解冻**；
- 该事实必须持久化在 epoch/vault 上（例如 `epochs.revoked_once` 或 `vault.revoked_epoch`），**不得**从最新 rotation 行推导；
- `abort` 只能把 vault 置回 `active` 当且仅当目标旧 epoch 从未发生过吊销；否则保持 `ROTATING`，只能 `activate` 新 epoch 或走显式恢复流程；
- 候选 epoch 一旦带过吊销，abort 后**不得复用**，必须分配更高 epoch。

### 23.2 管理权转换必须双签验证

`activate` 的 `new_manager` 不得只入库：

- 转换记录需包含 `old_rev, new_rev, new_pubkey, transition_payload`；
- 必须同时验证**旧管理 key 签名**与**新管理 key 签名**（两者都覆盖同一转换字节）；
- 缺任一签名/签名不匹配 → 409 拒绝，不得推进 `manager_rev`。

---

## 24. 轮换安全补丁 2：候选 checkpoint 不得参与 GC；grant 必须可验证

### 24.1 候选 epoch 的 checkpoint 生命周期

现实现：候选 epoch 的 checkpoint 在 `publish` 时就标 `published` 并 `schedule_gc_plan`；GC 查"最近两份基线"时**不区分 epoch**。
后果：未 activate（甚至 aborted）的候选基线会参与旧日志 GC，可能在轮换拖过 7 天或 abort 后删掉**旧根仍可能恢复所需**的日志——这不是 ADR §8 的"原子发布"。

**规则**：

- 非 active epoch 的 checkpoint 发布后只能处于 **`candidate`**，**不进入**"最近两份有效基线"，**不产生 GC 计划**，不推动 `gc_floor`；
- `activate` 事务必须**原子地**把该候选 epoch 的两份 checkpoint 从 `candidate` 置为 `published`（且都覆盖 S），此前它们不可被任何客户端 bootstrap；
- GC 的"两最新基线"查询必须限定 `epoch = vault.active_epoch AND state='published'`；候选/aborted epoch 的 checkpoint 永不参与 GC 计算；
- abort 后候选 checkpoint 置 `aborted`，不得复用 object/cid。

### 24.2 grant 必须绑定 recipient 且带可验证签名

现实现 `stage_grant` 用 `Expectation::None`，且 `grants` 表/最新 grant 响应不保存/返回外层管理签名，客户端无法验证 grant 来源。

**规则**：

- `stage_grant` 必须验证管理签名：`signature = Ed25519(manager_sk, AAD || enc32 || SHA256(hpke_ciphertext))`（AAD 见 §17.2），
  并把 `signed_header`、`signature` 持久化；
- 签名必须绑定 `recipient_device_id`、`epoch`、`hpke_enc`、`hpke_ciphertext` 摘要；任一不匹配即拒绝；
- `GET /devices/self/grants/latest` 与 rotation grant 列表必须返回外层签名，客户端用控制链中的管理公钥验签后才解封；
- 客户端不得接受"没有外层签名"的 grant。

---

## 25. 轮换安全补丁 3：资产登记表、旧槽退役、基线 prepare/validate/promote

### 25.1 资产枚举不得依赖可回收的 log

现实现 `objects::list_assets` 通过 `JOIN log (kind='asset')` 枚举；日志前缀 GC 后旧资产会漏列；且候选 epoch 的对象 complete 刻意不写 log、activate 也不补资产事件，
导致**第二轮轮换枚举不到上一轮已复制的资产**。

**规则**：

- 新增持久**资产登记表**（例如 `asset_objects(epoch, object_id, state, finalized_seq, rotation_id)`），
  对象 `complete` 时写入，**与 log 无关**；log 只是变更通知，不是事实来源；
- **relay 侧登记只存不透明 `object_id`（HMAC 派生）与 `ObjectRefV1`**；
  `real_sha256`、MIME、`byte_len` 继续只存在于加密 Manifest / asset payload 中，**不得写入 relay 表**（E2E 边界）；
- 轮换开始时冻结并记录 `asset_set_digest = SHA256(CDE(sorted ObjectRefV1[type=ASSET]))`（opaque，不含真实 hash），
  候选 epoch 复制旧 epoch 的**完整登记集合**（不是"可见引用"）；
- `GET P/objects?type=asset&epoch=e&through=S` 与 rotation/GC 的资产枚举都查登记表，不查 log；
- abort/激活不改变登记表的历史事实，只改 epoch 状态。

### 25.2 旧槽退役必须真正生效

- `retire_at` 必须由 GC tick 执行：删除 `key_slots` 行**并删除对应 KEY_SLOT 对象 body/parts**；
- 对象读取授权必须检查槽状态与 `retire_at`：过期后用 `slot_id` 直接 GET 也必须 410，不能"知道 id 就能读"；
- 仅对仍在迁移窗口内的**授权设备**保留可读；窗口结束即拒绝。

### 25.3 轮换用的基线必须 prepare → validate → promote

- 轮换基线不能复用普通 checkpoint 的"先导出再拿 S"：
  1. **prepare**：先固定 S（拉齐并 integrate 到 S），再导出快照/manifest；
  2. **validate**：从 relay 读回、解密、`refs_digest`/签名/覆盖 S 全部校验；
  3. **promote**：验证通过后才允许 `activate` 引用它们；
- bootstrap 的 fallback 不是"只换 manifest"：**必须使用 fallback 自己的 S 与 cursor**，并把 fallback 的全部 docs/assets 与本地已有内容合并（已有 body 不能直接跳过，要按 VV 合并）。

---

## 26. 轮换请求的签名必须覆盖真正被消费的字段

现状：`POST P/rotations` 的管理签名 `unsigned_body` 与实际消费的 `expected_epoch`、`revoke[]`、`new_manager`
存在**可替换的双份载荷**（外层字段与签名体各自解析），攻击者/错误实现可以让"签的是一套、执行的是另一套"。
`verify.rs` 的字段一致性校验不足以保证唯一性。

**规则**：

- 每个管理写请求只解析**一份 canonical body**；签名输入必须由这份 body 重新编码（CDE）得到，不得另行拼装；
- 或者：所有外层字段必须与 canonical body 中对应字段**逐字节一致**，任一不一致即 403（并记录原因）；
- `expected_epoch` / `revoke[]` / `new_manager` / `expected_control_rev` 全部进入被签名体；
- 同样规则适用于 `/rotations/{r}/activate`、`/rotations/{r}/grants/{d}`、`/key-slots/password/activate` 等管理写端点；
- 测试：替换外层字段但签名不变 → 拒绝；替换签名体内字段 → 拒绝；两份载荷一致 → 接受。

---

## 27. v5 迁移与管理转换补丁（不能只加列/只调 helper）

### 27.1 `revoked_once` 的迁移必须回填历史事实

`v4 → v5` 若只是 `ADD COLUMN revoked_once DEFAULT 0`，会把"旧库已执行过 `begin(revoke=[...]) → abort`、旧 epoch 仍冻结"这一事实清成 0，
升级后一次 `abort` 就能解冻。迁移必须：

```sql
UPDATE epochs SET revoked_once = 1
WHERE epoch IN (SELECT from_epoch FROM rotations WHERE revoked_count > 0);
```

- 迁移在事务内完成，失败回滚；
- 测试要**先造 v4 库并跑 begin(revoke)→abort**，再升级到 v5，断言 `revoked_once=1` 且 abort 仍保持 ROTATING。

### 27.2 管理转换必须是"被绑定 + 被持久 + 可对外验证"的完整对象

只用一个 helper 对任意 transition bytes 验两签不够。v1 要求：

- 转换对象须 CDE 解码并绑定：`vault_id, instance_id, rotation_id, from_epoch, to_epoch, old_rev, new_rev, new_pubkey, S`；
- 旧/新管理 key 的签名必须覆盖**同一份**上述 CDE 字节；任一签名缺失/不匹配 → 拒绝；
- **`new_manager` 在轮换 activate 中不得为 `None`**（ADR §8：v1 每次轮换默认新管理 key），否则新管理权要求可被绕过；
- `manager_keys` / `control_commits` 必须持久化**转换记录 + 两把签名**，而不是只存原 transition；
- `GET P/control` 必须输出该转换（含双签），客户端才能验证管理权链；
- 测试：缺 `new_manager` 拒绝；字段被篡改（rev/epoch/S/pubkey）双签失败；`/control` 返回双签且可逐级验证。

---

## 28. 轮换安全评审收敛（以评审 0001 为准）

完整评审存档：[`docs/reviews/0001-rotation-security-review.md`](../reviews/0001-rotation-security-review.md)。
**119–120 个 relay 测试不构成轮换安全验收**；v1 轮换的交付门槛是：本 ADR §8 + §23–§27 + 评审 0001 的"必须修"全部落实，
且 T01–T21 测试矩阵有对应实现证据。

### 28.1 relay 侧收口条件

1. `begin([])` 也要 bump `auth_rev`（若 ADR 规定 freeze 一律废旧 token）；
2. 候选 checkpoint 只能 `candidate`，activate 原子 promote 指定的两个 cid，并创建该 pair 的 7 天计划；GC 只看 active epoch 的 committed pair；
   abort 作废候选，旧 pair/log/lease 保持可恢复；旧 cp 无根/lease 后 prune 并清 refs，保留签名骨架；
3. activate 必须验证**两基线独立**：cid/manifest 不同、SNAPSHOT refs 不相交（同 epoch ASSET 可共享）、同 epoch/S、commit/摘要绑定；
   客户端另行在**两个互不共享的空上下文**导入并比较实际 VV；
4. 管理转换 mandatory（`new_manager` 不得 None）、共享 canonical Transition、双签覆盖全部绑定字段，持久化两签并由 `/control` 输出；
5. grant 必须按 §17.2/§24.2 验签、绑定 recipient/epoch/enc/cipher，持久化并返回签名与完整包 digest；staged grants 仅 management 可读；
6. 管理写只解析一份 canonical body（§26），并实现 `request_id` 幂等 dedup；
7. 旧槽 TTL 由 tick 真正执行（删行+对象 body），读取检查状态与 `retire_at`；
8. 资产登记表为 opaque（不含真实 hash/MIME/长度），不依赖 log；冻结 asset cutoff/digest；
9. 旧 cp/refs/跨 epoch 副本按 §25 与评审 10 清理；7 天密文 GC 与 14 天槽期限分开；
10. wire 增补以评审 0001 §三 的 10 项为准（`rotations/current`、r view 全量字段、candidate cp 元数据、staged grants、typed 双签控制记录、
    activate 指名两 cid、restart/revoke-more、frozen 资产分页、degraded 读回、EpochActivated 事件）。

### 28.2 客户端轮换状态机（阶段 A–J）

按评审 0001 §二 实现；硬性约束：

- **新 password 必须是新秘密，R' = random32，新管理 key 由 R' 派生**；不得用旧 password 加新 salt；
- 持久 journal 字段清单按评审；任何上传前固定 ciphertext；响应丢失按 request_id 查询，不盲 begin；
- freeze 后 `≤S` 有 quarantine/pending 或因果洞 → 禁止 ready；
- 两份基线在**互不共享状态**的空上下文导入验证（含 actual VV、tombstone、assets），ready 后才允许 activate；
- 本地推广必须原子（DPAPI root/manager pin/epoch），其它设备按 EpochActivated 控制事件校验后应用；
- outbox 只能用"已激活、远端 durable、已验证 baseline 的 VV"判断覆盖；绝不能用合并后的 local VV；
  未覆盖的保持原 Loro bytes，用新 message_id/object_id/E/nonce 重包，先 durable 固定 parts 再发送；旧 ACK 按三元组 CAS 忽略；
- resume/restart 规则按评审：候选根泄露或收件人被吊销 → 原子 restart（更高 epoch、fresh root/slots/R'/管理 key），旧写保持冻结。

### 28.3 交付顺序

relay 收口（§28.1）→ 客户端 checkpoint prepare/validate/promote（§25.3）→ 轮换状态机 A–J → T01–T21 →
真实双设备（含离线 90 天模拟）+ Windows 真机验收。任何一步未过，不进入下一阶段。

---

## 29. relay schema 版本对照（截至 2026-11-11）

| 版本 | 内容 |
| --- | --- |
| v1 | 初始 vault/devices/objects/log/checkpoints/slots/grants/rotations |
| v2 | `objects.commit_signature`、`log.signature` |
| v3 | `devices.certificate_signature` |
| v4 | `checkpoints.commit_signature` |
| v5 | `epochs.revoked_once`（含历史 `rotations(from_epoch, revoked_count>0)` 回填） |
| v6 | checkpoint 生命周期（`staging/candidate/published/aborted/pruned`）、`device_grants.signature` |
| v7 | `asset_objects(epoch, object_id, state, finalized_seq, rotation_id)`、`rotations.asset_set_digest` |

新迁移从 **v8** 起；任何文档/客户端若引用旧版本号，以本表为准。

---

## 30. relay 轮换收口（task-99/100/101）与客户端对齐

### 30.1 schema v8/v9

- **v8**：`checkpoints.role`、`rotations.primary_checkpoint/fallback_checkpoint/activation_commit/activated_at`、
  `device_grants.grant_id/activation_ref`；
- **v9**：`rotations.activated_control_rev`。
- 迁移链 v1→v9 实测；新迁移从 v10 起。

### 30.2 新增共享 CDE 记录（必须移入 `sync-proto`）

relay 侧为落地先定义在 `crates/relay`，**客户端无法解码/复算**，需在 `sync-proto` 冻结并出 golden vectors：

- `ActivationCommitV1`
- `SignedControlRecord`
- `EpochActivated`
- `RotationRestarted`
- `RevocationsAdded`
- 各管理 canonical body 的字段编号（begin/activate/grants/abort/slots/invites/approve）
- `member_cert_digest` / `slot_set_digest` / `grant_package_digest` 的口径

### 30.3 sync 客户端需要跟进的 wire 变更

1. `POST /key-slots/password/activate` 的被签字节收紧为
   `CDE{0: expected_slot_set_rev, 1: new_slot_id}`（不再是 descriptor）；
2. `activate` body 追加键 3–6 四个 digest（必填，缺 → 409）；
3. `GET /state` 新增键 15（current_rotation_id）；
4. `/control` 条目新增键 4（typed 控制记录 kind）；
5. latest grant 新增键 7–10（grant_id/activation_ref/leaf digest/signature）；
6. r view 键 10–26 全量；
7. `bootstrap` 新增键 7 与 `?degraded=1`；
8. 新端点：`rotations/current`、candidate cp、staged grants、`restart`、`revoke-more`、frozen 资产分页；
9. 路径用 `/rotations/{r}/restart`、`/rotations/{r}/revoke-more`（axum 不支持冒号语法，语义一致）。

### 30.4 已接受的行为

- `revoke-more` 偏保守：有新增吊销且存在候选材料/grant → 废弃候选、分配更高 epoch（安全方向，接受）；
- 同 epoch 资产孤儿删除仍后置；`asset_objects` 保持历史事实；
- degraded bootstrap 每次重算完整性；GC 在不足两份健康基线时 `blocked`、`gc_floor` 不动。

---

## 31. ASSET 确定性 nonce 修正（评审 0002 发现，必须修）

§19 的 ASSET 确定性 nonce 假设"同一 `object_id` 必然对应同一明文与同一 AAD"，但实现不成立：

- `asset_object_id(root, real_sha256)` 只提交 `real_sha256`；
- 明文是 `real_sha256 || BE64(byte_len) || BE16(mime_len) || mime || data`（`asset_plaintext`）；
- AAD 含 `producer_device_id`（以及 `message_id` 等 header 字段）；
- nonce 只由 `HMAC(root, object_id || part_index)` 决定。

后果：**同一资产字节、不同 MIME**（明文不同）或**不同设备上传**（AAD 不同）会在**同一 key + 同一 nonce** 下加密不同消息。XChaCha20-Poly1305 nonce 复用是灾难性的（keystream/MAC key 泄露），且上传前已发送，relay 的 `ID_CONFLICT` 补救不了。

**修正（v1 wire 冻结前）**：

1. `asset_object_id(root, real_sha256, byte_len, media_type)`
   = `HMAC-SHA256(K_asset_id, "MN/v1/asset-object\0" || real_sha256 || BE64(byte_len) || BE16(mime_len) || mime)`；
   即 object_id 必须提交**完整资产明文头部 + 数据摘要**（MIME/长度不同 → 不同 object_id）；
2. ASSET 的 AAD **不得包含 `producer_device_id`**；`message_id` 仍由 `asset_message_id(root, object_id)` 确定性派生，`sender_seq=0`、`generation=0`；
3. nonce 仍可 `HMAC(root, "MN/v1/asset-nonce\0" || object_id || part_index)`，因为 object_id 已提交完整明文与确定性 header；
4. 重传同一资产（同字节/同 MIME/同长度）必须产生**逐字节相同**的信封；不同 MIME 必须产生不同 object_id/nonce；
5. 兼容性：这是 v1 冻结前的 wire 修正，旧 fixture/旧资产需重新生成；`sync`/`relay` 不保留旧 object_id 兼容（尚无生产部署）。

测试必须覆盖：
- 同字节不同 MIME → object_id 不同、nonce 不同、无 nonce 复用；
- 两设备（不同 `producer_device_id`）上传同资产 → 信封逐字节相同（AAD 不含 producer）；
- 同一资产重传 → 相同；
- 旧实现的两个不同 MIME 资产若强行同 id，必须被拒绝。

---

## 32. checkpoint 的 validate 必须真的读回并导入（不得假验证）

现实现（`checkpoint.rs`）在 `prepare` 内就签 `CheckpointCommit(validation_version=1)` 并 publish；`validate` 只读回 manifest、比 report/refs/自签、查 `relay.head >= S`，
**不下载/导入 snapshots/assets，也不证明 integrated >= S**；`promote` 只是本地返回结构。active epoch 的已发布 checkpoint 可驱动 GC → 可能"假覆盖"后 GC 删掉仍需要的远端更新。

**规则**：

1. `prepare` 只做：固定 S（lease）→ 导出/上传对象与 refs → **不 publish、不签最终 commit**；返回 `PreparedCheckpoint`；
2. `validate` 必须：
   - 按 manifest 逐个下载 snapshot/asset 对象并解密，校验信封/签名/摘要/refs_digest；
   - 把每个 doc snapshot 导入**独立空 LoroDoc**，计算 actual VV，验证覆盖 manifest 声明的 VV 与 ≤S 的所有更新（不能只信载荷宣称）；
   - 校验本地 `integrated >= S` 且无 pending/quarantine/因果洞；
   - 校验资产真实 SHA256/长度/MIME 与 manifest 一致；
3. 只有 validate 全部通过，`promote` 才签名 `CheckpointCommit` 并 `publish_checkpoint`；publish 后该基线才可参与 GC/普通 bootstrap；
4. `create_checkpoint` 必须是 prepare → validate → promote 的真实顺序（read-back 后发布）；
5. 失败路径：validate 失败不得 publish；已上传对象可留待重试，但不得产生可驱动 GC 的 checkpoint。

测试：manifest 宣称 VV 虚高/缺 op、快照损坏、资产损坏、S 未 integrated、publish 前崩溃 → 均不得出现 published/可 GC 基线。

---

## 33. restart 的确定性契约与 durable journal

### 33.1 契约错误

relay `rotations::restart` 保留 `from_epoch = vault.active_epoch`（如 1），分配新的 `to_epoch`（旧候选 2 → 新 3）；
客户端 `rotation_restart.rs` 却把同一个 `current_epoch` 同时当 `from` 和 `to-1`：
- 传 active=1 会去上传 epoch 2 的废弃材料；
- 传旧候选=2 会生成本地 3，但双签 transition 的 `from=2`，与 relay 的 `from=1` 不符。

**规则**：restart 请求必须显式携带并由 relay 返回：
`from_epoch`（active）、`previous_candidate_epoch`、`new_to_epoch`（relay 分配）、`rotation_id`、`control_rev`；
客户端**不得本地 +1**，不得复用旧候选。

### 33.2 durable journal / 固定密文

现 `execute_restart` 先生成 fresh 秘密 → 上传 → activate，只在返回结果里给新 R'/password；没有 journal 持久化、没有固定 ciphertext、激活前也没有"新恢复码已备份"确认。失败会丢秘密，重试又换根。

**规则**：

1. restart 前把 fresh root/password/R'/管理 key 写入 DPAPI 保护的 restart journal（phase/secret/epoch/rotation_id），**先 durable 再上传**；
2. 上传的信封 ciphertext 必须固定并记录在 journal；重试复用同一字节，不重新生成秘密；
3. activate 前必须确认用户已备份新恢复码（`recovery_backup_confirmed`），否则不得 activate；
4. 失败/崩溃：保持旧写冻结、旧 epoch 可恢复；重启后按 journal 续跑；只有 relay 返回 activated 才推广本地身份；
5. 旧候选材料、旧 grant、旧槽一律不可复用。

### 31.1 确定性 AAD 与版本化（评审补充）

- ASSET 的 AAD 必须对同一 `(plaintext, object_id)` **逐字节确定**：`producer_device_id` 从 AAD 中**移除**（不是置 0 后仍参与），
  `message_id = asset_message_id(root, object_id)`、`sender_seq = 0`、`generation = 0`、`part_count` 由固定分片规则决定；
  其余 header 字段均由 object_id/epoch/长度派生。
- 若 relay 的对象 ownership/鉴权依赖 producer，要改为按 vault + object_id 授权，不能靠 AAD 里的 producer 区分。
- 资产 object_id 的派生域固定为 `"MN/v1/asset-object\0"`；将来任何输入集合变化必须用 `v2` 域/显式协议版本，禁止静默改已落盘 v1。
  当前尚无生产数据；修正后所有 fixture/e2e 数据重新生成，不保留旧 object_id 兼容。

---

## 34. 完整 activation T 的 wire 与 digest 口径

`sync-proto` 已冻结 `ActivationTV1`（20 字段，0..19，golden 锁定）与签名域
`MN/v1/activation-t\0 || CDE(T)`。为避免双签双方各算一套，本 ADR 钉死 T 内各 32B 字段的输入：

| 字段 | 口径 |
| --- | --- |
| `primary_commit_digest` / `fallback_commit_digest` | `SHA256(CDE(CheckpointCommitV1))`（即 relay `signed_commit` 字节的 SHA256） |
| `slot_set_digest` | `sync-proto::slot_set_digest`：按 slot_id 升序 `slot_id32 || SHA256(descriptor)` |
| `member_cert_digest` | `sync-proto::member_cert_digest`：按 device_id 升序 `device_id || cert || signature?` |
| `grants_digest` | `SHA256(concat(sorted(device_id || SHA256(signed_header))))`（relay 现口径；仅 staged/active grant） |
| `old_asset_set_digest` | `SHA256(CDE(sorted ObjectRefV1[type=ASSET]))`，取**冻结时旧 epoch 的完整 finalized 集合**（relay frozen source digest） |
| `new_asset_set_digest` | 同上，取**新 epoch 候选资产对象集合**（重加密后的 ObjectRefV1） |
| `prev_control_hash` | 当前控制链链尾 `record_hash`（`SHA256(prev_hash||signed_record)` 口径） |
| `expected_control_rev` | 链尾 rev + 1 |

其他约束：`new_manager_rev = old_manager_rev + 1`；`to_epoch > from_epoch`（restart 可跳，不要求 +1）；
`freeze_seq` 允许 0（空库首次激活）。

**wire**：`activate` 的 `new_manager`（`NewManagerV1`）扩展为：
`0 transition, 1 old_signature, 2 new_signature, 3 activation_t(CDE), 4 old_t_signature(64B), 5 new_t_signature(64B)`。
relay 必须验证两份双签（transition 与 T）且 T 的每个字段与请求材料/DB 事实逐项一致；缺 T/任一签名 → 409。
`/control` 的 `EpochActivated` 需携带 T 与其双签，客户端可复算 T 并验证。

---

## 35. 控制/成员认证边界（评审 0003 P0-A）

1. **active/revoked 只能由已签控制事实推导**：
   - active 成员来自已验签的 genesis/加入（invite/approve 控制事件）与历史管理转换；
   - revoked 只认已签 `revocation`/`revoke-more`/等价控制事件；
   - relay 返回的 `member.status` **不得作为授权依据**（只能作提示/展示）；无法由已签事实证明 active 的，一律排除（fail-closed）。
2. **genesis 信任锚**：必须来自本地 identity/pin；relay 的 rev1 只能校验相等，不能作为 anchor，也不再有"开发者 raw genesis"例外。
3. **abort 认证**：`SignedControlRecord` 的 `rotation.abort` 事件必须携带可重建被签输入的字段（rotation id、method/path、body digest 或 canonical body）；relay 不持久化这些字段时，客户端不得把 abort 当作可验证事实，只能 fail-closed。未来 wire 增补必须在 `sync-proto` 冻结并带 golden。
4. **ControlPin 必须强制**：任何消费 `/control` 的客户端（rotation A/B、apply、bootstrap 后刷新）都必须从本地 identity/`sync.pinned_*` 构造 `ControlPin`；`None` 只允许历史/测试路径，生产构造点不得默认 None。pin 的 `(control_rev, record_hash)` 只进不退，成功后原子持久化。
5. **测试负例**：保留全部真实签名/控制 hash，只把已撤销设备 status 改回 active → 必须在任何 HPKE seal/上传前失败；替换 genesis/低 pin/合法跳 epoch 分别 fail-stop/成功。

---

## 36. 覆盖证据必须实证（task-118 阻断 B）

### 36.1 缺陷

旧实现把 `(本地 cursor, S]` 里 relay 日志的 `kind=Asset` 事件直接记为 `ObservedAsset`
候选，`validate` 的覆盖复核只重扫**同一个 relay 控制的日志**，从不下载/验证资产。
恶意 relay 可以抬高 lease 的 S，并把真实 UPDATE 伪报成"没有真实密文的 ASSET"；
writer 会因此签名一个**假切点 S**，而 `bootstrap` 只验签/refs，无法发现覆盖里漏了 UPDATE。

### 36.2 结论（writer 侧最小承重集）

1. **prepare 只产 pending 候选**：`try_record_coverage_receipt` 只记录缺口里实际看到的
   **完整 `ObjectCommit` 字节 + seq**（连同 `log_id/coverage_epoch/cursor/起始 seq`），
   不再生成任何"成功覆盖证明"。结构/分页/写盘异常显式返回，不吞证明错误。
2. **validate 实证 refs**：manifest refs 里的资产必须真实读回验证（原本已有），并建立
   **完整声明键的正缓存**（键含验证器修订、根指纹、vault/epoch、OID、type、
   generation、message_id、sender_seq、part_count、总长、object_digest）。只缓存正结果；
   同 OID 不同声明或跨根不得命中。
3. **覆盖复核实证**：对每条 pending 候选，重扫日志核对**逐字节相同的 ObjectCommit**
   （event/commit/root 绑定），缓存不中则用 `coverage_root` 真实下载验证；不在 refs 的
   orphan/被替换资产同样必须验证。全部候选成功后，才合并覆盖区间并返回私有验证事实
   （durable 资产数 / 实证候选数 / 证明摘要）。
4. **共享验证器**：`upload::fetch_verified_asset` 是 engine pull 与 checkpoint writer
   唯一的 ASSET 来源认证口径——分片上限/分配保护（`try_reserve`）、固定切片、每片
   vault/OID/type/part_index/part_count/this_plaintext_len/epoch/generation/message
   上下文、AEAD（AAD）、密文 digest、明文长度、data SHA256、MIME、根派生
   `asset_object_id` 重算；只返回 `real_sha256/byte_len/media_type/object_digest` 事实，
   不返回持久明文。
5. **coverage_root 显式**：候选 epoch 的尾部分片是**旧 active 根**加密的，而 manifest
   refs 用新 epoch 根；`CheckpointWriter` 必须显式接收 `coverage_root`（rotation
   restart 传 `request.old_root`），普通同 epoch checkpoint 的 coverage_root = 当前根。
   缺旧根 → `Paused(EpochMismatch)`/明确错误，**不猜根**。
6. **本地 durable 证据独立修订**：`skipped-asset` v3（额外绑定完整 ObjectCommit）、
   `VerifiedBaseline` v3（额外绑定覆盖复核的私有事实）、coverage pending receipt v2。
   旧修订一律拒绝——不得把旧字节改号升级；restart 快捷恢复还必须命中新修订的
   durable verified receipt（journal 里的 `verified=true` 本身不是证据）。
7. **失败零副作用**：任一候选验证失败 → 不写成功 coverage/verified receipt、不
   publish、不推 cursor/ACK、不清 outbox；允许保留 lease 与 pending 进度。
8. **扫描与 S 不变量**：缺口扫描必须从可信锚连续到 S；空页、停滞、乱序、重复、洞、
   声明变化全部拒绝；S 原样等于 lease 返回值，lease 后诚实追加 `head >= S` 允许；
   可信 cursor 必须 `<= S`；`log_id/coverage_epoch` 不变。

### 36.3 v0.4 支持范围（重要）

v0.4 **只支持修复后新建的 vault/身份/控制链**。旧开发 vault 明确失效：其
`skipped-asset`/`VerifiedBaseline`/coverage receipt 都是旧修订，会被 fail-closed
拒绝；不要沿用旧根重新 bootstrap，也不要尝试迁移旧 durable 证据。

### 36.4 后置项（不在本任务范围）

- `CheckpointCommit validation_version=2` 与 proto/relay 联动（本任务不改 wire）；
- receipt HMAC / 设备签名覆盖；
- relay GC 版本门禁；
- Merkle/inclusion proof；
- Byzantine 日志完整性（多 relay/一致性证明）；
- `try_record_coverage_receipt` 的静默早退（cursor 缺失/非 UTF-8、relay state/log_id/epoch
  不匹配、无缺口等）仅浪费 lease/配额，不产生假覆盖；后续治理为显式记账/告警；
- coverage pending receipt 与 verified receipt 的 meta 行长期未清理（随 checkpoint/重启
  累积），列为后续 GC/治理项。

### 36.5 测试

新增覆盖：伪报 ASSET 掩盖 UPDATE（零成功 receipt/publish、cursor 不变）、orphan/被替换
资产实证、refs 内资产只读回一次、同 OID 不同声明/跨根缓存不命中、多片缺失/AEAD/摘要/
OID 与明文头不符、E1 tail + E2 候选（旧根成功/缺旧根暂停）、空页/停滞/重复序号/
声明变化/lease 后诚实追加、旧 verified/receipt 修订拒绝、restart 缺 verified receipt
拒绝快捷恢复。wire/编码格式（MNE1/AAD/OID/CDE、`CheckpointCommit` 字段）未改动。

`````



@@SECTION docs/10 `docs/adr/0012-autosync-two-phase-gate.md`


`````markdown
# ADR 0012 — 自动同步与两阶段持闸

> 状态：**已决**（2026-11-13，GPT 决策；S1–S3 实施）。
> 关联：[ADR 0009](0009-sync-ui-integration.md)（持闸/adopt/Phase 1 简化）、[ADR 0011](0011-sync-onboarding-ui.md)。
> 目标：真实用户不手动点同步，且同步不周期性冻结输入。

## 背景

当前 `SyncOnce` 要求 UI 先持外部提交闸门，并在**整段网络同步**期间保持 Held（ADR 0009 的
Phase 1 简化）。自动同步若沿用，会在每次 pull/push 时冻结输入，不可接受。

现状纠正：已同步持闸不仅冻结正文，还冻结**活动文档路径的结构写入与文档切换**；无活动文档
路径另禁结构写。服务看门狗会合成失败，但 worker 仍可能迟到 integrate——**超时/取消安全性
必须先修**，否则自动同步会基于“假安全终态”解闸。

真实传输预算也未接通：`DevLocal` 自建 transport，未接收服务配置；请求总预算默认 `None`。
建议自动同步连接/IO/请求预算 **5 / 10 / 30s**。

## 决策：B — 网络不持闸，只有本地集成与交付持闸

### 单操作阶段

```text
Network -> NeedGate -> WaitingGate -> HeldIntegrating -> Delivering
        -> Released -> NetworkTail -> Completed
```

- 整个流程占用同一个 operation slot；
- 网络阶段：验证并 pull 到 durable inbox，可先推送已有 durable outbox；**禁止外部文档 append、
  结构投影、live 导入**，不能直接调用原整轮 `sync_once`；
- UI 等 IME 自然结束、flush 到明确目标、取得 `DurableAck` 与 flusher quiescence；后台验证 head
  后才晋升 Held；新编辑使旧验证票据失效；
- 授权绑定 `op_id + vault_epoch + session_epoch + doc_id + gate_generation + durable_head +
  app_barrier_generation`；无活动文档也须有 vault/结构写屏障证明；
- **worker 必须可暂停**：拆 runner 阶段，增加独立握手/控制通道；`Resume/Cancel` 不能排在正等待
  授权的普通任务后面；不需要第二个同步 worker；
- `Granted→Integrating` 与 `Granted→Revoked` 原子竞争；worker 在首次外部事务边界再次检查
  授权/head；取消赢则不得 append；集成赢则等真实事务结束并交付已提交变化；
- 看门狗只产生 `DeadlineExceeded/Cancelling`，不能制造“安全终态”；只有收到“已禁止未来外部写”
  的回执且全部 durable 变化交付后才能解闸；迟到真实回执不可丢弃；
- 自动申请闸门等不到闲置/IME 结束时，约 2s 后 defer（不强制结束组合），释放操作槽，后续仍可
  推拉，避免持续输入饿死网络；
- 同步路径的 `poll/adopt/end`、prepare、重放、投影、读树均改为后台验证回执；GUI 只做内存
  reducer/adopt；现有 adopt 读库比对 payload 的校验不能删；
- 集成按最多 32 对象 / 约 1MiB / 50ms **软预算**分批；到事务边界交付并解闸再续批；慢盘/大对象
  不能硬中断，不能承诺固定最大冻结时间；
- Held 至释放期间禁：正文写、撤销重做、结构写、打开/切换文档、切库；允许滚动、复制、选区、
  目录折叠。网络阶段恢复正常编辑与切换。

### 调度

- 启动（身份解锁、初始化完成、进入 Main）合并一次；不自动 Create/Join/发布/重新 bootstrap。
- 编辑：仅本地修改触发尾缘 **2s debounce**，且目标编辑已 `DurableAck`；远端 adopt 的 ACK 不触发。
- 周期：默认 **45s**（可配 30–60s）；失焦可降到 60s。
- 网络恢复/重新聚焦：有待发送或待应用内容时合并一次探测；无平台网络回调则按退避探测。
- 优先级：收尾/取消控制 > 用户发布、设备操作、手动同步 > 编辑 debounce > 恢复/聚焦 > 周期；
  自动请求只有一个合并 pending，不抢占已开始事务。
- 自动操作启动间隔至少 5s；失败退避 5/10/20/40…300s + jitter；编辑不重置退避。手动可绕过一次
  网络退避，但不能绕过单操作、Held 或 Paused。
- 仅明确 retryable 错误重试；鉴权、TLS、签名、epoch/游标异常进入需处理状态。网络退避不阻挡
  已 verified inbox 的本地交付。

## 一致性与多文档

- 当前活动 driver 只 adopt 已 durable 增量，不走会 journal 的 `apply_remote`；保持同一编辑器实体，
  更新 flusher CAS 基准并变换选区/撤销历史。
- 网络期间切换文档时，NeedGate 以**当时**的活动 session 为准；非活动文档下次打开从 durable
  journal 重放，缓存 session 失效；changed heads 同时触发物化/索引更新。
- 结构阶段固定：集成结构 → 后台重放/投影并建立 body 行 → 重试依赖就绪正文 → 准备交付与侧栏
  快照；取消 UI “再联网补一轮”的依赖修补。
- deferred 保持 durable，按结构/正文依赖公平取批，有进展才继续；无进展等待新依赖，不循环刷
  最早一批；received 与 integrated 水位分别维护，不跨 deferred 提前 ACK。
- pull 期间新增本地编辑由 Held 前 flush 纳入新基准；`HeadConflict` 先停外部写、验证授权并对账，
  再有界重放，不能简单改 CAS、覆盖 head 或丢弃 inbox/outbox。
- 部分集成成功但网络 ACK/push 失败：先交付已 durable 内容；交付失败保持写禁用。成功仅描述
  已确认的同步 cut，残留 deferred/新本地意图仍显示 pending。

## UX

- 常驻轻量状态：`同步中 / 已同步（时间）/ 离线·待发送 N / 已接收·待应用 N / 重试于…`；
  每轮成功不弹 toast、不抢焦点。
- 普通失败保留脱敏错误与“重试”；只有交付失败且仍 Held 才显示明确冻结说明与“重试收尾”；
  不能把“收到”显示成“已应用”。

## 测试

- scheduler 与 service deadline 注入同一单调时钟（复用 `ManualClock`），测试关闭墙钟 ticker，
  显式 `advance/tick`；jitter 可注入。
- 用 ControlledRunner/transport barrier 卡住 pull 返回、授权消费、事务提交、交付返回；等明确
  `DurableAck/Integrated/Released/Completed`，不以队列为空判完成。
- 必测：持续输入/IME、网络期间编辑与切换、Held 禁写、重复触发合并、用户操作串行、取消两种
  竞争结果、超时迟到、部分提交后失败、`HeadConflict`、结构先后乱序与空库收敛。

## 实施切片

- **S1 安全底座**：修真实超时/取消收尾，接通传输预算，加入纯 scheduler；**默认不启用自动同步**，
  原手动路径仍可用。验收：`cargo test -p menghunote-app --features dev-sync --lib --test dev_sync_service`，
  barrier 证明超时后不能提前解闸、不能派发第二操作。
- **S2 手动 B 闭环**：可暂停 worker、独立控制握手、后台验证回执、结构/body 同阶段收敛；仍手动触发。
  验收：`--test dev_sync_adoption`、`--test dev_sync_sync_once`，另跑 sync/storage。
- **S3 自动接线与状态 UX**：启用调度、持久待应用恢复与公平续批，真实 AppShell 自动同步测试。
  验收：新增 `--test dev_sync_autosync`。

公共门槛：`cargo fmt --check`；`cargo clippy --workspace --all-targets --features menghunote-app/dev-sync
-- -D warnings`；默认与 dev-sync workspace 测试通过。可见功能仍需 Windows 真机验。

## 降级与回滚

- 若 B 工期不足：S1 后才允许 dev 开关启用 **A（整段持闸）**，限闲置≥2s/失焦、小批次与真实短传输
  预算；明确整轮冻结，不能用 UI 超时伪装短冻结。
- 要求“同步时继续输入”、接真实远端、或实测 Held p95 >100ms / 单次 >500ms → 升级 B，不堆 debounce。
- 回滚关闭 autosync，停发新授权并等真实安全收尾，恢复安全手动路径；保留 journals、inbox/outbox、
  身份与 pins。失败保持只读，禁止删身份/重建库假装恢复。

## 非目标

C（无闸并发 integrate / 会话重构）、wire/DDL 变更、自动轮换/发布、平台网络抽象。

### S2a 实现记录（2026-11-13，`dev`）

- **命令/事件**：新增 `Command::SyncTwoPhase`（一次操作覆盖三阶段）与**独立控制通道**
  `SyncControl::{GrantGate, Finish, Cancel, Stop}`（不经过普通 job 队列）；worker 上报
  `Event::NeedGate { authorization_epoch, .. }` / `Event::Integrated { .. }`，面板/测试通过
  `StateSnapshot.two_phase`（`SyncPhase::{Prepare, NeedGate, Integrate, Integrated, Finish}`）
  断言阶段。
- **手动同步**（有活动文档）：`sync_prepare`（encrypt/push/pull，**不** integrate/adopt）→
  NeedGate → 当时活动文档 `begin/poll` 到 Held → `grant_gate` → `sync_integrate` →
  Integrated → `prepare_committed_delta → adopt → end` → `finish_op` → `sync_finish`
  （push tail + ack）→ Completed。发布基线、无会话同步仍走旧整轮 `SyncOnce`/`SyncOnceNoSession`。
- **授权绑定**：`op_id + authorization_epoch`（worker 在 NeedGate 生成）+ `HeldGate`
  （doc_id / durable_head_seq / 闸门代际 / 会话纪元）。服务层拒绝错误 op/代际；worker 在
  `integrate` 前复核库中 head，不一致返回 `CancelledBeforeIntegrate`（不 append）。
- **超时**：Prepare/NeedGate 超时只写 `Cancelling` 并把取消穿透到等待（不发第二操作）；
  已授权后的超时**不**打断事务，等 UI 交付/解闸与真实回执，迟到回执照常记录。
- **右移**：结构/正文依赖公平取批、deferred 续批、非活动文档的即时投影属 S2b；自动调度
  接线与状态 UX 属 S3。S2a 不引入"再联网补一轮"的依赖修补。

### S2b 实现记录（2026-11-13，`dev`）

- **结构→正文依赖收敛（worker 内）**：`SyncEngine::integrate_dependency_ordered` 按
  「① 先集成 `STRUCTURE_DOC_ID` 的 durable inbox 行 → ② 结构 head 真变才
  `StructureWriter::open_with_sync(...).commit("s2b-project")` 只做投影 → ③ 正文按 doc
  轮转公平续批 → ④ 无进展时同一次调用内再重试一批」执行；不再要求 UI 补一轮网络。
  UI 删除无会话路径的「继续同步正文」补轮，只保留结构 head 变化时的侧栏刷新。
- **批次/水位/公平**：每批 ≤32 对象 / 约 1MiB / 50ms 软预算（`IntegrateConfig`），
  到每个对象自己的 A 事务边界即 durable；`integrated_seq` 只沿 `state='integrated'`
  连续前缀推进，ACK 因此**不可能越过 deferred**；`received_seq` 与 `integrated_seq`
  继续分别维护（`NeedGate.deferred` 改为真实「已接收·待应用」计数）。连续两批零进展
  即停止并上报剩余 deferred（不循环刷最早一批）。
- **changed heads / 物化**：`IntegrateReport` 增加 `structure_integrated`/`body_integrated`/
  `batches`/`projected`，`IntegrateOutcome.changed_heads` 只含**真实前进**的结构/正文文档；
  正文 changed heads 通过既有 `projection_outbox` → `materialize_body`（`BodyMaterialized`
  事件）触发物化/索引增量，结构投影由 `StructureWriter::commit` 发布 `StructureProjected`。
- **投影失败 fail-closed**：新增 `SyncError::StructureProjection`；UI 收到
  「结构投影失败」标记后先交付已 durable 正文，再**保持闸门/写禁用**并保留重试收尾入口，
  不把失败显示成「已刷新」。
- **HeadConflict / 对账**：Integrate 前复核持闸 head（不变则 `CancelledBeforeIntegrate`，
  不 append）；单个对象的 `append_updates` CAS 冲突在**不覆盖 head、不丢 inbox/outbox**
  的前提下按新 head 有界重放一次，再冲突就明确失败，重试可交付。
- **非活动文档**：本轮只 adopt 活动文档；非活动正文下次打开由 `SessionDriver::open`
  从 durable journal 重放（不读旧缓存 session），changed heads 触发既有物化/索引接口。
- **测试**：`dev_local.rs` 新增结构先于正文收敛、批次上限/续批、ACK 不越 deferred、
  授权过期不 append 且重试可交付；`dev_sync_sync_once.rs` 新增「一次两阶段同步后树与
  正文都出现（Prepare/NeedGate 各一次）」「非活动文档切换后重放」「部分集成成功但
  Finish 失败仍先交付」。
- **仍未做（S3）**：自动调度接线、状态 UX、跨操作/跨解闸的公平续批（S2b 的续批发生在
  同一次 Held 的 Integrate 内）；本片未启用 autosync。

### S3 实现记录（2026-11-13，`dev`）

- **调度接线（AppShell / Main）**：新增 `dev_sync::autosync`（纯逻辑 + 注入时钟）：
  - 进入 Main（身份/vault 就绪、`AppShell` 构造完成）即接线；**构造期只登记启动合并**
    （`AutoSync::prime_startup`，只有 durable outbox/inbox 有待办才排队），第一拍（40ms
    ticker / 测试显式 tick）才真正走调度决策，避免构造期重入同步状态机；
  - 时刻表：启动合并一次、本地编辑 `DurableAck` 尾缘 **2s**（`EditorView::dev_sync_local_edit_seq`
    只在 `EditOutcome::Applied` 自增，远端 adopt 结构性地不触发）、周期 **45s**、失焦
    **60s**（`Scheduler::set_period` 按"上一次周期点 + 新周期"平移，不补发错过的周期）、
    重新聚焦时有 durable 待办合并一次探测、失败退避 5/10/20/40…300s + 可注入 jitter、
    自动启动间隔 ≥5s、自动请求**只有一个合并 pending**；
  - 优先级：收尾/取消控制 > 手动同步/发布/设备命令 > 编辑 debounce > 恢复/聚焦 > 周期。
    自动轮在跑时点「同步」只**合并**（`AutoSync::queue_manual`），本轮结束后的第一拍补一次
    手动轮；自动遇到 Held/单操作（面板的自动设备刷新等）先退回这次 `Start`
    （`on_busy_after_start`，绝不留下假的 `Running`）再合并"稍后重试"。发布/设备命令与
    自动轮共用单操作槽：自动在途时它们仍按单操作语义得到**明确 Busy**（不静默排队、不
    谎报成功），自动轮收尾后由用户/面板重发；面板的**自动**设备刷新本来就只在空闲时派发
    （`refresh_pending`），不构成抢占；
  - 墙钟 ticker 只在**生产入口**开启（`new_with_dev_sync_with_autosync`，`main.rs`/`AppRoot`
    的 Main 用）；`new_with_dev_sync` / `new_with_dev_sync_service` 保留接线但
    `AutoSyncConfig::no_ticker()`，headless 用显式 tick + 手动时钟（同一 `OperationClock`）。
- **自动 B 阶段（两阶段 + defer）**：自动轮复用 S2a 同一状态机与单操作槽——
  `Prepare`（网络，不持闸，可编辑/切换）→ `NeedGate` → 在**当时**活动文档上 `begin` 闸门 →
  `Held` → `GrantGate` → `Integrate` → `Integrated` → `adopt → end` → `Finish`。
  自动专属的一条：等 IME 自然结束/空闲（`GateState::WaitingIME`）超过 **约 2s** 就
  `defer`——释放票据（尚未 Held，worker 绝不会 append）、`Cancel` 本轮、把 durable inbox
  留在库里并合并"稍后续批"；**不强制结束组合**。手动轮保持原语义（继续等，不替用户放弃）。
  `Held` 仍只覆盖 integrate+adopt。
- **状态 UX（不打扰）**：`AutoSync::SyncStatus` 是唯一投影，侧栏底部常驻一行
  （selector `sync-status`；"需要处理/交付失败仍冻结"才多一行 `sync-status-error`）：
  `同步中` / `自动同步中` / `已同步 HH:MM`（UTC，dev-only，不引入时区依赖）/
  `重试于 Ns 后` / `需要处理：<脱敏原因>` / `已接收·待应用 N（续批暂停：无进展）`，
  并追加真实的 `待发送 N` / `已接收·待应用 N`（`deferred` 数来自 `NeedGate/Integrated`
  与库里的 `count_inbox_ready`，**收到 ≠ 已应用**）。每轮成功不弹 toast、不抢焦点；
  普通失败只保留"重试"，只有交付失败且闸门仍 Held 才写冻结说明与"重试收尾"。
  面板的同步行在自动轮用「自动同步：…」前缀；手动「同步」按钮语义不变。
- **重启恢复与跨操作续批**：启动时读 `durable_backlog`（`sync_outbox` 的
  `pending+encrypted` + `sync_inbox` 的 verified 连续前缀），有待办就必然排一次，
  不依赖"上次有没有 2s 编辑信号"；relay/实例切换走新的 `AppShell`（新配置）自然生效。
  一轮 Integrate 因批次预算/依赖留下 deferred 时，**不在同一次 Held 里无限续**：
  收尾后按 `remaining = count_inbox_ready` 与"本轮有没有进展"（`integrated > 0` 或
  durable 待应用真的变少）决定——有进展才 `on_continuation`（受 5s 最小间隔约束），
  连续无进展转 `StatusState::NoProgress { remaining }` 并如实显示剩余，不再空转。
  关窗（`close_all`）先 `AutoSync::stop()` 再请求 worker 停止，**不 join**、不等网络；
  S1/S2 的安全收尾语义不变。不自动 Create/Join/发布/重新 bootstrap。
- **测试**：新增 `crates/app/tests/dev_sync_autosync.rs`（7 个用例，手动时钟 + 真实
  dev-relay 或脚本 runner，关闭墙钟 ticker、显式 tick，无 sleep）：启动待发送→两阶段→对端
  收到；编辑 2s 尾缘合并/远端 adopt 不触发/手动优先/≥5s；退避序列 + jitter + 仅 retryable
  重试 + 鉴权类需处理；Prepare 可输入/切换 + NeedGate 遇 IME 组合 defer 且结束后自动继续；
  跨操作续批有进展才继续、零进展暂停并显示剩余；重启（新 AppShell）有待应用自动同步；
  状态行 selector/文案与手动同步/发布回归。纯逻辑的 `autosync`/`scheduler` 另有单测
  （尾缘合并、周期/失焦、jitter、defer 不计失败、5s 最小间隔、Status 文案）。
- **仍未做（后续）**：`key_epoch` 轮换与 metadata 事件认证/对账（task-121…124）；
  Windows 真机验收（自动同步/输入法/失焦降频都需要真机复核）；平台网络回调（当前用
  周期 + 聚焦探测代替"网络恢复"信号）。

`````



@@SECTION docs/11 `docs/adr/0026-relay-deployment-and-client-releases.md`


`````markdown
# ADR 0026：relay 已部署，默认只发布客户端

- 状态：已接受（2026-11-20）
- 相关：[ADR 0013](0013-production-relay-pinning.md)、[ADR 0018](0018-embedded-web-admin.md)、[ADR 0014](0014-multi-vault-invites-management.md)
- 操作指南：[Windows 下编译与部署 Linux ARM64 Relay](../guides/relay-build-deploy.md)
- 部署地址：`https://zh-menghu-relay.odn.cc`

## 背景

用户已在真实服务器上部署 relay，并完成 **TLS 连接验证**。

当前发布物是 Windows 客户端 zip；用户日常只重新编译/运行 app，不重新构建 relay。
过去 relay 与 app 在同一个 workspace 里，改动 `crates/relay`、`crates/sync-proto`、
relay 配置或 schema 时，很容易让服务器端与客户端版本不一致。

## 决策

1. **默认只发布 app**。用户构建/运行：
   ```bash
   cargo build -p menghunote-app --release
   ```
   或使用仓库打出的 Windows zip；不再默认要求用户编译/部署 relay。
2. **relay 程序的变化必须显式通知**。任何触及以下内容的改动，Lead 在交付说明里必须单独列出：
   - `crates/relay`
   - `crates/sync-proto` 的 wire/schema/版本
   - relay 的配置、数据库 migration、管理面/路由、限流/鉴权行为
   - 需要重新 `init`、迁移数据库、改 `admin_allowed_hosts`/`public_url` 的变更
   并给出**是否需要更新 relay、更新步骤、是否需要重启/迁移**的明确结论；不能只给 app zip。
3. **客户端 TLS 仍走系统信任根**，不内置私有 CA、不忽略证书；relay 继续由反向代理终止 TLS。
   真实公网 TLS 已由用户验证；这不等于完成安全审计，仍不因此打生产安全 tag。
4. ADR/交付说明记录部署事实；relay 的具体版本按提交哈希追踪。

## 后果

- 用户默认只更新 app，不会被无关的 relay 改动打扰。
- relay 改动需要额外的发布说明与真机/服务器操作；CI/打包脚本不应把 relay 默认混进客户端 zip。
- 若未来 relay 与客户端 wire 不兼容，必须通过显式版本协商/迁移处理，不能靠"用户顺手一起编译"。
- 本文只记录部署与发布流程决策，不改变 relay 的协议/认证/schema。

`````



@@SECTION docs/12 `docs/briefs/0004-sync-v1.md`


`````markdown
# 简报 0004 — 同步/加密 v1 落地契约（喂 GPT）

> 输入给 `gpt-6.1-sol` 做决策；ADR 0001 §3.6/§3.7 已定**方向**，本简报只求**可执行细节与 v1 范围裁剪**。
> 决策落地后写入 `docs/adr/0006-sync-v1.md`。

## 1. 问题

在既有实现（SQLite 本地真源 + Loro 分区 CRDT + 检索）之上，落**自建中继 + 端到端加密**的第一版。
需要你拍板：**v1 范围**、**密文信封与 AAD**、**密钥槽与设备鉴权**、**relay API/存储**、**测试矩阵**，
以及明确**这一版不做什么**。

## 2. 约束（不可协商）

- 平台 Windows 桌面优先，Rust；本地**明文**（依赖 BitLocker），relay **只见密文**。
- ADR 0001 已定：随机 256-bit vault 根密钥；Argon2id 只派生 KEK 封装根密钥；XChaCha20-Poly1305；
  HKDF-SHA256 分用途子密钥；DPAPI CurrentUser 本机包裹；口令**永不发服务端**；
  设备吊销需轮换 `key_epoch` 并重发布加密基线。
- relay：持久密文中继，**不运行 Loro、不做搜索、不见明文**；HTTPS 拉取 + WS 仅作唤醒。
- 单用户自建（MVP 不做多租户 SaaS）：一个 relay 实例可托管多个 vault，但按 vault 隔离。
- 仓库依赖方向：`relay → sync-proto`；`sync → storage + sync-proto`；`app → sync`；
  `sync-proto` 纯类型（serde），不依赖 storage/GPUI。

## 3. 现状（已实现）

SQLite 表（节选，STRICT）：

```sql
loro_docs(doc_id PK, kind, generation, codec_version, min_reader,
          snapshot BLOB, snapshot_seq, snapshot_frontier, head_seq, head_frontier, head_vv)
loro_updates(doc_id, seq, update_id UNIQUE, batch_id, codec_version,
             payload BLOB, end_frontier, end_vv, PK(doc_id,seq))
sync_outbox(message_id PK, batch_id, doc_id, seq, key_epoch, ciphertext BLOB,
            FK(doc_id,seq) -> loro_updates)
projection_outbox / index_outbox（物化与索引，不参与同步）
assets(asset_hash BLOB PK, byte_len, media_type, data BLOB, orphan_since)
meta(key TEXT PK, value BLOB)   -- 现有键：search.vault_uuid / search.active_generation
```

- `sync_outbox.ciphertext` 与 `key_epoch` 已预留，**尚无任何读写代码**；
- 无 `vault_uuid` 之外的同步身份；`peer_id` 不入库（ADR 0001 §3.2）；
- 附件表存在、UI/导入未接（v1 可先不同步附件）。

## 4. 需要你决断

### A. v1 范围裁剪（最重要）
候选内容：密钥槽/口令/DPAPI、设备注册与鉴权、update 推拉（游标）、新设备 bootstrap（快照+manifest）、
改口令重包、设备吊销轮换、附件同步、relay GC（两 checkpoint）、恢复码、配额。
请给：**v1 必做 / v1.1 可后置 / 明确不做**，并说明为什么这些不能省。

### B. 密文信封格式与 AAD
请给出二进制布局（magic/version/type/key_epoch/nonce/密文长度/明文长度等）与 **AAD 字段的精确清单**
（vault_id、object_type、object_id、generation、key_epoch、seq/part、protocol_version 等）；
以及 manifest / snapshot / update / key_slot 四类对象是否共用同一信封。

### C. 密钥槽与本地身份
- 槽表/本地 meta 的字段（password / dpapi / recovery 三类）；Argon2id 参数如何随槽存；
- 改口令流程（新增槽 + 删除旧槽，是否需要过渡期双槽）；
- 恢复码形态（高熵一次性，如何不落明文日志）；
- `key_epoch` 轮换的最小正确流程（哪些对象要重加密、旧槽保留多久）。

### D. 设备鉴权与注册
- 设备密钥算法（Ed25519？）、注册流程（首台设备如何建立账户；邀请如何传递高熵一次令牌）；
- challenge-response 与短期 token 的具体格式/有效期/刷新；
- 吊销语义（立即拒绝、轮换 epoch、旧密文如何处理）。

### E. relay API 与存储
- 端点清单（方法/路径/请求响应体/错误码）、游标语义（单调不透明）、幂等键、批量与单对象大小上限；
- WS 唤醒消息形状；
- relay 侧表结构（SQLite STRICT）与配额/GC v1 是否实现；
- 部署形态（单二进制 + 配置文件 + docker）与备份注意点。

### F. 测试矩阵与故障注入
必须覆盖：篡改密文/AAD 不匹配、错误密钥、重放、游标回退、key_epoch 不匹配、
新设备离线 bootstrap、推送中断续传、单批部分成功、SQLite 崩溃点。

## 5. 已排除（不要重复提）

- 口令直接派生数据密钥（改口令需重加密全库）；
- relay 纯转发（离线设备日志无处存放）；
- 服务端合并/服务端搜索（违反 E2E）；
- 明文资产 hash 作为 relay 对象名（用 HMAC 派生不透明 ID）；
- 多租户 SaaS 与账号体系（v1 自建单用户）。

## 6. 产出要求

1. **推荐方案**（v1 范围 + 上述 B–E 的具体格式/DDL/端点），按可执行粒度给；
2. **理由与风险**；不可逆点与回滚方式；
3. **明确延期项**（这版不建什么，避免团队顺手做了）；
4. **落地步骤**（可按 crate 拆分：`sync-proto` / `sync` / `relay`，每步验收判据）。

请直接给结论，不要泛泛而谈；细节用代码块/表结构/端点表表达。

`````



@@SECTION docs/13 `docs/briefs/0007-sync-handoff.md`


`````markdown
# 0007 同步/加密 v1 交接（新窗口开发用）

> 历史文档：以下 commit 哈希属于 2026-11-16 整理前历史；映射与 backup 分支见 [history-rewrite-20261116](../notes/history-rewrite-20261116.md)。


> 状态：**v0.4-sync 已由 GPT 复审 APPROVE，并打 annotated tag**（非生产，仅 localhost 开发验收）。
> 复审锚点：整理前 `321f732`（等价新提交 `61d8de4`；APPROVE，无必修项）。
> **tag 前移（2026-11-13）**：`v0.4-sync` 现指 `d16f6fe`（含 e2e 去脚本化 / 8 条 headless UI 测试 / 文档）；
> 旧锚点保存在 `v0.4-sync-backup-20261113`（→ `738f3f0`）。产品 `crates/*/src` 逻辑与 `61d8de4` 一致（新增仅 `debug_selector`）。
> **Windows 实机已验证**：用户于 2026-11-13 用 `5538344` 源码包（`menghunote-v0.4-sync-5538344.zip`）实测通过；
> 该包与 tag 的 `crates/*/src`、`Cargo.toml/lock` **完全一致**，差异仅为 e2e 驱动/文档/脚本（命令细节未逐条留档）。
> **历史整理（2026-11-13）**：`dev` 已重建为 12 个语义提交（tree 与整理前 `3c69f16`（backup tip）一致）；
> 文中旧哈希（含 `321f732`）只在 `backup/dev-20261113` 可达，见 [`docs/notes/history-rewrite-20261113.md`](../notes/history-rewrite-20261113.md)。
> 代码 HEAD：`61d8de4`（整理后；整理前 `321f732`）。本机（Linux）基线：
> `cargo test --workspace --exclude menghunote-app` = **1006 passed / 0 failed**；
> `cargo test -p menghunote-sync --all-targets` = **417 passed / 0 failed**；
> two-round e2e `EXIT=0` 且打印 `TWO_ROUND_EXIT=0`。
>
> **v0.4 只支持修复后新建的 vault/身份/控制链**；旧开发 vault 不得复用（原因见 §3）。
>
> **task-122 已完成（2026-11-19，未 commit）**：`CheckpointCommit.validation_version=2`
> （v1 解码保留、所有消费者显式拒绝）、receipt HMAC、relay GC 版本门禁已落地；
> 完整预算框架明确未做。详见 [`docs/notes/task-122-validation-v2.md`](../notes/task-122-validation-v2.md)。

> **ADR 0009 Phase 1（dev-only app 集成）完成（2026-11-13，HEAD `7f0e4fe`）**：
> 切片 1（面板/状态机/worker）→ 2（身份/peer/intents）→ 3a（外部提交闸门 + adopt）→
> 3b（EditorView/AppShell 转发）→ 4b（GPUI-free `DevLocal` 编排）→ 4c-1（启动期
> Create/Join）→ 4c-2（持闸 SyncOnce）→ 4c-3（create 一次性输出 vault_id/恢复码、
> `--dev-sync-publish` / `--dev-sync-info` CLI）全部落地。
> UI 先持外部提交闸门 → worker 线程里跑完整 `push/pull/integrate/ack` → 活动文档
> `prepare_committed_delta → adopt → end` → 编辑器**不重开**就看到远端文字；面板
> 「同步」按钮走同一条路径（面板只发请求，持闸由 `AppShell` 做）。
> dev-only 简化：**整段 sync 期间持闸**（不只 integrate）、只 adopt 活动文档、结构树仅在
> 结构 head 变化时重投影（做不到的部分见 ADR 0009 的「已知边界」）。
> 验收：`cargo test -p menghunote-app --features dev-sync --test dev_sync_sync_once`
> （真实进程内 relay + 两个 AppShell 双向文字 / 失败释放闸门 / 面板按钮）；
> app 默认 134 / feature 193 / sync 432+1；手工双实例步骤见
> [`docs/notes/dev-sync-manual-test.md`](../notes/dev-sync-manual-test.md)。
> **Windows 真机手工闭环尚未跑**（本机 Linux 缺 `libxcb-xkb.so.1`，只到 headless 集成测试）；
> Create/Join/Publish 仍 CLI，轮换/metadata 对账属 Phase 2。

## 1. 一句话结论

原三个阻断（ASSET 来源认证、post-activation 基线消费、同一 durable E1 跨两轮 e2e）都已修复并有 Linux 真实执行证据。
Windows 侧已由用户于 2026-11-13 用 `5538344` 包实测通过（与 tag 的产品代码一致；当时命令与计数未逐条留档，复跑命令见 §5）。

## 2. 三个阻断的落地

### 2.1 ASSET 来源认证（原 §3.1）

以下四项同属整理后的 `2c88f72`（整理前分别为 `5b07242`/`2ed4d87`/`c4453ef`/`9aa07f0`）：

- pull 消费 ASSET 前真实下载全部 parts，验证每片 header/固定切片/AEAD、明文长度、data SHA256、MIME、canonical `asset_object_id`；写 durable skipped-asset v2 记录后才推进游标。
- 阻断 A——`ObjectCommit::validate` 给 ASSET 补 64MiB 上界（与 `ObjectRefV1` 同口径），读回层改 `try_reserve` 并有界分配，engine 先验 epoch/长度再下载；恶意 `part_count=u32::MAX` 不再触发进程 abort。
- 阻断 B——checkpoint 覆盖证据必须实证：
  - `prepare` 只产出 pending 候选，不产生成功覆盖证明；
  - `validate` 对每条候选复核完整 `ObjectCommit` 并真实下载验证；refs 内资产复用读回缓存，orphan/被替换资产也下载；缓存键含验证修订/根指纹/vault/epoch/OID/类型/generation/message/sender/parts/总长/digest，只缓存正结果；
  - 候选 epoch 的 `coverage_root`（旧 active 根）显式传入，缺根即暂停；
  - 本地 durable verified/coverage receipt 升修订（v3/v2），旧记录一律拒绝或强制重验。
- 删除「inbox OID 属于 manifest 资产集即 proven」捷径（必须走 `actual_vv` 绑定）；relay 改写已消费 seq 从 `LocalCorrupt` 改为 `Paused(RelayHistoryRewritten)`。

### 2.2 post-activation 基线消费（原 §3.2）

`2c88f72`（整理前 `bf24185`）：I 阶段把「T/latest grant 对根的授权」与「当前 checkpoint 自身认证」解耦。

- 保留全部 T 检查（双签、链逐环、control_rev/prev_hash/管理 rev、grant inclusion、根↔epoch、实例 pin）；
- pair 内路径继续强制 T 的 commit digest 强绑定，**不得降级**；
- 非 pair 路径走完整认证：`published`、生产者证书且属 active 成员、`CheckpointCommit` 签名、manifest/refs 复算、实际导入、`S ≥ freeze/gc_floor`、`S ≤ head`、本地水位；
- 激活后普通健康 checkpoint 替换/prune 激活 pair，旧设备仍能 apply（现有负例翻转为正例，另加 12 条负例与多 epoch 正例）。

### 2.3 同一 durable E1 跨两轮 e2e（原 §3.3，T13）

`61d8de4`（整理前 `58117dc`）：`crates/sync/examples/e2e_rotation_two_rounds.rs` 改为**自包含进程内 relay**（`examples/dev_relay.rs`：临时 SQLite + `FakeClock` + 仅 loopback 放行验签 hook + `127.0.0.1:0` + 独立线程 axum），不再需要 PowerShell/bash 编排，也不新增 dev HTTP 端点。

实际场景与断言：

- B 在 E1 bootstrap 后**全程离线**：identity 文件 SHA256、`current_epoch=1`、grant epoch、cursor/inbox/outbox 快照前后不变，两轮期间不构造 client；
- A：E1→E2（普通轮）→ begin 分配 E3 → `POST /rotations/{r}/restart` 原子废弃 E3 → **E4**；控制链 `epoch.activated=2 / rotation.restarted=1`；
- 两轮后 A 发布 2 份普通 checkpoint 顶掉激活 pair；`FakeClock` 前进 90d + GC×3（`pruned_checkpoints=8, purged_slots=3, gc_floor=3`），激活 pair 变 `pruned`、refs=0，但 manifest/signed_commit/签名保留，旧链仍可复算；
- B 用同一 E1 身份/旧 grant 走真实 HTTP/HPKE/Loro apply 到 E4（`baseline_s=3`）；apply 的
  candidate root 直接使用 B 从 relay latest grant 用**旧 HPKE 私钥解出的根**（先 `ct_eq` 与 A 的
  candidate 交叉断言），不是 A 的测试根；离线编辑与 local-only 保留、水位单调、outbox repack 2/0/0 不重复；
- 负例：E1/E2 根解不开 E4 outbox part 与 E4 基线快照；E1/E2 口令与恢复码 R 解不开 E4 槽；
- 运行：`cargo run -p menghunote-sync --example e2e_rotation_two_rounds` → `TWO_ROUND_EXIT=0`。

### 2.4 tag 后迁移：三条 e2e 全部改为自包含 example（tag 本身未改动）

`v0.4-sync` tag 之后，剩余两条 PowerShell e2e 也迁移为**进程内 relay 的 Rust example**，
`scripts/e2e_rotation_verify.ps1` / `scripts/e2e_rotation_two_devices.ps1` 已删除；
**`v0.4-sync` tag 内仍保留这两条 ps1**（tag 不动，历史版本可 `git show v0.4-sync:...` 查看）。

- `crates/sync/examples/e2e_rotation_verify.rs`：启动 `DevRelay`，在同一 relay DB 上依次调用
  `e2e_localhost::run`（genesis + localhost 同步 + 初始槽/checkpoint）与
  `e2e_rotation::run`（同一 vault 的轮换 D–F），两阶段之间不重启 relay；成功打印 `VERIFY_EXIT=0`。
  `e2e_localhost` / `e2e_rotation` 的 `main` 已拆成可调用入口 + CLI 薄壳，`--relay/--setup` 外部模式保留。
- `crates/sync/examples/e2e_rotation_two_devices.rs`：缺省自起 `DevRelay`，成功打印 `TWO_DEVICE_EXIT=0`；
  显式 `--relay` + `--setup-secret` 仍走外部 relay。
- 至此三条 e2e（two_rounds / rotation_verify / two_devices）都只需 cargo 命令，Windows 不需要 PowerShell。

## 3. v0.4 范围与限制（必须写进 tag 说明）

1. **只支持修复后新建的 vault/身份/控制链**。旧开发 vault 的本地 durable 记录会被新修订拒绝（可重 bootstrap）；但**旧签名 checkpoint 无法仅靠本地版本拒绝隔离**——`bootstrap` 仍会接受修订 1 的签名基线。未实现 `CheckpointCommit.validation_version=2` 之前，不得让旧 vault 继续自动消费旧基线；生产升级路径见 §4。
2. `validation_version=2` 的 proto/relay 联动、receipt HMAC、relay GC 版本门禁、消费者拒绝修订 1：**已由 task-122 完成**（完整预算框架未做），见 [`docs/notes/task-122-validation-v2.md`](../notes/task-122-validation-v2.md)。
3. **不声称 Byzantine 日志完整性**：根认证只能证明「声明为 ASSET 的 seq 有真实密文」，不能证明该 seq 原本不是 UPDATE。relay 仍可隐藏/替换事件；Merkle/inclusion 后置。
4. Linux 本地身份封装是 ADR 0008 的**开发/验证实现**：0600 裸主钥 + XChaCha20-Poly1305，安全弱于 Windows DPAPI，不宣称等价；Windows 生产仍走原始 DPAPI。

## 4. 已知后置/缺陷（事实性说明）

- **task-121（relay reservation 复用）**：轮换期重加密的资产挂在 rotation reservation 上；之后普通 checkpoint 用 checkpoint reservation 重传同一 `object_id`/同字节会被 relay `PUT parts` 判 `409 ID_CONFLICT`（`crates/relay/src/objects.rs:495-501`）。two-round e2e 用 `EmptyAssets` 规避（资产覆盖由激活 pair 承担）。修复方向：同字节跨 reservation 幂等复用且不重复计费，或客户端跳过已存在对象。
- ~~**task-122（验证修订 2）**~~ **已完成（2026-11-19）**：`CheckpointCommit.validation_version=2` + proto/relay 编解码 + 所有消费者拒绝修订 1；receipt HMAC；relay GC 版本门禁。**完整预算框架明确未做**；详见 [`docs/notes/task-122-validation-v2.md`](../notes/task-122-validation-v2.md)。
- `try_record_coverage_receipt` 仍有静默早退（仅浪费 lease/配额，不产生假覆盖）；coverage/verified receipt meta 无清理路径，长期 vault 会增长（ADR 0006 §36.4 已记）。
- Windows 目录 fsync 明确 `PlatformUnsupported`（只声明进程 kill，不声明硬掉电）。
- `RestartBaselineJournal` 明文 meta、与 RotationJournal 非原子（bind/fail-closed）。
- app/生产 `ControlPin` 构造点未落地；非 localhost 消费前必须接持久 pin。
- ASSET 换设备签名种子后旧 skipped-asset HMAC 不可验证 → 需重新 bootstrap。
- 同 epoch orphan 物理 GC、Merkle inclusion、历史/回收站/UI/MCP/生产运维后置。
- 历史/回收站：`docs/briefs/0006-history-and-trash.md`，任务 `task-115/116/117`。
- `scripts/e2e_rotation_two_rounds.ps1` / `e2e_rotation_verify.ps1` / `e2e_rotation_two_devices.ps1` 均已删除（被自包含 example 取代，见 §2.4）；`e2e_localhost` / `e2e_rotation` 仍保留 `--relay/--setup` 外部模式。

## 5. 验证命令

Linux（本机已实际执行）：

```bash
cargo fmt --all
cargo clippy --workspace --exclude menghunote-app --all-targets -- -D warnings
cargo test --workspace --exclude menghunote-app        # 1006 passed / 0 failed
cargo test -p menghunote-sync --all-targets           # 417 passed / 0 failed
cargo run -p menghunote-sync --example e2e_rotation_two_rounds   # TWO_ROUND_EXIT=0
```

Windows（**已由用户于 2026-11-13 在 `5538344` 包实测通过**；以下命令可复跑。tag 后 e2e 已去 PowerShell 化）：

```powershell
cargo test --all
cargo run -p menghunote-sync --example e2e_rotation_verify        # VERIFY_EXIT=0
cargo run -p menghunote-sync --example e2e_rotation_two_devices   # TWO_DEVICE_EXIT=0
cargo run -p menghunote-sync --example e2e_rotation_two_rounds    # TWO_ROUND_EXIT=0
```

不需要 PowerShell，也不需要外部 `dev_server`；`e2e_localhost` / `e2e_rotation` 仍保留
`--relay/--setup` 外部模式（需自行起 relay）。

Windows 重点：DPAPI identity 往返与跨 SID 失败语义、`dpapi.rs` 零改动编译、rustls native certs、MSVC `D8050/LNK1104`、ps1 的 `KEY_*` needle。

## 6. 关键文件/函数

- `crates/sync/src/identity_wrap.rs`：平台门面（Windows DPAPI / Linux `MNKW` + 用户主钥，ADR 0008）。
- `crates/sync/src/checkpoint.rs`：`prepare`（pending 候选）/ `validate` / `ensure_coverage_proven` / `verify_coverage_with_receipt` / `coverage_root` / durable v3。
- `crates/sync/src/upload.rs`：`fetch_verified_asset`（共享实证验证器）、`fetch_object`。
- `crates/sync/src/engine.rs`：`pull` / `persist_skipped_asset` v3 / `RelayHistoryRewritten`。
- `crates/sync/src/rotation_apply.rs`：`AuthenticatedCheckpoint` / `verify_checkpoint_authorization` / `verify_activation_t_gate` / I 阶段入口。
- `crates/sync/src/rotation_stage.rs` / `rotation_restart.rs`：`coverage_root`、restart 基线、verified receipt 门禁。
- `crates/dev-relay/src/lib.rs`（原 `crates/sync/examples/dev_relay.rs`，DEV ONLY、仅 `[dev-dependencies]`）+ `crates/sync/examples/e2e_rotation_two_rounds.rs`：自包含 T13 e2e。
- `crates/sync/examples/e2e_rotation_verify.rs`（调用 `e2e_localhost.rs` / `e2e_rotation.rs` 的 `run` 入口）+ `e2e_rotation_two_devices.rs`：tag 后迁移的三条自包含 e2e。
- `crates/relay/src/{checkpoints,rotations,quota,control,log}.rs`；ADR 0006 §31–§36；ADR 0008。

## 7. 开发陷阱

- Linux 主钥路径 `$XDG_DATA_HOME/menghunote/local-identity/key-v1.bin`（0600，目录 0700，ADR 0008）；测试用 `target/tmp/xdg-<pid>` 并调用生产 `initialize_user_key`。删钥=身份不可解，**不会**自动重建。
- two-round e2e 是进程内 relay：`FakeClock` 推进时必须同步客户端 `now` override；外部 `--relay` 模式没有假时钟/GC，会打印 `TWO_ROUND_EXIT=2 (SKIP)`，**不算通过**。
- 显式 `--dir` 必须是空目录（guard 在 genesis/写任何文件之前）；设备身份实际在 `device-a|device-b/identity.bin`，只查顶层 `identity.bin` 会漏过并覆盖旧身份。
- 三条 e2e 都用进程内 relay；`e2e_rotation_verify` 的两阶段必须共享同一 relay DB/vault，不要拆成两次运行（vault 会走重启分支，relay 库也对不上）。`--relay` 外部模式没有假时钟/GC，只作兼容入口。
- 测试临时目录用 `CARGO_TARGET_TMPDIR`；禁止用 sleep 当收敛判据（用 `DurableAck`/明确 phase）。
- 长驻 exe 不要 `cargo run`；先复制到 `target/tmp`，避免 LNK1104/D8050（Windows）。
- 旧 vault/旧签名 checkpoint 不在 v0.4 支持范围（§3）。
- 禁止 `--amend`/改写历史；并发 agent 不要同文件；`git add/commit` 前先 `git status`（见 `docs/notes/commit-attribution.md`）。

## 8. 下一步顺序

1. ~~GPT 收尾复审~~（已完成：APPROVE，tag `v0.4-sync`；整理前锚点 `321f732`，等价新提交 `61d8de4`）。
2. Windows 实机按 §5 复跑，把结果补进本文。
3. ~~`task-121`（relay reservation 复用）/ `task-122`（验证修订 2 与 receipt HMAC、GC 版本门禁）~~ **均已完成**。
4. `task-123`（默认「仅删凭证」吊销端点 + 客户端/UI；轮换改为显式 opt-in）/ `task-124`（metadata 事件认证对账与 cursor 续读）。
4. 历史/回收站 `task-115/116/117` 按 `docs/briefs/0006-history-and-trash.md` 排期。

`````


---

# 附录 B：完整源码/测试



@@SECTION code/057 `crates/sync-proto/src/activation_t.rs`


`````rust
//! 评审 0002 P1-1.4 / ADR 0006 §23.2、§27.2：完整 activation T 记录（`ActivationTV1`）。
//!
//! 九字段 `ManagerTransitionV1`（relay `rotations::ManagerTransition`）只保护
//! “管理权从旧 key 转到新 key”这一事实；它**不能**替代完整 activation 授权对象，
//! 否则旧管理私钥退役后，仍可能用一份合法的 new-key 转换签名去拼接**另一次**
//! activation 的基线/槽/证书/grant/资产材料。本记录把这次 activation 真正消费的
//! 全部材料收进同一份 canonical CDE，并要求旧、新两把管理 key 对
//!
//! ```text
//! MN/v1/activation-t\0 || CDE(ActivationTV1)
//! ```
//!
//! 双签（域分隔符是一个**真实的 0x00 字节**，与 `MN/v1/manage\0` 等域互不复用）。
//!
//! # 字段表（整数键、不可重排、不可增删；golden vector 锁定）
//!
//! | key | 字段 | wire 类型 | 语义 |
//! | --- | --- | --- | --- |
//! | 0 | `vault_id` | 16B | 库标识 |
//! | 1 | `instance_id` | 16B | relay 实例标识 |
//! | 2 | `rotation_id` | 16B | 本次轮换标识 |
//! | 3 | `from_epoch` | uint | 旧 active epoch |
//! | 4 | `to_epoch` | uint | 待激活候选 epoch |
//! | 5 | `freeze_seq` | uint | 冻结截止 S（= `rotation.freeze_seq`） |
//! | 6 | `old_manager_rev` | uint | 旧管理 key rev |
//! | 7 | `new_manager_rev` | uint | 新管理 key rev（v1 恒为旧 rev + 1） |
//! | 8 | `new_manager_pubkey` | 32B | 新管理 Ed25519 公钥 |
//! | 9 | `prev_control_hash` | 32B | 激活前控制链链尾 `record_hash`（pin，防分叉拼接） |
//! | 10 | `expected_control_rev` | uint | 期望的激活后控制链 rev（CAS） |
//! | 11 | `primary_cid` | 16B | 主基线 checkpoint id |
//! | 12 | `primary_commit_digest` | 32B | 主基线 `CheckpointCommit` 摘要 |
//! | 13 | `fallback_cid` | 16B | 备基线 checkpoint id |
//! | 14 | `fallback_commit_digest` | 32B | 备基线 `CheckpointCommit` 摘要 |
//! | 15 | `slot_set_digest` | 32B | 槽集合 digest（`slot_set_digest` 口径） |
//! | 16 | `member_cert_digest` | 32B | 成员证书集合 digest（`member_cert_digest` 口径） |
//! | 17 | `grants_digest` | 32B | **完整** grants 包 digest（`grant_package_digest` 口径） |
//! | 18 | `old_asset_set_digest` | 32B | 旧 epoch 资产集合 digest |
//! | 19 | `new_asset_set_digest` | 32B | 新 epoch 资产集合 digest |
//!
//! 本模块只做 canonical 编解码与校验，不做 SHA-256、不碰 IO/密钥/存储。

use crate::cde::{
    decode, encode, expect_bytes_n, expect_i64_max, expect_u32, into_record_fields, CdeValue,
};
use crate::constants::{signature_input, SIGN_ACTIVATION_T_DOMAIN};
use crate::error::ProtoError;
use crate::records::{CheckpointId, Ed25519PublicKey, RelayInstanceId, Sha256, VaultId};

/// 字段个数；键必须恰好是 `0..20`，不得增删或重排。
pub const ACTIVATION_T_FIELD_COUNT: usize = 20;

/// 完整 activation T：新旧管理 key 双签的授权对象（评审 0002 P1-1.4）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivationTV1 {
    pub vault_id: VaultId,
    pub instance_id: RelayInstanceId,
    pub rotation_id: [u8; 16],
    pub from_epoch: u64,
    pub to_epoch: u64,
    /// 冻结截止 S；必须等于 `rotation.freeze_seq`。
    pub freeze_seq: u64,
    pub old_manager_rev: u32,
    /// v1 激活后恒为 `old_manager_rev + 1`（relay `bind` 同口径）。
    pub new_manager_rev: u32,
    pub new_manager_pubkey: Ed25519PublicKey,
    /// 激活前控制链链尾 `record_hash`；把 activation 钉在一条确定的控制链上。
    pub prev_control_hash: Sha256,
    pub expected_control_rev: u64,
    pub primary_cid: CheckpointId,
    pub primary_commit_digest: Sha256,
    pub fallback_cid: CheckpointId,
    pub fallback_commit_digest: Sha256,
    pub slot_set_digest: Sha256,
    pub member_cert_digest: Sha256,
    pub grants_digest: Sha256,
    pub old_asset_set_digest: Sha256,
    pub new_asset_set_digest: Sha256,
}

impl ActivationTV1 {
    /// 字段未全零、长度正确、u64 不超 SQLite INTEGER 上界、epoch/rev 单调。
    ///
    /// 刻意**不**要求 `to_epoch == from_epoch + 1`：restart 会保留原 from 并分配更高的
    /// 已废弃候选之后的新 epoch（ADR 0006 §33），因此只要求 `to_epoch > from_epoch`。
    /// 也不单独要求 `freeze_seq > 0`：空库首次激活时 S 可以为 0。
    pub fn validate(&self) -> Result<(), ProtoError> {
        for (name, value) in [
            ("ActivationTV1.vault_id", &self.vault_id[..]),
            ("ActivationTV1.instance_id", &self.instance_id[..]),
            ("ActivationTV1.rotation_id", &self.rotation_id[..]),
            (
                "ActivationTV1.new_manager_pubkey",
                &self.new_manager_pubkey[..],
            ),
            (
                "ActivationTV1.prev_control_hash",
                &self.prev_control_hash[..],
            ),
            ("ActivationTV1.primary_cid", &self.primary_cid[..]),
            (
                "ActivationTV1.primary_commit_digest",
                &self.primary_commit_digest[..],
            ),
            ("ActivationTV1.fallback_cid", &self.fallback_cid[..]),
            (
                "ActivationTV1.fallback_commit_digest",
                &self.fallback_commit_digest[..],
            ),
            ("ActivationTV1.slot_set_digest", &self.slot_set_digest[..]),
            (
                "ActivationTV1.member_cert_digest",
                &self.member_cert_digest[..],
            ),
            ("ActivationTV1.grants_digest", &self.grants_digest[..]),
            (
                "ActivationTV1.old_asset_set_digest",
                &self.old_asset_set_digest[..],
            ),
            (
                "ActivationTV1.new_asset_set_digest",
                &self.new_asset_set_digest[..],
            ),
        ] {
            if value.iter().all(|byte| *byte == 0) {
                return Err(ProtoError::Invalid(format!("{name} 不能全零")));
            }
        }

        for (name, value) in [
            ("ActivationTV1.from_epoch", self.from_epoch),
            ("ActivationTV1.to_epoch", self.to_epoch),
            ("ActivationTV1.freeze_seq", self.freeze_seq),
            (
                "ActivationTV1.expected_control_rev",
                self.expected_control_rev,
            ),
        ] {
            if value > i64::MAX as u64 {
                return Err(ProtoError::Invalid(format!("{name} 超过 i64::MAX")));
            }
        }

        if self.from_epoch == 0 {
            return Err(ProtoError::Invalid(
                "ActivationTV1.from_epoch 必须 >0".to_owned(),
            ));
        }
        if self.to_epoch <= self.from_epoch {
            return Err(ProtoError::Invalid(format!(
                "ActivationTV1.to_epoch={} 必须大于 from_epoch={}",
                self.to_epoch, self.from_epoch
            )));
        }
        if self.old_manager_rev == 0 {
            return Err(ProtoError::Invalid(
                "ActivationTV1.old_manager_rev 必须 >0".to_owned(),
            ));
        }
        let Some(expected_new_manager_rev) = self.old_manager_rev.checked_add(1) else {
            return Err(ProtoError::Invalid(format!(
                "ActivationTV1.old_manager_rev={} 无法加 1",
                self.old_manager_rev
            )));
        };
        if self.new_manager_rev != expected_new_manager_rev {
            return Err(ProtoError::Invalid(format!(
                "ActivationTV1.new_manager_rev={} 必须等于 old_manager_rev+1={expected_new_manager_rev}",
                self.new_manager_rev
            )));
        }
        if self.expected_control_rev == 0 {
            return Err(ProtoError::Invalid(
                "ActivationTV1.expected_control_rev 必须 >0".to_owned(),
            ));
        }
        Ok(())
    }

    /// 映射到整数键 map（键 `0..20`）。
    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.validate()?;
        Ok(CdeValue::Map(vec![
            (0, CdeValue::Bytes(self.vault_id.to_vec())),
            (1, CdeValue::Bytes(self.instance_id.to_vec())),
            (2, CdeValue::Bytes(self.rotation_id.to_vec())),
            (3, CdeValue::UInt(self.from_epoch)),
            (4, CdeValue::UInt(self.to_epoch)),
            (5, CdeValue::UInt(self.freeze_seq)),
            (6, CdeValue::UInt(u64::from(self.old_manager_rev))),
            (7, CdeValue::UInt(u64::from(self.new_manager_rev))),
            (8, CdeValue::Bytes(self.new_manager_pubkey.to_vec())),
            (9, CdeValue::Bytes(self.prev_control_hash.to_vec())),
            (10, CdeValue::UInt(self.expected_control_rev)),
            (11, CdeValue::Bytes(self.primary_cid.to_vec())),
            (12, CdeValue::Bytes(self.primary_commit_digest.to_vec())),
            (13, CdeValue::Bytes(self.fallback_cid.to_vec())),
            (14, CdeValue::Bytes(self.fallback_commit_digest.to_vec())),
            (15, CdeValue::Bytes(self.slot_set_digest.to_vec())),
            (16, CdeValue::Bytes(self.member_cert_digest.to_vec())),
            (17, CdeValue::Bytes(self.grants_digest.to_vec())),
            (18, CdeValue::Bytes(self.old_asset_set_digest.to_vec())),
            (19, CdeValue::Bytes(self.new_asset_set_digest.to_vec())),
        ]))
    }

    /// 从 CDE map 解码；字段数必须恰好 20，键必须是 `0..20` 的连续升序。
    pub fn from_cde(value: CdeValue) -> Result<Self, ProtoError> {
        let fields = into_record_fields(value, ACTIVATION_T_FIELD_COUNT)?;
        let record = Self {
            vault_id: expect_bytes_n::<16>(fields[0].clone(), "ActivationTV1.vault_id")?,
            instance_id: expect_bytes_n::<16>(fields[1].clone(), "ActivationTV1.instance_id")?,
            rotation_id: expect_bytes_n::<16>(fields[2].clone(), "ActivationTV1.rotation_id")?,
            from_epoch: expect_i64_max(fields[3].clone(), "ActivationTV1.from_epoch")?,
            to_epoch: expect_i64_max(fields[4].clone(), "ActivationTV1.to_epoch")?,
            freeze_seq: expect_i64_max(fields[5].clone(), "ActivationTV1.freeze_seq")?,
            old_manager_rev: expect_u32(fields[6].clone(), "ActivationTV1.old_manager_rev")?,
            new_manager_rev: expect_u32(fields[7].clone(), "ActivationTV1.new_manager_rev")?,
            new_manager_pubkey: expect_bytes_n::<32>(
                fields[8].clone(),
                "ActivationTV1.new_manager_pubkey",
            )?,
            prev_control_hash: expect_bytes_n::<32>(
                fields[9].clone(),
                "ActivationTV1.prev_control_hash",
            )?,
            expected_control_rev: expect_i64_max(
                fields[10].clone(),
                "ActivationTV1.expected_control_rev",
            )?,
            primary_cid: expect_bytes_n::<16>(fields[11].clone(), "ActivationTV1.primary_cid")?,
            primary_commit_digest: expect_bytes_n::<32>(
                fields[12].clone(),
                "ActivationTV1.primary_commit_digest",
            )?,
            fallback_cid: expect_bytes_n::<16>(fields[13].clone(), "ActivationTV1.fallback_cid")?,
            fallback_commit_digest: expect_bytes_n::<32>(
                fields[14].clone(),
                "ActivationTV1.fallback_commit_digest",
            )?,
            slot_set_digest: expect_bytes_n::<32>(
                fields[15].clone(),
                "ActivationTV1.slot_set_digest",
            )?,
            member_cert_digest: expect_bytes_n::<32>(
                fields[16].clone(),
                "ActivationTV1.member_cert_digest",
            )?,
            grants_digest: expect_bytes_n::<32>(fields[17].clone(), "ActivationTV1.grants_digest")?,
            old_asset_set_digest: expect_bytes_n::<32>(
                fields[18].clone(),
                "ActivationTV1.old_asset_set_digest",
            )?,
            new_asset_set_digest: expect_bytes_n::<32>(
                fields[19].clone(),
                "ActivationTV1.new_asset_set_digest",
            )?,
        };
        record.validate()?;
        Ok(record)
    }

    /// canonical 编码（唯一字节串）。
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    /// canonical 解码；拒绝未知键、缺键、错序、错误长度与尾部多余字节。
    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        Self::from_cde(decode(bytes)?)
    }

    /// `MN/v1/activation-t\0 || CDE(self)`。
    pub fn signing_input(&self) -> Result<Vec<u8>, ProtoError> {
        activation_t_signing_input(self)
    }
}

/// 完整 activation T 的双签输入：`MN/v1/activation-t\0 || CDE(T)`。
///
/// 专用域，不与 `MN/v1/manage\0` / `MN/v1/device-cert\0` / `MN/v1/checkpoint\0`
/// 等任何既有签名域复用（AAD/域分离是硬约束，见 ADR 0006 §14）。
pub fn activation_t_signing_input(record: &ActivationTV1) -> Result<Vec<u8>, ProtoError> {
    Ok(signature_input(SIGN_ACTIVATION_T_DOMAIN, &record.encode()?))
}

`````



@@SECTION code/058 `crates/sync-proto/src/cde.rs`


`````rust
//! 确定性 CBOR（CDE，RFC 8949）最小实现。
//!
//! 只接受 v1 用到的类型：uint、byte string、text、array、map（uint 键）、bool。
//! 禁止：浮点、null、undefined、tag、不定长、重复键、乱序键、非最短整数。
//!
//! map 的键在编码时按整数升序输出；解码时要求严格升序，因此同一语义值只有
//! 一种字节串。

use crate::error::ProtoError;

/// 单次 CDE 解码/编码的硬上限（与 ADR 0006 的 64MiB 对象上限一致）。
pub const MAX_CDE_BYTES: usize = 64 * 1024 * 1024;
/// 嵌套深度上限，防止恶意输入打爆栈。
pub const MAX_CDE_DEPTH: usize = 64;
/// 单层 array/map 项数上限；v1 最大的数组是 manifest refs（≤1024）。
pub const MAX_CDE_CONTAINER_ITEMS: usize = 65_536;

/// CDE 值。记录类型在 `records` 中映射到它。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CdeValue {
    UInt(u64),
    Bytes(Vec<u8>),
    Text(String),
    Array(Vec<CdeValue>),
    /// 整数键 map；编码时按 key 升序，解码时拒绝乱序/重复。
    Map(Vec<(u64, CdeValue)>),
    Bool(bool),
}

impl CdeValue {
    pub fn uint(value: u64) -> Self {
        Self::UInt(value)
    }

    pub fn bytes(value: impl Into<Vec<u8>>) -> Self {
        Self::Bytes(value.into())
    }

    pub fn text(value: impl Into<String>) -> Self {
        Self::Text(value.into())
    }

    pub fn array(value: Vec<CdeValue>) -> Self {
        Self::Array(value)
    }

    pub fn map(value: Vec<(u64, CdeValue)>) -> Self {
        Self::Map(value)
    }

    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(self)
    }
}

/// canonical 编码。map 会排序并拒绝重复键。
pub fn encode(value: &CdeValue) -> Result<Vec<u8>, ProtoError> {
    let mut out = Vec::new();
    write_value(value, &mut out, 0)?;
    if out.len() > MAX_CDE_BYTES {
        return Err(ProtoError::TooLarge);
    }
    Ok(out)
}

/// canonical 解码；必须恰好消费全部输入。
pub fn decode(bytes: &[u8]) -> Result<CdeValue, ProtoError> {
    if bytes.len() > MAX_CDE_BYTES {
        return Err(ProtoError::TooLarge);
    }
    let (value, used) = read_value(bytes, 0, 0)?;
    if used != bytes.len() {
        return Err(ProtoError::NonCanonical(format!(
            "尾部还有 {} 字节未消费",
            bytes.len() - used
        )));
    }
    Ok(value)
}

fn write_value(value: &CdeValue, out: &mut Vec<u8>, depth: usize) -> Result<(), ProtoError> {
    if depth > MAX_CDE_DEPTH {
        return Err(ProtoError::Invalid("CDE 嵌套过深".to_owned()));
    }
    match value {
        CdeValue::UInt(value) => write_head(0, *value, out),
        CdeValue::Bytes(bytes) => {
            write_head(2, bytes.len() as u64, out);
            out.extend_from_slice(bytes);
        }
        CdeValue::Text(text) => {
            write_head(3, text.len() as u64, out);
            out.extend_from_slice(text.as_bytes());
        }
        CdeValue::Array(items) => {
            if items.len() > MAX_CDE_CONTAINER_ITEMS {
                return Err(ProtoError::TooLarge);
            }
            write_head(4, items.len() as u64, out);
            for item in items {
                write_value(item, out, depth + 1)?;
            }
        }
        CdeValue::Map(entries) => {
            if entries.len() > MAX_CDE_CONTAINER_ITEMS {
                return Err(ProtoError::TooLarge);
            }
            let mut sorted = entries.clone();
            sorted.sort_by_key(|(key, _)| *key);
            if sorted.windows(2).any(|pair| pair[0].0 == pair[1].0) {
                return Err(ProtoError::NonCanonical("map 含重复键".to_owned()));
            }
            write_head(5, sorted.len() as u64, out);
            for (key, value) in sorted {
                write_head(0, key, out);
                write_value(&value, out, depth + 1)?;
            }
        }
        CdeValue::Bool(value) => out.push(if *value { 0xf5 } else { 0xf4 }),
    }
    Ok(())
}

fn write_head(major: u8, value: u64, out: &mut Vec<u8>) {
    let major = major << 5;
    if value < 24 {
        out.push(major | value as u8);
    } else if value <= u8::MAX as u64 {
        out.push(major | 24);
        out.push(value as u8);
    } else if value <= u16::MAX as u64 {
        out.push(major | 25);
        out.extend_from_slice(&(value as u16).to_be_bytes());
    } else if value <= u32::MAX as u64 {
        out.push(major | 26);
        out.extend_from_slice(&(value as u32).to_be_bytes());
    } else {
        out.push(major | 27);
        out.extend_from_slice(&value.to_be_bytes());
    }
}

fn read_value(bytes: &[u8], offset: usize, depth: usize) -> Result<(CdeValue, usize), ProtoError> {
    if depth > MAX_CDE_DEPTH {
        return Err(ProtoError::Invalid("CDE 嵌套过深".to_owned()));
    }
    let (major, argument, mut position) = read_head(bytes, offset)?;

    let value = match major {
        0 => CdeValue::UInt(argument),
        2 => {
            let length = usize_of(argument)?;
            let end = position.checked_add(length).ok_or(ProtoError::Truncated)?;
            if end > bytes.len() {
                return Err(ProtoError::Truncated);
            }
            let value = CdeValue::Bytes(bytes[position..end].to_vec());
            position = end;
            value
        }
        3 => {
            let length = usize_of(argument)?;
            let end = position.checked_add(length).ok_or(ProtoError::Truncated)?;
            if end > bytes.len() {
                return Err(ProtoError::Truncated);
            }
            let text = std::str::from_utf8(&bytes[position..end])
                .map_err(|_| ProtoError::Invalid("text 不是合法 UTF-8".to_owned()))?;
            position = end;
            CdeValue::Text(text.to_owned())
        }
        4 => {
            let length = usize_of(argument)?;
            if length > MAX_CDE_CONTAINER_ITEMS || length > bytes.len() {
                return Err(ProtoError::TooLarge);
            }
            let mut items = Vec::with_capacity(length);
            for _ in 0..length {
                let (item, used) = read_value(bytes, position, depth + 1)?;
                items.push(item);
                position = used;
            }
            CdeValue::Array(items)
        }
        5 => {
            let length = usize_of(argument)?;
            if length > MAX_CDE_CONTAINER_ITEMS || length > bytes.len() {
                return Err(ProtoError::TooLarge);
            }
            let mut entries = Vec::with_capacity(length);
            let mut previous = None;
            for _ in 0..length {
                let (key, used) = read_value(bytes, position, depth + 1)?;
                let CdeValue::UInt(key) = key else {
                    return Err(ProtoError::NonCanonical("map 的键必须是整数".to_owned()));
                };
                if let Some(previous) = previous {
                    if key <= previous {
                        return Err(ProtoError::NonCanonical("map 的键必须严格升序".to_owned()));
                    }
                }
                previous = Some(key);
                position = used;
                let (value, used) = read_value(bytes, position, depth + 1)?;
                entries.push((key, value));
                position = used;
            }
            CdeValue::Map(entries)
        }
        7 => match argument {
            20 => CdeValue::Bool(false),
            21 => CdeValue::Bool(true),
            _ => {
                return Err(ProtoError::NonCanonical(
                    "只允许 bool，拒绝 null/undefined/float/simple".to_owned(),
                ))
            }
        },
        _ => {
            return Err(ProtoError::NonCanonical(format!(
                "不允许的 CBOR major type {major}"
            )))
        }
    };
    Ok((value, position))
}

fn read_head(bytes: &[u8], offset: usize) -> Result<(u8, u64, usize), ProtoError> {
    let first = *bytes.get(offset).ok_or(ProtoError::Truncated)?;
    let major = first >> 5;
    let additional = first & 0x1f;
    let (argument, position) = match additional {
        0..=23 => (u64::from(additional), offset + 1),
        24 => {
            let value = u64::from(*bytes.get(offset + 1).ok_or(ProtoError::Truncated)?);
            if value < 24 {
                return Err(ProtoError::NonCanonical("整数未用最短形式".to_owned()));
            }
            (value, offset + 2)
        }
        25 => {
            let slice = bytes
                .get(offset + 1..offset + 3)
                .ok_or(ProtoError::Truncated)?;
            let value = u64::from(u16::from_be_bytes([slice[0], slice[1]]));
            if value <= u8::MAX as u64 {
                return Err(ProtoError::NonCanonical("整数未用最短形式".to_owned()));
            }
            (value, offset + 3)
        }
        26 => {
            let slice = bytes
                .get(offset + 1..offset + 5)
                .ok_or(ProtoError::Truncated)?;
            let value = u64::from(u32::from_be_bytes([slice[0], slice[1], slice[2], slice[3]]));
            if value <= u16::MAX as u64 {
                return Err(ProtoError::NonCanonical("整数未用最短形式".to_owned()));
            }
            (value, offset + 5)
        }
        27 => {
            let slice = bytes
                .get(offset + 1..offset + 9)
                .ok_or(ProtoError::Truncated)?;
            let value = u64::from_be_bytes([
                slice[0], slice[1], slice[2], slice[3], slice[4], slice[5], slice[6], slice[7],
            ]);
            if value <= u32::MAX as u64 {
                return Err(ProtoError::NonCanonical("整数未用最短形式".to_owned()));
            }
            (value, offset + 9)
        }
        _ => {
            return Err(ProtoError::NonCanonical(
                "不允许不定长/保留的 additional info".to_owned(),
            ))
        }
    };
    Ok((major, argument, position))
}

fn usize_of(value: u64) -> Result<usize, ProtoError> {
    usize::try_from(value).map_err(|_| ProtoError::TooLarge)
}

/// 把 CDE 值按“恰好 n 个连续整数键”的 record map 拆开。
pub(crate) fn into_record_fields(
    value: CdeValue,
    expected_count: usize,
) -> Result<Vec<CdeValue>, ProtoError> {
    let CdeValue::Map(entries) = value else {
        return Err(ProtoError::Invalid("record 必须是 map".to_owned()));
    };
    if entries.len() != expected_count {
        return Err(ProtoError::Invalid(format!(
            "record 字段数 {}，期望 {expected_count}",
            entries.len()
        )));
    }
    let mut fields = Vec::with_capacity(expected_count);
    for (expected, (key, value)) in entries.into_iter().enumerate() {
        let expected = expected as u64;
        if key != expected {
            if key > expected && key < expected_count as u64 {
                return Err(ProtoError::MissingField(expected));
            }
            return Err(ProtoError::UnknownField(key));
        }
        fields.push(value);
    }
    Ok(fields)
}

pub(crate) fn expect_uint(value: CdeValue, field: &str) -> Result<u64, ProtoError> {
    match value {
        CdeValue::UInt(value) => Ok(value),
        other => Err(ProtoError::Invalid(format!(
            "{field} 必须是 uint，实际 {other:?}"
        ))),
    }
}

pub(crate) fn expect_u32(value: CdeValue, field: &str) -> Result<u32, ProtoError> {
    let value = expect_uint(value, field)?;
    u32::try_from(value).map_err(|_| ProtoError::Invalid(format!("{field} 超出 u32")))
}

pub(crate) fn expect_u16(value: CdeValue, field: &str) -> Result<u16, ProtoError> {
    let value = expect_uint(value, field)?;
    u16::try_from(value).map_err(|_| ProtoError::Invalid(format!("{field} 超出 u16")))
}

pub(crate) fn expect_u8(value: CdeValue, field: &str) -> Result<u8, ProtoError> {
    let value = expect_uint(value, field)?;
    u8::try_from(value).map_err(|_| ProtoError::Invalid(format!("{field} 超出 u8 标签范围")))
}

/// 协议内所有 u64 逻辑值必须 ≤ i64::MAX（SQLite INTEGER）。
pub(crate) fn expect_i64_max(value: CdeValue, field: &str) -> Result<u64, ProtoError> {
    let value = expect_uint(value, field)?;
    if value > i64::MAX as u64 {
        return Err(ProtoError::Invalid(format!("{field} 超过 i64::MAX")));
    }
    Ok(value)
}

pub(crate) fn expect_bool(value: CdeValue, field: &str) -> Result<bool, ProtoError> {
    match value {
        CdeValue::Bool(value) => Ok(value),
        other => Err(ProtoError::Invalid(format!(
            "{field} 必须是 bool，实际 {other:?}"
        ))),
    }
}

pub(crate) fn expect_bytes(value: CdeValue, field: &str) -> Result<Vec<u8>, ProtoError> {
    match value {
        CdeValue::Bytes(value) => Ok(value),
        other => Err(ProtoError::Invalid(format!(
            "{field} 必须是 byte string，实际 {other:?}"
        ))),
    }
}

pub(crate) fn expect_text(value: CdeValue, field: &str) -> Result<String, ProtoError> {
    match value {
        CdeValue::Text(value) => Ok(value),
        other => Err(ProtoError::Invalid(format!(
            "{field} 必须是 text，实际 {other:?}"
        ))),
    }
}

pub(crate) fn expect_array(value: CdeValue, field: &str) -> Result<Vec<CdeValue>, ProtoError> {
    match value {
        CdeValue::Array(value) => Ok(value),
        other => Err(ProtoError::Invalid(format!(
            "{field} 必须是 array，实际 {other:?}"
        ))),
    }
}

pub(crate) fn expect_bytes_n<const N: usize>(
    value: CdeValue,
    field: &str,
) -> Result<[u8; N], ProtoError> {
    let bytes = expect_bytes(value, field)?;
    bytes.try_into().map_err(|bytes: Vec<u8>| {
        ProtoError::Invalid(format!("{field} 长度应为 {N}，实际 {}", bytes.len()))
    })
}

`````



@@SECTION code/059 `crates/sync-proto/src/constants.rs`


`````rust
//! ADR 0006 §2/§3/§4/§14 的 HKDF info、签名域与待哈希输入。
//!
//! 域分隔符是**一个真实的 0x00 字节**（源码中的 `\0`），不是两个 ASCII 字符。

use crate::error::ProtoError;
use crate::records::{Challenge, CheckpointCommit, HttpMethod, ObjectCommit, ObjectType};

/// 数据对象 HKDF：`MN/v1/data\0`。
pub const HKDF_DATA_INFO_PREFIX: &[u8] = b"MN/v1/data\0";
/// 资产对象名 HKDF：`MN/v1/asset-id\0`。
pub const HKDF_ASSET_ID_INFO: &[u8] = b"MN/v1/asset-id\0";
/// 口令槽包装 HKDF 前缀。
pub const HKDF_PASSWORD_WRAP_PREFIX: &[u8] = b"MN/v1/password-wrap\0";
/// 恢复码槽包装 HKDF 前缀。
pub const HKDF_RECOVERY_WRAP_PREFIX: &[u8] = b"MN/v1/recovery-wrap\0";
/// 管理 Ed25519 种子 HKDF info。
pub const HKDF_MANAGEMENT_ED25519_INFO: &[u8] = b"MN/v1/management-ed25519\0";
/// HPKE info。
pub const HPKE_INFO: &[u8] = b"MN/v1/epoch-grant\0";
/// DPAPI entropy 前缀。
pub const DPAPI_ENTROPY_PREFIX: &[u8] = b"MN/dpapi/v1\0";
/// 设备 challenge 签名域。
pub const SIGN_AUTH_DOMAIN: &[u8] = b"MN/v1/auth\0";
/// 管理写签名域。
pub const SIGN_MANAGE_DOMAIN: &[u8] = b"MN/v1/manage\0";
/// ObjectCommit 签名域。
pub const SIGN_OBJECT_COMMIT_DOMAIN: &[u8] = b"MN/v1/object-commit\0";
/// DeviceCertV1 管理签名域（§17.1）。
pub const SIGN_DEVICE_CERT_DOMAIN: &[u8] = b"MN/v1/device-cert\0";
/// CheckpointCommit 签名域。
pub const SIGN_CHECKPOINT_DOMAIN: &[u8] = b"MN/v1/checkpoint\0";
/// 验证修订 2 的证据摘要域（`validation_digest` 的输入前缀；不是签名域）。
pub const CHECKPOINT_VALIDATION_DOMAIN: &[u8] = b"MN/v1/checkpoint-validation\0";
/// 当前生产 `CheckpointCommit.validation_version`（task-122 起）。
///
/// 旧修订仍能**解码**（便于识别并显式拒绝），但不得作为基线/GC 依据。
pub const CHECKPOINT_COMMIT_VALIDATION_VERSION: u16 = 2;
/// 完整 activation T 的双签域（评审 0002 P1-1.4）：只用于
/// `activation_t_signing_input`，不得与其它签名域复用。
pub const SIGN_ACTIVATION_T_DOMAIN: &[u8] = b"MN/v1/activation-t\0";
/// cursor HMAC 域（relay 使用）。
pub const CURSOR_HMAC_DOMAIN: &[u8] = b"MN/v1/cursor\0";

// —— ADR 0015：管理控制链 V2 的域（与 v1 域互不复用）——

/// V2 设备控制记录签名域。
pub const SIGN_V2_CONTROL_DOMAIN: &[u8] = b"MN/v2/control\0";
/// V2 旧 creator 升级签名域。
pub const SIGN_V2_UPGRADE_DOMAIN: &[u8] = b"MN/v2/upgrade\0";
/// V2 离线恢复根签名域。
pub const SIGN_V2_RECOVER_DOMAIN: &[u8] = b"MN/v2/recover\0";
/// V2 目标设备 PoP 签名域（`MN/v2/pop\0 || h || pk32`）。
pub const SIGN_V2_POP_DOMAIN: &[u8] = b"MN/v2/pop\0";
/// V2 鉴权签名域（token 请求签 `MN/v2/auth\0 || challenge`）。
pub const SIGN_AUTH_V2_DOMAIN: &[u8] = b"MN/v2/auth\0";
/// V2 epoch grant 的 HPKE info（ADR 0015：明文仅 epoch root）。
pub const HPKE_INFO_V2: &[u8] = b"MN/v2/epoch-grant\0";

/// `K_object` 的 HKDF info：`MN/v1/data\0 || type_u8 || epoch_BE64 || object_id`。
pub fn hkdf_data_info(
    object_type: ObjectType,
    key_epoch: u64,
    object_id: &[u8; 32],
) -> Result<Vec<u8>, ProtoError> {
    if key_epoch > i64::MAX as u64 {
        return Err(ProtoError::Invalid("key_epoch 超过 i64::MAX".to_owned()));
    }
    let mut out = Vec::with_capacity(HKDF_DATA_INFO_PREFIX.len() + 1 + 8 + 32);
    out.extend_from_slice(HKDF_DATA_INFO_PREFIX);
    out.push(object_type.as_u8());
    out.extend_from_slice(&key_epoch.to_be_bytes());
    out.extend_from_slice(object_id);
    Ok(out)
}

pub fn password_wrap_info(slot_id: &[u8; 32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HKDF_PASSWORD_WRAP_PREFIX.len() + 32);
    out.extend_from_slice(HKDF_PASSWORD_WRAP_PREFIX);
    out.extend_from_slice(slot_id);
    out
}

pub fn recovery_wrap_info(slot_id: &[u8; 32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HKDF_RECOVERY_WRAP_PREFIX.len() + 32);
    out.extend_from_slice(HKDF_RECOVERY_WRAP_PREFIX);
    out.extend_from_slice(slot_id);
    out
}

pub fn dpapi_entropy(vault_id: &[u8; 16], device_id: &[u8; 16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(DPAPI_ENTROPY_PREFIX.len() + 32);
    out.extend_from_slice(DPAPI_ENTROPY_PREFIX);
    out.extend_from_slice(vault_id);
    out.extend_from_slice(device_id);
    out
}

fn with_domain(domain: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(domain.len() + payload.len());
    out.extend_from_slice(domain);
    out.extend_from_slice(payload);
    out
}

/// 设备 challenge 的签名输入：`MN/v1/auth\0 || CDE(challenge)`。
pub fn auth_signing_input(challenge: &Challenge) -> Result<Vec<u8>, ProtoError> {
    Ok(with_domain(SIGN_AUTH_DOMAIN, &challenge.encode()?))
}

/// ObjectCommit 的签名输入：`MN/v1/object-commit\0 || CDE(ObjectCommit)`。
pub fn object_commit_signing_input(commit: &ObjectCommit) -> Result<Vec<u8>, ProtoError> {
    Ok(with_domain(SIGN_OBJECT_COMMIT_DOMAIN, &commit.encode()?))
}

/// CheckpointCommit 的签名输入：`MN/v1/checkpoint\0 || CDE(CheckpointCommit)`。
pub fn checkpoint_signing_input(commit: &CheckpointCommit) -> Result<Vec<u8>, ProtoError> {
    Ok(with_domain(SIGN_CHECKPOINT_DOMAIN, &commit.encode()?))
}

/// 验证修订 2 证据摘要的待哈希输入：
/// `MN/v1/checkpoint-validation\0 || CDE(CheckpointValidationV2)`。
///
/// 调用方做 `SHA256(本函数输出)` 后写入 `CheckpointCommit.validation_digest`。
pub fn checkpoint_validation_input(
    evidence: &crate::records::CheckpointValidationV2,
) -> Result<Vec<u8>, ProtoError> {
    Ok(with_domain(
        CHECKPOINT_VALIDATION_DOMAIN,
        &evidence.encode()?,
    ))
}

/// 管理写签名输入：`MN/v1/manage\0 || instance || vault || request_id ||
/// expected_control_rev_BE64 || SHA256(CDE{method,path,unsigned_body})`。
///
/// `body_digest` 由调用方（relay/sync）计算；本 crate 不做 SHA256。
pub fn management_signing_input(
    instance_id: &[u8; 16],
    vault_id: &[u8; 16],
    request_id: &[u8; 16],
    expected_control_rev: u64,
    body_digest: &[u8; 32],
) -> Result<Vec<u8>, ProtoError> {
    if expected_control_rev > i64::MAX as u64 {
        return Err(ProtoError::Invalid("control_rev 超过 i64::MAX".to_owned()));
    }
    let mut out = Vec::with_capacity(SIGN_MANAGE_DOMAIN.len() + 16 + 16 + 16 + 8 + 32);
    out.extend_from_slice(SIGN_MANAGE_DOMAIN);
    out.extend_from_slice(instance_id);
    out.extend_from_slice(vault_id);
    out.extend_from_slice(request_id);
    out.extend_from_slice(&expected_control_rev.to_be_bytes());
    out.extend_from_slice(body_digest);
    Ok(out)
}

/// 管理签名 CDE 的待哈希输入（method/path/unsigned_body 的 typed CDE map）。
pub fn management_request_cde(
    method: HttpMethod,
    path: &str,
    unsigned_body: &[u8],
) -> Result<Vec<u8>, ProtoError> {
    if !path.starts_with('/') {
        return Err(ProtoError::Invalid("management path 必须绝对".to_owned()));
    }
    crate::cde::encode(&crate::cde::CdeValue::Map(vec![
        (0, crate::cde::CdeValue::UInt(u64::from(u8::from(method)))),
        (1, crate::cde::CdeValue::Text(path.to_owned())),
        (2, crate::cde::CdeValue::Bytes(unsigned_body.to_vec())),
    ]))
}

/// 通用 `domain || payload` 签名输入。
pub fn signature_input(domain: &[u8], payload: &[u8]) -> Vec<u8> {
    with_domain(domain, payload)
}

/// 域常量必须各自恰好以一个 0x00 结尾（§14）；测试和调用方都可复用。
pub fn domain_ends_with_nul(domain: &[u8]) -> bool {
    domain.last() == Some(&0)
        && domain[..domain.len().saturating_sub(1)]
            .iter()
            .all(|b| *b != 0)
}

`````



@@SECTION code/060 `crates/sync-proto/src/cursor.rs`


`````rust
//! 不可伪造游标（ADR 0006 §5）：`base64url(version || instance || vault ||
//! log_incarnation || last_seq_BE64 || HMAC32)`。
//!
//! 本 crate 不做 HMAC；`CursorMacVerifier` 由 relay 注入。

use crate::error::ProtoError;

pub const CURSOR_VERSION: u8 = 1;
pub const CURSOR_RAW_LEN: usize = 1 + 16 + 16 + 16 + 8 + 32;
pub const CURSOR_MAC_INPUT_LEN: usize = 57;
pub const CURSOR_MAC_LEN: usize = 32;
pub const CURSOR_BASE64_LEN: usize = 119;

/// 游标；`mac` 由 relay 的实例密钥验证。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    pub instance_id: [u8; 16],
    pub vault_id: [u8; 16],
    pub log_incarnation: [u8; 16],
    pub last_seq: u64,
    pub mac: [u8; CURSOR_MAC_LEN],
}

/// relay 提供的 HMAC 校验接口；sync-proto 不持有密钥。
pub trait CursorMacVerifier {
    fn verify(&self, input: &[u8], mac: &[u8; CURSOR_MAC_LEN]) -> bool;
}

impl Cursor {
    pub fn new(
        instance_id: [u8; 16],
        vault_id: [u8; 16],
        log_incarnation: [u8; 16],
        last_seq: u64,
        mac: [u8; CURSOR_MAC_LEN],
    ) -> Result<Self, ProtoError> {
        if last_seq > i64::MAX as u64 {
            return Err(ProtoError::Invalid(
                "cursor.last_seq 超过 i64::MAX".to_owned(),
            ));
        }
        Ok(Self {
            instance_id,
            vault_id,
            log_incarnation,
            last_seq,
            mac,
        })
    }

    /// 89B 原始游标（含 MAC）。
    pub fn encode_bytes(&self) -> Result<[u8; CURSOR_RAW_LEN], ProtoError> {
        if self.last_seq > i64::MAX as u64 {
            return Err(ProtoError::Invalid(
                "cursor.last_seq 超过 i64::MAX".to_owned(),
            ));
        }
        let mut out = [0u8; CURSOR_RAW_LEN];
        out[0] = CURSOR_VERSION;
        out[1..17].copy_from_slice(&self.instance_id);
        out[17..33].copy_from_slice(&self.vault_id);
        out[33..49].copy_from_slice(&self.log_incarnation);
        out[49..57].copy_from_slice(&self.last_seq.to_be_bytes());
        out[57..89].copy_from_slice(&self.mac);
        Ok(out)
    }

    /// 前 57B，作为 HMAC 的 input（不含 mac）。
    pub fn mac_input(&self) -> Result<[u8; CURSOR_MAC_INPUT_LEN], ProtoError> {
        let raw = self.encode_bytes()?;
        raw[..CURSOR_MAC_INPUT_LEN]
            .try_into()
            .map_err(|_| ProtoError::Truncated)
    }

    /// `MN/v1/cursor\0 || 前 57B`，relay 计算 HMAC 时使用。
    pub fn mac_input_with_domain(&self) -> Result<Vec<u8>, ProtoError> {
        let mut out = Vec::with_capacity(crate::constants::CURSOR_HMAC_DOMAIN.len() + 57);
        out.extend_from_slice(crate::constants::CURSOR_HMAC_DOMAIN);
        out.extend_from_slice(&self.mac_input()?);
        Ok(out)
    }

    /// 解码结构，不验 MAC；`last_seq`/版本仍严格校验。
    pub fn decode_bytes(bytes: &[u8]) -> Result<Self, ProtoError> {
        if bytes.len() != CURSOR_RAW_LEN {
            return Err(ProtoError::Truncated);
        }
        if bytes[0] != CURSOR_VERSION {
            return Err(ProtoError::UnsupportedVersion(u16::from(bytes[0])));
        }
        let last_seq = u64::from_be_bytes(bytes[49..57].try_into().expect("长度8"));
        if last_seq > i64::MAX as u64 {
            return Err(ProtoError::Invalid(
                "cursor.last_seq 超过 i64::MAX".to_owned(),
            ));
        }
        Ok(Self {
            instance_id: bytes[1..17].try_into().expect("长度16"),
            vault_id: bytes[17..33].try_into().expect("长度16"),
            log_incarnation: bytes[33..49].try_into().expect("长度16"),
            last_seq,
            mac: bytes[57..89].try_into().expect("长度32"),
        })
    }

    /// 结构解码 + relay 注入的 MAC 校验。
    pub fn decode_verified(
        bytes: &[u8],
        verifier: &dyn CursorMacVerifier,
    ) -> Result<Self, ProtoError> {
        let cursor = Self::decode_bytes(bytes)?;
        let input = cursor.mac_input()?;
        if !verifier.verify(&input, &cursor.mac) {
            return Err(ProtoError::Invalid("cursor MAC 校验失败".to_owned()));
        }
        Ok(cursor)
    }

    pub fn to_base64url(&self) -> Result<String, ProtoError> {
        let raw = self.encode_bytes()?;
        Ok(base64url_encode(&raw))
    }

    pub fn from_base64url(value: &str) -> Result<Self, ProtoError> {
        let raw = base64url_decode(value)?;
        Self::decode_bytes(&raw)
    }

    pub fn from_base64url_verified(
        value: &str,
        verifier: &dyn CursorMacVerifier,
    ) -> Result<Self, ProtoError> {
        let raw = base64url_decode(value)?;
        Self::decode_verified(&raw, verifier)
    }
}

fn base64url_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity((bytes.len() * 4).div_ceil(3));
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        out.push(ALPHABET[(b0 >> 2) as usize] as char);
        out.push(ALPHABET[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() >= 2 {
            out.push(ALPHABET[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char);
        }
        if chunk.len() == 3 {
            out.push(ALPHABET[(b2 & 0x3f) as usize] as char);
        }
    }
    out
}

fn base64url_decode(value: &str) -> Result<Vec<u8>, ProtoError> {
    if value.len() % 4 == 1 {
        return Err(ProtoError::Invalid("base64url 长度非法".to_owned()));
    }
    let mut out = Vec::with_capacity(value.len() * 3 / 4);
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for (index, byte) in value.bytes().enumerate() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            b'=' => {
                return Err(ProtoError::Invalid(
                    "cursor base64url 不带 padding".to_owned(),
                ))
            }
            _ => {
                return Err(ProtoError::Invalid(format!(
                    "base64url 第 {index} 字节非法"
                )))
            }
        };
        buffer = (buffer << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    if bits >= 6 || (buffer != 0 && bits > 0) {
        return Err(ProtoError::Invalid("base64url 尾部有非零填充".to_owned()));
    }
    Ok(out)
}

`````



@@SECTION code/061 `crates/sync-proto/src/envelope.rs`


`````rust
//! MNE1 固定 196B header、AAD 与信封级规范化（ADR 0006 §2/§14/§16）。

use crate::error::ProtoError;
use crate::limits::{
    part_count_for, validate_slicing, CIPHER_SUITE_XCHACHA20_POLY1305, CONTEXT_HASH_LEN,
    HEADER_LEN, MAGIC, NONCE_LEN, PROTOCOL_VERSION,
};
use crate::records::{
    AssetPrefix, DeviceId, ManifestV1, MessageId, ObjectId, ObjectType, SnapshotV1, UpdateV1,
    VaultId,
};

/// AAD 域：`MN/e2e/v1` 后接**一个** 0x00 字节（§14）。
pub const AAD_DOMAIN: &[u8] = b"MN/e2e/v1\0";

/// MNE1 固定 header。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvelopeHeader {
    pub protocol_version: u16,
    pub header_len: u16,
    pub object_type: ObjectType,
    pub cipher_suite: u8,
    pub flags: u16,
    pub vault_id: VaultId,
    pub object_id: ObjectId,
    pub message_id: MessageId,
    pub producer_device_id: DeviceId,
    pub generation: u64,
    pub key_epoch: u64,
    pub sender_seq: u64,
    pub part_index: u32,
    pub part_count: u32,
    pub total_plaintext_len: u64,
    pub this_plaintext_len: u32,
    pub ciphertext_len: u32,
    pub context_hash: [u8; CONTEXT_HASH_LEN],
    pub nonce: [u8; NONCE_LEN],
}

impl Default for EnvelopeHeader {
    fn default() -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            header_len: HEADER_LEN as u16,
            object_type: ObjectType::Update,
            cipher_suite: CIPHER_SUITE_XCHACHA20_POLY1305,
            flags: 0,
            vault_id: [0; 16],
            object_id: [0; 32],
            message_id: [0; 16],
            producer_device_id: [0; 16],
            generation: 1,
            key_epoch: 1,
            sender_seq: 1,
            part_index: 0,
            part_count: 1,
            total_plaintext_len: 1,
            this_plaintext_len: 1,
            ciphertext_len: 1 + 16,
            context_hash: [0; 32],
            nonce: [0; 24],
        }
    }
}

impl EnvelopeHeader {
    /// 固定 encode 为 196B，并做完整校验。
    pub fn encode(&self) -> Result<[u8; HEADER_LEN], ProtoError> {
        self.validate()?;
        let mut out = [0u8; HEADER_LEN];
        out[0..4].copy_from_slice(&MAGIC);
        out[4..6].copy_from_slice(&self.protocol_version.to_be_bytes());
        out[6..8].copy_from_slice(&(HEADER_LEN as u16).to_be_bytes());
        out[8] = self.object_type.as_u8();
        out[9] = self.cipher_suite;
        out[10..12].copy_from_slice(&self.flags.to_be_bytes());
        out[12..28].copy_from_slice(&self.vault_id);
        out[28..60].copy_from_slice(&self.object_id);
        out[60..76].copy_from_slice(&self.message_id);
        out[76..92].copy_from_slice(&self.producer_device_id);
        out[92..100].copy_from_slice(&self.generation.to_be_bytes());
        out[100..108].copy_from_slice(&self.key_epoch.to_be_bytes());
        out[108..116].copy_from_slice(&self.sender_seq.to_be_bytes());
        out[116..120].copy_from_slice(&self.part_index.to_be_bytes());
        out[120..124].copy_from_slice(&self.part_count.to_be_bytes());
        out[124..132].copy_from_slice(&self.total_plaintext_len.to_be_bytes());
        out[132..136].copy_from_slice(&self.this_plaintext_len.to_be_bytes());
        out[136..140].copy_from_slice(&self.ciphertext_len.to_be_bytes());
        out[140..172].copy_from_slice(&self.context_hash);
        out[172..196].copy_from_slice(&self.nonce);
        Ok(out)
    }

    /// 解析前 196B；更长的输入只取 header（给信封解析用）。
    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        if bytes.len() < HEADER_LEN {
            return Err(ProtoError::Truncated);
        }
        if bytes[0..4] != MAGIC {
            return Err(ProtoError::Invalid("MNE1 magic 不匹配".to_owned()));
        }
        let protocol_version = u16::from_be_bytes([bytes[4], bytes[5]]);
        if protocol_version != PROTOCOL_VERSION {
            return Err(ProtoError::UnsupportedVersion(protocol_version));
        }
        let header_len = u16::from_be_bytes([bytes[6], bytes[7]]);
        if usize::from(header_len) != HEADER_LEN {
            return Err(ProtoError::Invalid(format!(
                "header_len={header_len}，必须是 196"
            )));
        }
        let object_type = ObjectType::try_from(bytes[8])?;
        let cipher_suite = bytes[9];
        let flags = u16::from_be_bytes([bytes[10], bytes[11]]);
        let value = Self {
            protocol_version,
            header_len,
            object_type,
            cipher_suite,
            flags,
            vault_id: bytes[12..28].try_into().expect("长度16"),
            object_id: bytes[28..60].try_into().expect("长度32"),
            message_id: bytes[60..76].try_into().expect("长度16"),
            producer_device_id: bytes[76..92].try_into().expect("长度16"),
            generation: read_u64(bytes, 92),
            key_epoch: read_u64(bytes, 100),
            sender_seq: read_u64(bytes, 108),
            part_index: read_u32(bytes, 116),
            part_count: read_u32(bytes, 120),
            total_plaintext_len: read_u64(bytes, 124),
            this_plaintext_len: read_u32(bytes, 132),
            ciphertext_len: read_u32(bytes, 136),
            context_hash: bytes[140..172].try_into().expect("长度32"),
            nonce: bytes[172..196].try_into().expect("长度24"),
        };
        value.validate()?;
        Ok(value)
    }

    pub fn to_bytes(&self) -> Result<[u8; HEADER_LEN], ProtoError> {
        self.encode()
    }

    pub fn validate(&self) -> Result<(), ProtoError> {
        if self.protocol_version != PROTOCOL_VERSION {
            return Err(ProtoError::UnsupportedVersion(self.protocol_version));
        }
        if self.header_len as usize != HEADER_LEN {
            return Err(ProtoError::Invalid("header_len 必须为 196".to_owned()));
        }
        if self.cipher_suite != CIPHER_SUITE_XCHACHA20_POLY1305 {
            return Err(ProtoError::Invalid(format!(
                "未知 cipher suite {}",
                self.cipher_suite
            )));
        }
        if self.flags != 0 {
            return Err(ProtoError::Invalid("v1 flags 必须为 0".to_owned()));
        }
        self.validate_normalization()?;
        self.validate_lengths()?;
        Ok(())
    }

    /// §16.1/§16.2：不需要密文载荷就能校验的 generation/sender_seq 规则。
    pub fn validate_normalization(&self) -> Result<(), ProtoError> {
        match self.object_type {
            ObjectType::Update | ObjectType::Snapshot => {
                if self.generation == 0 {
                    return Err(ProtoError::Invalid(
                        "UPDATE/SNAPSHOT header.generation 必须 >0".to_owned(),
                    ));
                }
            }
            ObjectType::Manifest | ObjectType::Asset | ObjectType::KeySlot => {
                if self.generation != 0 {
                    return Err(ProtoError::Invalid(
                        "MANIFEST/ASSET/KEY_SLOT header.generation 必须为 0".to_owned(),
                    ));
                }
            }
        }
        if self.object_type != ObjectType::Update && self.sender_seq != 0 {
            return Err(ProtoError::Invalid(
                "非 UPDATE 的 header.sender_seq 必须为 0".to_owned(),
            ));
        }
        if self.generation > i64::MAX as u64
            || self.key_epoch > i64::MAX as u64
            || self.sender_seq > i64::MAX as u64
            || self.total_plaintext_len > i64::MAX as u64
        {
            return Err(ProtoError::Invalid(
                "header u64 字段超过 i64::MAX".to_owned(),
            ));
        }
        Ok(())
    }

    /// §16.3 固定切片和本类型的长度上限。
    pub fn validate_lengths(&self) -> Result<(), ProtoError> {
        let total = self.total_plaintext_len;
        if total == 0 {
            return Err(ProtoError::Invalid("载荷不能为空".to_owned()));
        }
        let expected_parts = part_count_for(total, self.object_type);
        if u64::from(self.part_count) != expected_parts {
            return Err(ProtoError::Invalid(format!(
                "part_count={}，固定切片应为 {expected_parts}",
                self.part_count
            )));
        }
        validate_slicing(
            self.object_type,
            total,
            self.part_index,
            self.part_count,
            self.this_plaintext_len,
        )?;
        let expected_ciphertext = self
            .this_plaintext_len
            .checked_add(16)
            .ok_or(ProtoError::TooLarge)?;
        if self.ciphertext_len != expected_ciphertext {
            return Err(ProtoError::Invalid(format!(
                "ciphertext_len={}，应为 this_plaintext_len+16={expected_ciphertext}",
                self.ciphertext_len
            )));
        }
        // 单片密文不得超过固定上限（header+tag 计入 envelope）。
        let max_ciphertext = match self.object_type {
            ObjectType::Asset => crate::limits::FOUR_MIB - HEADER_LEN,
            _ => crate::limits::ONE_MIB - HEADER_LEN,
        };
        if self.ciphertext_len as usize > max_ciphertext {
            return Err(ProtoError::TooLarge);
        }
        match self.object_type {
            ObjectType::Update => {
                if total > crate::limits::MAX_UPDATE_CDE_BYTES as u64 {
                    return Err(ProtoError::TooLarge);
                }
            }
            ObjectType::Manifest => {
                if total > crate::limits::MAX_MANIFEST_CDE_BYTES as u64 {
                    return Err(ProtoError::TooLarge);
                }
            }
            ObjectType::Snapshot => {
                if total > crate::limits::MAX_LORO_SNAPSHOT_BYTES as u64 {
                    return Err(ProtoError::TooLarge);
                }
            }
            ObjectType::Asset => {
                if total > crate::limits::MAX_ASSET_RAW_BYTES as u64 {
                    return Err(ProtoError::TooLarge);
                }
            }
            ObjectType::KeySlot => {
                if total > crate::limits::MAX_KEY_SLOT_PLAINTEXT as u64 {
                    return Err(ProtoError::TooLarge);
                }
            }
        }
        if self.object_type == ObjectType::KeySlot {
            if self.part_count != 1 {
                return Err(ProtoError::Invalid("KEY_SLOT 必须单片".to_owned()));
            }
            if self.part_index != 0 {
                return Err(ProtoError::Invalid(
                    "KEY_SLOT part_index 必须为 0".to_owned(),
                ));
            }
        }
        if self.object_type != ObjectType::KeySlot && self.context_hash != [0u8; 32] {
            return Err(ProtoError::Invalid(
                "非 KEY_SLOT 的 context_hash 必须全零".to_owned(),
            ));
        }
        Ok(())
    }

    /// §16.1：UPDATE 的 header.generation 必须等于载荷 doc generation。
    pub fn validate_update_payload(&self, update: &UpdateV1) -> Result<(), ProtoError> {
        self.validate()?;
        if self.object_type != ObjectType::Update {
            return Err(ProtoError::Invalid("header 不是 UPDATE".to_owned()));
        }
        if self.generation != update.generation {
            return Err(ProtoError::Invalid(
                "header.generation 与 UpdateV1.generation 不一致".to_owned(),
            ));
        }
        if self.sender_seq != update.origin_seq {
            return Err(ProtoError::Invalid(
                "header.sender_seq 与 UpdateV1.origin_seq 不一致".to_owned(),
            ));
        }
        Ok(())
    }

    /// §16.1：SNAPSHOT 的 generation 对齐，sender_seq 必须为 0。
    pub fn validate_snapshot_payload(&self, snapshot: &SnapshotV1) -> Result<(), ProtoError> {
        self.validate()?;
        if self.object_type != ObjectType::Snapshot {
            return Err(ProtoError::Invalid("header 不是 SNAPSHOT".to_owned()));
        }
        if self.generation != snapshot.generation {
            return Err(ProtoError::Invalid(
                "header.generation 与 SnapshotV1.generation 不一致".to_owned(),
            ));
        }
        if self.sender_seq != 0 {
            return Err(ProtoError::Invalid(
                "SNAPSHOT sender_seq 必须为 0".to_owned(),
            ));
        }
        Ok(())
    }

    /// §16.1/§16.2：MANIFEST 的 generation/sender_seq 均为 0，key_epoch 与载荷一致。
    pub fn validate_manifest_payload(&self, manifest: &ManifestV1) -> Result<(), ProtoError> {
        self.validate()?;
        if self.object_type != ObjectType::Manifest {
            return Err(ProtoError::Invalid("header 不是 MANIFEST".to_owned()));
        }
        if self.generation != 0 || self.sender_seq != 0 {
            return Err(ProtoError::Invalid(
                "MANIFEST generation/sender_seq 必须为 0".to_owned(),
            ));
        }
        if self.key_epoch != manifest.key_epoch {
            return Err(ProtoError::Invalid(
                "header.key_epoch 与 ManifestV1.key_epoch 不一致".to_owned(),
            ));
        }
        if self.vault_id != manifest.vault_id {
            return Err(ProtoError::Invalid(
                "header.vault_id 与 ManifestV1.vault_id 不一致".to_owned(),
            ));
        }
        Ok(())
    }

    pub fn validate_asset_payload(&self, prefix: &AssetPrefix) -> Result<(), ProtoError> {
        self.validate()?;
        if self.object_type != ObjectType::Asset {
            return Err(ProtoError::Invalid("header 不是 ASSET".to_owned()));
        }
        if self.generation != 0 || self.sender_seq != 0 {
            return Err(ProtoError::Invalid(
                "ASSET generation/sender_seq 必须为 0".to_owned(),
            ));
        }
        if self.total_plaintext_len < 42 {
            return Err(ProtoError::Invalid("ASSET 明文不足以容纳前缀".to_owned()));
        }
        let _ = prefix;
        Ok(())
    }

    /// AAD = `MN/e2e/v1\0 || header || BE32(descriptor_len) || descriptor`。
    pub fn aad(&self, descriptor: &[u8]) -> Result<Vec<u8>, ProtoError> {
        self.validate()?;
        if self.object_type != ObjectType::KeySlot && !descriptor.is_empty() {
            return Err(ProtoError::Invalid(
                "非 KEY_SLOT 的 descriptor 必须为空".to_owned(),
            ));
        }
        if self.object_type == ObjectType::KeySlot && descriptor.is_empty() {
            return Err(ProtoError::Invalid("KEY_SLOT 必须带 descriptor".to_owned()));
        }
        let descriptor_len = u32::try_from(descriptor.len()).map_err(|_| ProtoError::TooLarge)?;
        let header = self.encode()?;
        let mut out = Vec::with_capacity(AAD_DOMAIN.len() + HEADER_LEN + 4 + descriptor.len());
        out.extend_from_slice(AAD_DOMAIN);
        out.extend_from_slice(&header);
        out.extend_from_slice(&descriptor_len.to_be_bytes());
        out.extend_from_slice(descriptor);
        Ok(out)
    }
}

/// 便捷构造 AAD（会做 header 校验与 descriptor 规则）。
pub fn aad(header: &EnvelopeHeader, descriptor: &[u8]) -> Result<Vec<u8>, ProtoError> {
    header.aad(descriptor)
}

/// 一个完整 MNE1 信封。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope {
    pub header: EnvelopeHeader,
    pub ciphertext: Vec<u8>,
}

impl Envelope {
    pub fn new(header: EnvelopeHeader, ciphertext: Vec<u8>) -> Result<Self, ProtoError> {
        let value = Self { header, ciphertext };
        value.validate()?;
        Ok(value)
    }

    pub fn validate(&self) -> Result<(), ProtoError> {
        self.header.validate()?;
        let expected = self.header.ciphertext_len as usize;
        if self.ciphertext.len() != expected {
            return Err(ProtoError::Invalid(format!(
                "ciphertext {}B，header 声明 {expected}B",
                self.ciphertext.len()
            )));
        }
        Ok(())
    }

    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        self.validate()?;
        let header = self.header.encode()?;
        let mut out = Vec::with_capacity(HEADER_LEN + self.ciphertext.len());
        out.extend_from_slice(&header);
        out.extend_from_slice(&self.ciphertext);
        Ok(out)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        if bytes.len() < HEADER_LEN {
            return Err(ProtoError::Truncated);
        }
        let header = EnvelopeHeader::decode(&bytes[..HEADER_LEN])?;
        let expected = HEADER_LEN + header.ciphertext_len as usize;
        if bytes.len() != expected {
            return Err(ProtoError::Invalid(format!(
                "信封总长 {}B，应为 {expected}B",
                bytes.len()
            )));
        }
        Ok(Self {
            header,
            ciphertext: bytes[HEADER_LEN..].to_vec(),
        })
    }
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes(bytes[offset..offset + 4].try_into().expect("长度4"))
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_be_bytes(bytes[offset..offset + 8].try_into().expect("长度8"))
}

`````



@@SECTION code/062 `crates/sync-proto/src/error.rs`


`````rust
//! sync-proto 的基础错误与错误码。

use std::fmt;

/// 纯协议层错误；不做 IO/网络描述。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtoError {
    /// 数据太短（header/part 截断）。
    Truncated,
    /// 文本/字节长度或字段值越界。
    Invalid(String),
    /// 非 canonical CBOR（整数过长、键乱序、重复键、浮点/null/不定长等）。
    NonCanonical(String),
    /// 未知字段/键。
    UnknownField(u64),
    /// 字段缺失。
    MissingField(u64),
    /// 不允许的版本。
    UnsupportedVersion(u16),
    /// 超过协议上限。
    TooLarge,
}

impl fmt::Display for ProtoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated => f.write_str("截断的协议数据"),
            Self::Invalid(message) => write!(f, "协议字段非法: {message}"),
            Self::NonCanonical(message) => write!(f, "非 canonical CDE: {message}"),
            Self::UnknownField(key) => write!(f, "未知字段键 {key}"),
            Self::MissingField(key) => write!(f, "缺少字段键 {key}"),
            Self::UnsupportedVersion(version) => write!(f, "不支持的协议版本 {version}"),
            Self::TooLarge => f.write_str("超过协议大小上限"),
        }
    }
}

impl std::error::Error for ProtoError {}

/// ADR 0006 §5 的统一错误码（wire 用字符串）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    AlreadyInitialized,
    Revoked,
    ChallengeInvalid,
    ChallengeUsed,
    ManagementRequired,
    InviteUsed,
    Expired,
    Rotating,
    IdConflict,
    Epoch,
    ObjectBusy,
    Incomplete,
    BadCursor,
    CursorExpired,
    Reset,
    NoValidBaseline,
    LeaseExpired,
    Busy,
    Reserve,
    RefConflict,
    NonMonotone,
    RecipientMismatch,
    CursorBackward,
    RollbackDetected,
    RelayReset,
    Forbidden,
    Unauthorized,
    NotFound,
    Gone,
    MethodNotAllowed,
    Conflict,
    RequestTooLarge,
    TooManyRequests,
    Internal,
}

impl ErrorCode {
    /// wire 字符串（全大写，稳定）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AlreadyInitialized => "ALREADY_INITIALIZED",
            Self::Revoked => "REVOKED",
            Self::ChallengeInvalid => "CHALLENGE_INVALID",
            Self::ChallengeUsed => "CHALLENGE_USED",
            Self::ManagementRequired => "MANAGEMENT_REQUIRED",
            Self::InviteUsed => "INVITE_USED",
            Self::Expired => "EXPIRED",
            Self::Rotating => "ROTATING",
            Self::IdConflict => "ID_CONFLICT",
            Self::Epoch => "EPOCH",
            Self::ObjectBusy => "OBJECT_BUSY",
            Self::Incomplete => "INCOMPLETE",
            Self::BadCursor => "BAD_CURSOR",
            Self::CursorExpired => "CURSOR_EXPIRED",
            Self::Reset => "RESET",
            Self::NoValidBaseline => "NO_VALID_BASELINE",
            Self::LeaseExpired => "LEASE_EXPIRED",
            Self::Busy => "BUSY",
            Self::Reserve => "RESERVE",
            Self::RefConflict => "REF_CONFLICT",
            Self::NonMonotone => "NON_MONOTONE",
            Self::RecipientMismatch => "RECIPIENT_MISMATCH",
            Self::CursorBackward => "CURSOR_BACKWARD",
            Self::RollbackDetected => "ROLLBACK_DETECTED",
            Self::RelayReset => "RELAY_RESET",
            Self::Forbidden => "FORBIDDEN",
            Self::Unauthorized => "UNAUTHORIZED",
            Self::NotFound => "NOT_FOUND",
            Self::Gone => "GONE",
            Self::MethodNotAllowed => "METHOD_NOT_ALLOWED",
            Self::Conflict => "CONFLICT",
            Self::RequestTooLarge => "REQUEST_TOO_LARGE",
            Self::TooManyRequests => "TOO_MANY_REQUESTS",
            Self::Internal => "INTERNAL",
        }
    }

    /// 客户端是否可重试（保守口径）。
    pub fn retryable(self) -> bool {
        matches!(
            self,
            Self::Busy | Self::ObjectBusy | Self::TooManyRequests | Self::Internal
        )
    }
}

/// 错误响应体；不含秘密。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorBody {
    pub code: ErrorCode,
    pub retryable: bool,
    pub current_epoch: Option<u64>,
    pub current_control_rev: Option<u64>,
    pub bootstrap_hint: Option<String>,
}

impl ErrorBody {
    pub fn new(code: ErrorCode) -> Self {
        Self {
            code,
            retryable: code.retryable(),
            current_epoch: None,
            current_control_rev: None,
            bootstrap_hint: None,
        }
    }
}

`````



@@SECTION code/063 `crates/sync-proto/src/lib.rs`


`````rust
//! sync-proto —— 同步/加密 v1 的纯类型与 wire 编解码。
//!
//! 契约见 `docs/adr/0006-sync-v1.md`：MNE1 196B header、CDE record、AAD/域
//! 分隔符、固定切片、ObjectDigest/cursor 类型、错误码与 limits。
//! 本 crate 不做 IO / crypto / Loro / SQLite / GPUI。

pub mod activation_t;
pub mod cde;
pub mod constants;
pub mod cursor;
pub mod envelope;
pub mod error;
pub mod limits;
pub mod management_cap_v2;
pub mod records;
mod rotation_records;

pub use activation_t::{activation_t_signing_input, ActivationTV1, ACTIVATION_T_FIELD_COUNT};
pub use cde::CdeValue as CborValue;
pub use cde::{decode as decode_cde, encode as encode_cde, CdeValue, MAX_CDE_BYTES};
pub use constants::*;
pub use cursor::{
    Cursor, CursorMacVerifier, CURSOR_BASE64_LEN, CURSOR_MAC_INPUT_LEN, CURSOR_MAC_LEN,
    CURSOR_RAW_LEN, CURSOR_VERSION,
};
pub use envelope::EnvelopeHeader as Header;
pub use envelope::{aad, Envelope, EnvelopeHeader, AAD_DOMAIN};
pub use envelope::{Envelope as Mne1Envelope, EnvelopeHeader as Mne1Header};
pub use error::{ErrorBody, ErrorCode, ProtoError};
pub use limits::*;
pub use management_cap_v2::{
    auth_signing_input_v2, canonical_submit_path, control_signing_input, pop_signing_input,
    recover_signing_input, set_digest_v2, upgrade_signing_input, ActivationV2, CapGrantPayloadV2,
    CapId, CapRevokePayloadV2, CapScope, ChallengeV2, ControlKindV2, DeviceApprovePayloadV2,
    DeviceCertV2, DeviceCredentialPayloadV2, DeviceRevokePayloadV2, DeviceRightsChangePayloadV2,
    EpochActivatePayloadV2, GrantAadV2, GrantIdV2, GrantPackageV2, InviteCancelPayloadV2,
    InviteCreatePayloadV2, ManagementCapV2, OperationV2, PolicyV2, RecoveryKeyChangePayloadV2,
    RecoveryResetPayloadV2, RequestIdV2, RotationAbortPayloadV2, RotationAddRevokesPayloadV2,
    RotationBeginPayloadV2, RotationIdV2, RotationRestartPayloadV2, SetDigestEntryV2, Sha256Fn,
    SignedControlV2, SignerRole, SignerV2, UpgradePolicyPayloadV2, ACTIVATION_V2_FIELD_COUNT,
    CAP_GRANT_PAYLOAD_V2_FIELD_COUNT, CAP_REVOKE_PAYLOAD_V2_FIELD_COUNT, CHALLENGE_V2_FIELD_COUNT,
    CHALLENGE_V2_REALM_DEVICE, CHALLENGE_V2_REALM_MANAGEMENT_DEVICE, CHALLENGE_V2_REALM_RECOVERY,
    DEVICE_CERT_V2_FIELD_COUNT, DEVICE_CREDENTIAL_PAYLOAD_V2_FIELD_COUNT,
    DEVICE_REVOKE_PAYLOAD_V2_FIELD_COUNT, EPOCH_ACTIVATE_PAYLOAD_V2_FIELD_COUNT,
    GRANT_AAD_V2_FIELD_COUNT, GRANT_PACKAGE_V2_FIELD_COUNT, INVITE_CANCEL_PAYLOAD_V2_FIELD_COUNT,
    INVITE_CREATE_PAYLOAD_V2_FIELD_COUNT, MANAGEMENT_CAP_V2_FIELD_COUNT, OPERATION_V2_FIELD_COUNT,
    POLICY_V2_FIELD_COUNT, RECOVERY_KEY_CHANGE_PAYLOAD_V2_FIELD_COUNT,
    RECOVERY_RESET_PAYLOAD_V2_FIELD_COUNT, ROTATION_ABORT_PAYLOAD_V2_FIELD_COUNT,
    ROTATION_ADD_REVOKES_PAYLOAD_V2_FIELD_COUNT, ROTATION_BEGIN_PAYLOAD_V2_FIELD_COUNT,
    ROTATION_RESTART_PAYLOAD_V2_FIELD_COUNT, SIGNED_CONTROL_V2_FIELD_COUNT, SIGNER_V2_FIELD_COUNT,
    UPGRADE_POLICY_PAYLOAD_V2_FIELD_COUNT, V2_WIRE_VERSION,
};
pub use records::ObjectType as MessageType;
pub use records::{
    check_device_count, device_cert_signed_bytes, grant_signed_bytes,
    management_signing_input as management_request_signing_input, object_digest_input,
    refs_digest_input, Argon2idParams, AssetPrefix, AuthRealm, Challenge, ChallengeId,
    CheckpointCommit, CheckpointId, CheckpointValidationV2, DeviceCertV1, DeviceId,
    Ed25519PublicKey, GrantAadV1, HttpMethod, Kdf, ManifestAsset, ManifestDoc, ManifestV1,
    MessageId, ObjectCommit, ObjectId, ObjectRefV1, ObjectType, RelayInstanceId, Rights,
    RootSecretV1, Sha256, SlotDescriptorV1, SlotKind, SnapshotV1, UpdateV1, VaultId,
    X25519PublicKey,
};
pub use rotation_records::{
    grant_package_digest, member_cert_digest, slot_set_digest, AbortBodyV1, ActivateBodyV1,
    ActivationCommitV1, ApproveBodyV1, BeginBodyV1, ControlEvent, ControlKind, DeviceApproveBodyV1,
    EpochActivated, GrantStageBodyV1, GrantsBodyV1, InviteCreateBodyV1, InvitesBodyV1,
    MemberCertDigestInputV1, NewManagerV1, RevocationsAdded, RotationAbortBodyV1, RotationAborted,
    RotationActivateBodyV1, RotationBeginBodyV1, RotationRestarted, SignedControlRecord,
    SlotActivateBodyV1, SlotSetDigestInputV1, SlotsActivateBodyV1,
};

`````



@@SECTION code/064 `crates/sync-proto/src/limits.rs`


`````rust
//! ADR 0006 的 wire 常量、上限与固定切片规则。

use crate::error::ProtoError;

pub const MAGIC: [u8; 4] = *b"MNE1";
pub const PROTOCOL_VERSION: u16 = 1;
pub const HEADER_LEN: usize = 196;
pub const TAG_LEN: usize = 16;
pub const NONCE_LEN: usize = 24;
pub const CONTEXT_HASH_LEN: usize = 32;
pub const OBJECT_ID_LEN: usize = 32;
pub const VAULT_ID_LEN: usize = 16;
pub const MESSAGE_ID_LEN: usize = 16;
pub const DEVICE_ID_LEN: usize = 16;
pub const CIPHER_SUITE_XCHACHA20_POLY1305: u8 = 1;
/// header 196 + AEAD tag 16。
pub const ENVELOPE_OVERHEAD: usize = HEADER_LEN + TAG_LEN; // 212
pub const ONE_MIB: usize = 1024 * 1024;
pub const FOUR_MIB: usize = 4 * 1024 * 1024;
pub const SIXTY_FOUR_MIB: usize = 64 * 1024 * 1024;
/// 非资产单片的明文上限（1MiB - 212）。
pub const MAX_NON_ASSET_PART_PLAINTEXT: usize = ONE_MIB - ENVELOPE_OVERHEAD;
/// 资产单片的明文上限（4MiB - 212）。
pub const MAX_ASSET_PART_PLAINTEXT: usize = FOUR_MIB - ENVELOPE_OVERHEAD;
/// KEY_SLOT 明文硬上限。
pub const MAX_KEY_SLOT_PLAINTEXT: usize = 4096;
pub const MAX_UPDATE_CDE_BYTES: usize = SIXTY_FOUR_MIB;
pub const MAX_SNAPSHOT_CDE_BYTES: usize = SIXTY_FOUR_MIB;
pub const MAX_MANIFEST_CDE_BYTES: usize = SIXTY_FOUR_MIB;
pub const MAX_ASSET_RAW_BYTES: usize = SIXTY_FOUR_MIB;
pub const MAX_ASSET_MIME_BYTES: usize = 255;
pub const MAX_REFS: usize = 1024;
pub const MAX_CONTROL_HTTP_BYTES: usize = ONE_MIB;
pub const MAX_WS_FRAME_BYTES: usize = 512;
pub const MAX_DEVICES: usize = 32;
pub const MAX_PASSWORD_BYTES: usize = 1024;
pub const MAX_LORO_SNAPSHOT_BYTES: usize = 4 * 1024 * 1024 * 1024;
pub const MAX_CHECKPOINT_REFS: usize = 1024;

// —— ADR 0015：管理控制链 V2 ——

/// `PolicyV2.max_cap_ttl` 的协议上限（秒，30 天）。
pub const MAX_CAP_TTL_SECS_V2: u64 = 2_592_000;
/// `PolicyV2.max_invite_ttl` 的协议上限（秒，1 天）。
pub const MAX_INVITE_TTL_SECS_V2: u64 = 86_400;
/// 单条 `OperationV2` 的 signer 上限。ADR 0015 只固定“高危两签”，未给数组上限；
/// 这里取 16 作为实现边界（升级/PoP/两签叠加仍有充足余量），超出即 `TooLarge`。
pub const MAX_OPERATION_SIGNERS_V2: usize = 16;
/// `OperationV2.canonical_path` 的字节上限；实际唯一提交路径固定 58B。
pub const MAX_CANONICAL_PATH_BYTES_V2: usize = 4096;
/// `UpgradePolicyPayloadV2.bootstrap_caps[]` 上限（实现边界；ADR 未固定条数）。
pub const MAX_BOOTSTRAP_CAPS_V2: usize = 64;
/// `DeviceApprove/DeviceRightsChange` payload 的 `grants[]` 上限（实现边界；ADR 未固定条数）。
///
/// 一个 epoch 只需一份 grant（PK `(vault_id, epoch, device_id)`），当前实现进一步要求
/// 恰好一份当前 epoch 的包；上限只是拒绝超长数组的 wire 边界。
pub const MAX_GRANT_PACKAGES_V2: usize = 32;

/// 固定切片明文容量。
pub const fn cap_plain(object_type: crate::records::ObjectType) -> u64 {
    use crate::records::ObjectType;
    match object_type {
        ObjectType::Asset => MAX_ASSET_PART_PLAINTEXT as u64,
        _ => MAX_NON_ASSET_PART_PLAINTEXT as u64,
    }
}

/// `ceil(total / cap)`；total=0 时返回 0（由调用方再判非空）。
pub const fn part_count_for(
    total_plaintext_len: u64,
    object_type: crate::records::ObjectType,
) -> u64 {
    let cap = cap_plain(object_type);
    if total_plaintext_len == 0 {
        0
    } else {
        total_plaintext_len.div_ceil(cap)
    }
}

/// 第 `part_index` 片应有的明文长度（0-based；调用方须保证 index < count）。
pub fn expected_part_plain_len(
    object_type: crate::records::ObjectType,
    total_plaintext_len: u64,
    part_index: u32,
) -> Result<usize, ProtoError> {
    if total_plaintext_len == 0 {
        return Err(ProtoError::Invalid(
            "total_plaintext_len 不能为 0".to_owned(),
        ));
    }
    let cap = cap_plain(object_type);
    let count = part_count_for(total_plaintext_len, object_type);
    let index = u64::from(part_index);
    if index >= count {
        return Err(ProtoError::Invalid(format!(
            "part_index {part_index} 超出 part_count {count}"
        )));
    }
    let before = cap * index;
    let remaining = total_plaintext_len - before;
    usize::try_from(remaining.min(cap)).map_err(|_| ProtoError::TooLarge)
}

/// 校验固定切片（§16.3）：count、每片长度、最后一片正长度、KEY_SLOT 单片。
pub fn validate_slicing(
    object_type: crate::records::ObjectType,
    total_plaintext_len: u64,
    part_index: u32,
    part_count: u32,
    this_plaintext_len: u32,
) -> Result<(), ProtoError> {
    use crate::records::ObjectType;
    if total_plaintext_len == 0 {
        return Err(ProtoError::Invalid("载荷不能为空".to_owned()));
    }
    if part_count == 0 {
        return Err(ProtoError::Invalid("part_count 必须 ≥1".to_owned()));
    }
    if part_index >= part_count {
        return Err(ProtoError::Invalid(
            "part_index 必须 < part_count".to_owned(),
        ));
    }
    let expected_count = part_count_for(total_plaintext_len, object_type);
    if u64::from(part_count) != expected_count {
        return Err(ProtoError::Invalid(format!(
            "part_count={part_count} 与固定切片计算值 {expected_count} 不符"
        )));
    }
    let expected_len = expected_part_plain_len(object_type, total_plaintext_len, part_index)?;
    if usize::try_from(this_plaintext_len).ok() != Some(expected_len) {
        return Err(ProtoError::Invalid(format!(
            "this_plaintext_len={this_plaintext_len}，应为 {expected_len}"
        )));
    }
    if object_type == ObjectType::KeySlot {
        if total_plaintext_len > MAX_KEY_SLOT_PLAINTEXT as u64 {
            return Err(ProtoError::TooLarge);
        }
        if part_count != 1 {
            return Err(ProtoError::Invalid("KEY_SLOT 必须单片".to_owned()));
        }
    }
    if object_type == ObjectType::Asset && total_plaintext_len > MAX_ASSET_RAW_BYTES as u64 {
        return Err(ProtoError::TooLarge);
    }
    Ok(())
}

`````



@@SECTION code/065 `crates/sync-proto/src/management_cap_v2.rs`


`````rust
//! ADR 0015：`ManagementCapV2` 与 V2 管理控制链的 wire 类型、严格 CDE 编解码与签名域。
//!
//! 本模块只做**纯字节布局与结构校验**：不计算 SHA-256、不验签、不碰 IO/数据库/时钟，
//! 与 v1 的 [`crate::records::management_signing_input`] 把 `body_digest` 交给调用方
//! 计算是同一口径（ADR 0003：sync-proto 是纯类型 crate，无 crypto 依赖）。
//!
//! 共享摘要 `h = SHA256(CDE(OperationV2))` 由调用方通过 [`Sha256Fn`] 注入：
//!
//! ```text
//! let h  = operation.digest(hash_fn)?;                 // = SHA256(CDE(OperationV2))
//! let in = control_signing_input(&operation, hash_fn)?; // = MN/v2/control\0 || h
//! ```
//!
//! # 固定对象（ADR 0015「固定对象」）
//!
//! | 对象 | 键 | 字段 |
//! | --- | --- | --- |
//! | `ManagementCapV2` | 0..12 | 2,I,V,C,subject_D,subject_ed_pk,scopes,not_before,expires_at,issued_control_rev,issued_authz_rev,issuer_authority_rev,false |
//! | `OperationV2` | 0..14 | 2,I,V,kind,R,expected_control_rev,prev_control_hash,expected_authz_rev,authority_rev,signed_at,not_after,2(POST),canonical_path,payload(CDE),signers[] |
//! | `PolicyV2` | 0..4 | 2,highrisk_threshold=2,min_quorum=2,max_cap_ttl,max_invite_ttl |
//! | `SignerV2` | 0..3 | role,device_id(0/16),cap_id(0/16),pk32 |
//! | `SignedControlV2` | 0..1 | bytes(OperationV2),array(sig64) |
//! | `DeviceCertV2` | 0..8 | 2,I,V,D,sign_pk,hpke_pk,serial,data_rights,issued_authz_rev |
//! | `GrantAadV2` | 0..8 | 2,I,V,epoch,recipient_D,recipient_hpke_pk,authority_rev,grant_id,issued_authz_rev |
//! | grant 包 `GrantPackageV2` | 0..2 | aad_bytes,enc,ciphertext |
//! | `ActivationV2` | 0..13 | 2,rotation_id,from_epoch,to_epoch,freeze_S,primary_CID,primary_commit_hash,fallback_CID,fallback_commit_hash,slot_set_hash,member_cert_hash,grants_hash,old_asset_hash,new_asset_hash |
//! | `DeviceCredentialPayloadV2` | 0..1 | cert_bytes, grant_packages[]（kind 4/16 共用一个 wire 形状）|
//! | `DeviceRevokePayloadV2` | 0..0 | sorted_device_ids[]（16B 数组，严格升序去重）|
//!
//! `I/V/D/C/R` 是 16B 的 instance/vault/device/cap/request ID；pk/hash 是 32B；
//! sig 是 64B；`u` 为 `0..=i64::MAX`（SQLite INTEGER 上界，计数/时间溢出即拒绝）。
//!
//! # 签名域（ADR 0015「签名域与提交」）
//!
//! 共同摘要 `h = SHA256(CDE(OperationV2))`，各域都已在 `constants` 中定义为
//! **真实 NUL 结尾**的字节串，与 v1 域互不复用：
//!
//! ```text
//! 设备签      MN/v2/control\0 || h
//! 升级根签    MN/v2/upgrade\0 || h
//! 恢复根签    MN/v2/recover\0 || h
//! PoP 签      MN/v2/pop\0     || h || pk32
//! 链 hash     SHA256(prev_hash || SignedControlV2_bytes)
//! 鉴权        MN/v2/auth\0    || challenge
//! ```
//!
//! # 严格 CDE
//!
//! 复用 [`crate::cde`] 的 canonical 编解码：整数最短形式、键升序、字段数精确、
//! 拒绝未知/重复键、null/undefined/tag/float/不定长、尾随字节、超深嵌套。
//! 每个记录再校验 ID/pk 非全零、长度、版本、枚举与取值范围；编解码是唯一的
//! 字节串（同一语义值只有一种 wire 形式）。

use std::cmp::Ordering;

use crate::cde::{
    decode, encode, expect_array, expect_bool, expect_bytes, expect_bytes_n, expect_i64_max,
    expect_u16, expect_u32, expect_u8, into_record_fields, CdeValue,
};
use crate::constants::{
    SIGN_AUTH_V2_DOMAIN, SIGN_V2_CONTROL_DOMAIN, SIGN_V2_POP_DOMAIN, SIGN_V2_RECOVER_DOMAIN,
    SIGN_V2_UPGRADE_DOMAIN,
};
use crate::error::ProtoError;
use crate::limits::{
    MAX_BOOTSTRAP_CAPS_V2, MAX_CANONICAL_PATH_BYTES_V2, MAX_CAP_TTL_SECS_V2, MAX_DEVICES,
    MAX_GRANT_PACKAGES_V2, MAX_INVITE_TTL_SECS_V2, MAX_OPERATION_SIGNERS_V2,
};
use crate::records::{
    DeviceId, Ed25519PublicKey, RelayInstanceId, Rights, Sha256, VaultId, X25519PublicKey,
};

/// 所有 V2 对象字段 0 的版本号。
pub const V2_WIRE_VERSION: u16 = 2;
/// `ManagementCapV2` 字段个数（键恰好 `0..13`）。
pub const MANAGEMENT_CAP_V2_FIELD_COUNT: usize = 13;
/// `OperationV2` 字段个数（键恰好 `0..15`）。
pub const OPERATION_V2_FIELD_COUNT: usize = 15;
/// `PolicyV2` 字段个数（键恰好 `0..5`）。
pub const POLICY_V2_FIELD_COUNT: usize = 5;
/// `SignerV2` 字段个数（键恰好 `0..4`）。
pub const SIGNER_V2_FIELD_COUNT: usize = 4;
/// `SignedControlV2` 字段个数（键恰好 `0..2`）。
pub const SIGNED_CONTROL_V2_FIELD_COUNT: usize = 2;
/// `DeviceCertV2` 字段个数（键恰好 `0..9`）。
pub const DEVICE_CERT_V2_FIELD_COUNT: usize = 9;
/// `GrantAadV2` 字段个数（键恰好 `0..9`）。
pub const GRANT_AAD_V2_FIELD_COUNT: usize = 9;
/// `GrantPackageV2` 字段个数（键恰好 `0..3`）。
pub const GRANT_PACKAGE_V2_FIELD_COUNT: usize = 3;
/// `ActivationV2` 字段个数（键恰好 `0..14`）。
pub const ACTIVATION_V2_FIELD_COUNT: usize = 14;
/// `UpgradePolicyPayloadV2` 字段个数（键恰好 `0..5`）。
pub const UPGRADE_POLICY_PAYLOAD_V2_FIELD_COUNT: usize = 5;
/// `CapGrantPayloadV2` 字段个数（键恰好 `0..1`）。
pub const CAP_GRANT_PAYLOAD_V2_FIELD_COUNT: usize = 1;
/// `CapRevokePayloadV2` 字段个数（键恰好 `0..1`）。
pub const CAP_REVOKE_PAYLOAD_V2_FIELD_COUNT: usize = 1;
/// `InviteCreatePayloadV2` 字段个数（键恰好 `0..3`）。
pub const INVITE_CREATE_PAYLOAD_V2_FIELD_COUNT: usize = 3;
/// `DeviceCredentialPayloadV2` 字段个数（键恰好 `0..2`）。
pub const DEVICE_CREDENTIAL_PAYLOAD_V2_FIELD_COUNT: usize = 2;
/// `DeviceRevokePayloadV2` 字段个数（键恰好 `0..1`）。
pub const DEVICE_REVOKE_PAYLOAD_V2_FIELD_COUNT: usize = 1;
/// `RotationBeginPayloadV2` 字段个数（键恰好 `0..4`）。
pub const ROTATION_BEGIN_PAYLOAD_V2_FIELD_COUNT: usize = 4;
/// `RotationAbortPayloadV2` 字段个数（键恰好 `0..1`）。
pub const ROTATION_ABORT_PAYLOAD_V2_FIELD_COUNT: usize = 1;
/// `RotationRestartPayloadV2` 字段个数（键恰好 `0..3`）。
pub const ROTATION_RESTART_PAYLOAD_V2_FIELD_COUNT: usize = 3;
/// `RotationAddRevokesPayloadV2` 字段个数（键恰好 `0..2`）。
pub const ROTATION_ADD_REVOKES_PAYLOAD_V2_FIELD_COUNT: usize = 2;
/// `EpochActivatePayloadV2` 字段个数（键恰好 `0..1`）。
pub const EPOCH_ACTIVATE_PAYLOAD_V2_FIELD_COUNT: usize = 1;
/// `InviteCancelPayloadV2` 字段个数（键恰好 `0..1`）。
pub const INVITE_CANCEL_PAYLOAD_V2_FIELD_COUNT: usize = 1;
/// `RecoveryResetPayloadV2` 字段个数（键恰好 `0..5`）。
pub const RECOVERY_RESET_PAYLOAD_V2_FIELD_COUNT: usize = 5;
/// `RecoveryKeyChangePayloadV2` 字段个数（键恰好 `0..2`）。
pub const RECOVERY_KEY_CHANGE_PAYLOAD_V2_FIELD_COUNT: usize = 2;
/// `ChallengeV2` 字段个数（键恰好 `0..13`）。
pub const CHALLENGE_V2_FIELD_COUNT: usize = 13;

/// cap ID（16B）。
pub type CapId = [u8; 16];
/// 请求 ID（16B，提交幂等键 `(vault_id, request_id)`）。
pub type RequestIdV2 = [u8; 16];
/// grant ID（16B）。
pub type GrantIdV2 = [u8; 16];
/// 轮换 ID（16B）。
pub type RotationIdV2 = [u8; 16];

/// 调用方注入的 SHA-256 实现（sync-proto 不依赖 crypto）。
///
/// 测试用独立实现，sync/relay 用 `sha2`：`|bytes| Sha256::digest(bytes).into()`。
pub type Sha256Fn = fn(&[u8]) -> Sha256;

// —— 内部校验小工具 ——

fn check_version(version: u16) -> Result<(), ProtoError> {
    if version != V2_WIRE_VERSION {
        return Err(ProtoError::UnsupportedVersion(version));
    }
    Ok(())
}

fn check_i64_max(value: u64, name: &str) -> Result<(), ProtoError> {
    if value > i64::MAX as u64 {
        return Err(ProtoError::Invalid(format!("{name} 超过 i64::MAX")));
    }
    Ok(())
}

fn check_nonzero(name: &str, bytes: &[u8]) -> Result<(), ProtoError> {
    if bytes.iter().all(|byte| *byte == 0) {
        return Err(ProtoError::Invalid(format!("{name} 不能全零")));
    }
    Ok(())
}

fn expect_optional_id16(value: CdeValue, field: &str) -> Result<Option<[u8; 16]>, ProtoError> {
    let CdeValue::Bytes(bytes) = value else {
        return Err(ProtoError::Invalid(format!(
            "{field} 必须是 byte string（0 或 16B）"
        )));
    };
    match bytes.len() {
        0 => Ok(None),
        16 => Ok(Some(bytes.try_into().expect("长度16"))),
        other => Err(ProtoError::Invalid(format!(
            "{field} 长度必须是 0 或 16，实际 {other}"
        ))),
    }
}

fn encode_optional_id16(value: Option<[u8; 16]>) -> CdeValue {
    CdeValue::Bytes(value.map(|id| id.to_vec()).unwrap_or_default())
}

fn compare_optional_id(left: Option<&[u8; 16]>, right: Option<&[u8; 16]>) -> Ordering {
    let left = left.map_or(&[][..], |id| &id[..]);
    let right = right.map_or(&[][..], |id| &id[..]);
    left.cmp(right)
}

/// signer 的 canonical 升序键：`(role, device_id, cap_id, pk)`（空 ID 视为空字节串）。
fn compare_signers(left: &SignerV2, right: &SignerV2) -> Ordering {
    left.role
        .as_u8()
        .cmp(&right.role.as_u8())
        .then_with(|| compare_optional_id(left.device_id.as_ref(), right.device_id.as_ref()))
        .then_with(|| compare_optional_id(left.cap_id.as_ref(), right.cap_id.as_ref()))
        .then_with(|| left.pk.cmp(&right.pk))
}

fn record_map(fields: Vec<(u64, CdeValue)>) -> CdeValue {
    CdeValue::Map(fields)
}

// —— scopes ——

/// 管理能力 scope 位（u32）。拒绝零与未知位。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CapScope(u32);

impl CapScope {
    pub const INVITE: u32 = 1;
    pub const APPROVE: u32 = 2;
    pub const REVOKE: u32 = 4;
    pub const CAP_ADMIN: u32 = 8;
    pub const ROTATE: u32 = 16;
    pub const POLICY: u32 = 32;
    /// 全部已知位；任何超出此掩码的位都是未知位。
    pub const ALL: u32 =
        Self::INVITE | Self::APPROVE | Self::REVOKE | Self::CAP_ADMIN | Self::ROTATE | Self::POLICY;

    /// 只接受非零且仅含已知位的组合。
    pub fn new(bits: u32) -> Result<Self, ProtoError> {
        if bits == 0 || bits & !Self::ALL != 0 {
            return Err(ProtoError::Invalid(format!("非法 cap scope 位 {bits:#x}")));
        }
        Ok(Self(bits))
    }

    #[must_use]
    pub fn bits(self) -> u32 {
        self.0
    }

    /// `self` 是否覆盖 `other` 的全部位（门限校验用）。
    #[must_use]
    pub fn contains(self, other: Self) -> bool {
        other.is_subset_of(self)
    }

    /// `self` 是否是 `other` 的子集（“签发 scope 不得超过签发者有效 scope”）。
    #[must_use]
    pub fn is_subset_of(self, other: Self) -> bool {
        self.0 & !other.0 == 0
    }
}

// —— 角色与 kind ——

/// `SignerV2.role`（0..3）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum SignerRole {
    /// 0：ACTIVE 设备用自己的设备钥签名（可带签发它的 cap）。
    Device = 0,
    /// 1：旧 creator 管理根对 Upgrade 记录签名（无 device/cap）。
    CreatorUpgrade = 1,
    /// 2：离线恢复根签名（无 device/cap）。
    OfflineRecovery = 2,
    /// 3：目标设备 PoP（带目标 device，不带 cap）。
    TargetPop = 3,
}

impl SignerRole {
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }
}

impl TryFrom<u8> for SignerRole {
    type Error = ProtoError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Device),
            1 => Ok(Self::CreatorUpgrade),
            2 => Ok(Self::OfflineRecovery),
            3 => Ok(Self::TargetPop),
            other => Err(ProtoError::Invalid(format!("未知 signer role {other}"))),
        }
    }
}

impl From<SignerRole> for u8 {
    fn from(value: SignerRole) -> Self {
        value.as_u8()
    }
}

/// 控制记录 kind（ADR 0015「控制记录 kind」）。
///
/// 0..=14 连续，**15 未分配**（必须拒绝，未知 kind fail-stop），16 = DeviceRightsChange。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum ControlKindV2 {
    UpgradePolicy = 0,
    PolicyReplace = 1,
    CapGrant = 2,
    CapRevoke = 3,
    DeviceApprove = 4,
    DeviceRevoke = 5,
    RotationBegin = 6,
    RotationAbort = 7,
    RotationRestart = 8,
    RotationAddRevokes = 9,
    EpochActivate = 10,
    InviteCreate = 11,
    InviteCancel = 12,
    RecoveryReset = 13,
    RecoveryKeyChange = 14,
    DeviceRightsChange = 16,
}

impl ControlKindV2 {
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }
}

impl TryFrom<u8> for ControlKindV2 {
    type Error = ProtoError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::UpgradePolicy),
            1 => Ok(Self::PolicyReplace),
            2 => Ok(Self::CapGrant),
            3 => Ok(Self::CapRevoke),
            4 => Ok(Self::DeviceApprove),
            5 => Ok(Self::DeviceRevoke),
            6 => Ok(Self::RotationBegin),
            7 => Ok(Self::RotationAbort),
            8 => Ok(Self::RotationRestart),
            9 => Ok(Self::RotationAddRevokes),
            10 => Ok(Self::EpochActivate),
            11 => Ok(Self::InviteCreate),
            12 => Ok(Self::InviteCancel),
            13 => Ok(Self::RecoveryReset),
            14 => Ok(Self::RecoveryKeyChange),
            16 => Ok(Self::DeviceRightsChange),
            other => Err(ProtoError::Invalid(format!(
                "未知或未分配的 V2 控制 kind {other}"
            ))),
        }
    }
}

impl From<ControlKindV2> for u8 {
    fn from(value: ControlKindV2) -> Self {
        value.as_u8()
    }
}

// —— signer ——

/// `SignerV2`：`{0:role, 1:device_id(0/16), 2:cap_id(0/16), 3:pk32}`。
///
/// 字段长度按 ADR 允许 0/16；角色决定必填性（ADR 只写 0/16，这里是结构化解释）：
/// 设备/PoP 必须带 `device_id`，旧 creator 升级与离线恢复不带；只有设备签名可带
/// `cap_id`（cap 只用于授权设备自己的签名钥）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignerV2 {
    pub role: SignerRole,
    pub device_id: Option<DeviceId>,
    pub cap_id: Option<CapId>,
    pub pk: Ed25519PublicKey,
}

impl SignerV2 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        Self::from_cde(decode(bytes)?)
    }

    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.validate()?;
        Ok(record_map(vec![
            (0, CdeValue::UInt(u64::from(self.role.as_u8()))),
            (1, encode_optional_id16(self.device_id)),
            (2, encode_optional_id16(self.cap_id)),
            (3, CdeValue::Bytes(self.pk.to_vec())),
        ]))
    }

    pub fn from_cde(value: CdeValue) -> Result<Self, ProtoError> {
        let fields = into_record_fields(value, SIGNER_V2_FIELD_COUNT)?;
        let role = SignerRole::try_from(expect_u8(fields[0].clone(), "SignerV2.role")?)?;
        let signer = Self {
            role,
            device_id: expect_optional_id16(fields[1].clone(), "SignerV2.device_id")?,
            cap_id: expect_optional_id16(fields[2].clone(), "SignerV2.cap_id")?,
            pk: expect_bytes_n::<32>(fields[3].clone(), "SignerV2.pk")?,
        };
        signer.validate()?;
        Ok(signer)
    }

    /// 角色/ID 长度/ID 与 pk 非全零。
    pub fn validate(&self) -> Result<(), ProtoError> {
        match self.role {
            SignerRole::Device => {
                if self.device_id.is_none() {
                    return Err(ProtoError::Invalid(
                        "SignerV2: 设备签名必须带 device_id".to_owned(),
                    ));
                }
            }
            SignerRole::TargetPop => {
                if self.device_id.is_none() {
                    return Err(ProtoError::Invalid(
                        "SignerV2: PoP 签名必须带目标 device_id".to_owned(),
                    ));
                }
                if self.cap_id.is_some() {
                    return Err(ProtoError::Invalid(
                        "SignerV2: PoP 签名不能带 cap_id".to_owned(),
                    ));
                }
            }
            SignerRole::CreatorUpgrade | SignerRole::OfflineRecovery => {
                if self.device_id.is_some() {
                    return Err(ProtoError::Invalid(
                        "SignerV2: 根签名不能带 device_id".to_owned(),
                    ));
                }
                if self.cap_id.is_some() {
                    return Err(ProtoError::Invalid(
                        "SignerV2: 根签名不能带 cap_id".to_owned(),
                    ));
                }
            }
        }
        if let Some(device_id) = self.device_id {
            check_nonzero("SignerV2.device_id", &device_id)?;
        }
        if let Some(cap_id) = self.cap_id {
            check_nonzero("SignerV2.cap_id", &cap_id)?;
        }
        check_nonzero("SignerV2.pk", &self.pk)?;
        Ok(())
    }
}

// —— ManagementCapV2 ——

/// `ManagementCapV2`（键 `0..13`）：公开授权事实，不含任何私钥/root。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagementCapV2 {
    pub version: u16,
    pub instance_id: RelayInstanceId,
    pub vault_id: VaultId,
    pub cap_id: CapId,
    pub subject_device_id: DeviceId,
    pub subject_ed_pk: Ed25519PublicKey,
    pub scopes: CapScope,
    pub not_before: u64,
    pub expires_at: u64,
    /// 授予提交后的 control rev（ADR：新 cap 的 issued rev 必须等于提交后的值）。
    pub issued_control_rev: u64,
    /// 授予时的 authz rev（仅历史证据，不要求等于当前 rev）。
    pub issued_authz_rev: u64,
    /// 签发时的离线根代（必须等于当前 `authority_rev`）。
    pub issuer_authority_rev: u64,
    /// ADR 0015 固定为 false：cap 不可自行转授。
    pub delegable: bool,
}

impl ManagementCapV2 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        Self::from_cde(decode(bytes)?)
    }

    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.validate()?;
        Ok(record_map(vec![
            (0, CdeValue::UInt(u64::from(self.version))),
            (1, CdeValue::Bytes(self.instance_id.to_vec())),
            (2, CdeValue::Bytes(self.vault_id.to_vec())),
            (3, CdeValue::Bytes(self.cap_id.to_vec())),
            (4, CdeValue::Bytes(self.subject_device_id.to_vec())),
            (5, CdeValue::Bytes(self.subject_ed_pk.to_vec())),
            (6, CdeValue::UInt(u64::from(self.scopes.bits()))),
            (7, CdeValue::UInt(self.not_before)),
            (8, CdeValue::UInt(self.expires_at)),
            (9, CdeValue::UInt(self.issued_control_rev)),
            (10, CdeValue::UInt(self.issued_authz_rev)),
            (11, CdeValue::UInt(self.issuer_authority_rev)),
            (12, CdeValue::Bool(self.delegable)),
        ]))
    }

    pub fn from_cde(value: CdeValue) -> Result<Self, ProtoError> {
        let fields = into_record_fields(value, MANAGEMENT_CAP_V2_FIELD_COUNT)?;
        let cap = Self {
            version: expect_u16(fields[0].clone(), "ManagementCapV2.version")?,
            instance_id: expect_bytes_n::<16>(fields[1].clone(), "ManagementCapV2.instance_id")?,
            vault_id: expect_bytes_n::<16>(fields[2].clone(), "ManagementCapV2.vault_id")?,
            cap_id: expect_bytes_n::<16>(fields[3].clone(), "ManagementCapV2.cap_id")?,
            subject_device_id: expect_bytes_n::<16>(
                fields[4].clone(),
                "ManagementCapV2.subject_device_id",
            )?,
            subject_ed_pk: expect_bytes_n::<32>(
                fields[5].clone(),
                "ManagementCapV2.subject_ed_pk",
            )?,
            scopes: CapScope::new(expect_u32(fields[6].clone(), "ManagementCapV2.scopes")?)?,
            not_before: expect_i64_max(fields[7].clone(), "ManagementCapV2.not_before")?,
            expires_at: expect_i64_max(fields[8].clone(), "ManagementCapV2.expires_at")?,
            issued_control_rev: expect_i64_max(
                fields[9].clone(),
                "ManagementCapV2.issued_control_rev",
            )?,
            issued_authz_rev: expect_i64_max(
                fields[10].clone(),
                "ManagementCapV2.issued_authz_rev",
            )?,
            issuer_authority_rev: expect_i64_max(
                fields[11].clone(),
                "ManagementCapV2.issuer_authority_rev",
            )?,
            delegable: expect_bool(fields[12].clone(), "ManagementCapV2.delegable")?,
        };
        cap.validate()?;
        Ok(cap)
    }

    pub fn validate(&self) -> Result<(), ProtoError> {
        check_version(self.version)?;
        check_nonzero("ManagementCapV2.instance_id", &self.instance_id)?;
        check_nonzero("ManagementCapV2.vault_id", &self.vault_id)?;
        check_nonzero("ManagementCapV2.cap_id", &self.cap_id)?;
        check_nonzero("ManagementCapV2.subject_device_id", &self.subject_device_id)?;
        check_nonzero("ManagementCapV2.subject_ed_pk", &self.subject_ed_pk)?;
        if self.delegable {
            return Err(ProtoError::Invalid(
                "ManagementCapV2.delegable 必须为 false".to_owned(),
            ));
        }
        if self.expires_at <= self.not_before {
            return Err(ProtoError::Invalid(format!(
                "ManagementCapV2.expires_at={} 必须大于 not_before={}",
                self.expires_at, self.not_before
            )));
        }
        for (name, value) in [
            ("ManagementCapV2.not_before", self.not_before),
            ("ManagementCapV2.expires_at", self.expires_at),
            (
                "ManagementCapV2.issued_control_rev",
                self.issued_control_rev,
            ),
            ("ManagementCapV2.issued_authz_rev", self.issued_authz_rev),
            (
                "ManagementCapV2.issuer_authority_rev",
                self.issuer_authority_rev,
            ),
        ] {
            check_i64_max(value, name)?;
        }
        // V2 只在 Upgrade（一次提交初始化 authority/authz rev=1）之后存在，
        // 因此 cap 的三个 rev 从 1 起；0 不可能是合法的“已签发”值。
        for (name, value) in [
            (
                "ManagementCapV2.issued_control_rev",
                self.issued_control_rev,
            ),
            ("ManagementCapV2.issued_authz_rev", self.issued_authz_rev),
            (
                "ManagementCapV2.issuer_authority_rev",
                self.issuer_authority_rev,
            ),
        ] {
            if value == 0 {
                return Err(ProtoError::Invalid(format!("{name} 必须 >0")));
            }
        }
        Ok(())
    }
}

// —— PolicyV2 ——

/// `PolicyV2`（键 `0..4`）：门限与 TTL 上限。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyV2 {
    pub version: u16,
    /// 高危操作门限，ADR 固定 2。
    pub highrisk_threshold: u32,
    /// 最小 quorum，ADR 固定 2。
    pub min_quorum: u32,
    /// cap TTL 上限（秒），≤ 2_592_000。
    pub max_cap_ttl: u64,
    /// 邀请 TTL 上限（秒），≤ 86_400。
    pub max_invite_ttl: u64,
}

impl PolicyV2 {
    pub const HIGHRISK_THRESHOLD: u32 = 2;
    pub const MIN_QUORUM: u32 = 2;

    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        Self::from_cde(decode(bytes)?)
    }

    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.validate()?;
        Ok(record_map(vec![
            (0, CdeValue::UInt(u64::from(self.version))),
            (1, CdeValue::UInt(u64::from(self.highrisk_threshold))),
            (2, CdeValue::UInt(u64::from(self.min_quorum))),
            (3, CdeValue::UInt(self.max_cap_ttl)),
            (4, CdeValue::UInt(self.max_invite_ttl)),
        ]))
    }

    pub fn from_cde(value: CdeValue) -> Result<Self, ProtoError> {
        let fields = into_record_fields(value, POLICY_V2_FIELD_COUNT)?;
        let policy = Self {
            version: expect_u16(fields[0].clone(), "PolicyV2.version")?,
            highrisk_threshold: expect_u32(fields[1].clone(), "PolicyV2.highrisk_threshold")?,
            min_quorum: expect_u32(fields[2].clone(), "PolicyV2.min_quorum")?,
            max_cap_ttl: expect_i64_max(fields[3].clone(), "PolicyV2.max_cap_ttl")?,
            max_invite_ttl: expect_i64_max(fields[4].clone(), "PolicyV2.max_invite_ttl")?,
        };
        policy.validate()?;
        Ok(policy)
    }

    pub fn validate(&self) -> Result<(), ProtoError> {
        check_version(self.version)?;
        if self.highrisk_threshold != Self::HIGHRISK_THRESHOLD {
            return Err(ProtoError::Invalid(format!(
                "PolicyV2.highrisk_threshold 必须为 {}，实际 {}",
                Self::HIGHRISK_THRESHOLD,
                self.highrisk_threshold
            )));
        }
        if self.min_quorum != Self::MIN_QUORUM {
            return Err(ProtoError::Invalid(format!(
                "PolicyV2.min_quorum 必须为 {}，实际 {}",
                Self::MIN_QUORUM,
                self.min_quorum
            )));
        }
        check_i64_max(self.max_cap_ttl, "PolicyV2.max_cap_ttl")?;
        check_i64_max(self.max_invite_ttl, "PolicyV2.max_invite_ttl")?;
        if self.max_cap_ttl > MAX_CAP_TTL_SECS_V2 {
            return Err(ProtoError::Invalid(format!(
                "PolicyV2.max_cap_ttl={} 超过上限 {MAX_CAP_TTL_SECS_V2}",
                self.max_cap_ttl
            )));
        }
        if self.max_invite_ttl > MAX_INVITE_TTL_SECS_V2 {
            return Err(ProtoError::Invalid(format!(
                "PolicyV2.max_invite_ttl={} 超过上限 {MAX_INVITE_TTL_SECS_V2}",
                self.max_invite_ttl
            )));
        }
        Ok(())
    }
}

// —— 唯一提交路径 ——

/// `/v2/vaults/{lowerhex32}/control/submit`（ADR 0015 唯一提交路径）。
///
/// 禁止 query/转义别名；`OperationV2.canonical_path` 必须逐字节等于此值。
#[must_use]
pub fn canonical_submit_path(vault_id: &VaultId) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut path = String::with_capacity(11 + 32 + 15);
    path.push_str("/v2/vaults/");
    for byte in vault_id {
        path.push(HEX[(byte >> 4) as usize] as char);
        path.push(HEX[(byte & 0x0f) as usize] as char);
    }
    path.push_str("/control/submit");
    path
}

fn validate_canonical_path(path: &str, vault_id: &VaultId) -> Result<(), ProtoError> {
    if path.is_empty() || !path.starts_with('/') {
        return Err(ProtoError::Invalid(
            "OperationV2.canonical_path 必须是非空绝对路径".to_owned(),
        ));
    }
    if path.len() > MAX_CANONICAL_PATH_BYTES_V2 {
        return Err(ProtoError::TooLarge);
    }
    if !path.is_ascii() {
        return Err(ProtoError::Invalid(
            "OperationV2.canonical_path 只允许 ASCII".to_owned(),
        ));
    }
    if path.bytes().any(|byte| byte <= 0x20 || byte == 0x7f) {
        return Err(ProtoError::Invalid(
            "OperationV2.canonical_path 含空白/控制字符".to_owned(),
        ));
    }
    // query / fragment / 转义别名 / 反斜杠别名一律拒绝（ADR 0015）。
    if path
        .bytes()
        .any(|byte| matches!(byte, b'?' | b'#' | b'%' | b'\\'))
    {
        return Err(ProtoError::Invalid(
            "OperationV2.canonical_path 不允许 query/fragment/转义/反斜杠".to_owned(),
        ));
    }
    for segment in path.split('/').skip(1) {
        if segment.is_empty() {
            return Err(ProtoError::Invalid(
                "OperationV2.canonical_path 不允许空段".to_owned(),
            ));
        }
        if segment == "." || segment == ".." {
            return Err(ProtoError::Invalid(
                "OperationV2.canonical_path 不允许 . / .. 段".to_owned(),
            ));
        }
    }
    let expected = canonical_submit_path(vault_id);
    if path != expected {
        return Err(ProtoError::Invalid(format!(
            "OperationV2.canonical_path 必须是唯一提交路径 {expected}"
        )));
    }
    Ok(())
}

// —— OperationV2 ——

/// `OperationV2`（键 `0..15`）：一次控制提交的全部已签事实。
///
/// 键 11 的 method 在 wire 上固定为 2（POST），因此不作为结构体字段，
/// 由 [`OperationV2::to_cde`] 写常量、[`OperationV2::from_cde`] 严格校验。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationV2 {
    pub version: u16,
    pub instance_id: RelayInstanceId,
    pub vault_id: VaultId,
    pub kind: ControlKindV2,
    pub request_id: RequestIdV2,
    /// CAS 期望的提交后 control rev = 值 + 1。
    pub expected_control_rev: u64,
    /// 提交前链尾 hash；首条记录允许全零（genesis）。
    pub prev_control_hash: Sha256,
    pub expected_authz_rev: u64,
    /// 当前离线根代。
    pub authority_rev: u64,
    pub signed_at: u64,
    pub not_after: u64,
    pub canonical_path: String,
    /// kind 专属 payload 的 canonical CDE 字节（按 kind 由上层解码）。
    pub payload: Vec<u8>,
    /// 按 `(role, device_id, cap_id, pk)` 严格升序去重。
    pub signers: Vec<SignerV2>,
}

impl OperationV2 {
    /// `h = SHA256(CDE(OperationV2))`（ADR 0015 共同摘要）。
    pub fn digest(&self, hash: Sha256Fn) -> Result<Sha256, ProtoError> {
        Ok(hash(&self.encode()?))
    }

    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        Self::from_cde(decode(bytes)?)
    }

    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.validate()?;
        Ok(record_map(vec![
            (0, CdeValue::UInt(u64::from(self.version))),
            (1, CdeValue::Bytes(self.instance_id.to_vec())),
            (2, CdeValue::Bytes(self.vault_id.to_vec())),
            (3, CdeValue::UInt(u64::from(self.kind.as_u8()))),
            (4, CdeValue::Bytes(self.request_id.to_vec())),
            (5, CdeValue::UInt(self.expected_control_rev)),
            (6, CdeValue::Bytes(self.prev_control_hash.to_vec())),
            (7, CdeValue::UInt(self.expected_authz_rev)),
            (8, CdeValue::UInt(self.authority_rev)),
            (9, CdeValue::UInt(self.signed_at)),
            (10, CdeValue::UInt(self.not_after)),
            (11, CdeValue::UInt(2)),
            (12, CdeValue::Text(self.canonical_path.clone())),
            (13, CdeValue::Bytes(self.payload.clone())),
            (
                14,
                CdeValue::Array(
                    self.signers
                        .iter()
                        .map(SignerV2::to_cde)
                        .collect::<Result<Vec<_>, _>>()?,
                ),
            ),
        ]))
    }

    pub fn from_cde(value: CdeValue) -> Result<Self, ProtoError> {
        let fields = into_record_fields(value, OPERATION_V2_FIELD_COUNT)?;
        let method = expect_u8(fields[11].clone(), "OperationV2.method")?;
        if method != 2 {
            return Err(ProtoError::Invalid(format!(
                "OperationV2.method 必须为 2(POST)，实际 {method}"
            )));
        }
        let signers = expect_array(fields[14].clone(), "OperationV2.signers")?
            .into_iter()
            .map(SignerV2::from_cde)
            .collect::<Result<Vec<_>, _>>()?;
        let operation = Self {
            version: expect_u16(fields[0].clone(), "OperationV2.version")?,
            instance_id: expect_bytes_n::<16>(fields[1].clone(), "OperationV2.instance_id")?,
            vault_id: expect_bytes_n::<16>(fields[2].clone(), "OperationV2.vault_id")?,
            kind: ControlKindV2::try_from(expect_u8(fields[3].clone(), "OperationV2.kind")?)?,
            request_id: expect_bytes_n::<16>(fields[4].clone(), "OperationV2.request_id")?,
            expected_control_rev: expect_i64_max(
                fields[5].clone(),
                "OperationV2.expected_control_rev",
            )?,
            prev_control_hash: expect_bytes_n::<32>(
                fields[6].clone(),
                "OperationV2.prev_control_hash",
            )?,
            expected_authz_rev: expect_i64_max(
                fields[7].clone(),
                "OperationV2.expected_authz_rev",
            )?,
            authority_rev: expect_i64_max(fields[8].clone(), "OperationV2.authority_rev")?,
            signed_at: expect_i64_max(fields[9].clone(), "OperationV2.signed_at")?,
            not_after: expect_i64_max(fields[10].clone(), "OperationV2.not_after")?,
            canonical_path: crate::cde::expect_text(
                fields[12].clone(),
                "OperationV2.canonical_path",
            )?,
            payload: expect_bytes(fields[13].clone(), "OperationV2.payload")?,
            signers,
        };
        operation.validate()?;
        Ok(operation)
    }

    /// 字段/枚举/路径/payload canonical/signer 升序与取值范围。
    pub fn validate(&self) -> Result<(), ProtoError> {
        check_version(self.version)?;
        check_nonzero("OperationV2.instance_id", &self.instance_id)?;
        check_nonzero("OperationV2.vault_id", &self.vault_id)?;
        check_nonzero("OperationV2.request_id", &self.request_id)?;
        validate_canonical_path(&self.canonical_path, &self.vault_id)?;

        if self.payload.is_empty() {
            return Err(ProtoError::Invalid(
                "OperationV2.payload 不能为空".to_owned(),
            ));
        }
        // payload 必须是 canonical CDE；kind 专属字段表由上层按 kind 再解码。
        let payload = decode(&self.payload)?;
        if encode(&payload)? != self.payload {
            return Err(ProtoError::NonCanonical(
                "OperationV2.payload 不是 canonical CDE".to_owned(),
            ));
        }

        for (name, value) in [
            (
                "OperationV2.expected_control_rev",
                self.expected_control_rev,
            ),
            ("OperationV2.expected_authz_rev", self.expected_authz_rev),
            ("OperationV2.authority_rev", self.authority_rev),
            ("OperationV2.signed_at", self.signed_at),
            ("OperationV2.not_after", self.not_after),
        ] {
            check_i64_max(value, name)?;
        }
        if self.not_after < self.signed_at {
            return Err(ProtoError::Invalid(format!(
                "OperationV2.not_after={} 不能早于 signed_at={}",
                self.not_after, self.signed_at
            )));
        }

        if self.signers.is_empty() {
            return Err(ProtoError::Invalid(
                "OperationV2.signers 不能为空".to_owned(),
            ));
        }
        if self.signers.len() > MAX_OPERATION_SIGNERS_V2 {
            return Err(ProtoError::TooLarge);
        }
        for signer in &self.signers {
            signer.validate()?;
        }
        for pair in self.signers.windows(2) {
            if compare_signers(&pair[0], &pair[1]) != Ordering::Less {
                return Err(ProtoError::Invalid(
                    "OperationV2.signers 必须按 (role,device,cap,pk) 严格升序且不重复".to_owned(),
                ));
            }
        }
        Ok(())
    }

    /// 解码 kind 专属 payload 的 CDE 值（字段表由上层按 kind 解释）。
    pub fn payload_cde(&self) -> Result<CdeValue, ProtoError> {
        decode(&self.payload)
    }
}

// —— SignedControlV2 ——

/// `SignedControlV2`：`{0:bytes(OperationV2), 1:[sig64…]}`。
///
/// 签名数组与 signer 一一对应；数量/长度不符即拒绝。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedControlV2 {
    pub operation: OperationV2,
    /// 与 `operation.signers` 一一对应的 64B 签名。
    pub signatures: Vec<[u8; 64]>,
}

impl SignedControlV2 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        Self::from_cde(decode(bytes)?)
    }

    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.validate()?;
        Ok(record_map(vec![
            (0, CdeValue::Bytes(self.operation.encode()?)),
            (
                1,
                CdeValue::Array(
                    self.signatures
                        .iter()
                        .map(|signature| CdeValue::Bytes(signature.to_vec()))
                        .collect(),
                ),
            ),
        ]))
    }

    pub fn from_cde(value: CdeValue) -> Result<Self, ProtoError> {
        let fields = into_record_fields(value, SIGNED_CONTROL_V2_FIELD_COUNT)?;
        let operation_bytes = expect_bytes(fields[0].clone(), "SignedControlV2.operation")?;
        let signatures = expect_array(fields[1].clone(), "SignedControlV2.signatures")?
            .into_iter()
            .map(|value| expect_bytes_n::<64>(value, "SignedControlV2.signatures[]"))
            .collect::<Result<Vec<_>, _>>()?;
        let signed = Self {
            operation: OperationV2::decode(&operation_bytes)?,
            signatures,
        };
        signed.validate()?;
        Ok(signed)
    }

    pub fn validate(&self) -> Result<(), ProtoError> {
        self.operation.validate()?;
        if self.signatures.len() != self.operation.signers.len() {
            return Err(ProtoError::Invalid(format!(
                "SignedControlV2 签名数 {} 与 signer 数 {} 不一致",
                self.signatures.len(),
                self.operation.signers.len()
            )));
        }
        if self.signatures.len() > MAX_OPERATION_SIGNERS_V2 {
            return Err(ProtoError::TooLarge);
        }
        Ok(())
    }

    /// ADR 0015：链 hash 为 `SHA256(prev_hash || SignedControlV2_bytes)`。
    ///
    /// `prev_hash` 对链首记录用 32B 全零。
    pub fn chain_hash(&self, prev_hash: &Sha256, hash: Sha256Fn) -> Result<Sha256, ProtoError> {
        let signed = self.encode()?;
        let mut input = Vec::with_capacity(32 + signed.len());
        input.extend_from_slice(prev_hash);
        input.extend_from_slice(&signed);
        Ok(hash(&input))
    }
}

/// 设备控制签名输入：`MN/v2/control\0 || SHA256(CDE(OperationV2))`。
pub fn control_signing_input(
    operation: &OperationV2,
    hash: Sha256Fn,
) -> Result<Vec<u8>, ProtoError> {
    Ok(domain_digest_input(
        SIGN_V2_CONTROL_DOMAIN,
        &operation.digest(hash)?,
    ))
}

/// 旧 creator 升级签名输入：`MN/v2/upgrade\0 || SHA256(CDE(OperationV2))`。
pub fn upgrade_signing_input(
    operation: &OperationV2,
    hash: Sha256Fn,
) -> Result<Vec<u8>, ProtoError> {
    Ok(domain_digest_input(
        SIGN_V2_UPGRADE_DOMAIN,
        &operation.digest(hash)?,
    ))
}

/// 离线恢复根签名输入：`MN/v2/recover\0 || SHA256(CDE(OperationV2))`。
pub fn recover_signing_input(
    operation: &OperationV2,
    hash: Sha256Fn,
) -> Result<Vec<u8>, ProtoError> {
    Ok(domain_digest_input(
        SIGN_V2_RECOVER_DOMAIN,
        &operation.digest(hash)?,
    ))
}

/// 目标设备 PoP 签名输入：`MN/v2/pop\0 || SHA256(CDE(OperationV2)) || pk32`。
pub fn pop_signing_input(
    operation: &OperationV2,
    pk32: &Ed25519PublicKey,
    hash: Sha256Fn,
) -> Result<Vec<u8>, ProtoError> {
    let digest = operation.digest(hash)?;
    let mut input = Vec::with_capacity(SIGN_V2_POP_DOMAIN.len() + 32 + 32);
    input.extend_from_slice(SIGN_V2_POP_DOMAIN);
    input.extend_from_slice(&digest);
    input.extend_from_slice(pk32);
    Ok(input)
}

/// V2 鉴权签名输入：`MN/v2/auth\0 || challenge`（challenge 是请求里的原字节）。
#[must_use]
pub fn auth_signing_input_v2(challenge: &[u8]) -> Vec<u8> {
    let mut input = Vec::with_capacity(SIGN_AUTH_V2_DOMAIN.len() + challenge.len());
    input.extend_from_slice(SIGN_AUTH_V2_DOMAIN);
    input.extend_from_slice(challenge);
    input
}

fn domain_digest_input(domain: &[u8], digest: &Sha256) -> Vec<u8> {
    let mut input = Vec::with_capacity(domain.len() + 32);
    input.extend_from_slice(domain);
    input.extend_from_slice(digest);
    input
}

// —— SetDigestV2（ADR 0015 S4b 冻结的集合摘要）——

/// 集合摘要的输入项：`(id16, canonical_bytes)`。
///
/// `canonical_bytes` 必须是该对象的**完整** canonical CDE（例如整份 `DeviceCertV2`、
/// 整份 `GrantPackageV2`、整份 `ManagementCapV2`），不是裸拼接字段。
pub type SetDigestEntryV2 = ([u8; 16], Vec<u8>);

/// `SetDigestV2`：`SHA256(CDE(array(bytes)))`，按 id 字节升序去重后编码。
///
/// ADR 0015 附「集合摘要 `SetDigestV2`」：
/// - 输入为 `(id16, canonical_bytes)` 列表；
/// - 按 `id` 字节升序**去重**（同 id 必须是同一份字节，冲突即拒绝）；
/// - 摘要预映像 = `CDE(array(bytes))`，每个元素是该对象的完整 canonical CDE bytes；
/// - 最终 `SHA256` 由调用方注入（sync-proto 无 crypto 依赖）。
///
/// 空集合合法（`CDE([])` = `0x80`），由上层按具体语义决定是否允许。
pub fn set_digest_v2(entries: &[SetDigestEntryV2], hash: Sha256Fn) -> Result<Sha256, ProtoError> {
    let mut sorted: Vec<&SetDigestEntryV2> = entries.iter().collect();
    sorted.sort_by_key(|left| left.0);
    let mut unique: Vec<&SetDigestEntryV2> = Vec::with_capacity(sorted.len());
    for entry in sorted {
        // 每个元素必须自身是 canonical CDE（S4b 冻结：禁止裸拼接）。
        let decoded = decode(&entry.1)?;
        if encode(&decoded)? != entry.1 {
            return Err(ProtoError::NonCanonical(
                "SetDigestV2 元素不是 canonical CDE".to_owned(),
            ));
        }
        match unique.last() {
            Some(previous) if previous.0 == entry.0 => {
                if previous.1 != entry.1 {
                    return Err(ProtoError::Invalid(
                        "SetDigestV2 同一个 id 对应不同 canonical bytes".to_owned(),
                    ));
                }
                // 完全相同的重复项只保留一份。
            }
            _ => unique.push(entry),
        }
    }
    let value = CdeValue::Array(
        unique
            .into_iter()
            .map(|entry| CdeValue::Bytes(entry.1.clone()))
            .collect(),
    );
    Ok(hash(&encode(&value)?))
}

// —— kind 专属 payload（ADR 0015「控制记录 kind」+ S4b 前置冻结）——

/// `UpgradePolicy`（kind 0）payload：`{0:old_manager_rev,1:P,2:fresh_recovery_pk,
/// 3:bootstrap_caps[],4:ACTIVE_manifest[]}`。
///
/// `P` 是 `CDE(PolicyV2)` 原字节；caps 是 `CDE(ManagementCapV2)` 原字节；
/// manifest 是 `CDE(DeviceCertV2)` 原字节（S4b 冻结 §1/§2/§3）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpgradePolicyPayloadV2 {
    /// 当前 v1 管理 rev（`vaults.manager_rev`）。
    pub old_manager_rev: u64,
    pub policy: PolicyV2,
    /// 新离线恢复根公钥（独立随机 seed 的公钥，secret 永不进 relay）。
    pub fresh_recovery_pk: Ed25519PublicKey,
    /// 升级时原子安装的 bootstrap caps。
    pub bootstrap_caps: Vec<ManagementCapV2>,
    /// 全部当前 ACTIVE 设备的完整 `DeviceCertV2`（按 device_id 严格升序）。
    pub active_manifest: Vec<DeviceCertV2>,
}

impl UpgradePolicyPayloadV2 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        Self::from_cde(decode(bytes)?)
    }

    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.validate()?;
        Ok(record_map(vec![
            (0, CdeValue::UInt(self.old_manager_rev)),
            (1, CdeValue::Bytes(self.policy.encode()?)),
            (2, CdeValue::Bytes(self.fresh_recovery_pk.to_vec())),
            (
                3,
                CdeValue::Array(
                    self.bootstrap_caps
                        .iter()
                        .map(|cap| Ok(CdeValue::Bytes(cap.encode()?)))
                        .collect::<Result<Vec<_>, ProtoError>>()?,
                ),
            ),
            (
                4,
                CdeValue::Array(
                    self.active_manifest
                        .iter()
                        .map(|cert| Ok(CdeValue::Bytes(cert.encode()?)))
                        .collect::<Result<Vec<_>, ProtoError>>()?,
                ),
            ),
        ]))
    }

    pub fn from_cde(value: CdeValue) -> Result<Self, ProtoError> {
        let fields = into_record_fields(value, UPGRADE_POLICY_PAYLOAD_V2_FIELD_COUNT)?;
        let policy = PolicyV2::decode(&expect_bytes(
            fields[1].clone(),
            "UpgradePolicyPayloadV2.policy",
        )?)?;
        let bootstrap_caps =
            expect_array(fields[3].clone(), "UpgradePolicyPayloadV2.bootstrap_caps")?
                .into_iter()
                .map(|item| {
                    ManagementCapV2::decode(&expect_bytes(
                        item,
                        "UpgradePolicyPayloadV2.bootstrap_caps[]",
                    )?)
                })
                .collect::<Result<Vec<_>, ProtoError>>()?;
        let active_manifest =
            expect_array(fields[4].clone(), "UpgradePolicyPayloadV2.ACTIVE_manifest")?
                .into_iter()
                .map(|item| {
                    DeviceCertV2::decode(&expect_bytes(
                        item,
                        "UpgradePolicyPayloadV2.ACTIVE_manifest[]",
                    )?)
                })
                .collect::<Result<Vec<_>, ProtoError>>()?;
        let payload = Self {
            old_manager_rev: expect_i64_max(
                fields[0].clone(),
                "UpgradePolicyPayloadV2.old_manager_rev",
            )?,
            policy,
            fresh_recovery_pk: expect_bytes_n::<32>(
                fields[2].clone(),
                "UpgradePolicyPayloadV2.fresh_recovery_pk",
            )?,
            bootstrap_caps,
            active_manifest,
        };
        payload.validate()?;
        Ok(payload)
    }

    pub fn validate(&self) -> Result<(), ProtoError> {
        if self.old_manager_rev == 0 {
            return Err(ProtoError::Invalid(
                "UpgradePolicyPayloadV2.old_manager_rev 必须 >0".to_owned(),
            ));
        }
        check_i64_max(
            self.old_manager_rev,
            "UpgradePolicyPayloadV2.old_manager_rev",
        )?;
        self.policy.validate()?;
        check_nonzero(
            "UpgradePolicyPayloadV2.fresh_recovery_pk",
            &self.fresh_recovery_pk,
        )?;
        if self.bootstrap_caps.len() > MAX_BOOTSTRAP_CAPS_V2 {
            return Err(ProtoError::TooLarge);
        }
        let mut cap_ids: Vec<&[u8; 16]> = Vec::with_capacity(self.bootstrap_caps.len());
        for cap in &self.bootstrap_caps {
            cap.validate()?;
            if cap_ids.contains(&&cap.cap_id) {
                return Err(ProtoError::Invalid(
                    "UpgradePolicyPayloadV2.bootstrap_caps 含重复 cap_id".to_owned(),
                ));
            }
            cap_ids.push(&cap.cap_id);
        }
        if self.active_manifest.is_empty() {
            return Err(ProtoError::Invalid(
                "UpgradePolicyPayloadV2.ACTIVE_manifest 不能为空".to_owned(),
            ));
        }
        for cert in &self.active_manifest {
            cert.validate()?;
        }
        for pair in self.active_manifest.windows(2) {
            if pair[0].device_id >= pair[1].device_id {
                return Err(ProtoError::Invalid(
                    "UpgradePolicyPayloadV2.ACTIVE_manifest 必须按 device_id 严格升序且不重复"
                        .to_owned(),
                ));
            }
        }
        Ok(())
    }
}

/// `CapGrant`（kind 2）payload：`{0: bytes(ManagementCapV2 canonical CDE)}`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapGrantPayloadV2 {
    pub cap: ManagementCapV2,
}

impl CapGrantPayloadV2 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        Self::from_cde(decode(bytes)?)
    }

    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.cap.validate()?;
        Ok(record_map(vec![(0, CdeValue::Bytes(self.cap.encode()?))]))
    }

    pub fn from_cde(value: CdeValue) -> Result<Self, ProtoError> {
        let fields = into_record_fields(value, CAP_GRANT_PAYLOAD_V2_FIELD_COUNT)?;
        let cap =
            ManagementCapV2::decode(&expect_bytes(fields[0].clone(), "CapGrantPayloadV2.cap")?)?;
        Ok(Self { cap })
    }
}

/// `CapRevoke`（kind 3）payload：`{0: cap_id16}`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapRevokePayloadV2 {
    pub cap_id: CapId,
}

impl CapRevokePayloadV2 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        Self::from_cde(decode(bytes)?)
    }

    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        check_nonzero("CapRevokePayloadV2.cap_id", &self.cap_id)?;
        Ok(record_map(vec![(0, CdeValue::Bytes(self.cap_id.to_vec()))]))
    }

    pub fn from_cde(value: CdeValue) -> Result<Self, ProtoError> {
        let fields = into_record_fields(value, CAP_REVOKE_PAYLOAD_V2_FIELD_COUNT)?;
        let payload = Self {
            cap_id: expect_bytes_n::<16>(fields[0].clone(), "CapRevokePayloadV2.cap_id")?,
        };
        payload.to_cde()?;
        Ok(payload)
    }
}

/// `InviteCreate`（kind 11）payload：`{0:invite_id,1:token_hash32,2:expires_at}`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InviteCreatePayloadV2 {
    pub invite_id: [u8; 16],
    /// `SHA256(T)`；relay 永不落明文邀请令牌。
    pub token_hash: Sha256,
    pub expires_at: u64,
}

impl InviteCreatePayloadV2 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        Self::from_cde(decode(bytes)?)
    }

    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.validate()?;
        Ok(record_map(vec![
            (0, CdeValue::Bytes(self.invite_id.to_vec())),
            (1, CdeValue::Bytes(self.token_hash.to_vec())),
            (2, CdeValue::UInt(self.expires_at)),
        ]))
    }

    pub fn from_cde(value: CdeValue) -> Result<Self, ProtoError> {
        let fields = into_record_fields(value, INVITE_CREATE_PAYLOAD_V2_FIELD_COUNT)?;
        let payload = Self {
            invite_id: expect_bytes_n::<16>(fields[0].clone(), "InviteCreatePayloadV2.invite_id")?,
            token_hash: expect_bytes_n::<32>(
                fields[1].clone(),
                "InviteCreatePayloadV2.token_hash",
            )?,
            expires_at: expect_i64_max(fields[2].clone(), "InviteCreatePayloadV2.expires_at")?,
        };
        payload.validate()?;
        Ok(payload)
    }

    pub fn validate(&self) -> Result<(), ProtoError> {
        check_nonzero("InviteCreatePayloadV2.invite_id", &self.invite_id)?;
        check_nonzero("InviteCreatePayloadV2.token_hash", &self.token_hash)?;
        if self.expires_at == 0 {
            return Err(ProtoError::Invalid(
                "InviteCreatePayloadV2.expires_at 必须 >0".to_owned(),
            ));
        }
        check_i64_max(self.expires_at, "InviteCreatePayloadV2.expires_at")?;
        Ok(())
    }
}

// —— DeviceApprove / DeviceRightsChange payload（ADR 0015 kind 4/16）——

/// `DeviceApprove`（kind 4）/ `DeviceRightsChange`（kind 16）的 payload：
/// `{0: cert_bytes(DeviceCertV2), 1: grant_packages[]}`。
///
/// ADR 0015 只把两种 kind 的 payload 写成同一个形状，wire 上因此**只有一种**编码：
/// 键 0 是整份 `DeviceCertV2` 的 canonical CDE bytes（不是嵌套 map，与
/// `UpgradePolicyPayloadV2` 的 manifest / `CapGrantPayloadV2` 的 cap 同口径），
/// 键 1 是「每个元素为整份 `GrantPackageV2` canonical CDE bytes」的数组。
///
/// 证书与 grant 包都被两签 `OperationV2` 的摘要承诺（ADR：「证书、AAD、enc、完整
/// ciphertext 都由两签 Operation 承诺，不另造单签旁路」），因此换包/换证书必然验签失败。
///
/// 本类型只做**结构**校验：证书与每个 grant 自身合法、grant 的 `grant_id` 两两不同
/// （对象身份不能有二义）、`epoch` 两两不同（`device_grants` 的 PK 是
/// `(vault_id, epoch, device_id)`）。证书与 grant 之间的语义一致性（recipient/hpke/
/// authority/issued rev/当前 epoch）由 relay 按 DB 事实校验。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceCredentialPayloadV2 {
    pub cert: DeviceCertV2,
    pub grants: Vec<GrantPackageV2>,
}

/// kind 4 `DeviceApprove` 的 payload（与 kind 16 同一 wire 形状）。
pub type DeviceApprovePayloadV2 = DeviceCredentialPayloadV2;
/// kind 16 `DeviceRightsChange` 的 payload（与 kind 4 同一 wire 形状）。
pub type DeviceRightsChangePayloadV2 = DeviceCredentialPayloadV2;

impl DeviceCredentialPayloadV2 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        Self::from_cde(decode(bytes)?)
    }

    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.validate()?;
        Ok(record_map(vec![
            (0, CdeValue::Bytes(self.cert.encode()?)),
            (
                1,
                CdeValue::Array(
                    self.grants
                        .iter()
                        .map(|grant| Ok(CdeValue::Bytes(grant.encode()?)))
                        .collect::<Result<Vec<_>, ProtoError>>()?,
                ),
            ),
        ]))
    }

    pub fn from_cde(value: CdeValue) -> Result<Self, ProtoError> {
        let fields = into_record_fields(value, DEVICE_CREDENTIAL_PAYLOAD_V2_FIELD_COUNT)?;
        let cert = DeviceCertV2::decode(&expect_bytes(
            fields[0].clone(),
            "DeviceCredentialPayloadV2.cert",
        )?)?;
        let grants = expect_array(fields[1].clone(), "DeviceCredentialPayloadV2.grants")?
            .into_iter()
            .map(|item| {
                GrantPackageV2::decode(&expect_bytes(item, "DeviceCredentialPayloadV2.grants[]")?)
            })
            .collect::<Result<Vec<_>, ProtoError>>()?;
        let payload = Self { cert, grants };
        payload.validate()?;
        Ok(payload)
    }

    pub fn validate(&self) -> Result<(), ProtoError> {
        self.cert.validate()?;
        if self.grants.is_empty() {
            return Err(ProtoError::Invalid(
                "DeviceCredentialPayloadV2.grants 不能为空".to_owned(),
            ));
        }
        if self.grants.len() > MAX_GRANT_PACKAGES_V2 {
            return Err(ProtoError::TooLarge);
        }
        let mut grant_ids: Vec<&GrantIdV2> = Vec::with_capacity(self.grants.len());
        let mut epochs: Vec<u64> = Vec::with_capacity(self.grants.len());
        for grant in &self.grants {
            grant.validate()?;
            if grant_ids.contains(&&grant.aad.grant_id) {
                return Err(ProtoError::Invalid(
                    "DeviceCredentialPayloadV2.grants 含重复 grant_id".to_owned(),
                ));
            }
            grant_ids.push(&grant.aad.grant_id);
            if epochs.contains(&grant.aad.epoch) {
                return Err(ProtoError::Invalid(
                    "DeviceCredentialPayloadV2.grants 含重复 epoch".to_owned(),
                ));
            }
            epochs.push(grant.aad.epoch);
        }
        Ok(())
    }
}

// —— DeviceRevoke payload（ADR 0015 kind 5）——

/// `DeviceRevoke`（kind 5）payload：`{0: array(bytes16)}`（升序、去重的设备 ID）。
///
/// S4b 冻结 §1：`ids` 是「元素为 16B byte string 的数组」。实现要求**严格升序**
/// （排序 + 去重同时成立），这样集合摘要/链上字节唯一，重复目标不会产生两种编码。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceRevokePayloadV2 {
    pub device_ids: Vec<DeviceId>,
}

impl DeviceRevokePayloadV2 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        Self::from_cde(decode(bytes)?)
    }

    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.validate()?;
        Ok(record_map(vec![(
            0,
            CdeValue::Array(
                self.device_ids
                    .iter()
                    .map(|device_id| CdeValue::Bytes(device_id.to_vec()))
                    .collect(),
            ),
        )]))
    }

    pub fn from_cde(value: CdeValue) -> Result<Self, ProtoError> {
        let fields = into_record_fields(value, DEVICE_REVOKE_PAYLOAD_V2_FIELD_COUNT)?;
        let device_ids = expect_array(fields[0].clone(), "DeviceRevokePayloadV2.device_ids")?
            .into_iter()
            .map(|item| expect_bytes_n::<16>(item, "DeviceRevokePayloadV2.device_ids[]"))
            .collect::<Result<Vec<_>, ProtoError>>()?;
        let payload = Self { device_ids };
        payload.validate()?;
        Ok(payload)
    }

    pub fn validate(&self) -> Result<(), ProtoError> {
        if self.device_ids.is_empty() {
            return Err(ProtoError::Invalid(
                "DeviceRevokePayloadV2.device_ids 不能为空".to_owned(),
            ));
        }
        if self.device_ids.len() > MAX_DEVICES {
            return Err(ProtoError::TooLarge);
        }
        for device_id in &self.device_ids {
            check_nonzero("DeviceRevokePayloadV2.device_ids[]", device_id)?;
        }
        for pair in self.device_ids.windows(2) {
            if pair[0] >= pair[1] {
                return Err(ProtoError::Invalid(
                    "DeviceRevokePayloadV2.device_ids 必须严格升序且不重复".to_owned(),
                ));
            }
        }
        Ok(())
    }
}

// —— 轮换控制（ADR 0015 kind 6/7/8/9/10）——

/// 校验「严格升序去重」的设备 ID 数组（集合摘要/链上字节唯一）。
///
/// 允许为空（`RotationBegin`/`RotationRestart` 的撤销列表可以空），非空时元素必须
/// 非全零、严格升序；`max` 是协议上限（`MAX_DEVICES`）。
fn validate_sorted_device_ids(
    name: &str,
    device_ids: &[DeviceId],
    max: usize,
) -> Result<(), ProtoError> {
    if device_ids.len() > max {
        return Err(ProtoError::TooLarge);
    }
    for device_id in device_ids {
        check_nonzero(name, device_id)?;
    }
    for pair in device_ids.windows(2) {
        if pair[0] >= pair[1] {
            return Err(ProtoError::Invalid(format!("{name} 必须严格升序且不重复")));
        }
    }
    Ok(())
}

fn device_id_array(device_ids: &[DeviceId]) -> CdeValue {
    CdeValue::Array(
        device_ids
            .iter()
            .map(|device_id| CdeValue::Bytes(device_id.to_vec()))
            .collect(),
    )
}

fn decode_device_id_array(value: CdeValue, name: &str) -> Result<Vec<DeviceId>, ProtoError> {
    expect_array(value, name)?
        .into_iter()
        .map(|item| expect_bytes_n::<16>(item, name))
        .collect()
}

/// `RotationBegin`（kind 6）payload：`{0:rotation_id,1:from_epoch,2:to_epoch,3:revoke_ids[]}`。
///
/// 两签 `ROTATE`；`revoke_ids` 非空时**每一位**签名者还必须具备 `REVOKE`（relay 校验）。
/// `to_epoch` 由客户端指定（必须是本 vault 从未分配过的更高 epoch），relay 只做
/// 「不复用候选」的持久事实校验。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RotationBeginPayloadV2 {
    pub rotation_id: RotationIdV2,
    pub from_epoch: u64,
    pub to_epoch: u64,
    pub revoke_ids: Vec<DeviceId>,
}

impl RotationBeginPayloadV2 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        Self::from_cde(decode(bytes)?)
    }

    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.validate()?;
        Ok(record_map(vec![
            (0, CdeValue::Bytes(self.rotation_id.to_vec())),
            (1, CdeValue::UInt(self.from_epoch)),
            (2, CdeValue::UInt(self.to_epoch)),
            (3, device_id_array(&self.revoke_ids)),
        ]))
    }

    pub fn from_cde(value: CdeValue) -> Result<Self, ProtoError> {
        let fields = into_record_fields(value, ROTATION_BEGIN_PAYLOAD_V2_FIELD_COUNT)?;
        let payload = Self {
            rotation_id: expect_bytes_n::<16>(
                fields[0].clone(),
                "RotationBeginPayloadV2.rotation_id",
            )?,
            from_epoch: expect_i64_max(fields[1].clone(), "RotationBeginPayloadV2.from_epoch")?,
            to_epoch: expect_i64_max(fields[2].clone(), "RotationBeginPayloadV2.to_epoch")?,
            revoke_ids: decode_device_id_array(
                fields[3].clone(),
                "RotationBeginPayloadV2.revoke_ids",
            )?,
        };
        payload.validate()?;
        Ok(payload)
    }

    pub fn validate(&self) -> Result<(), ProtoError> {
        check_nonzero("RotationBeginPayloadV2.rotation_id", &self.rotation_id)?;
        check_i64_max(self.from_epoch, "RotationBeginPayloadV2.from_epoch")?;
        check_i64_max(self.to_epoch, "RotationBeginPayloadV2.to_epoch")?;
        if self.from_epoch == 0 {
            return Err(ProtoError::Invalid(
                "RotationBeginPayloadV2.from_epoch 必须 >0".to_owned(),
            ));
        }
        if self.to_epoch <= self.from_epoch {
            return Err(ProtoError::Invalid(format!(
                "RotationBeginPayloadV2.to_epoch={} 必须大于 from_epoch={}",
                self.to_epoch, self.from_epoch
            )));
        }
        validate_sorted_device_ids(
            "RotationBeginPayloadV2.revoke_ids[]",
            &self.revoke_ids,
            MAX_DEVICES,
        )
    }
}

/// `RotationAbort`（kind 7）payload：`{0:rotation_id}`。
///
/// 两签 `ROTATE`；abort **不复活**已吊销的 epoch（relay 按 `epochs.revoked_once` 判定）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RotationAbortPayloadV2 {
    pub rotation_id: RotationIdV2,
}

impl RotationAbortPayloadV2 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        Self::from_cde(decode(bytes)?)
    }

    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        check_nonzero("RotationAbortPayloadV2.rotation_id", &self.rotation_id)?;
        Ok(record_map(vec![(
            0,
            CdeValue::Bytes(self.rotation_id.to_vec()),
        )]))
    }

    pub fn from_cde(value: CdeValue) -> Result<Self, ProtoError> {
        let fields = into_record_fields(value, ROTATION_ABORT_PAYLOAD_V2_FIELD_COUNT)?;
        let payload = Self {
            rotation_id: expect_bytes_n::<16>(
                fields[0].clone(),
                "RotationAbortPayloadV2.rotation_id",
            )?,
        };
        payload.to_cde()?;
        Ok(payload)
    }
}

/// `RotationRestart`（kind 8）payload：`{0:rotation_id,1:new_to_epoch,2:extra_ids[]}`。
///
/// 两签 `ROTATE`；`extra_ids` 非空时每一位签名者还必须具备 `REVOKE`。旧候选 epoch
/// 永久作废（不得复用），新候选 epoch 必须是本 vault 从未分配过的值。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RotationRestartPayloadV2 {
    pub rotation_id: RotationIdV2,
    pub new_to_epoch: u64,
    pub extra_ids: Vec<DeviceId>,
}

impl RotationRestartPayloadV2 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        Self::from_cde(decode(bytes)?)
    }

    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.validate()?;
        Ok(record_map(vec![
            (0, CdeValue::Bytes(self.rotation_id.to_vec())),
            (1, CdeValue::UInt(self.new_to_epoch)),
            (2, device_id_array(&self.extra_ids)),
        ]))
    }

    pub fn from_cde(value: CdeValue) -> Result<Self, ProtoError> {
        let fields = into_record_fields(value, ROTATION_RESTART_PAYLOAD_V2_FIELD_COUNT)?;
        let payload = Self {
            rotation_id: expect_bytes_n::<16>(
                fields[0].clone(),
                "RotationRestartPayloadV2.rotation_id",
            )?,
            new_to_epoch: expect_i64_max(
                fields[1].clone(),
                "RotationRestartPayloadV2.new_to_epoch",
            )?,
            extra_ids: decode_device_id_array(
                fields[2].clone(),
                "RotationRestartPayloadV2.extra_ids",
            )?,
        };
        payload.validate()?;
        Ok(payload)
    }

    pub fn validate(&self) -> Result<(), ProtoError> {
        check_nonzero("RotationRestartPayloadV2.rotation_id", &self.rotation_id)?;
        check_i64_max(self.new_to_epoch, "RotationRestartPayloadV2.new_to_epoch")?;
        if self.new_to_epoch == 0 {
            return Err(ProtoError::Invalid(
                "RotationRestartPayloadV2.new_to_epoch 必须 >0".to_owned(),
            ));
        }
        validate_sorted_device_ids(
            "RotationRestartPayloadV2.extra_ids[]",
            &self.extra_ids,
            MAX_DEVICES,
        )
    }
}

/// `RotationAddRevokes`（kind 9）payload：`{0:rotation_id,1:ids[]}`。
///
/// 两签 `ROTATE` + 每一位签名者 `REVOKE`；`ids` 必须非空且严格升序去重。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RotationAddRevokesPayloadV2 {
    pub rotation_id: RotationIdV2,
    pub device_ids: Vec<DeviceId>,
}

impl RotationAddRevokesPayloadV2 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        Self::from_cde(decode(bytes)?)
    }

    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.validate()?;
        Ok(record_map(vec![
            (0, CdeValue::Bytes(self.rotation_id.to_vec())),
            (1, device_id_array(&self.device_ids)),
        ]))
    }

    pub fn from_cde(value: CdeValue) -> Result<Self, ProtoError> {
        let fields = into_record_fields(value, ROTATION_ADD_REVOKES_PAYLOAD_V2_FIELD_COUNT)?;
        let payload = Self {
            rotation_id: expect_bytes_n::<16>(
                fields[0].clone(),
                "RotationAddRevokesPayloadV2.rotation_id",
            )?,
            device_ids: decode_device_id_array(
                fields[1].clone(),
                "RotationAddRevokesPayloadV2.device_ids",
            )?,
        };
        payload.validate()?;
        Ok(payload)
    }

    pub fn validate(&self) -> Result<(), ProtoError> {
        check_nonzero("RotationAddRevokesPayloadV2.rotation_id", &self.rotation_id)?;
        if self.device_ids.is_empty() {
            return Err(ProtoError::Invalid(
                "RotationAddRevokesPayloadV2.device_ids 不能为空".to_owned(),
            ));
        }
        validate_sorted_device_ids(
            "RotationAddRevokesPayloadV2.device_ids[]",
            &self.device_ids,
            MAX_DEVICES,
        )
    }
}

/// `EpochActivate`（kind 10）payload：`{0: bytes(ActivationV2)}`。
///
/// 与 `CapGrant`/`UpgradePolicy` 的 manifest 同口径：键 0 是**整份** `ActivationV2`
/// canonical CDE 的 byte string（不是嵌套 map）。集合摘要由客户端取包重算，relay 只按
/// DB 事实复算并逐字段比对（ADR：「客户端必须取包重算，不能只信 relay 摘要」）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpochActivatePayloadV2 {
    pub activation: ActivationV2,
}

impl EpochActivatePayloadV2 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        Self::from_cde(decode(bytes)?)
    }

    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.activation.validate()?;
        Ok(record_map(vec![(
            0,
            CdeValue::Bytes(self.activation.encode()?),
        )]))
    }

    pub fn from_cde(value: CdeValue) -> Result<Self, ProtoError> {
        let fields = into_record_fields(value, EPOCH_ACTIVATE_PAYLOAD_V2_FIELD_COUNT)?;
        let activation = ActivationV2::decode(&expect_bytes(
            fields[0].clone(),
            "EpochActivatePayloadV2.activation",
        )?)?;
        Ok(Self { activation })
    }
}

// —— InviteCancel（ADR 0015 kind 12）——

/// `InviteCancel`（kind 12）payload：`{0: invite_id}`。
///
/// 一台 `INVITE` cap（或签发该邀请的 cap 本人）即可取消；只对**未兑换**邀请成立，
/// 已兑换/已用是永久事实（relay 拒绝，不伪造 `used_at`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InviteCancelPayloadV2 {
    pub invite_id: [u8; 16],
}

impl InviteCancelPayloadV2 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        Self::from_cde(decode(bytes)?)
    }

    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        check_nonzero("InviteCancelPayloadV2.invite_id", &self.invite_id)?;
        Ok(record_map(vec![(
            0,
            CdeValue::Bytes(self.invite_id.to_vec()),
        )]))
    }

    pub fn from_cde(value: CdeValue) -> Result<Self, ProtoError> {
        let fields = into_record_fields(value, INVITE_CANCEL_PAYLOAD_V2_FIELD_COUNT)?;
        let payload = Self {
            invite_id: expect_bytes_n::<16>(fields[0].clone(), "InviteCancelPayloadV2.invite_id")?,
        };
        payload.to_cde()?;
        Ok(payload)
    }
}

// —— RecoveryReset / RecoveryKeyChange（ADR 0015 kind 13/14）——

/// 新代 bootstrap cap 的公共结构校验（Reset/KeyChange 共用）。
///
/// 只校验**结构**：cap 自身合法、cap_id 两两不同、对象身份（instance/vault）由 relay
/// 按 DB 事实校验；`issued_control_rev`/`issued_authz_rev`/`issuer_authority_rev` 必须
/// 等于**提交后**的值（relay 校验，wire 只承诺字节）。
fn validate_bootstrap_caps_structure(caps: &[ManagementCapV2]) -> Result<(), ProtoError> {
    if caps.is_empty() {
        return Err(ProtoError::Invalid(
            "bootstrap_caps 不能为空（旧代 cap 会随 authority_rev+1 全部失效）".to_owned(),
        ));
    }
    if caps.len() > MAX_BOOTSTRAP_CAPS_V2 {
        return Err(ProtoError::TooLarge);
    }
    let mut cap_ids: Vec<&CapId> = Vec::with_capacity(caps.len());
    for cap in caps {
        cap.validate()?;
        if cap_ids.contains(&&cap.cap_id) {
            return Err(ProtoError::Invalid(
                "bootstrap_caps 含重复 cap_id".to_owned(),
            ));
        }
        cap_ids.push(&cap.cap_id);
    }
    Ok(())
}

fn bootstrap_caps_cde(caps: &[ManagementCapV2]) -> Result<CdeValue, ProtoError> {
    Ok(CdeValue::Array(
        caps.iter()
            .map(|cap| Ok(CdeValue::Bytes(cap.encode()?)))
            .collect::<Result<Vec<_>, ProtoError>>()?,
    ))
}

fn decode_bootstrap_caps(value: CdeValue, name: &str) -> Result<Vec<ManagementCapV2>, ProtoError> {
    expect_array(value, name)?
        .into_iter()
        .map(|item| ManagementCapV2::decode(&expect_bytes(item, name)?))
        .collect()
}

/// `RecoveryReset`（kind 13）payload：
/// `{0: policy_bytes(PolicyV2), 1: fresh_recovery_pk, 2: bootstrap_caps[],
///   3: device_adds[], 4: revoke_ids[]}`。
///
/// 授权 = 离线恢复根（**当前** `recovery_pub`，`recover` 域）**加上**新恢复钥 PoP
/// （新钥对同一摘要的 `recover` 域签名）；设备 adds 每一位还须目标设备 PoP（role3）。
/// 一次提交原子安装新 policy/新代 caps/设备增删，并把 `authority_rev+1`
/// （旧代 cap 与管理材料全部失效）。
///
/// 不足两台 ACTIVE 设备时进入 `recovery_only`（只许再 Reset，普通管理写 403）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryResetPayloadV2 {
    pub policy: PolicyV2,
    /// 新离线恢复根公钥（独立随机 seed；secret 永不进 relay）。
    pub fresh_recovery_pk: Ed25519PublicKey,
    pub bootstrap_caps: Vec<ManagementCapV2>,
    /// 设备 adds：每项是「证书 + 该设备当前 epoch 的 grant 包」。
    pub device_adds: Vec<DeviceCredentialPayloadV2>,
    /// 要撤销的设备（严格升序去重；已 revoked 的重复项幂等）。
    pub revoke_ids: Vec<DeviceId>,
}

impl RecoveryResetPayloadV2 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        Self::from_cde(decode(bytes)?)
    }

    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.validate()?;
        Ok(record_map(vec![
            (0, CdeValue::Bytes(self.policy.encode()?)),
            (1, CdeValue::Bytes(self.fresh_recovery_pk.to_vec())),
            (2, bootstrap_caps_cde(&self.bootstrap_caps)?),
            (
                3,
                CdeValue::Array(
                    self.device_adds
                        .iter()
                        .map(|add| Ok(CdeValue::Bytes(add.encode()?)))
                        .collect::<Result<Vec<_>, ProtoError>>()?,
                ),
            ),
            (4, device_id_array(&self.revoke_ids)),
        ]))
    }

    pub fn from_cde(value: CdeValue) -> Result<Self, ProtoError> {
        let fields = into_record_fields(value, RECOVERY_RESET_PAYLOAD_V2_FIELD_COUNT)?;
        let policy = PolicyV2::decode(&expect_bytes(
            fields[0].clone(),
            "RecoveryResetPayloadV2.policy",
        )?)?;
        let fresh_recovery_pk = expect_bytes_n::<32>(
            fields[1].clone(),
            "RecoveryResetPayloadV2.fresh_recovery_pk",
        )?;
        let bootstrap_caps =
            decode_bootstrap_caps(fields[2].clone(), "RecoveryResetPayloadV2.bootstrap_caps[]")?;
        let device_adds = expect_array(fields[3].clone(), "RecoveryResetPayloadV2.device_adds")?
            .into_iter()
            .map(|item| {
                DeviceCredentialPayloadV2::decode(&expect_bytes(
                    item,
                    "RecoveryResetPayloadV2.device_adds[]",
                )?)
            })
            .collect::<Result<Vec<_>, ProtoError>>()?;
        let revoke_ids =
            decode_device_id_array(fields[4].clone(), "RecoveryResetPayloadV2.revoke_ids")?;
        let payload = Self {
            policy,
            fresh_recovery_pk,
            bootstrap_caps,
            device_adds,
            revoke_ids,
        };
        payload.validate()?;
        Ok(payload)
    }

    pub fn validate(&self) -> Result<(), ProtoError> {
        self.policy.validate()?;
        check_nonzero(
            "RecoveryResetPayloadV2.fresh_recovery_pk",
            &self.fresh_recovery_pk,
        )?;
        validate_bootstrap_caps_structure(&self.bootstrap_caps)?;
        if self.device_adds.len() > MAX_DEVICES {
            return Err(ProtoError::TooLarge);
        }
        let mut device_ids: Vec<&DeviceId> = Vec::with_capacity(self.device_adds.len());
        for add in &self.device_adds {
            add.validate()?;
            if device_ids.contains(&&add.cert.device_id) {
                return Err(ProtoError::Invalid(
                    "RecoveryResetPayloadV2.device_adds 含重复 device_id".to_owned(),
                ));
            }
            device_ids.push(&add.cert.device_id);
        }
        validate_sorted_device_ids(
            "RecoveryResetPayloadV2.revoke_ids[]",
            &self.revoke_ids,
            MAX_DEVICES,
        )?;
        // 同一台设备不能既 add 又 revoke（效果互相抵消，链上事实必须唯一）。
        for device_id in &self.revoke_ids {
            if device_ids.contains(&device_id) {
                return Err(ProtoError::Invalid(
                    "RecoveryResetPayloadV2 同一设备不能同时 add 与 revoke".to_owned(),
                ));
            }
        }
        Ok(())
    }
}

/// `RecoveryKeyChange`（kind 14）payload：`{0: fresh_recovery_pk, 1: bootstrap_caps[]}`。
///
/// 授权 = 两台不同 ACTIVE 设备的 `POLICY` cap **加上**新恢复钥 PoP。只换恢复钥
/// （不改 policy/成员集合），但 `authority_rev+1` 让旧代 cap 全部失效，因此必须在
/// 同一提交里原子安装新代 bootstrap caps（否则管理面死锁）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryKeyChangePayloadV2 {
    pub fresh_recovery_pk: Ed25519PublicKey,
    pub bootstrap_caps: Vec<ManagementCapV2>,
}

impl RecoveryKeyChangePayloadV2 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        Self::from_cde(decode(bytes)?)
    }

    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.validate()?;
        Ok(record_map(vec![
            (0, CdeValue::Bytes(self.fresh_recovery_pk.to_vec())),
            (1, bootstrap_caps_cde(&self.bootstrap_caps)?),
        ]))
    }

    pub fn from_cde(value: CdeValue) -> Result<Self, ProtoError> {
        let fields = into_record_fields(value, RECOVERY_KEY_CHANGE_PAYLOAD_V2_FIELD_COUNT)?;
        let payload = Self {
            fresh_recovery_pk: expect_bytes_n::<32>(
                fields[0].clone(),
                "RecoveryKeyChangePayloadV2.fresh_recovery_pk",
            )?,
            bootstrap_caps: decode_bootstrap_caps(
                fields[1].clone(),
                "RecoveryKeyChangePayloadV2.bootstrap_caps[]",
            )?,
        };
        payload.validate()?;
        Ok(payload)
    }

    pub fn validate(&self) -> Result<(), ProtoError> {
        check_nonzero(
            "RecoveryKeyChangePayloadV2.fresh_recovery_pk",
            &self.fresh_recovery_pk,
        )?;
        validate_bootstrap_caps_structure(&self.bootstrap_caps)
    }
}

// —— ChallengeV2（ADR 0015 §鉴权）——

/// `ChallengeV2.realm`：普通设备（v2 鉴权，token 不带 cap）。
pub const CHALLENGE_V2_REALM_DEVICE: u8 = 1;
/// `ChallengeV2.realm`：管理设备（必须绑定一个有效 `ManagementCapV2`）。
pub const CHALLENGE_V2_REALM_MANAGEMENT_DEVICE: u8 = 3;
/// `ChallengeV2.realm`：离线恢复（S4c；本切片只保留取值）。
pub const CHALLENGE_V2_REALM_RECOVERY: u8 = 4;

/// `ChallengeV2`（键 `0..12`）：V2 challenge-response 的**被签名字节**。
///
/// ADR 0015：`{version,I,origin_hash,V,realm,subject,challenge_id,nonce,expires_at,
/// auth_rev,authz_rev,cap_id,authority_rev}`；客户端对**整份字节**签
/// `MN/v2/auth\0 || CDE(ChallengeV2)`，relay 从 DB 重算/逐字段比对后消费。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChallengeV2 {
    pub version: u16,
    pub instance_id: RelayInstanceId,
    /// relay origin 绑定摘要（relay 侧取值见实现记录；客户端必须原样回传）。
    pub origin_hash: Sha256,
    pub vault_id: VaultId,
    /// 1 设备 / 3 管理设备 / 4 恢复（2 仅旧 v1 wire，V2 不使用）。
    pub realm: u8,
    pub subject: DeviceId,
    pub challenge_id: [u8; 16],
    pub nonce: Sha256,
    pub expires_at: u64,
    pub auth_rev: u64,
    pub authz_rev: u64,
    /// 仅 realm 3 必填（管理与 cap 绑定）；realm 1/4 必须为空。
    pub cap_id: Option<CapId>,
    pub authority_rev: u64,
}

impl ChallengeV2 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        Self::from_cde(decode(bytes)?)
    }

    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.validate()?;
        Ok(record_map(vec![
            (0, CdeValue::UInt(u64::from(self.version))),
            (1, CdeValue::Bytes(self.instance_id.to_vec())),
            (2, CdeValue::Bytes(self.origin_hash.to_vec())),
            (3, CdeValue::Bytes(self.vault_id.to_vec())),
            (4, CdeValue::UInt(u64::from(self.realm))),
            (5, CdeValue::Bytes(self.subject.to_vec())),
            (6, CdeValue::Bytes(self.challenge_id.to_vec())),
            (7, CdeValue::Bytes(self.nonce.to_vec())),
            (8, CdeValue::UInt(self.expires_at)),
            (9, CdeValue::UInt(self.auth_rev)),
            (10, CdeValue::UInt(self.authz_rev)),
            (11, encode_optional_id16(self.cap_id)),
            (12, CdeValue::UInt(self.authority_rev)),
        ]))
    }

    pub fn from_cde(value: CdeValue) -> Result<Self, ProtoError> {
        let fields = into_record_fields(value, CHALLENGE_V2_FIELD_COUNT)?;
        let challenge = Self {
            version: expect_u16(fields[0].clone(), "ChallengeV2.version")?,
            instance_id: expect_bytes_n::<16>(fields[1].clone(), "ChallengeV2.instance_id")?,
            origin_hash: expect_bytes_n::<32>(fields[2].clone(), "ChallengeV2.origin_hash")?,
            vault_id: expect_bytes_n::<16>(fields[3].clone(), "ChallengeV2.vault_id")?,
            realm: expect_u8(fields[4].clone(), "ChallengeV2.realm")?,
            subject: expect_bytes_n::<16>(fields[5].clone(), "ChallengeV2.subject")?,
            challenge_id: expect_bytes_n::<16>(fields[6].clone(), "ChallengeV2.challenge_id")?,
            nonce: expect_bytes_n::<32>(fields[7].clone(), "ChallengeV2.nonce")?,
            expires_at: expect_i64_max(fields[8].clone(), "ChallengeV2.expires_at")?,
            auth_rev: expect_i64_max(fields[9].clone(), "ChallengeV2.auth_rev")?,
            authz_rev: expect_i64_max(fields[10].clone(), "ChallengeV2.authz_rev")?,
            cap_id: expect_optional_id16(fields[11].clone(), "ChallengeV2.cap_id")?,
            authority_rev: expect_i64_max(fields[12].clone(), "ChallengeV2.authority_rev")?,
        };
        challenge.validate()?;
        Ok(challenge)
    }

    pub fn validate(&self) -> Result<(), ProtoError> {
        check_version(self.version)?;
        check_nonzero("ChallengeV2.instance_id", &self.instance_id)?;
        check_nonzero("ChallengeV2.origin_hash", &self.origin_hash)?;
        check_nonzero("ChallengeV2.vault_id", &self.vault_id)?;
        check_nonzero("ChallengeV2.subject", &self.subject)?;
        check_nonzero("ChallengeV2.challenge_id", &self.challenge_id)?;
        check_nonzero("ChallengeV2.nonce", &self.nonce)?;
        match self.realm {
            CHALLENGE_V2_REALM_DEVICE | CHALLENGE_V2_REALM_RECOVERY => {
                if self.cap_id.is_some() {
                    return Err(ProtoError::Invalid(
                        "ChallengeV2: realm 1/4 不能绑定 cap".to_owned(),
                    ));
                }
            }
            CHALLENGE_V2_REALM_MANAGEMENT_DEVICE => {
                if self.cap_id.is_none() {
                    return Err(ProtoError::Invalid(
                        "ChallengeV2: realm 3 必须绑定 cap_id".to_owned(),
                    ));
                }
            }
            other => {
                return Err(ProtoError::Invalid(format!(
                    "ChallengeV2.realm 非法 {other}"
                )));
            }
        }
        if let Some(cap_id) = self.cap_id {
            check_nonzero("ChallengeV2.cap_id", &cap_id)?;
        }
        for (name, value) in [
            ("ChallengeV2.expires_at", self.expires_at),
            ("ChallengeV2.auth_rev", self.auth_rev),
            ("ChallengeV2.authz_rev", self.authz_rev),
            ("ChallengeV2.authority_rev", self.authority_rev),
        ] {
            check_i64_max(value, name)?;
        }
        if self.expires_at == 0 {
            return Err(ProtoError::Invalid(
                "ChallengeV2.expires_at 必须 >0".to_owned(),
            ));
        }
        if self.auth_rev == 0 {
            return Err(ProtoError::Invalid(
                "ChallengeV2.auth_rev 必须 >0".to_owned(),
            ));
        }
        Ok(())
    }
}

// —— DeviceCertV2 ——

/// `DeviceCertV2`（键 `0..9`）：V2 数据面权限，不兼任管理权限。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceCertV2 {
    pub version: u16,
    pub instance_id: RelayInstanceId,
    pub vault_id: VaultId,
    pub device_id: DeviceId,
    pub sign_ed25519_pub: Ed25519PublicKey,
    pub hpke_x25519_pub: X25519PublicKey,
    pub cert_serial: u64,
    /// 仅 READ/WRITE。
    pub data_rights: Rights,
    pub issued_authz_rev: u64,
}

impl DeviceCertV2 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        Self::from_cde(decode(bytes)?)
    }

    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.validate()?;
        Ok(record_map(vec![
            (0, CdeValue::UInt(u64::from(self.version))),
            (1, CdeValue::Bytes(self.instance_id.to_vec())),
            (2, CdeValue::Bytes(self.vault_id.to_vec())),
            (3, CdeValue::Bytes(self.device_id.to_vec())),
            (4, CdeValue::Bytes(self.sign_ed25519_pub.to_vec())),
            (5, CdeValue::Bytes(self.hpke_x25519_pub.to_vec())),
            (6, CdeValue::UInt(self.cert_serial)),
            (7, CdeValue::UInt(u64::from(self.data_rights.bits()))),
            (8, CdeValue::UInt(self.issued_authz_rev)),
        ]))
    }

    pub fn from_cde(value: CdeValue) -> Result<Self, ProtoError> {
        let fields = into_record_fields(value, DEVICE_CERT_V2_FIELD_COUNT)?;
        let cert = Self {
            version: expect_u16(fields[0].clone(), "DeviceCertV2.version")?,
            instance_id: expect_bytes_n::<16>(fields[1].clone(), "DeviceCertV2.instance_id")?,
            vault_id: expect_bytes_n::<16>(fields[2].clone(), "DeviceCertV2.vault_id")?,
            device_id: expect_bytes_n::<16>(fields[3].clone(), "DeviceCertV2.device_id")?,
            sign_ed25519_pub: expect_bytes_n::<32>(fields[4].clone(), "DeviceCertV2.sign_pub")?,
            hpke_x25519_pub: expect_bytes_n::<32>(fields[5].clone(), "DeviceCertV2.hpke_pub")?,
            cert_serial: expect_i64_max(fields[6].clone(), "DeviceCertV2.cert_serial")?,
            data_rights: Rights::new(expect_u32(fields[7].clone(), "DeviceCertV2.data_rights")?)?,
            issued_authz_rev: expect_i64_max(fields[8].clone(), "DeviceCertV2.issued_authz_rev")?,
        };
        cert.validate()?;
        Ok(cert)
    }

    pub fn validate(&self) -> Result<(), ProtoError> {
        check_version(self.version)?;
        check_nonzero("DeviceCertV2.instance_id", &self.instance_id)?;
        check_nonzero("DeviceCertV2.vault_id", &self.vault_id)?;
        check_nonzero("DeviceCertV2.device_id", &self.device_id)?;
        check_nonzero("DeviceCertV2.sign_ed25519_pub", &self.sign_ed25519_pub)?;
        check_nonzero("DeviceCertV2.hpke_x25519_pub", &self.hpke_x25519_pub)?;
        if self.cert_serial == 0 {
            return Err(ProtoError::Invalid(
                "DeviceCertV2.cert_serial 必须 >0".to_owned(),
            ));
        }
        // 只允许 READ/WRITE；不得把管理 scope 塞进数据权限位。
        Rights::new(self.data_rights.bits())?;
        check_i64_max(self.cert_serial, "DeviceCertV2.cert_serial")?;
        check_i64_max(self.issued_authz_rev, "DeviceCertV2.issued_authz_rev")?;
        if self.issued_authz_rev == 0 {
            return Err(ProtoError::Invalid(
                "DeviceCertV2.issued_authz_rev 必须 >0".to_owned(),
            ));
        }
        Ok(())
    }
}

// —— GrantAadV2 / GrantPackageV2 ——

/// `GrantAadV2`（键 `0..9`）：HPKE grant 的 AAD；与 v1 的 `GrantAadV1` 不兼容。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantAadV2 {
    pub version: u16,
    pub instance_id: RelayInstanceId,
    pub vault_id: VaultId,
    pub epoch: u64,
    pub recipient_device_id: DeviceId,
    pub recipient_hpke_pub: X25519PublicKey,
    pub authority_rev: u64,
    pub grant_id: GrantIdV2,
    pub issued_authz_rev: u64,
}

impl GrantAadV2 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        Self::from_cde(decode(bytes)?)
    }

    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.validate()?;
        Ok(record_map(vec![
            (0, CdeValue::UInt(u64::from(self.version))),
            (1, CdeValue::Bytes(self.instance_id.to_vec())),
            (2, CdeValue::Bytes(self.vault_id.to_vec())),
            (3, CdeValue::UInt(self.epoch)),
            (4, CdeValue::Bytes(self.recipient_device_id.to_vec())),
            (5, CdeValue::Bytes(self.recipient_hpke_pub.to_vec())),
            (6, CdeValue::UInt(self.authority_rev)),
            (7, CdeValue::Bytes(self.grant_id.to_vec())),
            (8, CdeValue::UInt(self.issued_authz_rev)),
        ]))
    }

    pub fn from_cde(value: CdeValue) -> Result<Self, ProtoError> {
        let fields = into_record_fields(value, GRANT_AAD_V2_FIELD_COUNT)?;
        let aad = Self {
            version: expect_u16(fields[0].clone(), "GrantAadV2.version")?,
            instance_id: expect_bytes_n::<16>(fields[1].clone(), "GrantAadV2.instance_id")?,
            vault_id: expect_bytes_n::<16>(fields[2].clone(), "GrantAadV2.vault_id")?,
            epoch: expect_i64_max(fields[3].clone(), "GrantAadV2.epoch")?,
            recipient_device_id: expect_bytes_n::<16>(
                fields[4].clone(),
                "GrantAadV2.recipient_device_id",
            )?,
            recipient_hpke_pub: expect_bytes_n::<32>(
                fields[5].clone(),
                "GrantAadV2.recipient_hpke_pub",
            )?,
            authority_rev: expect_i64_max(fields[6].clone(), "GrantAadV2.authority_rev")?,
            grant_id: expect_bytes_n::<16>(fields[7].clone(), "GrantAadV2.grant_id")?,
            issued_authz_rev: expect_i64_max(fields[8].clone(), "GrantAadV2.issued_authz_rev")?,
        };
        aad.validate()?;
        Ok(aad)
    }

    pub fn validate(&self) -> Result<(), ProtoError> {
        check_version(self.version)?;
        check_nonzero("GrantAadV2.instance_id", &self.instance_id)?;
        check_nonzero("GrantAadV2.vault_id", &self.vault_id)?;
        check_nonzero("GrantAadV2.recipient_device_id", &self.recipient_device_id)?;
        check_nonzero("GrantAadV2.recipient_hpke_pub", &self.recipient_hpke_pub)?;
        check_nonzero("GrantAadV2.grant_id", &self.grant_id)?;
        if self.epoch == 0 {
            return Err(ProtoError::Invalid("GrantAadV2.epoch 必须 >0".to_owned()));
        }
        check_i64_max(self.epoch, "GrantAadV2.epoch")?;
        check_i64_max(self.authority_rev, "GrantAadV2.authority_rev")?;
        check_i64_max(self.issued_authz_rev, "GrantAadV2.issued_authz_rev")?;
        if self.authority_rev == 0 {
            return Err(ProtoError::Invalid(
                "GrantAadV2.authority_rev 必须 >0".to_owned(),
            ));
        }
        if self.issued_authz_rev == 0 {
            return Err(ProtoError::Invalid(
                "GrantAadV2.issued_authz_rev 必须 >0".to_owned(),
            ));
        }
        Ok(())
    }

    /// AAD 字节 = `CDE(GrantAadV2)`。
    pub fn aad_bytes(&self) -> Result<Vec<u8>, ProtoError> {
        self.encode()
    }
}

/// grant 包 `{0:aad_bytes,1:enc,2:ciphertext}`；AAD 以类型化对象保存并在编解码时校验 canonical。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantPackageV2 {
    pub aad: GrantAadV2,
    pub enc: [u8; 32],
    pub ciphertext: Vec<u8>,
}

impl GrantPackageV2 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        Self::from_cde(decode(bytes)?)
    }

    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.validate()?;
        Ok(record_map(vec![
            (0, CdeValue::Bytes(self.aad.aad_bytes()?)),
            (1, CdeValue::Bytes(self.enc.to_vec())),
            (2, CdeValue::Bytes(self.ciphertext.clone())),
        ]))
    }

    pub fn from_cde(value: CdeValue) -> Result<Self, ProtoError> {
        let fields = into_record_fields(value, GRANT_PACKAGE_V2_FIELD_COUNT)?;
        let aad_bytes = expect_bytes(fields[0].clone(), "GrantPackageV2.aad")?;
        let package = Self {
            aad: GrantAadV2::decode(&aad_bytes)?,
            enc: expect_bytes_n::<32>(fields[1].clone(), "GrantPackageV2.enc")?,
            ciphertext: expect_bytes(fields[2].clone(), "GrantPackageV2.ciphertext")?,
        };
        package.validate()?;
        Ok(package)
    }

    pub fn validate(&self) -> Result<(), ProtoError> {
        self.aad.validate()?;
        check_nonzero("GrantPackageV2.enc", &self.enc)?;
        if self.ciphertext.is_empty() {
            return Err(ProtoError::Invalid(
                "GrantPackageV2.ciphertext 不能为空".to_owned(),
            ));
        }
        Ok(())
    }
}

// —— ActivationV2 ——

/// `ActivationV2`（键 `0..13`）：V2 epoch 激活材料；上下文/前链头由 `OperationV2` 绑定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivationV2 {
    pub version: u16,
    pub rotation_id: RotationIdV2,
    pub from_epoch: u64,
    pub to_epoch: u64,
    /// 冻结截止 S。
    pub freeze_seq: u64,
    pub primary_cid: [u8; 16],
    pub primary_commit_hash: Sha256,
    pub fallback_cid: [u8; 16],
    pub fallback_commit_hash: Sha256,
    pub slot_set_hash: Sha256,
    pub member_cert_hash: Sha256,
    pub grants_hash: Sha256,
    pub old_asset_hash: Sha256,
    pub new_asset_hash: Sha256,
}

impl ActivationV2 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        Self::from_cde(decode(bytes)?)
    }

    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.validate()?;
        Ok(record_map(vec![
            (0, CdeValue::UInt(u64::from(self.version))),
            (1, CdeValue::Bytes(self.rotation_id.to_vec())),
            (2, CdeValue::UInt(self.from_epoch)),
            (3, CdeValue::UInt(self.to_epoch)),
            (4, CdeValue::UInt(self.freeze_seq)),
            (5, CdeValue::Bytes(self.primary_cid.to_vec())),
            (6, CdeValue::Bytes(self.primary_commit_hash.to_vec())),
            (7, CdeValue::Bytes(self.fallback_cid.to_vec())),
            (8, CdeValue::Bytes(self.fallback_commit_hash.to_vec())),
            (9, CdeValue::Bytes(self.slot_set_hash.to_vec())),
            (10, CdeValue::Bytes(self.member_cert_hash.to_vec())),
            (11, CdeValue::Bytes(self.grants_hash.to_vec())),
            (12, CdeValue::Bytes(self.old_asset_hash.to_vec())),
            (13, CdeValue::Bytes(self.new_asset_hash.to_vec())),
        ]))
    }

    pub fn from_cde(value: CdeValue) -> Result<Self, ProtoError> {
        let fields = into_record_fields(value, ACTIVATION_V2_FIELD_COUNT)?;
        let activation = Self {
            version: expect_u16(fields[0].clone(), "ActivationV2.version")?,
            rotation_id: expect_bytes_n::<16>(fields[1].clone(), "ActivationV2.rotation_id")?,
            from_epoch: expect_i64_max(fields[2].clone(), "ActivationV2.from_epoch")?,
            to_epoch: expect_i64_max(fields[3].clone(), "ActivationV2.to_epoch")?,
            freeze_seq: expect_i64_max(fields[4].clone(), "ActivationV2.freeze_seq")?,
            primary_cid: expect_bytes_n::<16>(fields[5].clone(), "ActivationV2.primary_cid")?,
            primary_commit_hash: expect_bytes_n::<32>(
                fields[6].clone(),
                "ActivationV2.primary_commit_hash",
            )?,
            fallback_cid: expect_bytes_n::<16>(fields[7].clone(), "ActivationV2.fallback_cid")?,
            fallback_commit_hash: expect_bytes_n::<32>(
                fields[8].clone(),
                "ActivationV2.fallback_commit_hash",
            )?,
            slot_set_hash: expect_bytes_n::<32>(fields[9].clone(), "ActivationV2.slot_set_hash")?,
            member_cert_hash: expect_bytes_n::<32>(
                fields[10].clone(),
                "ActivationV2.member_cert_hash",
            )?,
            grants_hash: expect_bytes_n::<32>(fields[11].clone(), "ActivationV2.grants_hash")?,
            old_asset_hash: expect_bytes_n::<32>(
                fields[12].clone(),
                "ActivationV2.old_asset_hash",
            )?,
            new_asset_hash: expect_bytes_n::<32>(
                fields[13].clone(),
                "ActivationV2.new_asset_hash",
            )?,
        };
        activation.validate()?;
        Ok(activation)
    }

    pub fn validate(&self) -> Result<(), ProtoError> {
        check_version(self.version)?;
        for (name, value) in [
            ("ActivationV2.rotation_id", &self.rotation_id[..]),
            ("ActivationV2.primary_cid", &self.primary_cid[..]),
            (
                "ActivationV2.primary_commit_hash",
                &self.primary_commit_hash[..],
            ),
            ("ActivationV2.fallback_cid", &self.fallback_cid[..]),
            (
                "ActivationV2.fallback_commit_hash",
                &self.fallback_commit_hash[..],
            ),
            ("ActivationV2.slot_set_hash", &self.slot_set_hash[..]),
            ("ActivationV2.member_cert_hash", &self.member_cert_hash[..]),
            ("ActivationV2.grants_hash", &self.grants_hash[..]),
            ("ActivationV2.old_asset_hash", &self.old_asset_hash[..]),
            ("ActivationV2.new_asset_hash", &self.new_asset_hash[..]),
        ] {
            check_nonzero(name, value)?;
        }
        for (name, value) in [
            ("ActivationV2.from_epoch", self.from_epoch),
            ("ActivationV2.to_epoch", self.to_epoch),
            ("ActivationV2.freeze_seq", self.freeze_seq),
        ] {
            check_i64_max(value, name)?;
        }
        if self.from_epoch == 0 {
            return Err(ProtoError::Invalid(
                "ActivationV2.from_epoch 必须 >0".to_owned(),
            ));
        }
        if self.to_epoch <= self.from_epoch {
            return Err(ProtoError::Invalid(format!(
                "ActivationV2.to_epoch={} 必须大于 from_epoch={}",
                self.to_epoch, self.from_epoch
            )));
        }
        Ok(())
    }
}

`````



@@SECTION code/066 `crates/sync-proto/src/records.rs`


`````rust
//! ADR 0006 §2 / §15 / §16 的 CDE record 类型与手写编解码。
//!
//! 所有 record 都是整数键 map；字段按本文件/ADR 的声明顺序从 0 连续编号。
//! optional v1 不省略未定义字段；未知键/未知 tag 直接拒绝。

use crate::cde::{
    decode, encode, expect_array, expect_bool, expect_bytes, expect_bytes_n, expect_i64_max,
    expect_text, expect_u16, expect_uint, into_record_fields, CdeValue, MAX_CDE_BYTES,
};
use crate::error::ProtoError;
use crate::limits::{MAX_ASSET_MIME_BYTES, MAX_ASSET_RAW_BYTES, MAX_DEVICES, MAX_REFS};

pub type VaultId = [u8; 16];
pub type ObjectId = [u8; 32];
pub type MessageId = [u8; 16];
pub type DeviceId = [u8; 16];
pub type CheckpointId = [u8; 16];
pub type ChallengeId = [u8; 16];
pub type RelayInstanceId = [u8; 16];
pub type Sha256 = [u8; 32];
pub type Ed25519PublicKey = [u8; 32];
pub type X25519PublicKey = [u8; 32];

/// 对象类型（MNE1 header.type）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum ObjectType {
    Update = 1,
    Snapshot = 2,
    Manifest = 3,
    Asset = 4,
    KeySlot = 5,
}

impl ObjectType {
    pub fn as_u8(self) -> u8 {
        self as u8
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Update => "UPDATE",
            Self::Snapshot => "SNAPSHOT",
            Self::Manifest => "MANIFEST",
            Self::Asset => "ASSET",
            Self::KeySlot => "KEY_SLOT",
        }
    }

    pub fn is_data(self) -> bool {
        !matches!(self, Self::KeySlot)
    }

    /// UPDATE/SNAPSHOT 的 generation 必须 >0；其他类型固定为 0（§16.1）。
    pub fn requires_generation(self) -> bool {
        matches!(self, Self::Update | Self::Snapshot)
    }
}

impl TryFrom<u8> for ObjectType {
    type Error = ProtoError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Update),
            2 => Ok(Self::Snapshot),
            3 => Ok(Self::Manifest),
            4 => Ok(Self::Asset),
            5 => Ok(Self::KeySlot),
            other => Err(ProtoError::Invalid(format!("未知对象类型 {other}"))),
        }
    }
}

impl From<ObjectType> for u8 {
    fn from(value: ObjectType) -> Self {
        value.as_u8()
    }
}

/// 密钥槽种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum SlotKind {
    Password = 1,
    Recovery = 2,
}

impl TryFrom<u8> for SlotKind {
    type Error = ProtoError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Password),
            2 => Ok(Self::Recovery),
            other => Err(ProtoError::Invalid(format!("未知槽类型 {other}"))),
        }
    }
}

impl From<SlotKind> for u8 {
    fn from(value: SlotKind) -> Self {
        value as u8
    }
}

/// 鉴权 realm。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum AuthRealm {
    Device = 1,
    Management = 2,
}

impl TryFrom<u8> for AuthRealm {
    type Error = ProtoError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Device),
            2 => Ok(Self::Management),
            other => Err(ProtoError::Invalid(format!("未知 auth realm {other}"))),
        }
    }
}

impl From<AuthRealm> for u8 {
    fn from(value: AuthRealm) -> Self {
        value as u8
    }
}

/// HTTP method 的管理签名标签（GET=1..DELETE=5）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum HttpMethod {
    Get = 1,
    Post = 2,
    Put = 3,
    Patch = 4,
    Delete = 5,
}

impl TryFrom<u8> for HttpMethod {
    type Error = ProtoError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Get),
            2 => Ok(Self::Post),
            3 => Ok(Self::Put),
            4 => Ok(Self::Patch),
            5 => Ok(Self::Delete),
            other => Err(ProtoError::Invalid(format!(
                "未知 HTTP method 标签 {other}"
            ))),
        }
    }
}

impl From<HttpMethod> for u8 {
    fn from(value: HttpMethod) -> Self {
        value as u8
    }
}

/// 设备权限位。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rights(pub u32);

impl Rights {
    pub const READ: u32 = 1;
    pub const WRITE: u32 = 2;
    pub const ALL: u32 = Self::READ | Self::WRITE;

    pub fn new(bits: u32) -> Result<Self, ProtoError> {
        if bits == 0 || bits & !Self::ALL != 0 {
            return Err(ProtoError::Invalid(format!("非法 rights 位 {bits:#x}")));
        }
        Ok(Self(bits))
    }

    pub fn bits(self) -> u32 {
        self.0
    }

    pub fn can_read(self) -> bool {
        self.0 & Self::READ != 0
    }

    pub fn can_write(self) -> bool {
        self.0 & Self::WRITE != 0
    }
}

/// Argon2id 槽参数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Argon2idParams {
    pub version: u16,
    pub m_kib: u32,
    pub t: u32,
    pub p: u32,
    pub salt: [u8; 16],
    pub output_len: u32,
}

/// 槽 KDF union；tag=1 Argon2id、tag=2 RecoveryHKDF。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kdf {
    Argon2id(Argon2idParams),
    RecoveryHkdf,
}

impl Kdf {
    fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        match self {
            Self::Argon2id(params) => {
                if params.version != 0x13 {
                    return Err(ProtoError::UnsupportedVersion(params.version));
                }
                if !(65_536..=262_144).contains(&params.m_kib)
                    || !(3..=6).contains(&params.t)
                    || !(1..=4).contains(&params.p)
                    || params.output_len != 32
                {
                    return Err(ProtoError::Invalid("Argon2id 参数越界".to_owned()));
                }
                Ok(CdeValue::Array(vec![
                    CdeValue::UInt(1),
                    CdeValue::Map(vec![
                        (0, CdeValue::UInt(u64::from(params.version))),
                        (1, CdeValue::UInt(u64::from(params.m_kib))),
                        (2, CdeValue::UInt(u64::from(params.t))),
                        (3, CdeValue::UInt(u64::from(params.p))),
                        (4, CdeValue::Bytes(params.salt.to_vec())),
                        (5, CdeValue::UInt(u64::from(params.output_len))),
                    ]),
                ]))
            }
            Self::RecoveryHkdf => Ok(CdeValue::Array(vec![
                CdeValue::UInt(2),
                CdeValue::Map(Vec::new()),
            ])),
        }
    }

    fn from_cde(value: CdeValue) -> Result<Self, ProtoError> {
        let CdeValue::Array(items) = value else {
            return Err(ProtoError::Invalid("Kdf 必须是 [tag, params]".to_owned()));
        };
        if items.len() != 2 {
            return Err(ProtoError::Invalid("Kdf 长度必须为 2".to_owned()));
        }
        let mut items = items.into_iter();
        let tag = expect_uint(items.next().expect("长度2"), "Kdf.tag")?;
        let params = items.next().expect("长度2");
        match tag {
            1 => {
                let CdeValue::Map(entries) = params else {
                    return Err(ProtoError::Invalid("Argon2id params 必须是 map".to_owned()));
                };
                let fields = into_record_fields(CdeValue::Map(entries), 6)?;
                let version = expect_u16(fields[0].clone(), "argon2.version")?;
                let m_kib = crate::cde::expect_u32(fields[1].clone(), "argon2.m_kib")?;
                let t = crate::cde::expect_u32(fields[2].clone(), "argon2.t")?;
                let p = crate::cde::expect_u32(fields[3].clone(), "argon2.p")?;
                let salt = expect_bytes_n::<16>(fields[4].clone(), "argon2.salt")?;
                let output_len = crate::cde::expect_u32(fields[5].clone(), "argon2.output_len")?;
                let params = Argon2idParams {
                    version,
                    m_kib,
                    t,
                    p,
                    salt,
                    output_len,
                };
                Self::Argon2id(params).to_cde()?;
                Ok(Self::Argon2id(Argon2idParams {
                    version,
                    m_kib,
                    t,
                    p,
                    salt,
                    output_len,
                }))
            }
            2 => {
                let CdeValue::Map(entries) = params else {
                    return Err(ProtoError::Invalid(
                        "RecoveryHKDF params 必须是空 map".to_owned(),
                    ));
                };
                if !entries.is_empty() {
                    return Err(ProtoError::Invalid(
                        "RecoveryHKDF params 必须为空".to_owned(),
                    ));
                }
                Ok(Self::RecoveryHkdf)
            }
            other => Err(ProtoError::Invalid(format!("未知 Kdf tag {other}"))),
        }
    }
}

/// 资产明文固定前缀：sha256[32] || BE64(byte_len) || BE16(mime_len) || mime。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssetPrefix {
    pub sha256: Sha256,
    pub byte_len: u64,
    pub media_type: String,
}

impl AssetPrefix {
    pub fn new(
        sha256: Sha256,
        byte_len: u64,
        media_type: impl Into<String>,
    ) -> Result<Self, ProtoError> {
        let value = Self {
            sha256,
            byte_len,
            media_type: media_type.into(),
        };
        value.validate()?;
        Ok(value)
    }

    pub fn validate(&self) -> Result<(), ProtoError> {
        if self.byte_len > MAX_ASSET_RAW_BYTES as u64 {
            return Err(ProtoError::TooLarge);
        }
        if self.media_type.len() > MAX_ASSET_MIME_BYTES {
            return Err(ProtoError::TooLarge);
        }
        if self.media_type.is_empty() {
            return Err(ProtoError::Invalid("asset media_type 不能为空".to_owned()));
        }
        Ok(())
    }

    /// 只编码前缀（不含 data）。
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        self.validate()?;
        let mut out = Vec::with_capacity(32 + 8 + 2 + self.media_type.len());
        out.extend_from_slice(&self.sha256);
        out.extend_from_slice(&self.byte_len.to_be_bytes());
        out.extend_from_slice(&(self.media_type.len() as u16).to_be_bytes());
        out.extend_from_slice(self.media_type.as_bytes());
        Ok(out)
    }

    /// 从前缀 + data 里解析前缀并返回数据偏移。
    pub fn parse(bytes: &[u8]) -> Result<(Self, usize), ProtoError> {
        if bytes.len() < 32 + 8 + 2 {
            return Err(ProtoError::Truncated);
        }
        let sha256 = bytes[0..32].try_into().expect("长度32");
        let byte_len = u64::from_be_bytes(bytes[32..40].try_into().expect("长度8"));
        let mime_len = usize::from(u16::from_be_bytes(bytes[40..42].try_into().expect("长度2")));
        if mime_len > MAX_ASSET_MIME_BYTES {
            return Err(ProtoError::TooLarge);
        }
        let prefix_len = 42 + mime_len;
        if bytes.len() < prefix_len {
            return Err(ProtoError::Truncated);
        }
        let media_type = std::str::from_utf8(&bytes[42..prefix_len])
            .map_err(|_| ProtoError::Invalid("asset mime 不是合法 UTF-8".to_owned()))?
            .to_owned();
        let value = Self {
            sha256,
            byte_len,
            media_type,
        };
        value.validate()?;
        Ok((value, prefix_len))
    }

    /// 解析完整资产明文，返回前缀与 data 切片。
    pub fn split_payload(bytes: &[u8]) -> Result<(Self, &[u8]), ProtoError> {
        if bytes.len() < 42 {
            return Err(ProtoError::Truncated);
        }
        let mime_len = usize::from(u16::from_be_bytes(bytes[40..42].try_into().expect("长度2")));
        if mime_len > MAX_ASSET_MIME_BYTES {
            return Err(ProtoError::TooLarge);
        }
        let prefix_len = 42 + mime_len;
        if bytes.len() < prefix_len {
            return Err(ProtoError::Truncated);
        }
        let media_type = std::str::from_utf8(&bytes[42..prefix_len])
            .map_err(|_| ProtoError::Invalid("asset mime 不是合法 UTF-8".to_owned()))?
            .to_owned();
        let value = Self {
            sha256: bytes[0..32].try_into().expect("长度32"),
            byte_len: u64::from_be_bytes(bytes[32..40].try_into().expect("长度8")),
            media_type,
        };
        value.validate()?;
        let data = &bytes[prefix_len..];
        if data.len() as u64 != value.byte_len {
            return Err(ProtoError::Invalid(format!(
                "asset data {}B 与前缀 byte_len {} 不符",
                data.len(),
                value.byte_len
            )));
        }
        Ok((value, data))
    }
}

/// UpdateV1 密文载荷。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateV1 {
    pub doc_id: String,
    pub generation: u64,
    pub codec_version: String,
    pub app_doc_schema: u32,
    pub min_reader: u32,
    pub update_id: String,
    pub batch_id: String,
    pub origin_device_id: DeviceId,
    pub origin_seq: u64,
    pub base_vv: Vec<u8>,
    pub end_vv: Vec<u8>,
    pub end_frontier: Vec<u8>,
    pub loro_update_bytes: Vec<u8>,
}

impl UpdateV1 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        let fields = into_record_fields(decode(bytes)?, 13)?;
        let value = Self {
            doc_id: expect_text(fields[0].clone(), "UpdateV1.doc_id")?,
            generation: expect_i64_max(fields[1].clone(), "UpdateV1.generation")?,
            codec_version: expect_text(fields[2].clone(), "UpdateV1.codec_version")?,
            app_doc_schema: crate::cde::expect_u32(fields[3].clone(), "UpdateV1.app_doc_schema")?,
            min_reader: crate::cde::expect_u32(fields[4].clone(), "UpdateV1.min_reader")?,
            update_id: expect_text(fields[5].clone(), "UpdateV1.update_id")?,
            batch_id: expect_text(fields[6].clone(), "UpdateV1.batch_id")?,
            origin_device_id: expect_bytes_n::<16>(fields[7].clone(), "UpdateV1.origin_device_id")?,
            origin_seq: expect_i64_max(fields[8].clone(), "UpdateV1.origin_seq")?,
            base_vv: expect_bytes(fields[9].clone(), "UpdateV1.base_vv")?,
            end_vv: expect_bytes(fields[10].clone(), "UpdateV1.end_vv")?,
            end_frontier: expect_bytes(fields[11].clone(), "UpdateV1.end_frontier")?,
            loro_update_bytes: expect_bytes(fields[12].clone(), "UpdateV1.loro_update_bytes")?,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.validate()?;
        Ok(CdeValue::Map(vec![
            (0, CdeValue::Text(self.doc_id.clone())),
            (1, CdeValue::UInt(self.generation)),
            (2, CdeValue::Text(self.codec_version.clone())),
            (3, CdeValue::UInt(u64::from(self.app_doc_schema))),
            (4, CdeValue::UInt(u64::from(self.min_reader))),
            (5, CdeValue::Text(self.update_id.clone())),
            (6, CdeValue::Text(self.batch_id.clone())),
            (7, CdeValue::Bytes(self.origin_device_id.to_vec())),
            (8, CdeValue::UInt(self.origin_seq)),
            (9, CdeValue::Bytes(self.base_vv.clone())),
            (10, CdeValue::Bytes(self.end_vv.clone())),
            (11, CdeValue::Bytes(self.end_frontier.clone())),
            (12, CdeValue::Bytes(self.loro_update_bytes.clone())),
        ]))
    }

    pub fn validate(&self) -> Result<(), ProtoError> {
        if self.generation == 0 {
            return Err(ProtoError::Invalid(
                "UpdateV1.generation 必须 >0".to_owned(),
            ));
        }
        if self.doc_id.is_empty() || self.codec_version.is_empty() {
            return Err(ProtoError::Invalid(
                "UpdateV1 doc/codec 不能为空".to_owned(),
            ));
        }
        if self.update_id.is_empty() || self.batch_id.is_empty() {
            return Err(ProtoError::Invalid(
                "UpdateV1 update_id/batch_id 不能为空".to_owned(),
            ));
        }
        if self.loro_update_bytes.is_empty() {
            return Err(ProtoError::Invalid("UpdateV1 更新不能为空".to_owned()));
        }
        if self.encoded_size_upper_bound() > MAX_CDE_BYTES {
            return Err(ProtoError::TooLarge);
        }
        Ok(())
    }

    fn encoded_size_upper_bound(&self) -> usize {
        self.loro_update_bytes.len()
            + self.base_vv.len()
            + self.end_vv.len()
            + self.end_frontier.len()
            + self.doc_id.len()
            + self.codec_version.len()
            + self.update_id.len()
            + self.batch_id.len()
            + 128
    }
}

/// SnapshotV1 密文载荷；v1 只允许 full_history=true。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotV1 {
    pub doc_id: String,
    pub kind: u64,
    pub generation: u64,
    pub codec_version: String,
    pub app_doc_schema: u32,
    pub min_reader: u32,
    pub full_history: bool,
    pub vv: Vec<u8>,
    pub frontier: Vec<u8>,
    pub loro_snapshot_bytes: Vec<u8>,
}

impl SnapshotV1 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        let fields = into_record_fields(decode(bytes)?, 10)?;
        let value = Self {
            doc_id: expect_text(fields[0].clone(), "SnapshotV1.doc_id")?,
            kind: expect_i64_max(fields[1].clone(), "SnapshotV1.kind")?,
            generation: expect_i64_max(fields[2].clone(), "SnapshotV1.generation")?,
            codec_version: expect_text(fields[3].clone(), "SnapshotV1.codec_version")?,
            app_doc_schema: crate::cde::expect_u32(fields[4].clone(), "SnapshotV1.app_doc_schema")?,
            min_reader: crate::cde::expect_u32(fields[5].clone(), "SnapshotV1.min_reader")?,
            full_history: expect_bool(fields[6].clone(), "SnapshotV1.full_history")?,
            vv: expect_bytes(fields[7].clone(), "SnapshotV1.vv")?,
            frontier: expect_bytes(fields[8].clone(), "SnapshotV1.frontier")?,
            loro_snapshot_bytes: expect_bytes(fields[9].clone(), "SnapshotV1.loro_snapshot_bytes")?,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.validate()?;
        Ok(CdeValue::Map(vec![
            (0, CdeValue::Text(self.doc_id.clone())),
            (1, CdeValue::UInt(self.kind)),
            (2, CdeValue::UInt(self.generation)),
            (3, CdeValue::Text(self.codec_version.clone())),
            (4, CdeValue::UInt(u64::from(self.app_doc_schema))),
            (5, CdeValue::UInt(u64::from(self.min_reader))),
            (6, CdeValue::Bool(self.full_history)),
            (7, CdeValue::Bytes(self.vv.clone())),
            (8, CdeValue::Bytes(self.frontier.clone())),
            (9, CdeValue::Bytes(self.loro_snapshot_bytes.clone())),
        ]))
    }

    pub fn validate(&self) -> Result<(), ProtoError> {
        if self.generation == 0 {
            return Err(ProtoError::Invalid(
                "SnapshotV1.generation 必须 >0".to_owned(),
            ));
        }
        if !self.full_history {
            return Err(ProtoError::Invalid(
                "v1 禁止 shallow/state-only snapshot".to_owned(),
            ));
        }
        if self.doc_id.is_empty() || self.codec_version.is_empty() {
            return Err(ProtoError::Invalid(
                "SnapshotV1 doc/codec 不能为空".to_owned(),
            ));
        }
        if self.loro_snapshot_bytes.is_empty() {
            return Err(ProtoError::Invalid("SnapshotV1 快照不能为空".to_owned()));
        }
        if self.loro_snapshot_bytes.len() > crate::limits::MAX_LORO_SNAPSHOT_BYTES {
            return Err(ProtoError::TooLarge);
        }
        Ok(())
    }
}

/// Manifest 里的 doc 引用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestDoc {
    pub doc_id: String,
    pub kind: u64,
    pub generation: u64,
    pub key_epoch: u64,
    pub vv: Vec<u8>,
    pub frontier: Vec<u8>,
    pub snapshot_object_id: ObjectId,
    pub part_count: u32,
    pub total_plain_len: u64,
    pub object_digest: Sha256,
}

impl ManifestDoc {
    fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.validate()?;
        Ok(CdeValue::Map(vec![
            (0, CdeValue::Text(self.doc_id.clone())),
            (1, CdeValue::UInt(self.kind)),
            (2, CdeValue::UInt(self.generation)),
            (3, CdeValue::Bytes(self.vv.clone())),
            (4, CdeValue::Bytes(self.frontier.clone())),
            (5, CdeValue::Bytes(self.snapshot_object_id.to_vec())),
            (6, CdeValue::UInt(u64::from(self.part_count))),
            (7, CdeValue::Bytes(self.object_digest.to_vec())),
            (8, CdeValue::UInt(self.key_epoch)),
            (9, CdeValue::UInt(self.total_plain_len)),
        ]))
    }

    fn from_cde(value: CdeValue) -> Result<Self, ProtoError> {
        let fields = into_record_fields(value, 10)?;
        let value = Self {
            doc_id: expect_text(fields[0].clone(), "ManifestDoc.doc_id")?,
            kind: expect_i64_max(fields[1].clone(), "ManifestDoc.kind")?,
            generation: expect_i64_max(fields[2].clone(), "ManifestDoc.generation")?,
            key_epoch: expect_i64_max(fields[8].clone(), "ManifestDoc.key_epoch")?,
            vv: expect_bytes(fields[3].clone(), "ManifestDoc.vv")?,
            frontier: expect_bytes(fields[4].clone(), "ManifestDoc.frontier")?,
            snapshot_object_id: expect_bytes_n::<32>(
                fields[5].clone(),
                "ManifestDoc.snapshot_object_id",
            )?,
            part_count: crate::cde::expect_u32(fields[6].clone(), "ManifestDoc.part_count")?,
            total_plain_len: expect_i64_max(fields[9].clone(), "ManifestDoc.total_plain_len")?,
            object_digest: expect_bytes_n::<32>(fields[7].clone(), "ManifestDoc.object_digest")?,
        };
        value.validate()?;
        Ok(value)
    }

    fn validate(&self) -> Result<(), ProtoError> {
        if self.doc_id.is_empty() {
            return Err(ProtoError::Invalid(
                "ManifestDoc.doc_id 不能为空".to_owned(),
            ));
        }
        if self.generation == 0 {
            return Err(ProtoError::Invalid(
                "ManifestDoc.generation 必须 >0".to_owned(),
            ));
        }
        if self.part_count == 0 {
            return Err(ProtoError::Invalid(
                "ManifestDoc.part_count 必须 ≥1".to_owned(),
            ));
        }
        if self.key_epoch == 0 {
            return Err(ProtoError::Invalid(
                "ManifestDoc.key_epoch 必须 >0".to_owned(),
            ));
        }
        if self.total_plain_len == 0 {
            return Err(ProtoError::Invalid(
                "ManifestDoc.total_plain_len 必须 >0".to_owned(),
            ));
        }
        Ok(())
    }
}

/// Manifest 里的 asset 引用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestAsset {
    pub real_sha256: Sha256,
    pub byte_len: u64,
    pub media_type: String,
    pub object_id: ObjectId,
    pub key_epoch: u64,
    pub part_count: u32,
    pub total_plain_len: u64,
    pub object_digest: Sha256,
}

impl ManifestAsset {
    fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.validate()?;
        Ok(CdeValue::Map(vec![
            (0, CdeValue::Bytes(self.real_sha256.to_vec())),
            (1, CdeValue::UInt(self.byte_len)),
            (2, CdeValue::Text(self.media_type.clone())),
            (3, CdeValue::Bytes(self.object_id.to_vec())),
            (4, CdeValue::UInt(u64::from(self.part_count))),
            (5, CdeValue::Bytes(self.object_digest.to_vec())),
            (6, CdeValue::UInt(self.key_epoch)),
            (7, CdeValue::UInt(self.total_plain_len)),
        ]))
    }

    fn from_cde(value: CdeValue) -> Result<Self, ProtoError> {
        let fields = into_record_fields(value, 8)?;
        let value = Self {
            real_sha256: expect_bytes_n::<32>(fields[0].clone(), "ManifestAsset.real_sha256")?,
            byte_len: expect_i64_max(fields[1].clone(), "ManifestAsset.byte_len")?,
            media_type: expect_text(fields[2].clone(), "ManifestAsset.media_type")?,
            object_id: expect_bytes_n::<32>(fields[3].clone(), "ManifestAsset.object_id")?,
            key_epoch: expect_i64_max(fields[6].clone(), "ManifestAsset.key_epoch")?,
            part_count: crate::cde::expect_u32(fields[4].clone(), "ManifestAsset.part_count")?,
            total_plain_len: expect_i64_max(fields[7].clone(), "ManifestAsset.total_plain_len")?,
            object_digest: expect_bytes_n::<32>(fields[5].clone(), "ManifestAsset.object_digest")?,
        };
        value.validate()?;
        Ok(value)
    }

    fn validate(&self) -> Result<(), ProtoError> {
        if self.byte_len > MAX_ASSET_RAW_BYTES as u64 {
            return Err(ProtoError::TooLarge);
        }
        if self.media_type.is_empty() || self.media_type.len() > MAX_ASSET_MIME_BYTES {
            return Err(ProtoError::Invalid(
                "ManifestAsset.media_type 非法".to_owned(),
            ));
        }
        if self.part_count == 0 {
            return Err(ProtoError::Invalid(
                "ManifestAsset.part_count 必须 ≥1".to_owned(),
            ));
        }
        if self.key_epoch == 0 {
            return Err(ProtoError::Invalid(
                "ManifestAsset.key_epoch 必须 >0".to_owned(),
            ));
        }
        if self.total_plain_len == 0 {
            return Err(ProtoError::Invalid(
                "ManifestAsset.total_plain_len 必须 >0".to_owned(),
            ));
        }
        Ok(())
    }
}

/// ManifestV1 密文载荷。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestV1 {
    pub vault_id: VaultId,
    pub key_epoch: u64,
    pub checkpoint_id: CheckpointId,
    pub covered_relay_seq: u64,
    pub min_reader: u32,
    pub structure_doc_id: String,
    pub docs: Vec<ManifestDoc>,
    pub assets: Vec<ManifestAsset>,
}

impl ManifestV1 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        let fields = into_record_fields(decode(bytes)?, 8)?;
        let docs = expect_array(fields[6].clone(), "ManifestV1.docs")?
            .into_iter()
            .map(ManifestDoc::from_cde)
            .collect::<Result<Vec<_>, _>>()?;
        let assets = expect_array(fields[7].clone(), "ManifestV1.assets")?
            .into_iter()
            .map(ManifestAsset::from_cde)
            .collect::<Result<Vec<_>, _>>()?;
        let value = Self {
            vault_id: expect_bytes_n::<16>(fields[0].clone(), "ManifestV1.vault_id")?,
            key_epoch: expect_i64_max(fields[1].clone(), "ManifestV1.key_epoch")?,
            checkpoint_id: expect_bytes_n::<16>(fields[2].clone(), "ManifestV1.checkpoint_id")?,
            covered_relay_seq: expect_i64_max(fields[3].clone(), "ManifestV1.covered_relay_seq")?,
            min_reader: crate::cde::expect_u32(fields[4].clone(), "ManifestV1.min_reader")?,
            structure_doc_id: expect_text(fields[5].clone(), "ManifestV1.structure_doc_id")?,
            docs,
            assets,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.validate()?;
        let docs = self
            .docs
            .iter()
            .map(ManifestDoc::to_cde)
            .collect::<Result<Vec<_>, _>>()?;
        let assets = self
            .assets
            .iter()
            .map(ManifestAsset::to_cde)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(CdeValue::Map(vec![
            (0, CdeValue::Bytes(self.vault_id.to_vec())),
            (1, CdeValue::UInt(self.key_epoch)),
            (2, CdeValue::Bytes(self.checkpoint_id.to_vec())),
            (3, CdeValue::UInt(self.covered_relay_seq)),
            (4, CdeValue::UInt(u64::from(self.min_reader))),
            (5, CdeValue::Text(self.structure_doc_id.clone())),
            (6, CdeValue::Array(docs)),
            (7, CdeValue::Array(assets)),
        ]))
    }

    pub fn validate(&self) -> Result<(), ProtoError> {
        if self.key_epoch == 0 {
            return Err(ProtoError::Invalid(
                "ManifestV1.key_epoch 必须 >0".to_owned(),
            ));
        }
        if self.structure_doc_id.is_empty() {
            return Err(ProtoError::Invalid(
                "ManifestV1.structure_doc_id 不能为空".to_owned(),
            ));
        }
        if self.docs.len() + self.assets.len() > MAX_REFS {
            return Err(ProtoError::TooLarge);
        }
        // docs 按 doc_id 的 UTF-8 字节严格升序；assets 按 real_sha256 严格升序。
        for pair in self.docs.windows(2) {
            if pair[0].doc_id.as_bytes() >= pair[1].doc_id.as_bytes() {
                return Err(ProtoError::NonCanonical(
                    "ManifestV1.docs 未按 doc_id 升序".to_owned(),
                ));
            }
        }
        for pair in self.assets.windows(2) {
            if pair[0].real_sha256 >= pair[1].real_sha256 {
                return Err(ProtoError::NonCanonical(
                    "ManifestV1.assets 未按 real_sha256 升序".to_owned(),
                ));
            }
        }
        Ok(())
    }
}

/// Key slot 的 CDE descriptor；作为 KEY_SLOT AAD 的 descriptor。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotDescriptorV1 {
    pub protocol_version: u16,
    pub slot_id: ObjectId,
    pub vault_id: VaultId,
    pub key_epoch: u64,
    pub kind: SlotKind,
    pub wrap_suite: u16,
    pub kdf: Kdf,
    pub created_at: u64,
    pub manager_rev: u64,
}

impl SlotDescriptorV1 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        let fields = into_record_fields(decode(bytes)?, 9)?;
        let value = Self {
            protocol_version: expect_u16(fields[0].clone(), "SlotDescriptorV1.protocol_version")?,
            slot_id: expect_bytes_n::<32>(fields[1].clone(), "SlotDescriptorV1.slot_id")?,
            vault_id: expect_bytes_n::<16>(fields[2].clone(), "SlotDescriptorV1.vault_id")?,
            key_epoch: expect_i64_max(fields[3].clone(), "SlotDescriptorV1.key_epoch")?,
            kind: SlotKind::try_from(crate::cde::expect_u8(
                fields[4].clone(),
                "SlotDescriptorV1.kind",
            )?)?,
            wrap_suite: expect_u16(fields[5].clone(), "SlotDescriptorV1.wrap_suite")?,
            kdf: Kdf::from_cde(fields[6].clone())?,
            created_at: expect_i64_max(fields[7].clone(), "SlotDescriptorV1.created_at")?,
            manager_rev: expect_i64_max(fields[8].clone(), "SlotDescriptorV1.manager_rev")?,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.validate()?;
        Ok(CdeValue::Map(vec![
            (0, CdeValue::UInt(u64::from(self.protocol_version))),
            (1, CdeValue::Bytes(self.slot_id.to_vec())),
            (2, CdeValue::Bytes(self.vault_id.to_vec())),
            (3, CdeValue::UInt(self.key_epoch)),
            (4, CdeValue::UInt(u64::from(u8::from(self.kind)))),
            (5, CdeValue::UInt(u64::from(self.wrap_suite))),
            (6, self.kdf.to_cde()?),
            (7, CdeValue::UInt(self.created_at)),
            (8, CdeValue::UInt(self.manager_rev)),
        ]))
    }

    pub fn validate(&self) -> Result<(), ProtoError> {
        if self.protocol_version != 1 {
            return Err(ProtoError::UnsupportedVersion(self.protocol_version));
        }
        if self.key_epoch == 0 {
            return Err(ProtoError::Invalid(
                "SlotDescriptorV1.key_epoch 必须 >0".to_owned(),
            ));
        }
        if self.wrap_suite != 1 {
            return Err(ProtoError::Invalid(
                "SlotDescriptorV1.wrap_suite 必须为 1".to_owned(),
            ));
        }
        Ok(())
    }
}

/// RootSecretV1；只在槽/HPKE 内部使用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootSecretV1 {
    pub vault_id: VaultId,
    pub key_epoch: u64,
    pub root: Sha256,
}

impl RootSecretV1 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        let fields = into_record_fields(decode(bytes)?, 3)?;
        let value = Self {
            vault_id: expect_bytes_n::<16>(fields[0].clone(), "RootSecretV1.vault_id")?,
            key_epoch: expect_i64_max(fields[1].clone(), "RootSecretV1.key_epoch")?,
            root: expect_bytes_n::<32>(fields[2].clone(), "RootSecretV1.root")?,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.validate()?;
        Ok(CdeValue::Map(vec![
            (0, CdeValue::Bytes(self.vault_id.to_vec())),
            (1, CdeValue::UInt(self.key_epoch)),
            (2, CdeValue::Bytes(self.root.to_vec())),
        ]))
    }

    pub fn validate(&self) -> Result<(), ProtoError> {
        if self.key_epoch == 0 {
            return Err(ProtoError::Invalid(
                "RootSecretV1.key_epoch 必须 >0".to_owned(),
            ));
        }
        Ok(())
    }
}

/// DeviceCertV1；由管理根/control 链签名。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceCertV1 {
    pub vault_id: VaultId,
    pub device_id: DeviceId,
    pub sign_ed25519_pub: Ed25519PublicKey,
    pub hpke_x25519_pub: X25519PublicKey,
    pub cert_serial: u64,
    pub issued_manager_rev: u64,
    pub rights: Rights,
}

impl DeviceCertV1 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        let fields = into_record_fields(decode(bytes)?, 7)?;
        let value = Self {
            vault_id: expect_bytes_n::<16>(fields[0].clone(), "DeviceCertV1.vault_id")?,
            device_id: expect_bytes_n::<16>(fields[1].clone(), "DeviceCertV1.device_id")?,
            sign_ed25519_pub: expect_bytes_n::<32>(fields[2].clone(), "DeviceCertV1.sign_pub")?,
            hpke_x25519_pub: expect_bytes_n::<32>(fields[3].clone(), "DeviceCertV1.hpke_pub")?,
            cert_serial: expect_i64_max(fields[4].clone(), "DeviceCertV1.cert_serial")?,
            issued_manager_rev: expect_i64_max(
                fields[5].clone(),
                "DeviceCertV1.issued_manager_rev",
            )?,
            rights: Rights::new(crate::cde::expect_u32(
                fields[6].clone(),
                "DeviceCertV1.rights",
            )?)?,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.validate()?;
        Ok(CdeValue::Map(vec![
            (0, CdeValue::Bytes(self.vault_id.to_vec())),
            (1, CdeValue::Bytes(self.device_id.to_vec())),
            (2, CdeValue::Bytes(self.sign_ed25519_pub.to_vec())),
            (3, CdeValue::Bytes(self.hpke_x25519_pub.to_vec())),
            (4, CdeValue::UInt(self.cert_serial)),
            (5, CdeValue::UInt(self.issued_manager_rev)),
            (6, CdeValue::UInt(u64::from(self.rights.bits()))),
        ]))
    }

    pub fn validate(&self) -> Result<(), ProtoError> {
        if self.cert_serial == 0 {
            return Err(ProtoError::Invalid(
                "DeviceCertV1.cert_serial 必须 >0".to_owned(),
            ));
        }
        Rights::new(self.rights.bits())?;
        Ok(())
    }
}

/// 鉴权 challenge。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Challenge {
    pub pv: u16,
    pub relay_instance_id: RelayInstanceId,
    pub origin_sha256: Sha256,
    pub vault_id: VaultId,
    pub realm: AuthRealm,
    pub subject: DeviceId,
    pub challenge_id: ChallengeId,
    pub nonce: Sha256,
    pub expires_at: u64,
}

impl Challenge {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        let fields = into_record_fields(decode(bytes)?, 9)?;
        let value = Self {
            pv: expect_u16(fields[0].clone(), "Challenge.pv")?,
            relay_instance_id: expect_bytes_n::<16>(
                fields[1].clone(),
                "Challenge.relay_instance_id",
            )?,
            origin_sha256: expect_bytes_n::<32>(fields[2].clone(), "Challenge.origin_sha256")?,
            vault_id: expect_bytes_n::<16>(fields[3].clone(), "Challenge.vault_id")?,
            realm: AuthRealm::try_from(crate::cde::expect_u8(
                fields[4].clone(),
                "Challenge.realm",
            )?)?,
            subject: expect_bytes_n::<16>(fields[5].clone(), "Challenge.subject")?,
            challenge_id: expect_bytes_n::<16>(fields[6].clone(), "Challenge.challenge_id")?,
            nonce: expect_bytes_n::<32>(fields[7].clone(), "Challenge.nonce")?,
            expires_at: expect_i64_max(fields[8].clone(), "Challenge.expires_at")?,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.validate()?;
        Ok(CdeValue::Map(vec![
            (0, CdeValue::UInt(u64::from(self.pv))),
            (1, CdeValue::Bytes(self.relay_instance_id.to_vec())),
            (2, CdeValue::Bytes(self.origin_sha256.to_vec())),
            (3, CdeValue::Bytes(self.vault_id.to_vec())),
            (4, CdeValue::UInt(u64::from(u8::from(self.realm)))),
            (5, CdeValue::Bytes(self.subject.to_vec())),
            (6, CdeValue::Bytes(self.challenge_id.to_vec())),
            (7, CdeValue::Bytes(self.nonce.to_vec())),
            (8, CdeValue::UInt(self.expires_at)),
        ]))
    }

    pub fn validate(&self) -> Result<(), ProtoError> {
        if self.pv != 1 {
            return Err(ProtoError::UnsupportedVersion(self.pv));
        }
        Ok(())
    }
}

/// HPKE grant 的 AAD（ADR 0006 §17.2）；字段按整数键 0..6 声明。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantAadV1 {
    pub pv: u16,
    pub vault_id: VaultId,
    pub epoch: u64,
    pub recipient_device_id: DeviceId,
    pub recipient_x25519_pub: X25519PublicKey,
    pub manager_rev: u64,
    pub grant_id: [u8; 16],
}

impl GrantAadV1 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        let fields = into_record_fields(decode(bytes)?, 7)?;
        let value = Self {
            pv: expect_u16(fields[0].clone(), "GrantAadV1.pv")?,
            vault_id: expect_bytes_n::<16>(fields[1].clone(), "GrantAadV1.vault_id")?,
            epoch: expect_i64_max(fields[2].clone(), "GrantAadV1.epoch")?,
            recipient_device_id: expect_bytes_n::<16>(
                fields[3].clone(),
                "GrantAadV1.recipient_device_id",
            )?,
            recipient_x25519_pub: expect_bytes_n::<32>(
                fields[4].clone(),
                "GrantAadV1.recipient_x25519_pub",
            )?,
            manager_rev: expect_i64_max(fields[5].clone(), "GrantAadV1.manager_rev")?,
            grant_id: expect_bytes_n::<16>(fields[6].clone(), "GrantAadV1.grant_id")?,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.validate()?;
        Ok(CdeValue::Map(vec![
            (0, CdeValue::UInt(u64::from(self.pv))),
            (1, CdeValue::Bytes(self.vault_id.to_vec())),
            (2, CdeValue::UInt(self.epoch)),
            (3, CdeValue::Bytes(self.recipient_device_id.to_vec())),
            (4, CdeValue::Bytes(self.recipient_x25519_pub.to_vec())),
            (5, CdeValue::UInt(self.manager_rev)),
            (6, CdeValue::Bytes(self.grant_id.to_vec())),
        ]))
    }

    pub fn validate(&self) -> Result<(), ProtoError> {
        if self.pv != 1 {
            return Err(ProtoError::UnsupportedVersion(self.pv));
        }
        if self.epoch == 0 {
            return Err(ProtoError::Invalid("GrantAadV1.epoch 必须 >0".to_owned()));
        }
        Ok(())
    }

    /// AAD 字节 = `CDE(GrantAadV1)`。
    pub fn aad_bytes(&self) -> Result<Vec<u8>, ProtoError> {
        self.encode()
    }

    /// HPKE 外层管理签名待签字节 = `AAD || enc32 || sha256(hpke_ciphertext)`。
    ///
    /// 本函数只拼字节，不计算 SHA256；`ciphertext_sha256` 由 sync 计算。
    pub fn grant_signed_bytes(
        &self,
        enc: &[u8; 32],
        ciphertext_sha256: &[u8; 32],
    ) -> Result<Vec<u8>, ProtoError> {
        let aad = self.aad_bytes()?;
        let mut out = Vec::with_capacity(aad.len() + 32 + 32);
        out.extend_from_slice(&aad);
        out.extend_from_slice(enc);
        out.extend_from_slice(ciphertext_sha256);
        Ok(out)
    }
}

/// `DeviceCertV1` 的管理签名待签字节：`MN/v1/device-cert\0 || CDE(cert)`。
pub fn device_cert_signed_bytes(cert: &DeviceCertV1) -> Result<Vec<u8>, ProtoError> {
    let mut out = Vec::with_capacity(crate::constants::SIGN_DEVICE_CERT_DOMAIN.len());
    out.extend_from_slice(crate::constants::SIGN_DEVICE_CERT_DOMAIN);
    out.extend_from_slice(&cert.encode()?);
    Ok(out)
}

impl DeviceCertV1 {
    /// 管理签名待签字节；与 [`device_cert_signed_bytes`] 相同。
    pub fn device_cert_signed_bytes(&self) -> Result<Vec<u8>, ProtoError> {
        device_cert_signed_bytes(self)
    }
}

/// `GrantAadV1` 外层管理签名待签字节的便捷函数。
pub fn grant_signed_bytes(
    grant: &GrantAadV1,
    enc: &[u8; 32],
    ciphertext_sha256: &[u8; 32],
) -> Result<Vec<u8>, ProtoError> {
    grant.grant_signed_bytes(enc, ciphertext_sha256)
}

/// ObjectCommit 签名载荷。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectCommit {
    pub vault_id: VaultId,
    pub object_id: ObjectId,
    pub message_id: MessageId,
    pub object_type: ObjectType,
    pub generation: u64,
    pub key_epoch: u64,
    pub sender_seq: u64,
    pub part_count: u32,
    pub total_len: u64,
    pub object_digest: Sha256,
}

impl ObjectCommit {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        let fields = into_record_fields(decode(bytes)?, 10)?;
        let value = Self {
            vault_id: expect_bytes_n::<16>(fields[0].clone(), "ObjectCommit.vault_id")?,
            object_id: expect_bytes_n::<32>(fields[1].clone(), "ObjectCommit.object_id")?,
            message_id: expect_bytes_n::<16>(fields[2].clone(), "ObjectCommit.message_id")?,
            object_type: ObjectType::try_from(crate::cde::expect_u8(
                fields[3].clone(),
                "ObjectCommit.type",
            )?)?,
            generation: expect_i64_max(fields[4].clone(), "ObjectCommit.generation")?,
            key_epoch: expect_i64_max(fields[5].clone(), "ObjectCommit.key_epoch")?,
            sender_seq: expect_i64_max(fields[6].clone(), "ObjectCommit.sender_seq")?,
            part_count: crate::cde::expect_u32(fields[7].clone(), "ObjectCommit.part_count")?,
            total_len: expect_i64_max(fields[8].clone(), "ObjectCommit.total_len")?,
            object_digest: expect_bytes_n::<32>(fields[9].clone(), "ObjectCommit.object_digest")?,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.validate()?;
        Ok(CdeValue::Map(vec![
            (0, CdeValue::Bytes(self.vault_id.to_vec())),
            (1, CdeValue::Bytes(self.object_id.to_vec())),
            (2, CdeValue::Bytes(self.message_id.to_vec())),
            (3, CdeValue::UInt(u64::from(self.object_type.as_u8()))),
            (4, CdeValue::UInt(self.generation)),
            (5, CdeValue::UInt(self.key_epoch)),
            (6, CdeValue::UInt(self.sender_seq)),
            (7, CdeValue::UInt(u64::from(self.part_count))),
            (8, CdeValue::UInt(self.total_len)),
            (9, CdeValue::Bytes(self.object_digest.to_vec())),
        ]))
    }

    pub fn validate(&self) -> Result<(), ProtoError> {
        if self.key_epoch == 0 {
            return Err(ProtoError::Invalid(
                "ObjectCommit.key_epoch 必须 >0".to_owned(),
            ));
        }
        if self.part_count == 0 || self.total_len == 0 {
            return Err(ProtoError::Invalid(
                "ObjectCommit part/total 必须 >0".to_owned(),
            ));
        }
        if self.object_type.requires_generation() {
            if self.generation == 0 {
                return Err(ProtoError::Invalid(
                    "UPDATE/SNAPSHOT ObjectCommit.generation 必须 >0".to_owned(),
                ));
            }
        } else if self.generation != 0 {
            return Err(ProtoError::Invalid(
                "其他对象 ObjectCommit.generation 必须为 0".to_owned(),
            ));
        }
        let expected_parts = crate::limits::part_count_for(self.total_len, self.object_type);
        if u64::from(self.part_count) != expected_parts {
            return Err(ProtoError::Invalid(format!(
                "ObjectCommit.part_count={} 与固定切片 {expected_parts} 不符",
                self.part_count
            )));
        }
        if self.total_len > crate::limits::MAX_UPDATE_CDE_BYTES as u64
            && self.object_type != ObjectType::Asset
            && self.object_type != ObjectType::Snapshot
        {
            return Err(ProtoError::TooLarge);
        }
        match self.object_type {
            ObjectType::KeySlot
                if self.total_len > crate::limits::MAX_KEY_SLOT_PLAINTEXT as u64 =>
            {
                return Err(ProtoError::TooLarge)
            }
            // 与 `ObjectRefV1`/`AssetRefV1` 同口径：ASSET 明文上限 64MiB。缺这条时
            // `part_count=u32::MAX` + 自洽的 `total_len` 能通过 wire 校验并被消费端当成
            // 分配长度（`part_count_for` 只约束关系、不约束上界）。
            ObjectType::Asset if self.total_len > MAX_ASSET_RAW_BYTES as u64 => {
                return Err(ProtoError::TooLarge)
            }
            ObjectType::Manifest
                if self.total_len > crate::limits::MAX_MANIFEST_CDE_BYTES as u64 =>
            {
                return Err(ProtoError::TooLarge)
            }
            ObjectType::Snapshot
                if self.total_len > crate::limits::MAX_LORO_SNAPSHOT_BYTES as u64 =>
            {
                return Err(ProtoError::TooLarge)
            }
            _ => {}
        }
        Ok(())
    }
}

/// CheckpointCommit 签名载荷。
///
/// **wire 修订**（task-122）：
/// - `validation_version = 1`：字段 0..8（9 项），签名只覆盖原 v1 语义；
/// - `validation_version = 2`：追加字段 9 `validation_digest`（32B，见
///   [`crate::CheckpointValidationV2`] / [`crate::checkpoint_validation_input`]），
///   共 10 项。v2 是当前生产修订；旧修订仍可**解码**（用于识别/拒绝），但当前消费者
///   必须显式拒绝，不得静默接受。
///
/// 解码规则是"按显式版本号 + 字段集合严格匹配"，不做任何猜测：
/// - 9 字段 + `validation_version=1` → 合法 v1（`validation_digest` 为空零，不进 wire）；
/// - 10 字段 + `validation_version=2` → 合法 v2；
/// - 9 字段 + 版本 2、10 字段 + 版本 1 → 结构非法（缺/多余字段，拒绝）；
/// - 其它版本 → [`ProtoError::UnsupportedVersion`]。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointCommit {
    pub vault_id: VaultId,
    pub checkpoint_id: CheckpointId,
    pub epoch: u64,
    pub covered_relay_seq: u64,
    pub manifest_object_id: ObjectId,
    pub manifest_digest: Sha256,
    pub refs_digest: Sha256,
    pub producer_device_id: DeviceId,
    pub validation_version: u16,
    /// v2 起必填：验证修订 2 的证据摘要；v1 固定 `[0u8; 32]`，不编码进 wire。
    pub validation_digest: Sha256,
}

impl CheckpointCommit {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        let map = decode(bytes)?;
        let CdeValue::Map(entries) = map else {
            return Err(ProtoError::Invalid(
                "CheckpointCommit 必须是 map".to_owned(),
            ));
        };
        let field_count = entries.len();
        // v1 是 9 项，v2 是 10 项；其它数量在解析版本号之前就拒绝（不猜测）。
        if field_count != 9 && field_count != 10 {
            return Err(ProtoError::Invalid(format!(
                "CheckpointCommit 字段数 {field_count}，期望 9（v1）或 10（v2）"
            )));
        }
        let fields = into_record_fields(CdeValue::Map(entries), field_count)?;
        let validation_version =
            expect_u16(fields[8].clone(), "CheckpointCommit.validation_version")?;
        let validation_digest = match validation_version {
            1 if field_count == 9 => [0u8; 32],
            2 if field_count == 10 => {
                expect_bytes_n::<32>(fields[9].clone(), "CheckpointCommit.validation_digest")?
            }
            // 版本与字段集合必须严格匹配：9 字段的"版本 2"不能把缺失字段猜成 0，
            // 10 字段的"版本 1"也不能丢弃多余字段。
            1 => {
                return Err(ProtoError::Invalid(
                    "CheckpointCommit v1 不接受 validation_digest 字段".to_owned(),
                ))
            }
            2 => {
                return Err(ProtoError::MissingField(9));
            }
            other => return Err(ProtoError::UnsupportedVersion(other)),
        };
        let value = Self {
            vault_id: expect_bytes_n::<16>(fields[0].clone(), "CheckpointCommit.vault_id")?,
            checkpoint_id: expect_bytes_n::<16>(
                fields[1].clone(),
                "CheckpointCommit.checkpoint_id",
            )?,
            epoch: expect_i64_max(fields[2].clone(), "CheckpointCommit.epoch")?,
            covered_relay_seq: expect_i64_max(fields[3].clone(), "CheckpointCommit.S")?,
            manifest_object_id: expect_bytes_n::<32>(
                fields[4].clone(),
                "CheckpointCommit.manifest_oid",
            )?,
            manifest_digest: expect_bytes_n::<32>(
                fields[5].clone(),
                "CheckpointCommit.manifest_digest",
            )?,
            refs_digest: expect_bytes_n::<32>(fields[6].clone(), "CheckpointCommit.refs_digest")?,
            producer_device_id: expect_bytes_n::<16>(
                fields[7].clone(),
                "CheckpointCommit.producer_device_id",
            )?,
            validation_version,
            validation_digest,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.validate()?;
        let mut fields = vec![
            (0, CdeValue::Bytes(self.vault_id.to_vec())),
            (1, CdeValue::Bytes(self.checkpoint_id.to_vec())),
            (2, CdeValue::UInt(self.epoch)),
            (3, CdeValue::UInt(self.covered_relay_seq)),
            (4, CdeValue::Bytes(self.manifest_object_id.to_vec())),
            (5, CdeValue::Bytes(self.manifest_digest.to_vec())),
            (6, CdeValue::Bytes(self.refs_digest.to_vec())),
            (7, CdeValue::Bytes(self.producer_device_id.to_vec())),
            (8, CdeValue::UInt(u64::from(self.validation_version))),
        ];
        if self.validation_version == 2 {
            fields.push((9, CdeValue::Bytes(self.validation_digest.to_vec())));
        }
        Ok(CdeValue::Map(fields))
    }

    /// 当前生产验证修订。消费者必须用 [`Self::is_current_validation`] 显式拒绝旧修订。
    pub const CURRENT_VALIDATION_VERSION: u16 =
        crate::constants::CHECKPOINT_COMMIT_VALIDATION_VERSION;

    /// 是否为当前验证修订（task-122 之后只有 v2 可作为基线/GC 依据）。
    #[must_use]
    pub fn is_current_validation(&self) -> bool {
        self.validation_version == Self::CURRENT_VALIDATION_VERSION
    }

    /// 消费者门禁：不是当前修订就返回 [`ProtoError::UnsupportedVersion`]。
    ///
    /// 解码层仍允许 v1（用于识别/审计/负例），但任何"把 checkpoint 当基线/覆盖证据/
    /// GC 依据"的路径都必须调用本方法显式拒绝旧修订，不得静默接受。
    pub fn require_current_validation(&self) -> Result<(), ProtoError> {
        if self.is_current_validation() {
            Ok(())
        } else {
            Err(ProtoError::UnsupportedVersion(self.validation_version))
        }
    }

    pub fn validate(&self) -> Result<(), ProtoError> {
        if self.epoch == 0 {
            return Err(ProtoError::Invalid(
                "CheckpointCommit.epoch 必须 >0".to_owned(),
            ));
        }
        match self.validation_version {
            1 => {
                if self.validation_digest != [0u8; 32] {
                    return Err(ProtoError::Invalid(
                        "CheckpointCommit v1 的 validation_digest 必须为空零".to_owned(),
                    ));
                }
            }
            2 => {
                if self.validation_digest == [0u8; 32] {
                    return Err(ProtoError::Invalid(
                        "CheckpointCommit v2 缺少 validation_digest".to_owned(),
                    ));
                }
            }
            other => return Err(ProtoError::UnsupportedVersion(other)),
        }
        Ok(())
    }
}

/// 验证修订 2 的证据（本地/审计输入；**不直接上 wire**）。
///
/// 写入 `CheckpointCommit.validation_digest` 的是
/// `SHA256(checkpoint_validation_input(self))`。它把"这份 checkpoint 的 validate
/// 真实做过的覆盖复核事实"绑定进生产者签名：
/// - 覆盖扫描冻结 epoch、S、manifest/refs 与生产者；
/// - 实际导入的文档数/actual VV 摘要、实证资产数；
/// - coverage 复核的 durable/实证候选数与证明摘要。
///
/// relay 只能看到 32B 摘要（无法复算，这不是问题：它不声称验证过覆盖）；客户端/测试
/// 可从本地 `VerifiedBaseline` 事实重算并与签名 commit 对撞。旧修订没有该字段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointValidationV2 {
    pub vault_id: VaultId,
    pub checkpoint_id: CheckpointId,
    pub epoch: u64,
    pub covered_relay_seq: u64,
    /// prepare 时冻结的 relay active epoch（候选基线 = `from_epoch`）。
    pub coverage_epoch: u64,
    pub manifest_object_id: ObjectId,
    pub manifest_digest: Sha256,
    pub refs_digest: Sha256,
    pub producer_device_id: DeviceId,
    pub imported_docs: u64,
    pub imported_vv_digest: Sha256,
    pub verified_assets: u64,
    pub coverage_durable_assets: u64,
    pub coverage_verified_candidates: u64,
    pub coverage_proof_digest: Sha256,
}

impl CheckpointValidationV2 {
    /// 确定性 CDE 编码（字段 0..14）。
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        for (name, value) in [
            ("epoch", self.epoch),
            ("S", self.covered_relay_seq),
            ("coverage_epoch", self.coverage_epoch),
            ("imported_docs", self.imported_docs),
            ("verified_assets", self.verified_assets),
            ("coverage_durable_assets", self.coverage_durable_assets),
            (
                "coverage_verified_candidates",
                self.coverage_verified_candidates,
            ),
        ] {
            if value > i64::MAX as u64 {
                return Err(ProtoError::Invalid(format!(
                    "CheckpointValidationV2.{name} 超过 i64::MAX"
                )));
            }
        }
        if self.epoch == 0 || self.coverage_epoch > self.epoch {
            return Err(ProtoError::Invalid(
                "CheckpointValidationV2 的 coverage_epoch 必须 <= epoch 且 epoch > 0".to_owned(),
            ));
        }
        encode(&CdeValue::Map(vec![
            (0, CdeValue::Bytes(self.vault_id.to_vec())),
            (1, CdeValue::Bytes(self.checkpoint_id.to_vec())),
            (2, CdeValue::UInt(self.epoch)),
            (3, CdeValue::UInt(self.covered_relay_seq)),
            (4, CdeValue::UInt(self.coverage_epoch)),
            (5, CdeValue::Bytes(self.manifest_object_id.to_vec())),
            (6, CdeValue::Bytes(self.manifest_digest.to_vec())),
            (7, CdeValue::Bytes(self.refs_digest.to_vec())),
            (8, CdeValue::Bytes(self.producer_device_id.to_vec())),
            (9, CdeValue::UInt(self.imported_docs)),
            (10, CdeValue::Bytes(self.imported_vv_digest.to_vec())),
            (11, CdeValue::UInt(self.verified_assets)),
            (12, CdeValue::UInt(self.coverage_durable_assets)),
            (13, CdeValue::UInt(self.coverage_verified_candidates)),
            (14, CdeValue::Bytes(self.coverage_proof_digest.to_vec())),
        ]))
    }
}

/// 公开对象引用（manifest/GC 共用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectRefV1 {
    pub object_id: ObjectId,
    pub object_type: ObjectType,
    pub generation: u64,
    pub key_epoch: u64,
    pub part_count: u32,
    pub total_plain_len: u64,
    pub object_digest: Sha256,
}

impl ObjectRefV1 {
    pub fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.validate()?;
        Ok(CdeValue::Map(vec![
            (0, CdeValue::Bytes(self.object_id.to_vec())),
            (1, CdeValue::UInt(u64::from(self.object_type.as_u8()))),
            (2, CdeValue::UInt(self.generation)),
            (3, CdeValue::UInt(self.key_epoch)),
            (4, CdeValue::UInt(u64::from(self.part_count))),
            (5, CdeValue::UInt(self.total_plain_len)),
            (6, CdeValue::Bytes(self.object_digest.to_vec())),
        ]))
    }

    pub fn from_cde(value: CdeValue) -> Result<Self, ProtoError> {
        let fields = into_record_fields(value, 7)?;
        let value = Self {
            object_id: expect_bytes_n::<32>(fields[0].clone(), "ObjectRefV1.object_id")?,
            object_type: ObjectType::try_from(crate::cde::expect_u8(
                fields[1].clone(),
                "ObjectRefV1.type",
            )?)?,
            generation: expect_i64_max(fields[2].clone(), "ObjectRefV1.generation")?,
            key_epoch: expect_i64_max(fields[3].clone(), "ObjectRefV1.key_epoch")?,
            part_count: crate::cde::expect_u32(fields[4].clone(), "ObjectRefV1.part_count")?,
            total_plain_len: expect_i64_max(fields[5].clone(), "ObjectRefV1.total_plain_len")?,
            object_digest: expect_bytes_n::<32>(fields[6].clone(), "ObjectRefV1.object_digest")?,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        Self::from_cde(decode(bytes)?)
    }

    pub fn validate(&self) -> Result<(), ProtoError> {
        if self.key_epoch == 0 {
            return Err(ProtoError::Invalid(
                "ObjectRefV1.key_epoch 必须 >0".to_owned(),
            ));
        }
        if self.part_count == 0 || self.total_plain_len == 0 {
            return Err(ProtoError::Invalid(
                "ObjectRefV1 part/total 必须 >0".to_owned(),
            ));
        }
        if self.object_type.requires_generation() {
            if self.generation == 0 {
                return Err(ProtoError::Invalid(
                    "UPDATE/SNAPSHOT ObjectRefV1.generation 必须 >0".to_owned(),
                ));
            }
        } else if self.generation != 0 {
            return Err(ProtoError::Invalid(
                "其他对象 ObjectRefV1.generation 必须为 0".to_owned(),
            ));
        }
        let expected_parts = crate::limits::part_count_for(self.total_plain_len, self.object_type);
        if u64::from(self.part_count) != expected_parts {
            return Err(ProtoError::Invalid(format!(
                "ObjectRefV1.part_count={} 与固定切片 {expected_parts} 不符",
                self.part_count
            )));
        }
        match self.object_type {
            ObjectType::Update | ObjectType::Manifest
                if self.total_plain_len > crate::limits::MAX_UPDATE_CDE_BYTES as u64 =>
            {
                return Err(ProtoError::TooLarge)
            }
            ObjectType::Asset
                if self.total_plain_len > crate::limits::MAX_ASSET_RAW_BYTES as u64 =>
            {
                return Err(ProtoError::TooLarge)
            }
            ObjectType::Snapshot
                if self.total_plain_len > crate::limits::MAX_LORO_SNAPSHOT_BYTES as u64 =>
            {
                return Err(ProtoError::TooLarge)
            }
            ObjectType::KeySlot
                if self.total_plain_len > crate::limits::MAX_KEY_SLOT_PLAINTEXT as u64 =>
            {
                return Err(ProtoError::TooLarge)
            }
            _ => {}
        }
        Ok(())
    }
}

/// `refs_digest` 的待编码输入：按 object_id 字节升序的 `CDE(array<ObjectRefV1>)`。
/// 本函数不计算 SHA256（sync-proto 不做 crypto）。
pub fn refs_digest_input(refs: &[ObjectRefV1]) -> Result<Vec<u8>, ProtoError> {
    if refs.len() > MAX_REFS {
        return Err(ProtoError::TooLarge);
    }
    let mut refs = refs.to_vec();
    refs.sort_by_key(|a| a.object_id);
    for reference in &refs {
        if !matches!(
            reference.object_type,
            ObjectType::Snapshot | ObjectType::Asset
        ) {
            return Err(ProtoError::Invalid(
                "refs_digest 只覆盖 snapshot/asset 数据对象".to_owned(),
            ));
        }
    }
    for pair in refs.windows(2) {
        if pair[0].object_id == pair[1].object_id {
            return Err(ProtoError::Invalid("refs 中 object_id 重复".to_owned()));
        }
    }
    let values = refs
        .iter()
        .map(ObjectRefV1::to_cde)
        .collect::<Result<Vec<_>, _>>()?;
    encode(&CdeValue::Array(values))
}

/// `object_digest` 的待哈希输入：
/// `BE32(part_count) || concat(BE32(part_index) || SHA256(exact_envelope_part))`。
pub fn object_digest_input(
    part_count: u32,
    part_hashes: &[[u8; 32]],
) -> Result<Vec<u8>, ProtoError> {
    if part_count == 0 {
        return Err(ProtoError::Invalid("part_count 必须 ≥1".to_owned()));
    }
    if part_hashes.len() != part_count as usize {
        return Err(ProtoError::Invalid(format!(
            "part_count={part_count} 但提供 {} 个 part hash",
            part_hashes.len()
        )));
    }
    if part_hashes.len() > u32::MAX as usize {
        return Err(ProtoError::TooLarge);
    }
    let mut out = Vec::with_capacity(4 + part_hashes.len() * 36);
    out.extend_from_slice(&part_count.to_be_bytes());
    for (index, hash) in part_hashes.iter().enumerate() {
        out.extend_from_slice(&(index as u32).to_be_bytes());
        out.extend_from_slice(hash);
    }
    Ok(out)
}

/// 管理请求签名的待编码输入。`path` 必须是规范化后的实际路径。
pub fn management_signing_input(
    method: HttpMethod,
    path: &str,
    unsigned_body: &[u8],
) -> Result<Vec<u8>, ProtoError> {
    if !path.starts_with('/') {
        return Err(ProtoError::Invalid(
            "management path 必须是绝对路径".to_owned(),
        ));
    }
    if path.len() > 4096 {
        return Err(ProtoError::TooLarge);
    }
    let mut out = Vec::with_capacity(1 + path.len() + 4 + unsigned_body.len());
    out.push(u8::from(method));
    out.extend_from_slice(path.as_bytes());
    out.extend_from_slice(&(unsigned_body.len() as u32).to_be_bytes());
    out.extend_from_slice(unsigned_body);
    Ok(out)
}

/// 校验设备数上限（供 sync/relay 使用）。
pub fn check_device_count(count: usize) -> Result<(), ProtoError> {
    if count > MAX_DEVICES {
        return Err(ProtoError::TooLarge);
    }
    Ok(())
}

`````



@@SECTION code/067 `crates/sync-proto/src/rotation_records.rs`


`````rust
//! ADR 0006 §30.2：relay 轮换新增的共享 CDE 记录。
//!
//! 字段编号与 `crates/relay/src/{control,rotations,api}.rs` / ADR §30 逐字节一致；
//! 本模块只做 canonical 编解码与 digest 的**字节输入**，不做 SHA-256、不碰 SQLite。
//!
//! `SignedControlRecord` 的 `event` 保存 relay 事件 CDE 原字节；客户端按 kind 用
//! [`EpochActivated::decode`]、[`RotationRestarted::decode`]、[`RevocationsAdded::decode`]、
//! [`RotationAborted::decode`] 解码。

use std::collections::BTreeMap;

use crate::activation_t::ActivationTV1;
use crate::cde::{
    decode, encode, expect_array, expect_bool, expect_bytes, expect_bytes_n, expect_i64_max,
    expect_text, expect_u8, into_record_fields, CdeValue,
};
use crate::error::ProtoError;
use crate::records::{DeviceId, HttpMethod};

pub type Sha256 = [u8; 32];
pub type CheckpointId = [u8; 16];
pub type RotationId = [u8; 16];

fn record_map(fields: Vec<(u64, CdeValue)>) -> CdeValue {
    CdeValue::Map(fields)
}

fn optional_fields(
    value: CdeValue,
    allowed: &[u64],
    required: &[u64],
) -> Result<BTreeMap<u64, CdeValue>, ProtoError> {
    let CdeValue::Map(entries) = value else {
        return Err(ProtoError::Invalid("record 必须是 map".to_owned()));
    };
    let mut fields = BTreeMap::new();
    for (key, value) in entries {
        if !allowed.contains(&key) {
            return Err(ProtoError::UnknownField(key));
        }
        fields.insert(key, value);
    }
    for key in required {
        if !fields.contains_key(key) {
            return Err(ProtoError::MissingField(*key));
        }
    }
    Ok(fields)
}

fn take(fields: &mut BTreeMap<u64, CdeValue>, key: u64) -> Result<CdeValue, ProtoError> {
    fields.remove(&key).ok_or(ProtoError::MissingField(key))
}

fn take_uint(
    fields: &mut BTreeMap<u64, CdeValue>,
    key: u64,
    name: &str,
) -> Result<u64, ProtoError> {
    expect_i64_max(take(fields, key)?, name)
}

fn take_id16(
    fields: &mut BTreeMap<u64, CdeValue>,
    key: u64,
    name: &str,
) -> Result<[u8; 16], ProtoError> {
    expect_bytes_n::<16>(take(fields, key)?, name)
}

fn take_hash32(
    fields: &mut BTreeMap<u64, CdeValue>,
    key: u64,
    name: &str,
) -> Result<[u8; 32], ProtoError> {
    expect_bytes_n::<32>(take(fields, key)?, name)
}

/// 控制链 kind 的 wire 文本（与 relay `ControlKind::as_str` 一致）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlKind {
    RotationBegin,
    RotationAbort,
    EpochActivated,
    RotationRestarted,
    RevocationsAdded,
}

impl ControlKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RotationBegin => "rotation.begin",
            Self::RotationAbort => "rotation.abort",
            Self::EpochActivated => "epoch.activated",
            Self::RotationRestarted => "rotation.restarted",
            Self::RevocationsAdded => "rotation.revoked-more",
        }
    }

    pub fn parse(value: &str) -> Result<Self, ProtoError> {
        match value {
            "rotation.begin" => Ok(Self::RotationBegin),
            "rotation.abort" => Ok(Self::RotationAbort),
            "epoch.activated" => Ok(Self::EpochActivated),
            "rotation.restarted" => Ok(Self::RotationRestarted),
            "rotation.revoked-more" => Ok(Self::RevocationsAdded),
            _ => Err(ProtoError::Invalid(format!("未知控制记录 kind {value}"))),
        }
    }
}

/// `SignedControlRecord`（relay `control.rs` 的 0..6 字段）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedControlRecord {
    pub kind: ControlKind,
    pub request_id: DeviceId,
    pub manager_rev: u64,
    pub expected_control_rev: u64,
    pub signed_body: Vec<u8>,
    pub signature: Vec<u8>,
    /// 可选 typed 事件 CDE 原字节（kind=epoch.activated / rotation.abort /
    /// rotation.restarted / rotation.revoked-more 时存在）。
    pub event: Option<Vec<u8>>,
}

impl SignedControlRecord {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        let mut fields = vec![
            (0, CdeValue::Text(self.kind.as_str().to_owned())),
            (1, CdeValue::Bytes(self.request_id.to_vec())),
            (2, CdeValue::UInt(self.manager_rev)),
            (3, CdeValue::UInt(self.expected_control_rev)),
            (4, CdeValue::Bytes(self.signed_body.clone())),
            (5, CdeValue::Bytes(self.signature.clone())),
        ];
        if let Some(event) = &self.event {
            fields.push((6, CdeValue::Bytes(event.clone())));
        }
        encode(&record_map(fields))
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        let value = decode(bytes)?;
        let mut fields = optional_fields(value, &[0, 1, 2, 3, 4, 5, 6], &[0, 1, 2, 3, 4, 5])?;
        let kind = ControlKind::parse(&expect_text(
            take(&mut fields, 0)?,
            "SignedControlRecord.kind",
        )?)?;
        let record = Self {
            kind,
            request_id: take_id16(&mut fields, 1, "SignedControlRecord.request_id")?,
            manager_rev: take_uint(&mut fields, 2, "SignedControlRecord.manager_rev")?,
            expected_control_rev: take_uint(
                &mut fields,
                3,
                "SignedControlRecord.expected_control_rev",
            )?,
            signed_body: expect_bytes(take(&mut fields, 4)?, "SignedControlRecord.signed_body")?,
            signature: expect_bytes(take(&mut fields, 5)?, "SignedControlRecord.signature")?,
            event: fields
                .remove(&6)
                .map(|value| expect_bytes(value, "SignedControlRecord.event"))
                .transpose()?,
        };
        Ok(record)
    }

    /// 按 kind 解码 event；kind 为 begin 或 event 缺失时返回 `Ok(None)`。
    pub fn decoded_event(&self) -> Result<Option<ControlEvent>, ProtoError> {
        let Some(bytes) = &self.event else {
            return Ok(None);
        };
        let event = match self.kind {
            ControlKind::EpochActivated => {
                ControlEvent::EpochActivated(EpochActivated::decode(bytes)?)
            }
            ControlKind::RotationAbort => {
                ControlEvent::RotationAborted(RotationAborted::decode(bytes)?)
            }
            ControlKind::RotationRestarted => {
                ControlEvent::RotationRestarted(RotationRestarted::decode(bytes)?)
            }
            ControlKind::RevocationsAdded => {
                ControlEvent::RevocationsAdded(RevocationsAdded::decode(bytes)?)
            }
            ControlKind::RotationBegin => {
                return Err(ProtoError::Invalid(
                    "rotation.begin 不应带 typed event".to_owned(),
                ));
            }
        };
        Ok(Some(event))
    }
}

/// `SignedControlRecord` 携带的 typed 事件。
///
/// `EpochActivated` 现在携带 T 与双签（ADR §34），比其它变体大；这是低频控制
/// 事件的解码结果，不值得为 288B 引入 Box 间接层与 API 变更。
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlEvent {
    EpochActivated(EpochActivated),
    RotationAborted(RotationAborted),
    RotationRestarted(RotationRestarted),
    RevocationsAdded(RevocationsAdded),
}

/// `rotation.abort` 的 typed 事件（键 0..3）：重建管理签名输入所需的**完整被签事实**。
///
/// 背景（ADR 0006 §35.3）：abort 的请求 path 含 rotation id，而 canonical body 是
/// 空 map；relay 只存 body + 签名时，客户端无法重建
/// `management_signing_input(instance, vault, request_id, expected_rev,
/// SHA256(CDE{method, path, body}))`。本事件把 path/method/body digest 一起写进控制记录，
/// 使 abort 变成**可验签事实**而不是"结构上像签名"的记录。
///
/// 字段：
/// - 0 rotation_id(bytes16)：被废弃的轮换 id（必须与 path 中的 id 一致）；
/// - 1 method(uint)：管理签名标签（GET=1..DELETE=5；abort 固定 POST=2）；
/// - 2 path(text)：被签名的请求 path（`/v1/vaults/{v}/rotations/{r}/abort`）；
/// - 3 body_digest(bytes32)：`SHA256(record.signed_body)`，用于交叉校验 body 未被替换。
///
/// **安全边界**：事件本身不单独被签名；它只是把签名输入所需的公开事实显式化。
/// 客户端必须用 `record.signed_body` 复用同一 digest 重建签名输入再验管理签名，
/// 事件字段任一被改都会让验签失败（或被 path/digest 一致性检查拒绝）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RotationAborted {
    /// 被废弃的轮换 id。
    pub rotation_id: RotationId,
    /// 管理签名覆盖的 HTTP method（abort 固定 [`HttpMethod::Post`]）。
    pub method: HttpMethod,
    /// 管理签名覆盖的请求 path。
    pub path: String,
    /// `SHA256(record.signed_body)`。
    pub body_digest: Sha256,
}

impl RotationAborted {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&record_map(vec![
            (0, CdeValue::Bytes(self.rotation_id.to_vec())),
            (1, CdeValue::UInt(u64::from(u8::from(self.method)))),
            (2, CdeValue::Text(self.path.clone())),
            (3, CdeValue::Bytes(self.body_digest.to_vec())),
        ]))
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        let fields = into_record_fields(decode(bytes)?, 4)?;
        let mut fields = fields
            .into_iter()
            .enumerate()
            .map(|(index, value)| (index as u64, value))
            .collect::<BTreeMap<_, _>>();
        Ok(Self {
            rotation_id: take_id16(&mut fields, 0, "RotationAborted.rotation_id")?,
            method: HttpMethod::try_from(expect_u8(
                take(&mut fields, 1)?,
                "RotationAborted.method",
            )?)?,
            path: expect_text(take(&mut fields, 2)?, "RotationAborted.path")?,
            body_digest: take_hash32(&mut fields, 3, "RotationAborted.body_digest")?,
        })
    }
}

/// `epoch.activated`（relay `EpochActivated`，键 0..15，13–15 可省略）。
///
/// ADR 0006 §34：新记录必须携带本次激活的完整 activation T（`ActivationTV1` 的
/// canonical CDE 原字节）与旧/新管理 key 对 `activation_t_signing_input(T)` 的双签，
/// 客户端据此从 `/control` 独立复算并验签。13–15 全体省略只用于兼容升级前的历史
/// 控制记录（它们不带 T，客户端应视为"无法复算"而不是"复算通过"）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpochActivated {
    pub vault_id: [u8; 16],
    pub instance_id: [u8; 16],
    pub rotation_id: RotationId,
    pub from_epoch: u64,
    pub to_epoch: u64,
    pub covered_seq: u64,
    pub primary_checkpoint: CheckpointId,
    pub fallback_checkpoint: CheckpointId,
    /// `ActivationCommitV1` 的 SHA-256。
    pub activation_commit: Sha256,
    pub member_set_hash: Sha256,
    pub grants_digest: Sha256,
    pub control_rev: u64,
    pub auth_rev: u64,
    /// 13：complete activation T 的 canonical CDE 原字节。
    pub activation_t: Option<Vec<u8>>,
    /// 14：旧管理 key 对 `activation_t_signing_input(T)` 的 64B 签名。
    pub old_t_signature: Option<Vec<u8>>,
    /// 15：新管理 key 对同一输入的 64B 签名。
    pub new_t_signature: Option<Vec<u8>>,
}

impl EpochActivated {
    /// `activation_commit` 字段的别名（语义为 ActivationCommitV1 的 SHA-256）。
    #[must_use]
    pub fn activation_commit_hash(&self) -> Sha256 {
        self.activation_commit
    }

    /// T 与双签必须同时存在；存在时签名各 64B，且 T 内的重复事实必须与本事件一致。
    fn check_activation_t_attachment(&self) -> Result<(), ProtoError> {
        let (Some(bytes), Some(old), Some(new)) = (
            &self.activation_t,
            &self.old_t_signature,
            &self.new_t_signature,
        ) else {
            if self.activation_t.is_none()
                && self.old_t_signature.is_none()
                && self.new_t_signature.is_none()
            {
                return Ok(());
            }
            return Err(ProtoError::Invalid(
                "EpochActivated 的 activation_t 与双签必须同时存在".to_owned(),
            ));
        };
        if old.len() != 64 || new.len() != 64 {
            return Err(ProtoError::Invalid(
                "EpochActivated 的 T 双签必须各 64 字节".to_owned(),
            ));
        }
        let activation_t = ActivationTV1::decode(bytes)?;
        let matches = activation_t.vault_id == self.vault_id
            && activation_t.instance_id == self.instance_id
            && activation_t.rotation_id == self.rotation_id
            && activation_t.from_epoch == self.from_epoch
            && activation_t.to_epoch == self.to_epoch
            && activation_t.freeze_seq == self.covered_seq
            && activation_t.primary_cid == self.primary_checkpoint
            && activation_t.fallback_cid == self.fallback_checkpoint
            && activation_t.grants_digest == self.grants_digest
            && activation_t.expected_control_rev == self.control_rev;
        if !matches {
            return Err(ProtoError::Invalid(
                "EpochActivated 与 activation_t 的重复事实不一致".to_owned(),
            ));
        }
        Ok(())
    }

    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        self.check_activation_t_attachment()?;
        let mut fields = vec![
            (0, CdeValue::Bytes(self.vault_id.to_vec())),
            (1, CdeValue::Bytes(self.instance_id.to_vec())),
            (2, CdeValue::Bytes(self.rotation_id.to_vec())),
            (3, CdeValue::UInt(self.from_epoch)),
            (4, CdeValue::UInt(self.to_epoch)),
            (5, CdeValue::UInt(self.covered_seq)),
            (6, CdeValue::Bytes(self.primary_checkpoint.to_vec())),
            (7, CdeValue::Bytes(self.fallback_checkpoint.to_vec())),
            (8, CdeValue::Bytes(self.activation_commit.to_vec())),
            (9, CdeValue::Bytes(self.member_set_hash.to_vec())),
            (10, CdeValue::Bytes(self.grants_digest.to_vec())),
            (11, CdeValue::UInt(self.control_rev)),
            (12, CdeValue::UInt(self.auth_rev)),
        ];
        if let Some(activation_t) = &self.activation_t {
            // `check_activation_t_attachment` 已确保 14/15 同时存在。
            fields.push((13, CdeValue::Bytes(activation_t.clone())));
            fields.push((
                14,
                CdeValue::Bytes(
                    self.old_t_signature.clone().ok_or_else(|| {
                        ProtoError::Invalid("EpochActivated.old_t_signature".into())
                    })?,
                ),
            ));
            fields.push((
                15,
                CdeValue::Bytes(
                    self.new_t_signature.clone().ok_or_else(|| {
                        ProtoError::Invalid("EpochActivated.new_t_signature".into())
                    })?,
                ),
            ));
        }
        encode(&record_map(fields))
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        let mut fields = optional_fields(
            decode(bytes)?,
            &(0..=15).collect::<Vec<_>>(),
            &(0..=12).collect::<Vec<_>>(),
        )?;
        let record = Self {
            vault_id: take_id16(&mut fields, 0, "EpochActivated.vault_id")?,
            instance_id: take_id16(&mut fields, 1, "EpochActivated.instance_id")?,
            rotation_id: take_id16(&mut fields, 2, "EpochActivated.rotation_id")?,
            from_epoch: take_uint(&mut fields, 3, "EpochActivated.from_epoch")?,
            to_epoch: take_uint(&mut fields, 4, "EpochActivated.to_epoch")?,
            covered_seq: take_uint(&mut fields, 5, "EpochActivated.covered_seq")?,
            primary_checkpoint: take_id16(&mut fields, 6, "EpochActivated.primary_checkpoint")?,
            fallback_checkpoint: take_id16(&mut fields, 7, "EpochActivated.fallback_checkpoint")?,
            activation_commit: take_hash32(&mut fields, 8, "EpochActivated.activation_commit")?,
            member_set_hash: take_hash32(&mut fields, 9, "EpochActivated.member_set_hash")?,
            grants_digest: take_hash32(&mut fields, 10, "EpochActivated.grants_digest")?,
            control_rev: take_uint(&mut fields, 11, "EpochActivated.control_rev")?,
            auth_rev: take_uint(&mut fields, 12, "EpochActivated.auth_rev")?,
            activation_t: fields
                .remove(&13)
                .map(|value| expect_bytes(value, "EpochActivated.activation_t"))
                .transpose()?,
            old_t_signature: fields
                .remove(&14)
                .map(|value| expect_bytes(value, "EpochActivated.old_t_signature"))
                .transpose()?,
            new_t_signature: fields
                .remove(&15)
                .map(|value| expect_bytes(value, "EpochActivated.new_t_signature"))
                .transpose()?,
        };
        record.check_activation_t_attachment()?;
        Ok(record)
    }
}

/// `rotation.restarted`（relay `RotationRestarted`，键 0..5）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RotationRestarted {
    pub old_rotation: RotationId,
    pub new_rotation: RotationId,
    pub from_epoch: u64,
    pub old_to_epoch: u64,
    pub new_to_epoch: u64,
    pub freeze_seq: u64,
}

impl RotationRestarted {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&record_map(vec![
            (0, CdeValue::Bytes(self.old_rotation.to_vec())),
            (1, CdeValue::Bytes(self.new_rotation.to_vec())),
            (2, CdeValue::UInt(self.from_epoch)),
            (3, CdeValue::UInt(self.old_to_epoch)),
            (4, CdeValue::UInt(self.new_to_epoch)),
            (5, CdeValue::UInt(self.freeze_seq)),
        ]))
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        let fields = into_record_fields(decode(bytes)?, 6)?;
        let mut fields = fields
            .into_iter()
            .enumerate()
            .map(|(index, value)| (index as u64, value))
            .collect::<BTreeMap<_, _>>();
        Ok(Self {
            old_rotation: take_id16(&mut fields, 0, "RotationRestarted.old_rotation")?,
            new_rotation: take_id16(&mut fields, 1, "RotationRestarted.new_rotation")?,
            from_epoch: take_uint(&mut fields, 2, "RotationRestarted.from_epoch")?,
            old_to_epoch: take_uint(&mut fields, 3, "RotationRestarted.old_to_epoch")?,
            new_to_epoch: take_uint(&mut fields, 4, "RotationRestarted.new_to_epoch")?,
            freeze_seq: take_uint(&mut fields, 5, "RotationRestarted.freeze_seq")?,
        })
    }
}

/// `rotation.revoked-more`（relay `RevocationsAdded`，键 0..3，3 可省略）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevocationsAdded {
    pub rotation_id: RotationId,
    pub devices: Vec<DeviceId>,
    pub restarted: bool,
    pub new_rotation: Option<RotationId>,
}

impl RevocationsAdded {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        let mut fields = vec![
            (0, CdeValue::Bytes(self.rotation_id.to_vec())),
            (
                1,
                CdeValue::Array(
                    self.devices
                        .iter()
                        .map(|id| CdeValue::Bytes(id.to_vec()))
                        .collect(),
                ),
            ),
            (2, CdeValue::Bool(self.restarted)),
        ];
        if let Some(id) = self.new_rotation {
            fields.push((3, CdeValue::Bytes(id.to_vec())));
        }
        encode(&record_map(fields))
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        let mut fields = optional_fields(decode(bytes)?, &[0, 1, 2, 3], &[0, 1, 2])?;
        let rotation_id = take_id16(&mut fields, 0, "RevocationsAdded.rotation_id")?;
        let devices = expect_array(take(&mut fields, 1)?, "RevocationsAdded.devices")?
            .into_iter()
            .map(|value| expect_bytes_n::<16>(value, "RevocationsAdded.devices[]"))
            .collect::<Result<Vec<_>, _>>()?;
        let restarted = expect_bool(take(&mut fields, 2)?, "RevocationsAdded.restarted")?;
        let new_rotation = fields
            .remove(&3)
            .map(|value| expect_bytes_n::<16>(value, "RevocationsAdded.new_rotation"))
            .transpose()?;
        Ok(Self {
            rotation_id,
            devices,
            restarted,
            new_rotation,
        })
    }
}

/// `ActivationCommitV1`（relay `rotations::ActivationCommit`，键 0..15，14 可省略）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivationCommitV1 {
    pub vault_id: [u8; 16],
    pub instance_id: [u8; 16],
    pub rotation_id: RotationId,
    pub from_epoch: u64,
    pub to_epoch: u64,
    pub covered_seq: u64,
    pub primary_checkpoint: CheckpointId,
    pub fallback_checkpoint: CheckpointId,
    pub primary_covered_seq: u64,
    pub fallback_covered_seq: u64,
    pub primary_manifest: Sha256,
    pub fallback_manifest: Sha256,
    pub member_set_hash: Sha256,
    pub grants_digest: Sha256,
    pub asset_set_digest: Option<Sha256>,
    pub control_rev: u64,
}

impl ActivationCommitV1 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        let mut fields = vec![
            (0, CdeValue::Bytes(self.vault_id.to_vec())),
            (1, CdeValue::Bytes(self.instance_id.to_vec())),
            (2, CdeValue::Bytes(self.rotation_id.to_vec())),
            (3, CdeValue::UInt(self.from_epoch)),
            (4, CdeValue::UInt(self.to_epoch)),
            (5, CdeValue::UInt(self.covered_seq)),
            (6, CdeValue::Bytes(self.primary_checkpoint.to_vec())),
            (7, CdeValue::Bytes(self.fallback_checkpoint.to_vec())),
            (8, CdeValue::UInt(self.primary_covered_seq)),
            (9, CdeValue::UInt(self.fallback_covered_seq)),
            (10, CdeValue::Bytes(self.primary_manifest.to_vec())),
            (11, CdeValue::Bytes(self.fallback_manifest.to_vec())),
            (12, CdeValue::Bytes(self.member_set_hash.to_vec())),
            (13, CdeValue::Bytes(self.grants_digest.to_vec())),
            (15, CdeValue::UInt(self.control_rev)),
        ];
        if let Some(digest) = self.asset_set_digest {
            fields.push((14, CdeValue::Bytes(digest.to_vec())));
        }
        encode(&record_map(fields))
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        let mut fields = optional_fields(
            decode(bytes)?,
            &(0..=15).collect::<Vec<_>>(),
            &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 15],
        )?;
        Ok(Self {
            vault_id: take_id16(&mut fields, 0, "ActivationCommitV1.vault_id")?,
            instance_id: take_id16(&mut fields, 1, "ActivationCommitV1.instance_id")?,
            rotation_id: take_id16(&mut fields, 2, "ActivationCommitV1.rotation_id")?,
            from_epoch: take_uint(&mut fields, 3, "ActivationCommitV1.from_epoch")?,
            to_epoch: take_uint(&mut fields, 4, "ActivationCommitV1.to_epoch")?,
            covered_seq: take_uint(&mut fields, 5, "ActivationCommitV1.covered_seq")?,
            primary_checkpoint: take_id16(&mut fields, 6, "ActivationCommitV1.primary_checkpoint")?,
            fallback_checkpoint: take_id16(
                &mut fields,
                7,
                "ActivationCommitV1.fallback_checkpoint",
            )?,
            primary_covered_seq: take_uint(
                &mut fields,
                8,
                "ActivationCommitV1.primary_covered_seq",
            )?,
            fallback_covered_seq: take_uint(
                &mut fields,
                9,
                "ActivationCommitV1.fallback_covered_seq",
            )?,
            primary_manifest: take_hash32(&mut fields, 10, "ActivationCommitV1.primary_manifest")?,
            fallback_manifest: take_hash32(
                &mut fields,
                11,
                "ActivationCommitV1.fallback_manifest",
            )?,
            member_set_hash: take_hash32(&mut fields, 12, "ActivationCommitV1.member_set_hash")?,
            grants_digest: take_hash32(&mut fields, 13, "ActivationCommitV1.grants_digest")?,
            asset_set_digest: fields
                .remove(&14)
                .map(|value| expect_bytes_n::<32>(value, "ActivationCommitV1.asset_set_digest"))
                .transpose()?,
            control_rev: take_uint(&mut fields, 15, "ActivationCommitV1.control_rev")?,
        })
    }
}

/// `POST /rotations` 的 canonical body（键 0..3）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RotationBeginBodyV1 {
    pub expected_epoch: u64,
    pub expected_control_rev: u64,
    pub request_id: DeviceId,
    pub revoke: Vec<DeviceId>,
}

impl RotationBeginBodyV1 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&record_map(vec![
            (0, CdeValue::UInt(self.expected_epoch)),
            (1, CdeValue::UInt(self.expected_control_rev)),
            (2, CdeValue::Bytes(self.request_id.to_vec())),
            (
                3,
                CdeValue::Array(
                    self.revoke
                        .iter()
                        .map(|id| CdeValue::Bytes(id.to_vec()))
                        .collect(),
                ),
            ),
        ]))
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        let fields = into_record_fields(decode(bytes)?, 4)?;
        let mut fields = fields
            .into_iter()
            .enumerate()
            .map(|(index, value)| (index as u64, value))
            .collect::<BTreeMap<_, _>>();
        Ok(Self {
            expected_epoch: take_uint(&mut fields, 0, "RotationBeginBodyV1.expected_epoch")?,
            expected_control_rev: take_uint(
                &mut fields,
                1,
                "RotationBeginBodyV1.expected_control_rev",
            )?,
            request_id: take_id16(&mut fields, 2, "RotationBeginBodyV1.request_id")?,
            revoke: expect_array(take(&mut fields, 3)?, "RotationBeginBodyV1.revoke")?
                .into_iter()
                .map(|value| expect_bytes_n::<16>(value, "RotationBeginBodyV1.revoke[]"))
                .collect::<Result<Vec<_>, _>>()?,
        })
    }
}

/// activate body 里的管理权转换包装
/// （键 0 transition / 1 old_sig? / 2 new_sig? / 3 activation_t? / 4 old_t_sig? / 5 new_t_sig?）。
///
/// ADR 0006 §34：完整 activation T（canonical CDE）与旧/新管理 key 对该 T 的双签随
/// `new_manager` 一起传输。3–5 在 wire 上**必须同时存在或全缺**（全缺只用于渐进接入
/// 的客户端与历史 body；relay 执行 activate 时三者必填，缺任一 → 409）。1/2 保留既有
/// 可选语义（管理权转换双签，relay 自行校验）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewManagerV1 {
    pub transition: Vec<u8>,
    pub old_signature: Option<Vec<u8>>,
    pub new_signature: Option<Vec<u8>>,
    /// 3：complete activation T 的 canonical CDE 原字节。
    pub activation_t: Option<Vec<u8>>,
    /// 4：旧管理 key 对 `activation_t_signing_input(T)` 的 64B 签名。
    pub old_t_signature: Option<Vec<u8>>,
    /// 5：新管理 key 对同一输入的 64B 签名。
    pub new_t_signature: Option<Vec<u8>>,
}

impl NewManagerV1 {
    /// 3–5 同时存在或全缺；存在时两份 T 签名各 64B、T 必须是可解码的 `ActivationTV1`。
    fn validate(&self) -> Result<(), ProtoError> {
        let present = [
            self.activation_t.is_some(),
            self.old_t_signature.is_some(),
            self.new_t_signature.is_some(),
        ];
        if present.iter().all(|value| !value) {
            return Ok(());
        }
        if !present.iter().all(|value| *value) {
            return Err(ProtoError::Invalid(
                "NewManagerV1 的 activation_t 与旧/新 T 签名必须同时存在".to_owned(),
            ));
        }
        let activation_t = self
            .activation_t
            .as_deref()
            .ok_or_else(|| ProtoError::Invalid("NewManagerV1.activation_t".to_owned()))?;
        ActivationTV1::decode(activation_t)?;
        for (name, signature) in [
            ("NewManagerV1.old_t_signature", &self.old_t_signature),
            ("NewManagerV1.new_t_signature", &self.new_t_signature),
        ] {
            let signature = signature
                .as_deref()
                .ok_or_else(|| ProtoError::Invalid(name.to_owned()))?;
            if signature.len() != 64 {
                return Err(ProtoError::Invalid(format!(
                    "{name} 必须是 64 字节，实际 {}",
                    signature.len()
                )));
            }
        }
        Ok(())
    }

    fn to_cde(&self) -> Result<CdeValue, ProtoError> {
        self.validate()?;
        let mut fields = vec![(0, CdeValue::Bytes(self.transition.clone()))];
        for (key, signature) in [
            (1u64, &self.old_signature),
            (2, &self.new_signature),
            (3, &self.activation_t),
            (4, &self.old_t_signature),
            (5, &self.new_t_signature),
        ] {
            if let Some(value) = signature {
                fields.push((key, CdeValue::Bytes(value.clone())));
            }
        }
        Ok(record_map(fields))
    }

    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&self.to_cde()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        Self::from_fields(optional_fields(decode(bytes)?, &[0, 1, 2, 3, 4, 5], &[0])?)
    }

    fn from_fields(mut fields: BTreeMap<u64, CdeValue>) -> Result<Self, ProtoError> {
        let record = Self {
            transition: expect_bytes(take(&mut fields, 0)?, "NewManagerV1.transition")?,
            old_signature: fields
                .remove(&1)
                .map(|value| expect_bytes(value, "NewManagerV1.old_signature"))
                .transpose()?,
            new_signature: fields
                .remove(&2)
                .map(|value| expect_bytes(value, "NewManagerV1.new_signature"))
                .transpose()?,
            activation_t: fields
                .remove(&3)
                .map(|value| expect_bytes(value, "NewManagerV1.activation_t"))
                .transpose()?,
            old_t_signature: fields
                .remove(&4)
                .map(|value| expect_bytes(value, "NewManagerV1.old_t_signature"))
                .transpose()?,
            new_t_signature: fields
                .remove(&5)
                .map(|value| expect_bytes(value, "NewManagerV1.new_t_signature"))
                .transpose()?,
        };
        record.validate()?;
        Ok(record)
    }
}

/// `POST /rotations/{r}/activate` 的 canonical body（键 0..6，均可省略）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RotationActivateBodyV1 {
    pub new_manager: Option<NewManagerV1>,
    pub primary_checkpoint: Option<CheckpointId>,
    pub fallback_checkpoint: Option<CheckpointId>,
    pub member_cert_digest: Option<Sha256>,
    pub grants_digest: Option<Sha256>,
    pub slot_set_digest: Option<Sha256>,
    pub asset_set_digest: Option<Sha256>,
}

impl RotationActivateBodyV1 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        let mut fields = Vec::new();
        if let Some(manager) = &self.new_manager {
            fields.push((0, manager.to_cde()?));
        }
        if let Some(id) = self.primary_checkpoint {
            fields.push((1, CdeValue::Bytes(id.to_vec())));
        }
        if let Some(id) = self.fallback_checkpoint {
            fields.push((2, CdeValue::Bytes(id.to_vec())));
        }
        if let Some(digest) = self.member_cert_digest {
            fields.push((3, CdeValue::Bytes(digest.to_vec())));
        }
        if let Some(digest) = self.grants_digest {
            fields.push((4, CdeValue::Bytes(digest.to_vec())));
        }
        if let Some(digest) = self.slot_set_digest {
            fields.push((5, CdeValue::Bytes(digest.to_vec())));
        }
        if let Some(digest) = self.asset_set_digest {
            fields.push((6, CdeValue::Bytes(digest.to_vec())));
        }
        encode(&record_map(fields))
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        let mut fields = optional_fields(decode(bytes)?, &[0, 1, 2, 3, 4, 5, 6], &[])?;
        let new_manager = fields
            .remove(&0)
            .map(|value| {
                NewManagerV1::from_fields(optional_fields(
                    value,
                    &[0, 1, 2, 3, 4, 5],
                    &[0, 3, 4, 5],
                )?)
            })
            .transpose()?;
        Ok(Self {
            new_manager,
            primary_checkpoint: fields
                .remove(&1)
                .map(|value| expect_bytes_n::<16>(value, "RotationActivateBodyV1.primary"))
                .transpose()?,
            fallback_checkpoint: fields
                .remove(&2)
                .map(|value| expect_bytes_n::<16>(value, "RotationActivateBodyV1.fallback"))
                .transpose()?,
            member_cert_digest: fields
                .remove(&3)
                .map(|value| expect_bytes_n::<32>(value, "RotationActivateBodyV1.member_digest"))
                .transpose()?,
            grants_digest: fields
                .remove(&4)
                .map(|value| expect_bytes_n::<32>(value, "RotationActivateBodyV1.grants_digest"))
                .transpose()?,
            slot_set_digest: fields
                .remove(&5)
                .map(|value| expect_bytes_n::<32>(value, "RotationActivateBodyV1.slot_digest"))
                .transpose()?,
            asset_set_digest: fields
                .remove(&6)
                .map(|value| expect_bytes_n::<32>(value, "RotationActivateBodyV1.asset_digest"))
                .transpose()?,
        })
    }
}

/// `POST /rotations/{r}/grants/{device}` 的 canonical body（键 0..5）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantStageBodyV1 {
    pub grant_id: DeviceId,
    pub hpke_enc: [u8; 32],
    pub hpke_ciphertext: Vec<u8>,
    pub signed_header: Vec<u8>,
    pub signature: Vec<u8>,
    pub manager_rev: u64,
}

impl GrantStageBodyV1 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&record_map(vec![
            (0, CdeValue::Bytes(self.grant_id.to_vec())),
            (1, CdeValue::Bytes(self.hpke_enc.to_vec())),
            (2, CdeValue::Bytes(self.hpke_ciphertext.clone())),
            (3, CdeValue::Bytes(self.signed_header.clone())),
            (4, CdeValue::Bytes(self.signature.clone())),
            (5, CdeValue::UInt(self.manager_rev)),
        ]))
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        let fields = into_record_fields(decode(bytes)?, 6)?;
        let mut fields = fields
            .into_iter()
            .enumerate()
            .map(|(index, value)| (index as u64, value))
            .collect::<BTreeMap<_, _>>();
        Ok(Self {
            grant_id: take_id16(&mut fields, 0, "GrantStageBodyV1.grant_id")?,
            hpke_enc: take_hash32(&mut fields, 1, "GrantStageBodyV1.hpke_enc")?,
            hpke_ciphertext: expect_bytes(
                take(&mut fields, 2)?,
                "GrantStageBodyV1.hpke_ciphertext",
            )?,
            signed_header: expect_bytes(take(&mut fields, 3)?, "GrantStageBodyV1.signed_header")?,
            signature: expect_bytes(take(&mut fields, 4)?, "GrantStageBodyV1.signature")?,
            manager_rev: take_uint(&mut fields, 5, "GrantStageBodyV1.manager_rev")?,
        })
    }
}

/// `POST /rotations/{r}/abort` 的 canonical body：空 map。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RotationAbortBodyV1;

impl RotationAbortBodyV1 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&record_map(Vec::new()))
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        let fields = optional_fields(decode(bytes)?, &[], &[])?;
        debug_assert!(fields.is_empty());
        Ok(Self)
    }
}

/// `POST /key-slots/password/activate` 的 canonical body（键 0/1）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotActivateBodyV1 {
    pub expected_slot_set_rev: u64,
    pub new_slot_id: Sha256,
}

impl SlotActivateBodyV1 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&record_map(vec![
            (0, CdeValue::UInt(self.expected_slot_set_rev)),
            (1, CdeValue::Bytes(self.new_slot_id.to_vec())),
        ]))
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        let fields = into_record_fields(decode(bytes)?, 2)?;
        let mut fields = fields
            .into_iter()
            .enumerate()
            .map(|(index, value)| (index as u64, value))
            .collect::<BTreeMap<_, _>>();
        Ok(Self {
            expected_slot_set_rev: take_uint(
                &mut fields,
                0,
                "SlotActivateBodyV1.expected_slot_set_rev",
            )?,
            new_slot_id: take_hash32(&mut fields, 1, "SlotActivateBodyV1.new_slot_id")?,
        })
    }
}

/// `POST /invites` 的 canonical body（键 0 token_hash / 1 expires_at?）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InviteCreateBodyV1 {
    pub token_hash: Sha256,
    pub expires_at: Option<u64>,
}

impl InviteCreateBodyV1 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        let mut fields = vec![(0, CdeValue::Bytes(self.token_hash.to_vec()))];
        if let Some(expires_at) = self.expires_at {
            fields.push((1, CdeValue::UInt(expires_at)));
        }
        encode(&record_map(fields))
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        let mut fields = optional_fields(decode(bytes)?, &[0, 1], &[0])?;
        Ok(Self {
            token_hash: take_hash32(&mut fields, 0, "InviteCreateBodyV1.token_hash")?,
            expires_at: fields
                .remove(&1)
                .map(|value| expect_i64_max(value, "InviteCreateBodyV1.expires_at"))
                .transpose()?,
        })
    }
}

/// `POST /devices/{device}/approve` 的 canonical body（键 0..7）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceApproveBodyV1 {
    pub certificate: Vec<u8>,
    pub certificate_signature: Vec<u8>,
    pub grant_id: DeviceId,
    pub hpke_enc: [u8; 32],
    pub hpke_ciphertext: Vec<u8>,
    pub grant_signature: Vec<u8>,
    pub request_id: DeviceId,
    pub expected_control_rev: u64,
}

impl DeviceApproveBodyV1 {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        encode(&record_map(vec![
            (0, CdeValue::Bytes(self.certificate.clone())),
            (1, CdeValue::Bytes(self.certificate_signature.clone())),
            (2, CdeValue::Bytes(self.grant_id.to_vec())),
            (3, CdeValue::Bytes(self.hpke_enc.to_vec())),
            (4, CdeValue::Bytes(self.hpke_ciphertext.clone())),
            (5, CdeValue::Bytes(self.grant_signature.clone())),
            (6, CdeValue::Bytes(self.request_id.to_vec())),
            (7, CdeValue::UInt(self.expected_control_rev)),
        ]))
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        let fields = into_record_fields(decode(bytes)?, 8)?;
        let mut fields = fields
            .into_iter()
            .enumerate()
            .map(|(index, value)| (index as u64, value))
            .collect::<BTreeMap<_, _>>();
        Ok(Self {
            certificate: expect_bytes(take(&mut fields, 0)?, "DeviceApproveBodyV1.certificate")?,
            certificate_signature: expect_bytes(
                take(&mut fields, 1)?,
                "DeviceApproveBodyV1.certificate_signature",
            )?,
            grant_id: take_id16(&mut fields, 2, "DeviceApproveBodyV1.grant_id")?,
            hpke_enc: take_hash32(&mut fields, 3, "DeviceApproveBodyV1.hpke_enc")?,
            hpke_ciphertext: expect_bytes(
                take(&mut fields, 4)?,
                "DeviceApproveBodyV1.hpke_ciphertext",
            )?,
            grant_signature: expect_bytes(
                take(&mut fields, 5)?,
                "DeviceApproveBodyV1.grant_signature",
            )?,
            request_id: take_id16(&mut fields, 6, "DeviceApproveBodyV1.request_id")?,
            expected_control_rev: take_uint(
                &mut fields,
                7,
                "DeviceApproveBodyV1.expected_control_rev",
            )?,
        })
    }
}

/// `member_cert_digest` 的字节输入项（relay 按 device_id 升序拼接）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberCertDigestInputV1 {
    pub device_id: DeviceId,
    pub certificate: Vec<u8>,
    pub certificate_signature: Option<Vec<u8>>,
}

/// 成员证书 digest 的**字节输入**（不做 SHA-256）；按 `device_id` 升序。
#[must_use]
pub fn member_cert_digest(entries: &[MemberCertDigestInputV1]) -> Vec<u8> {
    let mut ordered = entries.iter().collect::<Vec<_>>();
    ordered.sort_by_key(|entry| entry.device_id);
    let mut out = Vec::new();
    for entry in ordered {
        out.extend_from_slice(&entry.device_id);
        out.extend_from_slice(&entry.certificate);
        if let Some(signature) = &entry.certificate_signature {
            out.extend_from_slice(signature);
        }
    }
    out
}

/// `slot_set_digest` 的字节输入项；`descriptor_sha256` 由调用方先算。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotSetDigestInputV1 {
    pub slot_id: Sha256,
    pub descriptor_sha256: Sha256,
}

/// 槽集合 digest 的**字节输入**（不做 SHA-256）；按 `slot_id` 升序。
#[must_use]
pub fn slot_set_digest(entries: &[SlotSetDigestInputV1]) -> Vec<u8> {
    let mut ordered = entries.iter().collect::<Vec<_>>();
    ordered.sort_by_key(|entry| entry.slot_id);
    let mut out = Vec::new();
    for entry in ordered {
        out.extend_from_slice(&entry.slot_id);
        out.extend_from_slice(&entry.descriptor_sha256);
    }
    out
}

/// grant 包 digest 的**字节输入**（不做 SHA-256）：
/// `hpke_enc || ciphertext_sha256 || signed_header_sha256 || signature`。
#[must_use]
pub fn grant_package_digest(
    hpke_enc: &Sha256,
    ciphertext_sha256: &Sha256,
    signed_header_sha256: &Sha256,
    signature: &[u8],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(32 + 32 + 32 + signature.len());
    out.extend_from_slice(hpke_enc);
    out.extend_from_slice(ciphertext_sha256);
    out.extend_from_slice(signed_header_sha256);
    out.extend_from_slice(signature);
    out
}

/// 管理 canonical body 的简名别名（begin/activate/grants/abort/slots-activate/invites/approve）。
pub type BeginBodyV1 = RotationBeginBodyV1;
pub type ActivateBodyV1 = RotationActivateBodyV1;
pub type GrantsBodyV1 = GrantStageBodyV1;
pub type AbortBodyV1 = RotationAbortBodyV1;
pub type SlotsActivateBodyV1 = SlotActivateBodyV1;
pub type InvitesBodyV1 = InviteCreateBodyV1;
pub type ApproveBodyV1 = DeviceApproveBodyV1;

`````


