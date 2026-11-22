# 同步 Phase 0 评审索引（历史快照）

> **2026-11-22 更新**：Phase 1B MVP 已实现（bootstrap prepare/integrate、CatchUpMode、
> catchup、prune、两阶段/sync_once 接入）；本文以下“Phase 1B 未实现”是当时评审上下文，
> 现状见 [`sync-fast-catchup-plan.md`](sync-fast-catchup-plan.md) §11。

> **已过期，仅作历史对照。** 最新评审材料见
> [`sync-review-packet-20261121.md`](sync-review-packet-20261121.md) 与
> [`sync-phase1a-review-index.md`](sync-phase1a-review-index.md)。
>
> 本文的 Phase 1 未实现、500 阈值、128 MiB 上限等描述均为 Phase 0 当时状态；
> 当前 Phase 1A 已落地、Phase 1B 未实现，阈值以计划 §9.1 为准。
>
> 历史基线：`cd10b29`（Phase 0 代码在 `98cbc83`，之后只有代码块高亮）。
> Phase 0 **未改** relay / `sync-proto` / schema / `bootstrap` / 游标语义。

## 1. 先读这两份

1. **计划与评审修正**
   [`sync-fast-catchup-plan.md`](sync-fast-catchup-plan.md)
   - §4 Phase 0：要量什么（§4，L87）；
   - §8 上一轮评审后的 6 点修正（L157）：触发窗口、Asset 只补缺失、写闸拆分、
     inbox 清理与 coverage 证明的冲突、CursorExpired 分类、批处理对照。
2. **Phase 0 实测报告**
   [`sync-phase0-metrics.md`](sync-phase0-metrics.md)
   - §1 结论；§3 指标口径/批量 API；§4 storage 基准；§5 两设备追赶基准；
     §6 建议阈值与优先级；§7 未测/限制。

## 2. 需要对照的契约

- [`docs/adr/0006-sync-v1.md`](../adr/0006-sync-v1.md)：checkpoint/manifest 字段（L80 起）、
  `/bootstrap` 返回（L186 起）、`integrated >= S` / pending / 回滚检测（L212 起）、
  两 checkpoint GC 与水位。评审“快照追赶是否仍值得、S 是否能安全使用”必须对照它。
- [`docs/adr/0012-autosync-two-phase-gate.md`](../adr/0012-autosync-two-phase-gate.md)：
  两阶段“网络不持闸 / integrate 持闸”的现有骨架；计划里的写闸拆分应尽量复用。
- [`docs/adr/0026-relay-deployment-and-client-releases.md`](../adr/0026-relay-deployment-and-client-releases.md)：
  relay 已部署且 release 纪律是“只有 relay 程序变更才需要服务端更新”；Phase 0/1
  不应悄悄改 wire。

## 3. 关键代码/证据（如果需要看实现）

| 内容 | 位置 |
| --- | --- |
| `PullReport` 字节/耗时/分页/批量开关 | `crates/sync/src/engine.rs`（L221 起） |
| `IntegrateReport.elapsed_ms` | `crates/sync/src/engine.rs`（L309 起） |
| pull 批量攒行与逐条/批量分流 | `crates/sync/src/engine.rs`（L1310 起） |
| `ingest_update` 拆出 `verify_update_event` | `crates/sync/src/engine.rs`（L1693 起附近） |
| `store_inbox_verified_batch` | `crates/storage/src/sync_bridge.rs`（L1063 起） |
| 前缀推进改为“扫窗口一次写” | `crates/storage/src/sync_bridge.rs`（L1340 起） |
| Phase 0 回归（指标对账、批量等价） | `crates/sync/tests/engine.rs`（L1483/L1567） |
| 两设备落后追赶基准 | `crates/sync/tests/phase0_catchup.rs` |
| inbox 批量语义/冲突/前缀测试 | `crates/storage/tests/sync_inbox_batch.rs` |
| inbox 逐条 vs 批量基准 | `crates/storage/tests/sync_inbox_bench.rs` |
| app 摘要展示指标 | `crates/app/src/dev_sync/service.rs` |

复跑命令见 metrics §2；基准带 `#[ignore]`，不会进普通 CI。

## 4. 关键数字（请按“量级/比值”读）

- storage 批量 inbox：256 行 **270.1 ms → 7.6 ms（35.6×）**；
  1024 行 **799.3 ms → 35.2 ms（22.7×）**。
- 两设备 600 条 update：pull **674 → 60 ms（11.2×）**，integrate **3426 → 3679 ms**，
  总时间 **4100 → 3739 ms（1.10×）**。
- 两设备 2000 条 update：pull **1981 → 189 ms（10.5×）**，integrate **31753 → 31735 ms**，
  总时间 **33734 → 31924 ms（1.06×）**。
- 单文档热路径 integrate ≈ `本页事件数 × 完整历史重放`：历史 0 时 0.9 s/256 条，
  历史 1792 时 6.8 s/256 条；`replay_probe_ms` 随历史线性增长，每轮 integrate 近似
  `256 × 单次重放`，即二次成本。
- 结论：**integrate 是主瓶颈；批量 inbox 只解 pull，不是追赶主解**。
  计划建议先做“按文档聚合 import”，再做 checkpoint 快速追赶。

## 5. 请高级模型重点判断的问题

1. **度量方法是否成立**：`elapsed_ms` 是墙钟（含 SQLite fsync），`update_bytes` 是明文
   负载下界；`FakeRelay` 无网络/无 TLS；只取中位数/单场景。这样的证据是否足以支撑
   “integrate 是主瓶颈”和 Phase 1 阈值？
2. **O(历史 × 事件) 的归因是否准确**：`load_body_doc` → `load_replay_parts` 每事件全量
   重放；是否还有 Loro `import`、投影、A 事务、`mark_integrated` 等未拆分成本？
   用 `replay_probe_ms` 外推是否会把重放成本算重？
3. **按文档聚合 import 是否安全且真能消掉瓶颈**：同一页同文档 pending update 能否一次
   `load_body_doc`、按因果顺序批量 `import`、一次 A 事务 + 一次 `mark_integrated`？
   怎么保持 durable `stored`、deferred、崩溃恢复、`integrated` 前缀与 ACK 语义？
4. **阈值是否合理**：`S - received >= 500` / `estimated_replay_bytes >= 8 MiB` /
   `CursorExpired` / snapshot ≤ 128 MiB，是否应改为“按文档聚合后的实测成本”动态判断？
   500 这个初始值是否太激进/太保守？
5. **checkpoint 追赶仍值得吗**：如果聚合 import 把 integrate 降到接近一次重放，
   快照追赶的收益是否只剩“少 replay 日志”？计划里的 Asset 只补缺失、写闸拆分、
   inbox 清理是否应按原优先级做？
6. **批量 inbox API 的正确性**：冲突整批不写、重复 seq 拒绝、跨 ASSET 空洞前缀推进、
   8 MiB 分批、事务后推进前缀，是否有竞态/半写/游标漏洞？生产默认关闭是否应保持？
7. **还缺什么证据**：真实 relay/TLS、快照下载/merge/`gate_hold_ms`、多文档、
   1–10 MB 文档、附件占比、Windows/DPAPI、GC 边界；哪些必须在 Phase 1 前补？

## 6. 已知边界（不要在评审中当成遗漏）

- Phase 1 **未实现**：没有 `CatchUpMode`、没有写闸拆分、没有 snapshot 下载/merge 实测。
- 常规同步面板行**没有**显示字节/耗时；指标在 `PullReport`/`IntegrateReport` 和
  `[sync]` 日志、worker 摘要里。
- inbox 清理 API 尚未做；计划明确“只能在 finalize 成功且 integrated ≥ S 后清理”，
  否则会破坏 checkpoint coverage 证明。
- 触发公式必须用 `S - received_seq`，不能用 `head - received_seq`；`/bootstrap` 响应
  没有快照字节数，需要另取 manifest 估算。
- 计划已否定“服务端只留 id + r7”；本评审不需要再评估该方向。

## 7. 评审产出建议

请按以下格式回：

- **结论**：Phase 0 证据是否足以进入 Phase 1；按文档聚合 import / 批量 inbox /
  checkpoint 追赶的优先级是否同意。
- **必须修正**：按严重度列，指出具体文档/代码位置。
- **阈值建议**：给一个可执行的初始阈值与需要补测的校准项。
- **新增风险**：特别是并发编辑、崩溃、epoch/rotation、inbox 清理与 coverage 证明。
- **不建议做**：明确哪些应后置，避免继续扩大范围。
