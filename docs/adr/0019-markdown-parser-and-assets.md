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
  悬浮显示块工具条（放大、编辑源码；Mermaid 后续同款但只保留编辑）。
  只有点工具条「编辑」/`F2` 才进入源码模式，显示完整 `![alt](asset://hash)` 或
  fenced mermaid；`Esc`、点击块外或 `Ctrl+Enter` 退出并提交。
- 对象块内部不产生文本 caret/部分选区；Delete/Backspace 先选块、再删整个源跨度；
  复制对象得到完整 Markdown。键盘 Tab 可聚焦块，Enter 打开编辑器（与普通文本区分）。
- **文本选区与对象块相交时**：块以整体选中态显示（描边/底色），不展开源码，也不允许
  只选中块内某段源码；复制时按完整 Markdown 源跨度还原到剪贴板。移动 caret 跨过块
  同样不展开源码。只有工具条「编辑」/F2 才切换该块到源码模式。
- 普通 fenced code（非 mermaid）仍按行内代码编辑；只有图片和 Mermaid 走对象块。
- 表格也是 **Object Block**：按 GFM 行列布局，**选中/光标进入都不显示 `|` 源码**；
  单元格就地编辑（只显示 cell 文本），不进入整表源码模式；需要时工具条提供「编辑表格源码」。
  **实施顺序**：S3a 先落“只读表格 Object + 工具条编辑源码”（§10）；单元格就地编辑、
  加行加列、拖拽重排属于 S3b（§10.5），横向滚动同样后置。
- 表格边缘操作（Notion/Obsidian 式）：
  - 右下/右边缘/下边缘显示「+」：添加列/行；右下角可用菜单选择「下方插入行 / 右侧插入列」，
    对齐行（`---`）同步扩列；插入后 source 事务提交，整次操作一个撤销组。
  - 左上角显示拖拽把手：按住后上下拖动重排行、左右拖动重排列；拖动时高亮目标行列，
    松手后只改对应 Markdown 行/列，不重写整表。
  - 悬停行/列边缘显示行/列把手与增删入口；删除行列走确认或撤销可恢复。
- 单元格编辑：单击选中单元格，键入进入 cell 编辑（不显示 `|`）；Tab/ShiftTab 移动单元格，
  Esc 退出表格对象，Ctrl+Enter 提交；单元格内容转义 `|`、保留行内 Markdown，禁止破坏表格结构。
- **合并单元格、嵌套表格、富文本单元格不做**（2026-11-17 确认）；表格对象与图片/Mermaid
  一样在文本选区中保持原子。cell 仍是 GFM 源码文本，后续只考虑**行内 Markdown**
  （粗体/斜体/删除线/行内代码/链接等），不支持块级元素/换行/图片；当前 cell 渲染仍是
  纯文本，行内 Markdown 属于后续切片，不是既有能力。
- 表格源码仍是唯一真源；插件/外部编辑产生的非规范表格先按原样显示并提示，不静默重写。

## 2. 图片与外部导入

- 保持 `gpui-kit =0.9.1` 与现有 lockfile 组合；复用 GPUI `RenderImage/ImageSource::Render`。
- 直接复用已锁定的 `image 0.25.10`：后台受控解码、EXIF、RGBA→BGRA；不走隐式 HTTP/file 加载器。
- 首版 PNG/JPEG/WebP/GIF，动画只取首帧、原字节保留；SVG 作普通附件，不执行/不栅格化。
  Windows 剪贴板 CF_DIB 由 GPUI 转成 BMP，落库前统一转 PNG（2026-11-17）。
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
  - S3 任务列表：已完成（§9）；**S3a 表格只读渲染 + 编辑源码切换**：已完成（§10）；
    **S3b 表格加行/加列**：已完成（§11）；**S3b2 单元格选择与就地编辑 + Tab 导航**：
    已完成（§12）；**S3b3 删除行/删除列**：已完成（§13）；**S3b4 正文右键「插入 → 表格」**：
    已完成（§14）；**S3b5 表格拖拽重排行/列**：已完成（§16）；S3b 其余
    （合并单元格、横向滚动）：未做。
  - 物理资产 GC 仍未做（不在 S3a/S3b）。

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
- S3 表格/任务列表：`ListBlock`/`LineInfo.kind` 已给出行/单元语义；任务框点击只需改
  `[ ]↔[x]` 的 marker 区间（同事务），已完成（§9）。表格块树 `TableBlock`、Object 投影与
  只读渲染已完成（§10，S3a）；加行/加列已完成（§11，S3b）；单元格就地编辑与 Tab 导航
  已完成（§12，S3b2）；行列删除已完成（§13，S3b3）；行列拖拽重排已完成（§16，S3b5）。
- 物理资产 GC 不依赖本切片；但 S2 的引用对账应使用本解析器的完整 span（含引用式链接/
  图片），不要再回到正则。

## 7. S2a 实施记录（2026-11-16）

> 范围：**图片 Object 的投影 / 布局 / 只读预览 / 编辑切换**。
> 明确**未做**：粘贴/拖放/剪贴板图片、文件系统对话框、引用对账与 GC、Mermaid、
> 表格与任务列表交互（S2b/S3 的范围）。

### 7.1 选择与判定（写死，测试钉住）

- **Object Block 判定（editor `MarkdownImage::is_block`）**：图片 span 必须
  1) 从行内容起点开始；2) 之后到行尾只有空白（行尾空格/Tab 允许）；3) 该行是普通
  段落（不是标题/引用/列表/围栏/缩进代码/表格/HTML/分隔线），且不在
  `code_spans`/`unsupported_spans` 里。
- **混排/行内图片首版保持源码原样显示**（不原子替换半行内容，避免错位）：
  同一行还有别的文字、行首缩进、列表项、引用、标题、表格里的图片都不投影。
  围栏代码里的 `![...]` 本来就不产生图片事件（字面文本）。
- 引用式图片（`![alt][label]`）从 `pulldown-cmark` 的 `Tag::Image::dest_url` 取
  **解析后的真实 URL**，`span` 仍是源码里的 `![alt][label]`；正文零改写。
  markdown crate 新增 `ParsedMarkdown::images`（`ImageSpan { span, content, dest }`），
  不重序列化 AST。

### 7.2 投影（`SegmentKind::Object`）

- `ProjectionPiece::Object(SourceRange)`；`SegmentKind::Object` 正式实现：
  source 是完整 `![alt](dest)`，display 是**占位符** `U+FFFC`（`OBJECT_PLACEHOLDER`，
  3 字节）。选占位符而非零宽的理由：display 上对象有明确区间，文本选区能"碰到"
  对象、复制能完整反查 source，不会出现"光标消失"。
- 偏置规则（editor `projection.rs` 模块文档 + `tests/image_object.rs` 钉住）：
  - source 落在对象内部（含起点）→ 占位符起点；对象末尾 → 占位符末尾（接缝/文档尾）；
  - display 落在占位符内部（含起点）→ 对象 source 起点（`Left` 偏置同样不落进源码中间）；
    占位符末尾 → 对象 source 末尾；
  - display 区间终点落在占位符内部或末尾 → source 区间**完整包含**对象源码
    （复制得到完整 Markdown）；终点恰在占位符起点 → 不包含对象。
- **显现规则**：图片对象**不**参与"caret/选区相交即显现"的普通构造规则。只有
  `EditorController::begin_object_source_edit(line)`（工具条「编辑源码」/`F2`）显式
  设置 `object_edit_line` 才展开源码；文本选区跨过对象时保持占位符 + 整块描边。
- 源码被改坏（图片不再是图片）时 `refresh_markdown` 自动清空源码模式。

### 7.3 app 布局 / 绘制 / 命中

- 行高：`LineMetric::line_height = 内容高 + 2 × OBJECT_PADDING`（`+12px`），
  内容高 = 已解码图片按内容宽度等比（`fit_size`：`min(原始宽, 内容宽)`），
  未就绪时用 `OBJECT_PLACEHOLDER_HEIGHT`。`WrapCache` 的度量键包含行高，
  解码完成后自动重排。
- 几何：`EditorGeometry::objects: Vec<ObjectPaint>`（窗口坐标矩形 + 图片/占位文案 +
  选中态）。滚动/命中/选中框与 `LayoutFrame` 的行顶/行高**同一套**坐标；
  对象行的拖选/方向键统一落到占位符末尾（对象内部不产生 caret）。
- 绘制：对象行不画字形；已解码用 `Window::paint_image`（BGRA `RenderImage`，首帧），
  未就绪画占位框 + alt/状态文案（`图片加载中…` / `图片缺失或待同步` / `图片无法预览`）；
  选中态画 2px 描边。`paint_selection` 跳过对象行（不画占位符的文本选区）。
- 悬浮工具条：对象被悬停/选中/文本选区相交时出现，「放大」「编辑源码」；
  放大按钮或已选中图片再次单击打开遮罩；按钮/遮罩样式参考 Obsidian；
  位置来自上一帧对象矩形，点击由工具条按钮处理，元素层的鼠标监听用
  `toolbar_rect` 放行（不会同时挪光标）。
- 键盘：`F2` 有对象目标时进入源码模式（否则仍是侧栏重命名）；`Esc` / `Ctrl+Enter` /
  点击块外退出；`Backspace/Delete` 在对象被点选时删除**完整源跨度**，折叠 caret 紧贴
  对象时**先选块**、再按一次才删；`Ctrl+C` 在对象选中时复制完整 Markdown，否则复制
  选区的 source 区间（对象行同样包含完整源码）。对象内部不产生文本 caret/部分选区。

### 7.4 资产加载（不依赖 storage、不阻塞主线程）

- `crates/app/src/editor/object_image.rs`：
  - `AssetBytesProvider`（trait + 闭包 blanket impl）是唯一能力面；`AppShell` 用
    `Vault` 的只读 `asset_bytes` 注入，**EditorView 不依赖 storage**；
  - 解析只接受 `asset://<64 小写 hex>`；`asset://` 之外不猜、不归一；
  - **后台单线程**解码（结构性并发 1）：主线程只 `request(hash)`（幂等，登记 inflight）
    并读状态；worker 读字节 → 校验预算 → 解码 → 写回缓存；`pump` 用
    `take_completed()` 触发一次重绘。主线程不读 SQLite、不解码；
  - 预算：边长 ≤ 8192、总像素 ≤ 16MP、单图解码内存 ≤ 128MiB；超限/损坏/不支持的格式
    （SVG/BMP/TIFF…）只降级占位，不 panic、不删正文；
  - 只解码 PNG/JPEG/WebP/GIF，EXIF 方向在解码前应用；GIF/WebP 动画**只取首帧**；
  - 已解码缓存 LRU ≤ 128MiB（`Ready` 项按最近使用淘汰，`Missing/Failed` 是零字节终态，
    不触发淘汰）；`image = "=0.25.10"`（`default-features = false` + png/jpeg/webp/gif），
    与 gpui-pre 的传递依赖同版本，lockfile 只多一行。
- `EditorView::new` 增加一个 `SharedAssetProvider` 参数（`AppShell` 注入）；
  `AppShell::active_editor_view()` 提供不依赖 dev-sync feature 的测试入口。

### 7.5 测试

- `cargo test -p menghunote-markdown`：**15**（新增 3 个图片 golden：dest/alt/span、
  引用式解析、嵌套在链接里）；
- `cargo test -p menghunote-editor`：**148**（新增 `tests/image_object.rs` 11 个：
  block 判定/混排保持源码/Object 段/端点偏置/复制源文/选区不显现/源码模式往返/
  删除整段/语法改坏自动退出）；
- `cargo test -p menghunote-app --test headless_images`：**7**（真实临时 vault + 真实
  PNG 资产：打开不 panic/不重写源码、对象可选中、选中不出现源码、工具条编辑/Esc 往返、
  Backspace 先选块再删整段、宽图等比缩到内容宽度、选区跨对象复制完整 Markdown、
  缺失/损坏资产降级占位且正文不动）；
- 回归：S1 列表/标题 headless（`headless_lists` 6、`headless_ui` 8）全绿；
  app 默认 **153**、app `--features dev-sync`、editor/markdown 如上；
  `cargo fmt --all` 与 markdown/editor/app（含 `--features dev-sync`）的
  `clippy --all-targets -D warnings` 通过。

### 7.6 已知限制与 S2b 接口建议

- **未做 S2b**：粘贴/拖放/剪贴板图片、引用对账（`asset_refs`）、资产 GC；S2a 只**读**
  资产字节，不写 `assets`/`asset_refs`。
- 像素级外观（DPI、真实窗口里的图片清晰度/工具条位置）**未在 Windows 真机验证**；
  headless 测试只证明几何/事务/状态链路。
- 行内/混排图片首版保持源码；后续要做"原子缩略图"时应复用
  `ProjectionPiece::Object` + `SegmentKind::Object`（display 已是占位符），只需在
  `is_block_image` 的判定与布局里允许行内对象，不必再改映射语义。
- `ImageService` 的接口已预留 S2b 需要的点：
  - `ImageService::request(hash)` / `state(hash)` / `display_size(hash, max_width)`；
  - 缺字节的 `Missing` 是终态（不重试）：S2b 资产同步完成后调用方应能清缓存重试，
    建议加 `ImageService::forget(hash)` / 按 vault 事件失效（本次未加，等对账切片给
    明确的"资产字节已到达"事件再接线）；
  - provider 只认 `&str` hex，S2b 的导入/去重逻辑在 storage 侧完成后无需改这里。
- `asset://` 解析是**严格** `asset://<64 小写 hex>`；S2b 若要支持带 title 的
  `![alt](asset://hash "title")`，应在 markdown crate 的 `ImageSpan::dest` 已解析的前提
  下扩展（当前 `dest_url` 已去掉 title，无需改 editor）。
- **S2b 已补齐上述"未做"项**（粘贴/拖放/引用对账/旧路径输入条移除），见 §8；S2a 的
  投影/绘制/解码语义未改动。

## 8. S2b 实施记录（2026-11-16）

> 范围：**粘贴/拖放 → 资产入库 → 正文插入 → 引用对账**。
> 明确**未做**：Windows CF_DIB/CF_DIBV5 剪贴板位图与真实 `IDropTarget` 真机验收、
> Mermaid/表格 Object、物理资产 GC；旧侧栏「插入附件…」路径输入条**已移除**。

### 8.1 剪贴板分流（每次只选一种）

- 唯一决策点 `editor::clipboard::choose_paste`（纯函数，可单测）：
  **本应用 Markdown/文本元数据 → 文件路径 → 图片 → 普通文本**；
  `Ctrl+Shift+V`（`force_text`）只看字符串条目，**绝不**把 `CF_HDROP` 变成绝对路径文字
  （`ClipboardItem::text()` 的路径回落被显式绕开）。
- "本应用元数据"用纯文本前缀 `menghunote/1 markdown`（不引 JSON 依赖，也不信任任意
  元数据）；`Ctrl+C` 复制对象 Markdown/选区源码时写入该标记，所以复制粘贴回本应用时
  走文本路径（与 S1 行为一致）。
- 图片：`ClipboardEntry::Image` 的**原始编码字节**（PNG/JPEG/WebP/GIF）直接入库，不转码；
  Windows CF_DIB 经 GPUI 转成 BMP，落库前统一转 PNG（2026-11-17 修复）；
  声明的格式与字节魔数不一致时以**字节**为准。
- SVG / TIFF / ICO（以及 BMP **文件**，不是 CF_DIB）：按**普通附件**存，
  不执行、不栅格化、不进图片对象预览。
- 入口：`on_key_down` 的 `Ctrl+V`/`Ctrl+Shift+V` + `InputHandler::paste`（平台直接派发
  paste 的后端，例如 web）+ 编辑区 `on_drop::<ExternalPaths>`。三条路都走同一套
  分流/事务/回滚/对账。

### 8.2 导入能力面与写入顺序

`EditorView` 只认一个 trait `editor::clipboard::AssetImportWriter`（与 S2a 的
`AssetBytesProvider` 同一模式，编辑器不依赖 storage/文件系统）；`AppShell` 注入
`attachment::VaultAssetWriter`：

1. `stage_and_store`：按 `body_doc_id` 从**当前结构投影**反查 `note_id`（切笔记/删笔记
   后这里就失败）→ 全量校验/读取 → 写 `assets`（内容寻址，重复导入 = 单资产）→ 按
   `基址 + 相对偏移` 写 `asset_refs`；**任何失败都发生在正文写入之前**，中途失败回滚
   本批已写引用；
2. 编辑器把整批 Markdown 作为**一个** `compose_replace_selection` 提交（一个撤销组）；
3. 插入前重验：`doc_id` / `body_seq` / 插入锚点（光标 source 偏移）/ 持闸 / IME 组合，
   任一变化 → 回滚引用并放弃（"切笔记/IME 不偷插"）；
4. 正文成功 → `commit`：按**当前正文**整篇重建引用；失败只记日志（下次对账修好）。

上限（ADR §2）：每批 ≤ 10 个文件、总输入 ≤ 100 MiB、单资产 ≤ 20 MiB；只接受**普通
文件**（目录/相对路径/设备文件整批失败）。多项之间用 `\n` 分隔；单项不补换行。

### 8.3 图片字节校验（先校验，再入库）

`stage_image`：非空 → ≤ 20 MiB → 魔数必须是 PNG/JPEG/WebP/GIF → `ImageReader` 只读头部
拿尺寸 → 复用 S2a 预算（边长 ≤ 8192、总像素 ≤ 16MP）。任一失败 → 明确错误、
**不写资产、不写正文**（损坏图片不会留半条正文）。文件名 `粘贴图片.<ext>`（alt 转义
`\ [ ] ( )`，换行压空格）。

### 8.4 引用对账与坐标约定

- 新增 storage 事务 API `menghunote_storage::replace_asset_refs(vault, note_id, body_seq,
  refs)`：一个事务里 `DELETE` 该笔记全部引用 → 为缺元数据的 hash 建占位
  （`byte_len/media_type/data` 全 `NULL`，否则外键不成立）→ 逐条 `INSERT`；任一步失败
  整笔回滚。**不删资产**（v1 不做孤儿删除），`data IS NULL` 的合法引用照写。
- **定位点约定**：`asset_refs.start_byte` 是对账路径写下的 **`asset://` 前缀在正文中的
  字节偏移**（内联/引用式链接/`[label]: asset://…` 定义行都能扫到）。它不是长期身份，
  任何时候都按正文整篇重建；旧路径插入写的是链接起点，两种都只是"位置标签"。
- 扫描 `attachment::asset_refs_in_source`：严格 `asset://` + 正好 64 个小写 hex，且后一个
  字节不是小写 hex；不解析 Markdown 结构（URI 扫描对引用式链接天然成立）。代码块里的
  出现**也计入**（fail-open：宁可多留一条账本，也不误删真实引用）。
- 触发点：
  - 导入成功（`commit` 精确重建）；
  - **每次本地编辑被受理**（`EditOutcome::Applied`，含撤销/重做）——没有资产链接的笔记
    在这里按"上一次/这一次是否出现过 `asset://`"短路，不查库；
  - `DriverEvent::Durable`（正文真落库后）；
  - 打开笔记（构造时）、切换/关闭笔记（`drain_and_close` 之后）。
  - **剩余触发点（明确未接）**：远端 adopt 若没有产生 `DriverEvent::Durable`，引用要等到
    下一次本地编辑/切换/关闭才对账。没有把对账放进 storage 的 A 事务（那需要改
    `storage::session` 的 flush 路径，超出本切片写作用域）：因此"最坏情况"是引用账本
    短暂落后于正文，下一次提交/关闭即收敛。
- 幂等：对账前先比较"当前引用集合 vs 扫描结果"，相同则**不写库**（重复对账零事务）。

### 8.5 移除旧路径输入条

侧栏「插入附件…」按钮 + 路径输入框（`render_attachment_block` / `AttachmentUi` /
`attachment_toggle/submit_path/disabled_reason` 等）**整体删除**；粘贴/拖放成为唯一导入
入口。`crates/app/src/attachment.rs` 保留可复用的读取/校验/去重/`assets` 写入核心，
并去掉 `dev-sync` feature 门禁（默认构建立即具备粘贴/拖放导入能力，不引入新依赖）。

### 8.6 测试

- `cargo test -p menghunote-app --test headless_import`：**10**（默认 9 + dev-sync 1）：
  PNG 粘贴 → 资产字节 + 引用 + S2a 对象 Ready；同图两次 = 单资产两引用；多文件单事务
  （一次 Ctrl+Z 全撤、Ctrl+Shift+Z 整批恢复）+ 落库后引用复算；目录/11 个文件/损坏图片
  整批失败零写入；纯文本粘贴与 `Ctrl+Shift+V` 强制文本（含"只有文件时不写路径"）；
  IME 组合中不插；拖放用**真实 `FileDropEvent`**（caret 在第二行、落点在第一行 → 插在
  caret）；SVG 按普通附件；持闸期间不插（dev-sync）。
- `attachment` 单测 **6**：链接/alt 转义、扩展名/MIME/SVG 判定、`asset_refs_in_source`
  （内联/引用式定义/多字节前缀/大小写/63/65 位）、`batch_markdown` 偏移、图片魔数与
  尺寸校验、SVG 附件与扩展名清洗；`editor::clipboard` 单测 **6**：优先级、元数据、
  强制文本、位图/非位图分流、`ClipboardItem` 真实条目。
- `menghunote-storage` 单测 **4**（`replace_asset_refs`）：整篇替换、幂等/去重、缺字节
  引用保留 + 占位、失败不改已有引用。
- 回归：app 默认 / `--features dev-sync`、storage/editor/markdown/text 全绿；
  `dev_sync_sync_once` 的"附件经 relay 到另一台"改走**粘贴链路**后仍通过。
- headless 拖放的一个框架细节：GPUI 的 drop 派发用 `hitbox.is_hovered(window)`，键盘
  模态下恒为 `false`（按键后不显示 hover 高亮）。真实拖拽本身是鼠标手势（先有
  mouse move），测试里显式补一次 `simulate_mouse_move` 才能命中 `on_drop`。

### 8.7 已知限制（明确记录，不假装全量）

- **导入在宿主帧内同步完成**（读文件 → SHA-256 → 写 assets/refs → 编辑器事务），没有
  后台线程；上限 100 MiB/批就是给这条同步路径设的护栏。往后台搬需要"资产已落库但正文
  还没插"的中间态，属于后续切片。
- **远端 adopt 的对账依赖 `DriverEvent::Durable`**：若某次远端变更没有产生 durable 事件，
  引用要等到下一次本地编辑/切换/关闭才对账（见 §8.4 的触发点）。没有把对账放进 storage
  的 A 事务（需要改 `storage::session` 的 flush 路径，超出本切片写作用域）。
- **编辑时引用账本可能短暂领先/落后正文**：对账在"编辑被受理（乐观镜像）"时就做，正文
  durable 之后再做一次；引用只是账本（同步用它挑要上传的资产），不参与裁决。
- 移到最后一条 `asset://` 之后的对账靠"上一次有过链接"的痕迹触发；没有资产链接的笔记
  完全不查引用表。
- **S2a 的 `Missing` 缓存不失效**（继承限制，未在本次改）：资产字节稍后由同步到达时，
  已缓存 `Missing` 的图片要重开文档才刷新；粘贴路径的字节在插正文前就已落库，不受影响。
- 混排/行内图片仍按 S2a 决定原样显示源码；粘贴在行中产生的 `![](...)` 不会自动补换行，
  因此不会变成 Object Block（ADR §1：不擅自补换行）。

### 8.8 Windows 真机清单（未做，后续）

1. 资源管理器复制文件 → `Ctrl+V`：CF_HDROP → `ExternalPaths` 分流（本机已有代码路径，
   需真机确认不落成文本/路径）；
2. 截图工具/浏览器复制图片 → `Ctrl+V`：Windows 后端读 `PNG`/`JFIF`/`GIF` 自定义格式；
   **CF_DIB 已支持**（GPUI 转 BMP → 落库前转 PNG，2026-11-17），CF_DIBV5 仍是缺口；
3. 从资源管理器拖文件到编辑区：`IDropTarget` → `FileDropEvent` → `on_drop`（含落点
   hover 命中、DPI 缩放下的坐标）；
4. 中文/空格路径、多文件（≤10）、超大文件（>20 MiB 拒绝）与目录拖入的提示行；
5. 粘贴图片后立刻按 `Ctrl+Z`：一次撤销整批；重开后引用 `start_byte` 与正文一致。

## 9. S3 实施记录（2026-11-17，任务列表复选框交互）

> 范围：**任务列表复选框的解析/投影/绘制/命中/切换**。
> 明确**未做**：表格对象与交互（S3 另一半）、资产物理 GC、列表折叠/拖拽；
> 不改 schema / 同步 wire / tag。

### 9.1 解析层：`ListItem.task_marker`

- `menghunote-markdown` 的 `ListItem` 新增 `task_marker: Option<Span>`：
  **源码**里 `[ ]` / `[x]` / `[X]` 的 3 字节区间（两个方括号 + 内部字符），
  **不含** `]` 之后的空白或正文。区间来自 `pulldown-cmark` 的
  `Event::TaskListMarker` range，再做形状校验（`[` + 空格/Tab/`x`/`X` + `]`），
  形状不符只保留 `task` 状态、不暴露可点区间；已有 `task: Option<bool>` 语义不变。
- golden 测试钉住：普通/嵌套/有序/引用任务项各自的 span、`[X]` 仍为已勾选、
  区间终点正好在 `]` 后；`- [ ]no-space` 与 marker 后 5 空格（pulldown 只看 ≤3）
  不产生任务项，也就不暴露 span。

### 9.2 editor：占位投影与切换 API

- `TASK_BOX_DISPLAY = "\u{00A0}\u{00A0}"`（两个不换行空格）：隐藏态下把任务框
  `Replace` 成不可见占位。不用可见 `☐` 的原因：绘制层自绘复选框，可见字符会在
  缺字时露豆腐块/与自绘方框重影；NBSP 还避免复选框在折行处被拆开。
- 显现规则与列表 marker 一致：caret 落在 marker **或任务框**内、或非空选区与之
  相交时，marker + 任务框都显示源码（`- [ ] …`），此时不画复选框。
- `ProjectionMap::replace_display_range(source)`：只有 source 完全等于某个
  `Replace` 段时返回 display 区间；app 据此自绘/命中，显现源码时返回 `None`。
- `EditorController::toggle_task_marker(index)`：
  - 只替换任务框 3 个字节（`[ ]` → `[x]`，`[x]`/`[X]` → `[ ]`，**勾选统一写小写
    `x`**；`[X]` 视为已勾选）；缩进、marker、`]` 后空白、其它正文/换行逐字不变；
  - 一次调用 = 一个 `EditRequest`（单 `ScalarEdit`），进会话默认撤销组；
  - `compose_edits` 的“caret 落到替换之后”在这里被纠正：替换等长（3 scalar），
    返回请求前恢复原 selection，点击复选框不落正文 caret；
  - 非任务项/越界 → `Ok(None)`；IME 组合由 `ensure_editable` 拒绝。
- 测试（`crates/editor/tests/task_box.rs`，6 个）：占位/映射/复制反查、caret 与
  选区显现、父/子/有序/大写独立切换、caret 不变、非任务 no-op。

### 9.3 app：绘制 / 命中 / 编辑接线

- 复用现有列表几何：`block_metrics` 对任务项把 **marker 显示文本 + 任务框占位 +
  `]` 后源空白**一起按真实 shaping 测宽，内容列/续行 hanging indent 与复选框位置
  同一来源；非任务列表宽度不变。
- `EditorView::task_checkboxes()` 给出 `(item, checked, source span, display range)`；
  `EditorElement::prepaint` 用与链接 hitbox 相同的 `caret_row_x` 把 display 区间换算成
  窗口矩形，写入 `EditorGeometry.task_boxes`（绘制/命中/headless 断言同源）。
- 绘制：行内矩形内居中画圆角方框；未勾选白底灰描边，已勾选主题蓝底 + 白色勾形
  （颜色只在 `theme` 里）。矩形是整个占位区（约一个字符格），比 13px 方框好点。
- 命中：每帧为可见任务框 `insert_hitbox` + `set_cursor_style(PointingHand)`（沿用
  AGENTS §6 的 gpui 原生 cursor 机制，不自建 hover 状态机）；mouse-down 在处理正文
  caret/选区**之前**命中任务框，只调 `toggle_task_checkbox` 并 `return` —— 不落
  caret、不开始拖选、不触发对象选中/列表折叠。命中矩形带 `source span`，点击时与
  当前解析结果比对；陈旧帧（远端已改文本）直接忽略，不拿旧偏移改新内容。
- headless（`crates/app/tests/headless_tasks.rs`，4 个，真实 `AppShell` + 临时 vault）：
  点击 `[ ]`→`[x]` 再点回 `[ ]` 且 caret 不变；嵌套/有序/`[X]` 各自独立；`Ctrl+Z` /
  `Ctrl+Shift+Z` 一次撤销/重做；durable 后重开 vault 与重新打开 shell 仍是新勾选态。

### 9.4 验证与已知限制

- `cargo fmt --all -- --check`、`cargo clippy -p menghunote-markdown -p menghunote-editor
  -p menghunote-app --all-targets --features dev-sync -- -D warnings` 通过；
  `cargo test -p menghunote-markdown` **22**、`-p menghunote-editor` **157**、
  `-p menghunote-app --features dev-sync` **427**，0 failed（本机 Linux）。
- 未做像素/真机验收：复选框外观、手形光标、DPI 下命中仍需 Windows `ui_smoke.ps1`；
  本环境只证明了“几何 → hitbox → EditRequest → undo/持久化”链路。
- 任务框显现源码时（caret 进入）不画复选框是**写死行为**：与列表 marker 的 Typora
  规则一致；如果后续要 Obsidian 式“光标在框里仍显示复选框”，需要另立决策。
- 表格的只读 Object 渲染与编辑源码切换见 §10（S3a）；单元格交互（S3b）与资产物理 GC
  仍后置。

## 10. S3a 实施记录（2026-11-17，表格 Object 只读渲染 + 编辑源码切换）

> 范围：**表格的解析块树 / Object Block 投影 / GPUI 只读渲染 / 选中与「编辑源码」切换**。
> 明确**未做**（S3b）：单元格就地编辑、加行加列、拖拽重排、行/列把手与删除、Tab 单元格
> 导航、合并单元格、嵌套表格、富文本单元格、表格排序、复制单元格 Markdown；资产物理 GC
> 仍未做。不改 schema / 同步 wire / tag。

### 10.1 解析层：`TableBlock` 块树（`menghunote-markdown`）

- `ParsedMarkdown` 新增 `tables: Vec<TableBlock>`：
  - `span`：整表源码区间，**不含**最后一行之后的换行（Object 占位不能吞掉换行，否则
    会和下一段粘成一行）；端点全部是 UTF-8 字节 / char 边界。
  - `header: TableRow`：表头行（GFM 表格恒有）；`delimiter: Span`：分隔行整行区间
    （`pulldown-cmark` 不为分隔行发 `TableRow` 事件，这里按“表头行的下一行”定位）；
  - `rows: Vec<TableRow>`：数据行；每行 `span`（不含换行）、`line`（0 起）、
    `cells: Vec<Span>`（与 `columns` **等长**；缺列补空区间，多列按 GFM 丢弃）；
  - `alignments: Vec<TableAlignment>`（`None/Left/Center/Right`，来自
    `Tag::Table(alignments)`，越界按 `None` 取）；`columns`、`first_line`、`last_line`。
- cell 区间取 `Start(TableCell)..End(TableCell)` 的事件范围再去掉首尾 ASCII 空白：
  含 `|` 边界以外的全部源码，**不含**左侧 `|` 与两侧空白。
- **escaped `\|`** 是 cell 原文（`c\|d` 一个 cell）；GFM 规定行内代码里的字面 pipe
  也必须转义 —— 未转义的 `` `a|b` `` 会按列边界切开（golden 测试钉住“不 panic、列数
  稳定、源码逐字节保留”，不自行改写）。
- **表格不再进 `unsupported_spans`**：它有块树，属于“已支持原子块”；把整表塞进未支持
  块会与 Object Block 判定冲突。新增 `ParsedMarkdown::opaque_spans()` 统一给出
  “代码 + 表格 + 未支持块”的原子区间，editor 用它抑制内部行内投影。
  表格内部的行内构造（`**`、`` ` ``、链接、wiki）与表格内图片都不参与投影；其它
  未支持块（HTML / Setext / 无空格 marker）行为不变。
- 无表头异常：只有一行、首行是分隔行等输入 `pulldown-cmark` 根本不发 Table 事件
  （按普通段落处理），`tables` 为空且不产生未支持块；空表头 `| | |` 是合法表格。

### 10.2 editor：统一 `ObjectBlock` 抽象

- 新增 `crates/editor/src/model/object.rs`：`ObjectKind { Image, Table }` +
  `ObjectBlock { kind, span, line, end_line, index }`。**图片与表格共用一套**投影 /
  选中 / 源码切换 / 删除路径，没有第二套平行逻辑：
  - `MarkdownDocument::objects()`：按源码顺序的全部整块对象；
  - `object_at_line(line)` / `object_image(object)` / `object_table(object)`：通过
    `kind + index` 取载荷（图片 → `images()`，表格 → `tables()`）；
  - 图片专用 `images()/block_image_at_line()/block_images()` 保留为便捷入口。
- 表格的 block 判定（写死，测试钉住）：第一行是 `LineKind::Table`、不是引用、不在列表
  项里、源码从行内容起点开始（无缩进）、区间结束在末行内容终点且至少覆盖表头 + 分隔行。
  引用/列表/缩进里的表格仍有完整块树，但 `is_block = false` → 首版原样显示源码。
- `projection_pieces_with_objects` 遍历 `objects()`：不在 `revealed_object_lines` 的
  整块 → 一个 `ProjectionPiece::Object`（display 是 `U+FFFC` 占位符）。映射语义与图片
  完全一致：块内部（含起点）→ 占位符起点；占位符内部 → 块起点（**不可能把 caret 放进
  单元格**）；选区覆盖占位符 → 反查完整表格 Markdown；文本选区相交**不**展开源码。
- `EditorController`：`objects()/object_at_line()/image_of()/table_of()`；
  `begin_object_source_edit(line)` / `end_object_source_edit()` / `delete_object(line)` /
  `object_lines_in_display_selection()` 全部改成按对象行（图片所在行 / 表头行）寻址；
  源码模式只由工具条 / `F2` 设置，`Esc` / `Ctrl+Enter` / 点击块外退出；语法被改坏
  （对象消失）时自动退出。
- 删除：`delete_object` 选中整块 source 跨度后走同一条 `compose_edits` 路径，一次
  `EditRequest`（单 `ScalarEdit`）删掉表头/分隔行/全部数据行。

### 10.3 app：几何 / 绘制 / 命中 / 工具条

- **显示行号 ≠ 源码行号**：表格把 N 行源码压成一个 display 行，后面的 display 行整体前移。
  `block_metrics` 改为先把“display 行首字节”经投影映射回源码字节，再按源码行取度量
  （源码行起点前缀数组 + 二分）；否则表格之后的标题字号 / 列表缩进会整体错位。headless
  用 `visual_row_metrics()`（视觉行度量快照）钉住这一点。
- 表格行高 = `TABLE_HEADER_HEIGHT + rows × TABLE_ROW_HEIGHT`（固定值，布局度量阶段即可
  给出，不需要先 shaping），对象行高再加 `TABLE_PADDING × 2`；绘制矩形 = 表格内容矩形，
  与命中/工具条共用**同一个** `ObjectPaint.rect`。
- 自绘（`paint_table`）：外框 + 表头底色 + 单元格文本 + 1px 网格线；对齐来自 GFM 分隔行
  （`TextAlign` + `align_width` = 单元格内宽）；单元格文本按**真实 shaping 宽度**截断并
  补 `…`（`glyph::shape_row_truncated_at`），不做字符数估算；`\|` 显示为 `|`。
- 长表格：只 shaping / 绘制与视口相交的行（`table_paint` 在**元素局部坐标**里按
  `0..viewport_height` 过滤；`rect` 已经减过 `scroll`，不能再用内容坐标比较，否则滚到
  长表格中间会整表空白）；整表滚出视口后对象不绘制、不产生 hitbox。窄窗口：列宽 =
  `max(内容宽/列数, TABLE_MIN_COLUMN_WIDTH)`，超出部分由元素内容遮罩裁剪，不缩放、
  不换行、不 panic。
- 命中：鼠标按下先命中对象（整块选中，caret 落到块末尾），不会落进单元格；工具条只有
  「编辑源码」（表格没有「放大」），图片仍是「放大 + 编辑源码」两按钮。
- 元素层把对象矩形（含表格内部的表头/行/单元格矩形）统一从元素局部坐标平移到窗口坐标；
  表格单元格不注册独立 hitbox（整块一个可点击区域）。

### 10.4 验证与已知限制

- `cargo fmt --all -- --check`、`cargo clippy -p menghunote-markdown -p menghunote-editor
  -p menghunote-app --all-targets --features dev-sync -- -D warnings` 通过；
  `cargo test -p menghunote-markdown` **28**、`-p menghunote-editor` **170**、
  `-p menghunote-app --features dev-sync` **434**（本机 Linux，0 failed）。
  与 S2b/S3 基线的差值：markdown +6（表格 golden）、editor +13（`table_object.rs`）、
  app +7（headless 表格 5 + `view` 纯函数单测 2）。
- 新增 app headless（`crates/app/tests/headless_tables.rs`，真实 `AppShell` + 临时 vault，
  `#[gpui::test]`、不 sleep）：点击表格不出源码 + caret 不落进单元格 + 工具条只有「编辑
  源码」；工具条/F2 进源码、`Esc`/点击块外退出；长表格按行裁剪 + 滚出视口不绘制；
  300/240px 窄窗口不 panic；表格之后的标题/嵌套列表度量正确；删除整表落库。
- 未做真机像素验收：表头底色/网格线/对齐/手形光标/DPI 仍需 Windows `ui_smoke.ps1`；
  本环境只证明“解析 → 投影 → 几何 → 命中 → 工具条 → 事务”链路。
- 首版单元格纯文本（行内 `**`/`` ` ``/链接标记原样显示，不解析富文本）；表格不做横向
  滚动（窄窗口按裁剪处理，`TABLE_MIN_COLUMN_WIDTH` 保证可读）。
- 对象行仍以**行号**作为身份（与图片一致）：在表格源码模式里在表格**之前**插入/删除
  行会让对象行号变化，源码模式会自动退出（需重新点「编辑源码」）。这是既有图片语义，
  S3a 未改。
- GFM 规定表格在第一个空行处结束：紧跟在表格行之后、没有空行的普通文本会被
  `pulldown-cmark` 当成**表格数据行**（源码逐字节保留，不重写；这是解析器/GFM 语义，
  不是本切片的投影选择）。

### 10.5 S3b 范围（S3a 当时的明确不做项）

单元格就地编辑（单击进 cell、只显示 cell 文本）、Tab/ShiftTab 单元格导航、加行/加列
（含边缘「+」与对齐行同步扩列）、行列把手与删除、拖拽重排行/列、合并单元格、嵌套表格、
表格内图片、表格排序、复制单元格 Markdown、表格横向滚动。表格源码仍是唯一真源；
插件/外部编辑产生的非规范表格按原样显示，不静默重写。

其中「加行/加列」已在 §11、单元格选择与就地编辑在 §12、删除行/删除列在 §13
落地（同一契约：源码唯一真源、不重写非规范表格）；拖拽重排、合并单元格、横向滚动等
其余各项仍未做。

## 11. S3b 实施记录（2026-11-17，表格 Object 加行 / 加列）

> 范围：**表格 Object 的「加行」「加列」两个结构动作**（入口、源码构造、事务、几何刷新）。
> 明确**未做**：单元格就地编辑、Tab/ShiftTab 单元格导航、行列删除、行列拖拽重排、
> 合并单元格、嵌套表格、表格内图片、表格排序、复制单元格 Markdown、表格横向滚动。
> 不改 schema / 同步 wire / tag；表格仍是 Object Block，源码仍是唯一真源。

### 11.1 源码构造（`menghunote-editor::model::table_edit`）

- 新增纯函数模块 `table_edit`，只产出「在哪个 source 字节处插入哪段文本」
  （`TableInsertion { at, text }`，**恒为空区间 = 纯插入**），不接触 Rope / 会话 /
  投影 / UI：
  - `append_row_insertions`：插入点 = 整表区间末尾（= 最后一行内容终点，不含换行；
    没有数据行时就是分隔行末尾），插入 `\n + 空数据行`。空数据行 =
    `|  |  | …`（列数个空 cell）；行尾取**文档已有**的换行（CRLF 文档插 CRLF）；
  - `append_column_insertions`：表头、分隔行、**每一条数据行**的行内容末尾各一条插入。
    普通行：行尾是 `|` → 追加 `" |"`，否则 `" | |"`（两种都让 GFM 多解析出一个
    空 cell）；分隔行：行尾是 `|` → 追加 `" ---|"`，否则 `"| ---"`（分隔 cell 必须是
    `-+` 形状，不能用空 cell）。新列默认对齐（无冒号），原有冒号/对齐标记一个字节不动。
- 只插入、不替换：行首 `|`、cell 之间的空白、分隔行的对齐标记**逐字节保留**；
  不复制上一行内容；列数由 GFM 分隔行决定，所以加列必须三处都追加（缺一列会被
  解析回旧列数），加行只多一条数据行、不影响列数。
- `empty_row_text(columns)` 对 0 列按 1 列收口（不产生 `|` 这种空行）。

### 11.2 控制器事务与 caret（`EditorController`）

- `append_table_row(line)` / `append_table_column(line)`：只在行号上是**整块表格**
  （`ObjectBlock` 且 `kind == Table`）时生效，否则 `Ok(None)`（不产生请求、不改正文）；
  加行 1 条 `ScalarEdit`，加列 `rows + 2` 条（按位置升序、互不重叠），**一个
  `EditRequest` = 一个撤销组**（一次 `Ctrl+Z` 撤回整次点击）。
- caret/选区规则：端点按**文本位置**映射 —— 插入点在端点之前或正好等于端点时一起
  平移（表格末尾 → 新表格末尾），因此结构操作后 caret **绝不会落进表格源码内部**
  （对象内部没有文本位置，否则投影会把 caret 折回块首，下次输入就跑到表格前面）。
  表格之外的 caret/选区不受影响（插入点都在它之后）。
- 不切换源码模式：`object_edit_line` 不变，表格继续投影成一个 Object 占位符；
  工具条「编辑源码」/ `Esc` 往返（S3a）完全不受影响。
- IME 组合中沿用 `ensure_editable` 拒绝；app 侧再挡一层（`is_composing` / 同步持闸）。

### 11.3 app：右下角浮层与命中（`crates/app/src/editor/view.rs`）

- 选中或悬浮整块表格时，在表格**右下角外侧**显示胶囊浮层：两枚小按钮
  `+ 行` / `+ 列`（debug selector `table-actions` / `table-insert-row` /
  `table-insert-column`）。点击分别走 `insert_table_row` / `insert_table_column`
  → 控制器事务（app 不自己拼 `ScalarEdit`、不改 body）。
- 几何（写死，headless 钉住）：
  - 浮层**整体落在表格底边之下**（`top >= table.bottom`），右边缘贴住表格右边缘，
    因此不遮挡任何单元格文本；宽度含 1px 边框与 `Bounds` 实际尺寸逐像素一致；
  - 右上角悬浮工具条（编辑源码）比表格更矮时，浮层再往下让开，避免两个胶囊重叠；
  - 夹在视口内（窄窗口不越界）；表格底边已滚出视口时不画（此时“右下角”不在屏幕上，
    宁可不给入口，也不让浮层压住可见单元格）——**已知限制**：长表格需滚到表格
    底部才出现边缘入口；
  - 图片没有这个浮层；**源码模式下不显示**（源码模式不是 Object）。
- 命中：`point_in_toolbar` 现在同时包含工具条与表格浮层的窗口矩形，元素层的全局
  mouse-down 会先放行这些控件（点击按钮不挪 caret、不开始拖选、不改变选中态）；
  指针悬停在浮层上时 hover 目标保持，不会因“离开表格”而闪断。
- 点击后 `cx.notify()`：下一帧按新的行数/列数重算 `LayoutFrame` 行高与表格矩形
  （行高 = 表头 + 数据行 × 固定行高 + 上下内边距），绘制/命中/工具条同一套几何。
- S3a 行为不回退：单击只选中整块、不展开源码；工具条「编辑源码」/`F2`/`Esc`/点击
  块外往返；长表格按视口裁剪；窄窗口裁剪不 panic —— 都在 headless 里继续覆盖。

### 11.4 验证

- 新增 `crates/editor/tests/table_edit.rs`（11 个纯模型用例 + `table_edit` 模块内 1 个
  `empty_row_text` 单测）：2×2 加行逐字节、空 cell、
  分隔行不变；无数据行加行/加列；三种对齐的加列（对齐保留 + 新列默认）；无尾管道
  表格加列；紧凑写法的字节保留；CRLF；非表格/引用表格/越界行 no-op；仍是 Object；
  caret 在表格前后两种情形。
- 新增 `crates/app/tests/headless_table_edit.rs`（6 个真实 `AppShell` 用例）：
  选中 → 右下角浮层（在表格外面、右对齐）→ 加行/加列逐字节 + 行列数 + 几何长高 +
  仍 Object + caret 不落进表格；悬浮（未选中）也能加行且 caret 不动；一次 `Ctrl+Z`
  撤回整次点击 + 重做；重开 vault 行列与 Object 一致；图片无此浮层、源码模式隐藏；
  320px 窄窗口浮层夹在视口内且仍可点击。
- 命令与计数（本机 Linux）：
  `cargo fmt --all -- --check`、`cargo clippy -p menghunote-markdown -p menghunote-editor
  -p menghunote-app --all-targets --features dev-sync -- -D warnings` 通过；
  `cargo test -p menghunote-markdown` **28**、`-p menghunote-editor` **182**、
  `-p menghunote-app --features dev-sync` **440**，0 failed。
  与 S3a 基线的差值：editor +12（表格编辑 11 + 模块单测 1）、app +6（headless 表格编辑）；
  markdown 无改动（表格块树已经够用，本切片不碰解析器）。
- 未做真机像素验收：按钮观感/DPI/手形光标仍需 Windows `ui_smoke.ps1`；本环境只证明
  “几何 → 命中 → 一次 `EditRequest` → 源码/undo/持久化”链路。

## 12. S3b2 实施记录（2026-11-17，表格单元格选择与就地编辑）

> **2026-11-17 交互修正（用户实测反馈）**：编辑态不再把整表投影成一行 cell 文本；
> **单击单元格直接进入原地浮层编辑**。当前实现以 §12.6 为准，§12.1–§12.5 保留为
> 当时（控制器内 cell 编辑态）的实现记录。

> 范围：**单击选中单元格 / 再次单击（或双击 / Enter / F2）进入该 cell 的就地编辑 /
> Enter 提交 / Tab 下一格 / Esc 取消 / 点击 cell 外提交**。表格源码仍是唯一真源。
> 明确**未做**：行列删除、行列拖拽重排、合并单元格、嵌套表格、表格内图片、表格排序、
> 复制单元格 Markdown、表格横向滚动、单元格内富文本（首版纯文本显示）。
> 不改 schema / 同步 wire / tag；删除行列等其余 S3b 项继续后置。
> （行列删除已在后续 §13 / S3b3 落地。）

### 12.1 交互与状态机

- **单击单元格**（`click_table_cell`）：用上一帧 `table_paint` 的
  `TableCellPaint.rect` 命中（与绘制/工具条同一份几何，不按字符宽度另算），
  只标记「选中该格」并给格子画 2px 强调色描边 + 浅底色；表格仍是 **Object**
  （display 里没有任何 `|` / 整表源码），**正文 caret 一动不动**（这一点与 S3a 的
  “点整块 → caret 落到块末尾”不同：点格子不再移动正文 caret）。
- **再次单击同一格 / 双击（`click_count >= 2`）/ `Enter` / `F2`**：进入该 cell 的
  **就地编辑**。三处入口共用 `EditorController::begin_table_cell_edit(object_line, row, column)`。
- **编辑态语义**（写死，测试钉住）：
  - display **只显示该 cell 的源码原文**：投影把整表变成
    `Hidden(表头..cell.start) + Copy(cell) + Hidden(cell.end..表尾)`（`ProjectionCellReveal`），
    因此没有 `|`、没有分隔行、没有其它 cell，也没有整表源码；
  - caret/选区**只在该 cell 文本内**（`set_selection` 把两端夹进 cell；表格其余部分在
    display 上不可见、也没有文本落点）；
  - **不进入整表源码模式**（`object_edit_line` 不变），工具条「编辑源码」与边缘浮层在
    编辑期间隐藏（它们只对 Object 出现）。
- **提交 / 取消 / 导航**：
  - `Enter` → `commit_table_cell_edit`：**唯一一条** `ScalarEdit`（进入编辑前的 cell 区间
    → 当前 cell 区域文本），一个 `EditRequest` = 一个撤销组；提交后表格重新投影成 Object，
    caret 落到整表末尾；
  - `Tab` / `Shift+Tab` → 先提交，再按最新解析的行列数进入下一格（行末回卷到下一行行首；
    最后一格之后退出表格）；`row 0 = 表头` 是与控制器统一的寻址口径；
  - `Esc` → `cancel_table_cell_edit`：把镜像里的 cell 区域恢复成进入前的原文，
    **不产生任何请求**（因此撤销历史里不会留下“一次取消”）；
  - 点击 cell 外 / 关闭或切换文档 → 先提交（`drain_and_close` 里显式提交，持闸中则放弃草稿）；
  - 编辑期间 `Ctrl+Z` 解释为“取消这次未提交的编辑”（不能把会话历史按 live 坐标套到
    领先于会话的镜像上）。

### 12.2 为什么“草稿只写镜像、提交才发请求”

display 永远由镜像派生（投影层只认 source），所以编辑中的 cell 文本必须先进镜像才可见；
但这些按键**不产生** `EditRequest`：

- 一次单元格编辑在提交时只产生一条 `ScalarEdit` → 会话里就是一个撤销组（本仓 `UndoManager`
  没有开时间合并窗口，逐键发请求会变成逐键撤销）；
- 提交时请求用的**是进入编辑时的坐标**，而镜像此刻已经等于提交后的文本 → 控制器不再二次
  应用，只等会话回 `local_echo` 确认；
- 文本与进入时相同 → **不发**请求（空 op 请求不会推进 `live_rev`，会把单飞行的 `pending` 卡死）；
- 唯一的临时不一致是“镜像领先于会话”，它有确定的收敛点（提交一条请求 / 取消恢复原文）；
  远端 delta、撤销、重做在改镜像前先 `abort_cell_edit_draft()` 恢复基线，绝不带病套用 splice；
- 结构类操作（加行加列 / 删除对象 / 任务框 / 列表 Enter / 整表源码模式）在编辑期间一律
  `Err(EditorError::CellEditActive)`：它们按会话坐标改正文，与未提交的草稿不能混。

### 12.3 GFM 源码安全（转义与行尾）

- 进入 cell 的替换文本统一经 `table_cell_edit::sanitize_cell_text`：`\r\n`/`\n`/`\r` → 空格、
  **未转义**的 `|` → `\|`（前面连续反斜杠为奇数时视为已转义，保持原样，`a\|b` 不会被写成
  `a\\|b`）；因此输入 `|` 不会把一格切成两格、粘多行文本也不会切断表格行。
- 提交按“原始 cell 区间起点 + 长度增量”切镜像（`current_cell_region`），**不**用最新解析的
  裁剪区间：解析器会裁掉 cell 首尾空白，用裁剪区间提交会让镜像与会话差那几个字节。
- cell 的两侧 `|`、分隔行（含 `:--` / `--:` 对齐标记）、其它 cell、行尾换行**逐字节不变**；
  cell 内的 `**` / 反引号 / 链接标记首版原样显示（纯文本），源码编辑逐字保留。
- 缺列补出来的空区间（cell 起点已在行内容末尾之外、行里根本没有这一格的 `|` 边界）**拒绝
  进入编辑**：往那里插字会写到表格行之外、把表格写坏；加行/加列产生的空 cell 有真实边界，
  正常可编辑。

### 12.4 app 接线

- `EditorView` 只持“选中的单元格” `(对象行, row, column)`；编辑态在控制器里
  （`is_editing_table_cell` / `table_cell_edit_target`），app 每帧询问，不做第二份真源。
- `TableRowPaint` 增加 `row`（0 = 表头）：长表格按视口裁剪后 `rows` 只含可见行，行下标不能
  由位置推断；`EditorGeometry::table_cell_at` 用同一份矩形做命中 →
  `EditorView::click_table_cell` → 选中或进入编辑。
- 编辑态的行高按**普通文本**度量（`block_metrics` 跳过该对象行的表格高度），否则表头格会
  撑成整张表的高度、caret/选区/点击全部错位。
- 点击 cell 外提交时，表格重新变成一个占位符（display 由“cell 文本”变成 3 字节），
  因此点击位置按这个差值换算到新 display 再落 caret，不会错行。
- 状态栏在编辑期间显示「单元格编辑（第 N 行 row=R col=C）」：display 只剩一行文本，
  没有这句提示用户会以为表格被删掉了。
- 平台可能为 `Enter` / `Tab` 补发 `\n` / `\t` 文本输入：两者都要消费掉
  （`pending_enter_text_input` / `pending_tab_text_input`），否则一次提交会多一个换行、
  一次 Tab 会把制表符插进下一格。

### 12.5 验证

- `crates/editor/tests/table_cell_edit.rs`（20 个纯模型用例）+ 模块内
  `table_cell_edit` 单测 5：进入后 display 只有 cell 文本（无 `|`/无占位符）、
  输入立刻可见且只改该 cell（其余逐字节）、提交只有**一条** `ScalarEdit`
  且区间正好是进入前的 cell 区间、`|` 转义与换行→空格、Esc 恢复原文且不产生请求、
  无改动提交不发请求、Tab/Shift+Tab 走格与退出、caret/选区夹在 cell 内、
  加行/加列后的新空 cell 可编辑、空 cell 可编辑、缺列补的空区间拒绝进入、
  IME 上屏进 cell、编辑期间结构操作被拒、远端 delta 放弃草稿、一次提交=一个撤销单元、
  对齐标记/行尾换行逐字保留、编辑一张表不影响另一张表仍投影成 Object、
  单元格内 `**`/反引号/链接标记按纯文本显示且字节逐字保留。
- `crates/app/tests/headless_table_cell.rs`（12 个真实 `AppShell` 用例）：
  单击选中格（表格仍 Object、display 无源码、caret 不动）、再次单击/F2/双击进入编辑
  （display 只有 cell 文本、caret 在格内）、输入 + Enter 提交（一次 `Ctrl+Z` 撤回整格输入 +
  重做）、Esc 取消（源码逐字节恢复、不进 undo）、Tab 提交 + 走格 + 末格退出、
  点击外面提交并落 caret、加行/加列后新空 cell 可编辑（逐字节）、退格与 IME(F9) 走同一路径、
  纯文本粘贴进格（换行→空格、`|`→`\|`）、选中格 Enter 进入编辑、`|` 输入不破表格结构、
  持久化后重开仍可编辑。
- S3a/S3b1 回归（`headless_tables.rs` / `headless_table_edit.rs`）已按新语义更新：
  点格子不再移动正文 caret（整表选中/工具条/`+ 行`/`+ 列`/源码模式切换/长表格裁剪
  全部保持）；有单元格选中态时 `F2` = 进入单元格编辑，`Esc` 清掉单元格选中态后
  `F2` 仍是整表源码模式。
- 命令与计数（本机 Linux）：
  `cargo fmt --all -- --check`、`cargo clippy -p menghunote-markdown -p menghunote-editor
  -p menghunote-app --all-targets --features dev-sync -- -D warnings` 通过；
  `cargo test -p menghunote-markdown` **28**（未改解析器）、`-p menghunote-editor` **207**
  （+25：单元格编辑 20 + 模型单测 5）、`-p menghunote-app --features dev-sync` **452**
  （+12：headless 单元格；S3a/S3b1 的两个用例按新语义更新断言，不增数），0 failed。
- 未做真机像素验收：单元格选中描边/底色、DPI 下的命中、真实 IME 候选窗位置仍需 Windows
  `ui_smoke.ps1`；本环境只证明“几何 → 点击 → 一次 `EditRequest` → 源码/undo/持久化”链路。
- 已知限制（首版写死）：
  - 编辑一个单元格时，display 上整张表只剩这一行文本（表格网格暂时消失），空 cell 编辑时
    就是一个空行——这是“只显示该 cell 文本”的直接后果；
  - 编辑期间点击**同一张表的另一格**会先提交并落 caret（不会直接进下一格），再点一次才
    选中/编辑它；跨格连续输入请用 `Tab`；
  - `\|` 在编辑态按**源码**显示（`|` 的转义形态），只读态仍显示为 `|`；
  - 表格源码被改坏（表格/该格消失）时自动退出编辑，不猜新坐标。

### 12.6 交互修正：cell 矩形上的原地浮层编辑（2026-11-17）

> 触发：用户实测反馈「表格编辑时其它行列必须仍然可见；单击单元格直接编辑，不要 F2」。
> 结论：§12.1 的“display 只显示该 cell 文本”交互**废弃**（控制器内的 cell 编辑态/
> 投影显现仍保留在 `menghunote-editor` 并继续由纯模型测试钉住，但 app 不再使用）。

- **状态**：`EditorView` 持 `CellEditOverlay { line, row, column, input, draft }`；
  `input` 就是侧栏重命名复用的 `RenameInput`（新增 `RenameMode::Cell` 与
  `RenameState::at_end`：光标在末尾、Enter 提交原始文本、Tab/Shift+Tab 发
  `Advance{forward,text}`、`Ctrl+V` 走平台剪贴板、F9 模拟 IME 上屏）。
  **没有第二套 IME 实现**；Title 模式的侧栏行为逐字不变。
- **绘制**：表格始终是完整 Object（`table_paint` 照常画表头/所有数据行/网格线），
  输入框是一个绝对定位 div，矩形 = 被点 cell 的 `TableCellPaint.rect`（与
  `table_cell_rects()` 同源），只覆盖这一格；超长文本/IME preedit 在 cell 内裁剪。
- **单击 / 双击**：`click_table_cell` 直接 `begin_table_cell_edit`，不再要求先选中、
  也不需要 F2；`F2` 现在只表示**整表源码模式**（有图片对象时仍是图片源码）。
- **提交**：新增 `EditorController::replace_table_cell_text(line,row,column,text)`：
  一次调用 = 一条 `ScalarEdit`（替换整个 cell 源码区间）+ 一个撤销组；文本先经
  `sanitize_cell_text`（换行→空格、未转义 `|`→`\|`）；与当前源码相同 → `Ok(None)`
  （不发空 op）；caret 提交后落到整表末尾。编辑期间正文源码**一个字节都不提前改**。
- **取消/点外/切格**：`Esc` 丢弃浮层草稿（零请求，无 undo 垃圾）；点击浮层外先提交，
  再继续按正常命中处理（点另一格 = 提交并直接开始编辑新格；点正文 = 落 caret）。
  `Tab` 提交后按最新表格行列进入下一格，末格退出。
- **结构闸门**：编辑期间整表工具条、加行/加列/删除浮层、行列拖拽把手全部隐藏
  （按钮路径同时 API 级拒绝）；`drain_and_close` 用浮层里最近一次的 `draft` 提交，
  持闸中则丢弃草稿。
- **坐标简化**：因为表格不再被压成一行文本，提交前后 display 长度/布局不变，
  点击位置不需要再做 “cell 文本 ↔ 占位符” 的字节换算。
- **滚轮（同轮反馈）**：图片放大遮罩 `on_scroll` 以 0.5–4.0 有界倍率缩放并
  `stop_propagation()`；图谱覆盖层 `.on_scroll` 缩放后同样 `stop_propagation()`，
  底层 `EditorView` 的窗口级滚动监听不再收到这两类滚轮事件。
- **测试**：`headless_table_cell.rs` 11 个用例（单击即编辑 + 整表仍绘制/输入框 bounds
  在 cell 内、输入/Enter 单请求单撤销、Esc 零请求、点外提交、直接点另一格切格、
  Tab/双击、IME(F9)+退格+多行粘贴、加行加列新格、持久化重开、空串清格、`|` 转义、
  Enter 不破结构）；`headless_images.rs` 新增滚轮缩放/阻断穿透/上下界用例；
  `headless_graph.rs` 新增“图谱滚轮不滚动底层编辑器”用例；其余表格拖拽/加删/任务/
  图片/图谱用例全部保留并按新交互调整入口（编辑期先 `Esc` 再点浮层）。

## 13. S3b3 实施记录（2026-11-17，表格删除行 / 删除列）

> 范围：**表格 Object 的「删除行」「删除列」两个结构动作**（入口、源码删除区间、
> 事务、caret/选中格修正）。表格源码仍是唯一真源。
> 明确**未做**：表头行删除、拖拽重排、合并单元格、嵌套表格、表格内图片、表格排序、
> 复制单元格 Markdown、表格横向滚动、单元格内富文本；不改 schema / 同步 wire / tag。

### 13.1 源码删除区间（`menghunote-editor::model::table_edit`）

- 新增 `TableDeletion { span }`（两端是 scalar 边界的 source 区间；控制器把它变成
  `replacement = ""` 的 `ScalarEdit`），以及两个纯函数：
  - `delete_row_deletion(source, table, row)`：只接受**数据行**（`row == 0` 表头、
    越界 → `None`）；删除区间 = 行内容 + **本行行尾换行**（CRLF 文档连 `\r\n`），
    因此不留空行；表格在文档末尾、最后一行没有换行时改从**前一行行尾**开始删，
    否则会残留一个空行；
  - `delete_column_deletions(source, table, column)`：`columns() <= 1` 或越界 → 空
    （**至少保留 1 列**：GFM 分隔行不能没有分隔 cell）；表头 / 分隔行 / 每条数据行
    各产出一条删除区间（按位置升序、互不重叠）：
    - 非末列：删 `[本列文本起点, 下一列文本起点)`（本列文本 + 尾部空白 + 分隔 `|` +
      下一列前导空白）；
    - 末列：删 `[上一列文本终点, 本列文本终点)`（保留本列后侧空白与收尾 `|`）；
    - 分隔行没有 cell 事件，按 `|` 切段 + 裁首尾空白得到每列区间（GFM 分隔 cell 只含
      `-`/`:`/空白，`|` 恒是列边界）；切出的 cell 数 ≠ 列数时**不动手**（返回空）；
    - 缺列补出来的空区间跳过（GFM 会自动补空 cell，不重写这一行）。
- 删除只删这一段字节：其它 cell 的源码、行首/行尾 `|`、分隔行冒号（对齐）逐字节保留。

### 13.2 控制器事务与 caret（`EditorController`）

- `delete_table_row(line, row)` / `delete_table_column(line, column)`：只在行号上是
  **整块表格**（Object + Table）且参数合法时生效，否则 `Ok(None)`；一次点击产生
  **一个** `EditRequest`（删列是 `rows + 2` 条 `ScalarEdit`，同一个撤销组）→
  一次 `Ctrl+Z` 撤回整次删除。
- caret/选区按**删除区间**做文本位置映射（区间之后左移删除量；落在区间内部收敛到
  区间起点），随后再夹到**删除后的表格之外**：caret 绝不会落进表格源码内部；表格
  之外的 caret/选区只按删除量左移。
- 不切换源码模式（`object_edit_line` 不变），表格继续投影成一个 Object 占位符；
  单元格编辑期间与加行加列同一闸门（`ensure_no_cell_edit` → `CellEditActive`）。

### 13.3 app：浮层入口与选中格修正（`crates/app/src/editor/view.rs`）

- 右下角浮层按 S3b1 的几何继续贴住表格右下角外侧，新增两枚按钮：
  debug selector `table-delete-row` / `table-delete-column`。
  - **只在选着某个仍存在的单元格时显示删除**（悬浮或只选中整块时只有 `+ 行`/`+ 列`，
    避免误删）；`Esc` 退出单元格选中态后删除按钮一起消失；
  - 选中**表头行**时不提供「删除行」（首版明确不删表头）；「删除列」始终提供；
  - **只剩 1 列**时「删除列」是禁用态：弱化文字（`table_actions_text_disabled`）、
    无 hover、点击只记日志且**不产生任何请求**；控制器再兜一层 `Ok(None)`；
  - 宽度按按钮数与两类按钮宽度逐像素计算（插入按钮 34px / 删除按钮 46px，3 个汉字的
    `删除行` 需要 44px）；**窄到编辑区放不下整条浮层时隐藏删除组**，优先保住 S3b1
    “浮层必须夹在编辑区内”的契约（已知限制：极窄窗口只剩加行加列）；
  - 无键盘快捷键 → 删除只可能来自带位置语义的按钮点击（没有“无确认的快捷键直接删”；
    `Ctrl+Delete` / `Ctrl+Backspace` 不改表格结构，headless 钉住）。
- 点击后 `delete_table_row` / `delete_table_column` → 控制器事务，随后按**最新解析**的
  表格修正单元格选中态（纯函数在 `table_cell_edit`）：
  - 删行：同列**上一行**优先；删的是第一条数据行 → 落到下一行同列（上移补位）；
    没有数据行可落 → 清空选中态；
  - 删列：同行**右侧格**优先（右邻左移补位），删末列 → 左侧格；
  - 修正只改 app 的选中地址，不产生第二个请求；caret 由控制器映射，app 只保证可见。
- caret 不落进表格：headless 用例先在单元格编辑里 `Enter` 提交（caret 到整表末尾），
  再删行，断言 caret 恰好落在**新表格末尾**。
- S3a/S3b1/S3b2 行为不回退：整表选中/编辑源码/长表格裁剪/加行加列/单元格编辑与
  `Tab` 导航全部继续通过。

### 13.4 验证

- `crates/editor/src/model/table_edit.rs` 模块内单测 12 个（新增 11）：删除中间/首/末
  数据行逐字节、文档末尾无换行不留空行、CRLF、表头/越界 no-op、中间/首/末列
  （表头/分隔行/所有数据行，对齐保留）、无尾管道、无数据行、只剩 1 列拒绝、
  缺列行跳过；`table_cell_edit` 模块新增 2 个（删除后选中格修正规则）。
- `crates/editor/tests/table_edit.rs` 新增 10 个：删行/删列的请求形状（单删行 1 条、
  删列 `rows + 2` 条、纯删除、按位置升序）、逐字节结果、仍是 Object/源码模式不回退、
  caret 在新表格末尾或表格外、表头/越界/非表格 no-op、编辑期间被拒、CRLF。
- `crates/app/tests/headless_table_delete.rs`（7 个真实 `AppShell` 用例）：
  未选格不显示删除；选格 → 删行逐字节 + 行数 + 仍 Object + 选中格上一行 + caret 不动 +
  几何/绘制行数 + 一次撤销/重做；表头不提供删行 + 删列保留对齐 + 一次撤销；
  删首/末列逐字节 + 选中格修正；只剩 1 列时禁用且点击无请求 + 删唯一数据行后
  清空选中态；重开 vault 行列与 Object 一致；`Esc`/悬浮时删除入口消失、
  `Ctrl+Delete`/`Ctrl+Backspace` 不动表格；提交单元格编辑后再删行 caret 落新表格末尾。
- 命令与计数（本机 Linux）：
  `cargo fmt --all -- --check`、`cargo clippy -p menghunote-markdown -p menghunote-editor
  -p menghunote-app --all-targets --features dev-sync -- -D warnings` 通过；
  `cargo test -p menghunote-editor` **230**（+23：模型单测 13 + 集成 10）、
  `cargo test -p menghunote-app --features dev-sync` **459**（+7）、
  `cargo test -p menghunote-app` **228**（+7），0 failed；markdown 未改（**28**）。
- 未做真机像素验收：删除按钮观感/禁用态颜色、DPI、手形光标仍需 Windows
  `ui_smoke.ps1`；本环境只证明“几何 → 点击 → 一次 `EditRequest` → 源码/undo/持久化”。
- 已知限制（首版写死）：
  - 不删表头行（删掉 GFM 表格会整体消失）；
  - 极窄编辑区（比整条 4 枚按钮还窄）隐藏删除组，只剩加行加列；
  - 删除列只删该列一侧的分隔符与相邻空白，其它列的空白形状保留原样（不重排表格）；
  - 单元格编辑期间删除按钮不显示（表格已不是 Object），提交后才恢复。

## 14. S3b4 实施记录（2026-11-17，正文右键「插入 → 表格」）

> 范围：**正文右键菜单 + 「插入 → 表格」**（菜单入口、默认表格源码、插入事务、
> Object 投影）。表格源码仍是唯一真源。
> 明确**未做**：子菜单（「插入」目前是分组标签 + 「表格」一项）、其它插入项、
> 右键落 caret、菜单里的删除/复制、表格拖拽重排/合并单元格/横向滚动。

### 14.1 默认表格与插入文本（`menghunote-editor::model::table_edit`）

- `new_table_text(eol)`：默认表格 = **2 列 × 1 条数据行**，cell 全空、分隔行 `---`
  （默认对齐），恒以行尾结束：`|  |  |` / `| --- | --- |` / `|  |  |`。
- `new_table_insertion(source, start, end)`：只回答“这次插入的文本是什么”，保证表格
  **独占行块**：
  - 前面：文档开头 / 已紧跟空行 → 不补；行首（前一个字节是行尾）→ 补一个行尾；
    行中间 → 补两个行尾（结束当前行 + 空行）；
  - 后面：原文为空 / 紧随其后是行尾（已有空行）→ 不补；否则补一个行尾；
  - 行尾跟随文档（CRLF 用 CRLF），不与 LF 混用；越界 / 非 scalar 边界返回 `CoordError`；
  - `start < end` 时语义是“替换选区”（与打字一致），由控制器决定。
- 纯插入时正文其它字节逐字节不变（前后只按需要补空行）；插入出来的表可投影成
  Object Block（表头 + 分隔行 + 数据行都满足 GFM）。

### 14.2 控制器（`EditorController::insert_table`）

- 在当前 **caret/选区**处插入默认表格：一个 `EditRequest`（单 `ScalarEdit`），
  `start == end` 是纯插入，非空选区则替换；由会话默认撤销组进 undo（一次 `Ctrl+Z`
  撤回整次插入）。
- caret 落在插入文本之后 = **表格之后**（插入文本恒以行尾结束，表格对象内部没有
  文本位置，caret 不会落进表格源码内部）。
- 插入后按新 source 重扫 Markdown：表格是整块 Object 时下一帧就按 Object 投影；
  不切换源码模式。单元格编辑期间与其它结构操作同一闸门（`CellEditActive`）。

### 14.3 app：正文右键菜单（`crates/app/src/editor/view.rs`）

- 编辑器元素层新增**右键**监听：右键落点在正文内 → 打开菜单（debug selector
  `editor-context-menu`），面板包含分组标签「插入」和菜单项「表格」
  （selector `editor-context-insert-table`）。
  - **右键只决定菜单位置，不移动正文 caret/选区**；插入位置始终是打开菜单时编辑器
    里的 caret/选区（没有实现“右键落 caret”）；
  - 菜单面板按编辑器视口夹取；命中矩形写进 `context_menu_rect`，元素层把它并入
    `point_in_toolbar`，因此点菜单不会挪正文 caret、也不会开始拖选；
  - `Esc` 关闭菜单（其它按键先关菜单再按正常按键处理，不吞输入）；点击菜单外关闭
    菜单并照常处理这次左键；单元格就地编辑期间不开菜单（镜像领先于会话）；
  - **同一次手势**：菜单项是元素级 `on_mouse_down`（先于窗口级监听执行），插入后
    保留一帧 `context_menu_rect` 让窗口级左键监听继续放行这次点击，避免“插入完成
    又被同一次点击挪 caret”（headless 钉住）。
- 菜单项点击 → `EditorController::insert_table`：一次 `EditRequest`，caret 在表格之后；
  随后清空图片/表格点选态并保证 caret 可见。
- 已知限制（首版）：菜单只有「表格」一项（「插入」是分组标签，不是可展开子菜单）；
  右键不落 caret；滚动/切换笔记时菜单不主动跟随（下一次点击/Esc 关闭）；
  在代码块等不投影成表格的上下文里插入只会得到合法源码文本。

### 14.4 验证

- `crates/editor/src/model/table_edit.rs` 模块内单测新增 6 个：默认表格形状/2 列 ×
  1 数据行、空文档插入、段落末尾/行首/已有空行/行中间/选区替换的空行补法、CRLF、
  非法偏移（非 boundary / 越界 / 倒序）。
- `crates/editor/tests/table_edit.rs` 新增 4 个：caret 处插入是一条纯插入、逐字节结果、
  Object/占位符/caret 在表格之后；空文档插入落在第 0 行；替换选区；单元格编辑期间被拒。
- `crates/app/tests/headless_context_menu.rs`（3 个真实 `AppShell` 用例）：
  右键菜单出现且 caret 不动、Esc/点击菜单外关闭；点「表格」→ 默认表格逐字节插入、
  正文其它字节不变、表格 Object/占位符/对象矩形、caret 在表格之后、一次撤销 + 重做；
  空文档插入后表格在第 0 行且仍是 Object。
- 命令与计数（本机 Linux）：见 §13.4 的同一组命令；本切片后
  `cargo test -p menghunote-editor` **240**（+10：模型单测 6 + 集成 4）、
  `cargo test -p menghunote-app --features dev-sync` **462**（+3）、
  `cargo test -p menghunote-app` **231**（+3），0 failed。
- 未做真机像素验收：菜单观感/阴影/DPI、真实鼠标右键在 Windows 上的行为仍需
  `ui_smoke.ps1`；本环境只证明“右键 → 菜单 → 一次 `EditRequest` → 源码/undo/Object”。

## 15. M1 实施记录（2026-11-17，Mermaid 渲染底座）

> 范围：**依赖 pin + 纯 SVG 渲染封装 + 资源上限/panic 兜底/SVG 形状校验 + 单测 + 文档**。
> 明确**未做**：编辑器里的 fenced `mermaid` 识别、Object Block 投影、源码切换/工具条、
> 缓存、暗色主题、导出、子进程隔离（那是 M2/M3；见 [`docs/notes/editor-backlog-links-mermaid.md`](../notes/editor-backlog-links-mermaid.md)）。
> 本切片没有改任何 editor 现有文件（`view.rs`/`element.rs`/`geometry.rs`/`controller.rs`/
> `markdown.rs`），只新增 `crates/app/src/mermaid.rs` + 模块声明 + 依赖 + 文档。

### 15.1 依赖与特性（精确 pin）

- `crates/app/Cargo.toml`：`merman = "=0.10.0"`，`default-features = false`，
  `features = ["all-diagrams", "complete-svg"]`。
- `all-diagrams`：解析并渲染 Mermaid 12.3 的全部内置图族；M1 不做按图族裁剪。
- `complete-svg` = `svg` + `layout-cytoscape`；**不启用**默认特性里的 `layout-elk`
  （ELK 是 EPL-2.0 组件、体积也大）。显式要求 ELK 布局的图不保证渲染结果
  （本切片只保证 Cytoscape/内置布局的图）。
- 无 Node / Chromium / WebView / 远程渲染；`SvgEnvironment::deterministic()` 固定文本
  度量（不读系统字体），同一输入在同一 crate 版本上输出字节级一致。
- 实际编译：该 feature 组合**一次通过**，无需再收窄；`Cargo.lock` 新增 **42** 个包
  （merman / merman-core/model/render/theme-contract/markup、dugong、cssparser、
  quick-xml 等；`phf_codegen` 有两个版本，所以 `+name` 记录是 43 条）。merman 的
  `analysis`/`ascii`/`editor` 包出现在 lock 里是 optional 依赖记录，**不在实际编译图**
  （`cargo tree -i` 为空）；ELK 根本没进 lock。
- 许可证：merman 及其渲染后端 MIT/Apache-2.0；未引入 EPL 的 ELK。传递依赖里
  `cssparser`/`dtoa-short` 是 MPL-2.0（文件级 copyleft，不是 GPL/AGPL），其余为
  MIT/Apache-2.0/BSD 系；无 GPL/AGPL。

> 注：`crates/app/Cargo.toml` 在基线 blob 里是 CRLF（`git ls-files --eol` =
> `i/crlf`，与 `.gitattributes` 的 `eol=lf` 不一致）。按 LF 写回后 `git diff` 会把
> 整个文件显示成改写，**语义改动只有新增 `merman` 的 12 行**；
> `git diff --ignore-cr-at-eol` 可核对。

### 15.2 API（`menghunote-app::mermaid`）

- `pub fn render_svg(source: &str) -> Result<String, MermaidError>`：同步纯函数；
  每次调用新建 `Renderer`，无跨调用全局状态，可在当前线程或后台线程调用（M1 不替
  调用方选线程）。
- `MAX_MERMAID_SOURCE_BYTES = 256 KiB`、`MAX_MERMAID_SVG_BYTES = 8 MiB`。
- `MermaidError::{Empty, TooLarge { what: MermaidLimit, actual, limit }, Render(String),
  Panic(String), InvalidSvg(String)}`；实现 `Display` + `std::error::Error`。
  merman 0.10 把“没有图类型”也报成解析错误（`No Mermaid diagram type detected`），
  因此统一落在 `Render`，不额外造一个实际不可达的 `NoDiagram` 变体。
- `MermaidLimit::{Source, Svg}`：区分超的是哪一侧预算（错误文案用）。
- 成功只返回 SVG 字符串，**不做**任何 HTML/SVG 内容改写或执行。

### 15.3 资源上限与失败语义

| 边界 | 上限 | 实现 |
| --- | --- | --- |
| 源码 | 256 KiB（UTF-8 字节） | `render_svg` 入口先查，超限直接 `TooLarge`，不进渲染器 |
| 输出 SVG | 8 MiB | merman 资源策略 `max_svg_bytes` 在序列化阶段拒绝 + 返回后复核 |

- 空/纯空白源码 → `Empty`；不是图/语法/布局/输出错误 → `Render(msg)`（文案来自 merman）；
  渲染器 panic → `Panic(msg)`；去掉 BOM/前导空白后不是以 `<svg` 根元素开头或不含
  `</svg>` → `InvalidSvg(prefix)`（错误文案只带前 32 个字符，不倾倒整段输出）。
- 解析/布局/序列化整体包在 `catch_unwind` 里；**不安装全局 panic hook**，不把 panic
  文案拼进 SVG。失败**不 panic、不返回半成品**。
- 回退语义（写给 M2）：任何 `MermaidError` 都显示原始 fenced code + 失败状态，
  **不写回正文**、不阻塞编辑；源码仍是唯一真源。
- M1 不做超时/子进程隔离：最坏情况由源码 256 KiB、输出 8 MiB 与 merman 自带的
  布局工作预算共同限制；独立 helper 子进程 + 超时留到 M3。

### 15.4 验证

- `crates/app/src/mermaid.rs` 模块内单测 **11** 个：简单 flowchart/sequenceDiagram
  成功且含 `<svg>`；空/纯空白拒绝；非图源码拒绝；截断语法（`A[未闭合`、缺 `end` 的
  `loop`）拒绝不 panic；源码超限（上限 +1 字节）在渲染前拒绝；SVG 输出超限复核；
  SVG 形状校验（BOM/前导空白/`<svgfoo>`/未闭合/非 SVG）；重复渲染字节级一致且中间
  渲染别的图不污染；跨线程渲染一致；`catch_unwind` 把字符串/非字符串 panic 载荷
  都转成 `Panic`。
- 文档示例写成 `text` 块而非 doctest：本机 app crate 的 doctest 链接缺
  `zed_font_kit` rlib（环境限制，与本切片无关），同一路径由
  `flowchart_renders_svg` 单测覆盖。
- 命令与数字（本机 Linux，`CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0`；
  app 测试链接需要 `LIBRARY_PATH`/`LD_LIBRARY_PATH` 指向本机 xkbcommon 开发库的
  本地 shim `/tmp/xkblib`，这是环境限制、与代码无关）：
  - `cargo test -p menghunote-app --lib mermaid` = **11 passed / 0 failed**；
  - `cargo test -p menghunote-app` = **273 passed / 0 failed**（35 个 suite；交接快照，
    计数含本切片以外的并行功能，本切片只占其中 11 个）；
  - `cargo test -p menghunote-app --features dev-sync` = **504 passed / 0 failed**
    （35 个 suite，lib 单测 271）；
  - `cargo fmt --all -- --check` = 通过；
  - `cargo clippy -p menghunote-app --all-targets --features dev-sync -- -D warnings`：
    本切片新增/修改的文件没有告警（全仓 clippy 由 Lead 集成时复跑）。

## 16. S3b5 实施记录（2026-11-17，表格拖拽重排行 / 列）

> 范围：**表格 Object 的行 / 列拖拽重排**（把手、拖拽状态机、插入指示线、源码重排、
> 一次撤销、选中格跟随）。表格源码仍是唯一真源。
> 明确**未做**：合并单元格、嵌套表格、表格横向滚动、列宽调整、复制单元格、拖拽自动滚动、
> 表头行参与行重排；不改 schema / 同步 wire / tag。

### 16.1 交互模型（写死，headless 钉住）

- 表格**悬浮或选中**时，在边框上浮出小把手（`div` 浮层，压在 1px 边框上）：
  - **列**：表头**上边框**上每列一个横向胶囊（debug selector `table-drag-column-{n}`），
    水平拖动；表头被滚出视口时不显示（把手锚在表头顶边）；
  - **行**：每条**数据行**左边框上一个纵向胶囊（`table-drag-row-{n}`，`n` = 数据行下标、
    0 起），垂直拖动；**表头固定，不参与行重排**，因此没有表头行把手；
  - 把手一半在表格外、一半在 padding/边框里，**不遮挡单元格文字**（行把手横向只压进
    单元格 < `TABLE_CELL_PAD_X`；列把手纵向只压进表头 3px），并列数/数据行数 < 2 时
    不显示（没有可换的位置）；
- **命中优先级**：把手是浮层 div，元素级 `on_mouse_down` 先于窗口级正文监听执行；
  同时其命中矩形写进 `table_handle_rects`（并入 `point_in_toolbar`），因此按下把手
  **绝不**会选中单元格 / 整表选中 / 落 caret；
- **拖拽**：按下把手进入拖拽（`TableDragState { line, axis, from, slot, target, position }`），
  指针位置换算成插入**槽位**（列：`0..=columns`；行：`0..=row_count`，按目标列/行的
  左/右、上/下半格判定），有效槽位画 2px 插入指示线（debug selector
  `table-drop-indicator`）；槽位与指针位置变化时重绘，**正文 caret/选区一动不动**；
- **释放**：槽位换算成最终下标（`menghunote_editor::drop_target`，原地/越界 → 不变）→
  **一次** `EditRequest`（一个撤销组）；**`Esc` / 拖出表格**（指针离开表格超过
  `TABLE_DRAG_CANCEL_MARGIN`，任意方向）→ 取消，**不产生任何请求**；拖拽中按其它键也先
  取消（拖拽不接受键盘改目标）。

### 16.2 源码重排（`menghunote-editor::model::table_edit`）

新增 `TableReplacement { span, text }`（**等长**整段替换）与三个纯函数：

- `drop_target(from, slot, count)`：拖放槽位 → 最终下标；落在自身两侧 → `None`
  （不产生编辑）。UI 与控制器共用同一口径；
- `move_row_replacements(source, table, from, to)`：一条替换，区间 = 第一条数据行内容
  起点 .. 最后一条数据行内容终点；行内容按 `to` 重排，**槽位之间的行尾换行留在原位**
  → LF 恒 LF、CRLF 恒 CRLF，表格在文档末尾且末行没有换行时也不会凭空补换行；
  表头 / 分隔行 / 表格之外的正文逐字节不变，表格总字节数不变；
- `move_column_replacements(source, table, from, to)`：表头 / 分隔行 / 每条数据行**各一条**
  替换（按行顺序、互不重叠）；cell 文本按 `to` 重排，**槽位之间的 `|` 与空白留在原位**
  → 分隔行的对齐标记（`:-:` 等）跟着列走，其它列对齐不变；行首/行尾 `|` 与多余 cell
  （GFM 本来忽略）逐字节保留；缺列的行（GFM 把空列补在行内容末尾）**整次放弃**
  （与删列同样的保守口径：不猜位置）；
- 内容完全相同的列/行、原地、越界、少于 2 项 → 空 `Vec`（**不产生任何编辑**）。

### 16.3 控制器（`EditorController::move_table_row` / `move_table_column`）

- 只在行号上是**整块表格**（Object + Table）且参数合法时生效，否则 `Ok(None)`；
  一次拖拽释放产生**一个** `EditRequest`（行重排 1 条、列重排 `rows + 2` 条等长
  `ScalarEdit`，整批同一个撤销组）→ 一次 `Ctrl+Z` 回到原顺序；
- **caret/选区不变**：重排是等长置换、表格区间不变，表格之外的坐标天然有效；只有落在
  表格源码**内部**的端点被夹到表格末尾（对象内部没有文本落点，与插入/删除同一契约）；
- 不切换源码模式（表格继续投影成一个 Object 占位符）；单元格编辑期间与其它结构动作
  同一闸门（`CellEditActive`）。

### 16.4 app（`crates/app/src/editor/{view,geometry,element,theme}.rs`）

- 几何（`geometry.rs`，**绘制/命中/headless 断言同源**）：`TablePaint` 增加
  `row_count`、`column_width`；把手绘制/命中矩形、按指针位置算出的列/行插入槽位、
  插入指示线矩形全部由它给（命中矩形按 `TABLE_DRAG_HIT_PAD` 外扩，但**不越过对象行
  上边 / 相邻数据行**，避免悬停抖动或抢相邻行的点击）；
- 视图（`view.rs`）：`TableDragState` + `begin_table_drag` / `update_table_drag` /
  `finish_table_drag` / `cancel_table_drag`；`table_drag()`、`table_drag_handles()`、
  `table_drop_indicator()` 是可测状态；持闸 / IME 组合 / 单元格编辑 / 表格源码模式
  **不进入拖拽**；
- 元素（`element.rs`）：鼠标按下先放行把手（`point_in_toolbar` 并入把手命中矩形 +
  “拖拽中”短路），拖拽期间鼠标移动只更新槽位（不拖选正文、不改悬停目标），
  释放按位置完成重排后再结束文本拖选；`Esc` 在 `on_key_down` 最前面取消拖拽；
- 选中格修正（纯函数在 `table_cell_edit`）：`cell_after_row_move` /
  `cell_after_column_move` —— **选中的内容跟着走**（拖动项 → 目标下标；被让位的项
  整体挪一格），表头行不参与行重排（原样留在表头）。

### 16.5 验证

- `crates/editor/src/model/table_edit.rs` 模块内新增 **12** 个：2×2 行重排逐字节、多行多列
  首/中/末行、表头与分隔行不动、含对齐列重排（对齐跟列走）、CRLF 行/列、文档末尾无换行、
  行/列 no-op（原地/越界/单项）、缺列行拒绝、多出的 cell 不参与、无数据行的表、`drop_target`；
- `crates/editor/src/model/table_cell_edit.rs` 模块内新增 **2** 个：重排后的选中格修正
  （内容跟随、让位、表头、越界）；
- `crates/editor/tests/table_edit.rs` 新增 **10** 个：单条/多条请求形状（行 1 条、列
  `rows + 2` 条、全等长、升序不重叠）、逐字节首/中/末行与首/末列、CRLF、文档末尾、
  caret 在表格前后都不动、非表格/越界/原地 no-op、单元格编辑期间被拒、连续两次移动
  是两个撤销组；
- `crates/app/tests/headless_table_drag.rs`（8 个真实 `AppShell` 用例）：把手只在悬浮时
  出现且表头没有行把手、把手压在边框上不遮挡文字、不抢单元格点击；按把手不落 caret/
  不选格、原位释放零请求；首列拖到末列（逐字节 + 仍 Object + 指示线 + 一次撤销/重做）；
  首行拖到末行（表头固定 + 逐字节 + 一次撤销 + 末行拖回最前）；`Esc`/拖出表格取消
  （零请求）；选中格跟着内容走（列/行各一次）；单元格编辑期间没有把手且入口被拒；
  重开 vault 后重排结果存在、反向拖回逐字节恢复；
- 命令与计数（本机 Linux，环境变量见 AGENTS §6）：见 §16.6。

### 16.6 计数（本机 Linux，S3b5 完成时）

- `cargo fmt --all -- --check` = 通过；
- `cargo clippy -p menghunote-editor -p menghunote-app --all-targets --features dev-sync
  -- -D warnings` = 通过（0 warning）；
- `cargo test -p menghunote-editor` = **264 passed / 0 failed**（+24：模型单测 12 +
  选中格修正 2 + 集成 10；含本切片与基线其余用例）；
- `cargo test -p menghunote-app --features dev-sync` = **504 passed / 0 failed**（+8：
  `crates/app/tests/headless_table_drag.rs`）；
- `cargo test -p menghunote-app` = **273 passed / 0 failed**（+8，同上）；
- 计数包含同仓并行切片（Mermaid/图谱等）的用例；本切片自身的增量是 editor +24、
  app +8（两个 feature 组合各 +8）。
- 未做真机像素验收：把手大小/颜色/抓取光标观感、DPI、真实窗口的按下-拖动-释放仍需
  Windows `ui_smoke.ps1`；本环境只证明“几何 → 按下/拖动/释放 → 一次 `EditRequest` →
  源码/undo/Object/持久化”。

### 16.7 已知限制（首版写死）

- 长表格滚动时**不自动滚动**：只能拖到当前可见的数据行位置；
- 表头被滚出视口时不显示列把手（把手锚在表头顶边）；
- 表格在单元格就地编辑 / 源码模式 / 同步持闸 / IME 组合期间不显示把手，也不能开始拖拽；
- 缺列的行（GFM 隐式补空列在行尾）不能做列重排（整次放弃，不猜列边界）；
- 行重排把行尾换行视为**槽位**属性（末行没有换行时不会凭空补），因此极端混用行尾的
  文档里换行字节留在原槽位（表格语义不变，但换行样式不跟着行走）。

## 17. S3b6 实施记录（2026-11-17，表格整行/整列选中与拖拽 ghost）

> 范围：**表格行列拖拽的选中/拖动反馈**（Obsidian 式：点击把手选中整行/整列、
> 低透明度高亮 + 把手强调色、跟随指针的整行/整列 ghost + “移动第 N 列/行”文案）。
> 只加视觉与选择状态，**不改**重排算法、源码写入、undo 语义、schema / 同步 wire / tag。
> 明确**未做**：直接点表格内容选中整行/整列、多行/多列范围选择、拖拽自动滚动、
> 列宽调整、合并单元格。

### 17.1 交互与选中状态机（写死，headless 钉住）

- **按下把手 = 选中**：列把手 → 选中整列；行把手 → 选中整行。按下同时进入 S3b5 的
  拖拽状态（保持原状态机不变），但**选中是独立状态**（`TableAxisSelection`），
  不随一次手势的结束而消失：
  - 单击（按下 → 原位释放）：选中保留、**零请求**（`finish_table_drag` 的 no-op 分支）；
  - 拖拽释放（有效槽位）：选中**移动到新下标**（与插入指示线同一个 `target`）；
  - 拖出表格释放（取消重排）：原表不变，选中留在原下标（用户没移动成功，仍选着它）；
  - `Esc`：取消拖拽并清除轴选中；不在拖拽时 `Esc` 也清除轴选中；
  - 点表格外 / 移动正文 caret（`commit_selection`）：清除轴选中；
  - 点单元格：**单元格选中优先**，轴选中退出，随后进入该格原地编辑；
  - 进入表格源码模式：清除轴选中（源码模式下没有“整行/整列”的绘制几何）。
- **互斥**：同一时间只有一个轴选中；选中与拖拽不产生任何 `EditRequest`，
  一次释放仍然只有一条 `EditRequest` / 一个撤销组（沿用 S3b5）。
- **快捷感知**：被选中的行/列，其把手换成主题强调色（普通把手仍是灰色）；
  拖动中正文指针从 I-beam 换成“抓紧”（`ClosedHand`），不改变任何命中/caret 行为。

### 17.2 绘制与几何（`crates/app/src/editor/{geometry,view,theme}.rs`）

- 几何仍是**一份**：`TablePaint::axis_rect(axis, index)` 给出整列（表头 + 全部数据行）
  或整条数据行（不含表头）的窗口矩形；选中高亮与 ghost 都从它算，不另算一套坐标。
  整列/整行高亮矩形因此与 `table_cell_rects()` 的并集逐像素一致（headless 断言）。
- **选中高亮**：`table-axis-selection` 浮层 div —— 低透明度填充（约 12%）+ 2px 强调色
  边框，画在表格之上、单元格输入框/把手之下；因此文字透出、不被单元格描边盖住
  （点单元格会先清轴选中，两者不叠加）。表头行不参与行选中（与行把手同一寻址）。
- **ghost**：`table-drag-ghost` 浮层 div = 被拖项矩形按 `position - press` 平移，
  半透明填充（约 20%）+ 1px 强调色边框；旁边 `table-drag-ghost-label` 深色文案
  “移动第 N 列/行 → 第 M 列/行”（没有有效落点时只写“移动第 N 列/行”）。
  - 指针离开按下点超过 `TABLE_DRAG_GHOST_MIN_MOVE`（3px）后才出现 → 单击把手
    只看选中、不闪 ghost；`moved` 一旦为真保持到手势结束；
  - 拖出表格（`slot == None`）→ ghost/文案与插入指示线一起消失；
  - 两个 div **都没有鼠标监听 / hitbox**：不拦截鼠标、不改 caret；矩形裁剪到编辑器
    可见区，不越出编辑区压到宿主 UI；
  - ghost 与插入指示线/文案来自**同一个** `TableDragState`（`target` / `slot`），
    落点高亮与 ghost 永远同步。
- **可测状态**：`table_axis_selection()`、`table_axis_selection_rect()`、
  `table_drag_ghost_rect()`、`table_drag_ghost_label()`；debug selector
  `table-axis-selection` / `table-drag-ghost` / `table-drag-ghost-label`。
  `TableDragState` 增加 `press`（ghost 锚点）与 `moved`（ghost 可见阈值）。

### 17.3 验证

- `crates/app/tests/headless_table_drag.rs`：原 **8** 个用例全部保留并通过，新增 **5** 个
  （共 **13** 个真实 `AppShell` 用例）：
  1. `clicking_handles_selects_whole_column_and_row`：点列/行把手选中整列/整行，
     高亮矩形 = 对应 cell 并集（含表头/不含表头边界）、debug selector 与 accessor
     同源、相邻列/行不被盖住、点其它把手移动选中、全程零请求且 caret 不动；
  2. `dragging_shows_ghost_and_moves_selection_to_drop_target`：按下只选中不闪 ghost →
     移动后 ghost + 文案 + 指示线同时出现（尺寸 = 列宽 × 表格高、跟随指针、文案带
     起止序号）→ 释放后逐字节重排、选中到新下标、ghost/指示线消失、一次撤销/重做；
  3. `dragging_rows_shows_vertical_ghost_and_moves_selection`：行 ghost 尺寸（表格宽 ×
     行高）、纵向跟手、文案“第 1 行 → 第 3 行”、释放后选中到末行、一次撤销；
  4. `escape_click_outside_and_cell_click_clear_axis_selection_without_requests`：
     `Esc` / 点表格外正文 / 点单元格三种清除路径，全部零请求；单元格点击优先；
  5. `dragging_out_of_the_table_hides_ghost_and_cancels_without_requests`：拖出表格 →
     ghost/文案/指示线消失、原表逐字节不变、选中留在原下标；随后 `Esc` 清除选中。
- 命令与计数（本机 Linux，环境变量见 AGENTS §6）：见 §17.4。

### 17.4 计数（本机 Linux，S3b6 完成时）

- `cargo fmt --all -- --check` = 通过；
- `cargo clippy -p menghunote-editor -p menghunote-app --all-targets --features dev-sync
  -- -D warnings` = 通过（0 warning）；
- `cargo test -p menghunote-app --features dev-sync` = **510 passed / 0 failed**
  （`headless_table_drag` +5：8 → 13）；
- `cargo test -p menghunote-app` = **279 passed / 0 failed**（同上 +5）；
- 未做真机像素验收：高亮/ghost 的透明度与观感、把手强调色、抓取光标、DPI、真实窗口
  的按下-拖动-释放仍需 Windows `ui_smoke.ps1` 人工确认；本环境只证明
  “几何 → 按下/选中/拖动/释放 → 选中/ghost → 一次 `EditRequest` → 源码/undo/Object”。

### 17.5 已知限制（本切片写死）

- 只能通过**把手**选中整行/整列；直接点单元格仍是单元格选中（单元格优先）；
- 长表格滚动时**不自动滚动**（沿用 S3b5）：只能拖到当前可见的数据行位置，整列 ghost
  会被裁剪到编辑器可见区；
- 表头固定、不参与行选中/行重排（与行把手同一口径）；
- 一次只有一个轴选中；不提供多行/多列范围选择；
- 表格在单元格就地编辑 / 源码模式 / 同步持闸 / IME 组合期间不显示把手，也不能开始拖拽；
- 点**编辑器以外**（侧栏 / 状态栏 / 设置页）不会立刻清除轴选中：编辑器收不到那次点击，
  回到编辑器后任意点击（正文/单元格/表格）会按上面的清除路径处理；
- 轴选中不参与 Ctrl+C 复制（复制语义仍是图片对象/正文选区，未扩展到整行/整列）。

## 18. M2 实施记录（2026-11-17，fenced Mermaid Object 预览）

> 范围：` ```mermaid ` fenced code → 编辑器 Object Block（后台渲染/光栅化/缓存/源码切换/
> 工具条）。**不做**暗色主题、CJK 字体策略、导出、子进程隔离（M3）；不改
> storage/sync/relay/schema/wire/tag；不改表格单元格/拖拽/shell。
> M1 的 `render_svg` 语义不变，只**新增**预览入口 `render_svg_for_preview`。

### 18.1 解析（`menghunote-markdown`）

- 新增 `FencedMermaidBlock`：整块 span（开围栏行首 .. 闭围栏行内容终点，不含末尾换行）、
  content span（围栏之间原始源码）、opening span（围栏标记，跳过行首缩进，含 info）、
  first/last line、info、`block`。
- 只识别 **去掉首尾空白后精确等于 `mermaid`** 的 info（小写；`Mermaid`/`mermaid title=…`
  按普通代码块处理）。未闭合围栏不产生该元数据（沿用 `fenced_construct` 的闭合判定）。
- `block = true` 的条件：闭合围栏 + 开/闭行都是 `LineKind::FencedCode` + 非引用/非列表 +
  开围栏从行内容起点开始（无行首缩进）+ 结束在闭围栏行内容终点。引用里的围栏沿用
  M1 前的既有边界（不产生 mermaid 元数据，仍是普通代码）。
- 可投影块**移除** `ConstructKind::FencedCode`（否则围栏隐藏会与 Object 段重叠）；
  其它语言/非 Object 的 mermaid（引用/列表/缩进）构造与行为完全不变。
  `code_spans` 仍包含 mermaid 块（内部行内构造不投影）。

### 18.2 编辑器（`menghunote-editor`）

- `ObjectKind::Mermaid` + `MarkdownMermaid`（span/content/line/end_line/info/block）。
  图片/表格/Mermaid 共用 **同一条** `ObjectBlock` 路径：投影占位符、整块选中、
  `begin/end_object_source_edit`、删除完整 span、复制完整源码、端点偏置都不另写一套。
- `collect_objects(images, tables, mermaid)` 按 span 排序；`object_mermaid` 取载荷。
- 源码模式显示**完整 fenced 源码**（` ```mermaid ` + 内容 + ` ``` `）；单击/选区相交不展开。

### 18.3 app：SVG → GPUI 图像（关键取舍）

- **不新增依赖**：`gpui-pre 0.3.12` 已带 `gpui::SvgRenderer`（usvg/resvg 0.46，与
  gpui-kit 同一依赖图/版本）。不引 resvg/usvg 第二份、不开 Node/WebView。
- Mermaid 默认标签是 `<foreignObject>` HTML，`usvg` 会忽略 → 直接光栅化会“没有文字”。
  因此 M1 新增 `render_svg_for_preview`：用 merman 的 `SvgPipeline::resvg_safe()` 把
  HTML 标签投影成 SVG `<text>` 并清理 rasterization hazard；仍复用 M1 的输入/输出上限、
  `catch_unwind`、SVG 形状校验。`render_svg`（Parity）语义不变。
- 光栅化：`SvgRenderer::render_single_frame(svg, scale)`（内部固定 2× 平滑，逻辑尺寸
  记自然尺寸 × scale）。自然尺寸取 SVG 根 `viewBox`；无 `viewBox` 时退回数值
  `width`/`height`，百分比宽高按失败占位。小图不放大，大图按内容宽度缩到上限。

### 18.4 后台线程、缓存与资源上限

- 一个后台 worker（串行，并发 1）：`render_svg_for_preview` → `SvgRenderer` 光栅化 →
  写缓存；主线程只 `request/state/take_completed`，`pump` 完成计数触发重绘。
- cache key = 源码 **FNV-1a 64 + 字节长度** + `MERMAN_VERSION`（`0.10.0`）+ 主题 id
  （M2 固定 `light`；M3a 起使用真实 `light`/`dark`）+ 渲染宽度桶（64px，夹在
  128–2048）。同一 key 只渲染一次；失败也缓存为终态（避免每帧重试），工具条
  「重新渲染」可强制丢弃终态重排一次。
- LRU：64 MiB / 最多 64 项（失败项零字节，防病态笔记刷爆表）。
- 光栅预算：单边 ≤ 4096 device px、总像素 ≤ 8 MP（约 32 MiB BGRA）；超限先等比缩小，
  异常 SVG 仍超则 `TooLarge`。失败/空/超限都不 panic、不改正文。
- 失败回退：Object 仍存在，显示「Mermaid 渲染失败」占位；「编辑源码」/F2/Esc 仍走
  与图片/表格相同的源码切换。工具条：Mermaid 只有「编辑源码 + 重新渲染」，**没有放大**。

### 18.5 测试与计数（本机 Linux）

- `crates/markdown/tests/golden.rs` +5：顶层元数据（span/content/opening/行号/info）、
  info 精确匹配、列表/缩进非 Object、引用 mermaid 不产生对象、未闭合围栏。
- `crates/editor/tests/mermaid_object.rs` **9**：Object/投影/端点偏置/复制完整源码/
  选区不展开/源码往返/删除整块/非普通上下文保持代码/三类对象源码顺序。
- `crates/app/src/editor/mermaid_preview.rs` **10**：宽度桶、key（版本/主题/宽度）、
  viewBox/属性解析、真实 GPUI 光栅化 2×、后台 Loading→Ready（Condvar 收敛，无 sleep）、
  失败缓存、renderer panic 兜底、LRU 淘汰、重新渲染。
- `crates/app/src/mermaid.rs` +2：ResvgSafe 无 `foreignObject` 且有 `<text>`；失败分类一致。
- `crates/app/tests/headless_mermaid.rs` **3**：真实 AppShell+vault+真实 merman 渲染，
  成功/失败对象、占位→Ready 几何、工具条（无放大）、F2/工具条/Esc、完整复制、
  帧循环不重复渲染、重新渲染恰好一次、正文逐字节不变；失败回退与列表内 mermaid。
- 命令（`CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0`，环境变量见 AGENTS §6）：
  - `cargo fmt --all -- --check` = 通过；
  - `cargo clippy -p menghunote-markdown -p menghunote-editor -p menghunote-app
    --all-targets --features dev-sync -- -D warnings` = 通过（0 warning）；
  - `cargo test -p menghunote-markdown` = **33 passed / 0 failed**；
  - `cargo test -p menghunote-editor` = **274 passed / 0 failed**；
  - `cargo test -p menghunote-app --features dev-sync` = **525 passed / 0 failed**（36 suites）；
  - `cargo test -p menghunote-app` = **294 passed / 0 failed**（36 suites）。

### 18.6 已知限制 / 风险

- **字体/主题**：光栅化走系统字体（GPUI SvgRenderer 的 fontdb）；Merman 的确定性只保证
  SVG 字符串，**不保证**跨平台像素一致。暗色主题与 CJK 字体回退已在 M3a 完成
  （见 §18.8）；导出/子进程隔离等仍在 M3 后续，字体替换仍需真机看观感。
- **缓存作用域**：`MermaidService` 与 `ImageService` 一样是 **per-view**（打开/切换笔记
  会重建 EditorView，Mermaid 会重新渲染）；同一视图内同源码不重复渲染。跨文档共享缓存
  需另做（不在 M2）。
- **安全**：输入仍有 256 KiB / 8 MiB 上限与 `catch_unwind`；ResvgSafe 会清理动画/外链
  等 rasterization hazard，但**不是**完整沙箱；恶意输入仍可能耗时/OOM。子进程隔离 +
  超时是 M3。
- **真机未验**：高 DPI 下 2× 光栅化的清晰度、字体回退、选中描边与工具条观感、Windows
  实际窗口行为仍需 `ui_smoke.ps1` 人工验收。

### 18.7 M2 收尾（2026-11-17，延迟渲染 + 反引号补全）

> 用户真机反馈后的收尾。**明确不做** SVG 文字拖选/复制文字（GPUI 的 `SvgRenderer`
> 是光栅图，没有 DOM 文本；完整实现需自建矢量文本/命中/选区层，成本高且易错，用户
> 决定先不做）；不引入 WebView。

#### 延迟渲染 + “光标进入即显示源码”（避免输入 ` ```mermaid ` 时立刻变占位）

- **投影规则**（editor）：caret（含两端）或非空选区与 Mermaid block span 相交时，
  该块**不产生 `ProjectionPiece::Object`**，整段 fenced 源码保持 Copy 可见/可编辑；
  此时 `object_rects` 为空、工具条不出现，也不提交渲染请求。移出后下一次投影重新
  产生 Object。图片/表格**不变**（仍是整块选中，不跟随 caret 展开源码）。
- 单击 Mermaid 对象 / 退出源码模式时，app/controller 把 caret 放到块尾换行**之后**
  （块外），因此对象仍能被点选并显示工具条；块在文档尾且无换行时只能落在块尾
  （此时显示源码，属边界情况）。
- **等待态** `MermaidPreviewState::Waiting`（服务本身不会返回）：Object 已投影
  （caret 已移出）但内容为空/纯空白时，显示「等待光标移出后渲染…」占位且不请求；
  空 fenced block 永远等待，不进入 `Failed`。未闭合围栏本来就不产生 Object。
- 移出后绘制路径才 `request`；服务按 key 去重，所以**只请求/渲染一次**。等待态不
  清理缓存：已有 `Ready` 移出后立即复用，不产生第二次请求。
- 工具条「重新渲染」是**显式用户动作**，不受自动延迟门控；自动绘制仍等光标移出。
- 测试：`headless_mermaid` 2 个更新为“caret 内显示源码/无 Object/0 请求 → 移出后
  出现 Object/请求 1/Ready、缓存复用；空内容/未闭合不 Failed”。editor
  `mermaid_object` 增加 selection/caret 端点投影断言。

#### 反引号补全（纯函数 + 平台输入接线）

- 纯规则在 `crates/editor/src/model/backtick.rs`（`backtick_action`），app 只按行语义
  过滤并执行：表格 / 围栏代码 / 缩进代码 / HTML 行**不自动补全**；IME 组合中不触发
  （组合上屏走 `commit_composition`）；非空选区走普通替换。
- 规则（写死，单测钉住）：
  1. 普通位置输入 `` ` `` → 插入成对 `` `` ``，caret 在中间；
  2. caret 在成对反引号中间（含行内代码闭合位置）再输入 `` ` `` → 跳过自动补的那个，
     caret 右移，**零 `EditRequest`**；
  3. 行首连续反引号：已有 1 个时继续单个；第 3 个（行尾只有空白）补全
     ```` ``` ```` + 空行 + 闭围栏，caret 落在**开围栏之后**（可直接输入 `mermaid`
     info string）；一次按键一个 `EditRequest`/撤销组；
  4. 行首已有正文/表格时只继续单个反引号，不吞已有文本；已有 3+ 个时不再嵌套围栏。
- 普通行内代码（`` `x` ``）闭合、表格输入、已有文本均不回归；IME 组合测试覆盖。
- 测试：editor 纯函数 **9**；app `headless_backtick` **4**（成对/跳过、行内闭合、
  三反引号围栏 + 一次撤销、IME 组合不触发）。

#### 计数（本机 Linux）

- `cargo fmt --all -- --check` = 通过；
- `cargo clippy -p menghunote-markdown -p menghunote-editor -p menghunote-app
  --all-targets --features dev-sync -- -D warnings` = 通过；
- `cargo test -p menghunote-markdown` = **33 passed / 0 failed**；
- `cargo test -p menghunote-editor` = **283 passed / 0 failed**（+9 backtick）；
- `cargo test -p menghunote-app --features dev-sync` = **531 passed / 0 failed**（+6 headless）；
- `cargo test -p menghunote-app` = **300 passed / 0 failed**。

#### 真机注意点

- 打开笔记时光标默认在文首：若文首就是 Mermaid block，会先显示**完整源码**（可编辑），
  **移动光标离开 block 后**才投影成 Object 并渲染；这是预期行为。
- 空 fenced block 在 caret 移出后显示等待态（不会自动变成失败）；需要内容后移出光标。
- Mermaid 的“单击选中对象”与“caret 在块内显示源码”以 caret 位置区分：单击对象会把
  caret 落到块尾换行之后；文档尾无换行的块是边界情况。
- 反引号补全靠平台文本输入路径（Windows 实际按键/IME 仍需真机确认）；表格单元格
  浮层编辑走输入框，不经过自动补全。
- 未做 SVG 文字拖选/复制；工具条仍只有「编辑源码 / 重新渲染」。

### 18.8 M3a 实施记录（2026-11-18，暗色主题 + CJK 字体回退）

> 输入：[`docs/notes/editor-backlog-links-mermaid.md`](../notes/editor-backlog-links-mermaid.md)
> §2「M3」。本切片只做**暗色 SVG 配色 + CJK 字体回退**，并保持 M1/M2 的
> 输入/输出上限、`catch_unwind`、`ResvgSafe` 管线与失败回退语义。
> **不做**：SVG/HTML/PDF 导出、独立子进程 + 超时隔离、WebView、跨文档共享缓存、
> Mermaid 文字选择。

#### 主题 API（`crates/app/src/mermaid.rs`）

- `render_svg_for_preview(source: &str, theme: crate::theme::ThemeId)`：预览入口新签名。
  浅色 → `ThemePreset::EditorLight`，深色 → `ThemePreset::EditorDark`；通过
  `RenderRequest::svg(...).with_theme(compiled)` 绑定到**整条**解析→布局→SVG 管线。
  `render_svg`（Parity SVG，浏览器/未来导出）保持 M1 语义，**不**绑定主题。
- 主题编译走 merman 内置 `DiagramThemeCompiler`：`export_preset(preset)` →
  `with_cjk_font_fallback(recipe)` → `compile_recipe(recipe)`。编译结果按预设
  `OnceLock` 缓存（`DiagramTheme` 内部 `Arc`，clone 只加引用）；编译失败映射为
  `MermaidError::Render`，调用方仍回退源码，不 panic。
- **浅色外观**：M2 的“无主题”输出（Mermaid 默认调色板）→ EditorLight（白底
  `#ffffff`、深色文字 `#0f172a`）。画布/文字对比度有单测（WCAG AA ≥ 4.5，实测
  浅/深色都远高于阈值）；`A-->B` 的 SVG 自然尺寸由 `168×156` 变为 `168×162`
  （预设排版差异），headless 几何断言已同步更新。
- **深色外观**：EditorDark 深色画布 `#0f172a`、浅色文字 `#e5e7eb`；与应用深色
  `editor_bg #0d1117` 有一个很浅的层次差（画布即“图卡片”），不额外绘制背板。

#### 缓存 / worker 接线（`editor/mermaid_preview.rs`）

- `MermaidCacheKey` 继续携带主题 id（ADR 0021 P2 已接），新增
  `theme_id() -> ThemeId` 供 worker 解析；`MermaidService::request` 只按 key 去重。
- `Job::Render` 与 `RenderFn` 均携带 `ThemeId`：
  生产为 `render_svg_for_preview(source, theme)`；测试可注入 fake 并记录主题。
  worker **不读全局主题**，避免“主线程已切主题、任务却是旧 key”的竞态。
- 行为：浅/深色 key 不同 → 各渲染一次；同一主题重复请求/帧循环命中缓存；
  切主题后旧 key 不命中、新 key 渲染一次；再切回旧主题命中缓存（不重复渲染）。

#### CJK 字体回退

- merman 的 `SvgEnvironment::deterministic()` 不读系统字体：**排版**是确定性的；
  **画字**由显示层 GPUI `SvgRenderer`（usvg/fontdb）按 `font-family` 链解析。
- 内置预设字体栈为 `Inter, ui-sans-serif, system-ui, sans-serif`；编译前把
  `Microsoft YaHei`、`Noto Sans CJK SC` 插到通用族 `sans-serif` 之前（幂等）：
  `Inter, ui-sans-serif, system-ui, "Microsoft YaHei", "Noto Sans CJK SC", sans-serif`。
  只改 SVG 字体栈声明，**不携带字体文件、不新增依赖**；缺失字体仍由 usvg 的
  per-char fallback 兜底。
- 真实中文标签（`flowchart TD\n  开始[开始] --> 结束[结束]`）：
  - ResvgSafe 后是两个 SVG `<text>`（无 `foreignObject`），浅/深色均可渲染；
  - 单元测试用真实 `SvgRenderer` 光栅化，对照“空标签”为 0 个文字色像素，
    中文标签出现 ≥100 个 `#e5e7eb` 文字墨迹像素（本机 Linux = 772）；
  - headless 深色 AppShell 下从占位（Loading）收敛为 Ready，中文标签不失败。
- **Windows 真机仍需人工确认**字体回退观感（是否出现方框/字重/度量差异）；
  headless 只能证明“选中了能覆盖 CJK 的系统字体并画出了墨迹”。

#### 测试与计数（本机 Linux，`CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0`）

- `crates/app/src/mermaid.rs` **16**（+3）：浅/深色调色板不同 + 文字/画布对比度；
  字体栈包含四个回退族且通用族在最后；中文标签是 SVG `<text>` 且同主题字节级稳定。
- `crates/app/src/editor/mermaid_preview.rs` **12**（+2）：浅/深色 key 不同、
  worker 各渲染一次并记录主题、同主题命中缓存、新宽度桶才再渲染；真实光栅化
  CJK 墨迹。
- `crates/app/tests/headless_mermaid.rs` **5**（M2 原 5，几何期望更新为 168×162）。
- `crates/app/tests/headless_mermaid_theme.rs` **1**（新，单独 binary）：真实
  AppShell 浅色 Ready → 切深色占位 Loading → 深色 CJK Ready（只多渲染一次）→
  切回浅/深色命中缓存；正文/选区不变；落库正文逐字节不变。
- 命令与结果：
  - `cargo fmt --all -- --check` = 通过；
  - `cargo clippy -p menghunote-app --all-targets --features dev-sync -- -D warnings`
    = 通过（0 warning）；
  - `cargo test -p menghunote-app --features dev-sync` = **626 passed / 0 failed**；
  - `cargo test -p menghunote-app` = **380 passed / 0 failed**。

#### 已知限制

- **未做导出**：SVG/HTML/PDF 导出、子进程隔离 + 超时仍是后续切片；当前安全边界
  仍为 256 KiB / 8 MiB + `catch_unwind` + ResvgSafe。
- **缓存仍 per-view**：切换笔记会重建 `EditorView` 并重新渲染；跨文档共享缓存未做。
- **主题范围**：只认内置 `light`/`dark`；自定义主题包（ADR 0021 P5）后置。
- **真机未验**：Windows 的字体回退观感、高 DPI 清晰度、深色画布与编辑器底色
  的层次感仍需 `ui_smoke.ps1` 人工验收。

## 19. 外部链接增强实施记录（2026-11-17，Ctrl/Cmd+单击 / 右键菜单 / scheme 策略 / 内部链接）

> 输入：[`docs/notes/editor-backlog-links-mermaid.md`](../notes/editor-backlog-links-mermaid.md) §1。
> 本切片只做**普通 Markdown 链接**（`[text](dest)`）；wiki 链接、图片、表格、Mermaid
> 的既有行为不变（wiki 仍是单击打开；图片/表格/Mermaid 仍是 Object）。

### 19.1 交互（写死，headless 钉住）

- **普通单击即打开**（Ctrl/Cmd+单击同样打开）。修饰键判定仍保留在
  `editor::link::is_open_link_modifier`，但不再作为打开的必要条件。
- 悬停手形、链接末尾边界（`display == end` 不命中）保持：命中仍走
  `external_link_hit_at_display` 的可见内容区间。
- **右键菜单**：普通链接上右键显示「打开链接 / 复制链接 / 复制 Markdown」；
  其它位置仍是既有的「插入 → 表格」。右键本身**不移动 caret**；动作在菜单项的
  mouse-down 上执行，随后 `swallow_mouse_up` 吞掉同一次手势的 mouse-up，并保留
  `context_menu_rect` 一帧让窗口级左键监听放行（不挪 caret）。
- 确认框是 app 内最小浮层（`editor-link-confirm`）：遮罩 + 面板 +「取消 / 打开」；
  未知自定义 scheme 多一个「记住此 scheme（仅本机）」。模态：遮罩 `occlude` 且各
  按钮 `stop_propagation`，点击不落到正文；`Esc` 关闭。

### 19.2 scheme 策略（`crates/app/src/editor/link.rs`）

| 目标 | 行为 |
| --- | --- |
| `http` / `https` / `mailto` / `zotero` | 直接交给系统；Zotero **原样**传递（不解析/不改写） |
| `file:` / UNC（`\\server\share`、`//server/share`）/ Windows 盘符路径 | 打开前**确认**（每次） |
| 未知自定义 scheme | 首次**确认**，可勾选「记住此 scheme」 |
| `javascript` / `data` / `vbscript` / 含控制字符 | **拒绝**；提示只显示 scheme，不回显路径 |
| 无 scheme（`#heading` / `Note` / `Folder/Note`） | 内部链接（见 19.3） |

- 确认框/错误提示只显示 `scheme[:host]`（`display_target`）：`mailto:` 不显示收件人、
  `file:` 不显示本地路径、`https://user:pass@host/...` 只显示 host；**URL 不写日志**。
- 「记住」落在**本机偏好文件**（`crates/app/src/editor/link_settings.rs`）：
  `MENGHUNOTE_LINK_SCHEMES_PATH` 覆盖；否则平台配置目录
  （Windows `%LOCALAPPDATA%\menghunote\link-schemes.txt`、macOS
  `~/Library/Application Support/menghunote/link-schemes.txt`、Linux
  `$XDG_CONFIG_HOME|~/.config/menghunote/link-schemes.txt`）。只存 scheme 名，
  **不写正文、不同步**；写盘失败退化为进程内记忆（打开流程不失败）。
- **平台打开统一收口** `editor::link::open_external_url`：交给 GPUI 平台层
  （Windows `ShellExecuteW`、macOS `NSWorkspace`、Linux `xdg-open`/portal），
  都是**参数数组**调用、不经 `sh -c`/`cmd /c`；只去掉首尾空白，其余原样传递。

### 19.3 内部链接

- `#heading`：在当前笔记里按标题文本（大小写不敏感、去掉 `#` 与首尾空白）匹配
  `BlockRun::kind == Heading` 的行；命中后**滚动到标题行顶部**并把 caret 放到标题行首；
  找不到只给非阻塞提示（`editor-link-notice`），不改正文。
- 相对笔记路径（`Note`、`Folder/Note`，可带 `#heading`）：复用 wiki 链接的宿主解析
  （`EditorEvent::WikiLinkRequested`），按路径精确 > 标题 > 最短路径打开；解析不到
  只提示、**不自动建笔记**。`Note#heading` 的 heading 目前只参与语法识别（宿主打开
  笔记后不滚动，沿用 wiki 链接的既有语义）。

### 19.4 验证

- 纯逻辑单测（`editor::link` 9 + `editor::link_settings` 2）：scheme 分类、Zotero 原样、
  file/UNC 确认、未知 scheme、危险 scheme、显示脱敏、标题提取、修饰键、偏好持久化。
- `crates/app/tests/headless_links.rs`（12）：普通单击不打开、Ctrl+单击打开且 caret 不动、
  末尾边界、Zotero、file 确认、未知 scheme 记住、危险 scheme 拒绝不回显路径、右键三项
  与 caret 不动、`#heading` 滚动/定位、相对笔记打开、解析不到只提示。
- 计数（本机 Linux）：`cargo test -p menghunote-editor` = **284**；
  `cargo test -p menghunote-app --features dev-sync` = **575**（+12 headless_links）；
  `cargo test -p menghunote-app` = **344**。
- 真机注意点：手形光标/确认框阴影/DPI 仍需真机；macOS 的 Cmd 判定按 `platform`
  修饰键（本机 Linux 只验证了 Ctrl 路径）。

### 19.5 已知限制

- `Note#heading` 打开笔记后不滚动到标题（需要宿主把 heading 传给新视图，本切片未做）；
- 未知 scheme 的「记住」是**全局本机**偏好（不按 vault 区分）；
- 确认框不显示完整 URL（安全取舍：只显示 scheme/host）；
- 危险 scheme 名单目前是 `javascript`/`data`/`vbscript` + 控制字符，未做可配置。

## 20. S3b7 实施记录（2026-11-17，表格单元格行内 Markdown）

> 范围：GFM 表格 cell 的**渲染**（行内 Markdown → 样式 span + 可见链接）与**链接点击**。
> 源码仍是 GFM 文本（`|` 必须转义），编辑浮层仍显示源码原文，提交仍走
> `sanitize_cell_text`。**不改** storage/sync/relay/schema/wire，不新增依赖，
> 不引入第二套表格几何。

### 20.1 支持与不支持（写死，测试钉住）

| 构造 | 语法 | 渲染 |
| --- | --- | --- |
| 粗体 | `**text**` | `StyleKind::Bold` |
| 斜体 | `*text*` / `_text_` | `StyleKind::Emphasis` |
| 删除线 | `~~text~~` | `StyleKind::Strike` |
| 行内代码 | `` `text` `` | `StyleKind::InlineCode`（等宽 + 底色） |
| 普通链接 | `[text](url)` / `<https://…>` | `StyleKind::Link` + 可点击 |

不支持（**整格退回源码原文**，`TableCellInline::is_plain() == true`）：块级元素
（标题/引用/列表/围栏代码/分隔线/HTML 块/嵌套表格）、换行、图片、合并单元格、富文本 cell。
图片按 `![alt](dest)` 源码原样显示（不解码、不投影成对象）；同一格里其它受支持的构造
仍正常渲染。行内 HTML / 脚注引用按源码原样显示。

### 20.2 解析层（`menghunote-markdown::table_inline`）

- 新增 `parse_table_cell_inline(&str) -> TableCellInline`（纯函数，无 IO/无 GPUI）：
  - `display`：渲染用文本（标记隐藏、转义还原、实体解码）。**不是**源码切片，
    因此不保留源码偏移（与整篇 `ParsedMarkdown` 是两套坐标）；
  - `spans`：display 字节区间上的语义段，**有序、不重叠、完整覆盖** `[0, display.len())`，
    无样式处为 `Plain`；
  - `links`：链接的 display 可见区间 + 目标（自动链接/引用式链接已展开）。
- **`\|` 在行内解析之前还原**：GFM 规定 `\|` 是列边界保护，行内代码**内部**也要还原
  （`` `a\|b` `` 渲染成 `a|b`，不是 `a\|b`）。因此解析前先做一次 `\|` → `|` 预处理
  （`unescape_cell_pipes`，按 CommonMark 的奇偶反斜杠规则）；不预处理会让代码里的
  `\|` 原样显示成反斜杠。
- 语义枚举 `TableCellStyle` 是本 crate 自己的类型（与 `TableAlignment` 同一套做法）：
  依赖方向 `editor → markdown`，本 crate 不能反向依赖 editor 的 `StyleKind`。
- 嵌套/优先级与 editor 的 `model::style::runs` **同一套规则**：同一 display 位置取
  display 区间最短的构造，长度相同再按 `InlineCode > Bold > Emphasis > Strike > Link`。
  因此 `**a *b* c**` 在正文与表格 cell 里着色完全一致。
- 防御性：`\n`/`\r` 一律收敛成空格（`shape_line` 对含换行的文本会断言）；未闭合定界符
  是字面文本（不隐藏）。

### 20.3 编辑器（`menghunote-editor`）

- `MarkdownTableRow` 新增 `inline: Vec<TableCellInline>`（与 `cells` 一一对应），在
  `convert_table_row` 里**随 Markdown 文档重建算一次**；新增
  `MarkdownTableRow::cell_inline(column)` 与 `MarkdownTable::cell_inline(row, column)`
  （行寻址含表头，与 `cell` 同一套）。
- 放在这里的理由：cell 行内解析只随**源码**变化，绘制/命中每帧只借用；放在 app 里
  按帧解析会在大表格上产生无谓的重复工作。
- 源码零改写：`cells` 仍是原始源码区间（`\|` 与标记都在），编辑浮层/提交路径不变。

### 20.4 app（`crates/app/src/editor/{view,geometry,element,glyph}.rs`）

- `table_cell_paint` 从「Plain 单段」改为**样式段**：`TableCellInline.spans()` →
  `(字节数, StyleKind)` → `shape_row_truncated_at`（列宽截断/对齐/空 cell 逻辑不变）。
- `glyph::shape_row_truncated_at` 返回 `TruncatedLine { line, visible_len }`：
  `visible_len` 是**可见前缀**的字节长度（未截断 = 全文长度）。链接命中必须按它裁剪——
  省略号之后的内容没画出来，也就不该可点。
- `TableCellPaint.links: Vec<TableCellLinkPaint>`（`rect` + `display` 区间 + `dest`）：
  矩形用截断后 `ShapedLine::x_for_index` 换算，偏移与 `ShapedLine::paint` 的 `TextAlign`
  语义一致（Center/Right 在单元格内宽里平移），因此点击与绘制逐像素一致。
- 命中优先级：`EditorGeometry::table_cell_link_at` 在**单元格命中之前**判定
  （`element.rs` 的 mouse-down）。单击链接只发 `EditorEvent::ExternalLinkRequested`，
  不移动正文 caret、不选中单元格、不进入编辑；scheme 白名单仍是
  `http`/`https`/`mailto`（其它明确拒绝并给提示）。链接末尾边界不算命中（与正文链接一致）。
- 悬停：单元格链接矩形与正文链接共用 `link_hitboxes`（gpui 原生 `PointingHand`）。
- 编辑浮层：`table_cell_display_text` 仍是**源码原文**（只还原 `\|`），提交走
  `sanitize_cell_text`；无改动提交逐字节不变（不会把标记重写/转义）。

### 20.5 验证

- `cargo test -p menghunote-markdown` = **45 passed / 0 failed**（+11 table_inline 单测，
  +1 golden 接缝）；
- `cargo test -p menghunote-editor` = **285 passed / 0 failed**（+1 `cell_inline`）；
- `cargo test -p menghunote-app --features dev-sync` = **538 passed / 0 failed**
  （+7 `headless_table_inline`）；
- `cargo test -p menghunote-app` = **307 passed / 0 failed**；
- 现有表格/任务/图片/Mermaid 测试全绿（结构、拖拽、undo、加删行列、单元格编辑均未回退）。

### 20.6 已知限制（本切片写死）

- 只渲染**单行**：cell 不能跨行，块级元素/图片一律退回源码原文（不做部分渲染之外的
  富文本 cell）；
- 表格 cell 里的链接**只支持单击打开**：不做 Ctrl+单击/右键菜单/复制链接；
- 链接命中矩形按截断后的可见前缀裁剪：被省略号截掉的链接部分不可点（与“没画出来就
  不可点”一致）；
- 样式外观（粗体字重、代码底色、链接下划线）仍需真机验证：headless 的
  `NoopTextSystem` 把所有字体解析成同一个 `FontId`，无法用 run 的字体 id 断言；
- 不做单元格内的 wiki 链接（`[[Note]]` 在 cell 里按普通文本显示）。

## 非目标

不换富文本真源、不自动远程取图、不做 SVG/动画/图像编辑器、不首版做脚注/数学/alerts、
不移动 `v0.4-sync`、不在本 ADR 启用物理资产 GC。
