# 原生 headless UI 测试（`#[gpui::test]` + `TestAppContext`）

> 2026-11-13 起；入口 `crates/app/tests/headless_ui.rs`；依赖见 `crates/app/Cargo.toml` 的 `[dev-dependencies]`。
> 背景：`ui_smoke.ps1` 只能在 Windows 真机上跑且慢，"侧栏树画出几项""根视图有没有尺寸"
> 这类回归没必要等真机。GPUI 自带 `TestAppContext`（`gpui-kit` 的 `test-support` 特性），
> 可以在进程内**不开真实窗口/不碰 GPU** 地跑真实视图的 Render/prepaint/paint，并读出布局。

## 能测什么

- **状态**：真实 `AppShell` + 真实 `Vault`（临时库）跑完整启动路径（读 `list_tree`、
  打开第一篇笔记、建 `SessionDriver`/`EditorController`）后各层的状态；
- **布局**：`debug_selector("...")` 标记的元素在 `VisualTestContext::debug_bounds` 里有
  非零 bounds（本机测试平台窗口是 1920×1080，所以能直接断言"占满窗口"这类事实）；
- **事件（第二批起已实测）**：`simulate_click(position, modifiers)` 打左键、
  `simulate_mouse_down(position, MouseButton::Right, modifiers)` 打右键、
  `simulate_keystrokes("ctrl-k" / "escape")` 打真实按键。它们走平台的命中/焦点链路，
  不是"直接调业务函数"：测试证明的是**绑定与命中都可达**；
- **文本输入（第三批起已实测）**：`simulate_keystrokes("a")` 先派发 `KeyDown`，若事件没被
  消费，gpui 会把 `key_char` 交给**当前聚焦元素**的 `EntityInputHandler::replace_text_in_range`
  ——与真实平台键入同一条入口。`editor_keystroke_inserts_character_and_persists` 就是靠
  这条链把字符写进正文并落库的。**仍然测不到 IME**（候选窗、组合串、`WM_IME_*`）；
- **多帧**：可以反复 `run_until_parked()` + 再查 bounds，验证"点开菜单后多了一个面板"。

## 不能测什么（这些仍靠 Windows 真机 `ui_smoke.ps1`）

- **像素**：测试平台只出 frame 的调试数据，不做真实栅格化。颜色/抗锯齿/圆角/阴影一律测不到；
- **IME**：输入法候选窗、组合串上屏、`WM_IME_*` 全部不经过测试平台（历史上已经踩过：
  `WM_CHAR` 注入"能打字"≠ 输入法可用）；
- **DPI**：没有真实显示器，scale factor 恒为 1.0；150% 缩放下的度量/坐标偏差测不出；
- **真实窗口管理**：窗口层级、焦点抢占、置顶、多显示器、`PrintWindow` 的旧 surface 问题。
  测试里"窗口"只是一个 `AnyWindowHandle`。

一句话：**UI 逻辑（状态/布局/事件）用本测试；"看起来对不对"仍然必须真机。**

## 运行命令

```bash
cargo test -p menghunote-app --test headless_ui            # 只跑本文件的 5 条
cargo test -p menghunote-app                               # 该 crate 全量（含单元测试与其它集成测试）
cargo test -p menghunote-app --test headless_ui -- --nocapture   # 想看 println 的 bounds/节点数
```

不需要 `#[ignore]`，Linux 上可直接跑（链接依赖见下）。测试是**确定性**的：
不 `sleep`、不依赖系统时间与窗口焦点，每个测试用独立临时库。

### Linux 链接依赖

`gpui-pre-linux`（真实平台后端）在 Linux 上链接系统 xkbcommon。缺库时的报错是
`rust-lld: error: unable to find library -lxkbcommon` / `-lxkbcommon-x11` —— 这是**环境**
缺库，不是测试逻辑失败。

Debian/Ubuntu 系安装（包名）：

```bash
sudo apt install libxkbcommon-dev libxkbcommon-x11-dev
# 部分发行版还需要：libxcb1-dev libfreetype-dev（freetype-sys 由 pkg-config 找）
```

其它发行版按各自的名字装 xkbcommon 的**开发包**（提供 `.so` 与 `pkgconfig`）。

> 本地应急（不写进仓库、不提交）：如果机器上暂时没有这些包，可以临时指向一个已解包的
> sysroot，用环境变量让 `cc`/pkg-config 找到库与 `.pc` 文件，例如
> `LIBRARY_PATH=<sysroot>/usr/lib/x86_64-linux-gnu PKG_CONFIG_PATH=<sysroot>/usr/lib/x86_64-linux-gnu/pkgconfig cargo test ...`。
> 这只是本机验证手段；**不要**把任何本机绝对路径、`build.rs` 改动或链接参数写进仓库。

## 固定 selector 约定（第二批起）

`debug_bounds` 只接受 `&'static str`，所以 selector 必须是**固定字符串**（不能把 doc_id
拼进去）；每个 `div` 也只能记**一个** `debug_selector`（后一次调用覆盖前一次）。

| selector | 挂在哪个元素 | 用途 |
| --- | --- | --- |
| `ribbon` | 最左侧图标栏容器（宽 44px，在侧栏**左边**） | 存在/宽度/位置 |
| `ribbon-daily` / `ribbon-template` / `ribbon-graph` / `ribbon-search` | ribbon 的四个快捷操作（今日日记/从模板新建/图谱/搜索） | 拿按钮 bounds 去点击 |
| `sidebar` | 侧栏容器（`render_sidebar` 根 div） | 侧栏存在/宽度 |
| `sidebar-sort` / `sidebar-sort-menu` / `sidebar-sort-<key>` | 排序按钮 / 弹出菜单 / 6 个排序项（`name_asc` 等） | 切排序、断言菜单开合 |
| `tree-row-<先序下标>` | 每个结构树行（下标 = **`list_tree_sorted` 的展示顺序**：目录优先 + 当前排序键，**不是**显示行号，也不是 `list_tree` 的 rank 顺序） | 拿行 bounds 去点击 |
| `tree-row-<先序下标>-selected` | 该行的标题子 div（`selected` = 当前打开的笔记） | 断言"切到了哪一篇" |
| `editor-pane` | 右侧编辑器面板包装 div（空态也有） | 编辑区存在 |
| `editor-current-doc` | `EditorView` 渲染根（一帧只有一个） | 有打开中的文档 |
| `search-overlay` | 搜索浮层根（`absolute inset_0`） | `Some` = 打开 / `None` = 关闭 |
| `search-input` | 自绘搜索输入框根 | 输入框挂上了 |
| `search-results` / `search-empty` | 结果容器 / 空态占位 | 结果区结构存在 |
| `context-menu` | 右键菜单面板（不含遮罩） | 菜单开/关 |
| `menu-item-<渲染下标>` | 每个菜单项（0 起，按**渲染顺序**：目录 = 重命名/新建笔记/新建文件夹；笔记 = 重命名/删除） | 拿菜单项 bounds 去点（`menu_hit` 是几何命中，点中心才落在该项上） |

为什么选中标记挂在**行内的标题子 div** 上：`div` 只能带一个 selector，行本身要用
`tree-row-N` 去点击；换成 `tree-row-N-selected` 会让"已选中的行"点不到自己的 bounds。

selector 只是测试元数据：`debug_selector` 只在 `debug_assertions` + `test-support`
下记录，release 构建里是 no-op；**不要**让业务逻辑依赖它。修改 selector 名属于破坏测试
契约，和改行为一样要看一遍本文件的表。

## dev-sync 面板（`dev-sync` feature，ADR 0009 切片 1）

面板只在 `--features dev-sync` **且**运行时门禁通过（`--dev-sync` 或 `MENGHUNOTE_DEV_SYNC=1`，
外加显式 `MENGHUNOTE_DEV_VAULT` 与 localhost relay）时挂到 `AppShell` 右上角（覆盖层，
不挤压编辑器）；未启用时这些 selector **全部不存在**（不渲染、不占布局）。通用命令要带 feature：

```bash
cargo test -p menghunote-app --features dev-sync --test dev_sync_ui
cargo test -p menghunote-app --features dev-sync           # Linux 链接依赖见上一节
```

| selector | 挂在哪个元素 | 用途 |
| --- | --- | --- |
| `dev-sync-panel` | 面板根（绝对定位覆盖层） | 面板存在 / 尺寸非零 |
| `dev-sync-state` | 状态行（状态 + 在途 op + 关闭门禁） | 断言存在（`debug_bounds` 读不到文本） |
| `dev-sync-relay` | relay 行 | 只显示公开地址（不含 secret） |
| `dev-sync-error` | 最近错误行 | 未实现/失败的明确原因 |
| `dev-sync-notice` | 最近一次按钮操作的结果 | 排队 / 被拒绝 / 同步完成 |
| `dev-sync-sync-state` | 持闸同步行（切片 4c-2） | 空闲 / 已持闸 / 完成（N 条变化） |
| `dev-sync-create` / `-join` / `-sync` / `-publish` / `-stop` | 5 个按钮 | `-sync` → 面板请求（见下）；其余 → `SyncService::try_command` |
| `dev-sync-devices` | 设备列表区域（ADR 0010 切片 2a-2） | 区域存在；内含固定警告文案（文本靠 `REVOKE_WARNING` 单测固定） |
| `dev-sync-refresh-devices` | 刷新设备按钮 | 点击 → `Command::ListDevices` |
| `dev-sync-member-<i>` / `-id` / `-status` | 第 i 行设备（i = `MemberView` 的稳定升序下标） | 行存在；本机标记写在 `-status` 的文案里（单测固定） |
| `dev-sync-remove-<i>` | 该行移除按钮 | **只**在 Active 且非本机时渲染；Join 设备置灰（点了只给原因） |
| `dev-sync-confirm-revoke` / `dev-sync-cancel-revoke` | 二次确认的确认/取消 | 确认才发 `Command::RevokeDevice` |

设备行 selector 带下标：`debug_bounds` 只接受 `&'static str`，测试侧用 `probe()`（`Box::leak`）
把 `format!` 出来的名字变成静态串；不要为此改生产 selector 的形状。

服务层（纯状态机、运行时门禁、worker、关闭票据）用 `crates/app/tests/dev_sync_service.rs`：
**不 sleep**，收敛点只有状态机的同步返回与 `SyncService::reap_worker()`（join 建立
happens-before，之后 `poll()` 必然拿到 worker 已发出的全部事件）。

### 持闸同步（切片 4c-2）：`crates/app/tests/dev_sync_sync_once.rs`

「同步」按钮**不直接**对 service 发 `SyncOnce`：面板只 `emit` 一个
`DevSyncPanelEvent::SyncRequested`，`AppShell` 在下一拍 `dev_sync_pump` 里走
`begin_external_commit → Held → try_command_with_gate(SyncOnce) → prepare/adopt/end`。
测试入口与判据（没有真实时钟，全部是**状态收敛**）：

- `shell.dev_sync_sync_once(cx)`：登记一次持闸同步（面板按钮走同一条路径）；
- `shell.dev_sync_pump(cx)`：泵一拍（面板/服务事件 → 闸门推进 → 活动视图）。它是
  headless 的"单步收敛点"，**不是** sleep 的替代；
- 收敛判据：`shell.dev_sync_sync_in_progress()` 转 `false`；
  `shell.dev_sync_last_sync()` 给出的 `SyncOnceReport`（`dispatched_under_gate` /
  `changes` / `error` / `structure_refreshed`）；
- 闸门可见性：`editor-sync-held` selector 只在 Held 时存在；
  `EditorView::dev_sync_is_held()` / `dev_sync_durable_head_seq()` / `dev_sync_doc_id()`
  是只读断言入口；
- 两个实例：同一 `TestAppContext` 里开**两个 window**（各自的 `AppShell` + 独立
  vault/身份/服务，同一进程/同一 relay），用 `VisualTestContext::from_window(handle)`
  切到各自窗口；工作目录/临时库纪律同上。

四条测试覆盖：A/B 双向文字（不重开文档）、传输失败 → 闸门释放 + 可重试 + 重复命令被拒、
面板按钮走同一路径、远端结构变更 → 同步后重投影 + 侧栏出现新行（验证不是假刷新）。

### 设备列表与移除设备（ADR 0010 切片 2a-2）：`crates/app/tests/dev_sync_devices.rs`

列表/移除的命令与事件（`ListDevices` / `RevokeDevice` → `DevicesListed` / `DeviceRevoked`）
仍走**同一个单操作 worker**，所以测试的收敛判据是"目标 op 收尾"：
`service.snapshot().in_flight.is_none()` **且** 列表内容满足预期（列表事件在 `Completed`
之前就发出，只看"设备变了"会把在途 op 当成已收尾，下一条命令会被 `Busy` 拒绝）。

- 面板打开/同步完成后的**自动刷新**由 40 ms 事件泵派发；headless 下 GPUI 定时器不会自己
  走时，测试用 `executor().advance_clock(SYNC_PUMP_INTERVAL)` 显式推进测试时钟再泵；
- 两个真实 `AppShell`（真实 dev relay）+ `VisualTestContext` 切窗口，覆盖：A 面板列出 A/B、
  A 标「本机」无按钮、B 行移除 → 二次确认（含取消）→ 确认 → B=已移除 + notice、
  Join 设备按钮点不动、被移除设备同步 `REVOKED`、重复移除不谎称「认证已刷新」。
**仍然测不到**真实 relay/TLS/多进程/真实时钟下的 UX；这些留在真机 `ui_smoke.ps1`
与 relay 生产路径的正负例。

## 如何新增一条测试

1. **测试放哪**：只涉及 `crates/app` 视图/启动路径的，加进 `crates/app/tests/headless_ui.rs`
   （GPUI 依赖已由 `[dev-dependencies]` 的 `test-support` 提供）；文件多了再拆。
   `#[gpui::test]` 的函数签名是 `async fn xxx(cx: &mut TestAppContext)`。
2. **起库**：用文件里的 `open_temp_dev_vault()`——它在 `CARGO_TARGET_TMPDIR`（`target/tmp`）
   下 `tempdir_in` 建独立目录再 `open_vault_at`，返回的 `TempDir` **必须**用 `let (dir, dev) = ...`
   接住并活到测试结束（`_dir` 也行，但不要只取 `dev.vault`）。
   **禁止** `tempfile::tempdir()`（`%TEMP%`）——AGENTS.md §6 的固定做法。
3. **起窗口**：照现有两条写 `cx.add_window_view(|_, cx| { let shell = cx.new(|cx| AppShell::new(vault, label, cx)); ShellHost { shell } })`。
   `ShellHost` 只做两件事：`size_full()` 让根视图占满窗口、`debug_selector("app-shell-host")`
   给断言一个可查的 key（`AppShell` 本身不是 div，没有 selector 可挂）。
   `add_window_view` 内部已经 `run_until_parked()`；要等 `AppShell` 的启动任务收敛再显式调一次。
4. **断言**：布局用 `cx.debug_bounds("...")`；状态优先从**存储/领域 API** 拿（如
   `list_tree`、`load_body_doc`），不要为了可测性在生产代码里加 getter。
   交互用 `simulate_*`，收敛判据仍是 `run_until_parked()` 或真实完成信号，**不要 sleep**。
5. **交互测试的固定套路**（照 `click_tree_row_switches_note` 抄）：
   ```rust
   let row = cx.debug_bounds("tree-row-2").expect("行没画出来"); // 先确认前置态存在
   cx.simulate_click(row.center(), Modifiers::default());        // 点真实坐标
   cx.run_until_parked();                                        // 唯一收敛点
   assert!(cx.debug_bounds("tree-row-2-selected").is_some());    // 再查状态
   ```
   按键同理用 `cx.simulate_keystrokes("ctrl-k")` / `("escape")`：**先**确保焦点在目标
   元素上（本文件里编辑器首帧会 `focus_self`；搜索浮层打开即聚焦输入框），否则按键会被
   别处消费，断言会莫名其妙地失败。右键用
   `cx.simulate_mouse_down(pos, MouseButton::Right, Modifiers::default())`。
6. **收尾**：`cargo fmt --all` + `cargo clippy -p menghunote-app --all-targets -- -D warnings`
   + `cargo test -p menghunote-app`。

## 当前覆盖（8 条：2 条静态 + 6 条交互）

| 测试 | 断言 |
| --- | --- |
| `app_shell_paints_headless` | 空临时库播种成功（`seeded == true`）；`add_window_view` 后恰好 1 个窗口；`app-shell-host` 的 `debug_bounds` 为 `Some` 且宽高 > 0（实测 1920×1080） |
| `dev_vault_tree_and_first_note_are_loaded` | `list_tree` 恰好 3 项且 rank 顺序/深度与播种常量一致（根级笔记 / 示例目录 / depth=1 的第二篇）；根级笔记的 `body_doc_id` 经 `load_body_doc` 重放后正文非空且包含演示标题（5602 scalars）；同一 `ShellHost` 再建窗口画一帧不 panic、bounds 非零 |
| `click_tree_row_switches_note` | 启动态 `tree-row-1-selected` 有值（**侧栏展示顺序**：0 目录 / 1 目录内笔记 / 2 根级笔记；启动打开排序后的第一**笔记**）、`tree-row-2-selected` 无值；`simulate_click` 点 `tree-row-2` 中心（根级演示笔记）后 `tree-row-2-selected` 出现、`tree-row-1-selected` 消失、`editor-current-doc` 仍在；存储侧 `load_body_doc` 确认该篇正文包含 `DEMO_TITLE` |
| `ctrl_k_opens_search_and_escape_closes` | 启动时 `search-overlay` 为 `None`；`simulate_keystrokes("ctrl-k")`（真实按键 → 编辑器 `on_key_down` → 宿主建浮层）后 `search-overlay` / `search-input` / `search-results` 都有非零 bounds；`simulate_keystrokes("escape")` 后 `search-overlay` 回到 `None`、编辑器仍在。**不断言搜索结果**（后台真实索引） |
| `right_click_folder_row_opens_menu_and_escape_closes` | 启动时 `context-menu` 为 `None`；`simulate_mouse_down(..., MouseButton::Right, ...)` 打 `tree-row-0`（目录行）后 `context-menu` 出现；`escape` 后回到 `None`（菜单面板自己 `track_focus` 接 `Esc`） |
| `folder_click_collapses_and_expands_children` | 启动态 `tree-row-1`（目录内笔记）有非零 bounds；点 `tree-row-0`（目录）中心后 `tree-row-1` 的 bounds **消失**、目录行仍在、`editor-current-doc` 不变（折叠不切文档；活动笔记在折叠目录里时它的行被隐藏是预期）；再点一次后 `tree-row-1` 回来且 bounds 非零。折叠态只存在内存（`AppShell::collapsed`），所以只能靠 bounds 有无来断言 |
| `context_menu_new_note_item_creates_note` | 右键 `tree-row-0`（目录）后三个菜单项都画出 `menu-item-0/1/2`；点 `menu-item-1`（新建笔记）中心 → 菜单关闭；树从 3 项变 4 项、新节点 `parent_id == 目录 node_id` 且 `depth == 1`；侧栏展示顺序下新笔记（`未命名笔记`）排在目录子项第一位 → `tree-row-1` 出现并带 `-selected`（新建后自动打开），旧的第一篇 `-selected` 消失 |
| `editor_keystroke_inserts_character_and_persists` | 点 `editor-pane` 中心聚焦 → `simulate_keystrokes("a")`（`KeyDown` 不被消费 → gpui 把 `key_char` 交给聚焦元素的 `EntityInputHandler::replace_text_in_range`）→ `AppShell::close_all` 做有界排空（durable flush）→ `load_body_doc` 重放：正文**恰好**多一个 `'a'`（公共前缀/后缀核对，证明是行内插入而非 append），`head_seq` 推进（durable 版本变化） |

## 新增覆盖（2026-11-18：ribbon / 侧栏排序 / 设置页导航）

| 测试文件 | 条数 | 断言 |
| --- | --- | --- |
| `headless_sidebar_sort.rs` | 8 | ribbon 在侧栏左侧（宽 `RIBBON_WIDTH`）、无设置按钮、旧 selector 是同一按钮的别名；ribbon 四键复用既有动作；默认目录优先 + A-Z；排序菜单 6 项 + 点空白关闭；六种键重排 + 写盘；重排保持折叠/打开笔记；偏好往返 + 损坏回退；启动读盘 + 损坏回退 |
| `onboarding_ui.rs`（`--features dev-sync`） | +1 | `settings_nav_switches_sections_and_keeps_old_selectors`：8 个固定分类 + `settings-nav-divider` + 2 个插件分类的顺序断言、默认「外观」、只渲染当前分类、旧 selector 在对应分类仍存在、同步分类直接渲染面板、内容区不超过面板高度、会话内记住上次分类 |

设置页固定 selector：`settings-nav` / `settings-nav-<section>` / `settings-section-<section>`
（固定 section = `appearance` / `language` / `sync` / `instances` / `relay` / `security` /
`plugins` / `about`；插件 section = `templates` / `daily_notes`，只在启用时存在）、
`settings-nav-divider`（固定项后、插件项前的 1px 分界线）、
`settings-plugin-<id>[-toggle|-state]`（插件总开关页）；`settings-save` / `settings-cancel` /
`settings-error` / `settings-notice` 固定在底栏（任何分类都可见）。详见
[`ui-refactor-settings-ribbon-sort.md`](ui-refactor-settings-ribbon-sort.md)。

## 后续计划

- 已覆盖（第二批）：点侧栏行切换笔记、`Ctrl+K` 打开搜索浮层、`Esc` 关闭浮层、右键目录行
  弹菜单 + `Esc` 关闭菜单。
- 已覆盖（第三批）：目录行折叠/展开、右键菜单项点击（新建笔记，落库 + UI 双重断言）、
  编辑器普通字母键入 → 正文落库（`head_seq` 推进）。
- **右键菜单项只测非破坏性动作**：「新建笔记」是目录菜单里唯一"非破坏且有可观察结果"的
  动作（重命名进输入框、只发 UI 焦点事件；新建文件夹会改选中态但不开文档）。笔记菜单的
  第二项是**删除**（两步确认、无撤销），自动化不执行，继续只在真机手工验。
- **仍缺**：右键菜单的"点外部关闭"不能在测试里稳定复现——遮罩的 `OPEN_GUARD`（100ms）
  用的是**真实时钟** `Instant::elapsed`，测试里两次事件相隔微秒，要么等（= sleep，违反约定）
  要么放行，两条路都不该为测试改生产语义；菜单关闭只测 `Esc` 路径。
- **仍缺**：搜索"输入 → 结果上屏 → Enter 打开"这条链依赖后台真实 tantivy 索引与
  `TICK_MS` 定时器；测试调度器不推进时钟，`run_until_parked` 等不到 tick。要么给
  `SearchService` 注入可控时钟（重构，另立任务），要么继续由 `search_service.rs` 的
  集成测试覆盖数据面。
- **仍缺**：行内重命名（`RenameInput` 的焦点/提交时序同样是 defer + 订阅）、删除的
  两步确认（涉及 `Instant` 与菜单几何 `menu_hit`；且删除不可撤销，不做自动化）。
- **注意环境变量**：`MENGHUNOTE_SEARCH_ON_START` 会让 AppShell 启动即开浮层，
  `ctrl_k_opens_search_and_escape_closes` 的"启动时无浮层"前置断言会失败；跑测试前确认它没被设置
  （同理 `MENGHUNOTE_SEARCH_QUERY` 只预填查询，不影响开关断言）。

## 仍然不能测（保持真机）

像素/颜色/抗锯齿/阴影、IME 候选窗与组合串、DPI 缩放（测试里恒为 1.0）、真实窗口层级与
焦点抢占——这些在 headless 测试里**永远**只能近似，继续用 Windows 真机 `ui_smoke.ps1`。
一句话：**交互绑定与状态迁移用本测试；"看起来对不对""输入法能不能用"仍然必须真机。**

补充（第三批核对过）：**普通字符键入可以测**（见 `editor_keystroke_inserts_character_and_persists`），
因为 gpui 的 `dispatch_keystroke` 在事件未被消费时会把 `key_char` 交给聚焦元素的
`EntityInputHandler`——这是平台键入与 IME **上屏**共用的入口。但它**不等于 IME 可用**：
组合串（`replace_and_mark_text_in_range` → preedit overlay）、候选窗位置、`WM_IME_*`
事件序列都不经过测试平台，历史上 `WM_CHAR` 注入"能打字"≠ 输入法可用的教训依然成立。
