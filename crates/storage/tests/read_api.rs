//! S7-7：storage 出站/bootstrap 只读 API。

mod support;

use support::*;

use menghunote_storage::{
    append_updates, body_doc_meta, doc_exists, doc_snapshot, doc_updates_since, doc_vv, load_head,
    loro_update_base_vv, loro_update_payload, read_doc_bootstrap, read_doc_for_export,
    store_snapshot, StorageError, UpdateRecord, APP_DOC_SCHEMA, CODEC_VERSION, MIN_READER,
};

fn assert_head_conflict(error: StorageError, expected: u64, actual: u64) {
    match error {
        StorageError::HeadConflict {
            expected: got_expected,
            actual: got_actual,
            ..
        } => {
            assert_eq!(got_expected, expected);
            assert_eq!(got_actual, actual);
        }
        other => panic!("期望 HeadConflict，实际 {other}"),
    }
}

#[test]
fn body_doc_meta_and_updates_match_appended_rows() {
    let (_dir, vault) = fresh_vault();
    let doc = seed_note(&vault, "note-1", "doc-1");

    let empty = body_doc_meta(&vault, "doc-1").expect("meta").expect("doc");
    assert_eq!(empty.generation, 1);
    assert_eq!(empty.codec_version, CODEC_VERSION);
    assert_eq!(empty.app_doc_schema, u32::try_from(APP_DOC_SCHEMA).unwrap());
    assert_eq!(empty.min_reader, u32::try_from(MIN_READER).unwrap());
    assert_eq!(empty.head_seq, 0);
    assert!(doc_updates_since(&vault, "doc-1", 0).unwrap().is_empty());

    doc.insert_scalars(0, "hello").expect("写正文");
    let head1 = commit_now(&vault, &doc, "batch-1");
    assert_eq!(head1.head_seq, 1);

    let meta1 = body_doc_meta(&vault, "doc-1").unwrap().unwrap();
    assert_eq!(meta1.head_seq, 1);
    assert_eq!(
        meta1.head_frontier.as_deref(),
        Some(head1.head_frontier.as_slice())
    );
    assert_eq!(meta1.head_vv.as_deref(), Some(head1.head_vv.as_slice()));

    let update1 = loro_update_payload(&vault, "doc-1", 1).expect("seq1");
    assert_eq!(update1.doc_id, "doc-1");
    assert_eq!(update1.seq, 1);
    assert_eq!(update1.update_id, "doc-1#1");
    assert_eq!(update1.batch_id, "batch-1");
    assert!(!update1.payload.is_empty());
    assert_eq!(update1.end_frontier, head1.head_frontier);
    assert_eq!(update1.end_vv, head1.head_vv);

    doc.insert_scalars(0, "!").expect("再写");
    let head2 = commit_now(&vault, &doc, "batch-2");
    assert_eq!(head2.head_seq, 2);
    let all = doc_updates_since(&vault, "doc-1", 0).unwrap();
    assert_eq!(
        all.iter().map(|update| update.seq).collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert_eq!(all[1].batch_id, "batch-2");
    assert_eq!(all[1].end_vv, head2.head_vv);

    let since1 = doc_updates_since(&vault, "doc-1", 1).unwrap();
    assert_eq!(since1.len(), 1);
    assert_eq!(since1[0].seq, 2);
    assert!(doc_updates_since(&vault, "doc-1", 2).unwrap().is_empty());
}

#[test]
fn payload_matches_the_appended_update_record() {
    let (_dir, vault) = fresh_vault();
    let doc = seed_note(&vault, "note-1", "doc-1");
    doc.insert_scalars(0, "exact").unwrap();
    let base = load_head(&vault, "doc-1")
        .unwrap()
        .version_vector()
        .unwrap();
    let pending = doc.pending_update(&base).expect("导出增量");
    let record = UpdateRecord::new("batch-exact", pending.clone());
    append_updates(&vault, "doc-1", 0, std::slice::from_ref(&record)).expect("落库");

    let stored = loro_update_payload(&vault, "doc-1", 1).expect("seq1");
    assert_eq!(stored.payload, pending.payload);
    assert_eq!(stored.end_frontier, pending.end_frontier);
    assert_eq!(stored.end_vv, pending.end_vv);
    assert_eq!(stored.update_id, "doc-1#1");
    assert_eq!(stored.batch_id, record.batch_id);
    assert_eq!(
        loro_update_base_vv(&vault, "doc-1", 1).unwrap(),
        Vec::<u8>::new()
    );
    assert_eq!(
        loro_update_base_vv(&vault, "doc-1", 2).unwrap(),
        pending.end_vv
    );
}

#[test]
fn export_requires_head_cas_and_never_returns_partial_snapshot() {
    let (_dir, vault) = fresh_vault();
    let doc = seed_note(&vault, "note-1", "doc-1");
    doc.insert_scalars(0, "abc").unwrap();
    let head1 = commit_now(&vault, &doc, "batch-1");

    let exported = read_doc_for_export(&vault, "doc-1", head1.head_seq).expect("export");
    assert_eq!(exported.meta.head_seq, 1);
    assert_eq!(exported.updates.len(), 1);
    assert_eq!(exported.updates[0].seq, 1);

    doc.insert_scalars(3, "d").unwrap();
    commit_now(&vault, &doc, "batch-2");

    let error = read_doc_for_export(&vault, "doc-1", 1).expect_err("旧 head 必须冲突");
    assert_head_conflict(error, 1, 2);

    let error = read_doc_for_export(&vault, "doc-1", 99).expect_err("未来 head 必须冲突");
    assert_head_conflict(error, 99, 2);

    // 空文档导出合法：head=0、updates 为空。
    seed_note(&vault, "note-empty", "doc-empty");
    let exported = read_doc_for_export(&vault, "doc-empty", 0).expect("empty export");
    assert_eq!(exported.meta.head_seq, 0);
    assert!(exported.updates.is_empty());
}

#[test]
fn empty_doc_and_bootstrap_snapshot_reads() {
    let (_dir, vault) = fresh_vault();
    let doc = seed_note(&vault, "note-1", "doc-1");
    assert!(doc_exists(&vault, "doc-1").unwrap());
    assert!(!doc_exists(&vault, "no-such-doc").unwrap());
    assert!(body_doc_meta(&vault, "no-such-doc").unwrap().is_none());
    assert!(doc_snapshot(&vault, "no-such-doc").unwrap().is_none());
    assert!(doc_vv(&vault, "no-such-doc").unwrap().is_none());
    assert!(loro_update_payload(&vault, "doc-1", 1).is_err());

    let boot = read_doc_bootstrap(&vault, "doc-1").unwrap().unwrap();
    assert!(boot.snapshot.is_none());
    assert_eq!(boot.snapshot_seq, 0);
    assert!(boot.updates_since_snapshot.is_empty());

    doc.insert_scalars(0, "hello").unwrap();
    let head1 = commit_now(&vault, &doc, "batch-1");
    doc.insert_scalars(5, "!").unwrap();
    let head2 = commit_now(&vault, &doc, "batch-2");

    store_snapshot(&vault, "doc-1", 1, &head1.head_frontier, b"snapshot-1").expect("存快照");
    let snapshot = doc_snapshot(&vault, "doc-1").unwrap().unwrap();
    assert_eq!(snapshot.snapshot.as_deref(), Some(b"snapshot-1".as_slice()));
    assert_eq!(snapshot.snapshot_seq, 1);
    assert_eq!(snapshot.head_seq, 2);
    assert_eq!(
        doc_vv(&vault, "doc-1").unwrap(),
        Some(head2.head_vv.clone())
    );

    let boot = read_doc_bootstrap(&vault, "doc-1").unwrap().unwrap();
    assert_eq!(boot.snapshot.as_deref(), Some(b"snapshot-1".as_slice()));
    assert_eq!(boot.snapshot_seq, 1);
    assert_eq!(
        boot.snapshot_frontier.as_deref(),
        Some(head1.head_frontier.as_slice())
    );
    assert_eq!(
        boot.updates_since_snapshot
            .iter()
            .map(|update| update.seq)
            .collect::<Vec<_>>(),
        vec![2]
    );
    assert_eq!(boot.meta.head_seq, 2);
}

#[test]
fn reads_are_isolated_across_documents() {
    let (_dir, vault) = fresh_vault();
    let first = seed_note(&vault, "note-1", "doc-1");
    let second = seed_note(&vault, "note-2", "doc-2");
    first.insert_scalars(0, "first").unwrap();
    commit_now(&vault, &first, "batch-1");
    second.insert_scalars(0, "second").unwrap();
    commit_now(&vault, &second, "batch-2");
    second.insert_scalars(0, "!").unwrap();
    commit_now(&vault, &second, "batch-3");

    assert_eq!(body_doc_meta(&vault, "doc-1").unwrap().unwrap().head_seq, 1);
    assert_eq!(body_doc_meta(&vault, "doc-2").unwrap().unwrap().head_seq, 2);

    let first_updates = doc_updates_since(&vault, "doc-1", 0).unwrap();
    assert_eq!(first_updates.len(), 1);
    assert_eq!(first_updates[0].doc_id, "doc-1");
    assert_eq!(first_updates[0].batch_id, "batch-1");

    let second_updates = doc_updates_since(&vault, "doc-2", 0).unwrap();
    assert_eq!(second_updates.len(), 2);
    assert!(second_updates.iter().all(|update| update.doc_id == "doc-2"));

    let first_export = read_doc_for_export(&vault, "doc-1", 1).expect("doc1 export");
    assert_eq!(first_export.updates.len(), 1);
    assert_eq!(first_export.updates[0].doc_id, "doc-1");
}
