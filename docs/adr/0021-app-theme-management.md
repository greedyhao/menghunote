# ADR 0021 — 应用级主题管理

> 状态：**已决**（2026-11-17，用户确认：只做应用级主题；实现待排期）。
> 关联：ADR 0003（模块边界）、ADR 0019（Markdown/Object/表格/Mermaid）、
> ADR 0020（i18n 与插件边界）。
>
> 本 ADR 只决定**应用级主题管理**的边界与架构，不包含每篇笔记单独主题。
> 具体实现切片、主题包格式与插件主题 API 在后续 ADR/任务中细化。

## 1. 背景

当前 UI 颜色是**硬编码浅色**：

- `crates/app/src/editor/theme.rs` 提供 `window_bg()` / `text_color()` 等自由函数，
  642 行常量与取色函数，没有主题状态；
- 侧栏、状态栏、表格、任务框、图片遮罩、图谱、Mermaid 各自直接调用这些函数；
- Mermaid 预览缓存 key 里已有 `PREVIEW_THEME = "light"`，但没有真正的主题切换；
- 设置页目前没有主题选择，也没有应用级偏好持久化。

后期要支持：

- 浅色 / 深色 / 跟随系统；
- 自定义主题包（后期）；
- 插件 UI 使用 host 主题 token；
- i18n 文案与主题设置解耦。

如果继续用 `if dark { ... }` 或各模块自己取色，会造成：

- 同一语义在不同模块颜色不一致；
- 切主题时漏改、闪烁、缓存不失效；
- 插件/自定义主题无法安全接入；
- 测试只能截图，无法做 token 完整性/对比度断言。

## 2. 范围

### 2.1 做

- **应用级**主题：整个窗口/所有笔记共用；
- 内置浅色、深色；
- 跟随系统（启动与系统切换时更新）；
- 语义 token 化：UI 不再直接写颜色常量；
- 主题选择持久化（本机偏好，不同步）；
- Mermaid/图谱/表格/图片遮罩等派生视图在切主题时正确失效/重绘；
- 为后期自定义主题包与插件主题预留 manifest 边界。

### 2.2 不做

- **每篇笔记单独主题**：不把 `theme_id` 写进笔记正文/元数据；
- 任意 CSS/HTML 注入；
- 允许插件直接绘制/覆盖 host 颜色；
- 高对比度/色弱模式首版不承诺（可后续作为内置主题变体）；
- 主题跟随笔记导出（导出使用应用主题的默认样式）；
- 主题包市场/在线安装（后期）。

## 3. 主题模型

### 3.1 标识与模式

```rust
pub enum ThemeMode {
    Light,
    Dark,
    System,
}

pub struct ThemeId(String); // 内置 "light" / "dark"；自定义主题包 id
```

- `ThemeMode::System` 解析为当前 OS 的 light/dark；OS 变化时重新解析。
- 设置里保存 **mode** 与（自定义主题时）**theme_id**；内置模式不依赖 id。
- 找不到/损坏/不兼容的主题回退到内置浅色，并记录一次非敏感诊断。

### 3.2 语义 token

主题是**语义 token 的集合**，不是一堆散落颜色。至少覆盖：

| 类别 | token 示例 |
| --- | --- |
| 基础 | `window_bg`、`surface`、`surface_muted`、`text`、`text_muted`、`border` |
| 交互 | `accent`、`accent_text`、`selection_bg`、`caret`、`hover_bg`、`focus_ring` |
| 状态 | `danger`、`warning`、`success`、`info` |
| 编辑器 | `editor_bg`、`editor_text`、`code_bg`、`quote_border`、`link` |
| 表格 | `table_border`、`table_header_bg`、`table_selected_bg`、`table_drag_*` |
| 图谱 | `graph_backdrop`、`graph_node_*`、`graph_edge_*`、`graph_label` |
| Mermaid | `mermaid_placeholder_bg`、`mermaid_placeholder_text`、`mermaid_error` |
| 图片 | `image_overlay_bg`、`image_toolbar_*` |

规则：

- 组件只允许读语义 token，不允许写 `0x...` 颜色（测试/主题定义文件除外）。
- 字体族/字号/行高属于**排版 token**，首版可继续用常量；主题包后期可覆盖字体，
  但不得改变布局命中契约（行高/内边距由 host 决定）。
- 圆角/间距不进入主题，避免“主题一换布局就变”。

### 3.3 主题包（后期）

自定义主题 manifest 至少包含：

```json
{
  "id": "my-theme",
  "name": "My Theme",
  "version": "1.0.0",
  "api_version": 1,
  "appearance": "dark",
  "tokens": { "window_bg": "#111318", "text": "#e6edf3" }
}
```

- 只允许 token 覆盖，不执行任意代码；
- 加载时校验必需 token、颜色格式、对比度阈值；
- 主题包不参与同步，不写进 vault 正文；
- 插件主题与自定义主题共用同一 manifest/校验路径，插件不能绕过 host 校验。

## 4. 架构

### 4.1 状态归属

- 新增 app 层 `ThemeState`（或等价实体）：
  - 当前解析后的 `Theme`；
  - `ThemeMode` / `ThemeId`；
  - 系统外观监听句柄；
  - 设置持久化句柄。
- 通过 GPUI 的全局/根实体暴露只读 `Theme`；绘制函数优先接收 `&Theme`，
  避免在 paint 里反复查全局。
- 现有 `editor::theme::*` 自由函数保留为**迁移期兼容包装**（默认浅色），
  新代码一律走 token；迁移完成后删除或降为 `light()` 测试辅助。

### 4.2 持久化

- 主题偏好是**本机应用设置**，不同步、不写正文、不写同步 schema；
- 存储位置与现有应用设置一致（本地配置文件/设置表，具体实现定）；
- 只保存 `mode` + 可选 `theme_id`，不保存解析后的颜色；
- 读取失败/未知主题 → 回退 `System` 或 `Light`，不阻塞启动；
- 不记录敏感路径；主题 id 可进日志。

### 4.3 切换与失效

切主题时必须：

1. 更新 `ThemeState` 并通知所有窗口重绘；
2. 清空/失效 Mermaid 预览缓存（key 已含 theme id，改为真实 id）；
3. 图谱重绘（节点/边/标签颜色全部来自 token）；
4. 图片遮罩/工具条/表格拖拽 ghost 等浮层重绘；
5. 不重建编辑器文档、不丢选区/滚动/撤销栈；
6. 不写正文、不产生 `EditRequest`。

### 4.4 与 Mermaid/图谱/插件的关系

- **Mermaid**：`PREVIEW_THEME` 从常量改为真实 `theme_id`；切主题后旧缓存自然失效，
  重新渲染；暗色下需重新检查 SVG 文本/线条对比度（M3）。
- **图谱**：所有颜色从 token 取；暗色下重新检查边/节点/标签对比度。
- **插件**：host 提供只读 token 集；插件 UI 只能声明使用哪些 token，
  不直接拿 `Rgba` 任意绘制；第三方主题同样走 manifest 校验。
- **i18n**：主题设置页文案走 message id；语言与主题互不依赖。

## 5. 迁移路线

1. **P0 盘点**：列出 `theme.rs` 全部取色函数与调用点，定义 token 映射表；
   标注哪些是语义色、哪些是组件私有色。
2. **P1 主题骨架**：`Theme` / `ThemeMode` / `ThemeState` + 内置浅色；
   现有函数改为读默认主题；测试 token 完整性。
3. **P2 深色 + 跟随系统**：内置深色 token；系统外观监听；设置页切换；
   持久化；启动回退。
4. **P3 组件迁移**：按 shell → 编辑器 → 表格/任务 → 图片 → 图谱 → Mermaid
   顺序替换硬编码；每步保留截图/headless 断言。
5. **P4 缓存/性能**：Mermaid 缓存 key 接真实主题；切主题只失效必要缓存；
   避免全量重排。
6. **P5 自定义主题包**：manifest/校验/加载/错误回退；再评估插件主题。

## 6. 测试与验收

- **token 完整性**：每个内置主题必须提供全部必需 token；缺一即测试失败。
- **对比度**：文本/背景、边框/背景、强调色/背景达到最低对比度阈值（WCAG AA 文本）；
  不达标主题加载失败或内置主题修正。
- **切换不变量**：切主题不改变正文、选区、滚动位置、撤销栈、表格选中、Mermaid 源码。
- **缓存失效**：同一 Mermaid 源码在 light/dark 下 key 不同；切主题后重新渲染一次。
- **持久化**：重启后恢复；未知/损坏主题回退；`System` 跟随 OS 变化。
- **headless**：真实 `AppShell` 切主题、断言 token/缓存/选择不变；
  像素/DPI/字体仍必须 Windows 真机验收。
- **性能**：切主题不触发文档重解析/全量 shaping；大图/大表格只重绘可见部分。

## 7. 后果与风险

- 主题是**跨模块重构**，会触及 `theme.rs` 所有调用点，必须分切片推进，避免大 diff。
- 现有硬编码颜色很多，P0 盘点不完整会导致漏改；需要编译期/测试兜底。
- 深色下 Mermaid/图谱的对比度不能只靠“看起来还行”，需要样本与真机验收。
- 自定义主题包是安全边界：只允许 token，不允许代码/CSS/网络；加载失败必须回退。
- 每篇笔记主题被明确拒绝；如果未来要支持，需要单独 ADR，不能偷偷塞进正文。
- 插件主题 API 必须等 token 集稳定后再冻结，避免插件生态跟着改色。

## 8. 待决问题

1. 主题偏好持久化的具体位置（本地 JSON / 设置表 / 平台配置）与迁移策略。
2. 自定义主题包格式（JSON/TOML）、版本兼容与签名/来源信任。
3. 是否提供高对比度/色弱内置变体，以及是否复用同一 token 集。
4. 字体是否进入主题 token，还是只允许应用级字体设置（首版建议后者）。
5. 插件主题是否允许覆盖全部 token，还是只允许一组受限子集。

## 9. 实施记录（P1/P2，2026-11-18）

> 实施位于 `dev` 分支 HEAD `3b1fdf5` 之上的**未提交工作树**（按任务要求不 commit）。
> 只改 app 层与测试/docs；未触碰 `sync`/`relay`/`storage`/schema/wire，未新增依赖。
> 本记录对应 ADR 0021 §5 的 **P0（盘点）+ P1（骨架）+ P2（深色/跟随系统/设置页/持久化）**；
> P3 组件迁移、P4 性能、P5 自定义主题包仍未做（见 §9.8）。

### 9.1 文件与职责

| 文件 | 变更 |
| --- | --- |
| `crates/app/src/theme.rs`（新增） | `ThemeMode` / `ThemeId` / `Theme`（91 个语义 token）；内置浅色/深色；全局 `ThemeState`（`RwLock`）；`current_theme()` / `theme_id()` / `set_mode()` / `apply_system_appearance()`；本机 JSON 偏好（读写/原子替换/损坏回退）；测试钩子 `lock_for_test()` |
| `crates/app/src/theme_settings.rs`（新增） | 设置页「外观」三选一组件（AppShell / AppRoot 共用）；固定 selector 见 §9.5 |
| `crates/app/src/editor/theme.rs` | 保留全部排版常量；**81 个取色自由函数**改为读 `crate::theme::current_theme()` 的兼容包装（浅色值逐色不变）；新增 6 个 token 包装（Mermaid 占位/错误、图片遮罩、浮层遮罩） |
| `crates/app/src/editor/mermaid_preview.rs` | 删除 `PREVIEW_THEME`；`MermaidCacheKey::new` 用真实 `theme_id()`；新增纯函数 `new_for_theme()` 供确定性测试 |
| `crates/app/src/editor/glyph.rs` | 新增 `text_runs_with_color()` / `shape_row_styled_at_with_color()`：占位文案不再用正文色（深色下可读） |
| `crates/app/src/editor/element.rs` | 图片与 Mermaid 占位框分别取 `object_placeholder_*` / `mermaid_placeholder_*` |
| `crates/app/src/editor/view.rs` | Mermaid 失败文案取 `mermaid_error`、加载/等待取 `mermaid_placeholder_text`；图片放大遮罩取 `image_overlay_bg` |
| `crates/app/src/shell.rs` | 根视图订阅 `observe_window_appearance`（只一次）；设置页底部加「外观」区（不挤动既有同步/设备/轮换按钮）；设置遮罩取 `overlay_scrim_soft` |
| `crates/app/src/onboarding/app_root.rs` | 根视图订阅 `observe_window_appearance`；设置页标题下加「外观」区（与 relay/同步区并列）；遮罩取 `overlay_scrim` |
| `crates/app/src/search_overlay.rs` | 浮层遮罩取 `overlay_scrim` |
| `crates/app/src/main.rs` | 启动时 `theme::init_from_disk()`（读偏好失败不阻塞启动） |
| `crates/app/tests/headless_theme.rs`（新增） | 4 个 headless 集成测试（切换不变量 / Mermaid 重渲染 / System / 持久化） |
| `crates/app/tests/headless_theme_ui.rs`（新增，`dev-sync`） | 真实 AppShell 设置页点击切换（1 个测试） |

### 9.2 模型与默认值

- `ThemeMode { Light, Dark, System }`，持久化字符串 `light` / `dark` / `system`；
- `ThemeId` 是 `&'static str` 的 newtype（内置 `light` / `dark`；保持 `MermaidCacheKey: Copy`，
  自定义主题包 P5 再扩成 `String`）；
- **默认 `System`**：首次启动跟随 OS；无窗口/外观不可用时按 `Light` 回退（`ThemeMode::resolve`）；
- 浅色 token 是迁移前 `editor/theme.rs` 的**逐色拷贝**，没有视觉回归；深色是新增值。

### 9.3 token 映射（P0 盘点）

91 个语义 token；下表是 81 个兼容包装的映射。其余 10 个不直接对应旧函数，供新代码/
后续组件按语义取用：`surface_muted`、`text_muted`、`text_disabled`、`border`、`hover_bg`、
`focus_ring`、`success`、`info`、`editor_bg`、`editor_text`（其中 success/info 为后续状态条预留）。

| 旧函数 | 新 token |
| --- | --- |
| `window_bg()` | `window_bg` |
| `text_color()` | `text` |
| `selection_bg()` | `selection_bg` |
| `caret_color()` | `caret` |
| `status_bar_bg()` | `status_bar_bg` |
| `status_bar_text()` | `status_bar_text` |
| `sidebar_bg()` | `sidebar_bg` |
| `sidebar_border()` | `sidebar_border` |
| `sidebar_selected_bg()` | `sidebar_selected_bg` |
| `danger_text()` | `danger` |
| `menu_bg()` | `surface` |
| `input_bg()` | `input_bg` |
| `input_border()` | `input_border` |
| `input_selection()` | `input_selection` |
| `input_caret()` | `input_caret` |
| `code_bg()` | `code_bg` |
| `link_color()` | `link` |
| `task_box_bg()` | `task_box_bg` |
| `task_box_border()` | `task_box_border` |
| `task_box_checked_bg()` | `task_box_checked_bg` |
| `task_box_checked_border()` | `task_box_checked_border` |
| `task_box_check()` | `task_box_check` |
| `quote_bar_color()` | `quote_border` |
| `quote_text_color()` | `quote_text` |
| `search_highlight_bg()` | `search_highlight_bg` |
| `search_highlight_text()` | `search_highlight_text` |
| `search_warn_text()` | `warning` |
| `search_progress_track()` | `search_progress_track` |
| `search_progress_fill()` | `search_progress_fill` |
| `search_toggle_active()` | `accent` |
| `search_toggle_active_text()` | `on_accent` |
| `search_group_selected()` | `search_group_selected` |
| `object_placeholder_bg()` | `object_placeholder_bg` |
| `object_placeholder_border()` | `object_placeholder_border` |
| `object_label_color()` | `object_label_color` |
| `object_selected_border()` | `object_selected_border` |
| `object_toolbar_bg()` | `object_toolbar_bg` |
| `object_toolbar_border()` | `object_toolbar_border` |
| `object_toolbar_text()` | `object_toolbar_text` |
| `table_actions_bg()` | `table_actions_bg` |
| `table_actions_border()` | `table_actions_border` |
| `table_actions_text()` | `table_actions_text` |
| `table_actions_text_disabled()` | `table_actions_text_disabled` |
| `table_drag_handle_bg()` | `table_drag_handle_bg` |
| `table_drag_handle_hover_bg()` | `table_drag_handle_hover_bg` |
| `table_drag_indicator()` | `table_drag_indicator` |
| `table_axis_selection_bg()` | `table_axis_selection_bg` |
| `table_axis_selection_border()` | `table_axis_selection_border` |
| `table_drag_handle_selected_bg()` | `table_drag_handle_selected_bg` |
| `table_drag_ghost_bg()` | `table_drag_ghost_bg` |
| `table_drag_ghost_border()` | `table_drag_ghost_border` |
| `table_drag_ghost_label_bg()` | `table_drag_ghost_label_bg` |
| `table_drag_ghost_label_text()` | `table_drag_ghost_label_text` |
| `link_confirm_overlay()` | `link_confirm_overlay` |
| `link_confirm_primary_bg()` | `link_confirm_primary_bg` |
| `link_confirm_primary_text()` | `link_confirm_primary_text` |
| `link_notice_bg()` | `link_notice_bg` |
| `link_notice_text()` | `link_notice_text` |
| `table_border()` | `table_border` |
| `table_bg()` | `table_bg` |
| `table_header_bg()` | `table_header_bg` |
| `table_cell_selected_bg()` | `table_cell_selected_bg` |
| `table_cell_selected_border()` | `table_cell_selected_border` |
| `graph_backdrop()` | `graph_backdrop` |
| `graph_toolbar_bg()` | `graph_toolbar_bg` |
| `graph_node_bg()` | `graph_node_bg` |
| `graph_node_border()` | `graph_node_border` |
| `graph_node_active_bg()` | `graph_node_active_bg` |
| `graph_node_active_text()` | `graph_node_active_text` |
| `graph_node_dim_bg()` | `graph_node_dim_bg` |
| `graph_node_dim_border()` | `graph_node_dim_border` |
| `graph_label_text()` | `graph_label_text` |
| `graph_edge()` | `graph_edge` |
| `graph_edge_active()` | `graph_edge_active` |
| `graph_edge_dim()` | `graph_edge_dim` |
| `mermaid_placeholder_bg()`（新） | `mermaid_placeholder_bg` |
| `mermaid_placeholder_text()`（新） | `mermaid_placeholder_text` |
| `mermaid_error()`（新） | `mermaid_error` |
| `image_overlay_bg()`（新） | `image_overlay_bg` |
| `overlay_scrim()`（新） | `overlay_scrim` |
| `overlay_scrim_soft()`（新） | `overlay_scrim_soft` |

### 9.4 持久化

- 路径：`MENGHUNOTE_THEME_PATH`（测试/便携）→ Windows `%LOCALAPPDATA%\menghunote\theme.json`
  → macOS `~/Library/Application Support/menghunote/theme.json`
  → Linux `$XDG_CONFIG_HOME/menghunote/theme.json`（缺省 `~/.config/...`）；
- 格式（手写 JSON，默认构建不引入 `serde_json`）：
  `{ "version": 1, "mode": "system", "theme_id": null }`；
- 只存 `mode`（+ 可选 `theme_id`）；不存解析后的颜色、不写正文、不进同步；
- 原子写：同目录 `theme.json.tmp` → rename；失败退化为覆盖写，不影响当前会话；
- 读取：文件缺失/不可读/JSON 损坏/未知版本/未知模式 → `ThemeMode::System`（无窗口时浅色），
  **不阻塞启动**（`main.rs` 第一行之后调用，不 panic）。

### 9.5 设置页与系统外观

- **AppRoot 设置页**（产品入口，`dev-sync` 构建）：标题/banner 之后并列「外观」区；
- **AppShell 设置页**（测试/自动化）：放在面板最底部，避免把既有同步/设备/轮换按钮挤出可视区；
- 固定 selector：`settings-appearance`、`settings-theme-light`、`settings-theme-dark`、
  `settings-theme-system`、`settings-theme-current`；
- 点击走 `theme::set_mode(mode, window.appearance(), cx)`：只改全局 `Copy` 状态 + 持久化 +
  `refresh_windows()`；**不重建 `AppShell`/`EditorView`**、不写正文、不产生 `EditRequest`；
- 系统外观：`AppShell` / `AppRoot` 根视图第一次 `render` 时各订阅一次
  `Context::observe_window_appearance`；OS 切换 → `apply_system_appearance(window.appearance())`
  → 解析结果变化才 `refresh_windows()`；启动无窗口时回退浅色。

### 9.6 Mermaid / 派生视图

- `MermaidCacheKey::new` 的 `theme` 字段 = `crate::theme::theme_id().as_str()`（真实 `light`/`dark`）；
- 浅色/深色 key 不同 → 旧缓存不再命中 → 切主题后重新渲染**一次**；再切同一主题命中缓存；
- 占位框/失败文案/图片遮罩/工具条/表格/任务框/图谱/状态栏/侧栏全部从 token 取色；
- 原先散落的 4 处 `gpui::rgba(...)` 遮罩已收口为 token（设置/搜索浮层、图片放大、设置浅遮罩）。

### 9.7 测试与对比度

验证命令与结果（Linux headless，`CARGO_INCREMENTAL=0`、`CARGO_PROFILE_DEV_DEBUG=0`）：

| 命令 | 结果 |
| --- | --- |
| `cargo fmt --all -- --check` | 通过 |
| `cargo clippy -p menghunote-app --all-targets --features dev-sync -- -D warnings` | 通过（0 warning） |
| `cargo test -p menghunote-app` | **363 passed / 0 failed**（基线 352，+11） |
| `cargo test -p menghunote-app --features dev-sync` | **608 passed / 0 failed**（基线 596，+12） |

新增测试：

- `theme::tests`：token 唯一/完整（91 个）、浅色逐色抽查、WCAG 对比度、模式解析、偏好往返/损坏回退、文件读写；
- `headless_theme.rs`：切主题保持正文/选区/滚动/撤销栈/表格选中/Mermaid 源码；Mermaid 深浅 key 不同且重渲染一次；
  `System` 跟随 `apply_system_appearance`；持久化→模拟重启→损坏回退；
- `headless_theme_ui.rs`：真实 `AppShell` 设置页点击「深色/浅色/跟随系统」，断言主题 id、写盘、
  设置页仍打开、编辑器实体 id 不变、文档状态与本地编辑代数不变、撤销栈仍可用。

**深色对比度（WCAG 2.x，相对亮度；半透明先合成到不透明背景）**：

| 前景/背景 | 比值 | 阈值 |
| --- | --- | --- |
| `text` / `window_bg` | 16.02 | 4.5 |
| `text` / `surface` | 14.64 | 4.5 |
| `text_muted` / `surface` | 5.62 | 4.5 |
| `accent` / `window_bg` | 7.49 | 4.5 |
| `on_accent` / `accent` | 7.49 | 4.5 |
| `border` / `surface` | 3.77 | 3.0 |
| `input_border` / `surface` | 5.62 | 3.0 |
| `danger` / `surface` | 6.86 | 4.5 |
| `warning` / `surface` | 6.85 | 4.5 |
| `success` / `surface` | 6.81 | 4.5 |
| `search_highlight_text` / `search_highlight_bg` | 7.92 | 4.5 |
| `graph_label_text` / `graph_backdrop` | 16.02 | 4.5 |
| `graph_edge` / `graph_backdrop` | 4.12 | 3.0 |
| `text` / `selection_bg` | 7.19 | 4.5 |
| `input_caret` / `input_selection` | 5.32 | 4.5 |
| `text` / `table_cell_selected_bg` | 9.72 | 4.5 |
| `table_drag_ghost_label_text` / `table_drag_ghost_label_bg` | 16.02 | 4.5 |

浅色：`text`/白 15.80、`text_muted`/白 6.11、`accent`/白 5.19、`input_border`/白 6.39、
`danger`/白 5.36、`warning`/白 4.83、`search_highlight` 12.93、选区正文 12.16、表格选中正文 12.95。
**例外（有意保留）**：浅色装饰性分隔线 `border`/白 ≈ 1.45、图谱边线 ≈ 1.53 ——
ADR 0021 要求浅色逐色保持，改它会破坏"现有外观零回归"；测试对浅色只设回归下限，
对深色分隔线/边线要求 ≥ 3:1（已达标）。

### 9.8 已知限制 / 后置

- **自定义主题包（P5）未做**：`ThemeId` 目前只认内置 `light`/`dark`；manifest/校验/加载/回退后置；
- **Mermaid SVG 内容仍是 merman 默认（浅色）配色**：本切片只保证缓存 key 用真实主题并重渲染，
  暗色 SVG 配色属于 ADR 0019 M3；容器/占位/工具条的对比度已由 token 覆盖；
- **真实 OS 外观切换未真机验证**：headless 平台外观恒为 Light，测试用
  `apply_system_appearance()` 驱动同一状态机；Windows/macOS 真机仍需验像素/DPI/切换时机；
- **默认 demo 构建（无 `dev-sync`）没有设置页**（迁移前就如此）：它仍默认 `System` 并跟随 OS，
  但没有手动切换入口；产品入口（`AppRoot`，`dev-sync`）有完整「外观」区；
- 字体/字号/行高/圆角/间距仍不进入主题（ADR 0021 §3.2 的既定边界）；
- i18n 文案未接线（与现有设置页一致，中文硬编码；ADR 0020 后续统一）；
- 主题不参与导出/同步；不记录敏感路径，主题 id 可进日志。
