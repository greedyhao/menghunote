# ManagementCapV2 schema 12 与迁移（ADR 0015 S4a2）

> 状态：S4a2 已实现（`crates/relay/src/db/schema.rs`）。**未启用 V2**：本切片只落
> **存储形状 + 迁移 + 约束**，不实现控制链验证/cap 门限/`/authz/*`/`/control/submit`
> （S4b），不改客户端，不改 v1 wire 与行为。依据
> [`docs/adr/0015-management-cap-v2.md`](../adr/0015-management-cap-v2.md)；
> wire 对照见 [`management-cap-v2-wire.md`](management-cap-v2-wire.md)。

## 1. 范围

**做**：schema 11 → 12（`vaults` 授权列、`management_caps`、`device_credentials`、`dedup_v2`、
`challenges`/`tokens` 重建、`devices`/`device_grants`/`invites` 的证书/grant/邀请投影列）、
单事务迁移 + WAL 快照 + 中断回滚、数据库层跨租户兜底、迁移/约束测试。

**不做**：任何签名/链/门限/cap 有效性判定；不写 `management_caps`/`device_credentials`/
`dedup_v2` 的业务行（当前 binary 只建表）；不切 V2（`vaults.authz_state` 恒为 `legacy`）；
不改 `sync`/`app`；不改 v1 API/DDL 之外的任何行为。

**v1 兼容口径**：v1 起就有的 22 张租户表**逐字节保留**（11→12 只加列），只有
`challenges`/`tokens` 按既有维护窗口语义清空（与 9→10 完全一致）；旧 binary 打开 schema 12
仍被版本门禁拒绝（`relay schema is newer than this binary`），不提供自动降 schema。

## 2. schema 12 形状

### 2.1 `vaults`（重建，全局表）

| 列 | 类型/默认 | 语义 |
|---|---|---|
| `authz_version` | `INTEGER NOT NULL DEFAULT 1 CHECK IN (1,2)` | 1 = v1 legacy，2 = ManagementCapV2 |
| `authz_rev` | `INTEGER NOT NULL DEFAULT 0` | 授权状态版本（授权变化 +1；cap 自然到期不 +1） |
| `authority_rev` | `INTEGER NOT NULL DEFAULT 0` | 离线恢复根代（换恢复钥/Reset +1） |
| `recovery_pub` | `BLOB NULL CHECK(length=32)` | 离线恢复根公钥（secret 永不进 relay） |
| `policy_bytes` | `BLOB NULL CHECK(length>0)` | `CDE(PolicyV2)` 原始字节（服务端只存不重编码） |
| `upgrade_control_rev` | `INTEGER NULL` | 完成 UpgradePolicy 的控制提交 rev；**复合 FK** 到 `control_commits(vault_id, rev)` |
| `authz_state` | `TEXT NOT NULL DEFAULT 'legacy' CHECK IN ('legacy','active','recovery_only')` | 授权状态机 |

跨列 CHECK（三态**互斥且完备**，旧行天然满足）：

- `legacy`：`authz_version=1`、两 rev 为 0、`recovery_pub/policy_bytes/upgrade_control_rev` 全 NULL；
- `active`：`authz_version=2`、两 rev ≥ 1、policy/恢复钥/upgrade rev **都非空**；
- `recovery_only`：`authz_version=2`、两 rev ≥ 1、恢复钥非空（只许离线 Reset，不要求 policy）。

### 2.2 `management_caps`（新表，PK `(vault_id, cap_id)`）

`cap_id`（16B）、`canonical_bytes`（客户端签名的 `CDE(ManagementCapV2)` 原文）、
`subject_device` + `subject_pk`、`scopes`（`1..63`，即 1/2/4/8/16/32 的非空组合）、
`not_before`/`expires_at`（`expires_at > not_before`）、`issued_control_rev`/`issued_authz_rev`/
`issuer_authority_rev`、`revoked_control_rev`（NULL = 未撤销，`> issued_control_rev`）、
`delegable`（**固定 0**，ADR 不支持转授）、`created_at`。

复合 FK：`(vault_id, subject_device) → devices`、`(vault_id, issued_control_rev)` 与
`(vault_id, revoked_control_rev) → control_commits(vault_id, rev)`。历史行不 GC。

### 2.3 `device_credentials`（新表，V2 凭证历史，PK `(vault_id, device_id, serial)`）

每次改权一份完整证书（`authz_version`/`sign_pk`/`hpke_pk`/`certificate`/
`certificate_signature`/`issued_authz_rev`/`approval_control_rev`/`created_at`），
FK 连 `devices` 与批准它的控制提交；**不 GC**。v1 行不写这张表。

### 2.4 `challenges` / `tokens`（重建）

- `realm` 由 `TEXT` 改 **`INTEGER`**：1 设备 / 2 旧管理（仅 legacy）/ 3 管理设备 / 4 恢复
  （3/4 由 S4b 写；当前 binary 只写 1/2，与 wire 的 `realm_tag` 一致）；
- 新增 `authz_version/authz_rev/authority_rev/cap_id`；
- `CHECK(realm <> 3 OR cap_id IS NOT NULL)`：管理 token/challenge **必须绑定 cap**；
- `cap_id` 复合 FK 连 `management_caps(vault_id, cap_id)`：跨租户 cap 必然失败；
- 迁移**不复制任何行**（旧 challenge/token 全部失效，与 9→10 同语义）。

### 2.5 `devices` / `device_grants` / `invites`（重建）

| 表 | 新列 | 说明 |
|---|---|---|
| `devices` | `approval_control_rev`（FK → 控制提交）、`issued_authz_rev` | 证书批准的链证据（v1 行 NULL） |
| `device_grants` | 同上两列 | grant 的批准点与授权 rev |
| `invites` | `issuer_cap_id`（FK → `management_caps`） | ADR 0015 kind 11/12：只产生 PENDING，绑定签发 cap |

### 2.6 `dedup_v2`（新表，PK `(vault_id, request_id)`）

V2 控制提交的幂等/回执：`request_hash`、`control_rev`（FK → 控制提交）、`accepted_seq`、
`response`、`created_at`。**与 v1 的 `dedup`（PK `(vault_id, scope, id)`）并存**，v1 行完全不改；
同 request_id 同摘要回放、异摘要 409 的判定属 S4b。

### 2.7 索引（全部带 `vault_id` 前缀）

`devices_approval`、`challenges_cap`、`tokens_cap`、`management_caps_subject`
（`vault_id, subject_device, expires_at`）、`management_caps_revoked`、
`device_credentials_approval`、`invites_issuer_cap`、`device_grants_approval`；
`vaults_legacy_default` 在 `vaults` 重建后幂等重建。

## 3. 迁移 11 → 12

单事务 + 维护窗口（与 ADR 0015 §DDL 一致）：

1. 文件库先 `VACUUM INTO <db>.schema11.bak`（WAL 一致快照；已存在则保留最早一份，不覆盖）；
2. `PRAGMA foreign_keys=OFF`（SQLite 不能 `ALTER TABLE` 加表级复合 FK / 跨列 CHECK）；
3. `BEGIN IMMEDIATE`：**逐表重建** `vaults`/`devices`/`device_grants`/`invites`：
   `CREATE <name>__v12` → 复制**新旧共有的列**（新列取默认值/NULL）→ 行数 + 双向多重集校验 →
   `DROP` 旧表 → `RENAME`；
4. `challenges`/`tokens`：建新表 → `DROP` 旧表 → `RENAME`（**不复制行**）；
5. `CREATE TABLE IF NOT EXISTS` 补齐 `management_caps`/`device_credentials`/`dedup_v2`；
6. 重建索引 → `PRAGMA foreign_key_check` 必须为空 → 写 `schema_version=12` → 提交；
   任何一步失败整体回滚（旧库仍是 schema 11，旧 binary 仍可打开）。

### 链式迁移与形状识别

- **9 → 10 → 11 → 12** 一条链走完：9→10 仍只重建 schema 10 就有的 22 张表
  （`V10_TENANT_TABLES`），随后把 12 才有的三张表建成**空表**（否则重建后的
  `challenges`/`tokens`/`invites` 的复合 FK 无法解析）；
- 9→10 **在复制前**删除旧 `challenges`/`tokens`：schema 12 起 `realm` 是 INTEGER，
  v9 的 TEXT 行写不进 STRICT 新列；语义与旧实现（复制后删新表）完全相同；
- **元数据丢失**时按表形状判定版本（与 S2a/S2b 一致）：`vaults` 不存在 → 空库建表；
  `vault` 单表存在 → schema 9 走 9→10；`vaults.authz_state` 存在 → 12，否则按 **11** 继续
  跑 11→12（绝不"直接标成最新版"）。

## 4. 约束清单（数据库层兜底）

| 约束 | 兜的是什么 |
|---|---|
| `vaults` 三态跨列 CHECK | `authz_state='active'` 缺 policy/恢复钥/upgrade rev、legacy 带 V2 材料 |
| `vaults.upgrade_control_rev` 复合 FK | upgrade rev 指向不存在的控制提交（含跨租户） |
| `management_caps` scopes/delegable/时间 CHECK | 零/未知 scope 位、`delegable=true`、`expires_at<=not_before` |
| `management_caps` subject / issued / revoked 复合 FK | cap 挂到别的 vault 的设备或控制提交；撤销 rev 早于签发 |
| `challenges`/`tokens` realm CHECK + `realm<>3 OR cap_id NOT NULL` | 未知 realm；管理凭据未绑定 cap |
| `challenges`/`tokens`/`invites` cap 复合 FK | 跨租户 cap / 跨租户邀请签发者 |
| `devices`/`device_grants`/`device_credentials` approval 复合 FK | 证书/grant 的批准点不是本 vault 的真实提交 |
| `dedup_v2` PK + `control_rev` 复合 FK | 同 request_id 重复；回执挂到别的 vault 的提交 |

全部新 PK/UNIQUE/FK/索引都以 `vault_id` 开头；领域代码仍只经 `ScopeConn` 访问
（`auth=vault` 的显式作用域），跨租户在**数据库层**也会失败。

## 5. 运维与回滚

**升级前**

1. 停掉 relay 进程（单 writer；避免与迁移抢写锁）；
2. 备份 `<db>`、`<db>-wal`、`<db>-shm`（迁移自己也会留 `<db>.schema11.bak`，但它是
   迁移**开始**时的快照；运维备份才覆盖整个维护窗口）；
3. 确认磁盘有 ≥ 1 份库大小的空闲（`VACUUM INTO` 需要临时空间）。

**升级**：用新 binary 启动 → `apply` 自动迁移 11→12（或 9→10→11→12）。
核对：`relay schema`/`relay status` 输出 `schema_version=12`；启动日志无迁移错误。

**回滚**（失败的迁移已经整体回滚，此时库仍是 11，可直接换回旧 binary）：

1. 停进程（确认没有 writer）；
2. 恢复**完整快照**：运维备份，或 `<db>.schema11.bak`（连同 `-wal/-shm` 一起清掉/替换）；
3. 换回旧 binary 启动。

**禁止**：手工改 `relay_meta.schema_version`、用旧 binary 直接打开 12 库（会被版本门禁拒绝）、
在未恢复快照的情况下删除新表/新列。12 库一旦有新的 v1 写入，回滚只能回快照，
**迁移后产生的新数据会丢失**——需要保留时应先导出或接受丢失。

**迁移后的已知影响**：旧 challenge/token 全部失效（客户端重新认证即可）；邀请/已消费邀请、
管理幂等回执（v1 `dedup`）、控制链、对象/日志/游标、配额账本、lease、checkpoint 全部保留。

## 6. 未做（S4b 依赖）

- **payload/chain 语义**：`OperationV2` kind 0..14/16 的 payload schema、链 head/CAS、
  两签门限、`signers` 校验、时间窗，全部未实现（`control_commits` 仍是 v1 的裸 `signed_record`）；
- **`/authz/challenges|tokens`** 与 `/v2/.../control/submit` 未新增路由；`Realm` 只有
  1/2 两个变体（3/4 已在 DB CHECK 预留，`parse_tag` 遇到 3/4 仍是不变量错误）；
- **cap 写入**：没有签发/撤销/到期逻辑，`management_caps` 只有 DDL；
- **V2 凭证/grant/activation**：`device_credentials`/`dedup_v2` 只有 DDL；
  `DeviceCertV2`/`GrantAadV2`/`ActivationV2` 的写入与校验未接；
- **UpgradePolicy / 恢复 / 关闭 v1 鉴权**：`authz_state` 只可能是 `legacy`（无迁移入口会把
  vault 切到 `active`/`recovery_only`）；
- **集合摘要预映像**（activation 的 slot_set/member_cert/grants/资产）仍未固定（见 wire §6.4）。

S4b 落地时只需在此 schema 上写读写路径：表/列/约束已经就位，**不需要再升 schema**。

## 7. 验证（2026-11-14，Linux，dev `2b606f9` + 本切片）

```bash
export CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0
cargo fmt --all
cargo clippy -p menghunote-relay --all-targets -- -D warnings
cargo test -p menghunote-relay --all-targets    # 255 passed（246 基线 + 9 新）
cargo test -p menghunote-sync --all-targets     # 579 passed + 1 ignored
cargo test -p menghunote-app --features dev-sync  # 365 passed
```

新增测试（`crates/relay/tests/migration_v12.rs`，真实 schema 11 fixture =
`tests/fixtures/relay_schema11.sql` 冻结 DDL + 两个 vault 的全量数据）：

1. `v11_full_two_vault_fixture_migrates_to_v12_preserving_every_fact` —— 逐表按**迁移前捕获的
   列集**比对字节/行数；新列默认 `legacy/1/0/NULL`；控制链可逐条重算；FK 干净；快照可打开；
   重复打开幂等；
2. `interrupted_v12_migration_rolls_back_and_retry_succeeds` —— 注入故障 → 版本/表形状/行数
   全回滚，快照仍在；重试成功；
3. `missing_metadata_on_v11_shape_still_runs_the_v12_migration` —— 元数据丢失但表是 v11 形状：
   必须继续跑 11→12；
4. `future_schema_is_rejected_and_version_is_not_rewritten` —— 版本门禁（= 旧 binary 拒绝 12）；
5. `vault_authz_state_and_version_constraints_hold`、`management_cap_constraints_hold_across_vaults_and_revs`、
   `challenge_and_token_constraints_require_cap_and_stay_in_their_vault`、
   `v2_dedup_and_credential_history_constraints_hold` —— §4 的负例（`CHECK`/`FOREIGN KEY`/`UNIQUE`）；
6. `fresh_schema_12_has_the_adr_shape` —— 新表/列形状与 STRICT + PK 前缀。

测试临时目录用 `CARGO_TARGET_TMPDIR`；没有 `sleep`（全部同步断言）。
