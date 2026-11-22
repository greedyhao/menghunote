# 同步产品化 P1：`sync-ui` 从 `dev-sync` 编译门禁拆出

> 2026-11-18。范围：只拆 **Cargo feature / 编译门禁 / 入口 / 文档**。
> **不改协议、schema、密钥、网络行为；不迁移数据目录；不重做 UI 文案。**
> 不 commit/打包（由 Lead 统一处理）。

## 1. Feature 矩阵

| feature | 含义 | 默认 |
| --- | --- | --- |
| `sync` | 仅协议/能力：`menghunote-sync` 依赖 + `sync_gate`；**无 UI** | 否（被 `sync-ui` 隐含） |
| `sync-ui` | 产品同步 UI：`onboarding` + `dev_sync` 面板/设置/状态 + 公开配置 JSON | **是**（`default = ["sync-ui"]`） |
| `dev-sync` | `sync-ui` + dev 专用：`local_dev`、`--dev-sync*` CLI/setup、localhost relay 门禁、V2 管理实验 | 否 |

构建/测试命令：

```bash
cargo build -p menghunote-app                 # 产品：sync-ui（默认）→ AppRoot
cargo run   -p menghunote-app                 # 产品：Welcome/Create/Join/打开实例
cargo test  -p menghunote-app                 # 产品构建测试（sync-ui）
cargo test  -p menghunote-app --features dev-sync   # dev 构建测试（local_dev/CLI/V2）
cargo test  -p menghunote-app --no-default-features # 旧默认（纯 dev vault，无同步）
```

`cargo test -p menghunote-app --features sync-ui` 与默认等价（`sync-ui` 已在 default 里）。

## 2. 入口

| 构建 | `main.rs` 入口 | 根视图 |
| --- | --- | --- |
| 默认（`sync-ui`） | `run_onboarding_window()` | `AppRoot`（Welcome/Create/Join/Main） |
| `dev-sync`（无显式 CLI） | `run_onboarding_window()` | 同上；`--dev-sync*` 时走旧 `AppShell` dev 生命周期 |
| `dev-sync` + `--dev-sync*` | `run_dev_sync_window()` | `AppShell`（local_dev / CLI） |
| `--no-default-features` / 只开 `sync` | `run_default_window()` | `AppShell`（dev vault，逐字节同 P1 前） |

产品构建（`sync-ui`，无 `dev-sync`）**没有** `--dev-sync*` / `MENGHUNOTE_DEV_SYNC`
入口；`mode_allowed(LocalDev)` 仍为 `false`。UI 层在应用创建链接、提交创建、兑换邀请
三处显式拒绝 `local_dev`（不联网、不消费票据）；打开已有实例仍走
`mode_allowed` + `parse_for_build`。

## 2.1 同步改动的相邻文件（非协议）

- `crates/app/src/shell.rs` / `editor/view.rs`：同步面板、设置、状态栏、持闸/adopt 转发
  的 cfg 从 `dev-sync` 迁到 `sync-ui`；V2 管理区仍 `dev-sync`。只改编译门禁，未改任何
  协议/网络/持闸语义。
- `crates/app/src/modules.rs`：第一方模块注册表里 Sync 的编译事实改为跟随 `sync-ui`
  （`ModuleFeatureGate::SyncUi`），与实际产物一致。
- `scripts/ui_smoke.ps1` / `scripts/README.md`：编辑器冒烟仍要旧的 dev vault 入口，
  构建改为 `cargo build -p menghunote-app --no-default-features`（默认 `sync-ui`
  会进 AppRoot 同步向导，不适合该场景）。

## 3. 编译期 cfg 分类

`sync-ui`（产品可用）：

- `onboarding`、`dev_sync::panel/service/state/sync_once/autosync/scheduler/identity/gate`；
- `AppShell` 的同步面板 / 设备列表 / 轮换面板 / 自动同步 / 状态栏投影 / 设置页；
- `EditorView` 的 external commit / adopt 转发（持闸两阶段）；
- `GRAPH_ENTRY_BOTTOM_GAP` 的"有 AppRoot 设置按钮"版本。

仍保留 `dev-sync`（**没有**下放到产品构建）：

- `sync_gate::mode_allowed(LocalDev)` 的许可（`local_dev` 只在 dev 构建为 `true`）；
- `dev_sync::setup`（`--dev-sync-create/join` 启动生命周期）、`dev_sync::cli`
  （`--dev-sync-info/publish`）；
- `dev_sync::management` + `AppShell` 管理区 + `AppRoot` V2 管理入口
  （`MENGHUNOTE_EXPERIMENTAL_V2=1` 也只是 dev 构建可见）；
- `main.rs` 的 `gate::resolve` 调用与 `run_dev_sync_window`；
- 现有 dev-sync 测试文件（`#![cfg(feature = "dev-sync")]`）保持显式门禁，
  不会静默变成 0 测试而丢失覆盖。

## 4. 测试数字（Linux，本机）

| 构建 | 结果 |
| --- | --- |
| 默认 / `--features sync-ui` | **473 passed**, 0 failed |
| `--features dev-sync` | **631 passed**, 0 failed |
| `--no-default-features` | **384 passed**, 0 failed（与 P1 前默认一致） |
| `cargo check -p menghunote-app` / `--features sync-ui` / `--features dev-sync` / `--no-default-features` | 通过 |
| `cargo clippy --all-targets`（dev-sync / sync-ui / no-default） | `-D warnings` 干净 |
| `cargo fmt --all -- --check` | 干净 |

新增 `crates/app/tests/sync_ui_product.rs`（3 个 headless 测试，`sync-ui` 且非
`dev-sync`）：AppRoot Welcome、`local_dev` 创建链接被拒 + `remote_tls` 可应用、
无同步面板的 `AppShell` 不 panic。

## 5. 明确未做（P2）

- ~~**数据目录/迁移**~~ **已由 P2 完成**（`menghunote` / `sync.json` + 旧配置保守迁移；
  `dev-identity.bin` / `vault.sqlite3` 文件名仍未改），见
  [`sync-product-p2.md`](sync-product-p2.md)。
- **TLS/DPAPI 真机**：`remote_tls` 仍只有 Linux 离线 harness；真实鉴权 relay /
  Windows 系统根 / DPAPI 未验。
- ~~**UI 去 DEV 标注**~~ **已由 P2 完成**（产品构建隐藏 DEV 横幅/面板标语/本地入口，
  AppRoot 启动日志去 DEV 标语），见 [`sync-product-p2.md`](sync-product-p2.md)。
- 轮换入口的 `MENGHUNOTE_EXPERIMENTAL_ROTATION`、V2 管理 UI 的可视化打磨。
- 协议、wire、schema、数据目录、网络行为均未改。
