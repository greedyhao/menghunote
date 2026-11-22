//! 轮换阶段 G/H：activate、grant 解根校验与本地原子推广。
//!
//! 只实现“发起设备”的路径；其它设备应用与 outbox 重打包见后续阶段 I/J。
//!
//! 推广的硬约束（本次修正）：
//!
//! - 身份里的 `manager_seed` 必须是**新管理 key** 的 seed（由 R' 派生），绝不能继续保存
//!   旧 seed 却 pin 新 manager；
//! - 旧 epoch 根按 epoch 保留到本地 outbox 重包完成（[`crate::identity::HistoricalRootV1`]），
//!   rename/重启后仍能解旧 parts；
//! - `control_rev`（控制链）与 `new_manager_rev`（管理 key）分开，不互相写入；
//! - journal 只能 `→ Promoted → Completed`，不能跳步。

use std::fs;
use std::io::Write as _;
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::api_version::RelayApi;
use crate::device::{DeviceHpkeKey, DeviceSigningKey, ManagementKey};
use crate::error::SyncError;
use crate::hpke::open_epoch_grant;
use crate::http_relay::HttpTransport;
use crate::identity::{HistoricalRootV1, IdentityRotationStateV1, LocalIdentityV1};
use crate::keys::{RootSecretV1, Secret32};
use crate::recovery_enroll::{decode, device_token, management_token, send};
use crate::relay::RotationView;
use crate::rotation_journal::{RotationJournal, RotationPhase};
use crate::{DeviceId, VaultId};
use menghunote_storage::Vault;
use menghunote_sync_proto::{GrantAadV1, RotationActivateBodyV1};

/// 阶段 G 的 activate 材料。
#[derive(Clone)]
pub struct ActivationInput {
    pub rotation_id: [u8; 16],
    pub request_id: [u8; 16],
    pub expected_control_rev: u64,
    pub primary_checkpoint: [u8; 16],
    pub fallback_checkpoint: [u8; 16],
    pub member_cert_digest: [u8; 32],
    pub slot_set_digest: [u8; 32],
    pub grants_digest: [u8; 32],
    pub asset_set_digest: [u8; 32],
    pub new_manager: Option<menghunote_sync_proto::NewManagerV1>,
    /// 候选根（只用于推广与 grant 校验，不上 wire）。
    pub candidate_root: RootSecretV1,
    pub new_manager_public: [u8; 32],
    pub new_manager_rev: u64,
    /// 四个 digest 的 canonical 绑定版本。
    pub pv: u64,
}

impl ActivationInput {
    fn body(&self) -> RotationActivateBodyV1 {
        RotationActivateBodyV1 {
            new_manager: self.new_manager.clone(),
            primary_checkpoint: Some(self.primary_checkpoint),
            fallback_checkpoint: Some(self.fallback_checkpoint),
            member_cert_digest: Some(self.member_cert_digest),
            grants_digest: Some(self.grants_digest),
            slot_set_digest: Some(self.slot_set_digest),
            asset_set_digest: Some(self.asset_set_digest),
        }
    }
}

/// activate 成功后的 relay 结果（响应键 0–6，不是 rotation view）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivatedOutcome {
    pub epoch: u64,
    pub control_rev: u64,
    pub auth_rev: u64,
    pub member_set_hash: [u8; 32],
    pub primary_checkpoint: [u8; 16],
    pub fallback_checkpoint: [u8; 16],
    pub activation_commit: Vec<u8>,
}

/// 阶段 G：management activate；超时只查 `rotations/current`，不 abort。
#[allow(clippy::too_many_arguments)]
pub fn activate_http<T: HttpTransport>(
    transport: &mut T,
    api: &RelayApi,
    vault_id: VaultId,
    instance_id: &[u8; 16],
    manager: &ManagementKey,
    input: &ActivationInput,
    now: u64,
) -> Result<ActivatedOutcome, SyncError> {
    let body = input.body();
    let signed = crate::rotation::sign_activate(
        manager,
        instance_id,
        vault_id,
        input.rotation_id,
        input.request_id,
        input.expected_control_rev,
        &body,
        api.version(),
    )?;
    let (token, _) = management_token(transport, api, vault_id, manager, now)?;
    let signed_value = crate::cbor::CdeValue::Map(vec![
        (0, crate::cbor::bytes(signed.body.clone())),
        (1, crate::cbor::bytes(signed.signature.to_vec())),
    ]);
    let path = api.url(&api.vault_path(
        &vault_id,
        &format!(
            "/rotations/{}/activate",
            crate::base64url::encode(&input.rotation_id)
        ),
    ));
    let response = send(
        transport,
        "POST",
        path,
        Some(&token),
        crate::cbor::encode(vec![
            (0, signed_value),
            (1, crate::cbor::bytes(input.request_id.to_vec())),
            (2, crate::cbor::uint(input.expected_control_rev)),
        ])?,
    );
    match response {
        Ok(response) => {
            let fields = decode(&response)?;
            parse_activated_outcome(&fields)
        }
        Err(error @ SyncError::Transport(_)) => {
            let view = query_current_rotation(transport, api, vault_id, manager, now)?;
            if view.state == "activated" {
                Ok(ActivatedOutcome {
                    epoch: view.to_epoch,
                    control_rev: view.control_rev,
                    auth_rev: view.auth_rev,
                    member_set_hash: view.member_set_hash.unwrap_or_default(),
                    primary_checkpoint: view.primary_checkpoint.unwrap_or_default(),
                    fallback_checkpoint: view.fallback_checkpoint.unwrap_or_default(),
                    activation_commit: view.activation_commit.unwrap_or_default(),
                })
            } else {
                Err(error)
            }
        }
        Err(error) => Err(error),
    }
}

fn parse_activated_outcome(fields: &crate::cbor::Fields) -> Result<ActivatedOutcome, SyncError> {
    Ok(ActivatedOutcome {
        epoch: crate::cbor::uint_field(fields, 0)?,
        control_rev: crate::cbor::uint_field(fields, 1)?,
        auth_rev: crate::cbor::uint_field(fields, 2)?,
        member_set_hash: crate::cbor::bytes_fixed(fields, 3)?,
        primary_checkpoint: crate::cbor::bytes_fixed(fields, 4)?,
        fallback_checkpoint: crate::cbor::bytes_fixed(fields, 5)?,
        activation_commit: crate::cbor::bytes_field(fields, 6)?,
    })
}

fn query_current_rotation<T: HttpTransport>(
    transport: &mut T,
    api: &RelayApi,
    vault_id: VaultId,
    manager: &ManagementKey,
    now: u64,
) -> Result<RotationView, SyncError> {
    let (token, _) = management_token(transport, api, vault_id, manager, now)?;
    let response = send(
        transport,
        "GET",
        api.url(&api.vault_path(&vault_id, "/rotations/current")),
        Some(&token),
        Vec::new(),
    )?;
    let fields = decode(&response)?;
    let view = crate::cbor::value_map(crate::cbor::field(&fields, 0)?)?;
    parse_activated_view(&view)
}

fn parse_activated_view(view: &crate::cbor::Fields) -> Result<RotationView, SyncError> {
    Ok(RotationView {
        rotation_id: crate::cbor::bytes_fixed(view, 0)?,
        state: crate::cbor::text_field(view, 1)?,
        from_epoch: crate::cbor::uint_field(view, 2)?,
        to_epoch: crate::cbor::uint_field(view, 3)?,
        freeze_seq: crate::cbor::uint_field(view, 4)?,
        revoked_count: crate::cbor::uint_field(view, 5)?,
        member_set_hash: None,
        grants_count: crate::cbor::uint_field(view, 7)?,
        grants_digest: None,
        published_checkpoints: crate::cbor::uint_field(view, 9)?,
        control_rev: crate::cbor::uint_field(view, 10)?,
        auth_rev: crate::cbor::uint_field(view, 11)?,
        manager_rev: crate::cbor::uint_field(view, 12)?,
        from_epoch_revoked_once: cbor_uint(view, 13) != 0,
        freeze_asset_count: cbor_uint(view, 15),
        freeze_asset_digest: None,
        member_cert_digest: None,
        primary_checkpoint: None,
        fallback_checkpoint: None,
        activation_commit: None,
        activated_at: cbor_opt_uint(view, 21),
        activated_control_rev: cbor_opt_uint(view, 22),
        candidate_count: 0,
        candidate_ready: false,
        reservation_id: None,
    })
}

fn cbor_uint(fields: &crate::cbor::Fields, key: u64) -> u64 {
    crate::cbor::uint_field(fields, key).unwrap_or(0)
}

fn cbor_opt_uint(fields: &crate::cbor::Fields, key: u64) -> Option<u64> {
    crate::cbor::opt_uint_field(fields, key).ok().flatten()
}

/// 阶段 H：读回自己的最新 grant，解封必须等于 candidate root。
#[allow(clippy::too_many_arguments)]
pub fn verify_latest_grant_root<T: HttpTransport>(
    transport: &mut T,
    api: &RelayApi,
    vault_id: VaultId,
    device_id: DeviceId,
    signing_key: &DeviceSigningKey,
    hpke_key: &DeviceHpkeKey,
    expected_root: &RootSecretV1,
    now: u64,
) -> Result<(), SyncError> {
    let token = device_token(transport, api, vault_id, device_id, signing_key, now)?;
    let response = send(
        transport,
        "GET",
        api.url(&api.vault_path(&vault_id, "/devices/self/grants/latest")),
        Some(&token),
        Vec::new(),
    )?;
    let fields = decode(&response)?;
    let epoch = crate::cbor::uint_field(&fields, 0)?;
    let grant_device: [u8; 16] = crate::cbor::bytes_fixed(&fields, 1)?;
    let enc: [u8; 32] = crate::cbor::bytes_fixed(&fields, 3)?;
    let ciphertext = crate::cbor::bytes_field(&fields, 4)?;
    let signed_header = crate::cbor::bytes_field(&fields, 5)?;
    if grant_device != device_id || expected_root.key_epoch != epoch || signed_header.len() < 64 {
        return Err(SyncError::RelayWire("latest grant 与候选 epoch/设备不符"));
    }
    let aad_len = signed_header.len() - 64;
    let aad = GrantAadV1::decode(&signed_header[..aad_len])?;
    if aad.epoch != epoch
        || aad.recipient_device_id != device_id
        || aad.recipient_x25519_pub != hpke_key.public_key()
    {
        return Err(SyncError::RelayWire("latest grant AAD 与本地设备不符"));
    }
    let opened = open_epoch_grant(hpke_key.secret(), &enc, &aad, &ciphertext)?;
    if opened.encode()? != expected_root.encode()? {
        return Err(SyncError::Object(
            "latest grant 解出的 root 不是 candidate root，拒绝推广",
        ));
    }
    Ok(())
}

/// 本次 activation 已经由 T / `ActivationCommitV1` 验证过的身份 pin 值（评审 0003 P1-3）。
///
/// 推广必须写入这些值，而不是保留旧身份的 `control_rev`/instance：
///
/// - `instance_id`：activation T 与 `/state` 共同绑定的 relay instance；
/// - `control_rev`：激活提交里的控制链 rev（**不是**管理 key rev）；
/// - `activation_hash`：`SHA256(CDE(ActivationCommitV1))`，即本次激活的认证摘要。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthenticatedActivation {
    /// 已验证的 relay instance（T 的 `instance_id`）。
    pub instance_id: [u8; 16],
    /// 已验证的控制链 rev（`ActivationCommitV1.control_rev` == T.expected_control_rev）。
    pub control_rev: u64,
    /// 已验证的 activation 摘要：`SHA256(CDE(ActivationCommitV1))`。
    pub activation_hash: [u8; 32],
}

/// 推广后写入的 authenticated pin（`vault.meta`，与身份旁路状态同 epoch 语义）。
///
/// 它不是身份 BLOB 的一部分（身份字段清单被冻结为 0..11），而是与身份同源的旁路记录：
/// `epoch` 必须等于身份 `current_epoch` 才被视为"当前 pin"，否则是崩溃残留（忽略）。
pub const AUTHENTICATED_PIN_META: &str = "sync.identity.authenticated_pin";

/// 推广后 durable 的认证 pin。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthenticatedPinV1 {
    /// 与身份 `current_epoch` 对齐；不一致即崩溃残留。
    pub epoch: u64,
    /// 控制链 rev（不是管理 key rev）。
    pub control_rev: u64,
    /// 已验证的 relay instance。
    pub instance_id: [u8; 16],
    /// 本次 activation 的 `SHA256(CDE(ActivationCommitV1))`。
    pub activation_hash: [u8; 32],
}

impl AuthenticatedPinV1 {
    /// 读 pin；不存在/空值返回 `None`。
    pub fn load(vault: &Vault) -> Result<Option<Self>, SyncError> {
        let Some(blob) = vault.get_meta(AUTHENTICATED_PIN_META)? else {
            return Ok(None);
        };
        if blob.is_empty() {
            return Ok(None);
        }
        let fields = crate::cbor::decode(&blob)?;
        Ok(Some(Self {
            epoch: crate::cbor::uint_field(&fields, 0)?,
            control_rev: crate::cbor::uint_field(&fields, 1)?,
            instance_id: crate::cbor::bytes_fixed(&fields, 2)?,
            activation_hash: crate::cbor::bytes_fixed(&fields, 3)?,
        }))
    }

    /// 读"当前身份的" pin：`epoch` 不匹配时返回 `None`（崩溃残留，旧身份仍权威）。
    pub fn load_current(
        vault: &Vault,
        identity: &LocalIdentityV1,
    ) -> Result<Option<Self>, SyncError> {
        Ok(Self::load(vault)?.filter(|pin| pin.epoch == identity.current_epoch))
    }

    fn save(&self, vault: &Vault) -> Result<(), SyncError> {
        let blob = crate::cbor::encode(vec![
            (0, crate::cbor::uint(self.epoch)),
            (1, crate::cbor::uint(self.control_rev)),
            (2, crate::cbor::bytes(self.instance_id.to_vec())),
            (3, crate::cbor::bytes(self.activation_hash.to_vec())),
        ])?;
        vault.set_meta(AUTHENTICATED_PIN_META, &blob)?;
        Ok(())
    }
}

/// 父目录 durability 的处理结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectoryDurability {
    /// 已对父目录做 `fsync`（Unix）。
    Synced,
    /// 平台不提供目录 fsync（Windows 的 std 无法打开带备份语义的目录句柄）。
    PlatformUnsupported,
}

/// 对 `path` 的父目录做"可用"的 durability 处理。
///
/// - Unix：`open(dir)` + `sync_all()`（目录 `fsync`），让 rename 的目录项落盘；
/// - Windows：标准库没有暴露 `FILE_FLAG_BACKUP_SEMANTICS` 的目录句柄，无法对目录
///   调用 `FlushFileBuffers`；这里**明确**返回 [`DirectoryDurability::PlatformUnsupported`]。
///   因此 Windows 上的可验证耐久边界是：
///   **临时文件在 rename 前已 `File::sync_all`（等价 `FlushFileBuffers`）+ rename 原子提交**；
///   掉电后目录项是否立即持久不在本标准库能力范围内（NTFS 日志通常会在重放时恢复，
///   但不得把它当作保证）。测试必须显式断言这条平台差异。
pub fn sync_parent_directory(path: &Path) -> Result<DirectoryDurability, SyncError> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    #[cfg(unix)]
    {
        fs::File::open(parent)
            .and_then(|dir| dir.sync_all())
            .map_err(|error| SyncError::Io(format!("父目录 fsync 失败: {error}")))?;
        Ok(DirectoryDurability::Synced)
    }
    #[cfg(not(unix))]
    {
        let _ = parent;
        Ok(DirectoryDurability::PlatformUnsupported)
    }
}

/// 写临时身份并在 rename 之前 `File::sync_all`（Windows = `FlushFileBuffers`）。
fn write_identity_temp(path: &Path, blob: &[u8]) -> Result<(), SyncError> {
    let mut file = fs::File::create(path)
        .map_err(|error| SyncError::Io(format!("创建临时身份失败: {error}")))?;
    file.write_all(blob)
        .map_err(|error| SyncError::Io(format!("写临时身份失败: {error}")))?;
    // 必须在 rename 之前完成：rename 成功不等于内容已经落盘（Windows 尤其如此）。
    file.sync_all()
        .map_err(|error| SyncError::Io(format!("临时身份 sync_all 失败: {error}")))?;
    Ok(())
}

/// 以 `rename` 为唯一提交点的耐久替换。
///
/// 失败语义：rename 之前任何失败都保留旧身份文件不变；rename 是唯一提交点。
/// 提交之后再刷新父目录/文件属于"尽力而为"（Windows 无目录 fsync），不会回滚已提交的身份
/// ——回滚会引入第二个非原子提交点，反而扩大半新半旧窗口。
fn replace_identity_file(temp: &Path, target: &Path) -> Result<(), SyncError> {
    // 提交前刷新父目录（Unix 上把临时文件目录项落盘）；失败时旧身份仍未被替换。
    sync_parent_directory(temp)?;
    fs::rename(temp, target)
        .map_err(|error| SyncError::Io(format!("身份 rename 失败: {error}")))?;
    // 提交后再次刷新父目录：Unix 上补齐目录项持久；Windows 明确不支持（已知边界）。
    let _ = sync_parent_directory(target);
    Ok(())
}

/// 本地推广所需的轮换材料。
///
/// 与旧 6 参数入口相比，这里显式分开了三个语义（此前被混在一起）：
///
/// - `candidate_root`：新 epoch 的根（覆盖身份 `current_root`）；
/// - `new_manager_seed`：由 R' 派生的新管理私钥种子（只有管理设备有；B 侧为 `None`）；
/// - `control_rev` / `new_manager_rev`：**控制链 rev** 与**管理 key rev** 是两个独立计数，
///   绝不允许互相写入（旧实现把 `new_manager_rev` 写进了 `control_rev`）。
#[derive(Clone)]
pub struct PromotionMaterial {
    /// 新 epoch 根（必须高于身份当前 epoch）。
    pub candidate_root: RootSecretV1,
    /// 新管理 key 的 seed（由 R' 派生）；非管理设备为 `None`。
    pub new_manager_seed: Option<Secret32>,
    /// 新管理公钥（relay 双签后生效的那把）。
    pub new_manager_public: [u8; 32],
    /// 管理 key rev。
    pub new_manager_rev: u64,
    /// 控制链 rev（activate/restart 回执里的值，不是管理 rev）。
    pub control_rev: u64,
    /// 本次 activation 的已验证认证值；B apply 路径必须提供，A 管理路径可为 `None`。
    pub authentication: Option<AuthenticatedActivation>,
}

impl PromotionMaterial {
    /// 用 activate/restart 回执构造推广材料：`control_rev` 取回执里的控制链 rev。
    #[must_use]
    pub fn from_activated(
        outcome: &ActivatedOutcome,
        candidate_root: RootSecretV1,
        new_manager_seed: Option<Secret32>,
        new_manager_public: [u8; 32],
        new_manager_rev: u64,
    ) -> Self {
        Self {
            candidate_root,
            new_manager_seed,
            new_manager_public,
            new_manager_rev,
            control_rev: outcome.control_rev,
            // A 管理设备手上没有"已验证 instance/hash"的 apply 上下文；
            // 推广仍更新 epoch/root/manager/control_rev，pin 由 apply 路径补齐。
            authentication: None,
        }
    }
}

/// 管理设备专用：由 R' 派生新管理 seed，并与公钥一起组装推广材料。
///
/// 这是"推广后 `manager_seed` 必须对应新 manager 公钥"的唯一正确来源：seed 走
/// `HKDF(R', vault_id, management-ed25519)`，公钥从同一 seed 派生，调用方不需要也不允许
/// 自己拼接。
pub fn promotion_material_from_recovery(
    outcome: &ActivatedOutcome,
    candidate_root: RootSecretV1,
    recovery_seed: &crate::recovery::RecoverySeed,
    vault_id: &VaultId,
    new_manager_rev: u64,
) -> Result<PromotionMaterial, SyncError> {
    let seed = crate::recovery::management_seed(recovery_seed, vault_id)?;
    let public = ManagementKey::from_seed(seed.clone()).public_key();
    Ok(PromotionMaterial::from_activated(
        outcome,
        candidate_root,
        Some(seed),
        public,
        new_manager_rev,
    ))
}

/// 阶段 H：原子推广 DPAPI 身份 + 历史根/管理 rev 旁路状态 + authenticated pin + repack 屏障。
///
/// 写入顺序（即崩溃/掉电语义，评审 0003 P1-3）：
///
/// 1. 全部校验先做完（epoch/归属/新管理 seed 与公钥一致/认证 pin 与 control_rev 一致），
///    失败时**不写任何字节**；
/// 2. 写临时身份 BLOB 并 `File::sync_all`（Windows 等价 `FlushFileBuffers`），
///    保证 rename 之前内容已落盘；
/// 3. 写旁路状态（历史根 + 控制链/管理 rev，带新 epoch）；
/// 4. 写 authenticated pin（instance/epoch/control_rev/activation hash，带新 epoch）；
/// 5. 置 `sync.repack_required=1`；
/// 6. 刷新父目录后 `rename` 临时身份 → 正式路径（**唯一提交点**）；提交后再次刷新
///    父目录（Unix 有目录 fsync；Windows 无目录 fsync，边界见
///    [`sync_parent_directory`]）。
///
/// 若在 3–6 之间崩溃：旁路状态/pin 的 epoch 领先旧身份，[`IdentityRotationStateV1::load_current`]
/// 与 [`AuthenticatedPinV1::load_current`] 会因 epoch 不匹配忽略它们；旧身份文件未动，
/// 旧根仍可解旧 outbox。rename 失败时旧身份逐字节不变，临时文件被清理，可原样重试。
pub fn promote_identity_with_material(
    vault: &menghunote_storage::Vault,
    identity_path: &Path,
    identity: &LocalIdentityV1,
    material: &PromotionMaterial,
) -> Result<LocalIdentityV1, SyncError> {
    let candidate_epoch = material.candidate_root.key_epoch;
    if material.candidate_root.vault_id != identity.vault_id {
        return Err(SyncError::Identity("候选根归属与本地身份不符"));
    }
    if identity.current_epoch >= candidate_epoch {
        return Err(SyncError::Object("候选 epoch 必须高于当前身份 epoch"));
    }
    // 新管理 seed 必须与 pin 住的新公钥一致；旧 seed 绝不能继续冒充 current。
    if let Some(seed) = &material.new_manager_seed {
        if ManagementKey::from_seed(seed.clone()).public_key() != material.new_manager_public {
            return Err(SyncError::Object("新管理 key seed 与公钥不一致，拒绝推广"));
        }
    } else if identity.manager_seed.is_some() {
        return Err(SyncError::Unsupported(
            "管理设备推广必须提供由 R' 派生的新管理 seed",
        ));
    }
    if let Some(authentication) = &material.authentication {
        if authentication.control_rev != material.control_rev {
            return Err(SyncError::Object(
                "authenticated control_rev 与推广材料的 control_rev 不一致",
            ));
        }
    }
    // 旧根按 epoch 保留（至少最近一个）；本地重包完成前不得丢弃。
    let mut previous_roots = IdentityRotationStateV1::load_current(vault, identity)?
        .map(|state| state.previous_roots)
        .unwrap_or_default();
    previous_roots.push(HistoricalRootV1::new(
        identity.current_epoch,
        identity.current_root.clone(),
    ));
    previous_roots.sort_by_key(|entry| entry.epoch);
    previous_roots.dedup_by_key(|entry| entry.epoch);
    previous_roots.retain(|entry| entry.epoch < candidate_epoch);
    let rotation_state = IdentityRotationStateV1 {
        current_epoch: candidate_epoch,
        control_rev: material.control_rev,
        manager_rev: material.new_manager_rev,
        previous_roots,
    };
    // 推广后的身份 pin 必须来自本次 activation 的已验证值，而不是沿用旧 control_rev。
    let pinned_instance = material
        .authentication
        .map_or(identity.pinned_instance, |auth| auth.instance_id);
    let promoted = LocalIdentityV1 {
        vault_id: identity.vault_id,
        device_id: identity.device_id,
        peer_id: identity.peer_id,
        ed25519_seed: identity.ed25519_seed.clone(),
        x25519_secret: identity.x25519_secret.clone(),
        current_epoch: candidate_epoch,
        current_root: material.candidate_root.root.clone(),
        manager_seed: material.new_manager_seed.clone(),
        pinned_instance,
        pinned_manager: material.new_manager_public,
        control_rev: material.control_rev,
        pending_rotation: None,
    };
    let blob = promoted.protect()?;
    let temp = identity_path.with_extension("promote.tmp");
    // 1) 临时身份必须 `sync_all` 后才允许进入提交序列（Windows = FlushFileBuffers）。
    write_identity_temp(&temp, &blob)?;
    // 2) 旁路状态 / authenticated pin / repack 屏障都写在 rename 之前：
    //    崩溃时旧身份 epoch 与它们不匹配，旁路记录会被忽略（旧身份仍权威）。
    let committed = (|| -> Result<(), SyncError> {
        rotation_state.save(vault, &identity.vault_id, &identity.device_id)?;
        if let Some(authentication) = material.authentication {
            AuthenticatedPinV1 {
                epoch: candidate_epoch,
                control_rev: material.control_rev,
                instance_id: authentication.instance_id,
                activation_hash: authentication.activation_hash,
            }
            .save(vault)?;
        }
        vault.set_meta("sync.repack_required", b"1")?;
        // 3) rename = 唯一提交点（失败时旧身份逐字节不变，见测试）。
        replace_identity_file(&temp, identity_path)
    })();
    if let Err(error) = committed {
        // 失败回滚：正式身份没动，清掉临时文件避免下一次重试读到半份新 BLOB。
        let _ = fs::remove_file(&temp);
        return Err(error);
    }
    Ok(promoted)
}

/// 阶段 H：B 侧兼容入口。
///
/// B 侧没有 R'，因此只能推广**非管理设备**；管理设备必须走
/// [`promote_identity_with_material`] 并提供新管理 seed。
///
/// 与旧 6 参数入口的关键差别（评审 0003 P1-3）：推广**不再保留旧
/// `identity.control_rev`**，而是写入本次 activation 已验证的
/// [`AuthenticatedActivation`]（instance/control_rev/activation hash）。调用方必须先把
/// T/commit 验证结果传进来，不能再用"只换 root/manager pub"的旧形态。
pub fn promote_identity_atomic(
    vault: &menghunote_storage::Vault,
    identity_path: &Path,
    identity: &LocalIdentityV1,
    candidate_root: &RootSecretV1,
    authenticated: &AuthenticatedActivation,
    new_manager_public: [u8; 32],
    new_manager_rev: u64,
) -> Result<LocalIdentityV1, SyncError> {
    let material = PromotionMaterial {
        candidate_root: candidate_root.clone(),
        new_manager_seed: None,
        new_manager_public,
        new_manager_rev,
        control_rev: authenticated.control_rev,
        authentication: Some(*authenticated),
    };
    promote_identity_with_material(vault, identity_path, identity, &material)
}

/// 阶段 H：把 journal 按 `→ Promoted → Completed` 推进并清除候选秘密。
///
/// journal 只允许单步前进，旧实现从 `Activated` 直接 `advance(Completed)` 必报
/// "rotation phase 不能跳步"。这里兼容 restart 的 `RestartActivated`，并允许在
/// `Promoted`/`Completed` 上重入（崩溃后重试幂等）。
pub fn finish_promotion(
    journal: &mut RotationJournal,
    vault: &menghunote_storage::Vault,
) -> Result<(), SyncError> {
    match journal.phase {
        RotationPhase::Activated | RotationPhase::RestartActivated => {
            journal.advance(RotationPhase::Promoted)?;
        }
        RotationPhase::Promoted | RotationPhase::Completed => {}
        _ => {
            return Err(SyncError::Unsupported(
                "promotion 需要 Activated/RestartActivated/Promoted journal",
            ))
        }
    }
    if journal.phase != RotationPhase::Completed {
        journal.advance(RotationPhase::Completed)?;
    }
    // 本地推广完成后，journal 里的裸根与槽体密文不再需要。
    journal.candidate_root = None;
    journal.new_slot_bodies.clear();
    journal.save(vault)
}

/// G 的 canonical body digest（供测试/日志，不参与签名）。
pub fn activate_body_digest(input: &ActivationInput) -> Result<[u8; 32], SyncError> {
    let body = input.body();
    Ok(Sha256::digest(body.encode()?).into())
}
