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
