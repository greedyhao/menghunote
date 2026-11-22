# ADR 0016 — 双签协调：ApprovalEnvelope 与收件箱

> 状态：**已决**（2026-11-14，GPT 决策；S4d1/S4d2）。
> 关联：[ADR 0015](0015-management-cap-v2.md)（OperationV2/两签）。
> S0 门禁：可先做离线/仿真；真实 vault 启用/发布 V2 前必须过真实 TLS/Windows。

## 现状纠正

- kind 1 `PolicyReplace` 仍 501，15 未分配；并非 0–16 全实现。
- **signers 参与摘要**，`SignedControlV2` 必须满签；不能先签后增加第二人。
  `submit` 自证、无需 bearer；协调包必须携带完整已满签操作。
- 单签仅 **11/12 InviteCreate/Cancel**（INVITE）；其余高风险需双签。
  首次 Approve 另需目标 PoP；Upgrade 用旧 creator 根 + 两台 ACTIVE 确认 + 新恢复钥证明。
- 时间窗：提交时过去≤300s、未来≤30s，`not_after≤signed_at+300s`，链时间不倒退；
  不能跨天等待授权，长期只能存未授权草稿，双方就绪后重新构造/重签。

## 决策

**S4d1 手工通道 A，S4d2 默认收件箱 B；共享不可变操作与签名包。**
relay 只是运输/存储者，不提供第二签、不持有管理私钥。

### 共同包 `ApprovalEnvelopeV1`

canonical CDE：
`version、完整 op_bytes、h=SHA256(op_bytes)、initiator_index、[(signer_index,sig64)]、cap/控制链证据`。
- `h` 必须重算；缺签不使用零占位；签名槽在构造时冻结（角色/device/cap/pk 有序），
  换 B/cap 必须新建 Operation，原签名作废。
- 文件 `.mn-approval` 二进制 CDE；复制用带版本前缀的 base64url；二维码只承载完整小包，
  容量不足转文件；**只有摘要的二维码不能用于签署**。
- 导入限长、不自动执行/联网；B 验证严格编码/完整 payload/I/V/本地 pin/路径/request_id/
  control/head/authz/authority rev/全部角色与签名/cap 来源与 scope/设备 key；按 kind 验证
  cert/grant/集合承诺；包内 URL 不能改变连接目标。
- 双端显示同一操作详情（动作/目标/权限差异、租户/实例/路径/rev、A/B 指纹、cap scope/期限、
  h 的 128 位比较指纹 + 可展开 256 位）；确认页由解析后的 Operation 生成。

### cap 获取/存储/选择

- 首版用 realm1 设备鉴权 `GET /v2/vaults/{V}/control?after_rev=…` 逐页验证并回放
  Upgrade/CapGrant/Revoke/DeviceRevoke/Recovery；每页≤256；无需新 cap endpoint。
- 裸 cap/members 投影不是授权证据，必须关联已验证控制链；离线只用已验证前缀，
  证据缺口不能手动“相信”跳过。
- 本地 vault SQLite 版本化公开 meta 存 canonical cap/来源 rev/撤销投影/验证水位；
  受保护 V2 control pin 存 I/V/head/authz/authority；缓存可重建，不作信任根。
- 选本机 device/key 匹配、当前 authority、未撤销、时间有效的 cap；一张 cap 覆盖全部所需
  scope，不能拼 cap/拼两人 scope；CapGrant 新 scope 为每位签发者 scope 子集。
- 启动/签前/提交前/rev 变化/失败后刷新；自然到期即时失效；realm3 token 绑定本机所选 cap；
  设备钥仍在本机封装身份，不导出 manager_seed/设备钥。

### 收件箱 B（S4d2，schema 12→13）

- `approval_jobs`：PK `(vault_id,request_id)`、UNIQUE `(vault_id,op_digest)`；字段
  op_bytes/op_digest/initiator_device+cap+index/recipient_device/kind/expected revs+head/
  created_at/expires_at/status/seq/receipt_ref；投影从 op 派生。
- `approval_signatures`：PK `(vault_id,request_id,signer_index)`，复合 FK 连 jobs；
  sig64/接收时间；索引 `(vault_id,recipient_device,status,seq)`；所有键/FK 含 vault。
- `/v2/vaults/{V}/approvals`：POST 上传 A 已签包；GET list（自己的待办）；GET {R}；
  POST {R}/signatures；POST {R}/cancel。
- 创建须 realm3 主体/cap 匹配 A 且 A 签名有效；list/read 仅 A 与预选 B、当前 ACTIVE 且持
  所需 scope；加签匹配固定槽；事务内重验授权/时间/CAS；可见性失败统一 404。
- PENDING 目标 PoP/Upgrade/离线根证明先沿 A 收集，不开放通用收件箱；接替 B 须 A 重新冻结
  并签新操作。
- 上传完整 Operation 及公开 cap 证据/签名（cert、grant AAD/enc/密文）；不上传笔记明文/
  root/seed/邀请 token/私密备注；索引/通知只摘要与状态。
- ≤1MiB 控制预算；按 IP/设备/cap/vault 限流；源设备≤8、vault≤64 待办、总字节上限、
  分页≤50；先鉴权/限长再验签；禁止匿名 vault 列表/全设备收件箱。
- `expires_at=min(op.not_after,参与 cap 到期)`；rev/head 变化惰性 stale；到期不可签；
  分批清理正文/签名，终态 24h 摘要墓碑；不删正式 control/dedup 历史。
- HTTP 轮询为基线（启动/重连/前台约 30s），WS 可选；丢通知不影响正确性；
  幂等 `(V,R,h)`；submit dedup 比较完整 SignedControl 字节，A/B 通道不得生成变体。
- 迁移 WAL 快照/FK/中断重试；旧 binary 拒 13；可关收件箱退回 A，不能降 V2 规则；
  数据库回退只完整快照恢复并明确新写损失。

### 双签 UI/恢复

- 状态：草稿→冻结双方→A 签署→等待 B 审核→满签就绪→提交中→控制链验证→完成；
  收件箱 “approved”/裸回执 != 已执行。
- A/B 签署即正式授权；满签包任何持有者可 submit；取消只停止协调，已外发签名不可撤回。
- 过期/CAS/rev 变化/撤权/目标变化→失效；用新 R/时间/payload 重新构造，全部重签；
  禁止自动改 rev/延 deadline/后台自动签。
- 本地持久化 op/签名/发送状态/回执；重启重验 pin/control；丢响应重发原字节。
  离线显示“撤权/时间未知，待 relay 最终验证”，不显示成功。
- 等待第二签不持 vault 编辑/同步闸；失败不清库/换 pin/回退 creator。

## 安全负例

- 客户端与 relay 都验完整字节/域/payload/路径/I/V/rev/signers/cap/key/scope/PoP；
  改一字节/跨租户/给另一操作复用签名均失败。
- relay 最终验 cap 过期/撤销/ACTIVE/不同 device ID 且不同公钥/每人全部 scope/quorum/CAS；
  同设备两 key/两 cap、两 device 同 key 不凑数。
- 旧签配新 R/rev/payload 失败；已成功原包重放只返原回执；伪造待办/签名/完成状态本地拒绝；
  枚举/超额/撤权后读取/WS 负例。
- 时间显示用 relay 时间样本+本地单调；签名时间不低于已验证前链；重启重新估时。
- 物理独立/可信真实时间/恶意 relay 未见分叉不由本协议证明。

## 切片

- **S4d1**：V2 鉴权/控制回放/pin + cap 缓存选择 + ApprovalEnvelope/手工双签状态机；
  非 creator 单签邀请、双签 cap/revoke/approve + 目标 PoP；其他 kind 接 typed 构造/审核，
  未接不显示可用。
- **S4d2**：新表/API/HTTP 待办 + 可选通知、与 A 混用/重启对账；不改 Operation/门限；
  禁用 B 后 S4d1 仍完整可用。

## 非目标

账号/发现/P2P/可靠推送、中心化代签、门限降级、可信时间、撤回已发布签名；
kind1 另验。真实 TLS/Windows 两实例导入/审核/重启实测仍为 V2 发布门禁。

## S4d1a 实施记录（2026-11-14）

> 状态：**S4d1a 已实现**（Linux，dev `b32a5b2` + 本切片）。范围 = 客户端 V2 管理**基础**：
> 控制链回放、cap 缓存/选择、realm3 challenge/token 与 v2 管理请求鉴权封装。
> **未做**：`OperationV2` 构造/签名/提交、`ApprovalEnvelopeV1`、双签 UI/收件箱（S4d1b）；
> **未启用真实 vault**；真实 TLS/Windows 系统根/DPAPI 未验，不宣称生产可用。
>
> 代码：`crates/sync/src/management_v2.rs`（新模块；`crates/sync/src/lib.rs` 注册）。
> 测试：模块内 11 个单测 + `crates/sync/tests/management_v2.rs`（真实 dev relay 全链）。
> wire 对照与客户端验证清单见 [`docs/notes/management-cap-v2-wire.md`](../notes/management-cap-v2-wire.md) §8。

### 1. 控制链回放（`V2ControlReplay`）

- **分页**：`GET {v2}/vaults/{b64(V)}/control?after_rev=N`（realm1 设备 token；每页 ≤256，
  ≤4096 页/≤65536 条安全上限）。`control_rev` 跨页一致、`rev` 连续、链尾必须等于 head；
  空页未到 head/rev 不前进/超页一律 fail-stop。
- **链 hash**：每条提交复算 `SHA256(prev_hash || signed_record)` 并与 relay 的
  `prev_hash`/`record_hash` 逐字节比较（链首 prev=32×0x00）；不从 relay 摘要推导任何授权。
- **记录分类**：只有 `{0:bytes(OperationV2), 1:array(sig64)}` 形状才是 V2 记录，且必须
  严格 canonical（重新编码逐字节相同）。rev1 设备证书/`SignedControlRecord`/槽激活等 v1
  历史只参与链 hash，不产生 V2 授权事实；未知 V2 kind/`PolicyReplace`(1) fail-stop。
- **上下文/时间**：`OperationV2.instance_id/vault_id` 必须等于本地 pin；`canonical_path`
  必须等于 `canonical_submit_path(V)`；`expected_control_rev+1==rev`；
  `prev_control_hash==前链尾`；`signed_at≤now+30`、`not_after≤signed_at+300`、
  V2 链签名时间不倒退（**历史回放按签名时刻**验 cap/时间窗）。
- **门限/域**：signers 由 proto 强制升序去重；按角色验证
  control/upgrade/recover/pop 四个签名域；两签必须两个不同 ACTIVE device id + 不同公钥，
  **每位**具备操作所需全部 scope（不能拼 cap/拼人）；cap 有效性按"签名时刻"逐项验
  （未撤销/authority 当前/时间窗/主体 ACTIVE/公钥匹配）。
- **kind 覆盖**：0 Upgrade、2/3 CapGrant/Revoke、4/5/16 DeviceApprove/Revoke/Rights、
  6–10 rotation（playback）、11/12 Invite、13/14 Recovery。每条规则见 wire 文档 §8.2。
- **`ControlStateV2`**：head/rev、authz/authority rev、authz_state、policy、recovery_pub、
  caps（canonical/来源 rev/撤销 rev）、devices（证书/状态/grant）、invites、rotations；
  不持久化信任根。`recovery_only` 下除 kind 13 外的管理记录一律拒绝。
- **与 durable pin 对齐**：`sync.control_pin_v2`（canonical CDE v1）存
  `I/V/head/authz/authority` + 缓存摘要，CAS 只前进/同 rev 相等（同 rev 换 head 或
  authz/authority = `Equivocation`；低于 pin = `ControlRollback`；损坏 = `PinCorrupt`）。
  重启从 pin 水位续拉，首条增量记录的 `prev_hash` 必须等于 pin 的 head；无法证明历史
  前缀 → `NeedsTrust`（不降级成 unpinned）。legacy vault（无 V2 记录）不建立 V2 pin。

### 2. cap 缓存与选择

- **缓存**：`sync.management_v2_cache` 存 canonical cap、来源/撤销 rev、设备、邀请、轮换
  与验证水位；只有"摘要 == pin.state_digest 且 rev/head/authz/authority 与 pin 逐项相等"
  时才恢复，否则**丢弃并从 rev 0 重放**（缓存是可重建加速结构，不是信任根；损坏缓存不会
  被误信）。pin 损坏则是明确错误，不当作"没有 pin"。
- **选择**：`ControlStateV2::select_cap(device_id, device_pk, required, now)` 只选**一张**
  cap：本机 device/key、未撤销、`issuer_authority_rev==当前 authority`、
  `not_before≤now<expires_at`、主体 ACTIVE 且登记公钥匹配、**单张覆盖全部所需 scope**；
  绝不拼接多张 cap/多人的 scope。确定性排序：多余 scope 位更少 → 到期更晚 → cap_id。
  错误信息区分"没有 cap/已撤销/旧代/时间窗外/单张不覆盖"。
- **刷新时机**：`refresh_control` 在启动、签前/提交前（S4d1b 调用点）、authority/authz
  变化与失败后调用；cap 自然到期**不造记录**，按 `expires_at` 即时失效。

### 3. realm3 鉴权

- `POST {v2}/vaults/{b64(V)}/authz/challenges` 请求 `{0:3,1:subject16,2:cap_id}`；严格
  验证 `ChallengeV2`：instance/vault/realm/subject/cap_id 精确匹配、
  `origin_hash==SHA256(instance||vault)`、`expires_at>now` 且不超过本地允许窗口、
  `authz_rev/authority_rev` **等于**已验证回放值（链陈旧/跨代 fail-stop）。
- 签 `MN/v2/auth\0 || challenge_bytes` → `POST /authz/tokens`；token 响应再验
  realm/cap/到期/`authz_rev/authority_rev`，且 `token.expires_at ≤ cap.expires_at`。
- **每次管理请求前**先 `refresh_control` 再取 token；token 两个 rev 必须**等于**当前回放值
  （不是 `<`）、cap 仍有效且覆盖所需 scope；relay 的 `403 REVOKED` 触发清缓存重认证一次
  （`management_request` 封装；S4d1b 的构造/提交复用同一入口）。
- realm1 设备 token 只用于读控制链本身：challenge 的 authz/authority rev 不与"尚未回放"
  的本地状态比较（relay 每请求重验自己的 token revs）；realm3 必须绑定当前回放视图。
- 与 v1 隔离：legacy vault 不进入本状态机（`/authz` 返回 `INVALID_STATE` → 明确的
  "未启用 V2"）；v1 鉴权/控制链路径未改。

### 4. 限制（明确不假装验证过的部分）

- **PENDING 登记公钥绑定**：兑换邀请时提交的 `sign_pk/hpke_pk` 只在 relay DB、不在控制链
  上；客户端能验"目标 PoP 证明持有证书私钥"，但"证书 == 兑换时登记的公钥"只能由 relay
  保证。
- **Upgrade 的 `old_manager_rev`/ACTIVE 清单完整性**：由 pin 住的管理公钥对整条 Operation
  的签名兜底；逐字段等于 relay DB 属服务端事实。
- **EpochActivate 的 `member_cert_hash`/`grants_hash`/资产摘要**：v1 时代设备证书是
  `DeviceCertV1` 字节、不在 V2 链上，客户端无法重算；本切片只回放 `to_epoch` 与成员
  变化（授权影响），不把数据面摘要当授权依据，也**不执行**客户端数据切换。
- **rotation 重启的链外 `rotation_id`**：relay 在 `RotationRestart` 时生成随机
  rotation_id（不在签名 payload 里）；客户端只能按 `(from_epoch,to_epoch)` 唯一匹配一条
  `Restarted` 记录来绑定后续 `EpochActivate`；restart 之后的 abort/add-revokes 因无法
  绑定该 id 而 fail-stop（playback 限制，待 S4c/S4d2 客户端状态补充）。
- **begin 的 `from_epoch == relay.active_epoch`** 无法仅由链证明（epoch 不在控制记录里）：
  客户端只验证 `to_epoch>from_epoch` 与签名/门限，不改数据 epoch。

### 5. 验证（2026-11-14，Linux）

```bash
export CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0
cargo fmt --all
cargo clippy -p menghunote-sync-proto --all-targets -- -D warnings
cargo clippy -p menghunote-sync --all-targets -- -D warnings
cargo test -p menghunote-sync-proto              # 61 passed（基线）
cargo test -p menghunote-sync --all-targets      # 591 passed + 1 ignored（579+12）
cargo test -p menghunote-relay --all-targets     # 277 passed（基线）
cargo test -p menghunote-app --features dev-sync # 365 passed（基线）
```

新增用例：

- 模块内 11 个：legacy→Upgrade→CapGrant/Revoke→Invite→DeviceApprove/Revoke→
  RecoveryKeyChange/Reset 全链回放、分页 `apply_page`、错签名/断链/跳号/跨 vault/跨
  instance、缺管理锚（`NeedsTrust`）、单签/同设备两 cap/scope 拼接/旧 authz rev、
  cap 选择矩阵（覆盖/过期/撤销/错 device/错 key/旧 authority/legacy）、pin 前进与
  回滚、缓存往返/损坏、challenge 跨租户/旧 rev 负例。
- 集成 1 个（`crates/sync/tests/management_v2.rs`，真实 dev relay + 真实 HTTP + 真实
  Ed25519，无 sleep、临时目录）：用真实 relay 路由构造
  legacy→Upgrade→CapGrant/Revoke→InviteCreate/Cancel→DeviceApprove(C)+CapGrant(C)→
  DeviceRevoke(B)→RecoveryKeyChange→RecoveryReset 的控制链；客户端从 0 回放并与 relay
  投影对齐（authz/authority 用 `/authz` 的 `ChallengeV2` 交叉核对）；cap 选择/撤销/B 已
  撤销拒绝；realm3 正例（bearer 可用于真实鉴权请求）与错 cap/旧代/未知 cap/跨 vault/
  challenge 重放（`CHALLENGE_USED`）/token rev 前进失效负例；自然到期即时失效；
  重启从 pin+缓存恢复、损坏缓存从 0 重建、增量回放与 pin 前进。

### 6. S4d1b 依赖

在本次基础上还需要：`ApprovalEnvelopeV1`（共享不可变操作/签名槽/指纹）、typed
`OperationV2` 构造与双签状态机（含目标 PoP/旧 creator/恢复根）、`submit` 提交与回执对账、
cap 证据/收件箱（S4d2 schema 12→13）与 UI。**不得**绕过本文的回放/门限/选择规则；
提交仍是自证的（bearer 不代替管理签名）。

## S4d1b 实施记录（2026-11-14）

> 状态：**S4d1b 已实现**（Linux，dev `b32a5b2` + 本切片）。范围 = 库级手工双签通道（通道 A）：
> `ApprovalEnvelopeV1`、typed `OperationV2` 构造/签名、双签状态机、`/v2/control/submit` 提交与
> 回执对账、草稿持久化。**未做** App UI（S4d1c）、收件箱 relay 表/API（S4d2）、
> rotation/recovery 构造；**未启用真实 vault**；真实 TLS/Windows/DPAPI 未验，不宣称生产可用。
>
> 代码：`crates/sync/src/approval_v1.rs`（新模块）+ `crates/sync/src/management_v2.rs`
> （`submit_control_bytes`/`ControlReceiptV2`/`validate_pending_control[_at]`/签名校验模式）。
> 测试：模块内 6 个单测 + `crates/sync/tests/management_v2.rs` 的 `approval_s4d1b`（5 个真实
> dev relay 集成用例）。wire 对照见 [`docs/notes/management-cap-v2-wire.md`](../notes/management-cap-v2-wire.md) §9。

### 1. `ApprovalEnvelopeV1`（共享不可变操作 + 签名包）

- canonical CDE（整数键 0..6，字段严格齐全）：

  ```text
  {0: version=1, 1: op_bytes(CDE(OperationV2)), 2: h=SHA256(op_bytes), 3: initiator_index,
   4: [{0: signer_index, 1: sig64}...], 5: [cap_bytes(CDE(ManagementCapV2))...],
   6: {0: control_rev, 1: head_hash, 2: authz_rev, 3: authority_rev}}
  ```

- **严格编解码**：解码后重新编码必须逐字节相同；`h` 必须重算等于 `SHA256(op_bytes)`；
  签名条目按槽位**严格升序去重**、槽位合法、`sig64` 非全零（**缺签就是没有条目，不用零占位**）；
  cap 证据按 `cap_id` 升序、无重复、**恰好等于** role0 槽引用的 cap 集合（缺/多都拒绝），
  且与本地已验证 cap 逐字节一致；控制链证据必须与 `expected_control_rev/prev_control_hash`
  一致（Upgrade 特例：`authz_rev/authority_rev` 证据为升级前的 0）。
- **导入限长**：文件/字节 >1 MiB、base64 文本超上界、非 canonical CDE、未知字段/版本一律拒绝；
  解析**不联网、不执行、不自动签**。
- **文件/复制**：`.mn-approval` 文件 = canonical CDE（写临时文件后 rename）；复制文本 =
  `mn-approval-v1:<base64url>`（版本前缀，缺前缀/非法字符拒绝）。
- **冻结语义**：签名槽在构造时快照（role/device/cap/pk，顺序 = `signers` 顺序）；换 B/cap/
  payload 只能新建 `OperationV2`（`h` 变 → 旧签在导入/加签/提交时全部验签失败）。
- `Debug`/错误只含公开证据（op/签名/cap/证书/rev），不含私钥/root/seed/邀请 token。

### 2. typed `OperationV2` 构造与签名

- `ApprovalDraftV1::propose(replay, initiator, request, rng, now)` 支持：
  kind 0 UpgradePolicy（v1 链位置 + 旧 creator 根 + ≥2 台 ACTIVE 确认 + 新恢复钥 PoP +
  bootstrap caps/manifest）、2 CapGrant / 3 CapRevoke（两台各持 `CAP_ADMIN` 的 ACTIVE 设备，
  新 cap scope 由回放校验每位签发者的子集）、4 DeviceApprove（两台 `APPROVE` + 首次准入的
  目标 PoP）、5 DeviceRevoke（两台 `REVOKE`）、16 DeviceRightsChange（两台 `APPROVE`）、
  11/12 InviteCreate/Cancel（一台 `INVITE`）。
- **rev/时间窗自动填充**：`expected_control_rev=state.rev()`、`prev_control_hash=state.head_hash()`、
  `expected_authz_rev=state.authz_rev()`、`authority_rev=state.authority_rev()`、
  `signed_at=now`、`not_after=now+300`、`request_id` 由随机源生成（非零）、
  `canonical_path=` 唯一提交路径；Upgrade 例外（`expected_authz_rev=0/authority_rev=1`，
  链位置由调用方从**已验证 v1 控制链**给出——V2 回放不含 v1 前缀）。
- **cap 选择**：复用 `ControlStateV2::select_cap`（本机/对方 device/key、当前 authority、未撤销、
  时间有效、**单张覆盖全部所需 scope**；不拼 cap/拼人）。
- **构造期干跑**：用同一回放做一次不验签的 `validate_pending_control[_at]`（与链上回放同一条
  kind 门限/角色/scope/authority/payload 校验路径），错误在构造阶段就明确（错 target/grant/
  serial/manifest/TTL/scope 等）。
- **签名**：`sign_with(key)` 只签公钥恰好匹配**一个**冻结槽的钥；设备=`MN/v2/control\0||h`、
  旧 creator=`upgrade` 域、恢复根=`recover` 域、目标 PoP=`pop\0||h||pk`；**A 必须先签**
  （第一签必须是 initiator 槽），重复签同槽幂等。
- rotation/recovery（6–10/13/14）返回 `Unsupported`、`PolicyReplace`(kind 1) 返回 `Unsupported`
  （relay 仍 501）——不构造、不显示可用。

### 3. 双签状态机

`Draft → Frozen → InitiatorSigned → AwaitingCosign → Ready → Submitting → Confirmed/Failed`：

- `propose` 产出 `Draft`（槽已冻结）；`freeze` → `Frozen`；A 签 → `InitiatorSigned`；
  导出/交给对方 → `AwaitingCosign`；满签 → `Ready`；**只有 `Ready` 允许 submit**；
- `Failed` 记录分类 + 人读信息；`Confirmed` 返回原回执（重放幂等）；
- **换人/改 payload 作废**：旧签名绑定旧 `h`，导入/加签/提交都过不了逐槽验签；
  链前进/证据不符 → `Stale`（必须新建 Operation 全签，禁止自动改 rev/延 deadline）；
- 草稿可持久化到 vault 公开 meta（`sync.approval_draft_v1`：envelope + 状态 + 回执/失败），
  重启后用 `load_draft` 恢复；内容全是公开证据，**不写公开配置**（`dev-sync.json` 等）。

### 4. 提交与回执对账

- `ApprovalDraftV1::submit(&mut client)`：
  1. 前置：满签/结构/逐槽验签；未满签 → `NotReady`（**不联网**）；
  2. `refresh_control`（提交前刷新里已验证链/pin）；
  3. **已在链上**（用本地复算的 `SHA256(prev||signed_bytes)` 命中 `record_hash_at(expected+1)`）
     → 跳过时间窗/语义预检，**重发原字节**让 relay `dedup_v2` 返回原回执（丢响应/重复提交）；
  4. 未在链上 → 时间窗（`now > not_after` → `Expired`）、链位置/证据（不符 → `Stale`）、
     本地满签语义验证（同一回放路径）；
  5. `POST /v2/vaults/{lowerhex32}/control/submit` body `{0: bytes(SignedControlV2)}`
     （**自证，不带 bearer**）；
  6. 回执严格 6 字段并与本地操作交叉校验：rev=`expected+1`、kind/request_id 相等、
     `head_hash` 必须等于本地复算链 hash（不信任 relay 摘要）；
  7. 再次 `refresh_control` + 对账：本地已验证链必须在同一 rev 给出同一 hash，
     `authz/authority_rev` 相等（链已前进则不得低于回执）；
  8. 成功 → `Confirmed` 并存回执。
- **失败分类**（`ApprovalFailureV1`）：`Transport`（可重发同一字节）/`RateLimited`/`Server`、
  `Forbidden`(403)/`NotFound`(404)/`Stale`(409 CAS)/`Conflict`(409 其他)/`Expired`(410)/
  `Invalid`(400)、本地 `NotReady`/`Signature`/`Mismatch`。**不清库、不换信任锚**；
  pin 只会按已验证链前进（不因失败回退）。
- 管理器 bearer 不参与提交（ADR 0015 §1）；提交后 `refresh_control` 把新记录纳入回放并推进
  durable pin/缓存。

### 5. 限制（明确不假装验证过的部分）

- **Upgrade 的 v1 链位置/`old_manager_rev`/ACTIVE 清单**由调用方从已验证 v1 控制链提供，
  客户端无法从 V2 回放推导（升级前没有 V2 记录）；本地只能验"未升级 + 回放不比包更靠前 +
  包内证据自洽 + 旧 creator 签名锚 + 成员/清单内部一致"，逐字段等于 relay DB 属服务端事实
  （与 S4d1a §4 同一口径，relay 侧另有 CAS/签名兜底）。
- **grant 包的当前 epoch**客户端不可从链上推导（`ControlStateV2` 不含 active epoch）：
  kind 4/16 的 `grants` 由调用方按本地身份现有逻辑生成/传入，客户端校验
  recipient/hpke/authority/issued rev/grant_id 不复用（epoch 是否当前由 relay 校验）。
- **PENDING 登记公钥绑定**：客户端只验目标 PoP 持有证书私钥，"证书 == 兑换时登记公钥"
  仍只能由 relay 保证（S4d1a §4 不变）。
- 不做 UI/确认页/指纹展示（S4d1c）、不做收件箱/`approval_jobs`（S4d2）、不自动重签、
  不撤回已外发签名；`recipient` 设备离线时手工通道需另外传输 `.mn-approval` 文件。
- 真实 TLS/Windows 系统根/DPAPI 与真实 vault 仍未验（S0 门禁不变）。

### 6. 验证（2026-11-14，Linux）

```bash
export CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0
cargo fmt --all
cargo clippy -p menghunote-sync-proto --all-targets -- -D warnings
cargo clippy -p menghunote-sync --all-targets -- -D warnings
cargo test -p menghunote-sync-proto              # 61 passed
cargo test -p menghunote-sync --all-targets      # 602 passed + 1 ignored（591+1 基线 + 11）
cargo test -p menghunote-relay --all-targets     # 277 passed（基线）
cargo test -p menghunote-app --features dev-sync # 365 passed（基线）
```

新增用例（真实 dev relay + 真实 HTTP/Ed25519、临时目录、无 sleep；两个本地身份 A/B 模拟
手工双签，C 作 PENDING 目标）：

- 单签 InviteCreate/Cancel：A 构造→签→提交；B 持有满签包可提交；A 陈旧视图提交仍幂等
  返回原回执；拒绝手改 wire（`h`/未知字段/版本/路径/索引/零占位）。
- 双签 CapGrant/CapRevoke：A 签→导出 file/base64→B 导入验证+加签→提交；缺签 `NotReady`；
  错索引/换 payload 的旧签/陌生钥加签本地拒绝；单签+零占位 relay 400 `SIGNATURE_INVALID`；
  授权变化后新 cap 生效/撤销（`cap_is_effective_at=false`）；陈旧包 `Stale` + relay 409 `CAS_CONFLICT`。
- DeviceApprove（目标 PoP）/Revoke/RightsChange：错误 PoP relay 400；错 grant/错 serial 构造期拒绝；
  首次准入后 C 变 ACTIVE；改权 serial/权限真的变化；撤销 B 后 A/C quorum 保持。
- Upgrade（构造器路径）：旧 creator 根 + 2 ACTIVE + 新恢复钥 PoP → 首条 V2 记录、
  `authz/authority=1`、bootstrap caps/恢复钥生效；缺成员 manifest 构造期拒绝；
  陌生恢复钥不能签 PoP 槽；重复升级 `Stale`。
- 时间窗/CAS/跨 vault/路径篡改/导入限长/文件 base64 往返/重启草稿/丢响应重发：
  401→`Expired`（relay 410）与 `CAS_CONFLICT`（409）；跨 vault 导入拒绝；路径篡改 decode 拒绝；
  超限文件拒绝；草稿保存/恢复后从新 client 提交；`LossyTransport` 丢响应 → `Transport` →
  重发原字节 → `Confirmed`，控制链**只前进一条**（不重复写链）。

## S4d1c 实施记录（2026-11-18）

> 状态：**S4d1c 已实现但默认隐藏**（Linux，dev `e3830ec` + 本切片；个人用户不需要 V2，
> 只有 `MENGHUNOTE_EXPERIMENTAL_V2=1` 才在设置页显示）。范围 = App 侧**管理 UI**
> （设置页里的「管理（V2 控制面 / 通道 A 双签）」区）：状态概览、设备/成员只读列表、
> 审批详情/指纹、`.mn-approval` 导入导出、A 签署 / B 加签 / 满签提交 / 取消。
> 不改 relay / sync-proto / schema / wire；不新增依赖；**真实 vault 仍未启用 V2**；
> 真实 TLS/Windows 未验，不宣称生产可用。
>
> 代码：`crates/app/src/dev_sync/management.rs`（新模块：快照/执行体/worker/面板）、
> `crates/app/src/dev_sync/mod.rs`（注册/导出）、`crates/app/src/dev_sync/service.rs`
> （只读 `vault_path` 投影）、`crates/app/src/shell.rs`（面板创建/设置页管理区/关停）、
> `crates/app/src/onboarding/app_root.rs`（真实设置页入口）；
> `crates/sync/src/approval_v1.rs`（`comparison_fingerprint`/`verify_against`/
> `operation_summary` 公开客户端方法）、`crates/sync/src/management_v2.rs`
> （`relay_member_hints` 只读成员投影）。**没有 wire 变更**。

### 1. 入口与布局

- 唯一入口在设置页：`AppRoot` 的真实设置覆盖层新增「管理（V2 控制面 / 通道 A 双签）」
  区（默认收起；`settings-toggle-management` 展开，展开时对面板调一次 `activate`
  触发首次刷新）；AppShell 自己的设置页有同一区块（`settings-toggle-management`），
  便于 headless 测试与诊断。**不新建窗口**。
- 面板固定 selector：`mgmt-panel` / `mgmt-v2-status` / `mgmt-control` / `mgmt-policy` /
  `mgmt-pending` / `mgmt-cap-<i>` / `mgmt-device-<i>` / `mgmt-approval` /
  `mgmt-approval-state|kind|target|scopes|ids|path|revs` /
  `mgmt-approval-fingerprint-128` / `mgmt-toggle-fingerprint` /
  `mgmt-approval-fingerprint-256` / `mgmt-approval-slot-<i>` /
  `mgmt-approval-receipt|failure|invalid|expired` / `mgmt-sign` / `mgmt-submit` /
  `mgmt-cancel-approval` / `mgmt-path-input` / `mgmt-base64-input` /
  `mgmt-import-file|base64` / `mgmt-export-file` / `mgmt-copy-base64` /
  `mgmt-error` / `mgmt-notice`。

### 2. 状态概览与设备/成员

- `v2` 来自**已验证回放**（`ControlStateV2::is_v2`）。legacy vault 显示固定文案
  「当前 vault 未启用 V2（没有已回放的 UpgradePolicy）…」：不展示 cap/设备成功状态、
  不伪造待办、不把"未启用"当错误；管理动作全部只给明确拒绝原因。
- control head/rev、authz/authority rev、authz 状态、PolicyV2（门限/TTL）、cap 摘要
  （主体/scope/生效窗/撤销/此刻是否有效）全部来自已验证控制链。
- 设备列表：**ACTIVE/REVOKED 只信已验证控制链**；PENDING 来自 relay `/control`
  响应里的 `members` 投影（`RelayMemberHintV2`，明确标注"relay 投影、不可执行"）。
  relay 伪造该投影最多影响展示，不参与 cap 选择/门限/任何管理动作，也不写 pin/缓存。
- 待办审批数 = PENDING 设备数 + 本机未确认草稿数（通道 A 没有 relay 收件箱）。

### 3. 审批详情（双端比对）

- 详情全部由 `ApprovalEnvelopeV1`/`OperationV2` 的公开证据投影：kind、目标
  （`approval_v1::operation_summary` 统一解析，避免双端文案漂移）、cap scope、
  租户 vault/实例/唯一提交路径、CAS rev/前链尾/authz/authority、签名时刻与短窗截止、
  冻结签名槽（角色/设备/cap/公钥短指纹/已签）、发起者下标、回执/失败分类。
- `h` 的 **128 位比较指纹** = `SHA256(op_bytes)` 前 16 字节（32 hex，4×8 分组）；
  可展开完整 256 位（64 hex，8×8 分组）。这只是展示投影，校验仍用完整 `h`。
- 草稿失效：除 `Confirmed` 外的状态会重新走 `verify_against(本地已验证回放, now)`；
  链前进/证据缺口 → 显示「已失效：…（禁止自动改 rev/延 deadline；必须新建 Operation）」；
  `now > not_after` → 显示已超出短窗。UI 的签署/提交按钮在这两种情况下**本地拦截**，
  不会把命令发给执行体。

### 4. 导入/导出与双签动作

- 导入：文件（`.mn-approval`）或带 `mn-approval-v1:` 前缀的 base64url；复用
  `V2ManagementClient::import_approval_file/base64`（刷新已验证链 + 限长 1 MiB +
  `verify_import`）。解析本身不执行、不自动签；验证需要显式刷新，失败只显示摘要。
- 导出：文件走 `ApprovalDraftV1::write_file`（临时文件 + rename，标记已导交给对方）；
  base64 走 `to_base64` 并写剪贴板（面板不把完整文本渲染进布局）。
- 动作：`Sign` → `verify_against`（签前重验）→ `sign_with`（只签唯一匹配的未签槽；
  首签必须是发起者）；`Submit` → `submit`（满签前置、时间窗/链位置、自证提交、回执
  按本地复算链 hash 对账）；`Cancel` → 只清 vault 公开 meta 里的本地草稿（已外发签名
  不可撤回）。**没有任何自动签/自动改 rev/延 deadline/自动提交**。
- 失败：提交失败分类写回草稿并持久化；不清库、不换 pin、不回退 creator；可重发的
  传输/限流/5xx 在 UI 提示「可重发原字节」，其余提示重建 Operation。
- **等待第二签不持 vault 编辑/同步闸**：管理执行体是独立 worker 线程，从不调用
  `storage::session` 闸门，也不碰 `AppShell` 的同步状态机；A 签后编辑器仍可输入、
  自动/手动同步不受影响。
- 重启恢复：草稿/回执存在 vault 公开 meta（`sync.approval_draft_v1`，ADR 0016 原约定）；
  面板首次刷新时 `load_draft` 恢复，并重新走 `refresh_control` + `verify_against`
  重验 pin/控制链。丢响应重发仍由 `ApprovalDraftV1::submit` 的原字节路径幂等处理。

### 5. 秘密纪律

- 本模块**不打印日志**；错误统一经 `sanitize_management_text`：遮恢复码（`mnrec1…`）、
  URL（`http(s)://` / `mn-pair://`）、绝对路径（`/…/…`、`X:\…`）与 ≥64 字符长
  token/base64，再截断到 240 字符。UI 只显示摘要。
- 不直接读写 SQLite/密钥文件：身份由 `DevIdentityStore::load` 在 worker 线程解封，
  网络/签名只经 `V2ManagementClient`/`ApprovalDraftV1`；面板拿到的全是公开快照。

### 6. 限制（明确不假装验证过的部分）

- **不在 UI 里构造新 Operation**（kind 0/2/3/4/5/11/12/16 的 typed 构造仍在库层
  S4d1b）；管理页处理"本地草稿 / 导入包"，不提供新建表单。rotation/recovery 只读导入、
  不构造。
- relay 成员投影只用于 PENDING 展示；"兑换公钥绑定"等仍是 relay 事实（S4d1a §4）。
- headless 测试用脚本化执行体验证 UI 状态机与真实 `ApprovalDraftV1` 的导入/导出/
  Ed25519 签署/草稿持久化；**真实两设备端到端提交**仍由
  `crates/sync/tests/management_v2.rs`（真实 dev relay）覆盖，未在 App 层重复。
- 真实 TLS/Windows 系统根/DPAPI 与真实 vault 启用 V2 仍未验（S0 门禁不变）；
  S4d2 relay 收件箱（schema 13→14）不在本切片。

### 7. 验证（2026-11-18，Linux）

```bash
export CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0
cargo fmt --all -- --check
cargo clippy -p menghunote-sync -p menghunote-app --all-targets --features dev-sync -- -D warnings
cargo test -p menghunote-sync
cargo test -p menghunote-app --features dev-sync
cargo test -p menghunote-app
```

新增用例：`crates/app/tests/management_ui.rs`（8 个 `#[gpui::test]`）：
legacy 真实 relay/vault 的"未启用 V2"、真实 `AppRoot` 设置页入口、概览/设备/审批详情
与 128/256 位指纹、导入/导出（文件+base64）/A 签/B 加签/满签提交/重启恢复、
取消只清本地草稿、过期与失效本地拦截、错误脱敏、等待第二签不冻结编辑/同步。

## S4d2 实施记录（2026-11-18）

> 状态：**S4d2 已实现**（Linux，dev `e3830ec` + 本切片）。范围 = relay 侧**审批收件箱（通道 B）**：
> schema 14、5 个 `/v2/vaults/{V}/approvals*` 端点、事务内授权/验签/幂等/惰性终态。
> **未做**：客户端管理 UI（S4d1c）、真实 vault 启用 V2、真实 TLS/Windows 实机；
> relay 仍**不持**任何管理私钥、不代签、不改控制链/门限/Operation 语义。
>
> 代码：`crates/relay/src/approvals.rs`（新模块）+ `crates/relay/src/control_v2/mod.rs`
> （`SigPolicy` / `validate_partial`：半签包**同源干跑**）+ `crates/relay/src/db/schema.rs`
> （13→14）+ `crates/relay/src/api.rs`（路由/鉴权接线）。
> 测试：`crates/relay/tests/approvals.rs`（10 个真实 HTTP + 真实 Ed25519 用例）、
> `crates/relay/tests/migration_v14.rs`（4 个）、模块内 3 个单测。
> wire 对照见 [`docs/notes/management-cap-v2-wire.md`](../notes/management-cap-v2-wire.md) §11。

### 1. schema 14（13 已被 web admin 的 `vault_registry` 占用）

`SCHEMA_VERSION = 14`；13→14 是**纯新增**迁移（不改任何既有表/数据）：

- `approval_jobs`：PK `(vault_id, request_id)`、UNIQUE `(vault_id, op_digest)`；字段
  `op_bytes`（canonical `CDE(OperationV2)`，终态清空）/`op_digest`/`kind`/
  `initiator_device + initiator_cap + initiator_index`/`recipient_device`/
  `expected_control_rev + expected_head_hash + expected_authz_rev + authority_rev`/
  `signed_at + not_after`/`created_at`/`expires_at`/`status`/`seq`/`terminal_at`/`receipt_ref`。
  复合 FK 连 `devices(vault_id, device_id)` 与 `management_caps(vault_id, cap_id)`；
  所有键/索引都含 `vault_id`。
- `approval_signatures`：PK `(vault_id, request_id, signer_index)`、复合 FK 连
  `approval_jobs`；`sig64`（64B）/`received_at`。
- 索引：`(vault_id, recipient_device, status, seq)`（待办）、
  `(vault_id, initiator_device, status, seq)`（outgoing）、
  `(vault_id, status, expires_at)`、`(vault_id, terminal_at)`、
  `(vault_id, request_id, received_at)`。
- 迁移：文件库先 `VACUUM INTO <db>.schema13.bak`，再单事务 `CREATE TABLE IF NOT EXISTS`
  + 索引 + `PRAGMA foreign_key_check` + 写版本；中断整体回滚。版本推断按**表形状**：
  12 标记齐全才是 12，13 标记（`vault_registry` + `create_tickets.created_at`）齐全才是 13，
  14 标记（两张收件箱表）齐全才是 14；标记只出现一半 → fail closed（绝不猜）。

**回退口径**：不支持降 schema。`SCHEMA_VERSION=13` 的旧 binary 打开本库会命中
"schema is newer than this binary" 门禁并拒绝启动；要回到 13 只能恢复 `.schema13.bak`
完整快照，迁移后新增的收件箱数据与其它写入都会丢失。删除作业（ADR 0018 S3）的分批
purge 已同步加入两张表（子表 `approval_signatures` 先删，再删 `approval_jobs`）。

### 2. API（只在 v2 挂载；v1 wire 不变）

| 方法 | 路径 | 语义 |
| --- | --- | --- |
| POST | `/v2/vaults/{V}/approvals` | 上传 A 已签的 `ApprovalEnvelopeV1`（body `{0: bytes}`，≤1MiB）。首次 201、幂等重放 200 |
| GET | `.../approvals?role=&status=&after_seq=&limit=` | 自己的待办（默认 `role=recipient`、活状态），分页 ≤50 |
| GET | `.../approvals/{R}` | 摘要 + 完整 op + 已收签名 + cap 证据（终态只给摘要） |
| POST | `.../approvals/{R}/signatures` | B 加签（body `{0: bytes(sig64)}`），固定槽位 |
| POST | `.../approvals/{R}/cancel` | 取消协调（无 body；已外发签名不可撤回） |

响应字段、状态机与错误码见 wire 文档 §11。全部端点要求 realm3 bearer（管理设备 +
有效 cap），且**事务内**重新鉴权（撤权/轮换/授权 rev 变化立即生效，不存在"读完 token
再开事务"的授权窗口）。

### 3. 创建/加签的校验顺序

创建（`approvals::create`）：
1. 严格解码信封：canonical CDE（重编码逐字节相同）、`version=1`、`op_bytes` 必须是
   `OperationV2` 唯一 canonical 编码、`h=SHA256(op_bytes)`、签名项按槽位严格升序且
   非零/不越界、cap 证据按 `cap_id` 升序且**恰好等于** role0 槽引用的 cap 集合、
   control 证据与 `expected_*`/`prev_control_hash`/`authority_rev` 逐项一致
   （Upgrade 特例：证据 `authz=authority=0`）。
2. 幂等 `(V,R,h)`：同 R 同 h → 原 job（不重新授权、不改写状态）；同 R 异 h → 409。
   bearer 仍每请求重验 cap——A 的 cap 撤销/过期后 HTTP 层先拒绝，重放只能发生在
   bearer 仍有效期间（客户端必须自己保留原包，relay 不是可靠备份）。
3. kind 白名单：只接**两签 + 两个 Device 槽**的 kind（2/3/4/5/16、6–10、14）。
   单签邀请（11/12）、Upgrade（0，升级前无法建立 realm3）、RecoveryReset（13，无 Device 槽）、
   PolicyReplace（1，仍 501）不开放通用收件箱。
4. `initiator_index` 槽必须是 `Device`、`device_id/cap_id` **等于 realm3 主体/cap**；不符 → 404。
5. 签名布局：A 必须已签；**非 Device 槽（目标 PoP/恢复根/旧 creator）必须已签**
   （这些证明沿 A 收集）；至少留一个 Device 槽未签（满签包应直接 submit → 409）。
6. **同源干跑**：`control_v2::validate_partial` 复用 `/control/submit` 的校验路径
   （时间窗/链 CAS/kind 门限/payload/路径/I/V/rev/signers/每一个 Device 槽的
   cap 有效性/ACTIVE/公钥/scope 覆盖/不同设备不同公钥/旧 creator/恢复根/PoP 域验签），
   只跳过"尚未收到的槽位"的验签（`SigPolicy::Partial`）。干跑包在 `SAVEPOINT` 里，
   无论成败都回滚——`EpochActivate` 的候选 promote 等投影副作用**绝不由收件箱触发**。
7. 派生 `recipient` = 另一个 Device 槽；`expires_at = min(op.not_after, 每个 Device 槽
   cap 的 expires_at)`（以 relay DB 的有效 cap 为准，不用信封证据）。
8. 限额 → 分配 `seq` → 落库（job + 已收签名，同事务）。

加签（`add_signature`）顺序：可见性（统一 404）→ 惰性终结（EXECUTED/EXPIRED/STALE）→
固定槽位（realm3 主体/cap 必须就是该 Device 槽）→ 幂等（同字节返回原 job、异字节 409）→
域验签（`MN/v2/control\0||h`，Device 槽共用 op 摘要）→ 整包干跑（含新签名的完整集合）→
落库 + 状态 `PENDING/READY`。

### 4. 可见性与失败语义

- list/read/sign/cancel 只对**该 job 的 A 与预选 B**可见；A 用创建时的 `initiator_cap`，
  B 用 op 里冻结的槽位 cap（换 cap 不认）；任何不符统一 **404**（不提供"存在但不是你的"
  的 oracle）。
- 墓碑（正文已清空）下 B 的冻结 cap 已不可考：只要求"本设备持一张覆盖该 kind 基础 scope
  的当前有效 cap"；摘要里没有任何正文。
- 到期 → 410 `EXPIRED`；陈旧（head/authz/authority rev 变化）→ 409 `CAS_CONFLICT`
  （带当前 control rev）；撤权/轮换导致的 bearer 失效 → 403 `REVOKED`；
  终态取消 → 409 `INVALID_STATE`。

### 5. 生命周期与清理

`PENDING → READY → EXECUTED`（`dedup_v2` 里出现**逐字节相同**的满签包回执，用本地
op 字节 + 槽位签名重建 `SignedControlV2` 并比较 `request_hash`；relay 摘要不作为授权依据）；
`READY/PENDING → EXPIRED`（`expires_at`）、`→ STALE`（head/authz/authority rev 变化）、
`→ CANCELLED`（显式取消）。终态**立即清空** `op_bytes` 与签名（墓碑只留摘要：
request_id/op_digest/kind/设备/rev/时间/状态/回执 rev/终态时刻），24h 后由有界 sweep
（每次收件箱请求 ≤64 行）删除；正式 `control_commits`/`dedup_v2` 历史**永不删除**。

实现取舍：清理与业务操作**同事务**，失败请求整体回滚（sweep 也回滚），下次成功请求
再清一次；这只影响清理时机，不改变终结判定的正确性（始终按提交时刻的事实重算）。
HTTP 轮询是基线，收件箱**不发** WS 通知（丢通知不影响正确性）。

### 6. 限额/限流/不存什么

完整包 ≤1MiB、vault 待办 ≤64、同一源设备待办 ≤8、待办正文总字节 ≤16MiB、分页 ≤50；
超限 507 `QUOTA_EXCEEDED` / 413 `REQUEST_TOO_LARGE`。入口复用 v2 业务限流（实例级
固定窗口）；**没有**独立的 per-IP/per-设备/per-cap 限流器（见 §7 限制）。
不存笔记明文、root/seed/恢复私钥、邀请 token、私密备注；本模块不写业务日志。

### 7. 已知限制

- 客户端 UI/确认页见 S4d1c 实施记录；本切片 App 仍只走通道 A。
- 收件箱不接收 PENDING 目标 PoP/Upgrade/恢复根证明的"收集"过程：这些证明必须由 A
  连同包一起上传（非 Device 槽未签 → 400），relay 不代替它们收集。
- 未提供 WS 推送/通知；客户端按启动/重连/前台 ~30s 轮询（本切片只保证 HTTP）。
- 限流是实例级固定窗口，不声称 per-IP/per-设备公平性；上限是资源保护，不是授权。
- 墓碑回收依赖"下一次成功请求"；长期无人访问的库会保留墓碑行直到下次访问/其它维护。
- 删除作业（S3）会随 vault 删除清空收件箱（不迁移到别处）；不是"永久审计日志"。
- 信封类型有**两份实现**（客户端 `crates/sync/src/approval_v1.rs` 与 relay
  `crates/relay/src/approvals.rs`）：本切片不允许改 sync 客户端，因此没有把纯类型上移
  `sync-proto`；`docs/notes/management-cap-v2-wire.md` §9 是唯一 wire 契约，两边都有严格
  解码测试。后续可把 `ApprovalEnvelopeV1` 纯类型与严格解码收口到 `sync-proto`。
- 真实 TLS/Windows 系统根/DPAPI 与真实 vault 启用 V2 仍未验（S0 门禁不变）。

### 8. 验证（2026-11-18，Linux）

```bash
export CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0
cargo fmt --all -- --check
cargo clippy -p menghunote-relay -p menghunote-sync-proto --all-targets -- -D warnings
cargo test -p menghunote-relay        # 373 passed（356 基线 + 3 lib + 10 approvals + 4 migration）
cargo test -p menghunote-sync-proto   # 61 passed（基线，未改 wire 类型）
```

新增用例（真实 relay 路由 + 真实 Ed25519 + FakeClock，无 sleep）：

- `tests/approvals.rs`（10）：
  - 全链路：A 创建 → B list/read（完整 op/签名/cap 证据）→ B 加签 → 满签提交 →
    再读 `EXECUTED`（回执 rev 对账）→ 默认待办为空、墓碑可查；
  - 创建负例：无 bearer/ realm1/ B 的 cap 上传 A 包/跨租户 token；篡改签名、缺 A 签、
    满签包、非 canonical、单签 kind、非 Device 槽未签、>1MiB；head 前进 CAS、
    过期、B 槽 cap 不存在、同设备两 cap 凑数；幂等重放与同 R 异 h 409；
  - 可见性：非 A/B 的 ACTIVE 管理设备 read/sign → 404、列表为空；未知 id → 404；
    未知 query 键 → 400；`limit` 收敛 50；A/B 视角区分；
  - 加签：错钥/全零 → 400、A 重发自己签名 → 200 幂等、异字节 → 409、撤权后
    B 的旧 token → 403 `REVOKED`、A 读 → 惰性 `STALE`（正文清空）、终态取消 409；
  - 生命周期：+301s → `EXPIRED`（410 加签）、+24h 墓碑删除（行/签名都为 0）；
    取消 → `CANCELLED`、重复取消幂等、加签 409、满签包仍可 submit；
  - 限额/分页：`limit=2` + `after_seq`、同源设备 8 待办上限 507、取消释放配额。
- `tests/migration_v14.rs`（4）：13→14 纯新增（非收件箱 schema 对象与既有行逐字节
  不变）、`.schema13.bak` 快照可独立打开、中断回滚 + 重试、缺元数据的 13/14 形状推断、
  14 标记只出现一半 fail closed、更高版本门禁不改写数据。
- 模块内 3 个：信封篡改/非 canonical/版本/证据集合负例、收件箱 kind 白名单、状态往返。
