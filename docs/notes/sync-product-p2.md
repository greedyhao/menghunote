# 同步产品化 P2（A 组第一切片）：数据目录产品化 + 旧数据保守迁移 + 去 DEV 标注

> 2026-11-18。范围：**本机数据目录/配置文件名、旧开发目录的一次性迁移、产品构建的
> DEV 标注**。不改 TLS/relay 协议、不改 schema/wire、不改身份文件与库文件名；
> 不 commit/打包（由 Lead 统一处理）。

## 1. 目录与文件名

| 项 | P1 及以前 | P2（本切片） |
| --- | --- | --- |
| 产品目录（Windows） | `%LOCALAPPDATA%\menghunote-dev\` | `%LOCALAPPDATA%\menghunote\` |
| 产品目录（Linux 配置） | `$XDG_CONFIG_HOME/menghunote-dev/` | `$XDG_CONFIG_HOME/menghunote/` |
| 产品目录（Linux 数据） | `$XDG_DATA_HOME/menghunote-dev/` | `$XDG_DATA_HOME/menghunote/` |
| 公开配置文件名 | `dev-sync.json` | **`sync.json`** |
| 实例目录 | `instances/<id>/` | 不变 |
| 实例库文件 | `vault.sqlite3` | **不变**（本轮不改名：避免动身份/用户数据） |
| 实例身份文件 | `dev-identity.bin` | **不变**（同上，另行规划） |

- 环境变量缺失时的回退不变：Linux 回退 `~/.local/share` 与 `~/.config`；
  **HOME/LOCALAPPDATA 都没有仍报错**，绝不回退 cwd/tmp。
- 主题/语言偏好早就是 `menghunote`（`%LOCALAPPDATA%\menghunote` /
  `$XDG_CONFIG_HOME/menghunote`），P2 只是把同步目录对齐到同一产品名。
- `ProfileLayout::new(config, instances)` 的**显式路径语义不变**：不参与旧目录迁移
  （`legacy_config_path = None`），测试/第三方注入完全不受影响。

代码：`crates/app/src/onboarding/layout.rs` 的 `APP_DIR_NAME` / `PROFILE_FILE_NAME` /
`LEGACY_APP_DIR_NAME` / `LEGACY_PROFILE_FILE_NAME`。

## 2. 旧数据保守迁移

### 2.1 触发与判据

`AppRoot::boot`（启动引导，构造根视图后立即执行）调用
`ProfileLayout::migrate_legacy_profile()`，顺序是硬约束：

1. **新配置已存在** → 一律以新配置为准，**完全不读**旧配置，返回 `false`（幂等）；
2. 旧配置不存在 → `false`（不建任何东西）；
3. 旧配置存在 → **只读解析**，把同一份公开实例列表原子写到新 `sync.json`，返回 `true`。

只有按平台默认布局解析（`ProfileLayout::discover`）才带旧配置路径
（`menghunote-dev/dev-sync.json`）；显式 `ProfileLayout::new` 恒为 `None`。

### 2.2 保什么 / 不做什么

- **`vault_path` 原样保留**：每个实例的库还是旧路径，**不移动、不复制** `instances`
  目录（避免大文件搬迁、SQLite 半搬状态、身份文件错位）。
- **旧目录/旧配置原样保留**：不删、不改写。v1 旧配置只在内存里升到 v2（`read_only`
  不回写），旧文件里仍是 `"version": 1`；新文件是 v2。
- **不改变模式**：旧 `relay_mode` 原样写入；`local_dev` 仍是 `local_dev`，
  不会被悄悄改成 `remote_tls`。
- 写新配置走既有 `ProfileStore::save` 原子写（同目录临时文件 → write/fsync → rename →
  父目录 fsync）；失败时旧配置不受影响。
- 迁移只涉及**公开配置**（instance_id/relay_mode/relay_url/vault_path/vault_id/
  device_id/api_version），没有任何秘密材料参与。

### 2.3 失败与错误语义

迁移失败（损坏/版本不支持/IO）一律**不重建、不覆盖**，并在 GUI 错误页给出原因（`AppRoot` 的 Error 状态，
`RetryKind::ReloadConfig`，可点「重新读取配置」重试；**不是**无提示退出/白屏）：

| 旧配置状态 | 结果 |
| --- | --- |
| 坏 JSON / 缺 version | `ProfileError::Corrupt { path: <旧文件> }`；不写新配置 |
| 未知版本（如 99） | `ProfileError::UnsupportedVersion`；不写新配置 |
| 产品构建里的旧 `local_dev` | **跳过迁移**（`parse_for_build`/`mode_allowed` 不适用）；不写新配置、不改模式；旧文件保留，产品从 Welcome 起步 |
| IO 失败（读/写） | `ProfileError::Io`；旧配置不动 |

### 2.4 幂等

- 第一次启动：新 `sync.json` 出现，旧配置逐字节不变；
- 之后每次启动：`sync.json` 已存在 → 直接跳过迁移，新配置逐字节不变（测试断言
  二次调用返回 `false` 且文件内容不变）；
- 迁移是单向的：不会把新配置回写旧目录。若用户把新配置删掉，下次启动会再从旧配置
  复制一份（旧配置仍在），行为仍是保守的。

### 2.5 已知限制

- 迁移发生在 `AppRoot` 启动引导；**relay 服务端/协议无需迁移**（wire 未变）。
- 旧目录中的 `instances/` 与身份文件保持原位；将来做「目录真正搬家」需要单独的
  原子迁移设计（含 SQLite 一致性、身份文件、实例锁），不在本切片。
- 两次进程并发首次启动时，`save` 的 rename 可能覆盖另一方刚写的新配置（与既有配置
  写入同一窗口）；单用户桌面场景接受，未加额外文件锁（实例锁在打开实例阶段才取）。
- 产品构建遇到旧 `local_dev` 配置时**跳过迁移**（不静默转换、不写新配置），旧文件
  保留、产品正常进入 Welcome；旧实例仍可用 `dev-sync` 构建打开。

## 3. 去 DEV 标注（产品构建）

产品构建 = 默认 `sync-ui`、**无** `dev-sync`。`dev-sync` 构建保留原有开发提示。

| 位置 | 产品构建（`sync-ui`） | `dev-sync` 构建 |
| --- | --- | --- |
| `AppRoot` 各页顶部横幅（`banner_text`） | **不渲染节点**（`Option<Div> = None`，`app-root-dev-banner` 不存在） | 红色 `DEV ONLY · 开发同步…`（`ui_text::settings_sync_dev_banner`） |
| 同步面板固定标题（`dev_sync/panel.rs`） | 中性「同步面板」（`settings.sync.panel_title`，`dev-sync-header` selector） | `DEV ONLY / 非生产 / localhost`（红字，原短句） |
| Welcome「继续本地（DEV ONLY，无同步）」 | **隐藏**（`AppRootOptions::discover` 的 `allow_local_vault = cfg!(feature = "dev-sync")`） | 显示（`dev-sync` 构建的本地 dev vault 入口） |
| AppRoot 启动日志（`main.rs`） | `[menghunote] 同步向导：配置 = …；实例目录 = …`（已删除 `DEV ONLY / localhost`） | 同上（日志不带 DEV 标语） |
| 面板/设备页的 relay 地址 | **照实显示**动态地址（`127.0.0.1` 等不硬改） | 同左 |
| `--dev-sync*` CLI / localhost setup / V2 管理 / 轮换实验入口 | 不存在（编译期 cfg） | 保留 |

i18n：删除不再使用的 `settings.sync.dev_title`（`settings_sync_dev_title` 无调用点）；
新增 `settings.sync.panel_title`（中「同步面板」/ 英「Sync panel」）；
`settings.sync.dev_banner` 保留给 `dev-sync` 构建。文案仍全部走 `ui_text` + catalog，
渲染代码不新增中文字面量。

## 4. 测试

Linux 本机实测（最终代码，复跑记录见 §6）：

| 命令 | 结果 |
| --- | --- |
| `cargo test -p menghunote-app`（产品 `sync-ui`） | **517 passed**, 0 failed |
| `cargo test -p menghunote-app --features dev-sync` | **675 passed**, 0 failed |
| `cargo test -p menghunote-i18n` | **16 passed**, 0 failed |
| `cargo test -p menghunote-app --no-default-features --no-run` | 构建通过（旧默认 406 计数未变） |
| `cargo fmt --all -- --check` / `cargo clippy -p menghunote-app --all-targets --features dev-sync -- -D warnings` | 干净 |

新增/更新的测试：

- `crates/app/src/onboarding/layout.rs` 单测（两种构建都跑）：
  - `linux_defaults_follow_xdg_and_fall_back_to_home` / `windows_defaults_...`：
    默认路径改为 `menghunote` / `sync.json`，并断言旧配置路径；
  - `explicit_layout_has_no_legacy_path_and_never_migrates`：显式 `new` 不迁移；
  - `legacy_profile_is_migrated_to_new_sync_json_without_moving_vault`：新文件是 v2、
    `vault_path` 不变、旧配置/旧库/旧身份逐字节不动、二次调用不改新配置；
  - `existing_new_config_takes_precedence_over_legacy`：新配置优先、不覆盖不合并；
  - `corrupt_or_unsupported_legacy_is_not_rebuilt`：坏 JSON/未知版本报错且不写；
  - `migrating_legacy_v1_keeps_old_file_and_writes_v2`（`dev-sync`）：v1 旧文件不回写、
    新模式仍 `local_dev`、新文件 v2；
  - `product_build_rejects_legacy_local_dev_without_writing`（产品）：`InvalidRelay`、
    不写新配置、不改模式。
- `crates/app/src/dev_sync/panel.rs` 单测：`panel_header_has_no_dev_wording_in_product_build`
  （产品构建标题无 `DEV`/`非生产`/`localhost`；dev 构建保留开发警示）。
- `crates/app/tests/sync_ui_product.rs`（产品构建 headless，3 → 5 个）：
  - `product_startup_uses_new_layout_migrates_legacy_and_hides_dev_entries`：启动迁移、
    旧文件保留、`vault_path` 不变、无 DEV 横幅/本地入口、默认 relay 为空；
  - `product_startup_shows_error_page_for_corrupt_legacy_config`：损坏旧配置进 Error 页；
  - 既有 Welcome 测试追加 `app-root-dev-banner` / `app-root-local` 不存在断言。
- `crates/app/tests/onboarding_ui.rs`（`dev-sync` headless）：新增
  `dev_build_shows_dev_banner_and_local_entry`（开发构建保留横幅与本地入口）。
- 临时目录路径更新：`onboarding_ui`、`sync_ui_product`、`onboarding_create_link`、
  `headless_notes_plugins_ui` 里写死的 `config/dev-sync.json` → `config/sync.json`
  （其它显式 `ProfileLayout::new` 路径保留，语义不变）。

## 5. 明确不做（后续切片）

- TLS/domain、真实 relay 鉴权、Windows DPAPI 真机、Windows 系统根 —— 仍只有 Linux
  离线 harness 覆盖。
- 产品本地-only 入口（无同步的本地库）：本切片只把「继续本地（DEV ONLY）」隐藏，
  不新增产品入口。**（已由 A 组第二切片补齐：产品「仅本地使用（不同步）」入口 +
  `<数据根>/local/vault.sqlite3`，见 [`sync-product-a2.md`](sync-product-a2.md)。）**
- 身份文件改名（`dev-identity.bin`）、库文件改名、`instances/` 真正搬家 —— 会动到
  用户数据/身份，需要单独设计与验收。
- 创建表单里既有的「我理解这是开发同步（DEV ONLY / localhost / 非生产）」同意文案与
  `remote_tls` 的「DEV 状态」提示：不在本切片清单内，未改（仍是既有开发措辞）。

## 6. 验证命令

```bash
export CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 \
  LIBRARY_PATH=/tmp/ui-test-probe/sysroot/usr/lib/x86_64-linux-gnu \
  PKG_CONFIG_PATH=/tmp/ui-test-probe/sysroot/usr/lib/x86_64-linux-gnu/pkgconfig
cargo fmt --all -- --check
cargo clippy -p menghunote-app --all-targets --features dev-sync -- -D warnings
cargo test -p menghunote-app
cargo test -p menghunote-app --features dev-sync
cargo test -p menghunote-i18n
cargo test -p menghunote-app --no-default-features --no-run
```
