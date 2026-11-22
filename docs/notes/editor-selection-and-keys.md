# 编辑器：鼠标选择与常用快捷键

> 2026-11-20。正文编辑器与标题栏的选区/按键约定。

## 鼠标

- **单击正文**：mouse-up 才落 caret（mouse-down 只记锚点），避免按下瞬间显现隐藏 Markdown 并闪蓝选；3px 内抖动仍算单击。
- **拖拽**：越过阈值后实时扩选。
- **Shift+单击**：从原选区 anchor 扩到点击处，连续 Shift+单击继续扩同一 anchor。
- **标题栏**：点击按位置落 caret；按住拖动可正向/反向选字；拖出标题区域仍更新；点击其他区域确认标题编辑。
- **标题栏双击**：按词选（`word_range_at`）；连续 CJK+ASCII alphanumeric/`_` 算一个词，空白/标点/emoji 只选自身一个字符；双击后随后的轻微移动不把词选区塌成 caret。
- **Esc**：有选区时先折叠到 head；无选区再走对象源码/表格选中/菜单等既有行为。

## 快捷键

| 按键 | 行为 |
| --- | --- |
| `Ctrl+←/→` | 按词移动（display 文本，隐藏标记不参与） |
| `Ctrl+Shift+←/→` | 按词扩展选择 |
| `Ctrl+L` | 选中当前源码行（含行尾换行） |
| `Ctrl+Home/End` | 到文件开头/结尾（display 起点，frontmatter 存在时落正文） |
| `Ctrl+Shift+Home/End` | 从 anchor 选到文件首/尾 |
| `Ctrl+B` / `Ctrl+I` | 加粗/斜体（可改绑） |
| `F2` | 重命名当前笔记（可改绑） |
| `Ctrl+K` / `Ctrl+F` | 搜索（可改绑） |

- 中文连续汉字按一个词 run；标点/空白/emoji 是分隔符；不做 ICU 词典分词。
- IME 组合中 Ctrl+方向键不处理，交输入法；标题栏输入法组合时点击/拖选不动 marked。

## 焦点与 caret

- 标题输入框获得焦点时，正文 **不画 caret**（修复“标题一个光标、正文又一个光标”）；提交/取消标题后焦点还给正文。
- 正文 caret 约 **530 ms** 闪烁：复用 50 ms `pump` 推进相位（不为光标单独起 timer）；按键/点击/移动/聚焦失焦都立刻回到可见并重新计时，IME 组合期间常亮。
- IME preedit 会先插入临时 **display 文本**再折行：组合串占据真实宽度，后续文字被推移/折行（Obsidian 式），不再盖在已有文字上。本帧**唯一** caret 画在组合串末尾，起点不重复画；preedit 下划线与 caret 闪烁相互独立。
- 组合期间 `EditorElement` 用 `EditorView::composition_display()` 的临时文本布局，`Controller` 的 source/投影/选区坐标不变；空选区替换可视化删除选区文本，提交/取消后下一帧回到正常布局。跨换行替换也进入临时布局，行度量沿用原 display 行，提交后修正。
- 标题/重命名输入框 caret 同样约 **530 ms** 闪烁（独立 timer，输入/点击/移动重置；`caret_blink_visible` 供 headless 断言）。
- 组合串进入临时布局后，`compose_style_runs` 会删除选区样式、插入 Plain preedit、平移后缀样式；空 spans 仍要有覆盖全文的 Plain run（GPUI 0.10 `shape_line` 零字形回归）。

## 标题编辑样式与 About

- 标题非编辑态与编辑态统一 **28px BOLD**；编辑输入框占满 **48px 行高**（`is_large_title()`
  时 `style.size.height = relative(1.0)`），所以点击后文字基线不跳、不再「上移 7px」。
- 标题与编辑输入框都挂 **`cursor_text()`** 文本光标（不再用手形指针）。
- 设置 →「关于」版本文案为 **`menghunote {version}（GPUI Kit 0.10.0；SQLite 单库为持久化真源）`**，
  中英 catalog（zh-CN / en）同步更新。

## 测试

- `headless_pointer_and_format`：mouse-up、Shift+单击、拖拽、Ctrl+B/I。
- `headless_editor_shortcuts`：按词移动/选择、Ctrl+L、Ctrl+Home/End、Esc 折叠。
- `headless_note_title`：点击定位、拖选（正/反向/越界）、粘贴。
- `sidebar_rename::tests::double_click_word_range_handles_cjk_ascii_and_punctuation`：CJK+ASCII 同词、空白只选自身、emoji 不并词。
- `headless_ime_preedit`：F8 preedit 进入绘制路径、插入布局文本推移后续文字、串尾 caret 位置、取消无残留。
