//! S7-15：checkpoint / bootstrap 需要的全量文档与资产只读 API。

mod support;

use support::*;

use menghunote_storage::{
    asset_bytes, asset_meta, create_body_doc, ensure_structure_row, list_all_docs, list_assets,
    BodyDocMeta, DocKind, Value,
};

fn find_doc<'a>(docs: &'a [BodyDocMeta], doc_id: &str) -> &'a BodyDocMeta {
    docs.iter()
        .find(|doc| doc.doc_id == doc_id)
        .unwrap_or_else(|| panic!("缺少文档 {doc_id}"))
}

#[test]
fn empty_vault_lists_no_docs_or_assets() {
    let (_dir, vault) = fresh_vault();
    assert!(list_all_docs(&vault).expect("docs").is_empty());
    assert!(list_assets(&vault).expect("assets").is_empty());
    assert!(asset_meta(&vault, [0u8; 32]).expect("meta").is_none());
    assert!(asset_bytes(&vault, [0u8; 32]).expect("bytes").is_none());
}

#[test]
fn lists_structure_all_bodies_and_tombstones() {
    let (_dir, vault) = fresh_vault();
    let first = seed_note(&vault, "note-1", "doc-body-1");
    first.insert_scalars(0, "hello").unwrap();
    commit_now(&vault, &first, "batch-1");

    // 无 note 行的 body：方向 A——从未建过 note 的正文。
    create_body_doc(&vault, "doc-orphan").expect("无 note 的 body doc");

    // 方向 B——note 被删除后留下的墓碑（loro_docs 仍在）。
    let second = seed_note(&vault, "note-2", "doc-body-2");
    second.insert_scalars(0, "bye").unwrap();
    commit_now(&vault, &second, "batch-2");
    vault
        .exec(
            "DELETE FROM notes WHERE note_id=?1",
            &[Value::Text("note-2".to_owned())],
        )
        .expect("删除 note 行，保留 doc");

    ensure_structure_row(&vault).expect("建 structure 行");

    let docs = list_all_docs(&vault).expect("list docs");
    assert_eq!(
        docs.iter()
            .map(|doc| doc.doc_id.as_str())
            .collect::<Vec<_>>(),
        vec!["doc-body-1", "doc-body-2", "doc-orphan", "structure"]
    );
    assert_eq!(find_doc(&docs, "structure").kind, DocKind::Structure);
    assert_eq!(find_doc(&docs, "structure").kind, "structure");
    assert_eq!(find_doc(&docs, "doc-body-1").kind, DocKind::Body);
    assert_eq!(find_doc(&docs, "doc-body-1").kind, "body");
    assert_eq!(find_doc(&docs, "doc-body-1").head_seq, 1);
    assert_eq!(find_doc(&docs, "doc-body-2").head_seq, 1);
    assert_eq!(find_doc(&docs, "doc-orphan").head_seq, 0);
    assert_eq!(find_doc(&docs, "doc-orphan").generation, 1);
}

#[test]
fn lists_asset_metadata_without_data_and_reads_bytes_on_demand() {
    let (_dir, vault) = fresh_vault();
    let with_data = [0x11u8; 32];
    let without_data = [0x22u8; 32];

    vault
        .exec(
            "INSERT INTO assets(asset_hash, byte_len, media_type, data, orphan_since) \
             VALUES(?1, 3, 'text/plain', ?2, NULL)",
            &[
                Value::Blob(with_data.to_vec()),
                Value::Blob(b"abc".to_vec()),
            ],
        )
        .expect("有 data 的资产");
    vault
        .exec(
            "INSERT INTO assets(asset_hash, byte_len, media_type, data, orphan_since) \
             VALUES(?1, 7, 'image/png', NULL, 12345)",
            &[Value::Blob(without_data.to_vec())],
        )
        .expect("无 data 的资产");

    let assets = list_assets(&vault).expect("list assets");
    assert_eq!(assets.len(), 2);
    let loaded = assets
        .iter()
        .find(|asset| asset.asset_hash == with_data)
        .expect("有 data 元数据");
    assert_eq!(loaded.byte_len, Some(3));
    assert_eq!(loaded.media_type.as_deref(), Some("text/plain"));
    assert!(loaded.has_data);
    assert_eq!(loaded.orphan_since, None);

    let orphan = assets
        .iter()
        .find(|asset| asset.asset_hash == without_data)
        .expect("无 data 元数据");
    assert_eq!(orphan.byte_len, Some(7));
    assert_eq!(orphan.media_type.as_deref(), Some("image/png"));
    assert!(!orphan.has_data);
    assert_eq!(orphan.orphan_since, Some(12345));

    assert_eq!(
        asset_bytes(&vault, with_data).expect("读 data"),
        Some(b"abc".to_vec())
    );
    assert_eq!(asset_bytes(&vault, without_data).expect("缺 data"), None);
    assert_eq!(asset_bytes(&vault, [0x33u8; 32]).expect("缺失"), None);

    let meta = asset_meta(&vault, without_data)
        .expect("asset meta")
        .expect("存在");
    assert_eq!(meta, *orphan);
    assert!(
        asset_meta(&vault, with_data)
            .expect("asset meta")
            .expect("存在")
            .has_data
    );
    assert!(asset_meta(&vault, [0x33u8; 32])
        .expect("缺失 meta")
        .is_none());
}
