# ADR 0019 — Markdown 解析器与附件/图片模型

> 状态：**已决**（2026-11-16，GPT 决策；S1–S3 实施）。
> 关联：ADR 0002（源码投影编辑器）、ADR 0006（sync v1，完整 assets 不做孤儿删除）。

## 1. Markdown 解析

- **选 B：成熟解析器负责语义，现有源码投影编辑器负责编辑与布局。**
  - 引入 `pulldown-cmark = "=0.13.6"`，`default-features = false`；
  - 运行时 Options 明确开启 `ENABLE_TABLES | ENABLE_TASKLISTS | ENABLE_STRIKETHROUGH`
    （`ENABLE_GFM` 不包含这三者，不能误用）；
  - 不换 `gpui-component`/`gpui-base` 的 rich-text Markdown 主编辑器，不引入 HTML renderer。
- 新增小型纯 Markdown crate（依赖解析器 + `text`），供 editor/storage 共用；
  storage 不反向依赖 editor，text 不承担 Markdown 语法。
- `into_offset_iter()` 只给源范围，不是无损 CST；必须有 span adapter 处理定界符、
  转义、实体、引用式链接、表格分隔符；禁止用 AST 重新序列化覆盖正文。
- 保持 `Copy/Elide`，正式实现 `Replace/Object`：可见原子映射完整源范围，端点有方向偏置；
  坐标统一 SourceByte，并正确转换 UTF-16/grapheme。
- 标记默认隐藏；caret 真正进入或文本选区相交时显现。未完成/未支持/定位不确定的构造
  原样显示；raw HTML 不执行；脚注/数学/alerts 后置。
- 列表按块树处理嵌套、松紧列表、续行、子段落及内部围栏；符号/编号独立 gutter，
  续行 hanging indent；度量/绘制/命中共用几何。
- Enter 同级、空项退出；Tab/ShiftTab 按 item 层级调整前缀；不整篇重编号/格式化。
  任务框点击只改 `[ ]↔[x]`，走同一正文撤销事务；IME 合成期间不触发结构动作。
- 图片/Mermaid 渲染块用 **Object Block**：独占段落按内容宽度等比显示，混排用原子缩略图；
  不擅自补换行/写尺寸 HTML。加载失败显示 alt/名称占位。
- **点击/选中对象块不直接展开源码**（Obsidian 式）：单击只选中块（描边/高亮）；
  悬浮显示块工具条（编辑、放大/预览、复制 Markdown；Mermaid 另有重新渲染）。
  只有点工具条「编辑」/`F2` 才进入源码模式，显示完整 `![alt](asset://hash)` 或
  fenced mermaid；`Esc`、点击块外或 `Ctrl+Enter` 退出并提交。
- 对象块内部不产生文本 caret/部分选区；Delete/Backspace 先选块、再删整个源跨度；
  复制对象得到完整 Markdown。键盘 Tab 可聚焦块，Enter 打开编辑器（与普通文本区分）。
- **文本选区与对象块相交时**：块以整体选中态显示（描边/底色），不展开源码，也不允许
  只选中块内某段源码；复制时按完整 Markdown 源跨度还原到剪贴板。移动 caret 跨过块
  同样不展开源码。只有工具条「编辑」/F2 才切换该块到源码模式。
- 普通 fenced code（非 mermaid）仍按行内代码编辑；只有图片和 Mermaid 走对象块。
- 表格也是 **Object Block**：按 GFM 行列布局与横向滚动，**选中/光标进入都不显示 `|` 源码**；
  单元格就地编辑（只显示 cell 文本），不进入整表源码模式；需要时工具条提供「编辑表格源码」。
- 表格边缘操作（Notion/Obsidian 式）：
  - 右下/右边缘/下边缘显示「+」：添加列/行；右下角可用菜单选择「下方插入行 / 右侧插入列」，
    对齐行（`---`）同步扩列；插入后 source 事务提交，整次操作一个撤销组。
  - 左上角显示拖拽把手：按住后上下拖动重排行、左右拖动重排列；拖动时高亮目标行列，
    松手后只改对应 Markdown 行/列，不重写整表。
  - 悬停行/列边缘显示行/列把手与增删入口；删除行列走确认或撤销可恢复。
- 单元格编辑：单击选中单元格，键入进入 cell 编辑（不显示 `|`）；Tab/ShiftTab 移动单元格，
  Esc 退出表格对象，Ctrl+Enter 提交；单元格内容转义 `|`、保留行内 Markdown，禁止破坏表格结构。
- 首版不做合并单元格、嵌套表格、富文本单元格；表格对象与图片/Mermaid 一样在文本选区中保持原子。
- 表格源码仍是唯一真源；插件/外部编辑产生的非规范表格先按原样显示并提示，不静默重写。

## 2. 图片与外部导入

- 保持 `gpui-kit =0.9.1` 与现有 lockfile 组合；复用 GPUI `RenderImage/ImageSource::Render`。
- 直接复用已锁定的 `image 0.25.10`：后台受控解码、EXIF、RGBA→BGRA；不走隐式 HTTP/file 加载器。
- 首版 PNG/JPEG/WebP/GIF，动画只取首帧、原字节保留；SVG 作普通附件，不执行/不栅格化。
- 预算：边长≤8192、总像素≤16MP、`max_alloc=128MiB`、缓存 LRU 128MiB、并发 1；
  超限/损坏只降级预览，不删正文/远端资产。
- GPUI 已支持 `ClipboardEntry::{String,Image,ExternalPaths}`、Windows CF_DIB/CF_HDROP、
  IDropTarget/FileDropEvent；应用侧只接 entry 分流与 `on_drop<ExternalPaths>`。
  Windows 模块只补 PNG 优先、有界重试、GlobalSize 检查、CF_DIBV5 缺口，不另造后端。
- 粘贴优先级：本应用 Markdown 元数据文本→文件→图片→普通文本；每次只选一种，
  Ctrl+Shift+V 强制文本。不得用 `text()` 把 CF_HDROP 变成绝对路径文字。
- 导入仅普通文件，不递归目录；首版每批≤10 项/总输入≤100MiB、单资产≤20MiB；
  整批失败不插半段正文；后台完成重验 note/generation/body_seq/锚点，切笔记/IME 不偷插。

## 3. 资产模型、命名与对账

- 身份 SHA-256 字节内容 + `asset://<64小写hex>`；同字节去重，不承诺同像素/跨格式。
  哈希在 storage 写边界验证，不信调用方。
- 大小/MIME 复用现有列；增加可空尺寸/解码版本缓存；可补 `end_byte/kind/display_name`、
  `asset_ref_state(note,generation,body_seq,parse_rev,dirty)`。
- **不设权威 refcount，不同步加减计数**；通过引用与保护根求可达性。
  缺字节的合法引用仍建占位元数据，显示“待同步/缺失”，不能当孤儿删除。
- 名称属于引用：alt/链接文字是持久真源，转义 Markdown，清洗导出文件名；不存绝对路径。
  不设全局/硬 UNIQUE：同笔记同名同 hash 复用、异 hash 加后缀；离线合并允许同名，用 hash 消歧。
- 每次正文提交/远端物化/导入/撤销重做/恢复后，按准确版本**整篇重建引用并事务替换**，
  涵盖内联及引用式链接/图片；不靠 insertion offset 长期不变。
- 资产、正文操作、引用状态及 outbox 在 storage reducer 的持久事务提交；做不到时先 staging，
  正文接受后再挂引用；一批插入一个撤销组。
- 迁移备份/递增 user_version/断点幂等回填；未对账/解析失败 fail-closed 禁止 GC，
  不把空缓存当零引用。

## 4. 最后引用删除与跨设备 GC

- 删除正文引用立即同步正文、重建本地引用；零当前引用只标记 orphan。
  预览缓存可淘汰，**持久字节不能立即删**。
- 保护根：其他笔记、垃圾箱、版本、撤销、未提交编辑/导入、物化/同步 outbox、因果历史、
  checkpoint/bootstrap lease、旧 epoch 轮换对象；未知/未对账根阻止回收。
- ADR 0006 明确：**完整 finalized assets v1 不做孤儿删除**；7 日是日志宽限，不是资产宽限；
  30 日/水位也不是现有删除许可。
- 物理 GC 必须另立版本化 lifecycle：加密认证的 `hash+gc_round+causal_cut` 删除意图/墓碑、
  保护根证明、全未吊销设备 flush/integrate 后 ACK、checkpoint/lease/epoch 覆盖；
  relay 只按授权 opaque object ID 回收。
- 新引用取消候选；已回收后合法重新导入需新轮次复活并先保证字节可达；
  不永久 ban 某 hash，旧 ASSET 重放不能自行复活墓碑。
- 离线未吊销设备阻 GC；用户显式吊销须告知未同步数据风险；GC 不等于安全擦除。

## 5. 切片

- **S1**：解析器 + span adapter + 列表投影 + 普通源码复制粘贴；10 万字节混合文档性能基线。
- **S2**：粘贴/拖放→字节去重→事务插入→图片预览→引用对账；Windows 真机 PNG/DIB/文件拖放。
- **S3**：任务列表 + 表格交互 + GC 保护根/dry-run；物理删除只在 lifecycle 协议另审后启用。

## 6. S1 实施记录（2026-11-16）

> 范围：Markdown 解析器 + span adapter + 列表投影 + GPUI 列表布局 + 普通源码复制粘贴。
> **未做**：图片 Object / 粘贴 / 拖放 / 表格渲染 / 任务列表交互 / 物理资产 GC（S2/S3）。

### 6.1 新 crate 与依赖

- 新增纯 crate **`crates/markdown`（`menghunote-markdown`）**，只依赖
  `pulldown-cmark = "=0.13.6"`（`default-features = false`）与 `menghunote-text`；
  不依赖 editor/storage/GPUI，editor 单向依赖它（`editor → markdown/text`）。
- **版本精确 pin**：事件类型与 offset 语义是 span adapter 的依据，升级必须先跑
  `crates/markdown/tests/golden.rs`（偏移会随版本变化）。
- `default-features = false` 关掉 `getopts`（CLI）与 `html`（HTML renderer）。
  **0.13.6 的 feature 表没有 `text` feature**（只有 `default = ["getopts", "html"]`
  与 `html/simd/gen-tests`），关掉 default 后剩下的就是纯事件流；ADR §1 的
  “`default-features=false` + text”按“只要文本/事件、不要 HTML 渲染”落地。
- 新增传递依赖仅 `bitflags`（已在依赖图）、`memchr`（已在依赖图）、`unicase`（极小，
  无 build script/C 源）；无其它重依赖。
- Options 显式开启 `ENABLE_TABLES | ENABLE_TASKLISTS | ENABLE_STRIKETHROUGH`，
  并有用例断言**未**使用 `ENABLE_GFM`。

### 6.2 span adapter（`into_offset_iter` 不是 CST）

不用 AST 重新序列化（会改写正文），只从**原始源码切片**定位定界符：

- 标题/引用/列表项/围栏：从事件起点或行首扫描源码，得到 marker + 内容区间；
- 行内：Strong/Emphasis/Strikethrough 按定界符 run 长度定位首尾；Code 用相同长度
  反引号 run；Link/Image 用方括号配对（尊重转义/嵌套），支持 inline / reference /
  collapsed / shortcut / autolink；表格分隔符、实体、转义只作为原始字节保留；
- **定位不确定**（如 `***a***` 的三种定界符嵌套）设置 `uncertain_inline`，editor
  整篇行内投影退回旧 scanner；`**a****b**` 等旧 scanner 已专门处理的相邻定界符由
  “legacy 构造必须逐一对齐”规则自动回退；
- 未完成构造（未闭合 `**`、`[](`、围栏）不产生可投影构造；表格/HTML/Setext/缩进代码
  记入 `unsupported_spans`/`code_spans`，内部的行内构造不投影（原样显示）；
- 所有偏移都是 UTF-8 字节且在校验后转成 `menghunote_text::SourceRange`；
- CRLF、中文、emoji 均有 golden 用例（行区间含 `\r\n`，内容区间不含）。

### 6.3 editor 集成与列表投影

- `MarkdownDocument` 优先消费新解析器的块/行内构造与列表块树；旧 scanner 保留为
  **fallback**（行内无法逐一对齐时整篇退回；块构造只在没有重叠时补位）。
  两条路径都只产出 source 区间，源码零改写。
- `ProjectionMap` 正式实现 `Replace` 段；新增 `ProjectionPiece::{Hidden, Replace}` 与
  `ProjectionMap::from_pieces` / `from_markdown_with_lists`（生产路径）。
- 列表 marker 用 **Replace**：`- ` → `• `；有序 → 右对齐编号（同一列表按最大编号
  位数补前导空格，保证宽度稳定）。source 区间完整保留，`list_marker_source()`
  可取回 `- ` / `1. `。
- 端点偏置（写死并测试）：source→display 在 marker 内部取替换段起点；display→source
  在 bullet 内取 marker 起点、在 bullet 末尾 `Right` 进入正文、`Left` 停在 marker；
  display 选区反查 source 会**包含**可见 marker 的源码区间（复制得到 `- `，不是 bullet）。
- 显现规则：caret/选区**进入 marker 区间**才显示源码（caret 恰好等于 marker.end 不显现，
  点正文仍是 bullet）；列表行前导空白恒为 `Elide`（布局缩进由 app 提供）。
- `Copy/Elide` 语义不变；未支持/定位不确定构造原样显示；源码复制/普通粘贴不改正文
  （粘贴仍走既有 `replace_text_in_range` 事务）。

### 6.4 app 列表布局与编辑

- 度量/绘制/命中/光标/选区共用 `LayoutFrame` 几何：
  - 列表首行 `indent = 引用缩进 + depth × 24px`，bullet/编号在 display 行首；
  - `wrap_indent = indent + marker 真实 shaping 宽度`，续行/子段落/内部围栏用 hanging
    indent；`VisualLine.indent` 逐视觉行保存，绘制与命中都只读它；
  - marker 宽度用与绘制同一字体/字号 shaping 测量（Consolas 等宽），不用字符数估算；
  - `WrapCache` 的键包含 `indent/wrap_indent`（caret 进入 marker 导致源码 marker 宽度
    变化时自动重排）；
  - 引用块缩进改为按解析器的 `quote` 前缀计算（`> - item` 不再丢引用缩进）；
  - 列表内 4+ 空格围栏由新解析器补出 `FencedCode` 构造/行语义，因此等宽字体与内部
    代码缩进正确。
- marker 默认隐藏为 bullet/编号；caret 进入 marker 区域显示源码 `- ` / `1. `，退出恢复
  （Typora 规则）。
- Enter：item 首行、caret 在 marker 之后时同级新建（嵌套保留源码缩进、有序 +1、
  **不整篇重编号**）；空项删除 marker 退出列表。一次按键一个 `EditRequest`；IME
  组合中先取消组合并**丢弃本次结构动作**；平台随后补发的同一 `\n` 文本输入会被
  消费，避免双换行/双请求。
- **S1 限制**：Tab/ShiftTab 层级调整未实现；Enter 只在 item 首行接管（续行/子段落
  交回普通换行）；不做任务框点击、表格。图片只解析 Image span，editor **整段原样
  显示**（并丢弃旧 scanner 在图片内部产生的 Link，避免显示成 `!alt`）；图片 Object
  留给 S2。

### 6.5 测试与性能基线

- `cargo test -p menghunote-markdown`：11 个 golden（嵌套/松紧/有序起始号/续行/子段落/
  内部围栏/转义/实体/引用式链接/CRLF/中文/emoji/未完成构造/表格/Setext/Options）；
- `cargo test -p menghunote-editor`：新增列表投影（Replace 映射/偏置/显现/复制）、
  列表 Enter（同级/嵌套/有序/空项/IME 守卫/选区替换）与 10 万字节解析+投影基线；
- `cargo test -p menghunote-app`：新增 `headless_lists`（真实 `AppShell` + 临时 vault：
  打开/关闭不改写源码；点击列表行 → Enter → 同级 item 落库；嵌套 item 保留 2 空格）；
- 现有 editor/app/sync 全回归通过；`cargo fmt --all`、editor/app 的
  `clippy --all-targets -D warnings` 通过（app 含 `--features dev-sync`）。

本机（Linux，`CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0`）10 万字节混合文档：

| 阶段 | 耗时 | 规模 |
| --- | --- | --- |
| `menghunote-markdown::parse` | ~40 ms | 10,116 构造 / 2,248 item / 12,365 行 |
| `MarkdownDocument::parse`（含旧 scanner fallback） | ~74 ms | 同上 |
| 列表感知投影 + 样式 runs | ~175 ms | 32,596 段 / display 74,198 B |

投影是 v1 全量 O(n) 路径（ADR 0002 已知代价）；基线只记录、不卡 CI（上限 5 s）。
内存未单独计量；构造/段表规模已随基线打印，后续增量解析/按块缓存时复用同一组指标。

### 6.6 S2/S3 依赖

- S2 直接复用：列表 item 的 `content`/`marker`/`task` 字段、`unsupported_spans` 与
  `code_spans`（图片/粘贴插入后重新解析即可准确定位）；图片 Object 需要把
  `ConstructKind::Image` 从“解析但不投影”切到 `SegmentKind::Object`。
- S3 表格/任务列表：`ListBlock`/`LineInfo.kind` 已给出行/单元语义，表格渲染需要新增
  `Table` 块树；任务框点击只需改 `[ ]↔[x]` 的 marker 区间（同事务）。
- 物理资产 GC 不依赖本切片；但 S2 的引用对账应使用本解析器的完整 span（含引用式链接/
  图片），不要再回到正则。

## 非目标

不换富文本真源、不自动远程取图、不做 SVG/动画/图像编辑器、不首版做脚注/数学/alerts、
不移动 `v0.4-sync`、不在本 ADR 启用物理资产 GC。
