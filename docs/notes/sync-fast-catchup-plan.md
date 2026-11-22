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
   update 大小，另加单次快照硬上限（建议 128 MiB）。
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
