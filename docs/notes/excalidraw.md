# 画布笔记（`.excalidraw`）+ Excalidraw 编辑器覆盖层

> 2026-11-19。范围：`crates/app` 的**第一方编入模块**（assets / drawing / shell /
> editor view / main）+ i18n 文案 + 文档/测试。
> **不改** `crates/storage` schema、`crates/sync`、`crates/relay`、`crates/excalidraw/src/**`
> 的 wire/协议/数据模型。本轮先以 `wip:` 提交，Windows 真机验收后再转正式。

## 1. 产品决定（已确认，勿改回去）

| 决定 | 含义 |
| --- | --- |
| **画布就是普通笔记** | 标题以 `.excalidraw` 结尾（**大小写不敏感**）即被识别；正文存 `.excalidraw` JSON 信封。**不新增** note kind / schema 列 / 同步消息——标题就是文件名语义。 |
| 打开画布 = 普通 `EditorView` + 覆盖层 | 仍创建现有 `EditorView`（正文是 JSON 文本），再在**编辑器内容区**叠一层 Excalidraw 覆盖层（侧栏可见）。编辑器/会话/同步/物化全部复用现有链路。 |
| 保存 = 整篇替换 | 把 `scene_json()` 一次性写回活动 `EditorView` 的源文（一条 replace-all 编辑 / 一个撤销组），然后关闭覆盖层。 |
| 取消 = 丢弃覆盖层 | 正文一个字节都不动；关闭后显示**占位页**，绝不把 JSON 源码铺到编辑区。 |
| 不做 Markdown 嵌入 | `![[xxx.excalidraw]]` 嵌入预览**本轮不做**（后续切片）。 |
| 真实第三方插件运行时仍不存在 | 这是**第一方编入模块**；可在设置插件页展示，但本轮不强制加开关。 |

## 2. 模块与依赖

| 文件 | 职责 |
| --- | --- |
| `crates/app/src/assets.rs` | `AppAssets`：`gpui_excalidraw::ui::icons::IconAssets` 叠在 `gpui::assets::Assets` 之上（两套嵌入式资源共存）。 |
| `crates/app/src/drawing.rs` | `is_drawing_title` / `display_title` / `DrawingSession` / `DrawingOverlay` / `DrawingOverlayEvent`。 |
| `crates/app/src/shell.rs` | `AppShell.drawing` 字段、`open_entry`/`create_drawing`/`save_drawing_overlay`/`close_drawing_overlay`、根层覆盖层。 |
| `crates/app/src/editor/view.rs` | `EditorView::replace_source_text`（整篇替换，走现有 `insert_text`）。 |
| `crates/app/src/main.rs` | 三条启动路径统一 `gpui::platform::application().with_assets(AppAssets)` + `gpui_excalidraw::register_hand_drawn_fonts(cx)`。 |
| `crates/app/src/ui_text.rs` + `crates/i18n/**` | 固定文案（`drawing.*` / `sidebar.new_drawing`），中英双语。 |

依赖：`crates/app` 新增 `gpui-excalidraw = { path = "../excalidraw" }`（默认 feature）。
vendor 自己精确 pin `gpui-kit = 0.10.0`，与仓库基线一致，**不引入新的版本解算**；
三种构建（默认 / `--features dev-sync` / `--no-default-features`）都编进来。

## 3. 识别与命名

```rust
pub const DRAWING_SUFFIX: &str = ".excalidraw";
pub fn is_drawing_title(title: &str) -> bool;   // 后缀，大小写不敏感
pub fn display_title(title: &str) -> &str;      // 当前保留原样（含后缀）
```

- `未命名画布.excalidraw` / `未命名画布.EXCALIDRAW` / `.excalidraw` 都算画布；
- `未命名画布.excalidraw.md` / `excalidraw` / 空串都不算；
- 侧栏「画布」新建的默认标题是 `未命名画布.excalidraw`，重名时**把序号插在后缀之前**
  （`未命名画布 2.excalidraw`）——不能直接用 `unique_title`（它把序号追加到末尾，
  会得到 `未命名画布.excalidraw 2`，那样就不再被识别为画布）。

## 4. 保存语义（硬约束）

1. 覆盖层打开时把活动 `EditorView` 的 `source_text()` 交给 `DrawingSession`：
   - 合法 JSON → 加载场景；
   - 空串 / 纯空白 / 非 JSON / 结构不合法 → **回退空场景**（不 panic、不弹错）；
   - 基准取**规范化后**的 JSON（否则"打开即脏"）。
2. 「保存并关闭」：
   - `overlay.read(cx).scene_json()` → `EditorView::replace_source_text(&json, cx)`；
   - `replace_source_text` 先把选区设成整篇（`0..source_scalar_len`，**scalar** 坐标），
     再走 `insert_text` → `compose_replace_selection`：一次 `EditRequest` = 一个撤销组，
     同步 outbox / B 物化 / 资产引用对账全部沿用现有链路；
   - 成功 → 关闭覆盖层 + 提示「画布已保存」；
   - 失败（序列化失败 / 编辑器拒绝）→ **只提示、不关闭**（用户可重试）。
3. 「关闭（不保存）」：丢弃覆盖层，正文零变化。
4. `Esc` / 点外部**不**关闭覆盖层（避免误丢画布改动），必须点按钮。

`dirty` 用 initial vs current 的 **JSON 字符串比较**（不依赖 vendor 内部状态）。

## 5. 覆盖层与其它模态

- 覆盖层挂在**编辑器内容区**，侧栏保持可见；vendor 的命中以 `canvas` 元素在
  窗口里的原点校正（`local_point`），绘制时再把原点加回 `Window::paint_*` 的窗口
  坐标（`paint_scroll_*`），所以内容区偏移不会造成“点哪画哪”错位；
- 保存/关闭动作放在 shell **底栏状态栏**，不再用右上角浮动 bar 遮挡 Excalidraw 工具栏；
- 画布支持**鼠标中键拖动平移**（与空格/抓手工具共用同一 pan 状态）；中键释放
  用窗口级 capture 监听兜底，即使指针已经拖到侧栏/状态栏也能停止；
- 绘图预览在**按下瞬间不套 100×100 默认尺寸**，只有真正“点击提交”才用默认尺寸，
  避免先闪一个大图形再缩成小图形；
- 抓手/选择/绘图工具分别显示系统手形、箭头、十字光标；中键拖动也显示手形。
  Windows 的 GPUI 后端目前只把 `PointingHand` 映射到 `IDC_HAND`，真正的
  open/closed 抓手需要自定义光标资源（后续再做）；
- 其它**全屏模态**（搜索 / 图谱 / 设置 / 模板选择器，通常由快捷键/环境触发）打开时覆盖层**让位**：
  实体保留（场景与撤销栈不丢）、只是不渲染——Excalidraw 编辑器每帧会自抢焦点，
  继续渲染会抢走设置页输入框/搜索框的焦点。关闭模态后自动回来；
- 切换笔记 / 重命名 / 删除时**关闭**覆盖层（丢弃未保存改动），避免 stale note。
- 关闭后编辑区显示 `drawing-placeholder`（标题 + 说明 + 「打开画布」`drawing-open`），
  不显示 JSON；点同一条笔记或「打开画布」重新进画布。
- 把当前活动笔记重命名为 `.excalidraw` 时立即重开画布（不闪 JSON 源码）。

## 6. 固定 selector（headless 断言用）

| selector | 含义 |
| --- | --- |
| `sidebar-new-drawing` | 侧栏「画布」按钮（新建 `.excalidraw` 笔记） |
| `drawing-overlay` | 覆盖层根（存在 = 打开；`debug_bounds` 为 `None` = 关闭） |
| `drawing-title` | 状态栏里的画布标题（`画布 · <标题>`） |
| `drawing-save` | 「保存并关闭」 |
| `drawing-close` | 「关闭（不保存）」 |
| `drawing-editor` | Excalidraw 编辑器容器（填满内容区） |

## 7. 测试

| 层 | 用例 |
| --- | --- |
| 单测（`src/drawing.rs`） | `is_drawing_title` 大小写/边界；`display_title`；空/空白/损坏 JSON 回退空场景；合法 JSON roundtrip；`dirty` 跟随场景变化。 |
| headless（`tests/headless_excalidraw.rs`） | 新建画布 → 覆盖层出现（只占内容区，侧栏可见）；注入矩形 → 保存 → 活动 `EditorView` 源文变成合法 `.excalidraw` JSON；关闭不保存不改正文；重开画布读回场景；普通笔记不出现覆盖层；重名序号在后缀之前；关闭后显示占位页（不显示 JSON）、点「打开画布」重新渲染。 |
| 既有回归 | `cargo test -p menghunote-app`（默认 / `--features dev-sync`）、`cargo test -p gpui-excalidraw` 全绿。 |

## 8. 已知限制 / 未做项

- vendor 的 `viewport` 由窗口尺寸减去画布原点得到；首帧原点尚未建立时可能有一帧
  尺寸偏差，之后自动校正。
- `Window::paint_*` 用窗口坐标而不是元素局部坐标：命中扣原点、绘制加原点，两者必须
  成对，否则会整体平移一个侧栏宽度（Windows 真机已实测）。
- 元素级 `on_mouse_up` 只在 hitbox 仍悬停时触发；中键平移的释放改用
  `window.on_mouse_event` capture 监听，避免拖出画布后“松手还在平移”。
- headless 已有嵌套“侧栏 264px + 绝对覆盖层”的画布拖拽测试，验证命中会扣除
  内容区原点；真实“点哪画哪”仍需 Windows 真机验（DPI/原生窗口坐标）。
- 侧栏切走/重命名/删除会关闭画布，丢弃未保存改动。
- `![[xxx.excalidraw]]` Markdown 嵌入预览未做（后续切片）。
- 画布笔记的**搜索/图谱**仍按普通笔记处理（正文是 JSON 源码，不做场景内检索）。
- 没有"未保存改动"的二次确认：切笔记/重命名/删除会**静默丢弃**画布改动
  （与"关闭（不保存）"同义）。
- Excalidraw 的**语言**跟随 vendor `Language::detect()`（`LANG`），**不**跟随应用
  `language.json`；主题跟随应用 `theme_id()`（浅色/深色）。
- 真实 Windows 真机（DPI/输入法/像素）未验。
