# 代码块只读语法高亮（syntect）

> 2026-11-21。代码块在现有源码投影编辑器里按 token 着色；**只改颜色，不改布局、
> 编辑、选区、IME、滚动、撤销**。不引入 GPUI TextView、tree-sitter 或 C 依赖。

## 方案

- `crates/markdown` 的 `ParsedMarkdown.code_blocks: Vec<CodeBlockSpan>` 暴露每个
  fenced/indented 代码块的 `span` / `content` / `opening` / `info` / 行号 / `fenced`；
  和既有 `code_spans` 口径一致，mermaid 块也在内。
- `crates/app/src/editor/code_highlight.rs`：`syntect 5.3`（纯 Rust，
  `default-features=false` + `parsing/default-syntaxes/regex-fancy`）解析 TextMate
  scope → 10 类 `CodeHighlightClass`；未知语言/`text`/空块/超 256 KiB 降级无色。
- `EditorView` 两级缓存：块级 `(语言, 内容哈希, 块区间)` 只在 source 变化时重跑
  syntect；display 级按 `(source_hash, selection, theme_id)` 重映射（围栏隐藏/显现
  会改变 display 偏移，但不需要重跑语法解析）。每帧只克隆一个 `Rc`。
- 绘制只在 `line.mono && 在代码块内` 走 `glyph::shape_row_code`；字体/字号与普通
  shaping 相同，行宽、caret、命中几何不变。未命中 token 的文本用正文色。
- 主题新增 10 个 token（关键字/字符串/注释/数字/类型/函数/变量/运算符/标点/属性），
  浅色/深色各一套；切主题自动失效重绘。
- **整块灰底**：`EditorElement::paint_code_block_backgrounds` 在正文/选区之前，按
  `code_block_display_ranges()` 画 `code_bg` 圆角矩形（只给 fenced/indented，行内代码
  仍走 `TextRun.background_color`）；组合帧/对象块不画。
- **复制按钮**：`EditorView::render_code_copy_buttons` 在光标/选区不在块内且非组合时，
  于块首行右上角渲染 `code-copy-<i>`（26×22，copy SVG）；`code_copy_rects` 让元素层
  把点击当浮层放行，`swallow_mouse_up` 吞掉抬起，所以不移动 caret。复制
  `controller.code_blocks()[i].content`（围栏之间源码，不含 info/围栏；indented 整块）。

## 依赖

`syntect = "5.3.0"`，精确 pin 在 `crates/app/Cargo.toml`；显式关闭 onig/C，
未引入 `two-face`、tree-sitter、GPUI TextView。release 增量未测，预计 +2–4 MB。

## 测试

- markdown 8 个代码块元数据单测；app `headless_code_highlight` 4 个：
  Rust 着色/display 区间、纯文本与未知语言 0、深浅色 token 不同且 span 保留、
  代码块内点击+输入可编辑、围栏显现后重新映射、无 panic。
- `code_highlight` 9 个单测 + `glyph::code_text_runs` 3 个（覆盖全文、UTF-8 边界、
  越界/空 span）。

## 已知限制

- syntect 是行级 TextMate 语法，不是 tree-sitter 的结构解析；嵌套模板/TSX 等复杂
  语言准确度有限，但代码块显示场景可接受。
- 首次遇到代码块时 `SyntaxSet` 初始化有一次性数十 ms 开销；有代码块时每帧对 source
  做一次哈希（无代码块零开销）；超大块（≤256 KiB）内打字会按块重跑 syntect。
- 未在 Windows 真机看像素/DPI；纯 Rust 依赖不触发 C 构建限制。
