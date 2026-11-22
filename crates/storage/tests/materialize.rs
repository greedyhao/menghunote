//! B 事务骨架：固定 frontier 物化、旧任务不覆盖新结果、启动重算、崩溃重放。

mod support;

use support::*;

use menghunote_storage::{
    load_body_state, load_head, materialize_body, pending_outbox, project_pending, reconcile,
    scan_stale_bodies, MaterializeOutcome,
};

/// 提交两段正文，返回编辑后的完整文本（= "A中😀e\u{301}"）。
fn seed_two_batches(
    vault: &menghunote_storage::Vault,
    doc: &menghunote_storage::BodyDoc,
) -> String {
    doc.insert_scalars(0, "A中").expect("第一段");
    commit_now(vault, doc, "batch-1");
    doc.insert_scalars(doc.scalar_len(), "😀e\u{301}")
        .expect("第二段");
    commit_now(vault, doc, "batch-2");
    doc.text()
}

#[test]
fn b_materializes_body_from_fixed_frontier() {
    let (_dir, vault) = fresh_vault();
    let doc = seed_note(&vault, "note-1", "doc-1");
    let full = seed_two_batches(&vault, &doc);

    // B 任务的目标是 seq 1：只应看到第一段的文本，不能偷偷读最新状态
    let outcome = materialize_body(&vault, "note-1", "doc-1", 1, PEER_A).expect("物化 seq1");
    assert_eq!(
        outcome,
        MaterializeOutcome::Applied {
            note_id: "note-1".to_owned(),
            body_seq: 1,
            body_scalars: 2
        }
    );
    let state = load_body_state(&vault, "note-1").expect("读 body 状态");
    assert_eq!(
        state.body_md.as_deref(),
        Some("A中"),
        "固定 frontier 只到 seq1"
    );
    assert_eq!(state.body_seq, 1);
    // outbox 的 target 是 2，seq1 的物化还不能清掉它
    let pending = pending_outbox(&vault).expect("读 outbox");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].target_seq, 2);

    // 再物化到 seq 2：文本追平，outbox 清空
    let outcome = materialize_body(&vault, "note-1", "doc-1", 2, PEER_A).expect("物化 seq2");
    assert_eq!(
        outcome,
        MaterializeOutcome::Applied {
            note_id: "note-1".to_owned(),
            body_seq: 2,
            body_scalars: 5
        }
    );
    let state = load_body_state(&vault, "note-1").expect("读 body 状态");
    assert_eq!(state.body_md.as_deref(), Some(full.as_str()));
    assert_eq!(
        state.body_frontier.as_deref(),
        Some(head(&vault, "doc-1").head_frontier.as_slice()),
        "body_frontier 必须等于 head_frontier"
    );
    assert!(pending_outbox(&vault).expect("读 outbox").is_empty());
}

#[test]
fn stale_b_task_cannot_overwrite_newer_result() {
    let (_dir, vault) = fresh_vault();
    let doc = seed_note(&vault, "note-1", "doc-1");
    let full = seed_two_batches(&vault, &doc);

    materialize_body(&vault, "note-1", "doc-1", 2, PEER_A).expect("物化 seq2");
    let newer = load_body_state(&vault, "note-1").expect("读状态");
    assert_eq!(newer.body_md.as_deref(), Some(full.as_str()));

    // 迟到的旧任务（target=1）不能覆盖新结果
    let outcome = materialize_body(&vault, "note-1", "doc-1", 1, PEER_A).expect("旧任务");
    assert_eq!(
        outcome,
        MaterializeOutcome::Stale {
            note_id: "note-1".to_owned(),
            body_seq: 2
        }
    );
    let after = load_body_state(&vault, "note-1").expect("读状态");
    assert_eq!(after, newer, "旧任务不得改动任何 body_* 字段");
}

#[test]
fn startup_scan_detects_mismatch_and_reconciles() {
    let (_dir, vault) = fresh_vault();
    let doc = seed_note(&vault, "note-1", "doc-1");
    let full = seed_two_batches(&vault, &doc);

    // 只做了 A、没做 B：启动扫描应发现
    let stale = scan_stale_bodies(&vault).expect("扫描");
    assert_eq!(stale.len(), 1);
    assert_eq!(stale[0].note_id, "note-1");
    assert_eq!(stale[0].head_seq, 2);
    assert_eq!(stale[0].body_seq, 0);

    let outcomes = reconcile(&vault, PEER_A).expect("重算");
    assert_eq!(outcomes.len(), 1);
    assert!(matches!(
        outcomes[0],
        MaterializeOutcome::Applied { body_seq: 2, .. }
    ));
    assert!(scan_stale_bodies(&vault).expect("再扫描").is_empty());

    // 人为把 body_frontier 改成垃圾：即使 body_seq 追平，也必须重算
    vault
        .exec(
            "UPDATE notes SET body_frontier = x'deadbeef' WHERE note_id = 'note-1'",
            &[],
        )
        .expect("制造 frontier 不一致");
    let stale = scan_stale_bodies(&vault).expect("扫描");
    assert_eq!(stale.len(), 1, "body_frontier ≠ head_frontier 时必须重算");
    reconcile(&vault, PEER_A).expect("重算");
    let state = load_body_state(&vault, "note-1").expect("读状态");
    assert_eq!(state.body_md.as_deref(), Some(full.as_str()));
    assert_eq!(
        state.body_frontier.as_deref(),
        Some(head(&vault, "doc-1").head_frontier.as_slice())
    );
}

#[test]
fn crash_after_a_before_b_recomputes_on_reopen() {
    let (dir, vault) = fresh_vault();
    let doc = seed_note(&vault, "note-1", "doc-1");
    let full = seed_two_batches(&vault, &doc);

    // 只有 A 落库，B 一个都没跑
    let state = load_body_state(&vault, "note-1").expect("读状态");
    assert_eq!(state.body_md, None, "尚未物化时 body_md 为 NULL");
    assert_eq!(state.body_seq, 0);
    assert_eq!(load_head(&vault, "doc-1").expect("head").head_seq, 2);

    // 模拟崩溃：直接丢句柄（连接在工作线程里关闭）
    drop(vault);

    let vault = open_vault(&vault_path(&dir));
    // 重开库后：A 的内容还在（loro_updates 2 行、head=2）
    assert_eq!(
        count(
            &vault,
            "SELECT count(*) FROM loro_updates WHERE doc_id = 'doc-1'",
            &[]
        ),
        2
    );
    assert_eq!(load_head(&vault, "doc-1").expect("head").head_seq, 2);

    // 启动重算把正文补齐，且 frontier 一致
    let outcomes = reconcile(&vault, PEER_A).expect("重算");
    assert_eq!(outcomes.len(), 1);
    let state = load_body_state(&vault, "note-1").expect("读状态");
    assert_eq!(state.body_md.as_deref(), Some(full.as_str()));
    assert_eq!(state.body_seq, 2);
    assert_eq!(
        state.body_frontier.as_deref(),
        Some(
            load_head(&vault, "doc-1")
                .expect("head")
                .head_frontier
                .as_slice()
        )
    );
    assert!(scan_stale_bodies(&vault).expect("扫描").is_empty());
}

#[test]
fn project_pending_walks_outbox() {
    let (_dir, vault) = fresh_vault();
    let doc = seed_note(&vault, "note-1", "doc-1");
    let full = seed_two_batches(&vault, &doc);

    let outcomes = project_pending(&vault, PEER_A).expect("B 一轮");
    assert_eq!(outcomes.len(), 1);
    assert!(matches!(
        outcomes[0],
        MaterializeOutcome::Applied { body_seq: 2, .. }
    ));
    assert_eq!(
        load_body_state(&vault, "note-1")
            .expect("状态")
            .body_md
            .as_deref(),
        Some(full.as_str())
    );

    // outbox 已清空：再跑一轮什么都不做
    assert!(project_pending(&vault, PEER_A)
        .expect("B 第二轮")
        .is_empty());
}
