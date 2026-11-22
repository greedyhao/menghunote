# 编辑器待办：外部链接与 Mermaid

> 2026-11-16 记录。**外部链接增强已完成（2026-11-17）**：普通 `[text](url)` 的
> Ctrl（macOS Cmd）+单击、右键三项（打开链接 / 复制链接 / 复制 Markdown）、
> Zotero 原样传递、`file:`/UNC 确认、未知 scheme 确认并记住（本机偏好）、危险
> scheme 拒绝、内部 `#heading`/相对笔记打开；详见 ADR 0019 §19 与
> `crates/app/tests/headless_links.rs`（12 个 headless）。
> **Mermaid M1 渲染底座已完成（2026-11-17）**：`merman =0.10.0` + 纯
> `render_svg` + 资源上限/panic 兜底/SVG 校验 + 11 个单测。
> **M2（fenced mermaid Object 预览/源码切换/后台缓存/工具条）也已完成（2026-11-17）**：
> `FencedMermaidBlock` + `ObjectKind::Mermaid` + `SvgRenderer` 光栅化 + LRU 缓存。
> **M3a（暗色 SVG + CJK 字体回退）已完成（2026-11-18）**：`render_svg_for_preview`
> 接收 `ThemeId`，浅色 `EditorLight` / 深色 `EditorDark`，worker 按 key 的主题渲染，
> SVG 字体栈补 `Microsoft YaHei` / `Noto Sans CJK SC`；**导出 / 子进程隔离仍待**
> （M3 后续）。详见 ADR 0019 §15/§18（§18.8 = M3a）与本文件 §2。

## 1. 外部链接打开（含 Zotero）

### 现状（2026-11-17 完成）

- 已解析 `[text](url)` 的 destination（`LinkSpan`），app 按可见文本区间命中；
- **普通单击只移动/落 caret；Ctrl（macOS Cmd）+单击才打开**；悬停手形、末尾边界不误触；
- **右键菜单**：普通链接上「打开链接 / 复制链接 / 复制 Markdown」；动作后吞掉同一次
  手势的 mouse-up，不移动 caret；其它位置仍是「插入 → 表格」；
- **scheme 策略**（`crates/app/src/editor/link.rs`）：
  - 允许 `http`、`https`、`mailto`、`zotero`（Zotero 原样传递，不解析/改写）；
  - `file:`/UNC 打开前确认；
  - 未知自定义 scheme 首次确认，可勾选「记住此 scheme」（本机偏好文件，不同步、不写正文）；
  - 拒绝 `javascript:`/`data:`/`vbscript:` 等危险 scheme；错误只显示 scheme/主机，
    不回显敏感路径；URL 不写日志；
- 平台打开统一收口（`editor::link::open_external_url` → GPUI 平台层，参数数组、不经 shell）；
- 内部链接：`#heading` 滚动/定位到标题；相对笔记路径（`Note`、`Folder/Note`）打开对应
  笔记；解析不到给非阻塞提示，不自动建笔记；
- 测试：`crates/app/tests/headless_links.rs`（12）+ 纯逻辑单测（link 9 / link_settings 2）。

### 仍未做（后续切片）

- `Note#heading` 打开笔记后滚动到标题（需要宿主把 heading 传给新视图）；
- 未知 scheme 的记住范围（当前全局本机；未按 vault 区分）；
- 危险 scheme 名单可配置；
- 真机验证（手形光标、确认框阴影、DPI、macOS Cmd 路径）。

### 早期候选方案（历史，2026-11-16）

- 普通单击只移动光标；**Ctrl+单击**（macOS Cmd）打开链接；
- 悬停手型 + 状态栏显示目标；右键「打开链接 / 复制链接 / 复制 Markdown」；
- 平台层统一 `ExternalOpener`：
  - Windows `ShellExecuteW(..., "open", url, ...)`
  - macOS `open <url>`（参数数组，不经 shell）
  - Linux `xdg-open <url>`（参数数组，不经 shell）
- scheme 策略：
  - 默认允许 `http`、`https`、`mailto`、`zotero`；
  - `file:`/UNC 先确认；
  - 未知自定义 scheme 首次确认，可记住；
  - 拒绝 `javascript:`/`data:`/`vbscript:` 等；
- Zotero 原样传递，不解析/改写：
  - `zotero://select/library/items/<KEY>`
  - `zotero://select/groups/<ID>/items/<KEY>`
  - `zotero://open-pdf/library/items/<KEY>?page=N`
- 解析用 S1 的 `pulldown-cmark` Link destination；click 命中用可见文本范围；
- 错误只显示 scheme/主机，不回显敏感路径；URL 不写日志。

### 待定问题（已决）

- Ctrl+单击 vs 设置项单键打开 → 采用 Ctrl/Cmd+单击；
- 未知 scheme 确认的持久化范围（会话/全局）→ 本机偏好文件（全局、不同步）；
- `file:` 是否允许、如何限制目录 → 允许但每次确认（不做目录白名单）；
- 内部链接（`#heading`、相对笔记）另做跳转，不走系统 → 已实现。

## 2. Mermaid 渲染

### M1 状态（2026-11-17，已完成）

- 依赖：`crates/app/Cargo.toml` 增加
  `merman = { version = "=0.10.0", default-features = false, features = ["all-diagrams", "complete-svg"] }`
  （精确 pin；`complete-svg` = `svg` + `layout-cytoscape`，**不启用** EPL 的 `layout-elk`）。
  该组合一次编译通过，无需再收窄；无 Node/WebView/远程渲染。
- 模块：`crates/app/src/mermaid.rs`（新文件，未动 editor 现有 view/element/geometry/
  controller/markdown）。公开 API：
  - `render_svg(source) -> Result<String, MermaidError>`（同步纯函数，可后台线程调用）；
  - `MAX_MERMAID_SOURCE_BYTES` = 256 KiB、`MAX_MERMAID_SVG_BYTES` = 8 MiB；
  - `MermaidError::{Empty, TooLarge, Render, Panic, InvalidSvg}`（merman 把“没有图”
    也报成解析错误，因此统一落在 `Render`）；
  - 源码超限在进渲染器前拒绝；SVG 超限同时写进 merman 的 `max_svg_bytes` 与返回后复核；
  - 解析/渲染包 `catch_unwind`；输出必须以 `<svg` 根元素开头且含 `</svg>`；
  - 确定性：`SvgEnvironment::deterministic()`（不读系统字体）+ 每次新建 `Renderer`，
    同输入字节级一致。
- 测试：模块内 11 个单测（flowchart/sequence 成功、空/非图/截断语法、源码与输出超限、
  SVG 形状校验、重复与跨线程确定性、panic 兜底）；文档示例写成 `text` 块
  （本机 app doctest 链接缺 `zed_font_kit` rlib，是环境限制）。
- **失败回退**：任何错误都由调用方显示原始 fenced code + 失败状态，不写回正文；
  M1 不做超时/子进程隔离（M3）。

### M2 状态（2026-11-17，已完成；编辑器预览）

- `crates/markdown`：`FencedMermaidBlock`（整块/content/opening span、起止行、info、
  普通段落上下文判定）；可投影块不再产生 `FencedCode` 构造，普通 fenced code 不变。
- `crates/editor`：`ObjectKind::Mermaid` + `MarkdownMermaid`，复用 `ObjectBlock` 的
  占位投影/整块选中/源码切换/完整复制/端点偏置/删除。
- `crates/app`：M1 新增 `render_svg_for_preview`（merman `ResvgSafe` 把
  `<foreignObject>` 标签投影成 SVG `<text>`）；`editor/mermaid_preview.rs` 后台线程
  `render_svg_for_preview` → `gpui::SvgRenderer` 2× 光栅化 → LRU（64 MiB/64 项；
  key = 源码 hash + 长度 + merman 版本 + 主题 + 64px 宽度桶）。
- 交互：渲染中/失败显示占位（失败文案「Mermaid 渲染失败」），不阻塞编辑；单击只选中，
  「编辑源码」/F2 显示完整 fenced 源码，Esc/点击块外/Ctrl+Enter 退出；工具条只有
  「编辑源码 + 重新渲染」，没有放大。
- 测试：markdown +5、editor 9、app 单元 10 + M1 preview 2、headless 3；全量计数见
  ADR 0019 §18.5（markdown 33 / editor 274 / app dev-sync 525 / app 默认 294）。
- 收尾（2026-11-17）：**光标进入即显示源码**（caret/选区与 Mermaid block 相交时
  不投影 Object，源码可直接编辑；移出后才投影）+ **延迟渲染**（Object 已投影但内容为空
  时显示「等待光标移出后渲染…」，不请求；缓存不受影响）+ **反引号补全**（成对/跳过/
  行首三反引号围栏，一次撤销）。**SVG 文字拖选/复制文字明确不做**（光栅图无 DOM 文本，
  用户决定先不做）。详见 ADR 0019 §18.7。
- 限制：缓存是 per-view（切换笔记会重新渲染）；暗色/CJK 已在 M3a 完成；
  导出/子进程隔离仍在 M3 后续。

### M3a（已完成，2026-11-18）

- **暗色主题**：`render_svg_for_preview(source, ThemeId)`；浅色 = merman
  `ThemePreset::EditorLight`，深色 = `EditorDark`，经
  `RenderRequest::with_theme` 绑定整条管线；浅/深色绘制路径、对比度、缓存失效
  均有单测/headless 断言。
- **CJK 字体回退**：编译预设前把 `Microsoft YaHei` / `Noto Sans CJK SC` 插入
  `font-family` 栈（通用族保持最后），不引入字体文件/新依赖；真实中文标签在
  headless 深色 AppShell 下占位 → Ready，真实 `SvgRenderer` 光栅化有文字墨迹。
- 限制：Windows 真机字体回退观感仍需人工确认；缓存仍 per-view。

### M3（后续，仍待做）

- SVG/HTML/PDF 导出、缓存失效策略；
- 独立 helper 子进程 + 超时/资源限制（比 `catch_unwind` 更稳的隔离）；
- 黄金图/样本回归（Merman 对齐 Mermaid 12.3 ≠ 逐像素一致）。

### 早期候选与风险评估（历史，2026-11-16）

- 首选 `merman = "=0.10.0"`（已采用）：纯 Rust，无 Node/Chromium/WebView；官方对齐
  Mermaid 12.3，Zed 已采用；Rust 1.95+；MIT/Apache-2.0；
  API 即 `Renderer::new().render(RenderRequest::svg(...))` / `render_svg_data`。
- 备选 `mermaid-rs-renderer = "=0.3.1"`（default-features=false，更轻、23 图），
  成熟度/对齐度不如 Merman；未采用。
- 风险（仍成立）：不可信输入可能 panic/超时/OOM，M1 已做输入/输出上限 +
  `catch_unwind`，M2 用 merman `ResvgSafe` 清理 rasterization hazard，仍保留
  “源码回退”；暗色主题/CJK 已在 M3a 完成，导出/子进程隔离留给 M3 后续。


## 优先级

这两项排在当前 **S1 解析/列表 → S2 粘贴/图片 → S3 表格/任务/GC** 之后；
外部链接优先于 Mermaid（成本低、日常影响大）。实施前分别补 ADR。

**2026-11-17 更新**：外部链接基础打开与 Mermaid M1/M2 均已落地；
下一步是 Mermaid M3（暗色/CJK/导出/子进程隔离）与外部链接的 Ctrl+单击/右键/Zotero。

**2026-11-18 更新**：Mermaid M3a（暗色主题 + CJK 字体回退）已完成；
**导出与子进程隔离仍待**，外部链接的 Ctrl+单击/右键/Zotero 已在 2026-11-17 完成。
