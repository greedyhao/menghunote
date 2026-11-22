# 提交归属说明（并发事故记录）

> 历史文档：以下 commit 哈希属于 2026-11-16 整理前历史；映射与 backup 分支见 [history-rewrite-20261116](../notes/history-rewrite-20261116.md)。


> **历史留档**：本文写于 2026-11-13 历史整理前；表中哈希只在 `backup/dev-20261113` 可达，
> 见 [`history-rewrite-20261113.md`](history-rewrite-20261113.md)。

2026-11-12 多路 subagent 在同一工作区并行 `git add/commit`，发生 index 混入，导致部分提交的 message 与实际内容不符。
按 AGENTS「禁止 amend / 改写他人历史」，**未改写历史**；本文件记录实际归属，供后续追溯/必要时重建分支。

| 提交 | message | 实际内容 |
| --- | --- | --- |
| `7e7075b` | docs(brief): 记录历史记录与回收站的后续范围与待决问题 | **包含 apply receipt/纯入口实现**：`rotation_apply.rs`、`rotation_journal.rs`、`engine.rs`、`tests/rotation_apply.rs`、`examples/e2e_rotation_two_devices.rs`，外加 `docs/briefs/0006-history-and-trash.md` |
| `9bfccb8` | fix(sync): I 阶段 apply receipt 收尾与纯入口门禁 | **实际是 P0-A effective manager/prev 修复**：`http_relay.rs` + 控制链测试 hunks |
| `3916ba4` | test(sync): 补控制链 activation T 的 from_epoch 有效 authority 负例 | 与 message 一致 |
| `cf515eb` | fix(sync): checkpoint coverage 绑定固定起点并拒绝空页/停滞/缺口 | 与 message 一致 |
| `4fa8639`/`8d5d2f1` | relay 峰值门禁/文档 | 与 message 一致 |
| `0b053ab` | fix(sync): B 离线跨多 epoch 用 activation 链授权 apply | 与 message 一致 |

## 处置

- 当前以**最终树**为准（`cargo test --all`/clippy/fmt 绿）。
- 若后续需要干净历史，建议在同步 v1 收口后、经用户同意，用新分支重建/整理，而不是在 `dev` 上 amend。
- 本表在最终验收时应随 `v0.4-sync` 说明一起保留。
