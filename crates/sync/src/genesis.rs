//! genesis 客户端（S7-11）：本地造钥 → `POST /v1/vaults` → 落本地身份。
//!
//! 顺序（ADR §3/§4/§8/§17）：
//!
//! 1. 随机 `vault_id` / `device_id`，生成设备 Ed25519 + X25519 身份；
//! 2. 随机根 `K_epoch(1)`；随机恢复种子 `R` → Bech32m 恢复码 `mnrec1...`；
//! 3. 管理私钥 = `HKDF(R, vault_id, "MN/v1/management-ed25519\0")`，用它对
//!    `DeviceCertV1` 做 §17.1 管理签名；
//! 4. 初始 password 槽（Argon2id）与 recovery 槽（HKDF）封装根；
//! 5. 组 CBOR `POST /v1/vaults`（字段以 relay `api.rs` 为准），成功后把
//!    instance/log_id/epoch/control_rev/auth_rev 与初始游标（relay 键 5）带回。
//!
//! **初始游标**：relay 的 `GET /log` 需要 HMAC 游标，客户端无法自造。genesis 响应
//! 键 5 应带 `cursor_at(0)`；若 relay 版本较旧，调用方可用
//! [`crate::http_relay::HttpRelayClient::initial_cursor`] 兜底读 `GET /state` 键 14。
//! 两者都没有时返回明确错误，调用方必须走 bootstrap（`gc_floor > 0` 时同样必须
//! bootstrap，relay 对 `cursor_at(0)` 会返回 410 CURSOR_EXPIRED）。

use sha2::{Digest, Sha256};

use crate::cbor;
use crate::device::{
    default_rights, sign_device_cert, DeviceHpkeKey, DeviceSigningKey, ManagementKey,
    SignedDeviceCert,
};
use crate::error::SyncError;
use crate::http_relay::{
    error_from_response, HttpRequest, HttpResponse, HttpTransport, CONTENT_TYPE_CBOR,
};
use crate::ids::PeerId;
use crate::keys::RootSecretV1;
use crate::random::RandomSource;
use crate::recovery::{management_seed, RecoverySeed};
use crate::slot::{context_hash, KeySlotV1, WRAP_SUITE_XCHACHA20_POLY1305};
use crate::{DeviceId, VaultId};
use menghunote_sync_proto::{
    DeviceCertV1, EnvelopeHeader, Kdf, ObjectType, SlotDescriptorV1, SlotKind, PROTOCOL_VERSION,
};
use zeroize::Zeroizing;

/// 初始身份代。
pub const GENESIS_EPOCH: u64 = 1;
/// 初始管理权版本。
pub const GENESIS_MANAGER_REV: u64 = 1;

/// genesis 生成的全部本地材料。
pub struct GenesisMaterials {
    /// vault 身份。
    pub vault_id: VaultId,
    /// 设备身份。
    pub device_id: DeviceId,
    /// Loro peer id。
    pub peer_id: PeerId,
    /// 设备签名私钥。
    pub signing_key: DeviceSigningKey,
    /// 设备 HPKE 私钥。
    pub hpke_key: DeviceHpkeKey,
    /// epoch 1 的根。
    pub root: RootSecretV1,
    /// 恢复码派生的管理私钥。
    pub manager: ManagementKey,
    /// 恢复种子（用于恢复槽；不打印）。
    pub recovery_seed: RecoverySeed,
    /// 展示给用户的恢复码。
    pub recovery_code: Zeroizing<String>,
    /// genesis 设备证书（含 §17.1 管理签名）。
    pub signed_cert: SignedDeviceCert,
    /// 初始 password 槽。
    pub password_slot: KeySlotV1,
    /// 初始 recovery 槽。
    pub recovery_slot: KeySlotV1,
}

impl GenesisMaterials {
    /// 设备证书（不含签名）。
    #[must_use]
    pub fn certificate(&self) -> &DeviceCertV1 {
        &self.signed_cert.cert
    }

    /// 管理公钥（控制链信任锚）。
    #[must_use]
    pub fn manager_pub(&self) -> [u8; 32] {
        self.manager.public_key()
    }

    /// 设备指纹（真机展示用）。
    #[must_use]
    pub fn fingerprint(&self) -> crate::device::DeviceFingerprint {
        self.signed_cert.fingerprint()
    }

    /// 用于 Loro 的 peer id（u64；high bit 清零避免超出 Loro 的 i64 计数器域）。
    ///
    /// 规则与 [`crate::identity::LocalIdentityV1::loro_peer_id`] **同一条**：唯一实现是
    /// [`PeerId::loro_u64`]。app 的 dev-sync 身份与真实 genesis 身份因此不会给出两套
    /// peer 口径（ADR 0009 切片 2）。
    #[must_use]
    pub fn peer_id_u64(&self) -> u64 {
        self.peer_id.loro_u64()
    }

    /// 构造本地身份（instance_id/control_rev 由 genesis 响应填入）。
    pub fn identity(
        &self,
        instance_id: [u8; 16],
        control_rev: u64,
    ) -> Result<crate::identity::LocalIdentityV1, SyncError> {
        Ok(crate::identity::LocalIdentityV1 {
            vault_id: self.vault_id,
            device_id: self.device_id,
            peer_id: self.peer_id,
            ed25519_seed: self.signing_key.seed().clone(),
            x25519_secret: self.hpke_key.secret().clone(),
            current_epoch: self.root.key_epoch,
            current_root: self.root.root.clone(),
            manager_seed: Some(management_seed(&self.recovery_seed, &self.vault_id)?),
            pinned_instance: instance_id,
            pinned_manager: self.manager_pub(),
            control_rev,
            pending_rotation: None,
        })
    }
}

impl core::fmt::Debug for GenesisMaterials {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GenesisMaterials")
            .field("vault_id", &crate::ids::hex_of(&self.vault_id))
            .field("device_id", &crate::ids::hex_of(&self.device_id))
            .field("peer_id", &self.peer_id)
            .field("epoch", &self.root.key_epoch)
            .field("managers", &"<redacted>")
            .finish()
    }
}

/// genesis 响应里的 relay 事实（**不含**本地秘密材料）。
///
/// 拆出来的理由（ADR 0013 S2b）：初始化必须先 durable 记录 pending 材料，再发网络请求；
/// 请求只依赖本地材料，回执只需要这些公开事实，因此"造材料 / 提交请求 / 收事实"可以
/// 分三步走，pending 与身份落盘的先后关系才可对账。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenesisReceipt {
    /// relay 实例 id（pin 用）。
    pub instance_id: [u8; 16],
    /// relay 日志 id。
    pub log_id: [u8; 16],
    /// 活动 epoch。
    pub epoch: u64,
    /// 控制链版本。
    pub control_rev: u64,
    /// 鉴权版本。
    pub auth_rev: u64,
    /// 初始游标（relay 响应键 5；旧版 relay 可能没有）。
    pub initial_cursor: Option<String>,
}

/// genesis 成功后的结果。
pub struct GenesisOutcome {
    /// 本地材料。
    pub materials: GenesisMaterials,
    /// relay 实例 id（pin 用）。
    pub instance_id: [u8; 16],
    /// relay 日志 id。
    pub log_id: [u8; 16],
    /// 活动 epoch。
    pub epoch: u64,
    /// 控制链版本。
    pub control_rev: u64,
    /// 鉴权版本。
    pub auth_rev: u64,
    /// 初始游标（relay 响应键 5；旧版 relay 可能没有）。
    pub initial_cursor: Option<String>,
    /// 已落盘的本地身份。
    pub identity: crate::identity::LocalIdentityV1,
}

impl core::fmt::Debug for GenesisOutcome {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GenesisOutcome")
            .field("materials", &self.materials)
            .field("instance_id", &crate::ids::hex_of(&self.instance_id))
            .field("log_id", &crate::ids::hex_of(&self.log_id))
            .field("epoch", &self.epoch)
            .field("control_rev", &self.control_rev)
            .field("auth_rev", &self.auth_rev)
            .field("has_cursor", &self.initial_cursor.is_some())
            .finish()
    }
}

/// 生成 genesis 材料（不联网；测试/调用方注入 `now` 与随机源）。
pub fn build_materials(
    vault_id: VaultId,
    device_id: DeviceId,
    password: &str,
    now: u64,
    rng: &mut impl RandomSource,
) -> Result<GenesisMaterials, SyncError> {
    let signing_key = DeviceSigningKey::generate(rng)?;
    let hpke_key = DeviceHpkeKey::generate(rng)?;
    let root = RootSecretV1::generate(vault_id, GENESIS_EPOCH, rng)?;
    let recovery_seed = RecoverySeed::generate(rng)?;
    let recovery_code = recovery_seed.to_code()?;
    let manager = ManagementKey::from_recovery(&recovery_seed, &vault_id)?;

    let cert = DeviceCertV1 {
        vault_id,
        device_id,
        sign_ed25519_pub: signing_key.public_key(),
        hpke_x25519_pub: hpke_key.public_key(),
        cert_serial: 1,
        issued_manager_rev: GENESIS_MANAGER_REV,
        rights: default_rights(),
    };
    let signed_cert = sign_device_cert(&cert, &manager)?;

    let peer_hex: [u8; 16] = crate::random::array::<16>(rng)?;
    let peer_id = PeerId::from_bytes(peer_hex);

    let password_slot_id: [u8; 32] = crate::random::array::<32>(rng)?;
    let recovery_slot_id: [u8; 32] = crate::random::array::<32>(rng)?;
    let params = crate::kdf::new_writer_params(rng)?;

    let password_descriptor = SlotDescriptorV1 {
        protocol_version: PROTOCOL_VERSION,
        slot_id: password_slot_id,
        vault_id,
        key_epoch: GENESIS_EPOCH,
        kind: SlotKind::Password,
        wrap_suite: WRAP_SUITE_XCHACHA20_POLY1305,
        kdf: Kdf::Argon2id(params),
        created_at: now,
        manager_rev: GENESIS_MANAGER_REV,
    };
    let recovery_descriptor = SlotDescriptorV1 {
        protocol_version: PROTOCOL_VERSION,
        slot_id: recovery_slot_id,
        vault_id,
        key_epoch: GENESIS_EPOCH,
        kind: SlotKind::Recovery,
        wrap_suite: WRAP_SUITE_XCHACHA20_POLY1305,
        kdf: Kdf::RecoveryHkdf,
        created_at: now,
        manager_rev: GENESIS_MANAGER_REV,
    };

    let slot_aad = |descriptor: &SlotDescriptorV1,
                    nonce: &[u8; 24],
                    plaintext_len: usize|
     -> Result<Vec<u8>, SyncError> {
        let mut message_id = [0u8; 16];
        message_id.copy_from_slice(&descriptor.slot_id[..16]);
        let header = EnvelopeHeader {
            protocol_version: PROTOCOL_VERSION,
            header_len: 196,
            object_type: ObjectType::KeySlot,
            cipher_suite: 1,
            flags: 0,
            vault_id: descriptor.vault_id,
            object_id: descriptor.slot_id,
            message_id,
            producer_device_id: device_id,
            generation: 0,
            key_epoch: descriptor.key_epoch,
            sender_seq: 0,
            part_index: 0,
            part_count: 1,
            total_plaintext_len: plaintext_len as u64,
            this_plaintext_len: u32::try_from(plaintext_len)
                .map_err(|_| SyncError::Descriptor("根明文长度超出 u32"))?,
            ciphertext_len: u32::try_from(plaintext_len + 16)
                .map_err(|_| SyncError::Descriptor("槽密文长度超出 u32"))?,
            context_hash: context_hash(descriptor)?,
            nonce: *nonce,
        };
        Ok(header.aad(&descriptor.encode()?)?)
    };

    let password_slot = KeySlotV1::wrap_password(
        &password_descriptor,
        password.as_bytes(),
        &root,
        &slot_aad,
        rng,
    )?;
    let recovery_slot =
        KeySlotV1::wrap_recovery(&recovery_descriptor, &recovery_seed, &root, &slot_aad, rng)?;

    Ok(GenesisMaterials {
        vault_id,
        device_id,
        peer_id,
        signing_key,
        hpke_key,
        root,
        manager,
        recovery_seed,
        recovery_code,
        signed_cert,
        password_slot,
        recovery_slot,
    })
}

/// genesis 请求的 CBOR 字段：`{0:vault_id,1:device_id,2:sign_pk,3:hpke_pk,4:manager_pub,
/// 5:setup_secret,6:certificate,7:certificate_signature}`（relay `api.rs` create_vault）。
pub fn genesis_fields(
    materials: &GenesisMaterials,
    setup_secret: &str,
) -> Result<cbor::Fields, SyncError> {
    Ok(vec![
        (0, cbor::bytes(materials.vault_id)),
        (1, cbor::bytes(materials.device_id)),
        (2, cbor::bytes(materials.certificate().sign_ed25519_pub)),
        (3, cbor::bytes(materials.certificate().hpke_x25519_pub)),
        (4, cbor::bytes(materials.manager_pub())),
        (5, cbor::text(setup_secret)),
        (6, cbor::bytes(materials.signed_cert.cert_bytes()?)),
        (7, cbor::bytes(materials.signed_cert.signature)),
    ])
}

/// genesis 请求体（CBOR；测试 golden 用）。
pub fn genesis_request_bytes(
    materials: &GenesisMaterials,
    setup_secret: &str,
) -> Result<Vec<u8>, SyncError> {
    cbor::encode(genesis_fields(materials, setup_secret)?)
}

/// rev1 控制记录摘要：`record_hash = SHA256(零32 || 设备证书)`（ADR 0013 §3 / §35）。
///
/// 与 relay `vault::init` 里插入 rev1 控制记录时用的公式**逐字节相同**：第一条控制记录的
/// `prev_hash` 是 32 字节全零，`signed_record` 就是 genesis 设备证书的 CDE 字节。客户端在
/// **发请求之前**就能用本地提交的证书复算它，因此 Create 可以把"rev1 强 pin"作为
/// 第一次联网的信任起点，而不需要任何 unpinned 消费。
///
/// 证书编码失败（CDE 契约）返回 [`SyncError::Proto`]，不做任何降级。
pub fn rev1_record_hash(
    certificate: &menghunote_sync_proto::DeviceCertV1,
) -> Result<[u8; 32], SyncError> {
    let cert_bytes = certificate.encode()?;
    let mut input = Vec::with_capacity(32 + cert_bytes.len());
    input.extend_from_slice(&[0u8; 32]);
    input.extend_from_slice(&cert_bytes);
    Ok(Sha256::digest(&input).into())
}

/// v2 创建票据请求的 CBOR 字段：`{0:ticket, 1..7: 与 v1 genesis 同序}`。
///
/// 与 v1 的唯一差别是**授权字段**：v2 没有 setup secret，键 0 是 32B 创建票据，
/// 原有字段整体后移一位（relay `api::create_vault_v2` 逐字段对齐）。
pub fn ticket_genesis_fields(
    materials: &GenesisMaterials,
    ticket: &[u8; 32],
) -> Result<cbor::Fields, SyncError> {
    Ok(vec![
        (0, cbor::bytes(*ticket)),
        (1, cbor::bytes(materials.vault_id)),
        (2, cbor::bytes(materials.device_id)),
        (3, cbor::bytes(materials.certificate().sign_ed25519_pub)),
        (4, cbor::bytes(materials.certificate().hpke_x25519_pub)),
        (5, cbor::bytes(materials.manager_pub())),
        (6, cbor::bytes(materials.signed_cert.cert_bytes()?)),
        (7, cbor::bytes(materials.signed_cert.signature)),
    ])
}

/// v2 创建票据请求体（CBOR；测试 golden 用）。
pub fn ticket_genesis_request_bytes(
    materials: &GenesisMaterials,
    ticket: &[u8; 32],
) -> Result<Vec<u8>, SyncError> {
    cbor::encode(ticket_genesis_fields(materials, ticket)?)
}

/// 对 relay 执行 genesis：`POST /v1/vaults`，并返回回执（不含本地材料）。
///
/// S2b 的受限初始化入口用它把"本地材料已 durable 记录"与"远端已创建"分成两步；
/// 传统入口 [`run`] 只是"造材料 + 调本函数"。
pub fn submit<T: HttpTransport>(
    transport: &mut T,
    materials: &GenesisMaterials,
    setup_secret: &str,
) -> Result<GenesisReceipt, SyncError> {
    let body = genesis_request_bytes(materials, setup_secret)?;
    let path = crate::api_version::RelayApi::new("", crate::api_version::RelayApiVersion::V1)
        .create_vault_path();
    submit_request(transport, path, body)
}

/// 对 relay 执行 **v2** 票据创建：`POST /v2/vaults`（ADR 0014 S3a）。
///
/// 授权来自 32B 一次性票据（只在内存里；不落库/日志/配置）。响应与 v1 create 同形，
/// 因此回执解析复用 [`submit_request`]；路径来自统一 [`RelayApi`] 生成器。
pub fn submit_with_ticket<T: HttpTransport>(
    transport: &mut T,
    materials: &GenesisMaterials,
    ticket: &[u8; 32],
) -> Result<GenesisReceipt, SyncError> {
    let body = ticket_genesis_request_bytes(materials, ticket)?;
    let path = crate::api_version::RelayApi::new("", crate::api_version::RelayApiVersion::V2)
        .create_vault_path();
    submit_request(transport, path, body)
}

/// 发送创建请求并解析同形回执（v1/v2 共用，避免两份解析）。
fn submit_request<T: HttpTransport>(
    transport: &mut T,
    path: String,
    body: Vec<u8>,
) -> Result<GenesisReceipt, SyncError> {
    let response: HttpResponse = transport.send(&HttpRequest {
        method: "POST",
        path,
        headers: vec![("content-type".to_owned(), CONTENT_TYPE_CBOR.to_owned())],
        body,
    })?;
    if !(200..300).contains(&response.status) {
        return Err(error_from_response(&response));
    }
    let fields = cbor::decode(&response.body)?;
    Ok(GenesisReceipt {
        instance_id: cbor::bytes_fixed(&fields, 0)?,
        log_id: cbor::bytes_fixed(&fields, 1)?,
        epoch: cbor::uint_field(&fields, 2)?,
        control_rev: cbor::uint_field(&fields, 3)?,
        auth_rev: cbor::uint_field(&fields, 4)?,
        initial_cursor: cbor::opt_text_field(&fields, 5)?,
    })
}

/// 对 relay 执行 genesis：`POST /v1/vaults`。
pub fn run<T: HttpTransport>(
    transport: &mut T,
    setup_secret: &str,
    password: &str,
    now: u64,
    rng: &mut impl RandomSource,
) -> Result<GenesisOutcome, SyncError> {
    let vault_id: VaultId = crate::random::array::<16>(rng)?;
    let device_id: DeviceId = crate::random::array::<16>(rng)?;
    let materials = build_materials(vault_id, device_id, password, now, rng)?;
    let receipt = submit(transport, &materials, setup_secret)?;
    let identity = materials.identity(receipt.instance_id, receipt.control_rev)?;
    Ok(GenesisOutcome {
        materials,
        instance_id: receipt.instance_id,
        log_id: receipt.log_id,
        epoch: receipt.epoch,
        control_rev: receipt.control_rev,
        auth_rev: receipt.auth_rev,
        initial_cursor: receipt.initial_cursor,
        identity,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestRng;
    use menghunote_sync_proto::device_cert_signed_bytes;

    fn materials(seed: u64) -> GenesisMaterials {
        let mut rng = TestRng::new(seed);
        build_materials(
            [0x5a; 16],
            [0x04; 16],
            "correct horse",
            1_700_000_000,
            &mut rng,
        )
        .expect("genesis 材料")
    }

    #[test]
    fn genesis_request_layout_matches_relay_fields() {
        let materials = materials(11);
        let bytes = genesis_request_bytes(&materials, "setup-secret").expect("请求");
        let fields = cbor::decode(&bytes).expect("解码");
        // 键必须 0..=7 连续（可选字段在 genesis 里都是必填）。
        assert_eq!(fields.len(), 8);
        for (index, (key, _)) in fields.iter().enumerate() {
            assert_eq!(*key, index as u64);
        }
        assert_eq!(cbor::bytes_fixed::<16>(&fields, 0).unwrap(), [0x5a; 16]);
        assert_eq!(cbor::bytes_fixed::<16>(&fields, 1).unwrap(), [0x04; 16]);
        assert_eq!(
            cbor::bytes_fixed::<32>(&fields, 2).unwrap(),
            materials.certificate().sign_ed25519_pub
        );
        assert_eq!(
            cbor::bytes_fixed::<32>(&fields, 4).unwrap(),
            materials.manager_pub()
        );
        assert_eq!(cbor::text_field(&fields, 5).unwrap(), "setup-secret");
        assert_eq!(
            cbor::bytes_field(&fields, 6).unwrap(),
            materials.signed_cert.cert_bytes().unwrap()
        );
        assert_eq!(
            cbor::bytes_fixed::<64>(&fields, 7).unwrap(),
            materials.signed_cert.signature
        );
    }

    #[test]
    fn certificate_is_verifiable_by_recovery_chain() {
        let materials = materials(12);
        let signed_input = device_cert_signed_bytes(materials.certificate()).expect("待签");
        crate::device::verify_signature(
            &materials.manager_pub(),
            &signed_input,
            &materials.signed_cert.signature,
        )
        .expect("管理签名");
        // 恢复码 → 管理 key 的链路也必须自洽。
        let parsed = RecoverySeed::from_code(&materials.recovery_code).expect("恢复码");
        let manager = ManagementKey::from_recovery(&parsed, &materials.vault_id).expect("管理密钥");
        assert_eq!(manager.public_key(), materials.manager_pub());
        materials
            .signed_cert
            .verify(&manager.public_key())
            .expect("证书链");
    }

    #[test]
    fn initial_slots_unwrap_to_the_same_root() {
        let materials = materials(13);
        let password = materials
            .password_slot
            .unwrap_password(b"correct horse", &slot_aad_for(&materials))
            .expect("口令槽");
        assert!(password.ct_eq(&materials.root));
        let recovery = materials
            .recovery_slot
            .unwrap_recovery(&materials.recovery_seed, &slot_aad_for(&materials))
            .expect("恢复槽");
        assert!(recovery.ct_eq(&materials.root));
        // 错口令必须失败（Argon2id 派生后 AEAD 拒绝）。
        assert!(materials
            .password_slot
            .unwrap_password(b"wrong", &slot_aad_for(&materials))
            .is_err());
    }

    fn slot_aad_for(
        materials: &GenesisMaterials,
    ) -> impl Fn(&SlotDescriptorV1, &[u8; 24], usize) -> Result<Vec<u8>, SyncError> {
        let device_id = materials.device_id;
        move |descriptor: &SlotDescriptorV1, nonce: &[u8; 24], plaintext_len: usize| {
            let mut message_id = [0u8; 16];
            message_id.copy_from_slice(&descriptor.slot_id[..16]);
            let header = EnvelopeHeader {
                protocol_version: PROTOCOL_VERSION,
                header_len: 196,
                object_type: ObjectType::KeySlot,
                cipher_suite: 1,
                flags: 0,
                vault_id: descriptor.vault_id,
                object_id: descriptor.slot_id,
                message_id,
                producer_device_id: device_id,
                generation: 0,
                key_epoch: descriptor.key_epoch,
                sender_seq: 0,
                part_index: 0,
                part_count: 1,
                total_plaintext_len: plaintext_len as u64,
                this_plaintext_len: u32::try_from(plaintext_len).expect("长度"),
                ciphertext_len: u32::try_from(plaintext_len + 16).expect("长度"),
                context_hash: context_hash(descriptor)?,
                nonce: *nonce,
            };
            Ok(header.aad(&descriptor.encode()?)?)
        }
    }

    /// rev1 控制记录摘要：**已知证书向量**（与 relay `vault::init` 的公式逐字节一致）。
    ///
    /// 固定一份证书的 CDE 编码 → `SHA256(零32 || cert)`；期望值是常量，因此任何对证书
    /// 编码/公式的静默修改（哪怕只改一个字段编号）都会让本测试失败。
    #[test]
    fn rev1_record_hash_has_a_known_answer() {
        let cert = menghunote_sync_proto::DeviceCertV1 {
            vault_id: [0x11; 16],
            device_id: [0x22; 16],
            sign_ed25519_pub: [0x33; 32],
            hpke_x25519_pub: [0x44; 32],
            cert_serial: 7,
            issued_manager_rev: 1,
            rights: default_rights(),
        };
        let hash = rev1_record_hash(&cert).expect("rev1 摘要");
        assert_eq!(
            crate::ids::hex_of(&hash),
            "c0cfd56ee351ad4cb96d9070734f63d04e8e27539cbf3dd551b9f4981eaec8e4",
            "已知证书向量的 rev1 record_hash 变了：公式/证书编码被改动"
        );
        // 公式本身就是 `SHA256(零32 || CDE(cert))`：用独立实现复算一遍。
        let mut input = vec![0u8; 32];
        input.extend_from_slice(&cert.encode().expect("证书编码"));
        let expected: [u8; 32] = Sha256::digest(&input).into();
        assert_eq!(hash, expected);
        // 换任何一个字段都必须变（防止常量折叠式的假实现）。
        let mut other = cert.clone();
        other.cert_serial = 8;
        assert_ne!(rev1_record_hash(&other).expect("摘要"), hash);
    }

    #[test]
    fn identity_roundtrips_and_pins_manager() {
        let materials = materials(14);
        let identity = materials.identity([0x22; 16], 1).expect("身份");
        assert_eq!(identity.pinned_manager, materials.manager_pub());
        assert_eq!(identity.pinned_instance, [0x22; 16]);
        let encoded = identity.encode().expect("编码");
        let decoded = crate::identity::LocalIdentityV1::decode(&encoded).expect("解码");
        assert_eq!(decoded.vault_id, identity.vault_id);
        assert_eq!(decoded.device_id, identity.device_id);
        assert!(decoded.current_root.ct_eq(&identity.current_root));
    }

    #[test]
    fn secret_types_do_not_leak_in_debug() {
        let materials = materials(15);
        let text = format!("{materials:?}");
        assert!(text.contains("<redacted>"));
        assert!(!text.contains(&materials.recovery_code.to_string().replace("mnrec1", "")));
    }
}
