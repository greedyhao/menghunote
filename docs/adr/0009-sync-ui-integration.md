# ADR 0009 — 同步接入 app UI（dev-only 面板 + 串行 worker）

> 2026-11-22 历史整理：本文是整理前的历史文档，文中旧 commit 锚点已清除（原哈希不再对应新 `main`）；原始版本见 `backup/dev-20261122`，旧→新映射见 `docs/notes/history-rewrite-20261122.md`。


> 状态：**已决**（2026-11-13，GPT 决策；Phase 1 实施中）。
> 输入：`crates/app` 现状、`crates/sync` 契约（ADR 0006）、headless UI 测试基座、e2e 流水。
> 关联：[ADR 0006](0006-sync-v1.md)、[0007 交接](../outdated/0007-sync-handoff.md)、[0008 Linux 封装](0008-linux-local-identity-wrap.md)。

## 背景与问题

同步/加密 v1 的库、relay、e2e 已完成，但 app 完全没有同步入口（`crates/app` 甚至不依赖 `menghunote-sync`）。
只读核实发现五处必须先补的接线，否则“接上 SyncEngine”只是假同步：

1. 真实编辑器的 journal/structure 追加不产生 sync intents（只有 e2e 显式调用 `append_updates_with_sync_intents`）；
2. `engine.integrate` 只把 update 追加进库，`SessionDriver` 不订阅外部提交，当前文档/结构树不会刷新；
3. app 的 dev vault 与 `SessionOptions` 固定 `peer_id=1`，两个实例会撞 CRDT peer；
4. session flusher 持有 durable head CAS 基准，engine 外部 append 后本地 flush 会 `HeadConflict`，当前没有 rebase；
5. 真实 relay 的数据日志只写 UPDATE/ASSET；metadata 事件与“已有 progress 时 bootstrap 保留旧 cursor”属于 Phase 2 风险，不能靠 `set_meta` 跳 cursor。

## 决策（Approve A）

在**真实 AppShell** 内做 **dev-only 同步面板 + 串行 sync worker**；不另起独立 GPUI 示例窗口，不复用 e2e 的写身份/日志代码。

- **门禁**：编译 `--features dev-sync`，且运行时 `--dev-sync` 或 `MENGHUNOTE_DEV_SYNC=1`；否则 app 行为完全不变。启用时只接受 localhost、显式独立的新实例目录。
- **Phase 1**：两个实例 + localhost relay，真实输入 → A 推送 → B 当前编辑器看到 → 双向继续编辑；包含初始双 checkpoint 与 Join，不开放轮换/自动同步/自动发布。
- **Phase 2**：真实轮换、B 自身 grant apply、旧 outbox repack、恢复/metadata reconciliation，再证明轮换后双向编辑。

## 数据结构与 API（草案）

### 同步 worker（app）
```rust
enum Command { Create, Join, SyncOnce, PublishInitialPair, Rotate, Resume, Cancel, Stop }
enum Event { Progress, Completed { op_id, changed_heads, result }, Stopped }
SyncService::try_command(Command) -> Result<OpId, Busy>;
SyncService::poll() -> Vec<Event>;
SyncService::request_stop() -> StopTicket;
SyncService::poll_close(ticket) -> CloseState;   // 等明确 DurableAck，不等待网络同步
```
- 专用 OS 线程做 IO/Argon2/crypto/checkpoint；`SyncEngine` 借用型，在每次 worker 操作内构造。
- GPUI 侧 `cx.spawn` 限量非阻塞泵事件、弱实体更新；后台不持有 GPUI entity。
- 单操作在途，`Busy` 拒绝新命令；`Completed` 结果/已 durable heads 不丢；`Stop` 后后台回收，GUI 不 join。

### 外部提交闸门（storage::session）
```rust
begin_external_commit() -> Result<ExternalCommitTicket, GateError>;
poll_external_commit(&ticket) -> GateState;     // WaitingIME → Flushing → Held
end_external_commit(lease, applied: AppliedHeads) -> Result<(), GateError>;
adopt_committed_delta(&mut self, lease: &ExternalCommitLease, committed: PreparedCommittedDelta)
    -> Result<AdoptOutcome, AdoptError>;
```
- 顺序：自然结束 IME → 目标 `DurableAck` 且 `FlushQuiescent` → Held；Held 前禁止外部 append；期间禁输入/undo/切换/结构操作。
- `adopt_committed_delta` 只导入**已 durable 的 CRDT**，不重新 journal/outbox；更新 session/flusher durable head、VV/frontier、`journal_base_vv`，产出 `Remote LiveDelta` 并保留 Undo/选区；同 head 幂等；失败保持 `ReconcileRequired` 禁写。
- 完成后显式结构投影 → 补正文行 → 重试依赖 inbox → 物化/侧栏刷新。

### 身份、peer 与 intents
- `DevIdentityStore::{load,persist_protected}`；peer 唯一来源是 `LocalIdentityV1::loro_peer_id()`（固定映射、重启不变）；两实例不得复制身份或沿用常量 1。
- `JournalEntry` 携 typed origin / 稳定 intent IDs / epoch fence；本地输入、IME、undo/redo、结构提交走同一事务 updates + intents；重试 ID 不变；remote/import/adopt 不生成 intent。
- 身份保护写盘：私有临时文件 → fsync → 原子发布 → 父目录 fsync。
- 只接受修复后新库；旧开发库拒绝启用，不做隐式迁移。

### Create / 发布基线 / Join / SyncOnce
- A：Create → 受保护身份持久化 → 用户真实确认恢复码备份 → 真实 UI 建笔记/输入 → `DurableAck` → `SyncOnce`（完整 push/pull/integrate/ACK）；随后用户点「发布基线」→ worker 等 flush/闸门并确认覆盖 → 上传初始槽、发布两份真实 checkpoint。
- B Join：用户输入 **vault_id + 恢复码**；`RecoverySeed → ManagementKey::from_recovery` 建管理信任锚 → self-enroll 新设备/peer → 认证 bootstrap/readback → pins/cursor/身份提交 → 打开同篇。**不读取 A 目录、identity/root**。
- 初始 pair 未完成时 Join 保持 Setup/错误，不退化为 push/pull；无资产 demo 才用 `EmptyAssets`。

## Phase 1 实现简化（切片 4c-2，2026-11-13）

切片 4c-2 把 `DevLocal::sync_once` 真正接到 app，并在 localhost dev 演示范围内做了三处
**明确的简化**（不是"看起来同步成功"，而是把边界写死在这里）：

- **整段 sync 期间持闸**（不只是 integrate）：UI 先 `begin_external_commit` → `Held`，
  然后才派发 `SyncOnce`；worker 跑完 `encrypt_pending → push → pull → integrate → ack`
  之前闸门一直持有，直到 UI `prepare_committed_delta` → `adopt_committed_delta` →
  `end_external_commit` 才释放。代价：同步期间活动文档输入被冻结（状态栏显示
  「外部提交中，输入已冻结」）；收益：integrate 永远发生在"本地没有未 ACK 写入"的
  窗口里。localhost 上可接受，**完整产品同步 UX（只在 integrate 阶段持闸、同步期间可继续
  输入）不做在 Phase 1**。
- **worker 的持闸证明**：`SyncOnce` 必须经 `SyncService::try_command_with_gate` 派发，
  命令携带 `HeldGate { doc_id, durable_head_seq, generation }`；worker 用自己的 `Vault`
  连接复核「库中该文档 head == 持闸时的 durable head」后才发网络请求/integrate。不满足
  就明确失败（裸 `try_command(SyncOnce)` 会被生产 runner 拒绝）。这是 in-process 的
  dev-only 协调，正确性仍由 storage 闸门（Held 期间本地写入 fail-closed）保证。
- **只 adopt 活动文档**：本片保证"不重开编辑器就看到远端文字"的只有当前活动文档。
  其它 body 文档本轮 integrate 也可能推进，但没有打开的会话；下次打开时由
  `SessionDriver::open` 从库里重放（天然正确）。
- **结构树刷新限制**：worker 的 integrate 只追加 CRDT update，**不**重投影 `tree/notes`。
  UI 在同步前记录结构文档 `head_seq`，同步完成后只有它变了才重放结构文档并
  `StructureWriter::commit`（无新 op 时只补投影）→ `refresh_tree`；没变就不动。
  结构变更若改了当前打开笔记的归属/标题，侧栏会刷新，但**不会**自动切换/重开文档。
- **无活动文档时走无会话同步**（2026-11-13 补丁）：窗口里没有打开的笔记（空库 / 刚 Join /
  用户没点开任何文档）时没有 `DocumentSession` 可以 begin/adopt，闸门无从谈起，因此走
  `Command::SyncOnceNoSession`（`SyncService::try_command_no_session`）：不持闸、不 adopt，
  同步期间 `AppShell::write_structure` 拒绝一切结构写入（新建/重命名/删除/移动），结束后
  按结构 head 重投影并把**第一篇笔记自动打开**。普通 `SyncOnce` 的持闸要求**没有放宽**，
  两条路径在命令/入口/worker 证明上互斥（无会话命令带 `HeldGate` 会被 worker 明确拒绝）。
- **无会话同步可能补一轮**：远端**新建**笔记时，正文 update 依赖本地已存在的 body 文档行，
  而那一行要等结构文档**投影**之后才会有；一轮 `sync_once` 只能拿到结构（正文被
  `integrate` 记为 deferred）。所以收尾发现"结构 head 变了 + 树里出现笔记"时会先重投影，
  再自动补**一轮**同步（`ActiveSyncOnce::follow_up_done` 防循环）然后才自动打开第一篇。
  这是同一次用户点击内的补轮，不是失败重试；产品化时应在 integrate 侧消除该依赖顺序
  （或让投影与 integrate 同阶段），而不是靠 UI 补轮。
  > **2026-11-13 更新（ADR 0012 S2b）**：该补轮已删除。Integrate 阶段按「结构 → 只投影
  > 建立 body 行 → 正文公平续批」收敛，UI 只保留结构 head 变化时的侧栏刷新。

已知边界（Phase 2 之前不修，均已在本片文档/代码注释里标注）：

- `operation_deadline` 超时是服务侧"放弃"：worker 仍可能阻塞在传输里。超时后 UI 按失败
  收尾并释放闸门；真实迟到的 integrate 会让下一次 `begin` 的 `Unadopted` 检查 fail-closed
  （不会静默通过），恢复方式是重启会话/实例。localhost 正常路径不会触发。
- 持闸期间禁止**切换文档**（`AppShell::open_entry` 明确拒绝），但侧栏的结构写操作
  （新建/重命名/删除）没有额外 UI 冻结；那短暂窗口里结构 CAS 冲突按存储层既有语义
  （错误可见、不静默丢弃）。产品化时统一收口。
- Create/Join 仍走 CLI（`--dev-sync-create` / `--dev-sync-join`，见切片 4c-1）；
  查询/自动化发布见切片 4c-3 的 `--dev-sync-info` / `--dev-sync-publish`；
  面板上的「创建/加入」按钮仍是明确"未实现"。**「发布基线」已在面板里（切片 4c-4，
  见下节）**。

## 手工演示交付（切片 4c-3，2026-11-13）

Phase 1 的面板/同步已能跑，但手工演示缺"操作者能拿到恢复码并发布基线"的最后一环。
切片 4c-3 **只补交付面，不改协议/worker/面板语义**：

- `--dev-sync-create` 成功、开窗前，把 `vault_id` 与恢复码以 **DEV ONLY** 块打到 stdout
  **一次**（不写日志/配置/meta/身份文件，不随 `SetupOutcome` 继续持有）；Join/Existing 不打印。
- `--dev-sync-info`：只读身份 + 库，打印 `vault_id` / `device_id` / `peer`（公开标识），
  不开窗、不联网、不创建；恢复码/root/私钥不出现。
- `--dev-sync-publish`：对已有实例（普通现有库 + `MENGHUNOTE_DEV_RECOVERY_CODE`）调
  `DevLocal::publish_initial_pair`，打印 checkpoint id / epoch / 覆盖 seq / 文档数；
  缺码/错码在**打开库与任何上传之前**失败（库不变），发布失败退出非 0，重跑沿用 4b 幂等。
- 三个 flag 与 Create/Join 同层互斥、不开窗；完整两实例步骤与已知限制见
  `docs/notes/dev-sync-manual-test.md`。

## 面板发布基线（切片 4c-4，2026-11-13）

用户反馈："每次手工创建太麻烦"——原来 A 建完笔记必须**关程序 → `--dev-sync-publish` →
重开**，本切片把这一环接进面板：

- **按钮**：「发布基线」只发 `DevSyncPanelEvent::PublishRequested`（不直接发命令）；宿主
  `AppShell::dev_sync_publish_baseline()` 复用**同一条持闸状态机**
  （`begin gate → Held → try_command_with_gate(PublishInitialPair) → Completed →
  prepare/adopt/end → notice`）。发布**不需要**新 adopt 语义：没有新落库行时
  `prepare_committed_delta` 不会被调用，复用同一条收尾路径不会多写任何东西。
- **worker 命令**：`PublishInitialPair` 与 `SyncOnce` 一样**必须带 `HeldGate`**
  （`try_command_with_gate`）；worker 打开自己的 `Vault`、复核「库中 head == 持闸时
  durable head」、加载身份后才联网。裸命令被明确拒绝。
- **不需要恢复码**：新库入口 `DevLocal::publish_initial_pair_with_identity` 的信任锚是
  **受保护身份里的管理私钥**（`manager_seed` → `ManagementKey`，并与 `pinned_manager`
  比对）。Join 设备没有管理私钥 → 面板置灰 + 宿主/worker 双重明确拒绝。CLI 的
  `publish_initial_pair`（恢复码）保留给自动化/恢复/旧实例。
- **初始 recovery 槽前移到 Create 上传**：KEY_SLOT 对象的 MNE1 密钥
  `K_slot_mne = HKDF(恢复码种子)` 不可逆、恢复码不落盘，所以"只有身份"时**无法**补传槽。
  因此 `DevLocal::create`（恢复码还在内存里）上传初始槽并落 durable 标记
  `dev.local.initial_slot_uploaded.v1`（slot_id + 确定性 message_id，都是公开事实）。
  发布只做 fail-closed 校验：槽材料与身份/库自洽（vault/epoch/manager pin）+ 标记存在。
  `create` 之后失败沿用既有清理语义（清掉本地库/身份，relay 侧留下不可用的空 vault）。
- **旧实例**：切片 4c-4 之前 Create 的库没有这条标记 → 面板发布明确失败并指向
  `--dev-sync-publish` + 恢复码补一次（之后面板发布一直可用），**不静默发布一份 B 无法
  Join 的假基线**。
- **限制**：面板发布要求有**活动文档**（发布读 durable 快照，先持闸 flush）；没有打开的
  笔记时明确拒绝。发布期间输入冻结、结束后释放；重复发布沿用 4b 幂等语义（多一对
  checkpoint，`/bootstrap` 只取最新两份）。

## 安全边界

- 开发演示沿用 localhost `DevPermissiveHook` relay，UI/文档固定标注「非生产、服务端验签放行」；**正式集成与鉴权负例必须用 `relay::Ed25519Hook`**，app 不链接 relay/hook 实现。
- `ControlPin` 的 localhost dev 豁免只允许首次无既有 pin 的开发初始化；加载身份后显式注入 identity + bootstrap 持久 pins/hash，不借豁免绕过旧 pin。
- 配置只存公开 URL/ID/路径；口令、root、setup secret、恢复码不入普通配置/明文 meta/日志，也不记录 HTTP headers/body；恢复码仅临时显示、提交后清除。
- Windows 走 DPAPI；Linux 显式初始化每用户主钥（ADR 0008），实例重置不删/换钥；0600 裸主钥弱于 DPAPI，SQLite 正文仍非静态加密。

## Phase 2（切片 5）

- A：flush/sync 到 S → freeze → 新槽 + R′ 备份确认 → 两份候选基线/资产 → 全部 active grants → 双签 T/控制链激活 → 原子 promotion/repack。
- B：认证 activation/control → 从自己的 HPKE grant 解根 → baseline apply/promotion/repack → 重载身份 → 闸门内 adopt → 更新 epoch 上下文后解冻。
- 遇到已知 metadata 必须验签绑定并 durable 记录消费前缀后才推进 cursor；未知仍 pause；`CursorExpired` 走认证 rebaseline/resume，不能 `set_meta`。
- 验收：轮换后双向文本、B 离线旧 outbox、apply 中断重启、repack 幂等、连续 Sync/发布 pair 后继续、注入 metadata/未知事件/游标过期；鉴权负例用 `Ed25519Hook`。

## 实施记录（Phase 2 切片 6a/6b：轮换接入，2026-11-13）

**6a（机制层，旧基线）**：`DevIdentityStore::replace_protected`（只允许同 vault/device 的
身份替换，0600 临时文件 → fsync → rename → 父目录 fsync；失败时旧身份逐字节保留）+
worker `Command::Rotate { recovery_code, revoke }` / `Command::ApplyEpoch`（**整段持闸**、
派发前复核库中 head、成功后把新身份写回 `dev-identity.bin` 并重新 load、失败保留 journal
与全部 durable 事实、只上报公开事实）+ `DevLocal::rotate` 的可选恢复码重封（`Some` 时先
校验该码派生的管理 key 就是当前 pin 的管理身份，再重封候选 epoch 的 recovery 槽；`None`
沿用一次性随机种子）+ `AppShell::dev_sync_rotate/replace_after` 与成功后活动视图重建
（关旧 `EditorView`/`SessionDriver`，用新 epoch 重新 `open_entry`）。

**6b（设备面板 UI）**：

| selector | 行为 |
| --- | --- |
| `dev-sync-rotate` / `dev-sync-apply-epoch` | 设备列表区里的两个**独立**入口；不可用时置灰 + 一行明确原因（无管理密钥 / 无活动文档 / 单操作在途 / worker 已停），点击只给原因、不展开确认 |
| `dev-sync-rotate-confirm` / `-cancel` | 二次确认的确认/取消；取消**不写**、清空恢复码输入 |
| `dev-sync-rotate-reissue` / `-recovery` | 「重新封装恢复码（推荐）」默认勾选；输入是**秘密字段**（遮罩、允许粘贴、不报告平台正文），提交后立刻清空，只在 `RotateRequest` 里留一份 `Zeroizing` |
| `dev-sync-rotate-revoke` | 「同时把已移除设备写入签名撤销列表」默认勾选（ADR 0010）；UI **不手选设备**，按设备列表 `status=revoked` 过滤后传给 `Command::Rotate` |
| `dev-sync-rotate-warning` | 未勾选重封装时的固定警告「轮换后新设备将无法用恢复码加入」；勾选但输入为空时的提示 |
| `dev-sync-apply-confirm` / `-cancel` | 应用新 epoch 的二次确认（固定文案见常量 `APPLY_CONFIRM_TEXT`） |

固定确认文案（一字不改）：

> 轮换会用新密钥重新加密整库，并生成两份候选基线；旧 epoch 数据不能再被新设备解密；
> 被移除设备仍可解其已获得的数据；轮换期间输入会冻结。是否继续？

交互约束：

- 面板**不直接**发 `Command::Rotate`：确认后只登记请求并发 `RotateRequested` /
  `ApplyEpochRequested` 事件，由 `AppShell::dev_sync_pump` 取走（秘密不进事件）再走 6a 的
  持闸状态机；同一时刻只允许一条持闸任务。
- 轮换/apply 在途：整段持闸（编辑器输入/结构/切换冻结）、两个入口置灰、自动同步暂停；
  面板任务行显示「轮换中（申请/候选/激活/推广）+ worker 明细」。
- 成功：视图重建、树/状态刷新，notice 显示新 epoch + checkpoint 短 id + 恢复码是否补发；
  失败：保持旧身份/旧视图，明确错误；停在 journal 之后的失败额外提示"重试前需重启实例"，
  apply 可原样重试（不自动重试）。
- **移除与轮换不串联**：默认移除仍只删凭证；轮换只有用户显式点击才发生。
- 不显示 root/manager/恢复码；恢复码输入只读一次、提交/取消后清空。

**验证**：`cargo test -p menghunote-app --features dev-sync --test dev_sync_rotation`（9 个：
6a 4 + 6b 5，真实 dev-relay 两 AppShell）；app --features dev-sync 311 / 默认 134 /
sync 459 / storage 171；`clippy --all-targets --features dev-sync -- -D warnings` 干净。

## 测试与验证

- 切片验收命令（新增测试）：`cargo test -p menghunote-storage sync_intent` / `committed_adoption`；`cargo test -p menghunote-sync dev_local`；`cargo test -p menghunote-app --features dev-sync --test dev_sync_service` / `sync_ui`。
- 现有 `crates/app/tests/headless_ui.rs` 回归；等待明确 ACK/物化/完成/收敛，不 sleep。
- 每个可见切片立即 Windows 真机验；再做 Linux 两进程 smoke：中文 IME、Undo/选区、DPI、关闭、轮换前后文字。
- Lead 实跑默认 app build、`fmt`、`clippy --workspace --all-targets --all-features`、`cargo test --workspace --all-features`。

## 非目标

生产部署、完整账户/设备 UX、同步期间并发编辑、附件 UI、旧开发库迁移、GPUI 血脉/重依赖替换、冻结 wire 改动；保持现有依赖方向与 Kit pin。

## 设备移除策略（2026-11-13 用户决定）

默认与轮换分离：

- **默认「移除设备（仅删凭证）」**：relay 管理端吊销该设备（`devices.status='revoked'` + `auth_rev` 递增），不换 epoch、不重加密。客户端用 `/control` 的 `member.status` 做 **fail-closed 否决**，把它排除出 active 集合与后续 grant。
- **只有用户显式点「轮换密钥」**才走 ADR 0006 的 rotation（新 root、HPKE grant、双签 activation、旧 epoch 冻结）。

语义边界（UI/文档必须写明）：

- 默认吊销是 **relay 可信的访问撤销**，不是密码学撤销；被移除设备仍持有旧 root，能解它已获得的密文。
- 签名控制链仍把它列为成员（旧加入事件），relay 的 status 只是收窄；被攻陷/恶意 relay 可把 status 改回 active，客户端不能凭空授权但默认吊销不具密码学强制性。
- 需要“未来数据即使被它拿到也解不开”时，必须用轮换。

落地（`task-123`，Phase 2 开头）：新增管理端端点 `POST /v1/vaults/{vault}/devices/{device}/revoke`（management proof + `expected_control_rev`；**拒绝吊销最后一个 ACTIVE 设备**、pending/轮换中拒绝、幂等；relay 无法识别管理设备，故不承诺“保留管理权”），sync 客户端 helper，app 设备列表默认按钮；测试覆盖 token 立即失效、`/control` active 集合排除、rotation plan 不含该设备、已吊销设备 push/pull 返回 REVOKED。完整语义见 [ADR 0010](0010-device-access-revocation.md)。

## 后果与回滚

- 五片依序集成，重叠 storage 写域不并行。
- 代码回滚：关闭 `dev-sync` feature，保留 identity、outbox、journals 与 Linux 用户主钥。
- 安全暂停优于“看起来成功”：失败保留 journals/outbox，不倒退已激活 epoch/pins，不删除身份重建。
