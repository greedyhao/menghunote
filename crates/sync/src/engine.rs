//! 客户端同步引擎（ADR 0006 §5/§7/§16；S7-5）。
//!
//! 四个阶段，全部**幂等、可续传、失败不推进游标**：
//!
//! 1. [`SyncEngine::encrypt_pending`]：`pending_encryption` 意图 → 组 `UpdateV1` →
//!    MNE1 UPDATE 信封（固定切片/AAD/context_hash/key_epoch）→ `store_encrypted_parts` CAS；
//! 2. [`SyncEngine::push`]：`pending_send` → 逐片幂等 PUT → `mark_sent` → 批量
//!    `POST /updates`（≤256）→ 逐项 accepted/duplicate **durable ACK**（`mark_acked` CAS）；
//! 3. [`SyncEngine::pull`]：`GET /log?after=cursor` → 校验 `ObjectCommit` 签名/object_digest →
//!    AEAD 解密 → 解码 `UpdateV1` → `store_inbox_verified`（依赖未齐也 durable）→ 推进游标；
//! 4. [`SyncEngine::integrate`]：`take_inbox_ready` → 依赖（base_vv ⊆ 本地 VV）就绪 →
//!    Loro import → 断言本地 seq = `head_seq+1` → `mark_integrated`；随后
//!    [`SyncEngine::ack`] 上报 received/integrated 游标。
//!
//! **重试语义**：加密失败/进程崩溃后重试必须复用已持久化密文（storage 的
//! `store_encrypted_parts` 对同字节幂等、对不同字节冲突）。因此一旦加密成功，重试路径
//! 只走推送，不会重新生成 nonce。
//!
//! **不推进游标的失败**：任何事件校验/解密/签名失败直接返回错误，`pull` 不写游标；
//! 回滚/log_id 变化/epoch 不匹配返回 [`SyncError::Paused`]，调用方必须停止自动同步。

use crate::device::{verify_signature, DeviceSigningKey};
use crate::error::SyncError;
use crate::keys::{derive_object_key, RootSecretV1};
use crate::relay::{
    PauseReason, RelayClient, RelayLogEvent, RelayLogKind, UpdateOutcome, UpdateStatus,
    UpdateSubmission,
};
use crate::storage_view;
use crate::{DeviceId, ObjectId, VaultId};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use loro::VersionVector;
use menghunote_storage::{
    append_updates, load_body_doc, load_head, mark_acked, mark_integrated, mark_sent, outbox_parts,
    outbox_rows, pending_encryption, set_confirmed_highwater, set_sync_log_id,
    store_encrypted_parts, store_inbox_verified, sync_cursor_state, take_inbox_ready,
    SyncEncryptedPart, SyncOutboxState, UpdateRecord, Vault,
};
use menghunote_sync_proto::{
    cap_plain, expected_part_plain_len, object_commit_signing_input, object_digest_input,
    part_count_for, Envelope, EnvelopeHeader, ObjectCommit, ObjectType, UpdateV1,
};
use sha2::{Digest, Sha256};

/// `meta` 键：本设备持久化的 relay 游标（bootstrap 或上一次 pull 写入）。
pub const RELAY_CURSOR_META: &str = "sync.relay_cursor";

const MESSAGE_ID_LEN: usize = 16;
/// 单批 `POST /updates` 上限（ADR §5）。
const MAX_UPDATES_PER_BATCH: usize = 256;
const NONCE_LEN: usize = 24;
const HEADER_LEN: u16 = 196;
const PROTOCOL_VERSION: u16 = 1;
const CIPHER_SUITE: u8 = 1;

/// 引擎配置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EngineConfig {
    /// 每批推送上限（ADR §5：`POST /updates` ≤256）。
    pub send_batch: usize,
    /// 每页日志上限。
    pub page_limit: u32,
    /// 每次 integrate 最多处理的行数。
    pub integrate_limit: usize,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            send_batch: 256,
            page_limit: 256,
            integrate_limit: 256,
        }
    }
}

/// 设备身份（引擎运行所需的最小集合）。
#[derive(Clone)]
pub struct EngineIdentity {
    /// 本机设备 ID。
    pub device_id: DeviceId,
    /// 设备签名私钥。
    pub signing_key: DeviceSigningKey,
    /// Loro peer id（本地 seq 分配用的 peer，永不复用 sender_seq）。
    pub peer_id: u64,
}

impl core::fmt::Debug for EngineIdentity {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("EngineIdentity")
            .field("device_id", &crate::ids::hex_of(&self.device_id))
            .field("signing_key", &"<redacted>")
            .field("peer_id", &self.peer_id)
            .finish()
    }
}

/// 推送报告。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PushReport {
    /// 逐片上传成功的对象数。
    pub uploaded: usize,
    /// durable ACK 的对象数。
    pub acked: usize,
    /// relay 逐项失败（保留 outbox 待重试）。
    pub failed: usize,
}

/// 拉取报告。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PullReport {
    /// 本页事件数。
    pub events: usize,
    /// 成功验收并 durable 写入 inbox 的事件数。
    pub stored: usize,
    /// 已收过（`relay_seq <= received_seq`）而跳过的事件数。
    pub skipped: usize,
    /// 新游标（已持久化）。
    pub next_cursor: String,
}

/// 集成报告。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IntegrateReport {
    /// 已 import 并推进本地 seq 的对象数。
    pub integrated: usize,
    /// 依赖未就绪/未知文档，保持 durable verified 的对象数。
    pub deferred: usize,
}

/// ACK 报告。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AckReport {
    /// 已上传的本地上报 received 游标。
    pub received_seq: u64,
    /// 已上报 integrated 游标。
    pub integrated_seq: u64,
}

/// 一次完整同步报告。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncReport {
    /// 本轮新加密的对象数。
    pub encrypted: usize,
    /// 推送结果。
    pub push: PushReport,
    /// 拉取结果。
    pub pull: PullReport,
    /// 集成结果。
    pub integrate: IntegrateReport,
    /// ACK 结果。
    pub ack: AckReport,
}

/// 同步引擎。
pub struct SyncEngine<'a, C: RelayClient> {
    vault: &'a Vault,
    client: &'a mut C,
    vault_id: VaultId,
    identity: EngineIdentity,
    root: RootSecretV1,
    config: EngineConfig,
    /// 轮换阶段 A–C 冻结：禁止 push/encrypt，但 pull/integrate 仍可继续。
    rotation_frozen: bool,
}

impl<'a, C: RelayClient> core::fmt::Debug for SyncEngine<'a, C> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SyncEngine")
            .field("vault_id", &crate::ids::hex_of(&self.vault_id))
            .field("identity", &self.identity)
            .field("key_epoch", &self.root.key_epoch)
            .field("config", &self.config)
            .finish()
    }
}

impl<'a, C: RelayClient> SyncEngine<'a, C> {
    /// 默认配置构造。
    pub fn new(
        vault: &'a Vault,
        client: &'a mut C,
        identity: EngineIdentity,
        root: RootSecretV1,
    ) -> Self {
        Self::with_config(vault, client, identity, root, EngineConfig::default())
    }

    /// 指定配置构造。
    pub fn with_config(
        vault: &'a Vault,
        client: &'a mut C,
        identity: EngineIdentity,
        root: RootSecretV1,
        config: EngineConfig,
    ) -> Self {
        Self {
            vault,
            client,
            vault_id: root.vault_id,
            identity,
            root,
            config,
            rotation_frozen: false,
        }
    }

    /// 轮换阶段 A：停止后续 push/encrypt（本地 A 事务编辑仍可继续）。
    pub fn freeze_rotation(&mut self) {
        self.rotation_frozen = true;
    }

    /// 轮换 abort/restart 后解冻。
    pub fn thaw_rotation(&mut self) {
        self.rotation_frozen = false;
    }

    /// 当前使用的 epoch 根（rotation 后由调用方替换）。
    pub fn root(&self) -> &RootSecretV1 {
        &self.root
    }

    /// 替换当前 epoch 根（rotation/解锁后）。
    pub fn set_root(&mut self, root: RootSecretV1) -> Result<(), SyncError> {
        if root.vault_id != self.vault_id {
            return Err(SyncError::Descriptor("根归属 vault 与引擎不一致"));
        }
        self.root = root;
        Ok(())
    }

    /// 阶段 1：把 `pending_encryption` 意图加密成 MNE1 parts。
    pub fn encrypt_pending(&mut self, limit: usize) -> Result<usize, SyncError> {
        let rows = pending_encryption(self.vault, limit)?;
        let mut encrypted = 0usize;
        for row in rows {
            if row.key_epoch != self.root.key_epoch {
                return Err(SyncError::Paused(PauseReason::EpochMismatch));
            }
            let object_id = row
                .object_id
                .ok_or(SyncError::LocalCorrupt("outbox intent 缺少 object_id"))?;
            let meta = storage_view::doc_meta(self.vault, &row.doc_id)?
                .ok_or(SyncError::LocalCorrupt("outbox 指向不存在的文档"))?;
            let stored = storage_view::stored_update(self.vault, &row.doc_id, row.seq)?;
            let base_vv =
                normalize_vv(&storage_view::base_vv_at(self.vault, &row.doc_id, row.seq)?);
            let message_id = decode_message_id(&row.message_id)?;
            let update = UpdateV1 {
                doc_id: row.doc_id.clone(),
                generation: meta.generation,
                codec_version: meta.codec_version,
                app_doc_schema: meta.app_doc_schema,
                min_reader: meta.min_reader,
                update_id: stored.update_id,
                batch_id: row.batch_id.clone(),
                origin_device_id: self.identity.device_id,
                origin_seq: row.seq,
                base_vv,
                end_vv: stored.end_vv,
                end_frontier: stored.end_frontier,
                loro_update_bytes: stored.payload,
            };
            let payload = update.encode()?;
            let parts = self.encrypt_update(
                &object_id,
                &message_id,
                meta.generation,
                row.key_epoch,
                row.seq,
                &payload,
            )?;
            let digest = object_digest_of(&parts)?;
            // CAS：同 message/epoch 已存在不同字节会失败，重试因此不会换 nonce。
            store_encrypted_parts(self.vault, &row.message_id, row.key_epoch, &digest, &parts)?;
            encrypted += 1;
        }
        Ok(encrypted)
    }

    /// 阶段 2：上传 parts + 批量提交 + durable ACK。
    pub fn push(&mut self) -> Result<PushReport, SyncError> {
        if self.rotation_frozen {
            return Err(SyncError::Paused(PauseReason::RotationFrozen));
        }
        // `encrypted` = 还没上传完；`sent` = 上次已上传但未获 ACK（必须继续提交）。
        let batch = self.config.send_batch.clamp(1, MAX_UPDATES_PER_BATCH);
        let mut rows = outbox_rows(self.vault, SyncOutboxState::Encrypted, batch)?;
        rows.extend(outbox_rows(self.vault, SyncOutboxState::Sent, batch)?);
        rows.truncate(batch);
        let mut report = PushReport::default();
        let mut submissions = Vec::with_capacity(rows.len());
        let mut expected: Vec<(ObjectId, String, u64, [u8; 32])> = Vec::with_capacity(rows.len());
        for row in rows {
            if row.key_epoch != self.root.key_epoch {
                return Err(SyncError::Paused(PauseReason::EpochMismatch));
            }
            let object_id = row
                .object_id
                .ok_or(SyncError::LocalCorrupt("outbox intent 缺少 object_id"))?;
            let digest = row
                .cipher_digest
                .ok_or(SyncError::LocalCorrupt("encrypted outbox 缺 cipher_digest"))?;
            let parts = outbox_parts(self.vault, &row.message_id, row.key_epoch)?;
            if parts.is_empty() {
                return Err(SyncError::LocalCorrupt("encrypted outbox 没有 parts"));
            }
            let computed = object_digest_of_parts(&parts)?;
            if computed != digest {
                return Err(SyncError::LocalCorrupt(
                    "outbox parts 与 cipher_digest 不一致（疑似被改写）",
                ));
            }
            if row.state == SyncOutboxState::Encrypted {
                for part in &parts {
                    self.client
                        .put_part(&object_id, part.part_no, &part.envelope)?;
                }
                // 上传成功后才推进 sent；submit 失败时保持 sent，下一轮继续提交。
                if !mark_sent(self.vault, &row.message_id, row.key_epoch, &digest)? {
                    return Err(SyncError::LocalCorrupt("mark_sent CAS 失败"));
                }
                report.uploaded += 1;
            }

            let first = Envelope::decode(&parts[0].envelope)?;
            if first.header.object_type != ObjectType::Update {
                return Err(SyncError::LocalCorrupt("outbox 里不是 UPDATE 对象"));
            }
            let commit = ObjectCommit {
                vault_id: self.vault_id,
                object_id,
                message_id: first.header.message_id,
                object_type: first.header.object_type,
                generation: first.header.generation,
                key_epoch: first.header.key_epoch,
                sender_seq: first.header.sender_seq,
                part_count: first.header.part_count,
                total_len: first.header.total_plaintext_len,
                object_digest: digest,
            };
            let signed_commit = commit.encode()?;
            let signature = self
                .identity
                .signing_key
                .sign(&object_commit_signing_input(&commit)?);
            expected.push((object_id, row.message_id.clone(), row.key_epoch, digest));
            submissions.push(UpdateSubmission {
                object_id,
                commit,
                signed_commit,
                signature,
            });
        }

        if submissions.is_empty() {
            return Ok(report);
        }
        let outcomes = self.client.submit_updates(submissions)?;
        let mut by_object: std::collections::HashMap<ObjectId, UpdateOutcome> = outcomes
            .into_iter()
            .map(|outcome| (outcome.object_id, outcome))
            .collect();
        for (object_id, message_id, epoch, digest) in expected {
            match by_object.remove(&object_id) {
                Some(UpdateOutcome {
                    status: UpdateStatus::Accepted { .. } | UpdateStatus::Duplicate,
                    ..
                }) => {
                    // durable ACK：CAS 成功才删除 outbox（旧 ACK 不会删新行）。
                    if mark_acked(self.vault, &message_id, epoch, &digest)? {
                        report.acked += 1;
                    }
                }
                Some(UpdateOutcome {
                    status: UpdateStatus::Failed { .. },
                    ..
                }) => report.failed += 1,
                None => return Err(SyncError::RelayWire("updates 响应缺少对象结果")),
            }
        }
        Ok(report)
    }

    /// 阶段 3：拉取一页日志并验收。
    pub fn pull(&mut self) -> Result<PullReport, SyncError> {
        let state = self.client.state()?;
        if state.vault_id != self.vault_id {
            return Err(SyncError::RelayWire("state.vault_id 与本地不一致"));
        }
        let local = sync_cursor_state(self.vault)?;
        match local.log_id {
            Some(log_id) if log_id != state.log_id => {
                return Err(SyncError::Paused(PauseReason::LogIdChanged));
            }
            None => set_sync_log_id(self.vault, &state.log_id)?,
            _ => {}
        }
        if let Some(highwater) = local.confirmed_highwater {
            if state.head_seq < highwater {
                return Err(SyncError::Paused(PauseReason::RollbackDetected));
            }
        }
        if state.gc_floor > local.received_seq {
            return Err(SyncError::Paused(PauseReason::CursorExpired));
        }
        if state.epoch != self.root.key_epoch {
            return Err(SyncError::Paused(PauseReason::EpochMismatch));
        }
        let Some(cursor) = self.read_cursor()? else {
            return Err(SyncError::Unsupported("缺少 relay 游标，必须先 bootstrap"));
        };
        let page = self.client.log(Some(&cursor), self.config.page_limit)?;
        if page.epoch != state.epoch {
            return Err(SyncError::Paused(PauseReason::EpochMismatch));
        }
        let mut report = PullReport {
            events: page.events.len(),
            ..PullReport::default()
        };
        for event in &page.events {
            if event.relay_seq <= local.received_seq {
                report.skipped += 1;
                continue;
            }
            match event.kind {
                RelayLogKind::Update => {
                    self.ingest_update(&state.log_id, event)?;
                    report.stored += 1;
                }
                // 资产事件不是文档水位事件：其密文本体已由 checkpoint refs 恢复，
                // 新设备从 S 补齐日志时直接跳过，不阻塞后续 update。
                RelayLogKind::Asset => {
                    report.skipped += 1;
                }
                // checkpoint/control 由 bootstrap/checkpoint 任务处理；静默跳过会
                // 让 received 前缀出现洞，所以这里明确暂停。
                _ => return Err(SyncError::Paused(PauseReason::UnsupportedEvent)),
            }
        }
        // 只有全部事件验收成功才推进游标（失败已在上面返回）。
        self.write_cursor(&page.next_cursor)?;
        report.next_cursor = page.next_cursor.clone();
        if page.query_head > local.confirmed_highwater.unwrap_or(0) {
            set_confirmed_highwater(self.vault, page.query_head)?;
        }
        Ok(report)
    }

    /// 阶段 4：把 verified inbox 中依赖就绪的对象集成进本地库。
    pub fn integrate(&mut self, limit: usize) -> Result<IntegrateReport, SyncError> {
        let rows = take_inbox_ready(self.vault, limit)?;
        let mut report = IntegrateReport::default();
        for row in rows {
            let payload = row
                .verified_payload
                .clone()
                .ok_or(SyncError::LocalCorrupt("verified inbox 缺 payload"))?;
            let update = UpdateV1::decode(&payload)?;
            if !storage_view::doc_exists(self.vault, &update.doc_id)? {
                report.deferred += 1;
                continue;
            }
            let head = load_head(self.vault, &update.doc_id)?;
            let local_vv = head.version_vector()?;
            // 本地已覆盖 end_vv → 这个 update 已经应用过（典型是自己产生的 update 从 relay
            // 回环回来，或同 ops 的重复投递）。不 import、不分配新 seq，只标记已集成。
            let end_vv = if update.end_vv.is_empty() {
                VersionVector::new()
            } else {
                VersionVector::decode(&update.end_vv)
                    .map_err(|_| SyncError::Object("end_vv 解码失败"))?
            };
            if vv_includes(&local_vv, &end_vv) {
                mark_integrated(self.vault, &row.log_id, row.relay_seq)?;
                report.integrated += 1;
                continue;
            }
            let base_vv = if update.base_vv.is_empty() {
                VersionVector::new()
            } else {
                VersionVector::decode(&update.base_vv)
                    .map_err(|_| SyncError::Object("base_vv 解码失败"))?
            };
            if !vv_includes(&local_vv, &base_vv) {
                // 依赖未齐：保持 durable verified，等前面的对象到达。
                report.deferred += 1;
                continue;
            }
            let (doc, current) =
                load_body_doc(self.vault, &update.doc_id, self.identity.peer_id, None)?;
            let before = doc.vv();
            doc.import(&update.loro_update_bytes)?;
            let pending = doc.pending_update(&before)?;
            if !pending.payload.is_empty() {
                // head CAS 冲突是可重试的并发冲突（调用方重读 head 后重放），
                // 必须映射成可重试错误，不能当成数据丢失。
                let head_after = append_updates(
                    self.vault,
                    &update.doc_id,
                    current.head_seq,
                    &[UpdateRecord::new(update.batch_id.clone(), pending)],
                )
                .map_err(storage_view::map_storage_error)?;
                // 本地 seq 只能由 head+1 分配（禁止 sender_seq）。
                if head_after.head_seq != current.head_seq.saturating_add(1) {
                    return Err(SyncError::LocalCorrupt("本地 seq 未按 head+1 分配"));
                }
            }
            mark_integrated(self.vault, &row.log_id, row.relay_seq)?;
            report.integrated += 1;
        }
        Ok(report)
    }

    /// 上报 received/integrated 游标。
    pub fn ack(&mut self) -> Result<AckReport, SyncError> {
        let state = self.client.state()?;
        if state.epoch != self.root.key_epoch {
            return Err(SyncError::Paused(PauseReason::EpochMismatch));
        }
        let local = sync_cursor_state(self.vault)?;
        self.client
            .ack(local.received_seq, local.integrated_seq, state.epoch)?;
        if local
            .confirmed_highwater
            .is_none_or(|high| state.head_seq > high)
        {
            set_confirmed_highwater(self.vault, state.head_seq)?;
        }
        Ok(AckReport {
            received_seq: local.received_seq,
            integrated_seq: local.integrated_seq,
        })
    }

    /// 一轮完整同步（加密 → 推送 → 拉取 → 集成 → ACK）。
    pub fn sync_once(&mut self) -> Result<SyncReport, SyncError> {
        let encrypted = self.encrypt_pending(self.config.send_batch)?;
        let push = self.push()?;
        let pull = self.pull()?;
        let integrate = self.integrate(self.config.integrate_limit)?;
        let ack = self.ack()?;
        Ok(SyncReport {
            encrypted,
            push,
            pull,
            integrate,
            ack,
        })
    }

    // ---------------------------------------------------------------- 内部

    fn encrypt_update(
        &self,
        object_id: &ObjectId,
        message_id: &[u8; MESSAGE_ID_LEN],
        generation: u64,
        key_epoch: u64,
        sender_seq: u64,
        plaintext: &[u8],
    ) -> Result<Vec<SyncEncryptedPart>, SyncError> {
        if plaintext.is_empty() {
            return Err(SyncError::LocalCorrupt("update 载荷为空"));
        }
        let object_type = ObjectType::Update;
        let total = plaintext.len() as u64;
        let part_count = u32::try_from(part_count_for(total, object_type))
            .map_err(|_| SyncError::LocalCorrupt("分片数超出 u32"))?;
        let key = derive_object_key(&self.root, object_type, object_id)?;
        let cipher = XChaCha20Poly1305::new_from_slice(key.expose_slice())
            .map_err(|_| SyncError::Object("数据密钥长度非法"))?;
        let cap = cap_plain(object_type) as usize;
        let mut parts = Vec::with_capacity(part_count as usize);
        let mut rng = crate::random::OsRandom;
        for index in 0..part_count {
            let this_len = expected_part_plain_len(object_type, total, index)?;
            let start = index as usize * cap;
            let end = start
                .checked_add(this_len)
                .ok_or(SyncError::LocalCorrupt("分片偏移溢出"))?;
            let chunk = plaintext
                .get(start..end)
                .ok_or(SyncError::LocalCorrupt("分片超出载荷长度"))?;
            let nonce = crate::random::array::<NONCE_LEN>(&mut rng)?;
            let header = EnvelopeHeader {
                protocol_version: PROTOCOL_VERSION,
                header_len: HEADER_LEN,
                object_type,
                cipher_suite: CIPHER_SUITE,
                flags: 0,
                vault_id: self.vault_id,
                object_id: *object_id,
                message_id: *message_id,
                producer_device_id: self.identity.device_id,
                generation,
                key_epoch,
                sender_seq,
                part_index: index,
                part_count,
                total_plaintext_len: total,
                this_plaintext_len: u32::try_from(chunk.len())
                    .map_err(|_| SyncError::LocalCorrupt("分片明文超出 u32"))?,
                ciphertext_len: u32::try_from(chunk.len() + 16)
                    .map_err(|_| SyncError::LocalCorrupt("分片密文超出 u32"))?,
                context_hash: [0u8; 32],
                nonce,
            };
            let aad = header.aad(&[])?;
            let ciphertext = cipher
                .encrypt(
                    XNonce::from_slice(&nonce),
                    Payload {
                        msg: chunk,
                        aad: &aad,
                    },
                )
                .map_err(|_| SyncError::Object("数据对象加密失败"))?;
            let envelope = Envelope::new(header, ciphertext)?.encode()?;
            let digest: [u8; 32] = Sha256::digest(&envelope).into();
            parts.push(SyncEncryptedPart {
                part_no: index,
                envelope,
                digest,
            });
        }
        Ok(parts)
    }

    fn ingest_update(&mut self, log_id: &[u8; 16], event: &RelayLogEvent) -> Result<(), SyncError> {
        let object_id = event
            .object_id
            .ok_or(SyncError::Object("update 事件缺少 object_id"))?;
        if event.epoch != self.root.key_epoch {
            return Err(SyncError::Paused(PauseReason::EpochMismatch));
        }
        let signature = event
            .signature
            .ok_or(SyncError::Paused(PauseReason::MissingSignature))?;
        let commit = ObjectCommit::decode(&event.event)?;
        if commit.vault_id != self.vault_id
            || commit.object_id != object_id
            || commit.key_epoch != event.epoch
            || commit.object_type != ObjectType::Update
        {
            return Err(SyncError::Object("ObjectCommit 与日志事件不一致"));
        }

        let mut raw_parts = Vec::with_capacity(commit.part_count as usize);
        let mut envelopes = Vec::with_capacity(commit.part_count as usize);
        for part_no in 0..commit.part_count {
            let bytes = self.client.part(&object_id, part_no)?;
            let envelope =
                Envelope::decode(&bytes).map_err(|_| SyncError::Object("信封解码失败"))?;
            let header = &envelope.header;
            if header.object_type != ObjectType::Update
                || header.vault_id != self.vault_id
                || header.object_id != object_id
                || header.part_index != part_no
                || header.part_count != commit.part_count
                || header.key_epoch != commit.key_epoch
                || header.generation != commit.generation
                || header.sender_seq != commit.sender_seq
                || header.total_plaintext_len != commit.total_len
            {
                return Err(SyncError::Object("信封 header 与 ObjectCommit 不一致"));
            }
            raw_parts.push(bytes);
            envelopes.push(envelope);
        }

        // object_digest = SHA256(BE32(part_count) || concat(BE32(no) || SHA256(part)))
        let hashes: Vec<[u8; 32]> = raw_parts
            .iter()
            .map(|bytes| {
                let digest: [u8; 32] = Sha256::digest(bytes).into();
                digest
            })
            .collect();
        let computed: [u8; 32] =
            Sha256::digest(object_digest_input(commit.part_count, &hashes)?).into();
        if computed != commit.object_digest {
            return Err(SyncError::Object("object_digest 不符"));
        }

        // 生产者一致性 + 证书验签。
        let producer = envelopes[0].header.producer_device_id;
        let message_id = envelopes[0].header.message_id;
        if envelopes.iter().any(|env| {
            env.header.producer_device_id != producer || env.header.message_id != message_id
        }) {
            return Err(SyncError::Object(
                "各片 header 的 producer/message_id 不一致",
            ));
        }
        let cert = self
            .client
            .device_cert(&producer)?
            .ok_or(SyncError::Object("缺少生产者设备证书"))?;
        if cert.vault_id != self.vault_id || cert.device_id != producer {
            return Err(SyncError::Object("设备证书归属不符"));
        }
        verify_signature(
            &cert.sign_ed25519_pub,
            &object_commit_signing_input(&commit)?,
            &signature,
        )?;

        // AEAD 解密 + §16 载荷校验。
        let key = derive_object_key(&self.root, ObjectType::Update, &object_id)?;
        let cipher = XChaCha20Poly1305::new_from_slice(key.expose_slice())
            .map_err(|_| SyncError::Object("数据密钥长度非法"))?;
        let mut plaintext = Vec::with_capacity(commit.total_len as usize);
        for (index, envelope) in envelopes.iter().enumerate() {
            let expected = expected_part_plain_len(
                ObjectType::Update,
                commit.total_len,
                u32::try_from(index).map_err(|_| SyncError::Object("分片下标溢出"))?,
            )?;
            if envelope.header.this_plaintext_len as usize != expected {
                return Err(SyncError::Object("分片明文长度不符合固定切片"));
            }
            let aad = envelope.header.aad(&[])?;
            let chunk = cipher
                .decrypt(
                    XNonce::from_slice(&envelope.header.nonce),
                    Payload {
                        msg: &envelope.ciphertext,
                        aad: &aad,
                    },
                )
                .map_err(|_| SyncError::Object("AEAD 解密失败"))?;
            plaintext.extend_from_slice(&chunk);
        }
        if plaintext.len() as u64 != commit.total_len {
            return Err(SyncError::Object("解密后的明文长度与 commit 不符"));
        }
        let update = UpdateV1::decode(&plaintext)?;
        if update.generation != commit.generation || update.origin_seq != commit.sender_seq {
            return Err(SyncError::Object("UpdateV1 与 ObjectCommit 不一致"));
        }
        envelopes[0].header.validate_update_payload(&update)?;

        // durable 写入（依赖未齐也保存）；received 前缀在事务内推进。
        store_inbox_verified(
            self.vault,
            log_id,
            event.relay_seq,
            &object_id,
            event.epoch,
            &plaintext,
        )?;
        Ok(())
    }

    fn read_cursor(&self) -> Result<Option<String>, SyncError> {
        match self.vault.get_meta(RELAY_CURSOR_META)? {
            Some(bytes) if bytes.is_empty() => Ok(None),
            Some(bytes) => String::from_utf8(bytes)
                .map(Some)
                .map_err(|_| SyncError::LocalCorrupt("relay 游标不是 UTF-8")),
            None => Ok(None),
        }
    }

    fn write_cursor(&self, cursor: &str) -> Result<(), SyncError> {
        self.vault.set_meta(RELAY_CURSOR_META, cursor.as_bytes())?;
        Ok(())
    }
}

/// 空的 `end_vv`（seq=1 时）规范化为空版本向量的编码，避免接收端解码失败。
fn normalize_vv(bytes: &[u8]) -> Vec<u8> {
    if bytes.is_empty() {
        VersionVector::new().encode()
    } else {
        bytes.to_vec()
    }
}

/// 版本向量包含关系：`local` 覆盖 `required` 的全部区间。
fn vv_includes(local: &VersionVector, required: &VersionVector) -> bool {
    // `VersionVector` 是 peer -> 已见最大 counter（不含边界之外）。
    for (peer, required_end) in required.iter() {
        match local.get(peer) {
            Some(local_end) if local_end >= required_end => {}
            _ => return false,
        }
    }
    true
}

fn object_digest_of(parts: &[SyncEncryptedPart]) -> Result<[u8; 32], SyncError> {
    let hashes: Vec<[u8; 32]> = parts.iter().map(|part| part.digest).collect();
    let input = object_digest_input(parts.len() as u32, &hashes)?;
    Ok(Sha256::digest(&input).into())
}

fn object_digest_of_parts(
    parts: &[menghunote_storage::SyncOutboxPart],
) -> Result<[u8; 32], SyncError> {
    let hashes: Vec<[u8; 32]> = parts.iter().map(|part| part.digest).collect();
    let input = object_digest_input(parts.len() as u32, &hashes)?;
    Ok(Sha256::digest(&input).into())
}

/// 把 outbox 里的 UUID 文本转成 16B wire message_id。
fn decode_message_id(text: &str) -> Result<[u8; MESSAGE_ID_LEN], SyncError> {
    let cleaned: String = text.chars().filter(|ch| *ch != '-').collect();
    if cleaned.len() != MESSAGE_ID_LEN * 2 {
        return Err(SyncError::LocalCorrupt("outbox message_id 不是 UUID"));
    }
    crate::ids::parse_hex::<MESSAGE_ID_LEN>(&cleaned)
        .map_err(|_| SyncError::LocalCorrupt("outbox message_id 非法"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_id_accepts_uuid_and_plain_hex() {
        let expected = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ];
        assert_eq!(
            decode_message_id("00112233-4455-6677-8899-aabbccddeeff").unwrap(),
            expected
        );
        assert_eq!(
            decode_message_id("00112233445566778899aabbccddeeff").unwrap(),
            expected
        );
        assert!(decode_message_id("msg-1").is_err());
        assert!(decode_message_id("00112233-4455-6677-8899-aabbccddee").is_err());
    }

    #[test]
    fn vv_containment() {
        let mut local = VersionVector::new();
        local.insert(1, 3);
        let mut required = VersionVector::new();
        required.insert(1, 2);
        assert!(vv_includes(&local, &required));
        let mut ahead = VersionVector::new();
        ahead.insert(1, 4);
        assert!(!vv_includes(&local, &ahead));
        let mut other_peer = VersionVector::new();
        other_peer.insert(2, 1);
        assert!(!vv_includes(&local, &other_peer));
        // 空 required 永远满足
        assert!(vv_includes(&local, &VersionVector::new()));
    }
}
