# 0005 同步/加密 v1 设计与开发计划（供 GPT 评审）

> **已归档（2026-11-22 文档整理）**：本文被判定为「已作废 / 被取代 / 已完成的待办」，移入 `docs/outdated/` 保留追溯，**不作为当前状态依据**。归档说明见 [`docs/outdated/README.md`](README.md)。

> 2026-11-22 历史整理：本文是整理前的历史文档，文中旧 commit 锚点已清除（原哈希不再对应新 `main`）；原始版本见 `backup/dev-20261122`，旧→新映射见 `docs/notes/history-rewrite-20261122.md`。


> 历史文档：以下 commit 哈希属于 2026-11-16 整理前历史；映射与 backup 分支见 [history-rewrite-20261116](../notes/history-rewrite-20261116.md)。


> **历史留档**：本文写于 2026-11-12 历史整理前；文中哈希（如 旧基线、旧基线）只在
> `backup/dev-20261113` 可达，见 [`docs/notes/history-rewrite-20261113.md`](../notes/history-rewrite-20261113.md)。

> 状态：2026-11-12，分支 `dev`，HEAD 旧基线（restart 执行链）；全仓测试在 旧基线 基线绿，之后 sync 新增 restart/repack/bugfix 在途。
> 关联：[ADR 0006](../adr/0006-sync-v1.md)（§1–§30）、[评审 0001](../reviews/0001-rotation-security-review.md)、[ADR 0001](../adr/0001-storage-format.md)、[ADR 0003](../adr/0003-module-boundaries.md)。

## 1. 目标与非目标

### 1.1 v1 目标
1. **多设备同步**：桌面端（Windows 优先）多台设备共享笔记库；正文/结构用 Loro CRDT，本地 SQLite 单库为真源。
2. **自建 relay**：用户可自托管密文中继；relay 不见明文、密钥、搜索索引。
3. **E2E 加密**：密码 + 恢复码两种解锁路径；relay 只存密文与不透明元数据。
4. **设备吊销与密钥轮换**：吊销设备后，旧 epoch 密文不能继续写；新设备可恢复；离线设备可重新取 grant。
5. **崩溃/离线安全**：任何阶段崩溃不产生"半库"或"静默丢操作"；离线编辑不因轮换丢失。

### 1.2 v1 非目标（明确后置）
- 多用户共享/权限模型（v1 单用户多设备）。
- 服务端搜索、服务端明文处理。
- 跨平台（macOS/Linux）正式验收。
- 资产孤儿自动 GC、Merkle inclusion proof、第二设备见证（评审允许后置，但不能替代双基线独立/双签/outbox CAS）。
- app 内的完整同步管理 UI（v1 先命令行/开发服务器验收，UI 紧随）。

## 2. 威胁模型与信任边界

| 对手 | 能力 | 我们的对策 |
| --- | --- | --- |
| 被动网络 | 看流量 | TLS（v1 开发期 HTTP 仅 localhost；生产必须 TLS） |
| 恶意 relay | 读写/删库、返回伪造响应、重放、回滚 manifest | 客户端验签名/摘要/manifest refs_digest；只信已签控制链；回滚检测有边界（§11.4） |
| 偷到磁盘的本地攻击者 | 读 SQLite/文件 | DPAPI CurrentUser 包根/种子；relay 密文；本地明文按 ADR 0001 决策 |
| 被吊销设备 | 旧 token/旧 grant/旧 root | `revoked_once` 永久事实、auth_rev、epoch 轮换、新槽/新 grant、旧密文 7 天 GC |
| 恶意/错误客户端 | 伪造管理请求 | canonical body 单源 + 管理签名 + request_id 幂等 + CAS |
| 内部实现错误 | 半成品状态/绕过 | 事务边界、候选 lifecycle、双基线独立、T01–T22 测试矩阵 |

**relay 不运行 Loro、不解密**是正确边界：它只能验证密文完整性/签名/元数据，不能证明"实际历史覆盖 S"；后者由管理客户端在两个互不共享的空上下文真实 import 完成。

## 3. 密钥与身份体系

### 3.1 层次
```
password ──Argon2id(m=64MiB,t=3,p=1,salt16)──> KEK ──wrap──> WrappedRoot (KEY_SLOT)
recovery seed R (32B, Bech32m mnrec) ──> K_recovery ──wrap──> WrappedRoot (KEY_SLOT)
K_epoch root (32B, 每 epoch 随机)
  └─ HKDF-SHA256(per-domain) ──> object keys（UPDATE/SNAPSHOT/MANIFEST/ASSET）
  └─ HKDF-SHA256(per-domain) ──> key-slot 包裹键 K_slot_mne
management key: Ed25519（genesis 或由 R' 派生）
device key: Ed25519 签名 + X25519 HPKE（grant 封 root）
DPAPI(CurrentUser) 本地包 root/seed/恢复码
```

### 3.2 关键约束
- `K_slot_mne = HKDF(IKM = Argon2id(password) | R, salt = vault_id, info = "MN/v1/key-slot\0" || kind_u8 || BE64(epoch) || slot_id32, 32)`（ADR §20）。
- `key_epoch` 与 Loro `generation` **正交**；轮换不改 generation。
- 恢复码派生管理 key；恢复码丢失 = 失去管理权（但可用已授权设备继续）。
- 新 password 必须是**新秘密**；`R'=random32`；不是"旧 password 加新 salt"。

## 4. 数据格式与会话

### 4.1 MNE1 信封
- 196B header；AAD = `b"MN/e2e/v1\0" || header[0..196] || BE32(desc_len) || desc`；
- 单字节 NUL 域分隔；CDE（确定性 CBOR，整数键、ID 为 byte string）；
- 对象类型：UPDATE/SNAPSHOT/MANIFEST/ASSET/KEY_SLOT；上限 4MiB（ASSET）/1MiB（其他）。

### 4.2 基线
- 两份 checkpoint（primary/fallback），各自 manifest + snapshot；
- `refs_digest = SHA256(CDE(sorted ObjectRefV1))`，manifest 字段（含 `key_epoch`/`total_plain_len`）足以客户端重算（ADR §21）；
- 签名 `CheckpointCommit` 由设备 key 签；bootstrap 必须验证 manifest/commit/refs/所有对象，再写库。

### 4.3 控制链
- typed `SignedControlRecord`（kind：rotation.begin/abort、epoch.activated、rotation.restarted、rotation.revoked-more）；
- 管理转换 `ManagerTransitionV1`（9 字段）旧/新管理 key 双签；
- `/control` 输出管理转换链 + 公钥链；客户端从 pin 起逐级验证。

## 5. relay

- axum + tokio + rusqlite；**单写线程 + BEGIN IMMEDIATE**；无跨 await 写事务。
- HMAC 游标（89B）；schema 当前 **v9**（v1–v9 迁移链实测）。
- 配额：Q（普通）/R=Q（维护预留），`maintenance_reservations`（rotation 粒度，预算共享）。
- GC：两份 active 基线后 7 天清旧密文；旧 slot `retire_at=activate+14d` 真删行+对象 body；旧 cp prune 保留签名骨架；degraded 读回（一基线损坏可读另一份，GC 不前进）。
- 鉴权：device realm（Ed25519 challenge/token）+ management realm（恢复码）；**management 仅可读 KEY_SLOT 对象**（ADR §22），写/事件不放宽。
- 管理写：唯一 canonical body + request_id/body_digest 幂等；activate 指名两 cid/两槽/证书集合/四个材料 digest/CAS。

## 6. 客户端生命周期

### 6.1 genesis
建 root/KEK/初始 password 槽 + recovery 槽；签设备证书；签 genesis 控制记录；上传两基线；初始 cursor。

### 6.2 push / pull
- push：本地 outbox（MNE1 parts）→ PUT parts → complete → log → ACK；ACK 三元组 CAS。
- pull：验 log 链/签名 → 按 object_id 下载 → 解密 → quarantine/pending 依赖 → integrate → 推 cursor。
- 重启续传：cursor/received/integrated/highwater 持久化。

### 6.3 checkpoint
`prepare` 先固定 S（lease）→ 导出上传；`validate` 从 relay 读回、解密、refs_digest/签名/head 覆盖；`promote` 只接受私有 `VerifiedBaseline`（ADR §25.3）。

### 6.4 bootstrap / 恢复
- 选择：primary → fallback **整条基线试跑**（manifest/commit/snapshot/asset 任一失败整体切）；S/cursor/docs/assets 全部来自所选基线；已有 body 用 Loro import 按 VV 合并（不跳过、不覆盖离线编辑）。
- 恢复码自批：管理 token → 读 KEY_SLOT（staging）→ 解真实 root → 用新设备 HPKE 封**真实 grant** → invite/redeem/approve → device token → bootstrap。
- 恢复后离线可读（已 e2e 验证，停 relay 后另一进程重读）。

### 6.5 轮换 A–J
A 验管理链/pin head/停 push、备份 R'；B begin（request_id、丢响应按 request_id 查回）；C integrate 到 S（received 不够），无 pending/quarantine；D 全量 frozen 资产重加密；E 两份独立基线（两个空上下文 import 验证）；F 全 active 设备 grants；G 双签 activate（四 digest + CAS）；H 读回自己 grant 解根 == candidate root，原子推广；I 其它设备 apply + baseline merge；J outbox 重打包（旧 ACK CAS 忽略）。

### 6.6 restart
`plan_restart`（epoch+1、fresh root/password/R'/管理 key、新槽、`repack_required=1`）→ 新槽上传 → 两基线 → 全设备 grants（排除 revoked）→ 双签 activate。**当前 `execute_restart` 假定 relay 侧 restart 已由调用方完成**；relay 已有 `POST /rotations/{r}/restart` 端点，但客户端尚未把两者串成单一入口。

## 7. 轮换安全约束（已收敛）

ADR §23–§30 + 评审 0001 的硬约束：
1. **吊销后旧 epoch 永久不可解冻**：`epochs.revoked_once` 持久化 + v4→v5 历史回填；abort 只看该事实，不看本次 rotation 计数。
2. **候选 checkpoint 不参与 GC/bootstrap**：非 active epoch 只能 `candidate`，activate 原子 promote 指定两 cid；GC 只看 active epoch 的 committed pair。
3. **activate 两基线独立**：manifest 不同、SNAPSHOT refs 不相交（ASSET 可共享）、同 epoch/S、commit 绑定；客户端另在两套空上下文验证 actual VV。
4. **管理转换双签绑定**：`ManagerTransitionV1` 9 字段、`new_manager` 不得 None、旧/新 key 各签 64B、持久化并由 `/control` 输出。
5. **grant 可验证**：绑定 recipient/epoch/enc/cipher，持久化并返回签名；客户端用管理公钥验外层签名。
6. **管理写唯一 canonical body**：外层字段逐字节一致或只解析 body；request_id 幂等。
7. **资产登记不依赖 log、不暴露真实 hash**：`asset_objects(epoch, object_id, state, finalized_seq, rotation_id)`，opaque ObjectRefV1。
8. **旧槽真退役**：到期删行+对象 body，直接 GET 也 410。
9. **outbox 重打包**：只能以"已激活、远端 durable、已验证 baseline VV"判断覆盖；**不得用合并后的 local VV**；未覆盖者保持原 Loro bytes，新 message_id/object_id/E/nonce 重包；旧 ACK CAS 忽略。

## 8. 当前状态

### 8.1 已实现并验收（Windows 真机）
| 项 | 证据 |
| --- | --- |
| 密钥/DPAPI/证书/初始槽 | sync 单元测试 + e2e_localhost |
| genesis / push / pull / 重启续传 | e2e_localhost（含 assets、checkpoint） |
| 新设备 bootstrap（恢复码） | 双目录 e2e：只给 vault_id + recovery code；停 relay 后离线读通过 |
| 改口令 | e2e + 单元 |
| relay 安全轮换收口 | relay 160 tests；schema v9 |
| 单设备轮换 A–H + P2 重包 | `scripts/e2e_rotation_verify.ps1` 复跑通过：`activated epoch=2 / grant root verified / repacked 1 / old epoch cannot decrypt new / delayed old ACK ignored / bootstrap=2` |
| restart 执行链 | 旧基线，168 tests（两基线/排除 revoked/双签 activate/四 digest） |

### 8.2 在途（未收口）
1. **两个 bug 修复中**（新 subagent）：
   - 轮换候选槽 AAD `message_id` 错位（标准 reader 读不回新槽）；
   - `stage_grants` 的 `grant_id` 填成 device_id。
2. **双设备 e2e 在途**（subagent）：`examples/e2e_common.rs`、`e2e_rotation_two_devices.rs`、`scripts/...ps1` 未提交。
3. `execute_restart` 与 relay `POST /restart` 的单一入口未串。
4. 全仓测试最后一次绿在 旧基线；旧基线 后需重跑全仓。

### 8.3 明确未做
- app 端同步 UI（连接/配对/恢复/轮换进度/确认）。
- TLS 传输（当前 dev HTTP；生产必须）。
- 第二设备见证 / rollback 检测（ADR §10 边界：新设备无 trusted highwater 时无法识别"有效签名的旧 manifest 回滚"）。
- 同 epoch 资产孤儿 GC、Merkle grant inclusion、自动接管 UI。
- relay 运维（备份/监控/限流/部署文档）。
- MCP 集成。

## 9. 里程碑计划

### M0 收口（当前）
- 修两个 bug；双设备 e2e；restart 客户端入口串联 + 回归；
- 全仓 `test --all` + `clippy` + `fmt`；真机复跑单/双设备脚本；
- 打 **`v0.4-sync`**（release gate：轮换正常路径 + 恢复 + 离线读 + 双设备合并）。
- 验收：T07/T08/T09/T11/T12/T13/T15/T17/T19/T21 有对应证据；双设备 e2e 四项断言全过。

### M1 app UI（1–2 周）
- 同步设置页：relay URL、连接测试、vault 创建/加入、恢复码展示与"已备份"确认；
- 恢复流程 UI：输入 vault_id + 恢复码 → 进度 → 失败可读错误；
- 轮换 UI：触发条件、进度阶段、失败/restart 确认、新恢复码强制备份；
- 状态栏：epoch、last sync、pending/conflict 数；
- Windows 真机验收（IME/DPI/长文本）。

### M2 TLS 与加固（约 1 周）
- relay HTTPS（rustls），客户端证书校验/可选 pinning；
- 限流、请求大小、鉴权失败审计；
- 生产部署文档（反代/证书/备份）。

### M3 生产就绪
- relay 备份/恢复演练；GC 监控与告警；轮换 runbook；
- 故障注入（kill relay/磁盘满/时钟偏移）；
- 第二设备见证或显式 rollback 边界文档。

### M4 MCP/自动化
- MCP server（只读搜索/导出/同步状态）；写操作另行评审。

## 10. 风险与开放问题（请 GPT 决断）

1. **信任边界**：management realm 只读 KEY_SLOT 是否可接受？是否存在"恢复码泄露即管理权 + 可读所有历史 KEY_SLOT 密文"的额外风险？是否需要把 KEY_SLOT 读取限制在一次性 challenge 或审计？
2. **restart 语义**：`execute_restart` 当前假定 relay 侧 restart 已完成；正确顺序应是客户端先 `POST /rotations/{r}/restart` 拿新 rotation_id/control_rev，再生成材料，还是 relay restart 只做状态迁移、材料完全由客户端决定？请给出契约（含失败/超时）。
3. **轮换复杂度**：A–J 是否可在 v1 简化（例如强制"轮换必须在所有设备在线/可通知"）以降低风险？还是必须保持离线设备可用？
4. **双基线独立**：客户端必须在两个空上下文 import 是正确门禁，但是否需要在 activate 前把"两基线实际 VV 证据"上传给 relay/控制链（便于审计），还是只本地验证即可？
5. **回滚检测边界**：新设备首次 bootstrap 无 trusted highwater，无法识别"有效签名的旧 manifest 回滚"。v1 是否接受并写入文档，还是必须做"第二设备见证/relay 一致性证明"？
6. **outbox 覆盖判据**：当前用"该 doc/generation 的已验证 baseline VV"，是否足够？多 doc 批量/资产引用是否需要额外判据？
7. **里程碑顺序**：M1 UI 与 M2 TLS 哪个优先？生产前是否必须 TLS（当前 localhost 开发服务器）？
8. **测试门禁**：T01–T22 中哪些是 v1 发布**必须**、哪些可后置？请给出最小发布矩阵。

## 11. 请 GPT 输出

- 对 §2/§3/§5/§7 的**安全结论**（可接受/必须改，按严重度）；
- 对 §6 轮换/restart 状态的**正确性审查**（尤其 restart 顺序与 fail-stop）；
- 对 §9 里程碑的**优先级与门禁**建议；
- §10 八个问题的明确回答；
- 若认为 v1 范围应缩减，请给出"可删/必须留"清单。

---

## 12. 评审 0002 结论（2026-11-12，gpt-6.1-sol）

完整评审见 [`docs/reviews/0002-sync-v1-security-review.md`](../reviews/0002-sync-v1-security-review.md)。

**结论：不批准按 M0 打 `v0.4-sync`，不批准生产部署。** 设计主干可保留，但当前不是"只差两个 bug/一条 e2e"。

已确认的发布阻塞（均需在 M0 收口）：

- **P0-1** ASSET 同 key+nonce 加密不同 plaintext/AAD（§31 已定修法）；
- **P0-2** 控制链/设备证书未认证，恶意 relay 可把候选根封给攻击者 HPKE key；
- **P0-3** checkpoint 先发布后弱验证，可假覆盖 S 并驱动 GC（§32 已定修法）；
- **P1-1** 真实 HTTP I/J 的 wire 错位 + 缺完整 activation T 双签/包含绑定；
- **P1-2** restart 的 epoch 契约与 durability 错误（§33 已定修法）；
- **P1-3** journal/promotion/outbox 未闭环（身份丢 old root/manager seed、状态机跳步、C 门禁过严、repack 未做 VV 选择）；
- **P1-4** bootstrap 多事务导入非原子、highwater 可回退；
- **P1-5** relay 旧 epoch/candidate/ASSET 副本不回收（task-109）；
- **生产阻断** 客户端只有 HTTP，无真实 TLS/hostname 校验。

**M0 门禁修正**：P0/P1 全收口 + 真实完整双签 T + durable restart/journal/promotion + 真 baseline 验证 + all outbox states +
cross-epoch/candidate GC + T01–T22 最小矩阵（见评审 §五）；M1 的 TLS 提前到任何非 localhost 发布之前。
**在 M0 完成前，`v0.4-sync` 不创建、不对外宣称"同步 v1 已完成"。**

---

## 13. 决策：走完整 M0（2026-11-12，用户确认）

不发布"实验版同步"；`v0.4-sync` 的验收范围按评审 0002 §五 的 M0。

### 13.1 执行顺序（串行为主）

1. **收口在途修复**（P0-1/P0-2/P0-3/P1-1 部分/P1-2/P1-3 部分/P1-5）：
   等各 worker 提交、解决并发编译冲突，`cargo test -p menghunote-sync` 恢复到绿。
2. **P1-4 bootstrap 原子导入/highwater 单调**（新任务）。
3. **完整 activation T 双签与包含绑定**（proto + relay + sync，必须按评审 0002 §二 P1-1.4 定义；
   不只九字段 `ManagerTransition`，T 绑定 pins/from/to/S、两基线 commit、slots、成员证书、全部 grants、old/new assets）。
4. **控制链分页/pin 单调**（>256 记录、历史吊销证书、manager 链）。
5. **TLS 客户端与 relay**（M1 提前；任何非 localhost 之前必须完成）。
6. **T01–T22 最小矩阵**（真实 HTTP + 真实 Ed25519/HPKE + 真实 SQLite/Loro；FakeRelay 绿不算门禁）。
7. **最终复审**（GPT 对最终 diff）+ Lead 跑 fmt/clippy/test + 真实双设备 + 打 `v0.4-sync`。

### 13.2 并行纪律

- 同一 crate 内不再开多个 worker 同时改不同文件；以"一个 worker 一个可编译切片"为原则；
- 每个提交必须 `cargo test -p <crate>` 绿，不允许把"等别人编译"当常态；
- 交叉 crate 的 wire 变更（activation T、TLS）先冻结 proto/ADR，再分 crate 实现。

### 13.3 不可延期清单

root/package/activation 认证、离线 latest grant、两独立真验证、durable 秘密/不可逆冻结、outbox 全状态、
跨 epoch/candidate GC、非 localhost TLS；完整 UI 美化可后置，但恢复备份确认/吊销确认/冻结状态/手工导出不能后置。
