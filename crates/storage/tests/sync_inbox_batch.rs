//! 批量 inbox API（Phase 1A-1）：与逐条入口的语义一致性、冲突不半写、前缀推进，
//! 以及批量 integrate 的严格全 verified 语义。
//!
//! 批量入口 [menghunote_storage::store_inbox_verified_batch] 现在是「既有行比对 +
//! 新行插入 + `received` 前缀推进」同一事务；这里只断言**行为**（与逐条一致、冲突整批
//! 回滚），不对实现（几条 SQL/几次事务）做断言。竞态本身不易在单测里构造，
//! 用「先插入后批量含同 seq」覆盖幂等与冲突两条路径。
//!
//! 末尾另有一组前缀扫描窗口用例：`sync.skipped_received` 标记超过 4096 条（窗口上限）
//! 时必须跨窗口继续扫描，且多个来源同时被截断时不能提前判定缺口。

mod support;

use support::*;

use menghunote_storage::{
    advance_integrated_prefix, append_updates_with_integrated_marks, count_inbox_ready, load_head,
    mark_integrated, mark_integrated_batch, mark_skipped_event_received, set_sync_log_id,
    store_inbox_verified, store_inbox_verified_batch, sync_cursor_state, take_inbox_ready,
    take_inbox_ready_where, StorageError, UpdateRecord, Value, Vault, VerifiedInboxRow,
    META_SYNC_LOG_ID, META_SYNC_RECEIVED_SEQ, META_SYNC_SKIPPED_RECEIVED_PREFIX,
};

const LOG_ID: [u8; 16] = [0x3a; 16];

/// 另一个 incarnation（restore 迁移后当前 `sync.log_id` 可能变成它，旧行仍然 durable）。
const OTHER_LOG_ID: [u8; 16] = [0x5b; 16];

fn bind_log(vault: &Vault) {
    set_sync_log_id(vault, &LOG_ID).expect("写 sync.log_id");
}

fn row(seq: u64, seed: u8, payload_len: usize) -> VerifiedInboxRow {
    VerifiedInboxRow {
        log_id: LOG_ID,
        relay_seq: seq,
        object_id: [seed; 32],
        epoch: 1,
        verified_payload: vec![seed; payload_len],
    }
}

fn rows(count: u64, offset: u64) -> Vec<VerifiedInboxRow> {
    (1..=count)
        .map(|index| {
            let seed = u8::try_from(index % 251).expect("seed 在 u8 内");
            row(
                offset + index,
                seed,
                32 + usize::try_from(index).unwrap_or(0),
            )
        })
        .collect()
}

fn received(vault: &Vault) -> u64 {
    sync_cursor_state(vault).expect("游标状态").received_seq
}

/// 一个 `sync.skipped_received` 标记的完整键（与生产写入格式逐字节一致）。
fn skipped_key(seq: u64) -> String {
    let hex: String = LOG_ID.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("{META_SYNC_SKIPPED_RECEIVED_PREFIX}{hex}.{seq:020}")
}

/// 直接写一批 `sync.skipped_received` 标记（**一个**事务，只付一次 fsync）。
///
/// 生产路径 [mark_skipped_event_received] 每写一个标记就推进一次前缀；这里要构造数千个
/// 标记验证**扫描窗口本身**，所以直接准备 durable 状态，不逐条触发推进。
fn seed_skipped_markers(vault: &Vault, seqs: impl IntoIterator<Item = u64>) {
    let mut sql = String::from("BEGIN IMMEDIATE;\n");
    let mut count = 0usize;
    for seq in seqs {
        sql.push_str("INSERT INTO meta(key,value) VALUES('");
        sql.push_str(&skipped_key(seq));
        sql.push_str("',x'31');\n");
        count += 1;
    }
    if count == 0 {
        return;
    }
    sql.push_str("COMMIT;\n");
    vault.exec_batch(&sql).expect("批量写入 skipped 标记");
}

fn integrated(vault: &Vault) -> u64 {
    sync_cursor_state(vault).expect("游标状态").integrated_seq
}

/// 各行的 `(relay_seq, state)`，按 seq 升序（批量 integrate 断言用）。
fn inbox_states(vault: &Vault) -> Vec<(u64, String)> {
    inbox_rows(vault)
        .into_iter()
        .map(|(seq, _, _, state, _)| (seq, state))
        .collect()
}

/// 一条 inbox 行（测试断言用）：`(seq, object_id, epoch, state, 明文负载)`。
type InboxRow = (u64, [u8; 32], u64, String, Vec<u8>);

/// 读出 inbox 的全部行（按 relay_seq 升序）。
fn inbox_rows(vault: &Vault) -> Vec<InboxRow> {
    vault
        .query(
            "SELECT relay_seq,object_id,epoch,state,verified_payload FROM sync_inbox \
             WHERE log_id=?1 ORDER BY relay_seq",
            &[Value::Blob(LOG_ID.to_vec())],
        )
        .expect("读 sync_inbox")
        .iter()
        .map(|row| {
            let seq = match row.first() {
                Some(Value::Integer(value)) => u64::try_from(*value).expect("seq 非负"),
                other => panic!("relay_seq 类型异常：{other:?}"),
            };
            let object_id: [u8; 32] = match row.get(1) {
                Some(Value::Blob(bytes)) => bytes.as_slice().try_into().expect("object_id 32B"),
                other => panic!("object_id 类型异常：{other:?}"),
            };
            let epoch = match row.get(2) {
                Some(Value::Integer(value)) => u64::try_from(*value).expect("epoch 非负"),
                other => panic!("epoch 类型异常：{other:?}"),
            };
            let state = match row.get(3) {
                Some(Value::Text(value)) => value.clone(),
                other => panic!("state 类型异常：{other:?}"),
            };
            let payload = match row.get(4) {
                Some(Value::Blob(bytes)) => bytes.clone(),
                other => panic!("verified_payload 类型异常：{other:?}"),
            };
            (seq, object_id, epoch, state, payload)
        })
        .collect()
}

#[test]
fn batch_matches_per_row_for_256_rows() {
    let (_dir_row, vault_row) = fresh_vault();
    let (_dir_batch, vault_batch) = fresh_vault();
    bind_log(&vault_row);
    bind_log(&vault_batch);
    let input = rows(256, 0);

    for item in &input {
        store_inbox_verified(
            &vault_row,
            &item.log_id,
            item.relay_seq,
            &item.object_id,
            item.epoch,
            &item.verified_payload,
        )
        .expect("逐条写入");
    }
    let inserted = store_inbox_verified_batch(&vault_batch, &input).expect("批量写入");

    assert_eq!(inserted, input.len(), "首次批量写入必须全部是新行");
    assert_eq!(received(&vault_row), 256);
    assert_eq!(received(&vault_batch), 256, "前缀必须推进到整页末尾");
    assert_eq!(
        inbox_rows(&vault_row),
        inbox_rows(&vault_batch),
        "逐条与批量必须产生逐字节相同的 inbox 表"
    );
    assert_eq!(count_inbox_ready(&vault_row).unwrap(), 256);
    assert_eq!(count_inbox_ready(&vault_batch).unwrap(), 256);
    let ready_row = take_inbox_ready(&vault_row, 16).expect("逐条 ready");
    let ready_batch = take_inbox_ready(&vault_batch, 16).expect("批量 ready");
    assert_eq!(ready_row, ready_batch);
}

#[test]
fn batch_is_idempotent_and_returns_zero_for_known_rows() {
    let (_dir, vault) = fresh_vault();
    bind_log(&vault);
    let input = rows(4, 0);
    assert_eq!(store_inbox_verified_batch(&vault, &input).unwrap(), 4);

    // 同字节重放：不新增、不改写、前缀不变。
    assert_eq!(
        store_inbox_verified_batch(&vault, &input).unwrap(),
        0,
        "同字节重放必须幂等跳过"
    );
    assert_eq!(inbox_rows(&vault).len(), 4);
    assert_eq!(received(&vault), 4);

    // 混合：一部分已存在（一致）+ 一部分新行 → 只插入新行，返回值只数新行。
    let mut mixed = rows(4, 0);
    mixed.extend(rows(2, 4));
    assert_eq!(store_inbox_verified_batch(&vault, &mixed).unwrap(), 2);
    assert_eq!(inbox_rows(&vault).len(), 6, "既有 1..4 不得重写，只补 5/6");
    assert_eq!(received(&vault), 6);
}

#[test]
fn batch_conflict_rejects_whole_batch_without_partial_write() {
    let (_dir, vault) = fresh_vault();
    bind_log(&vault);
    store_inbox_verified(&vault, &LOG_ID, 5, &[0x55; 32], 1, b"existing-5").expect("预置 5");
    let before = inbox_rows(&vault);
    let before_received = received(&vault);
    assert_eq!(before.len(), 1);
    assert_eq!(before_received, 0, "缺 1..4，前缀只能停在 0");

    // 4 / 6 是合法新行，5 与既有行字节不同 → 整批拒绝，4/6 也不能写。
    let conflicting = vec![
        row(4, 0x44, 16),
        VerifiedInboxRow {
            log_id: LOG_ID,
            relay_seq: 5,
            object_id: [0x55; 32],
            epoch: 1,
            verified_payload: b"different-5".to_vec(),
        },
        row(6, 0x66, 16),
    ];
    let error = store_inbox_verified_batch(&vault, &conflicting).expect_err("冲突必须报错");
    assert!(
        matches!(error, menghunote_storage::StorageError::SyncConflict { .. }),
        "冲突必须是 SyncConflict，实际 {error:?}"
    );
    assert_eq!(inbox_rows(&vault), before, "冲突批次不得写入任何行");
    assert_eq!(received(&vault), before_received, "冲突批次不得推进前缀");
}

#[test]
fn batch_conflict_does_not_advance_prefix_for_known_rows() {
    let (_dir, vault) = fresh_vault();
    bind_log(&vault);
    // 先有 1..3；批里 1..3 一致 + 4 新 + 5 冲突（不存在 5 但先单独写一条不同字节）。
    store_inbox_verified_batch(&vault, &rows(3, 0)).expect("预置 1..3");
    store_inbox_verified(&vault, &LOG_ID, 5, &[0x99; 32], 1, b"five").expect("预置 5");
    let before = inbox_rows(&vault);
    let before_received = received(&vault);
    assert_eq!(before_received, 3);

    let mut batch = rows(5, 0);
    batch[3] = row(4, 0x04, 8);
    batch[4] = VerifiedInboxRow {
        log_id: LOG_ID,
        relay_seq: 5,
        object_id: [0x99; 32],
        epoch: 1,
        verified_payload: b"other-five".to_vec(),
    };
    let error = store_inbox_verified_batch(&vault, &batch).expect_err("5 冲突");
    assert!(matches!(
        error,
        menghunote_storage::StorageError::SyncConflict { .. }
    ));
    assert_eq!(inbox_rows(&vault), before, "冲突批次不得写入 4");
    assert_eq!(received(&vault), before_received);
}

#[test]
fn batch_sorts_unsorted_input_and_rejects_duplicate_seq() {
    let (_dir, vault) = fresh_vault();
    bind_log(&vault);
    let mut shuffled = rows(3, 0);
    shuffled.swap(0, 2);
    shuffled.swap(0, 1);
    assert_eq!(shuffled[0].relay_seq, 2, "输入确实乱序");
    assert_eq!(
        store_inbox_verified_batch(&vault, &shuffled).unwrap(),
        3,
        "批量入口必须自己按 relay_seq 稳定排序"
    );
    assert_eq!(received(&vault), 3);
    let states: Vec<String> = inbox_rows(&vault)
        .into_iter()
        .map(|(seq, _, _, state, _)| format!("{seq}:{state}"))
        .collect();
    assert_eq!(
        states,
        vec![
            "1:verified".to_owned(),
            "2:verified".to_owned(),
            "3:verified".to_owned()
        ]
    );

    // 同一批重复 seq：直接拒绝，且不写入任何行。
    let before = inbox_rows(&vault);
    let duplicated = vec![row(4, 0xa4, 8), row(4, 0xa4, 8), row(5, 0xa5, 8)];
    let error = store_inbox_verified_batch(&vault, &duplicated).expect_err("重复 seq 必须拒绝");
    assert!(
        matches!(error, menghunote_storage::StorageError::Corrupt { .. }),
        "重复 seq 是上游错误（Corrupt），实际 {error:?}"
    );
    assert_eq!(inbox_rows(&vault), before);
    assert_eq!(received(&vault), 3);
}

#[test]
fn batch_advances_prefix_only_over_contiguous_consumed_seq() {
    let (_dir, vault) = fresh_vault();
    bind_log(&vault);

    // 不连续（缺 3）：前缀只能到 2。
    assert_eq!(store_inbox_verified_batch(&vault, &rows(2, 0)).unwrap(), 2);
    let gap = vec![row(4, 0x44, 8)];
    assert_eq!(store_inbox_verified_batch(&vault, &gap).unwrap(), 1);
    assert_eq!(received(&vault), 2, "缺 3 时前缀必须停在 2");

    // 补上 3 → 一次性推进到 4（跨过此前 durable 的 4）。
    assert_eq!(
        store_inbox_verified_batch(&vault, &[row(3, 0x33, 8)]).unwrap(),
        1
    );
    assert_eq!(received(&vault), 4);

    // ASSET 这类不写 inbox 的事件用 skipped_received 标记参与前缀推进。
    mark_skipped_event_received(&vault, &LOG_ID, 5).expect("标记 5 已消费");
    assert_eq!(received(&vault), 5, "skipped 标记不写 inbox 也要推进前缀");
    assert_eq!(
        store_inbox_verified_batch(&vault, &[row(6, 0x66, 8)]).unwrap(),
        1
    );
    assert_eq!(received(&vault), 6);
}

#[test]
fn batch_rejects_mixed_log_id_and_invalid_seq_without_writing() {
    let (_dir, vault) = fresh_vault();
    bind_log(&vault);

    let mixed = vec![
        row(1, 0x11, 8),
        VerifiedInboxRow {
            log_id: [0x3b; 16],
            relay_seq: 2,
            object_id: [0x22; 32],
            epoch: 1,
            verified_payload: vec![0x22; 8],
        },
    ];
    assert!(store_inbox_verified_batch(&vault, &mixed).is_err());
    assert!(inbox_rows(&vault).is_empty());

    let mut zero_seq = vec![row(1, 0x11, 8)];
    zero_seq[0].relay_seq = 0;
    assert!(store_inbox_verified_batch(&vault, &zero_seq).is_err());
    let mut zero_epoch = vec![row(1, 0x11, 8)];
    zero_epoch[0].epoch = 0;
    assert!(store_inbox_verified_batch(&vault, &zero_epoch).is_err());
    assert!(inbox_rows(&vault).is_empty());
    assert_eq!(received(&vault), 0);

    // 空批次是合法 no-op（便于调用方无条件调用）。
    assert_eq!(store_inbox_verified_batch(&vault, &[]).unwrap(), 0);
}

#[test]
fn batch_rejects_rows_from_a_different_log_than_the_vault() {
    let (_dir, vault) = fresh_vault();
    bind_log(&vault);
    // 与已绑定的 log_id 不同 → SyncLogMismatch（relay reset 必须显式迁移）。
    let other = vec![VerifiedInboxRow {
        log_id: [0x3c; 16],
        relay_seq: 1,
        object_id: [0x11; 32],
        epoch: 1,
        verified_payload: vec![0x11; 4],
    }];
    let error = store_inbox_verified_batch(&vault, &other).expect_err("不同 log_id 必须拒绝");
    assert!(
        matches!(
            error,
            menghunote_storage::StorageError::SyncLogMismatch { .. }
        ),
        "实际 {error:?}"
    );
}

#[test]
fn batch_full_idempotent_replay_repairs_unadvanced_prefix() {
    let (_dir, vault) = fresh_vault();
    bind_log(&vault);
    let input = rows(4, 0);
    assert_eq!(store_inbox_verified_batch(&vault, &input).unwrap(), 4);

    // 模拟「行已经提交、前缀还停在 0」（旧实现事务边界之间崩溃留下的状态）。
    vault
        .set_meta(META_SYNC_RECEIVED_SEQ, &0u64.to_be_bytes())
        .expect("手动退回前缀");
    assert_eq!(received(&vault), 0);

    // 全幂等重放：不新增、不报错、返回 0，但仍要把前缀补到最新。
    assert_eq!(store_inbox_verified_batch(&vault, &input).unwrap(), 0);
    assert_eq!(inbox_rows(&vault).len(), 4);
    assert_eq!(received(&vault), 4, "全幂等重放也要把前缀推进到最新");
}

/// 预置 1..3（连续前缀=3）与 seq5 后，整批 1..6（1..3 幂等、4/6 新、5 被改坏）：
/// 必须整批回滚（4/6 不写、1..3/5 不变、received 保持在 3）。
fn assert_tamper_rolls_back_whole_batch(label: &str, mutate: impl FnOnce(&mut VerifiedInboxRow)) {
    let (_dir, vault) = fresh_vault();
    bind_log(&vault);
    assert_eq!(store_inbox_verified_batch(&vault, &rows(3, 0)).unwrap(), 3);
    let existing = VerifiedInboxRow {
        log_id: LOG_ID,
        relay_seq: 5,
        object_id: [0x55; 32],
        epoch: 1,
        verified_payload: b"existing-5".to_vec(),
    };
    store_inbox_verified(
        &vault,
        &existing.log_id,
        existing.relay_seq,
        &existing.object_id,
        existing.epoch,
        &existing.verified_payload,
    )
    .expect("预置 seq5");
    let before = inbox_rows(&vault);
    assert_eq!(before.len(), 4);
    assert_eq!(received(&vault), 3, "缺 4，前缀只能停在 3");

    let mut tampered = existing;
    mutate(&mut tampered);
    let mut batch = rows(6, 0);
    batch[4] = tampered; // seq5
    let error = match store_inbox_verified_batch(&vault, &batch) {
        Ok(inserted) => panic!("{label} 篡改必须报错，实际插入 {inserted} 行"),
        Err(error) => error,
    };
    assert!(
        matches!(error, StorageError::SyncConflict { .. }),
        "{label}: 冲突必须是 SyncConflict，实际 {error:?}"
    );
    assert_eq!(inbox_rows(&vault), before, "{label}: 整批回滚，既有行不变");
    assert_eq!(
        received(&vault),
        3,
        "{label}: 冲突批次不得推进 received 前缀（4/6 都没写）"
    );
}

#[test]
fn batch_tampered_object_id_epoch_or_payload_rolls_back_whole_batch() {
    assert_tamper_rolls_back_whole_batch("object_id", |row| row.object_id = [0x99; 32]);
    assert_tamper_rolls_back_whole_batch("epoch", |row| row.epoch = 2);
    assert_tamper_rolls_back_whole_batch("payload", |row| {
        row.verified_payload = b"tampered".to_vec();
    });
}

#[test]
fn batch_conflict_rolls_back_log_id_binding_written_in_the_same_transaction() {
    let (_dir, vault) = fresh_vault();
    // 手工写一条 inbox 行但不写 sync.log_id（模拟旧库/异常状态）：
    // 批量入口必须在事务里补写 log_id，而冲突时这笔补写也要回滚。
    vault
        .exec(
            "INSERT INTO sync_inbox(log_id,relay_seq,object_id,epoch,state,verified_payload) \
             VALUES(?1,?2,?3,?4,'verified',?5)",
            &[
                Value::Blob(LOG_ID.to_vec()),
                Value::Integer(5),
                Value::Blob(vec![0x55; 32]),
                Value::Integer(1),
                Value::Blob(b"existing-5".to_vec()),
            ],
        )
        .expect("手工预置 inbox 行");
    assert!(vault
        .get_meta(META_SYNC_LOG_ID)
        .expect("读 log_id")
        .is_none());

    let conflicting = vec![
        row(4, 0x44, 16),
        VerifiedInboxRow {
            log_id: LOG_ID,
            relay_seq: 5,
            object_id: [0x55; 32],
            epoch: 1,
            verified_payload: b"different".to_vec(),
        },
    ];
    let error = store_inbox_verified_batch(&vault, &conflicting).expect_err("冲突必须报错");
    assert!(
        matches!(error, StorageError::SyncConflict { .. }),
        "实际 {error:?}"
    );
    assert!(
        vault
            .get_meta(META_SYNC_LOG_ID)
            .expect("读 log_id")
            .is_none(),
        "sync.log_id 的补写必须与新行一起回滚"
    );
    assert_eq!(received(&vault), 0);
    assert_eq!(inbox_rows(&vault).len(), 1, "新行 4 不得写入");
}

#[test]
fn batch_integrate_marks_all_verified_and_prefix_advances_only_on_demand() {
    let (_dir, vault) = fresh_vault();
    bind_log(&vault);
    assert_eq!(store_inbox_verified_batch(&vault, &rows(3, 0)).unwrap(), 3);
    assert_eq!(received(&vault), 3);
    assert_eq!(integrated(&vault), 0);

    // 空输入是合法 no-op。
    assert_eq!(mark_integrated_batch(&vault, &LOG_ID, &[]).unwrap(), 0);

    assert_eq!(
        mark_integrated_batch(&vault, &LOG_ID, &[1, 2, 3]).unwrap(),
        3
    );
    assert_eq!(
        inbox_states(&vault),
        vec![
            (1, "integrated".to_owned()),
            (2, "integrated".to_owned()),
            (3, "integrated".to_owned())
        ]
    );
    assert_eq!(
        integrated(&vault),
        0,
        "mark_integrated_batch 不得自己推进 integrated 前缀"
    );

    // 调用方在整批正文完成后统一推进一次。
    advance_integrated_prefix(&vault).expect("推进 integrated 前缀");
    assert_eq!(integrated(&vault), 3);
    // 再推进一次是幂等 no-op。
    advance_integrated_prefix(&vault).expect("重复推进");
    assert_eq!(integrated(&vault), 3);

    // 单条入口行为保持：已 integrated 的行重复标记仍是成功（幂等）。
    mark_integrated(&vault, &LOG_ID, 2).expect("单条幂等");
    assert_eq!(integrated(&vault), 3);
}

#[test]
fn batch_integrate_rolls_back_when_a_row_is_not_verified() {
    let (_dir, vault) = fresh_vault();
    bind_log(&vault);
    assert_eq!(store_inbox_verified_batch(&vault, &rows(3, 0)).unwrap(), 3);
    // 先把 seq2 标成 integrated：1 未集成，连续前缀仍停在 0。
    mark_integrated(&vault, &LOG_ID, 2).expect("先集成 seq2");
    assert_eq!(integrated(&vault), 0);
    let before = inbox_rows(&vault);

    let error =
        mark_integrated_batch(&vault, &LOG_ID, &[1, 2, 3]).expect_err("seq2 已不是 verified");
    assert!(
        matches!(error, StorageError::SyncConflict { .. }),
        "含已 integrated 行必须整批冲突，实际 {error:?}"
    );
    assert_eq!(
        inbox_rows(&vault),
        before,
        "整批回滚：1/3 必须仍是 verified"
    );
    assert_eq!(integrated(&vault), 0);
}

#[test]
fn batch_integrate_rolls_back_when_a_row_is_missing() {
    let (_dir, vault) = fresh_vault();
    bind_log(&vault);
    assert_eq!(store_inbox_verified_batch(&vault, &rows(2, 0)).unwrap(), 2);
    let before = inbox_rows(&vault);

    let error = mark_integrated_batch(&vault, &LOG_ID, &[1, 9]).expect_err("seq9 不存在");
    assert!(
        matches!(error, StorageError::SyncConflict { .. }),
        "含不存在行必须整批冲突，实际 {error:?}"
    );
    assert_eq!(
        inbox_rows(&vault),
        before,
        "整批回滚：seq1 必须仍是 verified"
    );
    assert_eq!(integrated(&vault), 0);
}

#[test]
fn batch_integrate_prefix_crosses_asset_skipped_marker() {
    let (_dir, vault) = fresh_vault();
    bind_log(&vault);
    assert_eq!(store_inbox_verified_batch(&vault, &rows(3, 0)).unwrap(), 3);
    // seq4 是 ASSET：不写 inbox，只留 durable skipped 标记。
    mark_skipped_event_received(&vault, &LOG_ID, 4).expect("标记 seq4 已消费");
    assert_eq!(received(&vault), 4);

    assert_eq!(
        mark_integrated_batch(&vault, &LOG_ID, &[1, 2, 3]).unwrap(),
        3
    );
    assert_eq!(integrated(&vault), 0);
    advance_integrated_prefix(&vault).expect("推进 integrated 前缀");
    assert_eq!(
        integrated(&vault),
        4,
        "integrated 前缀同样要越过 ASSET skipped 标记"
    );
}

// ============================================================ Phase 1A-2：A 事务内标记

/// `append_updates_with_integrated_marks`：update 与 inbox 标记同一事务提交，**不**自己
/// 推进 integrated 前缀（调用方整批结束后统一推进）。
#[test]
fn append_with_integrated_marks_commits_update_and_marks_together() {
    let (_dir, vault) = fresh_vault();
    bind_log(&vault);
    assert_eq!(store_inbox_verified_batch(&vault, &rows(3, 0)).unwrap(), 3);

    let doc = seed_note(&vault, "note-1", "doc-1");
    doc.insert_scalars(0, "hello").expect("写正文");
    let current = head(&vault, "doc-1");
    let base = current.version_vector().expect("解码 head vv");
    let update = pending(&doc, &base).expect("导出增量");
    let record = UpdateRecord::new("batch-1", update);

    let head_after = append_updates_with_integrated_marks(
        &vault,
        "doc-1",
        current.head_seq,
        &record,
        &LOG_ID,
        &[1, 2],
    )
    .expect("同事务 append + 标记");
    assert_eq!(head_after.head_seq, 1);
    assert_eq!(load_head(&vault, "doc-1").expect("head").head_seq, 1);
    assert_eq!(
        inbox_states(&vault),
        vec![
            (1, "integrated".to_owned()),
            (2, "integrated".to_owned()),
            (3, "verified".to_owned())
        ]
    );
    assert_eq!(integrated(&vault), 0, "本 API 不推进前缀");
    assert_eq!(count_inbox_ready(&vault).unwrap(), 1);

    advance_integrated_prefix(&vault).expect("统一推进前缀");
    assert_eq!(integrated(&vault), 2, "前缀只到连续 integrated 的末尾");
}

/// 标记守卫失败（不存在 / 已不是 verified）→ **整笔 A 事务回滚**：update 不落库、
/// 其它行的标记也不保留，inbox 与 head 完全回到调用前。
#[test]
fn append_with_integrated_marks_rolls_back_update_when_a_mark_is_not_verified() {
    let (_dir, vault) = fresh_vault();
    bind_log(&vault);
    assert_eq!(store_inbox_verified_batch(&vault, &rows(2, 0)).unwrap(), 2);
    // seq2 提前 integrated（seq1 仍 verified）：标记 [1,2] 时第二条守卫会失败。
    mark_integrated(&vault, &LOG_ID, 2).expect("先集成 seq2");
    let before = inbox_rows(&vault);

    let doc = seed_note(&vault, "note-1", "doc-1");
    doc.insert_scalars(0, "hello").expect("写正文");
    let current = head(&vault, "doc-1");
    let base = current.version_vector().expect("解码 head vv");
    let update = pending(&doc, &base).expect("导出增量");
    let record = UpdateRecord::new("batch-1", update);

    let error = append_updates_with_integrated_marks(
        &vault,
        "doc-1",
        current.head_seq,
        &record,
        &LOG_ID,
        &[1, 2],
    )
    .expect_err("含非 verified 行必须整笔失败");
    assert!(
        matches!(error, StorageError::SyncConflict { .. }),
        "实际 {error:?}"
    );
    assert_eq!(
        load_head(&vault, "doc-1").expect("head").head_seq,
        0,
        "update 必须随标记失败一起回滚"
    );
    assert_eq!(inbox_rows(&vault), before, "整批标记回滚");
    assert_eq!(integrated(&vault), 0);

    // 同一份 update 只标记合法行时成功（证明失败是原子的，不是数据被吃掉了）。
    let head_after = append_updates_with_integrated_marks(
        &vault,
        "doc-1",
        current.head_seq,
        &record,
        &LOG_ID,
        &[1],
    )
    .expect("合法标记成功");
    assert_eq!(head_after.head_seq, 1);
    assert_eq!(inbox_rows(&vault)[0].3, "integrated");
}

// ===================================== 前缀扫描窗口（>4096 个 skipped 标记 / 并发写锁）

/// `skipped_received` 窗口在 4096 条处被 `LIMIT` 截断时必须继续下一窗口。
///
/// 回归点：若只给 skipped 窗口加 `LIMIT`、循环条件仍沿用「合并后的 truncated 且
/// current < last」，这里 skipped 窗口只看到 1..=4096、inbox 窗口里还有更大的 5001，
/// 于是 `current(4096) < last(5001)` 会提前 return，把第二个窗口里的 4097..=5000
/// 永久漏掉、`received` 卡死在 4096。
#[test]
fn received_prefix_crosses_more_than_one_skipped_window() {
    let (_dir, vault) = fresh_vault();
    bind_log(&vault);
    // 1..=4096 正好填满第一个窗口（`LIMIT 4096` 截断）；4097..=5000 在第二个窗口。
    seed_skipped_markers(&vault, 1..=5000);

    let inserted = store_inbox_verified_batch(&vault, &[row(5001, 0x51, 8)]).expect("批量写");
    assert_eq!(inserted, 1);
    assert_eq!(
        received(&vault),
        5001,
        "skipped 窗口被截断后必须继续扫描下一窗口，直到第一个缺口 5002"
    );
}

/// 跨窗口扫描后，前缀仍然只推进到**第一个缺口**：第二窗口里 4098 起的标记不能把
/// 缺掉的 4097 一起算成已消费。
#[test]
fn received_prefix_stops_at_gap_across_skipped_windows() {
    let (_dir, vault) = fresh_vault();
    bind_log(&vault);
    // 第一窗口 1..=4096（截断），4097 故意缺失，4098..=4999 落在第二窗口。
    seed_skipped_markers(&vault, 1..=4096);
    seed_skipped_markers(&vault, 4098..=4999);

    assert_eq!(
        store_inbox_verified_batch(&vault, &[row(5000, 0x50, 8)]).unwrap(),
        1
    );
    assert_eq!(
        received(&vault),
        4096,
        "即使 4098.. 与 inbox 5000 都已 durable，缺口 4097 也必须卡住前缀"
    );

    // 补上缺口 → 一次推进到 5000（跨过第二窗口的 skipped 标记与 inbox 行）。
    mark_skipped_event_received(&vault, &LOG_ID, 4097).expect("补 4097");
    assert_eq!(received(&vault), 5000);
}

/// 混合 inbox 行 + skipped 标记 + 缺口：前缀只到第一个缺口；补上后一次跨过。
#[test]
fn received_prefix_mixes_inbox_and_skipped_until_first_gap() {
    let (_dir, vault) = fresh_vault();
    bind_log(&vault);
    assert_eq!(store_inbox_verified_batch(&vault, &rows(3, 0)).unwrap(), 3);
    mark_skipped_event_received(&vault, &LOG_ID, 4).expect("skipped 4");
    assert_eq!(received(&vault), 4);

    // 6 已 durable，但 5 是缺口：前缀必须停在 4。
    assert_eq!(
        store_inbox_verified_batch(&vault, &[row(6, 0x66, 8)]).unwrap(),
        1
    );
    assert_eq!(received(&vault), 4, "缺 5 时跳过 6 也不得推进前缀");

    // 用 skipped 标记补上 5 → 一次推进到 6。
    mark_skipped_event_received(&vault, &LOG_ID, 5).expect("skipped 5");
    assert_eq!(received(&vault), 6);
}

/// 两个来源的窗口**同时**被截断：奇数序号走 inbox、偶数序号走 skipped，前缀要跨越
/// 8192 之后继续推进到 8200。
#[test]
fn received_prefix_advances_while_both_windows_are_truncated() {
    let (_dir, vault) = fresh_vault();
    bind_log(&vault);

    // inbox：1,3,...,8199（4100 行 > 窗口 4096）；skipped：2,4,...,8200（4100 个标记）。
    let inbox: Vec<VerifiedInboxRow> = (0..4100u64)
        .map(|index| {
            let seq = 1 + 2 * index;
            row(seq, u8::try_from(seq % 251).expect("seed 在 u8 内"), 8)
        })
        .collect();
    seed_skipped_markers(&vault, (0..4100u64).map(|index| 2 + 2 * index));

    assert_eq!(
        store_inbox_verified_batch(&vault, &inbox).unwrap(),
        inbox.len()
    );
    assert_eq!(
        received(&vault),
        8200,
        "两个窗口各自截断在 4096/8192，前缀必须跨窗口继续推进到 8200"
    );
}

/// 外部写事务占着写锁时，批量入口必须在 **BEGIN 阶段**整笔失败（SQLITE_BUSY），
/// 而不是先读到旧快照、按字节比较出 SyncConflict；失败不留半写，锁释放后可重放。
///
/// 这是 Immediate 事务可从外部观测到的行为：deferred 的 `BEGIN` 不取写锁，读路径
/// 会在锁被占时照常完成并先返回 SyncConflict。
#[test]
fn batch_under_foreign_write_lock_fails_busy_without_partial_write() {
    let (dir, vault) = fresh_vault();
    bind_log(&vault);
    store_inbox_verified(&vault, &LOG_ID, 1, &[0x11; 32], 1, b"one").expect("预置 seq1");
    assert_eq!(received(&vault), 1);

    // 本连接不等锁（生产是 5s）：让 BEGIN IMMEDIATE 的失败立刻返回，测试无需 sleep。
    vault
        .exec_batch("PRAGMA busy_timeout=0")
        .expect("busy_timeout=0");

    // 第二个 Vault（= 第二个连接）先占住写锁。
    let holder = open_vault(&vault_path(&dir));
    holder
        .exec_batch("BEGIN IMMEDIATE")
        .expect("外部连接占写锁");
    assert_eq!(inbox_rows(&vault).len(), 1, "读快照不受外部写锁影响");

    // 同 seq、不同字节：deferred 会先读到 seq1 并报 SyncConflict；Immediate 在 BEGIN
    // 就撞上写锁 → SQLITE_BUSY。batch 里再带一条新行 2，确认不会留下半写。
    let conflicting = vec![
        VerifiedInboxRow {
            log_id: LOG_ID,
            relay_seq: 1,
            object_id: [0x11; 32],
            epoch: 1,
            verified_payload: b"changed".to_vec(),
        },
        row(2, 0x22, 8),
    ];
    let error = store_inbox_verified_batch(&vault, &conflicting).expect_err("写锁被占必须失败");
    assert!(
        matches!(
            error,
            StorageError::Sqlite(rusqlite::Error::SqliteFailure(code, _))
                if code.code == rusqlite::ErrorCode::DatabaseBusy
        ),
        "并发写事务在场时必须整笔 SQLITE_BUSY，实际 {error:?}"
    );

    holder.exec_batch("ROLLBACK").expect("释放外部写锁");
    drop(holder);

    assert_eq!(inbox_rows(&vault).len(), 1, "失败批次不得留下半写（seq2）");
    assert_eq!(received(&vault), 1, "失败批次不得推进前缀");

    // 锁释放后重放同一批：现在能读到真实冲突，仍然整批回滚。
    let error = store_inbox_verified_batch(&vault, &conflicting).expect_err("真实冲突");
    assert!(
        matches!(error, StorageError::SyncConflict { .. }),
        "实际 {error:?}"
    );
    assert_eq!(inbox_rows(&vault).len(), 1);
    assert_eq!(received(&vault), 1);
}

// ================================ incarnation 过滤（restore 迁移 log_id 后的旧行）

/// 当前 incarnation 的 ready `relay_seq`（升序；`take_inbox_ready` 已按 log/seq 排序）。
fn ready_seqs(vault: &Vault, limit: usize) -> Vec<u64> {
    take_inbox_ready(vault, limit)
        .expect("读 ready")
        .into_iter()
        .map(|row| row.relay_seq)
        .collect()
}

/// 直接写 `meta` 原始字节（模拟 restore 的显式迁移 / 损坏状态，绕过 Vault 校验）。
fn write_raw_meta(vault: &Vault, key: &str, value: &[u8]) {
    vault
        .exec(
            "INSERT INTO meta(key,value) VALUES(?1,?2) \
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            &[Value::Text(key.to_owned()), Value::Blob(value.to_vec())],
        )
        .expect("写原始 meta");
}

/// 直接插入一条**其它 incarnation** 的 verified 行。
///
/// 生产入口 `store_inbox_verified*` 都先 `ensure_log_id`，跨代写入会被
/// `SyncLogMismatch` 拒绝；这种 durable 状态只能由 restore 显式迁移 `sync.log_id`
/// 后留下，所以测试直接用 SQL 准备，不经过被测试的过滤逻辑。
fn insert_stale_incarnation_row(vault: &Vault, log_id: &[u8; 16], relay_seq: u64, seed: u8) {
    vault
        .exec(
            "INSERT INTO sync_inbox(log_id,relay_seq,object_id,epoch,state,verified_payload) \
             VALUES(?1,?2,?3,?4,'verified',?5)",
            &[
                Value::Blob(log_id.to_vec()),
                Value::Integer(i64::try_from(relay_seq).expect("relay_seq 在 i64 内")),
                Value::Blob(vec![seed; 32]),
                Value::Integer(1),
                Value::Blob(vec![seed; 16]),
            ],
        )
        .expect("插入旧 incarnation 的 verified 行");
}

/// 当前 log A 有 verified 行、库里另有 log B 的 verified 行时：
/// ready/count 只计 A；B 即使 `relay_seq <= received` 也不计入；received 前缀只按 A 推进。
#[test]
fn ready_and_count_only_include_current_log_incarnation() {
    let (_dir, vault) = fresh_vault();
    bind_log(&vault);
    assert_eq!(store_inbox_verified_batch(&vault, &rows(2, 0)).unwrap(), 2);
    assert_eq!(received(&vault), 2);

    // 旧 incarnation B 的 1..=5 仍在库里（restore 迁移 log_id 后旧行不能被静默删除）。
    for seq in 1..=5u64 {
        insert_stale_incarnation_row(
            &vault,
            &OTHER_LOG_ID,
            seq,
            u8::try_from(seq).expect("seq 在 u8 内"),
        );
    }
    // `set_sync_log_id` 的语义未变：生产入口仍拒绝跨代写入（restore 走显式迁移）。
    assert!(
        matches!(
            set_sync_log_id(&vault, &OTHER_LOG_ID),
            Err(StorageError::SyncLogMismatch { .. })
        ),
        "set_sync_log_id 必须继续拒绝跨代变更"
    );

    // B 的 seq1/seq2 都 <= received(2)，但 ready/count 只能看到 A 的 1/2。
    assert_eq!(ready_seqs(&vault, 16), vec![1, 2]);
    assert_eq!(
        count_inbox_ready(&vault).unwrap(),
        2,
        "旧 incarnation 的行即使 relay_seq<=received 也不得计入"
    );

    // A 的 seq3 落库 → received 只推进到 3，不跨 B 的 1..=5。
    assert_eq!(
        store_inbox_verified_batch(&vault, &[row(3, 0x33, 8)]).unwrap(),
        1
    );
    assert_eq!(received(&vault), 3, "received 前缀推进不跨 incarnation");
    assert_eq!(ready_seqs(&vault, 16), vec![1, 2, 3]);
    assert_eq!(count_inbox_ready(&vault).unwrap(), 3);

    // 状态变化同样只作用于当前 log：集成 A 的 seq1 后 B 的行仍不 ready、不影响计数。
    mark_integrated(&vault, &LOG_ID, 1).expect("集成 A seq1");
    assert_eq!(ready_seqs(&vault, 16), vec![2, 3]);
    assert_eq!(count_inbox_ready(&vault).unwrap(), 2);
}

/// `take_inbox_ready_where` 走同一条底层路径：谓词只能看到当前 log 的行。
#[test]
fn ready_where_path_filters_other_log_incarnation() {
    let (_dir, vault) = fresh_vault();
    bind_log(&vault);
    assert_eq!(store_inbox_verified_batch(&vault, &rows(3, 0)).unwrap(), 3);
    for seq in 1..=3u64 {
        insert_stale_incarnation_row(
            &vault,
            &OTHER_LOG_ID,
            seq,
            0xb0 + u8::try_from(seq).unwrap_or(0),
        );
    }

    // 恒真谓词 + 足够大的 limit：旧实现会把 B 的 3 行也交进来（6 条），现在必须只有 3 条。
    let mut observed_log_ids = Vec::new();
    let picked = take_inbox_ready_where(&vault, 16, |row| {
        observed_log_ids.push(row.log_id);
        true
    })
    .expect("where 路径读 ready");
    assert_eq!(
        picked.iter().map(|row| row.relay_seq).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    assert!(
        observed_log_ids.iter().all(|log_id| *log_id == LOG_ID),
        "谓词只应看到当前 incarnation 的行，实际 {observed_log_ids:?}"
    );

    // 谓词本身的过滤仍然生效（不是把 where 语义吞掉）。
    let picked: Vec<u64> = take_inbox_ready_where(&vault, 16, |row| row.relay_seq != 2)
        .expect("where 过滤")
        .into_iter()
        .map(|row| row.relay_seq)
        .collect();
    assert_eq!(picked, vec![1, 3]);
}

/// `sync.log_id` 缺失 / 长度非法时的语义：
/// - 缺失 → 空（`take`）/ 0（`count`），即新库旧行为；即使残留其它 log 的行与 received，
///   也不能回退成"不过滤 log"；
/// - 长度非法 → 明确 `Corrupt`（与 `sync_cursor_state` 同口径），不静默当作没有 log。
#[test]
fn ready_without_log_id_is_empty_and_invalid_length_is_corrupt() {
    let (_dir, vault) = fresh_vault();
    // 新库：没有 sync.log_id，也没有 inbox 行 → 空/0（旧实现同样返回空）。
    assert!(take_inbox_ready(&vault, 8)
        .expect("无 log 读 ready")
        .is_empty());
    assert_eq!(count_inbox_ready(&vault).expect("无 log 计数"), 0);

    // 不一致的持久状态：有遗留 verified 行与 received，却没有 sync.log_id。
    // 没有当前 incarnation 就没有可匹配的 log，ready 必须为空。
    insert_stale_incarnation_row(&vault, &OTHER_LOG_ID, 1, 0x5b);
    write_raw_meta(&vault, META_SYNC_RECEIVED_SEQ, &1u64.to_be_bytes());
    assert!(
        take_inbox_ready(&vault, 8)
            .expect("无 log 读 ready")
            .is_empty(),
        "缺 sync.log_id 时不能漏出其它 incarnation 的遗留行"
    );
    assert_eq!(count_inbox_ready(&vault).expect("无 log 计数"), 0);

    // 长度不是 16B = 损坏：显式错误，不静默当成"没有 log"。
    write_raw_meta(&vault, META_SYNC_LOG_ID, b"short");
    let error = take_inbox_ready(&vault, 8).expect_err("非法 log_id 长度必须报错");
    assert!(
        matches!(error, StorageError::Corrupt { .. }),
        "实际 {error:?}"
    );
    let error = count_inbox_ready(&vault).expect_err("非法 log_id 长度必须报错");
    assert!(
        matches!(error, StorageError::Corrupt { .. }),
        "实际 {error:?}"
    );
}
