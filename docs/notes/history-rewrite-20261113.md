# v0.3→v0.4 历史整理记录（2026-11-13）

> 历史文档：以下 commit 哈希属于 2026-11-16 整理前历史；映射与 backup 分支见 [history-rewrite-20261116](../notes/history-rewrite-20261116.md)。


## 为什么整理

`v0.3-search..v0.4-sync` 之间积累了 **185 个提交**：58 feat / 59 fix / 40 docs /
18 test / 4 chore / 2 perf / 2 refactor / 1 revert。其中大量是同一阶段的迭代修正、
ADR 追加与 clippy 修补，对后续开发的历史阅读价值低。按用户要求，在不丢失任何
文件内容的前提下，于新分支重建为 **12 个语义提交**，随后把原 `dev` 改为备份分支。

## 方法（可复核）

- 以 `v0.3-search`（`feceac5`）为父，按「阶段终点」取**原提交的 tree** 直接
  `git commit-tree` 重建；**不重放 diff**，因此没有冲突/冲突解决引入的偏差。
- 最终新 `dev`（`61d8de4`）的 tree 与整理前 `dev`（`3c69f16`）**逐字节一致**：
  重建时 `git diff dev clean/v0.4` 为空；每个新提交的 tree 等于其对应旧边界提交的 tree。
- 未使用 `--amend` 修改任何既有提交；旧历史完整保留在 `backup/dev-20261113`。

## 分支与 tag

| 名称 | 指向 | 说明 |
| --- | --- | --- |
| `dev` | `61d8de4` | 整理后的新历史（12 个提交），后续开发在此 |
| `backup/dev-20261113` | `3c69f16` | 整理前的完整历史（185 个提交） |
| `v0.4-sync`（annotated） | `738f3f0` | **未改动**，仍指向整理前历史，经 backup 分支可达 |

回滚方式：`git checkout backup/dev-20261113`，或
`git branch -f dev backup/dev-20261113` 后重新检出（会丢失整理后的 dev 指针，谨慎）。

## 旧 → 新对照（阶段终点）

| 新提交 | 旧阶段终点 | 内容 |
| --- | --- | --- |
| `fbb1fe9` | `1678eb6` | S6 批量物化 |
| `77d6ec4` | `62ab09c` | ADR 0006 + MNE1/CDE wire 冻结 |
| `72eacdb` | `d5b36af` | S7-1..5 密钥/身份/storage 桥/relay/engine |
| `9218a0c` | `4ebf94d` | 控制面/genesis/localhost e2e |
| `51d2d69` | `8d4b7bf` | 槽/checkpoint/bootstrap/恢复码自批 |
| `e928699` | `a43c372` | 轮换安全评审 0001 + journal A–C |
| `8af241a` | `cd3bbca` | 轮换 D–H + 单设备 activate |
| `fd61298` | `135a44c` | P2 重包 + P3 apply/restart + 双设备 e2e |
| `92fd921` | `d9b1636` | 评审 0002 生产阻断 + 安全矩阵 |
| `73526c7` | `35757ce` | 评审 0003 修复 + 交接文档 |
| `2c88f72` | `9aa07f0` | v0.4 三阻断（ASSET 认证/覆盖实证/post-activation/Linux 封装） |
| `61d8de4` | `3c69f16` | 自包含 e2e + v0.4 交接/放行 |

## 注意

- 仓库文档里仍可能有整理前的 7 位哈希（如 `0b30b2e`、`2c93f6e`、`9bfccb8`）。
  这些对象仍存在，但只在 `backup/dev-20261113` 历史中可达；需要查证时先 `git show backup/dev-20261113:<path>` 或在 backup 分支上 `git log`。
- 新提交的 message 末尾保留了「整理前阶段终点」哈希，便于追溯到旧历史。
- 新历史中的提交时间沿用各阶段终点的原始 author date，便于按时间阅读。
- 本次整理不改工作树内容、不改 tag、不改任何文件；唯一新增是本文档与
  `AGENTS.md`/交接文档里的指引。
