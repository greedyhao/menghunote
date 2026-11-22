# task-123：默认删凭证吊销 + 轮换显式 opt-in

> 结论：**能力已在树内由 ADR 0010（`d143489`）实现**；本轮（2026-11-19）做的是
> **验收补齐**：逐项审计默认语义与 relay 门禁、补 epoch 不变性断言、补
> 「默认移除 → 显式轮换」的组合 e2e、把兼容/错误码/边界写清。
> **不改** wire（MNE1 信封/密钥槽/控制记录/`/state` 字段）、**不改** schema、
> **不新增**错误码、**不新增**端点、**不改** app UI（`crates/app/**` 未触碰）。

## 1. 默认语义（移除设备 = 只吊销，不轮换）

| 层 | 入口 | 行为 |
| --- | --- | --- |
| relay | `POST /v1|v2/vaults/{vault}/devices/{device}/revoke`（`crates/relay/src/api.rs::revoke_device` → `crates/relay/src/revocation.rs::revoke_access`） | `devices.status='revoked'` + `revoked_at` + `vault.auth_rev += 1`；**不**推进 `control_rev`、**不**追加控制记录、**不**分配/切换 `epoch`、**不**冻结 vault、**不**删证书/grant/对象/日志 |
| sync 客户端 | `DevLocal::revoke_device`（`crates/sync/src/dev_local.rs`）→ `device_management::revoke_device`（`crates/sync/src/device_management.rs`） | 本地守卫（禁止 self-remove、必须有与身份 pin 一致的管理私钥）→ 管理 realm 认证 → 管理签名 `POST` → 逐字段校验响应 → 只读刷新成员视图。**不删除**身份/`manager_seed`/恢复材料/笔记，**不调用** `rotate` |
| app（既有，本轮不改） | dev-only 面板「移除设备（不轮换密钥）」 | 文案固定「仅断访问」；轮换是**另一个**独立按钮（默认隐藏，`MENGHUNOTE_EXPERIMENTAL_ROTATION=1`） |

固定响应：`200 CDE{0: target, 1: "revoked", 2: changed(bool), 3: auth_rev, 4: control_rev}`
（`control_rev` 与请求的 CAS 值逐字节一致；`changed=false` 表示此前已 revoked）。

## 2. relay 门禁（吊销后该设备还能做什么）

| 路径 | 结果 | 依据 |
| --- | --- | --- |
| 设备 realm challenge（`/v1/auth/challenges`、v2 device challenge） | `403 REVOKED`（未注册/非 active 同一码，避免枚举） | `auth::create_challenge`：`status != active` → `REVOKED` |
| 旧 token（任何数据面 `GET/POST`，含 `/state`、`/log`、`/updates`、对象、checkpoint、bootstrap、granular grants） | `403 REVOKED`（`auth_rev` 已前进；token 行保留到自然过期便于诊断） | `auth::authenticate`：`token.auth_rev < vault.auth_rev` → `REVOKED` |
| 新认证（realm 1 device） | 拒绝（challenge 阶段即 `REVOKED`） | 同上 |
| WS（已建立的订阅） | 每帧重鉴权；下一次推送/唤醒即 `close(1008)` | `api.rs` WS 循环每轮 `auth::authenticate` |
| 设备证书路径（`devices/{device}/approve`） | `403 REVOKED`，**永不复活** | `pairing::approve_device`：`status == "revoked"` → `Revoked` |
| 重新 redeem 邀请 | 同 device_id `409 ID_CONFLICT`（设备 id 永不复用）；新 id 只能成为 pending，需管理端批准 | `pairing::redeem_invite` |
| 签名请求（rotation begin / revoke-more / management 写） | 设备 **realm 1** bearer 已被拒，数据面签名提交因此无法进入；管理写需要**管理 realm** bearer + 管理私钥签名，被吊销设备的设备证书不授予管理权 | `management::run` / `auth::verify_management`（管理材料残留是 §7 的边界，不是本端点能撤销的） |
| V2（schema 12+，`authz_state != legacy`） | 旧 `/devices/{device}/revoke` 一律 `403 MANAGEMENT_REQUIRED`（ADR 0015 关闭 v1 管理写）；默认移除走签名 `DeviceRevoke`（kind 5），同样**不轮换** | `auth::require_legacy_auth`；ADR 0015 |

**权限**：该端点只接受「管理 realm bearer + 当前管理公钥签名 + `expected_control_rev` CAS」。
普通设备 bearer → `403 MANAGEMENT_REQUIRED`；错管理私钥 / 签名被篡改 → `403 MANAGEMENT_REQUIRED`；
`path`/外层镜像与被签 body 不一致 → `403 FORBIDDEN`；新请求过期 CAS → `409 CAS_CONFLICT`（带当前
`control_rev`）。**不存在**"设备自提权吊销别人"的路径，也没有普通设备可用的管理端点。

**幂等 / 守卫**（`revocation.rs::revoke_access`，顺序语义相关）：

- 目标不存在 → `404 NOT_FOUND`；已 revoked → `200 changed=false`（**不推进任何版本**，轮换期间也一样）；
- pending 目标 → `409 INVALID_STATE`（空证书占位不能直接置 revoked，否则旧客户端整条控制链 fail-stop）；
- 唯一 ACTIVE 目标 → `409 INVALID_STATE`（同事务内数 ACTIVE 并更新，两台并发互删最多成功一台）；
- ACTIVE 目标 + vault `rotating` → `409 ROTATING`；`initializing` → `409 INVALID_STATE`；
- 失败请求不写 dedup 行、不改状态、不推进 rev；
- 相同 `request_id` + 相同被签 body → 逐字节回放原响应（`management::run` dedup）；同 id 不同 body → `409 ID_CONFLICT`。

## 3. epoch 是否变化

**不变。** 独立吊销与 `key_epoch` 完全解耦：

- 只有 `vault.auth_rev` 前进（全 vault 旧 bearer 立即失效，各设备**一次**重新认证，数据面不受影响）；
- `vault.active_epoch` / `next_epoch` / `state` / `control_rev` 不变，`epochs.revoked_once` **不**被置位
  （`revoked_once` 只由**轮换期间**的真实 status 迁移产生，见 `rotations.rs`）；
- 客户端侧 `LocalIdentityV1.current_epoch`（A 与 B）不变；
- 断言位置：relay DB 快照 + wire `/state`（键 3/4/5）+ 同步 e2e 的身份 epoch（见 §6）。

## 4. 轮换 = 显式 opt-in

- 唯一轮换入口是 `DevLocal::rotate`（`crates/sync/src/dev_local_rotation.rs`）；
  `revoke_device` / `device_management` **不**引用它，也没有"移除失败就轮换"的降级路径。
- `rotate` 的 `revoke[]` 允许、并且应当包含**此前已被独立吊销**的设备（ADR 0010：
  让 signed revoke 也覆盖 status-only 撤销）：
  - 这类目标的 `auth::revoke_device` 返回 `changed=false`，所以 relay 的
    `rotations.revoked_count` 只统计**本次真实的 status 迁移** → 对"只含已 revoked 目标"的轮换为 `0`；
  - 但 `revoke[]` 仍在**被管理签名的 begin body** 里（客户端可逐字节复算 digest），
    `/control` 的成员推导与 grant 规划都按"已吊销"处理，被移除设备**不**进入 active 集合、
    **不**拿新 HPKE grant；
  - 这两种事实（`revoked_count=0` 与 signed `revoke[]` 非空）不矛盾，测试同时断言。
- 被移除设备不能借后续轮换重新入网：`apply_latest_epoch` 在 `/state` 认证阶段拿到 `REVOKED`
  → 返回 `ApplyOutcome::NotGranted`，身份文件逐字节不变（不推广、不回退）。

## 5. 错误码与兼容

**未新增错误码**；全部复用 ADR 0006 §5 的既有码：
`REVOKED` / `MANAGEMENT_REQUIRED` / `FORBIDDEN` / `NOT_FOUND` / `INVALID_STATE` / `ROTATING` /
`CAS_CONFLICT` / `ID_CONFLICT` / `INVALID_REQUEST`。

| 组合 | 行为 |
| --- | --- |
| 旧客户端（不认识该端点）+ 新 relay | 不调用 = 无变化；旧客户端仍可用 signed `rotation.begin` 的 `revoke[]` / `revoked-more` 移除设备。relay 端点是纯增量，旧客户端不解析新响应 |
| 新客户端 + 旧 relay（无该端点） | `404`：body 可解析时 `SyncError::RelayApi{code:"NOT_FOUND"}`，否则 `SyncError::HttpStatus(404)`；**不会**退化成轮换（客户端没有"404 就 rotate"的分支） |
| 新客户端 + 旧 relay 的 `REVOKED` | 该错误码在 ADR 0006 就存在，映射不变（`SyncError::RelayApi`），被吊销设备照常 fail-closed |
| 新客户端 + 新 relay | 本地守卫先于任何网络动作（self-remove / 缺管理 key / 管理公钥与身份 pin 不符 → `SyncError::Unsupported`，0 请求） |
| 响应丢失 / 断线重试 | 每次调用新 `request_id`（ADR 0010：不拿旧 id 撞 relay）；服务端以 `status` 与 dedup 双保险：重试要么 `changed=false`（幂等），要么命中同 id 同 body 的逐字节回放。没有 durable intent/journal（ADR 0010 设计），不会在本地留半状态 |

wire 兼容：MNE1 信封、密钥槽、控制记录/事件、`/state` 字段编号均**未改**；无需 `sync-proto` 变更。

## 6. 测试（本轮补强）

新增/补强（全部在允许写作用域内）：

- `crates/relay/tests/management_revoke.rs::revoking_active_device_cuts_access_and_replays`
  - 追加：DB 侧 `active_epoch` / `next_epoch` / `state` 与吊销前**逐项相等**（默认移除不轮换）；
  - 追加：重新认证后 wire `/state` 的 `epoch` / `state` / `control_rev` 与吊销前一致。
  - 既有覆盖（本文件其余 6 个测试）：旧 token 立即 `REVOKED`、新 challenge 拒绝、`/control`
    保留证书历史但 active 集合排除、重放/mirror/CAS/并发互删/唯一 ACTIVE/pending/
    initializing/rotating 守卫、失败无副作用。
- `crates/sync/tests/device_revocation.rs` 既有测试（本轮未改）覆盖权限负例与幂等细节：
  `helper_refuses_self_remove_and_wrong_manager_before_any_request`（self / 错管理 key → 本地
  拒绝，**0 请求**）、`dev_local_revoke_refuses_self_and_missing_manager_key_without_side_effects`
  （Join 设备没有管理密钥 → 本地拒绝）、`revoke_is_idempotent_and_failures_have_no_side_effects`
  （重复/失败无副作用）、`auth_rev_bump_refreshes_cached_members_without_control_rev_change`
  （`auth_rev` 缓存失效）、`member_view` 系列（吊销后成员视图）。
- `crates/sync/tests/device_revocation.rs::remove_is_revoke_only_and_rotation_is_explicit_opt_in`（新增）
  1. 默认移除 `changed=true`、`control_rev` 不变、`auth_rev+1`；relay `/state` 的
     `epoch`/`control_rev` 与 A/B 本地身份 epoch 全部不变；成员视图 B=Revoked、A=Active；
  2. 重复移除 `changed=false`，`auth_rev`/`epoch` 不再推进；
  3. A 在移除后 `sync_once` 与**新本地编辑**都能同步，epoch 仍不变；B 的同步 `REVOKED`
     且本地正文仍可读；
  4. 显式 `DevLocal::rotate`：`from_epoch → +1`、`revoked=[B]`、`grants=1`、A 推广到新 epoch；
     signed begin body 的 digest 能用含 `revoke=[B]` 的公开 API 复算；relay view
     `state=activated`、`revoked_count=0`（B 在轮换前已 revoked，无二次 status 迁移）；
  5. B 的 `apply_latest_epoch` → `NotGranted{local_epoch}`，身份文件逐字节不变；
  6. A 在新 epoch 继续同步。

全量（本机 Linux，2026-11-19；命令见 §8）：

- `cargo test -p menghunote-relay`：**380 passed / 0 failed**（新增 0 个测试，主用例追加了一致性断言）
- `cargo test -p menghunote-sync`：**618 passed / 0 failed**（新增 1 个测试；原 617）
- `cargo test -p menghunote-sync-proto`：**64 passed / 0 failed**
- `cargo clippy -p menghunote-relay -p menghunote-sync -p menghunote-sync-proto --all-targets -- -D warnings`：干净

## 7. 未做 / 边界（诚实清单）

- **不是密码学撤销**：独立吊销只改 relay 访问状态；被移除设备仍持旧 root，能解它已获得的
  同 epoch 密文。恶意 relay 可把 `status` 翻回 `active`——客户端成员视图 fail-closed
  （不把无已签加入证据的证书当 active），但**不能**阻止恶意 relay 把密文给它。
  要"未来数据连拿到也解不开"必须显式轮换。
- **不撤销管理权**：目标若持有 `manager_seed`/恢复码，仍能通过管理 realm 认证、批准新身份；
  隔离管理材料泄漏需要显式轮换管理密钥/恢复码（ADR 0010 明示，relay 无法识别谁持有管理私钥）。
- **V2 vault**：legacy 端点返回 `403 MANAGEMENT_REQUIRED`；默认移除是签名 `DeviceRevoke`
  （kind 5，双签、级联 cap、`authz_rev/auth_rev` +1，同样**不**轮换）。本笔记不重复其测试细节。
- **无 durable intent**：独立吊销按 ADR 0010 是"无本地副作用的单请求 + 幂等重试"，
  没有 journal；响应丢失后重试是新的 `request_id`（`changed=false`），不会二次推进 `auth_rev`。
- **轮换期间不能独立吊销 ACTIVE 设备**（`409 ROTATING`）：轮换期用 `revoke-more`（同样进入
  signed `revoke[]`），不是 task-123 的默认路径。
- **未改 app UI**：dev-sync 面板与产品设置页保持原样；产品设置页目前没有设备管理入口。
- 本轮未改 `AGENTS.md` 的"后置 task-123"清单（任务要求不编辑）；该条目已因 ADR 0010
  的落地而过时。

## 8. 复跑命令（Linux，本机）

```bash
export CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 \
  LIBRARY_PATH=/tmp/ui-test-probe/sysroot/usr/lib/x86_64-linux-gnu \
  PKG_CONFIG_PATH=/tmp/ui-test-probe/sysroot/usr/lib/x86_64-linux-gnu/pkgconfig

cargo fmt --all -- --check
cargo clippy -p menghunote-relay -p menghunote-sync -p menghunote-sync-proto --all-targets -- -D warnings
cargo test -p menghunote-relay
cargo test -p menghunote-sync
cargo test -p menghunote-sync-proto
```

本机结果：`fmt` 干净、`clippy` 干净、relay **380** / sync **618** / sync-proto **64** 全绿。
（工作树里同时有其它并行 agent 未提交的 `crates/app/**` / `crates/i18n/**` 改动；
本轮只改动了上面列出的 relay/sync 测试与两个 sync 源文件的文档注释、以及本笔记。）
