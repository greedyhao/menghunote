//! 双设备轮换 e2e 的共享步骤。
//!
//! 本文件通过 `#[path = "e2e_common.rs"] mod e2e_common;` 被
//! `e2e_rotation_two_devices` 复用；cargo 也会把 `examples/*.rs` 当独立 example 编译，
//! 所以这里保留一个只打印提示的 `main`（真正的入口在双设备示例里）。

#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use menghunote_storage::{
    append_updates, append_updates_with_sync_intents, create_body_doc, enqueue_sync_intent,
    ensure_structure_row, insert_asset, load_body_doc, load_head, BodyDoc, SyncIntent,
    UpdateRecord, Value, Vault,
};
use menghunote_sync::api_version::{RelayApi, RelayApiVersion};
use menghunote_sync::bootstrap::{self, BootstrapReport};
use menghunote_sync::checkpoint::{
    CheckpointReport, CheckpointWriter, EmptyAssets, SlotSecrets, StorageAssets,
};
use menghunote_sync::device::{DeviceHpkeKey, DeviceSigningKey, ManagementKey};
use menghunote_sync::engine::{EngineIdentity, SyncEngine, RELAY_CURSOR_META};
use menghunote_sync::genesis;
use menghunote_sync::http_relay::{HttpRelayClient, HttpRelayConfig, StdTcpTransport};
use menghunote_sync::identity::LocalIdentityV1;
use menghunote_sync::ids::{self, PeerId};
use menghunote_sync::keys::RootSecretV1;
use menghunote_sync::random::{array, OsRandom};
use menghunote_sync::recovery::RecoverySeed;
use menghunote_sync::recovery_enroll;
use menghunote_sync::relay::{PauseReason, RelayClient};
use menghunote_sync::slot::KeySlotV1;
use menghunote_sync::upload::{encrypt_local_parts, fetch_object};
use menghunote_sync::{DeviceId, SyncError, VaultId};
use menghunote_sync_proto::{Envelope, EnvelopeHeader, ObjectType};
use sha2::{Digest, Sha256};

/// 示例/脚本统一的错误类型。
pub type AnyResult<T> = Result<T, Box<dyn std::error::Error>>;

/// 客户端“当前时间”的测试覆盖（0 = 关闭）。
///
/// task-120：relay 侧换成 `FakeClock` 后，客户端签名/槽材料里带的 `now` 必须跟着 relay
/// 时钟走，否则跳 90 天后会出现"客户端时间早于 relay TTL 窗口"的假失败。这里**只**
/// 给 examples/测试用，生产代码永远自己取系统时间。
static NOW_OVERRIDE: AtomicU64 = AtomicU64::new(0);

/// 设置/清除“当前时间”覆盖（进程内 relay 推进假时钟后由 e2e 显式同步）。
pub fn set_now_override(now: Option<u64>) {
    NOW_OVERRIDE.store(now.unwrap_or(0), Ordering::SeqCst);
}

/// 当前 Unix 秒（真跑时取系统时间；受 [`set_now_override`] 影响）。
pub fn now_secs() -> u64 {
    let overridden = NOW_OVERRIDE.load(Ordering::SeqCst);
    if overridden != 0 {
        return overridden;
    }
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

/// 文件内容的 SHA256（身份文件"逐字节未变"的证据）。
pub fn file_sha256(path: &Path) -> AnyResult<[u8; 32]> {
    Ok(Sha256::digest(fs::read(path)?).into())
}

/// `sync_inbox` 行数（B 离线期间不该增长）。
pub fn inbox_count(vault: &Vault) -> AnyResult<u64> {
    let rows = vault.query("SELECT COUNT(*) FROM sync_inbox", &[])?;
    match rows.first().and_then(|row| row.first()) {
        Some(Value::Integer(value)) => Ok(u64::try_from(*value).unwrap_or(0)),
        _ => Err("sync_inbox COUNT(*) 不是整数".into()),
    }
}

fn peer_id_bytes(peer_id: u64) -> [u8; 16] {
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&peer_id.to_be_bytes());
    bytes
}

/// 从 `identity.meta` 读 `(vault_id, device_id)`。
pub fn read_identity_meta(dir: &Path) -> AnyResult<(VaultId, DeviceId)> {
    let meta = fs::read_to_string(dir.join("identity.meta"))?;
    let mut parts = meta.split_whitespace();
    let vault_id = ids::parse_hex::<16>(parts.next().ok_or("identity.meta 缺 vault_id")?)?;
    let device_id = ids::parse_hex::<16>(parts.next().ok_or("identity.meta 缺 device_id")?)?;
    Ok((vault_id, device_id))
}

/// 用 DPAPI 保护并写回本地身份。
pub fn write_identity(dir: &Path, identity: &LocalIdentityV1) -> AnyResult<()> {
    fs::create_dir_all(dir)?;
    fs::write(dir.join("identity.bin"), identity.protect()?)?;
    fs::write(
        dir.join("identity.meta"),
        format!(
            "{} {}\n",
            ids::hex_of(&identity.vault_id),
            ids::hex_of(&identity.device_id)
        ),
    )?;
    Ok(())
}

/// 从目录里的 DPAPI BLOB 重新加载身份（推广后复核用）。
pub fn load_identity(dir: &Path) -> AnyResult<LocalIdentityV1> {
    let (vault_id, device_id) = read_identity_meta(dir)?;
    let blob = fs::read(dir.join("identity.bin"))?;
    Ok(LocalIdentityV1::unprotect(&blob, &vault_id, &device_id)?)
}

/// 构造设备 realm 的 HTTP 客户端。
pub fn make_client(
    relay: &str,
    vault_id: VaultId,
    device_id: DeviceId,
    signing_key: DeviceSigningKey,
    manager_pub: Option<[u8; 32]>,
) -> AnyResult<HttpRelayClient<StdTcpTransport>> {
    Ok(HttpRelayClient::new(
        StdTcpTransport::new_local_dev(relay)?,
        HttpRelayConfig {
            base_url: relay.to_owned(),
            api_version: RelayApiVersion::V1,
            vault_id,
            device_id,
            manager_pub,
        },
        signing_key,
    ))
}

/// A 端 genesis 材料（含落盘身份与恢复码）。
pub struct GenesisDevice {
    pub dir: PathBuf,
    pub vault_id: VaultId,
    pub device_id: DeviceId,
    pub peer_id: u64,
    pub signing_key: DeviceSigningKey,
    pub hpke_key: DeviceHpkeKey,
    pub root: RootSecretV1,
    pub manager: ManagementKey,
    pub recovery_seed: RecoverySeed,
    pub recovery_code: String,
    pub password_slot: KeySlotV1,
    pub recovery_slot: KeySlotV1,
    pub identity: LocalIdentityV1,
    pub initial_cursor: Option<String>,
}

/// A 端：对真实 dev_server 走 genesis，并把身份/恢复码写到 `dir`。
pub fn run_genesis(
    relay: &str,
    setup_secret: &str,
    dir: &Path,
    password: &str,
) -> AnyResult<GenesisDevice> {
    // genesis 之前显式初始化 Linux 本地身份主钥（Windows 为 no-op）。重启/load 不初始化。
    menghunote_sync::identity_wrap::initialize_user_key()?;
    fs::create_dir_all(dir)?;
    let mut transport = StdTcpTransport::new_local_dev(relay)?;
    let outcome = genesis::run(
        &mut transport,
        setup_secret,
        password,
        now_secs(),
        &mut OsRandom,
    )?;
    let recovery_code = outcome.materials.recovery_code.as_str().to_owned();
    write_identity(dir, &outcome.identity)?;
    // dev e2e：恢复码落盘仅用于同一脚本里的 B 端，生产不得这样做。
    fs::write(dir.join("recovery-code.txt"), &recovery_code)?;
    fs::write(
        dir.join("slot-password.bin"),
        outcome.materials.password_slot.wrapped_bytes(),
    )?;
    fs::write(
        dir.join("slot-recovery.bin"),
        outcome.materials.recovery_slot.wrapped_bytes(),
    )?;
    println!(
        "[two-device] A genesis vault={} device={} epoch={} control_rev={} recovery_code={}",
        ids::hex_of(&outcome.materials.vault_id),
        ids::hex_of(&outcome.materials.device_id),
        outcome.epoch,
        outcome.control_rev,
        recovery_code
    );
    Ok(GenesisDevice {
        dir: dir.to_path_buf(),
        vault_id: outcome.materials.vault_id,
        device_id: outcome.materials.device_id,
        peer_id: u64::from_be_bytes(
            outcome.materials.peer_id.as_bytes()[..8]
                .try_into()
                .expect("peer_id 前 8 字节"),
        ) & 0x7fff_ffff_ffff_ffff,
        signing_key: outcome.materials.signing_key,
        hpke_key: outcome.materials.hpke_key,
        root: outcome.materials.root,
        manager: outcome.materials.manager,
        recovery_seed: outcome.materials.recovery_seed,
        recovery_code,
        password_slot: outcome.materials.password_slot,
        recovery_slot: outcome.materials.recovery_slot,
        identity: outcome.identity,
        initial_cursor: outcome.initial_cursor,
    })
}

fn commit_with_intent(
    vault: &Vault,
    doc: &BodyDoc,
    message_id: &str,
    object_id: [u8; 32],
    key_epoch: u64,
) -> AnyResult<u64> {
    let head = load_head(vault, doc.doc_id())?;
    let base = head.version_vector()?;
    let update = doc.pending_update(&base)?;
    let intent = SyncIntent {
        message_id: message_id.to_owned(),
        batch_id: format!("batch-{message_id}"),
        key_epoch,
        object_id,
    };
    let state = append_updates_with_sync_intents(
        vault,
        doc.doc_id(),
        head.head_seq,
        &[UpdateRecord::new(intent.batch_id.clone(), update)],
        &[intent],
    )?;
    Ok(state.head_seq)
}

fn uuid_like(value: u64) -> String {
    format!("00000000-0000-4000-8000-{value:012x}")
}

/// A 端：建库结构 + 两条笔记 + 推送 + 资产 + 初始槽 + 两份已发布 checkpoint。
///
/// 返回打开的 A vault；调用方在此之后才能让 B 用恢复码 bootstrap。
pub fn seed_a_vault(relay: &str, a: &GenesisDevice, password: &str) -> AnyResult<Vault> {
    // seed 前置只保证主钥已就绪；后续 identity.protect 走生产封装路径。
    menghunote_sync::identity_wrap::initialize_user_key()?;
    let vault = Vault::open(a.dir.join("vault.sqlite3"))?;
    ensure_structure_row(&vault)?;
    let mut client = make_client(
        relay,
        a.vault_id,
        a.device_id,
        a.signing_key.clone(),
        Some(a.manager.public_key()),
    )?;

    if vault.get_meta(RELAY_CURSOR_META)?.is_none() {
        let cursor = match &a.initial_cursor {
            Some(cursor) => cursor.clone(),
            None => client.initial_cursor()?,
        };
        vault.set_meta(RELAY_CURSOR_META, cursor.as_bytes())?;
        println!("[two-device] A initial cursor={cursor}");
    }

    for (index, doc_id) in ["note-1", "note-2"].iter().enumerate() {
        create_body_doc(&vault, doc_id)?;
        let doc = BodyDoc::create(*doc_id, a.peer_id)?;
        doc.insert_scalars(0, &format!("genesis note {index} from A\n"))?;
        let object_id: [u8; 32] = array(&mut OsRandom)?;
        commit_with_intent(
            &vault,
            &doc,
            &uuid_like(100 + index as u64),
            object_id,
            a.root.key_epoch,
        )?;
    }

    let report = {
        let identity = EngineIdentity {
            device_id: a.device_id,
            signing_key: a.signing_key.clone(),
            peer_id: a.peer_id,
        };
        let mut engine = SyncEngine::new(&vault, &mut client, identity, a.root.clone());
        engine.sync_once()?
    };
    println!(
        "[two-device] A push encrypted={} uploaded={} acked={} pull.events={} integrated={}",
        report.encrypted,
        report.push.uploaded,
        report.push.acked,
        report.pull.events,
        report.integrate.integrated
    );

    let asset_data = b"two-device rotation asset".to_vec();
    let asset_hash: [u8; 32] = Sha256::digest(&asset_data).into();
    insert_asset(
        &vault,
        &asset_hash,
        asset_data.len() as u64,
        Some("text/plain"),
        &asset_data,
    )?;

    let mut writer = CheckpointWriter::new(
        &vault,
        &mut client,
        a.vault_id,
        a.device_id,
        a.signing_key.clone(),
        a.root.clone(),
        a.peer_id,
    );
    let secrets = SlotSecrets {
        password: Some(password),
        recovery_seed: Some(&a.recovery_seed),
    };
    let slots = writer.upload_initial_slots(&a.password_slot, &a.recovery_slot, &secrets)?;
    println!(
        "[two-device] A initial slots password_state={} active_rev={} recovery_state={}",
        slots.password.state, slots.active_password_rev, slots.recovery.state
    );

    let first = writer.create_checkpoint(&StorageAssets)?;
    let second = writer.create_checkpoint(&StorageAssets)?;
    println!(
        "[two-device] A checkpoints primary={} S={} docs={} assets={} fallback={} S={}",
        ids::hex_of(&first.checkpoint_id),
        first.covered_seq,
        first.docs.len(),
        first.assets.len(),
        ids::hex_of(&second.checkpoint_id),
        second.covered_seq
    );
    println!("[two-device] A asset hash={}", ids::hex_of(&asset_hash));
    Ok(vault)
}

/// 读回一个自描述对象（manifest 等）：先从第 0 片拿 header，再按 header 读全对象。
///
/// `HttpRelayClient::checkpoint_manifest` 在 HEAD 上返回的是 manifest 对象 id 而不是
/// manifest 明文；e2e 用这个 helper 直接按 `manifest_oid` 读回真实对象。
pub fn fetch_self_described_object<C: RelayClient>(
    client: &mut C,
    root: &RootSecretV1,
    object_type: ObjectType,
    object_id: &[u8; 32],
) -> AnyResult<(EnvelopeHeader, Vec<u8>)> {
    let first = client.part(object_id, 0)?;
    let envelope = Envelope::decode(&first)?;
    if envelope.header.object_type != object_type || envelope.header.object_id != *object_id {
        return Err("自描述对象 header 不符".into());
    }
    let part_count = envelope.header.part_count;
    let mut digests = Vec::with_capacity(part_count as usize);
    for index in 0..part_count {
        let part = client.part(object_id, index)?;
        digests.push(Sha256::digest(&part).into());
    }
    let digest_input = menghunote_sync_proto::object_digest_input(part_count, &digests)?;
    let digest: [u8; 32] = Sha256::digest(&digest_input).into();
    Ok(fetch_object(
        client,
        root,
        object_type,
        object_id,
        part_count,
        envelope.header.total_plaintext_len,
        &digest,
        &[],
    )?)
}

/// B 端材料：独立目录、独立 device 身份与本地库。
pub struct BootstrapDevice {
    pub dir: PathBuf,
    pub vault_id: VaultId,
    pub device_id: DeviceId,
    pub peer_id: u64,
    pub signing_key: DeviceSigningKey,
    pub hpke_key: DeviceHpkeKey,
    pub root: RootSecretV1,
    pub identity: LocalIdentityV1,
    pub identity_path: PathBuf,
    pub report: BootstrapReport,
}

/// B 端：恢复码 self-enroll + bootstrap 到独立目录，并落盘 DPAPI 身份。
pub fn bootstrap_from_recovery(
    relay: &str,
    source_dir: &Path,
    target_dir: &Path,
    recovery_code: &str,
) -> AnyResult<BootstrapDevice> {
    // B 端首次写入身份前初始化主钥；之后 load 不初始化。
    menghunote_sync::identity_wrap::initialize_user_key()?;
    fs::create_dir_all(target_dir)?;
    let (vault_id, _) = read_identity_meta(source_dir)?;
    let recovery_seed = RecoverySeed::from_code(recovery_code)?;
    let manager = ManagementKey::from_recovery(&recovery_seed, &vault_id)?;
    let manager_pub = manager.public_key();
    // approve 已把 grant 外层签名落库，直接走 relay 真实响应，不再使用测试垫片。
    let mut transport = StdTcpTransport::new_local_dev(relay)?;
    let api = RelayApi::new(relay, RelayApiVersion::V1);
    let enrolled =
        recovery_enroll::self_enroll(&mut transport, &api, vault_id, recovery_code, now_secs())?;
    println!(
        "[two-device] B self-enroll device={} steps={}",
        ids::hex_of(&enrolled.device_id),
        enrolled.steps.join(" | ")
    );

    let signing_key = enrolled.signing_key.clone();
    let hpke_key = enrolled.hpke_key.clone();
    let mut client = HttpRelayClient::new(
        StdTcpTransport::new_local_dev(relay)?,
        HttpRelayConfig {
            base_url: relay.to_owned(),
            api_version: RelayApiVersion::V1,
            vault_id,
            device_id: enrolled.device_id,
            manager_pub: Some(manager_pub),
        },
        enrolled.signing_key,
    );
    let vault = Vault::open(target_dir.join("vault.sqlite3"))?;
    let report = bootstrap::run(
        &vault,
        &mut client,
        vault_id,
        enrolled.peer_id,
        &enrolled.root,
    )?;
    let state = client.state()?;
    let identity = LocalIdentityV1 {
        vault_id,
        device_id: enrolled.device_id,
        peer_id: PeerId::from_bytes(peer_id_bytes(enrolled.peer_id)),
        ed25519_seed: signing_key.seed().clone(),
        x25519_secret: hpke_key.secret().clone(),
        current_epoch: enrolled.root.key_epoch,
        current_root: enrolled.root.root.clone(),
        manager_seed: None,
        pinned_instance: state.instance_id,
        pinned_manager: manager_pub,
        control_rev: state.control_rev,
        pending_rotation: None,
    };
    write_identity(target_dir, &identity)?;
    println!(
        "[two-device] B bootstrap cid={} S={} fallback={} docs={} assets={} identity_epoch={}",
        ids::hex_of(&report.checkpoint_id),
        report.covered_seq,
        report.used_fallback,
        report.docs.len(),
        report.assets,
        identity.current_epoch
    );
    for (doc_id, text) in &report.docs {
        println!(
            "[two-device] B imported doc {doc_id} text={:?}",
            text.as_deref().unwrap_or("<structure>")
        );
    }
    Ok(BootstrapDevice {
        dir: target_dir.to_path_buf(),
        vault_id,
        device_id: enrolled.device_id,
        peer_id: enrolled.peer_id,
        signing_key,
        hpke_key,
        root: enrolled.root,
        identity,
        identity_path: target_dir.join("identity.bin"),
        report,
    })
}

/// B 离线：改已有正文（不跑 sync 循环，不产生 relay 流量）。
pub fn append_offline_edit(
    vault: &Vault,
    doc_id: &str,
    peer_id: u64,
    suffix: &str,
) -> AnyResult<u64> {
    let (doc, _) = load_body_doc(vault, doc_id, peer_id, None)?;
    doc.insert_scalars(doc.text().len(), suffix)?;
    let head = load_head(vault, doc_id)?;
    let base = head.version_vector()?;
    let update = doc.pending_update(&base)?;
    let state = append_updates(
        vault,
        doc_id,
        head.head_seq,
        &[UpdateRecord::new("b-offline-edit", update)],
    )?;
    Ok(state.head_seq)
}

/// B 离线：创建只存在于 B 本地库的文档。
pub fn create_local_only_doc(
    vault: &Vault,
    doc_id: &str,
    peer_id: u64,
    text: &str,
) -> AnyResult<u64> {
    create_body_doc(vault, doc_id)?;
    let doc = BodyDoc::create(doc_id, peer_id)?;
    doc.insert_scalars(0, text)?;
    let head = load_head(vault, doc_id)?;
    let base = head.version_vector()?;
    let update = doc.pending_update(&base)?;
    let state = append_updates(
        vault,
        doc_id,
        0,
        &[UpdateRecord::new("b-local-only", update)],
    )?;
    Ok(state.head_seq)
}

/// B 离线：造一条旧 epoch、已加密但从未 ACK 的 outbox（含 parts）。
///
/// 返回 `(message_id, cipher_digest, object_id)`。
pub fn inject_unacked_outbox(
    vault: &Vault,
    old_root: &RootSecretV1,
    device_id: DeviceId,
    doc_id: &str,
    seq: u64,
    plaintext: &[u8],
) -> AnyResult<(String, [u8; 32], [u8; 32])> {
    let message_bytes: [u8; 16] = array(&mut OsRandom)?;
    let message_id = ids::hex_of(&message_bytes);
    let object_id: [u8; 32] = array(&mut OsRandom)?;
    enqueue_sync_intent(
        vault,
        &message_id,
        &format!("batch-{message_id}"),
        doc_id,
        seq.max(1),
        old_root.key_epoch,
        &object_id,
    )?;
    let (parts, digest) = encrypt_local_parts(
        old_root,
        ObjectType::Update,
        &object_id,
        message_bytes,
        device_id,
        1,
        seq.max(1),
        plaintext,
        &[],
    )?;
    vault.exec(
        "UPDATE sync_outbox SET state='encrypted', cipher_digest=?1 WHERE message_id=?2",
        &[
            Value::Blob(digest.to_vec()),
            Value::Text(message_id.clone()),
        ],
    )?;
    for (part_no, envelope, part_digest) in parts {
        vault.exec(
            "INSERT INTO sync_outbox_parts(message_id,epoch,part_no,envelope,digest) \
             VALUES(?1,?2,?3,?4,?5)",
            &[
                Value::Blob(message_id.as_bytes().to_vec()),
                Value::Integer(old_root.key_epoch as i64),
                Value::Integer(i64::from(part_no)),
                Value::Blob(envelope),
                Value::Blob(part_digest.to_vec()),
            ],
        )?;
    }
    Ok((message_id, digest, object_id))
}

/// 读正文（用于 B 离线编辑保留断言）。
pub fn body_text(vault: &Vault, doc_id: &str, peer_id: u64) -> AnyResult<String> {
    Ok(load_body_doc(vault, doc_id, peer_id, None)?.0.text())
}

/// `sync_outbox` 行数（证明重包后旧行被删、新行只出现一次）。
pub fn outbox_count(vault: &Vault) -> AnyResult<u64> {
    let rows = vault.query("SELECT COUNT(*) FROM sync_outbox", &[])?;
    match rows.first().and_then(|row| row.first()) {
        Some(Value::Integer(value)) => Ok(u64::try_from(*value).unwrap_or(0)),
        _ => Err("sync_outbox COUNT(*) 不是整数".into()),
    }
}

/// 资产明文的 `MIME || data` 解析（frozen 分页只给不透明对象）。
pub fn asset_plain_from_payload(payload: &[u8]) -> AnyResult<(String, Vec<u8>)> {
    if payload.len() < 42 {
        return Err("资产明文过短".into());
    }
    let mime_len = usize::from(u16::from_be_bytes([payload[40], payload[41]]));
    let mime =
        std::str::from_utf8(payload.get(42..42 + mime_len).ok_or("资产 MIME 越界")?)?.to_owned();
    let data = payload.get(42 + mime_len..).ok_or("资产数据越界")?.to_vec();
    Ok((mime, data))
}

/// `identity.meta` 公开 id（日志/断言用）。
pub fn identity_hex(identity: &LocalIdentityV1) -> (String, String) {
    (
        ids::hex_of(&identity.vault_id),
        ids::hex_of(&identity.device_id),
    )
}

/// A 端在当前 active epoch 上连续发布 **2 份普通 checkpoint**（不带 role），把该 epoch 的
/// 激活 pair 从 `/bootstrap` 的"最新两份"位置顶下去。
///
/// 这模拟"轮换激活之后设备继续正常发布"的真实序列：relay 的 `/bootstrap` 只按
/// `published_at DESC` 取 active epoch 最新两份 published，因此激活 pair 会退居 older，
/// 最终被 7 天后的 GC prune（只清 refs、保留签名骨架）。
///
/// **资产源用 [`EmptyAssets`]（不是 `StorageAssets`）**：轮换把资产重加密进新 epoch 时，
/// 该 ASSET 对象是用**轮换 reservation** 落库的；随后普通 checkpoint 用**checkpoint
/// reservation** 重传同一 object_id，relay `objects::put_part` 的"预算来源必须一致"
/// 规则会返回 409 ID_CONFLICT（`crates/relay/src/objects.rs:495-501`）。这是 relay 当前
/// 的限制（不在本任务改生产代码），所以普通 pair 只覆盖文档快照；
/// 资产覆盖仍由激活 pair 的候选基线承担。e2e 里要断言的链/高度/pair/prune 事实不受影响。
pub fn publish_ordinary_pair(
    vault: &Vault,
    client: &mut HttpRelayClient<StdTcpTransport>,
    vault_id: VaultId,
    device_id: DeviceId,
    signing_key: DeviceSigningKey,
    root: RootSecretV1,
    peer_id: u64,
) -> AnyResult<(CheckpointReport, CheckpointReport)> {
    // 发布前先把 engine 追到 relay 头部：候选/普通基线的 coverage 证明只认持久事实
    // （inbox 行 / skipped-asset 记录）。轮换后日志里已有 checkpoint/control 记录时按
    // 设计在它们处暂停——这正是候选基线覆盖证明需要的"暂停前已落盘"的事实。
    {
        let identity = EngineIdentity {
            device_id,
            signing_key: signing_key.clone(),
            peer_id,
        };
        let mut engine = SyncEngine::new(vault, client, identity, root.clone());
        match engine.sync_once() {
            Ok(report) => println!(
                "[two-device] A pre-checkpoint sync uploaded={} acked={} pulled={} integrated={}",
                report.push.uploaded,
                report.push.acked,
                report.pull.events,
                report.integrate.integrated
            ),
            Err(SyncError::Paused(PauseReason::UnsupportedEvent)) => println!(
                "[two-device] A pre-checkpoint sync stopped at checkpoint/control log event"
            ),
            Err(error) => return Err(error.into()),
        }
    }
    let mut writer = CheckpointWriter::new(
        vault,
        client,
        vault_id,
        device_id,
        signing_key,
        root,
        peer_id,
    );
    let first = writer.create_checkpoint(&EmptyAssets)?;
    let second = writer.create_checkpoint(&EmptyAssets)?;
    println!(
        "[two-device] A ordinary checkpoints epoch={} first={} S={} second={} S={}",
        first.epoch,
        ids::hex_of(&first.checkpoint_id),
        first.covered_seq,
        ids::hex_of(&second.checkpoint_id),
        second.covered_seq
    );
    Ok((first, second))
}

fn main() {
    println!("e2e_common 是双设备轮换示例的共享模块，请运行 e2e_rotation_two_devices。");
}
