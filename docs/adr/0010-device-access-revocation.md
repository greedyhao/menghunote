# ADR 0010 — 设备访问吊销（默认“仅删凭证”，不轮换）

> 状态：**已决**（2026-11-13，GPT 决策；实现分两片）。
> 关联：[ADR 0006](0006-sync-v1.md)（控制链/轮换）、[ADR 0009](0009-sync-ui-integration.md)（app 同步 UI）。
> 用户决定：设备移除**默认只删凭证/断访问**；密钥轮换（旋转）是显式 opt-in。

## 背景与问题

同步是端到端加密：relay 只存密文，删除设备凭证只能阻断**访问**，不能让已持有旧 root 的设备失去解密能力。
用户接受这一默认（便宜、常用），但当前协议没有独立吊销入口：`relay::auth::revoke_device` 只在 rotation
流程里被调用，API 仅有 `devices/{device}/approve` 与 `rotations/{rotation}/revoke-more`。

同时存在两个硬约束：

1. **不能空增 `control_rev`**：客户端要求 `/control` 的签名链尾精确等于 `control_rev`
   （`http_relay.rs` 的链尾校验）。独立吊销是 relay 侧访问状态，不是签名控制事实，因此只推进 `auth_rev`，
   `control_rev` 不变，也不追加占位记录。
2. **pending 设备不能直接置 `revoked`**：pending 成员是空证书占位；变成 revoked 会让现有客户端
   按“非待批准成员证书为空”整条控制链 fail-stop。取消待批准设备另做；本端点对 pending 返回
   `409 INVALID_STATE`。

服务端**无法识别哪个设备持有管理私钥**（管理 key 是 HKDF(R) 派生的独立 Ed25519，不是设备证书字段），
因此不能实现“最后一个管理设备”守卫；能做的是“不能吊销到 0 台 ACTIVE”。保留一台 ACTIVE 也**不证明**
管理密钥仍被保存——必须要求用户保留最新恢复码备份。

## 决策

新增管理端 HTTP 端点（API 增量，不改 MNE1/控制记录/DDL/磁盘格式）：

```text
POST /v1/vaults/{vault}/devices/{device}/revoke

B = CDE{0: target_device(bytes16), 1: request_id(bytes16), 2: expected_control_rev(uint) }
wire = { 0: { 0: bytes(B), 1: signature64 }, 1: request_id, 2: expected_control_rev }
```

- 管理 bearer + 当前管理密钥签名；签名绑定 `POST=2`、完整 path、instance/vault、request_id、
  expected_control_rev、canonical body。path 的 target、外层 request_id/rev 必须与 B 一致。
- 去重 scope 固定 `management/device-revoke`；target 在 body 内，防跨设备碰撞。
- 响应：`200 CDE{0: target, 1: "revoked", 2: changed(bool), 3: auth_rev, 4: control_rev}`。
- 沿用 `management::run`：验签 → 去重 → control CAS → 业务 → 同事务缓存响应。

### 守卫与幂等

- 目标不存在 → `404 NOT_FOUND`；
- 已 revoked → `changed=false`，不推进任何版本（含轮换期间的无操作重复请求）；
- pending → `409 INVALID_STATE`；
- ACTIVE 目标且 vault rotating → `409 ROTATING`；initializing → `409 INVALID_STATE`；
- **同一 `BEGIN IMMEDIATE` 内数 ACTIVE 并更新**；移除后 ACTIVE 不足 1 台 → `409 INVALID_STATE`；
  两台设备并发互删最多成功一个；
- 真变化只改 `status/revoked_at` 并调 `auth::revoke_device` 推进 `auth_rev`；保留证书、grant、历史，
  不换 root/epoch，不冻结；
- `control_rev` 不变、不追加占位记录；
- 错误 realm/管理签名 → `403 MANAGEMENT_REQUIRED`；镜像不符 → `403 FORBIDDEN`；非法 body →
  `400 INVALID_REQUEST`；新请求过期 CAS → `409 CAS_CONFLICT`（携当前 rev）；
- 同 id 同 B 回放原响应；同 id 不同 B → `409 ID_CONFLICT`；验签先于回放，成功重试不再推进 `auth_rev`。

### 客户端 helper

- 新建 `sync::device_management`，短接口：
  `revoke_device(transport, ctx, &ManagementKey, target, request_id, rev) -> RevokeOutcome`。
- 复用 `recovery_enroll::management_token` 与既有签名/CDE 函数；密钥来自受保护身份的 `manager_seed`
  或**最新恢复码**；核验当前有效管理公钥；绝不用设备签名 key。
- 标准 helper/UI **禁止移除本设备**；缺管理 key 直接拒绝；不删除本地身份、`manager_seed`、
  恢复材料或笔记。
- **缓存失效必须看 `auth_rev`**：`auth_rev` 是全 vault 的，连调用者 management bearer 和旧 token
  都失效；成功、REVOKED、认证刷新及 `/state.auth_rev` 变化都应使 token/active-control 缓存失效，
  不能只比较 `control_rev`。
- 响应丢失：重新管理认证，以同 id/B 重试；不盲改 rev。CAS 冲突则刷新列表、确认后使用新 id。

## 安全边界（UI/文档必须写明）

- 默认操作是**诚实 relay 上的设备数据访问撤销**，不是签名成员移除，也不是密码学撤销；
- 被移除设备仍持旧 root，能解它已获得/之后从其它渠道拿到的同 epoch 密文；
- 单纯 status 撤销可被恶意 relay 恢复；已有 signed rotation 撤销则不能被 status 翻转恢复；
- **若目标持有 `manager_seed`/恢复码，设备撤销不撤销管理权**；它仍可能管理认证、批准新身份；
  隔离管理密钥泄漏必须显式轮换管理密钥/恢复码；
- 显式轮换应把此前 status-only 撤销的目标也列入已有 signed `revoke[]`，不给新 HPKE grant；
  不新增 wire kind；
- UI 默认按钮写「移除设备（不轮换密钥）」；确认框提示旧数据/旧根不可追回、管理材料不被撤销、
  最新恢复码须保留；「轮换密钥」是独立动作，不自动串联。

## 实施切片

1. **切片 1（本 ADR 后）**：relay 端点 + 守卫/幂等/并发 + `sync::device_management` helper +
   `auth_rev` 缓存失效 + relay/sync/e2e 测试；可独立验收，不等设备列表 UI。
2. **切片 2**：Phase 2 设备列表与警告、独立轮换入口；完成后 Windows 真机验收再正式提交。

回退：禁用新入口即可，**不把 revoked 恢复为 active**；误删走管理/恢复码以新设备身份重入。

## 必须验收的测试

1. 真 HTTP 两设备：A 撤销 B，B 旧设备和管理 token 全部立即 REVOKED；A/manager 可重认证，
   B 新 challenge 和 push/pull 被拒；
2. `/control` 保留 B 历史证书但排除 active；在 `control_rev` 相同、`auth_rev` 改变且有缓存时，
   成员集合也更新；后续 grant plan 不含 B；
3. 响应丢失后重认证重放、重复请求、跨 target id 冲突、控制链后来前进后的成功回放；
   `auth_rev` 仅首次真实变化推进；
4. 唯一 ACTIVE、pending、initializing、rotating、缺目标、helper self/无管理 key、
   两台并发互删；断言失败无副作用；
5. 错管理签名即使命中已成功 id 也拒绝；target/path 与外层镜像篡改、过期 CAS 均拒绝；
6. 最新恢复码可独立管理认证，并以**新设备身份**恢复入网；保留 signed-revoke/status-flip 回归，
   并明确验证 standalone status 翻转没有密码学保障。
