//! ADR 0009 Phase 2 / 切片 5：`DevLocal::rotate` + `DevLocal::apply_latest_epoch` 的集成测试。
//!
//! 用 `menghunote-dev-relay`（进程内真实 axum relay + 临时 SQLite + FakeClock +
//! `127.0.0.1:0`），客户端走真实 HTTP（`StdTcpTransport::new_local_dev`）。**非生产**：
//! dev-relay 的 `DevPermissiveHook` 放行验签，本文件只证明"轮换编排/数据面/持久事实"，
//! 不能作为鉴权验收（那需要 `relay::Ed25519Hook`，见 e2e）。
//!
//! 纪律（AGENTS.md §6）：
//! - 临时目录一律在 `CARGO_TARGET_TMPDIR`（不用 `%TEMP%`）；
//! - 收敛点只用显式返回值（`sync_once` 的 durable ACK/integrate、`rotate`/`apply`/`bootstrap`
//!   的返回），**不用 sleep**；
//! - 身份持久事实必须从**磁盘**读回（`LocalIdentityV1::unprotect`），不只看内存；
//! - rotation 旁路事实（epoch/control_rev/manager_rev/历史根/认证 pin/repack 屏障）必须从
//!   `vault.meta` 读回，证明 durable 而不是进程内状态。
//!
//! 与任务书第 3 项的一处**真实差异**（已在测试里写清）：`rotate` 的第一步要求先
//! `sync_once` 到最新（本任务顺序），所以"A 轮换前的未发送 outbox"会被前置同步推平，
//! 不会留到阶段 J 重包。A 侧重包由 [`rotate_failure_after_activation_keeps_identity_and_apply_recovers`]
//! 用**真实旧 epoch outbox**覆盖（发起设备推广失败 → 用本机 grant apply 追平 → 重包）。

use std::path::{Path, PathBuf};

use menghunote_dev_relay::DevRelay;
use menghunote_storage::{
    append_updates_with_sync_intents, asset_bytes, asset_content_hash, insert_asset,
    link_asset_ref, list_tree, load_body_doc, local_sync_intent, outbox_parts, outbox_row,
    outbox_rows, repack_required, sync_cursor_state, BodyDoc, HeadState, StructureWriter,
    SyncContext, SyncOutboxRow, SyncOutboxState, UpdateRecord, Value, Vault,
};
use menghunote_sync::api_version::RelayApiVersion;
use menghunote_sync::checkpoint::{asset_object_id, CheckpointWriter, StorageAssets};
use menghunote_sync::dev_local::{
    ApplyOutcome, CreatedVault, DevLocal, JoinedVault, RotationOutcome,
};
use menghunote_sync::device::DeviceSigningKey;
use menghunote_sync::http_relay::{
    HttpRelayClient, HttpRelayConfig, MemberStatus, StdTcpTransport,
};
use menghunote_sync::identity::{
    IdentityRotationStateV1, LocalIdentityV1, IDENTITY_ROTATION_STATE_META,
};
use menghunote_sync::keys::{RootSecretV1, Secret32};
use menghunote_sync::relay::RelayClient;
use menghunote_sync::rotation_activate::{AuthenticatedPinV1, AUTHENTICATED_PIN_META};
use menghunote_sync::rotation_journal::{RotationApplyPhase, RotationJournal, RotationPhase};
use menghunote_sync::upload::{decrypt_envelope, fetch_object};
use menghunote_sync::{DeviceId, SyncError, VaultId};
use menghunote_sync_proto::{ManifestV1, ObjectType};
use tempfile::{Builder as TempDirBuilder, TempDir};

fn temp_root(prefix: &str) -> TempDir {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "rotation-dev-local-{prefix}-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).expect("创建 target/tmp 下的测试根目录失败");
    TempDirBuilder::new()
        .prefix("case-")
        .tempdir_in(&root)
        .expect("在 target/tmp 下创建临时目录失败")
}

/// Linux 上 `LocalIdentityV1::protect` 需要每用户主钥（Windows DPAPI 是 no-op）。
/// 同一测试二进制内的多个测试并发跑，用 `OnceLock` 保证只初始化一次。
fn init_identity_wrap() {
    static INIT: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
    let result = INIT.get_or_init(|| {
        menghunote_sync::identity_wrap::initialize_user_key().map_err(|error| error.to_string())
    });
    if let Err(error) = result {
        panic!("初始化本地身份封装主键失败：{error}");
    }
}

/// 把身份按 `LocalIdentityV1::protect()` 原字节落盘（与 `promote_identity_with_material`
/// 写回的是同一格式；app 侧切片 6 需要把 `DevIdentityStore` 适配成这条路径）。
fn persist_identity(path: &Path, identity: &LocalIdentityV1) {
    let blob = identity.protect().expect("保护本地身份失败");
    std::fs::write(path, blob).expect("写本地身份失败");
}

fn load_identity(path: &Path, vault_id: &VaultId, device_id: &DeviceId) -> LocalIdentityV1 {
    let blob = std::fs::read(path).expect("读本地身份失败");
    LocalIdentityV1::unprotect(&blob, vault_id, device_id).expect("解封本地身份失败")
}

fn file_sha256(path: &Path) -> [u8; 32] {
    use sha2::Digest as _;
    sha2::Sha256::digest(std::fs::read(path).expect("读文件失败")).into()
}

/// 一篇笔记的稳定标识（结构节点 + 正文文档 + 写入的文字）。
struct Note {
    doc_id: String,
}

fn create_note(vault: &Vault, peer: u64, epoch: u64, title: &str, text: &str) -> Note {
    let ctx = SyncContext::new(peer, epoch);
    let mut writer =
        StructureWriter::open_with_sync(vault, peer, Some(ctx)).expect("打开结构写入口失败");
    let seed = writer
        .doc()
        .create_note(title, 1)
        .expect("结构 CRDT 新建笔记失败");
    writer
        .commit("rotation-dev-local-note")
        .expect("结构提交失败");
    write_body(vault, &seed.body_doc_id, peer, epoch, text);
    Note {
        doc_id: seed.body_doc_id,
    }
}

fn write_body(vault: &Vault, doc_id: &str, peer: u64, epoch: u64, text: &str) {
    let (doc, head) = load_body_doc(vault, doc_id, peer, None).expect("加载正文失败");
    doc.insert_scalars(0, text).expect("写入正文失败");
    commit_body(
        vault,
        doc_id,
        peer,
        epoch,
        &doc,
        &head,
        "rotation-dev-local-body",
    );
}

fn append_body(vault: &Vault, doc_id: &str, peer: u64, epoch: u64, suffix: &str) {
    let (doc, head) = load_body_doc(vault, doc_id, peer, None).expect("加载正文失败");
    let position = doc.scalar_len();
    doc.insert_scalars(position, suffix).expect("追加正文失败");
    commit_body(
        vault,
        doc_id,
        peer,
        epoch,
        &doc,
        &head,
        "rotation-dev-local-append",
    );
}

fn commit_body(
    vault: &Vault,
    doc_id: &str,
    peer: u64,
    epoch: u64,
    doc: &BodyDoc,
    head: &HeadState,
    batch: &str,
) {
    let base = head.version_vector().expect("解码 head VV 失败");
    let update = doc.pending_update(&base).expect("导出增量失败");
    let seq = head.head_seq.saturating_add(1);
    let intent = local_sync_intent(doc_id, seq, batch, SyncContext::new(peer, epoch))
        .expect("派生 sync intent 失败");
    append_updates_with_sync_intents(
        vault,
        doc_id,
        head.head_seq,
        &[UpdateRecord::new(intent.batch_id.clone(), update)],
        &[intent],
    )
    .expect("A 事务写入正文 + intent 失败");
}

fn body_text(vault: &Vault, doc_id: &str, peer: u64) -> String {
    load_body_doc(vault, doc_id, peer, None)
        .expect("加载正文失败")
        .0
        .text()
}

fn outbox_messages(vault: &Vault) -> Vec<SyncOutboxRow> {
    let mut rows =
        outbox_rows(vault, SyncOutboxState::Pending, 100).expect("读 pending outbox 失败");
    rows.extend(
        outbox_rows(vault, SyncOutboxState::Encrypted, 100).expect("读 encrypted outbox 失败"),
    );
    rows.extend(outbox_rows(vault, SyncOutboxState::Sent, 100).expect("读 sent outbox 失败"));
    rows
}

/// `list_tree` 里某个正文文档对应的 note_id（`asset_refs.note_id` 用的键）。
fn note_id_for_body(vault: &Vault, doc_id: &str) -> String {
    list_tree(vault)
        .expect("读结构树失败")
        .into_iter()
        .find(|entry| entry.body_doc_id.as_deref() == Some(doc_id))
        .and_then(|entry| entry.note_id)
        .expect("结构树必须有该正文的 note 节点")
}

/// 在正文里插入一份真实资产（等价于 app 的 attachment-insert 写库部分）。
fn insert_referenced_asset(vault: &Vault, note_id: &str, data: &[u8]) -> [u8; 32] {
    let hash = asset_content_hash(data);
    insert_asset(vault, &hash, data.len() as u64, Some("text/plain"), data).expect("insert_asset");
    link_asset_ref(vault, note_id, &hash, 0, 1).expect("link_asset_ref");
    hash
}

/// 清空本机 "资产已上传" durable 标记：模拟另一台设备（或标记丢失）在轮换后发布
/// 普通 checkpoint —— 它不知道 relay 上已有同 object_id 的确定性密文，会真正重传。
fn clear_asset_markers(vault: &Vault) {
    const PREFIX: &str = "sync.asset_uploaded.";
    vault
        .exec(
            "DELETE FROM meta WHERE substr(key,1,?1)=?2",
            &[
                Value::Integer(PREFIX.len() as i64),
                Value::Text(PREFIX.to_owned()),
            ],
        )
        .expect("清 asset marker");
}

/// 本机 "资产已上传" durable 标记数量。
fn asset_marker_count(vault: &Vault) -> i64 {
    const PREFIX: &str = "sync.asset_uploaded.";
    let rows = vault
        .query(
            "SELECT COUNT(*) FROM meta WHERE substr(key,1,?1)=?2",
            &[
                Value::Integer(PREFIX.len() as i64),
                Value::Text(PREFIX.to_owned()),
            ],
        )
        .expect("读 asset marker");
    match rows.first().and_then(|row| row.first()) {
        Some(Value::Integer(count)) => *count,
        other => panic!("COUNT(*) 必须是整数，实际 {other:?}"),
    }
}

fn test_client(
    relay: &str,
    vault_id: &VaultId,
    device_id: DeviceId,
    seed: &Secret32,
    manager_pub: [u8; 32],
) -> HttpRelayClient<StdTcpTransport> {
    HttpRelayClient::new(
        StdTcpTransport::new_local_dev(relay).expect("构造本地传输失败"),
        HttpRelayConfig {
            base_url: relay.to_owned(),
            api_version: RelayApiVersion::V1,
            vault_id: *vault_id,
            device_id,
            manager_pub: Some(manager_pub),
        },
        DeviceSigningKey::from_seed(seed.clone()),
    )
}

/// A create（含一篇笔记）→ 发布初始基线 → B 恢复码 join → B 首次 sync。
struct Fixture {
    relay: DevRelay,
    _dir: TempDir,
    a_path: PathBuf,
    b_path: PathBuf,
    a_identity_path: PathBuf,
    b_identity_path: PathBuf,
    a: CreatedVault,
    b: JoinedVault,
    note: Note,
}

fn establish(prefix: &str) -> Fixture {
    init_identity_wrap();
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let dir = temp_root(prefix);
    let a_path = dir.path().join("a").join("vault.sqlite3");
    let a = DevLocal::create(relay.base_url(), relay.setup_secret(), &a_path, "A 实例")
        .expect("A create 失败");
    let note = create_note(
        &a.vault,
        a.identity.loro_peer_id(),
        a.identity.current_epoch,
        "A 的笔记",
        "A 写的第一行\n",
    );
    DevLocal::publish_initial_pair(relay.base_url(), &a.vault, &a.identity, &a.recovery_code)
        .expect("A 发布初始基线失败");
    let b_path = dir.path().join("b").join("vault.sqlite3");
    let b = DevLocal::join(relay.base_url(), &a.vault_id, &a.recovery_code, &b_path)
        .expect("B join 失败");
    DevLocal::sync_once(relay.base_url(), &b.vault, &b.identity).expect("B 首轮 sync_once 失败");

    let a_identity_path = a_path.parent().expect("A 目录").join("identity.bin");
    let b_identity_path = b_path.parent().expect("B 目录").join("identity.bin");
    persist_identity(&a_identity_path, &a.identity);
    persist_identity(&b_identity_path, &b.identity);
    Fixture {
        relay,
        _dir: dir,
        a_path,
        b_path,
        a_identity_path,
        b_identity_path,
        a,
        b,
        note,
    }
}

fn rotate_empty_revoke(fixture: &mut Fixture) -> RotationOutcome {
    let relay = fixture.relay.base_url().to_owned();
    DevLocal::rotate(
        &relay,
        &fixture.a.vault,
        &fixture.a_identity_path,
        &mut fixture.a.identity,
        &[],
        None,
    )
    .expect("A rotate 失败")
}

fn apply_b(fixture: &mut Fixture) -> ApplyOutcome {
    let relay = fixture.relay.base_url().to_owned();
    DevLocal::apply_latest_epoch(
        &relay,
        &fixture.b.vault,
        &fixture.b_identity_path,
        &mut fixture.b.identity,
    )
    .expect("B apply_latest_epoch 失败")
}

/// 断言 A 侧轮换后的 durable 事实（身份文件 + 旁路状态 + 认证 pin + repack 屏障）。
fn assert_a_promoted(fixture: &Fixture, rotation: &RotationOutcome, old_root_epoch: u64) {
    let a = &fixture.a;
    assert_eq!(a.identity.current_epoch, rotation.to_epoch);
    assert_eq!(a.identity.control_rev, rotation.control_rev);
    assert!(
        a.identity.pending_rotation.is_none(),
        "推广后不得残留 pending_rotation"
    );
    // 磁盘身份才是 durable 真源。
    let disk = load_identity(&fixture.a_identity_path, &a.vault_id, &a.identity.device_id);
    assert_eq!(
        disk.current_epoch, rotation.to_epoch,
        "身份文件 epoch 未推广"
    );
    assert_eq!(disk.control_rev, rotation.control_rev);
    assert!(disk.current_root.ct_eq(&a.identity.current_root));
    assert!(
        disk.pinned_manager == a.identity.pinned_manager,
        "本片不轮换管理 key：pin 必须仍是同一把"
    );
    // 旁路状态：新 epoch/管理 rev/旧根（旧 outbox 重包需要）。
    let state = IdentityRotationStateV1::load_current(&a.vault, &a.identity)
        .expect("读身份旁路状态失败")
        .expect("轮换后必须有身份旁路状态");
    assert_eq!(state.current_epoch, rotation.to_epoch);
    assert_eq!(state.manager_rev, rotation.manager_rev);
    assert_eq!(state.control_rev, rotation.control_rev);
    assert!(
        state.root_for_epoch(old_root_epoch).is_some(),
        "推广必须保留旧 epoch 根（重包期）"
    );
    // 认证 pin（A 侧也写：instance/epoch/control_rev/activation hash）。
    let pin = AuthenticatedPinV1::load_current(&a.vault, &a.identity)
        .expect("读认证 pin 失败")
        .expect("推广后必须有认证 pin");
    assert_eq!(pin.epoch, rotation.to_epoch);
    assert_eq!(pin.control_rev, rotation.control_rev);
    assert_eq!(pin.activation_hash, rotation.activation_commit_hash);
    assert!(a
        .vault
        .get_meta(IDENTITY_ROTATION_STATE_META)
        .expect("读旁路 meta 失败")
        .is_some());
    assert!(a
        .vault
        .get_meta(AUTHENTICATED_PIN_META)
        .expect("读 pin meta 失败")
        .is_some());
    // 重包完成后屏障必须清掉（引擎据此解冻）。
    assert!(!repack_required(&a.vault).expect("读 repack 屏障失败"));
}

fn assert_b_promoted(fixture: &Fixture, outcome: &ApplyOutcome, expected_epoch: u64) {
    let ApplyOutcome::Applied {
        to_epoch,
        control_rev,
        manager_rev,
        repack,
        ..
    } = outcome
    else {
        panic!("B apply 未成功：{outcome:?}");
    };
    assert_eq!(*to_epoch, expected_epoch);
    assert_eq!(repack.failed, 0);
    assert_eq!(repack.conflicts, 0);
    let b = &fixture.b;
    assert_eq!(b.identity.current_epoch, expected_epoch);
    assert_eq!(b.identity.control_rev, *control_rev);
    let disk = load_identity(&fixture.b_identity_path, &b.vault_id, &b.identity.device_id);
    assert_eq!(disk.current_epoch, expected_epoch, "B 身份文件未推广");
    assert_eq!(disk.control_rev, *control_rev);
    assert!(disk.current_root.ct_eq(&b.identity.current_root));
    assert!(disk.pending_rotation.is_none());
    let state = IdentityRotationStateV1::load_current(&b.vault, &b.identity)
        .expect("读 B 身份旁路状态失败")
        .expect("B 推广后必须有旁路状态");
    assert_eq!(state.current_epoch, expected_epoch);
    assert_eq!(state.manager_rev, *manager_rev);
    let pin = AuthenticatedPinV1::load_current(&b.vault, &b.identity)
        .expect("读 B 认证 pin 失败")
        .expect("B 推广后必须有认证 pin");
    assert_eq!(pin.epoch, expected_epoch);
    assert_eq!(pin.control_rev, *control_rev);
    assert!(!repack_required(&b.vault).expect("读 B repack 屏障失败"));
}

// ---------------------------------------------------------------------------
// 1) A rotate → B apply：epoch=2，正文一致，双方继续编辑双向可见
// ---------------------------------------------------------------------------

#[test]
fn rotate_and_apply_move_both_devices_to_epoch_two() {
    let mut fixture = establish("epoch-two");
    let relay = fixture.relay.base_url().to_owned();
    let doc_id = fixture.note.doc_id.clone();
    let a_peer = fixture.a.identity.loro_peer_id();
    let b_peer = fixture.b.identity.loro_peer_id();
    let old_a_root = fixture.a.identity.current_root.clone();
    let old_manager_public = fixture.a.identity.pinned_manager;

    // A 编辑并同步（rotate 之前 durable）。
    append_body(&fixture.a.vault, &doc_id, a_peer, 1, "A 轮换前的一行\n");
    DevLocal::sync_once(&relay, &fixture.a.vault, &fixture.a.identity).expect("A 轮换前 sync 失败");

    // A rotate（revoke 空）。
    let rotation = rotate_empty_revoke(&mut fixture);
    assert_eq!(rotation.from_epoch, 1);
    assert_eq!(rotation.to_epoch, 2);
    assert!(rotation.revoked.is_empty());
    assert!(
        rotation.grants >= 2,
        "A/B 都应在 active 集合里拿到 grant，实际 {}",
        rotation.grants
    );
    assert_ne!(
        rotation.checkpoint_ids[0], rotation.checkpoint_ids[1],
        "两份候选基线必须是不同 cid"
    );
    for covered in rotation.covered_seqs {
        assert!(
            covered >= rotation.freeze_seq,
            "候选基线的 S 必须覆盖冻结点"
        );
    }
    assert_eq!(rotation.repack.repacked, 0);
    assert_eq!(rotation.repack.failed, 0);
    assert!(!fixture.a.identity.current_root.ct_eq(&old_a_root));
    assert_eq!(
        fixture.a.identity.pinned_manager, old_manager_public,
        "本片不轮换管理 key"
    );
    assert_a_promoted(&fixture, &rotation, 1);

    // B apply（离线设备自己用 grant 解根）。
    let applied = apply_b(&mut fixture);
    assert_b_promoted(&fixture, &applied, 2);
    let (b_state, b_pin) = {
        let state = IdentityRotationStateV1::load_current(&fixture.b.vault, &fixture.b.identity)
            .expect("读 B 旁路状态失败")
            .expect("B 必须有旁路状态");
        let pin = AuthenticatedPinV1::load_current(&fixture.b.vault, &fixture.b.identity)
            .expect("读 B pin 失败")
            .expect("B 必须有认证 pin");
        (state, pin)
    };
    assert_eq!(
        b_pin.activation_hash, rotation.activation_commit_hash,
        "B 的认证 pin 必须绑定本次 activation commit"
    );
    assert_eq!(b_state.manager_rev, rotation.manager_rev);
    assert!(
        b_state.root_for_epoch(1).is_some(),
        "B 必须保留旧 epoch 根（旧 outbox 重包）"
    );
    // 两份实现（A 的推广 / B 的解根）必须落到同一个新根。
    assert!(fixture
        .a
        .identity
        .current_root
        .ct_eq(&fixture.b.identity.current_root));

    // 旧 epoch 根解不开新 epoch 基线快照（真实密码学隔离，不是"看着像"）。
    let new_root = RootSecretV1::new(
        fixture.a.vault_id,
        2,
        fixture.a.identity.current_root.clone(),
    );
    let old_root = RootSecretV1::new(fixture.a.vault_id, 1, old_a_root);
    let mut client = test_client(
        &relay,
        &fixture.b.vault_id,
        fixture.b.identity.device_id,
        &fixture.b.identity.ed25519_seed,
        fixture.b.identity.pinned_manager,
    );
    let bundle = client.bootstrap().expect("bootstrap 失败");
    assert_eq!(bundle.epoch, 2);
    let manifest_oid = bundle.primary.manifest_oid;
    let (manifest_bytes, _digest) = menghunote_sync::rotation_apply::fetch_manifest_object(
        &mut client,
        &new_root,
        &manifest_oid,
    )
    .expect("新根解 manifest 失败");
    let manifest = ManifestV1::decode(&manifest_bytes).expect("manifest 解码失败");
    let doc = manifest.docs.first().expect("manifest 没有文档快照");
    assert!(
        fetch_object(
            &mut client,
            &old_root,
            ObjectType::Snapshot,
            &doc.snapshot_object_id,
            doc.part_count,
            doc.total_plain_len,
            &doc.object_digest,
            &[],
        )
        .is_err(),
        "旧 epoch 根竟然解开了新 epoch 快照"
    );
    assert!(
        fetch_object(
            &mut client,
            &new_root,
            ObjectType::Snapshot,
            &doc.snapshot_object_id,
            doc.part_count,
            doc.total_plain_len,
            &doc.object_digest,
            &[],
        )
        .is_ok(),
        "新 epoch 根必须能解开新快照"
    );

    // 双方继续编辑 + sync：新 epoch 双向可见。
    append_body(&fixture.a.vault, &doc_id, a_peer, 2, "A 轮换后写的一行\n");
    append_body(&fixture.b.vault, &doc_id, b_peer, 2, "B 轮换后写的一行\n");
    DevLocal::sync_once(&relay, &fixture.a.vault, &fixture.a.identity).expect("A 轮换后 sync 失败");
    DevLocal::sync_once(&relay, &fixture.b.vault, &fixture.b.identity).expect("B 轮换后 sync 失败");
    DevLocal::sync_once(&relay, &fixture.a.vault, &fixture.a.identity).expect("A 收尾 sync 失败");
    let a_text = body_text(&fixture.a.vault, &doc_id, a_peer);
    let b_text = body_text(&fixture.b.vault, &doc_id, b_peer);
    for text in [&a_text, &b_text] {
        assert!(text.contains("A 轮换前的一行"));
        assert!(text.contains("A 轮换后写的一行"));
        assert!(text.contains("B 轮换后写的一行"));
    }
}

// ---------------------------------------------------------------------------
// 2) B 离线旧 epoch outbox → A rotate → B apply → repack 后以新 epoch 推送可见
// ---------------------------------------------------------------------------

#[test]
fn b_offline_edit_is_repacked_to_new_epoch() {
    let mut fixture = establish("b-offline");
    let relay = fixture.relay.base_url().to_owned();
    let doc_id = fixture.note.doc_id.clone();
    let b_peer = fixture.b.identity.loro_peer_id();
    let a_peer = fixture.a.identity.loro_peer_id();

    // B 离线编辑（旧 epoch=1 的本地 intent；不触网）。
    append_body(
        &fixture.b.vault,
        &doc_id,
        b_peer,
        1,
        "B 轮换前离线写的一行\n",
    );
    let b_rows = outbox_messages(&fixture.b.vault);
    assert_eq!(b_rows.len(), 1, "B 离线编辑必须产生一条旧 outbox");
    let old_message = b_rows[0].message_id.clone();
    assert_eq!(b_rows[0].key_epoch, 1);

    // A rotate，B 全程离线（身份/outbox 不动）。
    let rotation = rotate_empty_revoke(&mut fixture);
    assert_eq!(rotation.to_epoch, 2);
    assert_eq!(
        outbox_messages(&fixture.b.vault).len(),
        1,
        "B 在 apply 前不得被改动"
    );

    let applied = apply_b(&mut fixture);
    assert_b_promoted(&fixture, &applied, 2);
    let ApplyOutcome::Applied { repack, .. } = &applied else {
        unreachable!()
    };
    assert_eq!(repack.repacked, 1, "B 的旧 epoch outbox 必须被重包");
    assert_eq!(repack.failed, 0);
    assert_eq!(repack.conflicts, 0);
    assert!(
        outbox_row(&fixture.b.vault, &old_message)
            .expect("读旧 outbox 行失败")
            .is_none(),
        "重包后旧行必须被删除"
    );
    let new_rows = outbox_messages(&fixture.b.vault);
    assert_eq!(new_rows.len(), 1, "重包后只应有一条新行（不重复）");
    assert_eq!(new_rows[0].key_epoch, 2, "重包新行必须是新 epoch");
    let new_message = new_rows[0].message_id.clone();
    let new_parts = outbox_parts(&fixture.b.vault, &new_message, 2).expect("读新 parts 失败");
    assert!(!new_parts.is_empty());
    let new_object = new_rows[0].object_id.expect("重包新行缺 object_id");
    let new_root = RootSecretV1::new(
        fixture.b.vault_id,
        2,
        fixture.b.identity.current_root.clone(),
    );
    let old_root = RootSecretV1::new(fixture.b.vault_id, 1, {
        let state = IdentityRotationStateV1::load_current(&fixture.b.vault, &fixture.b.identity)
            .expect("读 B 旁路状态失败")
            .expect("B 必须有旁路状态");
        state.root_for_epoch(1).expect("B 必须保留 E1 根").clone()
    });
    assert!(
        decrypt_envelope(
            &old_root,
            ObjectType::Update,
            &new_object,
            &new_parts[0].envelope,
            &[],
        )
        .is_err(),
        "旧 epoch 根竟然解开了重包后的新 part"
    );
    assert!(
        decrypt_envelope(
            &new_root,
            ObjectType::Update,
            &new_object,
            &new_parts[0].envelope,
            &[],
        )
        .is_ok(),
        "新 epoch 根必须能解开重包后的新 part"
    );

    // 新 epoch 推送（B）→ A 可见。
    DevLocal::sync_once(&relay, &fixture.b.vault, &fixture.b.identity).expect("B 重包后 sync 失败");
    DevLocal::sync_once(&relay, &fixture.a.vault, &fixture.a.identity).expect("A 拉取失败");
    let a_text = body_text(&fixture.a.vault, &doc_id, a_peer);
    assert!(
        a_text.contains("B 轮换前离线写的一行"),
        "A 必须看到 B 用新 epoch 推送的离线编辑，实际 {a_text:?}"
    );
    let b_text = body_text(&fixture.b.vault, &doc_id, b_peer);
    assert_eq!(b_text, a_text, "B 的正文必须与 A 收敛");
}

// ---------------------------------------------------------------------------
// 3) A 轮换前的未发送 outbox：前置 sync 推平（真实语义）+ 轮换后不丢、B apply 可见
// ---------------------------------------------------------------------------

#[test]
fn a_pending_outbox_is_flushed_before_freeze_and_visible_after_apply() {
    let mut fixture = establish("a-pending");
    let relay = fixture.relay.base_url().to_owned();
    let doc_id = fixture.note.doc_id.clone();
    let a_peer = fixture.a.identity.loro_peer_id();
    let b_peer = fixture.b.identity.loro_peer_id();

    // A 只写本地、不 sync（旧 epoch 的 pending outbox）。
    append_body(
        &fixture.a.vault,
        &doc_id,
        a_peer,
        1,
        "A 轮换前未发送的一行\n",
    );
    assert_eq!(
        outbox_messages(&fixture.a.vault).len(),
        1,
        "A 必须有一条未发送的本地 outbox"
    );

    let rotation = rotate_empty_revoke(&mut fixture);
    assert_eq!(rotation.to_epoch, 2);
    // rotate 的第一步是 `sync_once`：这条 pending outbox 在冻结前就被推平并 durable ACK
    // （候选基线的覆盖证明只认 durable 事实），因此阶段 J 没有旧行可重包。
    assert!(
        outbox_messages(&fixture.a.vault).is_empty(),
        "rotate 的前置同步必须把 pending outbox 推平（本任务顺序）"
    );
    assert_eq!(rotation.repack.repacked, 0);
    assert!(!repack_required(&fixture.a.vault).expect("读屏障失败"));

    // B apply 后必须看到 A 轮换前的这段文字（基线已覆盖，不靠内存）。
    let applied = apply_b(&mut fixture);
    assert_b_promoted(&fixture, &applied, 2);
    let b_text = body_text(&fixture.b.vault, &doc_id, b_peer);
    assert!(
        b_text.contains("A 轮换前未发送的一行"),
        "B apply 后必须看到 A 轮换前的本地编辑，实际 {b_text:?}"
    );
    // 双方继续同步不丢。
    DevLocal::sync_once(&relay, &fixture.a.vault, &fixture.a.identity).expect("A 轮换后 sync 失败");
    DevLocal::sync_once(&relay, &fixture.b.vault, &fixture.b.identity).expect("B 轮换后 sync 失败");
    let a_text = body_text(&fixture.a.vault, &doc_id, a_peer);
    assert!(a_text.contains("A 轮换前未发送的一行"));
}

// ---------------------------------------------------------------------------
// 4a) 重复 apply 幂等：第二次是 AlreadyCurrent，不重复推广、不动水位/身份
// ---------------------------------------------------------------------------

#[test]
fn apply_is_idempotent_after_promotion() {
    let mut fixture = establish("apply-idempotent");
    rotate_empty_revoke(&mut fixture);
    apply_b(&mut fixture);

    let epoch_before = fixture.b.identity.current_epoch;
    let root_before = fixture.b.identity.current_root.clone();
    let control_before = fixture.b.identity.control_rev;
    let cursor_before = sync_cursor_state(&fixture.b.vault).expect("读游标失败");
    let identity_sha_before = file_sha256(&fixture.b_identity_path);
    let meta_before = fixture
        .b
        .vault
        .get_meta(IDENTITY_ROTATION_STATE_META)
        .expect("读旁路 meta 失败");

    let second = apply_b(&mut fixture);
    assert_eq!(second, ApplyOutcome::AlreadyCurrent { epoch: 2 });
    assert_eq!(fixture.b.identity.current_epoch, epoch_before);
    assert!(fixture.b.identity.current_root.ct_eq(&root_before));
    assert_eq!(fixture.b.identity.control_rev, control_before);
    assert_eq!(
        sync_cursor_state(&fixture.b.vault).expect("读游标失败"),
        cursor_before,
        "幂等 apply 不得移动水位"
    );
    assert_eq!(
        file_sha256(&fixture.b_identity_path),
        identity_sha_before,
        "幂等 apply 不得改写身份文件"
    );
    assert_eq!(
        fixture
            .b
            .vault
            .get_meta(IDENTITY_ROTATION_STATE_META)
            .expect("读旁路 meta 失败"),
        meta_before,
        "幂等 apply 不得改写旁路状态"
    );
}

// ---------------------------------------------------------------------------
// 4b) B 不在 grant（A rotate 时 revoke B）→ B apply 明确 NotGranted；
//     且 status-only 吊销目标进入 signed revoke[]（可复算 + relay revoked_count）
// ---------------------------------------------------------------------------

#[test]
fn revoked_device_is_not_granted_and_signed_revoke_is_recomputable() {
    let mut fixture = establish("revoked");
    let relay = fixture.relay.base_url().to_owned();
    let b_device = fixture.b.identity.device_id;
    let b_identity_sha = file_sha256(&fixture.b_identity_path);
    let b_root_before = fixture.b.identity.current_root.clone();

    let rotation = {
        let relay = relay.clone();
        DevLocal::rotate(
            &relay,
            &fixture.a.vault,
            &fixture.a_identity_path,
            &mut fixture.a.identity,
            &[b_device],
            None,
        )
        .expect("A rotate（revoke B）失败")
    };
    assert_eq!(rotation.to_epoch, 2);
    assert_eq!(rotation.revoked, vec![b_device]);
    assert_eq!(
        rotation.grants, 1,
        "被吊销的 B 不得进入 grants，只剩 A 自己"
    );

    // 签名事实：canonical begin body 能用公开 API 复算（含 revoke[]），digest 必须一致。
    let begin_body = menghunote_sync::rotation::begin_body(
        rotation.from_epoch,
        rotation.expected_control_rev,
        &rotation.request_id,
        &[b_device],
    )
    .expect("重建 begin body 失败");
    let recomputed: [u8; 32] = {
        use sha2::Digest as _;
        sha2::Sha256::digest(&begin_body).into()
    };
    assert_eq!(
        recomputed, rotation.begin_body_digest,
        "signed begin body 必须逐字节包含 revoke[]"
    );
    // relay 真的消费了这条 signed revoke：rotation view 的 revoked_count=1。
    let mut client = test_client(
        &relay,
        &fixture.a.vault_id,
        fixture.a.identity.device_id,
        &fixture.a.identity.ed25519_seed,
        fixture.a.identity.pinned_manager,
    );
    let view = client
        .rotation(rotation.rotation_id)
        .expect("读 rotation view 失败");
    assert_eq!(view.state, "activated");
    assert_eq!(view.revoked_count, 1);
    assert_eq!(view.to_epoch, 2);
    // 成员视图：B 已经是 revoked（status-only 吊销，仍然保留在签名控制链里）。
    let members =
        DevLocal::members(&relay, &fixture.a.vault, &fixture.a.identity).expect("读成员视图失败");
    let b_member = members
        .iter()
        .find(|member| member.device_id == b_device)
        .expect("成员视图必须含 B");
    assert_eq!(b_member.status, MemberStatus::Revoked);

    // B apply：明确 NotGranted；身份/旧 epoch/outbox 全部不变（不回退、不半推广）。
    let outcome = {
        let relay = relay.clone();
        DevLocal::apply_latest_epoch(
            &relay,
            &fixture.b.vault,
            &fixture.b_identity_path,
            &mut fixture.b.identity,
        )
        .expect("B apply 不应把被吊销当成 IO 错误")
    };
    assert_eq!(
        outcome,
        ApplyOutcome::NotGranted {
            device_id: b_device,
            local_epoch: 1,
        }
    );
    assert_eq!(fixture.b.identity.current_epoch, 1);
    assert!(fixture.b.identity.current_root.ct_eq(&b_root_before));
    assert_eq!(
        file_sha256(&fixture.b_identity_path),
        b_identity_sha,
        "被吊销设备不得改写身份文件"
    );
    assert!(
        IdentityRotationStateV1::load_current(&fixture.b.vault, &fixture.b.identity)
            .expect("读旁路状态失败")
            .is_none(),
        "被吊销设备不得留下轮换旁路状态"
    );
    assert!(
        menghunote_sync::dev_local_rotation::apply_phase(&fixture.b.vault)
            .expect("读 apply phase 失败")
            .is_none()
    );
}

// ---------------------------------------------------------------------------
// 5a) rotate 在 activate 之后失败（推广写临时文件失败）：身份不动，journal 保留；
//     用本机 grant apply 追平 + 真实旧 epoch outbox 重包（A 侧阶段 J）
// ---------------------------------------------------------------------------

#[test]
fn rotate_failure_after_activation_keeps_identity_and_apply_recovers() {
    let mut fixture = establish("rotate-failure");
    let relay = fixture.relay.base_url().to_owned();
    let doc_id = fixture.note.doc_id.clone();
    let a_peer = fixture.a.identity.loro_peer_id();
    let a_device = fixture.a.identity.device_id;
    let a_epoch_before = fixture.a.identity.current_epoch;
    let a_root_before = fixture.a.identity.current_root.clone();
    let a_identity_sha = file_sha256(&fixture.a_identity_path);

    // 让原子推广的临时路径不可写：`<identity>.promote.tmp` 先被建成目录。
    // 这是真实的 promotion 失败点（rename 之前写临时文件 EISDIR），可确定性复现、无需 fault hook。
    let promote_tmp = fixture.a_identity_path.with_extension("promote.tmp");
    std::fs::create_dir_all(&promote_tmp).expect("创建阻塞目录失败");

    let error = DevLocal::rotate(
        &relay,
        &fixture.a.vault,
        &fixture.a_identity_path,
        &mut fixture.a.identity,
        &[],
        None,
    )
    .expect_err("推广失败时 rotate 必须明确报错");
    assert!(
        matches!(error, SyncError::Io(_)),
        "失败必须来自本地推广 IO，实际 {error:?}"
    );

    // 不得半推广：内存/磁盘身份、旁路状态、pin 全部保持旧 epoch。
    assert_eq!(fixture.a.identity.current_epoch, a_epoch_before);
    assert!(fixture.a.identity.current_root.ct_eq(&a_root_before));
    assert!(fixture.a.identity.pending_rotation.is_none());
    assert_eq!(file_sha256(&fixture.a_identity_path), a_identity_sha);
    assert!(
        IdentityRotationStateV1::load_current(&fixture.a.vault, &fixture.a.identity)
            .expect("读旁路状态失败")
            .is_none(),
        "promotion 失败不得写新 epoch 的旁路状态"
    );
    assert!(
        AuthenticatedPinV1::load_current(&fixture.a.vault, &fixture.a.identity)
            .expect("读认证 pin 失败")
            .is_none()
    );
    assert!(!repack_required(&fixture.a.vault).expect("读屏障失败"));
    // journal 必须保留（不是静默清除）：失败时它是唯一的恢复线索。
    // 用**新开的连接**读（模拟进程重启后的 durable 事实，不依赖进程内缓存）。
    let fresh_a = Vault::open(&fixture.a_path).expect("重开 A 库失败");
    let journal = RotationJournal::load(&fresh_a)
        .expect("读 journal 失败")
        .expect("activate 之后的失败必须保留 journal");
    assert_eq!(journal.phase, RotationPhase::Activated);
    assert_eq!(journal.to_epoch, 2);
    assert_eq!(
        journal.new_manager_rev, 2,
        "journal 必须记下本次管理 rev（恢复对账用）"
    );
    drop(fresh_a);
    // relay 侧其实已经激活（这是真实的"激活后推广失败"窗口）。
    let mut client = test_client(
        &relay,
        &fixture.a.vault_id,
        a_device,
        &fixture.a.identity.ed25519_seed,
        fixture.a.identity.pinned_manager,
    );
    assert_eq!(client.state().expect("/state 失败").epoch, 2);

    // 旧 epoch 的未发送 outbox（恢复后必须被重包，证明 A 侧阶段 J 真跑）。
    append_body(
        &fixture.a.vault,
        &doc_id,
        a_peer,
        1,
        "A 恢复前离线写的一行\n",
    );
    let old_rows = outbox_messages(&fixture.a.vault);
    assert_eq!(old_rows.len(), 1);
    assert_eq!(old_rows[0].key_epoch, 1);

    // 解除阻塞 → 用**本机 grant** apply 追平（A 也是 grant 收件人）。
    std::fs::remove_dir(&promote_tmp).expect("移除阻塞目录失败");
    let applied = DevLocal::apply_latest_epoch(
        &relay,
        &fixture.a.vault,
        &fixture.a_identity_path,
        &mut fixture.a.identity,
    )
    .expect("A apply_latest_epoch（恢复）失败");
    let ApplyOutcome::Applied {
        to_epoch, repack, ..
    } = &applied
    else {
        panic!("A 恢复 apply 未成功：{applied:?}");
    };
    assert_eq!(*to_epoch, 2);
    assert_eq!(repack.repacked, 1, "A 的旧 epoch outbox 必须被重包");
    assert_eq!(repack.failed, 0);
    assert_eq!(repack.conflicts, 0);
    assert_eq!(fixture.a.identity.current_epoch, 2);
    let disk = load_identity(&fixture.a_identity_path, &fixture.a.vault_id, &a_device);
    assert_eq!(disk.current_epoch, 2);
    assert!(disk.current_root.ct_eq(&fixture.a.identity.current_root));
    let state = IdentityRotationStateV1::load_current(&fixture.a.vault, &fixture.a.identity)
        .expect("读 A 旁路状态失败")
        .expect("恢复 apply 后必须有旁路状态");
    assert_eq!(state.current_epoch, 2);
    assert!(state.root_for_epoch(1).is_some());
    assert!(!repack_required(&fixture.a.vault).expect("读屏障失败"));
    let new_rows = outbox_messages(&fixture.a.vault);
    assert_eq!(new_rows.len(), 1);
    assert_eq!(new_rows[0].key_epoch, 2, "重包后必须用新 epoch");

    // 上一次失败留下的 journal 由既有能力清除（不静默跳步），之后 A 能正常同步。
    RotationJournal::clear(&fixture.a.vault).expect("清 journal 失败");
    assert!(RotationJournal::load(&fixture.a.vault)
        .expect("读 journal 失败")
        .is_none());
    DevLocal::sync_once(&relay, &fixture.a.vault, &fixture.a.identity).expect("A 恢复后 sync 失败");
    assert!(outbox_messages(&fixture.a.vault).is_empty());

    // B 仍离线/旧 epoch；apply 后按 E2 基线合并，再从 S 之后拉取 A 的增量（含恢复前那段）。
    let applied_b = apply_b(&mut fixture);
    assert_b_promoted(&fixture, &applied_b, 2);
    DevLocal::sync_once(&relay, &fixture.b.vault, &fixture.b.identity)
        .expect("B apply 后 sync 失败");
    let b_text = body_text(&fixture.b.vault, &doc_id, fixture.b.identity.loro_peer_id());
    assert!(b_text.contains("A 恢复前离线写的一行"), "实际 {b_text:?}");
    assert!(b_text.contains("A 写的第一行"));
}

// ---------------------------------------------------------------------------
// 5b) apply 中断（merge 已提交、推广失败）→ 重试不半推广，最终追平
// ---------------------------------------------------------------------------

#[test]
fn apply_interruption_retries_without_half_promotion() {
    let mut fixture = establish("apply-interrupt");
    let relay = fixture.relay.base_url().to_owned();
    let b_device = fixture.b.identity.device_id;

    // A 连做两轮：E1 -> E2（B apply），E2 -> E3（B 的 apply 要被中断）。
    rotate_empty_revoke(&mut fixture);
    apply_b(&mut fixture);
    let second_rotation = rotate_empty_revoke(&mut fixture);
    assert_eq!(second_rotation.to_epoch, 3);

    let b_identity_before = file_sha256(&fixture.b_identity_path);
    let cursor_before = sync_cursor_state(&fixture.b.vault).expect("读游标失败");
    let promote_tmp = fixture.b_identity_path.with_extension("promote.tmp");
    std::fs::create_dir_all(&promote_tmp).expect("创建阻塞目录失败");

    let interrupted = {
        let relay = relay.clone();
        DevLocal::apply_latest_epoch(
            &relay,
            &fixture.b.vault,
            &fixture.b_identity_path,
            &mut fixture.b.identity,
        )
        .expect("apply 内部失败必须变成 Paused，而不是 panic/半推广")
    };
    let ApplyOutcome::Paused { reason } = &interrupted else {
        panic!("apply 中断必须是 Paused，实际 {interrupted:?}");
    };
    assert!(
        reason.contains("IO"),
        "Paused 原因必须指向推广 IO：{reason}"
    );
    // 身份（内存 + 磁盘）仍是 E2，不半推广。
    assert_eq!(fixture.b.identity.current_epoch, 2);
    assert_eq!(file_sha256(&fixture.b_identity_path), b_identity_before);
    let disk = load_identity(&fixture.b_identity_path, &fixture.b.vault_id, &b_device);
    assert_eq!(disk.current_epoch, 2);
    // apply phase 停在 paused（冻结屏障），receipt 已 durable，供重试收尾。
    // 用新开的连接读：模拟进程重启后只能依赖 durable 事实。
    let fresh_b = Vault::open(&fixture.b_path).expect("重开 B 库失败");
    assert_eq!(
        menghunote_sync::dev_local_rotation::apply_phase(&fresh_b).expect("读 apply phase 失败"),
        Some(RotationApplyPhase::Paused)
    );
    // 旁路状态仍是 E2（没有 E3 的旁路事实）。
    let state = IdentityRotationStateV1::load_current(&fresh_b, &fixture.b.identity)
        .expect("读旁路状态失败")
        .expect("E2 推广后必须有旁路状态");
    assert_eq!(state.current_epoch, 2);
    // 这次中断发生在 promotion 写临时文件之前：merge + typed receipt 已 durable，
    // 但 repack 屏障还没写（promotion 的写入序列里屏障在 rename 之前、临时文件之后）。
    // 也就是说"引擎继续冻结旧写"靠的是 `apply_phase=paused`，不是 repack 屏障。
    assert!(
        !repack_required(&fresh_b).expect("读屏障失败"),
        "promotion 未到达写屏障的阶段，repack 屏障不应被置位"
    );
    let receipt = menghunote_sync::rotation_journal::RotationApplyReceipt::load(&fresh_b)
        .expect("读 apply receipt 失败")
        .expect("merge 完成后必须留下 durable receipt（重试收尾用的唯一认证线索）");
    assert_eq!(receipt.from_epoch(), 2);
    assert_eq!(receipt.epoch(), 3);
    assert_eq!(receipt.new_manager_rev(), second_rotation.manager_rev);
    // merge 会把水位推进到基线 S（这是判据，允许前进）；重试必须停在同一 S，不重复/跳段。
    let cursor_interrupted = sync_cursor_state(&fresh_b).expect("读游标失败");
    assert!(
        cursor_interrupted.integrated_seq >= cursor_before.integrated_seq,
        "水位不得倒退：{} -> {}",
        cursor_before.integrated_seq,
        cursor_interrupted.integrated_seq
    );
    assert_eq!(cursor_interrupted.integrated_seq, receipt.baseline_s());
    drop(fresh_b);

    // 解除阻塞后重试：不半推广、不重复身份写入，最终到 E3。
    std::fs::remove_dir(&promote_tmp).expect("移除阻塞目录失败");
    let retried = apply_b(&mut fixture);
    assert_b_promoted(&fixture, &retried, 3);
    let fresh_b = Vault::open(&fixture.b_path).expect("重开 B 库失败");
    assert_eq!(
        sync_cursor_state(&fresh_b)
            .expect("读游标失败")
            .integrated_seq,
        cursor_interrupted.integrated_seq,
        "重试必须停在同一基线 S（幂等）"
    );
    assert_eq!(
        menghunote_sync::dev_local_rotation::apply_phase(&fresh_b).expect("读 apply phase 失败"),
        Some(RotationApplyPhase::Applied),
        "重试成功后 apply phase 必须转 applied（引擎解冻）"
    );
    drop(fresh_b);
    assert!(
        menghunote_sync::rotation_journal::RotationApplyReceipt::load(&fixture.b.vault)
            .expect("读 apply receipt 失败")
            .is_some(),
        "成功 apply 后 receipt 仍应 durable（供 same-epoch 收尾）"
    );
    let state = IdentityRotationStateV1::load_current(&fixture.b.vault, &fixture.b.identity)
        .expect("读旁路状态失败")
        .expect("重试后必须有旁路状态");
    assert_eq!(state.current_epoch, 3);
    assert!(state.root_for_epoch(1).is_some());
    assert!(state.root_for_epoch(2).is_some());
    let disk = load_identity(&fixture.b_identity_path, &fixture.b.vault_id, &b_device);
    assert_eq!(disk.current_epoch, 3);
    assert!(disk.current_root.ct_eq(&fixture.b.identity.current_root));
    assert!(disk.pending_rotation.is_none());

    // 第三次 apply 幂等。
    let third = apply_b(&mut fixture);
    assert_eq!(third, ApplyOutcome::AlreadyCurrent { epoch: 3 });
}

// ---------------------------------------------------------------------------
// 5) 恢复码（ADR 0009 切片 6a）：Some = 候选 epoch 的 recovery 槽能用同一恢复码解开，
//    轮换后新设备可 Join；None = 一次性随机种子，新设备 Join 必须明确失败。
// ---------------------------------------------------------------------------

#[test]
fn recovery_code_decides_whether_new_devices_can_join_after_rotation() {
    let mut fixture = establish("recovery-code");
    let relay = fixture.relay.base_url().to_owned();
    let vault_id = fixture.a.vault_id;
    let code = fixture.a.recovery_code.clone();

    // 1) Some(当前恢复码)：轮换到 E2，候选 recovery 槽仍用同一恢复码封给新根。
    let rotation = {
        let relay = relay.clone();
        DevLocal::rotate(
            &relay,
            &fixture.a.vault,
            &fixture.a_identity_path,
            &mut fixture.a.identity,
            &[],
            Some(code.as_str()),
        )
        .expect("A rotate（带恢复码）失败")
    };
    assert_eq!(rotation.to_epoch, 2);
    assert_eq!(rotation.from_epoch, 1);

    // B apply 到 E2（管理设备之外没人能解出候选根的路径也顺带覆盖）。
    let applied = apply_b(&mut fixture);
    assert!(
        matches!(
            applied,
            ApplyOutcome::Applied {
                from_epoch: 1,
                to_epoch: 2,
                ..
            }
        ),
        "B 必须从 E1 apply 到 E2：{applied:?}"
    );

    // C 只用 vault_id + 恢复码 Join：必须落在 E2，且解出的根就是轮换后的新根。
    let c_path = fixture._dir.path().join("c").join("vault.sqlite3");
    let c = DevLocal::join(&relay, &vault_id, &code, &c_path)
        .expect("E2 之后新设备必须能用恢复码 Join");
    assert_eq!(c.identity.current_epoch, 2, "Join 必须解出轮换后的新根");
    assert!(
        c.identity
            .current_root
            .ct_eq(&fixture.a.identity.current_root),
        "C 与 A 的 E2 根必须逐字节相同"
    );

    // 2) None：再轮换到 E3，候选 recovery 槽用一次性随机种子（没人持有）。
    let rotation = {
        let relay = relay.clone();
        DevLocal::rotate(
            &relay,
            &fixture.a.vault,
            &fixture.a_identity_path,
            &mut fixture.a.identity,
            &[],
            None,
        )
        .expect("A rotate（不带恢复码）失败")
    };
    assert_eq!(rotation.to_epoch, 3);

    // D 用同一恢复码 Join：必须明确失败（不得悄悄绑到旧根/旧 epoch）。
    let d_path = fixture._dir.path().join("d").join("vault.sqlite3");
    let error = match DevLocal::join(&relay, &vault_id, &code, &d_path) {
        Ok(_) => panic!("E3 的恢复槽用随机种子封装，恢复码必须解不开（不得悄悄绑到旧根）"),
        Err(error) => error,
    };
    let message = error.to_string();
    assert!(
        message.contains("MNE") || message.contains("恢复槽") || message.contains("KEY_SLOT"),
        "失败必须点明恢复槽解不开：{message}"
    );
    assert!(
        !d_path.exists(),
        "Join 失败不得留下半初始化库：{}",
        d_path.display()
    );
}

// ---------------------------------------------------------------------------
// task-121：轮换期写入的 ASSET 可被普通 checkpoint 的 reservation 幂等复用
// ---------------------------------------------------------------------------

/// 轮换把资产重加密进新 epoch 时，ASSET 对象挂在 **rotation reservation** 上；
/// 激活之后普通 checkpoint 用 **checkpoint reservation** 重传同一 `object_id`/同字节，
/// 必须幂等成功（200 duplicate），不再 `409 ID_CONFLICT`；新 checkpoint 仍引用同一对象。
#[test]
fn rotated_asset_is_reused_by_an_ordinary_checkpoint() {
    let mut fixture = establish("asset-rotation-reuse");
    let relay = fixture.relay.base_url().to_owned();
    let note_id = note_id_for_body(&fixture.a.vault, &fixture.note.doc_id);
    let data = b"rotation reencrypted attachment bytes".to_vec();
    let hash = insert_referenced_asset(&fixture.a.vault, &note_id, &data);

    // 1) 轮换前普通同步：资产以普通预算上传并 complete，从而进入 freeze 集合。
    let before = DevLocal::sync_once(&relay, &fixture.a.vault, &fixture.a.identity)
        .expect("A 轮换前同步失败");
    assert_eq!(before.assets.uploaded, 1, "轮换前资产必须已上传");
    assert_eq!(before.assets.failed, 0);
    assert!(
        asset_marker_count(&fixture.a.vault) >= 1,
        "普通同步成功后必须落 durable 已上传标记"
    );
    // ASSET 事件不写 inbox，只推进 received；再来一条正文 update 并同步，才能让
    // integrated 连续前缀越过它（rotate 的 integrate_to_S 要求 integrated 到 S）。
    append_body(
        &fixture.a.vault,
        &fixture.note.doc_id,
        fixture.a.identity.loro_peer_id(),
        1,
        "A 轮换前的普通同步行\n",
    );
    DevLocal::sync_once(&relay, &fixture.a.vault, &fixture.a.identity)
        .expect("A 资产后的正文同步失败");

    // 2) 轮换：旧根解密 → 新根重加密 → 候选基线用 rotation reservation 落库。
    let rotation = rotate_empty_revoke(&mut fixture);
    assert_eq!(rotation.to_epoch, 2);

    // 3) B apply：拿到新 epoch 的资产字节；B 上没有 "已上传" 标记。
    let applied = apply_b(&mut fixture);
    assert!(matches!(applied, ApplyOutcome::Applied { to_epoch: 2, .. }));
    assert_eq!(
        asset_marker_count(&fixture.b.vault),
        0,
        "B 通过基线 apply 拿到字节，但不应有 sync.asset_uploaded 标记"
    );
    assert_eq!(
        asset_bytes(&fixture.b.vault, hash).expect("读 B 资产"),
        Some(data.clone()),
        "B 必须通过轮换基线拿到新 epoch 的资产字节"
    );

    let new_root = RootSecretV1::new(
        fixture.a.vault_id,
        2,
        fixture.a.identity.current_root.clone(),
    );
    let object_id = asset_object_id(&new_root, &hash, data.len() as u64, "text/plain")
        .expect("asset object id");

    // 4) 清掉 A 的 durable 标记，强制普通 checkpoint 真正重传该资产的确定性密文
    //    （等价于另一台设备/标记丢失的客户端）。
    clear_asset_markers(&fixture.a.vault);
    assert_eq!(asset_marker_count(&fixture.a.vault), 0);

    let published = {
        let mut client = test_client(
            &relay,
            &fixture.a.vault_id,
            fixture.a.identity.device_id,
            &fixture.a.identity.ed25519_seed,
            fixture.a.identity.pinned_manager,
        );
        let mut writer = CheckpointWriter::new(
            &fixture.a.vault,
            &mut client,
            fixture.a.vault_id,
            fixture.a.identity.device_id,
            DeviceSigningKey::from_seed(fixture.a.identity.ed25519_seed.clone()),
            new_root.clone(),
            fixture.a.identity.loro_peer_id(),
        );
        writer
            .create_checkpoint(&StorageAssets)
            .expect("轮换后普通 checkpoint 重传同一资产不得 409")
    };
    assert!(published.validated);
    assert_eq!(published.epoch, 2);
    assert_eq!(
        asset_marker_count(&fixture.a.vault),
        1,
        "checkpoint 复用/重传成功后必须重新落 durable 标记"
    );
    let asset = published
        .assets
        .iter()
        .find(|asset| asset.real_sha256 == hash)
        .expect("新 checkpoint 必须引用该资产");
    assert_eq!(
        asset.object.object_id, object_id,
        "新 checkpoint 必须引用轮换期写入的同一个 object_id"
    );

    // 5) wire 语义直测：另取一份 checkpoint 的维护 reservation，用它对同一
    //    object_id/同字节再做 PUT —— 必须成功；不同字节仍然冲突。
    let mut client = test_client(
        &relay,
        &fixture.a.vault_id,
        fixture.a.identity.device_id,
        &fixture.a.identity.ed25519_seed,
        fixture.a.identity.pinned_manager,
    );
    let lease = client
        .create_checkpoint(1024 * 1024, None)
        .expect("申请新的 checkpoint reservation 失败");
    let part = client
        .part(&object_id, 0)
        .expect("读回轮换期写入的资产分片失败");
    client
        .put_part_with_reservation(&object_id, 0, &part, Some(&lease.reservation_id))
        .expect("同字节跨 reservation 重放必须成功（不得 409 ID_CONFLICT）");
    let mut tampered = part.clone();
    let last = tampered.len() - 1;
    tampered[last] ^= 0xFF;
    let error = client
        .put_part_with_reservation(&object_id, 0, &tampered, Some(&lease.reservation_id))
        .expect_err("不同字节必须冲突");
    assert!(
        matches!(&error, SyncError::RelayApi { code, .. } if code == "PART_CONFLICT"),
        "期望 PART_CONFLICT，实际 {error:?}"
    );
}
