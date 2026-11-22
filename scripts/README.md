# scripts —— 开发期自动化脚本

这些脚本**不入产品**，只服务于"每次改完 UI 立刻真机跑一遍"（AGENTS.md §1）。

## ui_smoke.ps1 —— UI 冒烟（真实鼠标 + 首帧就绪轮询）

```powershell
# 右键目录 → 菜单（默认场景）
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/ui_smoke.ps1

# 左键点目录 → 折叠
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/ui_smoke.ps1 -Scenario toggle

# 右键笔记 → 删除第一步（进入「确认删除」态）
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/ui_smoke.ps1 -Scenario delete

# 其它参数
-Scenario menu|delete|toggle   # 默认 menu
-Exe <相对仓库根的 exe 路径>    # 默认 target\debug\menghunote-app.exe
-ReadyTimeoutMs 15000          # 等「首帧就绪」的上限
-PollIntervalMs 50             # 轮询间隔
```

固定流程（顺序不要改）：

1. `Stop-Process -Name menghunote-app -Force` —— **旧实例会占住 exe**，不杀就会
   `cargo build` 报「拒绝访问 (os error 5)」，脚本若继续跑就是拿旧二进制断言（踩过）。
2. `cargo build -p menghunote-app` 并检查**退出码**，非 0 立即失败退出。
3. 启动应用（stderr 落到 `target/tmp/ui-smoke.log.err`）。
4. **轮询**日志里的「首帧就绪」标记（50ms 一次，最多 15s）——不再固定 sleep 几秒。
5. 用**真实鼠标事件**（`SetCursorPos` + `mouse_event`）点击：`PostMessage` 注入在这台
   机器上会丢事件（踩过）。
6. 按场景等待期望日志出现，打印"点击→日志出现"的耗时。
7. 结束：`Stop-Process` + 打印日志尾部。

脚本会把 `MENGHUNOTE_TRACE_MENU=1` / `MENGHUNOTE_TRACE_MOUSE=1` 打开，因为菜单类日志
默认是关的（见下）。

### 关于「首帧就绪」

应用在 `EditorElement::paint`（真正向窗口提交绘制的那一帧）打一次：

```
[menghunote] 首帧就绪 t=+820ms
```

`t` 从 `main` 第一行开始计。**只有窗口真的被绘制时才会出现**：

- 正常启动 → 一定出现；
- 窗口被最小化创建 → Windows 不要求首帧绘制，可能不出现（此时脚本会在 15s 后超时失败，
  这是有意的：宁可失败也不要对着看不见的窗口乱点）；
- 应用在后台且系统节流绘制 → 可能延迟，超时同样失败。

### 日志开关

| 环境变量 | 内容 |
| --- | --- |
| `MENGHUNOTE_TRACE_MOUSE=1` | 窗口级鼠标事件（down/up + 坐标 + 是否 active） |
| `MENGHUNOTE_TRACE_MENU=1` | 行右键 / 菜单打开 / 菜单渲染 / 菜单关闭原因 |

默认都不输出，`cargo run` 的 stderr 保持干净。

## e2e（Rust example，跨平台，无 shell 编排）

同步/轮换 e2e 全部是**自包含 Rust example**：进程内启动真实 axum relay（临时 SQLite +
`FakeClock`，仅 loopback 放行验签、`127.0.0.1:0`），不需要 PowerShell/bash，也不需要外部
`dev_server`。Windows 上只要有 cargo 即可，一条命令即验收（退出码 0 + 末尾标记）：

```bash
cargo run -p menghunote-sync --example e2e_rotation_verify       # 两阶段 localhost+rotation，末尾 VERIFY_EXIT=0
cargo run -p menghunote-sync --example e2e_rotation_two_devices  # 末尾 TWO_DEVICE_EXIT=0
cargo run -p menghunote-sync --example e2e_rotation_two_rounds   # 末尾 TWO_ROUND_EXIT=0
```

旧的 `e2e_rotation_two_rounds.ps1` / `e2e_rotation_verify.ps1` / `e2e_rotation_two_devices.ps1`
已全部删除（被上面的自包含 example 取代）。`e2e_localhost`、`e2e_rotation` 仍是底层 example，
保留 `--relay` + `--setup` 的**外部** relay 模式（也可由 `e2e_rotation_verify` 在进程内调用）。
