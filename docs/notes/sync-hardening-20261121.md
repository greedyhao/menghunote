# 同步硬化修复记录（2026-11-21）

基线：`f8b02e1`（代码基线 `5d01e9e`）。本轮只做 Phase 1A 收尾硬化；
**未改 relay / sync-proto wire / SQLite DDL / checkpoint 签名 / 游标连续语义**。

## 1. P1 状态

| 项 | 状态 | 提交 | 测试 |
| --- | --- | --- | --- |
| P1-1 finish 完整性 | 完成 | `2ed485b` | `finish_without_all_docs_is_conflict`、`finish_after_full_batches_is_applied`、`begin_idempotent_replay_keeps_expected_set`、`finish_without_all_assets_is_conflict` |
| P1-2 tombstone TOCTOU | 完成（SQL 事务内重判） | `2ed485b` | `tombstone_outbox_race_between_decision_and_transaction_keeps_note` |
| P1-3 Journal HeadConflict | 完成（生产已接线；不是 JournalWriter rebuild 路径，见下） | `aba4375` + `3c5ddcf` | `flush_recovers_from_head_conflict_by_rebuilding_sink`、`flush_reports_unrecoverable_after_bounded_rebuild`、`flush_reports_unrecoverable_when_new_head_is_unreadable` |
| P1-4 ready/count log_id | 完成 | `be5244b` | `ready_and_count_only_include_current_log_incarnation`、`ready_where_path_filters_other_log_incarnation`、`ready_without_log_id_is_empty_and_invalid_length_is_corrupt` |

> **P1-3 最终口径（2026-11-21 晚）**：生产 `session/flush.rs` 已接线，但走的**不是**
> `JournalHeadStale` / `JournalWriter::rebuild_sink`：它的 `HeadConflict` 路径是
> `load_head` → 仅当新 head 的 ops ⊆ 未 ACK 前缀 `end_vv` 时同字节重试一次；
> 未知 ops fail-closed 要求重开；仍单后台 writer、无第二 journal、不重新导出增量。
> 失败语义与恢复路径以 `crates/storage/src/journal.rs` 模块头“两条恢复路径”为准。

### P1-1 细节

- `begin_baseline_apply` 增加 `BaselineExpectation { docs, assets }`，与 journal 同事务写 `sync.baseline_expected`（本地 `meta`，非 DDL）。
- `finish` 在任何 meta 写入前逐项比对 `baseline_apply_docs.applied=1` / `baseline_apply_assets.applied=1`；缺失/多出/集合不符 → `Conflict`，零写。
- `finishable` 改为 `staging && expected 全齐 && 无 verify 失败标记`；无记录不可 finish。
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
| HeadConflict 有界、不覆盖 head/丢数据 | P1-3 兼容 head 有界重试 + 未知 ops fail-closed；P2-5 值 CAS 回滚 |
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
cargo test --workspace                             2926 passed / 0 failed
cargo test --workspace --features menghunote-app/dev-sync 3077 passed / 0 failed
```

> **最终复跑（2026-11-21 评审收尾）**：review 后新增 P1 门禁/访问器/资产批测试，当前计数
> workspace **2926 / 0**、`--features menghunote-app/dev-sync` **3077 / 0**；
> storage **309** / sync **641** / app **740**。
> 上面的 2926 / 3077 是本文件收尾时点的批次；同轮后续新增测试已计入
> 按批次引用，不要把增量当成回归。scan 探针的批次口径：最终 release 复跑
> **0.428 / 4.039 ms** 与 `sync-phase0-metrics.md` §8.7.4 的初次 3-run
> **0.479 / 3.796 ms** 是两批，不要混算（详见该文件 §8.7.6）。

一次环境假失败：共享 `target` 中旧 `gpui-excalidraw` build script 烘入了已删除的 `/home/dev/menghunote-p2check`；`cargo clean -p gpui-excalidraw` 后 app/workspace 全绿，与本轮改动无关。

## 6. 需要用户决策 / 残余

1. **P1-3 生产接线已完成，UI 救济仍缺**：`session/flush.rs` 仍直接使用 `VaultJournalSink`，采用兼容 head 的同字节有界重试，不是 `JournalWriter` 的重新导出；未知外部 ops fail-closed 要求重开。`DocumentSession`/`SessionDriver::last_write_error` 与 `CloseReport.flush_error` 已让非持闸调用方读到原因，但 App 的“导出/复制 live 正文 → 重开”流程仍未接。
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
2. **失败标记门禁已下沉（2026-11-21 review 后补）**：`apply_baseline_batch`、`apply_baseline_assets` 和
   `finish_baseline_apply` 都会在解码/任何写之前检查 `sync.baseline_verify_failed`，有标记
   返回带原因的 `SyncConflict`，零写；`state_from_journal` 的 `finishable` 在标记存在时
   为 false（`staging && expected 全齐 && !verify_failed`）。因此 Phase1B 不能靠"调用方
   自觉先走 begin"来绕过失败 journal。`baseline_apply.status` 仍只有 staging/completed，
   失败只能通过 `baseline_verify_failure` 读取。
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



## 9. 高级评审补充口径（2026-11-21）

1. **P1-3 表格不再写“部分”**：§1 表已改为最终状态；历史收尾批次与最终行为分开引用，后续评审以本表/§9 为准。
2. **abort 的两个边界已钉住**：无 staging journal 但 marker 残留时 `abort_baseline_apply` 返回 `false` 并清 marker
   （`abort_without_journal_clears_residual_marker`）；guarded 事务失败（`AffectedGuard::exactly(3,1)` 不命中）
   整体回滚、返回 `false`、marker 保留，调用方必须重试 abort
   （`abort_guard_failure_keeps_marker_for_retry`，用 `RAISE(IGNORE)` 触发器模拟并发窗口）。单 writer 下
   第二条不可达，但语义已固定；Phase 1B 并发化后不得把 `false` 当成功。
3. **`loro_updates` 行数不是 op 数**：HeadConflict 兼容重试可能把与外部行重叠的 ops 再落一行；Loro import
   幂等，`head_vv==live_vv` 仍成立，但“一行 = 一批逻辑写入”允许 op 重叠，**不能按行数推断 op 数/去重率**。
4. **coverage 扫描上限未标定**：`COVERAGE_SCAN_MAX_PAGES = 4096` ≈ 1M 事件 → 4096 次串行页请求；
   当前探针只到 25k（98 页），真实 RTT 项是 40× 外推，没有数据。机会性追赶启用前必须补 4096 页/真实
   RTT 标定，或把上限改成可配置并默认更小。
5. **`gc_floor` 两次请求间前进**：`load_baseline` 只给 `SyncError::Object("S 低于 GC floor")`；primary/fallback
   双双越界时的最终映射未测。Phase 1B 应映射为可执行 `Paused`（“等待 relay 保留/重新 bootstrap”），
   不能落成普通 Object/空转。
6. **`verify_ns` 线程本地**：跨线程/线程池调用 `verify_imported` 时，`gate_hold` 报告若在另一线程读
   `baseline_apply_verify_ms()` 会得到 0；Phase 1B 报告必须写明“同线程读取”，或在调用线程显式回传耗时。
7. **“强制追赶失败绝不回退普通 pull”的语义**：`gc_floor > received` 时普通 pull 只会 `CursorExpired`，
   回退没有功能意义；规则实质是**不得把 `Paused` 静默成空转循环**，必须把原因（游标过期/需重试/
   等待 relay）送到 UI/调度器。
8. **auto-abort 不得发生在持闸段**：`reconcile_journal` 的换基线 auto-abort 会写库（清 journal+marker）；
   Phase 1B `catchup_integrate` 持闸期间若走“换基线”分支等于持闸写两遍。设计上应显式禁止，并把
   marker/换基线映射成 `Paused` 理由，不在持闸内自动 abort 重试。

9. **M4 门禁已补（2026-11-21）**：结构文档 local_extra 生产路径
   (`structure_doc_local_extra_merges_local_and_baseline_nodes`)、SyncContext 重建
   (`flush_rebuild_keeps_sync_intent_seq_at_new_head_plus_one`)、普通/重建后 Retryable
   (`flush_retryable_write_failure_keeps_prefix_then_recovers`、
   `flush_rebuild_retryable_write_failure_keeps_prefix_then_recovers`)、二次 HeadConflict
   (`flush_reports_unrecoverable_on_second_head_conflict`)、换基线 auto-abort/rotation 拒绝
   (`reconcile_journal_auto_aborts_bootstrap_staging_and_clears_marker`、
   `reconcile_journal_refuses_rotation_staging_without_abort`)、损坏 marker
   (`corrupt_marker_is_fail_closed_and_abort_does_not_silently_succeed`)。
   head 回退分支无法用公开接口确定性构造（head_seq 只前进），已用二次冲突覆盖可构造方向。

10. **M3 标定已完成（2026-11-21）**：`large_doc_per_page_replay_cost` 测 1 MiB
    单条大 insert `load_body_doc` ≈1.975 ms（复跑 1.996），256 条 4 KiB 增量 ≈5.965 ms（复跑 5.322）；
    `checkpoint_validate_scan_cost` 真跑 S=1,048,576（4096 页，1 run），FakeRelay
    夹具 12.8 s（复跑 12.2 s）/峰值 RSS 202 MiB，visitor ≈33 ms。结论：per-page replay 是毫秒级，
    §10.4 的“~2 s/篇”折算不成立；45 s@20MiB 是 verify/attached merge，不是纯 replay。
    4096 页真实成本仍需按 `4096 × RTT + 33 ms` 标定，FakeRelay 不能外推；阈值未改。


11. **第二轮评审 P1 修补（2026-11-21）**：
    - `finish_baseline_apply` 的 marker 检查移到 `load_journal` 之前：无 journal 但残留/损坏
      marker 时不再先返回 `NotFound`；测试
      `finish_without_journal_rejects_residual_marker_before_not_found`。
    - `flush.rs` 将 `StorageError::Corrupt` 分类为永久失败，首次写和重建后重试都直接
      `Unrecoverable`，不再落无限 `Retryable`；测试
      `flush_treats_permanent_corrupt_as_unrecoverable_not_retryable`。
    - `baseline_apply` 的 `head_seq.saturating_add(1)` 改 `checked_add(1)`；现有 schema 的
      head_seq 来自 SQLite i64，checked_add 分支不可达，真正兜底是随后的 `i64_of(seq)`
      （返回 `Corrupt`）。保留 checked_add 是防御未来放宽到 u64，**不能**把它报告成
      “已验证可达的溢出修复”。
    - `bootstrap::finalize` 与 `validate_remote` 共用 `validate_pin_shape`：`control_hash`
      无 `control_rev` 在 finalize 也拒绝；finalize 补 `stored.highwater > state.head_seq`
      的 rollback 检查；测试 `pin_shape_rejects_hash_without_rev_on_both_paths`。
    - 删除 `session_flush` DurableAck 测试里恒真的 `COUNT(*).len()==1` 死断言，保留真实
      `count()==2` 断言。
    - `exactly(3,1)` 的语义已确认：`(index, rows)`，第 3 条 staging journal DELETE 必须
      恰好 1 行；其余 DELETE 不设守卫；`abort_guard_failure_keeps_marker_for_retry` 已覆盖
      guard 失败整体回滚 + marker 保留。不是缺陷。
    - `SyncContext.key_epoch` 的 rotation 衔接已在 App 层存在（`shell.rs` 在 rotation 成功后
      先更新 `self.sync` 再重建活动视图），无需新增 storage API；但本轮未补跨 crate 接线测试。
    - 不可恢复 `CloseReport.flush_error` 已有测试；**UI 阻断关闭/切换 + 导出/复制 live 正文
      → 重开**仍未接。
    - detached 矩阵仍缺：baseline VV 含本地未知 peer、`from` 超前 doc VV、重复 import、
      多 peer 物理乱序、大文档 detached merge；结构 verify 对并发删除的产品语义仍未定。
    - 4096+1 页超限失败、真实 relay RTT、`unrecoverable && pending_len==0` 无 ACK 仍缺测试。


12. **第三轮评审 P1/P2 修补（2026-11-21）**：
    - `SessionDriver::close` 在 `session.flush_is_unrecoverable()` 时立即 break，不再等满
      timeout 才返回 `CloseReport.flush_error`；测试把 timeout 从 0 改为 5s 并断言
      `elapsed < 1s`。
    - `is_permanent_corrupt` 改为**正向白名单** `is_retryable_write_error`：只有
      `Io`/`Sqlite`/`SnapshotBusy`/`WriteBackpressure` 可重试；`Corrupt`、`Loro`、
      `LoroEncode`、`Coord`、schema 漂移、通道关闭等一律 fail-closed 到“需要重开文档”，
      不再形成永久 Retryable。
    - `apply_selected` 的 `mark_baseline_verify_failed` 写失败不再用 marker 错误覆盖原始
      verify 错误：返回带根因 + marker 失败 + 必须 abort 的 `StorageError::Corrupt`。
    - 结构 verify 的并发删除语义已按当前实现定案：`StructureDoc::snapshot()` 返回**全量
      节点**（含 `deleted`），删除是 grow-only meta 事件而不是 `LoroTree::delete`；因此
      本地并发删除基线笔记不会让 `merged_notes` 缺项，verify 不会误写 marker。产品语义是
      “删笔记不删正文，恢复仍可用”；`verify_imported` 已加注释，结构侧新增
      `deleted_note_remains_in_snapshot_with_note_identity` 钉住“deleted 节点仍保留
      note/body 身份”。若未来改成物理删节点，必须重新定义 verify。
    - `auto_rebuilt` 粒度仍是每条 flush 命令一次，不是每批一次；慢盘多批场景可能保守地
      进入不可恢复，已在 `flush.rs` 注释写明，未改行为。
    - UI 的关闭/切换阻断 + “导出/复制 live 正文 → 重开”仍未接；detached 五项、4096+1 页、
      真实 RTT、`unrecoverable && pending_len==0` 无 ACK 仍未补。


13. **第四轮评审 P1/P2 修补（2026-11-21）**：
    - `Sqlite` 不再整类可重试：只有 `DatabaseBusy`/`DatabaseLocked` 进入
      `is_retryable_write_error`；`ConstraintViolation`/`ReadOnly`/`CannotOpen`/
      `DatabaseCorrupt`/`NotFound` 等永久错误直接 `Unrecoverable`。新增
      `flush_treats_permanent_sqlite_error_as_unrecoverable` 与分类单测
      `retryable_write_error_is_a_positive_whitelist`。
    - 普通写失败的“可重试”集成用例改用**真实外部写锁**产生 SQLITE_BUSY，不再用
      `RAISE(ABORT)`（它实际是 `ConstraintViolation`，现在按永久错误处理）。原
      “重建后重试 Retryable”集成用例因无法用真实 BUSY 精确卡在重建后而移除；
      分类由单测覆盖，**RebuildOutcome::Retryable 的直接集成覆盖暂时缺一条**。
    - `apply_selected` 的 marker 写失败改报 `StorageError::SyncConflict`（journal 仍可
      续跑，不是数据损坏），保留原始 verify 根因。
    - `read_meta_array` 长度非法改报 `SyncError::LocalCorrupt`，与 pin 结构校验统一；
      新增 `read_meta_array_rejects_wrong_length_as_local_corrupt`。
    - `finalize` 在“已有进度、不改写 cursor”时也对 `sync.relay_cursor` 做 CAS 自写回，
      防止读完 cursor 后被并发改写而静默放过；暂无专门的并发注入用例。
    - 未处理：`state_from_journal` 的 `finishable` 在计数相等时仍做 O(N) 逐项校验
      （Phase1B 高频状态查询需缓存/近似）；结构 verify deleted 语义已定案但依赖当前
      snapshot 实现；UI 救济、detached 五项、真实 RTT、4096+1 页、pending_len==0 无 ACK
      仍未补。


14. **实现收口（2026-11-21，非 RTT）**：
    - detached 矩阵补齐 5 项：本地未知 peer、`from` 超前本地 VV、同一 update 重复 import、
      多 peer 物理乱序、大文档 merge；`body_doc_detached_oplog` 现在 **16** 用例。
    - `coverage_scan_rejects_more_than_max_pages`：S=4096×256+1，validate 用满 4096 页后
      返回 `未证明覆盖到 S（页数超过上限）`；`PageIndexRelay` 只替换 `log` 为 partition_point
      切片，避免 FakeRelay 的 O(S²) 夹具。生产文案仍不含数字 4096。
    - `state_from_journal` 的 `finishable` 改为 O(1) 计数近似；逐项 applied/摘要证明只在
      `finish_baseline_apply` 做，读状态不再 O(N)。
    - flusher 在 `unrecoverable` 时即使 `pending_len==0` 也不补发确认 ACK（防御分支，无专门
      注入用例）。
    - `RebuildOutcome::Retryable` 的分类抽成 `rebuild_write_error_outcome` 并单测 BUSY /
      Constraint；完整 flusher 路径的 Retryable 仍无直接集成用例（真实 BUSY 难以精确卡在
      重建后）。
    - UI：切换前 `CloseReport.needs_reopen`/`flush_error` 阻断并复制 `source_text`；关窗前
      `last_write_error` 不可恢复则复制正文并通过 `on_window_should_close` 返回 false 取消关窗。
      没有单独的“导出”按钮，救济是自动复制到剪贴板 + 保留现场。
    - 不再把真实 RTT 当本地可测项；机会性追赶启用前必须真实 relay 标定。
    - 仍未补：`finalize` cursor CAS 的并发注入用例、`unrecoverable && pending_len==0` 的
      专门测试、Rebuild Retryable 的完整路径集成、UI 真机观感验证。
