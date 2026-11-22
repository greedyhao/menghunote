# UI 重构：设置页分类导航 / 最左侧图标栏 / 侧栏排序

> 2026-11-18。范围：`crates/app`（shell / AppRoot 设置页 / 第一方插件设置接线）+
> `crates/storage/src/tree_view.rs`（树读接口的展示顺序）+ `crates/i18n` 文案。
> **不改 sync/relay/storage schema/wire/协议**；不新增重依赖；不 commit/打包。
> 三项都是**纯投影层**改动：不写正文、不写结构、不产生 `EditRequest`。

## 1. 设置页拆分（AppRoot 产品设置）

现状：AppRoot 设置是一张从上滚到下的长卡片。现在改成 Obsidian「选项」式
**左导航 + 右侧只渲染当前分类**：

```
┌ 设置（overlay，限高、拦截滚轮，不穿透到底层编辑器）─────────────┐
│ 标题 + DEV banner                                             │
│ ┌ settings-nav（固定 152px，独立滚动）┬ 内容区（独立滚动）────┐ │
│ │ 设置                                │ settings-section-<key> │ │
│ │ 外观（默认）                        │ 只渲染当前分类          │ │
│ │ 语言                                │                        │ │
│ │ 同步 / 实例 / Relay / 安全          │                        │ │
│ │ 插件 / 关于                         │                        │ │
│ │ ─── settings-nav-divider（1px）     │                        │ │
│ │ 模板（仅启用时）                    │                        │ │
│ │ 日记（仅启用时）                    │                        │ │
│ └─────────────────────────────────────┴────────────────────────┘ │
│ 底栏（固定在面板底部，所有分类都可见）：settings-error / notice /  │
│ settings-save / settings-cancel                                   │
└───────────────────────────────────────────────────────────────────┘
```

| 分类 key | 导航 selector | 右侧内容 selector | 说明 |
| --- | --- | --- | --- |
| `appearance`（固定） | `settings-nav-appearance` | `settings-section-appearance` | `settings-appearance` + `settings-theme-*` |
| `language`（固定） | `settings-nav-language` | `settings-section-language` | `settings-language` + `settings-language-*` |
| `sync`（固定） | `settings-nav-sync` | `settings-section-sync` | `settings-toggle-sync`（**同步内容容器**，进入分类即渲染）+ 同步面板（设备/邀请/轮换） |
| `instances`（固定） | `settings-nav-instances` | `settings-section-instances` | `settings-instance-<i>` / `-active` / `settings-api-version` / 公开值复制 |
| `relay`（固定） | `settings-nav-relay` | `settings-section-relay` | `settings-relay` / `settings-mode-*`（HTTPS 由权威 parser 校验；2026-11-20 起不再有 `settings-remote-consent` 勾选） |
| `security`（固定） | `settings-nav-security` | `settings-section-security` | `settings-fingerprint*` / `settings-security-category` / `settings-toggle-management`（V2 实验入口，`dev-sync` + 实验变量才渲染） |
| `plugins`（固定） | `settings-nav-plugins` | `settings-section-plugins` | `settings-plugin-templates*` / `settings-plugin-daily_notes*`（第一方核心插件总开关） |
| `about`（固定） | `settings-nav-about` | `settings-section-about` | `settings-about-version` / `settings-about-build`（只读） |
| `templates`（插件，仅启用时） | `settings-nav-templates` | `settings-section-templates` | `settings-templates*`（第一方插件） |
| `daily_notes`（插件，仅启用时） | `settings-nav-daily_notes` | `settings-section-daily_notes` | `settings-daily-notes*` |

- **默认选中「外观」**；上次分类记在本机 `settings-ui.json`（2026-11-19 起持久化，
  见 [`startup-resume-settings-memory.md`](startup-resume-settings-memory.md)），
  缺失/损坏/未知 key 回退「外观」，已关闭的插件分类 clamp 到「插件」。
- 左栏顺序：固定分类（外观/语言/同步/实例/Relay/安全/插件/关于）→
  `settings-nav-divider`（1px 主题线，整行不可点）→ 已启用的插件分类（模板/日记）。
  固定分类永远显示，插件分类只在对应 `enabled` 为真时显示。
- 旧 selector（`settings-theme-*` / `settings-language-*` / `settings-templates*` /
  `settings-daily-notes*`）在**对应分类打开时**仍然存在，既有 headless 测试语义不变。
- `settings-error` / `settings-notice` / `settings-save` / `settings-cancel` 搬到底栏：
  保存/关闭在**任何分类**下都可达（否则"保存 relay"只在 Relay 分类可见，容易点不到）。
- 功能没有挪走/删除：同步面板、实例列表、relay 编辑、安全指纹、V2 实验入口都仍可达
  （V2 管理入口放在「安全」分类下）。
- `AppShell` 的 dev-sync 设置页（`settings-panel`）**暂不拆**，行为不变。
  **2026-11-20 更新**：生产 `AppRoot` 创建的 Main 把设置入口交给宿主——点底栏
  `sync-status` 只发 `AppShellEvent::OpenSyncSettings`，由 `AppRoot` 打开本页并选中
  「同步」分类（不再打开 `settings-panel` 旧组合页；直接构造 `AppShell` 的测试仍走旧页）。

实现要点（`crates/app/src/onboarding/app_root.rs`）：

- `SettingsSection`（10 个分类）提供 `key()` / `parse()` / `label()` / `is_plugin()` /
  `is_enabled()`（文案走 `ui_text`；`parse()` 用于本机分类记忆的未知 key 回退）；导航按 `FIXED`（8 个固定）→ 分界线 → `PLUGIN_SECTIONS`（2 个插件，
  过滤未启用）渲染；`ALL` = 固定在前、插件在后的全部枚举。
- `render_settings` 只对 `self.settings_section` 建右侧内容；`settings-content` 是滚动容器，
  外层 `dev-sync-settings` 是 `overflow_hidden` + 窗口高度 clamp（限高保持）。
- 覆盖层滚轮仍然 `stop_propagation`（滚设置页不带动底层笔记滚动）。
- relay 输入框的"打开即聚焦"只在 Relay 分类激活时消费（不聚焦未渲染的输入框）。

## 2. 最左侧图标栏（Ribbon）

侧栏**左边**新增一条 44px 宽的窄图标栏（Obsidian ribbon 风格），只放**快捷操作**：

| selector | 图标（lucide） | 动作 |
| --- | --- | --- |
| `ribbon-daily` | `calendar` | `AppShell::open_daily_note`（今日日记；不存在则创建并打开） |
| `ribbon-template` | `layout-template` | `open_template_picker(NewNote)`（从模板新建；与旧文字按钮同一条路径） |
| `ribbon-graph` | `network` | `open_graph`（全屏图谱覆盖层） |
| `ribbon-search` | `search` | `open_search`（Ctrl+K 的同一个浮层） |

- **设置不在 ribbon 里**（用户明确要求）：设置入口仍然是侧栏底部（AppRoot 的
  `app-root-settings`；`AppRoot` 的绝对定位已按 `RIBBON_WIDTH` 右移，不压住 ribbon）。
- 原第二行动作行的文字按钮「从模板新建…」「今日日记」**已移除**（避免两套同样入口）；
  旧 selector `sidebar-new-from-template` / `sidebar-daily-note` / `sidebar-graph` 作为
  **兼容别名**挂在外层包装上，指向同一个 ribbon 按钮（既有 headless 测试仍可点击）。
- 图标实现：`crates/app/assets/icons/*.svg`（新增，lucide 24×24、`stroke="currentColor"`），
  `include_bytes!` 后由 `gpui::svg().data(...)` + `text_color(主题 token)` 绘制。
  **没有新增依赖边**：不引入 `gpui_component::Icon`（它要求组件全局 Theme），
  `Svg` 直接吃裸字节、颜色走主题 token。hover 有背景色、光标手形；tooltip 是 11px 的
  自绘小面板（`RibbonTooltip`），文案走 `ui_text`。
- 布局：`render()` 的 `content` 变成 `ribbon + sidebar + main`；侧栏宽度仍是 220px
  （`SIDEBAR_WIDTH` 未变），ribbon 宽度单独常量 `RIBBON_WIDTH = 44`。

## 3. 侧栏排序（目录优先）

### 3.1 数据与顺序

- `TreeEntry` 增加两个字段：`created_at: Option<i64>` / `updated_at: Option<i64>`
  （来自 `notes` 表 join；**目录为 `None`**，因为投影表里目录没有 `notes` 行）。
- `list_tree`（rank 顺序）**语义不变**：同步/导出/基线对账继续读它。
- 新增 `list_tree_sorted(vault, TreeSort)` = 侧栏展示顺序：**每个层级目录永远在笔记前面**，
  同组内按排序键；父子层级/`depth`/"父节点后紧跟全部后代"的连续区间性质完全保留
  （折叠 `visible_rows` 依赖它）。空/脏数据（父缺失、成环）处理与 `list_tree` 相同。
- 排序键（`TreeSort`，持久化用 `as_str()`）：

| 键 | 含义 |
| --- | --- |
| `name_asc`（默认） | 文件名 A-Z（大小写不敏感 → 原串 → node_id 兜底） |
| `name_desc` | 文件名 Z-A |
| `updated_newest` / `updated_oldest` | 修改时间 新→旧 / 旧→新 |
| `created_newest` / `created_oldest` | 创建时间 新→旧 / 旧→新 |

时间键下**目录没有时间**：目录组内仍按标题 A-Z（不参与时间比较）。
中文标题按码点稳定排序（与 Rust `String` 序一致），同大小写/同标题用 node_id 兜底。

### 3.2 UI

- 侧栏标题行右侧新增「排序」按钮（图标 + 文字）：`sidebar-sort`。
- 点开弹出菜单 `sidebar-sort-menu`（根层绝对定位，避免被 `overflow_y_scroll` 裁掉），
  6 项：`sidebar-sort-name_asc` / `-name_desc` / `-updated_newest` / `-updated_oldest` /
  `-created_newest` / `-created_oldest`；**当前项显示 ✓ 并高亮**；点空白关闭。
- 切换排序：只重建 `self.tree`（`refresh_tree`），按 node_id 保持**折叠集合**、
  **选中节点**、**当前打开的笔记**；`editor-current-doc` 不重建、不丢选区/滚动。

### 3.3 持久化

- 本机 `sidebar.json`（平台配置目录；`MENGHUNOTE_SIDEBAR_PATH` 可覆盖）：

  ```json
  { "version": 1, "sort": "name_asc" }
  ```

- **只存排序键**；原子写（同目录临时文件 → rename，失败退化为覆盖写）；
  文件缺失/损坏/未知版本/未知排序键一律回退 `name_asc`，不报错、不阻塞启动。
- 加载时机：**产品根视图** `AppRoot::new` 调 `init_sidebar_sort_from_disk()` 读一次；
  `AppShell` 只读这份缓存（构造期不读盘）。原因：`AppShell` 被大量 headless 测试直接
  构造，若构造期读用户配置目录，"默认顺序"的断言会随机器上残留的 `sidebar.json` 变化
  （与 `theme.json`/`language.json` 只在 `main.rs` 初始化的纪律一致）。
  测试用 `init_sidebar_sort_from_path(path)` + `lock_sidebar_for_test()` 显式隔离。
  **不同步**（不进 vault / 不进 sync outbox / 不写正文）。

## 4. 测试

实测（本机 Linux，`CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0`）：

| 命令 | 结果 |
| --- | --- |
| `cargo fmt --all -- --check` | 干净 |
| `cargo clippy -p menghunote-app -p menghunote-storage --all-targets --features dev-sync -- -D warnings` | 干净 |
| `cargo test -p menghunote-app` | **508 passed**（50 个 target；本页切片的 505 + 插件设置页 2 + 分类分组单测 1） |
| `cargo test -p menghunote-app --features dev-sync` | **667 passed**（本页切片的 664 + 插件设置页 2 + 分类分组单测 1） |
| `cargo test -p menghunote-storage` | **201 passed**（基线 195 + 新增 6） |
| `cargo test -p menghunote-i18n` | 16 passed |

新增：

- `crates/storage/tests/tree_view.rs` +6：
  目录优先（每层）/ 大小写与中文稳定 / 六种排序键 / 时间戳 join / 排序后父子连续区间 /
  已删除不返回 + 目录时间戳为 `None` / 键往返解析。
- `crates/app/tests/headless_sidebar_sort.rs` +8：
  ribbon 在侧栏左侧且宽度固定（无设置按钮、旧 selector 是同一按钮的别名）/
  ribbon 四个按钮复用既有动作（搜索浮层、图谱、模板选择器、今日日记）/
  默认目录优先 + A-Z / 排序菜单 6 项 + 点空白关闭 / 六种排序键重排 + 写盘 /
  重排保持折叠与打开笔记 / 偏好往返 + 损坏回退 / 启动读盘 + 损坏回退。
- `crates/app/tests/onboarding_ui.rs` +1：`settings_nav_switches_sections_and_keeps_old_selectors`
  （8 个固定分类 + 分界线 + 2 个插件分类、默认外观、顺序断言、每个分类只渲染自己、
  旧 selector 在对应分类仍存在、同步分类直接渲染面板、内容区不超过面板高度、
  会话内记住上次分类）。
- `crates/app/src/shell.rs` 单测：见 `headless_sidebar_sort`（偏好解析/写盘为纯函数）。

既有测试更新（语义跟着产品走）：

- 侧栏下标从 rank 顺序换成展示顺序（demo 树：**0 目录 / 1 目录内笔记 / 2 根级笔记**）；
  `tree-row-N` 的"先序下标"现在是 `list_tree_sorted` 的下标。
- AppRoot 设置测试先点 `settings-nav-<section>` 再断言该分类里的旧 selector。
- 窄窗口表格测试的窗口宽度各 +44px（ribbon 宽度），编辑区宽度仍等于旧基线，几何契约不变。

## 5. 已知限制

- 设置分类**不持久化**（只记本次会话），重启回到「外观」。
- `AppShell` 的 dev-sync 设置页仍是长卡片（本切片只拆产品 AppRoot；不破坏既有面板）。
- 排序是按**标题/时间戳排序后重建 DFS**，没有"手动拖拽/自定义顺序"；rank 顺序仍由结构
  写入决定，但侧栏不再展示它。
- 时间排序用 `notes.updated_at`（结构 `edited_at.max(created_at)`）；正文编辑是否立即
  推进 `updated_at` 取决于投影列（本切片未改投影/物化）。目录没有时间戳，只能按标题。
- 排序菜单只能点空白关闭（没有 Esc 关闭）：与既有右键菜单的"点外部关闭"一致。
- 排序偏好只在**产品根视图**（`AppRoot`）启动时读盘；纯 dev-vault 的 `AppShell` 命令行
  模式（dev-only）启动仍是默认顺序，但用户改了排序仍会写盘（下次产品启动读到）。
- 设置页导航没有图标（Obsidian 有）；本切片优先保证分类/可达性/selector 契约。
- ribbon 悬停提示走自绘小面板，没有键盘可达性（无障碍语义仍是后续项）。


## 6. 后续（2026-11-18）：插件总开关页 / 分界线 / 同步直显

在上面的分类导航之上又做了三项调整（仍然只改产品 `AppRoot` 设置页）：

1. **「插件」固定分类**（`settings-nav-plugins` / `settings-section-plugins`）：
   参考 Obsidian「核心插件」页，只列**有运行时开关**的第一方插件——
   模板（`settings-plugin-templates*`）与日记（`settings-plugin-daily_notes*`）；
   Mermaid/图谱没有可切换的运行时开关，不列进来。
   开关直接读写 `templates.json` / `daily-notes.json` 的 `enabled`（**没有第二份状态、
   不改 JSON 格式**）：关闭后左栏对应分类立即消失；若当前正停在该插件分类，自动回退到
   「插件」页（不出现空白页）；侧栏 ribbon 入口继续按同一份 `enabled` 隐藏。
2. **分界线**：固定分类之后、插件分类之前渲染 `settings-nav-divider`
   （1px `sidebar_border` 线 + 上下间距，没有点击处理器）。
3. **同步分类直显面板**：删除「已展开/收起」标题行与 `settings_sync_open` 状态；
   选中「同步」即渲染同步面板。`settings-toggle-sync` 保留为**同步内容容器**
   （兼容既有 selector 契约），内部 `dev-sync-*` selector 全部不变；
   没有可用实例时容器里是固定说明文案（`settings.sync.unavailable`）。

新增/更新测试：

- `crates/app/tests/headless_notes_plugins_ui.rs` +2：
  `plugins_page_toggles_hide_nav_and_persist_to_disk`（插件页 selector、关闭后左栏隐藏、
  `templates.json` 写盘、模拟重启读盘、重新开启恢复、日记同理）与
  `disabling_active_plugin_section_falls_back_to_plugins`（停在模板分类关闭模板 →
  回退插件页且内容非空）。
- `onboarding_ui` 的设置导航测试改为 `FIXED`（8）→ 分界线 → 插件（2）的顺序断言，
  并断言同步分类直接渲染 `dev-sync-panel`、`settings-toggle-sync` 是内容容器。
- `onboarding_create_link` 的辅助函数不再点「展开」。

本轮实测：app 默认 **508** / `--features dev-sync` **667** / i18n **16**；fmt/clippy 干净。
