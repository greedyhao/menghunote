# 编辑器待办：外部链接与 Mermaid

> 2026-11-16 记录。**外部链接基础打开已实现（2026-11-17）**：普通 `[text](url)` 单击
> 打开 http/https/mailto，悬停手形；Ctrl+单击/右键菜单/Zotero/file 确认尚未做。
> Mermaid 仍未实现、未加依赖。

## 1. 外部链接打开（含 Zotero）

### 现状（2026-11-17 更新）

- 已解析 `[text](url)` 的 destination（`LinkSpan`），app 按可见文本区间命中；
- 单击打开 `http`/`https`/`mailto`，悬停手形；末尾边界不误触；
- 仍未做 Ctrl+单击、右键菜单、Zotero/file 确认、未知 scheme 持久化。

### 候选方案

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

### 待定问题

- Ctrl+单击 vs 设置项单键打开；
- 未知 scheme 确认的持久化范围（会话/全局）；
- `file:` 是否允许、如何限制目录；
- 内部链接（`#heading`、相对笔记）另做跳转，不走系统。

## 2. Mermaid 渲染

### 候选依赖（2026-11-16 更新：优先 Merman）

- **首选 `merman = "=0.10.0"`**，`default-features = false`：
  - `features = ["all-diagrams", "complete-svg"]`（全图 + SVG + layout-cytoscape，
    避开默认的 `layout-elk`/EPL 组件；实际编译时再按需收窄）；
  - 纯 Rust，无 Node/Chromium/WebView；官方对齐 Mermaid 12.3，Zed 已采用；
  - Rust 1.95+，当前工具链满足；MIT/Apache-2.0；
  - API：`Renderer::new().render(RenderRequest::svg(source, OperationControl::new(), SvgRequest::default()))`
    → `RenderOutput::Svg(Some(svg))` → `svg.svg()` 字符串；
  - GPUI 用 `ImageFormat::Svg` 显示，和之前方案相同。
- 备选 `mermaid-rs-renderer = "=0.3.1"`（default-features=false，更轻、23 图），
  但成熟度/对齐度不如 Merman；只在 Merman 集成受阻时回退。
- 不引 Node/Chromium/WebView，不走远程渲染服务。

### 交互

- ` ```mermaid ` fenced code 仍是源码真源；
- 渲染块是 Object Block：单击/选中只选块，**不直接展开源码**（Obsidian 式）；
- 悬浮工具条：「编辑源码」「重新渲染」「复制源码」；点「编辑」/F2 才进入源码模式，
  Esc/点击块外/Ctrl+Enter 退出；
- 失败/渲染中显示状态，不阻塞编辑；
- 缓存 key = hash(source + crate 版本 + 主题 + 缩放)，LRU，不同步；
- Markdown 导出保留 fenced code；HTML/PDF 导出再接渲染器。

### 风险

- 0.3.x 仍是早期项目，视觉质量可能不完全等同 mermaid-cli；必须保留源码回退；
- 不可信 Mermaid 输入可能 panic/超时/OOM：输入/节点上限、`catch_unwind`、后台线程；
  更稳的是独立 helper 子进程 + 超时/资源限制；
- 新依赖需 pin 版本、依赖评审、黄金图测试；Merman 的 Mermaid 12.3 对齐不等于逐像素一致，仍需样本回归；
- 暗色主题、CJK 字体、导出、SVG 安全校验后续处理。

### 切片（未排期）

- M1：Merman 依赖 pin + SVG 渲染封装 + 黄金/失败回退测试；
- M2：fenced code 预览/源码切换 + 缓存；
- M3：暗色/导出/子进程隔离。

## 优先级

这两项排在当前 **S1 解析/列表 → S2 粘贴/图片 → S3 表格/任务/GC** 之后；
外部链接优先于 Mermaid（成本低、日常影响大）。实施前分别补 ADR。
