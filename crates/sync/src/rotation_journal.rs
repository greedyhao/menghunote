//! 轮换 journal（S7-28，ADR §28.2；阶段 A–C 的持久状态）。
//!
//! 这里只落**本地持久状态机**，不调用 relay：阶段 A freeze 固定 intent/secret，阶段 B
//! 记录 begin 响应（request_id 幂等查询在 relay task-101 落地后接），阶段 C 要求旧根
//! 已 integrate 到 S 且没有 ≤S 的 pending outbox，才允许继续。
//!
//! journal 用 DPAPI 保护后存 `vault.meta`，不落明文密钥；根/新管理 key 等敏感字段只在
//! 内存与 DPAPI 密文中出现。未完成字段用 `TODO(task-101)` 标注的占位保持可往返。

use sha2::{Digest, Sha256};

use crate::cbor;
use crate::dpapi;
use crate::error::SyncError;
use crate::keys::{RootSecretV1, Secret32};
use crate::slot::KeySlotV1;
use menghunote_storage::Vault;

/// journal 在 vault meta 中的键。
pub const ROTATION_JOURNAL_META: &str = "sync.rotation_journal";
const JOURNAL_DOMAIN: &[u8] = b"MN/v1/rotation-journal\0";

/// 轮换阶段（只允许按顺序推进；Aborted 可从任意阶段进入）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RotationPhase {
    /// 阶段 A：已冻结旧 epoch，网络 push/encrypt 停止。
    Frozen,
    /// 阶段 B：relay `/rotations` begin 已成功，S 固定。
    Begun,
    /// 阶段 C：旧根已 integrate 至 S、本地 cut L 已固定。
    Integrated,
    /// 阶段 D：旧资产已全量重加密（task-103）。
    Reencrypted,
    /// 阶段 E：两份独立基线已各自导入验证（task-103）。
    BaselinesReady,
    /// 阶段 F：grants 已 staging（task-103）。
    GrantsReady,
    /// 阶段 G：双签 activate（task-104）。
    Activated,
    /// 阶段 H：本地原子推广（task-104）。
    Promoted,
    /// 阶段 I/J：其它设备/outbox 重打包完成（task-104）。
    Completed,
    /// 原子 restart / abort。
    Aborted,
}

impl RotationPhase {
    fn code(self) -> u64 {
        match self {
            Self::Frozen => 1,
            Self::Begun => 2,
            Self::Integrated => 3,
            Self::Reencrypted => 4,
            Self::BaselinesReady => 5,
            Self::GrantsReady => 6,
            Self::Activated => 7,
            Self::Promoted => 8,
            Self::Completed => 9,
            Self::Aborted => 10,
        }
    }

    fn from_code(code: u64) -> Result<Self, SyncError> {
        Ok(match code {
            1 => Self::Frozen,
            2 => Self::Begun,
            3 => Self::Integrated,
            4 => Self::Reencrypted,
            5 => Self::BaselinesReady,
            6 => Self::GrantsReady,
            7 => Self::Activated,
            8 => Self::Promoted,
            9 => Self::Completed,
            10 => Self::Aborted,
            _ => return Err(SyncError::LocalCorrupt("rotation journal phase 非法")),
        })
    }

    fn next(self) -> Option<Self> {
        Some(match self {
            Self::Frozen => Self::Begun,
            Self::Begun => Self::Integrated,
            Self::Integrated => Self::Reencrypted,
            Self::Reencrypted => Self::BaselinesReady,
            Self::BaselinesReady => Self::GrantsReady,
            Self::GrantsReady => Self::Activated,
            Self::Activated => Self::Promoted,
            Self::Promoted => Self::Completed,
            Self::Completed | Self::Aborted => return None,
        })
    }
}

/// 一份候选基线的验证回执。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalBaseline {
    pub checkpoint_id: [u8; 16],
    pub covered_seq: u64,
    pub cursor: String,
    pub manifest_object_id: [u8; 32],
    pub manifest_digest: [u8; 32],
    pub refs_digest: [u8; 32],
    pub verified: bool,
}

/// 轮换 journal（DPAPI 持久）。
#[derive(Clone)]
pub struct RotationJournal {
    pub phase: RotationPhase,
    /// 幂等 request_id（begin 响应丢失时用它查询，绝不盲 begin）。
    pub request_id: [u8; 16],
    /// canonical begin body 摘要。
    pub body_digest: [u8; 32],
    pub from_epoch: u64,
    pub to_epoch: u64,
    /// relay begin 返回的 rotation id；阶段 B 前为 None。
    pub rotation_id: Option<[u8; 16]>,
    /// 阶段 A 固定的 freeze 信息。
    pub freeze_seq: Option<u64>,
    pub freeze_log_id: Option<[u8; 16]>,
    pub control_rev: u64,
    pub control_hash: [u8; 32],
    /// 新 epoch 的候选根（DPAPI 保护；阶段 E 前生成）。
    pub candidate_root: Option<RootSecretV1>,
    /// 新管理 key 公钥/rev；私钥由 R' 派生，不单独存。
    pub new_manager_public: Option<[u8; 32]>,
    pub new_manager_rev: u64,
    /// activate 的 new_manager 三键（relay `fd89cfa` 后布局）：
    /// `(CDE ManagerTransition 9 字段, old_signature64, new_signature64)`。
    pub new_manager_transition: Option<Vec<u8>>,
    pub old_manager_signature: Option<[u8; 64]>,
    pub new_manager_signature: Option<[u8; 64]>,
    /// `/control` 键 5 管理转换链的 rev。
    pub control_chain_rev: Option<u64>,
    /// 阶段 A pin 的 relay instance/log/control head。
    pub pinned_instance_id: Option<[u8; 16]>,
    pub pinned_log_id: Option<[u8; 16]>,
    pub pinned_control_hash: Option<[u8; 32]>,
    /// R' 已离线备份。
    pub r_prime_backed_up: bool,
    /// 新 password/recovery 槽描述符、固定槽体、槽 id（阶段 D 上传用）。
    pub new_slot_descriptors: Vec<Vec<u8>>,
    pub new_slot_bodies: Vec<Vec<u8>>,
    pub new_slot_ids: Vec<[u8; 32]>,
    /// 两份候选基线（phase E 后各一条，互不共享状态）。
    pub baselines: Vec<JournalBaseline>,
    /// 成员证书/grants/资产集合摘要（阶段 F 用）。
    pub member_set_digest: Option<[u8; 32]>,
    pub grants_digest: Option<[u8; 32]>,
    /// 阶段 F：每个待 stage 设备的完整 grant leaf manifest（规范 CDE 字节）。
    pub grant_leaf_manifests: Vec<Vec<u8>>,
    /// 阶段 F：所有 leaf manifest 的排序集合 digest（供 activate 锁定）。
    pub grant_leaf_digest: Option<[u8; 32]>,
    pub asset_set_digest: Option<[u8; 32]>,
    /// 上传进度（对象数）。
    pub uploaded_objects: u32,
    pub total_objects: u32,
    /// outbox 重打包进度（task-104）。
    pub repacked_outbox: u32,
    pub total_outbox: u32,
    /// 本地 cut L（阶段 C 固定）。
    pub local_cut: Option<u64>,
}

impl RotationJournal {
    /// 阶段 A：冻结旧 epoch，生成持久 intent。新根/新槽在后续阶段填充。
    pub fn freeze(
        vault: &Vault,
        from_epoch: u64,
        request_id: [u8; 16],
        control_rev: u64,
        control_hash: [u8; 32],
    ) -> Result<Self, SyncError> {
        if Self::load(vault)?.is_some() {
            return Err(SyncError::Unsupported("已有未完成 rotation journal"));
        }
        let journal = Self {
            phase: RotationPhase::Frozen,
            request_id,
            body_digest: begin_body_digest(from_epoch, &request_id),
            from_epoch,
            to_epoch: from_epoch.saturating_add(1),
            rotation_id: None,
            freeze_seq: None,
            freeze_log_id: None,
            control_rev,
            control_hash,
            candidate_root: None,
            new_manager_public: None,
            new_manager_rev: 0,
            new_manager_transition: None,
            old_manager_signature: None,
            new_manager_signature: None,
            control_chain_rev: None,
            pinned_instance_id: None,
            pinned_log_id: None,
            pinned_control_hash: None,
            r_prime_backed_up: false,
            new_slot_descriptors: Vec::new(),
            new_slot_bodies: Vec::new(),
            new_slot_ids: Vec::new(),
            baselines: Vec::new(),
            member_set_digest: None,
            grants_digest: None,
            grant_leaf_manifests: Vec::new(),
            grant_leaf_digest: None,
            asset_set_digest: None,
            uploaded_objects: 0,
            total_objects: 0,
            repacked_outbox: 0,
            total_outbox: 0,
            local_cut: None,
        };
        journal.save(vault)?;
        Ok(journal)
    }

    /// 阶段 F：持久化每个未吊销设备的 grant leaf manifest 与集合 digest。
    pub fn set_grants(&mut self, vault: &Vault, leaves: &[Vec<u8>]) -> Result<[u8; 32], SyncError> {
        let mut sorted = leaves.to_vec();
        sorted.sort();
        let mut input = Vec::with_capacity(4 + sorted.iter().map(Vec::len).sum::<usize>());
        input.extend_from_slice(&(sorted.len() as u32).to_be_bytes());
        for leaf in &sorted {
            input.extend_from_slice(leaf);
        }
        let digest: [u8; 32] = sha2::Sha256::digest(&input).into();
        self.grant_leaf_manifests = sorted;
        self.grant_leaf_digest = Some(digest);
        self.grants_digest = Some(digest);
        self.save(vault)?;
        Ok(digest)
    }

    /// 阶段 A：确认新恢复码（R'）已离线备份；未确认前禁止 begin。
    pub fn confirm_r_prime_backed_up(&mut self, vault: &Vault) -> Result<(), SyncError> {
        self.r_prime_backed_up = true;
        self.save(vault)
    }

    /// 读 journal；不存在返回 None。
    pub fn load(vault: &Vault) -> Result<Option<Self>, SyncError> {
        let Some(blob) = vault.get_meta(ROTATION_JOURNAL_META)? else {
            return Ok(None);
        };
        let plain = dpapi::unprotect(&blob, &journal_entropy(vault))?;
        Ok(Some(Self::decode(&plain)?))
    }

    /// DPAPI 保护后写回 journal。
    pub fn save(&self, vault: &Vault) -> Result<(), SyncError> {
        let bytes = self.encode()?;
        let blob = dpapi::protect(&bytes, &journal_entropy(vault))?;
        vault.set_meta(ROTATION_JOURNAL_META, &blob)?;
        Ok(())
    }

    /// phase 只允许前进一步（Aborted 例外）。
    pub fn advance(&mut self, next: RotationPhase) -> Result<(), SyncError> {
        if next == RotationPhase::Aborted {
            self.phase = next;
            return Ok(());
        }
        if self.phase.next() != Some(next) {
            return Err(SyncError::LocalCorrupt("rotation phase 不能跳步"));
        }
        self.phase = next;
        Ok(())
    }

    /// 阶段 B：记录 relay begin 响应；调用方必须先用 `request_id` 查询（task-101 接口）。
    pub fn mark_begun(
        &mut self,
        rotation_id: [u8; 16],
        freeze_seq: u64,
        freeze_log_id: [u8; 16],
    ) -> Result<(), SyncError> {
        self.advance(RotationPhase::Begun)?;
        self.rotation_id = Some(rotation_id);
        self.freeze_seq = Some(freeze_seq);
        self.freeze_log_id = Some(freeze_log_id);
        Ok(())
    }

    /// 阶段 C：旧根 integrate 至 S、固定本地 cut L；≤S 仍有 pending 则拒绝。
    pub fn mark_integrated(
        &mut self,
        vault: &Vault,
        target_s: u64,
        local_cut: u64,
    ) -> Result<(), SyncError> {
        let cursor = menghunote_storage::sync_cursor_state(vault)?;
        if cursor.confirmed_highwater.unwrap_or(0) < target_s {
            return Err(SyncError::Unsupported(
                "rotation integrate: confirmed highwater 未覆盖 S",
            ));
        }
        if cursor.integrated_seq < target_s {
            return Err(SyncError::Unsupported(
                "rotation integrate: integrated_seq 未到 S（received 不够）",
            ));
        }
        if cursor.received_seq != cursor.integrated_seq {
            return Err(SyncError::Unsupported(
                "rotation integrate: 存在 quarantine/因果洞（received != integrated）",
            ));
        }
        // 保守：任何未加密/未发送的 outbox 都视为 ≤S 的 pending；quarantine/因果洞
        // 的细粒度检查在 task-103 接 storage 接口后补。
        if !menghunote_storage::pending_encryption(vault, 1)?.is_empty()
            || !menghunote_storage::pending_send(vault, 1)?.is_empty()
        {
            return Err(SyncError::Unsupported(
                "rotation integrate: 仍有 pending outbox，不能进入 ready",
            ));
        }
        self.advance(RotationPhase::Integrated)?;
        self.local_cut = Some(local_cut);
        Ok(())
    }

    /// 阶段 B 的 request_id 幂等句柄（响应丢失时查询用，绝不重新 begin）。
    pub fn request_id(&self) -> [u8; 16] {
        self.request_id
    }

    /// 记录新根/新管理公钥（阶段 D 用）。
    pub fn set_candidate(
        &mut self,
        root: RootSecretV1,
        manager_public: [u8; 32],
        manager_rev: u64,
        r_prime_backed_up: bool,
    ) {
        self.candidate_root = Some(root);
        self.new_manager_public = Some(manager_public);
        self.new_manager_rev = manager_rev;
        self.r_prime_backed_up = r_prime_backed_up;
    }

    /// 记录新槽（描述符 + 固定槽体字节 + slot_id）。
    pub fn set_slots(&mut self, slots: &[KeySlotV1]) -> Result<(), SyncError> {
        self.new_slot_ids = slots.iter().map(|slot| slot.descriptor.slot_id).collect();
        self.new_slot_descriptors = slots
            .iter()
            .map(|slot| slot.descriptor.encode())
            .collect::<Result<Vec<_>, _>>()?;
        self.new_slot_bodies = slots.iter().map(|slot| slot.wrapped_bytes()).collect();
        Ok(())
    }

    /// 记录 activate 的 new_manager 三键（relay `fd89cfa`：transition/old_sig/new_sig）。
    pub fn set_activate_new_manager(
        &mut self,
        transition: Vec<u8>,
        old_signature: [u8; 64],
        new_signature: [u8; 64],
        control_chain_rev: u64,
    ) {
        self.new_manager_transition = Some(transition);
        self.old_manager_signature = Some(old_signature);
        self.new_manager_signature = Some(new_signature);
        self.control_chain_rev = Some(control_chain_rev);
    }

    /// 记录一份候选基线验证回执。
    pub fn push_baseline(&mut self, baseline: JournalBaseline) {
        self.baselines.push(baseline);
    }

    // ---------------------------------------------------------------- CDE

    fn encode(&self) -> Result<Vec<u8>, SyncError> {
        let mut fields = vec![
            (0, cbor::uint(self.phase.code())),
            (1, cbor::bytes(self.request_id.to_vec())),
            (2, cbor::bytes(self.body_digest.to_vec())),
            (3, cbor::uint(self.from_epoch)),
            (4, cbor::uint(self.to_epoch)),
            (8, cbor::uint(self.control_rev)),
            (9, cbor::bytes(self.control_hash.to_vec())),
            (12, cbor::uint(self.new_manager_rev)),
            (13, cbor::boolean(self.r_prime_backed_up)),
            (
                14,
                cbor::array(
                    self.new_slot_descriptors
                        .iter()
                        .map(|value| cbor::bytes(value.clone()))
                        .collect(),
                ),
            ),
            (
                15,
                cbor::array(
                    self.new_slot_bodies
                        .iter()
                        .map(|value| cbor::bytes(value.clone()))
                        .collect(),
                ),
            ),
            (
                16,
                cbor::array(
                    self.new_slot_ids
                        .iter()
                        .map(|value| cbor::bytes(value.to_vec()))
                        .collect(),
                ),
            ),
            (
                17,
                cbor::array(self.baselines.iter().map(baseline_cde).collect()),
            ),
            (21, cbor::uint(u64::from(self.uploaded_objects))),
            (22, cbor::uint(u64::from(self.total_objects))),
            (23, cbor::uint(u64::from(self.repacked_outbox))),
            (24, cbor::uint(u64::from(self.total_outbox))),
        ];
        if let Some(rotation_id) = self.rotation_id {
            fields.push((5, cbor::bytes(rotation_id.to_vec())));
        }
        if let Some(freeze_seq) = self.freeze_seq {
            fields.push((6, cbor::uint(freeze_seq)));
        }
        if let Some(log_id) = self.freeze_log_id {
            fields.push((7, cbor::bytes(log_id.to_vec())));
        }
        if let Some(root) = &self.candidate_root {
            fields.push((10, cbor::bytes(root.encode()?)));
        }
        if let Some(public) = self.new_manager_public {
            fields.push((11, cbor::bytes(public.to_vec())));
        }
        if let Some(digest) = self.member_set_digest {
            fields.push((18, cbor::bytes(digest.to_vec())));
        }
        if let Some(digest) = self.grants_digest {
            fields.push((19, cbor::bytes(digest.to_vec())));
        }
        if !self.grant_leaf_manifests.is_empty() {
            fields.push((
                33,
                cbor::array(
                    self.grant_leaf_manifests
                        .iter()
                        .map(|leaf| cbor::bytes(leaf.clone()))
                        .collect(),
                ),
            ));
        }
        if let Some(digest) = self.grant_leaf_digest {
            fields.push((34, cbor::bytes(digest.to_vec())));
        }
        if let Some(digest) = self.asset_set_digest {
            fields.push((20, cbor::bytes(digest.to_vec())));
        }
        if let Some(cut) = self.local_cut {
            fields.push((25, cbor::uint(cut)));
        }
        if let Some(transition) = &self.new_manager_transition {
            fields.push((26, cbor::bytes(transition.clone())));
        }
        if let Some(signature) = self.old_manager_signature {
            fields.push((27, cbor::bytes(signature.to_vec())));
        }
        if let Some(signature) = self.new_manager_signature {
            fields.push((28, cbor::bytes(signature.to_vec())));
        }
        if let Some(rev) = self.control_chain_rev {
            fields.push((29, cbor::uint(rev)));
        }
        if let Some(instance) = self.pinned_instance_id {
            fields.push((30, cbor::bytes(instance.to_vec())));
        }
        if let Some(log_id) = self.pinned_log_id {
            fields.push((31, cbor::bytes(log_id.to_vec())));
        }
        if let Some(hash) = self.pinned_control_hash {
            fields.push((32, cbor::bytes(hash.to_vec())));
        }
        fields.sort_by_key(|(key, _)| *key);
        cbor::encode(fields)
    }

    fn decode(bytes: &[u8]) -> Result<Self, SyncError> {
        let map = cbor::decode(bytes)?;
        let phase = RotationPhase::from_code(cbor::uint_field(&map, 0)?)?;
        let request_id = cbor::bytes_fixed(&map, 1)?;
        let body_digest = cbor::bytes_fixed(&map, 2)?;
        let from_epoch = cbor::uint_field(&map, 3)?;
        let to_epoch = cbor::uint_field(&map, 4)?;
        let rotation_id = cbor::opt_bytes_field(&map, 5)?
            .map(|value| fixed16(&value))
            .transpose()?;
        let freeze_seq = cbor::opt_uint_field(&map, 6)?;
        let freeze_log_id = cbor::opt_bytes_field(&map, 7)?
            .map(|value| fixed16(&value))
            .transpose()?;
        let control_rev = cbor::uint_field(&map, 8)?;
        let control_hash = cbor::bytes_fixed(&map, 9)?;
        let candidate_root = cbor::opt_bytes_field(&map, 10)?
            .map(|value| RootSecretV1::decode(&value))
            .transpose()?;
        let new_manager_public = cbor::opt_bytes_field(&map, 11)?
            .map(|value| fixed32(&value))
            .transpose()?;
        let new_manager_rev = cbor::uint_field(&map, 12)?;
        let r_prime_backed_up = cbor::bool_field(&map, 13)?;
        let new_slot_descriptors = cbor::array_field(&map, 14)?
            .iter()
            .map(value_bytes)
            .collect::<Result<Vec<_>, _>>()?;
        let new_slot_bodies = cbor::array_field(&map, 15)?
            .iter()
            .map(value_bytes)
            .collect::<Result<Vec<_>, _>>()?;
        let new_slot_ids = cbor::array_field(&map, 16)?
            .iter()
            .map(|value| fixed32(&value_bytes(value)?))
            .collect::<Result<Vec<_>, _>>()?;
        let baselines = cbor::array_field(&map, 17)?
            .iter()
            .map(decode_baseline)
            .collect::<Result<Vec<_>, _>>()?;
        let member_set_digest = cbor::opt_bytes_field(&map, 18)?
            .map(|value| fixed32(&value))
            .transpose()?;
        let grants_digest = cbor::opt_bytes_field(&map, 19)?
            .map(|value| fixed32(&value))
            .transpose()?;
        let grant_leaf_manifests = cbor::opt_array_field(&map, 33)?
            .unwrap_or_default()
            .iter()
            .map(cbor::value_bytes)
            .collect::<Result<Vec<_>, _>>()?;
        let grant_leaf_digest = cbor::opt_bytes_field(&map, 34)?
            .map(|value| fixed32(&value))
            .transpose()?;
        let asset_set_digest = cbor::opt_bytes_field(&map, 20)?
            .map(|value| fixed32(&value))
            .transpose()?;
        let uploaded_objects = u32::try_from(cbor::uint_field(&map, 21)?)
            .map_err(|_| SyncError::LocalCorrupt("journal uploaded_objects 溢出"))?;
        let total_objects = u32::try_from(cbor::uint_field(&map, 22)?)
            .map_err(|_| SyncError::LocalCorrupt("journal total_objects 溢出"))?;
        let repacked_outbox = u32::try_from(cbor::uint_field(&map, 23)?)
            .map_err(|_| SyncError::LocalCorrupt("journal repacked 溢出"))?;
        let total_outbox = u32::try_from(cbor::uint_field(&map, 24)?)
            .map_err(|_| SyncError::LocalCorrupt("journal total_outbox 溢出"))?;
        let local_cut = cbor::opt_uint_field(&map, 25)?;
        let new_manager_transition = cbor::opt_bytes_field(&map, 26)?;
        let old_manager_signature = cbor::opt_bytes_field(&map, 27)?
            .map(|value| fixed64(&value))
            .transpose()?;
        let new_manager_signature = cbor::opt_bytes_field(&map, 28)?
            .map(|value| fixed64(&value))
            .transpose()?;
        let control_chain_rev = cbor::opt_uint_field(&map, 29)?;
        let pinned_instance_id = cbor::opt_bytes_field(&map, 30)?
            .map(|value| fixed16(&value))
            .transpose()?;
        let pinned_log_id = cbor::opt_bytes_field(&map, 31)?
            .map(|value| fixed16(&value))
            .transpose()?;
        let pinned_control_hash = cbor::opt_bytes_field(&map, 32)?
            .map(|value| fixed32(&value))
            .transpose()?;
        Ok(Self {
            phase,
            request_id,
            body_digest,
            from_epoch,
            to_epoch,
            rotation_id,
            freeze_seq,
            freeze_log_id,
            control_rev,
            control_hash,
            candidate_root,
            new_manager_public,
            new_manager_rev,
            new_manager_transition,
            old_manager_signature,
            new_manager_signature,
            control_chain_rev,
            pinned_instance_id,
            pinned_log_id,
            pinned_control_hash,
            r_prime_backed_up,
            new_slot_descriptors,
            new_slot_bodies,
            new_slot_ids,
            baselines,
            member_set_digest,
            grants_digest,
            grant_leaf_manifests,
            grant_leaf_digest,
            asset_set_digest,
            uploaded_objects,
            total_objects,
            repacked_outbox,
            total_outbox,
            local_cut,
        })
    }
}

fn journal_entropy(_vault: &Vault) -> Vec<u8> {
    JOURNAL_DOMAIN.to_vec()
}

fn begin_body_digest(from_epoch: u64, request_id: &[u8; 16]) -> [u8; 32] {
    let mut input = Vec::with_capacity(8 + 16);
    input.extend_from_slice(&from_epoch.to_be_bytes());
    input.extend_from_slice(request_id);
    Sha256::digest(&input).into()
}

fn baseline_cde(baseline: &JournalBaseline) -> cbor::CdeValue {
    cbor::CdeValue::Map(vec![
        (0, cbor::bytes(baseline.checkpoint_id.to_vec())),
        (1, cbor::uint(baseline.covered_seq)),
        (2, cbor::text(baseline.cursor.clone())),
        (3, cbor::bytes(baseline.manifest_object_id.to_vec())),
        (4, cbor::bytes(baseline.manifest_digest.to_vec())),
        (5, cbor::bytes(baseline.refs_digest.to_vec())),
        (6, cbor::boolean(baseline.verified)),
    ])
}

fn value_bytes(value: &cbor::CdeValue) -> Result<Vec<u8>, SyncError> {
    match value {
        cbor::CdeValue::Bytes(bytes) => Ok(bytes.clone()),
        _ => Err(SyncError::LocalCorrupt("journal 数组元素不是 bytes")),
    }
}

fn decode_baseline(value: &cbor::CdeValue) -> Result<JournalBaseline, SyncError> {
    let map = cbor::value_map(value)?;
    Ok(JournalBaseline {
        checkpoint_id: cbor::bytes_fixed(&map, 0)?,
        covered_seq: cbor::uint_field(&map, 1)?,
        cursor: cbor::text_field(&map, 2)?,
        manifest_object_id: cbor::bytes_fixed(&map, 3)?,
        manifest_digest: cbor::bytes_fixed(&map, 4)?,
        refs_digest: cbor::bytes_fixed(&map, 5)?,
        verified: cbor::bool_field(&map, 6)?,
    })
}

fn fixed16(bytes: &[u8]) -> Result<[u8; 16], SyncError> {
    bytes
        .try_into()
        .map_err(|_| SyncError::LocalCorrupt("journal 字段长度不是 16B"))
}

fn fixed32(bytes: &[u8]) -> Result<[u8; 32], SyncError> {
    bytes
        .try_into()
        .map_err(|_| SyncError::LocalCorrupt("journal 字段长度不是 32B"))
}

fn fixed64(bytes: &[u8]) -> Result<[u8; 64], SyncError> {
    bytes
        .try_into()
        .map_err(|_| SyncError::LocalCorrupt("journal 签名长度不是 64B"))
}

/// 新秘密的固定长度（password 必须由用户新给，R'=random32）。
pub const NEW_RECOVERY_SEED_LEN: usize = 32;

/// 生成 R'（调用方负责离线备份并置 `r_prime_backed_up`）。
pub fn generate_r_prime() -> Result<Secret32, SyncError> {
    Ok(Secret32::from_array(crate::random::array(
        &mut crate::random::OsRandom,
    )?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use menghunote_storage::{create_body_doc, set_confirmed_highwater};

    fn fresh() -> (tempfile::TempDir, Vault) {
        let base = std::env::var("CARGO_TARGET_TMPDIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from("target/tmp"));
        let base = base.as_path();
        std::fs::create_dir_all(base).expect("target/tmp");
        let dir = tempfile::Builder::new()
            .prefix("rotation-journal-")
            .tempdir_in(base)
            .expect("临时目录");
        let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("vault");
        (dir, vault)
    }

    fn frozen(vault: &Vault, request_id: [u8; 16]) -> RotationJournal {
        RotationJournal::freeze(vault, 1, request_id, 7, [0x11; 32]).expect("freeze")
    }

    #[test]
    fn journal_roundtrips_with_secrets_and_activate_fields() {
        let (_dir, vault) = fresh();
        let mut journal = frozen(&vault, [0x21; 16]);
        let request_id = journal.request_id();
        journal
            .mark_begun([0x31; 16], 9, [0x41; 16])
            .expect("begun");
        let root = RootSecretV1::new([0x55; 16], 2, Secret32::from_array([0x66; 32]));
        journal.set_candidate(root.clone(), [0x77; 32], 2, true);
        journal.set_activate_new_manager(vec![1, 2, 3], [0x88; 64], [0x99; 64], 3);
        journal.save(&vault).expect("save");
        let loaded = RotationJournal::load(&vault)
            .expect("load")
            .expect("journal");
        assert_eq!(loaded.phase, RotationPhase::Begun);
        assert_eq!(loaded.request_id(), request_id);
        assert_eq!(loaded.rotation_id, Some([0x31; 16]));
        assert_eq!(loaded.freeze_seq, Some(9));
        assert!(loaded.r_prime_backed_up);
        assert_eq!(
            loaded
                .candidate_root
                .expect("root")
                .encode()
                .expect("encode"),
            root.encode().expect("root encode")
        );
        assert_eq!(loaded.new_manager_transition, Some(vec![1, 2, 3]));
        assert_eq!(loaded.old_manager_signature, Some([0x88; 64]));
        assert_eq!(loaded.control_chain_rev, Some(3));
    }

    #[test]
    fn phase_cannot_skip_and_request_id_is_stable() {
        let (_dir, vault) = fresh();
        let mut journal = frozen(&vault, [0x22; 16]);
        let first = journal.request_id();
        assert_eq!(first, journal.request_id());
        assert!(
            journal.advance(RotationPhase::Integrated).is_err(),
            "Frozen 不能直接到 Integrated"
        );
        journal
            .mark_begun([0x32; 16], 4, [0x42; 16])
            .expect("begun");
        assert_eq!(journal.phase, RotationPhase::Begun);
    }

    #[test]
    fn integrate_rejects_pending_outbox_before_s() {
        let (_dir, vault) = fresh();
        set_confirmed_highwater(&vault, 5).expect("highwater");
        vault
            .set_meta(
                menghunote_storage::META_SYNC_INTEGRATED_SEQ,
                &5u64.to_be_bytes(),
            )
            .expect("integrated");
        create_body_doc(&vault, "note-1").expect("doc");
        let body = menghunote_storage::BodyDoc::create("note-1", 1).expect("body");
        body.insert_scalars(0, "pending").expect("text");
        let pending = body
            .pending_update(&loro::VersionVector::default())
            .expect("pending update");
        menghunote_storage::append_updates(
            &vault,
            "note-1",
            0,
            &[menghunote_storage::UpdateRecord::new("msg-1", pending)],
        )
        .expect("local update");
        menghunote_storage::enqueue_sync_intent(
            &vault,
            "msg-1",
            "batch-1",
            "note-1",
            1,
            1,
            &[0xAA; 32],
        )
        .expect("pending outbox");
        let mut journal = frozen(&vault, [0x23; 16]);
        journal
            .mark_begun([0x33; 16], 5, [0x43; 16])
            .expect("begun");
        assert!(
            journal.mark_integrated(&vault, 5, 5).is_err(),
            "≤S 仍有 pending 时不能 ready"
        );
        assert_eq!(journal.phase, RotationPhase::Begun);
    }

    #[test]
    fn integrate_requires_highwater_to_cover_s() {
        let (_dir, vault) = fresh();
        set_confirmed_highwater(&vault, 3).expect("highwater");
        let mut journal = frozen(&vault, [0x24; 16]);
        journal
            .mark_begun([0x34; 16], 5, [0x44; 16])
            .expect("begun");
        assert!(journal.mark_integrated(&vault, 5, 5).is_err());
    }

    #[test]
    fn received_ahead_of_integrated_blocks_ready() {
        let (_dir, vault) = fresh();
        set_confirmed_highwater(&vault, 5).expect("highwater");
        vault
            .set_meta(
                menghunote_storage::META_SYNC_RECEIVED_SEQ,
                &6u64.to_be_bytes(),
            )
            .expect("received");
        vault
            .set_meta(
                menghunote_storage::META_SYNC_INTEGRATED_SEQ,
                &5u64.to_be_bytes(),
            )
            .expect("integrated");
        let mut journal = frozen(&vault, [0x26; 16]);
        journal
            .mark_begun([0x36; 16], 5, [0x46; 16])
            .expect("begun");
        assert!(
            journal.mark_integrated(&vault, 5, 5).is_err(),
            "received > integrated（quarantine/因果洞）必须拒绝 ready"
        );
        assert_eq!(journal.phase, RotationPhase::Begun);
    }
}
