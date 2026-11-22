//! 阶段 J：用新 message_id/object_id/E/nonce 重打包旧 epoch 的本地 outbox。
//!
//! 明文只在 `Zeroizing` 缓冲中短暂存在；旧行解密失败/冲突一律保留，不丢数据。

use zeroize::Zeroizing;

use crate::error::SyncError;
use crate::keys::RootSecretV1;
use crate::upload::{decrypt_envelope, encrypt_local_parts};
use crate::{DeviceId, ObjectId};
use menghunote_storage::{
    outbox_parts, outbox_repack_candidates, repack_outbox, store_encrypted_parts, OutboxKey,
    OutboxRepackCandidate, RepackOutcome, RepackPart, RepackSpec, SyncEncryptedPart,
    SyncOutboxState,
};
use menghunote_sync_proto::{ObjectType, UpdateV1};

/// 一条未重包记录，写入诊断 meta（不丢、不无限重试）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepackFailure {
    pub message_id: String,
    pub epoch: u64,
    pub reason: String,
}

/// 一轮 repack 的结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepackReport {
    /// 单事务 CAS 成功。
    pub repacked: usize,
    /// 解密失败（保留旧行）。
    pub failed: usize,
    /// NotFound/Conflict（保留旧行）。
    pub conflicts: usize,
    /// 新 message_id 列表（测试/日志用）。
    pub new_message_ids: Vec<String>,
    /// 未重包明细（写 `sync.rotation_repack_report`）。
    pub failures: Vec<RepackFailure>,
}

impl RepackReport {
    fn render(&self) -> String {
        let mut out = format!(
            "repacked={} failed={} conflicts={}\n",
            self.repacked, self.failed, self.conflicts
        );
        for failure in &self.failures {
            out.push_str(&format!(
                "{} {} {}\n",
                failure.message_id, failure.epoch, failure.reason
            ));
        }
        out
    }
}

/// 诊断 meta：重包计数与未重包明细。
pub const REPACK_REPORT_META: &str = "sync.rotation_repack_report";

fn new_message_id() -> ([u8; 16], String) {
    let bytes: [u8; 16] =
        crate::random::array(&mut crate::random::OsRandom).expect("OS CSPRNG 不可用");
    (bytes, crate::ids::hex_of(&bytes))
}

fn decode_message_id(text: &str) -> Result<[u8; 16], SyncError> {
    let cleaned: String = text.chars().filter(|ch| *ch != '-').collect();
    if cleaned.len() != 32 {
        return Err(SyncError::LocalCorrupt("outbox message_id 不是 UUID"));
    }
    crate::ids::parse_hex::<16>(&cleaned)
        .map_err(|_| SyncError::LocalCorrupt("outbox message_id 非法"))
}

fn normalize_vv(bytes: &[u8]) -> Vec<u8> {
    if bytes.is_empty() {
        loro::VersionVector::new().encode()
    } else {
        bytes.to_vec()
    }
}

/// 从 storage 的原始 Loro update 重建 pending 行的 `UpdateV1` 明文。
///
/// pending 行没有密文可解，但原始字节仍在 `loro_updates`：`update_id`、`batch_id`、
/// `end_vv`/`end_frontier` 与 payload 都按出站加密路径原样取回，origin 仍是本设备；
/// 这样重包不会丢 update_id/batch_id/origin/Loro bytes。
fn pending_update_plaintext(
    vault: &menghunote_storage::Vault,
    candidate: &OutboxRepackCandidate,
    device_id: DeviceId,
) -> Result<(u64, Vec<u8>), SyncError> {
    let meta = crate::storage_view::doc_meta(vault, &candidate.doc_id)?
        .ok_or(SyncError::LocalCorrupt("outbox 指向不存在的文档"))?;
    let stored = crate::storage_view::stored_update(vault, &candidate.doc_id, candidate.seq)?;
    let base_vv = normalize_vv(&crate::storage_view::base_vv_at(
        vault,
        &candidate.doc_id,
        candidate.seq,
    )?);
    let update = UpdateV1 {
        doc_id: candidate.doc_id.clone(),
        generation: meta.generation,
        codec_version: meta.codec_version,
        app_doc_schema: meta.app_doc_schema,
        min_reader: meta.min_reader,
        update_id: stored.update_id,
        // 出站加密写的是 outbox 行的 batch_id；这里保持同一来源。
        batch_id: candidate.batch_id.clone(),
        origin_device_id: device_id,
        origin_seq: candidate.seq,
        base_vv,
        end_vv: stored.end_vv,
        end_frontier: stored.end_frontier,
        loro_update_bytes: stored.payload,
    };
    Ok((meta.generation, update.encode()?))
}

/// 把 pending 的旧 epoch 意图按"旧 epoch 本来会写出的字节"先加密落库。
///
/// 这不是伪造新 epoch 密文：parts 用旧根加密、header 仍是旧 epoch；它只是把
/// `encrypt_pending` 对同一 intent 本来就会落下的 durable 状态补齐（拿到可 CAS 的
/// `cipher_digest`），随后与 `encrypted`/`sent` 走同一解密→新 epoch 重加密路径。
/// 若此时崩溃，库中留下的是合法的旧 epoch `encrypted` 行，下一轮 repack 可正常处理。
fn materialize_pending_old_epoch(
    vault: &menghunote_storage::Vault,
    candidate: &OutboxRepackCandidate,
    old_root: &RootSecretV1,
    device_id: DeviceId,
    object_id: &ObjectId,
) -> Result<[u8; 32], SyncError> {
    if candidate.state != SyncOutboxState::Pending {
        return Err(SyncError::LocalCorrupt("outbox 状态不是 pending"));
    }
    if old_root.key_epoch != candidate.key_epoch {
        return Err(SyncError::LocalCorrupt("旧根 epoch 与 outbox 不一致"));
    }
    let message_id = decode_message_id(&candidate.message_id)?;
    let (generation, plaintext) = pending_update_plaintext(vault, candidate, device_id)?;
    let (parts, digest) = encrypt_local_parts(
        old_root,
        ObjectType::Update,
        object_id,
        message_id,
        device_id,
        generation,
        candidate.seq,
        &plaintext,
        &[],
    )?;
    let parts: Vec<SyncEncryptedPart> = parts
        .into_iter()
        .map(|(part_no, envelope, digest)| SyncEncryptedPart {
            part_no,
            envelope,
            digest,
        })
        .collect();
    store_encrypted_parts(
        vault,
        &candidate.message_id,
        candidate.key_epoch,
        &digest,
        &parts,
    )?;
    Ok(digest)
}

/// 把一组旧 MNE1 part 解出同一对象明文，再用新 epoch 根重加密。
pub fn reencrypt_old_parts(
    old_root: &RootSecretV1,
    new_root: &RootSecretV1,
    old_object_id: &ObjectId,
    old_parts: &[menghunote_storage::SyncOutboxPart],
    device_id: DeviceId,
) -> Result<(ObjectId, String, Vec<RepackPart>, [u8; 32]), SyncError> {
    if old_parts.is_empty() {
        return Err(SyncError::LocalCorrupt("旧 outbox 没有 parts"));
    }
    let mut plaintext = Zeroizing::new(Vec::new());
    let mut generation = 0u64;
    let mut sender_seq = 0u64;
    for (index, part) in old_parts.iter().enumerate() {
        let (header, bytes) = decrypt_envelope(
            old_root,
            ObjectType::Update,
            old_object_id,
            &part.envelope,
            &[],
        )?;
        if index == 0 {
            generation = header.generation;
            sender_seq = header.sender_seq;
        }
        plaintext.extend_from_slice(&bytes);
    }
    let (message_bytes, message_id) = new_message_id();
    let new_object_id: [u8; 32] = crate::random::array(&mut crate::random::OsRandom)?;
    let (parts, cipher_digest) = encrypt_local_parts(
        new_root,
        ObjectType::Update,
        &new_object_id,
        message_bytes,
        device_id,
        generation,
        sender_seq,
        &plaintext,
        &[],
    )?;
    let parts = parts
        .into_iter()
        .map(|(part_no, envelope, digest)| RepackPart {
            part_no,
            envelope,
            digest,
        })
        .collect();
    Ok((new_object_id, message_id, parts, cipher_digest))
}

/// 旧 epoch 根集合：按 outbox row 的 `key_epoch` 选对应旧根。
///
/// 兼容单 epoch 调用方（`&RootSecretV1`），也接受 `&[RootSecretV1]` 切片/数组
/// 或 `Vec<RootSecretV1>`，让一次轮换可以重包跨多个旧 epoch 遗留的行。
/// 找不到对应 epoch 的根时返回 `None`，调用方必须 fail-closed。
pub trait OldEpochRoots {
    /// 取 `epoch` 对应的旧根。
    fn root_for(&self, epoch: u64) -> Option<&RootSecretV1>;
}

impl OldEpochRoots for RootSecretV1 {
    fn root_for(&self, epoch: u64) -> Option<&RootSecretV1> {
        (self.key_epoch == epoch).then_some(self)
    }
}

impl OldEpochRoots for [RootSecretV1] {
    fn root_for(&self, epoch: u64) -> Option<&RootSecretV1> {
        self.iter().find(|root| root.key_epoch == epoch)
    }
}

impl<const N: usize> OldEpochRoots for [RootSecretV1; N] {
    fn root_for(&self, epoch: u64) -> Option<&RootSecretV1> {
        self[..].root_for(epoch)
    }
}

impl OldEpochRoots for Vec<RootSecretV1> {
    fn root_for(&self, epoch: u64) -> Option<&RootSecretV1> {
        self[..].root_for(epoch)
    }
}

/// 阶段 J：列出旧 epoch outbox，按每行 `key_epoch` 选旧根重新加密，并通过
/// storage 单事务 CAS 替换。
///
/// 旧行可能跨多个 epoch（前几次轮换遗留），因此旧根是集合而不是单个；
/// 任何一条旧行找不到对应 epoch 的根都会**整轮 fail-closed**，不跳过、不丢行。
pub fn repack_outbox_after_rotation<R: OldEpochRoots + ?Sized>(
    vault: &menghunote_storage::Vault,
    old_roots: &R,
    new_root: &RootSecretV1,
    device_id: DeviceId,
) -> Result<RepackReport, SyncError> {
    let candidates = outbox_repack_candidates(vault, new_root.key_epoch)?;
    // 先解析出每条旧行对应的旧根；缺根必须在改动任何行之前失败。
    let mut per_row_roots = Vec::with_capacity(candidates.len());
    for candidate in &candidates {
        let old_root = old_roots
            .root_for(candidate.key_epoch)
            .ok_or_else(|| SyncError::LocalCorrupt("outbox 旧 epoch 缺少对应旧根，拒绝重包"))?;
        per_row_roots.push(old_root);
    }
    let mut report = RepackReport::default();
    for (candidate, old_root) in candidates.into_iter().zip(per_row_roots) {
        let Some(old_object_id) = candidate.object_id else {
            report.failed += 1;
            report.failures.push(RepackFailure {
                message_id: candidate.message_id,
                epoch: candidate.key_epoch,
                reason: "missing object_id".to_owned(),
            });
            continue;
        };
        // pending 行没有密文：先从 storage 的原始 Loro update 补出可 CAS 的旧 epoch
        // 密文（等价于 encrypt_pending 本会写下的 durable 状态），再走统一重打包路径。
        let old_digest = match candidate.cipher_digest {
            Some(digest) => digest,
            None if candidate.state == SyncOutboxState::Pending => {
                match materialize_pending_old_epoch(
                    vault,
                    &candidate,
                    old_root,
                    device_id,
                    &old_object_id,
                ) {
                    Ok(digest) => digest,
                    Err(_) => {
                        report.failed += 1;
                        report.failures.push(RepackFailure {
                            message_id: candidate.message_id,
                            epoch: candidate.key_epoch,
                            reason: "pending 行无法从 loro_updates 重建".to_owned(),
                        });
                        continue;
                    }
                }
            }
            None => {
                report.failed += 1;
                report.failures.push(RepackFailure {
                    message_id: candidate.message_id,
                    epoch: candidate.key_epoch,
                    reason: "missing cipher_digest".to_owned(),
                });
                continue;
            }
        };
        let parts = match outbox_parts(vault, &candidate.message_id, candidate.key_epoch) {
            Ok(parts) => parts,
            Err(_) => {
                report.failed += 1;
                report.failures.push(RepackFailure {
                    message_id: candidate.message_id,
                    epoch: candidate.key_epoch,
                    reason: "outbox parts 读取失败".to_owned(),
                });
                continue;
            }
        };
        let reencrypted =
            reencrypt_old_parts(old_root, new_root, &old_object_id, &parts, device_id);
        let (new_object_id, new_message_id, new_parts, cipher_digest) = match reencrypted {
            Ok(value) => value,
            Err(_) => {
                // 解密失败：保留旧行，等待人工/下一轮处理。
                report.failed += 1;
                report.failures.push(RepackFailure {
                    message_id: candidate.message_id,
                    epoch: candidate.key_epoch,
                    reason: "解密/重加密失败".to_owned(),
                });
                continue;
            }
        };
        let old = OutboxKey {
            message_id: candidate.message_id,
            epoch: candidate.key_epoch,
            cipher_digest: old_digest,
        };
        let spec = RepackSpec {
            message_id: new_message_id.clone(),
            object_id: new_object_id,
            epoch: new_root.key_epoch,
            parts: new_parts,
            cipher_digest,
        };
        match repack_outbox(vault, &old, &spec)? {
            RepackOutcome::Repacked => {
                report.repacked += 1;
                report.new_message_ids.push(new_message_id);
            }
            RepackOutcome::NotFound | RepackOutcome::Conflict => {
                report.conflicts += 1;
                report.failures.push(RepackFailure {
                    message_id: old.message_id,
                    epoch: old.epoch,
                    reason: "CAS NotFound/Conflict".to_owned(),
                });
            }
        }
    }
    vault.set_meta(REPACK_REPORT_META, report.render().as_bytes())?;
    Ok(report)
}
