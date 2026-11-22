# ManagementCapV2 wire 对照（ADR 0015 S4a1）

> 状态：S4a1 wire + **S4b1 relay 控制链/鉴权** + **S4b2 设备管理（kind 4/5/16）** +
> **S4c 轮换/恢复** + **S4d1a 客户端控制链回放/cap 选择/realm3 鉴权** +
> **S4d1b `ApprovalEnvelopeV1`/typed Operation 构造/双签状态机/提交对账**已实现
> （`crates/sync-proto/src/management_cap_v2.rs`、`crates/relay/src/control_v2/*`、
> `crates/relay/src/authz.rs`、`crates/sync/src/management_v2.rs`、
> `crates/sync/src/approval_v1.rs`）。**未启用真实 vault**；App UI（S4d1c）与收件箱（S4d2）
> 未做；v1 wire 未改。
> 本文件是 wire 对照表，S4b1/S4b2/S4c 取值见 §2.10–2.13 与 §6，客户端验证清单见 §8，
> 审批包布局与 base64 前缀见 §9。
> 依据：[`docs/adr/0015-management-cap-v2.md`](../adr/0015-management-cap-v2.md)。wire 有歧义时
> 以 ADR 文字为准，本文只记录实现取值；歧义见 §6。

## 1. 范围与哈希注入

`sync-proto` 是纯类型 crate（ADR 0003：无 IO/crypto/SQLite），因此：

- 编解码、长度/枚举/顺序校验在本 crate；
- `SHA-256` 由调用方注入 `Sha256Fn = fn(&[u8]) -> [u8;32]`（sync/relay 传 `sha2` 的闭包，
  测试传独立实现）；
- **不验签、不查时钟、不读数据库**。`signed_at/not_after` 的实时偏差与 300s 窗口、cap 是否
  当前有效、门限参与者是否 ACTIVE，都属于 relay/客户端验证。

## 2. 固定对象字段表

所有对象都是整数键 map，键从 0 连续编号；`u` 为 `0..=i64::MAX`；ID=16B，pk/hash=32B，sig=64B。
解码严格要求字段数精确、键升序、无重复/未知键；编码按升序输出（canonical）。

### 2.1 `ManagementCapV2`（键 0..12，13 字段）

| key | 字段 | wire | 校验 |
|---|---|---|---|
| 0 | `version` | uint | 必须 2 |
| 1 | `instance_id` | 16B | 非全零 |
| 2 | `vault_id` | 16B | 非全零 |
| 3 | `cap_id` | 16B | 非全零 |
| 4 | `subject_device_id` | 16B | 非全零 |
| 5 | `subject_ed_pk` | 32B | 非全零 |
| 6 | `scopes` | uint(u32) | 非零且仅 `1/2/4/8/16/32` |
| 7 | `not_before` | uint | ≤ i64::MAX |
| 8 | `expires_at` | uint | > not_before，≤ i64::MAX |
| 9 | `issued_control_rev` | uint | >0，≤ i64::MAX |
| 10 | `issued_authz_rev` | uint | >0，≤ i64::MAX（仅历史证据） |
| 11 | `issuer_authority_rev` | uint | >0，≤ i64::MAX |
| 12 | `delegable` | bool | **必须 false** |

### 2.2 `OperationV2`（键 0..14，15 字段）

| key | 字段 | wire | 校验 |
|---|---|---|---|
| 0 | `version` | uint | 必须 2 |
| 1 | `instance_id` | 16B | 非全零 |
| 2 | `vault_id` | 16B | 非全零 |
| 3 | `kind` | uint | `ControlKindV2`：0..14 与 16，15/≥17 拒绝 |
| 4 | `request_id` | 16B | 非全零；dedup 键 `(vault_id,request_id)` |
| 5 | `expected_control_rev` | uint | ≤ i64::MAX（CAS 前置；0 允许） |
| 6 | `prev_control_hash` | 32B | **允许全零**（链首 genesis） |
| 7 | `expected_authz_rev` | uint | ≤ i64::MAX（0 允许，Upgrade 前 legacy） |
| 8 | `authority_rev` | uint | ≤ i64::MAX（Upgrade 记录的取值见 §6） |
| 9 | `signed_at` | uint | ≤ i64::MAX |
| 10 | `not_after` | uint | ≥ signed_at，≤ i64::MAX |
| 11 | method | uint | **固定 2 (POST)**；解码为其他值即拒绝 |
| 12 | `canonical_path` | text | 必须逐字节等于 `/v2/vaults/{lowerhex32}/control/submit` |
| 13 | `payload` | bytes | 非空、canonical CDE（kind 专属 schema 在 S4b） |
| 14 | `signers` | array | 1..=16，按 `(role,device,cap,pk)` **严格升序**去重 |

`ControlKindV2`：0 UpgradePolicy、1 PolicyReplace、2 CapGrant、3 CapRevoke、4 DeviceApprove、
5 DeviceRevoke、6 RotationBegin、7 RotationAbort、8 RotationRestart、9 RotationAddRevokes、
10 EpochActivate、11 InviteCreate、12 InviteCancel、13 RecoveryReset、14 RecoveryKeyChange、
16 DeviceRightsChange。**15 未分配，必须拒绝**（未知 kind fail-stop）。

`canonical_path` 额外拒绝：非 ASCII、空白/控制字符、`?`、`#`、`%`、`\\`、空段、`.`/`..` 段。
唯一路径长度固定 58B（`/v2/vaults/` 11 + 32 hex + `/control/submit` 15）。

### 2.3 `PolicyV2`（键 0..4，5 字段）

| key | 字段 | wire | 校验 |
|---|---|---|---|
| 0 | `version` | uint | 必须 2 |
| 1 | `highrisk_threshold` | uint(u32) | **固定 2** |
| 2 | `min_quorum` | uint(u32) | **固定 2** |
| 3 | `max_cap_ttl` | uint | ≤ 2_592_000（30 天） |
| 4 | `max_invite_ttl` | uint | ≤ 86_400（1 天） |

### 2.4 `SignerV2`（键 0..3，4 字段）

| key | 字段 | wire | 语义 |
|---|---|---|---|
| 0 | `role` | uint | 0 设备 / 1 旧 creator 升级 / 2 离线恢复 / 3 目标 PoP；≥4 拒绝 |
| 1 | `device_id` | 0 或 16B | 设备/PoP 必须 16B；根签名必须 0B |
| 2 | `cap_id` | 0 或 16B | 只有 role 0 可带；其余必须 0B |
| 3 | `pk` | 32B | 非全零 |

### 2.5 `SignedControlV2`（键 0..1，2 字段）

| key | 字段 | wire | 校验 |
|---|---|---|---|
| 0 | operation | bytes | 必须能按 `OperationV2` canonical 解码 |
| 1 | signatures | array | 每项恰好 64B；数量必须等于 `operation.signers.len()`（1..=16） |

### 2.6 `DeviceCertV2`（键 0..8，9 字段）

| key | 字段 | wire | 校验 |
|---|---|---|---|
| 0 | `version` | uint | 必须 2 |
| 1 | `instance_id` | 16B | 非全零 |
| 2 | `vault_id` | 16B | 非全零 |
| 3 | `device_id` | 16B | 非全零 |
| 4 | `sign_pk` | 32B | 非全零 |
| 5 | `hpke_pk` | 32B | 非全零 |
| 6 | `serial` | uint | >0，≤ i64::MAX |
| 7 | `data_rights` | uint(u32) | 仅 READ=1/WRITE=2，非零；不得含管理位 |
| 8 | `issued_authz_rev` | uint | >0，≤ i64::MAX |

### 2.7 `GrantAadV2`（键 0..8，9 字段）

| key | 字段 | wire | 校验 |
|---|---|---|---|
| 0 | `version` | uint | 必须 2 |
| 1 | `instance_id` | 16B | 非全零 |
| 2 | `vault_id` | 16B | 非全零 |
| 3 | `epoch` | uint | >0，≤ i64::MAX |
| 4 | `recipient_D` | 16B | 非全零 |
| 5 | `recipient_hpke_pk` | 32B | 非全零 |
| 6 | `authority_rev` | uint | >0，≤ i64::MAX |
| 7 | `grant_id` | 16B | 非全零 |
| 8 | `issued_authz_rev` | uint | >0，≤ i64::MAX |

### 2.8 grant 包 `GrantPackageV2`（键 0..2，3 字段）

| key | 字段 | wire | 校验 |
|---|---|---|---|
| 0 | `aad` | bytes | 必须能按 `GrantAadV2` canonical 解码，重编码逐字节相同 |
| 1 | `enc` | 32B | 非全零 |
| 2 | `ciphertext` | bytes | 非空（完整 HPKE ciphertext） |

HPKE info 改为 `MN/v2/epoch-grant\0`；明文仅 epoch root。

### 2.9 `ActivationV2`（键 0..13，14 字段）

| key | 字段 | wire | 校验 |
|---|---|---|---|
| 0 | `version` | uint | 必须 2 |
| 1 | `rotation_id` | 16B | 非全零 |
| 2 | `from_epoch` | uint | >0，≤ i64::MAX |
| 3 | `to_epoch` | uint | > from_epoch，≤ i64::MAX |
| 4 | `freeze_S` | uint | ≤ i64::MAX（空库首激活可为 0） |
| 5 | `primary_CID` | 16B | 非全零 |
| 6 | `primary_commit_hash` | 32B | 非全零 |
| 7 | `fallback_CID` | 16B | 非全零 |
| 8 | `fallback_commit_hash` | 32B | 非全零 |
| 9 | `slot_set_hash` | 32B | 非全零 |
| 10 | `member_cert_hash` | 32B | 非全零 |
| 11 | `grants_hash` | 32B | 非全零 |
| 12 | `old_asset_hash` | 32B | 非全零 |
| 13 | `new_asset_hash` | 32B | 非全零 |

context（instance/vault/request/prev hash）由外层 `OperationV2` 绑定，`ActivationV2` 不再重复。

### 2.10 S4b1 kind 专属 payload

| 对象 | 键 | 字段 |
|---|---|---|
| `UpgradePolicyPayloadV2` | 0..4 | `old_manager_rev`（u，>0）、`P`（bytes=CDE(PolicyV2)）、`fresh_recovery_pk`（32B）、`bootstrap_caps`（array(bytes=CDE(ManagementCapV2))）、`ACTIVE_manifest`（array(bytes=CDE(DeviceCertV2)，device_id 严格升序） |
| `CapGrantPayloadV2` | 0..0 | `cap`（bytes=CDE(ManagementCapV2 canonical)） |
| `CapRevokePayloadV2` | 0..0 | `cap_id`（16B） |
| `InviteCreatePayloadV2` | 0..2 | `invite_id`（16B）、`token_hash`（32B）、`expires_at`（u，>0） |

payload 是 `OperationV2.payload` 的完整 canonical CDE；解码严格（字段数精确、键升序、无未知键）。

### 2.11 `ChallengeV2`（键 0..12，13 字段）

| key | 字段 | wire | 校验 |
|---|---|---|---|
| 0 | `version` | uint | 必须 2 |
| 1 | `instance_id` | 16B | 非全零 |
| 2 | `origin_hash` | 32B | 非全零；relay 取值 `SHA256(instance_id \|\| vault_id)` |
| 3 | `vault_id` | 16B | 非全零 |
| 4 | `realm` | uint | 1 设备 / 3 管理设备 / 4 恢复；realm 2 不是 V2 取值 |
| 5 | `subject` | 16B | 非全零 |
| 6 | `challenge_id` | 16B | 非全零 |
| 7 | `nonce` | 32B | 非全零 |
| 8 | `expires_at` | uint | >0，≤ i64::MAX |
| 9 | `auth_rev` | uint | >0 |
| 10 | `authz_rev` | uint | ≤ i64::MAX |
| 11 | `cap_id` | 0/16B | realm 3 必填；realm 1/4 必须为空 |
| 12 | `authority_rev` | uint | ≤ i64::MAX |

客户端对**整份 `CDE(ChallengeV2)` 字节**签 `MN/v2/auth\0 || challenge`；`/authz/tokens`
请求 `{0: challenge bytes, 1: sig64}`。challenge 行还投影签发瞬间的
`authz_version=2/authz_rev/authority_rev/cap_id`，兑现时逐字段比对。

### 2.12 `SetDigestV2`（集合摘要）

`set_digest_v2(entries, sha256)`：输入 `(id16, canonical_bytes)` 列表；按 `id` 字节升序**去重**
（同 id 不同字节 = 集合分叉，拒绝；相同重复项折叠）；预映像
`CDE(array(bytes))`——**每个元素是 byte string**，内容是该对象的完整 canonical CDE bytes
（id 不进预映像）；最终 `SHA256`。空集合合法（`0x80` 的 SHA-256）。

### 2.13 S4b2 kind 专属 payload（kind 4/16/5）

| 对象 | 键 | 字段 |
|---|---|---|
| `DeviceCredentialPayloadV2`（kind 4 `DeviceApprove` 与 kind 16 `DeviceRightsChange` **共用**） | 0..1 | `cert`（bytes=CDE(DeviceCertV2)）、`grants`（array(bytes=CDE(GrantPackageV2))） |
| `DeviceRevokePayloadV2`（kind 5） | 0..0 | `sorted_device_ids`（array(bytes16)，**严格升序去重**、非空、≤32） |

`DeviceApprovePayloadV2` / `DeviceRightsChangePayloadV2` 是同一类型的别名（wire 只有一种编码）。
proto 侧校验：证书/包各自合法、`grants` 非空且 ≤32、`grant_id` 两两不同、`epoch` 两两不同；
证书与包之间的语义（recipient/hpke/epoch/authority/issued rev/当前 epoch）由 relay 按 DB 校验：

- relay 只接受**当前活动 epoch** 的一份 grant；多 epoch 包属于轮换/激活（S4c）。
- `grant_id` 在 `device_grants` 全表内不复用（复用 → 409 `ID_CONFLICT`）。
- kind 4：PENDING 目标证书必须绑定兑换时提交的 `sign_pk`/`hpke_pk`、`serial=1`、
  `issued_authz_rev`=提交后 authz rev；首次准入需要 role3 目标 PoP（`MN/v2/pop\0||h||pk32`）；
  已 ACTIVE 目标只接受与库内**逐字节相同**的证书+grant（同一次批准的重放）。
- kind 16：目标 ACTIVE、两把公钥不变、`serial=max+1`、权限真的变化且必须含 READ、
  可写设备 quorum 不得从 ≥2 降到 <2。
- 落库：`device_credentials`（`approval_control_rev`=本次提交，`certificate_signature=NULL`）、
  `device_grants`（`signed_header`=grant AAD 字节，`signature=NULL`）。

### 2.14 S4c kind 专属 payload（kind 6/7/8/9/10/12/13/14）

| 对象 | 键 | 字段 |
|---|---|---|
| `RotationBeginPayloadV2`（kind 6） | 0..3 | `rotation_id`(16)、`from_epoch`、`to_epoch`、`revoke_ids`（array(bytes16)，严格升序去重、可空、≤32） |
| `RotationAbortPayloadV2`（kind 7） | 0..0 | `rotation_id`(16) |
| `RotationRestartPayloadV2`（kind 8） | 0..2 | `rotation_id`(16)、`new_to_epoch`、`extra_ids`（array(bytes16)，严格升序去重、可空、≤32） |
| `RotationAddRevokesPayloadV2`（kind 9） | 0..1 | `rotation_id`(16)、`device_ids`（array(bytes16)，严格升序去重、**非空**、≤32） |
| `EpochActivatePayloadV2`（kind 10） | 0..0 | `activation`（bytes=CDE(ActivationV2)） |
| `InviteCancelPayloadV2`（kind 12） | 0..0 | `invite_id`(16) |
| `RecoveryResetPayloadV2`（kind 13） | 0..4 | `policy`（bytes=CDE(PolicyV2)）、`fresh_recovery_pk`(32)、`bootstrap_caps`（array(bytes=CDE(ManagementCapV2))，非空）、`device_adds`（array(bytes=CDE(DeviceCredentialPayloadV2))）、`revoke_ids`（array(bytes16)，严格升序去重、可空） |
| `RecoveryKeyChangePayloadV2`（kind 14） | 0..1 | `fresh_recovery_pk`(32)、`bootstrap_caps`（array(bytes=CDE(ManagementCapV2))，非空） |

字段数常量：`ROTATION_BEGIN_PAYLOAD_V2_FIELD_COUNT=4`、`ROTATION_ABORT_PAYLOAD_V2_FIELD_COUNT=1`、
`ROTATION_RESTART_PAYLOAD_V2_FIELD_COUNT=3`、`ROTATION_ADD_REVOKES_PAYLOAD_V2_FIELD_COUNT=2`、
`EPOCH_ACTIVATE_PAYLOAD_V2_FIELD_COUNT=1`、`INVITE_CANCEL_PAYLOAD_V2_FIELD_COUNT=1`、
`RECOVERY_RESET_PAYLOAD_V2_FIELD_COUNT=5`、`RECOVERY_KEY_CHANGE_PAYLOAD_V2_FIELD_COUNT=2`。

proto 侧只做**结构**校验（对象各自合法、`rotation_id`/`fresh_recovery_pk` 非全零、
`from_epoch > 0`、`to_epoch > from_epoch`、`new_to_epoch > 0`、设备 ID 数组严格升序去重、
caps 非空且 `cap_id` 两两不同、`device_adds` 的 `device_id` 两两不同且不与 `revoke_ids`
相交）；语义（epoch 是否已分配、cap 的 `issued_*`/`issuer_authority_rev`、grant 的
recipient/epoch/authority、门限/角色/scope）由 relay 按 DB 事实校验：

- kind 6/8/9 含撤销时**每一位**签名者还必须具备 `REVOKE`（不能靠两签拼权限）。
- 候选 epoch 必须 `> active_epoch` 且**从未出现在 `epochs` 表**（aborted/retired 也算
  已分配）；`rotation_id` 同样不复用。
- kind 10 的五个集合摘要全部按 DB 事实重算后逐字段比对（`SetDigestV2` 的
  `member_cert_hash`/`grants_hash`、`slot_set_digest`、两个资产摘要），任一不符 → 409。
- kind 13 的 `bootstrap_caps` 必须绑定**提交后**的 `control_rev`/`authz_rev`/
  `authority_rev`（`authority_rev+1` 让旧代 cap 全部失效，因此必须原子装新代 cap）。
- kind 13/14 的签名：`role2`（`recover` 域）分别对应旧恢复根与新钥 PoP；设备 adds 每位
  还须 `role3`（`pop` 域）。kind 13 不接受 `role0`。

### 2.15 恢复 realm 4（`/authz/*` + 恢复证据）

- challenge 请求 `{0: realm=4, 1: subject16}`（`cap_id` 必须省略）：`subject` 必须等于
  `SHA256(recovery_pub)[..16]`；challenge 60s 单用；响应仍是 `{0: bytes(ChallengeV2)}`
  （`realm=4`、`cap_id` 省略）。
- token 请求 `{0: challenge bytes, 1: sig64}`，签 `MN/v2/auth\0 || challenge`；TTL **60s**；
  响应 `{0 token,1 expires_at,2 server_time,3 realm=4,4 auth_rev,5 authz_rev,6 authority_rev}`
  （**无** `cap_id`）。
- 恢复 bearer **只**能读 `GET /v2/vaults/{lowerhex32}/recovery/evidence`（`authenticate_v2`
  显式拒绝 realm 4）；kind 13/14 仍是自证控制记录，bearer 不能代替离线恢复根签名。
- 每请求 `authz_rev`/`authority_rev` 必须**相等**，并重验当前 `recovery_pub`（换钥/Reset
  后旧恢复 bearer 立即 403 `REVOKED`）。

## 3. 签名域与摘要

共同摘要 `h = SHA256(CDE(OperationV2))`（`OperationV2::digest`）。

| 用途 | 输入字节 | 实现 |
|---|---|---|
| 设备控制签 | `MN/v2/control\0 \|\| h` | `control_signing_input(&op, sha256)` |
| 旧 creator 升级签 | `MN/v2/upgrade\0 \|\| h` | `upgrade_signing_input` |
| 离线恢复根签 | `MN/v2/recover\0 \|\| h` | `recover_signing_input` |
| 目标设备 PoP | `MN/v2/pop\0 \|\| h \|\| pk32` | `pop_signing_input` |
| 链 hash | `SHA256(prev_hash \|\| CDE(SignedControlV2))` | `SignedControlV2::chain_hash` |
| V2 鉴权 | `MN/v2/auth\0 \|\| challenge` | `auth_signing_input_v2` |

域常量是**真实 0x00 结尾**的单字节串，与所有 v1 域（`MN/v1/*`、cursor、AAD）互不复用；
`domain_ends_with_nul` 与 golden 测试双重锁定。

## 4. 严格 CDE 与错误分类

复用 `crate::cde`：整数最短形式、map 键严格升序、字段数精确。以下输入在**解码前**即被拒绝
（`ProtoError::NonCanonical`）：乱序键、重复键、非最短整数、null/undefined、tag、
float/simple、不定长 map/array、尾随字节、截断、超深嵌套。字段级拒绝：

- 未知键 → `UnknownField`；缺键 → `MissingField`；字段数不符 → `Invalid`；
- 版本不符 → `UnsupportedVersion(version)`；
- 长度/类型/枚举/全零 ID/u 溢出 → `Invalid`；
- 容器超限（signer >16）→ `TooLarge`。

## 5. Golden 向量（`tests/golden_management_cap_v2.hex`）

确定性输入（与测试 `sample_*()` 一一对应）：id=`[n;16]`、hash/pk=`[n;32]`、sig=`[n;64]`；
cap scopes=11（INVITE|APPROVE|CAP_ADMIN）、not_before=1_700_000_000、
expires=1_700_086_400、issued rev=7/8/1、delegable=false；Operation 为
kind=3 CapRevoke、payload=`CDE({0: bytes([0x13;16])})`、signers=两台设备
（0x21/无 cap/0x31、0x22/cap 0x13/0x32），signed_at=1_700_000_000、not_after=+300s。

```text
cap ad00020150121212121212121212121212121212120250111111111111111111111111111111110350131313131313131313131313131313130450212121212121212121212121212121210558203131313131313131313131313131313131313131313131313131313131313131060b071a6553f100081a6555428009070a080b010cf4
policy a5000201020202031a00278d00041a00015180
signer_device a400000150212121212121212121212121212121210250131313131313131313131313131313130358203131313131313131313131313131313131313131313131313131313131313131
signer_creator a40001014002400358203232323232323232323232323232323232323232323232323232323232323232
signer_pop a4000301502222222222222222222222222222222202400358203333333333333333333333333333333333333333333333333333333333333333
operation af000201501212121212121212121212121212121202501111111111111111111111111111111103030450414141414141414141414141414141410507065820515151515151515151515151515151515151515151515151515151515151515107080801091a6553f1000a1a6553f22c0b020c783a2f76322f7661756c74732f31313131313131313131313131313131313131313131313131313131313131312f636f6e74726f6c2f7375626d69740d53a10050131313131313131313131313131313130e82a4000001502121212121212121212121212121212102400358203131313131313131313131313131313131313131313131313131313131313131a400000150222222222222222222222222222222220250131313131313131313131313131313130358203232323232323232323232323232323232323232323232323232323232323232
signed_control a20059014aaf000201501212121212121212121212121212121202501111111111111111111111111111111103030450414141414141414141414141414141410507065820515151515151515151515151515151515151515151515151515151515151515107080801091a6553f1000a1a6553f22c0b020c783a2f76322f7661756c74732f31313131313131313131313131313131313131313131313131313131313131312f636f6e74726f6c2f7375626d69740d53a10050131313131313131313131313131313130e82a4000001502121212121212121212121212121212102400358203131313131313131313131313131313131313131313131313131313131313131a40000015022222222222222222222222222222222025013131313131313131313131313131313035820323232323232323232323232323232323232323232323232323232323232323201825840a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a15840a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2
device_cert a9000201501212121212121212121212121212121202501111111111111111111111111111111103502222222222222222222222222222222204582034343434343434343434343434343434343434343434343434343434343434340558206262626262626262626262626262626262626262626262626262626262626262060307030808
grant_aad a900020150121212121212121212121212121212120250111111111111111111111111111111110302045022222222222222222222222222222222055820616161616161616161616161616161616161616161616161616161616161616106010750717171717171717171717171717171710808
grant_package a3005874a9000201501212121212121212121212121212121202501111111111111111111111111111111103020450222222222222222222222222222222220558206161616161616161616161616161616161616161616161616161616161616161060107507171717171717171717171717171717108080158206363636363636363636363636363636363636363636363636363636363636363025830c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3
activation ae00020150818181818181818181818181818181810202030304182a0550828282828282828282828282828282820658209191919191919191919191919191919191919191919191919191919191919191075083838383838383838383838383838383085820929292929292929292929292929292929292929292929292929292929292929209582093939393939393939393939393939393939393939393939393939393939393930a582094949494949494949494949494949494949494949494949494949494949494940b582095959595959595959595959595959595959595959595959595959595959595950c582096969696969696969696969696969696969696969696969696969696969696960d58209797979797979797979797979797979797979797979797979797979797979797
operation_digest 0aad31d5537b840c1fb7873ee27635d732ef35da5a8fb2fae2138437cefc7c05
control_signing_input 4d4e2f76322f636f6e74726f6c000aad31d5537b840c1fb7873ee27635d732ef35da5a8fb2fae2138437cefc7c05
upgrade_signing_input 4d4e2f76322f75706772616465000aad31d5537b840c1fb7873ee27635d732ef35da5a8fb2fae2138437cefc7c05
recover_signing_input 4d4e2f76322f7265636f766572000aad31d5537b840c1fb7873ee27635d732ef35da5a8fb2fae2138437cefc7c05
pop_signing_input 4d4e2f76322f706f70000aad31d5537b840c1fb7873ee27635d732ef35da5a8fb2fae2138437cefc7c053131313131313131313131313131313131313131313131313131313131313131
auth_signing_input 4d4e2f76322f6175746800cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc
chain_hash a766ddfa1fe7f248a6c18ce759d81e810a7293f3ec9a684c9012b760b2eda406
```

S4b1 追加（`policy_s4b1/cap_a_s4b1/cap_b_s4b1/cert_a_s4b1/cert_b_s4b1/upgrade_payload/
cap_grant_payload/cap_revoke_payload/invite_create_payload/challenge_v2/set_digest_v2/
set_digest_v2_input`）：kind payload、`ChallengeV2` 与 `SetDigestV2` 的确定性 golden；
同样的独立 Python 脚本生成，任何字段/顺序/预映像变更都会使其变化。

S4b2 追加（`device_approve_payload` / `device_revoke_payload`）：kind 4/16 共用形状与 kind 5
排序数组的确定性 golden（同一独立脚本口径；样例证书 `device_id=[0x24;16]`、`serial=1`、
grant `epoch=2`/`grant_id=[0x72;16]`、ciphertext 40×`0xC4`；撤销数组 `[0x24;16],[0x25;16]`）。

S4c 追加（kind 6–10/12/13/14）：`rotation_begin_payload` / `rotation_abort_payload` /
`rotation_restart_payload` / `rotation_add_revokes_payload` / `epoch_activate_payload` /
`invite_cancel_payload` / `recovery_reset_payload` / `recovery_key_change_payload`。
样例：`rotation_id=[0x81;16]`、from=2/to=3（restart new_to=4）、撤销数组
`[0x24;16],[0x25;16]`、`invite_id=[0x61;16]`、新恢复公钥 `[0x33;32]`；`epoch_activate_payload`
的键 0 是**整份**既有 `activation` golden 的 bytes；`recovery_reset_payload` 复用既有
`policy_s4b1`/`cap_a_s4b1`/`cap_b_s4b1`/`device_approve_payload` 字节（revoke_ids 取
`[0x25;16],[0x26;16]` 以避免与 add 的 `[0x24;16]` 相交）。

`cap/policy/signer_*/operation/signed_control/device_cert/grant_aad/grant_package/activation`
是完整对象字节；`operation_digest` 是 `h`；四个 `*_signing_input` 是域分隔后的最终待签字节；
`auth_signing_input` 用 32B `[0xCC;32]` challenge；`chain_hash` 用 prev=`[0x51;32]`。
任何字段/域/顺序改动都会使这些向量变化——**wire 变更必须显式升版本**。

## 6. 实现取值与 ADR 歧义（S4a2/S4b 需确认）

1. **signer 的 0/16 必填性**：ADR 只写 `device_id(0/16)`、`cap_id(0/16)`。实现按角色收紧：
   role 0/3 必须有 16B `device_id`；role 1/2 必须为空；只有 role 0 可带 `cap_id`。
   **S4b1 取值**：Upgrade 的 role0 带 device_id、cap_id 必须为空（升级前没有 cap）；
   CapGrant/CapRevoke/InviteCreate 的 role0 必须带 device_id + cap_id。
2. ~~Upgrade 记录的 `authority_rev`/`expected_authz_rev`~~ **S4b1 已冻结并强制**：
   Upgrade 提交前 `expected_authz_rev=0`、`authority_rev=1`；提交后 vault 为 `authz_rev=1/
   authority_rev=1`。其余 kind 的这两个值必须等于提交前 vault 当前值（不相等 → 409）。
3. ~~kind 专属 payload 未 typed~~ **S4b1 已 typed**：见 §2.10。CapGrant 的 cap 是
   `{0: bytes(CDE(ManagementCapV2))}`（不是嵌套 map）。
4. ~~集合摘要未固定编码~~ **S4b1 已固定**：见 §2.12；golden 为 `set_digest_v2` /
   `set_digest_v2_input`。activation 的三个集合摘要（S4c）复用同一 helper。
5. **唯一路径**：实现按 ADR 强制 `canonical_path` 逐字节等于 `/v2/vaults/{lowerhex32}/control/submit`。
   若 relay 实际挂载前缀不同（例如另有 `/v2` 之外的前缀），必须先改 ADR。
6. **signer 上限 16**：ADR 只固定“高危两签”，未给数组上限；`MAX_OPERATION_SIGNERS_V2=16`
   是 S4a1 的实现边界（`TooLarge`），可按 S4b 需要调整。
7. **`prev_control_hash` 全零**：wire 保留为链首合法值；**S4b1 relay 要求 Upgrade 必须逐字节
   等于既有 v1/v2 链 head hash**（不要求非零——空链 genesis 例外，但当前 vault 初始化总会写
   首条 v1 控制记录）。
8. **`method` 字段**：wire 上固定 2(POST)，结构体不暴露该字段；解码遇非 2 直接拒绝。
9. **kind 4/16 payload 的元素编码**：ADR 表只写 `{0:cert,1:grants[]}`；实现取
   `bytes(CDE(对象))`（与 Upgrade manifest/CapGrant/`SetDigestV2` 成员同口径），并让两种 kind
   共用同一类型（wire 只有一种编码）。若客户端要求嵌套 map，必须先改 ADR 并换 golden。
10. **kind 4/16 的 grant 数量**：S4b2 只接受**当前活动 epoch** 的一份包（`device_grants` 的 PK
    是 `(vault, epoch, device)`）；多 epoch/激活期包属于 S4c 的轮换/激活记录。
11. **V2 grant 的 `device_grants.signature`**：V2 授权是两签控制记录本身，因此该列写 NULL
    （v1 的"管理钥单独签名"在 V2 不存在）；`signed_header` 存 grant AAD 字节，客户端可用
    `aad + enc + ciphertext` 还原完整 `GrantPackageV2`。
12. **轮换 epoch 由客户端指定（S4c）**：kind 6/8 的 `to_epoch`/`new_to_epoch` 由**签名
    payload** 给出（v1 是 relay 按 `next_epoch` 分配）。relay 只校验「更高且从未分配」
    （`epochs` 表无该行），并让 `next_epoch` **跳号前进**；这样"候选不复用"不依赖
    客户端和 relay 对 `next_epoch` 的同一视图。
13. **恢复记录都带 `bootstrap_caps`（S4c）**：ADR「换恢复钥/Reset 时 `authority_rev+1`，
    旧代全部 cap 失效并原子配置新 cap」。`effective_cap` 要求 `issuer_authority_rev ==
    vault.authority_rev`，代一换旧 cap 自动失效；同一提交不装新 cap 会让管理面死锁。
    「KeyChange 仅换恢复钥」指不改 policy/成员集合，不指不重签 cap。
14. **`member_cert_hash` 取"当前证书字节"（S4c）**：ADR 写「成员含完整 DeviceCertV2」，
    实现取 `devices.certificate` 的**完整字节**（V2 证书；升级前未换证的是 canonical
    `DeviceCertV1` 字节，同样是 canonical CDE）。客户端可用 `/control` 的 members 字段
    逐字节复算；relay 不做"V1 证书合成为 V2"的隐式转换（那会造出两边不一致的摘要）。
15. **`slot_set_hash` 不用 `SetDigestV2`（S4c）**：`slot_id` 是 32B，`SetDigestV2` 的 id
    固定 16B；ADR 明示「`slot_set_hash`/assets 按各自对象既有 canonical 编码」，
    因此复用 v1 的 `slot_set_digest`（`SHA256(concat(slot_id || SHA256(descriptor)))`）
    与 `asset_set_digest`（`CDE(sorted ObjectRefV1[ASSET])`）。
16. **`rotations.member_set_hash` 在 V2 的语义（S4c）**：begin 时写入**冻结时**的成员
    摘要，激活时与当前值比较以决定 `authz_rev` 是否 +1（ADR：成员变更才 +1）；v1 行仍是
    "激活后的值"，语义不混用。
17. **InviteCancel 的投影（S4c）**：已兑换是永久事实（`used_at`+`pending_device` 都在）→
    409；已取消（`used_at` 在、`pending_device` 空）→ 幂等。取消写
    `used_at = 取消提交时刻`、`pending_device` 保持 NULL、`expires_at` 保留原值。

## 7. 验证

```bash
export CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0
cargo fmt --all
cargo clippy -p menghunote-sync-proto --all-targets -- -D warnings
cargo clippy -p menghunote-relay --all-targets -- -D warnings
cargo test -p menghunote-sync-proto              # 61 passed（含 S4b1/S4b2/S4c golden/负例）
cargo test -p menghunote-relay --all-targets     # 277 passed（268 基线 + 9 新）
cargo test -p menghunote-sync --all-targets      # 579 passed + 1 ignored
cargo test -p menghunote-app --features dev-sync # 365 passed
```

S4b1 新增 relay 集成用例（真实 HTTP + 真实 Ed25519、无 sleep）：
`tests/management_cap_v2_upgrade.rs`（5）、`tests/management_cap_v2_caps.rs`（3）。
S4b2 新增 `tests/management_cap_v2_devices.rs`（3）：批准（含 PoP/幂等/换包/错 key 负例）、
撤销（Bearer/WS/cap 级联、重复幂等、最后 quorum、邀请失效、跨 vault）、改权
（serial/历史/quorum），并逐条复算控制链 `record_hash`。
S4c 新增 `tests/management_cap_v2_rotation.rs`（4）与 `tests/management_cap_v2_recovery.rs`（5）：
两签/单签/同设备/错 from-to/候选复用/abort 不复活/restart/add-revokes/集合替换九连负例、
激活后 bearer+WS 失效与新 grant 可用；Reset/KeyChange 正负例、`recovery_only`、realm 4
（subject 绑定/60s/只读/rev 相等），同样逐条复算控制链 `record_hash`。

## 8. 客户端验证清单（ADR 0016 S4d1a，2026-11-14）

客户端实现：`crates/sync/src/management_v2.rs`（`V2ControlReplay` / `ControlStateV2` /
`V2ManagementClient`）。它从**已验证控制链**推导授权事实，不信任 relay 的 member/status/
`/state` 摘要。逐条验证如下（全部 fail-stop，错误不降级成"没有"）：

### 8.1 控制链与记录

1. 分页 `GET {v2}/vaults/{b64(V)}/control?after_rev=N`，每页 ≤256；`control_rev` 跨页必须
   一致、`rev` 连续、最后一页链尾必须等于 head（缺页/重排/截断拒绝）。
2. 每条提交复算 `record_hash = SHA256(prev_hash || signed_record)` 并与 relay 提供的
   `record_hash`/`prev_hash` 逐字节比较（`prev_hash` 链首为 32×0x00）。
3. 记录分类：只有 `{0:bytes(operation),1:[sig64…]}` 形状才是 V2 记录；`bytes` 元素还必须是
   严格 canonical 的 `SignedControlV2`（重新编码逐字节相同）。rev1 设备证书、
   `SignedControlRecord` 等 v1 历史只参与链 hash，不产生 V2 授权事实。
4. `OperationV2`：instance/vault 必须等于本地 pin；`canonical_path` 必须等于
   `canonical_submit_path(V)`（lowerhex32）；`expected_control_rev+1 == rev` 且
   `prev_control_hash` 必须等于前链尾；`signed_at ≤ now+30`、`not_after ≤ signed_at+300`、
   V2 链签名时间不得倒退（历史记录按**签名时刻**验 cap/时间窗）。
5. signers 由 proto 强制按 `(role,device,cap,pk)` 严格升序去重；签名域按角色：
   device=`MN/v2/control\0||h`、旧 creator=`MN/v2/upgrade\0||h`、恢复根=
   `MN/v2/recover\0||h`、PoP=`MN/v2/pop\0||h||pk32`。
6. 每个 `kind` 的门限/角色集合/scope 逐条复核（见 §8.2）；未知 kind 失败；
   `PolicyReplace`(1) 本客户端 fail-stop（不支持）。

### 8.2 kind 级授权规则（客户端复算）

| kind | 客户端验证 |
|---|---|
| 0 Upgrade | 首条 V2 记录；`expected_authz_rev=0/authority=1`；旧 creator pk == 本地 pin 的管理公钥；≥2 台清单内 ACTIVE 设备确认（device/pk 不重复）；新恢复钥 PoP；manifest 的 instance/vault/`issued_authz_rev=1`；bootstrap cap 的 `issued_control_rev=rev`、authz/authority=1、主体在清单内、TTL≤policy、≥2 台持 CAP_ADMIN |
| 2 CapGrant | 两台不同 ACTIVE 设备+不同公钥，各自 cap **在签名时刻**有效且含 CAP_ADMIN；新 cap 的 scope 是**每一位**签发者 scope 的子集；`issued_control_rev=rev`、`issued_authz_rev=authz+1`、authority 当前；TTL≤policy；subject ACTIVE 且公钥匹配；cap_id 未出现过 |
| 3 CapRevoke | 同上两签；目标存在且未撤销（重复撤销拒绝） |
| 4 DeviceApprove | 两签 APPROVE；首次准入另需目标 PoP（pk=证书 sign_pk）；目标 PENDING 时 serial=1/无历史/READ/`issued_authz_rev=authz+1`；已 ACTIVE 只接受逐字节相同证书+grant（不推进 authz）；REVOKED 永不复活；grant 的 recipient/hpke/authority/issued rev/grant_id 逐项校验 |
| 5 DeviceRevoke | 两签 REVOKE；目标都在已验证成员集合；剩余 ACTIVE≥min_quorum；任一高危 scope 撤销前有两设备 quorum 时撤销后不得 <2；全已 revoked 的重复不推进 authz |
| 16 DeviceRightsChange | 两签 APPROVE；目标 ACTIVE；serial=当前+1；sign/hpke 不变；READ 必须保留且权限真的变化；数据权限 quorum（可写设备 ≥2 时不变差）；grant 同上 |
| 6–10 rotation | 两签 ROTATE（含撤销还须每位 REVOKE）；rotation_id 不复用、to_epoch 前进；撤销走与 kind 5 相同的 quorum；begin 冻结成员摘要、activate 按成员变化决定 authz+1；**不执行**客户端数据切换（playback） |
| 11/12 Invite | 单台 INVITE cap（签名时刻有效/含 scope）；TTL≤policy；invite_id/token_hash 不复用；取消对已取消幂等 |
| 13 RecoveryReset | 当前恢复根 + 新钥 PoP（role2 各一）**加**每位 add 的目标 PoP；**拒绝 role0**；adds 只接受链上未知（PENDING）设备、serial=1、READ、`issued_authz_rev=authz+1`；新代 cap 的 `issued_control_rev=rev`、authz+1、authority+1；authority+1、authz+1、原子装新 cap；不足两台进入 `recovery_only` |
| 14 RecoveryKeyChange | 两签 POLICY + 新钥 PoP；不改 policy/成员；authority+1、authz+1、旧代 cap 自动失效并在同一提交装新代 cap（否则管理面死锁） |

### 8.3 cap 选择与 bearer

1. 只选**单张** cap：本机 device/key、未撤销、`issuer_authority_rev==当前 authority`、
   `not_before≤now<expires_at`、主体 ACTIVE 且登记公钥匹配、**一张覆盖全部所需 scope**；
   不允许拼 cap/拼人（错误信息明确区分"没有 cap/已撤销/旧代/过期/scope 不覆盖"）。
2. realm 3 challenge：`{0:3,1:subject16,2:cap_id}` → 严格验证 `ChallengeV2`：
   instance/vault/realm/subject/cap_id、`origin_hash=SHA256(instance||vault)`、
   `expires_at>now`（且不超过本地允许窗口）、`authz_rev/authority_rev` **等于**已验证回放值；
   签 `MN/v2/auth\0||challenge` 后换 token；token 的 `expires_at` 不得超过绑定 cap 的
   到期时间。
3. 每次使用 bearer 前重验：两个 rev **等于**当前回放值（不是 `<`）、cap 仍有效且覆盖
   scope；`/authz` 的 `REVOKED` 触发清缓存重认证一次。
4. 管理 bearer 只"选定一个 cap"，**不代替**管理签名；提交仍由 `OperationV2` 自证（S4d1b）。

### 8.4 本地持久化

- `sync.control_pin_v2`（canonical CDE v1）：`vault_id/instance_id/rev/head_hash/authz_rev/
  authority_rev` + **缓存摘要**；只前进/同 rev 相等（CAS），损坏即 fail-stop。
- `sync.management_v2_cache`：canonical cap/来源 rev/撤销投影/设备/邀请/轮换 + 验证水位；
  只有摘要与 pin 相符且标量一致时才恢复，否则丢弃并从 rev 0 重建（缓存不是信任根）。
- 重启：pin 水位续拉 `after_rev`，首条增量记录的 `prev_hash` 必须等于 pin 的 head；
  无法证明历史前缀 → `NeedsTrust`，绝不降级成 unpinned。

## 9. `ApprovalEnvelopeV1`（ADR 0016 S4d1b 客户端审批包）

> 实现：`crates/sync/src/approval_v1.rs`。这是**客户端之间**（手工通道 A / 收件箱 B）的
> 共享不可变操作 + 签名包；relay **不**解析它（提交时仍解出 `SignedControlV2` 走 §2.2）。

### 9.1 布局（canonical CDE，整数键 0..6，字段严格齐全）

```text
ApprovalEnvelopeV1 = {
  0: version (u = 1),
  1: operation_bytes (bytes = CDE(OperationV2)),      // 完整已冻结操作
  2: h (32B = SHA256(operation_bytes)),               // 必须重算一致
  3: initiator_index (u32, < signers.len()),
  4: signatures (array of {0: signer_index (u32), 1: sig64}),
  5: caps (array of bytes = CDE(ManagementCapV2)),     // 公开 cap 证据
  6: control (map {0: control_rev, 1: head_hash(32B), 2: authz_rev, 3: authority_rev}),
}
```

严格规则（解码/重新编码逐字节相同；任一违反即拒绝）：

- `version` 只接受 1；`operation_bytes` 必须是 `OperationV2` 的唯一 canonical 编码且
  `OperationV2.encode()` 逐字节相等；`h` 必须等于 `SHA256(operation_bytes)`。
- `signatures`：按 `signer_index` **严格升序去重**，下标 `< signers.len()`，`sig64` 非全零
  （**缺签 = 没有条目**，禁止零占位）；满签 = 条目数 == signer 数。
- `caps`：按 `cap_id` 严格升序去重，**恰好等于** role0 签名槽引用的 cap 集合（
  role0 必须带 `cap_id`，其余角色不得带）；每个 cap 与操作 `instance/vault` 一致，
  导入方还要与**本地已验证控制链**的 canonical cap 逐字节一致（裸 cap 不是授权证据）。
- `control`：`control_rev == OperationV2.expected_control_rev`、
  `head_hash == OperationV2.prev_control_hash`；
  - 非 Upgrade：`authz_rev == expected_authz_rev`、`authority_rev == authority_rev`；
  - Upgrade（kind 0）：`expected_authz_rev=0/authority_rev=1`，证据 `authz_rev=authority_rev=0`
    （升级前没有 V2 授权；v1 链位置由调用方从已验证 v1 控制链给出）。
- `initiator_index` 必须是发起者的 role0 槽；**A 先签**（第一签只能是该槽）。

签名域（与 §3 同源，包内已有的签名逐槽按角色验证）：

| 槽角色 | 待签字节 |
| --- | --- |
| 0 设备 | `MN/v2/control\0 \|\| h` |
| 1 旧 creator（仅 Upgrade） | `MN/v2/upgrade\0 \|\| h` |
| 2 离线恢复根 | `MN/v2/recover\0 \|\| h` |
| 3 目标 PoP | `MN/v2/pop\0 \|\| h \|\| pk32`（pk = 该槽公钥） |

### 9.2 文件与复制（版本前缀）

- **文件**：`<name>.mn-approval`，内容就是上面的 canonical CDE 字节（无额外包装；版本由键 0
  承担）。写入 = 先写 `<path>.tmp` 再 rename；读取限长（>1 MiB 在解码前拒绝）。
- **复制文本**：`mn-approval-v1:<base64url(无 padding)>`。前缀常量
  `APPROVAL_BASE64_PREFIX_V1`；缺前缀/非法字符/超长（> 上界）拒绝。
- 解析/导入**不联网、不执行、不自动签**；导入验证需要调用方先刷新
  `V2ManagementClient`（`import_approval_file/bytes/base64` 内部会 `refresh_control`），
  随后按 §8 的同一套回放规则做**干跑**（半签包跳过验签，但已存在的签名逐槽验；满签包全验）。

### 9.3 提交与回执（复用 §2.2 的唯一路径）

- 满签后再编码成 `{0: bytes(SignedControlV2), 1: [sig64…]}` 提交（**不**提交审批包本身）；
  body 仍是 `{0: bytes(SignedControlV2)}`，路径 `/v2/vaults/{lowerhex32}/control/submit`，
  **不带 bearer**（自证）。
- 回执字段见 §2.2/S4b1（`{0:control_rev,1:head_hash,2:kind,3:request_id,4:authz_rev,
  5:authority_rev}`）；客户端必须用本地字节复算 `SHA256(prev||signed_bytes)` 并与
  `head_hash` 比对、刷新回放后在同一 rev 对账——relay 摘要不作为授权依据。
- 已在链上（`record_hash_at(expected+1)` 命中本地复算值）时**重发原字节**：relay `dedup_v2`
  返回原回执（丢响应/重复提交幂等；不会重复写链）。
