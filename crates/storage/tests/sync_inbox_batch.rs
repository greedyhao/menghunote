//! 批量 inbox API（Phase 0）：与逐条入口的语义一致性、冲突不半写、前缀推进。
//!
//! 批量入口 [menghunote_storage::store_inbox_verified_batch] 是 Phase 0 的实验/后续优化
//! 目标，当前生产 `pull` 默认仍逐条写；因此这里只断言**行为**与逐条完全一致，
//! 不对实现（几条 SQL/几次事务）做断言。

mod support;

use support::*;

use menghunote_storage::{
    count_inbox_ready, mark_skipped_event_received, set_sync_log_id, store_inbox_verified,
    store_inbox_verified_batch, sync_cursor_state, take_inbox_ready, Value, Vault,
    VerifiedInboxRow,
};

const LOG_ID: [u8; 16] = [0x3a; 16];

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
