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
use hmac::{Hmac, Mac};
use loro::VersionVector;
use menghunote_storage::{
    append_updates, load_body_doc, load_head, mark_acked, mark_integrated, mark_sent, outbox_parts,
    outbox_rows, pending_encryption, repack_required, set_confirmed_highwater, set_sync_log_id,
    store_encrypted_parts, store_inbox_verified, sync_cursor_state, take_inbox_ready,
    SyncEncryptedPart, SyncOutboxState, UpdateRecord, Vault,
};
use menghunote_sync_proto::{
    cap_plain, expected_part_plain_len, object_commit_signing_input, object_digest_input,
    part_count_for, Envelope, EnvelopeHeader, ObjectCommit, ObjectType, UpdateV1,
    MAX_ASSET_MIME_BYTES, MAX_ASSET_RAW_BYTES,
};
use sha2::{Digest, Sha256};

/// `meta` 键：本设备持久化的 relay 游标（bootstrap 或上一次 pull 写入）。
pub const RELAY_CURSOR_META: &str = "sync.relay_cursor";

/// durable skipped-asset 记录的 `meta` 键前缀（沿用现有 `meta` 表，不新增 schema）。
///
/// 完整键：`sync.skipped_asset.<log_id base64url>.<relay_seq 20 位十进制>`。
pub(crate) const SKIPPED_ASSET_META_PREFIX: &str = "sync.skipped_asset.";

/// skipped-asset 记录值格式版本。
///
/// v1 只绑定 relay 的声明（seq/object_id/epoch/cursor），不证明资产真实存在；
/// v2 追加本机**实际下载并完成密码学验证**得到的资产事实（real_sha256/byte_len/
/// media_type/object_digest）。
///
/// v3 追加**完整 ObjectCommit 字节**：checkpoint validate 重扫日志时必须逐字节
/// 绑定 `event/commit`（而不是只认同一个 object_id 的 ASSET 事件）。v1/v2 一律
/// 拒绝——旧开发 vault 的 durable verified 不得当作新链的证据（独立修订号，
/// 禁止把旧字节改号升级）。
const SKIPPED_ASSET_MAGIC: &[u8] = b"MN/skipped-asset/3\0";
/// 记录里的日志事件类型：1 = ASSET（只允许这一种，UPDATE 必须写 inbox）。
const SKIPPED_ASSET_KIND_ASSET: u8 = 1;
/// 记录 HMAC 的 HKDF info（与设备签名密钥做域分离，不直接复用 Ed25519 种子）。
const SKIPPED_ASSET_MAC_INFO: &[u8] = b"MN/v1/skipped-asset\0";
/// 记录里允许保存的 relay 游标最大字节数（relay 游标是给 `/log` 回显的 token）。
const SKIPPED_ASSET_MAX_CURSOR: usize = 4096;
/// 记录里允许保存的 ObjectCommit 编码最大字节数（当前编码 ≤ 160B，留余量）。
const SKIPPED_ASSET_MAX_COMMIT: usize = 512;

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

/// durable skipped-asset 记录（评审 0003 P0-B 残余）。
///
/// `SyncEngine::pull` 消费 ASSET 事件时**不写** `sync_inbox` 行；为了让 checkpoint 的
/// `ensure_coverage_proven` 不再靠"无 inbox 行 + 在游标之前"推断该序号是 ASSET，
/// pull 在推进游标之前把"本机确实消费了这条 ASSET 事件"落成 durable 事实。
///
/// 记录值受设备签名密钥派生的 HMAC 保护：seq/object_id/log_id/kind/epoch/cursor 任一
/// 字段被改写（哪怕是本地 SQLite 里的行）都会校验失败，checkpoint 一律 fail-closed。
///
/// v3 还绑定 [`VerifiedAsset`] 的四个字段与**完整 ObjectCommit 字节**：前者来自本机
/// 真实下载全部 parts 并完成 AEAD/摘要/资产头/OID 校验的结果，不是 relay 的声明；
/// 后者让 checkpoint 重扫日志时能逐字节核对"这次看到的就是验证时那条声明"。HMAC
/// 因此证明的是"本机验证过这个真实资产与这条完整声明"。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SkippedAssetRecord {
    /// 该事件所属的 relay 日志。
    pub(crate) log_id: [u8; 16],
    /// relay 日志序号。
    pub(crate) relay_seq: u64,
    /// 日志事件里的 ASSET object_id。
    pub(crate) object_id: ObjectId,
    /// 记录类型（只允许 [`SKIPPED_ASSET_KIND_ASSET`]）。
    pub(crate) kind: u8,
    /// 事件所属 key epoch。
    pub(crate) epoch: u64,
    /// 资产数据的真实 SHA256（从已解密明文头读出并校验过）。
    pub(crate) real_sha256: [u8; 32],
    /// 资产数据的真实字节数（与明文头、密文总长一致）。
    pub(crate) byte_len: u64,
    /// 资产 MIME（canonical 头的一部分，参与 object_id 重算）。
    pub(crate) media_type: String,
    /// 已由实际 parts 重算并核对的密文对象摘要。
    pub(crate) object_digest: [u8; 32],
    /// 验证时那条日志事件的**完整** `ObjectCommit` 编码（v3 起必填）。
    pub(crate) commit: Vec<u8>,
    /// 消费该事件时的 pull 游标（`page.next_cursor`，原样保存供审计）。
    pub(crate) cursor: String,
}

/// 记录 HMAC 的密钥：`HKDF(设备签名种子, salt=vault_id, info=MN/v1/skipped-asset\0)`。
///
/// 用设备签名密钥而不是 epoch 根，是为了让跨 epoch 轮换的旧记录仍能被本机验证
/// （根每个 epoch 都会重新随机）。HKDF 做了域分离，不直接把 Ed25519 种子当 HMAC 密钥。
fn skipped_asset_mac_key(
    signing_key: &DeviceSigningKey,
    vault_id: &VaultId,
) -> Result<crate::keys::Secret32, SyncError> {
    crate::kdf::hkdf32(
        signing_key.seed().expose_slice(),
        vault_id,
        SKIPPED_ASSET_MAC_INFO,
    )
}

fn skipped_asset_mac(
    material: &[u8],
    mac_key: &crate::keys::Secret32,
) -> Result<[u8; 32], SyncError> {
    type HmacSha256 = Hmac<Sha256>;
    let mut mac = <HmacSha256 as Mac>::new_from_slice(mac_key.expose_slice())
        .map_err(|_| SyncError::LocalCorrupt("skipped-asset HMAC 密钥长度非法"))?;
    mac.update(material);
    Ok(mac.finalize().into_bytes().into())
}

impl SkippedAssetRecord {
    fn encode(&self, mac_key: &crate::keys::Secret32) -> Result<Vec<u8>, SyncError> {
        if self.relay_seq == 0 {
            return Err(SyncError::LocalCorrupt("skipped-asset relay_seq 必须 >0"));
        }
        if self.kind != SKIPPED_ASSET_KIND_ASSET {
            return Err(SyncError::LocalCorrupt("skipped-asset kind 必须是 ASSET"));
        }
        if self.epoch == 0 {
            return Err(SyncError::LocalCorrupt("skipped-asset epoch 必须 >0"));
        }
        if self.cursor.is_empty() || self.cursor.len() > SKIPPED_ASSET_MAX_CURSOR {
            return Err(SyncError::LocalCorrupt("skipped-asset 游标长度非法"));
        }
        if self.media_type.is_empty() || self.media_type.len() > MAX_ASSET_MIME_BYTES {
            return Err(SyncError::LocalCorrupt(
                "skipped-asset MIME 长度非法（必须 1..=255）",
            ));
        }
        if self.commit.is_empty() || self.commit.len() > SKIPPED_ASSET_MAX_COMMIT {
            return Err(SyncError::LocalCorrupt("skipped-asset commit 长度非法"));
        }
        let cursor_len = u32::try_from(self.cursor.len())
            .map_err(|_| SyncError::LocalCorrupt("skipped-asset 游标超长"))?;
        let commit_len = u32::try_from(self.commit.len())
            .map_err(|_| SyncError::LocalCorrupt("skipped-asset commit 超长"))?;
        let mut out = Vec::with_capacity(
            SKIPPED_ASSET_MAGIC.len()
                + 16
                + 8
                + 32
                + 1
                + 8
                + 32
                + 8
                + 2
                + self.media_type.len()
                + 32
                + 4
                + self.commit.len()
                + 4
                + self.cursor.len()
                + 32,
        );
        out.extend_from_slice(SKIPPED_ASSET_MAGIC);
        out.extend_from_slice(&self.log_id);
        out.extend_from_slice(&self.relay_seq.to_be_bytes());
        out.extend_from_slice(&self.object_id);
        out.push(self.kind);
        out.extend_from_slice(&self.epoch.to_be_bytes());
        out.extend_from_slice(&self.real_sha256);
        out.extend_from_slice(&self.byte_len.to_be_bytes());
        out.extend_from_slice(&(self.media_type.len() as u16).to_be_bytes());
        out.extend_from_slice(self.media_type.as_bytes());
        out.extend_from_slice(&self.object_digest);
        out.extend_from_slice(&commit_len.to_be_bytes());
        out.extend_from_slice(&self.commit);
        out.extend_from_slice(&cursor_len.to_be_bytes());
        out.extend_from_slice(self.cursor.as_bytes());
        let tag = skipped_asset_mac(&out, mac_key)?;
        out.extend_from_slice(&tag);
        Ok(out)
    }

    fn decode(bytes: &[u8], mac_key: &crate::keys::Secret32) -> Result<Self, SyncError> {
        // 最小长度：magic + 定长字段 + MIME ≥1B + commit ≥1B + cursor ≥1B + tag。
        let min_len = SKIPPED_ASSET_MAGIC.len()
            + 16
            + 8
            + 32
            + 1
            + 8
            + 32
            + 8
            + 2
            + 1
            + 32
            + 4
            + 1
            + 4
            + 1
            + 32;
        if bytes.len() < min_len {
            return Err(SyncError::LocalCorrupt("skipped-asset 记录过短"));
        }
        let (body, tag) = bytes.split_at(bytes.len() - 32);
        let expected = skipped_asset_mac(body, mac_key)?;
        if tag != expected {
            return Err(SyncError::LocalCorrupt(
                "skipped-asset 记录认证失败（被篡改）",
            ));
        }
        if !body.starts_with(SKIPPED_ASSET_MAGIC) {
            return Err(SyncError::LocalCorrupt("skipped-asset 记录版本非法"));
        }
        let mut input = &body[SKIPPED_ASSET_MAGIC.len()..];
        let log_id = skipped_take_array::<16>(&mut input)?;
        let relay_seq = skipped_take_u64(&mut input)?;
        let object_id = skipped_take_array::<32>(&mut input)?;
        let kind = skipped_take(&mut input, 1)?[0];
        let epoch = skipped_take_u64(&mut input)?;
        let real_sha256 = skipped_take_array::<32>(&mut input)?;
        let byte_len = skipped_take_u64(&mut input)?;
        let mime_len = usize::from(skipped_take_u16(&mut input)?);
        if !(1..=MAX_ASSET_MIME_BYTES).contains(&mime_len) {
            return Err(SyncError::LocalCorrupt(
                "skipped-asset MIME 长度非法（必须 1..=255）",
            ));
        }
        let media_type = String::from_utf8(skipped_take(&mut input, mime_len)?.to_vec())
            .map_err(|_| SyncError::LocalCorrupt("skipped-asset MIME 不是 UTF-8"))?;
        let object_digest = skipped_take_array::<32>(&mut input)?;
        let commit_len = skipped_take_u32(&mut input)? as usize;
        if commit_len == 0 || commit_len > SKIPPED_ASSET_MAX_COMMIT {
            return Err(SyncError::LocalCorrupt("skipped-asset commit 长度非法"));
        }
        let commit = skipped_take(&mut input, commit_len)?.to_vec();
        let cursor_len = skipped_take_u32(&mut input)? as usize;
        if cursor_len == 0 || cursor_len > SKIPPED_ASSET_MAX_CURSOR {
            return Err(SyncError::LocalCorrupt("skipped-asset 游标长度非法"));
        }
        let cursor = String::from_utf8(skipped_take(&mut input, cursor_len)?.to_vec())
            .map_err(|_| SyncError::LocalCorrupt("skipped-asset 游标不是 UTF-8"))?;
        if !input.is_empty() {
            return Err(SyncError::LocalCorrupt("skipped-asset 记录尾部有多余字节"));
        }
        if relay_seq == 0 || epoch == 0 || kind != SKIPPED_ASSET_KIND_ASSET {
            return Err(SyncError::LocalCorrupt("skipped-asset 记录字段非法"));
        }
        // 完整 commit 必须与记录里的身份字段自洽（防止 HMAC 记录本身字段错配）。
        let commit_decoded = ObjectCommit::decode(&commit)
            .map_err(|_| SyncError::LocalCorrupt("skipped-asset commit 无法解码"))?;
        if commit_decoded.object_id != object_id
            || commit_decoded.object_type != ObjectType::Asset
            || commit_decoded.key_epoch != epoch
            || commit_decoded.generation != 0
            || commit_decoded.sender_seq != 0
            || commit_decoded.total_len == 0
            || commit_decoded.total_len > MAX_ASSET_RAW_BYTES as u64
        {
            return Err(SyncError::LocalCorrupt(
                "skipped-asset commit 与记录声明不一致",
            ));
        }
        Ok(Self {
            log_id,
            relay_seq,
            object_id,
            kind,
            epoch,
            real_sha256,
            byte_len,
            media_type,
            object_digest,
            commit,
            cursor,
        })
    }
}

fn skipped_take<'a>(input: &mut &'a [u8], len: usize) -> Result<&'a [u8], SyncError> {
    if input.len() < len {
        return Err(SyncError::LocalCorrupt("skipped-asset 记录长度不足"));
    }
    let (head, tail) = input.split_at(len);
    *input = tail;
    Ok(head)
}

fn skipped_take_array<const N: usize>(input: &mut &[u8]) -> Result<[u8; N], SyncError> {
    let bytes = skipped_take(input, N)?;
    bytes
        .try_into()
        .map_err(|_| SyncError::LocalCorrupt("skipped-asset 字段长度非法"))
}

fn skipped_take_u16(input: &mut &[u8]) -> Result<u16, SyncError> {
    let bytes: [u8; 2] = skipped_take(input, 2)?
        .try_into()
        .map_err(|_| SyncError::LocalCorrupt("skipped-asset 字段长度非法"))?;
    Ok(u16::from_be_bytes(bytes))
}

fn skipped_take_u32(input: &mut &[u8]) -> Result<u32, SyncError> {
    let bytes: [u8; 4] = skipped_take(input, 4)?
        .try_into()
        .map_err(|_| SyncError::LocalCorrupt("skipped-asset 字段长度非法"))?;
    Ok(u32::from_be_bytes(bytes))
}

fn skipped_take_u64(input: &mut &[u8]) -> Result<u64, SyncError> {
    let bytes: [u8; 8] = skipped_take(input, 8)?
        .try_into()
        .map_err(|_| SyncError::LocalCorrupt("skipped-asset 字段长度非法"))?;
    Ok(u64::from_be_bytes(bytes))
}

/// skipped-asset 记录的 `meta` 键（seq 固定 20 位十进制，便于稳定排序/诊断）。
pub(crate) fn skipped_asset_meta_key(log_id: &[u8; 16], relay_seq: u64) -> String {
    format!(
        "{SKIPPED_ASSET_META_PREFIX}{}.{relay_seq:020}",
        crate::base64url::encode(log_id)
    )
}

/// 读取并**验证**本机为某个 `log_id` 保存的全部 skipped-asset 记录。
///
/// 任何一条记录结构/HMAC/键序号不自洽都返回错误（fail-closed）：checkpoint 不能
/// 把被改写的记录当成覆盖证据。
pub(crate) fn load_skipped_asset_records(
    vault: &Vault,
    signing_key: &DeviceSigningKey,
    vault_id: &VaultId,
    log_id: &[u8; 16],
) -> Result<Vec<SkippedAssetRecord>, SyncError> {
    let prefix = format!(
        "{SKIPPED_ASSET_META_PREFIX}{}.",
        crate::base64url::encode(log_id)
    );
    let prefix_len = i64::try_from(prefix.len())
        .map_err(|_| SyncError::LocalCorrupt("skipped-asset 前缀超长"))?;
    let rows = vault.query(
        "SELECT key,value FROM meta WHERE substr(key,1,?1)=?2 ORDER BY key",
        &[
            menghunote_storage::Value::Integer(prefix_len),
            menghunote_storage::Value::Text(prefix.clone()),
        ],
    )?;
    let mac_key = skipped_asset_mac_key(signing_key, vault_id)?;
    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        let key = match row.first() {
            Some(menghunote_storage::Value::Text(key)) => key,
            _ => return Err(SyncError::LocalCorrupt("skipped-asset 键类型非法")),
        };
        let bytes = match row.get(1) {
            Some(menghunote_storage::Value::Blob(bytes)) => bytes,
            _ => return Err(SyncError::LocalCorrupt("skipped-asset 值类型非法")),
        };
        let seq_text = key
            .strip_prefix(&prefix)
            .ok_or(SyncError::LocalCorrupt("skipped-asset 键前缀不符"))?;
        if seq_text.len() != 20 || !seq_text.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(SyncError::LocalCorrupt("skipped-asset 键序号非法"));
        }
        let seq: u64 = seq_text
            .parse()
            .map_err(|_| SyncError::LocalCorrupt("skipped-asset 键序号非法"))?;
        let record = SkippedAssetRecord::decode(bytes, &mac_key)?;
        if record.log_id != *log_id || record.relay_seq != seq {
            return Err(SyncError::LocalCorrupt(
                "skipped-asset 键与记录内容不一致（被篡改）",
            ));
        }
        out.push(record);
    }
    Ok(out)
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

/// 从持久事实重建轮换冻结屏障（不依赖 RAM）。
///
/// 满足任意一条即保持冻结：
/// - `sync.repack_required=true`（旧 epoch outbox 尚未重包）；
/// - `sync.rotation_apply_phase` 存在且不是 `applied`（`applying`/`paused`/`conflict`/
///   `needs_restart`/未知值都属于"apply 未完成"，包括 merge 后、promotion 前崩溃）；
/// - rotation journal 存在且 phase 不是明确的终态 `Completed`/`Promoted`
///   （`Frozen`…`Activated`、`Aborted`、restart 中间态都属于"激活中"）。
///
/// 读取失败一律 **fail closed**：无法证明轮换已完成时，宁可继续冻结旧 epoch 的
/// encrypt/push，也不能把旧 epoch 密文推给 relay。
fn durable_rotation_barrier(vault: &Vault) -> bool {
    match repack_required(vault) {
        Ok(true) => return true,
        Ok(false) => {}
        Err(_) => return true,
    }
    if crate::rotation_journal::apply_phase_barrier(vault) {
        return true;
    }
    match crate::rotation_journal::RotationJournal::load(vault) {
        Ok(None) => false,
        Ok(Some(journal)) => !matches!(
            journal.phase,
            crate::rotation_journal::RotationPhase::Completed
                | crate::rotation_journal::RotationPhase::Promoted
        ),
        Err(_) => true,
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
            // 冻结屏障必须从持久事实重建：进程重启后不能因为 RAM 归零就解冻。
            rotation_frozen: durable_rotation_barrier(vault),
        }
    }

    /// 轮换阶段 A：停止后续 push/encrypt（本地 A 事务编辑仍可继续）。
    ///
    /// 这里只置 RAM 标志；阶段 A 的持久事实由 rotation journal /
    /// `sync.repack_required` 落盘，重启后由 [`durable_rotation_barrier`] 重建。
    pub fn freeze_rotation(&mut self) {
        self.rotation_frozen = true;
    }

    /// 轮换 abort/restart 后按持久事实重新解冻。
    ///
    /// 只有 journal 明确 `Completed`/`Promoted` 且 `repack_required=false` 时才真正
    /// 解冻；否则保持冻结，避免用一次 RAM 调用绕过 durable barrier。
    pub fn thaw_rotation(&mut self) {
        self.rotation_frozen = durable_rotation_barrier(self.vault);
    }

    /// 当前是否处于轮换冻结（push/encrypt 被拒绝）。
    ///
    /// 该值在构造期由 [`durable_rotation_barrier`] 从持久事实重建，`thaw_rotation`
    /// 也只能按持久事实重新计算，不能用一次 RAM 调用绕过。
    #[must_use]
    pub fn is_rotation_frozen(&self) -> bool {
        self.rotation_frozen
    }

    /// same-epoch 启动收尾：消费 durable typed apply receipt（评审 0003 P1-C/P1-D）。
    ///
    /// 崩溃窗口：身份 rename 已提交、`sync.rotation_apply_phase='applied'` 尚未写入。
    /// 此时 relay 与本地身份已是同一 epoch，[`Self::apply_epoch_change`] 的 epoch
    /// 检测不会触发；如果没人处理 `applying`，engine 会永久冻结。
    ///
    /// 本方法只接受**可认证**的事实：receipt + 内存/磁盘身份 + authenticated pin +
    /// completed baseline journal + 重包屏障全部一致才写 `applied`；否则返回 `Paused`
    /// 并保持冻结（fail-closed）。收尾**不会**清 `sync.repack_required`。
    ///
    /// 返回 `None` 表示当前 phase 不是 `applying`（没有需要收尾的 apply）。
    pub fn reconcile_rotation_apply(
        &mut self,
        identity_path: &std::path::Path,
        identity: &crate::identity::LocalIdentityV1,
    ) -> Result<Option<crate::rotation_apply::ApplyOutcome>, SyncError> {
        let outcome =
            crate::rotation_apply::reconcile_same_epoch_apply(self.vault, identity_path, identity)?;
        if matches!(
            outcome,
            Some(crate::rotation_apply::ApplyOutcome::Applied { .. })
        ) {
            // receipt 已认证身份 epoch/pin；换根后按 durable 事实重算屏障
            // （重包未完成时仍然冻结）。
            self.set_root(RootSecretV1::new(
                self.vault_id,
                identity.current_epoch,
                identity.current_root.clone(),
            ))?;
            self.thaw_rotation();
        }
        Ok(outcome)
    }

    /// B 侧应用 epoch 变化：进入时冻结 push，只有 Applied 才换根并解冻。
    ///
    /// activation T 由 `C: RelayClient` 的 [`RelayClient::verified_activation_t`] 提供
    /// （`HttpRelayClient` 委托 `/control` 缓存复算双签；未覆盖的客户端拿默认 `None`
    /// → 一律 `Paused`），因此本方法不需要为 T 改签名就能走真实授权路径。
    ///
    /// **阶段 I 的持久屏障由 apply 入口自己落盘**（[`crate::rotation_apply::
    /// apply_epoch_change_with_client`] 在第一次写库/merge 前写 `sync.rotation_apply_phase`
    /// = `applying`，成功后转 `applied`，失败保持 `paused`/`conflict`）。本方法只负责
    /// RAM 侧的进入冻结与成功换根；**不再**用 `staging` 之类的前置值代替持久 phase。
    #[allow(clippy::too_many_arguments)]
    pub fn apply_epoch_change(
        &mut self,
        vault: &menghunote_storage::Vault,
        identity_path: &std::path::Path,
        identity: &crate::identity::LocalIdentityV1,
        checkpoint_id: [u8; 16],
        baseline_s: u64,
        baseline_cursor: &str,
        candidate_root: &RootSecretV1,
        manager_public: [u8; 32],
        new_manager_public: [u8; 32],
        new_manager_rev: u64,
        restart_reason: Option<crate::rotation_apply::RestartReason>,
    ) -> Result<crate::rotation_apply::ApplyOutcome, SyncError> {
        self.freeze_rotation();
        let outcome = crate::rotation_apply::apply_epoch_change_with_client(
            self.client,
            vault,
            identity_path,
            identity,
            checkpoint_id,
            baseline_s,
            baseline_cursor,
            candidate_root,
            manager_public,
            new_manager_public,
            new_manager_rev,
            restart_reason,
        )?;
        if matches!(outcome, crate::rotation_apply::ApplyOutcome::Applied { .. }) {
            // same-epoch receipt 收尾时身份可能已被 rename 到更高 epoch（调用方也许已
            // 没有候选根）。若传入的根落后于已认证身份，以身份文件中的当前根为准；
            // 正常 epoch change 仍用本次候选根。
            if identity.current_epoch > candidate_root.key_epoch {
                self.set_root(RootSecretV1::new(
                    self.vault_id,
                    identity.current_epoch,
                    identity.current_root.clone(),
                ))?;
            } else {
                self.set_root(candidate_root.clone())?;
            }
            self.thaw_rotation();
        }
        Ok(outcome)
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
        if self.rotation_frozen {
            return Err(SyncError::Paused(PauseReason::RotationFrozen));
        }
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
                //
                // 评审 0003 P0-B 残余：**推进游标之前**先把"本机消费了这条 ASSET"
                // 落成 durable 事实。否则 checkpoint 只能把"游标之前、无 inbox 行"
                // 的缺口推断成 ASSET——恶意 relay 只要漏发一条 UPDATE 并推进 cursor
                // 就能伪造覆盖。写记录前必须真正下载全部分片并完成密码学验证
                // （见 `persist_skipped_asset`）；记录写入失败时整个 pull 返回错误，
                // 游标不前进。
                RelayLogKind::Asset => {
                    self.persist_skipped_asset(&state.log_id, event, &page.next_cursor)?;
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

    /// 从 relay 完整读回并验证一个 ASSET 对象（评审 0003 P0-B：ASSET 来源认证）。
    ///
    /// 日志事件里的 `ObjectCommit` 只是 relay 的声明。只有真正取回全部分片、用本
    /// epoch 根解密成功，并且资产明文头能重算出同一个 `object_id`，才能证明该事件
    /// 对应一个真实存在的密文资产；任何一步失败都返回错误，调用方不得写记录/推游标。
    ///
    /// 验证口径与 checkpoint writer 完全一致：统一走
    /// [`crate::upload::fetch_verified_asset`]（分片上限/分配保护/固定切片/每片
    /// header 上下文/AEAD/摘要/明文头/OID 重算），这里只把事件里的完整声明传进去。
    fn download_verified_asset(
        &mut self,
        object_id: &ObjectId,
        commit: &ObjectCommit,
    ) -> Result<crate::upload::VerifiedAssetFacts, SyncError> {
        crate::upload::fetch_verified_asset(
            &mut *self.client,
            &self.root,
            self.vault_id,
            MAX_ASSET_RAW_BYTES as u64,
            &crate::upload::AssetExpectation {
                object_id,
                object_type: commit.object_type,
                generation: commit.generation,
                message_id: Some(commit.message_id),
                sender_seq: commit.sender_seq,
                part_count: commit.part_count,
                total_len: commit.total_len,
                object_digest: &commit.object_digest,
                key_epoch: commit.key_epoch,
            },
        )
    }

    /// 持久写入一条 skipped-asset 记录；同一 seq 重放必须幂等，内容冲突 fail-closed。
    ///
    /// 冲突分两类：HMAC/结构/认证失败是 `LocalCorrupt`（本地记录被篡改或旧修订）；
    /// HMAC 校验通过但同一 seq 的身份/资产事实不同，是 relay 重写已消费历史，
    /// 返回 `Paused(RelayHistoryRewritten)`，**不覆盖**已有记录、不推进游标。
    ///
    /// **先做来源认证**（[`Self::download_verified_asset`]）：只有真实下载并验证通过的
    /// ASSET 才允许落记录，记录里的资产事实全部来自本机验证结果。结构校验/下载/验证
    /// 任一失败都不写记录，调用方（pull）因此不会推进游标。
    ///
    /// 结构校验（不验签，因为 ASSET 信封的 producer 被规范化为 0，事件里拿不到
    /// 生产者公钥）：事件必须是可解码的 `ObjectCommit`，类型 ASSET、object_id /
    /// vault / epoch 与日志事件一致、generation=0、sender_seq=0。
    fn persist_skipped_asset(
        &mut self,
        log_id: &[u8; 16],
        event: &RelayLogEvent,
        cursor: &str,
    ) -> Result<(), SyncError> {
        let object_id = event
            .object_id
            .ok_or(SyncError::Object("ASSET 日志事件缺少 object_id"))?;
        // 与 UPDATE 路径同口径：事件 epoch 必须**等于**本地根。
        // 旧实现只拒绝"更新"的 relay（`>`），epoch 落后会走到 AEAD 失败并报成
        // `Object`；语义上这属于"本地根/relay 不同代"，应暂停而不是当成对象损坏。
        if event.epoch != self.root.key_epoch {
            return Err(SyncError::Paused(PauseReason::EpochMismatch));
        }
        let commit = ObjectCommit::decode(&event.event)?;
        if commit.vault_id != self.vault_id
            || commit.object_id != object_id
            || commit.key_epoch != event.epoch
            || commit.object_type != ObjectType::Asset
            || commit.generation != 0
            || commit.sender_seq != 0
        {
            return Err(SyncError::Object(
                "ASSET 日志事件的 ObjectCommit 与事件不符",
            ));
        }
        // 入口先验后下载：即便是 wire 层已收紧（`ObjectCommit::validate` 的 ASSET
        // 64MiB 上限），这里也显式挡一次，保证 `commit.total_len` 不会以无界值进入
        // 读回层的容量分配。
        if commit.total_len > MAX_ASSET_RAW_BYTES as u64 {
            return Err(SyncError::Object("ASSET 总长超出 64MiB 上限"));
        }
        // 评审 0003 P0-B 残余：HMAC 只能证明本机记录过 relay 的声明；写记录之前必须
        // 真正取回并验证资产。失败直接 `?`，不写记录、不写 inbox、不推游标。
        let verified = self.download_verified_asset(&object_id, &commit)?;
        let record = SkippedAssetRecord {
            log_id: *log_id,
            relay_seq: event.relay_seq,
            object_id,
            kind: SKIPPED_ASSET_KIND_ASSET,
            epoch: event.epoch,
            real_sha256: verified.real_sha256,
            byte_len: verified.byte_len,
            media_type: verified.media_type,
            object_digest: verified.object_digest,
            commit: event.event.clone(),
            cursor: cursor.to_owned(),
        };
        let mac_key = skipped_asset_mac_key(&self.identity.signing_key, &self.vault_id)?;
        let value = record.encode(&mac_key)?;
        let key = skipped_asset_meta_key(log_id, event.relay_seq);
        // 先 insert-do-nothing，再回读比较：并发/重放时先写者赢，后写者只做一致性校验，
        // 绝不覆盖成不同事实。
        let inserted = self.vault.exec(
            "INSERT INTO meta(key, value) VALUES(?1, ?2) ON CONFLICT(key) DO NOTHING",
            &[
                menghunote_storage::Value::Text(key.clone()),
                menghunote_storage::Value::Blob(value),
            ],
        )?;
        if inserted == 1 {
            return Ok(());
        }
        let existing = self
            .vault
            .get_meta(&key)?
            .ok_or(SyncError::LocalCorrupt("skipped-asset 记录缺失"))?;
        let existing = SkippedAssetRecord::decode(&existing, &mac_key)?;
        // cursor 允许因页边界不同而变化（重试可能拿到不同 next_cursor），但身份与已验证
        // 资产字段必须完全一致。HMAC 已通过，说明本地记录没有被篡改；此时同一 seq
        // 解析出不同事实，只可能是 relay 重写了历史（或对同一 OID 返回了不同密文）——
        // 映射成暂停而不是 `LocalCorrupt`，避免让本地库永久表现为"本地损坏"。
        if existing.log_id != record.log_id
            || existing.relay_seq != record.relay_seq
            || existing.object_id != record.object_id
            || existing.kind != record.kind
            || existing.epoch != record.epoch
            || existing.real_sha256 != record.real_sha256
            || existing.byte_len != record.byte_len
            || existing.media_type != record.media_type
            || existing.object_digest != record.object_digest
            || existing.commit != record.commit
        {
            return Err(SyncError::Paused(PauseReason::RelayHistoryRewritten));
        }
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

    #[test]
    fn skipped_asset_record_rejects_old_revision_bytes() {
        let mac_key = crate::keys::Secret32::from_array([0x42; 32]);
        let object_id = [0x11; 32];
        let commit = ObjectCommit {
            vault_id: [0x01; 16],
            object_id,
            message_id: [0x22; 16],
            object_type: ObjectType::Asset,
            generation: 0,
            key_epoch: 1,
            sender_seq: 0,
            part_count: 1,
            total_len: 67,
            object_digest: [0x33; 32],
        };
        let record = SkippedAssetRecord {
            log_id: [0x44; 16],
            relay_seq: 1,
            object_id,
            kind: SKIPPED_ASSET_KIND_ASSET,
            epoch: 1,
            real_sha256: [0x55; 32],
            byte_len: 25,
            media_type: "text/plain".to_owned(),
            object_digest: [0x33; 32],
            commit: commit.encode().expect("commit"),
            cursor: "c1".to_owned(),
        };
        let bytes = record.encode(&mac_key).expect("encode");
        SkippedAssetRecord::decode(&bytes, &mac_key).expect("当前修订必须可解码");

        // 旧 v2 记录（magic 旧、HMAC 自洽）必须被独立修订号拒绝，不得改号升级。
        let mut old = bytes;
        let magic_len = SKIPPED_ASSET_MAGIC.len();
        old[..magic_len].copy_from_slice(b"MN/skipped-asset/2\0");
        let body_len = old.len() - 32;
        let tag = skipped_asset_mac(&old[..body_len], &mac_key).expect("mac");
        old[body_len..].copy_from_slice(&tag);
        let error = SkippedAssetRecord::decode(&old, &mac_key).expect_err("旧修订必须拒绝");
        assert!(format!("{error}").contains("版本非法"), "{error}");
    }
}
