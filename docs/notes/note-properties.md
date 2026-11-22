# 笔记属性（YAML frontmatter）

> 2026-11-19。Obsidian 风格：笔记最开头写 `---` 后，编辑器把它变成「笔记属性」面板，
> 源码里的 YAML 不再铺成正文。

## 1. 数据格式

只认**文档第 0 行**的 frontmatter：

```markdown
---
tags: []
date_created: 2025-08-10T22:23:41
date_modified: 2025-11-25T17:54:49
---

正文从关闭分隔符的下一行开始。
```

- 打开/关闭分隔符：`---`（关闭也接受 `...`）；
- 属性行按**第一个 `:`** 切成 key/value；空行与 `#` 注释行跳过；
- 值只当字符串保存（`[]`、引号、日期、数字都不做类型转换）；
- 没有闭合行 / 不在第 0 行 → 不是 frontmatter，按普通 Markdown 水平线处理。

## 2. 交互

- **自动补全**：空笔记第一行输入完 `---` 就**立即**变成
  `---\n\n---\n`（不用 Enter），caret 落在关闭分隔符后的正文起点（中间空行是隐藏的，
  不能直接输入），属性面板出现；
- **面板（正文顶部块）**：frontmatter 存在时，正文整体下移面板高度，
  面板用 `-scroll_y` 平移，**随正文一起滚动**（像图片/表格块一样，不固定在窗口上方）：
  - 标题行 `▾/▸` 折叠/展开，折叠后只留标题行、正文随之上移；
  - 标题行「清除」→ 清空全部属性（保留 `---` 分隔符，仍可继续添加）；
  - 每行 `类型图标 + key + value`；空值显示「没有值」；
  - 点 key / value → 行内输入框（复用侧栏重命名的 IME 输入框，默认全选）；
  - `+ 添加笔记属性` → 追加 `property: ` 并直接编辑 key；
  - 行尾 `×` → 删除该属性；
- **写回**：每次提交是**一条整篇替换**（`EditorView::replace_source_text`），
  一个撤销组；写回后 caret 回到正文起点，不把滚动位置带到文末；
- **源码隐藏**：frontmatter 的 source 区间在投影里永远 `Hidden`，不参与
  caret 显现；打开笔记时初始 caret 落在正文起点，避免退格改到 `---`。

## 3. 分隔线

frontmatter 之后、正文里的 `---`（以及 `***` / `___`）按 CommonMark 渲染成一条横线：

- 语义来自 `menghunote-markdown` 的 `LineKind::ThematicBreak`；
- 源码整行在投影里隐藏，display 是一条空行；
- `EditorElement::paint_thematic_breaks` 在行高中心自绘 1px 横线，宽度 = 正文内容宽；
- 它不是 Object，不参与图片/表格工具条；caret 落在该空行时仍是普通文本行。

## 4. 代码位置

| 层 | 文件 |
| --- | --- |
| 纯解析/渲染 | `crates/editor/src/model/frontmatter.rs` |
| 投影隐藏 + 初始 caret | `crates/editor/src/model/markdown.rs`、`controller.rs` |
| 自动补全 | `crates/app/src/editor/view.rs`（`try_open_frontmatter`） |
| 属性面板 | `crates/app/src/note_properties.rs` |
| 宿主创建/注入 | `crates/app/src/shell.rs`（`open_entry`，弱引用注入编辑器） |
| 滚动/折叠布局 | `crates/app/src/editor/view.rs`（`properties_inset`）+ `element.rs`（整体下移） |
| 分隔线 | `crates/editor/src/model/block.rs`（`BlockKind::ThematicBreak`）+ `element.rs`（自绘横线） |
| 文案 | `crates/i18n` 的 `properties.*` + `crates/app/src/ui_text.rs` |

## 5. 固定 selector（headless 断言用）

| selector | 含义 |
| --- | --- |
| `note-properties` | 属性面板根（没有 frontmatter 时不渲染） |
| `property-row-<index>` | 第 index 条属性行 |
| `property-key-<index>` | 点击编辑 key |
| `property-value-<index>` | 点击编辑 value |
| `property-delete-<index>` | 删除该属性 |
| `property-edit` | 行内输入框覆盖层 |
| `properties-add` | 「添加笔记属性」 |
| `properties-collapse` | 折叠/展开面板 |
| `properties-clear` | 清除全部属性 |

## 6. 测试

- `crates/editor`：frontmatter 解析/渲染 5 个单测；
- `crates/app/tests/headless_excalidraw.rs`：
  - 面板渲染 + YAML 不铺进正文；
  - `---` + Enter 自动补全；
  - 编辑 value 写回 YAML；
  - 添加/删除属性；
  - 已有正文时在最开头输入 `---` + Enter 不吞正文；
  - 属性面板随正文滚动（scroll_by 40px 后位置同步上移）；
  - 属性面板可折叠、可清除全部属性；
  - 正文起点 Backspace 不动隐藏 frontmatter；
  - 正文 `---` 被识别为分隔线，源码不铺出来。

## 7. 已知限制

- 属性很多时面板整体高度会继续把正文下推（没有独立折叠/搜索）；
- 只支持常用 YAML 子集；编辑属性会**重写整段 frontmatter**，手工注释/空行不保留；
- 没有类型选择器/自动补全/日期选择器（key 与 value 都是文本框）；
- 不做 `![[xxx.excalidraw]]` 嵌入、不做属性查询/模板变量。
