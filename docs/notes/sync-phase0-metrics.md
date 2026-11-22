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

**checkpoint 快速追赶阈值（建议初值，待 Phase 1 实测校准）**：

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
