//! A 事务与库内重放：`loro_docs` / `loro_updates` / `projection_outbox`。

mod support;

use loro::VersionVector;
use support::*;

use menghunote_storage::{
    append_updates, load_body_doc, load_head, store_snapshot, AffectedGuard, Statement,
    StorageError, UpdateRecord, Value,
};

const NFD: &str = "A中😀e\u{301}";

#[test]
fn a_transaction_writes_updates_head_and_outbox() {
    let (_dir, vault) = fresh_vault();
    let doc = seed_note(&vault, "note-1", "doc-1");
    doc.insert_scalars(0, NFD).expect("插入正文");

    let before = head(&vault, "doc-1");
    assert_eq!(before.head_seq, 0, "新文档从 seq 0 起步");
    assert!(before.head_frontier.is_empty(), "新文档 frontier 为空");

    let update = doc
        .pending_update(&VersionVector::default())
        .expect("导出增量");
    let new_head = append_updates(&vault, "doc-1", 0, &[UpdateRecord::new("batch-1", update)])
        .expect("A 事务");

    assert_eq!(new_head.head_seq, 1);

    // 1. loro_updates：一行，seq / update_id / codec 都对
    let rows = vault
        .query(
            "SELECT seq, update_id, batch_id, codec_version, length(payload) FROM loro_updates",
            &[],
        )
        .expect("查 loro_updates");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::Integer(1));
    assert_eq!(rows[0][1], Value::Text("doc-1#1".to_owned()));
    assert_eq!(rows[0][2], Value::Text("batch-1".to_owned()));
    assert_eq!(rows[0][3], Value::Text("loro-1.17.1".to_owned()));
    match rows[0][4] {
        Value::Integer(len) => assert!(len > 0, "payload 不能为空"),
        ref other => panic!("payload 长度类型异常：{other:?}"),
    }

    // 2. head 三件套与返回值、与内存文档一致
    let stored = load_head(&vault, "doc-1").expect("读 head");
    assert_eq!(stored, new_head);
    assert_eq!(
        loro::Frontiers::decode(&stored.head_frontier).expect("解码 frontier"),
        doc.frontiers(),
        "库里的 head_frontier 必须等于文档当前 frontier"
    );
    assert_eq!(
        loro::VersionVector::decode(&stored.head_vv).expect("解码 VV"),
        doc.vv()
    );

    // 3. projection_outbox 指向新 head
    assert_eq!(
        count(
            &vault,
            "SELECT target_seq FROM projection_outbox WHERE doc_id = 'doc-1'",
            &[]
        ),
        1
    );
}

#[test]
fn a_transaction_rolls_back_on_head_conflict() {
    let (_dir, vault) = fresh_vault();
    let doc = seed_note(&vault, "note-1", "doc-1");

    doc.insert_scalars(0, "甲").expect("第一次编辑");
    let first = doc
        .pending_update(&VersionVector::default())
        .expect("导出 first");
    append_updates(
        &vault,
        "doc-1",
        0,
        &[UpdateRecord::new("batch-1", first.clone())],
    )
    .expect("第一批");

    doc.insert_scalars(1, "乙").expect("第二次编辑");
    let base = VersionVector::decode(&first.end_vv).expect("解码 base");
    let second = doc.pending_update(&base).expect("导出 second");

    // 用**过期**的基准写：必须整笔回滚并报 HeadConflict
    let error = append_updates(&vault, "doc-1", 0, &[UpdateRecord::new("batch-2", second)])
        .expect_err("过期基准必须冲突");
    match error {
        StorageError::HeadConflict {
            doc_id,
            expected,
            actual,
        } => {
            assert_eq!(doc_id, "doc-1");
            assert_eq!(expected, 0);
            assert_eq!(actual, 1);
        }
        other => panic!("期望 HeadConflict，实际 {other}"),
    }

    // 冲突批次一行都不能留，head 也不能动
    assert_eq!(
        count(&vault, "SELECT count(*) FROM loro_updates", &[]),
        1,
        "冲突批次不得部分落库"
    );
    assert_eq!(head(&vault, "doc-1").head_seq, 1);
    let (replayed, _) = load_body_doc(&vault, "doc-1", PEER_A, None).expect("重放");
    assert_eq!(replayed.text(), "甲");
}

#[test]
fn a_transaction_rolls_back_when_any_statement_fails() {
    let (_dir, vault) = fresh_vault();

    // 第一条合法，第二条违反外键（doc_id 不存在）→ 两行都不能留下
    let statements = [
        Statement::new(
            "INSERT INTO meta(key, value) VALUES(?1, ?2)",
            vec![
                Value::Text("first".to_owned()),
                Value::Blob(b"1".to_vec()),
            ],
        ),
        Statement::new(
            "INSERT INTO loro_updates(doc_id, seq, update_id, batch_id, codec_version, payload, end_frontier, end_vv) \
             VALUES('missing-doc', 1, 'u1', 'b1', 'c', x'00', x'00', x'00')",
            vec![],
        ),
    ];
    let error = vault
        .exec_transaction(&statements, &[])
        .expect_err("外键违规应失败");
    assert!(
        matches!(error, StorageError::Sqlite(_)),
        "期望 SQLite 错误，实际 {error}"
    );
    assert_eq!(
        count(&vault, "SELECT count(*) FROM meta", &[]),
        0,
        "同事务的第一条语句必须一起回滚"
    );
}

#[test]
fn guard_failure_rolls_back_entire_transaction() {
    let (_dir, vault) = fresh_vault();

    let statements = [
        Statement::new(
            "INSERT INTO meta(key, value) VALUES(?1, ?2)",
            vec![
                Value::Text("guarded".to_owned()),
                Value::Blob(b"1".to_vec()),
            ],
        ),
        Statement::new(
            "UPDATE loro_docs SET head_seq = 9 WHERE doc_id = 'not-exist'",
            vec![],
        ),
    ];
    let error = vault
        .exec_transaction(&statements, &[AffectedGuard::exactly(1, 1)])
        .expect_err("守卫应失败");
    assert!(
        matches!(
            error,
            StorageError::GuardFailed {
                statement: 1,
                expected: 1,
                actual: 0
            }
        ),
        "期望 GuardFailed，实际 {error}"
    );
    assert_eq!(
        count(&vault, "SELECT count(*) FROM meta", &[]),
        0,
        "守卫失败必须回滚前面的语句"
    );
}

#[test]
fn replay_from_db_matches_live_text_after_reopen() {
    let (dir, vault) = fresh_vault();
    let doc = seed_note(&vault, "note-1", "doc-1");

    let mut base = VersionVector::default();
    for (index, chunk) in ["A中", "😀e\u{301}"].iter().enumerate() {
        doc.insert_scalars(doc.scalar_len(), chunk).expect("编辑");
        let update = doc.pending_update(&base).expect("导出增量");
        let current = head(&vault, "doc-1");
        append_updates(
            &vault,
            "doc-1",
            current.head_seq,
            &[UpdateRecord::new(format!("batch-{index}"), update.clone())],
        )
        .expect("A 事务");
        base = VersionVector::decode(&update.end_vv).expect("推进基准");
    }

    let (replayed, replayed_head) = load_body_doc(&vault, "doc-1", PEER_A, None).expect("重放");
    assert_eq!(replayed.text(), doc.text());
    assert_eq!(replayed.text(), NFD);
    assert_eq!(replayed_head.head_seq, 2);
    assert_eq!(replayed.frontiers(), doc.frontiers());

    // 关库重开：只靠 snapshot/updates 也要还原出同一文本与同一 frontier
    drop(vault);
    let vault = open_vault(&vault_path(&dir));
    let (again, again_head) = load_body_doc(&vault, "doc-1", PEER_A, None).expect("重开重放");
    assert_eq!(again.text(), doc.text());
    assert_eq!(again_head.head_seq, 2);
    assert_eq!(again.frontiers(), doc.frontiers());
}

#[test]
fn replay_reports_corruption_instead_of_guessing() {
    let (_dir, vault) = fresh_vault();
    let doc = seed_note(&vault, "note-1", "doc-1");
    doc.insert_scalars(0, "损坏检查").expect("编辑");
    commit_now(&vault, &doc, "batch-1");

    // 改坏 head_frontier：重放得到的 frontier 与记录不一致 → 必须报损坏，不许继续
    vault
        .exec(
            "UPDATE loro_docs SET head_frontier = x'0000' WHERE doc_id = 'doc-1'",
            &[],
        )
        .expect("制造损坏");
    let error = load_body_doc(&vault, "doc-1", PEER_A, None).expect_err("必须报损坏");
    assert!(
        matches!(error, StorageError::Corrupt { .. }),
        "期望 Corrupt，实际 {error}"
    );

    // seq 断档同样要报损坏（删掉中间那一行）
    let (_dir2, vault2) = fresh_vault();
    let doc2 = seed_note(&vault2, "note-2", "doc-2");
    doc2.insert_scalars(0, "甲乙").expect("编辑");
    let update = doc2
        .pending_update(&VersionVector::default())
        .expect("增量");
    append_updates(
        &vault2,
        "doc-2",
        0,
        &[UpdateRecord::new("b1", update.clone())],
    )
    .expect("A1");
    append_updates(&vault2, "doc-2", 1, &[UpdateRecord::new("b2", update)]).expect("A2");
    vault2
        .exec(
            "DELETE FROM loro_updates WHERE doc_id = 'doc-2' AND seq = 1",
            &[],
        )
        .expect("制造断档");
    let error = load_body_doc(&vault2, "doc-2", PEER_A, None).expect_err("断档必须报损坏");
    assert!(
        matches!(error, StorageError::Corrupt { .. }),
        "期望 Corrupt，实际 {error}"
    );
}

#[test]
fn fixed_frontier_replay_stops_at_requested_seq() {
    let (_dir, vault) = fresh_vault();
    let doc = seed_note(&vault, "note-1", "doc-1");

    doc.insert_scalars(0, "第一段").expect("编辑 1");
    commit_now(&vault, &doc, "batch-1");
    doc.insert_scalars(doc.scalar_len(), "第二段")
        .expect("编辑 2");
    commit_now(&vault, &doc, "batch-2");

    let (at_one, head_one) = load_body_doc(&vault, "doc-1", PEER_A, Some(1)).expect("重放到 1");
    assert_eq!(at_one.text(), "第一段", "固定 frontier 只看到 seq=1 的状态");
    assert_eq!(head_one.head_seq, 1);

    let (at_two, _) = load_body_doc(&vault, "doc-1", PEER_A, Some(2)).expect("重放到 2");
    assert_eq!(at_two.text(), doc.text());

    // 超过 head 的请求必须拒绝（不能假装能物化未来的 frontier）
    assert!(matches!(
        load_body_doc(&vault, "doc-1", PEER_A, Some(3)).expect_err("越界"),
        StorageError::Corrupt { .. }
    ));
}

#[test]
fn snapshot_plus_later_updates_replay_to_same_text() {
    let (_dir, vault) = fresh_vault();
    let doc = seed_note(&vault, "note-1", "doc-1");

    doc.insert_scalars(0, "第一段").expect("编辑 1");
    commit_now(&vault, &doc, "batch-1");
    doc.insert_scalars(doc.scalar_len(), "第二段")
        .expect("编辑 2");
    commit_now(&vault, &doc, "batch-2");

    // 取 seq=1 的状态做快照并落库（模拟 checkpoint）
    let (at_one, head_one) = load_body_doc(&vault, "doc-1", PEER_A, Some(1)).expect("重放到 1");
    let snapshot = at_one.export_snapshot().expect("导出快照");
    store_snapshot(&vault, "doc-1", 1, &head_one.head_frontier, &snapshot).expect("落快照");

    // 现在启动恢复应走「snapshot(seq1) + update(seq2)」路径，结果必须与整链一致
    let (restored, restored_head) =
        load_body_doc(&vault, "doc-1", PEER_A, None).expect("快照+增量重放");
    assert_eq!(restored.text(), doc.text());
    assert_eq!(restored_head.head_seq, 2);
    assert_eq!(restored.frontiers(), doc.frontiers());

    // 快照不能倒退覆盖
    let (at_one_again, head_one_again) =
        load_body_doc(&vault, "doc-1", PEER_A, Some(1)).expect("重放到 1");
    let older = at_one_again.export_snapshot().expect("旧快照");
    assert!(
        store_snapshot(&vault, "doc-1", 1, &head_one_again.head_frontier, &older).is_ok(),
        "同 seq 重复落快照应幂等"
    );
    assert!(
        store_snapshot(&vault, "doc-1", 0, &head_one_again.head_frontier, &older).is_err(),
        "seq 0 非法"
    );

    // 请求重放到快照之前必须报错（不能倒着物化）
    assert!(matches!(
        load_body_doc(&vault, "doc-1", PEER_A, Some(0)).expect_err("快照之前"),
        StorageError::Corrupt { .. }
    ));
}
