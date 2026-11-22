# ADR 0018 — 内嵌 Web 管理、vault registry 与在线不可逆删除

> 状态：**已决**（2026-11-16，GPT 评审修订后；S1–S4 实施）。
> 关联：[ADR 0014](0014-multi-vault-invites-management.md)（多 vault/票据）、[ADR 0017](0017-ui-ticket-provisioning.md)。
> 方向：同进程双监听 Web 管理；在线删除采用 fenced 作业，不停 relay。

## 前提纠正

- 现有 `delete-vault` 删除整个 DB/WAL/SHM，不是单租户；`status` 调用 `Db::open` 会触发迁移，
  不是真正只读。
- 票据消费与 genesis 已同事务；发行时钟在进 writer 队列前采样，新设计要在事务执行时生成。
- 全局单 writer + 独立读连接池；HTTP 取消不会取消已入队 job，故“HTTP 已取消”不等于“不会写入”。
- WS 只在**出站水位**重验，查询与 `socket.send()` 分离，删除存在竞态。
- 客户端 401/404 当前进入 `NeedsAttention`（暂停自动重试），不是 `SecurityPaused`。
- schema 12 有 **25 张租户表**；带 `vault_id` 不等于 FK 到 `vaults`，不能机械倒序删除。

## 决策

### 1) Web 管理

- 与数据面**同一 relay 进程**，独立 admin listener；默认 `127.0.0.1:18081`，可配置 LAN。
- 不强制 TLS；文档要求不映射公网，跨不可信网络建议反代 TLS；HTTP 下明确承认 token/session
  可被截获。
- 初始化生成独立 **admin token**（32B，与 setup secret / 租户 bearer / ManagementCap 分离），
  只打印一次、存 SHA256、常量时间比较、日志脱敏；登录表单粘贴 token。
- 会话：随机 session id，HttpOnly/SameSite=Strict、host-only 独立 cookie；CSRF token；
  闲置/绝对过期；logout/换 token 全吊销；限制 body/尝试次数/会话数。
- 精确 Host/Origin 白名单，禁默认 CORS，防 DNS rebinding；admin 与数据 listener 不共享 Cookie。
- UI 仅：登录、vault 列表、创建/取消 claim 预留、详情、不可逆删除；CSP、转义、无第三方资产、
  no-store/no-referrer；不把 token/完整领取链接写入日志/浏览器持久存储。
- 页面不展示票据明文；链接只在创建响应中出现一次。

### 2) 权威 vault registry

- 新增 `vault_registry`（权威）：`vault_id`、状态、created/deleted、job_id、进度、错误码；
  终态墓碑只留最小管理元数据，无租户内容/密钥/凭据。
- 状态机：
  - `RESERVED → ACTIVE | EXPIRED | CANCELLED`
  - `ACTIVE → DELETING → DELETED`；失败 `DELETE_FAILED`，只能继续删除
  - 运行状态（initializing/rotating）与授权状态（legacy/active/recovery_only）独立
- CSPRNG 128-bit ID 在 registry（含墓碑）唯一；终态 ID 禁止复用。
- 领取：原子完成 registry CAS、票据消费、genesis、ACTIVE 转换、其它兄弟票据失效；
  取消输给领取则冲突，不能悄悄删除已创建 vault。
- 票据：TTL 900s、额度 0/1、created_at/expires_at 在 writer 事务内生成；只存 token hash，
  不能重印原链接；丢失就取消预留并重发。
- schema 12→13：保留现有 vault/control/instance/cursor；存量票据迁移或集中作废并提示重领；
  不得虚构 created_at；迁移/版本提交/FK 检查有失败恢复。
- **版本推断修复**：不得用 `authz_state` 存在性推断缺失版本；必须明确识别 12/13，
  旧 12 库不得被误标 13 跳过迁移。

### 3) 在线删除（fenced 作业）

1. admin 确认 `DELETE <id>`；
2. 同 writer 事务持久化 `DELETING`、作废票据/临时认证；返回 **202**（只在持久标记提交后）；
3. 入口 admission 关闭：所有读写、匿名 genesis、鉴权 challenge/token、控制/恢复、GC、dedup replay、
   WS upgrade/发送、v1 路径都在 scoped 事务开始执行时**再次检查持久 registry**；
4. cancel/drain：排队 DB job、请求上传、响应 body、socket 生命周期；慢发送/Close 限时可强制终止；
5. 分批 purge（不可逆），有字节/时间/WAL 预算；其它租户读写不被长事务阻塞；GC 跳过 fenced；
6. 最终原子终态 `DELETED`；25 张租户表 + 该 vault 全部票据零残留；FK 明确拓扑
   （`vaults.upgrade_control_rev → control_commits` 不能机械倒序）；不关闭 FK；
7. 失败 `DELETE_FAILED`，同一 job 可重试；启动在监听前加载 fence、恢复幂等作业；
   `DELETING/DELETE_FAILED` 永不重开 active；状态缺失/矛盾/结果不确定 fail closed。
- “同库单服务进程”需有实例 OS 锁/启动证明；仅 SQLite 写锁不能防第二个 relay 绕过 admission。
- 删除是逻辑清除，不是取证擦除；WAL/空闲页/备份/客户端副本仍可能留数据。
- 客户端保留本地内容/身份/pin；服务器拒绝/不可用保持 `NeedsAttention`，禁止自动 genesis/清 pin/重建。

## 4) 移除的旧入口

- 删除 `/v2/admin/create-tickets` 路由/handler/限流器/测试。
- 删除 App「管理员创建」、admin secret/worker、`sync::admin_ticket` 及路径/导出。
- 删除主 CLI `delete-vault` 与开发示例 `--issue-ticket`（参数/实现/帮助/脚本）；
  `admin` 子命令不新增。
- `serve` 保留；`init` 仅首次实例/管理凭据引导（一次性 stdout 例外）；
  `status` 改为真正只读、不迁移、不创建库的实例诊断。
- Legacy v1 创建 UI 不显示/不维护（未发布）。

## 5) 实施切片

- **S1**：admin 配置/凭据、双监听共享 Db、空 relay 启动、浏览器认证；封旧管理入口。
- **S2**：schema 13、registry/票据、版本推断、迁移、原子 claim/cancel。
- **S3**：全路径 fence、生命周期、WS 取消、可恢复分批 purge、崩溃恢复。
- **S4**：最小 Web UI、删除旧 CLI/App/SDK/测试/文档；App 默认粘贴 claim/邀请。

## 6) 验收矩阵

- 权限：双 listener、Host/Origin/DNS rebinding、CSRF、Cookie 跨端口、logout/rotation/过期、限流、XSS、
  日志/存储无秘密。
- 并发：鉴权后入队、取消后 job、旧读快照/响应 body、WS upgrade/发送/慢 Close、GC/dedup/recovery、
  claim-vs-cancel/delete、重复删除。
- 崩溃：删除标记前后、每批、最终提交、202 丢失；重启先 fence 后监听；失败不可访问；25 表/票据零残留。
- 迁移：旧 12、缺版本元数据、重复/过期票据、FK/注入失败；大 vault 删除时其它租户 p95、writer 队列、
  WAL 预算；barrier/failpoint，不用 sleep。
- App：只 Link/邀请；claim 成功/过期/取消；删除后 NeedsAttention 不清 pin；回归现有 369/619/290。

## 非目标

停 relay 删除、外部 CLI/独立 SQLite 连接代替 fence、可恢复软删、ID 复用、公网管理、多角色、
批量删除、取证擦除保证；Web 不生成 E2EE 邀请。

## S1 实施记录（2026-11-16，relay 内嵌 Web 管理骨架）

范围：admin 配置/凭据、双监听共享 `Db`、空库可登录的浏览器认证。
**未做**：schema 13 / `vault_registry` / 票据迁移（S2），fence 与在线删除（S3），
vault 列表/创建/删除 UI 与旧入口移除（S4）；`/v2/admin/create-tickets`、App「管理员创建」、
主 CLI `delete-vault` 等旧入口**保留**。

### 配置与凭据

- `ServeConfig` 新增（严格 TOML，未知字段仍拒绝）：
  - `admin_listen`：缺省 `127.0.0.1:18081`；`"off"`/`"none"`/`"disabled"` 才显式关闭；
    可改 LAN 地址（不套用数据面的 `allow_plaintext` 门禁，但启动时打印明文风险警告）。
  - `admin_allowed_hosts`：追加精确 `host:port` 条目（LAN/域名/反代访问时必填）。
  - `admin_token_sha256`：64 hex，可选；**库内 `relay_meta` 哈希优先**，配置只兜底。
- `RelayConfig` 对应字段：`admin_listen: Option<String>`（`None` = 关闭）、
  `admin_allowed_hosts`、`admin_token_hash`；默认值 `DEFAULT_ADMIN_LISTEN = "127.0.0.1:18081"`。
- `init`：
  - 生成**独立**的 32B CSPRNG admin token（base64url，43 字符），只输出一次到 stdout
    （`admin_token=` 与 `admin_token_sha256=`），**不写任何日志**；
  - 落 `SHA256(token 文本)` 到现有 `relay_meta` 键 `admin.token_hash`（`relay_meta(key,value)`
    已存在，**不是 schema 变更**；`SCHEMA_VERSION` 仍为 12，12→13 留给 S2）；
  - setup secret 与 admin token 是两枚独立随机值/两个键；两枚哈希在**同一个写事务**落库，
    CSPRNG 失败不会留下半初始化库；
  - 已有 admin 哈希时拒绝（不覆盖；`--rotate-admin-token` 后置到后续切片）。
- `serve`：`resolve_token_hash` = 库内优先（配置冲突只打警告）；无哈希时 admin listener
  照常启动但登录一律 401 并提示先 `init`，**不自动生成/打印**。

### 双监听与启动顺序

- 同一进程、同一个 `Db`，但 admin 使用**独立 `TcpListener`、独立 router、独立状态**
  （`admin_web::AdminWeb`：凭证/会话/限流/Host 白名单），不与数据面 `AppState` 共享路由。
- 顺序：打开/迁移 DB → 读 setup secret 与 admin 哈希 → **绑定数据面 + admin 两个
  listener** → 打印两条横幅 → 开始服务。任一绑定失败即返回错误退出，数据面 listener
  随函数返回被关闭（**不会静默只启动数据面**）；`admin_listen = "off"` 才只跑数据面。
- `serve` 横幅：`[relay] listening on http://…`（数据面）与
  `[relay] admin listening on http://…`（admin）；关闭时打印 `admin listener disabled`。
- 请求日志仍是 `method path -> status`（不含 body/query/header/cookie/token）；库模块
  禁止 `println!/eprintln!`（canary 审计保持通过）。

### Web 认证 / 会话 / CSRF / Host-Origin / 限流

- `GET /admin/login`：无 JS、无第三方资产的 server-rendered 表单；页面含明文 HTTP 警告，
  未初始化时额外提示先 `init`。`POST /admin/login`：校验一次性 CSRF、常数时间比较
  `SHA256(token)`，成功 303 → `/admin` 并下发 `Set-Cookie`。
- Cookie：`mn_admin_session`（32B 随机 id，base64url），`HttpOnly; SameSite=Strict; Path=/`、
  host-only（无 `Domain`）；`Secure` 仅当浏览器 `Origin`/`Referer` 是 `https` 时追加
  （不信任 `X-Forwarded-Proto`）。数据面**从不读 cookie**，admin cookie 无法访问 `/v1`/`/v2`。
- 会话：内存 map（进程重启即失效），闲置 30 分钟 / 绝对 8 小时；`GET /admin` 续闲置计时；
  logout 立即删除；token 指纹变化（`set_token_hash`）清空全部会话；最多 64 个活跃会话。
- CSRF：登录页 token 服务端一次性（10 分钟）；登录后表单 token 与 session 绑定；
  缺/错/重放一律 403。
- Host/Origin：Host 必须**精确**命中白名单（实际监听地址 + `localhost`/`127.0.0.1`/`[::1]`
  同端口 + `admin_allowed_hosts`）；POST 要求 `Origin`（缺省时 `Referer`）的 authority
  与 `Host` **完全一致**，两者都没有也拒绝；防 DNS rebinding/跨站提交。无 CORS。
- 响应头：`X-Frame-Options: DENY`、CSP（`default-src 'none'` + 仅内联 style + `form-action 'self'`）、
  `Cache-Control: no-store`、`Referrer-Policy: no-referrer`、`X-Content-Type-Options: nosniff`。
- 限流：实例级 30 次登录尝试/分钟；单 IP 失败 5 次/5 分钟，第 6 次起 429（失败计数有上限、
  过期自动清理）。登录 body 上限 4 KiB，超限 413；重复/非法 form 键 400。
- 日志/错误脱敏：错误页与页面都不回显 token；登录失败只 401/429，不区分"token 错/未初始化"
  之外的更多信息；admin 页面只显示布尔"已登录"、schema 版本与 vault 计数。

### 测试与验证

- relay：`cargo test -p menghunote-relay --all-targets` = **323 passed / 0 failed**
  （基线 290 + 新增 33：11 个 admin 单元测试、14 个 in-process axum 集成测试
  `tests/admin_web.rs`、4 个进程级 CLI 测试、4 个 `serve` 配置解析测试）。
  覆盖：登录成功/错误 token/限流/CSRF/Host-Origin 伪造、闲置与绝对过期、logout、
  token 轮换吊销、未初始化不生成、admin cookie 数据面隔离、body 超限、库内/配置哈希优先级。
  全部真实 axum + 临时 SQLite + `FakeClock` 推进，**无 sleep**。
- 数据面 `/v1`/`/v2` 行为不变（路由与 wire 未改）；sync = **619 passed / 1 ignored**，
  app（`--features dev-sync`）= **369 passed**。

### 运维注意（admin 端口）

- 默认 `127.0.0.1:18081`，只在本机可访问；浏览器打开 `http://127.0.0.1:18081/admin/login`。
- 改 LAN（如 `0.0.0.0:18081` 或内网 IP）时：启动会打印警告；**HTTP 明文下 token 与会话
  Cookie 可被截获**，只应在受信任私网/隧道内使用，或放在同机 TLS 反向代理之后
  （relay 自身不终止 TLS；反向代理到 admin 端口时把浏览器 `Host` 写进
  `admin_allowed_hosts`）。
- **不要把 admin 端口映射到公网**；token 只在 `init` 输出一次，丢失只能重建实例
  （轮换命令后置）。

## S2 实施记录（2026-11-16，schema 13 / 权威 registry / 原子 claim·cancel）

范围：schema 12→13、`vault_registry`、票据 `created_at`/registry 关联、版本推断修复、
迁移、reservation/claim/cancel 原子语义、S4 Web 用的只读列表/详情。
**未做**：删除状态机/WS fence/fenced purge（S3）、Web 列表/删除 UI 与旧入口移除（S4）；
`/v2/admin/create-tickets`、App「管理员创建」、v1 setup secret 创建全部**保留**（S4 再删）。

### schema 13

- `vault_registry`（全局，权威）：`vault_id BLOB PK CHECK(length=16)`、
  `state TEXT CHECK IN ('RESERVED','ACTIVE','EXPIRED','CANCELLED','DELETING','DELETED',
  'DELETE_FAILED')`、`created_at NOT NULL`、`deleted_at`、`delete_job_id`、`delete_progress
  NOT NULL DEFAULT 0`、`delete_error`；终态墓碑只保留这些最小管理元数据（无租户内容/密钥）。
  `vault_id` 全局唯一（含墓碑），**终态 ID 禁止复用**。
- 约束：活状态（RESERVED/ACTIVE）不得带 `deleted_at`；`delete_job_id` 只属于删除作业状态；
  `deleted_at >= created_at`；`delete_progress >= 0`。
- `create_tickets` **重建**：加 `created_at INTEGER NOT NULL` 与
  `FOREIGN KEY(vault_id) REFERENCES vault_registry(vault_id)`；保留 `token_hash/expires_at/
  quota/consumed_at`。`quota` 新发放固定 **1**（只 0/1；旧库历史值按事实保留，不重写）；
  `created_at/expires_at` 在**写事务内**由 relay 时钟生成（不再用进队列前的采样）。
- `vaults` 仍是租户内容表，列不变。`ACTIVE` 必须对应 `vaults` 行：由 genesis 与 claim 的
  同一写事务、迁移自检保证；SQLite 跨表 CHECK 不可用，**没有**加触发器（claim 需要先 CAS
  registry 再 genesis，触发器会挡住合法顺序）。

### 版本推断修复（ADR 0018 §2）

- 元数据缺失时**不再**用 `vaults.authz_state` 存在性猜版本（旧逻辑会把 schema 12 标成最新
  版并跳过 12→13）。
- 显式形状标记：schema 12 = `vaults.authz_state` + `management_caps` + `device_credentials` +
  `dedup_v2` + `challenges.realm` 为 INTEGER **全齐**；schema 13 = `vault_registry` 表 +
  `create_tickets.created_at` 列 **全齐**；10/11 = 多租户形状但无上述标记。
- 标记只出现一部分（半迁移/损坏）→ `Error::Invariant("ambiguous")` fail closed，不猜。
- 修复路径按目标版本补全局表：<13 用 5 列 `create_tickets`（冻结 DDL），13 只补被误删的
  空票据表；`instance_id` 只补缺失、不覆盖。
- `schema::apply_at(conn, now)`：`Db::open` 传入 relay 时钟（测试 `FakeClock` 可精确断言），
  直接调用 `apply` 时退化系统时钟。**只用于迁移时刻**，不用于虚构历史创建时间。

### 12 → 13 迁移

单事务（`BEGIN IMMEDIATE`）+ `VACUUM INTO <db>.schema12.bak` 快照 + 中断回滚；步骤：

1. 建 `vault_registry`（`IF NOT EXISTS`，重复执行幂等）；
2. 存量 `vaults` 每行 → `ACTIVE`，`created_at` 取该行**既有值**（不虚构）；
3. 存量票据（只补没有 `vaults` 行的 `vault_id`）：
   - 未消费且未过期 → `RESERVED`；
   - 未消费但已过期 → `EXPIRED`；
   - 已消费 → `DELETED` 墓碑（删除时刻未知 → `deleted_at` 留 NULL；最小元数据，ID 禁复用）；
   - 有 `vaults` 行的票据不改 registry（已是 `ACTIVE`，历史重复票据原样保留）；
4. 重建 `create_tickets`：旧五列逐字节保留（含旧库 quota>1 的历史值）；缺 `created_at`
   的旧行填**迁移时刻**（文档口径：不是原始创建时间）；
5. 自检：`ACTIVE` 数 == `vaults` 行数、票据 `vault_id` 全部可解析到 registry、行数/多重集
   一致、`PRAGMA foreign_key_check` 为空；写 `schema_version=13` 后提交。
   任何一步失败整体回滚（旧库仍是 schema 12，旧 binary 仍可打开）。

迁移不改任何租户表/数据（12→13 只新增 registry + 重建票据表）；旧 12 的
`challenges/tokens` **不再**像 11→12 那样清空。

### reservation / claim / cancel

- `admin::create_reservation(db, ttl)`：writer 事务内 CSPRNG `vault_id` + registry `RESERVED`
  + 票据（`created_at/expires_at/quota=1`），**不建** `vaults` 行。
- `admin::create_reservation_for(db, vault_id, ttl)`：显式 id 的预留；该 id 已有任何 registry
  行（`RESERVED/ACTIVE/终态`）→ `CAS_CONFLICT`（**同 vault 重复票拒绝**、**终态 ID 禁复用**）。
- 旧 API 兼容外壳：`issue_create_ticket(_with_details)` 对新 id 走同一预留；对已存在
  （`ACTIVE`）vault 只发票据（`/v2/vaults` 会以 `ALREADY_INITIALIZED` 拒绝且不消费，保持
  ADR 0017「已存在 vault 不覆盖」）；`quota != 1` → `INVALID_REQUEST`。
- `registry::claim_reserved`（`POST /v2/vaults` 的唯一事务体）：
  1. 票据**只读先验**（存在/未过期/未消费/额度>0/绑定本 vault）→ 统一 403，不泄露 vault
     是否存在；
  2. registry：`ACTIVE` → 409 `ALREADY_INITIALIZED`（票据不消费）；`RESERVED` → 继续；
     `EXPIRED/CANCELLED/DELETING/DELETED/DELETE_FAILED` → 403；缺行 → 500（损坏，fail closed）；
  3. 同一事务：消费票据（`quota=0`+`consumed_at` CAS）→ `RESERVED→ACTIVE` CAS →
     genesis/租户行（`vault::create_vault_with_ticket`）→ 同 vault 兄弟票据全部作废；
  4. 任一步失败整体回滚：票据不消费、registry 不改、不留半个 vault。
- `registry::cancel_reservation`：
  - `RESERVED` 且仍有有效票据 → `CANCELLED` + 作废全部未消费票据；
  - `RESERVED` 但票据全部过期/不存在 → 显式标 `EXPIRED` + 作废票据，返回 `Expired`；
  - 重复取消 → `AlreadyCancelled`（幂等）；已过期 → `Expired`；
  - `ACTIVE`（claim 已赢）/删除作业状态 → 409 `CAS_CONFLICT`，**绝不删除已创建 vault**。
  - 与 claim 的竞争由单 writer 串行化：恰好一个赢；取消赢后 claim 只能 403。
- 只读列表/详情（S4 Web 用，**existing-only、不迁移、不写库**）：
  - `registry::list_vaults(conn, now)` / `vault_detail(conn, id, now)`：
    `RESERVED` 的票据过期/不存在时**读时推导**为 `EXPIRED`（`stored_state` 仍是 RESERVED，
    不落库）；`ACTIVE` 以 registry 为准并带 `vault_exists`（`vaults` 行缺失时告警）；
    详情含 `legacy_default/authz_state/active_epoch/head_seq`、票据 total/consumed/outstanding
    与 S3 用的删除列。
- v1 legacy 创建：setup secret genesis 在同一事务写 registry `ACTIVE`（`created_at=genesis`），
  v1 wire/行为不变；`create_vault_in_scope`（测试/内部）同样登记。

### 测试与验证

- 新增 `tests/migration_v13.rs`（6 个用例）：真实 schema 12 fixture（`fixtures/relay_schema12.sql`
  冻结 DDL，两 vault/票据/设备/挑战/邀请/控制链）升 13 逐字节/行数/FK 不变；registry
  ACTIVE/RESERVED/EXPIRED/DELETED 墓碑；票据 `created_at`=迁移时刻；`.schema12.bak` 快照；
  中断回滚/重试/重复打开幂等；缺元数据的 **12** 库被识别为 12 并迁移（不被标 13 跳过）；
  13 形状不重迁移；矛盾形状 fail closed；版本门禁 + 不写快照。
- 新增 `tests/registry_v13.rs`（7 个用例）：预留随机/CSPRNG/TTL/created_at/不建 vault 行；
  重复预留/终态复用拒绝；claim 原子（registry/vault/control/ticket consumed/兄弟票失效）；
  错/过期/已用/取消票 403 且不部分写；ACTIVE → 409 且不消费；claim-vs-cancel（两种顺序 +
  并发）与双 claim 并发恰好一个赢；列表/详情区分 RESERVED/ACTIVE/EXPIRED 且前后字节不变。
- 更新既有测试口径：`migration_v11` 票据列形状（+created_at）、`migration_v12` 最终版本 13、
  `multivault_v2` 的 quota=2 用例改为一次性票据语义；sync/app 的 e2e 用例改用
  「已存在 vault 的新票据」验证 409（不再依赖 quota>1）。relay **338 passed / 0 failed**
  （`--all-targets`，含 main/example 单元测试；S2 前基线 323）。sync **619 passed / 1 ignored**，
  app（`--features dev-sync`）**369 passed**。

### 运维迁移步骤（12 → 13）

1. 停掉旧 relay（本迁移需要维护窗口；同一库只允许一个 relay 进程）。
2. 备份：复制 `<db>` + `<db>-wal` + `<db>-shm`，或 `sqlite3 <db> "VACUUM INTO '<db>.manual.bak'"`。
   迁移自身还会写 `<db>.schema12.bak`（已存在则不覆盖，保留最早一份）。
3. 用新 binary 启动（`serve`/`init` 均可）→ 自动跑 12→13；观察启动日志无错误。
4. 校验：`sqlite3 <db> "SELECT value FROM relay_meta WHERE key='schema_version'"` = `13`；
   `SELECT state, COUNT(*) FROM vault_registry GROUP BY state`（应有与 `vaults` 行数相等的
   `ACTIVE`；未领取票据为 `RESERVED`/`EXPIRED`；无 vault 行的历史已消费票据为 `DELETED`）；
   `PRAGMA foreign_key_check` 为空。
5. 回滚：停 relay → 用 `<db>.schema12.bak`（或第 2 步的手工备份）覆盖 `<db>`（连同
   `-wal/-shm` 一并清理）→ 用旧（12）binary 启动。**13 库不能直接给旧 binary 打开**
   （版本门禁会拒绝，这是预期行为）。
6. 注意：迁移给旧票据补的 `created_at` 是**迁移时刻**，不是原始创建时间；迁移不改写旧
   `quota` 历史值（新发放固定 1）。S2 不提供删除能力，`DELETED` 墓碑只来自「已消费票据
   但无 `vaults` 行」的历史/损坏修复路径。

### 遗留与依赖

- **S3**（已完成，见上文「S3 实施记录」）：删除入口复用 `vault_registry` 的 CAS 与票据
  失效口径；fence/admission/purge 全部以 registry 为权威；`ACTIVE` 与 `vaults` 行的一致性
  由 `claim`/genesis 事务与迁移自检维持；删除终态保留「ACTIVE 不得重开 / 终态 ID 禁复用」。
- **S4** 依赖：`registry::list_vaults`/`vault_detail` 是只读投影（含 `vault_exists` 与
  票据计数）；Web 列表/创建/取消/删除只能改 registry + 票据，不直接写租户表；
  旧 `/v2/admin/create-tickets`、App「管理员创建」、CLI 旧入口在该切片移除时，
  一并删除 `issue_create_ticket(_with_details)`/`administrative ticket-only for ACTIVE` 兼容分支。

## S3 实施记录（2026-11-16，在线不可逆删除作业）

范围：`ACTIVE → DELETING → DELETED`（失败 `DELETE_FAILED`）的在线删除作业、
admission/事务双层 fence、WS 关闭、显式 drain、可恢复分批 purge、崩溃恢复、
数据面客户端错误语义。
**未做**：Web 列表/删除 UI、App 文案/入口、旧 CLI/SDK/路由移除（S4）；schema 13 **不变**
（只复用 `vault_registry` 的 `state/deleted_at/delete_job_id/delete_progress/delete_error`）。
删除入口目前只有受保护的 admin 触发路由（S4 接 UI 按钮/列表）。

### 状态、作业与幂等

- 新增模块 `crates/relay/src/deletion.rs`：`Deletion`（唯一运行时，数据面 `AppState` 与
  admin listener **共享同一个 `Arc`**）+ `AdmissionFence` + `WsConnections` + purge 作业。
- `Deletion::start(vault_id)`（admin 路由调用）在**一个写事务**里：
  1. `ACTIVE` → `DELETING` CAS（`WHERE state='ACTIVE'`），写 `delete_job_id`（CSPRNG 128-bit）、
     `deleted_at = 删除请求时刻`、`delete_progress = 0`、`delete_error = NULL`；
  2. 作废该 vault 全部未消费创建票据（`quota=0` + `consumed_at`）；已消费历史行留给 purge 清；
  3. 删除该 vault 的 `tokens`/`challenges`（realm 1/2/3/4 的临时认证全部失效）。
  **提交之后**才：置 admission fence、关闭该 vault 的 WS、返回 `202`/job。作业是独立
  tokio task，**不借用 HTTP 连接生命周期**：管理员断连/关浏览器不会取消。
- 幂等：`DELETING` 返回同一 job（`AlreadyDeleting`，必要时重启作业任务）；`DELETE_FAILED`
  重新武装**同一 job id**（`Retried`，清 `delete_error`、`delete_progress` 保留）；
  `DELETED` 返回墓碑（`AlreadyDeleted`，无任何动作）。重复/并发请求由单 writer 串行化。
- 非 `ACTIVE`：`RESERVED` → 409（预留请先取消，不能把票据直接变墓碑）；`EXPIRED`/
  `CANCELLED` → 409（没有可删除的租户数据；ID 已禁止复用）；未知 id → 404。
  预留/终态删除**绝不**重开 `ACTIVE`。
- `deleted_at` 的双重口径（schema 13 不新增列）：`DELETING`/`DELETE_FAILED` = **删除请求
  时刻**（作业开始，失败重试不变）；`DELETED` = **删除完成时刻**。`registry::vault_detail`
  的 S4 投影因此能区分"进行中/失败/完成"。
- `delete_progress` 口径：**从 `PURGE_ORDER` 队首起连续清空的表数（0..=26，含 `vaults`）**；
  每批提交时持久化，崩溃续跑不需要额外游标（下一批从第一张非空表开始）。
- `delete_error` 只写短错误码（`purge failed: sqlite|invariant|cas|writer|api|injected|no progress`），
  不含租户内容/密文；重试前清空。

### 双层 fence（admission + 事务级复核）

- **admission（进程内）**：`AdmissionFence` 是 `vault_id → Closing/DELETING|DELETE_FAILED|DELETED`
  的单调集合（只关不开，重复 close 不改变原因）。
  - 数据面 axum **最外层** middleware 从路径 `/v1|/v2/vaults/{vault}` 抽出 vault 段，
    删除态直接 `410 VAULT_DELETED`（鉴权/限流/解析之前）；
  - `/v1/vaults`、`/v2/vaults`（genesis/claim）没有路径 vault，由事务级复核拒绝；
  - WS 升级时再查一次（middleware 之后、`on_upgrade` 之前），注册后**再查一次**并立刻
    发关闭信号（覆盖"检查后刚好开始删除"的竞态）；`push_wake` 在查询前与 `send` 前各查一次。
- **事务级复核（持久）**：`Db::read`/`Db::write`（以及 `write_blocking`）在执行时调用
  `registry::ensure_not_fenced`——在**同一个写事务拿到写锁后**查权威 `vault_registry`。
  单 writer FIFO 使"写任务入队顺序"与"标记 CAS"有全序：标记前入队的写一定先提交；标记后
  执行的写（含 HTTP 已取消、响应已丢弃的 job）一定被拒绝。读路径同样复核，删除作业提交
  过 purge 后不可能再通过 scoped 读拿回旧数据。**只拒绝删除三态**：`RESERVED/EXPIRED/
  CANCELLED` 仍走各自状态机（claim/cancel 的 403/409 语义不变）。
- 覆盖清点：v1/v2 读写、匿名 genesis、auth challenge/token、control submit、recovery evidence、
  dedup replay（在管理写事务里，同一复核）、GC（`run_gc` 先跳过 fenced，事务复核竞态也按跳过
  处理）、claim、WS upgrade/发送。
- 错误语义：`ErrorCode::VaultDeleted` → wire `VAULT_DELETED`、HTTP **410**、
  `retryable=false`。客户端（sync/app）把它归为**永久错误 → `NeedsAttention`**（暂停自动重试），
  与 ADR 0018 §3 的客户端语义一致：**不清本地 pin、不自动 genesis/重建**；本轮没有 App 改动。

### WS 关闭与 drain（显式屏障，不用 sleep）

- `WsConnections` 按 vault 记录每个连接的 oneshot 关闭信号（`WsGuard` 在连接任务结束时注销，
  不会有注册泄漏）。删除开始后 `close_vault` 一次性唤醒全部连接；`wake_loop` 的 `select!`
  收到信号即发 `Close(1008 "vault deleted")` 并结束任务。
- 慢发送限时：正常帧 `send` 5s 超时、Close 帧 1s 超时；超时直接丢 socket（不承诺撤回已进入
  网络的字节），删除作业不被慢客户端拖住。
- drain = **显式屏障**，不是 sleep：
  1. 写/上传/排队 DB job：标记事务本身是 writer FIFO 上的屏障——它提交前入队的写都已提交，
     之后入队的写在事务开始即被拒绝；
  2. 响应 body/HTTP 取消：客户端断开不会取消已入队 job，但也无法绕过事务复核；
  3. WS：`close_vault` 的信号 + 各发送超时（不等待对端确认）；
  4. 读快照：已开始的读可能返回**尚未 purge** 的旧数据（删除尚未完成），但不会返回被 purge
     的数据；删除完成后新读一律 410。

### 分批 purge（拓扑、预算、终态）

- 顺序 `PURGE_TABLES`（26 项，按 FK **子 → 父**）：
  `checkpoint_refs → checkpoints → object_parts → key_slots → objects →
  maintenance_reservations → asset_objects → log → challenges → tokens → invites →
  management_caps → device_credentials → device_grants → bootstrap_leases → device_acks →
  devices → manager_keys → epochs → rotations → dedup → dedup_v2 → gc_plans → usage →
  vaults → control_commits`。
  - `vaults.upgrade_control_rev` 是复合 FK 到 `control_commits`：**先删 `vaults` 行**、
    `control_commits` 最后；`devices/dedup_v2` 同样在 `control_commits` 之前。集合与 schema
    的 25 张租户表 + `vaults` 一致（单元测试守不变量，新增表必须同步）。
  - 全程**不关闭** `PRAGMA foreign_keys`；终态前再做一次 `PRAGMA foreign_key_check`。
- 单批 = 一个短事务，预算（`PurgeLimits`）：
  - 行：`rows_per_batch`（硬上界，含票据行）；
  - 字节：`bytes_per_batch` —— 每行字节按 `PURGE_TABLES` 的 SQL 表达式**先选行后删除**，
    严格累计（首行许可，单行超预算时该批只删这一行）；字节截断**立刻结束本批**，
    绝不跳到后面的父表（否则子表未清空时删父行会撞 FK）；
  - 时间：`millis_per_batch`（软预算，至少一步）；
  - WAL：批后若 `-wal` 超过 `wal_budget_bytes` 就在 writer 线程做一次
    `wal_checkpoint(TRUNCATE)`（checkpoint 不在事务内；仍被读快照挡住时留给下一批）。
  每批之间 `tokio::task::yield_now()` 让出执行权；其它租户的读写只与单批短事务竞争。
- 票据行（未用/过期/已消费/兄弟）在第一批清理；`vaults` 行在 `control_commits` 前清理。
- **终态与最后一批同一个事务**：零残留复核（26 张表逐张 `COUNT(*) == 0` + 票据 0）→
  `PRAGMA foreign_key_check` 为空 → `UPDATE ... SET state='DELETED', deleted_at=完成时刻,
  delete_progress=26, delete_error=NULL WHERE state='DELETING'`（CAS 失败 = 矛盾 → 本批回滚）。
  失败只写 `DELETE_FAILED` + 短错误码 + 保留进度；同一 job 可重试。
- 删除是**逻辑清除**：不保证 WAL/空闲页/备份/客户端副本被擦除。

### 崩溃恢复与 fail closed

- `Deletion::recover_startup` 在 `serve` **绑定 listener 之前**执行：
  - `DELETING`：校验 `delete_job_id` 存在 → 围栏 + 重新入队（幂等续跑，进度从 registry 读）；
  - `DELETE_FAILED`：只围栏，等显式重试（重复 `start` 或 admin 再确认一次）；
  - `DELETED`：复核零残留（26 表 + 票据 + `vaults` 行），围栏保持；
  - 任何矛盾/缺失（`DELETING`/`DELETE_FAILED` 缺 job id、`DELETED` 仍有租户行、registry
    返回非删除态）→ `Error::Invariant`，**启动中止**（宁可起不来，也不对外服务一个
    "说删了但其实还在"的实例）。
- 崩溃点覆盖：标记提交后（`DELETING`+进度 0）、任意批次后（进度 >0）、终态提交后
  （`DELETED`）——三者重启都由测试验证。已开始的 purge 任务被进程杀掉时，进行中的事务
  要么提交（进度单调）要么回滚（下一批重做），不会留下半个删除。
- 墓碑 `DELETED` 会一直留在 admission 集合（内存 16B/条），因此实例生命周期内删除过的
  vault 都不会被"看起来可服务"；重启后从 registry 重新加载。

### 受保护的 admin 触发路由（S3 只触发，不做 UI）

- `POST /admin/vaults/{vault}/delete`（独立 admin listener）：
  - 必须已登录（否则 303 到登录页）、会话绑定的 CSRF token 一致（否则 403）、
    `confirm` 必须等于该 vault 的 32 位十六进制 id（否则 400）——`DELETE <id>` 语义；
  - 成功返回 **202** 作业页（只显示不透明 id/状态/进度，无租户内容/无 token）；
    `RESERVED/EXPIRED/CANCELLED` → 409 页并显示当前状态；未知 id → 404。
- 页面文案明确：作业独立于页面继续执行、断开浏览器不会取消、删除不可逆且是逻辑清除。

### 客户端语义（本轮无 App 改动）

- 删除态/墓碑上，数据面所有入口返回 **410 + `VAULT_DELETED` + `retryable=false`**；
  v1 legacy 路径同样（legacy 默认 vault 被删后不再有可解析的租户）。
- 现有 App 错误分类（`classify_failure` / `classify_sync_error`）把非白名单 4xx 归为
  `NeedsAttention`：暂停自动重试、保持暂停，**不清 pin、不自动 genesis/重建**；
  不引入 `SecurityPaused` 误用。S4 只补文案/引导。

### 测试与验证（2026-11-16）

- relay：`cargo test -p menghunote-relay --all-targets` = **355 passed / 0 failed**
  （S3 前 338；新增 5 个 `deletion` 单元测试 + 12 个 `tests/deletion.rs` 集成测试）。
  全部真实 axum/temp SQLite/`FakeClock`，**无 sleep**；收敛靠 `broadcast` 作业事件
  （`Started/Batch/Failed/Deleted/Stopped`）+ registry 状态。
- 集成测试覆盖：完整删除（202 在持久 `DELETING` 后、入口立刻 410、WS Close 1008、
  终态 `DELETED`、26 表+票据+vaults 零残留、其它 vault 正常、墓碑不得重开）；
  双层 fence（已入队但响应被丢弃的写不落库、读拒绝、GC 跳过、匿名 genesis 410）；
  并发重复删除幂等（恰好一个 `Started`）、删除中并发写、失败重试同一 job、
  标记后/批次后/终态后重启恢复、状态矛盾 fail closed；
  大 vault 分批（行/字节/WAL 预算断言 + 其它租户读写 2s 上界内不被阻塞）；
  `RESERVED/EXPIRED/CANCELLED/未知` 的 409/404；admin 路由的会话/CSRF/confirm；
  单行超字节预算时不跨父表。
- sync **619 passed / 1 ignored**、app（`--features dev-sync`）**369 passed**（无改动，回归）；
  schema 13/迁移不变（`migration_v13` 全部通过）。

### 在线删除运维语义

- 触发（S4 有 UI 之前）：浏览器登录 admin → 用表单 POST
  `http://<admin>/admin/vaults/<32hex vault>/delete`，字段 `csrf`（管理页表单里的值）与
  `confirm=<32hex vault>`。成功 202 即表示**删除已持久化**；此后数据面立即 410。
- 断连/关页面/重启 admin 浏览器**不会**取消作业；也不要在删除过程中停 relay（可以停，
  重启会自动续跑 `DELETING`）。
- 观察进度：`sqlite3 <db> "SELECT state, delete_progress, delete_error, delete_job_id
  FROM vault_registry WHERE vault_id = X'<hex>'"`。`DELETING` 且 `delete_progress` 增加 =
  正常分批；`DELETE_FAILED` 可对同一 id 再 POST 一次（同一 job 重试）。
- `DELETED` 是**墓碑**：`vaults` 行/25 张租户表/该 vault 票据都已无行，只剩 registry
  最小元数据（id/状态/时间/作业 id/进度）；ID 永久禁止复用。不要为了"清理墓碑"手工删
  registry 行——那会让旧 id 重新可被创建，破坏"终态 ID 禁复用"。
- 删除后磁盘上仍可能有残留：SQLite 空闲页/WAL、运维备份、客户端本地副本；relay 不提供
  取证擦除。要彻底销毁请停 relay 后删除整个 DB（`delete-vault` CLI 行为，S4 移除）。
- 删除只作用于目标 vault；其它租户的读写继续服务（差距仅在单批短事务的写锁）。
- 崩溃/断电后启动：若启动日志出现"加载删除 fence/恢复删除作业失败"，**不要**绕过启动
  人为改 registry/租户表——按错误里的 invariant 检查残留行，修复后重启（fail closed 是
  有意设计）。

### 与 S4 的依赖（未变，另有补充）

- **S4** 依赖：`registry::list_vaults`/`vault_detail`（`deleted_at` 在 `DELETING/
  DELETE_FAILED` 是请求时刻、`DELETED` 是完成时刻）与 `deletion::PURGE_ORDER`/`PurgeLimits`
  的定义；Web 删除按钮必须 POST 到本切片的受保护路由并展示 202 作业页/进度，
  **不得**直接写租户表或手工改 registry。
- S4 移除旧入口时不要删除 `Deletion`/fence/`Db::write_maintenance`：数据面与 GC 依赖它们。

## S4a 实施记录（2026-11-16，最小 Web 管理页面：列表/详情/创建领取链接/取消/删除确认）

范围：登录后的**最小管理界面**（`GET /admin`、`GET /admin/vaults/{id}`、
`GET /admin/vaults/new`、`POST /admin/vaults`、`POST /admin/vaults/{id}/cancel`、
`GET|POST /admin/vaults/{id}/delete`），claim 链接的 canonical `public_url` 配置，
一次性领取链接页，删除确认字符串 `DELETE <id>` 与无 JS 的 202/进度页。
**未做（S4b）**：旧 `/v2/admin/create-tickets` 端点、App「管理员创建」、主 CLI
`delete-vault`/`--issue-ticket` 的移除，App 文案/引导；S4a **不**移除任何旧入口。

### 配置：`public_url`（claim 链接的 canonical 数据面 origin）

- `ServeConfig`/`RelayConfig` 新增可选 `public_url = "http://127.0.0.1:18080"`
  （或 `https://relay.example.com`）；**只**用于渲染 `mn-pair://create` 链接里的 `relay=`，
  绝**不**从 `Host`/`listen`/请求头推导（防 Host 注入/反代错配）。
- 启动校验（`config::PublicUrl::parse`）：
  - 只接受 canonical origin `scheme://host[:port]`（无 path/query/fragment/userinfo，
    默认端口 80/443 省略，host 小写；IPv6 必须方括号）；
  - `https`：任意合法 IP/DNS 主机；
  - `http`：**只**允许 `localhost` / `127.0.0.1` / `[::1]`（与客户端
    `sync::relay_policy::parse_relay_url` 的 `local_dev` 白名单一致）。
    非 loopback 明文 HTTP **拒绝启动**（即使 `allow_plaintext = true` 也不放宽）：
    客户端 `remote_tls` 只接受 https、`local_dev` 只接受 loopback，生成 LAN HTTP
    链接会得到"看起来可用、实际解析失败"的地址（违反"不生成不可用链接"）。
  - 解析结果与客户端 parser 的 canonical 输出**逐字节一致**，由
    `crates/sync/tests/create_link_relay_output.rs` 反向锁死（relay 不依赖 sync）。
- 未配置 `public_url`：relay 正常启动（管理页仍可浏览/取消/删除），但
  `GET /admin/vaults/new` 与 `POST /admin/vaults` 返回 **409** 并明确提示如何配置；
  **不会**退回 listen/Host 推导，也不会生成链接。启动时打印
  `[relay] public_url=…`（已配置）或一行警告（未配置）。
- 管理页（列表/详情/创建页）只读展示该值，并标注"来自配置 `public_url`"。

### 路由/页面（全部登录后；S1 的 session/CSRF/Host/Origin/限流/响应头原样复用）

| 方法/路径 | 行为 |
| --- | --- |
| `GET /admin` | registry `list_vaults`：id / 状态 / 创建时间 / 数据面 URL / 设备数 / 票据过期 / 删除进度；按 `created_at, vault_id` 稳定排序；空 relay 友好文案 |
| `GET /admin/vaults/{id}` | 详情：生命周期（状态/创建/结束/删除 job·进度·错误）与授权·运行（authz_state/runtime_state/legacy/active_epoch/control_rev/head_seq）分开；设备数、票据 total/consumed/outstanding/expires、用量字节；DELETING/DELETE_FAILED 显示进度/错误；**不显示** ticket hash/明文 |
| `GET /admin/vaults/new` | 创建表单（只有 CSRF；无 vault_id/TTL/quota 输入）；未配置 `public_url` → 409 说明 |
| `POST /admin/vaults` | CSRF → `admin::create_reservation`（TTL 900s、quota=1、CSPRNG id）→ 303 到 `/admin/vaults/created/{reveal}`（URL 不含票据） |
| `GET /admin/vaults/created/{reveal}` | **一次性**领取链接页：vault id / 链接（fragment 内 ticket）/ 创建·过期时间；首次 GET 后即删除，刷新 → 404 |
| `POST /admin/vaults/{id}/cancel` | CSRF → `registry::cancel_reservation`；`RESERVED` 成功 303 回列表；`ACTIVE`（claim 已赢）/删除态 → 409；重复取消幂等 303；过期 → 409 且落 `EXPIRED` |
| `GET /admin/vaults/{id}/delete` | `ACTIVE` 显示确认表单；`DELETING` 显示 job/进度 + `<meta http-equiv="refresh" content="3">`（另有"刷新本页"链接，无 JS）；`DELETE_FAILED` 可重试同一 job；`DELETED` 显示墓碑 |
| `POST /admin/vaults/{id}/delete` | CSRF + `confirm` **精确** 等于 `DELETE <32 位小写 hex>`（大小写/空格严格、无 trim；否则 400）→ `Deletion::start`；持久 `DELETING` 提交后才返回 **202** 作业页；重复 → 同一 job；非 `ACTIVE` → 409 |

- 一次性领取链接：`AdminWeb` 内存表（10 分钟 TTL、上限 64、绑定创建它的会话），
  URL 里只有 32B 随机 reveal id；ticket 明文只在该页 body 出现一次，之后即删。
  丢失只能取消预留后重新创建（registry/票据不保存可重印明文）。
- 页面安全：所有动态文本（含 registry `delete_error`、`public_url`）统一 `escape_html`；
  `X-Frame-Options: DENY`、CSP（`default-src 'none'` + 内联 style + `form-action 'self'`）、
  `Cache-Control: no-store`、`Referrer-Policy: no-referrer`、`nosniff` 由 S1 中间件覆盖；
  无第三方资产/框架/JS；服务端渲染；token/session 不出现在任何页面/日志。
- 只读投影扩展（**无 schema 变更**）：`registry::list_vaults`/`vault_detail` 增加
  `device_count`、`delete_progress`、`runtime_state`、`control_rev`、`usage_normal_bytes`、
  `usage_maintenance_bytes`；仍 existing-only、不迁移、不写库。
- 创建/取消/删除全部走 S2 registry 与 S3 `Deletion`（不直接写租户表/SQL）；
  `write_instance_blocking` 放进 `spawn_blocking`，事务/幂等/竞争语义完全沿用。

### 测试与验证（2026-11-16）

- relay：`cargo test -p menghunote-relay --all-targets` = **369 passed / 0 failed**
  （S4a 前 356；新增 9 个 `tests/admin_web_ui.rs`、2 个 `PublicUrl` 解析单测、
  2 个 HTML 转义/UTC 格式化与一次性 reveal 单测）。
- 覆盖：空/ACTIVE/RESERVED/DELETING/DELETED 列表与稳定排序；详情小节/字段与
  `delete_error` HTML 转义；创建 CSRF 正负、自动 id/900s、链接 canonical 与票据回查、
  一次性页刷新失效、无 `public_url` 的 409；取消成功/ACTIVE 409/重复幂等/EXPIRED 409；
  删除确认大小写与空格严格、202/进度/meta refresh、重复同 job、非 ACTIVE 409、
  墓碑无恢复/回滚；未登录写路由不创建/不删除。
  全部真实 axum router + 临时 SQLite + `FakeClock`，**无 sleep**（删除收敛靠作业事件）。
- sync **619 passed / 1 ignored**（`create_link_relay_output` 仅扩展既有用例：
  `PublicUrl` canonical 与 `CreateLink::parse` 一致；用数不变）；
  app（`--features dev-sync`）**369 passed**（无改动，回归）。
- CLI e2e 覆盖 `public_url` 配置解析/启动横幅/管理页展示；非法 path/非 loopback HTTP
  的启动拒绝有进程级测试。

### 手工步骤（最小管理面）

1. `serve` 配置加 `public_url`（本机开发 `http://127.0.0.1:18080`；TLS 入口
   `https://relay.example.com`），启动后确认日志 `[relay] public_url=…`。
2. 浏览器打开 `http://127.0.0.1:18081/admin/login`，粘贴 `init` 只显示一次的 admin token。
3. 「创建领取链接」→ 复制**只显示一次**的 `mn-pair://create…`；刷新页面应显示"一次性页面已失效"。
4. 列表/详情核对状态、创建时间、数据面 URL、设备数、票据过期；预留可「取消预留」，
   丢失链接时先取消再重建（旧 id 不能复用）。
5. 删除：详情 →「删除该 vault」→ 逐字输入 `DELETE <32 位 hex>` → 202 作业页/进度页
   （3 秒 meta refresh）；`DELETING` 期间数据面立即 410；完成前后台不会把 202 当"已删除"。
6. `public_url` 未配置时，创建页会 409 提示；不要把 `http://<LAN-IP>:18080` 当作
   `public_url`（客户端不接受；relay 启动时会拒绝）。

### S4a → S4b 依赖

- **保留**：`config::PublicUrl`/`RelayConfig.public_url`、`ticket_link::create_link_url`、
  `AdminWeb` 的列表/详情/创建/取消/删除页面与 `Deletion` 共享运行时。S4b 只能删旧入口，
  不能把 claim 链接 origin 退回 `listen`/`Host` 推导。
- **S4b 待移除**（当前仍可用/仍有测试）：`/v2/admin/create-tickets` 路由与限流、
  `admin::issue_create_ticket(_with_details)` 的兼容分支、App「管理员创建」/
  `sync::admin_ticket`、主 CLI `delete-vault` 与示例 `--issue-ticket`。
- S4b 删除旧入口时，`admin::create_reservation(_for)`、`registry::claim_reserved`/
  `cancel_reservation`、`Deletion`/fence/`Db::write_maintenance` 必须保留（新 UI 与数据面共用）；
  同时更新 `tests/admin_ticket_v2.rs`、`crates/app/tests/onboarding_admin_create.rs`、
  `crates/sync/tests/admin_ticket.rs`、CLI 帮助/脚本文档。

## S4b 实施记录（2026-11-16，移除旧管理入口 / App 管理员模式 / CLI 删库）

范围：ADR 0018 §4 的**移除清单**落地。Web 管理面（S4a）、claim/cancel、registry、
`Deletion`/fence、`public_url`、`ticket_link`、v1/v2 数据面与 schema 均**未改语义**；
不新增兼容后门，不留死代码。

### relay

- `POST /v2/admin/create-tickets` 路由/handler/请求解码/`admin` 限流器与相关测试删除；
  `PeerAddr`（loopback 判定）与 `RelayConfig.v2_admin_limit` /
  `DEFAULT_V2_ADMIN_LIMIT` 一并移除。Web 管理面仍用 S2 `admin::create_reservation`。
- `admin::issue_create_ticket` / `issue_create_ticket_with_details` /
  `consume_create_ticket` 删除（Web 用 `create_reservation(_for)`；claim 用
  `claim_create_ticket` + `registry::claim_reserved`）。`issue_ticket_in_tx` 去掉
  `allow_active_ticket_only` 兼容分支：`ACTIVE` 不再能用新票据"探测/覆盖"（CAS_CONFLICT）。
  保留 `create_reservation(_for)`、`validate_create_ticket`、`claim_create_ticket`、
  `invalidate_sibling_tickets`、`load_create_ticket`（测试/诊断）。
- 主 CLI：`delete-vault` 子命令/实现/帮助/二次确认删除；`status` 改为**只读诊断**
  （`SQLITE_OPEN_READ_ONLY` + `query_only`，不打开 writer、不 `schema::apply`、不建库；
  schema 版本缺失/不匹配明确报错）。`init`/`serve` 保留；`init` 的重置提示改为
  "停服 + 备份 + 手工删库，或在 Web 管理面按 vault 删除"。
- `examples/dev_server.rs`：`--issue-ticket`（含 `--vault/--ticket-ttl/--ticket-quota`）、
  `issue_ticket_and_exit`、`create_link_output` 与其单测删除；DEV server 仍可 `--reset` /
  `--setup-only` / 起服务，票据改由 Web 管理面或测试夹具发行。
- `ticket_link` 保留 Web 用的 `create_link_url`；只为旧 `--issue-ticket` 服务的
  `http_origin` 删除（origin 校验归 `config::PublicUrl`）。
- 数据面 `/v1`/`/v2`、`Deletion`、`Db::write_maintenance`、GC 全部未改。

### sync

- 删除 `src/admin_ticket.rs`、`tests/admin_ticket.rs`、`lib.rs` 导出、
  `AdminTicket*`/`AdminTicketFailure`/`classify_failure` 及 `api_version` 的
  `admin_create_tickets_path` 与单测。
- 保留 `create_link`（解析/编码/票据）、`create_vault_with_ticket`、S4d1a 管理回放/cap/
  realm3、S4d1b 审批包；无 App 以外的生产调用者。
- dev-relay（纯测试脚手架）：`issue_create_ticket` 改走
  `relay::admin::create_reservation_for`（只接受全新 vault id，去掉 quota 参数），
  `start_with_v2_admin_limit` 删除；新增 `seed_ticket_for_existing_vault`（仅测试夹具，
  直接种票据以复现 409 `ALREADY_INITIALIZED`，不暴露任何 HTTP）。

### App

- 删除「管理员创建」：`CreateCredential::AdminSecret`、admin secret 输入框/worker/事件/
  `CreateStage::AdminTicket`/`submit_admin_create`/模式选择器与相关测试全部移除。
- Create 只剩「粘贴创建链接」（`FormCreateMode` 仅 `Link`）：表单只有创建链接 + 实例
  目录 + 同意框；relay 地址/模式由链接 scheme 决定，`remote_tls` 仍需远程确认。
- 同时移除 v1 setup secret 创建 UI 与 worker 分支（`CreateCredential::SetupSecret`、
  `DevLocal::create_deferring_identity`/`create_remote` 的 App 编排、`CreateStage`
  旧阶段、`cleanup_created_files`）；旧 CLI（`--dev-sync-create` 等）仍可用于旧实例。
- `CreateCredential` 枚举删除，`CreateRequest` 直接携带 `vault_id/ticket/expected_instance`；
  `run_create` 只走 `create_vault_with_ticket`（v2）。备份页「复制邀请链接」/Join 邀请/
  PENDING/批准不变。

### 测试与验证（2026-11-16）

- relay：`cargo test -p menghunote-relay --all-targets` = **356 passed / 0 failed**
  （S4b 前 369；删除 `tests/admin_ticket_v2.rs` 等 13 个用例）。新增 `tests/cli.rs`
  `status_is_read_only_existing_only_and_rejects_incompatible_schema`：不存在的库不创建、
  schema 版本被改成 12 时明确报错且库文件逐字节不变。
- sync：**605 passed / 0 failed**（S4b 前 619+1；删除 `admin_ticket` 模块/测试 14 个）。
- app：`--features dev-sync` = **365 passed / 0 failed**；默认 = **134 passed / 0 failed**
  （删除 `onboarding_admin_create.rs`；Create 全链路用例改走真实创建链接，
  `onboarding_create_link.rs` 断言无 Admin/legacy/relay/secret selector）。
- 全部真实 axum / 临时 SQLite / `FakeClock` / dev-relay，**无 sleep**。
- `cargo fmt --check`、clippy（relay/sync/app，`-D warnings`）通过。

### 兼容与后置

- **不恢复**旧端点/App Admin/CLI 删除；旧 v1 客户端仍可直接用 `/v1` + setup secret
  （数据面 wire 未改），只是 App 不再提供该入口。
- 首次配置：`init`（admin token）→ `serve`（`public_url` + `admin_listen`）→ Web
  「创建领取链接」→ App 粘贴链接。手工步骤见
  [`docs/notes/dev-sync-manual-test.md`](../notes/dev-sync-manual-test.md) 的 S4b 章节。
- 剩余验收：Windows 真机（Web 管理/claim 链接/App 粘贴）、真实 TLS relay 下线管理面；
  `S4d1c` 客户端管理 UI 与 `S4d2` relay 审批收件箱仍未做。

## 附：Opaque Origin 兼容（2026-11-16）

某些浏览器/隐私扩展/沙箱 WebView 对同源表单 POST 发送 `Origin: null` 且无 `Referer`
（页面本身有 `Referrer-Policy: no-referrer`），严格 Origin 门禁会让管理员完全无法登录。
现改为：

- `Origin`/`Referer` 存在且 authority 与 `Host` 不一致 → 仍 403；
- `Origin: null` 或两者都缺失 → 放行到 handler，必须通过一次性/会话绑定 CSRF，否则 403；
- Host 精确白名单、SameSite=Strict cookie、CSP/XFO 不变；跨站带来源的 POST 仍拒绝。

CSRF 是登录表单的主防线；这不会允许无法读取 CSRF token 的跨站页面提交。
