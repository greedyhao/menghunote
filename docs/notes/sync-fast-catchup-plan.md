# 同步快速追赶（历史链压缩）计划

> 2026-11-20 计划；2026-11-22 已实现 Phase 1B MVP（见 §11），未改 relay wire/schema。
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

> 本节是**权威约束**；§6 的“500 条”初值已被本节取代。Phase 1B MVP 已于
> 2026-11-22 实现（见 §11）；阈值/标定结论仍以本节为准。

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

---

## 10. 性能校准（2026-11-21 review 后）

> 本节只校准**结论口径**，不改变 §9.1 的 3 s 持闸预算 / 16 MiB 机会性硬上限 /
> `N_min` / 成本交叉；也**不表示** Phase 1B 已实现（无 `CatchUpMode`、无
> prepare/integrate 拆分、无 snapshot merge 实测）。

1. **`N_min = 25 000` 是诚实兜底，不是“保守”。** 聚合后的 5 000 条尾部 replay
   只要 0.28 s（单文档）/ 0.76 s（20 篇）；而机会性追赶要下载/解密/验签整份快照，
   并在闸外 `verify_imported` 重放。按 §9.4 的形状，5 MiB verify 已 2.7 s、
   20 MiB 约 45 s（5→20 为 16.7×）。25k 以下启动快照追赶，省下的 replay 远小于
   快照 + verify 的固定成本——这是成本交叉算出来的 floor，不是故意调高的保守值。
2. **Phase 1B 的核心价值是 correctness，不是速度。** 当 `gc_floor > received_seq`
   时普通 `pull` 只会 `CursorExpired`，没有可回退的增量路径；checkpoint 追赶是
   唯一能离开过期游标的路径。速度收益只在“≥25k 事件、且差距极大”的极端场景成立；
   近端增量继续走普通聚合 pull，不为了“快”而拉整库快照。
3. **16× 超线性重放只是被移出持闸段，没有被消除。** detached oplog 把 5→20 MiB
   的 storage merge 压到 35 / 110 ms（3.1×，4× 数据），但闸外 `verify_imported`
   仍走 attached `load_body_doc` 重放大 update：5 MiB ≈2.7 s、20 MiB ≈45 s
   （最终 release 复跑 58.8 s，同量级；5→20 约 16–17×）。因此“`gate_hold` 小”
   不能当成“全链路 <3 s”，也不能据此放高 16 MiB 机会性上限。
4. **“每页 ~2 s/篇”的折算不成立；大文档成本在 verify，不在纯 replay。** M3 实测
   （`sync-phase0-metrics.md` §8.7.7）：1 MiB 单文档、1 条大 insert 的
   `load_body_doc` ≈2 ms；拆成 256 条 4 KiB update ≈6 ms。§10.3 的 45 s@20MiB
   是 **baseline_apply 的 `verify_imported`**（`local` + `baseline_merge` 两行
   update 的 attached import/merge），不是 aggregate 每页的 `load_body_doc`。
   因此：
   - 不能再拿“1 MiB 每页 2 s”给 25k/16 MiB 阈值背书；
   - 也不能反过来宣称“大文档没问题”：**verify/attached merge 仍可能 45 s+**，
     且 107 KiB 基准没有覆盖“大文档 + 多 update + 并发合并”的 verify 形状；
   - 机会性追赶若启用，仍必须单独标定 verify 的大文档形状，不能只测 per-page replay。
5. **阈值与预算不动。** §9.1 的 3 s 持闸预算、16 MiB 机会性硬上限、`N_min =
   25 000`、2.0×/1.5× 成本交叉全部保留；Phase 1B **未实现**，上述数字只用于
   评审与后续设计。


---

## 11. Phase 1B MVP 实现（2026-11-22）

### 已实现

- **`bootstrap::prepare` / `bootstrap::integrate` 拆分**：prepare 只做
  `/bootstrap` 选择/下载/解密/验签/校验，不写本地库；integrate 才走
  `baseline_apply`/`finalize`/abort。`run` 仍等价于 `prepare + integrate`。
- **`BootstrapAdvancement::CatchUp`**：成功导入并证明 `integrated >= S` 后允许把
  relay cursor 前移到实际选中 checkpoint 的 cursor；`MaxOnly` 仍是新设备 bootstrap 的
  原有 max 语义。
- **`catchup` 模块**：
  - `CatchUpMode::{Mandatory, Opportunistic}`、`CatchUpConfig`（`N_min=25_000`、
    16 MiB 机会性上限）、`decide`、`should_attempt`；
  - `prepare`（不持闸）与 `integrate`（调用方持闸）；integrate 后调用
    `prune_covered_inbox`，只在 `integrated >= S` 时删除已覆盖行。
- **两阶段接入**：`DevLocal::sync_prepare` 在强制过期或落后上界 ≥ `N_min` 时先准备
  catchup，命中后跳过普通 pull；`DevLocal::sync_integrate_with_catchup` 在持闸段
  执行追赶，并返回 `IntegratePhaseReport.catchup`；app worker 已携带
  `PrepareReport.catchup`。
- **完整 `sync_once`**：同样先尝试 catchup，再走普通 push/pull/integrate；强制追赶失败
  映射为 `Paused(CatchupFailed)`，不回退普通 pull。
- **缺失资产**：`load_baseline` 接收 vault；本地已有同 hash/长度/SHA256 的资产不再
  从 relay 下载，仍纳入 verify/apply。
- **机会性失败回退**：prepare 阶段失败时尚未写库，返回 `None` 让普通聚合 pull 继续；
  Mandatory 失败仍 `Paused(CatchupFailed)`。
- **S 之后普通 pull**：追赶测试追加“源端 checkpoint 后 push、目标端普通 pull”回归。
- **测试**：`catchup` 决策单测、`bootstrap` prepare 不写库 + integrate 前移 cursor +
  保留本地 op + 缺失资产、`prune_covered_inbox` watermark 门禁。

### MVP 的已知限制

1. **机会性已先做 manifest 预检**（`bootstrap::probe`）：只下载 manifest 读实际选中
   checkpoint 的 S/明文字节，满足阈值才进入完整 snapshot 下载；下载后仍会用实际
   fallback 的 S/字节复核一次，超限丢弃并回退普通 pull。
2. **成本交叉只做了事件差 + 16 MiB 快照上限**，没有 §9.1 的 1.5× 时间交叉；
   16 MiB 的 `verify_imported` 仍可能超过 3 s 持闸预算（见 §10）。
3. **`gate_hold_ms` 只记录在报告/日志，未作为硬中止条件**；超 3 s 不会自动无损回退。
4. **强制失败只映射 `Paused(CatchupFailed)`**，没有独立指数退避状态机；依赖 app 的
   自动同步冷却。
5. **快照/资产全量下载**；未做增量/文档级快照（wire v2）。
6. **真实 relay RTT/TLS 未标定**；机会性追赶默认仍只由 `N_min`/16 MiB 控制，
   生产启用前必须补真实标定（按用户要求本轮不测）。
7. **UI 未单独显示“快速追赶”阶段**；报告字段已有，但面板仍按普通 prepare 文案。
