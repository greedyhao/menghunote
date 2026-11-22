//! ADR 0024 本机历史的 storage 门禁：v4 迁移、捕获/列表/预览/去重、保留策略与
//! `asset_hashes` 口径。
//!
//! 纪律：临时目录一律 `CARGO_TARGET_TMPDIR`（见 `support`），不写 `%TEMP%`。

mod support;

use support::*;

use menghunote_storage::{
    capture_note_version, list_note_versions, load_note_version, load_note_version_for_note,
    prune_note_versions, replace_asset_refs, StorageError, Value, Vault,
    HISTORY_MAX_VERSIONS_PER_NOTE, HISTORY_SOURCE_LOCAL, SCHEMA_VERSION,
};

fn count_versions(vault: &Vault, note_id: &str) -> i64 {
    let rows = vault
        .query(
            "SELECT count(*) FROM versions WHERE note_id=?1",
            &[Value::Text(note_id.to_owned())],
        )
        .expect("查询版本数");
    match rows.first().and_then(|row| row.first()) {
        Some(Value::Integer(count)) => *count,
        other => panic!("版本数不是整数：{other:?}"),
    }
}

#[test]
fn schema_v4_migrates_a_v3_database_and_keeps_old_versions() {
    let dir = temp_dir();
    let path = vault_path(&dir);
    std::fs::create_dir_all(path.parent().expect("父目录")).expect("创建父目录");

    // 手工建一个 v3 库（历史用户库），再交给 Vault 自动升到 v4。
    {
        let conn = rusqlite::Connection::open(&path).expect("打开 v3 连接");
        conn.execute_batch(include_str!("../src/schema/v1.sql"))
            .expect("建 v1 schema");
        conn.execute_batch(include_str!("../src/schema/v2.sql"))
            .expect("建 v2 schema");
        conn.execute_batch(include_str!("../src/schema/v3.sql"))
            .expect("建 v3 schema");
        conn.execute_batch(
            "INSERT INTO loro_docs(doc_id, kind, generation, app_doc_schema, codec_version, min_reader) \
             VALUES('doc-v3', 'body', 1, 1, 'loro-v1', 1); \
             INSERT INTO notes(note_id, body_doc_id, title, created_at, updated_at, deleted, is_favorite, is_pinned) \
             VALUES('note-v3', 'doc-v3', '旧笔记', 1, 1, 0, 0, 0); \
             INSERT INTO versions(version_id, note_id, captured_at, generation, frontier, title, body_zstd, asset_hashes) \
             VALUES(1, 'note-v3', 123, 1, x'', '旧标题', x'', x'');",
        )
        .expect("写 v3 的旧版本行");
        conn.pragma_update(None, "user_version", 3)
            .expect("写 v3 版本");
    }

    let vault = Vault::open(&path).expect("打开并升级 v3 库");
    assert_eq!(
        vault.user_version().expect("读取 user_version"),
        SCHEMA_VERSION,
        "迁移后必须是最新 schema 版本"
    );

    let columns: Vec<String> = vault
        .query("PRAGMA table_info(versions)", &[])
        .expect("table_info(versions)")
        .into_iter()
        .filter_map(|row| match row.get(1) {
            Some(Value::Text(name)) => Some(name.clone()),
            _ => None,
        })
        .collect();
    for expected in ["source", "device_id", "body_hash"] {
        assert!(
            columns.iter().any(|column| column == expected),
            "v4 迁移缺列 {expected}：{columns:?}"
        );
    }

    // 旧行原样保留：source 取默认 local，device_id/body_hash 为 NULL。
    let rows = vault
        .query(
            "SELECT source, device_id, body_hash FROM versions WHERE version_id=1",
            &[],
        )
        .expect("读旧版本行");
    assert_eq!(rows.len(), 1, "旧版本行不能被迁移删掉");
    assert_eq!(rows[0].first(), Some(&Value::Text("local".to_owned())));
    assert_eq!(rows[0].get(1), Some(&Value::Null));
    assert_eq!(rows[0].get(2), Some(&Value::Null));
}

#[test]
fn capture_list_preview_and_dedup_share_one_snapshot() {
    let (_dir, vault) = fresh_vault();
    seed_note(&vault, "note-1", "doc-1");

    let first = capture_note_version(&vault, "note-1", "标题甲", "第一版\n正文", 1_000)
        .expect("捕获第一版");
    assert!(first.captured, "第一次捕获必须写入");
    let first_id = first.version_id.expect("新版本必须有 id");

    let versions = list_note_versions(&vault, "note-1", 200).expect("列表");
    assert_eq!(versions.len(), 1, "第一次捕获后恰好一个版本");
    assert_eq!(versions[0].version_id, first_id);
    assert_eq!(versions[0].title, "标题甲");
    assert_eq!(versions[0].captured_at, 1_000);
    assert_eq!(versions[0].source, HISTORY_SOURCE_LOCAL);
    assert_eq!(versions[0].device_id, None);

    let loaded = load_note_version(&vault, first_id)
        .expect("读取版本")
        .expect("版本必须存在");
    assert_eq!(loaded.note_id, "note-1");
    assert_eq!(loaded.title, "标题甲");
    assert_eq!(loaded.body, "第一版\n正文");
    assert_eq!(loaded.source, HISTORY_SOURCE_LOCAL);
    assert_eq!(loaded.device_id, None);

    // 正文没变：不重复写，返回上一行的 id。
    let duplicate = capture_note_version(&vault, "note-1", "标题乙", "第一版\n正文", 2_000)
        .expect("重复捕获不报错");
    assert!(!duplicate.captured, "正文相同必须去重");
    assert_eq!(duplicate.version_id, Some(first_id));
    assert_eq!(count_versions(&vault, "note-1"), 1, "去重后仍只有一行");

    // 正文变了：追加，并按时间倒序列表；标题取捕获时的标题。
    let second =
        capture_note_version(&vault, "note-1", "标题乙", "第二版", 3_000).expect("捕获第二版");
    assert!(second.captured);
    let versions = list_note_versions(&vault, "note-1", 200).expect("列表");
    assert_eq!(versions.len(), 2);
    assert_eq!(versions[0].captured_at, 3_000, "最新版本排最前");
    assert_eq!(versions[0].title, "标题乙");
    assert_eq!(versions[1].version_id, first_id);
    let second_id = second.version_id.expect("第二版 id");
    assert_eq!(
        load_note_version(&vault, second_id)
            .expect("读取第二版")
            .expect("第二版存在")
            .body,
        "第二版"
    );

    // 版本属于笔记：跨笔记读取返回 None（覆盖层防 stale version_id）。
    seed_note(&vault, "note-2", "doc-2");
    assert_eq!(
        load_note_version_for_note(&vault, "note-2", first_id).expect("跨笔记读取"),
        None,
        "不能把 note-1 的版本当成 note-2 的"
    );

    // 不存在的笔记：明确报错，不静默丢弃。
    let error = capture_note_version(&vault, "missing", "标题", "正文", 1)
        .expect_err("不存在的笔记必须报错");
    assert!(matches!(error, StorageError::Corrupt { .. }), "{error}");
}

#[test]
fn capture_concatenates_asset_hashes_from_asset_refs() {
    let (_dir, vault) = fresh_vault();
    seed_note(&vault, "note-1", "doc-1");

    // 乱序写入，捕获时必须按 hash 升序去重拼接。
    replace_asset_refs(
        &vault,
        "note-1",
        0,
        &[([2u8; 32], 0), ([1u8; 32], 10), ([2u8; 32], 20)],
    )
    .expect("写资产引用");

    let captured =
        capture_note_version(&vault, "note-1", "带图", "![图](asset://aa)", 1_000).expect("捕获");
    assert!(captured.captured);

    let rows = vault
        .query(
            "SELECT asset_hashes FROM versions WHERE note_id='note-1'",
            &[],
        )
        .expect("读 asset_hashes");
    let Some(Value::Blob(bytes)) = rows.first().and_then(|row| row.first()) else {
        panic!("asset_hashes 必须是 BLOB");
    };
    assert_eq!(bytes.len(), 64, "两个 32B hash 直接拼接");
    assert_eq!(&bytes[..32], &[1u8; 32], "按 hash 升序");
    assert_eq!(&bytes[32..], &[2u8; 32]);
}

#[test]
fn retention_keeps_only_the_latest_200_versions_per_note() {
    let (_dir, vault) = fresh_vault();
    seed_note(&vault, "note-1", "doc-1");

    for index in 1..=(HISTORY_MAX_VERSIONS_PER_NOTE + 1) {
        capture_note_version(
            &vault,
            "note-1",
            "标题",
            &format!("正文 {index}"),
            index as i64,
        )
        .expect("捕获");
    }

    assert_eq!(
        count_versions(&vault, "note-1"),
        HISTORY_MAX_VERSIONS_PER_NOTE as i64,
        "第 201 个版本写入后必须裁到 200"
    );
    let versions = list_note_versions(&vault, "note-1", 999).expect("列表");
    assert_eq!(versions.len(), HISTORY_MAX_VERSIONS_PER_NOTE);
    assert_eq!(
        versions.first().map(|version| version.captured_at),
        Some((HISTORY_MAX_VERSIONS_PER_NOTE + 1) as i64),
        "最新的版本必须保留"
    );
    assert_eq!(
        versions.last().map(|version| version.captured_at),
        Some(2),
        "最旧的版本（1）被裁掉"
    );
}

#[test]
fn retention_drops_versions_older_than_90_days() {
    let (_dir, vault) = fresh_vault();
    seed_note(&vault, "note-1", "doc-1");

    let day = 24 * 60 * 60 * 1_000;
    capture_note_version(&vault, "note-1", "旧", "旧正文", 0).expect("旧版本");
    capture_note_version(&vault, "note-1", "新", "新正文", 91 * day).expect("新版本");

    assert_eq!(
        count_versions(&vault, "note-1"),
        1,
        "捕获时同一事务里必须删掉 90 天前的版本"
    );
    let versions = list_note_versions(&vault, "note-1", 200).expect("列表");
    assert_eq!(versions[0].title, "新");

    // 独立的显式清理入口：10 天前的保留，90 天前的删除。
    capture_note_version(
        &vault,
        "note-1",
        "十天前",
        "十天前正文",
        91 * day + 10 * day,
    )
    .expect("十天前版本");
    let removed = prune_note_versions(&vault, "note-1", 91 * day + 10 * day).expect("清理");
    assert_eq!(removed, 0, "91 天内的版本不应被清理");
    let removed = prune_note_versions(&vault, "note-1", 91 * day + 100 * day).expect("清理");
    assert_eq!(removed, 1, "超过 90 天的版本被清理");
}
