# task-122：验证修订 2（CheckpointCommit v2 + receipt HMAC + GC 版本门禁）

> 状态：**已完成**（`dev` 工作树；未 commit）。范围按交接文档
> [`0007-sync-handoff.md`](../briefs/0007-sync-handoff.md) §3/§4/§8 的 task-122 条目。
> **不改** ADR 0006 已冻结的 MNE1 信封 / 密钥槽 / 控制链 / 鉴权语义，也不改
> `ObjectCommit`/`ManifestV1`/`ActivationTV1` 的既有字段与签名域。

## 1. wire 修订：`CheckpointCommit.validation_version = 2`

`crates/sync-proto/src/records.rs` 的 `CheckpointCommit` 新增字段：

| 字段 | v1 | v2（当前） |
| --- | --- | --- |
| 0..7 | 不变 | 不变 |
| 8 `validation_version` | `1` | `2` |
| 9 `validation_digest` | **不存在**（解码时 `[0u8;32]`，不编码） | 32B，必须非全零 |

**解码规则（不猜测）**：

- 9 字段 + 版本 1 → 合法 v1；`validation_digest` 为空零；
- 10 字段 + 版本 2 → 合法 v2；
- 9 字段 + 版本 2 → `MissingField(9)`（不把缺失摘要猜成 0）；
- 10 字段 + 版本 1 → `Invalid`（不丢弃多余字段静默降级）；
- 版本 0/3/… → `UnsupportedVersion`；
- 9/10 以外字段数 → `Invalid`。

`CheckpointCommit::CURRENT_VALIDATION_VERSION = 2`（同时由
`menghunote_sync_proto::CHECKPOINT_COMMIT_VALIDATION_VERSION` 导出）。
新增 `is_current_validation()` / `require_current_validation()`：后者对 v1 返回
`ProtoError::UnsupportedVersion(1)`，供所有消费者显式拒绝（解码仍允许 v1，只用于识别、
审计与负例）。

### 1.1 `validation_digest` 的口径

v2 的签名 commit 额外承诺"生产者在 validate 阶段真实做过的覆盖/导入证据"：

```
validation_digest = SHA256(
    b"MN/v1/checkpoint-validation\0"
    || CDE(CheckpointValidationV2)
)
```

`CheckpointValidationV2`（`sync-proto`，**不单独上 wire**）字段（CDE map 0..14）：

| key | 字段 |
| --- | --- |
| 0 | vault_id 16B |
| 1 | checkpoint_id 16B |
| 2 | epoch |
| 3 | covered_relay_seq (`S`) |
| 4 | coverage_epoch（prepare 冻结的 relay active epoch） |
| 5 | manifest_object_id 32B |
| 6 | manifest_digest 32B |
| 7 | refs_digest 32B |
| 8 | producer_device_id 16B |
| 9 | imported_docs |
| 10 | imported_vv_digest 32B |
| 11 | verified_assets |
| 12 | coverage_durable_assets |
| 13 | coverage_verified_candidates |
| 14 | coverage_proof_digest 32B |

客户端 `CheckpointWriter::validate` 从不可变 `BaselineFacts` 重算该摘要并写进
`VerifiedBaseline`（durable v4）；`promote` 重算对撞后才签 v2 commit；relay 只看到
32B 摘要（不声称能复算覆盖）。`checkpoint_commit_digest`（rotation T 的
`primary/fallback_commit_digest`）改为只取 `CheckpointReport.commit_digest`——即 promote
实际签名的字节 SHA256，不再用公开字段另拼 commit。

> 这是 wire 的**显式修订**，不是静默改已有字段；旧 9 字段记录仍可解码用于拒绝/审计。
> `ActivationTV1` 的 `primary/fallback_commit_digest` 口径仍是
> `SHA256(CDE(CheckpointCommit))`，但 v2 起 CDE 输入包含 `validation_digest`；T 的
> 字段编号/长度/双签域不变（`MN/v1/activation-t\0`），不属于控制链语义变更。

## 2. 兼容 / 拒绝策略（v0.4 迁移）

| 消费方 | 策略 |
| --- | --- |
| `sync-proto::decode` | 接受 v1/v2；未知版本拒绝；字段集合必须与版本严格匹配 |
| relay `POST /checkpoints/{cid}/publish` | **只接受 v2**；v1 与未知版本 → `400 INVALID_REQUEST`，在幂等返回/状态写入/健康标记/GC 计划之前失败；行 `validation_version` 写 2 |
| relay candidate promote / `commit_binding_issues` | 要求 commit 为当前修订且与行版本一致 |
| relay bootstrap / activation / GC 基线选择 | SQL 只选 `validation_version = 2` 的 published 行；不足两份 → `NO_VALID_BASELINE` / GC `blocked` |
| sync bootstrap | 解码后 `require_current_validation()`；v1 基线 → `UnsupportedVersion(1)`，零写入/零水位 |
| sync rotation apply / `checkpoint_manifest` | 同样拒绝 v1 |
| storage bridge | 只消费 sync 已验证的产物（不接触 `CheckpointCommit`），因此由 sync 门禁覆盖 |

**没有"双读/自动升级"路径**：v1 签名基线不会被迁移成 v2（那会伪造覆盖证据），也不会
被新客户端静默消费。旧开发 vault 若仍有 v1 published 基线，必须重新 bootstrap/创建；
relay 升级后也不接受新的 v1 publish。旧 v1 行可以留作历史，但**不能作为 GC 的保留根**
（见 §4）。

## 3. 本地 receipt HMAC（coverage pending + verified）

### 3.1 密钥来源与域分离

新增（**不进 wire、不落日志、不 Debug**）：

```
K_receipt = HKDF-SHA256(
    IKM  = K_epoch（当前 checkpoint 的 epoch 根）,
    salt = vault_id,
    info = b"MN/v1/checkpoint-receipt-key\0",
    32B
)
```

- 每个 epoch 的根重新随机；退役根不再可用于新回执。**不重用**根、`K_object`、
  `K_asset_id`、槽 KEK、设备签名私钥、relay cursor 密钥或任何签名域；
- 只用于本地 durable receipt 的 HMAC，不加密数据、不上 wire；
- `Secret32` 无 `Debug`/`Display`，HMAC 只在栈上做，不写日志。

### 3.2 MAC 输入与格式

```
tag = HMAC-SHA256(K_receipt,
        b"MN/v1/checkpoint-receipt-mac\0" || kind_u8 || body)
```

| receipt | magic（新修订） | kind | 变更 |
| --- | --- | --- | --- |
| coverage pending | `MN/cp-pending/3\0` | 1 | 原 SHA256 校验和 → HMAC；旧 v1/v2 拒绝 |
| verified | `MN/cp-verified/2\0` | 2 | 加 `validation_digest` + HMAC；旧 v1 拒绝 |

- 校验顺序：magic/长度 → **HMAC（常数时间 `verify_slice`）** → 字段解析/绑定检查。
  HMAC 失败一律 fail-closed（`LocalCorrupt`/`Object`），不得降级成"只重验"。
- `VerifiedBaseline` durable 修订升到 **v4**（新增 `coverage_epoch` 与
  `validation_digest`）；v1/v2/v3 字节一律拒绝，禁止改号升级。
- `load_verified_receipt(vault, root, cid)` 必须用同一 epoch 根派生密钥；epoch 不符也拒绝。

### 3.3 失败语义

- relay publish 验证失败（签名/版本/完整性）发生在任何 published/健康/GC 计划写入之前；
  该 checkpoint 不会成为 "covered" 依据（测试断言行仍 `staging`、无 GC 计划）。
- 客户端只有 `validate` 的覆盖复核全部成功后才写 verified receipt；`promote` 发布成功
  后才落盘；`resume_promoted_baseline` / restart 快捷恢复只认 HMAC 通过的新回执。
- 篡改 coverage/verified receipt 的 body、换根密钥、旧 magic 都会在读取时失败；不会
  被当成覆盖成功或 restart 证据。

## 4. relay GC 版本门禁

- `gc::published_any_newest_first` 只返回 `validation_version = 2` 的 published 行。
- 保留根不足两份当前修订 → `GcOutcome.blocked = true`，`cut_seq/floor` 不动，
  **不删日志/对象/不推进 gc_floor**，即使存在更多旧修订 published 行。
- 旧修订行只作为历史行/可被常规 retire 流程清理，不参与"两基线"计算，也不作为
  cut/tombstone 的依据。
- `published_for_epoch`（rotation activate 的"两份候选"门禁）同样只统计当前修订。

## 5. 完整预算框架：**明确未做**

task-122 交接条目里的"完整预算框架"本次**没有落地**（按任务允许的降级：先完成 1–3，
并把预算框架列为明确未做，不做半成品）。原因：

- 现有 ADR 0006 §7 已有 `Q/R`、维护 reservation、`future_live_bytes`、发布前 507 等
  机制；"完整预算框架"若指逐类账本/磁盘门禁/前台退避/资产 GC 的统一模型，需要新的
  ADR 级决策与 schema/语义改动，超出本修订"只做协议门禁"的边界；
- 在未定义预算类别与可回滚语义前硬塞半套账本，会与现有 `quota::recompute` 形成两套
  真相。

后续若要做，建议单独立项并在 ADR 记录：预算类别/归属、发布前与 GC 后对账、磁盘上限、
退避与拒绝码、旧库迁移。本任务**没有**新增/修改任何 quota/预算代码。

## 6. 测试（Linux 实测）

- `cargo test -p menghunote-sync-proto`：**64 passed / 0 failed**。新增 v1/v2 字段门禁、
  未知版本、v2 摘要输入确定性/字段敏感性；含既有 golden。
- `cargo test -p menghunote-relay`：**380 passed / 0 failed**。新增
  `tests/checkpoint_validation_v2.rs` 4 条（v1 publish 拒绝且零状态、未知版本拒绝、
  行版本落 2、v1 基线不能当 GC 保留根）。
- `cargo test -p menghunote-sync`：**617 passed / 0 failed**。新增客户端拒绝 v1 baseline
  （bootstrap 零写入）、receipt HMAC 篡改/换根拒绝、v2 commit 的 `validation_digest`
  绑定断言。
- `cargo test -p menghunote-app --features dev-sync`：**680 passed / 0 failed**
  （含其它 agent 并行改动的新增用例；本任务未改 app 源码）。
- `cargo run -p menghunote-sync --example e2e_rotation_two_rounds`：
  **EXIT=0 / TWO_ROUND_EXIT=0**，验证 v2 checkpoint/回执/轮换/同步闭环。
- `cargo fmt --all -- --check` 干净；
  `cargo clippy -p menghunote-sync -p menghunote-sync-proto -p menghunote-relay --all-targets -- -D warnings` 干净。

## 7. 未做 / 已知限制

- 完整预算框架（见 §5）。
- relay 对 v1 published 行的数据迁移/清理：不迁移，只禁止其作为基线与 GC 依据。
- v2 `validation_digest` 的**第三方可复算性**：relay/其他设备没有生产者的覆盖证据原文，
  只能信任签名；这仍是"生产者自证 + relay 不伪造"的口径，不声称 Byzantine 完整性。
- `coverage pending`/verified receipt 的 meta 行长期清理仍属后续治理项（ADR 0006 §36.4）。
