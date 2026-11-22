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
    RelayClient, RelayLogEvent, RelayLogKind, RelayLogPage, RelayState, UpdateOutcome,
    UpdateStatus, UpdateSubmission,
};
use menghunote_sync::{DeviceId, ObjectId, VaultId};
use menghunote_sync_proto::{DeviceCertV1, Rights};
use tempfile::TempDir;

pub const VAULT_ID: VaultId = [0x5a; 16];
pub const EPOCH: u64 = 1;

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
    /// 指定对象在 `submit_updates` 中失败一次/永久。
    pub fail_submit: HashMap<ObjectId, &'static str>,
    /// 下一次 `log` 返回传输错误（模拟截断）。
    pub truncate_next_log: bool,
    /// 下一次 `log` 把 `next_cursor` 回显为请求游标（模拟 relay 不推进游标）。
    pub freeze_next_cursor: bool,
    /// 所有 `part` 返回的首字节翻转（模拟篡改）。
    pub tamper_parts: bool,
    /// 日志事件里的签名翻转（模拟伪造签名）。
    pub tamper_signature: bool,
    /// 日志事件里的 commit 字节翻转（模拟伪造 commit）。
    pub tamper_commit: bool,
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
            fail_submit: HashMap::new(),
            truncate_next_log: false,
            freeze_next_cursor: false,
            tamper_parts: false,
            tamper_signature: false,
            tamper_commit: false,
        }
    }

    /// 直接追加一个 asset 事件（验证"范围外事件必须暂停"）。
    pub fn publish_asset(&mut self, object_id: ObjectId) {
        let seq = self.head_seq + 1;
        self.head_seq = seq;
        self.log.push(RelayLogEvent {
            relay_seq: seq,
            kind: RelayLogKind::Asset,
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
                status: UpdateStatus::Accepted { relay_seq },
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
}
