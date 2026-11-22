# 同步评审精简稿（272K 上下文，Phase 1B MVP 后）

> 代码/文档截止 **`038e92c`**。包含 Phase 1B checkpoint 快速追赶、M3/M4、
> marker/Corrupt/pin/close 修复。**Phase 1B 是 MVP，不是全量设计收口**。
> 约 738,337 字符，估计 **23–27 万 token**；附录 C 只放 Phase 1B diff
> （`7583d8a..038e92c`），完整历史 diff 在 1M 稿。真实 relay RTT/TLS **未做**。

## 提示词（复制给模型）

你是 Rust / Loro / SQLite 同步评审者。只依据本 Markdown；不要假设能运行 cargo 或访问仓库。
先确认是否读到附录 C 末尾；若没有，指出最后看到的文件/函数。按下面审：

1. **Phase 1B 双模式**：Mandatory（gc_floor>received，不受 16MiB 拦截）/
   Opportunistic（S-received>=25_000 且快照估算<=16MiB）；`should_attempt`、
   manifest 预检、fallback 后二次决策；强制失败 Paused、机会性回退普通 pull。
2. **prepare/integrate 拆分**：prepare 是否真的不写库/不持闸；只下载缺失资产；
   integrate 是否只在持闸后 baseline_apply/finish/游标 CAS；fallback 的 S/cursor 绑定。
3. **inbox prune**：`prune_covered_inbox` 的 `EXISTS integrated>=S` 证明；缺失/不足水位、
   并发推进、log incarnation 边界。
4. **S 之后普通 pull**：cursor 前移、下一轮增量、本地 op/outbox 保留。
5. **失败语义**：Mandatory Paused；Opportunistic prepare 回退；Opportunistic integrate
   失败直接返回错误是否有丢数据/卡死风险。
6. **gate_hold_ms**：只记录（>3s 告警）不硬中止；3s 预算/verify 风险。
7. **Phase1A/1B 交互**：detached merge、verify、finishable、marker、prune 顺序。
8. **测试覆盖**：决策、prepare 零写、Mandatory 公共入口、S 后 pull、prune watermark；
   缺 DevLocal/app 端到端、Opportunistic 真实触发、integrate 失败回退。

硬性检查：integrated>=S 前不删 inbox/skipped_asset；seq=head+1；received/integrated 连续；
remote/adopt 不产生 outbox；无第二写连接/网络持闸；不改 relay wire/schema。
输出完成/部分/不能判断 + 必须修正 + 未关风险。

---

## 0. 状态与证据

| 项 | 状态 | 证据 |
|---|---|---|
| Phase 1B Mandatory/Opportunistic | 完成（MVP） | catchup::decide/should_attempt；3 条决策单测 |
| catchup prepare 不写库 | 完成 | catchup_prepare_writes_nothing... |
| manifest 预检/缺失资产 | 完成（代码） | bootstrap::probe、load_baseline 本地资产跳过 |
| catchup integrate 持闸/游标 CAS | 完成 | BootstrapAdvancement::CatchUp；Mandatory 公共入口测试 |
| S 后普通 pull | 完成 | 追赶测试追加源端 push/目标 pull |
| prune only integrated>=S | 完成 | prune_covered_inbox + watermark 测试 |
| 失败 Paused/回退 | 部分 | Mandatory Paused；Opportunistic prepare 回退；integrate 失败未回退 |
| gate_hold 硬预算 | 未做 | 只记录/告警 |
| 真实 RTT | 未做（按用户） | 无真实 relay 标定 |

```text
sync 646 / 0 / 4 ignored; storage 310 / 0 / 1 ignored; app 740 / 0
workspace 2932 / 0；dev-sync 3083 / 0；fmt/clippy 全绿
```

---

# 附录 A：关键文档

## `sync-fast-catchup-plan.md`（含 §11 实现）

`````markdown
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

`````

## `sync-hardening-20261121.md`

`````markdown
# 同步硬化修复记录（2026-11-21）

基线：`f8b02e1`（代码基线 `5d01e9e`）。本轮只做 Phase 1A 收尾硬化；
**未改 relay / sync-proto wire / SQLite DDL / checkpoint 签名 / 游标连续语义**。

## 1. P1 状态

| 项 | 状态 | 提交 | 测试 |
| --- | --- | --- | --- |
| P1-1 finish 完整性 | 完成 | `2ed485b` | `finish_without_all_docs_is_conflict`、`finish_after_full_batches_is_applied`、`begin_idempotent_replay_keeps_expected_set`、`finish_without_all_assets_is_conflict` |
| P1-2 tombstone TOCTOU | 完成（SQL 事务内重判） | `2ed485b` | `tombstone_outbox_race_between_decision_and_transaction_keeps_note` |
| P1-3 Journal HeadConflict | 完成（生产已接线；不是 JournalWriter rebuild 路径，见下） | `aba4375` + `3c5ddcf` | `flush_recovers_from_head_conflict_by_rebuilding_sink`、`flush_reports_unrecoverable_after_bounded_rebuild`、`flush_reports_unrecoverable_when_new_head_is_unreadable` |
| P1-4 ready/count log_id | 完成 | `be5244b` | `ready_and_count_only_include_current_log_incarnation`、`ready_where_path_filters_other_log_incarnation`、`ready_without_log_id_is_empty_and_invalid_length_is_corrupt` |

> **P1-3 最终口径（2026-11-21 晚）**：生产 `session/flush.rs` 已接线，但走的**不是**
> `JournalHeadStale` / `JournalWriter::rebuild_sink`：它的 `HeadConflict` 路径是
> `load_head` → 仅当新 head 的 ops ⊆ 未 ACK 前缀 `end_vv` 时同字节重试一次；
> 未知 ops fail-closed 要求重开；仍单后台 writer、无第二 journal、不重新导出增量。
> 失败语义与恢复路径以 `crates/storage/src/journal.rs` 模块头“两条恢复路径”为准。

### P1-1 细节

- `begin_baseline_apply` 增加 `BaselineExpectation { docs, assets }`，与 journal 同事务写 `sync.baseline_expected`（本地 `meta`，非 DDL）。
- `finish` 在任何 meta 写入前逐项比对 `baseline_apply_docs.applied=1` / `baseline_apply_assets.applied=1`；缺失/多出/集合不符 → `Conflict`，零写。
- `finishable` 改为 `staging && expected 全齐 && 无 verify 失败标记`；无记录不可 finish。
- 同参数 begin 幂等；同 checkpoint 换集合 → `SyncConflict`。
- 旧 journal 无 expected 记录 → `Conflict`（拿不到证明不推进 S）。

### P1-2 细节

- 事务外仍做 VV/outbox 预判；事务内 `baseline_apply_tombstones` 用 `CASE WHEN EXISTS(sync_outbox...)` 重判；`notes` 删除只命中 `action='deleted'` 且无 outbox 的行。
- 安全方向单一：事务内只能把 deleted 降级为 kept_local_edit，不能反向升级；判定/写入之间无窗口（A 事务 BEGIN IMMEDIATE 串行化）。
- 失败模式：判定后插入本地编辑 → notes.deleted 保持 0，tombstone 记 kept，`kept_local_edit` 计数；不会整批回滚昂贵 merge。

## 2. P2 状态

| 项 | 状态 | 提交 | 测试 |
| --- | --- | --- | --- |
| P2-5 remote_known 单调性 | 完成（值 CAS） | `800cc47` | `concurrent_writers_from_same_old_converge_to_union`、`stale_remote_known_cas_fails_closed_and_rolls_back`、`begin_backfills_remote_known_*` |
| P2-6 meta 读取语义 | 完成（BLOB-only，非 BLOB `Corrupt`） | `2019490` | `text_meta_is_corrupt_from_all_three_entry_points`、`blob_meta_reads_consistently_from_all_three_entry_points`、`missing_meta_reads_as_absent_from_all_three_entry_points` |
| P2-7 exec_transaction Immediate | 完成（共享 `begin_immediate`） | `2019490` | `exec_transaction_under_foreign_write_lock_fails_busy_without_partial_write`、`exec_transaction_takes_write_lock_at_begin` |
| P2-8 空切片语义 | 完成（显式 `OriginMarks`） | `800cc47` | `all_local_origin_does_not_extend_remote_known`、`per_record_origin_only_extends_for_remote_records`、`per_record_origin_length_mismatch_is_rejected` |
| P2-9 资产本地损坏 | 完成（LocalCorrupt 上抛，健康资产继续） | `4960a47` | `corrupt_local_asset_raises_instead_of_counting_retryable_failure`、`corrupt_asset_does_not_block_healthy_upload_but_still_raises` |

### P2-5 细节

- `remote_known::cas_statement`：`expected=Some(old)` 用 `ON CONFLICT ... WHERE meta.value=?old`；`expected=None` 用 `INSERT ... WHERE NOT EXISTS`；均配 `AffectedGuard::exactly(1)`，CAS 不命中整笔回滚。
- `append_updates_inner` 对远端 CAS 最多重试 4 次（重读旧值、重算并集、替换语句）；head CAS 仍在最前，head 真被推进仍 `HeadConflict`；耗尽返回带 context 的 `SyncConflict`。
- `baseline_apply` 批次内 remote_known 写入同 CAS；冲突整批回滚，由调用方重跑幂等批次（不内层重试，避免重建大 snapshot）。
- `begin` 回填改为无条件执行，覆盖 completed journal 与“无 journal 但有残留 docs”的状态；`ON CONFLICT DO NOTHING` 保证不降级。
- `remote_known::load` 收紧为 BLOB-only（NULL/INTEGER/TEXT → Corrupt），对齐 P2-6。

### P2-6/P2-7 细节

- `sync_meta::meta_bytes_of` 是唯一判定；`Vault::get_meta`、`sync_bridge::read_meta`、`sync_meta::read_meta_on` 共用。
- 非 BLOB 的 `meta.value` 不再静默当缺失；`sync.repack_required`/`sync.relay_cursor`/`sync.log_id` 损坏会显式失败。
- `begin_immediate(conn)` 是唯一事务开启点；`exec_inbox_batch` 与 `exec_transaction` 共用，BUSY 语义一致。

## 3. P3 状态

P3 未做（本轮优先 P1/P2，且“不要挤占 P1/P2”）：

- `PendingJournal::peek_prefix` 拷贝优化；
- `PendingJournal::new(0)` 文档/行为一致；
- pull 每页 ASSET 一次前缀推进；
- `PendingInboxRow` 缓存 base/end VV、预检/执行共用纯函数；
- `push` 查询顺序；
- 补 journal 混合 Local/Remote 单测。

## 4. 不变量 → 测试映射

| 不变量 | 证明 |
| --- | --- |
| `loro_updates.seq = head_seq+1`，不得写 sender_seq | 既有 `aggregate_body_*`/`out_of_order_*` + P1-1 finish 后水位；本轮未改 seq 分配 |
| `received/integrated` 只连续推进 | 既有 `sync_meta` 窗口测试；P1-4 log 过滤 + P1-1 finish 完整性 |
| `integrated >= S` 前不删 inbox / skipped_asset | P1-1 finish 完整性测试；P1-2 只软删 notes、tombstone 记 kept；未新增 prune |
| epoch/rotation/log 异常 → Paused/错误 | 本轮未改 rotation/epoch 代码；P1-4 log 过滤避免跨代 ready |
| HeadConflict 有界、不覆盖 head/丢数据 | P1-3 兼容 head 有界重试 + 未知 ops fail-closed；P2-5 值 CAS 回滚 |
| remote/adopt 不产生 outbox | P2-8 `remote_only_flush_with_empty_slots_has_no_outbox` + 既有 engine 测试 |
| 不改 wire/relay/schema | `git diff` 无 relay/sync-proto/schema/migration；仅本地 `meta` 键 |

## 5. 验证

代码基线 `2ed485b` + P2 提交后的实际结果：

```text
cargo fmt --all -- --check                          EXIT 0
cargo clippy --workspace --all-targets \
  --features menghunote-app/dev-sync -- -D warnings EXIT 0
cargo test -p menghunote-storage                    274 passed / 0 failed (1 ignored bench)
cargo test -p menghunote-sync                       634 passed / 0 failed (3 ignored manual)
cargo test -p menghunote-app                        740 passed / 0 failed
cargo test -p menghunote-app --features dev-sync    891 passed / 0 failed
cargo test --workspace                             2932 passed / 0 failed
cargo test --workspace --features menghunote-app/dev-sync 3083 passed / 0 failed
```

> **最终复跑（2026-11-21 评审收尾）**：review 后新增 P1 门禁/访问器/资产批测试，当前计数
> workspace **2932 / 0**、`--features menghunote-app/dev-sync` **3083 / 0**；
> storage **310** / sync **646** / app **740**。
> 上面的 2926 / 3077 是本文件收尾时点的批次；同轮后续新增测试已计入
> 按批次引用，不要把增量当成回归。scan 探针的批次口径：最终 release 复跑
> **0.428 / 4.039 ms** 与 `sync-phase0-metrics.md` §8.7.4 的初次 3-run
> **0.479 / 3.796 ms** 是两批，不要混算（详见该文件 §8.7.6）。

一次环境假失败：共享 `target` 中旧 `gpui-excalidraw` build script 烘入了已删除的 `/home/dev/menghunote-p2check`；`cargo clean -p gpui-excalidraw` 后 app/workspace 全绿，与本轮改动无关。

## 6. 需要用户决策 / 残余

1. **P1-3 生产接线已完成，UI 救济仍缺**：`session/flush.rs` 仍直接使用 `VaultJournalSink`，采用兼容 head 的同字节有界重试，不是 `JournalWriter` 的重新导出；未知外部 ops fail-closed 要求重开。`DocumentSession`/`SessionDriver::last_write_error` 与 `CloseReport.flush_error` 已让非持闸调用方读到原因，但 App 的“导出/复制 live 正文 → 重开”流程仍未接。
2. **P2-5 baseline 冲突策略**：批次 CAS 冲突返回 `SyncConflict`，由调用方重跑整批；是否允许内层重建/重试需决策。
3. **P2-6 兼容性**：非 BLOB `meta` 现在显式 `Corrupt`；旧库若有外部写入的 TEXT 键会拒绝同步（按“屏障键不能静默绕过”取舍）。
4. **P2-9 行为**：本地资产损坏会中止该轮 sync（在 pull/integrate 前），修复后继续；这是 fail-closed，不是网络重试。
5. **P3**：未做，避免挤占 P1/P2。

## 7. 确认

本轮没有修改 relay 服务端、`sync-proto` wire/schema、SQLite DDL/迁移、checkpoint 签名或游标连续语义。

## 8. P1：verify_imported 失败语义（2026-11-21 补）

基线：`7a1a8e9`（在 §1/§2 的 P1/P2 修复之上）。只改 `storage::baseline_apply` 与
`sync::bootstrap` 的本地语义；**没有** relay / sync-proto wire / DDL / 迁移 / checkpoint /
游标连续推进改动。**Phase1B 当时未实现；2026-11-22 已单独实现 MVP，见
`sync-fast-catchup-plan.md` §11。**

### 8.1 失败不回滚已 merge 的 Loro 数据

`bootstrap::verify_imported` 是导入后的**读回校验**，不是数据库事务。它失败时：

1. **不回滚已 merge 的 Loro/已写入资产。** 导入走 `apply_baseline_batch` 的 CRDT merge
   （`BodyDoc::restore` + `import`，幂等）；本地编辑的 op 可能已经与基线 ops 合进同一条
   `loro_updates` 行，无法只回滚"远端部分"。删数据回滚会丢用户本地编辑，因此失败路径
   **只标记、不清数据**。
2. **把 baseline journal 置为不可继续。** 由于 `status` 不能新增 `failed`（见 §8.6），
   失败标记写在本地 `meta` 键 `sync.baseline_verify_failed`（BLOB：
   `MNBASEVERIFYFAIL1` + UTF-8 原因），journal 保持 `staging`，docs/assets 子表与已导入
   数据原样保留。
3. **不推进任何水位。** verify 在 `finalize` 之前失败，`received_seq` / `integrated_seq` /
   `confirmed_highwater` / relay cursor 一个都不写（测试直接断言）。

**契约**（`crates/storage/src/baseline_apply.rs`）：

- `mark_baseline_verify_failed(vault, reason)`：只写上述 `meta`，不改 `status`；
- `begin_baseline_apply` 读到标记即返回 `SyncConflict`（同参数幂等续跑也拒绝），错误文本
  明确要求先 `abort_baseline_apply`；
- `abort_baseline_apply` 在清 journal 的同一事务里删除标记（没有 staging journal 时也会
  清残留标记），已导入的 Loro/资产仍保留；abort 之后才能重新 begin。
- 读取用 `baseline_verify_failure(vault)`：无标记返回 `None`，标记损坏（缺魔数/非 UTF-8）
  返回 `StorageError::Corrupt`，不静默当成"没有失败"。

### 8.2 闸外编辑与 verify 不互斥；用 ⊆ 而不是相等

verify 不持闸、也不阻止编辑；判定方向固定为：

```text
snapshot.actual_vv ⊆ 当前本地 VV     // sub_iter 为空即通过；不是相等
```

导出点之后本地新增的 op 是合法编辑，不能因为 VV 不相等判失败。正文**只在**
`local_vv ⊆ actual_vv`（本地没有快照之外的 op）时才逐字节比较；有本地 extra 时 Loro
合并后的正文必然不同于快照，比较只会误报。实现提取为纯函数 `check_imported_doc`
（`crates/sync/src/bootstrap.rs`），`verify_imported` 与单测共用同一份判定，避免测试另写
一套语义。

### 8.3 verify_ms 只读指标

- `BaselineApplyDebugTimings.verify_ns`：线程本地累计，不持久化、不参与决策；
- `baseline_apply_verify_ms()`：上述值换算成毫秒（向下取整）；
- `record_baseline_verify_time(Duration)`：由 `bootstrap::apply_selected` 在
  `verify_imported` 前后用 `Instant` 计时后记录（失败也记录）；
- **只读**：失败与否只看 `verify_imported` 的返回值，指标不参与任何判断；
- Phase1B 的 gate_hold 报告可在同一线程读 `baseline_apply_debug_timings().verify_ns`
  或 `baseline_apply_verify_ms()`。注意它和现有分段计时一样是**线程本地**，跨线程不聚合；
  `crates/sync/tests/phase1a_release.rs` 现有的本地 `verify_ms` 计时与此指标相互独立。

### 8.4 代码位置

| 文件 | 改动 |
| --- | --- |
| `crates/storage/src/baseline_apply.rs` | 新增 `META_BASELINE_VERIFY_FAILED` / `mark_baseline_verify_failed` / `baseline_verify_failure`；`begin_baseline_apply` 拒绝失败标记；`abort_baseline_apply` 清标记；`BaselineApplyDebugTimings.verify_ns` + `record_baseline_verify_time` + `baseline_apply_verify_ms` |
| `crates/storage/src/lib.rs` | 导出上述新 API |
| `crates/sync/src/bootstrap.rs` | `apply_selected` 在 verify 失败时写标记、计时；提取 `check_imported_doc`；新增 `BootstrapFault::VerifyVvNotCovered`（doc-hidden 测试注入点） |
| `crates/sync/tests/bootstrap.rs` | `verify_failure_keeps_merged_data_and_marks_journal_failed` |

### 8.5 测试

| 测试 | 证明 |
| --- | --- |
| `verify_accepts_concurrent_local_edits_after_export`（`crates/sync/src/bootstrap.rs` 单测） | 导出点 VV ⊆ 当前 VV（含并发本地 op）时通过、不要求相等；无本地 extra 时正文不同才失败；本地缺导出点 op 时失败（方向不可反） |
| `verify_failure_keeps_merged_data_and_marks_journal_failed`（`crates/sync/tests/bootstrap.rs` 集成） | 注入 VV 未覆盖失败后：已 merge 的本地+基线正文仍在、journal 仍 `staging`、失败标记带原因、水位零推进；未 abort 前 begin 拒绝并提示 `abort_baseline_apply`；abort 清 journal+标记但保留 merged Loro；随后重跑成功 |

验证结果见本节末（实际命令输出；与其它并行改动的边界见 §8.6）。

### 8.6 与代码/schema 的冲突（如实记录，未假装修复）

1. **`baseline_apply.status` CHECK 冲突**：`crates/storage/src/schema/v3.sql` 只允许
   `staging|completed`，不能落 `failed`。本任务硬边界禁止 DDL/迁移，因此失败语义放在
   `meta`，**journal 仍显示 `staging`**。后果：`BaselineApplyState.status` 无法单独表达
   "校验失败"，必须另读 `baseline_verify_failure`；UI/续跑逻辑不能把 `staging` 当成
   "可续跑"。这个不一致是记录下来的冲突，没有通过改 schema 绕过。
2. **失败标记门禁已下沉（2026-11-21 review 后补）**：`apply_baseline_batch`、`apply_baseline_assets` 和
   `finish_baseline_apply` 都会在解码/任何写之前检查 `sync.baseline_verify_failed`，有标记
   返回带原因的 `SyncConflict`，零写；`state_from_journal` 的 `finishable` 在标记存在时
   为 false（`staging && expected 全齐 && !verify_failed`）。因此 Phase1B 不能靠"调用方
   自觉先走 begin"来绕过失败 journal。`baseline_apply.status` 仍只有 staging/completed，
   失败只能通过 `baseline_verify_failure` 读取。
3. **bootstrap 的换基线自动清理**：`reconcile_journal` 在"选择了另一份基线"时会对既有
   staging 调用 `abort_baseline_apply`（从而清掉失败标记）。这是既有的清理入口，不是绕过
   `begin` 门禁；同一基线的续跑仍然必须先显式 abort。
4. **verify 不持闸**：正文比较基于同一次 `load_body_doc` 读到的 body+head，二者是同一份
   已提交状态；这保证"读到的本地状态"与快照可比，但**不**证明 verify 期间没有本地写入
   （本来就不互斥）。并发编辑只会增加 ops，⊆ 判定保持成立。
5. **标记写失败时的行为**：`apply_selected` 返回标记写入的 storage 错误（不让原始 verify
   错误静默通过）；此时 journal 仍是可续跑的 staging（与旧行为一致），没有失败标记。这是
   fail-open 到"重试"的窄窗口，记录在此。
6. **标记的触发面是"verify_imported 返回任何错误"**：除校验结论不符（`SyncError::Object`）
   外，读回时的本地存储错误也会置标记。取舍是 fail-closed：宁可多一次 abort 后重跑
   （数据不丢、merge 幂等），也不把状态不明的失败当成"可续跑"。若将来要区分 transient
   读错误，应在 `apply_selected` 里按错误类别收窄，本轮未做。

### 8.7 验证（实际执行结果）

工作树包含并行的 P1-3（`session/flush.rs`）改动，因此下面 storage 总数高于本轮前的
274；本轮新增的 storage 测试为零，新增 sync 测试为 2 个（单测 + 集成各 1）。

```text
cargo fmt --all -- --check                                         EXIT 0
cargo test -p menghunote-storage                                   288 passed / 0 failed / 1 ignored
cargo test -p menghunote-sync                                      636 passed / 0 failed / 4 ignored
cargo clippy -p menghunote-storage -p menghunote-sync \
  --all-targets -- -D warnings                                     EXIT 0
```

新增的两个测试都实际跑过（非 ignore）：

```text
cargo test -p menghunote-sync --lib verify_accepts_concurrent_local_edits_after_export
  -> 1 passed
cargo test -p menghunote-sync --test bootstrap \
  verify_failure_keeps_merged_data_and_marks_journal_failed
  -> 1 passed
```


### 8.8 未做

- 未新增/修改任何 DDL、迁移、`baseline_apply.status` 取值；
- 未改 relay、`sync-proto` wire/schema、checkpoint 签名/验证、游标连续推进；
- 当时未实现 Phase1B（gate_hold 报告、更强的批次/finish 门禁等）；**Phase1B MVP
  已在 2026-11-22 实现，见 `sync-fast-catchup-plan.md` §11；gate_hold 硬中止、manifest
  预检、时间成本交叉仍待。**
- 未 commit。



## 9. 高级评审补充口径（2026-11-21）

1. **P1-3 表格不再写“部分”**：§1 表已改为最终状态；历史收尾批次与最终行为分开引用，后续评审以本表/§9 为准。
2. **abort 的两个边界已钉住**：无 staging journal 但 marker 残留时 `abort_baseline_apply` 返回 `false` 并清 marker
   （`abort_without_journal_clears_residual_marker`）；guarded 事务失败（`AffectedGuard::exactly(3,1)` 不命中）
   整体回滚、返回 `false`、marker 保留，调用方必须重试 abort
   （`abort_guard_failure_keeps_marker_for_retry`，用 `RAISE(IGNORE)` 触发器模拟并发窗口）。单 writer 下
   第二条不可达，但语义已固定；Phase 1B 并发化后不得把 `false` 当成功。
3. **`loro_updates` 行数不是 op 数**：HeadConflict 兼容重试可能把与外部行重叠的 ops 再落一行；Loro import
   幂等，`head_vv==live_vv` 仍成立，但“一行 = 一批逻辑写入”允许 op 重叠，**不能按行数推断 op 数/去重率**。
4. **coverage 扫描上限未标定**：`COVERAGE_SCAN_MAX_PAGES = 4096` ≈ 1M 事件 → 4096 次串行页请求；
   当前探针只到 25k（98 页），真实 RTT 项是 40× 外推，没有数据。机会性追赶启用前必须补 4096 页/真实
   RTT 标定，或把上限改成可配置并默认更小。
5. **`gc_floor` 两次请求间前进**：`load_baseline` 只给 `SyncError::Object("S 低于 GC floor")`；primary/fallback
   双双越界时的最终映射未测。Phase 1B 应映射为可执行 `Paused`（“等待 relay 保留/重新 bootstrap”），
   不能落成普通 Object/空转。
6. **`verify_ns` 线程本地**：跨线程/线程池调用 `verify_imported` 时，`gate_hold` 报告若在另一线程读
   `baseline_apply_verify_ms()` 会得到 0；Phase 1B 报告必须写明“同线程读取”，或在调用线程显式回传耗时。
7. **“强制追赶失败绝不回退普通 pull”的语义**：`gc_floor > received` 时普通 pull 只会 `CursorExpired`，
   回退没有功能意义；规则实质是**不得把 `Paused` 静默成空转循环**，必须把原因（游标过期/需重试/
   等待 relay）送到 UI/调度器。
8. **auto-abort 不得发生在持闸段**：`reconcile_journal` 的换基线 auto-abort 会写库（清 journal+marker）；
   Phase 1B `catchup_integrate` 持闸期间若走“换基线”分支等于持闸写两遍。设计上应显式禁止，并把
   marker/换基线映射成 `Paused` 理由，不在持闸内自动 abort 重试。

9. **M4 门禁已补（2026-11-21）**：结构文档 local_extra 生产路径
   (`structure_doc_local_extra_merges_local_and_baseline_nodes`)、SyncContext 重建
   (`flush_rebuild_keeps_sync_intent_seq_at_new_head_plus_one`)、普通/重建后 Retryable
   (`flush_retryable_write_failure_keeps_prefix_then_recovers`、
   `flush_rebuild_retryable_write_failure_keeps_prefix_then_recovers`)、二次 HeadConflict
   (`flush_reports_unrecoverable_on_second_head_conflict`)、换基线 auto-abort/rotation 拒绝
   (`reconcile_journal_auto_aborts_bootstrap_staging_and_clears_marker`、
   `reconcile_journal_refuses_rotation_staging_without_abort`)、损坏 marker
   (`corrupt_marker_is_fail_closed_and_abort_does_not_silently_succeed`)。
   head 回退分支无法用公开接口确定性构造（head_seq 只前进），已用二次冲突覆盖可构造方向。

10. **M3 标定已完成（2026-11-21）**：`large_doc_per_page_replay_cost` 测 1 MiB
    单条大 insert `load_body_doc` ≈1.975 ms（复跑 1.996），256 条 4 KiB 增量 ≈5.965 ms（复跑 5.322）；
    `checkpoint_validate_scan_cost` 真跑 S=1,048,576（4096 页，1 run），FakeRelay
    夹具 12.8 s（复跑 12.2 s）/峰值 RSS 202 MiB，visitor ≈33 ms。结论：per-page replay 是毫秒级，
    §10.4 的“~2 s/篇”折算不成立；45 s@20MiB 是 verify/attached merge，不是纯 replay。
    4096 页真实成本仍需按 `4096 × RTT + 33 ms` 标定，FakeRelay 不能外推；阈值未改。


11. **第二轮评审 P1 修补（2026-11-21）**：
    - `finish_baseline_apply` 的 marker 检查移到 `load_journal` 之前：无 journal 但残留/损坏
      marker 时不再先返回 `NotFound`；测试
      `finish_without_journal_rejects_residual_marker_before_not_found`。
    - `flush.rs` 将 `StorageError::Corrupt` 分类为永久失败，首次写和重建后重试都直接
      `Unrecoverable`，不再落无限 `Retryable`；测试
      `flush_treats_permanent_corrupt_as_unrecoverable_not_retryable`。
    - `baseline_apply` 的 `head_seq.saturating_add(1)` 改 `checked_add(1)`；现有 schema 的
      head_seq 来自 SQLite i64，checked_add 分支不可达，真正兜底是随后的 `i64_of(seq)`
      （返回 `Corrupt`）。保留 checked_add 是防御未来放宽到 u64，**不能**把它报告成
      “已验证可达的溢出修复”。
    - `bootstrap::finalize` 与 `validate_remote` 共用 `validate_pin_shape`：`control_hash`
      无 `control_rev` 在 finalize 也拒绝；finalize 补 `stored.highwater > state.head_seq`
      的 rollback 检查；测试 `pin_shape_rejects_hash_without_rev_on_both_paths`。
    - 删除 `session_flush` DurableAck 测试里恒真的 `COUNT(*).len()==1` 死断言，保留真实
      `count()==2` 断言。
    - `exactly(3,1)` 的语义已确认：`(index, rows)`，第 3 条 staging journal DELETE 必须
      恰好 1 行；其余 DELETE 不设守卫；`abort_guard_failure_keeps_marker_for_retry` 已覆盖
      guard 失败整体回滚 + marker 保留。不是缺陷。
    - `SyncContext.key_epoch` 的 rotation 衔接已在 App 层存在（`shell.rs` 在 rotation 成功后
      先更新 `self.sync` 再重建活动视图），无需新增 storage API；但本轮未补跨 crate 接线测试。
    - 不可恢复 `CloseReport.flush_error` 已有测试；**UI 阻断关闭/切换 + 导出/复制 live 正文
      → 重开**仍未接。
    - detached 矩阵仍缺：baseline VV 含本地未知 peer、`from` 超前 doc VV、重复 import、
      多 peer 物理乱序、大文档 detached merge；结构 verify 对并发删除的产品语义仍未定。
    - 4096+1 页超限失败、真实 relay RTT、`unrecoverable && pending_len==0` 无 ACK 仍缺测试。


12. **第三轮评审 P1/P2 修补（2026-11-21）**：
    - `SessionDriver::close` 在 `session.flush_is_unrecoverable()` 时立即 break，不再等满
      timeout 才返回 `CloseReport.flush_error`；测试把 timeout 从 0 改为 5s 并断言
      `elapsed < 1s`。
    - `is_permanent_corrupt` 改为**正向白名单** `is_retryable_write_error`：只有
      `Io`/`Sqlite`/`SnapshotBusy`/`WriteBackpressure` 可重试；`Corrupt`、`Loro`、
      `LoroEncode`、`Coord`、schema 漂移、通道关闭等一律 fail-closed 到“需要重开文档”，
      不再形成永久 Retryable。
    - `apply_selected` 的 `mark_baseline_verify_failed` 写失败不再用 marker 错误覆盖原始
      verify 错误：返回带根因 + marker 失败 + 必须 abort 的 `StorageError::Corrupt`。
    - 结构 verify 的并发删除语义已按当前实现定案：`StructureDoc::snapshot()` 返回**全量
      节点**（含 `deleted`），删除是 grow-only meta 事件而不是 `LoroTree::delete`；因此
      本地并发删除基线笔记不会让 `merged_notes` 缺项，verify 不会误写 marker。产品语义是
      “删笔记不删正文，恢复仍可用”；`verify_imported` 已加注释，结构侧新增
      `deleted_note_remains_in_snapshot_with_note_identity` 钉住“deleted 节点仍保留
      note/body 身份”。若未来改成物理删节点，必须重新定义 verify。
    - `auto_rebuilt` 粒度仍是每条 flush 命令一次，不是每批一次；慢盘多批场景可能保守地
      进入不可恢复，已在 `flush.rs` 注释写明，未改行为。
    - UI 的关闭/切换阻断 + “导出/复制 live 正文 → 重开”仍未接；detached 五项、4096+1 页、
      真实 RTT、`unrecoverable && pending_len==0` 无 ACK 仍未补。


13. **第四轮评审 P1/P2 修补（2026-11-21）**：
    - `Sqlite` 不再整类可重试：只有 `DatabaseBusy`/`DatabaseLocked` 进入
      `is_retryable_write_error`；`ConstraintViolation`/`ReadOnly`/`CannotOpen`/
      `DatabaseCorrupt`/`NotFound` 等永久错误直接 `Unrecoverable`。新增
      `flush_treats_permanent_sqlite_error_as_unrecoverable` 与分类单测
      `retryable_write_error_is_a_positive_whitelist`。
    - 普通写失败的“可重试”集成用例改用**真实外部写锁**产生 SQLITE_BUSY，不再用
      `RAISE(ABORT)`（它实际是 `ConstraintViolation`，现在按永久错误处理）。原
      “重建后重试 Retryable”集成用例因无法用真实 BUSY 精确卡在重建后而移除；
      分类由单测覆盖，**RebuildOutcome::Retryable 的直接集成覆盖暂时缺一条**。
    - `apply_selected` 的 marker 写失败改报 `StorageError::SyncConflict`（journal 仍可
      续跑，不是数据损坏），保留原始 verify 根因。
    - `read_meta_array` 长度非法改报 `SyncError::LocalCorrupt`，与 pin 结构校验统一；
      新增 `read_meta_array_rejects_wrong_length_as_local_corrupt`。
    - `finalize` 在“已有进度、不改写 cursor”时也对 `sync.relay_cursor` 做 CAS 自写回，
      防止读完 cursor 后被并发改写而静默放过；暂无专门的并发注入用例。
    - 未处理：`state_from_journal` 的 `finishable` 在计数相等时仍做 O(N) 逐项校验
      （Phase1B 高频状态查询需缓存/近似）；结构 verify deleted 语义已定案但依赖当前
      snapshot 实现；UI 救济、detached 五项、真实 RTT、4096+1 页、pending_len==0 无 ACK
      仍未补。


14. **实现收口（2026-11-21，非 RTT）**：
    - detached 矩阵补齐 5 项：本地未知 peer、`from` 超前本地 VV、同一 update 重复 import、
      多 peer 物理乱序、大文档 merge；`body_doc_detached_oplog` 现在 **16** 用例。
    - `coverage_scan_rejects_more_than_max_pages`：S=4096×256+1，validate 用满 4096 页后
      返回 `未证明覆盖到 S（页数超过上限）`；`PageIndexRelay` 只替换 `log` 为 partition_point
      切片，避免 FakeRelay 的 O(S²) 夹具。生产文案仍不含数字 4096。
    - `state_from_journal` 的 `finishable` 改为 O(1) 计数近似；逐项 applied/摘要证明只在
      `finish_baseline_apply` 做，读状态不再 O(N)。
    - flusher 在 `unrecoverable` 时即使 `pending_len==0` 也不补发确认 ACK（防御分支，无专门
      注入用例）。
    - `RebuildOutcome::Retryable` 的分类抽成 `rebuild_write_error_outcome` 并单测 BUSY /
      Constraint；完整 flusher 路径的 Retryable 仍无直接集成用例（真实 BUSY 难以精确卡在
      重建后）。
    - UI：切换前 `CloseReport.needs_reopen`/`flush_error` 阻断并复制 `source_text`；关窗前
      `last_write_error` 不可恢复则复制正文并通过 `on_window_should_close` 返回 false 取消关窗。
      没有单独的“导出”按钮，救济是自动复制到剪贴板 + 保留现场。
    - 不再把真实 RTT 当本地可测项；机会性追赶启用前必须真实 relay 标定。
    - 仍未补：`finalize` cursor CAS 的并发注入用例、`unrecoverable && pending_len==0` 的
      专门测试、Rebuild Retryable 的完整路径集成、UI 真机观感验证。


15. **Phase 1B MVP（2026-11-22，独立于本硬化清单）**：
    - `bootstrap::prepare` 不写库、`bootstrap::integrate` 写库；`BootstrapAdvancement::CatchUp`
      在 `integrated>=S` 后前移 cursor。
    - `catchup` 模块：Mandatory（gc_floor>received，不受 16MiB 上限）与 Opportunistic
      （S-received>=25k 且快照估算<=16MiB）；失败强制 `Paused(CatchupFailed)`，不回退普通 pull。
    - `prune_covered_inbox` 只在 `integrated>=S` 的同一 SQL 证明下删除；两阶段
      `sync_prepare/sync_integrate_with_catchup` 与完整 `sync_once` 已接入。
    - 测试：catchup 决策 3、bootstrap prepare 零写/integrate cursor+本地 op、inbox watermark。
    - 已知限制：机会性未做 manifest 预检（可能先下载后丢弃）、无 1.5× 时间成本交叉、
      `gate_hold_ms` 未硬中止、真实 RTT 未标定、UI 未单独显示追赶阶段。

`````

## `sync-phase0-metrics.md` §8.7.7–8.7.8

`````markdown
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

`````

---

# 附录 B：改动代码与关键测试


## `crates/sync/src/catchup.rs`

`````rust
//! Phase 1B：checkpoint 快速追赶（双模式 + prepare/integrate 拆分）。
//!
//! 约束见 `docs/notes/sync-fast-catchup-plan.md` §9：
//! - **Mandatory**：`gc_floor > received_seq`，普通 pull 必然 `CursorExpired`；不受
//!   机会性快照上限拦截，失败进入明确 `Paused`，禁止回退普通 pull。
//! - **Opportunistic**：`S - received_seq >= N_min` 且快照密文估算不超过 16 MiB；
//!   任一不满足走普通聚合 pull。
//! - 网络/下载/验签在 [`prepare`]（不持写闸）；写库/水位/游标在 [`integrate`]（调用方
//!   持外部提交闸）。本模块不碰 wire/schema。

use menghunote_storage::{sync_cursor_state, Vault};

use crate::bootstrap::{self, BootstrapPins, PreparedBootstrap};
use crate::error::SyncError;
use crate::relay::{RelayClient, RelayState};

/// 机会性追赶的最小事件差（plan §9.1）。
pub const DEFAULT_N_MIN: u64 = 25_000;
/// 机会性追赶的快照密文硬上限（plan §9.1）。
pub const DEFAULT_OPPORTUNISTIC_MAX_SNAPSHOT_BYTES: u64 = 16 * 1024 * 1024;
/// 明文快照 → 密文的保守安全系数（信封/分片/编码开销）。
const SNAPSHOT_CIPHERTEXT_FACTOR: u64 = 2;

/// 追赶模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatchUpMode {
    /// 游标已过期，必须用 checkpoint 才能继续。
    Mandatory,
    /// 落后足够多且快照成本可接受，主动追赶。
    Opportunistic,
}

/// 追赶阈值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CatchUpConfig {
    /// 机会性追赶的最小事件差。
    pub n_min: u64,
    /// 机会性追赶允许的快照密文字节上限。
    pub opportunistic_max_snapshot_bytes: u64,
}

impl Default for CatchUpConfig {
    fn default() -> Self {
        Self {
            n_min: DEFAULT_N_MIN,
            opportunistic_max_snapshot_bytes: DEFAULT_OPPORTUNISTIC_MAX_SNAPSHOT_BYTES,
        }
    }
}

/// 一次追赶决策（只读，不含明文）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CatchUpDecision {
    /// 强制或机会性。
    pub mode: CatchUpMode,
    /// 实际选中 checkpoint 的覆盖水位 S。
    pub checkpoint_s: u64,
    /// `S - received_seq`（饱和）。
    pub gap_events: u64,
    /// 快照明文字节估算（已乘安全系数）。
    pub estimated_snapshot_bytes: u64,
}

/// 是否值得进入网络准备（避免正常近端同步无条件下载整份快照）。
///
/// 强制条件用 `gc_floor > received`；机会性先用 `head_seq - received` 做上界，
/// 真正的 `S - received` 与快照上限在 [`prepare`] 拿到实际 checkpoint 后再判。
#[must_use]
pub fn should_attempt(state: &RelayState, local_received_seq: u64, config: &CatchUpConfig) -> bool {
    state.gc_floor > local_received_seq
        || state.head_seq.saturating_sub(local_received_seq) >= config.n_min
}

/// 决策：只依赖 relay state / 本地 received / 实际选中 checkpoint S / 快照估算。
///
/// 强制模式不受快照上限拦截；机会性必须同时满足事件差和快照上限。
#[must_use]
pub fn decide(
    state: &RelayState,
    local_received_seq: u64,
    checkpoint_s: u64,
    estimated_snapshot_bytes: u64,
    config: &CatchUpConfig,
) -> Option<CatchUpDecision> {
    let gap_events = checkpoint_s.saturating_sub(local_received_seq);
    if state.gc_floor > local_received_seq {
        return Some(CatchUpDecision {
            mode: CatchUpMode::Mandatory,
            checkpoint_s,
            gap_events,
            estimated_snapshot_bytes,
        });
    }
    if gap_events >= config.n_min
        && estimated_snapshot_bytes <= config.opportunistic_max_snapshot_bytes
    {
        return Some(CatchUpDecision {
            mode: CatchUpMode::Opportunistic,
            checkpoint_s,
            gap_events,
            estimated_snapshot_bytes,
        });
    }
    None
}

/// 已下载/校验、尚未写库的追赶候选。
#[derive(Debug)]
pub struct CatchUpPrepared {
    prepared: PreparedBootstrap,
    decision: CatchUpDecision,
}

impl CatchUpPrepared {
    /// 决策。
    #[must_use]
    pub fn decision(&self) -> CatchUpDecision {
        self.decision
    }

    /// 实际选中 checkpoint 的 S。
    #[must_use]
    pub fn checkpoint_s(&self) -> u64 {
        self.prepared.covered_seq()
    }

    /// 实际选中 checkpoint 的 cursor。
    #[must_use]
    pub fn checkpoint_cursor(&self) -> &str {
        self.prepared.checkpoint_cursor()
    }

    /// 是否使用 fallback。
    #[must_use]
    pub fn used_fallback(&self) -> bool {
        self.prepared.used_fallback()
    }

    /// 已选中文档数。
    #[must_use]
    pub fn doc_count(&self) -> usize {
        self.prepared.doc_count()
    }

    /// 已选中资产数。
    #[must_use]
    pub fn asset_count(&self) -> usize {
        self.prepared.asset_count()
    }
}

/// 追赶写库报告（不含明文）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatchUpReport {
    /// 模式。
    pub mode: CatchUpMode,
    /// 实际选中 checkpoint 的 S。
    pub checkpoint_s: u64,
    /// 是否使用 fallback。
    pub used_fallback: bool,
    /// 导入文档 id（用于追赶后物化；不含明文）。
    pub docs: Vec<String>,
    /// 导入资产数。
    pub assets: usize,
    /// 清理的 inbox 行数（仅在 integrated>=S 后）。
    pub pruned_inbox: usize,
    /// 写闸内耗时（毫秒）；只记录，不自动中止。
    pub gate_hold_ms: u64,
}

/// 网络准备：选 checkpoint、下载/验签/校验，**不写本地库**。
///
/// 返回 `None` 表示不需要追赶（普通 pull 更便宜）；调用方应继续普通 pull。
pub fn prepare<C: RelayClient>(
    vault: &Vault,
    client: &mut C,
    vault_id: [u8; 16],
    peer_id: u64,
    root: &crate::keys::RootSecretV1,
    pins: &BootstrapPins,
    config: &CatchUpConfig,
) -> Result<Option<CatchUpPrepared>, SyncError> {
    let local = sync_cursor_state(vault)?;
    let state = client.state()?;
    if state.vault_id != vault_id {
        return Err(SyncError::RelayWire("state.vault_id 与本地不一致"));
    }
    if let Some(log_id) = local.log_id {
        if log_id != state.log_id {
            return Err(SyncError::Paused(crate::relay::PauseReason::LogIdChanged));
        }
    }
    if state.epoch != root.key_epoch {
        return Err(SyncError::Paused(crate::relay::PauseReason::EpochMismatch));
    }

    // 1) 只拉 manifest 预检：先在真正下载快照之前用实际选中 checkpoint 的 S/明文字节
    //    判断机会性是否值得；不满足直接回普通 pull，不浪费带宽。
    let probe = match bootstrap::probe(client, vault_id, root, pins) {
        Ok(probe) => probe,
        Err(error) => {
            if state.gc_floor > local.received_seq {
                eprintln!("[sync] mandatory catchup probe failed: {error}");
                return Err(SyncError::Paused(crate::relay::PauseReason::CatchupFailed));
            }
            eprintln!("[sync] opportunistic catchup probe failed, fallback to pull: {error}");
            return Ok(None);
        }
    };
    let probe_estimated = probe
        .estimated_plain_bytes()
        .saturating_mul(SNAPSHOT_CIPHERTEXT_FACTOR);
    if decide(
        &state,
        local.received_seq,
        probe.covered_seq(),
        probe_estimated,
        config,
    )
    .is_none()
    {
        return Ok(None);
    }

    // 2) 预检通过后才下载 snapshot/缺失资产。fallback 可能和预检不同，实际 S/字节
    //    必须在下载完成后再判一次；mandatory 仍不受机会性上限拦截。
    let prepared = match bootstrap::prepare(vault, client, vault_id, peer_id, root, pins) {
        Ok(prepared) => prepared,
        Err(error) => {
            if state.gc_floor > local.received_seq {
                eprintln!("[sync] mandatory catchup prepare failed: {error}");
                return Err(SyncError::Paused(crate::relay::PauseReason::CatchupFailed));
            }
            eprintln!("[sync] opportunistic catchup prepare failed, fallback to pull: {error}");
            return Ok(None);
        }
    };
    let actual_estimated = prepared
        .estimated_snapshot_bytes()
        .saturating_mul(SNAPSHOT_CIPHERTEXT_FACTOR);
    let Some(actual_decision) = decide(
        &state,
        local.received_seq,
        prepared.covered_seq(),
        actual_estimated,
        config,
    ) else {
        // 预检主基线可接受，实际 fallback 超限：丢弃已下载快照，走普通 pull。
        return Ok(None);
    };
    Ok(Some(CatchUpPrepared {
        prepared,
        decision: actual_decision,
    }))
}

/// 写库：baseline_apply → finish/游标 CAS → 清理已覆盖 inbox。
///
/// 调用方必须持有外部提交写闸；网络已在 [`prepare`] 完成。
pub fn integrate(
    vault: &Vault,
    peer_id: u64,
    prepared: &CatchUpPrepared,
) -> Result<CatchUpReport, SyncError> {
    let started = std::time::Instant::now();
    let report = bootstrap::integrate(
        vault,
        peer_id,
        &prepared.prepared,
        bootstrap::BootstrapAdvancement::CatchUp,
        None,
    )?;
    let cursor = sync_cursor_state(vault)?;
    if cursor.integrated_seq < report.covered_seq {
        return Err(SyncError::LocalCorrupt(
            "catchup integrate 返回成功但 integrated_seq 未覆盖 checkpoint S",
        ));
    }
    let pruned = menghunote_storage::prune_covered_inbox(
        vault,
        &prepared.prepared.log_id(),
        report.covered_seq,
    )?;
    let gate_hold_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    if gate_hold_ms > 3_000 {
        eprintln!("[sync] catchup gate_hold_ms={gate_hold_ms} 超过 3s 预算（只记录，未自动中止）");
    }
    Ok(CatchUpReport {
        mode: prepared.decision.mode,
        checkpoint_s: report.covered_seq,
        used_fallback: report.used_fallback,
        docs: report.docs.into_iter().map(|(doc_id, _)| doc_id).collect(),
        assets: report.assets,
        pruned_inbox: pruned,
        gate_hold_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(head_seq: u64, gc_floor: u64) -> RelayState {
        RelayState {
            vault_id: [1; 16],
            log_id: [2; 16],
            instance_id: [3; 16],
            epoch: 1,
            control_rev: 0,
            auth_rev: 0,
            head_seq,
            gc_floor,
            current_rotation_id: None,
        }
    }

    #[test]
    fn mandatory_wins_even_when_snapshot_exceeds_opportunistic_cap() {
        let decision = decide(
            &state(100_000, 11_000),
            10_000,
            90_000,
            u64::MAX,
            &CatchUpConfig::default(),
        )
        .expect("过期游标必须强制追赶");
        assert_eq!(decision.mode, CatchUpMode::Mandatory);
        assert_eq!(decision.gap_events, 80_000);
    }

    #[test]
    fn opportunistic_requires_n_min_and_snapshot_cap() {
        let config = CatchUpConfig::default();
        let below_min = decide(&state(100_000, 0), 99_000, 99_000, 1, &config);
        assert!(below_min.is_none(), "事件差不足不得机会性追赶");

        let ok = decide(
            &state(100_000, 0),
            70_000,
            95_000,
            16 * 1024 * 1024,
            &config,
        )
        .expect("25k 且快照正好 16MiB 应允许");
        assert_eq!(ok.mode, CatchUpMode::Opportunistic);

        let too_big = decide(
            &state(100_000, 0),
            70_000,
            95_000,
            16 * 1024 * 1024 + 1,
            &config,
        );
        assert!(too_big.is_none(), "超过机会性快照上限必须回退普通 pull");
    }

    #[test]
    fn should_attempt_only_for_expired_or_large_gap() {
        let config = CatchUpConfig::default();
        assert!(should_attempt(&state(100_000, 0), 75_000, &config));
        assert!(!should_attempt(&state(100_000, 0), 90_000, &config));
        assert!(should_attempt(&state(100_000, 20_000), 10_000, &config));
    }
}

`````


## `crates/sync/src/bootstrap.rs`

`````rust
//! 新设备 bootstrap（S7-18；ADR §3.7/§4/§7/§25.3；评审 0002 P1-4）。
//!
//! 本模块先落"数据恢复"这一段：给定已解出的 root（由槽解封或后续恢复流程提供），
//! `POST /bootstrap` 取两份基线 → 下 manifest/snapshot/asset → 全量校验 → 空库落库。
//!
//! **可恢复导入（P1-4）**：docs/structure/assets 的写入全部经过 storage 的
//! `baseline_apply` staging journal（`begin_baseline_apply` / `apply_baseline_batch` /
//! `apply_baseline_assets`）。下载/写入阶段任一步失败都**不**清 journal、**不**推进任何
//! 水位；下一次调用按同一份基线幂等续跑。只有全部实际导入并通过读回校验后，才在**一个**
//! SQLite 事务里推进 `cursor/received/integrated/confirmed_highwater`（CAS）。
//!
//! **读回校验失败（同步硬化 P1）**：`verify_imported` 失败时**不回滚**已 merge 的
//! Loro/资产数据（CRDT merge 幂等；回滚会丢本地编辑），而是写一条只本地可见的
//! `sync.baseline_verify_failed` 失败标记（storage `mark_baseline_verify_failed`）。
//! journal 仍是 `staging`（schema 的 status CHECK 只允许 `staging/completed`，见
//! `docs/notes/sync-hardening-20261121.md` §8），但下一次 `begin_baseline_apply` 会拒绝，
//! 必须先 `abort_baseline_apply` 清理（abort 同时删标记，已导入数据保留）。
//!
//! **闸外编辑**：verify 不与编辑互斥，判定用「导出点 VV ⊆ 当前 VV」而不是相等；只有
//! 本地没有快照之外的 op 时才逐字节比较正文（见 [`check_imported_doc`]）。
//!
//! **水位单调（P1-4）**：bootstrap 允许作用在已有库上，但禁止降低任何单调值：
//! `received/integrated/highwater` 取 `max(现有, baseline S)`；已有进度时保留现有 relay
//! 游标（不把已经消费过的位置回退）；`instance/log_id/epoch/control rev+hash` 只允许
//! 前进或不变。storage 的 `finish_baseline_apply` 会把 highwater 直接写成 baseline S，
//! 对"已有 highwater 高于 S"的库会降低水位，因此本模块在同一个事务里自行做
//! `max` 语义的 CAS 推进，随后清掉 staging（`abort_baseline_apply` 只清 journal，
//! 不删已导入的 Loro/asset 数据）。
//!
//! **实际 VV 校验（P1-4）**：每个 snapshot 都在独立的空 `LoroDoc` 里 import，比较
//! actual VV / frontier 与 manifest 声明（既不虚高也不缺 op），并校验 full-history、
//! 信封字段、app schema / min_reader；结构文档再重放出结构树，校验"结构引用的正文
//! 文档必须都在基线 manifest 里"。不只信声明。
//!
//! **fallback**：primary 任一阶段（下载/解密/摘要/VV/结构语义）失败才整体切 fallback；
//! 选中哪个基线就只用**它自己的** S / cursor；写入阶段失败不切 fallback（避免半库
//! 换 S），必须原样重试或人工 abort journal。
//!
//! **写库路径**：全部经 storage 公开 API（`BodyDoc`/`StructureWriter`/`baseline_apply`），
//! 不直连 SQLite；只有最终的水位推进用 `Vault::exec_transaction` 的单事务 CAS
//! （storage 没有提供 max 语义的 finish，且本任务写作用域不允许改 storage）。
//!
//! **已知边界**：relay `/bootstrap` 不返回完整签名 checkpoint 链；本模块只验证
//! `CheckpointCommit` 的生产者签名、manifest/refs 摘要、以及所有实际对象。新设备无旧
//! 水位，无法检测"有效签名的旧基线回滚"（ADR §10 已记录）；已有库用本地 pins 检测
//! `instance/log_id/epoch/control_rev/highwater` 的回退。

use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use loro::VersionVector;
use sha2::{Digest, Sha256};

use crate::checkpoint::{verify_asset_plaintext, AssetRef, KIND_BODY, KIND_STRUCTURE};
use crate::control_pin::BootstrapAnchorV1;
use crate::device::verify_signature;
use crate::engine::RELAY_CURSOR_META;
use crate::error::SyncError;
use crate::keys::RootSecretV1;
use crate::relay::{
    BootstrapBundle, BootstrapCheckpoint, ManagerChainEntry, RelayClient, RelayState,
};
use crate::rotation::ManagerTransitionV1;
use crate::upload::fetch_object;
use menghunote_storage::{
    abort_baseline_apply, apply_baseline_assets, apply_baseline_batch, asset_bytes,
    baseline_apply_state, begin_baseline_apply, load_body_doc, load_structure,
    mark_baseline_verify_failed, record_baseline_verify_time, sync_cursor_state, AffectedGuard,
    BaselineApplyStatus, BaselineAsset, BaselineExpectation, BodyDoc, Statement, StructureDoc,
    StructureWriter, Value, Vault, APP_DOC_SCHEMA, CODEC_VERSION, META_SYNC_CONFIRMED_HIGHWATER,
    META_SYNC_INTEGRATED_SEQ, META_SYNC_LOG_ID, META_SYNC_RECEIVED_SEQ, MIN_READER,
    STRUCTURE_DOC_ID,
};
use menghunote_sync_proto::{
    checkpoint_signing_input, refs_digest_input, CheckpointCommit, Envelope, EnvelopeHeader,
    ManifestDoc, ManifestV1, ObjectRefV1, ObjectType, SnapshotV1,
};

/// 每批写入的文档数（storage 的 baseline batch 是单事务；分批只为控制单事务大小）。
const BOOTSTRAP_DOC_BATCH: usize = 64;
/// bootstrap 写入 `baseline_apply.source_cursor` 的前缀：用于区分 bootstrap 的 staging
/// 与轮换（`rotation_apply` 传空串）的 staging，防止误清别人的 journal。
const JOURNAL_SOURCE_PREFIX: &str = "bootstrap:";

/// `meta` 键：bootstrap pin 的 relay instance id（16B，只允许相等）。
pub const META_BOOTSTRAP_INSTANCE: &str = "sync.pinned_instance";
/// `meta` 键：bootstrap pin 的 epoch（BE64，只允许前进）。
pub const META_BOOTSTRAP_EPOCH: &str = "sync.pinned_epoch";
/// `meta` 键：bootstrap pin 的 control_rev（BE64，只允许前进）。
pub const META_BOOTSTRAP_CONTROL_REV: &str = "sync.pinned_control_rev";
/// `meta` 键：bootstrap pin 的控制头摘要（32B，见 [`control_head_hash`]；同 rev 必须相等）。
pub const META_BOOTSTRAP_CONTROL_HASH: &str = "sync.pinned_control_hash";

/// bootstrap 结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapReport {
    /// 使用的 checkpoint id。
    pub checkpoint_id: [u8; 16],
    /// 覆盖水位 S。
    pub covered_seq: u64,
    /// 是否回退到 fallback。
    pub used_fallback: bool,
    /// 导入文档（structure 的文本为 None）。
    pub docs: Vec<(String, Option<String>)>,
    /// 导入资产数。
    pub assets: usize,
    /// S 对应的 relay 游标。
    pub cursor: String,
    /// 是否验证了 checkpoint 发布签名（relay 返回键 6/7 且验签通过）。
    pub signed_checkpoint: bool,
}

/// bootstrap 前本地已经可信的单调值（P1-4 "已有库保护"）。
///
/// 新设备全部为 `None`；已有设备由 [`BootstrapPins::from_vault`] 从本库 meta 读出，
/// 或由调用方用 identity（`pinned_instance/control_rev/current_epoch`）填充后调用
/// [`run_with_pins`]。bootstrap 只允许这些值前进/不变，绝不回退。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BootstrapPins {
    /// relay instance id（防换实例/静默回滚）。
    pub instance_id: Option<[u8; 16]>,
    /// relay log_id（restore 必须显式换新，不能静默归零）。
    pub log_id: Option<[u8; 16]>,
    /// 当前密钥代。
    pub epoch: Option<u64>,
    /// 已信任的管理控制链版本。
    pub control_rev: Option<u64>,
    /// 已信任的控制头摘要（见 [`control_head_hash`]）。
    pub control_hash: Option<[u8; 32]>,
    /// 已确认的 relay 水位。
    pub highwater: Option<u64>,
}

impl BootstrapPins {
    /// 从本库持久化的 pin（`sync.pinned_*`）与同步游标读出已有可信值。
    ///
    /// pin 缺失（新设备/首次 bootstrap）返回 `None`，不报错；pin 存在但长度非法按
    /// 本地损坏处理（不能静默忽略，否则等于绕过单调保护）。
    pub fn from_vault(vault: &Vault) -> Result<Self, SyncError> {
        let cursor = sync_cursor_state(vault)?;
        Ok(Self {
            instance_id: read_meta_array::<16>(vault, META_BOOTSTRAP_INSTANCE)?,
            log_id: cursor.log_id,
            epoch: read_meta_u64(vault, META_BOOTSTRAP_EPOCH)?,
            control_rev: read_meta_u64(vault, META_BOOTSTRAP_CONTROL_REV)?,
            control_hash: read_meta_array::<32>(vault, META_BOOTSTRAP_CONTROL_HASH)?,
            highwater: cursor.confirmed_highwater,
        })
    }

    /// 把 `sync.pinned_*` 读成一个**带自己 rev/epoch/log** 的 bootstrap 锚（ADR 0013 §2）。
    ///
    /// 语义：
    /// - `control_hash` 与 `control_rev` 必须成对：只有 hash 没有 rev 是本地损坏
    ///   （既有行为）；只有 rev 没有 hash 时返回 `None`——rev 只是下界，不构成摘要锚；
    /// - hash 存在时 `epoch` / `log_id` 必须齐全：历史视图复算需要锚**自己的**坐标，
    ///   缺任一项都不能把摘要当"同 rev 校验"用（否则旧锚会在 rev 前进后被静默忽略）。
    ///   这是 fail-closed：缺字段报 [`SyncError::LocalCorrupt`]，**不**降级成"没有锚"。
    pub fn control_anchor(&self) -> Result<Option<BootstrapAnchorV1>, SyncError> {
        let Some(hash) = self.control_hash else {
            // 只有 rev 下界（历史实例/未写摘要）不构成摘要锚；单调性仍由 control_rev 保证。
            return Ok(None);
        };
        let rev = self.control_rev.ok_or(SyncError::LocalCorrupt(
            "bootstrap pins 有 control_hash 但缺 control_rev",
        ))?;
        let epoch = self.epoch.ok_or(SyncError::LocalCorrupt(
            "bootstrap pins 有 control_hash 但缺 epoch（无法按历史视图复算摘要）",
        ))?;
        let log_id = self.log_id.ok_or(SyncError::LocalCorrupt(
            "bootstrap pins 有 control_hash 但缺 log_id（无法按历史视图复算摘要）",
        ))?;
        Ok(Some(BootstrapAnchorV1 {
            rev,
            hash,
            epoch,
            log_id,
        }))
    }
}

/// 测试用故障注入点（生产调用路径不会构造）。
///
/// 语义等价于"进程在崩溃点直接消失"：不再执行后续步骤、不清理 journal、不推进水位。
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootstrapFault {
    /// 正文批次写完后崩溃（structure / assets / 水位都未写）。
    AfterBodies,
    /// structure 文档已写入、结构投影尚未执行时崩溃。
    BeforeStructureProjection,
    /// 全部 assets 已写入、读回校验与水位推进之前崩溃。
    AfterAssets,
    /// 水位事务读取当前状态之后、提交之前，模拟另一个写者改写了 relay 游标，
    /// 迫使 CAS 失败（整笔事务回滚）。
    CursorCasConflict,
    /// 读回校验阶段注入一次「本地 VV 未覆盖快照」失败：用于验证失败标记、
    /// `begin` 拒绝续跑与 `abort` 清理路径（不改数据库里的真实数据）。
    VerifyVvNotCovered,
}

/// 故障注入伪造的 peer id（只在校验失败测试路径出现；不影响真实数据）。
const VERIFY_FAULT_PEER: u64 = 0x0BAD_5EED;

#[derive(Debug)]
struct VerifiedDoc {
    doc_id: String,
    kind: u64,
    snapshot: SnapshotV1,
    actual_vv: VersionVector,
    /// 正文文本（structure 为 `None`）。
    text: Option<String>,
}

#[derive(Debug)]
struct VerifiedAsset {
    object_id: [u8; 32],
    real_sha256: [u8; 32],
    byte_len: u64,
    media_type: String,
    data: Vec<u8>,
}

/// 选中的基线：manifest/docs/assets + 基线本身 + 结构语义。
#[derive(Debug)]
struct SelectedBaseline {
    checkpoint: BootstrapCheckpoint,
    used_fallback: bool,
    docs: Vec<VerifiedDoc>,
    assets: Vec<VerifiedAsset>,
    /// structure 快照声明的 `note_id -> body_doc_id`（用于导入后的结构投影校验）。
    structure_notes: BTreeMap<String, String>,
}

/// bootstrap 写库阶段对游标/水位的推进策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootstrapAdvancement {
    /// 新设备/普通 bootstrap：只允许前进，已有进度时保留旧游标（max 语义）。
    MaxOnly,
    /// Phase 1B 追赶：成功导入并证明 `integrated >= S` 后，把 relay cursor 前移到
    /// 实际选中 checkpoint 的 cursor（S 覆盖已消费前缀）。
    CatchUp,
}

/// `catchup_prepare` 的结果：网络阶段已完成、**尚未写本地库**的基线。
///
/// 生命周期只到 `catchup_integrate`；内含快照/资产明文，不能持久化到磁盘/日志。
#[derive(Debug)]
pub struct PreparedBootstrap {
    selected: SelectedBaseline,
    state: RelayState,
    chain: Vec<ManagerChainEntry>,
    control_hash: [u8; 32],
    pins: BootstrapPins,
}

impl PreparedBootstrap {
    /// 实际选中的 checkpoint 覆盖水位 S（fallback 时是 fallback 的 S）。
    #[must_use]
    pub fn covered_seq(&self) -> u64 {
        self.selected.checkpoint.covered_seq
    }

    /// 实际选中的 checkpoint 游标（fallback 时是 fallback 的 cursor）。
    #[must_use]
    pub fn checkpoint_cursor(&self) -> &str {
        &self.selected.checkpoint.cursor
    }

    /// 是否使用了 fallback。
    #[must_use]
    pub fn used_fallback(&self) -> bool {
        self.selected.used_fallback
    }

    /// 已选中的文档/资产数量（用于决策/报告，不暴露明文）。
    #[must_use]
    pub fn doc_count(&self) -> usize {
        self.selected.docs.len()
    }

    /// 已选中的资产数量。
    #[must_use]
    pub fn asset_count(&self) -> usize {
        self.selected.assets.len()
    }

    /// 已选中的文档 id（用于追赶后物化/报告；不含明文）。
    #[must_use]
    pub fn doc_ids(&self) -> Vec<String> {
        self.selected
            .docs
            .iter()
            .map(|doc| doc.doc_id.clone())
            .collect()
    }

    /// 当前 relay log incarnation（追赶后清理 inbox 必须绑定它）。
    #[must_use]
    pub fn log_id(&self) -> [u8; 16] {
        self.state.log_id
    }

    /// 快照明文字节估算（文档 Loro snapshot + 资产字节）；调用方按密文上限再乘
    /// 安全系数，不能把它当成精确的 wire 字节数。
    #[must_use]
    pub fn estimated_snapshot_bytes(&self) -> u64 {
        let docs = self
            .selected
            .docs
            .iter()
            .map(|doc| u64::try_from(doc.snapshot.loro_snapshot_bytes.len()).unwrap_or(u64::MAX))
            .fold(0u64, u64::saturating_add);
        let assets = self
            .selected
            .assets
            .iter()
            .map(|asset| asset.byte_len)
            .fold(0u64, u64::saturating_add);
        docs.saturating_add(assets)
    }
}

/// 只下载 manifest 的预检结果（不下载 snapshot/asset，不写库）。
///
/// Phase 1B 机会性追赶先用它读实际选中 checkpoint 的 S / 明文字节估算；避免为了
/// 一个十几 MB 的快照先下载整份数据再回退。
#[derive(Debug)]
pub struct BootstrapProbe {
    checkpoint: BootstrapCheckpoint,
    used_fallback: bool,
    estimated_plain_bytes: u64,
}

impl BootstrapProbe {
    /// 实际选中 checkpoint 的 S。
    #[must_use]
    pub fn covered_seq(&self) -> u64 {
        self.checkpoint.covered_seq
    }

    /// 实际选中 checkpoint 的 cursor。
    #[must_use]
    pub fn checkpoint_cursor(&self) -> &str {
        &self.checkpoint.cursor
    }

    /// 是否使用 fallback。
    #[must_use]
    pub fn used_fallback(&self) -> bool {
        self.used_fallback
    }

    /// manifest 声明的明文字节（文档 + 资产）；不是 wire 密文字节。
    #[must_use]
    pub fn estimated_plain_bytes(&self) -> u64 {
        self.estimated_plain_bytes
    }
}

/// 只拉 manifest 的预检；选择逻辑与 `prepare` 相同，但不下载 snapshot/asset。
pub fn probe<C: RelayClient>(
    client: &mut C,
    vault_id: [u8; 16],
    root: &RootSecretV1,
    pins: &BootstrapPins,
) -> Result<BootstrapProbe, SyncError> {
    if root.vault_id != vault_id {
        return Err(SyncError::Object("bootstrap root 与 vault 不符"));
    }
    let bundle = client.bootstrap()?;
    let state = client.state()?;
    let chain = client.management_chain()?;
    let control_hash = control_head_hash(&state, &chain);
    validate_remote(vault_id, root, pins, &bundle, &state, &chain, &control_hash)?;

    let mut last_error: Option<SyncError> = None;
    for (checkpoint, used_fallback) in [
        (bundle.primary.clone(), false),
        (bundle.fallback.clone(), true),
    ] {
        let result = (|| {
            if checkpoint.epoch != state.epoch || checkpoint.epoch != bundle.epoch {
                return Err(SyncError::Object("bootstrap 基线 epoch 与 relay 不一致"));
            }
            if checkpoint.covered_seq > bundle.head_seq || checkpoint.covered_seq > state.head_seq {
                return Err(SyncError::Object("bootstrap 基线 S 超过 relay head"));
            }
            if checkpoint.covered_seq < bundle.gc_floor || checkpoint.covered_seq < state.gc_floor {
                return Err(SyncError::Object("bootstrap 基线 S 低于 relay GC floor"));
            }
            if checkpoint.cursor.is_empty() {
                return Err(SyncError::Object("bootstrap 基线缺 relay 游标"));
            }
            let (manifest, digest) = fetch_manifest(client, root, &checkpoint)?;
            verify_checkpoint_commit(client, &checkpoint, &manifest, &digest)?;
            verify_manifest(&manifest, vault_id, &checkpoint, state.epoch)?;
            let docs = manifest
                .docs
                .iter()
                .map(|doc| doc.total_plain_len)
                .fold(0u64, u64::saturating_add);
            let assets = manifest
                .assets
                .iter()
                .map(|asset| asset.byte_len)
                .fold(0u64, u64::saturating_add);
            Ok(BootstrapProbe {
                checkpoint,
                used_fallback,
                estimated_plain_bytes: docs.saturating_add(assets),
            })
        })();
        match result {
            Ok(probe) => return Ok(probe),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.expect("bootstrap 至少尝试过一个基线"))
}

/// 用已解出的 root 执行数据 bootstrap；本地 pins 从本库 meta 读取。
pub fn run<C: RelayClient>(
    vault: &Vault,
    client: &mut C,
    vault_id: [u8; 16],
    peer_id: u64,
    root: &RootSecretV1,
) -> Result<BootstrapReport, SyncError> {
    let pins = BootstrapPins::from_vault(vault)?;
    run_with_pins(vault, client, vault_id, peer_id, root, &pins)
}

/// 用调用方提供的本地 pins 执行 bootstrap（已有设备传 identity 的 pin）。
pub fn run_with_pins<C: RelayClient>(
    vault: &Vault,
    client: &mut C,
    vault_id: [u8; 16],
    peer_id: u64,
    root: &RootSecretV1,
    pins: &BootstrapPins,
) -> Result<BootstrapReport, SyncError> {
    run_inner(vault, client, vault_id, peer_id, root, pins, None)
}

/// 测试入口：在指定阶段注入崩溃/并发冲突（见 [`BootstrapFault`]）。
#[doc(hidden)]
pub fn run_with_fault<C: RelayClient>(
    vault: &Vault,
    client: &mut C,
    vault_id: [u8; 16],
    peer_id: u64,
    root: &RootSecretV1,
    fault: BootstrapFault,
) -> Result<BootstrapReport, SyncError> {
    let pins = BootstrapPins::from_vault(vault)?;
    run_inner(vault, client, vault_id, peer_id, root, &pins, Some(fault))
}

/// 网络准备阶段：选 checkpoint、下载/解密/验签/校验，**不写本地库**。
///
/// Phase 1B 的 `catchup_prepare` 直接复用它；不持写闸。
pub fn prepare<C: RelayClient>(
    vault: &Vault,
    client: &mut C,
    vault_id: [u8; 16],
    peer_id: u64,
    root: &RootSecretV1,
    pins: &BootstrapPins,
) -> Result<PreparedBootstrap, SyncError> {
    if root.vault_id != vault_id {
        return Err(SyncError::Object("bootstrap root 与 vault 不符"));
    }
    let bundle = client.bootstrap()?;
    let state = client.state()?;
    let chain = client.management_chain()?;
    let control_hash = control_head_hash(&state, &chain);
    validate_remote(vault_id, root, pins, &bundle, &state, &chain, &control_hash)?;

    // 选中哪个基线，就只用它的 S/cursor/manifest/docs；primary 任一阶段失败才整体切 fallback。
    // 所有下载/解密/摘要/VV/结构语义校验都在写库之前完成。
    let mut selected: Option<SelectedBaseline> = None;
    let mut last_error: Option<SyncError> = None;
    for (checkpoint, used_fallback) in [
        (bundle.primary.clone(), false),
        (bundle.fallback.clone(), true),
    ] {
        match load_baseline(
            vault,
            client,
            root,
            vault_id,
            peer_id,
            &checkpoint,
            &bundle,
            &state,
        ) {
            Ok(baseline) => {
                selected = Some(SelectedBaseline {
                    used_fallback,
                    ..baseline
                });
                break;
            }
            Err(error) => last_error = Some(error),
        }
    }
    let selected = selected.ok_or_else(|| last_error.expect("bootstrap 至少尝试过一个基线"))?;
    Ok(PreparedBootstrap {
        selected,
        state,
        chain,
        control_hash,
        pins: *pins,
    })
}

/// 写库阶段：复用 `baseline_apply` 的 staging journal 与实际导入/读回校验/水位 CAS。
///
/// 调用方必须持有外部提交写闸；网络已在 [`prepare`] 完成。
pub fn integrate(
    vault: &Vault,
    peer_id: u64,
    prepared: &PreparedBootstrap,
    advancement: BootstrapAdvancement,
    fault: Option<BootstrapFault>,
) -> Result<BootstrapReport, SyncError> {
    let selected = &prepared.selected;

    // 实际导入（staging journal 可续跑）；失败时 journal 保留，下一次按同一基线续跑。
    let report_docs = apply_selected(vault, peer_id, selected, fault)?;

    // 全部实际写入 + 读回校验通过后，才在一个事务里推进水位（max 语义 + CAS）。
    finalize(
        vault,
        selected,
        &prepared.state,
        &prepared.chain,
        &prepared.control_hash,
        &prepared.pins,
        advancement,
        fault,
    )?;

    // 成功后才清 staging journal；若这里失败，重试会幂等重放并再次清理。
    let _cleared = abort_baseline_apply(vault)?;

    Ok(BootstrapReport {
        checkpoint_id: selected.checkpoint.checkpoint_id,
        covered_seq: selected.checkpoint.covered_seq,
        used_fallback: selected.used_fallback,
        docs: report_docs,
        assets: selected.assets.len(),
        cursor: selected.checkpoint.cursor.clone(),
        signed_checkpoint: true,
    })
}

fn run_inner<C: RelayClient>(
    vault: &Vault,
    client: &mut C,
    vault_id: [u8; 16],
    peer_id: u64,
    root: &RootSecretV1,
    pins: &BootstrapPins,
    fault: Option<BootstrapFault>,
) -> Result<BootstrapReport, SyncError> {
    let prepared = prepare(vault, client, vault_id, peer_id, root, pins)?;
    integrate(
        vault,
        peer_id,
        &prepared,
        BootstrapAdvancement::MaxOnly,
        fault,
    )
}

/// 校验本地 durable bootstrap 锚（ADR 0013 §3；`validate_remote`/`finalize` 共用）。
///
/// 两类视图必须分开，**不能**把旧 hash 挂到 `max(rev)` 上比较：
///
/// - `anchor.rev == state.control_rev`：摘要必须逐字节等于当前视图（`epoch`/`log_id`
///   由当前 state 决定；同 rev 换 epoch/log/hash 都是 equivocation）；
/// - `anchor.rev <  state.control_rev`：**不能忽略**旧锚，也不能拿新 rev 的坐标复算；
///   按锚自己的 `rev/epoch/log` + 截断到该 rev 的管理链复算，证明当前链仍包含旧锚
///   认证过的前缀；复算不出视图 → [`SyncError::NeedsTrust`]，摘要不符 → equivocation。
fn verify_bootstrap_anchor(
    anchor: &BootstrapAnchorV1,
    state: &RelayState,
    chain: &[ManagerChainEntry],
    current_hash: &[u8; 32],
) -> Result<(), SyncError> {
    if anchor.rev > state.control_rev {
        return Err(SyncError::ControlRollback {
            expected: anchor.rev,
            actual: state.control_rev,
        });
    }
    if anchor.rev == state.control_rev {
        if anchor.hash != *current_hash {
            return Err(SyncError::Equivocation(
                "bootstrap 控制头同 rev 但 hash 变化（同 rev 历史不完整）",
            ));
        }
        return Ok(());
    }
    let computed = control_head_hash_at(state.vault_id, state.instance_id, anchor, chain).ok_or(
        SyncError::NeedsTrust("bootstrap 旧锚的 rev 不在当前管理链视图内，无法证明历史前缀"),
    )?;
    if computed != anchor.hash {
        return Err(SyncError::Equivocation(
            "bootstrap 旧锚未被当前管理链覆盖（rev 前进但前缀被重写）",
        ));
    }
    Ok(())
}

/// 本地 pin 的**结构**校验：`control_hash` 与 `control_rev` 必须成对。
///
/// 只有 hash 没有 rev 是本地损坏（无法按 rev 选择同 rev/历史锚视图）；必须在
/// `validate_remote` 与 `finalize` 两条路径都检查，不能只在前者检查。
fn validate_pin_shape(pins: &BootstrapPins) -> Result<(), SyncError> {
    if pins.control_hash.is_some() && pins.control_rev.is_none() {
        return Err(SyncError::LocalCorrupt(
            "bootstrap pins 有 control_hash 但缺 control_rev",
        ));
    }
    Ok(())
}

/// relay 侧与本地 pins 的写前校验：任何不满足都必须在写库前失败。
fn validate_remote(
    vault_id: [u8; 16],
    root: &RootSecretV1,
    pins: &BootstrapPins,
    bundle: &BootstrapBundle,
    state: &RelayState,
    chain: &[ManagerChainEntry],
    control_hash: &[u8; 32],
) -> Result<(), SyncError> {
    validate_pin_shape(pins)?;
    if state.vault_id != vault_id {
        return Err(SyncError::RelayWire(
            "bootstrap state.vault_id 与本地不一致",
        ));
    }
    if root.key_epoch != state.epoch {
        return Err(SyncError::Object("bootstrap root epoch 与 relay 不一致"));
    }
    if bundle.epoch != state.epoch {
        return Err(SyncError::RelayWire("bootstrap epoch 与 state 不一致"));
    }
    // bundle 先取、state 后取；head 只能前进，倒退说明 relay 换库/回滚。
    if bundle.head_seq > state.head_seq {
        return Err(SyncError::RelayWire("bootstrap/state head_seq 回退"));
    }
    if let Some(instance) = pins.instance_id {
        if instance != state.instance_id {
            return Err(SyncError::Object(
                "bootstrap relay instance 与本地 pin 不符",
            ));
        }
    }
    if let Some(log_id) = pins.log_id {
        if log_id != state.log_id {
            return Err(SyncError::Storage(
                menghunote_storage::StorageError::SyncLogMismatch {
                    expected: log_id.to_vec(),
                    found: state.log_id.to_vec(),
                },
            ));
        }
    }
    if let Some(epoch) = pins.epoch {
        if epoch > state.epoch {
            return Err(SyncError::Object("bootstrap 不允许回退 epoch"));
        }
    }
    if let Some(rev) = pins.control_rev {
        if rev > state.control_rev {
            return Err(SyncError::Object("bootstrap 不允许回退 control_rev"));
        }
        if let Some(anchor) = pins.control_anchor()? {
            verify_bootstrap_anchor(&anchor, state, chain, control_hash)?;
        }
    } else if pins.control_hash.is_some() {
        return Err(SyncError::LocalCorrupt(
            "bootstrap pins 有 control_hash 但缺 control_rev",
        ));
    }
    if let Some(highwater) = pins.highwater {
        if highwater > state.head_seq {
            return Err(SyncError::Paused(
                crate::relay::PauseReason::RollbackDetected,
            ));
        }
    }
    // 管理链必须是单调 rev 序列（内容摘要见 control_head_hash；完整链认证属于轮换阶段 A）。
    let mut previous_rev = 0u64;
    for entry in chain {
        if entry.rev <= previous_rev {
            return Err(SyncError::RelayWire("bootstrap 管理链 rev 不递增"));
        }
        previous_rev = entry.rev;
    }
    Ok(())
}

/// 控制头摘要：绑定 relay 可见的控制头坐标（instance/log/epoch/control_rev）与管理转换链。
///
/// 这是本地 pin 的摘要定义（不是 relay 内部的 `control_head` hash）：同一 `control_rev`
/// 下 relay 换 instance/log/epoch 或重写管理链都会改变摘要；rev 前进时允许摘要变化
/// （单调性由 `control_rev` 保证）。完整管理链的签名认证属于轮换阶段 A（ADR §28.2），
/// 本模块只做"同 rev 不得换内容"的回退检测。
#[must_use]
pub fn control_head_hash(state: &RelayState, chain: &[ManagerChainEntry]) -> [u8; 32] {
    control_head_hash_parts(
        state.vault_id,
        state.instance_id,
        state.log_id,
        state.epoch,
        state.control_rev,
        chain,
    )
}

/// [`control_head_hash`] 的**坐标化**输入：调用方可以显式给出历史视图的坐标
/// （ADR 0013 §3「按旧 rev/epoch/log/管理链视图验证」）。
///
/// 摘要公式与 [`control_head_hash`] 逐字节相同；`chain` 必须是**该 rev 下**的管理链
/// （即按 `entry.rev <= control_rev` 截断后的前缀）。
#[must_use]
pub fn control_head_hash_parts(
    vault_id: [u8; 16],
    instance_id: [u8; 16],
    log_id: [u8; 16],
    epoch: u64,
    control_rev: u64,
    chain: &[ManagerChainEntry],
) -> [u8; 32] {
    let mut input = Vec::new();
    input.extend_from_slice(b"menghunote/bootstrap/control-head/v1\0");
    input.extend_from_slice(&vault_id);
    input.extend_from_slice(&instance_id);
    input.extend_from_slice(&log_id);
    input.extend_from_slice(&epoch.to_be_bytes());
    input.extend_from_slice(&control_rev.to_be_bytes());
    input.extend_from_slice(&u32::try_from(chain.len()).unwrap_or(u32::MAX).to_be_bytes());
    for entry in chain {
        input.extend_from_slice(&entry.rev.to_be_bytes());
        input.extend_from_slice(&entry.pubkey);
        input.extend_from_slice(
            &u32::try_from(entry.transition.len())
                .unwrap_or(u32::MAX)
                .to_be_bytes(),
        );
        input.extend_from_slice(&entry.transition);
        push_optional_signature(&mut input, entry.old_signature);
        push_optional_signature(&mut input, entry.new_signature);
    }
    Sha256::digest(&input).into()
}

/// 按 bootstrap 锚**自己的** rev/epoch/log 视图复算控制头摘要（ADR 0013 §3）。
///
/// 为什么不能用当前 head 的坐标：摘要把 `(epoch, control_rev)` 也哈希进去，rev 前进后
/// 同一条历史管理链会算出不同摘要。历史锚只能按"它当初记录的坐标 + 当时的管理链"复算：
///
/// - `epoch` / `log_id` / `rev` 取自锚本身（本地 durable 事实，relay 不能改）；
/// - 管理链按**生效 epoch** 截断：`/control` 键 5 的 `rev` 是**管理 rev**（不是 control
///   rev），每条转换记录 `from_epoch → to_epoch`；epoch 只能由 activation 推进，因此
///   "锚当时的链" = rev1 genesis + `to_epoch <= anchor.epoch` 的转换（必须仍是链前缀）；
/// - `instance_id` 取自本地 pin/身份（换实例另有独立检查）。
///
/// 返回 `None` = 无法重建该历史视图（链不完整/非前缀/转换不可解码），调用方必须
/// fail-closed，不能把"视图不可用"当成"验证通过"。
#[must_use]
pub fn control_head_hash_at(
    vault_id: [u8; 16],
    instance_id: [u8; 16],
    anchor: &BootstrapAnchorV1,
    chain: &[ManagerChainEntry],
) -> Option<[u8; 32]> {
    if chain.first().is_none_or(|entry| entry.rev != 1) {
        return None;
    }
    // rev1 的 `transition` 是 genesis 原字节（不是 ManagerTransitionV1），不参与解码。
    let mut end = 1usize;
    for (index, entry) in chain.iter().enumerate().skip(1) {
        let transition = ManagerTransitionV1::decode(&entry.transition).ok()?;
        if transition.new_rev != entry.rev {
            return None;
        }
        if transition.to_epoch <= anchor.epoch {
            if index != end {
                // 更早的转换不属于历史视图，后面却又出现属于它的转换 = 非前缀，拒绝。
                return None;
            }
            end = index + 1;
        }
    }
    Some(control_head_hash_parts(
        vault_id,
        instance_id,
        anchor.log_id,
        anchor.epoch,
        anchor.rev,
        &chain[..end],
    ))
}

fn push_optional_signature(input: &mut Vec<u8>, signature: Option<[u8; 64]>) {
    match signature {
        None => input.push(0),
        Some(signature) => {
            input.push(1);
            input.extend_from_slice(&signature);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn load_baseline<C: RelayClient>(
    vault: &Vault,
    client: &mut C,
    root: &RootSecretV1,
    vault_id: [u8; 16],
    peer_id: u64,
    checkpoint: &BootstrapCheckpoint,
    bundle: &BootstrapBundle,
    state: &RelayState,
) -> Result<SelectedBaseline, SyncError> {
    if checkpoint.epoch != state.epoch || checkpoint.epoch != bundle.epoch {
        return Err(SyncError::Object("bootstrap 基线 epoch 与 relay 不一致"));
    }
    // "≤S"：基线只能覆盖 relay 已经产生的水位；低于 GC floor 的游标已不可继续。
    if checkpoint.covered_seq > bundle.head_seq || checkpoint.covered_seq > state.head_seq {
        return Err(SyncError::Object("bootstrap 基线 S 超过 relay head"));
    }
    // 低于 GC floor 的游标已经不可继续：两次请求之间 floor 也可能前进，取较大者。
    if checkpoint.covered_seq < bundle.gc_floor || checkpoint.covered_seq < state.gc_floor {
        return Err(SyncError::Object("bootstrap 基线 S 低于 relay GC floor"));
    }
    if checkpoint.cursor.is_empty() {
        return Err(SyncError::Object("bootstrap 基线缺 relay 游标"));
    }

    let (manifest, manifest_digest) = fetch_manifest(client, root, checkpoint)?;
    verify_checkpoint_commit(client, checkpoint, &manifest, &manifest_digest)?;
    verify_manifest(&manifest, vault_id, checkpoint, state.epoch)?;
    let docs = download_docs(client, root, peer_id, &manifest)?;
    let structure_notes = verify_structure_semantics(&docs, peer_id)?;
    let assets = download_assets(vault, client, root, &manifest)?;
    Ok(SelectedBaseline {
        checkpoint: checkpoint.clone(),
        used_fallback: false,
        docs,
        assets,
        structure_notes,
    })
}

fn verify_checkpoint_commit<C: RelayClient>(
    client: &mut C,
    checkpoint: &BootstrapCheckpoint,
    manifest: &ManifestV1,
    manifest_digest: &[u8; 32],
) -> Result<(), SyncError> {
    let signed_commit = checkpoint
        .signed_commit
        .as_deref()
        .ok_or(SyncError::Object("bootstrap 缺 CheckpointCommit"))?;
    let signature = checkpoint
        .signature
        .ok_or(SyncError::Object("bootstrap 缺 checkpoint 签名"))?;
    let commit = CheckpointCommit::decode(signed_commit)?;
    crate::checkpoint::require_current_checkpoint_validation(&commit)?;
    if commit.vault_id != manifest.vault_id
        || commit.checkpoint_id != checkpoint.checkpoint_id
        || commit.epoch != checkpoint.epoch
        || commit.covered_relay_seq != checkpoint.covered_seq
        || commit.manifest_object_id != checkpoint.manifest_oid
        || commit.manifest_digest != *manifest_digest
    {
        return Err(SyncError::Object("bootstrap checkpoint commit 与响应不符"));
    }
    // ADR §21：manifest 条目已带 key_epoch/total_plain_len/object_digest，可只凭 manifest
    // 重算 refs_digest；不再依赖 GET objects。
    let refs = manifest_refs(manifest);
    let refs_digest: [u8; 32] = Sha256::digest(refs_digest_input(&refs)?).into();
    if commit.refs_digest != refs_digest {
        return Err(SyncError::Object("bootstrap refs_digest 与 manifest 不符"));
    }
    if let Some(refs_digest) = checkpoint.refs_digest {
        if commit.refs_digest != refs_digest {
            return Err(SyncError::Object("bootstrap refs_digest 与 commit 不符"));
        }
    }
    let certificate = client
        .device_cert(&commit.producer_device_id)?
        .ok_or(SyncError::Object("bootstrap 缺 checkpoint 生产者证书"))?;
    let input = checkpoint_signing_input(&commit)?;
    verify_signature(&certificate.sign_ed25519_pub, &input, &signature)?;
    Ok(())
}

/// 先读第 0 片信封拿 part_count/total_len，再按实际片数重算 object_digest 并读全对象。
///
/// 这样多片 manifest 也能校验（`/bootstrap` 响应不直接给 manifest 的 part_count/digest，
/// 只能先看信封，再让签名 commit 绑定重算出的 digest）。
fn fetch_manifest<C: RelayClient>(
    client: &mut C,
    root: &RootSecretV1,
    checkpoint: &BootstrapCheckpoint,
) -> Result<(ManifestV1, [u8; 32]), SyncError> {
    let first = client.part(&checkpoint.manifest_oid, 0)?;
    let envelope = Envelope::decode(&first)?;
    if envelope.header.object_type != ObjectType::Manifest
        || envelope.header.object_id != checkpoint.manifest_oid
        || envelope.header.part_count == 0
    {
        return Err(SyncError::Object("bootstrap manifest 信封字段不符"));
    }
    if envelope.header.generation != 0 || envelope.header.sender_seq != 0 {
        return Err(SyncError::Object("bootstrap manifest 信封字段不符"));
    }
    let digest = object_digest_of(client, &checkpoint.manifest_oid, envelope.header.part_count)?;
    let (header, bytes) = fetch_object(
        client,
        root,
        ObjectType::Manifest,
        &checkpoint.manifest_oid,
        envelope.header.part_count,
        envelope.header.total_plaintext_len,
        &digest,
        &[],
    )?;
    if header.key_epoch != checkpoint.epoch {
        return Err(SyncError::Object(
            "bootstrap manifest epoch 与 checkpoint 不符",
        ));
    }
    Ok((ManifestV1::decode(&bytes)?, digest))
}

fn object_digest_of<C: RelayClient>(
    client: &mut C,
    object_id: &[u8; 32],
    part_count: u32,
) -> Result<[u8; 32], SyncError> {
    let mut digests = Vec::with_capacity(part_count as usize);
    for index in 0..part_count {
        let bytes = client.part(object_id, index)?;
        digests.push(Sha256::digest(&bytes).into());
    }
    let input = menghunote_sync_proto::object_digest_input(part_count, &digests)?;
    Ok(Sha256::digest(&input).into())
}

/// 由 manifest 重建 refs 集合（`refs_digest` 的唯一口径）。
///
/// `pub(crate)`：I 阶段的 checkpoint 级认证（`rotation_apply::verify_checkpoint_authorization`）
/// 必须复算 `refs_digest`，复用本函数而不是复制一份口径（ADR §21：manifest 条目已带
/// key_epoch/total_plain_len/object_digest，只凭 manifest 就能重算）。
pub(crate) fn manifest_refs(manifest: &ManifestV1) -> Vec<ObjectRefV1> {
    let mut refs = Vec::with_capacity(manifest.docs.len() + manifest.assets.len());
    refs.extend(manifest.docs.iter().map(|doc| ObjectRefV1 {
        object_id: doc.snapshot_object_id,
        object_type: ObjectType::Snapshot,
        generation: doc.generation,
        key_epoch: doc.key_epoch,
        part_count: doc.part_count,
        total_plain_len: doc.total_plain_len,
        object_digest: doc.object_digest,
    }));
    refs.extend(manifest.assets.iter().map(|asset| ObjectRefV1 {
        object_id: asset.object_id,
        object_type: ObjectType::Asset,
        generation: 0,
        key_epoch: asset.key_epoch,
        part_count: asset.part_count,
        total_plain_len: asset.total_plain_len,
        object_digest: asset.object_digest,
    }));
    refs
}

fn verify_manifest(
    manifest: &ManifestV1,
    vault_id: [u8; 16],
    checkpoint: &BootstrapCheckpoint,
    epoch: u64,
) -> Result<(), SyncError> {
    if manifest.vault_id != vault_id
        || manifest.checkpoint_id != checkpoint.checkpoint_id
        || manifest.covered_relay_seq != checkpoint.covered_seq
        || manifest.key_epoch != epoch
    {
        return Err(SyncError::Object("bootstrap manifest 与 checkpoint 不符"));
    }
    if manifest.min_reader > u32::try_from(MIN_READER).unwrap_or(u32::MAX) {
        return Err(SyncError::Object("bootstrap 基线要求更高的 reader"));
    }
    if manifest.docs.is_empty() {
        return Err(SyncError::Object("bootstrap manifest 没有文档"));
    }
    // 结构文档的 ID 是 storage 的固定常量：apply_baseline_batch 依赖它写 `kind='structure'`，
    // 允许 manifest 声明别的 ID 会让结构被当成正文落库。
    if manifest.structure_doc_id != STRUCTURE_DOC_ID {
        return Err(SyncError::Object(
            "bootstrap manifest structure_doc_id 非法",
        ));
    }
    let mut doc_ids = BTreeSet::new();
    let mut snapshot_ids = BTreeSet::new();
    let mut structure_docs = 0usize;
    for doc in &manifest.docs {
        if !doc_ids.insert(doc.doc_id.as_str()) || !snapshot_ids.insert(doc.snapshot_object_id) {
            return Err(SyncError::Object("bootstrap manifest 文档/对象重复"));
        }
        if doc.kind != KIND_BODY && doc.kind != KIND_STRUCTURE {
            return Err(SyncError::Object("bootstrap manifest 文档 kind 非法"));
        }
        if doc.key_epoch != epoch {
            return Err(SyncError::Object("bootstrap manifest 文档 epoch 不符"));
        }
        if doc.doc_id == STRUCTURE_DOC_ID {
            structure_docs += 1;
            if doc.kind != KIND_STRUCTURE {
                return Err(SyncError::Object("bootstrap 结构文档 kind 非法"));
            }
        } else if doc.kind == KIND_STRUCTURE {
            return Err(SyncError::Object("bootstrap manifest 含多余结构文档"));
        }
    }
    if structure_docs != 1 {
        return Err(SyncError::Object("bootstrap manifest 缺 structure 文档"));
    }
    let mut asset_objects = BTreeSet::new();
    let mut asset_hashes = BTreeSet::new();
    for asset in &manifest.assets {
        if !asset_objects.insert(asset.object_id) || !asset_hashes.insert(asset.real_sha256) {
            return Err(SyncError::Object("bootstrap manifest 资产重复"));
        }
        if asset.key_epoch != epoch {
            return Err(SyncError::Object("bootstrap manifest 资产 epoch 不符"));
        }
    }
    Ok(())
}

fn download_docs<C: RelayClient>(
    client: &mut C,
    root: &RootSecretV1,
    peer_id: u64,
    manifest: &ManifestV1,
) -> Result<Vec<VerifiedDoc>, SyncError> {
    let mut docs = Vec::with_capacity(manifest.docs.len());
    for doc in &manifest.docs {
        let (header, bytes) = fetch_self_described(
            client,
            root,
            ObjectType::Snapshot,
            &doc.snapshot_object_id,
            &doc.object_digest,
            &[],
        )?;
        if header.part_count != doc.part_count
            || header.generation != doc.generation
            || header.key_epoch != manifest.key_epoch
            || header.sender_seq != 0
        {
            return Err(SyncError::Object("bootstrap 快照信封字段不符"));
        }
        let snapshot = SnapshotV1::decode(&bytes)?;
        if !snapshot.full_history
            || snapshot.doc_id != doc.doc_id
            || snapshot.kind != doc.kind
            || snapshot.generation != doc.generation
            || snapshot.vv != doc.vv
            || snapshot.frontier != doc.frontier
        {
            return Err(SyncError::Object("bootstrap 快照与 manifest 不符"));
        }
        if snapshot.codec_version != CODEC_VERSION
            || snapshot.app_doc_schema != u32::try_from(APP_DOC_SCHEMA).unwrap_or(u32::MAX)
            || snapshot.min_reader > u32::try_from(MIN_READER).unwrap_or(u32::MAX)
        {
            return Err(SyncError::Object("bootstrap 快照 app schema/codec 不可读"));
        }
        // 实际 VV/frontier：独立空 LoroDoc import 后再比，不只信 manifest 声明。
        let actual_vv = verify_snapshot_history(&snapshot, doc)?;
        let text = if doc.kind == KIND_BODY {
            let body = BodyDoc::restore(
                doc.doc_id.clone(),
                peer_id,
                Some(&snapshot.loro_snapshot_bytes),
                &[],
            )?;
            Some(body.text())
        } else {
            None
        };
        docs.push(VerifiedDoc {
            doc_id: doc.doc_id.clone(),
            kind: doc.kind,
            snapshot,
            actual_vv,
            text,
        });
    }
    Ok(docs)
}

/// 把 snapshot 导入独立空 `LoroDoc`，校验 actual VV/frontier 与 manifest 声明一致。
///
/// 相等比较同时排除两种攻击：宣称了不存在的 op（虚高）与真实历史里缺 op。
fn verify_snapshot_history(
    snapshot: &SnapshotV1,
    manifest_doc: &ManifestDoc,
) -> Result<VersionVector, SyncError> {
    if !snapshot.full_history {
        return Err(SyncError::Object("bootstrap 快照不是 full-history"));
    }
    let doc = loro::LoroDoc::new();
    doc.import(&snapshot.loro_snapshot_bytes)
        .map_err(|_| SyncError::Object("bootstrap 快照 Loro 数据非法"))?;
    // shallow snapshot 的 VV/frontier 仍可能自洽，必须再问 Loro 的 shallow 状态。
    crate::checkpoint::ensure_snapshot_full_history(&doc)?;
    let actual_vv = doc.oplog_vv();
    let declared_vv = decode_vv(&manifest_doc.vv)?;
    if actual_vv != declared_vv {
        return Err(SyncError::Object(
            "bootstrap 快照实际 VV 与 manifest 声明不一致",
        ));
    }
    let actual_frontier = doc.state_frontiers();
    if manifest_doc.frontier.is_empty() {
        if actual_frontier.iter().next().is_some() {
            return Err(SyncError::Object(
                "bootstrap manifest frontier 为空但快照有实际 frontier",
            ));
        }
    } else if actual_frontier.encode() != manifest_doc.frontier {
        return Err(SyncError::Object(
            "bootstrap 快照 frontier 与 manifest 声明不一致",
        ));
    }
    Ok(actual_vv)
}

/// 结构语义校验：结构快照里的每篇笔记必须指向基线 manifest 内的正文文档。
fn verify_structure_semantics(
    docs: &[VerifiedDoc],
    peer_id: u64,
) -> Result<BTreeMap<String, String>, SyncError> {
    let structure = docs
        .iter()
        .find(|doc| doc.kind == KIND_STRUCTURE)
        .ok_or(SyncError::Object("bootstrap 缺 structure 文档"))?;
    let doc = StructureDoc::restore(peer_id, Some(&structure.snapshot.loro_snapshot_bytes), &[])?;
    let snapshot = doc.snapshot()?;
    let manifest_kinds: BTreeMap<&str, u64> = docs
        .iter()
        .map(|doc| (doc.doc_id.as_str(), doc.kind))
        .collect();
    let mut notes = BTreeMap::new();
    for node in &snapshot.nodes {
        if node.note_id.is_some() != node.body_doc_id.is_some() {
            return Err(SyncError::Object("bootstrap 结构节点笔记字段不成对"));
        }
        if !node.is_note() {
            continue;
        }
        let note_id = node.note_id.clone().unwrap_or_default();
        let body_doc_id = node.body_doc_id.clone().unwrap_or_default();
        if manifest_kinds.get(body_doc_id.as_str()) != Some(&KIND_BODY) {
            return Err(SyncError::Object(
                "bootstrap 结构引用的正文文档不在基线 manifest",
            ));
        }
        if notes.insert(note_id, body_doc_id).is_some() {
            return Err(SyncError::Object("bootstrap 结构快照 note_id 重复"));
        }
    }
    Ok(notes)
}

fn download_assets<C: RelayClient>(
    vault: &Vault,
    client: &mut C,
    root: &RootSecretV1,
    manifest: &ManifestV1,
) -> Result<Vec<VerifiedAsset>, SyncError> {
    let mut assets = Vec::with_capacity(manifest.assets.len());
    for manifest_asset in &manifest.assets {
        // 已有同 hash/长度的本地资产不重复下载；只拉缺失的（plan §8.2）。
        if let Some(local) = menghunote_storage::asset_bytes(vault, manifest_asset.real_sha256)? {
            if u64::try_from(local.len()).ok() == Some(manifest_asset.byte_len)
                && <[u8; 32]>::from(Sha256::digest(&local)) == manifest_asset.real_sha256
            {
                assets.push(VerifiedAsset {
                    object_id: manifest_asset.object_id,
                    real_sha256: manifest_asset.real_sha256,
                    byte_len: manifest_asset.byte_len,
                    media_type: manifest_asset.media_type.clone(),
                    data: local,
                });
                continue;
            }
        }
        let expected = crate::checkpoint::asset_object_id(
            root,
            &manifest_asset.real_sha256,
            manifest_asset.byte_len,
            &manifest_asset.media_type,
        )?;
        if expected != manifest_asset.object_id {
            return Err(SyncError::Object("bootstrap 资产 object_id 与根不符"));
        }
        let (header, bytes) = fetch_self_described(
            client,
            root,
            ObjectType::Asset,
            &manifest_asset.object_id,
            &manifest_asset.object_digest,
            &[],
        )?;
        if header.part_count != manifest_asset.part_count || header.key_epoch != manifest.key_epoch
        {
            return Err(SyncError::Object(
                "bootstrap 资产片数/epoch 与 manifest 不符",
            ));
        }
        // ADR §31：ASSET 的 header producer 固定为 0，且 message_id 必须由 identical
        // asset header 确定性派生（跨设备/重传的密文字节才可能一致）。
        if header.producer_device_id != [0u8; 16]
            || header.generation != 0
            || header.sender_seq != 0
        {
            return Err(SyncError::Object("bootstrap 资产信封字段不符（§31）"));
        }
        if header.message_id != crate::upload::asset_message_id(root, &manifest_asset.object_id)? {
            return Err(SyncError::Object("bootstrap 资产 message_id 非确定性派生"));
        }
        let reference = AssetRef {
            real_sha256: manifest_asset.real_sha256,
            byte_len: manifest_asset.byte_len,
            media_type: manifest_asset.media_type.clone(),
            object: crate::upload::UploadedObject {
                object_id: manifest_asset.object_id,
                part_count: manifest_asset.part_count,
                total_plain_len: header.total_plaintext_len,
                object_digest: manifest_asset.object_digest,
            },
        };
        verify_asset_plaintext(&bytes, &reference)?;
        let body_start = 42 + manifest_asset.media_type.len();
        let data = bytes
            .get(body_start..)
            .ok_or(SyncError::Object("bootstrap 资产明文越界"))?
            .to_vec();
        assets.push(VerifiedAsset {
            object_id: manifest_asset.object_id,
            real_sha256: manifest_asset.real_sha256,
            byte_len: manifest_asset.byte_len,
            media_type: manifest_asset.media_type.clone(),
            data,
        });
    }
    Ok(assets)
}

/// 读第 0 片拿 header，再按 header 的 part_count/total_len 读全对象并校验摘要/解密。
fn fetch_self_described<C: RelayClient>(
    client: &mut C,
    root: &RootSecretV1,
    object_type: ObjectType,
    object_id: &[u8; 32],
    expected_digest: &[u8; 32],
    descriptor: &[u8],
) -> Result<(EnvelopeHeader, Vec<u8>), SyncError> {
    let first = client.part(object_id, 0)?;
    let envelope = Envelope::decode(&first)?;
    if envelope.header.object_type != object_type
        || envelope.header.object_id != *object_id
        || envelope.header.part_count == 0
    {
        return Err(SyncError::Object("bootstrap 对象 header 不符"));
    }
    fetch_object(
        client,
        root,
        object_type,
        object_id,
        envelope.header.part_count,
        envelope.header.total_plaintext_len,
        expected_digest,
        descriptor,
    )
}

/// 清掉"上一次 bootstrap 选了另一份基线"的 staging（数据是 CRDT merge，重放幂等）。
///
/// 轮换的 baseline apply 传空 `source_cursor`，这里用前缀区分，绝不误清轮换 journal。
fn reconcile_journal(
    vault: &Vault,
    checkpoint_id: &[u8; 16],
    baseline_s: u64,
    source_cursor: &str,
) -> Result<(), SyncError> {
    let Some(state) = baseline_apply_state(vault)? else {
        return Ok(());
    };
    if state.status != BaselineApplyStatus::Staging {
        return Ok(());
    }
    let same = state.checkpoint_id == *checkpoint_id
        && state.baseline_s == baseline_s
        && state.source_cursor == source_cursor;
    if same {
        return Ok(());
    }
    if state.source_cursor.starts_with(JOURNAL_SOURCE_PREFIX) {
        let _aborted = abort_baseline_apply(vault)?;
        return Ok(());
    }
    Err(SyncError::Storage(
        menghunote_storage::StorageError::SyncConflict {
            context: "已有非 bootstrap 的 baseline apply staging（轮换？），拒绝 bootstrap"
                .to_owned(),
        },
    ))
}

/// staging journal + 分批导入 docs/structure/assets；返回按 manifest 顺序的报告文本。
fn apply_selected(
    vault: &Vault,
    peer_id: u64,
    selected: &SelectedBaseline,
    fault: Option<BootstrapFault>,
) -> Result<Vec<(String, Option<String>)>, SyncError> {
    let checkpoint_id = selected.checkpoint.checkpoint_id;
    let baseline_s = selected.checkpoint.covered_seq;
    let source_cursor = format!("{JOURNAL_SOURCE_PREFIX}{}", selected.checkpoint.cursor);
    // 完整性基准：本 checkpoint 覆盖的全部文档与资产都必须出现在 apply 批里。
    // 集合由 begin 记录，任何"少导入一批"的续跑都不会被误当成完整
    // （rotation 的 finish 会直接 Conflict；bootstrap 的 finalize 另有逐项读回校验）。
    let expected = BaselineExpectation::new(
        selected.docs.iter().map(|doc| doc.doc_id.clone()),
        selected.assets.iter().map(|asset| asset.object_id),
    );
    reconcile_journal(vault, &checkpoint_id, baseline_s, &source_cursor)?;
    begin_baseline_apply(vault, &checkpoint_id, baseline_s, &source_cursor, &expected)?;

    // body → structure → asset（structure 投影依赖正文行已存在；与旧实现的顺序一致）。
    let bodies: Vec<&VerifiedDoc> = selected
        .docs
        .iter()
        .filter(|doc| doc.kind == KIND_BODY)
        .collect();
    for chunk in bodies.chunks(BOOTSTRAP_DOC_BATCH) {
        let batch: Vec<(String, Vec<u8>)> = chunk
            .iter()
            .map(|doc| (doc.doc_id.clone(), doc.snapshot.loro_snapshot_bytes.clone()))
            .collect();
        apply_baseline_batch(vault, &batch, &[])?;
    }
    if fault == Some(BootstrapFault::AfterBodies) {
        return Err(injected_fault());
    }

    let structure = selected
        .docs
        .iter()
        .find(|doc| doc.kind == KIND_STRUCTURE)
        .ok_or(SyncError::Object("bootstrap 缺 structure 文档"))?;
    apply_baseline_batch(
        vault,
        &[(
            structure.doc_id.clone(),
            structure.snapshot.loro_snapshot_bytes.clone(),
        )],
        &[],
    )?;
    if fault == Some(BootstrapFault::BeforeStructureProjection) {
        return Err(injected_fault());
    }
    let mut writer = StructureWriter::open(vault, peer_id)?;
    writer.commit("bootstrap")?;

    if !selected.assets.is_empty() {
        let assets: Vec<BaselineAsset> = selected
            .assets
            .iter()
            .map(|asset| BaselineAsset {
                object_id: asset.object_id,
                asset_hash: asset.real_sha256,
                byte_len: asset.byte_len,
                media_type: Some(asset.media_type.clone()),
                data: asset.data.clone(),
            })
            .collect();
        apply_baseline_assets(vault, &assets)?;
    }
    if fault == Some(BootstrapFault::AfterAssets) {
        return Err(injected_fault());
    }

    // 读回校验的耗时只作诊断指标（不参与决策）；失败也要记录。
    let verify_started = Instant::now();
    let verified = verify_imported(vault, peer_id, selected, fault);
    record_baseline_verify_time(verify_started.elapsed());
    match verified {
        Ok(report) => Ok(report),
        Err(error) => {
            // 不回滚已 merge 的 Loro/资产，也不改 journal.status（schema CHECK 只有
            // staging/completed）：只写失败标记，让下一次 begin 拒绝续跑，直到 abort。
            if let Err(mark_error) = mark_baseline_verify_failed(vault, &error.to_string()) {
                // 不能把原始 verify 错误替换成 marker 写错误：调用方需要根因；同时
                // 明确 marker 未持久化意味着 journal 仍可续跑，必须先 abort。
                return Err(SyncError::Storage(
                    menghunote_storage::StorageError::SyncConflict {
                        context: format!(
                            "baseline 读回校验失败：{error}；另外写失败标记也失败（{mark_error}），journal 仍可续跑，必须先 abort_baseline_apply"
                        ),
                    },
                ));
            }
            Err(error)
        }
    }
}

/// 单篇文档的读回校验：导出点 VV ⊆ 当前本地 VV，且仅在无本地 extra 时比较正文。
///
/// - 方向固定为「快照/导出点 VV 必须被本地 VV 覆盖」：`actual_vv.sub_iter(local_vv)`
///   为空才通过；**不要求相等**。导出点之后本地新增的 op 是合法编辑（verify 不与编辑
///   互斥），相等比较会把它误判成失败；
/// - 只有 `local_vv ⊆ actual_vv`（本地没有快照之外的 op）时，Loro 才保证本地正文等于
///   快照正文，此时才做逐字节比较；有本地 extra 时正文必然已被合并改写，比较必然误报。
fn check_imported_doc(
    actual_vv: &VersionVector,
    local_vv: &VersionVector,
    expected_text: Option<&str>,
    actual_text: &str,
) -> Result<(), &'static str> {
    if actual_vv.sub_iter(local_vv).next().is_some() {
        return Err("bootstrap 导入后本地 VV 未覆盖快照");
    }
    if local_vv.sub_iter(actual_vv).next().is_none() {
        if let Some(expected) = expected_text {
            if expected != actual_text {
                return Err("bootstrap 后正文与快照不一致");
            }
        }
    }
    Ok(())
}

/// 读回校验：本地 VV 必须覆盖快照、正文/结构投影/资产必须与快照一致。
fn verify_imported(
    vault: &Vault,
    peer_id: u64,
    selected: &SelectedBaseline,
    fault: Option<BootstrapFault>,
) -> Result<Vec<(String, Option<String>)>, SyncError> {
    let mut body_texts: BTreeMap<String, String> = BTreeMap::new();
    for (index, doc) in selected
        .docs
        .iter()
        .filter(|doc| doc.kind == KIND_BODY)
        .enumerate()
    {
        let (body, head) = load_body_doc(vault, &doc.doc_id, peer_id, None)?;
        let local_vv = head.version_vector()?;
        // 测试故障注入：伪造一个「导出点有、本地没有」的 op，制造真实的 VV 未覆盖失败。
        let actual_vv = if fault == Some(BootstrapFault::VerifyVvNotCovered) && index == 0 {
            let mut mismatched = doc.actual_vv.clone();
            mismatched.insert(VERIFY_FAULT_PEER, 1);
            mismatched
        } else {
            doc.actual_vv.clone()
        };
        let text = body.text();
        if let Err(reason) = check_imported_doc(&actual_vv, &local_vv, doc.text.as_deref(), &text) {
            return Err(SyncError::Object(reason));
        }
        body_texts.insert(doc.doc_id.clone(), text);
    }

    // 结构重放一致性 + 基线声明的每篇笔记都已投影且正文存在。
    //
    // `StructureSnapshot` 是**全量节点**（including deleted），删除是 grow-only meta
    // 事件而不是 LoroTree::delete；因此本地并发删除基线笔记不会让这里缺项，也不会
    // 误写失败 marker。verify 只要求“基线声明的 note/body 仍存在”，不把 deleted 当作
    // 结构缺失；这是当前产品语义（删笔记不删正文，恢复仍可用）。
    let (structure, _head) = load_structure(vault, peer_id)?;
    let merged = structure.snapshot()?;
    let merged_notes: BTreeMap<&str, &str> = merged
        .nodes
        .iter()
        .filter(|node| node.is_note())
        .filter_map(|node| Some((node.note_id.as_deref()?, node.body_doc_id.as_deref()?)))
        .collect();
    for (note_id, body_doc_id) in &selected.structure_notes {
        if merged_notes.get(note_id.as_str()) != Some(&body_doc_id.as_str()) {
            return Err(SyncError::Object("bootstrap 结构投影与快照不一致"));
        }
        if !body_texts.contains_key(body_doc_id) {
            return Err(SyncError::Object("bootstrap 结构引用的正文缺失"));
        }
    }

    for asset in &selected.assets {
        match asset_bytes(vault, asset.real_sha256)? {
            Some(stored) if stored == asset.data => {}
            _ => return Err(SyncError::Object("bootstrap 资产未正确落库")),
        }
    }

    Ok(selected
        .docs
        .iter()
        .map(|doc| {
            let text = if doc.kind == KIND_STRUCTURE {
                None
            } else {
                body_texts.get(&doc.doc_id).cloned()
            };
            (doc.doc_id.clone(), text)
        })
        .collect())
}

/// 原子推进水位（max 语义 + CAS）。
///
/// 只允许前进：`received/integrated = max(现有, S)`、`highwater = max(现有, pin, S)`；
/// 已有进度时保留现有 relay 游标（不把已消费的位置回退）。`instance/log_id/epoch/
/// control_rev/control_hash` 只允许前进或不变。任何一项回退都在**写事务之前**硬失败。
#[allow(clippy::too_many_arguments)]
fn finalize(
    vault: &Vault,
    selected: &SelectedBaseline,
    state: &RelayState,
    chain: &[ManagerChainEntry],
    control_hash: &[u8; 32],
    pins: &BootstrapPins,
    advancement: BootstrapAdvancement,
    fault: Option<BootstrapFault>,
) -> Result<(), SyncError> {
    let s = selected.checkpoint.covered_seq;
    let cursor_state = sync_cursor_state(vault)?;
    if let Some(log_id) = &cursor_state.log_id {
        if *log_id != state.log_id {
            return Err(SyncError::Storage(
                menghunote_storage::StorageError::SyncLogMismatch {
                    expected: log_id.to_vec(),
                    found: state.log_id.to_vec(),
                },
            ));
        }
    }
    let stored = BootstrapPins::from_vault(vault)?;
    validate_pin_shape(&stored)?;
    if let Some(instance) = stored.instance_id {
        if instance != state.instance_id {
            return Err(SyncError::Object("bootstrap 不允许更换 relay instance"));
        }
    }
    if let Some(epoch) = stored.epoch {
        if epoch > state.epoch {
            return Err(SyncError::Object("bootstrap 不允许回退 epoch"));
        }
    }
    if let Some(rev) = stored.control_rev {
        if rev > state.control_rev {
            return Err(SyncError::Object("bootstrap 不允许回退 control_rev"));
        }
        // 与 `validate_remote` 同一口径：两类视图（同 rev 摘要 / 异 rev 历史锚）都验，
        // 不因 rev 前进就跳过旧摘要（ADR 0013 §3）。
        if let Some(anchor) = stored.control_anchor()? {
            verify_bootstrap_anchor(&anchor, state, chain, control_hash)?;
        }
    }
    if let Some(highwater) = pins.highwater {
        if highwater > state.head_seq {
            return Err(SyncError::Paused(
                crate::relay::PauseReason::RollbackDetected,
            ));
        }
    }
    if let Some(highwater) = stored.highwater {
        if highwater > state.head_seq {
            return Err(SyncError::Paused(
                crate::relay::PauseReason::RollbackDetected,
            ));
        }
    }

    let current_cursor = vault.get_meta(RELAY_CURSOR_META)?;
    let raw_received = vault.get_meta(META_SYNC_RECEIVED_SEQ)?;
    let raw_integrated = vault.get_meta(META_SYNC_INTEGRATED_SEQ)?;
    let raw_highwater = vault.get_meta(META_SYNC_CONFIRMED_HIGHWATER)?;

    let received_target = cursor_state.received_seq.max(s);
    let integrated_target = cursor_state.integrated_seq.max(s);
    let mut highwater_target = cursor_state.confirmed_highwater.unwrap_or(0).max(s);
    if let Some(pin) = pins.highwater {
        highwater_target = highwater_target.max(pin);
    }
    let has_progress = cursor_state.received_seq > 0
        || cursor_state.integrated_seq > 0
        || cursor_state
            .confirmed_highwater
            .is_some_and(|value| value > 0);
    if has_progress && current_cursor.is_none() {
        return Err(SyncError::LocalCorrupt("本地已有同步水位但缺 relay 游标"));
    }
    // 已有进度时保留现有游标：baseline 的 cursor 只用于"从零开始"的设备，避免回退。
    // Phase 1B 追赶例外：导入并读回校验成功后，S 已覆盖旧前缀，允许前移到 checkpoint
    // cursor（最终仍由后续 `advance_cursor` 条件约束，不能因低 S 回退）。
    let write_cursor = !has_progress || advancement == BootstrapAdvancement::CatchUp;

    let mut statements = Vec::new();
    let mut guards = Vec::new();
    // 已有进度时不改写 cursor，但仍要 CAS 自写回一次：若并发写者在读完
    // current_cursor 之后改写了 sync.relay_cursor，这里必须 Conflict，而不是静默
    // 放过一个已经变化的游标。
    if !write_cursor {
        if let Some(cursor) = current_cursor.as_deref() {
            push_meta_cas_bytes(
                &mut statements,
                &mut guards,
                RELAY_CURSOR_META,
                Some(cursor),
                cursor,
            );
        }
    }
    if cursor_state.log_id.is_none() {
        push_meta_insert_absent(
            &mut statements,
            &mut guards,
            META_SYNC_LOG_ID,
            &state.log_id,
        );
    }
    if raw_received.is_none() || received_target != cursor_state.received_seq {
        push_meta_cas_u64(
            &mut statements,
            &mut guards,
            META_SYNC_RECEIVED_SEQ,
            cursor_state.received_seq,
            received_target,
        );
    }
    if raw_integrated.is_none() || integrated_target != cursor_state.integrated_seq {
        push_meta_cas_u64(
            &mut statements,
            &mut guards,
            META_SYNC_INTEGRATED_SEQ,
            cursor_state.integrated_seq,
            integrated_target,
        );
    }
    if raw_highwater.is_none() || cursor_state.confirmed_highwater != Some(highwater_target) {
        push_meta_cas_opt_u64(
            &mut statements,
            &mut guards,
            META_SYNC_CONFIRMED_HIGHWATER,
            cursor_state.confirmed_highwater,
            highwater_target,
        );
    }
    if write_cursor {
        push_meta_cas_opt_bytes(
            &mut statements,
            &mut guards,
            RELAY_CURSOR_META,
            current_cursor.as_deref(),
            selected.checkpoint.cursor.as_bytes(),
        );
    }
    if stored.instance_id.is_none() {
        push_meta_insert_absent(
            &mut statements,
            &mut guards,
            META_BOOTSTRAP_INSTANCE,
            &state.instance_id,
        );
    }
    if stored.epoch != Some(state.epoch) {
        push_meta_cas_opt_u64(
            &mut statements,
            &mut guards,
            META_BOOTSTRAP_EPOCH,
            stored.epoch,
            state.epoch,
        );
    }
    if stored.control_rev != Some(state.control_rev) {
        push_meta_cas_opt_u64(
            &mut statements,
            &mut guards,
            META_BOOTSTRAP_CONTROL_REV,
            stored.control_rev,
            state.control_rev,
        );
    }
    if stored.control_hash != Some(*control_hash) {
        push_meta_cas_opt_bytes(
            &mut statements,
            &mut guards,
            META_BOOTSTRAP_CONTROL_HASH,
            stored.control_hash.as_ref().map(<[u8; 32]>::as_slice),
            control_hash,
        );
    }

    // 模拟并发写者在水位事务提交前改写游标：CAS 必须失败且整笔回滚。
    if fault == Some(BootstrapFault::CursorCasConflict) {
        if !write_cursor {
            return Err(injected_fault());
        }
        vault.set_meta(RELAY_CURSOR_META, b"fault-cursor")?;
    }

    match vault.exec_transaction(&statements, &guards) {
        Ok(_) => Ok(()),
        Err(menghunote_storage::StorageError::GuardFailed { .. }) => Err(SyncError::Object(
            "bootstrap 水位 CAS 冲突（游标/水位已被并发写入），未推进",
        )),
        Err(error) => Err(error.into()),
    }
}

fn injected_fault() -> SyncError {
    SyncError::Object("bootstrap 注入故障（测试）")
}

fn push_meta_insert_absent(
    statements: &mut Vec<Statement>,
    guards: &mut Vec<AffectedGuard>,
    key: &str,
    value: &[u8],
) {
    let index = statements.len();
    statements.push(Statement::new(
        "INSERT INTO meta(key,value) SELECT ?1,?2 \
         WHERE NOT EXISTS(SELECT 1 FROM meta WHERE key=?1)",
        vec![Value::Text(key.to_owned()), Value::Blob(value.to_vec())],
    ));
    guards.push(AffectedGuard::exactly(index, 1));
}

fn push_meta_cas_u64(
    statements: &mut Vec<Statement>,
    guards: &mut Vec<AffectedGuard>,
    key: &str,
    old: u64,
    new: u64,
) {
    if old == 0 {
        // 0 与"meta 缺失"等价：插入，或 CAS 更新已存在的 0。
        let index = statements.len();
        statements.push(Statement::new(
            "INSERT INTO meta(key,value) VALUES(?1,?2) \
             ON CONFLICT(key) DO UPDATE SET value=excluded.value WHERE meta.value=?3",
            vec![
                Value::Text(key.to_owned()),
                Value::Blob(new.to_be_bytes().to_vec()),
                Value::Blob(old.to_be_bytes().to_vec()),
            ],
        ));
        guards.push(AffectedGuard::exactly(index, 1));
        return;
    }
    push_meta_cas_bytes(
        statements,
        guards,
        key,
        Some(&old.to_be_bytes()),
        &new.to_be_bytes(),
    );
}

fn push_meta_cas_opt_u64(
    statements: &mut Vec<Statement>,
    guards: &mut Vec<AffectedGuard>,
    key: &str,
    old: Option<u64>,
    new: u64,
) {
    match old {
        None => push_meta_insert_absent(statements, guards, key, &new.to_be_bytes()),
        Some(old) => push_meta_cas_u64(statements, guards, key, old, new),
    }
}

fn push_meta_cas_opt_bytes(
    statements: &mut Vec<Statement>,
    guards: &mut Vec<AffectedGuard>,
    key: &str,
    old: Option<&[u8]>,
    new: &[u8],
) {
    match old {
        None => push_meta_insert_absent(statements, guards, key, new),
        Some(old) => push_meta_cas_bytes(statements, guards, key, Some(old), new),
    }
}

fn push_meta_cas_bytes(
    statements: &mut Vec<Statement>,
    guards: &mut Vec<AffectedGuard>,
    key: &str,
    old: Option<&[u8]>,
    new: &[u8],
) {
    let Some(old) = old else {
        push_meta_insert_absent(statements, guards, key, new);
        return;
    };
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

fn decode_vv(bytes: &[u8]) -> Result<VersionVector, SyncError> {
    if bytes.is_empty() {
        return Ok(VersionVector::new());
    }
    VersionVector::decode(bytes).map_err(|_| SyncError::Object("bootstrap 快照 VV 非法"))
}

fn read_meta_array<const N: usize>(vault: &Vault, key: &str) -> Result<Option<[u8; N]>, SyncError> {
    match vault.get_meta(key)? {
        None => Ok(None),
        Some(bytes) => bytes
            .try_into()
            .map(Some)
            .map_err(|_| SyncError::LocalCorrupt("bootstrap 本地 pin 长度非法")),
    }
}

fn read_meta_u64(vault: &Vault, key: &str) -> Result<Option<u64>, SyncError> {
    match read_meta_array::<8>(vault, key)? {
        None => Ok(None),
        Some(bytes) => Ok(Some(u64::from_be_bytes(bytes))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use menghunote_storage::{
        append_updates, baseline_verify_failure, create_body_doc, load_head, StorageError,
        UpdateRecord,
    };

    fn manifest(key_epoch: u64, total_plain_len: u64) -> ManifestV1 {
        ManifestV1 {
            vault_id: [1; 16],
            key_epoch: 1,
            checkpoint_id: [2; 16],
            covered_relay_seq: 3,
            min_reader: 1,
            structure_doc_id: "structure".to_owned(),
            docs: vec![ManifestDoc {
                doc_id: "structure".to_owned(),
                kind: KIND_STRUCTURE,
                generation: 1,
                key_epoch,
                vv: vec![1],
                frontier: vec![2],
                snapshot_object_id: [3; 32],
                part_count: 1,
                total_plain_len,
                object_digest: [4; 32],
            }],
            assets: vec![],
        }
    }

    fn digest(value: &ManifestV1) -> [u8; 32] {
        Sha256::digest(refs_digest_input(&manifest_refs(value)).expect("refs")).into()
    }

    #[test]
    fn read_meta_array_rejects_wrong_length_as_local_corrupt() {
        let (_dir, vault) = fresh_vault();
        vault
            .set_meta(META_BOOTSTRAP_EPOCH, b"short")
            .expect("写坏 pin");
        let error = BootstrapPins::from_vault(&vault).expect_err("短 pin 必须拒绝");
        assert!(
            matches!(error, SyncError::LocalCorrupt(_)),
            "pin 长度非法必须是 LocalCorrupt，实际 {error:?}"
        );
    }

    #[test]
    fn pin_shape_rejects_hash_without_rev_on_both_paths() {
        let pins = BootstrapPins {
            control_hash: Some([0x42; 32]),
            control_rev: None,
            ..BootstrapPins::default()
        };
        let error = validate_pin_shape(&pins).expect_err("hash 无 rev 必须拒绝");
        assert!(
            matches!(error, SyncError::LocalCorrupt(_)),
            "应当是 LocalCorrupt，实际 {error:?}"
        );
    }

    #[test]
    fn manifest_key_epoch_or_total_len_tamper_changes_refs_digest() {
        let base = digest(&manifest(1, 100));
        assert_ne!(
            digest(&manifest(2, 100)),
            base,
            "key_epoch 篡改必须改变 refs_digest"
        );
        assert_ne!(
            digest(&manifest(1, 101)),
            base,
            "total_plain_len 篡改必须改变 refs_digest"
        );
    }

    fn document() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let doc = loro::LoroDoc::new();
        doc.get_text("body")
            .insert(0, "hello")
            .expect("insert body text");
        doc.commit();
        (
            doc.export(loro::ExportMode::Snapshot).expect("snapshot"),
            doc.oplog_vv().encode(),
            doc.state_frontiers().encode(),
        )
    }

    fn snapshot(loro: Vec<u8>, vv: Vec<u8>, frontier: Vec<u8>) -> SnapshotV1 {
        SnapshotV1 {
            doc_id: "doc".to_owned(),
            kind: KIND_BODY,
            generation: 1,
            codec_version: CODEC_VERSION.to_owned(),
            app_doc_schema: u32::try_from(APP_DOC_SCHEMA).unwrap_or(1),
            min_reader: 1,
            full_history: true,
            vv,
            frontier,
            loro_snapshot_bytes: loro,
        }
    }

    fn manifest_doc(vv: Vec<u8>, frontier: Vec<u8>) -> ManifestDoc {
        ManifestDoc {
            doc_id: "doc".to_owned(),
            kind: KIND_BODY,
            generation: 1,
            key_epoch: 1,
            vv,
            frontier,
            snapshot_object_id: [7; 32],
            part_count: 1,
            total_plain_len: 1,
            object_digest: [8; 32],
        }
    }

    #[test]
    fn actual_vv_must_match_manifest_claim() {
        let (loro, vv, frontier) = document();
        verify_snapshot_history(
            &snapshot(loro.clone(), vv.clone(), frontier.clone()),
            &manifest_doc(vv.clone(), frontier.clone()),
        )
        .expect("真实历史必须通过");

        // manifest 宣称一个不存在的 peer 的 ops（虚高）：实际 import 必须识破。
        let mut inflated = VersionVector::decode(&vv).expect("vv");
        inflated.insert(0xDEAD, 9);
        assert!(
            verify_snapshot_history(
                &snapshot(loro.clone(), inflated.encode(), frontier.clone()),
                &manifest_doc(inflated.encode(), frontier.clone()),
            )
            .is_err(),
            "虚高 VV 必须被实际 Loro 导入拒绝"
        );

        // frontier 与 manifest 声明不一致。
        let mut wrong_frontier = VersionVector::new();
        wrong_frontier.insert(7, 1);
        assert!(
            verify_snapshot_history(
                &snapshot(loro, vv.clone(), frontier),
                &manifest_doc(vv, wrong_frontier.encode()),
            )
            .is_err(),
            "frontier 不一致必须拒绝"
        );
    }

    #[test]
    fn verify_accepts_concurrent_local_edits_after_export() {
        // 导出点（快照）VV：peer 7 有 2 个 op、peer 8 有 1 个。
        let mut exported_vv = VersionVector::new();
        exported_vv.insert(7, 2);
        exported_vv.insert(8, 1);

        // 导出点之后本地继续编辑：当前 VV 严格包含导出点，但不相等。
        let mut local_vv = exported_vv.clone();
        local_vv.insert(9, 1);
        assert_ne!(local_vv, exported_vv);
        check_imported_doc(
            &exported_vv,
            &local_vv,
            Some("导出点正文"),
            "导出点正文 + 本地新编辑",
        )
        .expect("导出点 VV ⊆ 当前 VV 时必须接受并发本地编辑（不要求相等）");

        // 没有本地 extra（VV 相等）时才逐字节比较正文：不一致必须失败。
        assert!(
            check_imported_doc(&exported_vv, &exported_vv, Some("期望正文"), "实际正文").is_err(),
            "无本地 extra 时正文不一致必须失败"
        );
        check_imported_doc(&exported_vv, &exported_vv, Some("同一正文"), "同一正文")
            .expect("无本地 extra 且正文一致必须通过");

        // 当前 VV 缺少导出点里的 op：方向不能反（不是相等/双向比较）。
        let mut missing_vv = VersionVector::new();
        missing_vv.insert(7, 2);
        assert!(
            check_imported_doc(&exported_vv, &missing_vv, Some("正文"), "正文").is_err(),
            "导出点 VV 未被本地覆盖时必须失败"
        );
    }

    #[test]
    fn full_history_is_mandatory() {
        let (loro, vv, frontier) = document();
        let mut value = snapshot(loro, vv.clone(), frontier.clone());
        value.full_history = false;
        assert!(verify_snapshot_history(&value, &manifest_doc(vv, frontier)).is_err());
    }

    #[test]
    fn structure_semantics_require_referenced_body_in_manifest() {
        let peer = 7u64;
        let structure = menghunote_storage::StructureDoc::create(peer).expect("structure");
        let seed = structure
            .create_note("标题", 1_700_000_000)
            .expect("create note");
        let bytes = structure.export_snapshot().expect("snapshot");
        let doc = VerifiedDoc {
            doc_id: STRUCTURE_DOC_ID.to_owned(),
            kind: KIND_STRUCTURE,
            snapshot: SnapshotV1 {
                doc_id: STRUCTURE_DOC_ID.to_owned(),
                kind: KIND_STRUCTURE,
                generation: 1,
                codec_version: CODEC_VERSION.to_owned(),
                app_doc_schema: 1,
                min_reader: 1,
                full_history: true,
                vv: Vec::new(),
                frontier: Vec::new(),
                loro_snapshot_bytes: bytes,
            },
            actual_vv: VersionVector::new(),
            text: None,
        };
        // 结构引用了 seed.body_doc_id，但基线 manifest 里没有它：必须拒绝。
        assert!(verify_structure_semantics(std::slice::from_ref(&doc), peer).is_err());

        // 正文在 manifest 里时通过，并给出 note_id -> body_doc_id。
        let body = VerifiedDoc {
            doc_id: seed.body_doc_id.clone(),
            kind: KIND_BODY,
            snapshot: SnapshotV1 {
                doc_id: seed.body_doc_id.clone(),
                kind: KIND_BODY,
                generation: 1,
                codec_version: CODEC_VERSION.to_owned(),
                app_doc_schema: 1,
                min_reader: 1,
                full_history: true,
                vv: Vec::new(),
                frontier: Vec::new(),
                loro_snapshot_bytes: Vec::new(),
            },
            actual_vv: VersionVector::new(),
            text: Some(String::new()),
        };
        let notes = verify_structure_semantics(&[doc, body], peer).expect("结构语义通过");
        assert_eq!(notes.get(&seed.note_id), Some(&seed.body_doc_id));
    }

    #[test]
    fn manifest_structure_doc_must_be_storage_constant() {
        let mut value = manifest(1, 100);
        value.structure_doc_id = "other".to_owned();
        let checkpoint = BootstrapCheckpoint {
            checkpoint_id: value.checkpoint_id,
            epoch: value.key_epoch,
            covered_seq: value.covered_relay_seq,
            manifest_oid: [8; 32],
            refs_digest: None,
            cursor: "c3".to_owned(),
            signed_commit: None,
            signature: None,
        };
        assert!(verify_manifest(&value, value.vault_id, &checkpoint, 1).is_err());
    }

    #[test]
    fn control_head_hash_binds_coordinates_and_chain() {
        let state = RelayState {
            vault_id: [1; 16],
            log_id: [2; 16],
            instance_id: [3; 16],
            epoch: 4,
            control_rev: 5,
            auth_rev: 1,
            head_seq: 10,
            gc_floor: 0,
            current_rotation_id: None,
        };
        let base = control_head_hash(&state, &[]);
        let mut other_instance = state.clone();
        other_instance.instance_id = [9; 16];
        assert_ne!(control_head_hash(&other_instance, &[]), base);
        let mut other_rev = state.clone();
        other_rev.control_rev = 6;
        assert_ne!(control_head_hash(&other_rev, &[]), base);
        let entry = ManagerChainEntry {
            rev: 2,
            pubkey: [7; 32],
            transition: vec![1, 2, 3],
            old_rev: 1,
            old_signature: Some([4; 64]),
            new_signature: None,
        };
        assert_ne!(control_head_hash(&state, &[entry]), base);
    }

    /// 单测用的临时 vault：lib 单测没有编译期 `CARGO_TARGET_TMPDIR`，回退到仓库 `target/tmp`。
    fn fresh_vault() -> (tempfile::TempDir, Vault) {
        let base = std::env::var_os("CARGO_TARGET_TMPDIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/tmp")
            });
        std::fs::create_dir_all(&base).expect("创建 target/tmp");
        let dir = tempfile::Builder::new()
            .prefix("bootstrap-gate-")
            .tempdir_in(&base)
            .expect("创建临时目录");
        let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开 vault");
        (dir, vault)
    }

    /// 基线 staging 的预期参与集合（本文件两个用例都只声明文档）。
    fn expectation(docs: &[&str]) -> BaselineExpectation {
        BaselineExpectation::new(
            docs.iter().map(|doc_id| (*doc_id).to_owned()),
            std::iter::empty::<[u8; 32]>(),
        )
    }

    /// 生成一份基线快照（另一个 peer 的空文档，只含给定正文）。
    fn baseline_snapshot(doc_id: &str, peer_id: u64, text: &str) -> Vec<u8> {
        let doc = BodyDoc::create(doc_id, peer_id).expect("创建基线 LoroDoc");
        doc.insert_scalars(0, text).expect("写基线正文");
        doc.export_snapshot().expect("导出基线 snapshot")
    }

    /// 在本地先提交一条正文 update（证明 abort 不会丢本地已 merge 的 op）。
    fn seed_local_body(vault: &Vault, doc_id: &str, peer_id: u64, text: &str) {
        create_body_doc(vault, doc_id).expect("建 loro_docs 行");
        let doc = BodyDoc::create(doc_id, peer_id).expect("创建本地 BodyDoc");
        doc.insert_scalars(0, text).expect("写本地正文");
        let head = load_head(vault, doc_id).expect("读本地 head");
        let base = head.version_vector().expect("解码 head_vv");
        let update = doc.pending_update(&base).expect("导出本地增量");
        append_updates(
            vault,
            doc_id,
            head.head_seq,
            &[UpdateRecord::new("local-merge", update)],
        )
        .expect("提交本地增量");
    }

    #[test]
    fn reconcile_journal_auto_aborts_bootstrap_staging_and_clears_marker() {
        let (_dir, vault) = fresh_vault();
        // 本地先有一份正文；随后 baseline 批次与它 merge（staging 里已有 merged 数据）。
        seed_local_body(&vault, "doc-merged", 0xA11CE, "local-merged|");
        begin_baseline_apply(
            &vault,
            &[1; 16],
            10,
            "bootstrap:relay-cursor-10",
            &expectation(&["doc-merged"]),
        )
        .expect("begin bootstrap staging");
        let applied = apply_baseline_batch(
            &vault,
            &[(
                "doc-merged".to_owned(),
                baseline_snapshot("doc-merged", 0xB0B, "baseline-merged"),
            )],
            &[],
        )
        .expect("merge baseline snapshot");
        assert_eq!(applied.applied_docs, 1, "前置条件：基线批次已 merge");
        mark_baseline_verify_failed(&vault, "读回校验失败：注入").expect("写失败标记");
        assert!(
            baseline_apply_state(&vault).expect("state").is_some(),
            "前置条件：staging journal 必须存在"
        );

        // 换新基线（checkpoint / S / source 全不同）：bootstrap staging 必须 auto-abort。
        reconcile_journal(&vault, &[2; 16], 20, "bootstrap:relay-cursor-20")
            .expect("bootstrap 换基线应 auto-abort 旧 staging");

        assert!(
            baseline_apply_state(&vault).expect("state").is_none(),
            "auto-abort 后 staging journal 必须清除"
        );
        assert_eq!(
            baseline_verify_failure(&vault).expect("读失败标记"),
            None,
            "auto-abort 必须同时清除 verify 失败标记"
        );
        // 已 merge 的 Loro 数据保留（abort 只清 journal，不回滚 CRDT merge）。
        let (merged, _) =
            load_body_doc(&vault, "doc-merged", 0xA11CE, None).expect("读回 merged 文档");
        let merged_text = merged.text();
        assert!(
            merged_text.contains("local-merged"),
            "auto-abort 不得丢本地已 merge 的 op，实际={merged_text:?}"
        );
        assert!(
            merged_text.contains("baseline-merged"),
            "auto-abort 不得回滚已 merge 的基线数据，实际={merged_text:?}"
        );
        // 新基线可以直接 begin（旧 journal/marker 不会挡住续跑）。
        let reopened = begin_baseline_apply(
            &vault,
            &[2; 16],
            20,
            "bootstrap:relay-cursor-20",
            &expectation(&["doc-merged"]),
        )
        .expect("auto-abort 后必须能以新基线重新 begin");
        assert_eq!(reopened.status, BaselineApplyStatus::Staging);
        assert_eq!(reopened.applied_docs, 0, "新基线应是全新 journal");
    }

    #[test]
    fn reconcile_journal_refuses_rotation_staging_without_abort() {
        // 轮换的 baseline apply 传空 source（rotation_apply.rs）；非 bootstrap 前缀同样拒绝。
        for source_cursor in ["", "rotation:custom"] {
            let (_dir, vault) = fresh_vault();
            begin_baseline_apply(
                &vault,
                &[3; 16],
                5,
                source_cursor,
                &expectation(&["doc-rotation"]),
            )
            .expect("begin rotation staging");
            mark_baseline_verify_failed(&vault, "轮换中的失败标记").expect("写失败标记");

            let error = reconcile_journal(&vault, &[4; 16], 9, "bootstrap:relay-cursor-9")
                .expect_err("不得 auto-abort 非 bootstrap 的 staging");
            match &error {
                SyncError::Storage(StorageError::SyncConflict { context }) => {
                    assert!(
                        context.contains("非 bootstrap"),
                        "错误必须点明拒绝原因，实际：{context}"
                    );
                }
                other => panic!("应当拒绝为 SyncConflict，实际 {other:?}"),
            }

            // journal 与失败标记都必须原样保留：绝不能被误当成 bootstrap 的 staging 清掉。
            let state = baseline_apply_state(&vault)
                .expect("state")
                .expect("轮换 journal 必须保留");
            assert_eq!(state.status, BaselineApplyStatus::Staging);
            assert_eq!(state.checkpoint_id, [3; 16]);
            assert_eq!(state.baseline_s, 5);
            assert_eq!(state.source_cursor, source_cursor);
            assert!(!state.finishable, "失败标记存在时不得 finishable");
            assert_eq!(
                baseline_verify_failure(&vault)
                    .expect("读失败标记")
                    .as_deref(),
                Some("轮换中的失败标记"),
                "拒绝 auto-abort 时绝不能误清失败标记"
            );
        }
    }
}

`````


## `crates/sync/src/dev_local.rs`

`````rust
//! ADR 0009 切片 4b：**GPUI-free** 的 dev-local 同步编排（Create / 发布初始基线 / Join / SyncOnce）。
//!
//! 本模块只做"把 `crates/sync` 已有的真实步骤按 ADR 0009 的顺序串起来"，不新增协议、
//! 不改 wire、不碰 UI/worker（切片 4c 才接 `SyncService`）。它存在的理由是：同样的
//! 编排写在 `examples/e2e_common.rs` 里只能给 example 用，而 app 的 dev-sync worker
//! 需要一个**可测、无 GPUI**的库入口。
//!
//! ## 编排
//!
//! 1. [`DevLocal::create`]：`POST /v1/vaults`（genesis）→ 新建本地 SQLite vault →
//!    落 `sync.relay_cursor`（relay 提供时）/ `sync.log_id` / 展示标签 /
//!    **初始 recovery 槽材料** → **上传初始 recovery 槽**（KEY_SLOT staging）并落
//!    「已上传」标记。槽必须在恢复码还在内存里的这一刻上传：KEY_SLOT 对象的 MNE1
//!    密钥 `K_slot_mne = HKDF(恢复码种子)` 不可逆，恢复码又不落盘，事后无法补传；
//! 2. 发布初始基线有两条入口，共用同一条核心路径（sync + 两份 checkpoint）：
//!    - [`DevLocal::publish_initial_pair`]（CLI）：校验恢复码与本地管理身份一致 →
//!      上传/重传初始 recovery 槽（幂等）→ 核心；
//!    - [`DevLocal::publish_initial_pair_with_identity`]（面板 worker）：**不接收恢复码**，
//!      信任锚是受保护身份里的 `manager_seed`（Join 设备没有 → 明确拒绝），初始槽以
//!      Create 落下的「已上传」标记为前提（旧实例明确失败并指向 CLI 路径）；
//!
//!    两条入口随后都跑一轮完整 `SyncOnce`（让覆盖证明只认 durable 事实）→ 用
//!    [`CheckpointWriter`] 发布**两份**真实同 epoch checkpoint（manifest/snapshot/
//!    asset/refs），并把最新一份落成 durable 事实；
//! 3. [`DevLocal::join`]：只接收 `vault_id + 恢复码 + 新路径`。`RecoverySeed →
//!    ManagementKey::from_recovery` → `recovery_enroll::self_enroll` 建管理信任锚并
//!    自批新设备 → `bootstrap::run_with_pins` 认证读取并导入基线 → pins/cursor 由
//!    bootstrap 落库，identity 只返回给调用方（**不读 A 的目录/identity/root**，
//!    也不在 vault 里写任何私钥或恢复码）；
//! 4. [`DevLocal::sync_once`]：构造借用型 [`SyncEngine`]，按 `encrypt_pending → push →
//!    pull → integrate → ack` 跑一轮，返回 [`SyncReport`]（游标由 engine 自己推进）；
//! 5. ADR 0010 切片 2a-2 的两个**只读/管理**入口，给 dev-only 设备列表 UI 用：
//!    [`DevLocal::members`]（已验证控制链的只读成员视图）与 [`DevLocal::revoke_device`]
//!    （管理密钥 + 管理端 helper 移除一台设备的访问，**不轮换密钥**、不删本地身份）。
//!
//! ## 凭据模型（dev-only）
//!
//! dev-local 只有**恢复码**一种凭据：`create` 不接收口令，因此 password 槽材料只存在于
//! 进程内、不上传、不落盘（口令槽 onboarding 属于切片 4c 的正式流程）。所以
//! `create` / [`DevLocal::publish_initial_pair`] 上传的是 **genesis 生成的那一个 recovery
//! 槽**（staging），Join 全程只靠恢复码解出根。
//!
//! 面板的「发布基线」走 [`DevLocal::publish_initial_pair_with_identity`]：它**不需要恢复码**
//! （恢复码在 Create 之后就不在进程里了），只要求身份里有管理私钥，且初始槽已由 Create
//! 上传过（[`INITIAL_SLOT_UPLOADED_META`]）。这是 ADR 0009 切片 4c-4 的取舍：为了让面板
//! 能一键发布，把"只有恢复码能做的事"（上传初始槽）前移到 Create，发布本身只做
//! "同步 + 两份 checkpoint"。
//!
//! ## 安全与非目标
//!
//! - relay 接入统一走 [`crate::relay_context::RelayContext`]：本模块的每个操作都从
//!   **同一个** context 取客户端/传输（不再各自拼 `StdTcpTransport`）；URL 兼容入口
//!   只是先构造一个 `local_dev` context。`remote_tls` 的 Create/Join 初始化在
//!   [`crate::remote_init`]（S3b），本模块保持 local_dev 语义。普通同步的
//!   生产 TLS 由 app 按公开配置的模式用 `remote_tls` context 打开；本模块不实现验签——测试用
//!   `menghunote-dev-relay`，它是**放行验签**的测试脚手架，**非生产**；正式鉴权负例
//!   必须走 relay 生产 hook；
//! - 不在这里写 app 的受保护身份文件（`DevIdentityStore` 负责 DPAPI/Linux 主钥封装）：
//!   identity 只经返回值交给调用方，绝不进 vault meta/明文配置/日志；
//! - 恢复码只以 `String` 形式交给调用方（ADR 0009：仅临时显示，提交后清除），本模块
//!   自身不打印、不落盘；`setup_secret` 只回显、不持久化；
//! - 初始 recovery 槽材料（描述符 + 包裹根密文）会写入 vault meta
//!   [`INITIAL_RECOVERY_SLOT_META`]：那是**已经要发到 relay 的密文**，不含根/恢复码明文；
//!   它让"发布基线"可重复且幂等（同一 slot_id/描述符，relay 只登记一次）。
//!
//! ## 失败可恢复性
//!
//! - `create` 是新建库：路径已存在直接拒绝；任何一步失败都**不留下**可被误用的本地
//!   vault（已创建的文件会被清掉）。relay 侧可能留下一个"没有槽、没有 checkpoint"
//!   的空 vault，它无法被任何设备使用，重试 `create` 会建新的；
//! - `publish_initial_pair` 先做恢复码一致性校验（错了在**任何上传之前**失败），再做
//!   槽上传/同步/两份 checkpoint；它是幂等的，失败后重跑会补齐（relay 只把最新的两份
//!   published 作为 `/bootstrap` 的候选）；
//! - `join` 失败不写 cursor/identity：bootstrap 只在全部导入通过后一个事务推进水位；
//!   失败时本模块清掉新建的本地文件（不在新目录留半初始化库），调用方重新 Join 即可。

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::checkpoint::{CheckpointReport, CheckpointWriter, StorageAssets};
use crate::device::{DeviceSigningKey, ManagementKey};
use crate::device_management::{RevokeContext, RevokeOutcome};
use crate::engine::{
    AckReport, AssetSyncReport, EngineConfig, EngineIdentity, IntegrateConfig, IntegrateReport,
    PullReport, PushReport, SyncEngine, SyncReport, RELAY_CURSOR_META,
};
use crate::error::SyncError;
use crate::genesis;
use crate::http_relay::{Clock, HttpRelayClient, MemberView, SystemClock};
use crate::http_transport::{HttpTransport, TransportTimeouts, DEFAULT_TRANSPORT_TIMEOUTS};
use crate::identity::LocalIdentityV1;
use crate::ids;
use crate::init_session::InitSession;
use crate::keys::RootSecretV1;
use crate::random::{array, OsRandom};
use crate::recovery::{management_seed, RecoverySeed};
use crate::relay::RelayClient as _;
use crate::relay_context::{RelayContext, RelayTarget};
use crate::slot::{KeySlotV1, WrappedRoot};
use crate::{DeviceId, VaultId};
use menghunote_storage::{
    ensure_structure_row, load_head, materialize_body, pending_outbox, set_sync_log_id, Vault,
    STRUCTURE_DOC_ID,
};
use menghunote_sync_proto::{SlotDescriptorV1, SlotKind};

// ADR 0009 Phase 2（切片 5）：轮换的库级入口与结果类型，与 Create/Join/SyncOnce 同层导出。
pub use crate::dev_local_rotation::{ApplyOutcome, RotationOutcome};

/// `meta` 键：dev-local 实例的展示标签（仅本机，非 wire；Join 不导入）。
pub const VAULT_LABEL_META: &str = "dev.local.vault_label";
/// `meta` 键：genesis 生成的初始 recovery 槽材料（描述符 CDE + `nonce||ciphertext`）。
///
/// **只含密文**：包裹根用 `HKDF(恢复码)` 派生 KEK 加 AEAD，槽体与上传到 relay 的
/// KEY_SLOT 明文逐字节相同；恢复码本身不落盘。写入后只由
/// [`DevLocal::upload_initial_slot`] 读取（复用同一 slot_id，relay 幂等）。
pub const INITIAL_RECOVERY_SLOT_META: &str = "dev.local.initial_recovery_slot.v1";
/// `meta` 键：初始 recovery 槽**已上传到 relay** 的 durable 事实（ADR 0009 切片 4c-4）。
///
/// 记录 `slot_id`（32B）+ 确定性 `message_id`（16B），两者都是公开事实，不含恢复码/根。
/// 它是"只凭身份发布基线"（[`DevLocal::publish_initial_pair_with_identity`]）的前置条件：
/// 上传 KEY_SLOT 需要的 `K_slot_mne` 只能由恢复码种子派生，恢复码不落盘 → 事后无法补传，
/// 所以缺这条事实的旧实例必须明确失败并指向 CLI 的恢复码路径（不静默发布一个 B 无法
/// Join 的假基线）。
pub const INITIAL_SLOT_UPLOADED_META: &str = "dev.local.initial_slot_uploaded.v1";
/// `meta` 键：最近一次发布基线的**较新**那份 checkpoint。
pub const PUBLISHED_BASELINE_META: &str = "dev.local.published_baseline.v1";

/// 初始 recovery 槽记录 magic（"MenghuNote Dev Local Slot v1"）。
const INITIAL_SLOT_MAGIC: &[u8; 8] = b"MN/DLS1\0";
/// 初始 recovery 槽「已上传」记录 magic。
const INITIAL_SLOT_UPLOADED_MAGIC: &[u8; 8] = b"MN/DLS2\0";
/// 已发布基线记录 magic。
const PUBLISHED_BASELINE_MAGIC: &[u8; 8] = b"MN/DLB1\0";
/// 槽描述符 CDE 的合理上限（`SlotDescriptorV1` 现在是 ~70B，留余量防止坏长度读爆内存）。
const MAX_SLOT_DESCRIPTOR_LEN: usize = 4096;
/// KEY_SLOT `message_id` 派生的域分隔（非秘密；换它等于换对象字节，会导致 409）。
const KEY_SLOT_MESSAGE_DOMAIN: &[u8] = b"MN/v1/dev-local/key-slot-message\0";

/// 一轮同步的可选注入项（ADR 0012 S1）：**传输预算** + **集成前守卫**。
///
/// 为什么需要它：`DevLocal` 过去自己构造 transport（超时预算接不进来），而且
/// `integrate` 一旦开始就没有"取消/授权复核"的边界。两者都必须由调用方（app 的
/// worker）决定，所以收进一个显式结构，而不是在 `DevLocal` 内部硬编。
///
/// - `transport = None`：保持历史默认（连接/读写 30s、无整次请求预算）；
/// - `before_integrate = None`：不做额外复核（CLI/测试路径）。
#[derive(Default)]
pub struct SyncOnceOptions<'a> {
    /// 传输超时预算；`None` = 历史默认（向后兼容）。
    pub transport: Option<TransportTimeouts>,
    /// `integrate` 之前最后一道门：返回 `Err` 即**不 append**（取消/授权已失效）。
    pub before_integrate: Option<&'a mut (dyn FnMut() -> Result<(), SyncError> + 'a)>,
}

/// dev-local 编排入口（纯函数集合，无状态）。
pub struct DevLocal;

/// `create` 的结果：本地 vault + 正式身份 + 恢复码。
///
/// **不实现 `Debug`/`Display`**：`identity` 与 `recovery_code` 都是秘密材料
/// （`LocalIdentityV1` 本身也不实现，避免误打印）。
pub struct CreatedVault {
    /// 新开的本地库（已建 structure 行、游标、log_id 与标签）。
    pub vault: Vault,
    /// genesis 身份（含管理私钥；app 由 `DevIdentityStore` 受保护落盘）。
    pub identity: LocalIdentityV1,
    /// 新 vault 的 id（Join 时展示/输入的公开标识）。
    pub vault_id: VaultId,
    /// 一次性展示给用户的恢复码（`mnrec1...`，**不得**写日志/配置）。
    pub recovery_code: String,
    /// 回显调用方传入的 setup secret（dev-only；**不得**落盘/写日志）。
    pub setup_secret: String,
}

/// `join` 的结果：新设备自己的 vault + 设备身份。
pub struct JoinedVault {
    /// 新开的本地库（已由 bootstrap 导入基线并落 pins/cursor）。
    pub vault: Vault,
    /// 新设备身份（`manager_seed = None`：普通设备没有管理私钥）。
    pub identity: LocalIdentityV1,
    /// 加入的 vault id。
    pub vault_id: VaultId,
}

/// [`DevLocal::published_baseline`] 读出的 durable 事实（本地 meta，非 wire）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublishedBaseline {
    /// 较新那份 checkpoint 的 id。
    pub checkpoint_id: [u8; 16],
    /// 该 checkpoint 的 epoch。
    pub epoch: u64,
    /// 该 checkpoint 固定的 S。
    pub covered_seq: u64,
    /// relay 记录的发布时间（Unix 秒）。
    pub published_at: u64,
}

/// [`DevLocal::revoke_device`] 的结果：relay 事实 + **刷新后**的成员视图（ADR 0010 切片 2a-2）。
///
/// 两个字段都不含秘密：`outcome` 只有目标设备/`changed`/`auth_rev`/`control_rev`；
/// `members` 是只读投影（`MemberView` 的 `Debug` 只打印 id/status/fingerprint）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevokedDevice {
    /// 管理端请求返回的事实（`control_rev` 不变、`auth_rev` 可能已前进）。
    pub outcome: RevokeOutcome,
    /// 吊销**之后**重新拉取的成员视图（已按 `device_id` 稳定升序）。
    pub members: Vec<MemberView>,
}

/// 两阶段同步 **Prepare** 阶段的报告（encrypt/push/pull；不含 integrate/ack）。
#[derive(Debug, Default)]
pub struct PrepareReport {
    /// 本轮新加密的本地意图数。
    pub encrypted: usize,
    /// 推送结果。
    pub push: PushReport,
    /// 增量资产上传结果（正文 push 之后、pull 之前）。
    pub assets: AssetSyncReport,
    /// 拉取结果（游标已 durable 推进）；切换为 checkpoint 追赶时为默认值。
    pub pull: PullReport,
    /// Phase 1B：已准备好、等待持闸的追赶候选；`None` = 本轮走普通聚合 pull。
    pub catchup: Option<crate::catchup::CatchUpPrepared>,
}

/// 两阶段同步 **Integrate** 阶段的报告。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IntegratePhaseReport {
    /// 集成结果（integrated/deferred/批次/是否投影）。
    pub integrate: IntegrateReport,
    /// 本阶段 head **真的前进**的文档（结构 + 正文；UI 据此判断交付/刷新）。
    pub changed_heads: Vec<String>,
    /// `watch_doc` 是否真的前进（= 需要交付）。
    pub watch_doc_changed: bool,
    /// 本阶段按 durable outbox 触发物化的正文篇数（changed heads → 物化/索引更新）。
    pub materialized: usize,
    /// 物化失败篇数（失败只记日志，不影响已 durable 的 CRDT 集成结论）。
    pub materialize_failed: usize,
    /// Phase 1B：本轮是否执行了 checkpoint 追赶。
    pub catchup: Option<crate::catchup::CatchUpReport>,
}

/// 两阶段同步 **Finish** 阶段的报告（push tail + ack）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FinishReport {
    /// 上报的 received/integrated 游标。
    pub ack: AckReport,
    /// tail 阶段新加密的本地意图数。
    pub encrypted: usize,
    /// tail 推送结果。
    pub push: PushReport,
}

impl DevLocal {
    /// Create：对真实 relay 走 genesis，并新建（**必须是新路径**）本地 vault。
    ///
    /// 初始化走 ADR 0013 S2b 的受限能力 [`InitSession::new_genesis`]：
    ///
    /// 1. 先在本地生成 vault/device **材料**（不联网），再开新库并写 durable pending
    ///    （`sync.init_pending`）——顺序是"先记录意图、再联网"；
    /// 2. `POST /v1/vaults`（写前先推进 `GenesisRequested`）；成功后**不重复**创建；
    /// 3. 用本地提交证书复算 rev1 `record_hash`，验证 relay 当前链/前缀，再 durable
    ///    提交强 pin（`sync.control_pin`，rev1 record 锚）；
    /// 4. 写本地事实（structure/label/cursor/log_id/初始槽材料）→ 上传初始 recovery 槽
    ///    → pending 进入 `Done` 并清理（app 走 [`Self::create_deferring_identity`]：
    ///    停在 `PinCommitted`，身份 rename 成功后再推进 `IdentityWritten → Done`）。
    ///
    /// 失败语义沿用 ADR 0009（app 验收依赖它）：**任何返回错误都清掉本次新建的库文件**
    /// （relay 侧可能留下一个没有槽/没有 checkpoint 的空 vault，重试会建新的）；
    /// pending 的用途是**崩溃**（进程消失、清理代码没跑）之后的对账，见
    /// [`crate::init_session::reconcile`]。
    ///
    /// 口令：dev-local 不接收口令，genesis 的 password 槽材料用一次随机口令生成后**丢弃**
    /// （不上传、不落盘），因此该 vault 只能靠恢复码加入——这是 ADR 0009 Phase 1 的
    /// dev-only 取舍，口令 onboarding 在切片 4c。
    pub fn create(
        relay_url: &str,
        setup_secret: &str,
        vault_path: &Path,
        vault_label: &str,
    ) -> Result<CreatedVault, SyncError> {
        Self::create_inner(relay_url, setup_secret, vault_path, vault_label, true)
    }

    /// **app 专用**：与 [`Self::create`] 完全相同，但 pending 停在 `PinCommitted`。
    ///
    /// 调用方必须在身份**原子落盘**（`DevIdentityStore::persist_protected` 的临时文件 +
    /// rename）之后调用 [`crate::init_session::mark_identity_written`] 与
    /// `finish_identity_written`。这样"relay 已建 vault、本地身份还没落盘"的崩溃窗口
    /// 有 durable pending 可对账（ADR 0013 §2/S3），而不是靠重跑 genesis。
    ///
    /// 失败语义与 [`Self::create`] 一致：返回错误时清掉本次新建的本地文件。
    pub fn create_deferring_identity(
        relay_url: &str,
        setup_secret: &str,
        vault_path: &Path,
        vault_label: &str,
    ) -> Result<CreatedVault, SyncError> {
        Self::create_inner(relay_url, setup_secret, vault_path, vault_label, false)
    }

    fn create_inner(
        relay_url: &str,
        setup_secret: &str,
        vault_path: &Path,
        vault_label: &str,
        finish_identity: bool,
    ) -> Result<CreatedVault, SyncError> {
        if vault_path.as_os_str().is_empty() {
            return Err(SyncError::LocalCorrupt("Create 需要明确的 vault 路径"));
        }
        if vault_path.exists() {
            return Err(SyncError::LocalCorrupt(
                "Create 只接受尚不存在的 vault 路径（已有库请按其身份使用）",
            ));
        }

        // 0. 按唯一权威策略构造 context（local_dev 只接受 http + loopback 白名单）。
        let context = RelayContext::local_dev(relay_url, DEFAULT_TRANSPORT_TIMEOUTS)?;

        // 1. 本地材料：vault_id/device_id 必须**先进 pending**，之后才能发请求。
        let vault_id: VaultId = array(&mut OsRandom)?;
        let device_id: DeviceId = array(&mut OsRandom)?;
        let throwaway_password = throwaway_password()?;
        let materials = genesis::build_materials(
            vault_id,
            device_id,
            &throwaway_password,
            SystemClock.now_secs(),
            &mut OsRandom,
        )?;
        let session = InitSession::new_genesis(context, &materials)?;
        let context = session.context().clone();

        // 2. 新库（pending 需要 SQLite；genesis 失败时会连同 pending 一起清掉）。
        let vault = match Vault::open(vault_path) {
            Ok(vault) => vault,
            Err(error) => {
                remove_vault_files(vault_path);
                return Err(SyncError::Storage(error));
            }
        };
        let created = (|| -> Result<(LocalIdentityV1, String), SyncError> {
            session.prepare_pending(&vault, SystemClock.now_secs())?;
            let receipt = session.request_genesis(&vault, &materials, setup_secret)?;
            let report = session.finalize_genesis(&vault, &materials, &receipt)?;

            ensure_structure_row(&vault)?;
            vault.set_meta(VAULT_LABEL_META, vault_label.as_bytes())?;
            // 初始游标：genesis 响应键 5（旧 relay 可能没有 —— 那就留空，publish 时用
            // `GET /state` 键 14 兜底，且绝不把"缺失"当成 0 号游标）。
            if let Some(cursor) = &receipt.initial_cursor {
                vault.set_meta(RELAY_CURSOR_META, cursor.as_bytes())?;
            }
            // log_id 来自 genesis 响应：先钉住 relay 日志代，pull 再复核。
            set_sync_log_id(&vault, &receipt.log_id)?;
            // 初始 recovery 槽材料（密文）留给发布/Join 复用（同一 slot_id 幂等）。
            let slot_record = encode_initial_slot(&materials.recovery_slot)?;
            vault.set_meta(INITIAL_RECOVERY_SLOT_META, &slot_record)?;

            // 初始 recovery 槽必须**现在**上传（ADR 0009 切片 4c-4）：KEY_SLOT 对象的 MNE1
            // 密钥 `K_slot_mne = HKDF(恢复码种子)` 不可逆，而恢复码只在这一刻还在内存里、不落盘。
            // 这一步是初始化内部操作（`open_for_init` 跳过 pending 守卫）。
            Self::upload_initial_slot_during_init(
                &context,
                &vault,
                &report.identity,
                &materials.recovery_seed,
            )?;

            // 本地初始化完成：pending → Done → 清理（清理只在 Done）。
            // app 的延迟路径停在 PinCommitted，等身份 rename 成功后由调用方推进。
            if finish_identity {
                session.finish(&vault)?;
            }
            Ok((report.identity, materials.recovery_code.as_str().to_owned()))
        })();
        match created {
            Ok((identity, recovery_code)) => Ok(CreatedVault {
                vault,
                vault_id: identity.vault_id,
                recovery_code,
                identity,
                setup_secret: setup_secret.to_owned(),
            }),
            Err(error) => {
                drop(vault);
                remove_vault_files(vault_path);
                Err(error)
            }
        }
    }

    /// CLI 路径的发布初始基线：**用恢复码**确认管理身份，然后与身份路径共用同一条核心。
    ///
    /// 步骤（顺序不可交换）：
    /// 1. 恢复码必须与本地管理身份一致（`HKDF(R) == identity.manager_seed`）——错了在
    ///    任何上传之前失败，避免用一个错码把 KEY_SLOT 密文写坏；
    /// 2. 上传 genesis 的 recovery 槽（staging；`PUT key-slots` 对同 slot_id 同描述符幂等；
    ///    新实例在 Create 时已上传过一次，这里只是补齐旧实例或灾后重传）并落 durable 标记；
    /// 3. 核心：一轮完整 `SyncOnce`（覆盖证明只认 durable 事实）+ 连续两份同 epoch
    ///    checkpoint（见 [`Self::publish_baseline_core_with`]）。
    ///
    /// 这条入口保留给 CLI（`--dev-sync-publish`）、自动化与恢复场景；面板按钮走
    /// [`Self::publish_initial_pair_with_identity`]（不需要恢复码）。
    pub fn publish_initial_pair<T: RelayTarget + ?Sized>(
        target: &T,
        vault: &Vault,
        identity: &LocalIdentityV1,
        recovery_code: &str,
    ) -> Result<CheckpointReport, SyncError> {
        let seed = RecoverySeed::from_code(recovery_code)?;
        verify_recovery_matches(identity, &seed)?;
        // 预算取历史默认（本入口没有 per-call 预算参数）；显式 context 用自己的预算。
        let context = target.relay_context(None)?;
        Self::upload_initial_slot(&context, vault, identity, &seed)?;
        Self::publish_baseline_core_with(&context, vault, identity, SyncOnceOptions::default())
    }

    /// **只凭受保护身份**发布初始基线（ADR 0009 切片 4c-4；面板「发布基线」的库入口）。
    ///
    /// 与 [`Self::publish_initial_pair`] 的差别只有"信任锚 + 初始槽"两步：
    ///
    /// - **不接收恢复码**：管理私钥（`identity.manager_seed`）必须存在，且派生出的管理公钥
    ///   必须等于身份里 pin 住的 `pinned_manager`。Join 设备没有管理私钥 → 明确错误，
    ///   绝不退化成"用设备 key 冒充管理"；
    /// - 初始 recovery 槽由 Create 上传（恢复码当时还在内存里），这里只做 fail-closed
    ///   校验：槽材料与身份/库自洽 + [`INITIAL_SLOT_UPLOADED_META`] 存在。旧实例（建库时
    ///   没传过槽）明确失败并指向 CLI 的恢复码路径——**绝不**静默发布一份 B 无法 Join 的
    ///   假基线；
    /// - 其余（sync + 两份 checkpoint + durable 基线事实）与 CLI 路径**逐字节同一条**
    ///   核心路径（[`Self::publish_baseline_core_with`]），不复制逻辑。
    ///
    /// 幂等语义同 4b：重跑会多一对真实 checkpoint，`/bootstrap` 只取最新两份 published。
    pub fn publish_initial_pair_with_identity<T: RelayTarget + ?Sized>(
        target: &T,
        vault: &Vault,
        identity: &LocalIdentityV1,
    ) -> Result<CheckpointReport, SyncError> {
        Self::publish_initial_pair_with_identity_with(
            target,
            vault,
            identity,
            SyncOnceOptions::default(),
        )
    }

    /// 同 [`Self::publish_initial_pair_with_identity`]，但接收**传输预算 + 集成前守卫**
    /// （ADR 0012 S1：发布内部也跑一轮同步并 `integrate`，所以与同步共用同一条安全纪律）。
    pub fn publish_initial_pair_with_identity_with<T: RelayTarget + ?Sized>(
        target: &T,
        vault: &Vault,
        identity: &LocalIdentityV1,
        options: SyncOnceOptions<'_>,
    ) -> Result<CheckpointReport, SyncError> {
        // 先拒 Join 设备（真正原因），再看槽材料/上传标记（保持既有错误优先级：
        // 本地可判定原因先于 URL/网络配置错误）。
        require_manager_identity(identity)?;
        let slot = load_initial_slot(vault, &identity.vault_id)?;
        verify_identity_slot_consistency(identity, &slot)?;
        if !initial_slot_uploaded(vault, &slot)? {
            return Err(SyncError::Unsupported(
                "本实例建库时没有上传初始 recovery 槽（旧版本 Create）：面板无法补传\
                 （K_slot_mne 只能由恢复码派生）；请先用 --dev-sync-publish 与恢复码补一次，\
                 之后面板「发布基线」一直可用",
            ));
        }
        let context = target.relay_context(options.transport)?;
        Self::publish_baseline_core_with(&context, vault, identity, options)
    }

    /// 上传 genesis 的初始 recovery 槽（staging）并落 durable 标记。
    ///
    /// **只有恢复码在内存里的调用点能用**：`create`（genesis 刚返回）与
    /// [`Self::publish_initial_pair`]（CLI，恢复码来自环境变量）。KEY_SLOT 对象的 MNE1
    /// 密钥 `K_slot_mne = HKDF(恢复码种子)` 不可逆，恢复码又不落盘，所以事后（只有
    /// `manager_seed` 时）无法补传——这是 ADR 0009 切片 4c-4 把槽上传前移的根因。
    ///
    /// message id 确定性派生，整份 KEY_SLOT 对象逐字节可复现，重传 relay 幂等接受。
    fn upload_initial_slot(
        context: &RelayContext,
        vault: &Vault,
        identity: &LocalIdentityV1,
        seed: &RecoverySeed,
    ) -> Result<(), SyncError> {
        Self::upload_initial_slot_with(context, vault, identity, seed, false)
    }

    /// Create 内部的初始槽上传：pending 尚未收尾，走 `open_for_init`（跳过 pending 守卫）。
    pub(crate) fn upload_initial_slot_during_init(
        context: &RelayContext,
        vault: &Vault,
        identity: &LocalIdentityV1,
        seed: &RecoverySeed,
    ) -> Result<(), SyncError> {
        Self::upload_initial_slot_with(context, vault, identity, seed, true)
    }

    fn upload_initial_slot_with(
        context: &RelayContext,
        vault: &Vault,
        identity: &LocalIdentityV1,
        seed: &RecoverySeed,
        during_init: bool,
    ) -> Result<(), SyncError> {
        let slot = load_initial_slot(vault, &identity.vault_id)?;
        verify_identity_slot_consistency(identity, &slot)?;

        let root = root_of(identity);
        let signing_key = DeviceSigningKey::from_seed(identity.ed25519_seed.clone());
        let pinned = if during_init {
            context.open_for_init(vault, identity)?
        } else {
            context.open(vault, identity)?
        };
        let mut client = pinned.into_client();
        ensure_cursor(&mut client, vault)?;
        let message_id = key_slot_message_id(&slot)?;
        {
            let mut writer = CheckpointWriter::new(
                vault,
                &mut client,
                identity.vault_id,
                identity.device_id,
                signing_key,
                root,
                identity.loro_peer_id(),
            );
            writer.upload_recovery_slot(&slot, seed, message_id)?;
        }
        write_initial_slot_uploaded(vault, &slot.descriptor.slot_id, &message_id)
    }

    /// 发布核心：一轮完整 `SyncOnce` + 两份真实同 epoch checkpoint（两条入口共用）。
    ///
    /// - 一轮完整 `SyncOnce`：覆盖证明只认 durable 事实（integrated 前缀/skipped-asset
    ///   记录），所以本地刚建好的笔记必须先推送到 relay 并拉回 integrate；
    /// - 连续两份同 epoch checkpoint（`CheckpointWriter` 的 prepare→validate→promote）：
    ///   `/bootstrap` 只按 `published_at DESC` 取最新两份 published，所以**必须**成对发布。
    ///
    /// 资产源用 [`StorageAssets`]（真实 storage 资产；空库自然是 0 个资产），不用合成
    /// fixture。返回值是**较新那份** checkpoint 的报告（pair = 前一份 + 这一份）。
    fn publish_baseline_core_with(
        context: &RelayContext,
        vault: &Vault,
        identity: &LocalIdentityV1,
        mut options: SyncOnceOptions<'_>,
    ) -> Result<CheckpointReport, SyncError> {
        let root = root_of(identity);
        let signing_key = DeviceSigningKey::from_seed(identity.ed25519_seed.clone());
        let mut client = context.open(vault, identity)?.into_client();
        // 缺 `sync.relay_cursor` 时用 `GET /state` 键 14 兜底（旧 relay 兼容）。
        ensure_cursor(&mut client, vault)?;

        // 1) 发布前把 durable 数据推/拉平（错误一律向上传播：暂停=不得发布基线）。
        //    集成前守卫与同步共用一条：取消/授权已失效时**不 append**。
        {
            let engine_identity = EngineIdentity {
                device_id: identity.device_id,
                signing_key: signing_key.clone(),
                peer_id: identity.loro_peer_id(),
            };
            let mut engine = SyncEngine::new(vault, &mut client, engine_identity, root.clone());
            match options.before_integrate.as_mut() {
                Some(guard) => engine.sync_once_with_guard(&mut **guard)?,
                None => engine.sync_once()?,
            };
        }

        // 2) 两份真实 checkpoint（prepare→validate→promote 全部真实读回/导入）。
        let mut writer = CheckpointWriter::new(
            vault,
            &mut client,
            identity.vault_id,
            identity.device_id,
            signing_key,
            root,
            identity.loro_peer_id(),
        );
        let _first = writer.create_checkpoint(&StorageAssets)?;
        let second = writer.create_checkpoint(&StorageAssets)?;
        write_published_baseline(vault, &second)?;
        Ok(second)
    }

    /// Join：只给 `vault_id + 恢复码 + 新路径`，不读 A 的目录/identity/root。
    ///
    /// 初始化走 ADR 0013 S2b 的受限能力 [`InitSession::new_fresh_recovery`]：
    ///
    /// 1. 恢复码 → 恢复种子 →（会话内）管理私钥（管理信任锚）；
    /// 2. 新库 + durable pending（`Prepared`）→ 写前推进 `GenesisRequested` → `self_enroll`
    ///    自批新设备（唯一允许 unpinned 消费 `/control` 的路径，仍强制管理锚 + 逐条签名）；
    /// 3. 认证 `bootstrap::run_with_pins` 导入基线并单事务推进 pins/cursor；
    /// 4. 用**同一验证视图**提交 record pin + bootstrap 锚（`commit_recovery_pin`）；
    /// 5. pending → `Done` → 清理（清理只在 Done）；app 走
    ///    [`Self::join_deferring_identity`] 时停在 `PinCommitted`，身份落盘后再推进。
    ///
    /// 失败语义沿用 ADR 0009（app 验收依赖它）：**返回错误时清掉本次新建的本地文件**，
    /// 不写 cursor/identity（bootstrap 只在全部导入成功后推进水位）；pending 的用途是
    /// **崩溃**之后的启动对账（[`crate::init_session::reconcile`]）。
    pub fn join(
        relay_url: &str,
        vault_id: &VaultId,
        recovery_code: &str,
        vault_path: &Path,
    ) -> Result<JoinedVault, SyncError> {
        Self::join_inner(relay_url, vault_id, recovery_code, vault_path, true)
    }

    /// **app 专用**：与 [`Self::join`] 相同，但 pending 停在 `PinCommitted`。
    ///
    /// 与 [`Self::create_deferring_identity`] 对称：身份原子落盘后由调用方调用
    /// [`crate::init_session::mark_identity_written`] + `finish_identity_written`，
    /// 崩溃窗口由启动对账（[`crate::init_session::reconcile`]）补 finalize。
    pub fn join_deferring_identity(
        relay_url: &str,
        vault_id: &VaultId,
        recovery_code: &str,
        vault_path: &Path,
    ) -> Result<JoinedVault, SyncError> {
        Self::join_inner(relay_url, vault_id, recovery_code, vault_path, false)
    }

    fn join_inner(
        relay_url: &str,
        vault_id: &VaultId,
        recovery_code: &str,
        vault_path: &Path,
        finish_identity: bool,
    ) -> Result<JoinedVault, SyncError> {
        if vault_path.as_os_str().is_empty() {
            return Err(SyncError::LocalCorrupt("Join 需要明确的 vault 路径"));
        }
        if vault_path.exists() {
            return Err(SyncError::LocalCorrupt(
                "Join 只接受尚不存在的 vault 路径（已有库请按其身份使用）",
            ));
        }

        // 1. 恢复码 → 恢复种子；管理锚由 InitSession 从种子推导（不信任 relay 的公钥）。
        let seed = RecoverySeed::from_code(recovery_code)?;
        let context = RelayContext::local_dev(relay_url, DEFAULT_TRANSPORT_TIMEOUTS)?;
        let session = InitSession::new_fresh_recovery(context, *vault_id, &seed)?;

        // 2. 新库（pending 需要 SQLite；任何失败都清掉刚建的文件）。
        let vault = match Vault::open(vault_path) {
            Ok(vault) => vault,
            Err(error) => {
                remove_vault_files(vault_path);
                return Err(SyncError::Storage(error));
            }
        };
        let joined = (|| -> Result<LocalIdentityV1, SyncError> {
            session.prepare_pending(&vault, SystemClock.now_secs())?;
            let recovered = session.recover_identity(&vault, SystemClock.now_secs())?;
            session.commit_recovery_pin(&vault, &recovered.identity)?;
            if finish_identity {
                session.finish(&vault)?;
            }
            Ok(recovered.identity)
        })();

        match joined {
            Ok(identity) => Ok(JoinedVault {
                vault,
                identity,
                vault_id: *vault_id,
            }),
            Err(error) => {
                // 失败不留下半初始化库（cursor/identity 都没写；文件也清掉，重试从头来）。
                drop(vault);
                remove_vault_files(vault_path);
                Err(error)
            }
        }
    }

    /// SyncOnce：一轮完整同步（`encrypt_pending → push → pull → integrate → ack`）。
    ///
    /// [`SyncEngine`] 是借用型的，每次调用内构造；游标/水位由 engine 自己按 durable 事实推进。
    ///
    /// 这是**向后兼容入口**（CLI/测试/旧调用）：传输预算与集成前守卫都用历史默认
    /// （ADR 0012 S1）。需要可配预算或取消复核时用 [`Self::sync_once_with`]。
    pub fn sync_once<T: RelayTarget + ?Sized>(
        target: &T,
        vault: &Vault,
        identity: &LocalIdentityV1,
    ) -> Result<SyncReport, SyncError> {
        Self::sync_once_with(target, vault, identity, SyncOnceOptions::default())
    }

    /// 同 [`Self::sync_once`]，但由调用方注入**传输预算**与**集成前守卫**（ADR 0012 S1）。
    ///
    /// `options.before_integrate` 在 `integrate` 之前调用一次：返回 `Err` 时本轮
    /// **不会追加任何本地行**（取消/授权已失效），已推送的对象与已拉取的 inbox 保持
    /// durable 事实（重试可复用）。进入 integrate 后事务必须跑完，不再中断。
    pub fn sync_once_with<T: RelayTarget + ?Sized>(
        target: &T,
        vault: &Vault,
        identity: &LocalIdentityV1,
        mut options: SyncOnceOptions<'_>,
    ) -> Result<SyncReport, SyncError> {
        let context = target.relay_context(options.transport)?;
        let root = root_of(identity);
        let signing_key = DeviceSigningKey::from_seed(identity.ed25519_seed.clone());
        let mut client = context.open(vault, identity)?.into_client();
        // pull 必须有游标：B 由 bootstrap 落库，A 由 genesis 落库；旧 relay 两者都没有时
        // 用 `GET /state` 键 14 兜底（绝不把"缺失"当成 0 号游标硬编）。
        ensure_cursor(&mut client, vault)?;
        // Phase 1B：完整 sync_once 也先尝试 checkpoint 追赶；失败不回退普通 pull
        // （Mandatory 时普通 pull 只会再次 CursorExpired）。追赶在调用方的
        // `before_integrate` 守卫之后写入，语义与普通 integrate 相同。
        let pins = crate::bootstrap::BootstrapPins::from_vault(vault)?;
        let catchup_config = crate::catchup::CatchUpConfig::default();
        let local = menghunote_storage::sync_cursor_state(vault)?;
        let state = client.state()?;
        if crate::catchup::should_attempt(&state, local.received_seq, &catchup_config) {
            let prepared = crate::catchup::prepare(
                vault,
                &mut client,
                root.vault_id,
                identity.loro_peer_id(),
                &root,
                &pins,
                &catchup_config,
            )?;
            if let Some(prepared) = prepared {
                if let Some(guard) = options.before_integrate.as_mut() {
                    guard()?;
                }
                crate::catchup::integrate(vault, identity.loro_peer_id(), &prepared)?;
            }
        }
        let engine_identity = EngineIdentity {
            device_id: identity.device_id,
            signing_key,
            peer_id: identity.loro_peer_id(),
        };
        let mut engine = SyncEngine::new(vault, &mut client, engine_identity, root);
        match options.before_integrate.as_mut() {
            Some(guard) => engine.sync_once_with_guard(&mut **guard),
            None => engine.sync_once(),
        }
    }

    // ------------------------------------------------------------------
    // ADR 0012 S2a：可暂停的两阶段同步（Prepare / Integrate / Finish）
    // ------------------------------------------------------------------
    //
    // 三个入口各自**重新构造** client/engine：真正的事实只存在 DB（outbox/inbox/
    // cursor/pins），不依赖进程内 engine。调用方（app 的 worker）负责在阶段之间等待
    // 授权/交付回执；本模块只保证每一步"只做这一阶段的事"：
    //
    // - `sync_prepare`：encrypt → push → pull（**不** integrate、**不** ack）；
    // - `sync_integrate`：integrate（外加授权 head 复核），**不** ack；
    // - `sync_finish`：push tail（若有）→ ack。

    /// 两阶段同步的 **Prepare** 阶段：加密待发送意图 → 推送 → 拉取一页。
    ///
    /// 明确**不**做 `integrate`/`ack`：本阶段只把远端变化落成 durable inbox 并推进
    /// pull 游标，正文/结构 head 一律不动。因此 UI 在网络阶段不需要持闸、可以继续编辑。
    pub fn sync_prepare<T: RelayTarget + ?Sized>(
        target: &T,
        vault: &Vault,
        identity: &LocalIdentityV1,
        transport: Option<TransportTimeouts>,
    ) -> Result<PrepareReport, SyncError> {
        let context = target.relay_context(transport)?;
        let root = root_of(identity);
        let signing_key = DeviceSigningKey::from_seed(identity.ed25519_seed.clone());
        let mut client = context.open(vault, identity)?.into_client();
        ensure_cursor(&mut client, vault)?;
        // Phase 1B：网络阶段先决策/下载 checkpoint；不写库、不持闸。
        // 若决策为 None，继续普通 push + pull（快照可能已下载但未写，属于保守的
        // 机会性误判；Mandatory 命中时必须跳过普通 pull，否则会立即 CursorExpired）。
        let pins = crate::bootstrap::BootstrapPins::from_vault(vault)?;
        let catchup_config = crate::catchup::CatchUpConfig::default();
        // 近端同步不下载快照：只有强制过期或落后上界达到 N_min 才进入 prepare。
        let catchup = {
            let local = menghunote_storage::sync_cursor_state(vault)?;
            let state = client.state()?;
            if crate::catchup::should_attempt(&state, local.received_seq, &catchup_config) {
                crate::catchup::prepare(
                    vault,
                    &mut client,
                    root.vault_id,
                    identity.loro_peer_id(),
                    &root,
                    &pins,
                    &catchup_config,
                )?
            } else {
                None
            }
        };
        let engine_identity = EngineIdentity {
            device_id: identity.device_id,
            signing_key,
            peer_id: identity.loro_peer_id(),
        };
        let mut engine = SyncEngine::new(vault, &mut client, engine_identity, root);
        let config = EngineConfig::default();
        let encrypted = engine.encrypt_pending(config.send_batch)?;
        let push = engine.push()?;
        // ADR 0012 两阶段路径的资产上传点：与完整 `sync_once` 同一阶段（正文之后、
        // pull 之前），保证自动同步（S3）也会增量上传新资产。
        let assets = engine.upload_pending_assets()?;
        let pull = if catchup.is_some() {
            PullReport::default()
        } else {
            engine.pull()?
        };
        Ok(PrepareReport {
            encrypted,
            push,
            assets,
            pull,
            catchup,
        })
    }

    /// 两阶段同步的 **Integrate** 阶段：把 durable verified inbox 集成进本地库。
    ///
    /// `expect_head` 是调用方（UI 持闸）给出的授权基准：`(doc_id, durable_head_seq)`。
    /// 任一条不满足就返回 [`SyncError::CancelledBeforeIntegrate`]，**不 append**：
    ///
    /// - 库中该文档 head 已被移动（授权失效 / 本地还有未持闸写入）；
    /// - 调用方在进入本阶段前已复核取消标志（见 app worker：先查 `OpControl`，再调本函数）。
    ///
    /// `watch_doc` 是 UI 准备 adopt 的活动文档：报告里会给出它的 head 是否在本阶段前进了，
    /// 供 UI 判定"是否需要交付"。**不** ack（ack 属于 Finish，等 UI adopt/end 之后）。
    ///
    /// 集成本身按 ADR 0012 S2b 的**依赖顺序**收敛（结构 → 只投影 → 正文公平轮转），
    /// 因此调用方不需要"再联网补一轮"来修结构/正文依赖。
    pub fn sync_integrate<T: RelayTarget + ?Sized>(
        target: &T,
        vault: &Vault,
        identity: &LocalIdentityV1,
        transport: Option<TransportTimeouts>,
        expect_head: Option<(&str, u64)>,
        watch_doc: &str,
    ) -> Result<IntegratePhaseReport, SyncError> {
        Self::sync_integrate_with_config(
            target,
            vault,
            identity,
            transport,
            expect_head,
            watch_doc,
            IntegrateConfig::default(),
        )
    }

    /// 同 [`Self::sync_integrate`]，但携带 Prepare 阶段准备好的 checkpoint 追赶候选。
    ///
    /// 追赶写入/水位/游标在持闸段完成；`integrated >= S` 后清理已覆盖 inbox。
    pub fn sync_integrate_with_catchup<T: RelayTarget + ?Sized>(
        target: &T,
        vault: &Vault,
        identity: &LocalIdentityV1,
        transport: Option<TransportTimeouts>,
        expect_head: Option<(&str, u64)>,
        watch_doc: &str,
        catchup: Option<&crate::catchup::CatchUpPrepared>,
    ) -> Result<IntegratePhaseReport, SyncError> {
        Self::sync_integrate_with_config_and_catchup(
            target,
            vault,
            identity,
            transport,
            expect_head,
            watch_doc,
            IntegrateConfig::default(),
            catchup,
        )
    }

    /// 同 [`Self::sync_integrate`]，但显式指定集成批次预算（测试/诊断用）。
    #[allow(clippy::too_many_arguments)]
    pub fn sync_integrate_with_config<T: RelayTarget + ?Sized>(
        target: &T,
        vault: &Vault,
        identity: &LocalIdentityV1,
        transport: Option<TransportTimeouts>,
        expect_head: Option<(&str, u64)>,
        watch_doc: &str,
        integrate_config: IntegrateConfig,
    ) -> Result<IntegratePhaseReport, SyncError> {
        Self::sync_integrate_with_config_and_catchup(
            target,
            vault,
            identity,
            transport,
            expect_head,
            watch_doc,
            integrate_config,
            None,
        )
    }

    /// 同 [`Self::sync_integrate_with_config`]，可附加 checkpoint 追赶。
    #[allow(clippy::too_many_arguments)]
    pub fn sync_integrate_with_config_and_catchup<T: RelayTarget + ?Sized>(
        target: &T,
        vault: &Vault,
        identity: &LocalIdentityV1,
        transport: Option<TransportTimeouts>,
        expect_head: Option<(&str, u64)>,
        watch_doc: &str,
        integrate_config: IntegrateConfig,
        catchup: Option<&crate::catchup::CatchUpPrepared>,
    ) -> Result<IntegratePhaseReport, SyncError> {
        // Integrate 前复核授权基准（ADR 0012 S2b）：head 被推进 = 授权已失效，
        // 一律不 append；inbox/outbox 原样保留，调用方可按新授权重试。
        if let Some((doc_id, expected)) = expect_head {
            let head = load_head(vault, doc_id)?;
            if head.head_seq != expected {
                return Err(SyncError::CancelledBeforeIntegrate);
            }
        }
        let before = load_head(vault, watch_doc)?.head_seq;
        let peer_id = identity.loro_peer_id();
        let catchup_report = match catchup {
            Some(prepared) => match crate::catchup::integrate(vault, peer_id, prepared) {
                Ok(report) => Some(report),
                Err(error) => {
                    if prepared.decision().mode == crate::catchup::CatchUpMode::Mandatory {
                        eprintln!("[sync] mandatory catchup integrate failed: {error}");
                        return Err(SyncError::Paused(crate::relay::PauseReason::CatchupFailed));
                    }
                    return Err(error);
                }
            },
            None => None,
        };
        let context = target.relay_context(transport)?;
        let root = root_of(identity);
        let signing_key = DeviceSigningKey::from_seed(identity.ed25519_seed.clone());
        let mut client = context.open(vault, identity)?.into_client();
        ensure_cursor(&mut client, vault)?;
        let engine_identity = EngineIdentity {
            device_id: identity.device_id,
            signing_key,
            peer_id,
        };
        let mut engine = SyncEngine::new(vault, &mut client, engine_identity, root);
        let outcome = engine.integrate_dependency_ordered(integrate_config)?;
        drop(engine);
        // changed heads（正文）触发一次**现有接口**的物化：B 事务写 `notes.body_md`
        // 并发布 `BodyMaterialized` 事件（索引器据此增量更新）。结构投影的事件由
        // `StructureWriter::commit` 自己发布（`StructureProjected`）。
        let mut changed_heads = outcome.changed_heads;
        if let Some(report) = &catchup_report {
            for doc_id in &report.docs {
                if !changed_heads.iter().any(|head| head == doc_id) {
                    changed_heads.push(doc_id.clone());
                }
            }
        }
        let (materialized, materialize_failed) =
            materialize_changed_bodies(vault, peer_id, &changed_heads);
        let after = load_head(vault, watch_doc)?.head_seq;
        let watch_doc_changed = after != before;
        Ok(IntegratePhaseReport {
            integrate: outcome.report,
            changed_heads,
            watch_doc_changed,
            materialized,
            materialize_failed,
            catchup: catchup_report,
        })
    }

    /// 两阶段同步的 **Finish** 阶段：推送本阶段新落库的本地意图（若有）→ 上报 ACK。
    ///
    /// 调用方契约：UI 已经把 integrate 的 durable 变化 adopt 进 live、并 `end` 释放闸门。
    /// 本阶段不再 `pull`/`integrate`：它只做 "让 relay 知道本机已经收到/整合到哪" 以及
    /// 清掉本轮 Held 期间新产生的本地意图。
    pub fn sync_finish<T: RelayTarget + ?Sized>(
        target: &T,
        vault: &Vault,
        identity: &LocalIdentityV1,
        transport: Option<TransportTimeouts>,
    ) -> Result<FinishReport, SyncError> {
        let context = target.relay_context(transport)?;
        let root = root_of(identity);
        let signing_key = DeviceSigningKey::from_seed(identity.ed25519_seed.clone());
        let mut client = context.open(vault, identity)?.into_client();
        ensure_cursor(&mut client, vault)?;
        let engine_identity = EngineIdentity {
            device_id: identity.device_id,
            signing_key,
            peer_id: identity.loro_peer_id(),
        };
        let mut engine = SyncEngine::new(vault, &mut client, engine_identity, root);
        let config = EngineConfig::default();
        // 先推 tail（Held 窗口内新落库的本地意图），再上报 received/integrated 游标。
        let encrypted = engine.encrypt_pending(config.send_batch)?;
        let push = engine.push()?;
        let ack = engine.ack()?;
        Ok(FinishReport {
            ack,
            encrypted,
            push,
        })
    }

    /// ADR 0010 切片 2a-2：读一次**已验证控制链的只读成员视图**（设备列表 UI 的数据源）。
    ///
    /// 只构造一个**临时** `HttpRelayClient`（用完即丢，不缓存 token/控制链）：调用方拿到的
    /// 只有 [`MemberView`]——它不含 token/密钥，`Debug` 也只打印 id/status/fingerprint。
    /// 缓存失效语义由 2a-1 的 [`HttpRelayClient::members`] 负责（先比对 `/state.auth_rev`
    /// 再决定是否重新拉控制链）；每次新客户端天然读到最新的 `auth_rev`。
    ///
    /// 失败即真实失败（relay 不可达 / 被吊销 `REVOKED` / 控制链 pin 不符），不返回旧快照。
    pub fn members<T: RelayTarget + ?Sized>(
        target: &T,
        vault: &Vault,
        identity: &LocalIdentityV1,
    ) -> Result<Vec<MemberView>, SyncError> {
        let context = target.relay_context(None)?;
        let mut client = context.open(vault, identity)?.into_client();
        client.members()
    }

    /// ADR 0010 切片 1b + 2a-2：**移除一台设备对 relay 的访问**（不轮换密钥）。
    ///
    /// 语义与安全边界严格照 ADR 0010：
    ///
    /// - 只是诚实 relay 上的**访问撤销**（`auth_rev` 前进、目标 `status=revoked`），
    ///   不是签名成员移除、也不是密码学撤销：被移除设备仍可能用它已获得的旧 root 解密
    ///   旧/同 epoch 密文；管理材料（`manager_seed`/恢复码）**不会被撤销**；
    /// - **禁止移除本机**（这里在任何网络动作之前拒绝；helper 里还有第二道）；
    /// - **必须有管理密钥**：只有 `manager_seed` 在身份里的设备（Create/恢复码路径）能管理；
    ///   Join 设备明确失败，绝不退化成"用设备 key 冒充管理"；
    /// - 不删本地身份 / `manager_seed` / 恢复材料 / 笔记，不自动轮换、不把 revoked 翻回 active；
    /// - `request_id` 每次调用随机 16B（`OsRandom`）：响应丢失就整次重试（新 id），
    ///   不拿旧 id 去撞 relay；`expected_control_rev` 取刚读到的 `/state.control_rev`
    ///   （独立吊销**不推进** `control_rev`，CAS 冲突时应刷新后重试）。
    ///
    /// 成功后**重新拉一次成员视图**再返回，调用方（worker）可直接把最新列表投影到面板，
    /// 不需要另发一条列表命令。
    ///
    /// ## 这是**默认**移除路径，且**永不**轮换（task-123）
    ///
    /// 本调用只产生"设备访问已吊销"这一件事：返回的 [`RevokedDevice::outcome`] 里
    /// `changed`/`auth_rev` 可能变化，`control_rev` 与 vault 的 `key_epoch` **保持不变**
    /// （测试断言 relay `/state`、DB 与两台设备的本地身份 epoch）。它**不会**、也**不得**
    /// 被改成隐式触发轮换：需要新 root / 重加密历史时必须由用户显式走
    /// [`crate::dev_local::DevLocal::rotate`]（opt-in），两者的 wire 与错误语义互不复用。
    pub fn revoke_device<T: RelayTarget + ?Sized>(
        relay: &T,
        vault: &Vault,
        identity: &LocalIdentityV1,
        target: DeviceId,
    ) -> Result<RevokedDevice, SyncError> {
        // ① 本机守卫：先于任何 IO（打开库/读身份以外）拒绝。
        if target == identity.device_id {
            return Err(SyncError::Unsupported(
                "禁止移除本机设备：会立即自断同步与管理访问（ADR 0010）；请用另一台管理设备或恢复码",
            ));
        }
        // ② 管理密钥必须来自受保护身份（Join 设备没有）：缺它就直接失败。
        let Some(seed) = identity.manager_seed.clone() else {
            return Err(SyncError::Unsupported(
                "本机没有管理密钥（Join 设备）：无法移除设备；请用 Create 设备或最新恢复码操作",
            ));
        };
        let manager = ManagementKey::from_seed(seed);

        // ③ CAS 基准取当前 `/state.control_rev`（独立吊销不推进它；不盲用 identity 里的旧值）。
        let context = relay.relay_context(None)?;
        let mut client = context.open(vault, identity)?.into_client();
        let state = client.state()?;
        if state.instance_id != identity.pinned_instance {
            return Err(SyncError::RelayWire("relay instance 与本地 pin 不符"));
        }

        // ④ 每次点击一个新 request_id（去重 scope 是固定 `management/device-revoke`，
        //    target 在 canonical body 里，跨设备不会碰撞）。
        let request_id: [u8; 16] = array(&mut OsRandom)?;
        let ctx = RevokeContext::from_identity(
            context.origin_str().as_str(),
            identity,
            state.control_rev,
            SystemClock.now_secs(),
        )
        .with_api_version(context.api_version());
        let mut transport = context.transport()?;
        let outcome = crate::device_management::revoke_device(
            &mut transport,
            &ctx,
            &manager,
            target,
            request_id,
        )?;

        // ⑤ 只读刷新：`auth_rev` 已前进，新客户端自然读到吊销后的成员集合。
        let members = Self::members(&context, vault, identity)?;
        Ok(RevokedDevice { outcome, members })
    }

    /// 读 dev-local 实例标签（[`VAULT_LABEL_META`]；Join 的库没有，返回 `None`）。
    pub fn vault_label(vault: &Vault) -> Result<Option<String>, SyncError> {
        match vault.get_meta(VAULT_LABEL_META)? {
            None => Ok(None),
            Some(bytes) => String::from_utf8(bytes)
                .map(Some)
                .map_err(|_| SyncError::LocalCorrupt("vault 标签不是 UTF-8")),
        }
    }

    /// 读最近一次发布基线的 durable 事实（[`PUBLISHED_BASELINE_META`]）。
    pub fn published_baseline(vault: &Vault) -> Result<Option<PublishedBaseline>, SyncError> {
        let Some(bytes) = vault.get_meta(PUBLISHED_BASELINE_META)? else {
            return Ok(None);
        };
        decode_published_baseline(&bytes).map(Some)
    }
}

// ---------------------------------------------------------------- 内部助手

/// genesis 的 password 槽材料只用于凑齐 `GenesisMaterials`（本模块不上传口令槽）：
/// 用 32B 随机数的十六进制当一次性口令，构造完立即丢弃。
pub(crate) fn throwaway_password() -> Result<String, SyncError> {
    let bytes: [u8; 32] = array(&mut OsRandom)?;
    Ok(ids::hex_of(&bytes))
}

/// 身份的当前 epoch 根。
pub(crate) fn root_of(identity: &LocalIdentityV1) -> RootSecretV1 {
    RootSecretV1::new(
        identity.vault_id,
        identity.current_epoch,
        identity.current_root.clone(),
    )
}

/// 对 changed heads 里的**正文**文档触发一次现有接口的物化（`projection_outbox` →
/// `notes.body_md`，成功时发布 `BodyMaterialized` → 索引增量）。
///
/// 只处理派生数据：失败计入返回值并打日志，不改 CRDT 事实、也不改变集成结论
/// （搜索索引还有周期对账兜底）。结构文档的投影事件由 `StructureWriter::commit`
/// 自己发布。
fn materialize_changed_bodies(
    vault: &Vault,
    peer_id: u64,
    changed_heads: &[String],
) -> (usize, usize) {
    if changed_heads.is_empty() {
        return (0, 0);
    }
    let pending = match pending_outbox(vault) {
        Ok(pending) => pending,
        Err(error) => {
            eprintln!("[dev-sync] 读取 projection_outbox 失败（跳过本轮物化）：{error}");
            return (0, changed_heads.len());
        }
    };
    let mut materialized = 0usize;
    let mut failed = 0usize;
    for item in pending {
        if item.doc_id == STRUCTURE_DOC_ID || !changed_heads.iter().any(|head| head == &item.doc_id)
        {
            continue;
        }
        let Some(note_id) = item.note_id else {
            // 结构投影还没建立 notes 行：不是失败，等下一轮结构/索引对账。
            continue;
        };
        match materialize_body(vault, &note_id, &item.doc_id, item.target_seq, peer_id) {
            Ok(_) => materialized += 1,
            Err(error) => {
                failed += 1;
                eprintln!(
                    "[dev-sync] 正文物化失败（{}，目标 seq {}）：{error}",
                    item.doc_id, item.target_seq
                );
            }
        }
    }
    (materialized, failed)
}

/// 把 `PeerId` 的 u64 映射回 16B（`identity.peer_id` 的存储形态）。
///
/// 受限初始化入口（[`crate::init_session`]）也用同一条映射：Join 的 peer 口径只能有
/// 一处，不允许各写一套。
pub(crate) fn peer_bytes_of(peer_id: u64) -> [u8; 16] {
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&peer_id.to_be_bytes());
    bytes
}

/// 缺 `sync.relay_cursor` 时用 relay `GET /state` 键 14 兜底（genesis 键 5 的旧 relay 兼容）。
fn ensure_cursor<T: HttpTransport>(
    client: &mut HttpRelayClient<T>,
    vault: &Vault,
) -> Result<(), SyncError> {
    if vault.get_meta(RELAY_CURSOR_META)?.is_none() {
        let cursor = client.initial_cursor()?;
        vault.set_meta(RELAY_CURSOR_META, cursor.as_bytes())?;
    }
    Ok(())
}

/// 恢复码必须与本地管理身份一致：`HKDF(R, vault_id, "MN/v1/management-ed25519\0")`
/// 逐字节等于 identity 里的 `manager_seed`。错码在这里失败，**不必先上传任何对象**。
fn verify_recovery_matches(
    identity: &LocalIdentityV1,
    seed: &RecoverySeed,
) -> Result<(), SyncError> {
    let Some(stored) = &identity.manager_seed else {
        return Err(SyncError::Unsupported(
            "发布初始基线需要 genesis 身份（含管理私钥）：Join 设备不能发布",
        ));
    };
    let derived = management_seed(seed, &identity.vault_id)?;
    if !derived.ct_eq(stored) {
        return Err(SyncError::RecoveryCode("恢复码与本地管理身份不匹配"));
    }
    Ok(())
}

/// 身份 ↔ 初始 recovery 槽的自洽校验（不联网、不碰秘密输出）。
///
/// 三条都对应真实故障模式，缺一不可：
/// 1. 槽归属 = 身份 vault（加载时已校验，这里再兜一次防"换库/换身份"的错配）；
/// 2. 槽 epoch = 身份当前 epoch：轮换过 epoch 的实例不能再用"初始基线"发布（否则会拿
///    旧 epoch 的槽配新 root，Join 端解出的根对不上）；
/// 3. 管理私钥必须存在，且派生公钥 == `pinned_manager`（管理 pin 自洽）：Join 设备没有
///    管理私钥 → 明确拒绝，绝不退化成"用设备 key 冒充管理"。
fn verify_identity_slot_consistency(
    identity: &LocalIdentityV1,
    slot: &KeySlotV1,
) -> Result<(), SyncError> {
    if slot.descriptor.vault_id != identity.vault_id {
        return Err(SyncError::LocalCorrupt(
            "初始 recovery 槽的 vault 归属与身份不一致",
        ));
    }
    if slot.descriptor.key_epoch != identity.current_epoch {
        return Err(SyncError::Unsupported(
            "初始基线只能在 genesis epoch 发布：初始槽的 epoch 与身份当前 epoch 不一致\
             （轮换之后不能用初始基线发布）",
        ));
    }
    require_manager_identity(identity)
}

/// 管理密钥（信任锚）必须来自受保护身份，且与 `pinned_manager` 自洽。
///
/// 这条校验**先于**读槽/任何上传：Join 设备（`manager_seed = None`）必须在拿到"缺初始槽"
/// 之类无关错误之前就看到真正原因，也绝不退化成"用设备 key 冒充管理"。
fn require_manager_identity(identity: &LocalIdentityV1) -> Result<(), SyncError> {
    let Some(seed) = identity.manager_seed.clone() else {
        return Err(SyncError::Unsupported(
            "发布初始基线需要 genesis 身份（含管理私钥）：Join 设备没有管理密钥，不能发布",
        ));
    };
    if ManagementKey::from_seed(seed).public_key() != identity.pinned_manager {
        return Err(SyncError::LocalCorrupt(
            "身份的管理私钥与 pinned_manager 不一致（身份文件被换过或损坏）",
        ));
    }
    Ok(())
}

/// 初始 recovery 槽（KEY_SLOT 对象）的确定性 `message_id`。
///
/// `message_id` 进 MNE1 header/AAD；relay 对同 `object_id` 换了 `message_id`（=不同信封
/// 字节）会返回 409 ID_CONFLICT，而"上传初始槽 / 发布基线"都是**可重跑**的动作（Create
/// 传过一次、CLI 发布补传、checkpoint 失败后重试）。所以这里只用槽材料（slot_id + 描述符
/// CDE，都是非秘密）派生一个稳定 id：重试/重启/跨入口都得到逐字节相同的 KEY_SLOT 对象，
/// relay 按幂等接受。
fn key_slot_message_id(slot: &KeySlotV1) -> Result<[u8; 16], SyncError> {
    let descriptor = slot.descriptor.encode()?;
    let mut hasher = Sha256::new();
    hasher.update(KEY_SLOT_MESSAGE_DOMAIN);
    hasher.update(slot.descriptor.slot_id);
    hasher.update(&descriptor);
    let digest = hasher.finalize();
    let mut message_id = [0u8; 16];
    message_id.copy_from_slice(&digest[..16]);
    Ok(message_id)
}

/// 从 `meta` 读回 genesis 的 recovery 槽（描述符 + 包裹根），并校验归属。
fn load_initial_slot(vault: &Vault, vault_id: &VaultId) -> Result<KeySlotV1, SyncError> {
    let bytes = vault
        .get_meta(INITIAL_RECOVERY_SLOT_META)?
        .ok_or(SyncError::LocalCorrupt(
            "vault 缺 genesis 初始 recovery 槽记录（只有 DevLocal::create 的库能发布基线）",
        ))?;
    decode_initial_slot(&bytes, vault_id)
}

/// `magic || u32 BE(descriptor_len) || CDE(descriptor) || nonce||ciphertext`。
pub(crate) fn encode_initial_slot(slot: &KeySlotV1) -> Result<Vec<u8>, SyncError> {
    let descriptor = slot.descriptor.encode()?;
    let descriptor_len = u32::try_from(descriptor.len())
        .map_err(|_| SyncError::LocalCorrupt("初始槽描述符长度超出 u32"))?;
    let wrapped = slot.wrapped_bytes();
    let mut out =
        Vec::with_capacity(INITIAL_SLOT_MAGIC.len() + 4 + descriptor.len() + wrapped.len());
    out.extend_from_slice(INITIAL_SLOT_MAGIC);
    out.extend_from_slice(&descriptor_len.to_be_bytes());
    out.extend_from_slice(&descriptor);
    out.extend_from_slice(&wrapped);
    Ok(out)
}

/// 解析 [`encode_initial_slot`] 的记录；任何不自洽都按本地损坏处理（不猜、不修复）。
fn decode_initial_slot(bytes: &[u8], vault_id: &VaultId) -> Result<KeySlotV1, SyncError> {
    let header_len = INITIAL_SLOT_MAGIC.len() + 4;
    if bytes.len() < header_len || !bytes.starts_with(INITIAL_SLOT_MAGIC) {
        return Err(SyncError::LocalCorrupt(
            "初始 recovery 槽记录 magic/长度不符",
        ));
    }
    let mut raw_len = [0u8; 4];
    raw_len.copy_from_slice(&bytes[INITIAL_SLOT_MAGIC.len()..header_len]);
    let descriptor_len = usize::try_from(u32::from_be_bytes(raw_len))
        .map_err(|_| SyncError::LocalCorrupt("初始 recovery 槽描述符长度非法"))?;
    let descriptor_end = header_len
        .checked_add(descriptor_len)
        .ok_or(SyncError::LocalCorrupt("初始 recovery 槽描述符长度溢出"))?;
    if descriptor_len == 0
        || descriptor_len > MAX_SLOT_DESCRIPTOR_LEN
        || descriptor_end >= bytes.len()
    {
        return Err(SyncError::LocalCorrupt(
            "初始 recovery 槽记录的分段长度非法",
        ));
    }
    let descriptor = SlotDescriptorV1::decode(&bytes[header_len..descriptor_end])?;
    if descriptor.vault_id != *vault_id {
        return Err(SyncError::LocalCorrupt("初始 recovery 槽的 vault 归属不符"));
    }
    if descriptor.kind != SlotKind::Recovery {
        return Err(SyncError::LocalCorrupt("初始槽记录不是 recovery 槽"));
    }
    let wrapped = WrappedRoot::from_bytes(&bytes[descriptor_end..])
        .map_err(|_| SyncError::LocalCorrupt("初始 recovery 槽的槽体损坏"))?;
    Ok(KeySlotV1 {
        descriptor,
        wrapped,
    })
}

/// `magic || slot_id(32) || message_id(16)`（初始 recovery 槽「已上传」记录）。
fn encode_initial_slot_uploaded(slot_id: &[u8; 32], message_id: &[u8; 16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(INITIAL_SLOT_UPLOADED_MAGIC.len() + 32 + 16);
    out.extend_from_slice(INITIAL_SLOT_UPLOADED_MAGIC);
    out.extend_from_slice(slot_id);
    out.extend_from_slice(message_id);
    out
}

/// 解析 [`encode_initial_slot_uploaded`] 的记录（长度/magic 不符按本地损坏处理）。
fn decode_initial_slot_uploaded(bytes: &[u8]) -> Result<([u8; 32], [u8; 16]), SyncError> {
    let expected = INITIAL_SLOT_UPLOADED_MAGIC.len() + 32 + 16;
    if bytes.len() != expected || !bytes.starts_with(INITIAL_SLOT_UPLOADED_MAGIC) {
        return Err(SyncError::LocalCorrupt(
            "初始 recovery 槽上传记录 magic/长度不符",
        ));
    }
    let mut slot_id = [0u8; 32];
    let mut message_id = [0u8; 16];
    slot_id.copy_from_slice(&bytes[INITIAL_SLOT_UPLOADED_MAGIC.len()..][..32]);
    message_id.copy_from_slice(&bytes[INITIAL_SLOT_UPLOADED_MAGIC.len() + 32..]);
    Ok((slot_id, message_id))
}

/// 上传成功后写 durable 标记（`create` / CLI 发布都会走这里；重传幂等）。
fn write_initial_slot_uploaded(
    vault: &Vault,
    slot_id: &[u8; 32],
    message_id: &[u8; 16],
) -> Result<(), SyncError> {
    vault.set_meta(
        INITIAL_SLOT_UPLOADED_META,
        &encode_initial_slot_uploaded(slot_id, message_id),
    )?;
    Ok(())
}

/// 初始 recovery 槽是否**当前这份槽材料**已经上传过（`false` = 旧版本 Create 的实例）。
///
/// 记录存在但 `slot_id` 对不上 → 本地事实自相矛盾（换过库/槽材料），按损坏 fail-closed，
/// 不猜、不重写。
fn initial_slot_uploaded(vault: &Vault, slot: &KeySlotV1) -> Result<bool, SyncError> {
    let Some(bytes) = vault.get_meta(INITIAL_SLOT_UPLOADED_META)? else {
        return Ok(false);
    };
    let (slot_id, _message_id) = decode_initial_slot_uploaded(&bytes)?;
    if slot_id != slot.descriptor.slot_id {
        return Err(SyncError::LocalCorrupt(
            "初始 recovery 槽上传记录与本库槽材料不一致（不重写、不猜）",
        ));
    }
    Ok(true)
}

/// 把"最新一份已发布 checkpoint"落成本地 durable 事实（不覆盖更旧的两份，只记较新的）。
fn write_published_baseline(vault: &Vault, report: &CheckpointReport) -> Result<(), SyncError> {
    let published_at = report
        .published_at
        .ok_or(SyncError::LocalCorrupt("checkpoint report 缺发布时间"))?;
    let mut out = Vec::with_capacity(PUBLISHED_BASELINE_MAGIC.len() + 16 + 24);
    out.extend_from_slice(PUBLISHED_BASELINE_MAGIC);
    out.extend_from_slice(&report.checkpoint_id);
    out.extend_from_slice(&report.epoch.to_be_bytes());
    out.extend_from_slice(&report.covered_seq.to_be_bytes());
    out.extend_from_slice(&published_at.to_be_bytes());
    vault.set_meta(PUBLISHED_BASELINE_META, &out)?;
    Ok(())
}

/// 解析 [`write_published_baseline`] 的记录。
fn decode_published_baseline(bytes: &[u8]) -> Result<PublishedBaseline, SyncError> {
    let expected = PUBLISHED_BASELINE_MAGIC.len() + 16 + 8 * 3;
    if bytes.len() != expected || !bytes.starts_with(PUBLISHED_BASELINE_MAGIC) {
        return Err(SyncError::LocalCorrupt("已发布基线记录 magic/长度不符"));
    }
    let mut checkpoint_id = [0u8; 16];
    let mut offset = PUBLISHED_BASELINE_MAGIC.len();
    checkpoint_id.copy_from_slice(&bytes[offset..offset + 16]);
    offset += 16;
    let mut field = |name: &'static str| -> Result<u64, SyncError> {
        let end = offset
            .checked_add(8)
            .ok_or(SyncError::LocalCorrupt("已发布基线记录越界"))?;
        let raw = bytes
            .get(offset..end)
            .ok_or(SyncError::LocalCorrupt(name))?;
        offset = end;
        let mut buf = [0u8; 8];
        buf.copy_from_slice(raw);
        Ok(u64::from_be_bytes(buf))
    };
    let epoch = field("已发布基线记录缺 epoch")?;
    let covered_seq = field("已发布基线记录缺 covered_seq")?;
    let published_at = field("已发布基线记录缺 published_at")?;
    Ok(PublishedBaseline {
        checkpoint_id,
        epoch,
        covered_seq,
        published_at,
    })
}

/// 清掉"本次调用刚建出来"的 vault 文件（主文件 + WAL 边车）与**空**实例目录。
///
/// 只允许在调用方已确认路径原本不存在时使用；失败路径尽力而为，不掩盖原始错误。
///
/// 目录只在"刚被本次初始化创建且为空"时删除（`remove_dir` 对非空目录失败，因此用户
/// 已有内容的目录不会被删）：Create 现在必须**先**建库写 pending、再联网（ADR 0013
/// S2b 的写前记录），所以"联网失败不该留下实例目录"这条既有验收要靠这里兜住。
pub(crate) fn remove_vault_files(path: &Path) {
    let _ = std::fs::remove_file(path);
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut sidecar = path.as_os_str().to_os_string();
        sidecar.push(suffix);
        let _ = std::fs::remove_file(PathBuf::from(sidecar));
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            let _ = std::fs::remove_dir(parent);
        }
    }
}

`````


## `crates/storage/src/session/flush.rs`

`````rust
//! 未 ACK journal 的后台 flush 线程（ADR 0003 §5 / ADR 0001 §3.4）。
//!
//! 契约：
//!
//! - `flush()` 只投递一条命令就返回票据，**调用线程绝不碰 SQLite**；
//! - 后台线程每次取 journal 的**连续前缀**，合并成一次 A 事务写 durable；
//! - 写成功后先裁剪前缀、推进 durable 水位，再把 [`SessionEvent::Durable`] 推进事件队列；
//! - 写失败**不裁剪、不 ACK**：同一前缀下次重试，因此「已 ACK 的部分」不会丢；
//! - **head 过期**（[`StorageError::HeadConflict`]）：同一命令内**至多**自动
//!   `load_head` + 重建 sink 一次，并用新 CAS 基准重试**同一前缀**一次。只有新 head 的
//!   ops 全部落在未 ACK 前缀最后一条的 `end_vv` 里（即外部推进写的是本会话 live 已知的
//!   op）才允许重试——否则写入的 `head_vv`/`frontier` 会缺掉外部 ops，下一次
//!   `load_body_doc` 的完整性校验直接 `Corrupt`。不满足或重建后仍失败 → **不可恢复**：
//!   `last_write_error` 明确提示「需要重开文档」，journal 原样保留，后续 flush 立即失败、
//!   不再碰数据库（有界重试，不无限写）。

use std::collections::VecDeque;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::journal::{DurableSink, JournalEntry, JournalOrigin, PendingJournal, VaultJournalSink};
use crate::store::{load_head, HeadState};
use crate::sync_bridge::SyncContext;
use crate::{StorageError, Vault};

use super::gate::AppliedHeads;
use super::protocol::{BatchId, DurableAck, FlushTicketId, LiveRev, SessionEvent};

/// 闸门判定用的水位快照（见 [`JournalHandle::gate_progress`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GateProgress {
    /// 未 ACK 条数。
    pub(crate) pending: usize,
    /// 已 durable 到的 live 修订号。
    pub(crate) durable_through: LiveRev,
    /// 已 durable 的 head seq（外部提交的 CAS 基准）。
    pub(crate) durable_head_seq: u64,
    /// 最近一次前缀写失败原因（成功写入后清空；不可恢复时保留「需要重开文档」提示）。
    pub(crate) last_write_error: Option<String>,
}

/// 取锁；锁被毒化也继续用内部值（清理路径不允许 panic）。
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 后台 flush 命令（票据号已由调用方持有，线程只需要目标水位）。
#[derive(Debug, Clone, Copy)]
pub(crate) struct FlushCommand {
    /// 本次 flush 需要覆盖到的 live 修订号。
    pub(crate) target_live_rev: LiveRev,
}

/// 会话与 flush 线程共享的未 ACK 状态。
#[derive(Debug)]
pub(crate) struct PendingState {
    /// 未 ACK journal（容量准入 / 连续前缀 / durable 后裁剪）。
    pub(crate) journal: PendingJournal,
    /// 与 journal 前缀一一对应的 live 修订号。
    pub(crate) revs: VecDeque<LiveRev>,
    /// 与 journal 前缀一一对应的 update 字节数（64 KiB 阈值用）。
    pub(crate) sizes: VecDeque<usize>,
    /// 未 ACK update 的总字节数。
    pub(crate) pending_bytes: usize,
    /// 下一个批次号。
    pub(crate) next_batch: u64,
    /// 下一张票据号。
    pub(crate) next_ticket: u64,
    /// 已 durable 的 head 状态（A 事务的 CAS 基准）。
    pub(crate) durable: HeadState,
    /// 最近一次成功 durable 的批次号。
    pub(crate) durable_batch: Option<BatchId>,
    /// 已 durable 到的 live 修订号。
    pub(crate) durable_through: LiveRev,
    /// 最近一次前缀写失败的原因；下一次成功写入后清空。
    ///
    /// 外部提交闸门据此把「还在 Flushing」明确升级成 `Failed`，不静默等一个永远不来的 ACK。
    pub(crate) last_write_error: Option<String>,
    /// 不可恢复的 flush 失败（head 过期且无法安全自动重建）。
    ///
    /// 一旦置位就**不再自动重试**：后续 flush 命令立即失败、不再碰数据库；journal 原样
    /// 保留（未 ACK 的数据不丢也不会被静默丢弃），只有关闭并重开文档才能恢复。
    /// 外部提交闸门通过 [`GateProgress::last_write_error`] 把它升级成 `Failed`，UI 可据此
    /// 提示重开文档。
    pub(crate) unrecoverable: Option<String>,
}

/// journal 句柄：会话侧投递命令，后台线程负责写。
#[derive(Debug)]
pub(crate) struct JournalHandle {
    state: Arc<Mutex<PendingState>>,
    events: Arc<Mutex<VecDeque<SessionEvent>>>,
    sender: Option<Sender<FlushCommand>>,
    worker: Option<JoinHandle<()>>,
}

impl JournalHandle {
    /// 启动后台 flush 线程。
    ///
    /// `flush_delay` 是**测试/故障注入**用的可控延迟（模拟慢盘），生产传 `None`；
    /// `sync` 是 ADR 0009 切片 2b 的本地意图上下文（`None` = 不写 `sync_outbox`）。
    pub(crate) fn spawn(
        vault: Vault,
        doc_id: String,
        head: HeadState,
        capacity: usize,
        max_batch: usize,
        flush_delay: Option<Duration>,
        sync: Option<SyncContext>,
    ) -> Self {
        let state = Arc::new(Mutex::new(PendingState {
            journal: PendingJournal::new(capacity),
            revs: VecDeque::new(),
            sizes: VecDeque::new(),
            pending_bytes: 0,
            next_batch: 1,
            next_ticket: 1,
            durable: head,
            durable_batch: None,
            durable_through: LiveRev(0),
            last_write_error: None,
            unrecoverable: None,
        }));
        let events: Arc<Mutex<VecDeque<SessionEvent>>> = Arc::new(Mutex::new(VecDeque::new()));
        let (sender, receiver) = channel::<FlushCommand>();

        let worker_state = Arc::clone(&state);
        let worker_events = Arc::clone(&events);
        let config = FlusherConfig {
            doc_id,
            max_batch: max_batch.max(1),
            flush_delay,
            sync,
        };
        let worker = thread::Builder::new()
            .name("menghunote-flush".to_owned())
            .spawn(move || {
                flusher_main(vault, worker_state, worker_events, receiver, config);
            });

        // 线程起不来时退化为「没有后台 flush」：票据照发，但不会有 ACK（调用方可重启会话）。
        let worker = worker.ok();

        Self {
            state,
            events,
            sender: Some(sender),
            worker,
        }
    }

    /// 受理一次 flush：登记票据并投递命令，立即返回票据号。
    pub(crate) fn submit_ticket(&self, target: LiveRev) -> FlushTicketId {
        let ticket_id = {
            let mut guard = lock(&self.state);
            guard.next_ticket += 1;
            FlushTicketId(guard.next_ticket)
        };
        if let Some(sender) = self.sender.as_ref() {
            let _ = sender.send(FlushCommand {
                target_live_rev: target,
            });
        }
        ticket_id
    }

    /// 共享状态（会话核在持锁区间内完成「准入 + 改 Loro + 追加」）。
    pub(crate) fn state_ref(&self) -> &Mutex<PendingState> {
        &self.state
    }

    /// 事件队列（flush 线程写入，会话核读取）。
    pub(crate) fn events_ref(&self) -> &Mutex<VecDeque<SessionEvent>> {
        &self.events
    }

    /// 未 ACK 条数。
    pub(crate) fn pending_len(&self) -> usize {
        lock(&self.state).journal.pending_len()
    }

    /// 是否已满（背压判定）。
    pub(crate) fn is_full(&self) -> bool {
        lock(&self.state).journal.is_full()
    }

    /// 未 ACK update 总字节数（调度器判断 64 KiB 阈值）。
    pub(crate) fn pending_bytes(&self) -> usize {
        lock(&self.state).pending_bytes
    }

    /// 已 durable 的 head seq。
    pub(crate) fn durable_head_seq(&self) -> u64 {
        lock(&self.state).durable.head_seq
    }

    /// 已 durable 到哪个 live 修订号。
    pub(crate) fn durable_through(&self) -> LiveRev {
        lock(&self.state).durable_through
    }

    /// 外部提交闸门判定用的**一次性快照**：一次取锁同时读 pending / durable 水位 /
    /// 最近一次写失败原因，避免分多次取锁看到互相矛盾的组合。
    pub(crate) fn gate_progress(&self) -> GateProgress {
        let guard = lock(&self.state);
        GateProgress {
            pending: guard.journal.pending_len(),
            durable_through: guard.durable_through,
            durable_head_seq: guard.durable.head_seq,
            last_write_error: guard.last_write_error.clone(),
        }
    }

    /// 最近一次 flush 写失败的原因（**一次取锁**）。
    ///
    /// 优先返回 [`PendingState::unrecoverable`]：不可恢复的失败必须让 UI/调用方看到
    /// 「需要重开文档」，不能被后到的普通写失败（字段会被覆盖）稀释；没有不可恢复失败时
    /// 返回最近一次普通写失败（成功写入后清空）。会话核的
    /// [`super::core::DocumentSession::last_write_error`] 与 `CloseReport.flush_error`
    /// 都以本方法为唯一来源（闸门走 [`Self::gate_progress`]，读同一份状态的字段）。
    pub(crate) fn last_write_error(&self) -> Option<String> {
        let guard = lock(&self.state);
        guard
            .unrecoverable
            .clone()
            .or_else(|| guard.last_write_error.clone())
    }

    /// 是否已进入不可恢复状态（只有重开文档能继续）。
    ///
    /// `SessionDriver::close` 用它短路等待：不可恢复时不会再有 DurableAck，继续等满
    /// timeout 只会让关闭/切换卡住 UI。
    pub(crate) fn is_unrecoverable(&self) -> bool {
        lock(&self.state).unrecoverable.is_some()
    }

    /// 外部提交 adopt / end 之后重定 durable 基准（调用方必须已持锁）。
    ///
    /// `loro_docs.head_*` 已由外部提交（`engine.integrate`）推进；这里把 flusher 的 CAS
    /// 基准与 durable 水位对齐，避免下一次本地 flush 报 `HeadConflict`。
    ///
    /// **只允许在 journal `pending == 0` 时调用**：journal 条目的 `end_vv` / `end_frontier`
    /// 是追加那一刻的快照，有在途条目时重定基准会让水位于条目自相矛盾（前瞻 head 回退）。
    pub(crate) fn rebase_locked(
        guard: &mut PendingState,
        heads: &AppliedHeads,
        through: LiveRev,
    ) -> Result<(), StorageError> {
        if guard.journal.pending_len() != 0 {
            return Err(StorageError::Corrupt {
                context: "有未 ACK 条目时不能重定 durable 基准".to_owned(),
            });
        }
        guard.durable.head_seq = heads.head_seq;
        guard.durable.head_frontier = heads.frontier.clone();
        guard.durable.head_vv = heads.vv.clone();
        guard.durable_through = through;
        Ok(())
    }

    /// 取走全部待推送事件。
    pub(crate) fn drain_events(&self) -> Vec<SessionEvent> {
        let mut queue = lock(&self.events);
        queue.drain(..).collect()
    }

    /// 在 journal 里登记一条未 ACK 条目（调用方必须已通过容量准入）。
    ///
    /// `origin` 决定 flush 时是否产生出站同步意图：只有本地更新才产生。
    pub(crate) fn append(
        guard: &mut PendingState,
        doc_id: &str,
        batch_label: &str,
        origin: JournalOrigin,
        update: crate::body_doc::PendingUpdate,
        rev: LiveRev,
    ) -> Result<BatchId, crate::StorageError> {
        let batch = BatchId(guard.next_batch);
        guard.next_batch += 1;
        let size = update.payload.len();
        let entry = JournalEntry {
            doc_id: doc_id.to_owned(),
            batch_id: format!("{batch_label}-{}", batch.0),
            update,
            origin,
        };
        guard.journal.append(entry)?;
        guard.revs.push_back(rev);
        guard.sizes.push_back(size);
        guard.pending_bytes += size;
        Ok(batch)
    }
}

impl Drop for JournalHandle {
    fn drop(&mut self) {
        // 先关命令通道：flusher 的 recv 返回 Err 后退出循环；再 join，绝不 panic。
        drop(self.sender.take());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// flush 线程的一次性配置（与可变状态分开，避免 `flusher_main` 参数过多）。
#[derive(Debug, Clone)]
struct FlusherConfig {
    /// 归属文档（一次 flush 里不允许混文档）。
    doc_id: String,
    /// 一次 A 事务最多合并多少条。
    max_batch: usize,
    /// 注入的慢盘延迟（生产 `None`）。
    flush_delay: Option<Duration>,
    /// 本地意图上下文（`None` = 不写 `sync_outbox`）。
    sync: Option<SyncContext>,
}

/// flush 线程主体：一条命令 = 至少一次前缀写，直到覆盖 `target_live_rev` 或 journal 空。
fn flusher_main(
    vault: Vault,
    state: Arc<Mutex<PendingState>>,
    events: Arc<Mutex<VecDeque<SessionEvent>>>,
    receiver: Receiver<FlushCommand>,
    config: FlusherConfig,
) {
    let FlusherConfig {
        doc_id,
        max_batch,
        flush_delay,
        sync,
    } = &config;
    let doc_id = doc_id.clone();
    let max_batch = *max_batch;
    let flush_delay = *flush_delay;
    let sync = *sync;
    while let Ok(command) = receiver.recv() {
        if let Some(delay) = flush_delay {
            thread::sleep(delay);
        }

        let mut emitted_max: Option<LiveRev> = None;
        // P1-3：head 过期时同一命令**最多**自动重建 1 次 sink（`load_head` → 新基准 →
        // 重试同一前缀一次）；用尽后进入不可恢复状态，不再自动重试。
        //
        // 粒度是“每条 flush 命令”而不是“每个批次”：一条命令拆成多批时，若第一批
        // 重建成功、第二批再遇 HeadConflict，会直接不可恢复。这是保守取舍（慢盘多批
        // 场景可能误判），当前不自动扩成每批一次。
        let mut auto_rebuilt = false;
        loop {
            // 1) 取连续前缀（持锁只做内存操作，绝不在锁内写盘）
            let (entries, through, count, head_seq) = {
                let mut guard = lock(&state);
                // 不可恢复：后续 flush 立即失败、绝不再碰数据库（journal 原样保留）。
                if guard.unrecoverable.is_some() {
                    break;
                }
                let pending = guard.journal.pending_len();
                if pending == 0 {
                    break;
                }
                let take = max_batch.min(pending);
                let entries: Vec<JournalEntry> = guard.journal.peek_prefix(take).to_vec();
                let through = match guard.revs.get(take - 1) {
                    Some(rev) => *rev,
                    // 理论上不可能：journal 与 revs 一一对应；防御性退出
                    None => break,
                };
                let head_seq = guard.durable.head_seq;
                (entries, through, take, head_seq)
            };

            // 2) A 事务（在锁外，慢盘不会阻塞会话线程）
            let mut sink = VaultJournalSink::new(vault.clone(), doc_id.clone(), head_seq);
            if let Some(sync) = sync {
                sink = sink.with_sync_context(sync);
            }
            let written = match sink.write_prefix(&entries) {
                Ok(seq) => seq,
                // 普通写失败（磁盘/SQLite 等）：不裁剪、不 ACK，同一前缀下次重试；
                // 同时记录失败原因：外部提交闸门据此把 "Flushing" 明确升级为 Failed。
                //
                // `Corrupt` 是永久性本地状态错误（混文档/OriginMarks 长度不匹配/seq 溢出/
                // head 快照不可解码等），不能当成瞬时失败无限重试；直接进入不可恢复，
                // 让 UI 明确要求重开，而不是每次 flush 都撞同一错误。
                Err(error)
                    if !matches!(&error, StorageError::HeadConflict { .. })
                        && !is_retryable_write_error(&error) =>
                {
                    let message = reopen_message(
                        &doc_id,
                        lock(&state).journal.pending_len(),
                        format!("前缀写失败（永久性 Corrupt，不再自动重试）：{error}"),
                    );
                    let mut guard = lock(&state);
                    guard.last_write_error = Some(message.clone());
                    guard.unrecoverable = Some(message);
                    break;
                }
                Err(error) if !matches!(&error, StorageError::HeadConflict { .. }) => {
                    lock(&state).last_write_error = Some(error.to_string());
                    break;
                }
                // head 已被外部推进：同一份字节按旧 CAS 基准重试永远不会成功。
                // 先 `load_head` 拿真实基准、有界重建 sink，再重试**同一前缀**一次。
                Err(error) => {
                    if auto_rebuilt {
                        let message = reopen_message(
                            &doc_id,
                            lock(&state).journal.pending_len(),
                            format!(
                                "自动重建后再次 head 冲突（{error}），本命令的 1 次自动重建已用尽"
                            ),
                        );
                        let mut guard = lock(&state);
                        guard.last_write_error = Some(message.clone());
                        guard.unrecoverable = Some(message);
                        break;
                    }
                    auto_rebuilt = true;
                    let pending = lock(&state).journal.pending_len();
                    match retry_prefix_after_rebuild(
                        &vault, &doc_id, &entries, pending, head_seq, sync,
                    ) {
                        RebuildOutcome::Written(seq) => seq,
                        // 普通写失败：保持既有「同一前缀下次重试」契约，不升级成不可恢复。
                        RebuildOutcome::Retryable(message) => {
                            lock(&state).last_write_error = Some(message);
                            break;
                        }
                        RebuildOutcome::Unrecoverable(message) => {
                            let mut guard = lock(&state);
                            guard.last_write_error = Some(message.clone());
                            guard.unrecoverable = Some(message);
                            break;
                        }
                    }
                }
            };

            // 3) durable 之后才裁剪并推进水位
            let (batch, head_frontier) = {
                let mut guard = lock(&state);
                guard.last_write_error = None;
                let _ = guard.journal.ack_prefix(count);
                for _ in 0..count {
                    guard.revs.pop_front();
                    if let Some(size) = guard.sizes.pop_front() {
                        guard.pending_bytes = guard.pending_bytes.saturating_sub(size);
                    }
                }
                let batch = BatchId(guard.next_batch);
                guard.next_batch += 1;
                guard.durable.head_seq = written;
                let mut frontier = guard.durable.head_frontier.clone();
                let mut vv = guard.durable.head_vv.clone();
                if let Some(last) = entries.last() {
                    frontier = last.update.end_frontier.clone();
                    vv = last.update.end_vv.clone();
                }
                guard.durable.head_frontier = frontier.clone();
                guard.durable.head_vv = vv;
                guard.durable_through = through;
                guard.durable_batch = Some(batch);
                (batch, frontier)
            };

            let ack = DurableAck {
                batch_id: batch,
                through_live_rev: through,
                head_seq: written,
                head_frontier,
            };
            lock(&events).push_back(SessionEvent::Durable(ack));
            emitted_max = Some(through);

            if through >= command.target_live_rev {
                break;
            }
        }

        // 每个票据都必须有结局：没有待写条目（或已覆盖目标）时补一张确认票据，
        // 否则「手动保存」在无脏数据时永远等不到 ACK。
        let confirmation = {
            let guard = lock(&state);
            // 不可恢复时即使 pending_len==0（防御性状态，正常路径不可达）也不能补发
            // 确认票据：那会让等待方误以为 durable 成功。
            if guard.unrecoverable.is_none()
                && guard.journal.pending_len() == 0
                && emitted_max.is_none_or(|through| through < command.target_live_rev)
            {
                Some((
                    guard.durable_batch.unwrap_or(BatchId(0)),
                    guard.durable_through,
                    guard.durable.head_seq,
                    guard.durable.head_frontier.clone(),
                ))
            } else {
                None
            }
        };
        if let Some((batch_id, through_live_rev, head_seq, head_frontier)) = confirmation {
            lock(&events).push_back(SessionEvent::Durable(DurableAck {
                batch_id,
                through_live_rev,
                head_seq,
                head_frontier,
            }));
        }
    }
}

/// 写失败是否可能靠“下一次 flush 再试”自愈。
///
/// **正向白名单**：只有 IO / SQLite 瞬时错误 / 读快照 BUSY / 背压值得重试；其余
///（`Corrupt`、`Loro`、`LoroEncode`、`Coord`、schema 漂移、通道关闭等）都是永久性
/// 本地状态错误，重试不会自愈，必须 fail-closed 到“需要重开文档”。
fn is_retryable_write_error(error: &StorageError) -> bool {
    match error {
        StorageError::Io(_)
        | StorageError::SnapshotBusy { .. }
        | StorageError::WriteBackpressure { .. } => true,
        // SQLite 错误不能整类重试：ConstraintViolation/ReadOnly/CannotOpen/
        // DatabaseCorrupt/NotFound/Misuse 等都是永久错误，重试只会无限撞同一面墙。
        // 只有 BUSY/LOCKED 这类并发等待值得下一次 flush 再试。
        StorageError::Sqlite(rusqlite::Error::SqliteFailure(code, _)) => matches!(
            code.code,
            rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
        ),
        StorageError::Sqlite(_) => false,
        _ => false,
    }
}

/// head 冲突后自动重建 sink 的结果（P1-3 生产接线）。
#[derive(Debug)]
enum RebuildOutcome {
    /// 重建 sink 后用同一前缀重试成功；值为 durable 后的 `head_seq`。
    Written(u64),
    /// 不可恢复：必须关闭并重开文档（不再自动重试，也不再写库）。
    Unrecoverable(String),
    /// 重建后重试遇到普通写失败（磁盘/SQLite 等）：journal 保留，下一次 flush 再试。
    Retryable(String),
}

/// 重建后重试失败的分类（独立出来便于单测 `RebuildOutcome::Retryable` 分支）。
fn rebuild_write_error_outcome(
    doc_id: &str,
    pending: usize,
    error: StorageError,
) -> RebuildOutcome {
    if !is_retryable_write_error(&error) {
        return RebuildOutcome::Unrecoverable(reopen_message(
            doc_id,
            pending,
            format!("自动重建后重试前缀遇到永久性 Corrupt，不再自动重试：{error}"),
        ));
    }
    RebuildOutcome::Retryable(format!(
        "{doc_id} 自动重建后重试前缀失败（journal 保留，等待下次 flush）：{error}"
    ))
}

/// `HeadConflict` 后的有界自动重建：`load_head` → 新 CAS 基准 sink → 重试同一前缀一次。
///
/// **为什么不是无条件重试**：journal 条目的 `PendingUpdate` 是**追加那一刻**按旧 head
/// 导出的字节，写入 durable 的 `loro_docs.head_vv` / `head_frontier` 取自前缀最后一条的
/// `end_*` 快照。只有新 head 的 ops 全部包含在该快照的 VV 里（即外部推进写的是本会话
/// live 已知的 op）时，原样重试才与本会话 live 一致；否则 head 快照会缺掉外部 ops，
/// 下一次 `load_body_doc` 的完整性校验直接 `Corrupt`。后者 fail-closed：保留 journal、
/// 提示重开文档，绝不硬写。
///
/// `durable_base` 是本次写入用的 CAS 基准；新 head 必须严格大于它，回退的基准一律拒绝。
fn retry_prefix_after_rebuild(
    vault: &Vault,
    doc_id: &str,
    entries: &[JournalEntry],
    pending: usize,
    durable_base: u64,
    sync: Option<SyncContext>,
) -> RebuildOutcome {
    let head = match load_head(vault, doc_id) {
        Ok(head) => head,
        Err(error) => {
            return RebuildOutcome::Unrecoverable(reopen_message(
                doc_id,
                pending,
                format!("读不到当前 head（{error}），无法重建 sink"),
            ));
        }
    };
    if head.head_seq <= durable_base {
        return RebuildOutcome::Unrecoverable(reopen_message(
            doc_id,
            pending,
            format!(
                "库中 head 没有前进（CAS 基准 {durable_base}，读到 {}）；拒绝用回退的基准重写",
                head.head_seq
            ),
        ));
    }
    // 空前缀在 flusher 里不可达；防御性 fail-closed，不 panic。
    let Some(last) = entries.last() else {
        return RebuildOutcome::Unrecoverable(reopen_message(
            doc_id,
            pending,
            "未 ACK 前缀为空，无法校验 base".to_owned(),
        ));
    };
    let prefix_vv = match loro::VersionVector::decode(&last.update.end_vv) {
        Ok(vv) => vv,
        Err(error) => {
            return RebuildOutcome::Unrecoverable(reopen_message(
                doc_id,
                pending,
                format!("未 ACK 前缀的 end_vv 解码失败（{error}）"),
            ));
        }
    };
    let head_vv = match head.version_vector() {
        Ok(vv) => vv,
        Err(error) => {
            return RebuildOutcome::Unrecoverable(reopen_message(
                doc_id,
                pending,
                format!("新 head 的 VV 解码失败（{error}）"),
            ));
        }
    };
    if head_vv.is_empty() {
        return RebuildOutcome::Unrecoverable(reopen_message(
            doc_id,
            pending,
            format!(
                "新 head（head_seq {}）的 VV 为空，无法证明未 ACK 前缀与新 base 兼容",
                head.head_seq
            ),
        ));
    }
    if !prefix_vv.includes_vv(&head_vv) {
        return RebuildOutcome::Unrecoverable(reopen_message(
            doc_id,
            pending,
            format!(
                "库中 head 已推进到 {} 且包含本会话 live 文档未知的并发 ops；\
                 原样重试会写坏 head_vv/frontier，已拒绝自动重建",
                head.head_seq
            ),
        ));
    }

    // 同一个后台 flusher 线程内重建 sink（复用既有 Vault 写连接，不新建 journal/连接）。
    let mut sink = VaultJournalSink::new(vault.clone(), doc_id.to_owned(), head.head_seq);
    if let Some(sync) = sync {
        sink = sink.with_sync_context(sync);
    }
    match sink.write_prefix(entries) {
        Ok(seq) => RebuildOutcome::Written(seq),
        Err(StorageError::HeadConflict {
            expected, actual, ..
        }) => RebuildOutcome::Unrecoverable(reopen_message(
            doc_id,
            pending,
            format!("自动重建后再发生 head 冲突（基准 {expected}，实际 {actual}）"),
        )),
        Err(error) => rebuild_write_error_outcome(doc_id, pending, error),
    }
}

/// 统一的「需要重开文档」失败文案：闸门/UI 只看这一个字符串就必须知道该做什么。
fn reopen_message(doc_id: &str, pending: usize, reason: String) -> String {
    format!(
        "需要重开文档：{doc_id} 的未 ACK journal 无法安全自动重建（{reason}）；\
         {pending} 条未 ACK 更新保留在当前会话 journal 中（未写入、未静默丢弃）。\
         请关闭并重开该文档，从 durable 前缀重新加载后再继续编辑；\
         重开前未 durable 的本地改动不会自动落库"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sqlite_error(code: rusqlite::ErrorCode, extended_code: i32) -> StorageError {
        StorageError::Sqlite(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code,
                extended_code,
            },
            None,
        ))
    }

    #[test]
    fn retryable_write_error_is_a_positive_whitelist() {
        assert!(is_retryable_write_error(&sqlite_error(
            rusqlite::ErrorCode::DatabaseBusy,
            5
        )));
        assert!(is_retryable_write_error(&sqlite_error(
            rusqlite::ErrorCode::DatabaseLocked,
            6
        )));
        assert!(!is_retryable_write_error(&sqlite_error(
            rusqlite::ErrorCode::ConstraintViolation,
            1811
        )));
        assert!(!is_retryable_write_error(&sqlite_error(
            rusqlite::ErrorCode::ReadOnly,
            8
        )));
        assert!(!is_retryable_write_error(&StorageError::Corrupt {
            context: "test".to_owned(),
        }));
    }

    #[test]
    fn rebuild_write_error_outcome_maps_busy_to_retryable_and_permanent_to_unrecoverable() {
        let busy = sqlite_error(rusqlite::ErrorCode::DatabaseBusy, 5);
        let retryable = rebuild_write_error_outcome("doc-1", 2, busy);
        assert!(matches!(retryable, RebuildOutcome::Retryable(_)));
        let permanent = sqlite_error(rusqlite::ErrorCode::ConstraintViolation, 1811);
        let unrecoverable = rebuild_write_error_outcome("doc-1", 2, permanent);
        assert!(matches!(unrecoverable, RebuildOutcome::Unrecoverable(_)));
    }
}

`````


## `crates/storage/src/journal.rs`

`````rust
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
//! # head 过期：两条恢复路径（P1-3 契约）
//!
//! journal 条目里的 `PendingUpdate` 是**追加那一刻**按当时 head 导出的字节
//! （`end_frontier` / `end_vv` 是那次导出的快照），**不可重算**。如果同一文档的 head
//! 被别的 writer 推进（同步引擎 integrate、`SessionDriver` 的 adopt、另一个 Vault
//! 进程……），前缀的 CAS 基准（`loro_docs.head_seq`）就永远对不上：`append_updates`
//! 每次都返回 [`StorageError::HeadConflict`]，但**同一份字节按旧基准重试多少次都不会
//! 成功**。这正是旧实现「flush 一直 HeadConflict、水位永远不前进」的卡死来源。
//!
//! 本节先写库内契约（[`JournalWriter`]），再写生产 flusher 的实际路径；两者不要混为一谈。
//!
//! ## 1. `JournalWriter`：冻结 writer，调用方重建 sink / 重新导出
//!
//! - [`JournalWriter::flush`] 捕获 sink 的 [`StorageError::HeadConflict`]，**不重试**
//!   （0 次），立即转成终态错误 [`StorageError::JournalHeadStale`]
//!   （`doc_id` / `expected` / `actual` / `pending` 齐全，
//!   [`StorageError::requires_session_rebuild`] 为 `true`）。其它错误（磁盘失败等）
//!   原样透传 —— 那才是可重试的，journal 原样保留。
//! - 进入 stale 之后，`flush` / `flush_all` **不再触碰数据库**，幂等地返回同一个
//!   stale 错误；`submit` 也返回 `JournalHeadStale`（writer 已冻结，新条目必须交给
//!   重建后的 writer）。因此调用方无论怎么轮询，都不会挂死、也不会刷屏重试。
//! - **谁负责重建**：持有 live `BodyDoc` 的调用方，而不是本模块。调用方必须：
//!   ① 把 durable journal 重放到**新 head**（`load_body_doc` / `load_head`），让内存
//!   文档与库一致；② 用新 head 的 VV 重新导出本地意图（`BodyDoc::pending_update`）——
//!   只有这一步得到的字节才能写；③ 用新 `head_seq` 造一个 [`VaultJournalSink`]，
//!   调用 [`JournalWriter::rebuild_sink`] 装回去。
//! - **未 durable 的旧条目怎么办**：`rebuild_sink` 把它们**原样交还**调用方（返回值）
//!   并从 writer 的 journal 里清空。它们**没有被写入、也没有被静默丢弃** —— 调用方
//!   必须用 ② 重新导出的增量覆盖这段内容；若调用方决定不要，也必须显式消费返回值
//!   并在上层报告，不能假装成功。重建后 `durable_seq` 等于新 sink 的 CAS 基准。
//!
//! ## 2. 生产 `session/flush.rs`：兼容 head 时同字节有界重试（当前接线）
//!
//! 生产后台 flusher **不经过** [`JournalWriter`]：它直接在后台线程构造
//! [`VaultJournalSink`] 并调用 `write_prefix`，因此**不会**调用
//! [`JournalWriter::rebuild_sink`]，也不会重新导出增量。它的
//! [`StorageError::HeadConflict`] 路径是**同一份字节前缀的有界重试**：
//!
//! 1. `load_head` 读回当前 `head_seq` 与 VV；
//! 2. **仅当新 head 的 ops 全部包含在**未 ACK 前缀最后一条的 `end_vv` 里
//!    （`prefix_vv.includes_vv(head_vv)`，即外部推进写的是本会话 live 已知的 op）时，
//!    用新 CAS 基准重试**同一份字节前缀一次**；
//! 3. 新 head 含本会话未知 ops、head 未前进/回退、VV 为空或解码失败、重试后仍冲突
//!    → **fail-closed**：不写库、journal 原样保留，`last_write_error` 明确要求
//!    「关闭并重开文档」；后续 flush 立即失败，不再碰数据库。
//!
//! 两条路径的共同点：CAS 冲突在 journal 层的自动重试是 **0** 次（基准是常量，
//! 重试同一份旧字节不可能成功）；有界重试只发生在生产 session flusher，且**仅一次**，
//! 条件不满足就上报告，不无限重试。生产路径**没有第二 journal**：journal 仍是会话
//! 内存里的那一条，重建的只是 sink 的 CAS 基准。

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

`````


## `crates/storage/src/error.rs`

`````rust
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

`````


## `crates/storage/src/body_doc.rs`

`````rust
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

`````


## `crates/storage/src/baseline_apply.rs`

`````rust
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
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
/// 导入后读回校验失败的持久标记（**只本地可见**；不是 schema/wire）。
///
/// 为什么不是 `baseline_apply.status='failed'`：schema v3 的
/// `CHECK(status IN('staging','completed'))` 不允许新值（见
/// `crates/storage/src/schema/v3.sql`），加状态需要 DDL/迁移，超出同步硬化 P1 的边界。
/// 失败语义改由本 meta 键承载：journal 保持 `staging`（已导入的 Loro/资产数据全部
/// 保留，便于诊断/重跑），但 [`begin_baseline_apply`] 见到该键一律拒绝，必须先
/// [`abort_baseline_apply`] 清理（abort 同时删除本键）。
pub const META_BASELINE_VERIFY_FAILED: &str = "sync.baseline_verify_failed";
/// 预期集合编码的魔数（防止误读旧值/损坏值）。
const EXPECTED_MAGIC: &[u8] = b"MNBASEEXP1";
/// 失败标记编码的魔数（`magic | utf8 reason`，防止把任意 BLOB 误读成失败标记）。
const VERIFY_FAILED_MAGIC: &[u8] = b"MNBASEVERIFYFAIL1";
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
    /// 导入后读回校验（sync 的 `verify_imported`）耗时。
    ///
    /// **只读指标**：不参与任何决策；由 sync 的 bootstrap 调用
    /// [`record_baseline_verify_time`] 记录，Phase1B 的 gate_hold 报告可在同一线程
    /// 通过 [`baseline_apply_debug_timings`] 或 [`baseline_apply_verify_ms`] 读取。
    pub verify_ns: u64,
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

/// 记录一次导入后读回校验（sync 的 `verify_imported`）耗时。
///
/// **只读指标**：不参与任何决策，也不持久化；与其它分段计时一样按线程累计。
/// Phase1B 的 gate_hold 报告可在同一线程读取 [`baseline_apply_debug_timings`] 的
/// `verify_ns`（或 [`baseline_apply_verify_ms`]）。
#[doc(hidden)]
pub fn record_baseline_verify_time(elapsed: Duration) {
    let elapsed = u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX);
    BASELINE_DEBUG.with(|acc| {
        let mut acc = acc.borrow_mut();
        acc.timings.verify_ns = acc.timings.verify_ns.saturating_add(elapsed);
    });
}

/// 当前线程累计的读回校验耗时（毫秒，向下取整；只读指标，不参与决策）。
#[doc(hidden)]
pub fn baseline_apply_verify_ms() -> u64 {
    baseline_apply_debug_timings().verify_ns / 1_000_000
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

/// 记录「baseline 导入后的读回校验失败」。
///
/// 只写一条本地 `meta`（BLOB：魔数 + UTF-8 原因），**不改** `baseline_apply.status`
/// （schema v3 的 CHECK 只允许 `staging/completed`，新增 `failed` 需要 DDL/迁移）。
/// journal 与已导入的 Loro/资产数据原样保留：CRDT merge 幂等，回滚已 merge 的数据会
/// 丢掉本地编辑（本地 op 可能已经与基线 ops 合进同一条 update，无法原样拆分）。
///
/// 契约：标记存在时 [`begin_baseline_apply`] 一律拒绝；必须先 [`abort_baseline_apply`]
/// 清理失败 journal（abort 同时删除本标记）。
pub fn mark_baseline_verify_failed(vault: &Vault, reason: &str) -> Result<(), StorageError> {
    let mut value = Vec::with_capacity(VERIFY_FAILED_MAGIC.len() + reason.len());
    value.extend_from_slice(VERIFY_FAILED_MAGIC);
    value.extend_from_slice(reason.as_bytes());
    vault.set_meta(META_BASELINE_VERIFY_FAILED, &value)
}

/// 失败标记存在时统一的拒绝错误：必须带原因，并明确唯一清理入口是 abort。
fn verify_failed_conflict(reason: &str, action: &str) -> StorageError {
    StorageError::SyncConflict {
        context: format!(
            "baseline 导入的读回校验已失败（{META_BASELINE_VERIFY_FAILED}: {reason}）；\
             {action} 已拒绝且零写入，必须先 abort_baseline_apply 清理失败 journal 与标记"
        ),
    }
}

/// 读失败标记的原因；没有标记返回 `None`。
///
/// 标记存在但编码非法（缺魔数/非 UTF-8）返回 [`StorageError::Corrupt`]，不静默当成
/// 「没有失败」——那会让 `begin` 放行一份未知状态的 journal。
pub fn baseline_verify_failure(vault: &Vault) -> Result<Option<String>, StorageError> {
    let Some(bytes) = vault.get_meta(META_BASELINE_VERIFY_FAILED)? else {
        return Ok(None);
    };
    let reason = bytes
        .strip_prefix(VERIFY_FAILED_MAGIC)
        .ok_or_else(|| StorageError::Corrupt {
            context: format!("{META_BASELINE_VERIFY_FAILED} 编码非法（缺魔数）"),
        })?;
    String::from_utf8(reason.to_vec())
        .map(Some)
        .map_err(|_| StorageError::Corrupt {
            context: format!("{META_BASELINE_VERIFY_FAILED} 原因不是合法 UTF-8"),
        })
}

/// 开始一次 baseline apply staging；同参数（含预期参与集合）重复调用幂等，冲突参数返回
/// SyncConflict。
///
/// `expected` 必须覆盖**本 checkpoint 的全部** docs 与资产：finish 前会拿它逐项核对
/// `baseline_apply_docs/applied=1` 与 `baseline_apply_assets/applied=1`（见
/// [`BaselineExpectation`]）。同一 checkpoint/S/cursor 但 `expected` 不同 → 冲突，
/// 不会静默沿用旧集合。
///
/// 若存在 [`META_BASELINE_VERIFY_FAILED`] 标记（上一次导入读回校验失败），本函数**拒绝**
/// 一切 begin（包括同参数幂等续跑），必须先 [`abort_baseline_apply`]。
pub fn begin_baseline_apply(
    vault: &Vault,
    checkpoint_id: &[u8; 16],
    baseline_s: u64,
    source_cursor: &str,
    expected: &BaselineExpectation,
) -> Result<BaselineApplyState, StorageError> {
    // 失败标记优先于幂等/冲突判断：journal 仍是 staging（schema 不允许新 status），
    // 但语义上已不可续跑；必须先 abort 清理失败 journal 与标记。
    if let Some(reason) = baseline_verify_failure(vault)? {
        return Err(StorageError::SyncConflict {
            context: format!(
                "baseline 导入的读回校验已失败（{META_BASELINE_VERIFY_FAILED}: {reason}）；\
                 已 merge 的 Loro/资产数据仍保留，必须先 abort_baseline_apply 清理失败 journal"
            ),
        });
    }
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
///
/// 若存在 [`META_BASELINE_VERIFY_FAILED`] 标记，本函数在**任何解码/写事务之前**返回
/// [`StorageError::SyncConflict`]（带原因并提示先 [`abort_baseline_apply`]），零写入。
pub fn apply_baseline_batch(
    vault: &Vault,
    docs: &[(String, Vec<u8>)],
    tombstones: &[String],
) -> Result<BaselineApplyState, StorageError> {
    // 失败标记优先于一切：journal 仍是 staging（schema 不允许新 status），但语义上
    // 已不可续跑。必须在解码 snapshot / 开写事务之前拒绝，避免继续往失败 journal
    // 里追加数据而让 abort 后的重跑更难判断。
    if let Some(reason) = baseline_verify_failure(vault)? {
        return Err(verify_failed_conflict(&reason, "apply_baseline_batch"));
    }
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
            let Some(seq) = head.head_seq.checked_add(1) else {
                return Err(StorageError::Corrupt {
                    context: format!(
                        "baseline 文档 {} 的 head_seq 已到 u64::MAX，无法追加新 seq",
                        prepared.doc_id
                    ),
                });
            };
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
    // 与 batch/finish 一致：失败标记存在时，资产批也必须在任何解码/写事务之前
    // 拒绝。否则 Phase 1B 的 catchup_integrate 循环调用这批 API 时，会在已判定
    // 失败的 journal 上继续写 baseline_apply_assets/计数，破坏"marker = 完全冻结"。
    if let Some(reason) = baseline_verify_failure(vault)? {
        return Err(verify_failed_conflict(&reason, "apply_baseline_assets"));
    }
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
    // marker 检查必须先于 journal 查询：无 journal 但残留/损坏 marker 时，不能先
    // 返回 `NotFound` 把失败状态掩盖掉；abort 是唯一清理入口。
    if let Some(reason) = baseline_verify_failure(vault)? {
        return Err(verify_failed_conflict(&reason, "finish_baseline_apply"));
    }
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

/// 只清 staging journal（含子表、预期参与集合与失败标记）；已导入的 Loro update/资产保留。
///
/// 没有 staging journal 时仍会清掉可能残留的 [`META_BASELINE_VERIFY_FAILED`]（abort 是
/// 失败标记的唯一清理入口），并返回 `false` 表示没有 journal 可中止。
pub fn abort_baseline_apply(vault: &Vault) -> Result<bool, StorageError> {
    match load_journal(vault)? {
        Some(journal) if journal.status == BaselineApplyStatus::Staging => {}
        _ => {
            // completed journal 的数据不动；只清失败标记（若存在），保持返回 false 的既有语义。
            let _cleared = vault.exec(
                "DELETE FROM meta WHERE key=?1",
                &[Value::Text(META_BASELINE_VERIFY_FAILED.to_owned())],
            )?;
            return Ok(false);
        }
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
        // 失败标记随失败 journal 一起清：abort 之后才允许重新 begin。
        Statement::new(
            "DELETE FROM meta WHERE key=?1",
            vec![Value::Text(META_BASELINE_VERIFY_FAILED.to_owned())],
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
    // finishable 是 **O(1) 近似**：只校验 status 与「已应用计数 == 预期计数」。
    // 逐项 `applied=1`/摘要的精确证明只在 `finish_baseline_apply` 里做：
    //   - 这样 Phase1B 的 catchup 循环每次 batch 后读 state 不会做 O(N) 查询；
    //   - 代价是计数相等但底层行损坏时 finishable 可能短暂为 true，finish 会
    //     `Conflict` 且零写，不会误推进水位。
    //
    // 失败标记存在时 journal 仍是 staging（schema v3 的 CHECK 只允许 staging/completed），
    // 但语义上已不可续跑：即使预期集合全齐也必须 finishable=false。
    let verify_failed = baseline_verify_failure(vault)?.is_some();
    let finishable = !verify_failed
        && journal.status == BaselineApplyStatus::Staging
        && match &expected {
            Some(expected) => {
                journal.applied_docs == expected.doc_count()
                    && journal.applied_assets == expected.asset_count()
            }
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

`````


## `crates/storage/src/sync_bridge.rs`

`````rust
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
/// 清理已被 `integrated` 前缀覆盖的 inbox 行（Phase 1B 追赶后）。
///
/// **安全边界**：只删除 `relay_seq <= up_to_seq` 且同一 SQL 内证明
/// `sync.integrated_seq >= up_to_seq` 的行；证明不成立时一行不删。调用方还必须保证
/// `log_id` 是当前 relay incarnation，且 `up_to_seq` 是实际成功的 checkpoint S。
///
/// 为什么不用先读后删：读 `integrated` 与 DELETE 之间可能被并发推进/回退；
/// `EXISTS` 子查询把证明和删除放在同一条原子语句里。BLOB 按 8 字节 BE 比较。
pub fn prune_covered_inbox(
    vault: &Vault,
    log_id: &[u8; 16],
    up_to_seq: u64,
) -> Result<usize, StorageError> {
    let up_to = i64_of(up_to_seq)?;
    let statements = [Statement::new(
        "DELETE FROM sync_inbox \
         WHERE log_id=?1 AND relay_seq<=?2 \
           AND EXISTS (SELECT 1 FROM meta \
                        WHERE key=?3 AND typeof(value)='blob' AND length(value)=8 \
                          AND value>=?4)",
        vec![
            Value::Blob(log_id.to_vec()),
            Value::Integer(up_to),
            Value::Text(META_SYNC_INTEGRATED_SEQ.to_owned()),
            Value::Blob(up_to_seq.to_be_bytes().to_vec()),
        ],
    )];
    let changed = vault.exec_transaction(&statements, &[])?;
    Ok(changed.first().copied().unwrap_or(0))
}

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

`````


## `crates/storage/src/session/core.rs`

`````rust
//! 会话核：`EditorDocumentPort` 的串行 reducer（ADR 0003 §5）。
//!
//! 一个 [`DocumentSession`] = 一个正文文档的**唯一 live owner**：
//!
//! - 持有 live [`BodyDoc`]（Loro）、generation / session_epoch / live_rev；
//! - 持有唯一未 ACK journal 与事件队列；
//! - 所有编辑走 [`DocumentSession::apply`]，受理顺序**固定**（见方法文档）；
//! - `flush` 只投递命令即返回票据，SQLite 由后台线程写（见 [`super::flush`]）。
//!
//! 端口协议类型在 [`super::protocol`]，本模块**不新增协议字段**（缺字段应先报告 Lead）。
//! 远端导入路径在 [`DocumentSession::apply_remote`]，撤销/重做在
//! [`DocumentSession::undo`] / [`DocumentSession::redo`]，都用同一个 Loro `UndoManager`。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use loro::{UndoManager, VersionVector};
use menghunote_text::UnicodeScalar;
use rusqlite::types::Value;

use super::flush::{lock, JournalHandle};
use super::gate::ExternalCommitGate;
use super::protocol::{
    Applied, ChangeSource, CommandId, DeltaSplice, DocId, DurableAck, EditOutcome, EditRequest,
    FlushReason, FlushTicket, Generation, HistoryState, LiveDelta, LiveRev, MergeInfo,
    OpenSnapshot, PortError, RejectReason, Rejected, ScalarEdit, SessionEpoch, SessionEvent,
    StaleBase,
};
use crate::body_doc::BodyDoc;
use crate::error::StorageError;
use crate::store;
use crate::Vault;

/// 会话配置。
///
/// `flush_delay` 是**测试/故障注入**用的可控慢盘延迟，生产用 `None`；
/// `sync` 是 ADR 0009 切片 2b 的本地意图上下文，`None` = 不产生 `sync_outbox`
/// 行（默认构建与既有调用点行为完全不变）。
#[derive(Debug, Clone, Copy)]
pub struct SessionOptions {
    /// 本机 peer id（每设备唯一）。
    pub peer_id: u64,
    /// 会话纪元：旧会话的迟到请求会被拒。
    pub session_epoch: SessionEpoch,
    /// 未 ACK journal 容量（条）。满了就是背压（拒绝新写，不丢已 ACC 部分）。
    pub journal_capacity: usize,
    /// 一次 A 事务最多合并多少条（慢盘时 pending 会堆到这里为止）。
    pub flush_batch: usize,
    /// 注入的 flush 延迟（模拟慢盘）。
    pub flush_delay: Option<Duration>,
    /// 本地写入的同步上下文；`None` = 不产生出站意图（默认）。
    pub sync: Option<crate::sync_bridge::SyncContext>,
}

impl Default for SessionOptions {
    fn default() -> Self {
        Self {
            peer_id: 1,
            session_epoch: SessionEpoch(1),
            journal_capacity: 64,
            flush_batch: 8,
            flush_delay: None,
            sync: None,
        }
    }
}

/// 一个正文文档的会话（串行 reducer）。
///
/// 字段对 `super::gate` 开放（`pub(crate)`）：外部提交闸门与 adopt 必须直接操作**同一份**
/// live 文档、journal 与水位，复制状态只会制造撕裂。对外部 crate 仍然是完全私有的。
pub struct DocumentSession {
    pub(crate) vault: Vault,
    pub(crate) doc_id: DocId,
    /// 本机 peer id（重放 scratch 副本时用；不影响 import）。
    pub(crate) peer_id: u64,
    generation: Generation,
    pub(crate) session_epoch: SessionEpoch,
    pub(crate) doc: BodyDoc,
    pub(crate) live_rev: LiveRev,
    pub(crate) live_frontier: Vec<u8>,
    /// 上一次写入 journal 的 VV：增量导出的起点（不在 durable 前推进）。
    pub(crate) journal_base_vv: VersionVector,
    pub(crate) journal: JournalHandle,
    undo: UndoManager,
    /// command_id 幂等缓存（只缓存终态：Applied；背压/过期不缓存）。
    command_cache: HashMap<CommandId, EditOutcome>,
    /// composition 深度（`> 0` 表示组合中；嵌套 begin 计数）。
    composition_depth: u32,
    /// composition 期间延后的远端 update（按到达顺序；结束后串行合并，不丢）。
    deferred_remote: Vec<Vec<u8>>,
    /// 外部提交闸门（ADR 0009 切片 3a）；无新线程/新全局状态。
    pub(crate) gate: ExternalCommitGate,
}

impl std::fmt::Debug for DocumentSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DocumentSession")
            .field("doc_id", &self.doc_id)
            .field("generation", &self.generation)
            .field("session_epoch", &self.session_epoch)
            .field("live_rev", &self.live_rev)
            .field("pending", &self.pending_len())
            .finish()
    }
}

impl DocumentSession {
    /// 打开会话：从库里的 snapshot + updates 重放出 live 文档。
    pub fn open(vault: &Vault, doc_id: &str, options: SessionOptions) -> Result<Self, PortError> {
        // 同步上下文在打开时校验一次：epoch/peer 非法要尽早报错，
        // 而不是让之后每一次 flush 都因非法 intent 失败、journal 永远重试。
        if let Some(sync) = options.sync {
            sync.validate().map_err(port)?;
        }
        let generation = load_generation(vault, doc_id)?;
        let (doc, head) =
            store::load_body_doc(vault, doc_id, options.peer_id, None).map_err(port)?;
        let live_frontier = doc.frontiers().encode();
        // 打开时没有未 ACK 条目：增量基准就是 durable 的 VV
        let journal_base_vv = head.version_vector().map_err(port)?;
        let journal = JournalHandle::spawn(
            vault.clone(),
            doc_id.to_owned(),
            head,
            options.journal_capacity,
            options.flush_batch,
            options.flush_delay,
            options.sync,
        );
        let undo = doc.undo_manager();
        Ok(Self {
            vault: vault.clone(),
            doc_id: doc_id.to_owned(),
            peer_id: options.peer_id,
            generation,
            session_epoch: options.session_epoch,
            doc,
            live_rev: LiveRev(0),
            live_frontier,
            journal_base_vv,
            journal,
            undo,
            command_cache: HashMap::new(),
            composition_depth: 0,
            deferred_remote: Vec::new(),
            gate: ExternalCommitGate::default(),
        })
    }

    /// 文档 ID。
    pub fn doc_id(&self) -> &str {
        &self.doc_id
    }

    /// 文档代际。
    pub fn generation(&self) -> Generation {
        self.generation
    }

    /// 会话纪元。
    pub fn session_epoch(&self) -> SessionEpoch {
        self.session_epoch
    }

    /// 当前 live 修订号（`EditRequest.base_live_rev` 的正确基准）。
    pub fn live_rev(&self) -> LiveRev {
        self.live_rev
    }

    /// 当前 live frontier（不透明字节）。
    pub fn live_frontier(&self) -> &[u8] {
        &self.live_frontier
    }

    /// 当前 live 正文（LF 归一后的 Markdown）。
    pub fn markdown(&self) -> String {
        self.doc.text()
    }

    /// 打开快照（调用方据此初始化自己的 Rope 镜像）。
    pub fn snapshot(&self) -> OpenSnapshot {
        OpenSnapshot {
            doc_id: self.doc_id.clone(),
            generation: self.generation,
            session_epoch: self.session_epoch,
            live_rev: self.live_rev,
            live_frontier: self.live_frontier.clone(),
            live_vv: self.doc.vv().encode(),
            // 批次号不跨会话持久化：重启后只能确认 durable 水位（head_seq）
            durable_batch: None,
            durable_head_seq: self.journal.durable_head_seq(),
            markdown: self.doc.text(),
        }
    }

    /// 未 ACK 条数。
    pub fn pending_len(&self) -> usize {
        self.journal.pending_len()
    }

    /// 未 ACK update 的总字节数（调度器用：达到 64 KiB 立即 flush）。
    pub fn pending_bytes(&self) -> usize {
        self.journal.pending_bytes()
    }

    /// 已 durable 的 head seq。
    pub fn durable_head_seq(&self) -> u64 {
        self.journal.durable_head_seq()
    }

    /// 已 durable 到哪个 live 修订号。
    pub fn durable_through(&self) -> LiveRev {
        self.journal.durable_through()
    }

    /// 最近一次 flush 写失败的可读原因（`None` = 没有已知失败）。
    ///
    /// 不可恢复的失败（head 过期且无法安全自动重建）优先，文案明确提示
    /// 「需要重开文档」；否则是最近一次普通写失败（磁盘/SQLite 等，下次写成功后清空）。
    ///
    /// **只读且稳定**：同一失败可以反复读取，UI 可以在状态栏/关闭报告里直接展示；
    /// 不要求调用方先走外部提交闸门（`GateState::Failed` 只有走闸门的路径才看得到）。
    pub fn last_write_error(&self) -> Option<String> {
        self.journal.last_write_error()
    }

    /// flush 是否已进入不可恢复状态（只有重开文档能继续）。
    pub fn flush_is_unrecoverable(&self) -> bool {
        self.journal.is_unrecoverable()
    }

    /// 取走事件队列（Durable / Remote）。
    pub fn events(&self) -> Vec<SessionEvent> {
        self.journal.drain_events()
    }

    /// 等待某个 live 修订号变成 durable（测试/关闭前等待用）。
    ///
    /// 只消费覆盖到 `target` 的 `Durable`，其它事件留在队列里。
    pub fn wait_for_durable(&self, target: LiveRev, timeout: Duration) -> Option<DurableAck> {
        let deadline = Instant::now() + timeout;
        loop {
            let mut found = None;
            {
                let mut queue = lock(self.journal.events_ref());
                let mut remaining = std::collections::VecDeque::new();
                while let Some(event) = queue.pop_front() {
                    match event {
                        SessionEvent::Durable(ack)
                            if ack.through_live_rev >= target && found.is_none() =>
                        {
                            found = Some(ack);
                        }
                        other => remaining.push_back(other),
                    }
                }
                *queue = remaining;
            }
            if let Some(ack) = found {
                return Some(ack);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// 受理一次编辑请求（顺序固定，任何一步失败都**不改 Loro**）。
    ///
    /// 顺序：
    ///
    /// 1. 校验 `doc_id` / `generation` / `session_epoch`；
    /// 2. 按 `command_id` 幂等（终态直接返回首次结果）；
    /// 3. 外部提交闸门 Held → `Rejected{NotReady}`（ADR 0009：Held 期间禁写，fail-closed）；
    /// 4. composition 进行中 → `Rejected{CompositionActive}`；
    /// 5. 校验 `edits`：升序、互不重叠、`end <= scalar_len`；`base_live_rev` 不匹配 → `Stale`；
    /// 6. journal 容量准入（满了 → `Rejected{Backpressure}`，此时 Loro 未被触碰）；
    /// 7. 才允许改 Loro：按**逆序** `delete + insert`，导出增量、进 journal、推进 live_rev。
    pub fn apply(&mut self, request: EditRequest) -> EditOutcome {
        // 1. 身份与代际
        if request.doc_id != self.doc_id {
            return rejected(request.command_id, RejectReason::UnknownDoc);
        }
        if request.generation != self.generation {
            return rejected(request.command_id, RejectReason::WrongGeneration);
        }
        if request.session_epoch != self.session_epoch {
            return rejected(request.command_id, RejectReason::WrongEpoch);
        }

        // 2. 幂等：只命中终态（背压会随 flush 解除，不能缓存）
        if let Some(cached) = self.command_cache.get(&request.command_id) {
            return cached.clone();
        }

        // 2.5 外部提交闸门 Held：一律拒绝新的本地正文改动（fail-closed，不碰 Loro）。
        //     3b 负责让界面在 Held 期间禁写；存储层这里必须自己兜底。
        if self.gate.is_held() {
            return rejected(request.command_id, RejectReason::NotReady);
        }

        // 3. composition 闸门：组合期间拒绝其它本地正文改动（**不碰 Loro**）
        if self.is_composing() {
            return rejected(request.command_id, RejectReason::CompositionActive);
        }

        // 4. 区间校验 + 基准校验（都在改 Loro 之前）
        let scalar_len = self.doc.scalar_len();
        if validate_edits(&request.edits, scalar_len).is_err() {
            return rejected(request.command_id, RejectReason::InvalidRange);
        }
        if request.base_live_rev != self.live_rev {
            return EditOutcome::Stale(StaleBase {
                command_id: request.command_id,
                current_live_rev: self.live_rev,
                compensation: None,
            });
        }

        // 5+6. 容量准入与 Loro 修改在同一把锁内完成：
        //      持锁期间 flush 线程只会**释放**容量，因此准入通过后 append 不可能失败。
        let mut guard = lock(self.journal.state_ref());
        if guard.journal.is_full() {
            drop(guard);
            return rejected(request.command_id, RejectReason::Backpressure);
        }

        if request.edits.is_empty() {
            let outcome = EditOutcome::Applied(Applied {
                command_id: request.command_id.clone(),
                live_rev: self.live_rev,
                live_frontier: self.live_frontier.clone(),
                splice: DeltaSplice {
                    at: UnicodeScalar::new(0),
                    delete_scalars: 0,
                    insert: String::new(),
                },
                local_echo: true,
                source: ChangeSource::Local,
                merge: MergeInfo::default(),
            });
            self.command_cache
                .insert(request.command_id.clone(), outcome.clone());
            return outcome;
        }

        // 逆序应用：后面的编辑先做，前面编辑的坐标才不会失效（协议 §「按逆序应用」）
        self.doc.set_next_commit_origin(&request.origin);
        if self.undo.group_start().is_err() {
            drop(guard);
            return rejected(request.command_id, RejectReason::NotReady);
        }
        for edit in request.edits.iter().rev() {
            let start = edit.start.get();
            let len = edit.end.get() - start;
            if let Err(_error) = self.doc.replace_scalars(start, len, &edit.replacement) {
                // 校验已保证范围内合法；真出错时丢弃分组，不推进任何水位
                self.undo.group_end();
                drop(guard);
                return rejected(request.command_id, RejectReason::InvalidRange);
            }
        }
        self.undo.group_end();

        // 没有产生任何 op（例如空替换）→ 不回显、不进 journal、不推进 live_rev
        let vv_after = self.doc.vv();
        if vv_after == self.journal_base_vv {
            let first = request.edits.first().map_or(0, |edit| edit.start.get());
            let outcome = EditOutcome::Applied(Applied {
                command_id: request.command_id.clone(),
                live_rev: self.live_rev,
                live_frontier: self.live_frontier.clone(),
                splice: DeltaSplice {
                    at: UnicodeScalar::new(first),
                    delete_scalars: 0,
                    insert: String::new(),
                },
                local_echo: true,
                source: ChangeSource::Local,
                merge: MergeInfo::default(),
            });
            self.command_cache
                .insert(request.command_id.clone(), outcome.clone());
            return outcome;
        }

        let update = match self.doc.pending_update(&self.journal_base_vv) {
            Ok(update) => update,
            Err(_) => {
                drop(guard);
                return rejected(request.command_id, RejectReason::NotReady);
            }
        };

        let rev = LiveRev(self.live_rev.0 + 1);
        // 持锁且已准入：这里不可能失败（见上面的注释），失败只可能是内部不一致
        if JournalHandle::append(
            &mut guard,
            &self.doc_id,
            "session",
            crate::journal::JournalOrigin::Local,
            update,
            rev,
        )
        .is_err()
        {
            debug_assert!(false, "持锁且已准入时 journal 追加失败");
            drop(guard);
            return rejected(request.command_id, RejectReason::NotReady);
        }
        drop(guard);

        self.journal_base_vv = vv_after;
        self.live_rev = rev;
        self.live_frontier = self.doc.frontiers().encode();

        let first = request.edits.first();
        let splice = DeltaSplice {
            at: first.map_or(UnicodeScalar::new(0), |edit| edit.start),
            delete_scalars: first.map_or(0, |edit| (edit.end.get() - edit.start.get()) as u64),
            insert: first
                .map(|edit| edit.replacement.clone())
                .unwrap_or_default(),
        };
        let outcome = EditOutcome::Applied(Applied {
            command_id: request.command_id.clone(),
            live_rev: self.live_rev,
            live_frontier: self.live_frontier.clone(),
            splice,
            // 调用方已经把自己的镜像写好了：不得再按 splice 二次应用
            local_echo: true,
            source: ChangeSource::Local,
            merge: MergeInfo::default(),
        });
        self.command_cache
            .insert(request.command_id.clone(), outcome.clone());
        outcome
    }

    /// 开始一次 IME composition（可嵌套，深度计数）。
    ///
    /// preedit **不经过本层**：会话只记录「组合是否在进行」，不接收 preedit 文本，
    /// 也不因 preedit 更新而阻塞。返回当前嵌套深度。
    ///
    /// 外部提交闸门 Held 期间**不进入组合**（fail-closed）并返回当前深度；需要显式
    /// 错误请用 [`DocumentSession::try_begin_composition`]（3b 用）。
    pub fn begin_composition(&mut self) -> u32 {
        match self.try_begin_composition() {
            Ok(depth) => depth,
            Err(_) => self.composition_depth,
        }
    }

    /// 开始一次 IME composition 的**可报错**版本：Held 期间返回 [`PortError`]。
    ///
    /// Held 期间通道已冻结，这里再开组合只会让 preedit 与提交竞争，因此直接拒绝。
    pub fn try_begin_composition(&mut self) -> Result<u32, PortError> {
        if self.gate.is_held() {
            return Err(PortError::Storage(
                "外部提交闸门 Held：拒绝开始 IME 组合".to_owned(),
            ));
        }
        self.composition_depth += 1;
        Ok(self.composition_depth)
    }

    /// 结束一次 composition；只有**最外层**结束才合并延后的远端导入。
    ///
    /// 重复 `end`（没有对应 `begin`）是 no-op：平台可能重复回调 cancel，
    /// 这里不 panic、不改变状态。
    pub fn end_composition(&mut self) -> Vec<LiveDelta> {
        if self.composition_depth > 0 {
            self.composition_depth -= 1;
        }
        if self.is_composing() {
            return Vec::new();
        }
        self.merge_deferred_remote()
    }

    /// 按到达顺序串行合并延后的远端 update。
    ///
    /// - 背压（journal 满）时**保留剩余项**，等 `flush` 之后调用方再调一次；
    /// - 解码/写入失败时也保留队首，不丢远端数据。
    pub fn merge_deferred_remote(&mut self) -> Vec<LiveDelta> {
        let mut merged = Vec::new();
        loop {
            if self.deferred_remote.is_empty() || !self.can_accept_remote() {
                break;
            }
            let Some(update) = self.deferred_remote.first().cloned() else {
                break;
            };
            match self.merge_remote_inner(&update, false) {
                Ok(outcome) => {
                    self.deferred_remote.remove(0);
                    if let Some(delta) = outcome {
                        merged.push(delta);
                    }
                }
                // 保留在队首：不丢远端数据，调用方可稍后重试
                Err(_) => break,
            }
        }
        merged
    }

    /// 是否正处于 composition 中。
    pub fn is_composing(&self) -> bool {
        self.composition_depth > 0
    }

    /// 当前嵌套深度（0 = 不在组合中）。
    pub fn composition_depth(&self) -> u32 {
        self.composition_depth
    }

    /// 仍未合并的远端 update 条数。
    pub fn deferred_remote_len(&self) -> usize {
        self.deferred_remote.len()
    }

    fn can_accept_remote(&self) -> bool {
        // Held 期间不合并延后的远端：闸门要求 live 水位静止，等 end 之后再合并
        !self.journal.is_full() && !self.gate.is_held()
    }

    /// 远端导入：把 update 合进 live 文档，返回驱动镜像的 [`LiveDelta`]。
    ///
    /// 容量不足时拒绝（返回 `PortError::Storage`），保证「背压时 Loro 未被修改」对远端路径同样成立。
    /// Held 期间一律拒绝（fail-closed）：调用方应在 `end_external_commit` 之后重试。
    pub fn apply_remote(&mut self, update: &[u8]) -> Result<Option<LiveDelta>, PortError> {
        if self.gate.is_held() {
            return Err(PortError::Storage(
                "外部提交闸门 Held：暂不接受远端导入".to_owned(),
            ));
        }
        if self.is_composing() {
            // 组合期间延后：进队列（不丢、不报错），结束后按到达顺序串行合并
            self.deferred_remote.push(update.to_vec());
            return Ok(None);
        }
        self.merge_remote_now(update)
    }

    /// 立即合并一条远端 update（**不做** composition 判断；闸门在 [`Self::apply_remote`]）。
    fn merge_remote_now(&mut self, update: &[u8]) -> Result<Option<LiveDelta>, PortError> {
        self.merge_remote_inner(update, true)
    }

    /// `publish = false` 时只返回 delta、不重复推进事件队列（延后合并的唯一投递口是
    /// [`Self::end_composition`] 的返回值，避免调用方把同一条变更应用到镜像两次）。
    fn merge_remote_inner(
        &mut self,
        update: &[u8],
        publish: bool,
    ) -> Result<Option<LiveDelta>, PortError> {
        if self.journal.is_full() {
            return Err(PortError::Storage(
                "未 ACK journal 已满，暂不接受远端导入（背压）".to_owned(),
            ));
        }

        let mut guard = lock(self.journal.state_ref());
        let diff = match import_updates(&self.doc, std::iter::once(update)) {
            Ok(diff) => diff,
            Err(error) => {
                drop(guard);
                return Err(port(error));
            }
        };
        if !diff.changed() {
            drop(guard);
            return Ok(None);
        }

        let splice = diff.splice();
        let after_vv = diff.after_vv;
        let from_rev = self.live_rev;
        let to_rev = LiveRev(self.live_rev.0 + 1);

        // 远端 op 也进 journal（A 事务落库后才算 durable）；容量已准入。
        let pending = match self.doc.pending_update(&self.journal_base_vv) {
            Ok(update) => update,
            Err(error) => {
                drop(guard);
                return Err(port(error));
            }
        };
        if JournalHandle::append(
            &mut guard,
            &self.doc_id,
            "remote",
            // 远端导入**绝不**产生出站意图（ADR 0009）；否则会把刚收到的数据推回 relay。
            crate::journal::JournalOrigin::Remote,
            pending,
            to_rev,
        )
        .is_err()
        {
            drop(guard);
            return Err(PortError::Storage("journal 追加失败".to_owned()));
        }
        drop(guard);

        self.journal_base_vv = after_vv;
        self.live_rev = to_rev;
        self.live_frontier = self.doc.frontiers().encode();

        let delta = LiveDelta {
            doc_id: self.doc_id.clone(),
            from_rev,
            to_rev,
            live_frontier: self.live_frontier.clone(),
            splice,
            cause: None,
            source: ChangeSource::Remote,
            merge: MergeInfo {
                concurrent: true,
                remote_changes: 1,
            },
        };
        if publish {
            lock(self.journal.events_ref()).push_back(SessionEvent::Remote(delta.clone()));
        }
        Ok(Some(delta))
    }

    /// 撤销：用**唯一** `UndoManager` 生成本地新 op（不回退 VV，也不回退远端字符）。
    ///
    /// 返回：
    /// - `Ok(Some(Applied))`：已撤销并进 journal；
    /// - `Ok(None)`：没有可撤销项（no-op undo，应提示调用方消费该项）；
    /// - `Ok(Some(Rejected{Backpressure}))`：journal 满，**未 undo**（Loro 未被修改）；
    /// - `Ok(Some(Rejected{NotReady}))`：外部提交闸门 Held，**未 undo**（fail-closed）。
    pub fn undo(&mut self) -> Result<Option<EditOutcome>, PortError> {
        // Held 期间 undo 也会改 Loro/journal：fail-closed，等 end 之后重试
        if self.gate.is_held() {
            return Ok(Some(rejected(
                CommandId::from("undo"),
                RejectReason::NotReady,
            )));
        }
        // composition 期间 undo/redo 也属于「其它本地正文改动」：拒绝且不碰 Loro
        if self.is_composing() {
            return Ok(Some(rejected(
                CommandId::from("undo"),
                RejectReason::CompositionActive,
            )));
        }
        // 容量准入在改 Loro 之前；准入通过后 flusher 只会释放容量，append 必然成功
        if self.journal.is_full() {
            return Ok(Some(rejected(
                CommandId::from("undo"),
                RejectReason::Backpressure,
            )));
        }
        let before = self.doc.text();
        let before_vv = self.doc.vv();
        if self.undo.group_start().is_err() {
            return Err(PortError::Storage("UndoManager 分组失败".to_owned()));
        }
        let changed = match self.undo.undo() {
            Ok(changed) => changed,
            Err(error) => {
                self.undo.group_end();
                return Err(port_loro(error));
            }
        };
        self.undo.group_end();
        if !changed || self.doc.vv() == before_vv {
            return Ok(None);
        }
        Ok(Some(EditOutcome::Applied(
            self.finish_history_change(&before, ChangeSource::Undo)?,
        )))
    }

    /// 重做：与 [`DocumentSession::undo`] 对称。
    pub fn redo(&mut self) -> Result<Option<EditOutcome>, PortError> {
        if self.gate.is_held() {
            return Ok(Some(rejected(
                CommandId::from("redo"),
                RejectReason::NotReady,
            )));
        }
        if self.is_composing() {
            return Ok(Some(rejected(
                CommandId::from("redo"),
                RejectReason::CompositionActive,
            )));
        }
        if self.journal.is_full() {
            return Ok(Some(rejected(
                CommandId::from("redo"),
                RejectReason::Backpressure,
            )));
        }
        let before = self.doc.text();
        let before_vv = self.doc.vv();
        let changed = match self.undo.redo() {
            Ok(changed) => changed,
            Err(error) => return Err(port_loro(error)),
        };
        if !changed || self.doc.vv() == before_vv {
            return Ok(None);
        }
        Ok(Some(EditOutcome::Applied(
            self.finish_history_change(&before, ChangeSource::Redo)?,
        )))
    }

    /// 撤销/重做共同的收尾：导出增量、进 journal、推进 live_rev、产出 Applied。
    fn finish_history_change(
        &mut self,
        before: &str,
        source: ChangeSource,
    ) -> Result<Applied, PortError> {
        let after_vv = self.doc.vv();
        let update = self
            .doc
            .pending_update(&self.journal_base_vv)
            .map_err(port)?;
        let rev = LiveRev(self.live_rev.0 + 1);
        let label = match source {
            ChangeSource::Undo => "undo",
            ChangeSource::Redo => "redo",
            _ => "history",
        };
        {
            let mut guard = lock(self.journal.state_ref());
            // 外层已做容量准入，且持锁期间容量只增不减 → 这里不会失败
            // undo/redo 是本机产生的新 op（不是远端回放），因此同样产生出站意图。
            if JournalHandle::append(
                &mut guard,
                &self.doc_id,
                label,
                crate::journal::JournalOrigin::Local,
                update,
                rev,
            )
            .is_err()
            {
                debug_assert!(false, "持锁且已准入时 journal 追加失败");
                return Err(PortError::Storage("journal 追加失败".to_owned()));
            }
        }
        self.journal_base_vv = after_vv;
        self.live_rev = rev;
        self.live_frontier = self.doc.frontiers().encode();

        let splice = diff_splice(before, &self.doc.text());
        Ok(Applied {
            command_id: format!("{label}-{}", rev.0),
            live_rev: rev,
            live_frontier: self.live_frontier.clone(),
            splice,
            // 撤销不是用户"已经写好"的输入：调用方要按 splice 更新镜像
            local_echo: false,
            source,
            merge: MergeInfo::default(),
        })
    }

    /// 撤销/重做栈状态。
    pub fn history(&self) -> HistoryState {
        HistoryState {
            can_undo: self.undo.can_undo(),
            undo_count: self.undo.undo_count(),
            can_redo: self.undo.can_redo(),
            redo_count: self.undo.redo_count(),
        }
    }

    /// 受理一次 flush：同步返回票据，后台线程写 A 事务并推 `DurableAck`。
    pub fn flush(&mut self, _reason: FlushReason) -> FlushTicket {
        let ticket_id = self.journal.submit_ticket(self.live_rev);
        FlushTicket {
            ticket_id,
            target_live_rev: self.live_rev,
        }
    }

    /// 已 durable 的 head seq（方便测试/状态栏）。
    pub fn vault(&self) -> &Vault {
        &self.vault
    }
}

fn rejected(command_id: CommandId, reason: RejectReason) -> EditOutcome {
    EditOutcome::Rejected(Rejected { command_id, reason })
}

fn port(error: StorageError) -> PortError {
    PortError::Storage(error.to_string())
}

/// Loro 错误同样归到端口错误（协议不泄露 Loro 类型）。
fn port_loro(error: loro::LoroError) -> PortError {
    PortError::Storage(error.to_string())
}

/// 区间校验：升序、互不重叠、`end <= scalar_len`。
fn validate_edits(edits: &[ScalarEdit], scalar_len: usize) -> Result<(), ()> {
    let mut previous_end = 0usize;
    for (index, edit) in edits.iter().enumerate() {
        let start = edit.start.get();
        let end = edit.end.get();
        if start > end || end > scalar_len {
            return Err(());
        }
        if index > 0 && start < previous_end {
            return Err(());
        }
        previous_end = end;
    }
    Ok(())
}

/// 读文档代际（`loro_docs.generation`），并确认它是正文文档。
fn load_generation(vault: &Vault, doc_id: &str) -> Result<Generation, PortError> {
    let rows = vault
        .query(
            "SELECT generation, kind FROM loro_docs WHERE doc_id = ?1",
            &[Value::Text(doc_id.to_owned())],
        )
        .map_err(port)?;
    let row = rows
        .into_iter()
        .next()
        .ok_or_else(|| PortError::NotOpen(doc_id.to_owned()))?;

    let generation = match row.first() {
        Some(Value::Integer(value)) if *value >= 0 => Generation(*value as u64),
        _ => return Err(PortError::NotOpen(doc_id.to_owned())),
    };
    match row.get(1) {
        Some(Value::Text(kind)) if kind == "body" => Ok(generation),
        _ => Err(PortError::NotOpen(doc_id.to_owned())),
    }
}

/// 一批 update 导入后的文本/版本变化（**不推进任何水位、不碰 journal**）。
pub(crate) struct ImportDiff {
    /// 导入前的正文。
    pub(crate) before_text: String,
    /// 导入后的正文（没有新 op 时与 `before_text` 相同）。
    pub(crate) after_text: String,
    /// 导入后的版本向量。
    pub(crate) after_vv: VersionVector,
    /// 是否真的产生了新的 op（以 VV 为准：文本相同也可能有纯格式 op）。
    changed: bool,
}

impl ImportDiff {
    /// 是否产生了新的 op。
    pub(crate) fn changed(&self) -> bool {
        self.changed
    }

    /// 本次变化的最小 splice（驱动调用方镜像与选区变换）。
    pub(crate) fn splice(&self) -> DeltaSplice {
        diff_splice(&self.before_text, &self.after_text)
    }
}

/// 把一批 update 合进 live 文档，返回前后文本与 VV。
///
/// **只做 Loro 导入**：journal、水位、事件全部由调用方决定——
/// `apply_remote` 走 journal（会产生 A 事务条目），`adopt_committed_delta` 走已 durable
/// 路径（不进 journal、不产生 sync intent）。两条路径共用同一份导入/选区变换逻辑。
pub(crate) fn import_updates<'a, I>(doc: &BodyDoc, updates: I) -> Result<ImportDiff, StorageError>
where
    I: IntoIterator<Item = &'a [u8]>,
{
    let before_text = doc.text();
    let before_vv = doc.vv();
    for update in updates {
        doc.import(update)?;
    }
    let after_vv = doc.vv();
    let changed = after_vv != before_vv;
    let after_text = if changed {
        doc.text()
    } else {
        before_text.clone()
    };
    Ok(ImportDiff {
        before_text,
        after_text,
        after_vv,
        changed,
    })
}

/// 以 scalar 为单位求「最小差异」splice（公共前缀 + 公共后缀）。
pub(crate) fn diff_splice(before: &str, after: &str) -> DeltaSplice {
    let before_chars: Vec<char> = before.chars().collect();
    let after_chars: Vec<char> = after.chars().collect();

    let mut prefix = 0usize;
    while prefix < before_chars.len()
        && prefix < after_chars.len()
        && before_chars[prefix] == after_chars[prefix]
    {
        prefix += 1;
    }

    let mut suffix = 0usize;
    while suffix < before_chars.len() - prefix
        && suffix < after_chars.len() - prefix
        && before_chars[before_chars.len() - 1 - suffix]
            == after_chars[after_chars.len() - 1 - suffix]
    {
        suffix += 1;
    }

    let delete_scalars = before_chars.len() - prefix - suffix;
    let insert: String = after_chars[prefix..after_chars.len() - suffix]
        .iter()
        .collect();
    DeltaSplice {
        at: UnicodeScalar::new(prefix),
        delete_scalars: delete_scalars as u64,
        insert,
    }
}

`````


## `crates/storage/src/session/schedule.rs`

`````rust
//! 存储侧调度（ADR 0003 切片 4a）：自动 flush + B 物化后台驱动。
//!
//! ## 自动 flush（ADR 0002 §9）
//!
//! - **固定窗口**：从「首次 dirty」起算 250 ms，**后续输入不重置**（不是尾缘 debounce），
//!   否则连续输入会无限延期；
//! - **64 KiB**：未 ACK 的 update 字节数达到阈值立即 flush；
//! - `Ctrl+S` / 切换 / 关闭仍可显式 [`SessionDriver::flush`]；[`SessionDriver::close`] 有界排空。
//!
//! 时钟通过 [`Clock`] 注入：生产用 [`SystemClock`]（墙钟），测试用 [`ManualClock`]
//! 直接 [`ManualClock::set`] 推进——不靠真睡 250 ms 碰运气。
//!
//! ## B 物化
//!
//! 只有收到 [`SessionEvent::Durable`] 之后才把 B 任务入队；后台线程按 ACK 里的
//! `head_seq` 从**固定 frontier**调用 [`materialize_body`]（内部 `end_frontier_at` + CAS），
//! 因此：
//!
//! - 不读取、不修改正在编辑的 live doc（worker 自己从库里重放）；
//! - 旧任务迟到时 CAS 命中 0 行 → `Stale`，不会覆盖新结果；
//! - 失败会记录在 [`DriverEvent::MaterializeFailed`]，可用 [`SessionDriver::retry_failed`] 重试。
//!
//! 调用方线程只做内存操作：`apply` / `flush` / `pump` 都不等待 SQLite；只有 `close`
//! 与显式 `materialize_now` 会有界等待。
//!
//! ## 切片 3b：外部提交闸门 / adopt 的驱动转发
//!
//! [`SessionDriver`] 不重新实现闸门语义，只把 [`DocumentSession`] 的 3a API 原样转发，
//! 并把 adopt 的结果变成**驱动事件**：
//!
//! - `adopt` 成功后把 `AdoptOutcome.delta` 作为 [`DriverEvent::Remote`]、把 `ack` 作为
//!   [`DriverEvent::Durable`] 推入事件队列（顺序固定：先 Remote 再 Durable），
//!   这样 UI 的 pump 就能在不重开文档的情况下更新镜像与 durable 水位；
//! - **绝不**调用会 journal 的 `apply_remote`：那份 update 已经 durable，
//!   再走一次会让 flusher 重复写库并产生多余的出站意图；
//! - Held 期间 `apply` 的拒绝（[`super::protocol::RejectReason::NotReady`]）原样透传，
//!   驱动不额外吞错、也不替调用方兜底。

use std::collections::VecDeque;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use rusqlite::types::Value;

use super::core::{DocumentSession, SessionOptions};
use super::gate::{
    AdoptError, AdoptOutcome, AppliedHeads, ExternalCommitLease, ExternalCommitTicket, GateError,
    GateState, PreparedCommittedDelta,
};
use super::protocol::{
    DurableAck, EditOutcome, EditRequest, FlushReason, FlushTicket, HistoryState, LiveDelta,
    LiveRev, PortError, SessionEvent,
};
use crate::error::StorageError;
use crate::materialize::{materialize_body, MaterializeOutcome};
use crate::Vault;

/// 取锁；毒化也继续用内部值。
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 可注入时钟：返回「驱动启动以来」的时长。
pub trait Clock: Send + 'static {
    /// 当前虚拟/墙钟时刻。
    fn now(&self) -> Duration;
}

/// 墙钟（生产用）。
#[derive(Debug)]
pub struct SystemClock {
    started: Instant,
}

impl SystemClock {
    /// 从现在开始计时。
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Duration {
        self.started.elapsed()
    }
}

/// 手动时钟（测试用）：显式 `set`/`advance`，不依赖真实时间。
#[derive(Debug, Clone)]
pub struct ManualClock {
    now: Arc<Mutex<Duration>>,
}

impl ManualClock {
    /// 从 0 开始。
    pub fn new() -> Self {
        Self {
            now: Arc::new(Mutex::new(Duration::ZERO)),
        }
    }

    /// 设置到某个时刻。
    pub fn set(&self, at: Duration) {
        *lock(&self.now) = at;
    }

    /// 前进一段时间。
    pub fn advance(&self, by: Duration) {
        let mut now = lock(&self.now);
        *now += by;
    }

    /// 读取当前时刻。
    pub fn get(&self) -> Duration {
        *lock(&self.now)
    }
}

impl Default for ManualClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Duration {
        self.get()
    }
}

/// 自动 flush 策略。
#[derive(Debug, Clone, Copy)]
pub struct FlushPolicy {
    /// 首次 dirty 起算的固定窗口（默认 250 ms）。
    pub interval: Duration,
    /// 未 ACK update 字节阈值（默认 64 KiB）。
    pub size_bytes: usize,
}

impl Default for FlushPolicy {
    fn default() -> Self {
        Self {
            interval: Duration::from_millis(250),
            size_bytes: 64 * 1024,
        }
    }
}

/// 驱动配置。
#[derive(Debug, Clone, Copy)]
pub struct DriverOptions {
    /// 会话配置。
    pub session: SessionOptions,
    /// flush 策略。
    pub policy: FlushPolicy,
    /// 是否启动墙钟 ticker（生产 `true`；测试用假时钟时可关）。
    pub auto_tick: bool,
    /// ticker 间隔（默认 = `policy.interval`）。
    pub tick_interval: Option<Duration>,
}

impl Default for DriverOptions {
    fn default() -> Self {
        Self {
            session: SessionOptions::default(),
            policy: FlushPolicy::default(),
            auto_tick: true,
            tick_interval: None,
        }
    }
}

/// 驱动向调用方推送的事件（不含请求的同步回复）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriverEvent {
    /// 已提交一张 flush 票据（后台还没写完）。
    FlushScheduled {
        /// 票据。
        ticket: FlushTicket,
        /// 触发原因。
        reason: FlushReason,
    },
    /// durable 水位推进（同时会触发 B 物化入队）。
    Durable(DurableAck),
    /// 远端变更。
    Remote(LiveDelta),
    /// B 物化完成。
    Materialized(MaterializeOutcome),
    /// B 物化失败（已记录，可 `retry_failed` 重试）。
    MaterializeFailed {
        /// 笔记 ID。
        note_id: String,
        /// 失败信息。
        message: String,
    },
}

/// `close` 的排空报告。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseReport {
    /// 覆盖到最后一个 live_rev 的 ACK（超时为 `None`）。
    pub durable: Option<DurableAck>,
    /// 本次关闭过程中完成的 B 物化数。
    pub materialized: usize,
    /// 仍未 ACK 的编辑条数（正常情况下为 0）。
    pub pending_edits: usize,
    /// 仍未完成的 B 任务数（正常情况下为 0）。
    pub pending_materialize: usize,
    /// 是否超时（`true` 表示没排空，调用方应保留现场）。
    pub timed_out: bool,
    /// 关闭时仍未解决的 flush 写失败原因（`None` = 没有已知失败）。
    ///
    /// 取自 [`DocumentSession::last_write_error`]：不可恢复的失败（文案含
    /// 「需要重开文档」）优先，否则是最近一次普通写失败。关闭之后会话可能被丢弃，
    /// 调用方只能靠这份报告知道「没排空是为什么」，因此它必须随报告一起返回。
    pub flush_error: Option<String>,
}

impl CloseReport {
    /// 这次关闭是否暴露了需要“重开文档”的不可恢复失败。
    ///
    /// 只看 `flush_error` 的统一文案（`reopen_message`）；普通磁盘/SQLite 的瞬时失败
    /// 也可能带 `flush_error`，但允许下一次再 flush，不应被 UI 当成永久坏状态。
    #[must_use]
    pub fn needs_reopen(&self) -> bool {
        self.flush_error
            .as_deref()
            .is_some_and(|message| message.contains("需要重开文档"))
    }
}

/// 一条 B 物化任务（只带固定 seq，不带 live 句柄）。
#[derive(Debug, Clone, PartialEq, Eq)]
struct MaterializeJob {
    note_id: String,
    doc_id: String,
    target_seq: u64,
}

/// B 物化后台线程。
struct Materializer {
    sender: Option<Sender<MaterializeJob>>,
    results: Arc<Mutex<VecDeque<MaterializeOutcome>>>,
    errors: Arc<Mutex<VecDeque<(MaterializeJob, String)>>>,
    pending: Arc<Mutex<usize>>,
    worker: Option<JoinHandle<()>>,
}

impl Materializer {
    fn spawn(vault: Vault, peer_id: u64, delay: Option<Duration>) -> Self {
        let (sender, receiver) = channel::<MaterializeJob>();
        let results: Arc<Mutex<VecDeque<MaterializeOutcome>>> =
            Arc::new(Mutex::new(VecDeque::new()));
        let errors: Arc<Mutex<VecDeque<(MaterializeJob, String)>>> =
            Arc::new(Mutex::new(VecDeque::new()));
        let pending = Arc::new(Mutex::new(0usize));

        let worker_results = Arc::clone(&results);
        let worker_errors = Arc::clone(&errors);
        let worker_pending = Arc::clone(&pending);
        let worker = thread::Builder::new()
            .name("menghunote-materialize".to_owned())
            .spawn(move || {
                while let Ok(job) = receiver.recv() {
                    if let Some(delay) = delay {
                        thread::sleep(delay);
                    }
                    // 运行期再解析 note_id：笔记行可能还没投影出来（失败要能记录并重试）
                    let outcome = run_materialize(&vault, &job, peer_id);
                    match outcome {
                        Ok(outcome) => lock(&worker_results).push_back(outcome),
                        Err(error) => {
                            let message = error.to_string();
                            lock(&worker_errors).push_back((job, message));
                        }
                    }
                    let mut pending = lock(&worker_pending);
                    *pending = pending.saturating_sub(1);
                }
            })
            .ok();

        Self {
            sender: Some(sender),
            results,
            errors,
            pending,
            worker,
        }
    }

    fn enqueue(&self, job: MaterializeJob) {
        if let Some(sender) = self.sender.as_ref() {
            *lock(&self.pending) += 1;
            if sender.send(job).is_err() {
                let mut pending = lock(&self.pending);
                *pending = pending.saturating_sub(1);
            }
        }
    }

    fn pending(&self) -> usize {
        *lock(&self.pending)
    }

    fn take_results(&self) -> Vec<MaterializeOutcome> {
        lock(&self.results).drain(..).collect()
    }

    fn take_errors(&self) -> Vec<(MaterializeJob, String)> {
        lock(&self.errors).drain(..).collect()
    }
}

impl Drop for Materializer {
    fn drop(&mut self) {
        drop(self.sender.take());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// 墙钟 ticker：按固定间隔投递 `()`，驱动 `pump` 检查固定窗口。
struct Ticker {
    stop: Option<Sender<()>>,
    worker: Option<JoinHandle<()>>,
}

impl Ticker {
    fn spawn(interval: Duration) -> (Self, Receiver<()>) {
        let interval = interval.max(Duration::from_millis(1));
        let (tick_sender, tick_receiver) = channel::<()>();
        let (stop_sender, stop_receiver) = channel::<()>();

        let worker = thread::Builder::new()
            .name("menghunote-tick".to_owned())
            .spawn(move || loop {
                match stop_receiver.recv_timeout(interval) {
                    Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        if tick_sender.send(()).is_err() {
                            break;
                        }
                    }
                }
            })
            .ok();

        (
            Self {
                stop: Some(stop_sender),
                worker,
            },
            tick_receiver,
        )
    }
}

impl Drop for Ticker {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// 会话驱动：会话核 + 自动 flush 调度 + B 物化后台线程。
pub struct SessionDriver {
    session: DocumentSession,
    vault: Vault,
    peer_id: u64,
    policy: FlushPolicy,
    clock: Box<dyn Clock>,
    /// 首次 dirty 的虚拟时刻；`None` = 没有等待窗口的脏数据。
    dirty_since: Option<Duration>,
    /// 已提交 flush 覆盖到的 live_rev（避免空 flush 造成重复票据）。
    flushed_live_rev: LiveRev,
    materializer: Materializer,
    /// B 失败任务（可 `retry_failed`）。
    failed_jobs: Vec<MaterializeJob>,
    /// 只负责保活：Drop 时停线程；逻辑上不读取。
    _ticker: Option<Ticker>,
    tick_receiver: Option<Receiver<()>>,
    events: VecDeque<DriverEvent>,
}

impl std::fmt::Debug for SessionDriver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionDriver")
            .field("doc_id", &self.session.doc_id())
            .field("live_rev", &self.session.live_rev())
            .field("dirty_since", &self.dirty_since)
            .field("pending_bytes", &self.session.pending_bytes())
            .finish()
    }
}

impl SessionDriver {
    /// 打开驱动（墙钟 + ticker）。
    pub fn open(vault: &Vault, doc_id: &str, options: DriverOptions) -> Result<Self, PortError> {
        Self::open_with_clock(vault, doc_id, options, Box::new(SystemClock::new()))
    }

    /// 打开驱动并注入时钟（测试用假时钟）。
    pub fn open_with_clock(
        vault: &Vault,
        doc_id: &str,
        options: DriverOptions,
        clock: Box<dyn Clock>,
    ) -> Result<Self, PortError> {
        Self::open_with_clock_and_materialize_delay(vault, doc_id, options, clock, None)
    }

    /// 打开驱动并注入时钟 + **B 物化延迟**（测试/故障注入，与 `SessionOptions::flush_delay` 同性质）。
    ///
    /// 延迟是构造参数而不是 `DriverOptions` 字段：后者是公开结构体，
    /// 加字段会让外部（editor 的 e2e 夹具）的结构体字面量编译失败。
    pub fn open_with_clock_and_materialize_delay(
        vault: &Vault,
        doc_id: &str,
        options: DriverOptions,
        clock: Box<dyn Clock>,
        materialize_delay: Option<Duration>,
    ) -> Result<Self, PortError> {
        let session = DocumentSession::open(vault, doc_id, options.session)?;
        let materializer =
            Materializer::spawn(vault.clone(), options.session.peer_id, materialize_delay);
        let (ticker, tick_receiver) = if options.auto_tick {
            let interval = options.tick_interval.unwrap_or(options.policy.interval);
            let (ticker, receiver) = Ticker::spawn(interval);
            (Some(ticker), Some(receiver))
        } else {
            (None, None)
        };
        Ok(Self {
            session,
            vault: vault.clone(),
            peer_id: options.session.peer_id,
            policy: options.policy,
            clock,
            dirty_since: None,
            flushed_live_rev: LiveRev(0),
            materializer,
            failed_jobs: Vec::new(),
            _ticker: ticker,
            tick_receiver,
            events: VecDeque::new(),
        })
    }

    /// 会话核（只读能力：快照/历史/水位）。
    pub fn session(&self) -> &DocumentSession {
        &self.session
    }

    /// 当前 live 正文。
    pub fn markdown(&self) -> String {
        self.session.markdown()
    }

    /// 当前 live 修订号。
    pub fn live_rev(&self) -> LiveRev {
        self.session.live_rev()
    }

    /// 未 ACK update 字节数。
    pub fn pending_bytes(&self) -> usize {
        self.session.pending_bytes()
    }

    /// 未 ACK 编辑条数。
    pub fn pending_edits(&self) -> usize {
        self.session.pending_len()
    }

    /// 最近一次 flush 写失败的可读原因（转发会话核的只读快照）。
    ///
    /// 不可恢复的失败（「需要重开文档」）优先，否则是最新一次普通写失败；`None` = 没失败。
    /// UI 不必等 `pump` 或走外部提交闸门就能读到它。
    #[must_use]
    pub fn last_write_error(&self) -> Option<String> {
        self.session.last_write_error()
    }

    /// 下一次固定窗口到期时刻（`None` 表示当前没有等待中的脏数据）。
    pub fn next_deadline(&self) -> Option<Duration> {
        self.dirty_since.map(|since| since + self.policy.interval)
    }

    /// 开始组合（透传会话闸门）。
    pub fn begin_composition(&mut self) -> u32 {
        self.session.begin_composition()
    }

    /// 结束组合：合并延后的远端导入；这些 op 已进 journal，需要计入 dirty。
    pub fn end_composition(&mut self) -> Vec<LiveDelta> {
        let deltas = self.session.end_composition();
        if !deltas.is_empty() {
            self.mark_dirty();
        }
        deltas
    }

    /// 背压导致延后队列没排空时，flush 之后再调一次。
    pub fn merge_deferred_remote(&mut self) -> Vec<LiveDelta> {
        let deltas = self.session.merge_deferred_remote();
        if !deltas.is_empty() {
            self.mark_dirty();
        }
        deltas
    }

    /// 是否正处于 composition。
    pub fn is_composing(&self) -> bool {
        self.session.is_composing()
    }

    /// 仍未合并的远端 update 条数。
    pub fn deferred_remote_len(&self) -> usize {
        self.session.deferred_remote_len()
    }

    /// 受理编辑；首次 dirty 记录窗口起点，达到 64 KiB 立即 flush。
    pub fn apply(&mut self, request: EditRequest) -> EditOutcome {
        let outcome = self.session.apply(request);
        if matches!(outcome, EditOutcome::Applied(_)) {
            self.mark_dirty();
        }
        outcome
    }

    /// 远端导入：同样计入 dirty（远端 op 也要落 durable）。
    ///
    /// **已 durable** 的外部增量不要走这里（见 [`Self::adopt_committed_delta`]）：
    /// 这条路径会 journal 并产生出站意图。
    pub fn apply_remote(&mut self, update: &[u8]) -> Result<Option<LiveDelta>, PortError> {
        let delta = self.session.apply_remote(update)?;
        if delta.is_some() {
            self.mark_dirty();
        }
        Ok(delta)
    }

    // ---- 外部提交闸门 / adopt（ADR 0009 切片 3b：只转发 3a 的存储层语义）----

    /// Held 期间本地写入是否被冻结（= 外部提交闸门持有中）。
    #[must_use]
    pub fn is_external_commit_held(&self) -> bool {
        self.session.is_external_commit_held()
    }

    /// 当前外部提交租约（`None` = 不在 Held）。
    #[must_use]
    pub fn external_commit_lease(&self) -> Option<&ExternalCommitLease> {
        self.session.external_commit_lease()
    }

    /// 登记一次外部提交（只建立票据；重复 begin 由会话拒绝）。
    pub fn begin_external_commit(&mut self) -> Result<ExternalCommitTicket, GateError> {
        self.session.begin_external_commit()
    }

    /// 非阻塞推进闸门：`WaitingIME` → `Flushing` → `Held(lease)`。
    ///
    /// 进入 Held 时顺手把调度器自己的水位对齐：闸门的成立条件就是「目标已 durable 且
    /// journal 空闲」，因此这里不再欠这次 flush（否则下一次 pump 会为已经不脏的数据
    /// 再投一张空票据）。
    pub fn poll_external_commit(&mut self, ticket: &ExternalCommitTicket) -> GateState {
        let state = self.session.poll_external_commit(ticket);
        if let GateState::Held(lease) = &state {
            self.dirty_since = None;
            self.flushed_live_rev = self.flushed_live_rev.max(lease.target_live_rev());
        }
        state
    }

    /// adopt 一份**已 durable** 的外部增量（必须 Held 且租约匹配）。
    ///
    /// 成功后把增量与 ACK 变成驱动事件（**先 Remote 再 Durable**），`pump` / `tick`
    /// 立刻能取到；失败时 live 与 durable 水位都不变（3a 保证）。
    pub fn adopt_committed_delta(
        &mut self,
        lease: &ExternalCommitLease,
        prepared: PreparedCommittedDelta,
    ) -> Result<AdoptOutcome, AdoptError> {
        let outcome = self.session.adopt_committed_delta(lease, prepared)?;
        // 不调用 `apply_remote`：这份 CRDT 已经 durable，再 journal 一次会重复写库 + 产生意图。
        self.events
            .push_back(DriverEvent::Remote(outcome.delta.clone()));
        self.push_durable(outcome.ack.clone());
        // adopt 要求 journal 空闲，因此"欠一次 flush"的水位到此为止。
        self.dirty_since = None;
        self.flushed_live_rev = self.flushed_live_rev.max(outcome.ack.through_live_rev);
        Ok(outcome)
    }

    /// 结束外部提交并释放闸门（`applied` 必须就是库中当前 head）。
    ///
    /// 失败/取消路径下 3a 会把 Held 期间已 durable 的变化交付进 live 并作为
    /// `SessionEvent::Remote` 入队，驱动会在下一次 `pump` 转发成 [`DriverEvent::Remote`]。
    pub fn end_external_commit(
        &mut self,
        lease: ExternalCommitLease,
        applied: AppliedHeads,
    ) -> Result<(), GateError> {
        self.session.end_external_commit(lease, applied)
    }

    /// 放弃一次外部提交（失败/取消），同样先交付已 durable 的变化。
    ///
    /// 返回的错误原样来自会话（例如仍 `pending` 时 [`GateError::Pending`]）；交付的增量
    /// 会作为会话事件由下一次 `pump` 转发，这里不重复推送，避免同一份 delta 应用两次。
    pub fn cancel_external_commit(
        &mut self,
        ticket: &ExternalCommitTicket,
    ) -> Result<(), GateError> {
        self.session.cancel_external_commit(ticket).map(|_| ())
    }

    /// **固定窗口**：只在首次 dirty 时记录起点，后续输入不重置。
    fn mark_dirty(&mut self) {
        if self.dirty_since.is_none() {
            self.dirty_since = Some(self.clock.now());
        }
        if self.session.pending_bytes() >= self.policy.size_bytes
            && self.session.live_rev() > self.flushed_live_rev
        {
            self.flush(FlushReason::Size);
        }
    }

    /// 撤销（转发会话核的唯一 `UndoManager`）。
    ///
    /// dirty 规则与 [`SessionDriver::apply`] 对齐：
    /// - `Ok(Some(Applied))` → `mark_dirty()`（撤销也是正文改动，必须落 durable）；
    /// - `Ok(None)`（没有可撤销项）/ `Ok(Some(Rejected))` / `Stale`（不会出现）→ **不** dirty；
    /// - `Err(PortError)` → 原样透传，不 dirty。
    pub fn undo(&mut self) -> Result<Option<EditOutcome>, PortError> {
        let outcome = self.session.undo()?;
        if matches!(&outcome, Some(EditOutcome::Applied(_))) {
            self.mark_dirty();
        }
        Ok(outcome)
    }

    /// 重做；dirty 规则同 [`SessionDriver::undo`]。
    pub fn redo(&mut self) -> Result<Option<EditOutcome>, PortError> {
        let outcome = self.session.redo()?;
        if matches!(&outcome, Some(EditOutcome::Applied(_))) {
            self.mark_dirty();
        }
        Ok(outcome)
    }

    /// 撤销/重做栈状态（真实计数来自 Loro `UndoManager`）。
    pub fn history(&self) -> HistoryState {
        self.session.history()
    }

    /// 手动/按原因 flush（同步返回票据，不等待 SQLite）。
    pub fn flush(&mut self, reason: FlushReason) -> FlushTicket {
        let ticket = self.session.flush(reason);
        self.flushed_live_rev = ticket.target_live_rev;
        self.dirty_since = None;
        self.events
            .push_back(DriverEvent::FlushScheduled { ticket, reason });
        ticket
    }

    /// 推进到指定虚拟时刻（测试用于精确控制固定窗口）。
    pub fn tick(&mut self, now: Duration) -> Vec<DriverEvent> {
        self.advance_to(now);
        self.collect_session_events();
        self.collect_materialize_results();
        self.drain_events()
    }

    /// 非阻塞泵：读 ticker、检查固定窗口、收 session/B 事件。
    pub fn pump(&mut self) -> Vec<DriverEvent> {
        self.take_ticks();
        let now = self.clock.now();
        self.advance_to(now);
        self.collect_session_events();
        self.collect_materialize_results();
        self.drain_events()
    }

    fn take_ticks(&mut self) {
        let Some(receiver) = self.tick_receiver.as_ref() else {
            return;
        };
        // 只做「消化信号」：固定窗口是否到期由虚拟/墙钟时刻决定
        while receiver.try_recv().is_ok() {}
    }

    fn advance_to(&mut self, now: Duration) {
        if let Some(since) = self.dirty_since {
            if now >= since + self.policy.interval {
                self.flush(FlushReason::Interval);
            }
        }
    }

    fn collect_session_events(&mut self) {
        for event in self.session.events() {
            match event {
                SessionEvent::Durable(ack) => self.push_durable(ack),
                SessionEvent::Remote(delta) => self.events.push_back(DriverEvent::Remote(delta)),
            }
        }
    }

    /// 推入一条 `Durable` 事件，并按既有约定入队 B 物化。
    ///
    /// 两条来源共用：flush 线程的 ACK，以及 adopt 之后由外部提交推进的 head（3b）。
    fn push_durable(&mut self, ack: DurableAck) {
        // DurableAck 之后才入队 B（用 ACK 的固定 head_seq，不碰 live doc）。
        self.enqueue_materialize(&ack);
        self.events.push_back(DriverEvent::Durable(ack));
    }

    /// DurableAck 之后才入队 B（用 ACK 的固定 head_seq，不碰 live doc）。
    fn enqueue_materialize(&mut self, ack: &DurableAck) {
        if ack.head_seq == 0 {
            return;
        }
        let doc_id = self.session.doc_id().to_owned();
        // note_id 只是给事件用的提示；权威解析在 worker 运行期做
        let note_hint = lookup_note_id(&self.vault, &doc_id)
            .ok()
            .flatten()
            .unwrap_or_default();
        self.materializer.enqueue(MaterializeJob {
            note_id: note_hint,
            doc_id,
            target_seq: ack.head_seq,
        });
    }

    fn collect_materialize_results(&mut self) {
        for outcome in self.materializer.take_results() {
            self.events.push_back(DriverEvent::Materialized(outcome));
        }
        for (job, message) in self.materializer.take_errors() {
            self.events.push_back(DriverEvent::MaterializeFailed {
                note_id: job.note_id.clone(),
                message,
            });
            self.failed_jobs.push(job);
        }
    }

    fn drain_events(&mut self) -> Vec<DriverEvent> {
        self.events.drain(..).collect()
    }

    /// 有界排空：flush(Close) → 等 durable → 等 B 完成。
    ///
    /// 调用方可以是 UI 线程（关闭窗口），但必须有界：`timeout` 到了就返回并置 `timed_out`。
    pub fn close(&mut self, timeout: Duration) -> CloseReport {
        let target = self.session.live_rev();
        let needs_flush = self.session.pending_len() > 0 || target > self.flushed_live_rev;
        if needs_flush {
            self.flush(FlushReason::Close);
        }

        let deadline = Instant::now() + timeout;
        let mut durable: Option<DurableAck> = None;
        let mut materialized = 0usize;

        loop {
            for event in self.pump() {
                match event {
                    DriverEvent::Durable(ack) if ack.through_live_rev >= target => {
                        durable = Some(ack);
                    }
                    DriverEvent::Materialized(_) => materialized += 1,
                    _ => {}
                }
            }

            let durable_ok = durable.is_some() || !needs_flush;
            let drained = durable_ok && self.materializer.pending() == 0;
            // 不可恢复的 flush 不会再有 DurableAck：不要等满 timeout，直接把报告
            // （含 flush_error）交给调用方。正常磁盘错误不走这里，仍按原超时收敛。
            if self.session.flush_is_unrecoverable() {
                break;
            }
            if drained || Instant::now() >= deadline {
                // 收尾：把最后一批 B 结果也计入
                for event in self.pump() {
                    if matches!(event, DriverEvent::Materialized(_)) {
                        materialized += 1;
                    }
                }
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }

        CloseReport {
            durable,
            materialized,
            pending_edits: self.session.pending_len(),
            pending_materialize: self.materializer.pending(),
            timed_out: self.session.pending_len() > 0 || self.materializer.pending() > 0,
            // 关闭后会话可能被丢弃：没排空的原因必须随报告带走（不可恢复优先）。
            flush_error: self.session.last_write_error(),
        }
    }

    /// 显式同步物化（启动修复/测试用）：从固定 `target_seq` 走 CAS。
    pub fn materialize_now(
        &mut self,
        note_id: &str,
        doc_id: &str,
        target_seq: u64,
    ) -> Result<MaterializeOutcome, StorageError> {
        materialize_body(&self.vault, note_id, doc_id, target_seq, self.peer_id)
    }

    /// 重新入队失败的 B 任务；返回重试条数。
    pub fn retry_failed(&mut self) -> usize {
        let jobs = std::mem::take(&mut self.failed_jobs);
        let count = jobs.len();
        for job in jobs {
            self.materializer.enqueue(job);
        }
        count
    }

    /// 已 durable 的 head seq。
    pub fn durable_head_seq(&self) -> u64 {
        self.session.durable_head_seq()
    }

    /// 等待 B 队列排空（测试/关闭辅助），返回是否排空。
    ///
    /// **契约**：返回 `true` 时，worker 已完成的 B 结果都已经被搬进事件队列并取走，
    /// 之后的 `pump()` 不会再冒出「上一次等待之前就该完成」的 `Materialized` 事件。
    /// （worker 是「先 push 结果、再减 pending」，所以 pending 归零后再泵一次才能收干净。）
    pub fn wait_materialize(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            // 搬走已完成的结果/错误（本方法是等待辅助，事件直接丢弃）
            for _ in self.pump() {}
            if self.materializer.pending() == 0 {
                // 收尾：把「pending 已减、结果刚入队」的那一批也搬走
                for _ in self.pump() {}
                return self.materializer.pending() == 0;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(2));
        }
    }
}

/// worker 运行期：解析 note_id 后从固定 seq 物化。
fn run_materialize(
    vault: &Vault,
    job: &MaterializeJob,
    peer_id: u64,
) -> Result<MaterializeOutcome, StorageError> {
    let note_id = lookup_note_id(vault, &job.doc_id)?.ok_or_else(|| StorageError::Corrupt {
        context: format!("找不到 body_doc_id={} 的笔记行，暂时无法物化", job.doc_id),
    })?;
    materialize_body(vault, &note_id, &job.doc_id, job.target_seq, peer_id)
}

/// `body_doc_id` → `note_id`（B 物化需要笔记行）。
fn lookup_note_id(vault: &Vault, doc_id: &str) -> Result<Option<String>, StorageError> {
    let rows = vault.query(
        "SELECT note_id FROM notes WHERE body_doc_id = ?1 ORDER BY note_id LIMIT 1",
        &[Value::Text(doc_id.to_owned())],
    )?;
    match rows.first().and_then(|row| row.first()) {
        Some(Value::Text(note_id)) => Ok(Some(note_id.clone())),
        Some(Value::Null) | None => Ok(None),
        other => Err(StorageError::Corrupt {
            context: format!("notes.note_id 读回异常：{other:?}"),
        }),
    }
}

`````


## `crates/storage/tests/session_flush.rs`

`````rust
//! 后台 flush 与 durable 契约：异步票据、ACK 顺序、崩溃后重放。

mod support;

use std::time::{Duration, Instant};

use menghunote_storage::session::{
    DocumentSession, DriverOptions, EditOutcome, EditRequest, ExternalCommitTicket, FlushReason,
    GateError, GateState, LiveRev, ManualClock, ScalarEdit, SessionDriver, SessionEpoch,
    SessionEvent, SessionOptions,
};
use menghunote_storage::{
    append_updates, intent_message_id, intent_object_id, load_body_doc, load_body_state,
    materialize_body, outbox_row, BodyDoc, SyncContext, SyncOutboxState, UpdateRecord, Value,
    Vault,
};
use support::*;

fn options(peer: u64) -> SessionOptions {
    SessionOptions {
        peer_id: peer,
        session_epoch: SessionEpoch(3),
        ..SessionOptions::default()
    }
}

/// 测试用同步上下文：peer 故意不用常量 1，epoch = 1（Phase 1）。
///
/// 与 `sync_intent.rs` 的 `CTX` 同口径：本地 intent 只依赖
/// `SessionOptions { peer_id, sync }`，当前代码路径**不需要**额外的
/// epoch/log/relay meta（`local_sync_intent` 是纯函数，`key_epoch` 来自这里）。
const SYNC_CTX: SyncContext = SyncContext {
    peer_id: 0xA1CE_0001,
    key_epoch: 1,
};

/// 打开「会产生本地出站意图」的会话配置。
fn options_with_sync() -> SessionOptions {
    SessionOptions {
        sync: Some(SYNC_CTX),
        ..options(SYNC_CTX.peer_id)
    }
}

fn edit(start: usize, end: usize, replacement: &str) -> ScalarEdit {
    ScalarEdit {
        start: menghunote_text::UnicodeScalar::new(start),
        end: menghunote_text::UnicodeScalar::new(end),
        replacement: replacement.to_owned(),
    }
}

fn request(
    session: &DocumentSession,
    command: &str,
    base: u64,
    edits: Vec<ScalarEdit>,
) -> EditRequest {
    EditRequest {
        doc_id: session.doc_id().to_owned(),
        generation: session.generation(),
        session_epoch: session.session_epoch(),
        command_id: command.to_owned(),
        base_live_rev: LiveRev(base),
        edits,
        origin: "local".to_owned(),
        undo_group: None,
    }
}

fn open_session(vault: &Vault, options: SessionOptions) -> DocumentSession {
    DocumentSession::open(vault, "doc-1", options).expect("打开会话")
}

#[test]
fn flush_returns_ticket_without_waiting_for_sqlite() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let options = SessionOptions {
        flush_delay: Some(Duration::from_millis(300)),
        ..options(PEER_A)
    };
    let mut session = open_session(&vault, options);
    session.apply(request(&session, "c1", 0, vec![edit(0, 0, "甲")]));

    let started = Instant::now();
    let ticket = session.flush(FlushReason::Manual);
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_millis(150),
        "flush 必须在调用线程立即返回票据（实际 {elapsed:?}）"
    );
    assert_eq!(ticket.target_live_rev, LiveRev(1));
    assert!(
        session.events().is_empty(),
        "票据返回时后台还没写完（延迟注入生效）"
    );

    // ACK 只会异步到达
    let ack = session
        .wait_for_durable(ticket.target_live_rev, Duration::from_secs(5))
        .expect("应收到 DurableAck");
    assert_eq!(ack.through_live_rev, LiveRev(1));
    assert_eq!(ack.head_seq, 1);
    assert!(!ack.head_frontier.is_empty(), "ACK 要带 durable frontier");
    assert_eq!(session.durable_head_seq(), 1);
    assert_eq!(session.durable_through(), LiveRev(1));
}

#[test]
fn durable_acks_are_monotonic_and_cover_each_target() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let mut session = open_session(&vault, options(PEER_A));

    let mut acks = Vec::new();
    for (index, ch) in ["一", "二", "三"].iter().enumerate() {
        let command = format!("c{}", index + 1);
        let expected_rev = LiveRev(index as u64 + 1);
        let outcome = session.apply(request(
            &session,
            &command,
            index as u64,
            vec![edit(index, index, ch)],
        ));
        assert!(matches!(outcome, EditOutcome::Applied(_)));
        let ticket = session.flush(FlushReason::Interval);
        assert_eq!(ticket.target_live_rev, expected_rev);
        let ack = session
            .wait_for_durable(ticket.target_live_rev, Duration::from_secs(5))
            .expect("应收到 DurableAck");
        assert_eq!(ack.through_live_rev, expected_rev);
        assert_eq!(ack.head_seq, index as u64 + 1, "head_seq 逐批递增");
        acks.push(ack);
    }

    // ACK 的批次号与水位严格递增
    for pair in acks.windows(2) {
        assert!(pair[1].batch_id > pair[0].batch_id, "batch 必须递增");
        assert!(
            pair[1].through_live_rev > pair[0].through_live_rev,
            "through_live_rev 必须递增"
        );
    }
    assert_eq!(session.markdown(), "一二三");
    assert_eq!(session.pending_len(), 0);
}

#[test]
fn crash_after_a_before_b_recovers_on_reopen() {
    let (dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let mut session = open_session(&vault, options(PEER_A));
    session.apply(request(
        &session,
        "c1",
        0,
        vec![edit(0, 0, "A中😀e\u{301}")],
    ));
    let ticket = session.flush(FlushReason::Manual);
    session
        .wait_for_durable(ticket.target_live_rev, Duration::from_secs(5))
        .expect("durable");

    // A 已提交、B 未完成：此时崩溃（直接丢会话与库句柄）
    assert!(load_body_state(&vault, "note-1")
        .expect("状态")
        .body_md
        .is_none());
    drop(session);
    drop(vault);

    // 重开：正文从 loro_updates 重放出来，durable 水位还在
    let vault = open_vault(&vault_path(&dir));
    let session = open_session(&vault, options(PEER_A));
    let snapshot = session.snapshot();
    assert_eq!(snapshot.markdown, "A中😀e\u{301}");
    assert_eq!(snapshot.durable_head_seq, 1);
    assert_eq!(snapshot.live_rev, LiveRev(0), "live_rev 是新会话的基准");

    // B 物化补齐
    materialize_body(&vault, "note-1", "doc-1", 1, PEER_A).expect("B 物化");
    let state = load_body_state(&vault, "note-1").expect("状态");
    assert_eq!(state.body_md.as_deref(), Some("A中😀e\u{301}"));
    assert_eq!(state.body_seq, 1);
}

#[test]
fn unflushed_edits_are_lost_on_reopen_but_durable_prefix_survives() {
    let (dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let mut session = open_session(&vault, options(PEER_A));

    session.apply(request(&session, "c1", 0, vec![edit(0, 0, "甲")]));
    session.apply(request(&session, "c2", 1, vec![edit(1, 1, "乙")]));
    let ticket = session.flush(FlushReason::Manual);
    session
        .wait_for_durable(ticket.target_live_rev, Duration::from_secs(5))
        .expect("durable");

    // 这两条没 flush：只存在于内存与 Loro 的未 ACK 状态
    session.apply(request(&session, "c3", 2, vec![edit(2, 2, "丙")]));
    session.apply(request(&session, "c4", 3, vec![edit(3, 3, "丁")]));
    assert_eq!(session.markdown(), "甲乙丙丁");
    drop(session);
    drop(vault);

    // 重开：只保留已 ACK 的连续前缀（甲乙），未 ACK 的丙丁丢掉——「只丢连续前缀之后」
    let vault = open_vault(&vault_path(&dir));
    let session = open_session(&vault, options(PEER_A));
    assert_eq!(session.markdown(), "甲乙");
    // 两条 update 各自占一个 seq
    assert_eq!(session.snapshot().durable_head_seq, 2);
    assert_eq!(
        count(
            &vault,
            "SELECT count(*) FROM loro_updates WHERE doc_id = 'doc-1'",
            &[]
        ),
        2,
        "只有已 ACK 的两条 update 落库"
    );
}

#[test]
fn flush_without_pending_edits_is_harmless() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let mut session = open_session(&vault, options(PEER_A));

    let ticket = session.flush(FlushReason::Manual);
    assert_eq!(ticket.target_live_rev, LiveRev(0));
    assert!(session
        .wait_for_durable(ticket.target_live_rev, Duration::from_secs(5))
        .is_some());
    assert_eq!(session.durable_head_seq(), 0);
    assert_eq!(session.markdown(), "");
}

#[test]
fn remote_event_is_pushed_to_the_queue() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let mut session = open_session(&vault, options(PEER_A));

    let remote = BodyDoc::create("doc-1", PEER_B).expect("远端副本");
    remote.insert_scalars(0, "远端").expect("远端编辑");
    let update = remote.export_all_updates().expect("导出");
    session
        .apply_remote(&update)
        .expect("导入")
        .expect("有变更");

    let events = session.events();
    let remote_events = events
        .iter()
        .filter(|event| matches!(event, SessionEvent::Remote(_)))
        .count();
    assert_eq!(remote_events, 1, "远端变更要进事件队列");
    assert!(events
        .iter()
        .any(|event| matches!(event, SessionEvent::Remote(delta) if delta.to_rev == LiveRev(1))));
    assert!(session.events().is_empty(), "事件队列是取走语义");
}

/// 把闸门驱到明确的失败态（时间只作为上限，不用 sleep 当收敛判据）。
fn drive_gate_to_failure(
    session: &mut DocumentSession,
    ticket: &ExternalCommitTicket,
) -> GateError {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match session.poll_external_commit(ticket) {
            GateState::Failed(error) => return error,
            GateState::Flushing | GateState::WaitingIME => std::thread::yield_now(),
            other => panic!("闸门没有给出明确失败：{other:?}"),
        }
        assert!(Instant::now() < deadline, "闸门 10s 内没有给出明确失败");
    }
}

/// 读 flusher 失败原因（闸门是 UI/驱动读取 `last_write_error` 的既有入口）。
fn flush_failure_message(session: &mut DocumentSession, ticket: &ExternalCommitTicket) -> String {
    match drive_gate_to_failure(session, ticket) {
        GateError::FlushFailed { message, .. } => message,
        other => panic!("期望 GateError::FlushFailed，实际 {other:?}"),
    }
}

/// 有界等待 flusher 记录失败原因。
///
/// 普通写失败没有 `DurableAck` 这个收敛点：`last_write_error` 由 flusher 在同一条
/// 命令内写完后就不再变化，因此轮询它是有界的显式状态，不是用 sleep 猜收敛。
fn wait_for_write_error(session: &DocumentSession, timeout: Duration) -> String {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(message) = session.last_write_error() {
            return message;
        }
        assert!(
            Instant::now() < deadline,
            "flush 在 {timeout:?} 内没有记录写失败原因"
        );
        std::thread::yield_now();
    }
}

/// `sync_outbox` 行数（null 的 intent 不落行，所以也可以当“有没有产生意图”）。
fn outbox_count(vault: &Vault) -> i64 {
    count(vault, "SELECT count(*) FROM sync_outbox", &[])
}

/// 测试用 SQLite 触发器：安装后由 `Drop` 兜底删除（panic 安全，正常路径显式 `remove`）。
struct SqlTrigger {
    vault: Vault,
    name: &'static str,
}

impl SqlTrigger {
    fn install(vault: &Vault, name: &'static str, sql: &str) -> Self {
        vault.exec_batch(sql).expect("安装故障注入触发器");
        Self {
            vault: vault.clone(),
            name,
        }
    }

    /// 正常路径显式删除；`Drop` 的 `IF EXISTS` 只是 panic 安全网。
    fn remove(&self) {
        self.vault
            .exec_batch(&format!("DROP TRIGGER IF EXISTS {}", self.name))
            .expect("删除故障注入触发器");
    }
}

impl Drop for SqlTrigger {
    fn drop(&mut self) {
        let _ = self
            .vault
            .exec_batch(&format!("DROP TRIGGER IF EXISTS {}", self.name));
    }
}

/// P1-3 生产接线：head 被外部推进后，flusher 必须在同一个后台线程里
/// `load_head` + 重建 sink，并重试**同一前缀**一次；成功后正常裁剪/推进 durable + ACK。
///
/// 外部推进用「同一份远端 update 已经先落库、会话又把它导入了 live」构造：新 head 的
/// ops 全部落在未 ACK 前缀的 `end_vv` 内，因此原样重试不会写坏 `head_vv`/`frontier`
/// （下面的完整重放会验证这一点）。
#[test]
fn flush_recovers_from_head_conflict_by_rebuilding_sink() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let mut session = open_session(&vault, options(PEER_A));

    // 会话先把远端 update 导入 live（journal 里留下同一批 op 的未 ACK 增量）。
    let remote = BodyDoc::create("doc-1", PEER_B).expect("远端副本");
    remote.insert_scalars(0, "远端").expect("远端编辑");
    let remote_bytes = remote.export_all_updates().expect("导出远端 update");
    let remote_update = pending(&remote, &loro::VersionVector::default()).expect("远端增量");
    session
        .apply_remote(&remote_bytes)
        .expect("导入远端 update")
        .expect("必须有变更");

    // 外部 writer（同步引擎 integrate / 另一个 Vault）把同一份 update 先落库：head 0→1。
    let advanced = append_updates(
        &vault,
        "doc-1",
        0,
        &[UpdateRecord::new("external-1", remote_update)],
    )
    .expect("外部推进 head");
    assert_eq!(advanced.head_seq, 1);
    assert_eq!(
        session.durable_head_seq(),
        0,
        "会话的 CAS 基准仍停在打开时的 head 0"
    );

    // 修复前：这里每次都拿旧 head 0 撞 CAS —— 不写库、不裁剪、不 ACK，下一次还是旧 head。
    let ticket = session.flush(FlushReason::Manual);
    let ack = session
        .wait_for_durable(ticket.target_live_rev, Duration::from_secs(5))
        .expect("HeadConflict 后必须自动重建 sink 并发出 DurableAck");
    assert_eq!(ack.through_live_rev, LiveRev(1));
    assert_eq!(
        ack.head_seq, 2,
        "重试写在外部那一条之后（同一前缀、新 CAS 基准）"
    );
    assert_eq!(session.durable_head_seq(), 2);
    assert_eq!(session.durable_through(), LiveRev(1));
    assert_eq!(session.pending_len(), 0, "成功后必须裁剪前缀");
    assert_eq!(session.markdown(), "远端");

    // 正文/head 正确：完整重放必须通过 `loro_docs.head_frontier` 的完整性校验。
    let (replayed, head) = load_body_doc(&vault, "doc-1", PEER_A, None).expect("重放正文");
    assert_eq!(replayed.text(), "远端");
    assert_eq!(head.head_seq, 2);
    let head_vv = head.version_vector().expect("解码 head vv");
    let live_vv = loro::VersionVector::decode(&session.snapshot().live_vv).expect("解码 live vv");
    assert_eq!(
        head_vv, live_vv,
        "durable head 的 ops 必须与本会话 live 一致"
    );
    assert_eq!(
        count(
            &vault,
            "SELECT count(*) FROM loro_updates WHERE doc_id = 'doc-1'",
            &[]
        ),
        2,
        "外部一条 + 自动重建后重试的一条"
    );
}

/// P1-3 反向边界：外部 head 含本会话 live **未知**的并发 ops 时，原样重试会写坏
/// `head_vv`/`frontier`（重放完整性校验直接 Corrupt），因此自动重建必须拒绝并进入
/// 不可恢复态：`last_write_error` 明确提示「需要重开文档」、journal 原样保留、
/// 后续 flush 立即失败且不再写库。
#[test]
fn flush_reports_unrecoverable_after_bounded_rebuild() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let mut session = open_session(&vault, options(PEER_A));

    session.apply(request(&session, "c1", 0, vec![edit(0, 0, "本地")]));

    // 外部 writer 写入本会话完全不知道的并发 ops：head 0→1。
    let external = BodyDoc::create("doc-1", PEER_B).expect("外部副本");
    external.insert_scalars(0, "远端").expect("外部编辑");
    let external_update = pending(&external, &loro::VersionVector::default()).expect("外部增量");
    append_updates(
        &vault,
        "doc-1",
        0,
        &[UpdateRecord::new("external-1", external_update)],
    )
    .expect("外部推进 head");

    // 闸门必须收敛到 Failed 并带出可执行的原因；修复前只会带出裸 HeadConflict，
    // 且同一个失败会永久重复（journal 不裁剪、durable 水位不前进）。
    let ticket = session.begin_external_commit().expect("begin");
    let message = flush_failure_message(&mut session, &ticket);
    assert!(
        message.contains("需要重开文档"),
        "失败原因必须明确提示重开：{message}"
    );
    assert!(
        message.contains("未知的并发 ops"),
        "必须说明为什么不能安全自动重建：{message}"
    );

    assert_eq!(session.pending_len(), 1, "不可恢复时 journal 必须原样保留");
    let rows = || {
        count(
            &vault,
            "SELECT count(*) FROM loro_updates WHERE doc_id = 'doc-1'",
            &[],
        )
    };
    assert_eq!(rows(), 1, "只有外部那一行落库（旧前缀不得被硬写）");
    assert_eq!(head(&vault, "doc-1").head_seq, 1, "库中 head 不得被改写");

    // 后续 flush 立即失败：不发 ACK、不再写库（不能无限重试/无限写）。
    for _ in 0..3 {
        let attempt = session.flush(FlushReason::Manual);
        assert!(
            session
                .wait_for_durable(attempt.target_live_rev, Duration::from_millis(200))
                .is_none(),
            "不可恢复后不得再发出 DurableAck"
        );
        assert_eq!(rows(), 1, "不可恢复后不得再写 loro_updates");
        assert_eq!(head(&vault, "doc-1").head_seq, 1, "不可恢复后不得推进 head");
    }

    // 同一个错误仍可被闸门读到（不是一次性的诊断字符串）。
    match session.poll_external_commit(&ticket) {
        GateState::Failed(GateError::FlushFailed { message: again, .. }) => {
            assert_eq!(again, message, "不可恢复状态必须稳定可读");
        }
        other => panic!("期望持续的 Failed，实际 {other:?}"),
    }
    assert_eq!(
        session.markdown(),
        "本地",
        "live 保持会话内容，靠关闭并重开文档与库对账"
    );

    // P1：不可恢复原因必须能被会话**直接**读到（UI/驱动不必先走外部提交闸门），
    // 且与闸门看到的是同一个字符串；反复读取与再次 flush 都不得改变它。
    let direct = session
        .last_write_error()
        .expect("不可恢复失败必须能从会话读到");
    assert_eq!(direct, message, "会话与闸门必须看到同一个失败原因");
    assert!(
        direct.contains("需要重开文档"),
        "会话读到的原因必须可执行：{direct}"
    );
    for _ in 0..3 {
        session.flush(FlushReason::Manual);
        assert_eq!(
            session.last_write_error().as_deref(),
            Some(message.as_str()),
            "多次读取/再次 flush 不得改变不可恢复原因"
        );
    }
    assert_eq!(session.pending_len(), 1, "读取失败原因不得动 journal");
}

/// P1-3 的另一个不可恢复分支：自动重建要读新 head，读不到/解码失败同样必须 fail-closed
/// （不尝试写、不无限重试），并明确提示需要重开文档。
/// 永久性 SQLite 错误（约束/ReadOnly/NotFound 等）也不能整类当成 Retryable；
/// 只有 BUSY/LOCKED 才值得下一次 flush 再试。
#[test]
fn flush_treats_permanent_sqlite_error_as_unrecoverable() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let mut session = open_session(&vault, options(PEER_A));
    session.apply(request(&session, "c1", 0, vec![edit(0, 0, "本地")]));

    vault
        .exec(
            "CREATE TRIGGER fail_updates BEFORE INSERT ON loro_updates \
             BEGIN SELECT RAISE(FAIL, 'permanent sqlite failure'); END",
            &[],
        )
        .expect("创建永久失败触发器");

    let ticket = session.flush(FlushReason::Manual);
    assert!(
        session
            .wait_for_durable(ticket.target_live_rev, Duration::from_secs(1))
            .is_none(),
        "永久 SQLite 错误不得发 DurableAck"
    );
    let message = session.last_write_error().expect("必须有可读失败原因");
    assert!(
        message.contains("永久性") || message.contains("需要重开文档"),
        "永久 SQLite 错误必须 fail-closed：{message}"
    );
    assert_eq!(session.pending_len(), 1, "不可恢复时 journal 必须保留");
    assert_eq!(head(&vault, "doc-1").head_seq, 0, "整笔事务必须回滚");

    vault
        .exec("DROP TRIGGER fail_updates", &[])
        .expect("移除触发器");
    // 不可恢复后不会自动重试；重开会话才能继续。
    let mut reopened = open_session(&vault, options(PEER_A));
    reopened.apply(request(&reopened, "c2", 0, vec![edit(0, 0, "重开")]));
    let ticket = reopened.flush(FlushReason::Manual);
    assert!(
        reopened
            .wait_for_durable(ticket.target_live_rev, Duration::from_secs(5))
            .is_some(),
        "重开会话后应恢复"
    );
}

/// 永久性 Corrupt 不能被当成“下次再试”的 Retryable：它不会自愈。
/// 用 `head_seq = i64::MAX` 构造 `seq = head + 1` 的 `i64_of` 溢出（append_update 返回
/// Corrupt），断言 flusher 直接进入不可恢复，而不是每次重试同一永久错误。
#[test]
fn flush_treats_permanent_corrupt_as_unrecoverable_not_retryable() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    vault
        .exec(
            "UPDATE loro_docs SET head_seq = ?1 WHERE doc_id = 'doc-1'",
            &[Value::Integer(i64::MAX)],
        )
        .expect("把 head_seq 推到 i64::MAX");
    let mut session = open_session(&vault, options(PEER_A));
    session.apply(request(&session, "c1", 0, vec![edit(0, 0, "本地")]));

    let ticket = session.flush(FlushReason::Manual);
    assert!(
        session
            .wait_for_durable(ticket.target_live_rev, Duration::from_secs(1))
            .is_none(),
        "永久 Corrupt 不得发 DurableAck"
    );
    let message = session.last_write_error().expect("必须有可读失败原因");
    assert!(
        message.contains("永久性 Corrupt") || message.contains("需要重开文档"),
        "必须明确永久失败/重开：{message}"
    );
    assert_eq!(session.pending_len(), 1, "不可恢复时 journal 必须保留");
    // 后续 flush 不再写库/不再发 ACK。
    let again = session.flush(FlushReason::Manual);
    assert!(session
        .wait_for_durable(again.target_live_rev, Duration::from_millis(200))
        .is_none());
}

#[test]
fn flush_reports_unrecoverable_when_new_head_is_unreadable() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let mut session = open_session(&vault, options(PEER_A));
    session.apply(request(&session, "c1", 0, vec![edit(0, 0, "本地")]));

    // 外部推进 head 后把 head_vv 写坏：CAS 冲突后能读到行，但 VV 解码必然失败。
    let external = BodyDoc::create("doc-1", PEER_B).expect("外部副本");
    external.insert_scalars(0, "远端").expect("外部编辑");
    let external_update = pending(&external, &loro::VersionVector::default()).expect("外部增量");
    append_updates(
        &vault,
        "doc-1",
        0,
        &[UpdateRecord::new("external-1", external_update)],
    )
    .expect("外部推进 head");
    vault
        .exec(
            "UPDATE loro_docs SET head_vv = x'01' WHERE doc_id = ?1",
            &[Value::Text("doc-1".to_owned())],
        )
        .expect("写坏 head_vv");

    let ticket = session.begin_external_commit().expect("begin");
    let message = flush_failure_message(&mut session, &ticket);
    assert!(
        message.contains("需要重开文档"),
        "失败原因必须明确提示重开：{message}"
    );
    assert!(
        message.contains("解码失败"),
        "必须说明新 head 读不出来：{message}"
    );
    assert_eq!(session.pending_len(), 1, "不可恢复时 journal 必须原样保留");
    assert_eq!(
        count(
            &vault,
            "SELECT count(*) FROM loro_updates WHERE doc_id = 'doc-1'",
            &[]
        ),
        1,
        "拒绝重建后不得写旧前缀"
    );
}

/// P2 门禁（Phase 1B 前）：HeadConflict 自动重建 sink 时，必须把同一个 `SyncContext`
/// 带进新 sink；本地 intent 的 `seq` 必须按**新** CAS 基准派生（新 head + 1），不能沿用
/// 旧 head 算出的 seq，也不能重复/留下孤儿 intent。
///
/// 场景与 [`flush_recovers_from_head_conflict_by_rebuilding_sink`] 相同：会话先导入远端
/// update，外部 writer 再把同一份 update 落库推进 head。区别是 journal 前缀里还有一条
/// 本地编辑，因此 flush 会同时写本地/远端两条记录并产生一条本地 intent。
#[test]
fn flush_rebuild_keeps_sync_intent_seq_at_new_head_plus_one() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let mut session = open_session(&vault, options_with_sync());

    // 本地编辑先入 journal，远端 update 随后导入：一次 flush 的前缀 = [本地, 远端]。
    session.apply(request(&session, "c1", 0, vec![edit(0, 0, "本地")]));
    let remote = BodyDoc::create("doc-1", PEER_B).expect("远端副本");
    remote.insert_scalars(0, "远端").expect("远端编辑");
    let remote_bytes = remote.export_all_updates().expect("导出远端 update");
    let remote_update = pending(&remote, &loro::VersionVector::default()).expect("远端增量");
    session
        .apply_remote(&remote_bytes)
        .expect("导入远端 update")
        .expect("必须有变更");

    // 外部 writer（同步引擎 integrate 的等价物）先把同一份 update 落库：head 0→1。
    let advanced = append_updates(
        &vault,
        "doc-1",
        0,
        &[UpdateRecord::new("external-1", remote_update)],
    )
    .expect("外部推进 head");
    assert_eq!(advanced.head_seq, 1);
    assert_eq!(
        session.durable_head_seq(),
        0,
        "会话的 CAS 基准仍停在打开时的 head 0"
    );

    let ticket = session.flush(FlushReason::Manual);
    let ack = session
        .wait_for_durable(ticket.target_live_rev, Duration::from_secs(5))
        .expect("HeadConflict 后必须重建 sink 并发出 DurableAck");
    assert_eq!(ack.through_live_rev, LiveRev(2));
    assert_eq!(ack.head_seq, 3, "外部一条 + 重试前缀两条");
    assert_eq!(session.durable_head_seq(), 3);
    assert_eq!(session.durable_through(), LiveRev(2));
    assert_eq!(session.pending_len(), 0, "成功后必须裁剪前缀");
    assert_eq!(
        session.last_write_error(),
        None,
        "成功写入后必须清空失败原因"
    );

    // 恰一条本地 intent：远端条目 / 外部条目都不得产生 intent。
    assert_eq!(
        outbox_count(&vault),
        1,
        "一次本地编辑只应产生一条 intent（远端/外部条目必须没有）"
    );
    let expected_seq = advanced.head_seq + 1;
    let expected_message = intent_message_id("doc-1", expected_seq, SYNC_CTX);
    let row = outbox_row(&vault, &expected_message)
        .expect("读 outbox")
        .expect("重建后仍必须产生本地 intent（说明 SyncContext 被带进新 sink）");
    assert_eq!(row.doc_id, "doc-1");
    assert_eq!(
        row.seq, expected_seq,
        "intent seq 必须按新 CAS 基准派生（新 head {} + 1）",
        advanced.head_seq
    );
    assert_eq!(row.key_epoch, SYNC_CTX.key_epoch);
    assert_eq!(row.state, SyncOutboxState::Pending);
    assert_eq!(
        row.object_id,
        Some(intent_object_id(&expected_message, SYNC_CTX)),
        "object_id 必须按重建后的 message 派生"
    );
    assert!(
        row.batch_id.starts_with("session-"),
        "intent 必须沿用本地 journal 条目的 batch，实际 {:?}",
        row.batch_id
    );

    // 直接查 outbox 列：没有第二张 intent、没有指向外部行的孤儿。
    let raw = vault
        .query("SELECT doc_id, seq, key_epoch, state FROM sync_outbox", &[])
        .expect("查 sync_outbox");
    assert_eq!(raw.len(), 1, "不得重复写 intent：{raw:?}");
    assert_eq!(raw[0][0], Value::Text("doc-1".to_owned()));
    assert_eq!(
        raw[0][1],
        Value::Integer(i64::try_from(expected_seq).expect("seq")),
        "intent 必须指向重试写入的本地 update"
    );
    assert_eq!(raw[0][2], Value::Integer(1));
    assert_eq!(raw[0][3], Value::Text("pending".to_owned()));

    // intent 指向的本地行存在且唯一；总行数 = 外部 1 + 本地 1 + 远端 1。
    assert_eq!(
        count(
            &vault,
            "SELECT count(*) FROM loro_updates WHERE doc_id='doc-1' AND seq=?1",
            &[Value::Integer(i64::try_from(expected_seq).expect("seq"))],
        ),
        1,
        "intent 指向的 update 必须存在且唯一"
    );
    assert_eq!(
        count(
            &vault,
            "SELECT count(*) FROM loro_updates WHERE doc_id = 'doc-1'",
            &[],
        ),
        3,
        "外部 + 本地 + 远端各一条，重试不得重复写"
    );

    // 正文/head 重放一致：重建后的重试没有写坏 head_vv/frontier。
    let live_markdown = session.markdown();
    assert!(
        live_markdown.contains("本地") && live_markdown.contains("远端"),
        "live 正文应包含本地与远端内容：{live_markdown:?}"
    );
    let (replayed, head) = load_body_doc(&vault, "doc-1", PEER_A, None).expect("重放正文");
    assert_eq!(replayed.text(), live_markdown);
    assert_eq!(head.head_seq, 3);
    let head_vv = head.version_vector().expect("解码 head vv");
    let live_vv = loro::VersionVector::decode(&session.snapshot().live_vv).expect("解码 live vv");
    assert_eq!(
        head_vv, live_vv,
        "durable head 的 ops 必须与本会话 live 一致"
    );
}

/// P2 门禁：非 `HeadConflict` 的普通写失败（磁盘/SQLite）必须保持「同一前缀下次重试」：
/// 不 ACK、不裁剪、不推进 durable/head；`last_write_error` 可读且**不得**升级成不可恢复。
/// 移除注入后同一前缀必须能成功落库。
#[test]
fn flush_retryable_write_failure_keeps_prefix_then_recovers() {
    let (dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let mut session = open_session(&vault, options_with_sync());
    session.apply(request(&session, "c1", 0, vec![edit(0, 0, "本地")]));
    assert_eq!(session.pending_len(), 1);

    // 真实瞬时错误：外部连接持有写锁，且本 Vault 关闭 busy 等待，A 事务拿到
    // SQLITE_BUSY（不是约束/只读/损坏等永久错误）。
    vault
        .query("PRAGMA busy_timeout=0", &[])
        .expect("关闭 busy 等待");
    let blocker = rusqlite::Connection::open(vault_path(&dir)).expect("打开外部连接");
    blocker
        .execute_batch("BEGIN IMMEDIATE")
        .expect("外部连接占写锁");

    let ticket = session.flush(FlushReason::Manual);
    let message = wait_for_write_error(&session, Duration::from_secs(10));
    assert!(!message.is_empty(), "失败原因必须可读");
    assert!(
        !message.contains("需要重开文档"),
        "普通写失败不得升级成不可恢复：{message}"
    );
    assert!(
        message.contains("锁") || message.contains("locked") || message.contains("busy"),
        "失败原因必须来自 BUSY：{message}"
    );
    assert!(
        session
            .wait_for_durable(ticket.target_live_rev, Duration::ZERO)
            .is_none(),
        "普通写失败不得发出 DurableAck"
    );

    blocker.execute_batch("ROLLBACK").expect("释放外部写锁");
    drop(blocker);
    vault
        .query("PRAGMA busy_timeout=5000", &[])
        .expect("恢复 busy 等待");

    // 前缀原样保留；没有 durable/head/intent 被写。
    assert_eq!(session.pending_len(), 1, "失败后必须保留同一前缀");
    assert_eq!(session.durable_head_seq(), 0);
    assert_eq!(session.durable_through(), LiveRev(0));
    assert_eq!(
        count(
            &vault,
            "SELECT count(*) FROM loro_updates WHERE doc_id = 'doc-1'",
            &[],
        ),
        0,
        "失败事务不得写 loro_updates"
    );
    assert_eq!(head(&vault, "doc-1").head_seq, 0, "失败事务不得推进 head");
    assert_eq!(outbox_count(&vault), 0, "失败事务不得留下本地 intent");

    // 锁释放后重试：同一前缀成功，水位/正文/intent 都补齐。
    let ticket = session.flush(FlushReason::Manual);
    let ack = session
        .wait_for_durable(ticket.target_live_rev, Duration::from_secs(5))
        .expect("注入移除后同一前缀必须可重试成功");
    assert_eq!(ack.through_live_rev, LiveRev(1));
    assert_eq!(ack.head_seq, 1);
    assert_eq!(session.pending_len(), 0);
    assert_eq!(session.durable_head_seq(), 1);
    assert_eq!(
        session.last_write_error(),
        None,
        "成功写入后必须清空失败原因"
    );
    assert_eq!(session.markdown(), "本地");
    assert_eq!(
        count(
            &vault,
            "SELECT count(*) FROM loro_updates WHERE doc_id = 'doc-1'",
            &[],
        ),
        1
    );
    assert_eq!(head(&vault, "doc-1").head_seq, 1);
    assert_eq!(outbox_count(&vault), 1, "成功后必须补写本地 intent");
    let row = outbox_row(&vault, &intent_message_id("doc-1", 1, SYNC_CTX))
        .expect("读 intent")
        .expect("本地 intent");
    assert_eq!(row.seq, 1);

    let (replayed, head_state) = load_body_doc(&vault, "doc-1", PEER_A, None).expect("重放");
    assert_eq!(replayed.text(), "本地");
    assert_eq!(head_state.head_seq, 1);
}

/// `RebuildOutcome::Retryable` 门禁：head 冲突触发自动重建后，重试**同一前缀**时若遇到
/// 非 HeadConflict 写失败（磁盘/SQLite），必须保持「可重试」——不 ACK、不裁剪、不升级成
/// 不可恢复；移除注入后下一次 flush 仍能自动重建并成功。
///
/// 注入点选 `BEFORE INSERT ON loro_updates`：第一次写会在 CAS（第 0 条语句）就因 head
/// 过期失败，根本到不了 INSERT，因此触发器只命中「重建后的重试」这一次。
#[test]
fn flush_reports_unrecoverable_on_second_head_conflict() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let mut session = open_session(&vault, options_with_sync());

    session.apply(request(&session, "c1", 0, vec![edit(0, 0, "本地")]));
    let remote = BodyDoc::create("doc-1", PEER_B).expect("远端副本");
    remote.insert_scalars(0, "远端").expect("远端编辑");
    let remote_bytes = remote.export_all_updates().expect("导出远端 update");
    let remote_update = pending(&remote, &loro::VersionVector::default()).expect("远端增量");
    session
        .apply_remote(&remote_bytes)
        .expect("导入远端 update")
        .expect("必须有变更");

    // 外部先落同一份 update：head 0→1（前缀兼容，重建本身是允许的）。
    append_updates(
        &vault,
        "doc-1",
        0,
        &[UpdateRecord::new("external-1", remote_update)],
    )
    .expect("外部推进 head");

    // 只吞掉重建后重试的 CAS，不改 head：模拟「load_head 之后、重试写入之前 head 又被
    // 推进」的交错结果（第二次 CAS 未命中）。
    let trigger = SqlTrigger::install(
        &vault,
        "swallow_second_head_cas",
        "CREATE TRIGGER swallow_second_head_cas BEFORE UPDATE OF head_seq ON loro_docs \
         BEGIN SELECT RAISE(IGNORE); END;",
    );

    let ticket = session.flush(FlushReason::Manual);
    let message = wait_for_write_error(&session, Duration::from_secs(10));
    assert!(
        message.contains("自动重建后再发生 head 冲突"),
        "必须走二次冲突的不可恢复分支：{message}"
    );
    assert!(
        message.contains("需要重开文档"),
        "不可恢复原因必须明确提示重开：{message}"
    );
    assert!(
        session
            .wait_for_durable(ticket.target_live_rev, Duration::ZERO)
            .is_none(),
        "二次冲突不得发出 DurableAck"
    );
    assert_eq!(session.pending_len(), 2, "不可恢复时 journal 必须原样保留");
    assert_eq!(session.durable_head_seq(), 0);
    assert_eq!(outbox_count(&vault), 0, "失败事务不得留下 intent");
    assert_eq!(
        count(
            &vault,
            "SELECT count(*) FROM loro_updates WHERE doc_id = 'doc-1'",
            &[],
        ),
        1,
        "只有外部那条：自动重建的重试也不得写库"
    );
    assert_eq!(head(&vault, "doc-1").head_seq, 1, "库中 head 不得被改写");

    // 移除触发器后再次 flush：不可恢复态必须短路（若真的碰库，这次就会成功）。
    trigger.remove();
    let attempt = session.flush(FlushReason::Manual);
    assert!(
        session
            .wait_for_durable(attempt.target_live_rev, Duration::ZERO)
            .is_none(),
        "不可恢复后不得再发出 DurableAck"
    );
    assert_eq!(
        count(
            &vault,
            "SELECT count(*) FROM loro_updates WHERE doc_id = 'doc-1'",
            &[],
        ),
        1,
        "不可恢复后不得再写 loro_updates"
    );
    assert_eq!(head(&vault, "doc-1").head_seq, 1, "不可恢复后不得推进 head");
    assert_eq!(
        session.last_write_error().as_deref(),
        Some(message.as_str()),
        "失败原因必须稳定可读"
    );

    // 消息里的救济方式（关闭并重开）必须真的可用：重开后从 durable 前缀加载，
    // 未 durable 的本地改动不会自动落库（这是文档化语义，不是数据静默丢失）。
    drop(session);
    let mut reopened = open_session(&vault, options_with_sync());
    assert_eq!(reopened.durable_head_seq(), 1);
    assert_eq!(reopened.markdown(), "远端", "重开后只保留 durable 前缀");
    reopened.apply(request(&reopened, "c2", 0, vec![edit(0, 0, "新")]));
    let ticket = reopened.flush(FlushReason::Manual);
    let ack = reopened
        .wait_for_durable(ticket.target_live_rev, Duration::from_secs(5))
        .expect("重开后必须能继续落库");
    assert_eq!(ack.head_seq, 2);
    assert_eq!(reopened.pending_len(), 0);
    assert!(
        reopened.markdown().contains('新') && reopened.markdown().contains("远端"),
        "重开后的正文必须同时包含 durable 前缀与新编辑：{:?}",
        reopened.markdown()
    );
}

/// 打开一个不启 ticker 的驱动（假时钟）：失败路径的收敛只靠显式 `poll`，不靠真睡。
fn open_test_driver(vault: &Vault) -> SessionDriver {
    SessionDriver::open_with_clock(
        vault,
        "doc-1",
        DriverOptions {
            session: options(PEER_A),
            auto_tick: false,
            ..DriverOptions::default()
        },
        Box::new(ManualClock::new()),
    )
    .expect("打开驱动")
}

/// 把驱动推进到「不可恢复 flush 失败」，返回闸门读到的失败原因。
///
/// 构造方式与 [`flush_reports_unrecoverable_after_bounded_rebuild`] 相同：外部 writer 写入
/// 本会话 live 未知的并发 ops，自动重建必须拒绝。
fn drive_driver_to_unrecoverable(driver: &mut SessionDriver, vault: &Vault) -> String {
    driver.apply(request(driver.session(), "c1", 0, vec![edit(0, 0, "本地")]));

    let external = BodyDoc::create("doc-1", PEER_B).expect("外部副本");
    external.insert_scalars(0, "远端").expect("外部编辑");
    let external_update = pending(&external, &loro::VersionVector::default()).expect("外部增量");
    append_updates(
        vault,
        "doc-1",
        0,
        &[UpdateRecord::new("external-1", external_update)],
    )
    .expect("外部推进 head");

    let ticket = driver.begin_external_commit().expect("begin");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match driver.poll_external_commit(&ticket) {
            GateState::Failed(GateError::FlushFailed { message, .. }) => return message,
            GateState::Flushing | GateState::WaitingIME => std::thread::yield_now(),
            other => panic!("闸门没有给出明确失败：{other:?}"),
        }
        assert!(Instant::now() < deadline, "闸门 10s 内没有给出明确失败");
    }
}

/// P1：不可恢复的 flush 失败必须能通过 `SessionDriver::last_write_error` 直接读到
/// （UI 不必先走外部提交闸门），并且没有失败时是 `None`。
#[test]
fn driver_forwards_unrecoverable_flush_error() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let mut driver = open_test_driver(&vault);
    assert_eq!(
        driver.last_write_error(),
        None,
        "没有失败时驱动必须返回 None"
    );

    let message = drive_driver_to_unrecoverable(&mut driver, &vault);
    let forwarded = driver
        .last_write_error()
        .expect("驱动必须转发不可恢复的失败原因");
    assert_eq!(forwarded, message, "驱动与闸门必须看到同一个原因");
    assert!(
        forwarded.contains("需要重开文档"),
        "驱动读到的原因必须可执行：{forwarded}"
    );
    assert_eq!(
        driver.last_write_error().as_deref(),
        Some(forwarded.as_str()),
        "多次读取必须稳定"
    );
}

/// P1：`CloseReport.flush_error` 必须带出没排空的原因；报告拿到之后驱动仍可读同一个值。
#[test]
fn close_report_surfaces_unrecoverable_flush_error() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let mut driver = open_test_driver(&vault);
    let message = drive_driver_to_unrecoverable(&mut driver, &vault);

    // 不可恢复时 `close` 必然排不空：即使 timeout 5s 也必须立即返回（不挂住 UI）。
    let started = Instant::now();
    let report = driver.close(Duration::from_secs(5));
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "不可恢复时 close 必须短路，不能等满 timeout：{:?}",
        started.elapsed()
    );
    let close_error = report
        .flush_error
        .clone()
        .expect("CloseReport 必须带出 flush 失败原因");
    assert_eq!(close_error, message, "关闭报告与闸门必须一致");
    assert!(
        close_error.contains("需要重开文档"),
        "CloseReport 的原因必须可执行：{close_error}"
    );
    assert!(report.timed_out, "不可恢复时关闭必然超时：{report:?}");
    assert!(
        report.needs_reopen(),
        "不可恢复报告必须被 UI 识别为需要重开"
    );
    assert_eq!(report.pending_edits, 1, "未 ACK 条目必须原样保留");
    assert!(
        report.durable.is_none(),
        "不可恢复后不得再发出 DurableAck：{report:?}"
    );
    // 报告拿走后驱动仍在：同一个原因必须还能读到。
    assert_eq!(
        driver.last_write_error(),
        Some(close_error),
        "关闭报告产生后原因仍须可读"
    );
}

`````


## `crates/storage/tests/body_doc_detached_oplog.rs`

`````rust
//! 任务 2 P0：detached oplog 测试矩阵（只加测试，不改生产代码）。
//!
//! `BodyDoc::import_oplog_only` / `pending_update_from_oplog` 是 `pub(crate)`，生产里唯一
//! 调用方是 baseline apply 的 `existing_local_extra` 分支（见
//! `crates/storage/src/baseline_apply.rs`）。本文件不放大可见性、不改生产代码，而是驱动
//! 该分支真正落库的 `baseline_merge` 行来验证语义：
//!
//! 1. **detached 路径**：`BodyDoc::restore(基线快照)` → `detach` → `import(本地差集)` →
//!    `pending_update_from_oplog(本地 VV)`。产物就是 `loro_updates` 里
//!    `batch_id == BASELINE_MERGE_BATCH` 的那一行（payload / end_frontier / end_vv）。
//! 2. **attached 参考**：`BodyDoc::restore(基线快照) + import(本地差集)`——同一条本地
//!    增量，改成 attached 导入。
//! 3. **attached 重放**：`BodyDoc::restore(None, 库里全部 update)` 与生产读取路径
//!    `load_body_doc`。
//!
//! 每个用例都断言：detached 的 `end_frontier` 与 attached 的 frontier 逐字节相等，
//! `end_vv` 与 attached 的 VV 语义相等（`VersionVector::encode` 的字节顺序取决于
//! import 顺序，不是规范形式，见 [`canonical_vv`]）；三份 attached 结果的**正文逐字节
//! 相同**；本地与基线内容不丢、不重复。
//!
//! LoroTree 用例：`BodyDoc` 只暴露正文 text，`StructureDoc` 不暴露 detach，因此按任务
//! 约定用 `loro` 直接构造 `LoroDoc` / `LoroTree`，验证 `import_oplog_only` 依赖的同一条
//! CRDT 不变量：detach 后 import 只写 oplog；从 oplog 导出的差集 attached 重放后，
//! frontier / VV 与树的可见形状（遍历结果）都与 attached 合并完全一致。

mod support;

use loro::{ExportMode, LoroDoc, LoroTree, LoroValue, TreeID, VersionVector};
use support::*;

use menghunote_storage::{
    append_updates, apply_baseline_batch, baseline_apply_debug_counters, begin_baseline_apply,
    doc_updates_since, finish_baseline_apply, load_body_doc, reset_baseline_apply_debug,
    BaselineApplyFinish, BaselineExpectation, BodyDoc, PendingUpdate, StoredLoroUpdate,
    UpdateRecord, Vault, BASELINE_MERGE_BATCH,
};

/// 本地设备（attached 侧）。
const A_PEER: u64 = PEER_A;
/// 远端基线导出方（与本地不同的 peer）。
const B_PEER: u64 = PEER_B;
/// 第三个 peer：只出现在基线快照里、本地从未导入的远端设备（本地未知 peer）。
const C_PEER: u64 = 3;
/// 第四个 peer：本地以非因果顺序导入的另一个远端设备。
const D_PEER: u64 = 4;

// ---------------------------------------------------------------------------
// 正文（LoroText）用例的脚手架
// ---------------------------------------------------------------------------

/// 在内存里构造一篇正文（不落库），用作「远端基线」。
fn memory_doc(doc_id: &str, peer: u64, text: &str) -> BodyDoc {
    let doc = BodyDoc::create(doc_id, peer).expect("创建内存文档");
    if !text.is_empty() {
        doc.insert_scalars(0, text).expect("写远端基线正文");
    }
    doc
}

/// begin 一份「只含这一篇文档」的 baseline staging。
fn begin_apply(vault: &Vault, doc_id: &str, baseline_s: u64, cursor: &str) {
    let expected = BaselineExpectation::new(
        std::iter::once(doc_id.to_owned()),
        std::iter::empty::<[u8; 32]>(),
    );
    let checkpoint_id: [u8; 16] = [baseline_s as u8; 16];
    begin_baseline_apply(vault, &checkpoint_id, baseline_s, cursor, &expected)
        .expect("begin baseline apply");
}

/// 快照蕴含的 VV（生产 `baseline_vv` 的来源：restore snapshot 后取 vv）。
fn snapshot_vv(snapshot: &[u8]) -> VersionVector {
    BodyDoc::restore("vv-probe", B_PEER, Some(snapshot), &[])
        .expect("基线快照可恢复")
        .vv()
}

/// VV 的规范表示：`VersionVector::encode` 的字节顺序取决于 import 顺序（不是规范形式），
/// 所以按 peer 排序后比较「peer -> counter」集合；语义相等即等价。
fn canonical_vv(bytes: &[u8]) -> Vec<(u64, i32)> {
    canonical_of_vv(&VersionVector::decode(bytes).expect("VV 可解码"))
}

/// 同 [`canonical_vv`]，但输入已是解码后的 VV（断言 blob 元信息与文档 VV 用）。
fn canonical_of_vv(vv: &VersionVector) -> Vec<(u64, i32)> {
    let mut entries: Vec<(u64, i32)> = vv.iter().map(|(peer, counter)| (*peer, *counter)).collect();
    entries.sort_unstable();
    entries
}

/// 单个 peer 的计数（缺失按 0 处理，与 `VersionVector` 语义一致）。
fn counter_of(vv: &VersionVector, peer: u64) -> i32 {
    vv.iter()
        .find(|(candidate, _)| **candidate == peer)
        .map(|(_, counter)| *counter)
        .unwrap_or(0)
}

/// 已编码 VV 里的 peer 集合（升序）。
///
/// 多 peer 用例用它断言 merged oplog VV 覆盖了场景的全部 peer：`oplog_frontiers` 只是
/// DAG tips，被后续 op 依赖的 peer 会从 tips 里消失，不能当成「peer 全集」。
fn vv_peers(bytes: &[u8]) -> Vec<u64> {
    let vv = VersionVector::decode(bytes).expect("VV 可解码");
    let mut peers: Vec<u64> = vv.keys().copied().collect();
    peers.sort_unstable();
    peers.dedup();
    peers
}

/// `loro_updates` 里 baseline merge 追加的那一行。
struct DetachedMerge {
    /// payload 由 `pending_update_from_oplog` 导出，end_frontier / end_vv 取 **oplog** 版本。
    merged: StoredLoroUpdate,
    /// 本地相对基线 VV 的差集（与生产 detached 分支 import 的字节同源）。
    local_delta: Vec<u8>,
}

/// 驱动生产 `existing_local_extra`（detached）分支，并取回它落库的差集。
///
/// 常规两 peer 用例走这个入口：期望合并后的 oplog frontier（DAG tips）恰好是
/// 本地 + 基线两个 peer。多 peer 边界用例用 [`merge_detached_with_peers`] 显式给出
/// 该场景期望的 frontier peer 集合，断言不放宽。
fn merge_detached(
    vault: &Vault,
    doc_id: &str,
    snapshot: &[u8],
    local: &BodyDoc,
    baseline_s: u64,
    cursor: &str,
) -> DetachedMerge {
    merge_detached_with_peers(
        vault,
        doc_id,
        snapshot,
        local,
        baseline_s,
        cursor,
        &[A_PEER, B_PEER],
    )
}

/// 同 [`merge_detached`]，但合并后 oplog **frontier（DAG tips）** 的 peer 集合由
/// 调用方给出（升序）。
///
/// 注意 frontier 是 DAG tips，不是「peer 全集」：某一 peer 的 op 一旦被后续 op 依赖
/// （例如基线在未知 peer C 的 op 之后继续写），它就不再是 tip。真正的全量证据是
/// merged `end_vv`，由各多 peer 用例用 [`vv_peers`] 单独断言，并与
/// [`assert_detached_matches_attached`] 的 attached VV 逐项相等。
fn merge_detached_with_peers(
    vault: &Vault,
    doc_id: &str,
    snapshot: &[u8],
    local: &BodyDoc,
    baseline_s: u64,
    cursor: &str,
    expected_peers: &[u64],
) -> DetachedMerge {
    let local_delta = local
        .pending_update(&snapshot_vv(snapshot))
        .expect("导出本地差集")
        .payload;
    assert!(
        !local_delta.is_empty(),
        "本地必须在基线之外还有 op，否则生产走 attached 分支，测不到 detached"
    );
    begin_apply(vault, doc_id, baseline_s, cursor);
    apply_baseline_batch(vault, &[(doc_id.to_owned(), snapshot.to_vec())], &[])
        .expect("应用基线快照");
    let merged = doc_updates_since(vault, doc_id, 0)
        .expect("读 loro_updates")
        .into_iter()
        .rfind(|update| update.batch_id == BASELINE_MERGE_BATCH)
        .expect("detached 分支必须落一条 baseline_merge 差集");
    assert!(!merged.payload.is_empty(), "detached 导出的差集不能为空");

    // 分支守卫：detached 合并后的 oplog frontier 必须与场景一致。
    // 若生产误走 attached 分支，这里只会是基线 state 的 peer 集合。
    let frontier = loro::Frontiers::decode(&merged.end_frontier).expect("frontier 可解码");
    let mut peers: Vec<u64> = frontier.iter().map(|id| id.peer).collect();
    peers.sort_unstable();
    peers.dedup();
    assert_eq!(
        peers.as_slice(),
        expected_peers,
        "detached 分支未生效？合并后的 oplog frontier peer 集合必须与场景一致（实际 {peers:?}）"
    );

    DetachedMerge {
        merged,
        local_delta,
    }
}

/// `loro_updates` 里 `batch_id == BASELINE_MERGE_BATCH` 的全部行（按 seq 升序）。
///
/// 幂等用例用它证明「重放/重复投递不新增、不改写 detached 落库的差集行」。
fn baseline_merge_rows(vault: &Vault, doc_id: &str) -> Vec<StoredLoroUpdate> {
    doc_updates_since(vault, doc_id, 0)
        .expect("读 loro_updates")
        .into_iter()
        .filter(|update| update.batch_id == BASELINE_MERGE_BATCH)
        .collect()
}

/// 断言 detached 结果与 attached 路径完全等价，返回合并后的正文。
///
/// attached 侧有三条证据链，全部要与 detached 的 frontier 逐字节一致、VV 语义一致：
/// 1. 基线副本 attached 导入同一条本地差集；
/// 2. 库里全部 update 重新 restore（attached 重放）；
/// 3. 生产读取入口 `load_body_doc`。
fn assert_detached_matches_attached(
    vault: &Vault,
    doc_id: &str,
    snapshot: &[u8],
    parts: &DetachedMerge,
) -> String {
    // 1) attached 参考：基线副本 + attached 导入本地差集。
    let attached =
        BodyDoc::restore(doc_id, A_PEER, Some(snapshot), &[]).expect("attached 基线副本");
    attached
        .import(&parts.local_delta)
        .expect("attached 导入本地差集");
    let attached_text = attached.text();
    assert_eq!(
        canonical_vv(&attached.vv().encode()),
        canonical_vv(&parts.merged.end_vv),
        "detached oplog VV 与 attached 合并 VV 不一致"
    );
    assert_eq!(
        attached.frontiers().encode(),
        parts.merged.end_frontier,
        "detached oplog frontier 与 attached 合并 frontier 不一致"
    );

    // 2) attached 重放：库里全部 update（本地 update + detached 导出的差集）。
    let payloads: Vec<Vec<u8>> = doc_updates_since(vault, doc_id, 0)
        .expect("读全部 update")
        .into_iter()
        .map(|update| update.payload)
        .collect();
    let replayed = BodyDoc::restore(doc_id, A_PEER, None, &payloads).expect("attached 重放");
    assert_eq!(
        replayed.text(),
        attached_text,
        "attached 重放文本与 attached 合并文本不是逐字节相同"
    );
    assert_eq!(
        canonical_vv(&replayed.vv().encode()),
        canonical_vv(&parts.merged.end_vv)
    );
    assert_eq!(replayed.frontiers().encode(), parts.merged.end_frontier);

    // 3) 生产读取路径：文本、frontier、VV 必须与 attached 参考完全一致。
    let (loaded, head) = load_body_doc(vault, doc_id, A_PEER, None).expect("生产重放");
    assert_eq!(loaded.text(), attached_text, "load_body_doc 文本不一致");
    assert_eq!(
        canonical_vv(&loaded.vv().encode()),
        canonical_vv(&parts.merged.end_vv)
    );
    assert_eq!(loaded.frontiers().encode(), parts.merged.end_frontier);
    assert_eq!(
        head.head_frontier, parts.merged.end_frontier,
        "head_frontier 必须来自 detached 合并后的 oplog frontier"
    );
    assert_eq!(
        canonical_vv(&head.head_vv),
        canonical_vv(&parts.merged.end_vv),
        "head_vv 必须来自 detached 合并后的 oplog VV"
    );

    attached_text
}

// ---------------------------------------------------------------------------
// 1. 两个 peer 并发编辑同一 LoroText
// ---------------------------------------------------------------------------

#[test]
fn concurrent_two_peer_text_inserts_match_attached_byte_for_byte() {
    let (_dir, vault) = fresh_vault();
    // 本地 peer A：独立文档，与基线完全不共享历史（最纯粹的并发同位置插入）。
    let local = seed_note(&vault, "note-1", "doc-1");
    local.insert_scalars(0, "LOCAL").expect("本地编辑");
    commit_now(&vault, &local, "local-batch");

    // 远端 peer B：导出基线快照。
    let remote = memory_doc("doc-1", B_PEER, "REMOTE");
    let snapshot = remote.export_snapshot().expect("基线快照");

    let parts = merge_detached(&vault, "doc-1", &snapshot, &local, 5, "cursor-1");
    let text = assert_detached_matches_attached(&vault, "doc-1", &snapshot, &parts);

    assert_eq!(
        text.matches("LOCAL").count(),
        1,
        "本地文本必须恰好出现一次：{text:?}"
    );
    assert_eq!(
        text.matches("REMOTE").count(),
        1,
        "基线文本必须恰好出现一次：{text:?}"
    );
    assert!(
        text.is_ascii(),
        "纯 ASCII 合并后不得引入非 ASCII 字节：{text:?}"
    );
    assert_eq!(
        text.len(),
        "LOCAL".len() + "REMOTE".len(),
        "字节数必须是两段 ASCII 之和：{text:?}"
    );
    assert_eq!(text.chars().count(), 11);
}

#[test]
fn local_op_depending_on_baseline_matches_attached_after_detached_merge() {
    let (_dir, vault) = fresh_vault();
    // 远端：先导出 base，再在本地收不到的情况下追加 -r2（新基线是原来 + 这一笔）。
    let remote = memory_doc("doc-1", B_PEER, "base");
    let partial = remote.export_snapshot().expect("base 快照");
    remote.insert_scalars(4, "-r2").expect("远端第二笔");
    let full = remote.export_snapshot().expect("full 快照");

    // 本地：先收到 base，再在 base 之上编辑——本地 op 因果依赖基线 op。
    let _ = seed_note(&vault, "note-1", "doc-1");
    let local = BodyDoc::restore("doc-1", A_PEER, Some(&partial), &[]).expect("本地副本");
    local.insert_scalars(4, "-local").expect("本地并发编辑");
    commit_now(&vault, &local, "local-batch");

    let parts = merge_detached(&vault, "doc-1", &full, &local, 6, "cursor-2");
    let text = assert_detached_matches_attached(&vault, "doc-1", &full, &parts);

    assert_eq!(
        text.matches("-local").count(),
        1,
        "依赖基线的本地 op 必须恰好一次：{text:?}"
    );
    assert_eq!(
        text.matches("-r2").count(),
        1,
        "远端并发 op 必须恰好一次：{text:?}"
    );
    assert_eq!(
        text.matches("base").count(),
        1,
        "基线文本必须恰好一次：{text:?}"
    );
    assert_eq!(
        text.len(),
        "base".len() + "-local".len() + "-r2".len(),
        "不得有重复或残留字符：{text:?}"
    );
}

// ---------------------------------------------------------------------------
// 2. LoroTree create / move / delete
// ---------------------------------------------------------------------------

/// 新建一个 LoroDoc（未启用任何容器配置）。
fn tree_doc(peer: u64) -> LoroDoc {
    let doc = LoroDoc::new();
    doc.set_peer_id(peer).expect("设置 peer id");
    doc
}

/// 取树容器并启用 fractional index（与 `StructureDoc::create/restore` 一致）。
fn editable_tree(doc: &LoroDoc) -> LoroTree {
    let tree = doc.get_tree("tree");
    tree.enable_fractional_index(0);
    tree
}

fn set_title(tree: &LoroTree, node: TreeID, title: &str) {
    tree.get_meta(node)
        .expect("节点 meta 可写")
        .insert("title", title)
        .expect("写标题");
}

fn title_of(tree: &LoroTree, node: TreeID) -> String {
    let meta = tree.get_meta(node).expect("节点 meta 可读");
    match meta.get_deep_value() {
        LoroValue::Map(entries) => entries
            .get("title")
            .and_then(|value| match value {
                LoroValue::String(value) => Some(value.to_string()),
                _ => None,
            })
            .unwrap_or_default(),
        _ => String::new(),
    }
}

/// 树的可读形状：深度优先的「标题（含缩进）」，根顺序也参与比较。
fn shape(doc: &LoroDoc) -> Vec<String> {
    let tree = doc.get_tree("tree");
    let mut out = Vec::new();
    walk_shape(&tree, None, 0, &mut out);
    out
}

fn walk_shape(tree: &LoroTree, parent: Option<TreeID>, depth: usize, out: &mut Vec<String>) {
    let Some(children) = tree.children(parent) else {
        return;
    };
    for child in children {
        out.push(format!("{}{}", "  ".repeat(depth), title_of(tree, child)));
        walk_shape(tree, Some(child), depth + 1, out);
    }
}

/// 节点 ID + 父节点 ID（连同深度），用于断言两副本是同一棵树而不是「同形状」。
fn parent_ids(doc: &LoroDoc) -> Vec<String> {
    let tree = doc.get_tree("tree");
    let mut out = Vec::new();
    walk_ids(&tree, None, 0, &mut out);
    out
}

fn walk_ids(tree: &LoroTree, parent: Option<TreeID>, depth: usize, out: &mut Vec<String>) {
    let Some(children) = tree.children(parent) else {
        return;
    };
    for child in children {
        let parent_label = parent
            .map(|parent| parent.to_string())
            .unwrap_or_else(|| "<root>".to_owned());
        out.push(format!("{parent_label}|{child}|{depth}"));
        walk_ids(tree, Some(child), depth + 1, out);
    }
}

/// 在树上跑一遍 detached / attached 三条路径，返回 attached 参考文档。
///
/// - `snapshot`：本轮要应用的远端基线快照（peer B 导出）；
/// - `local_delta`：本地相对该快照 VV 的差集；
/// - `local_vv`：本地副本的 oplog VV（detached 导出的基准）；
/// - `local_snapshot`：本地副本完整快照（attached 重放的起点）。
fn assert_tree_paths_agree(
    peer: u64,
    snapshot: &[u8],
    local_delta: &[u8],
    local_vv: &VersionVector,
    local_snapshot: &[u8],
) -> LoroDoc {
    // 1) attached 参考：基线副本 attached 导入同一条本地差集。
    let attached = tree_doc(peer);
    attached.import(snapshot).expect("attached 基线快照");
    attached.import(local_delta).expect("attached 合并本地差集");

    // 2) detached：先 detach，只把增量写进 oplog，再从 oplog 导出差集。
    let detached = tree_doc(peer);
    detached.import(snapshot).expect("detached 基线快照");
    editable_tree(&detached);
    detached.detach();
    assert!(detached.is_detached());
    detached.import(local_delta).expect("detached import");
    assert!(
        detached.is_detached(),
        "import 不得把 detached 文档翻回 attached"
    );
    assert_eq!(
        detached.oplog_frontiers().encode(),
        attached.state_frontiers().encode(),
        "detached 合并 frontier 与 attached 合并 frontier 不一致"
    );
    assert_eq!(
        detached.oplog_frontiers().encode(),
        attached.oplog_frontiers().encode(),
        "detached oplog frontier 与 attached oplog frontier 不一致"
    );
    assert_eq!(
        canonical_vv(&detached.oplog_vv().encode()),
        canonical_vv(&attached.oplog_vv().encode()),
        "detached 合并 VV 与 attached 合并 VV 不一致"
    );
    let delta_from_oplog = detached
        .export(ExportMode::updates(local_vv))
        .expect("从 detached oplog 导出差集");
    assert!(!delta_from_oplog.is_empty(), "本轮 detached 差集必须非空");

    // attach 后读取合并后的树状态（生产从不读 detached 状态，这里只做等价性证据）。
    detached.attach();
    assert!(!detached.is_detached());
    assert_eq!(shape(&detached), shape(&attached), "attach 后树形状不一致");
    assert_eq!(
        parent_ids(&detached),
        parent_ids(&attached),
        "attach 后树父子关系不一致"
    );

    // 3) attached 重放：本地副本 + detached 导出的差集（= 库里本地 update + 合并差集）。
    let replay = tree_doc(peer);
    replay.import(local_snapshot).expect("重放本地副本");
    editable_tree(&replay);
    replay
        .import(&delta_from_oplog)
        .expect("重放 detached 差集");
    assert_eq!(
        replay.state_frontiers().encode(),
        attached.state_frontiers().encode(),
        "attached 重放 frontier 与 attached 合并不一致"
    );
    assert_eq!(
        canonical_vv(&replay.oplog_vv().encode()),
        canonical_vv(&attached.oplog_vv().encode()),
        "attached 重放 VV 与 attached 合并不一致"
    );
    assert_eq!(
        shape(&replay),
        shape(&attached),
        "attached 重放树形状不一致"
    );
    assert_eq!(
        parent_ids(&replay),
        parent_ids(&attached),
        "attached 重放父子关系不一致"
    );

    attached
}

#[test]
fn tree_create_and_move_match_attached_frontier_and_shape() {
    // 第一份基线（peer B）：Folder / Note 1、根级 Note 2。
    let remote = tree_doc(B_PEER);
    let remote_tree = editable_tree(&remote);
    let folder = remote_tree.create(None).expect("创建 Folder");
    let note1 = remote_tree.create(folder).expect("创建 Note 1");
    let note2 = remote_tree.create(None).expect("创建 Note 2");
    set_title(&remote_tree, folder, "Folder");
    set_title(&remote_tree, note1, "Note 1");
    set_title(&remote_tree, note2, "Note 2");
    remote.commit();
    let first = remote.export(ExportMode::Snapshot).expect("第一份基线");

    // 本地（peer A）：在基线上并发 create + move（把 Note 2 重挂到本地新节点下）。
    let local = tree_doc(A_PEER);
    local.import(&first).expect("本地副本");
    let local_tree = editable_tree(&local);
    let local_node = local_tree.create(note1).expect("本地新节点");
    set_title(&local_tree, local_node, "Local");
    local_tree.mov(note2, local_node).expect("本地移动");
    local.commit();
    let local_snapshot = local.export(ExportMode::Snapshot).expect("本地快照");
    let local_vv = local.oplog_vv();

    // 远端在本地编辑期间继续前进：新建根节点 Remote（第二轮 frontier 的来源）。
    let remote_node = remote_tree.create(None).expect("远端新根");
    set_title(&remote_tree, remote_node, "Remote");
    remote.commit();
    let snapshot = remote.export(ExportMode::Snapshot).expect("第二份基线");
    let baseline_vv = remote.oplog_vv();

    let local_delta = local
        .export(ExportMode::updates(&baseline_vv))
        .expect("本地差集");
    assert!(!local_delta.is_empty(), "本地差集不能为空");

    let attached =
        assert_tree_paths_agree(A_PEER, &snapshot, &local_delta, &local_vv, &local_snapshot);

    // 可见形状硬断言：Folder > Note 1 > Local > Note 2，远端新根 Remote 在根级末尾。
    assert_eq!(
        shape(&attached),
        vec![
            "Folder".to_owned(),
            "  Note 1".to_owned(),
            "    Local".to_owned(),
            "      Note 2".to_owned(),
            "Remote".to_owned(),
        ]
    );
}

#[test]
fn tree_delete_and_reparent_match_attached_frontier_and_shape() {
    // 第一份基线（peer B）：Folder / Note 1（本地会删），根级 Note 2、Note 3（本地会重挂）。
    let remote = tree_doc(B_PEER);
    let remote_tree = editable_tree(&remote);
    let folder = remote_tree.create(None).expect("创建 Folder");
    let note1 = remote_tree.create(folder).expect("创建 Note 1");
    let note2 = remote_tree.create(None).expect("创建 Note 2");
    let note3 = remote_tree.create(None).expect("创建 Note 3");
    set_title(&remote_tree, folder, "Folder");
    set_title(&remote_tree, note1, "Note 1");
    set_title(&remote_tree, note2, "Note 2");
    set_title(&remote_tree, note3, "Note 3");
    remote.commit();
    let first = remote.export(ExportMode::Snapshot).expect("第一份基线");

    // 本地（peer A）：并发 delete + create + reparent。
    let local = tree_doc(A_PEER);
    local.import(&first).expect("本地副本");
    let local_tree = editable_tree(&local);
    local_tree.delete(note1).expect("本地删除 Note 1");
    assert!(
        local_tree.is_node_deleted(&note1).expect("删除标记可读"),
        "本地删除后 Note 1 必须是删除态"
    );
    let local_node = local_tree.create(note2).expect("本地新节点");
    set_title(&local_tree, local_node, "Local");
    local_tree.mov(note3, local_node).expect("本地重挂 Note 3");
    local.commit();
    let local_snapshot = local.export(ExportMode::Snapshot).expect("本地快照");
    let local_vv = local.oplog_vv();

    // 远端继续：新建根节点 Remote。
    let remote_node = remote_tree.create(None).expect("远端新根");
    set_title(&remote_tree, remote_node, "Remote");
    remote.commit();
    let snapshot = remote.export(ExportMode::Snapshot).expect("第二份基线");
    let baseline_vv = remote.oplog_vv();

    let local_delta = local
        .export(ExportMode::updates(&baseline_vv))
        .expect("本地差集");
    assert!(!local_delta.is_empty(), "本地差集不能为空");

    let attached =
        assert_tree_paths_agree(A_PEER, &snapshot, &local_delta, &local_vv, &local_snapshot);

    // 可见形状硬断言：Note 1 被删（Folder 无子），Note 2 > Local > Note 3，Remote 在根级末尾。
    assert_eq!(
        shape(&attached),
        vec![
            "Folder".to_owned(),
            "Note 2".to_owned(),
            "  Local".to_owned(),
            "    Note 3".to_owned(),
            "Remote".to_owned(),
        ]
    );
    assert!(
        attached
            .get_tree("tree")
            .is_node_deleted(&note1)
            .expect("删除标记可读"),
        "合并后 Note 1 必须仍是删除态"
    );
}

// ---------------------------------------------------------------------------
// 3. 本地在 baseline 导出之后继续编辑
// ---------------------------------------------------------------------------

#[test]
fn local_edits_after_baseline_export_survive_exactly_once() {
    let (_dir, vault) = fresh_vault();
    let remote = memory_doc("doc-1", B_PEER, "Hello world");
    let first = remote.export_snapshot().expect("第一份基线");

    // 本地在 S0 之后继续写（首尾各一笔），远端也在 S0 之后写（中间一笔）。
    let _ = seed_note(&vault, "note-1", "doc-1");
    let local = BodyDoc::restore("doc-1", A_PEER, Some(&first), &[]).expect("本地副本");
    local.insert_scalars(0, ">> ").expect("本地行首编辑");
    local
        .insert_scalars(local.scalar_len(), "!")
        .expect("本地行尾编辑");
    commit_now(&vault, &local, "local-batch");

    remote.insert_scalars(5, " MID").expect("远端第二笔");
    let full = remote.export_snapshot().expect("第二份基线");

    let parts = merge_detached(&vault, "doc-1", &full, &local, 6, "cursor-2");
    let text = assert_detached_matches_attached(&vault, "doc-1", &full, &parts);

    assert_eq!(
        text, ">> Hello MID world!",
        "并发的本地首尾编辑必须保留且只保留一次：{text:?}"
    );
    assert_eq!(text.matches(">> ").count(), 1);
    assert_eq!(text.matches(" MID").count(), 1);
    assert_eq!(text.matches('!').count(), 1);
    assert_eq!(text.matches("Hello").count(), 1);
    assert_eq!(text.len(), 19);
}

#[test]
fn second_detached_merge_keeps_both_local_writes_once() {
    let (_dir, vault) = fresh_vault();
    let local = seed_note(&vault, "note-1", "doc-1");
    local.insert_scalars(0, "-L1").expect("本地第一笔");
    commit_now(&vault, &local, "local-round-1");

    let remote = memory_doc("doc-1", B_PEER, "BASE");
    let first = remote.export_snapshot().expect("第一份基线");

    let parts = merge_detached(&vault, "doc-1", &first, &local, 5, "cursor-1");
    let first_text = assert_detached_matches_attached(&vault, "doc-1", &first, &parts);
    assert_eq!(first_text.matches("-L1").count(), 1);
    assert_eq!(first_text.matches("BASE").count(), 1);

    // 第一轮 finish；完成后再 begin 第二轮（第二次 checkpoint 追赶）。
    let finish = finish_baseline_apply(&vault, 5, "cursor-1").expect("finish 第一轮");
    assert!(
        matches!(finish, BaselineApplyFinish::Applied(_)),
        "第一轮 finish 应成功：{finish:?}"
    );

    // 合并态上继续本地写：生产路径就是重放合并结果再编辑。
    let (merged, _head) = load_body_doc(&vault, "doc-1", A_PEER, None).expect("第一轮合并态");
    merged
        .insert_scalars(merged.scalar_len(), "-L2")
        .expect("本地第二笔");
    commit_now(&vault, &merged, "local-round-2");

    // 远端也继续前进：第二轮 detached 差集必须非空（否则测不到 exported 差集）。
    remote.insert_scalars(0, "R2-").expect("远端第二笔");
    let second = remote.export_snapshot().expect("第二份基线");

    let parts = merge_detached(&vault, "doc-1", &second, &merged, 9, "cursor-2");
    let text = assert_detached_matches_attached(&vault, "doc-1", &second, &parts);

    assert_eq!(
        text.matches("-L1").count(),
        1,
        "第一笔本地编辑必须恰好一次：{text:?}"
    );
    assert_eq!(
        text.matches("-L2").count(),
        1,
        "第二笔本地编辑必须恰好一次：{text:?}"
    );
    assert_eq!(text.matches("BASE").count(), 1);
    assert_eq!(text.matches("R2-").count(), 1);
    assert!(text.ends_with("-L2"), "本地追加必须仍在末尾：{text:?}");
    assert_eq!(
        text.len(),
        "BASE".len() + "R2-".len() + "-L1".len() + "-L2".len(),
        "不得有重复或残留字符：{text:?}"
    );
}

// ---------------------------------------------------------------------------
// 4. 边界：空文档 / 单字符 / emoji 与组合字符
// ---------------------------------------------------------------------------

#[test]
fn empty_baseline_text_merges_to_local_text_only() {
    let (_dir, vault) = fresh_vault();
    // 基线：插入再删除——正文为空，但 oplog 非空，走的是 detached 合并而不是「新文档」分支。
    let remote = BodyDoc::create("doc-1", B_PEER).expect("远端基线");
    remote.insert_scalars(0, "x").expect("基线插入");
    remote.delete_scalars(0, 1).expect("基线删除");
    assert_eq!(remote.text(), "");
    let snapshot = remote.export_snapshot().expect("基线快照");

    let local = seed_note(&vault, "note-1", "doc-1");
    local.insert_scalars(0, "local").expect("本地编辑");
    commit_now(&vault, &local, "local-batch");

    let parts = merge_detached(&vault, "doc-1", &snapshot, &local, 7, "cursor-3");
    let text = assert_detached_matches_attached(&vault, "doc-1", &snapshot, &parts);

    assert_eq!(text, "local", "空基线不得留下任何可见字符：{text:?}");
    assert_eq!(text.len(), 5);
}

#[test]
fn local_doc_emptied_by_delete_merges_to_baseline_text() {
    let (_dir, vault) = fresh_vault();
    let remote = memory_doc("doc-1", B_PEER, "baseline");
    let snapshot = remote.export_snapshot().expect("基线快照");

    // 本地写过再全部删掉：正文为空但 oplog 非空。
    let local = seed_note(&vault, "note-1", "doc-1");
    local.insert_scalars(0, "tmp").expect("本地临时文本");
    local.delete_scalars(0, 3).expect("本地清空");
    assert_eq!(local.text(), "");
    commit_now(&vault, &local, "local-batch");

    let parts = merge_detached(&vault, "doc-1", &snapshot, &local, 8, "cursor-4");
    let text = assert_detached_matches_attached(&vault, "doc-1", &snapshot, &parts);

    assert_eq!(text, "baseline", "本地清空不得删掉基线字符：{text:?}");
    assert_eq!(text.len(), 8);
}

#[test]
fn single_char_docs_keep_both_chars_after_detached_merge() {
    let (_dir, vault) = fresh_vault();
    let remote = memory_doc("doc-1", B_PEER, "a");
    let snapshot = remote.export_snapshot().expect("基线快照");

    let local = seed_note(&vault, "note-1", "doc-1");
    local.insert_scalars(0, "b").expect("本地单字符");
    commit_now(&vault, &local, "local-batch");

    let parts = merge_detached(&vault, "doc-1", &snapshot, &local, 9, "cursor-5");
    let text = assert_detached_matches_attached(&vault, "doc-1", &snapshot, &parts);

    assert_eq!(text.len(), 2, "两个单字符必须都在：{text:?}");
    assert_eq!(text.chars().count(), 2);
    assert_eq!(text.matches('a').count(), 1, "基线字符丢失/重复：{text:?}");
    assert_eq!(text.matches('b').count(), 1, "本地字符丢失/重复：{text:?}");
}

#[test]
fn emoji_and_combining_mark_preserved_byte_exact() {
    let (_dir, vault) = fresh_vault();
    let remote = memory_doc("doc-1", B_PEER, "😀");
    let snapshot = remote.export_snapshot().expect("基线快照");

    let local = seed_note(&vault, "note-1", "doc-1");
    local
        .insert_scalars(0, "e\u{301}")
        .expect("本地组合字符（e + U+0301）");
    commit_now(&vault, &local, "local-batch");

    let parts = merge_detached(&vault, "doc-1", &snapshot, &local, 10, "cursor-6");
    let text = assert_detached_matches_attached(&vault, "doc-1", &snapshot, &parts);

    assert_eq!(
        text.chars().count(),
        3,
        "😀 + e + ◌́ = 3 个 scalar：{text:?}"
    );
    assert_eq!(
        text.len(),
        4 + 3,
        "UTF-8 字节数必须原样保留（😀 4B + e 1B + U+0301 2B）：{text:?}"
    );
    assert_eq!(text.matches("😀").count(), 1, "emoji 丢失/重复：{text:?}");
    assert_eq!(
        text.matches("e\u{301}").count(),
        1,
        "组合字符不得被拆开：{text:?}"
    );
    assert!(
        !text.contains('\u{fffd}'),
        "不得出现替换字符（说明按字节截断过）：{text:?}"
    );
}

#[test]
fn precomposed_vs_decomposed_accent_merges_without_normalization() {
    let (_dir, vault) = fresh_vault();
    // 远端用预组合 é（U+00E9，2B），本地用分解 e + U+0301（3B）：同形不同字节。
    let remote = memory_doc("doc-1", B_PEER, "é");
    let snapshot = remote.export_snapshot().expect("基线快照");

    let local = seed_note(&vault, "note-1", "doc-1");
    local.insert_scalars(0, "e\u{301}").expect("本地分解形式");
    commit_now(&vault, &local, "local-batch");

    let parts = merge_detached(&vault, "doc-1", &snapshot, &local, 11, "cursor-7");
    let text = assert_detached_matches_attached(&vault, "doc-1", &snapshot, &parts);

    assert_eq!(text.len(), 2 + 3, "两种编码的字节都要保留：{text:?}");
    assert_eq!(text.chars().count(), 3);
    assert_eq!(text.matches('é').count(), 1, "预组合形式丢失：{text:?}");
    assert_eq!(
        text.matches("e\u{301}").count(),
        1,
        "分解形式丢失：{text:?}"
    );
    assert_ne!(text, "éé", "不得做 NFC 归一化：{text:?}");
    assert_ne!(text, "e\u{301}e\u{301}", "不得做 NFD 归一化：{text:?}");
}

// ---------------------------------------------------------------------------
// 5. 边界：基线 VV 里含本地从未导入的 peer
// ---------------------------------------------------------------------------

/// 基线快照里带一个本地从未导入的 peer C（基线 VV 有 C、本地 VV 没有）时，
/// detached 导出/合并必须被钉死为：
/// - 不 panic、不把 C 的 op 丢掉（否则本地会永久缺一段远端历史）；
/// - 本地差集只含本地 peer 的 op——C 已在基线 VV 覆盖范围内，不能重复导出；
/// - merged 差集恰好是「基线相对本地 VV 的差集」：单独重放等于基线文本（含未知 peer C 的 op），
///   而不是全量历史、也不是空集。
#[test]
fn detached_baseline_vv_with_unimported_peer_keeps_both_sides_once() {
    let (_dir, vault) = fresh_vault();

    // 本地 peer A：独立文档，从未导入基线（本地 VV 只有 A）。
    let local = seed_note(&vault, "note-1", "doc-1");
    local.insert_scalars(0, "LOCAL").expect("本地编辑");
    commit_now(&vault, &local, "local-batch");

    // 基线：peer B 先导入 peer C 的 update（本地从未见过 C），再追加自己的文本。
    let third = BodyDoc::create("doc-1", C_PEER).expect("第三台设备文档");
    third.insert_scalars(0, "C").expect("C 的文本");
    let third_update = third
        .pending_update(&VersionVector::new())
        .expect("C 的增量");
    let remote = BodyDoc::create("doc-1", B_PEER).expect("基线文档");
    remote
        .import(&third_update.payload)
        .expect("基线导入 C 的 op");
    remote
        .insert_scalars(remote.scalar_len(), "BASE")
        .expect("基线文本");
    let snapshot = remote.export_snapshot().expect("基线快照");

    let baseline_vv = snapshot_vv(&snapshot);
    let local_vv = local.vv();
    assert!(
        counter_of(&baseline_vv, C_PEER) > 0,
        "基线 VV 必须含本地从未导入的 peer C：{baseline_vv:?}"
    );
    assert_eq!(
        counter_of(&local_vv, C_PEER),
        0,
        "本地 VV 不得含 C peer（否则测不到「本地未知 peer」）"
    );
    // 生产分支守卫：本地在基线之外还有 op（A 不在基线 VV 里）。
    assert!(
        local_vv.sub_iter(&baseline_vv).next().is_some(),
        "本地必须相对基线有额外 op，否则生产走 attached 分支"
    );

    let parts = merge_detached_with_peers(
        &vault,
        "doc-1",
        &snapshot,
        &local,
        5,
        "cursor-1",
        &[A_PEER, B_PEER],
    );
    // detached 合并后的 oplog VV 必须覆盖全部三个 peer（未知 peer C 的 op 不能丢）。
    assert_eq!(
        vv_peers(&parts.merged.end_vv),
        vec![A_PEER, B_PEER, C_PEER],
        "合并后的 oplog VV 必须含未知 peer C"
    );
    let text = assert_detached_matches_attached(&vault, "doc-1", &snapshot, &parts);

    assert_eq!(
        text.len(),
        "LOCAL".len() + "CBASE".len(),
        "不得有重复或残留字符：{text:?}"
    );
    assert_eq!(
        text.matches("LOCAL").count(),
        1,
        "本地文本必须恰好一次：{text:?}"
    );
    assert_eq!(
        text.matches("CBASE").count(),
        1,
        "基线文本必须恰好一次：{text:?}"
    );

    // 本地差集只含本地 peer 的 op：单独重放就是本地文本，不含 C/B 的历史。
    let local_delta_alone = BodyDoc::restore(
        "local-delta-probe",
        A_PEER,
        None,
        std::slice::from_ref(&parts.local_delta),
    )
    .expect("本地差集可重放");
    assert_eq!(
        local_delta_alone.text(),
        "LOCAL",
        "本地差集不得包含基线 op：{local_delta_alone:?}"
    );
    assert_eq!(
        canonical_vv(&local_delta_alone.vv().encode()),
        canonical_of_vv(&local_vv),
        "本地差集的 VV 必须等于本地 VV"
    );

    // merged 差集 = 基线相对本地 VV 的差集：单独重放恰好是基线文本（C 的 op 必须在内）。
    let merged_alone = BodyDoc::restore(
        "merged-probe",
        A_PEER,
        None,
        std::slice::from_ref(&parts.merged.payload),
    )
    .expect("merged 差集可重放");
    assert_eq!(
        merged_alone.text(),
        "CBASE",
        "merged 差集必须正好覆盖基线文本：{merged_alone:?}"
    );
    assert_eq!(
        canonical_vv(&merged_alone.vv().encode()),
        canonical_of_vv(&baseline_vv),
        "merged 差集的 VV 必须等于基线 VV（未知 peer C 的 op 不能丢）"
    );
}

// ---------------------------------------------------------------------------
// 6. 边界：`from` 超前于 doc 当前 VV（病态输入）
// ---------------------------------------------------------------------------

/// `from`（基线 VV）对本地 peer 的计数严格超前于本地文档当前 VV 时（病态输入：基线
/// 声称包含本地还没拿到的 A op），按现有生产 API 的语义钉死：
/// 不 panic、不报错：`export(updates(from))` 对超前的 from 只导出**未被覆盖**的 op；
/// 本地差集 = 未被 from 覆盖的 peer 的 op（C/D），绝不把 A 的旧 op 再导出一遍；
/// 也不退化成空集（C/D 是本地真正多出来的 op）。
/// 断言用 blob 元信息（partial_start_vv / partial_end_vv）证明导出区间，不靠猜测文本顺序。
#[test]
fn detached_export_from_ahead_of_local_vv_keeps_only_uncovered_ops() {
    let (_dir, vault) = fresh_vault();

    // peer A 的两笔历史：基线包含 A 的全量（A:1..4），本地只导入前半（A:1..2）。
    let a_doc = BodyDoc::create("doc-1", A_PEER).expect("A 文档");
    a_doc.insert_scalars(0, "A1").expect("A 第一笔");
    let a_first = a_doc
        .pending_update(&VersionVector::new())
        .expect("A 第一笔增量");
    a_doc.insert_scalars(2, "A2").expect("A 第二笔");
    let a_all = a_doc
        .pending_update(&VersionVector::new())
        .expect("A 全量增量");

    // 本地：A 的前半 + 两个本地真正多出来的 peer（C/D）。
    let third = BodyDoc::create("doc-1", C_PEER).expect("C 文档");
    third.insert_scalars(0, "C").expect("C 文本");
    let third_update = third.pending_update(&VersionVector::new()).expect("C 增量");
    let fourth = BodyDoc::create("doc-1", D_PEER).expect("D 文档");
    fourth.insert_scalars(0, "D").expect("D 文本");
    let fourth_update = fourth
        .pending_update(&VersionVector::new())
        .expect("D 增量");
    let local = seed_note(&vault, "note-1", "doc-1");
    local.import(&a_first.payload).expect("本地导入 A 前半");
    local.import(&third_update.payload).expect("本地导入 C");
    local.import(&fourth_update.payload).expect("本地导入 D");
    commit_now(&vault, &local, "local-batch");

    // 基线：导入 A 的全量（对 A 的计数超前本地）再写自己的文本。
    let remote = BodyDoc::create("doc-1", B_PEER).expect("基线文档");
    remote.import(&a_all.payload).expect("基线导入 A 全量");
    remote.insert_scalars(0, "BASE").expect("基线文本");
    let snapshot = remote.export_snapshot().expect("基线快照");

    let baseline_vv = snapshot_vv(&snapshot);
    let local_vv = local.vv();
    assert!(
        counter_of(&baseline_vv, A_PEER) > counter_of(&local_vv, A_PEER),
        "基线对 A 的计数必须严格超前本地（from 超前）：baseline={baseline_vv:?} local={local_vv:?}"
    );
    assert!(
        local_vv.sub_iter(&baseline_vv).next().is_some(),
        "本地必须相对基线有额外 op（C/D），否则生产走 attached 分支"
    );

    let parts = merge_detached_with_peers(
        &vault,
        "doc-1",
        &snapshot,
        &local,
        6,
        "cursor-2",
        &[B_PEER, C_PEER, D_PEER],
    );
    // detached 合并后的 oplog VV 必须覆盖全部四个 peer（A 的基线 op 与本地 C/D 的 op 都在）。
    assert_eq!(
        vv_peers(&parts.merged.end_vv),
        vec![A_PEER, B_PEER, C_PEER, D_PEER],
        "合并后的 oplog VV 必须含全部四个 peer"
    );
    let text = assert_detached_matches_attached(&vault, "doc-1", &snapshot, &parts);

    assert_eq!(
        text.len(),
        "BASEA1A2CD".len(),
        "不得有重复或残留字符：{text:?}"
    );
    for marker in ["BASE", "A1A2", "C", "D"] {
        assert_eq!(
            text.matches(marker).count(),
            1,
            "{marker} 必须恰好一次：{text:?}"
        );
    }

    // 本地差集：blob 元信息证明只导出 C/D（A 的旧 op 被 from 覆盖，不重复导出）。
    let delta_meta =
        LoroDoc::decode_import_blob_meta(&parts.local_delta, true).expect("本地差集元信息");
    assert_eq!(
        canonical_of_vv(&delta_meta.partial_start_vv),
        vec![(C_PEER, 0), (D_PEER, 0)],
        "本地差集必须从 C/D 的 0 开始（不含 A 的 op）"
    );
    assert_eq!(
        canonical_of_vv(&delta_meta.partial_end_vv),
        vec![
            (C_PEER, counter_of(&local_vv, C_PEER)),
            (D_PEER, counter_of(&local_vv, D_PEER)),
        ],
        "本地差集终点必须是 C/D 的本地计数"
    );
    // 单独重放：只有 C/D 两个字符（依赖缺失的 op 不会凭空出现）。
    let local_delta_alone = BodyDoc::restore(
        "local-delta-probe",
        A_PEER,
        None,
        std::slice::from_ref(&parts.local_delta),
    )
    .expect("本地差集可重放");
    assert_eq!(
        local_delta_alone.text().chars().count(),
        2,
        "本地差集只应含 C/D：{local_delta_alone:?}"
    );
    assert_eq!(local_delta_alone.text().matches('C').count(), 1);
    assert_eq!(local_delta_alone.text().matches('D').count(), 1);
    assert!(
        !local_delta_alone.text().contains('A'),
        "不得把 A 的旧 op 再导出：{local_delta_alone:?}"
    );

    // merged 差集：起点 = 本地 VV（A:2、B:0），终点 = 基线 VV（A:4、B:4）。
    let merged_meta =
        LoroDoc::decode_import_blob_meta(&parts.merged.payload, true).expect("merged 元信息");
    assert_eq!(
        canonical_of_vv(&merged_meta.partial_start_vv),
        vec![(A_PEER, counter_of(&local_vv, A_PEER)), (B_PEER, 0)],
        "merged 差集必须从本地 VV 起算（超前 from 的语义）"
    );
    assert_eq!(
        canonical_of_vv(&merged_meta.partial_end_vv),
        vec![
            (A_PEER, counter_of(&baseline_vv, A_PEER)),
            (B_PEER, counter_of(&baseline_vv, B_PEER)),
        ],
        "merged 差集终点必须是基线 VV"
    );
    // 单独重放 merged 差集：A 的后续 op 缺依赖、B 的 op 依赖也未满足 → 不产生可见文本；
    // 关键是否定断言：绝不能把本地独有的 C/D 再导出一遍。
    let merged_alone = BodyDoc::restore(
        "merged-probe",
        A_PEER,
        None,
        std::slice::from_ref(&parts.merged.payload),
    )
    .expect("merged 差集可重放");
    assert_eq!(
        merged_alone.text(),
        "",
        "merged 差集单独重放不得产生文本：{merged_alone:?}"
    );
}

// ---------------------------------------------------------------------------
// 7. 边界：同一个 update 重复 import 的幂等性
// ---------------------------------------------------------------------------

/// 同一个 update 重复 import 的幂等性。两轮 apply 走**不同**的生产分支：
/// 首轮：本地有 extra → `existing_local_extra`（detached）分支，落一条 baseline_merge；
/// 次轮：同一 snapshot digest 已应用 → 生产在 detached 之前短路到
/// `replayed_same_digest` 分支，**不得**再进 detached，也不得新增 baseline_merge、
/// 推进 head_seq、改 head_vv/head_frontier 或文本。
/// 另在 Loro 层直接钉住「同一 update 导入两次不产生新 op」（`import_oplog_only` 依赖的
/// 不变量），并在数据库层钉住「重复投递同一 payload 行经 `load_body_doc` 重放不变」。
#[test]
fn detached_same_update_reimport_is_idempotent() {
    let (_dir, vault) = fresh_vault();
    let local = seed_note(&vault, "note-1", "doc-1");
    local.insert_scalars(0, "LOCAL").expect("本地编辑");
    commit_now(&vault, &local, "local-batch");

    let remote = memory_doc("doc-1", B_PEER, "BASE");
    let snapshot = remote.export_snapshot().expect("基线快照");

    // 首轮：detached 分支（重放计数清零后再调用，断言生产真的进了这个分支）。
    reset_baseline_apply_debug();
    let parts = merge_detached(&vault, "doc-1", &snapshot, &local, 5, "cursor-1");
    let first = baseline_apply_debug_counters();
    assert_eq!(
        first.existing_local_extra, 1,
        "首轮必须命中 detached 分支：{first:?}"
    );
    assert_eq!(
        first.replayed_same_digest, 0,
        "首轮不应走重放分支：{first:?}"
    );

    let head_before = head(&vault, "doc-1");
    let rows_before = baseline_merge_rows(&vault, "doc-1");
    assert_eq!(
        rows_before.len(),
        1,
        "首轮必须恰好落一条 baseline_merge 差集"
    );

    // 次轮：同一 snapshot 在同一 staging 再 apply 一次（同一 digest）。
    apply_baseline_batch(&vault, &[("doc-1".to_owned(), snapshot.clone())], &[])
        .expect("同一 digest 重放必须成功");
    let second = baseline_apply_debug_counters();
    assert_eq!(
        second.existing_local_extra, 1,
        "重放不得再进 detached 分支：{second:?}"
    );
    assert_eq!(
        second.replayed_same_digest, 1,
        "同一 digest 必须走生产既有的 replayed_same_digest 分支：{second:?}"
    );

    let rows_after = baseline_merge_rows(&vault, "doc-1");
    assert_eq!(rows_after.len(), 1, "重放不得新增 baseline_merge 行");
    assert_eq!(
        rows_after[0].payload, rows_before[0].payload,
        "重放不得改写差集 payload"
    );
    assert_eq!(
        rows_after[0].end_vv, rows_before[0].end_vv,
        "重放不得改写差集 VV"
    );
    assert_eq!(
        rows_after[0].end_frontier, rows_before[0].end_frontier,
        "重放不得改写差集 frontier"
    );

    let head_after = head(&vault, "doc-1");
    assert_eq!(
        head_after.head_seq, head_before.head_seq,
        "重放不得推进 head_seq"
    );
    assert_eq!(
        head_after.head_vv, head_before.head_vv,
        "重放不得改 head_vv"
    );
    assert_eq!(
        head_after.head_frontier, head_before.head_frontier,
        "重放不得改 head_frontier"
    );

    let text = assert_detached_matches_attached(&vault, "doc-1", &snapshot, &parts);
    assert_eq!(
        text.len(),
        "LOCAL".len() + "BASE".len(),
        "重放不得重复文本：{text:?}"
    );
    assert_eq!(text.matches("LOCAL").count(), 1);
    assert_eq!(text.matches("BASE").count(), 1);

    // Loro 层：同一 update 导入两次不产生新 op（`import_oplog_only` 依赖的不变量）。
    let once = BodyDoc::restore(
        "idem-once",
        A_PEER,
        None,
        std::slice::from_ref(&parts.merged.payload),
    )
    .expect("单次导入");
    let twice = BodyDoc::restore(
        "idem-twice",
        A_PEER,
        None,
        &[parts.merged.payload.clone(), parts.merged.payload.clone()],
    )
    .expect("重复导入");
    assert_eq!(once.text(), twice.text(), "重复 import 不得改文本");
    assert_eq!(
        once.vv().encode(),
        twice.vv().encode(),
        "重复 import 不得改 VV"
    );
    assert_eq!(
        once.frontiers().encode(),
        twice.frontiers().encode(),
        "重复 import 不得改 frontier"
    );

    // 数据库层：同一 payload 再落一行（模拟 relay 重复投递），生产读取路径必须幂等。
    let duplicate = UpdateRecord::new(
        "duplicate-delivery",
        PendingUpdate {
            payload: parts.merged.payload.clone(),
            end_frontier: parts.merged.end_frontier.clone(),
            end_vv: parts.merged.end_vv.clone(),
        },
    );
    append_updates(&vault, "doc-1", head_after.head_seq, &[duplicate]).expect("重复投递落库");
    let (loaded, loaded_head) =
        load_body_doc(&vault, "doc-1", A_PEER, None).expect("重复投递后重放");
    assert_eq!(loaded.text(), text, "重复投递不得改文本");
    assert_eq!(
        canonical_vv(&loaded.vv().encode()),
        canonical_vv(&parts.merged.end_vv)
    );
    assert_eq!(
        loaded_head.head_vv, parts.merged.end_vv,
        "重复投递不得改 head_vv"
    );
    assert_eq!(loaded_head.head_frontier, parts.merged.end_frontier);
    assert_eq!(
        baseline_merge_rows(&vault, "doc-1").len(),
        1,
        "重复投递不得新增 baseline_merge 行"
    );
}

// ---------------------------------------------------------------------------
// 8. 边界：多 peer 物理乱序导入
// ---------------------------------------------------------------------------

/// 多 peer 物理乱序：D 的 update 先于它依赖的 C、C 又先于它依赖的基线 S0 导入本地
/// （Loro 先把它们挂成 pending，依赖到齐后一次性应用），然后本地再编辑。之后
/// detached 导出必须与 attached 参考完全一致，且 merged 差集只含新基线 S1 新产生的
/// B op——不重复导出本地已导入的 A/C/D op。
#[test]
fn detached_multi_peer_out_of_order_imports_match_attached() {
    let (_dir, vault) = fresh_vault();

    // 基线 S0（peer B）。
    let remote = memory_doc("doc-1", B_PEER, "BASE");
    let first = remote.export_snapshot().expect("基线 S0");
    let first_vv = snapshot_vv(&first);

    // C：在 S0 上插入；D：在 S0 + C 上插入（D 的 op 因果依赖 C 的 op）。
    let third = BodyDoc::restore("doc-1", C_PEER, Some(&first), &[]).expect("C 文档");
    third.insert_scalars(0, "CCC").expect("C 文本");
    let third_update = third.pending_update(&first_vv).expect("C 增量");
    let fourth = BodyDoc::restore("doc-1", D_PEER, Some(&first), &[]).expect("D 文档");
    fourth.import(&third_update.payload).expect("D 导入 C");
    fourth.insert_scalars(0, "DDD").expect("D 文本");
    let fourth_update = fourth.pending_update(&third.vv()).expect("D 增量");

    // 本地 peer A：物理导入顺序 D → C → S0，与因果顺序完全相反。
    let local = seed_note(&vault, "note-1", "doc-1");
    local
        .import(&fourth_update.payload)
        .expect("本地先导 D（缺依赖）");
    local
        .import(&third_update.payload)
        .expect("本地再导 C（缺基线）");
    local.import(&first).expect("本地最后导基线 S0");
    assert_eq!(
        local.text().matches("BASE").count(),
        1,
        "乱序导入后基线文本必须就位：{:?}",
        local.text()
    );
    assert_eq!(
        local.text().matches("CCC").count(),
        1,
        "乱序导入后 C 文本必须就位：{:?}",
        local.text()
    );
    assert_eq!(
        local.text().matches("DDD").count(),
        1,
        "乱序导入后 D 文本必须就位：{:?}",
        local.text()
    );
    local.insert_scalars(0, "AAA").expect("本地编辑");
    commit_now(&vault, &local, "local-batch");

    // 远端在 S0 之后继续写：新基线 S1 必须带来新 op（否则 merged 差集测不到内容）。
    remote
        .insert_scalars(remote.scalar_len(), "NEW")
        .expect("远端新文本");
    let second = remote.export_snapshot().expect("基线 S1");
    let second_vv = snapshot_vv(&second);
    let local_vv = local.vv();
    assert!(
        local_vv.sub_iter(&second_vv).next().is_some(),
        "本地必须相对新基线有额外 op（A/C/D）"
    );

    let parts = merge_detached_with_peers(
        &vault,
        "doc-1",
        &second,
        &local,
        7,
        "cursor-3",
        &[A_PEER, B_PEER],
    );
    // detached 合并后的 oplog VV 必须覆盖全部四个 peer（乱序导入的 A/C/D 与基线的 B）。
    assert_eq!(
        vv_peers(&parts.merged.end_vv),
        vec![A_PEER, B_PEER, C_PEER, D_PEER],
        "合并后的 oplog VV 必须含全部四个 peer"
    );
    let text = assert_detached_matches_attached(&vault, "doc-1", &second, &parts);

    assert_eq!(
        text.len(),
        "AAADDDCCCBASENEW".len(),
        "不得有重复或残留字符：{text:?}"
    );
    for marker in ["AAA", "DDD", "CCC", "BASE", "NEW"] {
        assert_eq!(
            text.matches(marker).count(),
            1,
            "{marker} 必须恰好一次：{text:?}"
        );
    }

    // 本地差集：从 0 起算 A/C/D 的 op，且**不含**基线 S0 的 B op（B 已被 from 覆盖）。
    let delta_meta =
        LoroDoc::decode_import_blob_meta(&parts.local_delta, true).expect("本地差集元信息");
    assert_eq!(
        canonical_of_vv(&delta_meta.partial_start_vv),
        vec![(A_PEER, 0), (C_PEER, 0), (D_PEER, 0)],
        "本地差集必须从 A/C/D 的 0 起算"
    );
    assert_eq!(
        canonical_of_vv(&delta_meta.partial_end_vv),
        vec![
            (A_PEER, counter_of(&local_vv, A_PEER)),
            (C_PEER, counter_of(&local_vv, C_PEER)),
            (D_PEER, counter_of(&local_vv, D_PEER)),
        ],
        "本地差集终点必须是 A/C/D 的本地计数"
    );
    assert!(
        canonical_of_vv(&delta_meta.partial_end_vv)
            .iter()
            .all(|(peer, _)| *peer != B_PEER),
        "本地差集不得包含基线 S0 的 op（from 已覆盖）"
    );

    // merged 差集：起点 = 本地 VV 的 B 计数（S0 已导入），终点 = 新基线 VV，只含 B 的新 op。
    let merged_meta =
        LoroDoc::decode_import_blob_meta(&parts.merged.payload, true).expect("merged 元信息");
    assert_eq!(
        canonical_of_vv(&merged_meta.partial_start_vv),
        vec![(B_PEER, counter_of(&local_vv, B_PEER))],
        "merged 差集必须从本地 VV 的 B 计数起算"
    );
    assert_eq!(
        canonical_of_vv(&merged_meta.partial_end_vv),
        vec![(B_PEER, counter_of(&second_vv, B_PEER))],
        "merged 差集终点必须是新基线 VV"
    );
    let merged_alone = BodyDoc::restore(
        "merged-probe",
        A_PEER,
        None,
        std::slice::from_ref(&parts.merged.payload),
    )
    .expect("merged 差集可重放");
    assert!(
        !merged_alone.text().contains("AAA")
            && !merged_alone.text().contains("CCC")
            && !merged_alone.text().contains("DDD"),
        "merged 差集不得把本地 op 再导出一遍：{merged_alone:?}"
    );
}

// ---------------------------------------------------------------------------
// 9. 边界：大文档（10,000 scalar / 10,000 op）
// ---------------------------------------------------------------------------

/// 大文档：基线 10,000 字（100 块 × 100 字，逐块提交 = 大量 op），本地另有编辑。
/// detached merge 后文本必须与 attached 参考逐字节一致（字符数/字节数精确），
/// 且 merged 差集单独重放恰好还原基线原文（不含本地文本）。
#[test]
fn detached_large_document_merge_matches_attached() {
    let (_dir, vault) = fresh_vault();
    let local = seed_note(&vault, "note-1", "doc-1");
    local.insert_scalars(0, "LOCAL").expect("本地编辑");
    commit_now(&vault, &local, "local-batch");

    // 基线：100 块 × 100 字 = 10,000 个 scalar（每块一次提交）。
    let remote = BodyDoc::create("doc-1", B_PEER).expect("基线文档");
    let chunk = "基".repeat(100);
    for index in 0..100 {
        let end = remote.scalar_len();
        remote.insert_scalars(end, &chunk).expect("基线分块写入");
        assert_eq!(
            remote.scalar_len(),
            (index + 1) * 100,
            "基线长度必须逐块增长"
        );
    }
    let snapshot = remote.export_snapshot().expect("大文档基线快照");
    let baseline_vv = snapshot_vv(&snapshot);
    assert_eq!(
        counter_of(&baseline_vv, B_PEER),
        10_000,
        "基线必须是 10,000 个 op"
    );

    let parts = merge_detached(&vault, "doc-1", &snapshot, &local, 12, "cursor-4");
    let text = assert_detached_matches_attached(&vault, "doc-1", &snapshot, &parts);

    assert_eq!(
        text.matches('基').count(),
        10_000,
        "基线字符必须恰好 10,000 个"
    );
    assert_eq!(text.matches("LOCAL").count(), 1, "本地文本必须恰好一次");
    assert_eq!(text.chars().count(), 10_005, "scalar 总数必须是 10,000 + 5");
    assert_eq!(text.len(), 30_005, "UTF-8 字节数必须原样保留（基 3B/字）");
    assert_eq!(
        text.replace("LOCAL", ""),
        "基".repeat(10_000),
        "除本地前缀外必须与基线逐字节一致"
    );

    // merged 差集 = 基线相对本地 VV 的差集：单独重放恰好是基线原文（不含 LOCAL）。
    let merged_alone = BodyDoc::restore(
        "merged-probe",
        A_PEER,
        None,
        std::slice::from_ref(&parts.merged.payload),
    )
    .expect("大差集可重放");
    assert_eq!(
        merged_alone.scalar_len(),
        10_000,
        "merged 差集必须覆盖全部基线 op"
    );
    assert_eq!(
        merged_alone.text(),
        "基".repeat(10_000),
        "merged 差集单独重放必须等于基线原文"
    );
    assert_eq!(
        canonical_vv(&merged_alone.vv().encode()),
        canonical_of_vv(&baseline_vv),
        "merged 差集 VV 必须等于基线 VV"
    );
}

`````


## `crates/storage/tests/sync_inbox_batch.rs`

`````rust
//! 批量 inbox API（Phase 1A-1）：与逐条入口的语义一致性、冲突不半写、前缀推进，
//! 以及批量 integrate 的严格全 verified 语义。
//!
//! 批量入口 [menghunote_storage::store_inbox_verified_batch] 现在是「既有行比对 +
//! 新行插入 + `received` 前缀推进」同一事务；这里只断言**行为**（与逐条一致、冲突整批
//! 回滚），不对实现（几条 SQL/几次事务）做断言。竞态本身不易在单测里构造，
//! 用「先插入后批量含同 seq」覆盖幂等与冲突两条路径。
//!
//! 末尾另有一组前缀扫描窗口用例：`sync.skipped_received` 标记超过 4096 条（窗口上限）
//! 时必须跨窗口继续扫描，且多个来源同时被截断时不能提前判定缺口。

mod support;

use support::*;

use menghunote_storage::{
    advance_integrated_prefix, append_updates_with_integrated_marks, count_inbox_ready, load_head,
    mark_integrated, mark_integrated_batch, mark_skipped_event_received, prune_covered_inbox,
    set_sync_log_id, store_inbox_verified, store_inbox_verified_batch, sync_cursor_state,
    take_inbox_ready, take_inbox_ready_where, StorageError, UpdateRecord, Value, Vault,
    VerifiedInboxRow, META_SYNC_INTEGRATED_SEQ, META_SYNC_LOG_ID, META_SYNC_RECEIVED_SEQ,
    META_SYNC_SKIPPED_RECEIVED_PREFIX,
};

const LOG_ID: [u8; 16] = [0x3a; 16];

/// 另一个 incarnation（restore 迁移后当前 `sync.log_id` 可能变成它，旧行仍然 durable）。
const OTHER_LOG_ID: [u8; 16] = [0x5b; 16];

fn bind_log(vault: &Vault) {
    set_sync_log_id(vault, &LOG_ID).expect("写 sync.log_id");
}

fn row(seq: u64, seed: u8, payload_len: usize) -> VerifiedInboxRow {
    VerifiedInboxRow {
        log_id: LOG_ID,
        relay_seq: seq,
        object_id: [seed; 32],
        epoch: 1,
        verified_payload: vec![seed; payload_len],
    }
}

fn rows(count: u64, offset: u64) -> Vec<VerifiedInboxRow> {
    (1..=count)
        .map(|index| {
            let seed = u8::try_from(index % 251).expect("seed 在 u8 内");
            row(
                offset + index,
                seed,
                32 + usize::try_from(index).unwrap_or(0),
            )
        })
        .collect()
}

fn received(vault: &Vault) -> u64 {
    sync_cursor_state(vault).expect("游标状态").received_seq
}

/// 一个 `sync.skipped_received` 标记的完整键（与生产写入格式逐字节一致）。
fn skipped_key(seq: u64) -> String {
    let hex: String = LOG_ID.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("{META_SYNC_SKIPPED_RECEIVED_PREFIX}{hex}.{seq:020}")
}

/// 直接写一批 `sync.skipped_received` 标记（**一个**事务，只付一次 fsync）。
///
/// 生产路径 [mark_skipped_event_received] 每写一个标记就推进一次前缀；这里要构造数千个
/// 标记验证**扫描窗口本身**，所以直接准备 durable 状态，不逐条触发推进。
fn seed_skipped_markers(vault: &Vault, seqs: impl IntoIterator<Item = u64>) {
    let mut sql = String::from("BEGIN IMMEDIATE;\n");
    let mut count = 0usize;
    for seq in seqs {
        sql.push_str("INSERT INTO meta(key,value) VALUES('");
        sql.push_str(&skipped_key(seq));
        sql.push_str("',x'31');\n");
        count += 1;
    }
    if count == 0 {
        return;
    }
    sql.push_str("COMMIT;\n");
    vault.exec_batch(&sql).expect("批量写入 skipped 标记");
}

fn integrated(vault: &Vault) -> u64 {
    sync_cursor_state(vault).expect("游标状态").integrated_seq
}

/// 各行的 `(relay_seq, state)`，按 seq 升序（批量 integrate 断言用）。
fn inbox_states(vault: &Vault) -> Vec<(u64, String)> {
    inbox_rows(vault)
        .into_iter()
        .map(|(seq, _, _, state, _)| (seq, state))
        .collect()
}

/// 一条 inbox 行（测试断言用）：`(seq, object_id, epoch, state, 明文负载)`。
type InboxRow = (u64, [u8; 32], u64, String, Vec<u8>);

/// 读出 inbox 的全部行（按 relay_seq 升序）。
fn inbox_rows(vault: &Vault) -> Vec<InboxRow> {
    vault
        .query(
            "SELECT relay_seq,object_id,epoch,state,verified_payload FROM sync_inbox \
             WHERE log_id=?1 ORDER BY relay_seq",
            &[Value::Blob(LOG_ID.to_vec())],
        )
        .expect("读 sync_inbox")
        .iter()
        .map(|row| {
            let seq = match row.first() {
                Some(Value::Integer(value)) => u64::try_from(*value).expect("seq 非负"),
                other => panic!("relay_seq 类型异常：{other:?}"),
            };
            let object_id: [u8; 32] = match row.get(1) {
                Some(Value::Blob(bytes)) => bytes.as_slice().try_into().expect("object_id 32B"),
                other => panic!("object_id 类型异常：{other:?}"),
            };
            let epoch = match row.get(2) {
                Some(Value::Integer(value)) => u64::try_from(*value).expect("epoch 非负"),
                other => panic!("epoch 类型异常：{other:?}"),
            };
            let state = match row.get(3) {
                Some(Value::Text(value)) => value.clone(),
                other => panic!("state 类型异常：{other:?}"),
            };
            let payload = match row.get(4) {
                Some(Value::Blob(bytes)) => bytes.clone(),
                other => panic!("verified_payload 类型异常：{other:?}"),
            };
            (seq, object_id, epoch, state, payload)
        })
        .collect()
}

#[test]
fn batch_matches_per_row_for_256_rows() {
    let (_dir_row, vault_row) = fresh_vault();
    let (_dir_batch, vault_batch) = fresh_vault();
    bind_log(&vault_row);
    bind_log(&vault_batch);
    let input = rows(256, 0);

    for item in &input {
        store_inbox_verified(
            &vault_row,
            &item.log_id,
            item.relay_seq,
            &item.object_id,
            item.epoch,
            &item.verified_payload,
        )
        .expect("逐条写入");
    }
    let inserted = store_inbox_verified_batch(&vault_batch, &input).expect("批量写入");

    assert_eq!(inserted, input.len(), "首次批量写入必须全部是新行");
    assert_eq!(received(&vault_row), 256);
    assert_eq!(received(&vault_batch), 256, "前缀必须推进到整页末尾");
    assert_eq!(
        inbox_rows(&vault_row),
        inbox_rows(&vault_batch),
        "逐条与批量必须产生逐字节相同的 inbox 表"
    );
    assert_eq!(count_inbox_ready(&vault_row).unwrap(), 256);
    assert_eq!(count_inbox_ready(&vault_batch).unwrap(), 256);
    let ready_row = take_inbox_ready(&vault_row, 16).expect("逐条 ready");
    let ready_batch = take_inbox_ready(&vault_batch, 16).expect("批量 ready");
    assert_eq!(ready_row, ready_batch);
}

#[test]
fn batch_is_idempotent_and_returns_zero_for_known_rows() {
    let (_dir, vault) = fresh_vault();
    bind_log(&vault);
    let input = rows(4, 0);
    assert_eq!(store_inbox_verified_batch(&vault, &input).unwrap(), 4);

    // 同字节重放：不新增、不改写、前缀不变。
    assert_eq!(
        store_inbox_verified_batch(&vault, &input).unwrap(),
        0,
        "同字节重放必须幂等跳过"
    );
    assert_eq!(inbox_rows(&vault).len(), 4);
    assert_eq!(received(&vault), 4);

    // 混合：一部分已存在（一致）+ 一部分新行 → 只插入新行，返回值只数新行。
    let mut mixed = rows(4, 0);
    mixed.extend(rows(2, 4));
    assert_eq!(store_inbox_verified_batch(&vault, &mixed).unwrap(), 2);
    assert_eq!(inbox_rows(&vault).len(), 6, "既有 1..4 不得重写，只补 5/6");
    assert_eq!(received(&vault), 6);
}

#[test]
fn batch_conflict_rejects_whole_batch_without_partial_write() {
    let (_dir, vault) = fresh_vault();
    bind_log(&vault);
    store_inbox_verified(&vault, &LOG_ID, 5, &[0x55; 32], 1, b"existing-5").expect("预置 5");
    let before = inbox_rows(&vault);
    let before_received = received(&vault);
    assert_eq!(before.len(), 1);
    assert_eq!(before_received, 0, "缺 1..4，前缀只能停在 0");

    // 4 / 6 是合法新行，5 与既有行字节不同 → 整批拒绝，4/6 也不能写。
    let conflicting = vec![
        row(4, 0x44, 16),
        VerifiedInboxRow {
            log_id: LOG_ID,
            relay_seq: 5,
            object_id: [0x55; 32],
            epoch: 1,
            verified_payload: b"different-5".to_vec(),
        },
        row(6, 0x66, 16),
    ];
    let error = store_inbox_verified_batch(&vault, &conflicting).expect_err("冲突必须报错");
    assert!(
        matches!(error, menghunote_storage::StorageError::SyncConflict { .. }),
        "冲突必须是 SyncConflict，实际 {error:?}"
    );
    assert_eq!(inbox_rows(&vault), before, "冲突批次不得写入任何行");
    assert_eq!(received(&vault), before_received, "冲突批次不得推进前缀");
}

#[test]
fn batch_conflict_does_not_advance_prefix_for_known_rows() {
    let (_dir, vault) = fresh_vault();
    bind_log(&vault);
    // 先有 1..3；批里 1..3 一致 + 4 新 + 5 冲突（不存在 5 但先单独写一条不同字节）。
    store_inbox_verified_batch(&vault, &rows(3, 0)).expect("预置 1..3");
    store_inbox_verified(&vault, &LOG_ID, 5, &[0x99; 32], 1, b"five").expect("预置 5");
    let before = inbox_rows(&vault);
    let before_received = received(&vault);
    assert_eq!(before_received, 3);

    let mut batch = rows(5, 0);
    batch[3] = row(4, 0x04, 8);
    batch[4] = VerifiedInboxRow {
        log_id: LOG_ID,
        relay_seq: 5,
        object_id: [0x99; 32],
        epoch: 1,
        verified_payload: b"other-five".to_vec(),
    };
    let error = store_inbox_verified_batch(&vault, &batch).expect_err("5 冲突");
    assert!(matches!(
        error,
        menghunote_storage::StorageError::SyncConflict { .. }
    ));
    assert_eq!(inbox_rows(&vault), before, "冲突批次不得写入 4");
    assert_eq!(received(&vault), before_received);
}

#[test]
fn batch_sorts_unsorted_input_and_rejects_duplicate_seq() {
    let (_dir, vault) = fresh_vault();
    bind_log(&vault);
    let mut shuffled = rows(3, 0);
    shuffled.swap(0, 2);
    shuffled.swap(0, 1);
    assert_eq!(shuffled[0].relay_seq, 2, "输入确实乱序");
    assert_eq!(
        store_inbox_verified_batch(&vault, &shuffled).unwrap(),
        3,
        "批量入口必须自己按 relay_seq 稳定排序"
    );
    assert_eq!(received(&vault), 3);
    let states: Vec<String> = inbox_rows(&vault)
        .into_iter()
        .map(|(seq, _, _, state, _)| format!("{seq}:{state}"))
        .collect();
    assert_eq!(
        states,
        vec![
            "1:verified".to_owned(),
            "2:verified".to_owned(),
            "3:verified".to_owned()
        ]
    );

    // 同一批重复 seq：直接拒绝，且不写入任何行。
    let before = inbox_rows(&vault);
    let duplicated = vec![row(4, 0xa4, 8), row(4, 0xa4, 8), row(5, 0xa5, 8)];
    let error = store_inbox_verified_batch(&vault, &duplicated).expect_err("重复 seq 必须拒绝");
    assert!(
        matches!(error, menghunote_storage::StorageError::Corrupt { .. }),
        "重复 seq 是上游错误（Corrupt），实际 {error:?}"
    );
    assert_eq!(inbox_rows(&vault), before);
    assert_eq!(received(&vault), 3);
}

#[test]
fn batch_advances_prefix_only_over_contiguous_consumed_seq() {
    let (_dir, vault) = fresh_vault();
    bind_log(&vault);

    // 不连续（缺 3）：前缀只能到 2。
    assert_eq!(store_inbox_verified_batch(&vault, &rows(2, 0)).unwrap(), 2);
    let gap = vec![row(4, 0x44, 8)];
    assert_eq!(store_inbox_verified_batch(&vault, &gap).unwrap(), 1);
    assert_eq!(received(&vault), 2, "缺 3 时前缀必须停在 2");

    // 补上 3 → 一次性推进到 4（跨过此前 durable 的 4）。
    assert_eq!(
        store_inbox_verified_batch(&vault, &[row(3, 0x33, 8)]).unwrap(),
        1
    );
    assert_eq!(received(&vault), 4);

    // ASSET 这类不写 inbox 的事件用 skipped_received 标记参与前缀推进。
    mark_skipped_event_received(&vault, &LOG_ID, 5).expect("标记 5 已消费");
    assert_eq!(received(&vault), 5, "skipped 标记不写 inbox 也要推进前缀");
    assert_eq!(
        store_inbox_verified_batch(&vault, &[row(6, 0x66, 8)]).unwrap(),
        1
    );
    assert_eq!(received(&vault), 6);
}

#[test]
fn batch_rejects_mixed_log_id_and_invalid_seq_without_writing() {
    let (_dir, vault) = fresh_vault();
    bind_log(&vault);

    let mixed = vec![
        row(1, 0x11, 8),
        VerifiedInboxRow {
            log_id: [0x3b; 16],
            relay_seq: 2,
            object_id: [0x22; 32],
            epoch: 1,
            verified_payload: vec![0x22; 8],
        },
    ];
    assert!(store_inbox_verified_batch(&vault, &mixed).is_err());
    assert!(inbox_rows(&vault).is_empty());

    let mut zero_seq = vec![row(1, 0x11, 8)];
    zero_seq[0].relay_seq = 0;
    assert!(store_inbox_verified_batch(&vault, &zero_seq).is_err());
    let mut zero_epoch = vec![row(1, 0x11, 8)];
    zero_epoch[0].epoch = 0;
    assert!(store_inbox_verified_batch(&vault, &zero_epoch).is_err());
    assert!(inbox_rows(&vault).is_empty());
    assert_eq!(received(&vault), 0);

    // 空批次是合法 no-op（便于调用方无条件调用）。
    assert_eq!(store_inbox_verified_batch(&vault, &[]).unwrap(), 0);
}

#[test]
fn batch_rejects_rows_from_a_different_log_than_the_vault() {
    let (_dir, vault) = fresh_vault();
    bind_log(&vault);
    // 与已绑定的 log_id 不同 → SyncLogMismatch（relay reset 必须显式迁移）。
    let other = vec![VerifiedInboxRow {
        log_id: [0x3c; 16],
        relay_seq: 1,
        object_id: [0x11; 32],
        epoch: 1,
        verified_payload: vec![0x11; 4],
    }];
    let error = store_inbox_verified_batch(&vault, &other).expect_err("不同 log_id 必须拒绝");
    assert!(
        matches!(
            error,
            menghunote_storage::StorageError::SyncLogMismatch { .. }
        ),
        "实际 {error:?}"
    );
}

#[test]
fn batch_full_idempotent_replay_repairs_unadvanced_prefix() {
    let (_dir, vault) = fresh_vault();
    bind_log(&vault);
    let input = rows(4, 0);
    assert_eq!(store_inbox_verified_batch(&vault, &input).unwrap(), 4);

    // 模拟「行已经提交、前缀还停在 0」（旧实现事务边界之间崩溃留下的状态）。
    vault
        .set_meta(META_SYNC_RECEIVED_SEQ, &0u64.to_be_bytes())
        .expect("手动退回前缀");
    assert_eq!(received(&vault), 0);

    // 全幂等重放：不新增、不报错、返回 0，但仍要把前缀补到最新。
    assert_eq!(store_inbox_verified_batch(&vault, &input).unwrap(), 0);
    assert_eq!(inbox_rows(&vault).len(), 4);
    assert_eq!(received(&vault), 4, "全幂等重放也要把前缀推进到最新");
}

/// 预置 1..3（连续前缀=3）与 seq5 后，整批 1..6（1..3 幂等、4/6 新、5 被改坏）：
/// 必须整批回滚（4/6 不写、1..3/5 不变、received 保持在 3）。
fn assert_tamper_rolls_back_whole_batch(label: &str, mutate: impl FnOnce(&mut VerifiedInboxRow)) {
    let (_dir, vault) = fresh_vault();
    bind_log(&vault);
    assert_eq!(store_inbox_verified_batch(&vault, &rows(3, 0)).unwrap(), 3);
    let existing = VerifiedInboxRow {
        log_id: LOG_ID,
        relay_seq: 5,
        object_id: [0x55; 32],
        epoch: 1,
        verified_payload: b"existing-5".to_vec(),
    };
    store_inbox_verified(
        &vault,
        &existing.log_id,
        existing.relay_seq,
        &existing.object_id,
        existing.epoch,
        &existing.verified_payload,
    )
    .expect("预置 seq5");
    let before = inbox_rows(&vault);
    assert_eq!(before.len(), 4);
    assert_eq!(received(&vault), 3, "缺 4，前缀只能停在 3");

    let mut tampered = existing;
    mutate(&mut tampered);
    let mut batch = rows(6, 0);
    batch[4] = tampered; // seq5
    let error = match store_inbox_verified_batch(&vault, &batch) {
        Ok(inserted) => panic!("{label} 篡改必须报错，实际插入 {inserted} 行"),
        Err(error) => error,
    };
    assert!(
        matches!(error, StorageError::SyncConflict { .. }),
        "{label}: 冲突必须是 SyncConflict，实际 {error:?}"
    );
    assert_eq!(inbox_rows(&vault), before, "{label}: 整批回滚，既有行不变");
    assert_eq!(
        received(&vault),
        3,
        "{label}: 冲突批次不得推进 received 前缀（4/6 都没写）"
    );
}

#[test]
fn batch_tampered_object_id_epoch_or_payload_rolls_back_whole_batch() {
    assert_tamper_rolls_back_whole_batch("object_id", |row| row.object_id = [0x99; 32]);
    assert_tamper_rolls_back_whole_batch("epoch", |row| row.epoch = 2);
    assert_tamper_rolls_back_whole_batch("payload", |row| {
        row.verified_payload = b"tampered".to_vec();
    });
}

#[test]
fn batch_conflict_rolls_back_log_id_binding_written_in_the_same_transaction() {
    let (_dir, vault) = fresh_vault();
    // 手工写一条 inbox 行但不写 sync.log_id（模拟旧库/异常状态）：
    // 批量入口必须在事务里补写 log_id，而冲突时这笔补写也要回滚。
    vault
        .exec(
            "INSERT INTO sync_inbox(log_id,relay_seq,object_id,epoch,state,verified_payload) \
             VALUES(?1,?2,?3,?4,'verified',?5)",
            &[
                Value::Blob(LOG_ID.to_vec()),
                Value::Integer(5),
                Value::Blob(vec![0x55; 32]),
                Value::Integer(1),
                Value::Blob(b"existing-5".to_vec()),
            ],
        )
        .expect("手工预置 inbox 行");
    assert!(vault
        .get_meta(META_SYNC_LOG_ID)
        .expect("读 log_id")
        .is_none());

    let conflicting = vec![
        row(4, 0x44, 16),
        VerifiedInboxRow {
            log_id: LOG_ID,
            relay_seq: 5,
            object_id: [0x55; 32],
            epoch: 1,
            verified_payload: b"different".to_vec(),
        },
    ];
    let error = store_inbox_verified_batch(&vault, &conflicting).expect_err("冲突必须报错");
    assert!(
        matches!(error, StorageError::SyncConflict { .. }),
        "实际 {error:?}"
    );
    assert!(
        vault
            .get_meta(META_SYNC_LOG_ID)
            .expect("读 log_id")
            .is_none(),
        "sync.log_id 的补写必须与新行一起回滚"
    );
    assert_eq!(received(&vault), 0);
    assert_eq!(inbox_rows(&vault).len(), 1, "新行 4 不得写入");
}

#[test]
fn batch_integrate_marks_all_verified_and_prefix_advances_only_on_demand() {
    let (_dir, vault) = fresh_vault();
    bind_log(&vault);
    assert_eq!(store_inbox_verified_batch(&vault, &rows(3, 0)).unwrap(), 3);
    assert_eq!(received(&vault), 3);
    assert_eq!(integrated(&vault), 0);

    // 空输入是合法 no-op。
    assert_eq!(mark_integrated_batch(&vault, &LOG_ID, &[]).unwrap(), 0);

    assert_eq!(
        mark_integrated_batch(&vault, &LOG_ID, &[1, 2, 3]).unwrap(),
        3
    );
    assert_eq!(
        inbox_states(&vault),
        vec![
            (1, "integrated".to_owned()),
            (2, "integrated".to_owned()),
            (3, "integrated".to_owned())
        ]
    );
    assert_eq!(
        integrated(&vault),
        0,
        "mark_integrated_batch 不得自己推进 integrated 前缀"
    );

    // 调用方在整批正文完成后统一推进一次。
    advance_integrated_prefix(&vault).expect("推进 integrated 前缀");
    assert_eq!(integrated(&vault), 3);
    // 再推进一次是幂等 no-op。
    advance_integrated_prefix(&vault).expect("重复推进");
    assert_eq!(integrated(&vault), 3);

    // 单条入口行为保持：已 integrated 的行重复标记仍是成功（幂等）。
    mark_integrated(&vault, &LOG_ID, 2).expect("单条幂等");
    assert_eq!(integrated(&vault), 3);
}

#[test]
fn batch_integrate_rolls_back_when_a_row_is_not_verified() {
    let (_dir, vault) = fresh_vault();
    bind_log(&vault);
    assert_eq!(store_inbox_verified_batch(&vault, &rows(3, 0)).unwrap(), 3);
    // 先把 seq2 标成 integrated：1 未集成，连续前缀仍停在 0。
    mark_integrated(&vault, &LOG_ID, 2).expect("先集成 seq2");
    assert_eq!(integrated(&vault), 0);
    let before = inbox_rows(&vault);

    let error =
        mark_integrated_batch(&vault, &LOG_ID, &[1, 2, 3]).expect_err("seq2 已不是 verified");
    assert!(
        matches!(error, StorageError::SyncConflict { .. }),
        "含已 integrated 行必须整批冲突，实际 {error:?}"
    );
    assert_eq!(
        inbox_rows(&vault),
        before,
        "整批回滚：1/3 必须仍是 verified"
    );
    assert_eq!(integrated(&vault), 0);
}

#[test]
fn batch_integrate_rolls_back_when_a_row_is_missing() {
    let (_dir, vault) = fresh_vault();
    bind_log(&vault);
    assert_eq!(store_inbox_verified_batch(&vault, &rows(2, 0)).unwrap(), 2);
    let before = inbox_rows(&vault);

    let error = mark_integrated_batch(&vault, &LOG_ID, &[1, 9]).expect_err("seq9 不存在");
    assert!(
        matches!(error, StorageError::SyncConflict { .. }),
        "含不存在行必须整批冲突，实际 {error:?}"
    );
    assert_eq!(
        inbox_rows(&vault),
        before,
        "整批回滚：seq1 必须仍是 verified"
    );
    assert_eq!(integrated(&vault), 0);
}

#[test]
fn batch_integrate_prefix_crosses_asset_skipped_marker() {
    let (_dir, vault) = fresh_vault();
    bind_log(&vault);
    assert_eq!(store_inbox_verified_batch(&vault, &rows(3, 0)).unwrap(), 3);
    // seq4 是 ASSET：不写 inbox，只留 durable skipped 标记。
    mark_skipped_event_received(&vault, &LOG_ID, 4).expect("标记 seq4 已消费");
    assert_eq!(received(&vault), 4);

    assert_eq!(
        mark_integrated_batch(&vault, &LOG_ID, &[1, 2, 3]).unwrap(),
        3
    );
    assert_eq!(integrated(&vault), 0);
    advance_integrated_prefix(&vault).expect("推进 integrated 前缀");
    assert_eq!(
        integrated(&vault),
        4,
        "integrated 前缀同样要越过 ASSET skipped 标记"
    );
}

// ============================================================ Phase 1A-2：A 事务内标记

/// `append_updates_with_integrated_marks`：update 与 inbox 标记同一事务提交，**不**自己
/// 推进 integrated 前缀（调用方整批结束后统一推进）。
#[test]
fn append_with_integrated_marks_commits_update_and_marks_together() {
    let (_dir, vault) = fresh_vault();
    bind_log(&vault);
    assert_eq!(store_inbox_verified_batch(&vault, &rows(3, 0)).unwrap(), 3);

    let doc = seed_note(&vault, "note-1", "doc-1");
    doc.insert_scalars(0, "hello").expect("写正文");
    let current = head(&vault, "doc-1");
    let base = current.version_vector().expect("解码 head vv");
    let update = pending(&doc, &base).expect("导出增量");
    let record = UpdateRecord::new("batch-1", update);

    let head_after = append_updates_with_integrated_marks(
        &vault,
        "doc-1",
        current.head_seq,
        &record,
        &LOG_ID,
        &[1, 2],
    )
    .expect("同事务 append + 标记");
    assert_eq!(head_after.head_seq, 1);
    assert_eq!(load_head(&vault, "doc-1").expect("head").head_seq, 1);
    assert_eq!(
        inbox_states(&vault),
        vec![
            (1, "integrated".to_owned()),
            (2, "integrated".to_owned()),
            (3, "verified".to_owned())
        ]
    );
    assert_eq!(integrated(&vault), 0, "本 API 不推进前缀");
    assert_eq!(count_inbox_ready(&vault).unwrap(), 1);

    advance_integrated_prefix(&vault).expect("统一推进前缀");
    assert_eq!(integrated(&vault), 2, "前缀只到连续 integrated 的末尾");
}

/// 标记守卫失败（不存在 / 已不是 verified）→ **整笔 A 事务回滚**：update 不落库、
/// 其它行的标记也不保留，inbox 与 head 完全回到调用前。
#[test]
fn append_with_integrated_marks_rolls_back_update_when_a_mark_is_not_verified() {
    let (_dir, vault) = fresh_vault();
    bind_log(&vault);
    assert_eq!(store_inbox_verified_batch(&vault, &rows(2, 0)).unwrap(), 2);
    // seq2 提前 integrated（seq1 仍 verified）：标记 [1,2] 时第二条守卫会失败。
    mark_integrated(&vault, &LOG_ID, 2).expect("先集成 seq2");
    let before = inbox_rows(&vault);

    let doc = seed_note(&vault, "note-1", "doc-1");
    doc.insert_scalars(0, "hello").expect("写正文");
    let current = head(&vault, "doc-1");
    let base = current.version_vector().expect("解码 head vv");
    let update = pending(&doc, &base).expect("导出增量");
    let record = UpdateRecord::new("batch-1", update);

    let error = append_updates_with_integrated_marks(
        &vault,
        "doc-1",
        current.head_seq,
        &record,
        &LOG_ID,
        &[1, 2],
    )
    .expect_err("含非 verified 行必须整笔失败");
    assert!(
        matches!(error, StorageError::SyncConflict { .. }),
        "实际 {error:?}"
    );
    assert_eq!(
        load_head(&vault, "doc-1").expect("head").head_seq,
        0,
        "update 必须随标记失败一起回滚"
    );
    assert_eq!(inbox_rows(&vault), before, "整批标记回滚");
    assert_eq!(integrated(&vault), 0);

    // 同一份 update 只标记合法行时成功（证明失败是原子的，不是数据被吃掉了）。
    let head_after = append_updates_with_integrated_marks(
        &vault,
        "doc-1",
        current.head_seq,
        &record,
        &LOG_ID,
        &[1],
    )
    .expect("合法标记成功");
    assert_eq!(head_after.head_seq, 1);
    assert_eq!(inbox_rows(&vault)[0].3, "integrated");
}

// ===================================== 前缀扫描窗口（>4096 个 skipped 标记 / 并发写锁）

/// `skipped_received` 窗口在 4096 条处被 `LIMIT` 截断时必须继续下一窗口。
///
/// 回归点：若只给 skipped 窗口加 `LIMIT`、循环条件仍沿用「合并后的 truncated 且
/// current < last」，这里 skipped 窗口只看到 1..=4096、inbox 窗口里还有更大的 5001，
/// 于是 `current(4096) < last(5001)` 会提前 return，把第二个窗口里的 4097..=5000
/// 永久漏掉、`received` 卡死在 4096。
#[test]
fn received_prefix_crosses_more_than_one_skipped_window() {
    let (_dir, vault) = fresh_vault();
    bind_log(&vault);
    // 1..=4096 正好填满第一个窗口（`LIMIT 4096` 截断）；4097..=5000 在第二个窗口。
    seed_skipped_markers(&vault, 1..=5000);

    let inserted = store_inbox_verified_batch(&vault, &[row(5001, 0x51, 8)]).expect("批量写");
    assert_eq!(inserted, 1);
    assert_eq!(
        received(&vault),
        5001,
        "skipped 窗口被截断后必须继续扫描下一窗口，直到第一个缺口 5002"
    );
}

/// 跨窗口扫描后，前缀仍然只推进到**第一个缺口**：第二窗口里 4098 起的标记不能把
/// 缺掉的 4097 一起算成已消费。
#[test]
fn received_prefix_stops_at_gap_across_skipped_windows() {
    let (_dir, vault) = fresh_vault();
    bind_log(&vault);
    // 第一窗口 1..=4096（截断），4097 故意缺失，4098..=4999 落在第二窗口。
    seed_skipped_markers(&vault, 1..=4096);
    seed_skipped_markers(&vault, 4098..=4999);

    assert_eq!(
        store_inbox_verified_batch(&vault, &[row(5000, 0x50, 8)]).unwrap(),
        1
    );
    assert_eq!(
        received(&vault),
        4096,
        "即使 4098.. 与 inbox 5000 都已 durable，缺口 4097 也必须卡住前缀"
    );

    // 补上缺口 → 一次推进到 5000（跨过第二窗口的 skipped 标记与 inbox 行）。
    mark_skipped_event_received(&vault, &LOG_ID, 4097).expect("补 4097");
    assert_eq!(received(&vault), 5000);
}

/// 混合 inbox 行 + skipped 标记 + 缺口：前缀只到第一个缺口；补上后一次跨过。
#[test]
fn received_prefix_mixes_inbox_and_skipped_until_first_gap() {
    let (_dir, vault) = fresh_vault();
    bind_log(&vault);
    assert_eq!(store_inbox_verified_batch(&vault, &rows(3, 0)).unwrap(), 3);
    mark_skipped_event_received(&vault, &LOG_ID, 4).expect("skipped 4");
    assert_eq!(received(&vault), 4);

    // 6 已 durable，但 5 是缺口：前缀必须停在 4。
    assert_eq!(
        store_inbox_verified_batch(&vault, &[row(6, 0x66, 8)]).unwrap(),
        1
    );
    assert_eq!(received(&vault), 4, "缺 5 时跳过 6 也不得推进前缀");

    // 用 skipped 标记补上 5 → 一次推进到 6。
    mark_skipped_event_received(&vault, &LOG_ID, 5).expect("skipped 5");
    assert_eq!(received(&vault), 6);
}

/// 两个来源的窗口**同时**被截断：奇数序号走 inbox、偶数序号走 skipped，前缀要跨越
/// 8192 之后继续推进到 8200。
#[test]
fn received_prefix_advances_while_both_windows_are_truncated() {
    let (_dir, vault) = fresh_vault();
    bind_log(&vault);

    // inbox：1,3,...,8199（4100 行 > 窗口 4096）；skipped：2,4,...,8200（4100 个标记）。
    let inbox: Vec<VerifiedInboxRow> = (0..4100u64)
        .map(|index| {
            let seq = 1 + 2 * index;
            row(seq, u8::try_from(seq % 251).expect("seed 在 u8 内"), 8)
        })
        .collect();
    seed_skipped_markers(&vault, (0..4100u64).map(|index| 2 + 2 * index));

    assert_eq!(
        store_inbox_verified_batch(&vault, &inbox).unwrap(),
        inbox.len()
    );
    assert_eq!(
        received(&vault),
        8200,
        "两个窗口各自截断在 4096/8192，前缀必须跨窗口继续推进到 8200"
    );
}

/// 外部写事务占着写锁时，批量入口必须在 **BEGIN 阶段**整笔失败（SQLITE_BUSY），
/// 而不是先读到旧快照、按字节比较出 SyncConflict；失败不留半写，锁释放后可重放。
///
/// 这是 Immediate 事务可从外部观测到的行为：deferred 的 `BEGIN` 不取写锁，读路径
/// 会在锁被占时照常完成并先返回 SyncConflict。
#[test]
fn batch_under_foreign_write_lock_fails_busy_without_partial_write() {
    let (dir, vault) = fresh_vault();
    bind_log(&vault);
    store_inbox_verified(&vault, &LOG_ID, 1, &[0x11; 32], 1, b"one").expect("预置 seq1");
    assert_eq!(received(&vault), 1);

    // 本连接不等锁（生产是 5s）：让 BEGIN IMMEDIATE 的失败立刻返回，测试无需 sleep。
    vault
        .exec_batch("PRAGMA busy_timeout=0")
        .expect("busy_timeout=0");

    // 第二个 Vault（= 第二个连接）先占住写锁。
    let holder = open_vault(&vault_path(&dir));
    holder
        .exec_batch("BEGIN IMMEDIATE")
        .expect("外部连接占写锁");
    assert_eq!(inbox_rows(&vault).len(), 1, "读快照不受外部写锁影响");

    // 同 seq、不同字节：deferred 会先读到 seq1 并报 SyncConflict；Immediate 在 BEGIN
    // 就撞上写锁 → SQLITE_BUSY。batch 里再带一条新行 2，确认不会留下半写。
    let conflicting = vec![
        VerifiedInboxRow {
            log_id: LOG_ID,
            relay_seq: 1,
            object_id: [0x11; 32],
            epoch: 1,
            verified_payload: b"changed".to_vec(),
        },
        row(2, 0x22, 8),
    ];
    let error = store_inbox_verified_batch(&vault, &conflicting).expect_err("写锁被占必须失败");
    assert!(
        matches!(
            error,
            StorageError::Sqlite(rusqlite::Error::SqliteFailure(code, _))
                if code.code == rusqlite::ErrorCode::DatabaseBusy
        ),
        "并发写事务在场时必须整笔 SQLITE_BUSY，实际 {error:?}"
    );

    holder.exec_batch("ROLLBACK").expect("释放外部写锁");
    drop(holder);

    assert_eq!(inbox_rows(&vault).len(), 1, "失败批次不得留下半写（seq2）");
    assert_eq!(received(&vault), 1, "失败批次不得推进前缀");

    // 锁释放后重放同一批：现在能读到真实冲突，仍然整批回滚。
    let error = store_inbox_verified_batch(&vault, &conflicting).expect_err("真实冲突");
    assert!(
        matches!(error, StorageError::SyncConflict { .. }),
        "实际 {error:?}"
    );
    assert_eq!(inbox_rows(&vault).len(), 1);
    assert_eq!(received(&vault), 1);
}

// ================================ incarnation 过滤（restore 迁移 log_id 后的旧行）

/// 当前 incarnation 的 ready `relay_seq`（升序；`take_inbox_ready` 已按 log/seq 排序）。
fn ready_seqs(vault: &Vault, limit: usize) -> Vec<u64> {
    take_inbox_ready(vault, limit)
        .expect("读 ready")
        .into_iter()
        .map(|row| row.relay_seq)
        .collect()
}

/// 直接写 `meta` 原始字节（模拟 restore 的显式迁移 / 损坏状态，绕过 Vault 校验）。
fn write_raw_meta(vault: &Vault, key: &str, value: &[u8]) {
    vault
        .exec(
            "INSERT INTO meta(key,value) VALUES(?1,?2) \
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            &[Value::Text(key.to_owned()), Value::Blob(value.to_vec())],
        )
        .expect("写原始 meta");
}

/// 直接插入一条**其它 incarnation** 的 verified 行。
///
/// 生产入口 `store_inbox_verified*` 都先 `ensure_log_id`，跨代写入会被
/// `SyncLogMismatch` 拒绝；这种 durable 状态只能由 restore 显式迁移 `sync.log_id`
/// 后留下，所以测试直接用 SQL 准备，不经过被测试的过滤逻辑。
fn insert_stale_incarnation_row(vault: &Vault, log_id: &[u8; 16], relay_seq: u64, seed: u8) {
    vault
        .exec(
            "INSERT INTO sync_inbox(log_id,relay_seq,object_id,epoch,state,verified_payload) \
             VALUES(?1,?2,?3,?4,'verified',?5)",
            &[
                Value::Blob(log_id.to_vec()),
                Value::Integer(i64::try_from(relay_seq).expect("relay_seq 在 i64 内")),
                Value::Blob(vec![seed; 32]),
                Value::Integer(1),
                Value::Blob(vec![seed; 16]),
            ],
        )
        .expect("插入旧 incarnation 的 verified 行");
}

/// 当前 log A 有 verified 行、库里另有 log B 的 verified 行时：
/// ready/count 只计 A；B 即使 `relay_seq <= received` 也不计入；received 前缀只按 A 推进。
#[test]
fn ready_and_count_only_include_current_log_incarnation() {
    let (_dir, vault) = fresh_vault();
    bind_log(&vault);
    assert_eq!(store_inbox_verified_batch(&vault, &rows(2, 0)).unwrap(), 2);
    assert_eq!(received(&vault), 2);

    // 旧 incarnation B 的 1..=5 仍在库里（restore 迁移 log_id 后旧行不能被静默删除）。
    for seq in 1..=5u64 {
        insert_stale_incarnation_row(
            &vault,
            &OTHER_LOG_ID,
            seq,
            u8::try_from(seq).expect("seq 在 u8 内"),
        );
    }
    // `set_sync_log_id` 的语义未变：生产入口仍拒绝跨代写入（restore 走显式迁移）。
    assert!(
        matches!(
            set_sync_log_id(&vault, &OTHER_LOG_ID),
            Err(StorageError::SyncLogMismatch { .. })
        ),
        "set_sync_log_id 必须继续拒绝跨代变更"
    );

    // B 的 seq1/seq2 都 <= received(2)，但 ready/count 只能看到 A 的 1/2。
    assert_eq!(ready_seqs(&vault, 16), vec![1, 2]);
    assert_eq!(
        count_inbox_ready(&vault).unwrap(),
        2,
        "旧 incarnation 的行即使 relay_seq<=received 也不得计入"
    );

    // A 的 seq3 落库 → received 只推进到 3，不跨 B 的 1..=5。
    assert_eq!(
        store_inbox_verified_batch(&vault, &[row(3, 0x33, 8)]).unwrap(),
        1
    );
    assert_eq!(received(&vault), 3, "received 前缀推进不跨 incarnation");
    assert_eq!(ready_seqs(&vault, 16), vec![1, 2, 3]);
    assert_eq!(count_inbox_ready(&vault).unwrap(), 3);

    // 状态变化同样只作用于当前 log：集成 A 的 seq1 后 B 的行仍不 ready、不影响计数。
    mark_integrated(&vault, &LOG_ID, 1).expect("集成 A seq1");
    assert_eq!(ready_seqs(&vault, 16), vec![2, 3]);
    assert_eq!(count_inbox_ready(&vault).unwrap(), 2);
}

/// `take_inbox_ready_where` 走同一条底层路径：谓词只能看到当前 log 的行。
#[test]
fn ready_where_path_filters_other_log_incarnation() {
    let (_dir, vault) = fresh_vault();
    bind_log(&vault);
    assert_eq!(store_inbox_verified_batch(&vault, &rows(3, 0)).unwrap(), 3);
    for seq in 1..=3u64 {
        insert_stale_incarnation_row(
            &vault,
            &OTHER_LOG_ID,
            seq,
            0xb0 + u8::try_from(seq).unwrap_or(0),
        );
    }

    // 恒真谓词 + 足够大的 limit：旧实现会把 B 的 3 行也交进来（6 条），现在必须只有 3 条。
    let mut observed_log_ids = Vec::new();
    let picked = take_inbox_ready_where(&vault, 16, |row| {
        observed_log_ids.push(row.log_id);
        true
    })
    .expect("where 路径读 ready");
    assert_eq!(
        picked.iter().map(|row| row.relay_seq).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    assert!(
        observed_log_ids.iter().all(|log_id| *log_id == LOG_ID),
        "谓词只应看到当前 incarnation 的行，实际 {observed_log_ids:?}"
    );

    // 谓词本身的过滤仍然生效（不是把 where 语义吞掉）。
    let picked: Vec<u64> = take_inbox_ready_where(&vault, 16, |row| row.relay_seq != 2)
        .expect("where 过滤")
        .into_iter()
        .map(|row| row.relay_seq)
        .collect();
    assert_eq!(picked, vec![1, 3]);
}

/// `sync.log_id` 缺失 / 长度非法时的语义：
/// - 缺失 → 空（`take`）/ 0（`count`），即新库旧行为；即使残留其它 log 的行与 received，
///   也不能回退成"不过滤 log"；
/// - 长度非法 → 明确 `Corrupt`（与 `sync_cursor_state` 同口径），不静默当作没有 log。
#[test]
fn ready_without_log_id_is_empty_and_invalid_length_is_corrupt() {
    let (_dir, vault) = fresh_vault();
    // 新库：没有 sync.log_id，也没有 inbox 行 → 空/0（旧实现同样返回空）。
    assert!(take_inbox_ready(&vault, 8)
        .expect("无 log 读 ready")
        .is_empty());
    assert_eq!(count_inbox_ready(&vault).expect("无 log 计数"), 0);

    // 不一致的持久状态：有遗留 verified 行与 received，却没有 sync.log_id。
    // 没有当前 incarnation 就没有可匹配的 log，ready 必须为空。
    insert_stale_incarnation_row(&vault, &OTHER_LOG_ID, 1, 0x5b);
    write_raw_meta(&vault, META_SYNC_RECEIVED_SEQ, &1u64.to_be_bytes());
    assert!(
        take_inbox_ready(&vault, 8)
            .expect("无 log 读 ready")
            .is_empty(),
        "缺 sync.log_id 时不能漏出其它 incarnation 的遗留行"
    );
    assert_eq!(count_inbox_ready(&vault).expect("无 log 计数"), 0);

    // 长度不是 16B = 损坏：显式错误，不静默当成"没有 log"。
    write_raw_meta(&vault, META_SYNC_LOG_ID, b"short");
    let error = take_inbox_ready(&vault, 8).expect_err("非法 log_id 长度必须报错");
    assert!(
        matches!(error, StorageError::Corrupt { .. }),
        "实际 {error:?}"
    );
    let error = count_inbox_ready(&vault).expect_err("非法 log_id 长度必须报错");
    assert!(
        matches!(error, StorageError::Corrupt { .. }),
        "实际 {error:?}"
    );
}

#[test]
fn prune_covered_inbox_requires_integrated_watermark() {
    let (_dir, vault) = fresh_vault();
    bind_log(&vault);
    store_inbox_verified_batch(&vault, &rows(3, 0)).expect("写 inbox");
    assert_eq!(inbox_rows(&vault).len(), 3);

    // integrated=2 < 3：一行都不删。
    vault
        .set_meta(META_SYNC_INTEGRATED_SEQ, &2u64.to_be_bytes())
        .expect("写 integrated");
    assert_eq!(
        prune_covered_inbox(&vault, &LOG_ID, 3).expect("prune below watermark"),
        0,
        "integrated < S 时绝不能删"
    );
    assert_eq!(inbox_rows(&vault).len(), 3);

    // integrated=3：删掉 <=3 的行；别的 incarnation 不受影响。
    vault
        .set_meta(META_SYNC_INTEGRATED_SEQ, &3u64.to_be_bytes())
        .expect("写 integrated");
    assert_eq!(
        prune_covered_inbox(&vault, &LOG_ID, 3).expect("prune covered"),
        3
    );
    assert!(inbox_rows(&vault).is_empty());

    // 缺失 integrated 也 fail-closed：不删。
    vault
        .exec(
            "DELETE FROM meta WHERE key=?1",
            &[Value::Text(META_SYNC_INTEGRATED_SEQ.to_owned())],
        )
        .expect("删 integrated");
    store_inbox_verified_batch(&vault, &rows(1, 3)).expect("再写一条");
    assert_eq!(
        prune_covered_inbox(&vault, &LOG_ID, 4).expect("prune without watermark"),
        0,
        "没有 integrated 证据时不得删"
    );
}

`````


## `crates/sync/tests/bootstrap.rs`

`````rust
//! S7-18：新设备从 relay 全量 bootstrap 的集成测试（FakeRelay + 真 SQLite/Loro）。
//!
//! 覆盖评审 0002 P1-4：
//! - body / structure / asset 各阶段失败或"崩溃"后**不推进水位**、journal 可续跑；
//! - 水位 CAS 失败不写半套单调值；
//! - 已有库的 pins/highwater 不被 bootstrap 降低（highwater = max(现有, S)）；
//! - fallback 只用自己的 S/cursor；
//! - 实际 VV/frontier 校验由 `bootstrap.rs` 单测覆盖（不只信 manifest 声明）。
//!
//! 全程无 sleep：失败点由 `BootstrapFault` 或真实 CAS 冲突制造，收敛点都是显式返回值。

mod support;

use support::*;

use menghunote_storage::{
    abort_baseline_apply, asset_bytes, baseline_apply_state, baseline_verify_failure,
    body_doc_meta, create_note_slot, ensure_structure_row, insert_asset, list_all_docs, list_notes,
    load_body_doc, sync_cursor_state, BaselineApplyStatus, BodyDoc, StructureWriter, Value,
    META_SYNC_CONFIRMED_HIGHWATER, META_SYNC_INTEGRATED_SEQ, META_SYNC_RECEIVED_SEQ,
    STRUCTURE_DOC_ID,
};
use menghunote_sync::bootstrap::{
    self, control_head_hash, BootstrapFault, BootstrapPins, META_BOOTSTRAP_CONTROL_HASH,
    META_BOOTSTRAP_CONTROL_REV, META_BOOTSTRAP_EPOCH, META_BOOTSTRAP_INSTANCE,
};
use menghunote_sync::checkpoint::{CheckpointWriter, SlotSecrets, StorageAssets};
use menghunote_sync::engine::{EngineIdentity, SyncEngine, RELAY_CURSOR_META};
use menghunote_sync::genesis;
use menghunote_sync::keys::RootSecretV1;
use menghunote_sync::relay::RelayClient as _;
use menghunote_sync_proto::ObjectType;
use sha2::{Digest, Sha256};

const STRUCTURED_TEXT: &str = "结构化笔记正文";
const ASSET_DATA: &[u8] = b"bootstrap real asset";

fn setup() -> (
    tempfile::TempDir,
    menghunote_storage::Vault,
    genesis::GenesisMaterials,
) {
    let (dir, vault) = fresh_vault();
    ensure_structure_row(&vault).expect("structure 行");
    let materials = genesis::build_materials(
        VAULT_ID,
        [0x92; 16],
        "pw",
        1_700_000_000,
        &mut TestRandom::new(0x92_18),
    )
    .expect("genesis 材料");
    (dir, vault, materials)
}

fn add_note(
    vault: &menghunote_storage::Vault,
    note_id: &str,
    peer: u64,
    text: &str,
    message_id: &str,
    object_id: [u8; 32],
) {
    let doc = seed_doc(vault, note_id, peer);
    create_note_slot(vault, note_id, note_id, note_id, 1_700_000_000).expect("notes 行");
    doc.insert_scalars(0, text).expect("正文");
    commit_with_intent(vault, &doc, message_id, object_id, EPOCH);
}

fn new_relay(materials: &genesis::GenesisMaterials) -> FakeRelay {
    let mut relay = FakeRelay::new(&[]);
    relay
        .certs
        .insert(materials.device_id, materials.certificate().clone());
    relay
}

fn seed_source(
    source: &menghunote_storage::Vault,
    relay: &mut FakeRelay,
    materials: &genesis::GenesisMaterials,
) {
    // 真实设备在 genesis 后已有 relay 游标；checkpoint validate 依赖它核对 S 之前的日志。
    source
        .set_meta(RELAY_CURSOR_META, b"c0")
        .expect("写 relay 游标");
    add_note(
        source,
        "note-1",
        materials.peer_id_u64(),
        "第一行正文",
        &uuid_like(901),
        [0xA1; 32],
    );
    add_note(
        source,
        "note-2",
        materials.peer_id_u64(),
        "second note",
        &uuid_like(902),
        [0xA2; 32],
    );
    // 无 notes 行的墓碑文档（checkpoint 必须导出，bootstrap 必须恢复）。
    let orphan = seed_doc(source, "orphan-body", materials.peer_id_u64());
    orphan.insert_scalars(0, "orphan text").expect("墓碑正文");
    commit_local(source, &orphan);
    let hash: [u8; 32] = Sha256::digest(ASSET_DATA).into();
    insert_asset(
        source,
        &hash,
        ASSET_DATA.len() as u64,
        Some("text/plain"),
        ASSET_DATA,
    )
    .expect("资产");

    let mut writer = CheckpointWriter::new(
        source,
        relay,
        VAULT_ID,
        materials.device_id,
        materials.signing_key.clone(),
        materials.root.clone(),
        materials.peer_id_u64(),
    );
    let secrets = SlotSecrets {
        password: Some("pw"),
        recovery_seed: Some(&materials.recovery_seed),
    };
    writer
        .upload_initial_slots(&materials.password_slot, &materials.recovery_slot, &secrets)
        .expect("槽上传");
    writer
        .create_checkpoint(&StorageAssets)
        .expect("checkpoint 1");
    writer
        .create_checkpoint(&StorageAssets)
        .expect("checkpoint 2");
}

fn flush_pending_and_checkpoint(
    source: &menghunote_storage::Vault,
    relay: &mut FakeRelay,
    materials: &genesis::GenesisMaterials,
) {
    source.set_meta(RELAY_CURSOR_META, b"c0").expect("cursor");
    source
        .set_meta(META_SYNC_RECEIVED_SEQ, &[0u8; 8])
        .expect("received");
    source
        .set_meta(META_SYNC_INTEGRATED_SEQ, &[0u8; 8])
        .expect("integrated");
    let mut engine = SyncEngine::new(
        source,
        relay,
        EngineIdentity {
            device_id: materials.device_id,
            signing_key: materials.signing_key.clone(),
            peer_id: materials.peer_id_u64(),
        },
        materials.root.clone(),
    );
    engine.sync_once().expect("flush seed updates");
    let mut writer = CheckpointWriter::new(
        source,
        relay,
        VAULT_ID,
        materials.device_id,
        materials.signing_key.clone(),
        materials.root.clone(),
        materials.peer_id_u64(),
    );
    writer
        .create_checkpoint(&StorageAssets)
        .expect("checkpoint after flush");
}

/// 造一份"结构文档 + 一条已推到 relay 的正文"的源；返回正文 doc id。
///
/// relay head 推到 1 后打两份 checkpoint（S=1），让"水位是否推进"可观察。
fn seed_structured_source(
    source: &menghunote_storage::Vault,
    relay: &mut FakeRelay,
    materials: &genesis::GenesisMaterials,
) -> String {
    let peer = materials.peer_id_u64();
    source
        .set_meta(RELAY_CURSOR_META, b"c0")
        .expect("写 relay 游标");
    source
        .set_meta(META_SYNC_RECEIVED_SEQ, &[0u8; 8])
        .expect("received");
    source
        .set_meta(META_SYNC_INTEGRATED_SEQ, &[0u8; 8])
        .expect("integrated");

    let mut writer = StructureWriter::open(source, peer).expect("structure writer");
    let seed = writer
        .doc()
        .create_note("结构标题", 1_700_000_000)
        .expect("结构笔记");
    writer.commit("structure").expect("结构提交");
    let body_doc_id = seed.body_doc_id.clone();
    let doc = BodyDoc::restore(body_doc_id.clone(), peer, None, &[]).expect("正文 doc");
    doc.insert_scalars(0, STRUCTURED_TEXT).expect("正文");
    commit_with_intent(source, &doc, &uuid_like(801), [0xD1; 32], EPOCH);

    let hash: [u8; 32] = Sha256::digest(ASSET_DATA).into();
    insert_asset(
        source,
        &hash,
        ASSET_DATA.len() as u64,
        Some("text/plain"),
        ASSET_DATA,
    )
    .expect("资产");

    let mut engine = SyncEngine::new(
        source,
        relay,
        EngineIdentity {
            device_id: materials.device_id,
            signing_key: materials.signing_key.clone(),
            peer_id: peer,
        },
        materials.root.clone(),
    );
    engine.sync_once().expect("push 正文 update");

    let mut writer = CheckpointWriter::new(
        source,
        relay,
        VAULT_ID,
        materials.device_id,
        materials.signing_key.clone(),
        materials.root.clone(),
        peer,
    );
    let secrets = SlotSecrets {
        password: Some("pw"),
        recovery_seed: Some(&materials.recovery_seed),
    };
    writer
        .upload_initial_slots(&materials.password_slot, &materials.recovery_slot, &secrets)
        .expect("槽上传");
    writer
        .create_checkpoint(&StorageAssets)
        .expect("checkpoint 1");
    writer
        .create_checkpoint(&StorageAssets)
        .expect("checkpoint 2");
    body_doc_id
}

fn set_u64_meta(vault: &menghunote_storage::Vault, key: &str, value: u64) {
    vault
        .set_meta(key, &value.to_be_bytes())
        .expect("写 BE64 meta");
}

fn relay_state_and_control_hash(relay: &mut FakeRelay) -> ([u8; 16], [u8; 32]) {
    let state = relay.state().expect("relay state");
    let chain = relay.management_chain().expect("management chain");
    (state.instance_id, control_head_hash(&state, &chain))
}

fn assert_no_sync_watermarks(vault: &menghunote_storage::Vault) {
    assert!(
        vault
            .get_meta(RELAY_CURSOR_META)
            .expect("cursor meta")
            .is_none(),
        "失败路径不得推进 relay 游标"
    );
    assert!(
        vault
            .get_meta(META_SYNC_RECEIVED_SEQ)
            .expect("received meta")
            .is_none(),
        "失败路径不得推进 received"
    );
    assert!(
        vault
            .get_meta(META_SYNC_INTEGRATED_SEQ)
            .expect("integrated meta")
            .is_none(),
        "失败路径不得推进 integrated"
    );
    assert!(
        vault
            .get_meta(META_SYNC_CONFIRMED_HIGHWATER)
            .expect("highwater meta")
            .is_none(),
        "失败路径不得推进 highwater"
    );
}

fn assert_staging_journal(vault: &menghunote_storage::Vault) -> [u8; 16] {
    let journal = baseline_apply_state(vault)
        .expect("journal state")
        .expect("staging journal 必须保留，供续跑");
    assert_eq!(journal.status, BaselineApplyStatus::Staging);
    journal.checkpoint_id
}

#[test]
fn bootstrap_restores_all_docs_and_assets() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_source(&source, &mut relay, &materials);
    assert_eq!(relay.published_checkpoints.len(), 2);

    let (_target_dir, target) = fresh_vault();
    let report = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect("bootstrap");
    assert_eq!(
        report.covered_seq,
        relay.published_checkpoints[1].covered_seq
    );
    assert!(!report.used_fallback);
    assert!(report.docs.iter().any(|(doc_id, _)| doc_id == "note-1"));
    assert!(report
        .docs
        .iter()
        .any(|(doc_id, _)| doc_id == "orphan-body"));
    assert_eq!(report.assets, 1);
    let note1 = load_body_doc(&target, "note-1", materials.peer_id_u64(), None)
        .expect("note-1")
        .0;
    assert_eq!(note1.text(), "第一行正文");
    let orphan = load_body_doc(&target, "orphan-body", materials.peer_id_u64(), None)
        .expect("orphan")
        .0;
    assert_eq!(orphan.text(), "orphan text");
    assert!(body_doc_meta(&target, "note-2").expect("meta").is_some());
    let asset_hash: [u8; 32] = Sha256::digest(ASSET_DATA).into();
    let asset = asset_bytes(&target, asset_hash).expect("asset_bytes");
    assert_eq!(asset.as_deref(), Some(ASSET_DATA));
    // 水位/cursor 已写入：engine 之后从 S 继续，不会重复导入。
    assert_eq!(
        target
            .get_meta(RELAY_CURSOR_META)
            .expect("cursor meta")
            .as_deref(),
        Some(report.cursor.as_bytes())
    );
    assert_eq!(
        target
            .get_meta(META_SYNC_RECEIVED_SEQ)
            .expect("received")
            .as_deref(),
        Some(report.covered_seq.to_be_bytes().as_slice())
    );
    assert_eq!(
        target
            .get_meta(META_SYNC_INTEGRATED_SEQ)
            .expect("integrated")
            .as_deref(),
        Some(report.covered_seq.to_be_bytes().as_slice())
    );
    assert_eq!(
        target
            .get_meta(META_SYNC_CONFIRMED_HIGHWATER)
            .expect("highwater")
            .as_deref(),
        Some(report.covered_seq.to_be_bytes().as_slice())
    );
    // staging journal 成功后必须清掉。
    assert!(baseline_apply_state(&target).expect("journal").is_none());
}

#[test]
fn structured_source_imports_structure_and_projects_notes() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    let body_doc_id = seed_structured_source(&source, &mut relay, &materials);

    let (_target_dir, target) = fresh_vault();
    let report = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect("bootstrap structured");
    assert_eq!(report.covered_seq, relay.head_seq);
    assert_eq!(report.cursor, format!("c{}", relay.head_seq));
    // structure 文档在报告里没有正文文本。
    assert!(report
        .docs
        .iter()
        .any(|(doc_id, text)| doc_id == STRUCTURE_DOC_ID && text.is_none()));
    let notes = list_notes(&target).expect("list_notes");
    assert_eq!(notes.len(), 1, "结构投影必须建出 1 篇笔记");
    let body = load_body_doc(&target, &body_doc_id, materials.peer_id_u64(), None)
        .expect("结构化正文")
        .0;
    assert_eq!(body.text(), STRUCTURED_TEXT);
}

#[test]
fn crash_after_bodies_resumes_without_advancing_watermarks() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    let body_doc_id = seed_structured_source(&source, &mut relay, &materials);
    let s = relay.head_seq;
    assert!(s > 0, "本测试要求 baseline S>0 才能区分是否推进水位");

    let (_target_dir, target) = fresh_vault();
    let error = bootstrap::run_with_fault(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
        BootstrapFault::AfterBodies,
    )
    .expect_err("注入故障必须失败");
    assert!(
        format!("{error}").contains("注入故障"),
        "错误应来自注入点：{error}"
    );

    // 正文已经落库（可续跑），但结构/资产/水位都没有。
    let body = load_body_doc(&target, &body_doc_id, materials.peer_id_u64(), None)
        .expect("正文已导入")
        .0;
    assert_eq!(body.text(), STRUCTURED_TEXT);
    assert!(list_notes(&target).expect("list_notes").is_empty());
    assert_no_sync_watermarks(&target);
    let journal_checkpoint = assert_staging_journal(&target);
    assert_eq!(
        journal_checkpoint,
        relay
            .published_checkpoints
            .last()
            .expect("primary checkpoint")
            .checkpoint_id
    );

    // 续跑：同一基线幂等重放，完成后水位才推进。
    let report = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect("续跑成功");
    assert_eq!(report.covered_seq, s);
    assert_eq!(
        target
            .get_meta(META_SYNC_RECEIVED_SEQ)
            .expect("received")
            .as_deref(),
        Some(s.to_be_bytes().as_slice())
    );
    assert_eq!(
        target
            .get_meta(META_SYNC_INTEGRATED_SEQ)
            .expect("integrated")
            .as_deref(),
        Some(s.to_be_bytes().as_slice())
    );
    assert_eq!(
        target
            .get_meta(RELAY_CURSOR_META)
            .expect("cursor")
            .as_deref(),
        Some(report.cursor.as_bytes())
    );
    assert_eq!(list_notes(&target).expect("list_notes").len(), 1);
    assert!(baseline_apply_state(&target).expect("journal").is_none());
}

#[test]
fn structure_projection_failure_keeps_cursor_and_resumes() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_structured_source(&source, &mut relay, &materials);
    let s = relay.head_seq;

    let (_target_dir, target) = fresh_vault();
    bootstrap::run_with_fault(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
        BootstrapFault::BeforeStructureProjection,
    )
    .expect_err("结构投影前崩溃必须失败");

    // structure 文档已写入，但投影还没发生：notes 仍为空，水位未推进。
    assert!(
        body_doc_meta(&target, STRUCTURE_DOC_ID)
            .expect("structure meta")
            .is_some(),
        "structure 文档应已落库"
    );
    assert!(list_notes(&target).expect("list_notes").is_empty());
    assert_no_sync_watermarks(&target);
    assert_staging_journal(&target);

    // 续跑补齐投影与水位。
    let report = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect("续跑成功");
    assert_eq!(report.covered_seq, s);
    assert_eq!(list_notes(&target).expect("list_notes").len(), 1);
    assert_eq!(
        target
            .get_meta(META_SYNC_CONFIRMED_HIGHWATER)
            .expect("highwater")
            .as_deref(),
        Some(s.to_be_bytes().as_slice())
    );
}

#[test]
fn asset_write_conflict_keeps_cursor_and_resumes_after_fix() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_source(&source, &mut relay, &materials);
    flush_pending_and_checkpoint(&source, &mut relay, &materials);
    let s = relay.head_seq;
    assert!(s > 0);

    let (_target_dir, target) = fresh_vault();
    // 预置同 asset_hash、同长度但内容不同的资产行：apply_baseline_assets 必须冲突。
    let asset_hash: [u8; 32] = Sha256::digest(ASSET_DATA).into();
    let mut wrong = ASSET_DATA.to_vec();
    wrong[0] ^= 0xFF;
    target
        .exec(
            "INSERT INTO assets(asset_hash,byte_len,media_type,data,orphan_since) \
             VALUES(?1,?2,?3,?4,NULL)",
            &[
                Value::Blob(asset_hash.to_vec()),
                Value::Integer(ASSET_DATA.len() as i64),
                Value::Text("text/plain".to_owned()),
                Value::Blob(wrong),
            ],
        )
        .expect("预置冲突资产");

    let error = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect_err("资产写失败必须冒泡，不能用半库换基线");
    assert!(
        format!("{error}").contains("资产"),
        "错误应指向资产冲突：{error}"
    );
    assert_no_sync_watermarks(&target);
    assert_staging_journal(&target);
    // 已导入的正文仍在（可续跑），冲突资产未被覆盖。
    assert_eq!(
        load_body_doc(&target, "note-1", materials.peer_id_u64(), None)
            .expect("正文已导入")
            .0
            .text(),
        "第一行正文"
    );
    let stored = asset_bytes(&target, asset_hash).expect("asset_bytes");
    assert_ne!(stored.as_deref(), Some(ASSET_DATA));

    // 修掉冲突后同一 journal 续跑成功。
    target
        .exec(
            "DELETE FROM assets WHERE asset_hash=?1",
            &[Value::Blob(asset_hash.to_vec())],
        )
        .expect("删除冲突资产");
    let report = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect("修复后续跑");
    assert_eq!(report.covered_seq, s);
    let stored = asset_bytes(&target, asset_hash).expect("asset_bytes");
    assert_eq!(stored.as_deref(), Some(ASSET_DATA));
    assert_eq!(
        target
            .get_meta(META_SYNC_RECEIVED_SEQ)
            .expect("received")
            .as_deref(),
        Some(s.to_be_bytes().as_slice())
    );
    assert!(baseline_apply_state(&target).expect("journal").is_none());
}

#[test]
fn crash_after_assets_resumes_without_duplicate_writes() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_structured_source(&source, &mut relay, &materials);
    let s = relay.head_seq;
    let asset_hash: [u8; 32] = Sha256::digest(ASSET_DATA).into();

    let (_target_dir, target) = fresh_vault();
    bootstrap::run_with_fault(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
        BootstrapFault::AfterAssets,
    )
    .expect_err("资产写完后崩溃必须失败，水位不得推进");
    // 资产与结构都已落库，但水位仍未写；journal 记录 applied_assets 供续跑。
    assert_eq!(
        asset_bytes(&target, asset_hash)
            .expect("asset_bytes")
            .as_deref(),
        Some(ASSET_DATA)
    );
    assert_eq!(list_notes(&target).expect("list_notes").len(), 1);
    assert_no_sync_watermarks(&target);
    let journal = baseline_apply_state(&target)
        .expect("journal state")
        .expect("journal 必须保留");
    assert_eq!(journal.applied_assets, 1);

    // 续跑：同资产幂等（不重复插入、不再要求不同明文），水位才推进。
    let report = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect("资产阶段崩溃后续跑");
    assert_eq!(report.covered_seq, s);
    assert_eq!(report.assets, 1);
    assert_eq!(
        asset_bytes(&target, asset_hash)
            .expect("asset_bytes")
            .as_deref(),
        Some(ASSET_DATA)
    );
    assert_eq!(
        target
            .get_meta(META_SYNC_CONFIRMED_HIGHWATER)
            .expect("highwater")
            .as_deref(),
        Some(s.to_be_bytes().as_slice())
    );
    assert!(baseline_apply_state(&target).expect("journal").is_none());
}

#[test]
fn verify_failure_keeps_merged_data_and_marks_journal_failed() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_source(&source, &mut relay, &materials);

    let (_target_dir, target) = fresh_vault();
    // 目标库已有同一篇正文的本地编辑：bootstrap 必须把它与基线 merge（不是覆盖）。
    let local_peer = materials.peer_id_u64().wrapping_add(1);
    let local = seed_doc(&target, "note-1", local_peer);
    local.insert_scalars(0, "本地优先编辑 ").expect("本地正文");
    commit_local(&target, &local);

    let error = bootstrap::run_with_fault(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
        BootstrapFault::VerifyVvNotCovered,
    )
    .expect_err("注入的读回校验失败必须冒泡");
    assert!(
        format!("{error}").contains("VV 未覆盖快照"),
        "错误应来自读回校验：{error}"
    );

    // 已 merge 的 Loro 数据保留：本地编辑与基线正文都在同一条文档里。
    let merged = load_body_doc(&target, "note-1", local_peer, None)
        .expect("merged doc")
        .0;
    assert!(
        merged.text().contains("本地优先编辑") && merged.text().contains("第一行正文"),
        "失败不得回滚已 merge 的数据，实际={:?}",
        merged.text()
    );

    // 水位没有推进；journal 仍是 staging（schema CHECK 只有 staging/completed），
    // 失败语义由 meta 失败标记承载。
    assert_no_sync_watermarks(&target);
    let journal = baseline_apply_state(&target)
        .expect("journal state")
        .expect("失败后 journal 必须保留");
    assert_eq!(journal.status, BaselineApplyStatus::Staging);
    assert!(journal.applied_docs >= 2, "已应用批次计数应保留");
    let reason = baseline_verify_failure(&target)
        .expect("读失败标记")
        .expect("校验失败必须写失败标记");
    assert!(
        reason.contains("VV 未覆盖快照"),
        "标记应保留失败原因：{reason}"
    );

    // 未 abort 前 begin 一律拒绝（同 checkpoint 幂等续跑也不行）。
    let retry = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect_err("失败标记必须阻止 begin 续跑");
    assert!(
        format!("{retry}").contains("abort_baseline_apply"),
        "错误必须提示先 abort：{retry}"
    );

    // abort 清 journal + 失败标记，但保留已 merge 的 Loro 数据。
    assert!(abort_baseline_apply(&target).expect("abort"));
    assert!(baseline_apply_state(&target).expect("state").is_none());
    assert!(baseline_verify_failure(&target).expect("标记").is_none());
    let kept = load_body_doc(&target, "note-1", local_peer, None)
        .expect("abort 后正文")
        .0;
    assert!(
        kept.text().contains("本地优先编辑") && kept.text().contains("第一行正文"),
        "abort 只清 journal，不得删已 merge 的 Loro 数据，实际={:?}",
        kept.text()
    );

    // abort 后可以重新 begin 并成功（失败标记已清）。
    let report = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect("abort 后重跑");
    assert_eq!(
        report.covered_seq,
        relay.published_checkpoints[1].covered_seq
    );
    assert!(report.docs.iter().any(|(doc_id, _)| doc_id == "note-1"));
    assert!(baseline_apply_state(&target).expect("state").is_none());
}

#[test]
fn cursor_cas_conflict_rolls_back_all_watermarks_and_retry_succeeds() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_source(&source, &mut relay, &materials);
    flush_pending_and_checkpoint(&source, &mut relay, &materials);
    let s = relay.head_seq;
    assert!(s > 0);

    let (_target_dir, target) = fresh_vault();
    let report = bootstrap::run_with_fault(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
        BootstrapFault::CursorCasConflict,
    );
    assert!(report.is_err(), "游标 CAS 冲突必须失败");
    // 事务整笔回滚：并发写者留下的游标保留，其余单调值一个都没写。
    assert_eq!(
        target
            .get_meta(RELAY_CURSOR_META)
            .expect("cursor")
            .as_deref(),
        Some(b"fault-cursor".as_slice())
    );
    assert!(target
        .get_meta(META_SYNC_RECEIVED_SEQ)
        .expect("received")
        .is_none());
    assert!(target
        .get_meta(META_SYNC_INTEGRATED_SEQ)
        .expect("integrated")
        .is_none());
    assert!(target
        .get_meta(META_SYNC_CONFIRMED_HIGHWATER)
        .expect("highwater")
        .is_none());
    assert_staging_journal(&target);

    // 重试：从当前游标 CAS 前进，单调且幂等。
    let report = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect("重试成功");
    assert_eq!(report.covered_seq, s);
    assert_eq!(
        target
            .get_meta(RELAY_CURSOR_META)
            .expect("cursor")
            .as_deref(),
        Some(report.cursor.as_bytes())
    );
    assert_eq!(
        target
            .get_meta(META_SYNC_CONFIRMED_HIGHWATER)
            .expect("highwater")
            .as_deref(),
        Some(s.to_be_bytes().as_slice())
    );
    assert!(baseline_apply_state(&target).expect("journal").is_none());
}

#[test]
fn existing_pins_and_highwater_are_not_lowered() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_source(&source, &mut relay, &materials);
    flush_pending_and_checkpoint(&source, &mut relay, &materials);
    // 让 primary 的 manifest 腐坏，强制选 fallback（fallback 的 S=0 < 本地水位 1）。
    let primary_manifest = relay
        .published_checkpoints
        .last()
        .and_then(|checkpoint| checkpoint.manifest_ref)
        .expect("primary manifest");
    relay
        .parts
        .get_mut(&(primary_manifest, 0))
        .expect("primary manifest part")[0] ^= 0x01;
    let (instance_id, control_hash) = relay_state_and_control_hash(&mut relay);

    let (_target_dir, target) = fresh_vault();
    // 已有库：一篇本地编辑过的正文 + 已确认的水位/游标/pins。
    let local_peer = materials.peer_id_u64().wrapping_add(1);
    let local = seed_doc(&target, "note-1", local_peer);
    local.insert_scalars(0, "local edit ").expect("本地正文");
    commit_local(&target, &local);
    target
        .set_meta(menghunote_storage::META_SYNC_LOG_ID, &relay.log_id)
        .expect("log id");
    target.set_meta(RELAY_CURSOR_META, b"c2").expect("cursor");
    set_u64_meta(&target, META_SYNC_RECEIVED_SEQ, 2);
    set_u64_meta(&target, META_SYNC_INTEGRATED_SEQ, 2);
    set_u64_meta(&target, META_SYNC_CONFIRMED_HIGHWATER, 2);
    target
        .set_meta(META_BOOTSTRAP_INSTANCE, &instance_id)
        .expect("instance pin");
    set_u64_meta(&target, META_BOOTSTRAP_EPOCH, EPOCH);
    set_u64_meta(&target, META_BOOTSTRAP_CONTROL_REV, relay.control_rev);
    target
        .set_meta(META_BOOTSTRAP_CONTROL_HASH, &control_hash)
        .expect("control hash pin");

    let report = bootstrap::run(&target, &mut relay, VAULT_ID, local_peer, &materials.root)
        .expect("已有库 bootstrap 必须成功");
    assert!(report.used_fallback);
    assert_eq!(report.covered_seq, 1, "fallback 必须用自己的 S");
    assert_eq!(report.cursor, "c1");

    // 单调值全部保持原值，绝不被 baseline S 降级。
    assert_eq!(
        target
            .get_meta(RELAY_CURSOR_META)
            .expect("cursor")
            .as_deref(),
        Some(b"c2".as_slice())
    );
    assert_eq!(
        target
            .get_meta(META_SYNC_RECEIVED_SEQ)
            .expect("received")
            .as_deref(),
        Some(2u64.to_be_bytes().as_slice())
    );
    assert_eq!(
        target
            .get_meta(META_SYNC_INTEGRATED_SEQ)
            .expect("integrated")
            .as_deref(),
        Some(2u64.to_be_bytes().as_slice())
    );
    assert_eq!(
        target
            .get_meta(META_SYNC_CONFIRMED_HIGHWATER)
            .expect("highwater")
            .as_deref(),
        Some(2u64.to_be_bytes().as_slice())
    );
    // pins 不变。
    assert_eq!(
        target
            .get_meta(META_BOOTSTRAP_INSTANCE)
            .expect("instance pin")
            .as_deref(),
        Some(instance_id.as_slice())
    );
    assert_eq!(
        target
            .get_meta(META_BOOTSTRAP_CONTROL_HASH)
            .expect("control hash pin")
            .as_deref(),
        Some(control_hash.as_slice())
    );
    // 已有本地编辑按 VV 合并，未被跳过/覆盖。
    let merged = load_body_doc(&target, "note-1", local_peer, None)
        .expect("note-1")
        .0;
    assert!(
        merged.text().contains("local edit") && merged.text().contains("第一行正文"),
        "已有 doc 必须按 VV 合并，实际={:?}",
        merged.text()
    );
}

#[test]
fn pin_regressions_reject_before_any_write() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_source(&source, &mut relay, &materials);
    let (instance_id, control_hash) = relay_state_and_control_hash(&mut relay);
    let state = relay.state().expect("state");
    let correct = BootstrapPins {
        instance_id: Some(instance_id),
        log_id: Some(relay.log_id),
        epoch: Some(EPOCH),
        control_rev: Some(relay.control_rev),
        control_hash: Some(control_hash),
        highwater: Some(0),
    };
    let regressions = [
        (
            "instance",
            BootstrapPins {
                instance_id: Some([0x99; 16]),
                ..correct
            },
        ),
        (
            "log_id",
            BootstrapPins {
                log_id: Some([0x99; 16]),
                ..correct
            },
        ),
        (
            "epoch",
            BootstrapPins {
                epoch: Some(EPOCH + 1),
                ..correct
            },
        ),
        (
            "control rev",
            BootstrapPins {
                control_rev: Some(relay.control_rev + 1),
                ..correct
            },
        ),
        (
            "control hash",
            BootstrapPins {
                control_hash: Some([0x99; 32]),
                ..correct
            },
        ),
        (
            "highwater",
            BootstrapPins {
                highwater: Some(state.head_seq + 1),
                ..correct
            },
        ),
    ];
    for (name, pins) in regressions {
        let (_target_dir, target) = fresh_vault();
        let error = bootstrap::run_with_pins(
            &target,
            &mut relay,
            VAULT_ID,
            materials.peer_id_u64(),
            &materials.root,
            &pins,
        )
        .expect_err(&format!("pins {name} 回退必须拒绝"));
        assert!(!format!("{error}").is_empty());
        assert!(
            list_all_docs(&target).expect("list_all_docs").is_empty(),
            "pins {name} 回退时不得写半库"
        );
        assert_no_sync_watermarks(&target);
        assert!(
            baseline_apply_state(&target).expect("journal").is_none(),
            "校验失败不得创建 journal"
        );
    }

    // 正确 pins 必须通过，并把 pins 持久化。
    let (_target_dir, target) = fresh_vault();
    bootstrap::run_with_pins(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
        &correct,
    )
    .expect("正确 pins 应通过");
    assert_eq!(
        target
            .get_meta(META_BOOTSTRAP_INSTANCE)
            .expect("instance pin")
            .as_deref(),
        Some(instance_id.as_slice())
    );
}

#[test]
fn control_pin_advances_but_never_rolls_back() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_source(&source, &mut relay, &materials);
    // 真实 relay 的 `/control` 键 5 必然从 rev1 genesis 起；历史锚按"截断到锚 rev 的
    // 管理链前缀"复算，空链无法复算视图 → 按 ADR 0013 §3 只能 fail-closed。
    relay.manager_chain = vec![menghunote_sync::relay::ManagerChainEntry {
        rev: 1,
        pubkey: materials.manager.public_key(),
        transition: b"test-genesis".to_vec(),
        old_rev: 1,
        old_signature: None,
        new_signature: None,
    }];

    let (_target_dir, target) = fresh_vault();
    bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect("首次 bootstrap");
    let first_rev = relay.control_rev;
    assert_eq!(
        target
            .get_meta(META_BOOTSTRAP_CONTROL_REV)
            .expect("control rev")
            .as_deref(),
        Some(first_rev.to_be_bytes().as_slice())
    );
    let first_hash = target
        .get_meta(META_BOOTSTRAP_CONTROL_HASH)
        .expect("control hash")
        .expect("hash pin");

    // control_rev 前进：允许并刷新 pin。
    relay.control_rev = first_rev + 1;
    bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect("control_rev 前进必须允许");
    assert_eq!(
        target
            .get_meta(META_BOOTSTRAP_CONTROL_REV)
            .expect("control rev")
            .as_deref(),
        Some((first_rev + 1).to_be_bytes().as_slice())
    );
    let second_hash = target
        .get_meta(META_BOOTSTRAP_CONTROL_HASH)
        .expect("control hash")
        .expect("hash pin");
    assert_ne!(first_hash, second_hash, "rev 前进时控制头摘要必须更新");

    // control_rev 回退：必须 fail-stop，且不改任何本地状态。
    relay.control_rev = first_rev;
    let error = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect_err("control_rev 回退必须拒绝");
    assert!(!format!("{error}").is_empty());
    assert_eq!(
        target
            .get_meta(META_BOOTSTRAP_CONTROL_REV)
            .expect("control rev")
            .as_deref(),
        Some((first_rev + 1).to_be_bytes().as_slice())
    );
    assert!(load_body_doc(&target, "note-1", materials.peer_id_u64(), None).is_ok());
}

#[test]
fn catchup_prepare_writes_nothing_and_integrate_advances_cursor() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_source(&source, &mut relay, &materials);
    let checkpoint = relay.published_checkpoints[1].clone();

    let (_target_dir, target) = fresh_vault();
    ensure_structure_row(&target).expect("structure 行");
    // 目标已有本地编辑 + 旧游标/水位：追赶不得丢本地 op，成功后 cursor 必须前移。
    let local = seed_doc(&target, "local-note", materials.peer_id_u64());
    create_note_slot(
        &target,
        "local-note",
        "local-note",
        "local-note",
        1_700_000_000,
    )
    .expect("notes 行");
    local.insert_scalars(0, "本地未上传").expect("正文");
    commit_local(&target, &local);
    target
        .set_meta(RELAY_CURSOR_META, b"c0")
        .expect("旧 cursor");
    target
        .set_meta(META_SYNC_RECEIVED_SEQ, &1u64.to_be_bytes())
        .expect("旧 received");
    target
        .set_meta(META_SYNC_INTEGRATED_SEQ, &1u64.to_be_bytes())
        .expect("旧 integrated");
    target
        .set_meta(menghunote_storage::META_SYNC_LOG_ID, &relay.log_id)
        .expect("log_id");

    let pins = BootstrapPins::from_vault(&target).expect("pins");
    let prepared = bootstrap::prepare(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
        &pins,
    )
    .expect("prepare");
    assert_eq!(prepared.covered_seq(), checkpoint.covered_seq);
    assert_eq!(
        target
            .get_meta(RELAY_CURSOR_META)
            .expect("cursor")
            .as_deref(),
        Some(b"c0".as_slice()),
        "prepare 不得写库/推进 cursor"
    );
    assert_eq!(
        target
            .get_meta(META_SYNC_INTEGRATED_SEQ)
            .expect("integrated")
            .as_deref(),
        Some(1u64.to_be_bytes().as_slice()),
        "prepare 不得推进 integrated"
    );

    let expected_cursor = prepared.checkpoint_cursor().to_owned();
    let report = bootstrap::integrate(
        &target,
        materials.peer_id_u64(),
        &prepared,
        bootstrap::BootstrapAdvancement::CatchUp,
        None,
    )
    .expect("integrate");
    assert_eq!(report.covered_seq, checkpoint.covered_seq);
    assert_eq!(
        target
            .get_meta(RELAY_CURSOR_META)
            .expect("cursor")
            .as_deref(),
        Some(expected_cursor.as_bytes()),
        "CatchUp 成功后 cursor 必须前移到实际选中 checkpoint"
    );
    let cursor = sync_cursor_state(&target).expect("cursor state");
    assert!(cursor.integrated_seq >= checkpoint.covered_seq);
    let local_text = load_body_doc(&target, "local-note", materials.peer_id_u64(), None)
        .expect("本地文档")
        .0
        .text();
    assert!(
        local_text.contains("本地未上传"),
        "本地未上传 op 不得丢：{local_text}"
    );

    // S 之后普通 pull：源在 checkpoint 后新增一条，目标用追赶后的 cursor 正常增量拉取。
    let (source_doc, _) =
        load_body_doc(&source, "note-1", materials.peer_id_u64(), None).expect("source note");
    source_doc
        .insert_scalars(0, "after-catchup ")
        .expect("source edit");
    commit_with_intent(&source, &source_doc, &uuid_like(9_777), [0xC7; 32], EPOCH);
    {
        let mut engine = SyncEngine::new(
            &source,
            &mut relay,
            EngineIdentity {
                device_id: materials.device_id,
                signing_key: materials.signing_key.clone(),
                peer_id: materials.peer_id_u64(),
            },
            materials.root.clone(),
        );
        engine.sync_once().expect("source push after catchup");
    }
    {
        let mut engine = SyncEngine::new(
            &target,
            &mut relay,
            EngineIdentity {
                device_id: materials.device_id,
                signing_key: materials.signing_key.clone(),
                peer_id: materials.peer_id_u64(),
            },
            materials.root.clone(),
        );
        engine.sync_once().expect("target pull after catchup");
    }
    let target_note = load_body_doc(&target, "note-1", materials.peer_id_u64(), None)
        .expect("target note")
        .0
        .text();
    assert!(
        target_note.contains("after-catchup"),
        "追赶后必须能从 S 继续普通 pull：{target_note}"
    );
}

#[test]
fn catchup_mandatory_prepare_and_integrate_advances_through_public_wrapper() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_source(&source, &mut relay, &materials);
    let checkpoint = relay.published_checkpoints[1].clone();

    let (_target_dir, target) = fresh_vault();
    ensure_structure_row(&target).expect("structure 行");
    target
        .set_meta(RELAY_CURSOR_META, b"c0")
        .expect("旧 cursor");
    target
        .set_meta(META_SYNC_RECEIVED_SEQ, &0u64.to_be_bytes())
        .expect("旧 received");
    target
        .set_meta(META_SYNC_INTEGRATED_SEQ, &0u64.to_be_bytes())
        .expect("旧 integrated");
    target
        .set_meta(menghunote_storage::META_SYNC_LOG_ID, &relay.log_id)
        .expect("log_id");
    // 让普通 pull 必然 CursorExpired：本地 received=0，relay floor=1。
    relay.gc_floor = 1;
    let pins = BootstrapPins::from_vault(&target).expect("pins");
    let prepared = menghunote_sync::catchup::prepare(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
        &pins,
        &menghunote_sync::catchup::CatchUpConfig::default(),
    )
    .expect("mandatory prepare")
    .expect("gc_floor > received 必须进入 mandatory");
    assert_eq!(
        prepared.decision().mode,
        menghunote_sync::catchup::CatchUpMode::Mandatory
    );
    assert_eq!(prepared.checkpoint_s(), checkpoint.covered_seq);
    let expected_cursor = prepared.checkpoint_cursor().to_owned();
    let report = menghunote_sync::catchup::integrate(&target, materials.peer_id_u64(), &prepared)
        .expect("mandatory integrate");
    assert_eq!(
        report.mode,
        menghunote_sync::catchup::CatchUpMode::Mandatory
    );
    assert_eq!(
        target
            .get_meta(RELAY_CURSOR_META)
            .expect("cursor")
            .as_deref(),
        Some(expected_cursor.as_bytes())
    );
}

#[test]
fn bootstrap_cursor_pulls_updates_after_s() {
    let (_dir, source, materials) = setup();
    let (_target_dir, target) = fresh_vault();
    let mut relay = new_relay(&materials);
    seed_source(&source, &mut relay, &materials);
    flush_pending_and_checkpoint(&source, &mut relay, &materials);
    let s = relay
        .published_checkpoints
        .last()
        .expect("checkpoint")
        .covered_seq;
    assert_eq!(relay.head_seq, s, "checkpoint 之后不应再有日志事件");

    // 源在 checkpoint 之后给已有 note-1 追加 1 条 update 并推到 relay。
    let (doc, _) = load_body_doc(&source, "note-1", materials.peer_id_u64(), None).expect("note-1");
    doc.insert_scalars(0, "after checkpoint ").expect("正文");
    commit_with_intent(&source, &doc, &uuid_like(990), [0xB1; 32], EPOCH);
    {
        let mut engine = SyncEngine::new(
            &source,
            &mut relay,
            EngineIdentity {
                device_id: materials.device_id,
                signing_key: materials.signing_key.clone(),
                peer_id: materials.peer_id_u64(),
            },
            materials.root.clone(),
        );
        let pushed = engine.sync_once().expect("source push after checkpoint");
        assert_eq!(pushed.push.uploaded, 1);
    }
    assert!(relay.head_seq > s);

    let report = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect("bootstrap at S");
    assert_eq!(report.covered_seq, s);
    {
        let mut engine = SyncEngine::new(
            &target,
            &mut relay,
            EngineIdentity {
                device_id: materials.device_id,
                signing_key: materials.signing_key.clone(),
                peer_id: materials.peer_id_u64(),
            },
            materials.root.clone(),
        );
        let pulled = engine.sync_once().expect("target catch up after S");
        assert!(pulled.pull.events >= 1, "必须拉到 S 之后的 update");
    }
    let (doc, _) = load_body_doc(&target, "note-1", materials.peer_id_u64(), None).expect("note-1");
    assert!(
        doc.text().contains("after checkpoint"),
        "S 之后的 update 必须被 integrate，实际={:?}",
        doc.text()
    );
}

#[test]
fn bootstrap_is_idempotent_on_second_run() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_source(&source, &mut relay, &materials);
    let (_target_dir, target) = fresh_vault();
    let first = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect("第一次 bootstrap");
    let second = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect("第二次 bootstrap");
    assert_eq!(first.covered_seq, second.covered_seq);
    assert_eq!(first.cursor, second.cursor);
    let (note1, _) =
        load_body_doc(&target, "note-1", materials.peer_id_u64(), None).expect("note-1");
    assert_eq!(note1.text(), "第一行正文");
}

#[test]
fn fallback_is_used_when_primary_manifest_is_tampered() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_source(&source, &mut relay, &materials);
    flush_pending_and_checkpoint(&source, &mut relay, &materials);
    let fallback = relay
        .published_checkpoints
        .get(relay.published_checkpoints.len() - 2)
        .expect("fallback checkpoint")
        .clone();
    let primary = relay
        .published_checkpoints
        .last()
        .and_then(|checkpoint| checkpoint.manifest_ref)
        .expect("primary manifest");
    assert_ne!(fallback.covered_seq, relay.head_seq);
    let bytes = relay.parts.get_mut(&(primary, 0)).expect("manifest part");
    bytes[0] ^= 0x01;

    let (_target_dir, target) = fresh_vault();
    let report = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect("fallback");
    assert!(report.used_fallback);
    assert_eq!(report.covered_seq, fallback.covered_seq);
    assert_eq!(report.cursor, format!("c{}", fallback.covered_seq));
    assert_eq!(
        target
            .get_meta(META_SYNC_RECEIVED_SEQ)
            .expect("received meta"),
        Some(fallback.covered_seq.to_be_bytes().to_vec())
    );
    assert_eq!(
        target
            .get_meta(META_SYNC_INTEGRATED_SEQ)
            .expect("integrated meta"),
        Some(fallback.covered_seq.to_be_bytes().to_vec())
    );
    assert!(load_body_doc(&target, "note-1", materials.peer_id_u64(), None).is_ok());
}

#[test]
fn fallback_is_used_when_primary_snapshot_is_tampered() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_source(&source, &mut relay, &materials);
    flush_pending_and_checkpoint(&source, &mut relay, &materials);
    let fallback = relay
        .published_checkpoints
        .get(relay.published_checkpoints.len() - 2)
        .expect("fallback checkpoint")
        .clone();
    let primary_snapshot = relay
        .published_checkpoints
        .last()
        .and_then(|checkpoint| checkpoint.data_refs.first().copied())
        .expect("primary snapshot");
    relay
        .parts
        .get_mut(&(primary_snapshot, 0))
        .expect("snapshot part")[0] ^= 0x01;

    let (_target_dir, target) = fresh_vault();
    let report = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect("fallback after snapshot tamper");
    assert!(report.used_fallback);
    assert_eq!(report.covered_seq, fallback.covered_seq);
    assert_eq!(report.cursor, format!("c{}", fallback.covered_seq));
}

#[test]
fn existing_local_doc_is_merged_not_skipped() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_source(&source, &mut relay, &materials);

    let (_target_dir, target) = fresh_vault();
    // 本机 peer 必须与快照生产者不同，否则同 peer 的 op 会被 Loro 当作重复丢弃。
    let local_peer = materials.peer_id_u64().wrapping_add(1);
    let local = seed_doc(&target, "note-1", local_peer);
    local.insert_scalars(0, "local edit ").expect("本地正文");
    commit_local(&target, &local);

    let report = bootstrap::run(&target, &mut relay, VAULT_ID, local_peer, &materials.root)
        .expect("bootstrap with existing doc");
    assert!(!report.used_fallback);
    let (merged, _) = load_body_doc(&target, "note-1", local_peer, None).expect("note-1");
    assert!(
        merged.text().contains("local edit") && merged.text().contains("第一行正文"),
        "已有 doc 必须按 VV 合并，实际={:?}",
        merged.text()
    );
}

#[test]
fn tampered_manifest_on_both_baselines_writes_nothing() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_source(&source, &mut relay, &materials);
    for checkpoint in &relay.published_checkpoints {
        if let Some(manifest) = checkpoint.manifest_ref {
            if let Some(bytes) = relay.parts.get_mut(&(manifest, 0)) {
                bytes[0] ^= 0x01;
            }
        }
    }
    let (_target_dir, target) = fresh_vault();
    let error = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect_err("篡改 manifest 必须失败");
    assert!(
        list_all_docs(&target).expect("list_all_docs").is_empty(),
        "校验失败前不得写半库: {error:?}"
    );
    assert!(baseline_apply_state(&target).expect("journal").is_none());
}

#[test]
fn tampered_snapshot_is_rejected_without_partial_import() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_source(&source, &mut relay, &materials);
    // 同时篡改两份基线的第一个 SNAPSHOT 对象，确保 fallback 也失败。
    for checkpoint in &relay.published_checkpoints {
        for object_id in &checkpoint.data_refs {
            if let Some(bytes) = relay.parts.get_mut(&(*object_id, 0)) {
                bytes[0] ^= 0x01;
            }
        }
    }
    let (_target_dir, target) = fresh_vault();
    let error = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect_err("篡改快照必须失败");
    assert!(
        list_all_docs(&target).expect("list_all_docs").is_empty(),
        "失败不得写半库: {error:?}"
    );
}

#[test]
fn wrong_root_rejects_bootstrap_before_write() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_source(&source, &mut relay, &materials);
    let wrong = RootSecretV1::new(
        VAULT_ID,
        EPOCH,
        menghunote_sync::keys::Secret32::from_array([0x5A; 32]),
    );
    let (_target_dir, target) = fresh_vault();
    assert!(bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &wrong
    )
    .is_err());
    assert!(list_all_docs(&target).expect("list_all_docs").is_empty());
}

#[test]
fn bootstrap_body_doc_head_is_one_full_update() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_source(&source, &mut relay, &materials);
    let (_target_dir, target) = fresh_vault();
    bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect("bootstrap");
    let rows = menghunote_storage::doc_updates_since(&target, "note-1", 0).expect("updates");
    assert_eq!(rows.len(), 1, "bootstrap 只写 1 条全量 update");
    assert_eq!(rows[0].seq, 1);
    // 不是 snapshot 行：snapshot_seq 必须保持 0（后续优化再补）。
    let meta = body_doc_meta(&target, "note-1")
        .expect("meta")
        .expect("row");
    assert_eq!(meta.head_seq, 1);
}

#[test]
fn empty_baseline_at_zero_is_idempotent() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    source.set_meta(RELAY_CURSOR_META, b"c0").expect("cursor");
    let mut writer = CheckpointWriter::new(
        &source,
        &mut relay,
        VAULT_ID,
        materials.device_id,
        materials.signing_key.clone(),
        materials.root.clone(),
        materials.peer_id_u64(),
    );
    let secrets = SlotSecrets {
        password: Some("pw"),
        recovery_seed: Some(&materials.recovery_seed),
    };
    writer
        .upload_initial_slots(&materials.password_slot, &materials.recovery_slot, &secrets)
        .expect("槽上传");
    writer
        .create_checkpoint(&StorageAssets)
        .expect("空库 checkpoint 1");
    writer
        .create_checkpoint(&StorageAssets)
        .expect("空库 checkpoint 2");
    assert_eq!(relay.head_seq, 0);

    let (_target_dir, target) = fresh_vault();
    let first = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect("空基线 bootstrap");
    assert_eq!(first.covered_seq, 0);
    assert_eq!(first.cursor, "c0");
    assert_eq!(
        sync_cursor_state(&target)
            .expect("cursor")
            .confirmed_highwater,
        Some(0)
    );
    assert!(body_doc_meta(&target, STRUCTURE_DOC_ID)
        .expect("structure meta")
        .is_some());
    assert!(list_notes(&target).expect("list_notes").is_empty());

    // S=0 的第二次 bootstrap 必须幂等（游标 c0 -> c0 的 CAS 也要通过）。
    let second = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect("空基线幂等重放");
    assert_eq!(first, second);
}

#[test]
fn cursor_metadata_reflects_written_watermarks() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_source(&source, &mut relay, &materials);
    let (_target_dir, target) = fresh_vault();
    let report = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect("bootstrap");
    let state = sync_cursor_state(&target).expect("cursor state");
    assert_eq!(state.received_seq, report.covered_seq);
    assert_eq!(state.integrated_seq, report.covered_seq);
    assert_eq!(state.confirmed_highwater, Some(report.covered_seq));
    assert_eq!(state.log_id, Some(relay.log_id));
}

// ============================================================ 评审 0002 T16/T17 补充
// primary 的 commit/signature/asset 各阶段腐坏 → 整体切 fallback，且只用 fallback 自己的 S/cursor；
// 两份都坏 → 失败且零写入/零水位（不能"两坏也 ACK"）。

fn tamper_checkpoint_commit(relay: &mut FakeRelay, index: usize) {
    let checkpoint = relay
        .published_checkpoints
        .get_mut(index)
        .expect("checkpoint");
    checkpoint.signed_commit.as_mut().expect("signed commit")[0] ^= 0x01;
}

fn tamper_checkpoint_signature(relay: &mut FakeRelay, index: usize) {
    let checkpoint = relay
        .published_checkpoints
        .get_mut(index)
        .expect("checkpoint");
    checkpoint.signature.as_mut().expect("signature")[0] ^= 0x01;
}

/// task-122：把已发布基线的 commit 改写成指定的验证修订（保持字段与签名自洽），
/// 用于证明客户端消费者会显式拒绝旧修订。
fn rewrite_checkpoint_validation_version(
    relay: &mut FakeRelay,
    materials: &genesis::GenesisMaterials,
    index: usize,
    version: u16,
) {
    let checkpoint = relay
        .published_checkpoints
        .get(index)
        .cloned()
        .expect("checkpoint");
    let signed = checkpoint.signed_commit.expect("signed commit");
    let mut commit =
        menghunote_sync_proto::CheckpointCommit::decode(&signed).expect("decode commit");
    commit.validation_version = version;
    commit.validation_digest = if version == 2 { [0x5A; 32] } else { [0u8; 32] };
    let signed = commit.encode().expect("encode commit");
    let signature = materials
        .signing_key
        .sign(&menghunote_sync_proto::checkpoint_signing_input(&commit).expect("signing input"));
    let entry = relay
        .published_checkpoints
        .get_mut(index)
        .expect("published checkpoint");
    entry.signed_commit = Some(signed);
    entry.signature = Some(signature);
}

fn checkpoint_asset_refs(relay: &FakeRelay, index: usize) -> Vec<[u8; 32]> {
    relay
        .published_checkpoints
        .get(index)
        .expect("checkpoint")
        .data_refs
        .iter()
        .filter(|object_id| {
            relay
                .objects
                .get(*object_id)
                .is_some_and(|object| object.commit.object_type == ObjectType::Asset)
        })
        .copied()
        .collect()
}

#[test]
fn fallback_uses_its_own_lower_s_and_cursor_after_primary_commit_corruption() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_source(&source, &mut relay, &materials);
    flush_pending_and_checkpoint(&source, &mut relay, &materials);

    let last = relay.published_checkpoints.len() - 1;
    let fallback = relay.published_checkpoints[last - 1].clone();
    let primary = relay.published_checkpoints[last].clone();
    assert!(
        fallback.covered_seq < primary.covered_seq,
        "前置：fallback S 必须低于 primary S（S0<S1），实际 {} vs {}",
        fallback.covered_seq,
        primary.covered_seq
    );

    // commit 字节腐坏：不是"自洽 hash 通过"就能信，必须整体切 fallback。
    tamper_checkpoint_commit(&mut relay, last);

    let (_target_dir, target) = fresh_vault();
    let report = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect("commit 腐坏后必须用 fallback");
    assert!(report.used_fallback);
    assert_eq!(report.covered_seq, fallback.covered_seq);
    assert_eq!(
        report.cursor,
        format!("c{}", fallback.covered_seq),
        "cursor 必须用 fallback 自己的 S"
    );
    assert_eq!(
        target.get_meta(META_SYNC_RECEIVED_SEQ).expect("received"),
        Some(fallback.covered_seq.to_be_bytes().to_vec())
    );
    assert_eq!(
        target
            .get_meta(META_SYNC_INTEGRATED_SEQ)
            .expect("integrated"),
        Some(fallback.covered_seq.to_be_bytes().to_vec())
    );
    assert!(target
        .get_meta(META_SYNC_CONFIRMED_HIGHWATER)
        .expect("highwater")
        .is_some());
    let (note, _) =
        load_body_doc(&target, "note-1", materials.peer_id_u64(), None).expect("note-1");
    assert!(note.text().contains("第一行正文"));
}

#[test]
fn corrupt_asset_on_primary_falls_back_and_both_corrupt_import_nothing() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_source(&source, &mut relay, &materials);
    // 在 checkpoint 2 之后再插入一个**只被 primary 引用**的资产，才能单独腐坏 primary。
    let extra_asset = b"primary-only asset";
    let extra_hash: [u8; 32] = Sha256::digest(extra_asset).into();
    insert_asset(
        &source,
        &extra_hash,
        extra_asset.len() as u64,
        Some("text/plain"),
        extra_asset,
    )
    .expect("primary-only 资产");
    flush_pending_and_checkpoint(&source, &mut relay, &materials);

    let last = relay.published_checkpoints.len() - 1;
    let fallback = relay.published_checkpoints[last - 1].clone();
    let fallback_refs = fallback.data_refs.clone();
    let primary_only_assets: Vec<[u8; 32]> = checkpoint_asset_refs(&relay, last)
        .into_iter()
        .filter(|object_id| !fallback_refs.contains(object_id))
        .collect();
    assert!(
        !primary_only_assets.is_empty(),
        "前置：primary 必须引用了 fallback 没有的 ASSET 对象"
    );
    // 只腐坏 primary 独有的 ASSET 密文：下载/摘要/AEAD 阶段失败 → 整体 fallback。
    relay
        .parts
        .get_mut(&(primary_only_assets[0], 0))
        .expect("asset part")[0] ^= 0x01;

    let (_target_dir, target) = fresh_vault();
    let report = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect("asset 腐坏必须用 fallback");
    assert!(report.used_fallback);
    assert_eq!(report.covered_seq, fallback.covered_seq);
    assert_eq!(report.cursor, format!("c{}", fallback.covered_seq));
    let asset_rows = target
        .query("SELECT COUNT(*) FROM assets", &[])
        .expect("asset count");
    assert_eq!(
        asset_rows[0][0],
        Value::Integer(1),
        "只允许导入 fallback 自己的资产，不得写入 primary 独有资产"
    );
    assert!(
        menghunote_storage::asset_meta_by_hash(&target, &extra_hash)
            .expect("asset meta")
            .is_none(),
        "primary 独有资产不得出现在 fallback 导入结果里"
    );

    // 两份基线的 ASSET 都腐坏：零写入、零水位，绝不 ACK。
    let fallback_index = last - 1;
    for object_id in checkpoint_asset_refs(&relay, fallback_index) {
        if let Some(part) = relay.parts.get_mut(&(object_id, 0)) {
            part[0] ^= 0x01;
        }
    }
    let (_second_dir, second_target) = fresh_vault();
    let error = bootstrap::run(
        &second_target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect_err("两份 asset 都坏必须失败");
    assert!(!format!("{error:?}").is_empty());
    assert!(
        list_all_docs(&second_target)
            .expect("list_all_docs")
            .is_empty(),
        "失败前不得写半库"
    );
    assert!(
        baseline_apply_state(&second_target)
            .expect("journal")
            .is_none(),
        "失败前不得建 staging journal"
    );
    assert_no_sync_watermarks(&second_target);
}

#[test]
fn corrupt_signature_on_both_baselines_fails_without_import_or_ack() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_source(&source, &mut relay, &materials);

    for index in 0..relay.published_checkpoints.len() {
        tamper_checkpoint_signature(&mut relay, index);
    }

    let (_target_dir, target) = fresh_vault();
    let error = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect_err("两份 checkpoint 签名都坏必须失败");
    assert!(!format!("{error:?}").is_empty());
    assert!(list_all_docs(&target).expect("list_all_docs").is_empty());
    assert!(baseline_apply_state(&target).expect("journal").is_none());
    assert_no_sync_watermarks(&target);
}

#[test]
fn revision_1_baselines_are_rejected_by_client_consumers() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_source(&source, &mut relay, &materials);
    assert!(relay.published_checkpoints.len() >= 2);
    for index in 0..relay.published_checkpoints.len() {
        rewrite_checkpoint_validation_version(&mut relay, &materials, index, 1);
    }

    let (_target_dir, target) = fresh_vault();
    let error = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect_err("修订 1 的签名基线必须被客户端拒绝（不得静默接受旧基线）");
    assert!(
        format!("{error:?}").contains("UnsupportedVersion")
            || format!("{error}").contains("协议版本"),
        "错误必须指向不支持的验证修订，实际：{error}"
    );
    assert!(list_all_docs(&target).expect("list_all_docs").is_empty());
    assert_no_sync_watermarks(&target);
}

// ============================================================ 评审 0002 T16/T17 剩余
// fallback 阶段仍必须真实验证 structure/codec/actual VV；任一基线腐坏即整体回退，
// 两基线都坏必须零写入、零水位、零 journal、零 ACK。

/// 用真实 root 重加密 manifest 并重签 checkpoint commit（模拟 prepare 产物被篡改）。
fn install_manifest(
    relay: &mut FakeRelay,
    root: &RootSecretV1,
    materials: &genesis::GenesisMaterials,
    index: usize,
    manifest: &menghunote_sync_proto::ManifestV1,
) {
    use menghunote_sync_proto::{
        checkpoint_signing_input, refs_digest_input, CheckpointCommit, ObjectRefV1, ObjectType,
    };
    let checkpoint = relay
        .published_checkpoints
        .get_mut(index)
        .expect("published checkpoint")
        .clone();
    let manifest_oid = checkpoint.manifest_ref.expect("manifest ref");
    let plaintext = manifest.encode().expect("manifest encode");
    let (parts, manifest_digest) = menghunote_sync::upload::encrypt_local_parts(
        root,
        ObjectType::Manifest,
        &manifest_oid,
        [0x42; 16],
        materials.device_id,
        0,
        0,
        &plaintext,
        &[],
    )
    .expect("reencrypt manifest");
    let old = relay
        .objects
        .get_mut(&manifest_oid)
        .expect("manifest object");
    for (part_no, envelope, _) in &parts {
        relay
            .parts
            .insert((manifest_oid, *part_no), envelope.clone());
    }
    old.commit.part_count = parts.len() as u32;
    old.commit.total_len = plaintext.len() as u64;
    old.commit.object_digest = manifest_digest;
    old.commit_bytes = old.commit.encode().expect("manifest commit");

    let refs: Vec<ObjectRefV1> = manifest
        .docs
        .iter()
        .map(|doc| ObjectRefV1 {
            object_id: doc.snapshot_object_id,
            object_type: ObjectType::Snapshot,
            generation: doc.generation,
            key_epoch: doc.key_epoch,
            part_count: doc.part_count,
            total_plain_len: doc.total_plain_len,
            object_digest: doc.object_digest,
        })
        .chain(manifest.assets.iter().map(|asset| ObjectRefV1 {
            object_id: asset.object_id,
            object_type: ObjectType::Asset,
            generation: 0,
            key_epoch: asset.key_epoch,
            part_count: asset.part_count,
            total_plain_len: asset.total_plain_len,
            object_digest: asset.object_digest,
        }))
        .collect();
    let refs_digest: [u8; 32] =
        Sha256::digest(refs_digest_input(&refs).expect("refs digest input")).into();
    let commit = CheckpointCommit {
        vault_id: VAULT_ID,
        checkpoint_id: checkpoint.checkpoint_id,
        epoch: checkpoint.epoch,
        covered_relay_seq: checkpoint.covered_seq,
        manifest_object_id: manifest_oid,
        manifest_digest,
        refs_digest,
        producer_device_id: materials.device_id,
        validation_version: 2,
        validation_digest: [0x5A; 32],
    };
    let signed_commit = commit.encode().expect("checkpoint commit");
    let signature = materials
        .signing_key
        .sign(&checkpoint_signing_input(&commit).expect("checkpoint signing input"));
    let entry = relay
        .published_checkpoints
        .get_mut(index)
        .expect("published checkpoint");
    entry.signed_commit = Some(signed_commit);
    entry.signature = Some(signature);
}

fn load_manifest(
    relay: &mut FakeRelay,
    root: &RootSecretV1,
    index: usize,
) -> menghunote_sync_proto::ManifestV1 {
    let checkpoint = relay.published_checkpoints[index].clone();
    let manifest_oid = checkpoint.manifest_ref.expect("manifest ref");
    let commit = relay.objects[&manifest_oid].commit.clone();
    let (_, bytes) = menghunote_sync::upload::fetch_object(
        relay,
        root,
        ObjectType::Manifest,
        &manifest_oid,
        commit.part_count,
        commit.total_len,
        &commit.object_digest,
        &[],
    )
    .expect("fetch manifest");
    menghunote_sync_proto::ManifestV1::decode(&bytes).expect("decode manifest")
}

/// 重写某个 baseline 的 structure（或第一个 body）快照并同步 manifest/commit。
fn rewrite_snapshot(
    relay: &mut FakeRelay,
    root: &RootSecretV1,
    materials: &genesis::GenesisMaterials,
    index: usize,
    kind: u64,
    mutate: impl FnOnce(&mut menghunote_sync_proto::SnapshotV1),
) {
    use menghunote_sync_proto::{ObjectType, SnapshotV1};
    let mut manifest = load_manifest(relay, root, index);
    let doc_index = manifest
        .docs
        .iter()
        .position(|doc| doc.kind == kind)
        .expect("目标 kind 文档");
    let doc = manifest.docs[doc_index].clone();
    let (_, bytes) = menghunote_sync::upload::fetch_object(
        relay,
        root,
        ObjectType::Snapshot,
        &doc.snapshot_object_id,
        doc.part_count,
        doc.total_plain_len,
        &doc.object_digest,
        &[],
    )
    .expect("fetch snapshot");
    let mut snapshot = SnapshotV1::decode(&bytes).expect("decode snapshot");
    mutate(&mut snapshot);
    let plaintext = snapshot.encode().expect("snapshot encode");
    let (parts, digest) = menghunote_sync::upload::encrypt_local_parts(
        root,
        ObjectType::Snapshot,
        &doc.snapshot_object_id,
        [0x43; 16],
        materials.device_id,
        doc.generation,
        0,
        &plaintext,
        &[],
    )
    .expect("reencrypt snapshot");
    {
        let object = relay
            .objects
            .get_mut(&doc.snapshot_object_id)
            .expect("snapshot object");
        for (part_no, envelope, _) in &parts {
            relay
                .parts
                .insert((doc.snapshot_object_id, *part_no), envelope.clone());
        }
        object.commit.part_count = parts.len() as u32;
        object.commit.total_len = plaintext.len() as u64;
        object.commit.object_digest = digest;
        object.commit_bytes = object.commit.encode().expect("snapshot commit");
    }
    manifest.docs[doc_index].part_count = parts.len() as u32;
    manifest.docs[doc_index].total_plain_len = plaintext.len() as u64;
    manifest.docs[doc_index].object_digest = digest;
    install_manifest(relay, root, materials, index, &manifest);
}

fn corrupt_structure_manifest(
    relay: &mut FakeRelay,
    root: &RootSecretV1,
    materials: &genesis::GenesisMaterials,
    index: usize,
) {
    let mut manifest = load_manifest(relay, root, index);
    manifest.structure_doc_id = "missing-structure".to_owned();
    install_manifest(relay, root, materials, index, &manifest);
}

fn corrupt_codec(
    relay: &mut FakeRelay,
    root: &RootSecretV1,
    materials: &genesis::GenesisMaterials,
    index: usize,
) {
    rewrite_snapshot(
        relay,
        root,
        materials,
        index,
        menghunote_sync::checkpoint::KIND_STRUCTURE,
        |snapshot| {
            snapshot.codec_version = "loro-v0".to_owned();
        },
    );
}

fn corrupt_actual_vv(
    relay: &mut FakeRelay,
    root: &RootSecretV1,
    materials: &genesis::GenesisMaterials,
    index: usize,
) {
    use menghunote_sync::checkpoint::KIND_STRUCTURE;
    let mut manifest = load_manifest(relay, root, index);
    let doc = manifest
        .docs
        .iter_mut()
        .find(|doc| doc.kind == KIND_STRUCTURE)
        .expect("structure doc");
    let mut vv = loro::VersionVector::decode(&doc.vv).expect("vv");
    vv.insert(0xDEAD, 7);
    doc.vv = vv.encode();
    install_manifest(relay, root, materials, index, &manifest);
}

fn run_bootstrap_with_target(
    relay: &mut FakeRelay,
    materials: &genesis::GenesisMaterials,
) -> (
    tempfile::TempDir,
    menghunote_storage::Vault,
    Result<menghunote_sync::bootstrap::BootstrapReport, menghunote_sync::SyncError>,
) {
    let (dir, target) = fresh_vault();
    let result = bootstrap::run(
        &target,
        relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    );
    (dir, target, result)
}

#[test]
fn fallback_rejects_corrupt_structure_and_both_corrupt_writes_nothing() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_structured_source(&source, &mut relay, &materials);
    let last = relay.published_checkpoints.len() - 1;

    // 只腐坏 primary 的 structure 声明 → 整体回退 fallback，且只写 fallback 的 S/cursor。
    corrupt_structure_manifest(&mut relay, &materials.root, &materials, last);
    let fallback_s = relay.published_checkpoints[last - 1].covered_seq;
    let (_target_dir, target, result) = run_bootstrap_with_target(&mut relay, &materials);
    let report = result.expect("structure 腐坏必须回退 fallback");
    assert!(
        report.used_fallback,
        "primary structure 腐坏必须 used_fallback"
    );
    assert_eq!(report.covered_seq, fallback_s);
    assert_eq!(report.cursor, format!("c{fallback_s}"));
    let notes = list_notes(&target).expect("notes");
    assert_eq!(notes.len(), 1, "fallback 必须恢复自己的结构笔记");
    let (body, _) = load_body_doc(
        &target,
        notes[0]
            .body_doc_id
            .as_deref()
            .expect("notes 行必须有正文 doc id"),
        materials.peer_id_u64(),
        None,
    )
    .expect("body");
    assert_eq!(body.text(), STRUCTURED_TEXT);

    // 两基线 structure 都坏 → 失败且零写入/零水位/零 journal。
    let acks_before = relay.acks.len();
    corrupt_structure_manifest(&mut relay, &materials.root, &materials, last - 1);
    let (_second_dir, second_target, result) = run_bootstrap_with_target(&mut relay, &materials);
    assert!(result.is_err(), "两基线 structure 都坏必须失败");
    assert_eq!(relay.acks.len(), acks_before, "失败不得 ACK");
    assert!(list_all_docs(&second_target).expect("docs").is_empty());
    assert!(baseline_apply_state(&second_target)
        .expect("journal")
        .is_none());
    assert_no_sync_watermarks(&second_target);
}

#[test]
fn fallback_rejects_unreadable_codec_and_both_corrupt_writes_nothing() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_structured_source(&source, &mut relay, &materials);
    let last = relay.published_checkpoints.len() - 1;

    corrupt_codec(&mut relay, &materials.root, &materials, last);
    let fallback_s = relay.published_checkpoints[last - 1].covered_seq;
    let (_target_dir, target, result) = run_bootstrap_with_target(&mut relay, &materials);
    let report = result.expect("codec 腐坏必须回退 fallback");
    assert!(report.used_fallback);
    assert_eq!(report.covered_seq, fallback_s);
    assert_eq!(report.cursor, format!("c{fallback_s}"));
    assert_eq!(list_notes(&target).expect("notes").len(), 1);

    let acks_before = relay.acks.len();
    corrupt_codec(&mut relay, &materials.root, &materials, last - 1);
    let (_second_dir, second_target, result) = run_bootstrap_with_target(&mut relay, &materials);
    assert!(result.is_err(), "两基线 codec 都坏必须失败");
    assert_eq!(relay.acks.len(), acks_before, "失败不得 ACK");
    assert!(list_all_docs(&second_target).expect("docs").is_empty());
    assert!(baseline_apply_state(&second_target)
        .expect("journal")
        .is_none());
    assert_no_sync_watermarks(&second_target);
}

#[test]
fn fallback_rejects_inflated_actual_vv_and_both_corrupt_writes_nothing() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_structured_source(&source, &mut relay, &materials);
    let last = relay.published_checkpoints.len() - 1;

    // manifest 宣称的 VV 高于快照真实历史：不能只信声明，必须整体回退。
    corrupt_actual_vv(&mut relay, &materials.root, &materials, last);
    let fallback_s = relay.published_checkpoints[last - 1].covered_seq;
    let (_target_dir, target, result) = run_bootstrap_with_target(&mut relay, &materials);
    let report = result.expect("actual VV 虚高必须回退 fallback");
    assert!(report.used_fallback);
    assert_eq!(report.covered_seq, fallback_s);
    assert_eq!(report.cursor, format!("c{fallback_s}"));
    assert_eq!(list_notes(&target).expect("notes").len(), 1);

    let acks_before = relay.acks.len();
    corrupt_actual_vv(&mut relay, &materials.root, &materials, last - 1);
    let (_second_dir, second_target, result) = run_bootstrap_with_target(&mut relay, &materials);
    assert!(result.is_err(), "两基线 actual VV 都虚高必须失败");
    assert_eq!(relay.acks.len(), acks_before, "失败不得 ACK");
    assert!(list_all_docs(&second_target).expect("docs").is_empty());
    assert!(baseline_apply_state(&second_target)
        .expect("journal")
        .is_none());
    assert_no_sync_watermarks(&second_target);
}

// ---------------------------------------------------------------------------
// ADR 0013 §3：bootstrap 历史 anchor（rev 前进不能丢、不能跳过）
// ---------------------------------------------------------------------------

/// 第一条 rev2 管理转换（`to_epoch = EPOCH + 1`）：比当前 epoch 更晚，属于"未来"。
fn future_manager_transition(instance_id: [u8; 16]) -> menghunote_sync::relay::ManagerChainEntry {
    let transition = menghunote_sync::rotation::ManagerTransitionV1 {
        vault_id: VAULT_ID,
        instance_id,
        rotation_id: [0x0b; 16],
        from_epoch: EPOCH,
        to_epoch: EPOCH + 1,
        old_rev: 1,
        new_rev: 2,
        new_pubkey: [0x02; 32],
        covered_seq: 0,
    }
    .encode()
    .expect("管理转换编码");
    menghunote_sync::relay::ManagerChainEntry {
        rev: 2,
        pubkey: [0x02; 32],
        transition,
        old_rev: 1,
        old_signature: Some([0x03; 64]),
        new_signature: Some([0x04; 64]),
    }
}

/// 首次 bootstrap 用一条 rev1 genesis 管理链（真实 relay 的 `/control` 键 5 必然有它）。
fn seed_manager_chain(relay: &mut FakeRelay, materials: &genesis::GenesisMaterials) {
    relay.manager_chain = vec![menghunote_sync::relay::ManagerChainEntry {
        rev: 1,
        pubkey: materials.manager.public_key(),
        transition: b"test-genesis".to_vec(),
        old_rev: 1,
        old_signature: None,
        new_signature: None,
    }];
}

#[test]
fn bootstrap_anchor_is_recomputed_from_the_epoch_history_when_rev_advances() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_source(&source, &mut relay, &materials);
    seed_manager_chain(&mut relay, &materials);

    let (_target_dir, target) = fresh_vault();
    bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect("首次 bootstrap");
    let first_rev = relay.control_rev;
    let anchor_hash = target
        .get_meta(META_BOOTSTRAP_CONTROL_HASH)
        .expect("control hash")
        .expect("锚必须写入");

    // control_rev 前进 + 管理链多一条"未来 epoch"的转换：旧锚必须按旧 epoch 的链前缀
    // 复算，而不是拿新 rev / 全链复算（那会算出不同摘要），也不能干脆忽略。
    relay.control_rev = first_rev + 1;
    relay
        .manager_chain
        .push(future_manager_transition(relay.instance_id));
    let state = relay.state().expect("state");
    let chain = relay.management_chain().expect("chain");
    let full_chain_hash = control_head_hash(&state, &chain);
    assert_ne!(
        full_chain_hash.as_slice(),
        anchor_hash.as_slice(),
        "新 rev + 全链的摘要必须与旧锚不同（否则这条用例证明不了历史视图被用到）"
    );

    bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect("rev 前进但历史前缀未变时必须允许（按 epoch 截断后的旧视图验证）");
    // pins 前进到新 rev / 新摘要。
    assert_eq!(
        target
            .get_meta(META_BOOTSTRAP_CONTROL_REV)
            .expect("control rev")
            .as_deref(),
        Some((first_rev + 1).to_be_bytes().as_slice())
    );
}

#[test]
fn bootstrap_anchor_rejects_a_rewritten_prefix_when_rev_advances() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_source(&source, &mut relay, &materials);
    seed_manager_chain(&mut relay, &materials);

    let (_target_dir, target) = fresh_vault();
    bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect("首次 bootstrap");
    let first_rev = relay.control_rev;

    // rev 前进，但 genesis 那条被换掉（历史前缀被重写）。
    relay.control_rev = first_rev + 1;
    relay.manager_chain[0].transition = b"rewritten-genesis".to_vec();
    let error = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect_err("旧锚未被新链覆盖必须拒绝，不能当 rev 前进就跳过");
    assert!(
        matches!(error, menghunote_sync::SyncError::Equivocation(_)),
        "{error:?}"
    );
    // 失败路径不得改写本地 pin。
    assert_eq!(
        target
            .get_meta(META_BOOTSTRAP_CONTROL_REV)
            .expect("control rev")
            .as_deref(),
        Some(first_rev.to_be_bytes().as_slice())
    );
}

#[test]
fn bootstrap_anchor_same_rev_hash_swap_is_equivocation() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_source(&source, &mut relay, &materials);
    seed_manager_chain(&mut relay, &materials);

    let (_target_dir, target) = fresh_vault();
    bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect("首次 bootstrap");

    // 同 rev 换摘要：必须 equivocation（不是"没有锚"、也不是普通 Object 错误）。
    let mut swapped = target
        .get_meta(META_BOOTSTRAP_CONTROL_HASH)
        .expect("control hash")
        .expect("锚");
    swapped[0] ^= 0xFF;
    target
        .set_meta(META_BOOTSTRAP_CONTROL_HASH, &swapped)
        .expect("改写摘要");
    let error = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect_err("同 rev 换 hash 必须拒绝");
    assert!(
        matches!(error, menghunote_sync::SyncError::Equivocation(_)),
        "{error:?}"
    );
}

`````


---

# 附录 C：Phase 1B diff（`7583d8a..038e92c`）

`````diff
diff --git a/crates/storage/src/baseline_apply.rs b/crates/storage/src/baseline_apply.rs
index 53fde44..0e0da63 100644
--- a/crates/storage/src/baseline_apply.rs
+++ b/crates/storage/src/baseline_apply.rs
@@ -1637,17 +1637,22 @@ fn state_from_journal(
         Some(expected) => (Some(expected.doc_count()), Some(expected.asset_count())),
         None => (None, None),
     };
-    // finishable 必须反映**真实就绪**：仅 status=staging 不够，还要 begin 记录的预期
-    // 集合全部 applied=1（否则 finish 会 Conflict，不能对 UI/续跑逻辑谎报可完成）。
+    // finishable 是 **O(1) 近似**：只校验 status 与「已应用计数 == 预期计数」。
+    // 逐项 `applied=1`/摘要的精确证明只在 `finish_baseline_apply` 里做：
+    //   - 这样 Phase1B 的 catchup 循环每次 batch 后读 state 不会做 O(N) 查询；
+    //   - 代价是计数相等但底层行损坏时 finishable 可能短暂为 true，finish 会
+    //     `Conflict` 且零写，不会误推进水位。
     //
     // 失败标记存在时 journal 仍是 staging（schema v3 的 CHECK 只允许 staging/completed），
-    // 但语义上已不可续跑：即使预期集合全齐也必须 finishable=false，否则 UI/续跑逻辑
-    // 会把"读回校验失败"当成"可以 finish"。标记损坏返回 Corrupt，同样是 fail-closed。
+    // 但语义上已不可续跑：即使预期集合全齐也必须 finishable=false。
     let verify_failed = baseline_verify_failure(vault)?.is_some();
     let finishable = !verify_failed
         && journal.status == BaselineApplyStatus::Staging
         && match &expected {
-            Some(expected) => expected_is_complete(vault, journal, expected)?,
+            Some(expected) => {
+                journal.applied_docs == expected.doc_count()
+                    && journal.applied_assets == expected.asset_count()
+            }
             None => false,
         };
     Ok(BaselineApplyState {
diff --git a/crates/storage/src/lib.rs b/crates/storage/src/lib.rs
index 0baaaf0..a30a87f 100644
--- a/crates/storage/src/lib.rs
+++ b/crates/storage/src/lib.rs
@@ -123,13 +123,14 @@ pub use sync_bridge::{
     intent_object_id, local_sync_intent, mark_acked, mark_inbox_quarantine, mark_integrated,
     mark_integrated_batch, mark_sent, mark_skipped_event_received, outbox_parts,
     outbox_repack_candidates, outbox_row, outbox_rows, pending_encryption, pending_send,
-    repack_outbox, repack_required, set_confirmed_highwater, set_repack_required, set_sync_log_id,
-    store_encrypted_parts, store_inbox_verified, store_inbox_verified_batch, sync_cursor_state,
-    take_inbox_ready, take_inbox_ready_where, update_referenced_by_outbox, OutboxKey,
-    OutboxRepackCandidate, RepackOutcome, RepackPart, RepackPartSummary, RepackSpec, SyncContext,
-    SyncCursorState, SyncEncryptedPart, SyncInboxRow, SyncInboxState, SyncIntent, SyncOutboxPart,
-    SyncOutboxRow, SyncOutboxState, VerifiedInboxRow, META_SYNC_CONFIRMED_HIGHWATER,
-    META_SYNC_INTEGRATED_SEQ, META_SYNC_LOG_ID, META_SYNC_RECEIVED_SEQ, META_SYNC_REPACK_REQUIRED,
-    META_SYNC_SKIPPED_RECEIVED_PREFIX, SYNC_INTENT_ID_DOMAIN,
+    prune_covered_inbox, repack_outbox, repack_required, set_confirmed_highwater,
+    set_repack_required, set_sync_log_id, store_encrypted_parts, store_inbox_verified,
+    store_inbox_verified_batch, sync_cursor_state, take_inbox_ready, take_inbox_ready_where,
+    update_referenced_by_outbox, OutboxKey, OutboxRepackCandidate, RepackOutcome, RepackPart,
+    RepackPartSummary, RepackSpec, SyncContext, SyncCursorState, SyncEncryptedPart, SyncInboxRow,
+    SyncInboxState, SyncIntent, SyncOutboxPart, SyncOutboxRow, SyncOutboxState, VerifiedInboxRow,
+    META_SYNC_CONFIRMED_HIGHWATER, META_SYNC_INTEGRATED_SEQ, META_SYNC_LOG_ID,
+    META_SYNC_RECEIVED_SEQ, META_SYNC_REPACK_REQUIRED, META_SYNC_SKIPPED_RECEIVED_PREFIX,
+    SYNC_INTENT_ID_DOMAIN,
 };
 pub use tree_view::{list_notes, list_tree, list_tree_sorted, NodeKind, TreeEntry, TreeSort};
diff --git a/crates/storage/src/session/flush.rs b/crates/storage/src/session/flush.rs
index fcb3c10..e6636f5 100644
--- a/crates/storage/src/session/flush.rs
+++ b/crates/storage/src/session/flush.rs
@@ -486,7 +486,10 @@ fn flusher_main(
         // 否则「手动保存」在无脏数据时永远等不到 ACK。
         let confirmation = {
             let guard = lock(&state);
-            if guard.journal.pending_len() == 0
+            // 不可恢复时即使 pending_len==0（防御性状态，正常路径不可达）也不能补发
+            // 确认票据：那会让等待方误以为 durable 成功。
+            if guard.unrecoverable.is_none()
+                && guard.journal.pending_len() == 0
                 && emitted_max.is_none_or(|through| through < command.target_live_rev)
             {
                 Some((
@@ -516,13 +519,20 @@ fn flusher_main(
 ///（`Corrupt`、`Loro`、`LoroEncode`、`Coord`、schema 漂移、通道关闭等）都是永久性
 /// 本地状态错误，重试不会自愈，必须 fail-closed 到“需要重开文档”。
 fn is_retryable_write_error(error: &StorageError) -> bool {
-    matches!(
-        error,
+    match error {
         StorageError::Io(_)
-            | StorageError::Sqlite(_)
-            | StorageError::SnapshotBusy { .. }
-            | StorageError::WriteBackpressure { .. }
-    )
+        | StorageError::SnapshotBusy { .. }
+        | StorageError::WriteBackpressure { .. } => true,
+        // SQLite 错误不能整类重试：ConstraintViolation/ReadOnly/CannotOpen/
+        // DatabaseCorrupt/NotFound/Misuse 等都是永久错误，重试只会无限撞同一面墙。
+        // 只有 BUSY/LOCKED 这类并发等待值得下一次 flush 再试。
+        StorageError::Sqlite(rusqlite::Error::SqliteFailure(code, _)) => matches!(
+            code.code,
+            rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
+        ),
+        StorageError::Sqlite(_) => false,
+        _ => false,
+    }
 }
 
 /// head 冲突后自动重建 sink 的结果（P1-3 生产接线）。
@@ -536,6 +546,24 @@ enum RebuildOutcome {
     Retryable(String),
 }
 
+/// 重建后重试失败的分类（独立出来便于单测 `RebuildOutcome::Retryable` 分支）。
+fn rebuild_write_error_outcome(
+    doc_id: &str,
+    pending: usize,
+    error: StorageError,
+) -> RebuildOutcome {
+    if !is_retryable_write_error(&error) {
+        return RebuildOutcome::Unrecoverable(reopen_message(
+            doc_id,
+            pending,
+            format!("自动重建后重试前缀遇到永久性 Corrupt，不再自动重试：{error}"),
+        ));
+    }
+    RebuildOutcome::Retryable(format!(
+        "{doc_id} 自动重建后重试前缀失败（journal 保留，等待下次 flush）：{error}"
+    ))
+}
+
 /// `HeadConflict` 后的有界自动重建：`load_head` → 新 CAS 基准 sink → 重试同一前缀一次。
 ///
 /// **为什么不是无条件重试**：journal 条目的 `PendingUpdate` 是**追加那一刻**按旧 head
@@ -638,16 +666,7 @@ fn retry_prefix_after_rebuild(
             pending,
             format!("自动重建后再发生 head 冲突（基准 {expected}，实际 {actual}）"),
         )),
-        Err(error) if !is_retryable_write_error(&error) => {
-            RebuildOutcome::Unrecoverable(reopen_message(
-                doc_id,
-                pending,
-                format!("自动重建后重试前缀遇到永久性 Corrupt，不再自动重试：{error}"),
-            ))
-        }
-        Err(error) => RebuildOutcome::Retryable(format!(
-            "{doc_id} 自动重建后重试前缀失败（journal 保留，等待下次 flush）：{error}"
-        )),
+        Err(error) => rebuild_write_error_outcome(doc_id, pending, error),
     }
 }
 
@@ -660,3 +679,51 @@ fn reopen_message(doc_id: &str, pending: usize, reason: String) -> String {
          重开前未 durable 的本地改动不会自动落库"
     )
 }
+
+#[cfg(test)]
+mod tests {
+    use super::*;
+
+    fn sqlite_error(code: rusqlite::ErrorCode, extended_code: i32) -> StorageError {
+        StorageError::Sqlite(rusqlite::Error::SqliteFailure(
+            rusqlite::ffi::Error {
+                code,
+                extended_code,
+            },
+            None,
+        ))
+    }
+
+    #[test]
+    fn retryable_write_error_is_a_positive_whitelist() {
+        assert!(is_retryable_write_error(&sqlite_error(
+            rusqlite::ErrorCode::DatabaseBusy,
+            5
+        )));
+        assert!(is_retryable_write_error(&sqlite_error(
+            rusqlite::ErrorCode::DatabaseLocked,
+            6
+        )));
+        assert!(!is_retryable_write_error(&sqlite_error(
+            rusqlite::ErrorCode::ConstraintViolation,
+            1811
+        )));
+        assert!(!is_retryable_write_error(&sqlite_error(
+            rusqlite::ErrorCode::ReadOnly,
+            8
+        )));
+        assert!(!is_retryable_write_error(&StorageError::Corrupt {
+            context: "test".to_owned(),
+        }));
+    }
+
+    #[test]
+    fn rebuild_write_error_outcome_maps_busy_to_retryable_and_permanent_to_unrecoverable() {
+        let busy = sqlite_error(rusqlite::ErrorCode::DatabaseBusy, 5);
+        let retryable = rebuild_write_error_outcome("doc-1", 2, busy);
+        assert!(matches!(retryable, RebuildOutcome::Retryable(_)));
+        let permanent = sqlite_error(rusqlite::ErrorCode::ConstraintViolation, 1811);
+        let unrecoverable = rebuild_write_error_outcome("doc-1", 2, permanent);
+        assert!(matches!(unrecoverable, RebuildOutcome::Unrecoverable(_)));
+    }
+}
diff --git a/crates/storage/src/session/schedule.rs b/crates/storage/src/session/schedule.rs
index d08054d..0f7a421 100644
--- a/crates/storage/src/session/schedule.rs
+++ b/crates/storage/src/session/schedule.rs
@@ -228,6 +228,19 @@ pub struct CloseReport {
     pub flush_error: Option<String>,
 }
 
+impl CloseReport {
+    /// 这次关闭是否暴露了需要“重开文档”的不可恢复失败。
+    ///
+    /// 只看 `flush_error` 的统一文案（`reopen_message`）；普通磁盘/SQLite 的瞬时失败
+    /// 也可能带 `flush_error`，但允许下一次再 flush，不应被 UI 当成永久坏状态。
+    #[must_use]
+    pub fn needs_reopen(&self) -> bool {
+        self.flush_error
+            .as_deref()
+            .is_some_and(|message| message.contains("需要重开文档"))
+    }
+}
+
 /// 一条 B 物化任务（只带固定 seq，不带 live 句柄）。
 #[derive(Debug, Clone, PartialEq, Eq)]
 struct MaterializeJob {
diff --git a/crates/storage/src/sync_bridge.rs b/crates/storage/src/sync_bridge.rs
index a831dea..fe052ef 100644
--- a/crates/storage/src/sync_bridge.rs
+++ b/crates/storage/src/sync_bridge.rs
@@ -1094,6 +1094,37 @@ pub fn store_inbox_verified_batch(
 /// `relay_seq <= received_seq`，不在连续前缀内的行保持 durable，等前面的缺口补齐。
 /// 排序为 `(log_id, relay_seq)`。
 ///
+/// 清理已被 `integrated` 前缀覆盖的 inbox 行（Phase 1B 追赶后）。
+///
+/// **安全边界**：只删除 `relay_seq <= up_to_seq` 且同一 SQL 内证明
+/// `sync.integrated_seq >= up_to_seq` 的行；证明不成立时一行不删。调用方还必须保证
+/// `log_id` 是当前 relay incarnation，且 `up_to_seq` 是实际成功的 checkpoint S。
+///
+/// 为什么不用先读后删：读 `integrated` 与 DELETE 之间可能被并发推进/回退；
+/// `EXISTS` 子查询把证明和删除放在同一条原子语句里。BLOB 按 8 字节 BE 比较。
+pub fn prune_covered_inbox(
+    vault: &Vault,
+    log_id: &[u8; 16],
+    up_to_seq: u64,
+) -> Result<usize, StorageError> {
+    let up_to = i64_of(up_to_seq)?;
+    let statements = [Statement::new(
+        "DELETE FROM sync_inbox \
+         WHERE log_id=?1 AND relay_seq<=?2 \
+           AND EXISTS (SELECT 1 FROM meta \
+                        WHERE key=?3 AND typeof(value)='blob' AND length(value)=8 \
+                          AND value>=?4)",
+        vec![
+            Value::Blob(log_id.to_vec()),
+            Value::Integer(up_to),
+            Value::Text(META_SYNC_INTEGRATED_SEQ.to_owned()),
+            Value::Blob(up_to_seq.to_be_bytes().to_vec()),
+        ],
+    )];
+    let changed = vault.exec_transaction(&statements, &[])?;
+    Ok(changed.first().copied().unwrap_or(0))
+}
+
 /// **incarnation 过滤**（ADR 0006 §5）：`sync_inbox` 的 `log_id` 是写入时那代 relay
 /// 日志的 incarnation。restore 迁移 `sync.log_id` 后，旧 incarnation 的行仍然 durable
 /// （不能在迁移时静默删数据），但绝不能进入 ready——`relay_seq` 只在同一 incarnation
diff --git a/crates/storage/tests/body_doc_detached_oplog.rs b/crates/storage/tests/body_doc_detached_oplog.rs
index 77996d1..9620bfb 100644
--- a/crates/storage/tests/body_doc_detached_oplog.rs
+++ b/crates/storage/tests/body_doc_detached_oplog.rs
@@ -29,15 +29,20 @@ use loro::{ExportMode, LoroDoc, LoroTree, LoroValue, TreeID, VersionVector};
 use support::*;
 
 use menghunote_storage::{
-    apply_baseline_batch, begin_baseline_apply, doc_updates_since, finish_baseline_apply,
-    load_body_doc, BaselineApplyFinish, BaselineExpectation, BodyDoc, StoredLoroUpdate, Vault,
-    BASELINE_MERGE_BATCH,
+    append_updates, apply_baseline_batch, baseline_apply_debug_counters, begin_baseline_apply,
+    doc_updates_since, finish_baseline_apply, load_body_doc, reset_baseline_apply_debug,
+    BaselineApplyFinish, BaselineExpectation, BodyDoc, PendingUpdate, StoredLoroUpdate,
+    UpdateRecord, Vault, BASELINE_MERGE_BATCH,
 };
 
 /// 本地设备（attached 侧）。
 const A_PEER: u64 = PEER_A;
 /// 远端基线导出方（与本地不同的 peer）。
 const B_PEER: u64 = PEER_B;
+/// 第三个 peer：只出现在基线快照里、本地从未导入的远端设备（本地未知 peer）。
+const C_PEER: u64 = 3;
+/// 第四个 peer：本地以非因果顺序导入的另一个远端设备。
+const D_PEER: u64 = 4;
 
 // ---------------------------------------------------------------------------
 // 正文（LoroText）用例的脚手架
@@ -73,12 +78,36 @@ fn snapshot_vv(snapshot: &[u8]) -> VersionVector {
 /// VV 的规范表示：`VersionVector::encode` 的字节顺序取决于 import 顺序（不是规范形式），
 /// 所以按 peer 排序后比较「peer -> counter」集合；语义相等即等价。
 fn canonical_vv(bytes: &[u8]) -> Vec<(u64, i32)> {
-    let vv = VersionVector::decode(bytes).expect("VV 可解码");
+    canonical_of_vv(&VersionVector::decode(bytes).expect("VV 可解码"))
+}
+
+/// 同 [`canonical_vv`]，但输入已是解码后的 VV（断言 blob 元信息与文档 VV 用）。
+fn canonical_of_vv(vv: &VersionVector) -> Vec<(u64, i32)> {
     let mut entries: Vec<(u64, i32)> = vv.iter().map(|(peer, counter)| (*peer, *counter)).collect();
     entries.sort_unstable();
     entries
 }
 
+/// 单个 peer 的计数（缺失按 0 处理，与 `VersionVector` 语义一致）。
+fn counter_of(vv: &VersionVector, peer: u64) -> i32 {
+    vv.iter()
+        .find(|(candidate, _)| **candidate == peer)
+        .map(|(_, counter)| *counter)
+        .unwrap_or(0)
+}
+
+/// 已编码 VV 里的 peer 集合（升序）。
+///
+/// 多 peer 用例用它断言 merged oplog VV 覆盖了场景的全部 peer：`oplog_frontiers` 只是
+/// DAG tips，被后续 op 依赖的 peer 会从 tips 里消失，不能当成「peer 全集」。
+fn vv_peers(bytes: &[u8]) -> Vec<u64> {
+    let vv = VersionVector::decode(bytes).expect("VV 可解码");
+    let mut peers: Vec<u64> = vv.keys().copied().collect();
+    peers.sort_unstable();
+    peers.dedup();
+    peers
+}
+
 /// `loro_updates` 里 baseline merge 追加的那一行。
 struct DetachedMerge {
     /// payload 由 `pending_update_from_oplog` 导出，end_frontier / end_vv 取 **oplog** 版本。
@@ -88,6 +117,10 @@ struct DetachedMerge {
 }
 
 /// 驱动生产 `existing_local_extra`（detached）分支，并取回它落库的差集。
+///
+/// 常规两 peer 用例走这个入口：期望合并后的 oplog frontier（DAG tips）恰好是
+/// 本地 + 基线两个 peer。多 peer 边界用例用 [`merge_detached_with_peers`] 显式给出
+/// 该场景期望的 frontier peer 集合，断言不放宽。
 fn merge_detached(
     vault: &Vault,
     doc_id: &str,
@@ -95,6 +128,33 @@ fn merge_detached(
     local: &BodyDoc,
     baseline_s: u64,
     cursor: &str,
+) -> DetachedMerge {
+    merge_detached_with_peers(
+        vault,
+        doc_id,
+        snapshot,
+        local,
+        baseline_s,
+        cursor,
+        &[A_PEER, B_PEER],
+    )
+}
+
+/// 同 [`merge_detached`]，但合并后 oplog **frontier（DAG tips）** 的 peer 集合由
+/// 调用方给出（升序）。
+///
+/// 注意 frontier 是 DAG tips，不是「peer 全集」：某一 peer 的 op 一旦被后续 op 依赖
+/// （例如基线在未知 peer C 的 op 之后继续写），它就不再是 tip。真正的全量证据是
+/// merged `end_vv`，由各多 peer 用例用 [`vv_peers`] 单独断言，并与
+/// [`assert_detached_matches_attached`] 的 attached VV 逐项相等。
+fn merge_detached_with_peers(
+    vault: &Vault,
+    doc_id: &str,
+    snapshot: &[u8],
+    local: &BodyDoc,
+    baseline_s: u64,
+    cursor: &str,
+    expected_peers: &[u64],
 ) -> DetachedMerge {
     let local_delta = local
         .pending_update(&snapshot_vv(snapshot))
@@ -114,16 +174,16 @@ fn merge_detached(
         .expect("detached 分支必须落一条 baseline_merge 差集");
     assert!(!merged.payload.is_empty(), "detached 导出的差集不能为空");
 
-    // 分支守卫：detached 合并后的 oplog frontier 必须同时包含本地与基线两个 peer。
-    // 若生产误走 attached 分支，这里只会是基线 state 的单 peer frontier。
+    // 分支守卫：detached 合并后的 oplog frontier 必须与场景一致。
+    // 若生产误走 attached 分支，这里只会是基线 state 的 peer 集合。
     let frontier = loro::Frontiers::decode(&merged.end_frontier).expect("frontier 可解码");
     let mut peers: Vec<u64> = frontier.iter().map(|id| id.peer).collect();
     peers.sort_unstable();
     peers.dedup();
     assert_eq!(
-        peers,
-        vec![A_PEER, B_PEER],
-        "detached 分支未生效？合并后的 oplog frontier 必须同时包含两个 peer"
+        peers.as_slice(),
+        expected_peers,
+        "detached 分支未生效？合并后的 oplog frontier peer 集合必须与场景一致（实际 {peers:?}）"
     );
 
     DetachedMerge {
@@ -132,6 +192,17 @@ fn merge_detached(
     }
 }
 
+/// `loro_updates` 里 `batch_id == BASELINE_MERGE_BATCH` 的全部行（按 seq 升序）。
+///
+/// 幂等用例用它证明「重放/重复投递不新增、不改写 detached 落库的差集行」。
+fn baseline_merge_rows(vault: &Vault, doc_id: &str) -> Vec<StoredLoroUpdate> {
+    doc_updates_since(vault, doc_id, 0)
+        .expect("读 loro_updates")
+        .into_iter()
+        .filter(|update| update.batch_id == BASELINE_MERGE_BATCH)
+        .collect()
+}
+
 /// 断言 detached 结果与 attached 路径完全等价，返回合并后的正文。
 ///
 /// attached 侧有三条证据链，全部要与 detached 的 frontier 逐字节一致、VV 语义一致：
@@ -788,3 +859,656 @@ fn precomposed_vs_decomposed_accent_merges_without_normalization() {
     assert_ne!(text, "éé", "不得做 NFC 归一化：{text:?}");
     assert_ne!(text, "e\u{301}e\u{301}", "不得做 NFD 归一化：{text:?}");
 }
+
+// ---------------------------------------------------------------------------
+// 5. 边界：基线 VV 里含本地从未导入的 peer
+// ---------------------------------------------------------------------------
+
+/// 基线快照里带一个本地从未导入的 peer C（基线 VV 有 C、本地 VV 没有）时，
+/// detached 导出/合并必须被钉死为：
+/// - 不 panic、不把 C 的 op 丢掉（否则本地会永久缺一段远端历史）；
+/// - 本地差集只含本地 peer 的 op——C 已在基线 VV 覆盖范围内，不能重复导出；
+/// - merged 差集恰好是「基线相对本地 VV 的差集」：单独重放等于基线文本（含未知 peer C 的 op），
+///   而不是全量历史、也不是空集。
+#[test]
+fn detached_baseline_vv_with_unimported_peer_keeps_both_sides_once() {
+    let (_dir, vault) = fresh_vault();
+
+    // 本地 peer A：独立文档，从未导入基线（本地 VV 只有 A）。
+    let local = seed_note(&vault, "note-1", "doc-1");
+    local.insert_scalars(0, "LOCAL").expect("本地编辑");
+    commit_now(&vault, &local, "local-batch");
+
+    // 基线：peer B 先导入 peer C 的 update（本地从未见过 C），再追加自己的文本。
+    let third = BodyDoc::create("doc-1", C_PEER).expect("第三台设备文档");
+    third.insert_scalars(0, "C").expect("C 的文本");
+    let third_update = third
+        .pending_update(&VersionVector::new())
+        .expect("C 的增量");
+    let remote = BodyDoc::create("doc-1", B_PEER).expect("基线文档");
+    remote
+        .import(&third_update.payload)
+        .expect("基线导入 C 的 op");
+    remote
+        .insert_scalars(remote.scalar_len(), "BASE")
+        .expect("基线文本");
+    let snapshot = remote.export_snapshot().expect("基线快照");
+
+    let baseline_vv = snapshot_vv(&snapshot);
+    let local_vv = local.vv();
+    assert!(
+        counter_of(&baseline_vv, C_PEER) > 0,
+        "基线 VV 必须含本地从未导入的 peer C：{baseline_vv:?}"
+    );
+    assert_eq!(
+        counter_of(&local_vv, C_PEER),
+        0,
+        "本地 VV 不得含 C peer（否则测不到「本地未知 peer」）"
+    );
+    // 生产分支守卫：本地在基线之外还有 op（A 不在基线 VV 里）。
+    assert!(
+        local_vv.sub_iter(&baseline_vv).next().is_some(),
+        "本地必须相对基线有额外 op，否则生产走 attached 分支"
+    );
+
+    let parts = merge_detached_with_peers(
+        &vault,
+        "doc-1",
+        &snapshot,
+        &local,
+        5,
+        "cursor-1",
+        &[A_PEER, B_PEER],
+    );
+    // detached 合并后的 oplog VV 必须覆盖全部三个 peer（未知 peer C 的 op 不能丢）。
+    assert_eq!(
+        vv_peers(&parts.merged.end_vv),
+        vec![A_PEER, B_PEER, C_PEER],
+        "合并后的 oplog VV 必须含未知 peer C"
+    );
+    let text = assert_detached_matches_attached(&vault, "doc-1", &snapshot, &parts);
+
+    assert_eq!(
+        text.len(),
+        "LOCAL".len() + "CBASE".len(),
+        "不得有重复或残留字符：{text:?}"
+    );
+    assert_eq!(
+        text.matches("LOCAL").count(),
+        1,
+        "本地文本必须恰好一次：{text:?}"
+    );
+    assert_eq!(
+        text.matches("CBASE").count(),
+        1,
+        "基线文本必须恰好一次：{text:?}"
+    );
+
+    // 本地差集只含本地 peer 的 op：单独重放就是本地文本，不含 C/B 的历史。
+    let local_delta_alone = BodyDoc::restore(
+        "local-delta-probe",
+        A_PEER,
+        None,
+        std::slice::from_ref(&parts.local_delta),
+    )
+    .expect("本地差集可重放");
+    assert_eq!(
+        local_delta_alone.text(),
+        "LOCAL",
+        "本地差集不得包含基线 op：{local_delta_alone:?}"
+    );
+    assert_eq!(
+        canonical_vv(&local_delta_alone.vv().encode()),
+        canonical_of_vv(&local_vv),
+        "本地差集的 VV 必须等于本地 VV"
+    );
+
+    // merged 差集 = 基线相对本地 VV 的差集：单独重放恰好是基线文本（C 的 op 必须在内）。
+    let merged_alone = BodyDoc::restore(
+        "merged-probe",
+        A_PEER,
+        None,
+        std::slice::from_ref(&parts.merged.payload),
+    )
+    .expect("merged 差集可重放");
+    assert_eq!(
+        merged_alone.text(),
+        "CBASE",
+        "merged 差集必须正好覆盖基线文本：{merged_alone:?}"
+    );
+    assert_eq!(
+        canonical_vv(&merged_alone.vv().encode()),
+        canonical_of_vv(&baseline_vv),
+        "merged 差集的 VV 必须等于基线 VV（未知 peer C 的 op 不能丢）"
+    );
+}
+
+// ---------------------------------------------------------------------------
+// 6. 边界：`from` 超前于 doc 当前 VV（病态输入）
+// ---------------------------------------------------------------------------
+
+/// `from`（基线 VV）对本地 peer 的计数严格超前于本地文档当前 VV 时（病态输入：基线
+/// 声称包含本地还没拿到的 A op），按现有生产 API 的语义钉死：
+/// 不 panic、不报错：`export(updates(from))` 对超前的 from 只导出**未被覆盖**的 op；
+/// 本地差集 = 未被 from 覆盖的 peer 的 op（C/D），绝不把 A 的旧 op 再导出一遍；
+/// 也不退化成空集（C/D 是本地真正多出来的 op）。
+/// 断言用 blob 元信息（partial_start_vv / partial_end_vv）证明导出区间，不靠猜测文本顺序。
+#[test]
+fn detached_export_from_ahead_of_local_vv_keeps_only_uncovered_ops() {
+    let (_dir, vault) = fresh_vault();
+
+    // peer A 的两笔历史：基线包含 A 的全量（A:1..4），本地只导入前半（A:1..2）。
+    let a_doc = BodyDoc::create("doc-1", A_PEER).expect("A 文档");
+    a_doc.insert_scalars(0, "A1").expect("A 第一笔");
+    let a_first = a_doc
+        .pending_update(&VersionVector::new())
+        .expect("A 第一笔增量");
+    a_doc.insert_scalars(2, "A2").expect("A 第二笔");
+    let a_all = a_doc
+        .pending_update(&VersionVector::new())
+        .expect("A 全量增量");
+
+    // 本地：A 的前半 + 两个本地真正多出来的 peer（C/D）。
+    let third = BodyDoc::create("doc-1", C_PEER).expect("C 文档");
+    third.insert_scalars(0, "C").expect("C 文本");
+    let third_update = third.pending_update(&VersionVector::new()).expect("C 增量");
+    let fourth = BodyDoc::create("doc-1", D_PEER).expect("D 文档");
+    fourth.insert_scalars(0, "D").expect("D 文本");
+    let fourth_update = fourth
+        .pending_update(&VersionVector::new())
+        .expect("D 增量");
+    let local = seed_note(&vault, "note-1", "doc-1");
+    local.import(&a_first.payload).expect("本地导入 A 前半");
+    local.import(&third_update.payload).expect("本地导入 C");
+    local.import(&fourth_update.payload).expect("本地导入 D");
+    commit_now(&vault, &local, "local-batch");
+
+    // 基线：导入 A 的全量（对 A 的计数超前本地）再写自己的文本。
+    let remote = BodyDoc::create("doc-1", B_PEER).expect("基线文档");
+    remote.import(&a_all.payload).expect("基线导入 A 全量");
+    remote.insert_scalars(0, "BASE").expect("基线文本");
+    let snapshot = remote.export_snapshot().expect("基线快照");
+
+    let baseline_vv = snapshot_vv(&snapshot);
+    let local_vv = local.vv();
+    assert!(
+        counter_of(&baseline_vv, A_PEER) > counter_of(&local_vv, A_PEER),
+        "基线对 A 的计数必须严格超前本地（from 超前）：baseline={baseline_vv:?} local={local_vv:?}"
+    );
+    assert!(
+        local_vv.sub_iter(&baseline_vv).next().is_some(),
+        "本地必须相对基线有额外 op（C/D），否则生产走 attached 分支"
+    );
+
+    let parts = merge_detached_with_peers(
+        &vault,
+        "doc-1",
+        &snapshot,
+        &local,
+        6,
+        "cursor-2",
+        &[B_PEER, C_PEER, D_PEER],
+    );
+    // detached 合并后的 oplog VV 必须覆盖全部四个 peer（A 的基线 op 与本地 C/D 的 op 都在）。
+    assert_eq!(
+        vv_peers(&parts.merged.end_vv),
+        vec![A_PEER, B_PEER, C_PEER, D_PEER],
+        "合并后的 oplog VV 必须含全部四个 peer"
+    );
+    let text = assert_detached_matches_attached(&vault, "doc-1", &snapshot, &parts);
+
+    assert_eq!(
+        text.len(),
+        "BASEA1A2CD".len(),
+        "不得有重复或残留字符：{text:?}"
+    );
+    for marker in ["BASE", "A1A2", "C", "D"] {
+        assert_eq!(
+            text.matches(marker).count(),
+            1,
+            "{marker} 必须恰好一次：{text:?}"
+        );
+    }
+
+    // 本地差集：blob 元信息证明只导出 C/D（A 的旧 op 被 from 覆盖，不重复导出）。
+    let delta_meta =
+        LoroDoc::decode_import_blob_meta(&parts.local_delta, true).expect("本地差集元信息");
+    assert_eq!(
+        canonical_of_vv(&delta_meta.partial_start_vv),
+        vec![(C_PEER, 0), (D_PEER, 0)],
+        "本地差集必须从 C/D 的 0 开始（不含 A 的 op）"
+    );
+    assert_eq!(
+        canonical_of_vv(&delta_meta.partial_end_vv),
+        vec![
+            (C_PEER, counter_of(&local_vv, C_PEER)),
+            (D_PEER, counter_of(&local_vv, D_PEER)),
+        ],
+        "本地差集终点必须是 C/D 的本地计数"
+    );
+    // 单独重放：只有 C/D 两个字符（依赖缺失的 op 不会凭空出现）。
+    let local_delta_alone = BodyDoc::restore(
+        "local-delta-probe",
+        A_PEER,
+        None,
+        std::slice::from_ref(&parts.local_delta),
+    )
+    .expect("本地差集可重放");
+    assert_eq!(
+        local_delta_alone.text().chars().count(),
+        2,
+        "本地差集只应含 C/D：{local_delta_alone:?}"
+    );
+    assert_eq!(local_delta_alone.text().matches('C').count(), 1);
+    assert_eq!(local_delta_alone.text().matches('D').count(), 1);
+    assert!(
+        !local_delta_alone.text().contains('A'),
+        "不得把 A 的旧 op 再导出：{local_delta_alone:?}"
+    );
+
+    // merged 差集：起点 = 本地 VV（A:2、B:0），终点 = 基线 VV（A:4、B:4）。
+    let merged_meta =
+        LoroDoc::decode_import_blob_meta(&parts.merged.payload, true).expect("merged 元信息");
+    assert_eq!(
+        canonical_of_vv(&merged_meta.partial_start_vv),
+        vec![(A_PEER, counter_of(&local_vv, A_PEER)), (B_PEER, 0)],
+        "merged 差集必须从本地 VV 起算（超前 from 的语义）"
+    );
+    assert_eq!(
+        canonical_of_vv(&merged_meta.partial_end_vv),
+        vec![
+            (A_PEER, counter_of(&baseline_vv, A_PEER)),
+            (B_PEER, counter_of(&baseline_vv, B_PEER)),
+        ],
+        "merged 差集终点必须是基线 VV"
+    );
+    // 单独重放 merged 差集：A 的后续 op 缺依赖、B 的 op 依赖也未满足 → 不产生可见文本；
+    // 关键是否定断言：绝不能把本地独有的 C/D 再导出一遍。
+    let merged_alone = BodyDoc::restore(
+        "merged-probe",
+        A_PEER,
+        None,
+        std::slice::from_ref(&parts.merged.payload),
+    )
+    .expect("merged 差集可重放");
+    assert_eq!(
+        merged_alone.text(),
+        "",
+        "merged 差集单独重放不得产生文本：{merged_alone:?}"
+    );
+}
+
+// ---------------------------------------------------------------------------
+// 7. 边界：同一个 update 重复 import 的幂等性
+// ---------------------------------------------------------------------------
+
+/// 同一个 update 重复 import 的幂等性。两轮 apply 走**不同**的生产分支：
+/// 首轮：本地有 extra → `existing_local_extra`（detached）分支，落一条 baseline_merge；
+/// 次轮：同一 snapshot digest 已应用 → 生产在 detached 之前短路到
+/// `replayed_same_digest` 分支，**不得**再进 detached，也不得新增 baseline_merge、
+/// 推进 head_seq、改 head_vv/head_frontier 或文本。
+/// 另在 Loro 层直接钉住「同一 update 导入两次不产生新 op」（`import_oplog_only` 依赖的
+/// 不变量），并在数据库层钉住「重复投递同一 payload 行经 `load_body_doc` 重放不变」。
+#[test]
+fn detached_same_update_reimport_is_idempotent() {
+    let (_dir, vault) = fresh_vault();
+    let local = seed_note(&vault, "note-1", "doc-1");
+    local.insert_scalars(0, "LOCAL").expect("本地编辑");
+    commit_now(&vault, &local, "local-batch");
+
+    let remote = memory_doc("doc-1", B_PEER, "BASE");
+    let snapshot = remote.export_snapshot().expect("基线快照");
+
+    // 首轮：detached 分支（重放计数清零后再调用，断言生产真的进了这个分支）。
+    reset_baseline_apply_debug();
+    let parts = merge_detached(&vault, "doc-1", &snapshot, &local, 5, "cursor-1");
+    let first = baseline_apply_debug_counters();
+    assert_eq!(
+        first.existing_local_extra, 1,
+        "首轮必须命中 detached 分支：{first:?}"
+    );
+    assert_eq!(
+        first.replayed_same_digest, 0,
+        "首轮不应走重放分支：{first:?}"
+    );
+
+    let head_before = head(&vault, "doc-1");
+    let rows_before = baseline_merge_rows(&vault, "doc-1");
+    assert_eq!(
+        rows_before.len(),
+        1,
+        "首轮必须恰好落一条 baseline_merge 差集"
+    );
+
+    // 次轮：同一 snapshot 在同一 staging 再 apply 一次（同一 digest）。
+    apply_baseline_batch(&vault, &[("doc-1".to_owned(), snapshot.clone())], &[])
+        .expect("同一 digest 重放必须成功");
+    let second = baseline_apply_debug_counters();
+    assert_eq!(
+        second.existing_local_extra, 1,
+        "重放不得再进 detached 分支：{second:?}"
+    );
+    assert_eq!(
+        second.replayed_same_digest, 1,
+        "同一 digest 必须走生产既有的 replayed_same_digest 分支：{second:?}"
+    );
+
+    let rows_after = baseline_merge_rows(&vault, "doc-1");
+    assert_eq!(rows_after.len(), 1, "重放不得新增 baseline_merge 行");
+    assert_eq!(
+        rows_after[0].payload, rows_before[0].payload,
+        "重放不得改写差集 payload"
+    );
+    assert_eq!(
+        rows_after[0].end_vv, rows_before[0].end_vv,
+        "重放不得改写差集 VV"
+    );
+    assert_eq!(
+        rows_after[0].end_frontier, rows_before[0].end_frontier,
+        "重放不得改写差集 frontier"
+    );
+
+    let head_after = head(&vault, "doc-1");
+    assert_eq!(
+        head_after.head_seq, head_before.head_seq,
+        "重放不得推进 head_seq"
+    );
+    assert_eq!(
+        head_after.head_vv, head_before.head_vv,
+        "重放不得改 head_vv"
+    );
+    assert_eq!(
+        head_after.head_frontier, head_before.head_frontier,
+        "重放不得改 head_frontier"
+    );
+
+    let text = assert_detached_matches_attached(&vault, "doc-1", &snapshot, &parts);
+    assert_eq!(
+        text.len(),
+        "LOCAL".len() + "BASE".len(),
+        "重放不得重复文本：{text:?}"
+    );
+    assert_eq!(text.matches("LOCAL").count(), 1);
+    assert_eq!(text.matches("BASE").count(), 1);
+
+    // Loro 层：同一 update 导入两次不产生新 op（`import_oplog_only` 依赖的不变量）。
+    let once = BodyDoc::restore(
+        "idem-once",
+        A_PEER,
+        None,
+        std::slice::from_ref(&parts.merged.payload),
+    )
+    .expect("单次导入");
+    let twice = BodyDoc::restore(
+        "idem-twice",
+        A_PEER,
+        None,
+        &[parts.merged.payload.clone(), parts.merged.payload.clone()],
+    )
+    .expect("重复导入");
+    assert_eq!(once.text(), twice.text(), "重复 import 不得改文本");
+    assert_eq!(
+        once.vv().encode(),
+        twice.vv().encode(),
+        "重复 import 不得改 VV"
+    );
+    assert_eq!(
+        once.frontiers().encode(),
+        twice.frontiers().encode(),
+        "重复 import 不得改 frontier"
+    );
+
+    // 数据库层：同一 payload 再落一行（模拟 relay 重复投递），生产读取路径必须幂等。
+    let duplicate = UpdateRecord::new(
+        "duplicate-delivery",
+        PendingUpdate {
+            payload: parts.merged.payload.clone(),
+            end_frontier: parts.merged.end_frontier.clone(),
+            end_vv: parts.merged.end_vv.clone(),
+        },
+    );
+    append_updates(&vault, "doc-1", head_after.head_seq, &[duplicate]).expect("重复投递落库");
+    let (loaded, loaded_head) =
+        load_body_doc(&vault, "doc-1", A_PEER, None).expect("重复投递后重放");
+    assert_eq!(loaded.text(), text, "重复投递不得改文本");
+    assert_eq!(
+        canonical_vv(&loaded.vv().encode()),
+        canonical_vv(&parts.merged.end_vv)
+    );
+    assert_eq!(
+        loaded_head.head_vv, parts.merged.end_vv,
+        "重复投递不得改 head_vv"
+    );
+    assert_eq!(loaded_head.head_frontier, parts.merged.end_frontier);
+    assert_eq!(
+        baseline_merge_rows(&vault, "doc-1").len(),
+        1,
+        "重复投递不得新增 baseline_merge 行"
+    );
+}
+
+// ---------------------------------------------------------------------------
+// 8. 边界：多 peer 物理乱序导入
+// ---------------------------------------------------------------------------
+
+/// 多 peer 物理乱序：D 的 update 先于它依赖的 C、C 又先于它依赖的基线 S0 导入本地
+/// （Loro 先把它们挂成 pending，依赖到齐后一次性应用），然后本地再编辑。之后
+/// detached 导出必须与 attached 参考完全一致，且 merged 差集只含新基线 S1 新产生的
+/// B op——不重复导出本地已导入的 A/C/D op。
+#[test]
+fn detached_multi_peer_out_of_order_imports_match_attached() {
+    let (_dir, vault) = fresh_vault();
+
+    // 基线 S0（peer B）。
+    let remote = memory_doc("doc-1", B_PEER, "BASE");
+    let first = remote.export_snapshot().expect("基线 S0");
+    let first_vv = snapshot_vv(&first);
+
+    // C：在 S0 上插入；D：在 S0 + C 上插入（D 的 op 因果依赖 C 的 op）。
+    let third = BodyDoc::restore("doc-1", C_PEER, Some(&first), &[]).expect("C 文档");
+    third.insert_scalars(0, "CCC").expect("C 文本");
+    let third_update = third.pending_update(&first_vv).expect("C 增量");
+    let fourth = BodyDoc::restore("doc-1", D_PEER, Some(&first), &[]).expect("D 文档");
+    fourth.import(&third_update.payload).expect("D 导入 C");
+    fourth.insert_scalars(0, "DDD").expect("D 文本");
+    let fourth_update = fourth.pending_update(&third.vv()).expect("D 增量");
+
+    // 本地 peer A：物理导入顺序 D → C → S0，与因果顺序完全相反。
+    let local = seed_note(&vault, "note-1", "doc-1");
+    local
+        .import(&fourth_update.payload)
+        .expect("本地先导 D（缺依赖）");
+    local
+        .import(&third_update.payload)
+        .expect("本地再导 C（缺基线）");
+    local.import(&first).expect("本地最后导基线 S0");
+    assert_eq!(
+        local.text().matches("BASE").count(),
+        1,
+        "乱序导入后基线文本必须就位：{:?}",
+        local.text()
+    );
+    assert_eq!(
+        local.text().matches("CCC").count(),
+        1,
+        "乱序导入后 C 文本必须就位：{:?}",
+        local.text()
+    );
+    assert_eq!(
+        local.text().matches("DDD").count(),
+        1,
+        "乱序导入后 D 文本必须就位：{:?}",
+        local.text()
+    );
+    local.insert_scalars(0, "AAA").expect("本地编辑");
+    commit_now(&vault, &local, "local-batch");
+
+    // 远端在 S0 之后继续写：新基线 S1 必须带来新 op（否则 merged 差集测不到内容）。
+    remote
+        .insert_scalars(remote.scalar_len(), "NEW")
+        .expect("远端新文本");
+    let second = remote.export_snapshot().expect("基线 S1");
+    let second_vv = snapshot_vv(&second);
+    let local_vv = local.vv();
+    assert!(
+        local_vv.sub_iter(&second_vv).next().is_some(),
+        "本地必须相对新基线有额外 op（A/C/D）"
+    );
+
+    let parts = merge_detached_with_peers(
+        &vault,
+        "doc-1",
+        &second,
+        &local,
+        7,
+        "cursor-3",
+        &[A_PEER, B_PEER],
+    );
+    // detached 合并后的 oplog VV 必须覆盖全部四个 peer（乱序导入的 A/C/D 与基线的 B）。
+    assert_eq!(
+        vv_peers(&parts.merged.end_vv),
+        vec![A_PEER, B_PEER, C_PEER, D_PEER],
+        "合并后的 oplog VV 必须含全部四个 peer"
+    );
+    let text = assert_detached_matches_attached(&vault, "doc-1", &second, &parts);
+
+    assert_eq!(
+        text.len(),
+        "AAADDDCCCBASENEW".len(),
+        "不得有重复或残留字符：{text:?}"
+    );
+    for marker in ["AAA", "DDD", "CCC", "BASE", "NEW"] {
+        assert_eq!(
+            text.matches(marker).count(),
+            1,
+            "{marker} 必须恰好一次：{text:?}"
+        );
+    }
+
+    // 本地差集：从 0 起算 A/C/D 的 op，且**不含**基线 S0 的 B op（B 已被 from 覆盖）。
+    let delta_meta =
+        LoroDoc::decode_import_blob_meta(&parts.local_delta, true).expect("本地差集元信息");
+    assert_eq!(
+        canonical_of_vv(&delta_meta.partial_start_vv),
+        vec![(A_PEER, 0), (C_PEER, 0), (D_PEER, 0)],
+        "本地差集必须从 A/C/D 的 0 起算"
+    );
+    assert_eq!(
+        canonical_of_vv(&delta_meta.partial_end_vv),
+        vec![
+            (A_PEER, counter_of(&local_vv, A_PEER)),
+            (C_PEER, counter_of(&local_vv, C_PEER)),
+            (D_PEER, counter_of(&local_vv, D_PEER)),
+        ],
+        "本地差集终点必须是 A/C/D 的本地计数"
+    );
+    assert!(
+        canonical_of_vv(&delta_meta.partial_end_vv)
+            .iter()
+            .all(|(peer, _)| *peer != B_PEER),
+        "本地差集不得包含基线 S0 的 op（from 已覆盖）"
+    );
+
+    // merged 差集：起点 = 本地 VV 的 B 计数（S0 已导入），终点 = 新基线 VV，只含 B 的新 op。
+    let merged_meta =
+        LoroDoc::decode_import_blob_meta(&parts.merged.payload, true).expect("merged 元信息");
+    assert_eq!(
+        canonical_of_vv(&merged_meta.partial_start_vv),
+        vec![(B_PEER, counter_of(&local_vv, B_PEER))],
+        "merged 差集必须从本地 VV 的 B 计数起算"
+    );
+    assert_eq!(
+        canonical_of_vv(&merged_meta.partial_end_vv),
+        vec![(B_PEER, counter_of(&second_vv, B_PEER))],
+        "merged 差集终点必须是新基线 VV"
+    );
+    let merged_alone = BodyDoc::restore(
+        "merged-probe",
+        A_PEER,
+        None,
+        std::slice::from_ref(&parts.merged.payload),
+    )
+    .expect("merged 差集可重放");
+    assert!(
+        !merged_alone.text().contains("AAA")
+            && !merged_alone.text().contains("CCC")
+            && !merged_alone.text().contains("DDD"),
+        "merged 差集不得把本地 op 再导出一遍：{merged_alone:?}"
+    );
+}
+
+// ---------------------------------------------------------------------------
+// 9. 边界：大文档（10,000 scalar / 10,000 op）
+// ---------------------------------------------------------------------------
+
+/// 大文档：基线 10,000 字（100 块 × 100 字，逐块提交 = 大量 op），本地另有编辑。
+/// detached merge 后文本必须与 attached 参考逐字节一致（字符数/字节数精确），
+/// 且 merged 差集单独重放恰好还原基线原文（不含本地文本）。
+#[test]
+fn detached_large_document_merge_matches_attached() {
+    let (_dir, vault) = fresh_vault();
+    let local = seed_note(&vault, "note-1", "doc-1");
+    local.insert_scalars(0, "LOCAL").expect("本地编辑");
+    commit_now(&vault, &local, "local-batch");
+
+    // 基线：100 块 × 100 字 = 10,000 个 scalar（每块一次提交）。
+    let remote = BodyDoc::create("doc-1", B_PEER).expect("基线文档");
+    let chunk = "基".repeat(100);
+    for index in 0..100 {
+        let end = remote.scalar_len();
+        remote.insert_scalars(end, &chunk).expect("基线分块写入");
+        assert_eq!(
+            remote.scalar_len(),
+            (index + 1) * 100,
+            "基线长度必须逐块增长"
+        );
+    }
+    let snapshot = remote.export_snapshot().expect("大文档基线快照");
+    let baseline_vv = snapshot_vv(&snapshot);
+    assert_eq!(
+        counter_of(&baseline_vv, B_PEER),
+        10_000,
+        "基线必须是 10,000 个 op"
+    );
+
+    let parts = merge_detached(&vault, "doc-1", &snapshot, &local, 12, "cursor-4");
+    let text = assert_detached_matches_attached(&vault, "doc-1", &snapshot, &parts);
+
+    assert_eq!(
+        text.matches('基').count(),
+        10_000,
+        "基线字符必须恰好 10,000 个"
+    );
+    assert_eq!(text.matches("LOCAL").count(), 1, "本地文本必须恰好一次");
+    assert_eq!(text.chars().count(), 10_005, "scalar 总数必须是 10,000 + 5");
+    assert_eq!(text.len(), 30_005, "UTF-8 字节数必须原样保留（基 3B/字）");
+    assert_eq!(
+        text.replace("LOCAL", ""),
+        "基".repeat(10_000),
+        "除本地前缀外必须与基线逐字节一致"
+    );
+
+    // merged 差集 = 基线相对本地 VV 的差集：单独重放恰好是基线原文（不含 LOCAL）。
+    let merged_alone = BodyDoc::restore(
+        "merged-probe",
+        A_PEER,
+        None,
+        std::slice::from_ref(&parts.merged.payload),
+    )
+    .expect("大差集可重放");
+    assert_eq!(
+        merged_alone.scalar_len(),
+        10_000,
+        "merged 差集必须覆盖全部基线 op"
+    );
+    assert_eq!(
+        merged_alone.text(),
+        "基".repeat(10_000),
+        "merged 差集单独重放必须等于基线原文"
+    );
+    assert_eq!(
+        canonical_vv(&merged_alone.vv().encode()),
+        canonical_of_vv(&baseline_vv),
+        "merged 差集 VV 必须等于基线 VV"
+    );
+}
diff --git a/crates/storage/tests/session_flush.rs b/crates/storage/tests/session_flush.rs
index e4cda3a..907d5c2 100644
--- a/crates/storage/tests/session_flush.rs
+++ b/crates/storage/tests/session_flush.rs
@@ -511,6 +511,53 @@ fn flush_reports_unrecoverable_after_bounded_rebuild() {
 
 /// P1-3 的另一个不可恢复分支：自动重建要读新 head，读不到/解码失败同样必须 fail-closed
 /// （不尝试写、不无限重试），并明确提示需要重开文档。
+/// 永久性 SQLite 错误（约束/ReadOnly/NotFound 等）也不能整类当成 Retryable；
+/// 只有 BUSY/LOCKED 才值得下一次 flush 再试。
+#[test]
+fn flush_treats_permanent_sqlite_error_as_unrecoverable() {
+    let (_dir, vault) = fresh_vault();
+    let _seed = seed_note(&vault, "note-1", "doc-1");
+    let mut session = open_session(&vault, options(PEER_A));
+    session.apply(request(&session, "c1", 0, vec![edit(0, 0, "本地")]));
+
+    vault
+        .exec(
+            "CREATE TRIGGER fail_updates BEFORE INSERT ON loro_updates \
+             BEGIN SELECT RAISE(FAIL, 'permanent sqlite failure'); END",
+            &[],
+        )
+        .expect("创建永久失败触发器");
+
+    let ticket = session.flush(FlushReason::Manual);
+    assert!(
+        session
+            .wait_for_durable(ticket.target_live_rev, Duration::from_secs(1))
+            .is_none(),
+        "永久 SQLite 错误不得发 DurableAck"
+    );
+    let message = session.last_write_error().expect("必须有可读失败原因");
+    assert!(
+        message.contains("永久性") || message.contains("需要重开文档"),
+        "永久 SQLite 错误必须 fail-closed：{message}"
+    );
+    assert_eq!(session.pending_len(), 1, "不可恢复时 journal 必须保留");
+    assert_eq!(head(&vault, "doc-1").head_seq, 0, "整笔事务必须回滚");
+
+    vault
+        .exec("DROP TRIGGER fail_updates", &[])
+        .expect("移除触发器");
+    // 不可恢复后不会自动重试；重开会话才能继续。
+    let mut reopened = open_session(&vault, options(PEER_A));
+    reopened.apply(request(&reopened, "c2", 0, vec![edit(0, 0, "重开")]));
+    let ticket = reopened.flush(FlushReason::Manual);
+    assert!(
+        reopened
+            .wait_for_durable(ticket.target_live_rev, Duration::from_secs(5))
+            .is_some(),
+        "重开会话后应恢复"
+    );
+}
+
 /// 永久性 Corrupt 不能被当成“下次再试”的 Retryable：它不会自愈。
 /// 用 `head_seq = i64::MAX` 构造 `seq = head + 1` 的 `i64_of` 溢出（append_update 返回
 /// Corrupt），断言 flusher 直接进入不可恢复，而不是每次重试同一永久错误。
@@ -734,19 +781,21 @@ fn flush_rebuild_keeps_sync_intent_seq_at_new_head_plus_one() {
 /// 移除注入后同一前缀必须能成功落库。
 #[test]
 fn flush_retryable_write_failure_keeps_prefix_then_recovers() {
-    let (_dir, vault) = fresh_vault();
+    let (dir, vault) = fresh_vault();
     let _seed = seed_note(&vault, "note-1", "doc-1");
     let mut session = open_session(&vault, options_with_sync());
     session.apply(request(&session, "c1", 0, vec![edit(0, 0, "本地")]));
     assert_eq!(session.pending_len(), 1);
 
-    // 注入非 HeadConflict 的写失败：A 事务在 INSERT 处被 SQLite ABORT。
-    let trigger = SqlTrigger::install(
-        &vault,
-        "injected_flush_disk_failure",
-        "CREATE TRIGGER injected_flush_disk_failure BEFORE INSERT ON loro_updates \
-         BEGIN SELECT RAISE(ABORT, 'injected disk'); END;",
-    );
+    // 真实瞬时错误：外部连接持有写锁，且本 Vault 关闭 busy 等待，A 事务拿到
+    // SQLITE_BUSY（不是约束/只读/损坏等永久错误）。
+    vault
+        .query("PRAGMA busy_timeout=0", &[])
+        .expect("关闭 busy 等待");
+    let blocker = rusqlite::Connection::open(vault_path(&dir)).expect("打开外部连接");
+    blocker
+        .execute_batch("BEGIN IMMEDIATE")
+        .expect("外部连接占写锁");
 
     let ticket = session.flush(FlushReason::Manual);
     let message = wait_for_write_error(&session, Duration::from_secs(10));
@@ -756,8 +805,8 @@ fn flush_retryable_write_failure_keeps_prefix_then_recovers() {
         "普通写失败不得升级成不可恢复：{message}"
     );
     assert!(
-        message.contains("injected disk"),
-        "失败原因必须来自注入的写失败：{message}"
+        message.contains("锁") || message.contains("locked") || message.contains("busy"),
+        "失败原因必须来自 BUSY：{message}"
     );
     assert!(
         session
@@ -766,6 +815,12 @@ fn flush_retryable_write_failure_keeps_prefix_then_recovers() {
         "普通写失败不得发出 DurableAck"
     );
 
+    blocker.execute_batch("ROLLBACK").expect("释放外部写锁");
+    drop(blocker);
+    vault
+        .query("PRAGMA busy_timeout=5000", &[])
+        .expect("恢复 busy 等待");
+
     // 前缀原样保留；没有 durable/head/intent 被写。
     assert_eq!(session.pending_len(), 1, "失败后必须保留同一前缀");
     assert_eq!(session.durable_head_seq(), 0);
@@ -782,8 +837,7 @@ fn flush_retryable_write_failure_keeps_prefix_then_recovers() {
     assert_eq!(head(&vault, "doc-1").head_seq, 0, "失败事务不得推进 head");
     assert_eq!(outbox_count(&vault), 0, "失败事务不得留下本地 intent");
 
-    // DROP 触发器后重试：同一前缀成功，水位/正文/intent 都补齐。
-    trigger.remove();
+    // 锁释放后重试：同一前缀成功，水位/正文/intent 都补齐。
     let ticket = session.flush(FlushReason::Manual);
     let ack = session
         .wait_for_durable(ticket.target_live_rev, Duration::from_secs(5))
@@ -825,98 +879,6 @@ fn flush_retryable_write_failure_keeps_prefix_then_recovers() {
 /// 注入点选 `BEFORE INSERT ON loro_updates`：第一次写会在 CAS（第 0 条语句）就因 head
 /// 过期失败，根本到不了 INSERT，因此触发器只命中「重建后的重试」这一次。
 #[test]
-fn flush_rebuild_retryable_write_failure_keeps_prefix_then_recovers() {
-    let (_dir, vault) = fresh_vault();
-    let _seed = seed_note(&vault, "note-1", "doc-1");
-    let mut session = open_session(&vault, options_with_sync());
-
-    session.apply(request(&session, "c1", 0, vec![edit(0, 0, "本地")]));
-    let remote = BodyDoc::create("doc-1", PEER_B).expect("远端副本");
-    remote.insert_scalars(0, "远端").expect("远端编辑");
-    let remote_bytes = remote.export_all_updates().expect("导出远端 update");
-    let remote_update = pending(&remote, &loro::VersionVector::default()).expect("远端增量");
-    session
-        .apply_remote(&remote_bytes)
-        .expect("导入远端 update")
-        .expect("必须有变更");
-
-    // 外部先落同一份 update：head 0→1，让本命令第 1 次写入必然拿到 HeadConflict。
-    append_updates(
-        &vault,
-        "doc-1",
-        0,
-        &[UpdateRecord::new("external-1", remote_update)],
-    )
-    .expect("外部推进 head");
-
-    let trigger = SqlTrigger::install(
-        &vault,
-        "injected_rebuild_retry_disk",
-        "CREATE TRIGGER injected_rebuild_retry_disk BEFORE INSERT ON loro_updates \
-         BEGIN SELECT RAISE(ABORT, 'injected retry disk'); END;",
-    );
-
-    let ticket = session.flush(FlushReason::Manual);
-    let message = wait_for_write_error(&session, Duration::from_secs(10));
-    assert!(
-        message.contains("自动重建后重试前缀失败"),
-        "必须走 RebuildOutcome::Retryable：{message}"
-    );
-    assert!(
-        !message.contains("需要重开文档"),
-        "可重试失败不得升级成不可恢复：{message}"
-    );
-    assert!(
-        session
-            .wait_for_durable(ticket.target_live_rev, Duration::ZERO)
-            .is_none(),
-        "重建后的重试失败不得发出 DurableAck"
-    );
-    assert_eq!(session.pending_len(), 2, "同一前缀必须原样保留");
-    assert_eq!(session.durable_head_seq(), 0);
-    assert_eq!(outbox_count(&vault), 0, "回滚的写事务不得留下 intent");
-    assert_eq!(
-        count(
-            &vault,
-            "SELECT count(*) FROM loro_updates WHERE doc_id = 'doc-1'",
-            &[],
-        ),
-        1,
-        "只有外部那条：重建后的重试整笔回滚"
-    );
-    assert_eq!(head(&vault, "doc-1").head_seq, 1);
-
-    // 移除注入后下一次 flush：仍从旧 CAS 基准开始，先冲突、再自动重建，这次成功。
-    trigger.remove();
-    let ticket = session.flush(FlushReason::Manual);
-    let ack = session
-        .wait_for_durable(ticket.target_live_rev, Duration::from_secs(5))
-        .expect("注入移除后必须收敛到 DurableAck");
-    assert_eq!(ack.through_live_rev, LiveRev(2));
-    assert_eq!(ack.head_seq, 3);
-    assert_eq!(session.pending_len(), 0);
-    assert_eq!(
-        session.last_write_error(),
-        None,
-        "成功写入后必须清空失败原因"
-    );
-    assert_eq!(outbox_count(&vault), 1);
-    let row = outbox_row(&vault, &intent_message_id("doc-1", 2, SYNC_CTX))
-        .expect("读 intent")
-        .expect("重建后的本地 intent");
-    assert_eq!(row.seq, 2);
-    assert_eq!(row.key_epoch, SYNC_CTX.key_epoch);
-}
-
-/// 二次冲突门禁：`auto_rebuilt` 用尽后重试仍撞 head 冲突时，必须 fail-closed 进入
-/// 不可恢复态（提示重开），journal/库原样保留，后续 flush 立即失败不再碰数据库。
-///
-/// 注入 `BEFORE UPDATE OF head_seq ON loro_docs ... RAISE(IGNORE)`：第一次写因为 head 已是
-/// 1（外部推进）而天然 CAS 失败，不触发该触发器；重建后的重试 CAS 命中 `head_seq=1` 时
-/// 被 `RAISE(IGNORE)` 跳过（0 行），稳定复现「同一命令内第二次 HeadConflict」的交错。
-/// `head_seq <= CAS 基准` 的回退分支要求第一次写入失败后 head 倒退，公开接口下
-/// `head_seq` 只前进、无法确定性构造，因此本测试只覆盖“二次冲突”这一支。
-#[test]
 fn flush_reports_unrecoverable_on_second_head_conflict() {
     let (_dir, vault) = fresh_vault();
     let _seed = seed_note(&vault, "note-1", "doc-1");
@@ -1125,6 +1087,10 @@ fn close_report_surfaces_unrecoverable_flush_error() {
         "CloseReport 的原因必须可执行：{close_error}"
     );
     assert!(report.timed_out, "不可恢复时关闭必然超时：{report:?}");
+    assert!(
+        report.needs_reopen(),
+        "不可恢复报告必须被 UI 识别为需要重开"
+    );
     assert_eq!(report.pending_edits, 1, "未 ACK 条目必须原样保留");
     assert!(
         report.durable.is_none(),
diff --git a/crates/storage/tests/structure_crdt.rs b/crates/storage/tests/structure_crdt.rs
index 478dbdf..335c0db 100644
--- a/crates/storage/tests/structure_crdt.rs
+++ b/crates/storage/tests/structure_crdt.rs
@@ -337,3 +337,23 @@ fn snapshot_is_deterministic_across_replays() {
         second.snapshot().expect("快照")
     );
 }
+
+#[test]
+fn deleted_note_remains_in_snapshot_with_note_identity() {
+    let doc = doc(1);
+    let seed = doc.create_note("删了也不物理移除", 1_000).expect("建笔记");
+    doc.delete_note(&seed.node_id, 2_000).expect("删除笔记");
+
+    // 删除是 grow-only meta 事件，不是 LoroTree::delete；snapshot 仍保留节点与
+    // note/body 身份。baseline verify 因此不会把“本地并发删除”误判成结构缺项。
+    let snapshot = doc.snapshot().expect("树快照");
+    let node = snapshot
+        .nodes
+        .iter()
+        .find(|node| node.node_id == seed.node_id)
+        .expect("删除节点仍在快照");
+    assert!(node.deleted, "有效删除必须反映在 snapshot.deleted");
+    assert!(node.is_note(), "deleted 节点仍有 note/body 身份");
+    assert_eq!(node.note_id.as_deref(), Some(seed.note_id.as_str()));
+    assert_eq!(node.body_doc_id.as_deref(), Some(seed.body_doc_id.as_str()));
+}
diff --git a/crates/storage/tests/sync_inbox_batch.rs b/crates/storage/tests/sync_inbox_batch.rs
index dc0352f..9ecaf46 100644
--- a/crates/storage/tests/sync_inbox_batch.rs
+++ b/crates/storage/tests/sync_inbox_batch.rs
@@ -15,10 +15,11 @@ use support::*;
 
 use menghunote_storage::{
     advance_integrated_prefix, append_updates_with_integrated_marks, count_inbox_ready, load_head,
-    mark_integrated, mark_integrated_batch, mark_skipped_event_received, set_sync_log_id,
-    store_inbox_verified, store_inbox_verified_batch, sync_cursor_state, take_inbox_ready,
-    take_inbox_ready_where, StorageError, UpdateRecord, Value, Vault, VerifiedInboxRow,
-    META_SYNC_LOG_ID, META_SYNC_RECEIVED_SEQ, META_SYNC_SKIPPED_RECEIVED_PREFIX,
+    mark_integrated, mark_integrated_batch, mark_skipped_event_received, prune_covered_inbox,
+    set_sync_log_id, store_inbox_verified, store_inbox_verified_batch, sync_cursor_state,
+    take_inbox_ready, take_inbox_ready_where, StorageError, UpdateRecord, Value, Vault,
+    VerifiedInboxRow, META_SYNC_INTEGRATED_SEQ, META_SYNC_LOG_ID, META_SYNC_RECEIVED_SEQ,
+    META_SYNC_SKIPPED_RECEIVED_PREFIX,
 };
 
 const LOG_ID: [u8; 16] = [0x3a; 16];
@@ -1030,3 +1031,46 @@ fn ready_without_log_id_is_empty_and_invalid_length_is_corrupt() {
         "实际 {error:?}"
     );
 }
+
+#[test]
+fn prune_covered_inbox_requires_integrated_watermark() {
+    let (_dir, vault) = fresh_vault();
+    bind_log(&vault);
+    store_inbox_verified_batch(&vault, &rows(3, 0)).expect("写 inbox");
+    assert_eq!(inbox_rows(&vault).len(), 3);
+
+    // integrated=2 < 3：一行都不删。
+    vault
+        .set_meta(META_SYNC_INTEGRATED_SEQ, &2u64.to_be_bytes())
+        .expect("写 integrated");
+    assert_eq!(
+        prune_covered_inbox(&vault, &LOG_ID, 3).expect("prune below watermark"),
+        0,
+        "integrated < S 时绝不能删"
+    );
+    assert_eq!(inbox_rows(&vault).len(), 3);
+
+    // integrated=3：删掉 <=3 的行；别的 incarnation 不受影响。
+    vault
+        .set_meta(META_SYNC_INTEGRATED_SEQ, &3u64.to_be_bytes())
+        .expect("写 integrated");
+    assert_eq!(
+        prune_covered_inbox(&vault, &LOG_ID, 3).expect("prune covered"),
+        3
+    );
+    assert!(inbox_rows(&vault).is_empty());
+
+    // 缺失 integrated 也 fail-closed：不删。
+    vault
+        .exec(
+            "DELETE FROM meta WHERE key=?1",
+            &[Value::Text(META_SYNC_INTEGRATED_SEQ.to_owned())],
+        )
+        .expect("删 integrated");
+    store_inbox_verified_batch(&vault, &rows(1, 3)).expect("再写一条");
+    assert_eq!(
+        prune_covered_inbox(&vault, &LOG_ID, 4).expect("prune without watermark"),
+        0,
+        "没有 integrated 证据时不得删"
+    );
+}
diff --git a/crates/sync/src/bootstrap.rs b/crates/sync/src/bootstrap.rs
index b0261ce..3d663cc 100644
--- a/crates/sync/src/bootstrap.rs
+++ b/crates/sync/src/bootstrap.rs
@@ -202,6 +202,7 @@ pub enum BootstrapFault {
 /// 故障注入伪造的 peer id（只在校验失败测试路径出现；不影响真实数据）。
 const VERIFY_FAULT_PEER: u64 = 0x0BAD_5EED;
 
+#[derive(Debug)]
 struct VerifiedDoc {
     doc_id: String,
     kind: u64,
@@ -211,6 +212,7 @@ struct VerifiedDoc {
     text: Option<String>,
 }
 
+#[derive(Debug)]
 struct VerifiedAsset {
     object_id: [u8; 32],
     real_sha256: [u8; 32],
@@ -220,6 +222,7 @@ struct VerifiedAsset {
 }
 
 /// 选中的基线：manifest/docs/assets + 基线本身 + 结构语义。
+#[derive(Debug)]
 struct SelectedBaseline {
     checkpoint: BootstrapCheckpoint,
     used_fallback: bool,
@@ -229,6 +232,193 @@ struct SelectedBaseline {
     structure_notes: BTreeMap<String, String>,
 }
 
+/// bootstrap 写库阶段对游标/水位的推进策略。
+#[derive(Debug, Clone, Copy, PartialEq, Eq)]
+pub enum BootstrapAdvancement {
+    /// 新设备/普通 bootstrap：只允许前进，已有进度时保留旧游标（max 语义）。
+    MaxOnly,
+    /// Phase 1B 追赶：成功导入并证明 `integrated >= S` 后，把 relay cursor 前移到
+    /// 实际选中 checkpoint 的 cursor（S 覆盖已消费前缀）。
+    CatchUp,
+}
+
+/// `catchup_prepare` 的结果：网络阶段已完成、**尚未写本地库**的基线。
+///
+/// 生命周期只到 `catchup_integrate`；内含快照/资产明文，不能持久化到磁盘/日志。
+#[derive(Debug)]
+pub struct PreparedBootstrap {
+    selected: SelectedBaseline,
+    state: RelayState,
+    chain: Vec<ManagerChainEntry>,
+    control_hash: [u8; 32],
+    pins: BootstrapPins,
+}
+
+impl PreparedBootstrap {
+    /// 实际选中的 checkpoint 覆盖水位 S（fallback 时是 fallback 的 S）。
+    #[must_use]
+    pub fn covered_seq(&self) -> u64 {
+        self.selected.checkpoint.covered_seq
+    }
+
+    /// 实际选中的 checkpoint 游标（fallback 时是 fallback 的 cursor）。
+    #[must_use]
+    pub fn checkpoint_cursor(&self) -> &str {
+        &self.selected.checkpoint.cursor
+    }
+
+    /// 是否使用了 fallback。
+    #[must_use]
+    pub fn used_fallback(&self) -> bool {
+        self.selected.used_fallback
+    }
+
+    /// 已选中的文档/资产数量（用于决策/报告，不暴露明文）。
+    #[must_use]
+    pub fn doc_count(&self) -> usize {
+        self.selected.docs.len()
+    }
+
+    /// 已选中的资产数量。
+    #[must_use]
+    pub fn asset_count(&self) -> usize {
+        self.selected.assets.len()
+    }
+
+    /// 已选中的文档 id（用于追赶后物化/报告；不含明文）。
+    #[must_use]
+    pub fn doc_ids(&self) -> Vec<String> {
+        self.selected
+            .docs
+            .iter()
+            .map(|doc| doc.doc_id.clone())
+            .collect()
+    }
+
+    /// 当前 relay log incarnation（追赶后清理 inbox 必须绑定它）。
+    #[must_use]
+    pub fn log_id(&self) -> [u8; 16] {
+        self.state.log_id
+    }
+
+    /// 快照明文字节估算（文档 Loro snapshot + 资产字节）；调用方按密文上限再乘
+    /// 安全系数，不能把它当成精确的 wire 字节数。
+    #[must_use]
+    pub fn estimated_snapshot_bytes(&self) -> u64 {
+        let docs = self
+            .selected
+            .docs
+            .iter()
+            .map(|doc| u64::try_from(doc.snapshot.loro_snapshot_bytes.len()).unwrap_or(u64::MAX))
+            .fold(0u64, u64::saturating_add);
+        let assets = self
+            .selected
+            .assets
+            .iter()
+            .map(|asset| asset.byte_len)
+            .fold(0u64, u64::saturating_add);
+        docs.saturating_add(assets)
+    }
+}
+
+/// 只下载 manifest 的预检结果（不下载 snapshot/asset，不写库）。
+///
+/// Phase 1B 机会性追赶先用它读实际选中 checkpoint 的 S / 明文字节估算；避免为了
+/// 一个十几 MB 的快照先下载整份数据再回退。
+#[derive(Debug)]
+pub struct BootstrapProbe {
+    checkpoint: BootstrapCheckpoint,
+    used_fallback: bool,
+    estimated_plain_bytes: u64,
+}
+
+impl BootstrapProbe {
+    /// 实际选中 checkpoint 的 S。
+    #[must_use]
+    pub fn covered_seq(&self) -> u64 {
+        self.checkpoint.covered_seq
+    }
+
+    /// 实际选中 checkpoint 的 cursor。
+    #[must_use]
+    pub fn checkpoint_cursor(&self) -> &str {
+        &self.checkpoint.cursor
+    }
+
+    /// 是否使用 fallback。
+    #[must_use]
+    pub fn used_fallback(&self) -> bool {
+        self.used_fallback
+    }
+
+    /// manifest 声明的明文字节（文档 + 资产）；不是 wire 密文字节。
+    #[must_use]
+    pub fn estimated_plain_bytes(&self) -> u64 {
+        self.estimated_plain_bytes
+    }
+}
+
+/// 只拉 manifest 的预检；选择逻辑与 `prepare` 相同，但不下载 snapshot/asset。
+pub fn probe<C: RelayClient>(
+    client: &mut C,
+    vault_id: [u8; 16],
+    root: &RootSecretV1,
+    pins: &BootstrapPins,
+) -> Result<BootstrapProbe, SyncError> {
+    if root.vault_id != vault_id {
+        return Err(SyncError::Object("bootstrap root 与 vault 不符"));
+    }
+    let bundle = client.bootstrap()?;
+    let state = client.state()?;
+    let chain = client.management_chain()?;
+    let control_hash = control_head_hash(&state, &chain);
+    validate_remote(vault_id, root, pins, &bundle, &state, &chain, &control_hash)?;
+
+    let mut last_error: Option<SyncError> = None;
+    for (checkpoint, used_fallback) in [
+        (bundle.primary.clone(), false),
+        (bundle.fallback.clone(), true),
+    ] {
+        let result = (|| {
+            if checkpoint.epoch != state.epoch || checkpoint.epoch != bundle.epoch {
+                return Err(SyncError::Object("bootstrap 基线 epoch 与 relay 不一致"));
+            }
+            if checkpoint.covered_seq > bundle.head_seq || checkpoint.covered_seq > state.head_seq {
+                return Err(SyncError::Object("bootstrap 基线 S 超过 relay head"));
+            }
+            if checkpoint.covered_seq < bundle.gc_floor || checkpoint.covered_seq < state.gc_floor {
+                return Err(SyncError::Object("bootstrap 基线 S 低于 relay GC floor"));
+            }
+            if checkpoint.cursor.is_empty() {
+                return Err(SyncError::Object("bootstrap 基线缺 relay 游标"));
+            }
+            let (manifest, digest) = fetch_manifest(client, root, &checkpoint)?;
+            verify_checkpoint_commit(client, &checkpoint, &manifest, &digest)?;
+            verify_manifest(&manifest, vault_id, &checkpoint, state.epoch)?;
+            let docs = manifest
+                .docs
+                .iter()
+                .map(|doc| doc.total_plain_len)
+                .fold(0u64, u64::saturating_add);
+            let assets = manifest
+                .assets
+                .iter()
+                .map(|asset| asset.byte_len)
+                .fold(0u64, u64::saturating_add);
+            Ok(BootstrapProbe {
+                checkpoint,
+                used_fallback,
+                estimated_plain_bytes: docs.saturating_add(assets),
+            })
+        })();
+        match result {
+            Ok(probe) => return Ok(probe),
+            Err(error) => last_error = Some(error),
+        }
+    }
+    Err(last_error.expect("bootstrap 至少尝试过一个基线"))
+}
+
 /// 用已解出的 root 执行数据 bootstrap；本地 pins 从本库 meta 读取。
 pub fn run<C: RelayClient>(
     vault: &Vault,
@@ -267,15 +457,17 @@ pub fn run_with_fault<C: RelayClient>(
     run_inner(vault, client, vault_id, peer_id, root, &pins, Some(fault))
 }
 
-fn run_inner<C: RelayClient>(
+/// 网络准备阶段：选 checkpoint、下载/解密/验签/校验，**不写本地库**。
+///
+/// Phase 1B 的 `catchup_prepare` 直接复用它；不持写闸。
+pub fn prepare<C: RelayClient>(
     vault: &Vault,
     client: &mut C,
     vault_id: [u8; 16],
     peer_id: u64,
     root: &RootSecretV1,
     pins: &BootstrapPins,
-    fault: Option<BootstrapFault>,
-) -> Result<BootstrapReport, SyncError> {
+) -> Result<PreparedBootstrap, SyncError> {
     if root.vault_id != vault_id {
         return Err(SyncError::Object("bootstrap root 与 vault 不符"));
     }
@@ -294,6 +486,7 @@ fn run_inner<C: RelayClient>(
         (bundle.fallback.clone(), true),
     ] {
         match load_baseline(
+            vault,
             client,
             root,
             vault_id,
@@ -313,12 +506,41 @@ fn run_inner<C: RelayClient>(
         }
     }
     let selected = selected.ok_or_else(|| last_error.expect("bootstrap 至少尝试过一个基线"))?;
+    Ok(PreparedBootstrap {
+        selected,
+        state,
+        chain,
+        control_hash,
+        pins: *pins,
+    })
+}
+
+/// 写库阶段：复用 `baseline_apply` 的 staging journal 与实际导入/读回校验/水位 CAS。
+///
+/// 调用方必须持有外部提交写闸；网络已在 [`prepare`] 完成。
+pub fn integrate(
+    vault: &Vault,
+    peer_id: u64,
+    prepared: &PreparedBootstrap,
+    advancement: BootstrapAdvancement,
+    fault: Option<BootstrapFault>,
+) -> Result<BootstrapReport, SyncError> {
+    let selected = &prepared.selected;
 
     // 实际导入（staging journal 可续跑）；失败时 journal 保留，下一次按同一基线续跑。
-    let report_docs = apply_selected(vault, peer_id, &selected, fault)?;
+    let report_docs = apply_selected(vault, peer_id, selected, fault)?;
 
     // 全部实际写入 + 读回校验通过后，才在一个事务里推进水位（max 语义 + CAS）。
-    finalize(vault, &selected, &state, &chain, &control_hash, pins, fault)?;
+    finalize(
+        vault,
+        selected,
+        &prepared.state,
+        &prepared.chain,
+        &prepared.control_hash,
+        &prepared.pins,
+        advancement,
+        fault,
+    )?;
 
     // 成功后才清 staging journal；若这里失败，重试会幂等重放并再次清理。
     let _cleared = abort_baseline_apply(vault)?;
@@ -329,11 +551,30 @@ fn run_inner<C: RelayClient>(
         used_fallback: selected.used_fallback,
         docs: report_docs,
         assets: selected.assets.len(),
-        cursor: selected.checkpoint.cursor,
+        cursor: selected.checkpoint.cursor.clone(),
         signed_checkpoint: true,
     })
 }
 
+fn run_inner<C: RelayClient>(
+    vault: &Vault,
+    client: &mut C,
+    vault_id: [u8; 16],
+    peer_id: u64,
+    root: &RootSecretV1,
+    pins: &BootstrapPins,
+    fault: Option<BootstrapFault>,
+) -> Result<BootstrapReport, SyncError> {
+    let prepared = prepare(vault, client, vault_id, peer_id, root, pins)?;
+    integrate(
+        vault,
+        peer_id,
+        &prepared,
+        BootstrapAdvancement::MaxOnly,
+        fault,
+    )
+}
+
 /// 校验本地 durable bootstrap 锚（ADR 0013 §3；`validate_remote`/`finalize` 共用）。
 ///
 /// 两类视图必须分开，**不能**把旧 hash 挂到 `max(rev)` 上比较：
@@ -578,7 +819,9 @@ fn push_optional_signature(input: &mut Vec<u8>, signature: Option<[u8; 64]>) {
     }
 }
 
+#[allow(clippy::too_many_arguments)]
 fn load_baseline<C: RelayClient>(
+    vault: &Vault,
     client: &mut C,
     root: &RootSecretV1,
     vault_id: [u8; 16],
@@ -607,7 +850,7 @@ fn load_baseline<C: RelayClient>(
     verify_manifest(&manifest, vault_id, checkpoint, state.epoch)?;
     let docs = download_docs(client, root, peer_id, &manifest)?;
     let structure_notes = verify_structure_semantics(&docs, peer_id)?;
-    let assets = download_assets(client, root, &manifest)?;
+    let assets = download_assets(vault, client, root, &manifest)?;
     Ok(SelectedBaseline {
         checkpoint: checkpoint.clone(),
         used_fallback: false,
@@ -944,12 +1187,28 @@ fn verify_structure_semantics(
 }
 
 fn download_assets<C: RelayClient>(
+    vault: &Vault,
     client: &mut C,
     root: &RootSecretV1,
     manifest: &ManifestV1,
 ) -> Result<Vec<VerifiedAsset>, SyncError> {
     let mut assets = Vec::with_capacity(manifest.assets.len());
     for manifest_asset in &manifest.assets {
+        // 已有同 hash/长度的本地资产不重复下载；只拉缺失的（plan §8.2）。
+        if let Some(local) = menghunote_storage::asset_bytes(vault, manifest_asset.real_sha256)? {
+            if u64::try_from(local.len()).ok() == Some(manifest_asset.byte_len)
+                && <[u8; 32]>::from(Sha256::digest(&local)) == manifest_asset.real_sha256
+            {
+                assets.push(VerifiedAsset {
+                    object_id: manifest_asset.object_id,
+                    real_sha256: manifest_asset.real_sha256,
+                    byte_len: manifest_asset.byte_len,
+                    media_type: manifest_asset.media_type.clone(),
+                    data: local,
+                });
+                continue;
+            }
+        }
         let expected = crate::checkpoint::asset_object_id(
             root,
             &manifest_asset.real_sha256,
@@ -1161,9 +1420,9 @@ fn apply_selected(
                 // 不能把原始 verify 错误替换成 marker 写错误：调用方需要根因；同时
                 // 明确 marker 未持久化意味着 journal 仍可续跑，必须先 abort。
                 return Err(SyncError::Storage(
-                    menghunote_storage::StorageError::Corrupt {
+                    menghunote_storage::StorageError::SyncConflict {
                         context: format!(
-                            "baseline 读回校验失败：{error}；另外写失败标记也失败（{mark_error}），                             journal 仍可续跑，必须先 abort_baseline_apply"
+                            "baseline 读回校验失败：{error}；另外写失败标记也失败（{mark_error}），journal 仍可续跑，必须先 abort_baseline_apply"
                         ),
                     },
                 ));
@@ -1279,6 +1538,7 @@ fn verify_imported(
 /// 只允许前进：`received/integrated = max(现有, S)`、`highwater = max(现有, pin, S)`；
 /// 已有进度时保留现有 relay 游标（不把已消费的位置回退）。`instance/log_id/epoch/
 /// control_rev/control_hash` 只允许前进或不变。任何一项回退都在**写事务之前**硬失败。
+#[allow(clippy::too_many_arguments)]
 fn finalize(
     vault: &Vault,
     selected: &SelectedBaseline,
@@ -1286,6 +1546,7 @@ fn finalize(
     chain: &[ManagerChainEntry],
     control_hash: &[u8; 32],
     pins: &BootstrapPins,
+    advancement: BootstrapAdvancement,
     fault: Option<BootstrapFault>,
 ) -> Result<(), SyncError> {
     let s = selected.checkpoint.covered_seq;
@@ -1357,10 +1618,26 @@ fn finalize(
         return Err(SyncError::LocalCorrupt("本地已有同步水位但缺 relay 游标"));
     }
     // 已有进度时保留现有游标：baseline 的 cursor 只用于"从零开始"的设备，避免回退。
-    let write_cursor = !has_progress;
+    // Phase 1B 追赶例外：导入并读回校验成功后，S 已覆盖旧前缀，允许前移到 checkpoint
+    // cursor（最终仍由后续 `advance_cursor` 条件约束，不能因低 S 回退）。
+    let write_cursor = !has_progress || advancement == BootstrapAdvancement::CatchUp;
 
     let mut statements = Vec::new();
     let mut guards = Vec::new();
+    // 已有进度时不改写 cursor，但仍要 CAS 自写回一次：若并发写者在读完
+    // current_cursor 之后改写了 sync.relay_cursor，这里必须 Conflict，而不是静默
+    // 放过一个已经变化的游标。
+    if !write_cursor {
+        if let Some(cursor) = current_cursor.as_deref() {
+            push_meta_cas_bytes(
+                &mut statements,
+                &mut guards,
+                RELAY_CURSOR_META,
+                Some(cursor),
+                cursor,
+            );
+        }
+    }
     if cursor_state.log_id.is_none() {
         push_meta_insert_absent(
             &mut statements,
@@ -1570,7 +1847,7 @@ fn read_meta_array<const N: usize>(vault: &Vault, key: &str) -> Result<Option<[u
         Some(bytes) => bytes
             .try_into()
             .map(Some)
-            .map_err(|_| SyncError::Object("bootstrap 本地 pin 长度非法")),
+            .map_err(|_| SyncError::LocalCorrupt("bootstrap 本地 pin 长度非法")),
     }
 }
 
@@ -1617,6 +1894,19 @@ mod tests {
         Sha256::digest(refs_digest_input(&manifest_refs(value)).expect("refs")).into()
     }
 
+    #[test]
+    fn read_meta_array_rejects_wrong_length_as_local_corrupt() {
+        let (_dir, vault) = fresh_vault();
+        vault
+            .set_meta(META_BOOTSTRAP_EPOCH, b"short")
+            .expect("写坏 pin");
+        let error = BootstrapPins::from_vault(&vault).expect_err("短 pin 必须拒绝");
+        assert!(
+            matches!(error, SyncError::LocalCorrupt(_)),
+            "pin 长度非法必须是 LocalCorrupt，实际 {error:?}"
+        );
+    }
+
     #[test]
     fn pin_shape_rejects_hash_without_rev_on_both_paths() {
         let pins = BootstrapPins {
diff --git a/crates/sync/src/catchup.rs b/crates/sync/src/catchup.rs
new file mode 100644
index 0000000..2b23a0b
--- /dev/null
+++ b/crates/sync/src/catchup.rs
@@ -0,0 +1,364 @@
+//! Phase 1B：checkpoint 快速追赶（双模式 + prepare/integrate 拆分）。
+//!
+//! 约束见 `docs/notes/sync-fast-catchup-plan.md` §9：
+//! - **Mandatory**：`gc_floor > received_seq`，普通 pull 必然 `CursorExpired`；不受
+//!   机会性快照上限拦截，失败进入明确 `Paused`，禁止回退普通 pull。
+//! - **Opportunistic**：`S - received_seq >= N_min` 且快照密文估算不超过 16 MiB；
+//!   任一不满足走普通聚合 pull。
+//! - 网络/下载/验签在 [`prepare`]（不持写闸）；写库/水位/游标在 [`integrate`]（调用方
+//!   持外部提交闸）。本模块不碰 wire/schema。
+
+use menghunote_storage::{sync_cursor_state, Vault};
+
+use crate::bootstrap::{self, BootstrapPins, PreparedBootstrap};
+use crate::error::SyncError;
+use crate::relay::{RelayClient, RelayState};
+
+/// 机会性追赶的最小事件差（plan §9.1）。
+pub const DEFAULT_N_MIN: u64 = 25_000;
+/// 机会性追赶的快照密文硬上限（plan §9.1）。
+pub const DEFAULT_OPPORTUNISTIC_MAX_SNAPSHOT_BYTES: u64 = 16 * 1024 * 1024;
+/// 明文快照 → 密文的保守安全系数（信封/分片/编码开销）。
+const SNAPSHOT_CIPHERTEXT_FACTOR: u64 = 2;
+
+/// 追赶模式。
+#[derive(Debug, Clone, Copy, PartialEq, Eq)]
+pub enum CatchUpMode {
+    /// 游标已过期，必须用 checkpoint 才能继续。
+    Mandatory,
+    /// 落后足够多且快照成本可接受，主动追赶。
+    Opportunistic,
+}
+
+/// 追赶阈值。
+#[derive(Debug, Clone, Copy, PartialEq, Eq)]
+pub struct CatchUpConfig {
+    /// 机会性追赶的最小事件差。
+    pub n_min: u64,
+    /// 机会性追赶允许的快照密文字节上限。
+    pub opportunistic_max_snapshot_bytes: u64,
+}
+
+impl Default for CatchUpConfig {
+    fn default() -> Self {
+        Self {
+            n_min: DEFAULT_N_MIN,
+            opportunistic_max_snapshot_bytes: DEFAULT_OPPORTUNISTIC_MAX_SNAPSHOT_BYTES,
+        }
+    }
+}
+
+/// 一次追赶决策（只读，不含明文）。
+#[derive(Debug, Clone, Copy, PartialEq, Eq)]
+pub struct CatchUpDecision {
+    /// 强制或机会性。
+    pub mode: CatchUpMode,
+    /// 实际选中 checkpoint 的覆盖水位 S。
+    pub checkpoint_s: u64,
+    /// `S - received_seq`（饱和）。
+    pub gap_events: u64,
+    /// 快照明文字节估算（已乘安全系数）。
+    pub estimated_snapshot_bytes: u64,
+}
+
+/// 是否值得进入网络准备（避免正常近端同步无条件下载整份快照）。
+///
+/// 强制条件用 `gc_floor > received`；机会性先用 `head_seq - received` 做上界，
+/// 真正的 `S - received` 与快照上限在 [`prepare`] 拿到实际 checkpoint 后再判。
+#[must_use]
+pub fn should_attempt(state: &RelayState, local_received_seq: u64, config: &CatchUpConfig) -> bool {
+    state.gc_floor > local_received_seq
+        || state.head_seq.saturating_sub(local_received_seq) >= config.n_min
+}
+
+/// 决策：只依赖 relay state / 本地 received / 实际选中 checkpoint S / 快照估算。
+///
+/// 强制模式不受快照上限拦截；机会性必须同时满足事件差和快照上限。
+#[must_use]
+pub fn decide(
+    state: &RelayState,
+    local_received_seq: u64,
+    checkpoint_s: u64,
+    estimated_snapshot_bytes: u64,
+    config: &CatchUpConfig,
+) -> Option<CatchUpDecision> {
+    let gap_events = checkpoint_s.saturating_sub(local_received_seq);
+    if state.gc_floor > local_received_seq {
+        return Some(CatchUpDecision {
+            mode: CatchUpMode::Mandatory,
+            checkpoint_s,
+            gap_events,
+            estimated_snapshot_bytes,
+        });
+    }
+    if gap_events >= config.n_min
+        && estimated_snapshot_bytes <= config.opportunistic_max_snapshot_bytes
+    {
+        return Some(CatchUpDecision {
+            mode: CatchUpMode::Opportunistic,
+            checkpoint_s,
+            gap_events,
+            estimated_snapshot_bytes,
+        });
+    }
+    None
+}
+
+/// 已下载/校验、尚未写库的追赶候选。
+#[derive(Debug)]
+pub struct CatchUpPrepared {
+    prepared: PreparedBootstrap,
+    decision: CatchUpDecision,
+}
+
+impl CatchUpPrepared {
+    /// 决策。
+    #[must_use]
+    pub fn decision(&self) -> CatchUpDecision {
+        self.decision
+    }
+
+    /// 实际选中 checkpoint 的 S。
+    #[must_use]
+    pub fn checkpoint_s(&self) -> u64 {
+        self.prepared.covered_seq()
+    }
+
+    /// 实际选中 checkpoint 的 cursor。
+    #[must_use]
+    pub fn checkpoint_cursor(&self) -> &str {
+        self.prepared.checkpoint_cursor()
+    }
+
+    /// 是否使用 fallback。
+    #[must_use]
+    pub fn used_fallback(&self) -> bool {
+        self.prepared.used_fallback()
+    }
+
+    /// 已选中文档数。
+    #[must_use]
+    pub fn doc_count(&self) -> usize {
+        self.prepared.doc_count()
+    }
+
+    /// 已选中资产数。
+    #[must_use]
+    pub fn asset_count(&self) -> usize {
+        self.prepared.asset_count()
+    }
+}
+
+/// 追赶写库报告（不含明文）。
+#[derive(Debug, Clone, PartialEq, Eq)]
+pub struct CatchUpReport {
+    /// 模式。
+    pub mode: CatchUpMode,
+    /// 实际选中 checkpoint 的 S。
+    pub checkpoint_s: u64,
+    /// 是否使用 fallback。
+    pub used_fallback: bool,
+    /// 导入文档 id（用于追赶后物化；不含明文）。
+    pub docs: Vec<String>,
+    /// 导入资产数。
+    pub assets: usize,
+    /// 清理的 inbox 行数（仅在 integrated>=S 后）。
+    pub pruned_inbox: usize,
+    /// 写闸内耗时（毫秒）；只记录，不自动中止。
+    pub gate_hold_ms: u64,
+}
+
+/// 网络准备：选 checkpoint、下载/验签/校验，**不写本地库**。
+///
+/// 返回 `None` 表示不需要追赶（普通 pull 更便宜）；调用方应继续普通 pull。
+pub fn prepare<C: RelayClient>(
+    vault: &Vault,
+    client: &mut C,
+    vault_id: [u8; 16],
+    peer_id: u64,
+    root: &crate::keys::RootSecretV1,
+    pins: &BootstrapPins,
+    config: &CatchUpConfig,
+) -> Result<Option<CatchUpPrepared>, SyncError> {
+    let local = sync_cursor_state(vault)?;
+    let state = client.state()?;
+    if state.vault_id != vault_id {
+        return Err(SyncError::RelayWire("state.vault_id 与本地不一致"));
+    }
+    if let Some(log_id) = local.log_id {
+        if log_id != state.log_id {
+            return Err(SyncError::Paused(crate::relay::PauseReason::LogIdChanged));
+        }
+    }
+    if state.epoch != root.key_epoch {
+        return Err(SyncError::Paused(crate::relay::PauseReason::EpochMismatch));
+    }
+
+    // 1) 只拉 manifest 预检：先在真正下载快照之前用实际选中 checkpoint 的 S/明文字节
+    //    判断机会性是否值得；不满足直接回普通 pull，不浪费带宽。
+    let probe = match bootstrap::probe(client, vault_id, root, pins) {
+        Ok(probe) => probe,
+        Err(error) => {
+            if state.gc_floor > local.received_seq {
+                eprintln!("[sync] mandatory catchup probe failed: {error}");
+                return Err(SyncError::Paused(crate::relay::PauseReason::CatchupFailed));
+            }
+            eprintln!("[sync] opportunistic catchup probe failed, fallback to pull: {error}");
+            return Ok(None);
+        }
+    };
+    let probe_estimated = probe
+        .estimated_plain_bytes()
+        .saturating_mul(SNAPSHOT_CIPHERTEXT_FACTOR);
+    if decide(
+        &state,
+        local.received_seq,
+        probe.covered_seq(),
+        probe_estimated,
+        config,
+    )
+    .is_none()
+    {
+        return Ok(None);
+    }
+
+    // 2) 预检通过后才下载 snapshot/缺失资产。fallback 可能和预检不同，实际 S/字节
+    //    必须在下载完成后再判一次；mandatory 仍不受机会性上限拦截。
+    let prepared = match bootstrap::prepare(vault, client, vault_id, peer_id, root, pins) {
+        Ok(prepared) => prepared,
+        Err(error) => {
+            if state.gc_floor > local.received_seq {
+                eprintln!("[sync] mandatory catchup prepare failed: {error}");
+                return Err(SyncError::Paused(crate::relay::PauseReason::CatchupFailed));
+            }
+            eprintln!("[sync] opportunistic catchup prepare failed, fallback to pull: {error}");
+            return Ok(None);
+        }
+    };
+    let actual_estimated = prepared
+        .estimated_snapshot_bytes()
+        .saturating_mul(SNAPSHOT_CIPHERTEXT_FACTOR);
+    let Some(actual_decision) = decide(
+        &state,
+        local.received_seq,
+        prepared.covered_seq(),
+        actual_estimated,
+        config,
+    ) else {
+        // 预检主基线可接受，实际 fallback 超限：丢弃已下载快照，走普通 pull。
+        return Ok(None);
+    };
+    Ok(Some(CatchUpPrepared {
+        prepared,
+        decision: actual_decision,
+    }))
+}
+
+/// 写库：baseline_apply → finish/游标 CAS → 清理已覆盖 inbox。
+///
+/// 调用方必须持有外部提交写闸；网络已在 [`prepare`] 完成。
+pub fn integrate(
+    vault: &Vault,
+    peer_id: u64,
+    prepared: &CatchUpPrepared,
+) -> Result<CatchUpReport, SyncError> {
+    let started = std::time::Instant::now();
+    let report = bootstrap::integrate(
+        vault,
+        peer_id,
+        &prepared.prepared,
+        bootstrap::BootstrapAdvancement::CatchUp,
+        None,
+    )?;
+    let cursor = sync_cursor_state(vault)?;
+    if cursor.integrated_seq < report.covered_seq {
+        return Err(SyncError::LocalCorrupt(
+            "catchup integrate 返回成功但 integrated_seq 未覆盖 checkpoint S",
+        ));
+    }
+    let pruned = menghunote_storage::prune_covered_inbox(
+        vault,
+        &prepared.prepared.log_id(),
+        report.covered_seq,
+    )?;
+    let gate_hold_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
+    if gate_hold_ms > 3_000 {
+        eprintln!("[sync] catchup gate_hold_ms={gate_hold_ms} 超过 3s 预算（只记录，未自动中止）");
+    }
+    Ok(CatchUpReport {
+        mode: prepared.decision.mode,
+        checkpoint_s: report.covered_seq,
+        used_fallback: report.used_fallback,
+        docs: report.docs.into_iter().map(|(doc_id, _)| doc_id).collect(),
+        assets: report.assets,
+        pruned_inbox: pruned,
+        gate_hold_ms,
+    })
+}
+
+#[cfg(test)]
+mod tests {
+    use super::*;
+
+    fn state(head_seq: u64, gc_floor: u64) -> RelayState {
+        RelayState {
+            vault_id: [1; 16],
+            log_id: [2; 16],
+            instance_id: [3; 16],
+            epoch: 1,
+            control_rev: 0,
+            auth_rev: 0,
+            head_seq,
+            gc_floor,
+            current_rotation_id: None,
+        }
+    }
+
+    #[test]
+    fn mandatory_wins_even_when_snapshot_exceeds_opportunistic_cap() {
+        let decision = decide(
+            &state(100_000, 11_000),
+            10_000,
+            90_000,
+            u64::MAX,
+            &CatchUpConfig::default(),
+        )
+        .expect("过期游标必须强制追赶");
+        assert_eq!(decision.mode, CatchUpMode::Mandatory);
+        assert_eq!(decision.gap_events, 80_000);
+    }
+
+    #[test]
+    fn opportunistic_requires_n_min_and_snapshot_cap() {
+        let config = CatchUpConfig::default();
+        let below_min = decide(&state(100_000, 0), 99_000, 99_000, 1, &config);
+        assert!(below_min.is_none(), "事件差不足不得机会性追赶");
+
+        let ok = decide(
+            &state(100_000, 0),
+            70_000,
+            95_000,
+            16 * 1024 * 1024,
+            &config,
+        )
+        .expect("25k 且快照正好 16MiB 应允许");
+        assert_eq!(ok.mode, CatchUpMode::Opportunistic);
+
+        let too_big = decide(
+            &state(100_000, 0),
+            70_000,
+            95_000,
+            16 * 1024 * 1024 + 1,
+            &config,
+        );
+        assert!(too_big.is_none(), "超过机会性快照上限必须回退普通 pull");
+    }
+
+    #[test]
+    fn should_attempt_only_for_expired_or_large_gap() {
+        let config = CatchUpConfig::default();
+        assert!(should_attempt(&state(100_000, 0), 75_000, &config));
+        assert!(!should_attempt(&state(100_000, 0), 90_000, &config));
+        assert!(should_attempt(&state(100_000, 20_000), 10_000, &config));
+    }
+}
diff --git a/crates/sync/src/dev_local.rs b/crates/sync/src/dev_local.rs
index dfe97b5..98a75ec 100644
--- a/crates/sync/src/dev_local.rs
+++ b/crates/sync/src/dev_local.rs
@@ -210,7 +210,7 @@ pub struct RevokedDevice {
 }
 
 /// 两阶段同步 **Prepare** 阶段的报告（encrypt/push/pull；不含 integrate/ack）。
-#[derive(Debug, Clone, Default, PartialEq, Eq)]
+#[derive(Debug, Default)]
 pub struct PrepareReport {
     /// 本轮新加密的本地意图数。
     pub encrypted: usize,
@@ -218,8 +218,10 @@ pub struct PrepareReport {
     pub push: PushReport,
     /// 增量资产上传结果（正文 push 之后、pull 之前）。
     pub assets: AssetSyncReport,
-    /// 拉取结果（游标已 durable 推进）。
+    /// 拉取结果（游标已 durable 推进）；切换为 checkpoint 追赶时为默认值。
     pub pull: PullReport,
+    /// Phase 1B：已准备好、等待持闸的追赶候选；`None` = 本轮走普通聚合 pull。
+    pub catchup: Option<crate::catchup::CatchUpPrepared>,
 }
 
 /// 两阶段同步 **Integrate** 阶段的报告。
@@ -235,6 +237,8 @@ pub struct IntegratePhaseReport {
     pub materialized: usize,
     /// 物化失败篇数（失败只记日志，不影响已 durable 的 CRDT 集成结论）。
     pub materialize_failed: usize,
+    /// Phase 1B：本轮是否执行了 checkpoint 追赶。
+    pub catchup: Option<crate::catchup::CatchUpReport>,
 }
 
 /// 两阶段同步 **Finish** 阶段的报告（push tail + ack）。
@@ -704,6 +708,30 @@ impl DevLocal {
         // pull 必须有游标：B 由 bootstrap 落库，A 由 genesis 落库；旧 relay 两者都没有时
         // 用 `GET /state` 键 14 兜底（绝不把"缺失"当成 0 号游标硬编）。
         ensure_cursor(&mut client, vault)?;
+        // Phase 1B：完整 sync_once 也先尝试 checkpoint 追赶；失败不回退普通 pull
+        // （Mandatory 时普通 pull 只会再次 CursorExpired）。追赶在调用方的
+        // `before_integrate` 守卫之后写入，语义与普通 integrate 相同。
+        let pins = crate::bootstrap::BootstrapPins::from_vault(vault)?;
+        let catchup_config = crate::catchup::CatchUpConfig::default();
+        let local = menghunote_storage::sync_cursor_state(vault)?;
+        let state = client.state()?;
+        if crate::catchup::should_attempt(&state, local.received_seq, &catchup_config) {
+            let prepared = crate::catchup::prepare(
+                vault,
+                &mut client,
+                root.vault_id,
+                identity.loro_peer_id(),
+                &root,
+                &pins,
+                &catchup_config,
+            )?;
+            if let Some(prepared) = prepared {
+                if let Some(guard) = options.before_integrate.as_mut() {
+                    guard()?;
+                }
+                crate::catchup::integrate(vault, identity.loro_peer_id(), &prepared)?;
+            }
+        }
         let engine_identity = EngineIdentity {
             device_id: identity.device_id,
             signing_key,
@@ -743,6 +771,29 @@ impl DevLocal {
         let signing_key = DeviceSigningKey::from_seed(identity.ed25519_seed.clone());
         let mut client = context.open(vault, identity)?.into_client();
         ensure_cursor(&mut client, vault)?;
+        // Phase 1B：网络阶段先决策/下载 checkpoint；不写库、不持闸。
+        // 若决策为 None，继续普通 push + pull（快照可能已下载但未写，属于保守的
+        // 机会性误判；Mandatory 命中时必须跳过普通 pull，否则会立即 CursorExpired）。
+        let pins = crate::bootstrap::BootstrapPins::from_vault(vault)?;
+        let catchup_config = crate::catchup::CatchUpConfig::default();
+        // 近端同步不下载快照：只有强制过期或落后上界达到 N_min 才进入 prepare。
+        let catchup = {
+            let local = menghunote_storage::sync_cursor_state(vault)?;
+            let state = client.state()?;
+            if crate::catchup::should_attempt(&state, local.received_seq, &catchup_config) {
+                crate::catchup::prepare(
+                    vault,
+                    &mut client,
+                    root.vault_id,
+                    identity.loro_peer_id(),
+                    &root,
+                    &pins,
+                    &catchup_config,
+                )?
+            } else {
+                None
+            }
+        };
         let engine_identity = EngineIdentity {
             device_id: identity.device_id,
             signing_key,
@@ -755,12 +806,17 @@ impl DevLocal {
         // ADR 0012 两阶段路径的资产上传点：与完整 `sync_once` 同一阶段（正文之后、
         // pull 之前），保证自动同步（S3）也会增量上传新资产。
         let assets = engine.upload_pending_assets()?;
-        let pull = engine.pull()?;
+        let pull = if catchup.is_some() {
+            PullReport::default()
+        } else {
+            engine.pull()?
+        };
         Ok(PrepareReport {
             encrypted,
             push,
             assets,
             pull,
+            catchup,
         })
     }
 
@@ -796,6 +852,30 @@ impl DevLocal {
         )
     }
 
+    /// 同 [`Self::sync_integrate`]，但携带 Prepare 阶段准备好的 checkpoint 追赶候选。
+    ///
+    /// 追赶写入/水位/游标在持闸段完成；`integrated >= S` 后清理已覆盖 inbox。
+    pub fn sync_integrate_with_catchup<T: RelayTarget + ?Sized>(
+        target: &T,
+        vault: &Vault,
+        identity: &LocalIdentityV1,
+        transport: Option<TransportTimeouts>,
+        expect_head: Option<(&str, u64)>,
+        watch_doc: &str,
+        catchup: Option<&crate::catchup::CatchUpPrepared>,
+    ) -> Result<IntegratePhaseReport, SyncError> {
+        Self::sync_integrate_with_config_and_catchup(
+            target,
+            vault,
+            identity,
+            transport,
+            expect_head,
+            watch_doc,
+            IntegrateConfig::default(),
+            catchup,
+        )
+    }
+
     /// 同 [`Self::sync_integrate`]，但显式指定集成批次预算（测试/诊断用）。
     #[allow(clippy::too_many_arguments)]
     pub fn sync_integrate_with_config<T: RelayTarget + ?Sized>(
@@ -806,6 +886,30 @@ impl DevLocal {
         expect_head: Option<(&str, u64)>,
         watch_doc: &str,
         integrate_config: IntegrateConfig,
+    ) -> Result<IntegratePhaseReport, SyncError> {
+        Self::sync_integrate_with_config_and_catchup(
+            target,
+            vault,
+            identity,
+            transport,
+            expect_head,
+            watch_doc,
+            integrate_config,
+            None,
+        )
+    }
+
+    /// 同 [`Self::sync_integrate_with_config`]，可附加 checkpoint 追赶。
+    #[allow(clippy::too_many_arguments)]
+    pub fn sync_integrate_with_config_and_catchup<T: RelayTarget + ?Sized>(
+        target: &T,
+        vault: &Vault,
+        identity: &LocalIdentityV1,
+        transport: Option<TransportTimeouts>,
+        expect_head: Option<(&str, u64)>,
+        watch_doc: &str,
+        integrate_config: IntegrateConfig,
+        catchup: Option<&crate::catchup::CatchUpPrepared>,
     ) -> Result<IntegratePhaseReport, SyncError> {
         // Integrate 前复核授权基准（ADR 0012 S2b）：head 被推进 = 授权已失效，
         // 一律不 append；inbox/outbox 原样保留，调用方可按新授权重试。
@@ -816,12 +920,25 @@ impl DevLocal {
             }
         }
         let before = load_head(vault, watch_doc)?.head_seq;
+        let peer_id = identity.loro_peer_id();
+        let catchup_report = match catchup {
+            Some(prepared) => match crate::catchup::integrate(vault, peer_id, prepared) {
+                Ok(report) => Some(report),
+                Err(error) => {
+                    if prepared.decision().mode == crate::catchup::CatchUpMode::Mandatory {
+                        eprintln!("[sync] mandatory catchup integrate failed: {error}");
+                        return Err(SyncError::Paused(crate::relay::PauseReason::CatchupFailed));
+                    }
+                    return Err(error);
+                }
+            },
+            None => None,
+        };
         let context = target.relay_context(transport)?;
         let root = root_of(identity);
         let signing_key = DeviceSigningKey::from_seed(identity.ed25519_seed.clone());
         let mut client = context.open(vault, identity)?.into_client();
         ensure_cursor(&mut client, vault)?;
-        let peer_id = identity.loro_peer_id();
         let engine_identity = EngineIdentity {
             device_id: identity.device_id,
             signing_key,
@@ -833,16 +950,25 @@ impl DevLocal {
         // changed heads（正文）触发一次**现有接口**的物化：B 事务写 `notes.body_md`
         // 并发布 `BodyMaterialized` 事件（索引器据此增量更新）。结构投影的事件由
         // `StructureWriter::commit` 自己发布（`StructureProjected`）。
+        let mut changed_heads = outcome.changed_heads;
+        if let Some(report) = &catchup_report {
+            for doc_id in &report.docs {
+                if !changed_heads.iter().any(|head| head == doc_id) {
+                    changed_heads.push(doc_id.clone());
+                }
+            }
+        }
         let (materialized, materialize_failed) =
-            materialize_changed_bodies(vault, peer_id, &outcome.changed_heads);
+            materialize_changed_bodies(vault, peer_id, &changed_heads);
         let after = load_head(vault, watch_doc)?.head_seq;
         let watch_doc_changed = after != before;
         Ok(IntegratePhaseReport {
             integrate: outcome.report,
-            changed_heads: outcome.changed_heads,
+            changed_heads,
             watch_doc_changed,
             materialized,
             materialize_failed,
+            catchup: catchup_report,
         })
     }
 
diff --git a/crates/sync/src/lib.rs b/crates/sync/src/lib.rs
index cf6ee25..6d8a4ec 100644
--- a/crates/sync/src/lib.rs
+++ b/crates/sync/src/lib.rs
@@ -31,6 +31,7 @@ pub mod approval_v1;
 pub(crate) mod asset_sync;
 pub mod base64url;
 pub mod bootstrap;
+pub mod catchup;
 pub mod cbor;
 pub mod checkpoint;
 pub mod control_pin;
diff --git a/crates/sync/src/relay.rs b/crates/sync/src/relay.rs
index 78fb80b..462dc81 100644
--- a/crates/sync/src/relay.rs
+++ b/crates/sync/src/relay.rs
@@ -27,6 +27,8 @@ pub enum PauseReason {
     CursorExpired,
     /// 日志里出现本任务范围外的事件（asset/checkpoint/control），不能静默跳过。
     UnsupportedEvent,
+    /// 游标已过期且 checkpoint 快速追赶失败；不得回退普通 pull（会立即再次过期）。
+    CatchupFailed,
     /// 事件缺少 ObjectCommit 签名（真实 relay 还没在 log 事件里带签名）。
     MissingSignature,
     /// 轮换阶段 A–C 已冻结：禁止再 push/encrypt（A 事务编辑仍可继续）。
@@ -47,6 +49,7 @@ impl core::fmt::Display for PauseReason {
             Self::EpochMismatch => "relay epoch 与本地根不匹配",
             Self::CursorExpired => "游标前缀已被 GC",
             Self::UnsupportedEvent => "日志含本任务范围外的事件类型",
+            Self::CatchupFailed => "checkpoint 快速追赶失败（游标已过期，等待重试）",
             Self::MissingSignature => "日志事件缺少 ObjectCommit 签名",
             Self::RotationFrozen => "轮换已冻结，禁止 push/encrypt",
             Self::RelayHistoryRewritten => {
diff --git a/crates/sync/tests/bootstrap.rs b/crates/sync/tests/bootstrap.rs
index 18069c4..2f50da3 100644
--- a/crates/sync/tests/bootstrap.rs
+++ b/crates/sync/tests/bootstrap.rs
@@ -1118,6 +1118,196 @@ fn control_pin_advances_but_never_rolls_back() {
     assert!(load_body_doc(&target, "note-1", materials.peer_id_u64(), None).is_ok());
 }
 
+#[test]
+fn catchup_prepare_writes_nothing_and_integrate_advances_cursor() {
+    let (_dir, source, materials) = setup();
+    let mut relay = new_relay(&materials);
+    seed_source(&source, &mut relay, &materials);
+    let checkpoint = relay.published_checkpoints[1].clone();
+
+    let (_target_dir, target) = fresh_vault();
+    ensure_structure_row(&target).expect("structure 行");
+    // 目标已有本地编辑 + 旧游标/水位：追赶不得丢本地 op，成功后 cursor 必须前移。
+    let local = seed_doc(&target, "local-note", materials.peer_id_u64());
+    create_note_slot(
+        &target,
+        "local-note",
+        "local-note",
+        "local-note",
+        1_700_000_000,
+    )
+    .expect("notes 行");
+    local.insert_scalars(0, "本地未上传").expect("正文");
+    commit_local(&target, &local);
+    target
+        .set_meta(RELAY_CURSOR_META, b"c0")
+        .expect("旧 cursor");
+    target
+        .set_meta(META_SYNC_RECEIVED_SEQ, &1u64.to_be_bytes())
+        .expect("旧 received");
+    target
+        .set_meta(META_SYNC_INTEGRATED_SEQ, &1u64.to_be_bytes())
+        .expect("旧 integrated");
+    target
+        .set_meta(menghunote_storage::META_SYNC_LOG_ID, &relay.log_id)
+        .expect("log_id");
+
+    let pins = BootstrapPins::from_vault(&target).expect("pins");
+    let prepared = bootstrap::prepare(
+        &target,
+        &mut relay,
+        VAULT_ID,
+        materials.peer_id_u64(),
+        &materials.root,
+        &pins,
+    )
+    .expect("prepare");
+    assert_eq!(prepared.covered_seq(), checkpoint.covered_seq);
+    assert_eq!(
+        target
+            .get_meta(RELAY_CURSOR_META)
+            .expect("cursor")
+            .as_deref(),
+        Some(b"c0".as_slice()),
+        "prepare 不得写库/推进 cursor"
+    );
+    assert_eq!(
+        target
+            .get_meta(META_SYNC_INTEGRATED_SEQ)
+            .expect("integrated")
+            .as_deref(),
+        Some(1u64.to_be_bytes().as_slice()),
+        "prepare 不得推进 integrated"
+    );
+
+    let expected_cursor = prepared.checkpoint_cursor().to_owned();
+    let report = bootstrap::integrate(
+        &target,
+        materials.peer_id_u64(),
+        &prepared,
+        bootstrap::BootstrapAdvancement::CatchUp,
+        None,
+    )
+    .expect("integrate");
+    assert_eq!(report.covered_seq, checkpoint.covered_seq);
+    assert_eq!(
+        target
+            .get_meta(RELAY_CURSOR_META)
+            .expect("cursor")
+            .as_deref(),
+        Some(expected_cursor.as_bytes()),
+        "CatchUp 成功后 cursor 必须前移到实际选中 checkpoint"
+    );
+    let cursor = sync_cursor_state(&target).expect("cursor state");
+    assert!(cursor.integrated_seq >= checkpoint.covered_seq);
+    let local_text = load_body_doc(&target, "local-note", materials.peer_id_u64(), None)
+        .expect("本地文档")
+        .0
+        .text();
+    assert!(
+        local_text.contains("本地未上传"),
+        "本地未上传 op 不得丢：{local_text}"
+    );
+
+    // S 之后普通 pull：源在 checkpoint 后新增一条，目标用追赶后的 cursor 正常增量拉取。
+    let (source_doc, _) =
+        load_body_doc(&source, "note-1", materials.peer_id_u64(), None).expect("source note");
+    source_doc
+        .insert_scalars(0, "after-catchup ")
+        .expect("source edit");
+    commit_with_intent(&source, &source_doc, &uuid_like(9_777), [0xC7; 32], EPOCH);
+    {
+        let mut engine = SyncEngine::new(
+            &source,
+            &mut relay,
+            EngineIdentity {
+                device_id: materials.device_id,
+                signing_key: materials.signing_key.clone(),
+                peer_id: materials.peer_id_u64(),
+            },
+            materials.root.clone(),
+        );
+        engine.sync_once().expect("source push after catchup");
+    }
+    {
+        let mut engine = SyncEngine::new(
+            &target,
+            &mut relay,
+            EngineIdentity {
+                device_id: materials.device_id,
+                signing_key: materials.signing_key.clone(),
+                peer_id: materials.peer_id_u64(),
+            },
+            materials.root.clone(),
+        );
+        engine.sync_once().expect("target pull after catchup");
+    }
+    let target_note = load_body_doc(&target, "note-1", materials.peer_id_u64(), None)
+        .expect("target note")
+        .0
+        .text();
+    assert!(
+        target_note.contains("after-catchup"),
+        "追赶后必须能从 S 继续普通 pull：{target_note}"
+    );
+}
+
+#[test]
+fn catchup_mandatory_prepare_and_integrate_advances_through_public_wrapper() {
+    let (_dir, source, materials) = setup();
+    let mut relay = new_relay(&materials);
+    seed_source(&source, &mut relay, &materials);
+    let checkpoint = relay.published_checkpoints[1].clone();
+
+    let (_target_dir, target) = fresh_vault();
+    ensure_structure_row(&target).expect("structure 行");
+    target
+        .set_meta(RELAY_CURSOR_META, b"c0")
+        .expect("旧 cursor");
+    target
+        .set_meta(META_SYNC_RECEIVED_SEQ, &0u64.to_be_bytes())
+        .expect("旧 received");
+    target
+        .set_meta(META_SYNC_INTEGRATED_SEQ, &0u64.to_be_bytes())
+        .expect("旧 integrated");
+    target
+        .set_meta(menghunote_storage::META_SYNC_LOG_ID, &relay.log_id)
+        .expect("log_id");
+    // 让普通 pull 必然 CursorExpired：本地 received=0，relay floor=1。
+    relay.gc_floor = 1;
+    let pins = BootstrapPins::from_vault(&target).expect("pins");
+    let prepared = menghunote_sync::catchup::prepare(
+        &target,
+        &mut relay,
+        VAULT_ID,
+        materials.peer_id_u64(),
+        &materials.root,
+        &pins,
+        &menghunote_sync::catchup::CatchUpConfig::default(),
+    )
+    .expect("mandatory prepare")
+    .expect("gc_floor > received 必须进入 mandatory");
+    assert_eq!(
+        prepared.decision().mode,
+        menghunote_sync::catchup::CatchUpMode::Mandatory
+    );
+    assert_eq!(prepared.checkpoint_s(), checkpoint.covered_seq);
+    let expected_cursor = prepared.checkpoint_cursor().to_owned();
+    let report = menghunote_sync::catchup::integrate(&target, materials.peer_id_u64(), &prepared)
+        .expect("mandatory integrate");
+    assert_eq!(
+        report.mode,
+        menghunote_sync::catchup::CatchUpMode::Mandatory
+    );
+    assert_eq!(
+        target
+            .get_meta(RELAY_CURSOR_META)
+            .expect("cursor")
+            .as_deref(),
+        Some(expected_cursor.as_bytes())
+    );
+}
+
 #[test]
 fn bootstrap_cursor_pulls_updates_after_s() {
     let (_dir, source, materials) = setup();
diff --git a/crates/sync/tests/phase1a_release.rs b/crates/sync/tests/phase1a_release.rs
index c445663..d73fa91 100644
--- a/crates/sync/tests/phase1a_release.rs
+++ b/crates/sync/tests/phase1a_release.rs
@@ -18,6 +18,10 @@
 //! 只打印页数/墙钟，不 assert 时间。1M 档 = 4096 页，是生产
 //! `COVERAGE_SCAN_MAX_PAGES` 的上限，只跑 1 次（常规档 3 次取中位数）。
 //!
+//! 同一上限还有一个**常规测试** [`coverage_scan_rejects_more_than_max_pages`]：覆盖日志
+//! 4097 页时 validate 必须以「页数超过上限」失败，不能静默截断/当成完整覆盖
+//! （FakeRelay 日志 + 真实 `CheckpointWriter::validate`，不跑网络）。
+//!
 //! 第三个探针 [`large_doc_per_page_replay_cost`]（M3 标定）测 **一篇 1 MiB 大正文的
 //! per-page replay 成本**：每次 aggregate 页都会为每个 touched doc 全量
 //! `load_body_doc`；该探针连续重放 5 次并打印 raw/中位数，另测 snapshot 导出/恢复与
@@ -50,8 +54,8 @@ use menghunote_sync::engine::RELAY_CURSOR_META;
 use menghunote_sync::genesis;
 use menghunote_sync::relay::{
     BootstrapBundle, BootstrapLease, CheckpointLease, CheckpointPublish, CheckpointRef,
-    ObjectSubmission, PublishedCheckpoint, RelayClient, RelayLogKind, RelayLogPage, RelayState,
-    SlotRecord, SlotUpload, UpdateOutcome, UpdateSubmission,
+    ObjectSubmission, PublishedCheckpoint, RelayClient, RelayLogEvent, RelayLogKind, RelayLogPage,
+    RelayState, SlotRecord, SlotUpload, UpdateOutcome, UpdateSubmission,
 };
 use menghunote_sync::{DeviceId, ObjectId, SyncError};
 use menghunote_sync_proto::DeviceCertV1;
@@ -957,6 +961,304 @@ fn checkpoint_validate_scan_cost() {
     }
 }
 
+// ============================================================ coverage 扫描页数上限（常规测试）
+//
+// 生产 `scan_coverage_log` 的循环上界是 `COVERAGE_SCAN_MAX_PAGES = 4096` 页
+// （`crates/sync/src/checkpoint.rs` 私有常量；每页 `COVERAGE_SCAN_PAGE_LIMIT = 256` 条）。
+// 覆盖日志一旦超过 4096 页，扫描必须**失败**，不能把"没扫完"当成"已证明覆盖到 S"。
+//
+// 构造：S = 4096 × 256 + 1 = 1_048_577 条日志（4096 个满页 + 1 条尾页）。本地
+// integrated 前缀 = S-1（1..=S-1 已消费），唯一缺口是第 S 条，持久游标是 c0，
+// 于是 validate 必须从 c0 连续扫到 S：需要 4097 页，而第 4096 页结束时只到 S-1。
+//
+// prepare 侧：`try_record_coverage_receipt` 在"本地没有 relay 游标"时直接返回（不写
+// pending receipt），所以夹具在 prepare 前**不写** `sync.relay_cursor`，prepare 成功
+// 之后才写 c0。否则 prepare 会先撞同一上限，测到的就不是 validate 的守卫。
+//
+// 页请求次数必须正好是 4096（= `COVERAGE_SCAN_MAX_PAGES`）：守卫按**页数**生效，
+// 与每页事件数无关；这也是"上限是 4096 页"的可执行证据（生产错误文案本身不带数字）。
+
+/// 生产 `COVERAGE_SCAN_MAX_PAGES`（私有常量）。生产上限改动时这里必须同步。
+const PAGE_LIMIT_MAX_PAGES: u64 = 4096;
+/// 覆盖日志事件总数：4096 个满页（每页 `SCAN_COST_PAGE_LIMIT` 条）+ 1 条尾页 = 4097 页。
+const PAGE_LIMIT_S: usize = (PAGE_LIMIT_MAX_PAGES as usize) * SCAN_COST_PAGE_LIMIT + 1;
+/// 夹具正文文档 id（与 coverage 无关，只给 prepare 提供可导出的本地快照）。
+const PAGE_LIMIT_NOTE_ID: &str = "coverage-page-limit-note";
+
+/// `log()` 的 O(page) 实现：FakeRelay 的 `log` 每页都全量扫描 `self.log`
+/// （O(S²/256) 夹具 artifact，1M 事件下不可接受）；真实 relay 分页是索引查找。
+/// 其余方法全部委托给 FakeRelay，事件仍来自同一个 `log`。
+struct PageIndexRelay {
+    inner: FakeRelay,
+    log_calls: u64,
+}
+
+impl PageIndexRelay {
+    fn new(inner: FakeRelay) -> Self {
+        Self {
+            inner,
+            log_calls: 0,
+        }
+    }
+}
+
+impl RelayClient for PageIndexRelay {
+    fn state(&mut self) -> Result<RelayState, SyncError> {
+        self.inner.state()
+    }
+
+    fn log(&mut self, cursor: Option<&str>, limit: u32) -> Result<RelayLogPage, SyncError> {
+        self.log_calls += 1;
+        let after = cursor
+            .ok_or(SyncError::RelayWire("缺少游标"))?
+            .strip_prefix('c')
+            .and_then(|value| value.parse::<u64>().ok())
+            .ok_or(SyncError::RelayWire("游标格式非法"))?;
+        let start = self
+            .inner
+            .log
+            .partition_point(|event| event.relay_seq <= after);
+        let end = start
+            .saturating_add(usize::try_from(limit).expect("页大小溢出 usize"))
+            .min(self.inner.log.len());
+        let events = self.inner.log[start..end].to_vec();
+        let last = events.last().map_or(after, |event| event.relay_seq);
+        Ok(RelayLogPage {
+            events,
+            next_cursor: self.inner.cursor_for(last),
+            through_seq: last,
+            query_head: self.inner.head_seq,
+            epoch: self.inner.epoch,
+            gc_floor: self.inner.gc_floor,
+        })
+    }
+
+    fn part(&mut self, object_id: &ObjectId, part_no: u32) -> Result<Vec<u8>, SyncError> {
+        self.inner.part(object_id, part_no)
+    }
+
+    fn put_part(
+        &mut self,
+        object_id: &ObjectId,
+        part_no: u32,
+        envelope: &[u8],
+    ) -> Result<(), SyncError> {
+        self.inner.put_part(object_id, part_no, envelope)
+    }
+
+    fn submit_updates(
+        &mut self,
+        items: Vec<UpdateSubmission>,
+    ) -> Result<Vec<UpdateOutcome>, SyncError> {
+        self.inner.submit_updates(items)
+    }
+
+    fn submit_object(&mut self, item: ObjectSubmission) -> Result<UpdateOutcome, SyncError> {
+        self.inner.submit_object(item)
+    }
+
+    fn ack(&mut self, received_seq: u64, integrated_seq: u64, epoch: u64) -> Result<(), SyncError> {
+        self.inner.ack(received_seq, integrated_seq, epoch)
+    }
+
+    fn device_cert(&mut self, device_id: &DeviceId) -> Result<Option<DeviceCertV1>, SyncError> {
+        self.inner.device_cert(device_id)
+    }
+
+    fn put_key_slot(&mut self, upload: SlotUpload) -> Result<SlotRecord, SyncError> {
+        self.inner.put_key_slot(upload)
+    }
+
+    fn list_key_slots(&mut self) -> Result<Vec<SlotRecord>, SyncError> {
+        self.inner.list_key_slots()
+    }
+
+    fn activate_password_slot(
+        &mut self,
+        expected_slot_set_rev: u64,
+        slot_id: ObjectId,
+        signed_message: Vec<u8>,
+        signature: [u8; 64],
+    ) -> Result<u64, SyncError> {
+        self.inner
+            .activate_password_slot(expected_slot_set_rev, slot_id, signed_message, signature)
+    }
+
+    fn create_checkpoint(
+        &mut self,
+        estimated_bytes: u64,
+        epoch: Option<u64>,
+    ) -> Result<CheckpointLease, SyncError> {
+        self.inner.create_checkpoint(estimated_bytes, epoch)
+    }
+
+    fn add_checkpoint_refs(
+        &mut self,
+        checkpoint_id: [u8; 16],
+        refs: Vec<CheckpointRef>,
+    ) -> Result<u64, SyncError> {
+        self.inner.add_checkpoint_refs(checkpoint_id, refs)
+    }
+
+    fn publish_checkpoint(
+        &mut self,
+        publish: CheckpointPublish,
+    ) -> Result<PublishedCheckpoint, SyncError> {
+        self.inner.publish_checkpoint(publish)
+    }
+
+    fn bootstrap(&mut self) -> Result<BootstrapBundle, SyncError> {
+        self.inner.bootstrap()
+    }
+
+    fn renew_bootstrap_lease(&mut self, lease_id: [u8; 16]) -> Result<BootstrapLease, SyncError> {
+        self.inner.renew_bootstrap_lease(lease_id)
+    }
+}
+
+/// 一次性夹具：真实 SQLite vault + S 条合成日志 + 已 prepare 的 checkpoint。
+struct PageLimitFixture {
+    _dir: tempfile::TempDir,
+    vault: Vault,
+    materials: genesis::GenesisMaterials,
+    relay: PageIndexRelay,
+    prepared: PreparedCheckpoint,
+}
+
+fn build_page_limit_fixture() -> PageLimitFixture {
+    let total = u64::try_from(PAGE_LIMIT_S).expect("S 溢出 u64");
+    let (dir, vault) = fresh_vault();
+    ensure_structure_row(&vault).expect("structure 行");
+    let materials = genesis::build_materials(
+        VAULT_ID,
+        [0x45; 16],
+        "pw",
+        1_700_000_000,
+        &mut TestRandom::new(0x88_15),
+    )
+    .expect("genesis 材料");
+    let mut relay = FakeRelay::new(&[]);
+    relay
+        .certs
+        .insert(materials.device_id, materials.certificate().clone());
+    vault
+        .set_meta(META_SYNC_LOG_ID, &relay.log_id)
+        .expect("写 log_id");
+
+    // 一篇真实正文：prepare 需要可导出的本地快照/head VV（与 coverage 无关）。
+    let doc = seed_doc(&vault, PAGE_LIMIT_NOTE_ID, materials.peer_id_u64());
+    create_note_slot(
+        &vault,
+        PAGE_LIMIT_NOTE_ID,
+        PAGE_LIMIT_NOTE_ID,
+        PAGE_LIMIT_NOTE_ID,
+        1_700_000_000,
+    )
+    .expect("notes 行");
+    doc.insert_scalars(0, "checkpoint coverage scan page limit")
+        .expect("正文");
+    commit_with_intent(&vault, &doc, &uuid_like(9_101), [0x92; 32], EPOCH);
+
+    // S 条 UPDATE 日志。扫描只核对序号/epoch/游标连续性；缺口在 S 之前一律被
+    // visitor 跳过，事件负载不参与判定，因此不物化 32B 的 `event` 控制夹具内存。
+    relay.log.reserve(PAGE_LIMIT_S);
+    for seq in 1..=total {
+        relay.log.push(RelayLogEvent {
+            relay_seq: seq,
+            kind: RelayLogKind::Update,
+            object_id: Some([0x15; 32]),
+            epoch: EPOCH,
+            event: Vec::new(),
+            signature: None,
+        });
+    }
+    relay.head_seq = total;
+
+    // 本地已消费 1..=S-1：唯一缺口是第 S 条（prepare 因此不写 receipt）。
+    let consumed = total - 1;
+    vault
+        .set_meta(META_SYNC_INTEGRATED_SEQ, &consumed.to_be_bytes())
+        .expect("写 integrated 前缀");
+    vault
+        .set_meta(META_SYNC_RECEIVED_SEQ, &consumed.to_be_bytes())
+        .expect("写 received 前缀");
+
+    // prepare 前**不**写 relay 游标：`try_record_coverage_receipt` 会跳过 pending
+    // receipt 记录（否则 prepare 自己就会先撞 4096 页上限，测不到 validate 的守卫）。
+    let mut relay = PageIndexRelay::new(relay);
+    let prepared = {
+        let mut writer = CheckpointWriter::new(
+            &vault,
+            &mut relay,
+            VAULT_ID,
+            materials.device_id,
+            materials.signing_key.clone(),
+            materials.root.clone(),
+            materials.peer_id_u64(),
+        );
+        writer.prepare(&EmptyAssets).expect("prepare")
+    };
+    assert_eq!(
+        prepared.report.covered_seq, total,
+        "S 必须等于日志条数（relay head）"
+    );
+    assert_eq!(
+        relay.log_calls, 0,
+        "prepare 不应扫描 coverage 日志（夹具在 prepare 前不写游标）"
+    );
+
+    // validate 的扫描起点：持久游标 c0（1..=S 全在游标之后）。
+    vault
+        .set_meta(RELAY_CURSOR_META, b"c0")
+        .expect("写 relay 游标");
+
+    PageLimitFixture {
+        _dir: dir,
+        vault,
+        materials,
+        relay,
+        prepared,
+    }
+}
+
+#[test]
+fn coverage_scan_rejects_more_than_max_pages() {
+    let mut fixture = build_page_limit_fixture();
+    let error = {
+        let mut writer = CheckpointWriter::new(
+            &fixture.vault,
+            &mut fixture.relay,
+            VAULT_ID,
+            fixture.materials.device_id,
+            fixture.materials.signing_key.clone(),
+            fixture.materials.root.clone(),
+            fixture.materials.peer_id_u64(),
+        );
+        writer
+            .validate(&fixture.prepared)
+            .expect_err("超过 4096 页必须失败，不能静默截断/当成完整覆盖")
+    };
+    // 守卫按页数生效：正好 4096 次页请求（= COVERAGE_SCAN_MAX_PAGES），
+    // 第 4096 页结束时 through_seq = S-1 < S，循环耗尽后必须报错。
+    assert_eq!(
+        fixture.relay.log_calls, PAGE_LIMIT_MAX_PAGES,
+        "coverage scan 必须正好用满 {PAGE_LIMIT_MAX_PAGES} 页上限（页大小 {SCAN_COST_PAGE_LIMIT}）"
+    );
+    match error {
+        SyncError::Object(message) => {
+            assert!(
+                message.contains("页数超过上限"),
+                "错误文案必须指出页数超限：{message}"
+            );
+            assert!(
+                message.contains("未证明覆盖到 S"),
+                "错误文案必须说明覆盖未证明：{message}"
+            );
+        }
+        other => panic!("期望 SyncError::Object（页数超限），实际 {other:?}"),
+    }
+}
+
 // ============================================================ M3 大文档 per-page replay 成本
 //
 // M3 标定：aggregate 每页（每批 pull）会为每个 **touched doc** 全量 `load_body_doc`

`````
