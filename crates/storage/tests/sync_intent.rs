//! ADR 0009 切片 2b：**真实本地写入**在同一 A 事务里产生 `sync_outbox` 意图。
//!
//! 覆盖五件事（全部读**落库的 outbox 行**，不只看内存返回值）：
//!
//! 1. 正文 session 本地编辑 → flush 后 outbox 行数 = update 数，ID 非空、epoch 正确；
//! 2. 同事务：注入 head 冲突 → 整笔回滚，无 outbox 残留；
//! 3. 重试稳定性：失败重试后 message_id / object_id 逐字节不变；
//! 4. 结构 `commit` → 恰好 1 条 structure intent；
//! 5. remote / import / adopt 路径 → 0 条新 intent（防自激循环回归）。
//!
//! 纪律（AGENTS.md §6）：临时目录由 `support` 放在 `CARGO_TARGET_TMPDIR` 下；收敛点只有
//! `wait_for_durable`（生产自己的 `DurableAck`）与失败路径的**有界**等待，不 sleep 断言。

mod support;

use std::time::Duration;

use support::{commit_now, count, fresh_vault, open_vault, seed_note, vault_path, PEER_B};
use tempfile::TempDir;

use menghunote_storage::session::{
    ChangeSource, DocumentSession, EditRequest, FlushReason, LiveRev, ScalarEdit, SessionEpoch,
    SessionOptions,
};
use menghunote_storage::{
    append_updates, apply_baseline_batch, begin_baseline_apply, enqueue_sync_intent,
    finish_baseline_apply, intent_message_id, intent_object_id, load_body_doc, load_head,
    local_sync_intent, outbox_row, pending_encryption, BodyDoc, DurableSink, JournalEntry,
    JournalOrigin, StorageError, StructureWriter, SyncContext, SyncIntent, SyncOutboxState,
    UpdateRecord, Value, Vault, VaultJournalSink, STRUCTURE_DOC_ID,
};
use menghunote_text::UnicodeScalar;

/// 测试用同步上下文：peer 故意不用常量 1（既有默认值），epoch = 1（Phase 1）。
const CTX: SyncContext = SyncContext {
    peer_id: 0xA1CE_0001,
    key_epoch: 1,
};

/// 新的空 sync 上下文（`peer`/`epoch` 由调用方给）。
fn edits(start: usize, end: usize, replacement: &str) -> ScalarEdit {
    ScalarEdit {
        start: UnicodeScalar::new(start),
        end: UnicodeScalar::new(end),
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

fn options(peer: u64, sync: Option<SyncContext>) -> SessionOptions {
    SessionOptions {
        peer_id: peer,
        session_epoch: SessionEpoch(9),
        sync,
        ..SessionOptions::default()
    }
}

fn outbox_count(vault: &Vault) -> i64 {
    count(vault, "SELECT count(*) FROM sync_outbox", &[])
}

fn updates_count(vault: &Vault, doc_id: &str) -> i64 {
    count(
        vault,
        "SELECT count(*) FROM loro_updates WHERE doc_id=?1",
        &[Value::Text(doc_id.to_owned())],
    )
}

fn assert_hex_message_id(message_id: &str) {
    assert_eq!(
        message_id.len(),
        32,
        "message_id 必须是 16B 的 32 位 hex：{message_id:?}"
    );
    assert!(
        message_id.chars().all(|ch| ch.is_ascii_hexdigit()),
        "engine 的 decode_message_id 只接受 hex/UUID 文本，实际 {message_id:?}"
    );
}

/// 造一条"远端设备"的 update（独立副本 → 导出）。
fn remote_update(doc_id: &str, peer: u64, text: &str) -> Vec<u8> {
    let doc = BodyDoc::create(doc_id, peer).expect("远端副本");
    doc.insert_scalars(0, text).expect("远端编辑");
    doc.export_all_updates().expect("导出 update")
}

fn fresh() -> (TempDir, Vault) {
    fresh_vault()
}

// ---------------------------------------------------------------------------
// 1) 正文 session 本地编辑 → flush → 每条 update 一条 intent
// ---------------------------------------------------------------------------

#[test]
fn body_session_flush_writes_one_intent_per_local_update() {
    let (_dir, vault) = fresh();
    seed_note(&vault, "note-1", "doc-1");

    let mut session =
        DocumentSession::open(&vault, "doc-1", options(CTX.peer_id, Some(CTX))).expect("打开会话");

    // 两次本地编辑 = 两条 update。
    session.apply(request(&session, "c1", 0, vec![edits(0, 0, "甲")]));
    session.apply(request(&session, "c2", 1, vec![edits(0, 0, "乙")]));
    let ticket = session.flush(FlushReason::Manual);
    assert_eq!(ticket.target_live_rev, LiveRev(2));
    let ack = session
        .wait_for_durable(LiveRev(2), Duration::from_secs(5))
        .expect("本地 flush 必须收敛到 DurableAck");

    assert_eq!(ack.head_seq, 2, "两条 update 必须 durable");
    assert_eq!(
        outbox_count(&vault),
        2,
        "一 update 一 intent（不是一条批意图）"
    );
    assert_eq!(updates_count(&vault, "doc-1"), 2);

    let rows = pending_encryption(&vault, 10).expect("pending 行");
    assert_eq!(rows.len(), 2);
    for (index, row) in rows.iter().enumerate() {
        let seq = index as u64 + 1;
        assert_eq!(row.doc_id, "doc-1");
        assert_eq!(
            row.seq, seq,
            "intent 的 seq 必须与 loro_updates.seq 一一对应"
        );
        assert_eq!(row.key_epoch, CTX.key_epoch, "epoch 必须来自 SyncContext");
        assert_eq!(
            row.message_id,
            intent_message_id("doc-1", seq, CTX),
            "message_id 必须是 (doc,seq,peer,epoch) 的确定性派生"
        );
        assert_hex_message_id(&row.message_id);
        assert_eq!(
            row.object_id,
            Some(intent_object_id(&row.message_id, CTX)),
            "object_id 必须是 32B 确定性派生"
        );
        assert!(
            row.object_id.is_some_and(|value| value != [0; 32]),
            "object_id 不得是全零"
        );
        assert_eq!(row.state, SyncOutboxState::Pending);
        assert!(
            row.batch_id.starts_with("session-"),
            "batch_id 必须沿用 UpdateRecord.batch_id，实际 {:?}",
            row.batch_id
        );
        // batch 与 loro_updates 行逐一致（同一次 A 事务写出的两份记录必须对齐）。
        let db_batch = vault
            .query(
                "SELECT batch_id FROM loro_updates WHERE doc_id='doc-1' AND seq=?1",
                &[Value::Integer(i64::try_from(seq).expect("seq"))],
            )
            .expect("读 batch_id");
        assert_eq!(db_batch[0][0], Value::Text(row.batch_id.clone()));
    }
    assert_ne!(
        rows[0].message_id, rows[1].message_id,
        "不同 seq 必须得到不同 message"
    );

    // 默认路径（sync = None）：一条都不写，行为与切片 2a 之前一致。
    let (_dir2, vault2) = fresh();
    seed_note(&vault2, "note-1", "doc-1");
    let mut plain = DocumentSession::open(&vault2, "doc-1", options(1, None)).expect("默认会话");
    plain.apply(request(&plain, "c1", 0, vec![edits(0, 0, "甲")]));
    plain.flush(FlushReason::Manual);
    assert!(
        plain
            .wait_for_durable(LiveRev(1), Duration::from_secs(5))
            .is_some(),
        "默认会话仍必须正常 flush"
    );
    assert_eq!(outbox_count(&vault2), 0, "sync=None 时不得产生任何 intent");
}

// ---------------------------------------------------------------------------
// 2) 同事务：head 冲突 → 无 outbox 残留
// ---------------------------------------------------------------------------

#[test]
fn head_conflict_rolls_back_intents_in_same_transaction() {
    let (_dir, vault) = fresh();
    seed_note(&vault, "note-1", "doc-1");

    let mut session =
        DocumentSession::open(&vault, "doc-1", options(CTX.peer_id, Some(CTX))).expect("打开会话");
    session.apply(request(&session, "c1", 0, vec![edits(0, 0, "甲")]));

    // 注入 head 冲突：session 看不到的"外部 append"（engine integrate 的等价物）先推进 head。
    let external = BodyDoc::create("doc-1", PEER_B).expect("外部副本");
    external.insert_scalars(0, "外").expect("外部编辑");
    let update = external
        .pending_update(&loro::VersionVector::default())
        .expect("外部增量");
    append_updates(&vault, "doc-1", 0, &[UpdateRecord::new("external", update)])
        .expect("外部 append");
    assert_eq!(load_head(&vault, "doc-1").expect("head").head_seq, 1);

    session.flush(FlushReason::Manual);
    // 失败路径没有 DurableAck：有界等待后必须是 None（不是"睡够时间再断言"）。
    assert!(
        session
            .wait_for_durable(LiveRev(1), Duration::from_millis(300))
            .is_none(),
        "head 冲突的 flush 不得产生 DurableAck"
    );

    assert_eq!(outbox_count(&vault), 0, "A 事务回滚后不得残留 intent");
    assert_eq!(
        updates_count(&vault, "doc-1"),
        1,
        "只有外部 append 的那一条 update"
    );

    // sink 层同样验证：过期 CAS 基准 → HeadConflict，且一行 intent 都不落。
    let mut stale_sink = VaultJournalSink::new(vault.clone(), "doc-1", 0).with_sync_context(CTX);
    let replay = load_body_doc(&vault, "doc-1", CTX.peer_id, None)
        .expect("重放")
        .0;
    let stale_update = replay
        .pending_update(&loro::VersionVector::default())
        .expect("增量");
    let error = stale_sink
        .write_prefix(&[JournalEntry::local("doc-1", "stale", stale_update)])
        .expect_err("过期 CAS 基准必须报 HeadConflict");
    assert!(
        matches!(error, StorageError::HeadConflict { .. }),
        "期望 HeadConflict，实际 {error:?}"
    );
    assert_eq!(outbox_count(&vault), 0, "过期基准那次必须整笔回滚");
}

// ---------------------------------------------------------------------------
// 3) 重试稳定性：同一 (doc, seq, ctx) → 同一 message/object id
// ---------------------------------------------------------------------------

#[test]
fn intent_ids_are_deterministic_and_stable_across_retry() {
    // 纯函数层面：同一输入永远同一输出；peer / epoch / seq 任一变则 ID 变。
    let first = local_sync_intent("doc-1", 3, "batch-1", CTX).expect("派生");
    let again = local_sync_intent("doc-1", 3, "batch-1", CTX).expect("派生");
    assert_eq!(first, again, "同一批次重试必须得到同一 ID");
    assert_eq!(
        first.message_id,
        intent_message_id("doc-1", 3, CTX),
        "message_id 是纯函数"
    );

    let other_peer = SyncContext::new(CTX.peer_id + 1, CTX.key_epoch);
    let other_epoch = SyncContext::new(CTX.peer_id, 2);
    assert_ne!(
        first.message_id,
        local_sync_intent("doc-1", 3, "batch-1", other_peer)
            .expect("派生")
            .message_id,
        "peer 必须参与派生"
    );
    assert_ne!(
        first.message_id,
        local_sync_intent("doc-1", 3, "batch-1", other_epoch)
            .expect("派生")
            .message_id,
        "epoch 必须参与派生"
    );
    assert_ne!(
        first.message_id,
        local_sync_intent("doc-1", 4, "batch-1", CTX)
            .expect("派生")
            .message_id,
        "seq 必须参与派生"
    );
    assert_eq!(
        first.object_id,
        local_sync_intent("doc-1", 3, "batch-2", CTX)
            .expect("派生")
            .object_id,
        "object_id 只由 message/peer/epoch 决定（batch 改名不影响）"
    );

    // 非法上下文：epoch = 0（禁止常量）与 peer = 0 必须明确报错。
    assert!(SyncContext::new(1, 0).validate().is_err());
    assert!(SyncContext::new(0, 1).validate().is_err());
    assert!(local_sync_intent("doc-1", 3, "b", SyncContext::new(1, 0)).is_err());

    // 事务层面：第一次因 message_id 唯一冲突整笔失败 → 清掉冲突 → 重试得到同样 ID。
    let (_dir, vault) = fresh();
    let body = seed_note(&vault, "note-1", "doc-1");
    body.insert_scalars(0, "甲").expect("编辑");
    let update = body
        .pending_update(&loro::VersionVector::default())
        .expect("增量");
    let expected = local_sync_intent("doc-1", 1, "batch-1", CTX).expect("派生");

    // 造一个占位冲突：用另一篇文档的真实 update 承载同一个 message_id（PK 冲突）。
    let other = seed_note(&vault, "note-2", "doc-2");
    other.insert_scalars(0, "乙").expect("编辑");
    let other_update = other
        .pending_update(&loro::VersionVector::default())
        .expect("增量");
    append_updates(
        &vault,
        "doc-2",
        0,
        &[UpdateRecord::new("other", other_update)],
    )
    .expect("另一篇 update");
    enqueue_sync_intent(
        &vault,
        &expected.message_id,
        "clash",
        "doc-2",
        1,
        1,
        &[7; 32],
    )
    .expect("占位冲突行");

    let mut sink = VaultJournalSink::new(vault.clone(), "doc-1", 0).with_sync_context(CTX);
    let entry = JournalEntry::local("doc-1", "batch-1", update);
    let error = sink
        .write_prefix(std::slice::from_ref(&entry))
        .expect_err("message_id 冲突必须整笔失败");
    assert!(
        matches!(error, StorageError::Sqlite(_)),
        "期望唯一约束失败，实际 {error:?}"
    );
    assert_eq!(updates_count(&vault, "doc-1"), 0, "失败事务不得写入 update");
    assert_eq!(outbox_count(&vault), 1, "只剩占位冲突行");

    // 清掉冲突后重试：同一批次、同一 seq → message/object 与失败前派生值逐字节相同。
    vault
        .exec(
            "DELETE FROM sync_outbox WHERE message_id=?1",
            &[Value::Text(expected.message_id.clone())],
        )
        .expect("清掉冲突行");
    let mut retry_sink = VaultJournalSink::new(vault.clone(), "doc-1", 0).with_sync_context(CTX);
    let seq = retry_sink
        .write_prefix(std::slice::from_ref(&entry))
        .expect("重试必须成功");
    assert_eq!(seq, 1);
    let row = outbox_row(&vault, &expected.message_id)
        .expect("读 outbox")
        .expect("重试后必须有 intent");
    assert_eq!(row.seq, 1);
    assert_eq!(row.key_epoch, CTX.key_epoch);
    assert_eq!(
        row.object_id,
        Some(expected.object_id),
        "重试后 object_id 必须不变"
    );
    assert_eq!(outbox_count(&vault), 1);
}

// ---------------------------------------------------------------------------
// 4) 结构 commit → 1 条 structure intent
// ---------------------------------------------------------------------------

#[test]
fn structure_commit_writes_exactly_one_intent_when_enabled() {
    let (_dir, vault) = fresh();
    let mut writer =
        StructureWriter::open_with_sync(&vault, CTX.peer_id, Some(CTX)).expect("打开结构写入口");
    writer.doc().create_note("笔记", 1).expect("建笔记");
    writer.commit("struct-1").expect("提交结构");
    assert_eq!(outbox_count(&vault), 1, "结构提交恰好 1 条 intent");

    let expected = local_sync_intent(STRUCTURE_DOC_ID, 1, "struct-1", CTX).expect("派生");
    let row = outbox_row(&vault, &expected.message_id)
        .expect("读 outbox")
        .expect("必须有结构 intent");
    assert_eq!(row.doc_id, STRUCTURE_DOC_ID);
    assert_eq!(row.seq, 1);
    assert_eq!(row.key_epoch, CTX.key_epoch);
    assert_eq!(row.batch_id, "struct-1");
    assert_eq!(row.object_id, Some(expected.object_id));
    assert_hex_message_id(&row.message_id);

    // 没有新 op 的 commit 只补投影 → 不得新增 intent。
    writer.commit("struct-noop").expect("空提交");
    assert_eq!(outbox_count(&vault), 1, "没有新 op 不得产生新 intent");

    // 再来一次真实结构 op → 第二条（seq = head+1）。
    writer.doc().create_folder("目录", 2).expect("建目录");
    writer.commit("struct-2").expect("提交");
    assert_eq!(outbox_count(&vault), 2, "第二次结构 op → 第二条 intent");
    let second = local_sync_intent(STRUCTURE_DOC_ID, 2, "struct-2", CTX).expect("派生");
    assert_eq!(
        outbox_row(&vault, &second.message_id)
            .expect("读 outbox")
            .expect("第二条")
            .seq,
        2
    );

    // 默认路径（open 不带 sync）：结构提交不产生 intent。
    let (_dir2, vault2) = fresh();
    let mut plain = StructureWriter::open(&vault2, CTX.peer_id).expect("默认结构写入口");
    plain.doc().create_note("笔记", 1).expect("建笔记");
    plain.commit("struct-1").expect("提交");
    assert_eq!(
        outbox_count(&vault2),
        0,
        "sync=None 的结构提交不得产生 intent"
    );
}

// ---------------------------------------------------------------------------
// 5) remote / import / adopt → 0 条新 intent
// ---------------------------------------------------------------------------

#[test]
fn remote_import_and_adopt_never_produce_intents() {
    // a) 纯 remote：session 启用 sync，但只收到远端导入 → 0 intent。
    let (_dir, vault) = fresh();
    seed_note(&vault, "note-1", "doc-1");
    let mut session =
        DocumentSession::open(&vault, "doc-1", options(CTX.peer_id, Some(CTX))).expect("打开会话");
    let delta = session
        .apply_remote(&remote_update("doc-1", PEER_B, "远端"))
        .expect("导入远端")
        .expect("应当产生 LiveDelta");
    assert_eq!(delta.source, ChangeSource::Remote);
    session.flush(FlushReason::Manual);
    session
        .wait_for_durable(LiveRev(1), Duration::from_secs(5))
        .expect("远端导入也必须 durable");
    assert_eq!(
        updates_count(&vault, "doc-1"),
        1,
        "远端 update 必须落库（只是没有 intent）"
    );
    assert_eq!(outbox_count(&vault), 0, "remote 导入绝不能产生 intent");

    // b) 混合前缀（本地 + remote 同一次 flush）：只有本地那条有 intent，
    //    且 intent 的 seq 指向本地 update 在 loro_updates 里的真实 seq。
    let (_dir2, vault2) = fresh();
    seed_note(&vault2, "note-1", "doc-1");
    let mut mixed =
        DocumentSession::open(&vault2, "doc-1", options(CTX.peer_id, Some(CTX))).expect("打开会话");
    mixed.apply(request(&mixed, "c1", 0, vec![edits(0, 0, "本")]));
    mixed
        .apply_remote(&remote_update("doc-1", PEER_B, "远"))
        .expect("导入")
        .expect("delta");
    mixed.flush(FlushReason::Manual);
    mixed
        .wait_for_durable(LiveRev(2), Duration::from_secs(5))
        .expect("混合前缀必须 durable");
    assert_eq!(updates_count(&vault2, "doc-1"), 2);
    assert_eq!(outbox_count(&vault2), 1, "混合前缀里只有本地条目有 intent");
    let rows = pending_encryption(&vault2, 10).expect("pending");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].seq, 1, "intent 必须指向本地那条 update 的 seq");
    assert_eq!(
        rows[0].message_id,
        intent_message_id("doc-1", 1, CTX),
        "本地条目的 seq 不得被 remote 条目挤偏"
    );
    let batches = vault2
        .query(
            "SELECT batch_id FROM loro_updates WHERE doc_id='doc-1' ORDER BY seq",
            &[],
        )
        .expect("batch ids");
    assert_eq!(batches.len(), 2);
    assert!(
        matches!(&batches[1][0], Value::Text(value) if value.starts_with("remote-")),
        "远端条目仍按 remote 批次落库：{:?}",
        batches[1][0]
    );

    // c) adopt（baseline apply）：整批 merge 只有 loro_updates，没有 intent。
    let (_dir3, vault3) = fresh();
    let local = seed_note(&vault3, "note-1", "doc-1");
    local.insert_scalars(0, "本地").expect("本地编辑");
    commit_now(&vault3, &local, "local-batch");
    let baseline = {
        let doc = BodyDoc::create("doc-1", PEER_B).expect("基线副本");
        doc.insert_scalars(0, "基线").expect("写基线");
        doc.export_snapshot().expect("导出 snapshot")
    };
    begin_baseline_apply(&vault3, &[7; 16], 3, "cursor-1").expect("begin");
    apply_baseline_batch(&vault3, &[("doc-1".to_owned(), baseline)], &[]).expect("apply");
    let _ = finish_baseline_apply(&vault3, 3, "cursor-2").expect("finish");
    assert_eq!(outbox_count(&vault3), 0, "adopt/baseline 绝不能产生 intent");
    assert!(
        updates_count(&vault3, "doc-1") >= 2,
        "adopt 必须真的写了 update（否则本断言没有判别力）"
    );

    // d) import 风格的裸 `append_updates`（engine integrate / rollback 等价物）：
    //    即使库里存在同步上下文，也不得凭空产生 intent。
    let head = load_head(&vault3, "doc-1").expect("head");
    let (doc, _) = load_body_doc(&vault3, "doc-1", CTX.peer_id, None).expect("重放");
    let base = head.version_vector().expect("vv");
    doc.insert_scalars(0, "导").expect("编辑");
    let update = doc.pending_update(&base).expect("增量");
    append_updates(
        &vault3,
        "doc-1",
        head.head_seq,
        &[UpdateRecord::new("import", update)],
    )
    .expect("import 追加");
    assert_eq!(outbox_count(&vault3), 0, "import 路径不得产生 intent");
}

// ---------------------------------------------------------------------------
// 显式断言：JournalOrigin 是 intent 的唯一闸门
// ---------------------------------------------------------------------------

#[test]
fn journal_origin_is_the_only_intent_gate() {
    assert!(JournalOrigin::Local.produces_sync_intent());
    assert!(!JournalOrigin::Remote.produces_sync_intent());

    let (_dir, vault) = fresh();
    let body = seed_note(&vault, "note-1", "doc-1");
    body.insert_scalars(0, "甲").expect("编辑");
    let update = body
        .pending_update(&loro::VersionVector::default())
        .expect("增量");
    // 即使 sink 启用了 sync，Remote 来源的条目也不写 intent。
    let mut sink = VaultJournalSink::new(vault.clone(), "doc-1", 0).with_sync_context(CTX);
    let seq = sink
        .write_prefix(&[JournalEntry::remote("doc-1", "remote-1", update)])
        .expect("远端条目写 durable");
    assert_eq!(seq, 1);
    assert_eq!(outbox_count(&vault), 0, "Remote 条目不得产生 intent");

    // 同一 sink 上随后写本地条目：intent 的 seq 必须跟着真实 head 前进。
    let body = load_body_doc(&vault, "doc-1", CTX.peer_id, None)
        .expect("重放")
        .0;
    let base = load_head(&vault, "doc-1")
        .expect("head")
        .version_vector()
        .expect("vv");
    body.insert_scalars(0, "乙").expect("编辑");
    let update = body.pending_update(&base).expect("增量");
    let seq = sink
        .write_prefix(&[JournalEntry::local("doc-1", "local-1", update)])
        .expect("本地条目写 durable");
    assert_eq!(seq, 2);
    assert_eq!(outbox_count(&vault), 1);
    assert_eq!(
        outbox_row(&vault, &intent_message_id("doc-1", 2, CTX))
            .expect("读 outbox")
            .expect("本地 intent")
            .seq,
        2,
        "本地 intent 必须指向它在 loro_updates 里的真实 seq"
    );
}

/// `SyncIntent` 仍是可构造的公开类型（sync crate 的 e2e 夹具依赖它）。
#[test]
fn sync_intent_is_constructible_from_outside() {
    let intent = SyncIntent {
        message_id: intent_message_id("doc-1", 1, CTX),
        batch_id: "b".to_owned(),
        key_epoch: CTX.key_epoch,
        object_id: [3; 32],
    };
    assert_eq!(intent.key_epoch, 1);
    assert!(!intent.message_id.is_empty());
}

// ---------------------------------------------------------------------------
// 6) 旧库探测：只读、不建库、不迁移（app 的旧库拒绝用它）
// ---------------------------------------------------------------------------

#[test]
fn existing_vault_content_probe_is_read_only_and_accurate() {
    use menghunote_storage::existing_vault_has_content;

    let (dir, vault) = fresh();
    let missing = dir.path().join("never-created.sqlite3");
    assert!(
        !existing_vault_has_content(&missing).expect("探测不存在的库"),
        "不存在的文件必须报 false"
    );
    assert!(!missing.exists(), "探测不得创建库文件");

    // 空库（建好 schema、零行）：允许，且探测前后文件字节不变。
    let empty_path = vault_path(&dir);
    drop(vault);
    let before = std::fs::read(&empty_path).expect("读空库字节");
    assert!(
        !existing_vault_has_content(&empty_path).expect("探测空库"),
        "只有 schema 的空库必须报 false"
    );
    assert_eq!(
        std::fs::read(&empty_path).expect("复读空库字节"),
        before,
        "只读探测不得改写库文件"
    );

    // 有内容（一篇笔记 + 一条 update）：必须报 true。
    let vault = open_vault(&empty_path);
    let body = seed_note(&vault, "note-1", "doc-1");
    body.insert_scalars(0, "甲").expect("编辑");
    commit_now(&vault, &body, "b1");
    drop(vault);
    assert!(
        existing_vault_has_content(&empty_path).expect("探测有内容的库"),
        "有任何 loro_updates/notes 行的库必须报 true"
    );

    // 0 字节文件（例如创建过程被打断）：SQLite 视为空库 → 允许。
    let zero = dir.path().join("zero-bytes.sqlite3");
    std::fs::write(&zero, []).expect("写 0 字节文件");
    assert!(
        !existing_vault_has_content(&zero).expect("探测 0 字节库"),
        "0 字节文件必须当成空库"
    );

    // 不是 SQLite 文件：明确报错（调用方 fail-closed）。
    let garbage = dir.path().join("garbage.sqlite3");
    std::fs::write(&garbage, b"not a sqlite database at all").expect("写垃圾文件");
    assert!(
        existing_vault_has_content(&garbage).is_err(),
        "非 SQLite 文件必须报错，不能当成空库"
    );
}

#[test]
fn invalid_sync_context_fails_at_open_not_at_flush() {
    let (_dir, vault) = fresh();
    seed_note(&vault, "note-1", "doc-1");
    let bad = SyncContext::new(CTX.peer_id, 0);
    assert!(
        DocumentSession::open(&vault, "doc-1", options(CTX.peer_id, Some(bad))).is_err(),
        "epoch=0 的上下文必须在打开会话时就报错"
    );
    assert!(
        StructureWriter::open_with_sync(&vault, CTX.peer_id, Some(bad)).is_err(),
        "epoch=0 的上下文必须在打开结构写入口时就报错"
    );
    assert_eq!(outbox_count(&vault), 0, "打开失败不得写任何 intent");
}
