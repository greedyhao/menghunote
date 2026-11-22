# ADR 0004：UI 层切换到 GPUI Kit（2026-11-10）

- 状态：**已接受**（人类拍板 C）
- 决策者：人类（产品负责人）；输入来自 GPT 评估 + 编译探针
- 影响范围：仅 `crates/app` 的 GPUI 依赖与平台入口；`crates/text`、`crates/editor`、`crates/storage` **不受影响**

## 背景

当前 UI 基线是 `gpui-unofficial 1.27` + `gpui-platform-gpui-unofficial 1.27.0`（AGENTS §1）。
出现的新选项是 [**GPUI Kit**](https://github.com/longbridge/gpui-kit)（`gpui-kit 0.9.1`，2026-11-09）：
75+ 组件、UI 集成测试、无障碍、docking/表格/webview 等能力，自带 `<Editor>`、markdown 等示例。

关键事实（已核实）：

| 包 | 来源 | 说明 |
| --- | --- | --- |
| `gpui-unofficial 1.27.0` | `gpui-archipelago/gpui-unofficial` 再发布 Zed 主干 | 2026-11-04，68,744 行 |
| `gpui-pre 0.3.12` | **`zed-industries/zed` 快照 `d2a4fb6`** | 2026-11-09，70,193 行 |
| `gpui-kit 0.9.1` | longbridge meta crate | 门面 = `gpui-pre` + `gpui-base` + `gpui-component`（+assets） |

- 两者同源，只差约一周 Zed 提交；**类型互不兼容，不能混用**，只能整体替换。
- 探针（隔离 worktree，见 `task-51`）结论：`gpui-kit` 用 Cargo 别名 `gpui = { package = "gpui-kit" }` 接入后，
  **错误分类：缺失符号 1 处（平台入口）、签名变化 0、trait 不匹配 0、平台层差异 0**；
  唯一代码改动是 `gpui_platform::application()` → `gpui::platform::application()`；
  冷构建 5m39s，窗口能起，标题/样式/投影/侧栏/状态栏渲染正常。

## 选项

- **A 维持 `gpui-unofficial`**：零迁移成本；但组件、UI 测试、无障碍都要继续手搓。
- **B 只换 `gpui-pre`（跟 Zed 主干）**：同样 1 行平台入口；不带来组件，只增加回归面，**收益不足**。
- **C 换 `gpui-kit` 门面**：多拉 `gpui-base/component/assets`，换来组件生态与测试基座；迁移成本经探针证明为"重命名级"。

## 结论

**选 C**：切到 `gpui-kit 0.9.1`，**精确 pin**（`=0.9.1`），不跟随每周自动升级。

理由：

1. 探针证明迁移是**纯重命名 + 1 行平台路径**，人工预算 0.5–1 天，远低于 GPT 设定的"≤1 周"门槛；
2. 后续搜索、设置、标签页、对话框都需要输入框/列表/对话框等组件，Kit 可直接复用，避免继续手搓；
3. 现在换最便宜——`app` 才 8 千行，越晚越贵。

## 落地

1. `crates/app/Cargo.toml`：
   ```toml
   # 依赖别名保持代码里 `gpui::` 路径不变（gpui-kit 是 gpui-pre 的门面）
   gpui = { package = "gpui-kit", version = "=0.9.1" }
   ```
   删除 `gpui-unofficial` 与 `gpui-platform-gpui-unofficial`。
2. `crates/app/src/main.rs`：`gpui_platform::application()` → `gpui::platform::application()`。
3. `crates/text`、`crates/editor`、`crates/storage` **一行不改**（无 GPUI 依赖）。
4. 更新 AGENTS §1 依赖基线；保留旧分支/`backup/main-wip-20261110` 供回退。

## 验收门（全部通过才合并 `dev`）

- 三绿：`fmt` / `clippy --all-targets -D warnings` / `test --all`；
- 依赖树只剩目标血脉（无 `gpui-unofficial`）；
- 全部 UI smoke 场景：menu / delete / rename / projection / displayedit / cursorkeys / blockcaret / export；
- **人工真机**：微软拼音（预编辑、候选窗、上屏、`Esc`、组合期切焦点、undo 无残留）、方向键/退格/选区、
  `DPI 100% / 150%` 各一次、多文档切换；
- 记录冷/热启动耗时与内存粗对比（不设硬门槛，仅记录）。

## 后果与风险

- **好处**：可直接用 `gpui-component` 的输入框/列表/对话框/树/表格/docking/测试基座；`gpui-base` 的文本与布局原语可用。
- **风险**：
  - `gpui-kit` 是 longbridge 的**周更**包且 pin 死 `gpui-pre` 快照；升级需整体对齐，**禁止**只升一层；
  - 依赖图更大（`gpui-base`/`gpui-component`/assets），冷构建时间上升（探针 5m39s）；
  - 探针只证明"能编译、能起窗、能画"，**IME/DP/选区几何必须真机逐项复核**；
  - 若出现无法接受的回归，回退方式 = 恢复两条依赖 + 一行平台入口（`git revert` 即可）。
- **不在本次范围**：把自研投影编辑器替换成 Kit 的 `<Editor>`（语义不同：它不处理"隐藏标记"）。

## 相关

- 探针报告：`task-51`（隔离 worktree `../menghunote-kit-probe`，分支 `probe/gpui-kit`，未合并）；
- GPT 评估：推荐"隔离风险验证、主线不动"；本次探针证明迁移成本极低后，人类拍板整体切换。
