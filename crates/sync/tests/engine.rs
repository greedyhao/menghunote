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
