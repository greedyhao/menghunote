# 同步评审 Part 1/6：核心代码与任务证据

@@PART 1/6

> 这是 **Part 1/6**。请先只确认收到，不要开始评审。
> 平台在约 72 万字符处会截断；本部分约 502,846 字符，低于上限。
> 后续 Part 2–6 会继续上传；全部到齐后再按最终提示词评审。
> 代码截止 `1bededa`，文档提交 `7a523bc`；Phase 1B 未实现。

## 收到确认（请只回复这三行）

1. `PART 1/6 RECEIVED`
2. 本部分 `@@SECTION` 数量（应为 17）
3. 最后一个完整文件/函数

本部分含：任务 1–4 状态、P1/P2 残余、`sync-hardening`/`critical`/`review-index` 文档、
`baseline_apply.rs`、`session/flush.rs`、`body_doc.rs`、`store.rs`、`sync_meta.rs`、
`actor.rs`、`remote_known.rs`、`journal.rs`、`error.rs`、`sync_bridge.rs`、`engine.rs`、
`bootstrap.rs`、`session_flush.rs` 测试、`body_doc_detached_oplog.rs` 测试。

---

## 0. 本轮结果（与代码/测试对应）

| 任务 | 状态 | 关键证据 |
|---|---|---|
| 1 HeadConflict 生产接线 | 完成 | `flush_recovers_from_head_conflict_by_rebuilding_sink`、`flush_reports_unrecoverable_after_bounded_rebuild`、`flush_reports_unrecoverable_when_new_head_is_unreadable`；`session_flush` 9 passed |
| 2 detached oplog 矩阵 | 完成 | `body_doc_detached_oplog` 11 passed（无 ignore），5 类/11 变体 |
| 3 verify 失败语义 | 完成 | `verify_accepts_concurrent_local_edits_after_export`、`verify_failure_keeps_merged_data_and_marks_journal_failed`；文档 §8 |
| 4 覆盖扫描成本 | 完成（探针） | `checkpoint_validate_scan_cost`；metrics §8.7；S=1k/5k/25k |
| 5 幂等只读预检 | 未做（可选 P2） | 不影响本轮 P0/P1 正确性论证 |

**测试计数（当前工作树）**：
```text
storage 288 passed / 0 failed / 1 ignored (manual bench)
sync 636 passed / 0 failed / 4 ignored (manual/probes)
app default 740 / dev-sync 891
workspace 2900 / 0
workspace dev-sync 3051 / 0
release phase1a 2 passed (baseline + scan probe)
fmt / clippy workspace -D warnings 全绿
```

**P1-3 残余（必须知道）**：自动重建只在“新 head 的 ops 全部包含在未 ACK 前缀最后一条
end_vv”时可安全重试同一字节前缀；若外部 head 含本会话未知 ops，则 fail-closed 进入
“需要重开文档”，避免把 head_vv/frontier 写成缺外部 ops 的旧快照。`session/flush.rs`
仍直接 `VaultJournalSink`（不是 `JournalWriter`），没有第二 journal，单后台线程；真正的
跨会话重导出 rebase 未在本轮实现（超出只改 flush.rs 的边界）。

**任务 4 实测**：S=1k pages=4 median scan_ms≈0.076；5k pages=20≈0.428；25k pages=98≈4.039；
FakeRelay 的 `log()` 每页扫描是夹具 artifact，真实成本主要是 98 次串行 RTT；prepare+validate
最多 2×S 事件访问，见 metrics §8.7。

---

# 附录 A：文档全文



---

# 本部分附录：核心代码/测试

@@SECTION docs/01 `docs/notes/sync-hardening-20261121.md`


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



@@SECTION docs/02 `docs/notes/sync-critical-modules-review.md`


`````markdown
# 同步 Phase 1A 关键模块评审（单文件版）

> **用途**：只上传这一份，评审 Phase 1A 的 6 个关键模块与 Phase 1B 阈值，不用再带
> 多个代码文件。代码基线 `5d01e9e`；Phase 1B **未实现**。
> 需要逐行看完整代码时，用文中 `路径:行号` 回仓库取即可。

## 0. 请评审者回答的问题

1. 聚合预检 + 同文档多轮 import，是否严格保持 Loro 因果与 `integrated` 连续前缀？
2. `BEGIN IMMEDIATE` 在单 writer actor 下是否足够；纯幂等重放也取写锁是否可接受？
3. `remote_known_vv` 的单调性、旧库追溯与 local-only 保护是否成立？
4. detached oplog 合并是否在所有 Loro 操作下与 attached 状态等价？
5. 25k / 16 MiB / 3 s / 2.0× / 1.5× 的 Phase 1B 阈值是否成立？
6. 还有哪些必须修/必须补测的阻断？

## 1. 状态与边界

**已实现（Phase 1A）**

- pull 早退前 flush；批量 inbox 单事务；正文按 doc 聚合；`load_head` 预检；
  多轮 import 自愈物理乱序；选批防饥饿；baseline 单次解码 + detached oplog 合并；
  remote-known VV；skipped 窗口有界扫描。

**未实现（Phase 1B）**

- 没有 `CatchUpMode`、`catchup_prepare`/`catchup_integrate` 拆分、真实 `/bootstrap`
  下载/验签、资产只补缺失、`prune_covered_inbox`、UI/状态机。

**本轮未改**：relay 服务端、`sync-proto` wire、schema、checkpoint 签名、游标语义。
只新增本地 `meta` 键 `sync.remote_known_vv.<doc_id>`（非 schema、不同步）。

**不可违反的契约（ADR 0006 / 0012）**

- 网络阶段不持写闸；integrate/adopt 短持闸。
- 触发窗口只能用 `S - received_seq`，不能用 `head - received`。
- fallback 必须绑定实际选中的 `S` / `cursor`。
- `integrated_seq` 只连续推进，缺口处不跳号 ACK。
- final `integrated >= S` 前不得删 inbox / 覆盖证明；`skipped_asset` 不能误删。
- epoch/rotation 控制边界未处理 → `Paused`，不得当损坏降级。

## 2. 模块 A：聚合预检与同文档多轮 import

文件：`crates/sync/src/engine.rs:1837`（`integrate_body_doc_group`）、
`crates/sync/src/engine.rs:2669`（`body_group_readiness`）。

### 2.1 预检（不 load_body_doc）

```rust
enum BodyGroupReadiness {
    Covered,  // 所有行 end_vv ⊆ head_vv：只标记
    Ready,    // 至少一行 base_vv ⊆ head_vv：需要 import
    Deferred, // 未覆盖行的 base_vv 全不满足：纯缺外部依赖，跳过
}

fn body_group_readiness(
    pending: &[PendingInboxRow],
    indices: &[usize],
    head_vv: &VersionVector,
) -> BodyGroupReadiness {
    let mut uncovered = false;
    for &index in indices {
        let Some(payload) = pending[index].row.verified_payload.as_deref() else {
            return BodyGroupReadiness::Ready; // 解码失败交给真正路径报错
        };
        let Ok(update) = UpdateV1::decode(payload) else {
            return BodyGroupReadiness::Ready;
        };
        let Ok(end_vv) = decode_vv(&update.end_vv, "end_vv 解码失败") else {
            return BodyGroupReadiness::Ready;
        };
        if vv_includes(head_vv, &end_vv) {
            continue;
        }
        uncovered = true;
        let Ok(base_vv) = decode_vv(&update.base_vv, "base_vv 解码失败") else {
            return BodyGroupReadiness::Ready;
        };
        if vv_includes(head_vv, &base_vv) {
            return BodyGroupReadiness::Ready;
        }
    }
    if uncovered { BodyGroupReadiness::Deferred } else { BodyGroupReadiness::Covered }
}
```

关键点：`Covered` 直接 `mark_integrated_batch`、`doc_loads=0`；`Deferred` 直接返回，
不读正文、不占选批预算；只有 `Ready` 才 `load_body_doc`。

### 2.2 多轮 import

```text
remaining = indices
loop:
  next = []
  round_imported = 0
  for row in remaining:
    if doc.vv() ⊇ row.end_vv:      // 已应用/被本轮覆盖
      mark row
    else if doc.vv() ⊇ row.base_vv:
      doc.import(row.payload)
      mark row; round_imported += 1
    else:
      next.push(row)               // 真缺外部依赖
  remaining = next
  if round_imported == 0: break    // 多扫无意义
```

所有成功 import 的 op 只用一次 `doc.pending_update(&before)` 导出，和全部可标记行
（含已覆盖行）进入同一笔 `append_updates_with_integrated_marks`；整批之后才推进一次
`integrated` 前缀。不能因为中间某行 deferred 就丢弃已成功子集。

### 2.3 选批防饥饿

`crates/sync/src/engine.rs:2562`：

- 每个 `(doc, log)` 子组先 readiness；
- `Deferred` 不进 selected、不占 `batch_bytes`、不占 `batch_objects`；
- `Covered` 进 selected 以便标记，按 0 字节计；
- `Ready` 才计入软预算；首条 ready 行永远放行（单行超大单独成批）。

### 2.4 测试与证据

- `aggregate_body_multi_round_heals_physical_out_of_order`
- `aggregate_body_precheck_skips_large_deferred_docs_without_starving_ready_doc`
- 纯已覆盖 `doc_loads=0`；256 行 `doc_loads=1`；物理乱序 `doc_loads=1`；
  `aggregate_body=false` 旧路径仍通过。
- `crates/sync/tests/engine.rs` 45 个用例。

### 2.5 请确认

1. 预检只信 `loro_docs.head_vv` 是否足够？是否还有旁路只改 history 不改 head？
2. 多轮 import 不要求 relay_seq 连续，是否与 Loro 的 op 依赖语义完全一致？
3. 部分行 deferred 后，`integrated` 标记与前缀缺口证明是否仍严格？
4. `Covered` 标记路径没有 head CAS（只标记 inbox），在并发 writer 下是否安全？

## 3. 模块 B：baseline_apply 单次解码 + detached oplog

文件：`crates/storage/src/baseline_apply.rs:334`、`crates/storage/src/body_doc.rs`。

### 3.1 分支逻辑

```text
baseline_doc = BodyDoc::restore(snapshot)     // 唯一一次 snapshot 解码
head = load_head(doc_id); local_vv = head.vv()
if !exists:
    update = baseline_doc.pending_update(default)
else if local_vv ⊆ baseline_vv:              // no local_extra
    update = (baseline_vv == local_vv) ? none
             : baseline_doc.pending_update(&local_vv)
else:                                        // local_extra
    local_doc = load_body_doc(doc_id)         // 仅这里一次
    local_delta = local_doc.pending_update(&baseline_vv)
    baseline_doc.import_oplog_only(local_delta)  // detached: 只合 oplog
    update = baseline_doc.pending_update_from_oplog(&local_vv)
```

- 旧实现：`BodyDoc::restore` 后再 `doc.import(snapshot)`，二次解码/导入大快照；
  已有本地文档走 attached import，成本占旧 gate 97.9–99.8%。
- 新实现：snapshot 只 decode 一次；仅取 VV 用 `load_head`；本地小 delta 只合
  **oplog**，不重算 state/diff；再导出差集。
- `import_oplog_only` 之后只允许 `pending_update_from_oplog` /
  `oplog_frontiers_encoded`，**不得读正文**。

### 3.2 实测

| 档 | 旧 gate_hold | 新 gate_hold | 闸外 verify |
|---|---:|---:|---:|
| 5 MiB | 2 718–4 143 ms | **35 ms** | 2 686 ms |
| 20 MiB | 44 574–61 308 ms | **110 ms** | 45 103 ms |
| 5→20 | 14.8–16.9× | **3.1×** | 16.7× |

根因：不是 decode（3.4×）、不是 pending_update（3.35×），是 Loro attached import
的 diff/state 应用（15.1–16.6×）。detached 反向导入 0.2 ms。

### 3.3 请确认

1. `oplog_frontiers` 与 state frontier 在所有 Loro 操作（多 peer、容器删除/移动、
   并发本地编辑）下是否总能一致？
2. detached 文档只导出、不读 state 的约束是否足够，还是需要类型层隔离？
3. 闸外 verify 仍 16.7×；是否必须先落 `loro_docs.snapshot`/流式重放，才启用机会性
   追赶？还是可以先接受 20 MiB 形状的 45 s 首次重放？

## 4. 模块 C：remote-known VV 与 tombstone

文件：`crates/storage/src/remote_known.rs`、`baseline_apply.rs:964`、
`store.rs:416`、`journal.rs`、`sync_bridge.rs`。

### 4.1 语义

`sync.remote_known_vv.<doc_id>` = 远端已确证见过的 oplog 前缀，只由两条路径扩大：

1. 应用 baseline snapshot：写入快照 VV；
2. 集成远端 update：`end_vv - 旧 head_vv` 并入。

本地写入不扩大（即使已 ACK）。tombstone 判定：

```text
if sync_outbox 有行: keep
if !doc_exists: deleted
if 本批有 snapshot:
    local_extra ? keep : deleted
if remote_known:
    local_vv ⊆ known ? deleted : keep
if 旧 journal baseline_vv:
    local_vv ⊆ baseline ? deleted : keep
else:
    keep   // local-only，绝不误删
```

### 4.2 已知残余（重要）

**旧库**（本次改动前普通 pull 同步过、之后再无远端 update）没有 remote-known 记录，
第一次追赶仍 `kept_local_edit`。这是保守行为，不会误删 local-only；要彻底追溯需
Phase 1B 从 manifest `ManifestDoc.vv` / 远端事件回填。**不能**凭 `head_seq>0` 猜。

### 4.3 请确认

1. `merge_statement` 在 A 事务外读旧值，并发 flush 下是否只导致“多保留”，不会少保留？
2. begin 回填旧 `baseline_apply_docs.baseline_vv` 的 SQL 是否覆盖所有旧 journal 状态？
3. manifest VV 回填方案会不会把本地未同步编辑误归入 remote-known？需要什么测试？

## 5. 模块 D：BEGIN IMMEDIATE

文件：`crates/storage/src/actor.rs:468`（`exec_inbox_batch`）。

```rust
// 先读（log_id/既有行）后写（插新行/推进 received）。
// Deferred 的锁升级不走 busy_timeout，可能直接 SQLITE_BUSY；
// Immediate 在事务开始取写锁，这一步仍受 5s busy_timeout 约束。
let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
```

测试：第二个 Vault 先 `BEGIN IMMEDIATE` 占写锁；批量入口设 `busy_timeout=0`，
断言整笔 `SQLITE_BUSY`、零半写；释放后重放得到真实 `SyncConflict`。回归证明退回
deferred 后该用例会失败（先读旧快照）。

**问题**：纯幂等重放也会取写锁；单 writer actor 下可接受吗？是否需要 BUSY 有界重试？

## 6. 模块 E：skipped_received 有界扫描

文件：`crates/storage/src/sync_meta.rs:209`。

```sql
SELECT relay_seq FROM sync_inbox
 WHERE log_id=?1 AND relay_seq>?2 AND <condition>
 ORDER BY relay_seq LIMIT ?3
```

```sql
SELECT key FROM meta WHERE key>=?1 AND key<=?2
 ORDER BY key ASC LIMIT ?3
```

- 两个窗口各自 `LIMIT PREFIX_SCAN_LIMIT=4096`，各自记录 `inbox_truncated` /
  `skipped_truncated` 与窗口内最大序号。
- `truncated()` 为两者并集。
- 缺口证明 `proves_gap_at(gap)`：每个被截断来源的最大观测序号必须 ≥ gap；
  否则继续下一窗口。修复了“skipped 截断在 4096、inbox 有 5001 时提前 return”
  导致前缀卡死的问题。
- 测试：1..=5000 skipped + inbox 5001 → received=5001；缺口停在 4096；
  两窗口同时截断推进到 8200。

**问题**：`proves_gap_at` 的边界是否严密？格式不可解析的 meta 键占满窗口时
fail-safe 会卡住前缀，是否需要在真实 corruption 场景中报警？

## 7. 模块 F：Phase 1B 阈值（待确认）

| 项 | 当前默认 | 依据 |
|---|---|---|
| 强制追赶 | `gc_floor > received_seq` | 普通 pull 会 CursorExpired |
| 强制失败 | `Paused` + 指数退避，**不回退 pull** | 防死循环 |
| 强制体积 | 不受机会性上限；仅本地磁盘预算 | 必须离开过期游标 |
| 机会性事件窗口 | `S - received_seq >= 25_000` | 5000 条尾部仅 0.28–0.76 s，不足以摊平全链路 |
| 机会性快照上限 | **16 MiB** 密文（可配 8–16） | 优化后 20 MiB gate 110 ms；3 s 预算 |
| 成本交叉 | replay ≥ max(8 MiB, 2.0×plain_len) 且 增量耗时 > 1.5×快照总耗时 | 防小增量拉大快照 |
| 持闸预算 | `gate_hold_ms < 3 000`；估算超限不启动，实测超限中止回退 | ADR 0012 短持闸 |
| 失败 | 清 staging、冷却、无损回退普通聚合 pull | 不能卡死 |

`catchup_prepare` 不持闸：primary/fallback 选点并绑定实际 `S`/cursor；manifest +
snapshot 验签；`asset_bytes` 只补缺失；epoch/rotation 安全暂停。

`catchup_integrate` 持闸：flush → 分批 `baseline_apply`（每批 5 篇）→ `finish`
单调 CAS；`verify_imported` 放闸外或单独预算。

`prune_covered_inbox` 仅在 finalize 成功且持久化 `integrated_seq >= S` 后；保留
`skipped_asset` 证据，不删未覆盖行。

## 8. 测试/运行证据（摘要）

- storage **239** / sync **631** / app 默认 **740** / dev-sync **891** /
  no-default **575**；fmt/clippy 全绿。
- release `phase1a_release` 1 个 ignored 探针通过。
- 聚合：单文档 5000 条 `147.0 s → 275 ms`，`doc_loads 5000 → 20`；
  20 篇/5000 条 `14.9 s → 0.76 s`，`doc_loads 5000 → 400`。
- baseline 数据见 §3.2；闸外 verify 是最大残余。

## 9. 代码锚点（需要全文时）

| 文件 | 行 |
|---|---|
| `crates/sync/src/engine.rs` | 1837 / 2562 / 2669 |
| `crates/storage/src/baseline_apply.rs` | 231 / 334 / 964 |
| `crates/storage/src/body_doc.rs` | `import_oplog_only` / `pending_update_from_oplog` |
| `crates/storage/src/remote_known.rs` | 全文 |
| `crates/storage/src/actor.rs` | 468 |
| `crates/storage/src/sync_meta.rs` | 105 / 209 |
| `crates/storage/src/store.rs` | 98 / 416 / 471 |
| `crates/storage/src/sync_bridge.rs` | 1069 / 1202 / 1318 |

## 10. 结论建议格式

请回：

- **结论**：Phase 1A 是否可接受；6 个模块哪些必须先改。
- **必须修正**：文件/行号 + 为什么。
- **阈值**：25k / 16 MiB / 3 s / 2.0× / 1.5× 是否调整；旧库 tombstone 追溯方案。
- **新增风险**：并发、崩溃、coverage 证明、detached 边界。
- **不建议做**：明确后置项，避免扩大 Phase 1B。

`````



@@SECTION docs/06 `docs/notes/sync-phase1a-review-index.md`


`````markdown
# 同步 Phase 1A 评审索引（给高级模型）

> 当前评审对象：**Phase 0 度量 + Phase 1A（批量 inbox 原子化、按文档聚合 integrate、baseline 优化、Release 标定）**。
> 完整评审包（架构/代码地图/问题清单）见
> [`sync-review-packet-20261121.md`](sync-review-packet-20261121.md)。
> 只上传一份给高级模型时，用自包含的
> [`sync-critical-modules-review.md`](sync-critical-modules-review.md)。
> **Phase 1B（checkpoint 快速追赶）尚未写代码**；计划里已有约束，不能按“已实现”评审。
> 本轮没有改 relay / `sync-proto` / schema / wire；不变量仍是 ADR 0006 的游标与 checkpoint 契约。

## 1. 先读（按顺序）

0. **完整评审包** [`sync-review-packet-20261121.md`](sync-review-packet-20261121.md)（建议先读）。
1. **计划与 Phase 1B 约束**
   [`sync-fast-catchup-plan.md`](sync-fast-catchup-plan.md)
   - §4 Phase 0；§8 上一轮评审的 6 点修正；
   - §9 本轮新增：双模式触发、强制/机会性追赶、写闸拆分、`prune_covered_inbox`、
     `N_min` 的 Stage 1A 重标定。
2. **实测报告**
   [`sync-phase0-metrics.md`](sync-phase0-metrics.md)
   - §1–§7 是 Phase 0 dev 数据（历史对照，**不要用其绝对值**）；
   - **§8 是 Phase 1A 的 `--release` 数据**：storage 批量、600/2000/5000 追赶、
     20 篇多文档、`baseline_apply` 5/20 MiB 探针。
3. **契约**
   - [`docs/adr/0006-sync-v1.md`](../adr/0006-sync-v1.md)：checkpoint/manifest、`/bootstrap`、
     `integrated >= S`、pending/回滚检测、两 checkpoint GC。
   - [`docs/adr/0012-autosync-two-phase-gate.md`](../adr/0012-autosync-two-phase-gate.md)：
     网络不持闸 / integrate 持闸的现有骨架。
   - [`docs/adr/0026-relay-deployment-and-client-releases.md`](../adr/0026-relay-deployment-and-client-releases.md)：
     发布纪律；本轮不需要服务端更新。

## 2. 代码文件（建议给高级模型的顺序）

高级模型有 1M 上下文，可以一次给“2 份文档 + 下面 5 个核心文件”，先审正确性；需要再审
测试/Phase 1B 上下文时再补后半组。

### 第一批：Phase 1A 正确性（必看）

| 文件 | 重点 |
| --- | --- |
| `crates/storage/src/sync_meta.rs` | `meta` 键、`ConsumedPrefix`、received/integrated 连续前缀扫描；skipped 窗口 `ORDER BY key LIMIT 4096` 与跨窗口推进 |
| `crates/storage/src/actor.rs` | `Command::ExecInboxBatch` / `AdvanceConsumedPrefix`；worker 独占连接上的 **BEGIN IMMEDIATE** 单事务、幂等/冲突/前缀 |
| `crates/storage/src/sync_bridge.rs` | `store_inbox_verified_batch` 入口、`mark_integrated_batch`、公开 `advance_integrated_prefix`、单条路径兼容、remote_marks 透传 |
| `crates/storage/src/store.rs` | `append_updates_with_integrated_marks`：一条组合 update + 多条 inbox 标记同一 A 事务；remote-known VV 同事务写入 |
| `crates/storage/src/remote_known.rs` | 每篇文档远端已知 VV 的非破坏性 meta（tombstone 判定用） |
| `crates/sync/src/engine.rs` | pull 早退 flush；`load_head` 预检、按 doc 多轮合并（乱序自愈）、选批防饥饿、组合落盘、`doc_loads` |

### 第二批：测试与基准（审证据）

| 文件 | 重点 |
| --- | --- |
| `crates/sync/tests/engine.rs` | 单文档 256 行 `doc_loads=1`、deferred 截断、已覆盖+新行、重跑幂等、pull checkpoint 早退 |
| `crates/storage/tests/sync_inbox_batch.rs` | 全幂等/半批/篡改整批回滚、跨 ASSET 前缀、`mark_integrated_batch` 回滚 |
| `crates/sync/tests/phase0_catchup.rs` | per_row / batch / aggregate 三模式；600/2000 单文档、5000/20 篇 |
| `crates/sync/tests/phase1a_release.rs` | `baseline_apply` 5/20 MiB 探针；打印 restore/head/local delta/detached import/pending/事务/verify/gate |
| `crates/storage/tests/sync_inbox_bench.rs` | 逐条 vs 批量 inbox（release 数字） |

### 第三批：Phase 1B 上下文（计划评审用，非本轮实现）

| 文件 | 用途 |
| --- | --- |
| `crates/storage/src/baseline_apply.rs` | begin/apply/finish、CAS；已改为单次 snapshot 解码 + detached oplog 合并 + remote-known tombstone 判定 |
| `crates/storage/src/body_doc.rs` | `import_oplog_only` / `pending_update_from_oplog`（detached 合并，baseline 专用） |
| `crates/sync/src/bootstrap.rs` | 现有 `/bootstrap` + snapshot/asset 下载；要拆 prepare/integrate |
| `crates/sync/src/dev_local.rs` | 现有两阶段 `sync_prepare` / `sync_integrate`；写闸骨架 |
| `crates/sync/src/rotation.rs` | epoch/rotation 边界，强制追赶必须安全处理 |
| `crates/app/src/dev_sync/service.rs` | worker 摘要/指标展示；非正确性关键 |

## 3. Phase 1A 关键结果（release）

- storage inbox（4 KiB/行）：256 行 **282.4 → 5.34 ms（52.9×）**，
  1024 行 **819.4 → 25.16 ms（32.6×）**。
- 单文档 600：per_row 3513 ms；batch 5307 ms（fsync 抖动）；aggregate **83 ms**。
- 单文档 2000：per_row 31 144 ms；batch 25 903 ms；aggregate **244 ms**；
  `doc_loads` 2000 → 8。
- 5000/20 篇（每篇 ≈107 KiB）：per_row 19 039 ms；batch 15 157 ms；
  aggregate **1 169 ms**；`doc_loads` 5000 → 400。
- 5000 单文档：per_row 150 790 ms；aggregate **676 ms**；`doc_loads` 5000 → 20。
- `baseline_apply`（20 篇、storage merge、非真实下载）旧实现：5 MiB ≈2.72–4.14 s；
  20 MiB ≈44.6–61.3 s；5→20 超线性 14.8–16.9×。
- **优化后**（detached oplog merge，2026-11-21）：5 MiB **35 ms**、20 MiB **110 ms**，
  5→20 **3.1×**；根因是 Loro attached import 的 diff/state，不是 decode/pending。
  闸外 verify 仍 2.7 / 45.1 s（16.7×），不能把 gate 数字外推到首次打开/B 物化。
- **结论**：聚合消除 `O(历史 × 事件)`；批量 inbox 仍必要；`N_min=25 000` +
  16 MiB 机会性硬上限 + 3 s 持闸 + 2.0×/1.5× 成本交叉；强制追赶不受体积上限，
  失败 Paused + 退避，绝不回退 pull。最终阈值待用户确认。

## 4. 请高级模型重点判断

1. **单事务语义**：`ExecInboxBatch` 在 worker 独占连接上做“读既有 + 冲突判定 + 插入 +
   received 前缀”，是否真正消除了先读后写竞态？幂等行不触发 guard、冲突整批回滚是否
   覆盖所有半写路径？外部多进程写同一 DB 的 `SQLITE_BUSY` 是否需要额外处理？
2. **聚合落盘语义**：同 doc 多条 remote update 合成一条本地 update + 多条 inbox 标记，
   同一个 A 事务；这种“本地 seq 粒度变化”是否影响 ACK、物化、undo、审计或任何
   “remote update 1:1 本地 seq”的既有假设？`HeadConflict` 有界重试是否够？
3. **deferred 截断**：遇到 base 未覆盖即停止该 doc，之后行保持 verified；下一轮从同一行
   继续。这个“按 relay_seq 前缀”假设在 relay 物理乱序时是否可靠？`aggregate_body=false`
   兜底是否足够？
4. **前缀与覆盖证明**：批量标记不推进前缀、整批后统一推进；`prune_covered_inbox` 只能
   在 finalize 且 `integrated >= S` 后清。有没有“行已 integrated、前缀未推进、checkpoint
   coverage 误判”的窗口？
5. **阈值**：`N_min=25 000` + 16 MiB 机会性硬上限 + 3 s 持闸 + 2.0×/1.5× 成本交叉
   是否成立？强制/机会性双模式、`S - received_seq`、真实 snapshot 形状、闸外 verify
   仍 16× 的残余风险怎么处理？
6. **Phase 1B 写闸**：`catchup_prepare`（不持闸）→ `catchup_integrate`（持闸）能否复用
   ADR 0012 骨架？`bootstrap::run` 单体拆分、fallback 用实际 S、epoch/rotation 跨越暂停，
   还缺哪些测试/证据？
7. **证据缺口**：真实 relay/TLS、真实 snapshot 形状、下载/解密/验签、`gate_hold_ms`、
   附件、Windows/DPAPI、多进程并发；哪些必须在 Phase 1B 前补？

## 5. 已知边界（不要当遗漏）

- Phase 1B **未实现**：无 `CatchUpMode`、无 prepare/integrate 拆分、无真实 snapshot merge 实测。
- `baseline_apply` 探针只测 storage merge；`S`/cursor 是自造合法值，没有 checkpoint 签名/网络。
- 常规同步面板行未显示字节/耗时；指标在报告/`[sync]` 日志/worker 摘要。
- `store_inbox_verified` 单条路径仍是“读→插→前缀”分离事务（既有路径，未改）；批量路径已原子化。
- 聚合依赖同一 doc 内 relay_seq 与因果序一致；物理乱序历史只能走 `aggregate_body=false`。
- `N_min=25 000` / 16 MiB 是计划 §9.1 的默认值，仍待用户确认；128 MiB 只允许强制追赶且受本地磁盘预算保护，不能走机会性路径。

## 6. 评审产出建议

- **结论**：Phase 1A 是否可接受；Phase 1B 是否按“先 20k/成本交叉 + 双模式”落地。
- **必须修正**：按严重度 + 文件/行号。
- **阈值**：确认 `N_min=25 000`、16 MiB 机会性上限、3 s 持闸；128 MiB 只用于强制追赶。
- **新增风险**：并发、崩溃、epoch/rotation、coverage 证明。
- **不建议做**：明确后置项。

`````



@@SECTION code/017 `crates/sync/src/bootstrap.rs`


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



@@SECTION code/028 `crates/sync/src/engine.rs`


`````rust
//! 客户端同步引擎（ADR 0006 §5/§7/§16；S7-5）。
//!
//! 四个阶段，全部**幂等、可续传、失败不推进游标**：
//!
//! 1. [`SyncEngine::encrypt_pending`]：`pending_encryption` 意图 → 组 `UpdateV1` →
//!    MNE1 UPDATE 信封（固定切片/AAD/context_hash/key_epoch）→ `store_encrypted_parts` CAS；
//! 2. [`SyncEngine::push`]：`pending_send` → 逐片幂等 PUT → `mark_sent` → 批量
//!    `POST /updates`（≤256）→ 逐项 accepted/duplicate **durable ACK**（`mark_acked` CAS）；
//! 3. [`SyncEngine::pull`]：`GET /log?after=cursor` → 校验 `ObjectCommit` 签名/object_digest →
//!    AEAD 解密 → 解码 `UpdateV1` → `store_inbox_verified`（依赖未齐也 durable）→ 推进游标；
//! 4. [`SyncEngine::integrate`]：`take_inbox_ready` → 依赖（base_vv ⊆ 本地 VV）就绪 →
//!    Loro import → 断言本地 seq = `head_seq+1` → `mark_integrated`；随后
//!    [`SyncEngine::ack`] 上报 received/integrated 游标。
//!
//! **重试语义**：加密失败/进程崩溃后重试必须复用已持久化密文（storage 的
//! `store_encrypted_parts` 对同字节幂等、对不同字节冲突）。因此一旦加密成功，重试路径
//! 只走推送，不会重新生成 nonce。
//!
//! **不推进游标的失败**：任何事件校验/解密/签名失败直接返回错误，`pull` 不写游标；
//! 回滚/log_id 变化/epoch 不匹配返回 [`SyncError::Paused`]，调用方必须停止自动同步。

use crate::asset_sync;
use crate::device::{verify_signature, DeviceSigningKey};
use crate::error::SyncError;
use crate::keys::{derive_object_key, RootSecretV1};
use crate::relay::{
    PauseReason, RelayClient, RelayLogEvent, RelayLogKind, UpdateOutcome, UpdateStatus,
    UpdateSubmission,
};
use crate::storage_view;
use crate::{DeviceId, ObjectId, VaultId};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use hmac::{Hmac, Mac};
use loro::VersionVector;
use menghunote_storage::{
    advance_integrated_prefix, append_updates, append_updates_with_integrated_marks, asset_bytes,
    count_inbox_ready, count_unreferenced_assets, list_referenced_assets, load_body_doc, load_head,
    mark_acked, mark_integrated, mark_integrated_batch, mark_sent, mark_skipped_event_received,
    outbox_parts, outbox_rows, pending_encryption, repack_required, set_confirmed_highwater,
    set_sync_log_id, store_encrypted_parts, store_inbox_verified, store_inbox_verified_batch,
    sync_cursor_state, take_inbox_ready, SyncContext, SyncEncryptedPart, SyncInboxRow,
    SyncOutboxState, UpdateRecord, Vault, VerifiedInboxRow, STRUCTURE_DOC_ID,
};
use menghunote_sync_proto::{
    cap_plain, expected_part_plain_len, object_commit_signing_input, object_digest_input,
    part_count_for, Envelope, EnvelopeHeader, ObjectCommit, ObjectType, UpdateV1,
    MAX_ASSET_MIME_BYTES, MAX_ASSET_RAW_BYTES,
};
use sha2::{Digest, Sha256};
use std::time::Instant;

/// `meta` 键：本设备持久化的 relay 游标（bootstrap 或上一次 pull 写入）。
pub const RELAY_CURSOR_META: &str = "sync.relay_cursor";

/// durable skipped-asset 记录的 `meta` 键前缀（沿用现有 `meta` 表，不新增 schema）。
///
/// 完整键：`sync.skipped_asset.<log_id base64url>.<relay_seq 20 位十进制>`。
pub(crate) const SKIPPED_ASSET_META_PREFIX: &str = "sync.skipped_asset.";

/// skipped-asset 记录值格式版本。
///
/// v1 只绑定 relay 的声明（seq/object_id/epoch/cursor），不证明资产真实存在；
/// v2 追加本机**实际下载并完成密码学验证**得到的资产事实（real_sha256/byte_len/
/// media_type/object_digest）。
///
/// v3 追加**完整 ObjectCommit 字节**：checkpoint validate 重扫日志时必须逐字节
/// 绑定 `event/commit`（而不是只认同一个 object_id 的 ASSET 事件）。v1/v2 一律
/// 拒绝——旧开发 vault 的 durable verified 不得当作新链的证据（独立修订号，
/// 禁止把旧字节改号升级）。
const SKIPPED_ASSET_MAGIC: &[u8] = b"MN/skipped-asset/3\0";
/// 记录里的日志事件类型：1 = ASSET（只允许这一种，UPDATE 必须写 inbox）。
const SKIPPED_ASSET_KIND_ASSET: u8 = 1;
/// 记录 HMAC 的 HKDF info（与设备签名密钥做域分离，不直接复用 Ed25519 种子）。
const SKIPPED_ASSET_MAC_INFO: &[u8] = b"MN/v1/skipped-asset\0";
/// 记录里允许保存的 relay 游标最大字节数（relay 游标是给 `/log` 回显的 token）。
const SKIPPED_ASSET_MAX_CURSOR: usize = 4096;
/// 记录里允许保存的 ObjectCommit 编码最大字节数（当前编码 ≤ 160B，留余量）。
const SKIPPED_ASSET_MAX_COMMIT: usize = 512;

const MESSAGE_ID_LEN: usize = 16;
/// 单批 `POST /updates` 上限（ADR §5）。
const MAX_UPDATES_PER_BATCH: usize = 256;
/// 集成批次的对象上限（ADR 0012 S2b：每批最多 32 对象）。
pub const INTEGRATE_BATCH_OBJECTS: usize = 32;
/// 集成批次的明文负载软上限（约 1 MiB；单个超大对象仍允许单独成批）。
pub const INTEGRATE_BATCH_BYTES: usize = 1024 * 1024;
/// 集成批次的选取时间软预算（50ms；到点先停手，下一批继续）。
pub const INTEGRATE_BATCH_TIME: std::time::Duration = std::time::Duration::from_millis(50);
/// 一次 integrate 最多跑多少批（防止病态库把持闸窗口拖到不可控）。
pub const INTEGRATE_MAX_BATCHES: usize = 32;
/// `pull` 批量写 inbox 的单批明文上限（只防一页大 update 在内存里堆太多；不改变语义）。
const PULL_INBOX_BATCH_BYTES: usize = 8 * 1024 * 1024;
const NONCE_LEN: usize = 24;
const HEADER_LEN: u16 = 196;
const PROTOCOL_VERSION: u16 = 1;
const CIPHER_SUITE: u8 = 1;

/// 引擎配置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EngineConfig {
    /// 每批推送上限（ADR §5：`POST /updates` ≤256）。
    pub send_batch: usize,
    /// 每页日志上限。
    pub page_limit: u32,
    /// 每次 integrate 最多处理的行数。
    pub integrate_limit: usize,
    /// `pull` 是否把一页的 verified 行用**一个事务**批量写入 inbox（默认真）。
    ///
    /// Phase 0 的实测开关，Phase 1A-2 起默认开启：整页 256 条 UPDATE 只产生一次
    /// `sync_inbox` 提交（`received` 前缀与行同一事务推进，见
    /// [`menghunote_storage::store_inbox_verified_batch`]）。结果与逐条路径等价
    /// （重复字节幂等、冲突 fail-closed、游标只在整页验收成功后推进）；差别只是
    /// durability 粒度从"条"变成"页内 flush"，以及页内提前返回（例如遇到
    /// checkpoint/control 的 `Paused(UnsupportedEvent)`）时会先 flush 已验收行再返回。
    ///
    /// 保留 `false` 分支给基准对照（Phase 0 的逐条事务路径）。
    pub inbox_batch: bool,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            send_batch: 256,
            page_limit: 256,
            integrate_limit: 256,
            inbox_batch: true,
        }
    }
}

/// 设备身份（引擎运行所需的最小集合）。
#[derive(Clone)]
pub struct EngineIdentity {
    /// 本机设备 ID。
    pub device_id: DeviceId,
    /// 设备签名私钥。
    pub signing_key: DeviceSigningKey,
    /// Loro peer id（本地 seq 分配用的 peer，永不复用 sender_seq）。
    pub peer_id: u64,
}

impl core::fmt::Debug for EngineIdentity {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("EngineIdentity")
            .field("device_id", &crate::ids::hex_of(&self.device_id))
            .field("signing_key", &"<redacted>")
            .field("peer_id", &self.peer_id)
            .finish()
    }
}

/// 推送报告。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PushReport {
    /// 逐片上传成功的对象数。
    pub uploaded: usize,
    /// durable ACK 的对象数。
    pub acked: usize,
    /// relay 逐项失败（保留 outbox 待重试）。
    pub failed: usize,
}

/// 增量资产上传报告（普通同步的"正文之后"阶段）。
///
/// 语义与顺序：
/// - 只处理 **`asset_refs` 引用得到**的本地资产（有明文）；历史孤儿跳过并计入
///   [`AssetSyncReport::orphans_skipped`]，不把整库历史资产重新上传。
/// - 已有 durable "已上传" 标记的资产直接跳过（不读字节、不发请求）。
/// - relay 上对象已完整（例如 checkpoint/轮换/另一台设备传过）时按确定性密文逐片
///   校验后只做提交/落标记，不重传分片；分片缺失时只补缺失片（已 durable 的片不重传）。
/// - 对象被其它 reservation 持有时整条资产**延后**（`deferred`），不阻塞正文/结构同步，
///   下一轮再试；relay/对象校验失败计入 `failed`，同样不阻塞本轮。
/// - **本地库损坏**（[`SyncError::LocalCorrupt`]，例如字节长度/哈希与元数据不一致）不是
///   可重试的网络失败：继续扫完其余健康资产后，把首个损坏原样上抛，见
///   [`SyncEngine::upload_pending_assets`]。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AssetSyncReport {
    /// 被正文引用、有明文的资产数（本轮扫描口径）。
    pub referenced: usize,
    /// 本轮成功上传并完成对象提交的资产数（含补齐缺失分片）。
    pub uploaded: usize,
    /// 本轮真正 PUT 的分片数（已存在/已 durable 的片不计）。
    pub parts_uploaded: usize,
    /// 本轮 PUT 的明文字节数。
    pub bytes_uploaded: u64,
    /// 命中本地 durable 标记、直接跳过的资产数。
    pub already_uploaded: usize,
    /// relay 上对象已完整（checkpoint/轮换/其他设备传过），本轮只校验+提交/落标记的数量。
    pub reused_existing: usize,
    /// 被其它 reservation 持有、需下一轮再试的资产数。
    pub deferred: usize,
    /// 单资产 relay/对象校验失败数（不阻塞正文同步，下一轮重试）。
    ///
    /// **本地库损坏不计入这里**：`LocalCorrupt` 每轮重试都不会自愈，会掩盖真正的
    /// "需要本地修复"状态，因此整轮上抛而不是伪装成可重试失败。
    pub failed: usize,
    /// 元数据不可用或空数据而跳过的资产数（不上传空资产）。
    pub skipped_invalid: usize,
    /// 有明文但没有任何正文引用的孤儿数（**不上传**；GC 不在本任务范围）。
    pub orphans_skipped: u64,
}

/// 单条资产的增量上传结果（内部用）。
enum AssetUploadOutcome {
    /// 命中 durable 标记，跳过。
    AlreadyUploaded,
    /// 本轮上传（含补齐缺失分片）并完成提交。
    Uploaded {
        parts_uploaded: usize,
        bytes_uploaded: u64,
    },
    /// relay 上对象已完整，只校验/提交/落标记。
    ReusedExisting {
        parts_uploaded: usize,
        bytes_uploaded: u64,
    },
    /// 被其它 reservation 持有，下一轮再试。
    Deferred,
    /// 元数据/明文不可用，跳空资产。
    SkippedInvalid,
}

/// 拉取报告。
///
/// 除事件计数外还带**只读性能指标**（Phase 0：先量清楚瓶颈，再决定要不要批量/快照）。
/// 所有字节/耗时字段都不含路径、密钥或明文内容，可以直接打日志。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PullReport {
    /// 本页事件数。
    pub events: usize,
    /// 成功验收并 durable 写入 inbox 的事件数。
    pub stored: usize,
    /// 已收过（`relay_seq <= received_seq`）而跳过的事件数。
    pub skipped: usize,
    /// 新游标（已持久化）。
    pub next_cursor: String,
    /// 本次成功验收的 UPDATE **明文负载**字节合计。
    ///
    /// 口径：`UpdateV1` 载荷的明文长度之和（= durable 写进 `sync_inbox.verified_payload`、
    /// 后续 integrate 要 import 的字节）。**不含**每片 196B 信封头与 16B AEAD tag，
    /// 因此它是网络拉取字节的下界；要估算 wire 体积需再加"分片数 × 212B"。
    pub update_bytes: u64,
    /// 本次 pull 实际下载 + 验证 + 落本地内容寻址表的 ASSET **明文字节**合计。
    ///
    /// 含幂等重放时对已消费 ASSET 的重新下载/校验（那是真实发生的网络与磁盘成本）。
    pub asset_bytes: u64,
    /// 本次 pull 的墙钟耗时（`Duration::as_millis` 截断成 `u64`；<1ms 记 0）。
    pub elapsed_ms: u64,
    /// 本次 pull 请求/消费的日志页数。
    ///
    /// 当前实现每次调用只拉一页，成功返回的报告恒为 `1`（空页也算 1）；字段保留给
    /// Phase 1 的多页/快照路径复用。
    pub pages: usize,
    /// 本页 verified 行是否用**一个事务**批量写入 inbox
    /// （[`EngineConfig::inbox_batch`]；默认真，`false` = 基准对照的逐条路径）。
    pub inbox_batch: bool,
}

/// 集成（Integrate）阶段的批次预算与上限（ADR 0012 S2b）。
///
/// 语义：
/// - 结构文档优先，正文按 **doc 轮转** 公平取批（不循环刷最早一批）；
/// - 每批最多 [`IntegrateConfig::batch_objects`] 个对象 / 约
///   [`IntegrateConfig::batch_bytes`] 字节 / [`IntegrateConfig::batch_time`] 的**软**预算；
/// - 到事务边界交付，**不跨批提前 ACK**：逐行路径是每个对象自己的 A 事务 +
///   `mark_integrated`；正文聚合路径是每篇文档一个 A 事务（组合 update + 批量标记），
///   整批正文结束后统一推进一次 integrated 前缀；
/// - 有进展才继续下一批；连续无进展就停，并把剩余 deferred 如实上报。
///
/// [`IntegrateConfig::aggregate_body`] 打开时正文的 `batch_objects` 语义不同：它限制
/// 的是**每批参与的文档数**（而不是行数），同一文档的所有待处理行尽量进同一批，
/// 只受 `batch_bytes` / `batch_time` 与扫描窗口约束。这样一页 256 条同一文档的 update
/// 只需一次 `load_body_doc`（旧路径是 32 行一批 = 8 次全量重放）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IntegrateConfig {
    /// 单批对象上限（1 个超大对象仍允许单独成批）。
    ///
    /// `aggregate_body = true` 时正文阶段把它解释成"单批最多多少篇文档"（结构阶段不变）。
    pub batch_objects: usize,
    /// 单批明文负载软上限（字节）。
    pub batch_bytes: usize,
    /// 单批选取时间软预算。
    pub batch_time: std::time::Duration,
    /// 一次 integrate 的批数上限（结构批 + 正文批）。
    pub max_batches: usize,
    /// 正文阶段是否按文档聚合（默认真）。
    ///
    /// - `true`：同一文档的待处理行按 `relay_seq` 升序在一批内处理；一次
    ///   `load_body_doc` + 内存逐条 import + **一条**组合 update 落盘，inbox 标记与
    ///   update 在同一 A 事务里（见 [`Self::batch_objects`] 的语义差异）；
    /// - `false`：Phase 0 的逐行路径（每条 update 一次全量重放 + 一次 A 事务），
    ///   保留给基准对照；聚合路径的多轮合并已能自愈同文档物理乱序。
    pub aggregate_body: bool,
}

impl Default for IntegrateConfig {
    fn default() -> Self {
        Self {
            batch_objects: INTEGRATE_BATCH_OBJECTS,
            batch_bytes: INTEGRATE_BATCH_BYTES,
            batch_time: INTEGRATE_BATCH_TIME,
            max_batches: INTEGRATE_MAX_BATCHES,
            aggregate_body: true,
        }
    }
}

impl IntegrateConfig {
    /// 按"本次最多集成多少个对象"折算批次（旧 `integrate(limit)` 入口的兼容语义）。
    ///
    /// `limit` 是**本次调用的对象预算**，不是单批上限：批次大小仍受 32 对象/1MiB/50ms
    /// 约束，批数按需折算（至少一批）。
    ///
    /// 注意：`aggregate_body = true`（默认）时正文按文档聚合，`batch_objects` 被解释成
    /// "每批参与的文档数"，因此一次调用可能处理**超过 `limit` 个正文对象**（同一文档
    /// 一页 256 行会一次集成完）。结构阶段仍严格按对象预算；需要严格逐对象预算的基准
    /// 对照请设 `aggregate_body = false`。
    #[must_use]
    pub fn with_object_budget(limit: usize) -> Self {
        let batch_objects = INTEGRATE_BATCH_OBJECTS.min(limit.max(1));
        let max_batches = limit
            .saturating_add(batch_objects.saturating_sub(1))
            .checked_div(batch_objects)
            .unwrap_or(usize::MAX)
            .max(1);
        Self {
            batch_objects,
            max_batches,
            ..Self::default()
        }
    }
}

/// 集成（Integrate）阶段的可见事实（ADR 0012 S2b）。
///
/// `deferred` 是**本轮结束时仍 durable verified、且落在已接收连续前缀内**的对象数：
/// 它们没有被丢弃，依赖就绪/下一次同步会继续。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IntegrateReport {
    /// 已 import 并推进本地 seq 的对象数（含"已应用过、只标记"的幂等对象）。
    pub integrated: usize,
    /// 依赖未就绪/未知文档，保持 durable verified 的对象数。
    pub deferred: usize,
    /// 其中结构文档的对象数。
    pub structure_integrated: usize,
    /// 其中正文文档的对象数。
    pub body_integrated: usize,
    /// 本轮实际跑了多少批（结构批 + 正文批）。
    pub batches: usize,
    /// 结构 head 真的前进了、并且**只做投影**重建了 body 行 / notes 视图。
    pub projected: bool,
    /// 整个 integrate 阶段的墙钟耗时（`Duration::as_millis` 截断成 `u64`）。
    ///
    /// 包含结构阶段、只投影、正文轮转与 `deferred` 计数；不含网络（pull 已单独计时）。
    pub elapsed_ms: u64,
    /// 本阶段调用 `load_body_doc`（全量重放历史）的次数。
    ///
    /// 聚合正文（[`IntegrateConfig::aggregate_body`]）下同一文档一批只算一次，且选批
    /// 预检判定"全部已覆盖/纯 deferred"的组为 0 次（只读 O(1) 的 head_vv）；结构阶段
    /// 仍是逐行路径（每行一次全量重放），因此本字段是"整个 integrate 的全量重放次数"，
    /// 供 Phase 1A 的 release 基准对比旧逐行路径（256 行 = 256 次）。
    pub doc_loads: usize,
}

/// 依赖有序集成的完整结果（`changed_heads` 供 UI 判断要不要交付/刷新）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IntegrateOutcome {
    /// 集成计数（含 deferred / 批次 / 是否投影）。
    pub report: IntegrateReport,
    /// 本轮 head **真的前进**的文档（结构 + 正文，按首次出现稳定序；去重）。
    pub changed_heads: Vec<String>,
}

/// ACK 报告。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AckReport {
    /// 已上传的本地上报 received 游标。
    pub received_seq: u64,
    /// 已上报 integrated 游标。
    pub integrated_seq: u64,
}

/// 一次完整同步报告。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncReport {
    /// 本轮新加密的对象数。
    pub encrypted: usize,
    /// 推送结果。
    pub push: PushReport,
    /// 增量资产上传结果（正文推送之后）。
    pub assets: AssetSyncReport,
    /// 拉取结果。
    pub pull: PullReport,
    /// 集成结果。
    pub integrate: IntegrateReport,
    /// ACK 结果。
    pub ack: AckReport,
}

/// durable skipped-asset 记录（评审 0003 P0-B 残余）。
///
/// `SyncEngine::pull` 消费 ASSET 事件时**不写** `sync_inbox` 行；为了让 checkpoint 的
/// `ensure_coverage_proven` 不再靠"无 inbox 行 + 在游标之前"推断该序号是 ASSET，
/// pull 在推进游标之前把"本机确实消费了这条 ASSET 事件"落成 durable 事实。
///
/// 记录值受设备签名密钥派生的 HMAC 保护：seq/object_id/log_id/kind/epoch/cursor 任一
/// 字段被改写（哪怕是本地 SQLite 里的行）都会校验失败，checkpoint 一律 fail-closed。
///
/// v3 还绑定 [`VerifiedAsset`] 的四个字段与**完整 ObjectCommit 字节**：前者来自本机
/// 真实下载全部 parts 并完成 AEAD/摘要/资产头/OID 校验的结果，不是 relay 的声明；
/// 后者让 checkpoint 重扫日志时能逐字节核对"这次看到的就是验证时那条声明"。HMAC
/// 因此证明的是"本机验证过这个真实资产与这条完整声明"。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SkippedAssetRecord {
    /// 该事件所属的 relay 日志。
    pub(crate) log_id: [u8; 16],
    /// relay 日志序号。
    pub(crate) relay_seq: u64,
    /// 日志事件里的 ASSET object_id。
    pub(crate) object_id: ObjectId,
    /// 记录类型（只允许 [`SKIPPED_ASSET_KIND_ASSET`]）。
    pub(crate) kind: u8,
    /// 事件所属 key epoch。
    pub(crate) epoch: u64,
    /// 资产数据的真实 SHA256（从已解密明文头读出并校验过）。
    pub(crate) real_sha256: [u8; 32],
    /// 资产数据的真实字节数（与明文头、密文总长一致）。
    pub(crate) byte_len: u64,
    /// 资产 MIME（canonical 头的一部分，参与 object_id 重算）。
    pub(crate) media_type: String,
    /// 已由实际 parts 重算并核对的密文对象摘要。
    pub(crate) object_digest: [u8; 32],
    /// 验证时那条日志事件的**完整** `ObjectCommit` 编码（v3 起必填）。
    pub(crate) commit: Vec<u8>,
    /// 消费该事件时的 pull 游标（`page.next_cursor`，原样保存供审计）。
    pub(crate) cursor: String,
}

/// 记录 HMAC 的密钥：`HKDF(设备签名种子, salt=vault_id, info=MN/v1/skipped-asset\0)`。
///
/// 用设备签名密钥而不是 epoch 根，是为了让跨 epoch 轮换的旧记录仍能被本机验证
/// （根每个 epoch 都会重新随机）。HKDF 做了域分离，不直接把 Ed25519 种子当 HMAC 密钥。
fn skipped_asset_mac_key(
    signing_key: &DeviceSigningKey,
    vault_id: &VaultId,
) -> Result<crate::keys::Secret32, SyncError> {
    crate::kdf::hkdf32(
        signing_key.seed().expose_slice(),
        vault_id,
        SKIPPED_ASSET_MAC_INFO,
    )
}

fn skipped_asset_mac(
    material: &[u8],
    mac_key: &crate::keys::Secret32,
) -> Result<[u8; 32], SyncError> {
    type HmacSha256 = Hmac<Sha256>;
    let mut mac = <HmacSha256 as Mac>::new_from_slice(mac_key.expose_slice())
        .map_err(|_| SyncError::LocalCorrupt("skipped-asset HMAC 密钥长度非法"))?;
    mac.update(material);
    Ok(mac.finalize().into_bytes().into())
}

impl SkippedAssetRecord {
    fn encode(&self, mac_key: &crate::keys::Secret32) -> Result<Vec<u8>, SyncError> {
        if self.relay_seq == 0 {
            return Err(SyncError::LocalCorrupt("skipped-asset relay_seq 必须 >0"));
        }
        if self.kind != SKIPPED_ASSET_KIND_ASSET {
            return Err(SyncError::LocalCorrupt("skipped-asset kind 必须是 ASSET"));
        }
        if self.epoch == 0 {
            return Err(SyncError::LocalCorrupt("skipped-asset epoch 必须 >0"));
        }
        if self.cursor.is_empty() || self.cursor.len() > SKIPPED_ASSET_MAX_CURSOR {
            return Err(SyncError::LocalCorrupt("skipped-asset 游标长度非法"));
        }
        if self.media_type.is_empty() || self.media_type.len() > MAX_ASSET_MIME_BYTES {
            return Err(SyncError::LocalCorrupt(
                "skipped-asset MIME 长度非法（必须 1..=255）",
            ));
        }
        if self.commit.is_empty() || self.commit.len() > SKIPPED_ASSET_MAX_COMMIT {
            return Err(SyncError::LocalCorrupt("skipped-asset commit 长度非法"));
        }
        let cursor_len = u32::try_from(self.cursor.len())
            .map_err(|_| SyncError::LocalCorrupt("skipped-asset 游标超长"))?;
        let commit_len = u32::try_from(self.commit.len())
            .map_err(|_| SyncError::LocalCorrupt("skipped-asset commit 超长"))?;
        let mut out = Vec::with_capacity(
            SKIPPED_ASSET_MAGIC.len()
                + 16
                + 8
                + 32
                + 1
                + 8
                + 32
                + 8
                + 2
                + self.media_type.len()
                + 32
                + 4
                + self.commit.len()
                + 4
                + self.cursor.len()
                + 32,
        );
        out.extend_from_slice(SKIPPED_ASSET_MAGIC);
        out.extend_from_slice(&self.log_id);
        out.extend_from_slice(&self.relay_seq.to_be_bytes());
        out.extend_from_slice(&self.object_id);
        out.push(self.kind);
        out.extend_from_slice(&self.epoch.to_be_bytes());
        out.extend_from_slice(&self.real_sha256);
        out.extend_from_slice(&self.byte_len.to_be_bytes());
        out.extend_from_slice(&(self.media_type.len() as u16).to_be_bytes());
        out.extend_from_slice(self.media_type.as_bytes());
        out.extend_from_slice(&self.object_digest);
        out.extend_from_slice(&commit_len.to_be_bytes());
        out.extend_from_slice(&self.commit);
        out.extend_from_slice(&cursor_len.to_be_bytes());
        out.extend_from_slice(self.cursor.as_bytes());
        let tag = skipped_asset_mac(&out, mac_key)?;
        out.extend_from_slice(&tag);
        Ok(out)
    }

    fn decode(bytes: &[u8], mac_key: &crate::keys::Secret32) -> Result<Self, SyncError> {
        // 最小长度：magic + 定长字段 + MIME ≥1B + commit ≥1B + cursor ≥1B + tag。
        let min_len = SKIPPED_ASSET_MAGIC.len()
            + 16
            + 8
            + 32
            + 1
            + 8
            + 32
            + 8
            + 2
            + 1
            + 32
            + 4
            + 1
            + 4
            + 1
            + 32;
        if bytes.len() < min_len {
            return Err(SyncError::LocalCorrupt("skipped-asset 记录过短"));
        }
        let (body, tag) = bytes.split_at(bytes.len() - 32);
        let expected = skipped_asset_mac(body, mac_key)?;
        if tag != expected {
            return Err(SyncError::LocalCorrupt(
                "skipped-asset 记录认证失败（被篡改）",
            ));
        }
        if !body.starts_with(SKIPPED_ASSET_MAGIC) {
            return Err(SyncError::LocalCorrupt("skipped-asset 记录版本非法"));
        }
        let mut input = &body[SKIPPED_ASSET_MAGIC.len()..];
        let log_id = skipped_take_array::<16>(&mut input)?;
        let relay_seq = skipped_take_u64(&mut input)?;
        let object_id = skipped_take_array::<32>(&mut input)?;
        let kind = skipped_take(&mut input, 1)?[0];
        let epoch = skipped_take_u64(&mut input)?;
        let real_sha256 = skipped_take_array::<32>(&mut input)?;
        let byte_len = skipped_take_u64(&mut input)?;
        let mime_len = usize::from(skipped_take_u16(&mut input)?);
        if !(1..=MAX_ASSET_MIME_BYTES).contains(&mime_len) {
            return Err(SyncError::LocalCorrupt(
                "skipped-asset MIME 长度非法（必须 1..=255）",
            ));
        }
        let media_type = String::from_utf8(skipped_take(&mut input, mime_len)?.to_vec())
            .map_err(|_| SyncError::LocalCorrupt("skipped-asset MIME 不是 UTF-8"))?;
        let object_digest = skipped_take_array::<32>(&mut input)?;
        let commit_len = skipped_take_u32(&mut input)? as usize;
        if commit_len == 0 || commit_len > SKIPPED_ASSET_MAX_COMMIT {
            return Err(SyncError::LocalCorrupt("skipped-asset commit 长度非法"));
        }
        let commit = skipped_take(&mut input, commit_len)?.to_vec();
        let cursor_len = skipped_take_u32(&mut input)? as usize;
        if cursor_len == 0 || cursor_len > SKIPPED_ASSET_MAX_CURSOR {
            return Err(SyncError::LocalCorrupt("skipped-asset 游标长度非法"));
        }
        let cursor = String::from_utf8(skipped_take(&mut input, cursor_len)?.to_vec())
            .map_err(|_| SyncError::LocalCorrupt("skipped-asset 游标不是 UTF-8"))?;
        if !input.is_empty() {
            return Err(SyncError::LocalCorrupt("skipped-asset 记录尾部有多余字节"));
        }
        if relay_seq == 0 || epoch == 0 || kind != SKIPPED_ASSET_KIND_ASSET {
            return Err(SyncError::LocalCorrupt("skipped-asset 记录字段非法"));
        }
        // 完整 commit 必须与记录里的身份字段自洽（防止 HMAC 记录本身字段错配）。
        let commit_decoded = ObjectCommit::decode(&commit)
            .map_err(|_| SyncError::LocalCorrupt("skipped-asset commit 无法解码"))?;
        if commit_decoded.object_id != object_id
            || commit_decoded.object_type != ObjectType::Asset
            || commit_decoded.key_epoch != epoch
            || commit_decoded.generation != 0
            || commit_decoded.sender_seq != 0
            || commit_decoded.total_len == 0
            || commit_decoded.total_len > MAX_ASSET_RAW_BYTES as u64
        {
            return Err(SyncError::LocalCorrupt(
                "skipped-asset commit 与记录声明不一致",
            ));
        }
        Ok(Self {
            log_id,
            relay_seq,
            object_id,
            kind,
            epoch,
            real_sha256,
            byte_len,
            media_type,
            object_digest,
            commit,
            cursor,
        })
    }
}

fn skipped_take<'a>(input: &mut &'a [u8], len: usize) -> Result<&'a [u8], SyncError> {
    if input.len() < len {
        return Err(SyncError::LocalCorrupt("skipped-asset 记录长度不足"));
    }
    let (head, tail) = input.split_at(len);
    *input = tail;
    Ok(head)
}

fn skipped_take_array<const N: usize>(input: &mut &[u8]) -> Result<[u8; N], SyncError> {
    let bytes = skipped_take(input, N)?;
    bytes
        .try_into()
        .map_err(|_| SyncError::LocalCorrupt("skipped-asset 字段长度非法"))
}

fn skipped_take_u16(input: &mut &[u8]) -> Result<u16, SyncError> {
    let bytes: [u8; 2] = skipped_take(input, 2)?
        .try_into()
        .map_err(|_| SyncError::LocalCorrupt("skipped-asset 字段长度非法"))?;
    Ok(u16::from_be_bytes(bytes))
}

fn skipped_take_u32(input: &mut &[u8]) -> Result<u32, SyncError> {
    let bytes: [u8; 4] = skipped_take(input, 4)?
        .try_into()
        .map_err(|_| SyncError::LocalCorrupt("skipped-asset 字段长度非法"))?;
    Ok(u32::from_be_bytes(bytes))
}

fn skipped_take_u64(input: &mut &[u8]) -> Result<u64, SyncError> {
    let bytes: [u8; 8] = skipped_take(input, 8)?
        .try_into()
        .map_err(|_| SyncError::LocalCorrupt("skipped-asset 字段长度非法"))?;
    Ok(u64::from_be_bytes(bytes))
}

/// skipped-asset 记录的 `meta` 键（seq 固定 20 位十进制，便于稳定排序/诊断）。
pub(crate) fn skipped_asset_meta_key(log_id: &[u8; 16], relay_seq: u64) -> String {
    format!(
        "{SKIPPED_ASSET_META_PREFIX}{}.{relay_seq:020}",
        crate::base64url::encode(log_id)
    )
}

/// 读取并**验证**本机为某个 `log_id` 保存的全部 skipped-asset 记录。
///
/// 任何一条记录结构/HMAC/键序号不自洽都返回错误（fail-closed）：checkpoint 不能
/// 把被改写的记录当成覆盖证据。
pub(crate) fn load_skipped_asset_records(
    vault: &Vault,
    signing_key: &DeviceSigningKey,
    vault_id: &VaultId,
    log_id: &[u8; 16],
) -> Result<Vec<SkippedAssetRecord>, SyncError> {
    let prefix = format!(
        "{SKIPPED_ASSET_META_PREFIX}{}.",
        crate::base64url::encode(log_id)
    );
    let prefix_len = i64::try_from(prefix.len())
        .map_err(|_| SyncError::LocalCorrupt("skipped-asset 前缀超长"))?;
    let rows = vault.query(
        "SELECT key,value FROM meta WHERE substr(key,1,?1)=?2 ORDER BY key",
        &[
            menghunote_storage::Value::Integer(prefix_len),
            menghunote_storage::Value::Text(prefix.clone()),
        ],
    )?;
    let mac_key = skipped_asset_mac_key(signing_key, vault_id)?;
    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        let key = match row.first() {
            Some(menghunote_storage::Value::Text(key)) => key,
            _ => return Err(SyncError::LocalCorrupt("skipped-asset 键类型非法")),
        };
        let bytes = match row.get(1) {
            Some(menghunote_storage::Value::Blob(bytes)) => bytes,
            _ => return Err(SyncError::LocalCorrupt("skipped-asset 值类型非法")),
        };
        let seq_text = key
            .strip_prefix(&prefix)
            .ok_or(SyncError::LocalCorrupt("skipped-asset 键前缀不符"))?;
        if seq_text.len() != 20 || !seq_text.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(SyncError::LocalCorrupt("skipped-asset 键序号非法"));
        }
        let seq: u64 = seq_text
            .parse()
            .map_err(|_| SyncError::LocalCorrupt("skipped-asset 键序号非法"))?;
        let record = SkippedAssetRecord::decode(bytes, &mac_key)?;
        if record.log_id != *log_id || record.relay_seq != seq {
            return Err(SyncError::LocalCorrupt(
                "skipped-asset 键与记录内容不一致（被篡改）",
            ));
        }
        out.push(record);
    }
    Ok(out)
}

/// 同步引擎。
pub struct SyncEngine<'a, C: RelayClient> {
    vault: &'a Vault,
    client: &'a mut C,
    vault_id: VaultId,
    identity: EngineIdentity,
    root: RootSecretV1,
    config: EngineConfig,
    /// 轮换阶段 A–C 冻结：禁止 push/encrypt，但 pull/integrate 仍可继续。
    rotation_frozen: bool,
}

impl<'a, C: RelayClient> core::fmt::Debug for SyncEngine<'a, C> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SyncEngine")
            .field("vault_id", &crate::ids::hex_of(&self.vault_id))
            .field("identity", &self.identity)
            .field("key_epoch", &self.root.key_epoch)
            .field("config", &self.config)
            .finish()
    }
}

/// 从持久事实重建轮换冻结屏障（不依赖 RAM）。
///
/// 满足任意一条即保持冻结：
/// - `sync.repack_required=true`（旧 epoch outbox 尚未重包）；
/// - `sync.rotation_apply_phase` 存在且不是 `applied`（`applying`/`paused`/`conflict`/
///   `needs_restart`/未知值都属于"apply 未完成"，包括 merge 后、promotion 前崩溃）；
/// - rotation journal 存在且 phase 不是明确的终态 `Completed`/`Promoted`
///   （`Frozen`…`Activated`、`Aborted`、restart 中间态都属于"激活中"）。
///
/// 读取失败一律 **fail closed**：无法证明轮换已完成时，宁可继续冻结旧 epoch 的
/// encrypt/push，也不能把旧 epoch 密文推给 relay。
fn durable_rotation_barrier(vault: &Vault) -> bool {
    match repack_required(vault) {
        Ok(true) => return true,
        Ok(false) => {}
        Err(_) => return true,
    }
    if crate::rotation_journal::apply_phase_barrier(vault) {
        return true;
    }
    match crate::rotation_journal::RotationJournal::load(vault) {
        Ok(None) => false,
        Ok(Some(journal)) => !matches!(
            journal.phase,
            crate::rotation_journal::RotationPhase::Completed
                | crate::rotation_journal::RotationPhase::Promoted
        ),
        Err(_) => true,
    }
}

impl<'a, C: RelayClient> SyncEngine<'a, C> {
    /// 默认配置构造。
    pub fn new(
        vault: &'a Vault,
        client: &'a mut C,
        identity: EngineIdentity,
        root: RootSecretV1,
    ) -> Self {
        Self::with_config(vault, client, identity, root, EngineConfig::default())
    }

    /// 指定配置构造。
    pub fn with_config(
        vault: &'a Vault,
        client: &'a mut C,
        identity: EngineIdentity,
        root: RootSecretV1,
        config: EngineConfig,
    ) -> Self {
        Self {
            vault,
            client,
            vault_id: root.vault_id,
            identity,
            root,
            config,
            // 冻结屏障必须从持久事实重建：进程重启后不能因为 RAM 归零就解冻。
            rotation_frozen: durable_rotation_barrier(vault),
        }
    }

    /// 轮换阶段 A：停止后续 push/encrypt（本地 A 事务编辑仍可继续）。
    ///
    /// 这里只置 RAM 标志；阶段 A 的持久事实由 rotation journal /
    /// `sync.repack_required` 落盘，重启后由 [`durable_rotation_barrier`] 重建。
    pub fn freeze_rotation(&mut self) {
        self.rotation_frozen = true;
    }

    /// 轮换 abort/restart 后按持久事实重新解冻。
    ///
    /// 只有 journal 明确 `Completed`/`Promoted` 且 `repack_required=false` 时才真正
    /// 解冻；否则保持冻结，避免用一次 RAM 调用绕过 durable barrier。
    pub fn thaw_rotation(&mut self) {
        self.rotation_frozen = durable_rotation_barrier(self.vault);
    }

    /// 当前是否处于轮换冻结（push/encrypt 被拒绝）。
    ///
    /// 该值在构造期由 [`durable_rotation_barrier`] 从持久事实重建，`thaw_rotation`
    /// 也只能按持久事实重新计算，不能用一次 RAM 调用绕过。
    #[must_use]
    pub fn is_rotation_frozen(&self) -> bool {
        self.rotation_frozen
    }

    /// same-epoch 启动收尾：消费 durable typed apply receipt（评审 0003 P1-C/P1-D）。
    ///
    /// 崩溃窗口：身份 rename 已提交、`sync.rotation_apply_phase='applied'` 尚未写入。
    /// 此时 relay 与本地身份已是同一 epoch，[`Self::apply_epoch_change`] 的 epoch
    /// 检测不会触发；如果没人处理 `applying`，engine 会永久冻结。
    ///
    /// 本方法只接受**可认证**的事实：receipt + 内存/磁盘身份 + authenticated pin +
    /// completed baseline journal + 重包屏障全部一致才写 `applied`；否则返回 `Paused`
    /// 并保持冻结（fail-closed）。收尾**不会**清 `sync.repack_required`。
    ///
    /// 返回 `None` 表示当前 phase 不是 `applying`（没有需要收尾的 apply）。
    pub fn reconcile_rotation_apply(
        &mut self,
        identity_path: &std::path::Path,
        identity: &crate::identity::LocalIdentityV1,
    ) -> Result<Option<crate::rotation_apply::ApplyOutcome>, SyncError> {
        let outcome =
            crate::rotation_apply::reconcile_same_epoch_apply(self.vault, identity_path, identity)?;
        if matches!(
            outcome,
            Some(crate::rotation_apply::ApplyOutcome::Applied { .. })
        ) {
            // receipt 已认证身份 epoch/pin；换根后按 durable 事实重算屏障
            // （重包未完成时仍然冻结）。
            self.set_root(RootSecretV1::new(
                self.vault_id,
                identity.current_epoch,
                identity.current_root.clone(),
            ))?;
            self.thaw_rotation();
        }
        Ok(outcome)
    }

    /// B 侧应用 epoch 变化：进入时冻结 push，只有 Applied 才换根并解冻。
    ///
    /// activation T 由 `C: RelayClient` 的 [`RelayClient::verified_activation_t`] 提供
    /// （`HttpRelayClient` 委托 `/control` 缓存复算双签；未覆盖的客户端拿默认 `None`
    /// → 一律 `Paused`），因此本方法不需要为 T 改签名就能走真实授权路径。
    ///
    /// **阶段 I 的持久屏障由 apply 入口自己落盘**（[`crate::rotation_apply::
    /// apply_epoch_change_with_client`] 在第一次写库/merge 前写 `sync.rotation_apply_phase`
    /// = `applying`，成功后转 `applied`，失败保持 `paused`/`conflict`）。本方法只负责
    /// RAM 侧的进入冻结与成功换根；**不再**用 `staging` 之类的前置值代替持久 phase。
    #[allow(clippy::too_many_arguments)]
    pub fn apply_epoch_change(
        &mut self,
        vault: &menghunote_storage::Vault,
        identity_path: &std::path::Path,
        identity: &crate::identity::LocalIdentityV1,
        checkpoint_id: [u8; 16],
        baseline_s: u64,
        baseline_cursor: &str,
        candidate_root: &RootSecretV1,
        manager_public: [u8; 32],
        new_manager_public: [u8; 32],
        new_manager_rev: u64,
        restart_reason: Option<crate::rotation_apply::RestartReason>,
    ) -> Result<crate::rotation_apply::ApplyOutcome, SyncError> {
        self.freeze_rotation();
        let outcome = crate::rotation_apply::apply_epoch_change_with_client(
            self.client,
            vault,
            identity_path,
            identity,
            checkpoint_id,
            baseline_s,
            baseline_cursor,
            candidate_root,
            manager_public,
            new_manager_public,
            new_manager_rev,
            restart_reason,
        )?;
        if matches!(outcome, crate::rotation_apply::ApplyOutcome::Applied { .. }) {
            // same-epoch receipt 收尾时身份可能已被 rename 到更高 epoch（调用方也许已
            // 没有候选根）。若传入的根落后于已认证身份，以身份文件中的当前根为准；
            // 正常 epoch change 仍用本次候选根。
            if identity.current_epoch > candidate_root.key_epoch {
                self.set_root(RootSecretV1::new(
                    self.vault_id,
                    identity.current_epoch,
                    identity.current_root.clone(),
                ))?;
            } else {
                self.set_root(candidate_root.clone())?;
            }
            self.thaw_rotation();
        }
        Ok(outcome)
    }

    /// 当前使用的 epoch 根（rotation 后由调用方替换）。
    pub fn root(&self) -> &RootSecretV1 {
        &self.root
    }

    /// 替换当前 epoch 根（rotation/解锁后）。
    pub fn set_root(&mut self, root: RootSecretV1) -> Result<(), SyncError> {
        if root.vault_id != self.vault_id {
            return Err(SyncError::Descriptor("根归属 vault 与引擎不一致"));
        }
        self.root = root;
        Ok(())
    }

    /// 阶段 1：把 `pending_encryption` 意图加密成 MNE1 parts。
    pub fn encrypt_pending(&mut self, limit: usize) -> Result<usize, SyncError> {
        if self.rotation_frozen {
            return Err(SyncError::Paused(PauseReason::RotationFrozen));
        }
        let rows = pending_encryption(self.vault, limit)?;
        let mut encrypted = 0usize;
        for row in rows {
            if row.key_epoch != self.root.key_epoch {
                return Err(SyncError::Paused(PauseReason::EpochMismatch));
            }
            let object_id = row
                .object_id
                .ok_or(SyncError::LocalCorrupt("outbox intent 缺少 object_id"))?;
            let meta = storage_view::doc_meta(self.vault, &row.doc_id)?
                .ok_or(SyncError::LocalCorrupt("outbox 指向不存在的文档"))?;
            let stored = storage_view::stored_update(self.vault, &row.doc_id, row.seq)?;
            let base_vv =
                normalize_vv(&storage_view::base_vv_at(self.vault, &row.doc_id, row.seq)?);
            let message_id = decode_message_id(&row.message_id)?;
            let update = UpdateV1 {
                doc_id: row.doc_id.clone(),
                generation: meta.generation,
                codec_version: meta.codec_version,
                app_doc_schema: meta.app_doc_schema,
                min_reader: meta.min_reader,
                update_id: stored.update_id,
                batch_id: row.batch_id.clone(),
                origin_device_id: self.identity.device_id,
                origin_seq: row.seq,
                base_vv,
                end_vv: stored.end_vv,
                end_frontier: stored.end_frontier,
                loro_update_bytes: stored.payload,
            };
            let payload = update.encode()?;
            let parts = self.encrypt_update(
                &object_id,
                &message_id,
                meta.generation,
                row.key_epoch,
                row.seq,
                &payload,
            )?;
            let digest = object_digest_of(&parts)?;
            // CAS：同 message/epoch 已存在不同字节会失败，重试因此不会换 nonce。
            store_encrypted_parts(self.vault, &row.message_id, row.key_epoch, &digest, &parts)?;
            encrypted += 1;
        }
        Ok(encrypted)
    }

    /// 阶段 2：上传 parts + 批量提交 + durable ACK。
    pub fn push(&mut self) -> Result<PushReport, SyncError> {
        if self.rotation_frozen {
            return Err(SyncError::Paused(PauseReason::RotationFrozen));
        }
        // `encrypted` = 还没上传完；`sent` = 上次已上传但未获 ACK（必须继续提交）。
        let batch = self.config.send_batch.clamp(1, MAX_UPDATES_PER_BATCH);
        let mut rows = outbox_rows(self.vault, SyncOutboxState::Encrypted, batch)?;
        rows.extend(outbox_rows(self.vault, SyncOutboxState::Sent, batch)?);
        rows.truncate(batch);
        let mut report = PushReport::default();
        let mut submissions = Vec::with_capacity(rows.len());
        let mut expected: Vec<(ObjectId, String, u64, [u8; 32])> = Vec::with_capacity(rows.len());
        for row in rows {
            if row.key_epoch != self.root.key_epoch {
                return Err(SyncError::Paused(PauseReason::EpochMismatch));
            }
            let object_id = row
                .object_id
                .ok_or(SyncError::LocalCorrupt("outbox intent 缺少 object_id"))?;
            let digest = row
                .cipher_digest
                .ok_or(SyncError::LocalCorrupt("encrypted outbox 缺 cipher_digest"))?;
            let parts = outbox_parts(self.vault, &row.message_id, row.key_epoch)?;
            if parts.is_empty() {
                return Err(SyncError::LocalCorrupt("encrypted outbox 没有 parts"));
            }
            let computed = object_digest_of_parts(&parts)?;
            if computed != digest {
                return Err(SyncError::LocalCorrupt(
                    "outbox parts 与 cipher_digest 不一致（疑似被改写）",
                ));
            }
            if row.state == SyncOutboxState::Encrypted {
                for part in &parts {
                    self.client
                        .put_part(&object_id, part.part_no, &part.envelope)?;
                }
                // 上传成功后才推进 sent；submit 失败时保持 sent，下一轮继续提交。
                if !mark_sent(self.vault, &row.message_id, row.key_epoch, &digest)? {
                    return Err(SyncError::LocalCorrupt("mark_sent CAS 失败"));
                }
                report.uploaded += 1;
            }

            let first = Envelope::decode(&parts[0].envelope)?;
            if first.header.object_type != ObjectType::Update {
                return Err(SyncError::LocalCorrupt("outbox 里不是 UPDATE 对象"));
            }
            let commit = ObjectCommit {
                vault_id: self.vault_id,
                object_id,
                message_id: first.header.message_id,
                object_type: first.header.object_type,
                generation: first.header.generation,
                key_epoch: first.header.key_epoch,
                sender_seq: first.header.sender_seq,
                part_count: first.header.part_count,
                total_len: first.header.total_plaintext_len,
                object_digest: digest,
            };
            let signed_commit = commit.encode()?;
            let signature = self
                .identity
                .signing_key
                .sign(&object_commit_signing_input(&commit)?);
            expected.push((object_id, row.message_id.clone(), row.key_epoch, digest));
            submissions.push(UpdateSubmission {
                object_id,
                commit,
                signed_commit,
                signature,
            });
        }

        if submissions.is_empty() {
            return Ok(report);
        }
        let outcomes = self.client.submit_updates(submissions)?;
        let mut by_object: std::collections::HashMap<ObjectId, UpdateOutcome> = outcomes
            .into_iter()
            .map(|outcome| (outcome.object_id, outcome))
            .collect();
        for (object_id, message_id, epoch, digest) in expected {
            match by_object.remove(&object_id) {
                Some(UpdateOutcome {
                    status: UpdateStatus::Accepted { .. } | UpdateStatus::Duplicate,
                    ..
                }) => {
                    // durable ACK：CAS 成功才删除 outbox（旧 ACK 不会删新行）。
                    if mark_acked(self.vault, &message_id, epoch, &digest)? {
                        report.acked += 1;
                    }
                }
                Some(UpdateOutcome {
                    status: UpdateStatus::Failed { .. },
                    ..
                }) => report.failed += 1,
                None => return Err(SyncError::RelayWire("updates 响应缺少对象结果")),
            }
        }
        Ok(report)
    }

    /// 阶段 2b：**增量上传被正文引用的本地资产**（正文推送之后、拉取之前）。
    ///
    /// 顺序是刻意的（ADR 0012 的两阶段路径也复用本方法）：
    /// - 正文/结构更新先 `push`，资产后置；单条资产 relay/对象校验失败只计入
    ///   [`AssetSyncReport::failed`]，**不阻塞**本轮正文交付；
    /// - 资产在 `pull` 之前完成，远端设备下一次 pull 就能同时拿到正文与 ASSET 事件；
    /// - 已有 durable 标记的资产不读字节、不发请求；relay 上已完整的对象只校验+提交，
    ///   被其它 reservation 持有的对象延后（`deferred`），下一轮重试。
    ///
    /// **本地库损坏上抛**：`upload_one_asset` 发现本地字节长度/哈希与元数据不一致、
    /// 或确定性分片事实与元数据不符时返回 [`SyncError::LocalCorrupt`]。这类错误每轮
    /// 重试都不可能自愈，必须让调用方看到（否则就是"永远重试成功、永不报错"）。
    /// 处理方式是**继续扫完其余资产**（健康资产照常上传/落标记），最后把第一个本地
    /// 损坏原样上抛；[`SyncError::Paused`]（全局暂停）与 [`SyncError::Storage`]
    /// （本地库读写失败）仍**立即**上抛，语义不变。
    ///
    /// 只处理 `asset_refs` 引用得到的资产：历史孤儿不重传（`orphans_skipped` 如实上报）。
    pub fn upload_pending_assets(&mut self) -> Result<AssetSyncReport, SyncError> {
        if self.rotation_frozen {
            return Err(SyncError::Paused(PauseReason::RotationFrozen));
        }
        let mut report = AssetSyncReport {
            orphans_skipped: count_unreferenced_assets(self.vault)?,
            ..AssetSyncReport::default()
        };
        let mut first_local_corrupt: Option<SyncError> = None;
        let assets = list_referenced_assets(self.vault)?;
        report.referenced = assets.len();
        for meta in assets {
            let (Some(byte_len), Some(media_type)) = (meta.byte_len, meta.media_type.as_deref())
            else {
                report.skipped_invalid += 1;
                continue;
            };
            if byte_len <= 0 {
                // 空资产/无长度元数据不上传（storage 的 insert_asset 已拒绝空明文，
                // 这里兜底历史占位/损坏行）。
                report.skipped_invalid += 1;
                continue;
            }
            let Ok(byte_len) = u64::try_from(byte_len) else {
                report.skipped_invalid += 1;
                continue;
            };
            match self.upload_one_asset(&meta.asset_hash, byte_len, media_type) {
                Ok(AssetUploadOutcome::AlreadyUploaded) => report.already_uploaded += 1,
                Ok(AssetUploadOutcome::ReusedExisting {
                    parts_uploaded,
                    bytes_uploaded,
                }) => {
                    report.reused_existing += 1;
                    report.parts_uploaded += parts_uploaded;
                    report.bytes_uploaded += bytes_uploaded;
                }
                Ok(AssetUploadOutcome::Uploaded {
                    parts_uploaded,
                    bytes_uploaded,
                }) => {
                    report.uploaded += 1;
                    report.parts_uploaded += parts_uploaded;
                    report.bytes_uploaded += bytes_uploaded;
                }
                Ok(AssetUploadOutcome::Deferred) => report.deferred += 1,
                Ok(AssetUploadOutcome::SkippedInvalid) => report.skipped_invalid += 1,
                // 单资产失败不阻塞正文：但全局暂停（epoch/轮换）与本地库读写失败
                // 必须上抛，否则会把"整库不可用"伪装成一次成功的同步。
                Err(error) if matches!(error, SyncError::Paused(_) | SyncError::Storage(_)) => {
                    return Err(error)
                }
                // 本地库损坏不是网络失败：先继续上传其余健康资产（不让单个损坏拖住
                // 整库），最后统一上抛。计成 `failed` 会让它每轮"重试成功"、永不报错。
                Err(error) if matches!(error, SyncError::LocalCorrupt(_)) => {
                    if first_local_corrupt.is_none() {
                        first_local_corrupt = Some(error);
                    }
                }
                // 其余（relay/传输/对象校验）沿用既有语义：单资产失败，下一轮重试。
                Err(_) => report.failed += 1,
            }
        }
        if let Some(error) = first_local_corrupt {
            return Err(error);
        }
        Ok(report)
    }

    /// 单条资产：标记 → 探测 relay → 只补缺失分片 → 提交 → durable 标记。
    fn upload_one_asset(
        &mut self,
        hash: &[u8; 32],
        byte_len: u64,
        media_type: &str,
    ) -> Result<AssetUploadOutcome, SyncError> {
        asset_sync::ensure_vault(&self.root, self.vault_id)?;
        let object = asset_sync::local_asset_object(&self.root, hash, byte_len, media_type)?;
        let total = asset_sync::asset_plain_len(byte_len, media_type)?;
        let part_count = u32::try_from(part_count_for(total, ObjectType::Asset))
            .map_err(|_| SyncError::LocalCorrupt("资产分片数超出 u32"))?;
        if let Some(fact) = asset_sync::read_uploaded(self.vault, &object.object_id)? {
            if fact.key_epoch == object.key_epoch
                && fact.part_count == part_count
                && fact.total_plain_len == total
            {
                return Ok(AssetUploadOutcome::AlreadyUploaded);
            }
        }
        let Some(data) = asset_bytes(self.vault, hash)? else {
            return Ok(AssetUploadOutcome::SkippedInvalid);
        };
        if data.is_empty() {
            return Ok(AssetUploadOutcome::SkippedInvalid);
        }
        if data.len() as u64 != byte_len {
            return Err(SyncError::LocalCorrupt("资产字节长度与元数据不一致"));
        }
        let real: [u8; 32] = Sha256::digest(&data).into();
        if real != *hash {
            return Err(SyncError::LocalCorrupt("资产字节哈希与元数据不一致"));
        }
        let plaintext = crate::checkpoint::asset_plaintext(hash, byte_len, media_type, &data)?;
        let encrypted = crate::upload::encrypt_asset_parts(
            &self.root,
            self.vault_id,
            object.object_id,
            &plaintext,
        )?;
        if encrypted.part_count != part_count || encrypted.total_plain_len != total {
            return Err(SyncError::LocalCorrupt("资产确定性分片事实与元数据不一致"));
        }

        let mut parts_uploaded = 0usize;
        let mut bytes_uploaded = 0u64;
        let reused_complete = match asset_sync::read_part(self.client, &object.object_id, 0)? {
            None => {
                // relay 上完全没有该对象：普通预算完整上传。
                for (part_no, envelope, _) in &encrypted.parts {
                    self.client
                        .put_part(&object.object_id, *part_no, envelope)?;
                    parts_uploaded += 1;
                    bytes_uploaded += expected_part_plain_len(
                        ObjectType::Asset,
                        encrypted.total_plain_len,
                        *part_no,
                    )? as u64;
                }
                false
            }
            Some(existing) => {
                // 确定性加密下，同一 object_id 的分片字节必须一致；不一致只能是
                // 本地根/代码版本变化或 relay 被篡改，必须 fail-closed（本轮不计成功）。
                if existing != encrypted.parts[0].1 {
                    return Err(SyncError::Object("资产对象已有密文与本地确定性密文不一致"));
                }
                let mut missing = Vec::new();
                for (part_no, envelope, _) in encrypted.parts.iter().skip(1) {
                    match asset_sync::read_part(self.client, &object.object_id, *part_no)? {
                        Some(stored) => {
                            if stored != *envelope {
                                return Err(SyncError::Object(
                                    "资产对象已有分片与本地确定性密文不一致",
                                ));
                            }
                        }
                        None => missing.push((*part_no, envelope)),
                    }
                }
                if missing.is_empty() {
                    // checkpoint/轮换/其他设备已完整上传：只做提交（幂等）并落标记。
                    true
                } else {
                    // 上次普通上传中断：只补缺失片，已 durable 的片不重传。
                    for (part_no, envelope) in missing {
                        match self.client.put_part(&object.object_id, part_no, envelope) {
                            Ok(()) => {
                                parts_uploaded += 1;
                                bytes_uploaded += expected_part_plain_len(
                                    ObjectType::Asset,
                                    encrypted.total_plain_len,
                                    part_no,
                                )? as u64;
                            }
                            // 对象已被 checkpoint 的维护 reservation 持有：普通预算
                            // 不能续传，也不能改用别人的 reservation；延后到下一轮。
                            Err(error) if asset_sync::is_id_conflict(&error) => {
                                return Ok(AssetUploadOutcome::Deferred)
                            }
                            Err(error) => return Err(error),
                        }
                    }
                    false
                }
            }
        };

        crate::upload::submit_prepared_object(
            self.client,
            self.vault_id,
            &self.identity.signing_key,
            ObjectType::Asset,
            object.object_id,
            object.message_id,
            0,
            self.root.key_epoch,
            0,
            encrypted.part_count,
            encrypted.total_plain_len,
            &encrypted.object_digest,
        )?;
        let fact = asset_sync::uploaded_fact_from_object(
            &object,
            encrypted.part_count,
            encrypted.total_plain_len,
            encrypted.object_digest,
        );
        asset_sync::mark_uploaded(self.vault, &fact)?;
        if reused_complete {
            Ok(AssetUploadOutcome::ReusedExisting {
                parts_uploaded,
                bytes_uploaded,
            })
        } else {
            Ok(AssetUploadOutcome::Uploaded {
                parts_uploaded,
                bytes_uploaded,
            })
        }
    }

    /// 阶段 3：拉取一页日志并验收。
    ///
    /// 除游标/计数外还会填写 [`PullReport`] 的只读指标（事件/明文字节/资产字节/耗时/
    /// 页数/是否批量），并在成功返回前打一行脱敏 `eprintln!`（事件、字节、耗时，不含
    /// 路径/密钥/明文）。
    pub fn pull(&mut self) -> Result<PullReport, SyncError> {
        let started = Instant::now();
        let state = self.client.state()?;
        if state.vault_id != self.vault_id {
            return Err(SyncError::RelayWire("state.vault_id 与本地不一致"));
        }
        let local = sync_cursor_state(self.vault)?;
        match local.log_id {
            Some(log_id) if log_id != state.log_id => {
                return Err(SyncError::Paused(PauseReason::LogIdChanged));
            }
            None => set_sync_log_id(self.vault, &state.log_id)?,
            _ => {}
        }
        if let Some(highwater) = local.confirmed_highwater {
            if state.head_seq < highwater {
                return Err(SyncError::Paused(PauseReason::RollbackDetected));
            }
        }
        if state.gc_floor > local.received_seq {
            return Err(SyncError::Paused(PauseReason::CursorExpired));
        }
        if state.epoch != self.root.key_epoch {
            return Err(SyncError::Paused(PauseReason::EpochMismatch));
        }
        let Some(cursor) = self.read_cursor()? else {
            return Err(SyncError::Unsupported("缺少 relay 游标，必须先 bootstrap"));
        };
        let page = self.client.log(Some(&cursor), self.config.page_limit)?;
        if page.epoch != state.epoch {
            return Err(SyncError::Paused(PauseReason::EpochMismatch));
        }
        let mut report = PullReport {
            events: page.events.len(),
            pages: 1,
            ..PullReport::default()
        };
        // Phase 1A-2：`inbox_batch`（默认真）把一页的 verified 行攒起来，用一个事务写入。
        // 关键纪律：**任何提前返回都必须先 flush 已验收的行**——逐条路径在每条事件后
        // 就 durable，批量路径不能在 `Paused(UnsupportedEvent)` 之类的早退时把已验证行
        // 留在内存里丢掉。用立即执行的闭包承载事件循环，所有 `?` 早退都会落到闭包外的
        // 统一 flush（flush 也失败时保留原错误优先，只记录）。
        let mut buffered: Vec<VerifiedInboxRow> = Vec::new();
        let mut buffered_bytes = 0usize;
        let consume_result: Result<(), SyncError> = (|| {
            for event in &page.events {
                if event.relay_seq <= local.received_seq {
                    // ASSET 已推进 received 前缀后仍要核对 durable skipped 记录：relay 把
                    // 同一 seq 重写成另一条资产时必须 Paused(RelayHistoryRewritten)，不能
                    // 因为"水位已覆盖"就静默跳过（记录本身还绑定完整 commit 字节）。
                    if event.kind == RelayLogKind::Asset {
                        let bytes =
                            self.persist_skipped_asset(&state.log_id, event, &page.next_cursor)?;
                        report.asset_bytes = report.asset_bytes.saturating_add(bytes);
                    }
                    report.skipped += 1;
                    continue;
                }
                match event.kind {
                    RelayLogKind::Update => {
                        let row = self.verify_update_event(&state.log_id, event)?;
                        // 指标口径：成功验收的 Update **明文负载**字节（见 PullReport 注释）。
                        report.update_bytes = report.update_bytes.saturating_add(
                            u64::try_from(row.verified_payload.len()).unwrap_or(u64::MAX),
                        );
                        if self.config.inbox_batch {
                            buffered_bytes =
                                buffered_bytes.saturating_add(row.verified_payload.len());
                            buffered.push(row);
                            if buffered_bytes >= PULL_INBOX_BATCH_BYTES {
                                // 返回的"新插入行数"只用于区分幂等命中；`stored` 口径是
                                // "成功验收并 durable"，与逐条路径一致（重放行也算已 durable）。
                                store_inbox_verified_batch(self.vault, &buffered)?;
                                report.stored += buffered.len();
                                report.inbox_batch = true;
                                buffered.clear();
                                buffered_bytes = 0;
                            }
                        } else {
                            store_inbox_verified(
                                self.vault,
                                &row.log_id,
                                row.relay_seq,
                                &row.object_id,
                                row.epoch,
                                &row.verified_payload,
                            )?;
                            report.stored += 1;
                        }
                    }
                    // 资产事件不是文档水位事件：其密文本体已由 checkpoint refs 恢复，
                    // 新设备从 S 补齐日志时直接跳过，不阻塞后续 update。
                    //
                    // 评审 0003 P0-B 残余：**推进游标之前**先把"本机消费了这条 ASSET"
                    // 落成 durable 事实。否则 checkpoint 只能把"游标之前、无 inbox 行"
                    // 的缺口推断成 ASSET——恶意 relay 只要漏发一条 UPDATE 并推进 cursor
                    // 就能伪造覆盖。写记录前必须真正下载全部分片并完成密码学验证
                    // （见 `persist_skipped_asset`）；记录写入失败时整个 pull 返回错误，
                    // 游标不前进。
                    RelayLogKind::Asset => {
                        let bytes =
                            self.persist_skipped_asset(&state.log_id, event, &page.next_cursor)?;
                        report.asset_bytes = report.asset_bytes.saturating_add(bytes);
                        // ASSET 不写 inbox，但必须在 durable 消费后推进 received 连续前缀，
                        // 否则同一页里"ASSET 在前、UPDATE 在后"会让后续 UPDATE 永远不 ready。
                        mark_skipped_event_received(self.vault, &state.log_id, event.relay_seq)?;
                        report.skipped += 1;
                    }
                    // checkpoint/control 由 bootstrap/checkpoint 任务处理；静默跳过会
                    // 让 received 前缀出现洞，所以这里明确暂停。
                    _ => return Err(SyncError::Paused(PauseReason::UnsupportedEvent)),
                }
            }
            Ok(())
        })();
        // 先 flush 已验收行（`stored` 口径仍是"成功验收并 durable"；重放行幂等）。
        if !buffered.is_empty() {
            match store_inbox_verified_batch(self.vault, &buffered) {
                Ok(_) => {
                    report.stored += buffered.len();
                    report.inbox_batch = true;
                    buffered.clear();
                }
                Err(flush_error) => {
                    if let Err(original) = &consume_result {
                        // 原错误优先：flush 失败只记录，不掩盖触发早退的真实原因。
                        eprintln!(
                            "[sync] pull: 早退前 flush {} 条已验证 inbox 行失败（保留原错误）：\
                             {flush_error}; original={original}",
                            buffered.len()
                        );
                    } else {
                        return Err(SyncError::from(flush_error));
                    }
                }
            }
        }
        consume_result?;
        // 只有全部事件验收成功才推进游标（失败已在上面返回）。
        self.write_cursor(&page.next_cursor)?;
        report.next_cursor = page.next_cursor.clone();
        if page.query_head > local.confirmed_highwater.unwrap_or(0) {
            set_confirmed_highwater(self.vault, page.query_head)?;
        }
        report.elapsed_ms = elapsed_millis(started);
        // 脱敏日志：只有数量/字节/耗时，不含路径、密钥或明文（Phase 0 观测用）。
        eprintln!(
            "[sync] pull events={} stored={} skipped={} update_bytes={} asset_bytes={} \
             pages={} inbox_batch={} elapsed_ms={}",
            report.events,
            report.stored,
            report.skipped,
            report.update_bytes,
            report.asset_bytes,
            report.pages,
            report.inbox_batch,
            report.elapsed_ms
        );
        Ok(report)
    }

    /// 阶段 4：把 verified inbox 中依赖就绪的对象集成进本地库（旧入口）。
    ///
    /// 等价于 [`Self::integrate_dependency_ordered`] + [`IntegrateConfig::with_object_budget`]：
    /// 保留"一次调用最多处理 `limit` 个对象"的兼容语义，但内部已按 ADR 0012 S2b 的
    /// 结构→投影→正文顺序收敛（不再需要调用方再发一轮网络）。
    ///
    /// Phase 1A-2 起正文默认按文档聚合：`limit` 仍决定批数与结构对象预算，但同一文档
    /// 的待处理行会在一批内一起集成（可能超过 `limit` 行）；需要严格逐对象预算时用
    /// [`IntegrateConfig::aggregate_body`] 显式关闭聚合。
    pub fn integrate(&mut self, limit: usize) -> Result<IntegrateReport, SyncError> {
        let config = IntegrateConfig::with_object_budget(limit);
        self.integrate_dependency_ordered(config)
            .map(|outcome| outcome.report)
    }

    /// 阶段 4（ADR 0012 S2b）：**依赖有序**地把 verified inbox 集成进本地库。
    ///
    /// 顺序是硬约束：
    ///
    /// 1. 先集成 [`STRUCTURE_DOC_ID`] 的 durable inbox 行；
    /// 2. 只有结构 head **真的前进**才用 [`menghunote_storage::StructureWriter`] 做一次
    ///    **纯投影**（`commit` 在没有新 op 时只重建 `tree/notes` 与 body 行）；
    /// 3. 再按 doc 轮转公平集成正文；若仍有 deferred，在**同一次调用**内重试一批
    ///    （不要求调用方再发一轮网络）。
    ///
    /// 批预算见 [`IntegrateConfig`]；无进展就停（不循环刷最早一批），`deferred` 如实上报。
    pub fn integrate_dependency_ordered(
        &mut self,
        config: IntegrateConfig,
    ) -> Result<IntegrateOutcome, SyncError> {
        let started = Instant::now();
        // 结构行是 vault 的长期不变量；但 engine 的单元测试会用"没有结构文档"的裸库，
        // 因此这里把它当**可选**前置：没有结构行 → 不投影（结构 update 会按未知文档
        // deferred，和旧行为一致）。
        let structure_before = self.structure_head_seq()?;
        let mut outcome = IntegrateOutcome::default();
        // 扫描窗口要明显大于单批上限：否则 `(log_id, relay_seq)` 顺序会让后面的 doc
        // 永远进不了候选（公平取批就无从谈起）。
        let scan_limit = config
            .batch_objects
            .saturating_mul(16)
            .clamp(64, 1024)
            .max(1);

        // 1) 结构阶段：结构行优先，绝不因正文 backlog 饿死。
        let mut batches = 0usize;
        while batches < config.max_batches {
            let pending = self.scan_ready_rows(scan_limit)?;
            let selected = select_structure_batch(&pending, &config);
            if selected.is_empty() {
                break;
            }
            batches += 1;
            if self.integrate_selected(&pending, &selected, &mut outcome)? == 0 {
                // 结构文档行一定存在；真无进展就停（不循环刷同一批）。
                break;
            }
        }

        // 2) 投影：只有结构 head 真变才做，且**只做投影**（无新 op）。
        //    失败 → 明确错误（调用方必须保持写禁用/不假刷新）；结构 CRDT 已 durable。
        if let Some(before) = structure_before {
            let after = self.structure_head_seq()?.unwrap_or(before);
            if after != before {
                self.project_structure_only()?;
                outcome.report.projected = true;
                outcome.changed_heads.push(STRUCTURE_DOC_ID.to_owned());
            }
        }

        // 3) 正文阶段。
        if config.aggregate_body {
            // Phase 1A-2：按文档聚合。每篇文档一批只做一次 `load_body_doc`，行内按
            // relay_seq 升序 import，最后把增量合成**一条** update 落盘、同事务标记
            // inbox；整轮正文结束后统一推进一次 integrated 前缀。
            let body_result =
                self.integrate_body_aggregated(&config, scan_limit, &mut outcome, &mut batches);
            // 即使正文中途报错，也要推进已经提交的 integrated 行前缀（deferred/错误不是
            // 丢数据）；前缀推进失败不得掩盖正文的原错误。
            let prefix_result = advance_integrated_prefix(self.vault).map_err(SyncError::from);
            match (body_result, prefix_result) {
                (Ok(()), Ok(())) => {}
                (Ok(()), Err(error)) => return Err(error),
                (Err(error), Ok(())) => return Err(error),
                (Err(error), Err(prefix_error)) => {
                    eprintln!(
                        "[sync] integrate: 正文报错后推进 integrated 前缀失败（保留原错误）：\
                         {prefix_error}; original={error}"
                    );
                    return Err(error);
                }
            }
        } else {
            // 旧逐行路径（基准对照）：每条 update 一次全量重放 + 一次 A 事务 + 一次标记。
            let mut retried_without_progress = false;
            while batches < config.max_batches {
                let pending = self.scan_ready_rows(scan_limit)?;
                let selected = select_body_batch(&pending, &config, batches);
                if selected.is_empty() {
                    break;
                }
                batches += 1;
                let progressed = self.integrate_selected(&pending, &selected, &mut outcome)?;
                if progressed == 0 {
                    if retried_without_progress {
                        // 连续两批零进展：依赖确实没就绪，停止并上报剩余 deferred。
                        break;
                    }
                    // 同一次 integrate 内对剩余 deferred 重试一次（结构投影可能刚建立 body 行）。
                    retried_without_progress = true;
                } else {
                    retried_without_progress = false;
                }
            }
        }

        outcome.report.batches = batches;
        // 剩余 verified 行：依赖未就绪或被本次批数上限留下，都不是丢失。
        let remaining = count_inbox_ready(self.vault)?;
        outcome.report.deferred = usize::try_from(remaining).unwrap_or(usize::MAX);
        outcome.report.elapsed_ms = elapsed_millis(started);
        // 脱敏日志：只有计数/耗时（Phase 0 观测用）。
        eprintln!(
            "[sync] integrate integrated={} structure={} body={} deferred={} batches={} \
             projected={} doc_loads={} elapsed_ms={}",
            outcome.report.integrated,
            outcome.report.structure_integrated,
            outcome.report.body_integrated,
            outcome.report.deferred,
            outcome.report.batches,
            outcome.report.projected,
            outcome.report.doc_loads,
            outcome.report.elapsed_ms
        );
        Ok(outcome)
    }

    /// 结构文档的当前 head seq（没有结构行 = `None`）。
    fn structure_head_seq(&self) -> Result<Option<u64>, SyncError> {
        if !storage_view::doc_exists(self.vault, STRUCTURE_DOC_ID)? {
            return Ok(None);
        }
        Ok(Some(load_head(self.vault, STRUCTURE_DOC_ID)?.head_seq))
    }

    /// 扫一批「已接收连续前缀内、verified、未集成」的 inbox 行，并解码出 doc id。
    fn scan_ready_rows(&self, limit: usize) -> Result<Vec<PendingInboxRow>, SyncError> {
        let rows = take_inbox_ready(self.vault, limit)?;
        let mut pending = Vec::with_capacity(rows.len());
        for row in rows {
            let payload = row
                .verified_payload
                .as_deref()
                .ok_or(SyncError::LocalCorrupt("verified inbox 缺 payload"))?;
            let update = UpdateV1::decode(payload)?;
            pending.push(PendingInboxRow {
                doc_id: update.doc_id,
                size: payload.len(),
                row,
            });
        }
        Ok(pending)
    }

    /// 集成选中的行（逐行路径：结构阶段与 `aggregate_body = false` 的正文阶段）；
    /// 返回**真的有进展**（integrated）的对象数。
    fn integrate_selected(
        &self,
        pending: &[PendingInboxRow],
        selected: &[usize],
        outcome: &mut IntegrateOutcome,
    ) -> Result<usize, SyncError> {
        let mut progressed = 0usize;
        for &index in selected {
            let item = &pending[index];
            match self.integrate_inbox_row(&item.row, &mut outcome.report.doc_loads)? {
                InboxRowOutcome::Integrated { changed } => {
                    outcome.report.integrated += 1;
                    if item.doc_id == STRUCTURE_DOC_ID {
                        outcome.report.structure_integrated += 1;
                    } else {
                        outcome.report.body_integrated += 1;
                    }
                    if changed
                        && !outcome
                            .changed_heads
                            .iter()
                            .any(|head| head == &item.doc_id)
                    {
                        outcome.changed_heads.push(item.doc_id.clone());
                    }
                    progressed += 1;
                }
                // 依赖未就绪：保持 durable verified，最终由 `count_inbox_ready` 统一上报。
                InboxRowOutcome::Deferred => {}
            }
        }
        Ok(progressed)
    }

    /// 集成**一个** inbox 行（每个对象自己的 A 事务 + `mark_integrated`）。
    ///
    /// `HeadConflict` 是可重放的并发冲突（别的 writer 把 head 推进了）：不覆盖 head、
    /// 不丢 inbox/outbox，重读 head 后按新基准**有界重放一次**；再冲突就如实报错。
    ///
    /// `doc_loads` 累计本阶段 `load_body_doc`（全量重放）次数：逐行路径每 import 一行
    /// 就 +1（Phase 0 证明的 O(历史 × 事件) 就在这里）；聚合路径按文档 +1。
    fn integrate_inbox_row(
        &self,
        row: &SyncInboxRow,
        doc_loads: &mut usize,
    ) -> Result<InboxRowOutcome, SyncError> {
        let payload = row
            .verified_payload
            .as_deref()
            .ok_or(SyncError::LocalCorrupt("verified inbox 缺 payload"))?;
        let update = UpdateV1::decode(payload)?;
        if !storage_view::doc_exists(self.vault, &update.doc_id)? {
            return Ok(InboxRowOutcome::Deferred);
        }
        let head = load_head(self.vault, &update.doc_id)?;
        let local_vv = head.version_vector()?;
        // 本地已覆盖 end_vv → 这个 update 已经应用过（典型是自己产生的 update 从 relay
        // 回环回来，或同 ops 的重复投递）。不 import、不分配新 seq，只标记已集成。
        let end_vv = if update.end_vv.is_empty() {
            VersionVector::new()
        } else {
            VersionVector::decode(&update.end_vv)
                .map_err(|_| SyncError::Object("end_vv 解码失败"))?
        };
        if vv_includes(&local_vv, &end_vv) {
            mark_integrated(self.vault, &row.log_id, row.relay_seq)?;
            return Ok(InboxRowOutcome::Integrated { changed: false });
        }
        let base_vv = if update.base_vv.is_empty() {
            VersionVector::new()
        } else {
            VersionVector::decode(&update.base_vv)
                .map_err(|_| SyncError::Object("base_vv 解码失败"))?
        };
        if !vv_includes(&local_vv, &base_vv) {
            // 依赖未齐：保持 durable verified，等前面的对象到达。
            return Ok(InboxRowOutcome::Deferred);
        }
        let mut conflict_retries = 0usize;
        loop {
            let (doc, current) =
                load_body_doc(self.vault, &update.doc_id, self.identity.peer_id, None)?;
            *doc_loads = doc_loads.saturating_add(1);
            let before = doc.vv();
            doc.import(&update.loro_update_bytes)?;
            let pending = doc.pending_update(&before)?;
            if pending.payload.is_empty() {
                break;
            }
            match append_updates(
                self.vault,
                &update.doc_id,
                current.head_seq,
                &[UpdateRecord::new(update.batch_id.clone(), pending)],
            ) {
                Ok(head_after) => {
                    // 本地 seq 只能由 head+1 分配（禁止 sender_seq）。
                    if head_after.head_seq != current.head_seq.saturating_add(1) {
                        return Err(SyncError::LocalCorrupt("本地 seq 未按 head+1 分配"));
                    }
                    mark_integrated(self.vault, &row.log_id, row.relay_seq)?;
                    return Ok(InboxRowOutcome::Integrated { changed: true });
                }
                // ADR 0012 S2b：并发本地写入把 head 推进了 → 重读 head 后重放一次。
                Err(menghunote_storage::StorageError::HeadConflict { .. })
                    if conflict_retries == 0 =>
                {
                    conflict_retries += 1;
                    continue;
                }
                Err(error) => return Err(storage_view::map_storage_error(error)),
            }
        }
        mark_integrated(self.vault, &row.log_id, row.relay_seq)?;
        Ok(InboxRowOutcome::Integrated { changed: false })
    }

    // ---------------------------------------------------------------- 正文聚合（Phase 1A-2）

    /// 正文聚合路径的批循环。
    ///
    /// 与逐行路径的区别只在选择与交付粒度：每批最多 `batch_objects` **篇文档**
    /// （而不是 32 行），同一文档的候选行一次处理；批之间仍按 `batches` 轮转起点，
    /// 多文档时不会总从第一篇开始。选批前先用 `head_vv` 做轻量预检：纯 deferred 组
    /// 直接跳过、不占正文预算（避免大 deferred 文档饿死后面的就绪文档）。
    /// 有进展才继续，连续两批零进展就停。
    fn integrate_body_aggregated(
        &self,
        config: &IntegrateConfig,
        scan_limit: usize,
        outcome: &mut IntegrateOutcome,
        batches: &mut usize,
    ) -> Result<(), SyncError> {
        let mut retried_without_progress = false;
        while *batches < config.max_batches {
            let pending = self.scan_ready_rows(scan_limit)?;
            let selected = select_body_batch_aggregated(self.vault, &pending, config, *batches)?;
            if selected.is_empty() {
                break;
            }
            *batches += 1;
            let progressed = self.integrate_body_selected(&pending, &selected, outcome)?;
            if progressed == 0 {
                if retried_without_progress {
                    // 连续两批零进展：依赖确实没就绪，停止并上报剩余 deferred。
                    break;
                }
                // 同一次 integrate 内对剩余 deferred 重试一次（结构投影可能刚建立 body 行）。
                retried_without_progress = true;
            } else {
                retried_without_progress = false;
            }
        }
        Ok(())
    }

    /// 处理一批正文行：按 `(doc_id, log_id)` 分组；同一文档的候选行在 `pending` 里
    /// 已按 relay_seq 升序，因此每个 doc 组内部天然有序。
    fn integrate_body_selected(
        &self,
        pending: &[PendingInboxRow],
        selected: &[usize],
        outcome: &mut IntegrateOutcome,
    ) -> Result<usize, SyncError> {
        let mut groups: Vec<(&str, [u8; 16], Vec<usize>)> = Vec::new();
        for &index in selected {
            let item = &pending[index];
            match groups
                .iter_mut()
                .find(|group| group.0 == item.doc_id.as_str() && group.1 == item.row.log_id)
            {
                Some((_, _, rows)) => rows.push(index),
                None => groups.push((item.doc_id.as_str(), item.row.log_id, vec![index])),
            }
        }
        let mut progressed = 0usize;
        for (doc_id, log_id, indices) in groups {
            progressed = progressed.saturating_add(
                self.integrate_body_doc_group(pending, doc_id, &log_id, &indices, outcome)?,
            );
        }
        Ok(progressed)
    }

    /// 聚合处理**一篇文档**的候选行；返回真正标记为 integrated 的行数。
    ///
    /// 顺序与失败语义（Phase 1A-2 的硬约束 + Phase 1A engine 聚合缺陷收口）：
    ///
    /// 1. **轻量预检**（不 `load_body_doc`）：只 [`load_head`] 取 O(1) 的 `head_vv`；
    ///    - 全部行的 `end_vv` 都已被覆盖 → [`mark_integrated_batch`] 一次标记后返回，
    ///      零重放；
    ///    - 未覆盖行里没有任何一行的 `base_vv` 满足 → 纯 deferred，直接返回、不重放；
    ///    - 只有至少一行需要 import 时才 `load_body_doc`（`HeadConflict` 重试才再读）；
    /// 2. 进入内存后对组内行做**多轮合并**：每轮把已覆盖的行标记、把 `base_vv` 已满足
    ///    的行 import，剩下的留到下一轮；只要本轮有新的 import 就继续（后 import 的 op
    ///    可能刚好满足前面行的依赖 → 物理乱序自愈），没有新 import 就停。真正缺外部
    ///    依赖的行保持 verified，**不会因为中间某行 deferred 而丢弃已成功的前缀**；
    /// 3. 所有成功 import 的 ops 用**一次** `pending_update(&before)` 合成一条
    ///    `UpdateRecord`，与所有可标记行的 `state='integrated'` 标记进同一个 A 事务
    ///    （inbox 行不满足 `verified` 则整笔回滚，组内不落盘）；
    /// 4. 只有已覆盖行时用 [`mark_integrated_batch`] 一次标记；
    /// 5. 任何 import 错误都原样返回：整组不落盘、不标记，inbox 保持 verified；
    /// 6. `HeadConflict` 重读 head 后**有界重试一次**，再冲突就报错。
    ///
    /// 本函数**不推进** integrated 前缀；调用方在整批正文结束后统一推进一次。
    fn integrate_body_doc_group(
        &self,
        pending: &[PendingInboxRow],
        doc_id: &str,
        log_id: &[u8; 16],
        indices: &[usize],
        outcome: &mut IntegrateOutcome,
    ) -> Result<usize, SyncError> {
        if indices.is_empty() || !storage_view::doc_exists(self.vault, doc_id)? {
            return Ok(0);
        }
        // 1) 轻量预检：只读 O(1) 的 head_vv，不重放历史。
        let head_vv = load_head(self.vault, doc_id)?.version_vector()?;
        match body_group_readiness(pending, indices, &head_vv) {
            // 纯 deferred：整组保持 durable verified，不 load_body_doc。
            BodyGroupReadiness::Deferred => return Ok(0),
            BodyGroupReadiness::Covered => {
                let seqs = relay_seqs_of(pending, indices);
                mark_integrated_batch(self.vault, log_id, &seqs)?;
                let count = seqs.len();
                outcome.report.integrated += count;
                outcome.report.body_integrated += count;
                return Ok(count);
            }
            BodyGroupReadiness::Ready => {}
        }
        let mut conflict_retries = 0usize;
        loop {
            let (doc, current) = load_body_doc(self.vault, doc_id, self.identity.peer_id, None)?;
            outcome.report.doc_loads = outcome.report.doc_loads.saturating_add(1);
            let before = doc.vv();
            let mut integrated_seqs = Vec::with_capacity(indices.len());
            let mut imported = 0usize;
            let mut last_batch_id: Option<String> = None;
            // 2) 同文档内存多轮合并：只有本轮真的 import 了新 op 才再扫一轮。
            let mut remaining: Vec<usize> = indices.to_vec();
            while !remaining.is_empty() {
                let mut deferred: Vec<usize> = Vec::new();
                let mut round_imported = 0usize;
                for &index in &remaining {
                    let item = &pending[index];
                    let payload = item
                        .row
                        .verified_payload
                        .as_deref()
                        .ok_or(SyncError::LocalCorrupt("verified inbox 缺 payload"))?;
                    let update = UpdateV1::decode(payload)?;
                    // 先看 end_vv：本地已覆盖 = 这个 update 应用过（自己产生的回环/重复
                    // 投递，或本轮前面的 import 已把它包含的 ops 带进来），只标记。
                    let end_vv = decode_vv(&update.end_vv, "end_vv 解码失败")?;
                    if vv_includes(&doc.vv(), &end_vv) {
                        integrated_seqs.push(item.row.relay_seq);
                        continue;
                    }
                    // 依赖未齐：留到下一轮（本轮后面的 import 可能刚好满足它）。
                    let base_vv = decode_vv(&update.base_vv, "base_vv 解码失败")?;
                    if !vv_includes(&doc.vv(), &base_vv) {
                        deferred.push(index);
                        continue;
                    }
                    doc.import(&update.loro_update_bytes)?;
                    imported += 1;
                    round_imported += 1;
                    last_batch_id = Some(update.batch_id);
                    integrated_seqs.push(item.row.relay_seq);
                }
                remaining = deferred;
                // 本轮没有任何新 import：剩下的行缺的是组外依赖，多扫也扫不出来。
                if round_imported == 0 {
                    break;
                }
            }
            if integrated_seqs.is_empty() {
                // 预检说 Ready、重放后却一行都动不了（并发/损坏）：整组保持 verified。
                return Ok(0);
            }
            let changed = imported > 0;
            if imported == 0 {
                // 只有已覆盖行：一次事务标记，不写 update。
                mark_integrated_batch(self.vault, log_id, &integrated_seqs)?;
            } else {
                let combined = doc.pending_update(&before)?;
                if combined.payload.is_empty() {
                    // 防御性分支：没有新 ops 时不能追加空 update，只标记。
                    mark_integrated_batch(self.vault, log_id, &integrated_seqs)?;
                } else {
                    let batch_id = last_batch_id.unwrap_or_else(|| "aggregate".to_owned());
                    let record = UpdateRecord::new(batch_id, combined);
                    match append_updates_with_integrated_marks(
                        self.vault,
                        doc_id,
                        current.head_seq,
                        &record,
                        log_id,
                        &integrated_seqs,
                    ) {
                        Ok(head_after) => {
                            // 本地 seq 只能由 head+1 分配（一次组合落盘 = 一个 seq）。
                            if head_after.head_seq != current.head_seq.saturating_add(1) {
                                return Err(SyncError::LocalCorrupt("本地 seq 未按 head+1 分配"));
                            }
                        }
                        // 并发 writer 推进了 head：整组没有落盘/标记，重读后重放一次。
                        Err(menghunote_storage::StorageError::HeadConflict { .. })
                            if conflict_retries == 0 =>
                        {
                            conflict_retries += 1;
                            continue;
                        }
                        Err(error) => return Err(storage_view::map_storage_error(error)),
                    }
                }
            }
            let count = integrated_seqs.len();
            outcome.report.integrated += count;
            outcome.report.body_integrated += count;
            if changed && !outcome.changed_heads.iter().any(|head| head == doc_id) {
                outcome.changed_heads.push(doc_id.to_owned());
            }
            return Ok(count);
        }
    }

    /// 只做投影：没有新结构 op 时 `commit` 仅重建 `tree/notes` 与 body 行（幂等）。
    ///
    /// 失败返回 [`SyncError::StructureProjection`]：结构 CRDT 已 durable，但本地投影
    /// **没有**追上，调用方必须明确报错并保持写禁用（不得假刷新）。
    fn project_structure_only(&self) -> Result<(), SyncError> {
        let sync = SyncContext::new(self.identity.peer_id, self.root.key_epoch);
        let mut writer = menghunote_storage::StructureWriter::open_with_sync(
            self.vault,
            self.identity.peer_id,
            Some(sync),
        )
        .map_err(|error| SyncError::StructureProjection(error.to_string()))?;
        writer
            .commit("s2b-project")
            .map_err(|error| SyncError::StructureProjection(error.to_string()))?;
        Ok(())
    }

    /// 上报 received/integrated 游标。
    pub fn ack(&mut self) -> Result<AckReport, SyncError> {
        let state = self.client.state()?;
        if state.epoch != self.root.key_epoch {
            return Err(SyncError::Paused(PauseReason::EpochMismatch));
        }
        let local = sync_cursor_state(self.vault)?;
        self.client
            .ack(local.received_seq, local.integrated_seq, state.epoch)?;
        if local
            .confirmed_highwater
            .is_none_or(|high| state.head_seq > high)
        {
            set_confirmed_highwater(self.vault, state.head_seq)?;
        }
        Ok(AckReport {
            received_seq: local.received_seq,
            integrated_seq: local.integrated_seq,
        })
    }

    /// 一轮完整同步（加密 → 推送 → 拉取 → 集成 → ACK）。
    pub fn sync_once(&mut self) -> Result<SyncReport, SyncError> {
        self.sync_once_with_guard(&mut || Ok(()))
    }

    /// 同 [`Self::sync_once`]，但在**首次外部集成之前**调用一次 `guard`（ADR 0012 S1）。
    ///
    /// 语义（安全优先）：
    ///
    /// - `guard` 返回 `Err` → 本轮**不 append**：在此之前只做了加密/推送/拉取（这些不改
    ///   本地正文 head），错误原样返回（取消用 [`SyncError::CancelledBeforeIntegrate`]）；
    /// - `guard` 返回 `Ok` → 正常集成；**进入 integrate 后不再检查**，真实事务必须跑完并
    ///   交付（已提交的 durable 变化不能被"取消"吞掉）；
    /// - 只在 `integrate` 之前调用一次，不是每对象回调（S2 才做可暂停的分批握手）。
    pub fn sync_once_with_guard(
        &mut self,
        guard: &mut dyn FnMut() -> Result<(), SyncError>,
    ) -> Result<SyncReport, SyncError> {
        let encrypted = self.encrypt_pending(self.config.send_batch)?;
        let push = self.push()?;
        // 正文优先：正文 push 之后才上传资产；单资产失败不阻塞后续 pull/integrate。
        let assets = self.upload_pending_assets()?;
        let pull = self.pull()?;
        guard()?;
        let integrate = self.integrate(self.config.integrate_limit)?;
        let ack = self.ack()?;
        Ok(SyncReport {
            encrypted,
            push,
            assets,
            pull,
            integrate,
            ack,
        })
    }

    // ---------------------------------------------------------------- 内部

    fn encrypt_update(
        &self,
        object_id: &ObjectId,
        message_id: &[u8; MESSAGE_ID_LEN],
        generation: u64,
        key_epoch: u64,
        sender_seq: u64,
        plaintext: &[u8],
    ) -> Result<Vec<SyncEncryptedPart>, SyncError> {
        if plaintext.is_empty() {
            return Err(SyncError::LocalCorrupt("update 载荷为空"));
        }
        let object_type = ObjectType::Update;
        let total = plaintext.len() as u64;
        let part_count = u32::try_from(part_count_for(total, object_type))
            .map_err(|_| SyncError::LocalCorrupt("分片数超出 u32"))?;
        let key = derive_object_key(&self.root, object_type, object_id)?;
        let cipher = XChaCha20Poly1305::new_from_slice(key.expose_slice())
            .map_err(|_| SyncError::Object("数据密钥长度非法"))?;
        let cap = cap_plain(object_type) as usize;
        let mut parts = Vec::with_capacity(part_count as usize);
        let mut rng = crate::random::OsRandom;
        for index in 0..part_count {
            let this_len = expected_part_plain_len(object_type, total, index)?;
            let start = index as usize * cap;
            let end = start
                .checked_add(this_len)
                .ok_or(SyncError::LocalCorrupt("分片偏移溢出"))?;
            let chunk = plaintext
                .get(start..end)
                .ok_or(SyncError::LocalCorrupt("分片超出载荷长度"))?;
            let nonce = crate::random::array::<NONCE_LEN>(&mut rng)?;
            let header = EnvelopeHeader {
                protocol_version: PROTOCOL_VERSION,
                header_len: HEADER_LEN,
                object_type,
                cipher_suite: CIPHER_SUITE,
                flags: 0,
                vault_id: self.vault_id,
                object_id: *object_id,
                message_id: *message_id,
                producer_device_id: self.identity.device_id,
                generation,
                key_epoch,
                sender_seq,
                part_index: index,
                part_count,
                total_plaintext_len: total,
                this_plaintext_len: u32::try_from(chunk.len())
                    .map_err(|_| SyncError::LocalCorrupt("分片明文超出 u32"))?,
                ciphertext_len: u32::try_from(chunk.len() + 16)
                    .map_err(|_| SyncError::LocalCorrupt("分片密文超出 u32"))?,
                context_hash: [0u8; 32],
                nonce,
            };
            let aad = header.aad(&[])?;
            let ciphertext = cipher
                .encrypt(
                    XNonce::from_slice(&nonce),
                    Payload {
                        msg: chunk,
                        aad: &aad,
                    },
                )
                .map_err(|_| SyncError::Object("数据对象加密失败"))?;
            let envelope = Envelope::new(header, ciphertext)?.encode()?;
            let digest: [u8; 32] = Sha256::digest(&envelope).into();
            parts.push(SyncEncryptedPart {
                part_no: index,
                envelope,
                digest,
            });
        }
        Ok(parts)
    }

    /// 只做**验收**（验签/摘要/解密/载荷校验），不写库；返回可批量写入的 inbox 行。
    ///
    /// 拆出这一步是为了让 `pull` 能在 `EngineConfig::inbox_batch` 打开时把整页行用
    /// 一个事务写入（指标/实验路径）；默认路径仍是"验收一条、立刻 durable 一条"。
    fn verify_update_event(
        &mut self,
        log_id: &[u8; 16],
        event: &RelayLogEvent,
    ) -> Result<VerifiedInboxRow, SyncError> {
        let object_id = event
            .object_id
            .ok_or(SyncError::Object("update 事件缺少 object_id"))?;
        if event.epoch != self.root.key_epoch {
            return Err(SyncError::Paused(PauseReason::EpochMismatch));
        }
        let signature = event
            .signature
            .ok_or(SyncError::Paused(PauseReason::MissingSignature))?;
        let commit = ObjectCommit::decode(&event.event)?;
        if commit.vault_id != self.vault_id
            || commit.object_id != object_id
            || commit.key_epoch != event.epoch
            || commit.object_type != ObjectType::Update
        {
            return Err(SyncError::Object("ObjectCommit 与日志事件不一致"));
        }

        let mut raw_parts = Vec::with_capacity(commit.part_count as usize);
        let mut envelopes = Vec::with_capacity(commit.part_count as usize);
        for part_no in 0..commit.part_count {
            let bytes = self.client.part(&object_id, part_no)?;
            let envelope =
                Envelope::decode(&bytes).map_err(|_| SyncError::Object("信封解码失败"))?;
            let header = &envelope.header;
            if header.object_type != ObjectType::Update
                || header.vault_id != self.vault_id
                || header.object_id != object_id
                || header.part_index != part_no
                || header.part_count != commit.part_count
                || header.key_epoch != commit.key_epoch
                || header.generation != commit.generation
                || header.sender_seq != commit.sender_seq
                || header.total_plaintext_len != commit.total_len
            {
                return Err(SyncError::Object("信封 header 与 ObjectCommit 不一致"));
            }
            raw_parts.push(bytes);
            envelopes.push(envelope);
        }

        // object_digest = SHA256(BE32(part_count) || concat(BE32(no) || SHA256(part)))
        let hashes: Vec<[u8; 32]> = raw_parts
            .iter()
            .map(|bytes| {
                let digest: [u8; 32] = Sha256::digest(bytes).into();
                digest
            })
            .collect();
        let computed: [u8; 32] =
            Sha256::digest(object_digest_input(commit.part_count, &hashes)?).into();
        if computed != commit.object_digest {
            return Err(SyncError::Object("object_digest 不符"));
        }

        // 生产者一致性 + 证书验签。
        let producer = envelopes[0].header.producer_device_id;
        let message_id = envelopes[0].header.message_id;
        if envelopes.iter().any(|env| {
            env.header.producer_device_id != producer || env.header.message_id != message_id
        }) {
            return Err(SyncError::Object(
                "各片 header 的 producer/message_id 不一致",
            ));
        }
        let cert = self
            .client
            .device_cert(&producer)?
            .ok_or(SyncError::Object("缺少生产者设备证书"))?;
        if cert.vault_id != self.vault_id || cert.device_id != producer {
            return Err(SyncError::Object("设备证书归属不符"));
        }
        verify_signature(
            &cert.sign_ed25519_pub,
            &object_commit_signing_input(&commit)?,
            &signature,
        )?;

        // AEAD 解密 + §16 载荷校验。
        let key = derive_object_key(&self.root, ObjectType::Update, &object_id)?;
        let cipher = XChaCha20Poly1305::new_from_slice(key.expose_slice())
            .map_err(|_| SyncError::Object("数据密钥长度非法"))?;
        let mut plaintext = Vec::with_capacity(commit.total_len as usize);
        for (index, envelope) in envelopes.iter().enumerate() {
            let expected = expected_part_plain_len(
                ObjectType::Update,
                commit.total_len,
                u32::try_from(index).map_err(|_| SyncError::Object("分片下标溢出"))?,
            )?;
            if envelope.header.this_plaintext_len as usize != expected {
                return Err(SyncError::Object("分片明文长度不符合固定切片"));
            }
            let aad = envelope.header.aad(&[])?;
            let chunk = cipher
                .decrypt(
                    XNonce::from_slice(&envelope.header.nonce),
                    Payload {
                        msg: &envelope.ciphertext,
                        aad: &aad,
                    },
                )
                .map_err(|_| SyncError::Object("AEAD 解密失败"))?;
            plaintext.extend_from_slice(&chunk);
        }
        if plaintext.len() as u64 != commit.total_len {
            return Err(SyncError::Object("解密后的明文长度与 commit 不符"));
        }
        let update = UpdateV1::decode(&plaintext)?;
        if update.generation != commit.generation || update.origin_seq != commit.sender_seq {
            return Err(SyncError::Object("UpdateV1 与 ObjectCommit 不一致"));
        }
        envelopes[0].header.validate_update_payload(&update)?;

        Ok(VerifiedInboxRow {
            log_id: *log_id,
            relay_seq: event.relay_seq,
            object_id,
            epoch: event.epoch,
            verified_payload: plaintext,
        })
    }

    /// 从 relay 完整读回并验证一个 ASSET 对象（评审 0003 P0-B：ASSET 来源认证）。
    ///
    /// 日志事件里的 `ObjectCommit` 只是 relay 的声明。只有真正取回全部分片、用本
    /// epoch 根解密成功，并且资产明文头能重算出同一个 `object_id`，才能证明该事件
    /// 对应一个真实存在的密文资产；任何一步失败都返回错误，调用方不得写记录/推游标。
    ///
    /// 验证口径与 checkpoint writer 完全一致：统一走
    /// [`crate::upload::fetch_verified_asset`]（分片上限/分配保护/固定切片/每片
    /// header 上下文/AEAD/摘要/明文头/OID 重算），这里只把事件里的完整声明传进去。
    ///
    /// 返回值额外带上**已验证的资产数据字节**：pull 消费 ASSET 事件时要把它们落进
    /// 本地内容寻址表（否则"另一台设备同步后取不到资产字节"——ASSET 事件是更新日志
    /// 里唯一携带资产数据的载体）。
    fn download_verified_asset(
        &mut self,
        object_id: &ObjectId,
        commit: &ObjectCommit,
    ) -> Result<(crate::upload::VerifiedAssetFacts, Vec<u8>), SyncError> {
        crate::upload::fetch_verified_asset_data(
            &mut *self.client,
            &self.root,
            self.vault_id,
            MAX_ASSET_RAW_BYTES as u64,
            &crate::upload::AssetExpectation {
                object_id,
                object_type: commit.object_type,
                generation: commit.generation,
                message_id: Some(commit.message_id),
                sender_seq: commit.sender_seq,
                part_count: commit.part_count,
                total_len: commit.total_len,
                object_digest: &commit.object_digest,
                key_epoch: commit.key_epoch,
            },
        )
    }

    /// 把已验证的资产明文写进本地内容寻址表（`assets`）。
    ///
    /// 幂等语义由 storage 保证：同 hash + 同字节 → `Ok(())`；同 hash 不同字节
    /// （本地已损坏或哈希碰撞）→ `Corrupt`，这里 fail-closed（不写记录、不推游标）。
    fn store_verified_asset(
        &self,
        verified: &crate::upload::VerifiedAssetFacts,
        data: &[u8],
    ) -> Result<(), SyncError> {
        menghunote_storage::insert_asset(
            self.vault,
            &verified.real_sha256,
            verified.byte_len,
            Some(&verified.media_type),
            data,
        )?;
        Ok(())
    }

    /// 持久写入一条 skipped-asset 记录；同一 seq 重放必须幂等，内容冲突 fail-closed。
    ///
    /// 冲突分两类：HMAC/结构/认证失败是 `LocalCorrupt`（本地记录被篡改或旧修订）；
    /// HMAC 校验通过但同一 seq 的身份/资产事实不同，是 relay 重写已消费历史，
    /// 返回 `Paused(RelayHistoryRewritten)`，**不覆盖**已有记录、不推进游标。
    ///
    /// **先做来源认证，再落资产字节**（[`Self::download_verified_asset`]）：只有真实下载
    /// 并验证通过的 ASSET 才允许落记录，记录里的资产事实全部来自本机验证结果。验证通过后
    /// 把明文写进本地内容寻址表（[`Self::store_verified_asset`]，幂等），这样**已存在的
    /// 设备**同步后也能用 `asset_bytes` 取到资产；结构校验/下载/验证/落库任一失败都不写
    /// 记录，调用方（pull）因此不会推进游标。
    ///
    /// 结构校验（不验签，因为 ASSET 信封的 producer 被规范化为 0，事件里拿不到
    /// 生产者公钥）：事件必须是可解码的 `ObjectCommit`，类型 ASSET、object_id /
    /// vault / epoch 与日志事件一致、generation=0、sender_seq=0。
    ///
    /// 返回值为本次**实际下载并验证通过的资产明文字节**（`PullReport::asset_bytes` 口径；
    /// 幂等重放也会真实下载一次，因此同样计入）。
    fn persist_skipped_asset(
        &mut self,
        log_id: &[u8; 16],
        event: &RelayLogEvent,
        cursor: &str,
    ) -> Result<u64, SyncError> {
        let object_id = event
            .object_id
            .ok_or(SyncError::Object("ASSET 日志事件缺少 object_id"))?;
        // 与 UPDATE 路径同口径：事件 epoch 必须**等于**本地根。
        // 旧实现只拒绝"更新"的 relay（`>`），epoch 落后会走到 AEAD 失败并报成
        // `Object`；语义上这属于"本地根/relay 不同代"，应暂停而不是当成对象损坏。
        if event.epoch != self.root.key_epoch {
            return Err(SyncError::Paused(PauseReason::EpochMismatch));
        }
        let commit = ObjectCommit::decode(&event.event)?;
        if commit.vault_id != self.vault_id
            || commit.object_id != object_id
            || commit.key_epoch != event.epoch
            || commit.object_type != ObjectType::Asset
            || commit.generation != 0
            || commit.sender_seq != 0
        {
            return Err(SyncError::Object(
                "ASSET 日志事件的 ObjectCommit 与事件不符",
            ));
        }
        // 入口先验后下载：即便是 wire 层已收紧（`ObjectCommit::validate` 的 ASSET
        // 64MiB 上限），这里也显式挡一次，保证 `commit.total_len` 不会以无界值进入
        // 读回层的容量分配。
        if commit.total_len > MAX_ASSET_RAW_BYTES as u64 {
            return Err(SyncError::Object("ASSET 总长超出 64MiB 上限"));
        }
        // 评审 0003 P0-B 残余：HMAC 只能证明本机记录过 relay 的声明；写记录之前必须
        // 真正取回并验证资产。失败直接 `?`，不写记录、不写 inbox、不推游标。
        let (verified, data) = self.download_verified_asset(&object_id, &commit)?;
        let asset_bytes = verified.byte_len;
        // 资产字节落本地（内容寻址、幂等）：只有这一步成功，别的设备同步后
        // `asset_bytes` 才真的取得到；记录只表示"本机消费了这条 ASSET"。
        self.store_verified_asset(&verified, &data)?;
        // 本机刚完成"下载 + 全部分片校验 + 落盘"，relay 上该对象必然完整：直接落
        // durable 已上传标记，避免下一轮普通同步再为这个对象发探测/重传请求。
        asset_sync::mark_uploaded(
            self.vault,
            &asset_sync::AssetUploadedFact {
                object_id,
                part_count: commit.part_count,
                total_plain_len: commit.total_len,
                object_digest: commit.object_digest,
                key_epoch: commit.key_epoch,
            },
        )?;
        let record = SkippedAssetRecord {
            log_id: *log_id,
            relay_seq: event.relay_seq,
            object_id,
            kind: SKIPPED_ASSET_KIND_ASSET,
            epoch: event.epoch,
            real_sha256: verified.real_sha256,
            byte_len: verified.byte_len,
            media_type: verified.media_type,
            object_digest: verified.object_digest,
            commit: event.event.clone(),
            cursor: cursor.to_owned(),
        };
        let mac_key = skipped_asset_mac_key(&self.identity.signing_key, &self.vault_id)?;
        let value = record.encode(&mac_key)?;
        let key = skipped_asset_meta_key(log_id, event.relay_seq);
        // 先 insert-do-nothing，再回读比较：并发/重放时先写者赢，后写者只做一致性校验，
        // 绝不覆盖成不同事实。
        let inserted = self.vault.exec(
            "INSERT INTO meta(key, value) VALUES(?1, ?2) ON CONFLICT(key) DO NOTHING",
            &[
                menghunote_storage::Value::Text(key.clone()),
                menghunote_storage::Value::Blob(value),
            ],
        )?;
        if inserted == 1 {
            return Ok(asset_bytes);
        }
        let existing = self
            .vault
            .get_meta(&key)?
            .ok_or(SyncError::LocalCorrupt("skipped-asset 记录缺失"))?;
        let existing = SkippedAssetRecord::decode(&existing, &mac_key)?;
        // cursor 允许因页边界不同而变化（重试可能拿到不同 next_cursor），但身份与已验证
        // 资产字段必须完全一致。HMAC 已通过，说明本地记录没有被篡改；此时同一 seq
        // 解析出不同事实，只可能是 relay 重写了历史（或对同一 OID 返回了不同密文）——
        // 映射成暂停而不是 `LocalCorrupt`，避免让本地库永久表现为"本地损坏"。
        if existing.log_id != record.log_id
            || existing.relay_seq != record.relay_seq
            || existing.object_id != record.object_id
            || existing.kind != record.kind
            || existing.epoch != record.epoch
            || existing.real_sha256 != record.real_sha256
            || existing.byte_len != record.byte_len
            || existing.media_type != record.media_type
            || existing.object_digest != record.object_digest
            || existing.commit != record.commit
        {
            return Err(SyncError::Paused(PauseReason::RelayHistoryRewritten));
        }
        Ok(asset_bytes)
    }

    fn read_cursor(&self) -> Result<Option<String>, SyncError> {
        match self.vault.get_meta(RELAY_CURSOR_META)? {
            Some(bytes) if bytes.is_empty() => Ok(None),
            Some(bytes) => String::from_utf8(bytes)
                .map(Some)
                .map_err(|_| SyncError::LocalCorrupt("relay 游标不是 UTF-8")),
            None => Ok(None),
        }
    }

    fn write_cursor(&self, cursor: &str) -> Result<(), SyncError> {
        self.vault.set_meta(RELAY_CURSOR_META, cursor.as_bytes())?;
        Ok(())
    }
}

/// `Instant::elapsed` 的毫秒截断（`Duration` 转 `u64`，超出范围饱和到 `u64::MAX`）。
fn elapsed_millis(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// 一条待集成的 inbox 行 + 取出后缓存的 doc id / 负载大小（供公平取批复用）。
struct PendingInboxRow {
    row: SyncInboxRow,
    doc_id: String,
    size: usize,
}

/// 单行集成的结果（deferred 时保持 durable verified，不推进任何水位）。
enum InboxRowOutcome {
    /// 已标记 integrated；`changed` = 该文档 head 真的前进了。
    Integrated { changed: bool },
    /// 依赖未就绪（未知文档 / base_vv 未覆盖）。
    Deferred,
}

/// 结构批选取：结构文档优先，最多 `batch_objects` / 约 `batch_bytes` / `batch_time`。
fn select_structure_batch(pending: &[PendingInboxRow], config: &IntegrateConfig) -> Vec<usize> {
    let started = std::time::Instant::now();
    let mut selected = Vec::new();
    let mut bytes = 0usize;
    for (index, item) in pending.iter().enumerate() {
        if item.doc_id != STRUCTURE_DOC_ID {
            continue;
        }
        if !selected.is_empty() {
            if selected.len() >= config.batch_objects {
                break;
            }
            if bytes.saturating_add(item.size) > config.batch_bytes {
                break;
            }
            if started.elapsed() >= config.batch_time {
                break;
            }
        }
        bytes = bytes.saturating_add(item.size);
        selected.push(index);
    }
    selected
}

/// 正文批选取：按 doc 轮转（`rotation` 决定起点），避免只刷最早的一篇。
fn select_body_batch(
    pending: &[PendingInboxRow],
    config: &IntegrateConfig,
    rotation: usize,
) -> Vec<usize> {
    let started = std::time::Instant::now();
    let mut selected = Vec::new();
    let mut bytes = 0usize;
    // (doc_id, 该 doc 的候选下标)。doc 数量很少（参与方按笔记数），线性查找足够。
    let mut docs: Vec<(&str, Vec<usize>)> = Vec::new();
    for (index, item) in pending.iter().enumerate() {
        if item.doc_id == STRUCTURE_DOC_ID {
            continue;
        }
        match docs
            .iter_mut()
            .find(|(doc_id, _)| *doc_id == item.doc_id.as_str())
        {
            Some((_, indices)) => indices.push(index),
            None => docs.push((item.doc_id.as_str(), vec![index])),
        }
    }
    if docs.is_empty() {
        return selected;
    }
    let offset = rotation % docs.len();
    let mut cursors = vec![0usize; docs.len()];
    'outer: loop {
        let mut took_any = false;
        for step in 0..docs.len() {
            let doc = (offset + step) % docs.len();
            let Some(&index) = docs[doc].1.get(cursors[doc]) else {
                continue;
            };
            cursors[doc] += 1;
            if !selected.is_empty() {
                if selected.len() >= config.batch_objects {
                    break 'outer;
                }
                if bytes.saturating_add(pending[index].size) > config.batch_bytes {
                    break 'outer;
                }
                if started.elapsed() >= config.batch_time {
                    break 'outer;
                }
            }
            bytes = bytes.saturating_add(pending[index].size);
            selected.push(index);
            took_any = true;
        }
        if !took_any {
            break;
        }
    }
    selected
}

/// 正文聚合批选取（[`IntegrateConfig::aggregate_body`]）：按 doc 轮转，**每篇文档**的
/// 候选行尽量全部进同一批。
///
/// 与 [`select_body_batch`] 的语义差异（Phase 1A-2 有意为之）：
/// - 旧路径把 `batch_objects` 解释为**行数**（32 行一批），一页 256 行同一文档会被切成
///   8 批 = 8 次 `load_body_doc` 全量重放；
/// - 聚合路径把 `batch_objects` 解释为**每批参与的文档数**（默认 32 篇），单篇文档的
///   行数只受 `batch_bytes` / `batch_time` 与扫描窗口约束。目标：单文档一页 256 行
///   一次 load；多文档仍按 `rotation` 轮转起点，避免饿死靠后的文档。
/// - 预算仍是**软**的：单行超大时允许它单独成批；遇到字节/时间上限就停这一批，
///   已选中的行全部交付，剩余行留在 durable verified 等下一批。
///
/// Phase 1A 缺陷收口起，选批前先按 doc 读一次 O(1) 的 `head_vv` 做与组处理**同一套**
/// 预检（[`body_group_readiness`]，不 `load_body_doc`）：
/// - 纯 deferred 组不进 selected，也不消耗 `batch_bytes` —— 否则两个大 deferred 文档
///   会把后面的就绪文档挤出本批，连续零进展后整轮提前停止（饿死）；
/// - 全部已覆盖组进 selected 以便标记，但按 0 字节计，不占就绪正文预算；
/// - 未知文档（没有 `loro_docs` 行）无法预检，按 deferred 处理（保持 durable verified，
///   等结构投影/依赖到达）。
fn select_body_batch_aggregated(
    vault: &Vault,
    pending: &[PendingInboxRow],
    config: &IntegrateConfig,
    rotation: usize,
) -> Result<Vec<usize>, SyncError> {
    let started = std::time::Instant::now();
    let mut selected = Vec::new();
    let mut bytes = 0usize;
    // 需要 import 的行数（软预算只看这些行，已覆盖的标记行是 0 字节）。
    let mut ready_rows = 0usize;
    // (doc_id, 该 doc 的候选下标)。doc 数量很少（参与方按笔记数），线性查找足够。
    let mut docs: Vec<(&str, Vec<usize>)> = Vec::new();
    for (index, item) in pending.iter().enumerate() {
        if item.doc_id == STRUCTURE_DOC_ID {
            continue;
        }
        match docs
            .iter_mut()
            .find(|(doc_id, _)| *doc_id == item.doc_id.as_str())
        {
            Some((_, indices)) => indices.push(index),
            None => docs.push((item.doc_id.as_str(), vec![index])),
        }
    }
    if docs.is_empty() {
        return Ok(selected);
    }
    let offset = rotation % docs.len();
    let max_docs = config.batch_objects.max(1);
    let mut groups = 0usize;
    'outer: for step in 0..docs.len() {
        if groups >= max_docs {
            break;
        }
        let doc = (offset + step) % docs.len();
        let doc_id = docs[doc].0;
        // 未知文档无法预检：按 deferred 跳过（组处理也不会 load；等结构投影建立文档行）。
        if !storage_view::doc_exists(vault, doc_id)? {
            continue;
        }
        let head_vv = load_head(vault, doc_id)?.version_vector()?;
        // 同一文档可能跨 log（日志轮换）：标记必须在同 log 内一次事务完成，因此按 log
        // 拆子组分别预检；文档级轮转与预算口径不变。
        let mut subgroups: Vec<([u8; 16], Vec<usize>)> = Vec::new();
        for &index in &docs[doc].1 {
            let log_id = pending[index].row.log_id;
            match subgroups.iter_mut().find(|(id, _)| *id == log_id) {
                Some((_, rows)) => rows.push(index),
                None => subgroups.push((log_id, vec![index])),
            }
        }
        let mut took_any = false;
        for (_, indices) in &subgroups {
            match body_group_readiness(pending, indices, &head_vv) {
                // 纯 deferred：不进入 selected、不消耗批次预算。
                BodyGroupReadiness::Deferred => continue,
                // 全部已覆盖：需要标记但不 load，按 0 字节计。
                BodyGroupReadiness::Covered => {
                    selected.extend(indices.iter().copied());
                    took_any = true;
                }
                BodyGroupReadiness::Ready => {
                    for &index in indices {
                        // 软预算：第一条 ready 行永远放行（单行超大时单独成批）。
                        if ready_rows > 0 {
                            if bytes.saturating_add(pending[index].size) > config.batch_bytes {
                                break 'outer;
                            }
                            if started.elapsed() >= config.batch_time {
                                break 'outer;
                            }
                        }
                        bytes = bytes.saturating_add(pending[index].size);
                        ready_rows += 1;
                        selected.push(index);
                        took_any = true;
                    }
                }
            }
        }
        if took_any {
            groups += 1;
        }
    }
    Ok(selected)
}

/// 一组（同一 `doc_id` + `log_id`，按 relay_seq 升序）inbox 行的预检结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BodyGroupReadiness {
    /// 所有行的 `end_vv` 都已被 `head_vv` 覆盖：只需标记，不需要 import。
    Covered,
    /// 至少一行未被覆盖且 `base_vv` 已被 `head_vv` 覆盖：至少有一行要 import。
    Ready,
    /// 未覆盖行的 `base_vv` 全都不满足：纯 deferred，本轮跳过（不重放、不占预算）。
    Deferred,
}

/// 用 `head_vv`（O(1)）对一组行做轻量预检，**不** `load_body_doc`。
///
/// 语义与组内逐行重放后的检查一致：`end_vv` 已被覆盖 = 该 update 应用过；未覆盖行的
/// `base_vv` 全不满足 = 真正缺外部依赖。空 `base_vv/end_vv` 由 [`decode_vv`] 归一化成
/// 空版本向量（与逐行路径一致），空 `head_vv` 由 `HeadState::version_vector` 归一化。
///
/// 解码失败时返回 [`BodyGroupReadiness::Ready`]：把错误留给真正的组处理路径原样报出，
/// 预检不能把损坏数据误判成 deferred 而让它永远滞留。
fn body_group_readiness(
    pending: &[PendingInboxRow],
    indices: &[usize],
    head_vv: &VersionVector,
) -> BodyGroupReadiness {
    let mut uncovered = false;
    for &index in indices {
        let Some(payload) = pending[index].row.verified_payload.as_deref() else {
            return BodyGroupReadiness::Ready;
        };
        let Ok(update) = UpdateV1::decode(payload) else {
            return BodyGroupReadiness::Ready;
        };
        let Ok(end_vv) = decode_vv(&update.end_vv, "end_vv 解码失败") else {
            return BodyGroupReadiness::Ready;
        };
        if vv_includes(head_vv, &end_vv) {
            continue;
        }
        uncovered = true;
        let Ok(base_vv) = decode_vv(&update.base_vv, "base_vv 解码失败") else {
            return BodyGroupReadiness::Ready;
        };
        if vv_includes(head_vv, &base_vv) {
            return BodyGroupReadiness::Ready;
        }
    }
    if uncovered {
        BodyGroupReadiness::Deferred
    } else {
        BodyGroupReadiness::Covered
    }
}

/// 组内各行的 relay_seq（批量标记用；顺序不影响标记语义）。
fn relay_seqs_of(pending: &[PendingInboxRow], indices: &[usize]) -> Vec<u64> {
    indices
        .iter()
        .map(|&index| pending[index].row.relay_seq)
        .collect()
}

/// 解码 `UpdateV1.base_vv/end_vv`；空字节等价于空版本向量（seq=1 的规范形式）。
fn decode_vv(bytes: &[u8], error: &'static str) -> Result<VersionVector, SyncError> {
    if bytes.is_empty() {
        return Ok(VersionVector::new());
    }
    VersionVector::decode(bytes).map_err(|_| SyncError::Object(error))
}

/// 空的 `end_vv`（seq=1 时）规范化为空版本向量的编码，避免接收端解码失败。
fn normalize_vv(bytes: &[u8]) -> Vec<u8> {
    if bytes.is_empty() {
        VersionVector::new().encode()
    } else {
        bytes.to_vec()
    }
}

/// 版本向量包含关系：`local` 覆盖 `required` 的全部区间。
fn vv_includes(local: &VersionVector, required: &VersionVector) -> bool {
    // `VersionVector` 是 peer -> 已见最大 counter（不含边界之外）。
    for (peer, required_end) in required.iter() {
        match local.get(peer) {
            Some(local_end) if local_end >= required_end => {}
            _ => return false,
        }
    }
    true
}

fn object_digest_of(parts: &[SyncEncryptedPart]) -> Result<[u8; 32], SyncError> {
    let hashes: Vec<[u8; 32]> = parts.iter().map(|part| part.digest).collect();
    let input = object_digest_input(parts.len() as u32, &hashes)?;
    Ok(Sha256::digest(&input).into())
}

fn object_digest_of_parts(
    parts: &[menghunote_storage::SyncOutboxPart],
) -> Result<[u8; 32], SyncError> {
    let hashes: Vec<[u8; 32]> = parts.iter().map(|part| part.digest).collect();
    let input = object_digest_input(parts.len() as u32, &hashes)?;
    Ok(Sha256::digest(&input).into())
}

/// 把 outbox 里的 UUID 文本转成 16B wire message_id。
fn decode_message_id(text: &str) -> Result<[u8; MESSAGE_ID_LEN], SyncError> {
    let cleaned: String = text.chars().filter(|ch| *ch != '-').collect();
    if cleaned.len() != MESSAGE_ID_LEN * 2 {
        return Err(SyncError::LocalCorrupt("outbox message_id 不是 UUID"));
    }
    crate::ids::parse_hex::<MESSAGE_ID_LEN>(&cleaned)
        .map_err(|_| SyncError::LocalCorrupt("outbox message_id 非法"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_id_accepts_uuid_and_plain_hex() {
        let expected = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ];
        assert_eq!(
            decode_message_id("00112233-4455-6677-8899-aabbccddeeff").unwrap(),
            expected
        );
        assert_eq!(
            decode_message_id("00112233445566778899aabbccddeeff").unwrap(),
            expected
        );
        assert!(decode_message_id("msg-1").is_err());
        assert!(decode_message_id("00112233-4455-6677-8899-aabbccddee").is_err());
    }

    #[test]
    fn vv_containment() {
        let mut local = VersionVector::new();
        local.insert(1, 3);
        let mut required = VersionVector::new();
        required.insert(1, 2);
        assert!(vv_includes(&local, &required));
        let mut ahead = VersionVector::new();
        ahead.insert(1, 4);
        assert!(!vv_includes(&local, &ahead));
        let mut other_peer = VersionVector::new();
        other_peer.insert(2, 1);
        assert!(!vv_includes(&local, &other_peer));
        // 空 required 永远满足
        assert!(vv_includes(&local, &VersionVector::new()));
    }

    #[test]
    fn skipped_asset_record_rejects_old_revision_bytes() {
        let mac_key = crate::keys::Secret32::from_array([0x42; 32]);
        let object_id = [0x11; 32];
        let commit = ObjectCommit {
            vault_id: [0x01; 16],
            object_id,
            message_id: [0x22; 16],
            object_type: ObjectType::Asset,
            generation: 0,
            key_epoch: 1,
            sender_seq: 0,
            part_count: 1,
            total_len: 67,
            object_digest: [0x33; 32],
        };
        let record = SkippedAssetRecord {
            log_id: [0x44; 16],
            relay_seq: 1,
            object_id,
            kind: SKIPPED_ASSET_KIND_ASSET,
            epoch: 1,
            real_sha256: [0x55; 32],
            byte_len: 25,
            media_type: "text/plain".to_owned(),
            object_digest: [0x33; 32],
            commit: commit.encode().expect("commit"),
            cursor: "c1".to_owned(),
        };
        let bytes = record.encode(&mac_key).expect("encode");
        SkippedAssetRecord::decode(&bytes, &mac_key).expect("当前修订必须可解码");

        // 旧 v2 记录（magic 旧、HMAC 自洽）必须被独立修订号拒绝，不得改号升级。
        let mut old = bytes;
        let magic_len = SKIPPED_ASSET_MAGIC.len();
        old[..magic_len].copy_from_slice(b"MN/skipped-asset/2\0");
        let body_len = old.len() - 32;
        let tag = skipped_asset_mac(&old[..body_len], &mac_key).expect("mac");
        old[body_len..].copy_from_slice(&tag);
        let error = SkippedAssetRecord::decode(&old, &mac_key).expect_err("旧修订必须拒绝");
        assert!(format!("{error}").contains("版本非法"), "{error}");
    }
}

`````



@@SECTION code/068 `crates/storage/src/baseline_apply.rs`


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



@@SECTION code/069 `crates/storage/src/journal.rs`


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



@@SECTION code/070 `crates/storage/src/error.rs`


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



@@SECTION code/071 `crates/storage/src/remote_known.rs`


`````rust
//! 每篇文档「远端已知 VV」的本地元数据（**非破坏性本地 meta**，不新增表/列、不参与 wire）。
//!
//! ## 为什么需要它
//!
//! `baseline_apply` 的远端 tombstone 需要回答：**这篇本地文档里有没有远端从未见过的 op？**
//! 有就必须保留（可能是 local-only 内容或尚未同步的本地编辑）；全都见过则可以按远端
//! tombstone 标记删除。仅凭 `loro_docs.head_seq>0` 或 `sync_outbox` 为空都不够：
//!
//! - 已有设备普通 pull/integrate 同步过的文档，`head_seq>0`、outbox 为空，
//!   但它的内容确实全部来自远端；
//! - 启用同步前就存在的 local-only 文档同样 `head_seq>0`（且没有 outbox 行），
//!   却从未离开本机。
//!
//! 因此这里持久化一个**只能单调扩大**的「远端已确证见过的 oplog 前缀」：
//! `meta` 键 `sync.remote_known_vv.<doc_id>`，值为 `VersionVector::encode()`。
//!
//! ## 谁会扩大它
//!
//! 1. 应用已验证基线快照：快照自带的 VV 就是远端在该 checkpoint 上已知的 ops
//!    （`baseline_apply` 在同一事务里写入）；
//! 2. 集成远端 update：`after_vv - before_vv` 就是这条远端消息实际带来的 ops
//!    （`store::append_updates_inner` 在同一 A 事务里写入）。
//!
//! 本地写入**不会**扩大它——即使已经 ACK 上传也不扩大。这是刻意的保守取舍：
//! tombstone 判定只会更倾向于 `kept_local_edit`，绝不会把本地未确证同步过的内容删掉。
//!
//! ## 单调性怎么强制（P2-5）
//!
//! 「只读旧值、在事务外算并集、再无条件覆盖」有两个写者互相覆盖的窗口（普通 pull 的
//! A 事务与 baseline apply 批次都可能写同一个键）。因此所有写入都改成**值 CAS**：
//!
//! - 生成语句时带上读到的旧值 `expected_old`，SQL 是
//!   `INSERT ... ON CONFLICT(key) DO UPDATE SET value=excluded.value WHERE meta.value=?old`；
//!   旧值不存在时用 `INSERT ... SELECT ... WHERE NOT EXISTS`；
//! - 语句带 `AffectedGuard::exactly(index, 1)`：CAS 不命中就是 0 行 → 整笔事务回滚，
//!   调用方拿到明确的冲突错误（重读后重试即收敛到并集），**绝不会写入更小的值**。
//!
//! ## 判定
//!
//! `local_vv.sub_iter(remote_known).next().is_none()` 表示本地没有任何远端未知的 op，
//! 才允许按 tombstone 删除。没有这条记录时一律视为「无证据」，保守保留。

use loro::VersionVector;
use rusqlite::types::Value;

use crate::actor::Statement;
use crate::error::StorageError;
use crate::Vault;

/// 每篇文档「远端已知 VV」的 meta 键前缀。
pub const META_REMOTE_KNOWN_VV_PREFIX: &str = "sync.remote_known_vv.";

/// 某篇文档的 meta 键（doc_id 由仓库生成，形如 `note-<peer>-<seq>`，不含前缀歧义）。
pub(crate) fn meta_key(doc_id: &str) -> String {
    format!("{META_REMOTE_KNOWN_VV_PREFIX}{doc_id}")
}

/// 读某篇文档的远端已知 VV；没有记录返回 `None`（= 无证据）。
pub(crate) fn load(vault: &Vault, doc_id: &str) -> Result<Option<VersionVector>, StorageError> {
    let key = meta_key(doc_id);
    let rows = vault.query(
        "SELECT value FROM meta WHERE key=?1",
        &[Value::Text(key.clone())],
    )?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    match row.first() {
        Some(Value::Blob(bytes)) => Ok(Some(decode(bytes)?)),
        // meta 值的统一口径（P2-6）：只认 BLOB；NULL/INTEGER/TEXT 都视为库被改坏。
        other => Err(StorageError::Corrupt {
            context: format!("{key} 读回异常：{other:?}"),
        }),
    }
}

/// 解码 VV；空字节等价于空版本向量（与 `HeadState::version_vector` 同口径）。
pub(crate) fn decode(bytes: &[u8]) -> Result<VersionVector, StorageError> {
    if bytes.is_empty() {
        return Ok(VersionVector::default());
    }
    VersionVector::decode(bytes).map_err(StorageError::from)
}

/// 逐 peer 取 max（单调并集）。
pub(crate) fn union(existing: &VersionVector, added: &VersionVector) -> VersionVector {
    let mut merged = existing.clone();
    for (peer, &counter) in added.iter() {
        let counter = counter.max(0);
        if counter <= 0 {
            continue;
        }
        let current = merged.get(peer).copied().unwrap_or(0).max(0);
        if counter > current {
            merged.insert(*peer, counter);
        }
    }
    merged
}

/// `after` 中不在 `before` 里的 ops（远端消息真正带来的那一段）。
///
/// 结果 VV 对某个 peer 的值是 `after[peer]`，即「远端已知 [0, after)」，与 `before`
/// 里已有的前缀合并是单调的。
pub(crate) fn added_since(after: &VersionVector, before: &VersionVector) -> VersionVector {
    let mut added = VersionVector::new();
    for span in after.sub_iter(before) {
        added.insert(span.peer, span.counter.end);
    }
    added
}

/// 生成一条**值 CAS** upsert 语句（调用方负责把它放进 A 事务，并加
/// `AffectedGuard::exactly(语句下标, 1)`）。
///
/// - `expected = Some(old)`：`INSERT ... ON CONFLICT(key) DO UPDATE ... WHERE meta.value=?old`。
///   只有库中值仍是读到的旧值时才写入；被并发写者改过就是 0 行 → 守卫失败 → 整笔回滚；
/// - `expected = None`：`INSERT ... SELECT ... WHERE NOT EXISTS`。只有键仍不存在时才插入；
///   并发插入过就是 0 行 → 同样回滚。
///
/// 两种写法都不会用旧并集覆盖（更小的）并发结果，因此值只能单调扩大。
pub(crate) fn cas_statement(
    doc_id: &str,
    expected: Option<&VersionVector>,
    merged: &VersionVector,
) -> Statement {
    assert_monotone(expected, merged);
    let key = meta_key(doc_id);
    let encoded = merged.encode();
    match expected {
        Some(old) => Statement::new(
            "INSERT INTO meta(key,value) VALUES(?1,?2) \
             ON CONFLICT(key) DO UPDATE SET value=excluded.value \
             WHERE meta.value=?3",
            vec![
                Value::Text(key),
                Value::Blob(encoded),
                Value::Blob(old.encode()),
            ],
        ),
        None => Statement::new(
            "INSERT INTO meta(key,value) SELECT ?1,?2 \
             WHERE NOT EXISTS(SELECT 1 FROM meta WHERE key=?1)",
            vec![Value::Text(key), Value::Blob(encoded)],
        ),
    }
}

/// 调试断言：CAS 的目标值必须包含读到的旧值（`old ⊆ merged`）。
///
/// 生产路径由 [`union`] 构造 merged，这里只是把「只能扩大」的契约钉在生成语句处；
/// 一旦将来有人改成直接写入更小的值，debug/test 构建会立刻失败。
fn assert_monotone(expected: Option<&VersionVector>, merged: &VersionVector) {
    if let Some(old) = expected {
        debug_assert!(
            old.sub_iter(merged).next().is_none(),
            "remote known VV 只能单调扩大：旧值必须是 merged 的子集"
        );
    }
}

/// 在 A 事务**之外**读旧值并返回合并后的 **CAS** 语句；`added` 为空时返回 `None`。
///
/// 旧值只作为 CAS 的期望值；若事务执行时值已被并发写者扩大，语句影响 0 行、守卫失败，
/// 调用方必须重读（重新调用本函数）后重试。这里**不会**用读到的旧并集覆盖库里的新值。
pub(crate) fn merge_statement(
    vault: &Vault,
    doc_id: &str,
    added: &VersionVector,
) -> Result<Option<Statement>, StorageError> {
    if added.is_empty() {
        return Ok(None);
    }
    let existing = load(vault, doc_id)?;
    let merged = match &existing {
        Some(existing) => union(existing, added),
        None => added.clone(),
    };
    Ok(Some(cas_statement(doc_id, existing.as_ref(), &merged)))
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::mpsc;
    use std::sync::{Arc, Barrier};
    use std::thread;

    use super::*;
    use crate::actor::AffectedGuard;

    /// 临时 vault：目录建在 `target/tmp` 下（AGENTS §6：不用 `%TEMP%`）。
    ///
    /// lib 单元测试拿不到 `CARGO_TARGET_TMPDIR`（只有集成测试有），所以从
    /// `CARGO_MANIFEST_DIR` 拼出同一个位置。
    fn fresh_vault(prefix: &str) -> (tempfile::TempDir, Vault) {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/tmp")
            .join(format!("remote-known-{prefix}"));
        std::fs::create_dir_all(&root).expect("创建 target/tmp 下的测试根目录");
        let dir = tempfile::Builder::new()
            .prefix("vault-")
            .tempdir_in(&root)
            .expect("创建临时目录");
        let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
        (dir, vault)
    }

    fn vv(peer: u64, counter: i32) -> VersionVector {
        let mut vv = VersionVector::new();
        vv.insert(peer, counter);
        vv
    }

    fn read(vault: &Vault, doc_id: &str) -> Option<VersionVector> {
        load(vault, doc_id).expect("读取远端已知 VV")
    }

    fn exec_cas(
        vault: &Vault,
        doc_id: &str,
        expected: Option<&VersionVector>,
        merged: &VersionVector,
    ) -> Result<(), StorageError> {
        vault
            .exec_transaction(
                &[cas_statement(doc_id, expected, merged)],
                &[AffectedGuard::exactly(0, 1)],
            )
            .map(|_| ())
    }

    /// 值 CAS 的三种状态：不存在时插入 → 陈旧期望被拒绝且不缩小 → 读到新值后并集成功。
    #[test]
    fn cas_statement_rejects_stale_expected_and_never_shrinks() {
        let (_dir, vault) = fresh_vault("stale");
        let doc_id = "doc-cas";
        let a = vv(0xA1, 2);
        let b = vv(0xB2, 3);

        // 读时不存在：直接插入。
        exec_cas(&vault, doc_id, None, &a).expect("首次插入");
        assert_eq!(read(&vault, doc_id), Some(a.clone()));

        // 另一个写者仍以为「不存在」，拿着旧的 None 来写 B：必须被 CAS 拒绝。
        let error = exec_cas(&vault, doc_id, None, &b).expect_err("陈旧期望必须失败");
        assert!(
            matches!(error, StorageError::GuardFailed { .. }),
            "CAS 不命中应当是 GuardFailed（调用方映射成明确冲突），实际 {error:?}"
        );
        assert_eq!(
            read(&vault, doc_id),
            Some(a.clone()),
            "CAS 失败绝不能把已有的 A 覆盖成只有 B"
        );

        // 重读后按并集重试：成功且值单调扩大。
        let current = read(&vault, doc_id).expect("已存在");
        let merged = union(&current, &b);
        exec_cas(&vault, doc_id, Some(&current), &merged).expect("并集重试");

        // 再拿一个更旧、且值更小的期望来写：必须继续被拒绝，值不得缩小。
        let stale = vv(0xA1, 1);
        let smaller = vv(0xA1, 1);
        let error = exec_cas(&vault, doc_id, Some(&stale), &smaller).expect_err("更小值必须失败");
        assert!(matches!(error, StorageError::GuardFailed { .. }));
        assert_eq!(
            read(&vault, doc_id),
            Some(merged),
            "失败后必须保持并集，绝不能写入更小的值"
        );
    }

    /// 两路并发写者从**同一旧值**出发写不同 `added`：barrier 对齐后同时 CAS。
    ///
    /// 结果只能是：恰好一方首次成功、另一方拿到明确的 `GuardFailed`；失败方重读后按
    /// 并集重试成功。最终值 = 两方新增的并集（不丢证据、不静默变小）。
    #[test]
    fn concurrent_writers_from_same_old_converge_to_union() {
        let (_dir, vault) = fresh_vault("concurrent");
        let doc_id = "doc-concurrent";
        let base = vv(0x10, 1);
        exec_cas(&vault, doc_id, None, &base).expect("初始值");

        let barrier = Arc::new(Barrier::new(2));
        let (tx, rx) = mpsc::channel();
        let mut handles = Vec::new();
        for peer in [0xA1_u64, 0xB2_u64] {
            let vault = vault.clone();
            let barrier = Arc::clone(&barrier);
            let tx = tx.clone();
            handles.push(thread::spawn(move || {
                let added = vv(peer, 5);
                // 两方都在 barrier 之前读到同一个旧值。
                let old = load(&vault, doc_id).expect("读旧值").expect("已存在");
                let merged = union(&old, &added);
                let statement = cas_statement(doc_id, Some(&old), &merged);
                barrier.wait();
                let first = vault.exec_transaction(&[statement], &[AffectedGuard::exactly(0, 1)]);
                // 失败方（陈旧旧值）重读后重试一次：必须成功并收敛到并集。
                let (first_failed, retried) = match first {
                    Ok(_) => (false, true),
                    Err(StorageError::GuardFailed { .. }) => {
                        let current = load(&vault, doc_id).expect("重读").expect("已存在");
                        let merged = union(&current, &added);
                        (
                            true,
                            exec_cas(&vault, doc_id, Some(&current), &merged).is_ok(),
                        )
                    }
                    Err(other) => panic!("并发 CAS 只允许守卫失败，实际 {other:?}"),
                };
                tx.send((peer, first_failed, retried)).expect("回传结果");
            }));
        }
        drop(tx);
        let results: Vec<(u64, bool, bool)> = rx.iter().take(2).collect();
        for handle in handles {
            handle.join().expect("写者线程不应 panic");
        }
        assert_eq!(results.len(), 2, "两路写者都必须有明确结果");
        assert_eq!(
            results.iter().filter(|(_, failed, _)| *failed).count(),
            1,
            "同一旧值下只允许一方首次成功，另一方必须拿到明确的 CAS 冲突"
        );
        for (peer, _, retried) in &results {
            assert!(*retried, "peer {peer:#x} 重读重试后必须成功");
        }

        let final_vv = read(&vault, doc_id).expect("最终值");
        let expected = union(&union(&base, &vv(0xA1, 5)), &vv(0xB2, 5));
        assert_eq!(final_vv, expected, "最终值必须包含两路 added 的并集");
        assert_eq!(final_vv.get(&0x10).copied(), Some(1), "旧前缀不得丢失");
    }
}

`````



@@SECTION code/072 `crates/storage/src/store.rs`


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



@@SECTION code/073 `crates/storage/src/sync_bridge.rs`


`````rust
//! S7 同步桥：出站意图 → 加密 parts → 推送/ACK；入站 verified inbox → 本地 seq → integrate。
//!
//! 本模块只碰 SQLite；不做 Argon、AEAD、HTTP、Loro 解码。ADR 0006 §5 的
//! 关键约束：
//!
//! - A 事务只写「出站意图」，worker 在事务外加密后 CAS 落 parts；
//! - 同一 message/epoch 的既有 parts 必须与再次提交的字节完全一致，否则冲突；
//! - ACK 用 `(message_id, epoch, object_digest)` CAS，旧 ACK 不能删除新行；
//! - 入站对象先 durable 写 inbox（依赖未齐也保存），再由 sync 层决定何时可 integrate；
//! - `allocate_local_seq_for_import` 只返回 `head_seq+1`，**绝不把 sender_seq 写进 loro_updates.seq`。

use std::collections::BTreeMap;

use rusqlite::types::Value;

use crate::actor::{AffectedGuard, Statement};
use crate::error::StorageError;
use crate::store;
use crate::Vault;

// 同步 `meta` 键与「连续前缀」的低层实现放在 `sync_meta`（直接跑在 `Connection` 上，
// 这样批量 inbox 的插入与前缀推进能共用一个事务）；这里保持原有的公开路径。
use crate::sync_meta::{meta_bytes_of, skipped_received_key, ConsumedPrefix};
pub use crate::sync_meta::{
    META_SYNC_CONFIRMED_HIGHWATER, META_SYNC_INTEGRATED_SEQ, META_SYNC_LOG_ID,
    META_SYNC_RECEIVED_SEQ, META_SYNC_SKIPPED_RECEIVED_PREFIX,
};

/// 出站意图（A 事务内与 update 一起落库）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncIntent {
    /// 稳定 message id（UUID 文本；parts 表以 UTF-8 字节存同一值）。
    pub message_id: String,
    /// 逻辑 batch，通常与 `UpdateRecord.batch_id` 相同。
    pub batch_id: String,
    /// 加密 epoch。
    pub key_epoch: u64,
    /// 对象名（32B；asset 为 HMAC 对象名，其他为随机 32B）。
    pub object_id: [u8; 32],
}

/// 本地写入的**同步上下文**：本机 peer + 当前加密 epoch。
///
/// ADR 0009 切片 2b：真实本地写入（正文 session flush、结构提交）在 A 事务里
/// 顺手写 `sync_outbox` 意图。这里只放"派生意图所需的常量"：
///
/// - `peer_id`：来自持久设备身份（`LocalIdentityV1::loro_peer_id()`），**不是**常量 1；
/// - `key_epoch`：Phase 1 恒为 `1`（ADR 0006 genesis 初始代），**禁止**写成 0
///   （0 在 `validate_intent` 里就是非法值，会让整笔 A 事务失败）。
///
/// `None` 表示不产生意图（默认路径 / remote 导入路径），行为与切片 2a 之前逐字节一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SyncContext {
    /// 本机 Loro peer id（`0` 非法：Loro 不接受 0 peer）。
    pub peer_id: u64,
    /// 当前加密 epoch（Phase 1 = 1；必须 > 0）。
    pub key_epoch: u64,
}

impl SyncContext {
    /// 构造上下文（调用方保证两个值都 > 0；不做隐式回退）。
    #[must_use]
    pub const fn new(peer_id: u64, key_epoch: u64) -> Self {
        Self { peer_id, key_epoch }
    }

    /// 校验：peer / epoch 都必须 > 0，否则明确报错（不静默替换成常量）。
    pub fn validate(self) -> Result<(), StorageError> {
        if self.peer_id == 0 {
            return Err(StorageError::Corrupt {
                context: "SyncContext.peer_id 必须 >0（0 不是合法 Loro peer）".to_owned(),
            });
        }
        if self.key_epoch == 0 {
            return Err(StorageError::Corrupt {
                context: "SyncContext.key_epoch 必须 >0（Phase 1 = 1，禁止常量 0）".to_owned(),
            });
        }
        Ok(())
    }
}

/// 本地意图 ID 的确定性派生域（版本化；改算法必须换常量）。
///
/// 三个派生都从这里起头：message id / object id 必须**纯函数**地由
/// `(doc_id, seq, peer, epoch)` 决定，重试才不会产生新 ID（ADR 0009）。
pub const SYNC_INTENT_ID_DOMAIN: &[u8] = b"MN/dev-sync-intent/v1\0";

/// 确定性 message id（32 位小写 hex = 16B wire id）。
///
/// **为什么不是可读串**：`crates/sync` 的 `engine::decode_message_id` 只接受
/// 16 字节的 hax/UUID 文本（32 个 hex 字符，允许带 `-`），可读的
/// `doc#seq@epoch` 会让 engine 直接报 `LocalCorrupt`。所以这里取
/// `sha256(domain‖"message"‖doc_id‖NUL‖seq_be‖peer_be‖epoch_be)` 的前 16B。
///
/// 纯函数语义：同一 `(doc_id, seq, peer_id, key_epoch)` 永远得到同一个 id；
/// 不同 seq / epoch 必然不同（hash 输入不同）。
#[must_use]
pub fn intent_message_id(doc_id: &str, seq: u64, ctx: SyncContext) -> String {
    let digest = intent_digest(
        b"message\0",
        &[
            doc_id.as_bytes(),
            &[0],
            &seq.to_be_bytes(),
            &ctx.peer_id.to_be_bytes(),
            &ctx.key_epoch.to_be_bytes(),
        ],
    );
    hex_lower(&digest[..16])
}

/// 确定性 object id（32B）。
///
/// `sha256(domain‖"object"‖message_id‖peer_be‖epoch_be)`：对象名与 message 一一对应，
/// 重试/重打包时同样的 message 得到同样的对象名。
#[must_use]
pub fn intent_object_id(message_id: &str, ctx: SyncContext) -> [u8; 32] {
    intent_digest(
        b"object\0",
        &[
            message_id.as_bytes(),
            &ctx.peer_id.to_be_bytes(),
            &ctx.key_epoch.to_be_bytes(),
        ],
    )
}

/// 为一条**本地** update 派生一条出站意图。
///
/// `seq` 必须是这次 A 事务里该 update 真正落库的 `loro_updates.seq`
/// （= `expected_head_seq + 1 + index`），否则意图与数据对不上。
pub fn local_sync_intent(
    doc_id: &str,
    seq: u64,
    batch_id: &str,
    ctx: SyncContext,
) -> Result<SyncIntent, StorageError> {
    ctx.validate()?;
    if doc_id.is_empty() {
        return Err(StorageError::Corrupt {
            context: "sync intent doc_id 不能为空".to_owned(),
        });
    }
    let message_id = intent_message_id(doc_id, seq, ctx);
    validate_intent(&message_id, batch_id, seq, ctx.key_epoch)?;
    Ok(SyncIntent {
        batch_id: batch_id.to_owned(),
        object_id: intent_object_id(&message_id, ctx),
        message_id,
        key_epoch: ctx.key_epoch,
    })
}

/// 同事务写入「一 update 一**可选**意图」：`None` = 该条不产生意图（remote 导入）。
///
/// 一次 flush 的连续前缀可能**混**本地编辑与远端导入（`JournalOrigin`），而
/// remote/import/adopt **绝不能**产生出站意图（否则会把刚收到的数据再推回 relay）。
/// 这里仍是一条 A 事务、按位置一一对应，只是允许某些位置为空。
///
/// `slots` 空 = 本批没有**任何**出站意图；来源标记用 [`store::OriginMarks`]
/// （`AllLocal` / 与 records 等长的 `PerRecord`），**不再用空切片**表示「全部本地」。
pub(crate) fn append_updates_with_intent_slots(
    vault: &Vault,
    doc_id: &str,
    expected_head_seq: u64,
    records: &[store::UpdateRecord],
    slots: &[Option<SyncIntent>],
    origin: store::OriginMarks<'_>,
) -> Result<store::HeadState, StorageError> {
    if !slots.is_empty() && slots.len() != records.len() {
        return Err(StorageError::Corrupt {
            context: format!(
                "append_updates_with_intent_slots: records={} slots={} 必须一对一",
                records.len(),
                slots.len()
            ),
        });
    }
    store::append_updates_with_intent_slots(
        vault,
        doc_id,
        expected_head_seq,
        records,
        slots,
        origin,
    )
}

fn intent_digest(tag: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    use sha2::{Digest as _, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(SYNC_INTENT_ID_DOMAIN);
    hasher.update(tag);
    for part in parts {
        hasher.update(part);
    }
    hasher.finalize().into()
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(HEX[usize::from(byte >> 4)]));
        out.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    out
}

/// outbox 状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SyncOutboxState {
    Pending,
    Encrypted,
    Sent,
    Acked,
}

impl SyncOutboxState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Encrypted => "encrypted",
            Self::Sent => "sent",
            Self::Acked => "acked",
        }
    }

    fn parse(value: &str) -> Result<Self, StorageError> {
        match value {
            "pending" => Ok(Self::Pending),
            "encrypted" => Ok(Self::Encrypted),
            "sent" => Ok(Self::Sent),
            "acked" => Ok(Self::Acked),
            other => Err(StorageError::Corrupt {
                context: format!("未知 sync_outbox.state {other:?}"),
            }),
        }
    }
}

/// 一条待处理出站消息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncOutboxRow {
    pub message_id: String,
    pub batch_id: String,
    pub doc_id: String,
    pub seq: u64,
    pub key_epoch: u64,
    pub object_id: Option<[u8; 32]>,
    pub state: SyncOutboxState,
    pub cipher_digest: Option<[u8; 32]>,
}

/// 一个已加密的 MNE1 part。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncEncryptedPart {
    /// 0-based 连续下标（0..part_count-1）。
    pub part_no: u32,
    /// exact MNE1 envelope bytes（header+ciphertext）。
    pub envelope: Vec<u8>,
    /// 该 part envelope 的 SHA256（sync 计算，storage 不验哈希）。
    pub digest: [u8; 32],
}

/// 从数据库读出的 part。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncOutboxPart {
    pub message_id: String,
    pub epoch: u64,
    pub part_no: u32,
    pub envelope: Vec<u8>,
    pub digest: [u8; 32],
}

/// inbox 状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SyncInboxState {
    Received,
    Verified,
    Integrated,
    Quarantine,
}

impl SyncInboxState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Received => "received",
            Self::Verified => "verified",
            Self::Integrated => "integrated",
            Self::Quarantine => "quarantine",
        }
    }

    fn parse(value: &str) -> Result<Self, StorageError> {
        match value {
            "received" => Ok(Self::Received),
            "verified" => Ok(Self::Verified),
            "integrated" => Ok(Self::Integrated),
            "quarantine" => Ok(Self::Quarantine),
            other => Err(StorageError::Corrupt {
                context: format!("未知 sync_inbox.state {other:?}"),
            }),
        }
    }
}

/// 一条入站对象。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncInboxRow {
    pub log_id: [u8; 16],
    pub relay_seq: u64,
    pub object_id: [u8; 32],
    pub epoch: u64,
    pub state: SyncInboxState,
    pub verified_payload: Option<Vec<u8>>,
}

/// 批量写入的一行**已验签 / 已解密**入站事实（[`store_inbox_verified_batch`] 的输入）。
///
/// 字段与 [`store_inbox_verified`] 的参数一一对应；依赖是否齐仍由 sync 层判断，
/// storage 只负责 durable 落行与推进 `received` 连续前缀。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedInboxRow {
    /// relay 日志 incarnation（同一批必须一致）。
    pub log_id: [u8; 16],
    /// relay 日志序号（必须 > 0；同一批内不得重复）。
    pub relay_seq: u64,
    /// 密文对象名（32B）。
    pub object_id: [u8; 32],
    /// 加密 epoch（必须 > 0）。
    pub epoch: u64,
    /// 已解密并完成载荷校验的 `UpdateV1` 明文（写入 `sync_inbox.verified_payload`）。
    pub verified_payload: Vec<u8>,
}

/// 本地同步水位（`meta` 约定）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SyncCursorState {
    pub received_seq: u64,
    pub integrated_seq: u64,
    pub log_id: Option<[u8; 16]>,
    pub confirmed_highwater: Option<u64>,
}

/// 在 A 事务内追加 update 的同时写出站意图。
///
/// `intents.len()` 必须与 `records.len()` 相同（一 update 一意图）。
pub fn append_updates_with_sync_intents(
    vault: &Vault,
    doc_id: &str,
    expected_head_seq: u64,
    records: &[store::UpdateRecord],
    intents: &[SyncIntent],
) -> Result<store::HeadState, StorageError> {
    if intents.len() != records.len() {
        return Err(StorageError::Corrupt {
            context: format!(
                "append_updates_with_sync_intents: records={} intents={} 必须一对一",
                records.len(),
                intents.len()
            ),
        });
    }
    let slots: Vec<Option<SyncIntent>> = intents.iter().cloned().map(Some).collect();
    store::append_updates_with_intent_slots(
        vault,
        doc_id,
        expected_head_seq,
        records,
        &slots,
        store::OriginMarks::AllLocal,
    )
}

/// 单独记录一条出站意图（调用方已确认 update 落库）。
///
/// 若希望意图与 update 同事务，请用 [`append_updates_with_sync_intents`]。
pub fn enqueue_sync_intent(
    vault: &Vault,
    message_id: &str,
    batch_id: &str,
    doc_id: &str,
    seq: u64,
    key_epoch: u64,
    object_id: &[u8; 32],
) -> Result<(), StorageError> {
    validate_intent(message_id, batch_id, seq, key_epoch)?;
    let affected = vault.exec(
        "INSERT INTO sync_outbox(message_id,batch_id,doc_id,seq,key_epoch,object_id,state,ciphertext) \
         VALUES(?1,?2,?3,?4,?5,?6,'pending',NULL)",
        &[
            Value::Text(message_id.to_owned()),
            Value::Text(batch_id.to_owned()),
            Value::Text(doc_id.to_owned()),
            Value::Integer(i64_of(seq)?),
            Value::Integer(i64_of(key_epoch)?),
            Value::Blob(object_id.to_vec()),
        ],
    )?;
    if affected != 1 {
        return Err(StorageError::SyncConflict {
            context: format!("message {message_id} 已存在"),
        });
    }
    Ok(())
}

/// 读指定状态的 outbox 行。
pub fn outbox_rows(
    vault: &Vault,
    state: SyncOutboxState,
    limit: usize,
) -> Result<Vec<SyncOutboxRow>, StorageError> {
    let rows = vault.query(
        "SELECT message_id,batch_id,doc_id,seq,key_epoch,object_id,state,cipher_digest \
         FROM sync_outbox WHERE state=?1 ORDER BY doc_id,seq,message_id LIMIT ?2",
        &[
            Value::Text(state.as_str().to_owned()),
            Value::Integer(i64::try_from(limit).unwrap_or(i64::MAX)),
        ],
    )?;
    rows.iter().map(row_to_outbox).collect()
}

/// 待加密（state=pending）的意图。
pub fn pending_encryption(vault: &Vault, limit: usize) -> Result<Vec<SyncOutboxRow>, StorageError> {
    outbox_rows(vault, SyncOutboxState::Pending, limit)
}

/// 待发送（state=encrypted）的批次。
pub fn pending_send(vault: &Vault, limit: usize) -> Result<Vec<SyncOutboxRow>, StorageError> {
    outbox_rows(vault, SyncOutboxState::Encrypted, limit)
}

/// 按 message_id 读一行。
pub fn outbox_row(vault: &Vault, message_id: &str) -> Result<Option<SyncOutboxRow>, StorageError> {
    let rows = vault.query(
        "SELECT message_id,batch_id,doc_id,seq,key_epoch,object_id,state,cipher_digest \
         FROM sync_outbox WHERE message_id=?1",
        &[Value::Text(message_id.to_owned())],
    )?;
    rows.first().map(row_to_outbox).transpose()
}

/// 读某 message/epoch 的全部 parts，按 part_no 升序。
pub fn outbox_parts(
    vault: &Vault,
    message_id: &str,
    epoch: u64,
) -> Result<Vec<SyncOutboxPart>, StorageError> {
    let rows = vault.query(
        "SELECT message_id,epoch,part_no,envelope,digest FROM sync_outbox_parts \
         WHERE message_id=?1 AND epoch=?2 ORDER BY part_no",
        &[
            Value::Blob(message_id.as_bytes().to_vec()),
            Value::Integer(i64_of(epoch)?),
        ],
    )?;
    rows.iter()
        .map(|row| {
            Ok(SyncOutboxPart {
                message_id: text_from_blob(row, 0, "sync_outbox_parts.message_id")?,
                epoch: u64_of(row, 1, "sync_outbox_parts.epoch")?,
                part_no: u32_of(row, 2, "sync_outbox_parts.part_no")?,
                envelope: blob_of(row, 3, "sync_outbox_parts.envelope")?,
                digest: array32(row, 4, "sync_outbox_parts.digest")?,
            })
        })
        .collect()
}

/// CAS 写入一批加密 parts，并把 outbox 置为 encrypted。
///
/// 同一 `(message_id, epoch)` 已有 parts 时，必须与新提交的整批字节完全一致；
/// 否则返回 [`StorageError::SyncConflict`]（不会覆盖旧 parts）。
pub fn store_encrypted_parts(
    vault: &Vault,
    message_id: &str,
    epoch: u64,
    object_digest: &[u8; 32],
    parts: &[SyncEncryptedPart],
) -> Result<(), StorageError> {
    if message_id.is_empty() {
        return Err(StorageError::Corrupt {
            context: "message_id 不能为空".to_owned(),
        });
    }
    if parts.is_empty() {
        return Err(StorageError::Corrupt {
            context: format!("message {message_id} 没有 parts"),
        });
    }
    for (index, part) in parts.iter().enumerate() {
        if part.part_no != index as u32 {
            return Err(StorageError::Corrupt {
                context: format!(
                    "message {message_id} parts 必须从 0 连续：第 {index} 项是 {}",
                    part.part_no
                ),
            });
        }
        if part.envelope.is_empty() {
            return Err(StorageError::Corrupt {
                context: format!("message {message_id} 第 {index} 片信封为空"),
            });
        }
    }

    let outbox = outbox_row(vault, message_id)?.ok_or_else(|| StorageError::Corrupt {
        context: format!("sync_outbox 缺少 message {message_id}"),
    })?;
    if outbox.key_epoch != epoch {
        return Err(StorageError::SyncConflict {
            context: format!(
                "message {message_id} epoch={epoch}，outbox 为 {}",
                outbox.key_epoch
            ),
        });
    }
    if outbox.state == SyncOutboxState::Acked {
        return Err(StorageError::SyncConflict {
            context: format!("message {message_id} 已 ACK，拒绝重加密"),
        });
    }

    let existing = outbox_parts(vault, message_id, epoch)?;
    let mut missing = Vec::new();
    if !existing.is_empty() {
        if let Some(old_digest) = outbox.cipher_digest {
            if old_digest != *object_digest {
                return Err(StorageError::SyncConflict {
                    context: format!("message {message_id}/{epoch} 整体 digest 不一致"),
                });
            }
        }
        if existing.len() != parts.len() {
            return Err(StorageError::SyncConflict {
                context: format!(
                    "message {message_id}/{epoch} 已有 {} parts，本次 {} parts",
                    existing.len(),
                    parts.len()
                ),
            });
        }
        for (old, new) in existing.iter().zip(parts) {
            if old.part_no != new.part_no
                || old.envelope != new.envelope
                || old.digest != new.digest
            {
                return Err(StorageError::SyncConflict {
                    context: format!(
                        "message {message_id}/{epoch} 第 {} 片字节不一致",
                        new.part_no
                    ),
                });
            }
        }
    } else {
        missing.extend(parts.iter());
    }

    let mut statements = Vec::with_capacity(1 + missing.len());
    statements.push(Statement::new(
        "UPDATE sync_outbox SET state='encrypted', cipher_digest=?2 \
         WHERE message_id=?1 AND key_epoch=?3 AND state IN('pending','encrypted','sent')",
        vec![
            Value::Text(message_id.to_owned()),
            Value::Blob(object_digest.to_vec()),
            Value::Integer(i64_of(epoch)?),
        ],
    ));
    for part in missing {
        statements.push(Statement::new(
            "INSERT INTO sync_outbox_parts(message_id,epoch,part_no,envelope,digest) \
             VALUES(?1,?2,?3,?4,?5)",
            vec![
                Value::Blob(message_id.as_bytes().to_vec()),
                Value::Integer(i64_of(epoch)?),
                Value::Integer(i64::from(part.part_no)),
                Value::Blob(part.envelope.clone()),
                Value::Blob(part.digest.to_vec()),
            ],
        ));
    }

    match vault.exec_transaction(&statements, &[AffectedGuard::exactly(0, 1)]) {
        Ok(_) => Ok(()),
        Err(StorageError::GuardFailed { .. }) => Err(StorageError::SyncConflict {
            context: format!("message {message_id}/{epoch} 状态已变化，未写入 parts"),
        }),
        Err(error) => Err(error),
    }
}

/// 把已加密批次标记为已发送（CAS digest）。
pub fn mark_sent(
    vault: &Vault,
    message_id: &str,
    epoch: u64,
    object_digest: &[u8; 32],
) -> Result<bool, StorageError> {
    let affected = vault.exec(
        "UPDATE sync_outbox SET state='sent' \
         WHERE message_id=?1 AND key_epoch=?2 AND cipher_digest=?3 \
           AND state IN('encrypted','sent')",
        &[
            Value::Text(message_id.to_owned()),
            Value::Integer(i64_of(epoch)?),
            Value::Blob(object_digest.to_vec()),
        ],
    )?;
    Ok(affected == 1)
}

/// durable ACK：CAS 成功后删除该 outbox 行及其 parts。
///
/// 旧 ACK 的 digest 对不上时只返回 `false`，**不会**删除同 doc/seq 的新 message。
pub fn mark_acked(
    vault: &Vault,
    message_id: &str,
    epoch: u64,
    object_digest: &[u8; 32],
) -> Result<bool, StorageError> {
    let statements = [
        Statement::new(
            "DELETE FROM sync_outbox_parts WHERE message_id=?1 AND epoch=?2",
            vec![
                Value::Blob(message_id.as_bytes().to_vec()),
                Value::Integer(i64_of(epoch)?),
            ],
        ),
        Statement::new(
            "DELETE FROM sync_outbox \
             WHERE message_id=?1 AND key_epoch=?2 AND cipher_digest=?3 \
               AND state IN('encrypted','sent')",
            vec![
                Value::Text(message_id.to_owned()),
                Value::Integer(i64_of(epoch)?),
                Value::Blob(object_digest.to_vec()),
            ],
        ),
    ];
    match vault.exec_transaction(&statements, &[AffectedGuard::exactly(1, 1)]) {
        Ok(_) => Ok(true),
        Err(StorageError::GuardFailed { .. }) => Ok(false),
        Err(error) => Err(error),
    }
}

/// `doc_id/seq` 是否仍被未 ACK 的 outbox 引用。
///
/// 只有返回 `false` 时才允许回收 `loro_updates`（经验证 snapshot VV 覆盖
/// 属于 sync 层的独立条件，本函数不解析 Loro）。
pub fn update_referenced_by_outbox(
    vault: &Vault,
    doc_id: &str,
    seq: u64,
) -> Result<bool, StorageError> {
    let rows = vault.query(
        "SELECT count(*) FROM sync_outbox WHERE doc_id=?1 AND seq=?2",
        &[Value::Text(doc_id.to_owned()), Value::Integer(i64_of(seq)?)],
    )?;
    Ok(rows
        .first()
        .and_then(|row| row.first())
        .and_then(|value| match value {
            Value::Integer(value) => Some(*value > 0),
            _ => None,
        })
        .unwrap_or(false))
}

/// `doc_id/seq` 的 update 是否已无未 ACK outbox 引用、可回收。
///
/// 这只是**必要**条件；调用方还必须确认“经验证 snapshot VV 已覆盖该 seq”。
pub fn can_recycle_update(vault: &Vault, doc_id: &str, seq: u64) -> Result<bool, StorageError> {
    Ok(!update_referenced_by_outbox(vault, doc_id, seq)?)
}

/// `meta` 键：轮换后必须 repack outbox 的持久屏障（sync 阶段 H 写入 `b"1"`）。
pub const META_SYNC_REPACK_REQUIRED: &str = "sync.repack_required";

/// 旧 outbox 行的 CAS 键：message/epoch/整体 digest 必须同时匹配。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxKey {
    pub message_id: String,
    pub epoch: u64,
    pub cipher_digest: [u8; 32],
}

/// repack 后要写入的一个 MNE1 part。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepackPart {
    pub part_no: u32,
    pub envelope: Vec<u8>,
    pub digest: [u8; 32],
}

/// 一条 outbox 重打包目标（保留旧行的 doc_id/seq/batch_id，只换消息元数据）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepackSpec {
    pub message_id: String,
    pub object_id: [u8; 32],
    pub epoch: u64,
    pub parts: Vec<RepackPart>,
    pub cipher_digest: [u8; 32],
}

/// repack 结果；`NotFound`/`Conflict` 都保证**没有改动任何行**。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepackOutcome {
    Repacked,
    NotFound,
    Conflict,
}

/// candidate 里的 parts 摘要（不把 envelope 全量搬进列表）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepackPartSummary {
    pub part_no: u32,
    pub envelope_len: usize,
    pub digest: [u8; 32],
}

/// 一条待 repack 的旧 outbox 及其 parts 摘要。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxRepackCandidate {
    pub message_id: String,
    pub batch_id: String,
    pub doc_id: String,
    pub seq: u64,
    pub key_epoch: u64,
    pub object_id: Option<[u8; 32]>,
    pub state: SyncOutboxState,
    pub cipher_digest: Option<[u8; 32]>,
    pub parts: Vec<RepackPartSummary>,
}

/// `sync.repack_required` 屏障读取；未知值按损坏处理，避免静默漏 repack。
pub fn repack_required(vault: &Vault) -> Result<bool, StorageError> {
    match vault.get_meta(META_SYNC_REPACK_REQUIRED)? {
        None => Ok(false),
        Some(value) if value == b"0" || value.is_empty() => Ok(false),
        Some(value) if value == b"1" => Ok(true),
        Some(value) => Err(StorageError::Corrupt {
            context: format!(
                "meta {META_SYNC_REPACK_REQUIRED} 非法值：{}",
                String::from_utf8_lossy(&value)
            ),
        }),
    }
}

/// 写 `sync.repack_required` 屏障；与 sync 现有 `vault.set_meta` 使用同一 meta 键。
pub fn set_repack_required(vault: &Vault, required: bool) -> Result<(), StorageError> {
    vault.set_meta(
        META_SYNC_REPACK_REQUIRED,
        if required { b"1" } else { b"0" },
    )
}

/// 列出仍未 ACK（pending/encrypted/sent）、且 key_epoch 不是给定当前 epoch 的 outbox 行。
///
/// `encrypted` 表示 parts 已 durable 但尚未发送；换 epoch 后同样必须重包，不能漏掉。
pub fn outbox_repack_candidates(
    vault: &Vault,
    current_epoch: u64,
) -> Result<Vec<OutboxRepackCandidate>, StorageError> {
    let rows = vault.query(
        "SELECT message_id,batch_id,doc_id,seq,key_epoch,object_id,state,cipher_digest \
         FROM sync_outbox WHERE state IN('pending','encrypted','sent') AND key_epoch<>?1 \
         ORDER BY doc_id,seq,message_id",
        &[Value::Integer(i64_of(current_epoch)?)],
    )?;
    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        let row = row_to_outbox(row)?;
        let parts = outbox_parts(vault, &row.message_id, row.key_epoch)?
            .into_iter()
            .map(|part| RepackPartSummary {
                part_no: part.part_no,
                envelope_len: part.envelope.len(),
                digest: part.digest,
            })
            .collect();
        out.push(OutboxRepackCandidate {
            message_id: row.message_id,
            batch_id: row.batch_id,
            doc_id: row.doc_id,
            seq: row.seq,
            key_epoch: row.key_epoch,
            object_id: row.object_id,
            state: row.state,
            cipher_digest: row.cipher_digest,
            parts,
        });
    }
    Ok(out)
}

/// 原子地把一条旧 outbox 重打包为新的 message/object/epoch/parts。
///
/// 先做无写入的旧行 CAS 校验；校验通过后在**同一个事务**里：
/// 删除旧 parts → 删除旧 outbox 行 → 插入新 outbox 行 → 插入新 parts。
/// 旧行已被 ACK、epoch/digest 不匹配或事务中途失败时，旧行与旧 parts 都保持不变。
pub fn repack_outbox(
    vault: &Vault,
    old: &OutboxKey,
    new: &RepackSpec,
) -> Result<RepackOutcome, StorageError> {
    validate_repack_spec(new)?;
    if old.message_id.is_empty() {
        return Err(StorageError::Corrupt {
            context: "repack old.message_id 不能为空".to_owned(),
        });
    }

    let old_row = outbox_row(vault, &old.message_id)?;
    let Some(old_row) = old_row else {
        return Ok(RepackOutcome::NotFound);
    };
    if old_row.state == SyncOutboxState::Acked
        || old_row.key_epoch != old.epoch
        || old_row.cipher_digest != Some(old.cipher_digest)
    {
        return Ok(RepackOutcome::Conflict);
    }
    // 若新旧 CAS 键完全相同，迟到的旧 ACK 仍可能命中新行；这不是一次有效的重包。
    if new.message_id == old.message_id
        && new.epoch == old.epoch
        && new.cipher_digest == old.cipher_digest
    {
        return Ok(RepackOutcome::Conflict);
    }

    // 旧 parts 必须是连续 0..n-1；数据库损坏时不能静默搬走。
    let old_parts = outbox_parts(vault, &old.message_id, old.epoch)?;
    for (index, part) in old_parts.iter().enumerate() {
        if part.part_no != index as u32 {
            return Err(StorageError::Corrupt {
                context: format!(
                    "旧 outbox {}/{} parts 不连续：第 {index} 项是 {}",
                    old.message_id, old.epoch, part.part_no
                ),
            });
        }
    }

    // 同一 message_id 只能有一行；复用旧 message 时由事务内“先删后插”保证。
    if new.message_id != old.message_id && outbox_row(vault, &new.message_id)?.is_some() {
        return Ok(RepackOutcome::Conflict);
    }

    let mut statements = Vec::with_capacity(2 + 1 + new.parts.len());
    statements.push(Statement::new(
        "DELETE FROM sync_outbox_parts WHERE message_id=?1 AND epoch=?2",
        vec![
            Value::Blob(old.message_id.as_bytes().to_vec()),
            Value::Integer(i64_of(old.epoch)?),
        ],
    ));
    statements.push(Statement::new(
        "DELETE FROM sync_outbox \
         WHERE message_id=?1 AND key_epoch=?2 AND cipher_digest=?3 \
           AND state IN('pending','encrypted','sent')",
        vec![
            Value::Text(old.message_id.clone()),
            Value::Integer(i64_of(old.epoch)?),
            Value::Blob(old.cipher_digest.to_vec()),
        ],
    ));
    statements.push(Statement::new(
        "INSERT INTO sync_outbox(message_id,batch_id,doc_id,seq,key_epoch,ciphertext,object_id,state,cipher_digest) \
         VALUES(?1,?2,?3,?4,?5,NULL,?6,'encrypted',?7)",
        vec![
            Value::Text(new.message_id.clone()),
            Value::Text(old_row.batch_id.clone()),
            Value::Text(old_row.doc_id.clone()),
            Value::Integer(i64_of(old_row.seq)?),
            Value::Integer(i64_of(new.epoch)?),
            Value::Blob(new.object_id.to_vec()),
            Value::Blob(new.cipher_digest.to_vec()),
        ],
    ));
    for part in &new.parts {
        statements.push(Statement::new(
            "INSERT INTO sync_outbox_parts(message_id,epoch,part_no,envelope,digest) \
             VALUES(?1,?2,?3,?4,?5)",
            vec![
                Value::Blob(new.message_id.as_bytes().to_vec()),
                Value::Integer(i64_of(new.epoch)?),
                Value::Integer(i64::from(part.part_no)),
                Value::Blob(part.envelope.clone()),
                Value::Blob(part.digest.to_vec()),
            ],
        ));
    }

    let mut guards = vec![
        AffectedGuard::exactly(0, old_parts.len()),
        AffectedGuard::exactly(1, 1),
        AffectedGuard::exactly(2, 1),
    ];
    guards.extend((0..new.parts.len()).map(|index| AffectedGuard::exactly(3 + index, 1)));

    match vault.exec_transaction(&statements, &guards) {
        Ok(_) => Ok(RepackOutcome::Repacked),
        Err(StorageError::GuardFailed { .. }) => Ok(RepackOutcome::Conflict),
        Err(error) => Err(error),
    }
}

fn validate_repack_spec(spec: &RepackSpec) -> Result<(), StorageError> {
    if spec.message_id.is_empty() {
        return Err(StorageError::Corrupt {
            context: "repack new.message_id 不能为空".to_owned(),
        });
    }
    if spec.epoch == 0 {
        return Err(StorageError::Corrupt {
            context: "repack new.epoch 必须 >0".to_owned(),
        });
    }
    if spec.parts.is_empty() {
        return Err(StorageError::Corrupt {
            context: "repack new.parts 不能为空".to_owned(),
        });
    }
    for (index, part) in spec.parts.iter().enumerate() {
        if part.part_no != index as u32 {
            return Err(StorageError::Corrupt {
                context: format!(
                    "repack new.parts 必须从 0 连续：第 {index} 项是 {}",
                    part.part_no
                ),
            });
        }
        if part.envelope.is_empty() {
            return Err(StorageError::Corrupt {
                context: format!("repack new.parts 第 {index} 片信封为空"),
            });
        }
        if part.digest.len() != 32 {
            return Err(StorageError::Corrupt {
                context: format!("repack new.parts 第 {index} 片 digest 不是 32B"),
            });
        }
    }
    Ok(())
}

fn validate_intent(
    message_id: &str,
    batch_id: &str,
    seq: u64,
    key_epoch: u64,
) -> Result<(), StorageError> {
    if message_id.is_empty() || batch_id.is_empty() {
        return Err(StorageError::Corrupt {
            context: "sync intent message_id/batch_id 不能为空".to_owned(),
        });
    }
    if seq == 0 || key_epoch == 0 {
        return Err(StorageError::Corrupt {
            context: "sync intent seq/key_epoch 必须 >0".to_owned(),
        });
    }
    Ok(())
}

fn row_to_outbox(row: &crate::Row) -> Result<SyncOutboxRow, StorageError> {
    let object_id = row
        .get(5)
        .and_then(blob_value)
        .map(|bytes| array32_bytes(bytes, "sync_outbox.object_id"))
        .transpose()?;
    let cipher_digest = row
        .get(7)
        .and_then(blob_value)
        .map(|bytes| array32_bytes(bytes, "sync_outbox.cipher_digest"))
        .transpose()?;
    Ok(SyncOutboxRow {
        message_id: text_of(row, 0, "sync_outbox.message_id")?,
        batch_id: text_of(row, 1, "sync_outbox.batch_id")?,
        doc_id: text_of(row, 2, "sync_outbox.doc_id")?,
        seq: u64_of(row, 3, "sync_outbox.seq")?,
        key_epoch: u64_of(row, 4, "sync_outbox.key_epoch")?,
        object_id,
        state: SyncOutboxState::parse(&text_of(row, 6, "sync_outbox.state")?)?,
        cipher_digest,
    })
}

/// 入站：durable 保存已验签/已解密对象（依赖是否齐由 sync 层决定）。
pub fn store_inbox_verified(
    vault: &Vault,
    log_id: &[u8; 16],
    relay_seq: u64,
    object_id: &[u8; 32],
    epoch: u64,
    verified_payload: &[u8],
) -> Result<(), StorageError> {
    if relay_seq == 0 || epoch == 0 {
        return Err(StorageError::Corrupt {
            context: "inbox relay_seq/epoch 必须 >0".to_owned(),
        });
    }
    ensure_log_id(vault, log_id)?;

    let rows = vault.query(
        "SELECT object_id,epoch,state,verified_payload FROM sync_inbox \
         WHERE log_id=?1 AND relay_seq=?2",
        &[
            Value::Blob(log_id.to_vec()),
            Value::Integer(i64_of(relay_seq)?),
        ],
    )?;
    if let Some(row) = rows.first() {
        let old_object = array32(row, 0, "sync_inbox.object_id")?;
        let old_epoch = u64_of(row, 1, "sync_inbox.epoch")?;
        let old_payload = row.get(3).and_then(blob_value).unwrap_or_default();
        if old_object != *object_id || old_epoch != epoch || old_payload != verified_payload {
            return Err(StorageError::SyncConflict {
                context: format!("inbox {log_id:?}/{relay_seq} 字节不一致"),
            });
        }
        advance_received_prefix(vault)?;
        return Ok(());
    }

    let changed = vault.exec(
        "INSERT INTO sync_inbox(log_id,relay_seq,object_id,epoch,state,verified_payload) \
         VALUES(?1,?2,?3,?4,'verified',?5)",
        &[
            Value::Blob(log_id.to_vec()),
            Value::Integer(i64_of(relay_seq)?),
            Value::Blob(object_id.to_vec()),
            Value::Integer(i64_of(epoch)?),
            Value::Blob(verified_payload.to_vec()),
        ],
    )?;
    if changed != 1 {
        return Err(StorageError::SyncConflict {
            context: format!("inbox {log_id:?}/{relay_seq} 写入冲突"),
        });
    }
    advance_received_prefix(vault)?;
    Ok(())
}

/// 入站批量入口：**一个事务**完成「既有行比对 + 新行插入 + `received` 前缀推进」。
///
/// 语义与逐条入口**完全一致**：
///
/// - 已存在同 `(log_id, relay_seq)` 行、且 `object_id/epoch/verified_payload` 完全相同 →
///   幂等跳过（不重写、不报错，也不触发任何写入守卫）；
/// - 已存在同行但字节不同 → [`StorageError::SyncConflict`]，**整批一行都不写**；
/// - 新行插入与 `received` 连续前缀推进在**同一个** SQLite 事务里（Phase 1A-1），
///   一次提交只产生一次 fsync；冲突/崩溃只会整批回滚，不存在"行写了、前缀没推进"的
///   半提交状态；前缀只覆盖已消费的连续序号（含 `sync.skipped_received` 的 ASSET 标记）。
///
/// 输入先按 `relay_seq` 升序**稳定排序**（不要求调用方预排序；允许序号不连续，例如一页里
/// 混有已消费的 ASSET 序号）；同一批里出现重复 `relay_seq` 直接拒绝——一条日志不可能有
/// 两个同序号事件，静默去重会掩盖上游错误。全部校验/排序在 worker 的事务入口
/// （`actor` 模块的 `ExecInboxBatch` 命令）再做一遍，这里只负责搬运数据。
///
/// 返回**本次新插入**的行数（幂等跳过的行不计）。
pub fn store_inbox_verified_batch(
    vault: &Vault,
    rows: &[VerifiedInboxRow],
) -> Result<usize, StorageError> {
    if rows.is_empty() {
        return Ok(0);
    }
    let batch = rows
        .iter()
        .map(|row| crate::actor::InboxBatchRow {
            log_id: row.log_id,
            relay_seq: row.relay_seq,
            object_id: row.object_id,
            epoch: row.epoch,
            verified_payload: row.verified_payload.clone(),
        })
        .collect();
    vault.exec_inbox_batch(batch)
}

/// 读当前已验证、且落在 durable 连续接收前缀内的 inbox 行。
///
/// 依赖完整性由 sync 层判断（本 crate 不解析 Loro VV）；这里的 "ready"
/// 指 cryptographically verified + durable + **属于当前 `sync.log_id`** +
/// `relay_seq <= received_seq`，不在连续前缀内的行保持 durable，等前面的缺口补齐。
/// 排序为 `(log_id, relay_seq)`。
///
/// **incarnation 过滤**（ADR 0006 §5）：`sync_inbox` 的 `log_id` 是写入时那代 relay
/// 日志的 incarnation。restore 迁移 `sync.log_id` 后，旧 incarnation 的行仍然 durable
/// （不能在迁移时静默删数据），但绝不能进入 ready——`relay_seq` 只在同一 incarnation
/// 内有序，跨代比较没有意义。过滤条件里的 `log_id` 由 [`read_current_log_id`] 读出后
/// 作为 SQL 参数绑定，不做子查询以便调用方/测试都看到同一个显式值。
///
/// `sync.log_id` 不存在（新库尚未绑定 relay 日志代）时返回空：没有当前 incarnation
/// 就没有可匹配的行；新库此时 `received` 也是 0，所以与旧实现（不按 log 过滤）的结果
/// 一致，不会把其它 incarnation 的遗留行漏出来。`sync.log_id` 存在但不是 16B →
/// [`StorageError::Corrupt`]（与 [`sync_cursor_state`] 同口径），不静默当作"没有 log"。
pub fn take_inbox_ready(vault: &Vault, limit: usize) -> Result<Vec<SyncInboxRow>, StorageError> {
    let Some(log_id) = read_current_log_id(vault)? else {
        return Ok(Vec::new());
    };
    let received = read_meta_u64(vault, META_SYNC_RECEIVED_SEQ)?.unwrap_or(0);
    let rows = vault.query(
        "SELECT log_id,relay_seq,object_id,epoch,state,verified_payload \
         FROM sync_inbox WHERE log_id=?1 AND state='verified' AND relay_seq<=?2 \
         ORDER BY log_id,relay_seq LIMIT ?3",
        &[
            Value::Blob(log_id.to_vec()),
            Value::Integer(i64_of(received)?),
            Value::Integer(i64::try_from(limit).unwrap_or(i64::MAX)),
        ],
    )?;
    rows.iter().map(row_to_inbox).collect()
}

/// 统计「已验证、属于当前 incarnation、落在 durable 连续接收前缀内、尚未集成」的行数。
///
/// 语义（ADR 0012 S2b）：这些行不是"丢失"——它们要么依赖未就绪（deferred），要么被
/// 批次预算留在本轮之后。调用方用它上报"已接收·待应用 N"，并据此判断"还有没有
/// 可继续的工作"，而不是循环刷最早的一批。
///
/// 与 [`take_inbox_ready`] 同口径：只计当前 `sync.log_id` 的行，旧 incarnation 的
/// verified 行即使 `relay_seq <= received` 也不计。`sync.log_id` 不存在返回 0
/// （没有当前 incarnation 就没有 ready 行）；存在但长度非法 → [`StorageError::Corrupt`]。
pub fn count_inbox_ready(vault: &Vault) -> Result<u64, StorageError> {
    let Some(log_id) = read_current_log_id(vault)? else {
        return Ok(0);
    };
    let received = read_meta_u64(vault, META_SYNC_RECEIVED_SEQ)?.unwrap_or(0);
    let rows = vault.query(
        "SELECT COUNT(*) FROM sync_inbox \
         WHERE log_id=?1 AND state='verified' AND relay_seq<=?2",
        &[
            Value::Blob(log_id.to_vec()),
            Value::Integer(i64_of(received)?),
        ],
    )?;
    rows.first()
        .map(|row| u64_of(row, 0, "sync_inbox.count"))
        .transpose()?
        .ok_or_else(|| StorageError::Corrupt {
            context: "sync_inbox 计数缺失".to_owned(),
        })
}

/// 带调用方依赖判断的 ready 选择器；不满足的项保持 durable verified。
///
/// 底层走 [`take_inbox_ready`]，因此同样只选择当前 `sync.log_id` 的行。
pub fn take_inbox_ready_where(
    vault: &Vault,
    limit: usize,
    mut is_ready: impl FnMut(&SyncInboxRow) -> bool,
) -> Result<Vec<SyncInboxRow>, StorageError> {
    let rows = take_inbox_ready(vault, limit.saturating_mul(16).max(limit))?;
    Ok(rows
        .into_iter()
        .filter(|row| is_ready(row))
        .take(limit)
        .collect())
}

/// 为 remote import 分配本地 `loro_updates.seq = head_seq + 1`。
///
/// **只分配，不写库**；调用方必须在同一 A 事务里把该 seq 用在
/// `loro_updates.seq`，禁止直接使用 sender_seq。
pub fn allocate_local_seq_for_import(vault: &Vault, doc_id: &str) -> Result<u64, StorageError> {
    let head = store::load_head(vault, doc_id)?;
    head.head_seq
        .checked_add(1)
        .ok_or_else(|| StorageError::Corrupt {
            context: format!("{doc_id} 本地 seq 溢出"),
        })
}

/// 把一个 verified inbox 行标为 integrated，并推进连续 integrated 前缀。
pub fn mark_integrated(
    vault: &Vault,
    log_id: &[u8; 16],
    relay_seq: u64,
) -> Result<(), StorageError> {
    let changed = vault.exec(
        "UPDATE sync_inbox SET state='integrated' \
         WHERE log_id=?1 AND relay_seq=?2 AND state='verified'",
        &[
            Value::Blob(log_id.to_vec()),
            Value::Integer(i64_of(relay_seq)?),
        ],
    )?;
    if changed == 0 {
        let rows = vault.query(
            "SELECT state FROM sync_inbox WHERE log_id=?1 AND relay_seq=?2",
            &[
                Value::Blob(log_id.to_vec()),
                Value::Integer(i64_of(relay_seq)?),
            ],
        )?;
        let Some(row) = rows.first() else {
            return Err(StorageError::Corrupt {
                context: format!("inbox {log_id:?}/{relay_seq} 不存在"),
            });
        };
        let state = text_of(row, 0, "sync_inbox.state")?;
        if state != SyncInboxState::Integrated.as_str() {
            return Err(StorageError::SyncConflict {
                context: format!("inbox {log_id:?}/{relay_seq} 当前 state={state}，不能 integrate"),
            });
        }
    }
    advance_integrated_prefix(vault)?;
    Ok(())
}

/// 批量把同一 log 的多条 verified inbox 行标为 integrated（**一个事务**）。
///
/// Phase 1A-1 为聚合 integrate 准备的 API：调用方在整批正文/依赖都落库后一次性标记，
/// 再统一调用 [`advance_integrated_prefix`]（本函数**不推进**前缀）。
///
/// 严格语义：`relay_seqs` 中每一行都必须存在且当前 `state='verified'`；任何一行
/// 不存在 / 已 integrated / 其它 state → 整批回滚并返回 [`StorageError::SyncConflict`]，
/// 不会留下半批已 integrated 的行。重复序号同理（第二次更新影响 0 行）整批回滚。
///
/// 返回标记的行数（= 输入行数；空输入返回 0）。
pub fn mark_integrated_batch(
    vault: &Vault,
    log_id: &[u8; 16],
    relay_seqs: &[u64],
) -> Result<usize, StorageError> {
    if relay_seqs.is_empty() {
        return Ok(0);
    }
    let mut statements = Vec::with_capacity(relay_seqs.len());
    let mut guards = Vec::with_capacity(relay_seqs.len());
    for (index, relay_seq) in relay_seqs.iter().enumerate() {
        statements.push(Statement::new(
            "UPDATE sync_inbox SET state='integrated' \
             WHERE log_id=?1 AND relay_seq=?2 AND state='verified'",
            vec![
                Value::Blob(log_id.to_vec()),
                Value::Integer(i64_of(*relay_seq)?),
            ],
        ));
        guards.push(AffectedGuard::exactly(index, 1));
    }
    match vault.exec_transaction(&statements, &guards) {
        Ok(_) => Ok(relay_seqs.len()),
        // 守卫不满足 = 某行不存在或已不是 verified；exec_transaction 已整笔回滚。
        Err(StorageError::GuardFailed { .. }) => Err(StorageError::SyncConflict {
            context: format!(
                "inbox {log_id:?} 批量 integrate 失败：某行不存在或当前不是 verified（整批已回滚）"
            ),
        }),
        Err(error) => Err(error),
    }
}

/// 把 inbox 行标为 quarantine（完整性失败/依赖永久缺失）。
pub fn mark_inbox_quarantine(
    vault: &Vault,
    log_id: &[u8; 16],
    relay_seq: u64,
) -> Result<(), StorageError> {
    let changed = vault.exec(
        "UPDATE sync_inbox SET state='quarantine' \
         WHERE log_id=?1 AND relay_seq=?2 AND state IN('received','verified')",
        &[
            Value::Blob(log_id.to_vec()),
            Value::Integer(i64_of(relay_seq)?),
        ],
    )?;
    if changed == 0 {
        return Err(StorageError::SyncConflict {
            context: format!("inbox {log_id:?}/{relay_seq} 不能进入 quarantine"),
        });
    }
    Ok(())
}

fn row_to_inbox(row: &crate::Row) -> Result<SyncInboxRow, StorageError> {
    Ok(SyncInboxRow {
        log_id: array16(row, 0, "sync_inbox.log_id")?,
        relay_seq: u64_of(row, 1, "sync_inbox.relay_seq")?,
        object_id: array32(row, 2, "sync_inbox.object_id")?,
        epoch: u64_of(row, 3, "sync_inbox.epoch")?,
        state: SyncInboxState::parse(&text_of(row, 4, "sync_inbox.state")?)?,
        verified_payload: row.get(5).and_then(blob_value),
    })
}

/// 读本地同步水位。
pub fn sync_cursor_state(vault: &Vault) -> Result<SyncCursorState, StorageError> {
    Ok(SyncCursorState {
        received_seq: read_meta_u64(vault, META_SYNC_RECEIVED_SEQ)?.unwrap_or(0),
        integrated_seq: read_meta_u64(vault, META_SYNC_INTEGRATED_SEQ)?.unwrap_or(0),
        log_id: read_current_log_id(vault)?,
        confirmed_highwater: read_meta_u64(vault, META_SYNC_CONFIRMED_HIGHWATER)?,
    })
}

/// 读当前 relay 日志 incarnation（`sync.log_id`，BLOB 16B）。
///
/// - 不存在（新库/尚未绑定）：`Ok(None)`，调用方按"没有当前 incarnation"处理
///   （ready 视为空，绝不回退成"不过滤 log"）；
/// - 存在但不是 16B：[`StorageError::Corrupt`]，与 [`sync_cursor_state`] 的既有口径一致。
fn read_current_log_id(vault: &Vault) -> Result<Option<[u8; 16]>, StorageError> {
    read_meta(vault, META_SYNC_LOG_ID)?
        .map(|bytes| array16_bytes(bytes, "sync.log_id"))
        .transpose()
}

/// 写入 `sync.log_id`；已有不同值则拒绝（restore 必须显式迁移）。
pub fn set_sync_log_id(vault: &Vault, log_id: &[u8; 16]) -> Result<(), StorageError> {
    ensure_log_id(vault, log_id)
}

/// 写 confirmed highwater（BE64 BLOB）。
pub fn set_confirmed_highwater(vault: &Vault, value: u64) -> Result<(), StorageError> {
    write_meta(vault, META_SYNC_CONFIRMED_HIGHWATER, &value.to_be_bytes())
}

/// 单条入口的 `sync.log_id` 校验（走 Vault 命令，独立事务）。
///
/// 批量 inbox 事务内的同语义校验在 `sync_meta::ensure_log_id_on`（直接跑在连接/事务上）；
/// 两处必须保持一致：不存在则写，存在不同则 `SyncLogMismatch`。
fn ensure_log_id(vault: &Vault, log_id: &[u8; 16]) -> Result<(), StorageError> {
    match read_meta(vault, META_SYNC_LOG_ID)? {
        Some(existing) => {
            if existing.as_slice() != log_id {
                return Err(StorageError::SyncLogMismatch {
                    expected: existing,
                    found: log_id.to_vec(),
                });
            }
        }
        None => write_meta(vault, META_SYNC_LOG_ID, log_id)?,
    }
    Ok(())
}

fn advance_received_prefix(vault: &Vault) -> Result<(), StorageError> {
    vault.advance_consumed_prefix(ConsumedPrefix::Received)
}

/// 把已集成连续前缀推进到最大已消费序号（**一次**写最终值）。
///
/// 供聚合 integrate 的调用方在整批正文都落库后调用一次（Phase 1A-1）。语义与旧实现
/// 一致：只认 inbox `state='integrated'` 或 `sync.skipped_received` 标记，扫窗口后
/// 一次写最终值；崩溃只会少推进，下次调用重算。
pub fn advance_integrated_prefix(vault: &Vault) -> Result<(), StorageError> {
    vault.advance_consumed_prefix(ConsumedPrefix::Integrated)
}

/// 登记一个**不写 inbox**、但已 durable 消费的事件（ASSET），并推进连续接收前缀。
///
/// 调用方必须先把该事件的事实（对 ASSET 是 skipped-asset HMAC 记录）durable 化，
/// 再调用本函数；顺序保证"received 只覆盖已消费的事件"。
pub fn mark_skipped_event_received(
    vault: &Vault,
    log_id: &[u8; 16],
    relay_seq: u64,
) -> Result<(), StorageError> {
    let key = skipped_received_key(log_id, relay_seq)?;
    write_meta(vault, &key, b"1")?;
    advance_received_prefix(vault)
}

/// 读一条 meta（同步桥经 Vault 命令的读取路径）。
///
/// 类型判定与 [`Vault::get_meta`] / `sync_meta::read_meta_on` 共用同一个 helper
/// [`meta_bytes_of`]：**只认 BLOB**，其它类型是 [`StorageError::Corrupt`]。
/// 这里以前把 TEXT 当"缺失"，会让 `sync.log_id` / `sync.repack_required` /
/// `sync.received_seq` 这些持久屏障与游标被静默绕过。
fn read_meta(vault: &Vault, key: &str) -> Result<Option<Vec<u8>>, StorageError> {
    let rows = vault.query(
        "SELECT value FROM meta WHERE key=?1",
        &[Value::Text(key.to_owned())],
    )?;
    let Some(value) = rows.first().and_then(|row| row.first()) else {
        return Ok(None);
    };
    Ok(Some(meta_bytes_of(key, value.clone())?))
}

fn read_meta_u64(vault: &Vault, key: &str) -> Result<Option<u64>, StorageError> {
    let Some(bytes) = read_meta(vault, key)? else {
        return Ok(None);
    };
    if bytes.len() != 8 {
        return Err(StorageError::Corrupt {
            context: format!("{key} 应为 BE64 8B，实际 {}B", bytes.len()),
        });
    }
    Ok(Some(u64::from_be_bytes(bytes.try_into().expect("长度8"))))
}

fn write_meta(vault: &Vault, key: &str, value: &[u8]) -> Result<(), StorageError> {
    vault.exec(
        "INSERT INTO meta(key,value) VALUES(?1,?2) \
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        &[Value::Text(key.to_owned()), Value::Blob(value.to_vec())],
    )?;
    Ok(())
}

fn text_of(row: &crate::Row, index: usize, field: &str) -> Result<String, StorageError> {
    match row.get(index) {
        Some(Value::Text(value)) => Ok(value.clone()),
        other => Err(StorageError::Corrupt {
            context: format!("{field} 类型异常：{other:?}"),
        }),
    }
}

fn text_from_blob(row: &crate::Row, index: usize, field: &str) -> Result<String, StorageError> {
    let bytes = blob_of(row, index, field)?;
    String::from_utf8(bytes).map_err(|_| StorageError::Corrupt {
        context: format!("{field} 不是 UTF-8 message id"),
    })
}

fn blob_of(row: &crate::Row, index: usize, field: &str) -> Result<Vec<u8>, StorageError> {
    row.get(index)
        .and_then(blob_value)
        .ok_or_else(|| StorageError::Corrupt {
            context: format!("{field} 缺失或类型异常"),
        })
}

fn blob_value(value: &Value) -> Option<Vec<u8>> {
    match value {
        Value::Blob(bytes) => Some(bytes.clone()),
        _ => None,
    }
}

fn array16(row: &crate::Row, index: usize, field: &str) -> Result<[u8; 16], StorageError> {
    array16_bytes(blob_of(row, index, field)?, field)
}

fn array32(row: &crate::Row, index: usize, field: &str) -> Result<[u8; 32], StorageError> {
    array32_bytes(blob_of(row, index, field)?, field)
}

fn array16_bytes(bytes: Vec<u8>, field: &str) -> Result<[u8; 16], StorageError> {
    bytes
        .try_into()
        .map_err(|bytes: Vec<u8>| StorageError::Corrupt {
            context: format!("{field} 应为 16B，实际 {}B", bytes.len()),
        })
}

fn array32_bytes(bytes: Vec<u8>, field: &str) -> Result<[u8; 32], StorageError> {
    bytes
        .try_into()
        .map_err(|bytes: Vec<u8>| StorageError::Corrupt {
            context: format!("{field} 应为 32B，实际 {}B", bytes.len()),
        })
}

fn u64_of(row: &crate::Row, index: usize, field: &str) -> Result<u64, StorageError> {
    match row.get(index) {
        Some(Value::Integer(value)) => u64::try_from(*value).map_err(|_| StorageError::Corrupt {
            context: format!("{field} 为负数"),
        }),
        other => Err(StorageError::Corrupt {
            context: format!("{field} 类型异常：{other:?}"),
        }),
    }
}

fn u32_of(row: &crate::Row, index: usize, field: &str) -> Result<u32, StorageError> {
    let value = u64_of(row, index, field)?;
    u32::try_from(value).map_err(|_| StorageError::Corrupt {
        context: format!("{field} 超出 u32"),
    })
}

fn i64_of(value: u64) -> Result<i64, StorageError> {
    i64::try_from(value).map_err(|_| StorageError::Corrupt {
        context: format!("数值 {value} 超出 SQLite INTEGER"),
    })
}

/// 出站 digest 的 map 形式；用于按 message 聚合时去重（本模块内部）。
#[allow(dead_code)]
fn group_parts_by_message(parts: Vec<SyncOutboxPart>) -> BTreeMap<String, Vec<SyncOutboxPart>> {
    let mut grouped = BTreeMap::new();
    for part in parts {
        grouped
            .entry(part.message_id.clone())
            .or_insert_with(Vec::new)
            .push(part);
    }
    grouped
}

`````



@@SECTION code/074 `crates/storage/src/sync_meta.rs`


`````rust
//! 同步元数据（`meta` 键）与「已消费连续前缀」的低层实现。
//!
//! 这里直接跑在 `rusqlite::Connection` 上，而不是走 [`crate::Vault`] 命令：
//! `rusqlite::Transaction` 通过 `Deref` 也是 `&Connection`，因此同一套扫描/写入既能被
//! 普通 Vault 命令复用（worker 线程的连接），也能被
//! `store_inbox_verified_batch` 放进**同一个**事务里执行——这是 Phase 1A-1
//! 「批量 inbox 原子化」的关键：新行插入与 `sync.received_seq` 推进一起提交/回滚。
//!
//! 语义与旧实现逐字等价：前缀只覆盖连续、已 durable 消费的序号；崩溃只会少推进
//! （下次扫描幂等重算），绝不会把未消费的事件算成已消费。

use rusqlite::types::Value;
use rusqlite::Connection;

use crate::error::StorageError;

/// `meta` 键：已 durable 连续接收前缀（BLOB BE64）。
pub const META_SYNC_RECEIVED_SEQ: &str = "sync.received_seq";
/// `meta` 键：已 integrate 连续前缀（BLOB BE64）。
pub const META_SYNC_INTEGRATED_SEQ: &str = "sync.integrated_seq";
/// `meta` 键：当前 relay log incarnation（BLOB 16B）。
pub const META_SYNC_LOG_ID: &str = "sync.log_id";
/// `meta` 键：已确认高水位（BLOB BE64）。
pub const META_SYNC_CONFIRMED_HIGHWATER: &str = "sync.confirmed_highwater";

/// `meta` 键前缀：**非 inbox 事件**（当前只有 ASSET）已 durable 消费的标记。
///
/// 完整键：`sync.skipped_received.<log_id 32 位十六进制>.<relay_seq 20 位十进制>`。
/// ASSET 事件不写 `sync_inbox`（它不是文档水位事件），但 `received_seq` 是"连续前缀"：
/// 若直接把 ASSET 当缺口，`received_seq` 会永久卡在该序号之前，后续 UPDATE 永远无法
/// 集成。pull 在把 ASSET 落成 durable skipped 记录后写本标记，前缀推进同时接受
/// inbox 行与这类标记，保证"已消费"的事件不会把前缀卡住。
pub const META_SYNC_SKIPPED_RECEIVED_PREFIX: &str = "sync.skipped_received.";

/// 一次前缀扫描中**每个来源**（inbox 行 / `skipped_received` 标记）各自的窗口大小。
/// 正常一页 256 条；4096 已远超单页，只作上限保护。
const PREFIX_SCAN_LIMIT: usize = 4096;

/// 要推进的连续前缀及其"已消费"判定。
///
/// 两个变体的条件与旧实现完全一致：
/// - `Received`：inbox `state IN('verified','integrated')`（读者尚未集成也算已接收）；
/// - `Integrated`：inbox `state='integrated'`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConsumedPrefix {
    /// `sync.received_seq`。
    Received,
    /// `sync.integrated_seq`。
    Integrated,
}

impl ConsumedPrefix {
    /// 对应的 `meta` 键。
    pub(crate) fn meta_key(self) -> &'static str {
        match self {
            Self::Received => META_SYNC_RECEIVED_SEQ,
            Self::Integrated => META_SYNC_INTEGRATED_SEQ,
        }
    }

    /// inbox 行算作「已消费」的 SQL 条件。
    pub(crate) fn inbox_condition(self) -> &'static str {
        match self {
            Self::Received => "state IN('verified','integrated')",
            Self::Integrated => "state='integrated'",
        }
    }
}

/// 校验 `sync.log_id`：不存在则写入，已存在不同值则 [`StorageError::SyncLogMismatch`]。
///
/// 与 Vault 版 [`crate::sync_bridge::set_sync_log_id`] 同语义，区别只是直接在给定连接
/// （或事务）上执行，供批量 inbox 在同一事务里完成校验。
///
/// 既有值不是 BLOB 时 [`read_meta_on`] 返回 [`StorageError::Corrupt`]，本函数直接传播：
/// 损坏的 incarnation 记录**不能**当成"尚未绑定"覆盖掉（那会让旧传代的 inbox 行混进
/// 新一代）。
pub(crate) fn ensure_log_id_on(conn: &Connection, log_id: &[u8; 16]) -> Result<(), StorageError> {
    match read_meta_on(conn, META_SYNC_LOG_ID)? {
        Some(existing) => {
            if existing.as_slice() != log_id {
                return Err(StorageError::SyncLogMismatch {
                    expected: existing,
                    found: log_id.to_vec(),
                });
            }
        }
        None => write_meta_on(conn, META_SYNC_LOG_ID, log_id)?,
    }
    Ok(())
}

/// 把连续前缀推进到最大已消费序号（**一次**写最终值）。
///
/// 语义与"从 `current+1` 起逐条检查、遇到第一个缺口停下"逐字等价；差别只在写盘次数：
/// 旧实现对每个前进的序号各写一次 `meta`（每次都是独立事务，WAL + `synchronous=FULL`
/// 下就是一次 fsync）。现在的做法：先用两次读（inbox 升序窗口 + 同 log 的
/// `skipped_received` 键范围扫描，**两个窗口各自**有 [`PREFIX_SCAN_LIMIT`] 条上限）
/// 在内存里走到第一个缺口，再**一次**写入最终值。
///
/// 两个来源的窗口可能分别被截断：只要还有来源没看到"缺口之后"的序号，缺口就不算
/// 证明完毕，必须从新的 `current` 继续下一窗口（见循环内注释）。
///
/// 崩溃语义没有变差：inbox 行与 skipped 标记都先 durable；前缀少推进只会让下一次调用
/// 重算（扫描幂等），绝不会把未消费的事件算成已消费。
///
/// 在 [`crate::Vault`] 命令里传入 worker 独占连接时是自动提交读 + 单条写；
/// 在批量 inbox 事务里传入 `&Transaction` 时，读/写都与新行插入处于同一事务。
pub(crate) fn advance_consumed_prefix_on(
    conn: &Connection,
    prefix: ConsumedPrefix,
) -> Result<(), StorageError> {
    let Some(log_id) = read_meta_on(conn, META_SYNC_LOG_ID)? else {
        return Ok(());
    };
    let log_id = array16_bytes(log_id, "sync.log_id")?;
    let meta_key = prefix.meta_key();
    let mut current = read_meta_u64_on(conn, meta_key)?.unwrap_or(0);
    loop {
        // 与旧实现同口径：前缀已经是 u64::MAX 时无法再前进（保留溢出错误）。
        current
            .checked_add(1)
            .ok_or_else(|| StorageError::Corrupt {
                context: format!("{meta_key} 溢出"),
            })?;
        let window = scan_consumed_after_on(conn, &log_id, current, prefix)?;
        if window.seqs.is_empty() {
            return Ok(());
        }
        // 在内存里从 current+1 走到第一个缺口（seqs 已升序去重）。
        let mut next = current;
        for &seq in &window.seqs {
            if Some(seq) == next.checked_add(1) {
                next = seq;
            } else {
                break;
            }
        }
        if next == current {
            // 首条观测序号就大于 current+1：缺口就在 current+1。
            // 两个窗口的下界都恰好是 current+1（inbox 用 relay_seq>current，skipped 用
            // key(current+1) 起扫），窗口又总是按升序取最小的若干条：若 current+1 确实
            // 已消费，它必然是窗口里最小的那一条，不可能被截断漏掉。缺口已证明，停止。
            return Ok(());
        }
        current = next;
        write_meta_on(conn, meta_key, &current.to_be_bytes())?;
        let gap = current
            .checked_add(1)
            .ok_or_else(|| StorageError::Corrupt {
                context: format!("{meta_key} 溢出"),
            })?;
        // 两个来源都没被截断 → 已看到 current 之后的全部已消费序号，缺口是真的。
        // 被截断但每个被截断来源都已观测到 ≥ 缺口位置的序号 → 缺口同样是真的
        // （未观测序号都大于各自的窗口最大值）。两种情况都不必再扫。
        if !window.truncated() || window.proves_gap_at(gap) {
            return Ok(());
        }
        // 否则继续下一窗口。典型场景：`skipped_received` 窗口被截断在 4096，而 inbox
        // 窗口里还有一条更大的序号（例如 10000）—— 若沿用「合并后的 truncated 且
        // current < last」判断，这里会因为 current(4096) < last(10000) 提前 return，
        // 把下一个窗口里 ≤ 缺口的 skipped 序号永久漏掉、前缀卡死。
        // 下一轮从新的 current+1 起扫；每个窗口只可能看到更大的序号，所以要么推进、
        // 要么在「首条观测序号 > current+1」处判定缺口并退出，不会死循环。
    }
}

/// 一次前缀扫描的窗口观测结果。
///
/// inbox 行与 `skipped_received` 标记是**两个独立的 `LIMIT` 窗口**：某个来源被截断只
/// 说明该来源还有更大的序号没读到（未观测序号一定大于它自己的最大观测序号），不能拿
/// 另一个来源的窗口去推断缺口是否成立。因此截断状态与最大观测序号都按来源记录。
struct ConsumedScanWindow {
    /// 两个来源合并、升序去重后的已消费序号。
    seqs: Vec<u64>,
    /// inbox 窗口是否被 `LIMIT` 截断。
    inbox_truncated: bool,
    /// `skipped_received` 窗口是否被 `LIMIT` 截断。
    skipped_truncated: bool,
    /// inbox 窗口观测到的最大序号（窗口按序号升序，末条即最大）。
    inbox_last: Option<u64>,
    /// `skipped_received` 窗口观测到的最大序号（键定宽，字典序即序号序）。
    skipped_last: Option<u64>,
}

impl ConsumedScanWindow {
    /// 任一来源还有未观测的已消费序号（需要继续下一个窗口）。
    fn truncated(&self) -> bool {
        self.inbox_truncated || self.skipped_truncated
    }

    /// `gap` 处的缺口是否已被本窗口证明。
    ///
    /// 对每个被截断的来源，未观测序号都严格大于该来源窗口的最大观测序号：只要该最大值
    /// ≥ `gap`，未观测序号就不可能落在 `gap` 上。任一被截断来源不满足这条，缺口就没有
    /// 证明完毕，必须继续下一窗口。
    fn proves_gap_at(&self, gap: u64) -> bool {
        (!self.inbox_truncated || self.inbox_last.is_some_and(|last| last >= gap))
            && (!self.skipped_truncated || self.skipped_last.is_some_and(|last| last >= gap))
    }
}

/// 扫出 `current` 之后**已消费**的序号（升序去重），以及两个来源窗口的截断状态。
///
/// "已消费"= inbox 行满足 [`ConsumedPrefix::inbox_condition`] **或**存在
/// `sync.skipped_received` 标记（ASSET 这类不写 inbox 的事件）。`skipped_received`
/// 键是固定宽度的 `<prefix><hex log_id>.<20 位序号>`，字典序等于序号序，因此可以用
/// `meta` 主键做**范围扫描**，不需要 `LIKE` 全表匹配。
///
/// 两个来源各自带 [`PREFIX_SCAN_LIMIT`] 上界并各自报告截断状态：一次把某条日志的全部
/// 标记读进内存会在长日志上无界增长，而只合并成一个 `truncated` 标志又无法区分"哪个
/// 来源还有更大的序号没读到"，缺口证明就会出错（见 [`ConsumedScanWindow::proves_gap_at`]）。
fn scan_consumed_after_on(
    conn: &Connection,
    log_id: &[u8; 16],
    current: u64,
    prefix: ConsumedPrefix,
) -> Result<ConsumedScanWindow, StorageError> {
    let sql = format!(
        "SELECT relay_seq FROM sync_inbox \
         WHERE log_id=?1 AND relay_seq>?2 AND {} \
         ORDER BY relay_seq LIMIT ?3",
        prefix.inbox_condition()
    );
    let window_limit = i64::try_from(PREFIX_SCAN_LIMIT).unwrap_or(i64::MAX);
    let (mut seqs, inbox_truncated) = {
        let mut statement = conn.prepare(&sql)?;
        let mut rows = statement.query(rusqlite::params_from_iter([
            Value::Blob(log_id.to_vec()),
            Value::Integer(i64_of(current)?),
            Value::Integer(window_limit),
        ]))?;
        let mut seqs = Vec::new();
        while let Some(row) = rows.next()? {
            let value: i64 = row.get(0)?;
            seqs.push(u64::try_from(value).map_err(|_| StorageError::Corrupt {
                context: "sync_inbox.relay_seq 为负数".to_owned(),
            })?);
        }
        let truncated = seqs.len() == PREFIX_SCAN_LIMIT;
        (seqs, truncated)
    };
    let inbox_last = seqs.last().copied();

    let lower = skipped_received_key(log_id, current.saturating_add(1))?;
    let upper = skipped_received_key(log_id, u64::MAX)?;
    let mut skipped_last: Option<u64> = None;
    let skipped_truncated = {
        let mut statement = conn
            .prepare("SELECT key FROM meta WHERE key>=?1 AND key<=?2 ORDER BY key ASC LIMIT ?3")?;
        let mut rows = statement.query(rusqlite::params_from_iter([
            Value::Text(lower),
            Value::Text(upper),
            Value::Integer(window_limit),
        ]))?;
        let mut scanned = 0usize;
        while let Some(row) = rows.next()? {
            scanned = scanned.saturating_add(1);
            let key: String = row.get(0)?;
            // 格式不认识的键不参与前缀推进（fail-safe）：它既不算已消费，也不提供
            // 截断边界。可解析的未观测序号仍严格大于这里见过的最大可解析序号，所以
            // 用已解析最大值做下界是安全的。
            let Some(seq) = parse_skipped_received_seq(&key) else {
                continue;
            };
            skipped_last = Some(skipped_last.map_or(seq, |last| last.max(seq)));
            if seq > current {
                seqs.push(seq);
            }
        }
        scanned == PREFIX_SCAN_LIMIT
    };

    seqs.sort_unstable();
    seqs.dedup();
    Ok(ConsumedScanWindow {
        seqs,
        inbox_truncated,
        skipped_truncated,
        inbox_last,
        skipped_last,
    })
}

/// 解析 `sync.skipped_received.<hex log_id>.<20 位序号>` 的序号。
///
/// 格式不认识就返回 `None`：不是本函数写下的键不参与前缀推进，宁可让前缀停在这里
/// （fail-safe），也不把来路不明的 meta 键当成"已消费"。
fn parse_skipped_received_seq(key: &str) -> Option<u64> {
    let rest = key.strip_prefix(META_SYNC_SKIPPED_RECEIVED_PREFIX)?;
    let (hex, digits) = rest.split_once('.')?;
    if hex.len() != 32 || digits.len() != 20 {
        return None;
    }
    if !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// `sync.skipped_received.<hex log_id>.<seq 20 位>`。
pub(crate) fn skipped_received_key(log_id: &[u8], relay_seq: u64) -> Result<String, StorageError> {
    if log_id.len() != 16 {
        return Err(StorageError::Corrupt {
            context: format!("sync.log_id 长度不是 16：{}", log_id.len()),
        });
    }
    let mut hex = String::with_capacity(32);
    for byte in log_id {
        use core::fmt::Write as _;
        write!(hex, "{byte:02x}").map_err(|_| StorageError::Corrupt {
            context: "sync.log_id 十六进制编码失败".to_owned(),
        })?;
    }
    Ok(format!(
        "{META_SYNC_SKIPPED_RECEIVED_PREFIX}{hex}.{relay_seq:020}"
    ))
}

/// `meta.value` 的**唯一**解读口径：只认 BLOB，其它 SQLite 类型一律是损坏。
///
/// 三个读取入口共享本函数，避免"同一份库读出两套结论"漂移：
///
/// - [`crate::Vault::get_meta`]（actor 线程上的命令）；
/// - `sync_bridge::read_meta`（同步桥经 Vault 命令读取）；
/// - 本模块的 [`read_meta_on`]（worker 独占连接 / 事务内的批量 inbox 校验）。
///
/// 为什么不把其它类型「当字节」或「当缺失」：
///
/// - **当缺失会静默绕过持久屏障**：`sync.repack_required`（轮换后必须重打包）、
///   `sync.log_id`（incarnation 绑定）、`sync.received_seq` 这类键"读不到"就等价于
///   "没有屏障/没有游标"，把一处损坏放大成绕过安全前置条件；
/// - **当字节与 schema 相悖**：`meta.value` 是 `BLOB NOT NULL`（STRICT 表），能读出
///   TEXT/INTEGER/REAL 本身就说明库被外部改坏或 schema 漂移，继续按字节解释只会让
///   损坏值流入上层语义。
///
/// 正常写入路径写不出非 BLOB（STRICT 表会直接拒绝），所以这里命中的都是
/// "库被外部改坏"的场景：宁可 fail-closed 报损坏，也不猜。
pub(crate) fn meta_bytes_of(key: &str, value: Value) -> Result<Vec<u8>, StorageError> {
    match value {
        Value::Blob(bytes) => Ok(bytes),
        other => Err(StorageError::Corrupt {
            context: format!(
                "meta[{key}] 应为 BLOB，实际 {}（库被外部改坏或 schema 漂移）",
                sqlite_type_name(&other)
            ),
        }),
    }
}

/// SQLite 值的类型名（只用于错误信息）。
fn sqlite_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "NULL",
        Value::Integer(_) => "INTEGER",
        Value::Real(_) => "REAL",
        Value::Text(_) => "TEXT",
        Value::Blob(_) => "BLOB",
    }
}

/// 在给定连接（或事务）上读一条 meta；键不存在返回 `None`。
///
/// 类型判定走 [`meta_bytes_of`]：只认 BLOB，其它类型是 [`StorageError::Corrupt`]
/// （不再静默当作缺失）。
pub(crate) fn read_meta_on(conn: &Connection, key: &str) -> Result<Option<Vec<u8>>, StorageError> {
    let mut statement = conn.prepare("SELECT value FROM meta WHERE key=?1")?;
    let mut rows = statement.query([key])?;
    match rows.next()? {
        None => Ok(None),
        Some(row) => Ok(Some(meta_bytes_of(key, row.get::<usize, Value>(0)?)?)),
    }
}

fn read_meta_u64_on(conn: &Connection, key: &str) -> Result<Option<u64>, StorageError> {
    let Some(bytes) = read_meta_on(conn, key)? else {
        return Ok(None);
    };
    if bytes.len() != 8 {
        return Err(StorageError::Corrupt {
            context: format!("{key} 应为 BE64 8B，实际 {}B", bytes.len()),
        });
    }
    Ok(Some(u64::from_be_bytes(bytes.try_into().expect("长度8"))))
}

fn write_meta_on(conn: &Connection, key: &str, value: &[u8]) -> Result<(), StorageError> {
    conn.execute(
        "INSERT INTO meta(key,value) VALUES(?1,?2) \
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        rusqlite::params![key, value],
    )?;
    Ok(())
}

fn array16_bytes(bytes: Vec<u8>, field: &str) -> Result<[u8; 16], StorageError> {
    bytes
        .try_into()
        .map_err(|bytes: Vec<u8>| StorageError::Corrupt {
            context: format!("{field} 应为 16B，实际 {}B", bytes.len()),
        })
}

fn i64_of(value: u64) -> Result<i64, StorageError> {
    i64::try_from(value).map_err(|_| StorageError::Corrupt {
        context: format!("数值 {value} 超出 SQLite INTEGER"),
    })
}

`````



@@SECTION code/075 `crates/storage/src/actor.rs`


`````rust
//! 单写者 actor：一个专用线程独占 `rusqlite::Connection`。
//!
//! 为什么需要它：
//!
//! - `rusqlite::Connection` 不是 `Sync`，多个线程各持一个连接写同一个文件时，
//!   SQLite 的写锁会退化成 `SQLITE_BUSY` 竞争；
//! - ADR 0001 §3.9 要求「每 vault 单 writer actor」。
//!
//! 实现方式：所有读写都变成消息排到同一个线程上顺序执行，外部通过 `mpsc` 发命令、
//! 通过容量为 1 的 `sync_channel` 收结果（等价 oneshot）。`Connection` 在工作线程内
//! 创建、也在工作线程内析构，**绝不跨线程移动**，因此关闭连接时的 WAL checkpoint
//! 也发生在它自己的线程上。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};

use rusqlite::types::Value;
use rusqlite::{Connection, Transaction, TransactionBehavior};

use crate::connection;
use crate::error::StorageError;
use crate::migrate;
use crate::sync_meta::{self, ConsumedPrefix};

/// 查询结果中的一行：按 SQL 的列顺序排列的值。
pub type Row = Vec<Value>;

/// 单次请求的回复通道：容量 1，等价 oneshot。
type Reply<T> = SyncSender<Result<T, StorageError>>;

/// 事务里的一条语句。
#[derive(Debug, Clone)]
pub struct Statement {
    /// SQL 文本。
    pub sql: String,
    /// 绑定参数。
    pub params: Vec<Value>,
}

impl Statement {
    /// 构造一条语句。
    pub fn new(sql: impl Into<String>, params: Vec<Value>) -> Self {
        Self {
            sql: sql.into(),
            params,
        }
    }
}

/// 「这条语句必须影响 N 行」的事务守卫；不满足则整笔事务回滚。
///
/// 用来把 CAS 变成原子语义：例如 `UPDATE loro_docs ... WHERE head_seq = ?` 必须影响 1 行，
/// 否则说明 head 已被别人推进，此时**不能**提交本批 update。
#[derive(Debug, Clone, Copy)]
pub struct AffectedGuard {
    /// 语句下标（0 起）。
    pub statement: usize,
    /// 期望影响的行数。
    pub expected: usize,
}

impl AffectedGuard {
    /// 要求第 `statement` 条语句恰好影响 `expected` 行。
    pub fn exactly(statement: usize, expected: usize) -> Self {
        Self {
            statement,
            expected,
        }
    }
}

/// 批量 inbox 原子写入的一行。
///
/// 由 `sync_bridge::store_inbox_verified_batch` 构造，送到 worker 线程后在同一事务里
/// 完成「既有行比对 + 新行插入 + `received` 连续前缀推进」；字段含义与
/// [`crate::VerifiedInboxRow`] 一一对应，只是在 actor 层不依赖 sync 语义。
#[derive(Debug, Clone)]
pub(crate) struct InboxBatchRow {
    /// relay 日志 incarnation（同一批必须一致）。
    pub(crate) log_id: [u8; 16],
    /// relay 日志序号（必须 > 0；同一批内不得重复）。
    pub(crate) relay_seq: u64,
    /// 密文对象名（32B）。
    pub(crate) object_id: [u8; 32],
    /// 加密 epoch（必须 > 0）。
    pub(crate) epoch: u64,
    /// 已解密并完成载荷校验的 `UpdateV1` 明文。
    pub(crate) verified_payload: Vec<u8>,
}

/// 工作线程命令。
enum Command {
    /// 执行单条写语句，返回受影响行数。
    Exec {
        /// SQL 文本。
        sql: String,
        /// 绑定参数。
        params: Vec<Value>,
        /// 结果回传通道。
        reply: Reply<usize>,
    },
    /// 执行多条语句（DDL / PRAGMA / 测试夹具），不关心行数。
    ExecBatch {
        /// SQL 文本。
        sql: String,
        /// 结果回传通道。
        reply: Reply<()>,
    },
    /// 执行单条查询，返回全部行。
    Query {
        /// SQL 文本。
        sql: String,
        /// 绑定参数。
        params: Vec<Value>,
        /// 结果回传通道。
        reply: Reply<Vec<Row>>,
    },
    /// 在**同一个事务**里执行多条语句，并按守卫校验影响行数。
    ExecTransaction {
        /// 语句列表。
        statements: Vec<Statement>,
        /// 守卫列表。
        guards: Vec<AffectedGuard>,
        /// 结果回传通道：每条语句的影响行数。
        reply: Reply<Vec<usize>>,
    },
    /// 读取一条 meta（键值，BLOB）。
    GetMeta {
        /// 键（命名空间由调用方约定，storage 不理解语义）。
        key: String,
        /// 结果回传通道。
        reply: Reply<Option<Vec<u8>>>,
    },
    /// 读取 `PRAGMA user_version`。
    UserVersion {
        /// 结果回传通道。
        reply: Reply<i64>,
    },
    /// 原子批量写 inbox：校验、既有行冲突检测、新行插入、`received` 连续前缀推进
    /// 全部在**同一个事务**里完成。
    ExecInboxBatch {
        /// 待写入行（worker 内重新校验/排序）。
        rows: Vec<InboxBatchRow>,
        /// 结果回传通道：本次真正新插入的行数（幂等命中不计）。
        reply: Reply<usize>,
    },
    /// 在同一条命令里推进连续前缀（读窗口 + 一次写最终值都在 worker 连接上完成）。
    AdvanceConsumedPrefix {
        /// 要推进的前缀（received / integrated）。
        prefix: ConsumedPrefix,
        /// 结果回传通道。
        reply: Reply<()>,
    },
}

/// vault 数据库句柄，克隆后共享同一个工作线程。
///
/// 它是 `Send + Sync`，可以跨线程共享；所有操作串行落在工作线程上。
#[derive(Debug, Clone)]
pub struct Vault {
    /// 共享状态，最后一个句柄析构时触发优雅关闭。
    inner: Arc<Shared>,
}

/// 句柄之间共享的状态。
#[derive(Debug)]
pub(crate) struct Shared {
    /// 命令发送端；关闭时取走并丢弃，工作线程的 `recv` 随之返回错误。
    tx: Mutex<Option<Sender<Command>>>,
    /// 工作线程句柄，只在最后一个句柄析构时 join。
    worker: Mutex<Option<JoinHandle<()>>>,
    /// 数据库文件路径。
    path: PathBuf,
    /// 变更通知的订阅者槽位（`(id, sender)`）。
    pub(crate) event_slots: Mutex<Vec<crate::events::EventSlot>>,
    /// 分配订阅者 id 用。
    pub(crate) next_event_slot: std::sync::atomic::AtomicU64,
    /// 「事件送不出去、请做全量对账」的粘性标记。
    pub(crate) reconcile_required: std::sync::atomic::AtomicBool,
}

impl Vault {
    /// 打开（或创建）给定路径的 vault 数据库。
    ///
    /// 内部依次完成：
    ///
    /// 1. 父目录不存在则创建；
    /// 2. 在工作线程打开连接；
    /// 3. 应用并读回校验 PRAGMA（WAL / FULL / foreign_keys / busy_timeout）；
    /// 4. 迁移到最新 schema。
    ///
    /// 只有全部成功才返回 `Ok`；失败时工作线程已被回收，不会泄漏。
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }

        let (tx, rx) = mpsc::channel::<Command>();
        // 初始化握手：工作线程把「打开 + 配置 + 迁移」的结果回传，成功后才交出句柄。
        let (init_tx, init_rx) = mpsc::sync_channel::<Result<(), StorageError>>(1);

        let worker_path = path.clone();
        let handle = thread::Builder::new()
            .name("menghunote-storage".to_owned())
            .spawn(move || worker_main(&worker_path, &rx, init_tx))?;

        match init_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                inner: Arc::new(Shared {
                    tx: Mutex::new(Some(tx)),
                    worker: Mutex::new(Some(handle)),
                    path,
                    event_slots: crate::events::new_slots(),
                    next_event_slot: std::sync::atomic::AtomicU64::new(0),
                    reconcile_required: std::sync::atomic::AtomicBool::new(false),
                }),
            }),
            Ok(Err(error)) => {
                // 初始化失败：关掉通道并等线程退出，再把手上的错误原样上报。
                drop(tx);
                let _ = handle.join();
                Err(error)
            }
            Err(_) => {
                // 工作线程没回消息就没了（panic 或连接都放不下），按通道失效处理。
                drop(tx);
                let _ = handle.join();
                Err(StorageError::ChannelClosed {
                    context: "等待工作线程初始化",
                })
            }
        }
    }

    /// vault 数据库文件路径。
    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    /// 打开一条**只读快照**连接（WAL 下与写路径并发，不占写 actor）。
    ///
    /// 语义见 [`crate::ReadSnapshot`]：只保证读到"已提交"的数据；忙则返回
    /// [`StorageError::SnapshotBusy`]，不做无限重试。
    pub fn open_read_snapshot(&self) -> Result<crate::ReadSnapshot, StorageError> {
        crate::ReadSnapshot::open(&self.inner.path)
    }

    /// 订阅轻量变更事件（容量 = 队列上限）。
    ///
    /// 发布走 `try_send`：队列满 / 无订阅者时不会阻塞写，而是置"需要全量对账"标记。
    pub fn subscribe_events(&self, capacity: usize) -> crate::StorageEventSubscriber {
        self.inner.subscribe_events(capacity)
    }

    /// 取走并清除"事件没送到、请做全量对账"的标记。
    pub fn take_reconcile_required(&self) -> bool {
        self.inner.take_reconcile_required()
    }

    /// 读一条 meta（键值 BLOB）；键不存在返回 `None`。
    ///
    /// 判定与另两个入口共享同一个 helper（`sync_meta::meta_bytes_of`）：**只认 BLOB**，
    /// TEXT/INTEGER/REAL 都是 [`StorageError::Corrupt`]，不会静默当成"没有这条 meta"
    /// ——否则 `sync.repack_required` / `sync.log_id` 之类持久屏障会被绕过。
    pub fn get_meta(&self, key: &str) -> Result<Option<Vec<u8>>, StorageError> {
        self.call(|reply| Command::GetMeta {
            key: key.to_owned(),
            reply,
        })
    }

    /// 写一条 meta：**走写 actor 的小事务**（`INSERT ... ON CONFLICT`），不绕过 actor。
    pub fn set_meta(&self, key: &str, value: &[u8]) -> Result<(), StorageError> {
        self.exec_transaction(
            &[Statement::new(
                "INSERT INTO meta(key, value) VALUES(?1, ?2) \
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                vec![Value::Text(key.to_owned()), Value::Blob(value.to_vec())],
            )],
            &[AffectedGuard::exactly(0, 1)],
        )?;
        Ok(())
    }

    /// 发布一条变更事件（内部用；B 物化与结构投影提交后调用）。
    ///
    /// 返回成功投递的订阅者数量；`0` 说明没人收到，已置"需要全量对账"标记。
    pub(crate) fn publish_event(&self, event: crate::StorageEvent) -> usize {
        self.inner.publish_event(event)
    }

    /// 当前 schema 版本（`PRAGMA user_version`）。
    pub fn user_version(&self) -> Result<i64, StorageError> {
        self.call(|reply| Command::UserVersion { reply })
    }

    /// 低层写命令：执行单条 SQL，返回受影响行数。
    ///
    /// 只用于写语句；会把结果集返回给调用方的语句请用 [`Vault::query`]。
    /// 参数用 [`Value`] 传递，调用方负责别把用户输入拼进 SQL。
    pub fn exec(&self, sql: &str, params: &[Value]) -> Result<usize, StorageError> {
        self.call(|reply| Command::Exec {
            sql: sql.to_owned(),
            params: params.to_vec(),
            reply,
        })
    }

    /// 低层批处理：执行多条语句（DDL、PRAGMA、测试夹具等），全部成功才返回 `Ok`。
    pub fn exec_batch(&self, sql: &str) -> Result<(), StorageError> {
        self.call(|reply| Command::ExecBatch {
            sql: sql.to_owned(),
            reply,
        })
    }

    /// 低层读命令：执行单条查询，返回全部行。
    pub fn query(&self, sql: &str, params: &[Value]) -> Result<Vec<Row>, StorageError> {
        self.call(|reply| Command::Query {
            sql: sql.to_owned(),
            params: params.to_vec(),
            reply,
        })
    }

    /// 低层事务命令：多条语句在同一事务里执行，`guards` 不满足则整笔回滚。
    ///
    /// 返回每条语句的影响行数（顺序与 `statements` 一致）。
    ///
    /// 事务以 `BEGIN IMMEDIATE` 显式开启（与批量 inbox 入口共用
    /// [`begin_immediate`]）：写锁在事务开始就取，并发写者按 `busy_timeout` 排队，
    /// 而不是在第一条语句处撞上"锁升级不走 busy handler"的立即失败。
    pub fn exec_transaction(
        &self,
        statements: &[Statement],
        guards: &[AffectedGuard],
    ) -> Result<Vec<usize>, StorageError> {
        self.call(|reply| Command::ExecTransaction {
            statements: statements.to_vec(),
            guards: guards.to_vec(),
            reply,
        })
    }

    /// 原子批量写 inbox（storage 内部入口；外部请走
    /// [`crate::sync_bridge::store_inbox_verified_batch`]）。
    ///
    /// 空批次是合法 no-op（与逐条入口一致），不会打开事务。
    pub(crate) fn exec_inbox_batch(&self, rows: Vec<InboxBatchRow>) -> Result<usize, StorageError> {
        if rows.is_empty() {
            return Ok(0);
        }
        self.call(|reply| Command::ExecInboxBatch { rows, reply })
    }

    /// 推进连续前缀（`received` / `integrated`），同一条命令内完成读窗口与写最终值。
    pub(crate) fn advance_consumed_prefix(
        &self,
        prefix: ConsumedPrefix,
    ) -> Result<(), StorageError> {
        self.call(|reply| Command::AdvanceConsumedPrefix { prefix, reply })
    }

    /// 把一条命令发给工作线程并等待回复。
    fn call<T>(&self, make: impl FnOnce(Reply<T>) -> Command) -> Result<T, StorageError> {
        let (reply_tx, reply_rx) = mpsc::sync_channel::<Result<T, StorageError>>(1);
        {
            // 只锁住"取发送端 + 投递命令"这一段，等回复时不持锁。
            let guard = lock(&self.inner.tx);
            let Some(tx) = guard.as_ref() else {
                return Err(StorageError::ChannelClosed {
                    context: "提交命令，工作线程已关闭",
                });
            };
            tx.send(make(reply_tx))
                .map_err(|_| StorageError::ChannelClosed {
                    context: "提交命令，工作线程已退出",
                })?;
        }
        reply_rx.recv().map_err(|_| StorageError::ChannelClosed {
            context: "等待命令回复，工作线程已退出",
        })?
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        // 丢弃发送端：工作线程的 recv 立刻返回错误，退出循环并在自己的线程上关闭连接
        // （关闭时 SQLite 会做 WAL checkpoint）。Drop 里只做清理，绝不 panic。
        drop(lock(&self.tx).take());
        if let Some(worker) = lock(&self.worker).take() {
            // 工作线程若已 panic，join 返回 Err；这里吞掉，避免析构路径上二次 panic。
            let _ = worker.join();
        }
    }
}

/// 工作线程入口：独占连接，顺序处理命令。
fn worker_main(path: &Path, rx: &Receiver<Command>, init: Reply<()>) {
    let conn = match connection::open_and_migrate(path) {
        Ok(conn) => conn,
        Err(error) => {
            let _ = init.send(Err(error));
            return;
        }
    };

    if init.send(Ok(())).is_err() {
        // 主线程已放弃初始化；连接在这里随函数返回于本线程析构。
        return;
    }

    while let Ok(command) = rx.recv() {
        handle_command(&conn, command);
    }
}

/// 执行一条命令并回传结果；调用方可能已经走了，回复失败只忽略。
fn handle_command(conn: &Connection, command: Command) {
    match command {
        Command::Exec { sql, params, reply } => {
            let _ = reply.send(exec(conn, &sql, &params));
        }
        Command::ExecBatch { sql, reply } => {
            let _ = reply.send(conn.execute_batch(&sql).map_err(StorageError::from));
        }
        Command::Query { sql, params, reply } => {
            let _ = reply.send(query(conn, &sql, &params));
        }
        Command::ExecTransaction {
            statements,
            guards,
            reply,
        } => {
            let _ = reply.send(exec_transaction(conn, &statements, &guards));
        }
        Command::GetMeta { key, reply } => {
            // 与 sync_bridge / sync_meta 同一个读取 helper（只认 BLOB，其它类型报损坏）。
            let _ = reply.send(sync_meta::read_meta_on(conn, &key));
        }
        Command::UserVersion { reply } => {
            let _ = reply.send(migrate::user_version(conn));
        }
        Command::ExecInboxBatch { rows, reply } => {
            let _ = reply.send(exec_inbox_batch(conn, rows));
        }
        Command::AdvanceConsumedPrefix { prefix, reply } => {
            let _ = reply.send(sync_meta::advance_consumed_prefix_on(conn, prefix));
        }
    }
}

/// 在连接上显式开启一个 **Immediate** 写事务（`BEGIN IMMEDIATE`）。
///
/// 为什么不用 deferred（`conn.unchecked_transaction()` 的默认行为）：
///
/// - deferred 的 `BEGIN` 只是登记，第一条 SELECT 拿共享锁，等到第一条写语句才尝试把
///   共享锁升级成写锁；而 SQLite 的锁升级**不走 busy handler**——此刻若已有别的写事务
///   在途，不会用 `busy_timeout` 等待，而是直接返回 `SQLITE_BUSY`，让调用方在"最多等
///   5s"的预期之外立刻失败；
/// - `BEGIN IMMEDIATE` 在事务开始就取写锁，这一步**照常**受 `busy_timeout` 约束，与
///   ADR 0001 §3.9「每 vault 单 writer actor」的模型一致：写事务不该和别的写事务撞在
///   升级点上。
///
/// 两个写事务入口都走这里，论证只写在这一处（避免注释在两个调用点各改一半而漂移）：
///
/// - [`exec_inbox_batch`]：先读后写，deferred 会在锁升级处失败，且读到的旧快照还会先
///   报出误导性的 `SyncConflict`；
/// - [`exec_transaction`]：承载 A 事务/资产/repack/finish，调用方习惯把写语句放最前，
///   但**没有任何断言**保证这一点（以后多一条只读语句就退化成升级路径），所以同样
///   显式 Immediate。
fn begin_immediate(conn: &Connection) -> Result<Transaction<'_>, StorageError> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    Ok(tx)
}

/// 批量 inbox 的原子执行。
///
/// 全部动作在 worker 持有独占连接上的**同一个事务**里：
///
/// 1. 校验批内行：同 `log_id`、`relay_seq/epoch > 0`、`relay_seq` 不重复；按序号排序；
/// 2. 校验 `sync.log_id`（不存在则写，存在不同则 [`StorageError::SyncLogMismatch`]）；
/// 3. 读该序号范围既有行，逐行比较 `object_id/epoch/verified_payload`：
///    - 完全相同 → 幂等跳过（不插入、不触发任何守卫）；
///    - 任意不同 → [`StorageError::SyncConflict`]，`tx` 在此 drop，整笔回滚（一行不写）；
/// 4. 新行在同一事务插入（每行要求恰好影响 1 行）；
/// 5. **同一事务内**推进 `received` 连续前缀（与普通路径同一套扫描语义）；
/// 6. 提交后返回新插入行数（幂等行不计）。
///
/// 先读后写的竞态因此被消除：快照与插入属于同一个写事务（SQLite 的单写者锁），
/// 且一次提交只产生一次 fsync；崩溃/冲突只会留下「什么都没发生」或「行+前缀一起提交」。
///
/// 事务用 [`begin_immediate`] 显式开启（而不是 deferred）：本事务先读后写，deferred
/// 的锁升级不走 busy handler，完整论证见该函数。
fn exec_inbox_batch(
    conn: &Connection,
    mut rows: Vec<InboxBatchRow>,
) -> Result<usize, StorageError> {
    if rows.is_empty() {
        return Ok(0);
    }
    let log_id = rows[0].log_id;
    for row in &rows {
        if row.log_id != log_id {
            return Err(StorageError::Corrupt {
                context: "批量 inbox 行必须属于同一 log_id".to_owned(),
            });
        }
        if row.relay_seq == 0 || row.epoch == 0 {
            return Err(StorageError::Corrupt {
                context: "inbox relay_seq/epoch 必须 >0".to_owned(),
            });
        }
    }
    rows.sort_by_key(|row| row.relay_seq);
    if rows
        .windows(2)
        .any(|pair| pair[0].relay_seq == pair[1].relay_seq)
    {
        return Err(StorageError::Corrupt {
            context: "批量 inbox 出现重复 relay_seq".to_owned(),
        });
    }

    // 事务模式与锁升级论证收在 [`begin_immediate`]：本事务先读后写，deferred 会在锁
    // 升级处立即 SQLITE_BUSY（见该函数的注释）。
    let tx = begin_immediate(conn)?;
    sync_meta::ensure_log_id_on(&tx, &log_id)?;

    let first_seq = rows.first().map_or(0, |row| row.relay_seq);
    let last_seq = rows.last().map_or(0, |row| row.relay_seq);
    let existing = read_existing_inbox(&tx, &log_id, first_seq, last_seq)?;
    let mut fresh: Vec<&InboxBatchRow> = Vec::with_capacity(rows.len());
    for row in &rows {
        match existing.get(&row.relay_seq) {
            Some((object_id, epoch, payload)) => {
                if *object_id != row.object_id
                    || *epoch != row.epoch
                    || *payload != row.verified_payload
                {
                    // 不 commit：tx 在此 drop，SQLite 自动回滚整笔事务（包括已插入的新行）。
                    return Err(StorageError::SyncConflict {
                        context: format!("inbox {log_id:?}/{} 字节不一致", row.relay_seq),
                    });
                }
            }
            None => fresh.push(row),
        }
    }

    if !fresh.is_empty() {
        insert_inbox_rows(&tx, &log_id, &fresh)?;
    }

    // 与插入处于同一事务：提交后 received 一定覆盖本批新行；崩溃/冲突则一起回滚。
    sync_meta::advance_consumed_prefix_on(&tx, ConsumedPrefix::Received)?;
    tx.commit()?;
    Ok(fresh.len())
}

/// 既有 inbox 行的比较字段：`(object_id, epoch, verified_payload)`。
type ExistingInboxFacts = ([u8; 32], u64, Vec<u8>);

/// 读 `[first_seq, last_seq]` 范围内的既有 inbox 行，按序号建索引。
fn read_existing_inbox(
    conn: &Connection,
    log_id: &[u8; 16],
    first_seq: u64,
    last_seq: u64,
) -> Result<BTreeMap<u64, ExistingInboxFacts>, StorageError> {
    let mut statement = conn.prepare(
        "SELECT relay_seq,object_id,epoch,verified_payload FROM sync_inbox \
         WHERE log_id=?1 AND relay_seq>=?2 AND relay_seq<=?3",
    )?;
    let mut rows = statement.query(rusqlite::params_from_iter([
        Value::Blob(log_id.to_vec()),
        Value::Integer(i64_of(first_seq)?),
        Value::Integer(i64_of(last_seq)?),
    ]))?;
    let mut existing = BTreeMap::new();
    while let Some(row) = rows.next()? {
        let seq: i64 = row.get(0)?;
        let seq = u64::try_from(seq).map_err(|_| StorageError::Corrupt {
            context: "sync_inbox.relay_seq 为负数".to_owned(),
        })?;
        let object_id: Vec<u8> = row.get(1)?;
        let object_id = object_id
            .try_into()
            .map_err(|bytes: Vec<u8>| StorageError::Corrupt {
                context: format!("sync_inbox.object_id 应为 32B，实际 {}B", bytes.len()),
            })?;
        let epoch: i64 = row.get(2)?;
        let epoch = u64::try_from(epoch).map_err(|_| StorageError::Corrupt {
            context: "sync_inbox.epoch 为负数".to_owned(),
        })?;
        let payload: Option<Vec<u8>> = row.get(3)?;
        existing.insert(seq, (object_id, epoch, payload.unwrap_or_default()));
    }
    Ok(existing)
}

/// 在同一事务里插入新行；每行都要求恰好影响 1 行，否则整笔回滚。
fn insert_inbox_rows(
    conn: &Connection,
    log_id: &[u8; 16],
    rows: &[&InboxBatchRow],
) -> Result<(), StorageError> {
    let mut statement = conn.prepare(
        "INSERT INTO sync_inbox(log_id,relay_seq,object_id,epoch,state,verified_payload) \
         VALUES(?1,?2,?3,?4,'verified',?5)",
    )?;
    for row in rows {
        let affected = statement.execute(rusqlite::params_from_iter([
            Value::Blob(log_id.to_vec()),
            Value::Integer(i64_of(row.relay_seq)?),
            Value::Blob(row.object_id.to_vec()),
            Value::Integer(i64_of(row.epoch)?),
            Value::Blob(row.verified_payload.clone()),
        ]))?;
        if affected != 1 {
            return Err(StorageError::SyncConflict {
                context: format!("inbox {log_id:?}/{} 写入冲突", row.relay_seq),
            });
        }
    }
    Ok(())
}

/// SQLite INTEGER（有符号 64 位）到 u64 的转换；负数说明库/入参不自洽。
fn i64_of(value: u64) -> Result<i64, StorageError> {
    i64::try_from(value).map_err(|_| StorageError::Corrupt {
        context: format!("数值 {value} 超出 SQLite INTEGER"),
    })
}

/// 执行单条写语句。
fn exec(conn: &Connection, sql: &str, params: &[Value]) -> Result<usize, StorageError> {
    let mut statement = conn.prepare(sql)?;
    Ok(statement.execute(rusqlite::params_from_iter(params))?)
}

/// 执行单条查询，把每一行转成 [`Row`]。
fn query(conn: &Connection, sql: &str, params: &[Value]) -> Result<Vec<Row>, StorageError> {
    let mut statement = conn.prepare(sql)?;
    let column_count = statement.column_count();
    let mut rows = statement.query(rusqlite::params_from_iter(params))?;

    let mut result = Vec::new();
    while let Some(row) = rows.next()? {
        let mut values = Vec::with_capacity(column_count);
        for index in 0..column_count {
            values.push(row.get::<_, Value>(index)?);
        }
        result.push(values);
    }
    Ok(result)
}

/// 在一个事务里执行多条语句；任一语句报错或守卫不满足 → 整笔回滚。
fn exec_transaction(
    conn: &Connection,
    statements: &[Statement],
    guards: &[AffectedGuard],
) -> Result<Vec<usize>, StorageError> {
    // 连接由工作线程独占，不会有嵌套事务；事务模式与批量 inbox 共用 begin_immediate
    // （锁升级论证见该函数），语句顺序/守卫/CAS 语义都不变。
    let tx = begin_immediate(conn)?;

    // 守卫必须在**该语句执行后立刻**校验：否则后续语句可能先撞 UNIQUE/FK，
    // 把「head 被人推进了」这个真正原因盖掉（CAS 语义要求先于任何写入冲突暴露）。
    for guard in guards {
        if guard.statement >= statements.len() {
            return Err(StorageError::GuardFailed {
                statement: guard.statement,
                expected: guard.expected,
                actual: 0,
            });
        }
    }

    let mut affected = Vec::with_capacity(statements.len());
    for (index, statement) in statements.iter().enumerate() {
        let mut prepared = tx.prepare(&statement.sql)?;
        let rows = prepared.execute(rusqlite::params_from_iter(&statement.params))?;
        affected.push(rows);

        for guard in guards.iter().filter(|guard| guard.statement == index) {
            if rows != guard.expected {
                // 不 commit：tx 在此 drop，SQLite 自动回滚整笔事务
                return Err(StorageError::GuardFailed {
                    statement: index,
                    expected: guard.expected,
                    actual: rows,
                });
            }
        }
    }

    tx.commit()?;
    Ok(affected)
}

/// 获取互斥锁；即使锁被毒化也继续使用内部数据（清理路径不允许 panic）。
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

`````



@@SECTION code/076 `crates/storage/src/body_doc.rs`


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



@@SECTION code/080 `crates/storage/src/session/flush.rs`


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



@@SECTION code/092 `crates/storage/tests/session_flush.rs`


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



@@SECTION code/093 `crates/storage/tests/body_doc_detached_oplog.rs`


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


