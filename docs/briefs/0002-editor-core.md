# 决策简报 0002 — 编辑器内核（Typora 式所见即所得）

> 2026-11-22 历史整理：本文是整理前的历史文档，文中旧 commit 锚点已清除（原哈希不再对应新 `main`）；原始版本见 `backup/dev-20261122`，旧→新映射见 `docs/notes/history-rewrite-20261122.md`。


> 状态：**待 GPT 决策**（`bluetrum/gpt-6.1-sol`, `reasoning_effort: "max"`）
> 决策落地后请写 `docs/adr/0002-editor-core.md`
> 提交时把「要决断的问题」至「输出要求」整段作为 prompt。

---

## 项目背景

- **menghunote**：GPUI（Rust）笔记软件，桌面端优先，**Windows 首要平台**。
- 语言 Rust；单人开发起步，AI agent 团队协作编码。
- 正文格式为 **Markdown 纯文本**（见决策 0001：源码即真源，`**` 等标记真实存在于文本中）。

## 要决断的问题

**编辑器内核架构**：如何在 GPUI 上实现 **Typora 式"所见即所得"** 的 Markdown 编辑器——即
**渲染视图就是编辑界面，Markdown 语法标记在非编辑状态下不绘制、光标进入时显现**——
并给出**分阶段**落地路径与 v1 功能边界。

## 目标编辑体验的精确定义

必须区分三种常见的"富文本 Markdown 编辑"，**本项目要的是第 1 种**：

| 模式 | 真源 | 标记字符 | 代表 |
| --- | --- | --- | --- |
| **① 原位 WYSIWYG（本项）** | Markdown 文本 | 存在，但**不绘制**（光标进入时显现） | Typora |
| ② 块编辑器 | 块树（保存时序列化） | 不存在 | Notion、Obsidian（部分） |
| ③ 分栏预览 | Markdown 文本 | 存在且始终绘制（左栏） | 早期 Markdown 编辑器 |

**关键要求**：
- 标题、粗体、斜体、行内代码、引用、列表：**渲染即所见**，标记默认隐藏；
- 光标/选区落到某个行内构造内时，该构造的标记**显现**（可继续编辑）；
- 块级构造（代码块、表格、图片、分隔线）渲染为块，非编辑态不显示 ``` 围栏；
- **中文输入法（IME）必须是第一等公民**：组合态文本、候选窗定位、光标跟随都要正确。

## 已核实的 GPUI 事实（2026-11）

| 事实 | 影响 |
| --- | --- |
| **GPUI 不自带文本编辑器** | Zed 的 `crates/editor` 不随 `gpui` 发布。编辑器必须自研 |
| `ElementInputHandler` / `EntityInputHandler` / `InputHandler` 三个 trait 存在，用于对接平台 IME | IME 有官方钩子，但组合态处理要自己写 |
| `WindowTextSystem::shape_line(text, font_size, runs, force_width) -> ShapedLine` | **只能排一行**；多行/换行/行高要自己用自定义 `Element` 实现 |
| `TextRun` 支持按子串设字体/字重/颜色；`DecorationRun` 设下划线/删除线 | 粗体、标题字号、行内代码底色可实现 |
| 存在 `Boundary` / `LineFragment` 类型 | 折行支持存在，但需自行驱动 |
| 渲染是多边形即时模式，`Canvas` 可低层绘制 | 光标、选区、块级装饰可自绘 |
| `gpui 0.2.2` 发布版支持 Windows（`windows-manifest` feature、HLSL shader；main 分支 README 明确 Win32 + DirectWrite） | 平台可行 |
| ⚠️ 基线已定为 `gpui-unofficial` 1.27 + `gpui-platform-gpui-unofficial`；但**生态已分叉**，见问题 6 | 组件库不能跨血脉复用 |
| 可用的 Markdown 解析生态：`markdown` 1.0（`^1.0.0`，serde 支持）、`pulldown-cmark`、`tree-sitter-md` 0.5 | 增量解析有现成积木，见问题 4 |
| `ropey` 2.0.0-beta.1 是社区里 gpui 相关项目常用的文本缓冲 | 文本缓冲可不必自研 |

## 核心技术难点（请逐条给出方案）

1. **Display ↔ Source 映射**：绘制时省略标记字符，于是**显示字符串 ≠ 源字符串**。
   光标位置、选区端点、鼠标点击命中、IME 组合态插入点，全部需要在
   `源字节偏移 ↔ 显示位置 ↔ 屏幕坐标` 之间双向映射。这个映射若设计不当，会成为全项目最大的 bug 温床。
2. **布局抖动**：光标移入/移出构造会改变显示串 → 该行重新排版 → 文档"跳动"。
   Typora 通过"只影响当前行"来控制，但需要精细的失效粒度。
3. **增量解析**：Markdown → 块树 + 行内 span。如何做到只重算被编辑的块？请给出缓存键与失效策略。
4. **块级 widget**：代码块（等宽 + 高亮）、表格、图片、数学公式——它们是**嵌在流式文本中的组件**，
   不是样式文本。v1 该支持哪些、哪些明确砍掉？
5. **选择与复制**：跨隐藏标记的选区复制出来的文本，应该是**带标记的 Markdown 源码**还是纯文本？
6. **撤销/重做**：编辑操作与"标记显现状态"这类视图态是否该进同一条撤销栈？

## 候选方案（可组合/改造）

- **A. 行级自研 WYSIWYG**：把每行渲染为"省略标记后的显示串"，自建映射层，光标/选区/命中测试全基于映射。
  → 最接近 Typora；控制力最强；**成本最高**。
- **B. 块树编辑器**：文档解析成块树，每块一个简单文本编辑控件，序列化回 Markdown。
  → 实现最省力；但会破坏"源码即真源"，跨块选区、复制粘贴、**解析-序列化往返保真**都是坑，且**不是 Typora 的编辑模型**。
- **C. 分层递进（推荐的讨论起点）**：v1 只做**块级容器渲染 + 行内纯文本编辑**（标题字号、列表缩进、引用条、代码块底色），
  **不做**行内标记隐藏；v2 再上线"行内驻留隐藏 + 光标显现"。
  → 先把"文档模型 + 布局 + IME + 映射层"骨架立起来，把最危险的映射问题推迟到有测试之后。
- **D. 先做分栏预览过渡**：左源码右预览，作为 v0 快速验证，再演进到 A/C。
  → 最快看到成果；但用户明确要的是所见即所得，属于临时形态。

## 需要明确回答

1. **选哪个方案（或组合）**，并明确 **v1 的功能边界**：哪些 Markdown 构造在 v1 必须支持、哪些可以拒绝渲染/降级为源码。
2. **映射层的具体设计**：给出数据结构（例如 `Vec<Segment>` 或区间映射表）、更新算法，以及"隐藏区间"如何随选区变化而重算。
3. **布局单元**：按"行"还是按"块"排版？折行如何处理？如何保证滚动时行高稳定、不抖动？
4. **增量解析的失效粒度**与缓存设计。
5. **IME 方案**：结合 GPUI 的 `EntityInputHandler` 给出组合态处理与光标定位做法（中文输入是主场景）。
6. **生态血脉取舍（重要）**：GPUI 已分裂为互不兼容的几条血脉，`Element` / `Entity` 类型不能跨血脉混用：
   - Zed 主干：`gpui-unofficial` 1.27 + `gpui-platform-gpui-unofficial`（**当前已选**，无现成组件库）
   - longbridge 系：`gpui-pre` / `gpui-fast` + `gpui-base` + **`gpui-component`（60+ 现成组件，47 万下载）**
   - gpui-ce 系：`gpui_ce_components`
   - Zed 官方 `gpui` 0.2.2（停在 2025-10，过时）

   问题：**为了拿到现成组件库而切换到 longbridge 血脉，是否值得？**
   请权衡 ①`gpui-component` 能省掉多少 UI 工作 ②换血脉后与 Zed 主干 API 的偏差与长期维护风险
   ③两条血脉各自的生命力（发布频率、维护者、issue 响应）。给出明确建议。**注意：我方的编辑器内核无论如何都要自研**，组件库只影响外壳 UI（列表、侧栏、对话框、按钮）。
7. **测试策略**：这类编辑器如何做**可回归的自动测试**（映射、选区、往返保真）？不要只依赖手测。已知 `gpui-testing-library` 0.1.0 存在，评估是否可用。
8. **工作量评估**：以"单人 + AI 辅助"给出各阶段的粗略投入量级，以及**最容易失控的地方**。

## 输出要求

- 直接给结论与理由，不复述问题。
- 明确标注 **不可逆** 决定 vs 可后补决定。
- 对每个"砍掉的功能"说明**降级体验**是什么（例如表格 v1 只读源码）。
- 最后回答："如果只做一件事，先做什么。"

---

## 附录：实测事实补充（2026-11-09，供决策与 ADR 使用）

### A. 测试库不可用（实测 Cargo.toml）

`gpui-testing-library 0.1.0` 的实际依赖：

```toml
gpui = { package = "gpui-pre", version = "=0.3.9", features = ["test-support"] }
gpui-kit = { version = "=0.7.3", default-features = false, features = ["test-support"], optional = true }
```

- 它把 `gpui` 精确绑定到 **`gpui-pre =0.3.9`**，且可选的 `gpui-kit =0.7.3` 也是精确版本；
- 与本基线 `gpui-unofficial 1.27` **不同血脉**，甚至与 longbridge 当前 0.9（`gpui-pre 0.3.11`）**不同版本**；
- 结论：**不能作为本项目的测试依赖**，也不为测试库更换基线。

### B. 同血脉测试能力存在（实测 feature 表）

`gpui-unofficial 1.27.0` 自带：

- `test-support`（含 `collections/test-support`、`http_client/test-support`、`proptest` 等）；
- `leak-detection`（"Tracks every entity handle so tests can assert nothing leaked"，也可用 `GPUI_LEAK_DETECTION` 环境变量开启）；
- `bench-support` / `profiler`（`hdrhistogram`）。

→ 测试路线：**纯内核属性测试（映射/选区/往返保真）+ 同血脉原生 `test-support` + 真实 Windows IME 手测**。

### C. 发布桥风险（实测 README，2026-11-09）

`gpui-unofficial 1.27.0` 的 README 原文要点：

- 它是 **gpui-archipelago（"GPUI-lago"）的自动化非官方发布**，由 Zed `v1.27.0`
  （commit 旧基线）生成；
- 明确写着 **"unofficial, not affiliated with or maintained by Zed Industries"**；
- 在项目自述里，`gpui-unofficial` 条目下直接写着 **"Currently needs a maintainer"**；
- 版本号逐字取自 Zed 的 semver：**已发布版本不可替换**，修复只能等下一个 Zed release。

近期节奏（约周级）：`gpui-unofficial` 1.25.3（10/23）、1.26.2（10/30）、1.27.0（11/4 UTC）；
`gpui-component` 0.8.0（10/19）、0.8.1（10/26）、0.9.0（11/2）。

**含义**：**"Zed 上游活跃" ≠ "发布桥有人兜底"**。上游周更保证"新版本来得快"，
不保证"我们踩到的 bug 能被补上"——因为版本一旦发布就锁死，补丁最快也要等下一轮。

**倾向（待 ADR 0002 正式记录）**：保留已在本机验证通过的 Windows 基线，不为外壳组件库或测试库换血脉；
把"发布桥无维护者 + 版本不可回改"写成项目级已知风险，并保留两条退出路径：
① 自建 fork 并本地 patch；② 整仓切到其他血脉（README 列出的还有 gpui-kit/longbridge、gpui-ce、Kael、WGPUI）。

可用于编辑器撤销栈，无需自造快照撤销：

| 能力 | API | 语义要点 |
| --- | --- | --- |
| 本 peer 选择性撤销 | `undo()` / `redo()` | **local-only**：只回退**本 peer 的本地操作**，不回退远端操作；全局回滚要用 `checkout`/`revert_to` |
| 分组 | `group_start()` / `group_end()` | 组内的连续修改合并为一个撤销项；遇到**冲突**的远端变更会拆组 |
| 手动断点 | `record_new_checkpoint()` | 在提交窗口（250 ms）边界处打点，避免把一次粘贴拆成多个撤销步 |
| 排除来源 | `add_exclude_origin_prefix(&str)` | 特定 origin 的本地事件不进撤销栈（可用于"派生投影/程序化修正"） |
| 深度与合并 | `set_max_undo_steps(n)`（默认 100）/ `set_merge_interval(ms)`（默认 0 = 不合并） | 与 250 ms 提交窗口配合时需显式设 `merge_interval` |
| 只读预览 | `pause()` / `resume()` / `is_paused()` | 查看历史版本时暂停撤销记录，避免污染栈 |
| 元数据 | `top_undo_meta()` / `top_undo_value()` 等 | 可用于"撤销提示"文案 |

**已知坑（必须写进 ADR）**：

1. 使用期间 **`peer_id` 必须稳定**，否则撤销分组/语义会被打乱；这与 ADR 0001「克隆备份必须换新 peer_id」并不冲突——换 peer 属于新实例，不是运行中更换。
2. 撤销"删除 `LoroMap` 普通子容器"会**重建一个带新 ID 的容器**；对旧 ID 的并发远端编辑**不会重定向**到新容器。
   若将来用容器承载块/附件引用，需要改用 `ensure_mergeable_*` 系列或在容器内用 `deleted` 字段表达删除，
   否则"撤销删除"会丢并发修改。

### E. GPT 拟定核心裁断（2026-11-09，完整版待收，实现前以此为准）

1. **目标方案 = A（源码投影编辑器）**，但布局单元不是"行"，而是 **语义块 → 逻辑行 / 视觉折行**。
2. **C（块级容器 + 行内纯文本）只允许作为内部 v0/alpha**；**不得**把"全部行内标记一直显示"叫产品 v1。
   **v1 必须包含粗体 / 斜体 / 行内代码的隐藏与"光标进入即显现"**。
3. **单编辑器 Entity**：一套**全局 source selection**、**一个 IME handler**；
   块级 widget **不得**各自拥有文本缓冲或自己的 Undo。
4. **IME 用"source + preedit 的虚拟输入文档"**：所有平台的 UTF-16 方法都针对这**同一个虚拟文档**，
   而**不是**针对隐藏标记后的显示串。
5. v1 在 **composition 期间暂缓同一 Doc 的远端 import**；commit / cancel 之后再**串行合并**——
   避免"针对当前锚点范围的删除"误伤刚到的远端字符。
6. **复制默认 = exact source slice**（带标记的源码切片），不是纯文本。
7. **视图态不进 Undo**；撤销采用 **Loro 1.17.1 的本 peer `UndoManager`**（见附录 C/D 的实测）。
8. 解析：倾向 **`tree-sitter-md` 的 block + inline 双 grammar** 做**增量派生层**；
   **隐藏只限已验收的语法规则**；tree-sitter-md 的 README 自承**不保证 CommonMark 完全正确**
   → 必须**保守源码降级**，并以 **`pulldown-cmark` 作测试参照**；
   **不得**对外宣传"用了 tree-sitter 就保证语义正确"。
9. **顺序**：先完成 **ADR 0001 第 2 步 gate 下的 Unicode 区间 / 基准 / 合并 / ACK 夹具**，
   再做最小"**粗体 + 中文 IME**"垂直切片；**不先堆外壳 UI**。
