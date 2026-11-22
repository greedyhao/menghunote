# Wiki 双链与关系图谱

> 2026-11-16 记录。**L1 已实现**（2026-11-17）：`[[...]]` 解析/投影/点击/
> 未解析灰色样式；**L2a 已实现**（2026-11-17）：links 物化 + 反向链接面板；
> **L2b 已实现**（2026-11-17）：笔记重命名联动改写解析到该笔记的 wiki 链接；
> **G0 + G1 已实现**（2026-11-17）：`GraphModel` 纯模型 + 全库 canvas 图谱覆盖层
> （侧栏「图谱」→ 全屏覆盖层；平移/缩放/悬停高亮/点节点打开笔记）。细节见下节
> 「G0+G1 已实现」。
> 图谱是 `links` 表的派生视图，不改同步协议、不改 schema。

## L1 已实现（2026-11-17）

- `crates/markdown` 自定义 inline pass：单行 `[[target#heading|alias]]`，排除代码块/
  行内代码/转义/`![[...]]`；产出 `WikiLink` 元数据 + 投影构造；
- editor 把 `WikiLink` 当行内构造（alias 时隐藏 target/`|`），app 单击打开；
- 目标解析：路径精确 > 标题（大小写不敏感）> 最短路径；歧义/无匹配只提示，不自动建；
- unresolved 链接渲染为灰色波浪下划线（`StyleKind::WikiLinkUnresolved`）；
- 回归：`crates/markdown/tests/golden.rs` 3 个、`crates/editor/tests/wiki_links.rs` 2 个、
  `crates/app/tests/headless_wiki.rs` 2 个。

## L2b 已实现（2026-11-17）

- `crates/storage/src/rename_links.rs`：
  - `plan_wiki_link_renames` 纯函数：按解析结果只规划 target 子区间，四种写法 +
    alias/heading/路径前缀/`.md`/末尾 `/` 逐字保留；代码/行内代码/转义/`![[...]]`
    天然不在解析结果里；`linked_targets` 逐字比对 `links.target`，未解析/同名歧义不误改；
  - `link_sources_for_note`：从 `links`（`kind='wiki'` 且 `to_note_id` 非空）取来源与
    target 集合，不扫正文；
  - `rewrite_link_sources`：逐篇 `SessionDriver::open → EditRequest → apply → close`
    （有界等 durable + B 物化）；一篇一次正文提交，跨篇不原子；失败只计数不回滚。
- App `commit_rename`：结构重命名**成功后**才联动（仅笔记，目录不改）；非活动来源走
  storage 逐篇会话；**活动笔记自身是来源时走活动 `EditorView` 的同一个 driver**
  （`rewrite_wiki_link_targets`），另开会话会用旧快照覆盖在途编辑；改写后 flush(Manual)
  并等该 note 的 B 物化，`links` 自然刷新；notice 显示「更新 X 篇 / Y 条链接，失败 Z 篇」。
- 回归：`crates/storage/tests/rename_links.rs` 8 个（纯函数 6 + 执行 2，含故障注入）；
  `crates/app/tests/headless_wiki_rename.rs` 2 个（活动来源不 stale 覆盖 / 活动目标后切换）。

## 前置：Wiki 双链

- 语法子集：`[[Note]]`、`[[Note|alias]]`、`[[Folder/Note]]`、`[[Note#Heading]]`、`[[#Heading]]`；
- 暂不做 `![[...]]` 嵌入、`^block-id`、跨 vault、自动创建；
- 解析在 `crates/markdown` 做原始 source 的 inline pass（排除 code/fence/转义）；
- 投影隐藏 `[[`/`]]`，alias 时隐藏 target/`|`，caret 进入 marker 显现源码；
- 点击打开目标；unresolved 灰色虚线，不自动建笔记；
- 解析规则：路径精确 > 标题（大小写不敏感）> 最短路径；仍冲突则 unresolved；
- **重命名必须联动改写正文里的旧链接**（已实现 L2b，2026-11-17）；详见下节；
- 现有 SQLite `links(from_note_id,start_byte,end_byte,to_note_id,target,kind,body_seq)` 已就绪：
  - 正文物化 B 事务里写入，`kind='wiki'`；
  - `to_note_id` 按当前结构树解析，解析不到为 `NULL`；
  - `links` 是本地派生投影，不同步，远端物化/结构变化后重建；
- 反向链接：`WHERE to_note_id=?` + join 来源标题，右侧/底部面板。

## 重命名联动改写（已决）

目标：笔记标题从 `旧名` 改为 `新名` 后，所有**解析到该笔记**的 wiki 链接源码一起改，
不留长期旧文本。

### 改写范围

- `[[旧名]]` → `[[新名]]`
- `[[旧名|别名]]` → `[[新名|别名]]`（别名逐字保留）
- `[[旧名#标题]]` → `[[新名#标题]]`（heading 逐字保留）
- `[[文件夹/旧名]]` → `[[文件夹/新名]]`（只改 basename，保留原有路径形式）
- 只改写 `links.to_note_id = 被重命名笔记` 的行；即“当前确实解析到该笔记”的链接
- 代码块/行内代码/转义 `\[[` 里的内容不改
- 未解析的同名文本默认不改（无法确认意图；用户可手动处理）
- 同名歧义导致 `to_note_id` 为 NULL 的链接不改，并在重命名结果里报告数量

### 执行顺序与一致性

1. 结构树重命名（先让 `note_id` 保持稳定；`links.to_note_id` 仍指向它，可用于找链接）；
2. 按 `links` 行逐篇重写正文：只替换 target 子区间，alias/heading/括号/其它字节不动；
   - 每篇正文一次事务，走与普通编辑相同的存储/同步路径；
   - 当前打开的笔记必须经活动 driver 或先持闸，不能绕过它直接写导致 pending overwrite；
3. 物化/重建受影响笔记的 `links`（`target` 更新，`to_note_id` 仍指向该笔记）；
4. 返回统计：改写篇数/链接数、跳过（未解析/代码）数、失败篇数。

### 失败与重试

- 结构重命名成功、正文改写失败时**不回滚重命名**；保留可重试的本地任务/报告，
  下次打开或显式「重试更新链接」继续；
- 每篇改写幂等：重复执行不会把 `新名` 再改成别的；已改过的链接自然匹配不到旧 target；
- 不做跨多篇的单一原子事务（SQLite/CRDT 不提供跨文档事务）；接受“部分完成 + 可恢复”，
  但 UI 必须显示未完成数量，不能假装全部成功。

### UI

- F2/重命名确认时显示「将同时更新 N 条链接」；
- 执行中显示进度（篇/条），可后台完成；
- 失败时提示「结构已重命名，N 篇链接未更新」+ 重试；
- 重命名对话框里不做“是否更新”选择（已决：默认更新）。

### 已知限制

- 跨多篇改写不进入单一撤销项；各篇仍按普通正文 undo 管理；
- 活动笔记里**尚未物化到 `links`** 的新键入链接（同一会话内刚写、还没 flush）不会被
  自动改写——L2b 以 `links` 行为“确实解析到该笔记”的判据，避免误改同名/歧义链接；
- 失败目前只报告篇数（notice + stderr），没有持久化重试任务/「重试更新链接」按钮；
  重命名本身已提交，后续可再做显式重试入口；
- 改写同步发生在重命名提交之后，来源篇数很多时会阻塞 UI 若干毫秒/篇；
- 远端并发编辑由 CRDT 合并；同一篇的改写若与远端冲突，以正常合并结果为准；
- 拖拽移动/改文件夹（非标题重命名）暂不自动改路径链接，后续单独定；
- 若 `links` 表尚未物化（L1 阶段），先扫描全库正文用解析器找链接，成本较高；
  L2 起优先用 `links.to_note_id`。

### 回归测试

- 四种语法写法 + alias/heading/路径逐字保留；
- 代码/转义不误改；
- 多篇/多链接、未解析跳过、重复执行幂等；
- 活动笔记打开时重命名不产生 stale 覆盖；
- 失败注入：某篇写入失败时结构已改、该篇可重试、其余篇不回滚；
- 远端并发编辑合并后 `links` 与正文一致。

## G0+G1 已实现（2026-11-17）

写作用域：`crates/app/src/graph.rs`（新）、`crates/app/src/shell.rs`（覆盖层与入口）、
`crates/app/src/editor/theme.rs`（配色）、`crates/app/tests/headless_graph.rs`（新）。
**没动** `crates/storage`/`editor`/`markdown`/`sync`，没加依赖，没改 schema。

### 模型口径（`crates/app/src/graph.rs`，测试钉住）

- 节点 = 未删除笔记：`tree t JOIN notes n ON n.note_id=t.note_id
  WHERE t.deleted=0 AND n.deleted=0`（`vault.query` 直读，不新增 storage API）；
- 边 = `links WHERE kind='wiki' AND to_note_id IS NOT NULL`；
- **自环排除并计数**：`[[#标题]]` 解析到自己 → `self_links`，不成边、不进度数；
- **未解析不画边**：`to_note_id IS NULL` → `unresolved_links`（只统计）；
- **悬空只统计**：`to_note_id` 指向已删除/不存在的笔记 → `dangling_links`；
  来源已删除的 `links` 行整行跳过（不参与任何统计）；
- **去重 + 权重**：同一对 `(from, to)` 的多条链接合并成一条 `GraphEdge`，
  `weight` = 条数；`A→B` 与 `B→A` 是两条不同的边；
- 节点字段：`note_id/node_id/title/path/folder/depth/in_degree/out_degree/pos`；
  `path` 按结构树父链拼（`文件夹/子文件夹/标题`），`folder` 取顶层文件夹（根级空串）；
- 顺序固定：`(folder, path, title, note_id)` 升序；边按 `(from, to)` 升序；
  邻接表升序——同一输入两次构建结果逐字节一致（输入行顺序也不影响）；
- **确定性布局**：排序后铺在圆周上（第 i 个节点角度 = `-π/2 + 2π·i/n`，
  半径 = `max(96, n·76/2π)`，单节点在原点）——无随机、无时钟、不落库；
- **上限**：`max_nodes = 800` / `max_edges = 3000`（`GraphLimits::default()`）；
  节点取排序后的前 N 个（前缀），边取 `(from, to)` 最小的前 M 条；
  截断时 `truncation` 给出"整库总数 / 实显数"，头部提示写清楚；
  度数只用保留下来的边算（画面与数字一致）；
- 只计算榜：`unresolved_links / self_links / dangling_links` 都是统计，不进画面。

### 视口（绘制/命中/测试共用）

`GraphViewport { pan, zoom }`：`screen = 视口中心 + pan + model·zoom`。

- `screen_pos` / `model_pos` 互为逆运算；**绘制节点、画边、命中、headless 断言
  全走它**，没有第二份几何；
- 缩放缓动有界 `0.2–3.0`；`zoom_by(factor, anchor, viewport)` 保持锚点下的
  模型坐标不动（滚轮缩放围绕鼠标）；缩放为 0 不会出现（除零防护）；
- `fit_to` 自适应：`usable = min(w,h) - 2·72`，`zoom = usable/(2·半径)`，
  夹进 `0.2–3.0`；空图/单节点（半径≈0）取 `zoom = 1`，不 panic；
- `hit_test`：屏幕点到节点圆心的距离 ≤ `节点半径 + 4px`，后画的节点优先（重叠时上层胜）。

### App 视图

- 入口：侧栏底部「图谱」按钮（`sidebar-graph`），位于 `AppRoot` 的「⚙ 设置」
  （绝对定位 `bottom: 30`）**正上方**；dev-sync 构建预留 40px 底部留白，两个入口不重叠；
- 覆盖层：全屏 `absolute inset_0`（不挤压编辑器）——半透明底 + 舞台 `graph-canvas`
  （`occlude()`：挡住下层编辑器的鼠标命中，点图谱不会误移 caret）；
- 自绘：边用 `gpui::canvas` + `PathBuilder::stroke` + `window.paint_path` 画；
  节点 = 绝对定位圆点（`rounded_full`）+ 固定宽度单行省略标签（无 hitbox，
  命中统一由舞台的 `hit_test` 决定）；
- 交互：拖拽平移（>4px 才算拖拽，抬手不算点击）、滚轮缩放（围绕鼠标、有界）、
  悬停高亮节点 + 邻居（其余压暗）、点节点打开笔记（按 `note_id` 找 tree 行 →
  关覆盖层 → 复用 `open_entry`）、点空白（遮罩）关闭、`Esc` 关闭、「关闭」按钮关闭、
  「复位」按钮重新自适应；
- 焦点：打开时抢到覆盖层（Esc 生效），关闭时还给当前编辑器——编辑器文档/滚动/
  撤销状态原样保留（覆盖层只挂渲染层，不重建 `EditorView`）；
- 头部统计行：`节点 N · 边 M · 未解析 K`（+ `自环/悬空`，+ 超上限时
  `只画前 X 个节点 / Y 条边（整库 A / B）`）。

### 性能与已知限制（首版）

- 模型在**打开时**同步构建一次（只读 SQL + O(n log n) 排序 + O(E) 去重），
  800 节点/3000 边量级是毫秒级；关闭即丢弃，不缓存、不落库（G3 再做）；
- 每帧重建节点层（≤800 个圆点 + 标签）与边（视口裁剪后绘制）；悬停变化才重绘，
  但窗口缩放/其它状态通知也会重绘——大库交互的实测与优化属于 G3；
- **不做**局部 depth/过滤/搜索/LOD/力导向/拖拽改结构（G2/G3）；标签在低缩放时
  可能互相压住（固定 11px、不随缩放），节点重叠时命中取上层；
- 平移不设边界（可把图拖出视口，用「复位」找回）；缩放不重排（位置固定）；
- 布局只在打开时算一次：窗口放大/缩小后不重新自适应（节点仍按新视口居中）；
- 图谱不写库：不改 `links`、不建笔记、不做未解析占位节点。

### 回归测试

- `crates/app/src/graph.rs` 单测 15 个（纯模型，不建库）：空图/单节点/环/多边去重
  加权/自环排除/未解析+悬空统计/已删来源忽略/确定性布局与输入顺序无关/
  path-folder-depth/上限截断/命中与绘制一致/缩放下界与锚点/平移/滚轮因子有界/
  空模型不 panic；
- `crates/app/tests/headless_graph.rs` 8 个（真实 `AppShell` + 临时 vault，
  `CARGO_TARGET_TMPDIR`，无 sleep）：打开→Esc 关闭（编辑器状态不变）、
  点节点打开笔记并关闭、空图占位 + 点遮罩关闭、拖拽只平移不关闭 / 点空白关闭、
  「复位」「关闭」按钮、悬停高亮 + 缩放夹在 0.2–3.0（缩放后圆心仍命中）、
  未解析/自环只统计不画边、窗口尺寸变化后覆盖层尺寸跟手（命中坐标仍有限）；
- 命令：`cargo test -p menghunote-app`（默认 265 passed）、`--features dev-sync`
  （496 passed）；`cargo clippy -p menghunote-app --lib/--test headless_graph
  --features dev-sync -- -D warnings` 干净。

### 后续（G2/G3）

- 局部图（depth 1–3）、按文件夹/标签/未解析过滤、搜索定位、LOD、方向箭头；
- 大库性能（后台布局/主线程只读位置）、布局缓存（本地表或文件，另议）、
  暗色/DPI 验收；
- 待定问题 1–5 仍未拍板（默认全库图已按"临时约定"落地，G2 再定默认视图）。

## 图谱目标

### 数据

- 节点 = 笔记（`note_id`），不是正文块；
- 边 = 已解析 wiki 链接（`to_note_id` 非空）；unresolved 边可另用虚线/灰色目标节点表示；
- 全量图 / 当前笔记局部图（depth 1–3）/ 按文件夹、标签过滤；
- 可选：反向边、孤立笔记开关、未解析链接开关；
- 图模型从 `links` + 结构树构建，不读正文；增量刷新用 body/structure 版本，必要时全量重建。

### 渲染与交互

- GPUI 自绘 canvas（不引 WebView/JS）；节点、边、标签分层绘制；
- 第一版确定性的静态布局（按文件夹/标签分组或 seeded force），不做随机抖动；
- 平移/缩放；点击节点打开笔记；悬停高亮邻居；搜索定位；
- 节点大小可按度数（入链+出链），颜色按文件夹/标签；标签按缩放级别 LOD；
- 布局位置是**本地缓存**，不同步；图表编辑（拖拽改结构/链接）首版不做；
- 深色模式、高分屏 DPI、空图/单节点占位。

### 性能预算（首版）

- 目标：全库 ≤ 5k 笔记 / 20k 边可交互；局部图默认 ≤ 300 节点；
- 力导向布局在后台计算，主线程只读位置；计算有界/可取消，切笔记不阻塞；
- 大图 LOD：低缩放不画标签/细边；视口裁剪；
- 退化路径：节点/边超上限时只显示局部图或提示过滤，不卡死。

### 切片

- **G0** ✅（2026-11-17）：`GraphModel`（节点/边/度数/过滤）+ 纯单元测试（含 unresolved、孤立、环）；
- **G1** ✅（2026-11-17）：全库 canvas + 确定性布局 + 点击打开 + 悬停高亮 + 平移缩放；
- **G2**：局部图 depth、过滤（文件夹/标签/未解析）、搜索定位、LOD；
- **G3**：大库性能、后台布局、布局缓存（本地表/文件另议）、暗色/DPI 验收。

### 待定问题

1. 默认打开全库图还是当前笔记局部图？（建议局部图，全库作为“打开完整图谱”）
2. unresolved 是否画成节点？（建议虚线边 + 灰色占位，不进入正式节点计数）
3. 边是否显示方向箭头？（建议默认无箭头，悬停/筛选时显示）
4. 是否允许在图谱里拖拽改层级/连线？（首版只读，避免隐式改正文）
5. 布局位置缓存是否落 SQLite 本地表？（首版内存缓存，G3 再定）

### 前置依赖

- 双链 L1（解析/投影/点击）与 L2（links 物化/反向链接）；
- 结构树稳定标题/path 解析；
- 后续如需布局缓存，单独评估本地派生表，不塞进同步 schema。
