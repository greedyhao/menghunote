# 2026-11-16 历史整理（v0.4-sync 之后）

`v0.4-sync`（commit `d16f6fe`）之后的 **97 个提交**整理为 **12 个语义提交**；
代码 tree 与整理前 tip `d9015c9` 一致（仅 `AGENTS.md` 的当前状态/测试数字另行更新）。

整理前历史完整保留在分支 **`backup/dev-before-cleanup-20261116`**（指向 `d9015c9`）。
`v0.4-sync` annotated tag **未移动**。

## 新历史与旧范围

| # | 新提交 | 主题 | 旧提交范围（含首尾） |
|---|---|---|---|
| 1 | `443150f` | dev-sync 面板/串行 worker 骨架（ADR 0009 Phase 1 起） | `bba66be..9ba859f` 的前段 |
| 2 | `d143489` | dev-sync 身份/写入/持闸与设备吊销（ADR 0009 Phase 1 + 0010） | 其余 Phase 1 + `f302d23..5e7545a` |
| 3 | `972a083` | 同步创建/加入向导与发布基线（ADR 0011） | `2934c97..debe0af` |
| 4 | `184a8dd` | 两阶段自动同步与持闸（ADR 0012） | `be852fd..11b8518` |
| 5 | `3d7e7d1` | 轮换库/机制与设备面板（ADR 0009 Phase 2） | `38ca276..6c31213` |
| 6 | `2124d6a` | 生产 relay 模式/ControlPin/远端初始化（ADR 0013） | `d6ae469..2091806` |
| 7 | `382ff41` | 附件插入与资产增量同步 | `3d1fb4f..5be12d2` |
| 8 | `ae3b136` | 多 vault/创建链接/邀请与 v2 数据面（ADR 0014） | `3e7cc7c..8a9926a` |
| 9 | `2b606f9` | ManagementCapV2 wire/schema（ADR 0015） | `5aa483d..39eb92e` |
| 10 | `53f1b15` | CapV2 控制链/设备生命周期（ADR 0015 S4b） | `5dcac32..7ee07bd` |
| 11 | `b32a5b2` | CapV2 轮换/恢复与双签客户端（ADR 0015/0016） | `a9e4029..2cc17ab` |
| 12 | 当前 final | 内嵌 Web 管理/claim/在线删除，移除旧管理入口（ADR 0017/0018） | `33d0b47..d9015c9` |

## 文档引用规则

- **当前状态/ADR 的实施锚点**已改为上表的新提交（或直接写主题，不再绑定哈希）。
- `docs/notes/commit-attribution.md`、`docs/notes/history-rewrite-20261113.md`、
  `docs/briefs/0005`/`0007`、`docs/reviews/0002`/`0003` 等是**历史记录**，
  其中哈希属于整理前历史，仅用于追溯；对象仍可由
  `backup/dev-before-cleanup-20261116` / 更早 backup 分支访问。
- 不要在 `dev` 上用旧哈希作为“当前实现”引用；需要 diff 时用 backup 分支。

## 验证

- `git diff d9015c9..<cleanup-final>`：仅 `AGENTS.md` 状态行（整理后补）。
- 整理后测试：relay 356 / sync 605+1 / app dev-sync 367 / 默认 134，
  fmt + clippy 全绿。
