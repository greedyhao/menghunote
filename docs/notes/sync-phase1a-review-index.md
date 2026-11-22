# 同步 Phase 1A 评审索引（给高级模型）

> **2026-11-22 更新**：Phase 1B MVP 已实现（bootstrap prepare/integrate、CatchUpMode、
> catchup、prune、两阶段/sync_once 接入）；本文以下“Phase 1B 未实现”是当时评审上下文，
> 现状见 [`sync-fast-catchup-plan.md`](sync-fast-catchup-plan.md) §11。

> 当前评审对象：**Phase 0 度量 + Phase 1A（批量 inbox 原子化、按文档聚合 integrate、baseline 优化、Release 标定）**。
> 完整评审包（架构/代码地图/问题清单）见
> [`sync-review-packet-20261121.md`](sync-review-packet-20261121.md)。
> 只上传一份给高级模型时，用自包含的
> [`sync-critical-modules-review.md`](sync-critical-modules-review.md)。
> **Phase 1B（checkpoint 快速追赶）尚未写代码**；计划里已有约束，不能按“已实现”评审。
> 本轮没有改 relay / `sync-proto` / schema / wire；不变量仍是 ADR 0006 的游标与 checkpoint 契约。

## 1. 先读（按顺序）

0. **完整评审包** [`sync-review-packet-20261121.md`](sync-review-packet-20261121.md)（建议先读）。
1. **计划与 Phase 1B 约束**
   [`sync-fast-catchup-plan.md`](sync-fast-catchup-plan.md)
   - §4 Phase 0；§8 上一轮评审的 6 点修正；
   - §9 本轮新增：双模式触发、强制/机会性追赶、写闸拆分、`prune_covered_inbox`、
     `N_min` 的 Stage 1A 重标定。
2. **实测报告**
   [`sync-phase0-metrics.md`](sync-phase0-metrics.md)
   - §1–§7 是 Phase 0 dev 数据（历史对照，**不要用其绝对值**）；
   - **§8 是 Phase 1A 的 `--release` 数据**：storage 批量、600/2000/5000 追赶、
     20 篇多文档、`baseline_apply` 5/20 MiB 探针。
3. **契约**
   - [`docs/adr/0006-sync-v1.md`](../adr/0006-sync-v1.md)：checkpoint/manifest、`/bootstrap`、
     `integrated >= S`、pending/回滚检测、两 checkpoint GC。
   - [`docs/adr/0012-autosync-two-phase-gate.md`](../adr/0012-autosync-two-phase-gate.md)：
     网络不持闸 / integrate 持闸的现有骨架。
   - [`docs/adr/0026-relay-deployment-and-client-releases.md`](../adr/0026-relay-deployment-and-client-releases.md)：
     发布纪律；本轮不需要服务端更新。

## 2. 代码文件（建议给高级模型的顺序）

高级模型有 1M 上下文，可以一次给“2 份文档 + 下面 5 个核心文件”，先审正确性；需要再审
测试/Phase 1B 上下文时再补后半组。

### 第一批：Phase 1A 正确性（必看）

| 文件 | 重点 |
| --- | --- |
| `crates/storage/src/sync_meta.rs` | `meta` 键、`ConsumedPrefix`、received/integrated 连续前缀扫描；skipped 窗口 `ORDER BY key LIMIT 4096` 与跨窗口推进 |
| `crates/storage/src/actor.rs` | `Command::ExecInboxBatch` / `AdvanceConsumedPrefix`；worker 独占连接上的 **BEGIN IMMEDIATE** 单事务、幂等/冲突/前缀 |
| `crates/storage/src/sync_bridge.rs` | `store_inbox_verified_batch` 入口、`mark_integrated_batch`、公开 `advance_integrated_prefix`、单条路径兼容、remote_marks 透传 |
| `crates/storage/src/store.rs` | `append_updates_with_integrated_marks`：一条组合 update + 多条 inbox 标记同一 A 事务；remote-known VV 同事务写入 |
| `crates/storage/src/remote_known.rs` | 每篇文档远端已知 VV 的非破坏性 meta（tombstone 判定用） |
| `crates/sync/src/engine.rs` | pull 早退 flush；`load_head` 预检、按 doc 多轮合并（乱序自愈）、选批防饥饿、组合落盘、`doc_loads` |

### 第二批：测试与基准（审证据）

| 文件 | 重点 |
| --- | --- |
| `crates/sync/tests/engine.rs` | 单文档 256 行 `doc_loads=1`、deferred 截断、已覆盖+新行、重跑幂等、pull checkpoint 早退 |
| `crates/storage/tests/sync_inbox_batch.rs` | 全幂等/半批/篡改整批回滚、跨 ASSET 前缀、`mark_integrated_batch` 回滚 |
| `crates/sync/tests/phase0_catchup.rs` | per_row / batch / aggregate 三模式；600/2000 单文档、5000/20 篇 |
| `crates/sync/tests/phase1a_release.rs` | `baseline_apply` 5/20 MiB 探针；打印 restore/head/local delta/detached import/pending/事务/verify/gate |
| `crates/storage/tests/sync_inbox_bench.rs` | 逐条 vs 批量 inbox（release 数字） |

### 第三批：Phase 1B 上下文（计划评审用，非本轮实现）

| 文件 | 用途 |
| --- | --- |
| `crates/storage/src/baseline_apply.rs` | begin/apply/finish、CAS；已改为单次 snapshot 解码 + detached oplog 合并 + remote-known tombstone 判定 |
| `crates/storage/src/body_doc.rs` | `import_oplog_only` / `pending_update_from_oplog`（detached 合并，baseline 专用） |
| `crates/sync/src/bootstrap.rs` | 现有 `/bootstrap` + snapshot/asset 下载；要拆 prepare/integrate |
| `crates/sync/src/dev_local.rs` | 现有两阶段 `sync_prepare` / `sync_integrate`；写闸骨架 |
| `crates/sync/src/rotation.rs` | epoch/rotation 边界，强制追赶必须安全处理 |
| `crates/app/src/dev_sync/service.rs` | worker 摘要/指标展示；非正确性关键 |

## 3. Phase 1A 关键结果（release）

- storage inbox（4 KiB/行）：256 行 **282.4 → 5.34 ms（52.9×）**，
  1024 行 **819.4 → 25.16 ms（32.6×）**。
- 单文档 600：per_row 3513 ms；batch 5307 ms（fsync 抖动）；aggregate **83 ms**。
- 单文档 2000：per_row 31 144 ms；batch 25 903 ms；aggregate **244 ms**；
  `doc_loads` 2000 → 8。
- 5000/20 篇（每篇 ≈107 KiB）：per_row 19 039 ms；batch 15 157 ms；
  aggregate **1 169 ms**；`doc_loads` 5000 → 400。
- 5000 单文档：per_row 150 790 ms；aggregate **676 ms**；`doc_loads` 5000 → 20。
- `baseline_apply`（20 篇、storage merge、非真实下载）旧实现：5 MiB ≈2.72–4.14 s；
  20 MiB ≈44.6–61.3 s；5→20 超线性 14.8–16.9×。
- **优化后**（detached oplog merge，2026-11-21）：5 MiB **35 ms**、20 MiB **110 ms**，
  5→20 **3.1×**；根因是 Loro attached import 的 diff/state，不是 decode/pending。
  闸外 verify 仍 2.7 / 45.1 s（16.7×），不能把 gate 数字外推到首次打开/B 物化。
- **结论**：聚合消除 `O(历史 × 事件)`；批量 inbox 仍必要；`N_min=25 000` +
  16 MiB 机会性硬上限 + 3 s 持闸 + 2.0×/1.5× 成本交叉；强制追赶不受体积上限，
  失败 Paused + 退避，绝不回退 pull。最终阈值待用户确认。

## 4. 请高级模型重点判断

1. **单事务语义**：`ExecInboxBatch` 在 worker 独占连接上做“读既有 + 冲突判定 + 插入 +
   received 前缀”，是否真正消除了先读后写竞态？幂等行不触发 guard、冲突整批回滚是否
   覆盖所有半写路径？外部多进程写同一 DB 的 `SQLITE_BUSY` 是否需要额外处理？
2. **聚合落盘语义**：同 doc 多条 remote update 合成一条本地 update + 多条 inbox 标记，
   同一个 A 事务；这种“本地 seq 粒度变化”是否影响 ACK、物化、undo、审计或任何
   “remote update 1:1 本地 seq”的既有假设？`HeadConflict` 有界重试是否够？
3. **deferred 截断**：遇到 base 未覆盖即停止该 doc，之后行保持 verified；下一轮从同一行
   继续。这个“按 relay_seq 前缀”假设在 relay 物理乱序时是否可靠？`aggregate_body=false`
   兜底是否足够？
4. **前缀与覆盖证明**：批量标记不推进前缀、整批后统一推进；`prune_covered_inbox` 只能
   在 finalize 且 `integrated >= S` 后清。有没有“行已 integrated、前缀未推进、checkpoint
   coverage 误判”的窗口？
5. **阈值**：`N_min=25 000` + 16 MiB 机会性硬上限 + 3 s 持闸 + 2.0×/1.5× 成本交叉
   是否成立？强制/机会性双模式、`S - received_seq`、真实 snapshot 形状、闸外 verify
   仍 16× 的残余风险怎么处理？
6. **Phase 1B 写闸**：`catchup_prepare`（不持闸）→ `catchup_integrate`（持闸）能否复用
   ADR 0012 骨架？`bootstrap::run` 单体拆分、fallback 用实际 S、epoch/rotation 跨越暂停，
   还缺哪些测试/证据？
7. **证据缺口**：真实 relay/TLS、真实 snapshot 形状、下载/解密/验签、`gate_hold_ms`、
   附件、Windows/DPAPI、多进程并发；哪些必须在 Phase 1B 前补？

## 5. 已知边界（不要当遗漏）

- Phase 1B **未实现**：无 `CatchUpMode`、无 prepare/integrate 拆分、无真实 snapshot merge 实测。
- `baseline_apply` 探针只测 storage merge；`S`/cursor 是自造合法值，没有 checkpoint 签名/网络。
- 常规同步面板行未显示字节/耗时；指标在报告/`[sync]` 日志/worker 摘要。
- `store_inbox_verified` 单条路径仍是“读→插→前缀”分离事务（既有路径，未改）；批量路径已原子化。
- 聚合依赖同一 doc 内 relay_seq 与因果序一致；物理乱序历史只能走 `aggregate_body=false`。
- `N_min=25 000` / 16 MiB 是计划 §9.1 的默认值，仍待用户确认；128 MiB 只允许强制追赶且受本地磁盘预算保护，不能走机会性路径。

## 6. 评审产出建议

- **结论**：Phase 1A 是否可接受；Phase 1B 是否按“先 20k/成本交叉 + 双模式”落地。
- **必须修正**：按严重度 + 文件/行号。
- **阈值**：确认 `N_min=25 000`、16 MiB 机会性上限、3 s 持闸；128 MiB 只用于强制追赶。
- **新增风险**：并发、崩溃、epoch/rotation、coverage 证明。
- **不建议做**：明确后置项。
