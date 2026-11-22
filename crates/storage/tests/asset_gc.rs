//! ADR 0019 §4 / S3：资产 GC dry-run 扫描的集成测试。
//!
//! 纪律：临时 vault 用 `CARGO_TARGET_TMPDIR`（见 `support`）；扫描只读，测试比较
//! 调用前后的 `assets` 行（含 `orphan_since`）。

mod support;

use support::{commit_now, fresh_vault, seed_note, PEER_A};

use menghunote_storage::{
    asset_content_hash, enqueue_sync_intent, insert_asset, link_asset_ref, local_sync_intent,
    scan_asset_gc, set_asset_orphan_since, SyncContext, Value, Vault,
};

const CTX: SyncContext = SyncContext {
    peer_id: PEER_A,
    key_epoch: 1,
};

fn insert(vault: &Vault, data: &[u8], media_type: &str) -> [u8; 32] {
    let hash = asset_content_hash(data);
    insert_asset(
        vault,
        &hash,
        u64::try_from(data.len()).expect("测试字节长度"),
        Some(media_type),
        data,
    )
    .expect("写资产");
    hash
}

fn insert_placeholder(vault: &Vault, hash: &[u8; 32]) {
    vault
        .exec(
            "INSERT INTO assets(asset_hash,byte_len,media_type,data,orphan_since) \
             VALUES(?1,NULL,'image/png',NULL,NULL)",
            &[Value::Blob(hash.to_vec())],
        )
        .expect("写 data IS NULL 占位");
}

fn insert_version(vault: &Vault, note_id: &str, hashes: &[[u8; 32]]) {
    let mut blob = Vec::with_capacity(hashes.len() * 32);
    for hash in hashes {
        blob.extend_from_slice(hash);
    }
    vault
        .exec(
            "INSERT INTO versions(\
               note_id,captured_at,generation,frontier,title,body_zstd,asset_hashes\
             ) VALUES(?1,0,1,X'00','版本',X'00',?2)",
            &[Value::Text(note_id.to_owned()), Value::Blob(blob)],
        )
        .expect("写 versions 行");
}

fn insert_baseline_asset(vault: &Vault, hash: &[u8; 32]) {
    vault
        .exec(
            "INSERT INTO baseline_apply_assets(object_id,asset_hash,byte_len,applied,updated_at) \
             VALUES(?1,?2,1,1,0)",
            &[Value::Blob(vec![0xB5; 32]), Value::Blob(hash.to_vec())],
        )
        .expect("写 baseline_apply_assets 行");
}

fn enqueue_outbox(vault: &Vault, doc_id: &str, seq: u64, object_id: &[u8; 32]) {
    let intent = local_sync_intent(doc_id, seq, "gc-test-batch", CTX).expect("派生同步意图");
    enqueue_sync_intent(
        vault,
        &intent.message_id,
        &intent.batch_id,
        doc_id,
        seq,
        intent.key_epoch,
        object_id,
    )
    .expect("写 sync_outbox 行");
}

fn asset_rows(vault: &Vault) -> Vec<menghunote_storage::Row> {
    vault
        .query(
            "SELECT asset_hash,byte_len,media_type,data,orphan_since \
             FROM assets ORDER BY asset_hash ASC",
            &[],
        )
        .expect("读 assets 行")
}

/// 全部已知保护根都能被识别；只有真正孤立、有字节的资产成为候选。
#[test]
fn all_known_roots_protect_and_true_orphan_is_candidate() {
    let (_dir, vault) = fresh_vault();
    let doc = seed_note(&vault, "note-1", "doc-1");

    let referenced = insert(&vault, b"referenced", "image/png");
    link_asset_ref(&vault, "note-1", &referenced, 0, 1).expect("建立正文引用");

    let versioned = insert(&vault, b"versioned", "image/png");
    insert_version(&vault, "note-1", &[versioned]);

    let synced = insert(&vault, b"synced", "image/png");
    // 让 outbox 的关联 update payload 里出现 `asset://<synced>`：模拟"同步还要传"。
    doc.insert_scalars(0, &format!("![图片](asset://{})", hex_lower(&synced)))
        .expect("写正文");
    let head = commit_now(&vault, &doc, "body-batch");
    enqueue_outbox(&vault, doc.doc_id(), head.head_seq, &[0xEE; 32]);

    let no_bytes = [0x44; 32];
    insert_placeholder(&vault, &no_bytes);

    let baseline = insert(&vault, b"baseline", "text/plain");
    insert_baseline_asset(&vault, &baseline);

    let orphan = insert(&vault, b"orphan", "application/octet-stream");

    let report = scan_asset_gc(&vault, 10).expect("扫描");

    assert_eq!(report.total_assets, 6);
    assert_eq!(report.assets_with_data, 5);
    assert_eq!(report.assets_without_data, 1);
    assert_eq!(report.protected_by_refs, 1);
    assert_eq!(report.protected_by_versions, 1);
    assert_eq!(report.protected_by_sync, 1);
    assert_eq!(report.protected_by_baseline, 1);
    assert_eq!(report.protected_by_no_bytes, 1);
    assert_eq!(report.protected_total, 5, "五个已知根各保护一个资产");
    assert_eq!(report.unknown_protected, 0);
    assert_eq!(report.skipped_unknown, 0);
    assert!(!report.fail_closed);
    assert_eq!(report.parse_errors, 0);

    assert_eq!(report.candidate_count, 1);
    assert_eq!(report.candidate_bytes, 6, "b\"orphan\" 的长度");
    assert_eq!(report.candidates.len(), 1);
    assert_eq!(report.candidates[0].hash, orphan);
    assert_eq!(report.candidates[0].byte_len, 6);
    assert_eq!(
        report.candidates[0].media_type.as_deref(),
        Some("application/octet-stream")
    );
    assert_eq!(report.candidates_truncated, 0);
    assert_eq!(
        report.total_assets,
        report.protected_total + report.candidate_count + report.skipped_unknown,
        "报告计数必须能对账"
    );
}

/// 仅被历史版本引用（没有当前正文引用、没有 outbox）也必须被保护。
#[test]
fn version_only_reference_is_protected() {
    let (_dir, vault) = fresh_vault();
    seed_note(&vault, "note-1", "doc-1");

    let versioned = insert(&vault, b"only-version", "image/webp");
    let versioned_second = insert(&vault, b"only-version-2", "image/gif");
    insert_version(&vault, "note-1", &[versioned, versioned_second]);
    let orphan = insert(&vault, b"orphan", "image/png");

    let report = scan_asset_gc(&vault, 10).expect("扫描");
    assert_eq!(report.total_assets, 3);
    assert_eq!(report.version_rows, 1);
    assert_eq!(report.version_hashes, 2, "多个 32B hash 直接拼接");
    assert_eq!(report.protected_by_refs, 0);
    assert_eq!(report.protected_by_versions, 2);
    assert_eq!(report.candidate_count, 1);
    assert_eq!(report.candidates[0].hash, orphan);
    assert!(!report.fail_closed);
}

/// `sync_outbox.object_id` 精确等于资产 hash 时保守保护该资产。
///
/// 当前 schema 的 outbox 都是文档 update 意图；这是"不猜 ASSET HMAC 编码"下的
/// 保守兜底，不能反推真实 ASSET 对象名。
#[test]
fn sync_outbox_object_id_match_is_protected() {
    let (_dir, vault) = fresh_vault();
    let doc = seed_note(&vault, "note-1", "doc-1");
    doc.insert_scalars(0, "普通正文，没有 asset 链接")
        .expect("写正文");
    let head = commit_now(&vault, &doc, "body-batch");

    let synced = insert(&vault, b"outbox-object-id", "image/png");
    enqueue_outbox(&vault, doc.doc_id(), head.head_seq, &synced);
    let orphan = insert(&vault, b"orphan", "image/png");

    let report = scan_asset_gc(&vault, 10).expect("扫描");
    assert_eq!(report.protected_by_sync, 1);
    assert_eq!(report.candidate_count, 1);
    assert_eq!(report.candidates[0].hash, orphan);
    assert!(!report.fail_closed);
}

/// 无字节占位即便没有任何引用也不是候选。
#[test]
fn data_null_placeholder_is_never_a_candidate() {
    let (_dir, vault) = fresh_vault();
    let placeholder = [0x77; 32];
    insert_placeholder(&vault, &placeholder);

    let report = scan_asset_gc(&vault, 10).expect("扫描");
    assert_eq!(report.total_assets, 1);
    assert_eq!(report.assets_without_data, 1);
    assert_eq!(report.protected_by_no_bytes, 1);
    assert_eq!(report.protected_total, 1);
    assert_eq!(report.candidate_count, 0);
    assert!(report.candidates.is_empty());
    assert!(!report.fail_closed);
}

/// `limit` 只截断列表，不截断 total/bytes；顺序按 hash 稳定。
#[test]
fn limit_truncates_candidates_but_not_totals() {
    let (_dir, vault) = fresh_vault();
    let mut hashes = Vec::new();
    for byte in 1u8..=5 {
        hashes.push(insert(&vault, &[byte, byte, byte], "text/plain"));
    }
    // 报告按 asset_hash（BLOB）升序，而内容 hash 与插入顺序无关。
    hashes.sort_unstable();

    let report = scan_asset_gc(&vault, 2).expect("扫描");
    assert_eq!(report.candidate_count, 5);
    assert_eq!(report.candidate_bytes, 15);
    assert_eq!(report.candidates.len(), 2);
    assert_eq!(report.candidates[0].hash, hashes[0]);
    assert_eq!(report.candidates[1].hash, hashes[1]);
    assert_eq!(report.candidates_truncated, 3);
    assert_eq!(report.limit, 2);
    assert!(!report.limit_capped);

    let empty = scan_asset_gc(&vault, 0).expect("零上限扫描");
    assert_eq!(empty.candidate_count, 5);
    assert!(empty.candidates.is_empty());
    assert_eq!(empty.candidates_truncated, 5);
}

/// 重复调用完全只读：候选报告稳定，`assets` 行（含 `orphan_since`）逐字节不变。
#[test]
fn repeated_scan_does_not_modify_assets_or_orphan_since() {
    let (_dir, vault) = fresh_vault();
    seed_note(&vault, "note-1", "doc-1");

    let referenced = insert(&vault, b"referenced", "image/png");
    link_asset_ref(&vault, "note-1", &referenced, 0, 1).expect("建立引用");
    let orphan = insert(&vault, b"orphan", "image/png");
    set_asset_orphan_since(&vault, &orphan, Some(42)).expect("设置 orphan_since");

    let before = asset_rows(&vault);
    let report_first = scan_asset_gc(&vault, 10).expect("第一次扫描");
    let report_second = scan_asset_gc(&vault, 10).expect("第二次扫描");
    let after = asset_rows(&vault);

    assert_eq!(report_first, report_second, "重复扫描报告必须一致");
    assert_eq!(before, after, "assets 行（含 orphan_since/data）不得被修改");

    let orphan_row = after
        .iter()
        .find(|row| row.first() == Some(&Value::Blob(orphan.to_vec())))
        .expect("找到候选行");
    assert_eq!(
        orphan_row.get(4),
        Some(&Value::Integer(42)),
        "orphan_since 必须保持原值"
    );
    assert_eq!(report_first.candidate_count, 1);
    assert_eq!(report_first.candidates[0].hash, orphan);
}

/// 版本格式无法解析 => 整次扫描 fail-closed，候选为空但原因可诊断。
#[test]
fn unknown_version_format_fails_closed() {
    let (_dir, vault) = fresh_vault();
    seed_note(&vault, "note-1", "doc-1");
    let _orphan = insert(&vault, b"orphan", "image/png");
    vault
        .exec(
            "INSERT INTO versions(\
               note_id,captured_at,generation,frontier,title,body_zstd,asset_hashes\
             ) VALUES('note-1',0,1,X'00','坏版本',X'00',?1)",
            &[Value::Blob(vec![0xAB; 33])],
        )
        .expect("写坏版本行");

    let report = scan_asset_gc(&vault, 10).expect("扫描");
    assert!(report.fail_closed);
    assert_eq!(report.version_parse_errors, 1);
    assert_eq!(report.parse_errors, 1);
    assert_eq!(report.candidate_count, 0);
    assert!(report.candidates.is_empty());
    assert_eq!(report.unknown_protected, 1, "孤立资产被抑制");
    assert_eq!(
        report.skipped_unknown, 1,
        "fail-closed 也计入 skipped_unknown"
    );
    assert_eq!(report.total_assets, 1);
    assert_eq!(report.protected_total, 0);
    assert_eq!(
        report.total_assets,
        report.protected_total + report.candidate_count + report.skipped_unknown,
        "fail-closed 时也必须能对账"
    );
    assert!(report.has_no_candidates());
}

/// 无法解释的 outbox 行（NULL object_id）也必须 fail-closed，不得当成"零引用"。
#[test]
fn sync_outbox_without_object_id_fails_closed() {
    let (_dir, vault) = fresh_vault();
    let doc = seed_note(&vault, "note-1", "doc-1");
    doc.insert_scalars(0, "正文").expect("写正文");
    let head = commit_now(&vault, &doc, "body-batch");
    assert_eq!(head.head_seq, 1);
    vault
        .exec(
            "INSERT INTO sync_outbox(\
               message_id,batch_id,doc_id,seq,key_epoch,object_id,state,ciphertext\
             ) VALUES('gc-null-oid','gc-test',?1,1,1,NULL,'pending',NULL)",
            &[Value::Text(doc.doc_id().to_owned())],
        )
        .expect("写 object_id 为 NULL 的 outbox 行");
    let _orphan = insert(&vault, b"orphan", "image/png");

    let report = scan_asset_gc(&vault, 10).expect("扫描");
    assert!(report.fail_closed);
    assert_eq!(report.sync_outbox_rows, 1);
    assert_eq!(report.sync_parse_errors, 1);
    assert_eq!(report.candidate_count, 0);
    assert!(report.candidates.is_empty());
    assert_eq!(report.unknown_protected, 1);
    assert_eq!(report.skipped_unknown, 1);
    assert_eq!(
        report.total_assets,
        report.protected_total + report.candidate_count + report.skipped_unknown
    );
}

fn hex_lower(hash: &[u8; 32]) -> String {
    let mut out = String::with_capacity(64);
    for byte in hash {
        use std::fmt::Write as _;
        write!(out, "{byte:02x}").expect("写十六进制");
    }
    out
}
