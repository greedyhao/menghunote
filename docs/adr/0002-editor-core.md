# ADR 0002 — 编辑器内核：源码投影编辑器（Typora 式原位 WYSIWYG）

- **状态**：已采纳（2026-11-09，`bluetrum/gpt-6.1-sol`，`reasoning_effort: max`）
- **输入**：[`0002-editor-core.md`](../briefs/0002-editor-core.md)（含附录 A–E 的实测事实与拟定裁断）
- **前置**：[ADR 0001](0001-storage-format.md) 的存储/同步契约不变，本 ADR 必须与之自洽
- **影响范围**：`crates/editor`、`crates/ui`（外壳）、`crates/core`
- **阻塞性**：本 ADR 第 11 节 **S0** 通过前，禁止并行堆 UI 控件

---

## 1. 背景

编辑体验对标 Typora：**渲染视图就是编辑界面，Markdown 语法标记在非编辑态不绘制、光标进入时显现**。
ADR 0001 已把持久化真源定为 SQLite 单库 + 每篇一个 `LoroDoc`/`LoroText`（内容为原始 Markdown），
因此编辑器必须做到：**编辑的就是 Markdown 源码本身**，显示只是投影。

方案取舍：

- **A. 源码投影编辑器**（逐行/逐块自建映射层）—— 最接近 Typora，成本最高；
- **C. 分层递进**（v1 只做块级容器渲染 + 行内纯文本）—— 最省力，但**不是 Typora**；
- **D. 分栏预览** —— 过渡形态。

实测事实：GPUI 不自带编辑器；`gpui-component` 与本基线不同血脉（见附录 §C）；
`gpui-testing-library 0.1.0` 钉死 `gpui-pre =0.3.9`，不可用（见附录 §A）。

---

## 2. 决策

**选 A 的「源码投影编辑器」，但不采用「一行一个独立编辑控件」**：
按**语义块**组织布局，块内生成**视觉行**；整篇只有**一套** source selection、命令入口、
IME handler 与撤销域。

- **C 只作为内部 v0/alpha 实现阶段**；**「行内标记始终显示」不得冒充产品 v1**。
- **v1 必须实现**标题、粗体、斜体、行内代码、引用、列表的**默认隐藏与进入显现**。
- **D 不做常规过渡产品**；源码切换只作为**同内核的调试/降级模式**。
- **保留已验证的** `gpui-unofficial 1.27 + gpui-platform-gpui-unofficial 1.27.0`，
  不为外壳组件或测试库换血脉。

### 2.1 不可逆契约（写进代码前必须冻结）

| 类别 | 内容 |
| --- | --- |
| **产品/数据契约（不可逆）** | Markdown/LF 真源与 SQLite/Loro 权威不变；**所有真实编辑都是"有当前基准的区间操作"**；块树、显示串、widget 状态都只是派生；坐标单位不得混用；source selection 与 CRDT 相对锚点是持久编辑语义；撤销**只撤本地操作**，不回退 VV、不恢复全篇快照覆盖远端；隐藏/显现/滚动/组合态**不进正文与撤销栈** |
| **重写级成本，v1 冻结** | 单文档串行修改入口；单编辑器焦点/IME；`Source → 临时输入文档 → 逻辑显示投影 → 视觉布局` 的分层；所有绘制、命中、选区、IME 几何共享**同一个 `LayoutFrame`** |
| **可后补/可替换** | Rope 具体版本、解析器实现、`Vec → 区间树` 优化、缓存阈值、主题、代码高亮、表格/公式/高级图片、完整 RTL 富编辑。GPUI 血脉**不是磁盘格式上的不可逆项**，但**不在 v1 中途反复切换**；将来整仓切换也只能更换 UI/平台适配层，不得污染纯内核 |

---

## 3. v1 边界与降级

**必需能力**：中文/Latin/emoji、grapheme 光标与删除、软折行、单光标跨块连续选择、
原文复制/粘贴、中文 IME、增量远端 delta、协作安全 Undo/Redo。

| 构造 | v1 行为 | 降级 / 说明 |
| --- | --- | --- |
| ATX H1–H6、粗体、斜体、行内代码、常见嵌套 | 阅读态有真实样式且**隐藏已确认标记**；进入构造显现**成对标记及必要祖先** | — |
| 引用、有序/无序及常见嵌套列表 | 引用条/缩进/列表标识由 source 派生；编辑前缀时源码可见；Enter 续项、空项退出、缩进/反缩进**只产生局部 op**，不整段格式化 | — |
| fenced code（```/~~~） | 等宽 + 底色，阅读态围栏不绘制；编辑该块时显现围栏 | v1 **不做**多语言高亮，降级纯等宽（不影响编辑/复制） |
| 分隔线 | 阅读态画线，进入后显示 Markdown 源行 | — |
| 简单行内链接 | 显示标签，编辑时显现完整语法及目标；**普通点击用于编辑**，跳转另用显式手势 | — |
| 本库 `asset://` 图片 | 静态缩略图/缺失占位，进入时用**同一个内核**编辑引用源码；外框预留固定尺寸 | 不做裁剪/拖动缩放/图片内部光标；远程图片不自动下载，显示占位与原引用 |
| 表格 | 整块显示**等宽 Markdown 源码**，可编辑、可跨块选取 | v1 **不做**网格 widget、单元格导航、增删行列 |
| 数学公式 | 不做排版，`$…$`/`$$…$$` 原文可编辑，块公式等宽显示 | — |
| raw HTML、脚注、引用式链接、wiki 链接、Setext 标题、未验收扩展/歧义语法 | **保留并显示源码**，不执行 HTML，**不偷偷改写/删除** | 这是"保守源码降级"的默认落点 |
| HTML 富剪贴板 | 只取 plain-text 部分并 LF 归一 | 不保留外来富格式 |
| 多光标、矩形选择、Vim 模式 | 后补 | v1 单光标/普通跨块选区 |
| 完整 RTL 富编辑 | 后补；RTL 段落**先源码模式**，必须保留原 Unicode 顺序 | 基础 shaping/命中未过验收则**明确只读**，不装作支持；映射架构现在就保留 bidi run 与双 caret affinity |

**体积上限**沿用 ADR 0001：正文 ≤10 MiB、单行 ≤1 MiB，**不截断**。
过大语义块先降级为源码；病理长行**关闭软折行 + 水平滚动 + 分片缓存**；超上限只读。
阈值可调，**不能靠改变显示模式掩盖数据丢失**。

---

## 4. 投影层（ProjectionMap）

**不用**"删掉若干字符后的字符串 + 一个偏移差"，改为**每块一个 `Vec<Segment>`**。

段记录：块本地 `source/input` 区间、逻辑显示 byte 区间、`kind`、所属构造、边界策略。
块前缀长度索引负责全篇偏移——避免"输入一个字就改后缀所有绝对 span"。

- 段类型至少：`Copy`、`Elide`（source 非空 / display 为零）、`Replace`（转义/换行/显示替代）、
  `Object`（widget 对应完整源码范围）。
- **preedit 用独立 `OverlayMap`**，不能塞成 CRDT 正文段。
- **source 区间必须完整覆盖**：包括隐藏标记、围栏、LF。
- 显示索引始终指**逻辑显示串**；不能拿屏幕 `x` 当它的顺序。

**坐标强类型**，互不隐式转换：

`SourceByte` / `UnicodeScalar` / `InputUtf16` / `DisplayByte` / `VisualCaret`

- byte↔scalar↔UTF-16 转换由 **Rope chunk 统计/前缀索引**完成，**不每次从文首扫描**；
- **grapheme 是交互边界，不是 CRDT 索引**；不做 NFC/NFD 归一。

**隐藏区间的多对一问题（Elide）**：

- 多对一 → 隐藏内部 source 点**没有唯一逆像**，必须带**边界 bias**；
- 鼠标默认选相邻可见内容一侧；键盘进入隐藏源码范围时**先显现**，让标记可达；
- 折行 upstream/downstream、bidi 强弱/leading/trailing affinity、与"源码前后黏附"的 CRDT bias
  **必须分开**，不能用一个 `Left/Right` bool 代替。

**显现（Reveal）规则**：

- 折叠光标揭示它所在的行内构造及**必要祖先**；
- 非空选区揭示端点切入/部分覆盖的构造；**完全包住的中间构造仍可保持渲染态**
  （`Ctrl+A` 不把全篇重排为源码）；
- 块级对象进入编辑时转源码态；
- 新旧 `RevealSet` 做**差集**，只重建受影响块的 map；选区变了但 `RevealSet` 没变时**只重画光标/选区**；
- **拖拽期间固定显现集合**，不随 head 来回开合；
- **IME 期间固定所在构造的显现及解析解释**，preedit 按字面显示，结束后再解析。

---

## 5. 布局、命中、拖拽

- **语义块** = 排版/缓存/高度失效单位；**视觉行** = `shape_line`、光标、选区、命中单位；
- **源码 LF、Markdown 软/硬换行、视觉软折行不能混为一件事**：软折行不写入正文，
  显示替代的换行必须进映射段；代码行默认不软折行；
- 每个 `LayoutFrame` 携带 `source/input/projection revision`；视觉行保存逻辑显示范围、
  真实 glyph cluster / caret stops、视觉 run、基线与几何；
- GPUI 负责 glyph shaping；**编辑器驱动折行、多行与块装饰**；不按"字符数 × 平均宽度"算位置，
  不切开 emoji/grapheme/shaping cluster。

**唯一坐标路径**（正向/逆向必须同一条链）：

```
GPUI 窗口逻辑坐标
  → 文档坐标（扣 inset、加 scroll）
  → 高度索引中的块
  → 视觉行
  → visual run / cluster / caret stop
  → 逻辑 display 点
  → input/source 点 或 preedit 内部点
  → 当前 CRDT 相对锚点
```

逆向绘制光标、选区、IME 候选窗沿同一条链反走。DPI/屏幕原点**只在平台边界转换一次**；
GPUI `bounds_for_range` 要窗口坐标，不要自己重复换成屏幕物理像素。

**命中**：

- 使用**最近实际绘制的 Frame**，而不是未显示的新布局；
- Frame 基准落后时只能"用有界 delta 日志转换到当前版本"或"重画再命中"，
  **绝不把旧 byte 当当前 byte**；
- `mousedown` 先在**旧 Frame** 命中得到 source 点，**再**显现/重排；
  不重排后拿同一个像素**第二次命中**改变目标；
- 拖拽 `anchor` 固定相对锚点，`head` 每次走同一路径。

**RTL**：独立 logical↔visual run 映射；selection 保存逻辑 source `anchor/head`，
视觉上可画多个不相邻矩形；逻辑偏移不必随 `x` 单调；同一 source 点可有多个视觉 caret，
**不能盲信一次 `x_for_index` / `closest_index_for_x`** 覆盖所有 bidi 边界。

**高度稳定**：

- 固定字号级行高、测量缓存、高度前缀树、可见区 + overscan 虚拟化、固定图片外框、source 滚动锚点；
- 显现标记可能增加折行，**数学上不能承诺"高度完全不变"**；
- 允许当前块局部重排，但**一次发布布局**；caret 可见时保持其 source 锚点的屏幕 `y`，
  否则保持首个可见 source 点 + `y` 偏移；
- 拖拽/IME 禁止反复切解释；
- 长行需**分片形状缓存/局部失效**，不能"全行 shape 再裁剪"后声称虚拟化。

---

## 6. 增量解析与缓存

- 运行时选 **`tree-sitter-md 0.5` 的 block + inline 双 grammar** 做**增量派生**；
  版本固定，解析器**不直接暴露为持久模型**。
- 其 README 明确**不保证 CommonMark 完整语义正确** → **只隐藏已通过语法/源码 delimiter 验收的构造**；
  错误、歧义或解析滞后时，**dirty 区域保守源码显示**。
- `pulldown-cmark` 作**规格/差分测试参照**，**不把两套 parser 都塞进每次输入热路径**。
- 每个真实 delta 同步驱动 **Rope** 与 **tree-sitter `InputEdit`**；
  以旧树增量解析新 revision；`dirty = 直接编辑范围 ∪ changed_ranges ∪ 必要边界邻居/容器上下文`；
  **不能只看 `changed_ranges`**（普通文本变化可能不改变语法树形状）。
- 列表/引用缩进影响 enclosing container；围栏修改可影响文档尾部——**允许合法扩大，不保证永远只重算一块**；
  引用定义等跨块依赖留**显式反向依赖索引**，将来启用引用渲染时按依赖失效。
- **三层缓存 key**：
  1. 语法：块内容 revision + 容器 context revision + grammar/dialect；
  2. 投影：语法 key + 该块 `RevealSet` + overlay revision + source/reading mode；
  3. 布局：投影 key + 宽度/缩进 + 字体/字号/DPI epoch + 影响度量的 widget 状态。
  光标闪烁、颜色、**不改变 RevealSet 的选择**不进布局 key；**文档全局 revision 不作为每块缓存失效键**。
- 只计算变化块 hash；块 ID 会话内稳定，分裂/合并重分配，**不落库**；
  后台结果带 revision 校验，**旧结果不能覆盖新 source**。

---

## 7. IME

- **整篇 Editor Entity 实现 `EntityInputHandler`**，自定义 `Element` 的 paint 注册对应
  `ElementInputHandler`；**不是每块一个 handler**。
- 平台看到的统一是**「虚拟输入文档 I」**：当前 Markdown source 在原选择范围用 preedit 暂时替换，
  坐标全是 **I 的全局 UTF-16**。它不是隐藏后的 display 串，也不是已提交的 Rope。
- `Composition` 保存：id、当前基准、原始 source 选择/相对锚点、preedit、marked range、
  preedit 内选择、显现锁。
- `replace_and_mark_text_in_range` **只修改 overlay**；
  `selected_text_range` / `marked_text_range` / `text_for_range` / `character_index_for_point` /
  `bounds_for_range` **全部指同一个 I**。
- 读范围必要时调整到合法 scalar 边界并回报 `adjusted_range`；**非法代理对写范围不能悄悄截 byte**。
- 确认时以**当前基准一次 `delete + insert`**，作为一个 Undo 组；取消仅撤 overlay。
- **`unmark_text` 不是「必然取消」的同义词**；确认/取消/失焦/点击外部等真实 Windows 回调序列
  **须各有夹具**，防止零次/两次提交。
- 键盘输入走文本服务，**不能 keydown 再插一次**；组合中的 `Enter`/`Esc`/方向键先交 IME，
  不触发列表/格式化/Undo。
- 候选窗由**同一 `LayoutFrame`** 的 marked-selection/caret 视觉矩形给出；零长度范围用正确 affinity；
  跨行范围按平台 first-rect 语义，**不返回整块 union**；滚动、折行、窗口移动/DPI 变化时更新
  原生文本/选择与几何失效通知。
- **v1 在 composition 期间对该 Doc 的真实 source mutation 加写锁**：不仅**延后远端 import**，
  也**延后同 Doc 的其它真实正文改动**（后台 AI/MCP 替换、非组合态命令、Undo/Redo 等）；
  **用户主动发起的命令必须先完成平台的 commit/cancel 再执行**。
  否则即使冻结了远端，另一个本地写入者仍会让原替换区间失效
  （"解析当前锚点 → delete 整个新范围"可能删掉刚插入的字符）。
- 该锁的边界**必须精确**：锁的是**该 Doc 的真实 source mutation**；
  **不锁** SQLite writer、**不锁其它 Doc**、**不锁滚动/窗口响应**、**不锁 preedit 更新**。
  网络队列到达界限时**暂停该 Doc 消费**，**不得**自动取消中文输入。
- 结束后**立即串行合并**并恢复相对锚点；**v2 才做**能保留并发字符 ID 的 composition 重基。
- 点击 preedit 内部得到 `PreeditPoint`，只调整组合选择；点击外部/拖拽跨出组合态，
  **先完成平台的 commit/cancel 流程**，再处理 source 选择，**不同时维护两个插入点**。
- 切换/关闭也先解决 composition，再 flush 已提交内容；**不能把 preedit 当已保存**。

---

## 8. 复制与撤销

**复制**：

- `Ctrl+C`/`Cut` 默认**严格的 source 半开区间切片**，包含范围内隐藏字符；
  **不自动补齐/平衡 Markdown**，部分选择可能带不成对标记；
- 整篇复制保持 Markdown 原文（LF 已归一）；
- 另提供**「复制为纯文本」**：从**与显现状态无关的阅读投影**取文本，不能直接复制当前屏幕串；
- 粘贴只产生**一次**当前基准区间命令；**未提交 preedit 不能偷偷进剪贴板/存储**。

**撤销**：

- 每正文 Doc 会话用 **Loro 1.17.1 `UndoManager`**，绑定当前本 peer；
  **唯一正文 Undo 栈由该串行 bridge 驱动**，不用第二套 byte 逆操作栈/文本快照栈；
- remote import **不成为本地 Undo 项**，但**要让 manager 处理**以变换既有 Undo/Redo；
  **不要手工清空全部历史**；
- 后台维护/编辑戳等 origin **排除**；用户授权粘贴/格式化/AI 替换**明确作为本地用户组**；
- **关闭自动时间猜测**，按内核事务显式分组：连续同方向输入约 **750 ms**；
  光标移动、选择变化、粘贴/格式命令、IME commit、remote import **打断**；每次 IME commit **单独一组**；
  **250 ms durable flush 不决定 Undo 分组**；
- Undo/Redo **生成新 CRDT op**，走相同 delta/落库/ACK；**不 checkout/revert VV**；
- `selection_before/after` 只保存相对锚点元数据，显现状态重算；
- 被远端删除导致 Undo no-op 时**消费该项并提示**，**不自动穿透**撤更早一项；
- **v1 有界（建议 100 用户组）、会话内撤销**；重启或会话卸载后**不承诺恢复 Undo 栈**，
  降级为版本回看另存恢复。

---

## 9. 编辑器—存储最小接口（`EditorDocumentPort`）

暴露：打开快照、`apply_edit`、创建/解析相对锚点、`history`、`flush`、订阅事件。
**跨边界不传 GPUI 类型/块树。**

| 消息 | 关键字段与规则 |
| --- | --- |
| `OpenSnapshot` | `doc_id`、`generation`、`session_epoch`、`live_rev`、live frontier/VV、durable head/seq、LF Markdown；**全量正文仅打开/故障恢复允许**，远端热路径禁用 |
| `EditRequest` | `doc/generation/session`、幂等 `command_id`、`base_live_rev/frontier`、`origin`、`undo_group`、Unicode scalar replacements。**所有半开区间统一相对 request 基准**，排序且不重叠，按**逆序**应用；不能一会儿原始坐标一会儿逐 op 坐标 |
| `Applied` / `LiveDelta` | `from_rev → to_rev`、当前 live frontier/VV、带明确单位/基准的增量 splice、`cause_command_id`、来源 `local/remote/undo/redo`、`MergeInfo`。**local echo 不再重复应用 Rope**。并发标志由导入前 VV 与 update 因果信息判定，**不把任何远端消息都叫冲突** |
| `StaleBase` / `Rejected` | 当前 `live_rev` + 可用补偿 delta；**过期范围不应用**，**不以 durable head 冒充当前 live 基准**；锚点只能帮助重定位，**不能**无条件把过期删除范围扩大覆盖他人新文字 |
| `DurableAck` | `durable_batch_id/seq`、确认的 command/prefix 水位（`through_live_rev`）、持久 head frontier/VV。**只在 WAL durable commit 后发**；批次号、编辑 `command_id`、`live_rev`、frontier、`relay_seq` **是不同概念** |
| `Flush` | `reason` = 固定 250 ms / 64 KiB / Ctrl+S / 切换 / 关闭；`target_live_rev`；返回**异步 ticket**；关闭保留会话并异步等 ACK/处理错误，**不阻塞 UI 线程等 SQLite** |
| `HistoryState` | `can/count undo/redo` 与变更事件；Undo/Redo 也要求当前 `Doc/generation/base` |
| `SaveError` / `SyncStatus` | 本机 durable 错误与网络待同步/离线/relay durable ACK **彼此独立** |

- **唯一 live `LoroDoc` owner** 放在文档会话 bridge 的**串行 reducer**；
  打开的编辑会话 v1 可在 GPUI 主线程做**短内存区间操作**；**Rope 是镜像，不能自立权威**。
- local/remote/history 都经此入口：**验证基准 → 改 Loro → 同一次事件改 Rope/锚点**。
- **统一写入口 = 该 Doc 的 source mutation 闸门**：composition 期间对**所有来源**关闭
  （remote import、Undo/Redo、AI/MCP 后台替换、非组合态命令）；**用户主动命令先走平台 commit/cancel**。
  闸门**不覆盖** SQLite writer、其它 Doc、滚动/窗口响应、preedit 更新。
  这就是 ADR 0001「本地与远端对同一文档串行化」在编辑器侧的落地形态：**显式的 per-Doc 写锁**。
  解析、图像解码/高亮、SQLite writer 在后台；**SQLite actor 接收增量 CRDT 数据，不等待正文物化，
  不反向写编辑器**。
- UI「已保存」= 当前所有真实编辑已被 durable 水位覆盖；旧批 ACK 后若还有新输入继续「保存中」。
  **物化 `body/chunks/links` 完成不是保存条件**。
  同步状态另列「已保存·待同步 / 同步中 / 离线」；**relay ACK 不等于所有设备已看到**；
  只有本地功能时标「已保存（本地）」。**preedit 标「输入中，尚未提交」**，不能单独声称画面里组合文字已保存。
- 提交维持**首次 dirty 固定 250 ms**，不尾缘 debounce。
  **必须只有一份可追加的未 ACK op journal**，writer 取**连续前缀**，ACK 裁剪，
  慢 I/O **合并 flush 请求并背压**；不能做"一个在途批 + 任意多个后继内存批"却仍声称只丢一个。
  **durable 事务批号 ≠ 未 ACK 内存 journal**；不承诺慢盘/断电下只丢 250 ms 输入。
  若把"一个批次"严格定义为 250 ms 封口不可变批，则慢 I/O 时**必须停止接受新写**，
  UI 仍响应而非等待 SQLite——不能无限无阻塞收写与该更强保证兼得。

---

## 10. 血脉与生态

- **保留现基线**，但理由**不是**"longbridge 不活跃"。最小外壳估计 `gpui-component` 可省 **2–4 人周**，
  消掉重复控件/主题/焦点样式工作的约一半以上；**但核心映射、IME、段落布局、CRDT 撤销节省约为零**。
  空脚手架当前迁移成本其实不高，**不以沉没成本作为理由**；选择当前基线是为了**冻结编辑器最敏感的
  已验证平台/API 组合**，而不是同时改变底层与内核。
- 截至 2026-11-09（crates 发行 UTC 日期）：unofficial 1.25.3=10/23、1.26.2=10/30、1.27.0=11/4；
  component 0.8.0=10/19、0.8.1=10/26、0.9.0=11/2。**两者都是近期约周级发布**，
  不能说 longbridge 停更，也不能说主干必然更有维护保障。
- `gpui-unofficial` 发布包明确：**非 Zed 官方维护、自动跟随 release tag、且 "Currently needs a maintainer"**；
  **发布桥风险必须承认**。longbridge 更贴近外壳组件需求，但引入 snapshot/facade API 差异与额外升级协调。
- **锁定同一血脉的发布族**，Windows 输入回归后再升级；**不尝试 alias / 跨 Entity 类型桥接**。
- `gpui-testing-library 0.1.0` **不直接采用**（精确依赖 `gpui-pre =0.3.9` / `gpui-kit =0.7.3`，
  不同血脉，连 component 0.9 的 0.3.11 都不是）；它主要观察 testable/accessibility facts、
  派发指针/按键，**本就不能替代** 本内核的 source↔display 与 Windows TSF/IME 验收。
  **不为它换血或维护大移植**；可借鉴其查询/事件设计。

---

## 11. 测试与阶段

### 11.1 六类测试（核心是"可确定的状态机"，不是只有截图）

1. **单位夹具**：`A中😀é` 应分别 **11 UTF-8 bytes / 5 scalars / 6 UTF-16 / 4 graphemes**；
   **注意 `é` 必须取分解形式**（`e` + U+0301）才是这组数字；预组合 `U+00E9` 是 **10/4/5/4**。
   两种形式都要进夹具，防的是将来有人"统一做 NFC 归一"把夹具悄悄改坏（ADR 不做归一）。
   再加 ZWJ 家庭 emoji、变体选择符、中文跨行、非法半代理对。
   Loro `insert/delete`、delta `retain/delete`、cursor 创建/解析**分别验实际 API 单位**；
   文档说默认 scalar 也**不免执行验收**。
2. **属性测试**：Segment 覆盖 source 不丢字、合法边界、显现/隐藏不改变正文、可见点 roundtrip、
   隐藏点按 bias 的等价类 roundtrip；随机嵌套/残缺 Markdown + 选区/拖拽/折行/bidi stops。
   **不能要求所有隐藏 source 点可精确反解**。
3. **差分/模型**：`String` 参照 vs Rope 区间操作；增量解析 vs 同 parser 全解析；
   supported Markdown 与 pulldown 参考语义；`Edit → Loro → delta → Rope → 导出原文` 一致；
   打开/保存不改空格、标记、换行以外的 Unicode 形式。
4. **两 peer 调度**：并发插入/删除/替换、local Undo 后 remote 字符仍在、redo 跨 remote import、
   no-op Undo、旧基准拒绝、重复 command 幂等、旧 parser/ACK 不能覆盖新状态；
   故障注入 A 事务、B 物化滞后、慢盘/保存失败/持续输入与单 journal。
5. **几何与平台**：mock shaper 几何快照 + 同血脉 `test-support` 事件测试 + **真实 Windows 窗口回归**。
   `test-support` 先编译门禁，不能把文档存在当可编译。微软拼音及至少一种常用 IME，
   DPI 100/150/200、多行 marked text、软折行边缘、emoji 前后、切换/失焦/关闭、candidate 位置、
   commit/cancel 各跑；**headless 按键不等于真正 IME**。
6. **埋点**：分别量转换/区间 apply/解析/投影/layout/绘制与**事件→下一帧 p95/p99**；
   1 MiB 普通笔记与 10 万字符单行分别测试，声明长行降级模式；
   保留 ADR 0001 的 **Windows 中文 emoji p95 ≤16 ms 为验收目标**，**不当已达事实**。
   每个可见特性当场在 Windows 验证。

### 11.2 阶段（单人全职 + AI 执行；不把 agent 数当倍速）

| 阶段 | 内容 | 粗估 |
| --- | --- | --- |
| **S0** | Unicode / Loro 区间 / 并发 / 重放 / durable 契约 | **2–3 人周**，先过 ADR 0001 第 2 步 |
| S1 | 单 Element 源码编辑、选择、折行、IME、Undo | 4–6 人周 |
| S2 | 只用粗体验证**真实隐藏/显现映射**、mouse/IME/scroll | 3–5 人周 |
| S3 | 增量块/行内构造、列表/code/hr/链接/图片 | 4–6 人周 |
| S4 | Windows 回归、大文档/长行、故障硬化 | 4–6 人周 |

**核心 17–26 人周**，预留 30–50% 不确定性约 **5–9 个月**；最小外壳另 **2–4 人周**，
可在接口冻结后并行。完整同步服务、检索、加密/配对**不算这笔内核预算**。

### 11.3 最容易失控的地方

1. **IME 确认/取消与窗口失焦回调**；
2. **隐藏边界/换行/bidi affinity 混用**；
3. 围栏/列表上下文越界失效；
4. 病理长行每键全 shape；
5. **把 SQLite 批次、Undo 组、`command_id`/frontier 混成一个编号**；
6. **两个 Loro owner，或本地 echo 重复 apply**。

**前两项未过垂直切片前，禁止并行堆 widget。**

---

## 12. 后果与已知风险

- **正面**：v1 就是真正的 Typora 模型（行内标记可隐藏），映射层从第一天起就是唯一坐标权威；
  IME 以虚拟输入文档为中心，中文输入不被"隐藏串"污染；撤销与 CRDT 协作语义明确（仅本地、可被远端变换）。
- **代价**：S0 必须先于任何 UI；v1 冻结"单文档串行入口 + 单 LayoutFrame"这类重写级结构；
  v1 不支持表格网格、公式排版、多光标、完整 RTL；RTL 与表格在 v1 是**源码可见**的体验；
  无头测试不能替代真机 IME。
- **已知风险**：tree-sitter-md 语义正确性不足（必须靠"只隐藏已验收规则 + 保守降级"兜底）；
  发布桥无人兜底（版本不可回改，修复等下一轮）；Undo 仅会话内，重启不恢复；
  高度"完全不变"不可达，只能保证局部重排与视觉稳定。

---

## 13. 一句话

**先把"有基准的 Unicode 区间编辑"做成可回归的最小链路**：
过 ADR 0001 第 2 步的 Loro/Rope/并发/ACK 门槛，再用 **`中😀é`** 做
Windows「命中 → 显现 → IME 一次提交 → Undo → durable 恢复」的垂直切片。
不要先堆外壳，也不要先堆全 Markdown。

---

## 14. 来源（实际 fetch，非搜索摘要）

- GPUI 测试库 manifest：<https://docs.rs/crate/gpui-testing-library/0.1.0/source/Cargo.toml.orig>
- GPUI 测试库范围：<https://docs.rs/gpui-testing-library/0.1.0/gpui_testing_library/>
- `EntityInputHandler`（1.27）：<https://docs.rs/gpui-unofficial/1.27.0/gpui/trait.EntityInputHandler.html>
- `InputHandler` UTF-16/窗口坐标/候选窗：<https://docs.rs/gpui-unofficial/1.27.0/gpui/trait.InputHandler.html>
- Loro 1.17.1 `UndoManager`：<https://docs.rs/loro/1.17.1/loro/struct.UndoManager.html>
- Loro 1.17.1 `LoroText`（默认 Unicode 与显式 UTF-8/UTF-16 API）：<https://docs.rs/loro/1.17.1/loro/struct.LoroText.html>
- `gpui-unofficial` 发布数据：<https://crates.io/api/v1/crates/gpui-unofficial>
- `gpui-component` 发布数据：<https://crates.io/api/v1/crates/gpui-component>
- unofficial 自动发布/需维护者说明：<https://docs.rs/crate/gpui-unofficial/1.27.0/source/README.md>
- `tree-sitter-md` 双 grammar 与语义准确性告警：<https://docs.rs/crate/tree-sitter-md/0.5.0/source/README.md>
  （服务实际显示 0.5.4 文档；**ADR 只固定 0.5 系列 + 最终已验证版本**，不假称 0.5.0 精确源码审计）

> 说明：本 ADR 记录的是**决策**，不是实测结果。编辑器尚未实现，"已验证"仅指上文列出的依赖/api 事实。
