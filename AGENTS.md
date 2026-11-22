# AGENTS.md — menghunote

本文件是 AI agent 在本仓库的行为契约。**优先级高于任何单次对话中的临时指令**。
若某次任务的要求与本文件冲突，以本文件为准，并在回复中说明冲突点。

---

## 1. 项目定位

**menghunote** —— 基于 [GPUI](https://gpui.rs/) 的笔记软件，**桌面端优先**。

| 项 | 决策 |
| --- | --- |
| UI 框架 | **`gpui-unofficial` 1.27 + `gpui-platform-gpui-unofficial`**（跟随 Zed 主干的非官方发布，活跃维护） |
| 语言 | Rust（edition 2021+） |
| **编辑模型** | **Typora 式原位所见即所得**：编辑器内文本即 Markdown 源码（`**` 等标记真实存在，默认不绘制、光标进入时显现）；持久化真源是 SQLite 单库（见 ADR 0001）。详见 [`docs/briefs/0002-editor-core.md`](docs/briefs/0002-editor-core.md) |
| 首要平台 | **Windows 桌面**（开发机即 Windows） |
| 后续平台 | macOS / Linux（架构上预留，但**不为未落地平台提前抽象**） |
| 存储 | **SQLite 单库（WAL）为持久化真源**；Markdown 仅单向导出（不做外部编辑、不监听文件）。详见 [`docs/adr/0001-storage-format.md`](docs/adr/0001-storage-format.md) |
| 同步 | **设计必须支持**多设备同步 + 用户自建中心服务器；实现可后置，但数据模型现在就要留口 |

> **Windows 现实约束**：GPUI 的 Windows 后端相对较新，跨平台细节（图形后端、崩溃上报、输入法）仍在磨合。
> 因此：**每完成一个可见特性就立刻在 Windows 上跑一遍**，不要攒到最后；
> 平台相关代码必须收敛在 `cfg(target_os = ...)` 或独立模块内，禁止散落在业务逻辑里。

> **依赖基线（已确认，勿再更换）**：
> ```toml
> [dependencies]
> gpui-unofficial = "1.27"
> gpui_platform = { package = "gpui-platform-gpui-unofficial", version = "1.27.0" }
> ```
> `gpui-unofficial` 跟随 Zed 主干，发布频繁（2026-11 仍在更新），API 形如 `gpui_platform::application()`。
> **crates.io 上的官方 `gpui` 停在 0.2.2（2025-10），已过时，不要用。**

> ⚠️ **生态分叉风险（选型前必读）**：GPUI 已分裂成**互不兼容**的多条血脉，各自的 `Element` / `Entity` 类型不能混用：
>
> | 血脉 | crate | 备注 |
> | --- | --- | --- |
> | Zed 主干（**本项目**） | `gpui-unofficial` + `gpui-platform-gpui-unofficial` | 1.27，活跃 |
> | Zed 官方发布 | `gpui` 0.2.2 | 停在 2025-10，过时 |
> | longbridge 组件系 | `gpui-pre` / `gpui-fast` + `gpui-base` + `gpui-component` | **组件最丰富**（60+ 组件，47 万下载），但血缘不同 |
> | gpui-ce 系 | `gpui_ce_components` | 另一分支 |
>
> **含义**：选了 `gpui-unofficial`，就**不能直接复用 `gpui-component` 的组件**。
> 这是"重造 UI 轮子"与"换基线"之间的真实取舍，**必须先经 GPT 决策**，不要随手混用依赖。
> 判定方法：写一个 5 行骨架直接编译验证，**不要只相信依赖图推断**。

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
│  ├─ text/       # 唯一坐标/单位原语：SourceByte/UnicodeScalar/InputUtf16/DisplayByte、SourceRange、CoordError
│  ├─ core/       # 领域模型（后补，暂未落地）
│  └─ storage/    # SQLite/CRDT/物化/同步 + session（EditorDocumentPort 的串行 reducer）
├─ docs/
│  ├─ briefs/     # 决策简报（喂给 GPT 的输入，提案阶段）
│  └─ adr/        # 架构决策记录（GPT 决策的结论落地处）
└─ AGENTS.md
```

**依赖方向（ADR 0003，禁止成环，含 dev-dependency）**：
`app → editor/storage/text/gpui`；`editor → storage/text`；`storage → text/loro/rusqlite`；
`text → unicode-segmentation`。`storage` **不依赖** `editor`；`editor`/`storage`/`text` **不依赖** GPUI 或 `app`。

**决策落地规则**：GPT 给出的每个架构级结论，都要在 `docs/adr/NNNN-*.md` 留一页
（背景 / 选项 / 结论 / 后果）。这样后续执行不再重复问 GPT。

---

## 6. 构建与验证

**已实测通过（2026-11-08，cargo 1.99.1 / rustc 1.99.1 / Windows）**：

```bash
cargo build -p menghunote-app     # ✅ 冷构建约 3m50s
cargo run   -p menghunote-app     # 开一个空窗口
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

Lead 在给出"完成"结论前，必须实际执行并贴出结果。

> ⚠️ **本机构建环境的第二个坑（2026-11-09 实测）**：cargo 启动的子进程（build script / test binary）
> **不能写 `%TEMP%`**（`Access denied, os error 5`；bash/python 不受影响）。由此产生两条固定做法，勿随手回退：
> 1. 根 `Cargo.toml` 的 `[profile.dev.package.libsqlite3-sys] debug = false`：`cl.exe` 在 build script 上下文里
>    带 `-Z7`/`-Zi` 编译 `sqlite3.c` 必报 `D8050 ... failed to get command line into debug records`。
>    删掉该节会直接构建失败（已反证）；换到可写环境后可删。
>    同因还有 `[profile.dev.package.stacker] debug = false`（`loro → pest → pest_stack_guard → stacker`，
>    stacker 带 `src/arch/windows.c`）。**今后任何带 C 源的新依赖撞到 D8050，都按同样方式单包覆盖并注明理由**，
>    不要用全局 `debug = false`（会连 Rust 依赖的调试信息一起关掉）。
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

---

## 8. 当前状态

- **脚手架已就位**：Cargo workspace + `crates/app`（空窗口），依赖基线**已编译验证通过**。
- **版本管理**：当前开发分支 **`dev`**（按"已验证检查点"重建的干净历史，9 个提交）；旧历史完整保留在
  **`backup/main-wip-20261110`**（含全部 wip 与被推翻的尝试）。tag `v0.1-slice6` = 切片 6 真机验收点。
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
