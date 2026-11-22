# 同步产品化 A 组第二切片：产品「仅本地使用（不同步）」入口

> 2026-11-18。范围：**产品的本地（不同步）笔记入口**——Welcome 入口、本地库路径、
> 打开行为、Main 提示、错误页。不改 TLS/relay 协议、不改 schema/wire、不改身份文件
> 与实例库文件名；不 commit/打包（由 Lead 统一处理）。

## 1. 背景

产品构建（默认 `sync-ui`，**不含** `dev-sync`）的 Welcome 只有「创建 / 加入同步实例」，
没有 relay 时新用户不可用；P2 又把旧的「继续本地（DEV ONLY）」开发入口从产品构建隐藏。
本切片补一条**不联网、不建同步实例**的本地笔记路径：用户点一下就能开始写，之后想同步
再从 Welcome 创建/加入实例（本地库与实例库互不干扰，不做迁移/导入）。

## 2. 入口与 selector

| 项 | 产品 `sync-ui`（无 `dev-sync`） | `dev-sync` 构建 |
| --- | --- | --- |
| Welcome selector | `app-root-local` | 同左（**同一 selector**） |
| 按钮文案 | **「仅本地使用（不同步）」**（`local.action`） | 「继续本地（DEV ONLY，无同步）」（`local.dev_action`） |
| 说明 | 「在本机打开一个独立笔记库…」（`local.welcome_hint`）+ 「本地库：{path}」（`local.path_hint`） | 「打开默认 dev vault…仅供开发」（`local.dev_hint`） |
| 行为 | 打开数据根下 `local/vault.sqlite3`（空库、不播种） | **保持旧行为**：默认 dev vault + 演示树 |

- `AppRootOptions::discover` 现在两种构建都是 `allow_local_vault = true`；
  `AppRootOptions::for_test` **默认关闭**（既有 Welcome 契约不变），要测本地路径的测试
  显式置 `true`（沿用原字段，没有新增构造参数）。
- `AppRoot::local_entry_label()` 是渲染与测试共用的**唯一文案来源**（按构建分派），
  文案全部走 `ui_text` + `crates/i18n` catalog（中英文），渲染代码没有新增中文字面量。
- 新消息命名空间 `local.*`：`welcome_title` / `welcome_hint` / `path_hint` / `action` /
  `mode_notice` / `open_failed_title` / `dir_label` / `dev_action` / `dev_hint` /
  `dev_open_failed_title`。

## 3. 本地库路径

由 `ProfileLayout` 的**数据根**推导（`instances/` 的上一级，本地目录与 `instances/` 同级）：

| 平台 | 路径 |
| --- | --- |
| Windows | `%LOCALAPPDATA%\menghunote\local\vault.sqlite3` |
| Linux | `$XDG_DATA_HOME/menghunote/local/vault.sqlite3`（未设置时 `~/.local/share/menghunote/local/vault.sqlite3`） |

- `ProfileLayout::data_root()` / `local_dir()` / `local_vault_path()`；常量
  `onboarding::LOCAL_DIR_NAME = "local"`，库文件名沿用 `VAULT_FILE_NAME = "vault.sqlite3"`。
- 显式 `ProfileLayout::new(config, instances)` 也按 `instances_dir` 的父目录推导
  （测试/第三方注入不用再传第二个根）；`local/` **不是** instance，不参与
  `instances/` 的枚举/迁移。
- 只做路径解析：`local_vault_path()` 本身**不建目录、不写文件**。

## 4. 打开行为

`dev_vault::open_local_vault_at(path)` = `open_vault_at_with_peer_opts(path, DEV_PEER_ID, false)`：

- 目录不存在时创建（`Vault::open` 前的 `create_dir_all`）；
- **不播种 demo 笔记**（`seed_demo = false`；空库侧栏为空，用户自己建笔记）；
- 已有库直接复用（`Vault::open`，内容不重置；同一路径再次打开读到原笔记）；
- peer 用 `DEV_PEER_ID = 1`（产品本地路径没有身份文件，只影响本次新增的 op）。

产品分支在 `AppRoot::open_local_vault` → `open_local_only_vault`；`dev-sync` 分支是
`open_local_dev_vault`（旧 `open_dev_vault()` + DEV 提示，行为逐字节不变）。

## 5. 打开后的 Main

- `cx.new(|cx| AppShell::new(vault, label, cx))`：**没有**同步面板/同步 worker/自动同步；
- `AppRoot::local_mode = true`，Main 顶部渲染固定提示条 **`app-root-local-mode`**，
  文案「本地模式：不同步」（`local.mode_notice`）；同步实例 Main 不渲染这一行；
- 不显示 `sync-status`、`dev-sync-panel`、`app-root-dev-banner`（产品构建本来就没有 DEV 横幅）；
- 设置/侧栏/编辑器/模板/日记都沿用 `AppShell` 既有能力（侧栏为空时照常可「＋ 笔记」）。

## 6. 数据安全

- **不写 `sync.json`**：本地库不登记为实例，`ProfileStore` 完全不参与；
- **不建 instance 目录**（不碰 `instances/`）、**不写身份文件**（`dev-identity.bin`）；
- **不联网**、不建同步实例、不参与同步；
- **不覆盖已有库**：路径已存在时只 `Vault::open`；打不开就报错，不截断/重建；
- **不取实例目录锁**：本地库与 `instances/<id>/` 分离，不影响同步实例的 `LOCK_FILE_NAME`；
  本地模式本身没有新锁（同一进程/多进程同时打开同一本地库仍依赖 SQLite 自身的
  `busy_timeout`，见 §9 已知限制）。

## 7. 错误

打不开本地库（权限、坏文件、非 SQLite 字节等）→ **GUI Error 页**（`AppRoot::set_error`，
`RetryKind::None` = 「返回欢迎页」），标题 `local.open_failed_title`（产品）/
`local.dev_open_failed_title`（dev-sync），公开字段是本地目录；错误文案经
`sanitize_error` 脱敏/截断。**不会静默退出、不会白屏、不会带病进入 Main**。

## 8. 测试

Linux 本机实测（最终代码）：

| 命令 | 结果 |
| --- | --- |
| `cargo test -p menghunote-app`（产品 `sync-ui`） | **522 passed**, 0 failed |
| `cargo test -p menghunote-app --features dev-sync` | **677 passed**, 0 failed |
| `cargo test -p menghunote-i18n` | **16 passed**, 0 failed |
| `cargo fmt --all -- --check` / `cargo clippy -p menghunote-app --all-targets --features dev-sync -- -D warnings` | 干净 |

新增/更新：

- `crates/app/tests/sync_ui_product.rs`（产品 headless，6 → **8**）：
  - 旧断言「产品不得出现 `app-root-local`」**反转**为必须出现，并断言文案是
    `ui_text::local_action()`、不含 `DEV`；P2 启动迁移测试同步更新；
  - `product_local_entry_opens_empty_local_vault_without_sync`：真实点击入口 → Main；
    `local/vault.sqlite3` 创建在 `ProfileLayout` 数据根下；无 `sync.json` / 无
    `instances/` / 无身份文件；`app-root-local-mode` 渲染；无同步面板/同步状态/DEV 横幅；
    侧栏空（不播种 demo）→ 点「＋ 笔记」可新建；关窗后 `replace_root` 模拟再次启动，
    再点同一入口 → 复用同一库（笔记仍在，不重置）；
  - `product_local_open_failure_lands_on_error_page`：写坏本地库字节 → 点击入口进
    `app-root-error`（标题 `ui_text::local_open_failed_title()`），不进 Main，
    且坏文件逐字节未被覆盖；
- `crates/app/tests/onboarding_ui.rs`：`dev_build_shows_dev_banner_and_local_entry`
  增加断言——`dev-sync` 构建的同一 selector 保留「继续本地（DEV ONLY）」文案；
- `crates/app/src/onboarding/layout.rs`：Linux/Windows 默认布局断言
  `local_vault_path()`；新增 `local_vault_path_lives_next_to_instances`（显式布局按
  `instances_dir` 父目录推导）；
- `crates/app/src/ui_text.rs`：`local_only_labels_follow_the_global_locale`（中英文）；
- `crates/i18n`：`local.*` 10 条消息 + 中英 catalog；命名空间校验加入 `local.`。

## 9. 已知限制 / 明确不做

- **本地模式不加新锁**：同一本地库被两个进程同时打开时只靠 SQLite `busy_timeout`；
  它不影响同步实例锁（本切片要求的最低标准），单用户桌面场景接受。
- 本地库**不进入** `sync.json`，也不提供「稍后把这个本地库转成同步实例」的迁移
  （创建实例仍会要求新目录/新库；转换需要单独的导入设计）。
- 重启**不会自动**进入本地 Main（没有 `sync.json` 记录），用户从 Welcome 再点一次；
  路径与内容不变（测试已覆盖）。
- 旧的 `dev-sync` 默认 dev vault（`target/tmp/menghunote-dev`）不会被搬到新路径，
  也不参与产品本地库；两者互不读取。
- UI 观感（提示条像素/DPI/IME）只做了 headless 布局断言，未真机验；
  Windows 本地路径只做单元断言（未在 Windows 真机跑）。
- 不改 TLS/relay 协议/schema/wire、不改身份/库文件名、不新增依赖。
