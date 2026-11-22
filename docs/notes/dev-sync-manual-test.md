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

前提：`cargo run -p menghunote-app --features dev-sync`（不带任何 `--dev-sync*`、不设
`MENGHUNOTE_DEV_VAULT`），relay 已按 §2.1 起好（`--setup-only`）。

1. **A 创建实例**：Welcome →「创建新的同步实例」→ 填 relay（默认
   `http://127.0.0.1:18080`）、setup secret（relay 打印的那串，粘贴即可）、实例目录
   （默认在 `%LOCALAPPDATA%` / `$XDG_DATA_HOME` 的 `menghunote-dev/instances/<id>/`，
   可改成任意**绝对路径**，且必须不存在或为空目录）→ 勾选「同意开发同步」→「创建」；
2. **A 备份恢复码**：创建成功后停在备份页，**只显示一次** `vault_id` + 恢复码；
   勾选「已在应用之外备份恢复码」→「继续进入主界面」。恢复码拥有数据/管理恢复能力，
   离开本页不再显示（想要自己抄一份，用「复制恢复码（有风险）」并清剪贴板）；
3. **A 建笔记并发布基线**：侧栏「＋ 笔记」建第一篇、输入正文并等自动保存 → 面板
   「发布基线」（§2.4）。**B 加入前必须先发布**（需要两份 checkpoint）；
4. **B 加入**：另开一个进程（同一个二进制，同样不带 env/flag）→ Welcome →「加入已有
   同步实例」→ 填：
   - relay：与 A 相同（只允许 loopback 明文）；
   - `vault_id`：A 备份页上的 32 位十六进制（公开标识，不遮罩）；
   - 恢复码：A 备份页上的 `mnrec1…`（**遮罩**；可 `Ctrl+V` / `Shift+Insert` 粘贴，
     `Ctrl+A` 全选，`Tab`/`Shift+Tab` 切字段，**不支持复制**）；
   - 实例目录：默认自动生成，必须不存在或为空目录（与 Create 共用校验与实例锁）；
   →「加入」。进度页显示固定阶段文案，UI 不冻结；
5. **B 进入 Main**：成功即进入真实主界面并**自动打开第一篇**（A 的笔记，正文来自基线），
   **不播种 demo**；公开配置 `dev-sync.json` 的 `active_instance` 指向 B；
6. 之后 B 想再打开：直接启动程序 → 有有效配置 + 身份 → **直接进 Main**（不闪 Welcome）。

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

- 仓库根目录执行；分支/HEAD 带 `--features dev-sync`（默认构建**不含**同步面板）。
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

### 2.4 A：在窗口里点「发布基线」（推荐）

A 建好笔记后**不用关窗口、不用命令行**：在面板上直接点「发布基线」。

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
轮换入口是下一片（Phase 2），本片**不提供**。

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
- **没有自动同步/自动发布/轮换**；轮换与 metadata 对账是 Phase 2。
- **移除设备 = 只断访问**（ADR 0010）：不轮换 root/epoch、不撤销管理材料、不能追回旧数据；
  本片**没有轮换入口**，「轮换密钥」是独立动作、需要显式调用。
- **pending 设备不能在这里移除**：没有移除按钮（relay 对 pending 返回 `409 INVALID_STATE`），
  取消待批准设备属于后续切片。
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
> （headless 用 debug 钩子模拟组合）。轮换（`key_epoch`）与 metadata 事件认证/对账
> （task-121…124）尚未接线，自动同步不触发它们。
