//! 普通同步的增量资产上传（FakeRelay + 真 SQLite，不 sleep）。
//!
//! 覆盖 attachment-insert 缺口：A 插入并引用新资产后，普通 `sync_once`（非发布基线）
//! 就应把 ASSET 对象增量上传，B 能取到与 A 一致的字节；第二个同步轮次不重传；
//! 多片资产传到一半失败后从缺失片续传；checkpoint 已上传的对象被普通同步直接复用/标记；
//! 被维护 reservation 持有的对象延后（ID_CONFLICT 不阻塞正文）。

mod support;

use support::*;

use menghunote_storage::sync_cursor_state;
use menghunote_storage::{
    asset_bytes, asset_content_hash, count_unreferenced_assets, insert_asset, link_asset_ref,
    Value, Vault, META_SYNC_LOG_ID,
};
use menghunote_sync::checkpoint::asset_object_id;
use menghunote_sync::engine::{EngineIdentity, SyncEngine};
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
fn asset_hash_mismatch_is_failed_without_uploading_wrong_bytes() {
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

    let report = {
        let mut sync = engine(&vault, &mut relay, &device);
        sync.sync_once().expect("正文同步不得被坏资产阻塞")
    };
    assert_eq!(report.assets.failed, 1);
    assert_eq!(report.assets.uploaded, 0);
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
