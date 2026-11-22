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
    load_body_doc, load_head, outbox_parts, outbox_row, sync_cursor_state, StorageError,
    SyncOutboxState, Vault,
};
use menghunote_sync::engine::{EngineIdentity, SyncEngine, RELAY_CURSOR_META};
use menghunote_sync::relay::{PauseReason, RelayLogKind};
use menghunote_sync::SyncError;
use menghunote_sync_proto::{Envelope, ObjectType};

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

#[test]
fn outbound_encrypt_push_ack_roundtrip() {
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
        let mut sync = engine(&vault_a, &mut relay, &device_a);
        assert_eq!(sync.pull().expect("拉取").stored, 2);
        let first = sync.integrate(16).expect("第一次集成");
        assert_eq!(first.integrated, 1, "update2 依赖未齐只能先集成 update1");
        assert_eq!(first.deferred, 1);
        assert_eq!(received(&vault_a), 2, "verified 行已 durable");
        let second = sync.integrate(16).expect("第二次集成");
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
        let report = sync.integrate(16).expect("集成");
        assert_eq!(report.integrated, 0);
        assert_eq!(report.deferred, 1);
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
fn head_conflict_is_retryable_and_export_retries_without_data_loss() {
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
