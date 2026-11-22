# 回收站（P0 查看与恢复）

> 2026-11-20。删除仍是 grow-only tombstone（ADR 0001 §3.3）；本切片只加查看/恢复，
> **不做彻底删除**。

## 入口与 selector

- 最左 ribbon：`ribbon-trash`（兼容别名 `sidebar-trash`，lucide `trash-2` 图标）；
- 覆盖层：`trash-overlay` / `trash-panel` / `trash-close` / `trash-list` /
  `trash-row-<i>` / `trash-restore-<i>` / `trash-empty` / `trash-error`。

## 行为

- 列表来自 `menghunote_storage::trashed_notes`；行显示标题、更新时间、父级路径/根目录。
- 「恢复」→ `TrashEvent::RestoreRequested` → 宿主 `restore_trashed_note`：
  `StructureWriter::restore_note(node_id)`（只确认当前已观察到的删除事件）→ commit → 刷新树。
- 恢复后回到原父级；选中恢复节点，但**不自动打开正文**；提示「已恢复」。
- 失败保留列表与行，`trash-error` 显示原因；同步进行中走 `write_structure` 同一冻结门禁。
- Esc / 点遮罩 / 关闭按钮关闭，焦点还给编辑器。

## 为什么不实现“彻底删除”

数据是 append-only + 多设备同步：需要先定义墓碑清理、迟到删除事件、资产 GC、
其他设备/历史 snapshot 的边界，否则删 SQL 行或 `LoroTree::delete` 只会造成局部假象。
明确列为后续 H2 决策。

## 测试

`crates/app/tests/headless_trash.rs` 5 条（删除→列表→恢复→正文一致、根级/文件夹内、
空态、失败保留行、Esc/遮罩/关闭与焦点回归），三套 feature 全绿。
