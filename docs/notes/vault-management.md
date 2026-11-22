# vault 新建 / 切换 / 路径选择（产品级）

> 状态：实现完成（headless 验证；Windows 真机观感未验）。
> 相关：[ADR 0011](../adr/0011-sync-onboarding-ui.md)、
> [startup-resume-settings-memory](startup-resume-settings-memory.md)。

## 1. 问题

用户要求的原话：

- 软件需要**新建 vault**、可以**切换 vault**；
- 以后测试多设备同步**不要设置 `LOCALAPPDATA`**；
- **不要直接保存到 `LOCALAPPDATA`**，允许用户选择位置、允许切换。

旧实现的两个默认值都落在应用数据目录：

- Create/Join 的实例目录默认 `instances_dir/<id>`（Windows = `%LOCALAPPDATA%\menghunote\instances`）；
- 本地（不同步）库默认 `<数据根>/local/vault.sqlite3`（同样在 LOCALAPPDATA/XDG data 下）；
- 设置里只能「设为默认 + 重启后生效」，不能真正切换。

## 2. 默认位置（用户可见）

| 用途 | Windows | Linux/macOS |
| --- | --- | --- |
| Create/Join 实例目录 | `%USERPROFILE%\Documents\menghunote\instances\<instance_id>` | `$HOME/Documents/menghunote/instances/<instance_id>` |
| 本地（不同步）库 | `%USERPROFILE%\Documents\menghunote\local\vault.sqlite3` | `$HOME/Documents/menghunote/local/vault.sqlite3` |
| 公开配置 `sync.json` | `%LOCALAPPDATA%\menghunote\sync.json`（不变） | `$XDG_CONFIG_HOME/menghunote/sync.json`（不变） |

- `Documents` 不存在 → 回退到 `%USERPROFILE%\menghunote` / `$HOME/menghunote`；
- 只是**默认值**：表单里有「浏览…」（GPUI `prompt_for_paths`，目录选择器），
  提交时把用户确认的绝对路径写进公开配置的 `vault_path`；
- 校验不变：实例目录仍是**绝对路径 + 不存在或空目录**（`validate_instance_directory`），
  已有库/身份/其它文件一律拒绝，不覆盖、不迁移；
- 旧配置里的 `vault_path` **原样保留**：不迁移、不改写、不移动库文件。

实现位置：`crates/app/src/onboarding/layout.rs`
（`ProfileLayout::{default_vault_root,default_instance_dir,default_local_vault_path}`）。
显式注入布局（`ProfileLayout::new`，测试/第三方）默认根仍为数据根，**不读平台 env**；
平台默认布局（`ProfileLayout::discover`）才使用用户文档目录。

## 3. 切换实例（不重启）

设置 →「实例」每行现在有两个动作：

- 「设为默认」：只改 `sync.json` 的 `active_instance`，重启后生效（原行为不变）；
- 「打开」：**立即切换**，不需要重启。

「打开」的顺序（`AppRoot::settings_open_instance`）：

1. 目标就是当前实例 → **no-op** + 设置页提示，不重建 `AppShell`；
2. `AppShell::close_all`（当前笔记有界排空/flush，dev-sync 只请求停止 worker，不 join）；
3. drop 旧 `AppShell`，释放旧 `InstanceLock`（Drop 删除锁文件）；
4. 关闭设置覆盖层；
5. 打开目标实例：成功 → `active_instance` = 目标 + `startup.json` 记 `sync`；
   失败 → 既有 Error 页（可「重试打开」或「返回欢迎页」）。

**绝不**把当前同步 worker 热切到另一个 relay：切换就是关旧开新，任一时刻最多一个
`AppShell`/worker。实例目录被其它进程占用时也进 Error 页（重试路径同 Welcome 的打开）。

## 4. 返回 Welcome / 新建 / 加入

设置 →「实例」新增三个入口：

| selector | 行为 |
| --- | --- |
| `settings-back-welcome` | `close_all` + 释放锁 → Welcome；`sync.json`/实例列表不动，本地模式提示清掉 |
| `settings-new-sync-vault` | 关闭 Main + 释放锁 → Welcome → 直接进 Create 表单（粘贴创建链接） |
| `settings-join-sync-vault` | 关闭 Main + 释放锁 → Welcome → 直接进 Join 表单 |

Welcome 原有的 Create/Join/本地入口全部保留。

## 5. 本地（不同步）vault 可选路径

Welcome 的本地区块（产品构建）新增：

- 路径输入框 `app-root-local-path`（默认用户文档目录）；
- 「浏览…」`app-root-local-browse`（目录选择器 → 自动补 `vault.sqlite3`）；
- 入口按钮 `app-root-local` 文案随文件是否存在变化：
  - 不存在 →「新建本地 vault」（`dev_vault::open_local_vault_at` 建空库，**不播种 demo**）；
  - 存在 →「打开本地 vault」（复用，不重置内容）。

成功后写 `startup.json`：

```json
{ "version": 1, "mode": "local", "local_vault_path": "/home/you/Documents/menghunote/local/vault.sqlite3" }
```

- `local_vault_path` 是**可选扩展字段**：旧文件 `{version,mode}` 仍可读；
- 旧格式 + 旧数据根下 `local/vault.sqlite3` 仍存在时，启动优先打开旧库（升级后不"丢库"），
  并把实际路径补写进新格式；
- 打开同步实例只换 `mode`，**保留** `local_vault_path`，返回本地入口时仍显示上次位置；
- `sync.json` 仍只存公开定位信息，不写正文/秘密。

## 6. 固定 selector

新增：

`settings-instance-<i>-open`、`settings-back-welcome`、`settings-new-sync-vault`、
`settings-join-sync-vault`、`app-root-dir-browse`、`app-root-join-dir-browse`、
`app-root-local-path`、`app-root-local-browse`。

既有 selector（`app-root-local`、`app-root-create-form`、`app-root-join-form`、
`settings-instance-<i>`/`-active` 等）保持不变。

## 7. 测试

`crates/app/tests/headless_vault_switch.rs`：

1. 设置实例列表「打开」另一个实例 → 旧 shell 换新、旧锁释放/新锁持有、目标库内容可见、
   active/startup 切到目标；
2. 点当前实例「打开」= no-op（同一 `AppShell` 实体、锁不释放、设置页不关）；
3. `settings-back-welcome` → Welcome + 无 `AppShell` + 锁释放 + 实例列表仍在；
4. `settings-new-sync-vault` / `settings-join-sync-vault` → 关 Main 后进入对应表单；
5. `discover` 布局下 Create/Join 默认目录在用户文档目录（不含 appdata），且可改成任意
   绝对路径；
6. 本地库自定义路径：不存在则新建空库、写 `startup.json`、重启恢复到同一路径；
7. 旧 `startup.json`（无 `local_vault_path`）继续打开旧数据根本地库；
8. 平台默认布局的默认 vault 根在 `Documents/menghunote`，配置仍在平台配置目录。

此外 `layout.rs` 单测覆盖：Documents 存在/不存在、Windows `USERPROFILE` 优先、
`startup.json` 新旧格式往返、显式布局默认根不变。

## 8. 测试计数（2026-11-20，Linux，`-j 8`）

| 测试 | 结果 |
| --- | --- |
| `cargo test -p menghunote-app`（默认 `sync-ui`） | **605 passed / 0 failed**（含 `headless_vault_switch` 9） |
| `cargo test -p menghunote-app --features dev-sync` | **754 passed / 0 failed**（`headless_vault_switch` 7；2 个本地用例仅产品构建编译） |
| `cargo test -p menghunote-app --no-default-features` | **470 passed / 0 failed** |
| `cargo test -p menghunote-app --lib onboarding` | 53 passed |

## 9. 已知限制 / 风险

- headless 无法弹真实系统对话框：测试直接调 `form_set_dir` / `local_set_path`；
  真机「浏览…」行为（GTK/Windows 对话框、取消、多屏）仍需 Windows/Linux 真机验。
- 「打开」是同步关闭+打开（本地 IO），大库首帧可能短暂停顿；与既有「打开实例」
  同一路径，未引入后台 Opening。
- 切换时 `AppShell::close_all` 只"请求停止"同步 worker（不 join），与关窗语义一致；
  旧 worker 在请求后自行收尾，但旧 `AppShell`/界面已经 drop，不会同时存在两个界面。
- 本地库仍不取实例锁；同一本地库被两个窗口/进程同时打开的风险与以前相同。
- 不迁移旧 `%LOCALAPPDATA%` 下的库：旧配置 `vault_path` 原样保留，用户可在 Welcome
  重新打开；本地库按第 5 节的旧路径兼容逻辑继续打开。
