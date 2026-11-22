# 本地模式自动进入 + 设置分类记忆（P1 产品体验）

日期：2026-11-19。范围：**只碰本机 UI 偏好**，不改 sync/relay/storage schema/wire，
不写 `AGENTS.md`，不改 `daily_notes.rs` / `templates.rs` / `shell.rs`。

## 1. 本地模式自动进入

### 目标

上次用「仅本地使用（不同步）」打开的库，重启后直接回到同一个本地 Main，
不再每次回 Welcome 点入口。

### 偏好文件与字段

两个文件都在 `ProfileLayout::data_root()`（产品 Linux
`$XDG_DATA_HOME/menghunote/`，Windows `%LOCALAPPDATA%\menghunote\`），与
`local/`、`instances/` 同级；**不写进 vault、不进 `sync.json`、不参与同步**。

| 文件 | 字段 | 取值 | 缺失/损坏 |
| --- | --- | --- | --- |
| `startup.json` | `mode`（可选 `version`） | `local` / `sync` | 视为"没有可恢复入口"→ Welcome |
| `settings-ui.json` | `section`（可选 `version`） | `appearance` / `language` / … | 回退 `appearance` |

示例（写入时带 `version`；读取时 `{"mode":"local"}` 这种最小写法也接受）：

```json
{
  "version": 1,
  "mode": "local"
}
```

```json
{
  "version": 1,
  "section": "language"
}
```

实现：`crates/app/src/onboarding/layout.rs`
（`StartupMode` / `load_startup_mode` / `save_startup_mode` /
`ProfileLayout::{startup_path,settings_ui_path}` / `write_json_file_atomically`）。
原子写：同目录临时文件 → `rename`；Windows 目标存在时 rename 失败退化为直接覆盖写，
并清掉临时文件。写失败只记日志（当前会话仍然有效），不阻塞打开/启动。

### 启动决策（`AppRoot::boot`）

按顺序：

1. **`startup.json` = `local`**
   - 本构建「本地入口」的库路径存在 → 直接打开并进 `Main`；
     **不读 `sync.json`、不迁移旧开发配置、不联网**，本地使用不被同步配置挡住；
   - 库不存在 → **不凭空创建**，回 Welcome，`app-root-notice` 明确提示
     `上次使用的本地笔记库不存在：<公开路径>；已回到欢迎页（不会自动新建库）`；
     实例列表容错读取（同步配置损坏在这里不进错误页）；
   - 库存在但打不开（例如文件被写坏）→ 沿用既有 Error 页语义（不静默、不重建）。
2. **`startup.json` 缺失/损坏/`sync`** → 沿用原有 active 实例自动打开逻辑：
   有有效公开配置 + 身份 → `Main`；否则 Welcome；配置损坏仍是原有 Error 页语义。

构建差异：

- 产品 `sync-ui`（默认）：本地路径 = `<数据根>/local/vault.sqlite3`；
- `dev-sync` 构建：「继续本地（DEV ONLY）」= 默认 dev vault
  （`target/tmp/menghunote-dev/vault.sqlite3`，可用 `MENGHUNOTE_DEV_VAULT` 覆盖），
  自动进入复用同一路径。

### 写盘时机

| 动作 | 写什么 |
| --- | --- |
| 本地库**打开成功**（手动点入口或启动自动恢复） | `startup.json` → `{"mode":"local"}` |
| 同步实例**打开成功**（启动自动或手动打开） | `startup.json` → `{"mode":"sync"}` |
| 设置里「设为默认」某同步实例 | `startup.json` → `{"mode":"sync"}`（兑现「重启后生效」） |
| 打开失败 / 回 Welcome | 不写（保留上次成功的值） |
| 关窗 / 退出 | 不额外写 |

本地打开**不取实例目录锁**（与旧行为一致），自动进入不新增任何锁语义；
同步实例锁/持闸/worker 未改动。

## 2. 设置分类记忆

- 打开设置页时读 `settings-ui.json` 恢复上次分类；
- 用户点左侧导航切换分类时写盘（同一分类重复点击不重写）；
- 恢复/打开时：未知 key 或损坏文件 → `appearance`；
  上次分类是**已关闭的插件分类**（模板/日记）→ clamp 到 `plugins`；
- clamp 只影响"恢复时的投影"，**不静默改写用户上次选择**（磁盘值保持原样；
  重新启用插件后再次打开仍可回到该分类）；
- 关闭设置不额外写；`AppShell` 的 dev-sync 设置页不拆分类，行为不变。

实现：`SettingsSection::parse` / `AppRoot::{open_settings, settings_select_section}` /
`load_settings_section` / `save_settings_section`
（`crates/app/src/onboarding/app_root.rs`）。

## 3. 测试

新增/更新 headless（不 sleep，`CARGO_TARGET_TMPDIR`，临时 `ProfileLayout::new`）：

- `crates/app/tests/startup_resume.rs`（`sync-ui`）
  - 产品构建 9 个：本地打开 → 重启自动进本地 Main（库不重置）；本地库被删 →
    Welcome + 提示、不建库、不 Error；偏好损坏 → Welcome；设置分类跨重启恢复；
    插件关闭 clamp 到「插件」；损坏/未知设置偏好回退「外观」；只在真实切换时写盘；
    本地模式里「设为默认」同步实例 → 启动偏好切 `sync`（重启走同步路径）。
  - dev-sync 构建 5 个（本地路径 4 个按构建关闭）：设置分类记忆相关。
- `crates/app/tests/sync_ui_product.rs`：原「本地重启仍回 Welcome」断言更新为
  「重启直接进本地 Main」，并断言 `startup.json` 内容与不写 `sync.json`。
- `crates/app/tests/onboarding_ui.rs`（dev-sync）：新增
  `sync_instance_open_is_remembered_and_resumes_after_restart`——同步实例打开写
  `mode=sync`，重建 `AppRoot` 仍自动进同一实例 Main；启动偏好不混进 `sync.json`。
- 单元测试：`layout.rs` 启动偏好解析/原子写/路径；`app_root.rs` 分类短名往返、
  设置偏好损坏矩阵；`ui_text.rs` 中英文案。

实测（2026-11-19，本机 Linux）：

| 命令 | 结果 |
| --- | --- |
| `cargo test -p menghunote-app` | **546 passed**（默认 `sync-ui` 产品构建） |
| `cargo test -p menghunote-app --features dev-sync` | **697 passed** |
| `cargo test -p menghunote-i18n` | **16 passed** |
| `cargo clippy -p menghunote-app --all-targets [--features dev-sync] -- -D warnings` | 干净 |
| `cargo fmt --all -- --check` | 干净 |

> 计数含并行开发中的其它改动；本任务的增量：`startup_resume` 9（产品）/5（dev-sync）、
> `onboarding_ui` +1、`layout` 单测 +3、`app_root` 单测 +2。

## 4. 已知限制

- 两个偏好文件是**本机**偏好，不同步、不参与多设备；换机器/清数据目录后回到
  Welcome/外观，属预期。
- `dev-sync` 与产品构建共用同一数据根与同一份 `startup.json`；交替运行时，
  另一种构建的本地库可能不存在 → 按"本地库不存在"回 Welcome + 提示（不报错、不建库）。
  `dev-sync` 的「本地」路径随 `MENGHUNOTE_DEV_VAULT` 改变，改环境变量后同样按
  "本地库不存在"处理。
- `startup.json` 每次成功打开都会重写（即使值未变）；写失败不阻塞本次打开。
- 本地库"存在但打不开"仍进 Error 页（与手动点入口一致）；只有"文件不存在"回
  Welcome——错误页会明确给出路径与原因，便于人工修复。
- 设置 clamp 不写回磁盘（见 §2）；若希望"关掉插件后永久忘掉该分类"，需要另一次
  偏好写盘，本切片没有做。
- 真机像素/DPI/重启观感仍需 Windows 人工验收；headless 只证明状态机、落盘与
  selector 契约。
