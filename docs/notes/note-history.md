# 笔记本地编辑历史（ADR 0024 H1）

> 2026-11-19。先做**本机历史**：数据只来自本地 SQLite，不查 relay、不改同步 wire；
> 跨设备一致时间线后置。入口是笔记右键「历史记录…」。

## 1. 用户行为

1. 侧栏右键一篇笔记 →「历史记录…」；
2. 左侧时间线按时间倒序列出本机版本（时间、捕获时标题、来源；设备字段本机为空）；
3. 点击一行 → 右侧**只读预览**该版本正文；
4. 「恢复为新笔记」→ 根级新建 `原标题（历史）`（冲突追加 ` 2/3…`），把历史正文
   整篇写入并打开；**当前笔记不变**，不改写 CRDT 历史，append-only；
5. 空时间线显示「还没有本机历史版本（编辑停止约 2 秒后会自动记录）」。

覆盖层不影响编辑器会话；关闭（Esc / 关闭按钮 / 点遮罩）后焦点回到正文。

## 2. 捕获时机

| 时机 | 行为 |
| --- | --- |
| 停止输入约 2 秒 | `AppShell` 周期 timer 读一次 active 源文，与上次基线不同才写版本（0–2s 粒度，不是严格 debounce） |
| 切换笔记 | 切换前 `flush_active_history` |
| 关闭窗口 / 应用 | `close_all` 最前面 flush |
| 首次打开笔记 | 只把当前源文设为基线，**不立刻写版本** |

去重兜底在 storage：最新一行 `body_hash` 与本次正文相同就不写；因此 timer/切换/关窗
重复调用不会产生重复版本。历史写失败只提示 + 打日志，**不阻塞正文保存**。

## 3. 存储

复用 v1 的 `versions` 表；migration **v4** 补三列：

| 列 | 含义 |
| --- | --- |
| `source` | `local`（当前唯一写入方；预留 `sync` / `checkpoint`） |
| `device_id` | 捕获设备，本机为 NULL |
| `body_hash` | 正文 SHA-256，捕获去重用 |

- `body_zstd` = zstd 压缩的 **UTF-8 Markdown 源文**（不是 Loro snapshot、不是 display 文本）；
- `asset_hashes` = `asset_refs` 去重按 hash 升序、若干 32B SHA-256 直接拼接，与
  `asset_gc` 的解析口径一致（历史恢复仍保护这些资产）；
- `frontier` = `notes.body_frontier` → `loro_docs.head_frontier` → 空 BLOB；
- 保留策略在同一次写事务：每篇最近 **200** 个版本或 **90** 天，先到者；
- schema 版本 3 → 4，旧库逐级迁移；不改 `loro_updates`/同步 outbox。

### Public API（`crates/storage/src/history.rs`）

```rust
pub fn capture_note_version(vault, note_id, title, body, captured_at)
    -> Result<CaptureNoteVersion, StorageError>;
pub fn list_note_versions(vault, note_id, limit)
    -> Result<Vec<HistoryVersionSummary>, StorageError>;
pub fn load_note_version(vault, version_id)
    -> Result<Option<HistoryVersion>, StorageError>;
pub fn load_note_version_for_note(vault, note_id, version_id)
    -> Result<Option<HistoryVersion>, StorageError>;
pub fn prune_note_versions(vault, note_id, now) -> Result<usize, StorageError>;
```

常量：`HISTORY_MAX_VERSIONS_PER_NOTE = 200`、`HISTORY_RETENTION_DAYS = 90`、
`HISTORY_SOURCE_LOCAL = "local"`。

## 4. 时间线 UI

`crates/app/src/note_history.rs` 的 `NoteHistoryPanel`：

- 左侧版本列表（`note-history-row-<N>`，0 = 最新）；右侧只读预览（`note-history-preview`）；
- 预览文本用只读 `Label`，不创建编辑器、不产生 `EditRequest`；
- 恢复按钮只在选中版本后出现（`note-history-restore`），事件
  `NoteHistoryEvent::RestoreRequested { title, body }` 交给 `AppShell`；
- 固定 selector 见下表。

| selector | 含义 |
| --- | --- |
| `note-history` | 覆盖层根（遮罩） |
| `note-history-panel` | 面板本体 |
| `note-history-row-<N>` | 左侧第 N 行（0 = 最新） |
| `note-history-preview` / `note-history-preview-text` | 右侧预览容器 / 只读文本 |
| `note-history-restore` |「恢复为新笔记」 |
| `note-history-close` |「关闭」 |
| `note-history-empty` | 空态/加载失败提示 |

## 5. 恢复语义

- 只复制**正文**，不复制资产字节；`asset_hashes` 已保护被历史引用的资产；
- 新笔记走结构 CRDT 一次提交 + `EditorView::replace_source_text`（一次整篇替换 =
  一个撤销组），不把正文插到 caret；
- 不覆盖当前笔记、不删除旧版本；没有 diff/分页/搜索/跨设备归属（v1 已知限制）。

## 6. 测试

- `cargo test -p menghunote-storage`：**206**（新增 v3→v4 迁移、去重、asset_hashes、
  90 天、200 上限 5 个用例）；
- `cargo test -p menghunote-app --test headless_note_history`：**6**（2s 捕获/不重复、
  菜单+空态+Esc、预览、恢复为新笔记且原笔记不变、切换 flush、close_all flush）；
- app 默认 **592** / `--features dev-sync` **743** / `--no-default-features` **470**；
- fmt/clippy 干净。

## 7. 已知限制

- 2s 周期检查，不是严格 debounce；停止输入后版本可能延迟到 0–2s；
- 历史写库是 UI 线程上的小同步事务（与 app 现有 storage 调用一致），失败只提示；
- 只做本机时间线：`device_id` 为空、无跨设备一致时间线、无 relay 旧日志查询；
- 无 diff、分页、搜索、自动物理裁剪 UI；
- 恢复只复制正文，资产字节不复制；历史保留策略不做用户可配置。
