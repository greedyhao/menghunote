# 同步评审：入口

> **1M 上下文请用 [`sync-review-1m.md`](sync-review-1m.md)**：
> 约 286.5 万字符 / 80–100 万 token，代码截止 `46b214e`；核心实现最前，含完整 sync
> 核心模块、storage/测试/文档/完整 diff；已包含 review 后的 marker 门禁与错误可读修复。

> **272K 上下文请直接用 [`sync-review-272k.md`](sync-review-272k.md)**：
> 约 61.6 万字符 / 约 18–23 万 token，代码截止 `46b214e`；包含 P1 修复、关键测试、
> 完整 diff 和提示词，单文件上传即可。

> 下面的 **6 片是 review 前（代码 `1bededa`）的历史完整版**，不含 marker 门禁/
> 错误可读修复；除非需要旧快照或更大上下文，否则不要用于当前评审。

---

# 同步评审：分片版（不要上传单个 full 文件）

平台在约 **72 万字符**处截断；原 3.1MB 单文件只能读到 docs/01–13 + code/014–028，
`engine.rs` 之后的核心实现（`baseline_apply.rs`、`session/flush.rs`、`body_doc.rs`、
`store.rs`、`sync_meta.rs` 等）全部不可见。

因此改成 **6 个分片**。每片约 29–54 万字符，均低于截断点；请**逐片上传、逐片确认**，
全部到齐后再让模型评审。**不要上传原单文件**（已从工作树移除，git 历史仍可查）。

## 上传顺序

| 顺序 | 文件 | 字符数 | section 数 | 内容 |
|---|---|---|---|---|
| 1 | `sync-chat-review-part1-core.md` | 502,846 | 17 | 任务 1–4 状态 + 核心 storage/engine/bootstrap 代码 + 任务 1/2 测试 |
| 2 | `sync-chat-review-part2-tests-storage.md` | 537,790 | 20 | 其余 storage 测试 + session gate/core/schedule |
| 3 | `sync-chat-review-part3-proto-docs.md` | 417,753 | 21 | sync-proto + 其余文档/ADR/brief |
| 4 | `sync-chat-review-part4-sync-a.md` | 498,066 | 17 | sync 源码 A |
| 5 | `sync-chat-review-part5-sync-b.md` | 515,796 | 22 | sync 源码 B |
| 6 | `sync-chat-review-part6-sync-c-diff.md` | 295,394 | 3 | sync 源码尾部 + 完整 diff + 最终评审提示词 |

总计 100 个 section，覆盖全部原内容，无删减。

## 逐片上传方法

每次只上传一个文件，并在同一条消息里说：

> 这是 Part N/6。请只确认：`PART N/6 RECEIVED`、本片 `@@SECTION` 数量、
> 最后一个完整文件。不要开始评审。

确认后再上传下一片。不要一次上传 6 个附件——平台可能合并后再次截断。

## 全部到齐后的最终提示词

Part 6 末尾已附；也可以复制：

```text
你现在已收到 Part 1–6。请只依据这些 Markdown 评审，不要假设能执行 cargo 或访问仓库。
先确认看到 PART 1/6 … PART 6/6，再输出：
1. 任务 1–4 每项“完成 / 部分 / 不能判断”，引用文件/函数/测试；
2. 必须修正（按严重度）；
3. 不能判断/需补材料；
4. 未关风险，以及是否允许进入 Phase 1B。

硬性检查：integrated >= S 前不得删 inbox/skipped_asset；seq=head+1；
received/integrated 连续前缀；remote/adopt 不产生 outbox；无第二写连接、网络不持闸。
特别审查 P1-3 的“兼容 head 才重试，未知外部 ops fail-closed 重开”是否可接受；
任务 3 的 meta 失败标记没有在 finish/apply_batch 强制是否阻断 Phase 1B。
不要重复贴大段代码，不要假设 Phase 1B 已实现。
```

## 关键结论（给模型/你快速定位）

- 代码截止 `1bededa`（文档提交 `7a523bc`，分片索引另提交）。
- 任务 1：flush 后台有界重建，成功发 DurableAck；未知外部 ops fail-closed 要求重开。
- 任务 2：11 个 detached/attached 等价测试，比较文本/树形状/VV。
- 任务 3：verify 失败不回滚 merged Loro，meta 标记 `sync.baseline_verify_failed`；
          `finish`/`apply_baseline_batch` 尚未强制检查，Phase 1B 必须显式门禁。
- 任务 4：S=1k/5k/25k 探针，25k pages=98、scan_ms≈4.0；FakeRelay 页内扫描是夹具 artifact。
- 任务 5：未做（可选 P2）。
