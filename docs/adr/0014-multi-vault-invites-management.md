# ADR 0014 — 多 vault relay、邀请链接与设备管理能力

> 状态：**已决**（2026-11-14，GPT 决策；S0–S4 实施）。
> 关联：[ADR 0006](0006-sync-v1.md)（wire v1/控制链）、[ADR 0010](0010-device-access-revocation.md)（设备吊销）、
> [ADR 0013](0013-production-relay-pinning.md)（生产 relay/pin）。
> 本轮只定架构与顺序；真实 relay/Windows 仍后置。

## 前提校正

- relay 当前是 **schema 9、单 vault**：`vault.singleton=1` 是 PK；不是 wire v1 的多租户。
- 鉴权仍走全局 `/v1/auth/challenges|tokens`，`AuthContext` 缺租户字段；不能只改 `vault` 表。
- setup secret 是**一次性初始化凭证**，不是通用连接密码。
- 客户端已有 ProfileV2 多本地实例目录/配置；缺的是同 origin 的多 vault 创建/接入流程。
- C 的限制是**有没有独立管理私钥**，不是 creator 标签；恢复码持有者已可自授权；普通 Join 不保留管理钥。
- 当前“移除设备”只撤销诚实 relay 访问，不撤销管理材料；轮换保持同一管理钥，`auth_rev/manager_rev` 增长不等于旧私钥/恢复码失效。

## 推荐顺序

**真实生产基线 → B 的 v1 邀请 UX → A 多 vault → 创建链接/系统深链 → C 授权 v2。**

## A. 多 vault：单进程、单多租户 SQLite

- 选 **单 DB/WAL 多租户**：预算、控制链、幂等、创建消费保持原子事务，备份最简单。
- 每 vault 独立进程可作过渡部署/强隔离，不作为本阶段默认；不引入 ATTACH/目录多 SQLite。
- `vault_id` 是随机 128-bit **公开路由标识**，不是授权；secret 拆分为创建票据、邀请 token、恢复码，
  严禁“一个永久共享密码同时给数据和管理权”。
- DDL 草案（保留 STRICT/WAL，省略号为现有字段）：
  ```sql
  vaults(vault_id BLOB PRIMARY KEY CHECK(length(vault_id)=16), ...);
  devices(vault_id, device_id, ..., PRIMARY KEY(vault_id, device_id));
  device_grants(vault_id, epoch, device_id, ..., PRIMARY KEY(vault_id, epoch, device_id),
    FOREIGN KEY(vault_id, device_id) REFERENCES devices(vault_id, device_id));
  create_tickets(token_hash PRIMARY KEY, vault_id, expires_at, quota, consumed_at);
  ```
- 所有租户表 PK/UNIQUE/FK/查询/JOIN 加 vault 前缀：管理钥、控制链、epoch、设备/grant、邀请、auth、
  对象/parts/refs、日志、lease、幂等、GC、usage；引入显式 `VaultScope`，不得留 singleton 回退。
- 强制 `route.vault = auth.vault = signed.vault`；只有实例/运维元数据全局化；revision/序号/游标/配额租户内；
  加全局磁盘预算、有界批次、公平调度；不宣称单 DB 提供进程级隔离。
- 新增 `/v2/vaults/{id}/auth/{challenges,tokens}`；`/v2/vaults` 原子消费运维发行的随机 32B、哈希存储、
  绑定 vault/TTL/配额的一次性创建票据。
- 旧 `/v1/auth/...` 只绑定**显式 legacy 默认 vault**，不按 subject 猜租户；不开放匿名 vault 列表，
  未授权错误不泄露租户状态；入口先限流。

## B. 邀请链接，不是恢复码链接

- 选邀请 URL；公共配置 URL 只预填向导，不能授权；恢复码 URL **拒绝**。
  “一键”是打开正确向导/请求配对，不是无确认取得数据。
- 草案：
  ```text
  mn-pair://join?v=1&relay=<percent-encoded HTTPS>&vault=<id>&iid=<invite_id>&pin=<anchor>#token=<T>
  mn-pair://create?v=1&relay=<HTTPS>&vault=<reserved_id>#ticket=<T>   # A 完成后
  ```
  现有兑换接口需要 `invite_id`，不能只放 token。
- 默认兑换 TTL **15 分钟**（上限现有 24h）；原子一次性消费只产生 **PENDING**，仍须管理端比对
  双端设备指纹、签证书和 HPKE grant 才 ACTIVE。
- 保存同一待配对设备身份处理崩溃/丢回应；v1 重兑 `INVITE_USED` 不能当成功；v2 补同 keys 查询/幂等、
  pending 独立期限与清理；邀请 TTL ≠ pending 自动失效。
- 复用唯一 URL parser：remote 仅 HTTPS canonical origin，拒绝 userinfo/恶意跳转/静默换 pin；
  anchor 来自邀请方已保存的实例/管理根/控制头，仍需可信传递与指纹核验；TLS 不证明目标 vault 身份。
- fragment 减少 query/referrer 泄露，但不防进程参数、剪贴板历史、截图、二维码；完整 URL 脱敏、
  不进公开配置；清剪贴板须确认仍是本应用写入的值。
- 先做“复制整段 → 向导粘贴”，后做 Windows scheme/QR；复用实例目录，隔离身份/root/pin，
  明确目标，不覆盖/切换当前 vault。
- creator 离线则等获授权管理端；恢复码只作**已验证能恢复当前 epoch**的手工应急，不自动走恢复注册。

## C. 按设备能力授权，禁止群发管理钥

- **拒绝共享 `manager_seed`**：无法用设备撤销回收已复制私钥；也不能塞进冻结的 V1 grant。
- 选 `ManagementCapV2`：绑定 `vault_id、cap_id、subject 设备公钥、scopes、expiry、issuer_manager_rev、
  issued_authz_rev`，沿已验证授权链签发，默认不可自行转授；请求用各设备自己的签名钥。
- 控制链：`旧 v1 head → Upgrade/Policy → CapGrant/CapRevoke → DeviceApprove/DeviceRevoke → Rotation…`；
  保留历史 bytes，签名绑定 instance/vault、操作摘要、当前授权版本、`expected_control_rev`、`request_id`，
  原子 CAS。
- 所有设备可查看/申请；授权设备可独立做低风险邀请。ACTIVE 准入/数据 grant、角色授予、管理/恢复钥变更、
  大范围撤销、最后管理者移除，须两个不同授权公钥签同一摘要，或明确离线恢复授权。
- “每台都能独立无限管理”与“任一台失守不会获得全库管理权”不可兼得；若承诺后者，v2 必须关闭旧管理钥旁路、
  根管理权转离线恢复材料，并真正换新旧 creator 钥。
- 独立管理需新版授权验证、DeviceCert/Grant/Activation 与签名域；relay 接受 device token 不够，
  客户端仍须验证能力链及门限。恢复码仍是终极旁路。
- v1 降级：全设备查看/申请、管理端批准；必要时手输恢复码短时管理且不持久化；**不宣传**普通 Join 已有直接管理权。

## 版本与不可逆点

- A：DDL 9→10、租户 API v2，继续承载 MNE1/原控制 v1；B 首片只包现有 v1；C 后续 DDL 与授权/control v2，
  冻结新证书/grant/Activation/签名域及 golden fixtures。
- ProfileV2 可复用；改变公开/受保护格式分别显式升级；每 vault 自愿、单向启用 v2；旧客户端停止同步但保留本地，
  不静默忽略新控制记录或自动降级。
- 不可逆：已分发私钥无法收回；新授权链不能解释回 v1；新写入后旧备份恢复会丢；禁止重签历史/重置 pin/自动降 schema。

## 实施切片

- **S0 生产门禁**：真实 TLS relay + Windows 系统根/DPAPI，Create/Join/重启/离线/鉴权失败/pin 不符；
  分别验证恢复码重封装前后的恢复能力。设计与单测可先做，发布不能跳过。
- **S1 邀请 UX**：兑换竞态、TTL/重放、指纹错误、creator 离线、丢回应/重启、恶意 URL、日志脱敏；
  每个可见特性立即 Windows 验；仍由现有管理钥批准。
- **S2 多租户 relay**：两个 vault 故意使用相同局部 ID；逐项证明 auth/游标/对象/refs/grant/邀请/lease/幂等
  不串库；验配额、公平 writer、冻结/GC 隔离。
- **S3 多 vault 客户端 + 创建链接**：同 origin 两 vault 的目录/身份/pin 隔离；票据过期/双消费/断点恢复，
  不覆盖已有库；先粘贴验，再真机 scheme/QR。
- **S4 真实 C**：错误公钥/租户、过期/撤销能力、自升权、陈旧链、重放、单签危险操作全部拒绝；
  双签摘要不一致拒绝；旧客户端升级门禁、撤销后 token/WS 失效可复现。

## 迁移/回滚/非目标

- 维护窗口做 WAL 一致备份；事务迁移旧 vault，保留 ID/密文字节/签名链/rev/instance/log incarnation/游标钥/pin；
  校验行数/摘要/FK，失效旧 challenge/token，保留已消费邀请事实；验证中断/重试/备份恢复。
- 旧 binary 拒绝新 schema；回滚只恢复完整快照，新写入须先导出或明确接受丢失；pin 已前进时走显式恢复，绝不自动删 pin。
- 非目标：账号/SaaS 计费、细粒度文档协作权限、HA、动态 ATTACH、无限设备、自动恢复码入网；relay 仍只存密文。

## S1 实施记录（2026-11-14，邀请 UX / 单 vault、现有管理钥）

> 本片只做 **B 的 v1 邀请 UX**：relay 仍是 schema 9 单 vault、批准仍用现有管理钥。
> 真实鉴权 relay/Windows/TLS 系统根仍未验（S0 后置）；不宣称生产可用。

### 已实现

- **链接生成（创建端）**：`menghunote-sync` 新增 `invite_link` 模块：
  `create_invite_link(relay, vault, identity) -> InviteLink`。要求 `manager_seed`、
  `pinned_instance`，management token + `/state` 取 `control_rev`，复用
  `recovery_enroll::create_invite` 的管理签名/canonical body（仅增加可选 `expires_at`
  字段，默认 TTL **15 分钟**、上限 24h）；token 32B 随机、relay 只存 `SHA256(T)`、
  本地不持久化。URL：
  `mn-pair://join?v=1&relay=<percent-encoded canonical origin>&vault=<hex32>&iid=<hex32>&pin=<hex32>#token=<base64url>`。
- **唯一 URL 规则**：解析复用 `relay_policy::parse_relay_url`（`http` 只允许 loopback
  白名单、远端只允许 `https`）；严格 host=`join`、无额外 path、拒绝 userinfo/未知或重复
  query/版本≠1/非 32 位 hex/非 32B token；错误文案与 `Debug` 都不含 token/完整 URL。
- **加入端 PENDING + durable**：`InviteJoin::redeem`（strict parse → 新设备 keys/peer →
  现有 `redeem_invite`）只产生 `PENDING`；pending 材料（sign/HPKE 私钥、peer、invite_id、
  relay、vault、pin、阶段）经 `identity_wrap`（DPAPI/Linux 主钥）封装写进新实例
  自己的 `sync.invite_pending.v1`；**token 不落盘**；阶段
  `Redeemed → Approved → Bootstrapped`，清理只在 app 写完公开配置后的
  `InviteJoin::complete`。同一链接重贴走 **resume**（纯本地，不重复兑换；
  `INVITE_USED` 不会出现）。
- **批准（创建端）**：`approve_pending_device` 从已验证成员视图取 pending 设备的
  `sign_pk/hpke_pk`（pending 空证书行已在成员视图里），用现有管理钥签 §17.1 证书 +
  封当前 epoch 根的 §17.2 grant（复用 `recovery_enroll::approve_device`，改为收
  **公钥**）；`manager_rev` 与轮换同一口径（`dev_local_rotation::local_manager_rev`）。
  `Active` 目标幂等返回 `changed=false`，不清 pending、不重签。
- **批准后 bootstrap（加入端）**：`InviteJoin::check/finish` 用 pending 签名钥走设备
  认证（回执 vault/instance 必须与链接 **pin** 一致，`InstanceMismatch` 且**不自动换
  pin**）→ self grant 结构/收件人/epoch/管理链签名逐项校验 → pending HPKE 私钥解 root →
  `bootstrap::run_with_pins`（pin = 链接 instance）→ `verified_head` +
  `commit_verified_head` 提交 durable control pin → 构造 `LocalIdentityV1`
  （`manager_seed=None`）。待批准时 `check` 返回 `WaitingForApproval`，`finish` 不报错；
  失败保留 pending 可重试。
- **UI**：
  - 设备面板：`dev-sync-invite`（无管理钥/无活动文档/在途时置灰 + 原因）、
    `dev-sync-invite-link`/`-ttl`/`dev-sync-copy-invite`（显式复制、只存内存）；
    pending 行 `dev-sync-approve-<短id>`（**仅管理设备渲染**）、确认区
    `dev-sync-approve-warning`（目标 device_id + sign/hpke 短 hash）与
    `dev-sync-approve-confirm`/`-cancel`；worker 命令 `CreateInvite`/`ApproveDevice`；
  - 引导向导：Join 表单新增「邀请链接（可选）」字段（`app-root-invite-input`，遮罩、
    允许粘贴、提交后清空）；兑换成功后进入等待页（`app-root-invite-pending`，
    显示公开事实与双端可核对的短 hash）、`app-root-invite-check` 检查状态、
    `app-root-invite-back` 返回（保留 durable pending）；批准后 bootstrap → 写配置 →
    Main；「重试保存」只重写配置。
- **安全**：链接/`Debug`/事件/快照都不打印 token；URL 只在 fragment；错误全部脱敏；
  pending 记录加密且校验公钥/私钥一致；不把恢复码放进链接；不自动信任/换 pin。

### 未做（本片明确）

- 多 vault / 租户 API v2（S2）、`mn-pair://create` 创建链接与创建票据（S3）、
  Windows scheme/深链注册与 QR（S3）、ManagementCap v2/按能力授权（S4）；
- 自动轮询批准（只有用户点「检查状态」）；PENDING 独立期限/GC（v1 只记录展示 TTL）；
- 真实鉴权 relay + Windows DPAPI + TLS 系统根的真机验收（S0）；本片测试全部是
  localhost 放行验签的 dev relay。

### 验证（本机 Linux，2026-11-14）

- `cargo clippy -p menghunote-sync --all-targets -- -D warnings` ✅
- `cargo clippy -p menghunote-app --all-targets --features dev-sync -- -D warnings` ✅
- `cargo test -p menghunote-sync --all-targets`：**559 passed / 0 failed**
  （基线 546 + 新增 13：`invite_link` 单测 8 + `tests/invite_links.rs` 5）
- `cargo test -p menghunote-app --features dev-sync`：**357 passed / 0 failed**
  （基线 350 + 新增 7：`dev_sync_invite.rs` 4 + `onboarding_invite.rs` 3）
- `cargo test -p menghunote-app`（默认 feature）：**134 passed / 0 failed**
- `cargo test -p menghunote-storage`：**173 passed / 0 failed**
- 全部为 Linux 本机、进程内 dev relay（放行验签）；**非**真实鉴权 relay/Windows/TLS 验收。

## S2a 实施记录（2026-11-14，relay 存储层多租户基础）

> 本片只做 **A 的 relay 存储层**：schema 9 → 10、全部租户表/查询带 `vault_id`、显式
> `VaultScope`、旧单 vault 数据迁移。**不改 v1 wire/API 行为**：v1 路由继续服务它的
> **显式 legacy 默认 vault**；v2 auth/创建票据（`/v2/vaults`）留到 S2b。

### schema 10（`crates/relay/src/db/schema.rs`）

- `vault`（`singleton=1` PK）→ **`vaults(vault_id BLOB PRIMARY KEY)`**，保留全部原列
  （`instance_id/log_id/log_incarnation/cursor_key/state/active_epoch/next_epoch/
  manager_rev/control_rev/auth_rev/head_seq/gc_floor/normal_limit/maintenance_limit/
  created_at`），新增 `legacy_default INTEGER NOT NULL DEFAULT 0`，唯一部分索引
  `vaults_legacy_default ... WHERE legacy_default = 1`（v1 旧 API 的显式默认 vault 至多一个）。
- 全部 **22 张租户表**加 `vault_id`，PK/UNIQUE/FK/索引全部带 vault 前缀（外键一律复合）：
  `manager_keys`、`control_commits`、`epochs`、`devices`、`challenges`、`tokens`、`invites`、
  `maintenance_reservations`、`usage`、`objects`、`object_parts`、`log`、`checkpoints`、
  `checkpoint_refs`、`asset_objects`、`key_slots`、`device_grants`、`rotations`、
  `bootstrap_leases`、`device_acks`、`dedup`、`gc_plans`。
  索引示例：`devices_status(vault_id,status)`、`objects_state(vault_id,state,touched_at)`、
  `checkpoints_published(vault_id,published_at)`、`active_slot_kind(vault_id,kind) WHERE
  state='active'`、`gc_plans(vault_id,epoch,cut_seq)`。
- 只有 `relay_meta`（instance/运维元数据）与 `vaults`（租户注册表）是全局表；后者只供
  CLI/GC/legacy 解析，**没有** HTTP 匿名列表接口。
- `usage` 的旧 `singleton` 列删除，改为 `PRIMARY KEY(vault_id)`；`log` 的 `relay_seq`
  （原 rowid 别名）改为 `PRIMARY KEY(vault_id, relay_seq)`，序号在 vault 内从 1 开始。

### 迁移 9 → 10（维护窗口，单事务 + WAL 一致快照）

1. 文件库先 `VACUUM INTO <db>.schema9.bak`（已有快照则不覆盖；内存库跳过）；
2. `PRAGMA foreign_keys=OFF`，然后 `BEGIN IMMEDIATE`：逐表 `CREATE <t>__v10` → 复制并补
   `vault_id` → `DROP` 旧表 → `RENAME`；`vault` 单行转 `vaults` 一行并标
   `legacy_default=1`（保留 `vault_id/instance/log incarnation/cursor_key/全部 rev/
   head_seq/gc_floor/配额/created_at`）；
3. 每表校验 **行数 + 双向多重集一致**（`GROUP BY` + `EXCEPT`，BLOB 逐字节、NULL 视为相等）；
   `PRAGMA foreign_key_check` 必须为空；失败 → 整体回滚，旧库仍是 schema 9；
4. 旧 `challenges`/`tokens` 在迁移中**清空**（ADR §A 维护窗口：短命凭据，客户端重新认证）；
   `invites.used_at/pending_device` 事实保留；
5. 最后写 `schema_version=10` 并提交；`PRAGMA foreign_keys=ON`。
- **版本门禁**：`apply` 对 `version > 10` 明确返回 `relay schema is newer than this binary;
  refusing to open`，不写任何数据（旧 v9 binary 的 `Some(_) => Err` 分支同样拒绝 schema 10）。
- 元数据缺失（只有表、`schema_version` 丢了）时按**表形状**判断：有 `vault(singleton)` 且
  没有 `vaults` → 视为 v9 并迁移；否则才按最新版补齐。补齐时 `instance_id` 只补缺失、不覆盖
  （它是游标 `instance16`）。
- 重复迁移幂等：版本已是 10 时 `apply` 不做事；再打开一次数据/行数不变。

### `VaultScope` 与查询收口

- 新增 `crates/relay/src/scope.rs`：`VaultScope { vault_id }` + `ScopeConn<'a>`（连接 + 作用域）。
  `Db::read/write/write_blocking` 都**必须**带 scope，回调拿到 `&ScopeConn`；实例/运维元数据
  另走 `read_instance`/`write_instance_blocking`（或显式 `ScopeConn::raw()`）。
- 领域模块（vault/auth/log/quota/pairing/objects/checkpoints/rotations/slots/management/
  gc/revocation/control/api/main）所有租户 SQL 显式带 `vault_id`；参数约定：**vault_id 作为
  最后一个绑定参数**，SQL 末尾用匿名 `?`（SQLite 编号为 `n+1`），因此既有 `?1..?n` 无需重排。
- `vault::load/load_opt` 等改为按 scope 查；`vault::initialize` 只用于 v1 genesis（库内已有
  vault 即 `ALREADY_INITIALIZED`），多租户创建原语 `create_vault_in_scope` 供 S2b 创建票据
  在授权后调用（S2a 只被测试使用）。
- **v1 legacy 默认 vault 显式解析**：`vault::resolve_legacy_scope(conn, cfg)` =
  部署配置 `legacy_vault_id`（必须真实存在）或库里 `legacy_default=1`；两者都没有 → 404，
  绝不按 subject/“第一行”猜租户。`/v1/auth/challenges|tokens` 用它；
  `require_vault` 按路由 `vault_id` 建 scope 并断言路由=scope。
- v1 错误码保持不变：旧 token 有效但属于别的 vault 时，`auth::token_valid_in_other_vault`
  显式复查后仍回 **404**（旧行为），无效 token 仍是 401；不回显其它租户数据/ID。
- GC 改为**逐 vault**：`api::run_gc` 先列 `vaults`，每个 vault 各跑一个 `gc::tick` 写事务
  （单个 vault 失败不影响其它）；writer 仍是单进程单线程（不引入 ATTACH/每 vault 库）。

### 限制（本片明确）

- v2 `/v2/vaults` + 创建票据（随机 32B 哈希存储/TTL/配额/原子消费）未实现（S2b）；
  因此 HTTP 上仍**只能**创建 v1 legacy 默认 vault，第二租户只在测试里用
  `create_vault_in_scope` 构造。
- 全局磁盘预算/公平调度只有"逐 vault 预算 + 单 writer 串行"；没有跨租户的全局上限。
- 旧 challenge/token 在迁移时清空；旧客户端需重新认证（token 本来 600s 短命）。
- `relay_meta.setup.consumed` 仍是**实例级**一次性 setup secret；创建票据不复用它。

### 迁移运维步骤

1. 停 relay（维护窗口）；确认没有其它进程打开该 SQLite；
2. 备份：迁移会自动 `VACUUM INTO <db>.schema9.bak`；**先人工确认磁盘有余量**（≈库大小）；
3. 升级二进制后直接 `menghunote-relay status --db <path>` 触发迁移并核对
   `schema_version=10`、`vaults=1`、`vault_id/epoch/control_rev/head_seq/gc_floor` 与旧值一致；
4. 校验：`PRAGMA foreign_key_check` 为空；抽查 `devices/objects/object_parts/log` 行数与
   `usage` 字节数；备份文件应仍可独立打开为 schema 9；
5. 回滚：只能恢复 `<db>.schema9.bak` 完整快照（迁移后新写入会丢），**不允许**自动降 schema；
   pin 已前进时走显式恢复，不自动删 pin；
6. 重启 `serve`：v1 客户端用同一 vault_id 重新走 challenge/token（旧 token 已按 §A 失效）。

### 验证（本机 Linux，2026-11-14）

- `cargo clippy -p menghunote-relay --all-targets -- -D warnings` ✅
- `cargo test -p menghunote-relay --all-targets`：**223 passed / 0 failed**
- 新增迁移测试 `tests/migration_v10.rs`（5）：全量 v9 fixture → 10 逐项字节/rev/签名链/
  cursor/邀请事实断言、行数与多重集、FK、备份可恢复、半途失败回滚、重复迁移幂等、版本门禁、
  控制链可重算；
- 新增隔离测试 `tests/multivault_isolation.rs`（7）：同库两 vault 用**相同局部 ID**
  （device/object/log seq/invite/request_id/lease）逐项证明 auth/控制链/对象/parts/log/
  邀请/reservation/lease/幂等/配额/GC 不串库；
- 新增并发测试 `tests/multivault_concurrency.rs`（3）：两 vault 同时写 + A 的 GC 并发、
  配额 507 只影响本租户、同 object_id 并发互不覆盖；
- 旧 v1 回归：`cargo test -p menghunote-relay --all-targets` 全绿；`crates/sync` 依赖的
  进程内真实 relay（dev-relay）e2e 也全绿：
  `cargo test -p menghunote-sync --all-targets` = **559 passed / 0 failed（1 ignored）**；
  `cargo test -p menghunote-app --features dev-sync` = **357 passed / 0 failed**；
  `cargo test -p menghunote-storage` = **173 passed / 0 failed**。
- 本机 app 链接需要 `libxkbcommon(-x11)` 开发库；本次用既有 sysroot
  （`/tmp/ui-test-probe/sysroot`）通过 `LIBRARY_PATH` 链接，未改仓库配置。

## S2b 实施记录（2026-11-14，v2 创建票据 + 租户鉴权）

> 本片做 **A 的 relay HTTP 层**：schema 10 → 11（新增 `create_tickets`）、运维发行/消费一次性
> 创建票据、`POST /v2/vaults`、`/v2/vaults/{id}/auth/{challenges,tokens}` 与 v2 入口限流。
> **v1 wire/行为完全不变**：`/v1/vaults` 仍只认 setup secret、`/v1/auth/*` 仍只绑定显式 legacy
> 默认 vault；真实鉴权 relay/Windows/TLS 系统根仍未验（S0 后置）。

### schema 11（`crates/relay/src/db/schema.rs`）

```sql
create_tickets(
  token_hash  BLOB PRIMARY KEY CHECK(length(token_hash) = 32),
  vault_id    BLOB NOT NULL CHECK(length(vault_id) = 16),
  expires_at  INTEGER NOT NULL,
  quota       INTEGER NOT NULL,
  consumed_at INTEGER
) STRICT;
CREATE INDEX create_tickets_vault ON create_tickets(vault_id);
```

- **全局表**（不是租户表）：票据发行时目标 `vaults` 行还不存在，不能做 `vault_id` 外键；
  `token_hash = SHA256(32B 明文)` 主键，明文只在发行时返回一次，**DB/日志都没有**。
- 迁移 **10 → 11**：单事务 `CREATE TABLE/INDEX IF NOT EXISTS` + 写 `schema_version=11`；
  **不改任何已有租户表/数据**，重复执行幂等；旧 binary 打开 schema 11 落到既有版本门禁
  （`relay schema is newer than this binary; refusing to open`），不自动降 schema。
- 顺带修正：v9→v10 迁移现在**只写 `schema_version=10`**（原先写 `SCHEMA_VERSION` 常量，
  升到 11 后会让 v9 旧库被误标成最新版而跳过 10→11；`tests/migration_v11.rs` 锁死这一点）。
- 元数据丢失但 `vaults` 形状仍在的库：补齐 `create_tickets`（幂等）后再标最新版。

### 运维票据 API（不是公开 HTTP）

`crates/relay/src/admin.rs`：

- `issue_create_ticket(db, vault_id, ttl: Duration, quota: u32) -> [u8; 32]`：
  32B CSPRNG；TTL 默认 15min、上限 24h；`quota` = 该票据允许的创建次数（1..=16，一次性票据=1）；
  只落 `SHA256(明文)`；用 `Db` 的时钟（生产 `SystemClock` / 测试 `FakeClock`）。
- `consume_create_ticket(&ScopeConn, now, token)`：**只**在调用方写事务里用；校验
  未过期/未消费/额度/`vault_id` 一致 → CAS 扣 1 次额度，额度归零时写 `consumed_at`。
  所有失败（不存在/过期/已消费/额度用完/错 vault/长度不对）统一 **403 `FORBIDDEN`**，
  不区分原因、不泄露票据或租户是否存在。
- `load_create_ticket(db, token) -> Option<CreateTicketRow>`：运维/测试诊断（不含明文）。
- `dev_server --issue-ticket [--vault <hex>] [--ticket-ttl <s>] [--ticket-quota <n>]`：
  打开库（触发迁移）→ 发行一张票据 → 打印 vault_id/明文/TTL/额度后退出；不启服务器，
  明文只打印这一次。请求日志仍只打 `method path -> status`。

### `POST /v2/vaults`（CBOR/CDE，v2 新端点）

请求字段：`0=ticket(32B 明文)`，`1=vault_id`，`2=device_id`，`3=sign_pk`，`4=hpke_pk`，
`5=manager_pub`，`6=certificate(CDE DeviceCertV1)`，`7=certificate_signature`。
**没有 setup secret**：授权来自票据；响应与 v1 create **同字段**
（`instance/log/epoch/control_rev/auth_rev/cursor`，状态 201）。

事务（一个 `BEGIN IMMEDIATE`）：

1. `admin::consume_create_ticket`（统一 403 的票据校验 + 额度 CAS）；
2. `vault::create_vault_with_ticket`（`GenesisAuth::CreateTicket`，不检查/不消费 setup secret，
   证书/签名校验与 v1 genesis 完全相同，`legacy_default = 0`；已存在 → 409 `ALREADY_INITIALIZED`，
   绝不覆盖）；
3. 同一事务提交，票据与 vault 原子生效。任何失败整体回滚：**不建库、不消费票据**；并发双消费
   只有一个成功（`tests/multivault_v2.rs` 用 `tokio::join!` 证明恰好一个 201、一个 403）。

票据 `quota > 1` 的语义：每次**成功创建**消费 1 次；额度未归零时 `consumed_at` 仍为 NULL，但同一
`vault_id` 已存在，第二次创建会被 409 拒绝并回滚额度（失败不消费）。不开放匿名 vault 列表。

### `/v2` 租户鉴权

- 新增 `POST /v2/vaults/{id}/auth/challenges`、`POST /v2/vaults/{id}/auth/tokens`：
  路由 `vault_id` 解析为显式 `VaultScope`，复用现有 `auth::create_challenge/issue_token`
  （全部 SQL 带 `vault_id`）；未知 vault → 404；subject 必须存在于该 vault（设备 realm 的
  `devices.vault_id` / 管理 realm 的该 vault 管理公钥）。
- `AuthContext` 新增 `vault: VaultScope`（鉴权发生的作用域）。v2 路由统一走
  `api::authenticate_v2`：在 v1 语义（跨 vault token → 404）之上再断言
  **`route.vault == auth.vault`**；签名校验的 `SignatureCheck.vault_id` 来自该 scope 的 vault 行，
  因此 `route = auth = signed` 三者强制一致。S2b 同时新增只读 `GET /v2/vaults/{id}/state`
  （字段与 v1 相同）作为该绑定的可测入口；其它 v2 业务路由留到 S3/S4。
- 跨 vault 拒绝：token 跨 vault → **404**（不泄露目标租户）；challenge 在别的 vault 的 tokens
  端点 → **401 `CHALLENGE_INVALID`**（该 scope 查不到该 challenge）。
- v1 `/v1/auth/*` 继续用 `resolve_legacy_scope`（部署配置或 `vaults.legacy_default=1`），
  **不按 subject 猜租户**；v1 设备 challenge 对"只存在于别的 vault 的设备"仍 403 `REVOKED`。

### 限流（`crates/relay/src/ratelimit.rs`）

- 固定窗口限流（`std::time::Instant` + `Mutex`，不引新依赖），**实例级**有界：
  v2 create 默认 6 次/分钟、v2 auth 默认 60 次/分钟；超限 → **429 `RATE_LIMITED`**
  （`retryable = true`，错误体与其它协议错误同构）。
- 只挂在 v2 create/challenges/tokens 的入口；**v1 完全不受影响**（测试用 1 次/分钟阈值证明
  v2 第二次 429 且 v1 连续挑战仍 200）。
- 部署可配：`RelayConfig.v2_create_limit` / `v2_auth_limit`（`None` 关闭；
  `with_v2_rate_limits(...)` / `without_v2_rate_limits()`）。
- 局限：全局而非 per-IP/per-vault；只做"有界"的最低要求，不声明 DDoS 防护。

### 迁移/回滚（10 → 11）

1. 停 relay（维护窗口可选：本迁移只新增一张全局空表，不重写租户数据，但仍建议单实例操作）；
2. 备份：沿用运维快照（`<db>.schema9.bak` 只在 9→10 留；10→11 前可再 `VACUUM INTO` 一份）；
3. 升级二进制后 `menghunote-relay status --db <path>` 触发迁移并核对 `schema_version=11`；
   `PRAGMA foreign_key_check` 为空；`create_tickets` 为空表；
4. 回滚：旧 binary 打开 schema 11 会明确拒绝（不自动降 schema）；只能恢复升级前完整快照，
   或先删掉 `create_tickets` 并把 `schema_version` 改回 10（仅限确认没有新票据时的人工恢复）。
   **不允许**自动降 schema；pin 已前进时走显式恢复，不自动删 pin。

### 验证（本机 Linux，2026-11-14）

- `cargo fmt --all` ✅；`cargo clippy -p menghunote-relay --all-targets -- -D warnings` ✅
- `cargo test -p menghunote-relay --all-targets`：**240 passed / 0 failed**
  （基线 223 + 新增 17：`admin` 3、`ratelimit` 3、`tests/multivault_v2.rs` 7、
  `tests/migration_v11.rs` 4）
- 新增 `tests/multivault_v2.rs`（7）：票据建库/单用/明文不落库、失败不建库不消费、
  quota 与失败回滚、并发双消费恰好一个成功、两 vault v2 auth 隔离与 v1 legacy 不串、
  v2 限流 429 且 v1 不受影响、票据不消费 setup secret / 不产生 legacy 默认；
- 新增 `tests/migration_v11.rs`（4）：v9→10→11 逐字节数据不变 + 幂等、fresh schema 11 表形状、
  元数据丢失修复、旧/新版本门禁；
- `cargo test -p menghunote-sync --all-targets`：**559 passed / 0 failed（1 ignored）**；
  `cargo test -p menghunote-app --features dev-sync`：**357 passed / 0 failed**
  （该两 crate 不受影响；app 链接沿用既有 sysroot）。
- 全部为 Linux 本机、进程内 dev relay/localhost；**非**真实鉴权 relay/Windows/TLS 验收；
  未打生产 tag。

### 未做（本片明确）

- 客户端多 vault（同 origin 两 vault 的目录/身份/pin 隔离）与 `mn-pair://create` 创建链接、
  Windows scheme/QR（S3）；`/v2` 的 control/对象/日志等业务路由（**S2c 已提前补齐**，见下）；
  ManagementCap v2（S4）；创建票据的运维 CLI 子命令（仅 `dev_server --issue-ticket`）；
  全局磁盘预算/公平调度（沿用 S2a 的逐 vault 预算 + 单 writer）。

## S2c 实施记录（2026-11-14，`/v2` 数据面业务路由全量镜像）

> 本片只做 **A 的 v2 业务路由**：`/v2/vaults/{id}/...` 与 v1 路由逐条镜像，全部走
> `authenticate_v2`（`route = auth = signed`）。**迁移无**（仅 API）；v1 wire/行为/错误码不变；
> 客户端仍只用 v1（S3 才切换）。真实鉴权 relay/Windows/TLS 系统根仍未验（S0 后置）。
> S2b 里"其它 v2 业务路由留到 S3/S4"的表述由本片提前落地：服务端路由与客户端切换解耦，
> S3 只需把客户端 base path 从 `/v1` 换到 `/v2`（并让管理签名覆盖新 path）。

### 实现方式（业务实现只写一份）

- `api::tenant_routes(prefix)` 把**同一批 handler** 分别注册到 `/v1/vaults/{vault}/...` 与
  `/v2/vaults/{vault}/...`；差异只由自定义 extractor `ApiFace` 决定。
- `ApiFace` 从 axum 已匹配的**真实请求路径**（`OriginalUri`）推导，**不由 token/subject 推断**；
  路径同时是管理签名绑定的事实（`management::run` 的 canonical 输入），因此 v2 客户端签
  `/v2/...` 路径天然绑定到 v2 面。
- 两面只在两处不同：鉴权严格度（v1 旧语义 / v2 强制 `route.vault == auth.vault`）与
  入口限流（只有 v2 业务路由受限）。**没有**复制任何 handler 业务逻辑。

### v2 路由清单（35 条路由 / 36 个 method+path = 15 读（含 WS）+ 20 写 + 1 公开）

| 类别 | 路由（`/v2/vaults/{vault}` 前缀） |
| --- | --- |
| 读 | `GET /state`、`GET /control?after_rev`、`GET /log?after&limit`、`GET /objects/{object}`、`GET /objects/{object}/parts/{n}`、`GET /objects?type=asset`、`GET /checkpoints/{cid}`、`GET /key-slots`、`GET /devices/self/grants/latest`、`GET /rotations/current?request_id`、`GET /rotations/{r}`、`GET /rotations/{r}/grants`、`GET /rotations/{r}/grants/leaves`、`GET /rotations/{r}/assets` |
| 读（WS） | `GET /wake`（WebSocket，水位 JSON 与 v1 相同） |
| 写 | `POST /acks`、`POST /updates`、`PUT /objects/{object}/parts/{n}`、`POST /objects/{object}/complete`、`POST /checkpoints`、`POST /checkpoints/{cid}/refs`、`POST /checkpoints/{cid}/publish`、`POST /bootstrap`、`PATCH /bootstrap-leases/{lease}`、`POST /invites`、`POST /devices/{d}/approve`、`POST /devices/{d}/revoke`、`PUT /key-slots/{slot}`、`POST /key-slots/{kind}/activate`、`POST /rotations`、`POST /rotations/{r}/restart`、`POST /rotations/{r}/revoke-more`、`PUT /rotations/{r}/grants/{d}`、`POST /rotations/{r}/activate`、`POST /rotations/{r}/abort` |
| 公开（无 token） | `POST /invites/{invite}/redeem`（授权来自 invite token；v2 形态仍计入业务入口限流） |

S2b 已有的 `/v2/vaults`（创建票据）、`/v2/vaults/{id}/auth/{challenges,tokens}`、`/state` 不变；
其中 `/state` 现在与其它业务路由同一实现、同样计业务限流。**没有** health 路由可选镜像。

### 鉴权 / scope

- 每个 v2 业务 handler 都按面走 `authenticate_face(.., ApiFace::V2)` → `authenticate_v2`：
  v1 语义（token 查询一律带 `vault_id`、跨 vault token → 404）+ 断言
  `auth.vault == route.vault`；`SignatureCheck.vault_id` 取自该 scope 的 vault 行，因此
  **route = auth = signed** 三者一致。
- 设备/管理 realm 门禁各有 face 变体（`require_device_face` / `require_management_face`）；
  管理写仍走 `management::run`（唯一 canonical body + 管理签名 + `request_id` 幂等 +
  `control_rev` CAS），签名的 path 是该请求的真实路径（v2 客户端必须签 `/v2/...`）。
- 未知 vault 与"token 属于别的 vault"的错误体一致：坏 token → 401、跨 vault → 404，
  与路由目标是否存在无关（不泄露租户存在性）。
- `/v2/.../wake`：升级前做 v2 同一鉴权（跨 vault → 404）；每轮重鉴权也断言 v2 绑定。
  另外把 wake 的事件订阅移到首帧推送**之前**，消除"查首帧水位"与"订阅"之间丢 notify 的窗口
  （测试不再依赖时序；该改动只可能多收事件，不改变 v1 语义）。
- **v1 逐字节保留**：同一 handler 在 `ApiFace::V1` 下完全走旧路径；`/v1/.../wake` 升级仍沿用
  历史的"不做跨 vault 复查"路径（跨 vault token → 401，与 S2b 前一致）。

### 限流（只 v2）

- 三面入口限流：create **6/分钟**、auth **60/分钟**（S2b）+ 业务 **3000/分钟**（本片新增，
  `RelayConfig.v2_data_limit` / `DEFAULT_V2_DATA_LIMIT`）。超限 → **429 `RATE_LIMITED`**
  （`retryable = true`），错误体与其它协议错误同构、不含 vault/token 信息。
- 业务面既有读轮询（`/state`、`/log`）也有分片上传（大资产 = 多个 part），阈值取
  "有界但不误伤"的 50 req/s；大规模初始同步应由部署方显式调高或关闭。
- 仍是**实例级固定窗口**（不声称 per-IP/per-vault 公平性）；v1 从不调用，测试证明
  v2 429 后 v1 连续请求仍 200。

### 隔离（相同局部 ID 不串库）

- v2 只是把 route 解析成显式 `VaultScope`；所有 SQL 仍由 `ScopeConn` 带 `vault_id`。
- 同一局部 ID 交叉验证：`device_id` / `object_id` / log `relay_seq` / invite token +
  `request_id` / `grant_id` / `checkpoint_id` / rotation `request_id` 在两个 vault 各自成立：
  两 vault 用同一 `object_id` 各自 PUT/GET/complete，读回各自信封；两边 log 都从 1 开始但
  游标（HMAC key 按 vault）与事件字节不同；同 `request_id` 的邀请/begin 在两边各自成功，
  幂等回放只命中本 vault；B 的 checkpoint 在 C 里 404；C 用 B 的 invite 兑换 → 409
  `INVITE_USED`（与"不存在"同一错误码，不泄露）。
- token 授权边界是 **vault scope**，不是 URL 版本：v1 为 legacy 默认 vault A 签发的 token
  对 A 的 `/v2` 路由同样有效（同一租户、同一 realm，无权限提升）；区分 token audience 需要
  DDL/迁移，本片明确不做（"迁移无"）。真正的安全属性"任何 token 都不能到达别的 vault"
  已逐项验证（v1/v2 两个方向）。

### 未实现 / 客户端 S3 依赖（本片明确）

- 客户端仍只用 `/v1`（S3 才切 v2）；S3 需要按面生成管理签名的 path（`/v2/...`），
  否则 `management::run` 验签失败——这是 API 契约，不是兼容问题。
- 多 vault 客户端（同 origin 两 vault 目录/身份/pin 隔离）、`mn-pair://create`、Windows
  scheme/QR（S3）；ManagementCap v2（S4）；匿名 vault 列表（**永不做**）。
- 本片**没有**未实现的 v2 路由（含 WebSocket `/wake`）；剩余缺口是部署侧：真实鉴权 relay +
  Windows DPAPI + TLS 系统根的真机验收（S0），以及实例级限流的公平性（见上）。

### 验证（本机 Linux，2026-11-14）

- `cargo fmt --all` ✅；
  `cargo clippy -p menghunote-relay --all-targets -- -D warnings` ✅
- `cargo test -p menghunote-relay --all-targets`：**246 passed / 0 failed**
  （基线 240 + 新增 6：`tests/multivault_v2_data.rs`）
- 新增测试覆盖：两 vault 各自全流程（control/log/对象分片/updates/checkpoint 建+refs+发布/
  bootstrap+lease/ack/key-slots/invite+redeem+approve/latest grant/rotation begin→current→
  view→grants→assets→abort/devices revoke 含幂等 `changed=false`）；相同局部 ID 不串；
  v2 路由清单的 realm 门禁（逐条 403 而非 404）；业务限流 429 且 v1 不受影响；
  未授权错误不泄露；`/v2/.../wake` 真实监听端口 + 跨 vault token 拒绝。
  新增用例连续跑 10 次无偶发（全部以明确响应/帧为收敛点，无 sleep）。
- `cargo test -p menghunote-sync --all-targets`：**559 passed / 0 failed（1 ignored）**；
  `cargo test -p menghunote-app --features dev-sync`：**357 passed / 0 failed**
  （两 crate 不受影响；app 链接沿用既有 sysroot）。

## S3a 实施记录（2026-11-14，客户端 v2 前缀 + 票据创建 + 创建链接）

> 本片只做**库级** v1/v2 API 面切换、一次性票据创建与 `mn-pair://create` 解析/编码；
> App 的创建表单/多实例面板、Windows scheme/QR 是 **S3b**，本片不开放 UI。
> 真实鉴权 relay/Windows/TLS 系统根仍未验（S0 后置）；测试全部用放行验签的 dev relay。

### API 面（`api_version`）收口

- 新增 `menghunote-sync::api_version`：
  - `RelayApiVersion::{V1,V2}`（公开配置可序列化；数字 1/2）；
  - `RelayApi { origin, version }`：`create_vault_path` / `vault_path` / `object_path` /
    `auth_path` / `revoke_device_path` / `url` 是**唯一**业务路径生成点。
- `RelayContext` 新增 `api_version`（缺省恒 **V1**；`with_api_version(V2)` 只由票据创建/
  新配置显式开启），并提供 `api()`；`local_dev`/`remote_tls`/`from_mode` 行为不变。
- `HttpRelayConfig` 新增 `api_version`：`HttpRelayClient` 的 state/control/log/objects/
  parts/updates/acks/checkpoints/refs/bootstrap/lease/key-slots/rotations/invites/
  approve/revoke/devices/wake **全部**由同一 `RelayApi` 生成；v1 输出与旧字符串逐字节相同。
- **v2 鉴权是租户内路径**：`/v2/vaults/{id}/auth/{challenges,tokens}`（不是全局
  `/v2/auth/*`）；v1 仍是全局 `/v1/auth/*`。
- **管理签名域**：v1/v2 的所有管理写都先用同一个路径变量构造请求与
  `management_request_cde(method,path,body)`；v2 请求签 `/v2/...`，用 v1 path 复算必须
  验签失败（测试逐字节断言）。控制链历史记录没有独立 path 字段，客户端按本 vault
  **固定的面**重建；v1→v2 中途切换会让旧记录验签失败（fail-closed，不支持混用）。
- 初始化 pending（`sync.init_pending`）新增**可选**键 8 `api_version`：v1 pending 编码
  逐字节不变（旧库/旧二进制兼容），只有 v2 创建才写；缺省/未知数字分别按 v1/明确错误。
  v2 创建崩溃后启动对账继续走 v2（`context_of` 从 pending 恢复面）。
- ProfileV2（`InstanceEntry`）新增 `api_version: 1|2`（serde default=1）：旧配置显式按
  v1 打开；新配置写 2；未知数字 `ProfileError::Corrupt`（不静默降级）。
  App 的 `ServiceConfig`/`DevLocalRunner` 透传该面（`relay_context()` 统一应用）；
  现有 Create/Join/Invite onboarding 流程仍是 v1（未开放 v2 表单）。

### 一次性票据创建（库级）

- `menghunote-sync::create_link`：
  - `CreateTicket`：32B 明文，只在内存；`Debug` 脱敏、`Drop` zeroize、常量时间比较、
    不可 Clone；不提供任何持久化 API。
  - `CreateLink { mode, relay, vault_id, ticket, pin?, url }`；URL：
    `mn-pair://create?v=1&relay=<percent-encoded canonical origin>&vault=<hex32>[&pin=<hex32>]#ticket=<base64url>`。
  - 严格解析：host 必须精确 `create`（同时拒绝 userinfo/端口/额外 path）、query 只允许
    `v/relay/vault/pin`（拒绝重复/未知字段）、`v=1`、`vault/pin` 必须 32 位 hex、
    fragment 只允许 `ticket=`（百分号编码，严格解码 32B base64url）；relay 复用唯一
    `parse_relay_url`（远端只 `https` canonical origin、`http` 只 loopback 白名单）。
    解析后按 canonical 组件重建 URL（不保留原始文本）；错误/`Debug` 不回显 ticket/URL。
  - `CreateLink::relay_context()` 固定返回 **v2** context（票据端点就是 `/v2/vaults`）。
- `remote_init::create_vault_with_ticket(context, vault_id, ticket, expected_instance,
  vault_path, label, persist_identity) -> CreatedVault`：
  - 只接受 v2 context；复用 `create_remote` 的同一主体（`create_with_auth`），**只**替换
    创建路由/请求字段（`POST /v2/vaults`：键 0=ticket，1..7 与 v1 genesis 同序），
    身份/pin/本地库/初始恢复槽/收尾全部同一套代码；
  - 票据不写 pending/配置/日志；`expected_instance = link.pin` 时创建回执的
    `instance_id` 必须相等，否则 `InstanceMismatch`（**不自动换 pin**），并按回执前
    失败语义清理本次新建库；
  - 403/409/429 分类由 relay 错误体给出：`FORBIDDEN`（不存在/过期/已消费/额度/错
    vault，统一不泄露）/`ALREADY_INITIALIZED`（同 vault 重复创建，事务回滚不扣额度）/
    `RATE_LIMITED`（`retryable=true`）；失败不建本地库、不消费票据（随后同票仍可用）。
  - `remote_init::CreatedVault` 是 v1/v2 共同返回类型（`RemoteCreatedVault` 为兼容别名）。

### 测试（本机 Linux，2026-11-14）

- `cargo fmt --all` ✅；
  `cargo clippy -p menghunote-sync --all-targets -- -D warnings` ✅；
  `cargo clippy -p menghunote-app --all-targets --features dev-sync -- -D warnings` ✅
- `cargo test -p menghunote-sync --all-targets`：**577 passed / 0 failed（1 ignored）**
  （基线 559+1 ignored；新增 18：`api_version` 3、`create_link` 6、`init_journal` 1、
  `tests/multivault_create.rs` 8）
- `cargo test -p menghunote-relay --all-targets`：**246 passed**（未改 relay wire）
- `cargo test -p menghunote-app --features dev-sync`：**359 passed**（基线 357 + 2：
  profile `api_version` 迁移/往返、`ServiceConfig` 面选择）
- `cargo test -p menghunote-storage`：**173 passed**
- 新增 `tests/multivault_create.rs`（8）：
  1. 同 dev relay 两张票各建 vault：身份/vault/设备独立、rev1 强 pin 各自 durable、
     同一 object_id + 同一 message_id/sender_seq 在两边各自成立（B 读不到 A，A 不变）；
  2. 错 vault → 403 且不消费票据（同票随后正确创建成功）；已用票 → 403；过期 → 403；
     quota>1 同 vault 重复 → 409；均不建本地库；
  3. v2 创建入口限流 → 429 `RATE_LIMITED`（retryable），不建库；
  4. 链接 `pin` 锚：同实例成功、错实例 `InstanceMismatch` 且不换 pin/不留库；
  5. v1/v2 `HttpRelayClient` 路径逐条断言（v2 无任何 `/v1/` 回落；含租户内 auth）；
  6. `sign_activate` 的 v2 path 可验、v1 path 复算失败；
  7. v2 吊销：实际请求 path 与管理签名 path 一致（v1 path 复算失败）；
  8. v2 管理写（`begin rotation`）经过真实 HTTP 后，控制链里的 typed 记录仍能被 v2
     客户端重新验签（证明历史记录 path 重建与请求 path 是同一个 `/v2/...`）。
- 新增 `create_link` 单测 6（往返、fragment 百分号编码、严格负例、Debug/错误脱敏、
  远端 https/canonical 化、context 固定 v2）与 `api_version` 单测 3（路径/v1 字节不变）。
- 全部为 Linux 本机、进程内 dev relay（放行验签）；**非**真实鉴权 relay/Windows/TLS 验收。

### 未做 / S3b 依赖（本片明确）

- App UI：`mn-pair://create` 表单/粘贴入口、多实例面板、Windows scheme 注册/QR/剪贴板；
- v2 profile 的 onboarding 创建流程（当前 v2 只由库级票据入口产生 profile 字段）；
- 自动发行票据（必须运维 `dev_server --issue-ticket`）；真实鉴权 relay/Windows DPAPI/
  TLS 系统根真机验收（S0 后置）；
- 同一 vault 的 v1→v2 迁移/混用（明确不支持；面在创建时固定）。

## S3b 实施记录（2026-11-14，App 创建链接/多实例 + v2 邀请链接）

> 本片把 S3a 的库级能力接到 App：Create 表单的「粘贴创建链接」（`mn-pair://create`
> + 一次性票据）、同一 relay 多实例、v2 邀请链接（`api=2`）与设置页 API 面展示。
> **只做粘贴**：Windows scheme/深链注册、QR、剪贴板监听仍不做；真实鉴权 relay /
> Windows DPAPI / TLS 系统根的真机验收仍未做（S0 后置）。测试全部是 Linux 本机 +
> 进程内 dev relay（放行验签 hook）。

### 1) Create 链接 / 票据（App）

- **入口**：Welcome 增加 `app-root-create-link`「粘贴创建链接（v2 票据）」→ Create
  表单；表单顶部新增创建链接字段（`app-root-create-link-input`，遮罩 + 禁复制 + 允许
  粘贴）与 `app-root-create-link-apply`（Enter 同路径）。应用后：
  - 严格解析 `CreateLink::parse`（S3a 的 host/query/hex/fragment/relay 规则），
    **立刻清空**输入框并丢弃完整 URL（fragment 里的票据只进内存状态）；
  - 预填 canonical relay / 模式 / vault id；显示 `app-root-create-link-warning`
    （一次性/默认 15min/不写配置日志/失败不消费票据）与 `app-root-create-link-facts`
    （relay + vault_id + API 面 v2）；
  - **隐藏 setup secret 字段**（票据模式不需要）；实例目录仍默认
    `instances/<自动 id>/`；保留「同意开发同步」与（remote_tls）远程确认；
  - `app-root-create-link-clear` 回到 v1 setup secret 模式（不联网）。
- **提交**：`CreateCredential`（新增枚举）与 v1 **类型级互斥**——同一个
  `CreateRequest` 只能带 `SetupSecret(Zeroizing<String>)` 或
  `Ticket { vault_id, CreateTicket, expected_instance }`。票据路径固定
  `.with_api_version(V2)` 调 `remote_init::create_vault_with_ticket`；成功后走
  **同一张**备份页/身份/配置路径，`CreateOutcome.api_version` 透传为 2，配置写
  `"api_version": 2`、`relay_mode` 与链接一致。
- **失败分类**（`ticket_failure_hint`）：403 `FORBIDDEN`（不存在/过期/已用/错 vault，
  relay 统一不区分）、409 `ALREADY_INITIALIZED`（同一 vault 重复，事务回滚不扣额度）、
  429 `RATE_LIMITED`、`InstanceMismatch`（链接 pin ≠ 回执 instance，**不自动换 pin**）。
  失败不建本地库/身份/配置；pre-receipt 失败由 `remote_init` 清本次库文件；票据不写
  pending/配置/日志，UI 里也随提交移除（可重贴同一链接重试）。
- **配置写失败**沿用 `RetryKind::SaveProfile`：只重写配置，不重跑票据创建。

### 2) 多实例（同一 relay 第二个 vault）

- 同一 `AppRoot` 连续用两条创建链接建两个 vault：`DevSyncProfile::upsert_active`
  **追加**第二个 profile（不删除/覆盖第一个），active 指向新实例；
- 实例目录/受保护身份/**每库各自的 durable `sync.control_pin`** 独立（同一 relay 的
  `instance_id` 相同是预期，pin 内容分别提交在各自库）；`api_version=2` 分别写入；
- Welcome 列表显示 `v… · vault 短值 · relay`，可分别打开（打开另一个 = 关当前
  `AppShell` + 取目标目录锁，**不热切换**）；重启按 active 自动进 Main；
- 设置页每行显示 `API v1/v2 · 模式 · vault_id/device_id 短值`，active 实例另有
  `settings-api-version` 与既有 `settings-fingerprint`（relay instance / record
  rev+hash / bootstrap 锚，全部公开只读）。

### 3) v2 邀请链接（`api=<version>`）

- `crates/sync/src/invite_link.rs`：
  - 链接 query 新增**可选** `api`（只接受 1/2）；缺省 = **v1**（v1 链接逐字节不变），
    v2 vault 生成时追加 `&api=2`；未知值 `InviteLinkError::ApiVersion` 本地拒绝；
  - `InviteLinkTarget/InvitePending` 持久/携带 `api_version`；pending CDE 新增键 15，
    旧记录缺失按 v1 解读、未知数字明确拒绝（不静默降级）；
  - `InviteJoin::redeem/check/finish` 都断言 **context 面 == 链接/pending 面**
    （`ConfigInvalid`，fail-closed）；因此 v2 邀请的兑换/设备认证/bootstrap/
    self-grant/管理签名全部走 `/v2/...`（S3a 的路径生成点唯一）。
- App 加入端：`InviteRedeemRequest/InviteCheckRequest` 携带 `api_version`，
  worker `context_for` 统一 `.with_api_version(...)`；`InvitePendingFacts`/`InviteOutcome`
  透传，`InviteOutcome.public_facts` 写 `api_version=2`。
- v1 链接（无 `api`）与既有 `onboarding_invite.rs` 流程完全不变；未知 `api` 拒绝。

### 4) 验证（本机 Linux，2026-11-14）

- `cargo fmt --all` ✅；
  `cargo clippy -p menghunote-app --all-targets --features dev-sync -- -D warnings` ✅；
  `cargo clippy -p menghunote-sync --all-targets -- -D warnings` ✅
- `cargo test -p menghunote-app --features dev-sync`：**365 passed / 0 failed**
  （基线 359 + 新增 6：`tests/onboarding_create_link.rs`）
- `cargo test -p menghunote-app`：**134 passed / 0 failed**
- `cargo test -p menghunote-sync --all-targets`：**579 passed / 0 failed（1 ignored）**
  （基线 577+1 ignored；新增 +1 单测 `api_field_marks_v2_links_and_defaults_to_v1`
  与 +1 集成 `v2_invite_link_joins_through_v2_plane_and_rejects_api_mismatch`）
- `cargo test -p menghunote-relay --all-targets`：**246 passed / 0 failed**（未改 relay wire）
- `cargo test -p menghunote-storage`：**173 passed / 0 failed**
- 新增 `crates/app/tests/onboarding_create_link.rs`（6）：
  1. 同 relay 两票据 vault：两个 profile/api_version=2/身份/pin/目录独立、两库各自建笔记
     并发布基线（真实 sync）、互不可见、重启分别进 Main、Welcome 多实例列表可分别打开；
  2. v2 邀请：`api=2` 链接 → PENDING（不写配置/身份）→ A 批准 → B Main
     （配置 api_version=2、manager_seed=None）→ B 用 `/v2` 面 `sync_once` 拉到 A 的新笔记；
  3. 失败矩阵：未知票据 403、错 vault 403（不消费票据，同票随后可成功）、过期 403、
     已用 403、同 vault 重复（quota=2）409、v2 创建限流第 7 次 429；全部不建库/身份/配置；
  4. 票据不落配置/实例目录/错误文案（扫描配置 JSON 与实例目录全部文件字节）；
  5. （Unix）配置写失败 → Error「重试保存」→ Main，vault_id 不变（不重跑创建）；
  6. 设置页显示 API 面 + durable pin 指纹（关闭不写配置）。
- 全部为 Linux 本机、进程内 dev relay（**放行验签**）；**非**真实鉴权 relay/Windows/
  TLS 验收，不打生产 tag。

### 5) 未做 / 后续依赖

- Windows scheme 注册/深链打开、QR、剪贴板监听（S3 的真机部分）；App 不生成
  `mn-pair://create` 链接、不自动发行票据（仍由运维 `dev_server --issue-ticket`）；
- 多实例只有一个进程内 relay/身份封装；不做跨进程多 vault；
- v1→v2 中途切换仍不支持（面在创建时固定）；
- ManagementCap v2 / 按能力授权（S4）；真实鉴权 relay + Windows DPAPI + TLS 系统根
  的真机验收（S0）。
