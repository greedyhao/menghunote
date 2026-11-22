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

### 8.7 `checkpoint validate` 覆盖日志扫描（S = 1k / 5k / 25k / 1M，2026-11-21）

> 任务 4 P1 探针：`crates/sync/tests/phase1a_release.rs::checkpoint_validate_scan_cost`
> （`#[ignore = "manual benchmark"]`）。只加基准与文档：**不改**生产
> `scan_coverage_log`，不改 relay / `sync-proto` / schema，未 commit。
>
> **M3 标定补充（同日）**：同一探针新增 **S = 1 048 576（= 4096 页）上限档**，
> 且该档只跑 1 次；另新增 `large_doc_per_page_replay_cost` 大文档 replay 探针。
> 结果与限制见 §8.7.7 / §8.7.8。两节都只打印、不 assert 时间；3 s / 16 MiB /
> `N_min = 25 000` 阈值与 `COVERAGE_SCAN_MAX_PAGES = 4096` 均未改。

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
  取中位数；一次进程内 4 档串行（1k → 5k → 25k → 1M）。**1M 上限档只跑 1 次**
  （避免百万事件 × 3，见 §8.7.8）；本节表中 3 档仍是 3-run 中位数。
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

> 上面是初次 3 档 3-run 批次的原始输出（旧 summary 行格式：`(median of 3; ...)`）。
> M3 追加 1M 档时 summary 行改为 `S=... pages=... scan_ms=... log_ms=... validate_ms=...`；
> 旧批次数字不回改，新格式的 1M 档原始输出见 §8.7.8。

#### 8.7.4 结果

| S | 事件（ASSET / UPDATE） | pages | `scan_ms` 中位数 | raw `scan_ms` | `log_ms` 中位数 | visitor ≈ | `validate_ms` 中位数 | prepare pages / `prepare_scan_ms` |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 1 000 | 143 / 857 | 4 | **0.076 ms** | 0.074–0.083 | 0.057 ms | 0.019 ms | **0.542 ms** | 4 / 0.114 ms |
| 5 000 | 715 / 4 285 | 20 | **0.479 ms** | 0.440–0.617 | 0.344 ms | 0.135 ms | **0.919 ms** | 20 / 0.450 ms |
| 25 000 | 3 572 / 21 428 | 98 | **3.796 ms** | 3.779–3.829 | 3.183 ms | 0.613 ms | **4.376 ms** | 98 / 3.660 ms |

> 上表是**初次 3-run、单探针批次**（§8.7.3 的 `scan-cost-run.log`，一次进程只跑本探针）。
> 最终 release 复跑（4-run 批次）得到的是 **0.428 / 4.039 ms**（1k 0.078），两批不要混用；
> 批次差异与口径见 §8.7.6。

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
  relay GC 后日志不可读、真实网络抖动；均未测。S = 1M（= 4096 页上限）已由
  §8.7.8 单跑一次覆盖，但只有 1 run、无真实 RTT。
- **页大小耦合**：256 是生产私有常量，探针里用 `ceil(S/256)` 校验页数；生产改页大小
  时探针与本节需同步。
- **时间噪声**：共享 8 vCPU 机器、load 0.1–2.4；5k 的第 3 轮 0.617 ms（比中位数高 29%），
  25k 的 prepare 曾出现一次 8.8 ms 冷启动离群值。绝对值只用于量级/线性度判断。
- 探针**未 commit**；写作用域仅 `crates/sync/tests/phase1a_release.rs` 与本文件。

#### 8.7.6 最终 release 复跑（4-run 批次）与数字口径

2026-11-21 评审收尾时又跑了一次 release 批次：`phase1a_release` 的两个 ignored 探针
（`checkpoint_validate_scan_cost` + `phase1a_baseline_apply_gate_hold`）同进程执行，
scan 仍是同档 3 轮取中位数。这是本探针的**第 4 次运行（4-run 批次）**，不是 §8.7.4
那一批的续测。

| S | pages | `scan_ms` 中位数（4-run 批次） | raw `scan_ms` | `validate_ms` 中位数 |
| --- | --- | --- | --- | --- |
| 1 000 | 4 | **0.078 ms** | 0.074–0.089 | 0.525 ms |
| 5 000 | 20 | **0.428 ms** | 0.419–0.449 | 0.848 ms |
| 25 000 | 98 | **4.039 ms** | 3.849–4.250 | 4.594 ms |

口径（两组都保留，不要混成一组）：

- §8.7.4 的 **0.479 / 3.796 ms** 是**初次 3-run、单探针批次**（`scan-cost-run.log`；
  一次进程只跑本探针，1k → 5k → 25k 串行）。
- 本节的 **0.428 / 4.039 ms** 是**最终 release 复跑（4-run 批次）**；同一进程还并行
  跑了 `baseline_apply` 探针，机器负载更高。25k 比初次高约 6%（4.039 vs 3.796），
  5k 低约 11%（0.428 vs 0.479）。**这是批次/负载差异，不是 scan 代码变化**：
  `scan_coverage_log` 两批之间未改，pages 仍严格 = `ceil(S/256)`。
- 评审摘要（`sync-review-272k.md` / `sync-review-1m.md` 的 §0）引用的是**本节这批
  0.428 / 4.039**；正文 §8.7.4 是初次 3-run 的 0.479 / 3.796。引用时必须带批次名，
  不能把两组放进同一张表或算同一个中位数。
- 绝对值仍只用于量级/线性度：1k 0.076 → 0.078、25k 3.796 → 4.039，都在共享机器
  噪声量级内；页数（4 / 20 / 98）与“visitor 线性、页数严格 = `ceil(S/256)`”的结论
  不受影响。

#### 8.7.7 M3 大文档 per-page replay（1 MiB 单文档，两种 update 形状，2026-11-21）

> 探针：`crates/sync/tests/phase1a_release.rs::large_doc_per_page_replay_cost` /
> `fragmented_doc_per_page_replay_cost`（均 `#[ignore = "manual benchmark"]`）。与 §8.7 的 coverage scan 无关：它测 aggregate
> 每页为每个 **touched doc** 做的全量 `load_body_doc`（快照 + 该 doc 全部
> `loro_updates` 重放）里，**单篇 1 MiB 正文**这一形状。只打印，不 assert 时间。

**环境**：Linux 6.8.0-149（Ubuntu），Intel Xeon Gold 6330 @ 2.00 GHz，8 vCPU，
15 GiB RAM，ext4（`/dev/sda1`）；`rustc`/`cargo` **1.100.0**，`--release`
（`opt-level=3`、`debug=0`、无 LTO）。同一台共享机器，测量时 load average 约
**0.5–2.4**（8 vCPU）。下表是**单探针单跑批次**（`cargo test ... large_doc_per_page_replay_cost
-- --ignored --nocapture`，一次进程只跑本探针；日志
`target/tmp/phase1a-logs/large-doc-replay-run.log`）。

**文本形状**（只此一档）：
- fresh vault（`CARGO_TARGET_TMPDIR` 下真 SQLite）+ `seed_doc("large-doc", LOCAL_PEER=0x41)`；
- 确定性伪随机 **ASCII** 正文 **1 MiB**（`pseudo_text`，小写字母 + 数字；byte ==
  Unicode scalar == UTF-8 字节）；一次 `insert_scalars(0, &text)` + `commit_local`；
- 落库恰好 **1 条 `loro_updates` 行**（单条大 insert op），Loro update payload
  **1 048 671 B**（1 048 576 B 正文 + 约 95 B 的 Loro 结构开销）。

**测量**：连续 **5 次** `load_body_doc(&vault, "large-doc", LOCAL_PEER, None)`，每次都是
新的 SQLite 读取 + snapshot/update import（不共享 BodyDoc）；每次断言
`scalar_len == byte_len == 1 048 576`、`head_seq` 等于提交后水位（与时间无关）。

| 项目 | 数值 | 说明 |
| --- | --- | --- |
| raw `replay_ms`（5 次） | **2.536 / 2.438 / 1.917 / 1.793 / 1.975 ms** | 第 1 轮含冷页缓存 |
| `replay_ms_median` | **1.975 ms** | 5 次中位数（最终复跑 1.996） |
| `replay_ms_warm_median` | **1.975 ms** | 去掉第 1 轮后的 4 次中位数 |
| `pending_update`（空 VV，参考） | 1.610 ms / 1 048 671 B | 全量 update 导出，不是 replay |
| `export_snapshot`（参考） | 7.580 ms / 2 097 403 B | snapshot ≈2.0× 正文 |
| `BodyDoc::restore(snapshot)`（参考） | 3.688 ms | 只含 snapshot import |

```text
[phase1a] large_doc_replay run=0 replay_ms=2.536
[phase1a] large_doc_replay run=1 replay_ms=2.438
[phase1a] large_doc_replay run=2 replay_ms=1.917
[phase1a] large_doc_replay run=3 replay_ms=1.793
[phase1a] large_doc_replay run=4 replay_ms=1.975
[phase1a] large_doc_replay doc_mib=1.000 text_bytes=1048576 loro_payload_bytes=1048671 update_rows=1 replay_runs=5 replay_ms_raw=[2.53614, 2.438051, 1.917222, 1.7926360000000001, 1.9749149999999998] replay_ms_median=1.975 replay_ms_warm_median=1.975
[phase1a] large_doc_replay_ref pending_update_ms=1.610 pending_bytes=1048671 export_snapshot_ms=7.580 snapshot_bytes=2097403 restore_from_snapshot_ms=3.688
```

**第二种形状：同一篇 1 MiB 正文拆成 256 条 4 KiB update**（`fragmented_doc_per_page_replay_cost`），
模拟真实库中“大文档 + 多批增量”而不是单条大 insert：

| 项目 | 数值 | 说明 |
| --- | --- | --- |
| `loro_updates` 行数 | **256** | 每 4 KiB 一次 `commit_local` |
| raw `replay_ms`（5 次） | **8.421 / 6.210 / 5.965 / 5.569 / 5.743 ms** | 首轮冷缓存 |
| `replay_ms_median` | **5.965 ms** | 仍远低于 2 s（最终复跑 5.322） |

结论：**单条大 op 与 256 条增量都只是毫秒级**。真实大文档的 per-page replay
成本由 update 条数/并发形状决定，但本机 1 MiB × 256 行也只有 ≈6 ms；不能把
verify 的 45 s 折算成 per-page replay。

读法：

1. **per-page replay 是毫秒级，§10.4 的 ~2 s/篇折算不成立。** 单条大 insert ≈2 ms，
   256 条 4 KiB 增量 ≈6 ms；§10.4 的 45 s 是 **baseline_apply 的 `verify_imported`**——
   每篇要 import `local` + `baseline_merge` 两行 update（后者是并发合并后的组合
   update），含 attached import 的 diff/merge 成本（§8.4b 的 5→20 MiB 超线性），
   不是 `load_body_doc` 的纯 replay。**不能把 verify 的 45 s 折算成 per-page 2 s**。
   仍需保留的结论是：大文档的 **verify/attached merge** 成本高；纯 replay 不是
   aggregate 页的已知瓶颈（本机 1 MiB 最多 ~6 ms）。
2. **本地 replay 本身不是 aggregate 页的瓶颈**：本探针只量 `load_body_doc` 这一小段，
   不含该页的 pull/解密/多文档 visitor；单篇 1 MiB 的 replay ≈2 ms。
3. **visitor/固定成本另计**：snapshot 导出/导入（7.6 / 3.7 ms）都比 update replay 贵，
   快照约 2.0× 正文；若后续 catch-up 用 snapshot 恢复单篇，不能拿 2 ms 代替。
4. **时间只用于量级**：5 次都在 1.8–2.5 ms；首轮冷缓存 2.54 ms，稳态 ≈1.8–2.0 ms。
   共享机器上 ±20% 属正常，不做阈值断言。
5. 该探针**不改变** §9.1 的 3 s 持闸预算 / 16 MiB 机会性上限 / `N_min = 25 000`，
   也不改任何生产 replay 路径；它只说明"纯 update replay"不是这些阈值的瓶颈。

整文件并行批次（`-- --ignored` 同时跑 4 个探针：baseline / large / frag / scan，2026-11-21）两次都通过
（3 passed 旧批次 / **4 passed 63.31 s**），同批 large-doc 中位数 **1.842 / 2.032 ms**
（第二次 raw 3.855 / 2.713 / 2.032 / 1.836 / 1.808——并行时另一探针的 CPU 竞争
把第 1 轮抬高）。两批都在噪声量级内；**表格取单跑批次**，并行批次只作为
可复跑证据。

#### 8.7.8 4096 页上限档（S = 1 048 576，单 run，2026-11-21）

> 探针仍是 `checkpoint_validate_scan_cost`（§8.7），只是 `SCAN_COST_S` 追加
> `1_048_576 = 4096 × 256`；**1k/5k/25k 仍 3 次取中位数，1M 档只跑 1 次**。
> 生产常量 `COVERAGE_SCAN_MAX_PAGES = 4096`、`COVERAGE_SCAN_PAGE_LIMIT = 256`
> 未改；`pages` 仍硬断言 = `ceil(S/256)`。

**结论先说：4096 页本机真跑通了，没有降档。** `S = 1 048 576` 恰好用满
`COVERAGE_SCAN_MAX_PAGES = 4096` 次页请求（每页 256 条）并在最后一页到达 S；
按 `scan_coverage_log` 的循环上界（代码路径，未单独构造失败用例），再多 1 条事件
（4097 页）会返回 `未证明覆盖到 S（coverage scan 页数超过上限）`。本档
`covered_seq == S`、`coverage_verified_candidates == 1`、`pages == 4096`
三条断言全部通过。

**结果（单探针单跑批次，日志 `target/tmp/phase1a-logs/scan-cost-4096-run.log`）**：

| 项目 | 数值 |
| --- | --- |
| S | **1 048 576**（ASSET 149 797 / UPDATE 898 779，14.3% ASSET） |
| pages | **4 096**（严格 = `ceil(S/256)`） |
| 夹具 setup（建 1M 事件 + prepare 扫描） | **13 737 ms**（其中 prepare scan 13 403 ms） |
| prepare pages / `prepare_scan_ms` / `prepare_log_ms` | 4 096 / **13 403 ms** / 13 372 ms |
| validate `scan_ms`（1 run） | **12 848 ms**（最终复跑 12 248 ms） |
| validate `log_ms`（1 run） | **12 814 ms** |
| visitor ≈ `scan_ms - log_ms` | **≈33 ms**（1.05M 事件 ≈32 ns/事件） |
| validate `validate_ms`（1 run，含 manifest/快照/尾部 ASSET） | **12 848 ms** |
| 进程峰值 RSS（夹具 + 1M 内存事件） | **≈202 MiB**（`VmHWM` 202.3；`/usr/bin/time -v` 206 828 KB） |

同批次常规档（3-run 中位数，仅为对照）：1k `scan_ms` **0.074 ms** / 4 页，
5k **0.421 ms** / 20 页，25k **3.822 ms** / 98 页；与 §8.7.4/§8.7.6 的批次同量级，
常规档口径仍以那两节为准。

```text
[phase1a] scan_cost setup S=1048576 asset_events=149797 update_events=898779 asset_pct=14.3 prepare_pages=4096 prepare_scan_ms=13403.399 prepare_log_ms=13372.472 setup_ms=13737.377 runs=1 peak_rss_mib=202.3
[phase1a] scan_cost S=1048576 run=0 pages=4096 scan_ms=12847.716 log_ms=12814.289 validate_ms=12848.455 part_calls=4 part_ms=0.003
[phase1a] scan_cost S=1048576 pages=4096 scan_ms=12847.716 log_ms=12814.289 validate_ms=12848.455 runs=1 raw_scan_ms=[12847.715872] raw_log_ms=[12814.289152] raw_validate_ms=[12848.454609999999] part_calls=4 part_ms_raw=[0.00316]
```

读法与限制：

1. **`scan_ms` 的 12.8 s 几乎全是 FakeRelay 的夹具成本，不是生产扫描成本。**
   `log_ms` 12.81 s、visitor 只有 ≈33 ms；FakeRelay 的 `log()` 每页都从 `Vec` 头
   `iter().filter(seq > cursor).take(256)`，第 p 页要跳过约 `256·p` 条，4096 页合计
   ≈`256 × (4095×4096/2)` ≈ **2.15×10⁹ 次跳过**。真实 relay 用游标/索引取页，
   单页 O(256)，不会随页号线性增长。因此**本表 `scan_ms` / `validate_ms` 是本地
   （夹具）上界**，25k → 1M 的 3361× 放大（3.822 ms → 12 848 ms，页数只 ×41.8）
   正是这个 artifact。
2. **4096 页的真实成本 = 4096 × RTT + 本地 visitor**：页请求是串行的；visitor 实测
   ≈32 ns/事件（1.05M 事件 ≈33 ms，与 25k 档 ≈0.6 ms 同量级）。本机没有真实
   relay/TLS/RTT，**不能把这 12.8 s 外推成生产延迟**。按算术（不是实测）：
   RTT 10 ms/页 → 4096 页 ≈41 s；RTT 50 ms/页 → ≈205 s。真实 relay 的单页
   查询/传输成本、TLS、丢包重试、relay GC 都未测，机会性追赶启用前仍需真实
   relay RTT 标定。
3. **prepare 也会扫一遍同一区间**：1M 档 `prepare_scan_ms` 13.4 s，与 validate
   同量级；一次 `prepare → validate` 最多访问 2×S 条事件。若本地账簿已完整覆盖
   （`missing` 为空），`ensure_coverage_proven` 直接返回、不扫描（生产路径，本档
   不是这个形状）。
4. **峰值 RSS ≈202 MiB 是夹具内存，不是生产内存**：FakeRelay 在 `Vec` 里持有
   1M 条 `RelayLogEvent`（每条一个 32 B 载荷 `Vec` + 固定字段），而生产 validate
   按页流式处理 256 条，不会同时持有整段 1M 事件。真实 relay/客户端缓存另需实测。
5. **1M 档只有 1 run**：不取中位数、无重复性数据；1k/5k/25k 仍为 3-run 中位数。
   1M 的绝对时间受共享机器负载影响，只用于三个结论：**能跑通、pages 严格
   4096、内存量级 ≈200 MiB（夹具）**。
6. **本节不改变任何生产阈值**：3 s 持闸预算、16 MiB 机会性硬上限、
   `N_min = 25 000`、`COVERAGE_SCAN_MAX_PAGES = 4096` 全部保留；也未改
   `scan_coverage_log` 实现。

> 口径说明：追加 1M 档时 summary 行改为
> `S=... pages=... scan_ms=... log_ms=... validate_ms=...`（§8.7.3 的
> `(median of 3; ...)` 是历史输出，不回改）；1M 档单 run 直接填中位数位置。
> 整文件并行批次（3 探针同跑，`-- --ignored`，2026-11-21）两次都通过
> （旧批 3 passed / 62.96 s、63.67 s；新批 4 passed / 63.31 s），但并行时基线探针占内存/CPU，1M 档 `scan_ms`
> 升到 **13 345 / 13 049 ms**、夹具峰值 RSS **348 MiB**（`VmHWM`，隔离单跑是
> 202 MiB）——**不要用并行批次数值**；本节表格取单跑批次。
