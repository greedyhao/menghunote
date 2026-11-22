# dev-sync 手工演示步骤（ADR 0009 Phase 1 / 4c-3、4c-4；ADR 0011 GUI 向导）

> **DEV ONLY / 非生产。** 本文只在 localhost 用**放行验签**的 dev relay 演示 Phase 1
> 的两实例同步；relay、身份封装、恢复码都是开发脚手架，**不得**当生产部署或鉴权验收。
> 相关决策：[ADR 0009](../adr/0009-sync-ui-integration.md)、[ADR 0006](../adr/0006-sync-v1.md)、
> [ADR 0008（Linux 身份封装）](../adr/0008-linux-local-identity-wrap.md)。

> **2026-11-13 起（ADR 0011 S1–S4）**：带 `--features dev-sync` 构建**不再需要** env/flag：
> 直接开窗进入同步向导，**Create 与 Join 都在窗口里完成**（Welcome → 创建/加入 → 备份
> 恢复码（仅 Create）→ Main）。本文下面的 CLI 步骤保留给**自动化与复现**（两实例演示、
> 旧实例补传、`--dev-sync-info`）；GUI 路径见下面「GUI 向导」，已知缺口见
> [ADR 0011](../adr/0011-sync-onboarding-ui.md) 的「实施记录」。

## GUI 向导（推荐；真实用户不需要命令行）

> **ADR 0018 S4b 起（2026-11-16）**：Create **只支持「粘贴创建链接」**。App 不再有
> 「管理员创建」/ admin secret / v1 setup secret 创建入口；创建票据由 relay 的 **Web
> 管理面**「创建领取链接」发行。首次配置的完整顺序见本文末尾
> 「ADR 0018 S4b：首次配置（Web 管理面创建 claim 链接 → App 粘贴）」。

前提：`cargo run -p menghunote-app --features dev-sync`（不带任何 `--dev-sync*`、不设
`MENGHUNOTE_DEV_VAULT`），relay 已按 S4b 章节起好（`init` + `serve --config`，配置
`public_url` 与 `admin_listen`），并在浏览器管理面创建了一条 claim 领取链接。

1. **A 创建实例**：Welcome →「创建新的同步实例」（或「粘贴创建链接」）→ 粘贴
   `mn-pair://create?...` 领取链接 →「应用创建链接」→ 实例目录
   （默认在 `%LOCALAPPDATA%` / `$XDG_DATA_HOME` 的 `menghunote-dev/instances/<id>/`，
   可改成任意**绝对路径**，且必须不存在或为目录）→ 勾选「同意开发同步」→
   「用一次性票据创建」；
2. **A 备份恢复码**：创建成功后停在备份页，**只显示一次** `vault_id` + 恢复码；
   勾选「已在应用之外备份恢复码」→「继续进入主界面」。恢复码拥有数据/管理恢复能力，
   离开本页不再显示（想要自己抄一份，用「复制恢复码（有风险）」并清剪贴板）；
3. **A 的初始基线已自动发布**：Create 成功后会自动跑一轮同步并发布两份 checkpoint，
   备份页显示「初始基线已自动发布」，**不需要**再手动点「发布基线」。若该行显示
   「基线未发布」（自动发布失败）→ 先继续进 Main，新建/打开一篇笔记后在同步面板点
   「重试发布基线」；成功前邀请会被本地前置检查挡住；
4. **A 复制邀请链接**：备份页点「复制邀请链接给其他设备」→ 链接（v2、默认 15 分钟）
   只进剪贴板，**界面不显示明文**；复制成功后备份页显示「已复制」提示。也可进 Main 后
   在右上同步面板「邀请设备…」→「复制邀请链接」（面板同样只显示复制按钮/TTL，复制后
   折叠为「已复制；关闭面板即失效」）；
5. **B 加入**：另开一个进程（同一个二进制，同样不带 env/flag）→ Welcome →「加入已有
   同步实例」→ 在「邀请链接」字段粘贴 A 复制的 `mn-pair://join?...`（填实例目录，
   默认自动生成、必须不存在或为空）→「加入」→ 进入 **PENDING**；
   A 在同步面板核对指纹后点 pending 行的「批准…」→「确认批准」；B 点「检查状态」→
   bootstrap 导入基线 → Main。**B 不需要填恢复码**（v2 邀请链接自带一次性 token；
   恢复码只用于 v1 实例/灾后恢复，token 不落盘、不写配置）；
6. **B 进入 Main**：成功即进入真实主界面并**自动打开第一篇**（A 的笔记，正文来自基线），
   **不播种 demo**；公开配置 `dev-sync.json` 的 `active_instance` 指向 B；
7. 之后 B 想再打开：直接启动程序 → 有有效配置 + 身份 → **直接进 Main**（不闪 Welcome）。

### GUI 设置（S4：不改配置/命令行）

Welcome 页和 Main 都能打开设置：Main 的入口固定在**右下角**（编辑器状态栏上方），
按钮文字「设置」（selector `app-root-settings`）；Welcome 页在「创建/加入」旁边。
设置是**覆盖层**，Main 的编辑器/当前文档/滚动状态都保留，关闭后回到原状态。

1. **看实例**：顶部列出公开配置里的实例（active 优先），显示本机 instance_id（短）、
   `vault_path`、`vault_id`（短）、`device_id`（短）与「当前默认」标记。设置面板
   **没有** setup secret / 恢复码 / root / manager seed 字段。
2. **切换默认实例**：点某行的「设为默认」→ 原子写 `dev-sync.json` 的
   `active_instance` → 面板提示「重启后生效」。**当前 Main 不变**（不热切换
   Vault/worker）；重启程序后才打开新实例。
3. **改 relay**：输入框只针对 active 实例；只接受明文 loopback（`http://127.0.0.1` /
   `http://localhost` / `http://[::1]`，端口任意）。空值、https、非 loopback、
   带 userinfo、带路径都**内联报错且不写配置**；保存成功提示「重启后生效」，
   当前窗口继续用旧 relay，重启后才用新地址。
4. **复制公开信息**：`vault_id` / `device_id` / 库路径旁的「复制」只复制这些公开值；
   提示会说明不要分享 setup secret、恢复码、根密钥或设备私钥。
5. **关闭/失败**：「关闭（不保存）」或点空白只关面板，不写配置；配置写失败（例如目录
   只读）显示内联错误，修正权限后可重试保存——**不会**重建实例、删目录/库/身份，
   也不会重跑 Create/Join。

> **重启语义**：设置里的修改只改公开配置；正在运行的 Vault/worker/relay 不热切换。
> 需要重启程序（关窗再开，或重新 `cargo run`）才生效。这是 ADR 0011 的 MVP 边界：
> 换 active/relay 先要求重启，避免半途切换 Vault 造成双写或身份错配。

GUI 的失败恢复语义（都不是"重来一遍命令行"）：

| 现象 | 界面行为 |
| --- | --- |
| 恢复码格式非法（`mnrec1…` 不完整/校验失败） | 留在 **Join 表单**、内联脱敏错误；relay/vault_id/目录都保留，恢复码需重输；**不联网、不建文件** |
| 恢复码内容错误（格式合法） | 同上，错误说明"relay 拒绝：恢复码或 vault_id 不匹配" |
| A 还没发布基线 / vault_id 不符 / 404 | 同上，提示「请创建者先在同步面板点『发布基线』（需要两份 checkpoint）」 |
| relay 没起 / 地址不对 | 同上，提示不可达、可重试；不建身份/配置 |
| 目标目录已有库/身份/别的文件 | 表单内联错误，**不覆盖**用户目录 |
| 目标目录被另一个进程占用 | 表单内联错误，说明锁文件持有者；**不自动删锁**，释放后可重试 |
| Join 成功但配置写失败 | 进 Error 页，提示「重试保存」；**只重写配置，不重新 Join**（重跑会再造设备身份） |
| Working 中关窗/取消 | 关窗被拦截；取消只表示"等真实收尾"，迟到的成功仍会写配置并进 Main |

> **已知缺口**：秘密输入框是简化遮罩（渲染 `•`、不向平台报告正文、禁复制、允许粘贴），
> 秘密字段里的 **IME 组合提交**尚未完成；setup secret/恢复码请直接键入 ASCII 或粘贴。
> Linux 首次运行仍需用户主钥（ADR 0008）；Windows 走 DPAPI。

---

本文（下面各节）覆盖「A create → 建笔记 → **面板发布基线** → B join → 双向 sync」的**CLI
自动化路径**，用于复现与旧实例恢复。切片 4c-3 之前，恢复码在启动后就被丢弃、操作者拿不到，
B 无法 join；**切片 4c-4 之前**发布基线还要关窗口跑 CLI，手工创建非常麻烦。现在：

- `--dev-sync-create` 成功后，在**开窗前**把 `vault_id` 与恢复码打到 stdout **一次**；
- **A 建完笔记直接在面板点「发布基线」**（§2.4）：不用关窗口、不用恢复码，成功后 B
  立刻可以 join；
- `--dev-sync-publish` 不开窗，只发布初始基线并打印公开事实（**仅**自动化/恢复/旧实例；
  §2.5）——旧实例（4c-4 之前 Create 的库）必须用这条命令补一次初始槽；
- `--dev-sync-info` 不开窗、不联网，只打印公开标识（恢复码丢了用它找 `vault_id`）。

---

## 0. 安全纪律（先读）

| 纪律 | 说明 |
| --- | --- |
| **DEV ONLY** | relay 用 `DevPermissiveHook`（任何签名一律放行）、明文 HTTP、只绑 loopback。绝不暴露到网络；正式鉴权/验签负例必须走生产 `Ed25519Hook`。 |
| **恢复码只显示一次** | Create 时 stdout 打印一次，程序**不写日志/配置/meta/身份文件**。关掉终端后无法找回（Phase 1 没有恢复流程），只能重建实例。 |
| **不截图 / 不入日志 / 不进 shell 历史** | 恢复码等于「加入 vault 的凭据」。演示后清终端 scrollback 与 shell history；需要手输时用 `read -rs`（见下）。 |
| **setup secret 同理** | 只从 `MENGHUNOTE_DEV_SETUP_SECRET` 读，只在 relay 终端上出现；app 不复述、错误文案不回显。 |
| **`--dev-sync-info` 只显示公开标识** | `vault_id` / `device_id` / `peer` 不是秘密；恢复码、root、管理私钥**不会**出现在任何 CLI 输出里。 |
| **面板「发布基线」不需要关 app** | 切片 4c-4 起发布就在 A 窗口里做：面板先持外部提交闸门（flush 到 durable）再派发 worker。**CLI** `--dev-sync-publish` 仍要求 app 已关闭（它与 GUI 同时打开同一个库会有并发写） |

演示机密用可丢弃的实例目录（例如 `target/tmp/dev-sync-demo/`），结束即删；不要指向你真实的 vault。

---

## 1. 前置

- 仓库根目录执行；分支/HEAD 带 `--features dev-sync`（默认构建含产品 `sync-ui`，但
  **不含** `local_dev` / `--dev-sync*` / V2 管理；本演示必须用 `dev-sync` 构建）。
- 两个实例各自一个**尚不存在**的库路径：`.../a/vault.sqlite3`、`.../b/vault.sqlite3`；
  身份文件 `dev-identity.bin` 落在同目录（Create/Join 只接受全新路径）。
- Linux 首次运行会初始化用户主钥（ADR 0008，`$XDG_DATA_HOME/menghunote/local-identity/key-v1.bin`）；
  删掉它 = 身份无法解封，**不会**自动重建。
- Windows 走 DPAPI（当前用户）；跨用户复制实例目录必然解封失败。
- 关掉旧的 dev-sync 进程，避免同时开同一个库。

环境变量一览（全部只从环境变量读，**不要**放进 argv）：

| 变量 | 何时需要 | 含义 |
| --- | --- | --- |
| `MENGHUNOTE_DEV_VAULT` | 所有 dev-sync 命令 | 本实例的独立库路径（**必须显式**，不回退默认 vault） |
| `MENGHUNOTE_DEV_SYNC_RELAY` | 可选 | relay 地址，默认 `http://127.0.0.1:18080`，只允许明文 loopback |
| `MENGHUNOTE_DEV_SETUP_SECRET` | A `--dev-sync-create` | relay 打印的 setup secret |
| `MENGHUNOTE_DEV_VAULT_ID` | B `--dev-sync-join` | A 的 `vault_id`（32 位小写十六进制，公开标识） |
| `MENGHUNOTE_DEV_RECOVERY_CODE` | B `--dev-sync-join`、A `--dev-sync-publish` | `mnrec1...` 恢复码（**秘密**） |

---

## 2. 步骤

### 2.1 起 relay（终端 R，保持前台运行）

> **ADR 0018 S4b**：首次配置推荐用「`init` + `serve --config` + Web 管理面 claim 链接」
> （见文末 S4b 章节）。下面这条 `dev_server --setup-only` 是**旧 v1 CLI/dev 回归路径**：
> 它仍可用于旧实例（v1 setup secret genesis），但 App 的 Create 表单已不再提供
> setup secret 输入入口；不要用它做新的首次配置。

```bash
cargo run -p menghunote-relay --example dev_server -- --setup-only --reset
```

`--setup-only` 表示 relay **不自动 genesis**，由 A 的 `--dev-sync-create` 调
`POST /v1/vaults` 完成；否则会多出一个用不上的 vault。
复制它打印的：

```text
setup secret    : <base64url...>
```

### 2.2 A：Create（终端 A）

新实例目录必须不存在。首次 Create 之后，请**立刻**把 `vault_id` 与恢复码抄到安全位置
（密码管理器/临时纸面；不要截图、不要写进仓库）。

```bash
export MENGHUNOTE_DEV_VAULT="$PWD/target/tmp/dev-sync-demo/a/vault.sqlite3"
export MENGHUNOTE_DEV_SETUP_SECRET='<relay 打印的 setup secret>'
cargo run -p menghunote-app --features dev-sync -- --dev-sync-create
```

预期输出（顺序固定，**只此一次**）：

```text
[menghunote] dev-sync 模式 = create（新建 vault + 身份）；实例库 = ...（启动前不存在）
[DEV ONLY] vault_id=<32 位十六进制>
[DEV ONLY] recovery_code=mnrec1...
[DEV ONLY] 上面的恢复码只显示一次；B 侧 Join 用 vault_id + 恢复码。关闭终端后不再可恢复。
```

然后窗口打开（A 实例，空结构树）。日志里**不会**出现 setup secret 或恢复码。

> **Create 后必须是空树**（没有 `menghunote · dev 演示笔记` 等 demo 节点）。dev-sync 的
> Create/Join 重开窗口不播种演示树；若看到 3 个 demo 节点，说明用的是修复前的二进制或旧
> 目录，删掉整个 `target/tmp/dev-sync-demo` 重来。

### 2.3 A：真实建笔记

在 A 窗口里点左下角 **「＋ 笔记」** 新建一篇、输入中英文正文，等编辑器自动 flush
（状态栏正常、不再显示未保存）。这篇笔记会作为初始基线一起发布。

> 建完笔记**不要关窗口**，直接进 §2.4 点面板「发布基线」。

### 2.4 A：发布基线（现在由 Create 自动完成）

> **2026-11-16 起**：Create 成功后会自动发布初始基线，备份页显示「初始基线已自动
> 发布」；正常实例的同步面板**不显示**「发布基线」按钮。本节保留给**旧实例**（Create
> 时自动发布失败/从未发布）与 CLI 恢复场景——只有基线缺失时面板才显示「重试发布基线」。

A 建好笔记后**不用关窗口、不用命令行**：在面板上直接点「重试发布基线」（仅缺基线时可见）。

1. 面板在右上角（DEV ONLY 浮层）。确认「发布基线」按钮**没有置灰**（置灰 = 本机没有
   管理密钥，即这是 Join 设备）；
2. 点按钮 → 面板「最近操作」出现「请求发布基线：由 AppShell 持闸后派发 worker」；
3. 发布期间：活动文档输入被冻结（状态栏「外部提交中，输入已冻结」），面板任务行显示
   「发布：等待持闸 → 发布：已持闸，worker 同步中 → 发布：初始基线已发布」；
4. 成功后面板「最近操作」显示：

   ```text
   初始基线已发布（checkpoint <短 id>，覆盖 seq <S>）；B 现在可以 Join
   ```

   同时闸门自动释放、编辑器可以继续打字。**B 现在就可以 Join**（不需要重开 A）。

发布的前置条件（都会给出明确原因，不会静默跳过）：

| 现象 | 原因 |
| --- | --- |
| 按钮置灰 + 「本机没有管理密钥（Join 设备）」 | 只有 Create 设备能发布；Join 设备没有 `manager_seed`。 |
| 「当前没有打开的笔记」 | 发布需要先持闸（flush 到 durable），所以必须有一篇打开着的笔记——先点开/新建一篇。 |
| 「已有操作在途」/「同步已在进行」 | 单操作串行：等面板回到空闲（同步/设备刷新先结束）。 |
| 「本机建库时没有上传初始 recovery 槽（旧版本 Create）」 | 见 §2.5 的旧实例说明：用 CLI + 恢复码补一次，之后面板发布一直可用。 |

### 2.5 可选：A 用 CLI 发布（自动化/恢复/旧实例）

> 正常手工演示**不需要**这一步。只有三种情况用它：
> 1. **旧实例**：库是在切片 4c-4 之前用 `--dev-sync-create` 建的（当时 Create 不上传初始
>    recovery 槽）。上传 KEY_SLOT 需要的 `K_slot_mne` 只能由恢复码派生、恢复码不落盘，
>    所以面板无法补传——用这条命令补传一次（需要恢复码），之后面板发布一直可用；
> 2. 自动化/脚本（不开窗）；
> 3. 灾后重传（relay 侧对象丢了）。

CLI 发布**要求 app 已关闭**（见 §0）。

```bash
# 避免恢复码进 shell 历史（bash/zsh）：
read -rs -p "A 的恢复码: " MENGHUNOTE_DEV_RECOVERY_CODE; echo
export MENGHUNOTE_DEV_RECOVERY_CODE
export MENGHUNOTE_DEV_VAULT="$PWD/target/tmp/dev-sync-demo/a/vault.sqlite3"
cargo run -p menghunote-app --features dev-sync -- --dev-sync-publish
echo "exit=$?"
```

预期（不开窗、退出码 0）：

```text
[DEV ONLY] 初始基线已发布（重跑会多一份 checkpoint；B 侧现在可以 Join）
[DEV ONLY] checkpoint_id=<32 位十六进制>
[DEV ONLY] epoch=1 covered_seq=<S>
[DEV ONLY] published_at=<unix 秒> notes=<文档数>
```

失败语义：

- 缺 `MENGHUNOTE_DEV_RECOVERY_CODE` → 点名变量、退出非 0；
- 恢复码格式非法，或与本地管理身份不匹配 → 明确错误（不回显恢复码），退出非 0；
  **这两种材料错误发生在打开库/发任何请求之前**，库与身份文件不变；
- 发布中途网络失败的：已完成的部分是 relay 上的幂等对象/本地 durable 游标，
  修复后重跑即可（不自动重试；每重跑一次会多一对 checkpoint，`/bootstrap` 只取最新两份）。

发布后可以再用 `--dev-sync-publish` 重跑（4b 幂等），但**不要**把发布当成日常同步：
日常同步用面板「同步」。

### 2.6 （仅 CLI 发布过才需要）重开 A

面板发布（§2.4）**不需要**这一步：窗口一直开着就行。只有走 §2.5 的 CLI 发布（关过窗口）
才需要重开：

```bash
unset MENGHUNOTE_DEV_SETUP_SECRET MENGHUNOTE_DEV_RECOVERY_CODE
cargo run -p menghunote-app --features dev-sync -- --dev-sync
```

普通 `--dev-sync` **只 load 已有身份**，缺身份会明确报错（不会自动创建）。

### 2.7 B：Join（终端 B，自动化路径）

> 真实用户走上面的「GUI 向导」第 4 步即可；本节只用于**自动化/无窗口复现**。
> B 只需要 `vault_id + 恢复码` + 一个全新路径；**不读** A 的目录/身份/root。

```bash
read -rs -p "A 的恢复码: " MENGHUNOTE_DEV_RECOVERY_CODE; echo
export MENGHUNOTE_DEV_RECOVERY_CODE
export MENGHUNOTE_DEV_VAULT_ID='<A 在 2.2 打印的 vault_id>'
export MENGHUNOTE_DEV_VAULT="$PWD/target/tmp/dev-sync-demo/b/vault.sqlite3"
cargo run -p menghunote-app --features dev-sync -- --dev-sync-join
```

窗口打开后，B 的侧栏应已能看到 A 的笔记，点开可读到同一篇正文（由基线 bootstrap 导入，
不需要先点同步）。B 是独立设备：`device_id`/`peer` 与 A 不同，`manager_seed` 为空
（普通设备不能发布基线）。

### 2.8 A/B：面板「同步」

两个窗口都有 dev-only 同步面板（默认右下角浮层）：

1. 打开 A 的面板点「同步」→ 完成后再点 B 的「同步」；
2. 面板状态行从「空闲」→「持闸中」→「已持闸 / N 条变化」→「空闲」；
3. **同步期间活动文档输入被冻结**（状态栏「外部提交中，输入已冻结」），这是切片 4c-2
   的已知简化（整段持闸），同步结束自动解冻。

顺序（A 先、B 后）不是协议要求，但第一次演示照此走最容易观察：A push → relay →
B pull/integrate。

**空库 / 没有打开的笔记也能直接同步**（本切片修复）：B 刚 Join 完、侧栏还是空的，或
当前窗口**没有活动文档**时，点「同步」不再报"当前没有打开的笔记"——没有编辑器会话就走
**无会话同步**（不持闸、不 adopt），同步期间侧栏的结构写操作（＋笔记 / ＋文件夹 / 重命名 /
删除）被明确拒绝（提示「同步进行中，稍后再改结构」），结束后按结构 head 重投影，
并在树里出现笔记时**自动打开第一篇**。

远端新建笔记的结构与正文依赖已在 **worker 的 Integrate 阶段内**收敛（ADR 0012 S2b）：
结构先集成 → 只投影建立 body 行 → 正文在同一轮内集成/重试 deferred，所以**一次点击
只有一轮网络同步**，自动打开的新笔记里就是完整正文（面板不会再出现「继续同步正文」）。

### 2.9 双向验证

1. A 在笔记末尾输入 `A-2`（中文/emoji 更佳）→ A 面板「同步」；
2. B 面板「同步」→ B 当前打开的同篇正文出现 `A-2`；
3. B 输入 `B-1` → B「同步」→ A「同步」→ A 看到 `B-1`；
4. 可选：A 新建第二篇笔记 → A「同步」→ B「同步」→ B 侧栏出现新节点（结构树只在结构文档
   head 变化时重投影；若 B 正打开着第一篇，正文不会自动切换）。

### 2.10 设备列表与「移除设备（不轮换密钥）」（ADR 0010 切片 2a-2）

面板底部的 `dev-sync-devices` 区域是**只读快照**（打开面板时自动读一次、每次同步成功后
再读一次，也可以点「刷新设备」）。

1. A 的面板里应看到**两行**设备：A 那行状态是「可用 · 本机」，B 那行是「可用」；
   每行只显示**短 device id**（前 8 位 hex）与**指纹摘要**，不显示公钥/证书原文；
2. A 的本机行**没有**移除按钮；B 那行有「移除设备（不轮换密钥）」。点它 →
   出现二次确认（目标短 id + 固定警告），此时**还没有**发请求；
3. 「取消」→ 确认区消失、列表不变；再点一次「确认移除」→ B 变成「已移除」、移除按钮
   消失，`dev-sync-notice` 出现「已移除 <id>（仅断访问）；认证已刷新，下次同步会重新认证」；
4. B 的面板仍能读列表（Join 设备只是**没有管理密钥**）：A 那行的移除按钮置灰，点它只会
   提示「本机没有管理密钥（Join 设备）」，不会出现确认区；
5. B 点「同步」→ 明确失败 `REVOKED`（relay 已吊销它的设备凭证）；A 点「同步」仍成功
   （`auth_rev` 前进后 A 会重新认证）；
6. 被移除设备的**本地**库/身份/笔记原样保留，面板也**不会**把「已移除」翻回「可用」。

固定警告文案（按钮/确认区必须可见，别删）：

> 移除设备（不轮换密钥）：仅阻断它对 relay 的访问；旧数据/旧 root 仍可能被解密；
> 管理材料不会被撤销；请保留最新恢复码。

⚠️ 这条操作的真相就是警告里写的：**它不是密码学撤销**，也不撤销管理权（若目标持有
`manager_seed`/恢复码，它仍可能管理认证）。真要隔离管理密钥，得**显式轮换**——
入口见下一节（§2.11，ADR 0009 Phase 2 切片 6b）。

### 2.11 轮换密钥 / 应用轮换（ADR 0009 Phase 2 切片 6b）

仍用 §2 的 A（Create，管理设备）/B（Join，普通设备）两个实例；A/B 都要**打开一篇笔记**
（轮换与 apply 都在活动文档上持闸冻结，空库/没打开笔记时按钮置灰并提示"请先打开一篇"）。

1. **A 点「轮换密钥…」**（面板设备列表区域）→ 只展开二次确认，**还没有**任何写入：
   - 固定文案（一字不改）：轮换会用新密钥重新加密整库，并生成两份候选基线；旧 epoch
     数据不能再被新设备解密；被移除设备仍可解其已获得的数据；轮换期间输入会冻结。是否继续？
   - `重新封装恢复码（推荐）`：默认勾选。勾选时下面出现**恢复码输入框**（遮罩显示 `•`，
     可粘贴 `Ctrl+V` / `Shift+Insert`，不显示明文、不进日志）；不勾选时出现固定警告
     `轮换后新设备将无法用恢复码加入`。
   - `同时把已移除设备写入签名撤销列表`：默认勾选（ADR 0010）。**不需要手选设备**：
     面板把当前设备列表里 `已移除` 的行收集进 signed revoke 列表。
   - `取消`：确认区消失、恢复码输入框清空、epoch/身份不变。
2. 点「确认轮换」→ 面板任务行出现「轮换中（申请/候选/激活/推广）+ worker 明细」，
   编辑器输入冻结、自动同步暂停，「同步」「发布基线」等按钮置灰；成功后：
   - `dev-sync-notice` 显示 `轮换完成：epoch 1 → 2；checkpoint <短 id>/<短 id>；…`；
   - 面板任务行 `轮换完成（epoch 已切换）`，输入恢复，活动文档用**新 epoch** 重开
     （同一篇笔记重新打开，正文与 durable 一致）。
3. **B 点「应用轮换」**→ 二次确认（`应用新 epoch 需要重新验证授权并导入基线；期间输入会冻结。是否继续？`）
   → 确认后 B 持闸 → apply → `已应用新 epoch：1 → 2；checkpoint <短 id>`；B 的活动文档
   同样按新 epoch 重开。
4. 双方继续输入并各自点「同步」：E2 里双向可见（与 §2.9 相同，只是 epoch 变成 2）。
5. **恢复码两种结果**：第 1 步勾选并粘贴当前恢复码 → 轮换后新设备 C 仍可用
   `vault_id + 同一恢复码` Join 到 E2；不勾选（`None`）→ 轮换后新设备用旧恢复码 Join
   **明确失败**（恢复槽只由一次性随机种子封着，没人能解开）。
6. **失败语义**：
   - 错恢复码：确认后立即失败（`轮换失败…恢复码与当前管理身份不符`），身份文件与视图
     逐字节/逐实体不变，可直接改对恢复码**重试**；
   - 失败停在 journal 之后（阶段不是「未开始」）：提示里会写明「保留了 rotation journal：
     重试前需重启实例」（同一实例再点轮换会被"已有未完成 rotation journal"拒绝）；
   - 被移除设备点「应用轮换」：明确失败（不在 grant 列表 / 认证被拒），身份/视图保持旧
     epoch，可再点重试（不会半推广、不自动重试）。
7. **移除与轮换是两件事**：移除设备仍然**只删凭证**（§2.10），不会自动轮换；轮换也不会
   自动移除任何设备。要"未来数据连旧设备也解不开"，先移除（或同时勾选撤销列表）再轮换。

恢复码输入的两个已知限制（与向导一致）：秘密字段里的输入法组合提交不完善（请直接键入
ASCII 或粘贴）；输入框只用于**本次**重封，提交/取消后立即清空。

---

## 3. 丢失 `vault_id` 时：`--dev-sync-info`

```bash
export MENGHUNOTE_DEV_VAULT="$PWD/target/tmp/dev-sync-demo/a/vault.sqlite3"
cargo run -p menghunote-app --features dev-sync -- --dev-sync-info
```

预期（不开窗、**不联网**——relay 没起也能跑）：

```text
[DEV ONLY] vault_id=<32 位十六进制>
[DEV ONLY] device_id=<32 位十六进制>
[DEV ONLY] peer=<十进制 u64>
[DEV ONLY] 以上都是公开标识；恢复码不在此显示、也不落盘（Create 时只打印一次）
```

它只读已有实例：身份/库缺失都明确报错，**不会**创建任何东西。恢复码无法从这里找回。

---

## 4. 已知限制（Phase 1 / 4c-2 / 4c-3 / 4c-4 / ADR 0011）

- **GUI 向导已覆盖 Create 与 Join**：真实用户不再需要 CLI/env；CLI
  `--dev-sync-create/join` 只保留给自动化与无窗口复现。**发布初始基线**也已在面板里
  （§2.4），CLI `--dev-sync-publish` 只给自动化/恢复/旧实例（§2.5）。
- **秘密输入的 IME 组合未完成**：setup secret / 恢复码是简化遮罩输入（遮罩、禁复制、
  支持粘贴、Tab 切换），但秘密字段里的输入法组合提交还不完善——请直接键入 ASCII 或粘贴。
- **面板发布需要一篇打开的笔记**：发布与同步一样先持外部提交闸门（flush 到 durable），
  没有活动文档就没有闸门目标 → 明确拒绝，先打开一篇即可。
- **旧实例需要一次 CLI 补传**：切片 4c-4 之前 Create 的库没有上传初始 recovery 槽；
  `K_slot_mne` 只能由恢复码派生（不可逆、恢复码不落盘），面板不能补传，必须用
  `--dev-sync-publish` + 恢复码补一次。
- **同步期间整段持闸**：活动文档在整轮同步里不能输入；不是"只在 integrate 时持闸"。
- **只即时 adopt 活动文档**：其它正文文档即使本轮集成推进，也要等下次打开时由会话重放
  （数据已 durable，不会丢）。
- **结构树不自动切换**：结构变化后侧栏刷新；**已有活动文档**时不会自动切换/重开当前
  文档。只有**没有活动文档**（空库/还没打开笔记）时，同步结束才会自动打开第一篇。
- **CLI 发布必须 app 已关闭**；面板发布（§2.4）不需要。重复发布允许但会积累
  checkpoint（4b 语义，`/bootstrap` 只取最新两份）。
- **没有自动轮换/自动发布**：自动同步（ADR 0012）已接线，但**不会**自动触发轮换、
  应用新 epoch 或发布；这些都必须由用户显式点面板按钮。
- **移除设备 = 只断访问**（ADR 0010）：不轮换 root/epoch、不撤销管理材料、不能追回旧数据。
  「轮换密钥」是独立动作（§2.11），只有显式点击才发生，**不会**被移除设备自动串联。
- **轮换/apply 必须有活动文档**：它们要在活动文档上持闸冻结；空库/没打开笔记时按钮置灰
  并提示先打开一篇（无会话路径没有 `HeldGate` 证明，不降级）。
- **轮换失败保留 journal**：停在 journal 之后的失败不会自动恢复/重试，面板会明确提示
  "重试前需重启实例"；验证阶段（未开始）失败可直接重试。
- **恢复码只重封不重发**：轮换时可选"重新封装恢复码"（需要粘贴当前恢复码）；本片
  **没有**独立的恢复码重发流程，轮换不会更换管理密钥/恢复码本身。
- **pending 设备不能在这里移除**：没有移除按钮（relay 对 pending 返回 `409 INVALID_STATE`），
  取消待批准设备属于后续切片。
- **附件插入只在 dev-sync 构建可见**：见本文末「附件插入」一节；无内联预览/无拖放/
  无系统文件对话框；普通 `SyncOnce` **不**上传新资产（只能随「发布基线」/轮换上传播），
  所以"另一台设备拿不到字节"先看 A 有没有发布过包含该资产的基线。
- **旧开发库不支持**：只接受修复后新建的 vault/身份（ADR 0009），不做隐式迁移。
- **Linux 安全弱于 Windows**：0600 裸用户主钥（ADR 0008），不宣称等价 DPAPI；
  SQLite 正文仍非静态加密。

---

## 5. 排错

| 现象 | 原因 / 处理 |
| --- | --- |
| `启用 dev-sync 必须显式设置 MENGHUNOTE_DEV_VAULT` | 没给实例库路径；两个实例必须各自独立，不回退默认 vault。 |
| `dev-sync 的 relay 只允许明文 loopback` | relay 写成 `https://`、非 127.0.0.1/::1/localhost，或带 userinfo；改回 `http://127.0.0.1:18080`。 |
| `Create 只接受尚不存在的 vault 路径` | 目录/库已存在；换新目录（不覆盖、不迁移、不删除）。 |
| `dev identity 文件已存在，拒绝 Create` | 同目录残留 `dev-identity.bin`；换新目录或人工确认后清理。 |
| `dev-sync 实例缺少持久身份`（普通 `--dev-sync`） | 该路径还没 Create/Join；先按 §2.2/§2.7 初始化。 |
| Join 报恢复码/格式错误 | 恢复码复制不完整，或与 `vault_id` 不是同一对；错误不会回显内容，重新从 A 的 Create 输出抄。 |
| 面板「发布基线」按钮置灰 / 提示「本机没有管理密钥」 | 这是 Join 设备（没有 `manager_seed`）；基线只能由 A（Create 设备）发布。 |
| 面板发布报「当前没有打开的笔记」 | 先打开/新建一篇笔记（发布要先持闸 flush）。 |
| 面板发布报「本机建库时没有上传初始 recovery 槽（旧版本 Create）」 | 旧实例：按 §2.5 用 `--dev-sync-publish` + 恢复码补一次，之后面板发布可用。 |
| publish 报 `恢复码与本地管理身份不匹配` | 用了别的实例的恢复码；publish 只能对 A（Create 出来的管理设备）执行。 |
| publish 报 `该实例没有管理私钥` | 在 B（Join 设备）上跑了 publish；基线只能由 A 发布。 |
| 同步面板「被拒绝」 | 单操作在途/闸门未就绪；等面板回到「空闲」再点。 |
| 同步后另一篇文档没变 | 已知限制：只即时 adopt 活动文档，重开该篇即从库重放。 |
| 空库点同步后自动打开了第一篇 | 预期行为：没有活动文档走无会话同步，结束后自动打开树里第一篇。 |
| 空库同步后新笔记正文已经是完整的 | 预期行为（ADR 0012 S2b）：结构→正文依赖在 worker 的 Integrate 内收敛，不需要第二轮网络同步。 |

---

## 6. 自动化覆盖（本文对应的测试）

手工步骤的库函数路径已由真实进程内 relay 覆盖（不是 mock）：

```bash
export CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0
cargo test -p menghunote-app --features dev-sync --test dev_sync_setup
cargo test -p menghunote-app --features dev-sync --test dev_sync_devices
cargo test -p menghunote-sync --all-targets
```

- 面板发布（切片 4c-4，`--test dev_sync_publish`）：
  `publish_from_panel_then_b_join_sees_the_note`（A 在窗口里建笔记 → 点「发布基线」→
  **A 不重启**，B 只凭 vault_id + 恢复码 Join 就读到正文）、
  `publish_without_manager_key_is_refused`（Join 设备：按钮提示 + 宿主/worker 双重拒绝）、
  `publish_requires_a_held_gate_and_rejects_duplicate_commands`（没有活动文档 / 裸命令缺
  `HeldGate` / 同步在途时重复发布 → 一律明确拒绝；失败后闸门释放、输入恢复）、
  `publish_twice_is_idempotent_and_sync_still_works`（发布 → 继续编辑 + 同步 → 再发布，
  durable 基线指向较新那份，B 仍能 Join）。
- `handoff_block_from_create_can_be_pasted_into_join_after_cli_publish`：
  Create 块 → 丢库句柄（模拟关 app）→ `cli::publish_baseline` → 用块里的
  `vault_id`/恢复码 Join → 读到 A 的正文；
- `publish_cli_requires_matching_recovery_code_and_keeps_the_vault_unchanged`：
  缺码/错码 → 明确错误、身份文件与库不变；正确码 → 发布成功、B 可见；
- `info_reads_identity_and_vault_without_network`：relay 指到不可达端口仍能打印公开标识。

轮换密钥 / 应用轮换（ADR 0009 Phase 2 切片 6b，`--test dev_sync_rotation`）对应：

- `ui_rotate_then_apply_keeps_both_sides_editing`：两个真实 `AppShell` + 真实 dev relay →
  A 点 `dev-sync-rotate`（只展开确认：固定文案/全部 selector/不写库）→ 取消 → 取消勾选
  重封装（固定警告）→ 确认 → E2；B 点 `dev-sync-apply-epoch`（确认/取消）→ E2；双方继续
  编辑并用面板「同步」按钮双向可见；轮换后设备列表/移除入口仍在；
- `ui_recovery_reissue_controls_whether_new_devices_can_join`：默认勾选重封装但空输入 →
  确认被阻止；剪贴板粘贴（`Ctrl+V`）→ 秘密输入（遮罩）→ 提交后立刻清空 → 报告
  "恢复码已补发"、新设备 C 用同一恢复码 Join 到 E2；不勾选再轮换到 E3 → 新设备 Join
  明确失败；所有公开投影都不含恢复码正文；
- `ui_rotation_buttons_reflect_manager_document_and_busy`：Join B 轮换不可用（原因+
  点不开确认区）、但「应用轮换」可用；空库（无活动文档）两个入口都不可用并提示先打开
  一篇；黑洞 relay 让设备列表卡在在途时两个入口都禁用；
- `ui_cancelled_and_failed_rotation_keep_state_then_retry_succeeds`：取消不写；错恢复码
  明确失败（身份文件逐字节不变、旧视图保留、不谎称有 journal），随后用正确恢复码重试成功；
- `ui_removed_device_apply_epoch_reports_error_and_keeps_identity`：A 面板移除 B（只删凭证）
  → A 轮换并勾选撤销列表（`facts.revoked=1`，B 不在 grant 列表）→ B 点「应用轮换」明确
  失败、身份/视图保持 E1，再点一次仍明确失败（不粘死、不半推广）；
- `rotation_freezes_input_structure_and_switch_and_failure_keeps_old_state`（6a，已扩）：
  轮换在途时阶段文案为「轮换中（申请/候选/激活/推广）…」，两个入口都给"在途"原因。

设备列表 / 移除设备（ADR 0010 切片 2a-2）对应：

- `dev_sync_devices::panel_lists_devices_revokes_other_and_join_device_cannot_remove`：
  两个真实 `AppShell`（真实 dev relay）→ A 面板列出 A/B、A 标本机且无按钮 → 点 B 移除 →
  二次确认（含取消路径）→ 确认后 B=已移除、按钮消失、notice 文案 → A 同步仍成功 →
  B 面板按钮点不动（缺管理密钥）→ B 同步 `REVOKED`；
- `dev_sync_devices::repeat_revoke_does_not_claim_auth_refresh`：重复移除成功但不谎称
  「认证已刷新」（`changed=false` 的幂等语义）；
- `device_revocation::dev_local_members_lists_devices_and_revoke_marks_target_revoked` /
  `dev_local_revoke_refuses_self_and_missing_manager_key_without_side_effects`：
  `DevLocal::members` / `DevLocal::revoke_device` 的库层语义与本地守卫。

stdout 的最终排版由 `main` 打印、人工核对；不对字节级输出做断言。

---

## 自动同步与状态行（ADR 0012 S3，2026-11-13 起）

> **DEV ONLY**；自动同步只在**生产 Main 入口**启用（`main.rs` / GUI 向导进入 Main 的
> `AppShell::new_with_dev_sync_with_autosync`）。headless 测试/确定性复现用的是
> `AutoSyncConfig::no_ticker()` + 显式 tick，**没有**后台 ticker。

### 看哪里

侧栏底部常驻一行同步状态（固定 selector `sync-status`；需要处理时才多一行
`sync-status-error`，selector 只存在于 headless/test-support 构建）：

| 文案 | 含义 |
| --- | --- |
| `尚未同步` | 进 Main 后还没有同步过、也没有 durable 待办 |
| `自动同步中` | 自动轮在跑（网络阶段**不冻结输入**；Held 只覆盖 integrate+adopt） |
| `已同步 HH:MM` | 最近一轮成功（UTC；dev-only 不引入时区依赖） |
| `重试于 Ns 后（第 K 次失败）` | 可重试失败退避：5/10/20/40/80/160/300s（+ jitter 可注入） |
| `需要处理：<原因>` | 鉴权/TLS/签名/epoch/游标/结构投影失败等**不自动重试**；点「同步」= 显式重试 |
| `已接收·待应用 N（续批暂停：无进展）` | durable inbox 还有 N 条，但本轮零进展（等新依赖/新数据） |
| `… · 待发送 N` / `… · 已接收·待应用 N` | 真实计数（`sync_outbox` 待发送 / durable inbox 待应用） |

面板的同步行在自动轮显示「自动同步：…」；每轮成功**不弹 toast、不抢焦点**。
「收到」与「已应用」分开显示：`deferred`/待应用计数来自 worker 的 `NeedGate`/`Integrated`
与库里的 `count_inbox_ready`，不会把"拉到了"写成"已经进正文"。

### 什么时候会自动同步

1. **进入 Main**：只有 durable `sync_outbox`（pending/encrypted）或 `sync_inbox`
   （verified 连续前缀）有待办时才排一次（重启恢复）；没有待办就等周期；
2. **本地编辑**：编辑器拿到 `DurableAck` 后**尾缘 2s**（连续输入只算最后一次）；
   远端 adopt 产生的 ACK **不触发**（不产生本地编辑代数）；
3. **周期**：45s 一次探测（可配 30–60s）；**窗口失焦**降到 60s；
4. **重新聚焦**：有 durable 待办时合并一次探测；
5. **失败重试**：只有明确 retryable（传输/5xx/BUSY/head CAS/超时）才退避重试；
6. **续批**：一轮 integrate 因批次预算/依赖留下 deferred 时，收尾后按"有进展才继续"
   再排一批（受 ≥5s 自动启动间隔约束），零进展就暂停这一轮并显示剩余。

自动请求同一时刻**只有一个**合并 pending；**不抢占**已开始的事务。手动「同步」/
「发布基线」/设备命令优先：自动轮在跑时点「同步」只提示"已合并"，本轮结束后立即补一次；
自动遇到 Held/单操作（例如面板的自动设备刷新）会稍后重试。

### 输入法与 defer

自动轮在 `NeedGate` 后才在当前活动文档上短持闸。若此时输入法正在组合（或 flush 还没到
durable），最多等**约 2s**；拿不到空闲就 **defer**：释放闸门票据、取消本轮（worker 在集成前
停手，**不 append**）、durable inbox 保留，随后自动续一次——**不会**强行结束你的组合。
手动同步保持原语义（继续等 IME 自然结束）。

### 手动验收步骤（两实例，同 §2 的 A/B）

1. 两边都按 §2 建好并进入 Main（A 先发布基线，B Join）；
2. A 输入一段文字后**什么都不点**：约 2s 后侧栏出现 `自动同步中`（网络阶段仍可继续输入），
   随后变 `已同步 HH:MM`；B 在 45s 内自动出现 A 的文字（也可切到 B 后等一个周期点）；
3. 把 relay 停掉：A 再输入 → 状态行出现 `重试于 …`（5s/10s/20s…递增），编辑不重置退避；
   把 relay 起回来 → 自动重试成功 → `已同步`；
4. 用错误身份/错误 epoch 制造鉴权类失败：状态行变 `需要处理：…`，不再自动重试；
   点「同步」（显式重试）才清掉暂停并重试；
5. 切到别的窗口（失焦）→ 周期降到 60s（日志/时序可观察）；切回来有 durable 待办会立即探测；
6. 输入法**组合中**正好赶上自动轮 NeedGate：约 2s 后状态行出现"稍后再试"类文案、
   **不会**被强制上屏/结束组合；组合结束后自动继续并成功；
7. 有 deferred（例如多文档/结构依赖）时观察 `已接收·待应用 N`：有进展会继续跑下一批，
   零进展显示"续批暂停：无进展"；不会在同一次 Held 里无限续；
8. 关掉窗口再开（新 `AppShell`）：若上次退出时还有 durable 待发送/待应用，进 Main 后
   **不用点任何按钮**就会自动同步一次；
9. 回退观察：手动「同步」「发布基线」仍然可用且仍然在 Held 下派发（`editor-sync-held`
   只在 integrate+adopt 期间出现）。

> **已知边界**：自动同步是 dev-only 功能；Windows 真机的输入法/失焦降频仍需人工复核
> （headless 用 debug 钩子模拟组合）。轮换（`key_epoch`）已由面板按钮接入（§2.11），但
> **自动同步不触发轮换/apply**（轮换期间自动轮暂停）；metadata 事件认证/对账
> （task-122…124）仍是后续。轮换期写下的 ASSET 与普通 checkpoint 的 reservation
> 复用已由 task-121 在 relay 侧幂等化（同字节 200 duplicate，不改 wire）。

## 远程模式、安全指纹与 SecurityPaused（ADR 0013 S3a，2026-11-13）

> **范围与状态**：本节对应 ADR 0013 **S3a**：公开配置的模式 UI、设置里的安全指纹、
> SecurityPaused 状态与启动 pending 对账。**远端 Create/Join 的网络接线仍属 S3b**，
> 本轮**没有接真实远端 relay**，也**没有 Windows 真机 / 真实 TLS 服务器实测**；
> 下面步骤只验证"配置/界面/失败语义"，不能当作生产可发布证明。
>
> 明确不做：私有 CA 导入 UI、`accept-invalid-cert`、自动"信任并继续"、自动清 pin、
> 自动重建实例。TLS 端点认证使用系统可信根；TLS 与控制链 pin 是两层独立校验。

### 1. 模式选择（`local_dev` / `remote_tls`）

- Create/Join 表单与设置都有「relay 模式」选择；**默认 `local_dev`**（dev 构建），
  它仍然只接受明文 loopback（`http://localhost` / `127.0.0.1` / `[::1]`）。
- `remote_tls` 只接受 `https://` + canonical origin（DNS/IP；拒绝 userinfo/query/
  fragment/非根路径）；**必须显式勾选**「我确认要连接远程服务器（HTTPS）」。
  确认端点 ≠ 授权接受服务器任意 hash：durable `sync.control_pin` 仍独立校验。
- ~~Create/Join 选择 remote_tls 会被明确拒绝~~ —— **S3b 起已放开**（见下一节）；
  S3a 当时不发起任何远程初始化请求、不建库、不写配置，也不静默降级 local_dev。
- 已有实例可以在**设置**里切换模式/URL：保存写 `ProfileV2`（`relay_mode` +
  canonical `relay_url`），提示「重启后生效」；当前窗口不热切换 Vault/worker。
- 手工演示（不需要远端服务器）：打开一个已存在的 local 实例 → 设置 → 勾选 remote 确认
  → URL 填 `https://relay.example`（或任意合法地址）→ 保存 → 关窗重启。重启后会按
  `remote_tls` 打开该实例；**不会**降级成本地明文。若随后触发同步，会走 production
  TLS 策略并（没有真实服务器时）失败——这正是下面 SecurityPaused 的路径。
- 需要切回本地：设置里选 `local_dev`、改回 `http://127.0.0.1:18080` → 保存 → 重启。
  切模式/换 origin **不会**清 pin；换 relay instance 要按新实例或重新 Join 处理。

### 2. 安全指纹（设置页只读区）

设置页新增「安全指纹（公开、只读）」：`relay 模式`、canonical origin、
relay `instance`（短）、record `rev` + hash（短）、bootstrap `rev`/`epoch`/`log_id` +
hash（短）；「复制安全指纹」给**完整公开值**（不含 token/恢复码/主密钥）。
旁边显示当前**错误类别**（安全暂停时）与固定说明「换服务器/清 pin 都不会自动发生」。

- 有 pin：显示上述公开指纹（来自 durable `sync.control_pin`）。
- **无 pin**：显示「未建立信任（NeedsTrust）：还没有 durable 控制 pin；不会自动信任，
  也不提供『信任并继续』」。
- pin 损坏：显示「控制 pin 损坏（PinCorrupt）…」并停止同步，**不**当成"没有 pin"。
- 设置页**没有**「重置 pin / 强制信任 / 接受无效证书」按钮；指纹区只读。
- 手工演示：Create 成功并进入 Main → 设置 → 指纹区应显示 record rev=1 与 hash；
  点复制后核对剪贴板里没有 `mnrec1`/root/私钥形态内容。

### 3. SecurityPaused（安全暂停）

遇到 `NeedsTrust` / `PinCorrupt` / `Equivocation` / `ControlRollback` /
`InstanceMismatch` / TLS（含传输策略拒绝）时：

- 自动同步进入 **SecurityPaused**：**停止自动重试、不重新连接**；状态行显示
  `需要处理：安全暂停（<脱敏类别>）`，`sync-status-error` 指向「设置 → 安全指纹」。
- 允许**本地编辑**（不持闸、不冻结编辑器）。
- **手动同步也会被明确拒绝**并给出原因；**不会**自动清 pin、不会盲信、不会重建实例。
- 网络类错误（连接失败/超时/5xx/BUSY/head CAS…）仍走 5/10/20/40… 退避，两者不混淆。
- 手工演示（无真实远端）：按 §1 把实例切到 `remote_tls` 并重启，触发一次同步：
  预期看到 SecurityPaused（TLS/传输策略），再点「同步」被拒绝；本地仍可输入文字。

### 4. 启动 pending 对账

Main/AppRoot 在**构造 `AppShell` 之前**调用 `init_session::reconcile`（无 pending 时
纯 no-op、**不联网**）：

| pending 阶段 | 启动行为 |
| --- | --- |
| 无 pending | 直接打开；不联网、不写库 |
| `Prepared`（请求未发） | 安全放弃（清 pending）；无身份时进 Error 恢复页 |
| `Done` | 清理 pending 后正常打开 |
| 已请求 + 有身份 | 用已有材料补 finalize（不重复 genesis/enroll）→ Done |
| 无身份 / 损坏 / 绑定矛盾 | Error 恢复页；**不自动重建、不删库/身份/pin、不启动同步 worker** |

身份落盘的 app 路径：Create/Join 让 pending 停在 `PinCommitted`，身份"临时文件 +
原子 rename"成功后推进 `IdentityWritten → Done`；若这一步前崩溃，下次启动对账补齐。
`local_dev` 的 Create/Join 先提交 pin 再落盘身份（`PinCommitted` 在阶段序上晚于
`IdentityWritten`，`mark_identity_written` 视为已满足、不回退）；远端 S3b 的规范顺序
（先身份、后 pin）会真正推进 `GenesisRequested → IdentityWritten`。

### 5. 未验证/后置

- **没有** Windows 真机与真实 TLS relay 实测；TLS 证书链/主机名/系统根只由
  `tests/tls_transport.rs` 的离线负例覆盖。
- 真实鉴权 relay、私有 CA 导入 UI、Windows DPAPI 真机均后置；本节不得作为生产验收依据。

## 远端模式（remote_tls）的 Create / Join（ADR 0013 S3b，2026-11-14）

> **范围与状态**：本节对应 ADR 0013 **S3b**：把 onboarding 的 remote Create/Join 接到
> `InitSession`（NewGenesis / FreshRecovery，production TLS + HTTP canonical origin）。
> 已验证的是**进程内真实 TLS relay**（自签 CA 显式注入客户端信任根）上的 A 创建 / B
> 恢复码 Join / pin 落盘 / 重启；**没有**连接任何真实远端 relay，也**没有** Windows
> 真机验证。因此这只是 dev 状态，不是生产可发布证明。
>
> 明确不做：私有 CA 导入 UI、`accept-invalid-cert`、自动"信任并继续"、自动清 pin、
> 自动重建实例、local_dev 与 remote_tls 之间的自动降级。TLS 端点认证使用系统可信根；
> TLS 与 ControlPin 是两层独立校验。

### 1. 前置（无真实远端时）

需要一个**支持 TLS 的 relay**。当前仓库内的 `menghunote-dev-relay` 只给测试用
（`DevRelay::start_tls()`：自签 CA + `localhost` 叶子证书，放行验签 hook）。手工演示
可在测试里跑，或自备一个带可信证书的 HTTPS relay（受系统根信任；本程序没有忽略证书
的开关）。**没有**内置的"启动 TLS dev relay"命令行——不要把它当成产品功能。

### 2. remote Create（NewGenesis）

UI：Welcome →「创建新的同步实例」→ 模式选 `remote_tls` → 勾选「我确认要连接远程
服务器（HTTPS）」→ 填 canonical `https://` origin、setup secret、实例目录 → 勾选
开发同步 →「创建」。

预期（真实顺序，可从设置/指纹与日志核对）：

1. 新建本地库并写 durable pending（`Prepared`，只含公开 id/模式/origin/阶段）；
2. `POST /v1/vaults`（写前推进 `GenesisRequested`）；relay 单事务创建 vault/设备；
3. 身份用「临时文件 + 原子 rename」受保护落盘（`DevIdentityStore`），随后推进
   `IdentityWritten`（**先身份、后 pin**）；
4. 用本地提交证书复算 `rev1 record_hash = SHA256(零32 || CDE(cert))`，在 TLS 上验证
   `/state + /control` 全链/分页连续性/rev1 前缀，`commit_verified_head` 提交强 pin
   （`PinCommitted`）；
5. 上传初始 recovery 槽；pending → `Done` 清理；
6. UI 停在**恢复码备份页**（与 local_dev 相同），确认后才写 `ProfileV2`
   （`relay_mode=remote_tls` + canonical `relay_url`）并进 Main。

失败语义（与 local_dev 不同，是有意的）：

- **拿到 genesis 回执/身份之前**失败（URL/模式非法、TLS 不受信、relay 明确拒绝如
  错 setup secret、传输中断）：清理本次新建的库文件与 pending（远端最多留下不可用的
  空 vault），**不写配置**，Error 页可回表单换目录重试。
- **拿到回执/身份材料之后**失败（控制链/前缀校验、pin 提交、身份落盘失败、槽上传等）：
  **保留** `pending`/库与已落盘事实（不删用户数据、不 unpin、不降级）；Error 页显示
  安全暂停类别；`Done` 之前的 pending 留给下次启动对账。初始槽上传/收尾失败降级为
  告警（实例可用，但其他设备用恢复码 Join 可能失败，恢复码仍在备份页）。
- TLS / NeedsTrust / PinCorrupt / Equivocation / InstanceMismatch 一律**不自动重试、
  不自动信任、不自动清 pin**。

### 3. remote Join（FreshRecovery）

UI：Welcome →「加入已有同步实例」→ 模式 `remote_tls` + 远程确认 → 填 HTTPS origin、
`vault_id`（32 位十六进制）、恢复码（`mnrec1…`）、新目录 →「加入」。

预期：

1. 恢复码格式在**建库之前**校验（错格式不建库/不联网）；
2. 新建本地库 + pending（`Prepared`）→ `self_enroll`（唯一一次 unpinned `/control`
   消费，仍强制恢复码推导的管理锚 + 逐条签名）→ 写前推进 `GenesisRequested`；
3. `bootstrap::run_with_pins` 认证导入基线（控制链/checkpoint/对象验证），pins/cursor
   单事务推进；
4. 身份原子落盘（`IdentityWritten`）→ 用**同一验证视图**复算 record pin + bootstrap
   锚 → `commit_verified_head`（`PinCommitted`）→ `Done`；
5. 写 `ProfileV2`（`remote_tls`）→ 进 Main，自动打开创建者的第一篇。

失败：错格式/错恢复码、TLS 不受信、无基线等**拿到身份材料之前**的失败都会清掉本次
新建的库文件（不留半初始化库，可换目录重试）；返回 JoinForm 并保留公开字段（relay/
`vault_id`/目录），恢复码已从输入框清空需重输。恢复码正文与 `mnrec1…` token 不出现在
错误文案里。

### 4. 配置与状态

- 配置：`ProfileV2` 每实例保存 `relay_mode` + canonical `relay_url`；Create/Join
  成功后写 active；**重启后生效**（当前 Main 不热切换模式/URL）。
- 安全指纹：remote Create 后应显示 record `rev=1` + hash（无 bootstrap 锚）；
  remote Join 后应显示 record hash + bootstrap 锚（**同 rev**）。
- SecurityPaused：初始化失败若属安全类，Error 页标题带「安全暂停（<类别>）」并给出
  类别提示；Join 表单同样标注，**不提供**信任/重置按钮。
- 启动对账：remote pending 与 local_dev 共用同一套 `reconcile`（`Prepared` 安全放弃；
  已请求 + 身份 → 补 finalize；无身份/损坏 → Error 恢复页，不重建/不删数据）。

### 5. 未验证/后置（S3b）

- **未**在真实鉴权 relay（`Ed25519Hook` 生产路径）上跑过 remote Create/Join；
  只用了进程内放行验签的测试 relay。
- **未**在 Windows 真机验证（DPAPI 身份封装、系统证书存储、原子 rename 语义）。
- **未**做私有 CA 导入 UI：目前用系统信任根；测试用自签根是**测试专用**
  注入（`with_test_extra_roots`，只加显式信任、不关闭链/SAN/有效期校验）。
- 跨机器网络抖动/代理/时钟偏差等真实环境因素未验证。

---

## 附件插入（dev-sync 构建，2026-11-14）

**只在 `--features dev-sync` 的构建里可见**（与 dev-sync 面板同一道门禁：运行时还要
`--dev-sync` / `MENGHUNOTE_DEV_SYNC=1` 才进 Main）。默认构建里没有这个按钮，也没有
`crates/app/src/attachment.rs`。

### 看哪里

| 位置 | 说明 |
| --- | --- |
| 侧栏底部「插入附件…」 | 点开路径输入（`attachment-path`），回车 / 「插入」提交，「取消」/ `Esc` 收起 |
| `attachment-error` | 失败原因（相对路径、不存在、不是普通文件、空文件、超过 20 MiB、文件名非 UTF-8、同步持闸、写入失败） |
| `attachment-notice` | 成功提示：文件名 + 字节数（不含绝对路径） |
| 正文 | 插入 `[附件：<文件名>](asset://<64 位十六进制 sha256>)` |

### 步骤（两个实例，A 建 B Join 之后）

1. **A**：打开任意一篇笔记（按钮要求有活动笔记；没有就整块置灰并说明原因）。
2. **A**：点侧栏底部「插入附件…」→ 粘贴/输入一个**绝对路径**（例如
   `/home/you/报告 最终.pdf`，中文与空格都可以）→ 回车。
   - 正文光标处出现 `[附件：报告 最终.pdf](asset://…)`；
   - 提示「已插入附件：报告 最终.pdf（N 字节）」；
   - 成功后可以 `Ctrl+Z` **一次**撤销这次插入（隐藏的资产不删，内容寻址无副作用）。
3. **A**：点面板「同步」（必要时再点「发布基线」）。**资产字节由「发布基线」上传**：
   `CheckpointWriter::prepare` 用 `StorageAssets` 读 `assets` 表 → ASSET 分片上传 →
   relay 为每个 ASSET 追加一条 `asset` 日志事件（本任务**没有**新增 wire 消息/DDL，
   这条上传路径本来就存在）。
4. **B**：点面板「同步」。B 的 pull 会真正下载并**验证**每条 ASSET 事件，然后把明文写进
   本地 `assets` 表（本任务补的最小接线；此前只验证不落地）。同步完成后：
   - B 的正文出现同一条 `[附件：…](asset://…)` 链接（普通 update，不重开文档）；
   - B 的 `asset_bytes(hash)` 与 A **逐字节一致**。
5. 收尾：两边关窗；资产留在 `assets`（没有删除/GC 入口）。

### 限制（非目标，别当 bug）

- **无内联图片预览**：`asset://` 只是普通链接文本，编辑器不解析、不渲染图片。
- **无拖放、无系统文件对话框**：`rfd` 在本仓库被禁用；路径只能手工输入/粘贴。
- **路径必须是绝对路径**；相对路径按 cwd 解释会漂移，直接拒绝。
- **单文件上限 20 MiB**（UI 边界，与 wire 的 64 MiB ASSET 上限无关）；空文件拒绝。
- **重复文件内容 = 单资产 + 多条引用**（内容寻址幂等）：插入两次只有一份资产字节，
  `asset_refs` 两条（按位置各一条）。
- **普通 SyncOnce 不会新上传资产**：ASSET 对象只能由「发布基线」/轮换这些维护路径上传
  （relay 的预算分类要求同一对象的全部分片归属一致，普通上传会和 checkpoint 的
  reservation 冲突）。因此「A 插入 → B 立刻同步」在 A 没发布基线时只能拿到链接，
  **拿不到字节**；这是已知限制，不是回归。
- **B 能拿到字节的前提**：B 的 `pull` 必须遇到 ASSET 日志事件（即 A 发布过包含该资产的
  基线）且 epoch 相同；轮换后由轮换路径重新上传。
- **文件名不是 UTF-8**（Unix 才可能）会被拒绝；Windows 路径用 UTF-16，不受影响。
- **没有"附件管理"UI**：不列、不删、不重命名资产；`asset_refs` 只是账面行。

### 自动化覆盖

| 测试 | 覆盖 |
| --- | --- |
| `crates/app/tests/attachment_ui.rs`（4 个） | 真实点击/按键插入（中文+空格+`[]()` 路径）、`asset_refs`/`asset_bytes`/`media_type`、重复插入幂等、undo/redo、上限/空/不存在/非普通文件/相对路径/非 UTF-8 文件名（零写入）、无活动笔记与同步持闸禁用 |
| `crates/app/tests/dev_sync_sync_once.rs::attachment_asset_bytes_and_link_reach_the_other_shell` | 两 AppShell + dev-relay：A 插入 → A 同步 + 发布基线 → B 同步后取到相同字节与链接（同步前 B 无资产） |
| `crates/sync`（既有 + 本次改动） | pull 消费 ASSET 时验证后落 `assets`（`skipped-asset` 记录语义不变） |
| `crates/app/src/attachment.rs` 单测 | Markdown 转义、MIME 映射、相对路径拒绝 |

### Windows 真机验点（未在本机验证）

- 绝对路径形如 `C:\Users\you\报告 最终.pdf`（反斜杠 + 中文 + 空格）能被接受并插入；
  相对路径被拒。
- `\\server\share\...`（UNC）是绝对路径，但网络盘读取失败/超时要给出明确错误（不 panic）。
- IME：路径里的中文由输入法上屏；组合中按回车不应插入（当前在「组合中」直接拒绝并提示）。
- 文件对话框/拖放**不应**出现（本片不实现）。

## 邀请链接（ADR 0014 S1，2026-11-14）

> **单 vault、现有管理钥。** 本片只把「加入」换成一次性邀请链接：兑换只产生 `PENDING`，
> 仍必须由创建端（有管理钥的设备）在设备面板**显式批准**，加入端才拿到数据。
> 不做多 vault / 创建链接（`mn-pair://create`）/ 系统 scheme 注册 / QR / 能力授权（v2）。

### 1. 创建端生成链接（A 的面板）

前提：A 是 **Create 设备**（身份里有管理钥）、实例已发布基线、**有活动文档**、当前没有
在途操作（同步/轮换/发布都算）。

1. A 打开同步面板 → 设备区域底部「邀请设备…」（selector `dev-sync-invite`）；
   按钮不可用时下方会说明原因（无管理钥 / 无活动文档 / 有操作在途 / worker 已停止）；
2. 点击后 worker 向 relay 申请一次性邀请，面板出现：
   - `dev-sync-invite-link`：完整链接
     `mn-pair://join?v=1&relay=<percent-encoded canonical origin>&vault=<hex32>&iid=<hex32>&pin=<hex32>#token=<base64url>`；
   - `dev-sync-invite-ttl`：TTL 文案（默认 **15 分钟**、上限 24h；过期只影响兑换）；
   - `dev-sync-copy-invite`：「复制链接」**显式**动作（不会自动复制）。
3. **链接只在内存**：关闭窗口/面板即丢；恢复码**绝不**进链接；完整 URL 不进日志/Debug/
   公开配置。token 只在 fragment，relay 只存 `SHA256(T)`。

### 2. 加入端兑换为 PENDING（B 的向导）

1. B 启动（同一二进制，`--features dev-sync`，无需 env/flag）→ Welcome →
   「加入已有同步实例」；
2. 在新字段「邀请链接（可选）」粘贴整段链接（**不需要**填 vault_id/恢复码）；
3. 提交后 worker 解析链接（严格 host/scheme/vault/iid/pin/token；拒绝 userinfo/未知
   query/额外 path）、生成**新设备**签名/HPKE key 与 peer、`redeem` 到 `PENDING`，
   并把 pending 材料（受保护）写进**新实例自己的**库；
4. 向导停在等待页（`app-root-invite-pending`）：显示 relay/vault/设备 id/**设备指纹**/
   sign 与 hpke 短 hash + 有效期；此时**没有**身份文件、**没有**公开配置；
5. 「检查状态」（`app-root-invite-check`）在创建者批准前只会得到「尚未批准」（relay 对
   pending 与已移除都回 `REVOKED`，不区分）。

### 3. 创建端批准（A 的面板）

1. A 点「刷新设备」→ 列表出现 `待批准` 行（ADR 0010 的成员视图已包含 pending 空证书行）；
2. pending 行出现「批准…」（selector `dev-sync-approve-<短id>`；**非管理设备不渲染**）；
3. 点击先展开确认区（`dev-sync-approve-warning`）：**目标 device_id + sign/hpke 短 hash**。
   把这两个短 hash 与加入端等待页显示的逐字核对，再点「确认批准」
   （`dev-sync-approve-confirm`）；
4. worker 用**现有管理钥**签 §17.1 设备证书 + 把当前 epoch 根封成 §17.2 HPKE grant；
   成功后 pending 行变 `可用`，notice 写「已批准 …（指纹 …）」；重复批准是幂等的
   （`changed=false`，不重复签名）。

### 4. 加入端批准后 bootstrap

1. B 点「检查状态」：设备认证成功 → 取 self grant → 用 pending 的 HPKE 私钥解出 root →
   `bootstrap::run_with_pins`（pin = 链接里的 relay instance）→ 提交 durable
   `sync.control_pin` → 写入受保护身份 → 写公开配置（active 指向新实例）→ **进入 Main**；
2. Main 自动打开基线里的第一篇（A 的笔记），之后双向编辑/同步与普通 Join 完全一致；
3. 失败保留 pending 可重试；bootstrap 成功但配置写失败 → Error 页「重试保存」
   （**只重写配置**，不重新联网/bootstrap）。

### 5. 重启与 PENDING 恢复

- PENDING 期间关闭窗口/PENDING 页点「返回（保留 PENDING）」：pending 已在库里 durable；
  再次启动 → 「加入已有同步实例」→ 粘贴**同一链接** → 走 **resume**（纯本地，
  **不重复兑换**、不会再造一台 pending 设备，`INVITE_USED` 不会出现）；
- 已批准但 bootstrap 未完成/已 bootstrap 但配置未写入：同样可以 resume；阶段
  `Bootstrapped` 时直接读回已落盘身份补齐配置（不重复联网）。

### 6. 失败矩阵（都对用户明确）

| 情况 | 表现 |
| --- | --- |
| 链接格式/host/scheme/vault/iid/pin/token 非法 | Join 表单内联错误；**不联网**、不建目录 |
| token 已用（`INVITE_USED`）| 明确「邀请已被使用或无效：请让创建者重新生成链接」；清掉本次新建库文件 |
| token 过期（`EXPIRED`）| 明确「邀请已过期（默认 15 分钟）」；清掉本次新建库文件 |
| 错 vault / 错 pin | relay 明确拒绝 / `InstanceMismatch`（**不自动换 pin**）|
| relay 不可达 / TLS 失败 | 可重试的分类提示；不建身份/配置 |
| 非管理设备批准/生成邀请 | 明确「本机没有管理密钥」；不发管理请求 |

### 7. 本片未做（明确）

- **多 vault**（relay 仍是单 vault schema 9）、`mn-pair://create` 创建链接、Windows
  scheme/深链注册、QR；
- 自动轮询批准状态（只有用户点「检查状态」；不做高频轮询）；
- 恢复码**链接**（明确拒绝；恢复码只作手工应急，不进 URL）；
- 能力授权/ManagementCap v2（仍是现有管理钥批准；普通设备不获得管理权）；
- 真实鉴权 relay / Windows DPAPI / TLS 系统根的真机验收（S0 后置）。

### 8. 自动化覆盖

| 测试 | 覆盖 |
| --- | --- |
| `crates/sync/tests/invite_links.rs`（5） | 链接→PENDING→批准→bootstrap→双向同步；durable resume（不重复兑换，含不可达 context）；已用/过期/错 vault/错 pin；非管理设备不能邀请/批准 |
| `crates/sync/src/invite_link.rs` 单测（8） | URL 编码/解析/恶意形态拒绝、token 不进 Debug/错误文案、TTL 边界、指纹/短 hash 稳定 |
| `crates/app/tests/dev_sync_invite.rs`（4） | 面板生成链接/复制文案/TTL、pending 行批准确认（device_id + 双端短 hash）、批准后 B 变 Active 并能同步、非管理设备无入口、无活动文档置灰 |
| `crates/app/tests/onboarding_invite.rs`（3） | 粘贴→PENDING→重启 resume→批准→Main（配置/笔记/身份事实）→再重启直接 Main；非法/恶意链接本地拒绝；已用链接明确失败 |

## ADR 0014 S3a：票据创建（`mn-pair://create`）手工步骤

> 本片只到**库级**：App UI 表单/多实例面板是 S3b。下面是当前可用的手工验证路径
> （Linux 本机、in-process/dev relay；真实 TLS relay 与 Windows 仍未验）。

### 1. 发行一次性创建票据（ADR 0018 S4b 起：只在 Web 管理面）

`dev_server --issue-ticket` 与 `POST /v2/admin/create-tickets` 已**移除**（S4b）；App 也
不再有「管理员创建」。创建票据的唯一入口是 relay 的 **Web 管理面**：

1. `init` 生成一次性 admin token（只打印一次）；`serve` 起数据面 + `admin_listen`
   （默认 `127.0.0.1:18081`），配置 `public_url`；
2. 浏览器登录 `http://127.0.0.1:18081/admin/login` →「创建领取链接」；
3. 页面**只显示一次** `mn-pair://create?...` 链接（含 ticket 明文）；刷新即失效；
   丢失就取消预留后重新创建（vault id 不能复用）。

票据语义不变：TTL 900s、quota=1、relay 库只有 `SHA256(ticket)`、绑定 `vault_id`；
跨 vault 创建统一 403 `FORBIDDEN`，已存在 vault → 409 `ALREADY_INITIALIZED`。

### 1b. 旧管理员 API / CLI（已删除）

`POST /v2/admin/create-tickets`、`dev_server --issue-ticket`、主 CLI `delete-vault` 均在
ADR 0018 S4b 删除；不要在任何新脚本/文档里引用它们。实例重置只能：停服 + 备份 + 手工
删库，或在 Web 管理面按 vault 执行 fenced 在线删除。

### 2. 生成创建链接（创建端/运维）

链接格式（ticket **只在 fragment**，`relay` 必须百分号编码）：

```text
mn-pair://create?v=1&relay=http%3A%2F%2F127.0.0.1%3A18080&vault=<hex32>[&pin=<hex32>]#ticket=<base64url>
```

- `pin` 可选：创建方已保存的 relay instance 公开锚；创建回执的 `instance_id` 必须与它相等，
  否则 `InstanceMismatch`（**不会**自动换 pin）；
- 解析复用唯一 relay URL parser：远端只接受 `https` canonical origin，`http` 只接受
  loopback 白名单；拒绝 userinfo/额外 path/未知或重复 query 字段；
- 链接/Debug/错误文案都不回显 ticket。

### 3. 用票据创建（当前库级入口）

```rust
let link = CreateLink::parse(text)?;                    // 或 CreateLink::encode(..) 生成
let context = link.relay_context(DEFAULT_TRANSPORT_TIMEOUTS)?; // 固定 v2 面
let created = create_vault_with_ticket(
    context,
    link.vault_id,
    &link.ticket,
    link.pin,             // 公开锚可选；None = 不要求同实例
    &vault_path,
    "实例标签",
    persist_identity,     // app 的原子落盘回调（S3b 才接 UI）
)?;
// created.vault / created.identity / created.recovery_code / created.warning
```

成功后：本地库已建、rev1 强 pin 已 durable、初始恢复码槽已上传、pending 已清理；
身份/公开配置由调用方决定是否落盘（库只返回事实）。403/409/429 分别对应
`FORBIDDEN` / `ALREADY_INITIALIZED` / `RATE_LIMITED`（`retryable=true`）；
失败不消费票据、不建本地库（回执前失败会清掉本次新建的库文件）。

### 4. API 面（v1/v2）

- 旧 ProfileV2（无 `api_version`）→ 显式 v1；新配置写 `"api_version": 2` 才走 v2；
- 同一 vault 的签名面在创建时固定：v2 创建的控制记录用 `/v2/...` 管理签名，
  用 v1 path 读取/复算会验签失败（fail-closed）；**不支持** v1 库中途切 v2；
- v2 的鉴权是租户内路径 `/v2/vaults/{id}/auth/{challenges,tokens}`，不是全局 `/v2/auth`。

### 5. 本片未做（明确）

- App UI 的创建表单、多实例面板、Windows scheme 注册/QR（S3b）；
- 自动创建票据（必须运维发行）；
- 真实鉴权 relay/Windows DPAPI/TLS 系统根真机验收（S0 后置）；
- 本机测试仍用放行验签的 dev relay，结论不含鉴权验收。

## ADR 0014 S3b：创建链接 UI、多实例与 v2 邀请（2026-11-14）

> 本片把 S3a 的库级能力接到 App：Create 表单的「粘贴创建链接」、同一 relay 多实例、
> v2 邀请链接（`api=2`）与设置页的 API 面展示。仍只做 **粘贴**（不做 Windows scheme/QR）；
> 测试全部是 Linux 本机 + 进程内 dev relay（放行验签），**不是**鉴权/Windows/TLS 验收。

### 1. 发行票据（ADR 0018 S4b 起：Web 管理面）

旧 `dev_server --issue-ticket` 已移除。用 `init` + `serve --config`（含 `public_url`）后，
浏览器登录管理面 →「创建领取链接」，复制**只显示一次**的创建链接：

```text
mn-pair://create?v=1&relay=http%3A%2F%2F127.0.0.1%3A18080&vault=<hex32>[&pin=<hex32>]#ticket=<base64url>
```

### 2. App 用创建链接 Create（Gui）

1. 启动（`--features dev-sync`，无需 env/flag）→ Welcome；
2. 点「粘贴创建链接（v2 票据）」（selector `app-root-create-link`）→ Create 表单；
3. 在「创建链接」字段（`app-root-create-link-input`，遮罩、允许粘贴）粘贴整段链接 →
   点「应用创建链接」（`app-root-create-link-apply`，或在该字段按 Enter）；
4. 应用成功后表单切到**票据模式**：
   - `app-root-create-link-warning`：一次性票据（默认 15 分钟）/不写配置日志/失败不消费票据；
   - `app-root-create-link-facts`：预填的 canonical relay + vault_id + 「API 面：v2」；
   - **setup secret 字段隐藏**（票据模式不需要）；链接输入框**立刻清空**（票据只在内存）；
   - 实例目录默认仍是 `instances/<自动 id>/`（可改；必须不存在或空目录）；
   - 勾选「同意开发同步」（`app-root-consent`）；`remote_tls` 链接还需勾选远程确认；
5. 点「用一次性票据创建」→ Working → **备份页**（恢复码只显示一次）→ 勾选备份 → Main。
   配置写入 `"api_version": 2`、`relay_mode` 与链接一致（`http`=local_dev / `https`=remote_tls）。

失败：403（`FORBIDDEN`，含已用/过期/错 vault，relay 统一不区分）、409
（`ALREADY_INITIALIZED`，同一 vault 重复）、429（`RATE_LIMITED`）都会带明确分类文案进
Error 页；**不建库/身份/配置**，票据不会被保存。配置写失败走既有「重试保存」（只重写配置，
不重跑创建）。

### 3. 同一 relay 第二个 vault（多实例）

1. 回到 Welcome（Main 右上「设置」不是必须；退出/返回即可）；
2. Welcome → `app-root-instance-<i>` 列表在创建第二个实例后出现两行（active 优先，
   行内显示 `v1/v2`、vault_id 短值、relay）；
3. 再走一次「粘贴创建链接」+ 新实例目录 → 第二个 profile **追加**（不删除/覆盖第一个），
   active 指向新实例；两个实例目录/身份/pin 各自独立；
4. 重启 → 自动打开 active；Welcome 列表可点开任一个（**不热切换**：打开另一个实例 = 关掉当前
   `AppShell` + 取目标目录锁；设置页「设为默认」只写配置，重启后生效）。

设置页（`app-root-settings` → `dev-sync-settings`）：
- 每行显示 `API v1/v2 · relay 模式 · vault_id 短值 · device_id 短值` + 库路径；
- active 实例另有 `settings-api-version`（API 面）与既有 `settings-fingerprint`
  （relay instance / record rev+hash / bootstrap 锚，全部公开只读）。

### 4. v2 邀请链接（`api=2`）

- A 是**票据创建**的 v2 vault：设备面板「邀请设备…」生成的链接自动带 `&api=2`：
  `mn-pair://join?v=1&relay=…&vault=<hex32>&iid=<hex32>&pin=<hex32>&api=2#token=…`；
- v1 vault 生成的链接仍**省略** `api`（旧链接逐字节兼容）；未知 `api` 值（如 `api=3`）
  在本地解析阶段拒绝，不联网；
- B 粘贴后：解析 → PENDING（库里的 pending 记录持久 `api_version=2`）→ A 在设备面板批准
  （v2 管理签名 `/v2/.../devices/{d}/approve`）→ B「检查状态」→ bootstrap 走 v2 数据面
  （租户内 auth / bootstrap / grants）→ Main；配置写 `"api_version": 2`；
- 面不一致（v2 pending 用 v1 context 检查）会 `ConfigInvalid` 拒绝，不会静默降级。

### 5. 已知限制 / 未做

- 只有「粘贴整段链接」：Windows scheme/深链注册、QR、系统剪贴板监听都未做；
- 不自动发行票据（必须 Web 管理面「创建领取链接」）；App 不生成 `mn-pair://create` 链接；
- v2 创建入口有实例级限流（默认 6 次/分钟）：批量演练会撞 429，等窗口过去再试；
- v1→v2 中途切换不支持（面在创建时固定）；多实例共用**同一个**进程内的 relay/身份封装；
- 真实鉴权 relay / Windows DPAPI / TLS 系统根的真机验收仍未做（S0 后置）。

### 6. 自动化覆盖（本片）

| 测试 | 覆盖 |
| --- | --- |
| `crates/app/tests/onboarding_create_link.rs`（5） | 同 relay 两票据 vault：profile/api_version=2/身份/pin/目录独立、笔记互不可见、重启分别进 Main、Welcome 列表分别打开；v2 邀请 `api=2` → PENDING → 批准 → Main → v2 面同步；403/409/429 失败矩阵（不建库/身份/配置）；票据不落配置/实例目录/错误文案；配置写失败只重试保存（不重跑创建） |
| `crates/sync/tests/invite_links.rs`（+1 = 6） | v2 链接 api=2、pending 持久 api=2、v1 context 拒绝（不降级）、v2 面批准/bootstrap、未知 api 本地拒绝 |
| `crates/sync/src/invite_link.rs` 单测（+1 = 9） | `api` 编码/解析/缺省 v1/未知拒绝/无 token 回显 |
| 既有 v1 回归 | `onboarding_ui.rs` / `onboarding_invite.rs` / `dev_sync_*` 全部不改语义 |

## ADR 0017 S5b：App 管理员创建（**ADR 0018 S4b 已移除**）+ 复制邀请链接（2026-11-15）

> **ADR 0018 S4b（2026-11-16）已删除本节的「管理员创建」部分**：App 不再有 admin secret
> 字段/管理员模式，`sync::admin_ticket` 与 `POST /v2/admin/create-tickets` 也一并移除。
> Create 现在只有「粘贴创建链接」（票据来自 Web 管理面「创建领取链接」）。
>
> 本节保留的 **「复制邀请链接」/加入 PENDING/批准** 流程不变（见 §3/§4）。

### 1. 起 dev relay（旧流程，已不用于 Create）

`dev_server`（可 `--setup-only --reset`）仍可用于旧 v1 实例/联调；新的创建票据必须由
`init` + `serve --config` 的内嵌 Web 管理面发行。App 不再接收 relay 管理凭证。

### 2. App：管理员创建（已移除）

不要再按旧步骤填「relay 管理凭证」。Create 表单现在只有：创建链接、实例目录、同意框、
「用一次性票据创建」；链接来自 Web 管理面（见文末 S4b 章节）。

### 3. 复制邀请链接（创建成功后立即可用）

- **备份页**：点「复制邀请链接给其他设备」（`app-root-copy-invite`）→ 生成 v2 邀请链接
  （`mn-pair://join?...&api=2#token=...`，默认 15 分钟）并复制到剪贴板；页面显示公开的
  剩余有效期（`app-root-invite-ttl`）与风险提示。
- **Main 设备面板**：`dev-sync-invite`（生成邀请）→ `dev-sync-copy-invite`（复制链接）。
  ADR 0017 S5b 起**不再要求有活动文档**：刚创建、尚未打开/发布任何笔记的实例也能直接
  生成邀请（邀请是管理操作，只需要 vault + 身份 + 管理私钥）。
- 链接含一次性 token：只私发给要加入的设备；不要截图/公开发布；剪贴板历史可能保留副本。

### 4. 其他设备加入（PENDING → 批准）

1. B 实例「加入已有同步实例」→ 粘贴邀请链接 + 实例目录 → 兑换为 **PENDING**
   （不写配置/身份，token 不落盘）；
2. A 在设备面板看到 pending 行 → 批准（`dev-sync-approve-<短id>`）；
3. B 点「检查状态」→ bootstrap → Main；B 的配置写 `api_version=2`，同步走 `/v2` 数据面。

### 5. 失败矩阵（都对用户明确、零本地残留）

| 情况 | 表现 |
| --- | --- |
| 创建链接被拒绝（票据不存在/过期/已用/错 vault） | 403 `FORBIDDEN`；不建库/身份/配置，票据不保存 |
| 链接已绑定该 vault（已存在） | 409 `ALREADY_INITIALIZED`；不消费票据 |
| v2 创建入口限流 | 429 `RATE_LIMITED`（默认 6 次/分钟）；同一次也可重试，失败不消费 |
| relay 未启动 / 地址错 / TLS 不受信 | 可重试分类或安全暂停；不建库/身份/配置 |
| HTTPS 链接未勾选远程确认 | 表单内联拒绝；不联网、不建目录 |
| 生成邀请无管理钥/无 pin/relay 断 | 备份页/面板显示脱敏原因；不写 token/配置 |

### 6. 本片未做（明确）

- 真实 TLS relay / Windows 真机验收：仍后置；
- 邀请的 relay 收件箱/审批 UI（S4d2）仍未做：批准仍在创建端设备面板完成；
- 票据 TTL/额度自定义、创建链接/邀请链接的 QR/scheme 注册：不做。

### 7. 自动化覆盖（本片）

| 测试 | 覆盖 |
| --- | --- |
| `crates/app/tests/onboarding_admin_create.rs`（**S4b 已删除**） | 随旧「管理员创建」一并移除 |
| `crates/sync/tests/admin_ticket.rs` / `sync::admin_ticket`（**S4b 已删除**） | 随旧管理发票端点一并移除 |
| `crates/app/tests/dev_sync_invite.rs`（改 1） | **无活动文档时邀请按钮不再置灰**，点击确实派发 `CreateInvite`（不再被门禁挡住） |
| `crates/app/tests/onboarding_ui.rs`（S4b 改） | Create 只剩创建链接 + 实例目录；无 admin/legacy/relay/secret selector；真实链接 → 备份页 → Main |
| 既有回归 | `onboarding_create_link.rs`（粘贴创建链接/v2 邀请/票据失败矩阵）与全部 `dev_sync_*` 不改语义 |

## ADR 0018 S4a：内嵌 Web 管理页面（列表/创建领取链接/取消/删除）

> 非生产：admin listener 默认 `127.0.0.1:18081`，HTTP 明文；token/会话/领取链接都可能被
> 同机或同一受信网络上的进程截获。不要映射到公网；跨不可信网络请放在同机 TLS
> 反向代理之后，并把浏览器使用的 `host:port` 写进 `admin_allowed_hosts`。
> 旧 `/v2/admin/create-tickets`、App「管理员创建」、CLI `delete-vault` 在 S4a 当时仍保留；
> **ADR 0018 S4b 已全部移除**（见文末 S4b 章节）。本节其余步骤（Web 登录/创建领取链接/
> 取消/删除）在 S4b 后仍然有效，且是新首次配置的唯一入口。

### 1. 起 relay（带 `public_url`）

1. `cargo run -p menghunote-relay -- init --db /tmp/mn-relay/relay.sqlite3`
   → **只显示一次** `admin_token=…`（另有一枚 setup secret）；记下 admin token。
2. `serve.toml`（示例）：

   ```toml
   listen = "127.0.0.1:18080"
   db = "/tmp/mn-relay/relay.sqlite3"
   admin_listen = "127.0.0.1:18081"
   public_url = "http://127.0.0.1:18080"   # claim 链接的数据面 origin（只来自配置）
   ```

   - 本机开发用 `http://127.0.0.1:18080` / `http://localhost:18080` / `http://[::1]:18080`；
   - 对外/TLS 用 `https://relay.example.com`（建议同机 nginx/Caddy 终止 TLS）；
   - **不要**写 `http://192.168.x.x:18080`：客户端 `local_dev` 只接受 loopback、
     `remote_tls` 只接受 https，relay 启动时会拒绝非 loopback 明文 `public_url`。
3. `cargo run -p menghunote-relay -- serve --config serve.toml`
   → 日志应有 `[relay] listening on http://127.0.0.1:18080`、
   `[relay] admin listening on http://127.0.0.1:18081`、
   `[relay] public_url=http://127.0.0.1:18080`。
   未配置 `public_url` 时只打印一行警告（创建页会 409）。

### 2. 登录与列表

1. 浏览器打开 `http://127.0.0.1:18081/admin/login`，粘贴第 1 步的 admin token。
2. `GET /admin`：显示"数据面 URL"（只读）、vault 列表（id/状态/创建时间/数据面 URL/
   设备数/票据过期/删除进度）；空 relay 显示"空库，尚未创建任何 vault"。
3. 点 vault id 进详情：生命周期与授权/运行状态分小节；不显示票据 hash/明文。

### 3. 创建领取链接（一次性）

1. 点「创建领取链接」→ 表单只有 CSRF（没有 vault_id/TTL/quota 输入）→「创建领取链接」。
2. POST 后 303 到 `/admin/vaults/created/<随机 id>`，页面显示**一次**：
   `vault id`、创建时间、票据过期（+900s）、`mn-pair://create?v=1&relay=…&vault=…#ticket=…`。
3. **刷新**该页（或后退再进）→ 404「一次性页面已失效」；列表/详情都不会再出现链接/ticket。
4. 把链接私发给要创建实例的设备（App 粘贴创建链接）；链接含一次性票据明文，不要截图/进工单。
5. 链接丢失：回列表/详情取消该预留（`RESERVED`），再重新创建（旧 vault id 不能复用）。

### 4. 取消预留

- 详情页 `RESERVED` →「取消预留」→ 303 回列表；状态 `CANCELLED`，票据作废。
- 重复取消幂等（仍回列表）；已经领取（`ACTIVE`）→ 409（绝不删除已创建 vault）。
- 票据过期后取消 → 409，registry 落 `EXPIRED`（票据作废，id 不可复用）。

### 5. 不可逆删除（202 / 进度）

1. 详情页 `ACTIVE` →「删除该 vault」→ 确认页逐字输入 `DELETE <32 位小写 hex>`
   （大小写、单个空格都严格；错/缺 CSRF 分别 400/403，不改变任何状态）。
2. 提交后返回 **202** 作业页（状态 `DELETING`、job id、进度 `0/26`）：只表示"已持久标记"，
   **不代表已完成**；数据面入口立即 410 `VAULT_DELETED`。
3. GET `/admin/vaults/<id>/delete`（或详情页链接）看进度：无 JS，靠
   `<meta http-equiv="refresh" content="3">` + "刷新本页"链接；`DELETE_FAILED` 可重试同一 job。
4. 完成前**不要**停 relay；若必须停，重启会自动续跑 `DELETING`（启动先加载 fence 再监听）。
5. 完成后状态变 `DELETED`（墓碑）：租户表/票据零残留，列表进度 `26/26`；
   页面没有恢复/回滚按钮，id 永久禁止复用。

### 6. 旧 App/CLI 入口（ADR 0018 S4b 已移除）

- App 的「管理员创建」/ admin secret 字段、`sync::admin_ticket` 与
  `POST /v2/admin/create-tickets`：**已删除**；
- 主 CLI `delete-vault` 与示例 `--issue-ticket`：**已删除**；
- App Create 只保留「粘贴创建链接」；首次配置的唯一入口是本节 Web 管理面。

## ADR 0018 S4b：首次配置（Web 管理面创建 claim 链接 → App 粘贴）

> ADR 0018 S4b（2026-11-16）：移除旧管理入口/App 管理员模式/CLI 发票与删库。保留的入口是
> **Web 管理面**（列表/创建领取链接/取消/删除）与 App 的**粘贴创建链接**。不涉及 schema/
> fence/Deletion 语义；claim/cancel/删除与 S2/S3 完全同一套。

### 1. relay 初始化（只一次）

```bash
cargo run -p menghunote-relay -- init --db /tmp/mn-relay/relay.sqlite3
# stdout 只显示一次：
#   setup_secret=…（v1 旧实例/CLI 兼容用；App Create 已不使用）
#   admin_token=…（Web 管理登录用；只显示一次）
```

### 2. 起 relay（数据面 + admin listener）

```toml
# serve.toml
listen = "127.0.0.1:18080"
db = "/tmp/mn-relay/relay.sqlite3"
admin_listen = "127.0.0.1:18081"
public_url = "http://127.0.0.1:18080"   # claim 链接的 canonical 数据面 origin（只来自配置）
```

```bash
cargo run -p menghunote-relay -- serve --config serve.toml
# 期望：[relay] listening on http://127.0.0.1:18080
#       [relay] admin listening on http://127.0.0.1:18081
#       [relay] public_url=http://127.0.0.1:18080
```

- `status` 是**只读诊断**：`cargo run -p menghunote-relay -- status --db /tmp/mn-relay/relay.sqlite3`；
  不创建库、不迁移；schema 版本不匹配会明确报错。
- **没有** `delete-vault` 子命令；实例重置 = 停 relay + 备份库文件 + 手工删除
  `<db>`（含 `-wal`/`-shm`），或在 Web 管理面按 vault 在线删除（不可逆，其它租户继续服务）。

### 3. Web 管理面创建 claim 领取链接

1. 浏览器打开 `http://127.0.0.1:18081/admin/login`，粘贴 `init` 只显示一次的 `admin_token`；
2. `GET /admin` 列表 →「创建领取链接」→ 自动生成 vault id（ellipse 900s、quota=1）；
3. 复制页面**只显示一次**的 `mn-pair://create?v=1&relay=…&vault=…#ticket=…`；
   刷新/后退即失效；丢失只能取消预留后重建（旧 vault id 不能复用）。

### 4. App 粘贴创建链接

1. `cargo run -p menghunote-app --features dev-sync`（无需 env/flag）；
2. Welcome →「创建新的同步实例」或「粘贴创建链接」→ 粘贴整段链接 →
   「应用创建链接」（输入框按 Enter 等价）；
3. 表单应只剩：创建链接、`app-root-create-link-warning` / `-facts`、实例目录、同意框、
   「用一次性票据创建」；**没有** admin secret / v1 setup secret / relay 模式选择器；
4. 勾选同意 →「用一次性票据创建」→ Working → 恢复码备份页（只显示一次）→ 勾选备份 →
   Main。配置写 `api_version=2`；链接为 `https://` 时还需勾选远程确认。

### 5. 之后

- 其它设备：A 的备份页/设备面板「复制邀请链接」→ B 粘贴 → PENDING → A 批准 → B
  「检查状态」→ Main（流程不变，见 §4）；
- 删除：Web 管理面详情 → 逐字输入 `DELETE <32 位 hex>` → 202 作业；数据面立即 410
  `VAULT_DELETED`；作业独立于浏览器，断连/重启自动续跑；
- 失败文案：链接过期/已用/错误 vault/无 `public_url` 都由 Web 端或 App 票据错误分类给出；
  App **不显示**任何 admin 凭证概念。

### 6. 自动化覆盖（S4b）

- relay：`cargo test -p menghunote-relay --all-targets`（旧 `admin_ticket_v2` 用例删除；
  新增 `status` 只读/不建库/schema 不兼容拒绝的进程级用例；admin web/删除/claim 全绿）；
- sync：`cargo test -p menghunote-sync --all-targets`（`admin_ticket` 模块/测试删除）；
- app：`cargo test -p menghunote-app --features dev-sync` / 默认（`onboarding_admin_create`
  删除；`onboarding_ui` Create 用例改走创建链接；`onboarding_create_link` 断言无 Admin 入口）。
