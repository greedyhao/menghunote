# AGENTS.md — menghunote

本文件是 AI agent 在本仓库的行为契约。**优先级高于任何单次对话中的临时指令**。
若某次任务的要求与本文件冲突，以本文件为准，并在回复中说明冲突点。

---

## 1. 项目定位

**menghunote** —— 基于 [GPUI](https://gpui.rs/) 的笔记软件，**桌面端优先**。

| 项 | 决策 |
| --- | --- |
| UI 框架 | **`gpui-kit` 0.10.0**（门面：`gpui-pre` 0.3.12 + `gpui-base` + `gpui-component`；2026-11-10 起，见 ADR 0004） |
| 语言 | Rust（edition 2021+） |
| **编辑模型** | **Typora 式原位所见即所得**：编辑器内文本即 Markdown 源码（`**` 等标记真实存在，默认不绘制、光标进入时显现）；持久化真源是 SQLite 单库（见 ADR 0001）。详见 [`docs/briefs/0002-editor-core.md`](docs/briefs/0002-editor-core.md) |
| 首要平台 | **Windows 桌面**（开发机即 Windows） |
| 后续平台 | macOS / Linux（架构上预留，但**不为未落地平台提前抽象**） |
| 存储 | **SQLite 单库（WAL）为持久化真源**；Markdown 仅单向导出（不做外部编辑、不监听文件）。详见 [`docs/adr/0001-storage-format.md`](docs/adr/0001-storage-format.md) |
| 同步 | **设计必须支持**多设备同步 + 用户自建中心服务器；实现可后置，但数据模型现在就要留口 |

> **Windows 现实约束**：GPUI 的 Windows 后端相对较新，跨平台细节（图形后端、崩溃上报、输入法）仍在磨合。
> 因此：**每完成一个可见特性就立刻在 Windows 上跑一遍**，不要攒到最后；
> 平台相关代码必须收敛在 `cfg(target_os = ...)` 或独立模块内，禁止散落在业务逻辑里。

> **依赖基线（ADR 0004，2026-11-10 切换到 GPUI Kit）**：
> ```toml
> [dependencies]
> # 别名把 lib 名固定成 `gpui`：代码里 `use gpui::*` 与 `gpui::platform::application()` 都靠它
> gpui = { package = "gpui-kit", version = "=0.10.0" }   # 精确 pin
> ```
> - `gpui-kit` 是 **longbridge 的 meta crate**（不是 Zed 官方再发布）：它依赖 `gpui-base 0.10.0` +
>   `gpui-pre 0.3.12` + `gpui-component 0.10.0`，其中 **`gpui-pre` 才是 Zed 主干的再发布**
>   （`repository = zed-industries/zed`）；
> - 选它的理由是**未来能直接用 `gpui-component` 的组件与测试基座**，不是"比 zed 新鲜"；
> - **升级机制（重要）**：`gpui-kit` 自己**精确 pin** 了 `gpui-pre =0.3.12`，但 `gpui-base` / `gpui-component` /
>   `gpui-kit-assets` 在 kit 的清单里是 **caret（`^0.10.0`）**——也就是说 `cargo update -p gpui-base` 能在 kit 不动的情况下
>   把 base 挪到 kit 未验证过的组合。真正焊死整套的是**本项目 Cargo.toml 的 `"=0.10.0"` + 提交进仓库的 `Cargo.lock`**。
>   升级做法：**只改 `gpui-kit` 版本** → 让 cargo 重新解算整套 → 跑三绿 + UI smoke + 真机验 → 提交（含 lockfile）；
>   **禁止**单独 `cargo update -p gpui-base` 或手工指定 base/component 版本。
> - 平台入口从 `gpui_platform::application()` 变成 **`gpui::platform::application()`**
>   （迁移分支 `feat/gpui-kit`；探针结论：0 签名变化、0 trait 不匹配、仅 1 处平台入口）；
> - **crates.io 上的官方 `gpui` 停在 0.2.2（2025-10），已过时，不要用。**

> ⚠️ **生态分叉风险（选型前必读）**：GPUI 已分裂成**互不兼容**的多条血脉，各自的 `Element` / `Entity` 类型不能混用：
>
> | 血脉 | crate | 备注 |
> | --- | --- | --- |
> | **Zed 主干再发布** | `gpui-pre`（+ `gpui-pre-platform` 等） | `repository = zed-industries/zed`，0.3.12 |
> | **longbridge 组件系（本项目现状）** | `gpui-kit` → `gpui-base` / `gpui-pre` / `gpui-component` | 组件最丰富；`gpui-kit` 只是门面，**内核仍是 `gpui-pre`** |
> | 旧基线（已弃用） | `gpui-unofficial` + `gpui-platform-gpui-unofficial` | 1.27，迁移前用；已切走，勿再引回 |
> | Zed 官方发布 | `gpui` 0.2.2 | 停在 2025-10，过时 |
> | gpui-ce 系 | `gpui_ce_components` | 另一分支 |
>
> **含义**：`Element` / `Entity` 等类型**只在同一血脉内兼容**；本项目现在用 `gpui-kit`
> （内核 `gpui-pre`），可以复用 `gpui-component` 的组件，但**不能**混入 `gpui-unofficial`
> 或 `gpui` 0.2.2 的类型。判定方法：写一个 5 行骨架直接编译验证，**不要只相信依赖图推断**。

---

## 2. 模型与委派路由（本文件最重要的一节）

### 2.1 两个引擎的定位

| 引擎 | 实际 id | 上下文 | 角色 |
| --- | --- | --- | --- |
| **DeepSeek** | `bluetrum/deepseek-v4.1-flash-1017` | **1,000,000** | **主力干活的**。快、便宜、能整仓吞入。一般事情全交给它。 |
| **GPT** | `bluetrum/gpt-6.1-sol` | 272,000 | **决策者**。参数更重、判断更稳。只用来做决策与攻坚。 |

选择依据不是"哪个更强"，而是**成本曲线**：
- DeepSeek 有 1M 上下文，把整个仓库丢进去也不心疼 → 适合**执行**（实现、批量修改、测试）。
- GPT 参数更大但窗口只有 272k → 适合**判断**（架构取舍、疑难诊断、方案评审），且必须喂**提炼过的决策简报**而非原始堆料。

> ⛔ **反直觉但重要**：正因为 DeepSeek 有 1M 窗口，**大范围上下文整合交给它**；
> 别把整仓库塞给 GPT —— 272k 装不下，且塞进去反而稀释判断力。

### 2.2 机制映射（这是硬约束，不是偏好）

关键事实：**`spawn_teammate` 没有 `provider` / `model` 参数**，teammate 一律继承会话默认模型（即 DeepSeek）。
只有 `subagent` / `subagent_fork` / `workflow` 的 `agent()` 能指定模型。

因此"一般 → spawn_teammate，复杂 → subagent"这条路由**由工具 schema 强制成立**，不存在走错的可能，
但也意味着：**想找 GPT 就只有一条路，想开 teammate 就只能是 DeepSeek。**

| 需求 | 用什么 | 模型如何确定 |
| --- | --- | --- |
| 一般开发、并行实现、批量机械改动 | `spawn_teammate` | 无需指定，继承 `bluetrum/deepseek-v4.1-flash-1017` |
| 单点决策咨询（只出判断，不写文件） | `subagent` | `provider: "bluetrum"`, `model: "gpt-6.1-sol"` |
| 需要继承**当前会话上下文**再决策 | `subagent_fork` | 同上 |
| 多个决策点并行 / 大批量交叉复核 | `workflow` 内 `agent()` | `opts.provider` / `opts.model` 同上 |

本环境 `allowedModels` 只放行这两个模型：
`bluetrum/gpt-6.1-sol`、`bluetrum/deepseek-v4.1-flash-1017`。
**调用 GPT 时 `provider` 与 `model` 必须成对给出**，缺一不可。

> ⚠️ **会话级限制（踩过一次，别再踩）**：模型选择授权由 Settings →「Model selection」→
> 「Allow agents to choose models for Subagents」控制，且**仅对新会话生效**——它是写入会话日志的 durable 决策。
> 若当前会话创建早于该配置，`subagent` 会报 `child LLM route "..." is not allowed for this Session`。
> 处理方式：**新开会话**，不要尝试改会话日志绕过。
> 判定依据：报错含 `without a recorded model-selection policy` = 本会话无授权，重试无用。
> teammate 不受此限制（只继承父路由）。

### 2.3 什么叫"需要决策或复杂" → 升级 GPT

命中**任意一条**即升级。有疑问时**倾向升级一次**，但不要重复升级同一问题。

**架构与不可逆选择**
- 模块边界划分、crate 拆分/合并、依赖引入或替换（尤其引入重依赖）
- **数据模型 / schema / 磁盘格式**：一旦写入用户数据，迁移成本极高
- 同步/合并方案（CRDT vs OT vs 单向覆盖）、冲突解决语义
- 编辑器内核路线（增量解析、rope/文本缓冲、渲染管线、光标与选区模型）
- 线程与异步模型（GPUI 的 `cx.spawn` / executor / 主线程约束怎么用）

**疑难与高风险**
- 性能病理：卡顿、掉帧、大文档渲染、内存增长 —— **需要先设计度量方法**的问题
- 疑难 bug：DeepSeek 连续 **2 轮**（最多 3 轮）未收敛 → 升级，别耗
- 安全与加密（本地加密、密钥派生、敏感数据落盘）
- 高风险 diff 评审：触及存储格式、并发、数据迁移的改动
- 跨模块重构方案（"怎么改"的规划，而非执行）

**需求与产品**
- 需求歧义、产品取舍、交互范式选择
- 两个方案都可行且各有代价 —— 这类**必须**让 GPT 拍板，不要自己随机选

### 2.4 什么叫"一般的事情" → 交给 DeepSeek teammate

- 常规 UI 布局、组件、样式、主题、间距
- **按既有模式**实现 CRUD、命令、快捷键、菜单项
- 单元测试、集成测试、快照测试、测试夹具
- 文档、注释、README、commit message、PR 描述
- 机械性批量改动：重命名、替换、格式化、升级调用点
- 脚手架、构建配置、CI 脚本
- 缺陷复现与最小化（复现出来交给 GPT 定位，或自己修掉）
- 在**已定方案**下逐条落地

> 判定口诀：**"怎么做"已有定论 → DeepSeek；"该怎么做"尚无定论 → GPT。**
> 决策一旦产生，立刻把结论写进仓库（ADR 或代码注释），后续执行就不再问 GPT。

### 2.5 调用范式

**A. 一般开发 → teammate（DeepSeek）**

```jsonc
spawn_teammate({
  name: "editor-ui",                 // 小写连字符，唯一
  description: "实现笔记列表与编辑器视图",
  context: "fresh",                  // fresh=不带我的历史；fork=继承我的上下文
  prompt: "<完整自包含任务书：目标、验收标准、写作用域、禁区>"
})
```

要点：
- **prompt 必须自包含** —— teammate 看不到你的对话历史（除非 `context: "fork"`）。
- 明确**写作用域**（允许改哪些文件/目录），并与其他 teammate **不重叠**。
- 一个 teammate 一个可交付的垂直切片，不要按"层"切（避免互相阻塞）。

**B. 决策 → subagent（GPT）**

```jsonc
subagent({
  description: "decide storage format",     // 3-5 词
  provider: "bluetrum",
  model: "gpt-6.1-sol",
  reasoning_effort: "max",                  // gpt-6.1-sol 仅支持 low / max；决策用 max
  prompt: "<决策简报，见 §2.6>"
})
```

**C. 需要我的上下文再决策 → subagent_fork**

同上，但 `subagent_fork` 自动继承已完成对话，**简报里不必复述背景**，只写"要你决断什么"。

**D. 多决策点并行 → workflow**

```js
const d1 = await agent("<存储格式决策简报>", {
  label: "decide: storage", phase: "decisions",
  provider: "bluetrum", model: "gpt-6.1-sol",
});
const d2 = await agent("<同步模型决策简报>", {
  label: "decide: sync", phase: "decisions",
  provider: "bluetrum", model: "gpt-6.1-sol",
});
```

只有在**确实有 3+ 个独立决策点**时才上 workflow；2 个以内直接开 subagent 更省事。

### 2.6 决策简报模板（喂 GPT 用）

GPT 看不到仓库时，简报质量决定决策质量。**必须**包含：

```
【问题】一句话说明要决断什么（要具体，不要"设计架构"这种）
【约束】平台/依赖/性能/工期/不可逆点；已知的硬性限制
【现状】相关代码路径与行号，或"仓库为空，全新项目"
【候选方案】2-4 个，各 1-2 句，附主要代价
【已排除】考虑过但不选的方向及原因（避免 GPT 重复提）
【产出要求】推荐方案 + 理由 + 风险/回滚方式 + 落地步骤（可执行粒度）
```

**不要**把整个仓库源码贴给 GPT。要贴就给 3-5 个关键文件的最相关片段 + 结构摘要。

### 2.7 禁止事项

- ❌ 用 `spawn_teammate` 求决策 —— 它只能是 DeepSeek，拿不到 GPT。
- ❌ 为机械改动开 GPT —— 贵且慢，这是 DeepSeek 的活。
- ❌ 把整仓源码灌给 GPT —— 272k 装不下，且降低判断质量。
- ❌ 同时开 3 个以上 GPT 决策 subagent —— 串行更省，决策往往有依赖。
- ❌ 同一问题反复升级 —— 升级前先把上一轮的结论落库，带着结论问，别重来。
- ❌ 让两个 teammate 写同一批文件 —— 写作用域必须不相交。
- ❌ 在没确认方案前让 teammate 大改存储层 —— 先决策，后执行。

---

## 3. 协作纪律（Agent Teams）

- **Lead（主会话）** 负责：路由判断、任务拆分、最终集成、`fmt`/`clippy`/`test` 把关、review 全部 diff。
- **teammate** 负责：各自写作用域内的实现与自测，完成后回报改动文件与验证结果。
- 共享任务板：`team_task_create` → 认领 → 完成后回写结论；存在依赖时用 `blocked_by`。
- 并行度：一般 2-3 个 teammate 为佳。超过 4 个时先问自己是不是该先做决策。
- **写作用域不相交**是硬要求；若冲突，先拆文件再并行。
- 交付前 Lead 必须亲自跑构建与测试，不接受"teammate 说通过了"。
- **所有改动在 git 下进行**：提交前必须 `cargo fmt` + `clippy --all-targets -- -D warnings` + `test` 通过；
  提交信息用中文讲清"为什么"，一次提交只做一件事；**禁止** `git commit --amend` 或改写他人历史。
  Lead review 一律看 `git diff`，不再接受"仓库无 git、无法给 diff"。
- **UI / 交互类改动必须先真机验证再提交**：单元测试证明不了"看起来对不对"（光标形状、候选窗位置、选区像素）。
  未验证的改动用 `wip:` 前缀提交，验证通过后再补一个正式提交；**不要**把未验证的东西混进已验证节点。
  （教训：`f4b49f6` 是先提交后验证的产物，随后被 `7d95610` 整体推翻。）
- **发布点用 annotated tag 标记**（首个：`v0.1-slice6` = 切片 6 真机验收通过的完整闭环）。
  需要回滚、对照或复现时 `git checkout <tag>`；tag 只打"已被真人验过"的状态，不打 WIP。

---

## 4. 代码约定

- `cargo fmt` 必须干净；`cargo clippy -- -D warnings` 必须通过。
- 错误处理：库层用 `thiserror` 定义错误枚举，应用层用 `anyhow` 汇总；**禁止**在业务路径 `unwrap()`（`expect()` 需带上下文信息）。
- GPUI 视图：一个视图一个模块，`Render` 实现保持纯渲染，副作用走 `cx.spawn` / 事件。
- 状态变更集中在少量可测的 state 类型上，UI 只做投影；便于后续加 undo/redo。
- 平台相关代码进 `#[cfg(target_os = "...")]` 或 `platform/` 模块，业务层不得出现平台分支。
- **异步/后台任务的测试必须等明确收敛点**（`DurableAck`、物化完成、事件收尾），**禁止**用 sleep 或"事件队列为空"当收敛判据——
  后者会把后台异步事件当成 bug（`2b8ed22` 的教训：B 物化事件与断言窗口重叠，5% 偶发）。
- 新依赖需在提交信息里说明理由与替代方案（重依赖须先经 GPT 评审）。
- 注释与文档用**中文**；标识符用英文。

---

## 5. 规划中的仓库结构（待确认，勿当既成事实）

```
menghunote/
├─ crates/
│  ├─ app/        # 桌面入口、窗口、GPUI Element、native 输入/shape/executor 适配
│  ├─ ui/         # 可复用视图与主题（后补）
│  ├─ editor/     # 编辑器纯模型：Rope 镜像、选择、ProjectionMap、IME 输入文档（不含 GPUI）
│  ├─ i18n/       # 纯 i18n：Locale/消息 id/静态 catalog/t·t_with·t_count（零外部依赖、无 GPUI）
│  ├─ plugin-api/ # 纯插件 manifest/host 契约：PluginId/Manifest/Capability/HostServices（serde + i18n，无 GPUI/storage/sync）
│  ├─ text/       # 唯一坐标/单位原语：SourceByte/UnicodeScalar/InputUtf16/DisplayByte、SourceRange、CoordError
│  ├─ core/       # 领域模型（后补，暂未落地）
│  └─ storage/    # SQLite/CRDT/物化/同步 + session（EditorDocumentPort 的串行 reducer）
├─ docs/
│  ├─ briefs/     # 决策简报（喂给 GPT 的输入，提案阶段）
│  └─ adr/        # 架构决策记录（GPT 决策的结论落地处）
└─ AGENTS.md
```

**依赖方向（ADR 0003 + ADR 0006 + ADR 0020，禁止成环，含 dev-dependency）**：
`app → editor/storage/text/gpui/sync/i18n`（`app` 可依赖 `plugin-api`；P2 注册表尚未消费它）；
`plugin-api` **不依赖** GPUI/storage/sync/relay/editor（仅 `serde` + 纯 `i18n`，无 IO/加载器）；
`i18n` **不依赖** GPUI/storage/sync/editor（纯 Rust、零外部依赖）；
`editor → storage/text`；`storage → text/loro/rusqlite`；
`sync → storage + sync-proto`；`relay → sync-proto`；`sync-proto` 纯类型（serde，无 IO/crypto/Loro/SQLite）；
`text → unicode-segmentation`；`storage → markdown`（仅 L2 正文链接物化/重写，markdown 不反向依赖 storage）。`storage` **不依赖** `editor`/`sync`；`editor`/`storage`/`text` **不依赖** GPUI；
`relay` **不依赖** storage/Loro/Tantivy。

**决策落地规则**：GPT 给出的每个架构级结论，都要在 `docs/adr/NNNN-*.md` 留一页
（背景 / 选项 / 结论 / 后果）。这样后续执行不再重复问 GPT。

---

## 6. 构建与验证

**已实测通过（2026-11-08，cargo 1.99.1 / rustc 1.99.1 / Windows）**：

```bash
cargo build -p menghunote-app     # ✅ 冷构建约 3m50s
cargo run   -p menghunote-app     # 默认 sync-ui → AppRoot 同步向导（Welcome/Create/Join）
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test  --all
```

> ⚠️ **沙箱内拉依赖会失败（已踩过）**：在本会话的受限沙箱里执行 `cargo build`，拉依赖阶段会报
> `schannel: AcquireCredentialsHandle failed: SEC_E_NO_CREDENTIALS (0x8009030e)`。
> 已定位**不是网络问题**（Python/OpenSSL 与 `git` 均可正常访问 crates.io，实测返回 200），
> 而是受限进程无法访问 Windows 用户凭证库。
> **处理方式**：对该条构建命令单独申请 `danger-full-access` 审批即可，无需改动任何配置或代理。
> 若不明原因又出现网络类报错，先跑 `python -c "import urllib.request; print(urllib.request.urlopen('https://index.crates.io/config.json', timeout=15).status)"`
> 区分"网络不通"与"沙箱阻挡 TLS"。

**同步 UI 的 feature 矩阵（2026-11-18，P1 同步产品化）**：

```bash
cargo build -p menghunote-app                  # 默认 = sync-ui → AppRoot 同步向导（Welcome/Create/Join）
cargo test  -p menghunote-app                  # 默认（sync-ui）测试
cargo test  -p menghunote-app --features dev-sync    # dev：local_dev / --dev-sync* CLI / 测试 setup / V2 管理
cargo test  -p menghunote-app --no-default-features  # 旧默认：纯 dev vault（无同步 UI）
```

- `sync-ui` 是 **default feature**，产品构建只需要它；`sync` 只提供协议/能力、不含 UI；
- `dev-sync` = `sync-ui` + `local_dev` 许可 + dev CLI/setup + localhost relay 门禁 +
  V2 管理实验入口。**产品构建不得启用 `dev-sync`**；
- 默认构建仍必须 `mode_allowed(LocalDev) == false`（链接/表单入口也会明确拒绝）；
- 细节与 cfg 分类见 [`docs/notes/sync-product-p1.md`](docs/notes/sync-product-p1.md)。

Lead 在给出"完成"结论前，必须实际执行并贴出结果。

> ⚠️ **本机构建环境的第二个坑（2026-11-09 实测）**：cargo 启动的子进程（build script / test binary）
> **不能写 `%TEMP%`**（`Access denied, os error 5`；bash/python 不受影响）。由此产生两条固定做法，勿随手回退：
> 1. 根 `Cargo.toml` 的 `[profile.dev.package.libsqlite3-sys] debug = false`：`cl.exe` 在 build script 上下文里
>    带 `-Z7`/`-Zi` 编译 `sqlite3.c` 必报 `D8050 ... failed to get command line into debug records`。
>    删掉该节会直接构建失败（已反证）；换到可写环境后可删。
>    同因还有 `[profile.dev.package.stacker] debug = false`（`loro → pest → pest_stack_guard → stacker`，
>    stacker 带 `src/arch/windows.c`）。**今后任何带 C 源的新依赖撞到 D8050，都按同样方式单包覆盖并注明理由**，
>    不要用全局 `debug = false`（会连 Rust 依赖的调试信息一起关掉）。
>    2026-11-10 又命中第三个：`zstd-sys`（`tantivy → zstd`），已加 `[profile.dev.package.zstd-sys] debug = false`；
>    注意探针用的独立 crate 不触发，**只有本仓 target 下会撞**。
> 2. 测试临时目录用 `CARGO_TARGET_TMPDIR`（`target/tmp`），**不要**用 `tempfile::tempdir()`。

> ⚠️ **别让管道吞掉退出码**：`cargo build | tail -40` 的退出码来自 `tail`，构建失败也会显示成功。
> 用 `cargo build ...; echo "EXIT=${PIPESTATUS[0]}"`，或干脆不加管道。这类"假通过"会把失败带进下游。

> ⚠️ **UI 自动化前先清旧进程**：Windows 上残留的 `menghunote-app.exe` 会占用 exe，`cargo build` 报
> `拒绝访问 (os error 5)`，若脚本不检查退出码就会继续跑——**截图与日志全部来自旧二进制**。
> 固定做法：先 `Stop-Process -Name menghunote-app -Force` → 确认 `cargo build` EXIT=0 → 再启动。
> 2026-11-10 踩过一次，差点把旧截图当成折叠功能的证据。

> ⚠️ **UI 自动化用真实鼠标事件**：`PostMessage(WM_MOUSEMOVE/WM_LBUTTON...)` 注入点击会丢事件
> （同一坐标时到时不到），截图/断言会假失败。用 `SetCursorPos` + `mouse_event`/`SendInput` 走真实输入路径；
> `PrintWindow` 也可能拿到上一帧 surface，断言前先等一帧或重抓。
> 另外本机会有其它前台窗口（含 DSH GUI）抢焦点，`SetForegroundWindow` 可能"假成功"、点击打到别处——
> 点击前用 `HWND_TOPMOST` 强制置顶；出现"点了完全没反应"先查这个，别先怀疑业务代码。

> **UI 逻辑测试（2026-11-13 起）**：`cargo test -p menghunote-app --test headless_ui`
> （`#[gpui::test]` + `TestAppContext`，真实 `AppShell` + 临时 dev vault，无窗口渲染并断言布局/状态）。
> 它**替代不了真机**：像素、IME、DPI、真实窗口管理仍必须用 `ui_smoke.ps1` 在 Windows 上真机验。
> Linux 跑该测试需装 `libxkbcommon-dev libxkbcommon-x11-dev`（Debian 系名），细节见
> [`docs/notes/ui-headless-tests.md`](docs/notes/ui-headless-tests.md)。

---

## 7. 待决策清单（交给 GPT）

按优先级；括号内为决策类型。

1. ~~**本地存储格式 + 数据流架构**~~ —— **已决**（2026-11-08，`gpt-6.1-sol`）：见 [`docs/adr/0001-storage-format.md`](docs/adr/0001-storage-format.md)。输入为 [`docs/briefs/0003-storage-sync-consolidated.md`](docs/briefs/0003-storage-sync-consolidated.md)；原 `0001-storage-format.md` 简报已作废。
2. ~~**编辑器内核：Typora 式所见即所得**~~ —— **已决**（2026-11-09，`gpt-6.1-sol`）：见 [`docs/adr/0002-editor-core.md`](docs/adr/0002-editor-core.md)。选**源码投影编辑器**（A，按语义块布局）；C 只作内部 v0/alpha，**v1 必须含行内标记隐藏与进入显现**。
3. GPUI 状态管理与异步模型在本项目的规范用法 —— *(架构)*
4. 笔记领域模型（笔记/块/标签/双链/查询的抽象粒度） —— *(数据模型)*
5. ~~**撤销/重做与命令栈设计**~~ —— **已决**（并入 [ADR 0002](docs/adr/0002-editor-core.md) §8）：单一正文 `UndoManager`（本 peer）、750 ms 同向合并、远端 import 不进本地 Undo 项但须被变换、v1 100 组且不跨重启。
6. ~~同步协议的服务器职责边界（relay-only vs 存 snapshot）~~ —— **已决**（并入 ADR 0001）：持久**密文**中继 + 密文快照/manifest；relay 不解析明文、不见密钥、不做搜索。
7. ~~**模块边界与依赖方向（crate 拆分）**~~ —— **已决**（2026-11-09，`gpt-6.1-sol`）：见 [`docs/adr/0003-module-boundaries.md`](docs/adr/0003-module-boundaries.md)。新建 `crates/text` 收口坐标/单位；会话端口放 `storage::session`；`editor → storage/text`。
8. ~~**同步/加密 v1 的实现契约**~~ —— **已决**（2026-11-11，`gpt-6.1-sol`，max）：见 [`docs/adr/0006-sync-v1.md`](docs/adr/0006-sync-v1.md)。输入 [`docs/briefs/0004-sync-v1.md`](docs/briefs/0004-sync-v1.md)；冻结 MNE1 信封（196B header + AAD + HKDF 域）、密钥槽/DPAPI/恢复码、Ed25519 设备鉴权 + HPKE grant、relay API/DDL/游标、两 checkpoint GC、`key_epoch` 轮换；wire 不可静默变更。

---

## 8. 当前状态

> **同步/加密 v1 交接入口（2026-11-13）**：见 [`docs/briefs/0007-sync-handoff.md`](docs/briefs/0007-sync-handoff.md)。
> 三个阻断已修：ASSET 来源认证/覆盖实证 + post-activation + Linux 封装（`2c88f72`）；
> 同一 durable E1 跨两轮 e2e 与三条 e2e 去脚本化（`61d8de4`）。
> 本机（Linux）基线：`cargo test --workspace --exclude menghunote-app` = **1006 passed**；sync = **417 passed**。
> **`v0.4-sync` 已由 GPT 复审 APPROVE（原锚点 `61d8de4`；整理前 `321f732`，无必修项）并打 annotated tag**；2026-11-13 前移至 `d16f6fe`（旧锚点 `v0.4-sync-backup-20261113` → `738f3f0`）；非生产，仅 localhost 开发验收。
> **Windows 实机已由用户于 2026-11-13 在 `5538344` 包上实测通过**（产品代码与 `v0.4-sync` tag 树一致；命令/计数未逐条留档，复跑命令见 0007 §5）。
> v0.4 **只支持修复后新建的 vault/身份**，旧开发 vault 失效（旧签名 checkpoint 无法仅靠本地版本拒绝隔离）。
> 后置：`task-123`（默认删凭证吊销 + 轮换 opt-in）、`task-124`（metadata 事件认证对账/续读）。
> `task-121`（relay reservation 复用）**已完成**（2026-11-18）：同字节跨 reservation
> （rotation/checkpoint/普通）`PUT parts` 幂等复用、对象只存一份且不重复计费；不同字节
> 仍 `PART_CONFLICT`，补缺失分片仍要求同一预算来源；**wire/错误码/schema 未变**。
> 详见 [`docs/notes/task-121-relay-reservation-reuse.md`](docs/notes/task-121-relay-reservation-reuse.md)。
> `task-122`（验证修订 2）**已完成**（2026-11-19，未 commit）：`CheckpointCommit`
> v2 追加 `validation_digest`（`SHA256(MN/v1/checkpoint-validation\0 || CDE(...))`），
> 9 字段 v1 仍可解码，但**所有消费者都显式拒绝**——relay publish、bootstrap、rotation apply、
> checkpoint manifest 都 `require_current_validation()` 拒绝 v1；relay 行只落 v2，
> bootstrap/activation/GC 只选 v2 published，GC 不足两份 v2 即 blocked；本地
> coverage/verified receipt 升 v3/v2 并加 HMAC-SHA256，密钥
> `HKDF(K_epoch, salt=vault_id, info=MN/v1/checkpoint-receipt-key\0)`，MAC 域
> `MN/v1/checkpoint-receipt-mac\0`，旧回执/篡改回执一律 fail-closed。
> **完整预算框架明确未做**（范围过大，只完成 1–3；无 quota 代码改动）。
> 测试：sync-proto **64** / relay **380** / sync **617** / app `--features dev-sync` **680**；
> `e2e_rotation_two_rounds` = `TWO_ROUND_EXIT=0`。详见
> [`docs/notes/task-122-validation-v2.md`](docs/notes/task-122-validation-v2.md)。
> Linux 开发适配见 ADR 0008（0600 裸主钥，安全弱于 DPAPI，不宣称等价）。
> **dev-only 同步 UI（ADR 0009）Phase 1 已完成**：真实 AppShell 内面板 + 持闸 SyncOnce +
> 活动文档 adopt；面板「发布基线」、设备列表/「移除设备（不轮换）」已接通。
> **UI 初始化（ADR 0011 S1–S3，2026-11-14，当前 `972a083`）**：`AppRoot` 同窗根视图；
> Create/Join 向导（恢复码一次性展示+备份确认、失败恢复、实例目录锁/公开配置），
> 真实用户不再需要 CLI/env；命令行仅保留自动化/恢复兼容。
> **自动同步（ADR 0012 S1–S3，2026-11-13，当前 `184a8dd`）**：两阶段（网络不持闸、
> integrate/adopt 短持闸）+ 编辑 debounce 2s/周期 45s/退避 + 状态栏 + 重启待应用恢复；
> 手动同步与发布/设备命令优先。并行测试改为显式事件收敛（连续 10 次全绿）。
> **轮换 UI（ADR 0009 Phase 2 / 切片 6a–6b）**：设备面板「轮换密钥…」/「应用轮换」、
> 确认/警告、可选恢复码重封装、signed revoke；轮换期间整段持闸，成功后重建活动视图切新 epoch。
> **生产 relay 机制（ADR 0013 S1–S3b）**：ProfileV2 的 `local_dev|remote_tls`、唯一 URL parser、
> durable `sync.control_pin`（record+bootstrap 各自 rev/CAS）、`RelayContext` 统一全部操作、
> remote Create/Join 走受限 `InitSession`、安全指纹与 SecurityPaused、启动 pending 对账；
> 离线 TLS harness 真跑。**真实鉴权 relay/Windows/DPAPI/TLS 系统根仍未验，不打生产 tag。**
> **附件**：旧的侧栏「插入附件…」路径输入条**已移除**；粘贴（`Ctrl+V`/`Ctrl+Shift+V`）
> 与拖放是唯一导入入口（ADR 0019 §8）。剪贴板分流优先级：本应用元数据文本 → 文件 →
> 图片 → 普通文本；每批 ≤10 文件 / ≤100 MiB / 单资产 ≤20 MiB，整批一个撤销组；
> 内容寻址去重；正文提交后按 `asset://` 扫描**整篇对账** `asset_refs`；无资产 GC。
> **Windows 真机（2026-11-14，用户确认）**：dev-sync Create/Join/自动同步闭环正常；
> TLS/remote_tls 未测（按用户要求后置）。
> **邀请链接（ADR 0014 S1）**：创建端生成 15min 一次性 `mn-pair://join?...`（token 仅
> fragment）；加入端 redeem 为 PENDING（durable、可重启，token 不落盘）；创建端批准后
> 加入端取 self grant/bootstrap 进 Main。
> **多 vault（ADR 0014 S2–S3b）**：relay schema 11、22 张租户表带 `vault_id` +
> `VaultScope`、v1→v10→v11 迁移；`/v2` 全数据面 + 一次性创建票据 + v2 租户鉴权/限流；
> 客户端 `api_version`、`mn-pair://create` 票据创建、同 relay 多实例、v2 邀请链接 `api=2`。
> **ManagementCapV2（ADR 0015 S4a–S4c + ADR 0016 S4d1a/b，2026-11-14，当前 `b32a5b2`）**：
> schema 12；relay 控制提交链/CAS/dedup、Upgrade/Cap/Device/Invite/rotation/Recovery 全链路；
> 客户端库已能回放控制链、选择 cap、realm3 鉴权、ApprovalEnvelope 双签、submit/回执/重放。
> **S4d1c App 管理 UI + S4d2 relay 审批收件箱已实现**（见 ADR 0016 实施记录）；
> relay schema 14（approval_jobs/signatures）；真实 vault 未启用 V2、TLS/Windows 未验。
> **V2 管理 UI 默认隐藏**：个人用户使用 V1 设备列表/邀请/移除即可；设置页只在
> `MENGHUNOTE_EXPERIMENTAL_V2=1` 时显示 V2 管理区（实验）。
> **内嵌 Web 管理 + 旧入口移除（ADR 0018 S1–S4b，2026-11-16，当前为 12 组语义提交）**：
> S1 独立 admin listener/token/session/CSRF/Host 白名单；S2 schema 13 `vault_registry` +
> 原子 claim/cancel；S3 fenced 在线删除（admission + 事务复核、WS 关闭、分批可恢复 purge）；
> S4a 最小 Web 管理页面（列表/详情/创建领取链接/取消/删除确认）；S4b 移除旧入口：
> `POST /v2/admin/create-tickets`、`sync::admin_ticket`、App「管理员创建」/admin secret、
> 主 CLI `delete-vault`、`dev_server --issue-ticket`。App Create 只保留「粘贴创建链接」；
> `status` 改为只读 existing-only（不迁移/不建库）。首次配置改为 Web 管理面
> 「创建领取链接」→ App 粘贴；手工步骤见
> [`docs/notes/dev-sync-manual-test.md`](docs/notes/dev-sync-manual-test.md) 的 S4b 章节。
> 测试：relay 380 / sync 618 / app 默认 **546** / `--features dev-sync` **697** / `--no-default-features` **418** / i18n **16** / plugin-api **24** / storage **201** / editor 287 / markdown 45。
> **Markdown/列表（ADR 0019 S1，2026-11-16）**：新增纯 crate `crates/markdown`
> （`pulldown-cmark = "=0.13.6"`，default-features=false），列表块树/投影/GPUI
> gutter/hanging indent/Enter 同级与空项退出；图片 Object Block 只读预览（S2a）。
> 回归：无空格 `-`/`1.`/`#` 不成块、列表缩进不画引用条、空列表 Backspace 只删空格、
> 删到纯数字后不残留惰性续行缩进/空缩进行、空白行不继承列表缩进。
> **S2b（2026-11-16）**：粘贴/拖放导入、资产入库、单事务插入、引用对账、旧路径输入条
> 移除已完成（ADR 0019 §8）；Mermaid/表格 Object 与物理资产 GC 仍后置（S3）。
> S2a：整行图片 Object 投影（占位符 U+FFFC）、后台受控解码（PNG/JPEG/WebP/GIF、
> 边长≤8192/16MP/128MiB、LRU 128MiB）、选中不展开源码、工具条编辑/复制、
> 缺失/损坏降级占位；headless 7 个图片集成测试。
> S2b 测试：app 默认 **183** / `--features dev-sync` **414**；storage+editor+markdown+text
> **361**；新增 `headless_import`（10：粘贴 PNG/同图去重/多文件单事务+撤销重做/失败整批
> 回滚/文本与强制文本/IME/对账幂等/拖放/SVG/持闸）、`attachment` 单测 6、
> `editor::clipboard` 单测 6、`replace_asset_refs` 单测 4。
> 历史整理：整理前 97 个提交完整保留在 `backup/dev-before-cleanup-20261116`；
> `v0.4-sync` tag 未移动；`v0.5-sync-web` 打在整理后的 12 组 tip `bcb20e8`。
> **同步 UI 收口（2026-11-16，`71f0164` + 后续微调）**：主界面不常驻 DevSyncPanel；
> 侧栏动作行（＋笔记/＋文件夹/导出）移到列表上方；唯一的「⚙ 设置」在侧栏底部，
> 打开 AppRoot 设置页（relay/实例/安全指纹 + 可展开的同步面板）；底栏只保留紧凑同步状态
> （点击仍可打开 AppShell 测试用同步设置页），不再有独立齿轮。侧栏旧状态移除；
> `sync-status` / `settings-panel` / `settings-close` / `settings-toggle-sync` 为固定 selector。
> worker/持闸/协议未动。
> **同步卡死修复（2026-11-16，`969a6f7`）**：窗口激活回调把
> `scheduler.on_network_recovered` 的 `Start` 决策丢弃，scheduler 留在 Running 但没人派发；
> 手动同步永远 `单操作在途`，重启后激活再次触发。现改为退回 pending（清
> `last_auto_start`），下一拍 tick 正常派发；另保留“无真实 op 时清理陈旧轮次”保险。
> 回归：`window_activation_never_leaves_scheduler_running_without_dispatch`。
> **图片对象交互（2026-11-17）**：悬浮/选中显示 Obsidian 式两按钮「放大」「编辑源码」；
> 放大按钮或已选中图片再次单击打开遮罩（点击/Esc 关闭）；图片 hitbox 悬停手形；
> 原「复制 Markdown」按钮移除以保持两按钮。
> **Windows 剪贴板 BMP（2026-11-17）**：CF_DIB 经 GPUI 转 BMP，`stage_image` 落库前
> 统一转 PNG（`image` 增加 `bmp` feature），粘贴写出 `![粘贴图片.png](asset://…)`
> 并投影成图片对象；CF_DIBV5 仍是缺口。
> **普通链接打开（2026-11-17）**：`[text](url)` 解析 destination，按可见文本区间命中，
> **单击即打开**（Ctrl/Cmd 同样打开），悬停手形；末尾边界不误触；其它 scheme 明确拒绝。
> **交互修复（2026-11-17）**：设置页按窗口高度限高并可滚动；wiki 链接悬停手形、
> 末尾边界不跳转、点击仅命中 display 可见区间（修复上一行误跳）；右键菜单动作后
> 吞掉同一次手势的 mouse-up，修复根目录笔记/文件夹右键重命名被立刻提交。
> **表格拖拽反馈（2026-11-17，S3b6）**：点列/行把手立即选中整列/整行（半透明填充+
> 强调边框，把手变色）；拖动超过 3px 只在表头顶线/表格左线显示一个跟随指针的小块
> ghost +「第 N → 第 M」文案（不复制整行/整列），插入线/落点同步；Esc/点外清除选中，
> 单元格点击优先；drop 后选中移动到新下标。
> **实测修正（2026-11-17）**：单元格改为单击即进入**原地浮层编辑**，表格始终完整
> 显示、不再压成一行 cell 文本；复用 RenameInput 的 IME，Enter/Esc/点外/Tab 走同一
> 提交路径；编辑时隐藏原 cell 文字与正文选区/光标，避免重影/双光标。
> 图片遮罩支持滚轮 0.5–4.0 缩放并阻断底层滚动；图谱滚轮也阻断穿透。
> **S3b5 表格拖拽重排（2026-11-17）**：悬浮/选中表格显示列顶部 / 数据行左侧把手，
> 拖动按槽位重排行/列，插入线提示，Esc/拖出取消；一次释放一个 EditRequest + undo，
> caret 不变、表格仍为 Object；行尾换行留在槽位，列重排对齐一起移动。
> **图谱 G0+G1（2026-11-17）**：`GraphModel`（tree+notes 节点、已解析 wiki 边、去重权重、
> 自环/未解析/悬空统计、确定性圆周布局、800/3000 上限）+ 侧栏「图谱」全屏覆盖层；
> 平移/缩放/悬停邻居/点节点打开笔记。
> **图谱 G2（2026-11-17）**：局部图 depth 1–3（默认 2，无当前笔记回退全库）、
> 文件夹/标签/未解析过滤、搜索定位/居中/Enter 打开、LOD、方向箭头；未解析只画
> 灰色虚线占位（不可打开），不自动建笔记。大库后台布局/缓存仍待 G3。
> **Mermaid M1（2026-11-17）**：`merman 0.10.0` 精确 pin（all-diagrams+complete-svg，
> 不引 ELK/Node/WebView）；`render_svg` 输入/输出上限 + catch_unwind + SVG 形状校验；
> 11 个渲染测试。
> **Mermaid M2（2026-11-17）**：```mermaid fenced block 投影为 Object；后台线程
> `ResvgSafe` SVG → GPUI `SvgRenderer` 光栅化，per-view LRU 64MiB/64 项；渲染中占位、
> 失败回退源码、工具条「编辑源码/重新渲染」；无新增依赖。**光标/选区在 block 内时
> 保持完整源码可编辑、不投影 Object，布局回落到普通 fenced code 行高（不再出现
> “大光标”）；**非空选区跨过 block 时仍保持 Object 并正常渲染**，拖选不会突然
> 切源码/变长；空 Mermaid 永远显示源码、不出现“等待渲染”占位。**
> **Mermaid M3a（2026-11-18，暗色 + CJK 回退）**：`render_svg_for_preview(source,
> ThemeId)` 用 merman 内置主题编译器绑定 `RenderRequest::with_theme`——
> 浅色 `ThemePreset::EditorLight`、深色 `EditorDark`；`MermaidService` 的 `Job`/
> `RenderFn` 携带 `ThemeId`，worker 按 key 的主题渲染（不读全局状态），浅/深色
> key 各渲染一次、切主题旧 key 失效、切回命中缓存。编译预设前把
> `Microsoft YaHei` / `Noto Sans CJK SC` 插入 SVG 字体栈（通用族仍在最后；
> 不引入字体文件/依赖）；真实中文标签在 headless 深色 AppShell 下占位 → Ready，
> 真实 `SvgRenderer` 光栅化有文字墨迹（对照空标签 0 像素）。浅色侧 `A-->B` 的
> 自然尺寸由 168×156 → 168×162（EditorLight 排版），headless 几何断言已同步。
> 测试：app 默认 **380** / `--features dev-sync` **626**；mermaid 单测 16、
> mermaid_preview 12、headless_mermaid 5 + headless_mermaid_theme 1；fmt/clippy 干净。
> **导出 / 子进程超时隔离仍待 M3 后续；Windows 真机字体回退观感未验。**
> **反引号补全（2026-11-17）**：普通输入自动配对、成对中间跳过、行首 ``` 自动补
> fenced block；表格/代码块/HTML/IME 组合中不触发，一次按键一个撤销组。
> **资产 GC dry-run（2026-11-17）**：storage 只读扫描候选，保护根=正文引用/历史版本/
> 同步 outbox/无本地字节/baseline journal；保护根解析不确定时整次 fail-closed；
> 只报告，不物理删除、不改 orphan_since/schema。
> **表格 cell 行内 Markdown（2026-11-17，S3b7）**：cell 非编辑时渲染粗体/斜体/
> 删除线/行内代码/普通链接，编辑浮层仍显示源码；`\|` 在行内代码里仍是字面 `|`；
> 不支持块级/图片/合并单元格/富文本；cell 链接单击打开（不做 Ctrl/右键）。
> 编辑器内部灰色状态条已移除，保存/同步状态统一由 shell 底栏承担。
> 列表项（含引用内列表）的行内 Markdown 有 app headless 回归，光标在行尾也不会显露标记。
> 侧栏提示（未解析链接/导入错误）移到「图谱」入口**上方**，不再把图谱顶上去。
> 设置覆盖层拦截滚轮/触控板滚动，滚设置页不再带动底层笔记滚动。
> V2 管理 UI 默认隐藏（`MENGHUNOTE_EXPERIMENTAL_V2=1` 才显示）；个人路径回到 V1。
> 设备页默认只保留设备列表/邀请/移除；轮换入口默认隐藏（`MENGHUNOTE_EXPERIMENTAL_ROTATION=1`）。
> **S3b3/S3b4 表格删除与右键插入（2026-11-17）**：表格浮层增删行/删列
>（表头不删、至少保留 1 列、删除后修正选中格）；编辑器右键「插入 → 表格」，
> 默认 2 列 × 1 数据行，一条 EditRequest，右键不移动 caret。拖拽重排仍未做。
> **S3b2 表格单元格编辑（2026-11-17）**：单击选中格、再次点击/F2/Enter 就地编辑；
> 编辑态 display 只显示该 cell 文本，草稿只写镜像，提交/`Tab` 一条 `EditRequest`，
> `Esc` 零请求恢复；未转义 `|` 写 `\|`；加行/加列/任务/图片回归通过。删除行列、
> 拖拽重排仍未做。
> **S3b1 表格加行/加列（2026-11-17）**：表格右下角浮层「+ 行」「+ 列」；只插入
> 不替换，加列同时扩表头/分隔行/所有数据行；一次点击一个 EditRequest/undo 组，
> caret 不落进表格内部。单元格编辑、删除行列、拖拽仍是 S3b2。
> **S3a 表格 Object（2026-11-17）**：GFM 表格解析成 `TableBlock`（表头/分隔行/数据行/
> cell 源码区间/对齐），图片与表格共用 `ObjectBlock` 投影；只读表格自绘、整块选中、
> 工具条「编辑源码」/Esc 往返；单元格编辑、加行加列、拖拽仍是 S3b。
> **S3 任务列表（2026-11-17）**：任务框 `[ ]`/`[x]` 解析出精确源码区间，
> 隐藏态自绘复选框；点击只发一个 `EditRequest` 切换 3 字节（不动 caret/其它字节），
> 进 undo；嵌套/有序/`[X]` 独立切换。表格 Object、物理资产 GC 仍未做。
> **Wiki L2（2026-11-17）**：L2a 正文 B 事务按解析结果重建 `links`（`kind='wiki'`，
> 当前结构树解析 `to_note_id`，未解析写 NULL）+ 状态栏「反向链接 N」右侧面板；
> L2b 重命名联动改写（仅解析到该笔记的 target；alias/heading/路径保留；活动来源走
> 活动 driver 防 stale 覆盖；失败只报告不回滚）。未做持久化重试入口。
> **Wiki 双链 L1（2026-11-17）**：`[[Note]]` / `[[Note|alias]]` / `[[Folder/Note]]` /
> `[[Note#Heading]]` / `[[#Heading]]` 解析、投影隐藏括号、单击打开、unresolved 灰色波浪线；
> 目标解析按路径精确 > 标题（大小写不敏感）> 最短路径，歧义/无匹配只提示不自动建。
> 代码/围栏/转义/`![[...]]` 不解析；links 物化/反向链接/重命名联动（L2）仍后置。
> **Windows 真机（Web 管理/claim/App 粘贴）与真实 TLS relay 未验；S4d1c/S4d2 已实现
> （Linux headless + relay 测试），真实 V2 vault/TLS/Windows 仍待验。**

- **脚手架已就位**：Cargo workspace + `crates/app`（空窗口），依赖基线**已编译验证通过**。
- **版本管理**：当前开发分支 **`dev`**（2026-11-16 历史整理：`v0.4-sync` 后 12 个语义提交；
  整理前 97 个提交保留在 `backup/dev-before-cleanup-20261116`）；
  旧历史保留：**`backup/main-wip-20261110`**（首个基线）、**`backup/dev-wip-20261111`**（重命名/搜索 wip）、
  **`backup/dev-wip-20261111b`**（S5 两次 wip 前）。
  tag：`v0.1-slice6` = 切片 6；**`v0.2-gpui-kit`** = S2 + 引用块 + 导出 + GPUI Kit；
  **`v0.3-search`** = S4/S5 检索（P0 物化修复、单行多行摘要、大小写/整词开关、进度条、分组折叠；真机验收）。
  **2026-11-13 历史整理**：新 `dev` 为 12 个语义提交（tree 与整理前 tip `3c69f16`（backup）逐字节一致）；
  旧 185 个提交完整保留在 **`backup/dev-20261113`**；`v0.4-sync` tag 未改动。详见
  [`docs/notes/history-rewrite-20261113.md`](docs/notes/history-rewrite-20261113.md)。
  更早一次重建（2026-11-11）遗留的哈希（如 `7d95610`/`6c0c302`）见 `backup/*-wip` 分支。
  `target/`（20G）与本地 vault 不入库；**Lead review 一律看 `git diff`**，提交前必须三绿。
- 已落盘：[`AGENTS.md`](AGENTS.md)、[`docs/briefs/0000-kickoff.md`](docs/briefs/0000-kickoff.md)、
  [`0001-storage-format.md`](docs/briefs/0001-storage-format.md)（**已作废**）、
  [`0002-editor-core.md`](docs/briefs/0002-editor-core.md)（含附录 A–E 实测事实）、
  [`0003-storage-sync-consolidated.md`](docs/briefs/0003-storage-sync-consolidated.md)、
  [`docs/adr/0001-storage-format.md`](docs/adr/0001-storage-format.md)、
  [`docs/adr/0002-editor-core.md`](docs/adr/0002-editor-core.md)、
  [`docs/adr/0003-module-boundaries.md`](docs/adr/0003-module-boundaries.md)。
- **已验证（2026-11-09）**：`gpui-component` **0.9.0** 依赖 **`gpui-pre` 0.3.11 + `gpui-base` 0.9.0**（longbridge 血缘），
  与本基线 `gpui-unofficial` 1.27 **不同源**，`Element` / `Entity` 类型不可互换 → **不可混用**。
  探测方式：一次性临时包（自带 `[workspace]`）+ `cargo tree`，只验不提交，已完成并删除。
- **下一步（按 [ADR 0003](docs/adr/0003-module-boundaries.md) 的 6 个切片）**：
  ① ~~切片 1「text 收口」~~ **已完成并验收**（2026-11-09）：`crates/text` 是唯一实现（四个坐标 + `SourceRange` + 唯一 `CoordError`），`storage → text`，无重复自由函数；
  ② **协议已冻结**：`crates/storage/src/session/protocol.rs`（ADR 0002 §9 的消息集，`pub mod session`）；
  ~~切片 2（storage 会话核）~~、~~切片 3（editor 纯模型）~~、~~切片 4a（自动 flush 调度 + B 物化驱动）~~ **均已完成并验收**；
  ③ ~~切片 4（无头端到端）~~ **已完成并验收** → **S0 通过**；
      ~~切片 5a（app 单 Element，首次 GPUI）~~ **已完成并验收**（identity 渲染 / 命中 / 选区 / 滚动，截图已核）；
      ~~切片 6a（storage composition 闸门）~~、~~6b 支撑（driver undo/redo 转发）~~、~~切片 6b（IME 输入闭环，自动化部分）~~、~~切片 6c（grapheme 删除 API）~~、~~切片 6d（app 退格/Delete 接线）~~ **均已完成并验收**；
      真机发现并修复：空行上折叠光标被误画成选区高亮（`6c0c302`）；软折行选区整体右移 14px（`4fbd5cc`，根因是 `PAD_X` 在几何与绘制两处各加一次）。
      **I-beam 的教训（三次尝试，`7d95610` 定稿）**：① 在 paint 里采样 `mouse_position()` → 无重绘就漏判；② 自建根容器 `on_mouse_move` 状态机 → 整体失效。两者都是"没先查框架"。
      **正确做法是 gpui 原生机制**：`prepaint` 里 `insert_hitbox`，`paint` 里 `set_cursor_style(IBeam, &hitbox)`，由框架在命中链变化时调 `reset_cursor_style` 自动切换。
      **以后遇到"悬停/交互态"这类需求，先查框架有没有一等公民 API（hitbox、hover、cursor 都属于此类），再考虑自建。**
      **IME 真机验收通过**（2026-11-10，用户确认）：微软拼音打字、候选窗跟随光标、`Esc` 取消、空格上屏、`Ctrl+Z` / `Ctrl+Shift+Z` 均正常 → **切片 6 关闭**（task-14 complete）。
      **已知缺口**：ADR 0001 §3.2 的「克隆备份必须换新 `peer_id`」尚未验证（`peer_id` 不入库，属设备身份/同步切片）。
      **多进程并发写同一库**：当前生产只有一个 Vault / 一条写连接，不会自争锁；但 `synchronous=FULL` + 慢盘下，
      第二个 Vault 并发写可能耗尽 5s `busy_timeout`（写者饥饿，`13111a8` 诊断）。将来做备份/迁移进程并发写时，
      需在 Vault 写入路径加**有界** BUSY 重试，并接受"错误延迟变长、可能掩盖持续冲突"的代价。
- **S2 源码投影编辑器已完成（2026-11-10，真机验收）**：Markdown 语义扫描 → `ProjectionMap` 真实省略 →
  `EditorController` 输出 display 文本/双向映射 → app 按投影渲染（隐藏标记、caret 进入显现、命中映射）→
  语义样式（粗体/斜体/行内代码/删除线/链接）→ 块级度量（h1–h6 字号、围栏代码等宽、逐行行高）。
  **编辑与移动都走 display 语义**：退格/Delete/方向键先取可见 grapheme，再映射成最小 source 区间——否则会删到
  隐藏的 `**` 或把 caret 落进标记中间。**允许删除"已显现的"标记**（此时它是可见文本，等同直接改源码）。
  引用块缩进/左侧竖条已完成（`6cc6fa4`：`text_left_for_row` 收口，绘制/命中共用一套左边界）。
  已知未做：其它块级样式（列表项符号需要 Replace/Object 段）、`***`/同种定界符嵌套、跨行行内。
- **S3 导出已完成（2026-11-10）**：整库单向导出 Markdown 到 `<vault>/export/<时间戳>/`，文件名清洗（Windows 非法字符 / 尾空格点 / 空标题 / 大小写不敏感去重）
  与目录结构镜像；导出取 `BodyDoc::text()`（源文）**不是** UI 的 display 文本；后台执行，2000 篇 1126ms，导出期间 UI 可交互。
- **S4 检索已完成（2026-11-11，真机验收）**：`crates/search`（Tantivy 0.27 + cang-jie `ForSearch`，
  analyzer = 去空白 token + LowerCaser）+ `IndexManager`（代目录 + meta 指针发布、启动对账、
  查询前新快照做墓碑终检并扩候选、事件增量 + debounce）；app 侧 Ctrl+K 浮层、**自绘 IME 输入**
  （`search_input.rs`；`gpui-component` 的 Input 无 `EntityInputHandler`，文本输入必须自绘）、
  snippet 高亮、↑/↓/Enter/点击跳转、点空白关闭、Ctrl+A 全选（Ctrl+C/X/V 明确 v1 不做）。
  **教训**：`WM_CHAR` 注入绕过 IME，自动化能打字 ≠ 输入法可用；文本输入控件必须真机验。
- **S5 检索体验（2026-11-11，真机验收）**：总计/单行→多行摘要（命中行全量、单行 160B 截断 + 省略号）、
  `Aa` 大小写敏感与 `整词`（关=子串，trigram 召回 + 原文校验）开关、两阶段重建进度条、按笔记分组折叠（默认 3 行）。
  索引 schema v2：**索引体积约 2.5×**；查询 p95 全量多行摘要后 **27.8–39.9ms**（仍 <50ms 门槛）。
  **批量物化（S6）**：逐篇物化瓶颈是"每篇一个事务"（占 97.9%），改 `reconcile_batched`（200/批）后
  2000 篇物化 46.8s→0.38s、进程启动→索引就绪 **44.2s→6.2s**；首建剩余主成本是建索引（1.7–2.4s）。
- **同步/加密（S7）进行中（2026-11-11，ADR 0006）**：`crates/sync-proto`（MNE1 信封/CDE/错误码/limits）→
  `crates/sync`（根/KEK/槽/DPAPI/HPKE/设备证书）→ `crates/relay`（axum+SQLite 密文中继）→ storage 桥 →
  bootstrap/GC → 轮换 → Windows 两设备验收。v1 必做含两 checkpoint GC、恢复码、独立管理权；附件只做最小分片传输，UI 后置。
- **UI 框架已切 GPUI Kit（2026-11-10，ADR 0004，真机验收）**：`gpui = { package = "gpui-kit", version = "=0.10.0" }`（精确 pin），
  平台入口 `gpui::platform::application()`；`text`/`editor`/`storage` **零改动**。
  探针结论：0 签名变化、0 trait 不匹配、仅 1 处平台入口；迁移后三绿 329 用例 + UI smoke 10/10 + 用户真机 IME/DPI 通过。
  迁移/探针 worktree 与临时分支已清理；**升级必须整套对齐**，见 §1 依赖基线。
- **应用级主题（ADR 0021 P1/P2，2026-11-18，`5fc84c5`）**：
  `crates/app/src/theme.rs` 定义 `ThemeMode { Light, Dark, System }` / `ThemeId` / 91 个语义 token；
  内置浅色**逐色等于**迁移前硬编码（零视觉回归），新增深色；全局 `ThemeState`（`RwLock`）+
  `current_theme()` / `theme_id()` / `set_mode()` / `apply_system_appearance()`；
  `editor::theme::*` 的 81 个取色函数保留为兼容包装（读当前主题，600+ 调用点零改动）。
  `ThemeMode::System` 用 `observe_window_appearance` 跟随 OS（`AppShell` / `AppRoot` 各订阅一次），
  启动无窗口回退浅色；**默认模式 = System**。本机偏好 `theme.json`（`MENGHUNOTE_THEME_PATH` /
  `%LOCALAPPDATA%\menghunote` / `$XDG_CONFIG_HOME/menghunote`），只存 `mode`（+ 可选 `theme_id`），
  原子写、损坏/未知回退 System、不阻塞启动、不同步/不写正文。
  设置页「外观」（浅色/深色/跟随系统）在 AppRoot 设置页（标题下）与 AppShell 设置页底部；
  切换立即生效、不重建编辑器、不丢选区/滚动/撤销栈、不产生 `EditRequest`。
  Mermaid 缓存 key 的 `PREVIEW_THEME` 改为真实 `theme_id`（切主题重渲染一次）；图谱/表格/任务框/
  图片遮罩/工具条/状态栏/侧栏全部从 token 取色；4 处散落遮罩 `rgba` 已收口。
  测试：app 默认 **363** / `--features dev-sync` **608**；新增 `headless_theme`（4）与
  `headless_theme_ui`（1，dev-sync）+ theme 单测 7；fmt/clippy 干净。
  已知限制：自定义主题包（P5）后置；Mermaid SVG 内容暗色配色属 M3；真实 OS 外观切换/像素/DPI
  仍需 Windows 真机验；默认 demo 构建（无 `dev-sync`）没有设置页（仍默认跟随系统）。
- **i18n 基础设施 + 首批迁移（ADR 0020 P1，2026-11-18，`7ef5040`）**：
  新增纯 crate `crates/i18n`（`menghunote-i18n`，零外部依赖、不依赖 GPUI）：
  `Locale { ZhCn, En }`（`LC_ALL`→`LANG` 解析；未知/`C`/缺失回退中文，Windows 无
  `LANG` 等价默认 `zh-CN`）+ `LocaleMode { ZhCn, En, System }`；消息 id 常量
  （`settings.*`/`sidebar.*`/`editor.menu.*`/`status.*`，宏生成 `ALL`）+ 静态
  中英 catalog；`t(id)` 回退链 **当前语言 → 英文 → 中文 → key 本身**、
  `t_with` `{name}` 插值、`t_count` 英文 `one/other` 两 key 复数（中文同形）。
  **不引入 fluent 等重依赖**（最终格式选型留到插件 API 冻结前，ADR 0020 §8/§9）。
  本机偏好 `language.json`（`MENGHUNOTE_LANGUAGE_PATH` / 平台配置目录），只存
  locale（`system`/`zh-CN`/`en`），原子写、损坏回退 `System`、不同步/不写正文；
  `main.rs` 启动调 `language_settings::init_from_disk()`。
  设置页「语言 / Language」（中文 / English / 跟随系统）与「外观」并排（同一设置页），
  切换立即生效、只重绘，不重建编辑器、不丢选区/滚动/撤销栈、不产生 `EditRequest`。
  首批迁移：设置页固定文案（外观/语言/同步区标题、按钮、错误/提示）、侧栏
  （笔记/新建/文件夹/导出/图谱/设置/反向链接标题与空态）、编辑器右键菜单
  （插入/表格/打开链接/复制链接/复制 Markdown）、状态栏固定提示（反向链接/同步）；
  动态数据/路径/错误原文不翻译。详细覆盖与未迁移清单见 ADR 0020 §9.4/§9.5。
  测试：i18n **16**；app 默认 **374** / `--features dev-sync` **620**（新增
  `headless_language` 3、`headless_language_ui` 1、`language_settings`/`ui_text`
  单测 8）；fmt/clippy 干净。已知限制：不订阅 OS 语言切换（需重启/重选跟随系统）；
  未迁移页面在英文界面仍显示中文。
- **插件契约 + 第一方模块注册表（ADR 0020 P2，2026-11-18）**：
  新增纯 crate `crates/plugin-api`（`menghunote-plugin-api`；运行时仅 `serde` +
  纯 `menghunote-i18n`，零 GPUI/storage/sync）：`PluginId`（小写字母/数字/`.`/`-`，
  1–64，首尾非分隔符）、`PluginVersion`（`major.minor.patch`）、`ApiVersion`（`u16`，
  当前只支持 `1`）、`PluginManifest`（id/name/version/api_version/locales/
  capabilities/commands/panels/settings）与 collect-all `validate`；8 个 capability
  （snake_case；未知能力 serde 拒绝）；`HostServices` + notes read/write、settings、
  notifications 最小 object-safe trait（host 自有类型，不暴露 SQLite/密钥/GPUI）。
  **不实现加载器/运行时/安装/命令执行/授权 UI/签名**；manifest 校验失败必须
  拒绝加载（未来加载器的契约，本切片无加载路径）。
  `crates/app/src/modules.rs` 只读注册表固定顺序 `Sync → Mermaid → ImportExport →
  Graph`；Sync `compiled` 仅 `dev-sync`（**P1 后改为跟随 `sync-ui`，见下条**），Mermaid/
  ImportExport/Graph 总是编入且无 feature gate；`enabled` 本切片恒等于 `compiled`，
  无运行时开关/设置页 UI，未新增 Cargo feature 裁剪。
  测试：plugin-api **24**（17 单测 + 7 JSON 集成）；app 默认 **384** /
  `--features dev-sync` **630**（各 +4 `modules` 单测；P2 前 380/626）；
  fmt/clippy 干净。已知限制：manifest/API schema 未冻结；`locales` 跟随 i18n
  当前 `zh-CN`/`en`；Network/Filesystem/Clipboard/Commands 只声明无接口。

- **同步产品化 P1（2026-11-18，`sync-ui` 拆出并设为默认）**：
  `crates/app/Cargo.toml` 新增 `sync-ui = ["sync", "dep:serde", "dep:serde_json",
  "dep:zeroize"]`，`dev-sync = ["sync-ui"]`，`default = ["sync-ui"]`；
  `onboarding`/`dev_sync` 模块从 `dev-sync` 改为 `sync-ui` 门禁，`main.rs` 默认
  进入 `AppRoot`（产品 Welcome/Create/Join），显式 `--dev-sync*` 仍只在 `dev-sync`。
  `local_dev` 许可、dev CLI/setup、localhost relay 门禁、V2 管理区仍保留在 `dev-sync`；
  默认构建 `mode_allowed(LocalDev)=false`，创建链接/提交创建/邀请兑换三处 UI 明确拒绝。
  测试：app 默认 **497** / `--features dev-sync` **655** / `--no-default-features` **406**
  （旧默认不变）；新增 `tests/sync_ui_product.rs` 3 个 headless；fmt/clippy 干净。
  数据目录/迁移与 UI 去 DEV 标注**已由 P2 完成**（见下条）；TLS/DPAPI 真机仍后置；
  详见 [`docs/notes/sync-product-p1.md`](docs/notes/sync-product-p1.md)。
- **模板 / 日记：两个第一方核心插件（2026-11-18）**：
  **模板就是普通笔记**——不建独立表、不隐藏文件夹、不做保留名、不做搜索/图谱过滤；
  模板文件夹是可配置的普通目录（默认 `模板`，支持多级），其子树下任意笔记都是模板；
  日记模板可以是任意普通笔记（精确路径，不要求和模板文件夹一致）。
  占位符：`{{date}}`/`{{time}}`/`{{title}}` + `{{date:FORMAT}}`/`{{time:FORMAT}}`
  （token `YYYY YY MM M DD D HH H mm m ss s`，本地时间，最长匹配；未知占位符原样保留，
  不 panic）；不做日期偏移/脚本/正则。日记模板复用同一引擎，默认格式取模板插件设置。
  入口与行为：编辑器右键「插入 → 模板…」→ 覆盖层选择器（自绘 IME 搜索框、输入过滤、
  ↑/↓、Enter、点击、Esc），选中后把渲染后的正文插入 caret（**有选区则替换**，一次
  `EditRequest`/一个撤销组，走现有 `insert_text`；模板笔记不移动、不复制资产）；
  侧栏第二动作行「从模板新建…」（默认模板标题、**只在根级去重**、不覆盖、
  创建后打开）与「今日日记」：按 `folder + date_format` 计算路径（`/` 生成子目录，
  标题 = 最后一段），已存在直接打开，不存在时**一条结构提交**创建缺少的目录+笔记，
  模板丢失非阻塞提示 + 空日记；插件关闭时入口整行隐藏。
  设置：只写本机 `templates.json` / `daily-notes.json`（原子写、损坏回退、不同步、
  不写正文）；AppRoot 设置页卡片**末尾**追加「模板」「日记」两区（不挪动既有同步/
  实例/relay 控件位置；`settings-templates*` / `settings-daily-notes*` 固定 selector；
  自绘 IME 字段 Enter 保存、Esc 放弃；显示日期/时间示例），AppShell 的 dev-sync 设置页
  同样追加。新增 `templates.*` / `daily_notes.*` 中英文案（渲染/测试统一走 `ui_text`）。
  不改 sync/relay/storage/schema/wire；`chrono = 0.4`（`clock`）显式加入 app，lock 里仍是
  **0.4.45**（只多一条 `menghunote-app → chrono` 依赖边）。
  测试：app 默认 **497** / `--features dev-sync` **655** / `--no-default-features` **406**；
  i18n **16**；
  新增 `headless_templates` 7、`headless_notes_plugins_ui` 2（`sync-ui` 产品构建）、
  `templates` 单测 7、`daily_notes` 单测 7、`ui_text` 1；fmt/clippy 干净。
  已知限制：路径精确/大小写敏感；无相对日期/日历/
  周期笔记；设置字段不做失焦自动保存；设置不参与多设备同步。详见
  [`docs/notes/templates-daily-notes.md`](docs/notes/templates-daily-notes.md)。

- **笔记属性 / YAML frontmatter（2026-11-19）**：
  笔记第 0 行输入完 `---` 就**立即**补成 `---\n\n---\n`（不用 Enter）；「笔记属性」
  面板作为正文顶部块（随正文滚动，不固定在窗口上方；可折叠、可一键清除全部属性；
  key/value 行内编辑、添加、删除），源码 frontmatter 永远隐藏，初始 caret 落在正文起点；
  正文起点 Backspace 不会删到隐藏的 `---`。正文里的 `---` 投影成空行 + 自绘分隔线。
  每次提交一条整篇替换（一个撤销组），写回后 caret 回正文起点。
  纯解析/渲染在 `crates/editor/src/model/frontmatter.rs`；面板在
  `crates/app/src/note_properties.rs`；文案 `properties.*`。
  测试：editor **292** / app 默认 **577** / `--features dev-sync` **728** /
  `--no-default-features` **455**；新增 `headless_excalidraw` 9 个属性/分隔线用例。
  已知限制：属性很多时面板整体下推正文；只支持常用 YAML 子集；编辑会重写整段
  frontmatter（注释/空行不保留）；无类型选择器/自动补全。详见
  [`docs/notes/note-properties.md`](docs/notes/note-properties.md)。

- **笔记顶部标题 / 路径 / 前进后退（2026-11-19）**：
  笔记主体顶部固定 header：`← →` 历史按钮 + `文件夹 / 子文件夹 / 标题` 路径面包屑 +
  可编辑大标题；点标题进入 `RenameInput::new_note_title`，提交走与侧栏 F2 相同的
  `commit_rename_node`，侧栏/文件名同步。历史只存 `node_id`（上限 100、forward 分支丢弃），
  鼠标侧键 `MouseButton::Navigate(Back/Forward)` 在根视图接收，整个窗口生效。
  selector：`note-header` / `note-path` / `note-title` / `note-title-edit` /
  `note-nav-back` / `note-nav-forward`。
  测试：app 默认 **577** / `--features dev-sync` **728** / `--no-default-features` **455**；
  新增 `headless_excalidraw` 2 个 header/history 用例（该计数是当时快照，最新见 0024/0025 条目）。
  已知限制：header 固定不随正文滚动；面包屑只读；历史不跨重启；无多标签页。详见
  [`docs/notes/note-header-navigation.md`](docs/notes/note-header-navigation.md)。

- **笔记本地编辑历史（ADR 0024 H1，2026-11-19）**：
  笔记右键「历史记录…」→ 本机时间线覆盖层（按时间倒序，来源/标题，只读预览）；
  「恢复为新笔记」创建根级 `原标题（历史）` 并整篇写正文，不覆盖当前、不改 CRDT 历史。
  捕获：`AppShell` 2s 周期 timer（源文变化才写，首次打开只设基线）+ 切笔记/关窗 flush；
  storage 用 v1 `versions` 表 + migration v4（`source`/`device_id`/`body_hash`），
  `body_zstd` 存 zstd(UTF-8 Markdown)，`asset_hashes` 沿用 asset_gc 口径，
  每篇保留最近 200 个版本/90 天（同事务裁剪），只追加、不查 relay。
  selector：`note-history` / `note-history-panel` / `note-history-row-<N>` /
  `note-history-preview` / `note-history-restore` / `note-history-close` /
  `note-history-empty`；菜单为笔记第 2 项（`menu-item-1`，可读 `menu-item-history`）。
  测试：storage **206** / app 默认 **592** / dev-sync **743** / no-default **470**；
  新增 `storage/tests/history.rs` 5、`headless_note_history.rs` 6。
  已知限制：2s 轮询不是严格 debounce；无 diff/分页/搜索/跨设备；恢复只复制正文，
  资产字节不复制。详见 [`docs/notes/note-history.md`](docs/notes/note-history.md)。

- **Excalidraw 嵌入预览（ADR 0025 后续切片，2026-11-19）**：
  普通 Markdown 整行 `![[目标.excalidraw]]`（支持 `|别名`）解析为 `ObjectKind::Drawing`，
  投影成一个图片式 Object（隐藏源码）；`ImageService` 用合成 key
  `drawing://<theme>/<target>` 请求，宿主 `vault_asset_provider` 在后台线程解析目标
  （路径唯一优先/标题唯一兜底）、读正文、`scene_json_to_png` 导出 PNG 再解码；
  缺失/失败显示「画布笔记不存在 / 画布预览加载中… / 画布预览失败」占位。
  单击预览发 `EditorEvent::WikiLinkRequested` 打开目标画布笔记（不移动 caret/不进源码）；
  无悬浮工具条；混排行内、代码/引用/列表/缩进里的 `![[...]]` 不解析。
  解析在 `crates/markdown`（`DrawingSpan`），模型在
  `crates/editor/src/model/markdown.rs`（`MarkdownDrawing`），绘制在
  `crates/app/src/editor/view.rs`，PNG helper 在 `crates/app/src/drawing.rs`。
  测试：markdown **53** / editor **299** / app 默认 **592** / dev-sync **743**；
  新增 `headless_excalidraw_embed` 2、`headless_excalidraw` 2（真实 provider 渲染 + 点击打开）。
  已知限制：预览缓存 key 只含 theme+target（目标画布在别处改后同视图可能看到旧图）；
  无 DPI 重新渲染；真实 Windows 像素/DPI 待验。详见
  [`docs/notes/excalidraw-embed.md`](docs/notes/excalidraw-embed.md)。

- **vault 新建 / 切换 / 路径选择（2026-11-20）**：
  默认 vault 不再写 `%LOCALAPPDATA%`：Create/Join 实例目录默认
  `%USERPROFILE%\Documents\menghunote\instances\<id>`（Linux/macOS
  `$HOME/Documents/menghunote/instances/<id>`；Documents 不存在则
  `$HOME/menghunote/instances/<id>`），本地库默认同根 `local/vault.sqlite3`；
  两个表单加「浏览…」（`cx.prompt_for_paths`，目录选择），仍要求绝对路径 +
  不存在/空目录（`validate_instance_directory` 不变）；`sync.json` 与旧 `vault_path`
  原样保留、不迁移。
  设置 →「实例」每行新增「打开」：`AppShell::close_all`（有界排空/flush）→ 释放旧
  `InstanceLock` → 关设置 → 打开目标实例（成功写 active + `startup.json` mode=sync；
  失败进既有 Error 页；目标就是当前实例 = no-op）；新增「返回欢迎页」
  「新建同步 vault…」「加入同步 vault…」，均先关旧 Main 再回 Welcome / 进 Create /
  Join 表单，**不热切当前 worker**。
  Welcome 本地入口可编辑库文件路径（`app-root-local-path` + 浏览）；文件不存在时按钮明确
  「新建本地 vault」（`open_local_vault_at` 建空库、不播种 demo），成功后把实际路径写进
  `startup.json` 的可选 `local_vault_path`（旧 `{version,mode}` 仍可读；旧格式 + 旧数据根
  `local/vault.sqlite3` 仍存在时继续打开旧库，不"丢库"）。
  selector：`settings-instance-<i>-open` / `settings-back-welcome` /
  `settings-new-sync-vault` / `settings-join-sync-vault` / `app-root-dir-browse` /
  `app-root-join-dir-browse` / `app-root-local-path` / `app-root-local-browse`。
  实现：`onboarding/{app_root,layout}.rs`；新增 `headless_vault_switch` 9（产品默认）/
  7（dev-sync；本地 2 用例仅产品构建）；layout 单测 +5。
  测试：app 默认 **652** / `--features dev-sync` **799** / `--no-default-features` **501**；
  `--lib onboarding` 计数见各分类；fmt/clippy（dev-sync all-targets `-D warnings`）干净。
  已知限制：headless 不弹真实系统对话框（浏览按钮真机未验）；切换是同步关+开，大库首帧
  可能短暂停顿；`close_all` 只请求停止同步 worker（不 join，与关窗一致）；不迁移旧
  LOCALAPPDATA 下的 vault。详见
  [`docs/notes/vault-management.md`](docs/notes/vault-management.md)。

- **同步 UI 修复与 Windows release 控制台（2026-11-20）**：
  - remote_tls 不再要求「我确认要连接远程服务器（HTTPS）」勾选：Create/Join/设置全部
    去掉 `remote_consent` 字段、selector 与提交校验；仍由权威 parser 强制 HTTPS
    canonical origin 与独立 pin 校验。
  - DEV 勾选/`DEV 状态`提示/「同意开发同步」错误只存在于 `dev-sync` 构建；产品
    Create/Join 不渲染、不要求，产品不再出现 DEV ONLY 文案。
  - 邀请 PENDING 等待页每 **5s** 自动调用 `check_invite`（`busy` 防重叠；失败保留页面），
    手动「检查状态」保留；事件泵在等待页保持存活，离开后自清 `pump_started`。
  - 底栏 `sync-status` 在 AppRoot 托管的 Main 里发
    `AppShellEvent::OpenSyncSettings`，由 AppRoot 打开设置并选中 `SettingsSection::Sync`；
    不再打开 AppShell 旧组合设置页（同步下方不再混入外观/语言/模板）。direct AppShell
    测试仍保留旧 `settings_host_managed = false` 路径。
  - `main.rs` 加 `#![cfg_attr(all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows")]`：Windows release 不再弹终端窗口；debug 构建保留
    控制台日志。
  新增/重写测试：`onboarding_invite` 自动检查、`sync_ui_product` 产品无 DEV 勾选、
  `headless_vault_switch` 状态栏打开 AppRoot 同步设置、`dev_sync_ui` host-managed 事件。
  **relay 模式不再手选**（2026-11-20 追加）：设置/Join 去掉 `settings-mode-*` /
  `app-root-join-mode-*` 单选，`infer_relay_mode` 按地址自动识别（https → remote_tls，
  http → local_dev 仅 dev-sync），保存/提交仍走同一权威 parser；设置→实例的长文案
  拆成三行短说明并改为垂直 `settings-instances` 容器，去掉带连续空格的漂浮排版。
  测试：app 默认 **652** / `--features dev-sync` **799** / no-default **501**；fmt/clippy 干净。
  已知限制：TLS/Windows 真机仍未验；direct AppShell 旧设置页仅测试兼容。

- **vault 切换器 / 标题输入 / 编辑器交互 / 快捷键 / 画布入口（2026-11-20）**：
  - 左下角 `app-root-vault-switcher`：当前 vault 名 + `▾`，菜单列实例（当前 ✓）、
    `管理仓库…`；管理仓库覆盖层 `app-root-vault-manager` 提供打开/设为默认/新建同步/
    加入同步/返回欢迎页。设置里的「实例」分类已移除；切换仍是 close_all → 释放锁 →
    开目标，不重启。
  - 笔记顶部标题：`Ctrl/Cmd+V` 粘贴（单行过滤）、点击按位置落 caret、新建普通笔记
    自动进入标题编辑并全选默认名；点正文仍还给正文焦点。
  - 编辑器：Shift+单击扩选；单击改为 mouse-up 落 caret（避免按下就闪语法/蓝选）；
    `Ctrl+B` / `Ctrl+I` 包裹/取消/折叠插入成对标记，一次 `EditRequest`/撤销组。
  - 设置 →「快捷键」：7 个命令可改绑、冲突红标/汇总、单行恢复默认、立即生效；
    `shortcuts.json` 原子写；命令默认值与接线见 `docs/notes/shortcuts.md`。
  - 画布从侧栏顶部动作行移到最左 ribbon（`ribbon-canvas`），图标用
    Excalidraw `logoMark.svg`；`sidebar-new-drawing` selector 保留在 ribbon 按钮上。
  - IME 回归：`headless_ime_preedit` 用 F8 真实走 `update_preedit` → `paint_preedit`，
    断言 `preedit_paint_count > 0`；paint 增加 selection 起点兜底，防止组合串静默不画。
  - relay 部署与“默认只发 app、relay 改动需显式提醒”记入
    [`docs/adr/0026-relay-deployment-and-client-releases.md`](docs/adr/0026-relay-deployment-and-client-releases.md)。
  - 标题编辑保持 **28px BOLD / 48px 行高**（编辑态不再上移）、统一 `cursor_text()` 文本光标、
    双击按词选（`word_range_at`，CJK+ASCII 连续词、空白/标点/emoji 只选自身）；About 文案
    改为 **`GPUI Kit 0.10.0`**（中英 catalog 同步）。
  - 侧栏拖放落点改为目标行**最近的祖先文件夹**（文件夹行 = 自身、子笔记/子文件夹行 = 父文件夹、
    根级笔记行 = 根）：文件夹展开后的整个目录范围都能拖入，嵌套时落到最近一层文件夹；
    自指/后代仍拒绝，一次合法移动只写一次结构。
  - 正文 IME preedit 的 caret 改画在**组合串末尾**（不再同时画在起点）；正文 caret 约 **530 ms**
    闪烁（复用 50 ms pump 推进，输入/点击/聚焦重置，组合期间常亮）。
  - 标题 / 重命名输入框 caret 同样约 **530 ms** 闪烁（独立 timer；输入/点击/移动重置，
    `caret_blink_visible` 供 headless 断言）。
  - Main 左下角改为**一行**动作（`app-root-bottom-actions`）：vault 切换器占剩余宽度 +
    紧凑「设置」按钮，两按钮同一底边/行高；`app-root-settings` / `app-root-vault-switcher`
    selector 不变，窄窗口宽度夹紧。
  测试：app 默认 **740** / `--features dev-sync` **891** / no-default **575**；storage **274** / sync **634**；本轮硬化见 [`docs/notes/sync-hardening-20261121.md`](docs/notes/sync-hardening-20261121.md)。
  后续同批追加：
  - Welcome 重做 Obsidian 式仓库选择页：`welcome-vault-list` 左栏列同步实例+本地库并直接打开；
    右栏 `welcome-new-local` / `welcome-open-local` / `welcome-sync-remote`；同步远程只用一条
    `mn-pair://create|join` 链接自动识别，不再手填 relay/vault/恢复码。管理仓库只保留一个统一入口。
  - 新建文件夹立即进入行内重命名；点其它行/正文/空白确认。侧栏笔记/文件夹可用原生
    `on_drag`/`on_drop` 拖入文件夹或根级，一次 `write_structure("移动节点")`，拒绝自指/后代。
  - 标题栏支持鼠标拖动正/反向选字；正文 caret 只在正文聚焦时绘制，避免标题+正文双光标。
  - 编辑器补 `Ctrl+←/→` 按词移动、`Ctrl+Shift+←/→` 按词扩选、`Ctrl+L` 选当前行、
    `Ctrl+Home/End` 文件首尾、`Ctrl+Shift+Home/End` 扩到首尾、`Esc` 折叠选区。
  - IME 根因：`shape_line` 空 runs 零字形；`text_runs_for_line` 空 spans 生成覆盖全文的
    Plain run，`headless_ime_preedit` 回归。详见
    [`docs/notes/welcome-vault-picker.md`](docs/notes/welcome-vault-picker.md)、
    [`docs/notes/editor-selection-and-keys.md`](docs/notes/editor-selection-and-keys.md)。
  - **仓库自定义命名**：`InstanceEntry.display_name: Option<String>`（serde default，配置仍 v2，
    旧配置缺字段回退目录名）；Welcome 同步表单 `welcome-sync-name`，管理仓库
    `vault-manager-rename-<i>` 行内改名，空白=清除回退；不改目录/`vault_path`/身份。
  - **回收站**：ribbon `ribbon-trash` + `trash-panel` 列表/恢复；复用
    `StructureWriter::restore_note`，回原父级、不自动打开；彻底删除因多设备语义未定**未做**。
    详见 [`docs/notes/trash.md`](docs/notes/trash.md)。
  - **代码块语法高亮/灰底/复制**：`pulldown-cmark` 暴露 `CodeBlockSpan`，`syntect`（纯 Rust）
    输出 token 颜色；只经 `glyph::shape_row_code` 改颜色，不改布局/编辑/选区；
    10 个主题 token、块级 + display 两级缓存、超 256 KiB 降级无色。代码块整块铺
    `code_bg` 灰色圆角底；光标/选区不在块内时右上角浮出 `code-copy-<i>` 复制按钮
    （只复制围栏内源码、不动 caret；组合/对象块不显示）。不引入 TextView/tree-sitter/C；
    详见 [`docs/notes/code-highlight.md`](docs/notes/code-highlight.md)。
  - **IME 组合串推移**：preedit 先插入 `composition_display()` 的临时布局文本再折行，
    后续文字被真实推移/折行（不再叠字）；`Controller` source/选区不变，跨行替换也进入临时布局；行度量沿用原 display 行，提交后修正。
    headless 断言布局文本、串尾 caret 与取消无残留。
  - **标题/单元格复制**：`Ctrl+C`/`Cmd+C` 复制选区、`Ctrl+X`/`Cmd+X` 剪切并删除选区
    （`text_shortcuts::copy_shortcut/cut_shortcut`）；搜索框/向导仍保持不支持剪贴板。
  - **同步 Phase 1A（已落地）**：批量 inbox 单事务 + `BEGIN IMMEDIATE` + skipped 窗口
    有界扫描；按文档聚合正文 integrate（`load_head` 预检、同 doc 多轮合并自愈物理乱序、
    选批防饥饿、整批一次 integrated 前缀）；`baseline_apply` 单次 snapshot 解码 +
    detached oplog 合并，5/20 MiB gate 从秒级降到 35/110 ms；remote-known VV 同事务
    落 meta 修复已有库 tombstone。Release 实测 5000 单文档 integrate 147s→275ms、
    20 篇 14.9s→0.76s。Phase 1B（checkpoint 快速追赶）**未实现**，N_min=25k /
    16 MiB 机会性上限 / 3 s 持闸见
    [`docs/notes/sync-fast-catchup-plan.md`](docs/notes/sync-fast-catchup-plan.md) §9；
    评审材料见 [`docs/notes/sync-review-packet-20261121.md`](docs/notes/sync-review-packet-20261121.md)；
单文件评审版见 [`docs/notes/sync-critical-modules-review.md`](docs/notes/sync-critical-modules-review.md)。
  已知限制：UI/拖拽/IME/标题/快捷键录制尚未 Windows 真机验收；TLS 已由用户验证但未做安全审计。

- **UI 重构：设置分类导航 / 最左侧图标栏 / 侧栏排序（2026-11-18，Obsidian 风格）**：
  **设置页（产品 AppRoot）** 从一张长卡片拆成「左侧分类导航 + 右侧只渲染当前分类」：
  `appearance`（默认）/ `language` / `templates` / `daily_notes` / `sync` / `instances` /
  `relay` / `security` / `about`；固定 selector `settings-nav` / `settings-nav-<section>` /
  `settings-section-<section>`，旧 `settings-theme-*` / `settings-language-*` /
  `settings-templates*` / `settings-daily-notes*` 在对应分类打开时仍存在；保存/关闭/错误/提示
  固定在底栏（任何分类可达）；分类只记本次会话（不持久化）；覆盖层限高 + 滚轮隔离保持，
  内容区独立滚动、左导航固定。`SettingsSection` + `render_settings` 的 section 分派在
  `onboarding/app_root.rs`；**AppShell 的 dev-sync 设置页不拆**（行为不变）。
  **最左侧图标栏（ribbon）**：侧栏左边 44px（`RIBBON_WIDTH`），只放快捷操作
  `ribbon-daily` / `ribbon-template` / `ribbon-graph` / `ribbon-search`，复用
  `open_daily_note` / `open_template_picker(NewNote)` / `open_graph` / `open_search`；
  **设置不在 ribbon**（仍在侧栏底部 `app-root-settings`，AppRoot 的绝对定位按 `RIBBON_WIDTH`
  右移）。原第二行文字按钮（「从模板新建…」「今日日记」）移除，旧 selector 作为兼容别名
  挂在外层包装（同一按钮）。图标是 `crates/app/assets/icons/*.svg`（lucide）+
  `include_bytes!` + `gpui::svg().data()` + 主题 token 着色，**不新增依赖边**（不引
  `gpui_component`），hover 背景/手形 + 自绘 tooltip。
  **侧栏排序**：`list_tree`（rank 顺序，同步/导出用）语义不变；新增
  `list_tree_sorted(vault, TreeSort)`：**每个层级目录永远在笔记前**，同组按
  `name_asc`（默认）/ `name_desc` / `updated_newest|oldest` / `created_newest|oldest`；
  `TreeEntry` 增加 `created_at` / `updated_at`（`notes` 表 join，目录为 `None`）；
  中文按码点、大小写不敏感 + 原串 + node_id 兜底。侧栏标题行「排序」按钮 `sidebar-sort`
  + 菜单 `sidebar-sort-menu` / `sidebar-sort-<key>`（当前项 ✓、点空白关闭）；切换只重建树，
  按 node_id 保住折叠/选中/打开笔记。本机 `sidebar.json`（`MENGHUNOTE_SIDEBAR_PATH` 可覆盖，
  原子写、损坏回退默认、不同步、不写正文；读盘收敛在产品根视图 `AppRoot::new`
  （`AppShell` 构造期不读盘，保证 headless 测试的默认顺序不随本机残留文件变化））。
  测试：`tree_view` +6（目录优先/六键/大小写中文/时间戳/父子连续/删除/键往返）、
  `headless_sidebar_sort` 8（ribbon 布局与动作复用、排序菜单、六键写盘、折叠/选中保持、
  偏好往返与损坏回退、启动读盘）、`onboarding_ui` +1（设置导航 9 分类/只渲染当前分类/
  旧 selector/内容区限高/会话记忆）；既有测试按新展示顺序更新（demo 树 0 目录 / 1 目录内
  笔记 / 2 根级笔记），窄窗口表格测试窗口 +44px（ribbon 宽度）。
  实测：app 默认 **505** / `--features dev-sync` **664** / storage **201** / i18n **16**；
  fmt/clippy 干净。设置页输入框只在**聚焦**时绘制选区（`set_text` 全选不再让未聚焦
  字段看起来像整页被选中）。详情见
  [`docs/notes/ui-refactor-settings-ribbon-sort.md`](docs/notes/ui-refactor-settings-ribbon-sort.md)。

- **设置页插件总开关 / 分界线 / 同步直显（2026-11-18，Obsidian 风格续）**：
  产品 `AppRoot` 设置页左栏改为「固定分类（外观/语言/同步/实例/Relay/安全/插件/关于）→
  `settings-nav-divider`（1px 主题线，不可点）→ 已启用的插件分类（模板/日记）」。
  新增固定分类 `plugins`（`settings-nav-plugins` / `settings-section-plugins`）：参考
  Obsidian「核心插件」页，只列**有运行时开关**的模板/日记（`settings-plugin-templates*` /
  `settings-plugin-daily_notes*`），开关直接读写 `templates.json` / `daily-notes.json` 的
  `enabled`（无第二份状态、不改 JSON 格式）；关闭后左栏分类立即消失，停在被关闭的插件
  分类时自动回退到「插件」页，ribbon 入口继续按同一份 `enabled` 隐藏。同步分类删除
  「展开/收起」状态与标题行，选中即渲染面板；`settings-toggle-sync` 保留为同步内容容器，
  内部 `dev-sync-*` selector 不变。`SettingsSection` 新增 `Plugins` 并区分
  `FIXED` / `PLUGIN_SECTIONS`（`ALL` 更新为 10 项）；新文案走 `ui_text` + `settings.plugins.*`
  中英文。测试：app 默认 **509** / `--features dev-sync` **668** / i18n **16**；
  新增 `headless_notes_plugins_ui` 2（插件页开关/写盘/重启读盘/回退）与
  `app_root` 分类分组单测 1（`FIXED`/`PLUGIN_SECTIONS`/`ALL` 关系）；插件行改为
  Obsidian 式「标题 + 说明 + 右侧胶囊开关」（不再有按钮文字/「已启用」重复行），
  开关轨道开=accent、关=灰，`-state` 仅作开关锚点。设置字段首次打开不再预选
  （`RenameInput::new_at_end`，光标在末尾；点击字段不再全选），避免「整页被选中」观感。
  fmt/clippy 干净。
  详情见 [`docs/notes/ui-refactor-settings-ribbon-sort.md`](docs/notes/ui-refactor-settings-ribbon-sort.md)。

- **同步产品化 P2 / A 组第一切片（2026-11-18，数据目录产品化 + 旧数据保守迁移 + 去 DEV 标注）**：
  **数据目录产品化**：默认同步目录 `menghunote-dev` → **`menghunote`**，公开配置
  `dev-sync.json` → **`sync.json`**（Windows `%LOCALAPPDATA%\menghunote\sync.json`、
  Linux `$XDG_CONFIG_HOME/menghunote/sync.json` + `$XDG_DATA_HOME/menghunote/instances/`）；
  `ProfileLayout::new` 显式路径语义不变，`env` 注入（HOME/XDG/LOCALAPPDATA）仍可用于测试；
  实例目录里 `vault.sqlite3` / `dev-identity.bin` **本轮不改名**。
  **旧数据保守迁移**：`AppRoot::boot` 调 `ProfileLayout::migrate_legacy_profile()`——
  新 `sync.json` 不存在、旧 `menghunote-dev/dev-sync.json` 存在时，把**同一份公开实例
  列表**原子写到新配置（v1 只在内存升 v2；新增 `ProfileStore::read_with_origin` /
  `read_only`，旧文件不回写）；`vault_path` 原样保留（**不移动/不复制** instances、
  身份、库），旧目录/旧配置**不删不改**；新配置存在 → 一律以新配置为准（幂等，重复启动
  逐字节不变）；旧配置损坏/版本不支持 → 不重建不覆盖，`AppRoot` 进 **GUI Error 页**
  （`旧同步配置无法迁移` + 旧路径 + 可重试），不再无提示退出/白屏；产品构建里的旧
  `local_dev` 因 `parse_for_build`/`mode_allowed` 不适用 → **只跳过迁移**、不写新配置、
  **不悄悄改** `remote_tls`，旧文件保留，产品正常进 Welcome。
  **去 DEV 标注（产品 `sync-ui` 构建）**：`AppRoot` 各页顶部 `DEV ONLY · 开发同步…`
  横幅 `banner_text` 改为 `Option<Div>`（产品 `None`，不挂节点；`dev-sync` 保留，
  selector `app-root-dev-banner`）；同步面板固定行（`dev_sync/panel.rs::header_line`）
  产品构建改中性「同步面板」（新 key `settings.sync.panel_title`，selector
  `dev-sync-header`），`dev-sync` 保留 `DEV ONLY / 非生产 / localhost` 红字；Welcome
  旧「继续本地（DEV ONLY，无同步）」的 `allow_local_vault = cfg!(feature = "dev-sync")`
  在 A 组第二切片被反转：产品构建改显示「仅本地使用」（见下条）；`main.rs` 向导启动日志删掉
  `DEV ONLY / localhost`；动态 relay 地址（127.0.0.1 等）照实显示；删除无调用的
  `settings.sync.dev_title`，`settings.sync.dev_banner` 仍供 `dev-sync` 使用。
  `--dev-sync*` CLI / localhost setup / V2 管理 / 轮换实验入口继续只在 `dev-sync`；
  TLS/relay 协议/schema/wire/身份文件均未动。
  测试：app 默认 **518** / `--features dev-sync` **675** / `--no-default-features`
  **构建通过**（406 计数未变）/ i18n **16**；新增 layout 单测 5（显式布局不迁移、
  复制公开列表且库/身份不动、新配置优先、坏旧配置不重建、产品跳过旧 local_dev）+
  `dev-sync` 额外 1（v1 旧文件不回写）+ panel 单测 1；`sync_ui_product` 3 → **6**
  （启动迁移 + 坏旧配置 GUI Error 页 + 旧 local_dev 正常进 Welcome），
  `onboarding_ui` +1（dev 构建保留横幅/本地入口）。表单输入（向导/设置 relay）预填
  改为光标在末尾、不预选；鼠标点击折叠选区到末尾，避免「打开即全选、点击取消不了」。
  fmt/clippy 干净。已知限制：迁移只在 `AppRoot` 启动引导执行；并发首次启动的
  rename 覆盖窗口未加文件锁；身份/库文件改名仍后置；创建表单既有
  「开发同步」同意文案与 `remote_tls` 的「DEV 状态」提示不在本切片、未改。
  详见 [`docs/notes/sync-product-p2.md`](docs/notes/sync-product-p2.md)。

- **同步产品化 A 组第二切片：产品「仅本地使用（不同步）」入口（2026-11-18）**：
  产品默认构建（`sync-ui`，无 `dev-sync`）的 Welcome 新增 **`app-root-local`**
  「仅本地使用（不同步）」：不联网、不建同步实例、不写 `sync.json`/身份文件，
  一条本地笔记路径即刻可用。**文案按构建区分**（`AppRoot::local_entry_label()` 是渲染与
  测试唯一来源）：产品「仅本地使用（不同步）」，`dev-sync` 保留旧
  「继续本地（DEV ONLY，无同步）」（同一 selector；旧 dev 行为/演示树不变）。
  `AppRootOptions::discover` 两种构建都 `allow_local_vault = true`；
  `for_test`/显式构造仍默认关闭（既有 Welcome 测试契约不变），要测时显式打开。
  **本地库路径**：`ProfileLayout` 数据根（`instances/` 上一级）下
  **`local/vault.sqlite3`** ——Windows `%LOCALAPPDATA%\menghunote\local\vault.sqlite3`、
  Linux `$XDG_DATA_HOME/menghunote/local/vault.sqlite3`；新增
  `ProfileLayout::data_root/local_dir/local_vault_path` 与 `onboarding::LOCAL_DIR_NAME`，
  `dev_vault::open_local_vault_at` = `open_vault_at_with_peer_opts(path, 1, false)`
  （**不播种 demo**、已有库直接复用、目录不存在时创建、不覆盖）。
  **打开后**：`AppShell::new`（无同步面板/worker/自动同步）+ Main 顶部固定提示条
  `app-root-local-mode`「本地模式：不同步」；不出现 `sync-status`/`dev-sync-panel`/
  `app-root-dev-banner`；设置/侧栏/编辑器/模板/日记照常。打不开本地库 →
  `set_error` 进 GUI Error 页（`local.open_failed_title`，不静默退出、不覆盖坏库）。
  **数据安全**：不写 `sync.json`、不建 `instances/`、不写 `dev-identity.bin`、
  不取实例目录锁（不影响同步实例锁；本地模式本轮不加新锁）。
  i18n：新增 `local.*` 10 条消息 + 中英 catalog（`ui_text::local_*`），渲染无新中文字面量。
  测试：app 默认 **518 → 522** / `--features dev-sync` **675 → 677** / i18n **16**；
  新增产品 headless 2（点击入口→Main/库创建/无 sync.json/无身份/无 demo/再次启动复用；
  坏库→Error 页），`sync_ui_product` 旧「产品不得出现 app-root-local」断言反转为必现，
  `onboarding_ui` dev 入口文案断言、layout 单测 2、`ui_text` 中英单测 1；
  fmt/clippy 干净。已知限制：本地库无新增进程锁（并发打开依赖 SQLite `busy_timeout`）；
  本地库不登记、不参与同步、无「转同步实例」导入；重启不自动进本地 Main（无配置记录，
  需再点入口）；旧 dev vault 不搬迁；UI 像素/DPI/IME 与 Windows 真机未验。
  详见 [`docs/notes/sync-product-a2.md`](docs/notes/sync-product-a2.md)。

- **日记模板笔记选择器（P1 日常体验，2026-11-19，未 commit）**：
  设置页「日记」的模板字段**保留手填输入框**（仍可精确输入任意普通笔记路径），旁边新增
  「选择模板笔记…」按钮（`settings-daily-notes-pick-template`）。点击打开独立覆盖层
  `DailyTemplatePicker`（与「插入模板」选择器的业务语义分离，只复用 IME 搜索框/覆盖层样式）：
  - 候选 = 当前库**全部普通笔记**（`TreeEntry::is_note()`；不按模板文件夹过滤、不加隐藏/
    保留名规则），按完整路径大小写不敏感字典序；每行标题 + 相对路径；
  - 搜索框（复用 `RenameInput`，标题/路径子串匹配）、`↑/↓`、`Enter`、点击、`Esc`、
    点空白关闭；当前 `template` 在列表里时默认选中并显示「✓ 当前」
    （`daily-template-picker-current`）；
  - 选中后按 `normalize_note_path` 规整 → **写回模板字段并立即原子写**
    `daily-notes.json`（`set_preferences`）→ 关闭选择器；空库/无候选时只显示空态，
    **不发选中事件、不写空路径、不 panic**；手填输入框仍原样可用（清空 = 无模板）。
  固定 selector：`settings-daily-notes-pick-template`、`daily-template-picker` /
  `-panel` / `-search` / `-list` / `-empty` / `item-<i>` / `-current`。
  选择器由宿主持有/渲染：产品 `AppRoot` 设置页与 dev-sync `AppShell` 设置页各挂一份
  （`AppShell` 只新增只读 `tree_snapshot()` 访问器与 UI 接线，无同步/relay/storage/
  schema/wire 改动）；设置关闭/重新打开时清空，不残留。新增 `daily_notes.*` 7 条
  中英文案（全部经 `ui_text`）。测试：app 默认 **522 → 526** /
  `--features dev-sync` **677 → 680** / i18n **16**；新增
  `headless_daily_template_picker` 2（产品构建：全库候选/搜索/Enter+点击写盘/当前标记/
  Esc+点空白不写/关闭重开不残留/手填仍可用；两种构建：空库空态不写盘）+
  `daily_notes` 单测 2（候选/排序/过滤）；fmt/clippy 干净。已知限制：只列当前库已有
  普通笔记（不支持新建/虚拟候选）；路径仍精确/大小写敏感；Windows 真机像素/IME 未验；
  AppShell 设置页宿主只做编译级覆盖（交互 e2e 走 AppRoot 设置页）。详见
  [`docs/notes/templates-daily-notes.md`](docs/notes/templates-daily-notes.md) §8.1。

- **插件启停单一入口 + 设置底栏按分类收敛 + 设置字段点击不选中（2026-11-19，ADR 0023）**：
  设置 →「插件」页的 `settings-plugin-<id>-toggle` 是模板/日记**唯一**启停入口；插件自己的
  设置分类（模板/日记）移除重复的「启用模板 / 启用日记」按钮、状态文本与
  `EnabledChanged` 事件（`TemplatesSettingsEvent` 整体删除；`DailyNotesSettingsEvent`
  只剩 `PickTemplateRequested`）。旧 selector `settings-templates-toggle` /
  `settings-daily-notes-toggle` 及 `-state` 删除，测试改为断言不存在并通过插件页总开关
  写盘/隐藏分类；程序化停用当前插件分类仍回退到「插件」页。
  设置底栏不再对所有分类显示「保存模式/relay（重启后生效）」：只有「Relay」分类渲染
  `settings-save`（文案「保存 relay 设置（重启后生效）」）、`settings-cancel`、
  `settings-error` / `settings-notice`；其它分类只显示 `settings-close`，避免让外观/
  语言/插件设置看起来需要保存或重启。`settings-error` 不再显示「无错误」占位；
  relay 默认提示文案限定为 relay（中英文同步）。
  设置字段单击**不再全选**：`RenameInput::focus_for_edit` 只聚焦并把光标放到末尾；
  需要替换整段用 `Ctrl+A`/拖选（`headless_notes_plugins_ui` 断言单击后输入是追加）。
  测试：app 默认 **526** / `--features dev-sync` **680**；新增
  `docs/adr/0023-plugin-toggle-and-settings-save.md`；fmt/clippy 干净。详见 ADR 0023。

- **笔记编辑历史：本地优先（2026-11-19，ADR 0024，已决待实现）**：
  用户确认先做**本机历史**，跨设备一致时间线后置。数据源是本地 `loro_updates` + snapshot，
  **不查询 relay 旧日志**；版本按编辑会话捕获（停止输入约 2s / 切换笔记 / 关闭窗口 /
  同步导入完成），保存压缩正文快照与时间/设备/来源/op 范围。入口为笔记「历史记录…」时间线
  覆盖层，点击版本只读预览；恢复默认**恢复为新笔记**（append-only，不覆盖当前、不改写
  CRDT 历史）。v1 默认保留最近 200 个版本或 90 天，不做自动物理裁剪 UI。跨设备一致历史
  需要独立加密 history log，后续单独排期；回收站/裁剪/归属仍按
  `docs/briefs/0006-history-and-trash.md`。详见
  [`docs/adr/0024-local-note-history.md`](docs/adr/0024-local-note-history.md)。

- **task-123 审计：默认「只吊销」与轮换 opt-in 已在树内（2026-11-19）**：
  ADR 0010 早已落地独立吊销：relay `POST .../devices/{device}/revoke` 只写
  `devices.status='revoked'` + `auth_rev += 1`，**不**推进 `control_rev`、不换 epoch、
  不冻结 vault；被移除设备 challenge/旧 token/WS/approve 全部 `403 REVOKED`，
  同 device_id 重新 redeem `409 ID_CONFLICT`。客户端默认 `DevLocal::revoke_device` 只吊销，
  轮换唯一入口仍是显式 `DevLocal::rotate`。本轮补验收断言（epoch/control_rev 不变、
  重复幂等、其余设备继续同步、显式 rotate 才换 epoch、被移除设备不拿 grant）与契约注释，
  wire/错误码/schema 未变。测试：relay **380** / sync **618** / sync-proto **64**。
  详见 [`docs/notes/task-123-revoke-only-rotation-opt-in.md`](docs/notes/task-123-revoke-only-rotation-opt-in.md)。

- **P1：本地/同步入口重启恢复 + 设置分类记忆（2026-11-19）**：
  本机 `startup.json`（数据根，原子写/损坏回退/不同步）记录 `mode=local|sync`：
  上次本地 → 重启直接打开 `<数据根>/local/vault.sqlite3` 进 Main（**不读 sync.json、
  不联网、不建库**）；本地库不存在 → Welcome + `app-root-notice` 明确提示，不自动建库；
  同步实例打开成功写 `sync`，之后沿用 active 实例自动打开。设置页分类记忆 `settings-ui.json`
  （只存短名，原子写/损坏/未知回退 `appearance`）；已关闭插件分类 clamp 到 `plugins`，
  且 clamp 不回写磁盘。产品 `AppRoot` 生效；`AppShell` dev-sync 设置页未改。
  测试：app 默认 **546** / `--features dev-sync` **697**（`startup_resume` 9 产品 / 5 dev-sync，
  onboarding_ui +1，layout/app_root 单测 +5）。详见
  [`docs/notes/startup-resume-settings-memory.md`](docs/notes/startup-resume-settings-memory.md)。

- **P1：模板/日记相对日期（2026-11-19）**：
  `{{date:-1d}}` / `{{date:+1d}}` / `{{date:+1w}}` / `{{date:-1m}}` / `{{date:+1y}}`，
  可带格式 `{{date:-1m:YYYY/MM/DD}}`；偏移 = 可选 `+/-` + 1–2 位数字 + `d/w/m/y`，
  先判偏移再判格式。纯函数 `RelativeDateOffset::{parse,looks_like_offset,apply}` 与
  `DateTimeParts::{add_days,add_months,add_years}`；跨月/跨年/闰日/月末对齐正确；
  非法偏移（空、`+d`、`1x`、`1.5d`、`123d`、大写单位、越界）**原样保留**、不 panic。
  日记纯 API `note_path_for_offset` / `note_path_for(offset_days)`（越界 `OutOfRange`）。
  设置 hint 中英文说明语法；未做日历覆盖层、周期笔记、昨天/明天入口。
  测试：headless_templates +1、templates/daily_notes 单测 +5；app 默认 **546** /
  dev-sync **697** / i18n **16**。详见 [`docs/notes/templates-daily-notes.md`](docs/notes/templates-daily-notes.md)。

- **GPUI Kit 升级 0.9.1 → 0.10.0（2026-11-19）**：
  按 ADR 0004 的升级规则只改 `gpui-kit` 版本并让 cargo 重解整套：`gpui-kit` / `gpui-base` /
  `gpui-component` / `gpui-component-macros` / `gpui-kit-assets` 全部 **0.10.0**，
  `gpui-pre` 仍为 **0.3.12**（kit 自身精确 pin），`gpui-fast*` 0.1.14 → 0.1.17。
  `crates/app/Cargo.toml` 两处 `gpui = { package = "gpui-kit", version = "=0.10.0" }`
  （普通 + `test-support` dev-dependency），`Cargo.lock` 一并提交。
  本机 Linux 验证：`cargo check -p menghunote-app --all-targets --features dev-sync` 通过；
  app 默认 **546** / `--features dev-sync` **697**；fmt/clippy 干净。
  **Windows 真机 UI smoke（IME/DPI/窗口）仍需用户验收**；未改业务代码。

- **画布笔记 / Excalidraw（2026-11-19，第一方编入模块）**：
  vendor `gpui-excalidraw 0.1.9` 到 `crates/excalidraw`，`gpui-kit` 精确 pin 到
  **=0.10.0**；上游 `on_scroll_wheel` 改为 `on_scroll`，`HelpIcon.svg`/`helpIcon.svg`
  大小写冲突只保留一份并做大小写不敏感查找。应用侧 `AppAssets` 叠加手绘图标与
  GPUI Kit 默认资产，启动注册 Excalifont/Virgil。
  画布是**普通笔记**：标题 `.excalidraw` 后缀（大小写不敏感）即识别，正文存
  `.excalidraw` JSON；**不新增** note kind/schema/sync 消息。打开时普通
  `EditorView` + **内容区** Excalidraw 覆盖层（侧栏可见；vendor 命中按 `canvas`
  原点校正 `local_point`，绘制加回原点，中键拖动平移）；保存 = 整篇 replace-all 写回正文（一条 `EditRequest`/一个撤销组），
  关闭不保存 = 正文零变化；关闭后显示**占位页**（绝不把 JSON 源码铺到编辑区），
  「打开画布」/再点同一行重新进画布；重命名为 `.excalidraw` 时立即重开。
  侧栏「画布」新建 `未命名画布.excalidraw`（序号在后缀前）。
  覆盖层只占编辑器内容区，侧栏/底栏保持可见。`![[xxx.excalidraw]]` 嵌入预览、
  文件导入/导出 UI、真实第三方插件运行时均未做。
  测试：app 默认 **577** / `--features dev-sync` **728** / `--no-default-features`
  **455**；gpui-excalidraw **301**；i18n **16**；plugin-api **24**；fmt/clippy 干净。
  **Windows 真机（DPI/输入法/画布点哪画哪/工具栏观感）尚未验。**

- **应用图标（2026-11-19）**：用户提供 512×512 SVG 母版，已放
  `crates/app/assets/app-icon.svg`；预生成 `app-icon-256.png`（运行时窗口/任务栏）、
  `app-icon-512.png`（大图/安装包）与 `app-icon.ico`（16/24/32/48/64/128/256）。
  `app_icon::window_icon()` 解码 PNG → `WindowOptions.icon`，失败不阻塞窗口；
  app 不新增 SVG 运行时依赖。`build.rs` 在 **Windows 目标**用 `embed-resource` 把
  `app-icon.rc` → `assets/app-icon.ico` 嵌进 exe（**资源 ID 必须是数字 1**，GPUI
  Windows 后端按 `PCWSTR(1)` 取图标；MSVC 自动找 SDK `rc.exe`，GNU 回退
  `windres`；Linux/macOS 跳过）；找不到资源编译器会明确失败，不静默无图标。
  **Windows 真机构建仍需验**；macOS `.icns` 未做。
  测试：`app_icon` 2 个；app 默认 **566**。
