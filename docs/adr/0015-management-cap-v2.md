# ADR 0015 — ManagementCapV2：设备管理能力的 wire/控制链契约

> 状态：**已决**（2026-11-14，GPT 决策；S4a–S4c 实施）。
> 关联：[ADR 0006](0006-sync-v1.md)（MNE1/控制链）、[ADR 0010](0010-device-access-revocation.md)、
> [ADR 0014](0014-multi-vault-invites-management.md)（C 目标）。
> **S0 门禁**：wire/离线 golden 可先做；真实 TLS、Windows 系统根/DPAPI 与恢复能力验收
> **必须先于真实 vault 启用/发布 V2**；未验前不宣称生产可用。

## 前提纠正

- v1 独立 revoke 只推进 `auth_rev`，**不写控制链**；V2 的设备撤销必须另立签名控制记录。
- v1 客户端遇到未知裸控制记录可能按 raw 忽略；**不能**靠“追加 V2 记录”让它 fail-stop。
  启用 V2 后必须由服务端关闭 v1 challenge/token/管理写，并要求 auth/control v2；外层 kind 显式标记。

## 决策

选 **ManagementCapV2 + 设备自己的 Ed25519 签名 + 控制链门限授权**。cap 是公开授权事实，
不是私钥；不向 Join 设备分发 `manager_seed`。保持 MNE1、CDE v1、历史控制字节不变；
新类型显式 V2，CDE 最短编码、整数键升序、字段精确齐全，拒绝未知/重复键、null/tag/float/
indefinite/尾随字节。`I/V/D/C/R` = instance/vault/device/cap/request ID（16B）；pk/hash=32B，
sig=64B；`u` 为 `0..i64::MAX`，计数溢出拒绝；新 cap/request ID 随机不复用。

### 固定对象

| 对象 | 字段（整数键） |
|---|---|
| `ManagementCapV2`（`0..12`） | 0:2, 1:I, 2:V, 3:C, 4:subject_D, 5:subject_ed_pk, 6:scopes(u32), 7:not_before, 8:expires_at, 9:issued_control_rev, 10:issued_authz_rev, 11:issuer_authority_rev, 12:false(delegable) |
| `OperationV2`（`0..14`） | 0:2, 1:I, 2:V, 3:kind, 4:R, 5:expected_control_rev, 6:prev_control_hash, 7:expected_authz_rev, 8:authority_rev, 9:signed_at, 10:not_after, 11:2(POST), 12:canonical_path, 13:payload(CDE), 14:signers[] |
| `PolicyV2`（`0..4`） | 0:2, 1:highrisk_threshold=2, 2:min_quorum=2, 3:max_cap_ttl≤2592000s, 4:max_invite_ttl≤86400s |
| signer | `{0:role,1:device_id(0/16),2:cap_id(0/16),3:pk32}`；role 0 设备/1 旧 creator 升级/2 离线恢复/3 目标 PoP；按 `(role,device,cap,pk)` 升序去重 |
| `SignedControlV2` | `{0:bytes(OperationV2),1:[sig64…]}`；签名数组与 signer 一一对应 |

scopes：`1 INVITE、2 APPROVE、4 REVOKE、8 CAP_ADMIN、16 ROTATE、32 POLICY`；拒绝零/未知位；
`delegable=true` 不支持。

### 签名域与提交

共同摘要 `h=SHA256(CDE(OperationV2))`：

- 设备签 `MN/v2/control\0 || h`
- 升级根签 `MN/v2/upgrade\0 || h`
- 恢复根签 `MN/v2/recover\0 || h`
- PoP 签 `MN/v2/pop\0 || h || pk32`

唯一提交路径 `/v2/vaults/{lowerhex32}/control/submit`，禁止 query/转义别名；路径/操作/参与者/
完整 payload 均已签入 OperationV2。链 hash `SHA256(prev_hash || SignedControlV2_bytes)`。

### 证书/grant/activation

- `DeviceCertV2`：`{0:2,1:I,2:V,3:D,4:sign_pk,5:hpke_pk,6:serial,7:data_rights(u32),8:issued_authz_rev}`；
  rights 仅 READ/WRITE，不兼任管理权限。
- `GrantAadV2`：`{0:2,1:I,2:V,3:epoch,4:recipient_D,5:recipient_hpke_pk,6:authority_rev,7:grant_id,8:issued_authz_rev}`；
  grant 包 `{0:aad_bytes,1:enc,2:ciphertext}`；HPKE info 改 `MN/v2/epoch-grant\0`，明文仅 epoch root。
  证书、AAD、enc、完整 ciphertext 都由两签 Operation 承诺，不另造单签旁路。
- `ActivationV2` 键 0..13：`2, rotation_id, from_epoch, to_epoch, freeze_S, primary_CID,
  primary_commit_hash, fallback_CID, fallback_commit_hash, slot_set_hash, member_cert_hash,
  grants_hash, old_asset_hash, new_asset_hash`；上下文/前链头由 OperationV2 绑定。
- 集合摘要对固定 canonical 项按 ID 升序编码后 SHA256；成员含完整证书、grant 含完整包；
  客户端必须取包重算，不能只信 relay 摘要。V2 替换而非扩展 `ActivationTV1`。

## 控制记录 kind

| kind | payload | 授权 |
|---|---|---|
| 0 UpgradePolicy | `{0:old_manager_rev,1:P,2:fresh_recovery_pk,3:bootstrap_caps[],4:ACTIVE_manifest[]}` | 旧根签 + ≥2 台现有 ACTIVE 同摘要确认 + 新恢复钥 PoP；一次提交初始化 authority/authz rev=1 |
| 1 PolicyReplace | `{0:P}` | 两台 POLICY；只影响后续签发，提前终止现有 cap 须显式撤销 |
| 2 CapGrant / 3 CapRevoke | `{0:cap}` / `{0:cap_id}` | 两台 CAP_ADMIN；签发 scope 不得超过每位签发者有效 scope |
| 4 DeviceApprove / 16 DeviceRightsChange | `{0:cert,1:grants[]}` | 两台 APPROVE；首次准入另需目标 PoP；绑定 PENDING 已提交两把公钥，ACTIVE 改权递增 serial；不得复活 revoked ID |
| 5 DeviceRevoke | `{0:sorted_device_ids[]}` | 两台 REVOKE；提交签名撤销事实并使目标全部 cap 失效；不能只改状态 |
| 6/7/8/9 Begin/Abort/Restart/AddRevokes | 按 ADR 固定 payload | 两台 ROTATE，含撤销还须各具 REVOKE；abort 不复活，候选 epoch 不复用 |
| 10 EpochActivate | `{0:ActivationV2}` | 两台 ROTATE；重算集合、冻结 S/双基线/资产/grant；只切数据 epoch，不换恢复 authority、不普遍撤 cap |
| 11/12 InviteCreate/InviteCancel | `{0:invite_id,1:token_hash,2:expires_at}` / `{0:invite_id}` | 一台 INVITE；只产生 PENDING；绑定签发 cap，cap 失效则未兑换邀请失效 |
| 13/14 RecoveryReset/RecoveryKeyChange | 按 ADR 固定 payload | Reset 当前离线根签；Change 两台 POLICY；均需新钥 PoP + 设备 adds 的 cert/grant/PoP |

所有效果只在控制 rev=`expected+1` 提交点生效；新 cap 的 issued rev 必须等于提交后的值；
客户端从已验证链推导状态，数据库行只是投影。

## 能力、门限与恢复

- 两签必须来自提交前有效的**两个不同 ACTIVE device ID 且两个不同公钥**，每位具备操作所需全部
  scope；两 cap/同设备两 key/新受赠者 PoP 不能凑门限；拒绝同公钥重复 ACTIVE。
- 无 cap 的 ACTIVE 设备可查看/申请；PENDING 只查自身；cap 从已验证 CapGrant/Upgrade 下载，
  签名钥仍是本机设备钥；cap 不含 root/不赋予解密、不能单方转授或自升权。
- 普通变更不得让任一高危 scope 的可用两设备 quorum 低于 2；到期自然失去 quorum 时**不得回退
  creator 单签**，只能提前两签续期或显式离线恢复。
- cap 有效：授予已提交、device/key 匹配且 ACTIVE、未撤销、authority_rev 当前、
  `not_before≤now<expires_at`；`issued_authz_rev` 仅历史证据，不要求等于当前 rev。
- 实时检查 cap/deadline；`signed_at` 与服务器时间偏差（过去≤300s、未来≤30s），
  `deadline≤signed_at+300s`，V2 链签名时间不倒退；历史回放按签名时刻验。
- 新恢复 authority 必须独立随机离线 seed，不能复用/派生旧 creator/RecoverySeed/epoch root；
  secret 不进在线身份、grant、relay。管理 authority 与当前 epoch 数据能力分别验证。
- 换恢复钥/Reset 时 authority_rev+1，旧代全部 cap 失效并原子配置新 cap；不足两台进入
  `recovery_only`，只许离线 Reset；旧恢复码即使能解旧数据也不能授权 V2。

**风险**：两台公钥不证明物理独立；离线恢复材料失守是终极旁路；恶意 relay 回滚/回填真实时间
不能仅靠签名消除；pin 只防已见历史回退，不解决未见分叉；已泄露 root/密文不能收回。

## 鉴权、revision 与事务

- 分开 `control_rev`（每提交+1）、`authz_rev`（授权状态变化+1）、`auth_rev`（令牌切断）、
  `authority_rev`（离线根代）；不复用随数据轮换变化的 manager_rev。
- 授权变化同事务推进 authz/auth rev 各一次；邀请/纯 rotation 只推进 control；epoch 激活至少推进
  auth_rev，若变成员则 authz 也+1；cap 自然到期不造记录，按 deadline 即时失效。
- `/authz/challenges` 请求 `{0:realm,1:subject16,2:cap_id(0/16)}`；realm 1 Device/3 ManagementDevice/
  4 Recovery，旧 realm2 仅 legacy。`ChallengeV2` 键 0..12：`2,I,origin_hash,V,realm,subject,
  challenge_id,nonce32,expires_at,auth_rev,authz_rev,cap_id,authority_rev`。
- `/authz/tokens` 请求 `{0:challenge,1:sig64}`，签 `MN/v2/auth\0 || challenge`；challenge 60s 单用；
  token 32B 随机仅存 hash，TTL Device 600s / 管理 min(600,cap 剩余) / 恢复 60s。
- management bearer 只选定一 cap，不代替管理签名；恢复 token 只能读恢复证据/提交 kind13。
- 每请求检查 token 两 rev **相等**，不能用旧 `<` 判断；管理 token 还检查当前 cap/authority。
- `BEGIN IMMEDIATE` 内重新鉴权→验证唯一 body/全部签名/提交前权限→双 rev/head hash/CAS→
  业务/链/投影/rev/dedup 一起提交；不允许事务外验 token 后竞态写。
- dedup 键 `(vault_id,request_id)`；同 D 摘要返原回执、异摘要 409；陈旧 CAS 必须新 request ID
  且双方重签；失权者可用仍有效 device token 查公开回执，不以幂等重放重新授权。
- 提交后发 vault 定向撤销通知；WS 每帧前重验、deadline 关闭，30s tick 仅兜底；失效 close 1008。
  不能承诺撤回在途字节。
- 每入口落实 `route.vault == token.vault == D.vault == cap.vault` 且 instance 匹配；跨租户 404
  不泄露存在性；token/邀请/回执/对象/游标不可串库。

## DDL、迁移与回滚

- **schema 11→12**；vault 加 `authz_version(1|2), authz_rev, authority_rev, recovery_pub,
  policy_bytes, upgrade_control_rev, authz_state(legacy|active|recovery_only)`，CHECK 模式/字段齐全；
  旧 vault 默认 legacy/0。
- 新 `management_caps`：PK `(vault_id,cap_id)`，存 canonical bytes/subject/key/scopes/时间/issued rev/
  authority/revoked_control_rev；复合 FK 连 devices/授予撤销 rev；历史不 GC。
- rebuild challenges/tokens 扩展 realm CHECK 与 `authz_version/authz_rev/authority_rev/cap_id`；
  管理 token 复合 FK 连 cap；迁移后旧 challenge/token 全部失效，不删已消费邀请事实。
- cert/grant 加 `approval_control_rev` 复合 FK 与 V2 凭证历史表；邀请记录 `issuer_cap_id`；
  V2 dedup 复合 PK；所有新 PK/UNIQUE/FK 含 vault_id，禁止旁路查询。
- Upgrade 在 active 且不轮换时单向：保留旧 bytes/rev/pin，签 ACTIVE 清单/历史凭证摘要，
  补齐 v1 裸撤销缺链证据；确认 ≥2 台现有设备、独立恢复备份/PoP，原子安装 policy/caps 并废旧鉴权。
- 升级后关闭**所有** v1 鉴权/管理写与旧语义 `/v2` 旁路；同步必须 V2 auth/control；未知 V2 记录
  fail-stop。不能假设旧客户端拒绝（它可能忽略未知 raw），必须由服务端阻止继续同步。
- 本地显式版本化受保护身份/control pin，带 V2 模式/head hash/authz/authority rev；普通管理端不存
  离线 seed，升级后不再用旧 manager_seed；旧客户端保留数据、提示升级，不换 pin/降级/清库。
- 迁移前 WAL 快照，验行数/摘要/FK/中断重试；旧 binary 拒 schema 12；回滚只恢复完整快照，
  新写入须导出或接受丢失；pin 已前进走显式恢复，禁止重签历史。

## 实施切片

| 切片 | 验收矩阵 |
|---|---|
| **S4a wire/验证/迁移** | 字段/域/长度/顺序/重复/未知键正负 golden；历史 head→Upgrade→caps 回放；断链/错租户/key/伪 raw/旧客户端门禁；schema 中断/重试/快照恢复；默认不开真实 vault |
| **S4b cap/鉴权/邀请**（S4b1 已完成；S4b2 = DeviceApprove/Revoke/RightsChange 已完成） | 非 creator 用自身 key 独立邀请；两签 cap 授予/撤销；自升权/同设备重复签/到期/撤销/旧 rev/重放/丢回应/CAS 竞态拒绝；cap 撤销后 token/WS 失效；同局部 ID 两 vault 全链隔离 |
| **S4c 完整高危与恢复** | APPROVE/REVOKE/角色/rotation/activation/换根单签全拒；两签不同 payload 或替换 cert/AAD/enc/ciphertext/集合全拒；最后 quorum 保护、RecoveryReset、旧 creator/恢复旁路全拒；真实 TLS/Windows 逐特性验后才放行 |

## 非目标

单设备无限管理、门限降级、细粒度文档 ACL、自动恢复码注册、收回已泄露数据、抗任意 relay 分叉/
可信时间证明、账户/HA。本决策落实 ADR 0014 的 C 边界，不是宣布已实现。

## 附：S4b 前置 wire 冻结（2026-11-14）

S4a1 实现时暴露的歧义，在此冻结；S4a1 golden 未覆盖处补 golden 后再接 relay：

1. **payload 形态**：`CapGrant` payload=`{0: bytes(ManagementCapV2 canonical CDE)}`；
   `CapRevoke`=`{0: bytes(cap_id16)}`；`DeviceRevoke` 的 ids 为 `array(bytes16)`；
   `InviteCreate` 的 token_hash 为 bytes32、expires_at 为 u；其余按 ADR 表。
2. **集合摘要 `SetDigestV2`**：输入为 `(id16, canonical_bytes)` 列表，按 id 字节升序去重，
   编码 `CDE(array(bytes))`（每个元素是该对象的完整 canonical CDE bytes），SHA256。
   - `member_cert_hash`：id=device_id，bytes=DeviceCertV2 canonical；
   - `grants_hash`：id=grant_id，bytes=GrantPackageV2 canonical（含 AAD/enc/ciphertext）；
   - bootstrap caps：id=cap_id，bytes=ManagementCapV2 canonical；
   - `slot_set_hash`/assets 按各自对象既有 canonical 编码，禁止裸拼接。
3. **UpgradePolicy 初始 rev**：提交前 `expected_authz_rev=0`、`expected_control_rev=head_rev`；
   Operation `authority_rev=1`；提交后 `control_rev=head+1`、`authz_rev=1`、`authority_rev=1`；
   bootstrap caps 的 `issued_control_rev=提交后 control_rev`、`issued_authz_rev=1`。
   payload 中 `old_manager_rev`=当前 v1 管理 rev；`fresh_recovery_pk` 为离线 seed 公钥；
   `ACTIVE_manifest` 含全部当前 ACTIVE 设备完整 DeviceCertV2（升序）。
4. **Upgrade signer 角色**：role1 旧 creator（device_id=creator、cap_id 空）1 个；
   role0 现有 ACTIVE 设备 2 个（device_id+cap_id 空，仅确认同摘要）；role2 新恢复钥 PoP 1 个
   （pk=fresh_recovery_pk，device/cap 空）。签名按 `(role,device,cap,pk)` 升序；上限 16。
5. **signer 字段约束**：role0 必填 device_id+cap_id；role1 必填 device_id、cap_id 空；
   role2 device/cap 空、pk=恢复钥；role3 必填 device_id、cap_id 空、pk=目标设备公钥。
   PoP 签名域 `MN/v2/pop\0||h||pk32`；role1/2 分别用 upgrade/recover 域。
6. **路径**：`canonical_path` 固定 `/v2/vaults/{lowerhex32}/control/submit`；不得 query/转义/
   大小写变体；relay 只接受该路径的 v2 提交。
7. **rev 下限**：cap/cert/AAD 的 issued/approval rev 必须 >0；Upgrade 前 authz_rev=0 仅允许
   legacy；升级后新对象不得为 0。
8. **signer 上限** 16（实现边界）；超过拒绝。签名数组长度必须等于 signer 数。

## S4b1 实施记录（2026-11-14）

> 状态：**S4b1 已实现**（Linux，dev `2b606f9` + 本切片）。**未启用真实 vault**、未改客户端
> UI/wire 兼容层；S4b2（DeviceApprove/Revoke/RightsChange）与 S4c（rotation/activation/
> RecoveryReset）当时仍未实现，对应 kind 一律 fail-stop（**S4b2 已由下节接上**，S4c 仍 501）。
>
> 代码：`crates/sync-proto/src/management_cap_v2.rs`（payload/`ChallengeV2`/`SetDigestV2`）、
> `crates/relay/src/control_v2.rs`（提交链/Upgrade/cap/invite）、`crates/relay/src/authz.rs`
> （`/authz/*`）、`crates/relay/src/{auth,api,pairing}.rs`（v1 门禁/路由/邀请兑换校验）。

### 1. 唯一提交入口与事务

- `POST /v2/vaults/{lowerhex32}/control/submit`，body **固定** `{0: bytes(SignedControlV2)}`；
  只接受 32 位小写 hex 段与逐字节唯一的 canonical 路径，query/大写/base64/转义/多余字段一律
  404/400（不生成第二种 wire 形式）。
- 提交是**自证**的：OperationV2 已把 vault/instance/kind/request_id/revs/head/path/payload/
  signers 全部签入；relay 按本 vault 的 DB 事实验证门限。控制提交**不要求 bearer token**
  （management bearer 只在 `/authz/*` 选定 cap，不代替管理签名）；入口走 v2 业务限流。
- 顺序（全部在 handler 的 `BEGIN IMMEDIATE` 内）：结构/租户/路径 → `dedup_v2` 幂等
  （同 request_id 同摘要回放原回执、异摘要 409）→ 时间窗 → 链 CAS
  （`expected_control_rev`/`prev_control_hash`）→ V2 签名时间不倒退 → 已知未实现 kind 501
  → authz/authority rev 绑定 → kind 校验/全部签名者验签 → 追加控制提交
  （`SHA256(prev || SignedControlV2)`）→ 投影/rev → 同事务写 `dedup_v2`。
- 幂等查询**先于**签名/时效：成功提交后 cap 可能被撤销/过期，重放必须仍返回原回执，
  不以幂等重放重新授权。回执只含公开链事实。
- 回执 wire：`{0: control_rev, 1: head_hash(32B), 2: kind, 3: request_id(16B),
  4: authz_rev, 5: authority_rev}`；不含 cap/私钥/root/payload。

错误映射：结构/CDE/`not_after` 超窗 → 400；签名不过 → 400 `SIGNATURE_INVALID`；
门限/角色/scope/authority/ACTIVE/manifest 不符 → 403；CAS/head/rev 陈旧、重复撤销、
同 request_id 异摘要、重复 cap/invite → 409；签名时间过旧/操作过期 → 410 `EXPIRED`；
已知未实现 kind → 501；未知 kind（15/≥17）在严格解码处 400。

### 2. UpgradePolicy（kind 0）

- 只在 `authz_state=legacy` 上成立；`expected_authz_rev=0`、`authority_rev=1`、
  `expected_control_rev=head`、`prev_control_hash=head_hash`；`old_manager_rev` 必须等于
  `vaults.manager_rev`。
- 签名者：role1 旧 creator ×1（`upgrade` 域，pk = 当前 `manager_keys`）；role0 现有 ACTIVE
  设备 ≥2（`control` 域，device 不同、公钥不同，cap_id 必须为空——升级前没有 cap）；
  role2 新恢复钥 ×1（`recover` 域，pk = `fresh_recovery_pk`）；role3 或其他组合一律拒绝。
- `ACTIVE_manifest` 必须逐字段等于 DB 全部 ACTIVE 设备（id/sign_pk/hpke_pk/instance/vault，
  `issued_authz_rev=1`，按 device_id 严格升序、无缺失/多余）。v1 设备证书是 `DeviceCertV1`，
  因此 manifest 只做**字段**一致性校验，不做 DB 证书字节比较。
- `PolicyV2` 门限固定 2/2（proto 层拒绝降为 1）；`max_cap_ttl ≤ 2592000`、
  `max_invite_ttl ≤ 86400`。bootstrap caps 的 `issued_control_rev=提交后 rev`、
  `issued_authz_rev=1`、`issuer_authority_rev=1`、subject 必须是 ACTIVE 设备且 key 匹配、
  TTL 在 policy 内、cap_id 唯一；至少两台不同设备持 `CAP_ADMIN`（拒绝把高危 quorum 降为 1）。
- 效果（同一事务）：追加控制提交 → 写 `management_caps` → `vaults` 切
  `authz_version=2/authz_rev=1/authority_rev=1/authz_state='active'`、写 `recovery_pub/
  policy_bytes/upgrade_control_rev`、`auth_rev+1`。旧 `manager_keys`/控制字节/rev/pin 全部保留，
  但旧管理钥不再能管理。
- 失败整体回滚（不含半装的 cap/vault 状态）；重复提交同字节回放回执。

### 3. CapGrant / CapRevoke（kind 2/3）

- CapGrant payload `{0: bytes(ManagementCapV2 canonical)}`；CapRevoke `{0: cap_id16}`。
- 两签必须来自**两个不同 ACTIVE device id + 两个不同公钥**，各自 cap 此刻有效
  （未撤销、`issuer_authority_rev` 当前、时间窗内、主体 ACTIVE 且注册公钥匹配）且含
  `CAP_ADMIN`；新 cap 的 scope 必须是**每一位**签发者有效 scope 的子集（不能靠两签拼权限）。
- 新 cap：instance/vault 匹配、`issued_control_rev=提交后 rev`、
  `issued_authz_rev=提交后 authz_rev`、`issuer_authority_rev=当前`、subject ACTIVE 且 key 匹配、
  `not_before ≤ now < expires_at`、`expires_at-not_before ≤ policy.max_cap_ttl`、cap_id 唯一。
- CapRevoke：目标存在且未撤销；写 `revoked_control_rev=提交后 rev`
  （DB `CHECK revoked > issued`）。重复撤销返回 409 `INVALID_STATE`（必须换 request_id 重签）。
- rev 效果：两者都 `control_rev+1`、`authz_rev+1`、`auth_rev+1`（授权变化同事务推进
  authz/auth）。**cap 自然到期不造记录**：每次使用按 `expires_at` 即时失效。
- 撤销后该 cap 的 `/authz` token 立即失效（token 两 rev 不等 + `authenticate` 重验 cap），
  WS 下一帧重验即 close 1008；提交后 `state.notify()` 兜底触发推送。

### 4. InviteCreate（kind 11）

- payload `{0: invite_id16, 1: token_hash32, 2: expires_at}`；只要一台 `INVITE` cap。
- `expires_at > signed_at` 且 `expires_at-signed_at ≤ policy.max_invite_ttl`，并仍在未来；
  invite_id/token_hash 冲突 → 409 `ID_CONFLICT`。
- 只写 `invites(issuer_cap_id=签发 cap)` 的 PENDING 事实：不附带证书/root/grant，不推进
  authz/auth rev（ADR：邀请只推进 control）。
- 兑换（现有 `/invites/{id}/redeem`）在 `issuer_cap_id != NULL` 时重验签发 cap：撤销/到期/
  authority 变更 → 403（v1 邀请行 issuer_cap_id=NULL，行为不变）。兑换本身仍只产生 PENDING，
  批准属 S4c。

### 5. `/authz/challenges|tokens`（management cap bearer）

- challenge 请求 `{0: realm, 1: subject16, 2: cap_id(0/16)}`；响应 `{0: bytes(ChallengeV2)}`。
  realm 1 = 普通设备；realm 3 = 管理设备（必须带 cap）；realm 2/4 不走这里。
- 仅在 `authz_state=active` 可用（legacy → 409 `INVALID_STATE`；recovery_only 属 S4c）。
- `ChallengeV2` 键 0..12，见 ADR 固定对象表；`origin_hash` 的实现取值是
  `SHA256(instance_id || vault_id)`（relay origin 绑定；客户端必须原样回传，签名覆盖它）。
- realm 3 逐项校验 cap：存在/未撤销/authority 当前/时间窗内/`cap.subject_device=subject`/
  注册公钥匹配；任何失败统一 403（不泄露 cap 状态）；未知 vault 才 404。
- token 请求 `{0: challenge bytes, 1: sig64}`，签 `MN/v2/auth\0 || challenge`；challenge 60s
  单用（CAS 消费），行内投影的 `authz_version=2/authz_rev/authority_rev/cap_id` 必须与字节
  和 vault 当前值一致；任何授权/鉴权变化让未兑现 challenge 失效。
- token 32B 随机、DB 只存 `SHA256`；TTL = realm1 `600s` / realm3 `min(600, cap 剩余)`；
  写 `tokens` 的 `authz_version/authz_rev/authority_rev/cap_id`。
- 每请求（含 WS 每帧）重验：token 的 `authz_rev/authority_rev` 必须**等于** vault 当前值
  （不是 `<`），realm 3 再验 cap 有效；失败 → 403 `REVOKED`（WS close 1008）。
- 响应 `{0: token, 1: expires_at, 2: server_time, 3: realm, 4: auth_rev, 5: authz_rev,
  6: authority_rev, 7?: cap_id}`；明文 token 只出现一次。

### 6. V2 启用后关闭 v1

`authz_state != legacy` 后：

- `/v1|v2/.../auth/{challenges,tokens}` 全部拒绝（含 realm 1；客户端必须改走 `/authz/*`）；
- 所有 v1 管理写（含挂在 `/v2` 上的同一 handler）在 `verify_management` 处返回 403；
- 升级前的旧 manager/device token 因 `auth_rev+1` + legacy 检查全部失效；
- 数据面 MNE1/对象/日志/游标语义不变（v1 wire 未改）。

### 7. 与 S4b 前置 wire 冻结的偏差说明

1. 冻结 §4 写"role0 现有 ACTIVE 设备 2 个（device_id+cap_id 空…）"，§5 又写"role0 必填
   device_id+cap_id"；S4a1 的 `SignerV2` 校验也要求 role0 带 device_id。实现取
   **device_id 必填、Upgrade 时 cap_id 必须为空**（升级前不存在 cap；不指定设备无法判断
   哪两台 ACTIVE 确认）。
2. role2 新恢复钥用 `recover` 域（冻结 §5），role3 PoP 才用 `pop` 域；本切片不接受 role3。
3. `SetDigestV2` 的预映像固定为 `CDE(array(bytes))`：每个元素是 byte string，内容是对象的
   完整 canonical CDE bytes；id 只用于升序去重（同 id 不同字节拒绝）。golden 见
   `crates/sync-proto/tests/golden_management_cap_v2.hex`（`set_digest_v2*`）。
4. `expected_authz_rev`/`authority_rev`：Upgrade 固定 0/1；其余 kind 必须等于提交前 vault
   当前值（不相等 → 409）。
5. 非 Upgrade 的 role0 signer 必须带 cap_id；Upgrade 的 role0 必须为空（见第 1 条）。

### 8. 限制与后续依赖

- **未做**：DeviceApprove/DeviceRevoke/DeviceRightsChange、rotation/activation、
  RecoveryReset/RecoveryKeyChange → 501 fail-stop；`recovery_only` 不可达。
- 不写 `device_credentials` 历史、不写 `devices/device_grants.approval_control_rev`
  （Upgrade 的 manifest 只校验字段，不落 V2 证书历史）。**S4b2 起**：kind 4/16 会写凭证历史
  与 `approval_control_rev`，Upgrade 的 manifest 仍只做字段校验（见下节 §2/§5）。
- 客户端（sync/app）仍未接 V2；**未启用真实 vault**，不宣称生产可用。
- `origin_hash` 是实现取值（见 §5）；若客户端需要其它 origin 定义，必须先改 ADR。
- 不撤回在途字节/WS 帧；cap 撤销只保证下一帧重验失败 + 通知兜底。

### 9. 验证（2026-11-14，Linux）

```bash
export CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0
cargo fmt --all
cargo clippy -p menghunote-sync-proto --all-targets -- -D warnings
cargo clippy -p menghunote-relay --all-targets -- -D warnings
cargo test -p menghunote-sync-proto              # 55 passed（含 4 个 S4b1 golden/负例）
cargo test -p menghunote-relay --all-targets     # 265 passed（255 基线 + 10 新）
cargo test -p menghunote-sync --all-targets      # 579 passed + 1 ignored
cargo test -p menghunote-app --features dev-sync # 365 passed
```

新增集成测试：`crates/relay/tests/management_cap_v2_upgrade.rs`（5 个）与
`management_cap_v2_caps.rs`（3 个），全部真实 HTTP 路由 + 真实 Ed25519、无 sleep。

## S4b2 实施记录（2026-11-14）

> 状态：**S4b2 已实现**（Linux，dev `53f1b15` + 本切片）。**未启用真实 vault**、未改客户端
> UI/wire 兼容层；rotation/activation/recovery（kind 6–10/13/14）仍 501 fail-stop，留 S4c。
>
> 代码：`crates/sync-proto/src/management_cap_v2.rs`（kind 4/5/16 payload + 集合字段数）、
> `crates/relay/src/control_v2/mod.rs`（提交链/kind 分派）、`crates/relay/src/control_v2/devices.rs`
> （DeviceApprove/DeviceRevoke/DeviceRightsChange 的校验与投影）。

### 1. payload 与 wire 取值（补 S4b 冻结的空白）

- `DeviceApprove`(4) / `DeviceRightsChange`(16) **共用一个 wire 形状**：
  `{0: bytes(CDE(DeviceCertV2)), 1: array(bytes(CDE(GrantPackageV2)))}`——证书与 grant 都是
  **整份 canonical CDE 的 byte string**（不是嵌套 map），与 Upgrade 的 manifest、`CapGrant`
  的 cap、`SetDigestV2` 的成员编码同一口径。proto 侧只校验结构：证书/包各自合法、`grant_id`
  两两不同、`epoch` 两两不同、数组非空且 ≤32；证书与包之间的语义（recipient/hpke/rev/epoch）
  由 relay 按 DB 事实校验。缺少独立 golden 的三类 payload 已补 `device_approve_payload` /
  `device_revoke_payload`（独立 Python 生成）。
- `DeviceRevoke`(5) = `{0: array(bytes16)}`（S4b 冻结 §1），要求**严格升序去重**、非空、≤32
  （去重后集合摘要/链上字节唯一，重复目标不可能有两种编码）。
- 两签签名覆盖 `OperationV2` 摘要，而证书/AAD/enc/完整 ciphertext 都在 payload 里：
  换任一字节 → 400 `SIGNATURE_INVALID`（不存在"重签绕过"的旁路）。

### 2. DeviceApprove（kind 4）

- 门限：两台**不同** ACTIVE 设备 + **不同**公钥，各自 cap 此刻有效（未撤销/authority 当前/
  时间窗内/主体 ACTIVE 且公钥匹配）且**本人**含 `APPROVE`（不能靠两签拼权限）。首次准入另需
  **目标 PoP**（role3：`device_id`=目标、无 cap、pk=目标已登记公钥，验
  `MN/v2/pop\0 || h || pk32`）。PENDING 目标缺 PoP → 403；ACTIVE 目标不接受 role3。
- 目标 PENDING：证书必须逐字段绑定兑换时提交的 `sign_pk`/`hpke_pk`（防 approve 换钥）；
  `serial=1` 且该设备**没有任何**历史凭证（serial 不复用）；`issued_authz_rev` = 提交后
  authz rev；`data_rights` 必须含 READ（可带 WRITE，不允许"只写"设备）。
- grant 包：恰好一份，`recipient`=目标、`recipient_hpke_pub`=证书 hpke、`epoch`=当前活动
  epoch、`authority_rev`=当前代、`issued_authz_rev`=提交后 rev、`grant_id` 在全表不复用
  （复用 → 409 `ID_CONFLICT`）。多 epoch 包属于轮换/激活（S4c），本切片 403。
- 已 ACTIVE：证书与 grant 包必须与库内**逐字节**相同（同一次批准的重放）→ 只追加控制记录，
  **不**推进 authz/auth；证书不同 → 409 `INVALID_STATE`（改权必须走 kind 16 并递增 serial）。
- 已 REVOKED：403，**永不复活**。
- 效果（同一事务）：`device_credentials` 追加一行（`approval_control_rev`=本次提交、
  `certificate_signature=NULL`——V2 证书的授权就是这条两签控制记录）、`devices` 置 ACTIVE 并写
  `approval_control_rev`/`issued_authz_rev`、`device_grants` upsert 当前 epoch 包
  （`signed_header`=grant AAD 字节、`signature=NULL`、`hpke_enc`/`hpke_ciphertext`=包内字段，
  `manager_rev` 保留 vault 当前值——v1 遗留列，不参与 V2 校验）、`authz_rev+1`、`auth_rev+1`。

### 3. DeviceRevoke（kind 5）

- 两签 `REVOKE`（同样要求两个不同 ACTIVE 设备/公钥，各自 cap 有效且含 REVOKE）。
- 目标：全部必须存在（否则 404）；PENDING → 403（v1 语义：空证书占位不能置 revoked）；
  已 REVOKED 视作幂等目标。
- quorum：撤销后剩余 ACTIVE 设备 ≥ `PolicyV2.min_quorum`(2)；任一高危 scope
  （APPROVE/REVOKE/CAP_ADMIN/ROTATE/POLICY）在撤销前已具备两设备 quorum 时，撤销后不得低于 2
  （按有效 cap 计：未撤销/authority 当前/时间窗内/主体 ACTIVE 且公钥匹配）。已经不足两台的
  情形不因本次撤销变得更差，不额外拦截。
- 效果：`devices.status='revoked'` + `revoked_at`；目标名下**全部** cap 写
  `revoked_control_rev`（复合 FK 指向本次提交，DDL 的 `CHECK revoked>issued` 兜底）；
  `authz_rev+1`、`auth_rev+1`。**不删**证书/grant/凭证历史（证据保留）。
- 级联：`auth_rev+1` 让所有旧 bearer（含 realm 3 管理 token）下一次请求立即失效；
  `authz_rev` 前进让未兑现 challenge 与 `/authz` token 失效，WS 在下一帧重验时 close 1008；
  提交成功后 handler 的 `state.notify()` 立即兜底触发一次重验。**已发出的字节不能撤回**。
- 重复撤销（新 request_id）：签名事实仍上链（control+1），但 `changed=false`，authz/auth 不动
  （与 v1 独立 revoke 的幂等口径一致）。
- 未兑换邀请：绑定 cap 随设备撤销失效；兑换时 `pairing::redeem_invite` 重验 `issuer_cap_id`
  对应 cap → 403，因此"邀请失效"是**派生**事实，不改写邀请历史（不伪造 `used_at`/`expires_at`）。
- 与 v1 隔离：V2 vault 上 v1 管理写因管理鉴权/legacy 门禁全部 403，设备撤销只走 kind 5
  （不再依赖 v1 那条"只改 access 状态、不写控制链"的旁路）。

### 4. DeviceRightsChange（kind 16）

- 两签 `APPROVE`；目标必须 ACTIVE（PENDING 只能走 kind 4；REVOKED → 403）。
- **只改数据权限**：证书里的 `sign_pk`/`hpke_pk` 必须仍是登记值（换钥属轮换/激活，S4c）。
- `serial` 必须 `max(serial)+1`（无凭证历史时按 Upgrade manifest 的 serial=1 起算）；
  旧证书行不覆盖（`device_credentials` PK `(device_id, serial)`，历史可验）。
- 权限必须真的变化（相同 → 409 `INVALID_STATE`），且必须含 READ（不允许"只写"设备）；
  可写设备数在变更前 ≥2 时，变更后不得 <2（数据权限 quorum；管理 quorum 由 cap 派生，
  不受数据权限影响）。
- grant 包与批准同口径（当前 epoch 一份、recipient/hpke/authority/issued rev/新 grant_id）。
- 效果：追加新凭证行、更新 `devices.certificate`/`approval_control_rev`/`issued_authz_rev`、
  upsert 当前 epoch grant、`authz_rev+1`、`auth_rev+1`；旧证书与旧 grant 事实都不删。

### 5. 限制与后续依赖

- **未做**：rotation/activation（6–10）、InviteCancel(12)、RecoveryReset/RecoveryKeyChange
  （13/14）→ 501；`recovery_only` 不可达。多 epoch grant、重签证书换钥、跨 epoch 改权不在本切片。
- 不改客户端（sync/app 仍未接 V2）、**未启用真实 vault**、不宣称生产可用；v1 wire/路由不变。
- Upgrade 的 ACTIVE manifest 仍**不落** `device_credentials`（S4b1 口径），因此"升级后首次改权"
  以 manifest 的 serial=1 为基线（改权后历史从 serial=2 起累积）。
- 撤销/改权只保证"下一次请求/下一帧"失效，不能撤回在途字节；离线持有的旧 root 仍能解旧密文
  （ADR 0010 的安全边界不变）。

### 6. 验证（2026-11-14，Linux）

```bash
export CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0
cargo fmt --all
cargo clippy -p menghunote-sync-proto --all-targets -- -D warnings
cargo clippy -p menghunote-relay --all-targets -- -D warnings
cargo test -p menghunote-sync-proto              # 57 passed（55 + 2 个 S4b2 payload）
cargo test -p menghunote-relay --all-targets     # 268 passed（265 基线 + 3）
cargo test -p menghunote-sync --all-targets      # 579 passed + 1 ignored
cargo test -p menghunote-app --features dev-sync # 365 passed
```

新增集成测试：`crates/relay/tests/management_cap_v2_devices.rs`（3 个用例，覆盖批准/撤销/改权
正负例、WS 级联、quorum、跨 vault 隔离与逐条链 hash 复算），全部真实 HTTP 路由 + 真实
Ed25519 + 真实 PoP、无 sleep（FakeClock + 显式收敛点）。

## S4c 实施记录（2026-11-14）

> 状态：**S4c 已实现**（Linux，dev `53f1b15` + 本切片）。**未启用真实 vault**、未改客户端
> UI/wire 兼容层；真实 TLS/Windows 系统根/DPAPI 仍未验，**不宣称生产可用**。
>
> 代码：`crates/sync-proto/src/management_cap_v2.rs`（kind 6–10/12/13/14 payload 与字段数）、
> `crates/relay/src/control_v2/rotation.rs`（轮换/激活）、
> `crates/relay/src/control_v2/recovery.rs`（Reset/KeyChange + `recovery_only`）、
> `crates/relay/src/control_v2/mod.rs`（kind 12 + 分派 + `recovery_only` 门禁）、
> `crates/relay/src/{auth,authz,api}.rs`（realm 4 与恢复证据只读端点）。

### 1. 轮换控制（kind 6–10）

payload 按 ADR「控制记录 kind」表（`bytes(CDE(对象))` 同口径，不另造嵌套 map）：

| kind | payload | 门限 |
| --- | --- | --- |
| 6 RotationBegin | `{0:rotation_id,1:from_epoch,2:to_epoch,3:revoke_ids[]}` | 2× ROTATE（含撤销时每位还须 REVOKE） |
| 7 RotationAbort | `{0:rotation_id}` | 2× ROTATE |
| 8 RotationRestart | `{0:rotation_id,1:new_to_epoch,2:extra_ids[]}` | 2× ROTATE（含撤销时每位还须 REVOKE） |
| 9 RotationAddRevokes | `{0:rotation_id,1:device_ids[]}` | 2× ROTATE + 每位 REVOKE |
| 10 EpochActivate | `{0: bytes(CDE(ActivationV2))}` | 2× ROTATE |

- **授权只走 V2 控制记录**：本切片从不调用 `auth::require_management`/`verify_management`，
  V2 vault 上不存在"管理钥单签旁路"（v1 轮换路由在此之前已被 legacy 门禁关闭）。
- **候选 epoch 不复用**：`to_epoch`/`new_to_epoch` 必须 `> active_epoch` 且**从未出现在
  `epochs` 表**（aborted/retired 也算已分配）；`rotation_id`/`grant_id`/`cap_id` 同样
  不复用（重复 → 409）。V2 的 epoch 由签名 payload 指定，relay 只做「从未分配」的持久
  事实校验并**跳号前进** `next_epoch`（`vault::allocate_epoch` 只按 `next_epoch` 递增）。
- **abort 不复活**：`RotationAbort` 删除候选 epoch 的 staged grants、候选 checkpoint 标
  aborted、epoch 标 aborted；旧 epoch 只有在 `epochs.revoked_once = 0` 时才解冻回
  `active`，**发生过吊销的旧 epoch 永久保持 `rotating`**（ADR §23.1）。
- **add-revokes 不自造 epoch**：候选 epoch 一旦已有材料（staged grant / 候选 checkpoint /
  staging 槽），kind 9 返回 409 `INVALID_STATE`——新候选 epoch 必须由**签名的**
  `RotationRestart` 指定（relay 不能发明一个未签名的 epoch）。
- **时间/门限/CAS**：`signed_at` 过去 ≤300s、未来 ≤30s，`not_after ≤ signed_at+300s`；
  链 CAS（`expected_control_rev`/`prev_control_hash`）与 `expected_authz_rev`/
  `authority_rev` 全部沿用 S4b1 的通用校验；签名时间不倒退。
- **rev 推进**：begin 推进 `auth_rev`（freeze 切断全部旧 token，即使撤销为空）；abort/
  restart/add-revokes 只推进 `control`；每台被吊销设备再各推进一次 `auth_rev`。

### 2. EpochActivate（kind 10）

- 上下文绑定：`ActivationV2` 的 `rotation_id/from_epoch/to_epoch/freeze_seq` 必须逐字段
  等于本 vault 的持久事实（`from_epoch == vault.active_epoch`），否则 409。
- 双基线：`checkpoints::promote_named_candidates` 只 promote 被指名的两个 cid（独立性/
  覆盖冻结 S/健康/完整性/`commit_binding_issues` 全在既有实现里），
  `primary_commit_hash`/`fallback_commit_hash` 必须等于 `SHA256(signed_commit)`。
- 全成员 grant：每个 ACTIVE 设备都必须有候选 epoch 的包（缺 → 409 `INCOMPLETE`）；
  候选 epoch 必须已发布 password + recovery 两个 staging 槽（`slots::rotation_slots_ready`）。
- **五个集合摘要全部按 DB 事实重算**后逐字段比对（任一不符 → 409，客户端必须重新准备
  并重签）：

  | 字段 | 口径 |
  | --- | --- |
  | `slot_set_hash` | `rotations::slot_set_digest`：`SHA256(concat(slot_id \|\| SHA256(descriptor)))`（既有编码） |
  | `member_cert_hash` | `SetDigestV2`：`(device_id, devices.certificate)` 全体 ACTIVE 的**完整证书字节** |
  | `grants_hash` | `SetDigestV2`：`(grant_id, 完整 GrantPackageV2 canonical)`（AAD/enc/完整 ciphertext） |
  | `old_asset_hash` | `objects::asset_set_digest(from_epoch, freeze_seq)` |
  | `new_asset_hash` | `objects::candidate_asset_set_digest(to_epoch, rotation_id)` |

- **只切数据 epoch**：不换恢复 authority、不改 policy、**不普遍撤 cap**（旧 cap 仍挂在
  当前 authority 代）；`auth_rev+1`（全部旧 bearer/WS 失效），**成员集合真的变化时**
  `authz_rev+1`。成员变化用「begin 时写入 `rotations.member_set_hash` 的冻结成员摘要」
  与激活时的当前摘要比较（v1 遗留行 `member_set_hash` 为 NULL 时保守按"已变化"处理）。
- 效果（同一事务）：旧 active 槽 → retiring（14 天）、候选 staging 槽 → active；
  旧 epoch → retired + `retire_at`、新 epoch → active；`rotations` 写 activated/
  `member_set_hash`/两个 cid/`activation_commit = SHA256(CDE(ActivationV2))`/
  `activated_control_rev`；`vaults.active_epoch = to_epoch`、`state = active`；
  轮换 reservation 完成；`quota::recompute` 按真实 parts 对账。
- 激活后旧 epoch 的数据/权限语义按 ADR：**不收回已泄露数据**，只保证下一次请求/下一帧
  重验失败（WS close 1008）。

### 3. InviteCancel（kind 12）

- payload `{0: invite_id}`；只要一台 `INVITE` cap（与 kind 11 同口径，cap 必须此刻有效）。
- 目标必须存在（否则 404）；**已兑换**（`used_at` 非空且 `pending_device` 非空）是永久
  事实 → 409 `INVALID_STATE`（不伪造 `used_at`、不删行）；**已取消**（`used_at` 非空、
  `pending_device` 为空）→ 幂等成功（只追加控制记录）。
- 投影：`used_at = 取消提交时刻`、`pending_device` 保持 NULL（兑换投影必须同时写
  `pending_device`，因此两种事实可区分）；`expires_at` 保留原值供审计，控制记录才是
  永久事实。只推进 `control`（ADR：邀请只推进 control）。
- 与 kind 11 的 cap 失效语义一致：签发 cap 被撤销/到期/authority 变更后，未兑换邀请在
  兑换时重验 `issuer_cap_id` 即 403（派生事实，不改写邀请历史）。

### 4. RecoveryReset（13）/ RecoveryKeyChange（14）

payload（S4c 冻结的 wire 形状）：

```text
13 RecoveryReset     {0: bytes(CDE(PolicyV2)), 1: fresh_recovery_pk32,
                      2: bootstrap_caps[](bytes(CDE(ManagementCapV2))),
                      3: device_adds[](bytes(CDE(DeviceCredentialPayloadV2))),
                      4: revoke_ids[](bytes16, 严格升序去重)}
14 RecoveryKeyChange {0: fresh_recovery_pk32,
                      1: bootstrap_caps[](bytes(CDE(ManagementCapV2)))}
```

- **授权面**：Reset = 旧恢复根（`role2` + `pk = vault.recovery_pub`，`recover` 域）
  **加**新钥 PoP（`role2` + `pk = fresh_recovery_pk`，同域）；KeyChange = 两台不同
  ACTIVE 设备的 `POLICY` cap **加**新钥 PoP。设备 adds 每位还须目标设备 PoP（`role3`，
  `MN/v2/pop\0 || h || pk32`）。**不接受 role0 参与 Reset**（恢复记录不是普通管理写）。
- **为什么两个 kind 都带 bootstrap caps**：ADR「换恢复钥/Reset 时 `authority_rev+1`，
  旧代全部 cap 失效并**原子配置新 cap**」。`effective_cap` 逐请求要求
  `issuer_authority_rev == vault.authority_rev`，代一换旧 cap 自动失效；同一提交里不装
  新 cap 会让管理面死锁（没有有效 cap 就无法再签任何管理记录）。「KeyChange 仅换恢复钥」
  指**不改 policy/成员集合**，不指不重签 cap。
- **不能复活 / 不能降门限**：adds 只接受 PENDING（`serial=1`、无凭证历史、证书绑定兑换
  时提交的两把公钥、`issued_authz_rev` = 提交后值），已 ACTIVE 拒绝（换钥属轮换）、已
  REVOKED 403（**永不复活**）；`PolicyV2` 在 proto 层固定 2/2，恢复不能降门限；结果仍有
  ≥2 台 ACTIVE 时必须保持 `active` 且至少两台不同设备持 `CAP_ADMIN`。
- **`recovery_only`**：结果不足两台 ACTIVE 时 `authz_state = 'recovery_only'`——只许
  kind 13（其余管理写 403），管理 realm 不再签出有效 cap bearer；设备 realm 与恢复 realm
  仍可读（撤权不等于删数据）。再 Reset 且带设备 add 使成员 ≥2 时回到 `active`。
- **原子安装**：撤销/凭证历史/`devices` 投影/grant/新代 caps/`vaults`（`authority_rev+1`、
  `recovery_pub`、`policy_bytes`、`authz_state`、`authz_rev+1`、`auth_rev+1`）全在同一
  `BEGIN IMMEDIATE` 事务里；任一校验失败整体回滚（零写入）。旧 `manager_keys` 行保留为
  历史（V2 里没有 bearer 语义）。
- **审计**：控制记录本身是审计事实（`control_commits` 永久保留、可逐条复算链 hash）。

### 5. realm 4（离线恢复）

- `Realm::Recovery`（DB 整数 tag 4）：`/authz/challenges` 的 `subject` 必须等于
  `SHA256(recovery_pub)[..16]`，`cap_id` 必须为空；challenge 60s 单用；token TTL **60s**
  （`AUTHZ_RECOVERY_TOKEN_TTL`），DB 只存 `SHA256`。
- 每请求：token 的 `authz_rev`/`authority_rev` 必须**等于** vault 当前值（不是 `<`），
  并重验 `recovery_pub` 仍是签发时那颗（换钥/Reset 后旧恢复 bearer 立即 403 `REVOKED`）。
- **只读恢复证据**：`GET /v2/vaults/{lowerhex32}/recovery/evidence`（realm 4 专用）返回
  `{0 authz_state,1 authority_rev,2 authz_rev,3 control_rev,4 head_hash,5 recovery_pub?,
  6 policy_bytes?,7 active_epoch,8 vault_state,9 members[](0 device_id,1 certificate),
  10 member_cert_hash,11 grants_hash,12 pending_rotation?}`。
  `authenticate_v2` 显式拒绝 realm 4，因此恢复 bearer 不能读数据面/管理面；kind 13/14
  仍是**自证**的控制记录（bearer 不能代替离线恢复根签名）。
- **旧 recovery code/旧 creator/旧 manager_seed 在 V2 下不能授权**：v1 管理写（含
  `/auth/challenges|tokens` 与全部 v1 管理路由）在 `authz_state != legacy` 后一律 403；
  role1（旧 creator）只在 UpgradePolicy 里成立；role2 的 pk 必须等于当前/新恢复根。

### 6. 事务与安全

- 控制 head/CAS/dedup/时间窗沿用 S4b1：所有效果只在 `control_rev = expected+1` 提交点
  生效，`dedup_v2` 同事务写回执（重放返回原回执，不以幂等重放重新授权）。
- `recovery_only` 门禁在 `submit` 的授权状态检查处（先于任何 kind 专属校验）。
- 跨租户：`route.vault == token.vault == operation.vault` 且 instance 匹配；跨 vault 的
  控制记录/恢复记录 403/409，不泄露存在性。
- 不泄露租户/密钥：回执只含公开链事实；恢复证据只含公开证书/摘要。

### 7. 限制与后续依赖

- **未做**：`PolicyReplace`（kind 1）仍 501 fail-stop；客户端（sync/app）未接 V2；
  **未启用真实 vault**；真实 TLS/Windows 系统根/DPAPI 未验。
- `rotations.member_set_hash` 在 V2 里存的是**冻结时**的成员摘要（v1 是激活后的值）：
  这是"成员是否变化"的唯一可复算依据，已在 §2 说明；v1 轮换行语义不变。
- 恢复记录不重写 `upgrade_control_rev`（它记录 UpgradePolicy 的提交 rev）。
- **Reset 不解除冻结**：`RecoveryReset` 只换授权代（`authority_rev`/恢复钥/policy/
  成员/caps），**不**改 `vault.state`/`active_epoch`。若旧 epoch 因撤销而永久冻结
  （abort 后保持 `rotating`），Reset 之后仍必须走一次正常的 begin→材料→activate
  才能恢复数据写入——否则等于允许继续写"被撤销设备仍持钥的旧 epoch"，违反撤权语义。
- 已发出的字节不能撤回：cap 撤销/epoch 切换只保证下一帧重验失败 + 通知兜底。
- 离线持有的旧 root 仍能解旧密文（ADR 0010 的安全边界不变）。

### 8. 验证（2026-11-14，Linux）

```bash
export CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0
cargo fmt --all
cargo clippy -p menghunote-sync-proto --all-targets -- -D warnings
cargo clippy -p menghunote-relay --all-targets -- -D warnings
cargo test -p menghunote-sync-proto              # 61 passed（57 基线 + 4 个 S4c payload/golden）
cargo test -p menghunote-relay --all-targets     # 277 passed（268 基线 + 9）
cargo test -p menghunote-sync --all-targets      # 579 passed + 1 ignored
cargo test -p menghunote-app --features dev-sync # 365 passed
```

新增集成测试（真实 HTTP 路由 + 真实 Ed25519、无 sleep、逐条复算链 hash）：
`crates/relay/tests/management_cap_v2_rotation.rs`（4）与
`crates/relay/tests/management_cap_v2_recovery.rs`（5）。
