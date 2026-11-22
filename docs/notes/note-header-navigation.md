# 笔记顶部标题 / 路径 / 前进后退

> 2026-11-19。参考 Obsidian：笔记主体顶部显示「路径面包屑 + 前进/后退 + 可编辑大标题」，
> 标题修改直接重命名文件（侧栏同步），鼠标侧键也能前进/后退。

## 1. 布局

```
┌──────────────────────────────────────────────┐
│ ← →  00_Inbox / 访问网站用根证书              │ 28px 路径行
│ 访问网站用根证书                              │ 48px 大标题
├──────────────────────────────────────────────┤
│ 正文（EditorView）                            │
└──────────────────────────────────────────────┘
```

- 顶部是**固定 header**（不随正文滚动）；正文仍由 `EditorView` 自己滚动。
- 路径由 `active_note_path()` 沿 `TreeEntry.parent_id` 向上走，根在前、` / ` 分隔；
  目录/笔记都显示标题。
- 大标题是 `RenameInput::new_note_title`（标题语义：全选、空标题拒绝、Enter 提交、
  Esc 取消）；点击标题进入编辑，提交后走与侧栏 F2 相同的 `commit_rename_node`。
- 标题/路径都从 `self.tree` 读，重命名后 `refresh_tree()` 自动同步侧栏与 header。

## 2. 历史

- `AppShell.history: Vec<HistoryEntry>` 只存稳定的 `node_id`（不存 index，树重排后仍能定位）；
- 每次 `open_entry` 成功打开新笔记时 `record_history`：
  - 当前游标后的 forward 分支丢弃；
  - 同一篇不重复入栈；
  - 上限 100 条，超出丢最旧；
- `navigate_history(-1/+1)` 找到目标 node_id → 在树里定位 → `open_entry_with_history(record=false)`；
  打开失败不移动游标；
- 入口：
  - header 的 `←` / `→`（`note-nav-back` / `note-nav-forward`）；
  - 鼠标侧键 `MouseButton::Navigate(Back/Forward)`：根视图 `on_mouse_down` 接收，
    整个窗口（含侧栏）都生效；
- 历史只存在内存，不跨重启；没有 tab 概念。

## 3. 固定 selector（headless 断言用）

| selector | 含义 |
| --- | --- |
| `note-header` | 顶部 header 根 |
| `note-path` | 路径面包屑 |
| `note-title` | 大标题（显示态） |
| `note-title-edit` | 大标题输入框（编辑态） |
| `note-nav-back` | 后退按钮 |
| `note-nav-forward` | 前进按钮 |

## 4. 代码位置

| 层 | 文件 |
| --- | --- |
| header / 历史 | `crates/app/src/shell.rs`（`render_note_header`、`record_history`、`navigate_history`） |
| 大标题输入 | `crates/app/src/sidebar_rename.rs`（`new_note_title` / `large_title`） |
| 重命名核心 | `crates/app/src/shell.rs`（`commit_rename_node`，侧栏与标题共用） |

## 5. 测试

- `headless_excalidraw`：
  - `note_header_shows_path_and_renames_the_file`：header/path 存在，点标题改文件名；
  - `note_history_buttons_and_mouse_side_buttons`：按钮与鼠标侧键前进/后退。

## 6. 已知限制

- header 固定，不随正文滚动；
- 路径面包屑只读，点击目录不跳转/不展开；
- 历史只存内存，重启清空；没有多标签页；
- 标题编辑不自动聚焦到正文（提交后需再点正文）。
