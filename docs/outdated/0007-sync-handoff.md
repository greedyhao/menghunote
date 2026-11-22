# 0007 同步/加密 v1 交接（新窗口开发用）

> **已归档（2026-11-22 文档整理）**：本文被判定为「已作废 / 被取代 / 已完成的待办」，移入 `docs/outdated/` 保留追溯，**不作为当前状态依据**。归档说明见 [`docs/outdated/README.md`](README.md)。

> 2026-11-22 历史整理：本文是整理前的历史文档，文中旧 commit 锚点已清除（原哈希不再对应新 `main`）；原始版本见 `backup/dev-20261122`，旧→新映射见 `docs/notes/history-rewrite-20261122.md`。


> 历史文档：以下 commit 哈希属于 2026-11-16 整理前历史；映射与 backup 分支见 [history-rewrite-20261116](../notes/history-rewrite-20261116.md)。


> 状态：**v0.4-sync 已由 GPT 复审 APPROVE，并打 annotated tag**（非生产，仅 localhost 开发验收）。
> 复审锚点：整理前的旧基线（等价于当时整理后的提交；APPROVE，无必修项）。
> **tag 前移（2026-11-13）**：`v0.4-sync` 现指整理前的锚点（含 e2e 去脚本化 / 8 条 headless UI 测试 / 文档）；
> 旧锚点保存在 `v0.4-sync-backup-20261113`（→ 旧基线）。产品 `crates/*/src` 逻辑与 旧基线 一致（新增仅 `debug_selector`）。
> **Windows 实机已验证**：用户于 2026-11-13 用 旧基线 源码包（`menghunote-v0.4-sync-整理前基线.zip`）实测通过；
> 该包与 tag 的 `crates/*/src`、`Cargo.toml/lock` **完全一致**，差异仅为 e2e 驱动/文档/脚本（命令细节未逐条留档）。
> **历史整理（2026-11-13）**：`dev` 已重建为 12 个语义提交（tree 与整理前的旧基线（backup tip）一致）；
> 文中旧哈希（含 旧基线）只在 `backup/dev-20261113` 可达，见 [`docs/notes/history-rewrite-20261113.md`](../notes/history-rewrite-20261113.md)。
> 代码 HEAD：旧基线（整理后；整理前的旧基线）。本机（Linux）基线：
> `cargo test --workspace --exclude menghunote-app` = **1006 passed / 0 failed**；
> `cargo test -p menghunote-sync --all-targets` = **417 passed / 0 failed**；
> two-round e2e `EXIT=0` 且打印 `TWO_ROUND_EXIT=0`。
>
> **v0.4 只支持修复后新建的 vault/身份/控制链**；旧开发 vault 不得复用（原因见 §3）。
>
> **task-122 已完成（2026-11-19，未 commit）**：`CheckpointCommit.validation_version=2`
> （v1 解码保留、所有消费者显式拒绝）、receipt HMAC、relay GC 版本门禁已落地；
> 完整预算框架明确未做。详见 [`docs/notes/task-122-validation-v2.md`](../notes/task-122-validation-v2.md)。

> **ADR 0009 Phase 1（dev-only app 集成）完成（2026-11-13，HEAD 旧基线）**：
> 切片 1（面板/状态机/worker）→ 2（身份/peer/intents）→ 3a（外部提交闸门 + adopt）→
> 3b（EditorView/AppShell 转发）→ 4b（GPUI-free `DevLocal` 编排）→ 4c-1（启动期
> Create/Join）→ 4c-2（持闸 SyncOnce）→ 4c-3（create 一次性输出 vault_id/恢复码、
> `--dev-sync-publish` / `--dev-sync-info` CLI）全部落地。
> UI 先持外部提交闸门 → worker 线程里跑完整 `push/pull/integrate/ack` → 活动文档
> `prepare_committed_delta → adopt → end` → 编辑器**不重开**就看到远端文字；面板
> 「同步」按钮走同一条路径（面板只发请求，持闸由 `AppShell` 做）。
> dev-only 简化：**整段 sync 期间持闸**（不只 integrate）、只 adopt 活动文档、结构树仅在
> 结构 head 变化时重投影（做不到的部分见 ADR 0009 的「已知边界」）。
> 验收：`cargo test -p menghunote-app --features dev-sync --test dev_sync_sync_once`
> （真实进程内 relay + 两个 AppShell 双向文字 / 失败释放闸门 / 面板按钮）；
> app 默认 134 / feature 193 / sync 432+1；手工双实例步骤见
> [`docs/notes/dev-sync-manual-test.md`](../notes/dev-sync-manual-test.md)。
> **Windows 真机手工闭环尚未跑**（本机 Linux 缺 `libxcb-xkb.so.1`，只到 headless 集成测试）；
> Create/Join/Publish 仍 CLI，轮换/metadata 对账属 Phase 2。

## 1. 一句话结论

原三个阻断（ASSET 来源认证、post-activation 基线消费、同一 durable E1 跨两轮 e2e）都已修复并有 Linux 真实执行证据。
Windows 侧已由用户于 2026-11-13 用 旧基线 包实测通过（与 tag 的产品代码一致；当时命令与计数未逐条留档，复跑命令见 §5）。

## 2. 三个阻断的落地

### 2.1 ASSET 来源认证（原 §3.1）

以下四项同属整理后的 旧基线（整理前分别为 旧基线/旧基线/旧基线/旧基线）：

- pull 消费 ASSET 前真实下载全部 parts，验证每片 header/固定切片/AEAD、明文长度、data SHA256、MIME、canonical `asset_object_id`；写 durable skipped-asset v2 记录后才推进游标。
- 阻断 A——`ObjectCommit::validate` 给 ASSET 补 64MiB 上界（与 `ObjectRefV1` 同口径），读回层改 `try_reserve` 并有界分配，engine 先验 epoch/长度再下载；恶意 `part_count=u32::MAX` 不再触发进程 abort。
- 阻断 B——checkpoint 覆盖证据必须实证：
  - `prepare` 只产出 pending 候选，不产生成功覆盖证明；
  - `validate` 对每条候选复核完整 `ObjectCommit` 并真实下载验证；refs 内资产复用读回缓存，orphan/被替换资产也下载；缓存键含验证修订/根指纹/vault/epoch/OID/类型/generation/message/sender/parts/总长/digest，只缓存正结果；
  - 候选 epoch 的 `coverage_root`（旧 active 根）显式传入，缺根即暂停；
  - 本地 durable verified/coverage receipt 升修订（v3/v2），旧记录一律拒绝或强制重验。
- 删除「inbox OID 属于 manifest 资产集即 proven」捷径（必须走 `actual_vv` 绑定）；relay 改写已消费 seq 从 `LocalCorrupt` 改为 `Paused(RelayHistoryRewritten)`。

### 2.2 post-activation 基线消费（原 §3.2）

旧基线（整理前的旧基线）：I 阶段把「T/latest grant 对根的授权」与「当前 checkpoint 自身认证」解耦。

- 保留全部 T 检查（双签、链逐环、control_rev/prev_hash/管理 rev、grant inclusion、根↔epoch、实例 pin）；
- pair 内路径继续强制 T 的 commit digest 强绑定，**不得降级**；
- 非 pair 路径走完整认证：`published`、生产者证书且属 active 成员、`CheckpointCommit` 签名、manifest/refs 复算、实际导入、`S ≥ freeze/gc_floor`、`S ≤ head`、本地水位；
- 激活后普通健康 checkpoint 替换/prune 激活 pair，旧设备仍能 apply（现有负例翻转为正例，另加 12 条负例与多 epoch 正例）。

### 2.3 同一 durable E1 跨两轮 e2e（原 §3.3，T13）

旧基线（整理前的旧基线）：`crates/sync/examples/e2e_rotation_two_rounds.rs` 改为**自包含进程内 relay**（`examples/dev_relay.rs`：临时 SQLite + `FakeClock` + 仅 loopback 放行验签 hook + `127.0.0.1:0` + 独立线程 axum），不再需要 PowerShell/bash 编排，也不新增 dev HTTP 端点。

实际场景与断言：

- B 在 E1 bootstrap 后**全程离线**：identity 文件 SHA256、`current_epoch=1`、grant epoch、cursor/inbox/outbox 快照前后不变，两轮期间不构造 client；
- A：E1→E2（普通轮）→ begin 分配 E3 → `POST /rotations/{r}/restart` 原子废弃 E3 → **E4**；控制链 `epoch.activated=2 / rotation.restarted=1`；
- 两轮后 A 发布 2 份普通 checkpoint 顶掉激活 pair；`FakeClock` 前进 90d + GC×3（`pruned_checkpoints=8, purged_slots=3, gc_floor=3`），激活 pair 变 `pruned`、refs=0，但 manifest/signed_commit/签名保留，旧链仍可复算；
- B 用同一 E1 身份/旧 grant 走真实 HTTP/HPKE/Loro apply 到 E4（`baseline_s=3`）；apply 的
  candidate root 直接使用 B 从 relay latest grant 用**旧 HPKE 私钥解出的根**（先 `ct_eq` 与 A 的
  candidate 交叉断言），不是 A 的测试根；离线编辑与 local-only 保留、水位单调、outbox repack 2/0/0 不重复；
- 负例：E1/E2 根解不开 E4 outbox part 与 E4 基线快照；E1/E2 口令与恢复码 R 解不开 E4 槽；
- 运行：`cargo run -p menghunote-sync --example e2e_rotation_two_rounds` → `TWO_ROUND_EXIT=0`。

### 2.4 tag 后迁移：三条 e2e 全部改为自包含 example（tag 本身未改动）

`v0.4-sync` tag 之后，剩余两条 PowerShell e2e 也迁移为**进程内 relay 的 Rust example**，
`scripts/e2e_rotation_verify.ps1` / `scripts/e2e_rotation_two_devices.ps1` 已删除；
**`v0.4-sync` tag 内仍保留这两条 ps1**（tag 不动，历史版本可 `git show v0.4-sync:...` 查看）。

- `crates/sync/examples/e2e_rotation_verify.rs`：启动 `DevRelay`，在同一 relay DB 上依次调用
  `e2e_localhost::run`（genesis + localhost 同步 + 初始槽/checkpoint）与
  `e2e_rotation::run`（同一 vault 的轮换 D–F），两阶段之间不重启 relay；成功打印 `VERIFY_EXIT=0`。
  `e2e_localhost` / `e2e_rotation` 的 `main` 已拆成可调用入口 + CLI 薄壳，`--relay/--setup` 外部模式保留。
- `crates/sync/examples/e2e_rotation_two_devices.rs`：缺省自起 `DevRelay`，成功打印 `TWO_DEVICE_EXIT=0`；
  显式 `--relay` + `--setup-secret` 仍走外部 relay。
- 至此三条 e2e（two_rounds / rotation_verify / two_devices）都只需 cargo 命令，Windows 不需要 PowerShell。

## 3. v0.4 范围与限制（必须写进 tag 说明）

1. **只支持修复后新建的 vault/身份/控制链**。旧开发 vault 的本地 durable 记录会被新修订拒绝（可重 bootstrap）；但**旧签名 checkpoint 无法仅靠本地版本拒绝隔离**——`bootstrap` 仍会接受修订 1 的签名基线。未实现 `CheckpointCommit.validation_version=2` 之前，不得让旧 vault 继续自动消费旧基线；生产升级路径见 §4。
2. `validation_version=2` 的 proto/relay 联动、receipt HMAC、relay GC 版本门禁、消费者拒绝修订 1：**已由 task-122 完成**（完整预算框架未做），见 [`docs/notes/task-122-validation-v2.md`](../notes/task-122-validation-v2.md)。
3. **不声称 Byzantine 日志完整性**：根认证只能证明「声明为 ASSET 的 seq 有真实密文」，不能证明该 seq 原本不是 UPDATE。relay 仍可隐藏/替换事件；Merkle/inclusion 后置。
4. Linux 本地身份封装是 ADR 0008 的**开发/验证实现**：0600 裸主钥 + XChaCha20-Poly1305，安全弱于 Windows DPAPI，不宣称等价；Windows 生产仍走原始 DPAPI。

## 4. 已知后置/缺陷（事实性说明）

- **task-121（relay reservation 复用）**：轮换期重加密的资产挂在 rotation reservation 上；之后普通 checkpoint 用 checkpoint reservation 重传同一 `object_id`/同字节会被 relay `PUT parts` 判 `409 ID_CONFLICT`（`crates/relay/src/objects.rs:495-501`）。two-round e2e 用 `EmptyAssets` 规避（资产覆盖由激活 pair 承担）。修复方向：同字节跨 reservation 幂等复用且不重复计费，或客户端跳过已存在对象。
- ~~**task-122（验证修订 2）**~~ **已完成（2026-11-19）**：`CheckpointCommit.validation_version=2` + proto/relay 编解码 + 所有消费者拒绝修订 1；receipt HMAC；relay GC 版本门禁。**完整预算框架明确未做**；详见 [`docs/notes/task-122-validation-v2.md`](../notes/task-122-validation-v2.md)。
- `try_record_coverage_receipt` 仍有静默早退（仅浪费 lease/配额，不产生假覆盖）；coverage/verified receipt meta 无清理路径，长期 vault 会增长（ADR 0006 §36.4 已记）。
- Windows 目录 fsync 明确 `PlatformUnsupported`（只声明进程 kill，不声明硬掉电）。
- `RestartBaselineJournal` 明文 meta、与 RotationJournal 非原子（bind/fail-closed）。
- app/生产 `ControlPin` 构造点未落地；非 localhost 消费前必须接持久 pin。
- ASSET 换设备签名种子后旧 skipped-asset HMAC 不可验证 → 需重新 bootstrap。
- 同 epoch orphan 物理 GC、Merkle inclusion、历史/回收站/UI/MCP/生产运维后置。
- 历史/回收站：`docs/briefs/0006-history-and-trash.md`，任务 `task-115/116/117`。
- `scripts/e2e_rotation_two_rounds.ps1` / `e2e_rotation_verify.ps1` / `e2e_rotation_two_devices.ps1` 均已删除（被自包含 example 取代，见 §2.4）；`e2e_localhost` / `e2e_rotation` 仍保留 `--relay/--setup` 外部模式。

## 5. 验证命令

Linux（本机已实际执行）：

```bash
cargo fmt --all
cargo clippy --workspace --exclude menghunote-app --all-targets -- -D warnings
cargo test --workspace --exclude menghunote-app        # 1006 passed / 0 failed
cargo test -p menghunote-sync --all-targets           # 417 passed / 0 failed
cargo run -p menghunote-sync --example e2e_rotation_two_rounds   # TWO_ROUND_EXIT=0
```

Windows（**已由用户于 2026-11-13 在 旧基线 包实测通过**；以下命令可复跑。tag 后 e2e 已去 PowerShell 化）：

```powershell
cargo test --all
cargo run -p menghunote-sync --example e2e_rotation_verify        # VERIFY_EXIT=0
cargo run -p menghunote-sync --example e2e_rotation_two_devices   # TWO_DEVICE_EXIT=0
cargo run -p menghunote-sync --example e2e_rotation_two_rounds    # TWO_ROUND_EXIT=0
```

不需要 PowerShell，也不需要外部 `dev_server`；`e2e_localhost` / `e2e_rotation` 仍保留
`--relay/--setup` 外部模式（需自行起 relay）。

Windows 重点：DPAPI identity 往返与跨 SID 失败语义、`dpapi.rs` 零改动编译、rustls native certs、MSVC `D8050/LNK1104`、ps1 的 `KEY_*` needle。

## 6. 关键文件/函数

- `crates/sync/src/identity_wrap.rs`：平台门面（Windows DPAPI / Linux `MNKW` + 用户主钥，ADR 0008）。
- `crates/sync/src/checkpoint.rs`：`prepare`（pending 候选）/ `validate` / `ensure_coverage_proven` / `verify_coverage_with_receipt` / `coverage_root` / durable v3。
- `crates/sync/src/upload.rs`：`fetch_verified_asset`（共享实证验证器）、`fetch_object`。
- `crates/sync/src/engine.rs`：`pull` / `persist_skipped_asset` v3 / `RelayHistoryRewritten`。
- `crates/sync/src/rotation_apply.rs`：`AuthenticatedCheckpoint` / `verify_checkpoint_authorization` / `verify_activation_t_gate` / I 阶段入口。
- `crates/sync/src/rotation_stage.rs` / `rotation_restart.rs`：`coverage_root`、restart 基线、verified receipt 门禁。
- `crates/dev-relay/src/lib.rs`（原 `crates/sync/examples/dev_relay.rs`，DEV ONLY、仅 `[dev-dependencies]`）+ `crates/sync/examples/e2e_rotation_two_rounds.rs`：自包含 T13 e2e。
- `crates/sync/examples/e2e_rotation_verify.rs`（调用 `e2e_localhost.rs` / `e2e_rotation.rs` 的 `run` 入口）+ `e2e_rotation_two_devices.rs`：tag 后迁移的三条自包含 e2e。
- `crates/relay/src/{checkpoints,rotations,quota,control,log}.rs`；ADR 0006 §31–§36；ADR 0008。

## 7. 开发陷阱

- Linux 主钥路径 `$XDG_DATA_HOME/menghunote/local-identity/key-v1.bin`（0600，目录 0700，ADR 0008）；测试用 `target/tmp/xdg-<pid>` 并调用生产 `initialize_user_key`。删钥=身份不可解，**不会**自动重建。
- two-round e2e 是进程内 relay：`FakeClock` 推进时必须同步客户端 `now` override；外部 `--relay` 模式没有假时钟/GC，会打印 `TWO_ROUND_EXIT=2 (SKIP)`，**不算通过**。
- 显式 `--dir` 必须是空目录（guard 在 genesis/写任何文件之前）；设备身份实际在 `device-a|device-b/identity.bin`，只查顶层 `identity.bin` 会漏过并覆盖旧身份。
- 三条 e2e 都用进程内 relay；`e2e_rotation_verify` 的两阶段必须共享同一 relay DB/vault，不要拆成两次运行（vault 会走重启分支，relay 库也对不上）。`--relay` 外部模式没有假时钟/GC，只作兼容入口。
- 测试临时目录用 `CARGO_TARGET_TMPDIR`；禁止用 sleep 当收敛判据（用 `DurableAck`/明确 phase）。
- 长驻 exe 不要 `cargo run`；先复制到 `target/tmp`，避免 LNK1104/D8050（Windows）。
- 旧 vault/旧签名 checkpoint 不在 v0.4 支持范围（§3）。
- 禁止 `--amend`/改写历史；并发 agent 不要同文件；`git add/commit` 前先 `git status`（见 `docs/notes/commit-attribution.md`）。

## 8. 下一步顺序

1. ~~GPT 收尾复审~~（已完成：APPROVE，tag `v0.4-sync`；整理前锚点 旧基线，等价于当时整理后的提交）。
2. Windows 实机按 §5 复跑，把结果补进本文。
3. ~~`task-121`（relay reservation 复用）/ `task-122`（验证修订 2 与 receipt HMAC、GC 版本门禁）~~ **均已完成**。
4. `task-123`（默认「仅删凭证」吊销端点 + 客户端/UI；轮换改为显式 opt-in）/ `task-124`（metadata 事件认证对账与 cursor 续读）。
4. 历史/回收站 `task-115/116/117` 按 `docs/briefs/0006-history-and-trash.md` 排期。
