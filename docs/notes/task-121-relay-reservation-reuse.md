# task-121：relay reservation 复用（同字节跨 reservation 幂等）

> 结论：**服务端幂等复用**，不改 wire / 不改错误码 / 不改 schema / 不改计费口径。
> 客户端 checkpoint 的 `ID_CONFLICT` 复用探测保留，用于"对象只写了一部分、已有分片
> 属于别的预算来源"的失败恢复与旧 relay 兼容。

## 1. 背景

轮换期重加密的 ASSET 对象由 **rotation reservation** 覆盖（候选 checkpoint 与
`reencrypt_frozen_assets` 共用同一份 `purpose=rotation` 的维护 reservation）；轮换激活后，
普通 checkpoint 为同一 `object_id` 的确定性密文申请的是 **checkpoint reservation**。
`crates/relay/src/objects.rs` 过去在 `put_part` 里先比较 `objects.reservation_id` 与请求里的
`reservation`，不一致就返回 `409 ID_CONFLICT`——即使该分片逐字节已经在库中。

## 2. 修复点（`crates/relay/src/objects.rs::put_part`）

判定顺序调整为：

1. 信封结构 / 租户 / 设备 / epoch 屏障不变；
2. 同 `object_id` 的固定上下文（epoch/message_id/producer/type/generation/sender_seq/
   part_count/plain_len）必须一致，否则 `409 ID_CONFLICT` 不变；
3. **先**查该分片是否已存在：
   - 已存在且 digest 相同 → `200 OK` + `duplicate=true`（与 reservation 无关）；
   - 已存在但字节不同 → `409 PART_CONFLICT`；
4. **再**检查"补缺失分片"的预算来源一致性：对象行已有 `reservation_id` 与本次请求不同
   → `409 ID_CONFLICT`（同字节重放已在第 3 步返回，不受影响）；
5. 其余（计费 + 落片）不变：`duplicate` 路径不扣预算、不写 `object_parts`、不新增对象行。

### 幂等语义

| 场景 | 结果 |
| --- | --- |
| 同一分片、同字节、不同 reservation（rotation↔checkpoint、checkpoint↔normal） | `200 duplicate=true`，不重复计费、对象/分片只存一份 |
| 同一分片、不同字节（任意 reservation） | `409 PART_CONFLICT` |
| 同 `object_id` 的 header 上下文不同 | `409 ID_CONFLICT`（不变） |
| 对象已 complete 后再补**缺失**分片 | `409 ID_CONFLICT`（不变） |
| 对象只写了一半，用**不同**预算来源补缺失分片 | `409 ID_CONFLICT`（保持不变，见限制） |

## 3. 是否改 wire

**没有。** 响应仍是既有的 `{object_id, part_no, digest, duplicate}`：`duplicate=true` 时
HTTP 200（原本同 reservation 的同字节重放就是这样），首次写入仍是 201。没有新增错误码、
没有新增字段、没有 schema 迁移；`sync-proto` 未改动。

## 4. 测试

- `crates/relay/tests/objects.rs`
  - `same_bytes_replay_is_idempotent_across_normal_and_checkpoint_reservations`：
    normal→checkpoint、checkpoint→normal 两个方向的同字节重放均为 duplicate，
    `usage` / 对象数 / 分片数 / reservation `consumed_bytes` 都不变；不同字节仍
    `PART_CONFLICT`。
  - `rotation_reservation_object_is_reusable_by_a_checkpoint_reservation`：
    完整轮换（begin → 候选基线 → 激活）后，用新的 checkpoint reservation 重放
    rotation reservation 写入的 ASSET 分片 → duplicate（不再 409），不同字节仍冲突。
  - `missing_part_across_reservations_still_requires_the_same_budget_source`：
    两片对象只写了 part 0 时，换 reservation 补 part 1 仍 `ID_CONFLICT`；同来源补齐后
    两片都能跨 reservation 幂等重放。
- `crates/sync/tests/rotation_dev_local.rs`
  - `rotated_asset_is_reused_by_an_ordinary_checkpoint`：真实进程内 relay，A 上传资产
    → 轮换（rotation reservation 重加密）→ B apply 拿到字节 → 清掉 A 的 durable
    `sync.asset_uploaded.*` 标记（模拟另一台设备）→ A 用普通 checkpoint 重传同一资产
    成功，且新 checkpoint 引用的 `object_id` 与轮换期一致；再用新建 checkpoint
    reservation 直测 `PUT part` 同字节成功、不同字节 `PART_CONFLICT`。
- `crates/sync/examples/e2e_common.rs`：`publish_ordinary_pair` 的资产源从
  `EmptyAssets` 改回 `StorageAssets`，two-round e2e 因此覆盖"轮换后普通 pair 引用
  同一批资产"的真实路径（不再靠 EmptyAssets 规避）。实跑
  `cargo run -p menghunote-sync --example e2e_rotation_two_rounds` 通过，日志中两轮
  轮换都是 `frozen assets count=1`、`reencrypted=1`，最后 `TWO_ROUND_EXIT=0`。

本机实测（Linux，2026-11-18）：

```text
cargo fmt --all -- --check                                            # 干净
cargo clippy -p menghunote-relay -p menghunote-sync -p menghunote-sync-proto
             --all-targets -- -D warnings                             # 干净
cargo test -p menghunote-relay                                        # 376 passed / 0 failed（基线 373，+3）
cargo test -p menghunote-sync                                         # 614 passed / 0 failed（基线 613，+1）
cargo test -p menghunote-sync-proto                                   #  61 passed / 0 failed
```

## 5. 已知限制

- **严格意义上的 checkpoint→rotation 复用不可达**：候选 epoch 的 checkpoint reservation
  就是该轮 rotation 的 reservation（relay `rotation_reservation()` 复用同一份），不存在
  "checkpoint reservation 写入的对象再换另一份 rotation reservation 重放"的合法状态机；
  实际会发生的跨 reservation 方向是"rotation 写入 → 激活后普通 checkpoint 重放"，
  另有 normal↔checkpoint 双向（两端都有测试覆盖）。幂等判定本身与 reservation 无关，
  所以也不会因为传了别的 reservation 而拒绝同字节重放。
- **部分分片 + 跨 reservation 续传**仍拒绝（不混拼预算来源/不绕过 reservation owner
  校验）。这是有意保留的保守语义：本次只保证逐字节重放的幂等复用。
- `EmptyAssets` 仍用于 `e2e_localhost` 的"第二份 checkpoint 无资产"路径（那是专门验证
  空资产基线，不是规避本缺陷）。
- relay 的 `objects.reservation_id` 仍是对象创建时的预算来源（历史事实）；激活后
  `budget_class` 由 `quota::recompute` 按 live 集合重算，二者语义不变。
- 客户端"已存在则跳过"的优化仍由既有的 durable `sync.asset_uploaded.*` 标记与
  `read_part` 探测承担；本次没有新增客户端状态，也没有新增 wire 字段。
