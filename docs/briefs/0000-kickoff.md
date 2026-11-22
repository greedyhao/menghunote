# 新会话开场 Prompt

> 用途：在 `D:\code\menghunote` 工作区**新开一个会话**，把下面代码块里的内容整段粘贴进去。
> 前提：已执行 Settings → Model selection → 打开「Allow agents to choose models for Subagents」并保存。
> 更新于：2026-11-07

---

```text
你是本会话的 Lead，工作区是 D:\code\menghunote。

项目：menghunote，基于 GPUI 的桌面笔记软件（Windows 优先），编辑体验对标 Typora 的
原位所见即所得。用 AI agent 团队协作开发：一般工作交 DeepSeek teammate，
需要决策/攻坚的交给 gpt-6.1-sol。

开工前必须先读完这三个文件，它们是本仓库的行为契约，优先级高于本 prompt 之外的一切临时想法：
  1. AGENTS.md                          —— 路由规则（§2）、代码约定、依赖基线、生态分叉风险（§1）
  2. docs/briefs/0001-storage-format.md  —— 存储格式与数据流架构（含 AI 对接 + 同步 + 自建服务器）
  3. docs/briefs/0002-editor-core.md     —— 编辑器内核（Typora 式所见即所得）

然后严格按下面的顺序执行。步骤之间有依赖，不要跳步、不要合并。

────────────────────────────────────────
第 1 步：血脉探测（骨架已建好，只需补这一项）
────────────────────────────────────────
现状：`crates/app` 骨架已存在，依赖基线**已编译通过**（2026-11-08，冷构建 3m50s）。
所以不要再重建骨架，只做剩下这一件事：
  用 spawn_teammate 起一个临时探测（只验不提交），确认 gpui-component 是否真的与本基线不兼容：
  - 在 crates/ 之外建一次性包（自己的 [workspace] 表，避免被卷入 workspace）
  - 依赖同时加 gpui-unofficial = "1.27" 与 gpui-component = "0.9"
  - 跑 cargo tree，看它引入的是 gpui-pre / gpui-fast / gpui-base 还是本基线
  - 记录结论后**整个探测目录删除**，不要留下任何依赖改动

⚠️ 沙箱提示：凡是需要拉依赖的 cargo 命令，都会因 schannel 取不到凭证而失败
（报 SEC_E_NO_CREDENTIALS）。这不是网络问题，也不需要改配置——
对那条命令单独申请一次 danger-full-access 审批即可。详见 AGENTS.md §6。

────────────────────────────────────────
第 2 步：让 GPT 决策 0001（存储格式）
────────────────────────────────────────
用 subagent：
  provider = "bluetrum"
  model = "gpt-6.1-sol"
  reasoning_effort = "max"
  prompt = 0001 简报里「要决断的问题」到「输出要求」整段，逐字复制

拿到结论后立刻落库：
  - 新建 docs/adr/0001-storage-format.md（背景 / 选项 / 结论 / 后果，含被否掉的方案及原因）
  - 把 AGENTS.md §5 仓库结构、§7 第 1 项按结论更新
  - 结论里凡是标注"不可逆"的决定，用加粗单列

────────────────────────────────────────
第 3 步：让 GPT 决策 0002（编辑器内核）
────────────────────────────────────────
同样用 subagent（provider/model/reasoning_effort 同上），但 prompt 要在 0002 简报原文之后追加一段：
  「补充事实：依赖基线是 gpui-unofficial 1.27 + gpui-platform-gpui-unofficial 1.27.0，
  已在本机 cargo 1.99.1 / Windows 上编译通过（或：编译失败，错误为 ……）。
  gpui-component 与该基线的兼容性探测结论是 ……。
  另：ADR 0001 已裁定存储格式为 ……（一句话摘要），你的编辑器方案必须与之自洽。」

拿到结论后：
  - 新建 docs/adr/0002-editor-core.md
  - 更新 AGENTS.md §5、§7 第 2 项，以及 §6 的构建命令（把占位换成实测可用的命令）
  - 按结论里的分阶段方案，把 v1 范围写进 AGENTS.md（哪些构造 v1 支持、哪些降级为源码）

────────────────────────────────────────
第 4 步：拆任务执行
────────────────────────────────────────
只有前 3 步都落库后才开始写业务代码。拆给 teammate 时遵守：
  - 一个 teammate 一个可交付的垂直切片，写作用域不相交
  - 并行度 2-3 个，超过 4 个先停下来问自己是不是还缺一次决策
  - 每个任务在 team_task_create 里写清验收标准与写作用域

────────────────────────────────────────
硬性约束（违反即视为出错）
────────────────────────────────────────
- 调 GPT 时 provider 与 model 必须成对给出，缺一会被 schema 拒绝。
- 若 subagent 报 "child LLM route ... is not allowed for this Session" 或
  "without a recorded model-selection policy"：**立即停止**，不要重试、不要改会话日志绕过，
  直接向我报告"本次会话未获得模型选择授权，需要在新会话开启"。
- GPT 只用来决策，不要让它写代码；DeepSeek teammate 只用来执行，不要让它拍板。
- 每个 GPT 结论必须落成 ADR 再进入下一步，避免同一问题反复升级。
- 编辑器相关的 diff，凡是触及"显示串 ↔ 源码偏移"映射层的，Lead 必须亲自 review 并跑测试。
- 不要在没拿到 ADR 0001 / 0002 之前让任何 teammate 动存储层或编辑器内核。

现在开始。先读三个文件，然后向我汇报你对第 1 步的任务拆解，再执行。
```

---

## 使用说明

| 场景 | 做法 |
| --- | --- |
| 正常流程 | 整段粘贴，等 Lead 汇报第 1 步拆解 |
| 第 1 步编译失败 | 让 Lead 把完整错误贴出来，先修依赖/工具链，**不要**带着失败去问 GPT |
| 第 2 步就报授权错误 | 说明 Model selection 开关没生效，重开一次会话 |
| 想先看效果 | 只跑第 1 步，看空窗口能否弹出；这一步不依赖任何 GPT 决策 |

**为什么第 1 步排在最前**：0002 第 6 问（生态血脉取舍）现在的答案是从依赖图推断的。
先编译一次，GPT 拿到的就是事实而不是猜测——决策质量差很多。
