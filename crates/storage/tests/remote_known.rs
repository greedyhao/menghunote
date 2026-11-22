//! P2-5 / P2-8：远端已知 VV 的单调写入与「来源标记」语义。
//!
//! 这里只走**公开**入口：`VaultJournalSink`（每次 flush 的 JournalOrigin 决定来源）
//! 与 `append_updates`（全部本地）。远端已知 VV 是 `meta` 里的非破坏性本地证据：
//!
//! - 本地写入（含已 ACK 的本地编辑）**不扩大**它；
//! - 只有远端条目把 `end_vv - 上一条 end_vv` 记进去；
//! - 没有出站意图的 flush（slots 空）不产生任何 outbox 行。

mod support;

use support::*;

use loro::VersionVector;

use menghunote_storage::{
    append_updates, create_body_doc, load_head, sync_cursor_state, BodyDoc, DurableSink,
    JournalEntry, SyncContext, UpdateRecord, Value, Vault, VaultJournalSink,
};

const CTX: SyncContext = SyncContext {
    peer_id: 0xA1CE_0001,
    key_epoch: 1,
};

fn remote_known(vault: &Vault, doc_id: &str) -> Option<VersionVector> {
    let rows = vault
        .query(
            "SELECT value FROM meta WHERE key=?1",
            &[Value::Text(format!("sync.remote_known_vv.{doc_id}"))],
        )
        .expect("meta 查询");
    match rows.first().and_then(|row| row.first()) {
        Some(Value::Blob(bytes)) => Some(VersionVector::decode(bytes).expect("解码远端已知 VV")),
        None => None,
        other => panic!("remote_known 值异常：{other:?}"),
    }
}

fn scalar(vault: &Vault, sql: &str) -> i64 {
    let rows = vault.query(sql, &[]).expect("查询");
    match rows.first().and_then(|row| row.first()) {
        Some(Value::Integer(value)) => *value,
        other => panic!("期望整数结果，实际 {other:?}"),
    }
}

/// 本地编辑的 `end_vv`（用于断言「本地写入不扩大远端已知 VV」）。
fn vv_of(update: &menghunote_storage::PendingUpdate) -> VersionVector {
    VersionVector::decode(&update.end_vv).expect("解码 end_vv")
}

fn local_entry(doc_id: &str, peer: u64, text: &str) -> (JournalEntry, VersionVector) {
    let doc = BodyDoc::create(doc_id, peer).expect("创建 BodyDoc");
    doc.insert_scalars(0, text).expect("本地编辑");
    let update = doc.pending_update(&VersionVector::default()).expect("导出");
    let vv = vv_of(&update);
    (JournalEntry::local(doc_id, "local-batch", update), vv)
}

fn remote_entry(
    doc_id: &str,
    peer: u64,
    base: &VersionVector,
    text: &str,
) -> (JournalEntry, VersionVector) {
    let doc = BodyDoc::create(doc_id, peer).expect("创建 BodyDoc");
    doc.insert_scalars(0, text).expect("远端编辑");
    let update = doc.pending_update(base).expect("导出远端增量");
    let vv = vv_of(&update);
    (JournalEntry::remote(doc_id, "remote-batch", update), vv)
}

/// 纯本地 flush（显式 `OriginMarks::AllLocal` 对应的公开路径）：不创建远端已知 VV。
#[test]
fn local_append_never_creates_remote_known() {
    let (_dir, vault) = fresh_vault();
    create_body_doc(&vault, "doc-local").expect("loro_docs 行");
    let doc = BodyDoc::create("doc-local", PEER_A).expect("创建 BodyDoc");
    doc.insert_scalars(0, "local only").expect("本地编辑");
    let current = load_head(&vault, "doc-local").expect("head");
    let update = doc
        .pending_update(&current.version_vector().expect("head vv"))
        .expect("导出");

    append_updates(
        &vault,
        "doc-local",
        current.head_seq,
        &[UpdateRecord::new("local-batch", update)],
    )
    .expect("A 事务");

    assert_eq!(
        remote_known(&vault, "doc-local"),
        None,
        "本地写入即使已经进 outbox 也不能扩大远端已知 VV"
    );
    assert_eq!(scalar(&vault, "SELECT count(*) FROM sync_outbox"), 0);
}

/// 远端条目（PerRecord 中 `true` 的那条）只把真正的远端增量记入远端已知 VV；
/// 同批的本地条目不记入。
#[test]
fn mixed_flush_extends_remote_known_only_for_remote_entries() {
    let (_dir, vault) = fresh_vault();
    create_body_doc(&vault, "doc-mixed").expect("loro_docs 行");

    let (local, local_vv) = local_entry("doc-mixed", PEER_A, "local");
    let (remote, remote_vv) = remote_entry("doc-mixed", PEER_B, &local_vv, "remote");
    let expected_remote = {
        // 远端记录带来的 ops = remote_vv - local_vv。
        let mut added = VersionVector::new();
        for span in remote_vv.sub_iter(&local_vv) {
            added.insert(span.peer, span.counter.end);
        }
        added
    };

    let mut sink = VaultJournalSink::new(vault.clone(), "doc-mixed", 0).with_sync_context(CTX);
    let head = sink.write_prefix(&[local, remote]).expect("混合 flush");
    assert_eq!(head, 2);

    let known = remote_known(&vault, "doc-mixed").expect("有远端条目就必须写");
    assert_eq!(known, expected_remote, "只能记入远端那一小段");
    assert_eq!(
        scalar(&vault, "SELECT count(*) FROM sync_outbox"),
        1,
        "本地条目产生 1 条意图，远端条目绝不能产生意图"
    );
    assert_eq!(sync_cursor_state(&vault).expect("cursor").received_seq, 0);
}

/// 纯远端 flush（`slots` 为空 = 没有出站意图）：扩展远端已知 VV，且不产生 outbox。
#[test]
fn remote_only_flush_with_empty_slots_has_no_outbox() {
    let (_dir, vault) = fresh_vault();
    create_body_doc(&vault, "doc-remote").expect("loro_docs 行");
    let (remote, remote_vv) =
        remote_entry("doc-remote", PEER_B, &VersionVector::default(), "remote");

    // 不带 sync context：slots 就是空切片（= 没有任何意图），过去与
    // 「remote_marks 空 = 全部本地」共用一个形状；现在来源由 OriginMarks 显式表达。
    let mut sink = VaultJournalSink::new(vault.clone(), "doc-remote", 0);
    let head = sink.write_prefix(&[remote]).expect("远端 flush");
    assert_eq!(head, 1);

    assert_eq!(remote_known(&vault, "doc-remote"), Some(remote_vv));
    assert_eq!(
        scalar(&vault, "SELECT count(*) FROM sync_outbox"),
        0,
        "slots 空 = 不产生任何出站意图"
    );
}

/// 同一 flush 连续两条远端记录：记入的是整段远端增量（`end - 前一条 end`），
/// 不是只记最后一条的绝对 VV。
#[test]
fn consecutive_remote_entries_record_the_whole_remote_delta() {
    let (_dir, vault) = fresh_vault();
    create_body_doc(&vault, "doc-remote-2").expect("loro_docs 行");

    let first_doc = BodyDoc::create("doc-remote-2", PEER_B).expect("创建 BodyDoc");
    first_doc.insert_scalars(0, "one").expect("第一次远端编辑");
    let first = first_doc
        .pending_update(&VersionVector::default())
        .expect("第一段");
    let first_vv = vv_of(&first);

    first_doc
        .insert_scalars(first_doc.scalar_len(), " two")
        .expect("第二次远端编辑");
    let second = first_doc.pending_update(&first_vv).expect("第二段");
    let end_vv = vv_of(&second);

    let mut sink = VaultJournalSink::new(vault.clone(), "doc-remote-2", 0);
    sink.write_prefix(&[
        JournalEntry::remote("doc-remote-2", "remote-1", first),
        JournalEntry::remote("doc-remote-2", "remote-2", second),
    ])
    .expect("两段远端 flush");

    assert_eq!(
        remote_known(&vault, "doc-remote-2"),
        Some(end_vv),
        "整段远端增量都必须记入，不能只记最后一条"
    );
    // 显式断言两条都写了（来源只有 remote，没有任何本地产物）。
    assert_eq!(scalar(&vault, "SELECT count(*) FROM loro_updates"), 2);
    assert_eq!(scalar(&vault, "SELECT count(*) FROM sync_outbox"), 0);
}
