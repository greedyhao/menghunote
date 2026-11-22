# ADR 0011 — 同步初始化/配置的 UI 向导（AppRoot 根视图）

> 状态：**已决**（2026-11-13，GPT 决策；分 S1–S3 实施）。
> 关联：[ADR 0006](0006-sync-v1.md)、[ADR 0008](0008-linux-local-identity-wrap.md)、
> [ADR 0009](0009-sync-ui-integration.md)（dev 面板/持闸同步）、[ADR 0010](0010-device-access-revocation.md)。
> 用户要求：真实用户不应使用命令行/环境变量完成同步初始化与配置。

## 背景

Phase 1 的 Create/Join/Publish 是 CLI（`--dev-sync-create/join/publish` + env secrets），
用于先把同步引擎接进真实编辑器。真实用户不可能这样操作；需要 UI 向导完成：
选择本地实例目录、配置 relay、创建或加入 vault、一次性展示并确认恢复码、进入主界面、
建首篇笔记、发布基线。

## 决策：B′ — 同窗常驻 `AppRoot`，根视图切换

- 一个常驻根视图 `AppRoot`，内部状态：`Setup / Opening / Main`；只有**真实 Vault 与持久身份
  就绪**后才构造 `AppShell`。禁止占位库、假 peer、先开空壳再补身份。
- 不采用“独立 setup 窗口”（多窗交接/关闭竞态无收益）；GPUI 的 `replace_root` 足够，
  但常驻根更简单。
- 定位仍是**开发同步**：明确 localhost/非生产提示，不包装成已完成的跨设备安全同步。
- 默认构建（无 `dev-sync` feature）行为不变；feature-on 且无选定配置 → Welcome，可
  “继续本地”或显式进入开发同步；已保存且有效的 profile 即运行时 opt-in，无需 env/flag。

## 生命周期与后台

状态草图：

```text
Welcome -> CreateForm | JoinForm | Existing
        -> Working -> (Create) BackupRequired -> Opening -> Main
                     (Join)                     Opening -> Main
任意 -> Error（保留表单与公开字段）
```

API 草图：

```rust
submit(OwnedSetupRequest, OpId) -> Progress | Committed { vault, config, sync_context, recovery_once }
                                 | TypedFailure
```

- 网络、Argon2、crypto、身份落盘、配置写入全部在**专用线程**；UI 非阻塞接收。
- worker 从持久身份派生 peer/epoch，再交出真实 `Vault`（`Vault` 已 `Send + Sync`）。
- Working 中关闭请求先拦截说明；只允许“停止后续自动进入”，必须等真实收尾。
- 迟到的成功不能丢：Create 成功仍必须展示恢复码；GUI 不 `join` 线程。
- 沿用操作预算（120s）与连接/IO/请求超时，但必须真正传入 Create/Join（当前
  `DevLocal` 自建 transport，不能假定 `ServiceConfig` 已生效）。逻辑超时不等于停止；
  终态前禁止重试。

## 数据目录与公开配置

- **Windows**：数据 `%LOCALAPPDATA%\menghunote-dev\instances\<local_id>\`；配置
  `%LOCALAPPDATA%\menghunote-dev\dev-sync.json`（不用 Roaming）。
- **Linux**：数据 `$XDG_DATA_HOME/menghunote-dev/instances/<local_id>/`；配置
  `$XDG_CONFIG_HOME/menghunote-dev/dev-sync.json`；分别回退 `~/.local/share`、`~/.config`。
  身份主钥仍遵守 ADR 0008，不迁移。
- 自动生成本机 `instance_id`（与服务器 `vault_id` 分离）；目录内固定
  `vault.sqlite3`、`dev-identity.bin`。只有测试注入 `target/tmp`；真实使用不回退 cwd/tmp。
- MVP **不引入 `rfd`**：默认路径 + 可编辑绝对目录即可；路径解析/权限/锁集中在平台模块。
- 配置草案（只存公开定位信息，**绝不存** setup secret、恢复码、根或私钥）：
  ```json
  { "version": 1, "active_instance": "...",
    "instances": [{ "instance_id": "...", "relay_url": "...", "vault_path": "...",
                    "vault_id": "...", "device_id": "..." }] }
  ```
- 每目录持有进程级 OS 独占锁；GUI/CLI 共用，覆盖 worker 真实生命周期；同目录冲突返回
  实例选择页，不删除锁文件。不同目录允许 A/B 两进程。
- 配置 ID 只是索引；缺失/坏配置/不可解身份/库事实错配 → 错误页，不静默重建、不打开 demo。

## 表单、恢复码与敏感输入

- **Create**：relay（默认 `http://127.0.0.1:18080`）、实例目录、服务器创建凭证
  `setup secret`、显式开发同步同意。
- **Join**：relay、全新实例目录、32 位十六进制 `vault_id`、恢复码；复用
  `RecoverySeed::from_code`，错误不回显输入。
- `setup secret` **不是笔记库密码**；当前 dev-local 不接收用户口令，不要添加无加密语义的
  “密码”字段。
- Create 仅在**受保护身份提交成功**后展示 `vault_id` + 恢复码，明确其数据/管理恢复能力；
  勾选“已在应用之外备份”后才能继续；不上传、不持久化、不在 Existing 重显。
- 提炼小型 Plain/Secret 输入组件，复用坐标/IME 算法；不要复用 `RenameState` 的
  `Debug/Clone` 存秘密。
- 必须有 `EntityInputHandler`、Tab/Shift+Tab、全选、Ctrl+V/Shift+Insert、正确 UTF-16
  选区/组合；组合中 Enter 只上屏、Esc 只取消组合。秘密输入遮罩、禁复制/剪切。
- Request/Outcome/缓冲用 `SecretText/Zeroizing`；事件与 `Debug` 不携带正文；日志/panic/
  遥测禁止秘密；错误按类型脱敏，禁止直接显示 `{error:#}` 或被拒 URL。
- 恢复码复制仅备份页显式动作并提示风险；退出页面清空应用持有内容。零化无法保证
  GPUI 临时副本/IME/系统剪贴板历史不留痕，不作绝对承诺。

## 首篇笔记与发布

- Creator 进入真实空树（不播种 demo），空态提供「新建笔记」；用户创建、编辑并 durable
  保存后，使用现有同步面板「发布基线」（publish-ui-2b 的持闸/后台发布入口）。
- 向导不另造发布实现，也不再索取恢复码（用受保护的 creator 管理身份）。
- Join 的 `NO_VALID_BASELINE` 提示“请创建者先发布两份基线”，保留公开字段、允许重输恢复码
  重试；成功后展示真实 bootstrap 内容，由 `AppShell` 自动打开首篇。

## 门禁与兼容

- CLI Info/Publish 保持不开窗；Create/Join 保留自动化兼容；显式 CLI 参数只覆盖本次运行，
  不静默改 GUI profile。GUI 不从环境自动填充秘密。
- MVP 单进程、单可见窗口、单活动 Vault；启动时可选多个实例。Main 设置提供配置入口；
  更换/新增实例先要求重启进入选择页，暂不做热切换。

## 实施切片

- **S1 入口 + 打开已有实例**：常驻根、路径默认值、公开配置、目录锁、真实身份加载、错误页；
  默认构建与旧 CLI 回归。
- **S2 创建 + 备份 + 首篇**：真实 genesis → 身份落盘 → 一次备份确认 → 空树 → 新建保存；
  与发布按钮联验，含配置保存失败与迟到成功。
- **S3 加入 + 失败恢复**：真实 Join/bootstrap → 首篇打开；错码、离线、未发布、目录占用、
  取消/关闭、重启重开与 A/B 闭环。
- **S4 设置 UI**：Welcome/Main 都能打开设置，查看公开配置、切换 active、修改 active 的
  relay；只写公开配置，明确「重启后生效」，不热切换 Vault/worker。
- Headless 用真实 `TestAppContext` + 生产 Vault/身份路径；等待明确终态/DurableAck，
  不以 sleep/“队列空”收敛；测试目录、XDG、超时显式注入。
- 每个可见切片立即 Windows 真机验：中文/空格路径、DPAPI/跨用户失败、IME 候选窗与遮罩、
  粘贴、Tab、DPI、单窗切换、Working 关窗。

## 非目标与风险

- 不改 wire/SQLite/身份封装格式；不迁移旧开发库；不导入目录克隆身份；不做账户密码、
  轮换、成员管理、全库本地加密。
- Create/Join 失败只清本次新建文件，绝不删用户目录；远端 genesis/enrollment 可能已生效，
  取消不是跨端事务回滚。硬杀发生在备份前无法补显恢复码，保留成功实例并明确说明。
- 产品化另验 TLS/真实服务器鉴权、非 dev 凭证 onboarding、Linux 安全存钥、崩溃策略与
  恢复/迁移/轮换；DevRelay 跳签名的测试不能证明生产鉴权，SQLite 正文仍非本地加密。
- 回滚只关闭 GUI onboarding/运行时入口，保留实例与身份，继续 CLI；不改 `v0.4-sync`。

## 实施记录（S1+S2，2026-11-13）

代码位置：`crates/app/src/onboarding/`（`layout` 公开目录/配置、`lock` 实例锁、`create`
后台 Create、`input` 自绘输入、`app_root` 常驻根视图）+ `crates/app/src/main.rs` 的第三条
启动路径。S1+S2 时 Join 向导尚未实现（S3 完成，见下一节），CLI `--dev-sync-join` 继续可用。

- **入口条件**：`--features dev-sync` 且**没有**显式 `--dev-sync*` / `MENGHUNOTE_DEV_SYNC=1`
  → 先开窗、根视图 `AppRoot`（不再要求 `MENGHUNOTE_DEV_VAULT`）；显式 CLI/env 仍走 ADR 0009
  的启动生命周期（自动化/测试兼容）；默认构建（无 feature）行为不变。
- **状态**：`Welcome → CreateForm → Working → BackupRequired → Main`，任意状态可进 Error。
  `AppRoot::new` 在公开配置 active 有效时**同步**打开实例并直接进 Main（不闪 Welcome）；
  打开实例不重建、不迁移、不打开 demo。
- **目录/配置**：Windows `%LOCALAPPDATA%\menghunote-dev\{dev-sync.json,instances\<id>\}`；
  Linux `$XDG_DATA_HOME/menghunote-dev/instances/<id>/` + `$XDG_CONFIG_HOME/menghunote-dev/dev-sync.json`
  （分别回退 `~/.local/share`、`~/.config`；HOME 缺失报错，不回退 cwd/tmp）。配置只含公开
  字段（`version/active_instance/instances[{instance_id,relay_url,vault_path,vault_id,device_id}]`），
  原子写（同目录临时文件 + write/fsync + rename + 父目录 fsync）。写失败留在 Error 页并提示
  「重试保存」，**不重跑 Create/Join**（重跑会生成新实例、新恢复码）。
- **实例锁（MVP 局限，必须写清）**：现有依赖里没有跨平台文件锁 crate，S1 用
  `<实例目录>/.dev-sync-instance.lock` + `create_new` + `pid/created` 内容实现。Drop 只删除
  "内容仍是本进程写入"的锁文件；**不删除实例目录**、不偷别人的锁。进程被硬杀时锁文件会
  残留，需要人工删除（错误文案给出提示），不会自动清理。**同目录冲突不是"实例损坏"**：
  `AppRoot` 回到 Welcome（实例选择页）并给明确原因，释放后再点同一实例即可进入 Main。
  产品化时应换成真正的 OS 文件锁（`fd-lock`/`fs2`），本模块的 API 形状
  （`InstanceLock::acquire` + `Drop`）不需要变。
- **Create 后台**：专用线程执行 `DevLocal::create`（genesis + 初始 recovery 槽上传）→
  `DevIdentityStore::persist_protected` → channel 交回 `Vault`/公开事实/一次性恢复码；UI 用
  40ms 非阻塞泵事件（不 join 线程）。`Working` 状态关窗**被拦截**；迟到的成功仍进入备份页
  （恢复码不能丢）。UI 侧的 120s 操作预算只是**逻辑超时提示**（超过后提示"仍在等真实
  收尾"），不中断网络/写盘；终态前禁止重复提交。备份页只在身份落盘成功后展示 `vault_id`
  + 恢复码（`Zeroizing` 缓冲），
  勾选「已在应用之外备份」才写配置 active，然后进**真实空树** Main（不播种 demo、不自动发布）。
- **错误页**：配置损坏/版本不符/身份缺失/库与配置错配 → Error，保留公开字段
  （实例 id / 路径 / relay / vault_id）+ 脱敏文案 + 明确下一步（重读配置 / 重试打开 /
  重试保存 / 回表单）。所有展示文案经 `onboarding::sanitize_error`（替换 setup secret、遮住
  恢复码 token、截断超长错误），不直接显示 `{error:#}` 原文。
- **测试**：`crates/app/tests/onboarding_ui.rs`（真实进程内 dev relay + 真实 Vault/身份路径：
  Welcome、Create→Working 关窗拦截→备份页→空树 Main→重启自动 Main、坏配置/缺身份/
  `vault_id` 错配）+ `onboarding::{layout,lock,mod}` 单测。收敛点只有
  `wait_for_create_terminal`（阻塞在 channel 终态，120s 上限）与 `run_until_parked`，**无 sleep**。
- **已知缺口**：① Join 向导（S3）；② 秘密输入是**简化遮罩**：渲染 `•`、`text_for_range`/
  `marked_text_range` 不把正文交给平台、禁复制，但秘密字段里的 IME 组合提交未完善（setup
  secret 的真实操作路径是 ASCII 键入或粘贴），完整 `EntityInputHandler` 秘密输入与恢复码
  输入在 S3 补齐；③ 打开已有实例是同步路径（本地 IO 很快，大库迁移会短暂阻塞窗口）；
  ④ 真实用户仍拿不到生产凭证 onboarding（dev genesis 的 setup secret 由 relay 部署方提供）。

## 实施记录（S3：Join + 失败恢复，2026-11-13）

代码增量：`crates/app/src/onboarding/join.rs`（Join 后台 worker + 失败分类）、
`onboarding/input.rs`（`Shift+Insert` 粘贴）、`onboarding/app_root.rs`（`JoinForm` 状态/
渲染/事件泵/配置重试）、`crates/app/tests/onboarding_ui.rs`（真实 relay A→B 闭环与失败
矩阵）。**S3 之后真实用户不再需要 `--dev-sync-join`**：CLI 只保留给自动化/复现。

- **Join 生命周期**：`Welcome → JoinForm → Working → Main`。表单字段：relay（默认
  `http://127.0.0.1:18080`）、`vault_id`（32 位十六进制，公开标识、不遮罩）、恢复码
  （`mnrec1…`，**遮罩**、禁复制、允许 `Ctrl+V`/`Shift+Insert` 粘贴）、实例目录（默认
  自动生成、可改成绝对路径；与 Create 共用"必须尚不存在或空目录"校验与实例锁）。
  表单明确写「本机没有这个设备身份，这是加入——会用恢复码自批一个普通设备身份，
  不会覆盖创建者」+ DEV ONLY/localhost 横幅。
- **后台顺序**（与 Create 同构，见 `join.rs` 模块文档）：专用线程执行
  `DevLocal::join`（恢复码 → 管理信任锚 → 自批设备 → `/bootstrap` 导入基线）→
  `DevIdentityStore::persist_protected` → channel 交回真实 `Vault` + 公开事实。UI 走同一个
  40ms 非阻塞泵（不 join 线程）；成功**没有备份页**（恢复码是用户自己输入的），直接写
  公开配置 active 并进 Main；`AppShell` 构造期自动打开第一篇（A 的笔记），**不播种 demo**。
- **失败分类**（`join::JoinFailureKind`，全部可重试；文案经 `sanitize_error` 脱敏）：
  `RecoveryCodeFormat`（Bech32m/长度非法，**不联网**）、`Credentials`（relay 拒绝：
  `MANAGEMENT_REQUIRED`/`SIGNATURE_INVALID`/`UNAUTHORIZED`…）、`NoBaseline`
  （`NO_VALID_BASELINE`、404、`NOT_FOUND`、无 recovery 槽、vault 不符 → 提示"请创建者先在
  面板发布初始基线"）、`Unreachable`（连接被拒/超时/5xx）、`Other`。失败**回到 Join 表单**
  并保留 relay/vault_id/目录，内联显示脱敏错误；恢复码输入框在提交时立即清空（需重输）。
- **取消/关窗**：`Working` 关窗仍被拦截；取消只表达"等真实收尾"。Join 迟到的成功**仍必须
  写配置并进 Main**（不能丢实例）；取消后 worker 才失败则回 Welcome 并说明"未完成"。
- **配置写失败**：进入 Error（`RetryKind::SaveProfile`），`PendingJoin` 持有已落盘的
  `Vault`/身份；「重试保存」**只重写配置**，不重新 Join（重跑会再登记一台设备身份）。
- **目录与锁**：`validate_instance_directory`（Create/Join 共用）要求绝对路径、尚不存在或
  空目录、无 `vault.sqlite3`/`dev-identity.bin`、无其它文件（只忽略本进程锁文件形态，
  真正冲突由 `InstanceLock` 报"另一个进程占用"）。失败只清本次新建的库/身份文件，
  **绝不删用户目录、不偷别人的锁**。
- **失败清理**：网络/认证失败不建本地文件；`join` 内部失败会清库文件，worker 在身份落盘
  失败时再清一次（含 WAL/SHM/journal）；bar 目录保留为空（可能本来就是用户目录）。
- **测试**（`onboarding_ui.rs`，真实进程内 dev relay + 真实 Vault/身份路径，**无 sleep**）：
  ① A（库函数 Create + 建笔记 + 发布基线）→ B `JoinForm`（真实点击进入）→ Working
  关窗拦截 + 取消请求 → 迟到成功进 Main、`dev_sync_current_text` 读到 A 的正文、树里只有
  A 的一篇（不播种 demo）、配置 active 写入、重启 AppRoot 直接 Main；② 错格式 + 错内容
  恢复码 → 回表单、公开字段保留、错误脱敏、改对后成功；③ A 未发布基线 → 提示
  「发布基线」、不进 Main、不写配置、不留库/身份；④ 目标目录已有库/身份、被占锁 →
  明确错误且不覆盖，释放后可重试；⑤ relay 不可达（临时占用后释放的 loopback 端口）→
  可重试且不建文件，改回可达 relay 后成功；⑥ 配置写失败 → Error →「重试保存」进 Main，
  并用 `DevLocal::members` 断言 relay 上**仍只有 2 台设备**（证明没有重跑 Join）；
  ⑦ 恢复码粘贴：真实剪贴板 + `simulate_keystrokes("shift-insert")` / `("ctrl-v")` 都能
  贴进遮罩框，粘贴后 Join 成功且公开配置里没有恢复码正文。
  收敛点：`wait_for_create_terminal` / `wait_for_join_terminal`（阻塞在 worker 终态
  channel，120s 上限）+ `run_until_parked`。
- **秘密输入**：恢复码与 setup secret 都走同一个遮罩控件：渲染 `•`、`text_for_range` /
  `marked_text_range` 不向平台报告正文、禁复制/剪切、允许粘贴（`Ctrl+V` /
  `Shift+Insert`）、`Ctrl+A`、`Tab`/`Shift+Tab`；`FormInput` 不实现 `Debug`/`Clone`，
  请求/产物（`JoinRequest`/`JoinOutcome`/`PendingJoin`）也不实现——正文没有 Debug 输出路径。
- **已知缺口（必须在产品化前补）**：
  1. 秘密字段的 **IME 组合提交**仍未完成（不报告 marked range 导致输入法无法正确上屏）；
     setup secret/恢复码的真实路径是 ASCII 键入或粘贴。完整 `EntityInputHandler`（组合中
     Enter 上屏、Esc 取消、UTF-16 选区）留给后续切片；
  2. `DevLocal::create/join` 自建 transport，`ServiceConfig` 的超时/deadline 不会传进去
     （S1 已记录）；120s 只是逻辑提示，不中断网络/写盘；
  3. 打开已有实例仍是同步路径（大库迁移会短暂阻塞窗口）；
  4. 真实用户仍拿不到生产凭证 onboarding（dev genesis 的 setup secret 由 relay 部署方提供）；
  5. 实例锁仍是"锁文件 + `create_new`"的 MVP（硬杀残留需人工删；产品化换 `fd-lock`/`fs2`）；
  6. 公开配置的写入仍在 **UI 线程**（与 S2 备份确认一致：profile 由 UI 独占、1KB 原子写），
     不走 worker；写失败的可重试语义不受影响。
- **Windows 真机验收点（未在本轮 Linux 环境执行，交给 Lead/真机）**：中文/空格路径的
  实例目录、DPAPI 跨用户解封失败文案、Join 表单遮罩 + 粘贴（Ctrl+V / Shift+Insert）、
  Tab 顺序、DPI 下表单布局、Working 关窗拦截、单窗切换、失败后回到表单且字段保留。

## 实施记录（S4：设置 UI，2026-11-13）

代码增量：`crates/app/src/onboarding/settings.rs`（设置状态 + relay 编辑校验 +
公开 id 短展示）、`onboarding/app_root.rs`（设置覆盖层/动作/选择器）、
`shell.rs`（关闭设置后把焦点还给当前编辑器）、`crates/app/tests/onboarding_ui.rs`
（6 个 headless 用例）。**未改 wire/SQLite/身份封装/配置格式**。

- **入口与状态**：设置是 `AppRoot` 的**覆盖层**（不是新的根状态）：Main 右下角
  （编辑器状态栏上方）固定入口 `app-root-settings`，Welcome 页也放同一 selector 的入口；
  打开时 Main 的 `AppShell` 实体原样保留（不销毁、不重建），关闭后回到原状态并把焦点
  还给当前编辑器。入口只用鼠标事件，不注册快捷键，不干扰编辑器键盘输入。
- **面板与 selector**：`dev-sync-settings`（覆盖层根）、`settings-relay`（预填 active
  relay，可编辑）、`settings-save` / `settings-cancel`、`settings-instance-<i>` /
  `settings-instance-<i>-active`（active 优先列表与「设为默认」）、`settings-error` /
  `settings-notice`（两行始终存在，便于断言/诊断），另有
  `settings-copy-vault-id` / `settings-copy-device-id` / `settings-copy-path`。
- **实例列表**：重新读公开配置（外部手改可见）后展示 instance_id（短）、`vault_path`、
  `vault_id`（短）、`device_id`（短）与 active 标记；active 排在最前。读失败只在面板
  内联报错，不覆盖配置、不重建实例。
- **切换 active**：只改 `active_instance` 并原子写配置；成功提示「重启后生效（当前主界面
  保持不变）」，不热切换 Vault/worker；relay 输入框跟着切到新 active，避免把 A 的 relay
  误写到 B。写失败保持原配置与原列表，可重试。
- **修改 relay**：只对 active 实例；校验与运行时门禁同源——`gate::validate_relay_url`
  （明文 loopback：`127.0.0.1` / `localhost` / `[::1]`，端口任意）+ `ServiceConfig::
  build_transport` 兜底（拒绝路径/query/userinfo 等）。空值、https、非 loopback、
  userinfo、带路径一律内联错误且**不写配置**；错误文案不回显 userinfo。保存成功后
  当前 worker/relay 不热切换；重启后 `AppRoot` 用新 relay 打开实例。
- **公开信息与秘密边界**：只展示/复制 `vault_id`、`device_id`、库路径这类公开值，并提示
  不要分享 setup secret/恢复码/根密钥/设备私钥；设置面板**没有**秘密字段（`InstanceEntry`
  类型上就没有 setup secret/恢复码/root/manager seed）。
- **失败语义**：配置写失败（Unix 只读目录实测）→ 内联错误 + 可重试；不重建实例、不删
  目录/库/身份、不触发 Create/Join。取消/点空白关闭 = 不写配置。
- **偏差与未做**：① 采用“Main 内 overlay”，未新增 `RootStateLabel::Settings`（ADR 允许
  二选一）；② 可选「移除本实例配置」**未实现**（本轮只做查看/切 active/改 relay；
  避免在没有二次确认与选择器契约时动索引）；③ 配置写入仍在 UI 线程（与 S2/S3 相同，
  1KB 原子写）；④ 无密钥轮换、无热切换、无多窗口。
- **测试**（`onboarding_ui.rs`，真实 dev relay + 真实 Vault/身份，**无 sleep**）：
  ① 创建实例 → Main 点设置 → 列表/active/公开 id/path 正确、无秘密控件、取消不写且
  `AppShell` Entity 不变；② 改合法 loopback relay → 保存 → 配置更新 +「重启后生效」+
  当前面板仍用旧 relay → 重启 AppRoot 后新 relay 生效；③ 空/非 loopback/https/userinfo/
  带路径 relay → 内联错误且配置字节不变；④ 两条配置切换 active → 写配置 + 当前 Main
  仍是旧实例 → 重启进入新 active；⑤ Unix 只读目录写失败 → 错误可重试、库/身份/库字节
  不变、无 Create/Join，恢复权限后重试成功；⑥ Main 建笔记后打开设置再取消 → 配置不变、
  `tree-row-0`/当前正文/`AppShell` Entity 都不变。另有 `settings.rs` 的 3 个纯函数单测
  （合法/非法 relay、短 id）。
- **Windows 真机验收点（未在本轮 Linux 环境执行，交给 Lead/真机）**：右下角设置入口在
  DPI/不同窗口尺寸下不遮挡编辑器状态栏与同步面板；覆盖层滚动/中文路径；切 active 与
  改 relay 后的重启行为；关闭设置后编辑器焦点恢复。
