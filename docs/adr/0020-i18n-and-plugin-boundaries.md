# ADR 0020 — i18n 与插件边界（方向提案）

> 状态：**提议**（2026-11-17，用户方向；未冻结实现）。
> P1 i18n 基础设施 + 首批 UI 文案迁移已实施（2026-11-18，见 §9）；
> P2 第一方模块注册表 + 插件 manifest/host 契约已实施（2026-11-18，见 §10）；
> **插件加载器/运行时/第三方 UI 仍未开始**，插件 API 未冻结。
> 关联：ADR 0003（模块边界）、ADR 0006（sync v1）、ADR 0009/0011（同步 UI/初始化）、
> ADR 0019（Markdown/Object/资产）。
>
> 本 ADR 记录“后期 i18n、插件系统、同步作为插件”的边界与推荐路线，避免把
> Obsidian 的术语直接套到 Rust/GPUI 上。**任何插件加载器、host API 或 i18n
> 基础设施的定稿，仍需另开 ADR 或更新本页状态。**

## 1. 背景与问题

用户提出后期需要：

- i18n 多语言；
- 插件系统；
- 同步等能力后期作为插件开发。

参考对象是 Obsidian。Obsidian 里大量功能以 **core plugin（核心插件）** 形式存在，
Sync、Publish、Graph、Canvas 等都属于此类；同时还有 **community plugin（社区插件）**。
如果直接把“插件”理解成“运行时可加载的第三方二进制”，会得到错误的架构结论。

## 2. 术语与边界

本 ADR 采用三层模型：

| 层 | 含义 | 例子 | 是否第三方可写 |
| --- | --- | --- | --- |
| **Host 核心** | 数据/协议/编辑内核，必须随主程序发布 | 编辑器内核、storage/schema、同步 wire/CRDT、密钥与鉴权 | 否 |
| **第一方核心模块** | 第一方维护、可编译期裁剪或设置开关的可选功能 | Sync UI/传输、Mermaid、导入导出、Graph 增强 | 否 |
| **第三方插件** | 后期生态扩展，通过 host API 运行 | 自定义命令、面板、导入器、图表语言 | 是（受权限约束） |

Obsidian 的 **core plugin 属于第二层，不是第三层**：它随安装包发布、第一方维护，
能访问内部 API；Sync 还是付费服务，与账号、加密、服务端强绑定。
Obsidian 的 community plugin 才是第三方 JS 包，运行在 Electron 渲染进程里，
通过 JS API 操作 vault/workspace；它权限很大，不是强沙箱。
Obsidian 的 Mermaid 是内置 Markdown 渲染能力（第一方），社区插件更多是 Mermaid 辅助工具。

因此本项目应把“核心插件”类比为 **Cargo feature / workspace crate / 设置开关**，
而不是 `dlopen` 的第三方动态库。

## 3. 决策方向

### 3.1 i18n 先于插件 API

- i18n 是 UI 的基础设施，插件 manifest、命令标题、设置项、面板文案都要用到它；
  应先把语言协商/message id/回退规则定下来，再冻结插件 API。
- 建议使用支持 plural/fallback 的消息方案（候选：`fluent` 系；最终选型另定），
  不允许在业务代码里散落 `if lang == "zh"`。
- UI 文案改为 **message id**；禁止字符串拼接，禁止把中文/英文直接写进逻辑。
- 插件文案使用命名空间：`plugin.<plugin_id>.*`；插件 manifest 声明 locales，
  host 统一解析当前语言、回退语言与缺 key 行为。
- **笔记内容、Markdown 源码、用户数据不翻译**；只翻译 host/插件 UI。
- 日期/数字/相对时间通过 locale 格式化；RTL、字体、CJK 换行后置。
- 迁移策略：先定规范并搭基础设施，再按页面增量提取（设置页、侧栏、编辑器菜单优先），
  不做一次性全仓替换。

### 3.2 第三方插件首版不加载原生动态库

不采用：

- `cdylib` 直接加载 Rust/GPUI 插件：Rust 无稳定 ABI，GPUI 类型/窗口/线程/GPU
  生命周期不可控，panic 会跨 FFI 边界，升级即全生态重编译；安全边界也不成立。
- Electron 式任意 JS 访问全部 host API：权限过大，难以审计。

首选后期方案（二选一，另开 ADR 决策）：

1. **WASM 沙箱插件**：稳定 ABI、可移植、可限制资源；插件只能通过 host 提供的
   capability API 访问文件/网络/设置/笔记；UI 用声明式描述，由 host 用 GPUI 渲染。
2. **独立进程 + JSON-RPC/stdio**：隔离性最好，适合同步、导入、索引等重服务；
   代价是进程管理、IPC、打包复杂度。

在插件运行时定稿前，只做 **第一方核心模块** 的接口抽象，不开放第三方加载。

### 3.3 Host API 最小面

后期插件 API 至少需要：

- manifest：`id`、`version`、`api_version`、`locales`、`capabilities`、
  `commands`、`panels`、`settings`；
- host services：命令注册、设置读写、通知、受控文件/网络访问、笔记读写
  （通过 host 的文档 API，不直接拿 SQLite/密钥）；
- UI：命令面板、声明式面板/设置页；**不把原始 GPUI Element 暴露给插件**；
- 生命周期与事件：加载/启用/禁用/卸载、配置变更、文档打开/保存；
- 权限：安装/首次使用时显式授权；签名/来源校验；敏感能力（网络、密钥、删除）
  单独授权并可审计。

### 3.4 同步的插件边界

- **同步协议、CRDT、schema、密钥、checkpoint 属于 Host 核心，不能第三方插件化。**
  否则数据完整性、加密边界与迁移都无法保证。
- 同步的 **UI、传输方式、可选服务** 可以先做成第一方核心模块：
  - 保留现有 `crates/sync` 协议实现；
  - 把 sync UI/worker 编排与具体传输抽到可裁剪的 app 模块/feature；
  - 传输接口（如 `SyncTransport`）只交换 host 定义的类型，不暴露密钥槽/outbox 内部。
- 第三方最多通过 host service 实现“可选传输/导入器”，不能接管同步语义。

### 3.5 Mermaid 的插件边界

- Mermaid 首版作为 **第一方核心模块**，不放到第三方插件市场：
  - 它深度接入 Markdown 解析、Object 投影、布局/绘制/命中、缓存、撤销与 IME；
  - 需要 host 统一做输入/输出上限、panic 隔离、版本 pin 与安全回退；
  - 二进制体积可用 Cargo feature 控制，不需要动态加载。
- 等 block renderer 插件 API 成熟后，再允许第三方注册其它图表语言；
  Mermaid 本身仍由第一方维护，源码 fenced block 始终是回退。

## 4. 候选方案与取舍

| 方案 | 结论 | 理由 |
| --- | --- | --- |
| 全功能单仓单二进制，无插件 | 短期可行，长期维护成本高 | 功能膨胀、无法裁剪、第三方无法扩展 |
| 第一方 feature 模块 + 后期 WASM/进程插件 | **推荐** | 先解决内部模块化，再开放安全扩展 |
| 原生 `cdylib` 插件 | 拒绝 | ABI/GPUI/panic/安全/升级不可控 |
| Electron 式任意 JS | 拒绝 | 权限模型不可接受，且本项目非 Web 前端 |
| 直接照搬 Obsidian core plugin 概念 | 不直接套用 | Obsidian core plugin 是第一方模块，不是第三方运行时插件 |

## 5. 实施路线（提案）

1. **P0（只写文档/规范）**
   - 本 ADR；
   - 字符串清单与 message id 命名规范；
   - 插件 manifest/capability/API 版本规则草案。
2. **P1（i18n 基础设施）**
   - 消息加载/回退/复数/日期格式化；
   - 中/英双语，先覆盖设置页、侧栏、编辑器菜单、错误提示；
   - 缺 key/未翻译检测进入测试。
3. **P2（第一方模块化）**
   - 把 sync UI、导入导出、可选传输抽成 feature/crate；
   - 定义 host service trait，但暂不开放第三方加载；
   - 验证裁剪构建（默认/无 sync/无 Mermaid）与测试矩阵。
4. **P3（插件原型）**
   - 选定 WASM 或独立进程；
   - 一个命令 + 一个面板 + 设置 + 权限的最小闭环；
   - 安全评审（资源限制、路径/网络权限、签名、崩溃隔离）。
5. **P4（第三方生态）**
   - 插件仓库/安装/更新/禁用；
   - i18n 文案与插件 UI 的兼容性测试；
   - 再评估是否开放 block renderer/同步传输给第三方。

## 6. 后果与风险

- **i18n 是全 UI 改造**：字符串提取会触及大量 `view`/`shell` 文件，必须分页面推进，
  避免一次性大 diff 与语义漂移。
- **插件 API 过早冻结风险**：在 i18n 与第一方模块边界稳定前开放 API，后续会破坏兼容；
  本 ADR 只记录方向，不承诺时间表。
- **同步抽取复杂**：现有 sync UI/worker 与 AppShell、设置页、自动同步调度耦合；
  先抽接口、再抽实现，协议层保持不动。
- **安全**：第三方插件必须默认最小权限；WASM/进程隔离、资源上限、签名与审计
  是上线前提，不能用“社区插件都这样”作为理由跳过。
- **测试矩阵**：语言 × 平台 × 功能裁剪（sync/Mermaid/插件）会放大；CI 至少覆盖
  默认构建、`dev-sync`、无可选模块构建与缺失翻译。
- **Obsidian 对照的边界**：其 core plugin 可访问内部 API，是因为第一方随包发布；
  本项目不能把同等权限给第三方。

## 7. 明确不做（本 ADR 阶段）

- 不实现插件加载器；
- 不引入 WASM 运行时/动态库依赖；
- 不冻结插件 API；
- 不把同步协议/schema/密钥迁出 Host；
- 不一次性替换现有 UI 文案。

## 8. 待决问题

1. i18n 消息格式最终选 `fluent` 还是更轻的 key-value（需评估 plural/编译期检查/包体）。
2. 第三方插件首选 WASM 还是独立进程（同步/索引类服务可能更适合进程）。
3. 插件 UI 的声明式 schema 粒度：只允许命令/设置/列表面板，还是允许自定义布局。
4. 第一方 sync UI 是否在正式构建中默认编入，还是作为可选 feature。
5. 插件签名/来源信任模型（官方仓库、手动安装、企业分发）的优先级。

---

## 9. 实施记录（P1，2026-11-18）

> 本节只记录 **P1 i18n 基础设施 + 首批 UI 文案迁移** 的落地事实。插件加载器、
> host API、同步插件化（P2–P4）仍未开始，§8 的其余待决问题继续开放。

### 9.1 消息格式选型（本切片）

**结论：先用轻量静态 key-value catalog，不引入 `fluent` 等重依赖。**

- 新增纯 crate `crates/i18n`（`menghunote-i18n`）：不依赖 GPUI/storage/sync，
  **零外部依赖**；消息是 `&'static [(&'static str, &'static str)]` 静态表。
- 理由：P1 只需要 `t(id)`、`{name}` 插值、单复数两条 key；静态表零启动成本，
  且能在编译期测试里做「中英 key 集合一致 / 占位符合法」的强校验。当前若引入
  fluent 生态，会在插件 API 冻结前把消息格式、运行时与工具链一起焊死。
- **§8 问题 1 仍未决**：fluent vs 轻量 key-value 的最终选型留到插件 API 冻结前
  评估；届时只替换 `menghunote-i18n` 内部实现，业务代码继续用 `t(...)`。
- 命名空间：`settings.*` / `sidebar.*` / `editor.menu.*` / `status.*`；
  `plugin.<plugin_id>.*` 已在规范里预留，但本切片没有插件运行时，不实现装载。
- 消息 id 常量集中在 `menghunote_i18n::msg`（宏同时生成 `ALL`），catalog 测试断言
  **catalog key 集合 == msg 常量集合 == 中英两边集合**，杜绝加了一边漏另一边。

### 9.2 API 与回退/复数规则

| API | 行为 |
| --- | --- |
| `Locale { ZhCn, En }` | `parse` 接受 `zh`/`zh-CN`/`zh_CN`/`zh-Hans`/`en-US` 等；`detect()` 读 `LC_ALL` → `LANG`，未知/`C`/缺失回退 `ZhCn`（Windows 无 `LANG` 时等价默认 `zh-CN`） |
| `LocaleMode { ZhCn, En, System }` | 持久化选择；`as_str` = `"system"`/`"zh-CN"`/`"en"`，`resolve(system)` 得输出语言 |
| `t(id) -> String` | 查找顺序：**当前语言 → 英文 → 中文 → key 本身**；缺 key 不 panic、不打印 |
| `t_with(id, &[(name, value)])` | 在 `t` 结果上做 `{name}` 逐字替换；未知占位符原样保留；缺 key 时原样返回 id（不拿 key 当模板） |
| `t_count(one_id, other_id, count)` | `count == 1` 用 `one`，其它（含 0）用 `other`；只支持英文这一条规则，中文两条 key 同文案 |
| `current_locale()` / `mode()` / `set_mode()` / `set_locale()` | 进程级 `RwLock` 状态；`set_mode(System)` 调用时按环境解析 |
| `lock_for_test()` | 串行化测试并自动复位（`System` + 中文），只给测试用 |

**禁止字符串拼接语言逻辑**：业务代码只允许 `t`/`t_with`/`t_count`；组合文案用
带占位符的完整模板（如 `settings.instance_row` 的 `{role}`），不把中英片段先拼再判断。

### 9.3 语言偏好与本机持久化

- 文件：平台配置目录下 `language.json`（与 `theme.json` 同一目录逻辑）；
  `MENGHUNOTE_LANGUAGE_PATH` 可覆盖（测试/便携）。
- 内容**只存 locale 选择**（`"system"` / `"zh-CN"` / `"en"`），不存解析结果：
  ```json
  { "version": 1, "locale": "system" }
  ```
- 原子写：同目录临时文件 → rename；Windows 目标存在时退化为直接覆盖。
- 缺失/损坏/未知版本/未知值 → 回退 `System`（按 `LC_ALL`/`LANG` 解析，未知语言
  回退中文），**不阻塞启动**。
- 启动：`main.rs` 在主题之后调用 `language_settings::init_from_disk()`。
- 边界：**不同步、不写正文、不写进 vault/schema/wire**；语言与主题互不依赖。
- 设置页：`AppRoot` 与 `AppShell`（dev-sync）的设置页都挂
  `appearance_and_language_row()`，外观与语言并排；固定 selector：
  `settings-language`、`settings-language-zh-CN`、`settings-language-en`、
  `settings-language-system`、`settings-language-current`。
- 切换只更新全局语言 + `refresh_windows()`：不重建编辑器、不丢选区/滚动/撤销栈、
  不产生 `EditRequest`。

### 9.4 覆盖范围（首批迁移）

只迁移以下四处**固定 UI 文案**，全部走 `crates/app/src/ui_text.rs`
（渲染与测试共用同一入口）：

1. **设置页固定文案**：
   - 外观区：标题/提示、浅色/深色/跟随系统、当前模式行；
   - 语言区：标题/提示、中文/English/跟随系统、当前语言行；
   - 同步区：`设置 · 同步`、`同步设置（DEV ONLY）`、DEV ONLY 提示条、同步面板
     展开/收起标题与按钮；
   - 设置页按钮：展开/收起/关闭/复制/设为默认/当前默认、保存模式/relay、
     关闭（不保存）；
   - 错误/提示：`无错误` 占位、默认提示、无 active 实例提示、无实例提示、
     保存时无 active 实例、未勾选 remote_tls 确认、实例未打开时的指纹占位；
   - 实例/relay/公开信息/安全指纹区标题、实例行角色、`vault_id`/`device_id`/
     库文件路径标签（**路径值本身不翻译**）。
2. **侧栏固定文案**：`笔记`、`＋ 笔记`、`＋ 文件夹`、`导出`/`导出中…`、`图谱`、
   `⚙ 设置`；反向链接面板标题（`反向链接 N` / 英文 `1 backlink`/`N backlinks`）
   与空态 `没有反向链接`。
3. **编辑器右键菜单**：`插入`、`表格`、`打开链接`、`复制链接`、`复制 Markdown`。
4. **状态栏固定提示**：`反向链接 N`、`同步：需处理`、`同步：{status}` 前缀。

**不翻译**：笔记标题/正文/Markdown、用户输入、路径、同步状态行正文、
worker/配置/解析器返回的错误原文、公开 id、fingerprint 值、URL。

侧栏树本身没有独立的空态文案（空树渲染空列表，空态可操作项就是「＋ 笔记」），
因此“空态”覆盖的是反向链接面板的空态行。

### 9.5 未迁移清单（明确后置）

- **同步面板内部**（`dev_sync/panel.rs`：设备/邀请/轮换/基线/最近错误等全部文案）；
- **onboarding**：Welcome/Create/Join/InvitePending/Working/Backup/Error 页面、
  relay 模式选择器（Create/Join/设置共用）、表单校验错误；
- **图谱覆盖层**：标题、统计 hint、过滤器（文件夹/标签/无标签/未解析）、
  搜索框、空态、复位/关闭按钮；
- **编辑器**：外部链接确认框（`取消`/`打开`/`打开外部链接？`/记住 scheme）、
  复制链接/复制 Markdown 的 notice、表格工具条（加行/加列/编辑源码等）、
  图片/Mermaid 工具条、搜索浮层；
- **侧栏其它**：重命名输入框错误、导入错误/提示、右键菜单项
  （重命名/新建笔记/新建文件夹/删除）、导出过程的提示；
- **设置页剩余**：V2 管理区标题（实验、默认隐藏）、fingerprint 明细行标签、
  安全暂停类别、API 版本说明行、保存成功 notice、配置保存失败/找不到实例等
  带动态值的错误提示（这些属“错误原文”边界）；
- **编辑器空态**：`没有打开的笔记（从左侧选一篇）`；
- 日期/数字/相对时间格式化、RTL、字体/CJK 换行、复数 `zero/few/many`、
  插件运行时命名空间装载。

### 9.6 测试与验证

- `cargo test -p menghunote-i18n`：**16 passed**（语言解析/系统探测、模式
  往返、catalog 中英 key 集合一致、占位符合法、回退链、插值、复数、全局状态）。
- `cargo test -p menghunote-app`（默认）：**374 passed**；
  `--features dev-sync`：**620 passed**（新增 `headless_language` 3 +
  `headless_language_ui` 1；app lib 单测新增 `language_settings` 4 +
  `ui_text` 4）。
- `cargo fmt --all -- --check`、`cargo clippy -p menghunote-i18n -p menghunote-app
  --all-targets --features dev-sync -- -D warnings`：干净。
- headless 证明：设置页点语言按钮 → 全局语言/偏好文件立即变化；编辑器实体、
  正文、选区、滚动、撤销栈、本地编辑代数不变（不产生 `EditRequest`）；
  重启恢复/损坏回退；英文下右键菜单真实渲染。

### 9.7 已知限制

- **不订阅 OS 语言变化**（GPUI 无跨平台语言变化回调）：改完系统语言需重启或
  重新点「跟随系统」。
- 复数只支持英文 `one/other`；中文同形。消息格式最终选型（§8 问题 1）未定。
- 静态 catalog 每次 `t` 返回 `String`（多数固定文案每帧一次小分配）；对话框/
  长列表全量迁移前若出现性能问题，再评估 `Cow<'static, str>`/预编译表。
- 未迁移文案在英文界面仍显示中文（见 §9.5），这是分页面推进的预期中间态。


---

## 10. 实施记录（P2，2026-11-18）

> 本节记录 **第一方模块化 + 插件 manifest/host 契约** 的最小切片。
> **不加载第三方运行时**：没有安装/加载/卸载、命令执行、权限授权 UI、
> WASM/独立进程选择；§7/§8 的其余开放项仍然有效。

### 10.1 切片范围与依赖边界

- 新增纯 crate `crates/plugin-api`（Cargo 包名 `menghunote-plugin-api`）：
  运行时只依赖 `serde` 与纯 crate `menghunote-i18n`；`serde_json` 只在
  `[dev-dependencies]`（JSON 契约测试），不在运行时依赖里。
- 不依赖 GPUI/storage/sync/relay/editor；不读磁盘、不读环境变量、不做 IO。
- `crates/app/src/modules.rs` 新增只读第一方模块注册表。`app` 本切片**没有**
  依赖 `plugin-api`：注册表只投影 Cargo feature 事实，不消费 manifest。
  依赖方向仍然只允许 `app → plugin-api`，反向禁止。
- 未改 sync/relay/storage/schema/wire/主题值；未新增 Cargo feature 裁剪。
  默认构建与 `dev-sync` 构建都必须全绿（见 §10.7）。

### 10.2 id 与版本方案

| 类型 | 规则 | 失败表现 |
| --- | --- | --- |
| `PluginId` | ASCII 小写字母/数字/`.`/`-`；1–64 字节；非空；首尾不能是分隔符；本切片**不**额外禁止连续分隔符 | `PluginIdError`（空/超长/非法字符/首分隔/尾分隔）；`validate` 汇总为 `InvalidPluginId` |
| `PluginVersion` | `major.minor.patch` 三段 `u32`；只允许 ASCII 数字；无前导零；不支持预发布/构建元数据 | `PluginVersionError`；JSON 反序列化直接失败 |
| `ApiVersion` | 单个 `u16`；host 支持集合**精确**为 `[1]` | `api_version=2` → `UnsupportedApiVersion` |

选型理由：插件版本是给人看的语义化版本，三段足够表达破坏性变更/特性/修复，
且不必引入 `semver` crate；host API 用**代**（generation）而不是区间，先在
插件 API 冻结前把“当前只支持 1”写成可测试事实。JSON 里 `version` 是字符串
`"1.2.3"`，`api_version` 是整数 `1`。

### 10.3 Manifest 字段与校验

`PluginManifest` 是纯数据；完整 JSON 形状见
`crates/plugin-api/src/lib.rs` 的 crate 文档。字段表：

| 字段 | JSON 类型 | 必需 | 默认 | 校验/说明 |
| --- | --- | --- | --- | --- |
| `id` | string | 是 | — | `PluginId` 规则 |
| `name` | string | 是 | — | trim 后非空 |
| `version` | string | 是 | — | `major.minor.patch` |
| `api_version` | integer | 是 | — | 必须在 `SUPPORTED_API_VERSIONS`（当前 `[1]`） |
| `locales` | string[] | 否 | `[]` | 复用 `menghunote-i18n::Locale`；解析后不得重复；未知语言拒绝 |
| `capabilities` | string[] | 否 | `[]` | 见 §10.4；未知能力 serde 拒绝、重复拒绝 |
| `commands` | `CommandSpec[]` | 否 | `[]` | id/title 非空；id 同集合唯一 |
| `panels` | `PanelSpec[]` | 否 | `[]` | id/title/slot 非空；id 同集合唯一 |
| `settings` | `SettingSpec[]` | 否 | `[]` | id 非空且唯一；default 类型必须匹配 type |

spec 字段保持最小，**不冻结** UI schema：

- `CommandSpec { id, title, description? }`：参数、快捷键、命令执行方式后置。
- `PanelSpec { id, title, slot }`：`slot` 是 host 定义的不透明字符串，不是
  布局 DSL；host 不认识时由未来加载器拒绝/忽略。
- `SettingSpec { id, type, default, scope }`：
  - `type` 为 `bool` / `integer` / `text`（JSON 字段名是 `type`）；
  - `default` 用原生 JSON 值（`SettingValue` 为 untagged bool/int/text）；
  - `scope` 为 `user` / `vault`；host 决定持久化位置，本切片不实现存储，
    也不承诺同步语义。

未知 **manifest 字段**被容忍（向前兼容），但未知 capability/type/scope 会被
serde 拒绝。`locales` 与 `capabilities` 重复、spec id 重复、空必需字段等由
`PluginManifest::validate` 收集；它返回全部错误，不短路、不 panic。

**拒绝加载规则（未来）**：反序列化失败或 `validate` 返回任何错误时，加载器
必须拒绝加载整个插件；不得部分加载、不得忽略未知能力/未知类型、不得降级
为“无能力运行”。本切片**没有加载器**，因此该规则目前只是契约与测试事实。

### 10.4 能力表

| capability | JSON 拼写 | 含义 | 本切片 host 接口 |
| --- | --- | --- | --- |
| `NotesRead` | `notes_read` | 列表/读取笔记快照 | `NotesReadService` |
| `NotesWrite` | `notes_write` | 创建/更新笔记正文 | `NotesWriteService` |
| `Network` | `network` | 网络请求（预留） | **无**（未开放） |
| `Filesystem` | `filesystem` | 用户选择的文件读写（预留） | **无**（未开放） |
| `Clipboard` | `clipboard` | 剪贴板读写（预留） | **无**（未开放） |
| `Notifications` | `notifications` | 用户通知 | `NotificationService` |
| `Settings` | `settings` | 插件自己的设置读写 | `SettingsService` |
| `Commands` | `commands` | 注册命令（预留） | **无**；只有 manifest 声明 |

能力是**声明**，不是授权结果。安装/首次使用授权、敏感能力单独授权、签名/
来源校验与审计全部后置；本切片没有授权 UI。`Network`/`Filesystem`/
`Clipboard`/`Commands` 只保留名字与语义，未定义接口、未实现任何能力。

### 10.5 Host 服务契约

`crates/plugin-api/src/host.rs` 只定义 host 自有类型与 object-safe trait：

| 类型 | 用途 |
| --- | --- |
| `NoteId` | 不透明笔记标识，插件只能原样传回 |
| `NoteSummary` / `NoteSnapshot` | 列表项 / id+标题+Markdown 正文快照 |
| `Notification` / `NotificationLevel` | 通知内容与级别 |
| `HostError` | 不透明可读错误（不暴露 SQLite/网络/密钥原文） |

trait：`NotesReadService`（list/read）、`NotesWriteService`（create/update）、
`SettingsService`（get/set）、`NotificationService`（notify）。`HostServices`
是 host 侧“完整实现”的**标记 trait**，仅用于静态约束；未来加载器应按
manifest capability 构造只实现相应子 trait 的 host 对象，**不要**把
`&dyn HostServices` 交给插件。

边界：不暴露 SQLite/密钥/GPUI 类型；不把 storage/sync 类型放进签名；不实现
真实 Vault/编辑器接线；不选择 WASM 还是独立进程（§8 问题 2 仍开放）。本切片
没有任何运行时路径导入这些 trait。

### 10.6 第一方模块清单

`crates/app/src/modules.rs` 的只读注册表固定顺序如下（顺序是契约，不是
字母序）：

| id | 模块 | `compiled` | feature gate | `enabled` |
| --- | --- | --- | --- | --- |
| `sync` | Sync | 仅 `--features dev-sync` | `dev-sync` | 同 `compiled` |
| `mermaid` | Mermaid | **总是**编入 | 无 | `true` |
| `import-export` | Import / Export | **总是**编入 | 无 | `true` |
| `graph` | Graph | **总是**编入 | 无 | `true` |

事实说明：

- Sync 的用户可见模块（UI/worker）只在 `dev-sync` 下编译；单独的 `sync`
  feature 只提供构建期 relay 能力闸（`sync_gate`），不构成可用同步模块，
  因此注册表把 `dev-sync` 作为 Sync 的唯一门禁。
- Mermaid / ImportExport / Graph 当前**没有**可关闭的 Cargo feature；注册表
  如实写 `compiled=true`，不伪造“可裁剪”状态，也没有新增裁剪 feature。
- `enabled` 在本切片恒等于 `compiled`：没有运行时开关、没有设置项、没有
  持久化配置。将来若做开关，必须在设置层实现，不能直接改只读描述符。
- **没有 UI**：设置页未新增“模块（实验）”列表，现有布局未改；本切片也没有
  任何 UI/平台行为需要真机验证。文案字段只是注册表元数据，不承诺最终 UI
  文案（设置页展示应走 i18n message id）。

### 10.7 验证矩阵（2026-11-18）

```bash
cargo fmt --all -- --check
cargo clippy -p menghunote-plugin-api -p menghunote-app --all-targets \
  --features dev-sync -- -D warnings
cargo test -p menghunote-plugin-api
cargo test -p menghunote-app
cargo test -p menghunote-app --features dev-sync
```

结果：

| 命令 | 结果 |
| --- | --- |
| `cargo test -p menghunote-plugin-api` | **24 passed**（17 单测 + 7 JSON 集成；另有 0 doc-test） |
| `cargo test -p menghunote-app`（默认） | **384 passed**（P2 前 380；新增 4 个 `modules` 单测） |
| `cargo test -p menghunote-app --features dev-sync` | **630 passed**（P2 前 626；新增 4 个 `modules` 单测） |
| `cargo fmt --all -- --check` | 干净 |
| `cargo clippy ... -- -D warnings` | 干净 |

`modules` 的四个测试分别锁定：固定顺序 + id 唯一 + 元数据非空；Sync 在默认
构建 `compiled=false` / `dev-sync` 构建 `true`；Mermaid/ImportExport/Graph
总是编入且无 feature gate；按 id 查询与注册表一致。plugin-api 的 JSON 测试
覆盖完整 manifest、可选字段默认、缺必需字段、未知 capability/type/scope、
重复 locale/capability、`api_version=2` 与序列化往返。

本切片不读写临时文件，未使用 `sleep` 或 `CARGO_TARGET_TMPDIR`。

### 10.8 明确未做

- **加载器与生命周期**：安装/加载/卸载/启用/禁用、配置变更、崩溃隔离。
- **运行时选择与实现**：WASM/独立进程、IPC、资源上限、宿主调度。
- **第三方 UI 与命令执行**：命令注册 API、声明式面板/设置页渲染、原始
  GPUI Element 暴露（仍拒绝）。
- **权限与信任**：授权 UI、安装确认、敏感能力单独授权、审计、签名/来源
  校验、插件仓库/更新。
- **插件 i18n 装载**：`plugin.<plugin_id>.*` 命名空间仍只在 i18n P1 文档里
  预留，没有 catalog 合并或运行时装载。
- **设置页“模块（实验）”只读列表**：未做；现有布局零改动。
- **Cargo feature 裁剪**：未给 Mermaid/ImportExport/Graph 新增可关闭 feature。
- **同步迁移**：同步协议/schema/密钥/checkpoint 仍在 Host，不因 P2 改变。
- **Host 服务实现**：未接真实 Vault/编辑器/通知；只有 trait 与纯类型。

### 10.9 已知限制与后续

- manifest/API schema **未冻结**；版本拼写、能力集合与 spec 字段在 P3 前
  仍可能调整，届时必须同步更新本页与测试。
- `PluginVersion` 当前不接受预发布/构建元数据；若插件生态需要，先扩展解析
  与测试，再更新校验。
- `locales` 与 host 支持集合绑定（当前只有 `zh-CN`/`en`）；新增语言应先更新
  `menghunote-i18n`，plugin-api 自动跟随。
- `HostServices` 只覆盖 notes/settings/notifications 四类能力；其它能力没有
  接口。任何人都不能据此宣称“网络/文件/剪贴板插件”已可用。
- `enabled` 是占位事实（等于 `compiled`）；运行时开关、持久化与设置页 UI
  后置。
- §8 问题 1（i18n 格式）、2（WASM/进程）、3（UI schema 粒度）、4（Sync 是否
  默认编入）、5（签名/信任）**均未因 P2 关闭**。P2 只固定了当前可验证的最小
  契约，不承诺插件生态的兼容性。
