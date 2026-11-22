# ADR 0017 — UI 创建票据：管理员首次配置与邀请链接

> 状态：**已决**（2026-11-15，用户需求 + Lead 决断；S5 实施）。
> 关联：[ADR 0014](0014-multi-vault-invites-management.md)（创建票据/邀请）、[ADR 0016](0016-dual-sign-approval.md)。
>
> **后续变更（ADR 0018 S4b，2026-11-16）**：本 ADR S5a 的
> `POST /v2/admin/create-tickets` 与 S5b 的 App「管理员创建」/`sync::admin_ticket`
> **已移除**；创建票据改由内嵌 Web 管理面「创建领取链接」发行，App 只保留粘贴创建链接。
> 下文是当时的实施记录，不要按旧命令/UI 操作。

## 背景

当前 v2 创建票据只能由运维命令行 `dev_server --issue-ticket` 发行；App 只能粘贴
`mn-pair://create` 链接。用户要求：

- `vault_id` 自动生成，不由用户填；
- ticket TTL 用默认值，不暴露给用户；
- UI 能创建票据：第一次配置时在 UI 填服务器地址/配置，之后其他设备直接拷贝链接同步；
- UI 同时支持手动参数与粘贴票据/链接。

## 决策

### 1) 特权边界

- “创建票据”是 relay 管理员操作，**不允许匿名**；App 必须持管理凭证。
- 当前 dev/MVP 用 relay 的 **setup secret** 作为管理凭证（它本来就只应交给 relay 所有者）；
  UI 标注“relay 管理凭证”，不保存、不写配置/日志/Debug，每次创建只在内存使用。
- 该端点默认**仅 loopback**；远程生产必须走 TLS + 明确的 admin 凭据（后续 admin token 替换
  setup secret），本轮不开放远程匿名/无 TLS。
- 生产化后续：独立 admin token/管理端；setup secret 只用于首次 genesis，不再兼任长期管理。

### 2) Relay 管理发票 API

- `POST /v2/admin/create-tickets`（loopback only）：
  - CBOR `{0: bytes(setup_secret)}`；常量时间比较，失败统一 403，不泄露；
  - relay 自动生成随机 `vault_id`（16B），TTL 固定 900s，quota 默认 1（可后续配置）；
  - 返回 `{0: vault_id,1: ticket bytes32,2: expires_at,3: quota}`；
  - 不落 ticket 明文/日志；仅存 `SHA256(ticket)`；限流（IP/relay 级）；
  - 与 `/v2/vaults` 消费路径一致，票据绑定该 vault_id；已存在 vault 不覆盖。
- 现有 `--issue-ticket` 保留（自动化/运维），输出可直接生成 `mn-pair://create` 链接。

### 3) App 首次配置 UI

- CreateForm 增加「管理员创建（推荐）」：
  - 字段：relay 地址、relay 管理凭证（遮罩、可粘贴）、实例目录（默认自动）；
  - 点击创建：调用 admin API 取 ticket/vault_id → 立即 `create_vault_with_ticket`（v2）→
    身份/配置/恢复码备份页同现有 Create；
  - UI 不显示/不输入 vault_id、ticket TTL、ticket 本身；
  - 失败：403/429/409/不可达 → 明确、不写配置、不建身份；凭证不落盘。
- 保留「粘贴创建链接」模式（预发票/运维）；两种模式互斥。
- 创建成功后显示「复制邀请链接」（v2 邀请链接），其他设备粘贴加入；不再拷贝 ticket。

### 4) 安全/失败

- 管理凭证/ticket/邀请 token 不写日志、配置、错误文案、截图友好文案；
- admin API 仅 loopback；远程 admin 后置；
- 不自动重试创建；失败只清理本次产物；不覆盖已有库；
- 创建链接/邀请链接的 TTL 默认，不在 UI 暴露高级参数。

## 切片

- **S5a** relay admin ticket API + 测试（auth/loopback/限流/自动 vault/TTL/不存在覆盖）。
- **S5b** App Create 管理员模式（凭证/自动票据/创建/备份/复制邀请链接）+ 测试。
- 手工验证：首次配置只填 relay+管理凭证；其他设备粘贴邀请链接；不再需要命令行拼链接。

## 非目标

生产 admin token/管理端、远程 admin、匿名创建、ticket TTL/额度 UI 自定义、真实 TLS/Windows
（仍为 V2 发布门禁）。

## S5a 实施记录（2026-11-15，relay 管理发票 API + CLI 链接）

> 本片只做 relay 侧：`POST /v2/admin/create-tickets` + `dev_server --issue-ticket` 输出
> 可粘贴的 `mn-pair://create` 链接。**不做 App 表单**（S5b）。真实 TLS/Windows 与远程
> admin 仍未验；管理端点不开放远程/匿名，也不经反向代理。

### 1) `POST /v2/admin/create-tickets`

- 路由只挂在 `/v2`（v1 没有同名端点，`/v1/admin/create-tickets` → 404）；不经过普通
  bearer/token 鉴权，单独处理。
- 请求：CBOR/CDE **恰好一个字段** `{0: bytes(setup_secret)}`。CDE 解码拒绝重复键/乱序/
  尾随字节；本端点额外要求 `fields.len() == 1 && fields[0].0 == 0`，否则
  `400 INVALID_REQUEST`；secret 为空或 >512B 同样 400。
- 鉴权：`SHA256(请求 secret)` 与期望哈希**常数时间**比较（`ids::ct_eq`）。期望哈希来源：
  `RelayConfig.setup_secret_hash` → `RelayConfig.setup_secret` → `relay_meta` 里 `init`
  落的哈希（生产 `serve` 重启后没有明文，仍可用管理 API）。三者都没有 / 不匹配 /
  secret 错误统一 `403 FORBIDDEN`（与票据失败同构，不泄露是否配置过）。
- **只允许 loopback**：从 axum `ConnectInfo<SocketAddr>` 取 peer addr，不读 Host、
  不信任 `X-Forwarded-*`；`None`（未注入/未知）也拒绝。IPv4/IPv6 loopback 均放行，
  `::ffff:127.0.0.1`（双栈监听）按 loopback 处理。**带任何常见代理/转发头**
  （Forwarded、X-Forwarded-For/Host/Port/Proto、X-Real-IP、X-Client-IP、
  True-Client-IP、CF-Connecting-IP 等）也 403：生产 relay 由同机反代终止 TLS，
  否则代理后的远程请求 peer 也是 127.0.0.1；S5a 不提供远程 admin，fail-closed。
  `serve` / `dev_server` 改用 `into_make_service_with_connect_info::<SocketAddr>()`。
  **残余风险**：反向代理若不设置任何转发头，relay 侧无法区分"本机直连"与"代理转发"；
  因此部署方必须**不要**把 `/v2/admin/*` 暴露到反代（S5a 不声称反代后仍安全），
  远程 admin 由后续独立 admin token/管理端解决。
- 限流：实例级固定窗口 `RelayConfig.v2_admin_limit`，默认
  `DEFAULT_V2_ADMIN_LIMIT = 6 次/分钟`，超限 `429 RATE_LIMITED`；
  `with_v2_admin_limit(None)` / `without_v2_rate_limits()` 可关闭（测试/受控部署）。
  顺序：peer/代理门禁 → 限流 → body/凭证，因此远程/代理拒绝不消耗本机管理预算。
- 自动生成 16B `vault_id`（CSPRNG），TTL 固定 `DEFAULT_CREATE_TICKET_TTL_SECS = 900s`，
  quota 固定 `DEFAULT_CREATE_TICKET_QUOTA = 1`；调用
  `admin::issue_create_ticket_with_details`（绑定 vault_id，只落 `SHA256(ticket)`）。
- 响应：`{0: vault_id(bytes16), 1: ticket(bytes32), 2: expires_at(u), 3: quota(u)}`。
  明文只出现在响应体/CLI stdout（banner 的 hex 与 link 的 base64url 是同一 ticket 的
  两种视图）；不写日志/配置/DB，库内只有 hash。
- 与 `/v2/vaults` 同一消费路径：票据绑定该 vault_id；已有 vault 用该票据创建 →
  `409 ALREADY_INITIALIZED`，事务回滚，不覆盖、不消费票据。失败（403/429/409/坏请求）
  都不发票/不建库。

### 2) `--issue-ticket` 链接

- 保留原有 hex ticket / expires_at / quota / next step 输出，新增一行（仅此一处）：
  `create_link : mn-pair://create?v=1&relay=<percent-encoded http://listen>&vault=<hex>#ticket=<base64url>`
- canonical origin 由 `relay::ticket_link::http_origin` 从 `--listen` 生成（IPv6 加
  方括号、默认端口 80 省略、非 loopback 也按 listen 原样输出，仍 dev only）；
  query 值只保留 RFC 3986 unreserved，其余大写百分号编码；ticket 无 padding base64url
  且只在 fragment；不自动改 scheme/pin。`--listen` 端口 0（不监听）时打印
  `create_link : (skipped: ...)`，不输出坏链接。
- **格式一致性**：按 ADR 0003/0006，relay 不能依赖 `sync`，因此 CLI 渲染是
  `relay::ticket_link` 的最小 canonical 子集；`crates/sync/tests/create_link_relay_output.rs`
  用 `CreateLink::parse` + `CreateLink::encode` 对 IPv4/IPv6/default port 逐字节比对，
  并把 fragment base64url 解回同一 32B ticket（hex 与 base64url 同一票据）。
  非 loopback 的 dev origin 仍会渲染，但客户端 `parse_relay_url` 的 local_dev 白名单
  会拒绝（生产必须 TLS），测试显式锁住这个 dev-only 例外。

### 3) 测试（本机 Linux，2026-11-15）

- `cargo fmt --all` ✅；
  `cargo clippy -p menghunote-relay --all-targets -- -D warnings` ✅；
  `cargo clippy -p menghunote-sync --all-targets -- -D warnings` ✅
- `cargo test -p menghunote-relay --all-targets`：**290 passed / 0 failed**
  （基线 277 + 新增 13：管理 API 7、`ticket_link` 3、`dev_server` 链接 3）
- `cargo test -p menghunote-sync --all-targets`：**606 passed / 0 failed（1 ignored）**
  （基线 602+1 ignored；新增 4：链接逐字节一致 3 + 真实 dev-relay 端口 loopback 管理请求 1）
- 新增 `crates/relay/tests/admin_ticket_v2.rs`（7）：
  1. 正确凭证 → 200 + 随机 vault_id/32B ticket/`BASE_TIME+900`/quota 1；两次调用
     vault_id/ticket 都不同；库内只有 hash；setup.consumed 不变；用该票据
     `POST /v2/vaults` 成功且 quota 归零；绑定已有 vault 的票据 → 409 不覆盖/不消费；
  2. 错/空/缺字段/重复键/多余字段/未知键/text 类型/超长 → 400/403，且发票数为 0，
     错误体不含 secret；
  3. 缺 ConnectInfo/非 loopback/伪造 Host/代理头 → 403；loopback + 任意 Host → 200；
     IPv6 与 IPv4-mapped loopback → 200；
  4. 管理限流 1/60：非 loopback 不消耗预算；第 2 次 200 后 429 且不发票；
  5. 过期（901s）/错 vault/一次性重放 → 403，不建库、不消费；v1 同名路径 404；
  6. 配置只有哈希、或配置全空回落 `relay_meta` 哈希 → 均可发票；错凭证仍 403；
  7. 响应 ticket 的 base64url/hex 视图一致（无 padding）。
- relay 全部 in-process axum `Router::oneshot`（真实 router/DB/FakeClock、临时目录），
  管理请求用显式 `ConnectInfo` 模拟本机/远程/未知；**无 sleep**。sync 侧另起真实
  `127.0.0.1:0` dev-relay，HTTP 直连验证真实端口上的 peer 识别。

### 4) 手工命令

```bash
# A. 运维 CLI：发行票据 + 直接可粘贴的创建链接（明文只打印一次）
cargo run -p menghunote-relay --example dev_server -- \
  --issue-ticket --listen 127.0.0.1:18080 --vault 00112233445566778899aabbccddeeff

# B. 起 dev relay（banner 打印 setup secret；App/脚本用它做管理员凭证）
cargo run -p menghunote-relay --example dev_server -- --listen 127.0.0.1:18080 --reset

# C. 管理员发行票据（loopback 直连；CBOR {0: bytes(setup secret)}）。
#    SECRET 用 B 的 banner 值；下面用 Python 生成/解析这个最小 CBOR，避免手写字节。
SECRET='<dev server banner 的 setup secret>'
python3 - "$SECRET" <<'PY'
import sys, urllib.request
secret = sys.argv[1].encode()
def head(major, n):
    return bytes([(major << 5) | n]) if n < 24 else bytes([(major << 5) | 24, n])
body = bytes([0xA1, 0x00]) + head(2, len(secret)) + secret
req = urllib.request.Request(
    "http://127.0.0.1:18080/v2/admin/create-tickets",
    data=body, headers={"content-type": "application/cbor"}, method="POST")
raw = urllib.request.urlopen(req).read()

def read_uint(buf, pos):
    add = buf[pos] & 31
    if add < 24:
        return add, pos + 1
    size = {24: 1, 25: 2, 26: 4, 27: 8}[add]
    return int.from_bytes(buf[pos + 1:pos + 1 + size], "big"), pos + 1 + size

i, out = 1, {}  # 跳过 map 头（a4 = 4 项）
while i < len(raw):
    key, i = read_uint(raw, i)
    if raw[i] >> 5 == 2:
        n, i = read_uint(raw, i)
        out[key] = raw[i:i + n].hex(); i += n
    else:
        out[key], i = read_uint(raw, i)
print(f"vault_id={out[0]}\nticket={out[1]}\nexpires_at={out[2]} quota={out[3]}")
# ticket(hex) 可直接用于 POST /v2/vaults；链接优先取 CLI 的 create_link 一行。
PY
```

> 上面 C 的解析只覆盖本响应字段（bytes/uint），不引入 CBOR 依赖；生产/自动化请用
> `menghunote-sync` 的 `CreateLink`/`CreateTicket`。

### 5) 未做 / S5b 依赖

- App Create「管理员创建（推荐）」表单、凭证遮罩、创建后备份页、复制邀请链接：
  **S5b**；本片只提供 `POST /v2/admin/create-tickets` 与链接格式。
- 远程 admin / 独立 admin token / 管理端：未做；setup secret 仍兼任管理凭证，
  端点仅直连 loopback（代理头 fail-closed）。
- dev-relay TLS 分支未注入 `ConnectInfo`（自定义 Listener 的 peer 类型），S5b 的
  App e2e 管理用例走**明文 loopback** harness；生产 TLS 后的远程 admin 仍后置。
- 真实鉴权 relay / Windows / TLS 系统根未验；不因此打生产 tag。

## S5b 实施记录（2026-11-15，App 管理员创建 + 复制邀请链接）

> 本片把 S5a 的 relay 管理端点接进 App：Create 默认「管理员创建」（relay 地址 + relay 管理
> 凭证 → 服务器自动票据/vault_id → v2 创建 → 备份页），创建成功后可在备份页/设备面板
> **复制邀请链接**给其他设备。粘贴创建链接（S3b）与 v1 setup secret 传统创建都保留。
> 真实 TLS、Windows、远程 admin、relay 审批收件箱（S4d2）仍后置。

### 1) 客户端管理发票 API（`menghunote-sync`）

- 新模块 `sync::admin_ticket`：
  `admin_create_ticket(&RelayContext, setup_secret: &str) -> Result<AdminTicket, SyncError>`，
  显式时间版本 `admin_create_ticket_with(ctx, secret, now)`（测试/受控调用）。
  路径唯一来源 `RelayApi::admin_create_tickets_path()`（固定
  `/v2/admin/create-tickets`；不受 context 面影响，但调用仍要求 v2 context）。
- 仅接受唯一 `RelayContext`：`local_dev` 只能是显式 loopback 明文、`remote_tls` 只能是
  HTTPS canonical origin；本模块不另造 transport，不提供跳过 TLS/明文的旁路。
- 请求：CBOR/CDE **恰好一个字段** `{0: bytes(setup_secret)}`；空 / >512B 在联网**之前**
  拒绝（错误只含长度）。`AdminTicket { vault_id(16B), ticket(CreateTicket 32B),
  expires_at, quota }`；`ticket` 是既有 `CreateTicket`（`Debug` 脱敏、`Drop` 擦除、
  不可 `Clone`），本模块不提供持久化 API——调用方必须立即交给
  `create_vault_with_ticket` 消费。
- 严格响应：恰好 4 个字段、键严格 `0..=3`；`vault_id` 16B 且非全零、`ticket` 32B、
  `expires_at ∈ (now, now+24h]`（客户端镜像 relay 的 TTL 上限，只做校验）、
  `quota ∈ 1..=16`（镜像 relay 的额度上限）；任何不符 → `RelayWire`（fail-closed）。
  错误响应复用既有映射：403→`RelayApi{FORBIDDEN}`、429→`RATE_LIMITED`(retryable)、
  5xx→`HTTP_5xx`(retryable)；`admin_ticket::classify_failure` 归成
  `Forbidden/RateLimited/Server/Unreachable/Other` 供 UI 文案。
- 秘密纪律：编码缓冲 `Zeroizing<Vec<u8>>`；`HttpRequest.body` 的副本发送后 `zeroize`；
  响应体解码后擦除，响应里 ticket 的 CBOR 明文也逐字段擦除。凭证/票据不进
  Debug/错误文案/日志/配置/库；**不自动重试**。
- `create_vault_with_ticket` 原样复用（固定 v2 面），管理员模式只多一步取票：
  `admin_create_ticket` → 立刻 `create_vault_with_ticket(context_v2, admin.vault_id,
  &admin.ticket, None, ...)`；`vault_id`/pin 均由服务器事实决定，不需要 UI 或创建链接。

### 2) Create UI：三种互斥方式（默认管理员创建）

- `FormCreateMode = Admin | Link | Legacy`，选择器
  `app-root-create-mode-admin` / `-link` / `-legacy`；默认 **Admin**（推荐）。
  切换方式会清空内联错误与**所有**秘密输入（管理员凭证 / v1 setup secret / 已应用创建链接
  的票据），并丢弃链接状态；Admin↔Link 互斥由类型上"一次只带一种 `CreateCredential`"
  保证（新增 `CreateCredential::AdminSecret(Zeroizing<String>)`）。
- **管理员创建**（`app-root-admin-secret`）：
  - 字段：relay 地址（默认 loopback）、relay 管理凭证（遮罩 + 允许粘贴 + 禁复制）、
    实例目录（默认自动）、同意勾选；**没有** vault_id/ticket/TTL 输入；
  - 固定 `local_dev`（relay 管理端点只允许 loopback；远程 admin 后置），提交前用同一
    权威 parser 校验 loopback；文案明确"vault_id 由服务器自动生成，票据 15 分钟默认"；
  - worker：Progress（申请票据）→ `admin_create_ticket` → 凭证立即 `drop`（Zeroizing）→
    v2 创建 → 身份落盘 → 备份页（vault_id/恢复码/备份确认，与 v1 同一条路径）→ Main；
  - 失败：403/429/5xx/不可达/目录/锁/配置写失败都有明确分类；**不建库/身份/配置**，
    凭证输入在提交时立即清空、失败也不回填（回表单重试 = 手动重输，不自动重试）。
- **粘贴创建链接**：保留 S3b 完整语义（`app-root-create-link-*`）；未应用链接就提交会
  得到内联错误而不是偷偷落到 v1 流程。
- **v1 传统创建（Legacy）**：保留 `local_dev`（及 remote_tls）setup secret 流程与
  兼容性，不在本片删除；`form_set_mode(..)` 会显式切到 Legacy（"选 relay 模式"只对 v1
  传统创建有意义）。
- 配置写 `api_version=2`（Admin/Link）、`relay_mode=local_dev` 且 `relay_url` 为 canonical
  origin（与 URL 一致）；实例目录默认与其它 Create 相同。

### 3) 复制邀请链接（备份页 + 设备面板）

- 备份页新增 `app-root-copy-invite`「复制邀请链接给其他设备」（`app-root-invite-ttl` /
  `app-root-invite-error` 显示剩余时间/脱敏失败）：新模块 `onboarding::invite_copy`
  在专用线程读受保护身份 → `RelayContext::from_mode(..).with_api_version(实例面)` →
  `sync::invite_link::create_invite_link`（v2 实例的链接带 `api=2`、默认 15 分钟）→
  交回 `InviteLink`，UI 复制到剪贴板并 `zeroize` 本地 URL 副本；备份页只保留公开的
  `invite_id`/`expires_at`，不长期展示 token。
- **不要求活动文档**：邀请是管理操作（vault + 身份 + 管理私钥即可）；设备面板
  `invite_block_reason` 移除"没有活动文档"分支（仍保留无管理钥/在途操作/持闸忙的明确
  原因）。`dev-sync-invite` → `dev-sync-copy-invite` 维持显式两步复制（不自动复制）。
- 风险提示：链接含一次性 token（只在 fragment），只私发、不要截图/公开发布，剪贴板
  历史可能保留副本；token 不写日志/配置/Debug。
- 其他设备仍按既有流程：粘贴邀请链接 → PENDING（token 不落盘、不写配置/身份）→
  创建端批准 → bootstrap → Main（配置 `api_version=2`，`/v2` 数据面同步）。
- 失败（无管理钥/无 pin/pending 未收尾/relay 不可达/TLS）都在备份页/面板给出脱敏原因，
  不产生半成品、不写 token；每次点击都会生成新邀请（旧链接按 relay 一次性语义失效）。

### 4) 测试（本机 Linux，2026-11-15；真实进程内 dev relay，无 sleep）

- `cargo fmt --all` ✅；
  `cargo clippy -p menghunote-sync --all-targets -- -D warnings` ✅；
  `cargo clippy -p menghunote-app --all-targets --features dev-sync -- -D warnings` ✅
- `cargo test -p menghunote-app --features dev-sync`：**369 passed / 0 failed**
  （基线 365；新增 `onboarding_admin_create.rs` 4，`dev_sync_invite.rs` 改 1 不增数，
  `onboarding_ui.rs` 36 不变）
- `cargo test -p menghunote-app`（默认构建）：**134 passed / 0 failed**（不变）
- `cargo test -p menghunote-sync --all-targets`：**619 passed / 0 failed（1 ignored）**
  （基线 606+1；新增管理发票单测 6 + `api_version` 路径 1 + `tests/admin_ticket.rs` 6）
- `cargo test -p menghunote-relay --all-targets`：**290 passed / 0 failed**（不变）
- 新增 `crates/app/tests/onboarding_admin_create.rs`（4）：
  1. 默认管理员创建（无 v1 secret/链接/vault_id/ticket/TTL 输入）→ 自动票据 → v2 配置 →
     备份页 → Main → 重启直接 Main；配置 `api_version=2`、`relay_mode/URL` 一致；
     实例目录逐字节扫描不含管理凭证，配置/提示不含凭证/票据；
  2. 错凭证 → 403 明确、零本地残留、凭证清空；回表单重输正确凭证可成功（不自动重试）；
  3. 管理端点限流（dev-relay 新增 `start_with_v2_admin_limit`，1 次/分钟）→ 429 明确、
     零残留；relay 关闭 → 不可达明确、零残留；
  4. 备份页复制邀请链接（`api=2`、疑似 token 只在 fragment、风险提示）→ B 粘贴 PENDING →
     A 批准 → B Main（`api=2`）→ B 看到 A 的笔记 → `/v2` 数据面同步 A 的新笔记。
- 新增 `crates/sync/tests/admin_ticket.rs`（6）：严格响应/每次新 id/`expires_at=now+900`/
  quota=1；票据立即消费 v2 创建；一次性/过期 403 且不留库；错/空凭证分类且不回显；
  不可达分类；取票不写本地状态。
- `crates/sync/src/admin_ticket.rs` 单测（6）+ `api_version` 单测（1）：请求形状/严格解码/
  长度/TTL/quota/403/429/5xx 分类/Debug 与错误不含凭证/v1 context 拒绝/路径固定。
- `dev_sync_invite.rs`：无活动文档时邀请按钮**不再置灰**，点击确实派发 `CreateInvite`。
- 票据过期/已用/已存在 vault（409）的 UI 分类仍由 `onboarding_create_link.rs` 失败矩阵
  覆盖（管理员模式每次都是**新**票据+**新** vault_id，不存在"同 vault 重复创建"）。
- relay 侧 wire 未改（S5a 端点原样）；`dev-relay` 只新增测试 harness 的
  `start_with_v2_admin_limit` 与 `RateLimitSpec` 再导出。

### 5) 未做 / 依赖（明确）

- 真实 TLS relay / Windows 真机验收：仍后置；管理端点仍只允许**本机 loopback 直连**
  （带任何代理/转发头 fail-closed），远程 admin 与独立 admin token/管理端未做。
- relay 审批收件箱（S4d2）未做：批准仍在创建端设备面板完成。
- 票据 TTL/额度自定义、创建链接/邀请的 QR/scheme 注册、自动重试：不做。

## S5c 实施记录（2026-11-16，Create 自动发布基线 + 邀请前置检查 + 面板收敛）

> 触发问题：**刚创建就复制邀请，B 粘贴后被批准、检查时失败 `NO_VALID_BASELINE`**。
> 根因不是邀请 token，而是 Create 只做了 genesis + 初始 recovery 槽，**没有两份基线
> checkpoint**；B 的 bootstrap 找不到 published 基线。S5c 把"发布初始基线"接进 Create，
> 并在邀请生成前加本地前置检查，避免把失败留到加入端。

### 1) Create 自动发布初始基线

- `onboarding::create::run_create` 在 `create_vault_with_ticket` 成功后、返回
  `CreateOutcome` 之前，用**同一个 v2 `RelayContext`**（clone；强 pin 已在 genesis 中
  durable 提交）调用
  `DevLocal::publish_initial_pair_with_identity(&context, &created.vault, &created.identity)`：
  一轮真实 sync + 两份同 epoch checkpoint；初始 recovery 槽已在 Create 内上传，本调用
  不重复上传（`publish_initial_pair_with_identity` 的 fail-closed 校验 + 幂等语义不变）。
- 失败**不是 Create 失败**：库/身份/恢复码全部保留并照常交回 UI；`CreateOutcome` 新增
  `baseline_published: bool`，失败时 `false` + 已脱敏 `warning`（`sanitize_error`，
  不含 token）。不自动重试；备份页显示「基线未发布：进主界面后点「重试发布基线」」，
  Main 的 notice 也带脱敏原因。`CreateStage::PublishingBaseline` 只增加进度文案，
  **不新增线程**。
- 旧创建路径（CLI `--dev-sync-create` / 库级 `DevLocal::create`）不受影响。

### 2) 邀请前置检查（不联网、不回显 token）

- `onboarding::invite_copy::run_copy_invite` 一进来先读 `DevLocal::published_baseline`：
  `None` → 新分类 `InviteCopyFailure::NoBaseline`（文案固定为"创建端尚未发布初始基线；
  请先在设备面板点「发布基线」/重试"），**不发任何网络请求**、不生成链接。
- 设备面板的 worker 入口（`DevLocalRunner::create_invite`）同样先做本地检查并明确拒绝；
  `NO_VALID_BASELINE` 的旧错误映射/加入端提示保留为兜底（服务端错误语义不改）。
- 检查读的是 durable meta（Create/发布时写入），不是网络状态；损坏 meta 走 `Other`
  脱敏失败，不宣称"有基线"。

### 3) 面板与邀请展示（防截图泄露 token）

- 顶部 dev 面板**移除**「创建」「加入」：真实入口只在 AppRoot Welcome/向导里，
  避免"在面板创建就能跳过备份页"的误导；保留「同步」「停止」。
- 「发布基线」改为条件渲染：仅当 `can_manage && baseline_known &&
  published_baseline.is_none()` 时显示「重试发布基线」（仍走既有 `request_publish` →
  AppShell 持闸）；正常实例（Create 已自动发布）**不显示**，Join 设备也不显示。
  `DevicesListed` 事件新增 `published_baseline`（`None`/`Some`）投影，发布成功后自动
  刷新设备列表即隐藏按钮。
- 邀请区**不渲染** `mn-pair://join?...` 明文（含 token）：只显示 TTL/风险提示 +
  「复制邀请链接」；点击复制后折叠为 `dev-sync-invite-copied`（"已复制；关闭面板即
  失效"），不再展示链接/复制按钮；新邀请走「邀请设备…」。token 只进剪贴板，不写日志/
  配置/Debug，失败文案不含 URL。
- 备份页同样不显示链接明文；复制成功后显示「已复制（token 只在剪贴板）」，再次点击
  会生成并复制新链接（旧的按一次性语义作废）。

### 4) 测试（Linux，真实进程内 dev relay，无 sleep）

- `cargo fmt --all` ✅；`cargo clippy -p menghunote-app --all-targets --features dev-sync
  -- -D warnings` ✅；`cargo clippy -p menghunote-sync --all-targets -- -D warnings` ✅
- `cargo test -p menghunote-app --features dev-sync`：**367 passed / 0 failed**
  （基线 365；新增 `onboarding_create_link.rs` 2）
  - 新增 `create_auto_publishes_baseline_and_backup_invite_joins_immediately`：票据
    Create → durable 基线存在 → 备份页复制邀请（token 只在剪贴板）→ B 粘贴 PENDING →
    A 批准 → B「检查状态」进 Main（不再 NO_VALID_BASELINE）；
  - 新增 `failed_auto_publish_keeps_instance_blocks_invite_then_retry_repairs`：
    注入发布不可达 → Create 仍成功/恢复码仍显示/`baseline_published=false`；备份页复制
    邀请被本地前置检查明确挡住；进 Main 后按钮为「重试发布基线」，重试成功后按钮消失、
    邀请可生成；
  - `dev_sync_publish.rs`：基线缺失时按钮可见、发布成功后隐藏；Join 设备不渲染入口；
  - `dev_sync_invite.rs`：链接明文 selector 不存在；复制后折叠、剪贴板=链接、
    notice 不回显 URL；无活动文档仍可邀请；
  - `dev_sync_ui.rs`：面板无「创建」「加入」，正常/未知状态无发布入口。
- `cargo test -p menghunote-app`（默认构建）：**134 passed / 0 failed**（不变）
- `cargo test -p menghunote-sync --all-targets`：**605 passed / 0 failed（1 ignored）**
  （不变）；`cargo test -p menghunote-relay --all-targets`：**356 passed / 0 failed**
  （不变）。
- 不改 relay/schema/wire/协议/删除语义；token 仍不落盘。

### 5) 未做 / 后续

- 自动发布失败后的「重试发布基线」仍沿用既有持闸纪律：**需要先打开/新建一篇笔记**
  （没有会话就没有闸门目标）；空库直接重试会被明确拒绝并给出原因。
- Windows 真机与真实 TLS relay 仍未验证；本轮只做 Linux headless + 进程内 dev relay。
