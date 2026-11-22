//! 引擎集成测试脚手架：真 SQLite + FakeRelay（无 sleep、无网络）。
//!
//! 临时目录放在 `CARGO_TARGET_TMPDIR`（本机 cargo 子进程不能写 `%TEMP%`）。

#![allow(dead_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use menghunote_storage::{
    append_updates, append_updates_with_sync_intents, create_body_doc, load_head, BodyDoc,
    SyncIntent, UpdateRecord, Vault,
};
use menghunote_sync::device::{DeviceHpkeKey, DeviceSigningKey};
use menghunote_sync::engine::EngineIdentity;
use menghunote_sync::keys::RootSecretV1;
use menghunote_sync::relay::{
    CheckpointLease, CheckpointPublish, CheckpointRef, CheckpointRole, CheckpointStatus,
    FrozenAssetPage, FrozenAssetRef, ObjectSubmission, PublishedCheckpoint, RelayClient,
    RelayLogEvent, RelayLogKind, RelayLogPage, RelayState, SlotRecord, SlotUpload, UpdateOutcome,
    UpdateStatus, UpdateSubmission,
};
use menghunote_sync::{DeviceId, ObjectId, VaultId};
use menghunote_sync_proto::{
    checkpoint_signing_input, object_commit_signing_input, refs_digest_input, CheckpointCommit,
    DeviceCertV1, ObjectCommit, ObjectRefV1, ObjectType, Rights, SlotDescriptorV1, SlotKind,
};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

pub const VAULT_ID: VaultId = [0x5a; 16];
pub const EPOCH: u64 = 1;

/// 让本测试二进制具备可用的本地身份封装（Linux 用户主密钥）。
///
/// 测试进程首次调用时设置**独立、vault 之外**的 `XDG_DATA_HOME`
/// （`CARGO_TARGET_TMPDIR/xdg-<pid>`），再调用生产入口
/// [`menghunote_sync::identity_wrap::initialize_user_key`]；之后所有
/// `identity.protect/unprotect` / `RotationJournal::save/load` 调用完全走生产路径，
/// **没有**测试旁路。`OnceLock` 防止多测试线程并发竞态；Windows 上初始化为 no-op。
pub fn ensure_test_identity_wrap() {
    static READY: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    READY.get_or_init(|| {
        #[cfg(target_os = "linux")]
        {
            let dir =
                Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("xdg-{}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("创建测试 XDG_DATA_HOME");
            std::env::set_var("XDG_DATA_HOME", &dir);
        }
        menghunote_sync::identity_wrap::initialize_user_key()
            .expect("初始化测试用户主密钥（Linux 本地封装）");
    });
}

/// 在 `target/tmp` 下建临时目录。
pub fn temp_dir() -> TempDir {
    let base = Path::new(env!("CARGO_TARGET_TMPDIR"));
    std::fs::create_dir_all(base).expect("创建 target/tmp");
    tempfile::Builder::new()
        .prefix("sync-engine-")
        .tempdir_in(base)
        .expect("在 target/tmp 下创建临时目录")
}

/// 打开（或创建）临时 vault。
pub fn fresh_vault() -> (TempDir, Vault) {
    let dir = temp_dir();
    let path: PathBuf = dir.path().join("vaults").join("primary.sqlite3");
    let vault = Vault::open(&path).expect("打开 vault");
    (dir, vault)
}

/// 建 `loro_docs` 行并返回空正文文档。
pub fn seed_doc(vault: &Vault, doc_id: &str, peer_id: u64) -> BodyDoc {
    create_body_doc(vault, doc_id).expect("建 loro_docs 行");
    BodyDoc::create(doc_id, peer_id).expect("创建 BodyDoc")
}

/// 生成确定性 UUID 文本。
pub fn uuid_like(value: u64) -> String {
    format!("00000000-0000-4000-8000-{value:012x}")
}

/// 测试设备：身份 + 根 + 预注册证书。
pub struct Device {
    pub identity: EngineIdentity,
    pub root: RootSecretV1,
    pub hpke_key: DeviceHpkeKey,
    pub vault_id: VaultId,
    pub device_id: DeviceId,
}

impl Device {
    pub fn new(seed: u8) -> Self {
        let device_id = [seed; 16];
        let signing_key = DeviceSigningKey::generate(&mut rand_source(seed)).expect("签名密钥");
        let hpke_key = DeviceHpkeKey::generate(&mut rand_source(seed)).expect("HPKE 密钥");
        // 真实场景里所有设备从同一个槽解出同一个 vault root；测试里固定同一随机流。
        let root =
            RootSecretV1::generate(VAULT_ID, EPOCH, &mut TestRandom::new(0x524f_4f54)).expect("根");
        Self {
            identity: EngineIdentity {
                device_id,
                signing_key,
                peer_id: u64::from(seed),
            },
            root,
            hpke_key,
            vault_id: VAULT_ID,
            device_id,
        }
    }

    pub fn cert(&self) -> DeviceCertV1 {
        DeviceCertV1 {
            vault_id: self.vault_id,
            device_id: self.device_id,
            sign_ed25519_pub: self.identity.signing_key.public_key(),
            hpke_x25519_pub: self.hpke_key.public_key(),
            cert_serial: 1,
            issued_manager_rev: 1,
            rights: Rights(Rights::ALL),
        }
    }
}

fn rand_source(seed: u8) -> impl menghunote_sync::random::RandomSource {
    // 用本 crate 的确定性测试源（同一 seed 产生同一密钥流；仅测试用）。
    TestRandom::new(u64::from(seed))
}

/// 确定性随机源（SplitMix64）。
pub struct TestRandom {
    state: u64,
}

impl TestRandom {
    pub fn new(seed: u64) -> Self {
        Self {
            state: seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1),
        }
    }
}

impl menghunote_sync::random::RandomSource for TestRandom {
    fn fill(&mut self, dst: &mut [u8]) -> Result<(), menghunote_sync::SyncError> {
        for chunk in dst.chunks_mut(8) {
            self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^= z >> 31;
            let bytes = z.to_le_bytes();
            chunk.copy_from_slice(&bytes[..chunk.len()]);
        }
        Ok(())
    }
}

/// 把一次本地提交（含出站意图）推进 A 事务；返回本地 seq。
pub fn commit_with_intent(
    vault: &Vault,
    doc: &BodyDoc,
    message_id: &str,
    object_id: ObjectId,
    epoch: u64,
) -> u64 {
    let head = load_head(vault, doc.doc_id()).expect("head");
    let base = head.version_vector().expect("head vv");
    let update = doc.pending_update(&base).expect("导出增量");
    let intent = SyncIntent {
        message_id: message_id.to_owned(),
        batch_id: format!("batch-{message_id}"),
        key_epoch: epoch,
        object_id,
    };
    let state = append_updates_with_sync_intents(
        vault,
        doc.doc_id(),
        head.head_seq,
        &[UpdateRecord::new(format!("batch-{message_id}"), update)],
        &[intent],
    )
    .expect("A 事务 + intent");
    assert_eq!(state.head_seq, head.head_seq + 1);
    state.head_seq
}

/// 本地提交（不带同步意图）。
pub fn commit_local(vault: &Vault, doc: &BodyDoc) -> u64 {
    let head = load_head(vault, doc.doc_id()).expect("head");
    let base = head.version_vector().expect("vv");
    let update = doc.pending_update(&base).expect("增量");
    let state = append_updates(
        vault,
        doc.doc_id(),
        head.head_seq,
        &[UpdateRecord::new("local", update)],
    )
    .expect("A 事务");
    state.head_seq
}

/// 手工构造 ASSET 明文前缀，格式与 `checkpoint::asset_plaintext` 一致：
/// `32B real_sha256 || BE64 byte_len || BE16 mime_len || MIME || data`。
///
/// 集成测试拿不到 crate 私有构造函数，所以夹具按同一格式手工拼；负例需要构造
/// "明文头与 object_id 不对应"的字节，正好也要求夹具能独立控制每个字段。
pub fn asset_plaintext_bytes(
    real_sha256: &[u8; 32],
    byte_len: u64,
    media_type: &str,
    data: &[u8],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(42 + media_type.len() + data.len());
    out.extend_from_slice(real_sha256);
    out.extend_from_slice(&byte_len.to_be_bytes());
    out.extend_from_slice(&(media_type.len() as u16).to_be_bytes());
    out.extend_from_slice(media_type.as_bytes());
    out.extend_from_slice(data);
    out
}

/// 用给定 `object_id` 上传任意 ASSET 明文，并让 FakeRelay 发布对应日志事件。
///
/// 返回 `(object_id, relay_seq)`。负例可借此构造"AEAD 可解但明文头不 canonical"
/// 的对象（object_id 与 MIME/长度/数据摘要不对应）。
pub fn upload_asset_plaintext(
    relay: &mut FakeRelay,
    vault_id: VaultId,
    producer_device_id: DeviceId,
    signing_key: &DeviceSigningKey,
    root: &RootSecretV1,
    object_id: ObjectId,
    plaintext: &[u8],
) -> (ObjectId, u64) {
    let message_id =
        menghunote_sync::upload::asset_message_id(root, &object_id).expect("ASSET message_id");
    let uploaded = menghunote_sync::upload::upload_object(
        relay,
        vault_id,
        producer_device_id,
        signing_key,
        root,
        ObjectType::Asset,
        object_id,
        message_id,
        0,
        0,
        plaintext,
        &[],
        None,
    )
    .expect("上传 ASSET 对象");
    assert_eq!(uploaded.object_id, object_id);
    let seq = relay.log.last().expect("ASSET 日志事件").relay_seq;
    (object_id, seq)
}

/// 上传一个**canonical** 真实 ASSET（数据/长度/MIME/object_id 自洽）并发布日志事件。
///
/// 返回 `(object_id, relay_seq)`。这是 engine pull 来源认证的正例夹具。
pub fn publish_real_asset(
    relay: &mut FakeRelay,
    vault_id: VaultId,
    producer_device_id: DeviceId,
    signing_key: &DeviceSigningKey,
    root: &RootSecretV1,
    data: &[u8],
    media_type: &str,
) -> (ObjectId, u64) {
    let real: [u8; 32] = Sha256::digest(data).into();
    let byte_len = data.len() as u64;
    let object_id = menghunote_sync::checkpoint::asset_object_id(root, &real, byte_len, media_type)
        .expect("资产 object_id");
    let plaintext = asset_plaintext_bytes(&real, byte_len, media_type, data);
    upload_asset_plaintext(
        relay,
        vault_id,
        producer_device_id,
        signing_key,
        root,
        object_id,
        &plaintext,
    )
}

/// 发布一条**声明完全自洽、但没有真实密文对象**的 ASSET 日志事件。
///
/// 用于"恶意 relay 把不存在的资产伪报成 ASSET"的负例：commit 的 message_id 按根
/// 确定性派生、分片/长度自洽，prepare 会记录 pending 候选，validate 必须真实下载
/// 失败（NOT_FOUND）。
pub fn publish_asset_declaration(
    relay: &mut FakeRelay,
    signing_key: &DeviceSigningKey,
    root: &RootSecretV1,
    object_id: ObjectId,
) -> u64 {
    let message_id =
        menghunote_sync::upload::asset_message_id(root, &object_id).expect("ASSET message_id");
    let commit = ObjectCommit {
        vault_id: relay.vault_id,
        object_id,
        message_id,
        object_type: ObjectType::Asset,
        generation: 0,
        key_epoch: relay.epoch,
        sender_seq: 0,
        part_count: 1,
        total_len: 128,
        object_digest: [0x3c; 32],
    };
    let signed = commit.encode().expect("ASSET ObjectCommit 编码");
    let signature =
        signing_key.sign(&object_commit_signing_input(&commit).expect("ASSET 签名输入"));
    let seq = relay.head_seq + 1;
    relay.log.push(RelayLogEvent {
        relay_seq: seq,
        kind: RelayLogKind::Asset,
        object_id: Some(object_id),
        epoch: relay.epoch,
        event: signed,
        signature: Some(signature),
    });
    relay.head_seq = seq;
    seq
}

/// 篡改 FakeRelay 最近一条 ASSET 日志事件里的 `ObjectCommit`（模拟恶意 relay 声明）。
///
/// 只改事件字节，不动已上传的真实 parts/对象——正是"字段自洽但没有真实对象/字段与
/// 对象不符"的攻击面。
pub fn tamper_last_asset_commit(relay: &mut FakeRelay, mutate: impl FnOnce(&mut ObjectCommit)) {
    let event = relay.log.last_mut().expect("ASSET 日志事件");
    assert_eq!(event.kind, RelayLogKind::Asset, "最后一条必须是 ASSET");
    let mut commit = ObjectCommit::decode(&event.event).expect("ASSET ObjectCommit 解码");
    mutate(&mut commit);
    event.event = commit.encode().expect("ASSET ObjectCommit 编码");
}

/// 给引擎预热游标（真实流程由 bootstrap 写；测试从空日志的 c0 开始）。
pub fn seed_cursor(vault: &Vault) {
    vault
        .set_meta(menghunote_sync::engine::RELAY_CURSOR_META, b"c0")
        .expect("写游标");
}

/// 读某文档的 (seq, update_id, payload_len) 列表（本地 seq 断言用；走 storage 只读 API）。
pub fn local_update_rows(vault: &Vault, doc_id: &str) -> Vec<(u64, String, usize)> {
    menghunote_storage::doc_updates_since(vault, doc_id, 0)
        .expect("doc_updates_since")
        .into_iter()
        .map(|update| (update.seq, update.update_id, update.payload.len()))
        .collect()
}

// ---------------------------------------------------------------- FakeRelay

#[derive(Debug, Clone)]
pub struct StoredObject {
    pub commit: menghunote_sync_proto::ObjectCommit,
    pub commit_bytes: Vec<u8>,
    pub signature: [u8; 64],
}

/// FakeRelay 的 checkpoint 状态。
#[derive(Debug, Clone)]
pub struct FakeCheckpoint {
    pub checkpoint_id: [u8; 16],
    pub epoch: u64,
    pub covered_seq: u64,
    pub data_refs: Vec<ObjectId>,
    pub manifest_ref: Option<ObjectId>,
    pub published: bool,
    pub signed_commit: Option<Vec<u8>>,
    pub signature: Option<[u8; 64]>,
}

/// 内存 relay：实现 `RelayClient`，带故障注入点。
pub struct FakeRelay {
    pub vault_id: VaultId,
    pub log_id: [u8; 16],
    pub instance_id: [u8; 16],
    pub epoch: u64,
    pub head_seq: u64,
    pub gc_floor: u64,
    pub control_rev: u64,
    pub log: Vec<RelayLogEvent>,
    pub objects: HashMap<ObjectId, StoredObject>,
    pub parts: HashMap<(ObjectId, u32), Vec<u8>>,
    pub certs: HashMap<DeviceId, DeviceCertV1>,
    pub acks: Vec<(u64, u64, u64)>,
    /// 每次 `put_part` 的记录（重试字节一致性断言）。
    pub put_history: Vec<(ObjectId, u32, Vec<u8>)>,
    /// 每次 `part`（GET）的记录：`(object_id, part_no)`（读回次数断言）。
    pub part_gets: Vec<(ObjectId, u32)>,
    /// 指定对象在 `submit_updates` 中失败一次/永久。
    pub fail_submit: HashMap<ObjectId, &'static str>,
    /// 下一次 `log` 返回传输错误（模拟截断）。
    pub truncate_next_log: bool,
    /// 下一次 `log` 把 `next_cursor` 回显为请求游标（模拟 relay 不推进游标）。
    pub freeze_next_cursor: bool,
    /// 允许为候选 epoch（`self.epoch + 1`）创建 checkpoint。
    ///
    /// 真实 relay 在 vault `Rotating` 且目标 epoch `Staged` 时允许候选基线；
    /// checkpoint coverage 测试需要"relay 仍停在 E1、候选 checkpoint 属于 E2"。
    pub allow_candidate_epoch: bool,
    /// 所有 `part` 返回的首字节翻转（模拟篡改）。
    pub tamper_parts: bool,
    /// 日志事件里的签名翻转（模拟伪造签名）。
    pub tamper_signature: bool,
    /// 日志事件里的 commit 字节翻转（模拟伪造 commit）。
    pub tamper_commit: bool,
    /// 已登记的密钥槽。
    pub slots: HashMap<ObjectId, SlotRecord>,
    /// 当前 checkpoint staging/已发布状态。
    pub checkpoint: Option<FakeCheckpoint>,
    /// 已发布 checkpoint 历史（bootstrap 取最近两份）。
    pub published_checkpoints: Vec<FakeCheckpoint>,
    /// 下一个 checkpoint id 计数。
    pub next_checkpoint: u64,
    /// Fake rotation 视图与 request_id→rotation_id 索引。
    pub rotations: HashMap<[u8; 16], menghunote_sync::relay::RotationView>,
    pub rotation_requests: HashMap<[u8; 16], [u8; 16]>,
    /// 下一次 begin 已落库但响应丢失（模拟网络丢响应）。
    pub drop_begin_response: bool,
    /// `/control` 键 5 的管理转换链（阶段 A 校验）。
    pub manager_chain: Vec<menghunote_sync::relay::ManagerChainEntry>,
    /// 当前已吊销的设备（历史证书仍保留在 `certs`，用于 S 前历史导入测试）。
    pub revoked: std::collections::HashSet<DeviceId>,
    /// frozen 资产分页（阶段 D）。
    pub frozen_assets: Vec<FrozenAssetRef>,
    pub frozen_asset_digest: Option<[u8; 32]>,
    /// 候选 checkpoint 状态（阶段 E）。
    pub checkpoint_status: HashMap<[u8; 16], CheckpointStatus>,
    /// `/control` 键 4 的 typed kind（B 侧 epoch 检测）。
    pub control_kinds: Vec<(u64, String)>,
    /// latest self grant 证据（B 侧 orchestrator）。
    pub self_grant: Option<menghunote_sync::relay::SelfGrantEvidence>,
    /// checkpoint id → manifest 明文（只读）。
    pub checkpoint_manifests: HashMap<[u8; 16], Vec<u8>>,
    /// `/control` activation 链（`epoch_activations` 的注入证据）。
    pub epoch_activations: Vec<menghunote_sync::relay::EpochActivationLink>,
}

impl FakeRelay {
    pub fn new(devices: &[&Device]) -> Self {
        let mut certs = HashMap::new();
        for device in devices {
            certs.insert(device.device_id, device.cert());
        }
        Self {
            vault_id: VAULT_ID,
            log_id: [0x11; 16],
            instance_id: [0x22; 16],
            epoch: EPOCH,
            head_seq: 0,
            gc_floor: 0,
            control_rev: 1,
            log: Vec::new(),
            objects: HashMap::new(),
            parts: HashMap::new(),
            certs,
            acks: Vec::new(),
            put_history: Vec::new(),
            part_gets: Vec::new(),
            fail_submit: HashMap::new(),
            truncate_next_log: false,
            freeze_next_cursor: false,
            allow_candidate_epoch: false,
            tamper_parts: false,
            tamper_signature: false,
            tamper_commit: false,
            slots: HashMap::new(),
            checkpoint: None,
            published_checkpoints: Vec::new(),
            next_checkpoint: 1,
            rotations: HashMap::new(),
            rotation_requests: HashMap::new(),
            drop_begin_response: false,
            manager_chain: Vec::new(),
            revoked: std::collections::HashSet::new(),
            frozen_assets: Vec::new(),
            frozen_asset_digest: None,
            checkpoint_status: HashMap::new(),
            control_kinds: Vec::new(),
            self_grant: None,
            checkpoint_manifests: HashMap::new(),
            epoch_activations: Vec::new(),
        }
    }

    /// 某个对象被 GET 的片数（读回次数断言）。
    pub fn part_get_count(&self, object_id: &ObjectId) -> usize {
        self.part_gets
            .iter()
            .filter(|(recorded, _)| recorded == object_id)
            .count()
    }

    /// 复制一条日志事件但不重编号（构造"重复序号"负例）。
    pub fn duplicate_log_event(&mut self, index: usize) {
        let event = self.log[index].clone();
        self.log.insert(index + 1, event);
    }

    /// 直接追加一个 asset 事件（验证"范围外事件必须暂停"）。
    pub fn publish_asset(&mut self, object_id: ObjectId) {
        self.publish_event(RelayLogKind::Asset, object_id);
    }

    /// 追加一条**结构合法**的 ASSET 日志事件（engine pull 会解码 ObjectCommit 并落
    /// skipped-asset 记录）。返回分配到的 relay_seq。
    pub fn publish_signed_asset(
        &mut self,
        signing_key: &DeviceSigningKey,
        object_id: ObjectId,
    ) -> u64 {
        let seq = self.head_seq + 1;
        let event = signed_asset_event(signing_key, self.vault_id, self.epoch, object_id, seq);
        self.head_seq = seq;
        self.log.push(event);
        seq
    }

    /// 发布任意类型的日志事件（用于 unsupported 分支测试）。
    pub fn publish_event(&mut self, kind: RelayLogKind, object_id: ObjectId) {
        let seq = self.head_seq + 1;
        self.head_seq = seq;
        self.log.push(RelayLogEvent {
            relay_seq: seq,
            kind,
            object_id: Some(object_id),
            epoch: self.epoch,
            event: vec![0; 32],
            signature: None,
        });
    }

    /// 交换两个日志事件的送达顺序并重编号 relay_seq（模拟乱序/依赖后到）。
    pub fn swap_log_order(&mut self, a: usize, b: usize) {
        self.log.swap(a, b);
        for (index, event) in self.log.iter_mut().enumerate() {
            event.relay_seq = index as u64 + 1;
        }
        self.head_seq = self.log.len() as u64;
    }

    pub fn cursor_for(&self, seq: u64) -> String {
        format!("c{seq}")
    }
}

fn parse_cursor(cursor: &str) -> Result<u64, menghunote_sync::SyncError> {
    cursor
        .strip_prefix('c')
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or(menghunote_sync::SyncError::RelayWire("游标格式非法"))
}

/// 构造一条结构合法的 ASSET 日志事件。
///
/// engine 对 ASSET 事件不验签（资产信封的 producer 被规范化为 0，日志事件里拿不到
/// 生产者公钥），但仍会解码 `ObjectCommit` 并校验 type/object_id/epoch；因此测试夹具
/// 必须给出可解码、字段自洽的 commit。签名一并带上以贴近真实 relay。
pub fn signed_asset_event(
    signing_key: &DeviceSigningKey,
    vault_id: VaultId,
    epoch: u64,
    object_id: ObjectId,
    relay_seq: u64,
) -> RelayLogEvent {
    let commit = ObjectCommit {
        vault_id,
        object_id,
        message_id: [0x5a; 16],
        object_type: ObjectType::Asset,
        generation: 0,
        key_epoch: epoch,
        sender_seq: 0,
        part_count: 1,
        total_len: 128,
        object_digest: [0x3c; 32],
    };
    let signed = commit.encode().expect("ASSET ObjectCommit 编码");
    let signature =
        signing_key.sign(&object_commit_signing_input(&commit).expect("ASSET 签名输入"));
    RelayLogEvent {
        relay_seq,
        kind: RelayLogKind::Asset,
        object_id: Some(object_id),
        epoch,
        event: signed,
        signature: Some(signature),
    }
}

impl RelayClient for FakeRelay {
    fn state(&mut self) -> Result<RelayState, menghunote_sync::SyncError> {
        Ok(RelayState {
            vault_id: self.vault_id,
            log_id: self.log_id,
            instance_id: self.instance_id,
            epoch: self.epoch,
            control_rev: self.control_rev,
            auth_rev: 1,
            head_seq: self.head_seq,
            gc_floor: self.gc_floor,
            current_rotation_id: self.rotations.keys().next().copied(),
        })
    }

    fn log(
        &mut self,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<RelayLogPage, menghunote_sync::SyncError> {
        if self.truncate_next_log {
            self.truncate_next_log = false;
            return Err(menghunote_sync::SyncError::Transport("模拟网络截断"));
        }
        let after = parse_cursor(cursor.ok_or(menghunote_sync::SyncError::RelayWire("缺少游标"))?)?;
        let events: Vec<RelayLogEvent> = self
            .log
            .iter()
            .filter(|event| event.relay_seq > after)
            .take(limit as usize)
            .cloned()
            .collect();
        let mut events = events;
        if self.tamper_signature {
            for event in &mut events {
                if let Some(signature) = &mut event.signature {
                    signature[0] ^= 0x01;
                }
            }
        }
        if self.tamper_commit {
            for event in &mut events {
                if let Some(byte) = event.event.first_mut() {
                    *byte ^= 0x01;
                }
            }
        }
        let last = events.last().map_or(after, |event| event.relay_seq);
        let next_cursor = if self.freeze_next_cursor {
            self.freeze_next_cursor = false;
            self.cursor_for(after)
        } else {
            self.cursor_for(last)
        };
        Ok(RelayLogPage {
            events,
            next_cursor,
            through_seq: last,
            query_head: self.head_seq,
            epoch: self.epoch,
            gc_floor: self.gc_floor,
        })
    }

    fn part(
        &mut self,
        object_id: &ObjectId,
        part_no: u32,
    ) -> Result<Vec<u8>, menghunote_sync::SyncError> {
        self.part_gets.push((*object_id, part_no));
        let mut bytes = self.parts.get(&(*object_id, part_no)).cloned().ok_or(
            menghunote_sync::SyncError::RelayApi {
                code: "NOT_FOUND".to_owned(),
                retryable: false,
                current_epoch: None,
                current_control_rev: None,
                bootstrap_hint: None,
            },
        )?;
        if self.tamper_parts {
            bytes[0] ^= 0x01;
        }
        Ok(bytes)
    }

    fn put_part(
        &mut self,
        object_id: &ObjectId,
        part_no: u32,
        envelope: &[u8],
    ) -> Result<(), menghunote_sync::SyncError> {
        match self.parts.get(&(*object_id, part_no)) {
            Some(existing) if existing != envelope => {
                return Err(menghunote_sync::SyncError::RelayApi {
                    code: "ID_CONFLICT".to_owned(),
                    retryable: false,
                    current_epoch: None,
                    current_control_rev: None,
                    bootstrap_hint: None,
                })
            }
            Some(_) => {}
            None => {
                self.parts.insert((*object_id, part_no), envelope.to_vec());
            }
        }
        self.put_history
            .push((*object_id, part_no, envelope.to_vec()));
        Ok(())
    }

    fn submit_updates(
        &mut self,
        items: Vec<UpdateSubmission>,
    ) -> Result<Vec<UpdateOutcome>, menghunote_sync::SyncError> {
        let mut outcomes = Vec::with_capacity(items.len());
        for item in items {
            let object_id = item.object_id;
            if let Some(code) = self.fail_submit.get(&object_id).copied() {
                outcomes.push(UpdateOutcome {
                    object_id,
                    status: UpdateStatus::Failed {
                        code: Some(code.to_owned()),
                        retryable: false,
                    },
                });
                continue;
            }
            if let Some(existing) = self.objects.get(&object_id) {
                if existing.commit_bytes == item.signed_commit {
                    outcomes.push(UpdateOutcome {
                        object_id,
                        status: UpdateStatus::Duplicate,
                    });
                } else {
                    outcomes.push(UpdateOutcome {
                        object_id,
                        status: UpdateStatus::Failed {
                            code: Some("ID_CONFLICT".to_owned()),
                            retryable: false,
                        },
                    });
                }
                continue;
            }
            // 校验 parts 齐全且 digest 与 commit 一致（模拟 relay 的 complete 语义）。
            let mut hashes = Vec::new();
            for part_no in 0..item.commit.part_count {
                let Some(bytes) = self.parts.get(&(object_id, part_no)) else {
                    outcomes.push(UpdateOutcome {
                        object_id,
                        status: UpdateStatus::Failed {
                            code: Some("INCOMPLETE".to_owned()),
                            retryable: false,
                        },
                    });
                    hashes.clear();
                    break;
                };
                let digest: [u8; 32] = {
                    use sha2::Digest;
                    sha2::Sha256::digest(bytes).into()
                };
                hashes.push(digest);
            }
            if hashes.is_empty() {
                continue;
            }
            let computed: [u8; 32] = {
                use sha2::Digest;
                let input =
                    menghunote_sync_proto::object_digest_input(item.commit.part_count, &hashes)
                        .expect("digest input");
                sha2::Sha256::digest(&input).into()
            };
            if computed != item.commit.object_digest {
                outcomes.push(UpdateOutcome {
                    object_id,
                    status: UpdateStatus::Failed {
                        code: Some("ID_CONFLICT".to_owned()),
                        retryable: false,
                    },
                });
                continue;
            }
            let relay_seq = self.head_seq + 1;
            self.head_seq = relay_seq;
            self.log.push(RelayLogEvent {
                relay_seq,
                kind: RelayLogKind::Update,
                object_id: Some(object_id),
                epoch: self.epoch,
                event: item.signed_commit.clone(),
                signature: Some(item.signature),
            });
            self.objects.insert(
                object_id,
                StoredObject {
                    commit: item.commit,
                    commit_bytes: item.signed_commit,
                    signature: item.signature,
                },
            );
            outcomes.push(UpdateOutcome {
                object_id,
                status: UpdateStatus::Accepted {
                    relay_seq: Some(relay_seq),
                },
            });
        }
        Ok(outcomes)
    }

    fn ack(
        &mut self,
        received_seq: u64,
        integrated_seq: u64,
        epoch: u64,
    ) -> Result<(), menghunote_sync::SyncError> {
        self.acks.push((received_seq, integrated_seq, epoch));
        Ok(())
    }

    fn device_cert(
        &mut self,
        device_id: &DeviceId,
    ) -> Result<Option<DeviceCertV1>, menghunote_sync::SyncError> {
        Ok(self.certs.get(device_id).cloned())
    }

    fn submit_object(
        &mut self,
        item: ObjectSubmission,
    ) -> Result<UpdateOutcome, menghunote_sync::SyncError> {
        let object_id = item.object_id;
        if let Some(code) = self.fail_submit.get(&object_id).copied() {
            return Ok(UpdateOutcome {
                object_id,
                status: UpdateStatus::Failed {
                    code: Some(code.to_owned()),
                    retryable: false,
                },
            });
        }
        if let Some(existing) = self.objects.get(&object_id) {
            return Ok(UpdateOutcome {
                object_id,
                status: if existing.commit_bytes == item.signed_commit {
                    UpdateStatus::Duplicate
                } else {
                    UpdateStatus::Failed {
                        code: Some("ID_CONFLICT".to_owned()),
                        retryable: false,
                    }
                },
            });
        }
        let commit = item.commit;
        let mut hashes = Vec::new();
        for part_no in 0..commit.part_count {
            let Some(bytes) = self.parts.get(&(object_id, part_no)) else {
                return Ok(UpdateOutcome {
                    object_id,
                    status: UpdateStatus::Failed {
                        code: Some("INCOMPLETE".to_owned()),
                        retryable: false,
                    },
                });
            };
            hashes.push(sha256(bytes));
        }
        let input = menghunote_sync_proto::object_digest_input(commit.part_count, &hashes)
            .expect("digest input");
        if sha256(&input) != commit.object_digest {
            return Ok(UpdateOutcome {
                object_id,
                status: UpdateStatus::Failed {
                    code: Some("ID_CONFLICT".to_owned()),
                    retryable: false,
                },
            });
        }
        // relay 只把 **active epoch** 的 UPDATE / ASSET 写进 log；SNAPSHOT/MANIFEST/
        // KEY_SLOT 仅通过 checkpoint refs 被引用，候选 epoch（key_epoch != 当前 active
        // epoch）的对象也只是候选材料，不进入 active 日志（与真实 relay 一致）。
        let relay_seq = if matches!(commit.object_type, ObjectType::Update | ObjectType::Asset)
            && commit.key_epoch == self.epoch
        {
            let relay_seq = self.head_seq + 1;
            self.head_seq = relay_seq;
            let kind = if commit.object_type == ObjectType::Asset {
                RelayLogKind::Asset
            } else {
                RelayLogKind::Update
            };
            self.log.push(RelayLogEvent {
                relay_seq,
                kind,
                object_id: Some(object_id),
                epoch: self.epoch,
                event: item.signed_commit.clone(),
                signature: Some(item.signature),
            });
            Some(relay_seq)
        } else {
            None
        };
        self.objects.insert(
            object_id,
            StoredObject {
                commit,
                commit_bytes: item.signed_commit,
                signature: item.signature,
            },
        );
        Ok(UpdateOutcome {
            object_id,
            status: UpdateStatus::Accepted { relay_seq },
        })
    }

    fn put_key_slot(
        &mut self,
        upload: SlotUpload,
    ) -> Result<SlotRecord, menghunote_sync::SyncError> {
        let descriptor = SlotDescriptorV1::decode(&upload.descriptor)
            .map_err(|_| relay_api("INVALID_REQUEST", false))?;
        if descriptor.slot_id != upload.slot_id
            || descriptor.key_epoch != upload.epoch
            || descriptor.kind != upload.kind
            || descriptor.vault_id != self.vault_id
        {
            return Err(relay_api("INVALID_REQUEST", false));
        }
        let Some(object) = self.objects.get(&upload.slot_id) else {
            return Err(relay_api("INCOMPLETE", false));
        };
        if object.commit.object_type != ObjectType::KeySlot
            || object.commit.key_epoch != upload.epoch
        {
            return Err(relay_api("INCOMPLETE", false));
        }
        if let Some(existing) = self.slots.get(&upload.slot_id) {
            if existing.descriptor == upload.descriptor
                && existing.epoch == upload.epoch
                && existing.kind == upload.kind
            {
                return Ok(existing.clone());
            }
            return Err(relay_api("ID_CONFLICT", false));
        }
        let rev = self
            .slots
            .values()
            .filter(|slot| slot.kind == upload.kind)
            .map(|slot| slot.slot_set_rev)
            .max()
            .unwrap_or(0)
            + 1;
        let record = SlotRecord {
            slot_id: upload.slot_id,
            epoch: upload.epoch,
            kind: upload.kind,
            descriptor: upload.descriptor,
            state: "staging".to_owned(),
            slot_set_rev: rev,
            retire_at: None,
        };
        self.slots.insert(upload.slot_id, record.clone());
        Ok(record)
    }

    fn list_key_slots(&mut self) -> Result<Vec<SlotRecord>, menghunote_sync::SyncError> {
        // 与真实 relay 一致：只返回 active / 未到期的 retiring（staging 不可见）。
        let mut slots: Vec<SlotRecord> = self
            .slots
            .values()
            .filter(|slot| slot.state == "active" || slot.state == "retiring")
            .cloned()
            .collect();
        slots.sort_by_key(|slot| slot.slot_set_rev);
        Ok(slots)
    }

    fn activate_password_slot(
        &mut self,
        expected_slot_set_rev: u64,
        slot_id: ObjectId,
        _signed_message: Vec<u8>,
        _signature: [u8; 64],
    ) -> Result<u64, menghunote_sync::SyncError> {
        let current = self
            .slots
            .values()
            .filter(|slot| slot.kind == SlotKind::Password && slot.state == "active")
            .map(|slot| slot.slot_set_rev)
            .max()
            .unwrap_or(0);
        if current != expected_slot_set_rev {
            return Err(relay_api("CAS_CONFLICT", false));
        }
        let Some(staged) = self.slots.get(&slot_id).cloned() else {
            return Err(relay_api("NOT_FOUND", false));
        };
        if staged.kind != SlotKind::Password
            || staged.state != "staging"
            || staged.epoch != self.epoch
        {
            return Err(relay_api("INCOMPLETE", false));
        }
        for (id, slot) in self.slots.iter_mut() {
            if slot.kind == SlotKind::Password && slot.state == "active" {
                slot.state = "retiring".to_owned();
                slot.retire_at = Some(0);
                let _ = id;
            }
        }
        let next = expected_slot_set_rev + 1;
        let entry = self.slots.get_mut(&slot_id).expect("staged slot");
        entry.state = "active".to_owned();
        entry.slot_set_rev = next;
        Ok(next)
    }

    fn create_checkpoint(
        &mut self,
        _estimated_bytes: u64,
        epoch: Option<u64>,
    ) -> Result<CheckpointLease, menghunote_sync::SyncError> {
        if self.checkpoint.as_ref().is_some_and(|cp| !cp.published) {
            return Err(relay_api("BUSY", true));
        }
        let target_epoch = epoch.unwrap_or(self.epoch);
        // 真实 relay 在 vault `Rotating` 时允许为任意 **Staged** 候选 epoch 建基线
        // （active epoch 本身不变；可能已有上一轮被废弃的候选 epoch）。
        let candidate_epoch = target_epoch > self.epoch && self.allow_candidate_epoch;
        if target_epoch != self.epoch && !candidate_epoch {
            return Err(relay_api("EPOCH", false));
        }
        let mut checkpoint_id = [0u8; 16];
        checkpoint_id[..8].copy_from_slice(&0xCFu64.to_be_bytes());
        checkpoint_id[8..].copy_from_slice(&self.next_checkpoint.to_be_bytes());
        self.next_checkpoint += 1;
        let covered_seq = self.head_seq;
        self.checkpoint = Some(FakeCheckpoint {
            checkpoint_id,
            epoch: target_epoch,
            covered_seq,
            data_refs: Vec::new(),
            manifest_ref: None,
            published: false,
            signed_commit: None,
            signature: None,
        });
        Ok(CheckpointLease {
            checkpoint_id,
            epoch: target_epoch,
            covered_seq,
            reservation_id: [0xCE; 16],
            expires_at: 10_000,
        })
    }

    fn add_checkpoint_refs(
        &mut self,
        checkpoint_id: [u8; 16],
        refs: Vec<CheckpointRef>,
    ) -> Result<u64, menghunote_sync::SyncError> {
        let Some(checkpoint) = self.checkpoint.as_mut() else {
            return Err(relay_api("NOT_FOUND", false));
        };
        if checkpoint.checkpoint_id != checkpoint_id || checkpoint.published {
            return Err(relay_api("INVALID_STATE", false));
        }
        let mut inserted = 0;
        for reference in refs {
            let Some(object) = self.objects.get(&reference.object_id) else {
                return Err(relay_api("INCOMPLETE", false));
            };
            if object.commit.key_epoch != checkpoint.epoch {
                return Err(relay_api("EPOCH", false));
            }
            match reference.role {
                CheckpointRole::Data => {
                    if !matches!(
                        object.commit.object_type,
                        ObjectType::Snapshot | ObjectType::Asset
                    ) {
                        return Err(relay_api("INVALID_REQUEST", false));
                    }
                    if checkpoint.data_refs.contains(&reference.object_id) {
                        // 真实 relay 对同 object_id + 同 role 的重复 ref 直接跳过。
                        continue;
                    }
                    checkpoint.data_refs.push(reference.object_id);
                }
                CheckpointRole::Manifest => {
                    if object.commit.object_type != ObjectType::Manifest {
                        return Err(relay_api("INVALID_REQUEST", false));
                    }
                    if checkpoint.manifest_ref == Some(reference.object_id) {
                        // 同上：同 manifest 重放是幂等的。
                        continue;
                    }
                    if checkpoint.manifest_ref.is_some() {
                        return Err(relay_api("CAS_CONFLICT", false));
                    }
                    checkpoint.manifest_ref = Some(reference.object_id);
                }
            }
            inserted += 1;
        }
        Ok(inserted)
    }

    fn publish_checkpoint(
        &mut self,
        publish: CheckpointPublish,
    ) -> Result<PublishedCheckpoint, menghunote_sync::SyncError> {
        let Some(checkpoint) = self.checkpoint.clone() else {
            return Err(relay_api("NOT_FOUND", false));
        };
        if checkpoint.published {
            return Err(relay_api("IMMUTABLE", false));
        }
        if checkpoint.covered_seq != publish.covered_seq
            || checkpoint.manifest_ref != Some(publish.manifest_oid)
            || checkpoint.data_refs.is_empty()
        {
            return Err(relay_api("INCOMPLETE", false));
        }
        let mut refs = Vec::new();
        for object_id in &checkpoint.data_refs {
            let Some(object) = self.objects.get(object_id) else {
                return Err(relay_api("INCOMPLETE", false));
            };
            refs.push(ObjectRefV1 {
                object_id: *object_id,
                object_type: object.commit.object_type,
                generation: object.commit.generation,
                key_epoch: object.commit.key_epoch,
                part_count: object.commit.part_count,
                total_plain_len: object.commit.total_len,
                object_digest: object.commit.object_digest,
            });
        }
        let digest_input =
            refs_digest_input(&refs).map_err(|_| relay_api("INVALID_REQUEST", false))?;
        if sha256(&digest_input) != publish.refs_digest {
            return Err(relay_api("CAS_CONFLICT", false));
        }
        let commit = CheckpointCommit::decode(&publish.signed_commit)
            .map_err(|_| relay_api("INVALID_REQUEST", false))?;
        if commit.vault_id != self.vault_id
            || commit.checkpoint_id != publish.checkpoint_id
            || commit.epoch != checkpoint.epoch
            || commit.covered_relay_seq != checkpoint.covered_seq
            || commit.manifest_object_id != publish.manifest_oid
            || commit.refs_digest != publish.refs_digest
        {
            return Err(relay_api("INVALID_REQUEST", false));
        }
        let Some(cert) = self.certs.get(&commit.producer_device_id) else {
            return Err(relay_api("REVOKED", false));
        };
        let input =
            checkpoint_signing_input(&commit).map_err(|_| relay_api("INVALID_REQUEST", false))?;
        if menghunote_sync::device::verify_signature(
            &cert.sign_ed25519_pub,
            &input,
            &publish.signature,
        )
        .is_err()
        {
            return Err(relay_api("SIGNATURE_INVALID", false));
        }
        let _ = &mut self.checkpoint;
        let entry = self.checkpoint.as_mut().expect("checkpoint");
        entry.published = true;
        entry.signed_commit = Some(publish.signed_commit.clone());
        entry.signature = Some(publish.signature);
        self.published_checkpoints.push(entry.clone());
        Ok(PublishedCheckpoint {
            checkpoint_id: publish.checkpoint_id,
            epoch: checkpoint.epoch,
            covered_seq: checkpoint.covered_seq,
            published_at: 20_000,
        })
    }

    fn bootstrap(
        &mut self,
    ) -> Result<menghunote_sync::relay::BootstrapBundle, menghunote_sync::SyncError> {
        if self.published_checkpoints.len() < 2 {
            return Err(relay_api("NO_VALID_BASELINE", false));
        }
        let mut sorted = self.published_checkpoints.clone();
        sorted.sort_by_key(|checkpoint| checkpoint.covered_seq);
        let fallback = sorted[sorted.len() - 2].clone();
        let primary = sorted[sorted.len() - 1].clone();
        let manifest_oid = primary
            .manifest_ref
            .ok_or_else(|| relay_api("NO_VALID_BASELINE", false))?;
        Ok(menghunote_sync::relay::BootstrapBundle {
            primary: menghunote_sync::relay::BootstrapCheckpoint {
                checkpoint_id: primary.checkpoint_id,
                epoch: primary.epoch,
                covered_seq: primary.covered_seq,
                manifest_oid,
                refs_digest: None,
                cursor: self.cursor_for(primary.covered_seq),
                signed_commit: primary.signed_commit.clone(),
                signature: primary.signature,
            },
            fallback: menghunote_sync::relay::BootstrapCheckpoint {
                checkpoint_id: fallback.checkpoint_id,
                epoch: fallback.epoch,
                covered_seq: fallback.covered_seq,
                manifest_oid: fallback
                    .manifest_ref
                    .ok_or_else(|| relay_api("NO_VALID_BASELINE", false))?,
                refs_digest: None,
                cursor: self.cursor_for(fallback.covered_seq),
                signed_commit: fallback.signed_commit.clone(),
                signature: fallback.signature,
            },
            head_seq: self.head_seq,
            gc_floor: self.gc_floor,
            epoch: self.epoch,
            lease: menghunote_sync::relay::BootstrapLease {
                lease_id: [0xBF; 16],
                expires_at: 30_000,
                floor_seq: fallback.covered_seq,
            },
            degraded: false,
        })
    }

    fn renew_bootstrap_lease(
        &mut self,
        lease_id: [u8; 16],
    ) -> Result<menghunote_sync::relay::BootstrapLease, menghunote_sync::SyncError> {
        Ok(menghunote_sync::relay::BootstrapLease {
            lease_id,
            expires_at: 60_000,
            floor_seq: self.gc_floor,
        })
    }

    fn begin_rotation(
        &mut self,
        begin: menghunote_sync::relay::RotationBegin,
    ) -> Result<menghunote_sync::relay::RotationView, menghunote_sync::SyncError> {
        if begin.expected_epoch != self.epoch || begin.expected_control_rev != self.control_rev {
            return Err(relay_api("CAS_CONFLICT", true));
        }
        let digest = sha256(&begin.signed_body);
        let mut rotation_id = [0u8; 16];
        rotation_id.copy_from_slice(&digest[..16]);
        let view = menghunote_sync::relay::RotationView {
            rotation_id,
            state: "frozen".to_owned(),
            from_epoch: self.epoch,
            to_epoch: self.epoch + 1,
            freeze_seq: self.head_seq,
            revoked_count: begin.revoke.len() as u64,
            member_set_hash: None,
            grants_count: 0,
            grants_digest: None,
            published_checkpoints: 0,
            control_rev: self.control_rev,
            auth_rev: 1,
            manager_rev: 1,
            from_epoch_revoked_once: false,
            freeze_asset_count: self.frozen_assets.len() as u64,
            freeze_asset_digest: self.frozen_asset_digest,
            member_cert_digest: None,
            primary_checkpoint: None,
            fallback_checkpoint: None,
            activation_commit: None,
            activated_at: None,
            activated_control_rev: None,
            candidate_count: 0,
            candidate_ready: false,
            reservation_id: None,
        };
        self.rotations.insert(rotation_id, view.clone());
        self.rotation_requests.insert(begin.request_id, rotation_id);
        if self.drop_begin_response {
            self.drop_begin_response = false;
            return Err(menghunote_sync::SyncError::Transport(
                "模拟 begin 响应丢失（relay 已落库）",
            ));
        }
        Ok(view)
    }

    fn rotation(
        &mut self,
        rotation_id: [u8; 16],
    ) -> Result<menghunote_sync::relay::RotationView, menghunote_sync::SyncError> {
        self.rotations
            .get(&rotation_id)
            .cloned()
            .ok_or_else(|| relay_api("NOT_FOUND", false))
    }

    fn rotation_for_request(
        &mut self,
        request_id: [u8; 16],
    ) -> Result<Option<menghunote_sync::relay::RotationView>, menghunote_sync::SyncError> {
        Ok(self
            .rotation_requests
            .get(&request_id)
            .and_then(|rotation_id| self.rotations.get(rotation_id))
            .cloned())
    }

    fn management_chain(
        &mut self,
    ) -> Result<Vec<menghunote_sync::relay::ManagerChainEntry>, menghunote_sync::SyncError> {
        Ok(self.manager_chain.clone())
    }

    fn active_member_hpke_keys(
        &mut self,
    ) -> Result<Vec<(DeviceId, [u8; 32])>, menghunote_sync::SyncError> {
        let mut keys = self
            .certs
            .iter()
            .filter(|(device_id, _)| !self.revoked.contains(*device_id))
            .map(|(device_id, cert)| (*device_id, cert.hpke_x25519_pub))
            .collect::<Vec<_>>();
        keys.sort_by_key(|(device_id, _)| *device_id);
        Ok(keys)
    }

    fn control_kinds(&mut self) -> Result<Vec<(u64, String)>, menghunote_sync::SyncError> {
        Ok(self.control_kinds.clone())
    }

    fn epoch_activations(
        &mut self,
        from_epoch: u64,
    ) -> Result<Vec<menghunote_sync::relay::EpochActivationLink>, menghunote_sync::SyncError> {
        // 模拟真实客户端的过滤：只返回本地 epoch 之后（to_epoch > from_epoch）的环。
        Ok(self
            .epoch_activations
            .iter()
            .filter(|link| link.evidence.activation_t.to_epoch > from_epoch)
            .cloned()
            .collect())
    }

    fn self_grant_evidence(
        &mut self,
    ) -> Result<menghunote_sync::relay::SelfGrantEvidence, menghunote_sync::SyncError> {
        self.self_grant
            .clone()
            .ok_or_else(|| menghunote_sync::SyncError::Unsupported("FakeRelay 未配置 self grant"))
    }

    fn checkpoint_manifest(
        &mut self,
        checkpoint_id: &[u8; 16],
    ) -> Result<menghunote_sync::relay::CheckpointManifest, menghunote_sync::SyncError> {
        let manifest_bytes = self
            .checkpoint_manifests
            .get(checkpoint_id)
            .cloned()
            .ok_or_else(|| menghunote_sync::SyncError::Unsupported("FakeRelay 无 manifest"))?;
        Ok(menghunote_sync::relay::CheckpointManifest {
            checkpoint_id: *checkpoint_id,
            manifest_bytes,
        })
    }

    fn rotation_assets(
        &mut self,
        _rotation_id: [u8; 16],
        after: Option<[u8; 32]>,
        limit: u32,
    ) -> Result<FrozenAssetPage, menghunote_sync::SyncError> {
        let start = after
            .map(|after| {
                self.frozen_assets
                    .iter()
                    .position(|asset| asset.object_id == after)
                    .map(|index| index + 1)
                    .unwrap_or(self.frozen_assets.len())
            })
            .unwrap_or(0);
        let assets = self
            .frozen_assets
            .iter()
            .skip(start)
            .take(limit as usize)
            .cloned()
            .collect();
        Ok(FrozenAssetPage {
            assets,
            freeze_seq: self.head_seq,
            asset_set_digest: self.frozen_asset_digest,
            total: self.frozen_assets.len() as u64,
        })
    }

    fn checkpoint_status(
        &mut self,
        checkpoint_id: [u8; 16],
    ) -> Result<CheckpointStatus, menghunote_sync::SyncError> {
        self.checkpoint_status.get(&checkpoint_id).cloned().ok_or(
            menghunote_sync::SyncError::RelayWire("fake checkpoint 不存在"),
        )
    }
}

fn relay_api(code: &str, retryable: bool) -> menghunote_sync::SyncError {
    menghunote_sync::SyncError::RelayApi {
        code: code.to_owned(),
        retryable,
        current_epoch: None,
        current_control_rev: None,
        bootstrap_hint: None,
    }
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}
