# 第一方核心插件：模板（Templates）+ 日记（Daily notes）

> 2026-11-18。范围：`crates/app` 的**第一方 UI 逻辑** + 本机设置 JSON + 文档/测试。
> **不改 sync/relay/storage/schema/wire，不改同步行为；不新增重依赖**（`chrono` 早已在
> lockfile，0.4.45）；不 commit/打包。

## 1. 产品决定（已确认，勿改回去）

| 决定 | 含义 |
| --- | --- |
| **模板就是普通笔记** | 不建独立表、不隐藏文件夹、不做保留名、不做搜索/图谱过滤。模板文件夹只是**可配置的普通目录**（默认 `模板`），其子树下任意笔记都是模板。 |
| 日记模板可以是任意普通笔记 | 不要求和模板文件夹一致；按**精确路径**解析（如 `模板/日记模板`），空 = 无模板。 |
| 设置只在本机 | `templates.json` / `daily-notes.json`（平台配置目录），**不同步、不写正文**。换设备需各自配置。 |
| 不复制资产字节 | 模板正文按**源文**（`BodyDoc::text()`）读取；其中的 `asset://` / `[[wiki]]` 链接保持普通语义（不做导入/重写/复制）。 |
| 插入只走现有编辑通道 | 一次 `EditRequest` / 一个撤销组：`EditorView::insert_template_text` → `insert_text` → `compose_replace_selection`（有选区则替换）。 |

## 2. 设置文件

平台配置目录与 `language.json` / `theme.json` 一致（`MENGHUNOTE_*_PATH` 环境变量可覆盖）：

| 文件 | 环境变量（测试/便携） | 字段 |
| --- | --- | --- |
| `templates.json` | `MENGHUNOTE_TEMPLATES_PATH` | `version` / `enabled` / `folder` / `date_format` / `time_format` |
| `daily-notes.json` | `MENGHUNOTE_DAILY_NOTES_PATH` | `version` / `enabled` / `date_format` / `folder` / `template` |

- 默认值：`enabled = true`、`folder = 模板`（模板）/ `日记`（日记）、
  `date_format = YYYY-MM-DD`、`time_format = HH:mm`、`template = ""`（无模板）。
- 原子写（同目录临时文件 → rename，失败退化为覆盖写）；**损坏/未知版本/未知值回退默认**
  （逐字段容错，不阻塞启动）。
- 手写 JSON 解析（不把 `serde_json` 拉进默认构建）；只接受本模块写出的扁平对象。
- 运行时状态懒加载：第一次需要偏好时从磁盘读一次（`main.rs` 无需新增启动步骤）。

## 3. 占位符（纯函数）

| 写法 | 含义 |
| --- | --- |
| `{{date}}` | 用设置里的 `date_format` 格式化**本地**日期 |
| `{{time}}` | 用设置里的 `time_format` 格式化**本地**时间 |
| `{{title}}` | 目标笔记标题：插入到当前笔记 = 当前笔记标题；从模板新建/今日日记 = 新笔记标题 |
| `{{date:FORMAT}}` / `{{time:FORMAT}}` | 单次覆盖格式；**空格式回退默认** |

- 支持 token：`YYYY` `YY` `MM` `M` `DD` `D` `HH` `H` `mm` `m` `ss` `s`
  （**最长匹配**：`YYYY` 先于 `YY`；其余字符原样保留）。
- 未知占位符（`{{foo}}`、`{{ Date }}`）与未闭合的 `{{` **原样保留**，不 panic、不丢弃。
- **日记模板的占位符**复用同一引擎：默认日期/时间格式取**模板插件**设置里的
  `date_format` / `time_format`（只读；日记插件没有第二套默认格式）。
- 明确不做：日期偏移（`{{date+1d}}`）、脚本、正则、命令面板。

## 4. 模板发现

- 读 `list_tree()` 快照 → 逐个节点向上拼路径（`parent_id` 递归；脏数据最多 `树长度` 步防环）。
- 模板 = 路径以 `folder + "/"` 开头的**笔记**（目录本身不算模板）；`folder` 先规整
  （去首尾空白/斜杠、折叠空段）；空设置 = 未配置（不做发现，不匹配全库）。
- 排序：按相对路径的字典序（Unicode 码位，不是拼音）；**相同标题靠相对路径区分**。
- 空态分四种（选择器里明确展示，**不自动创建文件夹**）：
  `未配置模板文件夹` / `文件夹不存在` / `文件夹为空` / `没有匹配的模板`。
- 大小写敏感、精确前缀匹配（与笔记树一致）。

## 5. 插入模板

入口：编辑器右键菜单「插入 → 模板…」（模板插件启用时才出现；菜单面板高度与命中矩形同步变高）。

选择器（覆盖层实体 `TemplatePicker`，宿主渲染）：

- 自绘 IME 搜索框（复用 `sidebar_rename::RenameInput`；`SearchInput` 的占位符是
  "搜索笔记…"，本切片不改 `search_input.rs`，所以搜索框上方用 `templates.picker_search` 标签）；
- 输入即过滤（标题 + 相对路径，大小写不敏感）；`↑/↓` 移动、`Enter` 确认、点击行、
  `Esc` 关闭；IME 组合态的方向键交给输入法，不移动列表选中；
- 行上显示：标题（主）+ 相对路径（次）。

插入语义：

- 打开菜单/选择器**不移动 caret/选区**；右键只决定菜单落点；
- 选中后把**替换过占位符**的模板正文插入当前 caret，**有选区则替换选区**；
- 一次 `EditRequest`、一个撤销组（`Ctrl+Z` 一次完全恢复）；
- 模板笔记本身不移动、不改写；同步持闸 / IME 组合 / 表格单元格就地编辑中拒绝插入
  （侧栏提示 `templates.insert_failed`）。

## 6. 从模板新建

入口：侧栏动作行第二行「从模板新建…」（模板插件启用时才渲染）。

- 新笔记标题默认模板标题；**只在根级去重**（`X`、`X 2`……）。模板笔记自己通常就叫
  这个名字（在模板文件夹里），全局去重会让每次新建都多出「 2」。
- storage 允许同名笔记：本操作**永远新建节点**，不会覆盖任何已有笔记。
- 创建后立即打开，并插入渲染后的正文（`{{title}}` = 最终标题，含序号）。
- 结构写入一条 `StructureWriter` 提交；失败给明确提示（`templates.write_failed`）。

## 7. 今日日记

入口：侧栏动作行第二行「今日日记」（日记插件启用时才渲染）。

1. 计算路径 `folder + "/" + format_datetime(date_format)`；日期格式里的 `/` **产生子目录**
   （`YYYY/MM/DD` → `日记/2026/11/18`，笔记标题 = 最后一段 `18`）。
2. 该路径已有笔记 → **直接打开**（不覆盖、不重复创建）。
3. 否则自动创建缺少的目录 + 笔记（**一条结构提交**；已有目录按路径复用，不重命名）。
4. 有模板 → 读源文 → 替换占位符 → 作为正文；**模板丢失是非阻塞提示**，仍然创建空日记
   （`daily_notes.template_missing`，路径可在设置页修正）。
5. 路径无效（格式只产出分隔符/层数病态）→ 明确错误，不建任何东西。
6. 创建后展开祖先目录并打开；正文写入失败给提示（不静默）。

## 8. 设置页

- AppRoot 设置页在卡片**末尾**追加「模板」「日记」两区（不挪动既有同步/实例/relay 控件的
  位置与点击几何；卡片本身可滚动）。
- AppShell 的（dev-sync）设置页同样追加两区。
- 字段是自绘 IME 输入框（复用 `RenameInput`）：**Enter 保存、Esc 放弃输入**
  （不做失焦自动保存）；空/无效值拒绝并提示，不写盘。
- 显示当前日期/时间示例（真实格式化结果）。

固定 selector：

| selector | 含义 |
| --- | --- |
| `settings-templates` / `settings-daily-notes` | 两区根 |
| `settings-templates-toggle` / `settings-templates-state` | 启用开关 / 状态文本 |
| `settings-templates-folder` / `-date-format` / `-time-format` | 三个输入框 |
| `settings-templates-example` / `-notice` | 示例（`日期 时间`） / 保存提示 |
| `settings-daily-notes-toggle` / `-state` | 启用开关 / 状态文本 |
| `settings-daily-notes-folder` / `-date-format` / `-template` | 三个输入框 |
| `settings-daily-notes-template-hint` / `-example` / `-notice` | 模板提示 / 今天路径示例 / 保存提示 |
| `sidebar-new-from-template` / `sidebar-daily-note` | 侧栏两个入口 |
| `editor-context-insert-template` | 右键菜单「模板…」 |
| `template-picker` / `-panel` / `-search` / `-list` / `-empty` / `item-<i>` | 选择器覆盖层 |

## 9. i18n

新增 `templates.*` / `daily_notes.*` 消息 id（中英双语），全部经 `t()` / `t_with()`；
`crates/app/src/ui_text.rs` 是渲染与测试共用的唯一取文案入口。不翻译：笔记标题/路径、
错误原文（`{reason}` 原样嵌入）、模板正文。

## 10. 测试

纯逻辑单测（`cargo test -p menghunote-app --lib`）：

- token 格式化（`YYYY/YY/MM/M/DD/D/HH/H/mm/m/ss/s`、最长匹配、未知字符保留）；
- `{{date:...}}` / `{{time:...}}` 覆盖、空格式回退、未知/未闭合占位符原样保留；
- 模板递归发现 + 相对路径/排序/空文件夹/未配置；`node_paths` 与按路径查笔记/目录；
- 日记路径（`/` 子目录、title = 最后一段、幂等）、无效格式、空仓库回退默认、模板解析；
- 两个设置 JSON 的往返/损坏回退/原子写（无残留 `.tmp`）/`init_from_path` 重启恢复。

headless 集成（真实 `AppShell` + 临时 vault，`CARGO_TARGET_TMPDIR`、不 sleep）：

- `tests/headless_templates.rs`：右键 → 选择器 → 插入（一次本地编辑、一次撤销）；
  有选区时替换选区；从模板新建（默认标题、根级去重、不覆盖、打开即渲染）；
  空态不建文件夹；今日日记创建/重复打开/模板丢失/`/` 子目录；插件关闭隐藏入口；
- `tests/headless_notes_plugins_ui.rs`（`sync-ui` 构建）：AppRoot 设置页两区 selector
  齐全、开关立即写盘、字段 Enter 提交写盘（原子写）、空字段拒绝、示例随格式变化；
  两区追加在卡片末尾，测试用真实滚轮事件滚到可见再点击（与用户路径一致）。

真机（Windows）仍需验：像素/DPI、IME 候选窗位置、滚动手感——headless 证明不了。

## 11. 已知限制 / 明确后置

- 模板路径大小写敏感、精确前缀匹配；标题里含 `/` 的笔记路径有歧义（普通笔记允许这种
  标题，本插件不特殊处理）。
- 日记模板路径是**手填**输入框（没有接模板选择器）；相对日期（昨天/明天）、日历视图、
  周期笔记（每周/每月）未做。
- 设置字段 Enter 保存、Esc 放弃；不做失焦自动保存与撤销。
- 命令面板、模板变量日期偏移、脚本、正则均**未做**（可后续）。
- 模板/日记设置**不参与同步**：多设备要各自配置（产品决定，避免把本机偏好写进 CRDT）。
