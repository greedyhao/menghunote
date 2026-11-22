# 过时文档归档

本目录存放 **2026-11-22 文档整理**中被判定为「已作废 / 被取代 / 已完成的待办」的文档。

> 这些文件**只用于追溯**，**不作为当前状态依据**。当前状态以 [`AGENTS.md`](../../AGENTS.md) 与 [`docs/adr/`](../adr/) 为准。
> 同批清理中，Phase 1A/1B 期间生成的一次性评审稿与代码快照（约 8.1 MB）已**直接从仓库删除**，可从 `main` 的上一个提交或 `backup/dev-20261122` 找回。

## 归档清单

| 文档 | 原位置 | 归档原因 |
| --- | --- | --- |
| [`0000-kickoff.md`](0000-kickoff.md) | `docs/briefs/` | 「新会话开场 Prompt」，写的还是旧工作区路径，已被 `AGENTS.md` 取代 |
| [`0001-storage-format.md`](0001-storage-format.md) | `docs/briefs/` | 文件自身标注「已被 0003 取代」，`AGENTS.md` 也标「已作废」 |
| [`0004-sync-v1.md`](0004-sync-v1.md) | `docs/briefs/` | 喂给 GPT 做决策的简报，结论已落入 [ADR 0006](../adr/0006-sync-v1.md) |
| [`0005-sync-status-and-plan.md`](0005-sync-status-and-plan.md) | `docs/briefs/` | 2026-11-12 供 GPT 评审的历史开发计划 |
| [`0007-sync-handoff.md`](0007-sync-handoff.md) | `docs/briefs/` | `v0.4-sync` 交接文档，其中列出的后续工作均已完成 |
| [`backlog-sync-settings-ui.md`](backlog-sync-settings-ui.md) | `docs/notes/` | 标题写「记录，未实现」，但同步设置收进设置页 / 底栏状态已完成 |
| [`editor-backlog-links-mermaid.md`](editor-backlog-links-mermaid.md) | `docs/notes/` | 编辑器待办清单，正文已注明外部链接与 Mermaid M1–M3a 全部完成 |

## 归档规则

1. 后续判定为过时的文档，**移到本目录并在上表登记**，不要直接删除有决策价值的历史材料。
2. 归档文件在标题下方统一带「已归档（2026-11-22 文档整理）」横幅。
3. 真正的一次性材料（喂模型的代码快照、评审包、重复索引）可以删除，但需在本文件或总账本
   [`docs/notes/history-rewrite-20261122.md`](../notes/history-rewrite-20261122.md) 留一行说明清理去向。
4. 引用本目录文档的链接仍应保持有效；移动文件后请跑一遍全仓链接检查。
