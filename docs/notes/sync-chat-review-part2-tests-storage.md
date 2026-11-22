# 同步评审 Part 2/6：测试与 storage 会话代码

@@PART 2/6

> 这是 **Part 2/6**。请先只确认收到，不要开始评审。
> 本部分约 537791 字符；应低于平台约 72 万字符的截断点。
> 内容与 Part 1 连续；全部到齐后再集中评审。代码截止 `1bededa`。

## 收到确认（请只回复这三行）

1. `PART 2/6 RECEIVED`
2. 本部分 `@@SECTION` 数量（应为 20）
3. 最后一个完整文件（若可见）

---

# 本部分附录

@@SECTION code/085 `crates/storage/tests/baseline_apply.rs`


`````rust
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
    begin_baseline_apply, create_note_slot, finish_baseline_apply, load_body_doc, load_head,
    reset_baseline_apply_debug, set_sync_log_id, store_inbox_verified_batch, sync_cursor_state,
    BaselineApplyFinish, BaselineApplyStatus, BaselineAsset, BaselineExpectation, BodyDoc,
    StorageError, SyncContext, UpdateRecord, Value, Vault, VerifiedInboxRow,
    META_SYNC_CONFIRMED_HIGHWATER, META_SYNC_INTEGRATED_SEQ, META_SYNC_RECEIVED_SEQ,
    META_SYNC_RELAY_CURSOR, SCHEMA_VERSION,
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
// P1-2：tombstone 判定与 notes 软删之间的 TOCTOU
// ---------------------------------------------------------------------------

/// 判定（事务外看到无 outbox）之后、事务执行期间插进一条 outbox：事务内重判必须把
/// action 降级为 kept_local_edit，`notes.deleted` 保持 0。
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

`````



@@SECTION code/086 `crates/storage/tests/journal.rs`


`````rust
//! 未 ACK journal 与背压：连续前缀、durable 后裁剪、慢盘合并、失败不丢已 ACK 部分。
//!
//! 另覆盖 P1-3 的 head 过期契约（见 `crates/storage/src/journal.rs` 模块头）：
//! 外部推进 head 后 flush 必须返回**可识别**的终态错误而不是裸 `HeadConflict` 或挂死；
//! 按契约重建 sink 后新条目可正常 durable；旧 journal 未 durable 条目被**显式交还**，
//! 既不写库也不静默丢弃。

mod support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use loro::VersionVector;
use support::*;

use menghunote_storage::{
    append_updates, load_body_doc, load_head, DurableSink, FlushReport, JournalEntry,
    JournalOrigin, JournalWriter, StorageError, UpdateRecord, VaultJournalSink,
};

/// 可注入慢盘/失败的 sink：记录每次 flush 的批大小，便于断言「合并」。
#[derive(Debug, Default)]
struct ScriptedSink {
    /// 每次 write_prefix 收到的条数。
    batches: Vec<usize>,
    /// 剩余几次要注入失败。
    fail_times: usize,
    /// 已 durable 的条目（按到达顺序）。
    durable: Vec<JournalEntry>,
    /// 已 durable 的条数（充作 seq）。
    seq: u64,
}

impl ScriptedSink {
    fn with_failures(fail_times: usize) -> Self {
        Self {
            fail_times,
            ..Self::default()
        }
    }
}

impl DurableSink for ScriptedSink {
    fn write_prefix(&mut self, entries: &[JournalEntry]) -> Result<u64, StorageError> {
        self.batches.push(entries.len());
        if self.fail_times > 0 {
            self.fail_times -= 1;
            // 注入失败：一个字节都不写（模拟事务回滚）
            return Err(StorageError::Corrupt {
                context: "注入的磁盘失败".to_owned(),
            });
        }
        for entry in entries {
            self.durable.push(entry.clone());
            self.seq += 1;
        }
        Ok(self.seq)
    }

    fn head_seq(&self) -> u64 {
        self.seq
    }
}

/// 计数包装：证明 stale 之后 writer **不再**触碰 sink（不重试）。
#[derive(Debug)]
struct CountingSink {
    inner: VaultJournalSink,
    attempts: Arc<AtomicUsize>,
}

impl DurableSink for CountingSink {
    fn write_prefix(&mut self, entries: &[JournalEntry]) -> Result<u64, StorageError> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        self.inner.write_prefix(entries)
    }

    fn head_seq(&self) -> u64 {
        self.inner.head_seq()
    }
}

fn entry(tag: &str) -> JournalEntry {
    JournalEntry {
        doc_id: "doc-1".to_owned(),
        batch_id: format!("batch-{tag}"),
        update: menghunote_storage::PendingUpdate {
            payload: tag.as_bytes().to_vec(),
            end_frontier: vec![0],
            end_vv: vec![0],
        },
        origin: JournalOrigin::Local,
    }
}

#[test]
fn journal_keeps_fifo_order_and_acks_contiguous_prefix() {
    let mut journal = menghunote_storage::PendingJournal::new(4);
    for tag in ["a", "b", "c"] {
        journal.append(entry(tag)).expect("append");
    }
    assert_eq!(journal.pending_len(), 3);

    // 只取前缀，不改变 journal
    let prefix: Vec<String> = journal
        .peek_prefix(2)
        .iter()
        .map(|item| item.batch_id.clone())
        .collect();
    assert_eq!(prefix, ["batch-a", "batch-b"]);
    assert_eq!(journal.pending_len(), 3, "peek 不是出队");

    journal.ack_prefix(2).expect("裁剪前缀");
    assert_eq!(journal.pending_len(), 1);
    assert_eq!(journal.peek_prefix(4)[0].batch_id, "batch-c");

    // 不能裁剪超过未 ACK 条数
    assert!(journal.ack_prefix(2).is_err());
}

#[test]
fn journal_backpressure_rejects_when_full_and_never_grows_unbounded() {
    let mut journal = menghunote_storage::PendingJournal::new(3);
    for tag in ["a", "b", "c"] {
        journal.append(entry(tag)).expect("前三条应接受");
    }
    assert!(journal.is_full());

    // 满了之后必须报背压，且**不写入**（无界增长被挡住）
    for tag in ["d", "e", "f"] {
        let error = journal.append(entry(tag)).expect_err("满容量必须背压");
        assert!(
            matches!(error, StorageError::WriteBackpressure { capacity: 3 }),
            "期望 WriteBackpressure，实际 {error}"
        );
        assert_eq!(journal.pending_len(), 3, "被拒的条目不能进 journal");
    }
    assert_eq!(journal.capacity(), 3);
}

#[test]
fn slow_io_coalesces_pending_prefix_into_one_flush() {
    let sink = ScriptedSink::default();
    let mut writer = JournalWriter::new(sink, 8, 8);

    // 慢盘时调用方会先攒着：一次 flush 应把整段前缀合并成一次写
    for tag in ["a", "b", "c", "d", "e"] {
        writer.submit(entry(tag)).expect("submit");
    }
    let report = writer.flush().expect("flush");
    assert_eq!(
        report,
        FlushReport {
            entries: 5,
            durable_seq: 5
        }
    );
    assert_eq!(writer.pending_len(), 0);
    assert_eq!(writer.durable_seq(), 5);
    assert_eq!(writer.flushed_entries(), 5);

    let sink = writer.into_sink();
    assert_eq!(sink.batches, [5], "5 条必须合并成一次 sink 写");
    assert_eq!(sink.durable.len(), 5);
    assert_eq!(sink.durable[0].batch_id, "batch-a", "顺序不能乱");
}

#[test]
fn failed_flush_loses_nothing_and_retries_to_the_same_prefix() {
    let sink = ScriptedSink::with_failures(2);
    let mut writer = JournalWriter::new(sink, 8, 8);
    for tag in ["a", "b", "c"] {
        writer.submit(entry(tag)).expect("submit");
    }

    // 连续两次注入失败：一条都不能少（sink 原子回滚）
    for _ in 0..2 {
        let error = writer.flush().expect_err("注入的失败应上报");
        assert!(matches!(error, StorageError::Corrupt { .. }));
        assert_eq!(writer.pending_len(), 3, "失败后 journal 原样保留");
        assert_eq!(writer.durable_seq(), 0, "失败不得推进 durable 水位");
    }

    // 第三次成功：仍然是从头开始的同一前缀
    let report = writer.flush().expect("第三次 flush");
    assert_eq!(report.entries, 3);
    assert_eq!(writer.pending_len(), 0);
    let sink = writer.into_sink();
    assert_eq!(sink.batches, [3, 3, 3], "每次失败也尝试了同一批前缀");
    assert_eq!(
        sink.durable
            .iter()
            .map(|e| e.batch_id.as_str())
            .collect::<Vec<_>>(),
        ["batch-a", "batch-b", "batch-c"]
    );
}

#[test]
fn max_batch_splits_prefix_without_gaps() {
    let sink = ScriptedSink::default();
    let mut writer = JournalWriter::new(sink, 8, 2);
    for tag in ["a", "b", "c", "d", "e"] {
        writer.submit(entry(tag)).expect("submit");
    }

    assert_eq!(writer.flush().expect("1").entries, 2);
    assert_eq!(writer.pending_len(), 3);
    let total = writer.flush_all().expect("排空");
    assert_eq!(total, 3);
    assert_eq!(writer.pending_len(), 0);

    let sink = writer.into_sink();
    assert_eq!(sink.batches, [2, 2, 1], "按 max_batch 切片，无空洞");
    assert_eq!(
        sink.durable
            .iter()
            .map(|e| e.batch_id.as_str())
            .collect::<Vec<_>>(),
        ["batch-a", "batch-b", "batch-c", "batch-d", "batch-e"]
    );
}

#[test]
fn unacked_suffix_is_lost_but_acked_prefix_survives_reopen() {
    // 真实落库：A 事务 + 崩溃（丢掉 writer 与 vault）
    let (dir, vault) = fresh_vault();
    let doc = seed_note(&vault, "note-1", "doc-1");

    let mut base = VersionVector::default();
    let mut entries = Vec::new();
    for (index, ch) in ["甲", "乙", "丙"].iter().enumerate() {
        doc.insert_scalars(doc.scalar_len(), ch).expect("编辑");
        let update = doc.pending_update(&base).expect("导出增量");
        base = VersionVector::decode(&update.end_vv).expect("推进基准");
        entries.push(JournalEntry {
            doc_id: "doc-1".to_owned(),
            batch_id: format!("batch-{index}"),
            update,
            origin: JournalOrigin::Local,
        });
    }

    let sink = VaultJournalSink::new(vault.clone(), "doc-1", 0);
    let mut writer = JournalWriter::new(sink, 4, 2);
    for entry in entries {
        writer.submit(entry).expect("submit");
    }
    // 只 flush 前 2 条（max_batch=2），第三条停在内存里
    let report = writer.flush().expect("flush 前缀");
    assert_eq!(report.entries, 2);
    assert_eq!(writer.pending_len(), 1, "第三条仍是未 ACK");
    assert_eq!(writer.durable_seq(), 2);

    // 崩溃：未 ACK 的第三条随内存一起丢
    drop(writer);
    drop(vault);

    let vault = open_vault(&vault_path(&dir));
    assert_eq!(
        count(
            &vault,
            "SELECT count(*) FROM loro_updates WHERE doc_id = 'doc-1'",
            &[]
        ),
        2,
        "只有已 ACK 的两条落库"
    );
    assert_eq!(load_head(&vault, "doc-1").expect("head").head_seq, 2);

    let (replayed, _) = load_body_doc(&vault, "doc-1", PEER_A, None).expect("重放");
    assert_eq!(
        replayed.text(),
        "甲乙",
        "只丢未 ACK 的连续前缀之后，已 ACK 部分完好"
    );
}

#[test]
fn sink_holds_head_watermark_and_rejects_mixed_docs() {
    let (_dir, vault) = fresh_vault();
    let doc = seed_note(&vault, "note-1", "doc-1");

    let base = VersionVector::default();
    doc.insert_scalars(0, "甲").expect("编辑");
    let update = doc.pending_update(&base).expect("增量");

    let mut sink = VaultJournalSink::new(vault.clone(), "doc-1", 0);
    assert_eq!(sink.head_seq(), 0);
    let first = JournalEntry {
        doc_id: "doc-1".to_owned(),
        batch_id: "b1".to_owned(),
        update: update.clone(),
        origin: JournalOrigin::Local,
    };
    let seq = sink
        .write_prefix(std::slice::from_ref(&first))
        .expect("写 durable");
    assert_eq!(seq, 1);
    assert_eq!(sink.head_seq(), 1, "sink 内部水位必须跟着推进");

    // 混入别的文档：必须拒绝（否则 seq 会串到别的 doc 上）
    let mixed = [
        first,
        JournalEntry {
            doc_id: "doc-other".to_owned(),
            batch_id: "b1".to_owned(),
            update,
            origin: JournalOrigin::Local,
        },
    ];
    let error = sink.write_prefix(&mixed).expect_err("混文档必须拒绝");
    assert!(matches!(error, StorageError::Corrupt { .. }));
}

/// P1-3：外部推进 head 后，flush 必须给出**可识别**的终态错误，不重试、不挂死，
/// 且旧条目既没写库也没被静默丢掉；按契约重建 sink 后新条目可正常 durable。
#[test]
fn stale_head_is_terminal_then_rebuild_hands_back_entries_and_new_entries_flush() {
    let (_dir, vault) = fresh_vault();
    let doc = seed_note(&vault, "note-1", "doc-1");

    // 本地编辑：按**空基准**导出，这就是 writer 手里的字节（追加时的 head 快照）。
    doc.insert_scalars(0, "甲").expect("编辑");
    let local = doc
        .pending_update(&VersionVector::default())
        .expect("导出本地增量");

    // writer 先按 head 0 建好 sink 并提交条目（真实路径同序：sink 先于外部推进）。
    let attempts = Arc::new(AtomicUsize::new(0));
    let sink = CountingSink {
        inner: VaultJournalSink::new(vault.clone(), "doc-1", 0),
        attempts: Arc::clone(&attempts),
    };
    let mut writer = JournalWriter::new(sink, 4, 4);
    assert_eq!(writer.durable_seq(), 0, "空 journal 时报告 sink 的真实水位");
    writer
        .submit(JournalEntry {
            doc_id: "doc-1".to_owned(),
            batch_id: "stale-batch".to_owned(),
            update: local.clone(),
            origin: JournalOrigin::Local,
        })
        .expect("submit");

    // 外部 writer（同步引擎 integrate / 另一个 Vault）在同一 doc 上推进 head。
    doc.insert_scalars(doc.scalar_len(), "乙").expect("编辑");
    let external = doc
        .pending_update(&VersionVector::default())
        .expect("导出外部增量");
    let advanced = append_updates(
        &vault,
        "doc-1",
        0,
        &[UpdateRecord::new("external-1", external)],
    )
    .expect("外部推进 head");
    assert_eq!(advanced.head_seq, 1);

    // 1) 第一次 flush：终态错误，而不是裸 HeadConflict。
    let error = writer.flush().expect_err("head 已被推进，flush 必须失败");
    match &error {
        StorageError::JournalHeadStale {
            doc_id,
            expected,
            actual,
            pending,
        } => {
            assert_eq!(doc_id, "doc-1");
            assert_eq!(*expected, 0, "错误必须带上写入用的 CAS 基准");
            assert_eq!(*actual, 1, "错误必须带上库里真实的 head");
            assert_eq!(*pending, 1, "错误必须说明有多少条未 durable 条目受影响");
        }
        other => panic!("期望 JournalHeadStale，实际 {other:?}"),
    }
    assert!(
        !matches!(&error, StorageError::HeadConflict { .. }),
        "journal 层不得把裸 HeadConflict 漏给调用方"
    );
    assert!(
        error.requires_session_rebuild(),
        "CAS 过期只能靠重建 session"
    );
    assert!(!error.is_retryable(), "CAS 过期不是可重试错误");
    assert_eq!(writer.pending_len(), 1, "失败不得丢条目");
    assert_eq!(writer.durable_seq(), 0, "失败不得推进 durable 水位");
    assert_eq!(writer.flushed_entries(), 0);
    assert!(writer.is_stale());
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        1,
        "CAS 冲突只尝试一次，不自动重试"
    );

    // 2) 再 flush / flush_all：幂等返回同一终态错误，且**不再碰数据库**（不会挂死、不刷屏）。
    for _ in 0..2 {
        let again = writer.flush().expect_err("stale 必须继续报错");
        assert!(
            matches!(again, StorageError::JournalHeadStale { pending: 1, .. }),
            "实际 {again:?}"
        );
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            1,
            "stale 之后不得重试 sink"
        );
    }
    assert!(writer.flush_all().is_err(), "排空也必须明确失败");

    // 3) stale 期间 submit 被拒绝：writer 已冻结，新条目必须交给重建后的 writer。
    let blocked = writer
        .submit(JournalEntry::local("doc-1", "later", local.clone()))
        .expect_err("stale 时 submit 必须被拒绝");
    assert!(
        matches!(blocked, StorageError::JournalHeadStale { pending: 1, .. }),
        "实际 {blocked:?}"
    );
    assert_eq!(writer.pending_len(), 1, "被拒的条目不能进 journal");

    // 4) 按契约重建 sink（新 head_seq）：旧条目**显式交还**，journal 清空，水位对齐新基准。
    let handed_back = writer
        .rebuild_sink(CountingSink {
            inner: VaultJournalSink::new(vault.clone(), "doc-1", 1),
            attempts: Arc::clone(&attempts),
        })
        .expect("stale 后重建 sink");
    assert_eq!(handed_back.len(), 1, "旧条目必须原样交还，不得静默丢弃");
    assert_eq!(handed_back[0].batch_id, "stale-batch");
    assert_eq!(handed_back[0].update, local);
    assert_eq!(writer.pending_len(), 0, "交还后 journal 清空");
    assert!(!writer.is_stale());
    assert_eq!(writer.durable_seq(), 1, "重建后水位 = 新 sink 的 CAS 基准");

    // 旧条目没有被偷偷写进库：库里仍只有外部那一条。
    assert_eq!(
        count(
            &vault,
            "SELECT count(*) FROM loro_updates WHERE doc_id = 'doc-1'",
            &[]
        ),
        1,
        "过期条目不得写入 loro_updates"
    );
    let (replayed, _) = load_body_doc(&vault, "doc-1", PEER_A, None).expect("重放");
    assert_eq!(replayed.text(), "甲乙", "只有外部那一条落库");

    // 5) 重建：用新 head 的 VV 重新导出（会话层的 ③），新条目 flush 成功且水位正确。
    let base = advanced.version_vector().expect("解码新 head 的 VV");
    doc.insert_scalars(doc.scalar_len(), "丙").expect("编辑");
    let fresh = doc.pending_update(&base).expect("按新 head 重新导出增量");
    writer
        .submit(JournalEntry {
            doc_id: "doc-1".to_owned(),
            batch_id: "fresh-1".to_owned(),
            update: fresh,
            origin: JournalOrigin::Local,
        })
        .expect("重建后可 submit");

    let report = writer.flush().expect("重建后的新条目必须能 flush");
    assert_eq!(
        report,
        FlushReport {
            entries: 1,
            durable_seq: 2
        }
    );
    assert_eq!(writer.durable_seq(), 2);
    assert_eq!(writer.flushed_entries(), 1);
    assert_eq!(writer.pending_len(), 0);
    assert_eq!(attempts.load(Ordering::SeqCst), 2, "重建后只多一次真实写入");
    assert_eq!(
        count(
            &vault,
            "SELECT count(*) FROM loro_updates WHERE doc_id = 'doc-1'",
            &[]
        ),
        2
    );
    assert_eq!(load_head(&vault, "doc-1").expect("head").head_seq, 2);
    let (replayed, _) = load_body_doc(&vault, "doc-1", PEER_A, None).expect("重放");
    assert_eq!(replayed.text(), "甲乙丙", "重建 + 重导出的内容完整");
}

/// 未过期时不允许重建 sink：那会把仍可写入的条目丢掉（契约的反向保护）。
#[test]
fn rebuild_sink_is_rejected_while_not_stale() {
    let (_dir, vault) = fresh_vault();
    let doc = seed_note(&vault, "note-1", "doc-1");
    doc.insert_scalars(0, "甲").expect("编辑");
    let update = doc.pending_update(&VersionVector::default()).expect("增量");

    let mut writer = JournalWriter::new(VaultJournalSink::new(vault.clone(), "doc-1", 0), 4, 4);
    writer
        .submit(JournalEntry::local("doc-1", "b1", update))
        .expect("submit");
    assert!(!writer.is_stale());

    let error = writer
        .rebuild_sink(VaultJournalSink::new(vault.clone(), "doc-1", 0))
        .expect_err("未 stale 时重建必须被拒绝");
    assert!(
        matches!(error, StorageError::Corrupt { .. }),
        "实际 {error:?}"
    );
    assert_eq!(writer.pending_len(), 1, "拒绝重建不得动 journal");
    assert_eq!(writer.flush().expect("正常 flush 仍可用").entries, 1);
}

`````



@@SECTION code/087 `crates/storage/tests/remote_known.rs`


`````rust
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

`````



@@SECTION code/088 `crates/storage/tests/sync_inbox_batch.rs`


`````rust
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

`````



@@SECTION code/089 `crates/storage/tests/meta_value_types.rs`


`````rust
//! P2-6：`meta.value` 的三个读取入口必须只有**一套**语义 —— 只认 BLOB。
//!
//! | 入口 | 实际代码路径 |
//! | --- | --- |
//! | [`menghunote_storage::Vault::get_meta`] | actor 线程上的 `GetMeta` 命令 |
//! | `sync_bridge::read_meta` | `sync_cursor_state` / `take_inbox_ready` / `set_sync_log_id` 等同步桥读取 |
//! | `sync_meta::read_meta_on` | worker 独占连接 / 批量 inbox 事务内的读取 |
//!
//! 为什么专门测非 BLOB：`meta.value` 是 `BLOB NOT NULL`（`schema/v1.sql` 里的 STRICT
//! 表），正常写入路径**写不出** TEXT/INTEGER/REAL。能读出来只可能是"库被外部工具改坏
//! 或 schema 漂移"，而旧行为对它有两种互相矛盾的解读：
//!
//! - `Vault::get_meta` 把 TEXT 当字节返回；
//! - `sync_bridge::read_meta` / `sync_meta::read_meta_on` 把 TEXT 当**缺失**。
//!
//! 后者会静默绕过持久屏障与游标：`sync.log_id` 读不到就当"还没绑定 incarnation"
//! （`take_inbox_ready` 静默返回空，`set_sync_log_id` 还会把损坏值覆盖掉，等于悄悄
//! 换绑传代），`sync.repack_required` 读不到就当"不需要 repack"。
//!
//! 测试库用 `PRAGMA writable_schema` 把 `meta` 表改回非 STRICT，从而如实构造出这种
//! 损坏状态（STRICT 表拒绝非 BLOB 写入，测试里没有别的办法造出 TEXT）。

mod support;

use std::path::Path;

use support::*;

use menghunote_storage::{
    advance_integrated_prefix, repack_required, set_repack_required, set_sync_log_id,
    sync_cursor_state, take_inbox_ready, StorageError, Value, Vault, META_SYNC_LOG_ID,
    META_SYNC_REPACK_REQUIRED,
};

/// 测试用的合法 relay log incarnation（16B BLOB）。
const LOG_ID: [u8; 16] = [0x2f; 16];

/// 把测试库的 `meta` 表从 STRICT 改成非 STRICT。
///
/// 只改 `sqlite_master` 里那条 DDL（去掉唯一的 `STRICT` 后缀），其余列定义逐字保留；
/// 之后的连接会按非 STRICT 解析这张表，于是能写入 TEXT/INTEGER/REAL —— 正是"库被外部
/// 改坏"的形状。
fn relax_meta_table(path: &Path) {
    let conn = rusqlite::Connection::open(path).expect("打开原始连接");
    conn.execute_batch("PRAGMA writable_schema=ON")
        .expect("打开 writable_schema");
    let ddl: String = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE name='meta'",
            [],
            |row| row.get(0),
        )
        .expect("读 meta 表 DDL");
    let relaxed = ddl.replacen(" STRICT", "", 1);
    assert_ne!(relaxed, ddl, "meta 表 DDL 应带 STRICT：{ddl}");
    conn.execute(
        "UPDATE sqlite_master SET sql=?1 WHERE name='meta'",
        [relaxed],
    )
    .expect("改写 meta 表 DDL");
    conn.execute_batch("PRAGMA writable_schema=OFF")
        .expect("关闭 writable_schema");
    drop(conn);
}

/// 建一个 vault，把 `meta` 表改成非 STRICT 后重新打开。
///
/// 重新打开走 [`Vault::open`]：`user_version` 已是 `SCHEMA_VERSION`，迁移不会重跑，
/// 所以非 STRICT 的 `meta` 表被原样沿用。
fn vault_with_typed_meta() -> (tempfile::TempDir, Vault) {
    let dir = temp_dir();
    let path = vault_path(&dir);
    {
        let vault = open_vault(&path);
        // 先写一版合法 BLOB，确保 schema 与 WAL 都已落盘，再改表结构。
        set_sync_log_id(&vault, &LOG_ID).expect("预置合法 BLOB");
    }
    relax_meta_table(&path);
    let vault = open_vault(&path);
    (dir, vault)
}

/// 把一个 meta 键写成指定 SQLite 类型（绕过 Vault 的 BLOB 写入路径）。
fn write_typed_meta(vault: &Vault, key: &str, value: Value) {
    vault
        .exec(
            "INSERT INTO meta(key,value) VALUES(?1,?2) \
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            &[Value::Text(key.to_owned()), value],
        )
        .expect("写入指定类型的 meta");
}

/// `meta.value` 的实际 SQLite 类型名（`typeof()`，用于确认构造出的损坏状态）。
fn value_type(vault: &Vault, key: &str) -> String {
    let rows = vault
        .query(
            "SELECT typeof(value) FROM meta WHERE key=?1",
            &[Value::Text(key.to_owned())],
        )
        .expect("读 typeof(value)");
    match rows.first().and_then(|row| row.first()) {
        Some(Value::Text(name)) => name.clone(),
        other => panic!("typeof(value) 类型异常：{other:?}"),
    }
}

/// 断言结果是 [`StorageError::Corrupt`]，返回错误上下文（便于继续断言 key 与实际类型）。
fn corrupt<T: std::fmt::Debug>(result: Result<T, StorageError>, label: &str) -> String {
    match result {
        Ok(value) => panic!("{label} 必须报损坏，实际 Ok({value:?})"),
        Err(StorageError::Corrupt { context }) => context,
        Err(other) => panic!("{label} 必须是 Corrupt，实际 {other:?}"),
    }
}

/// 同一 key 写成 TEXT：三个入口必须给出**同一个**损坏结论。
#[test]
fn text_meta_is_corrupt_from_all_three_entry_points() {
    let (_dir, vault) = vault_with_typed_meta();
    write_typed_meta(&vault, META_SYNC_LOG_ID, Value::Text("2f".repeat(16)));
    assert_eq!(
        value_type(&vault, META_SYNC_LOG_ID),
        "text",
        "构造出的值必须是 TEXT"
    );

    // 入口 1：Vault::get_meta（actor 线程）。旧行为把 TEXT 直接当字节返回。
    let from_actor = corrupt(vault.get_meta(META_SYNC_LOG_ID), "Vault::get_meta");
    // 入口 2：sync_bridge::read_meta（sync_cursor_state 读 sync.log_id）。旧行为当缺失。
    let from_bridge = corrupt(sync_cursor_state(&vault), "sync_bridge::read_meta");
    // 入口 3：sync_meta::read_meta_on（前缀推进在 worker 连接上读 sync.log_id）。
    let from_meta = corrupt(advance_integrated_prefix(&vault), "sync_meta::read_meta_on");

    for (label, context) in [
        ("Vault::get_meta", &from_actor),
        ("sync_bridge::read_meta", &from_bridge),
        ("sync_meta::read_meta_on", &from_meta),
    ] {
        assert!(
            context.contains(META_SYNC_LOG_ID),
            "{label} 的错误信息必须点名 key：{context}"
        );
        assert!(
            context.contains("TEXT") && context.contains("BLOB"),
            "{label} 的错误信息必须同时给出实际类型与期望类型：{context}"
        );
    }
    // 三处共用同一判定函数，连错误文案都不该有差别。
    assert_eq!(
        from_actor, from_bridge,
        "actor 与 sync_bridge 的结论必须一致"
    );
    assert_eq!(from_actor, from_meta, "actor 与 sync_meta 的结论必须一致");
}

/// BLOB（包括空 BLOB）继续按原语义读出，前缀推进不受影响。
#[test]
fn blob_meta_reads_consistently_from_all_three_entry_points() {
    let (_dir, vault) = fresh_vault();
    set_sync_log_id(&vault, &LOG_ID).expect("写 BLOB sync.log_id");

    assert_eq!(
        vault.get_meta(META_SYNC_LOG_ID).expect("读 BLOB meta"),
        Some(LOG_ID.to_vec()),
        "BLOB 必须逐字节读出"
    );
    let state = sync_cursor_state(&vault).expect("读游标状态");
    assert_eq!(
        state.log_id,
        Some(LOG_ID),
        "sync_bridge 必须读到同一个 BLOB"
    );
    assert_eq!(state.received_seq, 0);
    assert_eq!(state.integrated_seq, 0);
    assert_eq!(state.confirmed_highwater, None);

    advance_integrated_prefix(&vault).expect("BLOB 下前缀推进是合法 no-op");

    set_repack_required(&vault, true).expect("写 repack 屏障");
    assert!(
        repack_required(&vault).expect("读 repack 屏障"),
        "BLOB 屏障必须生效"
    );
    set_repack_required(&vault, false).expect("清 repack 屏障");
    assert!(!repack_required(&vault).expect("读 repack 屏障"));
}

/// 缺失的键在三个入口都是"没有"，不是损坏。
#[test]
fn missing_meta_reads_as_absent_from_all_three_entry_points() {
    let (_dir, vault) = fresh_vault();

    assert_eq!(vault.get_meta(META_SYNC_LOG_ID).expect("读缺失 key"), None);
    let state = sync_cursor_state(&vault).expect("读游标状态");
    assert_eq!(state.log_id, None);
    assert_eq!(state.received_seq, 0);
    assert_eq!(state.integrated_seq, 0);
    assert_eq!(state.confirmed_highwater, None);
    advance_integrated_prefix(&vault).expect("缺失时前缀推进是 no-op");
    assert!(!repack_required(&vault).expect("缺失屏障读成 false"));
    assert_eq!(
        vault
            .get_meta(META_SYNC_REPACK_REQUIRED)
            .expect("读缺失屏障"),
        None
    );
    assert!(
        take_inbox_ready(&vault, 8)
            .expect("没有 incarnation 时 ready 为空")
            .is_empty(),
        "缺失 sync.log_id 时 ready 必须为空（不回退成不过滤 log）"
    );
}

/// 损坏的 `sync.log_id` 不能被当成"还没绑定 incarnation"静默吞掉。
///
/// 这是 P2-6 的实际风险点：旧行为下 `take_inbox_ready` 会静默返回空（pull 永久卡住），
/// `set_sync_log_id` 会把损坏值覆盖成新值（悄悄换绑传代，旧行混进新代）。
#[test]
fn text_log_id_blocks_ready_and_rebinding_instead_of_silently_passing() {
    let (_dir, vault) = vault_with_typed_meta();
    write_typed_meta(&vault, META_SYNC_LOG_ID, Value::Text("2f".repeat(16)));

    let context = corrupt(take_inbox_ready(&vault, 8), "take_inbox_ready");
    assert!(
        context.contains(META_SYNC_LOG_ID),
        "错误必须点名 sync.log_id：{context}"
    );
    let context = corrupt(set_sync_log_id(&vault, &LOG_ID), "set_sync_log_id");
    assert!(
        context.contains(META_SYNC_LOG_ID),
        "错误必须点名 sync.log_id：{context}"
    );

    // 损坏值仍在库里：没有被"缺失 → 写入新版"的路径覆盖掉。
    assert_eq!(value_type(&vault, META_SYNC_LOG_ID), "text");
}

/// INTEGER / REAL / TEXT 全部按损坏处理（不是只有 TEXT 特殊）。
#[test]
fn non_blob_meta_types_are_corrupt() {
    let (_dir, vault) = vault_with_typed_meta();
    // NULL 造不出来：`meta.value` 是 NOT NULL，非 STRICT 表同样拒绝。
    for (value, expected_type) in [
        (Value::Integer(7), "INTEGER"),
        (Value::Real(1.5), "REAL"),
        (Value::Text("7".to_owned()), "TEXT"),
    ] {
        write_typed_meta(&vault, META_SYNC_LOG_ID, value);
        assert_eq!(
            value_type(&vault, META_SYNC_LOG_ID),
            expected_type.to_lowercase()
        );
        let context = corrupt(vault.get_meta(META_SYNC_LOG_ID), "Vault::get_meta");
        assert!(
            context.contains(expected_type),
            "错误必须报实际类型 {expected_type}：{context}"
        );
        let context = corrupt(sync_cursor_state(&vault), "sync_bridge::read_meta");
        assert!(
            context.contains(expected_type),
            "sync_bridge 也必须报实际类型 {expected_type}：{context}"
        );
    }
}

/// 屏障键本身：损坏的 `sync.repack_required` 不得读成"不需要 repack"。
#[test]
fn repack_barrier_is_not_silently_false_when_corrupt() {
    let (_dir, vault) = vault_with_typed_meta();
    write_typed_meta(
        &vault,
        META_SYNC_REPACK_REQUIRED,
        Value::Text("1".to_owned()),
    );

    let context = corrupt(repack_required(&vault), "repack_required");
    assert!(
        context.contains(META_SYNC_REPACK_REQUIRED),
        "错误必须点名屏障 key：{context}"
    );
}

`````



@@SECTION code/090 `crates/storage/tests/exec_transaction_busy.rs`


`````rust
//! P2-7：`exec_transaction` 用 `BEGIN IMMEDIATE` 开启事务（与批量 inbox 共用
//! `begin_immediate`）。
//!
//! 为什么必须是 Immediate：`exec_transaction` 承载 A 事务/资产/repack/finish，调用方
//! 习惯把写语句放在最前面，但**没有任何断言**保证这一点；只要事务里出现"先读后写"，
//! deferred 的锁升级就不走 `busy_timeout`（SQLite 的锁升级不调用 busy handler），会在
//! 预期之外立即 `SQLITE_BUSY`。
//!
//! 这些用例锁住外部可观测的后果：并发写者占着写锁时整笔事务失败、零半写、锁释放后
//! 同一批语句可重放且守卫照旧生效。

mod support;

use support::*;

use menghunote_storage::{AffectedGuard, Statement, StorageError, Value, Vault};

/// 断言错误是 `SQLITE_BUSY`（而不是守卫失败 / 约束冲突）。
fn assert_busy(error: &StorageError) {
    assert!(
        matches!(
            error,
            StorageError::Sqlite(rusqlite::Error::SqliteFailure(code, _))
                if code.code == rusqlite::ErrorCode::DatabaseBusy
        ),
        "并发写事务在场时必须整笔 SQLITE_BUSY，实际 {error:?}"
    );
}

/// 普通 `meta` 键的 BLOB 值（测试断言用）。
fn meta_blob(vault: &Vault, key: &str) -> Option<Vec<u8>> {
    vault.get_meta(key).expect("读 meta")
}

/// 外部写事务占着写锁时，`exec_transaction` 必须整笔 `SQLITE_BUSY` 且零半写；
/// 释放后同一批语句重放成功，语句顺序与守卫语义都不变。
#[test]
fn exec_transaction_under_foreign_write_lock_fails_busy_without_partial_write() {
    let (dir, vault) = fresh_vault();
    vault.set_meta("tx.cas", b"old").expect("预置 CAS 目标行");

    // 本连接不等锁（生产是 5s）：让 BEGIN IMMEDIATE 的失败立刻返回，测试无需 sleep。
    vault
        .exec_batch("PRAGMA busy_timeout=0")
        .expect("busy_timeout=0");

    // 第二个 Vault（= 第二个连接）先占住写锁。
    let holder = open_vault(&vault_path(&dir));
    holder
        .exec_batch("BEGIN IMMEDIATE")
        .expect("外部连接占写锁");

    // 第一条插入一行（半写的证据），第二条 CAS 更新既有行（守卫要求恰好 1 行）。
    let statements = [
        Statement::new(
            "INSERT INTO meta(key,value) VALUES(?1,?2)",
            vec![
                Value::Text("tx.half".to_owned()),
                Value::Blob(b"1".to_vec()),
            ],
        ),
        Statement::new(
            "UPDATE meta SET value=?2 WHERE key=?1",
            vec![
                Value::Text("tx.cas".to_owned()),
                Value::Blob(b"new".to_vec()),
            ],
        ),
    ];
    let guards = [AffectedGuard::exactly(0, 1), AffectedGuard::exactly(1, 1)];

    let error = vault
        .exec_transaction(&statements, &guards)
        .expect_err("写锁被占时必须整笔失败");
    assert_busy(&error);

    holder.exec_batch("ROLLBACK").expect("释放外部写锁");
    drop(holder);

    assert_eq!(meta_blob(&vault, "tx.half"), None, "失败事务不得留下半写");
    assert_eq!(
        meta_blob(&vault, "tx.cas"),
        Some(b"old".to_vec()),
        "失败事务不得改动 CAS 目标"
    );

    // 锁释放后同一批重放：整笔提交，每句影响行数与顺序都不变。
    let affected = vault
        .exec_transaction(&statements, &guards)
        .expect("锁释放后应成功");
    assert_eq!(affected, vec![1, 1], "语句顺序与每句影响行数必须保持原样");
    assert_eq!(meta_blob(&vault, "tx.half"), Some(b"1".to_vec()));
    assert_eq!(meta_blob(&vault, "tx.cas"), Some(b"new".to_vec()));
}

/// `exec_transaction` 在 `BEGIN` 阶段就取写锁（`BEGIN IMMEDIATE`），不是等第一条语句。
///
/// 有写语句时 deferred 也会在第一条语句撞上同样的 `SQLITE_BUSY`（`busy_timeout=0`），
/// 两种事务模式的结果无法区分；**空语句事务**是唯一能从外部观测到的差异：
///
/// - deferred：`BEGIN` 不碰写锁，`COMMIT` 空事务也成功 → `Ok(vec![])`；
/// - Immediate：`BEGIN` 就要写锁 → 占锁期间直接 `SQLITE_BUSY`。
///
/// 这也解释了为什么上面的"零半写"用例本身不足以证明模式切换。
#[test]
fn exec_transaction_takes_write_lock_at_begin() {
    let (dir, vault) = fresh_vault();
    vault
        .exec_batch("PRAGMA busy_timeout=0")
        .expect("busy_timeout=0");

    let holder = open_vault(&vault_path(&dir));
    holder
        .exec_batch("BEGIN IMMEDIATE")
        .expect("外部连接占写锁");

    let error = vault
        .exec_transaction(&[], &[])
        .expect_err("占锁期间 BEGIN IMMEDIATE 必须失败");
    assert_busy(&error);

    holder.exec_batch("ROLLBACK").expect("释放外部写锁");
    drop(holder);

    assert_eq!(
        vault
            .exec_transaction(&[], &[])
            .expect("释放后空事务应成功"),
        Vec::<usize>::new(),
        "空事务返回空的受影响行数列表"
    );
}

/// 占锁期间守卫错误也不该抢先暴露：整笔事务在 `BEGIN` 就失败，不会执行任何语句。
#[test]
fn exec_transaction_lock_failure_precedes_statement_effects() {
    let (dir, vault) = fresh_vault();
    vault.set_meta("tx.cas", b"old").expect("预置 CAS 目标行");
    vault
        .exec_batch("PRAGMA busy_timeout=0")
        .expect("busy_timeout=0");

    let holder = open_vault(&vault_path(&dir));
    holder
        .exec_batch("BEGIN IMMEDIATE")
        .expect("外部连接占写锁");

    // 守卫期望 0 行，但语句实际会影响 1 行：若事务能开始，会先写入再报 GuardFailed。
    let statements = [Statement::new(
        "UPDATE meta SET value=?2 WHERE key=?1",
        vec![
            Value::Text("tx.cas".to_owned()),
            Value::Blob(b"new".to_vec()),
        ],
    )];
    let guards = [AffectedGuard::exactly(0, 0)];
    let error = vault
        .exec_transaction(&statements, &guards)
        .expect_err("写锁被占时必须整笔失败");
    assert_busy(&error);

    holder.exec_batch("ROLLBACK").expect("释放外部写锁");
    drop(holder);

    assert_eq!(
        meta_blob(&vault, "tx.cas"),
        Some(b"old".to_vec()),
        "失败事务不得写入任何字节"
    );
}

`````



@@SECTION code/091 `crates/storage/tests/sync_intent.rs`


`````rust
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
    local_sync_intent, outbox_row, pending_encryption, BaselineExpectation, BodyDoc, DurableSink,
    JournalEntry, JournalOrigin, StorageError, StructureWriter, SyncContext, SyncIntent,
    SyncOutboxState, UpdateRecord, Value, Vault, VaultJournalSink, STRUCTURE_DOC_ID,
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
    begin_baseline_apply(
        &vault3,
        &[7; 16],
        3,
        "cursor-1",
        &BaselineExpectation::new(
            std::iter::once("doc-1".to_owned()),
            std::iter::empty::<[u8; 32]>(),
        ),
    )
    .expect("begin");
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

`````



@@SECTION code/094 `crates/sync/tests/engine.rs`


`````rust
//! S7-5 客户端同步引擎集成测试：FakeRelay + 真实 SQLite，不 sleep。
//!
//! 覆盖：出站加密/推送/重试字节、partial success、入站乱序/重放/篡改/缺依赖、
//! 本地 seq（禁止 sender_seq）、游标回退/log_id 变化/截断、ACK 语义。
//!
//! 借用说明：`SyncEngine` 持有 `&mut FakeRelay`，所以每次故障注入/检查 relay 状态前都
//! 先结束 engine 的作用域；测试里用 `{ let mut sync = ...; ... }` 分段。

mod support;

use support::*;

use menghunote_storage::{
    append_updates, asset_bytes, asset_content_hash, count_inbox_ready, create_note_slot,
    insert_asset, link_asset_ref, load_body_doc, load_head, outbox_parts, outbox_row,
    replace_asset_refs, store_inbox_verified_batch, sync_cursor_state, take_inbox_ready, BodyDoc,
    StorageError, SyncOutboxState, UpdateRecord, Vault, VerifiedInboxRow, APP_DOC_SCHEMA,
    CODEC_VERSION, META_SYNC_LOG_ID, MIN_READER,
};
use menghunote_sync::engine::{
    EngineConfig, EngineIdentity, IntegrateConfig, SyncEngine, RELAY_CURSOR_META,
};
use menghunote_sync::relay::{PauseReason, RelayLogKind};
use menghunote_sync::SyncError;
use menghunote_sync_proto::{
    cap_plain, encode_cde, part_count_for, CdeValue, Envelope, ObjectCommit, ObjectType,
    ProtoError, UpdateV1, MAX_ASSET_RAW_BYTES,
};
use sha2::{Digest, Sha256};

fn engine<'a>(
    vault: &'a Vault,
    relay: &'a mut FakeRelay,
    device: &Device,
) -> SyncEngine<'a, FakeRelay> {
    let identity = EngineIdentity {
        device_id: device.identity.device_id,
        signing_key: device.identity.signing_key.clone(),
        peer_id: device.identity.peer_id,
    };
    SyncEngine::new(vault, relay, identity, device.root.clone())
}

/// 指定 `EngineConfig` 的引擎（Phase 0 批量开关/页大小实验用）。
fn engine_with_config<'a>(
    vault: &'a Vault,
    relay: &'a mut FakeRelay,
    device: &Device,
    config: EngineConfig,
) -> SyncEngine<'a, FakeRelay> {
    let identity = EngineIdentity {
        device_id: device.identity.device_id,
        signing_key: device.identity.signing_key.clone(),
        peer_id: device.identity.peer_id,
    };
    SyncEngine::with_config(vault, relay, identity, device.root.clone(), config)
}

fn cursor(vault: &Vault) -> Option<String> {
    vault
        .get_meta(RELAY_CURSOR_META)
        .expect("读游标")
        .map(|bytes| String::from_utf8(bytes).expect("游标 UTF-8"))
}

fn received(vault: &Vault) -> u64 {
    sync_cursor_state(vault).expect("游标状态").received_seq
}

fn publish_from(relay: &mut FakeRelay, vault: &Vault, device: &Device) -> usize {
    let (encrypted, acked) = {
        let mut sync = engine(vault, relay, device);
        let encrypted = sync.encrypt_pending(10).expect("加密");
        let report = sync.push().expect("推送");
        (encrypted, report.acked)
    };
    assert_eq!(acked, encrypted);
    encrypted
}

// ============================================================ Phase 1A-2：正文聚合
//
// 这些用例直接在 inbox 里构造真实 `UpdateV1`（不走 relay 加密/拉取），这样能精确控制
// 依赖关系（缺哪一段 base_vv）与 relay_seq 顺序，且无需 sleep/网络。

/// Phase 1A-2 测试专用 log_id（与 FakeRelay 的默认 log_id 相同语义，独立命名便于断言）。
const INBOX_LOG_ID: [u8; 16] = [0x3b; 16];

/// 把源文档的一段真实增量包成 `UpdateV1`（inbox 明文）。
fn inbox_update(
    doc_id: &str,
    index: u64,
    base_vv: Vec<u8>,
    end_vv: Vec<u8>,
    end_frontier: Vec<u8>,
    loro_update_bytes: Vec<u8>,
) -> UpdateV1 {
    UpdateV1 {
        doc_id: doc_id.to_owned(),
        generation: 1,
        codec_version: CODEC_VERSION.to_owned(),
        app_doc_schema: u32::try_from(APP_DOC_SCHEMA).expect("app_doc_schema 在 u32 内"),
        min_reader: u32::try_from(MIN_READER).expect("min_reader 在 u32 内"),
        update_id: format!("{doc_id}#inbox-{index}"),
        batch_id: format!("inbox-batch-{index}"),
        origin_device_id: [0x91; 16],
        origin_seq: index,
        base_vv,
        end_vv,
        end_frontier,
        loro_update_bytes,
    }
}

/// 从空文档开始逐条插入文本，返回 `(每条更新的 inbox 明文, 最终正文)`。
///
/// 第 i 条的 `base_vv` 是插入前的 VV、`end_vv` 是插入后的 VV，因此这组 update 是严格
/// 因果连续的；调用方可以故意从 inbox 里抽掉中间某条来构造 deferred。
fn chained_updates(doc_id: &str, edits: &[String]) -> (Vec<UpdateV1>, String) {
    // 只构造 Loro 文档导出增量，不需要落库（BodyDoc::create 不碰 SQLite）。
    let source = BodyDoc::create(doc_id, 0x9001).expect("创建源文档");
    let mut base = loro::VersionVector::new();
    let mut updates = Vec::with_capacity(edits.len());
    for (index, edit) in edits.iter().enumerate() {
        let position = source.scalar_len();
        source.insert_scalars(position, edit).expect("源正文写入");
        let pending = source.pending_update(&base).expect("源增量");
        let end = source.vv();
        updates.push(inbox_update(
            doc_id,
            index as u64 + 1,
            base.encode(),
            end.encode(),
            source.frontiers().encode(),
            pending.payload,
        ));
        base = end;
    }
    (updates, source.text())
}

/// 把 `updates` 按给定顺序写入目标 vault 的 inbox（relay_seq = 下标 + 1）。
fn seed_inbox(vault: &Vault, updates: &[UpdateV1]) {
    let rows: Vec<VerifiedInboxRow> = updates
        .iter()
        .enumerate()
        .map(|(index, update)| VerifiedInboxRow {
            log_id: INBOX_LOG_ID,
            relay_seq: index as u64 + 1,
            object_id: [u8::try_from(index % 251).expect("index 在 u8 内"); 32],
            epoch: EPOCH,
            verified_payload: update.encode().expect("UpdateV1 编码"),
        })
        .collect();
    store_inbox_verified_batch(vault, &rows).expect("写 inbox");
}

/// inbox 里仍 verified 的 relay_seq（升序）。
fn verified_seqs(vault: &Vault) -> Vec<u64> {
    take_inbox_ready(vault, 4096)
        .expect("读 inbox ready")
        .into_iter()
        .map(|row| row.relay_seq)
        .collect()
}

#[test]
fn aggregate_body_loads_one_document_once_for_a_full_page() {
    ensure_test_identity_wrap();
    let (_dst_dir, vault_dst) = fresh_vault();
    let device = Device::new(0x7b);
    let edits: Vec<String> = (0..256).map(|index| format!("-{index:03}-")).collect();
    let (updates, expected_text) = chained_updates("doc-1", &edits);

    seed_doc(&vault_dst, "doc-1", device.identity.peer_id);
    seed_inbox(&vault_dst, &updates);

    let mut relay = FakeRelay::new(&[&device]);
    let config = IntegrateConfig::default();
    let outcome = {
        let mut sync = engine(&vault_dst, &mut relay, &device);
        sync.integrate_dependency_ordered(config).expect("聚合集成")
    };
    let report = outcome.report;
    assert_eq!(
        report.doc_loads, 1,
        "同一文档 256 行（一页）必须只 load_body_doc 一次（旧路径是 256 次）"
    );
    assert_eq!(report.integrated, 256);
    assert_eq!(report.body_integrated, 256);
    assert_eq!(report.structure_integrated, 0);
    assert_eq!(report.deferred, 0);
    assert_eq!(report.batches, 1, "256 行同一文档一批完成");
    assert_eq!(outcome.changed_heads, vec!["doc-1".to_owned()]);

    let (body, head) =
        load_body_doc(&vault_dst, "doc-1", device.identity.peer_id, None).expect("重放目标正文");
    assert_eq!(body.text(), expected_text, "聚合后的正文必须与源一致");
    assert_eq!(head.head_seq, 1, "256 条 remote 组合成一个本地 update");
    let cursor = sync_cursor_state(&vault_dst).expect("游标");
    assert_eq!(cursor.received_seq, 256);
    assert_eq!(cursor.integrated_seq, 256, "整批结束后统一推进一次前缀");
    assert_eq!(count_inbox_ready(&vault_dst).expect("剩余"), 0);
    assert!(verified_seqs(&vault_dst).is_empty());

    // 幂等重跑：没有新 import、head 不动、正文不变、doc_loads 不再增长。
    let again = {
        let mut sync = engine(&vault_dst, &mut relay, &device);
        sync.integrate_dependency_ordered(config)
            .expect("第二次集成")
    };
    assert_eq!(again.report.integrated, 0);
    assert_eq!(again.report.deferred, 0);
    assert_eq!(again.report.batches, 0);
    assert_eq!(again.report.doc_loads, 0);
    let (body_again, head_again) =
        load_body_doc(&vault_dst, "doc-1", device.identity.peer_id, None).expect("重放");
    assert_eq!(head_again.head_seq, 1, "重跑不得新增本地 seq");
    assert_eq!(body_again.text(), expected_text);
}

#[test]
fn aggregate_body_stops_at_missing_base_and_resumes_after_dependency_arrives() {
    ensure_test_identity_wrap();
    let (_dst_dir, vault_dst) = fresh_vault();
    let device = Device::new(0x7c);
    let edits: Vec<String> = (0..4).map(|index| format!("-{index}-")).collect();
    let (updates, expected_text) = chained_updates("doc-1", &edits);
    // inbox：u1、u3、u4（故意缺 u2）。u3 的 base 需要 u2，故只能先集成 u1。
    let inbox = vec![updates[0].clone(), updates[2].clone(), updates[3].clone()];

    seed_doc(&vault_dst, "doc-1", device.identity.peer_id);
    seed_inbox(&vault_dst, &inbox);
    let mut relay = FakeRelay::new(&[&device]);
    let config = IntegrateConfig::default();
    let first = {
        let mut sync = engine(&vault_dst, &mut relay, &device);
        sync.integrate_dependency_ordered(config)
            .expect("第一次集成")
    };
    assert_eq!(first.report.integrated, 1, "只集成因果前缀 u1");
    assert_eq!(first.report.body_integrated, 1);
    assert_eq!(first.report.deferred, 2);
    assert_eq!(
        verified_seqs(&vault_dst),
        vec![2, 3],
        "u3/u4 必须保持 durable verified（deferred 截断该 doc）"
    );
    assert_eq!(
        sync_cursor_state(&vault_dst).expect("游标").integrated_seq,
        1,
        "integrated 前缀不得跳过 deferred"
    );
    let (body, head) =
        load_body_doc(&vault_dst, "doc-1", device.identity.peer_id, None).expect("重放");
    assert_eq!(head.head_seq, 1);
    assert_eq!(body.text(), edits[0], "第一轮只应应用 u1");

    // "补齐依赖"：把缺失的 u2 直接 import 到本地文档（模拟 checkpoint/其它路径先补齐了
    // 这段历史），下一个 integrate 轮次必须从 deferred 处继续，不跳因果、不重复。
    {
        let (doc, head) =
            load_body_doc(&vault_dst, "doc-1", device.identity.peer_id, None).expect("重放");
        let before = doc.vv();
        doc.import(&updates[1].loro_update_bytes).expect("补齐 u2");
        let combined = doc.pending_update(&before).expect("u2 增量");
        append_updates(
            &vault_dst,
            "doc-1",
            head.head_seq,
            &[UpdateRecord::new("manual-missing-base", combined)],
        )
        .expect("落盘补齐的 u2");
    }
    let second = {
        let mut sync = engine(&vault_dst, &mut relay, &device);
        sync.integrate_dependency_ordered(config)
            .expect("第二次集成")
    };
    assert_eq!(second.report.integrated, 2, "依赖补齐后继续集成 u3/u4");
    assert_eq!(second.report.deferred, 0);
    assert_eq!(second.report.doc_loads, 1, "同一文档仍只全量重放一次");
    let (body, head) =
        load_body_doc(&vault_dst, "doc-1", device.identity.peer_id, None).expect("重放");
    assert_eq!(body.text(), expected_text, "无跳因果、无重复");
    assert_eq!(
        head.head_seq, 3,
        "u1 / 补齐的 u2 / 组合 u3+u4 各一个本地 seq"
    );
    assert_eq!(
        sync_cursor_state(&vault_dst).expect("游标").integrated_seq,
        3
    );
    assert_eq!(count_inbox_ready(&vault_dst).expect("剩余"), 0);
}

#[test]
fn aggregate_body_marks_covered_rows_and_imports_new_ones_in_one_transaction() {
    ensure_test_identity_wrap();
    let (_dst_dir, vault_dst) = fresh_vault();
    let device = Device::new(0x7d);
    let edits: Vec<String> = (0..2).map(|index| format!("-{index}-")).collect();
    let (updates, expected_text) = chained_updates("doc-1", &edits);

    // 目标库先应用 u1（模拟"自己产生的 update 从 relay 回环"或本地已有同 ops）。
    let target = seed_doc(&vault_dst, "doc-1", device.identity.peer_id);
    {
        let before = target.vv();
        target
            .import(&updates[0].loro_update_bytes)
            .expect("本地应用 u1");
        let combined = target.pending_update(&before).expect("u1 增量");
        append_updates(
            &vault_dst,
            "doc-1",
            0,
            &[UpdateRecord::new("local-u1", combined)],
        )
        .expect("本地落盘 u1");
    }
    let head_before = load_head(&vault_dst, "doc-1").expect("head").head_seq;
    assert_eq!(head_before, 1);
    // inbox：seq1 = u1（end_vv 已覆盖，只标记）、seq2 = u2（需要 import）。
    seed_inbox(&vault_dst, &updates);

    let mut relay = FakeRelay::new(&[&device]);
    let outcome = {
        let mut sync = engine(&vault_dst, &mut relay, &device);
        sync.integrate_dependency_ordered(IntegrateConfig::default())
            .expect("混合集成")
    };
    assert_eq!(outcome.report.doc_loads, 1);
    assert_eq!(outcome.report.integrated, 2, "已覆盖行也要标记");
    assert_eq!(outcome.report.body_integrated, 2);
    assert_eq!(outcome.report.deferred, 0);
    let (body, head) =
        load_body_doc(&vault_dst, "doc-1", device.identity.peer_id, None).expect("重放");
    assert_eq!(body.text(), expected_text, "不得重复应用 u1");
    assert_eq!(
        head.head_seq, 2,
        "已覆盖行只标记、新行 import：本地 seq 只前进一次"
    );
    assert_eq!(
        sync_cursor_state(&vault_dst).expect("游标").integrated_seq,
        2
    );
    assert_eq!(count_inbox_ready(&vault_dst).expect("剩余"), 0);
}

#[test]
fn aggregate_body_disabled_keeps_per_row_doc_loads() {
    ensure_test_identity_wrap();
    let (_dst_dir, vault_dst) = fresh_vault();
    let device = Device::new(0x7e);
    let edits: Vec<String> = (0..30).map(|index| format!("-{index}-")).collect();
    let (updates, expected_text) = chained_updates("doc-1", &edits);
    seed_doc(&vault_dst, "doc-1", device.identity.peer_id);
    seed_inbox(&vault_dst, &updates);

    let mut relay = FakeRelay::new(&[&device]);
    let legacy = IntegrateConfig {
        aggregate_body: false,
        ..IntegrateConfig::default()
    };
    let outcome = {
        let mut sync = engine(&vault_dst, &mut relay, &device);
        sync.integrate_dependency_ordered(legacy).expect("逐行集成")
    };
    assert_eq!(outcome.report.integrated, 30);
    assert_eq!(
        outcome.report.doc_loads, 30,
        "旧路径每条 update 都全量重放一次（基准对照）"
    );
    let (body, head) =
        load_body_doc(&vault_dst, "doc-1", device.identity.peer_id, None).expect("重放");
    assert_eq!(body.text(), expected_text);
    assert_eq!(head.head_seq, 30, "旧路径一条 update 一个本地 seq");
}

/// 多文档公平轮转：每批只放 5 篇文档（`batch_objects` 的聚合语义），4 批覆盖 20 篇；
/// 每篇文档仍然只全量重放一次，且没有文档被饿死。
#[test]
fn aggregate_body_rotates_across_documents_without_starving() {
    ensure_test_identity_wrap();
    let (_dst_dir, vault_dst) = fresh_vault();
    let device = Device::new(0x81);
    let docs: Vec<(String, Vec<UpdateV1>, String)> = (0..20)
        .map(|index| {
            let doc_id = format!("doc-{index:02}");
            let edits = vec![format!("-{index}-a"), format!("-{index}-b")];
            let (updates, text) = chained_updates(&doc_id, &edits);
            (doc_id, updates, text)
        })
        .collect();
    for (doc_id, _, _) in &docs {
        seed_doc(&vault_dst, doc_id, device.identity.peer_id);
    }
    // relay 日志按"轮次 × 文档"交错（不是按文档连续），确保聚合必须自己按 doc 分组。
    let mut inbox = Vec::new();
    for round in 0..2 {
        for (_, updates, _) in &docs {
            inbox.push(updates[round].clone());
        }
    }
    seed_inbox(&vault_dst, &inbox);

    let mut relay = FakeRelay::new(&[&device]);
    let config = IntegrateConfig {
        batch_objects: 5,
        max_batches: 8,
        ..IntegrateConfig::default()
    };
    let outcome = {
        let mut sync = engine(&vault_dst, &mut relay, &device);
        sync.integrate_dependency_ordered(config)
            .expect("多文档聚合集成")
    };
    assert_eq!(outcome.report.integrated, 40);
    assert_eq!(outcome.report.body_integrated, 40);
    assert_eq!(outcome.report.deferred, 0);
    assert_eq!(outcome.report.doc_loads, 20, "20 篇文档各自只全量重放一次");
    assert_eq!(outcome.report.batches, 4, "5 篇文档/批 × 4 批");
    assert_eq!(outcome.changed_heads.len(), 20, "每篇 head 只记一次");
    for (doc_id, _, expected_text) in &docs {
        let (body, head) =
            load_body_doc(&vault_dst, doc_id, device.identity.peer_id, None).expect("重放");
        assert_eq!(&body.text(), expected_text, "{doc_id} 正文必须一致");
        assert_eq!(head.head_seq, 1, "{doc_id} 两个 update 合成一个本地 seq");
    }
}

/// 把一组源 update 先在本地文档上应用，再合成**一条**本地 update 落盘。
///
/// 模拟"这些 ops 本地已经有了"（自己产生的 update 从 relay 回环 / checkpoint / 其它
/// 路径先导入）：目标库的 `head_vv` 会覆盖这些 update 的 `end_vv`。
fn apply_updates_locally(vault: &Vault, doc: &BodyDoc, updates: &[UpdateV1]) {
    let before = doc.vv();
    for update in updates {
        doc.import(&update.loro_update_bytes)
            .expect("本地应用 update");
    }
    let combined = doc.pending_update(&before).expect("组合增量");
    append_updates(
        vault,
        doc.doc_id(),
        0,
        &[UpdateRecord::new("local-precovered", combined)],
    )
    .expect("本地落盘");
}

/// Phase 1A 预检：本地 `head_vv` 已覆盖整组 `end_vv` 时只标记，不 `load_body_doc`、
/// 不 import、head 不前进。
#[test]
fn aggregate_body_precheck_marks_fully_covered_group_without_doc_load() {
    ensure_test_identity_wrap();
    let (_dst_dir, vault_dst) = fresh_vault();
    let device = Device::new(0x82);
    let edits: Vec<String> = (0..2).map(|index| format!("-covered-{index}-")).collect();
    let (updates, expected_text) = chained_updates("doc-1", &edits);

    // 目标库先应用这两条 update（模拟回环 / 其它路径已导入）。
    let target = seed_doc(&vault_dst, "doc-1", device.identity.peer_id);
    apply_updates_locally(&vault_dst, &target, &updates);
    assert_eq!(load_head(&vault_dst, "doc-1").expect("head").head_seq, 1);
    seed_inbox(&vault_dst, &updates);

    let mut relay = FakeRelay::new(&[&device]);
    let outcome = {
        let mut sync = engine(&vault_dst, &mut relay, &device);
        sync.integrate_dependency_ordered(IntegrateConfig::default())
            .expect("预检集成")
    };
    let report = outcome.report;
    assert_eq!(report.doc_loads, 0, "全部已覆盖不得 load_body_doc");
    assert_eq!(report.integrated, 2, "已覆盖行也必须标记 integrated");
    assert_eq!(report.body_integrated, 2);
    assert_eq!(report.deferred, 0);
    assert_eq!(report.batches, 1, "已覆盖组仍作为一个待处理批次交付");
    assert!(
        outcome.changed_heads.is_empty(),
        "没有 import，head 不得前进"
    );
    assert_eq!(
        load_head(&vault_dst, "doc-1").expect("head").head_seq,
        1,
        "只标记不写 update"
    );
    let (body, head) =
        load_body_doc(&vault_dst, "doc-1", device.identity.peer_id, None).expect("重放");
    assert_eq!(body.text(), expected_text, "不得重复应用已覆盖 update");
    assert_eq!(head.head_seq, 1);
    assert!(verified_seqs(&vault_dst).is_empty());
    assert_eq!(
        sync_cursor_state(&vault_dst).expect("游标").integrated_seq,
        2
    );
}

/// Phase 1A 预检：全部已覆盖组按 0 字节计，不占正文预算，不会把后面的就绪文档挤出
/// 本批（旧实现会先按 covered 行的 payload 字节占满 `batch_bytes`）。
#[test]
fn aggregate_body_precheck_covered_group_does_not_consume_batch_bytes() {
    ensure_test_identity_wrap();
    let (_dst_dir, vault_dst) = fresh_vault();
    let device = Device::new(0x83);
    // covered 文档：每条 24 KiB 文本，两行合计明显超过 32 KiB 预算。
    let covered_edits = vec!["C".repeat(24 * 1024), "D".repeat(24 * 1024)];
    let (covered_updates, covered_text) = chained_updates("doc-covered", &covered_edits);
    let ready_edits = vec!["-ready-a-".to_owned(), "-ready-b-".to_owned()];
    let (ready_updates, ready_text) = chained_updates("doc-ready", &ready_edits);

    let covered_doc = seed_doc(&vault_dst, "doc-covered", device.identity.peer_id);
    apply_updates_locally(&vault_dst, &covered_doc, &covered_updates);
    seed_doc(&vault_dst, "doc-ready", device.identity.peer_id);
    // relay 顺序：covered 文档在前（旧实现会先吃掉全部预算），就绪文档在后。
    let mut inbox = covered_updates.clone();
    inbox.extend(ready_updates);
    seed_inbox(&vault_dst, &inbox);

    let mut relay = FakeRelay::new(&[&device]);
    let config = IntegrateConfig {
        batch_bytes: 32 * 1024,
        ..IntegrateConfig::default()
    };
    let outcome = {
        let mut sync = engine(&vault_dst, &mut relay, &device);
        sync.integrate_dependency_ordered(config).expect("预检集成")
    };
    let report = outcome.report;
    assert_eq!(report.integrated, 4, "covered + ready 两篇都必须处理");
    assert_eq!(report.deferred, 0);
    assert_eq!(
        report.doc_loads, 1,
        "covered 文档不重放；就绪文档只重放一次"
    );
    assert_eq!(report.batches, 1, "covered 组 0 字节，就绪文档同一批完成");
    assert_eq!(outcome.changed_heads, vec!["doc-ready".to_owned()]);
    let (covered_body, covered_head) =
        load_body_doc(&vault_dst, "doc-covered", device.identity.peer_id, None)
            .expect("重放 covered");
    assert_eq!(covered_body.text(), covered_text);
    assert_eq!(covered_head.head_seq, 1, "covered 文档 head 不得前进");
    let (ready_body, ready_head) =
        load_body_doc(&vault_dst, "doc-ready", device.identity.peer_id, None).expect("重放 ready");
    assert_eq!(ready_body.text(), ready_text);
    assert_eq!(ready_head.head_seq, 1, "两条 ready update 合成一个本地 seq");
    assert!(verified_seqs(&vault_dst).is_empty());
}

/// Phase 1A 预检：纯 deferred 组不进入 selected、不消耗 `batch_bytes`；两个大 deferred
/// 文档不得饿死排在后面的就绪文档。
///
/// `max_batches = 2` 是故意卡紧的：旧实现前两批会被两个大 deferred 文档按字节占满
/// （每批都是 0 进展），排在最后的就绪文档整轮都选不到 → 饿死。
#[test]
fn aggregate_body_precheck_skips_large_deferred_docs_without_starving_ready_doc() {
    ensure_test_identity_wrap();
    let (_dst_dir, vault_dst) = fresh_vault();
    let device = Device::new(0x84);
    // 两篇 deferred：每篇 inbox 里放两条 update（都依赖永远缺失的第一条）→ base_vv 不满足。
    // 每行约 24 KiB，单篇两行就超过 32 KiB 预算：旧实现每批只装得下「一篇的第一行」。
    let (deferred_a, _) = chained_updates(
        "doc-deferred-a",
        &[
            "A".repeat(24 * 1024),
            "B".repeat(24 * 1024),
            "C".repeat(24 * 1024),
        ],
    );
    let (deferred_b, _) = chained_updates(
        "doc-deferred-b",
        &[
            "D".repeat(24 * 1024),
            "E".repeat(24 * 1024),
            "F".repeat(24 * 1024),
        ],
    );
    let ready_edits = vec!["-ready-0-".to_owned(), "-ready-1-".to_owned()];
    let (ready_updates, ready_text) = chained_updates("doc-ready", &ready_edits);

    for doc_id in ["doc-deferred-a", "doc-deferred-b", "doc-ready"] {
        seed_doc(&vault_dst, doc_id, device.identity.peer_id);
    }
    // relay 顺序：两个大 deferred 文档各占两行排在前，就绪文档在最后。
    seed_inbox(
        &vault_dst,
        &[
            deferred_a[1].clone(),
            deferred_a[2].clone(),
            deferred_b[1].clone(),
            deferred_b[2].clone(),
            ready_updates[0].clone(),
            ready_updates[1].clone(),
        ],
    );

    let mut relay = FakeRelay::new(&[&device]);
    let config = IntegrateConfig {
        batch_bytes: 32 * 1024,
        max_batches: 2,
        ..IntegrateConfig::default()
    };
    let outcome = {
        let mut sync = engine(&vault_dst, &mut relay, &device);
        sync.integrate_dependency_ordered(config).expect("预检集成")
    };
    let report = outcome.report;
    assert_eq!(report.integrated, 2, "只有就绪文档被集成");
    assert_eq!(report.body_integrated, 2);
    assert_eq!(report.deferred, 4, "两个大 deferred 文档的 4 行如实上报");
    assert_eq!(
        report.doc_loads, 1,
        "两篇纯 deferred 文档都不 load_body_doc"
    );
    assert_eq!(report.batches, 1, "大 deferred 不占预算，就绪文档同批完成");
    assert_eq!(outcome.changed_heads, vec!["doc-ready".to_owned()]);
    assert_eq!(
        verified_seqs(&vault_dst),
        vec![1, 2, 3, 4],
        "deferred 行保持 durable verified"
    );
    let (ready_body, ready_head) =
        load_body_doc(&vault_dst, "doc-ready", device.identity.peer_id, None).expect("重放 ready");
    assert_eq!(ready_body.text(), ready_text);
    assert_eq!(ready_head.head_seq, 1);
    for doc_id in ["doc-deferred-a", "doc-deferred-b"] {
        let (body, head) = load_body_doc(&vault_dst, doc_id, device.identity.peer_id, None)
            .expect("重放 deferred");
        assert_eq!(head.head_seq, 0, "{doc_id} 不得落盘");
        assert!(body.text().is_empty(), "{doc_id} 正文不得变化");
    }
    assert_eq!(count_inbox_ready(&vault_dst).expect("剩余"), 4);
}

/// Phase 1A 多轮合并：物理乱序（relay 先到的行 base 依赖后到的行）自愈；整组仍只
/// `load_body_doc` 一次、head 只前进一次。
#[test]
fn aggregate_body_multi_round_heals_physical_out_of_order() {
    ensure_test_identity_wrap();
    let (_dst_dir, vault_dst) = fresh_vault();
    let device = Device::new(0x85);
    let edits = vec!["-first-".to_owned(), "-second-".to_owned()];
    let (updates, expected_text) = chained_updates("doc-1", &edits);
    seed_doc(&vault_dst, "doc-1", device.identity.peer_id);
    // relay 顺序与因果顺序相反：seq=1 是后一步（base = u1），seq=2 才是它依赖的 u1。
    seed_inbox(&vault_dst, &[updates[1].clone(), updates[0].clone()]);

    let mut relay = FakeRelay::new(&[&device]);
    let outcome = {
        let mut sync = engine(&vault_dst, &mut relay, &device);
        sync.integrate_dependency_ordered(IntegrateConfig::default())
            .expect("多轮集成")
    };
    let report = outcome.report;
    assert_eq!(report.integrated, 2, "乱序两行都必须集成");
    assert_eq!(report.body_integrated, 2);
    assert_eq!(report.deferred, 0);
    assert_eq!(report.doc_loads, 1, "整组只全量重放一次");
    assert_eq!(report.batches, 1);
    assert_eq!(
        outcome.changed_heads,
        vec!["doc-1".to_owned()],
        "head 只前进一次（一条组合 update）"
    );
    let (body, head) =
        load_body_doc(&vault_dst, "doc-1", device.identity.peer_id, None).expect("重放");
    assert_eq!(body.text(), expected_text, "物理乱序不得丢正文");
    assert_eq!(head.head_seq, 1, "两条 update 合成一个本地 seq");
    assert!(
        verified_seqs(&vault_dst).is_empty(),
        "两行都必须标记 integrated"
    );
    assert_eq!(
        sync_cursor_state(&vault_dst).expect("游标").integrated_seq,
        2
    );
}

/// Phase 1A 多轮合并：同组里一行真缺外部依赖、另一行可满足；可满足的集成、缺依赖的
/// 留 deferred；补齐依赖后下一轮完成（不跳因果、不丢已成功行）。
#[test]
fn aggregate_body_multi_round_keeps_importable_rows_and_defers_missing_dependency() {
    ensure_test_identity_wrap();
    let (_dst_dir, vault_dst) = fresh_vault();
    let device = Device::new(0x86);
    let edits = vec!["-a-".to_owned(), "-b-".to_owned(), "-c-".to_owned()];
    let (updates, expected_text) = chained_updates("doc-1", &edits);
    seed_doc(&vault_dst, "doc-1", device.identity.peer_id);
    // relay 顺序：u3（需要 u2）在前，u1（无依赖）在后；u2 暂时缺失。
    seed_inbox(&vault_dst, &[updates[2].clone(), updates[0].clone()]);

    let mut relay = FakeRelay::new(&[&device]);
    let first = {
        let mut sync = engine(&vault_dst, &mut relay, &device);
        sync.integrate_dependency_ordered(IntegrateConfig::default())
            .expect("第一轮")
    };
    assert_eq!(first.report.integrated, 1, "只有 u1 可集成");
    assert_eq!(first.report.body_integrated, 1);
    assert_eq!(first.report.deferred, 1, "u3 真缺 u2");
    assert_eq!(first.report.doc_loads, 1, "整组只重放一次");
    assert_eq!(first.report.batches, 1);
    assert_eq!(first.changed_heads, vec!["doc-1".to_owned()]);
    assert_eq!(verified_seqs(&vault_dst), vec![1], "u3 那一行保持 verified");
    assert_eq!(
        sync_cursor_state(&vault_dst).expect("游标").integrated_seq,
        0,
        "deferred 的 seq=1 挡住前缀：不跳因果"
    );
    let (body, head) =
        load_body_doc(&vault_dst, "doc-1", device.identity.peer_id, None).expect("重放");
    assert_eq!(body.text(), edits[0], "第一轮只应用 u1");
    assert_eq!(head.head_seq, 1);

    // 补齐缺失的 u2（模拟 checkpoint / 其它路径先补到这段历史）。
    {
        let (doc, head) =
            load_body_doc(&vault_dst, "doc-1", device.identity.peer_id, None).expect("重放");
        let before = doc.vv();
        doc.import(&updates[1].loro_update_bytes).expect("补齐 u2");
        let combined = doc.pending_update(&before).expect("u2 增量");
        append_updates(
            &vault_dst,
            "doc-1",
            head.head_seq,
            &[UpdateRecord::new("manual-missing-dependency", combined)],
        )
        .expect("落盘补齐的 u2");
    }
    let second = {
        let mut sync = engine(&vault_dst, &mut relay, &device);
        sync.integrate_dependency_ordered(IntegrateConfig::default())
            .expect("第二轮")
    };
    assert_eq!(second.report.integrated, 1, "依赖补齐后 u3 集成");
    assert_eq!(second.report.deferred, 0);
    assert_eq!(second.report.doc_loads, 1, "仍只重放一次");
    assert_eq!(second.report.batches, 1);
    let (body, head) =
        load_body_doc(&vault_dst, "doc-1", device.identity.peer_id, None).expect("重放");
    assert_eq!(body.text(), expected_text, "无跳因果、无重复");
    assert_eq!(
        head.head_seq, 3,
        "第一轮的 u1 / 补齐的 u2 / 第二轮组合的 u3 各一个本地 seq"
    );
    assert!(verified_seqs(&vault_dst).is_empty());
    assert_eq!(
        sync_cursor_state(&vault_dst).expect("游标").integrated_seq,
        2
    );
}

#[test]
fn pull_flushes_verified_updates_before_pausing_on_checkpoint() {
    ensure_test_identity_wrap();
    let (_a_dir, vault_a) = fresh_vault();
    let (_b_dir, vault_b) = fresh_vault();
    let device_a = Device::new(0x7f);
    let device_b = Device::new(0x80);
    let mut relay = FakeRelay::new(&[&device_a, &device_b]);

    let doc_b = seed_doc(&vault_b, "doc-1", device_b.identity.peer_id);
    doc_b
        .insert_scalars(0, "flush-before-pause")
        .expect("B 正文");
    commit_with_intent(&vault_b, &doc_b, &uuid_like(0x7f01), [0x7f; 32], EPOCH);
    assert_eq!(publish_from(&mut relay, &vault_b, &device_b), 1);
    // update 之后跟一条 checkpoint：pull 必须先 flush update 再 Paused。
    relay.publish_event(RelayLogKind::Checkpoint, [0xaa; 32]);

    seed_doc(&vault_a, "doc-1", device_a.identity.peer_id);
    seed_cursor(&vault_a);

    let mut sync = engine(&vault_a, &mut relay, &device_a);
    for attempt in 0..2 {
        let error = sync.pull().expect_err("checkpoint 必须暂停");
        assert!(
            matches!(error, SyncError::Paused(PauseReason::UnsupportedEvent)),
            "第 {attempt} 次：实际 {error:?}"
        );
        assert_eq!(cursor(&vault_a).as_deref(), Some("c0"), "游标不得推进");
        assert_eq!(
            received(&vault_a),
            1,
            "第 {attempt} 次：update 已验证行必须 durable 且推进 received 前缀"
        );
        assert_eq!(
            verified_seqs(&vault_a),
            vec![1],
            "第 {attempt} 次：flush 后 inbox 行仍 verified，重试幂等"
        );
    }
    drop(sync);

    // 移走 checkpoint（模拟控制帧由 bootstrap 任务处理掉）后同一页可正常完成。
    relay.log.pop();
    relay.head_seq -= 1;
    {
        let mut sync = engine(&vault_a, &mut relay, &device_a);
        let pull = sync.pull().expect("checkpoint 移除后拉取");
        assert_eq!(pull.events, 1);
        assert_eq!(pull.skipped, 1, "已 durable 的 update 在重试页里跳过");
        assert_eq!(pull.stored, 0);
        assert_eq!(cursor(&vault_a).as_deref(), Some("c1"));
        let integrate = sync.integrate(16).expect("集成");
        assert_eq!(integrate.integrated, 1);
    }
    let (body, _) =
        load_body_doc(&vault_a, "doc-1", device_a.identity.peer_id, None).expect("重放");
    assert!(
        body.text().contains("flush-before-pause"),
        "{}",
        body.text()
    );
}

#[test]
fn outbound_encrypt_push_ack_roundtrip() {
    ensure_test_identity_wrap();
    let (_dir, vault) = fresh_vault();
    let device = Device::new(1);
    let mut relay = FakeRelay::new(&[&device]);
    let doc = seed_doc(&vault, "doc-1", device.identity.peer_id);
    doc.insert_scalars(0, "hello").expect("写正文");
    let object_id = [0x11; 32];
    let message_id = uuid_like(1);
    commit_with_intent(&vault, &doc, &message_id, object_id, EPOCH);

    {
        let mut sync = engine(&vault, &mut relay, &device);
        assert_eq!(sync.encrypt_pending(10).expect("加密"), 1);
        let parts = outbox_parts(&vault, &message_id, EPOCH).expect("parts");
        assert_eq!(parts.len(), 1);
        let envelope = Envelope::decode(&parts[0].envelope).expect("信封");
        assert_eq!(envelope.header.object_type, ObjectType::Update);
        assert_eq!(envelope.header.vault_id, VAULT_ID);
        assert_eq!(envelope.header.object_id, object_id);
        assert_eq!(envelope.header.key_epoch, EPOCH);
        assert_eq!(envelope.header.generation, 1);
        assert_eq!(envelope.header.sender_seq, 1, "sender_seq = origin_seq");
        assert_eq!(envelope.header.part_count, 1);
        assert_ne!(envelope.ciphertext, b"hello", "正文必须加密");

        let report = sync.push().expect("推送");
        assert_eq!(report.uploaded, 1);
        assert_eq!(report.acked, 1);
        assert_eq!(report.failed, 0);
        let ack = sync.ack().expect("ack");
        assert_eq!(ack.received_seq, 0);
    }
    assert_eq!(relay.log.len(), 1, "relay 收到一个 update 事件");
    assert!(outbox_row(&vault, &message_id).expect("row").is_none());
}

#[test]
fn push_retry_reuses_persisted_ciphertext() {
    ensure_test_identity_wrap();
    let (_dir, vault) = fresh_vault();
    let device = Device::new(2);
    let mut relay = FakeRelay::new(&[&device]);
    let doc = seed_doc(&vault, "doc-1", device.identity.peer_id);
    doc.insert_scalars(0, "retry me").expect("正文");
    let object_id = [0x21; 32];
    let message_id = uuid_like(2);
    commit_with_intent(&vault, &doc, &message_id, object_id, EPOCH);

    relay.fail_submit.insert(object_id, "EPOCH");
    {
        let mut sync = engine(&vault, &mut relay, &device);
        sync.encrypt_pending(10).expect("加密");
        let report = sync.push().expect("推送（部分失败）");
        assert_eq!(report.acked, 0);
        assert_eq!(report.failed, 1);
    }
    let first_parts = outbox_parts(&vault, &message_id, EPOCH).expect("parts");
    let pending = outbox_row(&vault, &message_id)
        .expect("outbox_row")
        .expect("仍在 outbox");
    assert_eq!(pending.state, SyncOutboxState::Sent);
    let first_upload = relay.put_history.clone();

    // 重试：不得重新加密（pending_encryption 为空），不得换 nonce。
    {
        let mut sync = engine(&vault, &mut relay, &device);
        assert_eq!(sync.encrypt_pending(10).expect("再加密"), 0);
        let parts_after = outbox_parts(&vault, &message_id, EPOCH).expect("parts");
        assert_eq!(parts_after, first_parts, "重试必须复用已持久化密文");
    }

    relay.fail_submit.clear();
    {
        let mut sync = engine(&vault, &mut relay, &device);
        assert_eq!(sync.push().expect("重试推送").acked, 1);
    }
    assert!(outbox_row(&vault, &message_id).expect("row").is_none());
    // 重试只重新提交、不重复上传（parts 已在 relay 上，字节未变）。
    assert_eq!(relay.put_history.len(), first_upload.len());
    // relay 收到的 commit 字节必须与本地持久化 parts 推导的一致（同一份密文）。
    let stored = relay.objects.get(&object_id).expect("relay 对象");
    assert_eq!(
        stored.commit.object_digest,
        pending.cipher_digest.expect("digest")
    );
}

#[test]
fn partial_success_keeps_remaining_outbox() {
    ensure_test_identity_wrap();
    let (_dir, vault) = fresh_vault();
    let device = Device::new(3);
    let mut relay = FakeRelay::new(&[&device]);
    let doc = seed_doc(&vault, "doc-1", device.identity.peer_id);
    doc.insert_scalars(0, "a").expect("a");
    commit_with_intent(&vault, &doc, &uuid_like(31), [0x31; 32], EPOCH);
    doc.insert_scalars(1, "b").expect("b");
    commit_with_intent(&vault, &doc, &uuid_like(32), [0x32; 32], EPOCH);

    relay.fail_submit.insert([0x32; 32], "EPOCH");
    {
        let mut sync = engine(&vault, &mut relay, &device);
        assert_eq!(sync.encrypt_pending(10).expect("加密"), 2);
        let report = sync.push().expect("推送");
        assert_eq!(report.acked, 1);
        assert_eq!(report.failed, 1);
    }
    let remaining = outbox_row(&vault, &uuid_like(32))
        .expect("outbox_row")
        .expect("失败项仍在 outbox");
    assert_eq!(remaining.state, SyncOutboxState::Sent);
    assert!(outbox_row(&vault, &uuid_like(31)).expect("ok").is_none());

    relay.fail_submit.clear();
    {
        let mut sync = engine(&vault, &mut relay, &device);
        assert_eq!(sync.push().expect("重试").acked, 1);
    }
    assert!(outbox_row(&vault, &uuid_like(32)).expect("row").is_none());
}

#[test]
fn inbound_pull_integrate_uses_local_seq_not_sender_seq() {
    ensure_test_identity_wrap();
    let (_a_dir, vault_a) = fresh_vault();
    let (_b_dir, vault_b) = fresh_vault();
    let device_a = Device::new(4);
    let device_b = Device::new(5);
    let mut relay = FakeRelay::new(&[&device_a, &device_b]);

    let doc_b = seed_doc(&vault_b, "doc-1", device_b.identity.peer_id);
    doc_b.insert_scalars(0, "from B").expect("B 正文");
    commit_with_intent(&vault_b, &doc_b, &uuid_like(51), [0x51; 32], EPOCH);
    assert_eq!(publish_from(&mut relay, &vault_b, &device_b), 1);

    let doc_a = seed_doc(&vault_a, "doc-1", device_a.identity.peer_id);
    doc_a.insert_scalars(0, "from A").expect("A 正文");
    commit_local(&vault_a, &doc_a);
    seed_cursor(&vault_a);

    {
        let mut sync = engine(&vault_a, &mut relay, &device_a);
        let pull = sync.pull().expect("拉取");
        assert_eq!(pull.events, 1);
        assert_eq!(pull.stored, 1);
        let integrate = sync.integrate(16).expect("集成");
        assert_eq!(integrate.integrated, 1);
        assert_eq!(integrate.deferred, 0);
        let ack = sync.ack().expect("ack");
        assert_eq!(ack.received_seq, 1);
        assert_eq!(ack.integrated_seq, 1);
    }

    // B 的 origin_seq 是 1，但 A 的本地 seq 必须是 head+1 = 2。
    let rows = local_update_rows(&vault_a, "doc-1");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1].0, 2);
    assert_eq!(rows[1].1, "doc-1#2");

    let (body, head) =
        load_body_doc(&vault_a, "doc-1", device_a.identity.peer_id, None).expect("重放");
    assert_eq!(head.head_seq, 2);
    let text = body.text();
    assert!(text.contains("from A"), "{text}");
    assert!(text.contains("from B"), "{text}");
}

#[test]
fn out_of_order_dependency_is_durable_then_integrated() {
    ensure_test_identity_wrap();
    let (_a_dir, vault_a) = fresh_vault();
    let (_b_dir, vault_b) = fresh_vault();
    let device_a = Device::new(6);
    let device_b = Device::new(7);
    let mut relay = FakeRelay::new(&[&device_a, &device_b]);

    let doc_b = seed_doc(&vault_b, "doc-1", device_b.identity.peer_id);
    doc_b.insert_scalars(0, "one").expect("one");
    commit_with_intent(&vault_b, &doc_b, &uuid_like(61), [0x61; 32], EPOCH);
    doc_b.insert_scalars(3, "-two").expect("two");
    commit_with_intent(&vault_b, &doc_b, &uuid_like(62), [0x62; 32], EPOCH);
    assert_eq!(publish_from(&mut relay, &vault_b, &device_b), 2);
    // relay 乱序送达：update2 先到（依赖 update1 未到）。
    relay.swap_log_order(0, 1);

    seed_doc(&vault_a, "doc-1", device_a.identity.peer_id);
    seed_cursor(&vault_a);
    {
        // 该用例模拟 relay 把同一文档的 update 乱序送达（依赖在更高 relay_seq）。
        // 聚合路径按 relay_seq 升序**截断**该文档（Phase 1A-2 的因果前缀语义），
        // 不会跳过 deferred 去集成后面的行；旧逐行路径保留给这种乱序历史兼容。
        // `with_object_budget(16)` 保持旧 `integrate(16)` 的"本次最多 1 批"语义。
        let legacy = IntegrateConfig {
            aggregate_body: false,
            ..IntegrateConfig::with_object_budget(16)
        };
        let mut sync = engine(&vault_a, &mut relay, &device_a);
        assert_eq!(sync.pull().expect("拉取").stored, 2);
        let first = sync
            .integrate_dependency_ordered(legacy)
            .expect("第一次集成")
            .report;
        assert_eq!(first.integrated, 1, "update2 依赖未齐只能先集成 update1");
        assert_eq!(first.deferred, 1);
        assert_eq!(received(&vault_a), 2, "verified 行已 durable");
        let second = sync
            .integrate_dependency_ordered(legacy)
            .expect("第二次集成")
            .report;
        assert_eq!(second.integrated, 1);
        assert_eq!(second.deferred, 0);
    }
    let (body, _head) =
        load_body_doc(&vault_a, "doc-1", device_a.identity.peer_id, None).expect("重放");
    let text = body.text();
    assert!(text.contains("one") && text.contains("-two"), "{text}");
}

#[test]
fn tampered_part_or_signature_rejected_without_cursor_advance() {
    ensure_test_identity_wrap();
    let (_a_dir, vault_a) = fresh_vault();
    let (_b_dir, vault_b) = fresh_vault();
    let device_a = Device::new(8);
    let device_b = Device::new(9);
    let mut relay = FakeRelay::new(&[&device_a, &device_b]);
    let doc_b = seed_doc(&vault_b, "doc-1", device_b.identity.peer_id);
    doc_b.insert_scalars(0, "tamper").expect("正文");
    commit_with_intent(&vault_b, &doc_b, &uuid_like(71), [0x71; 32], EPOCH);
    publish_from(&mut relay, &vault_b, &device_b);
    seed_doc(&vault_a, "doc-1", device_a.identity.peer_id);
    seed_cursor(&vault_a);

    // 篡改密文片：object_digest 不符，游标不动。
    relay.tamper_parts = true;
    {
        let mut sync = engine(&vault_a, &mut relay, &device_a);
        assert!(matches!(sync.pull(), Err(SyncError::Object(_))));
    }
    assert_eq!(cursor(&vault_a).as_deref(), Some("c0"));
    assert_eq!(received(&vault_a), 0);
    relay.tamper_parts = false;

    // 伪造签名：验签失败，游标不动。
    relay.tamper_signature = true;
    {
        let mut sync = engine(&vault_a, &mut relay, &device_a);
        assert!(matches!(sync.pull(), Err(SyncError::Signature)));
    }
    assert_eq!(cursor(&vault_a).as_deref(), Some("c0"));
    relay.tamper_signature = false;

    // 正常拉取后游标推进。
    {
        let mut sync = engine(&vault_a, &mut relay, &device_a);
        assert_eq!(sync.pull().expect("拉取").stored, 1);
    }
    assert_eq!(cursor(&vault_a).as_deref(), Some("c1"));
}

#[test]
fn replay_and_frozen_cursor_are_idempotent() {
    ensure_test_identity_wrap();
    let (_a_dir, vault_a) = fresh_vault();
    let (_b_dir, vault_b) = fresh_vault();
    let device_a = Device::new(10);
    let device_b = Device::new(11);
    let mut relay = FakeRelay::new(&[&device_a, &device_b]);
    let doc_b = seed_doc(&vault_b, "doc-1", device_b.identity.peer_id);
    doc_b.insert_scalars(0, "replay").expect("正文");
    commit_with_intent(&vault_b, &doc_b, &uuid_like(81), [0x81; 32], EPOCH);
    publish_from(&mut relay, &vault_b, &device_b);
    seed_doc(&vault_a, "doc-1", device_a.identity.peer_id);
    seed_cursor(&vault_a);

    relay.freeze_next_cursor = true;
    {
        let mut sync = engine(&vault_a, &mut relay, &device_a);
        let first = sync.pull().expect("拉取1");
        assert_eq!(first.stored, 1);
        assert_eq!(cursor(&vault_a).as_deref(), Some("c0"));
        // 第二次拉取同一条事件：已收过 → 跳过，不重复写 inbox；游标推进。
        let second = sync.pull().expect("拉取2");
        assert_eq!(second.stored, 0);
        assert_eq!(second.skipped, 1);
        assert_eq!(cursor(&vault_a).as_deref(), Some("c1"));
        assert_eq!(received(&vault_a), 1);
        let integrate = sync.integrate(16).expect("集成");
        assert_eq!(integrate.integrated, 1);
        // 第三次拉取：没有新事件。
        assert_eq!(sync.pull().expect("拉取3").events, 0);
    }
    assert_eq!(
        local_update_rows(&vault_a, "doc-1").len(),
        1,
        "不能重复集成"
    );
}

#[test]
fn rollback_log_id_change_and_cursor_expiry_pause() {
    ensure_test_identity_wrap();
    let (_a_dir, vault_a) = fresh_vault();
    let (_b_dir, vault_b) = fresh_vault();
    let device_a = Device::new(12);
    let device_b = Device::new(13);
    let mut relay = FakeRelay::new(&[&device_a, &device_b]);
    let doc_b = seed_doc(&vault_b, "doc-1", device_b.identity.peer_id);
    doc_b.insert_scalars(0, "rollback").expect("正文");
    commit_with_intent(&vault_b, &doc_b, &uuid_like(91), [0x91; 32], EPOCH);
    publish_from(&mut relay, &vault_b, &device_b);
    seed_doc(&vault_a, "doc-1", device_a.identity.peer_id);
    seed_cursor(&vault_a);

    {
        let mut sync = engine(&vault_a, &mut relay, &device_a);
        sync.pull().expect("首次拉取");
        sync.ack().expect("ack");
    }
    // 已确认高水位 = head_seq = 1；relay 回退 → ROLLBACK。
    relay.head_seq = 0;
    {
        let mut sync = engine(&vault_a, &mut relay, &device_a);
        assert!(matches!(
            sync.pull(),
            Err(SyncError::Paused(PauseReason::RollbackDetected))
        ));
    }
    relay.head_seq = 1;
    relay.log_id = [0x99; 16];
    {
        let mut sync = engine(&vault_a, &mut relay, &device_a);
        assert!(matches!(
            sync.pull(),
            Err(SyncError::Paused(PauseReason::LogIdChanged))
        ));
    }
    relay.log_id = [0x11; 16];
    relay.gc_floor = 5;
    {
        let mut sync = engine(&vault_a, &mut relay, &device_a);
        assert!(matches!(
            sync.pull(),
            Err(SyncError::Paused(PauseReason::CursorExpired))
        ));
    }
    relay.gc_floor = 0;
    relay.epoch = 2;
    {
        let mut sync = engine(&vault_a, &mut relay, &device_a);
        assert!(matches!(
            sync.pull(),
            Err(SyncError::Paused(PauseReason::EpochMismatch))
        ));
    }
    assert_eq!(cursor(&vault_a).as_deref(), Some("c1"));
}

#[test]
fn truncated_log_and_unsupported_event_keep_cursor() {
    ensure_test_identity_wrap();
    let (_a_dir, vault_a) = fresh_vault();
    let device_a = Device::new(14);
    let mut relay = FakeRelay::new(&[&device_a]);
    seed_doc(&vault_a, "doc-1", device_a.identity.peer_id);
    seed_cursor(&vault_a);

    relay.truncate_next_log = true;
    {
        let mut sync = engine(&vault_a, &mut relay, &device_a);
        assert!(matches!(sync.pull(), Err(SyncError::Transport(_))));
    }
    assert_eq!(cursor(&vault_a).as_deref(), Some("c0"));

    relay.publish_event(RelayLogKind::Checkpoint, [0xaa; 32]);
    {
        let mut sync = engine(&vault_a, &mut relay, &device_a);
        assert!(matches!(
            sync.pull(),
            Err(SyncError::Paused(PauseReason::UnsupportedEvent))
        ));
    }
    assert_eq!(cursor(&vault_a).as_deref(), Some("c0"));

    // 把日志清空后可以正常拉取（证明前面只是暂停，不是永久损坏）。
    relay.log.clear();
    relay.head_seq = 0;
    {
        let mut sync = engine(&vault_a, &mut relay, &device_a);
        assert_eq!(sync.pull().expect("恢复后拉取").events, 0);
    }
    assert_eq!(cursor(&vault_a).as_deref(), Some("c0"));
}

#[test]
fn wrong_root_missing_cert_and_missing_signature_fail() {
    ensure_test_identity_wrap();
    let (_a_dir, vault_a) = fresh_vault();
    let (_b_dir, vault_b) = fresh_vault();
    let device_a = Device::new(15);
    let device_b = Device::new(16);
    let mut relay = FakeRelay::new(&[&device_a, &device_b]);
    let doc_b = seed_doc(&vault_b, "doc-1", device_b.identity.peer_id);
    doc_b.insert_scalars(0, "keys").expect("正文");
    commit_with_intent(&vault_b, &doc_b, &uuid_like(101), [0xa1; 32], EPOCH);
    publish_from(&mut relay, &vault_b, &device_b);
    seed_doc(&vault_a, "doc-1", device_a.identity.peer_id);
    seed_cursor(&vault_a);

    // A 用错误 epoch 根（同 epoch、不同随机根）→ AEAD 解密失败，游标不动。
    let mut wrong_device = Device::new(15);
    wrong_device.root =
        menghunote_sync::keys::RootSecretV1::generate(VAULT_ID, EPOCH, &mut TestRandom::new(7777))
            .expect("错误根");
    {
        let mut sync = engine(&vault_a, &mut relay, &wrong_device);
        assert!(matches!(
            sync.pull(),
            Err(SyncError::Object("AEAD 解密失败"))
        ));
    }
    assert_eq!(cursor(&vault_a).as_deref(), Some("c0"));

    // 缺签名 → 暂停。
    let signature = relay.log[0].signature;
    relay.log[0].signature = None;
    {
        let mut sync = engine(&vault_a, &mut relay, &device_a);
        assert!(matches!(
            sync.pull(),
            Err(SyncError::Paused(PauseReason::MissingSignature))
        ));
    }
    relay.log[0].signature = signature;

    // 缺生产者证书 → 对象错误。
    let cert = relay.certs.remove(&device_b.device_id);
    {
        let mut sync = engine(&vault_a, &mut relay, &device_a);
        assert!(matches!(
            sync.pull(),
            Err(SyncError::Object("缺少生产者设备证书"))
        ));
    }
    if let Some(cert) = cert {
        relay.certs.insert(device_b.device_id, cert);
    }

    // 正常情况下可以拉取并集成。
    {
        let mut sync = engine(&vault_a, &mut relay, &device_a);
        assert_eq!(sync.pull().expect("拉取").stored, 1);
        assert_eq!(sync.integrate(16).expect("集成").integrated, 1);
    }
    assert_eq!(cursor(&vault_a).as_deref(), Some("c1"));
}

#[test]
fn integrate_defers_unknown_document() {
    ensure_test_identity_wrap();
    let (_a_dir, vault_a) = fresh_vault();
    let (_b_dir, vault_b) = fresh_vault();
    let device_a = Device::new(17);
    let device_b = Device::new(18);
    let mut relay = FakeRelay::new(&[&device_a, &device_b]);
    let doc_b = seed_doc(&vault_b, "doc-unknown", device_b.identity.peer_id);
    doc_b.insert_scalars(0, "unknown doc").expect("正文");
    commit_with_intent(&vault_b, &doc_b, &uuid_like(111), [0xb1; 32], EPOCH);
    publish_from(&mut relay, &vault_b, &device_b);
    seed_cursor(&vault_a);

    {
        let mut sync = engine(&vault_a, &mut relay, &device_a);
        sync.pull().expect("拉取");
        // ADR 0012 S2b：无进展只重试一批就停（不循环刷最早一批）。
        let outcome = sync
            .integrate_dependency_ordered(IntegrateConfig::default())
            .expect("集成");
        let report = outcome.report;
        assert_eq!(report.integrated, 0);
        assert_eq!(report.deferred, 1);
        // Phase 1A 预检收口后，未知文档在选批阶段就被认定为 deferred（没有 head_vv 可
        // 预检），不会再进 selected 触发"零进展重试一批"，所以 batches 从 2 变成 0。
        // 语义未变：行仍是 durable verified（deferred=1），只是少了一次必然零进展的批。
        assert_eq!(report.batches, 0, "纯 deferred 不再进入批次");
        assert_eq!(report.doc_loads, 0, "预检不 load_body_doc");
        assert!(!report.projected, "没有结构更新就不投影");
        assert_eq!(received(&vault_a), 1, "durable 保存但仍 verified");
    }
    // 建好本地文档行后可以继续集成。
    seed_doc(&vault_a, "doc-unknown", device_a.identity.peer_id);
    {
        let mut sync = engine(&vault_a, &mut relay, &device_a);
        assert_eq!(sync.integrate(16).expect("再次集成").integrated, 1);
    }
}

#[test]
fn sync_once_covers_all_phases() {
    ensure_test_identity_wrap();
    let (_dir, vault) = fresh_vault();
    let device = Device::new(19);
    let mut relay = FakeRelay::new(&[&device]);
    let doc = seed_doc(&vault, "doc-1", device.identity.peer_id);
    doc.insert_scalars(0, "all phases").expect("正文");
    commit_with_intent(&vault, &doc, &uuid_like(121), [0xc1; 32], EPOCH);
    seed_cursor(&vault);

    {
        let mut sync = engine(&vault, &mut relay, &device);
        let report = sync.sync_once().expect("完整同步");
        assert_eq!(report.encrypted, 1);
        assert_eq!(report.push.acked, 1);
        // 自己的 update 也会从 relay 拉回（禁止的是"再生成 outbox"，不是丢弃事件）；
        // 集成时 import 自己的字节是幂等的（pending 为空），游标照常推进。
        assert_eq!(report.pull.events, 1);
        assert_eq!(report.integrate.integrated, 1);
        assert_eq!(report.ack.received_seq, 1);
    }
    assert_eq!(relay.head_seq, 1, "push 后 relay 水位推进");
    assert_eq!(load_head(&vault, "doc-1").expect("head").head_seq, 1);
}

#[test]
fn integrate_guard_blocks_appends_and_a_later_round_finishes_the_job() {
    // ADR 0012 S1：取消/授权复核在首次外部事务边界（integrate 前）赢得竞争时，
    // 本轮**不得 append**；已推送的对象与已拉取的 inbox 仍是 durable 事实。
    ensure_test_identity_wrap();
    let (_dir, vault) = fresh_vault();
    let device = Device::new(31);
    let mut relay = FakeRelay::new(&[&device]);
    let doc = seed_doc(&vault, "doc-1", device.identity.peer_id);
    doc.insert_scalars(0, "guard").expect("正文");
    commit_with_intent(&vault, &doc, &uuid_like(131), [0xd1; 32], EPOCH);
    seed_cursor(&vault);
    let head_before = load_head(&vault, "doc-1").expect("head").head_seq;

    let mut calls = 0usize;
    {
        let mut sync = engine(&vault, &mut relay, &device);
        let mut cancel = || {
            calls += 1;
            Err(SyncError::CancelledBeforeIntegrate)
        };
        let error = sync
            .sync_once_with_guard(&mut cancel)
            .expect_err("取消赢时本轮必须中止");
        assert!(
            matches!(error, SyncError::CancelledBeforeIntegrate),
            "守卫错误必须原样上抛：{error:?}"
        );
    }
    assert_eq!(calls, 1, "守卫只在 integrate 之前调用一次");
    assert_eq!(
        load_head(&vault, "doc-1").expect("head").head_seq,
        head_before,
        "取消赢时不得追加任何本地行（head 必须停在同步前）"
    );
    assert_eq!(
        sync_cursor_state(&vault).expect("游标").integrated_seq,
        0,
        "没 integrate 就不能推进 integrated 水位"
    );
    assert_eq!(relay.head_seq, 1, "推送已经完成（不因取消丢已上传对象）");

    // 守卫放行后，同一份 durable 事实可以补完集成（没有重复 push、没有丢事件）。
    {
        let mut sync = engine(&vault, &mut relay, &device);
        let mut allow = || Ok(());
        let report = sync
            .sync_once_with_guard(&mut allow)
            .expect("守卫放行后必须成功");
        assert_eq!(
            report.integrate.integrated, 1,
            "上一轮拉到的 inbox 必须被集成"
        );
    }
    assert_eq!(load_head(&vault, "doc-1").expect("head").head_seq, 1);
    assert_eq!(
        sync_cursor_state(&vault).expect("游标").integrated_seq,
        1,
        "集成水位必须推进"
    );
}

#[test]
fn head_conflict_is_retryable_and_export_retries_without_data_loss() {
    ensure_test_identity_wrap();
    use menghunote_sync::storage_view;

    let (_dir, vault) = fresh_vault();
    let device = Device::new(21);
    let doc = seed_doc(&vault, "doc-1", device.identity.peer_id);
    doc.insert_scalars(0, "first").expect("first");
    commit_local(&vault, &doc);
    doc.insert_scalars(5, "-second").expect("second");
    commit_local(&vault, &doc);
    assert_eq!(load_head(&vault, "doc-1").expect("head").head_seq, 2);

    // 过期 head：必须报 HeadConflict（可重试），而不是返回半截导出。
    let stale = storage_view::export_at_head(&vault, "doc-1", 1).unwrap_err();
    assert!(matches!(
        stale,
        SyncError::HeadConflict {
            expected: 1,
            actual: 2,
            ..
        }
    ));
    assert!(stale.is_retryable(), "head CAS 冲突必须标记可重试");

    // 用当前 head 重试：完整导出，数据没丢。
    let export = storage_view::export_at_head(&vault, "doc-1", 2).expect("重试导出");
    assert_eq!(export.meta.head_seq, 2);
    assert_eq!(export.updates.len(), 2);
    assert_eq!(export.updates.last().map(|update| update.seq), Some(2));

    // `doc_updates_since` 是出站范围接口：after=1 只拿到第 2 条。
    let since = storage_view::updates_since(&vault, "doc-1", 1).expect("updates_since");
    assert_eq!(since.len(), 1);
    assert_eq!(since[0].seq, 2);

    // bootstrap 读取包合并且不丢行。
    let bootstrap = storage_view::bootstrap(&vault, "doc-1")
        .expect("bootstrap")
        .expect("文档存在");
    assert_eq!(bootstrap.meta.head_seq, 2);
    assert_eq!(bootstrap.updates_since_snapshot.len(), 2);

    // 直接映射 storage 的 HeadConflict 也保持同一可重试语义。
    let mapped = storage_view::map_storage_error(StorageError::HeadConflict {
        doc_id: "doc-1".to_owned(),
        expected: 1,
        actual: 2,
    });
    assert!(mapped.is_retryable());
    assert!(!storage_view::map_storage_error(StorageError::Corrupt {
        context: "bad".to_owned()
    })
    .is_retryable());
}

#[test]
fn genesis_materials_drive_full_fake_flow_with_restart() {
    ensure_test_identity_wrap();
    use menghunote_sync::genesis;

    let dir = temp_dir();
    let path = dir.path().join("vaults").join("e2e.sqlite3");
    let materials = genesis::build_materials(
        VAULT_ID,
        [0x21; 16],
        "pw",
        1_700_000_000,
        &mut TestRandom::new(0x00e2e),
    )
    .expect("genesis 材料");
    let signing = materials.signing_key.clone();
    let root = materials.root.clone();
    let peer = materials.peer_id_u64();
    let manager_pub = materials.manager_pub();

    let mut relay = FakeRelay::new(&[]);
    relay
        .certs
        .insert(materials.device_id, materials.certificate().clone());
    assert_eq!(relay.vault_id, VAULT_ID);

    let engine_identity = || EngineIdentity {
        device_id: [0x21; 16],
        signing_key: signing.clone(),
        peer_id: peer,
    };

    {
        let vault = Vault::open(&path).expect("打开 vault");
        seed_cursor(&vault);
        let doc = seed_doc(&vault, "note-1", peer);
        doc.insert_scalars(0, "genesis note 1").expect("正文");
        commit_with_intent(&vault, &doc, &uuid_like(201), [0x21; 32], EPOCH);

        let mut sync = engine(
            &vault,
            &mut relay,
            &Device {
                identity: EngineIdentity {
                    device_id: [0x21; 16],
                    signing_key: signing.clone(),
                    peer_id: peer,
                },
                root: root.clone(),
                hpke_key: materials.hpke_key.clone(),
                vault_id: VAULT_ID,
                device_id: [0x21; 16],
            },
        );
        let report = sync.sync_once().expect("第一轮同步");
        assert_eq!(report.encrypted, 1);
        assert_eq!(report.push.acked, 1);
        assert_eq!(report.pull.events, 1, "自己的 update 回环");
        assert_eq!(report.integrate.integrated, 1);
        assert_eq!(report.ack.received_seq, 1);
        assert_eq!(
            local_update_rows(&vault, "note-1").len(),
            1,
            "回环不得 echo"
        );
        assert_eq!(cursor(&vault).as_deref(), Some("c1"));
    }
    // 模拟重启：重新打开 Vault，用同一份游标续传。
    {
        let vault = Vault::open(&path).expect("重开 vault");
        let mut sync = SyncEngine::new(&vault, &mut relay, engine_identity(), root.clone());
        let pull = sync.pull().expect("续传拉取");
        assert_eq!(pull.events, 0, "重启后从 cursor 继续，不重复消费");
        assert_eq!(received(&vault), 1);
        // 重启后继续写并推送。
        let (doc, _head) = load_body_doc(&vault, "note-1", peer, None).expect("重放");
        doc.insert_scalars(14, "-after-restart").expect("正文");
        commit_with_intent(&vault, &doc, &uuid_like(202), [0x22; 32], EPOCH);
        let report = sync.sync_once().expect("第二轮同步");
        assert_eq!(report.encrypted, 1);
        assert_eq!(report.push.acked, 1);
        assert_eq!(report.pull.events, 1);
        assert_eq!(report.ack.received_seq, 2);
        assert_eq!(local_update_rows(&vault, "note-1").len(), 2);
    }

    // 管理公钥与证书链仍可校验（genesis 把 manager_pub pin 进身份）。
    materials.signed_cert.verify(&manager_pub).expect("证书链");
}

/// T01/T02/T03：freeze 后 kill/reopen，durable barrier 必须阻止旧 epoch 的 encrypt/push。
///
/// 屏障不依赖 RAM：`SyncEngine` 构造时从 rotation journal phase 与
/// `sync.repack_required` 重建，只有 journal 明确 Completed/Promoted 且屏障已清才解冻。
#[test]
fn freeze_barrier_survives_kill_and_blocks_old_epoch_encrypt_and_push() {
    ensure_test_identity_wrap();
    let (_dir, vault) = fresh_vault();
    let device = Device::new(0x61);
    let mut relay = FakeRelay::new(&[&device]);
    let doc = seed_doc(&vault, "doc-freeze", device.identity.peer_id);
    doc.insert_scalars(0, "frozen body").expect("正文");
    let message_id = uuid_like(0x71);
    let object_id: [u8; 32] = [0x71; 32];
    commit_with_intent(&vault, &doc, &message_id, object_id, EPOCH);

    // 阶段 A：冻结并落 journal；冻结前已把一条 intent 加密成 MNE1（旧写）。
    {
        let mut sync = engine(&vault, &mut relay, &device);
        assert_eq!(sync.encrypt_pending(10).expect("冻结前加密"), 1);
        sync.freeze_rotation();
        let journal = menghunote_sync::rotation_journal::RotationJournal::freeze(
            &vault, EPOCH, [0x71; 16], 3, [0x72; 32],
        )
        .expect("journal freeze");
        assert_eq!(
            journal.phase,
            menghunote_sync::rotation_journal::RotationPhase::Frozen
        );
        // 冻结中：同一引擎必须拒绝 push。
        assert!(matches!(
            sync.push(),
            Err(SyncError::Paused(PauseReason::RotationFrozen))
        ));
    }

    // 模拟 kill：journal 已 durable，engine 重建。
    let journal =
        menghunote_sync::rotation_journal::RotationJournal::load(&vault).expect("journal load");
    assert_eq!(
        journal.expect("journal").phase,
        menghunote_sync::rotation_journal::RotationPhase::Frozen
    );
    let mut restarted = engine(&vault, &mut relay, &device);
    // 旧写（已加密行）不得在重启后解冻；新 pending 也不得再加密。
    assert!(
        matches!(
            restarted.push(),
            Err(SyncError::Paused(PauseReason::RotationFrozen))
        ),
        "重启后旧 epoch push 仍必须被冻结屏障拒绝"
    );
    let new_doc = seed_doc(&vault, "doc-freeze-2", device.identity.peer_id);
    new_doc.insert_scalars(0, "local edit").expect("正文");
    commit_with_intent(&vault, &new_doc, &uuid_like(0x72), [0x72; 32], EPOCH);
    assert!(
        matches!(
            restarted.encrypt_pending(10),
            Err(SyncError::Paused(PauseReason::RotationFrozen))
        ),
        "重启后新 pending 也不得被加密成旧 epoch 密文"
    );
    // 本地 A 事务编辑仍然可见（冻结不阻断本地写）。
    let (local, _) =
        load_body_doc(&vault, "doc-freeze-2", device.identity.peer_id, None).expect("本地文档");
    assert_eq!(local.text(), "local edit");
}

/// 评审 0003 P1-3：I 阶段 apply phase 本身就是持久屏障。
///
/// 只有 `applied` 解冻；`applying`/`paused` 等（含 merge 后、promotion 前崩溃留下的
/// `applying`）在重启后必须继续冻结旧 epoch 的 encrypt/push。
#[test]
fn apply_phase_alone_freezes_until_applied() {
    ensure_test_identity_wrap();
    let (_dir, vault) = fresh_vault();
    let device = Device::new(0x63);
    let mut relay = FakeRelay::new(&[&device]);

    vault
        .set_meta("sync.rotation_apply_phase", b"applying")
        .expect("phase");
    {
        let mut sync = engine(&vault, &mut relay, &device);
        assert!(sync.is_rotation_frozen(), "applying phase 必须冻结");
        assert!(matches!(
            sync.encrypt_pending(1),
            Err(SyncError::Paused(PauseReason::RotationFrozen))
        ));
    }

    vault
        .set_meta("sync.rotation_apply_phase", b"applied")
        .expect("phase");
    let sync = engine(&vault, &mut relay, &device);
    assert!(
        !sync.is_rotation_frozen(),
        "applied 且无 repack/journal 时不得冻结"
    );

    // 未知值 fail-closed（重启后仍冻结）。
    drop(sync);
    vault
        .set_meta("sync.rotation_apply_phase", b"garbage")
        .expect("phase");
    let sync = engine(&vault, &mut relay, &device);
    assert!(sync.is_rotation_frozen(), "未知 phase 必须 fail-closed");
}

// ============================================================ 评审 0003 P0-B 残余
// checkpoint 的 `ConsumedAsset` 推断已删除：ASSET 必须由 pull 在推进游标前先**真实下载
// 并验证**，再落 durable skipped-asset 记录；重复 seq 幂等、冲突 fail-closed。
// 伪造/损坏的 ASSET（缺 parts、错 OID/digest/长度/AEAD、明文头不自洽）必须让 pull
// 返回 Err，且不写记录、不推游标。

const ASSET_DATA: &[u8] = b"engine verified asset payload";

fn skipped_asset_rows(vault: &Vault) -> Vec<(String, Vec<u8>)> {
    vault
        .query(
            "SELECT key,value FROM meta WHERE substr(key,1,?1)=?2 ORDER BY key",
            &[
                menghunote_storage::Value::Integer("sync.skipped_asset.".len() as i64),
                menghunote_storage::Value::Text("sync.skipped_asset.".to_owned()),
            ],
        )
        .expect("读 skipped-asset meta")
        .iter()
        .map(|row| {
            let key = match row.first() {
                Some(menghunote_storage::Value::Text(key)) => key.clone(),
                _ => panic!("skipped-asset key 类型非法"),
            };
            let value = match row.get(1) {
                Some(menghunote_storage::Value::Blob(value)) => value.clone(),
                _ => panic!("skipped-asset value 类型非法"),
            };
            (key, value)
        })
        .collect()
}

/// 真实 canonical ASSET 夹具（parts + 日志事件都由 FakeRelay 持有）。
fn publish_device_asset(
    relay: &mut FakeRelay,
    device: &Device,
    data: &[u8],
    media_type: &str,
) -> ([u8; 32], u64) {
    publish_real_asset(
        relay,
        device.vault_id,
        device.device_id,
        &device.identity.signing_key,
        &device.root,
        data,
        media_type,
    )
}

/// 失败的 pull 必须零副作用：不写 skipped-asset 记录、游标/received 水位不前进。
fn assert_asset_pull_rejected(vault: &Vault, relay: &mut FakeRelay, device: &Device) {
    let before_cursor = cursor(vault);
    let before_received = received(vault);
    {
        let mut sync = engine(vault, relay, device);
        assert!(
            sync.pull().is_err(),
            "伪造/损坏的 ASSET 必须让 pull 返回 Err"
        );
    }
    assert_eq!(
        cursor(vault),
        before_cursor,
        "ASSET 验证失败不得推进 relay 游标"
    );
    assert_eq!(
        received(vault),
        before_received,
        "ASSET 验证失败不得推进 received 水位"
    );
    assert!(
        skipped_asset_rows(vault).is_empty(),
        "ASSET 验证失败不得写 skipped-asset 记录"
    );
}

/// 准备一个已绑定 log_id/游标的空库（避免 pull 在失败前写 log_id 干扰零副作用断言）。
fn seed_asset_pull_fixture(vault: &Vault, relay: &FakeRelay) {
    vault
        .set_meta(META_SYNC_LOG_ID, &relay.log_id)
        .expect("写 log_id");
    seed_cursor(vault);
}

#[test]
fn pull_verifies_real_asset_before_persisting_skipped_record() {
    ensure_test_identity_wrap();
    let (_dir, vault) = fresh_vault();
    let device = Device::new(0x64);
    let mut relay = FakeRelay::new(&[&device]);
    let (object_id, seq) = publish_device_asset(&mut relay, &device, ASSET_DATA, "image/png");
    assert_eq!(seq, 1);
    seed_asset_pull_fixture(&vault, &relay);

    // 冻结游标：模拟 relay 回显旧 token。记录必须已 durable，但游标仍停在 c0。
    relay.freeze_next_cursor = true;
    {
        let mut sync = engine(&vault, &mut relay, &device);
        let first = sync.pull().expect("真实 ASSET 必须通过来源认证");
        assert_eq!(first.events, 1);
        assert_eq!(first.skipped, 1);
        assert_eq!(
            cursor(&vault).as_deref(),
            Some("c0"),
            "游标不得被冻结页推进"
        );
    }
    let rows = skipped_asset_rows(&vault);
    assert_eq!(rows.len(), 1, "ASSET 消费必须先落 durable 记录");
    assert!(
        rows[0]
            .0
            .contains(&menghunote_sync::base64url::encode(&relay.log_id)),
        "记录 key 必须绑定 log_id: {}",
        rows[0].0
    );
    assert!(
        rows[0].0.ends_with("00000000000000000001"),
        "记录 key 必须绑定 relay_seq: {}",
        rows[0].0
    );
    assert!(
        rows[0].1.windows(32).any(|window| window == object_id),
        "记录值必须包含 object_id"
    );
    let real_sha: [u8; 32] = Sha256::digest(ASSET_DATA).into();
    assert!(
        rows[0].1.windows(32).any(|window| window == real_sha),
        "记录值必须包含本机验证过的 real_sha256"
    );
    assert!(
        rows[0]
            .1
            .windows("image/png".len())
            .any(|window| window == b"image/png"),
        "记录值必须包含本机验证过的 MIME"
    );

    // 同一 seq 重放（游标仍是 c0）：幂等，不新增/不改写记录。
    {
        let mut sync = engine(&vault, &mut relay, &device);
        assert_eq!(sync.pull().expect("重放").skipped, 1);
    }
    assert_eq!(skipped_asset_rows(&vault), rows, "重复 seq 必须幂等");
    assert_eq!(cursor(&vault).as_deref(), Some("c1"));
}

/// 多片 ASSET：必须逐片拉取/验证，不能只看第 0 片。
#[test]
fn pull_verifies_multipart_asset() {
    ensure_test_identity_wrap();
    let (_dir, vault) = fresh_vault();
    let device = Device::new(0x6e);
    let mut relay = FakeRelay::new(&[&device]);
    let cap = menghunote_sync_proto::cap_plain(ObjectType::Asset) as usize;
    let data = vec![0x5a; cap + 17];
    let (object_id, _) =
        publish_device_asset(&mut relay, &device, &data, "application/octet-stream");
    seed_asset_pull_fixture(&vault, &relay);
    {
        let mut sync = engine(&vault, &mut relay, &device);
        let report = sync.pull().expect("多片 ASSET 必须逐片拉取并验证");
        assert_eq!(report.skipped, 1);
    }
    assert_eq!(cursor(&vault).as_deref(), Some("c1"));
    let rows = skipped_asset_rows(&vault);
    assert_eq!(rows.len(), 1);
    assert!(rows[0].1.windows(32).any(|window| window == object_id));
}

/// relay 重写已消费的 ASSET seq：HMAC 通过的 durable 记录与重发事件冲突时必须
/// `Paused(RelayHistoryRewritten)`，不得报成本地损坏，也不得覆盖原记录/推进游标。
#[test]
fn pull_pauses_when_relay_rewrites_consumed_asset_seq() {
    ensure_test_identity_wrap();
    let (_dir, vault) = fresh_vault();
    let device = Device::new(0x70);
    let mut relay = FakeRelay::new(&[&device]);
    let (first_object, first_seq) =
        publish_device_asset(&mut relay, &device, ASSET_DATA, "text/plain");
    assert_eq!(first_seq, 1);
    seed_asset_pull_fixture(&vault, &relay);

    // 第一次 pull：冻结游标，让记录 durable 而 cursor 停在 c0；否则第二次 pull 从
    // c1 开始，看不到被重写的 seq=1。
    relay.freeze_next_cursor = true;
    {
        let mut sync = engine(&vault, &mut relay, &device);
        assert_eq!(
            sync.pull().expect("真实 ASSET 必须落 durable 记录").skipped,
            1
        );
    }
    let records = skipped_asset_rows(&vault);
    assert_eq!(records.len(), 1, "第一次 pull 必须已落 durable 记录");
    assert_eq!(cursor(&vault).as_deref(), Some("c0"));

    // 另一条真实 ASSET（不同 OID/密文），把它的日志事件重编号到 seq=1：
    // relay 宣称"同一个 seq 当时就是另一条资产"。
    let rewritten_data = b"rewritten asset payload";
    let (rewritten_object, _) =
        publish_device_asset(&mut relay, &device, rewritten_data, "image/png");
    assert_ne!(rewritten_object, first_object);
    let mut rewritten = relay.log.last().expect("新资产事件").clone();
    rewritten.relay_seq = 1;
    relay.log = vec![rewritten];
    relay.head_seq = 1;

    let before_received = received(&vault);
    let error = {
        let mut sync = engine(&vault, &mut relay, &device);
        sync.pull().expect_err("relay 改写已消费 seq 必须暂停")
    };
    assert!(
        matches!(error, SyncError::Paused(PauseReason::RelayHistoryRewritten)),
        "必须是 Paused(RelayHistoryRewritten)，实际 {error:?}"
    );
    assert_eq!(cursor(&vault).as_deref(), Some("c0"), "暂停不得推进游标");
    assert_eq!(
        received(&vault),
        before_received,
        "暂停不得推进 received 水位"
    );
    assert_eq!(
        skipped_asset_rows(&vault),
        records,
        "原 durable 记录不得被覆盖"
    );
}

/// 缺 parts / `part()` 报错：事件声明了一个 relay 上没有密文的对象。
#[test]
fn pull_rejects_asset_without_parts() {
    ensure_test_identity_wrap();
    let (_dir, vault) = fresh_vault();
    let device = Device::new(0x65);
    let mut relay = FakeRelay::new(&[&device]);
    relay.publish_signed_asset(&device.identity.signing_key, [0x5e; 32]);
    seed_asset_pull_fixture(&vault, &relay);
    assert_asset_pull_rejected(&vault, &mut relay, &device);
}

/// 错误 OID：事件 object_id/commit 自洽，但指向不存在的密文对象。
#[test]
fn pull_rejects_asset_with_wrong_object_id() {
    ensure_test_identity_wrap();
    let (_dir, vault) = fresh_vault();
    let device = Device::new(0x66);
    let mut relay = FakeRelay::new(&[&device]);
    publish_device_asset(&mut relay, &device, ASSET_DATA, "text/plain");
    let fake_oid = [0x9a; 32];
    tamper_last_asset_commit(&mut relay, |commit| commit.object_id = fake_oid);
    relay.log.last_mut().expect("asset event").object_id = Some(fake_oid);
    seed_asset_pull_fixture(&vault, &relay);
    assert_asset_pull_rejected(&vault, &mut relay, &device);
}

/// 错误 object_digest：密文真实存在，但事件摘要与 parts 不符。
#[test]
fn pull_rejects_asset_with_wrong_object_digest() {
    ensure_test_identity_wrap();
    let (_dir, vault) = fresh_vault();
    let device = Device::new(0x67);
    let mut relay = FakeRelay::new(&[&device]);
    publish_device_asset(&mut relay, &device, ASSET_DATA, "text/plain");
    tamper_last_asset_commit(&mut relay, |commit| commit.object_digest[0] ^= 0x01);
    seed_asset_pull_fixture(&vault, &relay);
    assert_asset_pull_rejected(&vault, &mut relay, &device);
}

/// 错误 total_len：事件总长与固定切片规则/实际明文不符。
#[test]
fn pull_rejects_asset_with_wrong_total_len() {
    ensure_test_identity_wrap();
    let (_dir, vault) = fresh_vault();
    let device = Device::new(0x68);
    let mut relay = FakeRelay::new(&[&device]);
    publish_device_asset(&mut relay, &device, ASSET_DATA, "text/plain");
    tamper_last_asset_commit(&mut relay, |commit| commit.total_len += 1);
    seed_asset_pull_fixture(&vault, &relay);
    assert_asset_pull_rejected(&vault, &mut relay, &device);
}

/// 错误 part_count：声明 2 片，但 relay 上没有对应 parts（/ 总长也不符）。
#[test]
fn pull_rejects_asset_with_wrong_part_count() {
    ensure_test_identity_wrap();
    let (_dir, vault) = fresh_vault();
    let device = Device::new(0x69);
    let mut relay = FakeRelay::new(&[&device]);
    publish_device_asset(&mut relay, &device, ASSET_DATA, "text/plain");
    // 固定切片规则下只要总长跨过单片容量，part_count 就必须是 2（ObjectCommit
    // 编码会拒绝不自洽的声明）；relay 上实际只有 1 片 → 取第 1 片失败。
    let total_len = menghunote_sync_proto::cap_plain(ObjectType::Asset) + 1;
    tamper_last_asset_commit(&mut relay, |commit| {
        commit.total_len = total_len;
        commit.part_count = 2;
    });
    seed_asset_pull_fixture(&vault, &relay);
    assert_asset_pull_rejected(&vault, &mut relay, &device);
}

/// AEAD 密文翻 bit：信封仍可解码，但解密必须失败。
#[test]
fn pull_rejects_asset_with_corrupted_ciphertext() {
    ensure_test_identity_wrap();
    let (_dir, vault) = fresh_vault();
    let device = Device::new(0x6a);
    let mut relay = FakeRelay::new(&[&device]);
    let (object_id, _) = publish_device_asset(&mut relay, &device, ASSET_DATA, "text/plain");
    let part = relay.parts.get_mut(&(object_id, 0)).expect("ASSET part");
    // 跳过 196B MNE1 header，翻密文区第一个字节。
    let offset = 196;
    part[offset] ^= 0x01;
    seed_asset_pull_fixture(&vault, &relay);
    assert_asset_pull_rejected(&vault, &mut relay, &device);
}

/// 明文头 MIME 与 object_id 不对应：AEAD 可解，但 canonical OID 重算不一致。
#[test]
fn pull_rejects_asset_plaintext_mime_not_matching_object_id() {
    ensure_test_identity_wrap();
    let (_dir, vault) = fresh_vault();
    let device = Device::new(0x6b);
    let mut relay = FakeRelay::new(&[&device]);
    let real: [u8; 32] = Sha256::digest(ASSET_DATA).into();
    let byte_len = ASSET_DATA.len() as u64;
    // object_id 按 text/plain 派生，明文头却写 image/png。
    let object_id =
        menghunote_sync::checkpoint::asset_object_id(&device.root, &real, byte_len, "text/plain")
            .expect("canonical oid");
    let plaintext = asset_plaintext_bytes(&real, byte_len, "image/png", ASSET_DATA);
    upload_asset_plaintext(
        &mut relay,
        device.vault_id,
        device.device_id,
        &device.identity.signing_key,
        &device.root,
        object_id,
        &plaintext,
    );
    seed_asset_pull_fixture(&vault, &relay);
    assert_asset_pull_rejected(&vault, &mut relay, &device);
}

/// 明文 data 长度与头声明不符。
#[test]
fn pull_rejects_asset_plaintext_data_len_mismatch() {
    ensure_test_identity_wrap();
    let (_dir, vault) = fresh_vault();
    let device = Device::new(0x6c);
    let mut relay = FakeRelay::new(&[&device]);
    let real: [u8; 32] = Sha256::digest(ASSET_DATA).into();
    let plaintext =
        asset_plaintext_bytes(&real, ASSET_DATA.len() as u64 + 1, "text/plain", ASSET_DATA);
    upload_asset_plaintext(
        &mut relay,
        device.vault_id,
        device.device_id,
        &device.identity.signing_key,
        &device.root,
        [0x77; 32],
        &plaintext,
    );
    seed_asset_pull_fixture(&vault, &relay);
    assert_asset_pull_rejected(&vault, &mut relay, &device);
}

/// 明文 data 哈希与头声明不符。
#[test]
fn pull_rejects_asset_plaintext_data_hash_mismatch() {
    ensure_test_identity_wrap();
    let (_dir, vault) = fresh_vault();
    let device = Device::new(0x6d);
    let mut relay = FakeRelay::new(&[&device]);
    let plaintext = asset_plaintext_bytes(
        &[0x11; 32],
        ASSET_DATA.len() as u64,
        "text/plain",
        ASSET_DATA,
    );
    upload_asset_plaintext(
        &mut relay,
        device.vault_id,
        device.device_id,
        &device.identity.signing_key,
        &device.root,
        [0x78; 32],
        &plaintext,
    );
    seed_asset_pull_fixture(&vault, &relay);
    assert_asset_pull_rejected(&vault, &mut relay, &device);
}

// ============================================================ task-118 阻断 A
// 恶意 relay 用超大 `part_count`/`total_len` 的 ASSET 事件触发**无界分配**：读回层
// 曾在任何校验/IO 之前执行 `Vec::with_capacity(part_count)`，分配失败是 abort（不可
// 捕获），pull 无从 fail-closed。这里覆盖三层：wire（decode 拒绝）、读回层（先验后配）、
// engine 入口（零副作用）。

/// 手工拼 `ObjectCommit` 的 CDE 字节，绕过 `ObjectCommit::encode` 的 `validate`。
///
/// 攻击面是"relay 直接下发任意 CDE 字节"，不是"本机库能编码出非法声明"；负例必须能
/// 造出编码期校验会拒绝的字段组合，才能证明解码/消费路径本身 fail-closed。
fn asset_commit_cde_bytes(commit: &ObjectCommit) -> Vec<u8> {
    encode_cde(&CdeValue::Map(vec![
        (0, CdeValue::Bytes(commit.vault_id.to_vec())),
        (1, CdeValue::Bytes(commit.object_id.to_vec())),
        (2, CdeValue::Bytes(commit.message_id.to_vec())),
        (3, CdeValue::UInt(u64::from(commit.object_type.as_u8()))),
        (4, CdeValue::UInt(commit.generation)),
        (5, CdeValue::UInt(commit.key_epoch)),
        (6, CdeValue::UInt(commit.sender_seq)),
        (7, CdeValue::UInt(u64::from(commit.part_count))),
        (8, CdeValue::UInt(commit.total_len)),
        (9, CdeValue::Bytes(commit.object_digest.to_vec())),
    ]))
    .expect("CDE 编码")
}

/// 超大 `part_count` = `u32::MAX`、`total_len` 与固定切片规则自洽（≈1.8e16）。
///
/// 这条声明在收紧前能通过 wire 校验，然后被读回层当成容量使用。pull 必须在 wire
/// 层就 `TooLarge`，且零副作用：不写 skipped-asset 记录、不推游标/received。
#[test]
fn pull_rejects_asset_with_unbounded_part_count_and_total_len() {
    ensure_test_identity_wrap();
    let (_dir, vault) = fresh_vault();
    let device = Device::new(0x70);
    let mut relay = FakeRelay::new(&[&device]);
    let (_object_id, _) = publish_device_asset(&mut relay, &device, ASSET_DATA, "text/plain");

    let total_len = u64::from(u32::MAX) * cap_plain(ObjectType::Asset);
    let part_count = u32::try_from(part_count_for(total_len, ObjectType::Asset)).expect("片数");
    assert_eq!(part_count, u32::MAX, "攻击声明必须与固定切片规则自洽");

    {
        let event = relay.log.last_mut().expect("ASSET 日志事件");
        let mut commit = ObjectCommit::decode(&event.event).expect("原始 commit");
        commit.part_count = part_count;
        commit.total_len = total_len;
        // 保留真实 object_id/epoch/digest：攻击点只是无界长度声明。
        event.event = asset_commit_cde_bytes(&commit);
    }
    seed_asset_pull_fixture(&vault, &relay);
    let before_cursor = cursor(&vault);
    let before_received = received(&vault);
    let error = {
        let mut sync = engine(&vault, &mut relay, &device);
        sync.pull().expect_err("超限 ASSET 必须让 pull 返回 Err")
    };
    assert!(
        matches!(error, SyncError::Proto(ProtoError::TooLarge)),
        "必须在 wire 层拒绝（不能走到分配/下载）: {error:?}"
    );
    assert_eq!(cursor(&vault), before_cursor, "不得推进 relay 游标");
    assert_eq!(received(&vault), before_received, "不得推进 received 水位");
    assert!(
        skipped_asset_rows(&vault).is_empty(),
        "不得写 skipped-asset 记录"
    );
}

/// 读回层（A2）：`fetch_object` 必须在**分配/IO 之前**拒绝不合理声明。
///
/// 这里直接调用读回 API 而不是走 pull：wire 收紧后 pull 到不了这一层，但它是其它
/// 调用方（checkpoint/slot/重打包）的公共入口，必须自己有界。
#[test]
fn fetch_object_rejects_oversized_declarations_before_allocating() {
    ensure_test_identity_wrap();
    let (_dir, _vault) = fresh_vault();
    let device = Device::new(0x71);
    let mut relay = FakeRelay::new(&[&device]);
    let (object_id, _) = publish_device_asset(&mut relay, &device, ASSET_DATA, "text/plain");
    let digest = [0x3c; 32];

    // part_count 与固定切片规则不符（u32::MAX 但总长很小）→ 关系校验拒绝。
    let error = menghunote_sync::upload::fetch_object(
        &mut relay,
        &device.root,
        ObjectType::Asset,
        &object_id,
        u32::MAX,
        128,
        &digest,
        &[],
    )
    .expect_err("part_count 与固定切片不符必须拒绝");
    assert!(matches!(
        error,
        SyncError::Object("读回对象分片数与固定切片不符")
    ));

    // total_len 超 ASSET 上限（part_count 与之自洽）→ 类型上限拒绝。
    let total_len = MAX_ASSET_RAW_BYTES as u64 + 1;
    let part_count = u32::try_from(part_count_for(total_len, ObjectType::Asset)).expect("片数");
    let error = menghunote_sync::upload::fetch_object(
        &mut relay,
        &device.root,
        ObjectType::Asset,
        &object_id,
        part_count,
        total_len,
        &digest,
        &[],
    )
    .expect_err("超上限 total_len 必须拒绝");
    assert!(matches!(
        error,
        SyncError::Object("读回对象明文超出类型上限")
    ));
}

/// engine 入口（A3）：ASSET 事件的 epoch 只要**不等于**本地根（含落后），必须在
/// 下载之前统一报 `Paused(EpochMismatch)`。
///
/// 旧实现只拒绝"更新"的 relay（`event.epoch > root`），落后的 epoch 会走到取片失败
/// 或 AEAD 失败，被报成 `Object`/`RelayApi`，与 UPDATE 路径的暂停语义不一致。
#[test]
fn pull_pauses_on_lagging_asset_event_epoch_before_download() {
    ensure_test_identity_wrap();
    let (_dir, vault) = fresh_vault();
    let mut device = Device::new(0x72);
    let mut relay = FakeRelay::new(&[&device]);
    // 事件本身是 E1 的结构合法 ASSET（无真实 parts）。
    relay.publish_signed_asset(&device.identity.signing_key, [0x5e; 32]);
    // 本地刚完成轮换：根与 relay 都到 E2，日志里仍是旧 E1 事件。
    relay.epoch = 2;
    device.root =
        menghunote_sync::keys::RootSecretV1::new(device.vault_id, 2, device.root.root.clone());
    seed_asset_pull_fixture(&vault, &relay);

    let error = {
        let mut sync = engine(&vault, &mut relay, &device);
        sync.pull().expect_err("落后 epoch 的 ASSET 必须暂停")
    };
    assert!(
        matches!(error, SyncError::Paused(PauseReason::EpochMismatch)),
        "必须报 EpochMismatch（不能走到下载/AEAD）: {error:?}"
    );
    assert_eq!(cursor(&vault).as_deref(), Some("c0"), "不得推进游标");
    assert_eq!(received(&vault), 0, "不得推进 received 水位");
    assert!(
        skipped_asset_rows(&vault).is_empty(),
        "不得写 skipped-asset 记录"
    );
}

// ============================================================ P2-9：本地资产损坏
//
// `upload_one_asset` 在**本地字节与元数据不一致**时返回 `LocalCorrupt`。原实现把它
// 和网络失败一样 `report.failed += 1` 后继续，于是每轮都"重试成功"、永不报错。
// 约定：本地库损坏不是可重试失败——扫完其余资产后**上抛**；健康资产仍要完整上传。

const ASSET_MEDIA: &str = "text/plain";

/// 建一篇资产测试用的 note（`loro_docs` 行 + `notes` 行；`asset_refs` 的外键要求）。
fn seed_asset_note(vault: &Vault, note_id: &str, peer_id: u64) {
    seed_doc(vault, note_id, peer_id);
    create_note_slot(vault, note_id, note_id, note_id, 1_700_000_000).expect("notes 行");
}

/// 用 storage **公开 API** 构造"元数据 hash 与本地字节不一致"的损坏资产。
///
/// 路径：`replace_asset_refs` 先按引用建 `data IS NULL` 的元数据占位（`byte_len`/
/// `media_type` 也是 NULL），再用 `insert_asset` 的"补全占位明文"分支写入**另一份
/// 字节**——该分支只校验长度、不重算内容哈希。这正是真实损坏（外部写坏/恢复错
/// 字节）在库里的形态，且不需要测试直接改 SQL。
fn seed_corrupt_asset(vault: &Vault, note_id: &str, declared: [u8; 32], actual: &[u8]) {
    replace_asset_refs(vault, note_id, 1, &[(declared, 0)]).expect("元数据占位 + 引用");
    insert_asset(
        vault,
        &declared,
        actual.len() as u64,
        Some(ASSET_MEDIA),
        actual,
    )
    .expect("按占位补全明文");
}

/// 健康资产：元数据与字节自洽，并被 `asset_refs` 引用（可被增量上传）。
fn seed_healthy_asset(vault: &Vault, note_id: &str, data: &[u8]) -> [u8; 32] {
    let hash = asset_content_hash(data);
    insert_asset(vault, &hash, data.len() as u64, Some(ASSET_MEDIA), data).expect("insert_asset");
    link_asset_ref(vault, note_id, &hash, 0, 1).expect("link_asset_ref");
    hash
}

/// durable "已上传"标记（`sync.asset_uploaded.<hex object_id>`）的键列表。
fn asset_marker_keys(vault: &Vault) -> Vec<String> {
    vault
        .query(
            "SELECT key FROM meta WHERE substr(key,1,?1)=?2 ORDER BY key",
            &[
                menghunote_storage::Value::Integer("sync.asset_uploaded.".len() as i64),
                menghunote_storage::Value::Text("sync.asset_uploaded.".to_owned()),
            ],
        )
        .expect("读 asset 标记")
        .iter()
        .map(|row| match row.first() {
            Some(menghunote_storage::Value::Text(key)) => key.clone(),
            _ => panic!("asset 标记 key 类型非法"),
        })
        .collect()
}

/// 本地损坏（hash 与字节不一致）必须**上抛**，不能被计入 `failed` 后静默继续。
#[test]
fn corrupt_local_asset_raises_instead_of_counting_retryable_failure() {
    ensure_test_identity_wrap();
    let (_dir, vault) = fresh_vault();
    let device = Device::new(0x7c);
    let mut relay = FakeRelay::new(&[&device]);
    seed_asset_note(&vault, "note-corrupt", device.identity.peer_id);

    // 元数据声明的是 `expected-bytes-A` 的哈希，库里实际存的是 `tampered-bytes-A`
    // （同长、不同内容），`insert_asset` 的占位补全路径不会拦住它。
    let declared = asset_content_hash(b"expected-bytes-A");
    let actual = b"tampered-bytes-A";
    seed_corrupt_asset(&vault, "note-corrupt", declared, actual);
    assert_eq!(
        asset_bytes(&vault, declared).expect("读本地资产"),
        Some(actual.to_vec()),
        "夹具前提：本地确实存着与元数据 hash 不一致的字节"
    );
    assert_ne!(asset_content_hash(actual), declared, "夹具必须是损坏的");

    let error = {
        let mut sync = engine(&vault, &mut relay, &device);
        sync.upload_pending_assets()
            .expect_err("本地库损坏必须上抛，不能计入 failed 后静默继续")
    };
    assert!(
        matches!(error, SyncError::LocalCorrupt("资产字节哈希与元数据不一致")),
        "必须原样上抛 LocalCorrupt（不是可重试网络失败）：{error:?}"
    );
    assert!(relay.put_history.is_empty(), "损坏资产不得上传任何分片");
    assert!(relay.objects.is_empty(), "损坏资产不得提交对象");
    assert!(
        asset_marker_keys(&vault).is_empty(),
        "损坏资产不得落 durable 标记"
    );
}

/// 单个损坏资产不得阻止其余健康资产上传（扫描完再上抛），且下一轮不得静默"重试成功"。
#[test]
fn corrupt_asset_does_not_block_healthy_upload_but_still_raises() {
    ensure_test_identity_wrap();
    let (_dir, vault) = fresh_vault();
    let device = Device::new(0x7d);
    let mut relay = FakeRelay::new(&[&device]);

    // 健康资产放在**后面**：`list_referenced_assets` 按 hash 升序，4ba7… < d6cc…，
    // 损坏资产先被扫到——若实现改成 fail-fast，这条用例就会丢掉健康资产。
    seed_asset_note(&vault, "note-corrupt", device.identity.peer_id);
    seed_asset_note(&vault, "note-mixed", device.identity.peer_id);
    let declared = asset_content_hash(b"expected-bytes-A");
    seed_corrupt_asset(&vault, "note-corrupt", declared, b"tampered-bytes-A");
    let healthy_data = b"healthy asset bytes";
    let healthy = seed_healthy_asset(&vault, "note-mixed", healthy_data);
    assert!(
        declared < healthy,
        "夹具顺序前提：损坏资产必须先于健康资产被扫描"
    );

    let error = {
        let mut sync = engine(&vault, &mut relay, &device);
        sync.upload_pending_assets()
            .expect_err("存在本地损坏时整轮必须报错")
    };
    assert!(
        matches!(error, SyncError::LocalCorrupt("资产字节哈希与元数据不一致")),
        "必须原样上抛 LocalCorrupt：{error:?}"
    );

    // 健康资产仍然完整上传：分片 + 对象提交 + durable 标记。
    let healthy_object = menghunote_sync::checkpoint::asset_object_id(
        &device.root,
        &healthy,
        healthy_data.len() as u64,
        ASSET_MEDIA,
    )
    .expect("健康资产 object_id");
    assert!(
        relay.parts.contains_key(&(healthy_object, 0)),
        "健康资产分片必须已上传（不能被单个损坏拦住）"
    );
    assert_eq!(
        relay
            .objects
            .get(&healthy_object)
            .map(|object| object.commit.object_type),
        Some(ObjectType::Asset),
        "健康资产必须完成对象提交"
    );
    let markers = asset_marker_keys(&vault);
    assert_eq!(markers.len(), 1, "只有健康资产落 durable 标记");
    assert!(
        markers[0].ends_with(&menghunote_sync::ids::hex_of(&healthy_object)),
        "标记必须属于健康资产"
    );

    // 下一轮：健康资产命中标记（不重传、不再探测 relay），损坏资产仍如实上抛。
    let puts = relay.put_history.len();
    let gets = relay.part_gets.len();
    let again = {
        let mut sync = engine(&vault, &mut relay, &device);
        sync.upload_pending_assets()
            .expect_err("损坏仍在，下一轮必须继续报错")
    };
    assert!(
        matches!(again, SyncError::LocalCorrupt("资产字节哈希与元数据不一致")),
        "下一轮仍必须是 LocalCorrupt：{again:?}"
    );
    assert_eq!(
        relay.put_history.len(),
        puts,
        "健康资产命中标记后不得重传分片"
    );
    assert_eq!(
        relay.part_gets.len(),
        gets,
        "健康资产命中标记后不得再探测 relay"
    );
}

/// 正常路径不受影响：健康的被引用资产仍计入 `uploaded`。
#[test]
fn healthy_referenced_asset_is_still_counted_as_uploaded() {
    ensure_test_identity_wrap();
    let (_dir, vault) = fresh_vault();
    let device = Device::new(0x7e);
    let mut relay = FakeRelay::new(&[&device]);
    seed_asset_note(&vault, "note-healthy", device.identity.peer_id);
    seed_healthy_asset(&vault, "note-healthy", b"healthy asset bytes");

    let report = {
        let mut sync = engine(&vault, &mut relay, &device);
        sync.upload_pending_assets().expect("健康资产必须正常上传")
    };
    assert_eq!(report.referenced, 1);
    assert_eq!(report.uploaded, 1, "正常资产仍必须计入 uploaded");
    assert_eq!(report.failed, 0, "不得莫名计成失败");
    assert_eq!(report.skipped_invalid, 0);
    assert_eq!(asset_marker_keys(&vault).len(), 1);
}

// ============================================================ Phase 0：指标 + 批量 inbox
// 只加**只读**度量与默认关闭的实验开关；不改变追赶策略、不动 relay/wire/schema。

/// 指定 `EngineConfig` 跑一轮 pull + integrate（Phase 0 批量开关对照用）。
fn pull_integrate_with_config(
    vault: &Vault,
    relay: &mut FakeRelay,
    device: &Device,
    inbox_batch: bool,
) -> (
    menghunote_sync::engine::PullReport,
    menghunote_sync::engine::IntegrateReport,
) {
    let config = EngineConfig {
        inbox_batch,
        ..EngineConfig::default()
    };
    let mut sync = engine_with_config(vault, relay, device, config);
    let pull = sync.pull().expect("拉取");
    let integrate = sync.integrate(16).expect("集成");
    (pull, integrate)
}

/// 两个 UPDATE + 一个真实 ASSET：指标口径固定、非零、且与 durable 事实一致。
///
/// - `update_bytes` = 成功验收的 `UpdateV1` **明文负载**之和（= inbox 里的
///   `verified_payload` 长度之和），不含信封头/AEAD tag；
/// - `asset_bytes` = 真实下载并验证通过的资产明文字节；
/// - `elapsed_ms` 是毫秒截断（只断言合理上界，不断言快慢）。
#[test]
fn pull_and_integrate_reports_phase0_metrics() {
    ensure_test_identity_wrap();
    let (_a_dir, vault_a) = fresh_vault();
    let (_b_dir, vault_b) = fresh_vault();
    let device_a = Device::new(0x75);
    let device_b = Device::new(0x76);
    let mut relay = FakeRelay::new(&[&device_a, &device_b]);

    let doc_a = seed_doc(&vault_a, "doc-1", device_a.identity.peer_id);
    doc_a.insert_scalars(0, "alpha").expect("A 正文1");
    commit_with_intent(&vault_a, &doc_a, &uuid_like(0x7501), [0x75; 32], EPOCH);
    doc_a.insert_scalars(5, "-beta").expect("A 正文2");
    commit_with_intent(&vault_a, &doc_a, &uuid_like(0x7502), [0x76; 32], EPOCH);
    assert_eq!(publish_from(&mut relay, &vault_a, &device_a), 2);
    let (_asset, asset_seq) = publish_device_asset(&mut relay, &device_a, ASSET_DATA, "image/png");
    assert_eq!(asset_seq, 3);

    seed_doc(&vault_b, "doc-1", device_b.identity.peer_id);
    seed_cursor(&vault_b);

    let pull = {
        let mut sync = engine(&vault_b, &mut relay, &device_b);
        sync.pull().expect("拉取")
    };
    // 指标必须在 integrate 之前对账（integrate 会把 inbox 行标成 integrated，
    // `take_inbox_ready` 只返回仍 verified 的行）。
    let inbox_rows = take_inbox_ready(&vault_b, 16).expect("读 inbox");
    let integrate = {
        let mut sync = engine(&vault_b, &mut relay, &device_b);
        sync.integrate(16).expect("集成")
    };

    assert_eq!(pull.events, 3, "两条 UPDATE + 一条 ASSET");
    assert_eq!(pull.stored, 2);
    assert_eq!(pull.skipped, 1, "ASSET 走 skipped 计数不进 inbox");
    assert_eq!(pull.pages, 1);
    assert!(pull.inbox_batch, "Phase 1A-2 起默认批量写 inbox");
    assert_eq!(pull.next_cursor, "c3");
    assert!(pull.update_bytes > 0, "成功验收的 update 明文负载必须 > 0");
    assert_eq!(pull.asset_bytes, ASSET_DATA.len() as u64);
    assert!(pull.elapsed_ms < 60_000, "耗时必须是合理的毫秒数");

    // update_bytes 与 durable inbox 的明文负载逐字节对齐。
    assert_eq!(inbox_rows.len(), 2);
    let expected_update_bytes: u64 = inbox_rows
        .iter()
        .map(|row| {
            u64::try_from(row.verified_payload.as_deref().unwrap_or_default().len())
                .expect("payload 长度在 u64 内")
        })
        .sum();
    assert!(expected_update_bytes > 0);
    assert_eq!(pull.update_bytes, expected_update_bytes);

    assert_eq!(integrate.integrated, 2);
    assert_eq!(integrate.deferred, 0);
    assert_eq!(integrate.body_integrated, 2);
    assert_eq!(integrate.structure_integrated, 0);
    assert!(!integrate.projected, "没有结构文档就不会投影");
    assert!(integrate.elapsed_ms < 60_000);

    // 空页：零事件/零字节，但仍然算一次页请求；integrate 空转也有耗时。
    let (empty, again) = {
        let mut sync = engine(&vault_b, &mut relay, &device_b);
        let empty = sync.pull().expect("空页拉取");
        let again = sync.integrate(16).expect("再集成");
        (empty, again)
    };
    assert_eq!(empty.events, 0);
    assert_eq!(empty.stored, 0);
    assert_eq!(empty.skipped, 0);
    assert_eq!(empty.update_bytes, 0);
    assert_eq!(empty.asset_bytes, 0);
    assert_eq!(empty.pages, 1);
    assert!(empty.elapsed_ms < 60_000);
    assert_eq!(again.integrated, 0);
    assert!(again.elapsed_ms < 60_000);
}

/// Phase 0 批量开关：打开后**结果**必须与逐条路径一致，只是 `inbox_batch` 标为 true。
///
/// 对照两条独立的 B vault（同一份 relay 日志、各自 cursor/log_id），断言事件数/字节/
/// 游标/received 水位/integrate 结论与最终正文全部相同。
#[test]
fn pull_inbox_batch_flag_matches_per_event_results() {
    ensure_test_identity_wrap();
    let (_a_dir, vault_a) = fresh_vault();
    let device_a = Device::new(0x77);
    let device_b1 = Device::new(0x78);
    let device_b2 = Device::new(0x79);
    let mut relay = FakeRelay::new(&[&device_a, &device_b1, &device_b2]);

    // A：跨两个文档的两条 update + 一个真实 ASSET（覆盖批量行与 skipped 事件混排）。
    let doc_a1 = seed_doc(&vault_a, "doc-1", device_a.identity.peer_id);
    let doc_a2 = seed_doc(&vault_a, "doc-2", device_a.identity.peer_id);
    doc_a1.insert_scalars(0, "one").expect("doc1");
    commit_with_intent(&vault_a, &doc_a1, &uuid_like(0x7701), [0x77; 32], EPOCH);
    doc_a2.insert_scalars(0, "two").expect("doc2");
    commit_with_intent(&vault_a, &doc_a2, &uuid_like(0x7702), [0x78; 32], EPOCH);
    assert_eq!(publish_from(&mut relay, &vault_a, &device_a), 2);
    publish_device_asset(&mut relay, &device_a, ASSET_DATA, "image/png");

    let (_d1, vault_b1) = fresh_vault();
    let (_d2, vault_b2) = fresh_vault();
    for (vault, device) in [(&vault_b1, &device_b1), (&vault_b2, &device_b2)] {
        seed_doc(vault, "doc-1", device.identity.peer_id);
        seed_doc(vault, "doc-2", device.identity.peer_id);
        seed_cursor(vault);
    }

    let (row_pull, row_integrate) =
        pull_integrate_with_config(&vault_b1, &mut relay, &device_b1, false);
    let (batch_pull, batch_integrate) =
        pull_integrate_with_config(&vault_b2, &mut relay, &device_b2, true);

    assert!(!row_pull.inbox_batch, "逐条路径必须报 false");
    assert!(batch_pull.inbox_batch, "批量路径必须报 true");
    assert_eq!(batch_pull.events, row_pull.events);
    assert_eq!(batch_pull.stored, row_pull.stored);
    assert_eq!(batch_pull.skipped, row_pull.skipped);
    assert_eq!(batch_pull.update_bytes, row_pull.update_bytes);
    assert_eq!(batch_pull.asset_bytes, row_pull.asset_bytes);
    assert_eq!(batch_pull.pages, row_pull.pages);
    assert_eq!(batch_pull.next_cursor, row_pull.next_cursor);
    assert_eq!(cursor(&vault_b1), cursor(&vault_b2));
    assert_eq!(received(&vault_b1), received(&vault_b2));

    assert_eq!(batch_integrate.integrated, row_integrate.integrated);
    assert_eq!(batch_integrate.deferred, row_integrate.deferred);
    assert_eq!(
        batch_integrate.body_integrated,
        row_integrate.body_integrated
    );
    assert_eq!(
        batch_integrate.structure_integrated,
        row_integrate.structure_integrated
    );

    for doc_id in ["doc-1", "doc-2"] {
        let (body_row, head_row) =
            load_body_doc(&vault_b1, doc_id, device_b1.identity.peer_id, None).expect("B1 正文");
        let (body_batch, head_batch) =
            load_body_doc(&vault_b2, doc_id, device_b2.identity.peer_id, None).expect("B2 正文");
        assert_eq!(body_row.text(), body_batch.text(), "{doc_id} 正文必须等价");
        assert_eq!(
            head_row.head_seq, head_batch.head_seq,
            "{doc_id} head 必须一致"
        );
    }
    let (body, _) =
        load_body_doc(&vault_b2, "doc-1", device_b2.identity.peer_id, None).expect("B2");
    assert!(body.text().contains("one"), "批量路径必须真的集成了正文");
}

`````



@@SECTION code/095 `crates/sync/tests/phase1a_release.rs`


`````rust
//! Phase 1A release 探针：`baseline_apply` 持闸 merge 成本（手动运行，不 assert 时间）。
//!
//! 只测 **storage 的 baseline merge**：在已有库上准备 20 篇带本地未上传 delta 的正文，
//! 构造 5 MiB / 20 MiB 两档 Loro snapshot（20 篇合计明文），走
//! `begin_baseline_apply → apply_baseline_batch × N → finish_baseline_apply`，
//! 记录总墙钟 / 各段 / `gate_hold_ms`（begin + 各批 apply + finish 的墙钟和，
//! 对应 Phase 1A 计划里"持写闸 merge"的时间口径），以及 storage 内置的分段计时
//! 与计数（restore / head 查询 / 本地重放 / 本地差集 / 二次 snapshot import /
//! detached oplog import / pending_update / SQLite 事务），用于区分
//! 「Loro 单大文本 op 的超线性」与「重复解码/二次导入」两类成本。
//!
//! 不涉及 relay / checkpoint 签名 / 真实下载：`S` 与 cursor 是自造的合法值；
//! 每次都用 fresh vault，结束后 journal 一律 `completed`，不会给下一次留 staging。
//!
//! 第二个探针 [`checkpoint_validate_scan_cost`] 测 **checkpoint validate 的 coverage
//! 日志扫描 O(S) 成本**：FakeRelay 构造 S = 1k / 5k / 25k 条混合日志（85.7% UPDATE +
//! 14.3% ASSET），走真实 `CheckpointWriter::prepare → validate`（不复制 scan 实现），
//! 只打印页数/墙钟，不 assert 时间。
//!
//! ```bash
//! cargo test --release -p menghunote-sync --test phase1a_release -- --ignored --nocapture
//! ```

mod support;

use std::time::Instant;

use support::{
    commit_local, commit_with_intent, fresh_vault, publish_real_asset, seed_doc, uuid_like,
    FakeRelay, TestRandom, EPOCH, VAULT_ID,
};

use menghunote_storage::{
    apply_baseline_batch, baseline_apply_debug_counters, baseline_apply_debug_timings,
    baseline_apply_state, begin_baseline_apply, create_note_slot, ensure_structure_row,
    finish_baseline_apply, load_body_doc, reset_baseline_apply_debug, sync_cursor_state,
    BaselineApplyDebugCounters, BaselineApplyDebugTimings, BaselineApplyFinish,
    BaselineApplyStatus, BaselineExpectation, BodyDoc, Value, Vault, BASELINE_MERGE_BATCH,
    META_SYNC_CONFIRMED_HIGHWATER, META_SYNC_INTEGRATED_SEQ, META_SYNC_LOG_ID,
    META_SYNC_RECEIVED_SEQ, META_SYNC_RELAY_CURSOR,
};
use menghunote_sync::checkpoint::{CheckpointWriter, EmptyAssets, PreparedCheckpoint};
use menghunote_sync::engine::RELAY_CURSOR_META;
use menghunote_sync::genesis;
use menghunote_sync::relay::{
    BootstrapBundle, BootstrapLease, CheckpointLease, CheckpointPublish, CheckpointRef,
    ObjectSubmission, PublishedCheckpoint, RelayClient, RelayLogKind, RelayLogPage, RelayState,
    SlotRecord, SlotUpload, UpdateOutcome, UpdateSubmission,
};
use menghunote_sync::{DeviceId, ObjectId, SyncError};
use menghunote_sync_proto::DeviceCertV1;

/// 基线文档数（真实信任链里 docs 数量更多；这里固定 20 便于两档总量均分）。
const DOCS: usize = 20;
/// 本地设备 peer。
const LOCAL_PEER: u64 = 0x41;
/// 远端基线写者 peer（只用于生成 snapshot；与本地 peer 不同）。
const REMOTE_PEER: u64 = 0x42;
/// 本地未上传 delta 的标记（大写 + 下划线，不会出现在远端随机字母表里）。
const LOCAL_PREFIX: &str = "LOCAL_UNSYNCED_";
/// 远端基线内容标记（同上）。
const REMOTE_PREFIX: &str = "REMOTE_BASELINE_";
/// 每批 apply 的文档数（20 篇 → 4 批；生产 `BOOTSTRAP_DOC_BATCH=64`，这里分小批
/// 是为了能看到分段耗时，总 merge 工作量相同）。
const APPLY_CHUNK: usize = 5;
/// 模拟的 checkpoint 覆盖水位（自造合法值）。
const BASELINE_S: u64 = 500;
/// finish 选定的 cursor。
const BASELINE_CURSOR: &str = "c500";
/// 本地已有水位（finish 后必须单调前进到 `BASELINE_S`）。
const SEED_RECEIVED: u64 = 100;
const SEED_INTEGRATED: u64 = 80;
const SEED_HIGHWATER: u64 = 90;
/// 本地已有 relay cursor。
const SEED_CURSOR: &str = "c80";

/// 远端正文随机字母表（只用小写字母 + 数字，标记里的大写/下划线不可能偶然出现）。
const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";

fn pseudo_text(seed: u64, chars: usize) -> String {
    let mut state = seed
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(0xD1B5_4A32_D192_ED03);
    let mut out = String::with_capacity(chars + 32);
    while out.len() < chars {
        // xorshift64：确定性、跨平台一致，避免为基准引入随机依赖。
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        let value = state.wrapping_mul(0x2545_F491_4F6C_DD1D);
        for byte in value.to_le_bytes() {
            out.push(ALPHABET[(byte as usize) % ALPHABET.len()] as char);
            if out.len() >= chars {
                break;
            }
        }
    }
    out
}

fn remote_text(index: usize, chars: usize) -> String {
    let mut text = format!("{REMOTE_PREFIX}{index:02}|");
    text.push_str(&pseudo_text(0x5eed_0000 + index as u64, chars));
    text
}

fn local_text(index: usize) -> String {
    format!("{LOCAL_PREFIX}{index:02}|{}", "L".repeat(2048))
}

fn build_snapshot(index: usize, chars: usize) -> Vec<u8> {
    let doc = BodyDoc::create(format!("bench-doc-{index:02}"), REMOTE_PEER)
        .expect("创建远端基线 BodyDoc");
    doc.insert_scalars(0, &remote_text(index, chars))
        .expect("写远端基线正文");
    doc.export_snapshot().expect("导出 baseline snapshot")
}

/// 生成 20 篇 snapshot，使总字节数尽量贴近 `target_bytes`。
///
/// 先用 16 KiB 文本量出"snapshot 字节 / 文本字符"的比例，再按比例放大；若实测
/// 总字节偏离目标 >2%，按实测比例再重建一次（Loro snapshot 有固定的块/字典开销，
/// 超出这个范围说明比例外推不准）。
fn build_snapshot_set(target_bytes: usize) -> (Vec<Vec<u8>>, usize, f64) {
    let per_doc = target_bytes / DOCS;
    let pilot_chars = 16 * 1024;
    let pilot = build_snapshot(0x3ff, pilot_chars);
    let ratio = pilot.len() as f64 / pilot_chars as f64;
    assert!(ratio > 0.0, "snapshot 比例必须为正");

    let mut chars = ((per_doc as f64) / ratio).ceil() as usize;
    let mut snapshots: Vec<Vec<u8>> = (0..DOCS)
        .map(|index| build_snapshot(index, chars))
        .collect();
    let total: usize = snapshots.iter().map(Vec::len).sum();
    if (total as f64 - target_bytes as f64).abs() / target_bytes as f64 > 0.02 {
        let scale = target_bytes as f64 / total as f64;
        chars = ((chars as f64) * scale).ceil() as usize;
        snapshots = (0..DOCS)
            .map(|index| build_snapshot(index, chars))
            .collect();
    }
    (snapshots, chars, ratio)
}

fn count(vault: &Vault, sql: &str) -> i64 {
    let rows = vault.query(sql, &[]).expect("count 查询");
    match rows.first().and_then(|row| row.first()) {
        Some(Value::Integer(value)) => *value,
        other => panic!("count 结果异常：{other:?}"),
    }
}

fn seed_watermarks(vault: &Vault) {
    vault
        .set_meta(META_SYNC_RECEIVED_SEQ, &SEED_RECEIVED.to_be_bytes())
        .expect("seed received");
    vault
        .set_meta(META_SYNC_INTEGRATED_SEQ, &SEED_INTEGRATED.to_be_bytes())
        .expect("seed integrated");
    vault
        .set_meta(META_SYNC_CONFIRMED_HIGHWATER, &SEED_HIGHWATER.to_be_bytes())
        .expect("seed highwater");
    vault
        .set_meta(META_SYNC_RELAY_CURSOR, SEED_CURSOR.as_bytes())
        .expect("seed relay cursor");
}

/// 在 fresh vault 上准备 20 篇"已有本地正文 + 本地未上传 delta"的文档。
fn prepare_vault() -> (tempfile::TempDir, Vault, Vec<String>) {
    let (dir, vault) = fresh_vault();
    let mut doc_ids = Vec::with_capacity(DOCS);
    for index in 0..DOCS {
        let doc_id = format!("bench-doc-{index:02}");
        let doc = seed_doc(&vault, &doc_id, LOCAL_PEER);
        doc.insert_scalars(0, &local_text(index))
            .expect("写本地正文");
        // 不带同步意图：这是一条只在本机、从未上传的 delta。
        commit_local(&vault, &doc);
        doc_ids.push(doc_id);
    }
    seed_watermarks(&vault);
    (dir, vault, doc_ids)
}

#[derive(Debug)]
struct ProbeStats {
    label: &'static str,
    target_bytes: usize,
    snapshot_total: usize,
    snapshot_min: usize,
    snapshot_max: usize,
    chars_per_doc: usize,
    ratio: f64,
    build_snapshot_ms: u128,
    vault_prep_ms: u128,
    begin_ms: u128,
    apply_batch_ms: Vec<u128>,
    apply_total_ms: u128,
    finish_ms: u128,
    gate_hold_ms: u128,
    verify_ms: u128,
    total_wall_ms: u128,
    timings: BaselineApplyDebugTimings,
    counters: BaselineApplyDebugCounters,
}

#[allow(clippy::too_many_lines)]
fn run_probe(label: &'static str, target_bytes: usize) -> ProbeStats {
    let started = Instant::now();

    // 1) 生成快照（不持闸；真实实现里这段是下载 + 解密 + 校验）。
    let build_started = Instant::now();
    let (snapshots, chars_per_doc, ratio) = build_snapshot_set(target_bytes);
    let build_snapshot_ms = build_started.elapsed().as_millis();
    let snapshot_total: usize = snapshots.iter().map(Vec::len).sum();
    let snapshot_min = snapshots.iter().map(Vec::len).min().unwrap_or(0);
    let snapshot_max = snapshots.iter().map(Vec::len).max().unwrap_or(0);
    assert_eq!(snapshots.len(), DOCS);

    // 2) 已有库：20 篇本地正文 + 未上传 delta + 旧水位。
    let prep_started = Instant::now();
    let (_dir, vault, doc_ids) = prepare_vault();
    let vault_prep_ms = prep_started.elapsed().as_millis();
    assert_eq!(
        count(&vault, "SELECT count(*) FROM sync_outbox"),
        0,
        "本地 delta 不得有 outbox 行"
    );
    let before = sync_cursor_state(&vault).expect("seeded cursor");
    assert_eq!(before.received_seq, SEED_RECEIVED);
    assert_eq!(before.integrated_seq, SEED_INTEGRATED);
    assert_eq!(before.confirmed_highwater, Some(SEED_HIGHWATER));

    // 3) 持闸段：begin → apply 各批 → finish。
    reset_baseline_apply_debug();
    let begin_started = Instant::now();
    let expected =
        BaselineExpectation::new(doc_ids.iter().cloned(), std::iter::empty::<[u8; 32]>());
    let begin_state = begin_baseline_apply(&vault, &[0x51; 16], BASELINE_S, "c0-source", &expected)
        .expect("begin baseline");
    let begin_ms = begin_started.elapsed().as_millis();
    assert_eq!(begin_state.status, BaselineApplyStatus::Staging);
    assert_eq!(begin_state.expected_docs, Some(DOCS as u64));
    assert!(
        !begin_state.finishable,
        "begin 时预期 docs 尚未应用，finishable 必须为 false"
    );

    let entries: Vec<(String, Vec<u8>)> = doc_ids
        .iter()
        .cloned()
        .zip(snapshots.iter().cloned())
        .collect();
    let mut apply_batch_ms = Vec::new();
    let mut apply_total_ms = 0u128;
    for (batch_index, chunk) in entries.chunks(APPLY_CHUNK).enumerate() {
        let batch_started = Instant::now();
        let state = apply_baseline_batch(&vault, chunk, &[]).expect("apply baseline batch");
        let batch_ms = batch_started.elapsed().as_millis();
        apply_total_ms += batch_ms;
        apply_batch_ms.push(batch_ms);
        eprintln!(
            "[phase1a] baseline_apply label={label} batch={batch_index} docs={} apply_ms={batch_ms} applied_docs={}",
            chunk.len(),
            state.applied_docs
        );
    }

    let finish_started = Instant::now();
    let finish = finish_baseline_apply(&vault, BASELINE_S, BASELINE_CURSOR).expect("finish");
    let finish_ms = finish_started.elapsed().as_millis();
    let gate_hold_ms = begin_ms + apply_total_ms + finish_ms;
    let BaselineApplyFinish::Applied(finish_state) = finish else {
        panic!("finish 必须 Applied，实际 {finish:?}");
    };
    assert_eq!(finish_state.status, BaselineApplyStatus::Completed);
    assert_eq!(finish_state.applied_docs, DOCS as u64);
    assert!(finish_state.chosen_cursor.as_deref() == Some(BASELINE_CURSOR));
    let timings = baseline_apply_debug_timings();
    let counters = baseline_apply_debug_counters();

    // 4) 闸外校验：本地 delta 保留、正文可重放、水位单调、无残留 staging。
    let verify_started = Instant::now();
    let after = sync_cursor_state(&vault).expect("cursor after finish");
    assert!(
        after.received_seq >= before.received_seq,
        "received 不得回退"
    );
    assert!(
        after.integrated_seq >= before.integrated_seq,
        "integrated 不得回退"
    );
    assert!(
        after.confirmed_highwater.unwrap_or(0) >= before.confirmed_highwater.unwrap_or(0),
        "highwater 不得回退"
    );
    assert_eq!(after.received_seq, BASELINE_S);
    assert_eq!(after.integrated_seq, BASELINE_S);
    assert_eq!(after.confirmed_highwater, Some(BASELINE_S));
    assert_eq!(
        vault
            .get_meta(META_SYNC_RELAY_CURSOR)
            .expect("relay cursor")
            .as_deref(),
        Some(BASELINE_CURSOR.as_bytes())
    );
    for (index, doc_id) in doc_ids.iter().enumerate() {
        let (doc, head) = load_body_doc(&vault, doc_id, LOCAL_PEER, None).expect("merged doc");
        let text = doc.text();
        assert!(
            text.contains(&format!("{LOCAL_PREFIX}{index:02}")),
            "第 {index} 篇本地未上传 delta 丢失"
        );
        assert!(
            text.contains(&format!("{REMOTE_PREFIX}{index:02}")),
            "第 {index} 篇基线正文未导入"
        );
        assert!(
            head.head_seq >= 2,
            "第 {index} 篇应至少有本地 + baseline 两条 update"
        );
    }
    assert_eq!(
        count(
            &vault,
            "SELECT count(*) FROM loro_updates WHERE batch_id='local'",
        ),
        DOCS as i64,
        "本地 delta 的 update 行必须保留"
    );
    assert_eq!(
        count(
            &vault,
            &format!("SELECT count(*) FROM loro_updates WHERE batch_id='{BASELINE_MERGE_BATCH}'"),
        ),
        DOCS as i64,
        "每篇应恰好追加一条 baseline_merge 组合 update"
    );
    assert_eq!(count(&vault, "SELECT count(*) FROM sync_outbox"), 0);
    let journal = baseline_apply_state(&vault)
        .expect("state")
        .expect("journal");
    assert_eq!(journal.status, BaselineApplyStatus::Completed);
    assert!(!journal.finishable);
    let verify_ms = verify_started.elapsed().as_millis();

    let total_wall_ms = started.elapsed().as_millis();
    ProbeStats {
        label,
        target_bytes,
        snapshot_total,
        snapshot_min,
        snapshot_max,
        chars_per_doc,
        ratio,
        build_snapshot_ms,
        vault_prep_ms,
        begin_ms,
        apply_batch_ms,
        apply_total_ms,
        finish_ms,
        gate_hold_ms,
        verify_ms,
        total_wall_ms,
        timings,
        counters,
    }
}

fn print_probe(stats: &ProbeStats) {
    let mib = |bytes: usize| bytes as f64 / (1024.0 * 1024.0);
    eprintln!(
        "[phase1a] baseline_apply label={} target_mib={:.2} snapshot_total={} snapshot_mib={:.2} \
         per_doc_min={} per_doc_max={} chars_per_doc={} snapshot_ratio={:.4} docs={DOCS}",
        stats.label,
        mib(stats.target_bytes),
        stats.snapshot_total,
        mib(stats.snapshot_total),
        stats.snapshot_min,
        stats.snapshot_max,
        stats.chars_per_doc,
        stats.ratio,
    );
    eprintln!(
        "[phase1a] baseline_apply label={} build_snapshot_ms={} vault_prep_ms={} begin_ms={} \
         apply_batches_ms={:?} apply_total_ms={} finish_ms={} gate_hold_ms={} verify_ms={} \
         total_wall_ms={}",
        stats.label,
        stats.build_snapshot_ms,
        stats.vault_prep_ms,
        stats.begin_ms,
        stats.apply_batch_ms,
        stats.apply_total_ms,
        stats.finish_ms,
        stats.gate_hold_ms,
        stats.verify_ms,
        stats.total_wall_ms,
    );
    let t = &stats.timings;
    let ns_to_ms = |ns: u64| ns as f64 / 1e6;
    eprintln!(
        "[phase1a] baseline_apply label={} phases_ms restore={:.1} head_lookup={:.1} \
         load_body_doc={:.1} local_delta_export={:.1} local_delta_import={:.1} \
         snapshot_reimport={:.1} new_doc_import={:.1} pending_update={:.1} \
         transaction={:.1} batch_total_sum={:.1}",
        stats.label,
        ns_to_ms(t.restore_ns),
        ns_to_ms(t.head_lookup_ns),
        ns_to_ms(t.load_body_doc_ns),
        ns_to_ms(t.local_delta_export_ns),
        ns_to_ms(t.local_delta_import_ns),
        ns_to_ms(t.snapshot_reimport_ns),
        ns_to_ms(t.new_doc_import_ns),
        ns_to_ms(t.pending_update_ns),
        ns_to_ms(t.transaction_ns),
        ns_to_ms(t.batch_total_ns),
    );
    let c = &stats.counters;
    eprintln!(
        "[phase1a] baseline_apply label={} counters batches={} docs={} new={} \
         existing_no_extra={} existing_local_extra={} replayed={} snapshot_imports={} \
         snapshot_mib={:.2} load_body_doc_calls={} snapshot_reimport_mib={:.2} \
         new_doc_import_mib={:.2} pending_update_calls={} pending_update_mib={:.2} \
         local_delta_kib={:.2}",
        stats.label,
        c.batches,
        c.docs,
        c.new_docs,
        c.existing_no_extra,
        c.existing_local_extra,
        c.replayed_same_digest,
        c.snapshot_imports,
        mib(c.snapshot_bytes as usize),
        c.load_body_doc_calls,
        mib(c.snapshot_reimport_bytes as usize),
        mib(c.new_doc_import_bytes as usize),
        c.pending_update_calls,
        mib(c.pending_update_bytes as usize),
        c.local_delta_bytes as f64 / 1024.0,
    );
}

#[test]
#[ignore = "manual benchmark"]
fn phase1a_baseline_apply_gate_hold() {
    for (label, bytes) in [
        ("5MiB", 5usize * 1024 * 1024),
        ("20MiB", 20usize * 1024 * 1024),
    ] {
        let stats = run_probe(label, bytes);
        print_probe(&stats);
    }
}

// ============================================================ checkpoint validate 覆盖日志扫描成本
//
// 任务 4 P1：只测真实 `CheckpointWriter::validate` 里 coverage 日志扫描的 O(S) 成本，
// 不改生产 `scan_coverage_log`，也不复制它的实现。S 条日志是合成的（FakeRelay）：
// `1..=S-1` 落在本地 integrated 连续前缀（生产代码信任的"已消费"不变量），尾部第 S 条
// 是真实 ASSET（prepare 落 pending receipt，validate 必须下载实证）；前缀里按 1/7
// 混入 ASSET 事件（≈14.3%，任务要求的 10–20%）。
//
// 口径：
// - `pages` = validate 期间真实 `RelayClient::log` 调用次数（应从 c0 扫到 S，S/256 页）。
// - `scan_ms` = 第一次 log 开始到最后一次 log 返回的墙钟跨度（含页间逐事件核对；
//   不含最后一页返回之后的 256 条 visitor，误差上界 = 一页）。
// - `validate_ms` = 整个 validate 墙钟（含 manifest/快照读回、尾部 ASSET 实证）。
// 只打印不 assert 时间；硬断言只校验覆盖到了 S、尾部 ASSET 被真实验证、页数符合 S/256。

/// 探针档位（事件总数 S）。
const SCAN_COST_S: [usize; 3] = [1_000, 5_000, 25_000];
/// 每档重复次数（取中位数）。
const SCAN_COST_RUNS: usize = 3;
/// 生产 `COVERAGE_SCAN_PAGE_LIMIT`（`crates/sync/src/checkpoint.rs` 私有常量 = 256）。
/// 只用于校验探针确实按 S/256 页扫完整段；若生产改页大小，这里应同步更新。
const SCAN_COST_PAGE_LIMIT: usize = 256;
/// ASSET 事件间隔（每 7 条一条 → 14.3%）。
const SCAN_COST_ASSET_EVERY: usize = 7;
/// 探针正文文档 id（与 coverage 无关，只给 prepare 提供可导出的本地快照）。
const SCAN_COST_NOTE_ID: &str = "scan-cost-note";

/// 计数/计时包装：所有方法委托给 FakeRelay；只额外统计 `log()`（真实 coverage scan
/// 发起的页请求）与 `part()`（manifest/snapshot/ASSET 读回）的调用次数与墙钟。
///
/// 这是**探针侧 instrumentation**，不是 scan 实现的拷贝：validate 仍走生产
/// `CheckpointWriter::validate` → `scan_coverage_log`，wrapper 只是它调用的客户端。
struct CountingRelay {
    inner: FakeRelay,
    log_calls: u64,
    log_busy_ns: u128,
    scan_first: Option<Instant>,
    scan_last: Option<Instant>,
    part_calls: u64,
    part_busy_ns: u128,
}

impl CountingRelay {
    fn new(inner: FakeRelay) -> Self {
        Self {
            inner,
            log_calls: 0,
            log_busy_ns: 0,
            scan_first: None,
            scan_last: None,
            part_calls: 0,
            part_busy_ns: 0,
        }
    }

    fn reset_counters(&mut self) {
        self.log_calls = 0;
        self.log_busy_ns = 0;
        self.scan_first = None;
        self.scan_last = None;
        self.part_calls = 0;
        self.part_busy_ns = 0;
    }

    /// 第一次到最后一次 `log()` 返回的墙钟跨度（页扫描口径）。
    fn scan_span_ms(&self) -> f64 {
        match (self.scan_first, self.scan_last) {
            (Some(first), Some(last)) => last.duration_since(first).as_secs_f64() * 1e3,
            _ => 0.0,
        }
    }

    /// 仅 `log()` 内部（FakeRelay 取页/克隆事件）的墙钟合计。
    fn log_busy_ms(&self) -> f64 {
        self.log_busy_ns as f64 / 1e6
    }

    /// `part()` 读回墙钟合计（manifest/snapshot/ASSET）。
    fn part_busy_ms(&self) -> f64 {
        self.part_busy_ns as f64 / 1e6
    }
}

impl RelayClient for CountingRelay {
    fn state(&mut self) -> Result<RelayState, SyncError> {
        self.inner.state()
    }

    fn log(&mut self, cursor: Option<&str>, limit: u32) -> Result<RelayLogPage, SyncError> {
        if self.scan_first.is_none() {
            self.scan_first = Some(Instant::now());
        }
        let started = Instant::now();
        let result = self.inner.log(cursor, limit);
        self.log_busy_ns += started.elapsed().as_nanos();
        self.log_calls += 1;
        self.scan_last = Some(Instant::now());
        result
    }

    fn part(&mut self, object_id: &ObjectId, part_no: u32) -> Result<Vec<u8>, SyncError> {
        let started = Instant::now();
        let result = self.inner.part(object_id, part_no);
        self.part_busy_ns += started.elapsed().as_nanos();
        self.part_calls += 1;
        result
    }

    fn put_part(
        &mut self,
        object_id: &ObjectId,
        part_no: u32,
        envelope: &[u8],
    ) -> Result<(), SyncError> {
        self.inner.put_part(object_id, part_no, envelope)
    }

    fn submit_updates(
        &mut self,
        items: Vec<UpdateSubmission>,
    ) -> Result<Vec<UpdateOutcome>, SyncError> {
        self.inner.submit_updates(items)
    }

    fn submit_object(&mut self, item: ObjectSubmission) -> Result<UpdateOutcome, SyncError> {
        self.inner.submit_object(item)
    }

    fn ack(&mut self, received_seq: u64, integrated_seq: u64, epoch: u64) -> Result<(), SyncError> {
        self.inner.ack(received_seq, integrated_seq, epoch)
    }

    fn device_cert(&mut self, device_id: &DeviceId) -> Result<Option<DeviceCertV1>, SyncError> {
        self.inner.device_cert(device_id)
    }

    fn put_key_slot(&mut self, upload: SlotUpload) -> Result<SlotRecord, SyncError> {
        self.inner.put_key_slot(upload)
    }

    fn list_key_slots(&mut self) -> Result<Vec<SlotRecord>, SyncError> {
        self.inner.list_key_slots()
    }

    fn activate_password_slot(
        &mut self,
        expected_slot_set_rev: u64,
        slot_id: ObjectId,
        signed_message: Vec<u8>,
        signature: [u8; 64],
    ) -> Result<u64, SyncError> {
        self.inner
            .activate_password_slot(expected_slot_set_rev, slot_id, signed_message, signature)
    }

    fn create_checkpoint(
        &mut self,
        estimated_bytes: u64,
        epoch: Option<u64>,
    ) -> Result<CheckpointLease, SyncError> {
        self.inner.create_checkpoint(estimated_bytes, epoch)
    }

    fn add_checkpoint_refs(
        &mut self,
        checkpoint_id: [u8; 16],
        refs: Vec<CheckpointRef>,
    ) -> Result<u64, SyncError> {
        self.inner.add_checkpoint_refs(checkpoint_id, refs)
    }

    fn publish_checkpoint(
        &mut self,
        publish: CheckpointPublish,
    ) -> Result<PublishedCheckpoint, SyncError> {
        self.inner.publish_checkpoint(publish)
    }

    fn bootstrap(&mut self) -> Result<BootstrapBundle, SyncError> {
        self.inner.bootstrap()
    }

    fn renew_bootstrap_lease(&mut self, lease_id: [u8; 16]) -> Result<BootstrapLease, SyncError> {
        self.inner.renew_bootstrap_lease(lease_id)
    }
}

/// 一次性夹具：真实 SQLite vault + 合成 S 条日志 + 已 prepare 的 checkpoint。
struct ScanCostFixture {
    _dir: tempfile::TempDir,
    vault: Vault,
    materials: genesis::GenesisMaterials,
    relay: CountingRelay,
    prepared: PreparedCheckpoint,
    asset_events: usize,
    update_events: usize,
    prepare_pages: u64,
    prepare_scan_ms: f64,
    prepare_log_ms: f64,
}

fn build_scan_cost_fixture(total_events: usize) -> ScanCostFixture {
    assert!(
        total_events > 1,
        "S 至少需要一条前缀 UPDATE 与一条尾部 ASSET"
    );
    let (dir, vault) = fresh_vault();
    ensure_structure_row(&vault).expect("structure 行");
    vault
        .set_meta(RELAY_CURSOR_META, b"c0")
        .expect("写 relay 游标");
    let materials = genesis::build_materials(
        VAULT_ID,
        [0x44; 16],
        "pw",
        1_700_000_000,
        &mut TestRandom::new(0x88_14),
    )
    .expect("genesis 材料");
    let mut relay = FakeRelay::new(&[]);
    relay
        .certs
        .insert(materials.device_id, materials.certificate().clone());
    vault
        .set_meta(META_SYNC_LOG_ID, &relay.log_id)
        .expect("写 log_id");

    // 一篇真实正文：prepare 需要可导出的本地快照/head VV（与 coverage 无关）。
    let doc = seed_doc(&vault, SCAN_COST_NOTE_ID, materials.peer_id_u64());
    create_note_slot(
        &vault,
        SCAN_COST_NOTE_ID,
        SCAN_COST_NOTE_ID,
        SCAN_COST_NOTE_ID,
        1_700_000_000,
    )
    .expect("notes 行");
    doc.insert_scalars(0, "checkpoint coverage scan cost probe")
        .expect("正文");
    commit_with_intent(&vault, &doc, &uuid_like(9_001), [0x91; 32], EPOCH);

    // 1..S-1 按 1/7 混入 ASSET；第 S 条是真 ASSET。前 S-1 条都属于本地
    // integrated 连续前缀（只借生产代码信任的"已消费"不变量，不读资产），
    // 唯一缺口是尾部 ASSET → prepare 落 pending receipt，validate 必须下载实证。
    let mut asset_events = 0usize;
    for seq in 1..total_events {
        if seq % SCAN_COST_ASSET_EVERY == 0 {
            relay.publish_event(RelayLogKind::Asset, [0x2a; 32]);
            asset_events += 1;
        } else {
            relay.publish_event(RelayLogKind::Update, [0x15; 32]);
        }
    }
    let (_, tail_seq) = publish_real_asset(
        &mut relay,
        VAULT_ID,
        materials.device_id,
        &materials.signing_key,
        &materials.root,
        b"coverage scan cost tail asset",
        "text/plain",
    );
    assert_eq!(
        tail_seq,
        u64::try_from(total_events).expect("S 溢出 u64"),
        "尾部 ASSET 必须恰为第 S 条日志"
    );
    asset_events += 1;
    let update_events = total_events - asset_events;
    assert!(
        (update_events) > 0,
        "混合日志必须至少有一条 UPDATE（实际 S={total_events}）"
    );

    let prefix = u64::try_from(total_events - 1).expect("S 溢出 u64");
    vault
        .set_meta(META_SYNC_INTEGRATED_SEQ, &prefix.to_be_bytes())
        .expect("写 integrated 前缀");
    vault
        .set_meta(META_SYNC_RECEIVED_SEQ, &prefix.to_be_bytes())
        .expect("写 received 前缀");

    // 从这里开始的页请求才算探针成本（prepare 的 pending receipt 扫描也是 O(S)）。
    let mut relay = CountingRelay::new(relay);
    let prepared = {
        let mut writer = CheckpointWriter::new(
            &vault,
            &mut relay,
            VAULT_ID,
            materials.device_id,
            materials.signing_key.clone(),
            materials.root.clone(),
            materials.peer_id_u64(),
        );
        writer.prepare(&EmptyAssets).expect("prepare")
    };
    assert_eq!(
        prepared.report.covered_seq,
        u64::try_from(total_events).expect("S 溢出 u64")
    );
    assert_eq!(
        relay.log_calls,
        u64::try_from(total_events.div_ceil(SCAN_COST_PAGE_LIMIT)).expect("页数溢出 u64"),
        "prepare 的 receipt 扫描应从 c0 扫满 S/256 页"
    );
    let prepare_pages = relay.log_calls;
    let prepare_scan_ms = relay.scan_span_ms();
    let prepare_log_ms = relay.log_busy_ms();
    relay.reset_counters();

    ScanCostFixture {
        _dir: dir,
        vault,
        materials,
        relay,
        prepared,
        asset_events,
        update_events,
        prepare_pages,
        prepare_scan_ms,
        prepare_log_ms,
    }
}

fn median_ms(values: &[f64]) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).expect("时间不能是 NaN"));
    sorted[sorted.len() / 2]
}

fn run_scan_cost(total_events: usize) {
    let mut fixture = build_scan_cost_fixture(total_events);
    let asset_pct = fixture.asset_events as f64 * 100.0 / total_events as f64;
    eprintln!(
        "[phase1a] scan_cost setup S={total_events} asset_events={} update_events={} \
         asset_pct={asset_pct:.1} prepare_pages={} prepare_scan_ms={:.3} prepare_log_ms={:.3}",
        fixture.asset_events,
        fixture.update_events,
        fixture.prepare_pages,
        fixture.prepare_scan_ms,
        fixture.prepare_log_ms,
    );

    let mut pages = 0u64;
    let mut part_calls = 0u64;
    let mut raw_scan = Vec::with_capacity(SCAN_COST_RUNS);
    let mut raw_validate = Vec::with_capacity(SCAN_COST_RUNS);
    let mut raw_log = Vec::with_capacity(SCAN_COST_RUNS);
    let mut raw_part = Vec::with_capacity(SCAN_COST_RUNS);
    for run in 0..SCAN_COST_RUNS {
        fixture.relay.reset_counters();
        let started = Instant::now();
        let baseline = {
            let mut writer = CheckpointWriter::new(
                &fixture.vault,
                &mut fixture.relay,
                VAULT_ID,
                fixture.materials.device_id,
                fixture.materials.signing_key.clone(),
                fixture.materials.root.clone(),
                fixture.materials.peer_id_u64(),
            );
            writer.validate(&fixture.prepared).expect("validate")
        };
        let validate_ms = started.elapsed().as_secs_f64() * 1e3;
        assert_eq!(
            baseline.covered_seq(),
            u64::try_from(total_events).expect("S 溢出 u64")
        );
        assert_eq!(
            baseline.coverage_verified_candidates(),
            1,
            "尾部 ASSET 必须被真实下载验证（receipt 路径）"
        );
        assert_eq!(baseline.coverage_durable_assets(), 0);

        let run_pages = fixture.relay.log_calls;
        let scan_ms = fixture.relay.scan_span_ms();
        let log_ms = fixture.relay.log_busy_ms();
        let run_parts = fixture.relay.part_calls;
        let part_ms = fixture.relay.part_busy_ms();
        assert_eq!(
            run_pages,
            u64::try_from(total_events.div_ceil(SCAN_COST_PAGE_LIMIT)).expect("页数溢出 u64"),
            "validate 的 coverage scan 应从 c0 扫满 S/256 页"
        );
        assert!(run_parts >= 1, "validate 至少读回 manifest/快照/尾部 ASSET");
        pages = run_pages;
        part_calls = run_parts;
        eprintln!(
            "[phase1a] scan_cost S={total_events} run={run} pages={run_pages} \
             scan_ms={scan_ms:.3} validate_ms={validate_ms:.3} log_ms={log_ms:.3} \
             part_calls={run_parts} part_ms={part_ms:.3}",
        );
        raw_scan.push(scan_ms);
        raw_validate.push(validate_ms);
        raw_log.push(log_ms);
        raw_part.push(part_ms);
    }

    let scan_median = median_ms(&raw_scan);
    let validate_median = median_ms(&raw_validate);
    let log_median = median_ms(&raw_log);
    eprintln!(
        "[phase1a] scan_cost S={total_events} pages={pages} scan_ms={scan_median:.3} \
         (median of {SCAN_COST_RUNS}; raw_scan_ms={raw_scan:?} log_ms_median={log_median:.3} \
         validate_ms_median={validate_median:.3} validate_ms_raw={raw_validate:?} \
         part_calls={part_calls} part_ms_raw={raw_part:?})"
    );
}

#[test]
#[ignore = "manual benchmark"]
fn checkpoint_validate_scan_cost() {
    for total_events in SCAN_COST_S {
        run_scan_cost(total_events);
    }
}

`````



@@SECTION code/096 `crates/sync/tests/phase0_catchup.rs`


`````rust
//! Phase 0 → 1A：两设备「落后追赶」的计数/耗时基准（手动运行，不 assert 时间）。
//!
//! 场景：设备 A 先产生 N 条正文 update 并推到 relay；设备 B 从 c0（很早的游标）开始
//! 逐页 `pull → integrate`，直到 relay 日志被追平。对同一份日志跑三种模式：
//!
//! - `per_row`：`inbox_batch=false` + `IntegrateConfig { aggregate_body: false }`
//!   （Phase 0 的逐行路径：每条 update 一次 `store_inbox_verified` / 一次
//!   `load_body_doc` 全量重放 / 一次 A 事务 + `mark_integrated`）；
//! - `batch`：`inbox_batch=true` + `aggregate_body: false`
//!   （Phase 0 的批量 inbox + 逐行集成）；
//! - `aggregate`：`inbox_batch=true` + `aggregate_body: true`
//!   （Phase 1A 目标：整页 verified 行一个事务 + 同一文档一批只全量重放一次）。
//!
//! 只比较**结果一致**（事件数、字节、游标、水位、最终正文）与耗时，不对时间做断言。
//! 集成一律走 [`SyncEngine::integrate_dependency_ordered`]，否则无法关闭聚合
//! （`integrate(limit)` 默认 `aggregate_body = true`）。
//!
//! ```bash
//! cargo test --release -p menghunote-sync --test phase0_catchup -- --ignored --nocapture
//! ```
//!
//! 不 sleep、不读真实网络：`FakeRelay` 是纯内存 relay，耗时反映本地 SQLite/Loro 成本。

mod support;

use std::time::Instant;

use support::*;

use menghunote_storage::{load_body_doc, sync_cursor_state, BodyDoc};
use menghunote_sync::engine::{
    EngineConfig, EngineIdentity, IntegrateConfig, PullReport, SyncEngine,
};

/// 单文档场景每个 update 的正文片段（约 64B，贴近"小编辑"；不是 4KB 大块）。
fn edit_text(index: usize) -> String {
    format!("-{index:05}-{}", "x".repeat(56))
}

/// 多文档场景每个 update 的正文片段（439B）。
///
/// 20 篇 × 250 条 × 439B ≈ 107 KiB/篇（> 100 KB），同时保持 update 条数 5000 不变。
fn edit_text_long(index: usize) -> String {
    format!("-{index:05}-{}", "y".repeat(432))
}

/// 集成预算：沿用旧 `integrate(1_000_000)` 的语义（`limit` 只折算批次上限，
/// 本场景每页最多 256 行）。
const INTEGRATE_LIMIT: usize = 1_000_000;

fn doc_id(index: usize) -> String {
    format!("doc-{index:02}")
}

/// 三种追赶模式。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    PerRow,
    Batch,
    Aggregate,
}

impl Mode {
    const ALL: [Mode; 3] = [Mode::PerRow, Mode::Batch, Mode::Aggregate];

    fn name(self) -> &'static str {
        match self {
            Self::PerRow => "per_row",
            Self::Batch => "batch",
            Self::Aggregate => "aggregate",
        }
    }

    fn inbox_batch(self) -> bool {
        !matches!(self, Self::PerRow)
    }

    fn aggregate_body(self) -> bool {
        matches!(self, Self::Aggregate)
    }

    fn engine_config(self) -> EngineConfig {
        EngineConfig {
            inbox_batch: self.inbox_batch(),
            ..EngineConfig::default()
        }
    }

    /// `integrate_dependency_ordered` 的配置；用旧的 limit 折算批数上限，再显式
    /// 打开/关闭正文聚合。
    fn integrate_config(self) -> IntegrateConfig {
        let mut config = IntegrateConfig::with_object_budget(INTEGRATE_LIMIT);
        config.aggregate_body = self.aggregate_body();
        config
    }
}

/// 单个配置下的一次追赶统计。
#[derive(Debug, Default)]
struct CaseStats {
    rounds: usize,
    events: usize,
    stored: usize,
    skipped: usize,
    integrated: usize,
    update_bytes: u64,
    asset_bytes: u64,
    pull_elapsed_ms: u64,
    integrate_elapsed_ms: u64,
    pull_wall_ms: u128,
    integrate_wall_ms: u128,
    doc_loads: usize,
    batches: usize,
    max_deferred: usize,
    max_page_events: usize,
}

impl CaseStats {
    fn total_wall_ms(&self) -> u128 {
        self.pull_wall_ms + self.integrate_wall_ms
    }
}

fn engine_identity(device: &Device) -> EngineIdentity {
    EngineIdentity {
        device_id: device.identity.device_id,
        signing_key: device.identity.signing_key.clone(),
        peer_id: device.identity.peer_id,
    }
}

/// 跑一次完整追赶，返回统计（内部断言结果正确，只不测时间）。
///
/// `docs` 篇文档、每篇 `updates_per_doc` 条 update（总计 `docs * updates_per_doc`）。
/// A 按 **doc 轮转** 提交（每轮每篇一条），并且**每轮提交后立刻推送**：生产
/// `outbox_rows` 按 `doc_id,seq` 排序，只有"每篇每轮恰好一条 pending"时 relay 日志
/// 才会保持逐轮、跨文档交错的顺序（否则会先集中推完第一篇）。
fn run_case(
    mode: Mode,
    docs: usize,
    updates_per_doc: usize,
    text_for: fn(usize) -> String,
    scenario: &str,
) -> CaseStats {
    ensure_test_identity_wrap();
    let (_a_dir, vault_a) = fresh_vault();
    let (_b_dir, vault_b) = fresh_vault();
    let device_a = Device::new(0x61);
    let device_b = Device::new(0x62);
    let mut relay = FakeRelay::new(&[&device_a, &device_b]);
    let updates = docs * updates_per_doc;

    // ---- A：docs 篇文档，按 doc 轮转提交 + 每轮推送 ----
    let docs_a: Vec<BodyDoc> = (0..docs)
        .map(|doc| seed_doc(&vault_a, &doc_id(doc), device_a.identity.peer_id))
        .collect();
    let mut positions = vec![0usize; docs];
    let mut published = 0usize;
    let mut commit_index = 0usize;
    {
        // 引擎持有 `&mut relay`；本轮内只做加密/推送，不再借 relay。
        let mut engine = SyncEngine::new(
            &vault_a,
            &mut relay,
            engine_identity(&device_a),
            device_a.root.clone(),
        );
        for _round in 0..updates_per_doc {
            for doc in 0..docs {
                commit_index += 1;
                let text = text_for(commit_index);
                docs_a[doc]
                    .insert_scalars(positions[doc], &text)
                    .expect("A 写正文");
                positions[doc] += text.chars().count();
                let mut object_id = [0u8; 32];
                object_id[..8].copy_from_slice(&(commit_index as u64).to_be_bytes());
                object_id[31] = 0x61;
                commit_with_intent(
                    &vault_a,
                    &docs_a[doc],
                    &uuid_like(commit_index as u64),
                    object_id,
                    EPOCH,
                );
            }
            // 本轮恰好 docs 条 pending（每篇一条），推送顺序 = doc_id 升序 = 轮转顺序。
            let encrypted = engine.encrypt_pending(256).expect("A 加密");
            let push = engine.push().expect("A 推送");
            assert_eq!(push.acked, encrypted, "本轮推送必须全部 durable ACK");
            assert_eq!(encrypted, docs, "每轮必须恰好加密本轮 {docs} 条");
            published += encrypted;
        }
    }
    assert_eq!(published, updates, "A 必须推完所有 update");
    let head_seq = relay.head_seq;
    assert_eq!(head_seq, updates as u64);

    // ---- B：从 c0 逐页追赶 ----
    for doc in 0..docs {
        seed_doc(&vault_b, &doc_id(doc), device_b.identity.peer_id);
    }
    seed_cursor(&vault_b);
    let mut stats = CaseStats::default();
    loop {
        // 本轮开始前的历史行数（探针重放的就是这段历史）。
        let history_before = sync_cursor_state(&vault_b)
            .map(|state| state.integrated_seq)
            .unwrap_or(0);
        // 探针（不计入下面两段计时）：一次完整重放第 0 篇文档历史的成本。
        // integrate 的每个 remote import 都会走 `load_body_doc` → 全量重放，
        // 这个数字应当随轮次增长，用来解释 integrate 的超线性。
        let replay_started = Instant::now();
        let _ =
            load_body_doc(&vault_b, &doc_id(0), device_b.identity.peer_id, None).expect("重放探针");
        let replay_ms = replay_started.elapsed().as_millis();

        let pull_started = Instant::now();
        let pull: PullReport = {
            let mut sync = SyncEngine::with_config(
                &vault_b,
                &mut relay,
                engine_identity(&device_b),
                device_b.root.clone(),
                mode.engine_config(),
            );
            sync.pull().expect("B pull")
        };
        let pull_wall = pull_started.elapsed();
        stats.rounds += 1;
        stats.pull_wall_ms += pull_wall.as_millis();
        stats.pull_elapsed_ms += pull.elapsed_ms;
        stats.events += pull.events;
        stats.stored += pull.stored;
        stats.skipped += pull.skipped;
        stats.update_bytes += pull.update_bytes;
        stats.asset_bytes += pull.asset_bytes;
        stats.max_page_events = stats.max_page_events.max(pull.events);

        let integrate_started = Instant::now();
        let integrate = {
            let mut sync = SyncEngine::with_config(
                &vault_b,
                &mut relay,
                engine_identity(&device_b),
                device_b.root.clone(),
                mode.engine_config(),
            );
            // Phase 1A：必须走 dependency_ordered + 显式 IntegrateConfig，
            // 否则无法关闭正文聚合，per_row/batch 就不是旧路径。
            sync.integrate_dependency_ordered(mode.integrate_config())
                .expect("B integrate")
        };
        let integrate_wall = integrate_started.elapsed();
        let report = integrate.report;
        stats.integrate_wall_ms += integrate_wall.as_millis();
        stats.integrate_elapsed_ms += report.elapsed_ms;
        stats.integrated += report.integrated;
        stats.doc_loads += report.doc_loads;
        stats.batches += report.batches;
        stats.max_deferred = stats.max_deferred.max(report.deferred);
        eprintln!(
            "[phase1a] scenario={scenario} mode={} docs={} updates={} round={} history_rows={} \
             replay_probe_ms={} events={} stored={} skipped={} update_bytes={} pull_ms={} \
             integrate_ms={} doc_loads={} batches={} integrated={} deferred={}",
            mode.name(),
            docs,
            updates,
            stats.rounds,
            history_before,
            replay_ms,
            pull.events,
            pull.stored,
            pull.skipped,
            pull.update_bytes,
            pull.elapsed_ms,
            report.elapsed_ms,
            report.doc_loads,
            report.batches,
            report.integrated,
            report.deferred
        );
        if pull.events == 0 {
            assert_eq!(report.integrated, 0, "空页之后不应再有可集成行");
            break;
        }
    }

    // ---- 结果正确性（与时间无关的硬断言）----
    let state = sync_cursor_state(&vault_b).expect("B 游标状态");
    assert_eq!(state.received_seq, head_seq, "B 必须收到全部事件");
    assert_eq!(state.integrated_seq, head_seq, "B 必须集成全部事件");
    assert!(stats.events >= updates, "事件数至少等于 updates");
    assert_eq!(
        stats.events, updates,
        "本场景没有 ASSET，事件数应等于 updates"
    );
    assert_eq!(stats.stored, updates);
    assert_eq!(stats.skipped, 0);
    assert!(stats.update_bytes > 0, "Update 明文字节必须 > 0");
    assert_eq!(stats.asset_bytes, 0, "本场景没有 ASSET");
    assert_eq!(stats.integrated, updates, "所有事件都必须被集成");
    assert_eq!(
        stats.max_deferred, 0,
        "每轮 integrate 后都不得留下 deferred"
    );

    for doc in 0..docs {
        let (body_a, _) = load_body_doc(&vault_a, &doc_id(doc), device_a.identity.peer_id, None)
            .expect("A 正文重放");
        let (body_b, _) = load_body_doc(&vault_b, &doc_id(doc), device_b.identity.peer_id, None)
            .expect("B 正文重放");
        assert_eq!(
            body_a.text(),
            body_b.text(),
            "追赶后 B 第 {doc} 篇正文必须与 A 一致"
        );
    }
    stats
}

fn print_summary(scenario: &str, mode: Mode, docs: usize, updates: usize, stats: &CaseStats) {
    eprintln!(
        "[phase1a] summary scenario={scenario} mode={} docs={} updates={} rounds={} events={} \
         update_bytes={} pull_ms(report)={} pull_ms(wall)={} integrate_ms(report)={} \
         integrate_ms(wall)={} doc_loads={} batches={} deferred={} max_page_events={} total_ms={}",
        mode.name(),
        docs,
        updates,
        stats.rounds,
        stats.events,
        stats.update_bytes,
        stats.pull_elapsed_ms,
        stats.pull_wall_ms,
        stats.integrate_elapsed_ms,
        stats.integrate_wall_ms,
        stats.doc_loads,
        stats.batches,
        stats.max_deferred,
        stats.max_page_events,
        stats.total_wall_ms()
    );
}

/// 以 per_row 为基准打印同一场景下其它模式的加速比（只展示，不断言）。
fn print_speedups(scenario: &str, updates: usize, results: &[(Mode, CaseStats)]) {
    let Some((_, baseline)) = results.iter().find(|(mode, _)| *mode == Mode::PerRow) else {
        return;
    };
    for (mode, stats) in results {
        if *mode == Mode::PerRow {
            continue;
        }
        let pull_speedup = baseline.pull_wall_ms as f64 / stats.pull_wall_ms.max(1) as f64;
        let integrate_speedup =
            baseline.integrate_wall_ms as f64 / stats.integrate_wall_ms.max(1) as f64;
        let total_speedup = baseline.total_wall_ms() as f64 / stats.total_wall_ms().max(1) as f64;
        let doc_load_ratio = stats.doc_loads as f64 / baseline.doc_loads.max(1) as f64;
        eprintln!(
            "[phase1a] speedup scenario={scenario} updates={updates} mode={} \
             baseline=per_row pull_speedup={pull_speedup:.2}x \
             integrate_speedup={integrate_speedup:.2}x total_speedup={total_speedup:.2}x \
             doc_loads_ratio={doc_load_ratio:.4} (baseline_total={}ms mode_total={}ms)",
            mode.name(),
            baseline.total_wall_ms(),
            stats.total_wall_ms()
        );
    }
}

fn run_scenario(
    scenario: &str,
    docs: usize,
    updates_per_doc: usize,
    text_for: fn(usize) -> String,
) {
    let updates = docs * updates_per_doc;
    eprintln!(
        "[phase1a] scenario={scenario} begin docs={docs} updates={updates} \
         updates_per_doc={updates_per_doc}"
    );
    let mut results = Vec::with_capacity(Mode::ALL.len());
    for mode in Mode::ALL {
        let stats = run_case(mode, docs, updates_per_doc, text_for, scenario);
        print_summary(scenario, mode, docs, updates, &stats);
        results.push((mode, stats));
    }
    print_speedups(scenario, updates, &results);
}

#[test]
#[ignore = "manual benchmark"]
fn phase1a_two_device_catchup_matrix() {
    // 1) 单文档 600 / 2000（沿用 Phase 0 的 ~64B 小编辑）。
    for edits in [600usize, 2_000] {
        run_scenario("single", 1, edits, edit_text);
    }

    // 2) 5000 条 / 20 篇多文档（每篇 250 条 × 439B ≈ 107 KiB 正文）。
    assert_eq!(edit_text_long(1).chars().count(), 439);
    assert!(edit_text_long(1).chars().count() * 250 >= 100 * 1024);
    run_scenario("multi", 20, 250, edit_text_long);

    // 3) 可选：5000 条单文档（同一文档的 O(历史 × 事件) 最坏情形）。
    run_scenario("single", 1, 5_000, edit_text);
}

`````



@@SECTION code/097 `crates/sync/tests/asset_sync.rs`


`````rust
//! 普通同步的增量资产上传（FakeRelay + 真 SQLite，不 sleep）。
//!
//! 覆盖 attachment-insert 缺口：A 插入并引用新资产后，普通 `sync_once`（非发布基线）
//! 就应把 ASSET 对象增量上传，B 能取到与 A 一致的字节；第二个同步轮次不重传；
//! 多片资产传到一半失败后从缺失片续传；checkpoint 已上传的对象被普通同步直接复用/标记；
//! 被维护 reservation 持有的对象延后（ID_CONFLICT 不阻塞正文）；本地字节与元数据 hash
//! 不一致时必须上抛 `LocalCorrupt`（不能伪装成可重试失败）。

mod support;

use support::*;

use menghunote_storage::sync_cursor_state;
use menghunote_storage::{
    asset_bytes, asset_content_hash, count_unreferenced_assets, insert_asset, link_asset_ref,
    Value, Vault, META_SYNC_LOG_ID,
};
use menghunote_sync::checkpoint::asset_object_id;
use menghunote_sync::engine::{EngineIdentity, SyncEngine};
use menghunote_sync::SyncError;
use menghunote_sync_proto::{cap_plain, part_count_for, ObjectType};

const ASSET_MEDIA: &str = "text/plain";

fn engine<'a>(
    vault: &'a Vault,
    relay: &'a mut FakeRelay,
    device: &Device,
) -> SyncEngine<'a, FakeRelay> {
    let identity = EngineIdentity {
        device_id: device.identity.device_id,
        signing_key: device.identity.signing_key.clone(),
        peer_id: device.identity.peer_id,
    };
    SyncEngine::new(vault, relay, identity, device.root.clone())
}

/// 建一篇 note 行 + 引用的资产（`asset_refs`），返回资产 hash。
fn seed_referenced_asset(vault: &Vault, note_id: &str, peer: u64, data: &[u8]) -> [u8; 32] {
    let _doc = seed_doc(vault, note_id, peer);
    menghunote_storage::create_note_slot(vault, note_id, note_id, note_id, 1_700_000_000)
        .expect("notes 行");
    let hash = asset_content_hash(data);
    insert_asset(vault, &hash, data.len() as u64, Some(ASSET_MEDIA), data).expect("insert_asset");
    link_asset_ref(vault, note_id, &hash, 0, 1).expect("link_asset_ref");
    hash
}

/// 给 pull 准备好 durable 的 log_id + 游标（真实流程由 bootstrap 写）。
fn seed_pull_state(vault: &Vault, relay: &FakeRelay) {
    vault
        .set_meta(META_SYNC_LOG_ID, &relay.log_id)
        .expect("log_id");
    seed_cursor(vault);
    assert_eq!(sync_cursor_state(vault).expect("cursor").received_seq, 0);
}

fn marker_rows(vault: &Vault) -> Vec<(String, Vec<u8>)> {
    vault
        .query(
            "SELECT key,value FROM meta WHERE substr(key,1,?1)=?2 ORDER BY key",
            &[
                Value::Integer("sync.asset_uploaded.".len() as i64),
                Value::Text("sync.asset_uploaded.".to_owned()),
            ],
        )
        .expect("读 asset marker")
        .iter()
        .map(|row| {
            let key = match row.first() {
                Some(Value::Text(key)) => key.clone(),
                _ => panic!("marker key 类型非法"),
            };
            let value = match row.get(1) {
                Some(Value::Blob(value)) => value.clone(),
                _ => panic!("marker value 类型非法"),
            };
            (key, value)
        })
        .collect()
}

fn wipe_markers(vault: &Vault) {
    vault
        .exec(
            "DELETE FROM meta WHERE substr(key,1,?1)=?2",
            &[
                Value::Integer("sync.asset_uploaded.".len() as i64),
                Value::Text("sync.asset_uploaded.".to_owned()),
            ],
        )
        .expect("清 asset marker");
    assert!(marker_rows(vault).is_empty());
}

fn object_id(device: &Device, hash: &[u8; 32], data: &[u8]) -> [u8; 32] {
    asset_object_id(&device.root, hash, data.len() as u64, ASSET_MEDIA).expect("asset object_id")
}

fn put_count(relay: &FakeRelay, object_id: &[u8; 32]) -> usize {
    relay
        .put_history
        .iter()
        .filter(|(recorded, _, _)| recorded == object_id)
        .count()
}

#[test]
fn ordinary_sync_uploads_referenced_asset_once_and_peer_receives_bytes() {
    ensure_test_identity_wrap();
    let (_dir, vault) = fresh_vault();
    let device = Device::new(0x41);
    let mut relay = FakeRelay::new(&[&device]);
    let data = b"incremental asset bytes over ordinary sync";
    let hash = seed_referenced_asset(&vault, "note-asset", device.identity.peer_id, data);
    // 无正文引用的历史孤儿：不得被普通同步顺带上传。
    let orphan = b"orphan asset bytes";
    insert_asset(
        &vault,
        &asset_content_hash(orphan),
        orphan.len() as u64,
        Some(ASSET_MEDIA),
        orphan,
    )
    .expect("orphan insert");
    seed_pull_state(&vault, &relay);
    let object_id = object_id(&device, &hash, data);

    let first = {
        let mut sync = engine(&vault, &mut relay, &device);
        sync.sync_once().expect("普通同步")
    };
    assert_eq!(first.assets.referenced, 1);
    assert_eq!(first.assets.uploaded, 1, "普通 sync 必须增量上传新资产");
    assert_eq!(first.assets.already_uploaded, 0);
    assert_eq!(first.assets.failed, 0);
    assert_eq!(first.assets.deferred, 0);
    assert!(first.assets.parts_uploaded >= 1);
    assert_eq!(
        first.assets.bytes_uploaded,
        data.len() as u64 + 42 + ASSET_MEDIA.len() as u64,
        "单分片资产的上传字节 = canonical 明文头 + 数据"
    );
    assert_eq!(
        first.assets.orphans_skipped,
        count_unreferenced_assets(&vault).expect("孤儿计数"),
        "报告必须如实给孤儿策略"
    );
    assert_eq!(marker_rows(&vault).len(), 1, "成功后必须落 durable 标记");
    let fact = &marker_rows(&vault)[0].0;
    assert!(fact.ends_with(&menghunote_sync::ids::hex_of(&object_id)));
    assert_eq!(
        relay
            .log
            .iter()
            .filter(|event| event.kind == menghunote_sync::relay::RelayLogKind::Asset)
            .count(),
        1,
        "ASSET 提交必须产生一条资产日志事件"
    );
    assert_eq!(
        relay
            .objects
            .get(&object_id)
            .map(|object| object.commit.object_type),
        Some(ObjectType::Asset)
    );
    assert_eq!(
        part_count_for(
            data.len() as u64 + 42 + ASSET_MEDIA.len() as u64,
            ObjectType::Asset
        ),
        1,
        "单分片夹具"
    );

    // 第二轮：标记命中，不读字节、不发请求、不重传。
    let puts = relay.put_history.len();
    let gets = relay.part_gets.len();
    let second = {
        let mut sync = engine(&vault, &mut relay, &device);
        sync.sync_once().expect("第二次同步")
    };
    assert_eq!(second.assets.uploaded, 0);
    assert_eq!(second.assets.already_uploaded, 1);
    assert_eq!(second.assets.failed, 0);
    assert_eq!(second.assets.deferred, 0);
    assert_eq!(relay.put_history.len(), puts, "不得重复上传分片");
    assert_eq!(relay.part_gets.len(), gets, "标记命中时不得再探测 relay");

    // B（另一台设备、独立库、同一根）：pull 后取到逐字节一致的资产。
    let (_b_dir, b_vault) = fresh_vault();
    let b_device = Device::new(0x42);
    relay.certs.insert(b_device.device_id, b_device.cert());
    seed_pull_state(&b_vault, &relay);
    let b_report = {
        let mut sync_b = engine(&b_vault, &mut relay, &b_device);
        sync_b.pull().expect("B pull")
    };
    assert_eq!(b_report.skipped, 1, "B 必须消费 ASSET 事件");
    assert_eq!(
        asset_bytes(&b_vault, hash).expect("读 B 资产"),
        Some(data.to_vec()),
        "B 的资产字节必须与 A 完全一致"
    );
    // B 落盘后也应有 durable 已上传标记（避免 B 以后重复上传/探测）。
    assert_eq!(marker_rows(&b_vault).len(), 1);

    // 再拉一页：没有新事件，也不改写 durable 事实（同一 seq 的幂等由 engine 既有用例覆盖）。
    let skipped_before = vault_skipped_records(&b_vault);
    {
        let mut sync_b = engine(&b_vault, &mut relay, &b_device);
        let replay = sync_b.pull().expect("B 再拉一页");
        assert_eq!(replay.events, 0);
        assert_eq!(replay.skipped, 0);
    }
    assert_eq!(vault_skipped_records(&b_vault), skipped_before);
    assert_eq!(marker_rows(&b_vault).len(), 1, "标记不得被重放改写");
}

fn vault_skipped_records(vault: &Vault) -> usize {
    vault
        .query(
            "SELECT COUNT(*) FROM meta WHERE substr(key,1,?1)=?2",
            &[
                Value::Integer("sync.skipped_asset.".len() as i64),
                Value::Text("sync.skipped_asset.".to_owned()),
            ],
        )
        .expect("读 skipped-asset 记录数")
        .first()
        .and_then(|row| row.first())
        .and_then(|value| match value {
            Value::Integer(value) => usize::try_from(*value).ok(),
            _ => None,
        })
        .expect("计数")
}

#[test]
fn partial_asset_upload_resumes_missing_parts_without_retransmitting_durable_ones() {
    ensure_test_identity_wrap();
    let (_dir, vault) = fresh_vault();
    let device = Device::new(0x51);
    let mut relay = FakeRelay::new(&[&device]);
    let cap = cap_plain(ObjectType::Asset) as usize;
    let data = vec![0x5a; cap + 17];
    let hash = seed_referenced_asset(&vault, "note-multipart", device.identity.peer_id, &data);
    seed_pull_state(&vault, &relay);
    let object_id = object_id(&device, &hash, &data);
    let total = data.len() as u64 + 42 + ASSET_MEDIA.len() as u64;
    assert_eq!(part_count_for(total, ObjectType::Asset), 2, "两片夹具");

    // 第 1 片（第二片）注入一次失败：第 0 片已 durable。
    relay.fail_put_parts.insert((object_id, 1), "TRANSPORT");
    let first = {
        let mut sync = engine(&vault, &mut relay, &device);
        sync.sync_once().expect("正文同步不得被资产失败阻塞")
    };
    assert_eq!(first.assets.uploaded, 0);
    assert_eq!(first.assets.failed, 1, "单资产失败必须如实计入报告");
    assert!(relay.parts.contains_key(&(object_id, 0)));
    assert!(!relay.parts.contains_key(&(object_id, 1)));
    assert!(marker_rows(&vault).is_empty(), "未完成不得落标记");
    assert_eq!(put_count(&relay, &object_id), 1);

    // 第二轮：从缺失片续传，已 durable 的第 0 片不重传。
    let second = {
        let mut sync = engine(&vault, &mut relay, &device);
        sync.sync_once().expect("续传")
    };
    assert_eq!(second.assets.uploaded, 1);
    assert_eq!(second.assets.failed, 0);
    assert_eq!(second.assets.parts_uploaded, 1, "只补缺失片");
    assert_eq!(
        second.assets.bytes_uploaded, 69,
        "只计缺失片明文（17B 数据 + 42B 头 + 10B MIME）"
    );
    assert_eq!(put_count(&relay, &object_id), 2, "第 0 片只 PUT 过一次");
    assert!(relay.parts.contains_key(&(object_id, 1)));
    assert_eq!(marker_rows(&vault).len(), 1);
}

#[test]
fn reserved_asset_is_deferred_then_adopted_after_checkpoint_completes_it() {
    ensure_test_identity_wrap();
    let (_dir, vault) = fresh_vault();
    let device = Device::new(0x61);
    let mut relay = FakeRelay::new(&[&device]);
    let cap = cap_plain(ObjectType::Asset) as usize;
    let data = vec![0x33; cap + 5];
    let hash = seed_referenced_asset(&vault, "note-reserved", device.identity.peer_id, &data);
    seed_pull_state(&vault, &relay);
    let object_id = object_id(&device, &hash, &data);

    // 先完整上传一次，拿到确定性密文字节。
    {
        let mut sync = engine(&vault, &mut relay, &device);
        assert_eq!(sync.sync_once().expect("首次").assets.uploaded, 1);
    }
    let captured: Vec<(u32, Vec<u8>)> = relay
        .put_history
        .iter()
        .filter(|(recorded, _, _)| recorded == &object_id)
        .map(|(_, part, bytes)| (*part, bytes.clone()))
        .collect();
    assert_eq!(captured.len(), 2);

    // 模拟 checkpoint 正在用维护 reservation 传该对象：标记清掉、末片缺失、普通 PUT 409。
    wipe_markers(&vault);
    relay.parts.remove(&(object_id, 1));
    relay.reserved_objects.insert(object_id);
    let puts = relay.put_history.len();
    let deferred = {
        let mut sync = engine(&vault, &mut relay, &device);
        sync.sync_once().expect("资产延后不得阻塞正文")
    };
    assert_eq!(deferred.assets.deferred, 1);
    assert_eq!(deferred.assets.failed, 0);
    assert_eq!(deferred.assets.uploaded, 0);
    assert_eq!(relay.put_history.len(), puts, "冲突时不得写入任何分片");
    assert!(marker_rows(&vault).is_empty(), "延后不得落标记");
    assert!(!relay.parts.contains_key(&(object_id, 1)));

    // checkpoint 完成：末片出现、reservation 释放。普通同步只提交+落标记，不重传。
    relay.reserved_objects.remove(&object_id);
    relay.parts.insert(
        (object_id, 1),
        captured
            .iter()
            .find(|(part, _)| *part == 1)
            .expect("第 1 片")
            .1
            .clone(),
    );
    let puts = relay.put_history.len();
    let adopted = {
        let mut sync = engine(&vault, &mut relay, &device);
        let report = sync.sync_once().expect("复用 checkpoint 已传对象");
        assert!(report.pull.next_cursor.starts_with('c'));
        report
    };
    assert_eq!(adopted.assets.reused_existing, 1);
    assert_eq!(adopted.assets.uploaded, 0);
    assert_eq!(adopted.assets.deferred, 0);
    assert_eq!(adopted.assets.failed, 0);
    assert_eq!(relay.put_history.len(), puts, "已完整对象不得重传分片");
    assert_eq!(marker_rows(&vault).len(), 1, "复用后必须落标记");
}

#[test]
fn unresolvable_asset_conflict_is_reported_not_fatal_for_body_sync() {
    ensure_test_identity_wrap();
    let (_dir, vault) = fresh_vault();
    let device = Device::new(0x71);
    let mut relay = FakeRelay::new(&[&device]);
    let data = b"conflicting asset";
    let hash = seed_referenced_asset(&vault, "note-conflict", device.identity.peer_id, data);
    seed_pull_state(&vault, &relay);
    let object_id = object_id(&device, &hash, data);
    relay.reserved_objects.insert(object_id);

    let report = {
        let mut sync = engine(&vault, &mut relay, &device);
        sync.sync_once().expect("正文同步必须完成")
    };
    assert_eq!(report.assets.failed, 1);
    assert_eq!(report.assets.uploaded, 0);
    assert_eq!(report.assets.deferred, 0);
    assert!(marker_rows(&vault).is_empty());
}

#[test]
fn missing_media_or_data_skips_asset_without_upload() {
    ensure_test_identity_wrap();
    let (_dir, vault) = fresh_vault();
    let device = Device::new(0x81);
    let mut relay = FakeRelay::new(&[&device]);
    seed_doc(&vault, "note-invalid", device.identity.peer_id);
    menghunote_storage::create_note_slot(
        &vault,
        "note-invalid",
        "note-invalid",
        "note-invalid",
        1_700_000_000,
    )
    .expect("notes 行");
    let data = b"no media type";
    let hash = asset_content_hash(data);
    insert_asset(&vault, &hash, data.len() as u64, None, data).expect("insert");
    link_asset_ref(&vault, "note-invalid", &hash, 0, 1).expect("link");
    seed_pull_state(&vault, &relay);

    let report = {
        let mut sync = engine(&vault, &mut relay, &device);
        sync.sync_once().expect("同步")
    };
    assert_eq!(report.assets.referenced, 1);
    assert_eq!(report.assets.skipped_invalid, 1);
    assert_eq!(report.assets.uploaded, 0);
    assert!(relay.put_history.is_empty());
    assert!(marker_rows(&vault).is_empty());
}

#[test]
fn asset_hash_mismatch_raises_local_corrupt_without_uploading_wrong_bytes() {
    ensure_test_identity_wrap();
    let (_dir, vault) = fresh_vault();
    let device = Device::new(0x91);
    let mut relay = FakeRelay::new(&[&device]);
    seed_doc(&vault, "note-tamper", device.identity.peer_id);
    menghunote_storage::create_note_slot(
        &vault,
        "note-tamper",
        "note-tamper",
        "note-tamper",
        1_700_000_000,
    )
    .expect("notes 行");
    let data = b"claimed bytes";
    let claimed = [0xEEu8; 32];
    // 直接写一条"hash 与 data 不匹配"的坏行（模拟本地损坏），确认不会上传。
    vault
        .exec(
            "INSERT INTO assets(asset_hash,byte_len,media_type,data) VALUES(?1,?2,'text/plain',?3)",
            &[
                Value::Blob(claimed.to_vec()),
                Value::Integer(data.len() as i64),
                Value::Blob(data.to_vec()),
            ],
        )
        .expect("坏资产行");
    link_asset_ref(&vault, "note-tamper", &claimed, 0, 1).expect("link");
    seed_pull_state(&vault, &relay);

    // P2-9：本地库损坏不是可重试的网络失败，必须**上抛**让调用方看到；原实现把它
    // 计入 `failed` 后继续，于是每轮都"重试成功"、永不报错。
    let error = {
        let mut sync = engine(&vault, &mut relay, &device);
        sync.sync_once()
            .expect_err("本地资产 hash 与字节不一致必须上抛 LocalCorrupt")
    };
    assert!(
        matches!(error, SyncError::LocalCorrupt("资产字节哈希与元数据不一致")),
        "实际：{error:?}"
    );
    // 整轮报错也不能把错误字节传给 relay、不能落 durable 标记。
    assert!(relay.put_history.is_empty());
    assert!(marker_rows(&vault).is_empty());
}

#[test]
fn received_prefix_advances_over_asset_event_before_update() {
    ensure_test_identity_wrap();
    let (_dir, vault) = fresh_vault();
    let device = Device::new(0xA1);
    let mut relay = FakeRelay::new(&[&device]);
    let data = b"asset event ordering";
    let hash = seed_referenced_asset(&vault, "note-order", device.identity.peer_id, data);
    seed_pull_state(&vault, &relay);

    // 第一轮：只上传资产（ASSET 事件排在 UPDATE 之前）。
    {
        let mut sync = engine(&vault, &mut relay, &device);
        let report = sync.sync_once().expect("首轮");
        assert_eq!(report.assets.uploaded, 1);
        assert_eq!(
            sync_cursor_state(&vault).expect("A cursor").received_seq,
            1,
            "ASSET 也必须推进 received 连续前缀"
        );
    }
    // 第二轮：正文更新 → UPDATE 事件排在 ASSET 之后。
    let (doc, _) =
        menghunote_storage::load_body_doc(&vault, "note-order", device.identity.peer_id, None)
            .expect("加载正文");
    doc.insert_scalars(0, "after asset").expect("写正文");
    commit_with_intent(&vault, &doc, &uuid_like(77), [0x77; 32], EPOCH);
    {
        let mut sync = engine(&vault, &mut relay, &device);
        let report = sync.sync_once().expect("次轮");
        assert_eq!(report.push.acked, 1);
    }
    assert_eq!(
        relay.log.iter().map(|event| event.kind).collect::<Vec<_>>(),
        vec![
            menghunote_sync::relay::RelayLogKind::Asset,
            menghunote_sync::relay::RelayLogKind::Update
        ],
        "夹具顺序必须是 ASSET 在前、UPDATE 在后"
    );

    // 新设备 B 一页同时收到 ASSET + UPDATE：received 不能卡在 ASSET 上，UPDATE 必须可集成。
    let (_b_dir, b_vault) = fresh_vault();
    let b_device = Device::new(0xA2);
    relay.certs.insert(b_device.device_id, b_device.cert());
    let _b_doc = seed_doc(&b_vault, "note-order", b_device.identity.peer_id);
    seed_pull_state(&b_vault, &relay);
    {
        let mut sync_b = engine(&b_vault, &mut relay, &b_device);
        let pull = sync_b.pull().expect("B pull");
        assert_eq!(pull.events, 2);
        assert_eq!(pull.stored, 1);
        assert_eq!(pull.skipped, 1);
    }
    assert_eq!(
        sync_cursor_state(&b_vault).expect("B cursor").received_seq,
        2,
        "B 的 received 前缀必须越过 ASSET 事件"
    );
    let integrated = {
        let mut sync_b = engine(&b_vault, &mut relay, &b_device);
        sync_b.integrate(16).expect("B integrate")
    };
    assert_eq!(integrated.integrated, 1);
    assert_eq!(integrated.deferred, 0);
    let text =
        menghunote_storage::load_body_doc(&b_vault, "note-order", b_device.identity.peer_id, None)
            .expect("读 B 正文")
            .0
            .text();
    assert!(text.contains("after asset"), "B 正文必须完成集成：{text:?}");
    assert_eq!(
        asset_bytes(&b_vault, hash).expect("读 B 资产"),
        Some(data.to_vec())
    );
}

`````



@@SECTION code/098 `crates/sync/tests/remote_init.rs`


`````rust
//! ADR 0013 **S3b**：远端 `remote_tls` Create / Join 在**真实 TLS + 真实 axum relay** 上的回归。
//!
//! 不连任何真实远端 relay：`menghunote-dev-relay` 的 `start_tls()` 现场生成自签 CA +
//! `localhost` 叶子证书、起 rustls 监听；客户端用 `RelayContext::with_test_extra_roots`
//! 显式注入该 CA（仍校验证书链/SAN/有效期，**没有** accept-invalid）。
//!
//! 覆盖：
//!
//! 1. A（NewGenesis）→ rev1 强 pin → 上传初始恢复槽 → 发布基线；
//! 2. B（FreshRecovery）→ 真实 TLS 上的自批 + 认证 bootstrap → record/bootstrap 同视图 pin；
//! 3. 失败负例：错 setup secret / 未信任 TLS / 错恢复码 → 不留本地库/pending；
//! 4. 崩溃点：请求已发 + 身份已写 + pin 未提交 → `reconcile` 在真实 TLS 上补齐并清 pending；
//! 5. `local_dev` context 不能进入远端入口（不静默降级）。

use std::path::{Path, PathBuf};

use menghunote_dev_relay::DevRelay;
use menghunote_storage::{list_tree, StructureWriter, SyncContext, Vault};
use menghunote_sync::control_pin::load_control_pin;
use menghunote_sync::dev_local::DevLocal;
use menghunote_sync::genesis::build_materials;
use menghunote_sync::http_transport::DEFAULT_TRANSPORT_TIMEOUTS;
use menghunote_sync::identity::LocalIdentityV1;
use menghunote_sync::init_journal::{
    begin_pending, load_pending, InitKind, InitPendingV1, InitPhase,
};
use menghunote_sync::init_session::{reconcile_with_extra_trust_roots, InitReconcileOutcome};
use menghunote_sync::random::{array, OsRandom};
use menghunote_sync::recovery::RecoverySeed;
use menghunote_sync::relay_context::RelayContext;
use menghunote_sync::remote_init::{create_remote, join_remote};
use menghunote_sync::SyncError;
use tempfile::{Builder as TempDirBuilder, TempDir};

/// 在 `target/tmp` 下建独立测试根目录（AGENTS.md §6：不用 %TEMP%）。
fn temp_root(prefix: &str) -> TempDir {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("remote-init-{prefix}-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("创建 target/tmp 下的测试根目录失败");
    TempDirBuilder::new()
        .prefix("case-")
        .tempdir_in(&root)
        .expect("在 target/tmp 下创建临时目录失败")
}

/// 生产 TLS context + 显式注入本地 harness 的自签 CA（只加信任根，不改校验策略）。
fn remote_context(relay: &DevRelay) -> RelayContext {
    let ca = relay.tls_ca_der().expect("必须是 TLS relay");
    let context = RelayContext::remote_tls(relay.base_url(), DEFAULT_TRANSPORT_TIMEOUTS)
        .expect("remote_tls context")
        .with_test_extra_roots(vec![ca.to_vec()])
        .expect("注入自签根");
    assert_eq!(
        context.extra_trust_root_count(),
        1,
        "只应注入 harness 的自签 CA（不关闭任何校验）"
    );
    context
}

/// 测试用"身份落盘"回调：只记录已调用，不写任何真实受保护文件。
fn ok_identity(_identity: &LocalIdentityV1) -> Result<(), SyncError> {
    Ok(())
}

/// 断言目录里没有任何库/边车文件（失败路径"不建库"）。
fn assert_no_vault_files(vault_path: &Path) {
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let mut path = vault_path.as_os_str().to_os_string();
        path.push(suffix);
        let path = PathBuf::from(path);
        assert!(!path.exists(), "失败路径不得留下库文件：{}", path.display());
    }
    assert!(
        load_pending_of(vault_path).is_none(),
        "失败路径不得留下可读的 pending（库文件都不应存在）"
    );
}

/// 尽力读 pending：库文件不存在时返回 `None`（不把"打不开"当成"有 pending"）。
fn load_pending_of(vault_path: &Path) -> Option<InitPhase> {
    if !vault_path.exists() {
        return None;
    }
    let vault = Vault::open(vault_path).ok()?;
    load_pending(&vault, None)
        .ok()
        .flatten()
        .map(|pending| pending.phase)
}

/// 在 A 的库里建一篇笔记（结构 CRDT；足以验证基线导入）。
fn seed_note(vault: &Vault, identity: &LocalIdentityV1, title: &str) {
    let peer = identity.loro_peer_id();
    let epoch = identity.current_epoch;
    let mut writer =
        StructureWriter::open_with_sync(vault, peer, Some(SyncContext::new(peer, epoch)))
            .expect("打开结构写入口");
    writer.doc().create_note(title, 1).expect("新建笔记");
    writer.commit("remote-init-seed").expect("结构提交");
}

/// A（NewGenesis）→ rev1 强 pin 的真实 TLS 全链路。
#[test]
fn remote_create_commits_a_rev1_pin_over_real_tls_and_clears_pending() {
    let relay = DevRelay::start_tls().expect("起进程内 TLS relay 失败");
    let dir = temp_root("create");
    let vault_path = dir.path().join("a/vault.sqlite3");
    let ctx = remote_context(&relay);

    let created = create_remote(
        ctx,
        relay.setup_secret(),
        &vault_path,
        "remote-init 测试实例",
        ok_identity,
    )
    .expect("远端 Create 失败");

    assert!(created.vault_id != [0u8; 16], "vault_id 必须非零");
    assert!(
        created.recovery_code.starts_with("mnrec1"),
        "恢复码格式不对"
    );
    assert_eq!(created.warning, None, "正常路径不应有告警");
    assert_eq!(
        load_pending(&created.vault, None).expect("读 pending"),
        None,
        "Done 之后 pending 必须清理"
    );

    let pin = load_control_pin(&created.vault, created.vault_id)
        .expect("读 pin")
        .expect("必须有 rev1 强 pin");
    assert!(pin.instance_id != [0u8; 16], "instance 非零");
    assert_eq!(pin.record.rev, 1, "新库第一条控制记录是 rev1");
    assert!(
        pin.record.hash.is_some(),
        "remote Create 必须提交带 record hash 的强 pin"
    );
    assert!(pin.bootstrap.is_none(), "genesis 还没有历史 bootstrap 锚");
    // 本地事实：结构行可读（空树），身份 pin 与 genesis 回执一致。
    assert!(list_tree(&created.vault).expect("读结构投影").is_empty());
    assert_eq!(created.identity.pinned_instance, pin.instance_id);
    assert_eq!(created.identity.control_rev, 1);
    assert!(created.identity.pinned_manager != [0u8; 32]);
}

/// A → 发布真实基线 → B（FreshRecovery）在真实 TLS 上加入，record/bootstrap 同视图。
#[test]
fn remote_join_imports_baseline_over_real_tls_with_same_view_pin() {
    let relay = DevRelay::start_tls().expect("起进程内 TLS relay 失败");
    let dir = temp_root("join");
    let a_path = dir.path().join("a/vault.sqlite3");
    let ctx = remote_context(&relay);

    let a = create_remote(
        ctx.clone(),
        relay.setup_secret(),
        &a_path,
        "remote-init A",
        ok_identity,
    )
    .expect("A Create 失败");
    seed_note(&a.vault, &a.identity, "A 的远端笔记");
    let published = DevLocal::publish_initial_pair_with_identity(&ctx, &a.vault, &a.identity)
        .expect("发布基线");
    assert!(published.validated, "初始基线必须真实读回/导入通过");

    let b_path = dir.path().join("b/vault.sqlite3");
    let b =
        join_remote(ctx, a.vault_id, &a.recovery_code, &b_path, ok_identity).expect("B Join 失败");

    assert_eq!(
        load_pending(&b.vault, None).expect("读 pending"),
        None,
        "Join 完成后 pending 必须清理"
    );
    let pin = load_control_pin(&b.vault, a.vault_id)
        .expect("读 pin")
        .expect("B 必须有 durable pin");
    assert!(
        pin.record.rev >= 1,
        "B 的 record rev 至少覆盖 genesis（enroll 可能推进控制链）"
    );
    assert!(pin.record.hash.is_some(), "Join 必须提交 record 链尾 hash");
    let bootstrap = pin.bootstrap.expect("Join 必须带 bootstrap 锚");
    assert_eq!(
        bootstrap.rev, pin.record.rev,
        "record pin 与 bootstrap 锚必须是同一验证视图的 rev"
    );
    assert_ne!(bootstrap.log_id, [0u8; 16]);
    assert_ne!(pin.instance_id, [0u8; 16]);

    // B 的树必须来自 A 的真实基线（不播种、不伪造）。
    let tree = list_tree(&b.vault).expect("读 B 的结构投影");
    let titles: Vec<&str> = tree.iter().map(|row| row.title.as_str()).collect();
    assert_eq!(tree.len(), 1, "B 只应有 A 的那一篇：{titles:?}");
    assert_eq!(tree[0].title, "A 的远端笔记");
    assert_ne!(
        b.identity.device_id, a.identity.device_id,
        "B 必须是独立设备"
    );
    assert_eq!(b.identity.pinned_manager, a.identity.pinned_manager);
}

/// 错 setup secret：relay 明确拒绝（初始化单事务，不会创建 vault）→ 本地不留库/pending。
#[test]
fn remote_create_with_wrong_setup_secret_leaves_no_local_state() {
    let relay = DevRelay::start_tls().expect("起进程内 TLS relay 失败");
    let dir = temp_root("wrong-secret");
    let vault_path = dir.path().join("a/vault.sqlite3");
    let error = match create_remote(
        remote_context(&relay),
        "definitely-not-the-setup-secret",
        &vault_path,
        "remote-init 负例",
        ok_identity,
    ) {
        Ok(_) => panic!("错 setup secret 必须失败"),
        Err(error) => error,
    };
    assert!(
        matches!(error, SyncError::RelayApi { .. } | SyncError::HttpStatus(_)),
        "必须是 relay 明确拒绝：{error:?}"
    );
    assert_no_vault_files(&vault_path);
}

/// 未注入自签根 = 系统根不信任该 CA → TLS 拒绝，且**不建库**（没有发任何业务请求）。
#[test]
fn remote_create_without_injected_root_is_tls_rejected_and_leaves_no_vault() {
    let relay = DevRelay::start_tls().expect("起进程内 TLS relay 失败");
    let dir = temp_root("untrusted");
    let vault_path = dir.path().join("a/vault.sqlite3");
    // 故意不带 `with_test_extra_roots`：生产系统根不会信任测试自签 CA。
    let ctx = RelayContext::remote_tls(relay.base_url(), DEFAULT_TRANSPORT_TIMEOUTS)
        .expect("remote_tls context");
    let error = match create_remote(
        ctx,
        relay.setup_secret(),
        &vault_path,
        "remote-init 负例",
        ok_identity,
    ) {
        Ok(_) => panic!("未信任的自签 CA 必须失败"),
        Err(error) => error,
    };
    assert!(
        matches!(error, SyncError::Tls(_)),
        "必须是 TLS 错误：{error:?}"
    );
    assert_no_vault_files(&vault_path);
}

/// 格式合法但错误的恢复码：自批被 relay 拒绝 → 本地不留库/pending/身份。
#[test]
fn remote_join_with_wrong_recovery_code_leaves_no_local_state() {
    let relay = DevRelay::start_tls().expect("起进程内 TLS relay 失败");
    let dir = temp_root("wrong-code");
    let a_path = dir.path().join("a/vault.sqlite3");
    let ctx = remote_context(&relay);
    let a = create_remote(
        ctx.clone(),
        relay.setup_secret(),
        &a_path,
        "remote-init A",
        ok_identity,
    )
    .expect("A Create 失败");

    // 另一个种子生成的**格式合法**恢复码（不是 A 的）。
    let wrong = RecoverySeed::generate(&mut OsRandom)
        .expect("生成错误种子")
        .to_code()
        .expect("编码错误恢复码");
    let b_path = dir.path().join("b/vault.sqlite3");
    let error = match join_remote(ctx, a.vault_id, wrong.as_str(), &b_path, ok_identity) {
        Ok(_) => panic!("错恢复码必须失败"),
        Err(error) => error,
    };
    assert!(
        matches!(
            error,
            SyncError::RelayApi { .. } | SyncError::HttpStatus(_) | SyncError::RecoveryCode(_)
        ),
        "必须是恢复码/relay 拒绝：{error:?}"
    );
    assert_no_vault_files(&b_path);
}

/// 崩溃点：`GenesisRequested → IdentityWritten` 之后（pin 未提交）→ `reconcile` 在真实 TLS
/// 上补齐 finalize、提交强 pin 并清 pending（不重复 genesis）。
#[test]
fn remote_create_crash_after_identity_is_reconciled_over_tls() {
    let relay = DevRelay::start_tls().expect("起进程内 TLS relay 失败");
    let dir = temp_root("reconcile");
    let vault_path = dir.path().join("a/vault.sqlite3");
    let ctx = remote_context(&relay);

    // 手工复刻 `create_remote` 的前半程，然后在 IdentityWritten 处"崩溃"（drop 库句柄）。
    let vault_id: [u8; 16] = array(&mut OsRandom).expect("vault id");
    let device_id: [u8; 16] = array(&mut OsRandom).expect("device id");
    let materials = build_materials(
        vault_id,
        device_id,
        "remote-init-test-password",
        1,
        &mut OsRandom,
    )
    .expect("genesis 材料");
    let session = menghunote_sync::init_session::InitSession::new_genesis(ctx, &materials)
        .expect("genesis 会话");
    let vault = Vault::open(&vault_path).expect("新库");
    session.prepare_pending(&vault, 1).expect("写前 pending");
    let receipt = session
        .request_genesis(&vault, &materials, relay.setup_secret())
        .expect("genesis 请求");
    let identity = materials
        .identity(receipt.instance_id, receipt.control_rev)
        .expect("身份");
    menghunote_sync::init_session::mark_identity_written(&vault).expect("身份已落盘");
    assert_eq!(
        load_pending(&vault, None)
            .expect("读 pending")
            .expect("pending 仍在")
            .phase,
        InitPhase::IdentityWritten
    );
    // 崩溃：不提交 pin、不做 finish，直接关库。
    drop(vault);

    // 重启对账（用身份材料）：必须补齐 pin → Done。
    let vault = Vault::open(&vault_path).expect("重开库");
    let ca = relay.tls_ca_der().expect("TLS relay").to_vec();
    let report =
        reconcile_with_extra_trust_roots(&vault, Some(&identity), std::slice::from_ref(&ca))
            .expect("对账失败")
            .expect("必须有 pending 报告");
    assert_eq!(report.phase_before, InitPhase::IdentityWritten);
    assert_eq!(report.outcome, InitReconcileOutcome::Completed);
    assert_eq!(load_pending(&vault, None).expect("读 pending"), None);
    let pin = load_control_pin(&vault, vault_id)
        .expect("读 pin")
        .expect("对账必须提交强 pin");
    assert_eq!(pin.record.rev, 1);
    assert!(pin.record.hash.is_some());
}

/// `local_dev` context 不能进入远端入口（不静默降级成明文/开发语义）。
#[test]
fn remote_entry_requires_remote_tls_context() {
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let dir = temp_root("mode-gate");
    let vault_path = dir.path().join("a/vault.sqlite3");
    let ctx = RelayContext::local_dev(relay.base_url(), DEFAULT_TRANSPORT_TIMEOUTS)
        .expect("local_dev context");
    let error = match create_remote(
        ctx,
        relay.setup_secret(),
        &vault_path,
        "remote-init 负例",
        ok_identity,
    ) {
        Ok(_) => panic!("local_dev 必须被远端入口拒绝"),
        Err(error) => error,
    };
    assert!(matches!(error, SyncError::ConfigInvalid(_)), "{error:?}");
    assert!(!vault_path.exists(), "拒绝路径不得建库");
}

/// 一条 pending 夹具（覆盖 remote 模式/HTTPS origin 的 CDE 编码）。
#[test]
fn remote_pending_roundtrips_mode_and_https_origin() {
    let dir = temp_root("pending-cde");
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("vault");
    let pending = InitPendingV1::prepared(
        InitKind::NewGenesis,
        menghunote_sync::relay_policy::RelayMode::RemoteTls,
        "https://relay.example:8443",
        [0x11; 16],
        [0x22; 16],
        7,
    );
    begin_pending(&vault, &pending).expect("写 pending");
    let loaded = load_pending(&vault, Some([0x11; 16]))
        .expect("读 pending")
        .expect("pending 存在");
    assert_eq!(loaded, pending);
    assert_eq!(
        loaded.mode,
        menghunote_sync::relay_policy::RelayMode::RemoteTls
    );
    assert_eq!(loaded.origin, "https://relay.example:8443");
}

`````



@@SECTION code/099 `crates/sync/tests/bootstrap.rs`


`````rust
//! S7-18：新设备从 relay 全量 bootstrap 的集成测试（FakeRelay + 真 SQLite/Loro）。
//!
//! 覆盖评审 0002 P1-4：
//! - body / structure / asset 各阶段失败或"崩溃"后**不推进水位**、journal 可续跑；
//! - 水位 CAS 失败不写半套单调值；
//! - 已有库的 pins/highwater 不被 bootstrap 降低（highwater = max(现有, S)）；
//! - fallback 只用自己的 S/cursor；
//! - 实际 VV/frontier 校验由 `bootstrap.rs` 单测覆盖（不只信 manifest 声明）。
//!
//! 全程无 sleep：失败点由 `BootstrapFault` 或真实 CAS 冲突制造，收敛点都是显式返回值。

mod support;

use support::*;

use menghunote_storage::{
    abort_baseline_apply, asset_bytes, baseline_apply_state, baseline_verify_failure,
    body_doc_meta, create_note_slot, ensure_structure_row, insert_asset, list_all_docs, list_notes,
    load_body_doc, sync_cursor_state, BaselineApplyStatus, BodyDoc, StructureWriter, Value,
    META_SYNC_CONFIRMED_HIGHWATER, META_SYNC_INTEGRATED_SEQ, META_SYNC_RECEIVED_SEQ,
    STRUCTURE_DOC_ID,
};
use menghunote_sync::bootstrap::{
    self, control_head_hash, BootstrapFault, BootstrapPins, META_BOOTSTRAP_CONTROL_HASH,
    META_BOOTSTRAP_CONTROL_REV, META_BOOTSTRAP_EPOCH, META_BOOTSTRAP_INSTANCE,
};
use menghunote_sync::checkpoint::{CheckpointWriter, SlotSecrets, StorageAssets};
use menghunote_sync::engine::{EngineIdentity, SyncEngine, RELAY_CURSOR_META};
use menghunote_sync::genesis;
use menghunote_sync::keys::RootSecretV1;
use menghunote_sync::relay::RelayClient as _;
use menghunote_sync_proto::ObjectType;
use sha2::{Digest, Sha256};

const STRUCTURED_TEXT: &str = "结构化笔记正文";
const ASSET_DATA: &[u8] = b"bootstrap real asset";

fn setup() -> (
    tempfile::TempDir,
    menghunote_storage::Vault,
    genesis::GenesisMaterials,
) {
    let (dir, vault) = fresh_vault();
    ensure_structure_row(&vault).expect("structure 行");
    let materials = genesis::build_materials(
        VAULT_ID,
        [0x92; 16],
        "pw",
        1_700_000_000,
        &mut TestRandom::new(0x92_18),
    )
    .expect("genesis 材料");
    (dir, vault, materials)
}

fn add_note(
    vault: &menghunote_storage::Vault,
    note_id: &str,
    peer: u64,
    text: &str,
    message_id: &str,
    object_id: [u8; 32],
) {
    let doc = seed_doc(vault, note_id, peer);
    create_note_slot(vault, note_id, note_id, note_id, 1_700_000_000).expect("notes 行");
    doc.insert_scalars(0, text).expect("正文");
    commit_with_intent(vault, &doc, message_id, object_id, EPOCH);
}

fn new_relay(materials: &genesis::GenesisMaterials) -> FakeRelay {
    let mut relay = FakeRelay::new(&[]);
    relay
        .certs
        .insert(materials.device_id, materials.certificate().clone());
    relay
}

fn seed_source(
    source: &menghunote_storage::Vault,
    relay: &mut FakeRelay,
    materials: &genesis::GenesisMaterials,
) {
    // 真实设备在 genesis 后已有 relay 游标；checkpoint validate 依赖它核对 S 之前的日志。
    source
        .set_meta(RELAY_CURSOR_META, b"c0")
        .expect("写 relay 游标");
    add_note(
        source,
        "note-1",
        materials.peer_id_u64(),
        "第一行正文",
        &uuid_like(901),
        [0xA1; 32],
    );
    add_note(
        source,
        "note-2",
        materials.peer_id_u64(),
        "second note",
        &uuid_like(902),
        [0xA2; 32],
    );
    // 无 notes 行的墓碑文档（checkpoint 必须导出，bootstrap 必须恢复）。
    let orphan = seed_doc(source, "orphan-body", materials.peer_id_u64());
    orphan.insert_scalars(0, "orphan text").expect("墓碑正文");
    commit_local(source, &orphan);
    let hash: [u8; 32] = Sha256::digest(ASSET_DATA).into();
    insert_asset(
        source,
        &hash,
        ASSET_DATA.len() as u64,
        Some("text/plain"),
        ASSET_DATA,
    )
    .expect("资产");

    let mut writer = CheckpointWriter::new(
        source,
        relay,
        VAULT_ID,
        materials.device_id,
        materials.signing_key.clone(),
        materials.root.clone(),
        materials.peer_id_u64(),
    );
    let secrets = SlotSecrets {
        password: Some("pw"),
        recovery_seed: Some(&materials.recovery_seed),
    };
    writer
        .upload_initial_slots(&materials.password_slot, &materials.recovery_slot, &secrets)
        .expect("槽上传");
    writer
        .create_checkpoint(&StorageAssets)
        .expect("checkpoint 1");
    writer
        .create_checkpoint(&StorageAssets)
        .expect("checkpoint 2");
}

fn flush_pending_and_checkpoint(
    source: &menghunote_storage::Vault,
    relay: &mut FakeRelay,
    materials: &genesis::GenesisMaterials,
) {
    source.set_meta(RELAY_CURSOR_META, b"c0").expect("cursor");
    source
        .set_meta(META_SYNC_RECEIVED_SEQ, &[0u8; 8])
        .expect("received");
    source
        .set_meta(META_SYNC_INTEGRATED_SEQ, &[0u8; 8])
        .expect("integrated");
    let mut engine = SyncEngine::new(
        source,
        relay,
        EngineIdentity {
            device_id: materials.device_id,
            signing_key: materials.signing_key.clone(),
            peer_id: materials.peer_id_u64(),
        },
        materials.root.clone(),
    );
    engine.sync_once().expect("flush seed updates");
    let mut writer = CheckpointWriter::new(
        source,
        relay,
        VAULT_ID,
        materials.device_id,
        materials.signing_key.clone(),
        materials.root.clone(),
        materials.peer_id_u64(),
    );
    writer
        .create_checkpoint(&StorageAssets)
        .expect("checkpoint after flush");
}

/// 造一份"结构文档 + 一条已推到 relay 的正文"的源；返回正文 doc id。
///
/// relay head 推到 1 后打两份 checkpoint（S=1），让"水位是否推进"可观察。
fn seed_structured_source(
    source: &menghunote_storage::Vault,
    relay: &mut FakeRelay,
    materials: &genesis::GenesisMaterials,
) -> String {
    let peer = materials.peer_id_u64();
    source
        .set_meta(RELAY_CURSOR_META, b"c0")
        .expect("写 relay 游标");
    source
        .set_meta(META_SYNC_RECEIVED_SEQ, &[0u8; 8])
        .expect("received");
    source
        .set_meta(META_SYNC_INTEGRATED_SEQ, &[0u8; 8])
        .expect("integrated");

    let mut writer = StructureWriter::open(source, peer).expect("structure writer");
    let seed = writer
        .doc()
        .create_note("结构标题", 1_700_000_000)
        .expect("结构笔记");
    writer.commit("structure").expect("结构提交");
    let body_doc_id = seed.body_doc_id.clone();
    let doc = BodyDoc::restore(body_doc_id.clone(), peer, None, &[]).expect("正文 doc");
    doc.insert_scalars(0, STRUCTURED_TEXT).expect("正文");
    commit_with_intent(source, &doc, &uuid_like(801), [0xD1; 32], EPOCH);

    let hash: [u8; 32] = Sha256::digest(ASSET_DATA).into();
    insert_asset(
        source,
        &hash,
        ASSET_DATA.len() as u64,
        Some("text/plain"),
        ASSET_DATA,
    )
    .expect("资产");

    let mut engine = SyncEngine::new(
        source,
        relay,
        EngineIdentity {
            device_id: materials.device_id,
            signing_key: materials.signing_key.clone(),
            peer_id: peer,
        },
        materials.root.clone(),
    );
    engine.sync_once().expect("push 正文 update");

    let mut writer = CheckpointWriter::new(
        source,
        relay,
        VAULT_ID,
        materials.device_id,
        materials.signing_key.clone(),
        materials.root.clone(),
        peer,
    );
    let secrets = SlotSecrets {
        password: Some("pw"),
        recovery_seed: Some(&materials.recovery_seed),
    };
    writer
        .upload_initial_slots(&materials.password_slot, &materials.recovery_slot, &secrets)
        .expect("槽上传");
    writer
        .create_checkpoint(&StorageAssets)
        .expect("checkpoint 1");
    writer
        .create_checkpoint(&StorageAssets)
        .expect("checkpoint 2");
    body_doc_id
}

fn set_u64_meta(vault: &menghunote_storage::Vault, key: &str, value: u64) {
    vault
        .set_meta(key, &value.to_be_bytes())
        .expect("写 BE64 meta");
}

fn relay_state_and_control_hash(relay: &mut FakeRelay) -> ([u8; 16], [u8; 32]) {
    let state = relay.state().expect("relay state");
    let chain = relay.management_chain().expect("management chain");
    (state.instance_id, control_head_hash(&state, &chain))
}

fn assert_no_sync_watermarks(vault: &menghunote_storage::Vault) {
    assert!(
        vault
            .get_meta(RELAY_CURSOR_META)
            .expect("cursor meta")
            .is_none(),
        "失败路径不得推进 relay 游标"
    );
    assert!(
        vault
            .get_meta(META_SYNC_RECEIVED_SEQ)
            .expect("received meta")
            .is_none(),
        "失败路径不得推进 received"
    );
    assert!(
        vault
            .get_meta(META_SYNC_INTEGRATED_SEQ)
            .expect("integrated meta")
            .is_none(),
        "失败路径不得推进 integrated"
    );
    assert!(
        vault
            .get_meta(META_SYNC_CONFIRMED_HIGHWATER)
            .expect("highwater meta")
            .is_none(),
        "失败路径不得推进 highwater"
    );
}

fn assert_staging_journal(vault: &menghunote_storage::Vault) -> [u8; 16] {
    let journal = baseline_apply_state(vault)
        .expect("journal state")
        .expect("staging journal 必须保留，供续跑");
    assert_eq!(journal.status, BaselineApplyStatus::Staging);
    journal.checkpoint_id
}

#[test]
fn bootstrap_restores_all_docs_and_assets() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_source(&source, &mut relay, &materials);
    assert_eq!(relay.published_checkpoints.len(), 2);

    let (_target_dir, target) = fresh_vault();
    let report = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect("bootstrap");
    assert_eq!(
        report.covered_seq,
        relay.published_checkpoints[1].covered_seq
    );
    assert!(!report.used_fallback);
    assert!(report.docs.iter().any(|(doc_id, _)| doc_id == "note-1"));
    assert!(report
        .docs
        .iter()
        .any(|(doc_id, _)| doc_id == "orphan-body"));
    assert_eq!(report.assets, 1);
    let note1 = load_body_doc(&target, "note-1", materials.peer_id_u64(), None)
        .expect("note-1")
        .0;
    assert_eq!(note1.text(), "第一行正文");
    let orphan = load_body_doc(&target, "orphan-body", materials.peer_id_u64(), None)
        .expect("orphan")
        .0;
    assert_eq!(orphan.text(), "orphan text");
    assert!(body_doc_meta(&target, "note-2").expect("meta").is_some());
    let asset_hash: [u8; 32] = Sha256::digest(ASSET_DATA).into();
    let asset = asset_bytes(&target, asset_hash).expect("asset_bytes");
    assert_eq!(asset.as_deref(), Some(ASSET_DATA));
    // 水位/cursor 已写入：engine 之后从 S 继续，不会重复导入。
    assert_eq!(
        target
            .get_meta(RELAY_CURSOR_META)
            .expect("cursor meta")
            .as_deref(),
        Some(report.cursor.as_bytes())
    );
    assert_eq!(
        target
            .get_meta(META_SYNC_RECEIVED_SEQ)
            .expect("received")
            .as_deref(),
        Some(report.covered_seq.to_be_bytes().as_slice())
    );
    assert_eq!(
        target
            .get_meta(META_SYNC_INTEGRATED_SEQ)
            .expect("integrated")
            .as_deref(),
        Some(report.covered_seq.to_be_bytes().as_slice())
    );
    assert_eq!(
        target
            .get_meta(META_SYNC_CONFIRMED_HIGHWATER)
            .expect("highwater")
            .as_deref(),
        Some(report.covered_seq.to_be_bytes().as_slice())
    );
    // staging journal 成功后必须清掉。
    assert!(baseline_apply_state(&target).expect("journal").is_none());
}

#[test]
fn structured_source_imports_structure_and_projects_notes() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    let body_doc_id = seed_structured_source(&source, &mut relay, &materials);

    let (_target_dir, target) = fresh_vault();
    let report = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect("bootstrap structured");
    assert_eq!(report.covered_seq, relay.head_seq);
    assert_eq!(report.cursor, format!("c{}", relay.head_seq));
    // structure 文档在报告里没有正文文本。
    assert!(report
        .docs
        .iter()
        .any(|(doc_id, text)| doc_id == STRUCTURE_DOC_ID && text.is_none()));
    let notes = list_notes(&target).expect("list_notes");
    assert_eq!(notes.len(), 1, "结构投影必须建出 1 篇笔记");
    let body = load_body_doc(&target, &body_doc_id, materials.peer_id_u64(), None)
        .expect("结构化正文")
        .0;
    assert_eq!(body.text(), STRUCTURED_TEXT);
}

#[test]
fn crash_after_bodies_resumes_without_advancing_watermarks() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    let body_doc_id = seed_structured_source(&source, &mut relay, &materials);
    let s = relay.head_seq;
    assert!(s > 0, "本测试要求 baseline S>0 才能区分是否推进水位");

    let (_target_dir, target) = fresh_vault();
    let error = bootstrap::run_with_fault(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
        BootstrapFault::AfterBodies,
    )
    .expect_err("注入故障必须失败");
    assert!(
        format!("{error}").contains("注入故障"),
        "错误应来自注入点：{error}"
    );

    // 正文已经落库（可续跑），但结构/资产/水位都没有。
    let body = load_body_doc(&target, &body_doc_id, materials.peer_id_u64(), None)
        .expect("正文已导入")
        .0;
    assert_eq!(body.text(), STRUCTURED_TEXT);
    assert!(list_notes(&target).expect("list_notes").is_empty());
    assert_no_sync_watermarks(&target);
    let journal_checkpoint = assert_staging_journal(&target);
    assert_eq!(
        journal_checkpoint,
        relay
            .published_checkpoints
            .last()
            .expect("primary checkpoint")
            .checkpoint_id
    );

    // 续跑：同一基线幂等重放，完成后水位才推进。
    let report = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect("续跑成功");
    assert_eq!(report.covered_seq, s);
    assert_eq!(
        target
            .get_meta(META_SYNC_RECEIVED_SEQ)
            .expect("received")
            .as_deref(),
        Some(s.to_be_bytes().as_slice())
    );
    assert_eq!(
        target
            .get_meta(META_SYNC_INTEGRATED_SEQ)
            .expect("integrated")
            .as_deref(),
        Some(s.to_be_bytes().as_slice())
    );
    assert_eq!(
        target
            .get_meta(RELAY_CURSOR_META)
            .expect("cursor")
            .as_deref(),
        Some(report.cursor.as_bytes())
    );
    assert_eq!(list_notes(&target).expect("list_notes").len(), 1);
    assert!(baseline_apply_state(&target).expect("journal").is_none());
}

#[test]
fn structure_projection_failure_keeps_cursor_and_resumes() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_structured_source(&source, &mut relay, &materials);
    let s = relay.head_seq;

    let (_target_dir, target) = fresh_vault();
    bootstrap::run_with_fault(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
        BootstrapFault::BeforeStructureProjection,
    )
    .expect_err("结构投影前崩溃必须失败");

    // structure 文档已写入，但投影还没发生：notes 仍为空，水位未推进。
    assert!(
        body_doc_meta(&target, STRUCTURE_DOC_ID)
            .expect("structure meta")
            .is_some(),
        "structure 文档应已落库"
    );
    assert!(list_notes(&target).expect("list_notes").is_empty());
    assert_no_sync_watermarks(&target);
    assert_staging_journal(&target);

    // 续跑补齐投影与水位。
    let report = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect("续跑成功");
    assert_eq!(report.covered_seq, s);
    assert_eq!(list_notes(&target).expect("list_notes").len(), 1);
    assert_eq!(
        target
            .get_meta(META_SYNC_CONFIRMED_HIGHWATER)
            .expect("highwater")
            .as_deref(),
        Some(s.to_be_bytes().as_slice())
    );
}

#[test]
fn asset_write_conflict_keeps_cursor_and_resumes_after_fix() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_source(&source, &mut relay, &materials);
    flush_pending_and_checkpoint(&source, &mut relay, &materials);
    let s = relay.head_seq;
    assert!(s > 0);

    let (_target_dir, target) = fresh_vault();
    // 预置同 asset_hash、同长度但内容不同的资产行：apply_baseline_assets 必须冲突。
    let asset_hash: [u8; 32] = Sha256::digest(ASSET_DATA).into();
    let mut wrong = ASSET_DATA.to_vec();
    wrong[0] ^= 0xFF;
    target
        .exec(
            "INSERT INTO assets(asset_hash,byte_len,media_type,data,orphan_since) \
             VALUES(?1,?2,?3,?4,NULL)",
            &[
                Value::Blob(asset_hash.to_vec()),
                Value::Integer(ASSET_DATA.len() as i64),
                Value::Text("text/plain".to_owned()),
                Value::Blob(wrong),
            ],
        )
        .expect("预置冲突资产");

    let error = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect_err("资产写失败必须冒泡，不能用半库换基线");
    assert!(
        format!("{error}").contains("资产"),
        "错误应指向资产冲突：{error}"
    );
    assert_no_sync_watermarks(&target);
    assert_staging_journal(&target);
    // 已导入的正文仍在（可续跑），冲突资产未被覆盖。
    assert_eq!(
        load_body_doc(&target, "note-1", materials.peer_id_u64(), None)
            .expect("正文已导入")
            .0
            .text(),
        "第一行正文"
    );
    let stored = asset_bytes(&target, asset_hash).expect("asset_bytes");
    assert_ne!(stored.as_deref(), Some(ASSET_DATA));

    // 修掉冲突后同一 journal 续跑成功。
    target
        .exec(
            "DELETE FROM assets WHERE asset_hash=?1",
            &[Value::Blob(asset_hash.to_vec())],
        )
        .expect("删除冲突资产");
    let report = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect("修复后续跑");
    assert_eq!(report.covered_seq, s);
    let stored = asset_bytes(&target, asset_hash).expect("asset_bytes");
    assert_eq!(stored.as_deref(), Some(ASSET_DATA));
    assert_eq!(
        target
            .get_meta(META_SYNC_RECEIVED_SEQ)
            .expect("received")
            .as_deref(),
        Some(s.to_be_bytes().as_slice())
    );
    assert!(baseline_apply_state(&target).expect("journal").is_none());
}

#[test]
fn crash_after_assets_resumes_without_duplicate_writes() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_structured_source(&source, &mut relay, &materials);
    let s = relay.head_seq;
    let asset_hash: [u8; 32] = Sha256::digest(ASSET_DATA).into();

    let (_target_dir, target) = fresh_vault();
    bootstrap::run_with_fault(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
        BootstrapFault::AfterAssets,
    )
    .expect_err("资产写完后崩溃必须失败，水位不得推进");
    // 资产与结构都已落库，但水位仍未写；journal 记录 applied_assets 供续跑。
    assert_eq!(
        asset_bytes(&target, asset_hash)
            .expect("asset_bytes")
            .as_deref(),
        Some(ASSET_DATA)
    );
    assert_eq!(list_notes(&target).expect("list_notes").len(), 1);
    assert_no_sync_watermarks(&target);
    let journal = baseline_apply_state(&target)
        .expect("journal state")
        .expect("journal 必须保留");
    assert_eq!(journal.applied_assets, 1);

    // 续跑：同资产幂等（不重复插入、不再要求不同明文），水位才推进。
    let report = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect("资产阶段崩溃后续跑");
    assert_eq!(report.covered_seq, s);
    assert_eq!(report.assets, 1);
    assert_eq!(
        asset_bytes(&target, asset_hash)
            .expect("asset_bytes")
            .as_deref(),
        Some(ASSET_DATA)
    );
    assert_eq!(
        target
            .get_meta(META_SYNC_CONFIRMED_HIGHWATER)
            .expect("highwater")
            .as_deref(),
        Some(s.to_be_bytes().as_slice())
    );
    assert!(baseline_apply_state(&target).expect("journal").is_none());
}

#[test]
fn verify_failure_keeps_merged_data_and_marks_journal_failed() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_source(&source, &mut relay, &materials);

    let (_target_dir, target) = fresh_vault();
    // 目标库已有同一篇正文的本地编辑：bootstrap 必须把它与基线 merge（不是覆盖）。
    let local_peer = materials.peer_id_u64().wrapping_add(1);
    let local = seed_doc(&target, "note-1", local_peer);
    local.insert_scalars(0, "本地优先编辑 ").expect("本地正文");
    commit_local(&target, &local);

    let error = bootstrap::run_with_fault(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
        BootstrapFault::VerifyVvNotCovered,
    )
    .expect_err("注入的读回校验失败必须冒泡");
    assert!(
        format!("{error}").contains("VV 未覆盖快照"),
        "错误应来自读回校验：{error}"
    );

    // 已 merge 的 Loro 数据保留：本地编辑与基线正文都在同一条文档里。
    let merged = load_body_doc(&target, "note-1", local_peer, None)
        .expect("merged doc")
        .0;
    assert!(
        merged.text().contains("本地优先编辑") && merged.text().contains("第一行正文"),
        "失败不得回滚已 merge 的数据，实际={:?}",
        merged.text()
    );

    // 水位没有推进；journal 仍是 staging（schema CHECK 只有 staging/completed），
    // 失败语义由 meta 失败标记承载。
    assert_no_sync_watermarks(&target);
    let journal = baseline_apply_state(&target)
        .expect("journal state")
        .expect("失败后 journal 必须保留");
    assert_eq!(journal.status, BaselineApplyStatus::Staging);
    assert!(journal.applied_docs >= 2, "已应用批次计数应保留");
    let reason = baseline_verify_failure(&target)
        .expect("读失败标记")
        .expect("校验失败必须写失败标记");
    assert!(
        reason.contains("VV 未覆盖快照"),
        "标记应保留失败原因：{reason}"
    );

    // 未 abort 前 begin 一律拒绝（同 checkpoint 幂等续跑也不行）。
    let retry = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect_err("失败标记必须阻止 begin 续跑");
    assert!(
        format!("{retry}").contains("abort_baseline_apply"),
        "错误必须提示先 abort：{retry}"
    );

    // abort 清 journal + 失败标记，但保留已 merge 的 Loro 数据。
    assert!(abort_baseline_apply(&target).expect("abort"));
    assert!(baseline_apply_state(&target).expect("state").is_none());
    assert!(baseline_verify_failure(&target).expect("标记").is_none());
    let kept = load_body_doc(&target, "note-1", local_peer, None)
        .expect("abort 后正文")
        .0;
    assert!(
        kept.text().contains("本地优先编辑") && kept.text().contains("第一行正文"),
        "abort 只清 journal，不得删已 merge 的 Loro 数据，实际={:?}",
        kept.text()
    );

    // abort 后可以重新 begin 并成功（失败标记已清）。
    let report = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect("abort 后重跑");
    assert_eq!(
        report.covered_seq,
        relay.published_checkpoints[1].covered_seq
    );
    assert!(report.docs.iter().any(|(doc_id, _)| doc_id == "note-1"));
    assert!(baseline_apply_state(&target).expect("state").is_none());
}

#[test]
fn cursor_cas_conflict_rolls_back_all_watermarks_and_retry_succeeds() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_source(&source, &mut relay, &materials);
    flush_pending_and_checkpoint(&source, &mut relay, &materials);
    let s = relay.head_seq;
    assert!(s > 0);

    let (_target_dir, target) = fresh_vault();
    let report = bootstrap::run_with_fault(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
        BootstrapFault::CursorCasConflict,
    );
    assert!(report.is_err(), "游标 CAS 冲突必须失败");
    // 事务整笔回滚：并发写者留下的游标保留，其余单调值一个都没写。
    assert_eq!(
        target
            .get_meta(RELAY_CURSOR_META)
            .expect("cursor")
            .as_deref(),
        Some(b"fault-cursor".as_slice())
    );
    assert!(target
        .get_meta(META_SYNC_RECEIVED_SEQ)
        .expect("received")
        .is_none());
    assert!(target
        .get_meta(META_SYNC_INTEGRATED_SEQ)
        .expect("integrated")
        .is_none());
    assert!(target
        .get_meta(META_SYNC_CONFIRMED_HIGHWATER)
        .expect("highwater")
        .is_none());
    assert_staging_journal(&target);

    // 重试：从当前游标 CAS 前进，单调且幂等。
    let report = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect("重试成功");
    assert_eq!(report.covered_seq, s);
    assert_eq!(
        target
            .get_meta(RELAY_CURSOR_META)
            .expect("cursor")
            .as_deref(),
        Some(report.cursor.as_bytes())
    );
    assert_eq!(
        target
            .get_meta(META_SYNC_CONFIRMED_HIGHWATER)
            .expect("highwater")
            .as_deref(),
        Some(s.to_be_bytes().as_slice())
    );
    assert!(baseline_apply_state(&target).expect("journal").is_none());
}

#[test]
fn existing_pins_and_highwater_are_not_lowered() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_source(&source, &mut relay, &materials);
    flush_pending_and_checkpoint(&source, &mut relay, &materials);
    // 让 primary 的 manifest 腐坏，强制选 fallback（fallback 的 S=0 < 本地水位 1）。
    let primary_manifest = relay
        .published_checkpoints
        .last()
        .and_then(|checkpoint| checkpoint.manifest_ref)
        .expect("primary manifest");
    relay
        .parts
        .get_mut(&(primary_manifest, 0))
        .expect("primary manifest part")[0] ^= 0x01;
    let (instance_id, control_hash) = relay_state_and_control_hash(&mut relay);

    let (_target_dir, target) = fresh_vault();
    // 已有库：一篇本地编辑过的正文 + 已确认的水位/游标/pins。
    let local_peer = materials.peer_id_u64().wrapping_add(1);
    let local = seed_doc(&target, "note-1", local_peer);
    local.insert_scalars(0, "local edit ").expect("本地正文");
    commit_local(&target, &local);
    target
        .set_meta(menghunote_storage::META_SYNC_LOG_ID, &relay.log_id)
        .expect("log id");
    target.set_meta(RELAY_CURSOR_META, b"c2").expect("cursor");
    set_u64_meta(&target, META_SYNC_RECEIVED_SEQ, 2);
    set_u64_meta(&target, META_SYNC_INTEGRATED_SEQ, 2);
    set_u64_meta(&target, META_SYNC_CONFIRMED_HIGHWATER, 2);
    target
        .set_meta(META_BOOTSTRAP_INSTANCE, &instance_id)
        .expect("instance pin");
    set_u64_meta(&target, META_BOOTSTRAP_EPOCH, EPOCH);
    set_u64_meta(&target, META_BOOTSTRAP_CONTROL_REV, relay.control_rev);
    target
        .set_meta(META_BOOTSTRAP_CONTROL_HASH, &control_hash)
        .expect("control hash pin");

    let report = bootstrap::run(&target, &mut relay, VAULT_ID, local_peer, &materials.root)
        .expect("已有库 bootstrap 必须成功");
    assert!(report.used_fallback);
    assert_eq!(report.covered_seq, 1, "fallback 必须用自己的 S");
    assert_eq!(report.cursor, "c1");

    // 单调值全部保持原值，绝不被 baseline S 降级。
    assert_eq!(
        target
            .get_meta(RELAY_CURSOR_META)
            .expect("cursor")
            .as_deref(),
        Some(b"c2".as_slice())
    );
    assert_eq!(
        target
            .get_meta(META_SYNC_RECEIVED_SEQ)
            .expect("received")
            .as_deref(),
        Some(2u64.to_be_bytes().as_slice())
    );
    assert_eq!(
        target
            .get_meta(META_SYNC_INTEGRATED_SEQ)
            .expect("integrated")
            .as_deref(),
        Some(2u64.to_be_bytes().as_slice())
    );
    assert_eq!(
        target
            .get_meta(META_SYNC_CONFIRMED_HIGHWATER)
            .expect("highwater")
            .as_deref(),
        Some(2u64.to_be_bytes().as_slice())
    );
    // pins 不变。
    assert_eq!(
        target
            .get_meta(META_BOOTSTRAP_INSTANCE)
            .expect("instance pin")
            .as_deref(),
        Some(instance_id.as_slice())
    );
    assert_eq!(
        target
            .get_meta(META_BOOTSTRAP_CONTROL_HASH)
            .expect("control hash pin")
            .as_deref(),
        Some(control_hash.as_slice())
    );
    // 已有本地编辑按 VV 合并，未被跳过/覆盖。
    let merged = load_body_doc(&target, "note-1", local_peer, None)
        .expect("note-1")
        .0;
    assert!(
        merged.text().contains("local edit") && merged.text().contains("第一行正文"),
        "已有 doc 必须按 VV 合并，实际={:?}",
        merged.text()
    );
}

#[test]
fn pin_regressions_reject_before_any_write() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_source(&source, &mut relay, &materials);
    let (instance_id, control_hash) = relay_state_and_control_hash(&mut relay);
    let state = relay.state().expect("state");
    let correct = BootstrapPins {
        instance_id: Some(instance_id),
        log_id: Some(relay.log_id),
        epoch: Some(EPOCH),
        control_rev: Some(relay.control_rev),
        control_hash: Some(control_hash),
        highwater: Some(0),
    };
    let regressions = [
        (
            "instance",
            BootstrapPins {
                instance_id: Some([0x99; 16]),
                ..correct
            },
        ),
        (
            "log_id",
            BootstrapPins {
                log_id: Some([0x99; 16]),
                ..correct
            },
        ),
        (
            "epoch",
            BootstrapPins {
                epoch: Some(EPOCH + 1),
                ..correct
            },
        ),
        (
            "control rev",
            BootstrapPins {
                control_rev: Some(relay.control_rev + 1),
                ..correct
            },
        ),
        (
            "control hash",
            BootstrapPins {
                control_hash: Some([0x99; 32]),
                ..correct
            },
        ),
        (
            "highwater",
            BootstrapPins {
                highwater: Some(state.head_seq + 1),
                ..correct
            },
        ),
    ];
    for (name, pins) in regressions {
        let (_target_dir, target) = fresh_vault();
        let error = bootstrap::run_with_pins(
            &target,
            &mut relay,
            VAULT_ID,
            materials.peer_id_u64(),
            &materials.root,
            &pins,
        )
        .expect_err(&format!("pins {name} 回退必须拒绝"));
        assert!(!format!("{error}").is_empty());
        assert!(
            list_all_docs(&target).expect("list_all_docs").is_empty(),
            "pins {name} 回退时不得写半库"
        );
        assert_no_sync_watermarks(&target);
        assert!(
            baseline_apply_state(&target).expect("journal").is_none(),
            "校验失败不得创建 journal"
        );
    }

    // 正确 pins 必须通过，并把 pins 持久化。
    let (_target_dir, target) = fresh_vault();
    bootstrap::run_with_pins(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
        &correct,
    )
    .expect("正确 pins 应通过");
    assert_eq!(
        target
            .get_meta(META_BOOTSTRAP_INSTANCE)
            .expect("instance pin")
            .as_deref(),
        Some(instance_id.as_slice())
    );
}

#[test]
fn control_pin_advances_but_never_rolls_back() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_source(&source, &mut relay, &materials);
    // 真实 relay 的 `/control` 键 5 必然从 rev1 genesis 起；历史锚按"截断到锚 rev 的
    // 管理链前缀"复算，空链无法复算视图 → 按 ADR 0013 §3 只能 fail-closed。
    relay.manager_chain = vec![menghunote_sync::relay::ManagerChainEntry {
        rev: 1,
        pubkey: materials.manager.public_key(),
        transition: b"test-genesis".to_vec(),
        old_rev: 1,
        old_signature: None,
        new_signature: None,
    }];

    let (_target_dir, target) = fresh_vault();
    bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect("首次 bootstrap");
    let first_rev = relay.control_rev;
    assert_eq!(
        target
            .get_meta(META_BOOTSTRAP_CONTROL_REV)
            .expect("control rev")
            .as_deref(),
        Some(first_rev.to_be_bytes().as_slice())
    );
    let first_hash = target
        .get_meta(META_BOOTSTRAP_CONTROL_HASH)
        .expect("control hash")
        .expect("hash pin");

    // control_rev 前进：允许并刷新 pin。
    relay.control_rev = first_rev + 1;
    bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect("control_rev 前进必须允许");
    assert_eq!(
        target
            .get_meta(META_BOOTSTRAP_CONTROL_REV)
            .expect("control rev")
            .as_deref(),
        Some((first_rev + 1).to_be_bytes().as_slice())
    );
    let second_hash = target
        .get_meta(META_BOOTSTRAP_CONTROL_HASH)
        .expect("control hash")
        .expect("hash pin");
    assert_ne!(first_hash, second_hash, "rev 前进时控制头摘要必须更新");

    // control_rev 回退：必须 fail-stop，且不改任何本地状态。
    relay.control_rev = first_rev;
    let error = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect_err("control_rev 回退必须拒绝");
    assert!(!format!("{error}").is_empty());
    assert_eq!(
        target
            .get_meta(META_BOOTSTRAP_CONTROL_REV)
            .expect("control rev")
            .as_deref(),
        Some((first_rev + 1).to_be_bytes().as_slice())
    );
    assert!(load_body_doc(&target, "note-1", materials.peer_id_u64(), None).is_ok());
}

#[test]
fn bootstrap_cursor_pulls_updates_after_s() {
    let (_dir, source, materials) = setup();
    let (_target_dir, target) = fresh_vault();
    let mut relay = new_relay(&materials);
    seed_source(&source, &mut relay, &materials);
    flush_pending_and_checkpoint(&source, &mut relay, &materials);
    let s = relay
        .published_checkpoints
        .last()
        .expect("checkpoint")
        .covered_seq;
    assert_eq!(relay.head_seq, s, "checkpoint 之后不应再有日志事件");

    // 源在 checkpoint 之后给已有 note-1 追加 1 条 update 并推到 relay。
    let (doc, _) = load_body_doc(&source, "note-1", materials.peer_id_u64(), None).expect("note-1");
    doc.insert_scalars(0, "after checkpoint ").expect("正文");
    commit_with_intent(&source, &doc, &uuid_like(990), [0xB1; 32], EPOCH);
    {
        let mut engine = SyncEngine::new(
            &source,
            &mut relay,
            EngineIdentity {
                device_id: materials.device_id,
                signing_key: materials.signing_key.clone(),
                peer_id: materials.peer_id_u64(),
            },
            materials.root.clone(),
        );
        let pushed = engine.sync_once().expect("source push after checkpoint");
        assert_eq!(pushed.push.uploaded, 1);
    }
    assert!(relay.head_seq > s);

    let report = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect("bootstrap at S");
    assert_eq!(report.covered_seq, s);
    {
        let mut engine = SyncEngine::new(
            &target,
            &mut relay,
            EngineIdentity {
                device_id: materials.device_id,
                signing_key: materials.signing_key.clone(),
                peer_id: materials.peer_id_u64(),
            },
            materials.root.clone(),
        );
        let pulled = engine.sync_once().expect("target catch up after S");
        assert!(pulled.pull.events >= 1, "必须拉到 S 之后的 update");
    }
    let (doc, _) = load_body_doc(&target, "note-1", materials.peer_id_u64(), None).expect("note-1");
    assert!(
        doc.text().contains("after checkpoint"),
        "S 之后的 update 必须被 integrate，实际={:?}",
        doc.text()
    );
}

#[test]
fn bootstrap_is_idempotent_on_second_run() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_source(&source, &mut relay, &materials);
    let (_target_dir, target) = fresh_vault();
    let first = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect("第一次 bootstrap");
    let second = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect("第二次 bootstrap");
    assert_eq!(first.covered_seq, second.covered_seq);
    assert_eq!(first.cursor, second.cursor);
    let (note1, _) =
        load_body_doc(&target, "note-1", materials.peer_id_u64(), None).expect("note-1");
    assert_eq!(note1.text(), "第一行正文");
}

#[test]
fn fallback_is_used_when_primary_manifest_is_tampered() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_source(&source, &mut relay, &materials);
    flush_pending_and_checkpoint(&source, &mut relay, &materials);
    let fallback = relay
        .published_checkpoints
        .get(relay.published_checkpoints.len() - 2)
        .expect("fallback checkpoint")
        .clone();
    let primary = relay
        .published_checkpoints
        .last()
        .and_then(|checkpoint| checkpoint.manifest_ref)
        .expect("primary manifest");
    assert_ne!(fallback.covered_seq, relay.head_seq);
    let bytes = relay.parts.get_mut(&(primary, 0)).expect("manifest part");
    bytes[0] ^= 0x01;

    let (_target_dir, target) = fresh_vault();
    let report = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect("fallback");
    assert!(report.used_fallback);
    assert_eq!(report.covered_seq, fallback.covered_seq);
    assert_eq!(report.cursor, format!("c{}", fallback.covered_seq));
    assert_eq!(
        target
            .get_meta(META_SYNC_RECEIVED_SEQ)
            .expect("received meta"),
        Some(fallback.covered_seq.to_be_bytes().to_vec())
    );
    assert_eq!(
        target
            .get_meta(META_SYNC_INTEGRATED_SEQ)
            .expect("integrated meta"),
        Some(fallback.covered_seq.to_be_bytes().to_vec())
    );
    assert!(load_body_doc(&target, "note-1", materials.peer_id_u64(), None).is_ok());
}

#[test]
fn fallback_is_used_when_primary_snapshot_is_tampered() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_source(&source, &mut relay, &materials);
    flush_pending_and_checkpoint(&source, &mut relay, &materials);
    let fallback = relay
        .published_checkpoints
        .get(relay.published_checkpoints.len() - 2)
        .expect("fallback checkpoint")
        .clone();
    let primary_snapshot = relay
        .published_checkpoints
        .last()
        .and_then(|checkpoint| checkpoint.data_refs.first().copied())
        .expect("primary snapshot");
    relay
        .parts
        .get_mut(&(primary_snapshot, 0))
        .expect("snapshot part")[0] ^= 0x01;

    let (_target_dir, target) = fresh_vault();
    let report = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect("fallback after snapshot tamper");
    assert!(report.used_fallback);
    assert_eq!(report.covered_seq, fallback.covered_seq);
    assert_eq!(report.cursor, format!("c{}", fallback.covered_seq));
}

#[test]
fn existing_local_doc_is_merged_not_skipped() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_source(&source, &mut relay, &materials);

    let (_target_dir, target) = fresh_vault();
    // 本机 peer 必须与快照生产者不同，否则同 peer 的 op 会被 Loro 当作重复丢弃。
    let local_peer = materials.peer_id_u64().wrapping_add(1);
    let local = seed_doc(&target, "note-1", local_peer);
    local.insert_scalars(0, "local edit ").expect("本地正文");
    commit_local(&target, &local);

    let report = bootstrap::run(&target, &mut relay, VAULT_ID, local_peer, &materials.root)
        .expect("bootstrap with existing doc");
    assert!(!report.used_fallback);
    let (merged, _) = load_body_doc(&target, "note-1", local_peer, None).expect("note-1");
    assert!(
        merged.text().contains("local edit") && merged.text().contains("第一行正文"),
        "已有 doc 必须按 VV 合并，实际={:?}",
        merged.text()
    );
}

#[test]
fn tampered_manifest_on_both_baselines_writes_nothing() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_source(&source, &mut relay, &materials);
    for checkpoint in &relay.published_checkpoints {
        if let Some(manifest) = checkpoint.manifest_ref {
            if let Some(bytes) = relay.parts.get_mut(&(manifest, 0)) {
                bytes[0] ^= 0x01;
            }
        }
    }
    let (_target_dir, target) = fresh_vault();
    let error = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect_err("篡改 manifest 必须失败");
    assert!(
        list_all_docs(&target).expect("list_all_docs").is_empty(),
        "校验失败前不得写半库: {error:?}"
    );
    assert!(baseline_apply_state(&target).expect("journal").is_none());
}

#[test]
fn tampered_snapshot_is_rejected_without_partial_import() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_source(&source, &mut relay, &materials);
    // 同时篡改两份基线的第一个 SNAPSHOT 对象，确保 fallback 也失败。
    for checkpoint in &relay.published_checkpoints {
        for object_id in &checkpoint.data_refs {
            if let Some(bytes) = relay.parts.get_mut(&(*object_id, 0)) {
                bytes[0] ^= 0x01;
            }
        }
    }
    let (_target_dir, target) = fresh_vault();
    let error = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect_err("篡改快照必须失败");
    assert!(
        list_all_docs(&target).expect("list_all_docs").is_empty(),
        "失败不得写半库: {error:?}"
    );
}

#[test]
fn wrong_root_rejects_bootstrap_before_write() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_source(&source, &mut relay, &materials);
    let wrong = RootSecretV1::new(
        VAULT_ID,
        EPOCH,
        menghunote_sync::keys::Secret32::from_array([0x5A; 32]),
    );
    let (_target_dir, target) = fresh_vault();
    assert!(bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &wrong
    )
    .is_err());
    assert!(list_all_docs(&target).expect("list_all_docs").is_empty());
}

#[test]
fn bootstrap_body_doc_head_is_one_full_update() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_source(&source, &mut relay, &materials);
    let (_target_dir, target) = fresh_vault();
    bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect("bootstrap");
    let rows = menghunote_storage::doc_updates_since(&target, "note-1", 0).expect("updates");
    assert_eq!(rows.len(), 1, "bootstrap 只写 1 条全量 update");
    assert_eq!(rows[0].seq, 1);
    // 不是 snapshot 行：snapshot_seq 必须保持 0（后续优化再补）。
    let meta = body_doc_meta(&target, "note-1")
        .expect("meta")
        .expect("row");
    assert_eq!(meta.head_seq, 1);
}

#[test]
fn empty_baseline_at_zero_is_idempotent() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    source.set_meta(RELAY_CURSOR_META, b"c0").expect("cursor");
    let mut writer = CheckpointWriter::new(
        &source,
        &mut relay,
        VAULT_ID,
        materials.device_id,
        materials.signing_key.clone(),
        materials.root.clone(),
        materials.peer_id_u64(),
    );
    let secrets = SlotSecrets {
        password: Some("pw"),
        recovery_seed: Some(&materials.recovery_seed),
    };
    writer
        .upload_initial_slots(&materials.password_slot, &materials.recovery_slot, &secrets)
        .expect("槽上传");
    writer
        .create_checkpoint(&StorageAssets)
        .expect("空库 checkpoint 1");
    writer
        .create_checkpoint(&StorageAssets)
        .expect("空库 checkpoint 2");
    assert_eq!(relay.head_seq, 0);

    let (_target_dir, target) = fresh_vault();
    let first = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect("空基线 bootstrap");
    assert_eq!(first.covered_seq, 0);
    assert_eq!(first.cursor, "c0");
    assert_eq!(
        sync_cursor_state(&target)
            .expect("cursor")
            .confirmed_highwater,
        Some(0)
    );
    assert!(body_doc_meta(&target, STRUCTURE_DOC_ID)
        .expect("structure meta")
        .is_some());
    assert!(list_notes(&target).expect("list_notes").is_empty());

    // S=0 的第二次 bootstrap 必须幂等（游标 c0 -> c0 的 CAS 也要通过）。
    let second = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect("空基线幂等重放");
    assert_eq!(first, second);
}

#[test]
fn cursor_metadata_reflects_written_watermarks() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_source(&source, &mut relay, &materials);
    let (_target_dir, target) = fresh_vault();
    let report = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect("bootstrap");
    let state = sync_cursor_state(&target).expect("cursor state");
    assert_eq!(state.received_seq, report.covered_seq);
    assert_eq!(state.integrated_seq, report.covered_seq);
    assert_eq!(state.confirmed_highwater, Some(report.covered_seq));
    assert_eq!(state.log_id, Some(relay.log_id));
}

// ============================================================ 评审 0002 T16/T17 补充
// primary 的 commit/signature/asset 各阶段腐坏 → 整体切 fallback，且只用 fallback 自己的 S/cursor；
// 两份都坏 → 失败且零写入/零水位（不能"两坏也 ACK"）。

fn tamper_checkpoint_commit(relay: &mut FakeRelay, index: usize) {
    let checkpoint = relay
        .published_checkpoints
        .get_mut(index)
        .expect("checkpoint");
    checkpoint.signed_commit.as_mut().expect("signed commit")[0] ^= 0x01;
}

fn tamper_checkpoint_signature(relay: &mut FakeRelay, index: usize) {
    let checkpoint = relay
        .published_checkpoints
        .get_mut(index)
        .expect("checkpoint");
    checkpoint.signature.as_mut().expect("signature")[0] ^= 0x01;
}

/// task-122：把已发布基线的 commit 改写成指定的验证修订（保持字段与签名自洽），
/// 用于证明客户端消费者会显式拒绝旧修订。
fn rewrite_checkpoint_validation_version(
    relay: &mut FakeRelay,
    materials: &genesis::GenesisMaterials,
    index: usize,
    version: u16,
) {
    let checkpoint = relay
        .published_checkpoints
        .get(index)
        .cloned()
        .expect("checkpoint");
    let signed = checkpoint.signed_commit.expect("signed commit");
    let mut commit =
        menghunote_sync_proto::CheckpointCommit::decode(&signed).expect("decode commit");
    commit.validation_version = version;
    commit.validation_digest = if version == 2 { [0x5A; 32] } else { [0u8; 32] };
    let signed = commit.encode().expect("encode commit");
    let signature = materials
        .signing_key
        .sign(&menghunote_sync_proto::checkpoint_signing_input(&commit).expect("signing input"));
    let entry = relay
        .published_checkpoints
        .get_mut(index)
        .expect("published checkpoint");
    entry.signed_commit = Some(signed);
    entry.signature = Some(signature);
}

fn checkpoint_asset_refs(relay: &FakeRelay, index: usize) -> Vec<[u8; 32]> {
    relay
        .published_checkpoints
        .get(index)
        .expect("checkpoint")
        .data_refs
        .iter()
        .filter(|object_id| {
            relay
                .objects
                .get(*object_id)
                .is_some_and(|object| object.commit.object_type == ObjectType::Asset)
        })
        .copied()
        .collect()
}

#[test]
fn fallback_uses_its_own_lower_s_and_cursor_after_primary_commit_corruption() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_source(&source, &mut relay, &materials);
    flush_pending_and_checkpoint(&source, &mut relay, &materials);

    let last = relay.published_checkpoints.len() - 1;
    let fallback = relay.published_checkpoints[last - 1].clone();
    let primary = relay.published_checkpoints[last].clone();
    assert!(
        fallback.covered_seq < primary.covered_seq,
        "前置：fallback S 必须低于 primary S（S0<S1），实际 {} vs {}",
        fallback.covered_seq,
        primary.covered_seq
    );

    // commit 字节腐坏：不是"自洽 hash 通过"就能信，必须整体切 fallback。
    tamper_checkpoint_commit(&mut relay, last);

    let (_target_dir, target) = fresh_vault();
    let report = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect("commit 腐坏后必须用 fallback");
    assert!(report.used_fallback);
    assert_eq!(report.covered_seq, fallback.covered_seq);
    assert_eq!(
        report.cursor,
        format!("c{}", fallback.covered_seq),
        "cursor 必须用 fallback 自己的 S"
    );
    assert_eq!(
        target.get_meta(META_SYNC_RECEIVED_SEQ).expect("received"),
        Some(fallback.covered_seq.to_be_bytes().to_vec())
    );
    assert_eq!(
        target
            .get_meta(META_SYNC_INTEGRATED_SEQ)
            .expect("integrated"),
        Some(fallback.covered_seq.to_be_bytes().to_vec())
    );
    assert!(target
        .get_meta(META_SYNC_CONFIRMED_HIGHWATER)
        .expect("highwater")
        .is_some());
    let (note, _) =
        load_body_doc(&target, "note-1", materials.peer_id_u64(), None).expect("note-1");
    assert!(note.text().contains("第一行正文"));
}

#[test]
fn corrupt_asset_on_primary_falls_back_and_both_corrupt_import_nothing() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_source(&source, &mut relay, &materials);
    // 在 checkpoint 2 之后再插入一个**只被 primary 引用**的资产，才能单独腐坏 primary。
    let extra_asset = b"primary-only asset";
    let extra_hash: [u8; 32] = Sha256::digest(extra_asset).into();
    insert_asset(
        &source,
        &extra_hash,
        extra_asset.len() as u64,
        Some("text/plain"),
        extra_asset,
    )
    .expect("primary-only 资产");
    flush_pending_and_checkpoint(&source, &mut relay, &materials);

    let last = relay.published_checkpoints.len() - 1;
    let fallback = relay.published_checkpoints[last - 1].clone();
    let fallback_refs = fallback.data_refs.clone();
    let primary_only_assets: Vec<[u8; 32]> = checkpoint_asset_refs(&relay, last)
        .into_iter()
        .filter(|object_id| !fallback_refs.contains(object_id))
        .collect();
    assert!(
        !primary_only_assets.is_empty(),
        "前置：primary 必须引用了 fallback 没有的 ASSET 对象"
    );
    // 只腐坏 primary 独有的 ASSET 密文：下载/摘要/AEAD 阶段失败 → 整体 fallback。
    relay
        .parts
        .get_mut(&(primary_only_assets[0], 0))
        .expect("asset part")[0] ^= 0x01;

    let (_target_dir, target) = fresh_vault();
    let report = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect("asset 腐坏必须用 fallback");
    assert!(report.used_fallback);
    assert_eq!(report.covered_seq, fallback.covered_seq);
    assert_eq!(report.cursor, format!("c{}", fallback.covered_seq));
    let asset_rows = target
        .query("SELECT COUNT(*) FROM assets", &[])
        .expect("asset count");
    assert_eq!(
        asset_rows[0][0],
        Value::Integer(1),
        "只允许导入 fallback 自己的资产，不得写入 primary 独有资产"
    );
    assert!(
        menghunote_storage::asset_meta_by_hash(&target, &extra_hash)
            .expect("asset meta")
            .is_none(),
        "primary 独有资产不得出现在 fallback 导入结果里"
    );

    // 两份基线的 ASSET 都腐坏：零写入、零水位，绝不 ACK。
    let fallback_index = last - 1;
    for object_id in checkpoint_asset_refs(&relay, fallback_index) {
        if let Some(part) = relay.parts.get_mut(&(object_id, 0)) {
            part[0] ^= 0x01;
        }
    }
    let (_second_dir, second_target) = fresh_vault();
    let error = bootstrap::run(
        &second_target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect_err("两份 asset 都坏必须失败");
    assert!(!format!("{error:?}").is_empty());
    assert!(
        list_all_docs(&second_target)
            .expect("list_all_docs")
            .is_empty(),
        "失败前不得写半库"
    );
    assert!(
        baseline_apply_state(&second_target)
            .expect("journal")
            .is_none(),
        "失败前不得建 staging journal"
    );
    assert_no_sync_watermarks(&second_target);
}

#[test]
fn corrupt_signature_on_both_baselines_fails_without_import_or_ack() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_source(&source, &mut relay, &materials);

    for index in 0..relay.published_checkpoints.len() {
        tamper_checkpoint_signature(&mut relay, index);
    }

    let (_target_dir, target) = fresh_vault();
    let error = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect_err("两份 checkpoint 签名都坏必须失败");
    assert!(!format!("{error:?}").is_empty());
    assert!(list_all_docs(&target).expect("list_all_docs").is_empty());
    assert!(baseline_apply_state(&target).expect("journal").is_none());
    assert_no_sync_watermarks(&target);
}

#[test]
fn revision_1_baselines_are_rejected_by_client_consumers() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_source(&source, &mut relay, &materials);
    assert!(relay.published_checkpoints.len() >= 2);
    for index in 0..relay.published_checkpoints.len() {
        rewrite_checkpoint_validation_version(&mut relay, &materials, index, 1);
    }

    let (_target_dir, target) = fresh_vault();
    let error = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect_err("修订 1 的签名基线必须被客户端拒绝（不得静默接受旧基线）");
    assert!(
        format!("{error:?}").contains("UnsupportedVersion")
            || format!("{error}").contains("协议版本"),
        "错误必须指向不支持的验证修订，实际：{error}"
    );
    assert!(list_all_docs(&target).expect("list_all_docs").is_empty());
    assert_no_sync_watermarks(&target);
}

// ============================================================ 评审 0002 T16/T17 剩余
// fallback 阶段仍必须真实验证 structure/codec/actual VV；任一基线腐坏即整体回退，
// 两基线都坏必须零写入、零水位、零 journal、零 ACK。

/// 用真实 root 重加密 manifest 并重签 checkpoint commit（模拟 prepare 产物被篡改）。
fn install_manifest(
    relay: &mut FakeRelay,
    root: &RootSecretV1,
    materials: &genesis::GenesisMaterials,
    index: usize,
    manifest: &menghunote_sync_proto::ManifestV1,
) {
    use menghunote_sync_proto::{
        checkpoint_signing_input, refs_digest_input, CheckpointCommit, ObjectRefV1, ObjectType,
    };
    let checkpoint = relay
        .published_checkpoints
        .get_mut(index)
        .expect("published checkpoint")
        .clone();
    let manifest_oid = checkpoint.manifest_ref.expect("manifest ref");
    let plaintext = manifest.encode().expect("manifest encode");
    let (parts, manifest_digest) = menghunote_sync::upload::encrypt_local_parts(
        root,
        ObjectType::Manifest,
        &manifest_oid,
        [0x42; 16],
        materials.device_id,
        0,
        0,
        &plaintext,
        &[],
    )
    .expect("reencrypt manifest");
    let old = relay
        .objects
        .get_mut(&manifest_oid)
        .expect("manifest object");
    for (part_no, envelope, _) in &parts {
        relay
            .parts
            .insert((manifest_oid, *part_no), envelope.clone());
    }
    old.commit.part_count = parts.len() as u32;
    old.commit.total_len = plaintext.len() as u64;
    old.commit.object_digest = manifest_digest;
    old.commit_bytes = old.commit.encode().expect("manifest commit");

    let refs: Vec<ObjectRefV1> = manifest
        .docs
        .iter()
        .map(|doc| ObjectRefV1 {
            object_id: doc.snapshot_object_id,
            object_type: ObjectType::Snapshot,
            generation: doc.generation,
            key_epoch: doc.key_epoch,
            part_count: doc.part_count,
            total_plain_len: doc.total_plain_len,
            object_digest: doc.object_digest,
        })
        .chain(manifest.assets.iter().map(|asset| ObjectRefV1 {
            object_id: asset.object_id,
            object_type: ObjectType::Asset,
            generation: 0,
            key_epoch: asset.key_epoch,
            part_count: asset.part_count,
            total_plain_len: asset.total_plain_len,
            object_digest: asset.object_digest,
        }))
        .collect();
    let refs_digest: [u8; 32] =
        Sha256::digest(refs_digest_input(&refs).expect("refs digest input")).into();
    let commit = CheckpointCommit {
        vault_id: VAULT_ID,
        checkpoint_id: checkpoint.checkpoint_id,
        epoch: checkpoint.epoch,
        covered_relay_seq: checkpoint.covered_seq,
        manifest_object_id: manifest_oid,
        manifest_digest,
        refs_digest,
        producer_device_id: materials.device_id,
        validation_version: 2,
        validation_digest: [0x5A; 32],
    };
    let signed_commit = commit.encode().expect("checkpoint commit");
    let signature = materials
        .signing_key
        .sign(&checkpoint_signing_input(&commit).expect("checkpoint signing input"));
    let entry = relay
        .published_checkpoints
        .get_mut(index)
        .expect("published checkpoint");
    entry.signed_commit = Some(signed_commit);
    entry.signature = Some(signature);
}

fn load_manifest(
    relay: &mut FakeRelay,
    root: &RootSecretV1,
    index: usize,
) -> menghunote_sync_proto::ManifestV1 {
    let checkpoint = relay.published_checkpoints[index].clone();
    let manifest_oid = checkpoint.manifest_ref.expect("manifest ref");
    let commit = relay.objects[&manifest_oid].commit.clone();
    let (_, bytes) = menghunote_sync::upload::fetch_object(
        relay,
        root,
        ObjectType::Manifest,
        &manifest_oid,
        commit.part_count,
        commit.total_len,
        &commit.object_digest,
        &[],
    )
    .expect("fetch manifest");
    menghunote_sync_proto::ManifestV1::decode(&bytes).expect("decode manifest")
}

/// 重写某个 baseline 的 structure（或第一个 body）快照并同步 manifest/commit。
fn rewrite_snapshot(
    relay: &mut FakeRelay,
    root: &RootSecretV1,
    materials: &genesis::GenesisMaterials,
    index: usize,
    kind: u64,
    mutate: impl FnOnce(&mut menghunote_sync_proto::SnapshotV1),
) {
    use menghunote_sync_proto::{ObjectType, SnapshotV1};
    let mut manifest = load_manifest(relay, root, index);
    let doc_index = manifest
        .docs
        .iter()
        .position(|doc| doc.kind == kind)
        .expect("目标 kind 文档");
    let doc = manifest.docs[doc_index].clone();
    let (_, bytes) = menghunote_sync::upload::fetch_object(
        relay,
        root,
        ObjectType::Snapshot,
        &doc.snapshot_object_id,
        doc.part_count,
        doc.total_plain_len,
        &doc.object_digest,
        &[],
    )
    .expect("fetch snapshot");
    let mut snapshot = SnapshotV1::decode(&bytes).expect("decode snapshot");
    mutate(&mut snapshot);
    let plaintext = snapshot.encode().expect("snapshot encode");
    let (parts, digest) = menghunote_sync::upload::encrypt_local_parts(
        root,
        ObjectType::Snapshot,
        &doc.snapshot_object_id,
        [0x43; 16],
        materials.device_id,
        doc.generation,
        0,
        &plaintext,
        &[],
    )
    .expect("reencrypt snapshot");
    {
        let object = relay
            .objects
            .get_mut(&doc.snapshot_object_id)
            .expect("snapshot object");
        for (part_no, envelope, _) in &parts {
            relay
                .parts
                .insert((doc.snapshot_object_id, *part_no), envelope.clone());
        }
        object.commit.part_count = parts.len() as u32;
        object.commit.total_len = plaintext.len() as u64;
        object.commit.object_digest = digest;
        object.commit_bytes = object.commit.encode().expect("snapshot commit");
    }
    manifest.docs[doc_index].part_count = parts.len() as u32;
    manifest.docs[doc_index].total_plain_len = plaintext.len() as u64;
    manifest.docs[doc_index].object_digest = digest;
    install_manifest(relay, root, materials, index, &manifest);
}

fn corrupt_structure_manifest(
    relay: &mut FakeRelay,
    root: &RootSecretV1,
    materials: &genesis::GenesisMaterials,
    index: usize,
) {
    let mut manifest = load_manifest(relay, root, index);
    manifest.structure_doc_id = "missing-structure".to_owned();
    install_manifest(relay, root, materials, index, &manifest);
}

fn corrupt_codec(
    relay: &mut FakeRelay,
    root: &RootSecretV1,
    materials: &genesis::GenesisMaterials,
    index: usize,
) {
    rewrite_snapshot(
        relay,
        root,
        materials,
        index,
        menghunote_sync::checkpoint::KIND_STRUCTURE,
        |snapshot| {
            snapshot.codec_version = "loro-v0".to_owned();
        },
    );
}

fn corrupt_actual_vv(
    relay: &mut FakeRelay,
    root: &RootSecretV1,
    materials: &genesis::GenesisMaterials,
    index: usize,
) {
    use menghunote_sync::checkpoint::KIND_STRUCTURE;
    let mut manifest = load_manifest(relay, root, index);
    let doc = manifest
        .docs
        .iter_mut()
        .find(|doc| doc.kind == KIND_STRUCTURE)
        .expect("structure doc");
    let mut vv = loro::VersionVector::decode(&doc.vv).expect("vv");
    vv.insert(0xDEAD, 7);
    doc.vv = vv.encode();
    install_manifest(relay, root, materials, index, &manifest);
}

fn run_bootstrap_with_target(
    relay: &mut FakeRelay,
    materials: &genesis::GenesisMaterials,
) -> (
    tempfile::TempDir,
    menghunote_storage::Vault,
    Result<menghunote_sync::bootstrap::BootstrapReport, menghunote_sync::SyncError>,
) {
    let (dir, target) = fresh_vault();
    let result = bootstrap::run(
        &target,
        relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    );
    (dir, target, result)
}

#[test]
fn fallback_rejects_corrupt_structure_and_both_corrupt_writes_nothing() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_structured_source(&source, &mut relay, &materials);
    let last = relay.published_checkpoints.len() - 1;

    // 只腐坏 primary 的 structure 声明 → 整体回退 fallback，且只写 fallback 的 S/cursor。
    corrupt_structure_manifest(&mut relay, &materials.root, &materials, last);
    let fallback_s = relay.published_checkpoints[last - 1].covered_seq;
    let (_target_dir, target, result) = run_bootstrap_with_target(&mut relay, &materials);
    let report = result.expect("structure 腐坏必须回退 fallback");
    assert!(
        report.used_fallback,
        "primary structure 腐坏必须 used_fallback"
    );
    assert_eq!(report.covered_seq, fallback_s);
    assert_eq!(report.cursor, format!("c{fallback_s}"));
    let notes = list_notes(&target).expect("notes");
    assert_eq!(notes.len(), 1, "fallback 必须恢复自己的结构笔记");
    let (body, _) = load_body_doc(
        &target,
        notes[0]
            .body_doc_id
            .as_deref()
            .expect("notes 行必须有正文 doc id"),
        materials.peer_id_u64(),
        None,
    )
    .expect("body");
    assert_eq!(body.text(), STRUCTURED_TEXT);

    // 两基线 structure 都坏 → 失败且零写入/零水位/零 journal。
    let acks_before = relay.acks.len();
    corrupt_structure_manifest(&mut relay, &materials.root, &materials, last - 1);
    let (_second_dir, second_target, result) = run_bootstrap_with_target(&mut relay, &materials);
    assert!(result.is_err(), "两基线 structure 都坏必须失败");
    assert_eq!(relay.acks.len(), acks_before, "失败不得 ACK");
    assert!(list_all_docs(&second_target).expect("docs").is_empty());
    assert!(baseline_apply_state(&second_target)
        .expect("journal")
        .is_none());
    assert_no_sync_watermarks(&second_target);
}

#[test]
fn fallback_rejects_unreadable_codec_and_both_corrupt_writes_nothing() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_structured_source(&source, &mut relay, &materials);
    let last = relay.published_checkpoints.len() - 1;

    corrupt_codec(&mut relay, &materials.root, &materials, last);
    let fallback_s = relay.published_checkpoints[last - 1].covered_seq;
    let (_target_dir, target, result) = run_bootstrap_with_target(&mut relay, &materials);
    let report = result.expect("codec 腐坏必须回退 fallback");
    assert!(report.used_fallback);
    assert_eq!(report.covered_seq, fallback_s);
    assert_eq!(report.cursor, format!("c{fallback_s}"));
    assert_eq!(list_notes(&target).expect("notes").len(), 1);

    let acks_before = relay.acks.len();
    corrupt_codec(&mut relay, &materials.root, &materials, last - 1);
    let (_second_dir, second_target, result) = run_bootstrap_with_target(&mut relay, &materials);
    assert!(result.is_err(), "两基线 codec 都坏必须失败");
    assert_eq!(relay.acks.len(), acks_before, "失败不得 ACK");
    assert!(list_all_docs(&second_target).expect("docs").is_empty());
    assert!(baseline_apply_state(&second_target)
        .expect("journal")
        .is_none());
    assert_no_sync_watermarks(&second_target);
}

#[test]
fn fallback_rejects_inflated_actual_vv_and_both_corrupt_writes_nothing() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_structured_source(&source, &mut relay, &materials);
    let last = relay.published_checkpoints.len() - 1;

    // manifest 宣称的 VV 高于快照真实历史：不能只信声明，必须整体回退。
    corrupt_actual_vv(&mut relay, &materials.root, &materials, last);
    let fallback_s = relay.published_checkpoints[last - 1].covered_seq;
    let (_target_dir, target, result) = run_bootstrap_with_target(&mut relay, &materials);
    let report = result.expect("actual VV 虚高必须回退 fallback");
    assert!(report.used_fallback);
    assert_eq!(report.covered_seq, fallback_s);
    assert_eq!(report.cursor, format!("c{fallback_s}"));
    assert_eq!(list_notes(&target).expect("notes").len(), 1);

    let acks_before = relay.acks.len();
    corrupt_actual_vv(&mut relay, &materials.root, &materials, last - 1);
    let (_second_dir, second_target, result) = run_bootstrap_with_target(&mut relay, &materials);
    assert!(result.is_err(), "两基线 actual VV 都虚高必须失败");
    assert_eq!(relay.acks.len(), acks_before, "失败不得 ACK");
    assert!(list_all_docs(&second_target).expect("docs").is_empty());
    assert!(baseline_apply_state(&second_target)
        .expect("journal")
        .is_none());
    assert_no_sync_watermarks(&second_target);
}

// ---------------------------------------------------------------------------
// ADR 0013 §3：bootstrap 历史 anchor（rev 前进不能丢、不能跳过）
// ---------------------------------------------------------------------------

/// 第一条 rev2 管理转换（`to_epoch = EPOCH + 1`）：比当前 epoch 更晚，属于"未来"。
fn future_manager_transition(instance_id: [u8; 16]) -> menghunote_sync::relay::ManagerChainEntry {
    let transition = menghunote_sync::rotation::ManagerTransitionV1 {
        vault_id: VAULT_ID,
        instance_id,
        rotation_id: [0x0b; 16],
        from_epoch: EPOCH,
        to_epoch: EPOCH + 1,
        old_rev: 1,
        new_rev: 2,
        new_pubkey: [0x02; 32],
        covered_seq: 0,
    }
    .encode()
    .expect("管理转换编码");
    menghunote_sync::relay::ManagerChainEntry {
        rev: 2,
        pubkey: [0x02; 32],
        transition,
        old_rev: 1,
        old_signature: Some([0x03; 64]),
        new_signature: Some([0x04; 64]),
    }
}

/// 首次 bootstrap 用一条 rev1 genesis 管理链（真实 relay 的 `/control` 键 5 必然有它）。
fn seed_manager_chain(relay: &mut FakeRelay, materials: &genesis::GenesisMaterials) {
    relay.manager_chain = vec![menghunote_sync::relay::ManagerChainEntry {
        rev: 1,
        pubkey: materials.manager.public_key(),
        transition: b"test-genesis".to_vec(),
        old_rev: 1,
        old_signature: None,
        new_signature: None,
    }];
}

#[test]
fn bootstrap_anchor_is_recomputed_from_the_epoch_history_when_rev_advances() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_source(&source, &mut relay, &materials);
    seed_manager_chain(&mut relay, &materials);

    let (_target_dir, target) = fresh_vault();
    bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect("首次 bootstrap");
    let first_rev = relay.control_rev;
    let anchor_hash = target
        .get_meta(META_BOOTSTRAP_CONTROL_HASH)
        .expect("control hash")
        .expect("锚必须写入");

    // control_rev 前进 + 管理链多一条"未来 epoch"的转换：旧锚必须按旧 epoch 的链前缀
    // 复算，而不是拿新 rev / 全链复算（那会算出不同摘要），也不能干脆忽略。
    relay.control_rev = first_rev + 1;
    relay
        .manager_chain
        .push(future_manager_transition(relay.instance_id));
    let state = relay.state().expect("state");
    let chain = relay.management_chain().expect("chain");
    let full_chain_hash = control_head_hash(&state, &chain);
    assert_ne!(
        full_chain_hash.as_slice(),
        anchor_hash.as_slice(),
        "新 rev + 全链的摘要必须与旧锚不同（否则这条用例证明不了历史视图被用到）"
    );

    bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect("rev 前进但历史前缀未变时必须允许（按 epoch 截断后的旧视图验证）");
    // pins 前进到新 rev / 新摘要。
    assert_eq!(
        target
            .get_meta(META_BOOTSTRAP_CONTROL_REV)
            .expect("control rev")
            .as_deref(),
        Some((first_rev + 1).to_be_bytes().as_slice())
    );
}

#[test]
fn bootstrap_anchor_rejects_a_rewritten_prefix_when_rev_advances() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_source(&source, &mut relay, &materials);
    seed_manager_chain(&mut relay, &materials);

    let (_target_dir, target) = fresh_vault();
    bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect("首次 bootstrap");
    let first_rev = relay.control_rev;

    // rev 前进，但 genesis 那条被换掉（历史前缀被重写）。
    relay.control_rev = first_rev + 1;
    relay.manager_chain[0].transition = b"rewritten-genesis".to_vec();
    let error = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect_err("旧锚未被新链覆盖必须拒绝，不能当 rev 前进就跳过");
    assert!(
        matches!(error, menghunote_sync::SyncError::Equivocation(_)),
        "{error:?}"
    );
    // 失败路径不得改写本地 pin。
    assert_eq!(
        target
            .get_meta(META_BOOTSTRAP_CONTROL_REV)
            .expect("control rev")
            .as_deref(),
        Some(first_rev.to_be_bytes().as_slice())
    );
}

#[test]
fn bootstrap_anchor_same_rev_hash_swap_is_equivocation() {
    let (_dir, source, materials) = setup();
    let mut relay = new_relay(&materials);
    seed_source(&source, &mut relay, &materials);
    seed_manager_chain(&mut relay, &materials);

    let (_target_dir, target) = fresh_vault();
    bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect("首次 bootstrap");

    // 同 rev 换摘要：必须 equivocation（不是"没有锚"、也不是普通 Object 错误）。
    let mut swapped = target
        .get_meta(META_BOOTSTRAP_CONTROL_HASH)
        .expect("control hash")
        .expect("锚");
    swapped[0] ^= 0xFF;
    target
        .set_meta(META_BOOTSTRAP_CONTROL_HASH, &swapped)
        .expect("改写摘要");
    let error = bootstrap::run(
        &target,
        &mut relay,
        VAULT_ID,
        materials.peer_id_u64(),
        &materials.root,
    )
    .expect_err("同 rev 换 hash 必须拒绝");
    assert!(
        matches!(error, menghunote_sync::SyncError::Equivocation(_)),
        "{error:?}"
    );
}

`````


---

# 附录 C：完整代码 diff（`5d01e9e..HEAD`）



@@SECTION code/077 `crates/storage/src/lib.rs`


`````rust
//! menghunote 的本地存储层。
//!
//! 本 crate 只负责「SQLite 单库真源」这一层（见 `docs/adr/0001-storage-format.md`）：
//!
//! - 打开 / 创建 vault 数据库，并按 ADR 0001 §3.9 配置与**读回校验** PRAGMA；
//! - 基于 `PRAGMA user_version` 的逐级迁移，v1 为 ADR §7 附录 DDL 全量建表；
//! - 单写者 actor：一个专用线程独占 [`rusqlite::Connection`]，外部按命令交互；
//! - 正文 CRDT（Loro 1.17.1）：每个 body doc 一个 [`BodyDoc`]，
//!   单位换算的唯一实现在 [`menghunote_text`]（本 crate 实测结论见 body_doc 注释与 tests/loro_units.rs）；
//! - A 事务（[`store::append_updates`]）：append `loro_updates` + 推进 head + 写 outbox；
//! - 未 ACK journal + 背压（[`journal`]）；
//! - B 事务骨架（[`materialize`]）：从固定 frontier 物化 `notes.body_md`，旧任务不覆盖新结果。
//!
//! 本切片**不包含**：编辑器 UI/GPUI、Tantivy、relay、加密、同步协议。
//!
//! 用法：
//!
//! ```no_run
//! use menghunote_storage::{BodyDoc, Value, Vault};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let vault = Vault::open("D:/notes/vault.db")?;
//! vault.exec("INSERT INTO meta(key, value) VALUES(?1, ?2)", &[
//!     Value::Text("schema_owner".to_owned()),
//!     Value::Blob(b"menghunote".to_vec()),
//! ])?;
//!
//! let doc = BodyDoc::create("body-1", 1)?;
//! doc.insert_scalars(0, "A中😀e\u{301}")?;
//! assert_eq!(doc.scalar_len(), 5);
//! assert_eq!(vault.user_version()?, 4);
//! # Ok(())
//! # }
//! ```

mod actor;
pub mod asset_gc;
mod asset_store;
mod baseline_apply;
mod body_doc;
mod connection;
mod error;
mod events;
pub mod history;
mod index_manifest;
mod journal;
pub mod links;
mod materialize;
mod migrate;
mod projector;
mod read_api;
mod read_snapshot;
mod remote_known;
pub mod rename_links;
pub mod session;
mod store;
mod structure;
mod structure_store;
mod sync_bridge;
mod sync_meta;
mod tree_view;

pub use actor::{AffectedGuard, Row, Statement, Vault};
pub use asset_gc::{scan_asset_gc, AssetGcCandidate, AssetGcReport, ASSET_GC_MAX_CANDIDATES};
pub use asset_store::{
    asset_content_hash, asset_meta_by_hash, count_unreferenced_assets, insert_asset,
    link_asset_ref, list_asset_refs, list_referenced_assets, replace_asset_refs,
    set_asset_orphan_since, unlink_asset_ref, AssetRef, AssetRefReconcile,
};
pub use baseline_apply::{
    abort_baseline_apply, apply_baseline_assets, apply_baseline_batch,
    baseline_apply_debug_counters, baseline_apply_debug_timings, baseline_apply_state,
    baseline_apply_verify_ms, baseline_verify_failure, begin_baseline_apply, finish_baseline_apply,
    mark_baseline_verify_failed, record_baseline_verify_time, reset_baseline_apply_debug,
    BaselineApplyDebugCounters, BaselineApplyDebugTimings, BaselineApplyFinish, BaselineApplyState,
    BaselineApplyStatus, BaselineAsset, BaselineExpectation, BASELINE_MERGE_BATCH,
    META_BASELINE_EXPECTED, META_BASELINE_VERIFY_FAILED, META_SYNC_RELAY_CURSOR,
};
pub use body_doc::{
    BodyDoc, PendingUpdate, APP_DOC_SCHEMA, BODY_CONTAINER, CODEC_VERSION, MIN_READER,
};
pub use error::StorageError;
pub use events::{StorageEvent, StorageEventSubscriber, DEFAULT_EVENT_QUEUE};
pub use history::{
    capture_note_version, list_note_versions, load_note_version, load_note_version_for_note,
    prune_note_versions, CaptureNoteVersion, HistoryVersion, HistoryVersionSummary,
    HISTORY_MAX_VERSIONS_PER_NOTE, HISTORY_RETENTION_DAYS, HISTORY_SOURCE_LOCAL,
};
pub use index_manifest::{list_index_manifest, IndexManifestRow};
pub use journal::{
    DurableSink, FlushReport, JournalEntry, JournalOrigin, JournalWriter, PendingJournal,
    VaultJournalSink,
};
pub use links::{list_backlinks, Backlink};
pub use materialize::{
    materialize_body, materialize_body_profiled, pending_outbox, project_pending, reconcile,
    reconcile_batched, scan_stale_bodies, MaterializeOutcome, MaterializeProgress,
    MaterializeStageTimings, MaterializeSummary, PendingProjection, StaleBody,
};
pub use migrate::SCHEMA_VERSION;
pub use projector::{project, trashed_notes, visible_notes, NoteView, ProjectionReport};
pub use read_api::{
    asset_bytes, asset_meta, body_doc_meta, doc_exists, doc_snapshot, doc_updates_since, doc_vv,
    list_all_docs, list_assets, loro_update_base_vv, loro_update_payload, read_doc_bootstrap,
    read_doc_for_export, AssetMeta, BodyDocMeta, DocBootstrap, DocExport, DocKind, DocMeta,
    DocSnapshot, StoredLoroUpdate,
};
pub use read_snapshot::{existing_vault_has_content, ReadSnapshot};
pub use rusqlite::types::Value;
pub use store::{
    append_updates, append_updates_with_integrated_marks, create_body_doc, create_note_slot,
    end_frontier_at, end_vv_at, load_body_doc, load_body_state, load_head, store_snapshot,
    BodyState, HeadState, UpdateRecord,
};
pub use structure::{
    normalize_tag, NodeSnapshot, NoteSeed, StructureDoc, StructureSnapshot, TagSnapshot,
    STRUCTURE_DOC_ID,
};
pub use structure_store::{ensure_structure_row, load_structure, StructureWriter};
pub use sync_bridge::{
    advance_integrated_prefix, allocate_local_seq_for_import, append_updates_with_sync_intents,
    can_recycle_update, count_inbox_ready, enqueue_sync_intent, intent_message_id,
    intent_object_id, local_sync_intent, mark_acked, mark_inbox_quarantine, mark_integrated,
    mark_integrated_batch, mark_sent, mark_skipped_event_received, outbox_parts,
    outbox_repack_candidates, outbox_row, outbox_rows, pending_encryption, pending_send,
    repack_outbox, repack_required, set_confirmed_highwater, set_repack_required, set_sync_log_id,
    store_encrypted_parts, store_inbox_verified, store_inbox_verified_batch, sync_cursor_state,
    take_inbox_ready, take_inbox_ready_where, update_referenced_by_outbox, OutboxKey,
    OutboxRepackCandidate, RepackOutcome, RepackPart, RepackPartSummary, RepackSpec, SyncContext,
    SyncCursorState, SyncEncryptedPart, SyncInboxRow, SyncInboxState, SyncIntent, SyncOutboxPart,
    SyncOutboxRow, SyncOutboxState, VerifiedInboxRow, META_SYNC_CONFIRMED_HIGHWATER,
    META_SYNC_INTEGRATED_SEQ, META_SYNC_LOG_ID, META_SYNC_RECEIVED_SEQ, META_SYNC_REPACK_REQUIRED,
    META_SYNC_SKIPPED_RECEIVED_PREFIX, SYNC_INTENT_ID_DOMAIN,
};
pub use tree_view::{list_notes, list_tree, list_tree_sorted, NodeKind, TreeEntry, TreeSort};

`````



@@SECTION code/078 `crates/storage/src/structure.rs`


`````rust
//! 结构 CRDT：每 vault 一个结构 `LoroDoc`（`loro_docs.kind='structure'`）。
//!
//! 权威归属（ADR 0001 §3.1）：
//!
//! - 结构 `LoroTree` 只承载**笔记导航树**，不是 Markdown 块树；
//! - 节点 `meta`（`LoroMap`）保存 `title` / `note_id` / `body_doc_id` / `created_at` /
//!   标签成员 / 收藏 / 置顶 / 删除与恢复事件；
//! - 全局标签字典是根上另一个 `LoroMap`（tag_id → {name, normalized_name}）；
//! - 删除语义（ADR 0001 §3.3）：**grow-only 删除事件 ID** + **已观察到的恢复确认 ID**，
//!   有效删除 = 存在未被确认的删除事件。**不用** `LoroTree::delete`，也不用 SQL `DELETE`。
//!
//! 本模块只做 CRDT 侧；落库与投影见 [`crate::structure_store`] / [`crate::projector`]。

use loro::{ExportMode, Frontiers, LoroDoc, LoroMap, LoroTree, LoroValue, TreeID, VersionVector};

use crate::body_doc::PendingUpdate;
use crate::error::StorageError;

/// 结构文档在 `loro_docs` 里的固定 doc_id（每 vault 一个）。
pub const STRUCTURE_DOC_ID: &str = "structure";
/// 导航树容器名。
pub const TREE_CONTAINER: &str = "tree";
/// 全局标签字典容器名。
pub const TAG_DICT_CONTAINER: &str = "tag_dict";
/// 根级元数据（本机计数器等）容器名。
pub const ROOT_META_CONTAINER: &str = "meta";

/// 节点 meta：标题。
pub const KEY_TITLE: &str = "title";
/// 节点 meta：稳定 note_id。
pub const KEY_NOTE_ID: &str = "note_id";
/// 节点 meta：正文文档 ID。
pub const KEY_BODY_DOC_ID: &str = "body_doc_id";
/// 节点 meta：创建时间（客户端编辑戳）。
pub const KEY_CREATED_AT: &str = "created_at";
/// 节点 meta：最后一次结构改动戳。
pub const KEY_EDITED_AT: &str = "edited_at";
/// 节点 meta：收藏。
pub const KEY_FAVORITE: &str = "is_favorite";
/// 节点 meta：置顶。
pub const KEY_PINNED: &str = "is_pinned";
/// 节点 meta：标签成员集合（子 `LoroMap`，key = tag_id）。
pub const KEY_TAGS: &str = "tags";
/// 节点 meta：grow-only 删除事件列表（子 `LoroList`）。
pub const KEY_DELETE_EVENTS: &str = "delete_events";
/// 节点 meta：已确认（已见过）的删除事件集合（子 `LoroMap`）。
pub const KEY_RESTORE_ACKS: &str = "restore_acks";
/// 标签字典项：显示名。
const KEY_TAG_NAME: &str = "name";
/// 标签字典项：归一化名。
const KEY_TAG_NORMALIZED: &str = "normalized_name";

/// 新建笔记时返回的三个稳定 ID。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteSeed {
    /// 树节点 ID（`TreeID` 的字符串形式）。
    pub node_id: String,
    /// 永久稳定的笔记 ID。
    pub note_id: String,
    /// 对应的正文文档 ID。
    pub body_doc_id: String,
}

/// 一个标签字典项。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagSnapshot {
    /// 标签 ID。
    pub tag_id: String,
    /// 原始显示名。
    pub name: String,
    /// 归一化名（用于查重，不设 UNIQUE）。
    pub normalized_name: String,
}

/// 一个树节点的有效状态（投影器直接消费）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeSnapshot {
    /// `TreeID` 字符串。
    pub node_id: String,
    /// 父节点（根级为 `None`）。
    pub parent: Option<String>,
    /// 同级 dense rank（从遍历顺序生成，不用 native fractional index）。
    pub index: usize,
    /// 标题。
    pub title: String,
    /// 笔记 ID（纯目录节点为 `None`）。
    pub note_id: Option<String>,
    /// 正文文档 ID。
    pub body_doc_id: Option<String>,
    /// 创建时间。
    pub created_at: i64,
    /// 最后结构改动时间。
    pub edited_at: i64,
    /// 收藏。
    pub is_favorite: bool,
    /// 置顶。
    pub is_pinned: bool,
    /// 有效删除（存在未被确认的删除事件）。
    pub deleted: bool,
    /// 全部删除事件（grow-only）。
    pub delete_events: Vec<String>,
    /// 已被恢复确认的事件。
    pub confirmed_events: Vec<String>,
    /// 标签 ID 列表（顺序稳定：按 tag_id 排序）。
    pub tags: Vec<String>,
}

impl NodeSnapshot {
    /// 节点是否是笔记（有 note_id 与 body_doc_id）。
    pub fn is_note(&self) -> bool {
        self.note_id.is_some() && self.body_doc_id.is_some()
    }
}

/// 整棵结构树的快照。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructureSnapshot {
    /// 节点（先序 DFS 顺序）。
    pub nodes: Vec<NodeSnapshot>,
    /// 标签字典。
    pub tags: Vec<TagSnapshot>,
}

/// 结构 CRDT 文档。
pub struct StructureDoc {
    doc: LoroDoc,
    tree: LoroTree,
    tag_dict: LoroMap,
    root_meta: LoroMap,
}

impl std::fmt::Debug for StructureDoc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StructureDoc")
            .field("peer_id", &self.doc.peer_id())
            .field("nodes", &self.tree.nodes().len())
            .finish()
    }
}

impl StructureDoc {
    /// 新建结构文档并设置本机 peer id。
    pub fn create(peer_id: u64) -> Result<Self, StorageError> {
        let doc = LoroDoc::new();
        doc.set_peer_id(peer_id)?;
        let this = Self {
            tree: doc.get_tree(TREE_CONTAINER),
            tag_dict: doc.get_map(TAG_DICT_CONTAINER),
            root_meta: doc.get_map(ROOT_META_CONTAINER),
            doc,
        };
        // 启用 fractional index：本切片移动只需 append，但将来要插到指定位置时不必改协议
        this.tree.enable_fractional_index(0);
        Ok(this)
    }

    /// 从快照 + 后续 update 重放。
    pub fn restore(
        peer_id: u64,
        snapshot: Option<&[u8]>,
        updates: &[Vec<u8>],
    ) -> Result<Self, StorageError> {
        let this = Self::create(peer_id)?;
        if let Some(snapshot) = snapshot {
            this.doc.import(snapshot)?;
        }
        for payload in updates {
            this.doc.import(payload)?;
        }
        // fractional index 配置不在 oplog 里，重放后重新打开，保证后续 move 可用
        this.tree.enable_fractional_index(0);
        Ok(this)
    }

    /// 本机 peer id。
    pub fn peer_id(&self) -> u64 {
        self.doc.peer_id()
    }

    /// 当前 frontier。
    pub fn frontiers(&self) -> Frontiers {
        self.doc.state_frontiers()
    }

    /// 当前 oplog 版本向量。
    pub fn vv(&self) -> VersionVector {
        self.doc.oplog_vv()
    }

    /// 自 `from` 之后的新增 ops。
    pub fn pending_update(&self, from: &VersionVector) -> Result<PendingUpdate, StorageError> {
        let payload = self.doc.export(ExportMode::updates(from))?;
        Ok(PendingUpdate {
            payload,
            end_frontier: self.frontiers().encode(),
            end_vv: self.vv().encode(),
        })
    }

    /// 全量历史 update。
    pub fn export_all_updates(&self) -> Result<Vec<u8>, StorageError> {
        Ok(self.doc.export(ExportMode::all_updates())?)
    }

    /// 完整快照。
    pub fn export_snapshot(&self) -> Result<Vec<u8>, StorageError> {
        Ok(self.doc.export(ExportMode::Snapshot)?)
    }

    /// 导入 update / snapshot（幂等）。
    pub fn import(&self, bytes: &[u8]) -> Result<(), StorageError> {
        self.doc.import(bytes)?;
        Ok(())
    }

    /// 新建一篇笔记：根级树节点 + 全部 meta 字段 + 预先建好的子容器。
    ///
    /// 子容器先建好，是为了避免两个 peer 并发「首次打标签 / 首次删除」时各建一个同名容器。
    pub fn create_note(&self, title: &str, now: i64) -> Result<NoteSeed, StorageError> {
        let node = self.tree.create(None::<TreeID>)?;
        let note_seq = self.next_seq("note")?;
        let note_id = format!("note-{}-{note_seq:06}", self.doc.peer_id());
        let body_doc_id = format!("{note_id}-body");

        let meta = self.tree.get_meta(node)?;
        meta.insert(KEY_TITLE, title)?;
        meta.insert(KEY_NOTE_ID, note_id.as_str())?;
        meta.insert(KEY_BODY_DOC_ID, body_doc_id.as_str())?;
        meta.insert(KEY_CREATED_AT, now)?;
        meta.insert(KEY_EDITED_AT, now)?;
        meta.insert(KEY_FAVORITE, false)?;
        meta.insert(KEY_PINNED, false)?;
        meta.ensure_mergeable_map(KEY_TAGS)?;
        meta.ensure_mergeable_list(KEY_DELETE_EVENTS)?;
        meta.ensure_mergeable_map(KEY_RESTORE_ACKS)?;
        self.doc.commit();

        Ok(NoteSeed {
            node_id: node.to_string(),
            note_id,
            body_doc_id,
        })
    }

    /// 新建一个**目录**节点：与 [`StructureDoc::create_note`] 同一条写入路径，只是不写
    /// `note_id` / `body_doc_id`（目录没有正文文档，投影时落在那张 `notes` 表之外）。
    ///
    /// 返回 `TreeID` 字符串（移动/改名/嵌套都用它）。
    pub fn create_folder(&self, title: &str, now: i64) -> Result<String, StorageError> {
        let node = self.tree.create(None::<TreeID>)?;
        let meta = self.tree.get_meta(node)?;
        meta.insert(KEY_TITLE, title)?;
        meta.insert(KEY_CREATED_AT, now)?;
        meta.insert(KEY_EDITED_AT, now)?;
        meta.insert(KEY_FAVORITE, false)?;
        meta.insert(KEY_PINNED, false)?;
        meta.ensure_mergeable_map(KEY_TAGS)?;
        meta.ensure_mergeable_list(KEY_DELETE_EVENTS)?;
        meta.ensure_mergeable_map(KEY_RESTORE_ACKS)?;
        self.doc.commit();
        Ok(node.to_string())
    }

    /// 改名。
    pub fn rename(&self, node_id: &str, title: &str, now: i64) -> Result<(), StorageError> {
        let meta = self.node_meta(node_id)?;
        meta.insert(KEY_TITLE, title)?;
        meta.insert(KEY_EDITED_AT, now)?;
        self.doc.commit();
        Ok(())
    }

    /// 设置收藏。
    pub fn set_favorite(&self, node_id: &str, value: bool, now: i64) -> Result<(), StorageError> {
        let meta = self.node_meta(node_id)?;
        meta.insert(KEY_FAVORITE, value)?;
        meta.insert(KEY_EDITED_AT, now)?;
        self.doc.commit();
        Ok(())
    }

    /// 设置置顶。
    pub fn set_pinned(&self, node_id: &str, value: bool, now: i64) -> Result<(), StorageError> {
        let meta = self.node_meta(node_id)?;
        meta.insert(KEY_PINNED, value)?;
        meta.insert(KEY_EDITED_AT, now)?;
        self.doc.commit();
        Ok(())
    }

    /// 移动节点（挂到 `parent` 末尾；`parent = None` 表示移到根级）。
    pub fn move_node(
        &self,
        node_id: &str,
        parent: Option<&str>,
        now: i64,
    ) -> Result<(), StorageError> {
        let target = self.parse_node(node_id)?;
        let parent_id = match parent {
            Some(parent) => Some(self.parse_node(parent)?),
            None => None,
        };
        self.tree.mov(target, parent_id)?;
        let meta = self.tree.get_meta(target)?;
        meta.insert(KEY_EDITED_AT, now)?;
        self.doc.commit();
        Ok(())
    }

    /// 确保标签存在并返回 tag_id。
    ///
    /// tag_id 由**归一化名派生**（`tag-<normalized>`），不依赖本地计数器：
    /// 两副本并发创建同名标签会得到同一个 ID，于是成员集合天然去重、字典也不会出现两份。
    /// 这与 DDL 里 `tags.normalized_name` 不设 UNIQUE 不冲突（那只是兜底）。
    pub fn ensure_tag(&self, name: &str) -> Result<String, StorageError> {
        let normalized = normalize_tag(name);
        if normalized.is_empty() {
            return Err(StorageError::InvalidTagName {
                name: name.to_owned(),
            });
        }
        // 已有标签（含远端并发创建的同一个 ID）直接复用
        if let Some(existing) = self.find_tag(&normalized) {
            return Ok(existing);
        }

        let tag_id = format!("tag-{normalized}");
        let entry = self.tag_dict.ensure_mergeable_map(tag_id.as_str())?;
        entry.insert(KEY_TAG_NAME, name.trim())?;
        entry.insert(KEY_TAG_NORMALIZED, normalized.as_str())?;
        self.doc.commit();
        Ok(tag_id)
    }

    /// 给节点加标签（集合语义，重复加不产生重复项）。
    pub fn add_tag(&self, node_id: &str, name: &str, now: i64) -> Result<String, StorageError> {
        let tag_id = self.ensure_tag(name)?;
        let meta = self.node_meta(node_id)?;
        let tags = meta.ensure_mergeable_map(KEY_TAGS)?;
        tags.insert(tag_id.as_str(), true)?;
        meta.insert(KEY_EDITED_AT, now)?;
        self.doc.commit();
        Ok(tag_id)
    }

    /// 从节点移除标签（只删成员，不删字典项）。
    pub fn remove_tag(&self, node_id: &str, tag_id: &str, now: i64) -> Result<(), StorageError> {
        let meta = self.node_meta(node_id)?;
        let tags = meta.ensure_mergeable_map(KEY_TAGS)?;
        tags.delete(tag_id)?;
        meta.insert(KEY_EDITED_AT, now)?;
        self.doc.commit();
        Ok(())
    }

    /// 删除笔记：追加一个 **grow-only** 删除事件，返回事件 ID。
    ///
    /// 注意：这里**不删树节点**（ADR 0001 §3.3 禁止用 `LoroTree::delete` 当删除真源）。
    pub fn delete_note(&self, node_id: &str, now: i64) -> Result<String, StorageError> {
        let meta = self.node_meta(node_id)?;
        let events = meta.ensure_mergeable_list(KEY_DELETE_EVENTS)?;
        let event_id = format!("del-{}-{}", self.doc.peer_id(), self.next_seq("event")?);
        events.push(event_id.as_str())?;
        meta.insert(KEY_EDITED_AT, now)?;
        self.doc.commit();
        Ok(event_id)
    }

    /// 恢复笔记：只确认**当前已观察到**的删除事件，返回本次新确认的条数。
    ///
    /// 并发的新删除事件不在确认集合里，所以恢复之后它仍然生效（ADR 0001 §3.3）。
    pub fn restore_note(&self, node_id: &str, now: i64) -> Result<usize, StorageError> {
        let meta = self.node_meta(node_id)?;
        let events = meta.ensure_mergeable_list(KEY_DELETE_EVENTS)?;
        let acks = meta.ensure_mergeable_map(KEY_RESTORE_ACKS)?;

        let mut confirmed = 0usize;
        for index in 0..events.len() {
            let Some(value) = events.get(index) else {
                continue;
            };
            let LoroValue::String(event_id) = value.get_deep_value() else {
                continue;
            };
            if acks.get(event_id.as_str()).is_none() {
                acks.insert(event_id.as_str(), true)?;
                confirmed += 1;
            }
        }
        meta.insert(KEY_EDITED_AT, now)?;
        self.doc.commit();
        Ok(confirmed)
    }

    /// 节点当前是否有效删除。
    pub fn is_deleted(&self, node_id: &str) -> Result<bool, StorageError> {
        let meta = self.node_meta(node_id)?;
        let deep = meta.get_deep_value();
        Ok(effective_deleted(&deep))
    }

    /// 节点快照（含同级 dense rank）。
    pub fn node_snapshot(&self, node_id: &str) -> Result<NodeSnapshot, StorageError> {
        let node = self.parse_node(node_id)?;
        let parent = match self.tree.parent(node) {
            Some(loro::TreeParentId::Node(parent)) => Some(parent),
            _ => None,
        };
        let index = self
            .tree
            .children(parent)
            .unwrap_or_default()
            .iter()
            .position(|sibling| *sibling == node)
            .unwrap_or(0);
        self.build_snapshot(node, parent, index)
    }

    /// 整棵树 + 标签字典的快照（先序 DFS）。
    pub fn snapshot(&self) -> Result<StructureSnapshot, StorageError> {
        let mut nodes = Vec::new();
        self.walk(None, &mut nodes)?;
        Ok(StructureSnapshot {
            nodes,
            tags: self.tag_snapshots(),
        })
    }

    /// 标签字典快照（按 tag_id 排序，保证可比）。
    pub fn tag_snapshots(&self) -> Vec<TagSnapshot> {
        let deep = self.tag_dict.get_deep_value();
        let mut tags = Vec::new();
        if let LoroValue::Map(entries) = &deep {
            for (tag_id, value) in entries.iter() {
                let LoroValue::Map(fields) = value else {
                    continue;
                };
                let name = fields
                    .get(KEY_TAG_NAME)
                    .and_then(as_string)
                    .unwrap_or_default();
                let normalized_name = fields
                    .get(KEY_TAG_NORMALIZED)
                    .and_then(as_string)
                    .unwrap_or_else(|| normalize_tag(&name));
                tags.push(TagSnapshot {
                    tag_id: tag_id.clone(),
                    name,
                    normalized_name,
                });
            }
        }
        tags.sort_by(|a, b| a.tag_id.cmp(&b.tag_id));
        tags
    }

    fn walk(
        &self,
        parent: Option<TreeID>,
        out: &mut Vec<NodeSnapshot>,
    ) -> Result<(), StorageError> {
        let Some(children) = self.tree.children(parent) else {
            return Ok(());
        };
        for (index, node) in children.into_iter().enumerate() {
            out.push(self.build_snapshot(node, parent, index)?);
            self.walk(Some(node), out)?;
        }
        Ok(())
    }

    fn build_snapshot(
        &self,
        node: TreeID,
        parent: Option<TreeID>,
        index: usize,
    ) -> Result<NodeSnapshot, StorageError> {
        let meta = self.tree.get_meta(node)?;
        let deep = meta.get_deep_value();

        Ok(NodeSnapshot {
            node_id: node.to_string(),
            parent: parent.map(|parent| parent.to_string()),
            index,
            title: string_field(&deep, KEY_TITLE).unwrap_or_default(),
            note_id: string_field(&deep, KEY_NOTE_ID),
            body_doc_id: string_field(&deep, KEY_BODY_DOC_ID),
            created_at: i64_field(&deep, KEY_CREATED_AT).unwrap_or(0),
            edited_at: i64_field(&deep, KEY_EDITED_AT).unwrap_or(0),
            is_favorite: bool_field(&deep, KEY_FAVORITE).unwrap_or(false),
            is_pinned: bool_field(&deep, KEY_PINNED).unwrap_or(false),
            deleted: effective_deleted(&deep),
            delete_events: string_list_field(&deep, KEY_DELETE_EVENTS),
            confirmed_events: {
                let mut confirmed = map_keys_field(&deep, KEY_RESTORE_ACKS);
                confirmed.sort();
                confirmed
            },
            tags: {
                let mut tags = map_keys_field(&deep, KEY_TAGS);
                tags.sort();
                tags
            },
        })
    }

    fn node_meta(&self, node_id: &str) -> Result<LoroMap, StorageError> {
        let node = self.parse_node(node_id)?;
        self.tree.get_meta(node).map_err(StorageError::from)
    }

    fn parse_node(&self, node_id: &str) -> Result<TreeID, StorageError> {
        let node = TreeID::try_from(node_id).map_err(StorageError::from)?;
        if !self.tree.contains(node) {
            return Err(StorageError::UnknownNode {
                node_id: node_id.to_owned(),
            });
        }
        Ok(node)
    }

    fn find_tag(&self, normalized: &str) -> Option<String> {
        let deep = self.tag_dict.get_deep_value();
        let LoroValue::Map(entries) = &deep else {
            return None;
        };
        for (tag_id, value) in entries.iter() {
            let LoroValue::Map(fields) = value else {
                continue;
            };
            if fields
                .get(KEY_TAG_NORMALIZED)
                .and_then(as_string)
                .is_some_and(|existing| existing == normalized)
            {
                return Some(tag_id.clone());
            }
        }
        None
    }

    fn next_seq(&self, kind: &str) -> Result<u64, StorageError> {
        let key = format!("{kind}_seq_{}", self.doc.peer_id());
        let current = self
            .root_meta
            .get(&key)
            .and_then(|value| match value.get_deep_value() {
                LoroValue::I64(value) if value >= 0 => Some(value as u64),
                _ => None,
            })
            .unwrap_or(0);
        let next = current + 1;
        self.root_meta
            .insert(key.as_str(), i64::try_from(next).unwrap_or(i64::MAX))?;
        Ok(next)
    }
}

/// 标签归一化：去首尾空白、小写、内部空白折叠成单空格。
pub fn normalize_tag(name: &str) -> String {
    name.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// 有效删除 = 存在未被恢复确认的删除事件。
fn effective_deleted(deep: &LoroValue) -> bool {
    let events = string_list_field(deep, KEY_DELETE_EVENTS);
    let confirmed: std::collections::HashSet<String> =
        map_keys_field(deep, KEY_RESTORE_ACKS).into_iter().collect();
    events.iter().any(|event| !confirmed.contains(event))
}

fn as_string(value: &LoroValue) -> Option<String> {
    match value {
        LoroValue::String(value) => Some(value.to_string()),
        _ => None,
    }
}

fn field<'a>(deep: &'a LoroValue, key: &str) -> Option<&'a LoroValue> {
    match deep {
        LoroValue::Map(entries) => entries.get(key),
        _ => None,
    }
}

fn string_field(deep: &LoroValue, key: &str) -> Option<String> {
    field(deep, key).and_then(as_string)
}

fn i64_field(deep: &LoroValue, key: &str) -> Option<i64> {
    match field(deep, key) {
        Some(LoroValue::I64(value)) => Some(*value),
        Some(LoroValue::Double(value)) => Some(*value as i64),
        _ => None,
    }
}

fn bool_field(deep: &LoroValue, key: &str) -> Option<bool> {
    match field(deep, key) {
        Some(LoroValue::Bool(value)) => Some(*value),
        _ => None,
    }
}

fn string_list_field(deep: &LoroValue, key: &str) -> Vec<String> {
    match field(deep, key) {
        Some(LoroValue::List(items)) => items.iter().filter_map(as_string).collect(),
        _ => Vec::new(),
    }
}

fn map_keys_field(deep: &LoroValue, key: &str) -> Vec<String> {
    match field(deep, key) {
        Some(LoroValue::Map(entries)) => entries.keys().cloned().collect(),
        _ => Vec::new(),
    }
}

`````



@@SECTION code/079 `crates/storage/src/read_api.rs`


`````rust
//! S7-7：出站加密/bootstrap 所需的只读查询 API。
//!
//! sync 不再直连 `loro_updates` / `loro_docs` 表；本模块把它们收口成 storage 的
//! 公共读接口。`read_doc_for_export` 还会做 head CAS 复核，避免导出期间被新编辑
//! 悄悄改到一半。

use rusqlite::types::Value;

use crate::error::StorageError;
use crate::store;
use crate::Vault;

/// `loro_docs.kind`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocKind {
    Structure,
    Body,
}

impl DocKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Structure => "structure",
            Self::Body => "body",
        }
    }
}

impl std::fmt::Display for DocKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl PartialEq<&str> for DocKind {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl PartialEq<DocKind> for &str {
    fn eq(&self, other: &DocKind) -> bool {
        *self == other.as_str()
    }
}

/// `loro_docs` 的正文文档元数据（出站 UpdateV1 / checkpoint 需要）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BodyDocMeta {
    pub doc_id: String,
    pub kind: DocKind,
    pub generation: u64,
    pub codec_version: String,
    pub app_doc_schema: u32,
    pub min_reader: u32,
    pub head_seq: u64,
    pub head_frontier: Option<Vec<u8>>,
    pub head_vv: Option<Vec<u8>>,
}

/// 全量文档枚举的别名；语义同 [`BodyDocMeta`]（含 structure/tombstone）。
pub type DocMeta = BodyDocMeta;

/// 一条 `loro_updates` 记录（出站打包需要的明文列）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredLoroUpdate {
    pub doc_id: String,
    pub seq: u64,
    pub update_id: String,
    pub batch_id: String,
    pub payload: Vec<u8>,
    pub end_frontier: Vec<u8>,
    pub end_vv: Vec<u8>,
}

/// 一次 head-CAS 导出快照。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocExport {
    pub meta: BodyDocMeta,
    pub updates: Vec<StoredLoroUpdate>,
}

/// 文档的持久化快照元数据（bootstrap 用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocSnapshot {
    pub doc_id: String,
    pub snapshot: Option<Vec<u8>>,
    pub snapshot_seq: u64,
    pub snapshot_frontier: Option<Vec<u8>>,
    pub head_seq: u64,
    pub head_frontier: Option<Vec<u8>>,
    pub head_vv: Option<Vec<u8>>,
}

/// 从快照恢复所需的最小读取结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocBootstrap {
    pub meta: BodyDocMeta,
    pub snapshot: Option<Vec<u8>>,
    pub snapshot_seq: u64,
    pub snapshot_frontier: Option<Vec<u8>>,
    /// `seq > snapshot_seq` 且 `seq <= head_seq` 的 update，升序。
    pub updates_since_snapshot: Vec<StoredLoroUpdate>,
}

/// 读正文文档元数据；不存在返回 `None`。
pub fn body_doc_meta(vault: &Vault, doc_id: &str) -> Result<Option<BodyDocMeta>, StorageError> {
    let rows = vault.query(
        "SELECT kind, generation, codec_version, app_doc_schema, min_reader, head_seq, head_frontier, head_vv \
         FROM loro_docs WHERE doc_id=?1",
        &[Value::Text(doc_id.to_owned())],
    )?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    Ok(Some(BodyDocMeta {
        doc_id: doc_id.to_owned(),
        kind: doc_kind_of(row, 0)?,
        generation: u64_of(row, 1, "loro_docs.generation")?,
        codec_version: text_of(row, 2, "loro_docs.codec_version")?,
        app_doc_schema: u32_of(row, 3, "loro_docs.app_doc_schema")?,
        min_reader: u32_of(row, 4, "loro_docs.min_reader")?,
        head_seq: u64_of(row, 5, "loro_docs.head_seq")?,
        head_frontier: optional_blob(row, 6)?,
        head_vv: optional_blob(row, 7)?,
    }))
}

/// 遍历 `loro_docs` 全表（structure + 所有 body，含无 note 行的墓碑）。
pub fn list_all_docs(vault: &Vault) -> Result<Vec<DocMeta>, StorageError> {
    let rows = vault.query(
        "SELECT doc_id, kind, generation, codec_version, app_doc_schema, min_reader, \
         head_seq, head_frontier, head_vv FROM loro_docs ORDER BY doc_id ASC",
        &[],
    )?;
    rows.iter()
        .map(|row| {
            Ok(BodyDocMeta {
                doc_id: text_of(row, 0, "loro_docs.doc_id")?,
                kind: doc_kind_of(row, 1)?,
                generation: u64_of(row, 2, "loro_docs.generation")?,
                codec_version: text_of(row, 3, "loro_docs.codec_version")?,
                app_doc_schema: u32_of(row, 4, "loro_docs.app_doc_schema")?,
                min_reader: u32_of(row, 5, "loro_docs.min_reader")?,
                head_seq: u64_of(row, 6, "loro_docs.head_seq")?,
                head_frontier: optional_blob(row, 7)?,
                head_vv: optional_blob(row, 8)?,
            })
        })
        .collect()
}

/// 资产元数据；不包含 `assets.data`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssetMeta {
    pub asset_hash: [u8; 32],
    pub byte_len: Option<i64>,
    pub media_type: Option<String>,
    pub has_data: bool,
    pub orphan_since: Option<i64>,
}

/// 列出全部资产元数据（不加载 data），按 hash 升序。
pub fn list_assets(vault: &Vault) -> Result<Vec<AssetMeta>, StorageError> {
    let rows = vault.query(
        "SELECT asset_hash, byte_len, media_type, data IS NOT NULL, orphan_since \
         FROM assets ORDER BY asset_hash ASC",
        &[],
    )?;
    rows.iter()
        .map(|row| {
            Ok(AssetMeta {
                asset_hash: asset_hash_of(row, 0)?,
                byte_len: optional_i64(row, 1)?,
                media_type: optional_text(row, 2)?,
                has_data: bool_of(row, 3, "assets.data IS NOT NULL")?,
                orphan_since: optional_i64(row, 4)?,
            })
        })
        .collect()
}

/// 单个资产元数据；不存在返回 `None`。
pub fn asset_meta(
    vault: &Vault,
    hash: impl AsRef<[u8]>,
) -> Result<Option<AssetMeta>, StorageError> {
    let hash = hash_blob(hash.as_ref())?;
    let rows = vault.query(
        "SELECT asset_hash, byte_len, media_type, data IS NOT NULL, orphan_since \
         FROM assets WHERE asset_hash=?1",
        &[Value::Blob(hash)],
    )?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    Ok(Some(AssetMeta {
        asset_hash: asset_hash_of(row, 0)?,
        byte_len: optional_i64(row, 1)?,
        media_type: optional_text(row, 2)?,
        has_data: bool_of(row, 3, "assets.data IS NOT NULL")?,
        orphan_since: optional_i64(row, 4)?,
    }))
}

/// 读资产原始字节；资产不存在或 `data IS NULL` 都返回 `None`。
pub fn asset_bytes(vault: &Vault, hash: impl AsRef<[u8]>) -> Result<Option<Vec<u8>>, StorageError> {
    let hash = hash_blob(hash.as_ref())?;
    let rows = vault.query(
        "SELECT data FROM assets WHERE asset_hash=?1",
        &[Value::Blob(hash)],
    )?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    match row.first() {
        Some(Value::Null) | None => Ok(None),
        Some(Value::Blob(data)) => Ok(Some(data.clone())),
        other => Err(StorageError::Corrupt {
            context: format!("assets.data 类型异常：{other:?}"),
        }),
    }
}

/// 文档是否存在（入站 update 前检查依赖是否就绪）。
pub fn doc_exists(vault: &Vault, doc_id: &str) -> Result<bool, StorageError> {
    let rows = vault.query(
        "SELECT 1 FROM loro_docs WHERE doc_id=?1",
        &[Value::Text(doc_id.to_owned())],
    )?;
    Ok(!rows.is_empty())
}

/// 读指定 `(doc_id, seq)` 的 update；不存在视为损坏/缺失。
pub fn loro_update_payload(
    vault: &Vault,
    doc_id: &str,
    seq: u64,
) -> Result<StoredLoroUpdate, StorageError> {
    let rows = vault.query(
        "SELECT update_id, batch_id, payload, end_frontier, end_vv \
         FROM loro_updates WHERE doc_id=?1 AND seq=?2",
        &[Value::Text(doc_id.to_owned()), Value::Integer(i64_of(seq)?)],
    )?;
    let row = rows.first().ok_or_else(|| StorageError::Corrupt {
        context: format!("loro_updates 缺少 {doc_id}/{seq}"),
    })?;
    Ok(StoredLoroUpdate {
        doc_id: doc_id.to_owned(),
        seq,
        update_id: text_of(row, 0, "loro_updates.update_id")?,
        batch_id: text_of(row, 1, "loro_updates.batch_id")?,
        payload: blob_of(row, 2, "loro_updates.payload")?,
        end_frontier: blob_of(row, 3, "loro_updates.end_frontier")?,
        end_vv: blob_of(row, 4, "loro_updates.end_vv")?,
    })
}

/// `seq` 之前的 VV（`seq <= 1` 时为空字节串）；供出站 `base_vv` 使用。
pub fn loro_update_base_vv(vault: &Vault, doc_id: &str, seq: u64) -> Result<Vec<u8>, StorageError> {
    if seq <= 1 {
        return Ok(Vec::new());
    }
    Ok(loro_update_payload(vault, doc_id, seq - 1)?.end_vv)
}

/// `seq > after_seq` 的 update，按 seq 升序；`after_seq=0` 返回全部。
///
/// 只返回 `seq <= head_seq` 的行（正常库不存在更长的行）。
pub fn doc_updates_since(
    vault: &Vault,
    doc_id: &str,
    after_seq: u64,
) -> Result<Vec<StoredLoroUpdate>, StorageError> {
    let meta = body_doc_meta(vault, doc_id)?.ok_or_else(|| StorageError::Corrupt {
        context: format!("loro_docs 缺少 {doc_id}"),
    })?;
    let rows = vault.query(
        "SELECT seq, update_id, batch_id, payload, end_frontier, end_vv \
         FROM loro_updates WHERE doc_id=?1 AND seq>?2 AND seq<=?3 ORDER BY seq ASC",
        &[
            Value::Text(doc_id.to_owned()),
            Value::Integer(i64_of(after_seq)?),
            Value::Integer(i64_of(meta.head_seq)?),
        ],
    )?;
    rows.iter()
        .map(|row| {
            Ok(StoredLoroUpdate {
                doc_id: doc_id.to_owned(),
                seq: u64_of(row, 0, "loro_updates.seq")?,
                update_id: text_of(row, 1, "loro_updates.update_id")?,
                batch_id: text_of(row, 2, "loro_updates.batch_id")?,
                payload: blob_of(row, 3, "loro_updates.payload")?,
                end_frontier: blob_of(row, 4, "loro_updates.end_frontier")?,
                end_vv: blob_of(row, 5, "loro_updates.end_vv")?,
            })
        })
        .collect()
}

/// 读当前 head VV；文档不存在返回 `None`。
pub fn doc_vv(vault: &Vault, doc_id: &str) -> Result<Option<Vec<u8>>, StorageError> {
    Ok(body_doc_meta(vault, doc_id)?.and_then(|meta| meta.head_vv))
}

/// 读快照装载入口：`loro_docs.snapshot/snapshot_seq/snapshot_frontier`。
pub fn doc_snapshot(vault: &Vault, doc_id: &str) -> Result<Option<DocSnapshot>, StorageError> {
    let rows = vault.query(
        "SELECT snapshot, snapshot_seq, snapshot_frontier, head_seq, head_frontier, head_vv \
         FROM loro_docs WHERE doc_id=?1",
        &[Value::Text(doc_id.to_owned())],
    )?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    Ok(Some(DocSnapshot {
        doc_id: doc_id.to_owned(),
        snapshot: optional_blob(row, 0)?,
        snapshot_seq: u64_of(row, 1, "loro_docs.snapshot_seq")?,
        snapshot_frontier: optional_blob(row, 2)?,
        head_seq: u64_of(row, 3, "loro_docs.head_seq")?,
        head_frontier: optional_blob(row, 4)?,
        head_vv: optional_blob(row, 5)?,
    }))
}

/// 从已有快照 + 后续 update 组装 bootstrap 读取包（无快照时 snapshot 为 None）。
pub fn read_doc_bootstrap(
    vault: &Vault,
    doc_id: &str,
) -> Result<Option<DocBootstrap>, StorageError> {
    let Some(snapshot) = doc_snapshot(vault, doc_id)? else {
        return Ok(None);
    };
    let meta = body_doc_meta(vault, doc_id)?.ok_or_else(|| StorageError::Corrupt {
        context: format!("loro_docs 缺少 {doc_id}"),
    })?;
    let updates_since_snapshot = doc_updates_since(vault, doc_id, snapshot.snapshot_seq)?;
    let after = body_doc_meta(vault, doc_id)?.ok_or_else(|| StorageError::Corrupt {
        context: format!("loro_docs 缺少 {doc_id}"),
    })?;
    if after.head_seq != snapshot.head_seq {
        return Err(StorageError::HeadConflict {
            doc_id: doc_id.to_owned(),
            expected: snapshot.head_seq,
            actual: after.head_seq,
        });
    }
    Ok(Some(DocBootstrap {
        meta,
        snapshot: snapshot.snapshot,
        snapshot_seq: snapshot.snapshot_seq,
        snapshot_frontier: snapshot.snapshot_frontier,
        updates_since_snapshot,
    }))
}

/// 按指定 head 做一次一致性导出；head 不匹配返回 [`StorageError::HeadConflict`]。
///
/// 读取期间若 head 被新编辑推进，也会在末尾复核时返回冲突；绝不返回半截快照。
pub fn read_doc_for_export(
    vault: &Vault,
    doc_id: &str,
    expected_head_seq: u64,
) -> Result<DocExport, StorageError> {
    let meta = body_doc_meta(vault, doc_id)?.ok_or_else(|| StorageError::Corrupt {
        context: format!("loro_docs 缺少 {doc_id}"),
    })?;
    if meta.head_seq != expected_head_seq {
        return Err(StorageError::HeadConflict {
            doc_id: doc_id.to_owned(),
            expected: expected_head_seq,
            actual: meta.head_seq,
        });
    }

    let updates = doc_updates_since(vault, doc_id, 0)?;

    // 复核：若读到一半 head 变了，不能把它当成 expected_head_seq 的完整导出。
    let after = store::load_head(vault, doc_id)?;
    if after.head_seq != expected_head_seq {
        return Err(StorageError::HeadConflict {
            doc_id: doc_id.to_owned(),
            expected: expected_head_seq,
            actual: after.head_seq,
        });
    }
    let expected_last = updates.last().map(|update| update.seq).unwrap_or(0);
    if expected_last != expected_head_seq {
        return Err(StorageError::Corrupt {
            context: format!(
                "{doc_id} head={expected_head_seq} 但 update 最后 seq={expected_last}"
            ),
        });
    }
    Ok(DocExport { meta, updates })
}

fn text_of(row: &crate::Row, index: usize, field: &str) -> Result<String, StorageError> {
    match row.get(index) {
        Some(Value::Text(value)) => Ok(value.clone()),
        other => Err(StorageError::Corrupt {
            context: format!("{field} 类型异常：{other:?}"),
        }),
    }
}

fn blob_of(row: &crate::Row, index: usize, field: &str) -> Result<Vec<u8>, StorageError> {
    match row.get(index) {
        Some(Value::Blob(value)) => Ok(value.clone()),
        other => Err(StorageError::Corrupt {
            context: format!("{field} 类型异常：{other:?}"),
        }),
    }
}

fn optional_blob(row: &crate::Row, index: usize) -> Result<Option<Vec<u8>>, StorageError> {
    match row.get(index) {
        Some(Value::Null) | None => Ok(None),
        Some(Value::Blob(value)) => Ok(Some(value.clone())),
        other => Err(StorageError::Corrupt {
            context: format!("可选 BLOB 类型异常：{other:?}"),
        }),
    }
}

fn doc_kind_of(row: &crate::Row, index: usize) -> Result<DocKind, StorageError> {
    match text_of(row, index, "loro_docs.kind")?.as_str() {
        "structure" => Ok(DocKind::Structure),
        "body" => Ok(DocKind::Body),
        other => Err(StorageError::Corrupt {
            context: format!("loro_docs.kind 未知：{other}"),
        }),
    }
}

fn optional_text(row: &crate::Row, index: usize) -> Result<Option<String>, StorageError> {
    match row.get(index) {
        Some(Value::Null) | None => Ok(None),
        Some(Value::Text(value)) => Ok(Some(value.clone())),
        other => Err(StorageError::Corrupt {
            context: format!("可选 TEXT 类型异常：{other:?}"),
        }),
    }
}

fn optional_i64(row: &crate::Row, index: usize) -> Result<Option<i64>, StorageError> {
    match row.get(index) {
        Some(Value::Null) | None => Ok(None),
        Some(Value::Integer(value)) => Ok(Some(*value)),
        other => Err(StorageError::Corrupt {
            context: format!("可选 INTEGER 类型异常：{other:?}"),
        }),
    }
}

fn bool_of(row: &crate::Row, index: usize, field: &str) -> Result<bool, StorageError> {
    match row.get(index) {
        Some(Value::Integer(0)) => Ok(false),
        Some(Value::Integer(1)) => Ok(true),
        other => Err(StorageError::Corrupt {
            context: format!("{field} 非 0/1：{other:?}"),
        }),
    }
}

fn asset_hash_of(row: &crate::Row, index: usize) -> Result<[u8; 32], StorageError> {
    let blob = blob_of(row, index, "assets.asset_hash")?;
    blob.as_slice()
        .try_into()
        .map_err(|_| StorageError::Corrupt {
            context: format!("assets.asset_hash 长度不是 32：{}", blob.len()),
        })
}

fn hash_blob(hash: &[u8]) -> Result<Vec<u8>, StorageError> {
    if hash.len() != 32 {
        return Err(StorageError::Corrupt {
            context: format!("asset hash 长度不是 32：{}", hash.len()),
        });
    }
    Ok(hash.to_vec())
}

fn u64_of(row: &crate::Row, index: usize, field: &str) -> Result<u64, StorageError> {
    match row.get(index) {
        Some(Value::Integer(value)) => u64::try_from(*value).map_err(|_| StorageError::Corrupt {
            context: format!("{field} 为负数"),
        }),
        other => Err(StorageError::Corrupt {
            context: format!("{field} 类型异常：{other:?}"),
        }),
    }
}

fn u32_of(row: &crate::Row, index: usize, field: &str) -> Result<u32, StorageError> {
    u32::try_from(u64_of(row, index, field)?).map_err(|_| StorageError::Corrupt {
        context: format!("{field} 超出 u32"),
    })
}

fn i64_of(value: u64) -> Result<i64, StorageError> {
    i64::try_from(value).map_err(|_| StorageError::Corrupt {
        context: format!("数值 {value} 超出 SQLite INTEGER"),
    })
}

`````



@@SECTION code/081 `crates/storage/src/session/gate.rs`


`````rust
//! ADR 0009 切片 3a：**外部提交闸门** + **已提交增量 adopt**（只做存储层）。
//!
//! # 闸门
//!
//! 同步 worker 在把外部写入（`engine.integrate`）落进库之前，必须先让本地会话进入
//! [`GateState::Held`]，否则本地 flusher 的 head CAS 基准会被外部提交悄悄推过头：
//!
//! 1. [`DocumentSession::begin_external_commit`] 登记一张票据（记录当时的 `live_rev`）；
//! 2. [`DocumentSession::poll_external_commit`] 把状态驱到 Held：
//!    - composition 未结束 → [`GateState::WaitingIME`]（**绝不**强行结束 IME，也不触发 flush）；
//!    - 否则触发并等待 flush 到**明确目标**：`durable_through >= 目标 live_rev`
//!      且 journal `pending == 0`（两个值在 flusher 的同一临界区里推进，因此它等价于
//!      "覆盖该目标的 `DurableAck` 已经落地"，但不会和事件队列的其它消费者抢 ACK）；
//!    - 目标达成后再核对「会话 durable 基准 == `loro_docs.head_seq`」：不相等说明库里有
//!      没 adopt 的外部行，闸门直接 [`GateState::Failed`]，绝不带着过期基准进 Held；
//! 3. Held 期间本地写入一律 fail-closed：`apply` / `undo` / `redo` 返回
//!    [`super::protocol::RejectReason::NotReady`]，`begin_composition` 不进入组合，
//!    `apply_remote` 报 [`super::protocol::PortError`]；
//! 4. 外部提交完成后 [`DocumentSession::end_external_commit`] 释放；失败/取消用
//!    [`DocumentSession::cancel_external_commit`]——**两者都先把已 durable 的变化交付进
//!    live**，所以「同步失败」不会留下"库里有、live 没有"的撕裂状态。
//!
//! 闸门没有新线程、没有全局状态：状态挂在 [`DocumentSession`] 上，与 flush 的协作只读
//! [`JournalHandle`] 的共享水位（见 [`super::flush`]）。
//!
//! # adopt
//!
//! [`DocumentSession::adopt_committed_delta`] 把**已经 durable** 的外部 update 合进 live：
//!
//! - **不进 journal、不产生 sync intent**（不走会 journal 的 `apply_remote` 路径）；
//! - 先在不碰 live 的 scratch 副本上预演，校验 target frontier/vv 与库中连续的 payload，
//!   失败时 live 与 durable 水位**都不变**（Loro 导入不可回滚，所以校验必须在导入之前）；
//! - 成功后推进 `live_rev` / `live_frontier` / `journal_base_vv`，并把 flusher 的 head
//!   CAS 基准重定到外部提交的 head（**仅在 `pending == 0` 时**），后续本地 flush 不再
//!   `HeadConflict`；
//! - 同 head（空 delta）与重复交付同一份 delta 都幂等：不重复推进 `live_rev`。

use loro::{Frontiers, VersionVector};
use menghunote_text::UnicodeScalar;
use thiserror::Error;

use super::core::{import_updates, DocumentSession};
use super::flush::{lock, JournalHandle};
use super::protocol::{
    BatchId, ChangeSource, DeltaSplice, DocId, DurableAck, LiveDelta, LiveRev, MergeInfo,
    SessionEpoch, SessionEvent,
};
use crate::body_doc::BodyDoc;
use crate::error::StorageError;
use crate::read_api::doc_updates_since;
use crate::store;
use crate::Vault;

/// 闸门代际：每张票据/租约唯一，用来拒绝过期票据与跨代租约。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GateGeneration(pub u64);

/// 一张**待进入 Held** 的外部提交票据（由 [`DocumentSession::begin_external_commit`] 发）。
///
/// 票据只记录"进入闸门"的意图；真正的 durable 目标由 poll 用它建立时的 `live_rev`
/// 与当时的 live 一起确定，绝不拿"队列为空"当 quiescence。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalCommitTicket {
    doc_id: DocId,
    session_epoch: SessionEpoch,
    generation: GateGeneration,
    /// begin 时的 `live_rev`（Held 的目标 durable 水位下界）。
    target_live_rev: LiveRev,
    /// begin 时 flusher 已 durable 的 head seq（诊断/断言用）。
    baseline_head_seq: u64,
}

impl ExternalCommitTicket {
    /// 文档 ID。
    #[must_use]
    pub fn doc_id(&self) -> &str {
        &self.doc_id
    }

    /// 会话纪元。
    #[must_use]
    pub fn session_epoch(&self) -> SessionEpoch {
        self.session_epoch
    }

    /// 票据代际。
    #[must_use]
    pub fn generation(&self) -> GateGeneration {
        self.generation
    }

    /// begin 时的 live 修订号（Held 的目标 durable 水位下界）。
    #[must_use]
    pub fn target_live_rev(&self) -> LiveRev {
        self.target_live_rev
    }

    /// begin 时已 durable 的 head seq。
    #[must_use]
    pub fn baseline_head_seq(&self) -> u64 {
        self.baseline_head_seq
    }
}

/// 进入 [`GateState::Held`] 后拿到的租约：只有它能让外部提交写库、结束或 adopt。
///
/// 租约含 `doc_id` / `session_epoch` / `generation`：任何一项对不上都按
/// [`GateError::LeaseMismatch`] 拒绝，避免把上一轮闸门的结果套用到新提交上。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalCommitLease {
    doc_id: DocId,
    session_epoch: SessionEpoch,
    generation: GateGeneration,
    /// Held 时已达成的 durable 目标（= 该此刻的 `live_rev`）。
    target_live_rev: LiveRev,
    /// Held 时 flusher 已 durable 的 head seq（外部提交的 CAS 基准）。
    durable_head_seq: u64,
}

impl ExternalCommitLease {
    /// 文档 ID。
    #[must_use]
    pub fn doc_id(&self) -> &str {
        &self.doc_id
    }

    /// 会话纪元。
    #[must_use]
    pub fn session_epoch(&self) -> SessionEpoch {
        self.session_epoch
    }

    /// 租约代际。
    #[must_use]
    pub fn generation(&self) -> GateGeneration {
        self.generation
    }

    /// Held 时已达成的 durable 目标。
    #[must_use]
    pub fn target_live_rev(&self) -> LiveRev {
        self.target_live_rev
    }

    /// Held 时 flusher 已 durable 的 head seq。
    #[must_use]
    pub fn durable_head_seq(&self) -> u64 {
        self.durable_head_seq
    }
}

/// 闸门状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateState {
    /// IME composition 未结束：等它自然结束（闸门不强行结束、也不触发 flush）。
    WaitingIME,
    /// 正在触发/等待 flush 到明确目标。
    Flushing,
    /// 已持闸：外部提交可以写库；本地写入被拒绝。
    Held(ExternalCommitLease),
    /// 票据已失效（被释放、被新票据取代或从未登记）：调用方必须重新 begin。
    Stale,
    /// flush 明确失败 / 基准不一致：闸门**没有**静默释放，调用方应 retry 或 cancel。
    Failed(GateError),
}

/// 闸门错误（`begin` / `end` / `cancel` 的显式失败）。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum GateError {
    /// 已有票据或租约在途（重复 begin）。
    #[error("外部提交闸门已被代际 {generation:?} 占用")]
    AlreadyHeld {
        /// 占用中的代际。
        generation: GateGeneration,
    },
    /// 票据已失效。
    #[error("外部提交票据已失效（代际 {generation:?}）")]
    StaleTicket {
        /// 调用方持有的代际。
        generation: GateGeneration,
    },
    /// 当前不是 Held（还没有租约）。
    #[error("外部提交闸门当前不是 Held 状态")]
    NotHeld,
    /// 租约与当前持有者不匹配（doc/epoch/generation 任一对不上）。
    #[error("外部提交租约不匹配（当前 {expected:?}，调用方 {actual:?}）")]
    LeaseMismatch {
        /// 当前持有者的代际。
        expected: GateGeneration,
        /// 调用方给的代际。
        actual: GateGeneration,
    },
    /// 租约属于其它文档。
    #[error("外部提交租约属于其它文档（当前 {expected}，调用方 {actual}）")]
    WrongDoc {
        /// 当前文档。
        expected: DocId,
        /// 调用方给的文档。
        actual: DocId,
    },
    /// flush 明确失败（例如 head CAS 冲突）。
    #[error("flush 到 {target:?} 失败：{message}")]
    FlushFailed {
        /// 目标 live 修订号。
        target: LiveRev,
        /// 失败的底层信息。
        message: String,
    },
    /// 库中存在尚未 adopt 的外部行：不能带着过期基准进入 Held。
    #[error("库中 head seq {database_head_seq} 超出会话 durable {durable_head_seq}，必须先 adopt")]
    Unadopted {
        /// 会话 durable head seq。
        durable_head_seq: u64,
        /// 库中实际 head seq。
        database_head_seq: u64,
    },
    /// `applied` 与库中 head seq 不一致。
    #[error("applied head seq 与库中不一致（库中 {expected}，调用方 {actual}）")]
    HeadsMismatch {
        /// 库中 head seq。
        expected: u64,
        /// 调用方给的 head seq。
        actual: u64,
    },
    /// frontier / vv 与预期不一致。
    #[error("head 的 {field} 与预期不一致")]
    TargetMismatch {
        /// 不一致的字段（`frontier` / `vv`）。
        field: &'static str,
    },
    /// 仍有未 ACK 的本地更新：此时不能重定 durable 基准（fail-closed）。
    #[error("仍有 {pending} 条未 ACK 更新，不能结束外部提交")]
    Pending {
        /// 未 ACK 条数。
        pending: usize,
    },
    /// 已有外部行无法交付进 live（import/校验失败）。
    #[error("交付已 durable 的外部变更失败：{0}")]
    Delivery(String),
    /// 存储层错误。
    #[error("存储层错误：{0}")]
    Storage(String),
}

/// 已 durable 的 heads（`end_external_commit` 用它校验外部提交的落点）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AppliedHeads {
    /// `loro_docs.head_seq`。
    pub head_seq: u64,
    /// `loro_docs.head_frontier`（空 = 默认 frontier）。
    pub frontier: Vec<u8>,
    /// `loro_docs.head_vv`（空 = 空版本向量）。
    pub vv: Vec<u8>,
}

impl AppliedHeads {
    /// 从 [`crate::store::HeadState`] 取一份（NULL frontier/vv 会归一成默认编码）。
    #[must_use]
    pub fn from_head(head: &crate::store::HeadState) -> Self {
        Self {
            head_seq: head.head_seq,
            frontier: store::encoded_frontier(head),
            vv: head.head_vv.clone(),
        }
    }

    /// 读库中当前 head。
    pub fn load(vault: &Vault, doc_id: &str) -> Result<Self, StorageError> {
        Ok(Self::from_head(&store::load_head(vault, doc_id)?))
    }
}

/// 一份**已 durable**、待 adopt 的外部增量（`prepare_committed_delta` 产出）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedCommittedDelta {
    /// 文档 ID。
    pub doc_id: DocId,
    /// 起点：会话当前已 adopt 到的 durable head seq。
    pub base_seq: u64,
    /// 目标 head seq（= `base_seq + updates.len()`）。
    pub target_head_seq: u64,
    /// 目标 frontier（`Frontiers::encode`）。
    pub target_frontier: Vec<u8>,
    /// 目标 VV（`VersionVector::encode`）。
    pub target_vv: Vec<u8>,
    /// 连续覆盖 `base_seq + 1 ..= target_head_seq` 的 update payload；可为空（同 head）。
    pub updates: Vec<Vec<u8>>,
}

/// adopt 成功后交给 3b（SessionDriver/UI）的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdoptOutcome {
    /// 驱动 UI 镜像与选区变换的增量（`source = Remote`，`cause = None`）。
    pub delta: LiveDelta,
    /// 已 durable 的水位（外部提交的目标 head）。
    pub ack: DurableAck,
}

/// adopt 的错误：**任何**错误都保证 live 与 durable 水位未被改动。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AdoptError {
    /// 当前不是 Held。
    #[error("外部提交闸门当前不是 Held 状态，不能 adopt")]
    NotHeld,
    /// 租约不匹配。
    #[error("外部提交租约不匹配（当前 {expected:?}，调用方 {actual:?}）")]
    LeaseMismatch {
        /// 当前持有者的代际。
        expected: GateGeneration,
        /// 调用方给的代际。
        actual: GateGeneration,
    },
    /// delta 属于其它文档。
    #[error("delta 属于其它文档（当前 {expected}，delta {actual}）")]
    WrongDoc {
        /// 当前文档。
        expected: DocId,
        /// delta 的文档。
        actual: DocId,
    },
    /// 仍有未 ACK 的本地更新。
    #[error("仍有 {pending} 条未 ACK 更新，不能 adopt")]
    Pending {
        /// 未 ACK 条数。
        pending: usize,
    },
    /// updates 没有连续覆盖 `base_seq + 1 ..= target_head_seq`。
    #[error("updates 未连续覆盖 {base_seq}+1..={target_head_seq}（实际 {updates} 条）")]
    NonContiguous {
        /// 起点 seq。
        base_seq: u64,
        /// 目标 head seq。
        target_head_seq: u64,
        /// 实际 update 条数。
        updates: usize,
    },
    /// `base_seq` 与当前 durable head 不一致。
    #[error("delta 起点 {base_seq} 不是当前 durable head {durable_head_seq}")]
    StaleBase {
        /// delta 的起点。
        base_seq: u64,
        /// 当前 durable head seq。
        durable_head_seq: u64,
    },
    /// 目标 head seq 超过了库中已有的行。
    #[error("delta 目标 {target_head_seq} 超过库中 head {database_head_seq}")]
    TargetAhead {
        /// delta 的目标。
        target_head_seq: u64,
        /// 库中 head seq。
        database_head_seq: u64,
    },
    /// delta 的 payload 与库中同一 seq 的行不一致（不是"已 durable 的那份"）。
    #[error("delta 第 {seq} 条 payload 与库中不一致")]
    PayloadMismatch {
        /// 不一致的 seq。
        seq: u64,
    },
    /// 目标 frontier / vv 与预演结果不一致。
    #[error("目标 {field} 与导入结果不一致")]
    TargetMismatch {
        /// 不一致的字段（`frontier` / `vv`）。
        field: &'static str,
    },
    /// live 与库中 `base_seq` 的状态对不上（会话与库不同步，fail-closed）。
    #[error("live 与库中 base_seq 状态不一致：{context}")]
    Inconsistent {
        /// 具体上下文。
        context: String,
    },
    /// Loro 导入失败。
    #[error("导入已 durable 的 update 失败：{0}")]
    Import(String),
    /// 存储层错误。
    #[error("存储层错误：{0}")]
    Storage(String),
}

impl From<AdoptError> for GateError {
    fn from(error: AdoptError) -> Self {
        match error {
            AdoptError::NotHeld => Self::NotHeld,
            AdoptError::LeaseMismatch { expected, actual } => {
                Self::LeaseMismatch { expected, actual }
            }
            AdoptError::WrongDoc { expected, actual } => Self::WrongDoc { expected, actual },
            AdoptError::Pending { pending } => Self::Pending { pending },
            AdoptError::StaleBase {
                base_seq,
                durable_head_seq,
            } => Self::HeadsMismatch {
                expected: durable_head_seq,
                actual: base_seq,
            },
            other => Self::Delivery(other.to_string()),
        }
    }
}

/// 闸门内部状态（挂在 [`DocumentSession`] 上，无新线程/新全局状态）。
#[derive(Debug, Default)]
pub(crate) struct ExternalCommitGate {
    /// 已发出的最大代际（0 = 还没 begin 过）。
    next_generation: u64,
    /// 已登记但还没进入 Held 的票据。
    ticket: Option<TicketState>,
    /// 已持闸的租约。
    held: Option<ExternalCommitLease>,
}

#[derive(Debug)]
struct TicketState {
    ticket: ExternalCommitTicket,
    /// 已投递的 flush 目标（同一目标只投递一次，避免每帧 poll 重复塞命令）。
    submitted: Option<LiveRev>,
}

impl ExternalCommitGate {
    /// 是否处于 Held（本地写入必须被拒绝）。
    pub(crate) fn is_held(&self) -> bool {
        self.held.is_some()
    }

    /// 当前租约。
    pub(crate) fn held(&self) -> Option<&ExternalCommitLease> {
        self.held.as_ref()
    }

    /// 释放票据与租约（只有校验通过的 end/cancel 才允许调用）。
    fn release(&mut self) {
        self.ticket = None;
        self.held = None;
    }
}

/// 票据与给定代际/文档/会话是否同一张。
fn same_ticket(
    ticket: &ExternalCommitTicket,
    doc_id: &str,
    session_epoch: SessionEpoch,
    generation: GateGeneration,
) -> bool {
    ticket.doc_id == doc_id
        && ticket.session_epoch == session_epoch
        && ticket.generation == generation
}

fn normalize_frontier(bytes: &[u8]) -> Result<Vec<u8>, String> {
    if bytes.is_empty() {
        return Ok(Frontiers::default().encode());
    }
    Frontiers::decode(bytes)
        .map(|frontier| frontier.encode())
        .map_err(|error| error.to_string())
}

fn normalize_vv(bytes: &[u8]) -> Result<Vec<u8>, String> {
    if bytes.is_empty() {
        return Ok(VersionVector::default().encode());
    }
    VersionVector::decode(bytes)
        .map(|vv| vv.encode())
        .map_err(|error| error.to_string())
}

/// 从库里读出 `base_seq+1 ..= target_head_seq` 的连续 payload。
fn durable_updates_between(
    vault: &Vault,
    doc_id: &str,
    base_seq: u64,
    target_head_seq: u64,
) -> Result<Vec<Vec<u8>>, AdoptError> {
    if target_head_seq < base_seq {
        return Err(AdoptError::NonContiguous {
            base_seq,
            target_head_seq,
            updates: 0,
        });
    }
    if target_head_seq == base_seq {
        return Ok(Vec::new());
    }
    let rows = doc_updates_since(vault, doc_id, base_seq)
        .map_err(|error| AdoptError::Storage(error.to_string()))?;
    let mut updates = Vec::with_capacity(rows.len());
    let mut expected = base_seq + 1;
    for row in rows {
        if row.seq > target_head_seq {
            break;
        }
        if row.seq != expected {
            return Err(AdoptError::NonContiguous {
                base_seq,
                target_head_seq,
                updates: updates.len(),
            });
        }
        updates.push(row.payload);
        expected += 1;
    }
    if expected != target_head_seq + 1 {
        return Err(AdoptError::NonContiguous {
            base_seq,
            target_head_seq,
            updates: updates.len(),
        });
    }
    Ok(updates)
}

/// 从库中读出 `base_seq` 之后的连续增量（供 worker/测试构造 delta）。
///
/// 校验：`base_seq <= head`、seq 连续、最后一行就是 head、head 的 frontier/vv 与最后一行
/// 一致；没有新行时返回**空 delta**（`target_head_seq == base_seq`）。读取期间 head 被推进
/// 会返回 [`StorageError::HeadConflict`]，绝不返回半截 delta。
pub fn prepare_committed_delta(
    vault: &Vault,
    doc_id: &str,
    base_seq: u64,
) -> Result<PreparedCommittedDelta, StorageError> {
    let head = store::load_head(vault, doc_id)?;
    if head.head_seq < base_seq {
        return Err(StorageError::HeadConflict {
            doc_id: doc_id.to_owned(),
            expected: base_seq,
            actual: head.head_seq,
        });
    }

    let rows = doc_updates_since(vault, doc_id, base_seq)?;
    let mut updates = Vec::with_capacity(rows.len());
    for (expected, row) in (base_seq + 1..).zip(rows.iter()) {
        if row.seq > head.head_seq {
            return Err(StorageError::HeadConflict {
                doc_id: doc_id.to_owned(),
                expected: head.head_seq,
                actual: row.seq,
            });
        }
        if row.seq != expected {
            return Err(StorageError::Corrupt {
                context: format!(
                    "{doc_id} 的 loro_updates 出现断档：期望 seq {expected}，实际 {}",
                    row.seq
                ),
            });
        }
        updates.push(row.payload.clone());
    }

    // head 可能在读取期间被推进：复核，绝不返回半截 delta
    let after = store::load_head(vault, doc_id)?;
    if after.head_seq != head.head_seq {
        return Err(StorageError::HeadConflict {
            doc_id: doc_id.to_owned(),
            expected: head.head_seq,
            actual: after.head_seq,
        });
    }

    let (target_frontier, target_vv) = match rows.last() {
        Some(last) => {
            if last.seq != head.head_seq {
                return Err(StorageError::Corrupt {
                    context: format!(
                        "{doc_id} 最后一行 seq {} 不是 head {}",
                        last.seq, head.head_seq
                    ),
                });
            }
            (last.end_frontier.clone(), last.end_vv.clone())
        }
        None => (store::encoded_frontier(&head), head.head_vv.clone()),
    };

    // head 与最后一行必须一致（frontier/vv 任一漂移都说明数据不完整）
    let head_frontier = store::encoded_frontier(&head);
    if normalize_frontier(&target_frontier).map_err(|context| StorageError::Corrupt { context })?
        != normalize_frontier(&head_frontier)
            .map_err(|context| StorageError::Corrupt { context })?
    {
        return Err(StorageError::Corrupt {
            context: format!("{doc_id} head frontier 与最后一行 end_frontier 不一致"),
        });
    }
    if normalize_vv(&target_vv).map_err(|context| StorageError::Corrupt { context })?
        != normalize_vv(&head.head_vv).map_err(|context| StorageError::Corrupt { context })?
    {
        return Err(StorageError::Corrupt {
            context: format!("{doc_id} head vv 与最后一行 end_vv 不一致"),
        });
    }

    Ok(PreparedCommittedDelta {
        doc_id: doc_id.to_owned(),
        base_seq,
        target_head_seq: head.head_seq,
        target_frontier,
        target_vv,
        updates,
    })
}

impl DocumentSession {
    /// 是否处于外部提交 Held（Held 期间本地写入一律被拒）。
    #[must_use]
    pub fn is_external_commit_held(&self) -> bool {
        self.gate.is_held()
    }

    /// 当前外部提交租约（`None` = 不在 Held）。
    #[must_use]
    pub fn external_commit_lease(&self) -> Option<&ExternalCommitLease> {
        self.gate.held()
    }

    /// 登记一次外部提交（只建立票据，不阻塞、不 flush）。
    ///
    /// 重复 begin / 已有租约返回 [`GateError::AlreadyHeld`]；真正的等待全在
    /// [`Self::poll_external_commit`]。
    pub fn begin_external_commit(&mut self) -> Result<ExternalCommitTicket, GateError> {
        if let Some(lease) = self.gate.held.as_ref() {
            return Err(GateError::AlreadyHeld {
                generation: lease.generation,
            });
        }
        if let Some(state) = self.gate.ticket.as_ref() {
            return Err(GateError::AlreadyHeld {
                generation: state.ticket.generation,
            });
        }
        self.gate.next_generation += 1;
        let ticket = ExternalCommitTicket {
            doc_id: self.doc_id.clone(),
            session_epoch: self.session_epoch,
            generation: GateGeneration(self.gate.next_generation),
            target_live_rev: self.live_rev,
            baseline_head_seq: self.journal.durable_head_seq(),
        };
        self.gate.ticket = Some(TicketState {
            ticket: ticket.clone(),
            submitted: None,
        });
        Ok(ticket)
    }

    /// 非阻塞推进闸门：`WaitingIME` → `Flushing` → `Held(lease)`。
    ///
    /// 判据是 flusher 的 durable 水位与 `pending == 0`（同一临界区），不读事件队列，
    /// 因此调用方可以把 `events()` 用于别处；也不会用"队列为空"冒充 quiescence。
    /// flush 明确失败时返回 [`GateState::Failed`]，闸门**不会**静默释放。
    pub fn poll_external_commit(&mut self, ticket: &ExternalCommitTicket) -> GateState {
        // 1) 已 Held：只认同一代际/文档/会话
        if let Some(lease) = self.gate.held.clone() {
            if same_ticket(ticket, &lease.doc_id, lease.session_epoch, lease.generation) {
                return GateState::Held(lease);
            }
            return GateState::Stale;
        }

        // 2) 票据必须仍是当前登记的那张
        let Some(state) = self.gate.ticket.as_ref() else {
            return GateState::Stale;
        };
        if !same_ticket(
            ticket,
            &state.ticket.doc_id,
            state.ticket.session_epoch,
            state.ticket.generation,
        ) {
            return GateState::Stale;
        }
        let target = self.live_rev.max(state.ticket.target_live_rev);

        // 3) 组合中：等它自然结束（不强行结束 IME、也不触发 flush）
        if self.is_composing() {
            return GateState::WaitingIME;
        }

        // 4) 明确目标：durable 水位 + pending 归零（同一个快照，避免两次取锁看到不一致）
        let progress = self.journal.gate_progress();
        if progress.pending == 0 && progress.durable_through >= target {
            // 4a) 会话 durable 基准必须与库中 head 一致：有未 adopt 的外部行就 fail-closed
            let head = match store::load_head(&self.vault, &self.doc_id) {
                Ok(head) => head,
                Err(error) => return GateState::Failed(GateError::Storage(error.to_string())),
            };
            if head.head_seq != progress.durable_head_seq {
                return GateState::Failed(GateError::Unadopted {
                    durable_head_seq: progress.durable_head_seq,
                    database_head_seq: head.head_seq,
                });
            }
            // 4b) 晋升 Held（票据销毁，租约接管）
            let lease = ExternalCommitLease {
                doc_id: self.doc_id.clone(),
                session_epoch: self.session_epoch,
                generation: ticket.generation,
                target_live_rev: target,
                durable_head_seq: progress.durable_head_seq,
            };
            self.gate.ticket = None;
            self.gate.held = Some(lease.clone());
            return GateState::Held(lease);
        }

        if progress.pending > 0 {
            // 4c) 失败的 flush 不能静默吞掉：明确报错，等调用方 retry / cancel
            if let Some(message) = progress.last_write_error {
                return GateState::Failed(GateError::FlushFailed { target, message });
            }
            // 4d) 触发（或复用）一张覆盖明确目标的 flush 票据
            if let Some(state) = self.gate.ticket.as_mut() {
                if state.submitted != Some(target) {
                    self.journal.submit_ticket(target);
                    state.submitted = Some(target);
                }
            }
            return GateState::Flushing;
        }

        // 防御：pending == 0 但 durable 水位没到目标（每次 live 变更都会进 journal，
        // 正常路径不可达）；宁可明确失败，也不假装 Held。
        GateState::Failed(GateError::Storage(format!(
            "闸门等待目标 {target:?} 时出现水位不一致（durable_through={:?}, pending=0）",
            progress.durable_through
        )))
    }

    /// 结束外部提交并释放闸门（正常路径在 adopt 之后调用）。
    ///
    /// 校验：
    /// - 当前必须 Held 且 `lease` 逐字段匹配（doc/epoch/generation）；
    /// - `applied` 必须与库中**当前** head 完全一致（含 frontier/vv）；
    /// - 若 Held 期间又有已 durable 的行（失败路径），先交付进 live 再释放——取消/失败
    ///   同样不丢已 durable 的数据；交付的增量会作为 [`SessionEvent::Remote`] 入事件队列。
    ///
    /// 任何校验失败都**不释放**闸门（fail-closed，"失败不得静默释放"）；调用方可修好
    /// 现场后再调一次。
    pub fn end_external_commit(
        &mut self,
        lease: ExternalCommitLease,
        applied: AppliedHeads,
    ) -> Result<(), GateError> {
        let held = self.gate.held.clone().ok_or(GateError::NotHeld)?;
        if held.doc_id != lease.doc_id
            || held.session_epoch != lease.session_epoch
            || held.generation != lease.generation
        {
            return Err(GateError::LeaseMismatch {
                expected: held.generation,
                actual: lease.generation,
            });
        }
        if held.doc_id != self.doc_id || held.session_epoch != self.session_epoch {
            return Err(GateError::WrongDoc {
                expected: self.doc_id.clone(),
                actual: held.doc_id,
            });
        }

        // applied 必须就是库中当前 head：不猜、不替调用方决定
        let head = store::load_head(&self.vault, &self.doc_id)
            .map_err(|error| GateError::Storage(error.to_string()))?;
        let current = AppliedHeads::from_head(&head);
        if current.head_seq != applied.head_seq {
            return Err(GateError::HeadsMismatch {
                expected: current.head_seq,
                actual: applied.head_seq,
            });
        }
        if normalize_frontier(&current.frontier).map_err(GateError::Storage)?
            != normalize_frontier(&applied.frontier).map_err(GateError::Storage)?
        {
            return Err(GateError::TargetMismatch { field: "frontier" });
        }
        if normalize_vv(&current.vv).map_err(GateError::Storage)?
            != normalize_vv(&applied.vv).map_err(GateError::Storage)?
        {
            return Err(GateError::TargetMismatch { field: "vv" });
        }
        if applied.head_seq < held.durable_head_seq {
            return Err(GateError::HeadsMismatch {
                expected: held.durable_head_seq,
                actual: applied.head_seq,
            });
        }

        // 失败/取消路径：把 Held 期间变成 durable、但 live 还没导入的行补齐
        let base = self.journal.durable_head_seq();
        if applied.head_seq > base {
            let updates =
                durable_updates_between(&self.vault, &self.doc_id, base, applied.head_seq)?;
            let delivered = self.apply_durable_segment(
                base,
                applied.head_seq,
                &applied.frontier,
                &applied.vv,
                &updates,
            )?;
            if let Some(delta) = delivered {
                lock(self.journal.events_ref()).push_back(SessionEvent::Remote(delta));
            }
        }

        // 复核 live 已与 applied 一致，否则不得释放
        if !self.live_matches(&applied.frontier, &applied.vv)? {
            return Err(GateError::TargetMismatch {
                field: "live frontier/vv",
            });
        }
        self.gate.release();
        Ok(())
    }

    /// 放弃一次外部提交（失败/取消路径），**先把已 durable 的变化交付进 live**。
    ///
    /// 返回交付产生的 [`LiveDelta`]（没有可交付行时为 `None`），同一份 delta 也会进
    /// [`SessionEvent::Remote`] 事件队列。仍有未 ACK 的本地更新时返回
    /// [`GateError::Pending`] 并保持闸门不变：此时 journal 条目的 `end_vv` 早于外部 head，
    /// 重定基准会让水位与条目自相矛盾，必须先 retry flush 或重启会话。
    pub fn cancel_external_commit(
        &mut self,
        ticket: &ExternalCommitTicket,
    ) -> Result<Option<LiveDelta>, GateError> {
        let active = match (&self.gate.held, &self.gate.ticket) {
            (Some(lease), _) => {
                same_ticket(ticket, &lease.doc_id, lease.session_epoch, lease.generation)
            }
            (None, Some(state)) => same_ticket(
                ticket,
                &state.ticket.doc_id,
                state.ticket.session_epoch,
                state.ticket.generation,
            ),
            (None, None) => false,
        };
        if !active {
            return Err(GateError::StaleTicket {
                generation: ticket.generation,
            });
        }
        if ticket.doc_id != self.doc_id || ticket.session_epoch != self.session_epoch {
            return Err(GateError::WrongDoc {
                expected: self.doc_id.clone(),
                actual: ticket.doc_id.clone(),
            });
        }

        let pending = self.pending_len();
        if pending != 0 {
            return Err(GateError::Pending { pending });
        }
        let head = store::load_head(&self.vault, &self.doc_id)
            .map_err(|error| GateError::Storage(error.to_string()))?;
        let applied = AppliedHeads::from_head(&head);
        let base = self.journal.durable_head_seq();
        let mut delivered = None;
        if applied.head_seq > base {
            let updates =
                durable_updates_between(&self.vault, &self.doc_id, base, applied.head_seq)?;
            delivered = self.apply_durable_segment(
                base,
                applied.head_seq,
                &applied.frontier,
                &applied.vv,
                &updates,
            )?;
            if let Some(delta) = &delivered {
                lock(self.journal.events_ref()).push_back(SessionEvent::Remote(delta.clone()));
            }
        }
        self.gate.release();
        Ok(delivered)
    }

    /// 重投一次 flush（`GateState::Failed(FlushFailed)` 之后的显式重试）。
    ///
    /// 只对当前票据有效；已 Held 时是 no-op。注意 head CAS 冲突是确定性失败：同一基准
    /// 重试还会失败，必须 cancel/重启会话（这里不掩盖失败）。
    pub fn retry_external_commit(
        &mut self,
        ticket: &ExternalCommitTicket,
    ) -> Result<(), GateError> {
        if let Some(lease) = self.gate.held.as_ref() {
            if same_ticket(ticket, &lease.doc_id, lease.session_epoch, lease.generation) {
                return Ok(());
            }
            return Err(GateError::StaleTicket {
                generation: ticket.generation,
            });
        }
        let Some(state) = self.gate.ticket.as_ref() else {
            return Err(GateError::StaleTicket {
                generation: ticket.generation,
            });
        };
        if !same_ticket(
            ticket,
            &state.ticket.doc_id,
            state.ticket.session_epoch,
            state.ticket.generation,
        ) {
            return Err(GateError::StaleTicket {
                generation: ticket.generation,
            });
        }
        let target = self.live_rev.max(ticket.target_live_rev);
        self.journal.submit_ticket(target);
        if let Some(state) = self.gate.ticket.as_mut() {
            state.submitted = Some(target);
        }
        Ok(())
    }

    /// adopt 一份**已 durable** 的外部增量（必须 Held 且 lease 匹配）。
    ///
    /// 不写 journal、不产生 sync intent；成功后把 live 与 flusher 的 durable 基准一起推进，
    /// 失败时 live / durable 水位不变（预演校验在导入之前完成）。
    pub fn adopt_committed_delta(
        &mut self,
        lease: &ExternalCommitLease,
        delta: PreparedCommittedDelta,
    ) -> Result<AdoptOutcome, AdoptError> {
        let held = self.gate.held.clone().ok_or(AdoptError::NotHeld)?;
        if held.doc_id != lease.doc_id
            || held.session_epoch != lease.session_epoch
            || held.generation != lease.generation
        {
            return Err(AdoptError::LeaseMismatch {
                expected: held.generation,
                actual: lease.generation,
            });
        }
        if delta.doc_id != self.doc_id {
            return Err(AdoptError::WrongDoc {
                expected: self.doc_id.clone(),
                actual: delta.doc_id,
            });
        }
        let progress = self.journal.gate_progress();
        if progress.pending != 0 {
            return Err(AdoptError::Pending {
                pending: progress.pending,
            });
        }

        // 连续性：target 必须恰好被 updates 覆盖
        if delta.target_head_seq < delta.base_seq
            || delta.target_head_seq - delta.base_seq != delta.updates.len() as u64
        {
            return Err(AdoptError::NonContiguous {
                base_seq: delta.base_seq,
                target_head_seq: delta.target_head_seq,
                updates: delta.updates.len(),
            });
        }

        // 幂等：重复交付同一份 delta（其 target 已是当前 durable head 且 live 已就位）
        if delta.base_seq != progress.durable_head_seq {
            if delta.target_head_seq == progress.durable_head_seq
                && self.live_matches(&delta.target_frontier, &delta.target_vv)?
            {
                return Ok(self.idempotent_outcome(&delta));
            }
            return Err(AdoptError::StaleBase {
                base_seq: delta.base_seq,
                durable_head_seq: progress.durable_head_seq,
            });
        }

        // target 不能超过库中已有行（否则 rebase 会在 seq 上留洞）
        let head = store::load_head(&self.vault, &self.doc_id)
            .map_err(|error| AdoptError::Storage(error.to_string()))?;
        if head.head_seq < delta.target_head_seq {
            return Err(AdoptError::TargetAhead {
                target_head_seq: delta.target_head_seq,
                database_head_seq: head.head_seq,
            });
        }

        // 同 head：空 delta 幂等，不推进 rev；target 必须与 live 当前 heads 一致
        if delta.updates.is_empty() {
            if normalize_frontier(&self.doc.frontiers().encode()).map_err(AdoptError::Storage)?
                != normalize_frontier(&delta.target_frontier).map_err(AdoptError::Storage)?
            {
                return Err(AdoptError::TargetMismatch { field: "frontier" });
            }
            if normalize_vv(&self.doc.vv().encode()).map_err(AdoptError::Storage)?
                != normalize_vv(&delta.target_vv).map_err(AdoptError::Storage)?
            {
                return Err(AdoptError::TargetMismatch { field: "vv" });
            }
            return Ok(self.idempotent_outcome(&delta));
        }

        // 只导入"确实已 durable"的 payload：与库中行逐字节比对
        let durable = durable_updates_between(
            &self.vault,
            &self.doc_id,
            delta.base_seq,
            delta.target_head_seq,
        )?;
        if durable.len() != delta.updates.len() {
            return Err(AdoptError::NonContiguous {
                base_seq: delta.base_seq,
                target_head_seq: delta.target_head_seq,
                updates: durable.len(),
            });
        }
        for (index, (expected, actual)) in durable.iter().zip(delta.updates.iter()).enumerate() {
            if expected != actual {
                return Err(AdoptError::PayloadMismatch {
                    seq: delta.base_seq + 1 + index as u64,
                });
            }
        }

        let delivered = self.apply_durable_segment(
            delta.base_seq,
            delta.target_head_seq,
            &delta.target_frontier,
            &delta.target_vv,
            &delta.updates,
        )?;
        match delivered {
            Some(delta_live) => {
                let ack = DurableAck {
                    batch_id: BatchId(0),
                    through_live_rev: delta_live.to_rev,
                    head_seq: delta.target_head_seq,
                    head_frontier: delta.target_frontier.clone(),
                };
                Ok(AdoptOutcome {
                    delta: delta_live,
                    ack,
                })
            }
            // 全部 op 已存在（没有产生新 op）：按幂等处理，水位由 apply 内部 rebase 推进
            None => Ok(self.idempotent_outcome(&delta)),
        }
    }

    /// 幂等结果：不推进 `live_rev`、不产生 splice。
    fn idempotent_outcome(&self, delta: &PreparedCommittedDelta) -> AdoptOutcome {
        AdoptOutcome {
            delta: LiveDelta {
                doc_id: self.doc_id.clone(),
                from_rev: self.live_rev,
                to_rev: self.live_rev,
                live_frontier: self.live_frontier.clone(),
                splice: DeltaSplice {
                    at: UnicodeScalar::new(0),
                    delete_scalars: 0,
                    insert: String::new(),
                },
                cause: None,
                source: ChangeSource::Remote,
                merge: MergeInfo {
                    concurrent: false,
                    remote_changes: 0,
                },
            },
            ack: DurableAck {
                batch_id: BatchId(0),
                through_live_rev: self.live_rev,
                head_seq: delta.target_head_seq,
                head_frontier: delta.target_frontier.clone(),
            },
        }
    }

    /// live 的 frontier/vv 是否已经等于给定目标。
    fn live_matches(&self, frontier: &[u8], vv: &[u8]) -> Result<bool, AdoptError> {
        let live_frontier =
            normalize_frontier(&self.doc.frontiers().encode()).map_err(AdoptError::Storage)?;
        let expected_frontier = normalize_frontier(frontier).map_err(AdoptError::Storage)?;
        let live_vv = normalize_vv(&self.doc.vv().encode()).map_err(AdoptError::Storage)?;
        let expected_vv = normalize_vv(vv).map_err(AdoptError::Storage)?;
        Ok(live_frontier == expected_frontier && live_vv == expected_vv)
    }

    /// 交付一段已 durable 的外部增量：预演校验 → 导入 live → 重定 flusher 基准。
    ///
    /// 返回 `Some(delta)` 表示 live 真的产生了新 op；`None` 表示没有新 op（幂等）。
    fn apply_durable_segment(
        &mut self,
        base_seq: u64,
        target_head_seq: u64,
        target_frontier: &[u8],
        target_vv: &[u8],
        updates: &[Vec<u8>],
    ) -> Result<Option<LiveDelta>, AdoptError> {
        if updates.is_empty() {
            return Ok(None);
        }

        // 相位 1：scratch 预演（不碰 live、不改任何水位）
        self.validate_durable_segment(base_seq, updates, target_frontier, target_vv)?;

        // 相位 2：导入 + 重定基准（同一临界区；Held 期间 journal 不会再增长）
        let mut guard = lock(self.journal.state_ref());
        if guard.journal.pending_len() != 0 {
            let pending = guard.journal.pending_len();
            drop(guard);
            return Err(AdoptError::Pending { pending });
        }
        if guard.durable.head_seq != base_seq {
            let durable_head_seq = guard.durable.head_seq;
            drop(guard);
            return Err(AdoptError::StaleBase {
                base_seq,
                durable_head_seq,
            });
        }
        let diff = match import_updates(&self.doc, updates.iter().map(|update| update.as_slice())) {
            Ok(diff) => diff,
            Err(error) => {
                drop(guard);
                return Err(AdoptError::Import(error.to_string()));
            }
        };
        let from_rev = self.live_rev;
        let changed = diff.changed();
        let to_rev = if changed {
            LiveRev(from_rev.0 + 1)
        } else {
            from_rev
        };
        let frontier = self.doc.frontiers().encode();
        let vv = self.doc.vv();
        let heads = AppliedHeads {
            head_seq: target_head_seq,
            frontier: target_frontier.to_vec(),
            vv: target_vv.to_vec(),
        };
        let rebased = JournalHandle::rebase_locked(&mut guard, &heads, to_rev);
        drop(guard);
        if let Err(error) = rebased {
            // 上一步已在同一把锁内确认 pending == 0，正常路径不可达
            return Err(AdoptError::Storage(error.to_string()));
        }

        self.live_rev = to_rev;
        self.live_frontier = frontier.clone();
        self.journal_base_vv = vv;
        if !changed {
            return Ok(None);
        }
        Ok(Some(LiveDelta {
            doc_id: self.doc_id.clone(),
            from_rev,
            to_rev,
            live_frontier: frontier,
            splice: diff.splice(),
            cause: None,
            source: ChangeSource::Remote,
            merge: MergeInfo {
                concurrent: false,
                remote_changes: updates.len() as u32,
            },
        }))
    }

    /// 在不碰 live 的 scratch 副本上预演一次导入并校验目标 frontier/vv。
    ///
    /// 同时确认 live 与库中 `base_seq` 的状态一致——这是"导入必然成功、且不会只导入一半"
    /// 的前提：scratch 与 live 起点相同、payload 相同，因此 live 的导入结果与 scratch 一致。
    fn validate_durable_segment(
        &self,
        base_seq: u64,
        updates: &[Vec<u8>],
        target_frontier: &[u8],
        target_vv: &[u8],
    ) -> Result<(), AdoptError> {
        // base_seq == 0 表示库里还没有任何行：此时不能走 `load_body_doc(Some(0))`
        // （它会去读 seq 0 的 end_frontier），直接用一个空文档做 scratch。
        let scratch = if base_seq == 0 {
            BodyDoc::restore(self.doc_id.clone(), self.peer_id, None, &[])
                .map_err(|error| AdoptError::Storage(error.to_string()))?
        } else {
            store::load_body_doc(&self.vault, &self.doc_id, self.peer_id, Some(base_seq))
                .map_err(|error| AdoptError::Storage(error.to_string()))?
                .0
        };
        if scratch.vv() != self.doc.vv() {
            return Err(AdoptError::Inconsistent {
                context: "live VV 与库中 base_seq 不一致".to_owned(),
            });
        }
        if normalize_frontier(&scratch.frontiers().encode()).map_err(AdoptError::Storage)?
            != normalize_frontier(&self.live_frontier).map_err(AdoptError::Storage)?
        {
            return Err(AdoptError::Inconsistent {
                context: "live frontier 与库中 base_seq 不一致".to_owned(),
            });
        }

        import_updates(&scratch, updates.iter().map(|update| update.as_slice()))
            .map_err(|error| AdoptError::Import(error.to_string()))?;
        if normalize_frontier(&scratch.frontiers().encode()).map_err(AdoptError::Storage)?
            != normalize_frontier(target_frontier).map_err(AdoptError::Storage)?
        {
            return Err(AdoptError::TargetMismatch { field: "frontier" });
        }
        if normalize_vv(&scratch.vv().encode()).map_err(AdoptError::Storage)?
            != normalize_vv(target_vv).map_err(AdoptError::Storage)?
        {
            return Err(AdoptError::TargetMismatch { field: "vv" });
        }
        Ok(())
    }
}

`````



@@SECTION code/082 `crates/storage/src/session/core.rs`


`````rust
//! 会话核：`EditorDocumentPort` 的串行 reducer（ADR 0003 §5）。
//!
//! 一个 [`DocumentSession`] = 一个正文文档的**唯一 live owner**：
//!
//! - 持有 live [`BodyDoc`]（Loro）、generation / session_epoch / live_rev；
//! - 持有唯一未 ACK journal 与事件队列；
//! - 所有编辑走 [`DocumentSession::apply`]，受理顺序**固定**（见方法文档）；
//! - `flush` 只投递命令即返回票据，SQLite 由后台线程写（见 [`super::flush`]）。
//!
//! 端口协议类型在 [`super::protocol`]，本模块**不新增协议字段**（缺字段应先报告 Lead）。
//! 远端导入路径在 [`DocumentSession::apply_remote`]，撤销/重做在
//! [`DocumentSession::undo`] / [`DocumentSession::redo`]，都用同一个 Loro `UndoManager`。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use loro::{UndoManager, VersionVector};
use menghunote_text::UnicodeScalar;
use rusqlite::types::Value;

use super::flush::{lock, JournalHandle};
use super::gate::ExternalCommitGate;
use super::protocol::{
    Applied, ChangeSource, CommandId, DeltaSplice, DocId, DurableAck, EditOutcome, EditRequest,
    FlushReason, FlushTicket, Generation, HistoryState, LiveDelta, LiveRev, MergeInfo,
    OpenSnapshot, PortError, RejectReason, Rejected, ScalarEdit, SessionEpoch, SessionEvent,
    StaleBase,
};
use crate::body_doc::BodyDoc;
use crate::error::StorageError;
use crate::store;
use crate::Vault;

/// 会话配置。
///
/// `flush_delay` 是**测试/故障注入**用的可控慢盘延迟，生产用 `None`；
/// `sync` 是 ADR 0009 切片 2b 的本地意图上下文，`None` = 不产生 `sync_outbox`
/// 行（默认构建与既有调用点行为完全不变）。
#[derive(Debug, Clone, Copy)]
pub struct SessionOptions {
    /// 本机 peer id（每设备唯一）。
    pub peer_id: u64,
    /// 会话纪元：旧会话的迟到请求会被拒。
    pub session_epoch: SessionEpoch,
    /// 未 ACK journal 容量（条）。满了就是背压（拒绝新写，不丢已 ACC 部分）。
    pub journal_capacity: usize,
    /// 一次 A 事务最多合并多少条（慢盘时 pending 会堆到这里为止）。
    pub flush_batch: usize,
    /// 注入的 flush 延迟（模拟慢盘）。
    pub flush_delay: Option<Duration>,
    /// 本地写入的同步上下文；`None` = 不产生出站意图（默认）。
    pub sync: Option<crate::sync_bridge::SyncContext>,
}

impl Default for SessionOptions {
    fn default() -> Self {
        Self {
            peer_id: 1,
            session_epoch: SessionEpoch(1),
            journal_capacity: 64,
            flush_batch: 8,
            flush_delay: None,
            sync: None,
        }
    }
}

/// 一个正文文档的会话（串行 reducer）。
///
/// 字段对 `super::gate` 开放（`pub(crate)`）：外部提交闸门与 adopt 必须直接操作**同一份**
/// live 文档、journal 与水位，复制状态只会制造撕裂。对外部 crate 仍然是完全私有的。
pub struct DocumentSession {
    pub(crate) vault: Vault,
    pub(crate) doc_id: DocId,
    /// 本机 peer id（重放 scratch 副本时用；不影响 import）。
    pub(crate) peer_id: u64,
    generation: Generation,
    pub(crate) session_epoch: SessionEpoch,
    pub(crate) doc: BodyDoc,
    pub(crate) live_rev: LiveRev,
    pub(crate) live_frontier: Vec<u8>,
    /// 上一次写入 journal 的 VV：增量导出的起点（不在 durable 前推进）。
    pub(crate) journal_base_vv: VersionVector,
    pub(crate) journal: JournalHandle,
    undo: UndoManager,
    /// command_id 幂等缓存（只缓存终态：Applied；背压/过期不缓存）。
    command_cache: HashMap<CommandId, EditOutcome>,
    /// composition 深度（`> 0` 表示组合中；嵌套 begin 计数）。
    composition_depth: u32,
    /// composition 期间延后的远端 update（按到达顺序；结束后串行合并，不丢）。
    deferred_remote: Vec<Vec<u8>>,
    /// 外部提交闸门（ADR 0009 切片 3a）；无新线程/新全局状态。
    pub(crate) gate: ExternalCommitGate,
}

impl std::fmt::Debug for DocumentSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DocumentSession")
            .field("doc_id", &self.doc_id)
            .field("generation", &self.generation)
            .field("session_epoch", &self.session_epoch)
            .field("live_rev", &self.live_rev)
            .field("pending", &self.pending_len())
            .finish()
    }
}

impl DocumentSession {
    /// 打开会话：从库里的 snapshot + updates 重放出 live 文档。
    pub fn open(vault: &Vault, doc_id: &str, options: SessionOptions) -> Result<Self, PortError> {
        // 同步上下文在打开时校验一次：epoch/peer 非法要尽早报错，
        // 而不是让之后每一次 flush 都因非法 intent 失败、journal 永远重试。
        if let Some(sync) = options.sync {
            sync.validate().map_err(port)?;
        }
        let generation = load_generation(vault, doc_id)?;
        let (doc, head) =
            store::load_body_doc(vault, doc_id, options.peer_id, None).map_err(port)?;
        let live_frontier = doc.frontiers().encode();
        // 打开时没有未 ACK 条目：增量基准就是 durable 的 VV
        let journal_base_vv = head.version_vector().map_err(port)?;
        let journal = JournalHandle::spawn(
            vault.clone(),
            doc_id.to_owned(),
            head,
            options.journal_capacity,
            options.flush_batch,
            options.flush_delay,
            options.sync,
        );
        let undo = doc.undo_manager();
        Ok(Self {
            vault: vault.clone(),
            doc_id: doc_id.to_owned(),
            peer_id: options.peer_id,
            generation,
            session_epoch: options.session_epoch,
            doc,
            live_rev: LiveRev(0),
            live_frontier,
            journal_base_vv,
            journal,
            undo,
            command_cache: HashMap::new(),
            composition_depth: 0,
            deferred_remote: Vec::new(),
            gate: ExternalCommitGate::default(),
        })
    }

    /// 文档 ID。
    pub fn doc_id(&self) -> &str {
        &self.doc_id
    }

    /// 文档代际。
    pub fn generation(&self) -> Generation {
        self.generation
    }

    /// 会话纪元。
    pub fn session_epoch(&self) -> SessionEpoch {
        self.session_epoch
    }

    /// 当前 live 修订号（`EditRequest.base_live_rev` 的正确基准）。
    pub fn live_rev(&self) -> LiveRev {
        self.live_rev
    }

    /// 当前 live frontier（不透明字节）。
    pub fn live_frontier(&self) -> &[u8] {
        &self.live_frontier
    }

    /// 当前 live 正文（LF 归一后的 Markdown）。
    pub fn markdown(&self) -> String {
        self.doc.text()
    }

    /// 打开快照（调用方据此初始化自己的 Rope 镜像）。
    pub fn snapshot(&self) -> OpenSnapshot {
        OpenSnapshot {
            doc_id: self.doc_id.clone(),
            generation: self.generation,
            session_epoch: self.session_epoch,
            live_rev: self.live_rev,
            live_frontier: self.live_frontier.clone(),
            live_vv: self.doc.vv().encode(),
            // 批次号不跨会话持久化：重启后只能确认 durable 水位（head_seq）
            durable_batch: None,
            durable_head_seq: self.journal.durable_head_seq(),
            markdown: self.doc.text(),
        }
    }

    /// 未 ACK 条数。
    pub fn pending_len(&self) -> usize {
        self.journal.pending_len()
    }

    /// 未 ACK update 的总字节数（调度器用：达到 64 KiB 立即 flush）。
    pub fn pending_bytes(&self) -> usize {
        self.journal.pending_bytes()
    }

    /// 已 durable 的 head seq。
    pub fn durable_head_seq(&self) -> u64 {
        self.journal.durable_head_seq()
    }

    /// 已 durable 到哪个 live 修订号。
    pub fn durable_through(&self) -> LiveRev {
        self.journal.durable_through()
    }

    /// 取走事件队列（Durable / Remote）。
    pub fn events(&self) -> Vec<SessionEvent> {
        self.journal.drain_events()
    }

    /// 等待某个 live 修订号变成 durable（测试/关闭前等待用）。
    ///
    /// 只消费覆盖到 `target` 的 `Durable`，其它事件留在队列里。
    pub fn wait_for_durable(&self, target: LiveRev, timeout: Duration) -> Option<DurableAck> {
        let deadline = Instant::now() + timeout;
        loop {
            let mut found = None;
            {
                let mut queue = lock(self.journal.events_ref());
                let mut remaining = std::collections::VecDeque::new();
                while let Some(event) = queue.pop_front() {
                    match event {
                        SessionEvent::Durable(ack)
                            if ack.through_live_rev >= target && found.is_none() =>
                        {
                            found = Some(ack);
                        }
                        other => remaining.push_back(other),
                    }
                }
                *queue = remaining;
            }
            if let Some(ack) = found {
                return Some(ack);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// 受理一次编辑请求（顺序固定，任何一步失败都**不改 Loro**）。
    ///
    /// 顺序：
    ///
    /// 1. 校验 `doc_id` / `generation` / `session_epoch`；
    /// 2. 按 `command_id` 幂等（终态直接返回首次结果）；
    /// 3. 外部提交闸门 Held → `Rejected{NotReady}`（ADR 0009：Held 期间禁写，fail-closed）；
    /// 4. composition 进行中 → `Rejected{CompositionActive}`；
    /// 5. 校验 `edits`：升序、互不重叠、`end <= scalar_len`；`base_live_rev` 不匹配 → `Stale`；
    /// 6. journal 容量准入（满了 → `Rejected{Backpressure}`，此时 Loro 未被触碰）；
    /// 7. 才允许改 Loro：按**逆序** `delete + insert`，导出增量、进 journal、推进 live_rev。
    pub fn apply(&mut self, request: EditRequest) -> EditOutcome {
        // 1. 身份与代际
        if request.doc_id != self.doc_id {
            return rejected(request.command_id, RejectReason::UnknownDoc);
        }
        if request.generation != self.generation {
            return rejected(request.command_id, RejectReason::WrongGeneration);
        }
        if request.session_epoch != self.session_epoch {
            return rejected(request.command_id, RejectReason::WrongEpoch);
        }

        // 2. 幂等：只命中终态（背压会随 flush 解除，不能缓存）
        if let Some(cached) = self.command_cache.get(&request.command_id) {
            return cached.clone();
        }

        // 2.5 外部提交闸门 Held：一律拒绝新的本地正文改动（fail-closed，不碰 Loro）。
        //     3b 负责让界面在 Held 期间禁写；存储层这里必须自己兜底。
        if self.gate.is_held() {
            return rejected(request.command_id, RejectReason::NotReady);
        }

        // 3. composition 闸门：组合期间拒绝其它本地正文改动（**不碰 Loro**）
        if self.is_composing() {
            return rejected(request.command_id, RejectReason::CompositionActive);
        }

        // 4. 区间校验 + 基准校验（都在改 Loro 之前）
        let scalar_len = self.doc.scalar_len();
        if validate_edits(&request.edits, scalar_len).is_err() {
            return rejected(request.command_id, RejectReason::InvalidRange);
        }
        if request.base_live_rev != self.live_rev {
            return EditOutcome::Stale(StaleBase {
                command_id: request.command_id,
                current_live_rev: self.live_rev,
                compensation: None,
            });
        }

        // 5+6. 容量准入与 Loro 修改在同一把锁内完成：
        //      持锁期间 flush 线程只会**释放**容量，因此准入通过后 append 不可能失败。
        let mut guard = lock(self.journal.state_ref());
        if guard.journal.is_full() {
            drop(guard);
            return rejected(request.command_id, RejectReason::Backpressure);
        }

        if request.edits.is_empty() {
            let outcome = EditOutcome::Applied(Applied {
                command_id: request.command_id.clone(),
                live_rev: self.live_rev,
                live_frontier: self.live_frontier.clone(),
                splice: DeltaSplice {
                    at: UnicodeScalar::new(0),
                    delete_scalars: 0,
                    insert: String::new(),
                },
                local_echo: true,
                source: ChangeSource::Local,
                merge: MergeInfo::default(),
            });
            self.command_cache
                .insert(request.command_id.clone(), outcome.clone());
            return outcome;
        }

        // 逆序应用：后面的编辑先做，前面编辑的坐标才不会失效（协议 §「按逆序应用」）
        self.doc.set_next_commit_origin(&request.origin);
        if self.undo.group_start().is_err() {
            drop(guard);
            return rejected(request.command_id, RejectReason::NotReady);
        }
        for edit in request.edits.iter().rev() {
            let start = edit.start.get();
            let len = edit.end.get() - start;
            if let Err(_error) = self.doc.replace_scalars(start, len, &edit.replacement) {
                // 校验已保证范围内合法；真出错时丢弃分组，不推进任何水位
                self.undo.group_end();
                drop(guard);
                return rejected(request.command_id, RejectReason::InvalidRange);
            }
        }
        self.undo.group_end();

        // 没有产生任何 op（例如空替换）→ 不回显、不进 journal、不推进 live_rev
        let vv_after = self.doc.vv();
        if vv_after == self.journal_base_vv {
            let first = request.edits.first().map_or(0, |edit| edit.start.get());
            let outcome = EditOutcome::Applied(Applied {
                command_id: request.command_id.clone(),
                live_rev: self.live_rev,
                live_frontier: self.live_frontier.clone(),
                splice: DeltaSplice {
                    at: UnicodeScalar::new(first),
                    delete_scalars: 0,
                    insert: String::new(),
                },
                local_echo: true,
                source: ChangeSource::Local,
                merge: MergeInfo::default(),
            });
            self.command_cache
                .insert(request.command_id.clone(), outcome.clone());
            return outcome;
        }

        let update = match self.doc.pending_update(&self.journal_base_vv) {
            Ok(update) => update,
            Err(_) => {
                drop(guard);
                return rejected(request.command_id, RejectReason::NotReady);
            }
        };

        let rev = LiveRev(self.live_rev.0 + 1);
        // 持锁且已准入：这里不可能失败（见上面的注释），失败只可能是内部不一致
        if JournalHandle::append(
            &mut guard,
            &self.doc_id,
            "session",
            crate::journal::JournalOrigin::Local,
            update,
            rev,
        )
        .is_err()
        {
            debug_assert!(false, "持锁且已准入时 journal 追加失败");
            drop(guard);
            return rejected(request.command_id, RejectReason::NotReady);
        }
        drop(guard);

        self.journal_base_vv = vv_after;
        self.live_rev = rev;
        self.live_frontier = self.doc.frontiers().encode();

        let first = request.edits.first();
        let splice = DeltaSplice {
            at: first.map_or(UnicodeScalar::new(0), |edit| edit.start),
            delete_scalars: first.map_or(0, |edit| (edit.end.get() - edit.start.get()) as u64),
            insert: first
                .map(|edit| edit.replacement.clone())
                .unwrap_or_default(),
        };
        let outcome = EditOutcome::Applied(Applied {
            command_id: request.command_id.clone(),
            live_rev: self.live_rev,
            live_frontier: self.live_frontier.clone(),
            splice,
            // 调用方已经把自己的镜像写好了：不得再按 splice 二次应用
            local_echo: true,
            source: ChangeSource::Local,
            merge: MergeInfo::default(),
        });
        self.command_cache
            .insert(request.command_id.clone(), outcome.clone());
        outcome
    }

    /// 开始一次 IME composition（可嵌套，深度计数）。
    ///
    /// preedit **不经过本层**：会话只记录「组合是否在进行」，不接收 preedit 文本，
    /// 也不因 preedit 更新而阻塞。返回当前嵌套深度。
    ///
    /// 外部提交闸门 Held 期间**不进入组合**（fail-closed）并返回当前深度；需要显式
    /// 错误请用 [`DocumentSession::try_begin_composition`]（3b 用）。
    pub fn begin_composition(&mut self) -> u32 {
        match self.try_begin_composition() {
            Ok(depth) => depth,
            Err(_) => self.composition_depth,
        }
    }

    /// 开始一次 IME composition 的**可报错**版本：Held 期间返回 [`PortError`]。
    ///
    /// Held 期间通道已冻结，这里再开组合只会让 preedit 与提交竞争，因此直接拒绝。
    pub fn try_begin_composition(&mut self) -> Result<u32, PortError> {
        if self.gate.is_held() {
            return Err(PortError::Storage(
                "外部提交闸门 Held：拒绝开始 IME 组合".to_owned(),
            ));
        }
        self.composition_depth += 1;
        Ok(self.composition_depth)
    }

    /// 结束一次 composition；只有**最外层**结束才合并延后的远端导入。
    ///
    /// 重复 `end`（没有对应 `begin`）是 no-op：平台可能重复回调 cancel，
    /// 这里不 panic、不改变状态。
    pub fn end_composition(&mut self) -> Vec<LiveDelta> {
        if self.composition_depth > 0 {
            self.composition_depth -= 1;
        }
        if self.is_composing() {
            return Vec::new();
        }
        self.merge_deferred_remote()
    }

    /// 按到达顺序串行合并延后的远端 update。
    ///
    /// - 背压（journal 满）时**保留剩余项**，等 `flush` 之后调用方再调一次；
    /// - 解码/写入失败时也保留队首，不丢远端数据。
    pub fn merge_deferred_remote(&mut self) -> Vec<LiveDelta> {
        let mut merged = Vec::new();
        loop {
            if self.deferred_remote.is_empty() || !self.can_accept_remote() {
                break;
            }
            let Some(update) = self.deferred_remote.first().cloned() else {
                break;
            };
            match self.merge_remote_inner(&update, false) {
                Ok(outcome) => {
                    self.deferred_remote.remove(0);
                    if let Some(delta) = outcome {
                        merged.push(delta);
                    }
                }
                // 保留在队首：不丢远端数据，调用方可稍后重试
                Err(_) => break,
            }
        }
        merged
    }

    /// 是否正处于 composition 中。
    pub fn is_composing(&self) -> bool {
        self.composition_depth > 0
    }

    /// 当前嵌套深度（0 = 不在组合中）。
    pub fn composition_depth(&self) -> u32 {
        self.composition_depth
    }

    /// 仍未合并的远端 update 条数。
    pub fn deferred_remote_len(&self) -> usize {
        self.deferred_remote.len()
    }

    fn can_accept_remote(&self) -> bool {
        // Held 期间不合并延后的远端：闸门要求 live 水位静止，等 end 之后再合并
        !self.journal.is_full() && !self.gate.is_held()
    }

    /// 远端导入：把 update 合进 live 文档，返回驱动镜像的 [`LiveDelta`]。
    ///
    /// 容量不足时拒绝（返回 `PortError::Storage`），保证「背压时 Loro 未被修改」对远端路径同样成立。
    /// Held 期间一律拒绝（fail-closed）：调用方应在 `end_external_commit` 之后重试。
    pub fn apply_remote(&mut self, update: &[u8]) -> Result<Option<LiveDelta>, PortError> {
        if self.gate.is_held() {
            return Err(PortError::Storage(
                "外部提交闸门 Held：暂不接受远端导入".to_owned(),
            ));
        }
        if self.is_composing() {
            // 组合期间延后：进队列（不丢、不报错），结束后按到达顺序串行合并
            self.deferred_remote.push(update.to_vec());
            return Ok(None);
        }
        self.merge_remote_now(update)
    }

    /// 立即合并一条远端 update（**不做** composition 判断；闸门在 [`Self::apply_remote`]）。
    fn merge_remote_now(&mut self, update: &[u8]) -> Result<Option<LiveDelta>, PortError> {
        self.merge_remote_inner(update, true)
    }

    /// `publish = false` 时只返回 delta、不重复推进事件队列（延后合并的唯一投递口是
    /// [`Self::end_composition`] 的返回值，避免调用方把同一条变更应用到镜像两次）。
    fn merge_remote_inner(
        &mut self,
        update: &[u8],
        publish: bool,
    ) -> Result<Option<LiveDelta>, PortError> {
        if self.journal.is_full() {
            return Err(PortError::Storage(
                "未 ACK journal 已满，暂不接受远端导入（背压）".to_owned(),
            ));
        }

        let mut guard = lock(self.journal.state_ref());
        let diff = match import_updates(&self.doc, std::iter::once(update)) {
            Ok(diff) => diff,
            Err(error) => {
                drop(guard);
                return Err(port(error));
            }
        };
        if !diff.changed() {
            drop(guard);
            return Ok(None);
        }

        let splice = diff.splice();
        let after_vv = diff.after_vv;
        let from_rev = self.live_rev;
        let to_rev = LiveRev(self.live_rev.0 + 1);

        // 远端 op 也进 journal（A 事务落库后才算 durable）；容量已准入。
        let pending = match self.doc.pending_update(&self.journal_base_vv) {
            Ok(update) => update,
            Err(error) => {
                drop(guard);
                return Err(port(error));
            }
        };
        if JournalHandle::append(
            &mut guard,
            &self.doc_id,
            "remote",
            // 远端导入**绝不**产生出站意图（ADR 0009）；否则会把刚收到的数据推回 relay。
            crate::journal::JournalOrigin::Remote,
            pending,
            to_rev,
        )
        .is_err()
        {
            drop(guard);
            return Err(PortError::Storage("journal 追加失败".to_owned()));
        }
        drop(guard);

        self.journal_base_vv = after_vv;
        self.live_rev = to_rev;
        self.live_frontier = self.doc.frontiers().encode();

        let delta = LiveDelta {
            doc_id: self.doc_id.clone(),
            from_rev,
            to_rev,
            live_frontier: self.live_frontier.clone(),
            splice,
            cause: None,
            source: ChangeSource::Remote,
            merge: MergeInfo {
                concurrent: true,
                remote_changes: 1,
            },
        };
        if publish {
            lock(self.journal.events_ref()).push_back(SessionEvent::Remote(delta.clone()));
        }
        Ok(Some(delta))
    }

    /// 撤销：用**唯一** `UndoManager` 生成本地新 op（不回退 VV，也不回退远端字符）。
    ///
    /// 返回：
    /// - `Ok(Some(Applied))`：已撤销并进 journal；
    /// - `Ok(None)`：没有可撤销项（no-op undo，应提示调用方消费该项）；
    /// - `Ok(Some(Rejected{Backpressure}))`：journal 满，**未 undo**（Loro 未被修改）；
    /// - `Ok(Some(Rejected{NotReady}))`：外部提交闸门 Held，**未 undo**（fail-closed）。
    pub fn undo(&mut self) -> Result<Option<EditOutcome>, PortError> {
        // Held 期间 undo 也会改 Loro/journal：fail-closed，等 end 之后重试
        if self.gate.is_held() {
            return Ok(Some(rejected(
                CommandId::from("undo"),
                RejectReason::NotReady,
            )));
        }
        // composition 期间 undo/redo 也属于「其它本地正文改动」：拒绝且不碰 Loro
        if self.is_composing() {
            return Ok(Some(rejected(
                CommandId::from("undo"),
                RejectReason::CompositionActive,
            )));
        }
        // 容量准入在改 Loro 之前；准入通过后 flusher 只会释放容量，append 必然成功
        if self.journal.is_full() {
            return Ok(Some(rejected(
                CommandId::from("undo"),
                RejectReason::Backpressure,
            )));
        }
        let before = self.doc.text();
        let before_vv = self.doc.vv();
        if self.undo.group_start().is_err() {
            return Err(PortError::Storage("UndoManager 分组失败".to_owned()));
        }
        let changed = match self.undo.undo() {
            Ok(changed) => changed,
            Err(error) => {
                self.undo.group_end();
                return Err(port_loro(error));
            }
        };
        self.undo.group_end();
        if !changed || self.doc.vv() == before_vv {
            return Ok(None);
        }
        Ok(Some(EditOutcome::Applied(
            self.finish_history_change(&before, ChangeSource::Undo)?,
        )))
    }

    /// 重做：与 [`DocumentSession::undo`] 对称。
    pub fn redo(&mut self) -> Result<Option<EditOutcome>, PortError> {
        if self.gate.is_held() {
            return Ok(Some(rejected(
                CommandId::from("redo"),
                RejectReason::NotReady,
            )));
        }
        if self.is_composing() {
            return Ok(Some(rejected(
                CommandId::from("redo"),
                RejectReason::CompositionActive,
            )));
        }
        if self.journal.is_full() {
            return Ok(Some(rejected(
                CommandId::from("redo"),
                RejectReason::Backpressure,
            )));
        }
        let before = self.doc.text();
        let before_vv = self.doc.vv();
        let changed = match self.undo.redo() {
            Ok(changed) => changed,
            Err(error) => return Err(port_loro(error)),
        };
        if !changed || self.doc.vv() == before_vv {
            return Ok(None);
        }
        Ok(Some(EditOutcome::Applied(
            self.finish_history_change(&before, ChangeSource::Redo)?,
        )))
    }

    /// 撤销/重做共同的收尾：导出增量、进 journal、推进 live_rev、产出 Applied。
    fn finish_history_change(
        &mut self,
        before: &str,
        source: ChangeSource,
    ) -> Result<Applied, PortError> {
        let after_vv = self.doc.vv();
        let update = self
            .doc
            .pending_update(&self.journal_base_vv)
            .map_err(port)?;
        let rev = LiveRev(self.live_rev.0 + 1);
        let label = match source {
            ChangeSource::Undo => "undo",
            ChangeSource::Redo => "redo",
            _ => "history",
        };
        {
            let mut guard = lock(self.journal.state_ref());
            // 外层已做容量准入，且持锁期间容量只增不减 → 这里不会失败
            // undo/redo 是本机产生的新 op（不是远端回放），因此同样产生出站意图。
            if JournalHandle::append(
                &mut guard,
                &self.doc_id,
                label,
                crate::journal::JournalOrigin::Local,
                update,
                rev,
            )
            .is_err()
            {
                debug_assert!(false, "持锁且已准入时 journal 追加失败");
                return Err(PortError::Storage("journal 追加失败".to_owned()));
            }
        }
        self.journal_base_vv = after_vv;
        self.live_rev = rev;
        self.live_frontier = self.doc.frontiers().encode();

        let splice = diff_splice(before, &self.doc.text());
        Ok(Applied {
            command_id: format!("{label}-{}", rev.0),
            live_rev: rev,
            live_frontier: self.live_frontier.clone(),
            splice,
            // 撤销不是用户"已经写好"的输入：调用方要按 splice 更新镜像
            local_echo: false,
            source,
            merge: MergeInfo::default(),
        })
    }

    /// 撤销/重做栈状态。
    pub fn history(&self) -> HistoryState {
        HistoryState {
            can_undo: self.undo.can_undo(),
            undo_count: self.undo.undo_count(),
            can_redo: self.undo.can_redo(),
            redo_count: self.undo.redo_count(),
        }
    }

    /// 受理一次 flush：同步返回票据，后台线程写 A 事务并推 `DurableAck`。
    pub fn flush(&mut self, _reason: FlushReason) -> FlushTicket {
        let ticket_id = self.journal.submit_ticket(self.live_rev);
        FlushTicket {
            ticket_id,
            target_live_rev: self.live_rev,
        }
    }

    /// 已 durable 的 head seq（方便测试/状态栏）。
    pub fn vault(&self) -> &Vault {
        &self.vault
    }
}

fn rejected(command_id: CommandId, reason: RejectReason) -> EditOutcome {
    EditOutcome::Rejected(Rejected { command_id, reason })
}

fn port(error: StorageError) -> PortError {
    PortError::Storage(error.to_string())
}

/// Loro 错误同样归到端口错误（协议不泄露 Loro 类型）。
fn port_loro(error: loro::LoroError) -> PortError {
    PortError::Storage(error.to_string())
}

/// 区间校验：升序、互不重叠、`end <= scalar_len`。
fn validate_edits(edits: &[ScalarEdit], scalar_len: usize) -> Result<(), ()> {
    let mut previous_end = 0usize;
    for (index, edit) in edits.iter().enumerate() {
        let start = edit.start.get();
        let end = edit.end.get();
        if start > end || end > scalar_len {
            return Err(());
        }
        if index > 0 && start < previous_end {
            return Err(());
        }
        previous_end = end;
    }
    Ok(())
}

/// 读文档代际（`loro_docs.generation`），并确认它是正文文档。
fn load_generation(vault: &Vault, doc_id: &str) -> Result<Generation, PortError> {
    let rows = vault
        .query(
            "SELECT generation, kind FROM loro_docs WHERE doc_id = ?1",
            &[Value::Text(doc_id.to_owned())],
        )
        .map_err(port)?;
    let row = rows
        .into_iter()
        .next()
        .ok_or_else(|| PortError::NotOpen(doc_id.to_owned()))?;

    let generation = match row.first() {
        Some(Value::Integer(value)) if *value >= 0 => Generation(*value as u64),
        _ => return Err(PortError::NotOpen(doc_id.to_owned())),
    };
    match row.get(1) {
        Some(Value::Text(kind)) if kind == "body" => Ok(generation),
        _ => Err(PortError::NotOpen(doc_id.to_owned())),
    }
}

/// 一批 update 导入后的文本/版本变化（**不推进任何水位、不碰 journal**）。
pub(crate) struct ImportDiff {
    /// 导入前的正文。
    pub(crate) before_text: String,
    /// 导入后的正文（没有新 op 时与 `before_text` 相同）。
    pub(crate) after_text: String,
    /// 导入后的版本向量。
    pub(crate) after_vv: VersionVector,
    /// 是否真的产生了新的 op（以 VV 为准：文本相同也可能有纯格式 op）。
    changed: bool,
}

impl ImportDiff {
    /// 是否产生了新的 op。
    pub(crate) fn changed(&self) -> bool {
        self.changed
    }

    /// 本次变化的最小 splice（驱动调用方镜像与选区变换）。
    pub(crate) fn splice(&self) -> DeltaSplice {
        diff_splice(&self.before_text, &self.after_text)
    }
}

/// 把一批 update 合进 live 文档，返回前后文本与 VV。
///
/// **只做 Loro 导入**：journal、水位、事件全部由调用方决定——
/// `apply_remote` 走 journal（会产生 A 事务条目），`adopt_committed_delta` 走已 durable
/// 路径（不进 journal、不产生 sync intent）。两条路径共用同一份导入/选区变换逻辑。
pub(crate) fn import_updates<'a, I>(doc: &BodyDoc, updates: I) -> Result<ImportDiff, StorageError>
where
    I: IntoIterator<Item = &'a [u8]>,
{
    let before_text = doc.text();
    let before_vv = doc.vv();
    for update in updates {
        doc.import(update)?;
    }
    let after_vv = doc.vv();
    let changed = after_vv != before_vv;
    let after_text = if changed {
        doc.text()
    } else {
        before_text.clone()
    };
    Ok(ImportDiff {
        before_text,
        after_text,
        after_vv,
        changed,
    })
}

/// 以 scalar 为单位求「最小差异」splice（公共前缀 + 公共后缀）。
pub(crate) fn diff_splice(before: &str, after: &str) -> DeltaSplice {
    let before_chars: Vec<char> = before.chars().collect();
    let after_chars: Vec<char> = after.chars().collect();

    let mut prefix = 0usize;
    while prefix < before_chars.len()
        && prefix < after_chars.len()
        && before_chars[prefix] == after_chars[prefix]
    {
        prefix += 1;
    }

    let mut suffix = 0usize;
    while suffix < before_chars.len() - prefix
        && suffix < after_chars.len() - prefix
        && before_chars[before_chars.len() - 1 - suffix]
            == after_chars[after_chars.len() - 1 - suffix]
    {
        suffix += 1;
    }

    let delete_scalars = before_chars.len() - prefix - suffix;
    let insert: String = after_chars[prefix..after_chars.len() - suffix]
        .iter()
        .collect();
    DeltaSplice {
        at: UnicodeScalar::new(prefix),
        delete_scalars: delete_scalars as u64,
        insert,
    }
}

`````



@@SECTION code/083 `crates/storage/src/session/schedule.rs`


`````rust
//! 存储侧调度（ADR 0003 切片 4a）：自动 flush + B 物化后台驱动。
//!
//! ## 自动 flush（ADR 0002 §9）
//!
//! - **固定窗口**：从「首次 dirty」起算 250 ms，**后续输入不重置**（不是尾缘 debounce），
//!   否则连续输入会无限延期；
//! - **64 KiB**：未 ACK 的 update 字节数达到阈值立即 flush；
//! - `Ctrl+S` / 切换 / 关闭仍可显式 [`SessionDriver::flush`]；[`SessionDriver::close`] 有界排空。
//!
//! 时钟通过 [`Clock`] 注入：生产用 [`SystemClock`]（墙钟），测试用 [`ManualClock`]
//! 直接 [`ManualClock::set`] 推进——不靠真睡 250 ms 碰运气。
//!
//! ## B 物化
//!
//! 只有收到 [`SessionEvent::Durable`] 之后才把 B 任务入队；后台线程按 ACK 里的
//! `head_seq` 从**固定 frontier**调用 [`materialize_body`]（内部 `end_frontier_at` + CAS），
//! 因此：
//!
//! - 不读取、不修改正在编辑的 live doc（worker 自己从库里重放）；
//! - 旧任务迟到时 CAS 命中 0 行 → `Stale`，不会覆盖新结果；
//! - 失败会记录在 [`DriverEvent::MaterializeFailed`]，可用 [`SessionDriver::retry_failed`] 重试。
//!
//! 调用方线程只做内存操作：`apply` / `flush` / `pump` 都不等待 SQLite；只有 `close`
//! 与显式 `materialize_now` 会有界等待。
//!
//! ## 切片 3b：外部提交闸门 / adopt 的驱动转发
//!
//! [`SessionDriver`] 不重新实现闸门语义，只把 [`DocumentSession`] 的 3a API 原样转发，
//! 并把 adopt 的结果变成**驱动事件**：
//!
//! - `adopt` 成功后把 `AdoptOutcome.delta` 作为 [`DriverEvent::Remote`]、把 `ack` 作为
//!   [`DriverEvent::Durable`] 推入事件队列（顺序固定：先 Remote 再 Durable），
//!   这样 UI 的 pump 就能在不重开文档的情况下更新镜像与 durable 水位；
//! - **绝不**调用会 journal 的 `apply_remote`：那份 update 已经 durable，
//!   再走一次会让 flusher 重复写库并产生多余的出站意图；
//! - Held 期间 `apply` 的拒绝（[`super::protocol::RejectReason::NotReady`]）原样透传，
//!   驱动不额外吞错、也不替调用方兜底。

use std::collections::VecDeque;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use rusqlite::types::Value;

use super::core::{DocumentSession, SessionOptions};
use super::gate::{
    AdoptError, AdoptOutcome, AppliedHeads, ExternalCommitLease, ExternalCommitTicket, GateError,
    GateState, PreparedCommittedDelta,
};
use super::protocol::{
    DurableAck, EditOutcome, EditRequest, FlushReason, FlushTicket, HistoryState, LiveDelta,
    LiveRev, PortError, SessionEvent,
};
use crate::error::StorageError;
use crate::materialize::{materialize_body, MaterializeOutcome};
use crate::Vault;

/// 取锁；毒化也继续用内部值。
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 可注入时钟：返回「驱动启动以来」的时长。
pub trait Clock: Send + 'static {
    /// 当前虚拟/墙钟时刻。
    fn now(&self) -> Duration;
}

/// 墙钟（生产用）。
#[derive(Debug)]
pub struct SystemClock {
    started: Instant,
}

impl SystemClock {
    /// 从现在开始计时。
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Duration {
        self.started.elapsed()
    }
}

/// 手动时钟（测试用）：显式 `set`/`advance`，不依赖真实时间。
#[derive(Debug, Clone)]
pub struct ManualClock {
    now: Arc<Mutex<Duration>>,
}

impl ManualClock {
    /// 从 0 开始。
    pub fn new() -> Self {
        Self {
            now: Arc::new(Mutex::new(Duration::ZERO)),
        }
    }

    /// 设置到某个时刻。
    pub fn set(&self, at: Duration) {
        *lock(&self.now) = at;
    }

    /// 前进一段时间。
    pub fn advance(&self, by: Duration) {
        let mut now = lock(&self.now);
        *now += by;
    }

    /// 读取当前时刻。
    pub fn get(&self) -> Duration {
        *lock(&self.now)
    }
}

impl Default for ManualClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Duration {
        self.get()
    }
}

/// 自动 flush 策略。
#[derive(Debug, Clone, Copy)]
pub struct FlushPolicy {
    /// 首次 dirty 起算的固定窗口（默认 250 ms）。
    pub interval: Duration,
    /// 未 ACK update 字节阈值（默认 64 KiB）。
    pub size_bytes: usize,
}

impl Default for FlushPolicy {
    fn default() -> Self {
        Self {
            interval: Duration::from_millis(250),
            size_bytes: 64 * 1024,
        }
    }
}

/// 驱动配置。
#[derive(Debug, Clone, Copy)]
pub struct DriverOptions {
    /// 会话配置。
    pub session: SessionOptions,
    /// flush 策略。
    pub policy: FlushPolicy,
    /// 是否启动墙钟 ticker（生产 `true`；测试用假时钟时可关）。
    pub auto_tick: bool,
    /// ticker 间隔（默认 = `policy.interval`）。
    pub tick_interval: Option<Duration>,
}

impl Default for DriverOptions {
    fn default() -> Self {
        Self {
            session: SessionOptions::default(),
            policy: FlushPolicy::default(),
            auto_tick: true,
            tick_interval: None,
        }
    }
}

/// 驱动向调用方推送的事件（不含请求的同步回复）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriverEvent {
    /// 已提交一张 flush 票据（后台还没写完）。
    FlushScheduled {
        /// 票据。
        ticket: FlushTicket,
        /// 触发原因。
        reason: FlushReason,
    },
    /// durable 水位推进（同时会触发 B 物化入队）。
    Durable(DurableAck),
    /// 远端变更。
    Remote(LiveDelta),
    /// B 物化完成。
    Materialized(MaterializeOutcome),
    /// B 物化失败（已记录，可 `retry_failed` 重试）。
    MaterializeFailed {
        /// 笔记 ID。
        note_id: String,
        /// 失败信息。
        message: String,
    },
}

/// `close` 的排空报告。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseReport {
    /// 覆盖到最后一个 live_rev 的 ACK（超时为 `None`）。
    pub durable: Option<DurableAck>,
    /// 本次关闭过程中完成的 B 物化数。
    pub materialized: usize,
    /// 仍未 ACK 的编辑条数（正常情况下为 0）。
    pub pending_edits: usize,
    /// 仍未完成的 B 任务数（正常情况下为 0）。
    pub pending_materialize: usize,
    /// 是否超时（`true` 表示没排空，调用方应保留现场）。
    pub timed_out: bool,
}

/// 一条 B 物化任务（只带固定 seq，不带 live 句柄）。
#[derive(Debug, Clone, PartialEq, Eq)]
struct MaterializeJob {
    note_id: String,
    doc_id: String,
    target_seq: u64,
}

/// B 物化后台线程。
struct Materializer {
    sender: Option<Sender<MaterializeJob>>,
    results: Arc<Mutex<VecDeque<MaterializeOutcome>>>,
    errors: Arc<Mutex<VecDeque<(MaterializeJob, String)>>>,
    pending: Arc<Mutex<usize>>,
    worker: Option<JoinHandle<()>>,
}

impl Materializer {
    fn spawn(vault: Vault, peer_id: u64, delay: Option<Duration>) -> Self {
        let (sender, receiver) = channel::<MaterializeJob>();
        let results: Arc<Mutex<VecDeque<MaterializeOutcome>>> =
            Arc::new(Mutex::new(VecDeque::new()));
        let errors: Arc<Mutex<VecDeque<(MaterializeJob, String)>>> =
            Arc::new(Mutex::new(VecDeque::new()));
        let pending = Arc::new(Mutex::new(0usize));

        let worker_results = Arc::clone(&results);
        let worker_errors = Arc::clone(&errors);
        let worker_pending = Arc::clone(&pending);
        let worker = thread::Builder::new()
            .name("menghunote-materialize".to_owned())
            .spawn(move || {
                while let Ok(job) = receiver.recv() {
                    if let Some(delay) = delay {
                        thread::sleep(delay);
                    }
                    // 运行期再解析 note_id：笔记行可能还没投影出来（失败要能记录并重试）
                    let outcome = run_materialize(&vault, &job, peer_id);
                    match outcome {
                        Ok(outcome) => lock(&worker_results).push_back(outcome),
                        Err(error) => {
                            let message = error.to_string();
                            lock(&worker_errors).push_back((job, message));
                        }
                    }
                    let mut pending = lock(&worker_pending);
                    *pending = pending.saturating_sub(1);
                }
            })
            .ok();

        Self {
            sender: Some(sender),
            results,
            errors,
            pending,
            worker,
        }
    }

    fn enqueue(&self, job: MaterializeJob) {
        if let Some(sender) = self.sender.as_ref() {
            *lock(&self.pending) += 1;
            if sender.send(job).is_err() {
                let mut pending = lock(&self.pending);
                *pending = pending.saturating_sub(1);
            }
        }
    }

    fn pending(&self) -> usize {
        *lock(&self.pending)
    }

    fn take_results(&self) -> Vec<MaterializeOutcome> {
        lock(&self.results).drain(..).collect()
    }

    fn take_errors(&self) -> Vec<(MaterializeJob, String)> {
        lock(&self.errors).drain(..).collect()
    }
}

impl Drop for Materializer {
    fn drop(&mut self) {
        drop(self.sender.take());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// 墙钟 ticker：按固定间隔投递 `()`，驱动 `pump` 检查固定窗口。
struct Ticker {
    stop: Option<Sender<()>>,
    worker: Option<JoinHandle<()>>,
}

impl Ticker {
    fn spawn(interval: Duration) -> (Self, Receiver<()>) {
        let interval = interval.max(Duration::from_millis(1));
        let (tick_sender, tick_receiver) = channel::<()>();
        let (stop_sender, stop_receiver) = channel::<()>();

        let worker = thread::Builder::new()
            .name("menghunote-tick".to_owned())
            .spawn(move || loop {
                match stop_receiver.recv_timeout(interval) {
                    Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        if tick_sender.send(()).is_err() {
                            break;
                        }
                    }
                }
            })
            .ok();

        (
            Self {
                stop: Some(stop_sender),
                worker,
            },
            tick_receiver,
        )
    }
}

impl Drop for Ticker {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// 会话驱动：会话核 + 自动 flush 调度 + B 物化后台线程。
pub struct SessionDriver {
    session: DocumentSession,
    vault: Vault,
    peer_id: u64,
    policy: FlushPolicy,
    clock: Box<dyn Clock>,
    /// 首次 dirty 的虚拟时刻；`None` = 没有等待窗口的脏数据。
    dirty_since: Option<Duration>,
    /// 已提交 flush 覆盖到的 live_rev（避免空 flush 造成重复票据）。
    flushed_live_rev: LiveRev,
    materializer: Materializer,
    /// B 失败任务（可 `retry_failed`）。
    failed_jobs: Vec<MaterializeJob>,
    /// 只负责保活：Drop 时停线程；逻辑上不读取。
    _ticker: Option<Ticker>,
    tick_receiver: Option<Receiver<()>>,
    events: VecDeque<DriverEvent>,
}

impl std::fmt::Debug for SessionDriver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionDriver")
            .field("doc_id", &self.session.doc_id())
            .field("live_rev", &self.session.live_rev())
            .field("dirty_since", &self.dirty_since)
            .field("pending_bytes", &self.session.pending_bytes())
            .finish()
    }
}

impl SessionDriver {
    /// 打开驱动（墙钟 + ticker）。
    pub fn open(vault: &Vault, doc_id: &str, options: DriverOptions) -> Result<Self, PortError> {
        Self::open_with_clock(vault, doc_id, options, Box::new(SystemClock::new()))
    }

    /// 打开驱动并注入时钟（测试用假时钟）。
    pub fn open_with_clock(
        vault: &Vault,
        doc_id: &str,
        options: DriverOptions,
        clock: Box<dyn Clock>,
    ) -> Result<Self, PortError> {
        Self::open_with_clock_and_materialize_delay(vault, doc_id, options, clock, None)
    }

    /// 打开驱动并注入时钟 + **B 物化延迟**（测试/故障注入，与 `SessionOptions::flush_delay` 同性质）。
    ///
    /// 延迟是构造参数而不是 `DriverOptions` 字段：后者是公开结构体，
    /// 加字段会让外部（editor 的 e2e 夹具）的结构体字面量编译失败。
    pub fn open_with_clock_and_materialize_delay(
        vault: &Vault,
        doc_id: &str,
        options: DriverOptions,
        clock: Box<dyn Clock>,
        materialize_delay: Option<Duration>,
    ) -> Result<Self, PortError> {
        let session = DocumentSession::open(vault, doc_id, options.session)?;
        let materializer =
            Materializer::spawn(vault.clone(), options.session.peer_id, materialize_delay);
        let (ticker, tick_receiver) = if options.auto_tick {
            let interval = options.tick_interval.unwrap_or(options.policy.interval);
            let (ticker, receiver) = Ticker::spawn(interval);
            (Some(ticker), Some(receiver))
        } else {
            (None, None)
        };
        Ok(Self {
            session,
            vault: vault.clone(),
            peer_id: options.session.peer_id,
            policy: options.policy,
            clock,
            dirty_since: None,
            flushed_live_rev: LiveRev(0),
            materializer,
            failed_jobs: Vec::new(),
            _ticker: ticker,
            tick_receiver,
            events: VecDeque::new(),
        })
    }

    /// 会话核（只读能力：快照/历史/水位）。
    pub fn session(&self) -> &DocumentSession {
        &self.session
    }

    /// 当前 live 正文。
    pub fn markdown(&self) -> String {
        self.session.markdown()
    }

    /// 当前 live 修订号。
    pub fn live_rev(&self) -> LiveRev {
        self.session.live_rev()
    }

    /// 未 ACK update 字节数。
    pub fn pending_bytes(&self) -> usize {
        self.session.pending_bytes()
    }

    /// 未 ACK 编辑条数。
    pub fn pending_edits(&self) -> usize {
        self.session.pending_len()
    }

    /// 下一次固定窗口到期时刻（`None` 表示当前没有等待中的脏数据）。
    pub fn next_deadline(&self) -> Option<Duration> {
        self.dirty_since.map(|since| since + self.policy.interval)
    }

    /// 开始组合（透传会话闸门）。
    pub fn begin_composition(&mut self) -> u32 {
        self.session.begin_composition()
    }

    /// 结束组合：合并延后的远端导入；这些 op 已进 journal，需要计入 dirty。
    pub fn end_composition(&mut self) -> Vec<LiveDelta> {
        let deltas = self.session.end_composition();
        if !deltas.is_empty() {
            self.mark_dirty();
        }
        deltas
    }

    /// 背压导致延后队列没排空时，flush 之后再调一次。
    pub fn merge_deferred_remote(&mut self) -> Vec<LiveDelta> {
        let deltas = self.session.merge_deferred_remote();
        if !deltas.is_empty() {
            self.mark_dirty();
        }
        deltas
    }

    /// 是否正处于 composition。
    pub fn is_composing(&self) -> bool {
        self.session.is_composing()
    }

    /// 仍未合并的远端 update 条数。
    pub fn deferred_remote_len(&self) -> usize {
        self.session.deferred_remote_len()
    }

    /// 受理编辑；首次 dirty 记录窗口起点，达到 64 KiB 立即 flush。
    pub fn apply(&mut self, request: EditRequest) -> EditOutcome {
        let outcome = self.session.apply(request);
        if matches!(outcome, EditOutcome::Applied(_)) {
            self.mark_dirty();
        }
        outcome
    }

    /// 远端导入：同样计入 dirty（远端 op 也要落 durable）。
    ///
    /// **已 durable** 的外部增量不要走这里（见 [`Self::adopt_committed_delta`]）：
    /// 这条路径会 journal 并产生出站意图。
    pub fn apply_remote(&mut self, update: &[u8]) -> Result<Option<LiveDelta>, PortError> {
        let delta = self.session.apply_remote(update)?;
        if delta.is_some() {
            self.mark_dirty();
        }
        Ok(delta)
    }

    // ---- 外部提交闸门 / adopt（ADR 0009 切片 3b：只转发 3a 的存储层语义）----

    /// Held 期间本地写入是否被冻结（= 外部提交闸门持有中）。
    #[must_use]
    pub fn is_external_commit_held(&self) -> bool {
        self.session.is_external_commit_held()
    }

    /// 当前外部提交租约（`None` = 不在 Held）。
    #[must_use]
    pub fn external_commit_lease(&self) -> Option<&ExternalCommitLease> {
        self.session.external_commit_lease()
    }

    /// 登记一次外部提交（只建立票据；重复 begin 由会话拒绝）。
    pub fn begin_external_commit(&mut self) -> Result<ExternalCommitTicket, GateError> {
        self.session.begin_external_commit()
    }

    /// 非阻塞推进闸门：`WaitingIME` → `Flushing` → `Held(lease)`。
    ///
    /// 进入 Held 时顺手把调度器自己的水位对齐：闸门的成立条件就是「目标已 durable 且
    /// journal 空闲」，因此这里不再欠这次 flush（否则下一次 pump 会为已经不脏的数据
    /// 再投一张空票据）。
    pub fn poll_external_commit(&mut self, ticket: &ExternalCommitTicket) -> GateState {
        let state = self.session.poll_external_commit(ticket);
        if let GateState::Held(lease) = &state {
            self.dirty_since = None;
            self.flushed_live_rev = self.flushed_live_rev.max(lease.target_live_rev());
        }
        state
    }

    /// adopt 一份**已 durable** 的外部增量（必须 Held 且租约匹配）。
    ///
    /// 成功后把增量与 ACK 变成驱动事件（**先 Remote 再 Durable**），`pump` / `tick`
    /// 立刻能取到；失败时 live 与 durable 水位都不变（3a 保证）。
    pub fn adopt_committed_delta(
        &mut self,
        lease: &ExternalCommitLease,
        prepared: PreparedCommittedDelta,
    ) -> Result<AdoptOutcome, AdoptError> {
        let outcome = self.session.adopt_committed_delta(lease, prepared)?;
        // 不调用 `apply_remote`：这份 CRDT 已经 durable，再 journal 一次会重复写库 + 产生意图。
        self.events
            .push_back(DriverEvent::Remote(outcome.delta.clone()));
        self.push_durable(outcome.ack.clone());
        // adopt 要求 journal 空闲，因此"欠一次 flush"的水位到此为止。
        self.dirty_since = None;
        self.flushed_live_rev = self.flushed_live_rev.max(outcome.ack.through_live_rev);
        Ok(outcome)
    }

    /// 结束外部提交并释放闸门（`applied` 必须就是库中当前 head）。
    ///
    /// 失败/取消路径下 3a 会把 Held 期间已 durable 的变化交付进 live 并作为
    /// `SessionEvent::Remote` 入队，驱动会在下一次 `pump` 转发成 [`DriverEvent::Remote`]。
    pub fn end_external_commit(
        &mut self,
        lease: ExternalCommitLease,
        applied: AppliedHeads,
    ) -> Result<(), GateError> {
        self.session.end_external_commit(lease, applied)
    }

    /// 放弃一次外部提交（失败/取消），同样先交付已 durable 的变化。
    ///
    /// 返回的错误原样来自会话（例如仍 `pending` 时 [`GateError::Pending`]）；交付的增量
    /// 会作为会话事件由下一次 `pump` 转发，这里不重复推送，避免同一份 delta 应用两次。
    pub fn cancel_external_commit(
        &mut self,
        ticket: &ExternalCommitTicket,
    ) -> Result<(), GateError> {
        self.session.cancel_external_commit(ticket).map(|_| ())
    }

    /// **固定窗口**：只在首次 dirty 时记录起点，后续输入不重置。
    fn mark_dirty(&mut self) {
        if self.dirty_since.is_none() {
            self.dirty_since = Some(self.clock.now());
        }
        if self.session.pending_bytes() >= self.policy.size_bytes
            && self.session.live_rev() > self.flushed_live_rev
        {
            self.flush(FlushReason::Size);
        }
    }

    /// 撤销（转发会话核的唯一 `UndoManager`）。
    ///
    /// dirty 规则与 [`SessionDriver::apply`] 对齐：
    /// - `Ok(Some(Applied))` → `mark_dirty()`（撤销也是正文改动，必须落 durable）；
    /// - `Ok(None)`（没有可撤销项）/ `Ok(Some(Rejected))` / `Stale`（不会出现）→ **不** dirty；
    /// - `Err(PortError)` → 原样透传，不 dirty。
    pub fn undo(&mut self) -> Result<Option<EditOutcome>, PortError> {
        let outcome = self.session.undo()?;
        if matches!(&outcome, Some(EditOutcome::Applied(_))) {
            self.mark_dirty();
        }
        Ok(outcome)
    }

    /// 重做；dirty 规则同 [`SessionDriver::undo`]。
    pub fn redo(&mut self) -> Result<Option<EditOutcome>, PortError> {
        let outcome = self.session.redo()?;
        if matches!(&outcome, Some(EditOutcome::Applied(_))) {
            self.mark_dirty();
        }
        Ok(outcome)
    }

    /// 撤销/重做栈状态（真实计数来自 Loro `UndoManager`）。
    pub fn history(&self) -> HistoryState {
        self.session.history()
    }

    /// 手动/按原因 flush（同步返回票据，不等待 SQLite）。
    pub fn flush(&mut self, reason: FlushReason) -> FlushTicket {
        let ticket = self.session.flush(reason);
        self.flushed_live_rev = ticket.target_live_rev;
        self.dirty_since = None;
        self.events
            .push_back(DriverEvent::FlushScheduled { ticket, reason });
        ticket
    }

    /// 推进到指定虚拟时刻（测试用于精确控制固定窗口）。
    pub fn tick(&mut self, now: Duration) -> Vec<DriverEvent> {
        self.advance_to(now);
        self.collect_session_events();
        self.collect_materialize_results();
        self.drain_events()
    }

    /// 非阻塞泵：读 ticker、检查固定窗口、收 session/B 事件。
    pub fn pump(&mut self) -> Vec<DriverEvent> {
        self.take_ticks();
        let now = self.clock.now();
        self.advance_to(now);
        self.collect_session_events();
        self.collect_materialize_results();
        self.drain_events()
    }

    fn take_ticks(&mut self) {
        let Some(receiver) = self.tick_receiver.as_ref() else {
            return;
        };
        // 只做「消化信号」：固定窗口是否到期由虚拟/墙钟时刻决定
        while receiver.try_recv().is_ok() {}
    }

    fn advance_to(&mut self, now: Duration) {
        if let Some(since) = self.dirty_since {
            if now >= since + self.policy.interval {
                self.flush(FlushReason::Interval);
            }
        }
    }

    fn collect_session_events(&mut self) {
        for event in self.session.events() {
            match event {
                SessionEvent::Durable(ack) => self.push_durable(ack),
                SessionEvent::Remote(delta) => self.events.push_back(DriverEvent::Remote(delta)),
            }
        }
    }

    /// 推入一条 `Durable` 事件，并按既有约定入队 B 物化。
    ///
    /// 两条来源共用：flush 线程的 ACK，以及 adopt 之后由外部提交推进的 head（3b）。
    fn push_durable(&mut self, ack: DurableAck) {
        // DurableAck 之后才入队 B（用 ACK 的固定 head_seq，不碰 live doc）。
        self.enqueue_materialize(&ack);
        self.events.push_back(DriverEvent::Durable(ack));
    }

    /// DurableAck 之后才入队 B（用 ACK 的固定 head_seq，不碰 live doc）。
    fn enqueue_materialize(&mut self, ack: &DurableAck) {
        if ack.head_seq == 0 {
            return;
        }
        let doc_id = self.session.doc_id().to_owned();
        // note_id 只是给事件用的提示；权威解析在 worker 运行期做
        let note_hint = lookup_note_id(&self.vault, &doc_id)
            .ok()
            .flatten()
            .unwrap_or_default();
        self.materializer.enqueue(MaterializeJob {
            note_id: note_hint,
            doc_id,
            target_seq: ack.head_seq,
        });
    }

    fn collect_materialize_results(&mut self) {
        for outcome in self.materializer.take_results() {
            self.events.push_back(DriverEvent::Materialized(outcome));
        }
        for (job, message) in self.materializer.take_errors() {
            self.events.push_back(DriverEvent::MaterializeFailed {
                note_id: job.note_id.clone(),
                message,
            });
            self.failed_jobs.push(job);
        }
    }

    fn drain_events(&mut self) -> Vec<DriverEvent> {
        self.events.drain(..).collect()
    }

    /// 有界排空：flush(Close) → 等 durable → 等 B 完成。
    ///
    /// 调用方可以是 UI 线程（关闭窗口），但必须有界：`timeout` 到了就返回并置 `timed_out`。
    pub fn close(&mut self, timeout: Duration) -> CloseReport {
        let target = self.session.live_rev();
        let needs_flush = self.session.pending_len() > 0 || target > self.flushed_live_rev;
        if needs_flush {
            self.flush(FlushReason::Close);
        }

        let deadline = Instant::now() + timeout;
        let mut durable: Option<DurableAck> = None;
        let mut materialized = 0usize;

        loop {
            for event in self.pump() {
                match event {
                    DriverEvent::Durable(ack) if ack.through_live_rev >= target => {
                        durable = Some(ack);
                    }
                    DriverEvent::Materialized(_) => materialized += 1,
                    _ => {}
                }
            }

            let durable_ok = durable.is_some() || !needs_flush;
            let drained = durable_ok && self.materializer.pending() == 0;
            if drained || Instant::now() >= deadline {
                // 收尾：把最后一批 B 结果也计入
                for event in self.pump() {
                    if matches!(event, DriverEvent::Materialized(_)) {
                        materialized += 1;
                    }
                }
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }

        CloseReport {
            durable,
            materialized,
            pending_edits: self.session.pending_len(),
            pending_materialize: self.materializer.pending(),
            timed_out: self.session.pending_len() > 0 || self.materializer.pending() > 0,
        }
    }

    /// 显式同步物化（启动修复/测试用）：从固定 `target_seq` 走 CAS。
    pub fn materialize_now(
        &mut self,
        note_id: &str,
        doc_id: &str,
        target_seq: u64,
    ) -> Result<MaterializeOutcome, StorageError> {
        materialize_body(&self.vault, note_id, doc_id, target_seq, self.peer_id)
    }

    /// 重新入队失败的 B 任务；返回重试条数。
    pub fn retry_failed(&mut self) -> usize {
        let jobs = std::mem::take(&mut self.failed_jobs);
        let count = jobs.len();
        for job in jobs {
            self.materializer.enqueue(job);
        }
        count
    }

    /// 已 durable 的 head seq。
    pub fn durable_head_seq(&self) -> u64 {
        self.session.durable_head_seq()
    }

    /// 等待 B 队列排空（测试/关闭辅助），返回是否排空。
    ///
    /// **契约**：返回 `true` 时，worker 已完成的 B 结果都已经被搬进事件队列并取走，
    /// 之后的 `pump()` 不会再冒出「上一次等待之前就该完成」的 `Materialized` 事件。
    /// （worker 是「先 push 结果、再减 pending」，所以 pending 归零后再泵一次才能收干净。）
    pub fn wait_materialize(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            // 搬走已完成的结果/错误（本方法是等待辅助，事件直接丢弃）
            for _ in self.pump() {}
            if self.materializer.pending() == 0 {
                // 收尾：把「pending 已减、结果刚入队」的那一批也搬走
                for _ in self.pump() {}
                return self.materializer.pending() == 0;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(2));
        }
    }
}

/// worker 运行期：解析 note_id 后从固定 seq 物化。
fn run_materialize(
    vault: &Vault,
    job: &MaterializeJob,
    peer_id: u64,
) -> Result<MaterializeOutcome, StorageError> {
    let note_id = lookup_note_id(vault, &job.doc_id)?.ok_or_else(|| StorageError::Corrupt {
        context: format!("找不到 body_doc_id={} 的笔记行，暂时无法物化", job.doc_id),
    })?;
    materialize_body(vault, &note_id, &job.doc_id, job.target_seq, peer_id)
}

/// `body_doc_id` → `note_id`（B 物化需要笔记行）。
fn lookup_note_id(vault: &Vault, doc_id: &str) -> Result<Option<String>, StorageError> {
    let rows = vault.query(
        "SELECT note_id FROM notes WHERE body_doc_id = ?1 ORDER BY note_id LIMIT 1",
        &[Value::Text(doc_id.to_owned())],
    )?;
    match rows.first().and_then(|row| row.first()) {
        Some(Value::Text(note_id)) => Ok(Some(note_id.clone())),
        Some(Value::Null) | None => Ok(None),
        other => Err(StorageError::Corrupt {
            context: format!("notes.note_id 读回异常：{other:?}"),
        }),
    }
}

`````



@@SECTION code/084 `crates/storage/src/session/mod.rs`


`````rust
//! 文档会话层（ADR 0003 §5）：`EditorDocumentPort` 的串行 reducer 与协议类型。
//!
//! 归属：**`storage`**。它持有 live `LoroDoc`、唯一未 ACK journal、A/B 两阶段状态；
//! `editor` 通过本模块的**协议类型**与具体会话交互，但不得拿到可绕过会话的 live 句柄。
//!
//! 协议（[`protocol`]）由 Lead 冻结，**只读不改**；实现见 [`core`]（串行 reducer）
//! 与 [`flush`]（后台 A 事务写线程）。ADR 0009 切片 3a 的外部提交闸门与 adopt 在
//! [`gate`]（只做存储层，app/Driver 接线留 3b）。

mod core;
mod flush;
mod gate;
pub mod protocol;
mod schedule;

pub use core::{DocumentSession, SessionOptions};
pub use gate::{
    prepare_committed_delta, AdoptError, AdoptOutcome, AppliedHeads, ExternalCommitLease,
    ExternalCommitTicket, GateError, GateGeneration, GateState, PreparedCommittedDelta,
};
pub use schedule::{
    Clock, CloseReport, DriverEvent, DriverOptions, FlushPolicy, ManualClock, SessionDriver,
    SystemClock,
};

pub use protocol::{
    Applied, BatchId, ChangeSource, CommandId, DeltaSplice, DocId, DurableAck, EditOutcome,
    EditRequest, FlushReason, FlushTicket, FlushTicketId, Generation, HistoryState, LiveDelta,
    LiveRev, MergeInfo, OpenSnapshot, PortError, RejectReason, Rejected, ScalarEdit, SessionEpoch,
    SessionEvent, StaleBase,
};

`````


