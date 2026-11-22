# Markdown 内嵌画布预览（`![[目标.excalidraw]]`）

> 2026-11-19。ADR 0025 的**后续切片**：画布笔记（标题以 `.excalidraw` 结尾、正文是
> `.excalidraw` JSON）已经能打开/编辑/保存；本切片把**普通 Markdown 笔记里的
> 整行嵌入**投影成图片式 Object Block，显示该画布笔记的预览图，点击打开目标画布。
>
> 本文记录完整链路：编辑器侧解析/投影/请求/绘制/点击，以及宿主 `AppShell` 的
> 真实 provider 接线（读目标笔记正文 → `scene_json_to_png` → `ImageService` 解码）。

## 1. 产品行为

普通 Markdown 笔记里写整行：

```md
![[流程图.excalidraw]]
```

- 编辑器里显示**该画布笔记的预览图**（图片式 Object Block，与图片同一套几何/选中/
  删除路径）；
- 点击预览 → **打开目标画布笔记**（复用现有 wiki 打开路径，宿主负责按标题/路径解析）；
- 目标不存在 / 预览失败 → 显示占位文案（不 panic、不改正文）：
  - 加载中：「画布预览加载中…」
  - 目标不存在：「画布笔记不存在」
  - 渲染/解码失败：「画布预览失败」
  - 有别名时占位文案优先用别名（显示文本只用于占位）；
- `![[A.excalidraw|别名]]` 的 target 取 `|` **之前**（`A.excalidraw`），别名只用于占位；
- 混排行内 `![[a.excalidraw]]`、代码块/围栏/表格/引用/列表/标题里的嵌入、
  目标不以 `.excalidraw` 结尾（大小写不敏感）的嵌入**都不是**画布对象，继续按普通
  文本显示（绝不吞半行内容）。

## 2. 解析（`crates/markdown/src/lib.rs`）

新增 `DrawingSpan` 与 `ParsedMarkdown::drawings`（只收录可投影的整行写法）：

| 字段 | 含义 |
| --- | --- |
| `span` | 整行内容区间（行内容起点 .. 行内容终点，不含行尾空白/换行） |
| `target` | `|` 之前的源码区间（已去首尾空白，大小写不敏感以 `.excalidraw` 结尾） |
| `alias` | `|` 之后的源码区间（已去首尾空白；空白别名 = 没有别名） |
| `line` | 源码行号（0 起） |
| `block` | 是否可投影（当前恒为 `true`；保留字段与其它 Object 块一致） |

判定（写死，`drawing_tests` 单测钉住）：

1. 整行独占：行内容去掉行尾空白后必须以 `![[` 开始、以 `]]` 结束；
   **行首不允许缩进**（与图片/表格/围栏一致，避免占位框与 display 位置错开）；
2. 普通段落上下文：`LineKind::Paragraph`，非引用、不在列表项里、不在代码块/表格/
   未支持块（HTML）区间内；
3. target 非空且大小写不敏感地以 `.excalidraw` 结尾；内层不允许嵌套 `[[` / `]]`；
4. 不满足时不产生任何记录（该行按普通文本显示）。

`![[...]]` 既不会成为普通图片（没有目标 URL），也不会成为 wiki 链接
（`scan_wiki_links` 明确跳过 `![[`）。

## 3. 编辑器模型（`crates/editor`）

- `ObjectKind::Drawing`：与 Image/Table/Mermaid 共用 `ObjectBlock`
  （`index` 对应 `MarkdownDocument::drawings()` 的下标）；`label()` = 「画布」。
- `MarkdownDrawing`（`crates/editor/src/model/markdown.rs`）：

  ```rust
  pub fn span(&self) -> SourceRange;          // 整行源码区间
  pub fn target(&self) -> &str;               // `|` 前的目标标题（已 trim）
  pub fn target_span(&self) -> SourceRange;   // 目标在源码里的区间
  pub fn alias(&self) -> Option<&str>;        // `|` 后的显示文本
  pub fn line(&self) -> usize;                // 对象行
  pub fn is_block(&self) -> bool;             // 当前恒为 true
  ```

- `MarkdownDocument::drawings()` / `object_drawing(object)`；
  `EditorController::drawings()` / `drawing_of(object)`。
- 投影：整行源码被替换成一个 `OBJECT_PLACEHOLDER`（U+FFFC）——与图片一致；
  显式源码模式（`revealed_object_lines`）下显示原源码，可用于 F2/工具条路径。
- 点击 **不选中**对象、不移动 caret、不进源码模式（`select_object_at` 提前返回）。

## 4. 预览请求与绘制（`crates/app/src/editor/view.rs`）

**主线程不解析/渲染画布**。预览走已有的 `ImageService`（后台解码线程 + LRU）：

```text
render 每帧: request_object_images()
  └─ 画布对象: images.request(drawing_asset_key(target))      // 合成 key，幂等
后台 worker: host provider.read_asset(key) → PNG 字节 → 解码缓存
prepaint/绘制: images.state(key) → Loading/Missing/Ready/Failed
              → ObjectPaintContent::Image（复用 ImagePaint + fit_size）
```

- Object 高度 = `内容高度 + 2 × OBJECT_PADDING`；就绪按内容宽度等比（`fit_size`），
  其它状态为 `OBJECT_PLACEHOLDER_HEIGHT` 占位框（与图片一致）；
- 占位/失败文案见 §1；就绪时不叠文字；
- 画布对象**没有**悬浮工具条（不提供「编辑源码」「放大」）：单击即打开目标画布；
- 光标悬停仍显示手形（对象 hitbox 与图片共用）。

### 4.1 合成资产 key 协议（`crates/app/src/drawing.rs`）

```rust
pub const DRAWING_ASSET_PREFIX: &str = "drawing://";
pub fn drawing_asset_key(target: &str) -> String;              // drawing://<theme>/<target>
pub fn parse_drawing_asset_key(key: &str) -> Option<(&str, &str)>; // (theme, target)
pub fn drawing_theme_is_dark(theme: &str) -> bool;
```

- `<theme>` = 当前主题 id（`light`/`dark`）：深/浅色画布渲染结果不同，必须分开缓存，
  否则切主题会拿到旧反色图；切主题后 key 变化，下一次 `request` 自然重新生成；
- 按**第一个** `/` 分割主题与目标：目标可以是 `文件夹/子图.excalidraw`；
- 只做字符串拼接/解析，不读库、不渲染；
- 真实 `asset://<64 hex>` 不受影响（`parse_asset_hash` 只认 `asset://`）。

### 4.2 scene JSON → PNG（`scene_json_to_png`，纯 CPU helper）

```rust
pub fn scene_json_to_png(scene_json: &str, dark: bool) -> Result<Vec<u8>, String>;
```

- `gpui_excalidraw::Editor::new()` 是纯结构（不依赖 GPUI `App`/窗口），可在后台线程调用；
- `load_scene_json` 解析 `.excalidraw` 信封：空串/非 JSON/结构损坏返回 `Err`
  （**不 panic**），调用方降级成占位；`{}`（无 `elements` 键）被 vendor 视为空场景，
  导出空白 PNG（新建画布正文就是空串，不应报错）；
- 用 `gpui_excalidraw::render::export::export_png(scene, bg, scale)` 光栅化；
  深色背景 `#1e1e1e`、浅色 `#ffffff`，倍率 `1.0`（控制内存）；
- 不修改 vendor 依赖图，不新增依赖。

## 5. 宿主接线（已完成）

1. **`vault_asset_provider` 识别 `drawing://`**（`crates/app/src/shell.rs`）：
   - `parse_drawing_asset_key` → `(theme, target)`；
   - `drawing_preview_bytes` 用 `list_notes` 解析目标（完整路径唯一匹配优先，不含 `/`
     时标题唯一匹配兜底；无匹配/歧义返回 `None`）；
   - 读目标 `body_doc_id` 的正文（`templates::read_body_source`），
     `drawing_theme_is_dark` 选深浅背景，`scene_json_to_png` 生成 PNG；
   - 找不到/正文损坏/渲染失败 → `None`（编辑器显示占位），**不 panic**；
   - 该闭包在 `ImageService` 后台线程执行，主线程只读 `ImageState`。
2. **点击打开**：编辑器发 `EditorEvent::WikiLinkRequested { target, heading: None }`，
   `AppShell::open_wiki_link` 按标题/路径打开目标画布笔记；打开后现有画布覆盖层逻辑
   自动接管（未找到只提示，不自动建）。
3. **主题切换**：key 变化，下一帧自动重新请求；缓存按 theme+target 分开。

## 6. 测试

| 命令 | 覆盖 |
| --- | --- |
| `cargo test -p menghunote-markdown --lib drawing_tests` | 8 个：整行/大小写/别名/混排/缩进/代码/引用/列表/表格/HTML/CRLF/不产生 Image+Wiki |
| `cargo test -p menghunote-editor --lib drawing` | 7 个：Object 载荷/投影占位/源码模式/别名/非画布保持原文/`drawing_of` |
| `cargo test -p menghunote-app --lib drawing::tests` | 10 个（含既有画布覆盖层用例）：key 协议往返/非法 key/两种主题 PNG/损坏 JSON 不 panic |
| `cargo test -p menghunote-app --test headless_excalidraw_embed` | 2 个：真实 `EditorView` + fake provider（Ready/缺失/几何/点击事件/正文不变）|
| `cargo test -p menghunote-app --test headless_excalidraw` | 21 个（含真实 vault provider 渲染预览、点击嵌入打开目标画布）|

headless 集成测试的 fake provider 命中 `drawing://` key 时返回 8×8 PNG；断言
`display_text` 不出现 `![[...]]`、对象矩形 8×8、点击发出 `WikiLinkRequested`
（target 正确）、provider 缺失时「画布笔记不存在」占位且正文逐字节不变。

app 默认 **592** / `--features dev-sync` **743** / `--no-default-features` **470**；
markdown **53** / editor **299**；fmt/clippy 干净。

## 7. 已知限制

- 预览图不随编辑器缩放/DPI 变化重新渲染（`DRAWING_PREVIEW_SCALE = 1.0`）；
- 无「打开画布」悬浮按钮（点击预览本身就是打开）；无右键菜单项；
- 目标画布正文的读取/缓存/失效策略在宿主 provider（本切片不读库）；
- **预览缓存 key 只含主题 + target**：目标画布在别的视图里被修改后，已解码的预览
  不会自动失效（切走再回来会重建 `EditorView`/`ImageService`，缓存自然重来；
  同一视图内保持打开时可能看到旧图）。后续可把 body 修订/内容哈希并入 key，或由
  宿主在画布保存后清对应 key；
- `![[a.excalidraw]]` 在**行首缩进**（1 个及以上空格）时不投影（保持普通段落），
  与图片/表格/围栏的 Object 判定一致；
- 真实 Windows 真机像素/DPI 仍需验证（headless 只证明几何/状态机/事件链）。
