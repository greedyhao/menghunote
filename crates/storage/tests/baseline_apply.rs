//! S7-34：已验证基线分批 merge、tombstone 保守删除、cursor CAS 与崩溃续跑。

mod support;

use support::*;

use menghunote_storage::{
    abort_baseline_apply, apply_baseline_assets, apply_baseline_batch, baseline_apply_state,
    begin_baseline_apply, finish_baseline_apply, load_body_doc, sync_cursor_state,
    BaselineApplyFinish, BaselineApplyStatus, BaselineAsset, BodyDoc, Value, Vault,
    META_SYNC_CONFIRMED_HIGHWATER, META_SYNC_INTEGRATED_SEQ, META_SYNC_RECEIVED_SEQ,
    META_SYNC_RELAY_CURSOR, SCHEMA_VERSION,
};

fn snapshot(doc_id: &str, peer: u64, text: &str) -> Vec<u8> {
    let doc = BodyDoc::create(doc_id, peer).expect("创建基线 LoroDoc");
    doc.insert_scalars(0, text).expect("写基线正文");
    doc.export_snapshot().expect("导出基线 snapshot")
}

fn begin(vault: &Vault, s: u64, cursor: &str) {
    begin_baseline_apply(vault, &[7; 16], s, cursor).expect("begin baseline");
}

/// 模拟"已有库"的同步水位；`None` 表示 meta 缺省。
fn seed_watermarks(
    vault: &Vault,
    received: u64,
    integrated: u64,
    highwater: Option<u64>,
    relay_cursor: Option<&str>,
) {
    vault
        .set_meta(META_SYNC_RECEIVED_SEQ, &received.to_be_bytes())
        .expect("set received");
    vault
        .set_meta(META_SYNC_INTEGRATED_SEQ, &integrated.to_be_bytes())
        .expect("set integrated");
    if let Some(highwater) = highwater {
        vault
            .set_meta(META_SYNC_CONFIRMED_HIGHWATER, &highwater.to_be_bytes())
            .expect("set highwater");
    }
    if let Some(relay_cursor) = relay_cursor {
        vault
            .set_meta(META_SYNC_RELAY_CURSOR, relay_cursor.as_bytes())
            .expect("set relay cursor");
    }
}

fn relay_cursor(vault: &Vault) -> Option<Vec<u8>> {
    vault
        .get_meta(META_SYNC_RELAY_CURSOR)
        .expect("relay cursor")
}

fn notes_deleted(vault: &Vault, doc_id: &str) -> i64 {
    let rows = vault
        .query(
            "SELECT deleted FROM notes WHERE body_doc_id=?1",
            &[Value::Text(doc_id.to_owned())],
        )
        .expect("notes 查询");
    match rows.first().and_then(|row| row.first()) {
        Some(Value::Integer(value)) => *value,
        other => panic!("notes.deleted 异常：{other:?}"),
    }
}

fn local_doc(vault: &Vault, doc_id: &str, text: &str) -> BodyDoc {
    let doc = seed_note(vault, &format!("note-{doc_id}"), doc_id);
    doc.insert_scalars(0, text).expect("本地编辑");
    commit_now(vault, &doc, "local-batch");
    doc
}

#[test]
fn baseline_merge_preserves_local_edit_and_advances_cursor() {
    let (_dir, vault) = fresh_vault();
    let local = local_doc(&vault, "doc-1", "local");
    let snap = snapshot("doc-1", PEER_B, "remote");

    begin(&vault, 5, "source-cursor");
    assert_eq!(
        baseline_apply_state(&vault)
            .expect("state")
            .expect("staging")
            .status,
        BaselineApplyStatus::Staging
    );
    apply_baseline_batch(&vault, &[("doc-1".to_owned(), snap)], &[]).expect("apply baseline");

    let (merged, head) = load_body_doc(&vault, "doc-1", PEER_A, None).expect("merged doc");
    assert!(
        merged.text().contains("local"),
        "本地编辑丢失：{}",
        merged.text()
    );
    assert!(
        merged.text().contains("remote"),
        "基线内容丢失：{}",
        merged.text()
    );
    assert!(head.head_seq >= 2, "基线 update 应追加在本地 update 之后");

    let batch_ids = vault
        .query(
            "SELECT batch_id FROM loro_updates WHERE doc_id='doc-1' ORDER BY seq",
            &[],
        )
        .expect("batch ids");
    assert!(batch_ids.iter().any(|row| matches!(
        row.first(),
        Some(Value::Text(value)) if value == "baseline_merge"
    )));
    assert!(batch_ids
        .iter()
        .any(|row| matches!(row.first(), Some(Value::Text(value)) if value == "local-batch")));

    let outcome = finish_baseline_apply(&vault, 5, "chosen-cursor").expect("finish");
    let BaselineApplyFinish::Applied(state) = outcome else {
        panic!("finish 未 Applied：{outcome:?}");
    };
    assert_eq!(state.status, BaselineApplyStatus::Completed);
    assert!(!state.finishable);
    let cursor = sync_cursor_state(&vault).expect("cursor");
    assert_eq!(cursor.received_seq, 5);
    assert_eq!(cursor.integrated_seq, 5);
    assert_eq!(cursor.confirmed_highwater, Some(5));
    assert_eq!(
        vault
            .get_meta(META_SYNC_RELAY_CURSOR)
            .expect("relay cursor")
            .as_deref(),
        Some(b"chosen-cursor".as_slice())
    );

    // 完成后的 journal 不能再应用批次。
    assert!(apply_baseline_batch(&vault, &[], &[]).is_err());
    let _ = local;
}

#[test]
fn tombstone_keeps_local_edit_and_deletes_clean_doc() {
    let (_dir, vault) = fresh_vault();
    local_doc(&vault, "doc-local", "local");
    let clean = seed_note(&vault, "note-clean", "doc-clean");
    clean.insert_scalars(0, "").expect("空正文");
    let post = seed_note(&vault, "note-post", "doc-post");
    post.insert_scalars(0, "").expect("空正文");

    begin(&vault, 3, "src");
    apply_baseline_batch(
        &vault,
        &[
            (
                "doc-local".to_owned(),
                snapshot("doc-local", PEER_B, "remote"),
            ),
            (
                "doc-clean".to_owned(),
                snapshot("doc-clean", PEER_B, "baseline"),
            ),
            (
                "doc-post".to_owned(),
                snapshot("doc-post", PEER_B, "baseline"),
            ),
        ],
        &["doc-local".to_owned(), "doc-clean".to_owned()],
    )
    .expect("apply with tombstones");

    // baseline 应用后再产生本地编辑：tombstone 后续到达仍须保留。
    let (post_doc, _) = load_body_doc(&vault, "doc-post", PEER_A, None).expect("post doc");
    post_doc
        .insert_scalars(0, "post-local")
        .expect("post local edit");
    commit_now(&vault, &post_doc, "post-local-batch");
    apply_baseline_batch(&vault, &[], &["doc-post".to_owned()]).expect("post tombstone");

    assert_eq!(notes_deleted(&vault, "doc-local"), 0, "有本地编辑必须保留");
    assert_eq!(notes_deleted(&vault, "doc-clean"), 1, "无本地内容应删除");
    assert_eq!(
        notes_deleted(&vault, "doc-post"),
        0,
        "基线后的本地编辑必须保留"
    );
    let state = baseline_apply_state(&vault)
        .expect("state")
        .expect("staging");
    assert_eq!(state.kept_local_edit, 2);
    assert_eq!(state.applied_docs, 3);
}

#[test]
fn batch_is_resumable_after_reopen_and_idempotent() {
    let (dir, vault) = fresh_vault();
    local_doc(&vault, "doc-a", "a-local");
    local_doc(&vault, "doc-b", "b-local");
    let snap_a = snapshot("doc-a", PEER_B, "a-remote");
    let snap_b = snapshot("doc-b", PEER_B, "b-remote");

    begin(&vault, 9, "src");
    apply_baseline_batch(&vault, &[("doc-a".to_owned(), snap_a.clone())], &[]).expect("batch a");
    let cursor = sync_cursor_state(&vault).expect("cursor before finish");
    assert_eq!(cursor.received_seq, 0);
    assert_eq!(cursor.integrated_seq, 0);
    assert_eq!(cursor.confirmed_highwater, None);
    drop(vault);

    let vault = open_vault(&vault_path(&dir));
    let state = baseline_apply_state(&vault)
        .expect("state")
        .expect("staging");
    assert_eq!(state.applied_docs, 1);
    assert_eq!(state.status, BaselineApplyStatus::Staging);

    // 重放同一批：内容/计数不变。
    apply_baseline_batch(&vault, &[("doc-a".to_owned(), snap_a)], &[]).expect("idempotent a");
    assert_eq!(
        baseline_apply_state(&vault)
            .expect("state")
            .expect("staging")
            .applied_docs,
        1
    );

    apply_baseline_batch(&vault, &[("doc-b".to_owned(), snap_b)], &[]).expect("batch b");
    assert_eq!(
        finish_baseline_apply(&vault, 9, "new-src").expect("finish"),
        BaselineApplyFinish::Applied(
            baseline_apply_state(&vault)
                .expect("state")
                .expect("completed")
        )
    );
    let cursor = sync_cursor_state(&vault).expect("cursor");
    assert_eq!(cursor.received_seq, 9);
    assert_eq!(cursor.integrated_seq, 9);
    assert_eq!(cursor.confirmed_highwater, Some(9));
}

#[test]
fn finish_cursor_cas_conflict_does_not_advance() {
    let (_dir, vault) = fresh_vault();
    local_doc(&vault, "doc-1", "local");
    begin(&vault, 4, "src");
    apply_baseline_batch(
        &vault,
        &[("doc-1".to_owned(), snapshot("doc-1", PEER_B, "remote"))],
        &[],
    )
    .expect("apply");

    // 模拟其它写入在 finish 前推进了本地 cursor。
    vault
        .set_meta(META_SYNC_RECEIVED_SEQ, &1u64.to_be_bytes())
        .expect("推进 received");

    assert_eq!(
        finish_baseline_apply(&vault, 4, "chosen").expect("finish"),
        BaselineApplyFinish::Conflict
    );
    let cursor = sync_cursor_state(&vault).expect("cursor");
    assert_eq!(cursor.received_seq, 1, "冲突时不得推进");
    assert_eq!(cursor.integrated_seq, 0, "冲突时不得推进");
    assert_eq!(cursor.confirmed_highwater, None, "冲突时不得推进");
    assert_eq!(
        baseline_apply_state(&vault)
            .expect("state")
            .expect("still staging")
            .status,
        BaselineApplyStatus::Staging
    );
}

#[test]
fn finish_keeps_highwater_above_low_baseline() {
    let (_dir, vault) = fresh_vault();
    // 已有库的 highwater 远高于所选基线 S：finish 只允许前进。
    seed_watermarks(&vault, 0, 0, Some(30), None);
    begin(&vault, 5, "src");

    let outcome = finish_baseline_apply(&vault, 5, "baseline-cursor").expect("finish");
    assert!(
        matches!(outcome, BaselineApplyFinish::Applied(_)),
        "低 S 是单调推进而不是冲突：{outcome:?}"
    );
    let cursor = sync_cursor_state(&vault).expect("cursor");
    assert_eq!(
        cursor.confirmed_highwater,
        Some(30),
        "highwater 不得被低 S 降级"
    );
    assert_eq!(cursor.received_seq, 5, "低于 S 的水位仍推进到 S");
    assert_eq!(cursor.integrated_seq, 5);
    assert_eq!(
        relay_cursor(&vault).as_deref(),
        Some(b"baseline-cursor".as_slice()),
        "S 覆盖已 integrate 前缀时应切换为基线游标"
    );
}

#[test]
fn finish_low_baseline_keeps_integrated_relay_cursor_and_pins() {
    let (_dir, vault) = fresh_vault();
    seed_watermarks(&vault, 20, 12, Some(12), Some("old-cursor"));
    // 外部 pin（sync 模块维护）：storage 的 finish 不读不写，更不得降低。
    // 键名与 crates/sync/src/bootstrap.rs 的 META_BOOTSTRAP_* 对齐。
    vault.set_meta("sync.log_id", &[9; 16]).expect("set log id");
    vault
        .set_meta("sync.pinned_instance", &[7; 16])
        .expect("set instance pin");
    vault
        .set_meta("sync.pinned_epoch", &3u64.to_be_bytes())
        .expect("set epoch pin");
    vault
        .set_meta("sync.pinned_control_rev", &5u64.to_be_bytes())
        .expect("set control rev pin");
    vault
        .set_meta("sync.pinned_control_hash", &[1; 32])
        .expect("set control hash pin");
    begin(&vault, 5, "src");

    let outcome = finish_baseline_apply(&vault, 5, "baseline-cursor").expect("finish");
    assert!(matches!(outcome, BaselineApplyFinish::Applied(_)));
    let cursor = sync_cursor_state(&vault).expect("cursor");
    assert_eq!(cursor.received_seq, 20, "低 S 不得降级 received");
    assert_eq!(cursor.integrated_seq, 12, "低 S 不得降级 integrated");
    assert_eq!(
        cursor.confirmed_highwater,
        Some(12),
        "低 S 不得降级 highwater"
    );
    assert_eq!(
        relay_cursor(&vault).as_deref(),
        Some(b"old-cursor".as_slice()),
        "低 S 不得回退 relay 游标"
    );
    for (key, expected) in [
        ("sync.log_id", [9u8; 16].to_vec()),
        ("sync.pinned_instance", [7u8; 16].to_vec()),
        ("sync.pinned_epoch", 3u64.to_be_bytes().to_vec()),
        ("sync.pinned_control_rev", 5u64.to_be_bytes().to_vec()),
        ("sync.pinned_control_hash", [1u8; 32].to_vec()),
    ] {
        assert_eq!(
            vault.get_meta(key).expect("read pin"),
            Some(expected.clone()),
            "pin {key} 不得被 finish 改写/降低"
        );
    }
}

#[test]
fn finish_takes_per_field_max_and_advances_cursor_between_watermarks() {
    let (_dir, vault) = fresh_vault();
    // received > S > integrated：received/highwater 保留，integrated 推进，
    // S 已覆盖已消费前缀，游标可以切换到基线 cursor。
    seed_watermarks(&vault, 8, 3, Some(10), Some("old-cursor"));
    begin(&vault, 5, "src");

    assert!(matches!(
        finish_baseline_apply(&vault, 5, "baseline-cursor").expect("finish"),
        BaselineApplyFinish::Applied(_)
    ));
    let cursor = sync_cursor_state(&vault).expect("cursor");
    assert_eq!(cursor.received_seq, 8, "received 取 max(8, 5)");
    assert_eq!(cursor.integrated_seq, 5, "integrated 取 max(3, 5)");
    assert_eq!(
        cursor.confirmed_highwater,
        Some(10),
        "highwater 取 max(10, 5)"
    );
    assert_eq!(
        relay_cursor(&vault).as_deref(),
        Some(b"baseline-cursor".as_slice()),
        "S >= 现有 integrated 时游标前进"
    );
}

#[test]
fn finish_relay_cursor_cas_conflict_keeps_every_field() {
    let (_dir, vault) = fresh_vault();
    seed_watermarks(&vault, 2, 2, Some(2), Some("old-cursor"));
    begin(&vault, 5, "src");

    // 模拟其它写入在 finish 前改写了 relay 游标。
    vault
        .set_meta(META_SYNC_RELAY_CURSOR, b"raced-cursor")
        .expect("并发改写游标");

    assert_eq!(
        finish_baseline_apply(&vault, 5, "baseline-cursor").expect("finish"),
        BaselineApplyFinish::Conflict
    );
    let cursor = sync_cursor_state(&vault).expect("cursor");
    assert_eq!(cursor.received_seq, 2, "冲突时不得推进任何字段");
    assert_eq!(cursor.integrated_seq, 2);
    assert_eq!(cursor.confirmed_highwater, Some(2));
    assert_eq!(
        relay_cursor(&vault).as_deref(),
        Some(b"raced-cursor".as_slice()),
        "冲突时不得覆盖并发写入的游标"
    );
    assert_eq!(
        baseline_apply_state(&vault)
            .expect("state")
            .expect("still staging")
            .status,
        BaselineApplyStatus::Staging,
        "冲突时 journal 仍应可续跑"
    );
}

#[test]
fn batch_failure_rolls_back_every_doc_and_journal() {
    let (_dir, vault) = fresh_vault();
    local_doc(&vault, "doc-a", "a-local");
    local_doc(&vault, "doc-b", "b-local");
    begin(&vault, 6, "src");

    vault
        .exec_batch(
            "CREATE TRIGGER fail_baseline_batch BEFORE INSERT ON loro_updates \
             WHEN NEW.doc_id='doc-b' BEGIN SELECT RAISE(ABORT,'injected'); END;",
        )
        .expect("注入触发器");
    let error = apply_baseline_batch(
        &vault,
        &[
            ("doc-a".to_owned(), snapshot("doc-a", PEER_B, "a-remote")),
            ("doc-b".to_owned(), snapshot("doc-b", PEER_B, "b-remote")),
        ],
        &[],
    );
    assert!(error.is_err(), "事务中途失败必须返回错误");
    let state = baseline_apply_state(&vault)
        .expect("state")
        .expect("staging");
    assert_eq!(state.applied_docs, 0, "失败批次不能留下 journal 进度");
    let (doc_a, head_a) = load_body_doc(&vault, "doc-a", PEER_A, None).expect("doc a");
    assert_eq!(doc_a.text(), "a-local");
    assert_eq!(head_a.head_seq, 1);
    assert_eq!(
        vault
            .query(
                "SELECT count(*) FROM loro_updates WHERE doc_id='doc-a'",
                &[]
            )
            .expect("updates")[0][0],
        Value::Integer(1)
    );

    vault
        .exec_batch("DROP TRIGGER fail_baseline_batch")
        .expect("撤销触发器");
    apply_baseline_batch(
        &vault,
        &[
            ("doc-a".to_owned(), snapshot("doc-a", PEER_B, "a-remote")),
            ("doc-b".to_owned(), snapshot("doc-b", PEER_B, "b-remote")),
        ],
        &[],
    )
    .expect("重试批次");
    assert_eq!(
        baseline_apply_state(&vault)
            .expect("state")
            .expect("staging")
            .applied_docs,
        2
    );
}

#[test]
fn new_doc_from_baseline_is_created_and_readable() {
    let (_dir, vault) = fresh_vault();
    begin(&vault, 1, "src");
    apply_baseline_batch(
        &vault,
        &[(
            "doc-new".to_owned(),
            snapshot("doc-new", PEER_B, "from-baseline"),
        )],
        &[],
    )
    .expect("new doc");
    assert!(
        vault
            .query(
                "SELECT count(*) FROM loro_docs WHERE doc_id='doc-new' AND kind='body'",
                &[]
            )
            .expect("doc row")[0][0]
            == Value::Integer(1)
    );
    let (doc, head) = load_body_doc(&vault, "doc-new", PEER_A, None).expect("load new");
    assert_eq!(doc.text(), "from-baseline");
    assert_eq!(head.head_seq, 1);
    assert_eq!(
        vault
            .query(
                "SELECT batch_id FROM loro_updates WHERE doc_id='doc-new' AND seq=1",
                &[]
            )
            .expect("batch")[0][0],
        Value::Text("baseline_merge".to_owned())
    );
}

#[test]
fn assets_are_idempotent_and_abort_keeps_imported_doc() {
    let (_dir, vault) = fresh_vault();
    local_doc(&vault, "doc-1", "local");
    begin(&vault, 2, "src");
    apply_baseline_batch(
        &vault,
        &[("doc-1".to_owned(), snapshot("doc-1", PEER_B, "remote"))],
        &[],
    )
    .expect("apply doc");

    let asset = BaselineAsset {
        object_id: [4; 32],
        asset_hash: [9; 32],
        byte_len: 5,
        media_type: Some("text/plain".to_owned()),
        data: b"asset".to_vec(),
    };
    let first = apply_baseline_assets(&vault, std::slice::from_ref(&asset)).expect("asset 1");
    assert_eq!(first.applied_assets, 1);
    let second = apply_baseline_assets(&vault, std::slice::from_ref(&asset)).expect("asset 2");
    assert_eq!(second.applied_assets, 1, "同 HMAC oid 重放不应重复计数");
    assert_eq!(
        vault
            .query(
                "SELECT count(*) FROM assets WHERE asset_hash=?1",
                &[Value::Blob(vec![9; 32])]
            )
            .expect("asset count")[0][0],
        Value::Integer(1)
    );

    assert!(abort_baseline_apply(&vault).expect("abort"));
    assert!(baseline_apply_state(&vault).expect("state").is_none());
    let (doc, _) = load_body_doc(&vault, "doc-1", PEER_A, None).expect("imported doc survives");
    assert!(doc.text().contains("remote"));
    assert!(doc.text().contains("local"));
    // abort 不清已导入资产。
    assert_eq!(
        vault
            .query(
                "SELECT count(*) FROM assets WHERE asset_hash=?1",
                &[Value::Blob(vec![9; 32])]
            )
            .expect("asset count after abort")[0][0],
        Value::Integer(1)
    );
}

#[test]
fn schema_v3_has_baseline_journal_tables() {
    let (_dir, vault) = fresh_vault();
    assert_eq!(vault.user_version().expect("version"), SCHEMA_VERSION);
    for table in [
        "baseline_apply",
        "baseline_apply_docs",
        "baseline_apply_tombstones",
        "baseline_apply_assets",
    ] {
        let rows = vault
            .query(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1",
                &[Value::Text(table.to_owned())],
            )
            .expect("table query");
        assert_eq!(rows[0][0], Value::Integer(1), "缺少表 {table}");
    }
}
