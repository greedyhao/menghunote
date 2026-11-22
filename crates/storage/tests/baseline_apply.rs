//! S7-34：已验证基线分批 merge、tombstone 保守删除、cursor CAS 与崩溃续跑。

mod support;

use std::time::Duration;

use loro::VersionVector;
use support::*;

use menghunote_storage::session::{
    ChangeSource, DocumentSession, EditOutcome, EditRequest, FlushReason, LiveRev, ScalarEdit,
    SessionEpoch, SessionOptions,
};
use menghunote_storage::{
    abort_baseline_apply, append_updates_with_integrated_marks, apply_baseline_assets,
    apply_baseline_batch, baseline_apply_debug_counters, baseline_apply_state,
    baseline_verify_failure, begin_baseline_apply, create_note_slot, finish_baseline_apply,
    load_body_doc, load_head, load_structure, mark_baseline_verify_failed,
    reset_baseline_apply_debug, set_sync_log_id, store_inbox_verified_batch, sync_cursor_state,
    BaselineApplyFinish, BaselineApplyStatus, BaselineAsset, BaselineExpectation, BodyDoc,
    StorageError, StructureDoc, StructureWriter, SyncContext, UpdateRecord, Value, Vault,
    VerifiedInboxRow, BASELINE_MERGE_BATCH, META_BASELINE_VERIFY_FAILED,
    META_SYNC_CONFIRMED_HIGHWATER, META_SYNC_INTEGRATED_SEQ, META_SYNC_RECEIVED_SEQ,
    META_SYNC_RELAY_CURSOR, SCHEMA_VERSION, STRUCTURE_DOC_ID,
};
use menghunote_text::UnicodeScalar;

/// 测试用同步上下文：peer 故意不用常量 1（既有默认值），epoch = 1。
const SYNC_CTX: SyncContext = SyncContext {
    peer_id: 0xA1CE_0001,
    key_epoch: 1,
};

fn session_options(sync: Option<SyncContext>) -> SessionOptions {
    SessionOptions {
        peer_id: SYNC_CTX.peer_id,
        session_epoch: SessionEpoch(9),
        sync,
        ..SessionOptions::default()
    }
}

fn scalar_edit(start: usize, end: usize, replacement: &str) -> ScalarEdit {
    ScalarEdit {
        start: UnicodeScalar::new(start),
        end: UnicodeScalar::new(end),
        replacement: replacement.to_owned(),
    }
}

fn edit_request(
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

/// 生成一条远端 update（模拟普通 pull/integrate 收到的数据）。
fn remote_update(doc_id: &str, peer: u64, text: &str) -> Vec<u8> {
    let doc = BodyDoc::create(doc_id, peer).expect("远端副本");
    doc.insert_scalars(0, text).expect("远端编辑");
    doc.export_all_updates().expect("导出 update")
}

/// 把一个已有快照装进本地文档并提交，使本地 VV == 快照 VV。
fn seed_doc_from_snapshot(vault: &Vault, note_id: &str, doc_id: &str, snapshot: &[u8]) -> BodyDoc {
    let _ = seed_note(vault, note_id, doc_id);
    let doc = BodyDoc::restore(doc_id, PEER_A, Some(snapshot), &[]).expect("本地副本");
    commit_now(vault, &doc, "local-copy");
    doc
}

fn meta_remote_known_count(vault: &Vault, doc_id: &str) -> i64 {
    count(
        vault,
        "SELECT count(*) FROM meta WHERE key=?1",
        &[Value::Text(format!("sync.remote_known_vv.{doc_id}"))],
    )
}

/// 读某篇文档的远端已知 VV；没有记录返回 `None`。
fn meta_remote_known(vault: &Vault, doc_id: &str) -> Option<VersionVector> {
    meta_remote_known_bytes(vault, doc_id).map(|bytes| VersionVector::decode(&bytes).expect("解码"))
}

/// 读某篇文档的远端已知 VV 原始字节（用于断言「不降级」）。
fn meta_remote_known_bytes(vault: &Vault, doc_id: &str) -> Option<Vec<u8>> {
    let rows = vault
        .query(
            "SELECT value FROM meta WHERE key=?1",
            &[Value::Text(format!("sync.remote_known_vv.{doc_id}"))],
        )
        .expect("meta 查询");
    match rows.first().and_then(|row| row.first()) {
        Some(Value::Blob(bytes)) => Some(bytes.clone()),
        None => None,
        other => panic!("remote_known 值异常：{other:?}"),
    }
}

/// 注入「读旧值之后、CAS 之前 meta 被并发改写」：在 baseline_apply_docs 首次写入时
/// 把该文档的远端已知 VV 改成别的字节（同一事务内可见，CAS 必然不命中）。
fn install_remote_known_clobber_trigger(vault: &Vault, doc_id: &str) {
    let sql = format!(
        "CREATE TRIGGER remote_known_cas_race BEFORE INSERT ON baseline_apply_docs \
         BEGIN \
           INSERT OR REPLACE INTO meta(key,value) \
           VALUES('sync.remote_known_vv.{doc_id}', X'DEADBEEF'); \
         END;"
    );
    vault.exec(&sql, &[]).expect("安装竞态触发器");
}

fn snapshot(doc_id: &str, peer: u64, text: &str) -> Vec<u8> {
    let doc = BodyDoc::create(doc_id, peer).expect("创建基线 LoroDoc");
    doc.insert_scalars(0, text).expect("写基线正文");
    doc.export_snapshot().expect("导出基线 snapshot")
}

/// 预期参与集合：只声明文档（本文件里大多数用例没有资产）。
fn expectation(docs: &[&str]) -> BaselineExpectation {
    BaselineExpectation::new(
        docs.iter().map(|doc_id| (*doc_id).to_owned()),
        std::iter::empty::<[u8; 32]>(),
    )
}

/// 预期参与集合：文档 + 资产。
fn expectation_with_assets(docs: &[&str], assets: &[[u8; 32]]) -> BaselineExpectation {
    BaselineExpectation::new(
        docs.iter().map(|doc_id| (*doc_id).to_owned()),
        assets.iter().copied(),
    )
}

/// begin 一份**空预期集合**的 staging（只关心 cursor/journal 语义的用例）。
fn begin(vault: &Vault, s: u64, cursor: &str) -> menghunote_storage::BaselineApplyState {
    begin_with(vault, s, cursor, &BaselineExpectation::default())
}

/// begin 一份显式声明预期参与集合的 staging。
fn begin_with(
    vault: &Vault,
    s: u64,
    cursor: &str,
    expected: &BaselineExpectation,
) -> menghunote_storage::BaselineApplyState {
    begin_baseline_apply(vault, &[7; 16], s, cursor, expected).expect("begin baseline")
}

/// 读某篇文档的 tombstone action（当前 journal）。
fn tombstone_action(vault: &Vault, doc_id: &str) -> Option<String> {
    let rows = vault
        .query(
            "SELECT action FROM baseline_apply_tombstones WHERE doc_id=?1",
            &[Value::Text(doc_id.to_owned())],
        )
        .expect("tombstone 查询");
    match rows.first().and_then(|row| row.first()) {
        Some(Value::Text(value)) => Some(value.clone()),
        Some(other) => panic!("tombstone action 异常：{other:?}"),
        None => None,
    }
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

    begin_with(&vault, 5, "source-cursor", &expectation(&["doc-1"]));
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

    begin_with(&vault, 9, "src", &expectation(&["doc-a", "doc-b"]));
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
    begin_with(&vault, 4, "src", &expectation(&["doc-1"]));
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

// ---------------------------------------------------------------------------
// 解码/重放去重：snapshot 只解码一次，不再二次 import 大 snapshot
// ---------------------------------------------------------------------------

#[test]
fn new_doc_decodes_snapshot_once_without_legacy_create_import() {
    let (_dir, vault) = fresh_vault();
    begin(&vault, 1, "src");
    reset_baseline_apply_debug();
    apply_baseline_batch(
        &vault,
        &[(
            "doc-new".to_owned(),
            snapshot("doc-new", PEER_B, "from-baseline"),
        )],
        &[],
    )
    .expect("new doc");

    let counters = baseline_apply_debug_counters();
    assert_eq!(counters.snapshot_imports, 1, "snapshot 只允许解码/导入一次");
    assert_eq!(counters.new_docs, 1);
    assert_eq!(counters.load_body_doc_calls, 0, "新文档不需要重放本地副本");
    assert_eq!(
        counters.snapshot_reimport_bytes, 0,
        "不得二次 import 大 snapshot"
    );
    assert_eq!(
        counters.new_doc_import_bytes, 0,
        "新文档不得走 create+import 旧路径"
    );
    assert_eq!(counters.pending_update_calls, 1);
    assert!(counters.pending_update_bytes > 0);

    let (doc, head) = load_body_doc(&vault, "doc-new", PEER_A, None).expect("load new");
    assert_eq!(doc.text(), "from-baseline");
    assert_eq!(head.head_seq, 1, "新文档仍只落一条 baseline_merge update");
}

#[test]
fn existing_doc_fully_covered_by_baseline_writes_no_update() {
    let (_dir, vault) = fresh_vault();
    let snap = snapshot("doc-1", PEER_B, "shared content");
    seed_doc_from_snapshot(&vault, "note-1", "doc-1", &snap);
    let before_head = load_head(&vault, "doc-1").expect("head before");
    let before_updates = count(
        &vault,
        "SELECT count(*) FROM loro_updates WHERE doc_id='doc-1'",
        &[],
    );

    begin(&vault, 2, "src");
    reset_baseline_apply_debug();
    apply_baseline_batch(&vault, &[("doc-1".to_owned(), snap)], &[]).expect("apply");

    let counters = baseline_apply_debug_counters();
    assert_eq!(counters.existing_no_extra, 1);
    assert_eq!(
        counters.pending_update_calls, 0,
        "VV 完全相同时不应导出/落盘任何 update"
    );
    assert_eq!(
        counters.load_body_doc_calls, 0,
        "只判 local_extra 时不得重放正文"
    );
    assert_eq!(counters.snapshot_reimport_bytes, 0);

    let after_head = load_head(&vault, "doc-1").expect("head after");
    assert_eq!(after_head.head_seq, before_head.head_seq, "head 不得被推进");
    assert_eq!(
        count(
            &vault,
            "SELECT count(*) FROM loro_updates WHERE doc_id='doc-1'",
            &[]
        ),
        before_updates,
        "不得追加空 update"
    );
    assert_eq!(
        meta_remote_known_count(&vault, "doc-1"),
        1,
        "即使无新 op，也要记录远端已知 VV"
    );
}

#[test]
fn existing_doc_with_baseline_superset_exports_only_missing_ops() {
    let (_dir, vault) = fresh_vault();
    // 远端两段提交：本地只有第一段，baseline 是完整两段。
    let remote = BodyDoc::create("doc-1", PEER_B).expect("remote");
    remote.insert_scalars(0, "first").expect("first");
    let first_snapshot = remote.export_snapshot().expect("first snapshot");
    remote.insert_scalars(5, "-second").expect("second");
    let full_snapshot = remote.export_snapshot().expect("full snapshot");
    seed_doc_from_snapshot(&vault, "note-1", "doc-1", &first_snapshot);

    begin(&vault, 3, "src");
    reset_baseline_apply_debug();
    let full_len = full_snapshot.len() as u64;
    apply_baseline_batch(&vault, &[("doc-1".to_owned(), full_snapshot)], &[]).expect("apply");

    let counters = baseline_apply_debug_counters();
    assert_eq!(counters.existing_no_extra, 1, "本地没有 baseline 之外的 op");
    assert_eq!(counters.snapshot_imports, 1);
    assert_eq!(counters.load_body_doc_calls, 0);
    assert_eq!(counters.snapshot_reimport_bytes, 0);
    assert_eq!(counters.pending_update_calls, 1);
    assert!(
        counters.pending_update_bytes > 0 && counters.pending_update_bytes < full_len,
        "只应导出缺失的那一段（{} >= {}）",
        counters.pending_update_bytes,
        full_len
    );

    let (doc, head) = load_body_doc(&vault, "doc-1", PEER_A, None).expect("merged");
    assert_eq!(doc.text(), "first-second");
    assert_eq!(head.head_seq, 2, "本地一条 + baseline 增量一条");
}

#[test]
fn existing_doc_with_local_extra_imports_only_local_delta() {
    let (_dir, vault) = fresh_vault();
    local_doc(&vault, "doc-1", "local");
    let snap = snapshot(
        "doc-1",
        PEER_B,
        "remote baseline text that is much larger than the local delta",
    );
    let snapshot_len = snap.len() as u64;

    begin(&vault, 4, "src");
    reset_baseline_apply_debug();
    apply_baseline_batch(&vault, &[("doc-1".to_owned(), snap)], &[]).expect("apply");

    let counters = baseline_apply_debug_counters();
    assert_eq!(counters.existing_local_extra, 1);
    assert_eq!(counters.snapshot_imports, 1);
    assert_eq!(
        counters.snapshot_reimport_bytes, 0,
        "不得二次 import 大 snapshot"
    );
    assert_eq!(counters.new_doc_import_bytes, 0);
    assert_eq!(counters.load_body_doc_calls, 1, "只允许一次本地重放");
    assert!(counters.local_delta_bytes > 0, "本地增量不能为空");
    assert!(
        counters.local_delta_bytes < snapshot_len / 4,
        "反向导入的必须只是本地小增量（{} vs snapshot {}）",
        counters.local_delta_bytes,
        snapshot_len
    );
    assert_eq!(counters.pending_update_calls, 1);
    assert!(counters.pending_update_bytes > 0);

    let (doc, _head) = load_body_doc(&vault, "doc-1", PEER_A, None).expect("merged");
    assert!(doc.text().contains("local"));
    assert!(doc.text().contains("remote baseline text"));
}

#[test]
fn existing_doc_with_local_op_depending_on_baseline_keeps_merged_frontier() {
    let (_dir, vault) = fresh_vault();
    // 远端：base 之后又追加了 -r2（本地还没收到）。
    let remote = BodyDoc::create("doc-1", PEER_B).expect("remote");
    remote.insert_scalars(0, "base").expect("baseline edit");
    let first_snapshot = remote.export_snapshot().expect("first snapshot");
    remote.insert_scalars(4, "-r2").expect("remote second edit");
    let full_snapshot = remote.export_snapshot().expect("full snapshot");
    // 本地：先收到 base，再在 base 之上编辑（本地 op 因果依赖基线 op）。
    seed_doc_from_snapshot(&vault, "note-1", "doc-1", &first_snapshot);
    let (local, _) = load_body_doc(&vault, "doc-1", PEER_A, None).expect("local");
    local
        .insert_scalars(4, "-local")
        .expect("local edit on baseline");
    commit_now(&vault, &local, "local-on-baseline");

    begin(&vault, 5, "src");
    reset_baseline_apply_debug();
    apply_baseline_batch(&vault, &[("doc-1".to_owned(), full_snapshot)], &[]).expect("apply");

    let counters = baseline_apply_debug_counters();
    assert_eq!(counters.existing_local_extra, 1);
    assert_eq!(counters.load_body_doc_calls, 1);
    assert!(
        counters.local_delta_bytes > 0,
        "本地依赖基线的新 op 必须作为差集导出"
    );
    assert_eq!(counters.snapshot_reimport_bytes, 0);

    // 关键：本地 op 与远端 -r2 并发；head_frontier 必须等于重放后的 state frontiers，
    // 否则 load_body_doc 直接报 Corrupt（detached 合并的 oplog frontier 必须正确）。
    let (merged, head) = load_body_doc(&vault, "doc-1", PEER_A, None).expect("merged replay");
    assert!(merged.text().contains("base"));
    assert!(merged.text().contains("-local"));
    assert!(merged.text().contains("-r2"));
    assert_eq!(head.head_seq, 3, "base + local + baseline 差集");
}

#[test]
fn replay_same_digest_skips_snapshot_decode() {
    let (_dir, vault) = fresh_vault();
    local_doc(&vault, "doc-1", "local");
    let snap = snapshot("doc-1", PEER_B, "remote");
    begin(&vault, 6, "src");
    apply_baseline_batch(&vault, &[("doc-1".to_owned(), snap.clone())], &[]).expect("first apply");

    reset_baseline_apply_debug();
    apply_baseline_batch(&vault, &[("doc-1".to_owned(), snap)], &[]).expect("replay");
    let counters = baseline_apply_debug_counters();
    assert_eq!(counters.replayed_same_digest, 1);
    assert_eq!(counters.snapshot_imports, 0, "重放不得再解码 snapshot");
    assert_eq!(counters.snapshot_bytes, 0);
    assert_eq!(counters.load_body_doc_calls, 0, "重放只允许 load_head");
    assert_eq!(counters.pending_update_calls, 0);
    assert_eq!(
        baseline_apply_state(&vault)
            .expect("state")
            .expect("staging")
            .applied_docs,
        1
    );
}

// ---------------------------------------------------------------------------
// 已有库 tombstone 判定：cross-journal 远端已知 VV
// ---------------------------------------------------------------------------

#[test]
fn tombstone_deletes_previous_baseline_doc_after_begin_clears_staging_tables() {
    let (_dir, vault) = fresh_vault();
    begin_with(&vault, 10, "src1", &expectation(&["doc-x"]));
    apply_baseline_batch(
        &vault,
        &[("doc-x".to_owned(), snapshot("doc-x", PEER_B, "remote body"))],
        &[],
    )
    .expect("first baseline");
    assert!(matches!(
        finish_baseline_apply(&vault, 10, "c10").expect("finish"),
        BaselineApplyFinish::Applied(_)
    ));
    create_note_slot(&vault, "note-x", "doc-x", "远端笔记", 1).expect("note row");

    // 模拟旧版本数据：只有 journal 行、没有跨 journal 的 meta。
    vault
        .exec(
            "DELETE FROM meta WHERE key=?1",
            &[Value::Text("sync.remote_known_vv.doc-x".to_owned())],
        )
        .expect("删 meta");
    assert_eq!(
        count(
            &vault,
            "SELECT count(*) FROM baseline_apply_docs WHERE doc_id='doc-x'",
            &[]
        ),
        1
    );

    // 下一次快照追赶：begin 会清空 staging 子表，但必须先非破坏性地回填远端已知 VV。
    begin(&vault, 20, "src2");
    assert_eq!(
        count(&vault, "SELECT count(*) FROM baseline_apply_docs", &[]),
        0,
        "begin 仍清空本轮的 staging 子表"
    );
    assert_eq!(
        meta_remote_known_count(&vault, "doc-x"),
        1,
        "旧 baseline VV 必须回填到 meta（非破坏性）"
    );

    let state = apply_baseline_batch(&vault, &[], &["doc-x".to_owned()]).expect("tombstone");
    assert_eq!(meta_remote_known_count(&vault, "doc-x"), 1);
    assert_eq!(
        notes_deleted(&vault, "doc-x"),
        1,
        "远端已删 + 本地无额外 op 必须 deleted"
    );
    assert_eq!(state.kept_local_edit, 0);
}

#[test]
fn tombstone_deletes_doc_pulled_through_normal_sync() {
    let (_dir, vault) = fresh_vault();
    seed_note(&vault, "note-pull", "doc-pull");
    let mut session =
        DocumentSession::open(&vault, "doc-pull", session_options(Some(SYNC_CTX))).expect("会话");
    let delta = session
        .apply_remote(&remote_update("doc-pull", PEER_B, "pulled body"))
        .expect("导入远端")
        .expect("应当产生 LiveDelta");
    assert_eq!(delta.source, ChangeSource::Remote);
    session.flush(FlushReason::Manual);
    session
        .wait_for_durable(LiveRev(1), Duration::from_secs(5))
        .expect("远端导入必须 durable");
    drop(session);

    assert_eq!(
        count(&vault, "SELECT count(*) FROM sync_outbox", &[]),
        0,
        "普通 pull 不得留下 outbox"
    );
    assert_eq!(
        meta_remote_known_count(&vault, "doc-pull"),
        1,
        "normal pull/integrate 必须记录远端已知 VV"
    );

    begin(&vault, 7, "src");
    let state = apply_baseline_batch(&vault, &[], &["doc-pull".to_owned()]).expect("tombstone");
    assert_eq!(
        notes_deleted(&vault, "doc-pull"),
        1,
        "head_seq>0、outbox 为空、内容全来自远端的文档必须 deleted"
    );
    assert_eq!(state.kept_local_edit, 0);
}

#[test]
fn tombstone_keeps_local_only_doc() {
    let (_dir, vault) = fresh_vault();
    local_doc(&vault, "doc-local", "local only");
    assert_eq!(
        meta_remote_known_count(&vault, "doc-local"),
        0,
        "local-only 文档不应有远端已知 VV"
    );

    begin(&vault, 8, "src");
    let state = apply_baseline_batch(&vault, &[], &["doc-local".to_owned()]).expect("tombstone");
    assert_eq!(
        notes_deleted(&vault, "doc-local"),
        0,
        "local-only 文档绝不能被 tombstone 误删"
    );
    assert_eq!(state.kept_local_edit, 1);
}

#[test]
fn tombstone_keeps_doc_with_unsynced_outbox() {
    let (_dir, vault) = fresh_vault();
    seed_note(&vault, "note-sync", "doc-sync");
    let mut session =
        DocumentSession::open(&vault, "doc-sync", session_options(Some(SYNC_CTX))).expect("会话");
    let outcome = session.apply(edit_request(
        &session,
        "c1",
        0,
        vec![scalar_edit(0, 0, "unsynced local")],
    ));
    assert!(
        matches!(outcome, EditOutcome::Applied(_)),
        "本地编辑必须被受理：{outcome:?}"
    );
    session.flush(FlushReason::Manual);
    session
        .wait_for_durable(LiveRev(1), Duration::from_secs(5))
        .expect("本地编辑 durable");
    drop(session);
    assert_eq!(
        count(&vault, "SELECT count(*) FROM sync_outbox", &[]),
        1,
        "未上传的本地编辑必须留下 outbox"
    );

    begin(&vault, 9, "src");
    let state = apply_baseline_batch(&vault, &[], &["doc-sync".to_owned()]).expect("tombstone");
    assert_eq!(
        notes_deleted(&vault, "doc-sync"),
        0,
        "有未同步 outbox 必须保留"
    );
    assert_eq!(state.kept_local_edit, 1);
}

#[test]
fn tombstone_deletes_doc_integrated_through_engine_path() {
    // 模拟后台 engine integrate：远端 update 先合进内存副本，再走
    // `append_updates_with_integrated_marks` 组合落盘（普通 pull 的主路径）。
    let (_dir, vault) = fresh_vault();
    let log_id = [0x3a; 16];
    set_sync_log_id(&vault, &log_id).expect("写入 sync.log_id");
    let inbox = VerifiedInboxRow {
        log_id,
        relay_seq: 1,
        object_id: [0x11; 32],
        epoch: 1,
        verified_payload: b"remote-update".to_vec(),
    };
    assert_eq!(
        store_inbox_verified_batch(&vault, std::slice::from_ref(&inbox)).expect("inbox"),
        1
    );

    let doc = seed_note(&vault, "note-pull2", "doc-pull2");
    let current = load_head(&vault, "doc-pull2").expect("head");
    doc.import(&remote_update("doc-pull2", PEER_B, "engine pulled"))
        .expect("import remote");
    let combined = doc
        .pending_update(&current.version_vector().expect("vv"))
        .expect("delta");
    append_updates_with_integrated_marks(
        &vault,
        "doc-pull2",
        current.head_seq,
        &UpdateRecord::new("remote-batch", combined),
        &log_id,
        &[1],
    )
    .expect("engine integrate");

    assert_eq!(
        meta_remote_known_count(&vault, "doc-pull2"),
        1,
        "engine integrate 必须记录远端已知 VV"
    );
    assert_eq!(count(&vault, "SELECT count(*) FROM sync_outbox", &[]), 0);

    begin(&vault, 11, "src");
    let state = apply_baseline_batch(&vault, &[], &["doc-pull2".to_owned()]).expect("tombstone");
    assert_eq!(
        notes_deleted(&vault, "doc-pull2"),
        1,
        "后台 pull 集成来的文档在远端删除后必须 deleted"
    );
    assert_eq!(state.kept_local_edit, 0);
}

// ---------------------------------------------------------------------------
// P1-1：finish 完整性（begin 记录预期参与集合，finish 前逐项证明）
// ---------------------------------------------------------------------------

/// 少跑一批就 finish：必须 Conflict 且一行都不写（旧实现会 Applied 并推进 S，
/// pull 之后永久跳过这批 relay 事件）。
#[test]
fn finish_without_all_docs_is_conflict() {
    let (_dir, vault) = fresh_vault();
    local_doc(&vault, "doc-a", "a-local");
    local_doc(&vault, "doc-b", "b-local");
    begin_with(&vault, 5, "src", &expectation(&["doc-a", "doc-b"]));

    let state = apply_baseline_batch(
        &vault,
        &[("doc-a".to_owned(), snapshot("doc-a", PEER_B, "a-remote"))],
        &[],
    )
    .expect("batch a");
    assert_eq!(state.applied_docs, 1);
    assert_eq!(state.expected_docs, Some(2));
    assert!(
        !state.finishable,
        "预期集合未齐（doc-b 缺失）时不得声称可 finish"
    );

    assert_eq!(
        finish_baseline_apply(&vault, 5, "baseline-cursor").expect("finish"),
        BaselineApplyFinish::Conflict,
        "预期集合未齐时 finish 必须 Conflict"
    );

    // 冲突时一行都不写：水位/meta/游标/journal 全部保持 begin 前状态。
    let cursor = sync_cursor_state(&vault).expect("cursor");
    assert_eq!(cursor.received_seq, 0, "冲突时不得推进 received");
    assert_eq!(cursor.integrated_seq, 0, "冲突时不得推进 integrated");
    assert_eq!(cursor.confirmed_highwater, None, "冲突时不得写 highwater");
    assert_eq!(relay_cursor(&vault), None, "冲突时不得写 relay cursor");
    let journal = baseline_apply_state(&vault)
        .expect("state")
        .expect("journal");
    assert_eq!(journal.status, BaselineApplyStatus::Staging);
    assert_eq!(journal.chosen_cursor, None);
    assert_eq!(journal.applied_docs, 1, "冲突不得回滚已提交批次");

    // 补齐缺失批次后必须能正常 finish（Conflict 不是永久卡死）。
    let state = apply_baseline_batch(
        &vault,
        &[("doc-b".to_owned(), snapshot("doc-b", PEER_B, "b-remote"))],
        &[],
    )
    .expect("batch b");
    assert!(state.finishable, "两篇都到齐后必须可 finish");
    let BaselineApplyFinish::Applied(state) =
        finish_baseline_apply(&vault, 5, "baseline-cursor").expect("finish")
    else {
        panic!("补齐后 finish 必须 Applied");
    };
    assert_eq!(state.status, BaselineApplyStatus::Completed);
    assert_eq!(state.applied_docs, 2);
    let cursor = sync_cursor_state(&vault).expect("cursor");
    assert_eq!(cursor.received_seq, 5);
    assert_eq!(cursor.integrated_seq, 5);
    assert_eq!(cursor.confirmed_highwater, Some(5));
}

/// 全部分批到齐：finish 正常 Applied。
#[test]
fn finish_after_full_batches_is_applied() {
    let (_dir, vault) = fresh_vault();
    local_doc(&vault, "doc-a", "a-local");
    local_doc(&vault, "doc-b", "b-local");
    begin_with(&vault, 9, "src", &expectation(&["doc-a", "doc-b"]));

    let first = apply_baseline_batch(
        &vault,
        &[("doc-a".to_owned(), snapshot("doc-a", PEER_B, "a-remote"))],
        &[],
    )
    .expect("batch a");
    assert_eq!(first.expected_docs, Some(2));
    assert!(!first.finishable);

    let second = apply_baseline_batch(
        &vault,
        &[("doc-b".to_owned(), snapshot("doc-b", PEER_B, "b-remote"))],
        &[],
    )
    .expect("batch b");
    assert!(second.finishable);

    let BaselineApplyFinish::Applied(state) =
        finish_baseline_apply(&vault, 9, "baseline-cursor").expect("finish")
    else {
        panic!("完整批次 finish 必须 Applied");
    };
    assert_eq!(state.status, BaselineApplyStatus::Completed);
    assert_eq!(state.applied_docs, 2);
    assert_eq!(state.expected_docs, Some(2));
    assert_eq!(state.expected_assets, Some(0));
    assert!(!state.finishable, "completed 后不再是 finishable");
    let cursor = sync_cursor_state(&vault).expect("cursor");
    assert_eq!(cursor.received_seq, 9);
    assert_eq!(cursor.integrated_seq, 9);
    assert_eq!(cursor.confirmed_highwater, Some(9));
    assert_eq!(
        relay_cursor(&vault).as_deref(),
        Some(b"baseline-cursor".as_slice())
    );
}

/// begin 同参数重入幂等（含顺序无关的同一集合）；同 checkpoint 换集合必须冲突，
/// 且不得清掉已应用进度或预期集合。
#[test]
fn begin_idempotent_replay_keeps_expected_set() {
    let (_dir, vault) = fresh_vault();
    local_doc(&vault, "doc-a", "a-local");
    local_doc(&vault, "doc-b", "b-local");
    begin_with(&vault, 5, "src", &expectation(&["doc-a", "doc-b"]));
    apply_baseline_batch(
        &vault,
        &[("doc-a".to_owned(), snapshot("doc-a", PEER_B, "a-remote"))],
        &[],
    )
    .expect("batch a");

    // 同参数 + 同一集合（顺序不同）：幂等重入，保留进度与集合。
    let replayed = begin_with(&vault, 5, "src", &expectation(&["doc-b", "doc-a"]));
    assert_eq!(replayed.applied_docs, 1, "重入不得清空已应用批次");
    assert_eq!(replayed.expected_docs, Some(2), "重入必须保留预期集合");
    assert_eq!(replayed.source_cursor, "src");
    assert!(!replayed.finishable);

    // 同 checkpoint/S/cursor 但预期集合不同：必须冲突（不能静默沿用新集合）。
    let error = begin_baseline_apply(&vault, &[7; 16], 5, "src", &expectation(&["doc-a"]))
        .expect_err("同一 checkpoint 换预期集合必须冲突");
    assert!(
        matches!(error, StorageError::SyncConflict { .. }),
        "应当是 SyncConflict，实际 {error:?}"
    );
    let state = baseline_apply_state(&vault)
        .expect("state")
        .expect("journal");
    assert_eq!(state.applied_docs, 1, "冲突不得清空已应用批次");
    assert_eq!(state.expected_docs, Some(2), "冲突不得改写预期集合");

    // 补齐后仍能 finish。
    apply_baseline_batch(
        &vault,
        &[("doc-b".to_owned(), snapshot("doc-b", PEER_B, "b-remote"))],
        &[],
    )
    .expect("batch b");
    assert!(matches!(
        finish_baseline_apply(&vault, 5, "baseline-cursor").expect("finish"),
        BaselineApplyFinish::Applied(_)
    ));
}

/// 资产少一批同样必须 Conflict（bootstrap 的资产也只能在全部到齐后 finish）。
#[test]
fn finish_without_all_assets_is_conflict() {
    let (_dir, vault) = fresh_vault();
    local_doc(&vault, "doc-1", "local");
    let asset_a = BaselineAsset {
        object_id: [1; 32],
        asset_hash: [11; 32],
        byte_len: 3,
        media_type: Some("text/plain".to_owned()),
        data: b"aaa".to_vec(),
    };
    let asset_b = BaselineAsset {
        object_id: [2; 32],
        asset_hash: [12; 32],
        byte_len: 4,
        media_type: Some("text/plain".to_owned()),
        data: b"bbbb".to_vec(),
    };
    begin_with(
        &vault,
        4,
        "src",
        &expectation_with_assets(&["doc-1"], &[[1; 32], [2; 32]]),
    );
    apply_baseline_batch(
        &vault,
        &[("doc-1".to_owned(), snapshot("doc-1", PEER_B, "remote"))],
        &[],
    )
    .expect("doc batch");
    let state = apply_baseline_assets(&vault, std::slice::from_ref(&asset_a)).expect("asset a");
    assert_eq!(state.applied_assets, 1);
    assert_eq!(state.expected_assets, Some(2));
    assert!(!state.finishable, "资产少一批时不得声称可 finish");

    assert_eq!(
        finish_baseline_apply(&vault, 4, "baseline-cursor").expect("finish"),
        BaselineApplyFinish::Conflict,
        "预期资产未齐时 finish 必须 Conflict"
    );
    // 一行都不写：水位/游标/journal 保持 staging。
    let cursor = sync_cursor_state(&vault).expect("cursor");
    assert_eq!(cursor.received_seq, 0);
    assert_eq!(cursor.integrated_seq, 0);
    assert_eq!(cursor.confirmed_highwater, None);
    assert_eq!(relay_cursor(&vault), None);
    let journal = baseline_apply_state(&vault)
        .expect("state")
        .expect("journal");
    assert_eq!(journal.status, BaselineApplyStatus::Staging);
    assert_eq!(journal.chosen_cursor, None);
    assert_eq!(journal.applied_assets, 1, "冲突不得回滚已应用资产");

    let state = apply_baseline_assets(&vault, std::slice::from_ref(&asset_b)).expect("asset b");
    assert!(state.finishable);
    assert!(matches!(
        finish_baseline_apply(&vault, 4, "baseline-cursor").expect("finish"),
        BaselineApplyFinish::Applied(_)
    ));
}

// ---------------------------------------------------------------------------
// P1：verify 失败标记的 storage 门禁（apply / finish / finishable）
// ---------------------------------------------------------------------------

/// 失败标记存在时：
/// - `apply_baseline_batch` 在任何解码/写事务前拒绝，且零写；
/// - `finish_baseline_apply` 返回**带原因**的 `SyncConflict`，不是丢掉原因的 `Ok(Conflict)`；
/// - `baseline_apply_state.finishable` 在预期集合已全齐时也必须为 false（status 仍是 staging）；
/// - `abort_baseline_apply` 清标记后 `begin` 可重新成功。
#[test]
fn marker_blocks_apply_and_finish_and_finishable() {
    let (_dir, vault) = fresh_vault();
    local_doc(&vault, "doc-a", "a-local");
    begin_with(&vault, 5, "src", &expectation(&["doc-a"]));

    // 先真正跑完唯一一批，让预期集合在标记之前**已全齐**（证明 finishable 的变化来自标记）。
    let applied = apply_baseline_batch(
        &vault,
        &[("doc-a".to_owned(), snapshot("doc-a", PEER_B, "a-remote"))],
        &[],
    )
    .expect("batch a");
    assert!(applied.finishable, "标记写入前预期集合已齐，应可 finish");

    let head_before = head(&vault, "doc-a");
    let cursor_before = sync_cursor_state(&vault).expect("cursor");
    let remote_known_before = meta_remote_known_bytes(&vault, "doc-a");
    let updates_before = count(&vault, "SELECT count(*) FROM loro_updates", &[]);
    let journal_rows_before = count(&vault, "SELECT count(*) FROM baseline_apply_docs", &[]);

    const REASON: &str = "读回校验失败：snapshot 的 VV 未被本地 VV 覆盖";
    mark_baseline_verify_failed(&vault, REASON).expect("写失败标记");
    assert_eq!(
        baseline_verify_failure(&vault).expect("读标记").as_deref(),
        Some(REASON)
    );

    // 预期集合仍全齐（applied=1/expected=1），但 finishable 必须因标记为 false。
    let state = baseline_apply_state(&vault)
        .expect("state")
        .expect("journal 必须保留");
    assert_eq!(state.status, BaselineApplyStatus::Staging);
    assert_eq!(state.expected_docs, Some(1));
    assert_eq!(state.applied_docs, 1);
    assert!(
        !state.finishable,
        "失败标记存在时即使 expected 已全齐也不得 finishable"
    );

    // 资产批也必须被同一 marker 挡住（M1）：失败 journal 语义上已冻结，不能借
    // apply_baseline_assets 继续写 baseline_apply_assets/assets 计数。
    let assets_before = count(&vault, "SELECT count(*) FROM assets", &[]);
    let apply_assets_before = count(&vault, "SELECT count(*) FROM baseline_apply_assets", &[]);
    let asset = BaselineAsset {
        object_id: [0x7A; 32],
        asset_hash: [0x8B; 32],
        byte_len: 5,
        media_type: Some("text/plain".to_owned()),
        data: b"asset".to_vec(),
    };
    let asset_error = apply_baseline_assets(&vault, std::slice::from_ref(&asset))
        .expect_err("失败标记必须阻止 apply_baseline_assets");
    assert!(
        matches!(&asset_error, StorageError::SyncConflict { .. }),
        "应当是 SyncConflict，实际 {asset_error:?}"
    );
    let asset_message = format!("{asset_error}");
    assert!(
        asset_message.contains(REASON),
        "资产冲突必须携带原因：{asset_message}"
    );
    assert_eq!(
        count(&vault, "SELECT count(*) FROM assets", &[]),
        assets_before,
        "marker 下资产批必须零写"
    );
    assert_eq!(
        count(&vault, "SELECT count(*) FROM baseline_apply_assets", &[]),
        apply_assets_before,
        "marker 下不得推进 baseline_apply_assets"
    );

    // apply：用一篇新文档 doc-b 证明门禁在解码/写事务之前生效（若漏检会落 loro_docs/update）。
    let error = apply_baseline_batch(
        &vault,
        &[("doc-b".to_owned(), snapshot("doc-b", PEER_B, "b-remote"))],
        &[],
    )
    .expect_err("失败标记必须阻止 apply_baseline_batch");
    assert!(
        matches!(&error, StorageError::SyncConflict { .. }),
        "应当是 SyncConflict，实际 {error:?}"
    );
    let message = format!("{error}");
    assert!(message.contains(REASON), "冲突必须携带失败原因：{message}");
    assert!(
        message.contains("abort_baseline_apply"),
        "冲突必须提示先 abort_baseline_apply：{message}"
    );

    // 零写：doc-b 没有任何行；doc-a 的 head/远端已知 VV 与全库 update 数不变；
    // journal 的已应用集合与预期集合也不变。
    assert_eq!(
        count(
            &vault,
            "SELECT count(*) FROM loro_docs WHERE doc_id='doc-b'",
            &[]
        ),
        0
    );
    assert_eq!(
        count(
            &vault,
            "SELECT count(*) FROM loro_updates WHERE doc_id='doc-b'",
            &[]
        ),
        0
    );
    assert_eq!(
        count(&vault, "SELECT count(*) FROM loro_updates", &[]),
        updates_before
    );
    assert_eq!(
        count(&vault, "SELECT count(*) FROM baseline_apply_docs", &[]),
        journal_rows_before
    );
    assert_eq!(head(&vault, "doc-a").head_seq, head_before.head_seq);
    assert_eq!(
        meta_remote_known_bytes(&vault, "doc-a"),
        remote_known_before
    );
    let state = baseline_apply_state(&vault)
        .expect("state")
        .expect("journal");
    assert_eq!(state.status, BaselineApplyStatus::Staging);
    assert_eq!(state.applied_docs, 1);
    assert_eq!(state.expected_docs, Some(1));

    // finish：必须是**带原因**的 SyncConflict，不能退化成 Ok(Conflict) 把原因丢掉。
    let error = finish_baseline_apply(&vault, 5, "baseline-cursor")
        .expect_err("失败标记必须阻止 finish_baseline_apply");
    assert!(
        matches!(&error, StorageError::SyncConflict { .. }),
        "应当是 SyncConflict，实际 {error:?}"
    );
    let message = format!("{error}");
    assert!(
        message.contains(REASON),
        "finish 冲突必须携带失败原因：{message}"
    );
    assert!(
        message.contains("abort_baseline_apply"),
        "finish 冲突必须提示先 abort_baseline_apply：{message}"
    );

    // 零写：水位/cursor/journal 全部保持 begin/上次 apply 后的状态。
    let cursor_after = sync_cursor_state(&vault).expect("cursor");
    assert_eq!(cursor_after.received_seq, cursor_before.received_seq);
    assert_eq!(cursor_after.integrated_seq, cursor_before.integrated_seq);
    assert_eq!(
        cursor_after.confirmed_highwater,
        cursor_before.confirmed_highwater
    );
    assert_eq!(relay_cursor(&vault), None);
    let state = baseline_apply_state(&vault)
        .expect("state")
        .expect("journal");
    assert_eq!(state.status, BaselineApplyStatus::Staging);
    assert_eq!(state.chosen_cursor, None);
    assert_eq!(state.applied_docs, 1);

    // abort 清 journal + 标记，已 merge 的正文保留；之后 begin 可重新成功。
    assert!(abort_baseline_apply(&vault).expect("abort"));
    assert!(
        baseline_verify_failure(&vault).expect("读标记").is_none(),
        "abort 必须清除失败标记"
    );
    assert!(baseline_apply_state(&vault).expect("state").is_none());
    let merged = load_body_doc(&vault, "doc-a", PEER_A, None)
        .expect("abort 后正文")
        .0;
    assert!(
        merged.text().contains("a-local"),
        "abort 只清 journal，不得删已 merge 的正文，实际={:?}",
        merged.text()
    );

    let reopened = begin_with(&vault, 5, "src", &expectation(&["doc-a"]));
    assert_eq!(reopened.status, BaselineApplyStatus::Staging);
    assert_eq!(reopened.applied_docs, 0, "abort 后 begin 应是全新 journal");
    assert_eq!(reopened.expected_docs, Some(1));
    assert!(
        !reopened.finishable,
        "新建 staging 尚未应用任何批次，不应 finishable"
    );
}

// ---------------------------------------------------------------------------
// P1-2：tombstone 判定与 notes 软删之间的 TOCTOU
// ---------------------------------------------------------------------------

/// 判定（事务外看到无 outbox）之后、事务执行期间插进一条 outbox：事务内重判必须把
/// action 降级为 kept_local_edit，`notes.deleted` 保持 0。
#[test]
fn finish_without_journal_rejects_residual_marker_before_not_found() {
    let (_dir, vault) = fresh_vault();
    mark_baseline_verify_failed(&vault, "无 journal 的残留 marker").expect("marker");

    let error = finish_baseline_apply(&vault, 5, "c5")
        .expect_err("无 journal 但有 marker 时 finish 必须先拒绝 marker");
    assert!(
        matches!(&error, StorageError::SyncConflict { .. }),
        "应当是带原因的 SyncConflict，实际 {error:?}"
    );
    let message = format!("{error}");
    assert!(message.contains("无 journal 的残留 marker"));
    assert!(message.contains("abort_baseline_apply"));

    // abort 是唯一清理入口；清理后 begin 可重新开始。
    assert!(!abort_baseline_apply(&vault).expect("abort"));
    assert_eq!(baseline_verify_failure(&vault).expect("read"), None);
}

#[test]
fn abort_without_journal_clears_residual_marker() {
    let (_dir, vault) = fresh_vault();
    mark_baseline_verify_failed(&vault, "残留 marker").expect("marker");
    assert_eq!(
        baseline_verify_failure(&vault).expect("read").as_deref(),
        Some("残留 marker")
    );

    // 没有 staging journal 也必须清掉残留 marker：abort 是唯一清理入口。
    assert!(!abort_baseline_apply(&vault).expect("abort"));
    assert_eq!(baseline_verify_failure(&vault).expect("read"), None);
    assert!(baseline_apply_state(&vault).expect("state").is_none());
}

#[test]
fn abort_guard_failure_keeps_marker_for_retry() {
    let (_dir, vault) = fresh_vault();
    local_doc(&vault, "doc-a", "a-local");
    begin_with(&vault, 5, "src", &expectation(&["doc-a"]));
    mark_baseline_verify_failed(&vault, "保持到重试").expect("marker");

    // 模拟 Phase1B 并发化后才会出现的 journal 消失窗口：让第 4 条 DELETE
    //（baseline_apply staging 行）影响 0 行，guarded 事务整体回滚。
    vault
        .exec(
            "CREATE TRIGGER skip_baseline_apply_abort BEFORE DELETE ON baseline_apply \
             WHEN OLD.singleton = 1 BEGIN SELECT RAISE(IGNORE); END",
            &[],
        )
        .expect("trigger");
    assert!(
        !abort_baseline_apply(&vault).expect("guard failure -> false"),
        "guard 失败必须返回 false"
    );
    assert_eq!(
        baseline_verify_failure(&vault)
            .expect("marker survives")
            .as_deref(),
        Some("保持到重试"),
        "guarded 事务回滚时 marker 不能被半清"
    );
    assert_eq!(
        baseline_apply_state(&vault)
            .expect("state")
            .expect("journal survives")
            .status,
        BaselineApplyStatus::Staging
    );

    vault
        .exec("DROP TRIGGER skip_baseline_apply_abort", &[])
        .expect("drop trigger");
    assert!(abort_baseline_apply(&vault).expect("retry abort"));
    assert_eq!(baseline_verify_failure(&vault).expect("marker"), None);
}

// ---------------------------------------------------------------------------
// P1-3：损坏的失败标记（非法 BLOB）必须 fail-closed；abort 成功必须真的清掉它
// ---------------------------------------------------------------------------

/// 直接写入非法 BLOB（`mark_baseline_verify_failed` 永远产生不了这种编码）：
/// - `baseline_verify_failure` / `baseline_apply_state` → `StorageError::Corrupt`
///   （不能静默当成"没有失败"，否则 begin 会放行未知状态的 journal）；
/// - `begin` / `apply` / `finish` 全部拒绝（Corrupt，不是 SyncConflict），且零写；
/// - `abort_baseline_apply` 是失败标记的唯一清理入口：返回成功必须**真的**删掉损坏值，
///   不能"看起来成功但坏值还在"；没有 journal 时返回 false（不是成功）也必须清掉，
///   否则损坏值会永久阻断 begin。
#[test]
fn corrupt_marker_is_fail_closed_and_abort_does_not_silently_succeed() {
    let (_dir, vault) = fresh_vault();
    local_doc(&vault, "doc-a", "a-local");
    begin_with(&vault, 5, "src", &expectation(&["doc-a"]));
    let updates_before = count(&vault, "SELECT count(*) FROM loro_updates", &[]);

    // 非法编码：缺魔数。直接写 meta，绕过 mark_baseline_verify_failed。
    vault
        .set_meta(META_BASELINE_VERIFY_FAILED, b"bad")
        .expect("写损坏 marker");

    // 读：必须 Corrupt（缺魔数），不能退化成 None。
    let error = baseline_verify_failure(&vault).expect_err("损坏 marker 必须 Corrupt");
    assert!(
        matches!(&error, StorageError::Corrupt { .. }),
        "应当是 Corrupt，实际 {error:?}"
    );
    let message = format!("{error}");
    assert!(
        message.contains(META_BASELINE_VERIFY_FAILED),
        "错误必须点明损坏的 meta 键：{message}"
    );
    assert!(message.contains("魔数"), "错误必须说明缺魔数：{message}");

    // begin / apply / finish 三处入口全部拒绝；损坏标记不能被降级成 SyncConflict/None。
    let begin_error = begin_baseline_apply(
        &vault,
        &[9; 16],
        6,
        "other-cursor",
        &expectation(&["doc-a"]),
    )
    .expect_err("损坏 marker 必须阻止 begin（即使换了基线参数）");
    assert!(
        matches!(&begin_error, StorageError::Corrupt { .. }),
        "begin 应当是 Corrupt，实际 {begin_error:?}"
    );
    let apply_error = apply_baseline_batch(
        &vault,
        &[("doc-a".to_owned(), snapshot("doc-a", PEER_B, "a-remote"))],
        &[],
    )
    .expect_err("损坏 marker 必须阻止 apply_baseline_batch");
    assert!(
        matches!(&apply_error, StorageError::Corrupt { .. }),
        "apply 应当是 Corrupt，实际 {apply_error:?}"
    );
    let finish_error = finish_baseline_apply(&vault, 5, "baseline-cursor")
        .expect_err("损坏 marker 必须阻止 finish_baseline_apply");
    assert!(
        matches!(&finish_error, StorageError::Corrupt { .. }),
        "finish 应当是 Corrupt，实际 {finish_error:?}"
    );

    // state 读取也必须 Corrupt（不能靠猜算 finishable），journal 与数据保持零写。
    let state_error = baseline_apply_state(&vault).expect_err("损坏 marker 下 state 必须 Corrupt");
    assert!(
        matches!(&state_error, StorageError::Corrupt { .. }),
        "state 应当是 Corrupt，实际 {state_error:?}"
    );
    assert_eq!(
        count(
            &vault,
            "SELECT count(*) FROM baseline_apply WHERE singleton=1 AND status='staging'",
            &[],
        ),
        1,
        "被拒绝的调用不得清掉 staging journal"
    );
    assert_eq!(
        count(&vault, "SELECT count(*) FROM baseline_apply_docs", &[]),
        0,
        "apply 被拒时必须零写"
    );
    assert_eq!(
        count(&vault, "SELECT count(*) FROM loro_updates", &[]),
        updates_before,
        "begin/apply/finish 被拒时不得追加 Loro update"
    );

    // abort（有 staging journal）：返回 true，并且成功必须真的删掉损坏 marker。
    assert!(
        abort_baseline_apply(&vault).expect("abort 必须成功"),
        "有 staging journal 时 abort 必须返回 true"
    );
    assert_eq!(
        baseline_verify_failure(&vault).expect("abort 后必须能读"),
        None,
        "abort 返回成功却留下损坏 marker = 静默失败"
    );
    assert!(
        baseline_apply_state(&vault).expect("state").is_none(),
        "abort 必须清掉 staging journal"
    );
    assert_eq!(
        count(&vault, "SELECT count(*) FROM loro_updates", &[]),
        updates_before,
        "abort 只清 journal，不得删已 merge 的数据"
    );

    // 没有 staging journal 时 abort 返回 false（不是"成功"），但同样必须清掉损坏值，
    // 否则该值会永久阻断后续 begin。
    vault
        .set_meta(META_BASELINE_VERIFY_FAILED, b"")
        .expect("写空 marker（缺魔数）");
    assert!(
        matches!(
            baseline_verify_failure(&vault).expect_err("空 marker 也必须 Corrupt"),
            StorageError::Corrupt { .. }
        ),
        "空 BLOB 同样缺魔数，必须 Corrupt"
    );
    assert!(
        !abort_baseline_apply(&vault).expect("没有 journal 时 abort 返回 false"),
        "没有 staging journal 时必须返回 false（调用方不得把 false 当成功）"
    );
    assert_eq!(
        baseline_verify_failure(&vault).expect("abort 后必须能读"),
        None,
        "false 只表示没有 journal 可中止，损坏 marker 仍必须被清掉"
    );

    // abort 清掉损坏值后，begin 恢复可用（损坏 marker 不会永久 brick 库）。
    let reopened = begin_with(&vault, 5, "src", &expectation(&["doc-a"]));
    assert_eq!(reopened.status, BaselineApplyStatus::Staging);
    assert_eq!(reopened.applied_docs, 0);
}

#[test]
fn tombstone_outbox_race_between_decision_and_transaction_keeps_note() {
    let (_dir, vault) = fresh_vault();
    // doc-a：本批要 merge 的基线文档；触发器挂在它的 journal 写入上。
    local_doc(&vault, "doc-a", "a-local");

    // doc-pull：普通 pull 集成来的文档（远端已知、无 outbox），判定本来会删除它。
    seed_note(&vault, "note-pull", "doc-pull");
    let mut session =
        DocumentSession::open(&vault, "doc-pull", session_options(Some(SYNC_CTX))).expect("会话");
    let delta = session
        .apply_remote(&remote_update("doc-pull", PEER_B, "pulled body"))
        .expect("导入远端")
        .expect("应当产生 LiveDelta");
    assert_eq!(delta.source, ChangeSource::Remote);
    session.flush(FlushReason::Manual);
    session
        .wait_for_durable(LiveRev(1), Duration::from_secs(5))
        .expect("远端导入必须 durable");
    drop(session);
    assert_eq!(count(&vault, "SELECT count(*) FROM sync_outbox", &[]), 0);
    assert_eq!(meta_remote_known_count(&vault, "doc-pull"), 1);

    // 注入竞态：docs 批的 journal 写入（先于同批 tombstone 语句）触发一条 outbox 行，
    // 模拟"判定完成后本地编辑的 A 事务刚好提交"。
    vault
        .exec(
            "CREATE TRIGGER baseline_race_outbox BEFORE INSERT ON baseline_apply_docs \
             WHEN NEW.doc_id='doc-a' BEGIN \
               INSERT INTO sync_outbox(message_id,batch_id,doc_id,seq,key_epoch,ciphertext) \
               VALUES('race-outbox','race','doc-pull',1,1,NULL); \
             END;",
            &[],
        )
        .expect("注入竞态触发器");

    begin_with(&vault, 7, "src", &expectation(&["doc-a"]));
    let state = apply_baseline_batch(
        &vault,
        &[("doc-a".to_owned(), snapshot("doc-a", PEER_B, "a-remote"))],
        &["doc-pull".to_owned()],
    )
    .expect("apply with raced tombstone");

    assert_eq!(
        count(
            &vault,
            "SELECT count(*) FROM sync_outbox WHERE doc_id='doc-pull'",
            &[],
        ),
        1,
        "触发器必须真的插入了 outbox（证明竞态窗口被覆盖）"
    );
    assert_eq!(
        notes_deleted(&vault, "doc-pull"),
        0,
        "判定后被并发插入 outbox 的笔记绝不能软删"
    );
    assert_eq!(
        tombstone_action(&vault, "doc-pull").as_deref(),
        Some("kept_local_edit"),
        "事务内重判必须把 action 降级为 kept_local_edit"
    );
    assert_eq!(state.kept_local_edit, 1);
}

// ---------------------------------------------------------------------------
// P2-5：远端已知 VV 的单调性（值 CAS）与 begin 回填覆盖面
// ---------------------------------------------------------------------------

/// completed journal（finish 后 status='completed'、子表保留）在 begin 替换时
/// 必须先回填 baseline_vv；这是旧库升级后 tombstone 不误判的关键证据。
#[test]
fn begin_backfills_remote_known_for_completed_journal() {
    let (_dir, vault) = fresh_vault();
    begin_with(&vault, 10, "src1", &expectation(&["doc-completed"]));
    apply_baseline_batch(
        &vault,
        &[(
            "doc-completed".to_owned(),
            snapshot("doc-completed", PEER_B, "remote body"),
        )],
        &[],
    )
    .expect("first baseline");
    assert!(matches!(
        finish_baseline_apply(&vault, 10, "c10").expect("finish"),
        BaselineApplyFinish::Applied(_)
    ));
    let baseline_vv = meta_remote_known(&vault, "doc-completed").expect("已有远端已知 VV");

    // 模拟旧版本数据：没有跨 journal 的 meta，只剩 completed journal + 子表。
    vault
        .exec(
            "DELETE FROM meta WHERE key=?1",
            &[Value::Text("sync.remote_known_vv.doc-completed".to_owned())],
        )
        .expect("删 meta");
    assert_eq!(
        count(
            &vault,
            "SELECT count(*) FROM baseline_apply_docs WHERE doc_id='doc-completed'",
            &[]
        ),
        1
    );

    begin(&vault, 20, "src2");

    assert_eq!(
        meta_remote_known(&vault, "doc-completed"),
        Some(baseline_vv),
        "completed journal 的 baseline_vv 必须回填到 meta"
    );
    assert_eq!(
        count(&vault, "SELECT count(*) FROM baseline_apply_docs", &[]),
        0,
        "旧 journal 子表必须清掉"
    );
    let state = baseline_apply_state(&vault)
        .expect("state")
        .expect("journal");
    assert_eq!(state.status, BaselineApplyStatus::Staging);
    assert_eq!(state.baseline_s, 20);
}

/// 没有 journal 行、但残留 `baseline_apply_docs`（旧版本/异常写的孤儿行）时，
/// begin 也必须先把 `baseline_vv` 回填到 meta，再清掉残留子表。
#[test]
fn begin_backfills_remote_known_without_journal_but_with_residual_docs() {
    let (_dir, vault) = fresh_vault();
    begin_with(&vault, 10, "src1", &expectation(&["doc-residual"]));
    apply_baseline_batch(
        &vault,
        &[(
            "doc-residual".to_owned(),
            snapshot("doc-residual", PEER_B, "remote body"),
        )],
        &[],
    )
    .expect("first baseline");
    assert!(matches!(
        finish_baseline_apply(&vault, 10, "c10").expect("finish"),
        BaselineApplyFinish::Applied(_)
    ));

    // 模拟旧版本/异常写：journal 行丢失，但子表仍在；meta 也缺失。
    vault
        .exec("DELETE FROM baseline_apply", &[])
        .expect("删 journal");
    vault
        .exec(
            "DELETE FROM meta WHERE key=?1",
            &[Value::Text("sync.remote_known_vv.doc-residual".to_owned())],
        )
        .expect("删 meta");
    assert_eq!(
        count(
            &vault,
            "SELECT count(*) FROM baseline_apply_docs WHERE applied=1",
            &[]
        ),
        1,
        "前置：残留一行已应用的 baseline_apply_docs"
    );

    begin(&vault, 20, "src2");

    assert_eq!(
        meta_remote_known_count(&vault, "doc-residual"),
        1,
        "没有 journal 的残留 baseline_vv 也必须回填（否则证据会被 DELETE 静默丢弃）"
    );
    assert_eq!(
        meta_remote_known(&vault, "doc-residual"),
        Some(
            VersionVector::decode(
                &meta_remote_known_bytes(&vault, "doc-residual").expect("回填值")
            )
            .expect("解码")
        ),
        "回填的是旧 journal 的 baseline VV"
    );
    assert_eq!(
        count(&vault, "SELECT count(*) FROM baseline_apply_docs", &[]),
        0,
        "残留子表必须被清掉"
    );
    let state = baseline_apply_state(&vault)
        .expect("state")
        .expect("journal");
    assert_eq!(state.status, BaselineApplyStatus::Staging);
    assert_eq!(state.baseline_s, 20);
}

/// begin 的回填是单调的：已有 meta（普通 pull/integrate 记录的更新值）绝不被
/// 旧 journal 的 baseline_vv 降级（`DO NOTHING`）。
#[test]
fn begin_backfill_never_downgrades_existing_remote_known() {
    let (_dir, vault) = fresh_vault();
    begin_with(&vault, 10, "src1", &expectation(&["doc-keep"]));
    apply_baseline_batch(
        &vault,
        &[(
            "doc-keep".to_owned(),
            snapshot("doc-keep", PEER_B, "baseline body"),
        )],
        &[],
    )
    .expect("first baseline");
    assert!(matches!(
        finish_baseline_apply(&vault, 10, "c10").expect("finish"),
        BaselineApplyFinish::Applied(_)
    ));

    // 已有值比旧 journal 的 baseline VV 更大（模拟后续普通 pull 扩大了它）。
    let mut superset = meta_remote_known(&vault, "doc-keep").expect("baseline 后已有值");
    superset.insert(0xA1, 9);
    vault
        .exec(
            "INSERT INTO meta(key,value) VALUES(?1,?2) \
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            &[
                Value::Text("sync.remote_known_vv.doc-keep".to_owned()),
                Value::Blob(superset.encode()),
            ],
        )
        .expect("写入更大的旧值");
    let before = meta_remote_known_bytes(&vault, "doc-keep").expect("旧值");

    vault
        .exec("DELETE FROM baseline_apply", &[])
        .expect("模拟无 journal 的残留子表");
    begin(&vault, 20, "src2");

    assert_eq!(
        meta_remote_known_bytes(&vault, "doc-keep"),
        Some(before),
        "begin 回填绝不能把已有（更大的）远端已知 VV 降级"
    );
}

/// 新文档（读旧值时不存在）的 CAS：事务内被并发写入抢占 → 明确 SyncConflict，
/// 整批回滚，不覆盖任何值。
#[test]
fn baseline_remote_known_cas_fails_closed_for_new_doc() {
    let (_dir, vault) = fresh_vault();
    begin_with(&vault, 5, "src", &expectation(&["doc-cas-new"]));
    install_remote_known_clobber_trigger(&vault, "doc-cas-new");

    let error = apply_baseline_batch(
        &vault,
        &[(
            "doc-cas-new".to_owned(),
            snapshot("doc-cas-new", PEER_B, "remote"),
        )],
        &[],
    )
    .expect_err("CAS 必须失败");

    match &error {
        StorageError::SyncConflict { context } => {
            assert!(
                context.contains("远端已知 VV"),
                "错误必须点明远端已知 VV CAS：{context}"
            );
        }
        other => panic!("期望 SyncConflict，实际 {other:?}"),
    }
    assert_eq!(
        meta_remote_known_bytes(&vault, "doc-cas-new"),
        None,
        "触发器写入的 meta 必须随整批回滚"
    );
    assert_eq!(
        count(
            &vault,
            "SELECT count(*) FROM loro_docs WHERE doc_id='doc-cas-new'",
            &[]
        ),
        0,
        "整批回滚：文档行不得落库"
    );
    assert_eq!(
        count(
            &vault,
            "SELECT count(*) FROM loro_updates WHERE doc_id='doc-cas-new'",
            &[]
        ),
        0
    );
}

/// 已存在文档（读旧值 `Some`）的 CAS：库中值被并发改写 → 明确 SyncConflict，
/// 队头与远端已知 VV 都保持原值（绝不回退到本批并集）。
#[test]
fn baseline_remote_known_cas_fails_closed_for_existing_doc() {
    let (_dir, vault) = fresh_vault();
    begin_with(&vault, 5, "src1", &expectation(&["doc-cas-old"]));
    apply_baseline_batch(
        &vault,
        &[(
            "doc-cas-old".to_owned(),
            snapshot("doc-cas-old", PEER_B, "first"),
        )],
        &[],
    )
    .expect("first baseline");
    assert!(matches!(
        finish_baseline_apply(&vault, 5, "c5").expect("finish"),
        BaselineApplyFinish::Applied(_)
    ));
    let before_known = meta_remote_known_bytes(&vault, "doc-cas-old").expect("已有远端已知 VV");
    let before_head = load_head(&vault, "doc-cas-old").expect("head");

    begin_with(&vault, 6, "src2", &expectation(&["doc-cas-old"]));
    install_remote_known_clobber_trigger(&vault, "doc-cas-old");
    let error = apply_baseline_batch(
        &vault,
        &[(
            "doc-cas-old".to_owned(),
            snapshot("doc-cas-old", PEER_B, "second"),
        )],
        &[],
    )
    .expect_err("陈旧旧值的 CAS 必须失败");

    assert!(
        matches!(&error, StorageError::SyncConflict { context } if context.contains("doc-cas-old")),
        "必须是带文档上下文的 SyncConflict，实际 {error:?}"
    );
    assert_eq!(
        meta_remote_known_bytes(&vault, "doc-cas-old"),
        Some(before_known),
        "CAS 失败后已有值必须保持原样（不能变小，也不能被本批并集覆盖）"
    );
    assert_eq!(
        load_head(&vault, "doc-cas-old").expect("head").head_seq,
        before_head.head_seq,
        "整批回滚：head 不得推进"
    );
}

/// 正常路径是单调并集：普通 pull 已记录的远端 ops（本批快照里没有）必须保留。
#[test]
fn baseline_remote_known_write_unions_with_normal_pull_evidence() {
    let (_dir, vault) = fresh_vault();
    let mut pulled = VersionVector::new();
    pulled.insert(0xA1, 2);
    vault
        .exec(
            "INSERT INTO meta(key,value) VALUES(?1,?2)",
            &[
                Value::Text("sync.remote_known_vv.doc-union".to_owned()),
                Value::Blob(pulled.encode()),
            ],
        )
        .expect("预置普通 pull 的远端已知 VV");

    begin_with(&vault, 7, "src", &expectation(&["doc-union"]));
    apply_baseline_batch(
        &vault,
        &[(
            "doc-union".to_owned(),
            snapshot("doc-union", PEER_B, "remote body"),
        )],
        &[],
    )
    .expect("baseline apply");

    let known = meta_remote_known(&vault, "doc-union").expect("应用后必须有远端已知 VV");
    assert_eq!(
        known.get(&0xA1).copied(),
        Some(2),
        "普通 pull 的旧证据必须保留"
    );
    assert!(
        known.get(&PEER_B).copied().unwrap_or(0) >= 1,
        "本批快照的 VV 必须并入"
    );
    assert_eq!(
        count(
            &vault,
            "SELECT count(*) FROM meta WHERE key='sync.remote_known_vv.doc-union'",
            &[]
        ),
        1
    );
}

// ---------------------------------------------------------------------------
// Phase1B 门禁：结构文档（`doc_id = "structure"`）的 detached local_extra 生产路径
// ---------------------------------------------------------------------------

/// Phase1B 门禁：本地已有结构树时应用 baseline 结构快照，必须真正走
/// **detached local_extra** 分支（`existing_local_extra` 计数），而不是 attached/no-extra
/// 或 new-doc 分支；合并后本地与基线的节点都在、父子关系与根节点数不变，本地 op 不丢。
///
/// 与正文用例的差别：结构文档的 `loro_docs.kind` 必须是 `'structure'`，读回走
/// [`load_structure`]（自带重放 frontier 校验），而不是 `load_body_doc`。
#[test]
fn structure_doc_local_extra_merges_local_and_baseline_nodes() {
    const LOCAL_PEER: u64 = 0x5101_0001;
    const BASELINE_PEER: u64 = 0xB0B0_0002;

    let (_dir, vault) = fresh_vault();

    // 1) 本地结构：一个 folder + 一个 note，并把 note 挂到 folder 下。
    let mut writer = StructureWriter::open(&vault, LOCAL_PEER).expect("打开结构写入口");
    let local_folder = writer
        .doc()
        .create_folder("本地文件夹", 1_700_000_000)
        .expect("创建本地文件夹");
    let local_note = writer
        .doc()
        .create_note("本地笔记", 1_700_000_001)
        .expect("创建本地笔记");
    writer
        .doc()
        .move_node(
            &local_note.node_id,
            Some(local_folder.as_str()),
            1_700_000_002,
        )
        .expect("把本地笔记挂到本地文件夹下");
    writer.commit("local-structure").expect("提交本地结构");

    let local_head = head(&vault, STRUCTURE_DOC_ID);
    assert_eq!(local_head.head_seq, 1, "本地一次结构提交应产生一条 update");
    // 默认 `StructureWriter::open` 不写 outbox：本用例证明 local_extra 由 VV 判定，
    // 不依赖任何本地出站意图。
    assert_eq!(
        count(
            &vault,
            "SELECT count(*) FROM sync_outbox WHERE doc_id=?1",
            &[Value::Text(STRUCTURE_DOC_ID.to_owned())],
        ),
        0,
        "StructureWriter::open 默认路径不得写 outbox"
    );

    // 2) baseline 结构：另一个 peer 的独立树（不同 title / note_id）。
    let baseline = StructureDoc::create(BASELINE_PEER).expect("创建基线结构文档");
    let baseline_folder = baseline
        .create_folder("基线文件夹", 1_700_100_000)
        .expect("创建基线文件夹");
    let baseline_note = baseline
        .create_note("基线笔记", 1_700_100_001)
        .expect("创建基线笔记");
    baseline
        .move_node(
            &baseline_note.node_id,
            Some(baseline_folder.as_str()),
            1_700_100_002,
        )
        .expect("把基线笔记挂到基线文件夹下");
    let baseline_snapshot_bytes = baseline.export_snapshot().expect("导出基线结构快照");
    assert_ne!(
        local_note.note_id, baseline_note.note_id,
        "两侧 note_id 必须可区分"
    );

    // 3) begin：预期参与集合必须含 "structure"。
    let begin_state = begin_with(
        &vault,
        5,
        "structure-cursor",
        &expectation(&[STRUCTURE_DOC_ID]),
    );
    assert_eq!(begin_state.expected_docs, Some(1));
    assert_eq!(begin_state.applied_docs, 0);
    assert!(!begin_state.finishable, "尚未应用任何批次");

    // 4) 应用快照；计数器证明走的是 detached local_extra 分支。
    reset_baseline_apply_debug();
    apply_baseline_batch(
        &vault,
        &[(STRUCTURE_DOC_ID.to_owned(), baseline_snapshot_bytes)],
        &[],
    )
    .expect("应用基线结构快照");

    let counters = baseline_apply_debug_counters();
    assert!(
        counters.existing_local_extra >= 1,
        "结构文档必须走 local_extra 分支，实际计数 {}",
        counters.existing_local_extra
    );
    assert_eq!(
        counters.existing_local_extra, 1,
        "本批只有一个结构文档，应恰好命中一次 local_extra"
    );
    assert_eq!(
        counters.existing_no_extra, 0,
        "不能落在 attached/no-extra 分支"
    );
    assert_eq!(counters.new_docs, 0, "结构文档已存在，不能走 new-doc 分支");
    assert_eq!(counters.snapshot_imports, 1);
    assert_eq!(
        counters.snapshot_reimport_bytes, 0,
        "不得二次 import 基线快照"
    );
    assert_eq!(counters.load_body_doc_calls, 1, "只允许一次本地重放");
    assert!(counters.local_delta_bytes > 0, "反向导入的本地增量不能为空");
    assert_eq!(counters.pending_update_calls, 1);
    assert!(counters.pending_update_bytes > 0);

    // 5) 读回：本地与基线节点都在，树形状保持，本地 op 没有丢。
    let (structure, structure_head) =
        load_structure(&vault, LOCAL_PEER).expect("读回结构文档（含 frontier 校验）");
    assert_eq!(
        structure_head.head_seq,
        local_head.head_seq + 1,
        "baseline merge 应追加一条 update，而不是覆盖本地 update"
    );

    let merged = structure.snapshot().expect("结构快照");

    let local_note_node = merged
        .nodes
        .iter()
        .find(|node| node.title == "本地笔记")
        .expect("合并后缺少本地笔记");
    assert_eq!(local_note_node.node_id, local_note.node_id);
    assert_eq!(
        local_note_node.note_id.as_deref(),
        Some(local_note.note_id.as_str())
    );
    assert_eq!(
        local_note_node.body_doc_id.as_deref(),
        Some(local_note.body_doc_id.as_str())
    );
    assert_eq!(
        local_note_node.parent.as_deref(),
        Some(local_folder.as_str()),
        "本地笔记的父子关系不能被基线覆盖"
    );
    assert!(local_note_node.is_note());
    assert!(!local_note_node.deleted);

    let local_folder_node = merged
        .nodes
        .iter()
        .find(|node| node.title == "本地文件夹")
        .expect("合并后缺少本地文件夹");
    assert_eq!(local_folder_node.node_id, local_folder);
    assert_eq!(local_folder_node.parent, None, "本地文件夹应仍在根级");
    assert!(!local_folder_node.is_note());

    let baseline_note_node = merged
        .nodes
        .iter()
        .find(|node| node.title == "基线笔记")
        .expect("合并后缺少基线笔记");
    assert_eq!(baseline_note_node.node_id, baseline_note.node_id);
    assert_eq!(
        baseline_note_node.note_id.as_deref(),
        Some(baseline_note.note_id.as_str())
    );
    assert_eq!(
        baseline_note_node.parent.as_deref(),
        Some(baseline_folder.as_str()),
        "基线父子关系必须保留"
    );
    assert!(baseline_note_node.is_note());

    let baseline_folder_node = merged
        .nodes
        .iter()
        .find(|node| node.title == "基线文件夹")
        .expect("合并后缺少基线文件夹");
    assert_eq!(baseline_folder_node.node_id, baseline_folder);
    assert_eq!(baseline_folder_node.parent, None, "基线文件夹应在根级");

    // 树形状：两个根（两侧各建一个文件夹），每个根下恰好一个笔记。
    let roots: Vec<_> = merged
        .nodes
        .iter()
        .filter(|node| node.parent.is_none())
        .collect();
    assert_eq!(roots.len(), 2, "合并后应有两个根节点");
    assert!(
        !merged.nodes.iter().any(|node| node.deleted),
        "本用例没有删除事件，合并不得产生 deleted 节点"
    );
    for root in roots {
        let children: Vec<_> = merged
            .nodes
            .iter()
            .filter(|node| node.parent.as_deref() == Some(root.node_id.as_str()))
            .collect();
        assert_eq!(children.len(), 1, "每个根文件夹下应各有一个笔记");
        assert!(children[0].is_note());
    }

    // 本地 op 仍在库里的独立 update 中，baseline merge 是追加的第二个批次。
    let batches: Vec<String> = vault
        .query(
            "SELECT batch_id FROM loro_updates WHERE doc_id=?1 ORDER BY seq",
            &[Value::Text(STRUCTURE_DOC_ID.to_owned())],
        )
        .expect("查询结构文档批次")
        .into_iter()
        .map(|row| match row.into_iter().next() {
            Some(Value::Text(value)) => value,
            other => panic!("结构批次 batch_id 异常：{other:?}"),
        })
        .collect();
    assert_eq!(batches.len(), 2, "本地一条 + baseline merge 一条");
    assert_eq!(batches[0], "local-structure", "本地 op 必须保留");
    assert_eq!(batches[1], BASELINE_MERGE_BATCH);
    assert_eq!(
        count(
            &vault,
            "SELECT count(*) FROM loro_docs WHERE doc_id=?1 AND kind='structure'",
            &[Value::Text(STRUCTURE_DOC_ID.to_owned())],
        ),
        1,
        "baseline merge 不得改掉 kind='structure'"
    );
    assert_eq!(
        count(
            &vault,
            "SELECT count(*) FROM baseline_apply_docs WHERE doc_id=?1 AND applied=1",
            &[Value::Text(STRUCTURE_DOC_ID.to_owned())],
        ),
        1,
        "journal 必须记录 structure 已应用"
    );

    // 6) finish：预期集合全齐 → Applied；只动水位/cursor，结构内容不变。
    let outcome = finish_baseline_apply(&vault, 5, "chosen-cursor").expect("finish");
    let BaselineApplyFinish::Applied(state) = outcome else {
        panic!("预期集合全齐时 finish 必须 Applied：{outcome:?}");
    };
    assert_eq!(state.status, BaselineApplyStatus::Completed);
    assert_eq!(state.applied_docs, 1);
    assert_eq!(state.expected_docs, Some(1));
    assert!(!state.finishable);

    let cursor = sync_cursor_state(&vault).expect("cursor");
    assert_eq!(cursor.received_seq, 5);
    assert_eq!(cursor.integrated_seq, 5);
    assert_eq!(cursor.confirmed_highwater, Some(5));

    let (after_finish, after_head) =
        load_structure(&vault, LOCAL_PEER).expect("finish 后读回结构文档");
    assert_eq!(
        after_head.head_seq, structure_head.head_seq,
        "finish 只推进水位，不得改结构 head"
    );
    assert_eq!(
        after_finish.snapshot().expect("finish 后快照").nodes.len(),
        merged.nodes.len(),
        "finish 后节点数不变"
    );
}
