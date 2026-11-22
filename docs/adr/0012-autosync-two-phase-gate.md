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
