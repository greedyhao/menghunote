//! S7-16：资产写入、引用与 orphan 标记。

mod support;

use support::*;

use menghunote_storage::{
    asset_bytes, asset_meta, insert_asset, link_asset_ref, list_asset_refs, list_assets,
    set_asset_orphan_since, unlink_asset_ref, AssetRef,
};

#[test]
fn insert_is_consistent_idempotent_and_rejects_conflicts() {
    let (_dir, vault) = fresh_vault();
    let hash = [0xA1u8; 32];

    insert_asset(&vault, &hash, 3, Some("text/plain"), b"abc").expect("首次插入");
    let meta = asset_meta(&vault, hash).unwrap().expect("元数据");
    assert_eq!(meta.asset_hash, hash);
    assert_eq!(meta.byte_len, Some(3));
    assert_eq!(meta.media_type.as_deref(), Some("text/plain"));
    assert!(meta.has_data);
    assert_eq!(asset_bytes(&vault, hash).unwrap(), Some(b"abc".to_vec()));
    assert_eq!(list_assets(&vault).unwrap().len(), 1);

    // 幂等：同 hash、同字节、同长度重插成功。
    insert_asset(&vault, &hash, 3, Some("ignored/second"), b"abc").expect("幂等重插");
    assert_eq!(list_assets(&vault).unwrap().len(), 1);

    // 同 hash 不同内容（同长度）拒绝。
    assert!(insert_asset(&vault, &hash, 3, Some("text/plain"), b"abd").is_err());
    // 同 hash 不同长度拒绝。
    assert!(insert_asset(&vault, &hash, 4, Some("text/plain"), b"abcd").is_err());
    // byte_len 与 data.len 不符拒绝。
    assert!(insert_asset(&vault, &[0xA2u8; 32], 4, Some("text/plain"), b"abc").is_err());
    // 空明文拒绝。
    assert!(insert_asset(&vault, &[0xA3u8; 32], 0, Some("text/plain"), b"").is_err());

    assert_eq!(list_assets(&vault).unwrap().len(), 1);
}

#[test]
fn refs_can_be_linked_updated_listed_and_unlinked() {
    let (_dir, vault) = fresh_vault();
    let hash = [0xB1u8; 32];
    insert_asset(&vault, &hash, 3, Some("text/plain"), b"abc").unwrap();
    seed_note(&vault, "note-1", "doc-1");

    assert!(list_asset_refs(&vault, "note-1").unwrap().is_empty());

    link_asset_ref(&vault, "note-1", &hash, 10, 1).expect("link 10");
    link_asset_ref(&vault, "note-1", &hash, 20, 1).expect("link 20");
    let refs = list_asset_refs(&vault, "note-1").unwrap();
    assert_eq!(refs.len(), 2);
    assert_eq!(
        refs,
        vec![
            AssetRef {
                note_id: "note-1".to_owned(),
                asset_hash: hash,
                start_byte: 10,
                body_seq: 1,
            },
            AssetRef {
                note_id: "note-1".to_owned(),
                asset_hash: hash,
                start_byte: 20,
                body_seq: 1,
            },
        ]
    );

    // 同 PK 重链只更新 body_seq，不增加行。
    link_asset_ref(&vault, "note-1", &hash, 10, 2).expect("更新 body_seq");
    let refs = list_asset_refs(&vault, "note-1").unwrap();
    assert_eq!(refs.len(), 2);
    assert_eq!(refs[0].body_seq, 2);

    assert!(unlink_asset_ref(&vault, "note-1", &hash, 10).unwrap());
    assert!(!unlink_asset_ref(&vault, "note-1", &hash, 10).unwrap());
    let refs = list_asset_refs(&vault, "note-1").unwrap();
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].start_byte, 20);
}

#[test]
fn orphan_since_marker_roundtrips_and_missing_asset_is_noop() {
    let (_dir, vault) = fresh_vault();
    let hash = [0xC1u8; 32];
    insert_asset(&vault, &hash, 3, None, b"abc").unwrap();

    assert!(set_asset_orphan_since(&vault, &hash, Some(1234)).unwrap());
    let meta = asset_meta(&vault, hash).unwrap().expect("元数据");
    assert_eq!(meta.orphan_since, Some(1234));
    assert_eq!(list_assets(&vault).unwrap()[0].orphan_since, Some(1234));

    assert!(set_asset_orphan_since(&vault, &hash, None).unwrap());
    assert_eq!(
        asset_meta(&vault, hash)
            .unwrap()
            .expect("元数据")
            .orphan_since,
        None
    );
    assert!(!set_asset_orphan_since(&vault, &[0xC2u8; 32], Some(1)).unwrap());
}
