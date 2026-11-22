# 同步评审精简稿：272K 上下文专用

> **目标**：给 272K token 上下文的 chat 模型一份可完整读完的评审稿。
> 只保留 P1-3、detached oplog、verify 语义、checkpoint 扫描探针相关的
> **改动代码 + 直接依赖 + 关键测试 + 完整 diff**；不含未改动的 sync 全模块、
> 旧 ADR/brief、重复文档。
> 代码截止 `1bededa`；**Phase 1B 未实现**。
>
> 本文件约 568,607 字符。按混合内容保守估计 **18–22 万 token**，给 272K 留出
> 提示词/回答余量。若平台仍报超限：先删“附录 C 完整 diff”（约 25 万字符），
> 或改用分片版 `sync-chat-review-part1-core.md`。

## 提示词（复制给模型）

你是 Rust / Loro / SQLite 评审者。只依据本 Markdown 评审；不要假设能运行 cargo、
访问仓库或调用工具。先确认是否读到“附录 C 完整 diff”的末尾；若没有，指出最后
看到的文件/函数。

按任务审：

1. **P1-3 生产 flush**：`session/flush.rs` 的 HeadConflict 是否在同后台线程有界重建
   sink 并重试；成功是否发 DurableAck；失败是否可读地进入“需要重开文档”，不无限写；
   是否仍是单 writer、无第二 journal、不阻塞 UI。特别注意：只有新 head 的 ops ⊆
   未 ACK 前缀 `end_vv` 时才重试同一字节；未知外部 ops 时 fail-closed 要求重开。
2. **detached oplog**：`body_doc_detached_oplog.rs` 的 11 个用例是否真的覆盖两 peer、
   LoroTree create/move/delete、并发本地编辑、空/单字符/emoji/组合字符；是否比较文本/
   树形状，而不是只比 frontier；detached 与 attached 是否等价。
3. **verify 失败语义**：失败是否不回滚 merged Loro；`sync.baseline_verify_failed`
   meta 标记、下一次 begin 必须 abort 的契约是否成立；是否用 `actual_vv ⊆ local_vv`
   而非相等；`verify_ms` 是否只读；schema 不能新增 failed 的冲突是否处理诚实。
4. **checkpoint 扫描**：`checkpoint_validate_scan_cost` 是否走真实 prepare→validate；
   S=1k/5k/25k 的 pages/scan_ms 是否可复现；是否没改 `scan_coverage_log`。
5. **未做项**：幂等重放只读预检未做（可选 P2），是否影响正确性/仅性能。

硬性检查逐条回答：
- `integrated >= S` 前是否可能删 inbox / skipped_asset；
- 是否违反 `loro_updates.seq = head_seq + 1`；
- 是否违反 received/integrated 连续前缀；
- remote/adopt 是否可能产生 outbox；
- 是否新增第二写连接、网络阶段是否持闸。

输出：
1. 任务 1–4 每项“完成 / 部分 / 不能判断”，引用文件/函数/测试；
2. 必须修正（按严重度）；
3. 不能判断/需补材料；
4. 未关风险，以及是否允许进入 Phase 1B。
不要重复贴大段代码；不要假设 Phase 1B 已实现。

---

## 0. 本轮状态与证据

| 任务 | 状态 | 证据 |
|---|---|---|
| 1 HeadConflict 生产接线 | 完成（功能） | `flush_recovers_from_head_conflict_by_rebuilding_sink`、`flush_reports_unrecoverable_after_bounded_rebuild`、`flush_reports_unrecoverable_when_new_head_is_unreadable` |
| 2 detached oplog 矩阵 | 完成 | `body_doc_detached_oplog` 11 passed（无 ignore） |
| 3 verify 失败语义 | 完成（schema 限制用 meta） | `verify_accepts_concurrent_local_edits_after_export`、`verify_failure_keeps_merged_data_and_marks_journal_failed` |
| 4 覆盖扫描成本 | 完成（探针） | `checkpoint_validate_scan_cost`（ignored） |
| 5 幂等只读预检 | 未做（可选 P2） | 不影响 P0/P1 正确性 |

```text
storage 288 passed / 0 failed / 1 ignored (manual bench)
sync 636 passed / 0 failed / 4 ignored (manual/probes)
app 740 / dev-sync 891
workspace 2900 / 0；workspace dev-sync 3051 / 0
release phase1a 2 passed (baseline + scan)
fmt / clippy workspace -D warnings 全绿
```

**P1-3 残余**：自动重建只在兼容 head 时可重试同一字节前缀；未知外部 ops 时 fail-closed
要求重开；真正跨会话重导出 rebase 未做。`session/flush.rs` 仍直接 `VaultJournalSink`
（不是 `JournalWriter`），无第二 journal。

**任务 4 实测**：S=1k pages=4 median scan_ms≈0.076；5k pages=20≈0.428；
25k pages=98≈4.039；FakeRelay 页内 `log()` 扫描是夹具 artifact，真实成本主要是
98 次串行 RTT；prepare+validate 最多 2×S 事件访问。

---

# 附录 A：关键文档

## `docs/notes/sync-hardening-20261121.md`

`````markdown
# 同步硬化修复记录（2026-11-21）

基线：`f8b02e1`（代码基线 `5d01e9e`）。本轮只做 Phase 1A 收尾硬化；
**未改 relay / sync-proto wire / SQLite DDL / checkpoint 签名 / 游标连续语义**。

## 1. P1 状态

| 项 | 状态 | 提交 | 测试 |
| --- | --- | --- | --- |
| P1-1 finish 完整性 | 完成 | `2ed485b` | `finish_without_all_docs_is_conflict`、`finish_after_full_batches_is_applied`、`begin_idempotent_replay_keeps_expected_set`、`finish_without_all_assets_is_conflict` |
| P1-2 tombstone TOCTOU | 完成（SQL 事务内重判） | `2ed485b` | `tombstone_outbox_race_between_decision_and_transaction_keeps_note` |
| P1-3 Journal HeadConflict | **部分**：JournalWriter 契约/重建入口完成；生产 `session/flush.rs` 仍直接调 sink，尚未接 `JournalHeadStale` 的自动重建 | `aba4375` | `stale_head_is_terminal_then_rebuild_hands_back_entries_and_new_entries_flush`、`rebuild_sink_is_rejected_while_not_stale` |
| P1-4 ready/count log_id | 完成 | `be5244b` | `ready_and_count_only_include_current_log_incarnation`、`ready_where_path_filters_other_log_incarnation`、`ready_without_log_id_is_empty_and_invalid_length_is_corrupt` |

### P1-1 细节

- `begin_baseline_apply` 增加 `BaselineExpectation { docs, assets }`，与 journal 同事务写 `sync.baseline_expected`（本地 `meta`，非 DDL）。
- `finish` 在任何 meta 写入前逐项比对 `baseline_apply_docs.applied=1` / `baseline_apply_assets.applied=1`；缺失/多出/集合不符 → `Conflict`，零写。
- `finishable` 改为 `staging && expected 全齐`；无记录不可 finish。
- 同参数 begin 幂等；同 checkpoint 换集合 → `SyncConflict`。
- 旧 journal 无 expected 记录 → `Conflict`（拿不到证明不推进 S）。

### P1-2 细节

- 事务外仍做 VV/outbox 预判；事务内 `baseline_apply_tombstones` 用 `CASE WHEN EXISTS(sync_outbox...)` 重判；`notes` 删除只命中 `action='deleted'` 且无 outbox 的行。
- 安全方向单一：事务内只能把 deleted 降级为 kept_local_edit，不能反向升级；判定/写入之间无窗口（A 事务 BEGIN IMMEDIATE 串行化）。
- 失败模式：判定后插入本地编辑 → notes.deleted 保持 0，tombstone 记 kept，`kept_local_edit` 计数；不会整批回滚昂贵 merge。

## 2. P2 状态

| 项 | 状态 | 提交 | 测试 |
| --- | --- | --- | --- |
| P2-5 remote_known 单调性 | 完成（值 CAS） | `800cc47` | `concurrent_writers_from_same_old_converge_to_union`、`stale_remote_known_cas_fails_closed_and_rolls_back`、`begin_backfills_remote_known_*` |
| P2-6 meta 读取语义 | 完成（BLOB-only，非 BLOB `Corrupt`） | `2019490` | `text_meta_is_corrupt_from_all_three_entry_points`、`blob_meta_reads_consistently_from_all_three_entry_points`、`missing_meta_reads_as_absent_from_all_three_entry_points` |
| P2-7 exec_transaction Immediate | 完成（共享 `begin_immediate`） | `2019490` | `exec_transaction_under_foreign_write_lock_fails_busy_without_partial_write`、`exec_transaction_takes_write_lock_at_begin` |
| P2-8 空切片语义 | 完成（显式 `OriginMarks`） | `800cc47` | `all_local_origin_does_not_extend_remote_known`、`per_record_origin_only_extends_for_remote_records`、`per_record_origin_length_mismatch_is_rejected` |
| P2-9 资产本地损坏 | 完成（LocalCorrupt 上抛，健康资产继续） | `4960a47` | `corrupt_local_asset_raises_instead_of_counting_retryable_failure`、`corrupt_asset_does_not_block_healthy_upload_but_still_raises` |

### P2-5 细节

- `remote_known::cas_statement`：`expected=Some(old)` 用 `ON CONFLICT ... WHERE meta.value=?old`；`expected=None` 用 `INSERT ... WHERE NOT EXISTS`；均配 `AffectedGuard::exactly(1)`，CAS 不命中整笔回滚。
- `append_updates_inner` 对远端 CAS 最多重试 4 次（重读旧值、重算并集、替换语句）；head CAS 仍在最前，head 真被推进仍 `HeadConflict`；耗尽返回带 context 的 `SyncConflict`。
- `baseline_apply` 批次内 remote_known 写入同 CAS；冲突整批回滚，由调用方重跑幂等批次（不内层重试，避免重建大 snapshot）。
- `begin` 回填改为无条件执行，覆盖 completed journal 与“无 journal 但有残留 docs”的状态；`ON CONFLICT DO NOTHING` 保证不降级。
- `remote_known::load` 收紧为 BLOB-only（NULL/INTEGER/TEXT → Corrupt），对齐 P2-6。

### P2-6/P2-7 细节

- `sync_meta::meta_bytes_of` 是唯一判定；`Vault::get_meta`、`sync_bridge::read_meta`、`sync_meta::read_meta_on` 共用。
- 非 BLOB 的 `meta.value` 不再静默当缺失；`sync.repack_required`/`sync.relay_cursor`/`sync.log_id` 损坏会显式失败。
- `begin_immediate(conn)` 是唯一事务开启点；`exec_inbox_batch` 与 `exec_transaction` 共用，BUSY 语义一致。

## 3. P3 状态

P3 未做（本轮优先 P1/P2，且“不要挤占 P1/P2”）：

- `PendingJournal::peek_prefix` 拷贝优化；
- `PendingJournal::new(0)` 文档/行为一致；
- pull 每页 ASSET 一次前缀推进；
- `PendingInboxRow` 缓存 base/end VV、预检/执行共用纯函数；
- `push` 查询顺序；
- 补 journal 混合 Local/Remote 单测。

## 4. 不变量 → 测试映射

| 不变量 | 证明 |
| --- | --- |
| `loro_updates.seq = head_seq+1`，不得写 sender_seq | 既有 `aggregate_body_*`/`out_of_order_*` + P1-1 finish 后水位；本轮未改 seq 分配 |
| `received/integrated` 只连续推进 | 既有 `sync_meta` 窗口测试；P1-4 log 过滤 + P1-1 finish 完整性 |
| `integrated >= S` 前不删 inbox / skipped_asset | P1-1 finish 完整性测试；P1-2 只软删 notes、tombstone 记 kept；未新增 prune |
| epoch/rotation/log 异常 → Paused/错误 | 本轮未改 rotation/epoch 代码；P1-4 log 过滤避免跨代 ready |
| HeadConflict 有界、不覆盖 head/丢数据 | P1-3 JournalHeadStale 终态 + rebuild 测试；P2-5 值 CAS 回滚 |
| remote/adopt 不产生 outbox | P2-8 `remote_only_flush_with_empty_slots_has_no_outbox` + 既有 engine 测试 |
| 不改 wire/relay/schema | `git diff` 无 relay/sync-proto/schema/migration；仅本地 `meta` 键 |

## 5. 验证

代码基线 `2ed485b` + P2 提交后的实际结果：

```text
cargo fmt --all -- --check                          EXIT 0
cargo clippy --workspace --all-targets \
  --features menghunote-app/dev-sync -- -D warnings EXIT 0
cargo test -p menghunote-storage                    274 passed / 0 failed (1 ignored bench)
cargo test -p menghunote-sync                       634 passed / 0 failed (3 ignored manual)
cargo test -p menghunote-app                        740 passed / 0 failed
cargo test -p menghunote-app --features dev-sync    891 passed / 0 failed
cargo test --workspace                             2884 passed / 0 failed
cargo test --workspace --features menghunote-app/dev-sync 3035 passed / 0 failed
```

一次环境假失败：共享 `target` 中旧 `gpui-excalidraw` build script 烘入了已删除的 `/home/dev/menghunote-p2check`；`cargo clean -p gpui-excalidraw` 后 app/workspace 全绿，与本轮改动无关。

## 6. 需要用户决策 / 残余

1. **P1-3 生产接线**：`session/flush.rs` 仍直接使用 `VaultJournalSink`，没有使用 `JournalWriter` 的新重建契约；当前会把 `HeadConflict` 记入 `last_write_error`（不静默），但不会自动重建。是否把 session flusher 接到 `JournalHeadStale`/rebuild（需要跨 gate 状态机）需用户决定；本轮未改该文件。
2. **P2-5 baseline 冲突策略**：批次 CAS 冲突返回 `SyncConflict`，由调用方重跑整批；是否允许内层重建/重试需决策。
3. **P2-6 兼容性**：非 BLOB `meta` 现在显式 `Corrupt`；旧库若有外部写入的 TEXT 键会拒绝同步（按“屏障键不能静默绕过”取舍）。
4. **P2-9 行为**：本地资产损坏会中止该轮 sync（在 pull/integrate 前），修复后继续；这是 fail-closed，不是网络重试。
5. **P3**：未做，避免挤占 P1/P2。

## 7. 确认

本轮没有修改 relay 服务端、`sync-proto` wire/schema、SQLite DDL/迁移、checkpoint 签名或游标连续语义。

## 8. P1：verify_imported 失败语义（2026-11-21 补）

基线：`7a1a8e9`（在 §1/§2 的 P1/P2 修复之上）。只改 `storage::baseline_apply` 与
`sync::bootstrap` 的本地语义；**没有** relay / sync-proto wire / DDL / 迁移 / checkpoint /
游标连续推进改动，也**没有**实现 Phase1B。

### 8.1 失败不回滚已 merge 的 Loro 数据

`bootstrap::verify_imported` 是导入后的**读回校验**，不是数据库事务。它失败时：

1. **不回滚已 merge 的 Loro/已写入资产。** 导入走 `apply_baseline_batch` 的 CRDT merge
   （`BodyDoc::restore` + `import`，幂等）；本地编辑的 op 可能已经与基线 ops 合进同一条
   `loro_updates` 行，无法只回滚"远端部分"。删数据回滚会丢用户本地编辑，因此失败路径
   **只标记、不清数据**。
2. **把 baseline journal 置为不可继续。** 由于 `status` 不能新增 `failed`（见 §8.6），
   失败标记写在本地 `meta` 键 `sync.baseline_verify_failed`（BLOB：
   `MNBASEVERIFYFAIL1` + UTF-8 原因），journal 保持 `staging`，docs/assets 子表与已导入
   数据原样保留。
3. **不推进任何水位。** verify 在 `finalize` 之前失败，`received_seq` / `integrated_seq` /
   `confirmed_highwater` / relay cursor 一个都不写（测试直接断言）。

**契约**（`crates/storage/src/baseline_apply.rs`）：

- `mark_baseline_verify_failed(vault, reason)`：只写上述 `meta`，不改 `status`；
- `begin_baseline_apply` 读到标记即返回 `SyncConflict`（同参数幂等续跑也拒绝），错误文本
  明确要求先 `abort_baseline_apply`；
- `abort_baseline_apply` 在清 journal 的同一事务里删除标记（没有 staging journal 时也会
  清残留标记），已导入的 Loro/资产仍保留；abort 之后才能重新 begin。
- 读取用 `baseline_verify_failure(vault)`：无标记返回 `None`，标记损坏（缺魔数/非 UTF-8）
  返回 `StorageError::Corrupt`，不静默当成"没有失败"。

### 8.2 闸外编辑与 verify 不互斥；用 ⊆ 而不是相等

verify 不持闸、也不阻止编辑；判定方向固定为：

```text
snapshot.actual_vv ⊆ 当前本地 VV     // sub_iter 为空即通过；不是相等
```

导出点之后本地新增的 op 是合法编辑，不能因为 VV 不相等判失败。正文**只在**
`local_vv ⊆ actual_vv`（本地没有快照之外的 op）时才逐字节比较；有本地 extra 时 Loro
合并后的正文必然不同于快照，比较只会误报。实现提取为纯函数 `check_imported_doc`
（`crates/sync/src/bootstrap.rs`），`verify_imported` 与单测共用同一份判定，避免测试另写
一套语义。

### 8.3 verify_ms 只读指标

- `BaselineApplyDebugTimings.verify_ns`：线程本地累计，不持久化、不参与决策；
- `baseline_apply_verify_ms()`：上述值换算成毫秒（向下取整）；
- `record_baseline_verify_time(Duration)`：由 `bootstrap::apply_selected` 在
  `verify_imported` 前后用 `Instant` 计时后记录（失败也记录）；
- **只读**：失败与否只看 `verify_imported` 的返回值，指标不参与任何判断；
- Phase1B 的 gate_hold 报告可在同一线程读 `baseline_apply_debug_timings().verify_ns`
  或 `baseline_apply_verify_ms()`。注意它和现有分段计时一样是**线程本地**，跨线程不聚合；
  `crates/sync/tests/phase1a_release.rs` 现有的本地 `verify_ms` 计时与此指标相互独立。

### 8.4 代码位置

| 文件 | 改动 |
| --- | --- |
| `crates/storage/src/baseline_apply.rs` | 新增 `META_BASELINE_VERIFY_FAILED` / `mark_baseline_verify_failed` / `baseline_verify_failure`；`begin_baseline_apply` 拒绝失败标记；`abort_baseline_apply` 清标记；`BaselineApplyDebugTimings.verify_ns` + `record_baseline_verify_time` + `baseline_apply_verify_ms` |
| `crates/storage/src/lib.rs` | 导出上述新 API |
| `crates/sync/src/bootstrap.rs` | `apply_selected` 在 verify 失败时写标记、计时；提取 `check_imported_doc`；新增 `BootstrapFault::VerifyVvNotCovered`（doc-hidden 测试注入点） |
| `crates/sync/tests/bootstrap.rs` | `verify_failure_keeps_merged_data_and_marks_journal_failed` |

### 8.5 测试

| 测试 | 证明 |
| --- | --- |
| `verify_accepts_concurrent_local_edits_after_export`（`crates/sync/src/bootstrap.rs` 单测） | 导出点 VV ⊆ 当前 VV（含并发本地 op）时通过、不要求相等；无本地 extra 时正文不同才失败；本地缺导出点 op 时失败（方向不可反） |
| `verify_failure_keeps_merged_data_and_marks_journal_failed`（`crates/sync/tests/bootstrap.rs` 集成） | 注入 VV 未覆盖失败后：已 merge 的本地+基线正文仍在、journal 仍 `staging`、失败标记带原因、水位零推进；未 abort 前 begin 拒绝并提示 `abort_baseline_apply`；abort 清 journal+标记但保留 merged Loro；随后重跑成功 |

验证结果见本节末（实际命令输出；与其它并行改动的边界见 §8.6）。

### 8.6 与代码/schema 的冲突（如实记录，未假装修复）

1. **`baseline_apply.status` CHECK 冲突**：`crates/storage/src/schema/v3.sql` 只允许
   `staging|completed`，不能落 `failed`。本任务硬边界禁止 DDL/迁移，因此失败语义放在
   `meta`，**journal 仍显示 `staging`**。后果：`BaselineApplyState.status` 无法单独表达
   "校验失败"，必须另读 `baseline_verify_failure`；UI/续跑逻辑不能把 `staging` 当成
   "可续跑"。这个不一致是记录下来的冲突，没有通过改 schema 绕过。
2. **`finishable` 仍可能为 true**：`expected` 集合全齐时 `BaselineApplyState.finishable`
   为 true，即使失败标记存在。`finishable` 只表示 `finish` 的完整性前提，不是"可继续"
   判据；新增门禁只在 `begin_baseline_apply`。`apply_baseline_batch` /
   `finish_baseline_apply` **未**额外检查标记（最小改动；bootstrap 在 verify 失败后不会再
   调用它们）。若 Phase1B 需要更强保证，应在这两个入口也拒绝——本轮未做。
3. **bootstrap 的换基线自动清理**：`reconcile_journal` 在"选择了另一份基线"时会对既有
   staging 调用 `abort_baseline_apply`（从而清掉失败标记）。这是既有的清理入口，不是绕过
   `begin` 门禁；同一基线的续跑仍然必须先显式 abort。
4. **verify 不持闸**：正文比较基于同一次 `load_body_doc` 读到的 body+head，二者是同一份
   已提交状态；这保证"读到的本地状态"与快照可比，但**不**证明 verify 期间没有本地写入
   （本来就不互斥）。并发编辑只会增加 ops，⊆ 判定保持成立。
5. **标记写失败时的行为**：`apply_selected` 返回标记写入的 storage 错误（不让原始 verify
   错误静默通过）；此时 journal 仍是可续跑的 staging（与旧行为一致），没有失败标记。这是
   fail-open 到"重试"的窄窗口，记录在此。
6. **标记的触发面是"verify_imported 返回任何错误"**：除校验结论不符（`SyncError::Object`）
   外，读回时的本地存储错误也会置标记。取舍是 fail-closed：宁可多一次 abort 后重跑
   （数据不丢、merge 幂等），也不把状态不明的失败当成"可续跑"。若将来要区分 transient
   读错误，应在 `apply_selected` 里按错误类别收窄，本轮未做。

### 8.7 验证（实际执行结果）

工作树包含并行的 P1-3（`session/flush.rs`）改动，因此下面 storage 总数高于本轮前的
274；本轮新增的 storage 测试为零，新增 sync 测试为 2 个（单测 + 集成各 1）。

```text
cargo fmt --all -- --check                                         EXIT 0
cargo test -p menghunote-storage                                   288 passed / 0 failed / 1 ignored
cargo test -p menghunote-sync                                      636 passed / 0 failed / 4 ignored
cargo clippy -p menghunote-storage -p menghunote-sync \
  --all-targets -- -D warnings                                     EXIT 0
```

新增的两个测试都实际跑过（非 ignore）：

```text
cargo test -p menghunote-sync --lib verify_accepts_concurrent_local_edits_after_export
  -> 1 passed
cargo test -p menghunote-sync --test bootstrap \
  verify_failure_keeps_merged_data_and_marks_journal_failed
  -> 1 passed
```


### 8.8 未做

- 未新增/修改任何 DDL、迁移、`baseline_apply.status` 取值；
- 未改 relay、`sync-proto` wire/schema、checkpoint 签名/验证、游标连续推进；
- 未实现 Phase1B（gate_hold 报告、更强的批次/finish 门禁等）；
- 未 commit。


`````

## `docs/notes/sync-phase0-metrics.md` §8.7

`````markdown
### 8.7 `checkpoint validate` 覆盖日志扫描（S = 1k / 5k / 25k，2026-11-21）

> 任务 4 P1 探针：`crates/sync/tests/phase1a_release.rs::checkpoint_validate_scan_cost`
> （`#[ignore = "manual benchmark"]`）。只加基准与文档：**不改**生产
> `scan_coverage_log`，不改 relay / `sync-proto` / schema，未 commit。

#### 8.7.1 目的与口径

`CheckpointWriter::prepare` 在 coverage 有缺口时会把 pending receipt 落盘，
`validate` 的 `ensure_coverage_proven` 再从持久游标沿 relay 日志**连续扫到 S**
（每页 256 条，`COVERAGE_SCAN_MAX_PAGES = 4096`）。本探针只量这段**真实扫描**随 S 的
墙钟与页数：不复制 scan 实现，而是在测试侧用 `CountingRelay` 包装 FakeRelay，
统计真实 `scan_coverage_log` 发起的 `RelayClient::log` 调用（validate 仍走生产
`CheckpointWriter::validate`）。

| 指标 | 口径 |
| --- | --- |
| `S` | relay 日志事件总数 = checkpoint 的 `covered_seq` |
| `pages` | validate 期间真实 `log()` 调用次数（从持久游标 `c0` 扫到 S，应 = `ceil(S/256)`） |
| `scan_ms` | 第一次 `log()` 开始到最后一次 `log()` 返回的墙钟跨度；含页间逐事件核对（visitor），**不含**最后一页返回之后的 visitor（≤256 条） |
| `log_ms` | 仅 `log()` 内部（FakeRelay 取页 + 克隆事件）的墙钟 |
| `visitor ≈` | `scan_ms - log_ms`：逐事件序号/证据核对开销 |
| `validate_ms` | 整个 `validate` 墙钟（含 manifest/快照读回导入、尾部 ASSET 下载实证、coverage scan） |
| `prepare_*` | prepare 阶段 `try_record_coverage_receipt` 的同口径扫描（单次、含首扫冷启动，不是中位数） |

#### 8.7.2 夹具与测量方式（FakeRelay + 真 SQLite）

- 每个 S 新建 fresh vault（`CARGO_TARGET_TMPDIR` 下真 SQLite）+ `ensure_structure_row`
  + 1 篇小正文（`prepare` 需要可导出的本地快照/head VV，与 coverage 无关）；
  `RELAY_CURSOR_META = "c0"`、`sync.log_id` 与 FakeRelay 一致。
- relay 日志合成 `1..=S` 条：
  - `1..=S-1`：`seq % 7 == 0` 的发布为 ASSET（`publish_event` 的 dummy 事件），
    其余为 UPDATE（`publish_event`）；这一段都在本地 **integrated 连续前缀**内
    （直接写 `sync.integrated_seq = S-1`，合成值）。
  - 第 S 条是 `publish_real_asset` 发出的**真实 ASSET**（1 part、可解密、
    明文 `coverage scan cost tail asset`）。
  - 整体 ASSET 占比 **14.3%**（143 / 715 / 3 572 条），落在 10–20% 要求内。
- 因此 `local_coverage` 把 `1..=S-1` 视为已消费前缀，唯一缺口是第 S 条 ASSET：
  `prepare` 会扫一遍日志并落 pending receipt；`validate` 再从 `c0` 扫一遍、
  逐条核对 receipt 候选，并对尾部 ASSET 真实下载/解密/AEAD 验证。
- 每档同一夹具连续 `validate` 3 次（`prepare` 只做一次；每次 validate 前清零计数器），
  取中位数；一次进程内 3 档串行（1k → 5k → 25k）。
- 硬断言（与时间无关）：`covered_seq == S`、`coverage_verified_candidates == 1`、
  `coverage_durable_assets == 0`、每次 validate 的 `pages == ceil(S/256)`。
  **只打印不 assert 时间。**

环境：Linux 6.8.0-149（Ubuntu），Intel Xeon Gold 6330 @ 2.00 GHz，8 vCPU，
15 GiB RAM，ext4（`/dev/sda1`，空闲 16 G）；`rustc`/`cargo` **1.100.0**，
`--release`（`opt-level=3`、`debug=0`、无 LTO）。测量时机器为共享状态
（其它 agent 的构建/测试同时跑），load average 约 **0.1–2.4**（8 vCPU），
所以绝对值只看量级/线性度（同一探针隔几分钟复跑 4 次，25k 的 `scan_ms` 中位数
3.796 / 3.830 / 3.895 / 3.965 ms，最大偏差 <5%）。

```bash
export LIBRARY_PATH=/home/dev/xkb-sysroot/root/usr/lib/x86_64-linux-gnu
export PKG_CONFIG_PATH=/home/dev/xkb-sysroot/root/usr/lib/x86_64-linux-gnu/pkgconfig

# 只跑本探针（首次无编译 1.18 s；探针本身约 0.6 s）
cargo test --release -p menghunote-sync --test phase1a_release \
  checkpoint_validate_scan_cost -- --ignored --nocapture

# 整个 ignored 文件（另有 baseline_apply 探针；本次 2 passed、测试 66.23 s，
# 含 storage/sync 增量重编译的整条命令 88.44 s）
cargo test --release -p menghunote-sync --test phase1a_release -- --ignored --nocapture
```

> 注意：`cargo test ... -- --ignored` 默认按 test harness 并行执行**两个** ignored 探针，
> `scan_ms` 会被另一探针的 CPU 竞争抬高（实测 1k：0.153 vs 单跑的 0.076 ms；
> 25k：3.878 vs 3.796 ms）。本节表格取**单独运行本探针**的数据。

#### 8.7.3 原始输出（`target/tmp/phase1a-logs/scan-cost-run.log`）

```text
[phase1a] scan_cost setup S=1000 asset_events=143 update_events=857 asset_pct=14.3 prepare_pages=4 prepare_scan_ms=0.114 prepare_log_ms=0.096
[phase1a] scan_cost S=1000 run=0 pages=4 scan_ms=0.083 validate_ms=0.710 log_ms=0.062 part_calls=4 part_ms=0.002
[phase1a] scan_cost S=1000 run=1 pages=4 scan_ms=0.076 validate_ms=0.542 log_ms=0.057 part_calls=4 part_ms=0.001
[phase1a] scan_cost S=1000 run=2 pages=4 scan_ms=0.074 validate_ms=0.514 log_ms=0.055 part_calls=4 part_ms=0.002
[phase1a] scan_cost S=1000 pages=4 scan_ms=0.076 (median of 3; raw_scan_ms=[0.08260899999999999, 0.076162, 0.073966] log_ms_median=0.057 validate_ms_median=0.542 validate_ms_raw=[0.710135, 0.542165, 0.514101] part_calls=4 part_ms_raw=[0.001698, 0.001321, 0.001602])
[phase1a] scan_cost setup S=5000 asset_events=715 update_events=4285 asset_pct=14.3 prepare_pages=20 prepare_scan_ms=0.450 prepare_log_ms=0.342
[phase1a] scan_cost S=5000 run=0 pages=20 scan_ms=0.479 validate_ms=0.919 log_ms=0.344 part_calls=4 part_ms=0.002
[phase1a] scan_cost S=5000 run=1 pages=20 scan_ms=0.440 validate_ms=0.823 log_ms=0.316 part_calls=4 part_ms=0.002
[phase1a] scan_cost S=5000 run=2 pages=20 scan_ms=0.617 validate_ms=1.003 log_ms=0.439 part_calls=4 part_ms=0.002
[phase1a] scan_cost S=5000 pages=20 scan_ms=0.479 (median of 3; raw_scan_ms=[0.478646, 0.440245, 0.617419] log_ms_median=0.344 validate_ms_median=0.919 validate_ms_raw=[0.918995, 0.823261, 1.003386] part_calls=4 part_ms_raw=[0.00223, 0.001847, 0.001587])
[phase1a] scan_cost setup S=25000 asset_events=3572 update_events=21428 asset_pct=14.3 prepare_pages=98 prepare_scan_ms=3.660 prepare_log_ms=3.141
[phase1a] scan_cost S=25000 run=0 pages=98 scan_ms=3.796 validate_ms=4.376 log_ms=3.177 part_calls=4 part_ms=0.003
[phase1a] scan_cost S=25000 run=1 pages=98 scan_ms=3.779 validate_ms=4.356 log_ms=3.183 part_calls=4 part_ms=0.002
[phase1a] scan_cost S=25000 run=2 pages=98 scan_ms=3.829 validate_ms=4.468 log_ms=3.197 part_calls=4 part_ms=0.002
[phase1a] scan_cost S=25000 pages=98 scan_ms=3.796 (median of 3; raw_scan_ms=[3.796107, 3.7794470000000002, 3.829243] log_ms_median=3.183 validate_ms_median=4.376 validate_ms_raw=[4.376248, 4.35638, 4.46814] part_calls=4 part_ms_raw=[0.002683, 0.001617, 0.001777])
```

#### 8.7.4 结果

| S | 事件（ASSET / UPDATE） | pages | `scan_ms` 中位数 | raw `scan_ms` | `log_ms` 中位数 | visitor ≈ | `validate_ms` 中位数 | prepare pages / `prepare_scan_ms` |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 1 000 | 143 / 857 | 4 | **0.076 ms** | 0.074–0.083 | 0.057 ms | 0.019 ms | **0.542 ms** | 4 / 0.114 ms |
| 5 000 | 715 / 4 285 | 20 | **0.479 ms** | 0.440–0.617 | 0.344 ms | 0.135 ms | **0.919 ms** | 20 / 0.450 ms |
| 25 000 | 3 572 / 21 428 | 98 | **3.796 ms** | 3.779–3.829 | 3.183 ms | 0.613 ms | **4.376 ms** | 98 / 3.660 ms |

读法：

1. **逐事件 visitor 是线性的、常数很小**：`scan_ms - log_ms` ≈ 0.019 / 0.135 / 0.613 ms，
   折合 **19–27 ns/事件**（25 000 条 ≈ 0.6 ms）。页数严格 = `ceil(S/256)`
   （4 / 20 / 98）。
2. **本探针的 `scan_ms` 主要来自 FakeRelay 取页，而不是生产 scan 的 visitor**：
   FakeRelay 的 `log()` 每页都从 `Vec` 头 `iter().filter(seq > cursor)` 再 `take(256)`，
   跳过的前缀随页号增长。25k / 98 页合计约 `256 × (0+1+…+97)` ≈ **1.22M 次跳过**，
   这就是 `log_ms` 3.18 ms 的主要来源（1k/5k 分别只有约 1.5k/49k 次）。
   真实 relay 用游标/索引取页，理想成本 O(page)，**本表的 `scan_ms` 是上界**，
   不能按它外推真实 relay 的扫描成本。
3. **生产口径下真正要紧的是页数 × 网络 RTT**：S = 25 000 时是 **98 次串行页请求**
   （每页 256 条）；visitor 本身只有约 0.6 ms。FakeRelay 无网络，所以本表只能说明
   “CPU/扫描逻辑不是瓶颈”，不能给出真实端到端延迟。
4. **prepare 也会扫一遍同一区间**（写 pending receipt）：25k 时 `prepare_scan_ms` 3.66 ms
   （单次），即一次 “prepare → validate” 最多访问 **2 × S** 条事件；
   若本地账簿已完整覆盖（`missing` 为空），`ensure_coverage_proven` 直接返回、
   不扫描（本探针未测这一档，按代码路径）。

#### 8.7.5 限制（不外推）

- **FakeRelay 不是真实 relay**：纯内存、无 TLS/网络/RTT/重试/真实 relay GC；
  `log()` 每页从 `Vec` 头线性 filter（见读法 2），使 `scan_ms` 含
  ≈`O(S²/256)` 的夹具 artifact。真实 relay 的游标分页成本未测。
- **合成日志**：85.7% 的 UPDATE 是不含真实负载的 `publish_event`；integrated 前缀是
  直接写 meta 的合成值（不是 engine 真实 pull/integrate 推进）；前缀内的 ASSET
  事件不下载验证——生产里它们应由 engine 的 durable skipped-asset 记录证明，
  本探针只借用生产代码信任的“连续前缀 = 已消费”不变量。只有尾部第 S 条 ASSET
  是真实密文并走完整 receipt + 下载/解密/AEAD 验证。
- **夹具是单文档小正文**：manifest/snapshot 固定成本（`validate_ms - scan_ms` ≈ 0.44–0.58 ms）
  不随 S 缩放；真实 checkpoint 的快照导入/资产实证成本本探针只测了 1 条尾部 ASSET。
- **没有覆盖的形状**：缺口在持久游标之前（durable skipped-asset 路径）、多个缺口、
  ASSET 占 10–20% 之外、S < 1k / > 1M（`COVERAGE_SCAN_MAX_PAGES = 4096` 上限）、
  relay GC 后日志不可读、真实网络抖动；均未测。
- **页大小耦合**：256 是生产私有常量，探针里用 `ceil(S/256)` 校验页数；生产改页大小
  时探针与本节需同步。
- **时间噪声**：共享 8 vCPU 机器、load 0.1–2.4；5k 的第 3 轮 0.617 ms（比中位数高 29%），
  25k 的 prepare 曾出现一次 8.8 ms 冷启动离群值。绝对值只用于量级/线性度判断。
- 探针**未 commit**；写作用域仅 `crates/sync/tests/phase1a_release.rs` 与本文件。

`````

---

# 附录 B：改动代码与关键测试


## `crates/storage/src/session/flush.rs`

`````rust
//! 未 ACK journal 的后台 flush 线程（ADR 0003 §5 / ADR 0001 §3.4）。
//!
//! 契约：
//!
//! - `flush()` 只投递一条命令就返回票据，**调用线程绝不碰 SQLite**；
//! - 后台线程每次取 journal 的**连续前缀**，合并成一次 A 事务写 durable；
//! - 写成功后先裁剪前缀、推进 durable 水位，再把 [`SessionEvent::Durable`] 推进事件队列；
//! - 写失败**不裁剪、不 ACK**：同一前缀下次重试，因此「已 ACK 的部分」不会丢；
//! - **head 过期**（[`StorageError::HeadConflict`]）：同一命令内**至多**自动
//!   `load_head` + 重建 sink 一次，并用新 CAS 基准重试**同一前缀**一次。只有新 head 的
//!   ops 全部落在未 ACK 前缀最后一条的 `end_vv` 里（即外部推进写的是本会话 live 已知的
//!   op）才允许重试——否则写入的 `head_vv`/`frontier` 会缺掉外部 ops，下一次
//!   `load_body_doc` 的完整性校验直接 `Corrupt`。不满足或重建后仍失败 → **不可恢复**：
//!   `last_write_error` 明确提示「需要重开文档」，journal 原样保留，后续 flush 立即失败、
//!   不再碰数据库（有界重试，不无限写）。

use std::collections::VecDeque;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::journal::{DurableSink, JournalEntry, JournalOrigin, PendingJournal, VaultJournalSink};
use crate::store::{load_head, HeadState};
use crate::sync_bridge::SyncContext;
use crate::{StorageError, Vault};

use super::gate::AppliedHeads;
use super::protocol::{BatchId, DurableAck, FlushTicketId, LiveRev, SessionEvent};

/// 闸门判定用的水位快照（见 [`JournalHandle::gate_progress`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GateProgress {
    /// 未 ACK 条数。
    pub(crate) pending: usize,
    /// 已 durable 到的 live 修订号。
    pub(crate) durable_through: LiveRev,
    /// 已 durable 的 head seq（外部提交的 CAS 基准）。
    pub(crate) durable_head_seq: u64,
    /// 最近一次前缀写失败原因（成功写入后清空；不可恢复时保留「需要重开文档」提示）。
    pub(crate) last_write_error: Option<String>,
}

/// 取锁；锁被毒化也继续用内部值（清理路径不允许 panic）。
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 后台 flush 命令（票据号已由调用方持有，线程只需要目标水位）。
#[derive(Debug, Clone, Copy)]
pub(crate) struct FlushCommand {
    /// 本次 flush 需要覆盖到的 live 修订号。
    pub(crate) target_live_rev: LiveRev,
}

/// 会话与 flush 线程共享的未 ACK 状态。
#[derive(Debug)]
pub(crate) struct PendingState {
    /// 未 ACK journal（容量准入 / 连续前缀 / durable 后裁剪）。
    pub(crate) journal: PendingJournal,
    /// 与 journal 前缀一一对应的 live 修订号。
    pub(crate) revs: VecDeque<LiveRev>,
    /// 与 journal 前缀一一对应的 update 字节数（64 KiB 阈值用）。
    pub(crate) sizes: VecDeque<usize>,
    /// 未 ACK update 的总字节数。
    pub(crate) pending_bytes: usize,
    /// 下一个批次号。
    pub(crate) next_batch: u64,
    /// 下一张票据号。
    pub(crate) next_ticket: u64,
    /// 已 durable 的 head 状态（A 事务的 CAS 基准）。
    pub(crate) durable: HeadState,
    /// 最近一次成功 durable 的批次号。
    pub(crate) durable_batch: Option<BatchId>,
    /// 已 durable 到的 live 修订号。
    pub(crate) durable_through: LiveRev,
    /// 最近一次前缀写失败的原因；下一次成功写入后清空。
    ///
    /// 外部提交闸门据此把「还在 Flushing」明确升级成 `Failed`，不静默等一个永远不来的 ACK。
    pub(crate) last_write_error: Option<String>,
    /// 不可恢复的 flush 失败（head 过期且无法安全自动重建）。
    ///
    /// 一旦置位就**不再自动重试**：后续 flush 命令立即失败、不再碰数据库；journal 原样
    /// 保留（未 ACK 的数据不丢也不会被静默丢弃），只有关闭并重开文档才能恢复。
    /// 外部提交闸门通过 [`GateProgress::last_write_error`] 把它升级成 `Failed`，UI 可据此
    /// 提示重开文档。
    pub(crate) unrecoverable: Option<String>,
}

/// journal 句柄：会话侧投递命令，后台线程负责写。
#[derive(Debug)]
pub(crate) struct JournalHandle {
    state: Arc<Mutex<PendingState>>,
    events: Arc<Mutex<VecDeque<SessionEvent>>>,
    sender: Option<Sender<FlushCommand>>,
    worker: Option<JoinHandle<()>>,
}

impl JournalHandle {
    /// 启动后台 flush 线程。
    ///
    /// `flush_delay` 是**测试/故障注入**用的可控延迟（模拟慢盘），生产传 `None`；
    /// `sync` 是 ADR 0009 切片 2b 的本地意图上下文（`None` = 不写 `sync_outbox`）。
    pub(crate) fn spawn(
        vault: Vault,
        doc_id: String,
        head: HeadState,
        capacity: usize,
        max_batch: usize,
        flush_delay: Option<Duration>,
        sync: Option<SyncContext>,
    ) -> Self {
        let state = Arc::new(Mutex::new(PendingState {
            journal: PendingJournal::new(capacity),
            revs: VecDeque::new(),
            sizes: VecDeque::new(),
            pending_bytes: 0,
            next_batch: 1,
            next_ticket: 1,
            durable: head,
            durable_batch: None,
            durable_through: LiveRev(0),
            last_write_error: None,
            unrecoverable: None,
        }));
        let events: Arc<Mutex<VecDeque<SessionEvent>>> = Arc::new(Mutex::new(VecDeque::new()));
        let (sender, receiver) = channel::<FlushCommand>();

        let worker_state = Arc::clone(&state);
        let worker_events = Arc::clone(&events);
        let config = FlusherConfig {
            doc_id,
            max_batch: max_batch.max(1),
            flush_delay,
            sync,
        };
        let worker = thread::Builder::new()
            .name("menghunote-flush".to_owned())
            .spawn(move || {
                flusher_main(vault, worker_state, worker_events, receiver, config);
            });

        // 线程起不来时退化为「没有后台 flush」：票据照发，但不会有 ACK（调用方可重启会话）。
        let worker = worker.ok();

        Self {
            state,
            events,
            sender: Some(sender),
            worker,
        }
    }

    /// 受理一次 flush：登记票据并投递命令，立即返回票据号。
    pub(crate) fn submit_ticket(&self, target: LiveRev) -> FlushTicketId {
        let ticket_id = {
            let mut guard = lock(&self.state);
            guard.next_ticket += 1;
            FlushTicketId(guard.next_ticket)
        };
        if let Some(sender) = self.sender.as_ref() {
            let _ = sender.send(FlushCommand {
                target_live_rev: target,
            });
        }
        ticket_id
    }

    /// 共享状态（会话核在持锁区间内完成「准入 + 改 Loro + 追加」）。
    pub(crate) fn state_ref(&self) -> &Mutex<PendingState> {
        &self.state
    }

    /// 事件队列（flush 线程写入，会话核读取）。
    pub(crate) fn events_ref(&self) -> &Mutex<VecDeque<SessionEvent>> {
        &self.events
    }

    /// 未 ACK 条数。
    pub(crate) fn pending_len(&self) -> usize {
        lock(&self.state).journal.pending_len()
    }

    /// 是否已满（背压判定）。
    pub(crate) fn is_full(&self) -> bool {
        lock(&self.state).journal.is_full()
    }

    /// 未 ACK update 总字节数（调度器判断 64 KiB 阈值）。
    pub(crate) fn pending_bytes(&self) -> usize {
        lock(&self.state).pending_bytes
    }

    /// 已 durable 的 head seq。
    pub(crate) fn durable_head_seq(&self) -> u64 {
        lock(&self.state).durable.head_seq
    }

    /// 已 durable 到哪个 live 修订号。
    pub(crate) fn durable_through(&self) -> LiveRev {
        lock(&self.state).durable_through
    }

    /// 外部提交闸门判定用的**一次性快照**：一次取锁同时读 pending / durable 水位 /
    /// 最近一次写失败原因，避免分多次取锁看到互相矛盾的组合。
    pub(crate) fn gate_progress(&self) -> GateProgress {
        let guard = lock(&self.state);
        GateProgress {
            pending: guard.journal.pending_len(),
            durable_through: guard.durable_through,
            durable_head_seq: guard.durable.head_seq,
            last_write_error: guard.last_write_error.clone(),
        }
    }

    /// 外部提交 adopt / end 之后重定 durable 基准（调用方必须已持锁）。
    ///
    /// `loro_docs.head_*` 已由外部提交（`engine.integrate`）推进；这里把 flusher 的 CAS
    /// 基准与 durable 水位对齐，避免下一次本地 flush 报 `HeadConflict`。
    ///
    /// **只允许在 journal `pending == 0` 时调用**：journal 条目的 `end_vv` / `end_frontier`
    /// 是追加那一刻的快照，有在途条目时重定基准会让水位于条目自相矛盾（前瞻 head 回退）。
    pub(crate) fn rebase_locked(
        guard: &mut PendingState,
        heads: &AppliedHeads,
        through: LiveRev,
    ) -> Result<(), StorageError> {
        if guard.journal.pending_len() != 0 {
            return Err(StorageError::Corrupt {
                context: "有未 ACK 条目时不能重定 durable 基准".to_owned(),
            });
        }
        guard.durable.head_seq = heads.head_seq;
        guard.durable.head_frontier = heads.frontier.clone();
        guard.durable.head_vv = heads.vv.clone();
        guard.durable_through = through;
        Ok(())
    }

    /// 取走全部待推送事件。
    pub(crate) fn drain_events(&self) -> Vec<SessionEvent> {
        let mut queue = lock(&self.events);
        queue.drain(..).collect()
    }

    /// 在 journal 里登记一条未 ACK 条目（调用方必须已通过容量准入）。
    ///
    /// `origin` 决定 flush 时是否产生出站同步意图：只有本地更新才产生。
    pub(crate) fn append(
        guard: &mut PendingState,
        doc_id: &str,
        batch_label: &str,
        origin: JournalOrigin,
        update: crate::body_doc::PendingUpdate,
        rev: LiveRev,
    ) -> Result<BatchId, crate::StorageError> {
        let batch = BatchId(guard.next_batch);
        guard.next_batch += 1;
        let size = update.payload.len();
        let entry = JournalEntry {
            doc_id: doc_id.to_owned(),
            batch_id: format!("{batch_label}-{}", batch.0),
            update,
            origin,
        };
        guard.journal.append(entry)?;
        guard.revs.push_back(rev);
        guard.sizes.push_back(size);
        guard.pending_bytes += size;
        Ok(batch)
    }
}

impl Drop for JournalHandle {
    fn drop(&mut self) {
        // 先关命令通道：flusher 的 recv 返回 Err 后退出循环；再 join，绝不 panic。
        drop(self.sender.take());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// flush 线程的一次性配置（与可变状态分开，避免 `flusher_main` 参数过多）。
#[derive(Debug, Clone)]
struct FlusherConfig {
    /// 归属文档（一次 flush 里不允许混文档）。
    doc_id: String,
    /// 一次 A 事务最多合并多少条。
    max_batch: usize,
    /// 注入的慢盘延迟（生产 `None`）。
    flush_delay: Option<Duration>,
    /// 本地意图上下文（`None` = 不写 `sync_outbox`）。
    sync: Option<SyncContext>,
}

/// flush 线程主体：一条命令 = 至少一次前缀写，直到覆盖 `target_live_rev` 或 journal 空。
fn flusher_main(
    vault: Vault,
    state: Arc<Mutex<PendingState>>,
    events: Arc<Mutex<VecDeque<SessionEvent>>>,
    receiver: Receiver<FlushCommand>,
    config: FlusherConfig,
) {
    let FlusherConfig {
        doc_id,
        max_batch,
        flush_delay,
        sync,
    } = &config;
    let doc_id = doc_id.clone();
    let max_batch = *max_batch;
    let flush_delay = *flush_delay;
    let sync = *sync;
    while let Ok(command) = receiver.recv() {
        if let Some(delay) = flush_delay {
            thread::sleep(delay);
        }

        let mut emitted_max: Option<LiveRev> = None;
        // P1-3：head 过期时同一命令**最多**自动重建 1 次 sink（`load_head` → 新基准 →
        // 重试同一前缀一次）；用尽后进入不可恢复状态，不再自动重试。
        let mut auto_rebuilt = false;
        loop {
            // 1) 取连续前缀（持锁只做内存操作，绝不在锁内写盘）
            let (entries, through, count, head_seq) = {
                let mut guard = lock(&state);
                // 不可恢复：后续 flush 立即失败、绝不再碰数据库（journal 原样保留）。
                if guard.unrecoverable.is_some() {
                    break;
                }
                let pending = guard.journal.pending_len();
                if pending == 0 {
                    break;
                }
                let take = max_batch.min(pending);
                let entries: Vec<JournalEntry> = guard.journal.peek_prefix(take).to_vec();
                let through = match guard.revs.get(take - 1) {
                    Some(rev) => *rev,
                    // 理论上不可能：journal 与 revs 一一对应；防御性退出
                    None => break,
                };
                let head_seq = guard.durable.head_seq;
                (entries, through, take, head_seq)
            };

            // 2) A 事务（在锁外，慢盘不会阻塞会话线程）
            let mut sink = VaultJournalSink::new(vault.clone(), doc_id.clone(), head_seq);
            if let Some(sync) = sync {
                sink = sink.with_sync_context(sync);
            }
            let written = match sink.write_prefix(&entries) {
                Ok(seq) => seq,
                // 普通写失败（磁盘/SQLite 等）：不裁剪、不 ACK，同一前缀下次重试；
                // 同时记录失败原因：外部提交闸门据此把 "Flushing" 明确升级为 Failed。
                Err(error) if !matches!(&error, StorageError::HeadConflict { .. }) => {
                    lock(&state).last_write_error = Some(error.to_string());
                    break;
                }
                // head 已被外部推进：同一份字节按旧 CAS 基准重试永远不会成功。
                // 先 `load_head` 拿真实基准、有界重建 sink，再重试**同一前缀**一次。
                Err(error) => {
                    if auto_rebuilt {
                        let message = reopen_message(
                            &doc_id,
                            lock(&state).journal.pending_len(),
                            format!(
                                "自动重建后再次 head 冲突（{error}），本命令的 1 次自动重建已用尽"
                            ),
                        );
                        let mut guard = lock(&state);
                        guard.last_write_error = Some(message.clone());
                        guard.unrecoverable = Some(message);
                        break;
                    }
                    auto_rebuilt = true;
                    let pending = lock(&state).journal.pending_len();
                    match retry_prefix_after_rebuild(
                        &vault, &doc_id, &entries, pending, head_seq, sync,
                    ) {
                        RebuildOutcome::Written(seq) => seq,
                        // 普通写失败：保持既有「同一前缀下次重试」契约，不升级成不可恢复。
                        RebuildOutcome::Retryable(message) => {
                            lock(&state).last_write_error = Some(message);
                            break;
                        }
                        RebuildOutcome::Unrecoverable(message) => {
                            let mut guard = lock(&state);
                            guard.last_write_error = Some(message.clone());
                            guard.unrecoverable = Some(message);
                            break;
                        }
                    }
                }
            };

            // 3) durable 之后才裁剪并推进水位
            let (batch, head_frontier) = {
                let mut guard = lock(&state);
                guard.last_write_error = None;
                let _ = guard.journal.ack_prefix(count);
                for _ in 0..count {
                    guard.revs.pop_front();
                    if let Some(size) = guard.sizes.pop_front() {
                        guard.pending_bytes = guard.pending_bytes.saturating_sub(size);
                    }
                }
                let batch = BatchId(guard.next_batch);
                guard.next_batch += 1;
                guard.durable.head_seq = written;
                let mut frontier = guard.durable.head_frontier.clone();
                let mut vv = guard.durable.head_vv.clone();
                if let Some(last) = entries.last() {
                    frontier = last.update.end_frontier.clone();
                    vv = last.update.end_vv.clone();
                }
                guard.durable.head_frontier = frontier.clone();
                guard.durable.head_vv = vv;
                guard.durable_through = through;
                guard.durable_batch = Some(batch);
                (batch, frontier)
            };

            let ack = DurableAck {
                batch_id: batch,
                through_live_rev: through,
                head_seq: written,
                head_frontier,
            };
            lock(&events).push_back(SessionEvent::Durable(ack));
            emitted_max = Some(through);

            if through >= command.target_live_rev {
                break;
            }
        }

        // 每个票据都必须有结局：没有待写条目（或已覆盖目标）时补一张确认票据，
        // 否则「手动保存」在无脏数据时永远等不到 ACK。
        let confirmation = {
            let guard = lock(&state);
            if guard.journal.pending_len() == 0
                && emitted_max.is_none_or(|through| through < command.target_live_rev)
            {
                Some((
                    guard.durable_batch.unwrap_or(BatchId(0)),
                    guard.durable_through,
                    guard.durable.head_seq,
                    guard.durable.head_frontier.clone(),
                ))
            } else {
                None
            }
        };
        if let Some((batch_id, through_live_rev, head_seq, head_frontier)) = confirmation {
            lock(&events).push_back(SessionEvent::Durable(DurableAck {
                batch_id,
                through_live_rev,
                head_seq,
                head_frontier,
            }));
        }
    }
}

/// head 冲突后自动重建 sink 的结果（P1-3 生产接线）。
#[derive(Debug)]
enum RebuildOutcome {
    /// 重建 sink 后用同一前缀重试成功；值为 durable 后的 `head_seq`。
    Written(u64),
    /// 不可恢复：必须关闭并重开文档（不再自动重试，也不再写库）。
    Unrecoverable(String),
    /// 重建后重试遇到普通写失败（磁盘/SQLite 等）：journal 保留，下一次 flush 再试。
    Retryable(String),
}

/// `HeadConflict` 后的有界自动重建：`load_head` → 新 CAS 基准 sink → 重试同一前缀一次。
///
/// **为什么不是无条件重试**：journal 条目的 `PendingUpdate` 是**追加那一刻**按旧 head
/// 导出的字节，写入 durable 的 `loro_docs.head_vv` / `head_frontier` 取自前缀最后一条的
/// `end_*` 快照。只有新 head 的 ops 全部包含在该快照的 VV 里（即外部推进写的是本会话
/// live 已知的 op）时，原样重试才与本会话 live 一致；否则 head 快照会缺掉外部 ops，
/// 下一次 `load_body_doc` 的完整性校验直接 `Corrupt`。后者 fail-closed：保留 journal、
/// 提示重开文档，绝不硬写。
///
/// `durable_base` 是本次写入用的 CAS 基准；新 head 必须严格大于它，回退的基准一律拒绝。
fn retry_prefix_after_rebuild(
    vault: &Vault,
    doc_id: &str,
    entries: &[JournalEntry],
    pending: usize,
    durable_base: u64,
    sync: Option<SyncContext>,
) -> RebuildOutcome {
    let head = match load_head(vault, doc_id) {
        Ok(head) => head,
        Err(error) => {
            return RebuildOutcome::Unrecoverable(reopen_message(
                doc_id,
                pending,
                format!("读不到当前 head（{error}），无法重建 sink"),
            ));
        }
    };
    if head.head_seq <= durable_base {
        return RebuildOutcome::Unrecoverable(reopen_message(
            doc_id,
            pending,
            format!(
                "库中 head 没有前进（CAS 基准 {durable_base}，读到 {}）；拒绝用回退的基准重写",
                head.head_seq
            ),
        ));
    }
    // 空前缀在 flusher 里不可达；防御性 fail-closed，不 panic。
    let Some(last) = entries.last() else {
        return RebuildOutcome::Unrecoverable(reopen_message(
            doc_id,
            pending,
            "未 ACK 前缀为空，无法校验 base".to_owned(),
        ));
    };
    let prefix_vv = match loro::VersionVector::decode(&last.update.end_vv) {
        Ok(vv) => vv,
        Err(error) => {
            return RebuildOutcome::Unrecoverable(reopen_message(
                doc_id,
                pending,
                format!("未 ACK 前缀的 end_vv 解码失败（{error}）"),
            ));
        }
    };
    let head_vv = match head.version_vector() {
        Ok(vv) => vv,
        Err(error) => {
            return RebuildOutcome::Unrecoverable(reopen_message(
                doc_id,
                pending,
                format!("新 head 的 VV 解码失败（{error}）"),
            ));
        }
    };
    if head_vv.is_empty() {
        return RebuildOutcome::Unrecoverable(reopen_message(
            doc_id,
            pending,
            format!(
                "新 head（head_seq {}）的 VV 为空，无法证明未 ACK 前缀与新 base 兼容",
                head.head_seq
            ),
        ));
    }
    if !prefix_vv.includes_vv(&head_vv) {
        return RebuildOutcome::Unrecoverable(reopen_message(
            doc_id,
            pending,
            format!(
                "库中 head 已推进到 {} 且包含本会话 live 文档未知的并发 ops；\
                 原样重试会写坏 head_vv/frontier，已拒绝自动重建",
                head.head_seq
            ),
        ));
    }

    // 同一个后台 flusher 线程内重建 sink（复用既有 Vault 写连接，不新建 journal/连接）。
    let mut sink = VaultJournalSink::new(vault.clone(), doc_id.to_owned(), head.head_seq);
    if let Some(sync) = sync {
        sink = sink.with_sync_context(sync);
    }
    match sink.write_prefix(entries) {
        Ok(seq) => RebuildOutcome::Written(seq),
        Err(StorageError::HeadConflict {
            expected, actual, ..
        }) => RebuildOutcome::Unrecoverable(reopen_message(
            doc_id,
            pending,
            format!("自动重建后再发生 head 冲突（基准 {expected}，实际 {actual}）"),
        )),
        Err(error) => RebuildOutcome::Retryable(format!(
            "{doc_id} 自动重建后重试前缀失败（journal 保留，等待下次 flush）：{error}"
        )),
    }
}

/// 统一的「需要重开文档」失败文案：闸门/UI 只看这一个字符串就必须知道该做什么。
fn reopen_message(doc_id: &str, pending: usize, reason: String) -> String {
    format!(
        "需要重开文档：{doc_id} 的未 ACK journal 无法安全自动重建（{reason}）；\
         {pending} 条未 ACK 更新保留在当前会话 journal 中（未写入、未静默丢弃）。\
         请关闭并重开该文档，从 durable 前缀重新加载后再继续编辑；\
         重开前未 durable 的本地改动不会自动落库"
    )
}

`````


## `crates/storage/src/journal.rs`

`````rust
//! 未 ACK 的写入 journal + 背压（ADR 0001 §3.4「A 已提交而 B 未完成」）。
//!
//! 模型：
//!
//! - **单份可追加 journal**：已产生但**尚未 durable** 的 update 按到达顺序排成一条 FIFO；
//! - **writer 取连续前缀**：flush 永远从头取 `min(pending, max_batch)` 条，
//!   不会跳过更早的条目（skip 就会造成 seq 空洞）；
//! - **durable 后裁剪**：只有 sink 成功返回后，才把这一段从 journal 里去掉并推进水位；
//! - **慢 I/O 合并 + 背压**：一批 flush 把当前所有 pending 前缀**一次性**写进同一个
//!   SQLite 事务；pending 达到 `capacity` 时 `submit` 直接返回
//!   [`StorageError::WriteBackpressure`]，**不 append**，调用方必须先 flush（拒绝无界增长）。
//!
//! 崩溃语义：sink 成功 = 已 durable；未 flush 的条目只在内存里，重启后自然丢掉。
//! 因此「已 ACK 的不丢、未 ACK 的只丢连续前缀之后」由本模块与 [`crate::store`] 的 A 事务共同保证。
//!
//! # head 过期：可重试 vs 必须重建 session（P1-3 契约）
//!
//! journal 条目里的 `PendingUpdate` 是**追加那一刻**按当时 head 导出的字节
//! （`end_frontier` / `end_vv` 是那次导出的快照），**不可重算**。如果同一文档的 head
//! 被别的 writer 推进（同步引擎 integrate、`SessionDriver` 的 adopt、另一个 Vault
//! 进程……），前缀的 CAS 基准（`loro_docs.head_seq`）就永远对不上：`append_updates`
//! 每次都返回 [`StorageError::HeadConflict`]，但**同一份字节重试多少次都不会成功**。
//! 这正是旧实现「flush 一直 HeadConflict、水位永远不前进」的卡死来源。
//!
//! 本模块用一条明确的二分法代替无限重试：
//!
//! - [`JournalWriter::flush`] 捕获 sink 的 [`StorageError::HeadConflict`]，**不重试**
//!   （0 次），立即转成终态错误 [`StorageError::JournalHeadStale`]
//!   （`doc_id` / `expected` / `actual` / `pending` 齐全，
//!   [`StorageError::requires_session_rebuild`] 为 `true`）。其它错误（磁盘失败等）
//!   原样透传 —— 那才是可重试的，journal 原样保留。
//! - 进入 stale 之后，`flush` / `flush_all` **不再触碰数据库**，幂等地返回同一个
//!   stale 错误；`submit` 也返回 `JournalHeadStale`（writer 已冻结，新条目必须交给
//!   重建后的 writer）。因此调用方无论怎么轮询，都不会挂死、也不会刷屏重试。
//! - **谁负责重建**：持有 live `BodyDoc` 的会话层（`session::JournalHandle` /
//!   `SessionDriver`，实现在 `crate::session::flush`）。重建顺序：
//!   ① 先停外部写并按 ADR 0012 对账（`HeadConflict` 先停外部写、验证授权、再对账，
//!   然后才允许有界重放）；② 把 durable journal 重放到**新 head**（`load_body_doc` /
//!   `load_head`），让内存文档与库一致；③ 用新 head 的 VV 重新导出本地意图
//!   （`BodyDoc::pending_update`）——只有这一步得到的字节才能写；④ 用新 `head_seq`
//!   造一个 [`VaultJournalSink`]，调用 [`JournalWriter::rebuild_sink`] 装回去。
//! - **未 durable 的旧条目怎么办**：`rebuild_sink` 把它们**原样交还**调用方（返回值）
//!   并从 writer 的 journal 里清空。它们**没有被写入、也没有被静默丢弃** —— 调用方
//!   必须用 ③ 重新导出的增量覆盖这段内容；若调用方决定不要，也必须显式消费返回值
//!   并在上层报告，不能假装成功。重建后 `durable_seq` 等于新 sink 的 CAS 基准。
//! - **有界重试规则**：CAS 冲突在 journal 层的自动重试是 **0** 次（基准是常量，
//!   重试不可能成功）；需要重放时由 session 层按 ADR 0012 做**有界**重放（一次），
//!   再失败就上报告需要人工处理。本模块不提供、也不会退化成无限重试。

use std::collections::VecDeque;

use crate::body_doc::PendingUpdate;
use crate::error::StorageError;
use crate::store::{OriginMarks, UpdateRecord};
use crate::sync_bridge::{self, SyncContext, SyncIntent};
use crate::Vault;

/// 一条 update 的**来源**（ADR 0009：只有本地提交才产生出站意图）。
///
/// - [`JournalOrigin::Local`]：真实本地写入 —— 用户输入 / IME / undo / redo / 结构提交，
///   启用同步上下文时在同一个 A 事务里写一条 `sync_outbox` 意图；
/// - [`JournalOrigin::Remote`]：远端导入 / adopt，**必须不产生任何意图**，否则会把刚
///   收到的数据原样推回 relay（自激循环，且破坏 epoch/ACK 语义）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JournalOrigin {
    /// 本机产生的更新（本地输入、undo/redo）。
    Local,
    /// 从远端导入的更新（`DocumentSession::apply_remote`）。
    Remote,
}

impl JournalOrigin {
    /// 是否应该产生出站同步意图。
    #[must_use]
    pub const fn produces_sync_intent(self) -> bool {
        matches!(self, Self::Local)
    }
}

/// 一条待落库的 update。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalEntry {
    /// 所属文档。
    pub doc_id: String,
    /// 批次 ID（同一次 flush 共用一个 batch_id）。
    pub batch_id: String,
    /// 增量本身。
    pub update: PendingUpdate,
    /// 来源（决定 flush 时是否产生出站意图）。
    pub origin: JournalOrigin,
}

impl JournalEntry {
    /// 本地提交（会产生同步意图）。
    #[must_use]
    pub fn local(
        doc_id: impl Into<String>,
        batch_id: impl Into<String>,
        update: PendingUpdate,
    ) -> Self {
        Self {
            doc_id: doc_id.into(),
            batch_id: batch_id.into(),
            update,
            origin: JournalOrigin::Local,
        }
    }

    /// 远端导入（**不**产生同步意图）。
    #[must_use]
    pub fn remote(
        doc_id: impl Into<String>,
        batch_id: impl Into<String>,
        update: PendingUpdate,
    ) -> Self {
        Self {
            doc_id: doc_id.into(),
            batch_id: batch_id.into(),
            update,
            origin: JournalOrigin::Remote,
        }
    }
}

/// 只可追加的未 ACK journal。
#[derive(Debug)]
pub struct PendingJournal {
    capacity: usize,
    entries: VecDeque<JournalEntry>,
    next_id: u64,
}

impl PendingJournal {
    /// 创建容量为 `capacity` 条的 journal；容量必须 > 0，否则永远只能报背压。
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            entries: VecDeque::new(),
            next_id: 0,
        }
    }

    /// 追加一条。已满时返回 [`StorageError::WriteBackpressure`] 且**不写入**。
    pub fn append(&mut self, entry: JournalEntry) -> Result<u64, StorageError> {
        if self.entries.len() >= self.capacity {
            return Err(StorageError::WriteBackpressure {
                capacity: self.capacity,
            });
        }
        let id = self.next_id;
        self.next_id += 1;
        self.entries.push_back(entry);
        Ok(id)
    }

    /// 当前未 ACK 条数。
    pub fn pending_len(&self) -> usize {
        self.entries.len()
    }

    /// 容量上限。
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// journal 是否已满（调用方应先 flush）。
    pub fn is_full(&self) -> bool {
        self.entries.len() >= self.capacity
    }

    /// 取从头开始的连续前缀（最多 `max` 条），只读不改。
    ///
    /// 取的一定是 FIFO 的最前面若干条：跳过更早的条目会造成 seq 空洞，
    /// 所以 flush 只能「从头开始」而不能挑着写。
    pub fn peek_prefix(&mut self, max: usize) -> &[JournalEntry] {
        let take = max.min(self.entries.len());
        if take == 0 {
            return &[];
        }
        // VecDeque 可能是环形，先整理成连续内存再切片
        self.entries.make_contiguous();
        let (front, _) = self.entries.as_slices();
        &front[..take]
    }

    /// durable 之后裁剪前缀。
    pub fn ack_prefix(&mut self, count: usize) -> Result<(), StorageError> {
        if count > self.entries.len() {
            return Err(StorageError::Corrupt {
                context: format!("ack_prefix({count}) 超过未 ACK 条数 {}", self.entries.len()),
            });
        }
        for _ in 0..count {
            self.entries.pop_front();
        }
        Ok(())
    }

    /// 取走全部未 ACK 条目。
    ///
    /// **只**供 [`JournalWriter::rebuild_sink`] 在 head 过期后显式交还给调用方使用：
    /// 正常路径一律走 [`Self::ack_prefix`]，这里既不写库也不推进任何水位。
    fn drain_all(&mut self) -> Vec<JournalEntry> {
        self.entries.drain(..).collect()
    }
}

/// durable 落点（生产实现见 [`VaultJournalSink`]，测试可注入慢盘/失败实现）。
pub trait DurableSink {
    /// 把一段**连续前缀**写 durable；成功返回这段里最后一条的 `seq`。
    ///
    /// 实现必须是原子的：要么整段 durable，要么什么都没写（否则会丢已 ACK 数据）。
    ///
    /// CAS 基准冲突必须返回 [`StorageError::HeadConflict`]（不能自己改基准硬写）：
    /// [`JournalWriter`] 据此把 writer 判成 head 过期，而不是无限重试同一份字节。
    fn write_prefix(&mut self, entries: &[JournalEntry]) -> Result<u64, StorageError>;

    /// 当前 sink 使用的 CAS 基准（= 它认为已 durable 的 `head_seq`）。
    ///
    /// writer 用它给 `durable_seq` 定初值，并在 [`JournalWriter::rebuild_sink`] 之后重新对齐
    /// 水位。实现必须返回**真正用于 CAS 的**那个值，否则重建后的水位会骗人。
    fn head_seq(&self) -> u64;
}

/// 一次 flush 的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlushReport {
    /// 本次写 durable 的条数。
    pub entries: usize,
    /// 写完后的 durable seq（sink 返回）。
    pub durable_seq: u64,
}

/// head 过期的终态记录（见模块头契约）。
///
/// 只要它非空，writer 就冻结：`submit` / `flush` 一律返回同一个
/// [`StorageError::JournalHeadStale`]，直到调用方 [`JournalWriter::rebuild_sink`]。
#[derive(Debug, Clone, PartialEq, Eq)]
struct StaleHead {
    doc_id: String,
    expected: u64,
    actual: u64,
}

impl StaleHead {
    fn to_error(&self, pending: usize) -> StorageError {
        StorageError::JournalHeadStale {
            doc_id: self.doc_id.clone(),
            expected: self.expected,
            actual: self.actual,
            pending,
        }
    }
}

/// journal writer：持有 journal 与 sink，串行 flush。
#[derive(Debug)]
pub struct JournalWriter<S: DurableSink> {
    journal: PendingJournal,
    sink: S,
    max_batch: usize,
    durable_seq: u64,
    flushed_entries: u64,
    /// 非空 = CAS 基准已过期，writer 已冻结（见模块头契约）。
    stale: Option<StaleHead>,
}

impl<S: DurableSink> JournalWriter<S> {
    /// `max_batch` 是一次 flush 最多合并多少条（慢 I/O 时 pending 会堆到这里为止）。
    ///
    /// `durable_seq` 初值取 sink 的 CAS 基准（[`DurableSink::head_seq`]），
    /// 因此空 journal 上 flush 报告的是真实水位，而不是 0。
    pub fn new(sink: S, capacity: usize, max_batch: usize) -> Self {
        let durable_seq = sink.head_seq();
        Self {
            journal: PendingJournal::new(capacity),
            sink,
            max_batch: max_batch.max(1),
            durable_seq,
            flushed_entries: 0,
            stale: None,
        }
    }

    /// 追加一条待落库 update；满了就是背压，调用方必须 flush 后重试。
    ///
    /// head 过期后拒绝（返回 [`StorageError::JournalHeadStale`]）：writer 已冻结，
    /// 新条目必须交给重建后的 writer，否则只会堆在一条永远写不动的前缀后面。
    pub fn submit(&mut self, entry: JournalEntry) -> Result<u64, StorageError> {
        if let Some(stale) = self.stale.as_ref() {
            return Err(stale.to_error(self.journal.pending_len()));
        }
        self.journal.append(entry)
    }

    /// 把当前前缀合并成一次 sink 写；成功后裁剪 journal 并推进水位。
    ///
    /// 失败时 journal **原样保留**（sink 承诺原子），下次重试不会丢已成功部分。
    ///
    /// 唯一的例外是 CAS 冲突：sink 返回 [`StorageError::HeadConflict`] 时前缀字节已不可
    /// 重放，本方法**不重试**（0 次），标记 stale 并返回
    /// [`StorageError::JournalHeadStale`] —— 调用方必须按模块头的契约重建 session，
    /// 而不是再 flush 一次。
    pub fn flush(&mut self) -> Result<FlushReport, StorageError> {
        // 终态优先：stale 之后连数据库都不碰（有界重试 = 0 次）。
        if let Some(stale) = self.stale.as_ref() {
            return Err(stale.to_error(self.journal.pending_len()));
        }

        // 先拷出前缀，避免同时借用 journal 与 sink
        let prefix: Vec<JournalEntry> = self.journal.peek_prefix(self.max_batch).to_vec();
        if prefix.is_empty() {
            return Ok(FlushReport {
                entries: 0,
                durable_seq: self.durable_seq,
            });
        }

        let entries = prefix.len();
        let durable_seq = match self.sink.write_prefix(&prefix) {
            Ok(seq) => seq,
            // CAS 冲突是**终态**：前缀的 end_vv/end_frontier 是按旧 head 导出的快照，
            // 重新导出只能由持有 live 文档的会话层做；同一份字节重试不可能成功。
            Err(StorageError::HeadConflict {
                doc_id,
                expected,
                actual,
            }) => {
                let stale = StaleHead {
                    doc_id,
                    expected,
                    actual,
                };
                let error = stale.to_error(self.journal.pending_len());
                self.stale = Some(stale);
                return Err(error);
            }
            // 其它错误（磁盘失败等）原样透传：journal 原样保留，同一前缀可以重试。
            Err(error) => return Err(error),
        };
        self.journal.ack_prefix(entries)?;
        self.durable_seq = durable_seq;
        self.flushed_entries += entries as u64;

        Ok(FlushReport {
            entries,
            durable_seq,
        })
    }

    /// flush 直到 journal 空或没有进展（一次性排空，供关闭/手动保存用）。
    ///
    /// head 过期时第一拍就返回 [`StorageError::JournalHeadStale`]，不会空转。
    pub fn flush_all(&mut self) -> Result<usize, StorageError> {
        let mut total = 0;
        while self.journal.pending_len() > 0 {
            let report = self.flush()?;
            if report.entries == 0 {
                break;
            }
            total += report.entries;
        }
        Ok(total)
    }

    /// writer 是否已进入「head 过期、必须重建」的终态。
    pub fn is_stale(&self) -> bool {
        self.stale.is_some()
    }

    /// 重建 sink：head 过期后**唯一**合法的恢复入口（见模块头契约）。
    ///
    /// 调用方必须先用新 head 重放 live 文档并重新导出本地意图；本方法只负责把 writer
    /// 从 stale 拉回可用状态：
    ///
    /// - 把旧 journal 里未写入的条目**原样交还**（返回值），并从 journal 里清空 ——
    ///   它们不写库、也**不被静默丢弃**；调用方要么用重新导出的增量覆盖其内容，
    ///   要么显式报告这次丢弃；
    /// - `durable_seq` 对齐新 sink 的 CAS 基准（[`DurableSink::head_seq`]）；
    /// - 未处于 stale 时拒绝：还有能写的前缀就换 sink，等于把本可落库的条目丢掉。
    pub fn rebuild_sink(&mut self, sink: S) -> Result<Vec<JournalEntry>, StorageError> {
        if self.stale.is_none() {
            return Err(StorageError::Corrupt {
                context: "journal 未处于 head 过期状态，拒绝重建 sink（会丢弃仍可写入的条目）"
                    .to_owned(),
            });
        }
        let handed_back = self.journal.drain_all();
        self.sink = sink;
        self.stale = None;
        self.durable_seq = self.sink.head_seq();
        Ok(handed_back)
    }

    /// 未 ACK 条数。
    pub fn pending_len(&self) -> usize {
        self.journal.pending_len()
    }

    /// journal 容量。
    pub fn capacity(&self) -> usize {
        self.journal.capacity()
    }

    /// 已 durable 的最后 seq。
    pub fn durable_seq(&self) -> u64 {
        self.durable_seq
    }

    /// 累计 flush 成功的条数。
    pub fn flushed_entries(&self) -> u64 {
        self.flushed_entries
    }

    /// 取回 sink（关闭/测试用）。
    pub fn into_sink(self) -> S {
        self.sink
    }
}

/// 生产用 sink：把前缀写进 [`crate::store::append_updates`] 的 A 事务。
///
/// 带 [`SyncContext`] 时（`with_sync_context`），**本地**条目在同一 A 事务里各写一条
/// `sync_outbox` 意图（与 records 一一对应）；未启用时行为与切片 2a 之前完全一致。
#[derive(Debug, Clone)]
pub struct VaultJournalSink {
    vault: Vault,
    doc_id: String,
    head_seq: u64,
    sync: Option<SyncContext>,
}

impl VaultJournalSink {
    /// 以库中当前 head_seq 作为 CAS 基准（默认**不**产生同步意图）。
    pub fn new(vault: Vault, doc_id: impl Into<String>, head_seq: u64) -> Self {
        Self {
            vault,
            doc_id: doc_id.into(),
            head_seq,
            sync: None,
        }
    }

    /// 启用本地意图：每次 flush 为本地条目生成确定性 `SyncIntent`（同事务落库）。
    #[must_use]
    pub fn with_sync_context(mut self, sync: SyncContext) -> Self {
        self.sync = Some(sync);
        self
    }

    /// 当前已 durable 的 seq。
    pub fn head_seq(&self) -> u64 {
        self.head_seq
    }
}

impl DurableSink for VaultJournalSink {
    fn write_prefix(&mut self, entries: &[JournalEntry]) -> Result<u64, StorageError> {
        if entries.is_empty() {
            return Err(StorageError::Corrupt {
                context: "sink 收到空前缀".to_owned(),
            });
        }
        for entry in entries {
            if entry.doc_id != self.doc_id {
                return Err(StorageError::Corrupt {
                    context: format!(
                        "一次 flush 里混了多个文档：期望 {}，实际 {}",
                        self.doc_id, entry.doc_id
                    ),
                });
            }
        }
        // batch_id 逐条保留：合并 flush 可能跨多个逻辑批次
        let records: Vec<UpdateRecord> = entries
            .iter()
            .map(|entry| UpdateRecord::new(entry.batch_id.clone(), entry.update.clone()))
            .collect();
        // 远端导入条目：其 `end_vv - 上一条 end_vv` 是远端真正带来的 ops，
        // 同事务记入「远端已知 VV」（baseline tombstone 判定用）。
        let remote_marks: Vec<bool> = entries
            .iter()
            .map(|entry| entry.origin == JournalOrigin::Remote)
            .collect();
        let head = match self.sync {
            // 默认路径（未启用同步）：行为与切片 2a 之前一致（只是多记远端已知 VV）。
            None => sync_bridge::append_updates_with_intent_slots(
                &self.vault,
                &self.doc_id,
                self.head_seq,
                &records,
                &[],
                OriginMarks::PerRecord(&remote_marks),
            )?,
            Some(ctx) => {
                // 一 update 一意图（remote 条目为空位）；seq 必须与 A 事务写进
                // `loro_updates.seq` 的值完全一致，否则意图会指向别人的 update。
                let mut slots: Vec<Option<SyncIntent>> = Vec::with_capacity(entries.len());
                for (index, entry) in entries.iter().enumerate() {
                    if !entry.origin.produces_sync_intent() {
                        slots.push(None);
                        continue;
                    }
                    let seq = self
                        .head_seq
                        .checked_add(1)
                        .and_then(|value| value.checked_add(index as u64))
                        .ok_or_else(|| StorageError::Corrupt {
                            context: format!("{} 本地 seq 溢出", self.doc_id),
                        })?;
                    slots.push(Some(sync_bridge::local_sync_intent(
                        &self.doc_id,
                        seq,
                        &entry.batch_id,
                        ctx,
                    )?));
                }
                sync_bridge::append_updates_with_intent_slots(
                    &self.vault,
                    &self.doc_id,
                    self.head_seq,
                    &records,
                    &slots,
                    OriginMarks::PerRecord(&remote_marks),
                )?
            }
        };
        self.head_seq = head.head_seq;
        Ok(head.head_seq)
    }

    /// CAS 基准 = 本 sink 已 durable 的 head_seq（与固有方法 `head_seq` 同一个值）。
    fn head_seq(&self) -> u64 {
        self.head_seq
    }
}

`````


## `crates/storage/src/error.rs`

`````rust
//! 存储层统一错误类型。
//!
//! 库层只暴露 [`StorageError`]，应用层再用 `anyhow` 之类的汇总类型包装。
//! 业务路径禁止 `unwrap()`；需要上下文时用 `?` 或 `map_err` 转成带上下文的变体。

use thiserror::Error;

/// 存储层错误。
#[derive(Debug, Error)]
pub enum StorageError {
    /// 文件系统错误（建父目录、启动工作线程等）。
    #[error("文件系统操作失败：{0}")]
    Io(#[from] std::io::Error),

    /// SQLite 自身返回的错误（SQL 语法、约束冲突、类型不匹配、锁等待超时等）。
    #[error("SQLite 操作失败：{0}")]
    Sqlite(#[from] rusqlite::Error),

    /// 只读快照在 `busy_timeout` 内没拿到读锁（写事务长时间占用）。
    ///
    /// 读路径**不做无限重试**：调用方（search）应当退避后重开快照，或改用
    /// `reconcile_required` 走全量对账，而不是在这里阻塞写线程。
    #[error("只读快照在 {busy_timeout_ms}ms 内未获得读锁（{context}）：请退避后重试或走全量对账")]
    SnapshotBusy {
        /// 出错的具体动作（打开/查询/解码）。
        context: &'static str,
        /// 该连接实际使用的 `busy_timeout`。
        busy_timeout_ms: u32,
    },

    /// 索引清单里某列的类型不符合 schema 预期（说明库被外部改坏或 schema 漂移）。
    #[error("索引清单列 [{column}] 类型异常：{found}")]
    ManifestShape {
        /// 列名。
        column: &'static str,
        /// 实际读到的 SQLite 类型。
        found: &'static str,
    },

    /// 迁移失败：带上目标版本与迁移名称，便于定位是哪一级迁移脚本出错。
    #[error("迁移到 schema v{version}（{name}）失败：{source}")]
    Migration {
        /// 目标 schema 版本。
        version: i64,
        /// 迁移的可读名称。
        name: &'static str,
        /// 底层 SQLite 错误。
        #[source]
        source: rusqlite::Error,
    },

    /// PRAGMA 设置后读回校验不一致。
    #[error("PRAGMA 校验失败：[{name}] 期望 {expected}，实际 {actual}")]
    PragmaMismatch {
        /// PRAGMA 名称。
        name: &'static str,
        /// 期望值（统一字符串化，便于比较）。
        expected: String,
        /// 实际读回值。
        actual: String,
    },

    /// 数据库 schema 版本高于本程序支持的版本：拒绝以可写方式打开。
    ///
    /// 对应 ADR 0001 §3.9「新 schema 下旧程序只读拒写」。
    #[error("数据库 schema 版本为 v{found}，本程序最高支持 v{supported}；拒绝以可写方式打开")]
    UnsupportedSchemaVersion {
        /// 数据库中记录的 `user_version`。
        found: i64,
        /// 本程序支持的最高 schema 版本。
        supported: i64,
    },

    /// 工作线程通道不可用（线程已退出，或初始化阶段就失败了）。
    #[error("存储工作线程不可用（{context}）")]
    ChannelClosed {
        /// 出错位置的可读描述。
        context: &'static str,
    },

    /// Loro 返回的错误（非法 update、peer id 非法、解码失败等）。
    #[error("Loro 操作失败：{0}")]
    Loro(#[from] loro::LoroError),

    /// Loro 导出/编码失败。
    #[error("Loro 导出失败：{0}")]
    LoroEncode(#[from] loro::LoroEncodeError),

    /// 坐标 / 单位换算失败（越界、半截字符、半截代理对）。
    ///
    /// 全仓只有 `menghunote_text::CoordError` 一份定义（ADR 0003 §4），这里只做包装。
    #[error("坐标换算失败：{0}")]
    Coord(#[from] menghunote_text::CoordError),

    /// A 事务的乐观并发冲突：head 已被别人推进，本批 update 未落库。
    #[error("文档 {doc_id} 的 head_seq 冲突：期望 {expected}，实际 {actual}；本批 update 已回滚")]
    HeadConflict {
        /// 文档 ID。
        doc_id: String,
        /// 调用方看到并用作 CAS 基准的 seq。
        expected: u64,
        /// 库里实际的 seq。
        actual: u64,
    },

    /// 未 ACK journal 的 CAS 基准已过期：head 被别的 writer 推进了。
    ///
    /// journal 条目的 `PendingUpdate` 是**追加那一刻**按当时 head 导出的字节
    /// （`end_frontier` / `end_vv` 都是那次导出的快照），**不可重算**。head 一旦前进，
    /// 同一份字节写一万次也只会再得到一次 CAS 冲突 —— 所以这是**终态**错误，
    /// 由 [`StorageError::requires_session_rebuild`] 显式标记：
    ///
    /// - **谁负责重建**：持有 live `BodyDoc` 的会话层（`session::JournalHandle` /
    ///   `SessionDriver`）。它必须先把内存文档重放到新 head，再按新 head 重新导出增量，
    ///   并用新 `head_seq` 造 sink（见 `crate::journal` 模块头的契约）；
    /// - **旧条目**：不会被写入，也**不会**被静默丢弃 —— `JournalWriter::rebuild_sink`
    ///   把它们原样交还给调用方；
    /// - **重试**：本层自动重试次数为 0，`flush` 之后不再触碰数据库。
    #[error(
        "文档 {doc_id} 的未 ACK journal 基准已过期（期望 head {expected}，实际 {actual}）：\
         {pending} 条已导出增量不可重算，必须重建 session 后重新导出；已禁止重试"
    )]
    JournalHeadStale {
        /// 文档 ID。
        doc_id: String,
        /// 写入时使用的 CAS 基准（`loro_docs.head_seq`）。
        expected: u64,
        /// 库里实际看到的 head。
        actual: u64,
        /// 过期时 journal 里未 durable 的条数。
        pending: usize,
    },

    /// 未 ACK journal 已满：必须先 flush 再重试（背压，不允许无界增长）。
    #[error("未 ACK journal 已满（容量 {capacity} 条），拒绝新写入；请先 flush")]
    WriteBackpressure {
        /// journal 容量。
        capacity: usize,
    },

    /// 事务守卫不满足：某条语句没有影响预期行数，整笔事务已回滚。
    #[error("事务守卫失败：第 {statement} 条语句应影响 {expected} 行，实际 {actual} 行；已回滚")]
    GuardFailed {
        /// 语句下标（0 起）。
        statement: usize,
        /// 期望影响行数。
        expected: usize,
        /// 实际影响行数。
        actual: usize,
    },

    /// 相对光标无法解析成当前位置（历史不可读或已丢失）。
    #[error("光标无法解析到位置：{0}")]
    CursorUnresolved(String),

    /// 标签名为空（归一化后没有任何字符）。
    #[error("标签名非法：{name:?}（归一化后为空）")]
    InvalidTagName {
        /// 传入的原始标签名。
        name: String,
    },

    /// 结构树里找不到指定节点（节点 ID 非法或已被移除）。
    #[error("结构树里找不到节点 {node_id}")]
    UnknownNode {
        /// 节点 ID。
        node_id: String,
    },

    /// 同步桥 CAS/幂等冲突：同一 message/epoch 已存在不同字节，或旧 ACK/旧 digest。
    #[error("同步状态冲突：{context}")]
    SyncConflict {
        /// 具体上下文（message/epoch/digest）。
        context: String,
    },

    /// 同步 log_id 与本地已记录的不一致（relay reset 必须显式迁移，不能静默归零）。
    #[error("同步 log_id 不一致：本地 {expected:?}，收到 {found:?}")]
    SyncLogMismatch {
        /// 本地记录的 log_id。
        expected: Vec<u8>,
        /// 新收到的 log_id。
        found: Vec<u8>,
    },

    /// 历史版本正文的 zstd 压缩/解压失败（库里的历史快照损坏，或编码阶段内存不足）。
    ///
    /// 与 [`Self::Io`] 区分：这不是文件系统错误，调用方只该提示"历史版本读取失败"，
    /// 不应把它当成库路径/权限问题。
    #[error("历史版本编解码失败（{context}）：{source}")]
    HistoryCodec {
        /// 具体动作（压缩/解压）。
        context: &'static str,
        /// 底层 `std::io::Error`（zstd 流）。
        #[source]
        source: std::io::Error,
    },

    /// 数据库中出现不自洽状态（缺行、seq 断档、重放结果与记录不一致等）。
    #[error("存储状态不自洽：{context}")]
    Corrupt {
        /// 具体上下文。
        context: String,
    },
}

impl StorageError {
    /// 是否必须**重建 session**（而不是重试）。
    ///
    /// 目前只有 [`Self::JournalHeadStale`]：那批 `PendingUpdate` 是按旧 head 导出的字节，
    /// 没有任何 rebase 可能，原样重试永远不会成功。调用方必须先重建 session、重新导出
    /// 增量（见 `crate::journal` 模块头），否则只会把失败换个地方再报一次。
    #[must_use]
    pub fn requires_session_rebuild(&self) -> bool {
        matches!(self, Self::JournalHeadStale { .. })
    }

    /// 是否值得**原样重试**（只表示"条件变化后可能成功"，不表示"一定要重试"）。
    ///
    /// - [`Self::HeadConflict`]：store 层 CAS 冲突 —— 重读 head 后重放（sync engine 语义，
    ///   与 journal 层的[`Self::JournalHeadStale`]不同：那里的字节可以重新导出）；
    /// - [`Self::SnapshotBusy`]：读锁超时 —— 退避后重开快照；
    /// - [`Self::WriteBackpressure`]：journal 满 —— 先 flush 再重试。
    ///
    /// 其余变体一律 `false`：盲目重试只会挂死，或把真正的损坏/配置错误掩盖成"暂时失败"。
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::HeadConflict { .. } | Self::SnapshotBusy { .. } | Self::WriteBackpressure { .. }
        )
    }
}

`````


## `crates/storage/src/body_doc.rs`

`````rust
//! 每个正文 Doc 一个 `LoroDoc`（ADR 0001 §3.1 / §3.4）。
//!
//! 约定：
//!
//! - 正文只存**原始 Markdown**（LF 归一），不用 Loro Rich Text，不把块树落库；
//! - 对外编辑接口的 `pos` / `len` 一律是 **Unicode scalar**（Loro 实测单位；
//!   通用换算的唯一实现在 [`menghunote_text`]，实测结论见 `tests/loro_units.rs`）；
//!   `convert_pos` 对半截索引**向下取整**、`to_delta()` **只产出 Insert** 这两个陷阱也记在那里，
//!   适配逻辑留在本文件，不搬进 text（ADR 0003：text 只管通用单位，不管 Loro API 语义）；
//!   IME 需要 UTF-16 时用 [`BodyDoc::insert_utf16`] / [`BodyDoc::delete_utf16`] 的显式接口；
//! - 编辑即映射：一次改动就是 `delete(range) + insert(text)`（ADR 0001 §3.4），
//!   过期基准不许直接应用（基准校验由调用方的 `command`/frontier 负责，本层只校验范围）；
//! - 导出增量用「上次 durable 的 VV」作起点，**不在本层推进任何 watermark**。

use loro::cursor::{Cursor, Side};
use loro::{ExportMode, Frontiers, LoroDoc, LoroText, TextDelta, UndoManager, VersionVector};

use crate::error::StorageError;
use menghunote_text::CoordError;

/// 正文容器在 `LoroDoc` 里的名字。
pub const BODY_CONTAINER: &str = "body";

/// 落库时记录的 codec 标识（ADR 0001 §3.9「每个 snapshot/update 携带实际 codec 标识」）。
pub const CODEC_VERSION: &str = "loro-1.17.1";

/// 本切片写入的 `app_doc_schema`。
pub const APP_DOC_SCHEMA: i64 = 1;

/// 本切片要求的最低读取方版本。
pub const MIN_READER: i64 = 1;

/// 一次本地提交导出出的增量。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingUpdate {
    /// Loro update 字节（相对导出基准的增量）。
    pub payload: Vec<u8>,
    /// 该增量应用后的 frontier（`Frontiers::encode`）。
    pub end_frontier: Vec<u8>,
    /// 该增量应用后的 VV（`VersionVector::encode`）。
    pub end_vv: Vec<u8>,
}

/// 一个正文 CRDT 文档。
pub struct BodyDoc {
    doc_id: String,
    doc: LoroDoc,
    text: LoroText,
}

impl std::fmt::Debug for BodyDoc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BodyDoc")
            .field("doc_id", &self.doc_id)
            .field("scalars", &self.scalar_len())
            .finish()
    }
}

impl BodyDoc {
    /// 新建文档并设置本机 peer id。
    ///
    /// peer id 必须每设备唯一（ADR 0001 §3.2）；克隆旧备份当新实例时必须换新 peer id。
    pub fn create(doc_id: impl Into<String>, peer_id: u64) -> Result<Self, StorageError> {
        let doc = LoroDoc::new();
        doc.set_peer_id(peer_id)?;
        let text = doc.get_text(BODY_CONTAINER);
        Ok(Self {
            doc_id: doc_id.into(),
            doc,
            text,
        })
    }

    /// 从快照 + 后续 update 重放出一个文档（启动恢复路径）。
    pub fn restore(
        doc_id: impl Into<String>,
        peer_id: u64,
        snapshot: Option<&[u8]>,
        updates: &[Vec<u8>],
    ) -> Result<Self, StorageError> {
        let this = Self::create(doc_id, peer_id)?;
        if let Some(snapshot) = snapshot {
            this.doc.import(snapshot)?;
        }
        for payload in updates {
            this.doc.import(payload)?;
        }
        Ok(this)
    }

    /// 文档 ID。
    pub fn doc_id(&self) -> &str {
        &self.doc_id
    }

    /// 当前正文（原始 Markdown）。
    pub fn text(&self) -> String {
        self.text.to_string()
    }

    /// 以 Unicode scalar 计的正文长度（Loro 实测单位）。
    pub fn scalar_len(&self) -> usize {
        self.text.len_unicode()
    }

    /// 以 UTF-16 code unit 计的正文长度。
    pub fn utf16_len(&self) -> usize {
        self.text.len_utf16()
    }

    /// 以 UTF-8 字节计的正文长度。
    pub fn byte_len(&self) -> usize {
        self.text.len_utf8()
    }

    /// 按 **Unicode scalar** 位置插入文本。越界报错，不截断。
    pub fn insert_scalars(&self, pos: usize, text: &str) -> Result<(), StorageError> {
        self.check_range(pos, 0)?;
        self.text.insert(pos, text)?;
        self.doc.commit();
        Ok(())
    }

    /// 按 **Unicode scalar** 位置删除 `len` 个字符。
    pub fn delete_scalars(&self, pos: usize, len: usize) -> Result<(), StorageError> {
        self.check_range(pos, len)?;
        self.text.delete(pos, len)?;
        self.doc.commit();
        Ok(())
    }

    /// 替换 `[pos, pos + len)` 的 scalar 区间（IME 确认与格式替换都走这一个入口）。
    pub fn replace_scalars(&self, pos: usize, len: usize, text: &str) -> Result<(), StorageError> {
        self.check_range(pos, len)?;
        if len > 0 {
            self.text.delete(pos, len)?;
        }
        if !text.is_empty() {
            self.text.insert(pos, text)?;
        }
        self.doc.commit();
        Ok(())
    }

    /// 应用一段 delta（`Retain` / `Insert` / `Delete` 的计数单位实测为 Unicode scalar）。
    pub fn apply_delta_scalars(&self, delta: &[TextDelta]) -> Result<(), StorageError> {
        self.text.apply_delta(delta)?;
        self.doc.commit();
        Ok(())
    }

    /// 按 **UTF-16** 位置插入；落在代理对中间会报错（非法半代理对位置）。
    pub fn insert_utf16(&self, pos: usize, text: &str) -> Result<(), StorageError> {
        self.text.insert_utf16(pos, text)?;
        self.doc.commit();
        Ok(())
    }

    /// 按 **UTF-16** 位置删除；落在代理对中间会报错。
    pub fn delete_utf16(&self, pos: usize, len: usize) -> Result<(), StorageError> {
        self.text.delete_utf16(pos, len)?;
        self.doc.commit();
        Ok(())
    }

    /// 取 `pos` 处的相对光标；`pos` 单位实测为 Unicode scalar。
    pub fn cursor_at_scalar(&self, pos: usize) -> Option<Cursor> {
        self.text.get_cursor(pos, Side::Middle)
    }

    /// 把光标解析成当前版本的 scalar 位置（会随并发编辑移动）。
    pub fn resolve_cursor_scalar(&self, cursor: &Cursor) -> Result<usize, StorageError> {
        self.doc
            .get_cursor_pos(cursor)
            .map(|pos| pos.current.pos)
            .map_err(|error| StorageError::CursorUnresolved(error.to_string()))
    }

    /// 设置**下一次提交**的 origin（撤销分组 / 后台变更排除用）。
    pub fn set_next_commit_origin(&self, origin: &str) {
        self.doc.set_next_commit_origin(origin);
    }

    /// 撤销管理器（本 peer 栈，ADR 0002 §8）。
    pub fn undo_manager(&self) -> UndoManager {
        UndoManager::new(&self.doc)
    }

    /// 当前 frontier。
    pub fn frontiers(&self) -> Frontiers {
        self.doc.state_frontiers()
    }

    /// 当前 oplog 版本向量。
    pub fn vv(&self) -> VersionVector {
        self.doc.oplog_vv()
    }

    /// 自 `from` 之后的新增 ops（A 事务要落库的增量）。
    pub fn pending_update(&self, from: &VersionVector) -> Result<PendingUpdate, StorageError> {
        let payload = self.doc.export(ExportMode::updates(from))?;
        Ok(PendingUpdate {
            payload,
            end_frontier: self.frontiers().encode(),
            end_vv: self.vv().encode(),
        })
    }

    /// 全量历史 update（对端首次同步用）。
    pub fn export_all_updates(&self) -> Result<Vec<u8>, StorageError> {
        Ok(self.doc.export(ExportMode::all_updates())?)
    }

    /// 完整快照。
    pub fn export_snapshot(&self) -> Result<Vec<u8>, StorageError> {
        Ok(self.doc.export(ExportMode::Snapshot)?)
    }

    /// 导入一个 update 或 snapshot（幂等；重复导入不会产生新 ops）。
    pub fn import(&self, bytes: &[u8]) -> Result<(), StorageError> {
        self.doc.import(bytes)?;
        Ok(())
    }

    /// 基线 merge 专用：把增量只合进 **oplog**（detached），不触发 DocState 的
    /// diff 计算与状态应用。
    ///
    /// 语义等价性：CRDT 的 oplog 合并与 state 应用是同一组 op；detached 只跳过
    /// 「把新 op 应用到容器状态」这一步，oplog 的 VV/frontier 与 attached 完全一致。
    /// 调用方只能使用 [`Self::pending_update_from_oplog`] / [`Self::oplog_frontiers_encoded`]
    /// 这类纯 oplog 接口，**不得**在 detached 之后读正文。
    ///
    /// 为什么需要它：对「已有本地内容」的文档，Loro 的 attached import 会重算
    /// diff 并把大文本重新应用到 state，成本随总文本长度超线性增长
    /// （5→20 MiB 实测 ≈15–17×，见 `phase1a_release` 探针）；baseline merge 只需要
    /// 「oplog 并集 + 导出差集」，detached 路径把这部分成本降到 O(增量)。
    pub(crate) fn import_oplog_only(&self, bytes: &[u8]) -> Result<(), StorageError> {
        if !self.doc.is_detached() {
            self.doc.detach();
        }
        self.doc.import(bytes)?;
        Ok(())
    }

    /// detached 模式下合并后的 oplog frontier（attached 等价物 = [`Self::frontiers`]）。
    pub(crate) fn oplog_frontiers_encoded(&self) -> Vec<u8> {
        self.doc.oplog_frontiers().encode()
    }

    /// 从 oplog 导出 `from` 之后的增量；`end_frontier`/`end_vv` 取 **oplog** 版本。
    ///
    /// 仅用于已 [`Self::import_oplog_only`] 的文档；对 attached 文档它与
    /// [`Self::pending_update`] 结果一致（state 在 latest 时两者相同）。
    pub(crate) fn pending_update_from_oplog(
        &self,
        from: &VersionVector,
    ) -> Result<PendingUpdate, StorageError> {
        let payload = self.doc.export(ExportMode::updates(from))?;
        Ok(PendingUpdate {
            payload,
            end_frontier: self.oplog_frontiers_encoded(),
            end_vv: self.doc.oplog_vv().encode(),
        })
    }

    /// 校验 `[pos, pos + len)` 在当前文本内（scalar 口径），避免 Loro 抛出的越界信息丢上下文。
    ///
    /// 坐标错误直接复用 `menghunote_text::CoordError`（ADR 0003：全仓只有一份）。
    fn check_range(&self, pos: usize, len: usize) -> Result<(), StorageError> {
        let total = self.scalar_len();
        let end = pos.checked_add(len).ok_or(CoordError::ScalarOutOfBounds {
            index: usize::MAX,
            len: total,
        })?;
        if end > total {
            return Err(CoordError::ScalarOutOfBounds {
                index: end,
                len: total,
            }
            .into());
        }
        Ok(())
    }
}

`````


## `crates/storage/src/baseline_apply.rs`

`````rust
//! S7-34：已验证基线的分批 merge / 原子推进 cursor（P3-I 前置）。
//!
//! 语义：
//! - 下载/解密/摘要验证在 sync 层完成；本模块只接收已验证的 Loro snapshot 字节。
//! - 每批 docs/tombstones 在**一个 SQLite 事务**里落库；任何失败整批回滚。
//! - snapshot 每篇只解码/导入**一次**（`BodyDoc::restore`），之后的派生全部基于这个副本：
//!   - 新文档：直接 `pending_update(&VersionVector::default())`；
//!   - 已有且无本地额外 op：只读 `load_head` 的 VV，等价（VV 相同）则不写 update；
//!   - 已有且有本地额外 op：只重放一次本地副本，导出**极小**的本地差集并反向合进
//!     基线副本的 **oplog**（detached，不重算 state/diff），再导出差集。
//!     绝不二次 `import(snapshot)`、也不用 `load_body_doc` 只为取 VV。
//! - update 的 `batch_id` 固定为 [`BASELINE_MERGE_BATCH`]（origin 标记）。
//! - 每篇应用的快照 VV（远端已知 ops）写入非破坏性本地 meta
//!   `sync.remote_known_vv.<doc_id>`（见 [`crate::remote_known`]），跨 journal 保留，
//!   供后续 tombstone 判定区分「远端已删的已同步文档」与「local-only 文档」。
//! - `finish` 只做**单调**水位/cursor CAS 与 journal 状态迁移；失败不推进任何 cursor。
//! - `begin` 记录本 checkpoint 的**预期参与集合**（覆盖到的 doc 与资产，存于既有 `meta`
//!   单行、只本地可见）：`finish` 必须先证明集合里每一项都已 `applied=1`，才允许写任何
//!   meta；少跑一批（或跑错集合）返回 [`BaselineApplyFinish::Conflict`]，而不是推进 S
//!   之后让 pull 永久跳过 relay 事件。
//! - tombstone 的 outbox 判定会在**同一写事务内**用 SQL 重判一次：判定完成后被并发写入的
//!   outbox 行只会把 `deleted` 退化为 `kept_local_edit`，绝不误删本地编辑。
//! - `abort` 只清 staging，不删已导入的 Loro/asset 数据，也不回滚远端已知 VV。

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use loro::VersionVector;
use rusqlite::types::Value;
use sha2::{Digest, Sha256};

use crate::actor::{AffectedGuard, Statement};
use crate::body_doc::{BodyDoc, PendingUpdate, APP_DOC_SCHEMA, CODEC_VERSION, MIN_READER};
use crate::error::StorageError;
use crate::remote_known;
use crate::store::{load_body_doc, load_head, HeadState};
use crate::sync_bridge::{
    sync_cursor_state, META_SYNC_CONFIRMED_HIGHWATER, META_SYNC_INTEGRATED_SEQ,
    META_SYNC_RECEIVED_SEQ,
};
use crate::Vault;

/// baseline merge 写入 `loro_updates` 的 batch_id / origin 标记。
pub const BASELINE_MERGE_BATCH: &str = "baseline_merge";
/// relay 页面 cursor 的 meta 键（与 sync `engine::RELAY_CURSOR_META` 对齐）。
pub const META_SYNC_RELAY_CURSOR: &str = "sync.relay_cursor";
/// [`BaselineExpectation`] 的持久化 meta 键（**只本地可见**；不是 schema/wire）。
pub const META_BASELINE_EXPECTED: &str = "sync.baseline_expected";
/// 导入后读回校验失败的持久标记（**只本地可见**；不是 schema/wire）。
///
/// 为什么不是 `baseline_apply.status='failed'`：schema v3 的
/// `CHECK(status IN('staging','completed'))` 不允许新值（见
/// `crates/storage/src/schema/v3.sql`），加状态需要 DDL/迁移，超出同步硬化 P1 的边界。
/// 失败语义改由本 meta 键承载：journal 保持 `staging`（已导入的 Loro/资产数据全部
/// 保留，便于诊断/重跑），但 [`begin_baseline_apply`] 见到该键一律拒绝，必须先
/// [`abort_baseline_apply`] 清理（abort 同时删除本键）。
pub const META_BASELINE_VERIFY_FAILED: &str = "sync.baseline_verify_failed";
/// 预期集合编码的魔数（防止误读旧值/损坏值）。
const EXPECTED_MAGIC: &[u8] = b"MNBASEEXP1";
/// 失败标记编码的魔数（`magic | utf8 reason`，防止把任意 BLOB 误读成失败标记）。
const VERIFY_FAILED_MAGIC: &[u8] = b"MNBASEVERIFYFAIL1";
/// storage 内部构造 BodyDoc 时使用的 peer id（只影响新文档的本地 op id，不影响 import）。
const BASELINE_PEER_ID: u64 = 1;

/// 基线 apply 的分段计时（**仅用于基准/诊断**；按线程累计，不持久化、不参与语义）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BaselineApplyDebugTimings {
    /// `BodyDoc::restore`（含 snapshot 解码/导入）。
    pub restore_ns: u64,
    /// `doc_exists` / `load_head` / 本地 VV 比较。
    pub head_lookup_ns: u64,
    /// `load_body_doc`（本地重放）。
    pub load_body_doc_ns: u64,
    /// 本地极小增量导出（`local_delta = doc.pending_update(&baseline_vv)`）。
    pub local_delta_export_ns: u64,
    /// 本地极小增量反向导入 `baseline_doc`。
    pub local_delta_import_ns: u64,
    /// 旧实现：对本地 doc 二次 `import(snapshot)` 的耗时（优化后应恒为 0）。
    pub snapshot_reimport_ns: u64,
    /// 新文档：`BodyDoc::create` + `import(snapshot)` 的耗时（优化后应恒为 0）。
    pub new_doc_import_ns: u64,
    /// 落盘用的 `pending_update` 导出。
    pub pending_update_ns: u64,
    /// `exec_transaction`。
    pub transaction_ns: u64,
    /// 整个 `apply_baseline_batch`。
    pub batch_total_ns: u64,
    /// 导入后读回校验（sync 的 `verify_imported`）耗时。
    ///
    /// **只读指标**：不参与任何决策；由 sync 的 bootstrap 调用
    /// [`record_baseline_verify_time`] 记录，Phase1B 的 gate_hold 报告可在同一线程
    /// 通过 [`baseline_apply_debug_timings`] 或 [`baseline_apply_verify_ms`] 读取。
    pub verify_ns: u64,
}

/// 基线 apply 的计数（**仅用于基准/诊断**；按线程累计，不持久化）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BaselineApplyDebugCounters {
    pub batches: u64,
    pub docs: u64,
    pub new_docs: u64,
    pub existing_no_extra: u64,
    pub existing_local_extra: u64,
    pub replayed_same_digest: u64,
    pub snapshot_imports: u64,
    pub snapshot_bytes: u64,
    pub load_body_doc_calls: u64,
    pub snapshot_reimport_bytes: u64,
    pub new_doc_import_bytes: u64,
    pub pending_update_calls: u64,
    pub pending_update_bytes: u64,
    pub local_delta_bytes: u64,
}

#[derive(Default)]
struct BaselineDebugAcc {
    timings: BaselineApplyDebugTimings,
    counters: BaselineApplyDebugCounters,
}

thread_local! {
    static BASELINE_DEBUG: RefCell<BaselineDebugAcc> = RefCell::new(BaselineDebugAcc::default());
}

/// 清零当前线程的基线 apply 诊断计数（基准/测试用）。
#[doc(hidden)]
pub fn reset_baseline_apply_debug() {
    BASELINE_DEBUG.with(|acc| *acc.borrow_mut() = BaselineDebugAcc::default());
}

/// 读当前线程的分段计时（基准/测试用）。
#[doc(hidden)]
pub fn baseline_apply_debug_timings() -> BaselineApplyDebugTimings {
    BASELINE_DEBUG.with(|acc| acc.borrow().timings)
}

/// 读当前线程的计数（基准/测试用）。
#[doc(hidden)]
pub fn baseline_apply_debug_counters() -> BaselineApplyDebugCounters {
    BASELINE_DEBUG.with(|acc| acc.borrow().counters)
}

/// 记录一次导入后读回校验（sync 的 `verify_imported`）耗时。
///
/// **只读指标**：不参与任何决策，也不持久化；与其它分段计时一样按线程累计。
/// Phase1B 的 gate_hold 报告可在同一线程读取 [`baseline_apply_debug_timings`] 的
/// `verify_ns`（或 [`baseline_apply_verify_ms`]）。
#[doc(hidden)]
pub fn record_baseline_verify_time(elapsed: Duration) {
    let elapsed = u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX);
    BASELINE_DEBUG.with(|acc| {
        let mut acc = acc.borrow_mut();
        acc.timings.verify_ns = acc.timings.verify_ns.saturating_add(elapsed);
    });
}

/// 当前线程累计的读回校验耗时（毫秒，向下取整；只读指标，不参与决策）。
#[doc(hidden)]
pub fn baseline_apply_verify_ms() -> u64 {
    baseline_apply_debug_timings().verify_ns / 1_000_000
}

fn debug_add_time<F>(field: F, started: Instant)
where
    F: FnOnce(&mut BaselineApplyDebugTimings) -> &mut u64,
{
    let elapsed = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
    BASELINE_DEBUG.with(|acc| *field(&mut acc.borrow_mut().timings) += elapsed);
}

fn debug_add_count<F>(field: F, delta: u64)
where
    F: FnOnce(&mut BaselineApplyDebugCounters) -> &mut u64,
{
    BASELINE_DEBUG.with(|acc| *field(&mut acc.borrow_mut().counters) += delta);
}

/// staging journal 状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaselineApplyStatus {
    Staging,
    Completed,
}

impl BaselineApplyStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Staging => "staging",
            Self::Completed => "completed",
        }
    }

    fn parse(value: &str) -> Result<Self, StorageError> {
        match value {
            "staging" => Ok(Self::Staging),
            "completed" => Ok(Self::Completed),
            other => Err(StorageError::Corrupt {
                context: format!("未知 baseline_apply.status {other:?}"),
            }),
        }
    }
}

/// `baseline_apply_state` 的只读快照。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaselineApplyState {
    pub checkpoint_id: [u8; 16],
    pub baseline_s: u64,
    pub source_cursor: String,
    pub status: BaselineApplyStatus,
    pub applied_docs: u64,
    pub applied_assets: u64,
    pub kept_local_edit: u64,
    /// begin 记录的预期文档数；没有记录（旧 journal/损坏）时为 `None`，此时不可 finish。
    pub expected_docs: Option<u64>,
    /// begin 记录的预期资产数；同上。
    pub expected_assets: Option<u64>,
    /// staging 且 begin 记录的预期集合已全部 `applied=1` 时为 true；finish 还要求 cursor CAS 通过。
    pub finishable: bool,
    /// 所选基线自带的 relay cursor；是否真正写进 `meta` 取决于 finish 的单调判断。
    pub chosen_cursor: Option<String>,
}

/// `finish_baseline_apply` 的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BaselineApplyFinish {
    /// 成功推进 cursor；携带 completed 状态。
    Applied(BaselineApplyState),
    NotFound,
    /// begin 记录的水位/游标已被并发改写、S 不符、或预期参与集合未全部应用；
    /// 未修改任何行。
    Conflict,
}

/// `begin_baseline_apply` 记录的**预期参与集合**：本 checkpoint 覆盖的全部文档与资产。
///
/// 为什么需要：journal 只记录「已应用了多少」，不记录「本来应该应用哪些」。少跑一批
/// （进程被杀、调用方漏批、fallback 选错集）时旧实现仍能 finish 并推进 S，随后普通 pull
/// 会把这段前缀当成已消费，**永久跳过其中所有 relay 事件**。
///
/// 集合以单条 [`META_BASELINE_EXPECTED`] `meta` 行持久化（不新增列/表、不参与同步、不是
/// wire）；`new` 会排序去重，因此同一集合的不同传入顺序被视为同一份预期。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BaselineExpectation {
    docs: Vec<String>,
    assets: Vec<[u8; 32]>,
}

impl BaselineExpectation {
    /// 规范化（排序 + 去重）后构造。
    pub fn new(
        docs: impl IntoIterator<Item = String>,
        assets: impl IntoIterator<Item = [u8; 32]>,
    ) -> Self {
        let mut docs: Vec<String> = docs.into_iter().collect();
        docs.sort_unstable();
        docs.dedup();
        let mut assets: Vec<[u8; 32]> = assets.into_iter().collect();
        assets.sort_unstable();
        assets.dedup();
        Self { docs, assets }
    }

    /// 预期的文档 id（已排序去重）。
    pub fn docs(&self) -> &[String] {
        &self.docs
    }

    /// 预期的资产 object_id（已排序去重）。
    pub fn assets(&self) -> &[[u8; 32]] {
        &self.assets
    }

    /// 预期文档数。
    pub fn doc_count(&self) -> u64 {
        u64::try_from(self.docs.len()).unwrap_or(u64::MAX)
    }

    /// 预期资产数。
    pub fn asset_count(&self) -> u64 {
        u64::try_from(self.assets.len()).unwrap_or(u64::MAX)
    }

    /// 集合是否为空（空集合视为"begin 已显式声明无需应用任何项"）。
    pub fn is_empty(&self) -> bool {
        self.docs.is_empty() && self.assets.is_empty()
    }

    /// 编码成单条 `meta` 值：`magic | doc_count | (len,doc)* | asset_count | oid*`（全部 BE）。
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(
            EXPECTED_MAGIC.len()
                + 16
                + self.docs.iter().map(|doc| 8 + doc.len()).sum::<usize>()
                + self.assets.len() * 32,
        );
        out.extend_from_slice(EXPECTED_MAGIC);
        out.extend_from_slice(&self.doc_count().to_be_bytes());
        for doc in &self.docs {
            let len = u64::try_from(doc.len()).unwrap_or(u64::MAX);
            out.extend_from_slice(&len.to_be_bytes());
            out.extend_from_slice(doc.as_bytes());
        }
        out.extend_from_slice(&self.asset_count().to_be_bytes());
        for asset in &self.assets {
            out.extend_from_slice(asset);
        }
        out
    }

    /// 解码并校验 `encode` 的输出；任何不合法都视为本地 meta 损坏（fail-closed）。
    fn decode(bytes: &[u8]) -> Result<Self, StorageError> {
        let mut offset = 0usize;
        let magic = take_expected_bytes(bytes, &mut offset, EXPECTED_MAGIC.len())?;
        if magic != EXPECTED_MAGIC {
            return Err(corrupt_expected("魔数不符"));
        }
        let doc_count = read_expected_u64(bytes, &mut offset)?;
        let doc_count =
            usize::try_from(doc_count).map_err(|_| corrupt_expected("doc 数超过 usize"))?;
        // 损坏数据不得预分配过大内存。
        let mut docs = Vec::with_capacity(doc_count.min(64));
        for _ in 0..doc_count {
            let len = read_expected_u64(bytes, &mut offset)?;
            let len = usize::try_from(len).map_err(|_| corrupt_expected("doc 长度超过 usize"))?;
            let raw = take_expected_bytes(bytes, &mut offset, len)?;
            let doc = std::str::from_utf8(raw)
                .map_err(|_| corrupt_expected("doc_id 不是 UTF-8"))?
                .to_owned();
            docs.push(doc);
        }
        let asset_count = read_expected_u64(bytes, &mut offset)?;
        let asset_count =
            usize::try_from(asset_count).map_err(|_| corrupt_expected("资产数超过 usize"))?;
        let mut assets = Vec::with_capacity(asset_count.min(64));
        for _ in 0..asset_count {
            let raw = take_expected_bytes(bytes, &mut offset, 32)?;
            let object_id: [u8; 32] = raw
                .try_into()
                .map_err(|_| corrupt_expected("object_id 长度非法"))?;
            assets.push(object_id);
        }
        if offset != bytes.len() {
            return Err(corrupt_expected("尾部有多余字节"));
        }
        Ok(Self { docs, assets })
    }
}

/// 一个已验证的基线资产（HMAC oid 是 relay 对象名；明文 hash 是真实 sha256）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaselineAsset {
    pub object_id: [u8; 32],
    pub asset_hash: [u8; 32],
    pub byte_len: u64,
    pub media_type: Option<String>,
    pub data: Vec<u8>,
}

struct ApplyJournal {
    checkpoint_id: [u8; 16],
    baseline_s: u64,
    source_cursor: String,
    status: BaselineApplyStatus,
    expected_received_seq: u64,
    expected_integrated_seq: u64,
    expected_confirmed_highwater: Option<u64>,
    expected_relay_cursor: Option<Vec<u8>>,
    applied_docs: u64,
    applied_assets: u64,
    kept_local_edit: u64,
    chosen_cursor: Option<String>,
}

struct PreparedDoc {
    doc_id: String,
    snapshot_digest: [u8; 32],
    baseline_vv: Vec<u8>,
    expected_head: Option<HeadState>,
    update: Option<PendingUpdate>,
    /// 读到的旧「远端已知 VV」（值 CAS 的期望值；`None` = 读时不存在）。
    ///
    /// 写入必须走 CAS（P2-5）：本批准备期间，普通 pull 的 A 事务完全可能已经把同一个
    /// `sync.remote_known_vv.<doc_id>` 扩大；无条件覆盖会把它退回到本批的并集（丢失
    /// 远端已知证据 → tombstone 误判为 local-only，只会保守保留，但证据丢了）。
    remote_known_expected: Option<VersionVector>,
    /// 合并后的远端已知 VV（写入 `sync.remote_known_vv.<doc_id>`）。
    remote_known_merged: VersionVector,
}
struct PreparedTombstone {
    doc_id: String,
    action: &'static str,
}

struct PreparedAsset {
    object_id: [u8; 32],
    asset_hash: [u8; 32],
    byte_len: u64,
    statement: Option<Statement>,
}

/// 记录「baseline 导入后的读回校验失败」。
///
/// 只写一条本地 `meta`（BLOB：魔数 + UTF-8 原因），**不改** `baseline_apply.status`
/// （schema v3 的 CHECK 只允许 `staging/completed`，新增 `failed` 需要 DDL/迁移）。
/// journal 与已导入的 Loro/资产数据原样保留：CRDT merge 幂等，回滚已 merge 的数据会
/// 丢掉本地编辑（本地 op 可能已经与基线 ops 合进同一条 update，无法原样拆分）。
///
/// 契约：标记存在时 [`begin_baseline_apply`] 一律拒绝；必须先 [`abort_baseline_apply`]
/// 清理失败 journal（abort 同时删除本标记）。
pub fn mark_baseline_verify_failed(vault: &Vault, reason: &str) -> Result<(), StorageError> {
    let mut value = Vec::with_capacity(VERIFY_FAILED_MAGIC.len() + reason.len());
    value.extend_from_slice(VERIFY_FAILED_MAGIC);
    value.extend_from_slice(reason.as_bytes());
    vault.set_meta(META_BASELINE_VERIFY_FAILED, &value)
}

/// 读失败标记的原因；没有标记返回 `None`。
///
/// 标记存在但编码非法（缺魔数/非 UTF-8）返回 [`StorageError::Corrupt`]，不静默当成
/// 「没有失败」——那会让 `begin` 放行一份未知状态的 journal。
pub fn baseline_verify_failure(vault: &Vault) -> Result<Option<String>, StorageError> {
    let Some(bytes) = vault.get_meta(META_BASELINE_VERIFY_FAILED)? else {
        return Ok(None);
    };
    let reason = bytes
        .strip_prefix(VERIFY_FAILED_MAGIC)
        .ok_or_else(|| StorageError::Corrupt {
            context: format!("{META_BASELINE_VERIFY_FAILED} 编码非法（缺魔数）"),
        })?;
    String::from_utf8(reason.to_vec())
        .map(Some)
        .map_err(|_| StorageError::Corrupt {
            context: format!("{META_BASELINE_VERIFY_FAILED} 原因不是合法 UTF-8"),
        })
}

/// 开始一次 baseline apply staging；同参数（含预期参与集合）重复调用幂等，冲突参数返回
/// SyncConflict。
///
/// `expected` 必须覆盖**本 checkpoint 的全部** docs 与资产：finish 前会拿它逐项核对
/// `baseline_apply_docs/applied=1` 与 `baseline_apply_assets/applied=1`（见
/// [`BaselineExpectation`]）。同一 checkpoint/S/cursor 但 `expected` 不同 → 冲突，
/// 不会静默沿用旧集合。
///
/// 若存在 [`META_BASELINE_VERIFY_FAILED`] 标记（上一次导入读回校验失败），本函数**拒绝**
/// 一切 begin（包括同参数幂等续跑），必须先 [`abort_baseline_apply`]。
pub fn begin_baseline_apply(
    vault: &Vault,
    checkpoint_id: &[u8; 16],
    baseline_s: u64,
    source_cursor: &str,
    expected: &BaselineExpectation,
) -> Result<BaselineApplyState, StorageError> {
    // 失败标记优先于幂等/冲突判断：journal 仍是 staging（schema 不允许新 status），
    // 但语义上已不可续跑；必须先 abort 清理失败 journal 与标记。
    if let Some(reason) = baseline_verify_failure(vault)? {
        return Err(StorageError::SyncConflict {
            context: format!(
                "baseline 导入的读回校验已失败（{META_BASELINE_VERIFY_FAILED}: {reason}）；\
                 已 merge 的 Loro/资产数据仍保留，必须先 abort_baseline_apply 清理失败 journal"
            ),
        });
    }
    let now = now_secs()?;
    let expected_bytes = expected.encode();
    let existing = load_journal(vault)?;
    if let Some(existing) = &existing {
        if existing.status == BaselineApplyStatus::Staging
            && existing.checkpoint_id == *checkpoint_id
            && existing.baseline_s == baseline_s
            && existing.source_cursor == source_cursor
        {
            // 同参数重入必须幂等（崩溃续跑路径）；但预期集合必须对得上：
            // 旧 journal 没有集合记录、或调用方换了集合，都不能静默继续。
            let stored = vault.get_meta(META_BASELINE_EXPECTED)?;
            if stored.as_deref() != Some(expected_bytes.as_slice()) {
                return Err(StorageError::SyncConflict {
                    context: "同一 checkpoint 的 baseline apply staging 已有另一份预期参与集合"
                        .to_owned(),
                });
            }
            return state_from_journal(vault, existing);
        }
        if existing.status == BaselineApplyStatus::Staging {
            return Err(StorageError::SyncConflict {
                context: "已有另一份 baseline apply staging".to_owned(),
            });
        }
    }

    let cursor = sync_cursor_state(vault)?;
    let relay_cursor = vault.get_meta(META_SYNC_RELAY_CURSOR)?;

    let mut statements = Vec::new();
    let mut guards = Vec::new();
    // 旧 journal 子表的「远端已知 VV」证据回填 + 清理。
    //
    // `baseline_apply_docs.baseline_vv` 是**跨 journal 仍然有效**的「远端已知 VV」
    // 证据：必须在删除前落到非破坏性本地 meta 里，否则已有设备下一次快照追赶时会把
    // 远端已删文档误判成 local-only（只会保守保留，但证据丢了）。
    //
    // **无条件执行**（P2-5）：走到这里只可能是「没有 journal」或「completed journal」
    // （staging 同参已幂等早退、异参已 SyncConflict 返回）。completed journal 的
    // baseline_vv 要回填；没有 journal 但残留子表（旧版本/异常写留下的孤儿行）也必须
    // 先回填再清掉，否则下面无条件 `DELETE` 会把证据静默丢弃。
    //
    // `DO NOTHING` 是单调断言：已有 meta（普通 pull/integrate 记录的更新的远端已知 VV）
    // 永远不降级；缺失时才由旧 baseline_vv 补上。
    statements.push(Statement::new(
        format!(
            "INSERT INTO meta(key,value) \
             SELECT '{}' || doc_id, baseline_vv FROM baseline_apply_docs WHERE applied=1 \
             ON CONFLICT(key) DO NOTHING",
            remote_known::META_REMOTE_KNOWN_VV_PREFIX
        ),
        Vec::new(),
    ));
    // 旧 journal 子表只属于旧 journal（completed 或孤儿残留），一律清掉，
    // 避免它们被下一轮 `apply_baseline_batch` 当成「已应用」。
    statements.push(Statement::new(
        "DELETE FROM baseline_apply_docs",
        Vec::new(),
    ));
    statements.push(Statement::new(
        "DELETE FROM baseline_apply_tombstones",
        Vec::new(),
    ));
    statements.push(Statement::new(
        "DELETE FROM baseline_apply_assets",
        Vec::new(),
    ));
    statements.push(Statement::new("DELETE FROM baseline_apply", Vec::new()));
    let insert_index = statements.len();
    statements.push(Statement::new(
        "INSERT INTO baseline_apply(\
           singleton,checkpoint_id,baseline_s,source_cursor,status,\
           expected_received_seq,expected_integrated_seq,expected_confirmed_highwater,\
           expected_relay_cursor,applied_docs,applied_assets,kept_local_edit,chosen_cursor,\
           created_at,updated_at\
         ) VALUES(1,?1,?2,?3,'staging',?4,?5,?6,?7,0,0,0,NULL,?8,?8)",
        vec![
            Value::Blob(checkpoint_id.to_vec()),
            Value::Integer(i64_of(baseline_s)?),
            Value::Text(source_cursor.to_owned()),
            Value::Integer(i64_of(cursor.received_seq)?),
            Value::Integer(i64_of(cursor.integrated_seq)?),
            opt_u64(cursor.confirmed_highwater)?,
            opt_blob(relay_cursor),
            Value::Integer(now),
        ],
    ));
    guards.push(AffectedGuard::exactly(insert_index, 1));

    // 预期参与集合随 journal 同事务落 meta（replace 语义：completed journal 被替换时
    // 旧集合必须一起被覆盖，否则 finish 会拿上一轮的集合核对本轮进度）。
    let expected_index = statements.len();
    statements.push(Statement::new(
        "INSERT INTO meta(key,value) VALUES(?1,?2) \
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        vec![
            Value::Text(META_BASELINE_EXPECTED.to_owned()),
            Value::Blob(expected_bytes),
        ],
    ));
    guards.push(AffectedGuard::exactly(expected_index, 1));

    match vault.exec_transaction(&statements, &guards) {
        Ok(_) => {}
        Err(StorageError::GuardFailed { .. }) => {
            return Err(StorageError::SyncConflict {
                context: "baseline apply staging 已被其它写入占用".to_owned(),
            });
        }
        Err(error) => return Err(error),
    }
    baseline_apply_state(vault)?.ok_or_else(|| StorageError::Corrupt {
        context: "begin_baseline_apply 后 journal 不存在".to_owned(),
    })
}

/// 当前 staging journal 的只读状态；没有 journal 返回 `None`。
pub fn baseline_apply_state(vault: &Vault) -> Result<Option<BaselineApplyState>, StorageError> {
    match load_journal(vault)? {
        None => Ok(None),
        Some(journal) => state_from_journal(vault, &journal).map(Some),
    }
}

/// 应用一批已验证的快照文档和远端 tombstone。
pub fn apply_baseline_batch(
    vault: &Vault,
    docs: &[(String, Vec<u8>)],
    tombstones: &[String],
) -> Result<BaselineApplyState, StorageError> {
    let journal = load_active_journal(vault)?;
    if docs.is_empty() && tombstones.is_empty() {
        return state_from_journal(vault, &journal);
    }

    let mut seen_docs = BTreeSet::new();
    for (doc_id, _) in docs {
        if !seen_docs.insert(doc_id.clone()) {
            return Err(StorageError::Corrupt {
                context: format!("baseline batch 重复 doc {doc_id}"),
            });
        }
    }
    let mut seen_tombstones = BTreeSet::new();
    for doc_id in tombstones {
        if !seen_tombstones.insert(doc_id.clone()) {
            return Err(StorageError::Corrupt {
                context: format!("baseline batch 重复 tombstone {doc_id}"),
            });
        }
    }

    let applied_docs = load_applied_doc_digests(vault)?;
    let mut prepared_docs = Vec::new();
    let mut baseline_by_doc: BTreeMap<String, bool> = BTreeMap::new();
    let batch_started = Instant::now();
    debug_add_count(|c| &mut c.batches, 1);
    for (doc_id, snapshot) in docs {
        debug_add_count(|c| &mut c.docs, 1);
        if snapshot.is_empty() {
            return Err(StorageError::Corrupt {
                context: format!("baseline 文档 {doc_id} 快照为空"),
            });
        }
        let digest: [u8; 32] = Sha256::digest(snapshot).into();

        // 幂等重放：同一快照已应用过。**不再解码/导入 snapshot**，直接用本批已记录的
        // baseline VV 判断 tombstone 需要的 local_extra。
        if let Some(old_digest) = applied_docs.get(doc_id) {
            if old_digest != &digest {
                return Err(StorageError::SyncConflict {
                    context: format!("baseline 文档 {doc_id} 已用另一份快照应用"),
                });
            }
            let baseline_vv = load_applied_baseline_vv(vault, doc_id)?;
            let lookup_started = Instant::now();
            let local_extra = local_has_extra(vault, doc_id, &baseline_vv)?;
            debug_add_time(|t| &mut t.head_lookup_ns, lookup_started);
            debug_add_count(|c| &mut c.replayed_same_digest, 1);
            baseline_by_doc.insert(doc_id.clone(), local_extra);
            continue;
        }

        // snapshot 只解码/导入这一遍：后续所有派生都基于这个 `baseline_doc`。
        let restore_started = Instant::now();
        let baseline_doc = BodyDoc::restore(doc_id.clone(), BASELINE_PEER_ID, Some(snapshot), &[])?;
        debug_add_time(|t| &mut t.restore_ns, restore_started);
        debug_add_count(|c| &mut c.snapshot_imports, 1);
        debug_add_count(|c| &mut c.snapshot_bytes, snapshot.len() as u64);
        let baseline_vv = baseline_doc.vv();

        // 仅取 VV 的地方一律用 O(1) 的 load_head，不重放整篇文档。
        let lookup_started = Instant::now();
        let exists = crate::read_api::doc_exists(vault, doc_id)?;
        let (local_extra, head, local_vv) = if exists {
            let head = load_head(vault, doc_id)?;
            let local_vv = head.version_vector()?;
            let local_extra = local_vv.sub_iter(&baseline_vv).next().is_some();
            (local_extra, Some(head), Some(local_vv))
        } else {
            (false, None, None)
        };
        debug_add_time(|t| &mut t.head_lookup_ns, lookup_started);

        let mut update = None;
        let mut expected_head = None;
        if let (Some(head), Some(local_vv)) = (head, local_vv) {
            if !local_extra {
                debug_add_count(|c| &mut c.existing_no_extra, 1);
                if baseline_vv != local_vv {
                    let export_started = Instant::now();
                    let pending = baseline_doc.pending_update(&local_vv)?;
                    debug_add_time(|t| &mut t.pending_update_ns, export_started);
                    debug_add_count(|c| &mut c.pending_update_calls, 1);
                    debug_add_count(
                        |c| &mut c.pending_update_bytes,
                        pending.payload.len() as u64,
                    );
                    if !pending.payload.is_empty() {
                        update = Some(pending);
                    }
                }
                expected_head = Some(head);
            } else {
                debug_add_count(|c| &mut c.existing_local_extra, 1);
                // 只重放一次本地副本，导出**极小**的本地增量；绝不把大 snapshot 再导入一遍。
                let load_started = Instant::now();
                let (local_doc, loaded_head) =
                    load_body_doc(vault, doc_id, BASELINE_PEER_ID, None)?;
                debug_add_time(|t| &mut t.load_body_doc_ns, load_started);
                debug_add_count(|c| &mut c.load_body_doc_calls, 1);
                // 以本次重放读到的 head/VV 为准：若 load_head 之后有并发写，CAS 与导出基准
                // 都跟着最新值走，不 panic、也不写错 head。
                let local_vv = loaded_head.version_vector()?;
                let local_delta_started = Instant::now();
                let local_delta = local_doc.pending_update(&baseline_vv)?;
                debug_add_time(|t| &mut t.local_delta_export_ns, local_delta_started);
                debug_add_count(
                    |c| &mut c.local_delta_bytes,
                    local_delta.payload.len() as u64,
                );
                let import_started = Instant::now();
                if !local_delta.payload.is_empty() {
                    // 把本地极小增量反向合进基线副本，得到 merged 状态。
                    // 只合 oplog（detached）：attached import 会把大文本重新应用到 state，
                    // 成本随总文本长度超线性增长；这里只需要 oplog 并集。
                    baseline_doc.import_oplog_only(&local_delta.payload)?;
                }
                debug_add_time(|t| &mut t.local_delta_import_ns, import_started);
                let export_started = Instant::now();
                let pending = baseline_doc.pending_update_from_oplog(&local_vv)?;
                debug_add_time(|t| &mut t.pending_update_ns, export_started);
                debug_add_count(|c| &mut c.pending_update_calls, 1);
                debug_add_count(
                    |c| &mut c.pending_update_bytes,
                    pending.payload.len() as u64,
                );
                if !pending.payload.is_empty() {
                    update = Some(pending);
                }
                expected_head = Some(loaded_head);
            }
        } else {
            // 新文档：直接用已 restore 的 baseline_doc 导出相对空 VV 的全量历史。
            debug_add_count(|c| &mut c.new_docs, 1);
            let export_started = Instant::now();
            let pending = baseline_doc.pending_update(&VersionVector::default())?;
            debug_add_time(|t| &mut t.pending_update_ns, export_started);
            debug_add_count(|c| &mut c.pending_update_calls, 1);
            debug_add_count(
                |c| &mut c.pending_update_bytes,
                pending.payload.len() as u64,
            );
            if !pending.payload.is_empty() {
                update = Some(pending);
            }
        }

        let remote_known_expected = remote_known::load(vault, doc_id)?;
        let remote_known_merged = match &remote_known_expected {
            Some(existing) => remote_known::union(existing, &baseline_vv),
            None => baseline_vv.clone(),
        };
        prepared_docs.push(PreparedDoc {
            doc_id: doc_id.clone(),
            snapshot_digest: digest,
            baseline_vv: baseline_vv.encode(),
            expected_head,
            update,
            remote_known_expected,
            remote_known_merged,
        });
        baseline_by_doc.insert(doc_id.clone(), local_extra);
    }

    let mut prepared_tombstones = Vec::new();
    for doc_id in tombstones {
        let action = if tombstone_can_delete(vault, doc_id, baseline_by_doc.get(doc_id).copied())? {
            "deleted"
        } else {
            "kept_local_edit"
        };
        prepared_tombstones.push(PreparedTombstone {
            doc_id: doc_id.clone(),
            action,
        });
    }

    let now = now_secs()?;
    let mut statements = Vec::new();
    let mut guards = Vec::new();
    // 远端已知 VV 的 CAS 守卫下标 → doc_id：GuardFailed 时用它给出明确冲突错误。
    let mut remote_known_guards: Vec<(usize, String)> = Vec::new();
    for prepared in &prepared_docs {
        let journal_index = statements.len();
        statements.push(Statement::new(
            "INSERT INTO baseline_apply_docs(doc_id,snapshot_digest,baseline_vv,applied,updated_at) \
             VALUES(?1,?2,?3,1,?4) \
             ON CONFLICT(doc_id) DO UPDATE SET \
               snapshot_digest=excluded.snapshot_digest,baseline_vv=excluded.baseline_vv,applied=1,updated_at=excluded.updated_at",
            vec![
                Value::Text(prepared.doc_id.clone()),
                Value::Blob(prepared.snapshot_digest.to_vec()),
                Value::Blob(prepared.baseline_vv.clone()),
                Value::Integer(now),
            ],
        ));
        guards.push(AffectedGuard::exactly(journal_index, 1));

        // 跨 journal 保留的「远端已知 VV」：本批快照的 VV 就是远端已见过的 ops。
        // 与旧值取单调并集（旧值可能来自普通 pull/integrate 的更新远端已知集），
        // 但写入必须是**值 CAS**：只有库中值仍是准备阶段读到的旧值才更新。
        // 并发写者（普通 pull 的 A 事务）已经扩大过 → 0 行 → 整批回滚并报明确冲突，
        // 由调用方重试；绝不把更大的值覆盖回本批的并集（P2-5）。
        let remote_index = statements.len();
        statements.push(remote_known::cas_statement(
            &prepared.doc_id,
            prepared.remote_known_expected.as_ref(),
            &prepared.remote_known_merged,
        ));
        guards.push(AffectedGuard::exactly(remote_index, 1));
        remote_known_guards.push((remote_index, prepared.doc_id.clone()));

        let Some(update) = &prepared.update else {
            if prepared.expected_head.is_none() {
                // 新文档但快照没有 op：仍建行，保持空文档语义。
                statements.push(Statement::new(
                    "INSERT INTO loro_docs(doc_id,kind,generation,app_doc_schema,codec_version,min_reader,head_seq) \
                     VALUES(?1,'body',1,?2,?3,?4,0)",
                    vec![
                        Value::Text(prepared.doc_id.clone()),
                        Value::Integer(APP_DOC_SCHEMA),
                        Value::Text(CODEC_VERSION.to_owned()),
                        Value::Integer(MIN_READER),
                    ],
                ));
                guards.push(AffectedGuard::exactly(statements.len() - 1, 1));
            }
            continue;
        };

        if let Some(head) = &prepared.expected_head {
            let seq = head.head_seq.saturating_add(1);
            let cas_index = statements.len();
            statements.push(Statement::new(
                "UPDATE loro_docs SET head_seq=?2,head_frontier=?3,head_vv=?4 \
                 WHERE doc_id=?1 AND head_seq=?5",
                vec![
                    Value::Text(prepared.doc_id.clone()),
                    Value::Integer(i64_of(seq)?),
                    Value::Blob(update.end_frontier.clone()),
                    Value::Blob(update.end_vv.clone()),
                    Value::Integer(i64_of(head.head_seq)?),
                ],
            ));
            guards.push(AffectedGuard::exactly(cas_index, 1));
            let insert_index = statements.len();
            statements.push(Statement::new(
                "INSERT INTO loro_updates(doc_id,seq,update_id,batch_id,codec_version,payload,end_frontier,end_vv) \
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                vec![
                    Value::Text(prepared.doc_id.clone()),
                    Value::Integer(i64_of(seq)?),
                    Value::Text(format!("{}#{seq}", prepared.doc_id)),
                    Value::Text(BASELINE_MERGE_BATCH.to_owned()),
                    Value::Text(CODEC_VERSION.to_owned()),
                    Value::Blob(update.payload.clone()),
                    Value::Blob(update.end_frontier.clone()),
                    Value::Blob(update.end_vv.clone()),
                ],
            ));
            guards.push(AffectedGuard::exactly(insert_index, 1));
            let outbox_index = statements.len();
            statements.push(Statement::new(
                "INSERT INTO projection_outbox(doc_id,target_seq) VALUES(?1,?2) \
                 ON CONFLICT(doc_id) DO UPDATE SET target_seq=MAX(target_seq,excluded.target_seq)",
                vec![
                    Value::Text(prepared.doc_id.clone()),
                    Value::Integer(i64_of(seq)?),
                ],
            ));
            guards.push(AffectedGuard::exactly(outbox_index, 1));
        } else {
            let kind = if prepared.doc_id == crate::structure::STRUCTURE_DOC_ID {
                "structure"
            } else {
                "body"
            };
            let insert_index = statements.len();
            statements.push(Statement::new(
                "INSERT INTO loro_docs(doc_id,kind,generation,app_doc_schema,codec_version,min_reader,head_seq,head_frontier,head_vv) \
                 VALUES(?1,?2,1,?3,?4,?5,1,?6,?7)",
                vec![
                    Value::Text(prepared.doc_id.clone()),
                    Value::Text(kind.to_owned()),
                    Value::Integer(APP_DOC_SCHEMA),
                    Value::Text(CODEC_VERSION.to_owned()),
                    Value::Integer(MIN_READER),
                    Value::Blob(update.end_frontier.clone()),
                    Value::Blob(update.end_vv.clone()),
                ],
            ));
            guards.push(AffectedGuard::exactly(insert_index, 1));
            let update_index = statements.len();
            statements.push(Statement::new(
                "INSERT INTO loro_updates(doc_id,seq,update_id,batch_id,codec_version,payload,end_frontier,end_vv) \
                 VALUES(?1,1,?2,?3,?4,?5,?6,?7)",
                vec![
                    Value::Text(prepared.doc_id.clone()),
                    Value::Text(format!("{}#1", prepared.doc_id)),
                    Value::Text(BASELINE_MERGE_BATCH.to_owned()),
                    Value::Text(CODEC_VERSION.to_owned()),
                    Value::Blob(update.payload.clone()),
                    Value::Blob(update.end_frontier.clone()),
                    Value::Blob(update.end_vv.clone()),
                ],
            ));
            guards.push(AffectedGuard::exactly(update_index, 1));
            let outbox_index = statements.len();
            statements.push(Statement::new(
                "INSERT INTO projection_outbox(doc_id,target_seq) VALUES(?1,1) \
                 ON CONFLICT(doc_id) DO UPDATE SET target_seq=MAX(target_seq,excluded.target_seq)",
                vec![Value::Text(prepared.doc_id.clone())],
            ));
            guards.push(AffectedGuard::exactly(outbox_index, 1));
        }
    }

    for tombstone in &prepared_tombstones {
        // P1-2（TOCTOU）：`tombstone_can_delete` 在事务外读 outbox，判定完成后本地编辑的
        // A 事务完全可能插进一条 `sync_outbox` 行；若仍按旧判定无条件软删，就会把刚编辑
        // 的笔记删掉。因此最终 action 必须在**同一个写事务里**用 SQL 重判一次：
        // 只允许 `deleted → kept_local_edit` 的降级，绝不允许 `kept → deleted` 升级。
        let index = statements.len();
        statements.push(Statement::new(
            "INSERT INTO baseline_apply_tombstones(doc_id,action,updated_at) \
             VALUES(?1, CASE WHEN EXISTS(SELECT 1 FROM sync_outbox WHERE doc_id=?1) \
                             THEN 'kept_local_edit' ELSE ?2 END, ?3) \
             ON CONFLICT(doc_id) DO UPDATE SET \
               action=CASE WHEN EXISTS(SELECT 1 FROM sync_outbox WHERE doc_id=?1) \
                           THEN 'kept_local_edit' ELSE excluded.action END, \
               updated_at=excluded.updated_at",
            vec![
                Value::Text(tombstone.doc_id.clone()),
                Value::Text(tombstone.action.to_owned()),
                Value::Integer(now),
            ],
        ));
        // 这一句永远恰好写入 1 行（INSERT 或 DO UPDATE），是"判定已落库"的守卫：
        // 说明事务内重判真的执行了，而不是被别的写入/约束挡掉。
        guards.push(AffectedGuard::exactly(index, 1));

        if tombstone.action == "deleted" {
            // 软删与 outbox 判定在同一事务内：只有重判仍为 `deleted`（且此刻没有
            // outbox）才置 deleted=1。0 行是合法结果（该文档没有 notes 行，或已降级为
            // kept_local_edit），所以这句本身不设 exactly 守卫；守卫语义在上一条。
            statements.push(Statement::new(
                "UPDATE notes SET deleted=1 \
                 WHERE body_doc_id=?1 \
                   AND NOT EXISTS(SELECT 1 FROM sync_outbox WHERE doc_id=?1) \
                   AND EXISTS(SELECT 1 FROM baseline_apply_tombstones \
                              WHERE doc_id=?1 AND action='deleted')",
                vec![Value::Text(tombstone.doc_id.clone())],
            ));
        }
    }

    let journal_index = statements.len();
    statements.push(Statement::new(
        "UPDATE baseline_apply SET \
           applied_docs=(SELECT COUNT(*) FROM baseline_apply_docs WHERE applied=1), \
           applied_assets=(SELECT COUNT(*) FROM baseline_apply_assets WHERE applied=1), \
           kept_local_edit=(SELECT COUNT(*) FROM baseline_apply_tombstones WHERE action='kept_local_edit'), \
           updated_at=?1 \
         WHERE singleton=1 AND status='staging'",
        vec![Value::Integer(now)],
    ));
    guards.push(AffectedGuard::exactly(journal_index, 1));

    let transaction_started = Instant::now();
    match vault.exec_transaction(&statements, &guards) {
        Ok(_) => {}
        Err(StorageError::GuardFailed { statement, .. }) => {
            if let Some((_, doc_id)) = remote_known_guards
                .iter()
                .find(|(index, _)| *index == statement)
            {
                // 值 CAS 不命中：准备阶段读到的旧值已被普通 pull 的 A 事务改写。
                // 整批已回滚（update/head/journal 都没有落库），重跑本批即可读到并集。
                return Err(StorageError::SyncConflict {
                    context: format!(
                        "baseline 文档 {doc_id} 的远端已知 VV 值 CAS 失败：\
                         并发写者已改写旧值；整批已回滚，可重试"
                    ),
                });
            }
            return Err(StorageError::SyncConflict {
                context: "baseline batch 期间 journal/doc head 已变化".to_owned(),
            });
        }
        Err(error) => return Err(error),
    }
    debug_add_time(|t| &mut t.transaction_ns, transaction_started);
    debug_add_time(|t| &mut t.batch_total_ns, batch_started);
    baseline_apply_state(vault)?.ok_or_else(|| StorageError::Corrupt {
        context: "apply_baseline_batch 后 journal 不存在".to_owned(),
    })
}

/// 应用一批已验证的基线资产；同一 HMAC oid/真实 hash 重复应用幂等。
pub fn apply_baseline_assets(
    vault: &Vault,
    assets: &[BaselineAsset],
) -> Result<BaselineApplyState, StorageError> {
    let journal = load_active_journal(vault)?;
    if assets.is_empty() {
        return state_from_journal(vault, &journal);
    }

    let mut seen_oid = BTreeSet::new();
    let mut seen_hash = BTreeSet::new();
    let mut prepared = Vec::with_capacity(assets.len());
    for asset in assets {
        if asset.data.is_empty() {
            return Err(StorageError::Corrupt {
                context: "baseline 资产明文为空".to_owned(),
            });
        }
        if asset.byte_len != asset.data.len() as u64 {
            return Err(StorageError::Corrupt {
                context: format!(
                    "baseline 资产 {} byte_len={} 与 data.len={} 不一致",
                    hex32(&asset.object_id),
                    asset.byte_len,
                    asset.data.len()
                ),
            });
        }
        if !seen_oid.insert(asset.object_id) {
            return Err(StorageError::Corrupt {
                context: format!("baseline batch 重复 object_id {}", hex32(&asset.object_id)),
            });
        }
        if !seen_hash.insert(asset.asset_hash) {
            return Err(StorageError::Corrupt {
                context: format!(
                    "baseline batch 重复 asset_hash {}",
                    hex32(&asset.asset_hash)
                ),
            });
        }

        if let Some((old_hash, old_len)) = load_applied_asset(vault, &asset.object_id)? {
            if old_hash != asset.asset_hash || old_len != asset.byte_len {
                return Err(StorageError::SyncConflict {
                    context: format!(
                        "baseline 资产 object {} 已绑定另一份资产",
                        hex32(&asset.object_id)
                    ),
                });
            }
            continue;
        }

        let statement = prepare_asset_statement(vault, asset)?;
        prepared.push(PreparedAsset {
            object_id: asset.object_id,
            asset_hash: asset.asset_hash,
            byte_len: asset.byte_len,
            statement,
        });
    }

    let now = now_secs()?;
    let mut statements = Vec::new();
    let mut guards = Vec::new();
    for asset in &prepared {
        if let Some(statement) = &asset.statement {
            let index = statements.len();
            statements.push(statement.clone());
            guards.push(AffectedGuard::exactly(index, 1));
        }
        let index = statements.len();
        statements.push(Statement::new(
            "INSERT INTO baseline_apply_assets(object_id,asset_hash,byte_len,applied,updated_at) \
             VALUES(?1,?2,?3,1,?4) \
             ON CONFLICT(object_id) DO UPDATE SET \
               asset_hash=excluded.asset_hash,byte_len=excluded.byte_len,applied=1,updated_at=excluded.updated_at",
            vec![
                Value::Blob(asset.object_id.to_vec()),
                Value::Blob(asset.asset_hash.to_vec()),
                Value::Integer(i64_of(asset.byte_len)?),
                Value::Integer(now),
            ],
        ));
        guards.push(AffectedGuard::exactly(index, 1));
    }
    let journal_index = statements.len();
    statements.push(Statement::new(
        "UPDATE baseline_apply SET \
           applied_docs=(SELECT COUNT(*) FROM baseline_apply_docs WHERE applied=1), \
           applied_assets=(SELECT COUNT(*) FROM baseline_apply_assets WHERE applied=1), \
           kept_local_edit=(SELECT COUNT(*) FROM baseline_apply_tombstones WHERE action='kept_local_edit'), \
           updated_at=?1 \
         WHERE singleton=1 AND status='staging'",
        vec![Value::Integer(now)],
    ));
    guards.push(AffectedGuard::exactly(journal_index, 1));

    match vault.exec_transaction(&statements, &guards) {
        Ok(_) => {}
        Err(StorageError::GuardFailed { .. }) => {
            return Err(StorageError::SyncConflict {
                context: "baseline assets 写入期间 journal 已变化".to_owned(),
            });
        }
        Err(error) => return Err(error),
    }
    baseline_apply_state(vault)?.ok_or_else(|| StorageError::Corrupt {
        context: "apply_baseline_assets 后 journal 不存在".to_owned(),
    })
}

/// 全部批次完成后按**单调语义**推进水位与 relay cursor；CAS 失败返回 Conflict 且不写任何行。
///
/// 评审 0002 P1-4：bootstrap/rotation 会作用在**已有库**上，所选基线 S 可能低于本库已有
/// 水位。因此这里只允许前进（[`sync_cursor_state`] 的字段语义）：
///
/// - `received/integrated/confirmed_highwater = max(现有, S)`；
/// - relay cursor 只在 `S >= 现有 integrated`（基线覆盖了已消费前缀）时切换为基线 cursor，
///   否则保持原游标，绝不把已消费位置回退；
/// - 本函数只写上述四个 `meta` 键与 `baseline_apply` journal。sync 侧的 pin
///   （instance/log_id/epoch/control rev+hash）由 sync 模块维护，这里不读不写，
///   所以不可能被降低；不能把它们搬进本函数。
///
/// CAS 仍是"begin 时记录、finish 时校验"：任一字段在 finish 前被并发改写即返回
/// [`BaselineApplyFinish::Conflict`]，整笔事务不写。
///
/// **完整性前提**（P1-1）：begin 记录在 [`META_BASELINE_EXPECTED`] 的预期参与集合必须
/// 全部 `applied=1`。少跑一批（哪怕 status 仍是 staging）返回 Conflict；旧 journal
/// 没有集合记录同样 Conflict（拿不到证明就不允许推进 S），绝不明知不完整还写水位。
pub fn finish_baseline_apply(
    vault: &Vault,
    chosen_baseline_s: u64,
    cursor: &str,
) -> Result<BaselineApplyFinish, StorageError> {
    let journal = match load_journal(vault)? {
        None => return Ok(BaselineApplyFinish::NotFound),
        Some(journal) => journal,
    };
    if journal.status != BaselineApplyStatus::Staging || journal.baseline_s != chosen_baseline_s {
        return Ok(BaselineApplyFinish::Conflict);
    }

    // 完整性校验在任何写语句之前：拿不到 begin 的预期集合，或集合未全齐，一律 Conflict。
    let Some(expected) = load_expected_set(vault)? else {
        return Ok(BaselineApplyFinish::Conflict);
    };
    if !expected_is_complete(vault, &journal, &expected)? {
        return Ok(BaselineApplyFinish::Conflict);
    }

    // finish 前先校验 begin 时记录的旧值；任何一项被并发改写都按 Conflict 处理，不写任何行。
    let current = sync_cursor_state(vault)?;
    let current_relay_cursor = vault.get_meta(META_SYNC_RELAY_CURSOR)?;
    if current.received_seq != journal.expected_received_seq
        || current.integrated_seq != journal.expected_integrated_seq
        || current.confirmed_highwater != journal.expected_confirmed_highwater
        || current_relay_cursor != journal.expected_relay_cursor
    {
        return Ok(BaselineApplyFinish::Conflict);
    }

    // 单调目标：已有水位高于基线 S 时保留已有值。
    let received_target = current.received_seq.max(chosen_baseline_s);
    let integrated_target = current.integrated_seq.max(chosen_baseline_s);
    let highwater_target = current
        .confirmed_highwater
        .unwrap_or(0)
        .max(chosen_baseline_s);
    // 基线 cursor 指向位置 S；只有它覆盖了已 integrate 的前缀才允许切换。
    let advance_cursor = chosen_baseline_s >= current.integrated_seq;

    let now = now_secs()?;
    let mut statements = Vec::new();
    let mut guards = Vec::new();
    push_meta_cas(
        &mut statements,
        &mut guards,
        META_SYNC_RECEIVED_SEQ,
        current.received_seq,
        &received_target.to_be_bytes(),
    );
    push_meta_cas(
        &mut statements,
        &mut guards,
        META_SYNC_INTEGRATED_SEQ,
        current.integrated_seq,
        &integrated_target.to_be_bytes(),
    );
    push_meta_cas(
        &mut statements,
        &mut guards,
        META_SYNC_CONFIRMED_HIGHWATER,
        current.confirmed_highwater.unwrap_or(0),
        &highwater_target.to_be_bytes(),
    );
    match (&current_relay_cursor, advance_cursor) {
        // 基线覆盖已消费前缀：切到基线 cursor。
        (Some(old), true) => push_meta_cas_bytes(
            &mut statements,
            &mut guards,
            META_SYNC_RELAY_CURSOR,
            old,
            cursor.as_bytes(),
        ),
        // 低 S：原样写回旧游标，既保持语义也保留 CAS（并发改写 → Conflict）。
        (Some(old), false) => push_meta_cas_bytes(
            &mut statements,
            &mut guards,
            META_SYNC_RELAY_CURSOR,
            old,
            old,
        ),
        (None, true) => push_meta_absent_insert(
            &mut statements,
            &mut guards,
            META_SYNC_RELAY_CURSOR,
            cursor.as_bytes(),
        ),
        // 低 S 且原本没有游标：保持缺失，并发插入同样按 Conflict 处理。
        (None, false) => {
            let index = statements.len();
            statements.push(Statement::new(
                "UPDATE meta SET value=value WHERE key=?1",
                vec![Value::Text(META_SYNC_RELAY_CURSOR.to_owned())],
            ));
            guards.push(AffectedGuard::exactly(index, 0));
        }
    }

    let journal_index = statements.len();
    statements.push(Statement::new(
        "UPDATE baseline_apply SET status='completed',chosen_cursor=?1,updated_at=?2 \
         WHERE singleton=1 AND status='staging'",
        vec![Value::Text(cursor.to_owned()), Value::Integer(now)],
    ));
    guards.push(AffectedGuard::exactly(journal_index, 1));

    match vault.exec_transaction(&statements, &guards) {
        Ok(_) => {}
        Err(StorageError::GuardFailed { .. }) => return Ok(BaselineApplyFinish::Conflict),
        Err(error) => return Err(error),
    }
    let state = baseline_apply_state(vault)?.ok_or_else(|| StorageError::Corrupt {
        context: "finish_baseline_apply 后 journal 不存在".to_owned(),
    })?;
    Ok(BaselineApplyFinish::Applied(state))
}

/// 只清 staging journal（含子表、预期参与集合与失败标记）；已导入的 Loro update/资产保留。
///
/// 没有 staging journal 时仍会清掉可能残留的 [`META_BASELINE_VERIFY_FAILED`]（abort 是
/// 失败标记的唯一清理入口），并返回 `false` 表示没有 journal 可中止。
pub fn abort_baseline_apply(vault: &Vault) -> Result<bool, StorageError> {
    match load_journal(vault)? {
        Some(journal) if journal.status == BaselineApplyStatus::Staging => {}
        _ => {
            // completed journal 的数据不动；只清失败标记（若存在），保持返回 false 的既有语义。
            let _cleared = vault.exec(
                "DELETE FROM meta WHERE key=?1",
                &[Value::Text(META_BASELINE_VERIFY_FAILED.to_owned())],
            )?;
            return Ok(false);
        }
    }
    let statements = [
        Statement::new("DELETE FROM baseline_apply_docs", Vec::new()),
        Statement::new("DELETE FROM baseline_apply_tombstones", Vec::new()),
        Statement::new("DELETE FROM baseline_apply_assets", Vec::new()),
        Statement::new(
            "DELETE FROM baseline_apply WHERE singleton=1 AND status='staging'",
            Vec::new(),
        ),
        // 预期集合只属于被清掉的 journal；留着会被下一次 begin 的 replace 覆盖，
        // 但清掉可以避免"没有 journal 却有预期集合"的中间态被误读。
        Statement::new(
            "DELETE FROM meta WHERE key=?1",
            vec![Value::Text(META_BASELINE_EXPECTED.to_owned())],
        ),
        // 失败标记随失败 journal 一起清：abort 之后才允许重新 begin。
        Statement::new(
            "DELETE FROM meta WHERE key=?1",
            vec![Value::Text(META_BASELINE_VERIFY_FAILED.to_owned())],
        ),
    ];
    match vault.exec_transaction(&statements, &[AffectedGuard::exactly(3, 1)]) {
        Ok(_) => Ok(true),
        Err(StorageError::GuardFailed { .. }) => Ok(false),
        Err(error) => Err(error),
    }
}

fn tombstone_can_delete(
    vault: &Vault,
    doc_id: &str,
    baseline_in_batch: Option<bool>,
) -> Result<bool, StorageError> {
    // 本地仍待同步的 outbox 行：无条件保留（可能是用户尚未上传的编辑）。
    let outbox_rows = vault.query(
        "SELECT count(*) FROM sync_outbox WHERE doc_id=?1",
        &[Value::Text(doc_id.to_owned())],
    )?;
    let has_outbox = outbox_rows
        .first()
        .and_then(|row| row.first())
        .and_then(|value| match value {
            Value::Integer(value) => Some(*value > 0),
            _ => None,
        })
        .unwrap_or(false);
    if has_outbox {
        return Ok(false);
    }

    if !crate::read_api::doc_exists(vault, doc_id)? {
        return Ok(true);
    }
    match baseline_in_batch {
        // 有快照且本地没有快照之外的 ops：可安全删除。
        Some(false) => return Ok(true),
        // 有快照且本地有快照之外的 ops：保留。
        Some(true) => return Ok(false),
        None => {}
    }

    // 没有本批快照：用**跨 journal 保留**的「远端已知 VV」判断本地有没有远端未知的 op。
    //
    // - 普通 pull/integrate 集成过、`head_seq>0`、`sync_outbox` 为空 → 本地 ops 全被
    //   远端已知 VV 覆盖 → `deleted`；
    // - local-only 文档（从未远程同步、无任何证据）→ 没有记录 → 保守保留；
    // - 有本地未上传/未被远端确认的 op → `local_vv` 超出已知集 → 保留。
    let head = load_head(vault, doc_id)?;
    let local_vv = head.version_vector()?;
    if let Some(known) = remote_known::load(vault, doc_id)? {
        let known_all = local_vv.sub_iter(&known).next().is_none();
        return Ok(known_all);
    }

    // 兜底：当前 journal 里已经应用过该 doc（例如 meta 写入之前的历史数据）。
    // 只读取 VV，不再重放整篇文档。
    if let Some(baseline) = load_applied_baseline_vv_opt(vault, doc_id)? {
        let known = local_vv.sub_iter(&baseline).next().is_none();
        return Ok(known);
    }

    // 完全没有证据：local-only 内容必须保留；只有从未有过 op 的空文档才可删除。
    Ok(head.head_seq == 0)
}

/// 读当前 journal 中某篇已应用文档的 baseline VV（幂等重放分支用；缺失视为损坏）。
fn load_applied_baseline_vv(vault: &Vault, doc_id: &str) -> Result<VersionVector, StorageError> {
    load_applied_baseline_vv_opt(vault, doc_id)?.ok_or_else(|| StorageError::Corrupt {
        context: format!("baseline_apply_docs 缺少已应用文档 {doc_id} 的 baseline_vv"),
    })
}

/// 读当前 journal 中某篇已应用文档的 baseline VV；没有该行返回 `None`。
fn load_applied_baseline_vv_opt(
    vault: &Vault,
    doc_id: &str,
) -> Result<Option<VersionVector>, StorageError> {
    let rows = vault.query(
        "SELECT baseline_vv FROM baseline_apply_docs WHERE doc_id=?1 AND applied=1",
        &[Value::Text(doc_id.to_owned())],
    )?;
    match rows.first().and_then(|row| row.first()) {
        Some(Value::Blob(baseline_vv)) => VersionVector::decode(baseline_vv)
            .map(Some)
            .map_err(StorageError::from),
        Some(other) => Err(StorageError::Corrupt {
            context: format!("baseline_apply_docs.baseline_vv 读回异常：{other:?}"),
        }),
        None => Ok(None),
    }
}

/// 本地文档是否有 baseline 之外的 op（只看 head VV，O(1)，不重放正文）。
fn local_has_extra(
    vault: &Vault,
    doc_id: &str,
    baseline_vv: &VersionVector,
) -> Result<bool, StorageError> {
    if !crate::read_api::doc_exists(vault, doc_id)? {
        return Ok(false);
    }
    let local_vv = load_head(vault, doc_id)?.version_vector()?;
    let extra = local_vv.sub_iter(baseline_vv).next().is_some();
    Ok(extra)
}

fn prepare_asset_statement(
    vault: &Vault,
    asset: &BaselineAsset,
) -> Result<Option<Statement>, StorageError> {
    let rows = vault.query(
        "SELECT byte_len,data,media_type FROM assets WHERE asset_hash=?1",
        &[Value::Blob(asset.asset_hash.to_vec())],
    )?;
    let Some(row) = rows.first() else {
        return Ok(Some(Statement::new(
            "INSERT INTO assets(asset_hash,byte_len,media_type,data,orphan_since) \
             VALUES(?1,?2,?3,?4,NULL)",
            vec![
                Value::Blob(asset.asset_hash.to_vec()),
                Value::Integer(i64_of(asset.byte_len)?),
                opt_text(asset.media_type.clone()),
                Value::Blob(asset.data.clone()),
            ],
        )));
    };
    let old_len = u64_of(row, 0, "assets.byte_len")?;
    let old_data = optional_blob(row, 1)?;
    if old_len != asset.byte_len {
        return Err(StorageError::SyncConflict {
            context: format!("baseline 资产 {} byte_len 不一致", hex32(&asset.asset_hash)),
        });
    }
    match old_data {
        Some(bytes) if bytes == asset.data => Ok(None),
        Some(_) => Err(StorageError::SyncConflict {
            context: format!("baseline 资产 {} 明文不一致", hex32(&asset.asset_hash)),
        }),
        None => Ok(Some(Statement::new(
            "UPDATE assets SET data=?2 WHERE asset_hash=?1 AND data IS NULL",
            vec![
                Value::Blob(asset.asset_hash.to_vec()),
                Value::Blob(asset.data.clone()),
            ],
        ))),
    }
}

fn load_applied_asset(
    vault: &Vault,
    object_id: &[u8; 32],
) -> Result<Option<([u8; 32], u64)>, StorageError> {
    let rows = vault.query(
        "SELECT asset_hash,byte_len FROM baseline_apply_assets WHERE object_id=?1",
        &[Value::Blob(object_id.to_vec())],
    )?;
    rows.first()
        .map(|row| {
            Ok((
                array32(row, 0, "baseline_apply_assets.asset_hash")?,
                u64_of(row, 1, "baseline_apply_assets.byte_len")?,
            ))
        })
        .transpose()
}

fn load_applied_doc_digests(vault: &Vault) -> Result<BTreeMap<String, [u8; 32]>, StorageError> {
    let rows = vault.query(
        "SELECT doc_id,snapshot_digest FROM baseline_apply_docs WHERE applied=1",
        &[],
    )?;
    rows.iter()
        .map(|row| {
            Ok((
                text_of(row, 0, "baseline_apply_docs.doc_id")?,
                array32(row, 1, "baseline_apply_docs.snapshot_digest")?,
            ))
        })
        .collect()
}

/// 读 begin 记录的预期参与集合；缺失返回 `None`（旧 journal/异常，按不可 finish 处理）。
fn load_expected_set(vault: &Vault) -> Result<Option<BaselineExpectation>, StorageError> {
    match vault.get_meta(META_BASELINE_EXPECTED)? {
        None => Ok(None),
        Some(bytes) => BaselineExpectation::decode(&bytes).map(Some),
    }
}

/// 预期集合是否已全部 `applied=1`。
///
/// 先比计数（每批都会更新 `applied_docs/applied_assets`，因此未齐时 O(1) 就能拒绝），
/// 计数相等后再逐项确认集合身份：光比数量会放过"漏一篇 + 多跑一篇"的情况。计数相等
/// 且预期项全部在已应用集合中 ⇒ 两个集合相等。
fn expected_is_complete(
    vault: &Vault,
    journal: &ApplyJournal,
    expected: &BaselineExpectation,
) -> Result<bool, StorageError> {
    if journal.applied_docs != expected.doc_count()
        || journal.applied_assets != expected.asset_count()
    {
        return Ok(false);
    }
    if expected.is_empty() {
        return Ok(true);
    }
    let applied_docs: BTreeSet<String> = load_applied_doc_digests(vault)?.into_keys().collect();
    let docs_complete = expected
        .docs()
        .iter()
        .all(|doc_id| applied_docs.contains(doc_id));
    if !docs_complete {
        return Ok(false);
    }
    let applied_assets = load_applied_asset_ids(vault)?;
    Ok(expected
        .assets()
        .iter()
        .all(|object_id| applied_assets.contains(object_id)))
}

/// 当前 journal 中 `applied=1` 的资产 object_id 集合。
fn load_applied_asset_ids(vault: &Vault) -> Result<BTreeSet<[u8; 32]>, StorageError> {
    let rows = vault.query(
        "SELECT object_id FROM baseline_apply_assets WHERE applied=1",
        &[],
    )?;
    rows.iter()
        .map(|row| array32(row, 0, "baseline_apply_assets.object_id"))
        .collect()
}

fn corrupt_expected(reason: &str) -> StorageError {
    StorageError::Corrupt {
        context: format!("meta {META_BASELINE_EXPECTED} 非法：{reason}"),
    }
}

fn take_expected_bytes<'a>(
    bytes: &'a [u8],
    offset: &mut usize,
    len: usize,
) -> Result<&'a [u8], StorageError> {
    let end = offset
        .checked_add(len)
        .ok_or_else(|| corrupt_expected("长度溢出"))?;
    let slice = bytes
        .get(*offset..end)
        .ok_or_else(|| corrupt_expected("长度越界"))?;
    *offset = end;
    Ok(slice)
}

fn read_expected_u64(bytes: &[u8], offset: &mut usize) -> Result<u64, StorageError> {
    let raw = take_expected_bytes(bytes, offset, 8)?;
    let mut value = [0u8; 8];
    value.copy_from_slice(raw);
    Ok(u64::from_be_bytes(value))
}

fn load_active_journal(vault: &Vault) -> Result<ApplyJournal, StorageError> {
    let journal = load_journal(vault)?.ok_or_else(|| StorageError::SyncConflict {
        context: "没有 active baseline apply".to_owned(),
    })?;
    if journal.status != BaselineApplyStatus::Staging {
        return Err(StorageError::SyncConflict {
            context: "baseline apply 已完成，不能再应用批次".to_owned(),
        });
    }
    Ok(journal)
}

fn load_journal(vault: &Vault) -> Result<Option<ApplyJournal>, StorageError> {
    let rows = vault.query(
        "SELECT checkpoint_id,baseline_s,source_cursor,status,\
                expected_received_seq,expected_integrated_seq,expected_confirmed_highwater,\
                expected_relay_cursor,applied_docs,applied_assets,kept_local_edit,chosen_cursor \
         FROM baseline_apply WHERE singleton=1",
        &[],
    )?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    Ok(Some(ApplyJournal {
        checkpoint_id: array16(row, 0, "baseline_apply.checkpoint_id")?,
        baseline_s: u64_of(row, 1, "baseline_apply.baseline_s")?,
        source_cursor: text_of(row, 2, "baseline_apply.source_cursor")?,
        status: BaselineApplyStatus::parse(&text_of(row, 3, "baseline_apply.status")?)?,
        expected_received_seq: u64_of(row, 4, "baseline_apply.expected_received_seq")?,
        expected_integrated_seq: u64_of(row, 5, "baseline_apply.expected_integrated_seq")?,
        expected_confirmed_highwater: optional_u64(
            row,
            6,
            "baseline_apply.expected_confirmed_highwater",
        )?,
        expected_relay_cursor: optional_blob(row, 7)?,
        applied_docs: u64_of(row, 8, "baseline_apply.applied_docs")?,
        applied_assets: u64_of(row, 9, "baseline_apply.applied_assets")?,
        kept_local_edit: u64_of(row, 10, "baseline_apply.kept_local_edit")?,
        chosen_cursor: match row.get(11) {
            Some(Value::Text(value)) => Some(value.clone()),
            Some(Value::Null) | None => None,
            other => {
                return Err(StorageError::Corrupt {
                    context: format!("baseline_apply.chosen_cursor 读回异常：{other:?}"),
                });
            }
        },
    }))
}

fn state_from_journal(
    vault: &Vault,
    journal: &ApplyJournal,
) -> Result<BaselineApplyState, StorageError> {
    let expected = load_expected_set(vault)?;
    let (expected_docs, expected_assets) = match &expected {
        Some(expected) => (Some(expected.doc_count()), Some(expected.asset_count())),
        None => (None, None),
    };
    // finishable 必须反映**真实就绪**：仅 status=staging 不够，还要 begin 记录的预期
    // 集合全部 applied=1（否则 finish 会 Conflict，不能对 UI/续跑逻辑谎报可完成）。
    let finishable = journal.status == BaselineApplyStatus::Staging
        && match &expected {
            Some(expected) => expected_is_complete(vault, journal, expected)?,
            None => false,
        };
    Ok(BaselineApplyState {
        checkpoint_id: journal.checkpoint_id,
        baseline_s: journal.baseline_s,
        source_cursor: journal.source_cursor.clone(),
        status: journal.status,
        applied_docs: journal.applied_docs,
        applied_assets: journal.applied_assets,
        kept_local_edit: journal.kept_local_edit,
        expected_docs,
        expected_assets,
        finishable,
        chosen_cursor: journal.chosen_cursor.clone(),
    })
}

fn push_meta_cas(
    statements: &mut Vec<Statement>,
    guards: &mut Vec<AffectedGuard>,
    key: &str,
    old: u64,
    new: &[u8],
) {
    let index = statements.len();
    if old == 0 {
        // 初始 cursor 在 meta 中可能不存在；0 与“缺失”语义等价。
        statements.push(Statement::new(
            "INSERT INTO meta(key,value) VALUES(?1,?2) \
             ON CONFLICT(key) DO UPDATE SET value=excluded.value \
             WHERE meta.value=?3",
            vec![
                Value::Text(key.to_owned()),
                Value::Blob(new.to_vec()),
                Value::Blob(old.to_be_bytes().to_vec()),
            ],
        ));
    } else {
        statements.push(Statement::new(
            "UPDATE meta SET value=?3 WHERE key=?1 AND value=?2",
            vec![
                Value::Text(key.to_owned()),
                Value::Blob(old.to_be_bytes().to_vec()),
                Value::Blob(new.to_vec()),
            ],
        ));
    }
    guards.push(AffectedGuard::exactly(index, 1));
}

fn push_meta_cas_bytes(
    statements: &mut Vec<Statement>,
    guards: &mut Vec<AffectedGuard>,
    key: &str,
    old: &[u8],
    new: &[u8],
) {
    let index = statements.len();
    statements.push(Statement::new(
        "UPDATE meta SET value=?3 WHERE key=?1 AND value=?2",
        vec![
            Value::Text(key.to_owned()),
            Value::Blob(old.to_vec()),
            Value::Blob(new.to_vec()),
        ],
    ));
    guards.push(AffectedGuard::exactly(index, 1));
}

fn push_meta_absent_insert(
    statements: &mut Vec<Statement>,
    guards: &mut Vec<AffectedGuard>,
    key: &str,
    new: &[u8],
) {
    let index = statements.len();
    statements.push(Statement::new(
        "INSERT INTO meta(key,value) SELECT ?1,?2 \
         WHERE NOT EXISTS(SELECT 1 FROM meta WHERE key=?1)",
        vec![Value::Text(key.to_owned()), Value::Blob(new.to_vec())],
    ));
    guards.push(AffectedGuard::exactly(index, 1));
}

fn now_secs() -> Result<i64, StorageError> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| StorageError::Corrupt {
            context: format!("系统时间早于 UNIX_EPOCH：{error}"),
        })?;
    i64::try_from(now.as_secs()).map_err(|_| StorageError::Corrupt {
        context: "系统时间超过 i64".to_owned(),
    })
}

fn i64_of(value: u64) -> Result<i64, StorageError> {
    i64::try_from(value).map_err(|_| StorageError::Corrupt {
        context: format!("{value} 超过 i64::MAX"),
    })
}

fn opt_u64(value: Option<u64>) -> Result<Value, StorageError> {
    match value {
        None => Ok(Value::Null),
        Some(value) => Ok(Value::Integer(i64_of(value)?)),
    }
}

fn opt_blob(value: Option<Vec<u8>>) -> Value {
    match value {
        None => Value::Null,
        Some(value) => Value::Blob(value),
    }
}

fn opt_text(value: Option<String>) -> Value {
    match value {
        None => Value::Null,
        Some(value) => Value::Text(value),
    }
}

fn optional_u64(row: &crate::Row, index: usize, field: &str) -> Result<Option<u64>, StorageError> {
    match row.get(index) {
        Some(Value::Null) | None => Ok(None),
        Some(Value::Integer(value)) => {
            u64::try_from(*value)
                .map(Some)
                .map_err(|_| StorageError::Corrupt {
                    context: format!("{field} 为负：{value}"),
                })
        }
        other => Err(StorageError::Corrupt {
            context: format!("{field} 读回异常：{other:?}"),
        }),
    }
}

fn optional_blob(row: &crate::Row, index: usize) -> Result<Option<Vec<u8>>, StorageError> {
    match row.get(index) {
        Some(Value::Null) | None => Ok(None),
        Some(Value::Blob(value)) => Ok(Some(value.clone())),
        other => Err(StorageError::Corrupt {
            context: format!("blob 列读回异常：{other:?}"),
        }),
    }
}

fn text_of(row: &crate::Row, index: usize, field: &str) -> Result<String, StorageError> {
    match row.get(index) {
        Some(Value::Text(value)) => Ok(value.clone()),
        other => Err(StorageError::Corrupt {
            context: format!("{field} 应为 TEXT，实际 {other:?}"),
        }),
    }
}

fn u64_of(row: &crate::Row, index: usize, field: &str) -> Result<u64, StorageError> {
    match row.get(index) {
        Some(Value::Integer(value)) => u64::try_from(*value).map_err(|_| StorageError::Corrupt {
            context: format!("{field} 为负：{value}"),
        }),
        other => Err(StorageError::Corrupt {
            context: format!("{field} 应为 INTEGER，实际 {other:?}"),
        }),
    }
}

fn array16(row: &crate::Row, index: usize, field: &str) -> Result<[u8; 16], StorageError> {
    let bytes = blob_at(row, index, field)?;
    bytes
        .try_into()
        .map_err(|bytes: Vec<u8>| StorageError::Corrupt {
            context: format!("{field} 长度应为 16，实际 {}", bytes.len()),
        })
}

fn array32(row: &crate::Row, index: usize, field: &str) -> Result<[u8; 32], StorageError> {
    let bytes = blob_at(row, index, field)?;
    bytes
        .try_into()
        .map_err(|bytes: Vec<u8>| StorageError::Corrupt {
            context: format!("{field} 长度应为 32，实际 {}", bytes.len()),
        })
}

fn blob_at(row: &crate::Row, index: usize, field: &str) -> Result<Vec<u8>, StorageError> {
    match row.get(index) {
        Some(Value::Blob(value)) => Ok(value.clone()),
        other => Err(StorageError::Corrupt {
            context: format!("{field} 应为 BLOB，实际 {other:?}"),
        }),
    }
}

fn hex32(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

`````


## `crates/storage/src/store.rs`

`````rust
//! `loro_docs` / `loro_updates` 的读写与 A 事务。
//!
//! A 事务（ADR 0001 §3.4）在一次 SQLite 事务里完成三件事：
//!
//! 1. 逐个 append `loro_updates`（seq 连续，`update_id` 唯一）；
//! 2. 以 CAS 方式推进 `loro_docs.head_seq / head_frontier / head_vv`；
//! 3. 写 `projection_outbox.target_seq`（提醒 B 物化器）。
//!
//! CAS 用「该语句必须影响 1 行」的事务守卫实现：一旦 head 已被别人推进，
//! 整笔事务回滚，**不会出现「update 落库了但 head 没动」的半截状态**。

use loro::{Frontiers, VersionVector};
use rusqlite::types::Value;

use crate::actor::{AffectedGuard, Statement};
use crate::body_doc::{BodyDoc, PendingUpdate, APP_DOC_SCHEMA, CODEC_VERSION, MIN_READER};
use crate::error::StorageError;
use crate::Vault;

/// 文档在 `loro_docs` 里的权威行摘要。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadState {
    /// 文档 ID。
    pub doc_id: String,
    /// 已持久化的最后一个 batch seq（0 表示还没有任何 update）。
    pub head_seq: u64,
    /// 已持久化的 frontier（`Frontiers::encode`）。
    pub head_frontier: Vec<u8>,
    /// 已持久化的 VV（`VersionVector::encode`）。
    pub head_vv: Vec<u8>,
}

impl HeadState {
    /// 解码 frontier。
    pub fn frontiers(&self) -> Result<Frontiers, StorageError> {
        if self.head_frontier.is_empty() {
            return Ok(Frontiers::default());
        }
        Frontiers::decode(&self.head_frontier).map_err(StorageError::from)
    }

    /// 解码 VV。全新的文档 `head_vv` 是 NULL/空，等价于空版本向量。
    pub fn version_vector(&self) -> Result<loro::VersionVector, StorageError> {
        if self.head_vv.is_empty() {
            return Ok(loro::VersionVector::default());
        }
        loro::VersionVector::decode(&self.head_vv).map_err(StorageError::from)
    }
}

/// `loro_docs` 的一行（含快照）。
struct StoredDoc {
    snapshot: Option<Vec<u8>>,
    snapshot_seq: u64,
    head: HeadState,
}

/// 新建一个正文文档行（`kind = 'body'`）。
pub fn create_body_doc(vault: &Vault, doc_id: &str) -> Result<(), StorageError> {
    vault.exec(
        "INSERT INTO loro_docs(doc_id, kind, generation, app_doc_schema, codec_version, min_reader) \
         VALUES(?1, 'body', 1, ?2, ?3, ?4)",
        &[
            Value::Text(doc_id.to_owned()),
            Value::Integer(APP_DOC_SCHEMA),
            Value::Text(CODEC_VERSION.to_owned()),
            Value::Integer(MIN_READER),
        ],
    )?;
    Ok(())
}

/// 为一篇笔记准备好投影行，关联它的正文文档。
///
/// 注意：`notes` 是**投影**（ADR 0001 §3.1），最终由结构 CRDT 的投影器写入；
/// 本函数只是 S0 阶段的脚手架，让两阶段物化有落点，后续切片会由投影器接管。
pub fn create_note_slot(
    vault: &Vault,
    note_id: &str,
    body_doc_id: &str,
    title: &str,
    created_at: i64,
) -> Result<(), StorageError> {
    vault.exec(
        "INSERT INTO notes(note_id, body_doc_id, title, created_at, updated_at, deleted, is_favorite, is_pinned) \
         VALUES(?1, ?2, ?3, ?4, ?4, 0, 0, 0)",
        &[
            Value::Text(note_id.to_owned()),
            Value::Text(body_doc_id.to_owned()),
            Value::Text(title.to_owned()),
            Value::Integer(created_at),
        ],
    )?;
    Ok(())
}

/// 读 `loro_docs` 的 head 三件套；行不存在时报 [`StorageError::Corrupt`]。
pub fn load_head(vault: &Vault, doc_id: &str) -> Result<HeadState, StorageError> {
    let rows = vault.query(
        "SELECT head_seq, head_frontier, head_vv FROM loro_docs WHERE doc_id = ?1",
        &[Value::Text(doc_id.to_owned())],
    )?;
    let row = rows
        .into_iter()
        .next()
        .ok_or_else(|| StorageError::Corrupt {
            context: format!("loro_docs 缺少文档 {doc_id}"),
        })?;

    Ok(HeadState {
        doc_id: doc_id.to_owned(),
        head_seq: u64_of(&row, 0, "loro_docs.head_seq")?,
        head_frontier: blob_of(&row, 1, "loro_docs.head_frontier")?,
        head_vv: blob_of(&row, 2, "loro_docs.head_vv")?,
    })
}

/// 读 `loro_docs.head_seq` 仍等于 `expected_head_seq` 时的 `head_vv`（远端已知 VV 的起点）。
///
/// 若 head 已被并发推进（CAS 必然失败、整笔事务回滚），返回空 VV 即可：它不会落库。
fn load_head_vv_at(
    vault: &Vault,
    doc_id: &str,
    expected_head_seq: u64,
) -> Result<VersionVector, StorageError> {
    let rows = vault.query(
        "SELECT head_seq, head_vv FROM loro_docs WHERE doc_id = ?1",
        &[Value::Text(doc_id.to_owned())],
    )?;
    let row = rows
        .into_iter()
        .next()
        .ok_or_else(|| StorageError::Corrupt {
            context: format!("loro_docs 缺少文档 {doc_id}"),
        })?;
    if u64_of(&row, 0, "loro_docs.head_seq")? != expected_head_seq {
        return Ok(VersionVector::default());
    }
    match row.get(1) {
        Some(Value::Null) | None => Ok(VersionVector::default()),
        Some(Value::Blob(bytes)) => VersionVector::decode(bytes).map_err(StorageError::from),
        Some(other) => Err(StorageError::Corrupt {
            context: format!("loro_docs.head_vv 读回异常：{other:?}"),
        }),
    }
}

/// 读文档行 + 快照元信息。
fn load_stored(vault: &Vault, doc_id: &str) -> Result<StoredDoc, StorageError> {
    let rows = vault.query(
        "SELECT snapshot, snapshot_seq, head_seq, head_frontier, head_vv FROM loro_docs WHERE doc_id = ?1",
        &[Value::Text(doc_id.to_owned())],
    )?;
    let row = rows
        .into_iter()
        .next()
        .ok_or_else(|| StorageError::Corrupt {
            context: format!("loro_docs 缺少文档 {doc_id}"),
        })?;

    Ok(StoredDoc {
        snapshot: optional_blob(&row, 0)?,
        snapshot_seq: u64_of(&row, 1, "loro_docs.snapshot_seq")?,
        head: HeadState {
            doc_id: doc_id.to_owned(),
            head_seq: u64_of(&row, 2, "loro_docs.head_seq")?,
            head_frontier: blob_of(&row, 3, "loro_docs.head_frontier")?,
            head_vv: blob_of(&row, 4, "loro_docs.head_vv")?,
        },
    })
}

/// 库里的 frontier 可能是 NULL（从未提交），语义等价于空 frontier。
///
/// 注意：`Frontiers::default().encode()` **不是**空字节，所以不能直接比较原始 bytes。
pub(crate) fn encoded_frontier(head: &HeadState) -> Vec<u8> {
    if head.head_frontier.is_empty() {
        Frontiers::default().encode()
    } else {
        head.head_frontier.clone()
    }
}

/// 重放所需材料：快照 + 后续 update payload + 目标 head。
pub(crate) struct ReplayParts {
    pub(crate) snapshot: Option<Vec<u8>>,
    pub(crate) payloads: Vec<Vec<u8>>,
    pub(crate) head: HeadState,
}

/// 读取重放材料（快照 + 连续 update 前缀），并校验 seq 连续、不越过快照边界。
///
/// `up_to_seq = Some(n)` 只重放到第 n 个 batch（固定 frontier）；`None` 重放到 head。
pub(crate) fn load_replay_parts(
    vault: &Vault,
    doc_id: &str,
    up_to_seq: Option<u64>,
) -> Result<ReplayParts, StorageError> {
    let stored = load_stored(vault, doc_id)?;
    let limit = match up_to_seq {
        Some(seq) => {
            if seq > stored.head.head_seq {
                return Err(StorageError::Corrupt {
                    context: format!(
                        "请求重放到 seq {seq}，但 {doc_id} 的 head_seq 只有 {}",
                        stored.head.head_seq
                    ),
                });
            }
            seq
        }
        None => stored.head.head_seq,
    };
    if limit < stored.snapshot_seq {
        return Err(StorageError::Corrupt {
            context: format!(
                "请求重放到 seq {limit}，但快照已经覆盖到 seq {}；不能倒着物化",
                stored.snapshot_seq
            ),
        });
    }

    let rows = vault.query(
        "SELECT seq, payload FROM loro_updates WHERE doc_id = ?1 AND seq > ?2 AND seq <= ?3 ORDER BY seq",
        &[
            Value::Text(doc_id.to_owned()),
            Value::Integer(i64_of(stored.snapshot_seq)?),
            Value::Integer(i64_of(limit)?),
        ],
    )?;

    let mut payloads = Vec::with_capacity(rows.len());
    for (expected_seq, row) in (stored.snapshot_seq + 1..).zip(rows.iter()) {
        let seq = u64_of(row, 0, "loro_updates.seq")?;
        if seq != expected_seq {
            return Err(StorageError::Corrupt {
                context: format!(
                    "{doc_id} 的 loro_updates 出现断档：期望 seq {expected_seq}，实际 {seq}"
                ),
            });
        }
        payloads.push(blob_of(row, 1, "loro_updates.payload")?);
    }

    let snapshot = if stored.snapshot_seq == 0 {
        None
    } else {
        stored.snapshot.clone()
    };

    let mut head = stored.head;
    if let Some(limit) = up_to_seq {
        head = HeadState {
            doc_id: doc_id.to_owned(),
            head_seq: limit,
            head_frontier: end_frontier_at(vault, doc_id, limit)?,
            head_vv: end_vv_at(vault, doc_id, limit)?,
        };
    }

    Ok(ReplayParts {
        snapshot,
        payloads,
        head,
    })
}

/// 从库里重放出一个 [`BodyDoc`]。
///
/// `up_to_seq = Some(n)` 时只重放到第 n 个 batch，用于**固定 frontier** 的 B 物化；
/// `None` 表示重放到当前 head，并在结尾校验 frontier 与库中记录一致（不一致说明数据损坏）。
pub fn load_body_doc(
    vault: &Vault,
    doc_id: &str,
    peer_id: u64,
    up_to_seq: Option<u64>,
) -> Result<(BodyDoc, HeadState), StorageError> {
    let parts = load_replay_parts(vault, doc_id, up_to_seq)?;
    let doc = BodyDoc::restore(doc_id, peer_id, parts.snapshot.as_deref(), &parts.payloads)?;

    if up_to_seq.is_none() {
        // 启动校验：重放结果必须与库中 head_frontier 一致，否则视为损坏（不回写、不猜）
        let replayed = doc.frontiers().encode();
        let expected = encoded_frontier(&parts.head);
        if replayed != expected {
            return Err(StorageError::Corrupt {
                context: format!(
                    "重放 {doc_id} 得到的 frontier 与 loro_docs.head_frontier 不一致（len {} vs {}）",
                    replayed.len(),
                    expected.len()
                ),
            });
        }
    }

    Ok((doc, parts.head))
}

/// 读某个 batch 落库时的 `end_vv`。
pub fn end_vv_at(vault: &Vault, doc_id: &str, seq: u64) -> Result<Vec<u8>, StorageError> {
    let rows = vault.query(
        "SELECT end_vv FROM loro_updates WHERE doc_id = ?1 AND seq = ?2",
        &[Value::Text(doc_id.to_owned()), Value::Integer(i64_of(seq)?)],
    )?;
    let row = rows
        .into_iter()
        .next()
        .ok_or_else(|| StorageError::Corrupt {
            context: format!("{doc_id} 缺少 seq {seq} 的 loro_updates 行"),
        })?;
    blob_of(&row, 0, "loro_updates.end_vv")
}

/// 读某个 batch 落库时的 `end_frontier`（B 物化的固定 frontier）。
pub fn end_frontier_at(vault: &Vault, doc_id: &str, seq: u64) -> Result<Vec<u8>, StorageError> {
    let rows = vault.query(
        "SELECT end_frontier FROM loro_updates WHERE doc_id = ?1 AND seq = ?2",
        &[Value::Text(doc_id.to_owned()), Value::Integer(i64_of(seq)?)],
    )?;
    let row = rows
        .into_iter()
        .next()
        .ok_or_else(|| StorageError::Corrupt {
            context: format!("{doc_id} 缺少 seq {seq} 的 loro_updates 行"),
        })?;
    blob_of(&row, 0, "loro_updates.end_frontier")
}

/// A 事务里要写入的一条 update（带它自己的 `batch_id`）。
///
/// 一次 flush 的连续前缀可能来自不同逻辑批次（例如慢盘合并了两次提交），
/// 所以 `batch_id` 是**逐行**的，不能整批共用一个。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateRecord {
    /// 该 update 所属的逻辑批次。
    pub batch_id: String,
    /// 增量内容。
    pub update: PendingUpdate,
}

impl UpdateRecord {
    /// 构造一条记录。
    pub fn new(batch_id: impl Into<String>, update: PendingUpdate) -> Self {
        Self {
            batch_id: batch_id.into(),
            update,
        }
    }
}

/// 把一份快照落库，记录它覆盖到的 seq 与 frontier。
///
/// 本切片只提供落库与「snapshot + 后续 update」重放路径；快照的生成节奏与旧 update 的
/// 真正 GC（ADR 0001 §3.5）留给后续切片。快照不能倒退覆盖更早的 seq。
pub fn store_snapshot(
    vault: &Vault,
    doc_id: &str,
    seq: u64,
    frontier: &[u8],
    snapshot: &[u8],
) -> Result<(), StorageError> {
    if seq == 0 || seq > load_head(vault, doc_id)?.head_seq {
        return Err(StorageError::Corrupt {
            context: format!("快照 seq {seq} 不在 (0, head] 区间内"),
        });
    }
    let affected = vault.exec(
        "UPDATE loro_docs SET snapshot = ?2, snapshot_seq = ?3, snapshot_frontier = ?4 \
         WHERE doc_id = ?1 AND snapshot_seq <= ?3",
        &[
            Value::Text(doc_id.to_owned()),
            Value::Blob(snapshot.to_vec()),
            Value::Integer(i64_of(seq)?),
            Value::Blob(frontier.to_vec()),
        ],
    )?;
    if affected != 1 {
        return Err(StorageError::Corrupt {
            context: format!("{doc_id} 的快照 seq 倒退，拒绝覆盖"),
        });
    }
    Ok(())
}

/// A 事务：追加一批 update + 推进 head + 写 projection_outbox。
///
/// `expected_head_seq` 是调用方看到的 head（CAS 基准）；不匹配时整笔回滚并返回
/// [`StorageError::HeadConflict`]，绝不覆盖别人的新结果。
pub fn append_updates(
    vault: &Vault,
    doc_id: &str,
    expected_head_seq: u64,
    records: &[UpdateRecord],
) -> Result<HeadState, StorageError> {
    append_updates_inner(
        vault,
        doc_id,
        expected_head_seq,
        records,
        &[],
        &[],
        OriginMarks::AllLocal,
    )
}

/// A 事务：追加**一条**组合 update，并把同一批 remote inbox 行标为 `integrated`。
///
/// Phase 1A-2 的聚合 integrate 入口：同一文档的多条 inbox 行先在内存里逐条
/// `BodyDoc::import`，再把 `before..end` 的增量合成**一条** `UpdateRecord` 落盘；
/// 「追加 update + 推进 head + projection_outbox + 标记 inbox 行」全部在**同一个**
/// A 事务里（守卫要求每条 `UPDATE sync_inbox` 恰好影响 1 行，任何一行不存在/已不是
/// `verified` 都会整笔回滚，不会出现「update 写了但 inbox 没标」的半份状态）。
///
/// 与 [`append_updates`] 的差别只有同事务的批量标记：
/// - `relay_seqs` 为空时行为与 [`append_updates`] 逐字节一致（仍然只有一条记录）；
/// - 本函数**不推进** `sync.integrated_seq`；调用方在整批正文都完成后统一调用
///   [`crate::advance_integrated_prefix`]，避免每个 doc 组各写一次前缀；
/// - 校验失败映射：head CAS 失败仍是 [`StorageError::HeadConflict`]；inbox 标记失败
///   （行不存在 / 已 integrated / 已 quarantine）映射为 [`StorageError::SyncConflict`]，
///   两种情况下 update 都没有落库；
/// - 同事务把 `end_vv - 旧 head_vv`（= 这条远端消息实际带来的 ops）并入本机的
///   「远端已知 VV」（`sync.remote_known_vv.<doc_id>`，non-destructive meta），
///   供 `baseline_apply` 的 tombstone 判定区分已同步文档与 local-only 文档。
pub fn append_updates_with_integrated_marks(
    vault: &Vault,
    doc_id: &str,
    expected_head_seq: u64,
    record: &UpdateRecord,
    log_id: &[u8; 16],
    relay_seqs: &[u64],
) -> Result<HeadState, StorageError> {
    let marks: Vec<(&[u8; 16], u64)> = relay_seqs
        .iter()
        .map(|relay_seq| (log_id, *relay_seq))
        .collect();
    append_updates_inner(
        vault,
        doc_id,
        expected_head_seq,
        std::slice::from_ref(record),
        &[],
        &marks,
        // engine integrate 的这条组合 update 全部来自远端消息：其 `end_vv - 旧 head_vv`
        // 就是远端实际带来的 ops，记入「远端已知 VV」（tombstone 判定用）。
        OriginMarks::PerRecord(&[true]),
    )
}

/// 一次 A 事务里每条 update 的**来源标记**。
///
/// 这是 P2-8 修掉的类型陷阱：旧接口用 `&[bool]` 表示来源，**空切片 = 全部本地**；
/// 而同一次调用的 `slots` 也是切片，**空切片 = 没有任何出站意图**。两个空切片含义
/// 相反、类型相同，调用点很容易把「没有意图」写成「全部本地」或反之。这里把来源标记
/// 收成枚举，让「空」不再是两种含义：
///
/// - [`OriginMarks::AllLocal`]：本批记录全部是本地写入，**不**扩大远端已知 VV；
/// - [`OriginMarks::PerRecord`]：与 `records` **一一对应**（长度不匹配直接报
///   [`StorageError::Corrupt`]），`true` = 该条来自远端消息，其
///   `end_vv - 前一条 end_vv` 并入「远端已知 VV」。
///
/// 本地写入即使已经 ACK 也不会扩大远端已知 VV（tombstone 判定只允许更保守）。
#[derive(Debug, Clone, Copy)]
pub enum OriginMarks<'a> {
    /// 全部记录都是本地写入。
    AllLocal,
    /// 逐条标记（必须与 records 等长）。
    PerRecord(&'a [bool]),
}

impl<'a> OriginMarks<'a> {
    /// 本批是否有远端条目。
    fn any_remote(self) -> bool {
        match self {
            Self::AllLocal => false,
            Self::PerRecord(marks) => marks.iter().any(|flag| *flag),
        }
    }

    /// 第 `index` 条是否来自远端（`AllLocal` 恒为 false）。
    fn is_remote(self, index: usize) -> bool {
        match self {
            Self::AllLocal => false,
            Self::PerRecord(marks) => marks.get(index).copied().unwrap_or(false),
        }
    }

    /// 校验来源标记与 records 的对应关系。
    ///
    /// `AllLocal` 不需要长度；`PerRecord` 必须与 records 等长——**空 records 配非空
    /// marks、或 records 配空 marks 都会在这里被拒绝**，不会退化成「全部本地」。
    fn validate(self, records_len: usize) -> Result<(), StorageError> {
        if let Self::PerRecord(marks) = self {
            if marks.len() != records_len {
                return Err(StorageError::Corrupt {
                    context: format!(
                        "append_updates: records={records_len} \
                         OriginMarks::PerRecord={} 必须一对一",
                        marks.len()
                    ),
                });
            }
        }
        Ok(())
    }
}

/// A 事务内同时写出站同步意图（按位置一一对应；`None` = 该条不产生意图）。
///
/// 与 [`append_updates_with_sync_intents`] 相比，这里允许"某些 update 没有意图"：
/// 一次 flush 的前缀可能混本地编辑与 remote 导入，而 remote 导入**绝不能**产生
/// 出站意图（ADR 0009）。仍然只有**一条** A 事务，失败即整笔回滚。
///
/// 参数语义（P2-8 钉死）：
/// - `slots` 空 = 本批**没有任何**出站意图（不是"每条都要一条意图"）；`None` 位 = 该条
///   不产生意图；
/// - `origin` 见 [`OriginMarks`]：只有远端条目会扩大远端已知 VV，本地条目永远不扩大。
pub(crate) fn append_updates_with_intent_slots(
    vault: &Vault,
    doc_id: &str,
    expected_head_seq: u64,
    records: &[UpdateRecord],
    slots: &[Option<crate::sync_bridge::SyncIntent>],
    origin: OriginMarks<'_>,
) -> Result<HeadState, StorageError> {
    append_updates_inner(
        vault,
        doc_id,
        expected_head_seq,
        records,
        slots,
        &[],
        origin,
    )
}

/// 同事务附加的 inbox 标记：`(log_id, relay_seq)`，每个都要求恰好命中一行 `verified`。
type IntegratedMarks<'a> = &'a [(&'a [u8; 16], u64)];

fn append_updates_inner(
    vault: &Vault,
    doc_id: &str,
    expected_head_seq: u64,
    records: &[UpdateRecord],
    slots: &[Option<crate::sync_bridge::SyncIntent>],
    integrated_marks: IntegratedMarks<'_>,
    origin: OriginMarks<'_>,
) -> Result<HeadState, StorageError> {
    if records.is_empty() {
        if !slots.is_empty() {
            return Err(StorageError::Corrupt {
                context: "空 update 批次不能带 sync intent".to_owned(),
            });
        }
        if !integrated_marks.is_empty() {
            return Err(StorageError::Corrupt {
                context: "空 update 批次不能带 inbox 标记".to_owned(),
            });
        }
        // `PerRecord` 配空 records 时，非空标记同样算长度不匹配（不会退化成全部本地）。
        origin.validate(0)?;
        return load_head(vault, doc_id);
    }
    if !slots.is_empty() && slots.len() != records.len() {
        return Err(StorageError::Corrupt {
            context: format!(
                "append_updates: records={} intents={} 必须一对一",
                records.len(),
                slots.len()
            ),
        });
    }
    origin.validate(records.len())?;

    // 远端已知 VV 的增量：只有 remote 条目会扩大它（`after_vv - running_vv`）。
    //
    // 这里只算「本批新增了哪些远端 ops」；旧值读取与 CAS 语句在构造语句阶段生成，
    // 重试时按最新旧值重算并集，但 `added` 本身不变（head 基准没变）。
    let remote_added = if origin.any_remote() {
        let mut running = load_head_vv_at(vault, doc_id, expected_head_seq)?;
        let mut known = VersionVector::new();
        for (index, record) in records.iter().enumerate() {
            let after = VersionVector::decode(&record.update.end_vv).map_err(StorageError::from)?;
            if origin.is_remote(index) {
                let added = crate::remote_known::added_since(&after, &running);
                known = crate::remote_known::union(&known, &added);
            }
            running = after;
        }
        if known.is_empty() {
            None
        } else {
            Some(known)
        }
    } else {
        None
    };

    let last = &records
        .last()
        .ok_or_else(|| StorageError::Corrupt {
            context: "append_updates 收到空批次".to_owned(),
        })?
        .update;
    let head_seq = expected_head_seq + records.len() as u64;

    let mut statements = Vec::with_capacity(records.len() + 2 + slots.len());

    // 第 0 条就是 CAS：head_seq 必须还是调用方看到的那个值。
    // 放在最前面，过期基准会在碰到任何唯一约束之前就被守卫拦下，报 HeadConflict 而不是 UNIQUE。
    statements.push(Statement::new(
        "UPDATE loro_docs SET head_seq = ?2, head_frontier = ?3, head_vv = ?4 \
         WHERE doc_id = ?1 AND head_seq = ?5",
        vec![
            Value::Text(doc_id.to_owned()),
            Value::Integer(i64_of(head_seq)?),
            Value::Blob(last.end_frontier.clone()),
            Value::Blob(last.end_vv.clone()),
            Value::Integer(i64_of(expected_head_seq)?),
        ],
    ));

    for (index, record) in records.iter().enumerate() {
        let seq = expected_head_seq + 1 + index as u64;
        statements.push(Statement::new(
            "INSERT INTO loro_updates(doc_id, seq, update_id, batch_id, codec_version, payload, end_frontier, end_vv) \
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            vec![
                Value::Text(doc_id.to_owned()),
                Value::Integer(i64_of(seq)?),
                Value::Text(format!("{doc_id}#{seq}")),
                Value::Text(record.batch_id.clone()),
                Value::Text(CODEC_VERSION.to_owned()),
                Value::Blob(record.update.payload.clone()),
                Value::Blob(record.update.end_frontier.clone()),
                Value::Blob(record.update.end_vv.clone()),
            ],
        ));
    }

    statements.push(Statement::new(
        "INSERT INTO projection_outbox(doc_id, target_seq) VALUES(?1, ?2) \
         ON CONFLICT(doc_id) DO UPDATE SET target_seq = MAX(target_seq, excluded.target_seq)",
        vec![
            Value::Text(doc_id.to_owned()),
            Value::Integer(i64_of(head_seq)?),
        ],
    ));

    for (index, slot) in slots.iter().enumerate() {
        // `None`：这条 update 不产生出站意图（remote 导入 / adopt 等），同事务里跳过。
        let Some(intent) = slot else {
            continue;
        };
        if intent.message_id.is_empty() || intent.batch_id.is_empty() || intent.key_epoch == 0 {
            return Err(StorageError::Corrupt {
                context: "sync intent message/batch/epoch 非法".to_owned(),
            });
        }
        let seq = expected_head_seq + 1 + index as u64;
        statements.push(Statement::new(
            "INSERT INTO sync_outbox(message_id,batch_id,doc_id,seq,key_epoch,object_id,state,ciphertext) \
             VALUES(?1,?2,?3,?4,?5,?6,'pending',NULL)",
            vec![
                Value::Text(intent.message_id.clone()),
                Value::Text(intent.batch_id.clone()),
                Value::Text(doc_id.to_owned()),
                Value::Integer(i64_of(seq)?),
                Value::Integer(i64_of(intent.key_epoch)?),
                Value::Blob(intent.object_id.to_vec()),
            ],
        ));
    }

    // 远端已知 VV 的 meta 写入（与 update/head 同一 A 事务；失败一起回滚）。
    //
    // **值 CAS**：语句里带读到的旧值，只有库中值仍是旧值才会更新（`WHERE meta.value=?old`，
    // 旧值不存在时用 `INSERT ... WHERE NOT EXISTS`）。这样两个写者（普通 pull 的 A 事务
    // 与 baseline apply 批次）即使都拿着同一份旧并集，后到的那个也不会把先到的扩大值
    // 覆盖回更小的并集；CAS 不命中 → 0 行 → 守卫失败 → 整笔回滚后重读重试。
    let remote_slot = match &remote_added {
        Some(added) => {
            let index = statements.len();
            statements.push(cas_statement_or_corrupt(
                crate::remote_known::merge_statement(vault, doc_id, added)?,
                doc_id,
            )?);
            Some(index)
        }
        None => None,
    };

    // 同事务标记 inbox 行：每条都要求恰好影响 1 行（不存在 / 已 integrated /
    // 已 quarantine 都会让整笔 A 事务回滚）。标记语句放在最后，避免它们的守卫先于
    // head CAS 暴露，保证并发冲突仍然报 HeadConflict。
    let marks_start = statements.len();
    for (mark_log_id, relay_seq) in integrated_marks {
        statements.push(Statement::new(
            "UPDATE sync_inbox SET state='integrated' \
             WHERE log_id=?1 AND relay_seq=?2 AND state='verified'",
            vec![
                Value::Blob(mark_log_id.to_vec()),
                Value::Integer(i64_of(*relay_seq)?),
            ],
        ));
    }
    let mut guards = Vec::with_capacity(2 + integrated_marks.len());
    guards.push(AffectedGuard::exactly(0, 1));
    if let Some(index) = remote_slot {
        guards.push(AffectedGuard::exactly(index, 1));
    }
    guards.extend(
        integrated_marks
            .iter()
            .enumerate()
            .map(|(index, _)| AffectedGuard::exactly(marks_start + index, 1)),
    );

    // 远端已知 VV 的 CAS 失败是**可重试**的：并发写者只是先扩大了一步，重读旧值后
    // 把两边并集再写一次即可。重试期间 head CAS 仍在 index 0：若真的有人推进了 head，
    // 依旧按 HeadConflict 返回，不会把「基准过期」误当成 meta 冲突。
    const REMOTE_KNOWN_CAS_ATTEMPTS: usize = 4;
    let mut attempt = 0usize;
    loop {
        attempt += 1;
        match vault.exec_transaction(&statements, &guards) {
            Ok(_) => {
                return Ok(HeadState {
                    doc_id: doc_id.to_owned(),
                    head_seq,
                    head_frontier: last.end_frontier.clone(),
                    head_vv: last.end_vv.clone(),
                });
            }
            // 第 0 条守卫 = head CAS。
            Err(StorageError::GuardFailed { statement: 0, .. }) => {
                let actual = load_head(vault, doc_id)
                    .map(|head| head.head_seq)
                    .unwrap_or(0);
                return Err(StorageError::HeadConflict {
                    doc_id: doc_id.to_owned(),
                    expected: expected_head_seq,
                    actual,
                });
            }
            // 远端已知 VV 的值 CAS 不命中：指定下标只有它这一条守卫。
            Err(StorageError::GuardFailed { statement, .. }) if remote_slot == Some(statement) => {
                let Some(added) = remote_added.as_ref() else {
                    return Err(StorageError::Corrupt {
                        context: format!("文档 {doc_id} 的远端已知 VV CAS 守卫缺少重算依据"),
                    });
                };
                if attempt >= REMOTE_KNOWN_CAS_ATTEMPTS {
                    return Err(StorageError::SyncConflict {
                        context: format!(
                            "文档 {doc_id} 的远端已知 VV 值 CAS 连续 {attempt} 次被并发写者改写；\
                             整笔 A 事务（update+head+meta）已回滚，可重试"
                        ),
                    });
                }
                let slot = remote_slot.expect("守卫命中说明 remote_slot 存在");
                statements[slot] = cas_statement_or_corrupt(
                    crate::remote_known::merge_statement(vault, doc_id, added)?,
                    doc_id,
                )?;
            }
            // 标记失败：update 与所有标记一起回滚；调用方必须保持 inbox verified 原样。
            Err(StorageError::GuardFailed { statement, .. }) if statement >= marks_start => {
                return Err(StorageError::SyncConflict {
                    context: format!(
                        "inbox 标记 integrated 失败（第 {} 个标记行不存在或当前不是 verified）；\
                         整笔 A 事务（update+标记）已回滚",
                        statement - marks_start
                    ),
                });
            }
            Err(error) => return Err(error),
        }
    }
}

/// [`crate::remote_known::merge_statement`] 在 `remote_added` 非空时必然返回语句；
/// 这里把它转成带文档上下文的损坏错误，避免业务路径 `unwrap`。
fn cas_statement_or_corrupt(
    statement: Option<Statement>,
    doc_id: &str,
) -> Result<Statement, StorageError> {
    statement.ok_or_else(|| StorageError::Corrupt {
        context: format!("文档 {doc_id} 的远端已知 VV 增量非空，但没有生成 CAS 语句"),
    })
}

/// 笔记当前的物化水位（供 B 事务与测试读取）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BodyState {
    /// 笔记 ID。
    pub note_id: String,
    /// 已物化的 body_md。
    pub body_md: Option<String>,
    /// 已物化的 batch seq。
    pub body_seq: u64,
    /// 已物化的 frontier。
    pub body_frontier: Option<Vec<u8>>,
}

/// 读一篇笔记的物化状态。
pub fn load_body_state(vault: &Vault, note_id: &str) -> Result<BodyState, StorageError> {
    let rows = vault.query(
        "SELECT body_md, body_seq, body_frontier FROM notes WHERE note_id = ?1",
        &[Value::Text(note_id.to_owned())],
    )?;
    let row = rows
        .into_iter()
        .next()
        .ok_or_else(|| StorageError::Corrupt {
            context: format!("notes 缺少 {note_id}"),
        })?;

    let body_md = match row.first() {
        Some(Value::Text(text)) => Some(text.clone()),
        Some(Value::Null) | None => None,
        Some(other) => {
            return Err(StorageError::Corrupt {
                context: format!("notes.body_md 类型异常：{other:?}"),
            });
        }
    };

    Ok(BodyState {
        note_id: note_id.to_owned(),
        body_md,
        body_seq: u64_of(&row, 1, "notes.body_seq")?,
        body_frontier: optional_blob(&row, 2)?,
    })
}

fn i64_of(value: u64) -> Result<i64, StorageError> {
    i64::try_from(value).map_err(|_| StorageError::Corrupt {
        context: format!("数值 {value} 超出 SQLite INTEGER 范围"),
    })
}

fn u64_of(row: &[Value], index: usize, field: &str) -> Result<u64, StorageError> {
    match row.get(index) {
        Some(Value::Integer(value)) if *value >= 0 => Ok(*value as u64),
        other => Err(StorageError::Corrupt {
            context: format!("{field} 读回异常：{other:?}"),
        }),
    }
}

fn blob_of(row: &[Value], index: usize, field: &str) -> Result<Vec<u8>, StorageError> {
    match row.get(index) {
        Some(Value::Blob(bytes)) => Ok(bytes.clone()),
        Some(Value::Null) | None => Ok(Vec::new()),
        other => Err(StorageError::Corrupt {
            context: format!("{field} 读回异常：{other:?}"),
        }),
    }
}

fn optional_blob(row: &[Value], index: usize) -> Result<Option<Vec<u8>>, StorageError> {
    match row.get(index) {
        Some(Value::Blob(bytes)) => Ok(Some(bytes.clone())),
        Some(Value::Null) | None => Ok(None),
        other => Err(StorageError::Corrupt {
            context: format!("期望 BLOB/NULL，实际 {other:?}"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use loro::VersionVector;

    use super::*;
    use crate::body_doc::BodyDoc;
    use crate::remote_known;

    /// 临时 vault：目录建在 `target/tmp` 下（AGENTS §6：不用 `%TEMP%`）。
    ///
    /// lib 单元测试拿不到 `CARGO_TARGET_TMPDIR`（只有集成测试有），所以从
    /// `CARGO_MANIFEST_DIR` 拼出同一个位置。
    fn fresh_vault(prefix: &str) -> (tempfile::TempDir, Vault) {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/tmp")
            .join(format!("store-origin-{prefix}"));
        std::fs::create_dir_all(&root).expect("创建 target/tmp 下的测试根目录");
        let dir = tempfile::Builder::new()
            .prefix("vault-")
            .tempdir_in(&root)
            .expect("创建临时目录");
        let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
        (dir, vault)
    }

    fn scalar_i64(vault: &Vault, sql: &str) -> i64 {
        let rows = vault.query(sql, &[]).expect("查询");
        match rows.first().and_then(|row| row.first()) {
            Some(Value::Integer(value)) => *value,
            other => panic!("期望整数结果，实际 {other:?}"),
        }
    }

    fn local_update(doc: &BodyDoc, base: &VersionVector) -> PendingUpdate {
        doc.pending_update(base).expect("导出增量")
    }

    /// `OriginMarks::AllLocal`（显式类型）不扩大远端已知 VV，也不产生 outbox。
    #[test]
    fn all_local_origin_does_not_extend_remote_known() {
        let (_dir, vault) = fresh_vault("all-local");
        create_body_doc(&vault, "doc-local").expect("loro_docs 行");
        let doc = BodyDoc::create("doc-local", 0xA1).expect("创建 BodyDoc");
        doc.insert_scalars(0, "local").expect("本地编辑");
        let update = local_update(&doc, &VersionVector::default());

        let head = append_updates_inner(
            &vault,
            "doc-local",
            0,
            &[UpdateRecord::new("b0", update)],
            &[],
            &[],
            OriginMarks::AllLocal,
        )
        .expect("A 事务");

        assert_eq!(head.head_seq, 1);
        assert!(
            remote_known::load(&vault, "doc-local")
                .expect("读远端已知")
                .is_none(),
            "全部本地的批次绝不能创建/扩大远端已知 VV"
        );
        assert_eq!(
            scalar_i64(&vault, "SELECT count(*) FROM sync_outbox"),
            0,
            "slots 空 = 本批没有任何出站意图"
        );
    }

    /// `PerRecord` 混排：只有 remote 条目的 `after - running` 记入远端已知 VV。
    #[test]
    fn per_record_origin_only_extends_for_remote_records() {
        let (_dir, vault) = fresh_vault("mixed");
        create_body_doc(&vault, "doc-mixed").expect("loro_docs 行");

        let doc = BodyDoc::create("doc-mixed", 0xA1).expect("创建 BodyDoc");
        doc.insert_scalars(0, "local").expect("本地编辑");
        let local = local_update(&doc, &VersionVector::default());
        let remote_doc = BodyDoc::create("doc-mixed", 0xB2).expect("创建 BodyDoc");
        remote_doc.insert_scalars(0, "remote").expect("远端编辑");
        doc.import(&remote_doc.export_all_updates().expect("远端 update"))
            .expect("本地副本导入远端");
        let local_end = VersionVector::decode(&local.end_vv).expect("本地 VV");
        let remote = local_update(&doc, &local_end);
        let remote_end = VersionVector::decode(&remote.end_vv).expect("远端 VV");
        let expected = remote_known::added_since(&remote_end, &local_end);

        let head = append_updates_inner(
            &vault,
            "doc-mixed",
            0,
            &[
                UpdateRecord::new("b0", local),
                UpdateRecord::new("b1", remote),
            ],
            &[],
            &[],
            OriginMarks::PerRecord(&[false, true]),
        )
        .expect("A 事务");

        assert_eq!(head.head_seq, 2, "两条记录都必须落库");
        let known = remote_known::load(&vault, "doc-mixed")
            .expect("读远端已知")
            .expect("有 remote 条目就必须写 meta");
        assert_eq!(
            known, expected,
            "远端条目带来的 ops 必须记入，且只记入远端那一小段"
        );
        assert_eq!(
            known.get(&0xA1).copied().unwrap_or(0),
            0,
            "本地条目（哪怕同批先落库）绝不能记入远端已知 VV"
        );
        assert_eq!(
            scalar_i64(&vault, "SELECT count(*) FROM sync_outbox"),
            0,
            "没有传 slots 就不得写出站意图"
        );
    }

    /// 空 `PerRecord` 不再表示「全部本地」：长度不匹配直接拒绝，且不写任何行。
    #[test]
    fn per_record_origin_length_mismatch_is_rejected() {
        let (_dir, vault) = fresh_vault("mismatch");
        create_body_doc(&vault, "doc-mismatch").expect("loro_docs 行");
        let doc = BodyDoc::create("doc-mismatch", 0xA1).expect("创建 BodyDoc");
        doc.insert_scalars(0, "x").expect("编辑");
        let update = local_update(&doc, &VersionVector::default());
        let record = UpdateRecord::new("b0", update);

        let error = append_updates_inner(
            &vault,
            "doc-mismatch",
            0,
            std::slice::from_ref(&record),
            &[],
            &[],
            OriginMarks::PerRecord(&[]),
        )
        .expect_err("空 PerRecord 配非空 records 必须报错");
        assert!(
            matches!(error, StorageError::Corrupt { .. }),
            "必须是显式损坏/参数错误，实际 {error:?}"
        );
        assert_eq!(
            scalar_i64(&vault, "SELECT count(*) FROM loro_updates"),
            0,
            "参数错误不得留下半写"
        );

        let error = append_updates_inner(
            &vault,
            "doc-mismatch",
            0,
            &[record],
            &[],
            &[],
            OriginMarks::PerRecord(&[true, false]),
        )
        .expect_err("标记多于 records 也必须报错");
        assert!(matches!(error, StorageError::Corrupt { .. }));
    }

    /// 值 CAS 不命中（并发写者改了 meta）→ 有界重试后返回明确 SyncConflict，
    /// 整笔 A 事务回滚：head/update/outbox/meta 都不留半写，也绝不写入更小值。
    ///
    /// 注入方式：`loro_updates` 的 BEFORE INSERT 触发器在**同一事务里**把 meta 改成
    /// 别的值（模拟「读旧值之后、CAS 之前的并发提交」）；每次重试都会再次命中，
    /// 因此有界重试必然耗尽。
    #[test]
    fn stale_remote_known_cas_fails_closed_and_rolls_back() {
        let (_dir, vault) = fresh_vault("cas");
        create_body_doc(&vault, "doc-cas").expect("loro_docs 行");
        let remote_doc = BodyDoc::create("doc-cas", 0xB2).expect("创建 BodyDoc");
        remote_doc.insert_scalars(0, "remote").expect("远端编辑");
        let update = local_update(&remote_doc, &VersionVector::default());
        vault
            .exec(
                "CREATE TRIGGER remote_known_race BEFORE INSERT ON loro_updates BEGIN \
                   INSERT OR REPLACE INTO meta(key,value) \
                   VALUES('sync.remote_known_vv.doc-cas', X'DEADBEEF'); \
                 END;",
                &[],
            )
            .expect("安装竞态触发器");

        let error = append_updates_inner(
            &vault,
            "doc-cas",
            0,
            &[UpdateRecord::new("b0", update)],
            &[],
            &[],
            OriginMarks::PerRecord(&[true]),
        )
        .expect_err("CAS 必须失败");

        match error {
            StorageError::SyncConflict { context } => {
                assert!(
                    context.contains("远端已知 VV"),
                    "错误必须点明远端已知 VV CAS：{context}"
                );
                assert!(context.contains("可重试"), "错误必须说明可重试：{context}");
            }
            other => panic!("期望 SyncConflict，实际 {other:?}"),
        }
        assert_eq!(load_head(&vault, "doc-cas").expect("head").head_seq, 0);
        assert_eq!(
            scalar_i64(&vault, "SELECT count(*) FROM loro_updates"),
            0,
            "整笔回滚：update 不得落库"
        );
        assert_eq!(
            scalar_i64(&vault, "SELECT count(*) FROM projection_outbox"),
            0
        );
        assert_eq!(
            scalar_i64(
                &vault,
                "SELECT count(*) FROM meta WHERE key='sync.remote_known_vv.doc-cas'"
            ),
            0,
            "触发器写入随整笔回滚，失败后不残留"
        );
        assert!(
            remote_known::load(&vault, "doc-cas")
                .expect("读远端已知")
                .is_none(),
            "失败路径绝不能写入更小/错误的值"
        );
    }
}

`````


## `crates/sync/src/bootstrap.rs`

`````rust
//! 新设备 bootstrap（S7-18；ADR §3.7/§4/§7/§25.3；评审 0002 P1-4）。
//!
//! 本模块先落"数据恢复"这一段：给定已解出的 root（由槽解封或后续恢复流程提供），
//! `POST /bootstrap` 取两份基线 → 下 manifest/snapshot/asset → 全量校验 → 空库落库。
//!
//! **可恢复导入（P1-4）**：docs/structure/assets 的写入全部经过 storage 的
//! `baseline_apply` staging journal（`begin_baseline_apply` / `apply_baseline_batch` /
//! `apply_baseline_assets`）。下载/写入阶段任一步失败都**不**清 journal、**不**推进任何
//! 水位；下一次调用按同一份基线幂等续跑。只有全部实际导入并通过读回校验后，才在**一个**
//! SQLite 事务里推进 `cursor/received/integrated/confirmed_highwater`（CAS）。
//!
//! **读回校验失败（同步硬化 P1）**：`verify_imported` 失败时**不回滚**已 merge 的
//! Loro/资产数据（CRDT merge 幂等；回滚会丢本地编辑），而是写一条只本地可见的
//! `sync.baseline_verify_failed` 失败标记（storage `mark_baseline_verify_failed`）。
//! journal 仍是 `staging`（schema 的 status CHECK 只允许 `staging/completed`，见
//! `docs/notes/sync-hardening-20261121.md` §8），但下一次 `begin_baseline_apply` 会拒绝，
//! 必须先 `abort_baseline_apply` 清理（abort 同时删标记，已导入数据保留）。
//!
//! **闸外编辑**：verify 不与编辑互斥，判定用「导出点 VV ⊆ 当前 VV」而不是相等；只有
//! 本地没有快照之外的 op 时才逐字节比较正文（见 [`check_imported_doc`]）。
//!
//! **水位单调（P1-4）**：bootstrap 允许作用在已有库上，但禁止降低任何单调值：
//! `received/integrated/highwater` 取 `max(现有, baseline S)`；已有进度时保留现有 relay
//! 游标（不把已经消费过的位置回退）；`instance/log_id/epoch/control rev+hash` 只允许
//! 前进或不变。storage 的 `finish_baseline_apply` 会把 highwater 直接写成 baseline S，
//! 对"已有 highwater 高于 S"的库会降低水位，因此本模块在同一个事务里自行做
//! `max` 语义的 CAS 推进，随后清掉 staging（`abort_baseline_apply` 只清 journal，
//! 不删已导入的 Loro/asset 数据）。
//!
//! **实际 VV 校验（P1-4）**：每个 snapshot 都在独立的空 `LoroDoc` 里 import，比较
//! actual VV / frontier 与 manifest 声明（既不虚高也不缺 op），并校验 full-history、
//! 信封字段、app schema / min_reader；结构文档再重放出结构树，校验"结构引用的正文
//! 文档必须都在基线 manifest 里"。不只信声明。
//!
//! **fallback**：primary 任一阶段（下载/解密/摘要/VV/结构语义）失败才整体切 fallback；
//! 选中哪个基线就只用**它自己的** S / cursor；写入阶段失败不切 fallback（避免半库
//! 换 S），必须原样重试或人工 abort journal。
//!
//! **写库路径**：全部经 storage 公开 API（`BodyDoc`/`StructureWriter`/`baseline_apply`），
//! 不直连 SQLite；只有最终的水位推进用 `Vault::exec_transaction` 的单事务 CAS
//! （storage 没有提供 max 语义的 finish，且本任务写作用域不允许改 storage）。
//!
//! **已知边界**：relay `/bootstrap` 不返回完整签名 checkpoint 链；本模块只验证
//! `CheckpointCommit` 的生产者签名、manifest/refs 摘要、以及所有实际对象。新设备无旧
//! 水位，无法检测"有效签名的旧基线回滚"（ADR §10 已记录）；已有库用本地 pins 检测
//! `instance/log_id/epoch/control_rev/highwater` 的回退。

use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use loro::VersionVector;
use sha2::{Digest, Sha256};

use crate::checkpoint::{verify_asset_plaintext, AssetRef, KIND_BODY, KIND_STRUCTURE};
use crate::control_pin::BootstrapAnchorV1;
use crate::device::verify_signature;
use crate::engine::RELAY_CURSOR_META;
use crate::error::SyncError;
use crate::keys::RootSecretV1;
use crate::relay::{
    BootstrapBundle, BootstrapCheckpoint, ManagerChainEntry, RelayClient, RelayState,
};
use crate::rotation::ManagerTransitionV1;
use crate::upload::fetch_object;
use menghunote_storage::{
    abort_baseline_apply, apply_baseline_assets, apply_baseline_batch, asset_bytes,
    baseline_apply_state, begin_baseline_apply, load_body_doc, load_structure,
    mark_baseline_verify_failed, record_baseline_verify_time, sync_cursor_state, AffectedGuard,
    BaselineApplyStatus, BaselineAsset, BaselineExpectation, BodyDoc, Statement, StructureDoc,
    StructureWriter, Value, Vault, APP_DOC_SCHEMA, CODEC_VERSION, META_SYNC_CONFIRMED_HIGHWATER,
    META_SYNC_INTEGRATED_SEQ, META_SYNC_LOG_ID, META_SYNC_RECEIVED_SEQ, MIN_READER,
    STRUCTURE_DOC_ID,
};
use menghunote_sync_proto::{
    checkpoint_signing_input, refs_digest_input, CheckpointCommit, Envelope, EnvelopeHeader,
    ManifestDoc, ManifestV1, ObjectRefV1, ObjectType, SnapshotV1,
};

/// 每批写入的文档数（storage 的 baseline batch 是单事务；分批只为控制单事务大小）。
const BOOTSTRAP_DOC_BATCH: usize = 64;
/// bootstrap 写入 `baseline_apply.source_cursor` 的前缀：用于区分 bootstrap 的 staging
/// 与轮换（`rotation_apply` 传空串）的 staging，防止误清别人的 journal。
const JOURNAL_SOURCE_PREFIX: &str = "bootstrap:";

/// `meta` 键：bootstrap pin 的 relay instance id（16B，只允许相等）。
pub const META_BOOTSTRAP_INSTANCE: &str = "sync.pinned_instance";
/// `meta` 键：bootstrap pin 的 epoch（BE64，只允许前进）。
pub const META_BOOTSTRAP_EPOCH: &str = "sync.pinned_epoch";
/// `meta` 键：bootstrap pin 的 control_rev（BE64，只允许前进）。
pub const META_BOOTSTRAP_CONTROL_REV: &str = "sync.pinned_control_rev";
/// `meta` 键：bootstrap pin 的控制头摘要（32B，见 [`control_head_hash`]；同 rev 必须相等）。
pub const META_BOOTSTRAP_CONTROL_HASH: &str = "sync.pinned_control_hash";

/// bootstrap 结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapReport {
    /// 使用的 checkpoint id。
    pub checkpoint_id: [u8; 16],
    /// 覆盖水位 S。
    pub covered_seq: u64,
    /// 是否回退到 fallback。
    pub used_fallback: bool,
    /// 导入文档（structure 的文本为 None）。
    pub docs: Vec<(String, Option<String>)>,
    /// 导入资产数。
    pub assets: usize,
    /// S 对应的 relay 游标。
    pub cursor: String,
    /// 是否验证了 checkpoint 发布签名（relay 返回键 6/7 且验签通过）。
    pub signed_checkpoint: bool,
}

/// bootstrap 前本地已经可信的单调值（P1-4 "已有库保护"）。
///
/// 新设备全部为 `None`；已有设备由 [`BootstrapPins::from_vault`] 从本库 meta 读出，
/// 或由调用方用 identity（`pinned_instance/control_rev/current_epoch`）填充后调用
/// [`run_with_pins`]。bootstrap 只允许这些值前进/不变，绝不回退。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BootstrapPins {
    /// relay instance id（防换实例/静默回滚）。
    pub instance_id: Option<[u8; 16]>,
    /// relay log_id（restore 必须显式换新，不能静默归零）。
    pub log_id: Option<[u8; 16]>,
    /// 当前密钥代。
    pub epoch: Option<u64>,
    /// 已信任的管理控制链版本。
    pub control_rev: Option<u64>,
    /// 已信任的控制头摘要（见 [`control_head_hash`]）。
    pub control_hash: Option<[u8; 32]>,
    /// 已确认的 relay 水位。
    pub highwater: Option<u64>,
}

impl BootstrapPins {
    /// 从本库持久化的 pin（`sync.pinned_*`）与同步游标读出已有可信值。
    ///
    /// pin 缺失（新设备/首次 bootstrap）返回 `None`，不报错；pin 存在但长度非法按
    /// 本地损坏处理（不能静默忽略，否则等于绕过单调保护）。
    pub fn from_vault(vault: &Vault) -> Result<Self, SyncError> {
        let cursor = sync_cursor_state(vault)?;
        Ok(Self {
            instance_id: read_meta_array::<16>(vault, META_BOOTSTRAP_INSTANCE)?,
            log_id: cursor.log_id,
            epoch: read_meta_u64(vault, META_BOOTSTRAP_EPOCH)?,
            control_rev: read_meta_u64(vault, META_BOOTSTRAP_CONTROL_REV)?,
            control_hash: read_meta_array::<32>(vault, META_BOOTSTRAP_CONTROL_HASH)?,
            highwater: cursor.confirmed_highwater,
        })
    }

    /// 把 `sync.pinned_*` 读成一个**带自己 rev/epoch/log** 的 bootstrap 锚（ADR 0013 §2）。
    ///
    /// 语义：
    /// - `control_hash` 与 `control_rev` 必须成对：只有 hash 没有 rev 是本地损坏
    ///   （既有行为）；只有 rev 没有 hash 时返回 `None`——rev 只是下界，不构成摘要锚；
    /// - hash 存在时 `epoch` / `log_id` 必须齐全：历史视图复算需要锚**自己的**坐标，
    ///   缺任一项都不能把摘要当"同 rev 校验"用（否则旧锚会在 rev 前进后被静默忽略）。
    ///   这是 fail-closed：缺字段报 [`SyncError::LocalCorrupt`]，**不**降级成"没有锚"。
    pub fn control_anchor(&self) -> Result<Option<BootstrapAnchorV1>, SyncError> {
        let Some(hash) = self.control_hash else {
            // 只有 rev 下界（历史实例/未写摘要）不构成摘要锚；单调性仍由 control_rev 保证。
            return Ok(None);
        };
        let rev = self.control_rev.ok_or(SyncError::LocalCorrupt(
            "bootstrap pins 有 control_hash 但缺 control_rev",
        ))?;
        let epoch = self.epoch.ok_or(SyncError::LocalCorrupt(
            "bootstrap pins 有 control_hash 但缺 epoch（无法按历史视图复算摘要）",
        ))?;
        let log_id = self.log_id.ok_or(SyncError::LocalCorrupt(
            "bootstrap pins 有 control_hash 但缺 log_id（无法按历史视图复算摘要）",
        ))?;
        Ok(Some(BootstrapAnchorV1 {
            rev,
            hash,
            epoch,
            log_id,
        }))
    }
}

/// 测试用故障注入点（生产调用路径不会构造）。
///
/// 语义等价于"进程在崩溃点直接消失"：不再执行后续步骤、不清理 journal、不推进水位。
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootstrapFault {
    /// 正文批次写完后崩溃（structure / assets / 水位都未写）。
    AfterBodies,
    /// structure 文档已写入、结构投影尚未执行时崩溃。
    BeforeStructureProjection,
    /// 全部 assets 已写入、读回校验与水位推进之前崩溃。
    AfterAssets,
    /// 水位事务读取当前状态之后、提交之前，模拟另一个写者改写了 relay 游标，
    /// 迫使 CAS 失败（整笔事务回滚）。
    CursorCasConflict,
    /// 读回校验阶段注入一次「本地 VV 未覆盖快照」失败：用于验证失败标记、
    /// `begin` 拒绝续跑与 `abort` 清理路径（不改数据库里的真实数据）。
    VerifyVvNotCovered,
}

/// 故障注入伪造的 peer id（只在校验失败测试路径出现；不影响真实数据）。
const VERIFY_FAULT_PEER: u64 = 0x0BAD_5EED;

struct VerifiedDoc {
    doc_id: String,
    kind: u64,
    snapshot: SnapshotV1,
    actual_vv: VersionVector,
    /// 正文文本（structure 为 `None`）。
    text: Option<String>,
}

struct VerifiedAsset {
    object_id: [u8; 32],
    real_sha256: [u8; 32],
    byte_len: u64,
    media_type: String,
    data: Vec<u8>,
}

/// 选中的基线：manifest/docs/assets + 基线本身 + 结构语义。
struct SelectedBaseline {
    checkpoint: BootstrapCheckpoint,
    used_fallback: bool,
    docs: Vec<VerifiedDoc>,
    assets: Vec<VerifiedAsset>,
    /// structure 快照声明的 `note_id -> body_doc_id`（用于导入后的结构投影校验）。
    structure_notes: BTreeMap<String, String>,
}

/// 用已解出的 root 执行数据 bootstrap；本地 pins 从本库 meta 读取。
pub fn run<C: RelayClient>(
    vault: &Vault,
    client: &mut C,
    vault_id: [u8; 16],
    peer_id: u64,
    root: &RootSecretV1,
) -> Result<BootstrapReport, SyncError> {
    let pins = BootstrapPins::from_vault(vault)?;
    run_with_pins(vault, client, vault_id, peer_id, root, &pins)
}

/// 用调用方提供的本地 pins 执行 bootstrap（已有设备传 identity 的 pin）。
pub fn run_with_pins<C: RelayClient>(
    vault: &Vault,
    client: &mut C,
    vault_id: [u8; 16],
    peer_id: u64,
    root: &RootSecretV1,
    pins: &BootstrapPins,
) -> Result<BootstrapReport, SyncError> {
    run_inner(vault, client, vault_id, peer_id, root, pins, None)
}

/// 测试入口：在指定阶段注入崩溃/并发冲突（见 [`BootstrapFault`]）。
#[doc(hidden)]
pub fn run_with_fault<C: RelayClient>(
    vault: &Vault,
    client: &mut C,
    vault_id: [u8; 16],
    peer_id: u64,
    root: &RootSecretV1,
    fault: BootstrapFault,
) -> Result<BootstrapReport, SyncError> {
    let pins = BootstrapPins::from_vault(vault)?;
    run_inner(vault, client, vault_id, peer_id, root, &pins, Some(fault))
}

fn run_inner<C: RelayClient>(
    vault: &Vault,
    client: &mut C,
    vault_id: [u8; 16],
    peer_id: u64,
    root: &RootSecretV1,
    pins: &BootstrapPins,
    fault: Option<BootstrapFault>,
) -> Result<BootstrapReport, SyncError> {
    if root.vault_id != vault_id {
        return Err(SyncError::Object("bootstrap root 与 vault 不符"));
    }
    let bundle = client.bootstrap()?;
    let state = client.state()?;
    let chain = client.management_chain()?;
    let control_hash = control_head_hash(&state, &chain);
    validate_remote(vault_id, root, pins, &bundle, &state, &chain, &control_hash)?;

    // 选中哪个基线，就只用它的 S/cursor/manifest/docs；primary 任一阶段失败才整体切 fallback。
    // 所有下载/解密/摘要/VV/结构语义校验都在写库之前完成。
    let mut selected: Option<SelectedBaseline> = None;
    let mut last_error: Option<SyncError> = None;
    for (checkpoint, used_fallback) in [
        (bundle.primary.clone(), false),
        (bundle.fallback.clone(), true),
    ] {
        match load_baseline(
            client,
            root,
            vault_id,
            peer_id,
            &checkpoint,
            &bundle,
            &state,
        ) {
            Ok(baseline) => {
                selected = Some(SelectedBaseline {
                    used_fallback,
                    ..baseline
                });
                break;
            }
            Err(error) => last_error = Some(error),
        }
    }
    let selected = selected.ok_or_else(|| last_error.expect("bootstrap 至少尝试过一个基线"))?;

    // 实际导入（staging journal 可续跑）；失败时 journal 保留，下一次按同一基线续跑。
    let report_docs = apply_selected(vault, peer_id, &selected, fault)?;

    // 全部实际写入 + 读回校验通过后，才在一个事务里推进水位（max 语义 + CAS）。
    finalize(vault, &selected, &state, &chain, &control_hash, pins, fault)?;

    // 成功后才清 staging journal；若这里失败，重试会幂等重放并再次清理。
    let _cleared = abort_baseline_apply(vault)?;

    Ok(BootstrapReport {
        checkpoint_id: selected.checkpoint.checkpoint_id,
        covered_seq: selected.checkpoint.covered_seq,
        used_fallback: selected.used_fallback,
        docs: report_docs,
        assets: selected.assets.len(),
        cursor: selected.checkpoint.cursor,
        signed_checkpoint: true,
    })
}

/// 校验本地 durable bootstrap 锚（ADR 0013 §3；`validate_remote`/`finalize` 共用）。
///
/// 两类视图必须分开，**不能**把旧 hash 挂到 `max(rev)` 上比较：
///
/// - `anchor.rev == state.control_rev`：摘要必须逐字节等于当前视图（`epoch`/`log_id`
///   由当前 state 决定；同 rev 换 epoch/log/hash 都是 equivocation）；
/// - `anchor.rev <  state.control_rev`：**不能忽略**旧锚，也不能拿新 rev 的坐标复算；
///   按锚自己的 `rev/epoch/log` + 截断到该 rev 的管理链复算，证明当前链仍包含旧锚
///   认证过的前缀；复算不出视图 → [`SyncError::NeedsTrust`]，摘要不符 → equivocation。
fn verify_bootstrap_anchor(
    anchor: &BootstrapAnchorV1,
    state: &RelayState,
    chain: &[ManagerChainEntry],
    current_hash: &[u8; 32],
) -> Result<(), SyncError> {
    if anchor.rev > state.control_rev {
        return Err(SyncError::ControlRollback {
            expected: anchor.rev,
            actual: state.control_rev,
        });
    }
    if anchor.rev == state.control_rev {
        if anchor.hash != *current_hash {
            return Err(SyncError::Equivocation(
                "bootstrap 控制头同 rev 但 hash 变化（同 rev 历史不完整）",
            ));
        }
        return Ok(());
    }
    let computed = control_head_hash_at(state.vault_id, state.instance_id, anchor, chain).ok_or(
        SyncError::NeedsTrust("bootstrap 旧锚的 rev 不在当前管理链视图内，无法证明历史前缀"),
    )?;
    if computed != anchor.hash {
        return Err(SyncError::Equivocation(
            "bootstrap 旧锚未被当前管理链覆盖（rev 前进但前缀被重写）",
        ));
    }
    Ok(())
}

/// relay 侧与本地 pins 的写前校验：任何不满足都必须在写库前失败。
fn validate_remote(
    vault_id: [u8; 16],
    root: &RootSecretV1,
    pins: &BootstrapPins,
    bundle: &BootstrapBundle,
    state: &RelayState,
    chain: &[ManagerChainEntry],
    control_hash: &[u8; 32],
) -> Result<(), SyncError> {
    if state.vault_id != vault_id {
        return Err(SyncError::RelayWire(
            "bootstrap state.vault_id 与本地不一致",
        ));
    }
    if root.key_epoch != state.epoch {
        return Err(SyncError::Object("bootstrap root epoch 与 relay 不一致"));
    }
    if bundle.epoch != state.epoch {
        return Err(SyncError::RelayWire("bootstrap epoch 与 state 不一致"));
    }
    // bundle 先取、state 后取；head 只能前进，倒退说明 relay 换库/回滚。
    if bundle.head_seq > state.head_seq {
        return Err(SyncError::RelayWire("bootstrap/state head_seq 回退"));
    }
    if let Some(instance) = pins.instance_id {
        if instance != state.instance_id {
            return Err(SyncError::Object(
                "bootstrap relay instance 与本地 pin 不符",
            ));
        }
    }
    if let Some(log_id) = pins.log_id {
        if log_id != state.log_id {
            return Err(SyncError::Storage(
                menghunote_storage::StorageError::SyncLogMismatch {
                    expected: log_id.to_vec(),
                    found: state.log_id.to_vec(),
                },
            ));
        }
    }
    if let Some(epoch) = pins.epoch {
        if epoch > state.epoch {
            return Err(SyncError::Object("bootstrap 不允许回退 epoch"));
        }
    }
    if let Some(rev) = pins.control_rev {
        if rev > state.control_rev {
            return Err(SyncError::Object("bootstrap 不允许回退 control_rev"));
        }
        if let Some(anchor) = pins.control_anchor()? {
            verify_bootstrap_anchor(&anchor, state, chain, control_hash)?;
        }
    } else if pins.control_hash.is_some() {
        return Err(SyncError::LocalCorrupt(
            "bootstrap pins 有 control_hash 但缺 control_rev",
        ));
    }
    if let Some(highwater) = pins.highwater {
        if highwater > state.head_seq {
            return Err(SyncError::Paused(
                crate::relay::PauseReason::RollbackDetected,
            ));
        }
    }
    // 管理链必须是单调 rev 序列（内容摘要见 control_head_hash；完整链认证属于轮换阶段 A）。
    let mut previous_rev = 0u64;
    for entry in chain {
        if entry.rev <= previous_rev {
            return Err(SyncError::RelayWire("bootstrap 管理链 rev 不递增"));
        }
        previous_rev = entry.rev;
    }
    Ok(())
}

/// 控制头摘要：绑定 relay 可见的控制头坐标（instance/log/epoch/control_rev）与管理转换链。
///
/// 这是本地 pin 的摘要定义（不是 relay 内部的 `control_head` hash）：同一 `control_rev`
/// 下 relay 换 instance/log/epoch 或重写管理链都会改变摘要；rev 前进时允许摘要变化
/// （单调性由 `control_rev` 保证）。完整管理链的签名认证属于轮换阶段 A（ADR §28.2），
/// 本模块只做"同 rev 不得换内容"的回退检测。
#[must_use]
pub fn control_head_hash(state: &RelayState, chain: &[ManagerChainEntry]) -> [u8; 32] {
    control_head_hash_parts(
        state.vault_id,
        state.instance_id,
        state.log_id,
        state.epoch,
        state.control_rev,
        chain,
    )
}

/// [`control_head_hash`] 的**坐标化**输入：调用方可以显式给出历史视图的坐标
/// （ADR 0013 §3「按旧 rev/epoch/log/管理链视图验证」）。
///
/// 摘要公式与 [`control_head_hash`] 逐字节相同；`chain` 必须是**该 rev 下**的管理链
/// （即按 `entry.rev <= control_rev` 截断后的前缀）。
#[must_use]
pub fn control_head_hash_parts(
    vault_id: [u8; 16],
    instance_id: [u8; 16],
    log_id: [u8; 16],
    epoch: u64,
    control_rev: u64,
    chain: &[ManagerChainEntry],
) -> [u8; 32] {
    let mut input = Vec::new();
    input.extend_from_slice(b"menghunote/bootstrap/control-head/v1\0");
    input.extend_from_slice(&vault_id);
    input.extend_from_slice(&instance_id);
    input.extend_from_slice(&log_id);
    input.extend_from_slice(&epoch.to_be_bytes());
    input.extend_from_slice(&control_rev.to_be_bytes());
    input.extend_from_slice(&u32::try_from(chain.len()).unwrap_or(u32::MAX).to_be_bytes());
    for entry in chain {
        input.extend_from_slice(&entry.rev.to_be_bytes());
        input.extend_from_slice(&entry.pubkey);
        input.extend_from_slice(
            &u32::try_from(entry.transition.len())
                .unwrap_or(u32::MAX)
                .to_be_bytes(),
        );
        input.extend_from_slice(&entry.transition);
        push_optional_signature(&mut input, entry.old_signature);
        push_optional_signature(&mut input, entry.new_signature);
    }
    Sha256::digest(&input).into()
}

/// 按 bootstrap 锚**自己的** rev/epoch/log 视图复算控制头摘要（ADR 0013 §3）。
///
/// 为什么不能用当前 head 的坐标：摘要把 `(epoch, control_rev)` 也哈希进去，rev 前进后
/// 同一条历史管理链会算出不同摘要。历史锚只能按"它当初记录的坐标 + 当时的管理链"复算：
///
/// - `epoch` / `log_id` / `rev` 取自锚本身（本地 durable 事实，relay 不能改）；
/// - 管理链按**生效 epoch** 截断：`/control` 键 5 的 `rev` 是**管理 rev**（不是 control
///   rev），每条转换记录 `from_epoch → to_epoch`；epoch 只能由 activation 推进，因此
///   "锚当时的链" = rev1 genesis + `to_epoch <= anchor.epoch` 的转换（必须仍是链前缀）；
/// - `instance_id` 取自本地 pin/身份（换实例另有独立检查）。
///
/// 返回 `None` = 无法重建该历史视图（链不完整/非前缀/转换不可解码），调用方必须
/// fail-closed，不能把"视图不可用"当成"验证通过"。
#[must_use]
pub fn control_head_hash_at(
    vault_id: [u8; 16],
    instance_id: [u8; 16],
    anchor: &BootstrapAnchorV1,
    chain: &[ManagerChainEntry],
) -> Option<[u8; 32]> {
    if chain.first().is_none_or(|entry| entry.rev != 1) {
        return None;
    }
    // rev1 的 `transition` 是 genesis 原字节（不是 ManagerTransitionV1），不参与解码。
    let mut end = 1usize;
    for (index, entry) in chain.iter().enumerate().skip(1) {
        let transition = ManagerTransitionV1::decode(&entry.transition).ok()?;
        if transition.new_rev != entry.rev {
            return None;
        }
        if transition.to_epoch <= anchor.epoch {
            if index != end {
                // 更早的转换不属于历史视图，后面却又出现属于它的转换 = 非前缀，拒绝。
                return None;
            }
            end = index + 1;
        }
    }
    Some(control_head_hash_parts(
        vault_id,
        instance_id,
        anchor.log_id,
        anchor.epoch,
        anchor.rev,
        &chain[..end],
    ))
}

fn push_optional_signature(input: &mut Vec<u8>, signature: Option<[u8; 64]>) {
    match signature {
        None => input.push(0),
        Some(signature) => {
            input.push(1);
            input.extend_from_slice(&signature);
        }
    }
}

fn load_baseline<C: RelayClient>(
    client: &mut C,
    root: &RootSecretV1,
    vault_id: [u8; 16],
    peer_id: u64,
    checkpoint: &BootstrapCheckpoint,
    bundle: &BootstrapBundle,
    state: &RelayState,
) -> Result<SelectedBaseline, SyncError> {
    if checkpoint.epoch != state.epoch || checkpoint.epoch != bundle.epoch {
        return Err(SyncError::Object("bootstrap 基线 epoch 与 relay 不一致"));
    }
    // "≤S"：基线只能覆盖 relay 已经产生的水位；低于 GC floor 的游标已不可继续。
    if checkpoint.covered_seq > bundle.head_seq || checkpoint.covered_seq > state.head_seq {
        return Err(SyncError::Object("bootstrap 基线 S 超过 relay head"));
    }
    // 低于 GC floor 的游标已经不可继续：两次请求之间 floor 也可能前进，取较大者。
    if checkpoint.covered_seq < bundle.gc_floor || checkpoint.covered_seq < state.gc_floor {
        return Err(SyncError::Object("bootstrap 基线 S 低于 relay GC floor"));
    }
    if checkpoint.cursor.is_empty() {
        return Err(SyncError::Object("bootstrap 基线缺 relay 游标"));
    }

    let (manifest, manifest_digest) = fetch_manifest(client, root, checkpoint)?;
    verify_checkpoint_commit(client, checkpoint, &manifest, &manifest_digest)?;
    verify_manifest(&manifest, vault_id, checkpoint, state.epoch)?;
    let docs = download_docs(client, root, peer_id, &manifest)?;
    let structure_notes = verify_structure_semantics(&docs, peer_id)?;
    let assets = download_assets(client, root, &manifest)?;
    Ok(SelectedBaseline {
        checkpoint: checkpoint.clone(),
        used_fallback: false,
        docs,
        assets,
        structure_notes,
    })
}

fn verify_checkpoint_commit<C: RelayClient>(
    client: &mut C,
    checkpoint: &BootstrapCheckpoint,
    manifest: &ManifestV1,
    manifest_digest: &[u8; 32],
) -> Result<(), SyncError> {
    let signed_commit = checkpoint
        .signed_commit
        .as_deref()
        .ok_or(SyncError::Object("bootstrap 缺 CheckpointCommit"))?;
    let signature = checkpoint
        .signature
        .ok_or(SyncError::Object("bootstrap 缺 checkpoint 签名"))?;
    let commit = CheckpointCommit::decode(signed_commit)?;
    crate::checkpoint::require_current_checkpoint_validation(&commit)?;
    if commit.vault_id != manifest.vault_id
        || commit.checkpoint_id != checkpoint.checkpoint_id
        || commit.epoch != checkpoint.epoch
        || commit.covered_relay_seq != checkpoint.covered_seq
        || commit.manifest_object_id != checkpoint.manifest_oid
        || commit.manifest_digest != *manifest_digest
    {
        return Err(SyncError::Object("bootstrap checkpoint commit 与响应不符"));
    }
    // ADR §21：manifest 条目已带 key_epoch/total_plain_len/object_digest，可只凭 manifest
    // 重算 refs_digest；不再依赖 GET objects。
    let refs = manifest_refs(manifest);
    let refs_digest: [u8; 32] = Sha256::digest(refs_digest_input(&refs)?).into();
    if commit.refs_digest != refs_digest {
        return Err(SyncError::Object("bootstrap refs_digest 与 manifest 不符"));
    }
    if let Some(refs_digest) = checkpoint.refs_digest {
        if commit.refs_digest != refs_digest {
            return Err(SyncError::Object("bootstrap refs_digest 与 commit 不符"));
        }
    }
    let certificate = client
        .device_cert(&commit.producer_device_id)?
        .ok_or(SyncError::Object("bootstrap 缺 checkpoint 生产者证书"))?;
    let input = checkpoint_signing_input(&commit)?;
    verify_signature(&certificate.sign_ed25519_pub, &input, &signature)?;
    Ok(())
}

/// 先读第 0 片信封拿 part_count/total_len，再按实际片数重算 object_digest 并读全对象。
///
/// 这样多片 manifest 也能校验（`/bootstrap` 响应不直接给 manifest 的 part_count/digest，
/// 只能先看信封，再让签名 commit 绑定重算出的 digest）。
fn fetch_manifest<C: RelayClient>(
    client: &mut C,
    root: &RootSecretV1,
    checkpoint: &BootstrapCheckpoint,
) -> Result<(ManifestV1, [u8; 32]), SyncError> {
    let first = client.part(&checkpoint.manifest_oid, 0)?;
    let envelope = Envelope::decode(&first)?;
    if envelope.header.object_type != ObjectType::Manifest
        || envelope.header.object_id != checkpoint.manifest_oid
        || envelope.header.part_count == 0
    {
        return Err(SyncError::Object("bootstrap manifest 信封字段不符"));
    }
    if envelope.header.generation != 0 || envelope.header.sender_seq != 0 {
        return Err(SyncError::Object("bootstrap manifest 信封字段不符"));
    }
    let digest = object_digest_of(client, &checkpoint.manifest_oid, envelope.header.part_count)?;
    let (header, bytes) = fetch_object(
        client,
        root,
        ObjectType::Manifest,
        &checkpoint.manifest_oid,
        envelope.header.part_count,
        envelope.header.total_plaintext_len,
        &digest,
        &[],
    )?;
    if header.key_epoch != checkpoint.epoch {
        return Err(SyncError::Object(
            "bootstrap manifest epoch 与 checkpoint 不符",
        ));
    }
    Ok((ManifestV1::decode(&bytes)?, digest))
}

fn object_digest_of<C: RelayClient>(
    client: &mut C,
    object_id: &[u8; 32],
    part_count: u32,
) -> Result<[u8; 32], SyncError> {
    let mut digests = Vec::with_capacity(part_count as usize);
    for index in 0..part_count {
        let bytes = client.part(object_id, index)?;
        digests.push(Sha256::digest(&bytes).into());
    }
    let input = menghunote_sync_proto::object_digest_input(part_count, &digests)?;
    Ok(Sha256::digest(&input).into())
}

/// 由 manifest 重建 refs 集合（`refs_digest` 的唯一口径）。
///
/// `pub(crate)`：I 阶段的 checkpoint 级认证（`rotation_apply::verify_checkpoint_authorization`）
/// 必须复算 `refs_digest`，复用本函数而不是复制一份口径（ADR §21：manifest 条目已带
/// key_epoch/total_plain_len/object_digest，只凭 manifest 就能重算）。
pub(crate) fn manifest_refs(manifest: &ManifestV1) -> Vec<ObjectRefV1> {
    let mut refs = Vec::with_capacity(manifest.docs.len() + manifest.assets.len());
    refs.extend(manifest.docs.iter().map(|doc| ObjectRefV1 {
        object_id: doc.snapshot_object_id,
        object_type: ObjectType::Snapshot,
        generation: doc.generation,
        key_epoch: doc.key_epoch,
        part_count: doc.part_count,
        total_plain_len: doc.total_plain_len,
        object_digest: doc.object_digest,
    }));
    refs.extend(manifest.assets.iter().map(|asset| ObjectRefV1 {
        object_id: asset.object_id,
        object_type: ObjectType::Asset,
        generation: 0,
        key_epoch: asset.key_epoch,
        part_count: asset.part_count,
        total_plain_len: asset.total_plain_len,
        object_digest: asset.object_digest,
    }));
    refs
}

fn verify_manifest(
    manifest: &ManifestV1,
    vault_id: [u8; 16],
    checkpoint: &BootstrapCheckpoint,
    epoch: u64,
) -> Result<(), SyncError> {
    if manifest.vault_id != vault_id
        || manifest.checkpoint_id != checkpoint.checkpoint_id
        || manifest.covered_relay_seq != checkpoint.covered_seq
        || manifest.key_epoch != epoch
    {
        return Err(SyncError::Object("bootstrap manifest 与 checkpoint 不符"));
    }
    if manifest.min_reader > u32::try_from(MIN_READER).unwrap_or(u32::MAX) {
        return Err(SyncError::Object("bootstrap 基线要求更高的 reader"));
    }
    if manifest.docs.is_empty() {
        return Err(SyncError::Object("bootstrap manifest 没有文档"));
    }
    // 结构文档的 ID 是 storage 的固定常量：apply_baseline_batch 依赖它写 `kind='structure'`，
    // 允许 manifest 声明别的 ID 会让结构被当成正文落库。
    if manifest.structure_doc_id != STRUCTURE_DOC_ID {
        return Err(SyncError::Object(
            "bootstrap manifest structure_doc_id 非法",
        ));
    }
    let mut doc_ids = BTreeSet::new();
    let mut snapshot_ids = BTreeSet::new();
    let mut structure_docs = 0usize;
    for doc in &manifest.docs {
        if !doc_ids.insert(doc.doc_id.as_str()) || !snapshot_ids.insert(doc.snapshot_object_id) {
            return Err(SyncError::Object("bootstrap manifest 文档/对象重复"));
        }
        if doc.kind != KIND_BODY && doc.kind != KIND_STRUCTURE {
            return Err(SyncError::Object("bootstrap manifest 文档 kind 非法"));
        }
        if doc.key_epoch != epoch {
            return Err(SyncError::Object("bootstrap manifest 文档 epoch 不符"));
        }
        if doc.doc_id == STRUCTURE_DOC_ID {
            structure_docs += 1;
            if doc.kind != KIND_STRUCTURE {
                return Err(SyncError::Object("bootstrap 结构文档 kind 非法"));
            }
        } else if doc.kind == KIND_STRUCTURE {
            return Err(SyncError::Object("bootstrap manifest 含多余结构文档"));
        }
    }
    if structure_docs != 1 {
        return Err(SyncError::Object("bootstrap manifest 缺 structure 文档"));
    }
    let mut asset_objects = BTreeSet::new();
    let mut asset_hashes = BTreeSet::new();
    for asset in &manifest.assets {
        if !asset_objects.insert(asset.object_id) || !asset_hashes.insert(asset.real_sha256) {
            return Err(SyncError::Object("bootstrap manifest 资产重复"));
        }
        if asset.key_epoch != epoch {
            return Err(SyncError::Object("bootstrap manifest 资产 epoch 不符"));
        }
    }
    Ok(())
}

fn download_docs<C: RelayClient>(
    client: &mut C,
    root: &RootSecretV1,
    peer_id: u64,
    manifest: &ManifestV1,
) -> Result<Vec<VerifiedDoc>, SyncError> {
    let mut docs = Vec::with_capacity(manifest.docs.len());
    for doc in &manifest.docs {
        let (header, bytes) = fetch_self_described(
            client,
            root,
            ObjectType::Snapshot,
            &doc.snapshot_object_id,
            &doc.object_digest,
            &[],
        )?;
        if header.part_count != doc.part_count
            || header.generation != doc.generation
            || header.key_epoch != manifest.key_epoch
            || header.sender_seq != 0
        {
            return Err(SyncError::Object("bootstrap 快照信封字段不符"));
        }
        let snapshot = SnapshotV1::decode(&bytes)?;
        if !snapshot.full_history
            || snapshot.doc_id != doc.doc_id
            || snapshot.kind != doc.kind
            || snapshot.generation != doc.generation
            || snapshot.vv != doc.vv
            || snapshot.frontier != doc.frontier
        {
            return Err(SyncError::Object("bootstrap 快照与 manifest 不符"));
        }
        if snapshot.codec_version != CODEC_VERSION
            || snapshot.app_doc_schema != u32::try_from(APP_DOC_SCHEMA).unwrap_or(u32::MAX)
            || snapshot.min_reader > u32::try_from(MIN_READER).unwrap_or(u32::MAX)
        {
            return Err(SyncError::Object("bootstrap 快照 app schema/codec 不可读"));
        }
        // 实际 VV/frontier：独立空 LoroDoc import 后再比，不只信 manifest 声明。
        let actual_vv = verify_snapshot_history(&snapshot, doc)?;
        let text = if doc.kind == KIND_BODY {
            let body = BodyDoc::restore(
                doc.doc_id.clone(),
                peer_id,
                Some(&snapshot.loro_snapshot_bytes),
                &[],
            )?;
            Some(body.text())
        } else {
            None
        };
        docs.push(VerifiedDoc {
            doc_id: doc.doc_id.clone(),
            kind: doc.kind,
            snapshot,
            actual_vv,
            text,
        });
    }
    Ok(docs)
}

/// 把 snapshot 导入独立空 `LoroDoc`，校验 actual VV/frontier 与 manifest 声明一致。
///
/// 相等比较同时排除两种攻击：宣称了不存在的 op（虚高）与真实历史里缺 op。
fn verify_snapshot_history(
    snapshot: &SnapshotV1,
    manifest_doc: &ManifestDoc,
) -> Result<VersionVector, SyncError> {
    if !snapshot.full_history {
        return Err(SyncError::Object("bootstrap 快照不是 full-history"));
    }
    let doc = loro::LoroDoc::new();
    doc.import(&snapshot.loro_snapshot_bytes)
        .map_err(|_| SyncError::Object("bootstrap 快照 Loro 数据非法"))?;
    // shallow snapshot 的 VV/frontier 仍可能自洽，必须再问 Loro 的 shallow 状态。
    crate::checkpoint::ensure_snapshot_full_history(&doc)?;
    let actual_vv = doc.oplog_vv();
    let declared_vv = decode_vv(&manifest_doc.vv)?;
    if actual_vv != declared_vv {
        return Err(SyncError::Object(
            "bootstrap 快照实际 VV 与 manifest 声明不一致",
        ));
    }
    let actual_frontier = doc.state_frontiers();
    if manifest_doc.frontier.is_empty() {
        if actual_frontier.iter().next().is_some() {
            return Err(SyncError::Object(
                "bootstrap manifest frontier 为空但快照有实际 frontier",
            ));
        }
    } else if actual_frontier.encode() != manifest_doc.frontier {
        return Err(SyncError::Object(
            "bootstrap 快照 frontier 与 manifest 声明不一致",
        ));
    }
    Ok(actual_vv)
}

/// 结构语义校验：结构快照里的每篇笔记必须指向基线 manifest 内的正文文档。
fn verify_structure_semantics(
    docs: &[VerifiedDoc],
    peer_id: u64,
) -> Result<BTreeMap<String, String>, SyncError> {
    let structure = docs
        .iter()
        .find(|doc| doc.kind == KIND_STRUCTURE)
        .ok_or(SyncError::Object("bootstrap 缺 structure 文档"))?;
    let doc = StructureDoc::restore(peer_id, Some(&structure.snapshot.loro_snapshot_bytes), &[])?;
    let snapshot = doc.snapshot()?;
    let manifest_kinds: BTreeMap<&str, u64> = docs
        .iter()
        .map(|doc| (doc.doc_id.as_str(), doc.kind))
        .collect();
    let mut notes = BTreeMap::new();
    for node in &snapshot.nodes {
        if node.note_id.is_some() != node.body_doc_id.is_some() {
            return Err(SyncError::Object("bootstrap 结构节点笔记字段不成对"));
        }
        if !node.is_note() {
            continue;
        }
        let note_id = node.note_id.clone().unwrap_or_default();
        let body_doc_id = node.body_doc_id.clone().unwrap_or_default();
        if manifest_kinds.get(body_doc_id.as_str()) != Some(&KIND_BODY) {
            return Err(SyncError::Object(
                "bootstrap 结构引用的正文文档不在基线 manifest",
            ));
        }
        if notes.insert(note_id, body_doc_id).is_some() {
            return Err(SyncError::Object("bootstrap 结构快照 note_id 重复"));
        }
    }
    Ok(notes)
}

fn download_assets<C: RelayClient>(
    client: &mut C,
    root: &RootSecretV1,
    manifest: &ManifestV1,
) -> Result<Vec<VerifiedAsset>, SyncError> {
    let mut assets = Vec::with_capacity(manifest.assets.len());
    for manifest_asset in &manifest.assets {
        let expected = crate::checkpoint::asset_object_id(
            root,
            &manifest_asset.real_sha256,
            manifest_asset.byte_len,
            &manifest_asset.media_type,
        )?;
        if expected != manifest_asset.object_id {
            return Err(SyncError::Object("bootstrap 资产 object_id 与根不符"));
        }
        let (header, bytes) = fetch_self_described(
            client,
            root,
            ObjectType::Asset,
            &manifest_asset.object_id,
            &manifest_asset.object_digest,
            &[],
        )?;
        if header.part_count != manifest_asset.part_count || header.key_epoch != manifest.key_epoch
        {
            return Err(SyncError::Object(
                "bootstrap 资产片数/epoch 与 manifest 不符",
            ));
        }
        // ADR §31：ASSET 的 header producer 固定为 0，且 message_id 必须由 identical
        // asset header 确定性派生（跨设备/重传的密文字节才可能一致）。
        if header.producer_device_id != [0u8; 16]
            || header.generation != 0
            || header.sender_seq != 0
        {
            return Err(SyncError::Object("bootstrap 资产信封字段不符（§31）"));
        }
        if header.message_id != crate::upload::asset_message_id(root, &manifest_asset.object_id)? {
            return Err(SyncError::Object("bootstrap 资产 message_id 非确定性派生"));
        }
        let reference = AssetRef {
            real_sha256: manifest_asset.real_sha256,
            byte_len: manifest_asset.byte_len,
            media_type: manifest_asset.media_type.clone(),
            object: crate::upload::UploadedObject {
                object_id: manifest_asset.object_id,
                part_count: manifest_asset.part_count,
                total_plain_len: header.total_plaintext_len,
                object_digest: manifest_asset.object_digest,
            },
        };
        verify_asset_plaintext(&bytes, &reference)?;
        let body_start = 42 + manifest_asset.media_type.len();
        let data = bytes
            .get(body_start..)
            .ok_or(SyncError::Object("bootstrap 资产明文越界"))?
            .to_vec();
        assets.push(VerifiedAsset {
            object_id: manifest_asset.object_id,
            real_sha256: manifest_asset.real_sha256,
            byte_len: manifest_asset.byte_len,
            media_type: manifest_asset.media_type.clone(),
            data,
        });
    }
    Ok(assets)
}

/// 读第 0 片拿 header，再按 header 的 part_count/total_len 读全对象并校验摘要/解密。
fn fetch_self_described<C: RelayClient>(
    client: &mut C,
    root: &RootSecretV1,
    object_type: ObjectType,
    object_id: &[u8; 32],
    expected_digest: &[u8; 32],
    descriptor: &[u8],
) -> Result<(EnvelopeHeader, Vec<u8>), SyncError> {
    let first = client.part(object_id, 0)?;
    let envelope = Envelope::decode(&first)?;
    if envelope.header.object_type != object_type
        || envelope.header.object_id != *object_id
        || envelope.header.part_count == 0
    {
        return Err(SyncError::Object("bootstrap 对象 header 不符"));
    }
    fetch_object(
        client,
        root,
        object_type,
        object_id,
        envelope.header.part_count,
        envelope.header.total_plaintext_len,
        expected_digest,
        descriptor,
    )
}

/// 清掉"上一次 bootstrap 选了另一份基线"的 staging（数据是 CRDT merge，重放幂等）。
///
/// 轮换的 baseline apply 传空 `source_cursor`，这里用前缀区分，绝不误清轮换 journal。
fn reconcile_journal(
    vault: &Vault,
    checkpoint_id: &[u8; 16],
    baseline_s: u64,
    source_cursor: &str,
) -> Result<(), SyncError> {
    let Some(state) = baseline_apply_state(vault)? else {
        return Ok(());
    };
    if state.status != BaselineApplyStatus::Staging {
        return Ok(());
    }
    let same = state.checkpoint_id == *checkpoint_id
        && state.baseline_s == baseline_s
        && state.source_cursor == source_cursor;
    if same {
        return Ok(());
    }
    if state.source_cursor.starts_with(JOURNAL_SOURCE_PREFIX) {
        let _aborted = abort_baseline_apply(vault)?;
        return Ok(());
    }
    Err(SyncError::Storage(
        menghunote_storage::StorageError::SyncConflict {
            context: "已有非 bootstrap 的 baseline apply staging（轮换？），拒绝 bootstrap"
                .to_owned(),
        },
    ))
}

/// staging journal + 分批导入 docs/structure/assets；返回按 manifest 顺序的报告文本。
fn apply_selected(
    vault: &Vault,
    peer_id: u64,
    selected: &SelectedBaseline,
    fault: Option<BootstrapFault>,
) -> Result<Vec<(String, Option<String>)>, SyncError> {
    let checkpoint_id = selected.checkpoint.checkpoint_id;
    let baseline_s = selected.checkpoint.covered_seq;
    let source_cursor = format!("{JOURNAL_SOURCE_PREFIX}{}", selected.checkpoint.cursor);
    // 完整性基准：本 checkpoint 覆盖的全部文档与资产都必须出现在 apply 批里。
    // 集合由 begin 记录，任何"少导入一批"的续跑都不会被误当成完整
    // （rotation 的 finish 会直接 Conflict；bootstrap 的 finalize 另有逐项读回校验）。
    let expected = BaselineExpectation::new(
        selected.docs.iter().map(|doc| doc.doc_id.clone()),
        selected.assets.iter().map(|asset| asset.object_id),
    );
    reconcile_journal(vault, &checkpoint_id, baseline_s, &source_cursor)?;
    begin_baseline_apply(vault, &checkpoint_id, baseline_s, &source_cursor, &expected)?;

    // body → structure → asset（structure 投影依赖正文行已存在；与旧实现的顺序一致）。
    let bodies: Vec<&VerifiedDoc> = selected
        .docs
        .iter()
        .filter(|doc| doc.kind == KIND_BODY)
        .collect();
    for chunk in bodies.chunks(BOOTSTRAP_DOC_BATCH) {
        let batch: Vec<(String, Vec<u8>)> = chunk
            .iter()
            .map(|doc| (doc.doc_id.clone(), doc.snapshot.loro_snapshot_bytes.clone()))
            .collect();
        apply_baseline_batch(vault, &batch, &[])?;
    }
    if fault == Some(BootstrapFault::AfterBodies) {
        return Err(injected_fault());
    }

    let structure = selected
        .docs
        .iter()
        .find(|doc| doc.kind == KIND_STRUCTURE)
        .ok_or(SyncError::Object("bootstrap 缺 structure 文档"))?;
    apply_baseline_batch(
        vault,
        &[(
            structure.doc_id.clone(),
            structure.snapshot.loro_snapshot_bytes.clone(),
        )],
        &[],
    )?;
    if fault == Some(BootstrapFault::BeforeStructureProjection) {
        return Err(injected_fault());
    }
    let mut writer = StructureWriter::open(vault, peer_id)?;
    writer.commit("bootstrap")?;

    if !selected.assets.is_empty() {
        let assets: Vec<BaselineAsset> = selected
            .assets
            .iter()
            .map(|asset| BaselineAsset {
                object_id: asset.object_id,
                asset_hash: asset.real_sha256,
                byte_len: asset.byte_len,
                media_type: Some(asset.media_type.clone()),
                data: asset.data.clone(),
            })
            .collect();
        apply_baseline_assets(vault, &assets)?;
    }
    if fault == Some(BootstrapFault::AfterAssets) {
        return Err(injected_fault());
    }

    // 读回校验的耗时只作诊断指标（不参与决策）；失败也要记录。
    let verify_started = Instant::now();
    let verified = verify_imported(vault, peer_id, selected, fault);
    record_baseline_verify_time(verify_started.elapsed());
    match verified {
        Ok(report) => Ok(report),
        Err(error) => {
            // 不回滚已 merge 的 Loro/资产，也不改 journal.status（schema CHECK 只有
            // staging/completed）：只写失败标记，让下一次 begin 拒绝续跑，直到 abort。
            if let Err(mark_error) = mark_baseline_verify_failed(vault, &error.to_string()) {
                return Err(SyncError::Storage(mark_error));
            }
            Err(error)
        }
    }
}

/// 单篇文档的读回校验：导出点 VV ⊆ 当前本地 VV，且仅在无本地 extra 时比较正文。
///
/// - 方向固定为「快照/导出点 VV 必须被本地 VV 覆盖」：`actual_vv.sub_iter(local_vv)`
///   为空才通过；**不要求相等**。导出点之后本地新增的 op 是合法编辑（verify 不与编辑
///   互斥），相等比较会把它误判成失败；
/// - 只有 `local_vv ⊆ actual_vv`（本地没有快照之外的 op）时，Loro 才保证本地正文等于
///   快照正文，此时才做逐字节比较；有本地 extra 时正文必然已被合并改写，比较必然误报。
fn check_imported_doc(
    actual_vv: &VersionVector,
    local_vv: &VersionVector,
    expected_text: Option<&str>,
    actual_text: &str,
) -> Result<(), &'static str> {
    if actual_vv.sub_iter(local_vv).next().is_some() {
        return Err("bootstrap 导入后本地 VV 未覆盖快照");
    }
    if local_vv.sub_iter(actual_vv).next().is_none() {
        if let Some(expected) = expected_text {
            if expected != actual_text {
                return Err("bootstrap 后正文与快照不一致");
            }
        }
    }
    Ok(())
}

/// 读回校验：本地 VV 必须覆盖快照、正文/结构投影/资产必须与快照一致。
fn verify_imported(
    vault: &Vault,
    peer_id: u64,
    selected: &SelectedBaseline,
    fault: Option<BootstrapFault>,
) -> Result<Vec<(String, Option<String>)>, SyncError> {
    let mut body_texts: BTreeMap<String, String> = BTreeMap::new();
    for (index, doc) in selected
        .docs
        .iter()
        .filter(|doc| doc.kind == KIND_BODY)
        .enumerate()
    {
        let (body, head) = load_body_doc(vault, &doc.doc_id, peer_id, None)?;
        let local_vv = head.version_vector()?;
        // 测试故障注入：伪造一个「导出点有、本地没有」的 op，制造真实的 VV 未覆盖失败。
        let actual_vv = if fault == Some(BootstrapFault::VerifyVvNotCovered) && index == 0 {
            let mut mismatched = doc.actual_vv.clone();
            mismatched.insert(VERIFY_FAULT_PEER, 1);
            mismatched
        } else {
            doc.actual_vv.clone()
        };
        let text = body.text();
        if let Err(reason) = check_imported_doc(&actual_vv, &local_vv, doc.text.as_deref(), &text) {
            return Err(SyncError::Object(reason));
        }
        body_texts.insert(doc.doc_id.clone(), text);
    }

    // 结构重放一致性 + 基线声明的每篇笔记都已投影且正文存在。
    let (structure, _head) = load_structure(vault, peer_id)?;
    let merged = structure.snapshot()?;
    let merged_notes: BTreeMap<&str, &str> = merged
        .nodes
        .iter()
        .filter(|node| node.is_note())
        .filter_map(|node| Some((node.note_id.as_deref()?, node.body_doc_id.as_deref()?)))
        .collect();
    for (note_id, body_doc_id) in &selected.structure_notes {
        if merged_notes.get(note_id.as_str()) != Some(&body_doc_id.as_str()) {
            return Err(SyncError::Object("bootstrap 结构投影与快照不一致"));
        }
        if !body_texts.contains_key(body_doc_id) {
            return Err(SyncError::Object("bootstrap 结构引用的正文缺失"));
        }
    }

    for asset in &selected.assets {
        match asset_bytes(vault, asset.real_sha256)? {
            Some(stored) if stored == asset.data => {}
            _ => return Err(SyncError::Object("bootstrap 资产未正确落库")),
        }
    }

    Ok(selected
        .docs
        .iter()
        .map(|doc| {
            let text = if doc.kind == KIND_STRUCTURE {
                None
            } else {
                body_texts.get(&doc.doc_id).cloned()
            };
            (doc.doc_id.clone(), text)
        })
        .collect())
}

/// 原子推进水位（max 语义 + CAS）。
///
/// 只允许前进：`received/integrated = max(现有, S)`、`highwater = max(现有, pin, S)`；
/// 已有进度时保留现有 relay 游标（不把已消费的位置回退）。`instance/log_id/epoch/
/// control_rev/control_hash` 只允许前进或不变。任何一项回退都在**写事务之前**硬失败。
fn finalize(
    vault: &Vault,
    selected: &SelectedBaseline,
    state: &RelayState,
    chain: &[ManagerChainEntry],
    control_hash: &[u8; 32],
    pins: &BootstrapPins,
    fault: Option<BootstrapFault>,
) -> Result<(), SyncError> {
    let s = selected.checkpoint.covered_seq;
    let cursor_state = sync_cursor_state(vault)?;
    if let Some(log_id) = &cursor_state.log_id {
        if *log_id != state.log_id {
            return Err(SyncError::Storage(
                menghunote_storage::StorageError::SyncLogMismatch {
                    expected: log_id.to_vec(),
                    found: state.log_id.to_vec(),
                },
            ));
        }
    }
    let stored = BootstrapPins::from_vault(vault)?;
    if let Some(instance) = stored.instance_id {
        if instance != state.instance_id {
            return Err(SyncError::Object("bootstrap 不允许更换 relay instance"));
        }
    }
    if let Some(epoch) = stored.epoch {
        if epoch > state.epoch {
            return Err(SyncError::Object("bootstrap 不允许回退 epoch"));
        }
    }
    if let Some(rev) = stored.control_rev {
        if rev > state.control_rev {
            return Err(SyncError::Object("bootstrap 不允许回退 control_rev"));
        }
        // 与 `validate_remote` 同一口径：两类视图（同 rev 摘要 / 异 rev 历史锚）都验，
        // 不因 rev 前进就跳过旧摘要（ADR 0013 §3）。
        if let Some(anchor) = stored.control_anchor()? {
            verify_bootstrap_anchor(&anchor, state, chain, control_hash)?;
        }
    }
    if let Some(highwater) = pins.highwater {
        if highwater > state.head_seq {
            return Err(SyncError::Paused(
                crate::relay::PauseReason::RollbackDetected,
            ));
        }
    }

    let current_cursor = vault.get_meta(RELAY_CURSOR_META)?;
    let raw_received = vault.get_meta(META_SYNC_RECEIVED_SEQ)?;
    let raw_integrated = vault.get_meta(META_SYNC_INTEGRATED_SEQ)?;
    let raw_highwater = vault.get_meta(META_SYNC_CONFIRMED_HIGHWATER)?;

    let received_target = cursor_state.received_seq.max(s);
    let integrated_target = cursor_state.integrated_seq.max(s);
    let mut highwater_target = cursor_state.confirmed_highwater.unwrap_or(0).max(s);
    if let Some(pin) = pins.highwater {
        highwater_target = highwater_target.max(pin);
    }
    let has_progress = cursor_state.received_seq > 0
        || cursor_state.integrated_seq > 0
        || cursor_state
            .confirmed_highwater
            .is_some_and(|value| value > 0);
    if has_progress && current_cursor.is_none() {
        return Err(SyncError::LocalCorrupt("本地已有同步水位但缺 relay 游标"));
    }
    // 已有进度时保留现有游标：baseline 的 cursor 只用于"从零开始"的设备，避免回退。
    let write_cursor = !has_progress;

    let mut statements = Vec::new();
    let mut guards = Vec::new();
    if cursor_state.log_id.is_none() {
        push_meta_insert_absent(
            &mut statements,
            &mut guards,
            META_SYNC_LOG_ID,
            &state.log_id,
        );
    }
    if raw_received.is_none() || received_target != cursor_state.received_seq {
        push_meta_cas_u64(
            &mut statements,
            &mut guards,
            META_SYNC_RECEIVED_SEQ,
            cursor_state.received_seq,
            received_target,
        );
    }
    if raw_integrated.is_none() || integrated_target != cursor_state.integrated_seq {
        push_meta_cas_u64(
            &mut statements,
            &mut guards,
            META_SYNC_INTEGRATED_SEQ,
            cursor_state.integrated_seq,
            integrated_target,
        );
    }
    if raw_highwater.is_none() || cursor_state.confirmed_highwater != Some(highwater_target) {
        push_meta_cas_opt_u64(
            &mut statements,
            &mut guards,
            META_SYNC_CONFIRMED_HIGHWATER,
            cursor_state.confirmed_highwater,
            highwater_target,
        );
    }
    if write_cursor {
        push_meta_cas_opt_bytes(
            &mut statements,
            &mut guards,
            RELAY_CURSOR_META,
            current_cursor.as_deref(),
            selected.checkpoint.cursor.as_bytes(),
        );
    }
    if stored.instance_id.is_none() {
        push_meta_insert_absent(
            &mut statements,
            &mut guards,
            META_BOOTSTRAP_INSTANCE,
            &state.instance_id,
        );
    }
    if stored.epoch != Some(state.epoch) {
        push_meta_cas_opt_u64(
            &mut statements,
            &mut guards,
            META_BOOTSTRAP_EPOCH,
            stored.epoch,
            state.epoch,
        );
    }
    if stored.control_rev != Some(state.control_rev) {
        push_meta_cas_opt_u64(
            &mut statements,
            &mut guards,
            META_BOOTSTRAP_CONTROL_REV,
            stored.control_rev,
            state.control_rev,
        );
    }
    if stored.control_hash != Some(*control_hash) {
        push_meta_cas_opt_bytes(
            &mut statements,
            &mut guards,
            META_BOOTSTRAP_CONTROL_HASH,
            stored.control_hash.as_ref().map(<[u8; 32]>::as_slice),
            control_hash,
        );
    }

    // 模拟并发写者在水位事务提交前改写游标：CAS 必须失败且整笔回滚。
    if fault == Some(BootstrapFault::CursorCasConflict) {
        if !write_cursor {
            return Err(injected_fault());
        }
        vault.set_meta(RELAY_CURSOR_META, b"fault-cursor")?;
    }

    match vault.exec_transaction(&statements, &guards) {
        Ok(_) => Ok(()),
        Err(menghunote_storage::StorageError::GuardFailed { .. }) => Err(SyncError::Object(
            "bootstrap 水位 CAS 冲突（游标/水位已被并发写入），未推进",
        )),
        Err(error) => Err(error.into()),
    }
}

fn injected_fault() -> SyncError {
    SyncError::Object("bootstrap 注入故障（测试）")
}

fn push_meta_insert_absent(
    statements: &mut Vec<Statement>,
    guards: &mut Vec<AffectedGuard>,
    key: &str,
    value: &[u8],
) {
    let index = statements.len();
    statements.push(Statement::new(
        "INSERT INTO meta(key,value) SELECT ?1,?2 \
         WHERE NOT EXISTS(SELECT 1 FROM meta WHERE key=?1)",
        vec![Value::Text(key.to_owned()), Value::Blob(value.to_vec())],
    ));
    guards.push(AffectedGuard::exactly(index, 1));
}

fn push_meta_cas_u64(
    statements: &mut Vec<Statement>,
    guards: &mut Vec<AffectedGuard>,
    key: &str,
    old: u64,
    new: u64,
) {
    if old == 0 {
        // 0 与"meta 缺失"等价：插入，或 CAS 更新已存在的 0。
        let index = statements.len();
        statements.push(Statement::new(
            "INSERT INTO meta(key,value) VALUES(?1,?2) \
             ON CONFLICT(key) DO UPDATE SET value=excluded.value WHERE meta.value=?3",
            vec![
                Value::Text(key.to_owned()),
                Value::Blob(new.to_be_bytes().to_vec()),
                Value::Blob(old.to_be_bytes().to_vec()),
            ],
        ));
        guards.push(AffectedGuard::exactly(index, 1));
        return;
    }
    push_meta_cas_bytes(
        statements,
        guards,
        key,
        Some(&old.to_be_bytes()),
        &new.to_be_bytes(),
    );
}

fn push_meta_cas_opt_u64(
    statements: &mut Vec<Statement>,
    guards: &mut Vec<AffectedGuard>,
    key: &str,
    old: Option<u64>,
    new: u64,
) {
    match old {
        None => push_meta_insert_absent(statements, guards, key, &new.to_be_bytes()),
        Some(old) => push_meta_cas_u64(statements, guards, key, old, new),
    }
}

fn push_meta_cas_opt_bytes(
    statements: &mut Vec<Statement>,
    guards: &mut Vec<AffectedGuard>,
    key: &str,
    old: Option<&[u8]>,
    new: &[u8],
) {
    match old {
        None => push_meta_insert_absent(statements, guards, key, new),
        Some(old) => push_meta_cas_bytes(statements, guards, key, Some(old), new),
    }
}

fn push_meta_cas_bytes(
    statements: &mut Vec<Statement>,
    guards: &mut Vec<AffectedGuard>,
    key: &str,
    old: Option<&[u8]>,
    new: &[u8],
) {
    let Some(old) = old else {
        push_meta_insert_absent(statements, guards, key, new);
        return;
    };
    let index = statements.len();
    statements.push(Statement::new(
        "UPDATE meta SET value=?3 WHERE key=?1 AND value=?2",
        vec![
            Value::Text(key.to_owned()),
            Value::Blob(old.to_vec()),
            Value::Blob(new.to_vec()),
        ],
    ));
    guards.push(AffectedGuard::exactly(index, 1));
}

fn decode_vv(bytes: &[u8]) -> Result<VersionVector, SyncError> {
    if bytes.is_empty() {
        return Ok(VersionVector::new());
    }
    VersionVector::decode(bytes).map_err(|_| SyncError::Object("bootstrap 快照 VV 非法"))
}

fn read_meta_array<const N: usize>(vault: &Vault, key: &str) -> Result<Option<[u8; N]>, SyncError> {
    match vault.get_meta(key)? {
        None => Ok(None),
        Some(bytes) => bytes
            .try_into()
            .map(Some)
            .map_err(|_| SyncError::Object("bootstrap 本地 pin 长度非法")),
    }
}

fn read_meta_u64(vault: &Vault, key: &str) -> Result<Option<u64>, SyncError> {
    match read_meta_array::<8>(vault, key)? {
        None => Ok(None),
        Some(bytes) => Ok(Some(u64::from_be_bytes(bytes))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(key_epoch: u64, total_plain_len: u64) -> ManifestV1 {
        ManifestV1 {
            vault_id: [1; 16],
            key_epoch: 1,
            checkpoint_id: [2; 16],
            covered_relay_seq: 3,
            min_reader: 1,
            structure_doc_id: "structure".to_owned(),
            docs: vec![ManifestDoc {
                doc_id: "structure".to_owned(),
                kind: KIND_STRUCTURE,
                generation: 1,
                key_epoch,
                vv: vec![1],
                frontier: vec![2],
                snapshot_object_id: [3; 32],
                part_count: 1,
                total_plain_len,
                object_digest: [4; 32],
            }],
            assets: vec![],
        }
    }

    fn digest(value: &ManifestV1) -> [u8; 32] {
        Sha256::digest(refs_digest_input(&manifest_refs(value)).expect("refs")).into()
    }

    #[test]
    fn manifest_key_epoch_or_total_len_tamper_changes_refs_digest() {
        let base = digest(&manifest(1, 100));
        assert_ne!(
            digest(&manifest(2, 100)),
            base,
            "key_epoch 篡改必须改变 refs_digest"
        );
        assert_ne!(
            digest(&manifest(1, 101)),
            base,
            "total_plain_len 篡改必须改变 refs_digest"
        );
    }

    fn document() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let doc = loro::LoroDoc::new();
        doc.get_text("body")
            .insert(0, "hello")
            .expect("insert body text");
        doc.commit();
        (
            doc.export(loro::ExportMode::Snapshot).expect("snapshot"),
            doc.oplog_vv().encode(),
            doc.state_frontiers().encode(),
        )
    }

    fn snapshot(loro: Vec<u8>, vv: Vec<u8>, frontier: Vec<u8>) -> SnapshotV1 {
        SnapshotV1 {
            doc_id: "doc".to_owned(),
            kind: KIND_BODY,
            generation: 1,
            codec_version: CODEC_VERSION.to_owned(),
            app_doc_schema: u32::try_from(APP_DOC_SCHEMA).unwrap_or(1),
            min_reader: 1,
            full_history: true,
            vv,
            frontier,
            loro_snapshot_bytes: loro,
        }
    }

    fn manifest_doc(vv: Vec<u8>, frontier: Vec<u8>) -> ManifestDoc {
        ManifestDoc {
            doc_id: "doc".to_owned(),
            kind: KIND_BODY,
            generation: 1,
            key_epoch: 1,
            vv,
            frontier,
            snapshot_object_id: [7; 32],
            part_count: 1,
            total_plain_len: 1,
            object_digest: [8; 32],
        }
    }

    #[test]
    fn actual_vv_must_match_manifest_claim() {
        let (loro, vv, frontier) = document();
        verify_snapshot_history(
            &snapshot(loro.clone(), vv.clone(), frontier.clone()),
            &manifest_doc(vv.clone(), frontier.clone()),
        )
        .expect("真实历史必须通过");

        // manifest 宣称一个不存在的 peer 的 ops（虚高）：实际 import 必须识破。
        let mut inflated = VersionVector::decode(&vv).expect("vv");
        inflated.insert(0xDEAD, 9);
        assert!(
            verify_snapshot_history(
                &snapshot(loro.clone(), inflated.encode(), frontier.clone()),
                &manifest_doc(inflated.encode(), frontier.clone()),
            )
            .is_err(),
            "虚高 VV 必须被实际 Loro 导入拒绝"
        );

        // frontier 与 manifest 声明不一致。
        let mut wrong_frontier = VersionVector::new();
        wrong_frontier.insert(7, 1);
        assert!(
            verify_snapshot_history(
                &snapshot(loro, vv.clone(), frontier),
                &manifest_doc(vv, wrong_frontier.encode()),
            )
            .is_err(),
            "frontier 不一致必须拒绝"
        );
    }

    #[test]
    fn verify_accepts_concurrent_local_edits_after_export() {
        // 导出点（快照）VV：peer 7 有 2 个 op、peer 8 有 1 个。
        let mut exported_vv = VersionVector::new();
        exported_vv.insert(7, 2);
        exported_vv.insert(8, 1);

        // 导出点之后本地继续编辑：当前 VV 严格包含导出点，但不相等。
        let mut local_vv = exported_vv.clone();
        local_vv.insert(9, 1);
        assert_ne!(local_vv, exported_vv);
        check_imported_doc(
            &exported_vv,
            &local_vv,
            Some("导出点正文"),
            "导出点正文 + 本地新编辑",
        )
        .expect("导出点 VV ⊆ 当前 VV 时必须接受并发本地编辑（不要求相等）");

        // 没有本地 extra（VV 相等）时才逐字节比较正文：不一致必须失败。
        assert!(
            check_imported_doc(&exported_vv, &exported_vv, Some("期望正文"), "实际正文").is_err(),
            "无本地 extra 时正文不一致必须失败"
        );
        check_imported_doc(&exported_vv, &exported_vv, Some("同一正文"), "同一正文")
            .expect("无本地 extra 且正文一致必须通过");

        // 当前 VV 缺少导出点里的 op：方向不能反（不是相等/双向比较）。
        let mut missing_vv = VersionVector::new();
        missing_vv.insert(7, 2);
        assert!(
            check_imported_doc(&exported_vv, &missing_vv, Some("正文"), "正文").is_err(),
            "导出点 VV 未被本地覆盖时必须失败"
        );
    }

    #[test]
    fn full_history_is_mandatory() {
        let (loro, vv, frontier) = document();
        let mut value = snapshot(loro, vv.clone(), frontier.clone());
        value.full_history = false;
        assert!(verify_snapshot_history(&value, &manifest_doc(vv, frontier)).is_err());
    }

    #[test]
    fn structure_semantics_require_referenced_body_in_manifest() {
        let peer = 7u64;
        let structure = menghunote_storage::StructureDoc::create(peer).expect("structure");
        let seed = structure
            .create_note("标题", 1_700_000_000)
            .expect("create note");
        let bytes = structure.export_snapshot().expect("snapshot");
        let doc = VerifiedDoc {
            doc_id: STRUCTURE_DOC_ID.to_owned(),
            kind: KIND_STRUCTURE,
            snapshot: SnapshotV1 {
                doc_id: STRUCTURE_DOC_ID.to_owned(),
                kind: KIND_STRUCTURE,
                generation: 1,
                codec_version: CODEC_VERSION.to_owned(),
                app_doc_schema: 1,
                min_reader: 1,
                full_history: true,
                vv: Vec::new(),
                frontier: Vec::new(),
                loro_snapshot_bytes: bytes,
            },
            actual_vv: VersionVector::new(),
            text: None,
        };
        // 结构引用了 seed.body_doc_id，但基线 manifest 里没有它：必须拒绝。
        assert!(verify_structure_semantics(std::slice::from_ref(&doc), peer).is_err());

        // 正文在 manifest 里时通过，并给出 note_id -> body_doc_id。
        let body = VerifiedDoc {
            doc_id: seed.body_doc_id.clone(),
            kind: KIND_BODY,
            snapshot: SnapshotV1 {
                doc_id: seed.body_doc_id.clone(),
                kind: KIND_BODY,
                generation: 1,
                codec_version: CODEC_VERSION.to_owned(),
                app_doc_schema: 1,
                min_reader: 1,
                full_history: true,
                vv: Vec::new(),
                frontier: Vec::new(),
                loro_snapshot_bytes: Vec::new(),
            },
            actual_vv: VersionVector::new(),
            text: Some(String::new()),
        };
        let notes = verify_structure_semantics(&[doc, body], peer).expect("结构语义通过");
        assert_eq!(notes.get(&seed.note_id), Some(&seed.body_doc_id));
    }

    #[test]
    fn manifest_structure_doc_must_be_storage_constant() {
        let mut value = manifest(1, 100);
        value.structure_doc_id = "other".to_owned();
        let checkpoint = BootstrapCheckpoint {
            checkpoint_id: value.checkpoint_id,
            epoch: value.key_epoch,
            covered_seq: value.covered_relay_seq,
            manifest_oid: [8; 32],
            refs_digest: None,
            cursor: "c3".to_owned(),
            signed_commit: None,
            signature: None,
        };
        assert!(verify_manifest(&value, value.vault_id, &checkpoint, 1).is_err());
    }

    #[test]
    fn control_head_hash_binds_coordinates_and_chain() {
        let state = RelayState {
            vault_id: [1; 16],
            log_id: [2; 16],
            instance_id: [3; 16],
            epoch: 4,
            control_rev: 5,
            auth_rev: 1,
            head_seq: 10,
            gc_floor: 0,
            current_rotation_id: None,
        };
        let base = control_head_hash(&state, &[]);
        let mut other_instance = state.clone();
        other_instance.instance_id = [9; 16];
        assert_ne!(control_head_hash(&other_instance, &[]), base);
        let mut other_rev = state.clone();
        other_rev.control_rev = 6;
        assert_ne!(control_head_hash(&other_rev, &[]), base);
        let entry = ManagerChainEntry {
            rev: 2,
            pubkey: [7; 32],
            transition: vec![1, 2, 3],
            old_rev: 1,
            old_signature: Some([4; 64]),
            new_signature: None,
        };
        assert_ne!(control_head_hash(&state, &[entry]), base);
    }
}

`````


## `crates/storage/tests/session_flush.rs`

`````rust
//! 后台 flush 与 durable 契约：异步票据、ACK 顺序、崩溃后重放。

mod support;

use std::time::{Duration, Instant};

use menghunote_storage::session::{
    DocumentSession, EditOutcome, EditRequest, ExternalCommitTicket, FlushReason, GateError,
    GateState, LiveRev, ScalarEdit, SessionEpoch, SessionEvent, SessionOptions,
};
use menghunote_storage::{
    append_updates, load_body_doc, load_body_state, materialize_body, BodyDoc, UpdateRecord, Value,
    Vault,
};
use support::*;

fn options(peer: u64) -> SessionOptions {
    SessionOptions {
        peer_id: peer,
        session_epoch: SessionEpoch(3),
        ..SessionOptions::default()
    }
}

fn edit(start: usize, end: usize, replacement: &str) -> ScalarEdit {
    ScalarEdit {
        start: menghunote_text::UnicodeScalar::new(start),
        end: menghunote_text::UnicodeScalar::new(end),
        replacement: replacement.to_owned(),
    }
}

fn request(
    session: &DocumentSession,
    command: &str,
    base: u64,
    edits: Vec<ScalarEdit>,
) -> EditRequest {
    EditRequest {
        doc_id: session.doc_id().to_owned(),
        generation: session.generation(),
        session_epoch: session.session_epoch(),
        command_id: command.to_owned(),
        base_live_rev: LiveRev(base),
        edits,
        origin: "local".to_owned(),
        undo_group: None,
    }
}

fn open_session(vault: &Vault, options: SessionOptions) -> DocumentSession {
    DocumentSession::open(vault, "doc-1", options).expect("打开会话")
}

#[test]
fn flush_returns_ticket_without_waiting_for_sqlite() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let options = SessionOptions {
        flush_delay: Some(Duration::from_millis(300)),
        ..options(PEER_A)
    };
    let mut session = open_session(&vault, options);
    session.apply(request(&session, "c1", 0, vec![edit(0, 0, "甲")]));

    let started = Instant::now();
    let ticket = session.flush(FlushReason::Manual);
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_millis(150),
        "flush 必须在调用线程立即返回票据（实际 {elapsed:?}）"
    );
    assert_eq!(ticket.target_live_rev, LiveRev(1));
    assert!(
        session.events().is_empty(),
        "票据返回时后台还没写完（延迟注入生效）"
    );

    // ACK 只会异步到达
    let ack = session
        .wait_for_durable(ticket.target_live_rev, Duration::from_secs(5))
        .expect("应收到 DurableAck");
    assert_eq!(ack.through_live_rev, LiveRev(1));
    assert_eq!(ack.head_seq, 1);
    assert!(!ack.head_frontier.is_empty(), "ACK 要带 durable frontier");
    assert_eq!(session.durable_head_seq(), 1);
    assert_eq!(session.durable_through(), LiveRev(1));
}

#[test]
fn durable_acks_are_monotonic_and_cover_each_target() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let mut session = open_session(&vault, options(PEER_A));

    let mut acks = Vec::new();
    for (index, ch) in ["一", "二", "三"].iter().enumerate() {
        let command = format!("c{}", index + 1);
        let expected_rev = LiveRev(index as u64 + 1);
        let outcome = session.apply(request(
            &session,
            &command,
            index as u64,
            vec![edit(index, index, ch)],
        ));
        assert!(matches!(outcome, EditOutcome::Applied(_)));
        let ticket = session.flush(FlushReason::Interval);
        assert_eq!(ticket.target_live_rev, expected_rev);
        let ack = session
            .wait_for_durable(ticket.target_live_rev, Duration::from_secs(5))
            .expect("应收到 DurableAck");
        assert_eq!(ack.through_live_rev, expected_rev);
        assert_eq!(ack.head_seq, index as u64 + 1, "head_seq 逐批递增");
        acks.push(ack);
    }

    // ACK 的批次号与水位严格递增
    for pair in acks.windows(2) {
        assert!(pair[1].batch_id > pair[0].batch_id, "batch 必须递增");
        assert!(
            pair[1].through_live_rev > pair[0].through_live_rev,
            "through_live_rev 必须递增"
        );
    }
    assert_eq!(session.markdown(), "一二三");
    assert_eq!(session.pending_len(), 0);
}

#[test]
fn crash_after_a_before_b_recovers_on_reopen() {
    let (dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let mut session = open_session(&vault, options(PEER_A));
    session.apply(request(
        &session,
        "c1",
        0,
        vec![edit(0, 0, "A中😀e\u{301}")],
    ));
    let ticket = session.flush(FlushReason::Manual);
    session
        .wait_for_durable(ticket.target_live_rev, Duration::from_secs(5))
        .expect("durable");

    // A 已提交、B 未完成：此时崩溃（直接丢会话与库句柄）
    assert!(load_body_state(&vault, "note-1")
        .expect("状态")
        .body_md
        .is_none());
    drop(session);
    drop(vault);

    // 重开：正文从 loro_updates 重放出来，durable 水位还在
    let vault = open_vault(&vault_path(&dir));
    let session = open_session(&vault, options(PEER_A));
    let snapshot = session.snapshot();
    assert_eq!(snapshot.markdown, "A中😀e\u{301}");
    assert_eq!(snapshot.durable_head_seq, 1);
    assert_eq!(snapshot.live_rev, LiveRev(0), "live_rev 是新会话的基准");

    // B 物化补齐
    materialize_body(&vault, "note-1", "doc-1", 1, PEER_A).expect("B 物化");
    let state = load_body_state(&vault, "note-1").expect("状态");
    assert_eq!(state.body_md.as_deref(), Some("A中😀e\u{301}"));
    assert_eq!(state.body_seq, 1);
}

#[test]
fn unflushed_edits_are_lost_on_reopen_but_durable_prefix_survives() {
    let (dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let mut session = open_session(&vault, options(PEER_A));

    session.apply(request(&session, "c1", 0, vec![edit(0, 0, "甲")]));
    session.apply(request(&session, "c2", 1, vec![edit(1, 1, "乙")]));
    let ticket = session.flush(FlushReason::Manual);
    session
        .wait_for_durable(ticket.target_live_rev, Duration::from_secs(5))
        .expect("durable");

    // 这两条没 flush：只存在于内存与 Loro 的未 ACK 状态
    session.apply(request(&session, "c3", 2, vec![edit(2, 2, "丙")]));
    session.apply(request(&session, "c4", 3, vec![edit(3, 3, "丁")]));
    assert_eq!(session.markdown(), "甲乙丙丁");
    drop(session);
    drop(vault);

    // 重开：只保留已 ACK 的连续前缀（甲乙），未 ACK 的丙丁丢掉——「只丢连续前缀之后」
    let vault = open_vault(&vault_path(&dir));
    let session = open_session(&vault, options(PEER_A));
    assert_eq!(session.markdown(), "甲乙");
    // 两条 update 各自占一个 seq
    assert_eq!(session.snapshot().durable_head_seq, 2);
    assert_eq!(
        vault
            .query(
                "SELECT count(*) FROM loro_updates WHERE doc_id = 'doc-1'",
                &[]
            )
            .expect("count")
            .len(),
        1
    );
    assert_eq!(
        count(
            &vault,
            "SELECT count(*) FROM loro_updates WHERE doc_id = 'doc-1'",
            &[]
        ),
        2,
        "只有已 ACK 的两条 update 落库"
    );
}

#[test]
fn flush_without_pending_edits_is_harmless() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let mut session = open_session(&vault, options(PEER_A));

    let ticket = session.flush(FlushReason::Manual);
    assert_eq!(ticket.target_live_rev, LiveRev(0));
    assert!(session
        .wait_for_durable(ticket.target_live_rev, Duration::from_secs(5))
        .is_some());
    assert_eq!(session.durable_head_seq(), 0);
    assert_eq!(session.markdown(), "");
}

#[test]
fn remote_event_is_pushed_to_the_queue() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let mut session = open_session(&vault, options(PEER_A));

    let remote = BodyDoc::create("doc-1", PEER_B).expect("远端副本");
    remote.insert_scalars(0, "远端").expect("远端编辑");
    let update = remote.export_all_updates().expect("导出");
    session
        .apply_remote(&update)
        .expect("导入")
        .expect("有变更");

    let events = session.events();
    let remote_events = events
        .iter()
        .filter(|event| matches!(event, SessionEvent::Remote(_)))
        .count();
    assert_eq!(remote_events, 1, "远端变更要进事件队列");
    assert!(events
        .iter()
        .any(|event| matches!(event, SessionEvent::Remote(delta) if delta.to_rev == LiveRev(1))));
    assert!(session.events().is_empty(), "事件队列是取走语义");
}

/// 把闸门驱到明确的失败态（时间只作为上限，不用 sleep 当收敛判据）。
fn drive_gate_to_failure(
    session: &mut DocumentSession,
    ticket: &ExternalCommitTicket,
) -> GateError {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match session.poll_external_commit(ticket) {
            GateState::Failed(error) => return error,
            GateState::Flushing | GateState::WaitingIME => std::thread::yield_now(),
            other => panic!("闸门没有给出明确失败：{other:?}"),
        }
        assert!(Instant::now() < deadline, "闸门 10s 内没有给出明确失败");
    }
}

/// 读 flusher 失败原因（闸门是 UI/驱动读取 `last_write_error` 的既有入口）。
fn flush_failure_message(session: &mut DocumentSession, ticket: &ExternalCommitTicket) -> String {
    match drive_gate_to_failure(session, ticket) {
        GateError::FlushFailed { message, .. } => message,
        other => panic!("期望 GateError::FlushFailed，实际 {other:?}"),
    }
}

/// P1-3 生产接线：head 被外部推进后，flusher 必须在同一个后台线程里
/// `load_head` + 重建 sink，并重试**同一前缀**一次；成功后正常裁剪/推进 durable + ACK。
///
/// 外部推进用「同一份远端 update 已经先落库、会话又把它导入了 live」构造：新 head 的
/// ops 全部落在未 ACK 前缀的 `end_vv` 内，因此原样重试不会写坏 `head_vv`/`frontier`
/// （下面的完整重放会验证这一点）。
#[test]
fn flush_recovers_from_head_conflict_by_rebuilding_sink() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let mut session = open_session(&vault, options(PEER_A));

    // 会话先把远端 update 导入 live（journal 里留下同一批 op 的未 ACK 增量）。
    let remote = BodyDoc::create("doc-1", PEER_B).expect("远端副本");
    remote.insert_scalars(0, "远端").expect("远端编辑");
    let remote_bytes = remote.export_all_updates().expect("导出远端 update");
    let remote_update = pending(&remote, &loro::VersionVector::default()).expect("远端增量");
    session
        .apply_remote(&remote_bytes)
        .expect("导入远端 update")
        .expect("必须有变更");

    // 外部 writer（同步引擎 integrate / 另一个 Vault）把同一份 update 先落库：head 0→1。
    let advanced = append_updates(
        &vault,
        "doc-1",
        0,
        &[UpdateRecord::new("external-1", remote_update)],
    )
    .expect("外部推进 head");
    assert_eq!(advanced.head_seq, 1);
    assert_eq!(
        session.durable_head_seq(),
        0,
        "会话的 CAS 基准仍停在打开时的 head 0"
    );

    // 修复前：这里每次都拿旧 head 0 撞 CAS —— 不写库、不裁剪、不 ACK，下一次还是旧 head。
    let ticket = session.flush(FlushReason::Manual);
    let ack = session
        .wait_for_durable(ticket.target_live_rev, Duration::from_secs(5))
        .expect("HeadConflict 后必须自动重建 sink 并发出 DurableAck");
    assert_eq!(ack.through_live_rev, LiveRev(1));
    assert_eq!(
        ack.head_seq, 2,
        "重试写在外部那一条之后（同一前缀、新 CAS 基准）"
    );
    assert_eq!(session.durable_head_seq(), 2);
    assert_eq!(session.durable_through(), LiveRev(1));
    assert_eq!(session.pending_len(), 0, "成功后必须裁剪前缀");
    assert_eq!(session.markdown(), "远端");

    // 正文/head 正确：完整重放必须通过 `loro_docs.head_frontier` 的完整性校验。
    let (replayed, head) = load_body_doc(&vault, "doc-1", PEER_A, None).expect("重放正文");
    assert_eq!(replayed.text(), "远端");
    assert_eq!(head.head_seq, 2);
    let head_vv = head.version_vector().expect("解码 head vv");
    let live_vv = loro::VersionVector::decode(&session.snapshot().live_vv).expect("解码 live vv");
    assert_eq!(
        head_vv, live_vv,
        "durable head 的 ops 必须与本会话 live 一致"
    );
    assert_eq!(
        count(
            &vault,
            "SELECT count(*) FROM loro_updates WHERE doc_id = 'doc-1'",
            &[]
        ),
        2,
        "外部一条 + 自动重建后重试的一条"
    );
}

/// P1-3 反向边界：外部 head 含本会话 live **未知**的并发 ops 时，原样重试会写坏
/// `head_vv`/`frontier`（重放完整性校验直接 Corrupt），因此自动重建必须拒绝并进入
/// 不可恢复态：`last_write_error` 明确提示「需要重开文档」、journal 原样保留、
/// 后续 flush 立即失败且不再写库。
#[test]
fn flush_reports_unrecoverable_after_bounded_rebuild() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let mut session = open_session(&vault, options(PEER_A));

    session.apply(request(&session, "c1", 0, vec![edit(0, 0, "本地")]));

    // 外部 writer 写入本会话完全不知道的并发 ops：head 0→1。
    let external = BodyDoc::create("doc-1", PEER_B).expect("外部副本");
    external.insert_scalars(0, "远端").expect("外部编辑");
    let external_update = pending(&external, &loro::VersionVector::default()).expect("外部增量");
    append_updates(
        &vault,
        "doc-1",
        0,
        &[UpdateRecord::new("external-1", external_update)],
    )
    .expect("外部推进 head");

    // 闸门必须收敛到 Failed 并带出可执行的原因；修复前只会带出裸 HeadConflict，
    // 且同一个失败会永久重复（journal 不裁剪、durable 水位不前进）。
    let ticket = session.begin_external_commit().expect("begin");
    let message = flush_failure_message(&mut session, &ticket);
    assert!(
        message.contains("需要重开文档"),
        "失败原因必须明确提示重开：{message}"
    );
    assert!(
        message.contains("未知的并发 ops"),
        "必须说明为什么不能安全自动重建：{message}"
    );

    assert_eq!(session.pending_len(), 1, "不可恢复时 journal 必须原样保留");
    let rows = || {
        count(
            &vault,
            "SELECT count(*) FROM loro_updates WHERE doc_id = 'doc-1'",
            &[],
        )
    };
    assert_eq!(rows(), 1, "只有外部那一行落库（旧前缀不得被硬写）");
    assert_eq!(head(&vault, "doc-1").head_seq, 1, "库中 head 不得被改写");

    // 后续 flush 立即失败：不发 ACK、不再写库（不能无限重试/无限写）。
    for _ in 0..3 {
        let attempt = session.flush(FlushReason::Manual);
        assert!(
            session
                .wait_for_durable(attempt.target_live_rev, Duration::from_millis(200))
                .is_none(),
            "不可恢复后不得再发出 DurableAck"
        );
        assert_eq!(rows(), 1, "不可恢复后不得再写 loro_updates");
        assert_eq!(head(&vault, "doc-1").head_seq, 1, "不可恢复后不得推进 head");
    }

    // 同一个错误仍可被闸门读到（不是一次性的诊断字符串）。
    match session.poll_external_commit(&ticket) {
        GateState::Failed(GateError::FlushFailed { message: again, .. }) => {
            assert_eq!(again, message, "不可恢复状态必须稳定可读");
        }
        other => panic!("期望持续的 Failed，实际 {other:?}"),
    }
    assert_eq!(
        session.markdown(),
        "本地",
        "live 保持会话内容，靠关闭并重开文档与库对账"
    );
}

/// P1-3 的另一个不可恢复分支：自动重建要读新 head，读不到/解码失败同样必须 fail-closed
/// （不尝试写、不无限重试），并明确提示需要重开文档。
#[test]
fn flush_reports_unrecoverable_when_new_head_is_unreadable() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    let mut session = open_session(&vault, options(PEER_A));
    session.apply(request(&session, "c1", 0, vec![edit(0, 0, "本地")]));

    // 外部推进 head 后把 head_vv 写坏：CAS 冲突后能读到行，但 VV 解码必然失败。
    let external = BodyDoc::create("doc-1", PEER_B).expect("外部副本");
    external.insert_scalars(0, "远端").expect("外部编辑");
    let external_update = pending(&external, &loro::VersionVector::default()).expect("外部增量");
    append_updates(
        &vault,
        "doc-1",
        0,
        &[UpdateRecord::new("external-1", external_update)],
    )
    .expect("外部推进 head");
    vault
        .exec(
            "UPDATE loro_docs SET head_vv = x'01' WHERE doc_id = ?1",
            &[Value::Text("doc-1".to_owned())],
        )
        .expect("写坏 head_vv");

    let ticket = session.begin_external_commit().expect("begin");
    let message = flush_failure_message(&mut session, &ticket);
    assert!(
        message.contains("需要重开文档"),
        "失败原因必须明确提示重开：{message}"
    );
    assert!(
        message.contains("解码失败"),
        "必须说明新 head 读不出来：{message}"
    );
    assert_eq!(session.pending_len(), 1, "不可恢复时 journal 必须原样保留");
    assert_eq!(
        count(
            &vault,
            "SELECT count(*) FROM loro_updates WHERE doc_id = 'doc-1'",
            &[]
        ),
        1,
        "拒绝重建后不得写旧前缀"
    );
}

`````


## `crates/storage/tests/body_doc_detached_oplog.rs`

`````rust
//! 任务 2 P0：detached oplog 测试矩阵（只加测试，不改生产代码）。
//!
//! `BodyDoc::import_oplog_only` / `pending_update_from_oplog` 是 `pub(crate)`，生产里唯一
//! 调用方是 baseline apply 的 `existing_local_extra` 分支（见
//! `crates/storage/src/baseline_apply.rs`）。本文件不放大可见性、不改生产代码，而是驱动
//! 该分支真正落库的 `baseline_merge` 行来验证语义：
//!
//! 1. **detached 路径**：`BodyDoc::restore(基线快照)` → `detach` → `import(本地差集)` →
//!    `pending_update_from_oplog(本地 VV)`。产物就是 `loro_updates` 里
//!    `batch_id == BASELINE_MERGE_BATCH` 的那一行（payload / end_frontier / end_vv）。
//! 2. **attached 参考**：`BodyDoc::restore(基线快照) + import(本地差集)`——同一条本地
//!    增量，改成 attached 导入。
//! 3. **attached 重放**：`BodyDoc::restore(None, 库里全部 update)` 与生产读取路径
//!    `load_body_doc`。
//!
//! 每个用例都断言：detached 的 `end_frontier` 与 attached 的 frontier 逐字节相等，
//! `end_vv` 与 attached 的 VV 语义相等（`VersionVector::encode` 的字节顺序取决于
//! import 顺序，不是规范形式，见 [`canonical_vv`]）；三份 attached 结果的**正文逐字节
//! 相同**；本地与基线内容不丢、不重复。
//!
//! LoroTree 用例：`BodyDoc` 只暴露正文 text，`StructureDoc` 不暴露 detach，因此按任务
//! 约定用 `loro` 直接构造 `LoroDoc` / `LoroTree`，验证 `import_oplog_only` 依赖的同一条
//! CRDT 不变量：detach 后 import 只写 oplog；从 oplog 导出的差集 attached 重放后，
//! frontier / VV 与树的可见形状（遍历结果）都与 attached 合并完全一致。

mod support;

use loro::{ExportMode, LoroDoc, LoroTree, LoroValue, TreeID, VersionVector};
use support::*;

use menghunote_storage::{
    apply_baseline_batch, begin_baseline_apply, doc_updates_since, finish_baseline_apply,
    load_body_doc, BaselineApplyFinish, BaselineExpectation, BodyDoc, StoredLoroUpdate, Vault,
    BASELINE_MERGE_BATCH,
};

/// 本地设备（attached 侧）。
const A_PEER: u64 = PEER_A;
/// 远端基线导出方（与本地不同的 peer）。
const B_PEER: u64 = PEER_B;

// ---------------------------------------------------------------------------
// 正文（LoroText）用例的脚手架
// ---------------------------------------------------------------------------

/// 在内存里构造一篇正文（不落库），用作「远端基线」。
fn memory_doc(doc_id: &str, peer: u64, text: &str) -> BodyDoc {
    let doc = BodyDoc::create(doc_id, peer).expect("创建内存文档");
    if !text.is_empty() {
        doc.insert_scalars(0, text).expect("写远端基线正文");
    }
    doc
}

/// begin 一份「只含这一篇文档」的 baseline staging。
fn begin_apply(vault: &Vault, doc_id: &str, baseline_s: u64, cursor: &str) {
    let expected = BaselineExpectation::new(
        std::iter::once(doc_id.to_owned()),
        std::iter::empty::<[u8; 32]>(),
    );
    let checkpoint_id: [u8; 16] = [baseline_s as u8; 16];
    begin_baseline_apply(vault, &checkpoint_id, baseline_s, cursor, &expected)
        .expect("begin baseline apply");
}

/// 快照蕴含的 VV（生产 `baseline_vv` 的来源：restore snapshot 后取 vv）。
fn snapshot_vv(snapshot: &[u8]) -> VersionVector {
    BodyDoc::restore("vv-probe", B_PEER, Some(snapshot), &[])
        .expect("基线快照可恢复")
        .vv()
}

/// VV 的规范表示：`VersionVector::encode` 的字节顺序取决于 import 顺序（不是规范形式），
/// 所以按 peer 排序后比较「peer -> counter」集合；语义相等即等价。
fn canonical_vv(bytes: &[u8]) -> Vec<(u64, i32)> {
    let vv = VersionVector::decode(bytes).expect("VV 可解码");
    let mut entries: Vec<(u64, i32)> = vv.iter().map(|(peer, counter)| (*peer, *counter)).collect();
    entries.sort_unstable();
    entries
}

/// `loro_updates` 里 baseline merge 追加的那一行。
struct DetachedMerge {
    /// payload 由 `pending_update_from_oplog` 导出，end_frontier / end_vv 取 **oplog** 版本。
    merged: StoredLoroUpdate,
    /// 本地相对基线 VV 的差集（与生产 detached 分支 import 的字节同源）。
    local_delta: Vec<u8>,
}

/// 驱动生产 `existing_local_extra`（detached）分支，并取回它落库的差集。
fn merge_detached(
    vault: &Vault,
    doc_id: &str,
    snapshot: &[u8],
    local: &BodyDoc,
    baseline_s: u64,
    cursor: &str,
) -> DetachedMerge {
    let local_delta = local
        .pending_update(&snapshot_vv(snapshot))
        .expect("导出本地差集")
        .payload;
    assert!(
        !local_delta.is_empty(),
        "本地必须在基线之外还有 op，否则生产走 attached 分支，测不到 detached"
    );
    begin_apply(vault, doc_id, baseline_s, cursor);
    apply_baseline_batch(vault, &[(doc_id.to_owned(), snapshot.to_vec())], &[])
        .expect("应用基线快照");
    let merged = doc_updates_since(vault, doc_id, 0)
        .expect("读 loro_updates")
        .into_iter()
        .rfind(|update| update.batch_id == BASELINE_MERGE_BATCH)
        .expect("detached 分支必须落一条 baseline_merge 差集");
    assert!(!merged.payload.is_empty(), "detached 导出的差集不能为空");

    // 分支守卫：detached 合并后的 oplog frontier 必须同时包含本地与基线两个 peer。
    // 若生产误走 attached 分支，这里只会是基线 state 的单 peer frontier。
    let frontier = loro::Frontiers::decode(&merged.end_frontier).expect("frontier 可解码");
    let mut peers: Vec<u64> = frontier.iter().map(|id| id.peer).collect();
    peers.sort_unstable();
    peers.dedup();
    assert_eq!(
        peers,
        vec![A_PEER, B_PEER],
        "detached 分支未生效？合并后的 oplog frontier 必须同时包含两个 peer"
    );

    DetachedMerge {
        merged,
        local_delta,
    }
}

/// 断言 detached 结果与 attached 路径完全等价，返回合并后的正文。
///
/// attached 侧有三条证据链，全部要与 detached 的 frontier 逐字节一致、VV 语义一致：
/// 1. 基线副本 attached 导入同一条本地差集；
/// 2. 库里全部 update 重新 restore（attached 重放）；
/// 3. 生产读取入口 `load_body_doc`。
fn assert_detached_matches_attached(
    vault: &Vault,
    doc_id: &str,
    snapshot: &[u8],
    parts: &DetachedMerge,
) -> String {
    // 1) attached 参考：基线副本 + attached 导入本地差集。
    let attached =
        BodyDoc::restore(doc_id, A_PEER, Some(snapshot), &[]).expect("attached 基线副本");
    attached
        .import(&parts.local_delta)
        .expect("attached 导入本地差集");
    let attached_text = attached.text();
    assert_eq!(
        canonical_vv(&attached.vv().encode()),
        canonical_vv(&parts.merged.end_vv),
        "detached oplog VV 与 attached 合并 VV 不一致"
    );
    assert_eq!(
        attached.frontiers().encode(),
        parts.merged.end_frontier,
        "detached oplog frontier 与 attached 合并 frontier 不一致"
    );

    // 2) attached 重放：库里全部 update（本地 update + detached 导出的差集）。
    let payloads: Vec<Vec<u8>> = doc_updates_since(vault, doc_id, 0)
        .expect("读全部 update")
        .into_iter()
        .map(|update| update.payload)
        .collect();
    let replayed = BodyDoc::restore(doc_id, A_PEER, None, &payloads).expect("attached 重放");
    assert_eq!(
        replayed.text(),
        attached_text,
        "attached 重放文本与 attached 合并文本不是逐字节相同"
    );
    assert_eq!(
        canonical_vv(&replayed.vv().encode()),
        canonical_vv(&parts.merged.end_vv)
    );
    assert_eq!(replayed.frontiers().encode(), parts.merged.end_frontier);

    // 3) 生产读取路径：文本、frontier、VV 必须与 attached 参考完全一致。
    let (loaded, head) = load_body_doc(vault, doc_id, A_PEER, None).expect("生产重放");
    assert_eq!(loaded.text(), attached_text, "load_body_doc 文本不一致");
    assert_eq!(
        canonical_vv(&loaded.vv().encode()),
        canonical_vv(&parts.merged.end_vv)
    );
    assert_eq!(loaded.frontiers().encode(), parts.merged.end_frontier);
    assert_eq!(
        head.head_frontier, parts.merged.end_frontier,
        "head_frontier 必须来自 detached 合并后的 oplog frontier"
    );
    assert_eq!(
        canonical_vv(&head.head_vv),
        canonical_vv(&parts.merged.end_vv),
        "head_vv 必须来自 detached 合并后的 oplog VV"
    );

    attached_text
}

// ---------------------------------------------------------------------------
// 1. 两个 peer 并发编辑同一 LoroText
// ---------------------------------------------------------------------------

#[test]
fn concurrent_two_peer_text_inserts_match_attached_byte_for_byte() {
    let (_dir, vault) = fresh_vault();
    // 本地 peer A：独立文档，与基线完全不共享历史（最纯粹的并发同位置插入）。
    let local = seed_note(&vault, "note-1", "doc-1");
    local.insert_scalars(0, "LOCAL").expect("本地编辑");
    commit_now(&vault, &local, "local-batch");

    // 远端 peer B：导出基线快照。
    let remote = memory_doc("doc-1", B_PEER, "REMOTE");
    let snapshot = remote.export_snapshot().expect("基线快照");

    let parts = merge_detached(&vault, "doc-1", &snapshot, &local, 5, "cursor-1");
    let text = assert_detached_matches_attached(&vault, "doc-1", &snapshot, &parts);

    assert_eq!(
        text.matches("LOCAL").count(),
        1,
        "本地文本必须恰好出现一次：{text:?}"
    );
    assert_eq!(
        text.matches("REMOTE").count(),
        1,
        "基线文本必须恰好出现一次：{text:?}"
    );
    assert!(
        text.is_ascii(),
        "纯 ASCII 合并后不得引入非 ASCII 字节：{text:?}"
    );
    assert_eq!(
        text.len(),
        "LOCAL".len() + "REMOTE".len(),
        "字节数必须是两段 ASCII 之和：{text:?}"
    );
    assert_eq!(text.chars().count(), 11);
}

#[test]
fn local_op_depending_on_baseline_matches_attached_after_detached_merge() {
    let (_dir, vault) = fresh_vault();
    // 远端：先导出 base，再在本地收不到的情况下追加 -r2（新基线是原来 + 这一笔）。
    let remote = memory_doc("doc-1", B_PEER, "base");
    let partial = remote.export_snapshot().expect("base 快照");
    remote.insert_scalars(4, "-r2").expect("远端第二笔");
    let full = remote.export_snapshot().expect("full 快照");

    // 本地：先收到 base，再在 base 之上编辑——本地 op 因果依赖基线 op。
    let _ = seed_note(&vault, "note-1", "doc-1");
    let local = BodyDoc::restore("doc-1", A_PEER, Some(&partial), &[]).expect("本地副本");
    local.insert_scalars(4, "-local").expect("本地并发编辑");
    commit_now(&vault, &local, "local-batch");

    let parts = merge_detached(&vault, "doc-1", &full, &local, 6, "cursor-2");
    let text = assert_detached_matches_attached(&vault, "doc-1", &full, &parts);

    assert_eq!(
        text.matches("-local").count(),
        1,
        "依赖基线的本地 op 必须恰好一次：{text:?}"
    );
    assert_eq!(
        text.matches("-r2").count(),
        1,
        "远端并发 op 必须恰好一次：{text:?}"
    );
    assert_eq!(
        text.matches("base").count(),
        1,
        "基线文本必须恰好一次：{text:?}"
    );
    assert_eq!(
        text.len(),
        "base".len() + "-local".len() + "-r2".len(),
        "不得有重复或残留字符：{text:?}"
    );
}

// ---------------------------------------------------------------------------
// 2. LoroTree create / move / delete
// ---------------------------------------------------------------------------

/// 新建一个 LoroDoc（未启用任何容器配置）。
fn tree_doc(peer: u64) -> LoroDoc {
    let doc = LoroDoc::new();
    doc.set_peer_id(peer).expect("设置 peer id");
    doc
}

/// 取树容器并启用 fractional index（与 `StructureDoc::create/restore` 一致）。
fn editable_tree(doc: &LoroDoc) -> LoroTree {
    let tree = doc.get_tree("tree");
    tree.enable_fractional_index(0);
    tree
}

fn set_title(tree: &LoroTree, node: TreeID, title: &str) {
    tree.get_meta(node)
        .expect("节点 meta 可写")
        .insert("title", title)
        .expect("写标题");
}

fn title_of(tree: &LoroTree, node: TreeID) -> String {
    let meta = tree.get_meta(node).expect("节点 meta 可读");
    match meta.get_deep_value() {
        LoroValue::Map(entries) => entries
            .get("title")
            .and_then(|value| match value {
                LoroValue::String(value) => Some(value.to_string()),
                _ => None,
            })
            .unwrap_or_default(),
        _ => String::new(),
    }
}

/// 树的可读形状：深度优先的「标题（含缩进）」，根顺序也参与比较。
fn shape(doc: &LoroDoc) -> Vec<String> {
    let tree = doc.get_tree("tree");
    let mut out = Vec::new();
    walk_shape(&tree, None, 0, &mut out);
    out
}

fn walk_shape(tree: &LoroTree, parent: Option<TreeID>, depth: usize, out: &mut Vec<String>) {
    let Some(children) = tree.children(parent) else {
        return;
    };
    for child in children {
        out.push(format!("{}{}", "  ".repeat(depth), title_of(tree, child)));
        walk_shape(tree, Some(child), depth + 1, out);
    }
}

/// 节点 ID + 父节点 ID（连同深度），用于断言两副本是同一棵树而不是「同形状」。
fn parent_ids(doc: &LoroDoc) -> Vec<String> {
    let tree = doc.get_tree("tree");
    let mut out = Vec::new();
    walk_ids(&tree, None, 0, &mut out);
    out
}

fn walk_ids(tree: &LoroTree, parent: Option<TreeID>, depth: usize, out: &mut Vec<String>) {
    let Some(children) = tree.children(parent) else {
        return;
    };
    for child in children {
        let parent_label = parent
            .map(|parent| parent.to_string())
            .unwrap_or_else(|| "<root>".to_owned());
        out.push(format!("{parent_label}|{child}|{depth}"));
        walk_ids(tree, Some(child), depth + 1, out);
    }
}

/// 在树上跑一遍 detached / attached 三条路径，返回 attached 参考文档。
///
/// - `snapshot`：本轮要应用的远端基线快照（peer B 导出）；
/// - `local_delta`：本地相对该快照 VV 的差集；
/// - `local_vv`：本地副本的 oplog VV（detached 导出的基准）；
/// - `local_snapshot`：本地副本完整快照（attached 重放的起点）。
fn assert_tree_paths_agree(
    peer: u64,
    snapshot: &[u8],
    local_delta: &[u8],
    local_vv: &VersionVector,
    local_snapshot: &[u8],
) -> LoroDoc {
    // 1) attached 参考：基线副本 attached 导入同一条本地差集。
    let attached = tree_doc(peer);
    attached.import(snapshot).expect("attached 基线快照");
    attached.import(local_delta).expect("attached 合并本地差集");

    // 2) detached：先 detach，只把增量写进 oplog，再从 oplog 导出差集。
    let detached = tree_doc(peer);
    detached.import(snapshot).expect("detached 基线快照");
    editable_tree(&detached);
    detached.detach();
    assert!(detached.is_detached());
    detached.import(local_delta).expect("detached import");
    assert!(
        detached.is_detached(),
        "import 不得把 detached 文档翻回 attached"
    );
    assert_eq!(
        detached.oplog_frontiers().encode(),
        attached.state_frontiers().encode(),
        "detached 合并 frontier 与 attached 合并 frontier 不一致"
    );
    assert_eq!(
        detached.oplog_frontiers().encode(),
        attached.oplog_frontiers().encode(),
        "detached oplog frontier 与 attached oplog frontier 不一致"
    );
    assert_eq!(
        canonical_vv(&detached.oplog_vv().encode()),
        canonical_vv(&attached.oplog_vv().encode()),
        "detached 合并 VV 与 attached 合并 VV 不一致"
    );
    let delta_from_oplog = detached
        .export(ExportMode::updates(local_vv))
        .expect("从 detached oplog 导出差集");
    assert!(!delta_from_oplog.is_empty(), "本轮 detached 差集必须非空");

    // attach 后读取合并后的树状态（生产从不读 detached 状态，这里只做等价性证据）。
    detached.attach();
    assert!(!detached.is_detached());
    assert_eq!(shape(&detached), shape(&attached), "attach 后树形状不一致");
    assert_eq!(
        parent_ids(&detached),
        parent_ids(&attached),
        "attach 后树父子关系不一致"
    );

    // 3) attached 重放：本地副本 + detached 导出的差集（= 库里本地 update + 合并差集）。
    let replay = tree_doc(peer);
    replay.import(local_snapshot).expect("重放本地副本");
    editable_tree(&replay);
    replay
        .import(&delta_from_oplog)
        .expect("重放 detached 差集");
    assert_eq!(
        replay.state_frontiers().encode(),
        attached.state_frontiers().encode(),
        "attached 重放 frontier 与 attached 合并不一致"
    );
    assert_eq!(
        canonical_vv(&replay.oplog_vv().encode()),
        canonical_vv(&attached.oplog_vv().encode()),
        "attached 重放 VV 与 attached 合并不一致"
    );
    assert_eq!(
        shape(&replay),
        shape(&attached),
        "attached 重放树形状不一致"
    );
    assert_eq!(
        parent_ids(&replay),
        parent_ids(&attached),
        "attached 重放父子关系不一致"
    );

    attached
}

#[test]
fn tree_create_and_move_match_attached_frontier_and_shape() {
    // 第一份基线（peer B）：Folder / Note 1、根级 Note 2。
    let remote = tree_doc(B_PEER);
    let remote_tree = editable_tree(&remote);
    let folder = remote_tree.create(None).expect("创建 Folder");
    let note1 = remote_tree.create(folder).expect("创建 Note 1");
    let note2 = remote_tree.create(None).expect("创建 Note 2");
    set_title(&remote_tree, folder, "Folder");
    set_title(&remote_tree, note1, "Note 1");
    set_title(&remote_tree, note2, "Note 2");
    remote.commit();
    let first = remote.export(ExportMode::Snapshot).expect("第一份基线");

    // 本地（peer A）：在基线上并发 create + move（把 Note 2 重挂到本地新节点下）。
    let local = tree_doc(A_PEER);
    local.import(&first).expect("本地副本");
    let local_tree = editable_tree(&local);
    let local_node = local_tree.create(note1).expect("本地新节点");
    set_title(&local_tree, local_node, "Local");
    local_tree.mov(note2, local_node).expect("本地移动");
    local.commit();
    let local_snapshot = local.export(ExportMode::Snapshot).expect("本地快照");
    let local_vv = local.oplog_vv();

    // 远端在本地编辑期间继续前进：新建根节点 Remote（第二轮 frontier 的来源）。
    let remote_node = remote_tree.create(None).expect("远端新根");
    set_title(&remote_tree, remote_node, "Remote");
    remote.commit();
    let snapshot = remote.export(ExportMode::Snapshot).expect("第二份基线");
    let baseline_vv = remote.oplog_vv();

    let local_delta = local
        .export(ExportMode::updates(&baseline_vv))
        .expect("本地差集");
    assert!(!local_delta.is_empty(), "本地差集不能为空");

    let attached =
        assert_tree_paths_agree(A_PEER, &snapshot, &local_delta, &local_vv, &local_snapshot);

    // 可见形状硬断言：Folder > Note 1 > Local > Note 2，远端新根 Remote 在根级末尾。
    assert_eq!(
        shape(&attached),
        vec![
            "Folder".to_owned(),
            "  Note 1".to_owned(),
            "    Local".to_owned(),
            "      Note 2".to_owned(),
            "Remote".to_owned(),
        ]
    );
}

#[test]
fn tree_delete_and_reparent_match_attached_frontier_and_shape() {
    // 第一份基线（peer B）：Folder / Note 1（本地会删），根级 Note 2、Note 3（本地会重挂）。
    let remote = tree_doc(B_PEER);
    let remote_tree = editable_tree(&remote);
    let folder = remote_tree.create(None).expect("创建 Folder");
    let note1 = remote_tree.create(folder).expect("创建 Note 1");
    let note2 = remote_tree.create(None).expect("创建 Note 2");
    let note3 = remote_tree.create(None).expect("创建 Note 3");
    set_title(&remote_tree, folder, "Folder");
    set_title(&remote_tree, note1, "Note 1");
    set_title(&remote_tree, note2, "Note 2");
    set_title(&remote_tree, note3, "Note 3");
    remote.commit();
    let first = remote.export(ExportMode::Snapshot).expect("第一份基线");

    // 本地（peer A）：并发 delete + create + reparent。
    let local = tree_doc(A_PEER);
    local.import(&first).expect("本地副本");
    let local_tree = editable_tree(&local);
    local_tree.delete(note1).expect("本地删除 Note 1");
    assert!(
        local_tree.is_node_deleted(&note1).expect("删除标记可读"),
        "本地删除后 Note 1 必须是删除态"
    );
    let local_node = local_tree.create(note2).expect("本地新节点");
    set_title(&local_tree, local_node, "Local");
    local_tree.mov(note3, local_node).expect("本地重挂 Note 3");
    local.commit();
    let local_snapshot = local.export(ExportMode::Snapshot).expect("本地快照");
    let local_vv = local.oplog_vv();

    // 远端继续：新建根节点 Remote。
    let remote_node = remote_tree.create(None).expect("远端新根");
    set_title(&remote_tree, remote_node, "Remote");
    remote.commit();
    let snapshot = remote.export(ExportMode::Snapshot).expect("第二份基线");
    let baseline_vv = remote.oplog_vv();

    let local_delta = local
        .export(ExportMode::updates(&baseline_vv))
        .expect("本地差集");
    assert!(!local_delta.is_empty(), "本地差集不能为空");

    let attached =
        assert_tree_paths_agree(A_PEER, &snapshot, &local_delta, &local_vv, &local_snapshot);

    // 可见形状硬断言：Note 1 被删（Folder 无子），Note 2 > Local > Note 3，Remote 在根级末尾。
    assert_eq!(
        shape(&attached),
        vec![
            "Folder".to_owned(),
            "Note 2".to_owned(),
            "  Local".to_owned(),
            "    Note 3".to_owned(),
            "Remote".to_owned(),
        ]
    );
    assert!(
        attached
            .get_tree("tree")
            .is_node_deleted(&note1)
            .expect("删除标记可读"),
        "合并后 Note 1 必须仍是删除态"
    );
}

// ---------------------------------------------------------------------------
// 3. 本地在 baseline 导出之后继续编辑
// ---------------------------------------------------------------------------

#[test]
fn local_edits_after_baseline_export_survive_exactly_once() {
    let (_dir, vault) = fresh_vault();
    let remote = memory_doc("doc-1", B_PEER, "Hello world");
    let first = remote.export_snapshot().expect("第一份基线");

    // 本地在 S0 之后继续写（首尾各一笔），远端也在 S0 之后写（中间一笔）。
    let _ = seed_note(&vault, "note-1", "doc-1");
    let local = BodyDoc::restore("doc-1", A_PEER, Some(&first), &[]).expect("本地副本");
    local.insert_scalars(0, ">> ").expect("本地行首编辑");
    local
        .insert_scalars(local.scalar_len(), "!")
        .expect("本地行尾编辑");
    commit_now(&vault, &local, "local-batch");

    remote.insert_scalars(5, " MID").expect("远端第二笔");
    let full = remote.export_snapshot().expect("第二份基线");

    let parts = merge_detached(&vault, "doc-1", &full, &local, 6, "cursor-2");
    let text = assert_detached_matches_attached(&vault, "doc-1", &full, &parts);

    assert_eq!(
        text, ">> Hello MID world!",
        "并发的本地首尾编辑必须保留且只保留一次：{text:?}"
    );
    assert_eq!(text.matches(">> ").count(), 1);
    assert_eq!(text.matches(" MID").count(), 1);
    assert_eq!(text.matches('!').count(), 1);
    assert_eq!(text.matches("Hello").count(), 1);
    assert_eq!(text.len(), 19);
}

#[test]
fn second_detached_merge_keeps_both_local_writes_once() {
    let (_dir, vault) = fresh_vault();
    let local = seed_note(&vault, "note-1", "doc-1");
    local.insert_scalars(0, "-L1").expect("本地第一笔");
    commit_now(&vault, &local, "local-round-1");

    let remote = memory_doc("doc-1", B_PEER, "BASE");
    let first = remote.export_snapshot().expect("第一份基线");

    let parts = merge_detached(&vault, "doc-1", &first, &local, 5, "cursor-1");
    let first_text = assert_detached_matches_attached(&vault, "doc-1", &first, &parts);
    assert_eq!(first_text.matches("-L1").count(), 1);
    assert_eq!(first_text.matches("BASE").count(), 1);

    // 第一轮 finish；完成后再 begin 第二轮（第二次 checkpoint 追赶）。
    let finish = finish_baseline_apply(&vault, 5, "cursor-1").expect("finish 第一轮");
    assert!(
        matches!(finish, BaselineApplyFinish::Applied(_)),
        "第一轮 finish 应成功：{finish:?}"
    );

    // 合并态上继续本地写：生产路径就是重放合并结果再编辑。
    let (merged, _head) = load_body_doc(&vault, "doc-1", A_PEER, None).expect("第一轮合并态");
    merged
        .insert_scalars(merged.scalar_len(), "-L2")
        .expect("本地第二笔");
    commit_now(&vault, &merged, "local-round-2");

    // 远端也继续前进：第二轮 detached 差集必须非空（否则测不到 exported 差集）。
    remote.insert_scalars(0, "R2-").expect("远端第二笔");
    let second = remote.export_snapshot().expect("第二份基线");

    let parts = merge_detached(&vault, "doc-1", &second, &merged, 9, "cursor-2");
    let text = assert_detached_matches_attached(&vault, "doc-1", &second, &parts);

    assert_eq!(
        text.matches("-L1").count(),
        1,
        "第一笔本地编辑必须恰好一次：{text:?}"
    );
    assert_eq!(
        text.matches("-L2").count(),
        1,
        "第二笔本地编辑必须恰好一次：{text:?}"
    );
    assert_eq!(text.matches("BASE").count(), 1);
    assert_eq!(text.matches("R2-").count(), 1);
    assert!(text.ends_with("-L2"), "本地追加必须仍在末尾：{text:?}");
    assert_eq!(
        text.len(),
        "BASE".len() + "R2-".len() + "-L1".len() + "-L2".len(),
        "不得有重复或残留字符：{text:?}"
    );
}

// ---------------------------------------------------------------------------
// 4. 边界：空文档 / 单字符 / emoji 与组合字符
// ---------------------------------------------------------------------------

#[test]
fn empty_baseline_text_merges_to_local_text_only() {
    let (_dir, vault) = fresh_vault();
    // 基线：插入再删除——正文为空，但 oplog 非空，走的是 detached 合并而不是「新文档」分支。
    let remote = BodyDoc::create("doc-1", B_PEER).expect("远端基线");
    remote.insert_scalars(0, "x").expect("基线插入");
    remote.delete_scalars(0, 1).expect("基线删除");
    assert_eq!(remote.text(), "");
    let snapshot = remote.export_snapshot().expect("基线快照");

    let local = seed_note(&vault, "note-1", "doc-1");
    local.insert_scalars(0, "local").expect("本地编辑");
    commit_now(&vault, &local, "local-batch");

    let parts = merge_detached(&vault, "doc-1", &snapshot, &local, 7, "cursor-3");
    let text = assert_detached_matches_attached(&vault, "doc-1", &snapshot, &parts);

    assert_eq!(text, "local", "空基线不得留下任何可见字符：{text:?}");
    assert_eq!(text.len(), 5);
}

#[test]
fn local_doc_emptied_by_delete_merges_to_baseline_text() {
    let (_dir, vault) = fresh_vault();
    let remote = memory_doc("doc-1", B_PEER, "baseline");
    let snapshot = remote.export_snapshot().expect("基线快照");

    // 本地写过再全部删掉：正文为空但 oplog 非空。
    let local = seed_note(&vault, "note-1", "doc-1");
    local.insert_scalars(0, "tmp").expect("本地临时文本");
    local.delete_scalars(0, 3).expect("本地清空");
    assert_eq!(local.text(), "");
    commit_now(&vault, &local, "local-batch");

    let parts = merge_detached(&vault, "doc-1", &snapshot, &local, 8, "cursor-4");
    let text = assert_detached_matches_attached(&vault, "doc-1", &snapshot, &parts);

    assert_eq!(text, "baseline", "本地清空不得删掉基线字符：{text:?}");
    assert_eq!(text.len(), 8);
}

#[test]
fn single_char_docs_keep_both_chars_after_detached_merge() {
    let (_dir, vault) = fresh_vault();
    let remote = memory_doc("doc-1", B_PEER, "a");
    let snapshot = remote.export_snapshot().expect("基线快照");

    let local = seed_note(&vault, "note-1", "doc-1");
    local.insert_scalars(0, "b").expect("本地单字符");
    commit_now(&vault, &local, "local-batch");

    let parts = merge_detached(&vault, "doc-1", &snapshot, &local, 9, "cursor-5");
    let text = assert_detached_matches_attached(&vault, "doc-1", &snapshot, &parts);

    assert_eq!(text.len(), 2, "两个单字符必须都在：{text:?}");
    assert_eq!(text.chars().count(), 2);
    assert_eq!(text.matches('a').count(), 1, "基线字符丢失/重复：{text:?}");
    assert_eq!(text.matches('b').count(), 1, "本地字符丢失/重复：{text:?}");
}

#[test]
fn emoji_and_combining_mark_preserved_byte_exact() {
    let (_dir, vault) = fresh_vault();
    let remote = memory_doc("doc-1", B_PEER, "😀");
    let snapshot = remote.export_snapshot().expect("基线快照");

    let local = seed_note(&vault, "note-1", "doc-1");
    local
        .insert_scalars(0, "e\u{301}")
        .expect("本地组合字符（e + U+0301）");
    commit_now(&vault, &local, "local-batch");

    let parts = merge_detached(&vault, "doc-1", &snapshot, &local, 10, "cursor-6");
    let text = assert_detached_matches_attached(&vault, "doc-1", &snapshot, &parts);

    assert_eq!(
        text.chars().count(),
        3,
        "😀 + e + ◌́ = 3 个 scalar：{text:?}"
    );
    assert_eq!(
        text.len(),
        4 + 3,
        "UTF-8 字节数必须原样保留（😀 4B + e 1B + U+0301 2B）：{text:?}"
    );
    assert_eq!(text.matches("😀").count(), 1, "emoji 丢失/重复：{text:?}");
    assert_eq!(
        text.matches("e\u{301}").count(),
        1,
        "组合字符不得被拆开：{text:?}"
    );
    assert!(
        !text.contains('\u{fffd}'),
        "不得出现替换字符（说明按字节截断过）：{text:?}"
    );
}

#[test]
fn precomposed_vs_decomposed_accent_merges_without_normalization() {
    let (_dir, vault) = fresh_vault();
    // 远端用预组合 é（U+00E9，2B），本地用分解 e + U+0301（3B）：同形不同字节。
    let remote = memory_doc("doc-1", B_PEER, "é");
    let snapshot = remote.export_snapshot().expect("基线快照");

    let local = seed_note(&vault, "note-1", "doc-1");
    local.insert_scalars(0, "e\u{301}").expect("本地分解形式");
    commit_now(&vault, &local, "local-batch");

    let parts = merge_detached(&vault, "doc-1", &snapshot, &local, 11, "cursor-7");
    let text = assert_detached_matches_attached(&vault, "doc-1", &snapshot, &parts);

    assert_eq!(text.len(), 2 + 3, "两种编码的字节都要保留：{text:?}");
    assert_eq!(text.chars().count(), 3);
    assert_eq!(text.matches('é').count(), 1, "预组合形式丢失：{text:?}");
    assert_eq!(
        text.matches("e\u{301}").count(),
        1,
        "分解形式丢失：{text:?}"
    );
    assert_ne!(text, "éé", "不得做 NFC 归一化：{text:?}");
    assert_ne!(text, "e\u{301}e\u{301}", "不得做 NFD 归一化：{text:?}");
}

`````


## `crates/sync/tests/phase1a_release.rs`

`````rust
//! Phase 1A release 探针：`baseline_apply` 持闸 merge 成本（手动运行，不 assert 时间）。
//!
//! 只测 **storage 的 baseline merge**：在已有库上准备 20 篇带本地未上传 delta 的正文，
//! 构造 5 MiB / 20 MiB 两档 Loro snapshot（20 篇合计明文），走
//! `begin_baseline_apply → apply_baseline_batch × N → finish_baseline_apply`，
//! 记录总墙钟 / 各段 / `gate_hold_ms`（begin + 各批 apply + finish 的墙钟和，
//! 对应 Phase 1A 计划里"持写闸 merge"的时间口径），以及 storage 内置的分段计时
//! 与计数（restore / head 查询 / 本地重放 / 本地差集 / 二次 snapshot import /
//! detached oplog import / pending_update / SQLite 事务），用于区分
//! 「Loro 单大文本 op 的超线性」与「重复解码/二次导入」两类成本。
//!
//! 不涉及 relay / checkpoint 签名 / 真实下载：`S` 与 cursor 是自造的合法值；
//! 每次都用 fresh vault，结束后 journal 一律 `completed`，不会给下一次留 staging。
//!
//! 第二个探针 [`checkpoint_validate_scan_cost`] 测 **checkpoint validate 的 coverage
//! 日志扫描 O(S) 成本**：FakeRelay 构造 S = 1k / 5k / 25k 条混合日志（85.7% UPDATE +
//! 14.3% ASSET），走真实 `CheckpointWriter::prepare → validate`（不复制 scan 实现），
//! 只打印页数/墙钟，不 assert 时间。
//!
//! ```bash
//! cargo test --release -p menghunote-sync --test phase1a_release -- --ignored --nocapture
//! ```

mod support;

use std::time::Instant;

use support::{
    commit_local, commit_with_intent, fresh_vault, publish_real_asset, seed_doc, uuid_like,
    FakeRelay, TestRandom, EPOCH, VAULT_ID,
};

use menghunote_storage::{
    apply_baseline_batch, baseline_apply_debug_counters, baseline_apply_debug_timings,
    baseline_apply_state, begin_baseline_apply, create_note_slot, ensure_structure_row,
    finish_baseline_apply, load_body_doc, reset_baseline_apply_debug, sync_cursor_state,
    BaselineApplyDebugCounters, BaselineApplyDebugTimings, BaselineApplyFinish,
    BaselineApplyStatus, BaselineExpectation, BodyDoc, Value, Vault, BASELINE_MERGE_BATCH,
    META_SYNC_CONFIRMED_HIGHWATER, META_SYNC_INTEGRATED_SEQ, META_SYNC_LOG_ID,
    META_SYNC_RECEIVED_SEQ, META_SYNC_RELAY_CURSOR,
};
use menghunote_sync::checkpoint::{CheckpointWriter, EmptyAssets, PreparedCheckpoint};
use menghunote_sync::engine::RELAY_CURSOR_META;
use menghunote_sync::genesis;
use menghunote_sync::relay::{
    BootstrapBundle, BootstrapLease, CheckpointLease, CheckpointPublish, CheckpointRef,
    ObjectSubmission, PublishedCheckpoint, RelayClient, RelayLogKind, RelayLogPage, RelayState,
    SlotRecord, SlotUpload, UpdateOutcome, UpdateSubmission,
};
use menghunote_sync::{DeviceId, ObjectId, SyncError};
use menghunote_sync_proto::DeviceCertV1;

/// 基线文档数（真实信任链里 docs 数量更多；这里固定 20 便于两档总量均分）。
const DOCS: usize = 20;
/// 本地设备 peer。
const LOCAL_PEER: u64 = 0x41;
/// 远端基线写者 peer（只用于生成 snapshot；与本地 peer 不同）。
const REMOTE_PEER: u64 = 0x42;
/// 本地未上传 delta 的标记（大写 + 下划线，不会出现在远端随机字母表里）。
const LOCAL_PREFIX: &str = "LOCAL_UNSYNCED_";
/// 远端基线内容标记（同上）。
const REMOTE_PREFIX: &str = "REMOTE_BASELINE_";
/// 每批 apply 的文档数（20 篇 → 4 批；生产 `BOOTSTRAP_DOC_BATCH=64`，这里分小批
/// 是为了能看到分段耗时，总 merge 工作量相同）。
const APPLY_CHUNK: usize = 5;
/// 模拟的 checkpoint 覆盖水位（自造合法值）。
const BASELINE_S: u64 = 500;
/// finish 选定的 cursor。
const BASELINE_CURSOR: &str = "c500";
/// 本地已有水位（finish 后必须单调前进到 `BASELINE_S`）。
const SEED_RECEIVED: u64 = 100;
const SEED_INTEGRATED: u64 = 80;
const SEED_HIGHWATER: u64 = 90;
/// 本地已有 relay cursor。
const SEED_CURSOR: &str = "c80";

/// 远端正文随机字母表（只用小写字母 + 数字，标记里的大写/下划线不可能偶然出现）。
const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";

fn pseudo_text(seed: u64, chars: usize) -> String {
    let mut state = seed
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(0xD1B5_4A32_D192_ED03);
    let mut out = String::with_capacity(chars + 32);
    while out.len() < chars {
        // xorshift64：确定性、跨平台一致，避免为基准引入随机依赖。
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        let value = state.wrapping_mul(0x2545_F491_4F6C_DD1D);
        for byte in value.to_le_bytes() {
            out.push(ALPHABET[(byte as usize) % ALPHABET.len()] as char);
            if out.len() >= chars {
                break;
            }
        }
    }
    out
}

fn remote_text(index: usize, chars: usize) -> String {
    let mut text = format!("{REMOTE_PREFIX}{index:02}|");
    text.push_str(&pseudo_text(0x5eed_0000 + index as u64, chars));
    text
}

fn local_text(index: usize) -> String {
    format!("{LOCAL_PREFIX}{index:02}|{}", "L".repeat(2048))
}

fn build_snapshot(index: usize, chars: usize) -> Vec<u8> {
    let doc = BodyDoc::create(format!("bench-doc-{index:02}"), REMOTE_PEER)
        .expect("创建远端基线 BodyDoc");
    doc.insert_scalars(0, &remote_text(index, chars))
        .expect("写远端基线正文");
    doc.export_snapshot().expect("导出 baseline snapshot")
}

/// 生成 20 篇 snapshot，使总字节数尽量贴近 `target_bytes`。
///
/// 先用 16 KiB 文本量出"snapshot 字节 / 文本字符"的比例，再按比例放大；若实测
/// 总字节偏离目标 >2%，按实测比例再重建一次（Loro snapshot 有固定的块/字典开销，
/// 超出这个范围说明比例外推不准）。
fn build_snapshot_set(target_bytes: usize) -> (Vec<Vec<u8>>, usize, f64) {
    let per_doc = target_bytes / DOCS;
    let pilot_chars = 16 * 1024;
    let pilot = build_snapshot(0x3ff, pilot_chars);
    let ratio = pilot.len() as f64 / pilot_chars as f64;
    assert!(ratio > 0.0, "snapshot 比例必须为正");

    let mut chars = ((per_doc as f64) / ratio).ceil() as usize;
    let mut snapshots: Vec<Vec<u8>> = (0..DOCS)
        .map(|index| build_snapshot(index, chars))
        .collect();
    let total: usize = snapshots.iter().map(Vec::len).sum();
    if (total as f64 - target_bytes as f64).abs() / target_bytes as f64 > 0.02 {
        let scale = target_bytes as f64 / total as f64;
        chars = ((chars as f64) * scale).ceil() as usize;
        snapshots = (0..DOCS)
            .map(|index| build_snapshot(index, chars))
            .collect();
    }
    (snapshots, chars, ratio)
}

fn count(vault: &Vault, sql: &str) -> i64 {
    let rows = vault.query(sql, &[]).expect("count 查询");
    match rows.first().and_then(|row| row.first()) {
        Some(Value::Integer(value)) => *value,
        other => panic!("count 结果异常：{other:?}"),
    }
}

fn seed_watermarks(vault: &Vault) {
    vault
        .set_meta(META_SYNC_RECEIVED_SEQ, &SEED_RECEIVED.to_be_bytes())
        .expect("seed received");
    vault
        .set_meta(META_SYNC_INTEGRATED_SEQ, &SEED_INTEGRATED.to_be_bytes())
        .expect("seed integrated");
    vault
        .set_meta(META_SYNC_CONFIRMED_HIGHWATER, &SEED_HIGHWATER.to_be_bytes())
        .expect("seed highwater");
    vault
        .set_meta(META_SYNC_RELAY_CURSOR, SEED_CURSOR.as_bytes())
        .expect("seed relay cursor");
}

/// 在 fresh vault 上准备 20 篇"已有本地正文 + 本地未上传 delta"的文档。
fn prepare_vault() -> (tempfile::TempDir, Vault, Vec<String>) {
    let (dir, vault) = fresh_vault();
    let mut doc_ids = Vec::with_capacity(DOCS);
    for index in 0..DOCS {
        let doc_id = format!("bench-doc-{index:02}");
        let doc = seed_doc(&vault, &doc_id, LOCAL_PEER);
        doc.insert_scalars(0, &local_text(index))
            .expect("写本地正文");
        // 不带同步意图：这是一条只在本机、从未上传的 delta。
        commit_local(&vault, &doc);
        doc_ids.push(doc_id);
    }
    seed_watermarks(&vault);
    (dir, vault, doc_ids)
}

#[derive(Debug)]
struct ProbeStats {
    label: &'static str,
    target_bytes: usize,
    snapshot_total: usize,
    snapshot_min: usize,
    snapshot_max: usize,
    chars_per_doc: usize,
    ratio: f64,
    build_snapshot_ms: u128,
    vault_prep_ms: u128,
    begin_ms: u128,
    apply_batch_ms: Vec<u128>,
    apply_total_ms: u128,
    finish_ms: u128,
    gate_hold_ms: u128,
    verify_ms: u128,
    total_wall_ms: u128,
    timings: BaselineApplyDebugTimings,
    counters: BaselineApplyDebugCounters,
}

#[allow(clippy::too_many_lines)]
fn run_probe(label: &'static str, target_bytes: usize) -> ProbeStats {
    let started = Instant::now();

    // 1) 生成快照（不持闸；真实实现里这段是下载 + 解密 + 校验）。
    let build_started = Instant::now();
    let (snapshots, chars_per_doc, ratio) = build_snapshot_set(target_bytes);
    let build_snapshot_ms = build_started.elapsed().as_millis();
    let snapshot_total: usize = snapshots.iter().map(Vec::len).sum();
    let snapshot_min = snapshots.iter().map(Vec::len).min().unwrap_or(0);
    let snapshot_max = snapshots.iter().map(Vec::len).max().unwrap_or(0);
    assert_eq!(snapshots.len(), DOCS);

    // 2) 已有库：20 篇本地正文 + 未上传 delta + 旧水位。
    let prep_started = Instant::now();
    let (_dir, vault, doc_ids) = prepare_vault();
    let vault_prep_ms = prep_started.elapsed().as_millis();
    assert_eq!(
        count(&vault, "SELECT count(*) FROM sync_outbox"),
        0,
        "本地 delta 不得有 outbox 行"
    );
    let before = sync_cursor_state(&vault).expect("seeded cursor");
    assert_eq!(before.received_seq, SEED_RECEIVED);
    assert_eq!(before.integrated_seq, SEED_INTEGRATED);
    assert_eq!(before.confirmed_highwater, Some(SEED_HIGHWATER));

    // 3) 持闸段：begin → apply 各批 → finish。
    reset_baseline_apply_debug();
    let begin_started = Instant::now();
    let expected =
        BaselineExpectation::new(doc_ids.iter().cloned(), std::iter::empty::<[u8; 32]>());
    let begin_state = begin_baseline_apply(&vault, &[0x51; 16], BASELINE_S, "c0-source", &expected)
        .expect("begin baseline");
    let begin_ms = begin_started.elapsed().as_millis();
    assert_eq!(begin_state.status, BaselineApplyStatus::Staging);
    assert_eq!(begin_state.expected_docs, Some(DOCS as u64));
    assert!(
        !begin_state.finishable,
        "begin 时预期 docs 尚未应用，finishable 必须为 false"
    );

    let entries: Vec<(String, Vec<u8>)> = doc_ids
        .iter()
        .cloned()
        .zip(snapshots.iter().cloned())
        .collect();
    let mut apply_batch_ms = Vec::new();
    let mut apply_total_ms = 0u128;
    for (batch_index, chunk) in entries.chunks(APPLY_CHUNK).enumerate() {
        let batch_started = Instant::now();
        let state = apply_baseline_batch(&vault, chunk, &[]).expect("apply baseline batch");
        let batch_ms = batch_started.elapsed().as_millis();
        apply_total_ms += batch_ms;
        apply_batch_ms.push(batch_ms);
        eprintln!(
            "[phase1a] baseline_apply label={label} batch={batch_index} docs={} apply_ms={batch_ms} applied_docs={}",
            chunk.len(),
            state.applied_docs
        );
    }

    let finish_started = Instant::now();
    let finish = finish_baseline_apply(&vault, BASELINE_S, BASELINE_CURSOR).expect("finish");
    let finish_ms = finish_started.elapsed().as_millis();
    let gate_hold_ms = begin_ms + apply_total_ms + finish_ms;
    let BaselineApplyFinish::Applied(finish_state) = finish else {
        panic!("finish 必须 Applied，实际 {finish:?}");
    };
    assert_eq!(finish_state.status, BaselineApplyStatus::Completed);
    assert_eq!(finish_state.applied_docs, DOCS as u64);
    assert!(finish_state.chosen_cursor.as_deref() == Some(BASELINE_CURSOR));
    let timings = baseline_apply_debug_timings();
    let counters = baseline_apply_debug_counters();

    // 4) 闸外校验：本地 delta 保留、正文可重放、水位单调、无残留 staging。
    let verify_started = Instant::now();
    let after = sync_cursor_state(&vault).expect("cursor after finish");
    assert!(
        after.received_seq >= before.received_seq,
        "received 不得回退"
    );
    assert!(
        after.integrated_seq >= before.integrated_seq,
        "integrated 不得回退"
    );
    assert!(
        after.confirmed_highwater.unwrap_or(0) >= before.confirmed_highwater.unwrap_or(0),
        "highwater 不得回退"
    );
    assert_eq!(after.received_seq, BASELINE_S);
    assert_eq!(after.integrated_seq, BASELINE_S);
    assert_eq!(after.confirmed_highwater, Some(BASELINE_S));
    assert_eq!(
        vault
            .get_meta(META_SYNC_RELAY_CURSOR)
            .expect("relay cursor")
            .as_deref(),
        Some(BASELINE_CURSOR.as_bytes())
    );
    for (index, doc_id) in doc_ids.iter().enumerate() {
        let (doc, head) = load_body_doc(&vault, doc_id, LOCAL_PEER, None).expect("merged doc");
        let text = doc.text();
        assert!(
            text.contains(&format!("{LOCAL_PREFIX}{index:02}")),
            "第 {index} 篇本地未上传 delta 丢失"
        );
        assert!(
            text.contains(&format!("{REMOTE_PREFIX}{index:02}")),
            "第 {index} 篇基线正文未导入"
        );
        assert!(
            head.head_seq >= 2,
            "第 {index} 篇应至少有本地 + baseline 两条 update"
        );
    }
    assert_eq!(
        count(
            &vault,
            "SELECT count(*) FROM loro_updates WHERE batch_id='local'",
        ),
        DOCS as i64,
        "本地 delta 的 update 行必须保留"
    );
    assert_eq!(
        count(
            &vault,
            &format!("SELECT count(*) FROM loro_updates WHERE batch_id='{BASELINE_MERGE_BATCH}'"),
        ),
        DOCS as i64,
        "每篇应恰好追加一条 baseline_merge 组合 update"
    );
    assert_eq!(count(&vault, "SELECT count(*) FROM sync_outbox"), 0);
    let journal = baseline_apply_state(&vault)
        .expect("state")
        .expect("journal");
    assert_eq!(journal.status, BaselineApplyStatus::Completed);
    assert!(!journal.finishable);
    let verify_ms = verify_started.elapsed().as_millis();

    let total_wall_ms = started.elapsed().as_millis();
    ProbeStats {
        label,
        target_bytes,
        snapshot_total,
        snapshot_min,
        snapshot_max,
        chars_per_doc,
        ratio,
        build_snapshot_ms,
        vault_prep_ms,
        begin_ms,
        apply_batch_ms,
        apply_total_ms,
        finish_ms,
        gate_hold_ms,
        verify_ms,
        total_wall_ms,
        timings,
        counters,
    }
}

fn print_probe(stats: &ProbeStats) {
    let mib = |bytes: usize| bytes as f64 / (1024.0 * 1024.0);
    eprintln!(
        "[phase1a] baseline_apply label={} target_mib={:.2} snapshot_total={} snapshot_mib={:.2} \
         per_doc_min={} per_doc_max={} chars_per_doc={} snapshot_ratio={:.4} docs={DOCS}",
        stats.label,
        mib(stats.target_bytes),
        stats.snapshot_total,
        mib(stats.snapshot_total),
        stats.snapshot_min,
        stats.snapshot_max,
        stats.chars_per_doc,
        stats.ratio,
    );
    eprintln!(
        "[phase1a] baseline_apply label={} build_snapshot_ms={} vault_prep_ms={} begin_ms={} \
         apply_batches_ms={:?} apply_total_ms={} finish_ms={} gate_hold_ms={} verify_ms={} \
         total_wall_ms={}",
        stats.label,
        stats.build_snapshot_ms,
        stats.vault_prep_ms,
        stats.begin_ms,
        stats.apply_batch_ms,
        stats.apply_total_ms,
        stats.finish_ms,
        stats.gate_hold_ms,
        stats.verify_ms,
        stats.total_wall_ms,
    );
    let t = &stats.timings;
    let ns_to_ms = |ns: u64| ns as f64 / 1e6;
    eprintln!(
        "[phase1a] baseline_apply label={} phases_ms restore={:.1} head_lookup={:.1} \
         load_body_doc={:.1} local_delta_export={:.1} local_delta_import={:.1} \
         snapshot_reimport={:.1} new_doc_import={:.1} pending_update={:.1} \
         transaction={:.1} batch_total_sum={:.1}",
        stats.label,
        ns_to_ms(t.restore_ns),
        ns_to_ms(t.head_lookup_ns),
        ns_to_ms(t.load_body_doc_ns),
        ns_to_ms(t.local_delta_export_ns),
        ns_to_ms(t.local_delta_import_ns),
        ns_to_ms(t.snapshot_reimport_ns),
        ns_to_ms(t.new_doc_import_ns),
        ns_to_ms(t.pending_update_ns),
        ns_to_ms(t.transaction_ns),
        ns_to_ms(t.batch_total_ns),
    );
    let c = &stats.counters;
    eprintln!(
        "[phase1a] baseline_apply label={} counters batches={} docs={} new={} \
         existing_no_extra={} existing_local_extra={} replayed={} snapshot_imports={} \
         snapshot_mib={:.2} load_body_doc_calls={} snapshot_reimport_mib={:.2} \
         new_doc_import_mib={:.2} pending_update_calls={} pending_update_mib={:.2} \
         local_delta_kib={:.2}",
        stats.label,
        c.batches,
        c.docs,
        c.new_docs,
        c.existing_no_extra,
        c.existing_local_extra,
        c.replayed_same_digest,
        c.snapshot_imports,
        mib(c.snapshot_bytes as usize),
        c.load_body_doc_calls,
        mib(c.snapshot_reimport_bytes as usize),
        mib(c.new_doc_import_bytes as usize),
        c.pending_update_calls,
        mib(c.pending_update_bytes as usize),
        c.local_delta_bytes as f64 / 1024.0,
    );
}

#[test]
#[ignore = "manual benchmark"]
fn phase1a_baseline_apply_gate_hold() {
    for (label, bytes) in [
        ("5MiB", 5usize * 1024 * 1024),
        ("20MiB", 20usize * 1024 * 1024),
    ] {
        let stats = run_probe(label, bytes);
        print_probe(&stats);
    }
}

// ============================================================ checkpoint validate 覆盖日志扫描成本
//
// 任务 4 P1：只测真实 `CheckpointWriter::validate` 里 coverage 日志扫描的 O(S) 成本，
// 不改生产 `scan_coverage_log`，也不复制它的实现。S 条日志是合成的（FakeRelay）：
// `1..=S-1` 落在本地 integrated 连续前缀（生产代码信任的"已消费"不变量），尾部第 S 条
// 是真实 ASSET（prepare 落 pending receipt，validate 必须下载实证）；前缀里按 1/7
// 混入 ASSET 事件（≈14.3%，任务要求的 10–20%）。
//
// 口径：
// - `pages` = validate 期间真实 `RelayClient::log` 调用次数（应从 c0 扫到 S，S/256 页）。
// - `scan_ms` = 第一次 log 开始到最后一次 log 返回的墙钟跨度（含页间逐事件核对；
//   不含最后一页返回之后的 256 条 visitor，误差上界 = 一页）。
// - `validate_ms` = 整个 validate 墙钟（含 manifest/快照读回、尾部 ASSET 实证）。
// 只打印不 assert 时间；硬断言只校验覆盖到了 S、尾部 ASSET 被真实验证、页数符合 S/256。

/// 探针档位（事件总数 S）。
const SCAN_COST_S: [usize; 3] = [1_000, 5_000, 25_000];
/// 每档重复次数（取中位数）。
const SCAN_COST_RUNS: usize = 3;
/// 生产 `COVERAGE_SCAN_PAGE_LIMIT`（`crates/sync/src/checkpoint.rs` 私有常量 = 256）。
/// 只用于校验探针确实按 S/256 页扫完整段；若生产改页大小，这里应同步更新。
const SCAN_COST_PAGE_LIMIT: usize = 256;
/// ASSET 事件间隔（每 7 条一条 → 14.3%）。
const SCAN_COST_ASSET_EVERY: usize = 7;
/// 探针正文文档 id（与 coverage 无关，只给 prepare 提供可导出的本地快照）。
const SCAN_COST_NOTE_ID: &str = "scan-cost-note";

/// 计数/计时包装：所有方法委托给 FakeRelay；只额外统计 `log()`（真实 coverage scan
/// 发起的页请求）与 `part()`（manifest/snapshot/ASSET 读回）的调用次数与墙钟。
///
/// 这是**探针侧 instrumentation**，不是 scan 实现的拷贝：validate 仍走生产
/// `CheckpointWriter::validate` → `scan_coverage_log`，wrapper 只是它调用的客户端。
struct CountingRelay {
    inner: FakeRelay,
    log_calls: u64,
    log_busy_ns: u128,
    scan_first: Option<Instant>,
    scan_last: Option<Instant>,
    part_calls: u64,
    part_busy_ns: u128,
}

impl CountingRelay {
    fn new(inner: FakeRelay) -> Self {
        Self {
            inner,
            log_calls: 0,
            log_busy_ns: 0,
            scan_first: None,
            scan_last: None,
            part_calls: 0,
            part_busy_ns: 0,
        }
    }

    fn reset_counters(&mut self) {
        self.log_calls = 0;
        self.log_busy_ns = 0;
        self.scan_first = None;
        self.scan_last = None;
        self.part_calls = 0;
        self.part_busy_ns = 0;
    }

    /// 第一次到最后一次 `log()` 返回的墙钟跨度（页扫描口径）。
    fn scan_span_ms(&self) -> f64 {
        match (self.scan_first, self.scan_last) {
            (Some(first), Some(last)) => last.duration_since(first).as_secs_f64() * 1e3,
            _ => 0.0,
        }
    }

    /// 仅 `log()` 内部（FakeRelay 取页/克隆事件）的墙钟合计。
    fn log_busy_ms(&self) -> f64 {
        self.log_busy_ns as f64 / 1e6
    }

    /// `part()` 读回墙钟合计（manifest/snapshot/ASSET）。
    fn part_busy_ms(&self) -> f64 {
        self.part_busy_ns as f64 / 1e6
    }
}

impl RelayClient for CountingRelay {
    fn state(&mut self) -> Result<RelayState, SyncError> {
        self.inner.state()
    }

    fn log(&mut self, cursor: Option<&str>, limit: u32) -> Result<RelayLogPage, SyncError> {
        if self.scan_first.is_none() {
            self.scan_first = Some(Instant::now());
        }
        let started = Instant::now();
        let result = self.inner.log(cursor, limit);
        self.log_busy_ns += started.elapsed().as_nanos();
        self.log_calls += 1;
        self.scan_last = Some(Instant::now());
        result
    }

    fn part(&mut self, object_id: &ObjectId, part_no: u32) -> Result<Vec<u8>, SyncError> {
        let started = Instant::now();
        let result = self.inner.part(object_id, part_no);
        self.part_busy_ns += started.elapsed().as_nanos();
        self.part_calls += 1;
        result
    }

    fn put_part(
        &mut self,
        object_id: &ObjectId,
        part_no: u32,
        envelope: &[u8],
    ) -> Result<(), SyncError> {
        self.inner.put_part(object_id, part_no, envelope)
    }

    fn submit_updates(
        &mut self,
        items: Vec<UpdateSubmission>,
    ) -> Result<Vec<UpdateOutcome>, SyncError> {
        self.inner.submit_updates(items)
    }

    fn submit_object(&mut self, item: ObjectSubmission) -> Result<UpdateOutcome, SyncError> {
        self.inner.submit_object(item)
    }

    fn ack(&mut self, received_seq: u64, integrated_seq: u64, epoch: u64) -> Result<(), SyncError> {
        self.inner.ack(received_seq, integrated_seq, epoch)
    }

    fn device_cert(&mut self, device_id: &DeviceId) -> Result<Option<DeviceCertV1>, SyncError> {
        self.inner.device_cert(device_id)
    }

    fn put_key_slot(&mut self, upload: SlotUpload) -> Result<SlotRecord, SyncError> {
        self.inner.put_key_slot(upload)
    }

    fn list_key_slots(&mut self) -> Result<Vec<SlotRecord>, SyncError> {
        self.inner.list_key_slots()
    }

    fn activate_password_slot(
        &mut self,
        expected_slot_set_rev: u64,
        slot_id: ObjectId,
        signed_message: Vec<u8>,
        signature: [u8; 64],
    ) -> Result<u64, SyncError> {
        self.inner
            .activate_password_slot(expected_slot_set_rev, slot_id, signed_message, signature)
    }

    fn create_checkpoint(
        &mut self,
        estimated_bytes: u64,
        epoch: Option<u64>,
    ) -> Result<CheckpointLease, SyncError> {
        self.inner.create_checkpoint(estimated_bytes, epoch)
    }

    fn add_checkpoint_refs(
        &mut self,
        checkpoint_id: [u8; 16],
        refs: Vec<CheckpointRef>,
    ) -> Result<u64, SyncError> {
        self.inner.add_checkpoint_refs(checkpoint_id, refs)
    }

    fn publish_checkpoint(
        &mut self,
        publish: CheckpointPublish,
    ) -> Result<PublishedCheckpoint, SyncError> {
        self.inner.publish_checkpoint(publish)
    }

    fn bootstrap(&mut self) -> Result<BootstrapBundle, SyncError> {
        self.inner.bootstrap()
    }

    fn renew_bootstrap_lease(&mut self, lease_id: [u8; 16]) -> Result<BootstrapLease, SyncError> {
        self.inner.renew_bootstrap_lease(lease_id)
    }
}

/// 一次性夹具：真实 SQLite vault + 合成 S 条日志 + 已 prepare 的 checkpoint。
struct ScanCostFixture {
    _dir: tempfile::TempDir,
    vault: Vault,
    materials: genesis::GenesisMaterials,
    relay: CountingRelay,
    prepared: PreparedCheckpoint,
    asset_events: usize,
    update_events: usize,
    prepare_pages: u64,
    prepare_scan_ms: f64,
    prepare_log_ms: f64,
}

fn build_scan_cost_fixture(total_events: usize) -> ScanCostFixture {
    assert!(
        total_events > 1,
        "S 至少需要一条前缀 UPDATE 与一条尾部 ASSET"
    );
    let (dir, vault) = fresh_vault();
    ensure_structure_row(&vault).expect("structure 行");
    vault
        .set_meta(RELAY_CURSOR_META, b"c0")
        .expect("写 relay 游标");
    let materials = genesis::build_materials(
        VAULT_ID,
        [0x44; 16],
        "pw",
        1_700_000_000,
        &mut TestRandom::new(0x88_14),
    )
    .expect("genesis 材料");
    let mut relay = FakeRelay::new(&[]);
    relay
        .certs
        .insert(materials.device_id, materials.certificate().clone());
    vault
        .set_meta(META_SYNC_LOG_ID, &relay.log_id)
        .expect("写 log_id");

    // 一篇真实正文：prepare 需要可导出的本地快照/head VV（与 coverage 无关）。
    let doc = seed_doc(&vault, SCAN_COST_NOTE_ID, materials.peer_id_u64());
    create_note_slot(
        &vault,
        SCAN_COST_NOTE_ID,
        SCAN_COST_NOTE_ID,
        SCAN_COST_NOTE_ID,
        1_700_000_000,
    )
    .expect("notes 行");
    doc.insert_scalars(0, "checkpoint coverage scan cost probe")
        .expect("正文");
    commit_with_intent(&vault, &doc, &uuid_like(9_001), [0x91; 32], EPOCH);

    // 1..S-1 按 1/7 混入 ASSET；第 S 条是真 ASSET。前 S-1 条都属于本地
    // integrated 连续前缀（只借生产代码信任的"已消费"不变量，不读资产），
    // 唯一缺口是尾部 ASSET → prepare 落 pending receipt，validate 必须下载实证。
    let mut asset_events = 0usize;
    for seq in 1..total_events {
        if seq % SCAN_COST_ASSET_EVERY == 0 {
            relay.publish_event(RelayLogKind::Asset, [0x2a; 32]);
            asset_events += 1;
        } else {
            relay.publish_event(RelayLogKind::Update, [0x15; 32]);
        }
    }
    let (_, tail_seq) = publish_real_asset(
        &mut relay,
        VAULT_ID,
        materials.device_id,
        &materials.signing_key,
        &materials.root,
        b"coverage scan cost tail asset",
        "text/plain",
    );
    assert_eq!(
        tail_seq,
        u64::try_from(total_events).expect("S 溢出 u64"),
        "尾部 ASSET 必须恰为第 S 条日志"
    );
    asset_events += 1;
    let update_events = total_events - asset_events;
    assert!(
        (update_events) > 0,
        "混合日志必须至少有一条 UPDATE（实际 S={total_events}）"
    );

    let prefix = u64::try_from(total_events - 1).expect("S 溢出 u64");
    vault
        .set_meta(META_SYNC_INTEGRATED_SEQ, &prefix.to_be_bytes())
        .expect("写 integrated 前缀");
    vault
        .set_meta(META_SYNC_RECEIVED_SEQ, &prefix.to_be_bytes())
        .expect("写 received 前缀");

    // 从这里开始的页请求才算探针成本（prepare 的 pending receipt 扫描也是 O(S)）。
    let mut relay = CountingRelay::new(relay);
    let prepared = {
        let mut writer = CheckpointWriter::new(
            &vault,
            &mut relay,
            VAULT_ID,
            materials.device_id,
            materials.signing_key.clone(),
            materials.root.clone(),
            materials.peer_id_u64(),
        );
        writer.prepare(&EmptyAssets).expect("prepare")
    };
    assert_eq!(
        prepared.report.covered_seq,
        u64::try_from(total_events).expect("S 溢出 u64")
    );
    assert_eq!(
        relay.log_calls,
        u64::try_from(total_events.div_ceil(SCAN_COST_PAGE_LIMIT)).expect("页数溢出 u64"),
        "prepare 的 receipt 扫描应从 c0 扫满 S/256 页"
    );
    let prepare_pages = relay.log_calls;
    let prepare_scan_ms = relay.scan_span_ms();
    let prepare_log_ms = relay.log_busy_ms();
    relay.reset_counters();

    ScanCostFixture {
        _dir: dir,
        vault,
        materials,
        relay,
        prepared,
        asset_events,
        update_events,
        prepare_pages,
        prepare_scan_ms,
        prepare_log_ms,
    }
}

fn median_ms(values: &[f64]) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).expect("时间不能是 NaN"));
    sorted[sorted.len() / 2]
}

fn run_scan_cost(total_events: usize) {
    let mut fixture = build_scan_cost_fixture(total_events);
    let asset_pct = fixture.asset_events as f64 * 100.0 / total_events as f64;
    eprintln!(
        "[phase1a] scan_cost setup S={total_events} asset_events={} update_events={} \
         asset_pct={asset_pct:.1} prepare_pages={} prepare_scan_ms={:.3} prepare_log_ms={:.3}",
        fixture.asset_events,
        fixture.update_events,
        fixture.prepare_pages,
        fixture.prepare_scan_ms,
        fixture.prepare_log_ms,
    );

    let mut pages = 0u64;
    let mut part_calls = 0u64;
    let mut raw_scan = Vec::with_capacity(SCAN_COST_RUNS);
    let mut raw_validate = Vec::with_capacity(SCAN_COST_RUNS);
    let mut raw_log = Vec::with_capacity(SCAN_COST_RUNS);
    let mut raw_part = Vec::with_capacity(SCAN_COST_RUNS);
    for run in 0..SCAN_COST_RUNS {
        fixture.relay.reset_counters();
        let started = Instant::now();
        let baseline = {
            let mut writer = CheckpointWriter::new(
                &fixture.vault,
                &mut fixture.relay,
                VAULT_ID,
                fixture.materials.device_id,
                fixture.materials.signing_key.clone(),
                fixture.materials.root.clone(),
                fixture.materials.peer_id_u64(),
            );
            writer.validate(&fixture.prepared).expect("validate")
        };
        let validate_ms = started.elapsed().as_secs_f64() * 1e3;
        assert_eq!(
            baseline.covered_seq(),
            u64::try_from(total_events).expect("S 溢出 u64")
        );
        assert_eq!(
            baseline.coverage_verified_candidates(),
            1,
            "尾部 ASSET 必须被真实下载验证（receipt 路径）"
        );
        assert_eq!(baseline.coverage_durable_assets(), 0);

        let run_pages = fixture.relay.log_calls;
        let scan_ms = fixture.relay.scan_span_ms();
        let log_ms = fixture.relay.log_busy_ms();
        let run_parts = fixture.relay.part_calls;
        let part_ms = fixture.relay.part_busy_ms();
        assert_eq!(
            run_pages,
            u64::try_from(total_events.div_ceil(SCAN_COST_PAGE_LIMIT)).expect("页数溢出 u64"),
            "validate 的 coverage scan 应从 c0 扫满 S/256 页"
        );
        assert!(run_parts >= 1, "validate 至少读回 manifest/快照/尾部 ASSET");
        pages = run_pages;
        part_calls = run_parts;
        eprintln!(
            "[phase1a] scan_cost S={total_events} run={run} pages={run_pages} \
             scan_ms={scan_ms:.3} validate_ms={validate_ms:.3} log_ms={log_ms:.3} \
             part_calls={run_parts} part_ms={part_ms:.3}",
        );
        raw_scan.push(scan_ms);
        raw_validate.push(validate_ms);
        raw_log.push(log_ms);
        raw_part.push(part_ms);
    }

    let scan_median = median_ms(&raw_scan);
    let validate_median = median_ms(&raw_validate);
    let log_median = median_ms(&raw_log);
    eprintln!(
        "[phase1a] scan_cost S={total_events} pages={pages} scan_ms={scan_median:.3} \
         (median of {SCAN_COST_RUNS}; raw_scan_ms={raw_scan:?} log_ms_median={log_median:.3} \
         validate_ms_median={validate_median:.3} validate_ms_raw={raw_validate:?} \
         part_calls={part_calls} part_ms_raw={raw_part:?})"
    );
}

#[test]
#[ignore = "manual benchmark"]
fn checkpoint_validate_scan_cost() {
    for total_events in SCAN_COST_S {
        run_scan_cost(total_events);
    }
}

`````


---

# 附录 C：完整代码 diff（`5d01e9e..1bededa`）


`````diff
diff --git a/crates/storage/src/actor.rs b/crates/storage/src/actor.rs
index 6824e6c..0b4f53a 100644
--- a/crates/storage/src/actor.rs
+++ b/crates/storage/src/actor.rs
@@ -263,7 +263,11 @@ impl Vault {
         self.inner.take_reconcile_required()
     }
 
-    /// 读一条 meta（键值 BLOB）；不存在返回 `None`。
+    /// 读一条 meta（键值 BLOB）；键不存在返回 `None`。
+    ///
+    /// 判定与另两个入口共享同一个 helper（`sync_meta::meta_bytes_of`）：**只认 BLOB**，
+    /// TEXT/INTEGER/REAL 都是 [`StorageError::Corrupt`]，不会静默当成"没有这条 meta"
+    /// ——否则 `sync.repack_required` / `sync.log_id` 之类持久屏障会被绕过。
     pub fn get_meta(&self, key: &str) -> Result<Option<Vec<u8>>, StorageError> {
         self.call(|reply| Command::GetMeta {
             key: key.to_owned(),
@@ -328,6 +332,10 @@ impl Vault {
     /// 低层事务命令：多条语句在同一事务里执行，`guards` 不满足则整笔回滚。
     ///
     /// 返回每条语句的影响行数（顺序与 `statements` 一致）。
+    ///
+    /// 事务以 `BEGIN IMMEDIATE` 显式开启（与批量 inbox 入口共用
+    /// [`begin_immediate`]）：写锁在事务开始就取，并发写者按 `busy_timeout` 排队，
+    /// 而不是在第一条语句处撞上"锁升级不走 busy handler"的立即失败。
     pub fn exec_transaction(
         &self,
         statements: &[Statement],
@@ -433,7 +441,8 @@ fn handle_command(conn: &Connection, command: Command) {
             let _ = reply.send(exec_transaction(conn, &statements, &guards));
         }
         Command::GetMeta { key, reply } => {
-            let _ = reply.send(get_meta(conn, &key));
+            // 与 sync_bridge / sync_meta 同一个读取 helper（只认 BLOB，其它类型报损坏）。
+            let _ = reply.send(sync_meta::read_meta_on(conn, &key));
         }
         Command::UserVersion { reply } => {
             let _ = reply.send(migrate::user_version(conn));
@@ -447,6 +456,30 @@ fn handle_command(conn: &Connection, command: Command) {
     }
 }
 
+/// 在连接上显式开启一个 **Immediate** 写事务（`BEGIN IMMEDIATE`）。
+///
+/// 为什么不用 deferred（`conn.unchecked_transaction()` 的默认行为）：
+///
+/// - deferred 的 `BEGIN` 只是登记，第一条 SELECT 拿共享锁，等到第一条写语句才尝试把
+///   共享锁升级成写锁；而 SQLite 的锁升级**不走 busy handler**——此刻若已有别的写事务
+///   在途，不会用 `busy_timeout` 等待，而是直接返回 `SQLITE_BUSY`，让调用方在"最多等
+///   5s"的预期之外立刻失败；
+/// - `BEGIN IMMEDIATE` 在事务开始就取写锁，这一步**照常**受 `busy_timeout` 约束，与
+///   ADR 0001 §3.9「每 vault 单 writer actor」的模型一致：写事务不该和别的写事务撞在
+///   升级点上。
+///
+/// 两个写事务入口都走这里，论证只写在这一处（避免注释在两个调用点各改一半而漂移）：
+///
+/// - [`exec_inbox_batch`]：先读后写，deferred 会在锁升级处失败，且读到的旧快照还会先
+///   报出误导性的 `SyncConflict`；
+/// - [`exec_transaction`]：承载 A 事务/资产/repack/finish，调用方习惯把写语句放最前，
+///   但**没有任何断言**保证这一点（以后多一条只读语句就退化成升级路径），所以同样
+///   显式 Immediate。
+fn begin_immediate(conn: &Connection) -> Result<Transaction<'_>, StorageError> {
+    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
+    Ok(tx)
+}
+
 /// 批量 inbox 的原子执行。
 ///
 /// 全部动作在 worker 持有独占连接上的**同一个事务**里：
@@ -463,8 +496,8 @@ fn handle_command(conn: &Connection, command: Command) {
 /// 先读后写的竞态因此被消除：快照与插入属于同一个写事务（SQLite 的单写者锁），
 /// 且一次提交只产生一次 fsync；崩溃/冲突只会留下「什么都没发生」或「行+前缀一起提交」。
 ///
-/// 事务用 **Immediate** 显式开启（而不是 deferred）：本事务先读后写，deferred 的锁升级
-/// 详见下方注释。
+/// 事务用 [`begin_immediate`] 显式开启（而不是 deferred）：本事务先读后写，deferred
+/// 的锁升级不走 busy handler，完整论证见该函数。
 fn exec_inbox_batch(
     conn: &Connection,
     mut rows: Vec<InboxBatchRow>,
@@ -495,16 +528,9 @@ fn exec_inbox_batch(
         });
     }
 
-    // 显式 **Immediate** 事务（不是 deferred）：
-    //
-    // 本事务里先读（校验 `sync.log_id`、比对 `[first_seq,last_seq]` 既有行）再写
-    // （插新行、推进 received 前缀）。Deferred 事务的 `BEGIN` 只是登记，第一条 SELECT
-    // 拿共享锁，等到第一条写语句才尝试把共享锁升级成写锁；而 SQLite 的锁升级**不走
-    // busy handler**——此刻若已有别的写事务在途，不会用 `busy_timeout` 等待，而是直接
-    // 返回 `SQLITE_BUSY`，让调用方在"最多等 5s"的预期之外立刻失败。`BEGIN IMMEDIATE`
-    // 在事务开始就取写锁（这一步照常受 `busy_timeout` 约束），与 ADR 0001 §3.9
-    // 「每 vault 单 writer actor」的模型一致：写事务不该和别的写事务撞在升级点上。
-    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
+    // 事务模式与锁升级论证收在 [`begin_immediate`]：本事务先读后写，deferred 会在锁
+    // 升级处立即 SQLITE_BUSY（见该函数的注释）。
+    let tx = begin_immediate(conn)?;
     sync_meta::ensure_log_id_on(&tx, &log_id)?;
 
     let first_seq = rows.first().map_or(0, |row| row.relay_seq);
@@ -613,29 +639,6 @@ fn i64_of(value: u64) -> Result<i64, StorageError> {
     })
 }
 
-/// 读一条 meta。
-fn get_meta(conn: &Connection, key: &str) -> Result<Option<Vec<u8>>, StorageError> {
-    let mut statement = conn.prepare("SELECT value FROM meta WHERE key = ?1")?;
-    let mut rows = statement.query([key])?;
-    match rows.next()? {
-        None => Ok(None),
-        Some(row) => match row.get::<usize, Value>(0)? {
-            Value::Blob(blob) => Ok(Some(blob)),
-            Value::Text(text) => Ok(Some(text.into_bytes())),
-            other => Err(StorageError::ManifestShape {
-                column: "meta.value",
-                found: match other {
-                    Value::Null => "NULL",
-                    Value::Integer(_) => "INTEGER",
-                    Value::Real(_) => "REAL",
-                    Value::Text(_) => "TEXT",
-                    Value::Blob(_) => "BLOB",
-                },
-            }),
-        },
-    }
-}
-
 /// 执行单条写语句。
 fn exec(conn: &Connection, sql: &str, params: &[Value]) -> Result<usize, StorageError> {
     let mut statement = conn.prepare(sql)?;
@@ -665,8 +668,9 @@ fn exec_transaction(
     statements: &[Statement],
     guards: &[AffectedGuard],
 ) -> Result<Vec<usize>, StorageError> {
-    // 连接由工作线程独占，不会有嵌套事务，所以用 &self 版本的 unchecked_transaction
-    let tx = conn.unchecked_transaction()?;
+    // 连接由工作线程独占，不会有嵌套事务；事务模式与批量 inbox 共用 begin_immediate
+    // （锁升级论证见该函数），语句顺序/守卫/CAS 语义都不变。
+    let tx = begin_immediate(conn)?;
 
     // 守卫必须在**该语句执行后立刻**校验：否则后续语句可能先撞 UNIQUE/FK，
     // 把「head 被人推进了」这个真正原因盖掉（CAS 语义要求先于任何写入冲突暴露）。
diff --git a/crates/storage/src/baseline_apply.rs b/crates/storage/src/baseline_apply.rs
index b44412d..8fc0656 100644
--- a/crates/storage/src/baseline_apply.rs
+++ b/crates/storage/src/baseline_apply.rs
@@ -14,11 +14,17 @@
 //!   `sync.remote_known_vv.<doc_id>`（见 [`crate::remote_known`]），跨 journal 保留，
 //!   供后续 tombstone 判定区分「远端已删的已同步文档」与「local-only 文档」。
 //! - `finish` 只做**单调**水位/cursor CAS 与 journal 状态迁移；失败不推进任何 cursor。
+//! - `begin` 记录本 checkpoint 的**预期参与集合**（覆盖到的 doc 与资产，存于既有 `meta`
+//!   单行、只本地可见）：`finish` 必须先证明集合里每一项都已 `applied=1`，才允许写任何
+//!   meta；少跑一批（或跑错集合）返回 [`BaselineApplyFinish::Conflict`]，而不是推进 S
+//!   之后让 pull 永久跳过 relay 事件。
+//! - tombstone 的 outbox 判定会在**同一写事务内**用 SQL 重判一次：判定完成后被并发写入的
+//!   outbox 行只会把 `deleted` 退化为 `kept_local_edit`，绝不误删本地编辑。
 //! - `abort` 只清 staging，不删已导入的 Loro/asset 数据，也不回滚远端已知 VV。
 
 use std::cell::RefCell;
 use std::collections::{BTreeMap, BTreeSet};
-use std::time::{Instant, SystemTime, UNIX_EPOCH};
+use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
 
 use loro::VersionVector;
 use rusqlite::types::Value;
@@ -39,6 +45,21 @@ use crate::Vault;
 pub const BASELINE_MERGE_BATCH: &str = "baseline_merge";
 /// relay 页面 cursor 的 meta 键（与 sync `engine::RELAY_CURSOR_META` 对齐）。
 pub const META_SYNC_RELAY_CURSOR: &str = "sync.relay_cursor";
+/// [`BaselineExpectation`] 的持久化 meta 键（**只本地可见**；不是 schema/wire）。
+pub const META_BASELINE_EXPECTED: &str = "sync.baseline_expected";
+/// 导入后读回校验失败的持久标记（**只本地可见**；不是 schema/wire）。
+///
+/// 为什么不是 `baseline_apply.status='failed'`：schema v3 的
+/// `CHECK(status IN('staging','completed'))` 不允许新值（见
+/// `crates/storage/src/schema/v3.sql`），加状态需要 DDL/迁移，超出同步硬化 P1 的边界。
+/// 失败语义改由本 meta 键承载：journal 保持 `staging`（已导入的 Loro/资产数据全部
+/// 保留，便于诊断/重跑），但 [`begin_baseline_apply`] 见到该键一律拒绝，必须先
+/// [`abort_baseline_apply`] 清理（abort 同时删除本键）。
+pub const META_BASELINE_VERIFY_FAILED: &str = "sync.baseline_verify_failed";
+/// 预期集合编码的魔数（防止误读旧值/损坏值）。
+const EXPECTED_MAGIC: &[u8] = b"MNBASEEXP1";
+/// 失败标记编码的魔数（`magic | utf8 reason`，防止把任意 BLOB 误读成失败标记）。
+const VERIFY_FAILED_MAGIC: &[u8] = b"MNBASEVERIFYFAIL1";
 /// storage 内部构造 BodyDoc 时使用的 peer id（只影响新文档的本地 op id，不影响 import）。
 const BASELINE_PEER_ID: u64 = 1;
 
@@ -65,6 +86,12 @@ pub struct BaselineApplyDebugTimings {
     pub transaction_ns: u64,
     /// 整个 `apply_baseline_batch`。
     pub batch_total_ns: u64,
+    /// 导入后读回校验（sync 的 `verify_imported`）耗时。
+    ///
+    /// **只读指标**：不参与任何决策；由 sync 的 bootstrap 调用
+    /// [`record_baseline_verify_time`] 记录，Phase1B 的 gate_hold 报告可在同一线程
+    /// 通过 [`baseline_apply_debug_timings`] 或 [`baseline_apply_verify_ms`] 读取。
+    pub verify_ns: u64,
 }
 
 /// 基线 apply 的计数（**仅用于基准/诊断**；按线程累计，不持久化）。
@@ -114,6 +141,26 @@ pub fn baseline_apply_debug_counters() -> BaselineApplyDebugCounters {
     BASELINE_DEBUG.with(|acc| acc.borrow().counters)
 }
 
+/// 记录一次导入后读回校验（sync 的 `verify_imported`）耗时。
+///
+/// **只读指标**：不参与任何决策，也不持久化；与其它分段计时一样按线程累计。
+/// Phase1B 的 gate_hold 报告可在同一线程读取 [`baseline_apply_debug_timings`] 的
+/// `verify_ns`（或 [`baseline_apply_verify_ms`]）。
+#[doc(hidden)]
+pub fn record_baseline_verify_time(elapsed: Duration) {
+    let elapsed = u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX);
+    BASELINE_DEBUG.with(|acc| {
+        let mut acc = acc.borrow_mut();
+        acc.timings.verify_ns = acc.timings.verify_ns.saturating_add(elapsed);
+    });
+}
+
+/// 当前线程累计的读回校验耗时（毫秒，向下取整；只读指标，不参与决策）。
+#[doc(hidden)]
+pub fn baseline_apply_verify_ms() -> u64 {
+    baseline_apply_debug_timings().verify_ns / 1_000_000
+}
+
 fn debug_add_time<F>(field: F, started: Instant)
 where
     F: FnOnce(&mut BaselineApplyDebugTimings) -> &mut u64,
@@ -165,7 +212,11 @@ pub struct BaselineApplyState {
     pub applied_docs: u64,
     pub applied_assets: u64,
     pub kept_local_edit: u64,
-    /// staging 状态且没有未完成子步骤时为 true；finish 还要求 cursor CAS 通过。
+    /// begin 记录的预期文档数；没有记录（旧 journal/损坏）时为 `None`，此时不可 finish。
+    pub expected_docs: Option<u64>,
+    /// begin 记录的预期资产数；同上。
+    pub expected_assets: Option<u64>,
+    /// staging 且 begin 记录的预期集合已全部 `applied=1` 时为 true；finish 还要求 cursor CAS 通过。
     pub finishable: bool,
     /// 所选基线自带的 relay cursor；是否真正写进 `meta` 取决于 finish 的单调判断。
     pub chosen_cursor: Option<String>,
@@ -177,10 +228,126 @@ pub enum BaselineApplyFinish {
     /// 成功推进 cursor；携带 completed 状态。
     Applied(BaselineApplyState),
     NotFound,
-    /// begin 记录的水位/游标已被并发改写、S 不符或批次未完成；未修改任何行。
+    /// begin 记录的水位/游标已被并发改写、S 不符、或预期参与集合未全部应用；
+    /// 未修改任何行。
     Conflict,
 }
 
+/// `begin_baseline_apply` 记录的**预期参与集合**：本 checkpoint 覆盖的全部文档与资产。
+///
+/// 为什么需要：journal 只记录「已应用了多少」，不记录「本来应该应用哪些」。少跑一批
+/// （进程被杀、调用方漏批、fallback 选错集）时旧实现仍能 finish 并推进 S，随后普通 pull
+/// 会把这段前缀当成已消费，**永久跳过其中所有 relay 事件**。
+///
+/// 集合以单条 [`META_BASELINE_EXPECTED`] `meta` 行持久化（不新增列/表、不参与同步、不是
+/// wire）；`new` 会排序去重，因此同一集合的不同传入顺序被视为同一份预期。
+#[derive(Debug, Clone, Default, PartialEq, Eq)]
+pub struct BaselineExpectation {
+    docs: Vec<String>,
+    assets: Vec<[u8; 32]>,
+}
+
+impl BaselineExpectation {
+    /// 规范化（排序 + 去重）后构造。
+    pub fn new(
+        docs: impl IntoIterator<Item = String>,
+        assets: impl IntoIterator<Item = [u8; 32]>,
+    ) -> Self {
+        let mut docs: Vec<String> = docs.into_iter().collect();
+        docs.sort_unstable();
+        docs.dedup();
+        let mut assets: Vec<[u8; 32]> = assets.into_iter().collect();
+        assets.sort_unstable();
+        assets.dedup();
+        Self { docs, assets }
+    }
+
+    /// 预期的文档 id（已排序去重）。
+    pub fn docs(&self) -> &[String] {
+        &self.docs
+    }
+
+    /// 预期的资产 object_id（已排序去重）。
+    pub fn assets(&self) -> &[[u8; 32]] {
+        &self.assets
+    }
+
+    /// 预期文档数。
+    pub fn doc_count(&self) -> u64 {
+        u64::try_from(self.docs.len()).unwrap_or(u64::MAX)
+    }
+
+    /// 预期资产数。
+    pub fn asset_count(&self) -> u64 {
+        u64::try_from(self.assets.len()).unwrap_or(u64::MAX)
+    }
+
+    /// 集合是否为空（空集合视为"begin 已显式声明无需应用任何项"）。
+    pub fn is_empty(&self) -> bool {
+        self.docs.is_empty() && self.assets.is_empty()
+    }
+
+    /// 编码成单条 `meta` 值：`magic | doc_count | (len,doc)* | asset_count | oid*`（全部 BE）。
+    fn encode(&self) -> Vec<u8> {
+        let mut out = Vec::with_capacity(
+            EXPECTED_MAGIC.len()
+                + 16
+                + self.docs.iter().map(|doc| 8 + doc.len()).sum::<usize>()
+                + self.assets.len() * 32,
+        );
+        out.extend_from_slice(EXPECTED_MAGIC);
+        out.extend_from_slice(&self.doc_count().to_be_bytes());
+        for doc in &self.docs {
+            let len = u64::try_from(doc.len()).unwrap_or(u64::MAX);
+            out.extend_from_slice(&len.to_be_bytes());
+            out.extend_from_slice(doc.as_bytes());
+        }
+        out.extend_from_slice(&self.asset_count().to_be_bytes());
+        for asset in &self.assets {
+            out.extend_from_slice(asset);
+        }
+        out
+    }
+
+    /// 解码并校验 `encode` 的输出；任何不合法都视为本地 meta 损坏（fail-closed）。
+    fn decode(bytes: &[u8]) -> Result<Self, StorageError> {
+        let mut offset = 0usize;
+        let magic = take_expected_bytes(bytes, &mut offset, EXPECTED_MAGIC.len())?;
+        if magic != EXPECTED_MAGIC {
+            return Err(corrupt_expected("魔数不符"));
+        }
+        let doc_count = read_expected_u64(bytes, &mut offset)?;
+        let doc_count =
+            usize::try_from(doc_count).map_err(|_| corrupt_expected("doc 数超过 usize"))?;
+        // 损坏数据不得预分配过大内存。
+        let mut docs = Vec::with_capacity(doc_count.min(64));
+        for _ in 0..doc_count {
+            let len = read_expected_u64(bytes, &mut offset)?;
+            let len = usize::try_from(len).map_err(|_| corrupt_expected("doc 长度超过 usize"))?;
+            let raw = take_expected_bytes(bytes, &mut offset, len)?;
+            let doc = std::str::from_utf8(raw)
+                .map_err(|_| corrupt_expected("doc_id 不是 UTF-8"))?
+                .to_owned();
+            docs.push(doc);
+        }
+        let asset_count = read_expected_u64(bytes, &mut offset)?;
+        let asset_count =
+            usize::try_from(asset_count).map_err(|_| corrupt_expected("资产数超过 usize"))?;
+        let mut assets = Vec::with_capacity(asset_count.min(64));
+        for _ in 0..asset_count {
+            let raw = take_expected_bytes(bytes, &mut offset, 32)?;
+            let object_id: [u8; 32] = raw
+                .try_into()
+                .map_err(|_| corrupt_expected("object_id 长度非法"))?;
+            assets.push(object_id);
+        }
+        if offset != bytes.len() {
+            return Err(corrupt_expected("尾部有多余字节"));
+        }
+        Ok(Self { docs, assets })
+    }
+}
+
 /// 一个已验证的基线资产（HMAC oid 是 relay 对象名；明文 hash 是真实 sha256）。
 #[derive(Debug, Clone, PartialEq, Eq)]
 pub struct BaselineAsset {
@@ -212,8 +379,14 @@ struct PreparedDoc {
     baseline_vv: Vec<u8>,
     expected_head: Option<HeadState>,
     update: Option<PendingUpdate>,
+    /// 读到的旧「远端已知 VV」（值 CAS 的期望值；`None` = 读时不存在）。
+    ///
+    /// 写入必须走 CAS（P2-5）：本批准备期间，普通 pull 的 A 事务完全可能已经把同一个
+    /// `sync.remote_known_vv.<doc_id>` 扩大；无条件覆盖会把它退回到本批的并集（丢失
+    /// 远端已知证据 → tombstone 误判为 local-only，只会保守保留，但证据丢了）。
+    remote_known_expected: Option<VersionVector>,
     /// 合并后的远端已知 VV（写入 `sync.remote_known_vv.<doc_id>`）。
-    remote_known: Vec<u8>,
+    remote_known_merged: VersionVector,
 }
 struct PreparedTombstone {
     doc_id: String,
@@ -227,14 +400,71 @@ struct PreparedAsset {
     statement: Option<Statement>,
 }
 
-/// 开始一次 baseline apply staging；同参数重复调用幂等，冲突参数返回 SyncConflict。
+/// 记录「baseline 导入后的读回校验失败」。
+///
+/// 只写一条本地 `meta`（BLOB：魔数 + UTF-8 原因），**不改** `baseline_apply.status`
+/// （schema v3 的 CHECK 只允许 `staging/completed`，新增 `failed` 需要 DDL/迁移）。
+/// journal 与已导入的 Loro/资产数据原样保留：CRDT merge 幂等，回滚已 merge 的数据会
+/// 丢掉本地编辑（本地 op 可能已经与基线 ops 合进同一条 update，无法原样拆分）。
+///
+/// 契约：标记存在时 [`begin_baseline_apply`] 一律拒绝；必须先 [`abort_baseline_apply`]
+/// 清理失败 journal（abort 同时删除本标记）。
+pub fn mark_baseline_verify_failed(vault: &Vault, reason: &str) -> Result<(), StorageError> {
+    let mut value = Vec::with_capacity(VERIFY_FAILED_MAGIC.len() + reason.len());
+    value.extend_from_slice(VERIFY_FAILED_MAGIC);
+    value.extend_from_slice(reason.as_bytes());
+    vault.set_meta(META_BASELINE_VERIFY_FAILED, &value)
+}
+
+/// 读失败标记的原因；没有标记返回 `None`。
+///
+/// 标记存在但编码非法（缺魔数/非 UTF-8）返回 [`StorageError::Corrupt`]，不静默当成
+/// 「没有失败」——那会让 `begin` 放行一份未知状态的 journal。
+pub fn baseline_verify_failure(vault: &Vault) -> Result<Option<String>, StorageError> {
+    let Some(bytes) = vault.get_meta(META_BASELINE_VERIFY_FAILED)? else {
+        return Ok(None);
+    };
+    let reason = bytes
+        .strip_prefix(VERIFY_FAILED_MAGIC)
+        .ok_or_else(|| StorageError::Corrupt {
+            context: format!("{META_BASELINE_VERIFY_FAILED} 编码非法（缺魔数）"),
+        })?;
+    String::from_utf8(reason.to_vec())
+        .map(Some)
+        .map_err(|_| StorageError::Corrupt {
+            context: format!("{META_BASELINE_VERIFY_FAILED} 原因不是合法 UTF-8"),
+        })
+}
+
+/// 开始一次 baseline apply staging；同参数（含预期参与集合）重复调用幂等，冲突参数返回
+/// SyncConflict。
+///
+/// `expected` 必须覆盖**本 checkpoint 的全部** docs 与资产：finish 前会拿它逐项核对
+/// `baseline_apply_docs/applied=1` 与 `baseline_apply_assets/applied=1`（见
+/// [`BaselineExpectation`]）。同一 checkpoint/S/cursor 但 `expected` 不同 → 冲突，
+/// 不会静默沿用旧集合。
+///
+/// 若存在 [`META_BASELINE_VERIFY_FAILED`] 标记（上一次导入读回校验失败），本函数**拒绝**
+/// 一切 begin（包括同参数幂等续跑），必须先 [`abort_baseline_apply`]。
 pub fn begin_baseline_apply(
     vault: &Vault,
     checkpoint_id: &[u8; 16],
     baseline_s: u64,
     source_cursor: &str,
+    expected: &BaselineExpectation,
 ) -> Result<BaselineApplyState, StorageError> {
+    // 失败标记优先于幂等/冲突判断：journal 仍是 staging（schema 不允许新 status），
+    // 但语义上已不可续跑；必须先 abort 清理失败 journal 与标记。
+    if let Some(reason) = baseline_verify_failure(vault)? {
+        return Err(StorageError::SyncConflict {
+            context: format!(
+                "baseline 导入的读回校验已失败（{META_BASELINE_VERIFY_FAILED}: {reason}）；\
+                 已 merge 的 Loro/资产数据仍保留，必须先 abort_baseline_apply 清理失败 journal"
+            ),
+        });
+    }
     let now = now_secs()?;
+    let expected_bytes = expected.encode();
     let existing = load_journal(vault)?;
     if let Some(existing) = &existing {
         if existing.status == BaselineApplyStatus::Staging
@@ -242,7 +472,16 @@ pub fn begin_baseline_apply(
             && existing.baseline_s == baseline_s
             && existing.source_cursor == source_cursor
         {
-            return state_from_journal(existing);
+            // 同参数重入必须幂等（崩溃续跑路径）；但预期集合必须对得上：
+            // 旧 journal 没有集合记录、或调用方换了集合，都不能静默继续。
+            let stored = vault.get_meta(META_BASELINE_EXPECTED)?;
+            if stored.as_deref() != Some(expected_bytes.as_slice()) {
+                return Err(StorageError::SyncConflict {
+                    context: "同一 checkpoint 的 baseline apply staging 已有另一份预期参与集合"
+                        .to_owned(),
+                });
+            }
+            return state_from_journal(vault, existing);
         }
         if existing.status == BaselineApplyStatus::Staging {
             return Err(StorageError::SyncConflict {
@@ -256,38 +495,43 @@ pub fn begin_baseline_apply(
 
     let mut statements = Vec::new();
     let mut guards = Vec::new();
-    if existing.is_some() {
-        // completed 的 journal 可以安全替换；旧 journal 子表只属于它。
-        //
-        // 但 `baseline_apply_docs.baseline_vv` 是**跨 journal 仍然有效**的
-        // 「远端已知 VV」证据：必须在删除前落到非破坏性本地 meta 里，
-        // 否则已有设备下一次快照追赶时会把远端已删文档误判成 local-only。
-        //
-        // `DO NOTHING`：已有 meta（普通 pull/integrate 记录的更新的远端已知 VV）
-        // 永远不降级；缺失时才由旧 baseline_vv 补上。
-        statements.push(Statement::new(
-            format!(
-                "INSERT INTO meta(key,value) \
-                 SELECT '{}' || doc_id, baseline_vv FROM baseline_apply_docs WHERE applied=1 \
-                 ON CONFLICT(key) DO NOTHING",
-                remote_known::META_REMOTE_KNOWN_VV_PREFIX
-            ),
-            Vec::new(),
-        ));
-        statements.push(Statement::new(
-            "DELETE FROM baseline_apply_docs",
-            Vec::new(),
-        ));
-        statements.push(Statement::new(
-            "DELETE FROM baseline_apply_tombstones",
-            Vec::new(),
-        ));
-        statements.push(Statement::new(
-            "DELETE FROM baseline_apply_assets",
-            Vec::new(),
-        ));
-        statements.push(Statement::new("DELETE FROM baseline_apply", Vec::new()));
-    }
+    // 旧 journal 子表的「远端已知 VV」证据回填 + 清理。
+    //
+    // `baseline_apply_docs.baseline_vv` 是**跨 journal 仍然有效**的「远端已知 VV」
+    // 证据：必须在删除前落到非破坏性本地 meta 里，否则已有设备下一次快照追赶时会把
+    // 远端已删文档误判成 local-only（只会保守保留，但证据丢了）。
+    //
+    // **无条件执行**（P2-5）：走到这里只可能是「没有 journal」或「completed journal」
+    // （staging 同参已幂等早退、异参已 SyncConflict 返回）。completed journal 的
+    // baseline_vv 要回填；没有 journal 但残留子表（旧版本/异常写留下的孤儿行）也必须
+    // 先回填再清掉，否则下面无条件 `DELETE` 会把证据静默丢弃。
+    //
+    // `DO NOTHING` 是单调断言：已有 meta（普通 pull/integrate 记录的更新的远端已知 VV）
+    // 永远不降级；缺失时才由旧 baseline_vv 补上。
+    statements.push(Statement::new(
+        format!(
+            "INSERT INTO meta(key,value) \
+             SELECT '{}' || doc_id, baseline_vv FROM baseline_apply_docs WHERE applied=1 \
+             ON CONFLICT(key) DO NOTHING",
+            remote_known::META_REMOTE_KNOWN_VV_PREFIX
+        ),
+        Vec::new(),
+    ));
+    // 旧 journal 子表只属于旧 journal（completed 或孤儿残留），一律清掉，
+    // 避免它们被下一轮 `apply_baseline_batch` 当成「已应用」。
+    statements.push(Statement::new(
+        "DELETE FROM baseline_apply_docs",
+        Vec::new(),
+    ));
+    statements.push(Statement::new(
+        "DELETE FROM baseline_apply_tombstones",
+        Vec::new(),
+    ));
+    statements.push(Statement::new(
+        "DELETE FROM baseline_apply_assets",
+        Vec::new(),
+    ));
+    statements.push(Statement::new("DELETE FROM baseline_apply", Vec::new()));
     let insert_index = statements.len();
     statements.push(Statement::new(
         "INSERT INTO baseline_apply(\
@@ -309,6 +553,19 @@ pub fn begin_baseline_apply(
     ));
     guards.push(AffectedGuard::exactly(insert_index, 1));
 
+    // 预期参与集合随 journal 同事务落 meta（replace 语义：completed journal 被替换时
+    // 旧集合必须一起被覆盖，否则 finish 会拿上一轮的集合核对本轮进度）。
+    let expected_index = statements.len();
+    statements.push(Statement::new(
+        "INSERT INTO meta(key,value) VALUES(?1,?2) \
+         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
+        vec![
+            Value::Text(META_BASELINE_EXPECTED.to_owned()),
+            Value::Blob(expected_bytes),
+        ],
+    ));
+    guards.push(AffectedGuard::exactly(expected_index, 1));
+
     match vault.exec_transaction(&statements, &guards) {
         Ok(_) => {}
         Err(StorageError::GuardFailed { .. }) => {
@@ -325,9 +582,10 @@ pub fn begin_baseline_apply(
 
 /// 当前 staging journal 的只读状态；没有 journal 返回 `None`。
 pub fn baseline_apply_state(vault: &Vault) -> Result<Option<BaselineApplyState>, StorageError> {
-    load_journal(vault)?
-        .map(|journal| state_from_journal(&journal))
-        .transpose()
+    match load_journal(vault)? {
+        None => Ok(None),
+        Some(journal) => state_from_journal(vault, &journal).map(Some),
+    }
 }
 
 /// 应用一批已验证的快照文档和远端 tombstone。
@@ -338,7 +596,7 @@ pub fn apply_baseline_batch(
 ) -> Result<BaselineApplyState, StorageError> {
     let journal = load_active_journal(vault)?;
     if docs.is_empty() && tombstones.is_empty() {
-        return state_from_journal(&journal);
+        return state_from_journal(vault, &journal);
     }
 
     let mut seen_docs = BTreeSet::new();
@@ -484,9 +742,10 @@ pub fn apply_baseline_batch(
             }
         }
 
-        let remote_known = match remote_known::load(vault, doc_id)? {
-            Some(existing) => remote_known::union(&existing, &baseline_vv).encode(),
-            None => baseline_vv.encode(),
+        let remote_known_expected = remote_known::load(vault, doc_id)?;
+        let remote_known_merged = match &remote_known_expected {
+            Some(existing) => remote_known::union(existing, &baseline_vv),
+            None => baseline_vv.clone(),
         };
         prepared_docs.push(PreparedDoc {
             doc_id: doc_id.clone(),
@@ -494,7 +753,8 @@ pub fn apply_baseline_batch(
             baseline_vv: baseline_vv.encode(),
             expected_head,
             update,
-            remote_known,
+            remote_known_expected,
+            remote_known_merged,
         });
         baseline_by_doc.insert(doc_id.clone(), local_extra);
     }
@@ -515,6 +775,8 @@ pub fn apply_baseline_batch(
     let now = now_secs()?;
     let mut statements = Vec::new();
     let mut guards = Vec::new();
+    // 远端已知 VV 的 CAS 守卫下标 → doc_id：GuardFailed 时用它给出明确冲突错误。
+    let mut remote_known_guards: Vec<(usize, String)> = Vec::new();
     for prepared in &prepared_docs {
         let journal_index = statements.len();
         statements.push(Statement::new(
@@ -532,17 +794,18 @@ pub fn apply_baseline_batch(
         guards.push(AffectedGuard::exactly(journal_index, 1));
 
         // 跨 journal 保留的「远端已知 VV」：本批快照的 VV 就是远端已见过的 ops。
-        // 与旧值取单调并集（旧值可能来自普通 pull/integrate 的更新远端已知集）。
+        // 与旧值取单调并集（旧值可能来自普通 pull/integrate 的更新远端已知集），
+        // 但写入必须是**值 CAS**：只有库中值仍是准备阶段读到的旧值才更新。
+        // 并发写者（普通 pull 的 A 事务）已经扩大过 → 0 行 → 整批回滚并报明确冲突，
+        // 由调用方重试；绝不把更大的值覆盖回本批的并集（P2-5）。
         let remote_index = statements.len();
-        statements.push(Statement::new(
-            "INSERT INTO meta(key,value) VALUES(?1,?2) \
-             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
-            vec![
-                Value::Text(remote_known::meta_key(&prepared.doc_id)),
-                Value::Blob(prepared.remote_known.clone()),
-            ],
+        statements.push(remote_known::cas_statement(
+            &prepared.doc_id,
+            prepared.remote_known_expected.as_ref(),
+            &prepared.remote_known_merged,
         ));
         guards.push(AffectedGuard::exactly(remote_index, 1));
+        remote_known_guards.push((remote_index, prepared.doc_id.clone()));
 
         let Some(update) = &prepared.update else {
             if prepared.expected_head.is_none() {
@@ -650,23 +913,42 @@ pub fn apply_baseline_batch(
     }
 
     for tombstone in &prepared_tombstones {
-        if tombstone.action == "deleted" {
-            statements.push(Statement::new(
-                "UPDATE notes SET deleted=1 WHERE body_doc_id=?1",
-                vec![Value::Text(tombstone.doc_id.clone())],
-            ));
-        }
+        // P1-2（TOCTOU）：`tombstone_can_delete` 在事务外读 outbox，判定完成后本地编辑的
+        // A 事务完全可能插进一条 `sync_outbox` 行；若仍按旧判定无条件软删，就会把刚编辑
+        // 的笔记删掉。因此最终 action 必须在**同一个写事务里**用 SQL 重判一次：
+        // 只允许 `deleted → kept_local_edit` 的降级，绝不允许 `kept → deleted` 升级。
         let index = statements.len();
         statements.push(Statement::new(
-            "INSERT INTO baseline_apply_tombstones(doc_id,action,updated_at) VALUES(?1,?2,?3) \
-             ON CONFLICT(doc_id) DO UPDATE SET action=excluded.action,updated_at=excluded.updated_at",
+            "INSERT INTO baseline_apply_tombstones(doc_id,action,updated_at) \
+             VALUES(?1, CASE WHEN EXISTS(SELECT 1 FROM sync_outbox WHERE doc_id=?1) \
+                             THEN 'kept_local_edit' ELSE ?2 END, ?3) \
+             ON CONFLICT(doc_id) DO UPDATE SET \
+               action=CASE WHEN EXISTS(SELECT 1 FROM sync_outbox WHERE doc_id=?1) \
+                           THEN 'kept_local_edit' ELSE excluded.action END, \
+               updated_at=excluded.updated_at",
             vec![
                 Value::Text(tombstone.doc_id.clone()),
                 Value::Text(tombstone.action.to_owned()),
                 Value::Integer(now),
             ],
         ));
+        // 这一句永远恰好写入 1 行（INSERT 或 DO UPDATE），是"判定已落库"的守卫：
+        // 说明事务内重判真的执行了，而不是被别的写入/约束挡掉。
         guards.push(AffectedGuard::exactly(index, 1));
+
+        if tombstone.action == "deleted" {
+            // 软删与 outbox 判定在同一事务内：只有重判仍为 `deleted`（且此刻没有
+            // outbox）才置 deleted=1。0 行是合法结果（该文档没有 notes 行，或已降级为
+            // kept_local_edit），所以这句本身不设 exactly 守卫；守卫语义在上一条。
+            statements.push(Statement::new(
+                "UPDATE notes SET deleted=1 \
+                 WHERE body_doc_id=?1 \
+                   AND NOT EXISTS(SELECT 1 FROM sync_outbox WHERE doc_id=?1) \
+                   AND EXISTS(SELECT 1 FROM baseline_apply_tombstones \
+                              WHERE doc_id=?1 AND action='deleted')",
+                vec![Value::Text(tombstone.doc_id.clone())],
+            ));
+        }
     }
 
     let journal_index = statements.len();
@@ -684,7 +966,20 @@ pub fn apply_baseline_batch(
     let transaction_started = Instant::now();
     match vault.exec_transaction(&statements, &guards) {
         Ok(_) => {}
-        Err(StorageError::GuardFailed { .. }) => {
+        Err(StorageError::GuardFailed { statement, .. }) => {
+            if let Some((_, doc_id)) = remote_known_guards
+                .iter()
+                .find(|(index, _)| *index == statement)
+            {
+                // 值 CAS 不命中：准备阶段读到的旧值已被普通 pull 的 A 事务改写。
+                // 整批已回滚（update/head/journal 都没有落库），重跑本批即可读到并集。
+                return Err(StorageError::SyncConflict {
+                    context: format!(
+                        "baseline 文档 {doc_id} 的远端已知 VV 值 CAS 失败：\
+                         并发写者已改写旧值；整批已回滚，可重试"
+                    ),
+                });
+            }
             return Err(StorageError::SyncConflict {
                 context: "baseline batch 期间 journal/doc head 已变化".to_owned(),
             });
@@ -705,7 +1000,7 @@ pub fn apply_baseline_assets(
 ) -> Result<BaselineApplyState, StorageError> {
     let journal = load_active_journal(vault)?;
     if assets.is_empty() {
-        return state_from_journal(&journal);
+        return state_from_journal(vault, &journal);
     }
 
     let mut seen_oid = BTreeSet::new();
@@ -826,6 +1121,10 @@ pub fn apply_baseline_assets(
 ///
 /// CAS 仍是"begin 时记录、finish 时校验"：任一字段在 finish 前被并发改写即返回
 /// [`BaselineApplyFinish::Conflict`]，整笔事务不写。
+///
+/// **完整性前提**（P1-1）：begin 记录在 [`META_BASELINE_EXPECTED`] 的预期参与集合必须
+/// 全部 `applied=1`。少跑一批（哪怕 status 仍是 staging）返回 Conflict；旧 journal
+/// 没有集合记录同样 Conflict（拿不到证明就不允许推进 S），绝不明知不完整还写水位。
 pub fn finish_baseline_apply(
     vault: &Vault,
     chosen_baseline_s: u64,
@@ -839,6 +1138,14 @@ pub fn finish_baseline_apply(
         return Ok(BaselineApplyFinish::Conflict);
     }
 
+    // 完整性校验在任何写语句之前：拿不到 begin 的预期集合，或集合未全齐，一律 Conflict。
+    let Some(expected) = load_expected_set(vault)? else {
+        return Ok(BaselineApplyFinish::Conflict);
+    };
+    if !expected_is_complete(vault, &journal, &expected)? {
+        return Ok(BaselineApplyFinish::Conflict);
+    }
+
     // finish 前先校验 begin 时记录的旧值；任何一项被并发改写都按 Conflict 处理，不写任何行。
     let current = sync_cursor_state(vault)?;
     let current_relay_cursor = vault.get_meta(META_SYNC_RELAY_CURSOR)?;
@@ -937,13 +1244,21 @@ pub fn finish_baseline_apply(
     Ok(BaselineApplyFinish::Applied(state))
 }
 
-/// 只清 staging journal（含子表）；已导入的 Loro update/资产保留。
+/// 只清 staging journal（含子表、预期参与集合与失败标记）；已导入的 Loro update/资产保留。
+///
+/// 没有 staging journal 时仍会清掉可能残留的 [`META_BASELINE_VERIFY_FAILED`]（abort 是
+/// 失败标记的唯一清理入口），并返回 `false` 表示没有 journal 可中止。
 pub fn abort_baseline_apply(vault: &Vault) -> Result<bool, StorageError> {
-    let Some(journal) = load_journal(vault)? else {
-        return Ok(false);
-    };
-    if journal.status != BaselineApplyStatus::Staging {
-        return Ok(false);
+    match load_journal(vault)? {
+        Some(journal) if journal.status == BaselineApplyStatus::Staging => {}
+        _ => {
+            // completed journal 的数据不动；只清失败标记（若存在），保持返回 false 的既有语义。
+            let _cleared = vault.exec(
+                "DELETE FROM meta WHERE key=?1",
+                &[Value::Text(META_BASELINE_VERIFY_FAILED.to_owned())],
+            )?;
+            return Ok(false);
+        }
     }
     let statements = [
         Statement::new("DELETE FROM baseline_apply_docs", Vec::new()),
@@ -953,6 +1268,17 @@ pub fn abort_baseline_apply(vault: &Vault) -> Result<bool, StorageError> {
             "DELETE FROM baseline_apply WHERE singleton=1 AND status='staging'",
             Vec::new(),
         ),
+        // 预期集合只属于被清掉的 journal；留着会被下一次 begin 的 replace 覆盖，
+        // 但清掉可以避免"没有 journal 却有预期集合"的中间态被误读。
+        Statement::new(
+            "DELETE FROM meta WHERE key=?1",
+            vec![Value::Text(META_BASELINE_EXPECTED.to_owned())],
+        ),
+        // 失败标记随失败 journal 一起清：abort 之后才允许重新 begin。
+        Statement::new(
+            "DELETE FROM meta WHERE key=?1",
+            vec![Value::Text(META_BASELINE_VERIFY_FAILED.to_owned())],
+        ),
     ];
     match vault.exec_transaction(&statements, &[AffectedGuard::exactly(3, 1)]) {
         Ok(_) => Ok(true),
@@ -1134,6 +1460,86 @@ fn load_applied_doc_digests(vault: &Vault) -> Result<BTreeMap<String, [u8; 32]>,
         .collect()
 }
 
+/// 读 begin 记录的预期参与集合；缺失返回 `None`（旧 journal/异常，按不可 finish 处理）。
+fn load_expected_set(vault: &Vault) -> Result<Option<BaselineExpectation>, StorageError> {
+    match vault.get_meta(META_BASELINE_EXPECTED)? {
+        None => Ok(None),
+        Some(bytes) => BaselineExpectation::decode(&bytes).map(Some),
+    }
+}
+
+/// 预期集合是否已全部 `applied=1`。
+///
+/// 先比计数（每批都会更新 `applied_docs/applied_assets`，因此未齐时 O(1) 就能拒绝），
+/// 计数相等后再逐项确认集合身份：光比数量会放过"漏一篇 + 多跑一篇"的情况。计数相等
+/// 且预期项全部在已应用集合中 ⇒ 两个集合相等。
+fn expected_is_complete(
+    vault: &Vault,
+    journal: &ApplyJournal,
+    expected: &BaselineExpectation,
+) -> Result<bool, StorageError> {
+    if journal.applied_docs != expected.doc_count()
+        || journal.applied_assets != expected.asset_count()
+    {
+        return Ok(false);
+    }
+    if expected.is_empty() {
+        return Ok(true);
+    }
+    let applied_docs: BTreeSet<String> = load_applied_doc_digests(vault)?.into_keys().collect();
+    let docs_complete = expected
+        .docs()
+        .iter()
+        .all(|doc_id| applied_docs.contains(doc_id));
+    if !docs_complete {
+        return Ok(false);
+    }
+    let applied_assets = load_applied_asset_ids(vault)?;
+    Ok(expected
+        .assets()
+        .iter()
+        .all(|object_id| applied_assets.contains(object_id)))
+}
+
+/// 当前 journal 中 `applied=1` 的资产 object_id 集合。
+fn load_applied_asset_ids(vault: &Vault) -> Result<BTreeSet<[u8; 32]>, StorageError> {
+    let rows = vault.query(
+        "SELECT object_id FROM baseline_apply_assets WHERE applied=1",
+        &[],
+    )?;
+    rows.iter()
+        .map(|row| array32(row, 0, "baseline_apply_assets.object_id"))
+        .collect()
+}
+
+fn corrupt_expected(reason: &str) -> StorageError {
+    StorageError::Corrupt {
+        context: format!("meta {META_BASELINE_EXPECTED} 非法：{reason}"),
+    }
+}
+
+fn take_expected_bytes<'a>(
+    bytes: &'a [u8],
+    offset: &mut usize,
+    len: usize,
+) -> Result<&'a [u8], StorageError> {
+    let end = offset
+        .checked_add(len)
+        .ok_or_else(|| corrupt_expected("长度溢出"))?;
+    let slice = bytes
+        .get(*offset..end)
+        .ok_or_else(|| corrupt_expected("长度越界"))?;
+    *offset = end;
+    Ok(slice)
+}
+
+fn read_expected_u64(bytes: &[u8], offset: &mut usize) -> Result<u64, StorageError> {
+    let raw = take_expected_bytes(bytes, offset, 8)?;
+    let mut value = [0u8; 8];
+    value.copy_from_slice(raw);
+    Ok(u64::from_be_bytes(value))
+}
+
 fn load_active_journal(vault: &Vault) -> Result<ApplyJournal, StorageError> {
     let journal = load_journal(vault)?.ok_or_else(|| StorageError::SyncConflict {
         context: "没有 active baseline apply".to_owned(),
@@ -1185,7 +1591,22 @@ fn load_journal(vault: &Vault) -> Result<Option<ApplyJournal>, StorageError> {
     }))
 }
 
-fn state_from_journal(journal: &ApplyJournal) -> Result<BaselineApplyState, StorageError> {
+fn state_from_journal(
+    vault: &Vault,
+    journal: &ApplyJournal,
+) -> Result<BaselineApplyState, StorageError> {
+    let expected = load_expected_set(vault)?;
+    let (expected_docs, expected_assets) = match &expected {
+        Some(expected) => (Some(expected.doc_count()), Some(expected.asset_count())),
+        None => (None, None),
+    };
+    // finishable 必须反映**真实就绪**：仅 status=staging 不够，还要 begin 记录的预期
+    // 集合全部 applied=1（否则 finish 会 Conflict，不能对 UI/续跑逻辑谎报可完成）。
+    let finishable = journal.status == BaselineApplyStatus::Staging
+        && match &expected {
+            Some(expected) => expected_is_complete(vault, journal, expected)?,
+            None => false,
+        };
     Ok(BaselineApplyState {
         checkpoint_id: journal.checkpoint_id,
         baseline_s: journal.baseline_s,
@@ -1194,7 +1615,9 @@ fn state_from_journal(journal: &ApplyJournal) -> Result<BaselineApplyState, Stor
         applied_docs: journal.applied_docs,
         applied_assets: journal.applied_assets,
         kept_local_edit: journal.kept_local_edit,
-        finishable: journal.status == BaselineApplyStatus::Staging,
+        expected_docs,
+        expected_assets,
+        finishable,
         chosen_cursor: journal.chosen_cursor.clone(),
     })
 }
diff --git a/crates/storage/src/error.rs b/crates/storage/src/error.rs
index 569c8d1..02c8aaa 100644
--- a/crates/storage/src/error.rs
+++ b/crates/storage/src/error.rs
@@ -103,6 +103,34 @@ pub enum StorageError {
         actual: u64,
     },
 
+    /// 未 ACK journal 的 CAS 基准已过期：head 被别的 writer 推进了。
+    ///
+    /// journal 条目的 `PendingUpdate` 是**追加那一刻**按当时 head 导出的字节
+    /// （`end_frontier` / `end_vv` 都是那次导出的快照），**不可重算**。head 一旦前进，
+    /// 同一份字节写一万次也只会再得到一次 CAS 冲突 —— 所以这是**终态**错误，
+    /// 由 [`StorageError::requires_session_rebuild`] 显式标记：
+    ///
+    /// - **谁负责重建**：持有 live `BodyDoc` 的会话层（`session::JournalHandle` /
+    ///   `SessionDriver`）。它必须先把内存文档重放到新 head，再按新 head 重新导出增量，
+    ///   并用新 `head_seq` 造 sink（见 `crate::journal` 模块头的契约）；
+    /// - **旧条目**：不会被写入，也**不会**被静默丢弃 —— `JournalWriter::rebuild_sink`
+    ///   把它们原样交还给调用方；
+    /// - **重试**：本层自动重试次数为 0，`flush` 之后不再触碰数据库。
+    #[error(
+        "文档 {doc_id} 的未 ACK journal 基准已过期（期望 head {expected}，实际 {actual}）：\
+         {pending} 条已导出增量不可重算，必须重建 session 后重新导出；已禁止重试"
+    )]
+    JournalHeadStale {
+        /// 文档 ID。
+        doc_id: String,
+        /// 写入时使用的 CAS 基准（`loro_docs.head_seq`）。
+        expected: u64,
+        /// 库里实际看到的 head。
+        actual: u64,
+        /// 过期时 journal 里未 durable 的条数。
+        pending: usize,
+    },
+
     /// 未 ACK journal 已满：必须先 flush 再重试（背压，不允许无界增长）。
     #[error("未 ACK journal 已满（容量 {capacity} 条），拒绝新写入；请先 flush")]
     WriteBackpressure {
@@ -175,3 +203,31 @@ pub enum StorageError {
         context: String,
     },
 }
+
+impl StorageError {
+    /// 是否必须**重建 session**（而不是重试）。
+    ///
+    /// 目前只有 [`Self::JournalHeadStale`]：那批 `PendingUpdate` 是按旧 head 导出的字节，
+    /// 没有任何 rebase 可能，原样重试永远不会成功。调用方必须先重建 session、重新导出
+    /// 增量（见 `crate::journal` 模块头），否则只会把失败换个地方再报一次。
+    #[must_use]
+    pub fn requires_session_rebuild(&self) -> bool {
+        matches!(self, Self::JournalHeadStale { .. })
+    }
+
+    /// 是否值得**原样重试**（只表示"条件变化后可能成功"，不表示"一定要重试"）。
+    ///
+    /// - [`Self::HeadConflict`]：store 层 CAS 冲突 —— 重读 head 后重放（sync engine 语义，
+    ///   与 journal 层的[`Self::JournalHeadStale`]不同：那里的字节可以重新导出）；
+    /// - [`Self::SnapshotBusy`]：读锁超时 —— 退避后重开快照；
+    /// - [`Self::WriteBackpressure`]：journal 满 —— 先 flush 再重试。
+    ///
+    /// 其余变体一律 `false`：盲目重试只会挂死，或把真正的损坏/配置错误掩盖成"暂时失败"。
+    #[must_use]
+    pub fn is_retryable(&self) -> bool {
+        matches!(
+            self,
+            Self::HeadConflict { .. } | Self::SnapshotBusy { .. } | Self::WriteBackpressure { .. }
+        )
+    }
+}
diff --git a/crates/storage/src/journal.rs b/crates/storage/src/journal.rs
index 4aea6e6..f2d36cb 100644
--- a/crates/storage/src/journal.rs
+++ b/crates/storage/src/journal.rs
@@ -12,12 +12,46 @@
 //!
 //! 崩溃语义：sink 成功 = 已 durable；未 flush 的条目只在内存里，重启后自然丢掉。
 //! 因此「已 ACK 的不丢、未 ACK 的只丢连续前缀之后」由本模块与 [`crate::store`] 的 A 事务共同保证。
+//!
+//! # head 过期：可重试 vs 必须重建 session（P1-3 契约）
+//!
+//! journal 条目里的 `PendingUpdate` 是**追加那一刻**按当时 head 导出的字节
+//! （`end_frontier` / `end_vv` 是那次导出的快照），**不可重算**。如果同一文档的 head
+//! 被别的 writer 推进（同步引擎 integrate、`SessionDriver` 的 adopt、另一个 Vault
+//! 进程……），前缀的 CAS 基准（`loro_docs.head_seq`）就永远对不上：`append_updates`
+//! 每次都返回 [`StorageError::HeadConflict`]，但**同一份字节重试多少次都不会成功**。
+//! 这正是旧实现「flush 一直 HeadConflict、水位永远不前进」的卡死来源。
+//!
+//! 本模块用一条明确的二分法代替无限重试：
+//!
+//! - [`JournalWriter::flush`] 捕获 sink 的 [`StorageError::HeadConflict`]，**不重试**
+//!   （0 次），立即转成终态错误 [`StorageError::JournalHeadStale`]
+//!   （`doc_id` / `expected` / `actual` / `pending` 齐全，
+//!   [`StorageError::requires_session_rebuild`] 为 `true`）。其它错误（磁盘失败等）
+//!   原样透传 —— 那才是可重试的，journal 原样保留。
+//! - 进入 stale 之后，`flush` / `flush_all` **不再触碰数据库**，幂等地返回同一个
+//!   stale 错误；`submit` 也返回 `JournalHeadStale`（writer 已冻结，新条目必须交给
+//!   重建后的 writer）。因此调用方无论怎么轮询，都不会挂死、也不会刷屏重试。
+//! - **谁负责重建**：持有 live `BodyDoc` 的会话层（`session::JournalHandle` /
+//!   `SessionDriver`，实现在 `crate::session::flush`）。重建顺序：
+//!   ① 先停外部写并按 ADR 0012 对账（`HeadConflict` 先停外部写、验证授权、再对账，
+//!   然后才允许有界重放）；② 把 durable journal 重放到**新 head**（`load_body_doc` /
+//!   `load_head`），让内存文档与库一致；③ 用新 head 的 VV 重新导出本地意图
+//!   （`BodyDoc::pending_update`）——只有这一步得到的字节才能写；④ 用新 `head_seq`
+//!   造一个 [`VaultJournalSink`]，调用 [`JournalWriter::rebuild_sink`] 装回去。
+//! - **未 durable 的旧条目怎么办**：`rebuild_sink` 把它们**原样交还**调用方（返回值）
+//!   并从 writer 的 journal 里清空。它们**没有被写入、也没有被静默丢弃** —— 调用方
+//!   必须用 ③ 重新导出的增量覆盖这段内容；若调用方决定不要，也必须显式消费返回值
+//!   并在上层报告，不能假装成功。重建后 `durable_seq` 等于新 sink 的 CAS 基准。
+//! - **有界重试规则**：CAS 冲突在 journal 层的自动重试是 **0** 次（基准是常量，
+//!   重试不可能成功）；需要重放时由 session 层按 ADR 0012 做**有界**重放（一次），
+//!   再失败就上报告需要人工处理。本模块不提供、也不会退化成无限重试。
 
 use std::collections::VecDeque;
 
 use crate::body_doc::PendingUpdate;
 use crate::error::StorageError;
-use crate::store::UpdateRecord;
+use crate::store::{OriginMarks, UpdateRecord};
 use crate::sync_bridge::{self, SyncContext, SyncIntent};
 use crate::Vault;
 
@@ -161,6 +195,14 @@ impl PendingJournal {
         }
         Ok(())
     }
+
+    /// 取走全部未 ACK 条目。
+    ///
+    /// **只**供 [`JournalWriter::rebuild_sink`] 在 head 过期后显式交还给调用方使用：
+    /// 正常路径一律走 [`Self::ack_prefix`]，这里既不写库也不推进任何水位。
+    fn drain_all(&mut self) -> Vec<JournalEntry> {
+        self.entries.drain(..).collect()
+    }
 }
 
 /// durable 落点（生产实现见 [`VaultJournalSink`]，测试可注入慢盘/失败实现）。
@@ -168,7 +210,16 @@ pub trait DurableSink {
     /// 把一段**连续前缀**写 durable；成功返回这段里最后一条的 `seq`。
     ///
     /// 实现必须是原子的：要么整段 durable，要么什么都没写（否则会丢已 ACK 数据）。
+    ///
+    /// CAS 基准冲突必须返回 [`StorageError::HeadConflict`]（不能自己改基准硬写）：
+    /// [`JournalWriter`] 据此把 writer 判成 head 过期，而不是无限重试同一份字节。
     fn write_prefix(&mut self, entries: &[JournalEntry]) -> Result<u64, StorageError>;
+
+    /// 当前 sink 使用的 CAS 基准（= 它认为已 durable 的 `head_seq`）。
+    ///
+    /// writer 用它给 `durable_seq` 定初值，并在 [`JournalWriter::rebuild_sink`] 之后重新对齐
+    /// 水位。实现必须返回**真正用于 CAS 的**那个值，否则重建后的水位会骗人。
+    fn head_seq(&self) -> u64;
 }
 
 /// 一次 flush 的结果。
@@ -180,6 +231,28 @@ pub struct FlushReport {
     pub durable_seq: u64,
 }
 
+/// head 过期的终态记录（见模块头契约）。
+///
+/// 只要它非空，writer 就冻结：`submit` / `flush` 一律返回同一个
+/// [`StorageError::JournalHeadStale`]，直到调用方 [`JournalWriter::rebuild_sink`]。
+#[derive(Debug, Clone, PartialEq, Eq)]
+struct StaleHead {
+    doc_id: String,
+    expected: u64,
+    actual: u64,
+}
+
+impl StaleHead {
+    fn to_error(&self, pending: usize) -> StorageError {
+        StorageError::JournalHeadStale {
+            doc_id: self.doc_id.clone(),
+            expected: self.expected,
+            actual: self.actual,
+            pending,
+        }
+    }
+}
+
 /// journal writer：持有 journal 与 sink，串行 flush。
 #[derive(Debug)]
 pub struct JournalWriter<S: DurableSink> {
@@ -188,29 +261,52 @@ pub struct JournalWriter<S: DurableSink> {
     max_batch: usize,
     durable_seq: u64,
     flushed_entries: u64,
+    /// 非空 = CAS 基准已过期，writer 已冻结（见模块头契约）。
+    stale: Option<StaleHead>,
 }
 
 impl<S: DurableSink> JournalWriter<S> {
     /// `max_batch` 是一次 flush 最多合并多少条（慢 I/O 时 pending 会堆到这里为止）。
+    ///
+    /// `durable_seq` 初值取 sink 的 CAS 基准（[`DurableSink::head_seq`]），
+    /// 因此空 journal 上 flush 报告的是真实水位，而不是 0。
     pub fn new(sink: S, capacity: usize, max_batch: usize) -> Self {
+        let durable_seq = sink.head_seq();
         Self {
             journal: PendingJournal::new(capacity),
             sink,
             max_batch: max_batch.max(1),
-            durable_seq: 0,
+            durable_seq,
             flushed_entries: 0,
+            stale: None,
         }
     }
 
     /// 追加一条待落库 update；满了就是背压，调用方必须 flush 后重试。
+    ///
+    /// head 过期后拒绝（返回 [`StorageError::JournalHeadStale`]）：writer 已冻结，
+    /// 新条目必须交给重建后的 writer，否则只会堆在一条永远写不动的前缀后面。
     pub fn submit(&mut self, entry: JournalEntry) -> Result<u64, StorageError> {
+        if let Some(stale) = self.stale.as_ref() {
+            return Err(stale.to_error(self.journal.pending_len()));
+        }
         self.journal.append(entry)
     }
 
     /// 把当前前缀合并成一次 sink 写；成功后裁剪 journal 并推进水位。
     ///
     /// 失败时 journal **原样保留**（sink 承诺原子），下次重试不会丢已成功部分。
+    ///
+    /// 唯一的例外是 CAS 冲突：sink 返回 [`StorageError::HeadConflict`] 时前缀字节已不可
+    /// 重放，本方法**不重试**（0 次），标记 stale 并返回
+    /// [`StorageError::JournalHeadStale`] —— 调用方必须按模块头的契约重建 session，
+    /// 而不是再 flush 一次。
     pub fn flush(&mut self) -> Result<FlushReport, StorageError> {
+        // 终态优先：stale 之后连数据库都不碰（有界重试 = 0 次）。
+        if let Some(stale) = self.stale.as_ref() {
+            return Err(stale.to_error(self.journal.pending_len()));
+        }
+
         // 先拷出前缀，避免同时借用 journal 与 sink
         let prefix: Vec<JournalEntry> = self.journal.peek_prefix(self.max_batch).to_vec();
         if prefix.is_empty() {
@@ -221,7 +317,27 @@ impl<S: DurableSink> JournalWriter<S> {
         }
 
         let entries = prefix.len();
-        let durable_seq = self.sink.write_prefix(&prefix)?;
+        let durable_seq = match self.sink.write_prefix(&prefix) {
+            Ok(seq) => seq,
+            // CAS 冲突是**终态**：前缀的 end_vv/end_frontier 是按旧 head 导出的快照，
+            // 重新导出只能由持有 live 文档的会话层做；同一份字节重试不可能成功。
+            Err(StorageError::HeadConflict {
+                doc_id,
+                expected,
+                actual,
+            }) => {
+                let stale = StaleHead {
+                    doc_id,
+                    expected,
+                    actual,
+                };
+                let error = stale.to_error(self.journal.pending_len());
+                self.stale = Some(stale);
+                return Err(error);
+            }
+            // 其它错误（磁盘失败等）原样透传：journal 原样保留，同一前缀可以重试。
+            Err(error) => return Err(error),
+        };
         self.journal.ack_prefix(entries)?;
         self.durable_seq = durable_seq;
         self.flushed_entries += entries as u64;
@@ -233,6 +349,8 @@ impl<S: DurableSink> JournalWriter<S> {
     }
 
     /// flush 直到 journal 空或没有进展（一次性排空，供关闭/手动保存用）。
+    ///
+    /// head 过期时第一拍就返回 [`StorageError::JournalHeadStale`]，不会空转。
     pub fn flush_all(&mut self) -> Result<usize, StorageError> {
         let mut total = 0;
         while self.journal.pending_len() > 0 {
@@ -245,6 +363,35 @@ impl<S: DurableSink> JournalWriter<S> {
         Ok(total)
     }
 
+    /// writer 是否已进入「head 过期、必须重建」的终态。
+    pub fn is_stale(&self) -> bool {
+        self.stale.is_some()
+    }
+
+    /// 重建 sink：head 过期后**唯一**合法的恢复入口（见模块头契约）。
+    ///
+    /// 调用方必须先用新 head 重放 live 文档并重新导出本地意图；本方法只负责把 writer
+    /// 从 stale 拉回可用状态：
+    ///
+    /// - 把旧 journal 里未写入的条目**原样交还**（返回值），并从 journal 里清空 ——
+    ///   它们不写库、也**不被静默丢弃**；调用方要么用重新导出的增量覆盖其内容，
+    ///   要么显式报告这次丢弃；
+    /// - `durable_seq` 对齐新 sink 的 CAS 基准（[`DurableSink::head_seq`]）；
+    /// - 未处于 stale 时拒绝：还有能写的前缀就换 sink，等于把本可落库的条目丢掉。
+    pub fn rebuild_sink(&mut self, sink: S) -> Result<Vec<JournalEntry>, StorageError> {
+        if self.stale.is_none() {
+            return Err(StorageError::Corrupt {
+                context: "journal 未处于 head 过期状态，拒绝重建 sink（会丢弃仍可写入的条目）"
+                    .to_owned(),
+            });
+        }
+        let handed_back = self.journal.drain_all();
+        self.sink = sink;
+        self.stale = None;
+        self.durable_seq = self.sink.head_seq();
+        Ok(handed_back)
+    }
+
     /// 未 ACK 条数。
     pub fn pending_len(&self) -> usize {
         self.journal.pending_len()
@@ -343,7 +490,7 @@ impl DurableSink for VaultJournalSink {
                 self.head_seq,
                 &records,
                 &[],
-                &remote_marks,
+                OriginMarks::PerRecord(&remote_marks),
             )?,
             Some(ctx) => {
                 // 一 update 一意图（remote 条目为空位）；seq 必须与 A 事务写进
@@ -374,11 +521,16 @@ impl DurableSink for VaultJournalSink {
                     self.head_seq,
                     &records,
                     &slots,
-                    &remote_marks,
+                    OriginMarks::PerRecord(&remote_marks),
                 )?
             }
         };
         self.head_seq = head.head_seq;
         Ok(head.head_seq)
     }
+
+    /// CAS 基准 = 本 sink 已 durable 的 head_seq（与固有方法 `head_seq` 同一个值）。
+    fn head_seq(&self) -> u64 {
+        self.head_seq
+    }
 }
diff --git a/crates/storage/src/lib.rs b/crates/storage/src/lib.rs
index 884e5eb..0baaaf0 100644
--- a/crates/storage/src/lib.rs
+++ b/crates/storage/src/lib.rs
@@ -70,9 +70,11 @@ pub use asset_store::{
 pub use baseline_apply::{
     abort_baseline_apply, apply_baseline_assets, apply_baseline_batch,
     baseline_apply_debug_counters, baseline_apply_debug_timings, baseline_apply_state,
-    begin_baseline_apply, finish_baseline_apply, reset_baseline_apply_debug,
+    baseline_apply_verify_ms, baseline_verify_failure, begin_baseline_apply, finish_baseline_apply,
+    mark_baseline_verify_failed, record_baseline_verify_time, reset_baseline_apply_debug,
     BaselineApplyDebugCounters, BaselineApplyDebugTimings, BaselineApplyFinish, BaselineApplyState,
-    BaselineApplyStatus, BaselineAsset, BASELINE_MERGE_BATCH, META_SYNC_RELAY_CURSOR,
+    BaselineApplyStatus, BaselineAsset, BaselineExpectation, BASELINE_MERGE_BATCH,
+    META_BASELINE_EXPECTED, META_BASELINE_VERIFY_FAILED, META_SYNC_RELAY_CURSOR,
 };
 pub use body_doc::{
     BodyDoc, PendingUpdate, APP_DOC_SCHEMA, BODY_CONTAINER, CODEC_VERSION, MIN_READER,
diff --git a/crates/storage/src/remote_known.rs b/crates/storage/src/remote_known.rs
index 467b080..3f9bbf2 100644
--- a/crates/storage/src/remote_known.rs
+++ b/crates/storage/src/remote_known.rs
@@ -24,6 +24,17 @@
 //! 本地写入**不会**扩大它——即使已经 ACK 上传也不扩大。这是刻意的保守取舍：
 //! tombstone 判定只会更倾向于 `kept_local_edit`，绝不会把本地未确证同步过的内容删掉。
 //!
+//! ## 单调性怎么强制（P2-5）
+//!
+//! 「只读旧值、在事务外算并集、再无条件覆盖」有两个写者互相覆盖的窗口（普通 pull 的
+//! A 事务与 baseline apply 批次都可能写同一个键）。因此所有写入都改成**值 CAS**：
+//!
+//! - 生成语句时带上读到的旧值 `expected_old`，SQL 是
+//!   `INSERT ... ON CONFLICT(key) DO UPDATE SET value=excluded.value WHERE meta.value=?old`；
+//!   旧值不存在时用 `INSERT ... SELECT ... WHERE NOT EXISTS`；
+//! - 语句带 `AffectedGuard::exactly(index, 1)`：CAS 不命中就是 0 行 → 整笔事务回滚，
+//!   调用方拿到明确的冲突错误（重读后重试即收敛到并集），**绝不会写入更小的值**。
+//!
 //! ## 判定
 //!
 //! `local_vv.sub_iter(remote_known).next().is_none()` 表示本地没有任何远端未知的 op，
@@ -56,7 +67,7 @@ pub(crate) fn load(vault: &Vault, doc_id: &str) -> Result<Option<VersionVector>,
     };
     match row.first() {
         Some(Value::Blob(bytes)) => Ok(Some(decode(bytes)?)),
-        Some(Value::Null) | None => Ok(None),
+        // meta 值的统一口径（P2-6）：只认 BLOB；NULL/INTEGER/TEXT 都视为库被改坏。
         other => Err(StorageError::Corrupt {
             context: format!("{key} 读回异常：{other:?}"),
         }),
@@ -99,18 +110,59 @@ pub(crate) fn added_since(after: &VersionVector, before: &VersionVector) -> Vers
     added
 }
 
-/// 纯 SQL 语句：把某篇文档的远端已知 VV 覆盖为 `merged`（调用方负责事务与单调并集）。
-pub(crate) fn upsert_statement(doc_id: &str, merged: &VersionVector) -> Statement {
-    Statement::new(
-        "INSERT INTO meta(key,value) VALUES(?1,?2) \
-         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
-        vec![Value::Text(meta_key(doc_id)), Value::Blob(merged.encode())],
-    )
+/// 生成一条**值 CAS** upsert 语句（调用方负责把它放进 A 事务，并加
+/// `AffectedGuard::exactly(语句下标, 1)`）。
+///
+/// - `expected = Some(old)`：`INSERT ... ON CONFLICT(key) DO UPDATE ... WHERE meta.value=?old`。
+///   只有库中值仍是读到的旧值时才写入；被并发写者改过就是 0 行 → 守卫失败 → 整笔回滚；
+/// - `expected = None`：`INSERT ... SELECT ... WHERE NOT EXISTS`。只有键仍不存在时才插入；
+///   并发插入过就是 0 行 → 同样回滚。
+///
+/// 两种写法都不会用旧并集覆盖（更小的）并发结果，因此值只能单调扩大。
+pub(crate) fn cas_statement(
+    doc_id: &str,
+    expected: Option<&VersionVector>,
+    merged: &VersionVector,
+) -> Statement {
+    assert_monotone(expected, merged);
+    let key = meta_key(doc_id);
+    let encoded = merged.encode();
+    match expected {
+        Some(old) => Statement::new(
+            "INSERT INTO meta(key,value) VALUES(?1,?2) \
+             ON CONFLICT(key) DO UPDATE SET value=excluded.value \
+             WHERE meta.value=?3",
+            vec![
+                Value::Text(key),
+                Value::Blob(encoded),
+                Value::Blob(old.encode()),
+            ],
+        ),
+        None => Statement::new(
+            "INSERT INTO meta(key,value) SELECT ?1,?2 \
+             WHERE NOT EXISTS(SELECT 1 FROM meta WHERE key=?1)",
+            vec![Value::Text(key), Value::Blob(encoded)],
+        ),
+    }
+}
+
+/// 调试断言：CAS 的目标值必须包含读到的旧值（`old ⊆ merged`）。
+///
+/// 生产路径由 [`union`] 构造 merged，这里只是把「只能扩大」的契约钉在生成语句处；
+/// 一旦将来有人改成直接写入更小的值，debug/test 构建会立刻失败。
+fn assert_monotone(expected: Option<&VersionVector>, merged: &VersionVector) {
+    if let Some(old) = expected {
+        debug_assert!(
+            old.sub_iter(merged).next().is_none(),
+            "remote known VV 只能单调扩大：旧值必须是 merged 的子集"
+        );
+    }
 }
 
-/// 在 A 事务**之外**读旧值并返回合并后的 upsert 语句；`added` 为空时返回 `None`。
+/// 在 A 事务**之外**读旧值并返回合并后的 **CAS** 语句；`added` 为空时返回 `None`。
 ///
-/// 事务内的单调性由「先读后写 + 单写者」保证：所有写都经 Vault actor 串行执行。
+/// 旧值只作为 CAS 的期望值；若事务执行时值已被并发写者扩大，语句影响 0 行、守卫失败，
+/// 调用方必须重读（重新调用本函数）后重试。这里**不会**用读到的旧并集覆盖库里的新值。
 pub(crate) fn merge_statement(
     vault: &Vault,
     doc_id: &str,
@@ -119,9 +171,166 @@ pub(crate) fn merge_statement(
     if added.is_empty() {
         return Ok(None);
     }
-    let merged = match load(vault, doc_id)? {
-        Some(existing) => union(&existing, added),
+    let existing = load(vault, doc_id)?;
+    let merged = match &existing {
+        Some(existing) => union(existing, added),
         None => added.clone(),
     };
-    Ok(Some(upsert_statement(doc_id, &merged)))
+    Ok(Some(cas_statement(doc_id, existing.as_ref(), &merged)))
+}
+
+#[cfg(test)]
+mod tests {
+    use std::path::Path;
+    use std::sync::mpsc;
+    use std::sync::{Arc, Barrier};
+    use std::thread;
+
+    use super::*;
+    use crate::actor::AffectedGuard;
+
+    /// 临时 vault：目录建在 `target/tmp` 下（AGENTS §6：不用 `%TEMP%`）。
+    ///
+    /// lib 单元测试拿不到 `CARGO_TARGET_TMPDIR`（只有集成测试有），所以从
+    /// `CARGO_MANIFEST_DIR` 拼出同一个位置。
+    fn fresh_vault(prefix: &str) -> (tempfile::TempDir, Vault) {
+        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
+            .join("../../target/tmp")
+            .join(format!("remote-known-{prefix}"));
+        std::fs::create_dir_all(&root).expect("创建 target/tmp 下的测试根目录");
+        let dir = tempfile::Builder::new()
+            .prefix("vault-")
+            .tempdir_in(&root)
+            .expect("创建临时目录");
+        let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
+        (dir, vault)
+    }
+
+    fn vv(peer: u64, counter: i32) -> VersionVector {
+        let mut vv = VersionVector::new();
+        vv.insert(peer, counter);
+        vv
+    }
+
+    fn read(vault: &Vault, doc_id: &str) -> Option<VersionVector> {
+        load(vault, doc_id).expect("读取远端已知 VV")
+    }
+
+    fn exec_cas(
+        vault: &Vault,
+        doc_id: &str,
+        expected: Option<&VersionVector>,
+        merged: &VersionVector,
+    ) -> Result<(), StorageError> {
+        vault
+            .exec_transaction(
+                &[cas_statement(doc_id, expected, merged)],
+                &[AffectedGuard::exactly(0, 1)],
+            )
+            .map(|_| ())
+    }
+
+    /// 值 CAS 的三种状态：不存在时插入 → 陈旧期望被拒绝且不缩小 → 读到新值后并集成功。
+    #[test]
+    fn cas_statement_rejects_stale_expected_and_never_shrinks() {
+        let (_dir, vault) = fresh_vault("stale");
+        let doc_id = "doc-cas";
+        let a = vv(0xA1, 2);
+        let b = vv(0xB2, 3);
+
+        // 读时不存在：直接插入。
+        exec_cas(&vault, doc_id, None, &a).expect("首次插入");
+        assert_eq!(read(&vault, doc_id), Some(a.clone()));
+
+        // 另一个写者仍以为「不存在」，拿着旧的 None 来写 B：必须被 CAS 拒绝。
+        let error = exec_cas(&vault, doc_id, None, &b).expect_err("陈旧期望必须失败");
+        assert!(
+            matches!(error, StorageError::GuardFailed { .. }),
+            "CAS 不命中应当是 GuardFailed（调用方映射成明确冲突），实际 {error:?}"
+        );
+        assert_eq!(
+            read(&vault, doc_id),
+            Some(a.clone()),
+            "CAS 失败绝不能把已有的 A 覆盖成只有 B"
+        );
+
+        // 重读后按并集重试：成功且值单调扩大。
+        let current = read(&vault, doc_id).expect("已存在");
+        let merged = union(&current, &b);
+        exec_cas(&vault, doc_id, Some(&current), &merged).expect("并集重试");
+
+        // 再拿一个更旧、且值更小的期望来写：必须继续被拒绝，值不得缩小。
+        let stale = vv(0xA1, 1);
+        let smaller = vv(0xA1, 1);
+        let error = exec_cas(&vault, doc_id, Some(&stale), &smaller).expect_err("更小值必须失败");
+        assert!(matches!(error, StorageError::GuardFailed { .. }));
+        assert_eq!(
+            read(&vault, doc_id),
+            Some(merged),
+            "失败后必须保持并集，绝不能写入更小的值"
+        );
+    }
+
+    /// 两路并发写者从**同一旧值**出发写不同 `added`：barrier 对齐后同时 CAS。
+    ///
+    /// 结果只能是：恰好一方首次成功、另一方拿到明确的 `GuardFailed`；失败方重读后按
+    /// 并集重试成功。最终值 = 两方新增的并集（不丢证据、不静默变小）。
+    #[test]
+    fn concurrent_writers_from_same_old_converge_to_union() {
+        let (_dir, vault) = fresh_vault("concurrent");
+        let doc_id = "doc-concurrent";
+        let base = vv(0x10, 1);
+        exec_cas(&vault, doc_id, None, &base).expect("初始值");
+
+        let barrier = Arc::new(Barrier::new(2));
+        let (tx, rx) = mpsc::channel();
+        let mut handles = Vec::new();
+        for peer in [0xA1_u64, 0xB2_u64] {
+            let vault = vault.clone();
+            let barrier = Arc::clone(&barrier);
+            let tx = tx.clone();
+            handles.push(thread::spawn(move || {
+                let added = vv(peer, 5);
+                // 两方都在 barrier 之前读到同一个旧值。
+                let old = load(&vault, doc_id).expect("读旧值").expect("已存在");
+                let merged = union(&old, &added);
+                let statement = cas_statement(doc_id, Some(&old), &merged);
+                barrier.wait();
+                let first = vault.exec_transaction(&[statement], &[AffectedGuard::exactly(0, 1)]);
+                // 失败方（陈旧旧值）重读后重试一次：必须成功并收敛到并集。
+                let (first_failed, retried) = match first {
+                    Ok(_) => (false, true),
+                    Err(StorageError::GuardFailed { .. }) => {
+                        let current = load(&vault, doc_id).expect("重读").expect("已存在");
+                        let merged = union(&current, &added);
+                        (
+                            true,
+                            exec_cas(&vault, doc_id, Some(&current), &merged).is_ok(),
+                        )
+                    }
+                    Err(other) => panic!("并发 CAS 只允许守卫失败，实际 {other:?}"),
+                };
+                tx.send((peer, first_failed, retried)).expect("回传结果");
+            }));
+        }
+        drop(tx);
+        let results: Vec<(u64, bool, bool)> = rx.iter().take(2).collect();
+        for handle in handles {
+            handle.join().expect("写者线程不应 panic");
+        }
+        assert_eq!(results.len(), 2, "两路写者都必须有明确结果");
+        assert_eq!(
+            results.iter().filter(|(_, failed, _)| *failed).count(),
+            1,
+            "同一旧值下只允许一方首次成功，另一方必须拿到明确的 CAS 冲突"
+        );
+        for (peer, _, retried) in &results {
+            assert!(*retried, "peer {peer:#x} 重读重试后必须成功");
+        }
+
+        let final_vv = read(&vault, doc_id).expect("最终值");
+        let expected = union(&union(&base, &vv(0xA1, 5)), &vv(0xB2, 5));
+        assert_eq!(final_vv, expected, "最终值必须包含两路 added 的并集");
+        assert_eq!(final_vv.get(&0x10).copied(), Some(1), "旧前缀不得丢失");
+    }
 }
diff --git a/crates/storage/src/session/flush.rs b/crates/storage/src/session/flush.rs
index be36373..f97e7a4 100644
--- a/crates/storage/src/session/flush.rs
+++ b/crates/storage/src/session/flush.rs
@@ -5,7 +5,14 @@
 //! - `flush()` 只投递一条命令就返回票据，**调用线程绝不碰 SQLite**；
 //! - 后台线程每次取 journal 的**连续前缀**，合并成一次 A 事务写 durable；
 //! - 写成功后先裁剪前缀、推进 durable 水位，再把 [`SessionEvent::Durable`] 推进事件队列；
-//! - 写失败**不裁剪、不 ACK**：同一前缀下次重试，因此「已 ACK 的部分」不会丢。
+//! - 写失败**不裁剪、不 ACK**：同一前缀下次重试，因此「已 ACK 的部分」不会丢；
+//! - **head 过期**（[`StorageError::HeadConflict`]）：同一命令内**至多**自动
+//!   `load_head` + 重建 sink 一次，并用新 CAS 基准重试**同一前缀**一次。只有新 head 的
+//!   ops 全部落在未 ACK 前缀最后一条的 `end_vv` 里（即外部推进写的是本会话 live 已知的
+//!   op）才允许重试——否则写入的 `head_vv`/`frontier` 会缺掉外部 ops，下一次
+//!   `load_body_doc` 的完整性校验直接 `Corrupt`。不满足或重建后仍失败 → **不可恢复**：
+//!   `last_write_error` 明确提示「需要重开文档」，journal 原样保留，后续 flush 立即失败、
+//!   不再碰数据库（有界重试，不无限写）。
 
 use std::collections::VecDeque;
 use std::sync::mpsc::{channel, Receiver, Sender};
@@ -14,7 +21,7 @@ use std::thread::{self, JoinHandle};
 use std::time::Duration;
 
 use crate::journal::{DurableSink, JournalEntry, JournalOrigin, PendingJournal, VaultJournalSink};
-use crate::store::HeadState;
+use crate::store::{load_head, HeadState};
 use crate::sync_bridge::SyncContext;
 use crate::{StorageError, Vault};
 
@@ -30,7 +37,7 @@ pub(crate) struct GateProgress {
     pub(crate) durable_through: LiveRev,
     /// 已 durable 的 head seq（外部提交的 CAS 基准）。
     pub(crate) durable_head_seq: u64,
-    /// 最近一次前缀写失败原因（成功写入后清空）。
+    /// 最近一次前缀写失败原因（成功写入后清空；不可恢复时保留「需要重开文档」提示）。
     pub(crate) last_write_error: Option<String>,
 }
 
@@ -73,6 +80,13 @@ pub(crate) struct PendingState {
     ///
     /// 外部提交闸门据此把「还在 Flushing」明确升级成 `Failed`，不静默等一个永远不来的 ACK。
     pub(crate) last_write_error: Option<String>,
+    /// 不可恢复的 flush 失败（head 过期且无法安全自动重建）。
+    ///
+    /// 一旦置位就**不再自动重试**：后续 flush 命令立即失败、不再碰数据库；journal 原样
+    /// 保留（未 ACK 的数据不丢也不会被静默丢弃），只有关闭并重开文档才能恢复。
+    /// 外部提交闸门通过 [`GateProgress::last_write_error`] 把它升级成 `Failed`，UI 可据此
+    /// 提示重开文档。
+    pub(crate) unrecoverable: Option<String>,
 }
 
 /// journal 句柄：会话侧投递命令，后台线程负责写。
@@ -109,6 +123,7 @@ impl JournalHandle {
             durable_batch: None,
             durable_through: LiveRev(0),
             last_write_error: None,
+            unrecoverable: None,
         }));
         let events: Arc<Mutex<VecDeque<SessionEvent>>> = Arc::new(Mutex::new(VecDeque::new()));
         let (sender, receiver) = channel::<FlushCommand>();
@@ -305,10 +320,17 @@ fn flusher_main(
         }
 
         let mut emitted_max: Option<LiveRev> = None;
+        // P1-3：head 过期时同一命令**最多**自动重建 1 次 sink（`load_head` → 新基准 →
+        // 重试同一前缀一次）；用尽后进入不可恢复状态，不再自动重试。
+        let mut auto_rebuilt = false;
         loop {
             // 1) 取连续前缀（持锁只做内存操作，绝不在锁内写盘）
             let (entries, through, count, head_seq) = {
                 let mut guard = lock(&state);
+                // 不可恢复：后续 flush 立即失败、绝不再碰数据库（journal 原样保留）。
+                if guard.unrecoverable.is_some() {
+                    break;
+                }
                 let pending = guard.journal.pending_len();
                 if pending == 0 {
                     break;
@@ -331,12 +353,47 @@ fn flusher_main(
             }
             let written = match sink.write_prefix(&entries) {
                 Ok(seq) => seq,
-                // 不裁剪、不 ACK：同一前缀留着下次重试，已 ACK 部分不受影响。
+                // 普通写失败（磁盘/SQLite 等）：不裁剪、不 ACK，同一前缀下次重试；
                 // 同时记录失败原因：外部提交闸门据此把 "Flushing" 明确升级为 Failed。
-                Err(error) => {
+                Err(error) if !matches!(&error, StorageError::HeadConflict { .. }) => {
                     lock(&state).last_write_error = Some(error.to_string());
                     break;
                 }
+                // head 已被外部推进：同一份字节按旧 CAS 基准重试永远不会成功。
+                // 先 `load_head` 拿真实基准、有界重建 sink，再重试**同一前缀**一次。
+                Err(error) => {
+                    if auto_rebuilt {
+                        let message = reopen_message(
+                            &doc_id,
+                            lock(&state).journal.pending_len(),
+                            format!(
+                                "自动重建后再次 head 冲突（{error}），本命令的 1 次自动重建已用尽"
+                            ),
+                        );
+                        let mut guard = lock(&state);
+                        guard.last_write_error = Some(message.clone());
+                        guard.unrecoverable = Some(message);
+                        break;
+                    }
+                    auto_rebuilt = true;
+                    let pending = lock(&state).journal.pending_len();
+                    match retry_prefix_after_rebuild(
+                        &vault, &doc_id, &entries, pending, head_seq, sync,
+                    ) {
+                        RebuildOutcome::Written(seq) => seq,
+                        // 普通写失败：保持既有「同一前缀下次重试」契约，不升级成不可恢复。
+                        RebuildOutcome::Retryable(message) => {
+                            lock(&state).last_write_error = Some(message);
+                            break;
+                        }
+                        RebuildOutcome::Unrecoverable(message) => {
+                            let mut guard = lock(&state);
+                            guard.last_write_error = Some(message.clone());
+                            guard.unrecoverable = Some(message);
+                            break;
+                        }
+                    }
+                }
             };
 
             // 3) durable 之后才裁剪并推进水位
@@ -407,3 +464,132 @@ fn flusher_main(
         }
     }
 }
+
+/// head 冲突后自动重建 sink 的结果（P1-3 生产接线）。
+#[derive(Debug)]
+enum RebuildOutcome {
+    /// 重建 sink 后用同一前缀重试成功；值为 durable 后的 `head_seq`。
+    Written(u64),
+    /// 不可恢复：必须关闭并重开文档（不再自动重试，也不再写库）。
+    Unrecoverable(String),
+    /// 重建后重试遇到普通写失败（磁盘/SQLite 等）：journal 保留，下一次 flush 再试。
+    Retryable(String),
+}
+
+/// `HeadConflict` 后的有界自动重建：`load_head` → 新 CAS 基准 sink → 重试同一前缀一次。
+///
+/// **为什么不是无条件重试**：journal 条目的 `PendingUpdate` 是**追加那一刻**按旧 head
+/// 导出的字节，写入 durable 的 `loro_docs.head_vv` / `head_frontier` 取自前缀最后一条的
+/// `end_*` 快照。只有新 head 的 ops 全部包含在该快照的 VV 里（即外部推进写的是本会话
+/// live 已知的 op）时，原样重试才与本会话 live 一致；否则 head 快照会缺掉外部 ops，
+/// 下一次 `load_body_doc` 的完整性校验直接 `Corrupt`。后者 fail-closed：保留 journal、
+/// 提示重开文档，绝不硬写。
+///
+/// `durable_base` 是本次写入用的 CAS 基准；新 head 必须严格大于它，回退的基准一律拒绝。
+fn retry_prefix_after_rebuild(
+    vault: &Vault,
+    doc_id: &str,
+    entries: &[JournalEntry],
+    pending: usize,
+    durable_base: u64,
+    sync: Option<SyncContext>,
+) -> RebuildOutcome {
+    let head = match load_head(vault, doc_id) {
+        Ok(head) => head,
+        Err(error) => {
+            return RebuildOutcome::Unrecoverable(reopen_message(
+                doc_id,
+                pending,
+                format!("读不到当前 head（{error}），无法重建 sink"),
+            ));
+        }
+    };
+    if head.head_seq <= durable_base {
+        return RebuildOutcome::Unrecoverable(reopen_message(
+            doc_id,
+            pending,
+            format!(
+                "库中 head 没有前进（CAS 基准 {durable_base}，读到 {}）；拒绝用回退的基准重写",
+                head.head_seq
+            ),
+        ));
+    }
+    // 空前缀在 flusher 里不可达；防御性 fail-closed，不 panic。
+    let Some(last) = entries.last() else {
+        return RebuildOutcome::Unrecoverable(reopen_message(
+            doc_id,
+            pending,
+            "未 ACK 前缀为空，无法校验 base".to_owned(),
+        ));
+    };
+    let prefix_vv = match loro::VersionVector::decode(&last.update.end_vv) {
+        Ok(vv) => vv,
+        Err(error) => {
+            return RebuildOutcome::Unrecoverable(reopen_message(
+                doc_id,
+                pending,
+                format!("未 ACK 前缀的 end_vv 解码失败（{error}）"),
+            ));
+        }
+    };
+    let head_vv = match head.version_vector() {
+        Ok(vv) => vv,
+        Err(error) => {
+            return RebuildOutcome::Unrecoverable(reopen_message(
+                doc_id,
+                pending,
+                format!("新 head 的 VV 解码失败（{error}）"),
+            ));
+        }
+    };
+    if head_vv.is_empty() {
+        return RebuildOutcome::Unrecoverable(reopen_message(
+            doc_id,
+            pending,
+            format!(
+                "新 head（head_seq {}）的 VV 为空，无法证明未 ACK 前缀与新 base 兼容",
+                head.head_seq
+            ),
+        ));
+    }
+    if !prefix_vv.includes_vv(&head_vv) {
+        return RebuildOutcome::Unrecoverable(reopen_message(
+            doc_id,
+            pending,
+            format!(
+                "库中 head 已推进到 {} 且包含本会话 live 文档未知的并发 ops；\
+                 原样重试会写坏 head_vv/frontier，已拒绝自动重建",
+                head.head_seq
+            ),
+        ));
+    }
+
+    // 同一个后台 flusher 线程内重建 sink（复用既有 Vault 写连接，不新建 journal/连接）。
+    let mut sink = VaultJournalSink::new(vault.clone(), doc_id.to_owned(), head.head_seq);
+    if let Some(sync) = sync {
+        sink = sink.with_sync_context(sync);
+    }
+    match sink.write_prefix(entries) {
+        Ok(seq) => RebuildOutcome::Written(seq),
+        Err(StorageError::HeadConflict {
+            expected, actual, ..
+        }) => RebuildOutcome::Unrecoverable(reopen_message(
+            doc_id,
+            pending,
+            format!("自动重建后再发生 head 冲突（基准 {expected}，实际 {actual}）"),
+        )),
+        Err(error) => RebuildOutcome::Retryable(format!(
+            "{doc_id} 自动重建后重试前缀失败（journal 保留，等待下次 flush）：{error}"
+        )),
+    }
+}
+
+/// 统一的「需要重开文档」失败文案：闸门/UI 只看这一个字符串就必须知道该做什么。
+fn reopen_message(doc_id: &str, pending: usize, reason: String) -> String {
+    format!(
+        "需要重开文档：{doc_id} 的未 ACK journal 无法安全自动重建（{reason}）；\
+         {pending} 条未 ACK 更新保留在当前会话 journal 中（未写入、未静默丢弃）。\
+         请关闭并重开该文档，从 durable 前缀重新加载后再继续编辑；\
+         重开前未 durable 的本地改动不会自动落库"
+    )
+}
diff --git a/crates/storage/src/store.rs b/crates/storage/src/store.rs
index 3e04674..69e1fad 100644
--- a/crates/storage/src/store.rs
+++ b/crates/storage/src/store.rs
@@ -392,7 +392,15 @@ pub fn append_updates(
     expected_head_seq: u64,
     records: &[UpdateRecord],
 ) -> Result<HeadState, StorageError> {
-    append_updates_inner(vault, doc_id, expected_head_seq, records, &[], &[], &[])
+    append_updates_inner(
+        vault,
+        doc_id,
+        expected_head_seq,
+        records,
+        &[],
+        &[],
+        OriginMarks::AllLocal,
+    )
 }
 
 /// A 事务：追加**一条**组合 update，并把同一批 remote inbox 行标为 `integrated`。
@@ -434,25 +442,85 @@ pub fn append_updates_with_integrated_marks(
         &marks,
         // engine integrate 的这条组合 update 全部来自远端消息：其 `end_vv - 旧 head_vv`
         // 就是远端实际带来的 ops，记入「远端已知 VV」（tombstone 判定用）。
-        &[true],
+        OriginMarks::PerRecord(&[true]),
     )
 }
 
+/// 一次 A 事务里每条 update 的**来源标记**。
+///
+/// 这是 P2-8 修掉的类型陷阱：旧接口用 `&[bool]` 表示来源，**空切片 = 全部本地**；
+/// 而同一次调用的 `slots` 也是切片，**空切片 = 没有任何出站意图**。两个空切片含义
+/// 相反、类型相同，调用点很容易把「没有意图」写成「全部本地」或反之。这里把来源标记
+/// 收成枚举，让「空」不再是两种含义：
+///
+/// - [`OriginMarks::AllLocal`]：本批记录全部是本地写入，**不**扩大远端已知 VV；
+/// - [`OriginMarks::PerRecord`]：与 `records` **一一对应**（长度不匹配直接报
+///   [`StorageError::Corrupt`]），`true` = 该条来自远端消息，其
+///   `end_vv - 前一条 end_vv` 并入「远端已知 VV」。
+///
+/// 本地写入即使已经 ACK 也不会扩大远端已知 VV（tombstone 判定只允许更保守）。
+#[derive(Debug, Clone, Copy)]
+pub enum OriginMarks<'a> {
+    /// 全部记录都是本地写入。
+    AllLocal,
+    /// 逐条标记（必须与 records 等长）。
+    PerRecord(&'a [bool]),
+}
+
+impl<'a> OriginMarks<'a> {
+    /// 本批是否有远端条目。
+    fn any_remote(self) -> bool {
+        match self {
+            Self::AllLocal => false,
+            Self::PerRecord(marks) => marks.iter().any(|flag| *flag),
+        }
+    }
+
+    /// 第 `index` 条是否来自远端（`AllLocal` 恒为 false）。
+    fn is_remote(self, index: usize) -> bool {
+        match self {
+            Self::AllLocal => false,
+            Self::PerRecord(marks) => marks.get(index).copied().unwrap_or(false),
+        }
+    }
+
+    /// 校验来源标记与 records 的对应关系。
+    ///
+    /// `AllLocal` 不需要长度；`PerRecord` 必须与 records 等长——**空 records 配非空
+    /// marks、或 records 配空 marks 都会在这里被拒绝**，不会退化成「全部本地」。
+    fn validate(self, records_len: usize) -> Result<(), StorageError> {
+        if let Self::PerRecord(marks) = self {
+            if marks.len() != records_len {
+                return Err(StorageError::Corrupt {
+                    context: format!(
+                        "append_updates: records={records_len} \
+                         OriginMarks::PerRecord={} 必须一对一",
+                        marks.len()
+                    ),
+                });
+            }
+        }
+        Ok(())
+    }
+}
+
 /// A 事务内同时写出站同步意图（按位置一一对应；`None` = 该条不产生意图）。
 ///
 /// 与 [`append_updates_with_sync_intents`] 相比，这里允许"某些 update 没有意图"：
 /// 一次 flush 的前缀可能混本地编辑与 remote 导入，而 remote 导入**绝不能**产生
 /// 出站意图（ADR 0009）。仍然只有**一条** A 事务，失败即整笔回滚。
 ///
-/// `remote_marks` 与 records 一一对应（空切片 = 全部本地）：标为 remote 的记录会在
-/// 同一事务里把 `end_vv - 上一条 end_vv` 并入「远端已知 VV」，供 tombstone 判定。
+/// 参数语义（P2-8 钉死）：
+/// - `slots` 空 = 本批**没有任何**出站意图（不是"每条都要一条意图"）；`None` 位 = 该条
+///   不产生意图；
+/// - `origin` 见 [`OriginMarks`]：只有远端条目会扩大远端已知 VV，本地条目永远不扩大。
 pub(crate) fn append_updates_with_intent_slots(
     vault: &Vault,
     doc_id: &str,
     expected_head_seq: u64,
     records: &[UpdateRecord],
     slots: &[Option<crate::sync_bridge::SyncIntent>],
-    remote_marks: &[bool],
+    origin: OriginMarks<'_>,
 ) -> Result<HeadState, StorageError> {
     append_updates_inner(
         vault,
@@ -461,7 +529,7 @@ pub(crate) fn append_updates_with_intent_slots(
         records,
         slots,
         &[],
-        remote_marks,
+        origin,
     )
 }
 
@@ -475,7 +543,7 @@ fn append_updates_inner(
     records: &[UpdateRecord],
     slots: &[Option<crate::sync_bridge::SyncIntent>],
     integrated_marks: IntegratedMarks<'_>,
-    remote_marks: &[bool],
+    origin: OriginMarks<'_>,
 ) -> Result<HeadState, StorageError> {
     if records.is_empty() {
         if !slots.is_empty() {
@@ -488,6 +556,8 @@ fn append_updates_inner(
                 context: "空 update 批次不能带 inbox 标记".to_owned(),
             });
         }
+        // `PerRecord` 配空 records 时，非空标记同样算长度不匹配（不会退化成全部本地）。
+        origin.validate(0)?;
         return load_head(vault, doc_id);
     }
     if !slots.is_empty() && slots.len() != records.len() {
@@ -499,29 +569,28 @@ fn append_updates_inner(
             ),
         });
     }
-    if !remote_marks.is_empty() && remote_marks.len() != records.len() {
-        return Err(StorageError::Corrupt {
-            context: format!(
-                "append_updates: records={} remote_marks={} 必须一对一",
-                records.len(),
-                remote_marks.len()
-            ),
-        });
-    }
+    origin.validate(records.len())?;
 
     // 远端已知 VV 的增量：只有 remote 条目会扩大它（`after_vv - running_vv`）。
-    let remote_meta = if remote_marks.iter().any(|flag| *flag) {
+    //
+    // 这里只算「本批新增了哪些远端 ops」；旧值读取与 CAS 语句在构造语句阶段生成，
+    // 重试时按最新旧值重算并集，但 `added` 本身不变（head 基准没变）。
+    let remote_added = if origin.any_remote() {
         let mut running = load_head_vv_at(vault, doc_id, expected_head_seq)?;
         let mut known = VersionVector::new();
         for (index, record) in records.iter().enumerate() {
             let after = VersionVector::decode(&record.update.end_vv).map_err(StorageError::from)?;
-            if remote_marks.get(index).copied().unwrap_or(false) {
+            if origin.is_remote(index) {
                 let added = crate::remote_known::added_since(&after, &running);
                 known = crate::remote_known::union(&known, &added);
             }
             running = after;
         }
-        crate::remote_known::merge_statement(vault, doc_id, &known)?
+        if known.is_empty() {
+            None
+        } else {
+            Some(known)
+        }
     } else {
         None
     };
@@ -603,11 +672,22 @@ fn append_updates_inner(
     }
 
     // 远端已知 VV 的 meta 写入（与 update/head 同一 A 事务；失败一起回滚）。
-    let remote_guard = remote_meta.as_ref().map(|statement| {
-        let index = statements.len();
-        statements.push(statement.clone());
-        AffectedGuard::exactly(index, 1)
-    });
+    //
+    // **值 CAS**：语句里带读到的旧值，只有库中值仍是旧值才会更新（`WHERE meta.value=?old`，
+    // 旧值不存在时用 `INSERT ... WHERE NOT EXISTS`）。这样两个写者（普通 pull 的 A 事务
+    // 与 baseline apply 批次）即使都拿着同一份旧并集，后到的那个也不会把先到的扩大值
+    // 覆盖回更小的并集；CAS 不命中 → 0 行 → 守卫失败 → 整笔回滚后重读重试。
+    let remote_slot = match &remote_added {
+        Some(added) => {
+            let index = statements.len();
+            statements.push(cas_statement_or_corrupt(
+                crate::remote_known::merge_statement(vault, doc_id, added)?,
+                doc_id,
+            )?);
+            Some(index)
+        }
+        None => None,
+    };
 
     // 同事务标记 inbox 行：每条都要求恰好影响 1 行（不存在 / 已 integrated /
     // 已 quarantine 都会让整笔 A 事务回滚）。标记语句放在最后，避免它们的守卫先于
@@ -625,7 +705,9 @@ fn append_updates_inner(
     }
     let mut guards = Vec::with_capacity(2 + integrated_marks.len());
     guards.push(AffectedGuard::exactly(0, 1));
-    guards.extend(remote_guard);
+    if let Some(index) = remote_slot {
+        guards.push(AffectedGuard::exactly(index, 1));
+    }
     guards.extend(
         integrated_marks
             .iter()
@@ -633,38 +715,80 @@ fn append_updates_inner(
             .map(|(index, _)| AffectedGuard::exactly(marks_start + index, 1)),
     );
 
-    match vault.exec_transaction(&statements, &guards) {
-        Ok(_) => Ok(HeadState {
-            doc_id: doc_id.to_owned(),
-            head_seq,
-            head_frontier: last.end_frontier.clone(),
-            head_vv: last.end_vv.clone(),
-        }),
-        // 第 0 条守卫 = head CAS。
-        Err(StorageError::GuardFailed { statement: 0, .. }) => {
-            let actual = load_head(vault, doc_id)
-                .map(|head| head.head_seq)
-                .unwrap_or(0);
-            Err(StorageError::HeadConflict {
-                doc_id: doc_id.to_owned(),
-                expected: expected_head_seq,
-                actual,
-            })
-        }
-        // 标记失败：update 与所有标记一起回滚；调用方必须保持 inbox verified 原样。
-        Err(StorageError::GuardFailed { statement, .. }) if statement >= marks_start => {
-            Err(StorageError::SyncConflict {
-                context: format!(
-                    "inbox 标记 integrated 失败（第 {} 个标记行不存在或当前不是 verified）；\
-                     整笔 A 事务（update+标记）已回滚",
-                    statement - marks_start
-                ),
-            })
+    // 远端已知 VV 的 CAS 失败是**可重试**的：并发写者只是先扩大了一步，重读旧值后
+    // 把两边并集再写一次即可。重试期间 head CAS 仍在 index 0：若真的有人推进了 head，
+    // 依旧按 HeadConflict 返回，不会把「基准过期」误当成 meta 冲突。
+    const REMOTE_KNOWN_CAS_ATTEMPTS: usize = 4;
+    let mut attempt = 0usize;
+    loop {
+        attempt += 1;
+        match vault.exec_transaction(&statements, &guards) {
+            Ok(_) => {
+                return Ok(HeadState {
+                    doc_id: doc_id.to_owned(),
+                    head_seq,
+                    head_frontier: last.end_frontier.clone(),
+                    head_vv: last.end_vv.clone(),
+                });
+            }
+            // 第 0 条守卫 = head CAS。
+            Err(StorageError::GuardFailed { statement: 0, .. }) => {
+                let actual = load_head(vault, doc_id)
+                    .map(|head| head.head_seq)
+                    .unwrap_or(0);
+                return Err(StorageError::HeadConflict {
+                    doc_id: doc_id.to_owned(),
+                    expected: expected_head_seq,
+                    actual,
+                });
+            }
+            // 远端已知 VV 的值 CAS 不命中：指定下标只有它这一条守卫。
+            Err(StorageError::GuardFailed { statement, .. }) if remote_slot == Some(statement) => {
+                let Some(added) = remote_added.as_ref() else {
+                    return Err(StorageError::Corrupt {
+                        context: format!("文档 {doc_id} 的远端已知 VV CAS 守卫缺少重算依据"),
+                    });
+                };
+                if attempt >= REMOTE_KNOWN_CAS_ATTEMPTS {
+                    return Err(StorageError::SyncConflict {
+                        context: format!(
+                            "文档 {doc_id} 的远端已知 VV 值 CAS 连续 {attempt} 次被并发写者改写；\
+                             整笔 A 事务（update+head+meta）已回滚，可重试"
+                        ),
+                    });
+                }
+                let slot = remote_slot.expect("守卫命中说明 remote_slot 存在");
+                statements[slot] = cas_statement_or_corrupt(
+                    crate::remote_known::merge_statement(vault, doc_id, added)?,
+                    doc_id,
+                )?;
+            }
+            // 标记失败：update 与所有标记一起回滚；调用方必须保持 inbox verified 原样。
+            Err(StorageError::GuardFailed { statement, .. }) if statement >= marks_start => {
+                return Err(StorageError::SyncConflict {
+                    context: format!(
+                        "inbox 标记 integrated 失败（第 {} 个标记行不存在或当前不是 verified）；\
+                         整笔 A 事务（update+标记）已回滚",
+                        statement - marks_start
+                    ),
+                });
+            }
+            Err(error) => return Err(error),
         }
-        Err(error) => Err(error),
     }
 }
 
+/// [`crate::remote_known::merge_statement`] 在 `remote_added` 非空时必然返回语句；
+/// 这里把它转成带文档上下文的损坏错误，避免业务路径 `unwrap`。
+fn cas_statement_or_corrupt(
+    statement: Option<Statement>,
+    doc_id: &str,
+) -> Result<Statement, StorageError> {
+    statement.ok_or_else(|| StorageError::Corrupt {
+        context: format!("文档 {doc_id} 的远端已知 VV 增量非空，但没有生成 CAS 语句"),
+    })
+}
+
 /// 笔记当前的物化水位（供 B 事务与测试读取）。
 #[derive(Debug, Clone, PartialEq, Eq)]
 pub struct BodyState {
@@ -743,3 +867,242 @@ fn optional_blob(row: &[Value], index: usize) -> Result<Option<Vec<u8>>, Storage
         }),
     }
 }
+
+#[cfg(test)]
+mod tests {
+    use std::path::Path;
+
+    use loro::VersionVector;
+
+    use super::*;
+    use crate::body_doc::BodyDoc;
+    use crate::remote_known;
+
+    /// 临时 vault：目录建在 `target/tmp` 下（AGENTS §6：不用 `%TEMP%`）。
+    ///
+    /// lib 单元测试拿不到 `CARGO_TARGET_TMPDIR`（只有集成测试有），所以从
+    /// `CARGO_MANIFEST_DIR` 拼出同一个位置。
+    fn fresh_vault(prefix: &str) -> (tempfile::TempDir, Vault) {
+        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
+            .join("../../target/tmp")
+            .join(format!("store-origin-{prefix}"));
+        std::fs::create_dir_all(&root).expect("创建 target/tmp 下的测试根目录");
+        let dir = tempfile::Builder::new()
+            .prefix("vault-")
+            .tempdir_in(&root)
+            .expect("创建临时目录");
+        let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
+        (dir, vault)
+    }
+
+    fn scalar_i64(vault: &Vault, sql: &str) -> i64 {
+        let rows = vault.query(sql, &[]).expect("查询");
+        match rows.first().and_then(|row| row.first()) {
+            Some(Value::Integer(value)) => *value,
+            other => panic!("期望整数结果，实际 {other:?}"),
+        }
+    }
+
+    fn local_update(doc: &BodyDoc, base: &VersionVector) -> PendingUpdate {
+        doc.pending_update(base).expect("导出增量")
+    }
+
+    /// `OriginMarks::AllLocal`（显式类型）不扩大远端已知 VV，也不产生 outbox。
+    #[test]
+    fn all_local_origin_does_not_extend_remote_known() {
+        let (_dir, vault) = fresh_vault("all-local");
+        create_body_doc(&vault, "doc-local").expect("loro_docs 行");
+        let doc = BodyDoc::create("doc-local", 0xA1).expect("创建 BodyDoc");
+        doc.insert_scalars(0, "local").expect("本地编辑");
+        let update = local_update(&doc, &VersionVector::default());
+
+        let head = append_updates_inner(
+            &vault,
+            "doc-local",
+            0,
+            &[UpdateRecord::new("b0", update)],
+            &[],
+            &[],
+            OriginMarks::AllLocal,
+        )
+        .expect("A 事务");
+
+        assert_eq!(head.head_seq, 1);
+        assert!(
+            remote_known::load(&vault, "doc-local")
+                .expect("读远端已知")
+                .is_none(),
+            "全部本地的批次绝不能创建/扩大远端已知 VV"
+        );
+        assert_eq!(
+            scalar_i64(&vault, "SELECT count(*) FROM sync_outbox"),
+            0,
+            "slots 空 = 本批没有任何出站意图"
+        );
+    }
+
+    /// `PerRecord` 混排：只有 remote 条目的 `after - running` 记入远端已知 VV。
+    #[test]
+    fn per_record_origin_only_extends_for_remote_records() {
+        let (_dir, vault) = fresh_vault("mixed");
+        create_body_doc(&vault, "doc-mixed").expect("loro_docs 行");
+
+        let doc = BodyDoc::create("doc-mixed", 0xA1).expect("创建 BodyDoc");
+        doc.insert_scalars(0, "local").expect("本地编辑");
+        let local = local_update(&doc, &VersionVector::default());
+        let remote_doc = BodyDoc::create("doc-mixed", 0xB2).expect("创建 BodyDoc");
+        remote_doc.insert_scalars(0, "remote").expect("远端编辑");
+        doc.import(&remote_doc.export_all_updates().expect("远端 update"))
+            .expect("本地副本导入远端");
+        let local_end = VersionVector::decode(&local.end_vv).expect("本地 VV");
+        let remote = local_update(&doc, &local_end);
+        let remote_end = VersionVector::decode(&remote.end_vv).expect("远端 VV");
+        let expected = remote_known::added_since(&remote_end, &local_end);
+
+        let head = append_updates_inner(
+            &vault,
+            "doc-mixed",
+            0,
+            &[
+                UpdateRecord::new("b0", local),
+                UpdateRecord::new("b1", remote),
+            ],
+            &[],
+            &[],
+            OriginMarks::PerRecord(&[false, true]),
+        )
+        .expect("A 事务");
+
+        assert_eq!(head.head_seq, 2, "两条记录都必须落库");
+        let known = remote_known::load(&vault, "doc-mixed")
+            .expect("读远端已知")
+            .expect("有 remote 条目就必须写 meta");
+        assert_eq!(
+            known, expected,
+            "远端条目带来的 ops 必须记入，且只记入远端那一小段"
+        );
+        assert_eq!(
+            known.get(&0xA1).copied().unwrap_or(0),
+            0,
+            "本地条目（哪怕同批先落库）绝不能记入远端已知 VV"
+        );
+        assert_eq!(
+            scalar_i64(&vault, "SELECT count(*) FROM sync_outbox"),
+            0,
+            "没有传 slots 就不得写出站意图"
+        );
+    }
+
+    /// 空 `PerRecord` 不再表示「全部本地」：长度不匹配直接拒绝，且不写任何行。
+    #[test]
+    fn per_record_origin_length_mismatch_is_rejected() {
+        let (_dir, vault) = fresh_vault("mismatch");
+        create_body_doc(&vault, "doc-mismatch").expect("loro_docs 行");
+        let doc = BodyDoc::create("doc-mismatch", 0xA1).expect("创建 BodyDoc");
+        doc.insert_scalars(0, "x").expect("编辑");
+        let update = local_update(&doc, &VersionVector::default());
+        let record = UpdateRecord::new("b0", update);
+
+        let error = append_updates_inner(
+            &vault,
+            "doc-mismatch",
+            0,
+            std::slice::from_ref(&record),
+            &[],
+            &[],
+            OriginMarks::PerRecord(&[]),
+        )
+        .expect_err("空 PerRecord 配非空 records 必须报错");
+        assert!(
+            matches!(error, StorageError::Corrupt { .. }),
+            "必须是显式损坏/参数错误，实际 {error:?}"
+        );
+        assert_eq!(
+            scalar_i64(&vault, "SELECT count(*) FROM loro_updates"),
+            0,
+            "参数错误不得留下半写"
+        );
+
+        let error = append_updates_inner(
+            &vault,
+            "doc-mismatch",
+            0,
+            &[record],
+            &[],
+            &[],
+            OriginMarks::PerRecord(&[true, false]),
+        )
+        .expect_err("标记多于 records 也必须报错");
+        assert!(matches!(error, StorageError::Corrupt { .. }));
+    }
+
+    /// 值 CAS 不命中（并发写者改了 meta）→ 有界重试后返回明确 SyncConflict，
+    /// 整笔 A 事务回滚：head/update/outbox/meta 都不留半写，也绝不写入更小值。
+    ///
+    /// 注入方式：`loro_updates` 的 BEFORE INSERT 触发器在**同一事务里**把 meta 改成
+    /// 别的值（模拟「读旧值之后、CAS 之前的并发提交」）；每次重试都会再次命中，
+    /// 因此有界重试必然耗尽。
+    #[test]
+    fn stale_remote_known_cas_fails_closed_and_rolls_back() {
+        let (_dir, vault) = fresh_vault("cas");
+        create_body_doc(&vault, "doc-cas").expect("loro_docs 行");
+        let remote_doc = BodyDoc::create("doc-cas", 0xB2).expect("创建 BodyDoc");
+        remote_doc.insert_scalars(0, "remote").expect("远端编辑");
+        let update = local_update(&remote_doc, &VersionVector::default());
+        vault
+            .exec(
+                "CREATE TRIGGER remote_known_race BEFORE INSERT ON loro_updates BEGIN \
+                   INSERT OR REPLACE INTO meta(key,value) \
+                   VALUES('sync.remote_known_vv.doc-cas', X'DEADBEEF'); \
+                 END;",
+                &[],
+            )
+            .expect("安装竞态触发器");
+
+        let error = append_updates_inner(
+            &vault,
+            "doc-cas",
+            0,
+            &[UpdateRecord::new("b0", update)],
+            &[],
+            &[],
+            OriginMarks::PerRecord(&[true]),
+        )
+        .expect_err("CAS 必须失败");
+
+        match error {
+            StorageError::SyncConflict { context } => {
+                assert!(
+                    context.contains("远端已知 VV"),
+                    "错误必须点明远端已知 VV CAS：{context}"
+                );
+                assert!(context.contains("可重试"), "错误必须说明可重试：{context}");
+            }
+            other => panic!("期望 SyncConflict，实际 {other:?}"),
+        }
+        assert_eq!(load_head(&vault, "doc-cas").expect("head").head_seq, 0);
+        assert_eq!(
+            scalar_i64(&vault, "SELECT count(*) FROM loro_updates"),
+            0,
+            "整笔回滚：update 不得落库"
+        );
+        assert_eq!(
+            scalar_i64(&vault, "SELECT count(*) FROM projection_outbox"),
+            0
+        );
+        assert_eq!(
+            scalar_i64(
+                &vault,
+                "SELECT count(*) FROM meta WHERE key='sync.remote_known_vv.doc-cas'"
+            ),
+            0,
+            "触发器写入随整笔回滚，失败后不残留"
+        );
+        assert!(
+            remote_known::load(&vault, "doc-cas")
+                .expect("读远端已知")
+                .is_none(),
+            "失败路径绝不能写入更小/错误的值"
+        );
+    }
+}
diff --git a/crates/storage/src/sync_bridge.rs b/crates/storage/src/sync_bridge.rs
index c9e2b44..a831dea 100644
--- a/crates/storage/src/sync_bridge.rs
+++ b/crates/storage/src/sync_bridge.rs
@@ -20,7 +20,7 @@ use crate::Vault;
 
 // 同步 `meta` 键与「连续前缀」的低层实现放在 `sync_meta`（直接跑在 `Connection` 上，
 // 这样批量 inbox 的插入与前缀推进能共用一个事务）；这里保持原有的公开路径。
-use crate::sync_meta::{skipped_received_key, ConsumedPrefix};
+use crate::sync_meta::{meta_bytes_of, skipped_received_key, ConsumedPrefix};
 pub use crate::sync_meta::{
     META_SYNC_CONFIRMED_HIGHWATER, META_SYNC_INTEGRATED_SEQ, META_SYNC_LOG_ID,
     META_SYNC_RECEIVED_SEQ, META_SYNC_SKIPPED_RECEIVED_PREFIX,
@@ -157,13 +157,16 @@ pub fn local_sync_intent(
 /// 一次 flush 的连续前缀可能**混**本地编辑与远端导入（`JournalOrigin`），而
 /// remote/import/adopt **绝不能**产生出站意图（否则会把刚收到的数据再推回 relay）。
 /// 这里仍是一条 A 事务、按位置一一对应，只是允许某些位置为空。
+///
+/// `slots` 空 = 本批没有**任何**出站意图；来源标记用 [`store::OriginMarks`]
+/// （`AllLocal` / 与 records 等长的 `PerRecord`），**不再用空切片**表示「全部本地」。
 pub(crate) fn append_updates_with_intent_slots(
     vault: &Vault,
     doc_id: &str,
     expected_head_seq: u64,
     records: &[store::UpdateRecord],
     slots: &[Option<SyncIntent>],
-    remote_marks: &[bool],
+    origin: store::OriginMarks<'_>,
 ) -> Result<store::HeadState, StorageError> {
     if !slots.is_empty() && slots.len() != records.len() {
         return Err(StorageError::Corrupt {
@@ -174,22 +177,13 @@ pub(crate) fn append_updates_with_intent_slots(
             ),
         });
     }
-    if !remote_marks.is_empty() && remote_marks.len() != records.len() {
-        return Err(StorageError::Corrupt {
-            context: format!(
-                "append_updates_with_intent_slots: records={} remote_marks={} 必须一对一",
-                records.len(),
-                remote_marks.len()
-            ),
-        });
-    }
     store::append_updates_with_intent_slots(
         vault,
         doc_id,
         expected_head_seq,
         records,
         slots,
-        remote_marks,
+        origin,
     )
 }
 
@@ -370,7 +364,14 @@ pub fn append_updates_with_sync_intents(
         });
     }
     let slots: Vec<Option<SyncIntent>> = intents.iter().cloned().map(Some).collect();
-    store::append_updates_with_intent_slots(vault, doc_id, expected_head_seq, records, &slots, &[])
+    store::append_updates_with_intent_slots(
+        vault,
+        doc_id,
+        expected_head_seq,
+        records,
+        &slots,
+        store::OriginMarks::AllLocal,
+    )
 }
 
 /// 单独记录一条出站意图（调用方已确认 update 落库）。
@@ -1089,15 +1090,31 @@ pub fn store_inbox_verified_batch(
 /// 读当前已验证、且落在 durable 连续接收前缀内的 inbox 行。
 ///
 /// 依赖完整性由 sync 层判断（本 crate 不解析 Loro VV）；这里的 "ready"
-/// 指 cryptographically verified + durable + `relay_seq <= received_seq`，
-/// 不在连续前缀内的行保持 durable，等前面的缺口补齐。排序为 `(log_id, relay_seq)`。
+/// 指 cryptographically verified + durable + **属于当前 `sync.log_id`** +
+/// `relay_seq <= received_seq`，不在连续前缀内的行保持 durable，等前面的缺口补齐。
+/// 排序为 `(log_id, relay_seq)`。
+///
+/// **incarnation 过滤**（ADR 0006 §5）：`sync_inbox` 的 `log_id` 是写入时那代 relay
+/// 日志的 incarnation。restore 迁移 `sync.log_id` 后，旧 incarnation 的行仍然 durable
+/// （不能在迁移时静默删数据），但绝不能进入 ready——`relay_seq` 只在同一 incarnation
+/// 内有序，跨代比较没有意义。过滤条件里的 `log_id` 由 [`read_current_log_id`] 读出后
+/// 作为 SQL 参数绑定，不做子查询以便调用方/测试都看到同一个显式值。
+///
+/// `sync.log_id` 不存在（新库尚未绑定 relay 日志代）时返回空：没有当前 incarnation
+/// 就没有可匹配的行；新库此时 `received` 也是 0，所以与旧实现（不按 log 过滤）的结果
+/// 一致，不会把其它 incarnation 的遗留行漏出来。`sync.log_id` 存在但不是 16B →
+/// [`StorageError::Corrupt`]（与 [`sync_cursor_state`] 同口径），不静默当作"没有 log"。
 pub fn take_inbox_ready(vault: &Vault, limit: usize) -> Result<Vec<SyncInboxRow>, StorageError> {
+    let Some(log_id) = read_current_log_id(vault)? else {
+        return Ok(Vec::new());
+    };
     let received = read_meta_u64(vault, META_SYNC_RECEIVED_SEQ)?.unwrap_or(0);
     let rows = vault.query(
         "SELECT log_id,relay_seq,object_id,epoch,state,verified_payload \
-         FROM sync_inbox WHERE state='verified' AND relay_seq<=?1 \
-         ORDER BY log_id,relay_seq LIMIT ?2",
+         FROM sync_inbox WHERE log_id=?1 AND state='verified' AND relay_seq<=?2 \
+         ORDER BY log_id,relay_seq LIMIT ?3",
         &[
+            Value::Blob(log_id.to_vec()),
             Value::Integer(i64_of(received)?),
             Value::Integer(i64::try_from(limit).unwrap_or(i64::MAX)),
         ],
@@ -1105,16 +1122,27 @@ pub fn take_inbox_ready(vault: &Vault, limit: usize) -> Result<Vec<SyncInboxRow>
     rows.iter().map(row_to_inbox).collect()
 }
 
-/// 统计「已验证、落在 durable 连续接收前缀内、尚未集成」的 inbox 行数。
+/// 统计「已验证、属于当前 incarnation、落在 durable 连续接收前缀内、尚未集成」的行数。
 ///
 /// 语义（ADR 0012 S2b）：这些行不是"丢失"——它们要么依赖未就绪（deferred），要么被
 /// 批次预算留在本轮之后。调用方用它上报"已接收·待应用 N"，并据此判断"还有没有
 /// 可继续的工作"，而不是循环刷最早的一批。
+///
+/// 与 [`take_inbox_ready`] 同口径：只计当前 `sync.log_id` 的行，旧 incarnation 的
+/// verified 行即使 `relay_seq <= received` 也不计。`sync.log_id` 不存在返回 0
+/// （没有当前 incarnation 就没有 ready 行）；存在但长度非法 → [`StorageError::Corrupt`]。
 pub fn count_inbox_ready(vault: &Vault) -> Result<u64, StorageError> {
+    let Some(log_id) = read_current_log_id(vault)? else {
+        return Ok(0);
+    };
     let received = read_meta_u64(vault, META_SYNC_RECEIVED_SEQ)?.unwrap_or(0);
     let rows = vault.query(
-        "SELECT COUNT(*) FROM sync_inbox WHERE state='verified' AND relay_seq<=?1",
-        &[Value::Integer(i64_of(received)?)],
+        "SELECT COUNT(*) FROM sync_inbox \
+         WHERE log_id=?1 AND state='verified' AND relay_seq<=?2",
+        &[
+            Value::Blob(log_id.to_vec()),
+            Value::Integer(i64_of(received)?),
+        ],
     )?;
     rows.first()
         .map(|row| u64_of(row, 0, "sync_inbox.count"))
@@ -1125,6 +1153,8 @@ pub fn count_inbox_ready(vault: &Vault) -> Result<u64, StorageError> {
 }
 
 /// 带调用方依赖判断的 ready 选择器；不满足的项保持 durable verified。
+///
+/// 底层走 [`take_inbox_ready`]，因此同样只选择当前 `sync.log_id` 的行。
 pub fn take_inbox_ready_where(
     vault: &Vault,
     limit: usize,
@@ -1270,13 +1300,22 @@ pub fn sync_cursor_state(vault: &Vault) -> Result<SyncCursorState, StorageError>
     Ok(SyncCursorState {
         received_seq: read_meta_u64(vault, META_SYNC_RECEIVED_SEQ)?.unwrap_or(0),
         integrated_seq: read_meta_u64(vault, META_SYNC_INTEGRATED_SEQ)?.unwrap_or(0),
-        log_id: read_meta(vault, META_SYNC_LOG_ID)?
-            .map(|bytes| array16_bytes(bytes, "sync.log_id"))
-            .transpose()?,
+        log_id: read_current_log_id(vault)?,
         confirmed_highwater: read_meta_u64(vault, META_SYNC_CONFIRMED_HIGHWATER)?,
     })
 }
 
+/// 读当前 relay 日志 incarnation（`sync.log_id`，BLOB 16B）。
+///
+/// - 不存在（新库/尚未绑定）：`Ok(None)`，调用方按"没有当前 incarnation"处理
+///   （ready 视为空，绝不回退成"不过滤 log"）；
+/// - 存在但不是 16B：[`StorageError::Corrupt`]，与 [`sync_cursor_state`] 的既有口径一致。
+fn read_current_log_id(vault: &Vault) -> Result<Option<[u8; 16]>, StorageError> {
+    read_meta(vault, META_SYNC_LOG_ID)?
+        .map(|bytes| array16_bytes(bytes, "sync.log_id"))
+        .transpose()
+}
+
 /// 写入 `sync.log_id`；已有不同值则拒绝（restore 必须显式迁移）。
 pub fn set_sync_log_id(vault: &Vault, log_id: &[u8; 16]) -> Result<(), StorageError> {
     ensure_log_id(vault, log_id)
@@ -1333,15 +1372,21 @@ pub fn mark_skipped_event_received(
     advance_received_prefix(vault)
 }
 
+/// 读一条 meta（同步桥经 Vault 命令的读取路径）。
+///
+/// 类型判定与 [`Vault::get_meta`] / `sync_meta::read_meta_on` 共用同一个 helper
+/// [`meta_bytes_of`]：**只认 BLOB**，其它类型是 [`StorageError::Corrupt`]。
+/// 这里以前把 TEXT 当"缺失"，会让 `sync.log_id` / `sync.repack_required` /
+/// `sync.received_seq` 这些持久屏障与游标被静默绕过。
 fn read_meta(vault: &Vault, key: &str) -> Result<Option<Vec<u8>>, StorageError> {
     let rows = vault.query(
         "SELECT value FROM meta WHERE key=?1",
         &[Value::Text(key.to_owned())],
     )?;
-    Ok(rows
-        .first()
-        .and_then(|row| row.first())
-        .and_then(blob_value))
+    let Some(value) = rows.first().and_then(|row| row.first()) else {
+        return Ok(None);
+    };
+    Ok(Some(meta_bytes_of(key, value.clone())?))
 }
 
 fn read_meta_u64(vault: &Vault, key: &str) -> Result<Option<u64>, StorageError> {
diff --git a/crates/storage/src/sync_meta.rs b/crates/storage/src/sync_meta.rs
index 82db021..4ab5086 100644
--- a/crates/storage/src/sync_meta.rs
+++ b/crates/storage/src/sync_meta.rs
@@ -71,6 +71,10 @@ impl ConsumedPrefix {
 ///
 /// 与 Vault 版 [`crate::sync_bridge::set_sync_log_id`] 同语义，区别只是直接在给定连接
 /// （或事务）上执行，供批量 inbox 在同一事务里完成校验。
+///
+/// 既有值不是 BLOB 时 [`read_meta_on`] 返回 [`StorageError::Corrupt`]，本函数直接传播：
+/// 损坏的 incarnation 记录**不能**当成"尚未绑定"覆盖掉（那会让旧传代的 inbox 行混进
+/// 新一代）。
 pub(crate) fn ensure_log_id_on(conn: &Connection, log_id: &[u8; 16]) -> Result<(), StorageError> {
     match read_meta_on(conn, META_SYNC_LOG_ID)? {
         Some(existing) => {
@@ -313,16 +317,58 @@ pub(crate) fn skipped_received_key(log_id: &[u8], relay_seq: u64) -> Result<Stri
     ))
 }
 
-fn read_meta_on(conn: &Connection, key: &str) -> Result<Option<Vec<u8>>, StorageError> {
+/// `meta.value` 的**唯一**解读口径：只认 BLOB，其它 SQLite 类型一律是损坏。
+///
+/// 三个读取入口共享本函数，避免"同一份库读出两套结论"漂移：
+///
+/// - [`crate::Vault::get_meta`]（actor 线程上的命令）；
+/// - `sync_bridge::read_meta`（同步桥经 Vault 命令读取）；
+/// - 本模块的 [`read_meta_on`]（worker 独占连接 / 事务内的批量 inbox 校验）。
+///
+/// 为什么不把其它类型「当字节」或「当缺失」：
+///
+/// - **当缺失会静默绕过持久屏障**：`sync.repack_required`（轮换后必须重打包）、
+///   `sync.log_id`（incarnation 绑定）、`sync.received_seq` 这类键"读不到"就等价于
+///   "没有屏障/没有游标"，把一处损坏放大成绕过安全前置条件；
+/// - **当字节与 schema 相悖**：`meta.value` 是 `BLOB NOT NULL`（STRICT 表），能读出
+///   TEXT/INTEGER/REAL 本身就说明库被外部改坏或 schema 漂移，继续按字节解释只会让
+///   损坏值流入上层语义。
+///
+/// 正常写入路径写不出非 BLOB（STRICT 表会直接拒绝），所以这里命中的都是
+/// "库被外部改坏"的场景：宁可 fail-closed 报损坏，也不猜。
+pub(crate) fn meta_bytes_of(key: &str, value: Value) -> Result<Vec<u8>, StorageError> {
+    match value {
+        Value::Blob(bytes) => Ok(bytes),
+        other => Err(StorageError::Corrupt {
+            context: format!(
+                "meta[{key}] 应为 BLOB，实际 {}（库被外部改坏或 schema 漂移）",
+                sqlite_type_name(&other)
+            ),
+        }),
+    }
+}
+
+/// SQLite 值的类型名（只用于错误信息）。
+fn sqlite_type_name(value: &Value) -> &'static str {
+    match value {
+        Value::Null => "NULL",
+        Value::Integer(_) => "INTEGER",
+        Value::Real(_) => "REAL",
+        Value::Text(_) => "TEXT",
+        Value::Blob(_) => "BLOB",
+    }
+}
+
+/// 在给定连接（或事务）上读一条 meta；键不存在返回 `None`。
+///
+/// 类型判定走 [`meta_bytes_of`]：只认 BLOB，其它类型是 [`StorageError::Corrupt`]
+/// （不再静默当作缺失）。
+pub(crate) fn read_meta_on(conn: &Connection, key: &str) -> Result<Option<Vec<u8>>, StorageError> {
     let mut statement = conn.prepare("SELECT value FROM meta WHERE key=?1")?;
     let mut rows = statement.query([key])?;
     match rows.next()? {
         None => Ok(None),
-        // 与旧实现同口径：只认 BLOB；其它类型视为没有该 meta。
-        Some(row) => match row.get::<usize, Value>(0)? {
-            Value::Blob(blob) => Ok(Some(blob)),
-            _ => Ok(None),
-        },
+        Some(row) => Ok(Some(meta_bytes_of(key, row.get::<usize, Value>(0)?)?)),
     }
 }
 
diff --git a/crates/storage/tests/baseline_apply.rs b/crates/storage/tests/baseline_apply.rs
index 5aac713..f312f17 100644
--- a/crates/storage/tests/baseline_apply.rs
+++ b/crates/storage/tests/baseline_apply.rs
@@ -4,6 +4,7 @@ mod support;
 
 use std::time::Duration;
 
+use loro::VersionVector;
 use support::*;
 
 use menghunote_storage::session::{
@@ -15,9 +16,10 @@ use menghunote_storage::{
     apply_baseline_batch, baseline_apply_debug_counters, baseline_apply_state,
     begin_baseline_apply, create_note_slot, finish_baseline_apply, load_body_doc, load_head,
     reset_baseline_apply_debug, set_sync_log_id, store_inbox_verified_batch, sync_cursor_state,
-    BaselineApplyFinish, BaselineApplyStatus, BaselineAsset, BodyDoc, SyncContext, UpdateRecord,
-    Value, Vault, VerifiedInboxRow, META_SYNC_CONFIRMED_HIGHWATER, META_SYNC_INTEGRATED_SEQ,
-    META_SYNC_RECEIVED_SEQ, META_SYNC_RELAY_CURSOR, SCHEMA_VERSION,
+    BaselineApplyFinish, BaselineApplyStatus, BaselineAsset, BaselineExpectation, BodyDoc,
+    StorageError, SyncContext, UpdateRecord, Value, Vault, VerifiedInboxRow,
+    META_SYNC_CONFIRMED_HIGHWATER, META_SYNC_INTEGRATED_SEQ, META_SYNC_RECEIVED_SEQ,
+    META_SYNC_RELAY_CURSOR, SCHEMA_VERSION,
 };
 use menghunote_text::UnicodeScalar;
 
@@ -85,14 +87,89 @@ fn meta_remote_known_count(vault: &Vault, doc_id: &str) -> i64 {
     )
 }
 
+/// 读某篇文档的远端已知 VV；没有记录返回 `None`。
+fn meta_remote_known(vault: &Vault, doc_id: &str) -> Option<VersionVector> {
+    meta_remote_known_bytes(vault, doc_id).map(|bytes| VersionVector::decode(&bytes).expect("解码"))
+}
+
+/// 读某篇文档的远端已知 VV 原始字节（用于断言「不降级」）。
+fn meta_remote_known_bytes(vault: &Vault, doc_id: &str) -> Option<Vec<u8>> {
+    let rows = vault
+        .query(
+            "SELECT value FROM meta WHERE key=?1",
+            &[Value::Text(format!("sync.remote_known_vv.{doc_id}"))],
+        )
+        .expect("meta 查询");
+    match rows.first().and_then(|row| row.first()) {
+        Some(Value::Blob(bytes)) => Some(bytes.clone()),
+        None => None,
+        other => panic!("remote_known 值异常：{other:?}"),
+    }
+}
+
+/// 注入「读旧值之后、CAS 之前 meta 被并发改写」：在 baseline_apply_docs 首次写入时
+/// 把该文档的远端已知 VV 改成别的字节（同一事务内可见，CAS 必然不命中）。
+fn install_remote_known_clobber_trigger(vault: &Vault, doc_id: &str) {
+    let sql = format!(
+        "CREATE TRIGGER remote_known_cas_race BEFORE INSERT ON baseline_apply_docs \
+         BEGIN \
+           INSERT OR REPLACE INTO meta(key,value) \
+           VALUES('sync.remote_known_vv.{doc_id}', X'DEADBEEF'); \
+         END;"
+    );
+    vault.exec(&sql, &[]).expect("安装竞态触发器");
+}
+
 fn snapshot(doc_id: &str, peer: u64, text: &str) -> Vec<u8> {
     let doc = BodyDoc::create(doc_id, peer).expect("创建基线 LoroDoc");
     doc.insert_scalars(0, text).expect("写基线正文");
     doc.export_snapshot().expect("导出基线 snapshot")
 }
 
-fn begin(vault: &Vault, s: u64, cursor: &str) {
-    begin_baseline_apply(vault, &[7; 16], s, cursor).expect("begin baseline");
+/// 预期参与集合：只声明文档（本文件里大多数用例没有资产）。
+fn expectation(docs: &[&str]) -> BaselineExpectation {
+    BaselineExpectation::new(
+        docs.iter().map(|doc_id| (*doc_id).to_owned()),
+        std::iter::empty::<[u8; 32]>(),
+    )
+}
+
+/// 预期参与集合：文档 + 资产。
+fn expectation_with_assets(docs: &[&str], assets: &[[u8; 32]]) -> BaselineExpectation {
+    BaselineExpectation::new(
+        docs.iter().map(|doc_id| (*doc_id).to_owned()),
+        assets.iter().copied(),
+    )
+}
+
+/// begin 一份**空预期集合**的 staging（只关心 cursor/journal 语义的用例）。
+fn begin(vault: &Vault, s: u64, cursor: &str) -> menghunote_storage::BaselineApplyState {
+    begin_with(vault, s, cursor, &BaselineExpectation::default())
+}
+
+/// begin 一份显式声明预期参与集合的 staging。
+fn begin_with(
+    vault: &Vault,
+    s: u64,
+    cursor: &str,
+    expected: &BaselineExpectation,
+) -> menghunote_storage::BaselineApplyState {
+    begin_baseline_apply(vault, &[7; 16], s, cursor, expected).expect("begin baseline")
+}
+
+/// 读某篇文档的 tombstone action（当前 journal）。
+fn tombstone_action(vault: &Vault, doc_id: &str) -> Option<String> {
+    let rows = vault
+        .query(
+            "SELECT action FROM baseline_apply_tombstones WHERE doc_id=?1",
+            &[Value::Text(doc_id.to_owned())],
+        )
+        .expect("tombstone 查询");
+    match rows.first().and_then(|row| row.first()) {
+        Some(Value::Text(value)) => Some(value.clone()),
+        Some(other) => panic!("tombstone action 异常：{other:?}"),
+        None => None,
+    }
 }
 
 /// 模拟"已有库"的同步水位；`None` 表示 meta 缺省。
@@ -153,7 +230,7 @@ fn baseline_merge_preserves_local_edit_and_advances_cursor() {
     let local = local_doc(&vault, "doc-1", "local");
     let snap = snapshot("doc-1", PEER_B, "remote");
 
-    begin(&vault, 5, "source-cursor");
+    begin_with(&vault, 5, "source-cursor", &expectation(&["doc-1"]));
     assert_eq!(
         baseline_apply_state(&vault)
             .expect("state")
@@ -273,7 +350,7 @@ fn batch_is_resumable_after_reopen_and_idempotent() {
     let snap_a = snapshot("doc-a", PEER_B, "a-remote");
     let snap_b = snapshot("doc-b", PEER_B, "b-remote");
 
-    begin(&vault, 9, "src");
+    begin_with(&vault, 9, "src", &expectation(&["doc-a", "doc-b"]));
     apply_baseline_batch(&vault, &[("doc-a".to_owned(), snap_a.clone())], &[]).expect("batch a");
     let cursor = sync_cursor_state(&vault).expect("cursor before finish");
     assert_eq!(cursor.received_seq, 0);
@@ -317,7 +394,7 @@ fn batch_is_resumable_after_reopen_and_idempotent() {
 fn finish_cursor_cas_conflict_does_not_advance() {
     let (_dir, vault) = fresh_vault();
     local_doc(&vault, "doc-1", "local");
-    begin(&vault, 4, "src");
+    begin_with(&vault, 4, "src", &expectation(&["doc-1"]));
     apply_baseline_batch(
         &vault,
         &[("doc-1".to_owned(), snapshot("doc-1", PEER_B, "remote"))],
@@ -881,7 +958,7 @@ fn replay_same_digest_skips_snapshot_decode() {
 #[test]
 fn tombstone_deletes_previous_baseline_doc_after_begin_clears_staging_tables() {
     let (_dir, vault) = fresh_vault();
-    begin(&vault, 10, "src1");
+    begin_with(&vault, 10, "src1", &expectation(&["doc-x"]));
     apply_baseline_batch(
         &vault,
         &[("doc-x".to_owned(), snapshot("doc-x", PEER_B, "remote body"))],
@@ -1080,3 +1157,620 @@ fn tombstone_deletes_doc_integrated_through_engine_path() {
     );
     assert_eq!(state.kept_local_edit, 0);
 }
+
+// ---------------------------------------------------------------------------
+// P1-1：finish 完整性（begin 记录预期参与集合，finish 前逐项证明）
+// ---------------------------------------------------------------------------
+
+/// 少跑一批就 finish：必须 Conflict 且一行都不写（旧实现会 Applied 并推进 S，
+/// pull 之后永久跳过这批 relay 事件）。
+#[test]
+fn finish_without_all_docs_is_conflict() {
+    let (_dir, vault) = fresh_vault();
+    local_doc(&vault, "doc-a", "a-local");
+    local_doc(&vault, "doc-b", "b-local");
+    begin_with(&vault, 5, "src", &expectation(&["doc-a", "doc-b"]));
+
+    let state = apply_baseline_batch(
+        &vault,
+        &[("doc-a".to_owned(), snapshot("doc-a", PEER_B, "a-remote"))],
+        &[],
+    )
+    .expect("batch a");
+    assert_eq!(state.applied_docs, 1);
+    assert_eq!(state.expected_docs, Some(2));
+    assert!(
+        !state.finishable,
+        "预期集合未齐（doc-b 缺失）时不得声称可 finish"
+    );
+
+    assert_eq!(
+        finish_baseline_apply(&vault, 5, "baseline-cursor").expect("finish"),
+        BaselineApplyFinish::Conflict,
+        "预期集合未齐时 finish 必须 Conflict"
+    );
+
+    // 冲突时一行都不写：水位/meta/游标/journal 全部保持 begin 前状态。
+    let cursor = sync_cursor_state(&vault).expect("cursor");
+    assert_eq!(cursor.received_seq, 0, "冲突时不得推进 received");
+    assert_eq!(cursor.integrated_seq, 0, "冲突时不得推进 integrated");
+    assert_eq!(cursor.confirmed_highwater, None, "冲突时不得写 highwater");
+    assert_eq!(relay_cursor(&vault), None, "冲突时不得写 relay cursor");
+    let journal = baseline_apply_state(&vault)
+        .expect("state")
+        .expect("journal");
+    assert_eq!(journal.status, BaselineApplyStatus::Staging);
+    assert_eq!(journal.chosen_cursor, None);
+    assert_eq!(journal.applied_docs, 1, "冲突不得回滚已提交批次");
+
+    // 补齐缺失批次后必须能正常 finish（Conflict 不是永久卡死）。
+    let state = apply_baseline_batch(
+        &vault,
+        &[("doc-b".to_owned(), snapshot("doc-b", PEER_B, "b-remote"))],
+        &[],
+    )
+    .expect("batch b");
+    assert!(state.finishable, "两篇都到齐后必须可 finish");
+    let BaselineApplyFinish::Applied(state) =
+        finish_baseline_apply(&vault, 5, "baseline-cursor").expect("finish")
+    else {
+        panic!("补齐后 finish 必须 Applied");
+    };
+    assert_eq!(state.status, BaselineApplyStatus::Completed);
+    assert_eq!(state.applied_docs, 2);
+    let cursor = sync_cursor_state(&vault).expect("cursor");
+    assert_eq!(cursor.received_seq, 5);
+    assert_eq!(cursor.integrated_seq, 5);
+    assert_eq!(cursor.confirmed_highwater, Some(5));
+}
+
+/// 全部分批到齐：finish 正常 Applied。
+#[test]
+fn finish_after_full_batches_is_applied() {
+    let (_dir, vault) = fresh_vault();
+    local_doc(&vault, "doc-a", "a-local");
+    local_doc(&vault, "doc-b", "b-local");
+    begin_with(&vault, 9, "src", &expectation(&["doc-a", "doc-b"]));
+
+    let first = apply_baseline_batch(
+        &vault,
+        &[("doc-a".to_owned(), snapshot("doc-a", PEER_B, "a-remote"))],
+        &[],
+    )
+    .expect("batch a");
+    assert_eq!(first.expected_docs, Some(2));
+    assert!(!first.finishable);
+
+    let second = apply_baseline_batch(
+        &vault,
+        &[("doc-b".to_owned(), snapshot("doc-b", PEER_B, "b-remote"))],
+        &[],
+    )
+    .expect("batch b");
+    assert!(second.finishable);
+
+    let BaselineApplyFinish::Applied(state) =
+        finish_baseline_apply(&vault, 9, "baseline-cursor").expect("finish")
+    else {
+        panic!("完整批次 finish 必须 Applied");
+    };
+    assert_eq!(state.status, BaselineApplyStatus::Completed);
+    assert_eq!(state.applied_docs, 2);
+    assert_eq!(state.expected_docs, Some(2));
+    assert_eq!(state.expected_assets, Some(0));
+    assert!(!state.finishable, "completed 后不再是 finishable");
+    let cursor = sync_cursor_state(&vault).expect("cursor");
+    assert_eq!(cursor.received_seq, 9);
+    assert_eq!(cursor.integrated_seq, 9);
+    assert_eq!(cursor.confirmed_highwater, Some(9));
+    assert_eq!(
+        relay_cursor(&vault).as_deref(),
+        Some(b"baseline-cursor".as_slice())
+    );
+}
+
+/// begin 同参数重入幂等（含顺序无关的同一集合）；同 checkpoint 换集合必须冲突，
+/// 且不得清掉已应用进度或预期集合。
+#[test]
+fn begin_idempotent_replay_keeps_expected_set() {
+    let (_dir, vault) = fresh_vault();
+    local_doc(&vault, "doc-a", "a-local");
+    local_doc(&vault, "doc-b", "b-local");
+    begin_with(&vault, 5, "src", &expectation(&["doc-a", "doc-b"]));
+    apply_baseline_batch(
+        &vault,
+        &[("doc-a".to_owned(), snapshot("doc-a", PEER_B, "a-remote"))],
+        &[],
+    )
+    .expect("batch a");
+
+    // 同参数 + 同一集合（顺序不同）：幂等重入，保留进度与集合。
+    let replayed = begin_with(&vault, 5, "src", &expectation(&["doc-b", "doc-a"]));
+    assert_eq!(replayed.applied_docs, 1, "重入不得清空已应用批次");
+    assert_eq!(replayed.expected_docs, Some(2), "重入必须保留预期集合");
+    assert_eq!(replayed.source_cursor, "src");
+    assert!(!replayed.finishable);
+
+    // 同 checkpoint/S/cursor 但预期集合不同：必须冲突（不能静默沿用新集合）。
+    let error = begin_baseline_apply(&vault, &[7; 16], 5, "src", &expectation(&["doc-a"]))
+        .expect_err("同一 checkpoint 换预期集合必须冲突");
+    assert!(
+        matches!(error, StorageError::SyncConflict { .. }),
+        "应当是 SyncConflict，实际 {error:?}"
+    );
+    let state = baseline_apply_state(&vault)
+        .expect("state")
+        .expect("journal");
+    assert_eq!(state.applied_docs, 1, "冲突不得清空已应用批次");
+    assert_eq!(state.expected_docs, Some(2), "冲突不得改写预期集合");
+
+    // 补齐后仍能 finish。
+    apply_baseline_batch(
+        &vault,
+        &[("doc-b".to_owned(), snapshot("doc-b", PEER_B, "b-remote"))],
+        &[],
+    )
+    .expect("batch b");
+    assert!(matches!(
+        finish_baseline_apply(&vault, 5, "baseline-cursor").expect("finish"),
+        BaselineApplyFinish::Applied(_)
+    ));
+}
+
+/// 资产少一批同样必须 Conflict（bootstrap 的资产也只能在全部到齐后 finish）。
+#[test]
+fn finish_without_all_assets_is_conflict() {
+    let (_dir, vault) = fresh_vault();
+    local_doc(&vault, "doc-1", "local");
+    let asset_a = BaselineAsset {
+        object_id: [1; 32],
+        asset_hash: [11; 32],
+        byte_len: 3,
+        media_type: Some("text/plain".to_owned()),
+        data: b"aaa".to_vec(),
+    };
+    let asset_b = BaselineAsset {
+        object_id: [2; 32],
+        asset_hash: [12; 32],
+        byte_len: 4,
+        media_type: Some("text/plain".to_owned()),
+        data: b"bbbb".to_vec(),
+    };
+    begin_with(
+        &vault,
+        4,
+        "src",
+        &expectation_with_assets(&["doc-1"], &[[1; 32], [2; 32]]),
+    );
+    apply_baseline_batch(
+        &vault,
+        &[("doc-1".to_owned(), snapshot("doc-1", PEER_B, "remote"))],
+        &[],
+    )
+    .expect("doc batch");
+    let state = apply_baseline_assets(&vault, std::slice::from_ref(&asset_a)).expect("asset a");
+    assert_eq!(state.applied_assets, 1);
+    assert_eq!(state.expected_assets, Some(2));
+    assert!(!state.finishable, "资产少一批时不得声称可 finish");
+
+    assert_eq!(
+        finish_baseline_apply(&vault, 4, "baseline-cursor").expect("finish"),
+        BaselineApplyFinish::Conflict,
+        "预期资产未齐时 finish 必须 Conflict"
+    );
+    // 一行都不写：水位/游标/journal 保持 staging。
+    let cursor = sync_cursor_state(&vault).expect("cursor");
+    assert_eq!(cursor.received_seq, 0);
+    assert_eq!(cursor.integrated_seq, 0);
+    assert_eq!(cursor.confirmed_highwater, None);
+    assert_eq!(relay_cursor(&vault), None);
+    let journal = baseline_apply_state(&vault)
+        .expect("state")
+        .expect("journal");
+    assert_eq!(journal.status, BaselineApplyStatus::Staging);
+    assert_eq!(journal.chosen_cursor, None);
+    assert_eq!(journal.applied_assets, 1, "冲突不得回滚已应用资产");
+
+    let state = apply_baseline_assets(&vault, std::slice::from_ref(&asset_b)).expect("asset b");
+    assert!(state.finishable);
+    assert!(matches!(
+        finish_baseline_apply(&vault, 4, "baseline-cursor").expect("finish"),
+        BaselineApplyFinish::Applied(_)
+    ));
+}
+
+// ---------------------------------------------------------------------------
+// P1-2：tombstone 判定与 notes 软删之间的 TOCTOU
+// ---------------------------------------------------------------------------
+
+/// 判定（事务外看到无 outbox）之后、事务执行期间插进一条 outbox：事务内重判必须把
+/// action 降级为 kept_local_edit，`notes.deleted` 保持 0。
+#[test]
+fn tombstone_outbox_race_between_decision_and_transaction_keeps_note() {
+    let (_dir, vault) = fresh_vault();
+    // doc-a：本批要 merge 的基线文档；触发器挂在它的 journal 写入上。
+    local_doc(&vault, "doc-a", "a-local");
+
+    // doc-pull：普通 pull 集成来的文档（远端已知、无 outbox），判定本来会删除它。
+    seed_note(&vault, "note-pull", "doc-pull");
+    let mut session =
+        DocumentSession::open(&vault, "doc-pull", session_options(Some(SYNC_CTX))).expect("会话");
+    let delta = session
+        .apply_remote(&remote_update("doc-pull", PEER_B, "pulled body"))
+        .expect("导入远端")
+        .expect("应当产生 LiveDelta");
+    assert_eq!(delta.source, ChangeSource::Remote);
+    session.flush(FlushReason::Manual);
+    session
+        .wait_for_durable(LiveRev(1), Duration::from_secs(5))
+        .expect("远端导入必须 durable");
+    drop(session);
+    assert_eq!(count(&vault, "SELECT count(*) FROM sync_outbox", &[]), 0);
+    assert_eq!(meta_remote_known_count(&vault, "doc-pull"), 1);
+
+    // 注入竞态：docs 批的 journal 写入（先于同批 tombstone 语句）触发一条 outbox 行，
+    // 模拟"判定完成后本地编辑的 A 事务刚好提交"。
+    vault
+        .exec(
+            "CREATE TRIGGER baseline_race_outbox BEFORE INSERT ON baseline_apply_docs \
+             WHEN NEW.doc_id='doc-a' BEGIN \
+               INSERT INTO sync_outbox(message_id,batch_id,doc_id,seq,key_epoch,ciphertext) \
+               VALUES('race-outbox','race','doc-pull',1,1,NULL); \
+             END;",
+            &[],
+        )
+        .expect("注入竞态触发器");
+
+    begin_with(&vault, 7, "src", &expectation(&["doc-a"]));
+    let state = apply_baseline_batch(
+        &vault,
+        &[("doc-a".to_owned(), snapshot("doc-a", PEER_B, "a-remote"))],
+        &["doc-pull".to_owned()],
+    )
+    .expect("apply with raced tombstone");
+
+    assert_eq!(
+        count(
+            &vault,
+            "SELECT count(*) FROM sync_outbox WHERE doc_id='doc-pull'",
+            &[],
+        ),
+        1,
+        "触发器必须真的插入了 outbox（证明竞态窗口被覆盖）"
+    );
+    assert_eq!(
+        notes_deleted(&vault, "doc-pull"),
+        0,
+        "判定后被并发插入 outbox 的笔记绝不能软删"
+    );
+    assert_eq!(
+        tombstone_action(&vault, "doc-pull").as_deref(),
+        Some("kept_local_edit"),
+        "事务内重判必须把 action 降级为 kept_local_edit"
+    );
+    assert_eq!(state.kept_local_edit, 1);
+}
+
+// ---------------------------------------------------------------------------
+// P2-5：远端已知 VV 的单调性（值 CAS）与 begin 回填覆盖面
+// ---------------------------------------------------------------------------
+
+/// completed journal（finish 后 status='completed'、子表保留）在 begin 替换时
+/// 必须先回填 baseline_vv；这是旧库升级后 tombstone 不误判的关键证据。
+#[test]
+fn begin_backfills_remote_known_for_completed_journal() {
+    let (_dir, vault) = fresh_vault();
+    begin_with(&vault, 10, "src1", &expectation(&["doc-completed"]));
+    apply_baseline_batch(
+        &vault,
+        &[(
+            "doc-completed".to_owned(),
+            snapshot("doc-completed", PEER_B, "remote body"),
+        )],
+        &[],
+    )
+    .expect("first baseline");
+    assert!(matches!(
+        finish_baseline_apply(&vault, 10, "c10").expect("finish"),
+        BaselineApplyFinish::Applied(_)
+    ));
+    let baseline_vv = meta_remote_known(&vault, "doc-completed").expect("已有远端已知 VV");
+
+    // 模拟旧版本数据：没有跨 journal 的 meta，只剩 completed journal + 子表。
+    vault
+        .exec(
+            "DELETE FROM meta WHERE key=?1",
+            &[Value::Text("sync.remote_known_vv.doc-completed".to_owned())],
+        )
+        .expect("删 meta");
+    assert_eq!(
+        count(
+            &vault,
+            "SELECT count(*) FROM baseline_apply_docs WHERE doc_id='doc-completed'",
+            &[]
+        ),
+        1
+    );
+
+    begin(&vault, 20, "src2");
+
+    assert_eq!(
+        meta_remote_known(&vault, "doc-completed"),
+        Some(baseline_vv),
+        "completed journal 的 baseline_vv 必须回填到 meta"
+    );
+    assert_eq!(
+        count(&vault, "SELECT count(*) FROM baseline_apply_docs", &[]),
+        0,
+        "旧 journal 子表必须清掉"
+    );
+    let state = baseline_apply_state(&vault)
+        .expect("state")
+        .expect("journal");
+    assert_eq!(state.status, BaselineApplyStatus::Staging);
+    assert_eq!(state.baseline_s, 20);
+}
+
+/// 没有 journal 行、但残留 `baseline_apply_docs`（旧版本/异常写的孤儿行）时，
+/// begin 也必须先把 `baseline_vv` 回填到 meta，再清掉残留子表。
+#[test]
+fn begin_backfills_remote_known_without_journal_but_with_residual_docs() {
+    let (_dir, vault) = fresh_vault();
+    begin_with(&vault, 10, "src1", &expectation(&["doc-residual"]));
+    apply_baseline_batch(
+        &vault,
+        &[(
+            "doc-residual".to_owned(),
+            snapshot("doc-residual", PEER_B, "remote body"),
+        )],
+        &[],
+    )
+    .expect("first baseline");
+    assert!(matches!(
+        finish_baseline_apply(&vault, 10, "c10").expect("finish"),
+        BaselineApplyFinish::Applied(_)
+    ));
+
+    // 模拟旧版本/异常写：journal 行丢失，但子表仍在；meta 也缺失。
+    vault
+        .exec("DELETE FROM baseline_apply", &[])
+        .expect("删 journal");
+    vault
+        .exec(
+            "DELETE FROM meta WHERE key=?1",
+            &[Value::Text("sync.remote_known_vv.doc-residual".to_owned())],
+        )
+        .expect("删 meta");
+    assert_eq!(
+        count(
+            &vault,
+            "SELECT count(*) FROM baseline_apply_docs WHERE applied=1",
+            &[]
+        ),
+        1,
+        "前置：残留一行已应用的 baseline_apply_docs"
+    );
+
+    begin(&vault, 20, "src2");
+
+    assert_eq!(
+        meta_remote_known_count(&vault, "doc-residual"),
+        1,
+        "没有 journal 的残留 baseline_vv 也必须回填（否则证据会被 DELETE 静默丢弃）"
+    );
+    assert_eq!(
+        meta_remote_known(&vault, "doc-residual"),
+        Some(
+            VersionVector::decode(
+                &meta_remote_known_bytes(&vault, "doc-residual").expect("回填值")
+            )
+            .expect("解码")
+        ),
+        "回填的是旧 journal 的 baseline VV"
+    );
+    assert_eq!(
+        count(&vault, "SELECT count(*) FROM baseline_apply_docs", &[]),
+        0,
+        "残留子表必须被清掉"
+    );
+    let state = baseline_apply_state(&vault)
+        .expect("state")
+        .expect("journal");
+    assert_eq!(state.status, BaselineApplyStatus::Staging);
+    assert_eq!(state.baseline_s, 20);
+}
+
+/// begin 的回填是单调的：已有 meta（普通 pull/integrate 记录的更新值）绝不被
+/// 旧 journal 的 baseline_vv 降级（`DO NOTHING`）。
+#[test]
+fn begin_backfill_never_downgrades_existing_remote_known() {
+    let (_dir, vault) = fresh_vault();
+    begin_with(&vault, 10, "src1", &expectation(&["doc-keep"]));
+    apply_baseline_batch(
+        &vault,
+        &[(
+            "doc-keep".to_owned(),
+            snapshot("doc-keep", PEER_B, "baseline body"),
+        )],
+        &[],
+    )
+    .expect("first baseline");
+    assert!(matches!(
+        finish_baseline_apply(&vault, 10, "c10").expect("finish"),
+        BaselineApplyFinish::Applied(_)
+    ));
+
+    // 已有值比旧 journal 的 baseline VV 更大（模拟后续普通 pull 扩大了它）。
+    let mut superset = meta_remote_known(&vault, "doc-keep").expect("baseline 后已有值");
+    superset.insert(0xA1, 9);
+    vault
+        .exec(
+            "INSERT INTO meta(key,value) VALUES(?1,?2) \
+             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
+            &[
+                Value::Text("sync.remote_known_vv.doc-keep".to_owned()),
+                Value::Blob(superset.encode()),
+            ],
+        )
+        .expect("写入更大的旧值");
+    let before = meta_remote_known_bytes(&vault, "doc-keep").expect("旧值");
+
+    vault
+        .exec("DELETE FROM baseline_apply", &[])
+        .expect("模拟无 journal 的残留子表");
+    begin(&vault, 20, "src2");
+
+    assert_eq!(
+        meta_remote_known_bytes(&vault, "doc-keep"),
+        Some(before),
+        "begin 回填绝不能把已有（更大的）远端已知 VV 降级"
+    );
+}
+
+/// 新文档（读旧值时不存在）的 CAS：事务内被并发写入抢占 → 明确 SyncConflict，
+/// 整批回滚，不覆盖任何值。
+#[test]
+fn baseline_remote_known_cas_fails_closed_for_new_doc() {
+    let (_dir, vault) = fresh_vault();
+    begin_with(&vault, 5, "src", &expectation(&["doc-cas-new"]));
+    install_remote_known_clobber_trigger(&vault, "doc-cas-new");
+
+    let error = apply_baseline_batch(
+        &vault,
+        &[(
+            "doc-cas-new".to_owned(),
+            snapshot("doc-cas-new", PEER_B, "remote"),
+        )],
+        &[],
+    )
+    .expect_err("CAS 必须失败");
+
+    match &error {
+        StorageError::SyncConflict { context } => {
+            assert!(
+                context.contains("远端已知 VV"),
+                "错误必须点明远端已知 VV CAS：{context}"
+            );
+        }
+        other => panic!("期望 SyncConflict，实际 {other:?}"),
+    }
+    assert_eq!(
+        meta_remote_known_bytes(&vault, "doc-cas-new"),
+        None,
+        "触发器写入的 meta 必须随整批回滚"
+    );
+    assert_eq!(
+        count(
+            &vault,
+            "SELECT count(*) FROM loro_docs WHERE doc_id='doc-cas-new'",
+            &[]
+        ),
+        0,
+        "整批回滚：文档行不得落库"
+    );
+    assert_eq!(
+        count(
+            &vault,
+            "SELECT count(*) FROM loro_updates WHERE doc_id='doc-cas-new'",
+            &[]
+        ),
+        0
+    );
+}
+
+/// 已存在文档（读旧值 `Some`）的 CAS：库中值被并发改写 → 明确 SyncConflict，
+/// 队头与远端已知 VV 都保持原值（绝不回退到本批并集）。
+#[test]
+fn baseline_remote_known_cas_fails_closed_for_existing_doc() {
+    let (_dir, vault) = fresh_vault();
+    begin_with(&vault, 5, "src1", &expectation(&["doc-cas-old"]));
+    apply_baseline_batch(
+        &vault,
+        &[(
+            "doc-cas-old".to_owned(),
+            snapshot("doc-cas-old", PEER_B, "first"),
+        )],
+        &[],
+    )
+    .expect("first baseline");
+    assert!(matches!(
+        finish_baseline_apply(&vault, 5, "c5").expect("finish"),
+        BaselineApplyFinish::Applied(_)
+    ));
+    let before_known = meta_remote_known_bytes(&vault, "doc-cas-old").expect("已有远端已知 VV");
+    let before_head = load_head(&vault, "doc-cas-old").expect("head");
+
+    begin_with(&vault, 6, "src2", &expectation(&["doc-cas-old"]));
+    install_remote_known_clobber_trigger(&vault, "doc-cas-old");
+    let error = apply_baseline_batch(
+        &vault,
+        &[(
+            "doc-cas-old".to_owned(),
+            snapshot("doc-cas-old", PEER_B, "second"),
+        )],
+        &[],
+    )
+    .expect_err("陈旧旧值的 CAS 必须失败");
+
+    assert!(
+        matches!(&error, StorageError::SyncConflict { context } if context.contains("doc-cas-old")),
+        "必须是带文档上下文的 SyncConflict，实际 {error:?}"
+    );
+    assert_eq!(
+        meta_remote_known_bytes(&vault, "doc-cas-old"),
+        Some(before_known),
+        "CAS 失败后已有值必须保持原样（不能变小，也不能被本批并集覆盖）"
+    );
+    assert_eq!(
+        load_head(&vault, "doc-cas-old").expect("head").head_seq,
+        before_head.head_seq,
+        "整批回滚：head 不得推进"
+    );
+}
+
+/// 正常路径是单调并集：普通 pull 已记录的远端 ops（本批快照里没有）必须保留。
+#[test]
+fn baseline_remote_known_write_unions_with_normal_pull_evidence() {
+    let (_dir, vault) = fresh_vault();
+    let mut pulled = VersionVector::new();
+    pulled.insert(0xA1, 2);
+    vault
+        .exec(
+            "INSERT INTO meta(key,value) VALUES(?1,?2)",
+            &[
+                Value::Text("sync.remote_known_vv.doc-union".to_owned()),
+                Value::Blob(pulled.encode()),
+            ],
+        )
+        .expect("预置普通 pull 的远端已知 VV");
+
+    begin_with(&vault, 7, "src", &expectation(&["doc-union"]));
+    apply_baseline_batch(
+        &vault,
+        &[(
+            "doc-union".to_owned(),
+            snapshot("doc-union", PEER_B, "remote body"),
+        )],
+        &[],
+    )
+    .expect("baseline apply");
+
+    let known = meta_remote_known(&vault, "doc-union").expect("应用后必须有远端已知 VV");
+    assert_eq!(
+        known.get(&0xA1).copied(),
+        Some(2),
+        "普通 pull 的旧证据必须保留"
+    );
+    assert!(
+        known.get(&PEER_B).copied().unwrap_or(0) >= 1,
+        "本批快照的 VV 必须并入"
+    );
+    assert_eq!(
+        count(
+            &vault,
+            "SELECT count(*) FROM meta WHERE key='sync.remote_known_vv.doc-union'",
+            &[]
+        ),
+        1
+    );
+}
diff --git a/crates/storage/tests/body_doc_detached_oplog.rs b/crates/storage/tests/body_doc_detached_oplog.rs
new file mode 100644
index 0000000..77996d1
--- /dev/null
+++ b/crates/storage/tests/body_doc_detached_oplog.rs
@@ -0,0 +1,790 @@
+//! 任务 2 P0：detached oplog 测试矩阵（只加测试，不改生产代码）。
+//!
+//! `BodyDoc::import_oplog_only` / `pending_update_from_oplog` 是 `pub(crate)`，生产里唯一
+//! 调用方是 baseline apply 的 `existing_local_extra` 分支（见
+//! `crates/storage/src/baseline_apply.rs`）。本文件不放大可见性、不改生产代码，而是驱动
+//! 该分支真正落库的 `baseline_merge` 行来验证语义：
+//!
+//! 1. **detached 路径**：`BodyDoc::restore(基线快照)` → `detach` → `import(本地差集)` →
+//!    `pending_update_from_oplog(本地 VV)`。产物就是 `loro_updates` 里
+//!    `batch_id == BASELINE_MERGE_BATCH` 的那一行（payload / end_frontier / end_vv）。
+//! 2. **attached 参考**：`BodyDoc::restore(基线快照) + import(本地差集)`——同一条本地
+//!    增量，改成 attached 导入。
+//! 3. **attached 重放**：`BodyDoc::restore(None, 库里全部 update)` 与生产读取路径
+//!    `load_body_doc`。
+//!
+//! 每个用例都断言：detached 的 `end_frontier` 与 attached 的 frontier 逐字节相等，
+//! `end_vv` 与 attached 的 VV 语义相等（`VersionVector::encode` 的字节顺序取决于
+//! import 顺序，不是规范形式，见 [`canonical_vv`]）；三份 attached 结果的**正文逐字节
+//! 相同**；本地与基线内容不丢、不重复。
+//!
+//! LoroTree 用例：`BodyDoc` 只暴露正文 text，`StructureDoc` 不暴露 detach，因此按任务
+//! 约定用 `loro` 直接构造 `LoroDoc` / `LoroTree`，验证 `import_oplog_only` 依赖的同一条
+//! CRDT 不变量：detach 后 import 只写 oplog；从 oplog 导出的差集 attached 重放后，
+//! frontier / VV 与树的可见形状（遍历结果）都与 attached 合并完全一致。
+
+mod support;
+
+use loro::{ExportMode, LoroDoc, LoroTree, LoroValue, TreeID, VersionVector};
+use support::*;
+
+use menghunote_storage::{
+    apply_baseline_batch, begin_baseline_apply, doc_updates_since, finish_baseline_apply,
+    load_body_doc, BaselineApplyFinish, BaselineExpectation, BodyDoc, StoredLoroUpdate, Vault,
+    BASELINE_MERGE_BATCH,
+};
+
+/// 本地设备（attached 侧）。
+const A_PEER: u64 = PEER_A;
+/// 远端基线导出方（与本地不同的 peer）。
+const B_PEER: u64 = PEER_B;
+
+// ---------------------------------------------------------------------------
+// 正文（LoroText）用例的脚手架
+// ---------------------------------------------------------------------------
+
+/// 在内存里构造一篇正文（不落库），用作「远端基线」。
+fn memory_doc(doc_id: &str, peer: u64, text: &str) -> BodyDoc {
+    let doc = BodyDoc::create(doc_id, peer).expect("创建内存文档");
+    if !text.is_empty() {
+        doc.insert_scalars(0, text).expect("写远端基线正文");
+    }
+    doc
+}
+
+/// begin 一份「只含这一篇文档」的 baseline staging。
+fn begin_apply(vault: &Vault, doc_id: &str, baseline_s: u64, cursor: &str) {
+    let expected = BaselineExpectation::new(
+        std::iter::once(doc_id.to_owned()),
+        std::iter::empty::<[u8; 32]>(),
+    );
+    let checkpoint_id: [u8; 16] = [baseline_s as u8; 16];
+    begin_baseline_apply(vault, &checkpoint_id, baseline_s, cursor, &expected)
+        .expect("begin baseline apply");
+}
+
+/// 快照蕴含的 VV（生产 `baseline_vv` 的来源：restore snapshot 后取 vv）。
+fn snapshot_vv(snapshot: &[u8]) -> VersionVector {
+    BodyDoc::restore("vv-probe", B_PEER, Some(snapshot), &[])
+        .expect("基线快照可恢复")
+        .vv()
+}
+
+/// VV 的规范表示：`VersionVector::encode` 的字节顺序取决于 import 顺序（不是规范形式），
+/// 所以按 peer 排序后比较「peer -> counter」集合；语义相等即等价。
+fn canonical_vv(bytes: &[u8]) -> Vec<(u64, i32)> {
+    let vv = VersionVector::decode(bytes).expect("VV 可解码");
+    let mut entries: Vec<(u64, i32)> = vv.iter().map(|(peer, counter)| (*peer, *counter)).collect();
+    entries.sort_unstable();
+    entries
+}
+
+/// `loro_updates` 里 baseline merge 追加的那一行。
+struct DetachedMerge {
+    /// payload 由 `pending_update_from_oplog` 导出，end_frontier / end_vv 取 **oplog** 版本。
+    merged: StoredLoroUpdate,
+    /// 本地相对基线 VV 的差集（与生产 detached 分支 import 的字节同源）。
+    local_delta: Vec<u8>,
+}
+
+/// 驱动生产 `existing_local_extra`（detached）分支，并取回它落库的差集。
+fn merge_detached(
+    vault: &Vault,
+    doc_id: &str,
+    snapshot: &[u8],
+    local: &BodyDoc,
+    baseline_s: u64,
+    cursor: &str,
+) -> DetachedMerge {
+    let local_delta = local
+        .pending_update(&snapshot_vv(snapshot))
+        .expect("导出本地差集")
+        .payload;
+    assert!(
+        !local_delta.is_empty(),
+        "本地必须在基线之外还有 op，否则生产走 attached 分支，测不到 detached"
+    );
+    begin_apply(vault, doc_id, baseline_s, cursor);
+    apply_baseline_batch(vault, &[(doc_id.to_owned(), snapshot.to_vec())], &[])
+        .expect("应用基线快照");
+    let merged = doc_updates_since(vault, doc_id, 0)
+        .expect("读 loro_updates")
+        .into_iter()
+        .rfind(|update| update.batch_id == BASELINE_MERGE_BATCH)
+        .expect("detached 分支必须落一条 baseline_merge 差集");
+    assert!(!merged.payload.is_empty(), "detached 导出的差集不能为空");
+
+    // 分支守卫：detached 合并后的 oplog frontier 必须同时包含本地与基线两个 peer。
+    // 若生产误走 attached 分支，这里只会是基线 state 的单 peer frontier。
+    let frontier = loro::Frontiers::decode(&merged.end_frontier).expect("frontier 可解码");
+    let mut peers: Vec<u64> = frontier.iter().map(|id| id.peer).collect();
+    peers.sort_unstable();
+    peers.dedup();
+    assert_eq!(
+        peers,
+        vec![A_PEER, B_PEER],
+        "detached 分支未生效？合并后的 oplog frontier 必须同时包含两个 peer"
+    );
+
+    DetachedMerge {
+        merged,
+        local_delta,
+    }
+}
+
+/// 断言 detached 结果与 attached 路径完全等价，返回合并后的正文。
+///
+/// attached 侧有三条证据链，全部要与 detached 的 frontier 逐字节一致、VV 语义一致：
+/// 1. 基线副本 attached 导入同一条本地差集；
+/// 2. 库里全部 update 重新 restore（attached 重放）；
+/// 3. 生产读取入口 `load_body_doc`。
+fn assert_detached_matches_attached(
+    vault: &Vault,
+    doc_id: &str,
+    snapshot: &[u8],
+    parts: &DetachedMerge,
+) -> String {
+    // 1) attached 参考：基线副本 + attached 导入本地差集。
+    let attached =
+        BodyDoc::restore(doc_id, A_PEER, Some(snapshot), &[]).expect("attached 基线副本");
+    attached
+        .import(&parts.local_delta)
+        .expect("attached 导入本地差集");
+    let attached_text = attached.text();
+    assert_eq!(
+        canonical_vv(&attached.vv().encode()),
+        canonical_vv(&parts.merged.end_vv),
+        "detached oplog VV 与 attached 合并 VV 不一致"
+    );
+    assert_eq!(
+        attached.frontiers().encode(),
+        parts.merged.end_frontier,
+        "detached oplog frontier 与 attached 合并 frontier 不一致"
+    );
+
+    // 2) attached 重放：库里全部 update（本地 update + detached 导出的差集）。
+    let payloads: Vec<Vec<u8>> = doc_updates_since(vault, doc_id, 0)
+        .expect("读全部 update")
+        .into_iter()
+        .map(|update| update.payload)
+        .collect();
+    let replayed = BodyDoc::restore(doc_id, A_PEER, None, &payloads).expect("attached 重放");
+    assert_eq!(
+        replayed.text(),
+        attached_text,
+        "attached 重放文本与 attached 合并文本不是逐字节相同"
+    );
+    assert_eq!(
+        canonical_vv(&replayed.vv().encode()),
+        canonical_vv(&parts.merged.end_vv)
+    );
+    assert_eq!(replayed.frontiers().encode(), parts.merged.end_frontier);
+
+    // 3) 生产读取路径：文本、frontier、VV 必须与 attached 参考完全一致。
+    let (loaded, head) = load_body_doc(vault, doc_id, A_PEER, None).expect("生产重放");
+    assert_eq!(loaded.text(), attached_text, "load_body_doc 文本不一致");
+    assert_eq!(
+        canonical_vv(&loaded.vv().encode()),
+        canonical_vv(&parts.merged.end_vv)
+    );
+    assert_eq!(loaded.frontiers().encode(), parts.merged.end_frontier);
+    assert_eq!(
+        head.head_frontier, parts.merged.end_frontier,
+        "head_frontier 必须来自 detached 合并后的 oplog frontier"
+    );
+    assert_eq!(
+        canonical_vv(&head.head_vv),
+        canonical_vv(&parts.merged.end_vv),
+        "head_vv 必须来自 detached 合并后的 oplog VV"
+    );
+
+    attached_text
+}
+
+// ---------------------------------------------------------------------------
+// 1. 两个 peer 并发编辑同一 LoroText
+// ---------------------------------------------------------------------------
+
+#[test]
+fn concurrent_two_peer_text_inserts_match_attached_byte_for_byte() {
+    let (_dir, vault) = fresh_vault();
+    // 本地 peer A：独立文档，与基线完全不共享历史（最纯粹的并发同位置插入）。
+    let local = seed_note(&vault, "note-1", "doc-1");
+    local.insert_scalars(0, "LOCAL").expect("本地编辑");
+    commit_now(&vault, &local, "local-batch");
+
+    // 远端 peer B：导出基线快照。
+    let remote = memory_doc("doc-1", B_PEER, "REMOTE");
+    let snapshot = remote.export_snapshot().expect("基线快照");
+
+    let parts = merge_detached(&vault, "doc-1", &snapshot, &local, 5, "cursor-1");
+    let text = assert_detached_matches_attached(&vault, "doc-1", &snapshot, &parts);
+
+    assert_eq!(
+        text.matches("LOCAL").count(),
+        1,
+        "本地文本必须恰好出现一次：{text:?}"
+    );
+    assert_eq!(
+        text.matches("REMOTE").count(),
+        1,
+        "基线文本必须恰好出现一次：{text:?}"
+    );
+    assert!(
+        text.is_ascii(),
+        "纯 ASCII 合并后不得引入非 ASCII 字节：{text:?}"
+    );
+    assert_eq!(
+        text.len(),
+        "LOCAL".len() + "REMOTE".len(),
+        "字节数必须是两段 ASCII 之和：{text:?}"
+    );
+    assert_eq!(text.chars().count(), 11);
+}
+
+#[test]
+fn local_op_depending_on_baseline_matches_attached_after_detached_merge() {
+    let (_dir, vault) = fresh_vault();
+    // 远端：先导出 base，再在本地收不到的情况下追加 -r2（新基线是原来 + 这一笔）。
+    let remote = memory_doc("doc-1", B_PEER, "base");
+    let partial = remote.export_snapshot().expect("base 快照");
+    remote.insert_scalars(4, "-r2").expect("远端第二笔");
+    let full = remote.export_snapshot().expect("full 快照");
+
+    // 本地：先收到 base，再在 base 之上编辑——本地 op 因果依赖基线 op。
+    let _ = seed_note(&vault, "note-1", "doc-1");
+    let local = BodyDoc::restore("doc-1", A_PEER, Some(&partial), &[]).expect("本地副本");
+    local.insert_scalars(4, "-local").expect("本地并发编辑");
+    commit_now(&vault, &local, "local-batch");
+
+    let parts = merge_detached(&vault, "doc-1", &full, &local, 6, "cursor-2");
+    let text = assert_detached_matches_attached(&vault, "doc-1", &full, &parts);
+
+    assert_eq!(
+        text.matches("-local").count(),
+        1,
+        "依赖基线的本地 op 必须恰好一次：{text:?}"
+    );
+    assert_eq!(
+        text.matches("-r2").count(),
+        1,
+        "远端并发 op 必须恰好一次：{text:?}"
+    );
+    assert_eq!(
+        text.matches("base").count(),
+        1,
+        "基线文本必须恰好一次：{text:?}"
+    );
+    assert_eq!(
+        text.len(),
+        "base".len() + "-local".len() + "-r2".len(),
+        "不得有重复或残留字符：{text:?}"
+    );
+}
+
+// ---------------------------------------------------------------------------
+// 2. LoroTree create / move / delete
+// ---------------------------------------------------------------------------
+
+/// 新建一个 LoroDoc（未启用任何容器配置）。
+fn tree_doc(peer: u64) -> LoroDoc {
+    let doc = LoroDoc::new();
+    doc.set_peer_id(peer).expect("设置 peer id");
+    doc
+}
+
+/// 取树容器并启用 fractional index（与 `StructureDoc::create/restore` 一致）。
+fn editable_tree(doc: &LoroDoc) -> LoroTree {
+    let tree = doc.get_tree("tree");
+    tree.enable_fractional_index(0);
+    tree
+}
+
+fn set_title(tree: &LoroTree, node: TreeID, title: &str) {
+    tree.get_meta(node)
+        .expect("节点 meta 可写")
+        .insert("title", title)
+        .expect("写标题");
+}
+
+fn title_of(tree: &LoroTree, node: TreeID) -> String {
+    let meta = tree.get_meta(node).expect("节点 meta 可读");
+    match meta.get_deep_value() {
+        LoroValue::Map(entries) => entries
+            .get("title")
+            .and_then(|value| match value {
+                LoroValue::String(value) => Some(value.to_string()),
+                _ => None,
+            })
+            .unwrap_or_default(),
+        _ => String::new(),
+    }
+}
+
+/// 树的可读形状：深度优先的「标题（含缩进）」，根顺序也参与比较。
+fn shape(doc: &LoroDoc) -> Vec<String> {
+    let tree = doc.get_tree("tree");
+    let mut out = Vec::new();
+    walk_shape(&tree, None, 0, &mut out);
+    out
+}
+
+fn walk_shape(tree: &LoroTree, parent: Option<TreeID>, depth: usize, out: &mut Vec<String>) {
+    let Some(children) = tree.children(parent) else {
+        return;
+    };
+    for child in children {
+        out.push(format!("{}{}", "  ".repeat(depth), title_of(tree, child)));
+        walk_shape(tree, Some(child), depth + 1, out);
+    }
+}
+
+/// 节点 ID + 父节点 ID（连同深度），用于断言两副本是同一棵树而不是「同形状」。
+fn parent_ids(doc: &LoroDoc) -> Vec<String> {
+    let tree = doc.get_tree("tree");
+    let mut out = Vec::new();
+    walk_ids(&tree, None, 0, &mut out);
+    out
+}
+
+fn walk_ids(tree: &LoroTree, parent: Option<TreeID>, depth: usize, out: &mut Vec<String>) {
+    let Some(children) = tree.children(parent) else {
+        return;
+    };
+    for child in children {
+        let parent_label = parent
+            .map(|parent| parent.to_string())
+            .unwrap_or_else(|| "<root>".to_owned());
+        out.push(format!("{parent_label}|{child}|{depth}"));
+        walk_ids(tree, Some(child), depth + 1, out);
+    }
+}
+
+/// 在树上跑一遍 detached / attached 三条路径，返回 attached 参考文档。
+///
+/// - `snapshot`：本轮要应用的远端基线快照（peer B 导出）；
+/// - `local_delta`：本地相对该快照 VV 的差集；
+/// - `local_vv`：本地副本的 oplog VV（detached 导出的基准）；
+/// - `local_snapshot`：本地副本完整快照（attached 重放的起点）。
+fn assert_tree_paths_agree(
+    peer: u64,
+    snapshot: &[u8],
+    local_delta: &[u8],
+    local_vv: &VersionVector,
+    local_snapshot: &[u8],
+) -> LoroDoc {
+    // 1) attached 参考：基线副本 attached 导入同一条本地差集。
+    let attached = tree_doc(peer);
+    attached.import(snapshot).expect("attached 基线快照");
+    attached.import(local_delta).expect("attached 合并本地差集");
+
+    // 2) detached：先 detach，只把增量写进 oplog，再从 oplog 导出差集。
+    let detached = tree_doc(peer);
+    detached.import(snapshot).expect("detached 基线快照");
+    editable_tree(&detached);
+    detached.detach();
+    assert!(detached.is_detached());
+    detached.import(local_delta).expect("detached import");
+    assert!(
+        detached.is_detached(),
+        "import 不得把 detached 文档翻回 attached"
+    );
+    assert_eq!(
+        detached.oplog_frontiers().encode(),
+        attached.state_frontiers().encode(),
+        "detached 合并 frontier 与 attached 合并 frontier 不一致"
+    );
+    assert_eq!(
+        detached.oplog_frontiers().encode(),
+        attached.oplog_frontiers().encode(),
+        "detached oplog frontier 与 attached oplog frontier 不一致"
+    );
+    assert_eq!(
+        canonical_vv(&detached.oplog_vv().encode()),
+        canonical_vv(&attached.oplog_vv().encode()),
+        "detached 合并 VV 与 attached 合并 VV 不一致"
+    );
+    let delta_from_oplog = detached
+        .export(ExportMode::updates(local_vv))
+        .expect("从 detached oplog 导出差集");
+    assert!(!delta_from_oplog.is_empty(), "本轮 detached 差集必须非空");
+
+    // attach 后读取合并后的树状态（生产从不读 detached 状态，这里只做等价性证据）。
+    detached.attach();
+    assert!(!detached.is_detached());
+    assert_eq!(shape(&detached), shape(&attached), "attach 后树形状不一致");
+    assert_eq!(
+        parent_ids(&detached),
+        parent_ids(&attached),
+        "attach 后树父子关系不一致"
+    );
+
+    // 3) attached 重放：本地副本 + detached 导出的差集（= 库里本地 update + 合并差集）。
+    let replay = tree_doc(peer);
+    replay.import(local_snapshot).expect("重放本地副本");
+    editable_tree(&replay);
+    replay
+        .import(&delta_from_oplog)
+        .expect("重放 detached 差集");
+    assert_eq!(
+        replay.state_frontiers().encode(),
+        attached.state_frontiers().encode(),
+        "attached 重放 frontier 与 attached 合并不一致"
+    );
+    assert_eq!(
+        canonical_vv(&replay.oplog_vv().encode()),
+        canonical_vv(&attached.oplog_vv().encode()),
+        "attached 重放 VV 与 attached 合并不一致"
+    );
+    assert_eq!(
+        shape(&replay),
+        shape(&attached),
+        "attached 重放树形状不一致"
+    );
+    assert_eq!(
+        parent_ids(&replay),
+        parent_ids(&attached),
+        "attached 重放父子关系不一致"
+    );
+
+    attached
+}
+
+#[test]
+fn tree_create_and_move_match_attached_frontier_and_shape() {
+    // 第一份基线（peer B）：Folder / Note 1、根级 Note 2。
+    let remote = tree_doc(B_PEER);
+    let remote_tree = editable_tree(&remote);
+    let folder = remote_tree.create(None).expect("创建 Folder");
+    let note1 = remote_tree.create(folder).expect("创建 Note 1");
+    let note2 = remote_tree.create(None).expect("创建 Note 2");
+    set_title(&remote_tree, folder, "Folder");
+    set_title(&remote_tree, note1, "Note 1");
+    set_title(&remote_tree, note2, "Note 2");
+    remote.commit();
+    let first = remote.export(ExportMode::Snapshot).expect("第一份基线");
+
+    // 本地（peer A）：在基线上并发 create + move（把 Note 2 重挂到本地新节点下）。
+    let local = tree_doc(A_PEER);
+    local.import(&first).expect("本地副本");
+    let local_tree = editable_tree(&local);
+    let local_node = local_tree.create(note1).expect("本地新节点");
+    set_title(&local_tree, local_node, "Local");
+    local_tree.mov(note2, local_node).expect("本地移动");
+    local.commit();
+    let local_snapshot = local.export(ExportMode::Snapshot).expect("本地快照");
+    let local_vv = local.oplog_vv();
+
+    // 远端在本地编辑期间继续前进：新建根节点 Remote（第二轮 frontier 的来源）。
+    let remote_node = remote_tree.create(None).expect("远端新根");
+    set_title(&remote_tree, remote_node, "Remote");
+    remote.commit();
+    let snapshot = remote.export(ExportMode::Snapshot).expect("第二份基线");
+    let baseline_vv = remote.oplog_vv();
+
+    let local_delta = local
+        .export(ExportMode::updates(&baseline_vv))
+        .expect("本地差集");
+    assert!(!local_delta.is_empty(), "本地差集不能为空");
+
+    let attached =
+        assert_tree_paths_agree(A_PEER, &snapshot, &local_delta, &local_vv, &local_snapshot);
+
+    // 可见形状硬断言：Folder > Note 1 > Local > Note 2，远端新根 Remote 在根级末尾。
+    assert_eq!(
+        shape(&attached),
+        vec![
+            "Folder".to_owned(),
+            "  Note 1".to_owned(),
+            "    Local".to_owned(),
+            "      Note 2".to_owned(),
+            "Remote".to_owned(),
+        ]
+    );
+}
+
+#[test]
+fn tree_delete_and_reparent_match_attached_frontier_and_shape() {
+    // 第一份基线（peer B）：Folder / Note 1（本地会删），根级 Note 2、Note 3（本地会重挂）。
+    let remote = tree_doc(B_PEER);
+    let remote_tree = editable_tree(&remote);
+    let folder = remote_tree.create(None).expect("创建 Folder");
+    let note1 = remote_tree.create(folder).expect("创建 Note 1");
+    let note2 = remote_tree.create(None).expect("创建 Note 2");
+    let note3 = remote_tree.create(None).expect("创建 Note 3");
+    set_title(&remote_tree, folder, "Folder");
+    set_title(&remote_tree, note1, "Note 1");
+    set_title(&remote_tree, note2, "Note 2");
+    set_title(&remote_tree, note3, "Note 3");
+    remote.commit();
+    let first = remote.export(ExportMode::Snapshot).expect("第一份基线");
+
+    // 本地（peer A）：并发 delete + create + reparent。
+    let local = tree_doc(A_PEER);
+    local.import(&first).expect("本地副本");
+    let local_tree = editable_tree(&local);
+    local_tree.delete(note1).expect("本地删除 Note 1");
+    assert!(
+        local_tree.is_node_deleted(&note1).expect("删除标记可读"),
+        "本地删除后 Note 1 必须是删除态"
+    );
+    let local_node = local_tree.create(note2).expect("本地新节点");
+    set_title(&local_tree, local_node, "Local");
+    local_tree.mov(note3, local_node).expect("本地重挂 Note 3");
+    local.commit();
+    let local_snapshot = local.export(ExportMode::Snapshot).expect("本地快照");
+    let local_vv = local.oplog_vv();
+
+    // 远端继续：新建根节点 Remote。
+    let remote_node = remote_tree.create(None).expect("远端新根");
+    set_title(&remote_tree, remote_node, "Remote");
+    remote.commit();
+    let snapshot = remote.export(ExportMode::Snapshot).expect("第二份基线");
+    let baseline_vv = remote.oplog_vv();
+
+    let local_delta = local
+        .export(ExportMode::updates(&baseline_vv))
+        .expect("本地差集");
+    assert!(!local_delta.is_empty(), "本地差集不能为空");
+
+    let attached =
+        assert_tree_paths_agree(A_PEER, &snapshot, &local_delta, &local_vv, &local_snapshot);
+
+    // 可见形状硬断言：Note 1 被删（Folder 无子），Note 2 > Local > Note 3，Remote 在根级末尾。
+    assert_eq!(
+        shape(&attached),
+        vec![
+            "Folder".to_owned(),
+            "Note 2".to_owned(),
+            "  Local".to_owned(),
+            "    Note 3".to_owned(),
+            "Remote".to_owned(),
+        ]
+    );
+    assert!(
+        attached
+            .get_tree("tree")
+            .is_node_deleted(&note1)
+            .expect("删除标记可读"),
+        "合并后 Note 1 必须仍是删除态"
+    );
+}
+
+// ---------------------------------------------------------------------------
+// 3. 本地在 baseline 导出之后继续编辑
+// ---------------------------------------------------------------------------
+
+#[test]
+fn local_edits_after_baseline_export_survive_exactly_once() {
+    let (_dir, vault) = fresh_vault();
+    let remote = memory_doc("doc-1", B_PEER, "Hello world");
+    let first = remote.export_snapshot().expect("第一份基线");
+
+    // 本地在 S0 之后继续写（首尾各一笔），远端也在 S0 之后写（中间一笔）。
+    let _ = seed_note(&vault, "note-1", "doc-1");
+    let local = BodyDoc::restore("doc-1", A_PEER, Some(&first), &[]).expect("本地副本");
+    local.insert_scalars(0, ">> ").expect("本地行首编辑");
+    local
+        .insert_scalars(local.scalar_len(), "!")
+        .expect("本地行尾编辑");
+    commit_now(&vault, &local, "local-batch");
+
+    remote.insert_scalars(5, " MID").expect("远端第二笔");
+    let full = remote.export_snapshot().expect("第二份基线");
+
+    let parts = merge_detached(&vault, "doc-1", &full, &local, 6, "cursor-2");
+    let text = assert_detached_matches_attached(&vault, "doc-1", &full, &parts);
+
+    assert_eq!(
+        text, ">> Hello MID world!",
+        "并发的本地首尾编辑必须保留且只保留一次：{text:?}"
+    );
+    assert_eq!(text.matches(">> ").count(), 1);
+    assert_eq!(text.matches(" MID").count(), 1);
+    assert_eq!(text.matches('!').count(), 1);
+    assert_eq!(text.matches("Hello").count(), 1);
+    assert_eq!(text.len(), 19);
+}
+
+#[test]
+fn second_detached_merge_keeps_both_local_writes_once() {
+    let (_dir, vault) = fresh_vault();
+    let local = seed_note(&vault, "note-1", "doc-1");
+    local.insert_scalars(0, "-L1").expect("本地第一笔");
+    commit_now(&vault, &local, "local-round-1");
+
+    let remote = memory_doc("doc-1", B_PEER, "BASE");
+    let first = remote.export_snapshot().expect("第一份基线");
+
+    let parts = merge_detached(&vault, "doc-1", &first, &local, 5, "cursor-1");
+    let first_text = assert_detached_matches_attached(&vault, "doc-1", &first, &parts);
+    assert_eq!(first_text.matches("-L1").count(), 1);
+    assert_eq!(first_text.matches("BASE").count(), 1);
+
+    // 第一轮 finish；完成后再 begin 第二轮（第二次 checkpoint 追赶）。
+    let finish = finish_baseline_apply(&vault, 5, "cursor-1").expect("finish 第一轮");
+    assert!(
+        matches!(finish, BaselineApplyFinish::Applied(_)),
+        "第一轮 finish 应成功：{finish:?}"
+    );
+
+    // 合并态上继续本地写：生产路径就是重放合并结果再编辑。
+    let (merged, _head) = load_body_doc(&vault, "doc-1", A_PEER, None).expect("第一轮合并态");
+    merged
+        .insert_scalars(merged.scalar_len(), "-L2")
+        .expect("本地第二笔");
+    commit_now(&vault, &merged, "local-round-2");
+
+    // 远端也继续前进：第二轮 detached 差集必须非空（否则测不到 exported 差集）。
+    remote.insert_scalars(0, "R2-").expect("远端第二笔");
+    let second = remote.export_snapshot().expect("第二份基线");
+
+    let parts = merge_detached(&vault, "doc-1", &second, &merged, 9, "cursor-2");
+    let text = assert_detached_matches_attached(&vault, "doc-1", &second, &parts);
+
+    assert_eq!(
+        text.matches("-L1").count(),
+        1,
+        "第一笔本地编辑必须恰好一次：{text:?}"
+    );
+    assert_eq!(
+        text.matches("-L2").count(),
+        1,
+        "第二笔本地编辑必须恰好一次：{text:?}"
+    );
+    assert_eq!(text.matches("BASE").count(), 1);
+    assert_eq!(text.matches("R2-").count(), 1);
+    assert!(text.ends_with("-L2"), "本地追加必须仍在末尾：{text:?}");
+    assert_eq!(
+        text.len(),
+        "BASE".len() + "R2-".len() + "-L1".len() + "-L2".len(),
+        "不得有重复或残留字符：{text:?}"
+    );
+}
+
+// ---------------------------------------------------------------------------
+// 4. 边界：空文档 / 单字符 / emoji 与组合字符
+// ---------------------------------------------------------------------------
+
+#[test]
+fn empty_baseline_text_merges_to_local_text_only() {
+    let (_dir, vault) = fresh_vault();
+    // 基线：插入再删除——正文为空，但 oplog 非空，走的是 detached 合并而不是「新文档」分支。
+    let remote = BodyDoc::create("doc-1", B_PEER).expect("远端基线");
+    remote.insert_scalars(0, "x").expect("基线插入");
+    remote.delete_scalars(0, 1).expect("基线删除");
+    assert_eq!(remote.text(), "");
+    let snapshot = remote.export_snapshot().expect("基线快照");
+
+    let local = seed_note(&vault, "note-1", "doc-1");
+    local.insert_scalars(0, "local").expect("本地编辑");
+    commit_now(&vault, &local, "local-batch");
+
+    let parts = merge_detached(&vault, "doc-1", &snapshot, &local, 7, "cursor-3");
+    let text = assert_detached_matches_attached(&vault, "doc-1", &snapshot, &parts);
+
+    assert_eq!(text, "local", "空基线不得留下任何可见字符：{text:?}");
+    assert_eq!(text.len(), 5);
+}
+
+#[test]
+fn local_doc_emptied_by_delete_merges_to_baseline_text() {
+    let (_dir, vault) = fresh_vault();
+    let remote = memory_doc("doc-1", B_PEER, "baseline");
+    let snapshot = remote.export_snapshot().expect("基线快照");
+
+    // 本地写过再全部删掉：正文为空但 oplog 非空。
+    let local = seed_note(&vault, "note-1", "doc-1");
+    local.insert_scalars(0, "tmp").expect("本地临时文本");
+    local.delete_scalars(0, 3).expect("本地清空");
+    assert_eq!(local.text(), "");
+    commit_now(&vault, &local, "local-batch");
+
+    let parts = merge_detached(&vault, "doc-1", &snapshot, &local, 8, "cursor-4");
+    let text = assert_detached_matches_attached(&vault, "doc-1", &snapshot, &parts);
+
+    assert_eq!(text, "baseline", "本地清空不得删掉基线字符：{text:?}");
+    assert_eq!(text.len(), 8);
+}
+
+#[test]
+fn single_char_docs_keep_both_chars_after_detached_merge() {
+    let (_dir, vault) = fresh_vault();
+    let remote = memory_doc("doc-1", B_PEER, "a");
+    let snapshot = remote.export_snapshot().expect("基线快照");
+
+    let local = seed_note(&vault, "note-1", "doc-1");
+    local.insert_scalars(0, "b").expect("本地单字符");
+    commit_now(&vault, &local, "local-batch");
+
+    let parts = merge_detached(&vault, "doc-1", &snapshot, &local, 9, "cursor-5");
+    let text = assert_detached_matches_attached(&vault, "doc-1", &snapshot, &parts);
+
+    assert_eq!(text.len(), 2, "两个单字符必须都在：{text:?}");
+    assert_eq!(text.chars().count(), 2);
+    assert_eq!(text.matches('a').count(), 1, "基线字符丢失/重复：{text:?}");
+    assert_eq!(text.matches('b').count(), 1, "本地字符丢失/重复：{text:?}");
+}
+
+#[test]
+fn emoji_and_combining_mark_preserved_byte_exact() {
+    let (_dir, vault) = fresh_vault();
+    let remote = memory_doc("doc-1", B_PEER, "😀");
+    let snapshot = remote.export_snapshot().expect("基线快照");
+
+    let local = seed_note(&vault, "note-1", "doc-1");
+    local
+        .insert_scalars(0, "e\u{301}")
+        .expect("本地组合字符（e + U+0301）");
+    commit_now(&vault, &local, "local-batch");
+
+    let parts = merge_detached(&vault, "doc-1", &snapshot, &local, 10, "cursor-6");
+    let text = assert_detached_matches_attached(&vault, "doc-1", &snapshot, &parts);
+
+    assert_eq!(
+        text.chars().count(),
+        3,
+        "😀 + e + ◌́ = 3 个 scalar：{text:?}"
+    );
+    assert_eq!(
+        text.len(),
+        4 + 3,
+        "UTF-8 字节数必须原样保留（😀 4B + e 1B + U+0301 2B）：{text:?}"
+    );
+    assert_eq!(text.matches("😀").count(), 1, "emoji 丢失/重复：{text:?}");
+    assert_eq!(
+        text.matches("e\u{301}").count(),
+        1,
+        "组合字符不得被拆开：{text:?}"
+    );
+    assert!(
+        !text.contains('\u{fffd}'),
+        "不得出现替换字符（说明按字节截断过）：{text:?}"
+    );
+}
+
+#[test]
+fn precomposed_vs_decomposed_accent_merges_without_normalization() {
+    let (_dir, vault) = fresh_vault();
+    // 远端用预组合 é（U+00E9，2B），本地用分解 e + U+0301（3B）：同形不同字节。
+    let remote = memory_doc("doc-1", B_PEER, "é");
+    let snapshot = remote.export_snapshot().expect("基线快照");
+
+    let local = seed_note(&vault, "note-1", "doc-1");
+    local.insert_scalars(0, "e\u{301}").expect("本地分解形式");
+    commit_now(&vault, &local, "local-batch");
+
+    let parts = merge_detached(&vault, "doc-1", &snapshot, &local, 11, "cursor-7");
+    let text = assert_detached_matches_attached(&vault, "doc-1", &snapshot, &parts);
+
+    assert_eq!(text.len(), 2 + 3, "两种编码的字节都要保留：{text:?}");
+    assert_eq!(text.chars().count(), 3);
+    assert_eq!(text.matches('é').count(), 1, "预组合形式丢失：{text:?}");
+    assert_eq!(
+        text.matches("e\u{301}").count(),
+        1,
+        "分解形式丢失：{text:?}"
+    );
+    assert_ne!(text, "éé", "不得做 NFC 归一化：{text:?}");
+    assert_ne!(text, "e\u{301}e\u{301}", "不得做 NFD 归一化：{text:?}");
+}
diff --git a/crates/storage/tests/exec_transaction_busy.rs b/crates/storage/tests/exec_transaction_busy.rs
new file mode 100644
index 0000000..21877f7
--- /dev/null
+++ b/crates/storage/tests/exec_transaction_busy.rs
@@ -0,0 +1,170 @@
+//! P2-7：`exec_transaction` 用 `BEGIN IMMEDIATE` 开启事务（与批量 inbox 共用
+//! `begin_immediate`）。
+//!
+//! 为什么必须是 Immediate：`exec_transaction` 承载 A 事务/资产/repack/finish，调用方
+//! 习惯把写语句放在最前面，但**没有任何断言**保证这一点；只要事务里出现"先读后写"，
+//! deferred 的锁升级就不走 `busy_timeout`（SQLite 的锁升级不调用 busy handler），会在
+//! 预期之外立即 `SQLITE_BUSY`。
+//!
+//! 这些用例锁住外部可观测的后果：并发写者占着写锁时整笔事务失败、零半写、锁释放后
+//! 同一批语句可重放且守卫照旧生效。
+
+mod support;
+
+use support::*;
+
+use menghunote_storage::{AffectedGuard, Statement, StorageError, Value, Vault};
+
+/// 断言错误是 `SQLITE_BUSY`（而不是守卫失败 / 约束冲突）。
+fn assert_busy(error: &StorageError) {
+    assert!(
+        matches!(
+            error,
+            StorageError::Sqlite(rusqlite::Error::SqliteFailure(code, _))
+                if code.code == rusqlite::ErrorCode::DatabaseBusy
+        ),
+        "并发写事务在场时必须整笔 SQLITE_BUSY，实际 {error:?}"
+    );
+}
+
+/// 普通 `meta` 键的 BLOB 值（测试断言用）。
+fn meta_blob(vault: &Vault, key: &str) -> Option<Vec<u8>> {
+    vault.get_meta(key).expect("读 meta")
+}
+
+/// 外部写事务占着写锁时，`exec_transaction` 必须整笔 `SQLITE_BUSY` 且零半写；
+/// 释放后同一批语句重放成功，语句顺序与守卫语义都不变。
+#[test]
+fn exec_transaction_under_foreign_write_lock_fails_busy_without_partial_write() {
+    let (dir, vault) = fresh_vault();
+    vault.set_meta("tx.cas", b"old").expect("预置 CAS 目标行");
+
+    // 本连接不等锁（生产是 5s）：让 BEGIN IMMEDIATE 的失败立刻返回，测试无需 sleep。
+    vault
+        .exec_batch("PRAGMA busy_timeout=0")
+        .expect("busy_timeout=0");
+
+    // 第二个 Vault（= 第二个连接）先占住写锁。
+    let holder = open_vault(&vault_path(&dir));
+    holder
+        .exec_batch("BEGIN IMMEDIATE")
+        .expect("外部连接占写锁");
+
+    // 第一条插入一行（半写的证据），第二条 CAS 更新既有行（守卫要求恰好 1 行）。
+    let statements = [
+        Statement::new(
+            "INSERT INTO meta(key,value) VALUES(?1,?2)",
+            vec![
+                Value::Text("tx.half".to_owned()),
+                Value::Blob(b"1".to_vec()),
+            ],
+        ),
+        Statement::new(
+            "UPDATE meta SET value=?2 WHERE key=?1",
+            vec![
+                Value::Text("tx.cas".to_owned()),
+                Value::Blob(b"new".to_vec()),
+            ],
+        ),
+    ];
+    let guards = [AffectedGuard::exactly(0, 1), AffectedGuard::exactly(1, 1)];
+
+    let error = vault
+        .exec_transaction(&statements, &guards)
+        .expect_err("写锁被占时必须整笔失败");
+    assert_busy(&error);
+
+    holder.exec_batch("ROLLBACK").expect("释放外部写锁");
+    drop(holder);
+
+    assert_eq!(meta_blob(&vault, "tx.half"), None, "失败事务不得留下半写");
+    assert_eq!(
+        meta_blob(&vault, "tx.cas"),
+        Some(b"old".to_vec()),
+        "失败事务不得改动 CAS 目标"
+    );
+
+    // 锁释放后同一批重放：整笔提交，每句影响行数与顺序都不变。
+    let affected = vault
+        .exec_transaction(&statements, &guards)
+        .expect("锁释放后应成功");
+    assert_eq!(affected, vec![1, 1], "语句顺序与每句影响行数必须保持原样");
+    assert_eq!(meta_blob(&vault, "tx.half"), Some(b"1".to_vec()));
+    assert_eq!(meta_blob(&vault, "tx.cas"), Some(b"new".to_vec()));
+}
+
+/// `exec_transaction` 在 `BEGIN` 阶段就取写锁（`BEGIN IMMEDIATE`），不是等第一条语句。
+///
+/// 有写语句时 deferred 也会在第一条语句撞上同样的 `SQLITE_BUSY`（`busy_timeout=0`），
+/// 两种事务模式的结果无法区分；**空语句事务**是唯一能从外部观测到的差异：
+///
+/// - deferred：`BEGIN` 不碰写锁，`COMMIT` 空事务也成功 → `Ok(vec![])`；
+/// - Immediate：`BEGIN` 就要写锁 → 占锁期间直接 `SQLITE_BUSY`。
+///
+/// 这也解释了为什么上面的"零半写"用例本身不足以证明模式切换。
+#[test]
+fn exec_transaction_takes_write_lock_at_begin() {
+    let (dir, vault) = fresh_vault();
+    vault
+        .exec_batch("PRAGMA busy_timeout=0")
+        .expect("busy_timeout=0");
+
+    let holder = open_vault(&vault_path(&dir));
+    holder
+        .exec_batch("BEGIN IMMEDIATE")
+        .expect("外部连接占写锁");
+
+    let error = vault
+        .exec_transaction(&[], &[])
+        .expect_err("占锁期间 BEGIN IMMEDIATE 必须失败");
+    assert_busy(&error);
+
+    holder.exec_batch("ROLLBACK").expect("释放外部写锁");
+    drop(holder);
+
+    assert_eq!(
+        vault
+            .exec_transaction(&[], &[])
+            .expect("释放后空事务应成功"),
+        Vec::<usize>::new(),
+        "空事务返回空的受影响行数列表"
+    );
+}
+
+/// 占锁期间守卫错误也不该抢先暴露：整笔事务在 `BEGIN` 就失败，不会执行任何语句。
+#[test]
+fn exec_transaction_lock_failure_precedes_statement_effects() {
+    let (dir, vault) = fresh_vault();
+    vault.set_meta("tx.cas", b"old").expect("预置 CAS 目标行");
+    vault
+        .exec_batch("PRAGMA busy_timeout=0")
+        .expect("busy_timeout=0");
+
+    let holder = open_vault(&vault_path(&dir));
+    holder
+        .exec_batch("BEGIN IMMEDIATE")
+        .expect("外部连接占写锁");
+
+    // 守卫期望 0 行，但语句实际会影响 1 行：若事务能开始，会先写入再报 GuardFailed。
+    let statements = [Statement::new(
+        "UPDATE meta SET value=?2 WHERE key=?1",
+        vec![
+            Value::Text("tx.cas".to_owned()),
+            Value::Blob(b"new".to_vec()),
+        ],
+    )];
+    let guards = [AffectedGuard::exactly(0, 0)];
+    let error = vault
+        .exec_transaction(&statements, &guards)
+        .expect_err("写锁被占时必须整笔失败");
+    assert_busy(&error);
+
+    holder.exec_batch("ROLLBACK").expect("释放外部写锁");
+    drop(holder);
+
+    assert_eq!(
+        meta_blob(&vault, "tx.cas"),
+        Some(b"old".to_vec()),
+        "失败事务不得写入任何字节"
+    );
+}
diff --git a/crates/storage/tests/journal.rs b/crates/storage/tests/journal.rs
index 2c2d03a..f2dcceb 100644
--- a/crates/storage/tests/journal.rs
+++ b/crates/storage/tests/journal.rs
@@ -1,13 +1,21 @@
 //! 未 ACK journal 与背压：连续前缀、durable 后裁剪、慢盘合并、失败不丢已 ACK 部分。
+//!
+//! 另覆盖 P1-3 的 head 过期契约（见 `crates/storage/src/journal.rs` 模块头）：
+//! 外部推进 head 后 flush 必须返回**可识别**的终态错误而不是裸 `HeadConflict` 或挂死；
+//! 按契约重建 sink 后新条目可正常 durable；旧 journal 未 durable 条目被**显式交还**，
+//! 既不写库也不静默丢弃。
 
 mod support;
 
+use std::sync::atomic::{AtomicUsize, Ordering};
+use std::sync::Arc;
+
 use loro::VersionVector;
 use support::*;
 
 use menghunote_storage::{
-    load_body_doc, load_head, DurableSink, FlushReport, JournalEntry, JournalOrigin, JournalWriter,
-    StorageError, VaultJournalSink,
+    append_updates, load_body_doc, load_head, DurableSink, FlushReport, JournalEntry,
+    JournalOrigin, JournalWriter, StorageError, UpdateRecord, VaultJournalSink,
 };
 
 /// 可注入慢盘/失败的 sink：记录每次 flush 的批大小，便于断言「合并」。
@@ -48,6 +56,28 @@ impl DurableSink for ScriptedSink {
         }
         Ok(self.seq)
     }
+
+    fn head_seq(&self) -> u64 {
+        self.seq
+    }
+}
+
+/// 计数包装：证明 stale 之后 writer **不再**触碰 sink（不重试）。
+#[derive(Debug)]
+struct CountingSink {
+    inner: VaultJournalSink,
+    attempts: Arc<AtomicUsize>,
+}
+
+impl DurableSink for CountingSink {
+    fn write_prefix(&mut self, entries: &[JournalEntry]) -> Result<u64, StorageError> {
+        self.attempts.fetch_add(1, Ordering::SeqCst);
+        self.inner.write_prefix(entries)
+    }
+
+    fn head_seq(&self) -> u64 {
+        self.inner.head_seq()
+    }
 }
 
 fn entry(tag: &str) -> JournalEntry {
@@ -282,3 +312,197 @@ fn sink_holds_head_watermark_and_rejects_mixed_docs() {
     let error = sink.write_prefix(&mixed).expect_err("混文档必须拒绝");
     assert!(matches!(error, StorageError::Corrupt { .. }));
 }
+
+/// P1-3：外部推进 head 后，flush 必须给出**可识别**的终态错误，不重试、不挂死，
+/// 且旧条目既没写库也没被静默丢掉；按契约重建 sink 后新条目可正常 durable。
+#[test]
+fn stale_head_is_terminal_then_rebuild_hands_back_entries_and_new_entries_flush() {
+    let (_dir, vault) = fresh_vault();
+    let doc = seed_note(&vault, "note-1", "doc-1");
+
+    // 本地编辑：按**空基准**导出，这就是 writer 手里的字节（追加时的 head 快照）。
+    doc.insert_scalars(0, "甲").expect("编辑");
+    let local = doc
+        .pending_update(&VersionVector::default())
+        .expect("导出本地增量");
+
+    // writer 先按 head 0 建好 sink 并提交条目（真实路径同序：sink 先于外部推进）。
+    let attempts = Arc::new(AtomicUsize::new(0));
+    let sink = CountingSink {
+        inner: VaultJournalSink::new(vault.clone(), "doc-1", 0),
+        attempts: Arc::clone(&attempts),
+    };
+    let mut writer = JournalWriter::new(sink, 4, 4);
+    assert_eq!(writer.durable_seq(), 0, "空 journal 时报告 sink 的真实水位");
+    writer
+        .submit(JournalEntry {
+            doc_id: "doc-1".to_owned(),
+            batch_id: "stale-batch".to_owned(),
+            update: local.clone(),
+            origin: JournalOrigin::Local,
+        })
+        .expect("submit");
+
+    // 外部 writer（同步引擎 integrate / 另一个 Vault）在同一 doc 上推进 head。
+    doc.insert_scalars(doc.scalar_len(), "乙").expect("编辑");
+    let external = doc
+        .pending_update(&VersionVector::default())
+        .expect("导出外部增量");
+    let advanced = append_updates(
+        &vault,
+        "doc-1",
+        0,
+        &[UpdateRecord::new("external-1", external)],
+    )
+    .expect("外部推进 head");
+    assert_eq!(advanced.head_seq, 1);
+
+    // 1) 第一次 flush：终态错误，而不是裸 HeadConflict。
+    let error = writer.flush().expect_err("head 已被推进，flush 必须失败");
+    match &error {
+        StorageError::JournalHeadStale {
+            doc_id,
+            expected,
+            actual,
+            pending,
+        } => {
+            assert_eq!(doc_id, "doc-1");
+            assert_eq!(*expected, 0, "错误必须带上写入用的 CAS 基准");
+            assert_eq!(*actual, 1, "错误必须带上库里真实的 head");
+            assert_eq!(*pending, 1, "错误必须说明有多少条未 durable 条目受影响");
+        }
+        other => panic!("期望 JournalHeadStale，实际 {other:?}"),
+    }
+    assert!(
+        !matches!(&error, StorageError::HeadConflict { .. }),
+        "journal 层不得把裸 HeadConflict 漏给调用方"
+    );
+    assert!(
+        error.requires_session_rebuild(),
+        "CAS 过期只能靠重建 session"
+    );
+    assert!(!error.is_retryable(), "CAS 过期不是可重试错误");
+    assert_eq!(writer.pending_len(), 1, "失败不得丢条目");
+    assert_eq!(writer.durable_seq(), 0, "失败不得推进 durable 水位");
+    assert_eq!(writer.flushed_entries(), 0);
+    assert!(writer.is_stale());
+    assert_eq!(
+        attempts.load(Ordering::SeqCst),
+        1,
+        "CAS 冲突只尝试一次，不自动重试"
+    );
+
+    // 2) 再 flush / flush_all：幂等返回同一终态错误，且**不再碰数据库**（不会挂死、不刷屏）。
+    for _ in 0..2 {
+        let again = writer.flush().expect_err("stale 必须继续报错");
+        assert!(
+            matches!(again, StorageError::JournalHeadStale { pending: 1, .. }),
+            "实际 {again:?}"
+        );
+        assert_eq!(
+            attempts.load(Ordering::SeqCst),
+            1,
+            "stale 之后不得重试 sink"
+        );
+    }
+    assert!(writer.flush_all().is_err(), "排空也必须明确失败");
+
+    // 3) stale 期间 submit 被拒绝：writer 已冻结，新条目必须交给重建后的 writer。
+    let blocked = writer
+        .submit(JournalEntry::local("doc-1", "later", local.clone()))
+        .expect_err("stale 时 submit 必须被拒绝");
+    assert!(
+        matches!(blocked, StorageError::JournalHeadStale { pending: 1, .. }),
+        "实际 {blocked:?}"
+    );
+    assert_eq!(writer.pending_len(), 1, "被拒的条目不能进 journal");
+
+    // 4) 按契约重建 sink（新 head_seq）：旧条目**显式交还**，journal 清空，水位对齐新基准。
+    let handed_back = writer
+        .rebuild_sink(CountingSink {
+            inner: VaultJournalSink::new(vault.clone(), "doc-1", 1),
+            attempts: Arc::clone(&attempts),
+        })
+        .expect("stale 后重建 sink");
+    assert_eq!(handed_back.len(), 1, "旧条目必须原样交还，不得静默丢弃");
+    assert_eq!(handed_back[0].batch_id, "stale-batch");
+    assert_eq!(handed_back[0].update, local);
+    assert_eq!(writer.pending_len(), 0, "交还后 journal 清空");
+    assert!(!writer.is_stale());
+    assert_eq!(writer.durable_seq(), 1, "重建后水位 = 新 sink 的 CAS 基准");
+
+    // 旧条目没有被偷偷写进库：库里仍只有外部那一条。
+    assert_eq!(
+        count(
+            &vault,
+            "SELECT count(*) FROM loro_updates WHERE doc_id = 'doc-1'",
+            &[]
+        ),
+        1,
+        "过期条目不得写入 loro_updates"
+    );
+    let (replayed, _) = load_body_doc(&vault, "doc-1", PEER_A, None).expect("重放");
+    assert_eq!(replayed.text(), "甲乙", "只有外部那一条落库");
+
+    // 5) 重建：用新 head 的 VV 重新导出（会话层的 ③），新条目 flush 成功且水位正确。
+    let base = advanced.version_vector().expect("解码新 head 的 VV");
+    doc.insert_scalars(doc.scalar_len(), "丙").expect("编辑");
+    let fresh = doc.pending_update(&base).expect("按新 head 重新导出增量");
+    writer
+        .submit(JournalEntry {
+            doc_id: "doc-1".to_owned(),
+            batch_id: "fresh-1".to_owned(),
+            update: fresh,
+            origin: JournalOrigin::Local,
+        })
+        .expect("重建后可 submit");
+
+    let report = writer.flush().expect("重建后的新条目必须能 flush");
+    assert_eq!(
+        report,
+        FlushReport {
+            entries: 1,
+            durable_seq: 2
+        }
+    );
+    assert_eq!(writer.durable_seq(), 2);
+    assert_eq!(writer.flushed_entries(), 1);
+    assert_eq!(writer.pending_len(), 0);
+    assert_eq!(attempts.load(Ordering::SeqCst), 2, "重建后只多一次真实写入");
+    assert_eq!(
+        count(
+            &vault,
+            "SELECT count(*) FROM loro_updates WHERE doc_id = 'doc-1'",
+            &[]
+        ),
+        2
+    );
+    assert_eq!(load_head(&vault, "doc-1").expect("head").head_seq, 2);
+    let (replayed, _) = load_body_doc(&vault, "doc-1", PEER_A, None).expect("重放");
+    assert_eq!(replayed.text(), "甲乙丙", "重建 + 重导出的内容完整");
+}
+
+/// 未过期时不允许重建 sink：那会把仍可写入的条目丢掉（契约的反向保护）。
+#[test]
+fn rebuild_sink_is_rejected_while_not_stale() {
+    let (_dir, vault) = fresh_vault();
+    let doc = seed_note(&vault, "note-1", "doc-1");
+    doc.insert_scalars(0, "甲").expect("编辑");
+    let update = doc.pending_update(&VersionVector::default()).expect("增量");
+
+    let mut writer = JournalWriter::new(VaultJournalSink::new(vault.clone(), "doc-1", 0), 4, 4);
+    writer
+        .submit(JournalEntry::local("doc-1", "b1", update))
+        .expect("submit");
+    assert!(!writer.is_stale());
+
+    let error = writer
+        .rebuild_sink(VaultJournalSink::new(vault.clone(), "doc-1", 0))
+        .expect_err("未 stale 时重建必须被拒绝");
+    assert!(
+        matches!(error, StorageError::Corrupt { .. }),
+        "实际 {error:?}"
+    );
+    assert_eq!(writer.pending_len(), 1, "拒绝重建不得动 journal");
+    assert_eq!(writer.flush().expect("正常 flush 仍可用").entries, 1);
+}
diff --git a/crates/storage/tests/meta_value_types.rs b/crates/storage/tests/meta_value_types.rs
new file mode 100644
index 0000000..aabdc95
--- /dev/null
+++ b/crates/storage/tests/meta_value_types.rs
@@ -0,0 +1,283 @@
+//! P2-6：`meta.value` 的三个读取入口必须只有**一套**语义 —— 只认 BLOB。
+//!
+//! | 入口 | 实际代码路径 |
+//! | --- | --- |
+//! | [`menghunote_storage::Vault::get_meta`] | actor 线程上的 `GetMeta` 命令 |
+//! | `sync_bridge::read_meta` | `sync_cursor_state` / `take_inbox_ready` / `set_sync_log_id` 等同步桥读取 |
+//! | `sync_meta::read_meta_on` | worker 独占连接 / 批量 inbox 事务内的读取 |
+//!
+//! 为什么专门测非 BLOB：`meta.value` 是 `BLOB NOT NULL`（`schema/v1.sql` 里的 STRICT
+//! 表），正常写入路径**写不出** TEXT/INTEGER/REAL。能读出来只可能是"库被外部工具改坏
+//! 或 schema 漂移"，而旧行为对它有两种互相矛盾的解读：
+//!
+//! - `Vault::get_meta` 把 TEXT 当字节返回；
+//! - `sync_bridge::read_meta` / `sync_meta::read_meta_on` 把 TEXT 当**缺失**。
+//!
+//! 后者会静默绕过持久屏障与游标：`sync.log_id` 读不到就当"还没绑定 incarnation"
+//! （`take_inbox_ready` 静默返回空，`set_sync_log_id` 还会把损坏值覆盖掉，等于悄悄
+//! 换绑传代），`sync.repack_required` 读不到就当"不需要 repack"。
+//!
+//! 测试库用 `PRAGMA writable_schema` 把 `meta` 表改回非 STRICT，从而如实构造出这种
+//! 损坏状态（STRICT 表拒绝非 BLOB 写入，测试里没有别的办法造出 TEXT）。
+
+mod support;
+
+use std::path::Path;
+
+use support::*;
+
+use menghunote_storage::{
+    advance_integrated_prefix, repack_required, set_repack_required, set_sync_log_id,
+    sync_cursor_state, take_inbox_ready, StorageError, Value, Vault, META_SYNC_LOG_ID,
+    META_SYNC_REPACK_REQUIRED,
+};
+
+/// 测试用的合法 relay log incarnation（16B BLOB）。
+const LOG_ID: [u8; 16] = [0x2f; 16];
+
+/// 把测试库的 `meta` 表从 STRICT 改成非 STRICT。
+///
+/// 只改 `sqlite_master` 里那条 DDL（去掉唯一的 `STRICT` 后缀），其余列定义逐字保留；
+/// 之后的连接会按非 STRICT 解析这张表，于是能写入 TEXT/INTEGER/REAL —— 正是"库被外部
+/// 改坏"的形状。
+fn relax_meta_table(path: &Path) {
+    let conn = rusqlite::Connection::open(path).expect("打开原始连接");
+    conn.execute_batch("PRAGMA writable_schema=ON")
+        .expect("打开 writable_schema");
+    let ddl: String = conn
+        .query_row(
+            "SELECT sql FROM sqlite_master WHERE name='meta'",
+            [],
+            |row| row.get(0),
+        )
+        .expect("读 meta 表 DDL");
+    let relaxed = ddl.replacen(" STRICT", "", 1);
+    assert_ne!(relaxed, ddl, "meta 表 DDL 应带 STRICT：{ddl}");
+    conn.execute(
+        "UPDATE sqlite_master SET sql=?1 WHERE name='meta'",
+        [relaxed],
+    )
+    .expect("改写 meta 表 DDL");
+    conn.execute_batch("PRAGMA writable_schema=OFF")
+        .expect("关闭 writable_schema");
+    drop(conn);
+}
+
+/// 建一个 vault，把 `meta` 表改成非 STRICT 后重新打开。
+///
+/// 重新打开走 [`Vault::open`]：`user_version` 已是 `SCHEMA_VERSION`，迁移不会重跑，
+/// 所以非 STRICT 的 `meta` 表被原样沿用。
+fn vault_with_typed_meta() -> (tempfile::TempDir, Vault) {
+    let dir = temp_dir();
+    let path = vault_path(&dir);
+    {
+        let vault = open_vault(&path);
+        // 先写一版合法 BLOB，确保 schema 与 WAL 都已落盘，再改表结构。
+        set_sync_log_id(&vault, &LOG_ID).expect("预置合法 BLOB");
+    }
+    relax_meta_table(&path);
+    let vault = open_vault(&path);
+    (dir, vault)
+}
+
+/// 把一个 meta 键写成指定 SQLite 类型（绕过 Vault 的 BLOB 写入路径）。
+fn write_typed_meta(vault: &Vault, key: &str, value: Value) {
+    vault
+        .exec(
+            "INSERT INTO meta(key,value) VALUES(?1,?2) \
+             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
+            &[Value::Text(key.to_owned()), value],
+        )
+        .expect("写入指定类型的 meta");
+}
+
+/// `meta.value` 的实际 SQLite 类型名（`typeof()`，用于确认构造出的损坏状态）。
+fn value_type(vault: &Vault, key: &str) -> String {
+    let rows = vault
+        .query(
+            "SELECT typeof(value) FROM meta WHERE key=?1",
+            &[Value::Text(key.to_owned())],
+        )
+        .expect("读 typeof(value)");
+    match rows.first().and_then(|row| row.first()) {
+        Some(Value::Text(name)) => name.clone(),
+        other => panic!("typeof(value) 类型异常：{other:?}"),
+    }
+}
+
+/// 断言结果是 [`StorageError::Corrupt`]，返回错误上下文（便于继续断言 key 与实际类型）。
+fn corrupt<T: std::fmt::Debug>(result: Result<T, StorageError>, label: &str) -> String {
+    match result {
+        Ok(value) => panic!("{label} 必须报损坏，实际 Ok({value:?})"),
+        Err(StorageError::Corrupt { context }) => context,
+        Err(other) => panic!("{label} 必须是 Corrupt，实际 {other:?}"),
+    }
+}
+
+/// 同一 key 写成 TEXT：三个入口必须给出**同一个**损坏结论。
+#[test]
+fn text_meta_is_corrupt_from_all_three_entry_points() {
+    let (_dir, vault) = vault_with_typed_meta();
+    write_typed_meta(&vault, META_SYNC_LOG_ID, Value::Text("2f".repeat(16)));
+    assert_eq!(
+        value_type(&vault, META_SYNC_LOG_ID),
+        "text",
+        "构造出的值必须是 TEXT"
+    );
+
+    // 入口 1：Vault::get_meta（actor 线程）。旧行为把 TEXT 直接当字节返回。
+    let from_actor = corrupt(vault.get_meta(META_SYNC_LOG_ID), "Vault::get_meta");
+    // 入口 2：sync_bridge::read_meta（sync_cursor_state 读 sync.log_id）。旧行为当缺失。
+    let from_bridge = corrupt(sync_cursor_state(&vault), "sync_bridge::read_meta");
+    // 入口 3：sync_meta::read_meta_on（前缀推进在 worker 连接上读 sync.log_id）。
+    let from_meta = corrupt(advance_integrated_prefix(&vault), "sync_meta::read_meta_on");
+
+    for (label, context) in [
+        ("Vault::get_meta", &from_actor),
+        ("sync_bridge::read_meta", &from_bridge),
+        ("sync_meta::read_meta_on", &from_meta),
+    ] {
+        assert!(
+            context.contains(META_SYNC_LOG_ID),
+            "{label} 的错误信息必须点名 key：{context}"
+        );
+        assert!(
+            context.contains("TEXT") && context.contains("BLOB"),
+            "{label} 的错误信息必须同时给出实际类型与期望类型：{context}"
+        );
+    }
+    // 三处共用同一判定函数，连错误文案都不该有差别。
+    assert_eq!(
+        from_actor, from_bridge,
+        "actor 与 sync_bridge 的结论必须一致"
+    );
+    assert_eq!(from_actor, from_meta, "actor 与 sync_meta 的结论必须一致");
+}
+
+/// BLOB（包括空 BLOB）继续按原语义读出，前缀推进不受影响。
+#[test]
+fn blob_meta_reads_consistently_from_all_three_entry_points() {
+    let (_dir, vault) = fresh_vault();
+    set_sync_log_id(&vault, &LOG_ID).expect("写 BLOB sync.log_id");
+
+    assert_eq!(
+        vault.get_meta(META_SYNC_LOG_ID).expect("读 BLOB meta"),
+        Some(LOG_ID.to_vec()),
+        "BLOB 必须逐字节读出"
+    );
+    let state = sync_cursor_state(&vault).expect("读游标状态");
+    assert_eq!(
+        state.log_id,
+        Some(LOG_ID),
+        "sync_bridge 必须读到同一个 BLOB"
+    );
+    assert_eq!(state.received_seq, 0);
+    assert_eq!(state.integrated_seq, 0);
+    assert_eq!(state.confirmed_highwater, None);
+
+    advance_integrated_prefix(&vault).expect("BLOB 下前缀推进是合法 no-op");
+
+    set_repack_required(&vault, true).expect("写 repack 屏障");
+    assert!(
+        repack_required(&vault).expect("读 repack 屏障"),
+        "BLOB 屏障必须生效"
+    );
+    set_repack_required(&vault, false).expect("清 repack 屏障");
+    assert!(!repack_required(&vault).expect("读 repack 屏障"));
+}
+
+/// 缺失的键在三个入口都是"没有"，不是损坏。
+#[test]
+fn missing_meta_reads_as_absent_from_all_three_entry_points() {
+    let (_dir, vault) = fresh_vault();
+
+    assert_eq!(vault.get_meta(META_SYNC_LOG_ID).expect("读缺失 key"), None);
+    let state = sync_cursor_state(&vault).expect("读游标状态");
+    assert_eq!(state.log_id, None);
+    assert_eq!(state.received_seq, 0);
+    assert_eq!(state.integrated_seq, 0);
+    assert_eq!(state.confirmed_highwater, None);
+    advance_integrated_prefix(&vault).expect("缺失时前缀推进是 no-op");
+    assert!(!repack_required(&vault).expect("缺失屏障读成 false"));
+    assert_eq!(
+        vault
+            .get_meta(META_SYNC_REPACK_REQUIRED)
+            .expect("读缺失屏障"),
+        None
+    );
+    assert!(
+        take_inbox_ready(&vault, 8)
+            .expect("没有 incarnation 时 ready 为空")
+            .is_empty(),
+        "缺失 sync.log_id 时 ready 必须为空（不回退成不过滤 log）"
+    );
+}
+
+/// 损坏的 `sync.log_id` 不能被当成"还没绑定 incarnation"静默吞掉。
+///
+/// 这是 P2-6 的实际风险点：旧行为下 `take_inbox_ready` 会静默返回空（pull 永久卡住），
+/// `set_sync_log_id` 会把损坏值覆盖成新值（悄悄换绑传代，旧行混进新代）。
+#[test]
+fn text_log_id_blocks_ready_and_rebinding_instead_of_silently_passing() {
+    let (_dir, vault) = vault_with_typed_meta();
+    write_typed_meta(&vault, META_SYNC_LOG_ID, Value::Text("2f".repeat(16)));
+
+    let context = corrupt(take_inbox_ready(&vault, 8), "take_inbox_ready");
+    assert!(
+        context.contains(META_SYNC_LOG_ID),
+        "错误必须点名 sync.log_id：{context}"
+    );
+    let context = corrupt(set_sync_log_id(&vault, &LOG_ID), "set_sync_log_id");
+    assert!(
+        context.contains(META_SYNC_LOG_ID),
+        "错误必须点名 sync.log_id：{context}"
+    );
+
+    // 损坏值仍在库里：没有被"缺失 → 写入新版"的路径覆盖掉。
+    assert_eq!(value_type(&vault, META_SYNC_LOG_ID), "text");
+}
+
+/// INTEGER / REAL / TEXT 全部按损坏处理（不是只有 TEXT 特殊）。
+#[test]
+fn non_blob_meta_types_are_corrupt() {
+    let (_dir, vault) = vault_with_typed_meta();
+    // NULL 造不出来：`meta.value` 是 NOT NULL，非 STRICT 表同样拒绝。
+    for (value, expected_type) in [
+        (Value::Integer(7), "INTEGER"),
+        (Value::Real(1.5), "REAL"),
+        (Value::Text("7".to_owned()), "TEXT"),
+    ] {
+        write_typed_meta(&vault, META_SYNC_LOG_ID, value);
+        assert_eq!(
+            value_type(&vault, META_SYNC_LOG_ID),
+            expected_type.to_lowercase()
+        );
+        let context = corrupt(vault.get_meta(META_SYNC_LOG_ID), "Vault::get_meta");
+        assert!(
+            context.contains(expected_type),
+            "错误必须报实际类型 {expected_type}：{context}"
+        );
+        let context = corrupt(sync_cursor_state(&vault), "sync_bridge::read_meta");
+        assert!(
+            context.contains(expected_type),
+            "sync_bridge 也必须报实际类型 {expected_type}：{context}"
+        );
+    }
+}
+
+/// 屏障键本身：损坏的 `sync.repack_required` 不得读成"不需要 repack"。
+#[test]
+fn repack_barrier_is_not_silently_false_when_corrupt() {
+    let (_dir, vault) = vault_with_typed_meta();
+    write_typed_meta(
+        &vault,
+        META_SYNC_REPACK_REQUIRED,
+        Value::Text("1".to_owned()),
+    );
+
+    let context = corrupt(repack_required(&vault), "repack_required");
+    assert!(
+        context.contains(META_SYNC_REPACK_REQUIRED),
+        "错误必须点名屏障 key：{context}"
+    );
+}
diff --git a/crates/storage/tests/remote_known.rs b/crates/storage/tests/remote_known.rs
new file mode 100644
index 0000000..26b0ad7
--- /dev/null
+++ b/crates/storage/tests/remote_known.rs
@@ -0,0 +1,191 @@
+//! P2-5 / P2-8：远端已知 VV 的单调写入与「来源标记」语义。
+//!
+//! 这里只走**公开**入口：`VaultJournalSink`（每次 flush 的 JournalOrigin 决定来源）
+//! 与 `append_updates`（全部本地）。远端已知 VV 是 `meta` 里的非破坏性本地证据：
+//!
+//! - 本地写入（含已 ACK 的本地编辑）**不扩大**它；
+//! - 只有远端条目把 `end_vv - 上一条 end_vv` 记进去；
+//! - 没有出站意图的 flush（slots 空）不产生任何 outbox 行。
+
+mod support;
+
+use support::*;
+
+use loro::VersionVector;
+
+use menghunote_storage::{
+    append_updates, create_body_doc, load_head, sync_cursor_state, BodyDoc, DurableSink,
+    JournalEntry, SyncContext, UpdateRecord, Value, Vault, VaultJournalSink,
+};
+
+const CTX: SyncContext = SyncContext {
+    peer_id: 0xA1CE_0001,
+    key_epoch: 1,
+};
+
+fn remote_known(vault: &Vault, doc_id: &str) -> Option<VersionVector> {
+    let rows = vault
+        .query(
+            "SELECT value FROM meta WHERE key=?1",
+            &[Value::Text(format!("sync.remote_known_vv.{doc_id}"))],
+        )
+        .expect("meta 查询");
+    match rows.first().and_then(|row| row.first()) {
+        Some(Value::Blob(bytes)) => Some(VersionVector::decode(bytes).expect("解码远端已知 VV")),
+        None => None,
+        other => panic!("remote_known 值异常：{other:?}"),
+    }
+}
+
+fn scalar(vault: &Vault, sql: &str) -> i64 {
+    let rows = vault.query(sql, &[]).expect("查询");
+    match rows.first().and_then(|row| row.first()) {
+        Some(Value::Integer(value)) => *value,
+        other => panic!("期望整数结果，实际 {other:?}"),
+    }
+}
+
+/// 本地编辑的 `end_vv`（用于断言「本地写入不扩大远端已知 VV」）。
+fn vv_of(update: &menghunote_storage::PendingUpdate) -> VersionVector {
+    VersionVector::decode(&update.end_vv).expect("解码 end_vv")
+}
+
+fn local_entry(doc_id: &str, peer: u64, text: &str) -> (JournalEntry, VersionVector) {
+    let doc = BodyDoc::create(doc_id, peer).expect("创建 BodyDoc");
+    doc.insert_scalars(0, text).expect("本地编辑");
+    let update = doc.pending_update(&VersionVector::default()).expect("导出");
+    let vv = vv_of(&update);
+    (JournalEntry::local(doc_id, "local-batch", update), vv)
+}
+
+fn remote_entry(
+    doc_id: &str,
+    peer: u64,
+    base: &VersionVector,
+    text: &str,
+) -> (JournalEntry, VersionVector) {
+    let doc = BodyDoc::create(doc_id, peer).expect("创建 BodyDoc");
+    doc.insert_scalars(0, text).expect("远端编辑");
+    let update = doc.pending_update(base).expect("导出远端增量");
+    let vv = vv_of(&update);
+    (JournalEntry::remote(doc_id, "remote-batch", update), vv)
+}
+
+/// 纯本地 flush（显式 `OriginMarks::AllLocal` 对应的公开路径）：不创建远端已知 VV。
+#[test]
+fn local_append_never_creates_remote_known() {
+    let (_dir, vault) = fresh_vault();
+    create_body_doc(&vault, "doc-local").expect("loro_docs 行");
+    let doc = BodyDoc::create("doc-local", PEER_A).expect("创建 BodyDoc");
+    doc.insert_scalars(0, "local only").expect("本地编辑");
+    let current = load_head(&vault, "doc-local").expect("head");
+    let update = doc
+        .pending_update(&current.version_vector().expect("head vv"))
+        .expect("导出");
+
+    append_updates(
+        &vault,
+        "doc-local",
+        current.head_seq,
+        &[UpdateRecord::new("local-batch", update)],
+    )
+    .expect("A 事务");
+
+    assert_eq!(
+        remote_known(&vault, "doc-local"),
+        None,
+        "本地写入即使已经进 outbox 也不能扩大远端已知 VV"
+    );
+    assert_eq!(scalar(&vault, "SELECT count(*) FROM sync_outbox"), 0);
+}
+
+/// 远端条目（PerRecord 中 `true` 的那条）只把真正的远端增量记入远端已知 VV；
+/// 同批的本地条目不记入。
+#[test]
+fn mixed_flush_extends_remote_known_only_for_remote_entries() {
+    let (_dir, vault) = fresh_vault();
+    create_body_doc(&vault, "doc-mixed").expect("loro_docs 行");
+
+    let (local, local_vv) = local_entry("doc-mixed", PEER_A, "local");
+    let (remote, remote_vv) = remote_entry("doc-mixed", PEER_B, &local_vv, "remote");
+    let expected_remote = {
+        // 远端记录带来的 ops = remote_vv - local_vv。
+        let mut added = VersionVector::new();
+        for span in remote_vv.sub_iter(&local_vv) {
+            added.insert(span.peer, span.counter.end);
+        }
+        added
+    };
+
+    let mut sink = VaultJournalSink::new(vault.clone(), "doc-mixed", 0).with_sync_context(CTX);
+    let head = sink.write_prefix(&[local, remote]).expect("混合 flush");
+    assert_eq!(head, 2);
+
+    let known = remote_known(&vault, "doc-mixed").expect("有远端条目就必须写");
+    assert_eq!(known, expected_remote, "只能记入远端那一小段");
+    assert_eq!(
+        scalar(&vault, "SELECT count(*) FROM sync_outbox"),
+        1,
+        "本地条目产生 1 条意图，远端条目绝不能产生意图"
+    );
+    assert_eq!(sync_cursor_state(&vault).expect("cursor").received_seq, 0);
+}
+
+/// 纯远端 flush（`slots` 为空 = 没有出站意图）：扩展远端已知 VV，且不产生 outbox。
+#[test]
+fn remote_only_flush_with_empty_slots_has_no_outbox() {
+    let (_dir, vault) = fresh_vault();
+    create_body_doc(&vault, "doc-remote").expect("loro_docs 行");
+    let (remote, remote_vv) =
+        remote_entry("doc-remote", PEER_B, &VersionVector::default(), "remote");
+
+    // 不带 sync context：slots 就是空切片（= 没有任何意图），过去与
+    // 「remote_marks 空 = 全部本地」共用一个形状；现在来源由 OriginMarks 显式表达。
+    let mut sink = VaultJournalSink::new(vault.clone(), "doc-remote", 0);
+    let head = sink.write_prefix(&[remote]).expect("远端 flush");
+    assert_eq!(head, 1);
+
+    assert_eq!(remote_known(&vault, "doc-remote"), Some(remote_vv));
+    assert_eq!(
+        scalar(&vault, "SELECT count(*) FROM sync_outbox"),
+        0,
+        "slots 空 = 不产生任何出站意图"
+    );
+}
+
+/// 同一 flush 连续两条远端记录：记入的是整段远端增量（`end - 前一条 end`），
+/// 不是只记最后一条的绝对 VV。
+#[test]
+fn consecutive_remote_entries_record_the_whole_remote_delta() {
+    let (_dir, vault) = fresh_vault();
+    create_body_doc(&vault, "doc-remote-2").expect("loro_docs 行");
+
+    let first_doc = BodyDoc::create("doc-remote-2", PEER_B).expect("创建 BodyDoc");
+    first_doc.insert_scalars(0, "one").expect("第一次远端编辑");
+    let first = first_doc
+        .pending_update(&VersionVector::default())
+        .expect("第一段");
+    let first_vv = vv_of(&first);
+
+    first_doc
+        .insert_scalars(first_doc.scalar_len(), " two")
+        .expect("第二次远端编辑");
+    let second = first_doc.pending_update(&first_vv).expect("第二段");
+    let end_vv = vv_of(&second);
+
+    let mut sink = VaultJournalSink::new(vault.clone(), "doc-remote-2", 0);
+    sink.write_prefix(&[
+        JournalEntry::remote("doc-remote-2", "remote-1", first),
+        JournalEntry::remote("doc-remote-2", "remote-2", second),
+    ])
+    .expect("两段远端 flush");
+
+    assert_eq!(
+        remote_known(&vault, "doc-remote-2"),
+        Some(end_vv),
+        "整段远端增量都必须记入，不能只记最后一条"
+    );
+    // 显式断言两条都写了（来源只有 remote，没有任何本地产物）。
+    assert_eq!(scalar(&vault, "SELECT count(*) FROM loro_updates"), 2);
+    assert_eq!(scalar(&vault, "SELECT count(*) FROM sync_outbox"), 0);
+}
diff --git a/crates/storage/tests/session_flush.rs b/crates/storage/tests/session_flush.rs
index 2a99366..ffcf18e 100644
--- a/crates/storage/tests/session_flush.rs
+++ b/crates/storage/tests/session_flush.rs
@@ -5,10 +5,13 @@ mod support;
 use std::time::{Duration, Instant};
 
 use menghunote_storage::session::{
-    DocumentSession, EditOutcome, EditRequest, FlushReason, LiveRev, ScalarEdit, SessionEpoch,
-    SessionEvent, SessionOptions,
+    DocumentSession, EditOutcome, EditRequest, ExternalCommitTicket, FlushReason, GateError,
+    GateState, LiveRev, ScalarEdit, SessionEpoch, SessionEvent, SessionOptions,
+};
+use menghunote_storage::{
+    append_updates, load_body_doc, load_body_state, materialize_body, BodyDoc, UpdateRecord, Value,
+    Vault,
 };
-use menghunote_storage::{load_body_state, materialize_body, BodyDoc, Vault};
 use support::*;
 
 fn options(peer: u64) -> SessionOptions {
@@ -249,3 +252,224 @@ fn remote_event_is_pushed_to_the_queue() {
         .any(|event| matches!(event, SessionEvent::Remote(delta) if delta.to_rev == LiveRev(1))));
     assert!(session.events().is_empty(), "事件队列是取走语义");
 }
+
+/// 把闸门驱到明确的失败态（时间只作为上限，不用 sleep 当收敛判据）。
+fn drive_gate_to_failure(
+    session: &mut DocumentSession,
+    ticket: &ExternalCommitTicket,
+) -> GateError {
+    let deadline = Instant::now() + Duration::from_secs(10);
+    loop {
+        match session.poll_external_commit(ticket) {
+            GateState::Failed(error) => return error,
+            GateState::Flushing | GateState::WaitingIME => std::thread::yield_now(),
+            other => panic!("闸门没有给出明确失败：{other:?}"),
+        }
+        assert!(Instant::now() < deadline, "闸门 10s 内没有给出明确失败");
+    }
+}
+
+/// 读 flusher 失败原因（闸门是 UI/驱动读取 `last_write_error` 的既有入口）。
+fn flush_failure_message(session: &mut DocumentSession, ticket: &ExternalCommitTicket) -> String {
+    match drive_gate_to_failure(session, ticket) {
+        GateError::FlushFailed { message, .. } => message,
+        other => panic!("期望 GateError::FlushFailed，实际 {other:?}"),
+    }
+}
+
+/// P1-3 生产接线：head 被外部推进后，flusher 必须在同一个后台线程里
+/// `load_head` + 重建 sink，并重试**同一前缀**一次；成功后正常裁剪/推进 durable + ACK。
+///
+/// 外部推进用「同一份远端 update 已经先落库、会话又把它导入了 live」构造：新 head 的
+/// ops 全部落在未 ACK 前缀的 `end_vv` 内，因此原样重试不会写坏 `head_vv`/`frontier`
+/// （下面的完整重放会验证这一点）。
+#[test]
+fn flush_recovers_from_head_conflict_by_rebuilding_sink() {
+    let (_dir, vault) = fresh_vault();
+    let _seed = seed_note(&vault, "note-1", "doc-1");
+    let mut session = open_session(&vault, options(PEER_A));
+
+    // 会话先把远端 update 导入 live（journal 里留下同一批 op 的未 ACK 增量）。
+    let remote = BodyDoc::create("doc-1", PEER_B).expect("远端副本");
+    remote.insert_scalars(0, "远端").expect("远端编辑");
+    let remote_bytes = remote.export_all_updates().expect("导出远端 update");
+    let remote_update = pending(&remote, &loro::VersionVector::default()).expect("远端增量");
+    session
+        .apply_remote(&remote_bytes)
+        .expect("导入远端 update")
+        .expect("必须有变更");
+
+    // 外部 writer（同步引擎 integrate / 另一个 Vault）把同一份 update 先落库：head 0→1。
+    let advanced = append_updates(
+        &vault,
+        "doc-1",
+        0,
+        &[UpdateRecord::new("external-1", remote_update)],
+    )
+    .expect("外部推进 head");
+    assert_eq!(advanced.head_seq, 1);
+    assert_eq!(
+        session.durable_head_seq(),
+        0,
+        "会话的 CAS 基准仍停在打开时的 head 0"
+    );
+
+    // 修复前：这里每次都拿旧 head 0 撞 CAS —— 不写库、不裁剪、不 ACK，下一次还是旧 head。
+    let ticket = session.flush(FlushReason::Manual);
+    let ack = session
+        .wait_for_durable(ticket.target_live_rev, Duration::from_secs(5))
+        .expect("HeadConflict 后必须自动重建 sink 并发出 DurableAck");
+    assert_eq!(ack.through_live_rev, LiveRev(1));
+    assert_eq!(
+        ack.head_seq, 2,
+        "重试写在外部那一条之后（同一前缀、新 CAS 基准）"
+    );
+    assert_eq!(session.durable_head_seq(), 2);
+    assert_eq!(session.durable_through(), LiveRev(1));
+    assert_eq!(session.pending_len(), 0, "成功后必须裁剪前缀");
+    assert_eq!(session.markdown(), "远端");
+
+    // 正文/head 正确：完整重放必须通过 `loro_docs.head_frontier` 的完整性校验。
+    let (replayed, head) = load_body_doc(&vault, "doc-1", PEER_A, None).expect("重放正文");
+    assert_eq!(replayed.text(), "远端");
+    assert_eq!(head.head_seq, 2);
+    let head_vv = head.version_vector().expect("解码 head vv");
+    let live_vv = loro::VersionVector::decode(&session.snapshot().live_vv).expect("解码 live vv");
+    assert_eq!(
+        head_vv, live_vv,
+        "durable head 的 ops 必须与本会话 live 一致"
+    );
+    assert_eq!(
+        count(
+            &vault,
+            "SELECT count(*) FROM loro_updates WHERE doc_id = 'doc-1'",
+            &[]
+        ),
+        2,
+        "外部一条 + 自动重建后重试的一条"
+    );
+}
+
+/// P1-3 反向边界：外部 head 含本会话 live **未知**的并发 ops 时，原样重试会写坏
+/// `head_vv`/`frontier`（重放完整性校验直接 Corrupt），因此自动重建必须拒绝并进入
+/// 不可恢复态：`last_write_error` 明确提示「需要重开文档」、journal 原样保留、
+/// 后续 flush 立即失败且不再写库。
+#[test]
+fn flush_reports_unrecoverable_after_bounded_rebuild() {
+    let (_dir, vault) = fresh_vault();
+    let _seed = seed_note(&vault, "note-1", "doc-1");
+    let mut session = open_session(&vault, options(PEER_A));
+
+    session.apply(request(&session, "c1", 0, vec![edit(0, 0, "本地")]));
+
+    // 外部 writer 写入本会话完全不知道的并发 ops：head 0→1。
+    let external = BodyDoc::create("doc-1", PEER_B).expect("外部副本");
+    external.insert_scalars(0, "远端").expect("外部编辑");
+    let external_update = pending(&external, &loro::VersionVector::default()).expect("外部增量");
+    append_updates(
+        &vault,
+        "doc-1",
+        0,
+        &[UpdateRecord::new("external-1", external_update)],
+    )
+    .expect("外部推进 head");
+
+    // 闸门必须收敛到 Failed 并带出可执行的原因；修复前只会带出裸 HeadConflict，
+    // 且同一个失败会永久重复（journal 不裁剪、durable 水位不前进）。
+    let ticket = session.begin_external_commit().expect("begin");
+    let message = flush_failure_message(&mut session, &ticket);
+    assert!(
+        message.contains("需要重开文档"),
+        "失败原因必须明确提示重开：{message}"
+    );
+    assert!(
+        message.contains("未知的并发 ops"),
+        "必须说明为什么不能安全自动重建：{message}"
+    );
+
+    assert_eq!(session.pending_len(), 1, "不可恢复时 journal 必须原样保留");
+    let rows = || {
+        count(
+            &vault,
+            "SELECT count(*) FROM loro_updates WHERE doc_id = 'doc-1'",
+            &[],
+        )
+    };
+    assert_eq!(rows(), 1, "只有外部那一行落库（旧前缀不得被硬写）");
+    assert_eq!(head(&vault, "doc-1").head_seq, 1, "库中 head 不得被改写");
+
+    // 后续 flush 立即失败：不发 ACK、不再写库（不能无限重试/无限写）。
+    for _ in 0..3 {
+        let attempt = session.flush(FlushReason::Manual);
+        assert!(
+            session
+                .wait_for_durable(attempt.target_live_rev, Duration::from_millis(200))
+                .is_none(),
+            "不可恢复后不得再发出 DurableAck"
+        );
+        assert_eq!(rows(), 1, "不可恢复后不得再写 loro_updates");
+        assert_eq!(head(&vault, "doc-1").head_seq, 1, "不可恢复后不得推进 head");
+    }
+
+    // 同一个错误仍可被闸门读到（不是一次性的诊断字符串）。
+    match session.poll_external_commit(&ticket) {
+        GateState::Failed(GateError::FlushFailed { message: again, .. }) => {
+            assert_eq!(again, message, "不可恢复状态必须稳定可读");
+        }
+        other => panic!("期望持续的 Failed，实际 {other:?}"),
+    }
+    assert_eq!(
+        session.markdown(),
+        "本地",
+        "live 保持会话内容，靠关闭并重开文档与库对账"
+    );
+}
+
+/// P1-3 的另一个不可恢复分支：自动重建要读新 head，读不到/解码失败同样必须 fail-closed
+/// （不尝试写、不无限重试），并明确提示需要重开文档。
+#[test]
+fn flush_reports_unrecoverable_when_new_head_is_unreadable() {
+    let (_dir, vault) = fresh_vault();
+    let _seed = seed_note(&vault, "note-1", "doc-1");
+    let mut session = open_session(&vault, options(PEER_A));
+    session.apply(request(&session, "c1", 0, vec![edit(0, 0, "本地")]));
+
+    // 外部推进 head 后把 head_vv 写坏：CAS 冲突后能读到行，但 VV 解码必然失败。
+    let external = BodyDoc::create("doc-1", PEER_B).expect("外部副本");
+    external.insert_scalars(0, "远端").expect("外部编辑");
+    let external_update = pending(&external, &loro::VersionVector::default()).expect("外部增量");
+    append_updates(
+        &vault,
+        "doc-1",
+        0,
+        &[UpdateRecord::new("external-1", external_update)],
+    )
+    .expect("外部推进 head");
+    vault
+        .exec(
+            "UPDATE loro_docs SET head_vv = x'01' WHERE doc_id = ?1",
+            &[Value::Text("doc-1".to_owned())],
+        )
+        .expect("写坏 head_vv");
+
+    let ticket = session.begin_external_commit().expect("begin");
+    let message = flush_failure_message(&mut session, &ticket);
+    assert!(
+        message.contains("需要重开文档"),
+        "失败原因必须明确提示重开：{message}"
+    );
+    assert!(
+        message.contains("解码失败"),
+        "必须说明新 head 读不出来：{message}"
+    );
+    assert_eq!(session.pending_len(), 1, "不可恢复时 journal 必须原样保留");
+    assert_eq!(
+        count(
+            &vault,
+            "SELECT count(*) FROM loro_updates WHERE doc_id = 'doc-1'",
+            &[]
+        ),
+        1,
+        "拒绝重建后不得写旧前缀"
+    );
+}
diff --git a/crates/storage/tests/sync_inbox_batch.rs b/crates/storage/tests/sync_inbox_batch.rs
index 9c04bc9..dc0352f 100644
--- a/crates/storage/tests/sync_inbox_batch.rs
+++ b/crates/storage/tests/sync_inbox_batch.rs
@@ -17,12 +17,15 @@ use menghunote_storage::{
     advance_integrated_prefix, append_updates_with_integrated_marks, count_inbox_ready, load_head,
     mark_integrated, mark_integrated_batch, mark_skipped_event_received, set_sync_log_id,
     store_inbox_verified, store_inbox_verified_batch, sync_cursor_state, take_inbox_ready,
-    StorageError, UpdateRecord, Value, Vault, VerifiedInboxRow, META_SYNC_LOG_ID,
-    META_SYNC_RECEIVED_SEQ, META_SYNC_SKIPPED_RECEIVED_PREFIX,
+    take_inbox_ready_where, StorageError, UpdateRecord, Value, Vault, VerifiedInboxRow,
+    META_SYNC_LOG_ID, META_SYNC_RECEIVED_SEQ, META_SYNC_SKIPPED_RECEIVED_PREFIX,
 };
 
 const LOG_ID: [u8; 16] = [0x3a; 16];
 
+/// 另一个 incarnation（restore 迁移后当前 `sync.log_id` 可能变成它，旧行仍然 durable）。
+const OTHER_LOG_ID: [u8; 16] = [0x5b; 16];
+
 fn bind_log(vault: &Vault) {
     set_sync_log_id(vault, &LOG_ID).expect("写 sync.log_id");
 }
@@ -855,3 +858,175 @@ fn batch_under_foreign_write_lock_fails_busy_without_partial_write() {
     assert_eq!(inbox_rows(&vault).len(), 1);
     assert_eq!(received(&vault), 1);
 }
+
+// ================================ incarnation 过滤（restore 迁移 log_id 后的旧行）
+
+/// 当前 incarnation 的 ready `relay_seq`（升序；`take_inbox_ready` 已按 log/seq 排序）。
+fn ready_seqs(vault: &Vault, limit: usize) -> Vec<u64> {
+    take_inbox_ready(vault, limit)
+        .expect("读 ready")
+        .into_iter()
+        .map(|row| row.relay_seq)
+        .collect()
+}
+
+/// 直接写 `meta` 原始字节（模拟 restore 的显式迁移 / 损坏状态，绕过 Vault 校验）。
+fn write_raw_meta(vault: &Vault, key: &str, value: &[u8]) {
+    vault
+        .exec(
+            "INSERT INTO meta(key,value) VALUES(?1,?2) \
+             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
+            &[Value::Text(key.to_owned()), Value::Blob(value.to_vec())],
+        )
+        .expect("写原始 meta");
+}
+
+/// 直接插入一条**其它 incarnation** 的 verified 行。
+///
+/// 生产入口 `store_inbox_verified*` 都先 `ensure_log_id`，跨代写入会被
+/// `SyncLogMismatch` 拒绝；这种 durable 状态只能由 restore 显式迁移 `sync.log_id`
+/// 后留下，所以测试直接用 SQL 准备，不经过被测试的过滤逻辑。
+fn insert_stale_incarnation_row(vault: &Vault, log_id: &[u8; 16], relay_seq: u64, seed: u8) {
+    vault
+        .exec(
+            "INSERT INTO sync_inbox(log_id,relay_seq,object_id,epoch,state,verified_payload) \
+             VALUES(?1,?2,?3,?4,'verified',?5)",
+            &[
+                Value::Blob(log_id.to_vec()),
+                Value::Integer(i64::try_from(relay_seq).expect("relay_seq 在 i64 内")),
+                Value::Blob(vec![seed; 32]),
+                Value::Integer(1),
+                Value::Blob(vec![seed; 16]),
+            ],
+        )
+        .expect("插入旧 incarnation 的 verified 行");
+}
+
+/// 当前 log A 有 verified 行、库里另有 log B 的 verified 行时：
+/// ready/count 只计 A；B 即使 `relay_seq <= received` 也不计入；received 前缀只按 A 推进。
+#[test]
+fn ready_and_count_only_include_current_log_incarnation() {
+    let (_dir, vault) = fresh_vault();
+    bind_log(&vault);
+    assert_eq!(store_inbox_verified_batch(&vault, &rows(2, 0)).unwrap(), 2);
+    assert_eq!(received(&vault), 2);
+
+    // 旧 incarnation B 的 1..=5 仍在库里（restore 迁移 log_id 后旧行不能被静默删除）。
+    for seq in 1..=5u64 {
+        insert_stale_incarnation_row(
+            &vault,
+            &OTHER_LOG_ID,
+            seq,
+            u8::try_from(seq).expect("seq 在 u8 内"),
+        );
+    }
+    // `set_sync_log_id` 的语义未变：生产入口仍拒绝跨代写入（restore 走显式迁移）。
+    assert!(
+        matches!(
+            set_sync_log_id(&vault, &OTHER_LOG_ID),
+            Err(StorageError::SyncLogMismatch { .. })
+        ),
+        "set_sync_log_id 必须继续拒绝跨代变更"
+    );
+
+    // B 的 seq1/seq2 都 <= received(2)，但 ready/count 只能看到 A 的 1/2。
+    assert_eq!(ready_seqs(&vault, 16), vec![1, 2]);
+    assert_eq!(
+        count_inbox_ready(&vault).unwrap(),
+        2,
+        "旧 incarnation 的行即使 relay_seq<=received 也不得计入"
+    );
+
+    // A 的 seq3 落库 → received 只推进到 3，不跨 B 的 1..=5。
+    assert_eq!(
+        store_inbox_verified_batch(&vault, &[row(3, 0x33, 8)]).unwrap(),
+        1
+    );
+    assert_eq!(received(&vault), 3, "received 前缀推进不跨 incarnation");
+    assert_eq!(ready_seqs(&vault, 16), vec![1, 2, 3]);
+    assert_eq!(count_inbox_ready(&vault).unwrap(), 3);
+
+    // 状态变化同样只作用于当前 log：集成 A 的 seq1 后 B 的行仍不 ready、不影响计数。
+    mark_integrated(&vault, &LOG_ID, 1).expect("集成 A seq1");
+    assert_eq!(ready_seqs(&vault, 16), vec![2, 3]);
+    assert_eq!(count_inbox_ready(&vault).unwrap(), 2);
+}
+
+/// `take_inbox_ready_where` 走同一条底层路径：谓词只能看到当前 log 的行。
+#[test]
+fn ready_where_path_filters_other_log_incarnation() {
+    let (_dir, vault) = fresh_vault();
+    bind_log(&vault);
+    assert_eq!(store_inbox_verified_batch(&vault, &rows(3, 0)).unwrap(), 3);
+    for seq in 1..=3u64 {
+        insert_stale_incarnation_row(
+            &vault,
+            &OTHER_LOG_ID,
+            seq,
+            0xb0 + u8::try_from(seq).unwrap_or(0),
+        );
+    }
+
+    // 恒真谓词 + 足够大的 limit：旧实现会把 B 的 3 行也交进来（6 条），现在必须只有 3 条。
+    let mut observed_log_ids = Vec::new();
+    let picked = take_inbox_ready_where(&vault, 16, |row| {
+        observed_log_ids.push(row.log_id);
+        true
+    })
+    .expect("where 路径读 ready");
+    assert_eq!(
+        picked.iter().map(|row| row.relay_seq).collect::<Vec<_>>(),
+        vec![1, 2, 3]
+    );
+    assert!(
+        observed_log_ids.iter().all(|log_id| *log_id == LOG_ID),
+        "谓词只应看到当前 incarnation 的行，实际 {observed_log_ids:?}"
+    );
+
+    // 谓词本身的过滤仍然生效（不是把 where 语义吞掉）。
+    let picked: Vec<u64> = take_inbox_ready_where(&vault, 16, |row| row.relay_seq != 2)
+        .expect("where 过滤")
+        .into_iter()
+        .map(|row| row.relay_seq)
+        .collect();
+    assert_eq!(picked, vec![1, 3]);
+}
+
+/// `sync.log_id` 缺失 / 长度非法时的语义：
+/// - 缺失 → 空（`take`）/ 0（`count`），即新库旧行为；即使残留其它 log 的行与 received，
+///   也不能回退成"不过滤 log"；
+/// - 长度非法 → 明确 `Corrupt`（与 `sync_cursor_state` 同口径），不静默当作没有 log。
+#[test]
+fn ready_without_log_id_is_empty_and_invalid_length_is_corrupt() {
+    let (_dir, vault) = fresh_vault();
+    // 新库：没有 sync.log_id，也没有 inbox 行 → 空/0（旧实现同样返回空）。
+    assert!(take_inbox_ready(&vault, 8)
+        .expect("无 log 读 ready")
+        .is_empty());
+    assert_eq!(count_inbox_ready(&vault).expect("无 log 计数"), 0);
+
+    // 不一致的持久状态：有遗留 verified 行与 received，却没有 sync.log_id。
+    // 没有当前 incarnation 就没有可匹配的 log，ready 必须为空。
+    insert_stale_incarnation_row(&vault, &OTHER_LOG_ID, 1, 0x5b);
+    write_raw_meta(&vault, META_SYNC_RECEIVED_SEQ, &1u64.to_be_bytes());
+    assert!(
+        take_inbox_ready(&vault, 8)
+            .expect("无 log 读 ready")
+            .is_empty(),
+        "缺 sync.log_id 时不能漏出其它 incarnation 的遗留行"
+    );
+    assert_eq!(count_inbox_ready(&vault).expect("无 log 计数"), 0);
+
+    // 长度不是 16B = 损坏：显式错误，不静默当成"没有 log"。
+    write_raw_meta(&vault, META_SYNC_LOG_ID, b"short");
+    let error = take_inbox_ready(&vault, 8).expect_err("非法 log_id 长度必须报错");
+    assert!(
+        matches!(error, StorageError::Corrupt { .. }),
+        "实际 {error:?}"
+    );
+    let error = count_inbox_ready(&vault).expect_err("非法 log_id 长度必须报错");
+    assert!(
+        matches!(error, StorageError::Corrupt { .. }),
+        "实际 {error:?}"
+    );
+}
diff --git a/crates/storage/tests/sync_intent.rs b/crates/storage/tests/sync_intent.rs
index 2242567..7a7a4f1 100644
--- a/crates/storage/tests/sync_intent.rs
+++ b/crates/storage/tests/sync_intent.rs
@@ -25,9 +25,9 @@ use menghunote_storage::session::{
 use menghunote_storage::{
     append_updates, apply_baseline_batch, begin_baseline_apply, enqueue_sync_intent,
     finish_baseline_apply, intent_message_id, intent_object_id, load_body_doc, load_head,
-    local_sync_intent, outbox_row, pending_encryption, BodyDoc, DurableSink, JournalEntry,
-    JournalOrigin, StorageError, StructureWriter, SyncContext, SyncIntent, SyncOutboxState,
-    UpdateRecord, Value, Vault, VaultJournalSink, STRUCTURE_DOC_ID,
+    local_sync_intent, outbox_row, pending_encryption, BaselineExpectation, BodyDoc, DurableSink,
+    JournalEntry, JournalOrigin, StorageError, StructureWriter, SyncContext, SyncIntent,
+    SyncOutboxState, UpdateRecord, Value, Vault, VaultJournalSink, STRUCTURE_DOC_ID,
 };
 use menghunote_text::UnicodeScalar;
 
@@ -504,7 +504,17 @@ fn remote_import_and_adopt_never_produce_intents() {
         doc.insert_scalars(0, "基线").expect("写基线");
         doc.export_snapshot().expect("导出 snapshot")
     };
-    begin_baseline_apply(&vault3, &[7; 16], 3, "cursor-1").expect("begin");
+    begin_baseline_apply(
+        &vault3,
+        &[7; 16],
+        3,
+        "cursor-1",
+        &BaselineExpectation::new(
+            std::iter::once("doc-1".to_owned()),
+            std::iter::empty::<[u8; 32]>(),
+        ),
+    )
+    .expect("begin");
     apply_baseline_batch(&vault3, &[("doc-1".to_owned(), baseline)], &[]).expect("apply");
     let _ = finish_baseline_apply(&vault3, 3, "cursor-2").expect("finish");
     assert_eq!(outbox_count(&vault3), 0, "adopt/baseline 绝不能产生 intent");
diff --git a/crates/sync/src/bootstrap.rs b/crates/sync/src/bootstrap.rs
index 4dc768c..bcdb57e 100644
--- a/crates/sync/src/bootstrap.rs
+++ b/crates/sync/src/bootstrap.rs
@@ -5,10 +5,20 @@
 //!
 //! **可恢复导入（P1-4）**：docs/structure/assets 的写入全部经过 storage 的
 //! `baseline_apply` staging journal（`begin_baseline_apply` / `apply_baseline_batch` /
-//! `apply_baseline_assets`）。任一步失败都**不**清 journal、**不**推进任何水位；
-//! 下一次调用按同一份基线幂等续跑。只有全部实际导入并通过读回校验后，才在**一个**
+//! `apply_baseline_assets`）。下载/写入阶段任一步失败都**不**清 journal、**不**推进任何
+//! 水位；下一次调用按同一份基线幂等续跑。只有全部实际导入并通过读回校验后，才在**一个**
 //! SQLite 事务里推进 `cursor/received/integrated/confirmed_highwater`（CAS）。
 //!
+//! **读回校验失败（同步硬化 P1）**：`verify_imported` 失败时**不回滚**已 merge 的
+//! Loro/资产数据（CRDT merge 幂等；回滚会丢本地编辑），而是写一条只本地可见的
+//! `sync.baseline_verify_failed` 失败标记（storage `mark_baseline_verify_failed`）。
+//! journal 仍是 `staging`（schema 的 status CHECK 只允许 `staging/completed`，见
+//! `docs/notes/sync-hardening-20261121.md` §8），但下一次 `begin_baseline_apply` 会拒绝，
+//! 必须先 `abort_baseline_apply` 清理（abort 同时删标记，已导入数据保留）。
+//!
+//! **闸外编辑**：verify 不与编辑互斥，判定用「导出点 VV ⊆ 当前 VV」而不是相等；只有
+//! 本地没有快照之外的 op 时才逐字节比较正文（见 [`check_imported_doc`]）。
+//!
 //! **水位单调（P1-4）**：bootstrap 允许作用在已有库上，但禁止降低任何单调值：
 //! `received/integrated/highwater` 取 `max(现有, baseline S)`；已有进度时保留现有 relay
 //! 游标（不把已经消费过的位置回退）；`instance/log_id/epoch/control rev+hash` 只允许
@@ -36,6 +46,7 @@
 //! `instance/log_id/epoch/control_rev/highwater` 的回退。
 
 use std::collections::{BTreeMap, BTreeSet};
+use std::time::Instant;
 
 use loro::VersionVector;
 use sha2::{Digest, Sha256};
@@ -53,8 +64,9 @@ use crate::rotation::ManagerTransitionV1;
 use crate::upload::fetch_object;
 use menghunote_storage::{
     abort_baseline_apply, apply_baseline_assets, apply_baseline_batch, asset_bytes,
-    baseline_apply_state, begin_baseline_apply, load_body_doc, load_structure, sync_cursor_state,
-    AffectedGuard, BaselineApplyStatus, BaselineAsset, BodyDoc, Statement, StructureDoc,
+    baseline_apply_state, begin_baseline_apply, load_body_doc, load_structure,
+    mark_baseline_verify_failed, record_baseline_verify_time, sync_cursor_state, AffectedGuard,
+    BaselineApplyStatus, BaselineAsset, BaselineExpectation, BodyDoc, Statement, StructureDoc,
     StructureWriter, Value, Vault, APP_DOC_SCHEMA, CODEC_VERSION, META_SYNC_CONFIRMED_HIGHWATER,
     META_SYNC_INTEGRATED_SEQ, META_SYNC_LOG_ID, META_SYNC_RECEIVED_SEQ, MIN_READER,
     STRUCTURE_DOC_ID,
@@ -182,8 +194,14 @@ pub enum BootstrapFault {
     /// 水位事务读取当前状态之后、提交之前，模拟另一个写者改写了 relay 游标，
     /// 迫使 CAS 失败（整笔事务回滚）。
     CursorCasConflict,
+    /// 读回校验阶段注入一次「本地 VV 未覆盖快照」失败：用于验证失败标记、
+    /// `begin` 拒绝续跑与 `abort` 清理路径（不改数据库里的真实数据）。
+    VerifyVvNotCovered,
 }
 
+/// 故障注入伪造的 peer id（只在校验失败测试路径出现；不影响真实数据）。
+const VERIFY_FAULT_PEER: u64 = 0x0BAD_5EED;
+
 struct VerifiedDoc {
     doc_id: String,
     kind: u64,
@@ -1052,8 +1070,15 @@ fn apply_selected(
     let checkpoint_id = selected.checkpoint.checkpoint_id;
     let baseline_s = selected.checkpoint.covered_seq;
     let source_cursor = format!("{JOURNAL_SOURCE_PREFIX}{}", selected.checkpoint.cursor);
+    // 完整性基准：本 checkpoint 覆盖的全部文档与资产都必须出现在 apply 批里。
+    // 集合由 begin 记录，任何"少导入一批"的续跑都不会被误当成完整
+    // （rotation 的 finish 会直接 Conflict；bootstrap 的 finalize 另有逐项读回校验）。
+    let expected = BaselineExpectation::new(
+        selected.docs.iter().map(|doc| doc.doc_id.clone()),
+        selected.assets.iter().map(|asset| asset.object_id),
+    );
     reconcile_journal(vault, &checkpoint_id, baseline_s, &source_cursor)?;
-    begin_baseline_apply(vault, &checkpoint_id, baseline_s, &source_cursor)?;
+    begin_baseline_apply(vault, &checkpoint_id, baseline_s, &source_cursor, &expected)?;
 
     // body → structure → asset（structure 投影依赖正文行已存在；与旧实现的顺序一致）。
     let bodies: Vec<&VerifiedDoc> = selected
@@ -1109,7 +1134,47 @@ fn apply_selected(
         return Err(injected_fault());
     }
 
-    verify_imported(vault, peer_id, selected)
+    // 读回校验的耗时只作诊断指标（不参与决策）；失败也要记录。
+    let verify_started = Instant::now();
+    let verified = verify_imported(vault, peer_id, selected, fault);
+    record_baseline_verify_time(verify_started.elapsed());
+    match verified {
+        Ok(report) => Ok(report),
+        Err(error) => {
+            // 不回滚已 merge 的 Loro/资产，也不改 journal.status（schema CHECK 只有
+            // staging/completed）：只写失败标记，让下一次 begin 拒绝续跑，直到 abort。
+            if let Err(mark_error) = mark_baseline_verify_failed(vault, &error.to_string()) {
+                return Err(SyncError::Storage(mark_error));
+            }
+            Err(error)
+        }
+    }
+}
+
+/// 单篇文档的读回校验：导出点 VV ⊆ 当前本地 VV，且仅在无本地 extra 时比较正文。
+///
+/// - 方向固定为「快照/导出点 VV 必须被本地 VV 覆盖」：`actual_vv.sub_iter(local_vv)`
+///   为空才通过；**不要求相等**。导出点之后本地新增的 op 是合法编辑（verify 不与编辑
+///   互斥），相等比较会把它误判成失败；
+/// - 只有 `local_vv ⊆ actual_vv`（本地没有快照之外的 op）时，Loro 才保证本地正文等于
+///   快照正文，此时才做逐字节比较；有本地 extra 时正文必然已被合并改写，比较必然误报。
+fn check_imported_doc(
+    actual_vv: &VersionVector,
+    local_vv: &VersionVector,
+    expected_text: Option<&str>,
+    actual_text: &str,
+) -> Result<(), &'static str> {
+    if actual_vv.sub_iter(local_vv).next().is_some() {
+        return Err("bootstrap 导入后本地 VV 未覆盖快照");
+    }
+    if local_vv.sub_iter(actual_vv).next().is_none() {
+        if let Some(expected) = expected_text {
+            if expected != actual_text {
+                return Err("bootstrap 后正文与快照不一致");
+            }
+        }
+    }
+    Ok(())
 }
 
 /// 读回校验：本地 VV 必须覆盖快照、正文/结构投影/资产必须与快照一致。
@@ -1117,22 +1182,28 @@ fn verify_imported(
     vault: &Vault,
     peer_id: u64,
     selected: &SelectedBaseline,
+    fault: Option<BootstrapFault>,
 ) -> Result<Vec<(String, Option<String>)>, SyncError> {
     let mut body_texts: BTreeMap<String, String> = BTreeMap::new();
-    for doc in selected.docs.iter().filter(|doc| doc.kind == KIND_BODY) {
+    for (index, doc) in selected
+        .docs
+        .iter()
+        .filter(|doc| doc.kind == KIND_BODY)
+        .enumerate()
+    {
         let (body, head) = load_body_doc(vault, &doc.doc_id, peer_id, None)?;
         let local_vv = head.version_vector()?;
-        if doc.actual_vv.sub_iter(&local_vv).next().is_some() {
-            return Err(SyncError::Object("bootstrap 导入后本地 VV 未覆盖快照"));
-        }
+        // 测试故障注入：伪造一个「导出点有、本地没有」的 op，制造真实的 VV 未覆盖失败。
+        let actual_vv = if fault == Some(BootstrapFault::VerifyVvNotCovered) && index == 0 {
+            let mut mismatched = doc.actual_vv.clone();
+            mismatched.insert(VERIFY_FAULT_PEER, 1);
+            mismatched
+        } else {
+            doc.actual_vv.clone()
+        };
         let text = body.text();
-        // 本地没有快照之外的 op 时，正文必须与快照逐字节一致（合并过的正文由 Loro 保证）。
-        if local_vv.sub_iter(&doc.actual_vv).next().is_none() {
-            if let Some(expected) = &doc.text {
-                if expected != &text {
-                    return Err(SyncError::Object("bootstrap 后正文与快照不一致"));
-                }
-            }
+        if let Err(reason) = check_imported_doc(&actual_vv, &local_vv, doc.text.as_deref(), &text) {
+            return Err(SyncError::Object(reason));
         }
         body_texts.insert(doc.doc_id.clone(), text);
     }
@@ -1599,6 +1670,42 @@ mod tests {
         );
     }
 
+    #[test]
+    fn verify_accepts_concurrent_local_edits_after_export() {
+        // 导出点（快照）VV：peer 7 有 2 个 op、peer 8 有 1 个。
+        let mut exported_vv = VersionVector::new();
+        exported_vv.insert(7, 2);
+        exported_vv.insert(8, 1);
+
+        // 导出点之后本地继续编辑：当前 VV 严格包含导出点，但不相等。
+        let mut local_vv = exported_vv.clone();
+        local_vv.insert(9, 1);
+        assert_ne!(local_vv, exported_vv);
+        check_imported_doc(
+            &exported_vv,
+            &local_vv,
+            Some("导出点正文"),
+            "导出点正文 + 本地新编辑",
+        )
+        .expect("导出点 VV ⊆ 当前 VV 时必须接受并发本地编辑（不要求相等）");
+
+        // 没有本地 extra（VV 相等）时才逐字节比较正文：不一致必须失败。
+        assert!(
+            check_imported_doc(&exported_vv, &exported_vv, Some("期望正文"), "实际正文").is_err(),
+            "无本地 extra 时正文不一致必须失败"
+        );
+        check_imported_doc(&exported_vv, &exported_vv, Some("同一正文"), "同一正文")
+            .expect("无本地 extra 且正文一致必须通过");
+
+        // 当前 VV 缺少导出点里的 op：方向不能反（不是相等/双向比较）。
+        let mut missing_vv = VersionVector::new();
+        missing_vv.insert(7, 2);
+        assert!(
+            check_imported_doc(&exported_vv, &missing_vv, Some("正文"), "正文").is_err(),
+            "导出点 VV 未被本地覆盖时必须失败"
+        );
+    }
+
     #[test]
     fn full_history_is_mandatory() {
         let (loro, vv, frontier) = document();
diff --git a/crates/sync/src/engine.rs b/crates/sync/src/engine.rs
index 67ebbc8..75fcadd 100644
--- a/crates/sync/src/engine.rs
+++ b/crates/sync/src/engine.rs
@@ -170,7 +170,10 @@ pub struct PushReport {
 /// - relay 上对象已完整（例如 checkpoint/轮换/另一台设备传过）时按确定性密文逐片
 ///   校验后只做提交/落标记，不重传分片；分片缺失时只补缺失片（已 durable 的片不重传）。
 /// - 对象被其它 reservation 持有时整条资产**延后**（`deferred`），不阻塞正文/结构同步，
-///   下一轮再试；网络/单资产损坏计入 `failed`，同样不阻塞本轮。
+///   下一轮再试；relay/对象校验失败计入 `failed`，同样不阻塞本轮。
+/// - **本地库损坏**（[`SyncError::LocalCorrupt`]，例如字节长度/哈希与元数据不一致）不是
+///   可重试的网络失败：继续扫完其余健康资产后，把首个损坏原样上抛，见
+///   [`SyncEngine::upload_pending_assets`]。
 #[derive(Debug, Clone, Default, PartialEq, Eq)]
 pub struct AssetSyncReport {
     /// 被正文引用、有明文的资产数（本轮扫描口径）。
@@ -187,7 +190,10 @@ pub struct AssetSyncReport {
     pub reused_existing: usize,
     /// 被其它 reservation 持有、需下一轮再试的资产数。
     pub deferred: usize,
-    /// 单资产失败数（网络/校验失败；不阻塞正文同步，下一轮重试）。
+    /// 单资产 relay/对象校验失败数（不阻塞正文同步，下一轮重试）。
+    ///
+    /// **本地库损坏不计入这里**：`LocalCorrupt` 每轮重试都不会自愈，会掩盖真正的
+    /// "需要本地修复"状态，因此整轮上抛而不是伪装成可重试失败。
     pub failed: usize,
     /// 元数据不可用或空数据而跳过的资产数（不上传空资产）。
     pub skipped_invalid: usize,
@@ -1091,12 +1097,19 @@ impl<'a, C: RelayClient> SyncEngine<'a, C> {
     /// 阶段 2b：**增量上传被正文引用的本地资产**（正文推送之后、拉取之前）。
     ///
     /// 顺序是刻意的（ADR 0012 的两阶段路径也复用本方法）：
-    /// - 正文/结构更新先 `push`，资产后置；单条资产失败只计入
+    /// - 正文/结构更新先 `push`，资产后置；单条资产 relay/对象校验失败只计入
     ///   [`AssetSyncReport::failed`]，**不阻塞**本轮正文交付；
     /// - 资产在 `pull` 之前完成，远端设备下一次 pull 就能同时拿到正文与 ASSET 事件；
     /// - 已有 durable 标记的资产不读字节、不发请求；relay 上已完整的对象只校验+提交，
     ///   被其它 reservation 持有的对象延后（`deferred`），下一轮重试。
     ///
+    /// **本地库损坏上抛**：`upload_one_asset` 发现本地字节长度/哈希与元数据不一致、
+    /// 或确定性分片事实与元数据不符时返回 [`SyncError::LocalCorrupt`]。这类错误每轮
+    /// 重试都不可能自愈，必须让调用方看到（否则就是"永远重试成功、永不报错"）。
+    /// 处理方式是**继续扫完其余资产**（健康资产照常上传/落标记），最后把第一个本地
+    /// 损坏原样上抛；[`SyncError::Paused`]（全局暂停）与 [`SyncError::Storage`]
+    /// （本地库读写失败）仍**立即**上抛，语义不变。
+    ///
     /// 只处理 `asset_refs` 引用得到的资产：历史孤儿不重传（`orphans_skipped` 如实上报）。
     pub fn upload_pending_assets(&mut self) -> Result<AssetSyncReport, SyncError> {
         if self.rotation_frozen {
@@ -1106,6 +1119,7 @@ impl<'a, C: RelayClient> SyncEngine<'a, C> {
             orphans_skipped: count_unreferenced_assets(self.vault)?,
             ..AssetSyncReport::default()
         };
+        let mut first_local_corrupt: Option<SyncError> = None;
         let assets = list_referenced_assets(self.vault)?;
         report.referenced = assets.len();
         for meta in assets {
@@ -1149,9 +1163,20 @@ impl<'a, C: RelayClient> SyncEngine<'a, C> {
                 Err(error) if matches!(error, SyncError::Paused(_) | SyncError::Storage(_)) => {
                     return Err(error)
                 }
+                // 本地库损坏不是网络失败：先继续上传其余健康资产（不让单个损坏拖住
+                // 整库），最后统一上抛。计成 `failed` 会让它每轮"重试成功"、永不报错。
+                Err(error) if matches!(error, SyncError::LocalCorrupt(_)) => {
+                    if first_local_corrupt.is_none() {
+                        first_local_corrupt = Some(error);
+                    }
+                }
+                // 其余（relay/传输/对象校验）沿用既有语义：单资产失败，下一轮重试。
                 Err(_) => report.failed += 1,
             }
         }
+        if let Some(error) = first_local_corrupt {
+            return Err(error);
+        }
         Ok(report)
     }
 
diff --git a/crates/sync/src/rotation_apply.rs b/crates/sync/src/rotation_apply.rs
index 9dabfdb..b47ce45 100644
--- a/crates/sync/src/rotation_apply.rs
+++ b/crates/sync/src/rotation_apply.rs
@@ -28,7 +28,7 @@ use crate::{DeviceId, ObjectId, VaultId};
 use menghunote_storage::{
     abort_baseline_apply, apply_baseline_assets, apply_baseline_batch, baseline_apply_state,
     begin_baseline_apply, finish_baseline_apply, repack_required, BaselineApplyFinish,
-    BaselineApplyState, BaselineApplyStatus, BaselineAsset,
+    BaselineApplyState, BaselineApplyStatus, BaselineAsset, BaselineExpectation,
 };
 use menghunote_sync_proto::{
     activation_t_signing_input, checkpoint_signing_input, grant_package_digest, ActivationCommitV1,
@@ -307,7 +307,19 @@ pub fn apply_verified_baseline(
     payload: &BaselinePayload,
     baseline_cursor: &str,
 ) -> Result<BaselineApplyState, SyncError> {
-    begin_baseline_apply(vault, &payload.checkpoint_id, payload.baseline_s, "")?;
+    // 完整性基准：候选 checkpoint 覆盖的 docs/资产必须全部写进 journal，少一批就
+    // finish 不了（不会推进 cursor）。
+    let expected = BaselineExpectation::new(
+        payload.docs.iter().map(|(doc_id, _)| doc_id.clone()),
+        payload.assets.iter().map(|asset| asset.object_id),
+    );
+    begin_baseline_apply(
+        vault,
+        &payload.checkpoint_id,
+        payload.baseline_s,
+        "",
+        &expected,
+    )?;
     let result = (|| {
         for chunk in payload.docs.chunks(64) {
             apply_baseline_batch(vault, chunk, &[])?;
diff --git a/crates/sync/tests/asset_sync.rs b/crates/sync/tests/asset_sync.rs
index 5eea795..0f43745 100644
--- a/crates/sync/tests/asset_sync.rs
+++ b/crates/sync/tests/asset_sync.rs
@@ -3,7 +3,8 @@
 //! 覆盖 attachment-insert 缺口：A 插入并引用新资产后，普通 `sync_once`（非发布基线）
 //! 就应把 ASSET 对象增量上传，B 能取到与 A 一致的字节；第二个同步轮次不重传；
 //! 多片资产传到一半失败后从缺失片续传；checkpoint 已上传的对象被普通同步直接复用/标记；
-//! 被维护 reservation 持有的对象延后（ID_CONFLICT 不阻塞正文）。
+//! 被维护 reservation 持有的对象延后（ID_CONFLICT 不阻塞正文）；本地字节与元数据 hash
+//! 不一致时必须上抛 `LocalCorrupt`（不能伪装成可重试失败）。
 
 mod support;
 
@@ -16,6 +17,7 @@ use menghunote_storage::{
 };
 use menghunote_sync::checkpoint::asset_object_id;
 use menghunote_sync::engine::{EngineIdentity, SyncEngine};
+use menghunote_sync::SyncError;
 use menghunote_sync_proto::{cap_plain, part_count_for, ObjectType};
 
 const ASSET_MEDIA: &str = "text/plain";
@@ -401,7 +403,7 @@ fn missing_media_or_data_skips_asset_without_upload() {
 }
 
 #[test]
-fn asset_hash_mismatch_is_failed_without_uploading_wrong_bytes() {
+fn asset_hash_mismatch_raises_local_corrupt_without_uploading_wrong_bytes() {
     ensure_test_identity_wrap();
     let (_dir, vault) = fresh_vault();
     let device = Device::new(0x91);
@@ -431,12 +433,18 @@ fn asset_hash_mismatch_is_failed_without_uploading_wrong_bytes() {
     link_asset_ref(&vault, "note-tamper", &claimed, 0, 1).expect("link");
     seed_pull_state(&vault, &relay);
 
-    let report = {
+    // P2-9：本地库损坏不是可重试的网络失败，必须**上抛**让调用方看到；原实现把它
+    // 计入 `failed` 后继续，于是每轮都"重试成功"、永不报错。
+    let error = {
         let mut sync = engine(&vault, &mut relay, &device);
-        sync.sync_once().expect("正文同步不得被坏资产阻塞")
+        sync.sync_once()
+            .expect_err("本地资产 hash 与字节不一致必须上抛 LocalCorrupt")
     };
-    assert_eq!(report.assets.failed, 1);
-    assert_eq!(report.assets.uploaded, 0);
+    assert!(
+        matches!(error, SyncError::LocalCorrupt("资产字节哈希与元数据不一致")),
+        "实际：{error:?}"
+    );
+    // 整轮报错也不能把错误字节传给 relay、不能落 durable 标记。
     assert!(relay.put_history.is_empty());
     assert!(marker_rows(&vault).is_empty());
 }
diff --git a/crates/sync/tests/bootstrap.rs b/crates/sync/tests/bootstrap.rs
index 655d4a3..18069c4 100644
--- a/crates/sync/tests/bootstrap.rs
+++ b/crates/sync/tests/bootstrap.rs
@@ -14,10 +14,11 @@ mod support;
 use support::*;
 
 use menghunote_storage::{
-    asset_bytes, baseline_apply_state, body_doc_meta, create_note_slot, ensure_structure_row,
-    insert_asset, list_all_docs, list_notes, load_body_doc, sync_cursor_state, BaselineApplyStatus,
-    BodyDoc, StructureWriter, Value, META_SYNC_CONFIRMED_HIGHWATER, META_SYNC_INTEGRATED_SEQ,
-    META_SYNC_RECEIVED_SEQ, STRUCTURE_DOC_ID,
+    abort_baseline_apply, asset_bytes, baseline_apply_state, baseline_verify_failure,
+    body_doc_meta, create_note_slot, ensure_structure_row, insert_asset, list_all_docs, list_notes,
+    load_body_doc, sync_cursor_state, BaselineApplyStatus, BodyDoc, StructureWriter, Value,
+    META_SYNC_CONFIRMED_HIGHWATER, META_SYNC_INTEGRATED_SEQ, META_SYNC_RECEIVED_SEQ,
+    STRUCTURE_DOC_ID,
 };
 use menghunote_sync::bootstrap::{
     self, control_head_hash, BootstrapFault, BootstrapPins, META_BOOTSTRAP_CONTROL_HASH,
@@ -667,6 +668,103 @@ fn crash_after_assets_resumes_without_duplicate_writes() {
     assert!(baseline_apply_state(&target).expect("journal").is_none());
 }
 
+#[test]
+fn verify_failure_keeps_merged_data_and_marks_journal_failed() {
+    let (_dir, source, materials) = setup();
+    let mut relay = new_relay(&materials);
+    seed_source(&source, &mut relay, &materials);
+
+    let (_target_dir, target) = fresh_vault();
+    // 目标库已有同一篇正文的本地编辑：bootstrap 必须把它与基线 merge（不是覆盖）。
+    let local_peer = materials.peer_id_u64().wrapping_add(1);
+    let local = seed_doc(&target, "note-1", local_peer);
+    local.insert_scalars(0, "本地优先编辑 ").expect("本地正文");
+    commit_local(&target, &local);
+
+    let error = bootstrap::run_with_fault(
+        &target,
+        &mut relay,
+        VAULT_ID,
+        materials.peer_id_u64(),
+        &materials.root,
+        BootstrapFault::VerifyVvNotCovered,
+    )
+    .expect_err("注入的读回校验失败必须冒泡");
+    assert!(
+        format!("{error}").contains("VV 未覆盖快照"),
+        "错误应来自读回校验：{error}"
+    );
+
+    // 已 merge 的 Loro 数据保留：本地编辑与基线正文都在同一条文档里。
+    let merged = load_body_doc(&target, "note-1", local_peer, None)
+        .expect("merged doc")
+        .0;
+    assert!(
+        merged.text().contains("本地优先编辑") && merged.text().contains("第一行正文"),
+        "失败不得回滚已 merge 的数据，实际={:?}",
+        merged.text()
+    );
+
+    // 水位没有推进；journal 仍是 staging（schema CHECK 只有 staging/completed），
+    // 失败语义由 meta 失败标记承载。
+    assert_no_sync_watermarks(&target);
+    let journal = baseline_apply_state(&target)
+        .expect("journal state")
+        .expect("失败后 journal 必须保留");
+    assert_eq!(journal.status, BaselineApplyStatus::Staging);
+    assert!(journal.applied_docs >= 2, "已应用批次计数应保留");
+    let reason = baseline_verify_failure(&target)
+        .expect("读失败标记")
+        .expect("校验失败必须写失败标记");
+    assert!(
+        reason.contains("VV 未覆盖快照"),
+        "标记应保留失败原因：{reason}"
+    );
+
+    // 未 abort 前 begin 一律拒绝（同 checkpoint 幂等续跑也不行）。
+    let retry = bootstrap::run(
+        &target,
+        &mut relay,
+        VAULT_ID,
+        materials.peer_id_u64(),
+        &materials.root,
+    )
+    .expect_err("失败标记必须阻止 begin 续跑");
+    assert!(
+        format!("{retry}").contains("abort_baseline_apply"),
+        "错误必须提示先 abort：{retry}"
+    );
+
+    // abort 清 journal + 失败标记，但保留已 merge 的 Loro 数据。
+    assert!(abort_baseline_apply(&target).expect("abort"));
+    assert!(baseline_apply_state(&target).expect("state").is_none());
+    assert!(baseline_verify_failure(&target).expect("标记").is_none());
+    let kept = load_body_doc(&target, "note-1", local_peer, None)
+        .expect("abort 后正文")
+        .0;
+    assert!(
+        kept.text().contains("本地优先编辑") && kept.text().contains("第一行正文"),
+        "abort 只清 journal，不得删已 merge 的 Loro 数据，实际={:?}",
+        kept.text()
+    );
+
+    // abort 后可以重新 begin 并成功（失败标记已清）。
+    let report = bootstrap::run(
+        &target,
+        &mut relay,
+        VAULT_ID,
+        materials.peer_id_u64(),
+        &materials.root,
+    )
+    .expect("abort 后重跑");
+    assert_eq!(
+        report.covered_seq,
+        relay.published_checkpoints[1].covered_seq
+    );
+    assert!(report.docs.iter().any(|(doc_id, _)| doc_id == "note-1"));
+    assert!(baseline_apply_state(&target).expect("state").is_none());
+}
+
 #[test]
 fn cursor_cas_conflict_rolls_back_all_watermarks_and_retry_succeeds() {
     let (_dir, source, materials) = setup();
diff --git a/crates/sync/tests/engine.rs b/crates/sync/tests/engine.rs
index 922a57f..bd40809 100644
--- a/crates/sync/tests/engine.rs
+++ b/crates/sync/tests/engine.rs
@@ -11,10 +11,11 @@ mod support;
 use support::*;
 
 use menghunote_storage::{
-    append_updates, count_inbox_ready, load_body_doc, load_head, outbox_parts, outbox_row,
-    store_inbox_verified_batch, sync_cursor_state, take_inbox_ready, BodyDoc, StorageError,
-    SyncOutboxState, UpdateRecord, Vault, VerifiedInboxRow, APP_DOC_SCHEMA, CODEC_VERSION,
-    META_SYNC_LOG_ID, MIN_READER,
+    append_updates, asset_bytes, asset_content_hash, count_inbox_ready, create_note_slot,
+    insert_asset, link_asset_ref, load_body_doc, load_head, outbox_parts, outbox_row,
+    replace_asset_refs, store_inbox_verified_batch, sync_cursor_state, take_inbox_ready, BodyDoc,
+    StorageError, SyncOutboxState, UpdateRecord, Vault, VerifiedInboxRow, APP_DOC_SCHEMA,
+    CODEC_VERSION, META_SYNC_LOG_ID, MIN_READER,
 };
 use menghunote_sync::engine::{
     EngineConfig, EngineIdentity, IntegrateConfig, SyncEngine, RELAY_CURSOR_META,
@@ -2207,6 +2208,206 @@ fn pull_pauses_on_lagging_asset_event_epoch_before_download() {
     );
 }
 
+// ============================================================ P2-9：本地资产损坏
+//
+// `upload_one_asset` 在**本地字节与元数据不一致**时返回 `LocalCorrupt`。原实现把它
+// 和网络失败一样 `report.failed += 1` 后继续，于是每轮都"重试成功"、永不报错。
+// 约定：本地库损坏不是可重试失败——扫完其余资产后**上抛**；健康资产仍要完整上传。
+
+const ASSET_MEDIA: &str = "text/plain";
+
+/// 建一篇资产测试用的 note（`loro_docs` 行 + `notes` 行；`asset_refs` 的外键要求）。
+fn seed_asset_note(vault: &Vault, note_id: &str, peer_id: u64) {
+    seed_doc(vault, note_id, peer_id);
+    create_note_slot(vault, note_id, note_id, note_id, 1_700_000_000).expect("notes 行");
+}
+
+/// 用 storage **公开 API** 构造"元数据 hash 与本地字节不一致"的损坏资产。
+///
+/// 路径：`replace_asset_refs` 先按引用建 `data IS NULL` 的元数据占位（`byte_len`/
+/// `media_type` 也是 NULL），再用 `insert_asset` 的"补全占位明文"分支写入**另一份
+/// 字节**——该分支只校验长度、不重算内容哈希。这正是真实损坏（外部写坏/恢复错
+/// 字节）在库里的形态，且不需要测试直接改 SQL。
+fn seed_corrupt_asset(vault: &Vault, note_id: &str, declared: [u8; 32], actual: &[u8]) {
+    replace_asset_refs(vault, note_id, 1, &[(declared, 0)]).expect("元数据占位 + 引用");
+    insert_asset(
+        vault,
+        &declared,
+        actual.len() as u64,
+        Some(ASSET_MEDIA),
+        actual,
+    )
+    .expect("按占位补全明文");
+}
+
+/// 健康资产：元数据与字节自洽，并被 `asset_refs` 引用（可被增量上传）。
+fn seed_healthy_asset(vault: &Vault, note_id: &str, data: &[u8]) -> [u8; 32] {
+    let hash = asset_content_hash(data);
+    insert_asset(vault, &hash, data.len() as u64, Some(ASSET_MEDIA), data).expect("insert_asset");
+    link_asset_ref(vault, note_id, &hash, 0, 1).expect("link_asset_ref");
+    hash
+}
+
+/// durable "已上传"标记（`sync.asset_uploaded.<hex object_id>`）的键列表。
+fn asset_marker_keys(vault: &Vault) -> Vec<String> {
+    vault
+        .query(
+            "SELECT key FROM meta WHERE substr(key,1,?1)=?2 ORDER BY key",
+            &[
+                menghunote_storage::Value::Integer("sync.asset_uploaded.".len() as i64),
+                menghunote_storage::Value::Text("sync.asset_uploaded.".to_owned()),
+            ],
+        )
+        .expect("读 asset 标记")
+        .iter()
+        .map(|row| match row.first() {
+            Some(menghunote_storage::Value::Text(key)) => key.clone(),
+            _ => panic!("asset 标记 key 类型非法"),
+        })
+        .collect()
+}
+
+/// 本地损坏（hash 与字节不一致）必须**上抛**，不能被计入 `failed` 后静默继续。
+#[test]
+fn corrupt_local_asset_raises_instead_of_counting_retryable_failure() {
+    ensure_test_identity_wrap();
+    let (_dir, vault) = fresh_vault();
+    let device = Device::new(0x7c);
+    let mut relay = FakeRelay::new(&[&device]);
+    seed_asset_note(&vault, "note-corrupt", device.identity.peer_id);
+
+    // 元数据声明的是 `expected-bytes-A` 的哈希，库里实际存的是 `tampered-bytes-A`
+    // （同长、不同内容），`insert_asset` 的占位补全路径不会拦住它。
+    let declared = asset_content_hash(b"expected-bytes-A");
+    let actual = b"tampered-bytes-A";
+    seed_corrupt_asset(&vault, "note-corrupt", declared, actual);
+    assert_eq!(
+        asset_bytes(&vault, declared).expect("读本地资产"),
+        Some(actual.to_vec()),
+        "夹具前提：本地确实存着与元数据 hash 不一致的字节"
+    );
+    assert_ne!(asset_content_hash(actual), declared, "夹具必须是损坏的");
+
+    let error = {
+        let mut sync = engine(&vault, &mut relay, &device);
+        sync.upload_pending_assets()
+            .expect_err("本地库损坏必须上抛，不能计入 failed 后静默继续")
+    };
+    assert!(
+        matches!(error, SyncError::LocalCorrupt("资产字节哈希与元数据不一致")),
+        "必须原样上抛 LocalCorrupt（不是可重试网络失败）：{error:?}"
+    );
+    assert!(relay.put_history.is_empty(), "损坏资产不得上传任何分片");
+    assert!(relay.objects.is_empty(), "损坏资产不得提交对象");
+    assert!(
+        asset_marker_keys(&vault).is_empty(),
+        "损坏资产不得落 durable 标记"
+    );
+}
+
+/// 单个损坏资产不得阻止其余健康资产上传（扫描完再上抛），且下一轮不得静默"重试成功"。
+#[test]
+fn corrupt_asset_does_not_block_healthy_upload_but_still_raises() {
+    ensure_test_identity_wrap();
+    let (_dir, vault) = fresh_vault();
+    let device = Device::new(0x7d);
+    let mut relay = FakeRelay::new(&[&device]);
+
+    // 健康资产放在**后面**：`list_referenced_assets` 按 hash 升序，4ba7… < d6cc…，
+    // 损坏资产先被扫到——若实现改成 fail-fast，这条用例就会丢掉健康资产。
+    seed_asset_note(&vault, "note-corrupt", device.identity.peer_id);
+    seed_asset_note(&vault, "note-mixed", device.identity.peer_id);
+    let declared = asset_content_hash(b"expected-bytes-A");
+    seed_corrupt_asset(&vault, "note-corrupt", declared, b"tampered-bytes-A");
+    let healthy_data = b"healthy asset bytes";
+    let healthy = seed_healthy_asset(&vault, "note-mixed", healthy_data);
+    assert!(
+        declared < healthy,
+        "夹具顺序前提：损坏资产必须先于健康资产被扫描"
+    );
+
+    let error = {
+        let mut sync = engine(&vault, &mut relay, &device);
+        sync.upload_pending_assets()
+            .expect_err("存在本地损坏时整轮必须报错")
+    };
+    assert!(
+        matches!(error, SyncError::LocalCorrupt("资产字节哈希与元数据不一致")),
+        "必须原样上抛 LocalCorrupt：{error:?}"
+    );
+
+    // 健康资产仍然完整上传：分片 + 对象提交 + durable 标记。
+    let healthy_object = menghunote_sync::checkpoint::asset_object_id(
+        &device.root,
+        &healthy,
+        healthy_data.len() as u64,
+        ASSET_MEDIA,
+    )
+    .expect("健康资产 object_id");
+    assert!(
+        relay.parts.contains_key(&(healthy_object, 0)),
+        "健康资产分片必须已上传（不能被单个损坏拦住）"
+    );
+    assert_eq!(
+        relay
+            .objects
+            .get(&healthy_object)
+            .map(|object| object.commit.object_type),
+        Some(ObjectType::Asset),
+        "健康资产必须完成对象提交"
+    );
+    let markers = asset_marker_keys(&vault);
+    assert_eq!(markers.len(), 1, "只有健康资产落 durable 标记");
+    assert!(
+        markers[0].ends_with(&menghunote_sync::ids::hex_of(&healthy_object)),
+        "标记必须属于健康资产"
+    );
+
+    // 下一轮：健康资产命中标记（不重传、不再探测 relay），损坏资产仍如实上抛。
+    let puts = relay.put_history.len();
+    let gets = relay.part_gets.len();
+    let again = {
+        let mut sync = engine(&vault, &mut relay, &device);
+        sync.upload_pending_assets()
+            .expect_err("损坏仍在，下一轮必须继续报错")
+    };
+    assert!(
+        matches!(again, SyncError::LocalCorrupt("资产字节哈希与元数据不一致")),
+        "下一轮仍必须是 LocalCorrupt：{again:?}"
+    );
+    assert_eq!(
+        relay.put_history.len(),
+        puts,
+        "健康资产命中标记后不得重传分片"
+    );
+    assert_eq!(
+        relay.part_gets.len(),
+        gets,
+        "健康资产命中标记后不得再探测 relay"
+    );
+}
+
+/// 正常路径不受影响：健康的被引用资产仍计入 `uploaded`。
+#[test]
+fn healthy_referenced_asset_is_still_counted_as_uploaded() {
+    ensure_test_identity_wrap();
+    let (_dir, vault) = fresh_vault();
+    let device = Device::new(0x7e);
+    let mut relay = FakeRelay::new(&[&device]);
+    seed_asset_note(&vault, "note-healthy", device.identity.peer_id);
+    seed_healthy_asset(&vault, "note-healthy", b"healthy asset bytes");
+
+    let report = {
+        let mut sync = engine(&vault, &mut relay, &device);
+        sync.upload_pending_assets().expect("健康资产必须正常上传")
+    };
+    assert_eq!(report.referenced, 1);
+    assert_eq!(report.uploaded, 1, "正常资产仍必须计入 uploaded");
+    assert_eq!(report.failed, 0, "不得莫名计成失败");
+    assert_eq!(report.skipped_invalid, 0);
+    assert_eq!(asset_marker_keys(&vault).len(), 1);
+}
+
 // ============================================================ Phase 0：指标 + 批量 inbox
 // 只加**只读**度量与默认关闭的实验开关；不改变追赶策略、不动 relay/wire/schema。
 
diff --git a/crates/sync/tests/phase1a_release.rs b/crates/sync/tests/phase1a_release.rs
index 975e6af..b97f4d5 100644
--- a/crates/sync/tests/phase1a_release.rs
+++ b/crates/sync/tests/phase1a_release.rs
@@ -12,6 +12,11 @@
 //! 不涉及 relay / checkpoint 签名 / 真实下载：`S` 与 cursor 是自造的合法值；
 //! 每次都用 fresh vault，结束后 journal 一律 `completed`，不会给下一次留 staging。
 //!
+//! 第二个探针 [`checkpoint_validate_scan_cost`] 测 **checkpoint validate 的 coverage
+//! 日志扫描 O(S) 成本**：FakeRelay 构造 S = 1k / 5k / 25k 条混合日志（85.7% UPDATE +
+//! 14.3% ASSET），走真实 `CheckpointWriter::prepare → validate`（不复制 scan 实现），
+//! 只打印页数/墙钟，不 assert 时间。
+//!
 //! ```bash
 //! cargo test --release -p menghunote-sync --test phase1a_release -- --ignored --nocapture
 //! ```
@@ -20,16 +25,30 @@ mod support;
 
 use std::time::Instant;
 
-use support::{commit_local, fresh_vault, seed_doc};
+use support::{
+    commit_local, commit_with_intent, fresh_vault, publish_real_asset, seed_doc, uuid_like,
+    FakeRelay, TestRandom, EPOCH, VAULT_ID,
+};
 
 use menghunote_storage::{
     apply_baseline_batch, baseline_apply_debug_counters, baseline_apply_debug_timings,
-    baseline_apply_state, begin_baseline_apply, finish_baseline_apply, load_body_doc,
-    reset_baseline_apply_debug, sync_cursor_state, BaselineApplyDebugCounters,
-    BaselineApplyDebugTimings, BaselineApplyFinish, BaselineApplyStatus, BodyDoc, Value, Vault,
-    BASELINE_MERGE_BATCH, META_SYNC_CONFIRMED_HIGHWATER, META_SYNC_INTEGRATED_SEQ,
+    baseline_apply_state, begin_baseline_apply, create_note_slot, ensure_structure_row,
+    finish_baseline_apply, load_body_doc, reset_baseline_apply_debug, sync_cursor_state,
+    BaselineApplyDebugCounters, BaselineApplyDebugTimings, BaselineApplyFinish,
+    BaselineApplyStatus, BaselineExpectation, BodyDoc, Value, Vault, BASELINE_MERGE_BATCH,
+    META_SYNC_CONFIRMED_HIGHWATER, META_SYNC_INTEGRATED_SEQ, META_SYNC_LOG_ID,
     META_SYNC_RECEIVED_SEQ, META_SYNC_RELAY_CURSOR,
 };
+use menghunote_sync::checkpoint::{CheckpointWriter, EmptyAssets, PreparedCheckpoint};
+use menghunote_sync::engine::RELAY_CURSOR_META;
+use menghunote_sync::genesis;
+use menghunote_sync::relay::{
+    BootstrapBundle, BootstrapLease, CheckpointLease, CheckpointPublish, CheckpointRef,
+    ObjectSubmission, PublishedCheckpoint, RelayClient, RelayLogKind, RelayLogPage, RelayState,
+    SlotRecord, SlotUpload, UpdateOutcome, UpdateSubmission,
+};
+use menghunote_sync::{DeviceId, ObjectId, SyncError};
+use menghunote_sync_proto::DeviceCertV1;
 
 /// 基线文档数（真实信任链里 docs 数量更多；这里固定 20 便于两档总量均分）。
 const DOCS: usize = 20;
@@ -216,11 +235,17 @@ fn run_probe(label: &'static str, target_bytes: usize) -> ProbeStats {
     // 3) 持闸段：begin → apply 各批 → finish。
     reset_baseline_apply_debug();
     let begin_started = Instant::now();
-    let begin_state =
-        begin_baseline_apply(&vault, &[0x51; 16], BASELINE_S, "c0-source").expect("begin baseline");
+    let expected =
+        BaselineExpectation::new(doc_ids.iter().cloned(), std::iter::empty::<[u8; 32]>());
+    let begin_state = begin_baseline_apply(&vault, &[0x51; 16], BASELINE_S, "c0-source", &expected)
+        .expect("begin baseline");
     let begin_ms = begin_started.elapsed().as_millis();
     assert_eq!(begin_state.status, BaselineApplyStatus::Staging);
-    assert!(begin_state.finishable);
+    assert_eq!(begin_state.expected_docs, Some(DOCS as u64));
+    assert!(
+        !begin_state.finishable,
+        "begin 时预期 docs 尚未应用，finishable 必须为 false"
+    );
 
     let entries: Vec<(String, Vec<u8>)> = doc_ids
         .iter()
@@ -427,3 +452,422 @@ fn phase1a_baseline_apply_gate_hold() {
         print_probe(&stats);
     }
 }
+
+// ============================================================ checkpoint validate 覆盖日志扫描成本
+//
+// 任务 4 P1：只测真实 `CheckpointWriter::validate` 里 coverage 日志扫描的 O(S) 成本，
+// 不改生产 `scan_coverage_log`，也不复制它的实现。S 条日志是合成的（FakeRelay）：
+// `1..=S-1` 落在本地 integrated 连续前缀（生产代码信任的"已消费"不变量），尾部第 S 条
+// 是真实 ASSET（prepare 落 pending receipt，validate 必须下载实证）；前缀里按 1/7
+// 混入 ASSET 事件（≈14.3%，任务要求的 10–20%）。
+//
+// 口径：
+// - `pages` = validate 期间真实 `RelayClient::log` 调用次数（应从 c0 扫到 S，S/256 页）。
+// - `scan_ms` = 第一次 log 开始到最后一次 log 返回的墙钟跨度（含页间逐事件核对；
+//   不含最后一页返回之后的 256 条 visitor，误差上界 = 一页）。
+// - `validate_ms` = 整个 validate 墙钟（含 manifest/快照读回、尾部 ASSET 实证）。
+// 只打印不 assert 时间；硬断言只校验覆盖到了 S、尾部 ASSET 被真实验证、页数符合 S/256。
+
+/// 探针档位（事件总数 S）。
+const SCAN_COST_S: [usize; 3] = [1_000, 5_000, 25_000];
+/// 每档重复次数（取中位数）。
+const SCAN_COST_RUNS: usize = 3;
+/// 生产 `COVERAGE_SCAN_PAGE_LIMIT`（`crates/sync/src/checkpoint.rs` 私有常量 = 256）。
+/// 只用于校验探针确实按 S/256 页扫完整段；若生产改页大小，这里应同步更新。
+const SCAN_COST_PAGE_LIMIT: usize = 256;
+/// ASSET 事件间隔（每 7 条一条 → 14.3%）。
+const SCAN_COST_ASSET_EVERY: usize = 7;
+/// 探针正文文档 id（与 coverage 无关，只给 prepare 提供可导出的本地快照）。
+const SCAN_COST_NOTE_ID: &str = "scan-cost-note";
+
+/// 计数/计时包装：所有方法委托给 FakeRelay；只额外统计 `log()`（真实 coverage scan
+/// 发起的页请求）与 `part()`（manifest/snapshot/ASSET 读回）的调用次数与墙钟。
+///
+/// 这是**探针侧 instrumentation**，不是 scan 实现的拷贝：validate 仍走生产
+/// `CheckpointWriter::validate` → `scan_coverage_log`，wrapper 只是它调用的客户端。
+struct CountingRelay {
+    inner: FakeRelay,
+    log_calls: u64,
+    log_busy_ns: u128,
+    scan_first: Option<Instant>,
+    scan_last: Option<Instant>,
+    part_calls: u64,
+    part_busy_ns: u128,
+}
+
+impl CountingRelay {
+    fn new(inner: FakeRelay) -> Self {
+        Self {
+            inner,
+            log_calls: 0,
+            log_busy_ns: 0,
+            scan_first: None,
+            scan_last: None,
+            part_calls: 0,
+            part_busy_ns: 0,
+        }
+    }
+
+    fn reset_counters(&mut self) {
+        self.log_calls = 0;
+        self.log_busy_ns = 0;
+        self.scan_first = None;
+        self.scan_last = None;
+        self.part_calls = 0;
+        self.part_busy_ns = 0;
+    }
+
+    /// 第一次到最后一次 `log()` 返回的墙钟跨度（页扫描口径）。
+    fn scan_span_ms(&self) -> f64 {
+        match (self.scan_first, self.scan_last) {
+            (Some(first), Some(last)) => last.duration_since(first).as_secs_f64() * 1e3,
+            _ => 0.0,
+        }
+    }
+
+    /// 仅 `log()` 内部（FakeRelay 取页/克隆事件）的墙钟合计。
+    fn log_busy_ms(&self) -> f64 {
+        self.log_busy_ns as f64 / 1e6
+    }
+
+    /// `part()` 读回墙钟合计（manifest/snapshot/ASSET）。
+    fn part_busy_ms(&self) -> f64 {
+        self.part_busy_ns as f64 / 1e6
+    }
+}
+
+impl RelayClient for CountingRelay {
+    fn state(&mut self) -> Result<RelayState, SyncError> {
+        self.inner.state()
+    }
+
+    fn log(&mut self, cursor: Option<&str>, limit: u32) -> Result<RelayLogPage, SyncError> {
+        if self.scan_first.is_none() {
+            self.scan_first = Some(Instant::now());
+        }
+        let started = Instant::now();
+        let result = self.inner.log(cursor, limit);
+        self.log_busy_ns += started.elapsed().as_nanos();
+        self.log_calls += 1;
+        self.scan_last = Some(Instant::now());
+        result
+    }
+
+    fn part(&mut self, object_id: &ObjectId, part_no: u32) -> Result<Vec<u8>, SyncError> {
+        let started = Instant::now();
+        let result = self.inner.part(object_id, part_no);
+        self.part_busy_ns += started.elapsed().as_nanos();
+        self.part_calls += 1;
+        result
+    }
+
+    fn put_part(
+        &mut self,
+        object_id: &ObjectId,
+        part_no: u32,
+        envelope: &[u8],
+    ) -> Result<(), SyncError> {
+        self.inner.put_part(object_id, part_no, envelope)
+    }
+
+    fn submit_updates(
+        &mut self,
+        items: Vec<UpdateSubmission>,
+    ) -> Result<Vec<UpdateOutcome>, SyncError> {
+        self.inner.submit_updates(items)
+    }
+
+    fn submit_object(&mut self, item: ObjectSubmission) -> Result<UpdateOutcome, SyncError> {
+        self.inner.submit_object(item)
+    }
+
+    fn ack(&mut self, received_seq: u64, integrated_seq: u64, epoch: u64) -> Result<(), SyncError> {
+        self.inner.ack(received_seq, integrated_seq, epoch)
+    }
+
+    fn device_cert(&mut self, device_id: &DeviceId) -> Result<Option<DeviceCertV1>, SyncError> {
+        self.inner.device_cert(device_id)
+    }
+
+    fn put_key_slot(&mut self, upload: SlotUpload) -> Result<SlotRecord, SyncError> {
+        self.inner.put_key_slot(upload)
+    }
+
+    fn list_key_slots(&mut self) -> Result<Vec<SlotRecord>, SyncError> {
+        self.inner.list_key_slots()
+    }
+
+    fn activate_password_slot(
+        &mut self,
+        expected_slot_set_rev: u64,
+        slot_id: ObjectId,
+        signed_message: Vec<u8>,
+        signature: [u8; 64],
+    ) -> Result<u64, SyncError> {
+        self.inner
+            .activate_password_slot(expected_slot_set_rev, slot_id, signed_message, signature)
+    }
+
+    fn create_checkpoint(
+        &mut self,
+        estimated_bytes: u64,
+        epoch: Option<u64>,
+    ) -> Result<CheckpointLease, SyncError> {
+        self.inner.create_checkpoint(estimated_bytes, epoch)
+    }
+
+    fn add_checkpoint_refs(
+        &mut self,
+        checkpoint_id: [u8; 16],
+        refs: Vec<CheckpointRef>,
+    ) -> Result<u64, SyncError> {
+        self.inner.add_checkpoint_refs(checkpoint_id, refs)
+    }
+
+    fn publish_checkpoint(
+        &mut self,
+        publish: CheckpointPublish,
+    ) -> Result<PublishedCheckpoint, SyncError> {
+        self.inner.publish_checkpoint(publish)
+    }
+
+    fn bootstrap(&mut self) -> Result<BootstrapBundle, SyncError> {
+        self.inner.bootstrap()
+    }
+
+    fn renew_bootstrap_lease(&mut self, lease_id: [u8; 16]) -> Result<BootstrapLease, SyncError> {
+        self.inner.renew_bootstrap_lease(lease_id)
+    }
+}
+
+/// 一次性夹具：真实 SQLite vault + 合成 S 条日志 + 已 prepare 的 checkpoint。
+struct ScanCostFixture {
+    _dir: tempfile::TempDir,
+    vault: Vault,
+    materials: genesis::GenesisMaterials,
+    relay: CountingRelay,
+    prepared: PreparedCheckpoint,
+    asset_events: usize,
+    update_events: usize,
+    prepare_pages: u64,
+    prepare_scan_ms: f64,
+    prepare_log_ms: f64,
+}
+
+fn build_scan_cost_fixture(total_events: usize) -> ScanCostFixture {
+    assert!(
+        total_events > 1,
+        "S 至少需要一条前缀 UPDATE 与一条尾部 ASSET"
+    );
+    let (dir, vault) = fresh_vault();
+    ensure_structure_row(&vault).expect("structure 行");
+    vault
+        .set_meta(RELAY_CURSOR_META, b"c0")
+        .expect("写 relay 游标");
+    let materials = genesis::build_materials(
+        VAULT_ID,
+        [0x44; 16],
+        "pw",
+        1_700_000_000,
+        &mut TestRandom::new(0x88_14),
+    )
+    .expect("genesis 材料");
+    let mut relay = FakeRelay::new(&[]);
+    relay
+        .certs
+        .insert(materials.device_id, materials.certificate().clone());
+    vault
+        .set_meta(META_SYNC_LOG_ID, &relay.log_id)
+        .expect("写 log_id");
+
+    // 一篇真实正文：prepare 需要可导出的本地快照/head VV（与 coverage 无关）。
+    let doc = seed_doc(&vault, SCAN_COST_NOTE_ID, materials.peer_id_u64());
+    create_note_slot(
+        &vault,
+        SCAN_COST_NOTE_ID,
+        SCAN_COST_NOTE_ID,
+        SCAN_COST_NOTE_ID,
+        1_700_000_000,
+    )
+    .expect("notes 行");
+    doc.insert_scalars(0, "checkpoint coverage scan cost probe")
+        .expect("正文");
+    commit_with_intent(&vault, &doc, &uuid_like(9_001), [0x91; 32], EPOCH);
+
+    // 1..S-1 按 1/7 混入 ASSET；第 S 条是真 ASSET。前 S-1 条都属于本地
+    // integrated 连续前缀（只借生产代码信任的"已消费"不变量，不读资产），
+    // 唯一缺口是尾部 ASSET → prepare 落 pending receipt，validate 必须下载实证。
+    let mut asset_events = 0usize;
+    for seq in 1..total_events {
+        if seq % SCAN_COST_ASSET_EVERY == 0 {
+            relay.publish_event(RelayLogKind::Asset, [0x2a; 32]);
+            asset_events += 1;
+        } else {
+            relay.publish_event(RelayLogKind::Update, [0x15; 32]);
+        }
+    }
+    let (_, tail_seq) = publish_real_asset(
+        &mut relay,
+        VAULT_ID,
+        materials.device_id,
+        &materials.signing_key,
+        &materials.root,
+        b"coverage scan cost tail asset",
+        "text/plain",
+    );
+    assert_eq!(
+        tail_seq,
+        u64::try_from(total_events).expect("S 溢出 u64"),
+        "尾部 ASSET 必须恰为第 S 条日志"
+    );
+    asset_events += 1;
+    let update_events = total_events - asset_events;
+    assert!(
+        (update_events) > 0,
+        "混合日志必须至少有一条 UPDATE（实际 S={total_events}）"
+    );
+
+    let prefix = u64::try_from(total_events - 1).expect("S 溢出 u64");
+    vault
+        .set_meta(META_SYNC_INTEGRATED_SEQ, &prefix.to_be_bytes())
+        .expect("写 integrated 前缀");
+    vault
+        .set_meta(META_SYNC_RECEIVED_SEQ, &prefix.to_be_bytes())
+        .expect("写 received 前缀");
+
+    // 从这里开始的页请求才算探针成本（prepare 的 pending receipt 扫描也是 O(S)）。
+    let mut relay = CountingRelay::new(relay);
+    let prepared = {
+        let mut writer = CheckpointWriter::new(
+            &vault,
+            &mut relay,
+            VAULT_ID,
+            materials.device_id,
+            materials.signing_key.clone(),
+            materials.root.clone(),
+            materials.peer_id_u64(),
+        );
+        writer.prepare(&EmptyAssets).expect("prepare")
+    };
+    assert_eq!(
+        prepared.report.covered_seq,
+        u64::try_from(total_events).expect("S 溢出 u64")
+    );
+    assert_eq!(
+        relay.log_calls,
+        u64::try_from(total_events.div_ceil(SCAN_COST_PAGE_LIMIT)).expect("页数溢出 u64"),
+        "prepare 的 receipt 扫描应从 c0 扫满 S/256 页"
+    );
+    let prepare_pages = relay.log_calls;
+    let prepare_scan_ms = relay.scan_span_ms();
+    let prepare_log_ms = relay.log_busy_ms();
+    relay.reset_counters();
+
+    ScanCostFixture {
+        _dir: dir,
+        vault,
+        materials,
+        relay,
+        prepared,
+        asset_events,
+        update_events,
+        prepare_pages,
+        prepare_scan_ms,
+        prepare_log_ms,
+    }
+}
+
+fn median_ms(values: &[f64]) -> f64 {
+    let mut sorted = values.to_vec();
+    sorted.sort_by(|a, b| a.partial_cmp(b).expect("时间不能是 NaN"));
+    sorted[sorted.len() / 2]
+}
+
+fn run_scan_cost(total_events: usize) {
+    let mut fixture = build_scan_cost_fixture(total_events);
+    let asset_pct = fixture.asset_events as f64 * 100.0 / total_events as f64;
+    eprintln!(
+        "[phase1a] scan_cost setup S={total_events} asset_events={} update_events={} \
+         asset_pct={asset_pct:.1} prepare_pages={} prepare_scan_ms={:.3} prepare_log_ms={:.3}",
+        fixture.asset_events,
+        fixture.update_events,
+        fixture.prepare_pages,
+        fixture.prepare_scan_ms,
+        fixture.prepare_log_ms,
+    );
+
+    let mut pages = 0u64;
+    let mut part_calls = 0u64;
+    let mut raw_scan = Vec::with_capacity(SCAN_COST_RUNS);
+    let mut raw_validate = Vec::with_capacity(SCAN_COST_RUNS);
+    let mut raw_log = Vec::with_capacity(SCAN_COST_RUNS);
+    let mut raw_part = Vec::with_capacity(SCAN_COST_RUNS);
+    for run in 0..SCAN_COST_RUNS {
+        fixture.relay.reset_counters();
+        let started = Instant::now();
+        let baseline = {
+            let mut writer = CheckpointWriter::new(
+                &fixture.vault,
+                &mut fixture.relay,
+                VAULT_ID,
+                fixture.materials.device_id,
+                fixture.materials.signing_key.clone(),
+                fixture.materials.root.clone(),
+                fixture.materials.peer_id_u64(),
+            );
+            writer.validate(&fixture.prepared).expect("validate")
+        };
+        let validate_ms = started.elapsed().as_secs_f64() * 1e3;
+        assert_eq!(
+            baseline.covered_seq(),
+            u64::try_from(total_events).expect("S 溢出 u64")
+        );
+        assert_eq!(
+            baseline.coverage_verified_candidates(),
+            1,
+            "尾部 ASSET 必须被真实下载验证（receipt 路径）"
+        );
+        assert_eq!(baseline.coverage_durable_assets(), 0);
+
+        let run_pages = fixture.relay.log_calls;
+        let scan_ms = fixture.relay.scan_span_ms();
+        let log_ms = fixture.relay.log_busy_ms();
+        let run_parts = fixture.relay.part_calls;
+        let part_ms = fixture.relay.part_busy_ms();
+        assert_eq!(
+            run_pages,
+            u64::try_from(total_events.div_ceil(SCAN_COST_PAGE_LIMIT)).expect("页数溢出 u64"),
+            "validate 的 coverage scan 应从 c0 扫满 S/256 页"
+        );
+        assert!(run_parts >= 1, "validate 至少读回 manifest/快照/尾部 ASSET");
+        pages = run_pages;
+        part_calls = run_parts;
+        eprintln!(
+            "[phase1a] scan_cost S={total_events} run={run} pages={run_pages} \
+             scan_ms={scan_ms:.3} validate_ms={validate_ms:.3} log_ms={log_ms:.3} \
+             part_calls={run_parts} part_ms={part_ms:.3}",
+        );
+        raw_scan.push(scan_ms);
+        raw_validate.push(validate_ms);
+        raw_log.push(log_ms);
+        raw_part.push(part_ms);
+    }
+
+    let scan_median = median_ms(&raw_scan);
+    let validate_median = median_ms(&raw_validate);
+    let log_median = median_ms(&raw_log);
+    eprintln!(
+        "[phase1a] scan_cost S={total_events} pages={pages} scan_ms={scan_median:.3} \
+         (median of {SCAN_COST_RUNS}; raw_scan_ms={raw_scan:?} log_ms_median={log_median:.3} \
+         validate_ms_median={validate_median:.3} validate_ms_raw={raw_validate:?} \
+         part_calls={part_calls} part_ms_raw={raw_part:?})"
+    );
+}
+
+#[test]
+#[ignore = "manual benchmark"]
+fn checkpoint_validate_scan_cost() {
+    for total_events in SCAN_COST_S {
+        run_scan_cost(total_events);
+    }
+}

`````
