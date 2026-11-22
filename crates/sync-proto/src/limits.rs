//! ADR 0006 的 wire 常量、上限与固定切片规则。

use crate::error::ProtoError;

pub const MAGIC: [u8; 4] = *b"MNE1";
pub const PROTOCOL_VERSION: u16 = 1;
pub const HEADER_LEN: usize = 196;
pub const TAG_LEN: usize = 16;
pub const NONCE_LEN: usize = 24;
pub const CONTEXT_HASH_LEN: usize = 32;
pub const OBJECT_ID_LEN: usize = 32;
pub const VAULT_ID_LEN: usize = 16;
pub const MESSAGE_ID_LEN: usize = 16;
pub const DEVICE_ID_LEN: usize = 16;
pub const CIPHER_SUITE_XCHACHA20_POLY1305: u8 = 1;
/// header 196 + AEAD tag 16。
pub const ENVELOPE_OVERHEAD: usize = HEADER_LEN + TAG_LEN; // 212
pub const ONE_MIB: usize = 1024 * 1024;
pub const FOUR_MIB: usize = 4 * 1024 * 1024;
pub const SIXTY_FOUR_MIB: usize = 64 * 1024 * 1024;
/// 非资产单片的明文上限（1MiB - 212）。
pub const MAX_NON_ASSET_PART_PLAINTEXT: usize = ONE_MIB - ENVELOPE_OVERHEAD;
/// 资产单片的明文上限（4MiB - 212）。
pub const MAX_ASSET_PART_PLAINTEXT: usize = FOUR_MIB - ENVELOPE_OVERHEAD;
/// KEY_SLOT 明文硬上限。
pub const MAX_KEY_SLOT_PLAINTEXT: usize = 4096;
pub const MAX_UPDATE_CDE_BYTES: usize = SIXTY_FOUR_MIB;
pub const MAX_SNAPSHOT_CDE_BYTES: usize = SIXTY_FOUR_MIB;
pub const MAX_MANIFEST_CDE_BYTES: usize = SIXTY_FOUR_MIB;
pub const MAX_ASSET_RAW_BYTES: usize = SIXTY_FOUR_MIB;
pub const MAX_ASSET_MIME_BYTES: usize = 255;
pub const MAX_REFS: usize = 1024;
pub const MAX_CONTROL_HTTP_BYTES: usize = ONE_MIB;
pub const MAX_WS_FRAME_BYTES: usize = 512;
pub const MAX_DEVICES: usize = 32;
pub const MAX_PASSWORD_BYTES: usize = 1024;
pub const MAX_LORO_SNAPSHOT_BYTES: usize = 4 * 1024 * 1024 * 1024;
pub const MAX_CHECKPOINT_REFS: usize = 1024;

// —— ADR 0015：管理控制链 V2 ——

/// `PolicyV2.max_cap_ttl` 的协议上限（秒，30 天）。
pub const MAX_CAP_TTL_SECS_V2: u64 = 2_592_000;
/// `PolicyV2.max_invite_ttl` 的协议上限（秒，1 天）。
pub const MAX_INVITE_TTL_SECS_V2: u64 = 86_400;
/// 单条 `OperationV2` 的 signer 上限。ADR 0015 只固定“高危两签”，未给数组上限；
/// 这里取 16 作为实现边界（升级/PoP/两签叠加仍有充足余量），超出即 `TooLarge`。
pub const MAX_OPERATION_SIGNERS_V2: usize = 16;
/// `OperationV2.canonical_path` 的字节上限；实际唯一提交路径固定 58B。
pub const MAX_CANONICAL_PATH_BYTES_V2: usize = 4096;
/// `UpgradePolicyPayloadV2.bootstrap_caps[]` 上限（实现边界；ADR 未固定条数）。
pub const MAX_BOOTSTRAP_CAPS_V2: usize = 64;
/// `DeviceApprove/DeviceRightsChange` payload 的 `grants[]` 上限（实现边界；ADR 未固定条数）。
///
/// 一个 epoch 只需一份 grant（PK `(vault_id, epoch, device_id)`），当前实现进一步要求
/// 恰好一份当前 epoch 的包；上限只是拒绝超长数组的 wire 边界。
pub const MAX_GRANT_PACKAGES_V2: usize = 32;

/// 固定切片明文容量。
pub const fn cap_plain(object_type: crate::records::ObjectType) -> u64 {
    use crate::records::ObjectType;
    match object_type {
        ObjectType::Asset => MAX_ASSET_PART_PLAINTEXT as u64,
        _ => MAX_NON_ASSET_PART_PLAINTEXT as u64,
    }
}

/// `ceil(total / cap)`；total=0 时返回 0（由调用方再判非空）。
pub const fn part_count_for(
    total_plaintext_len: u64,
    object_type: crate::records::ObjectType,
) -> u64 {
    let cap = cap_plain(object_type);
    if total_plaintext_len == 0 {
        0
    } else {
        total_plaintext_len.div_ceil(cap)
    }
}

/// 第 `part_index` 片应有的明文长度（0-based；调用方须保证 index < count）。
pub fn expected_part_plain_len(
    object_type: crate::records::ObjectType,
    total_plaintext_len: u64,
    part_index: u32,
) -> Result<usize, ProtoError> {
    if total_plaintext_len == 0 {
        return Err(ProtoError::Invalid(
            "total_plaintext_len 不能为 0".to_owned(),
        ));
    }
    let cap = cap_plain(object_type);
    let count = part_count_for(total_plaintext_len, object_type);
    let index = u64::from(part_index);
    if index >= count {
        return Err(ProtoError::Invalid(format!(
            "part_index {part_index} 超出 part_count {count}"
        )));
    }
    let before = cap * index;
    let remaining = total_plaintext_len - before;
    usize::try_from(remaining.min(cap)).map_err(|_| ProtoError::TooLarge)
}

/// 校验固定切片（§16.3）：count、每片长度、最后一片正长度、KEY_SLOT 单片。
pub fn validate_slicing(
    object_type: crate::records::ObjectType,
    total_plaintext_len: u64,
    part_index: u32,
    part_count: u32,
    this_plaintext_len: u32,
) -> Result<(), ProtoError> {
    use crate::records::ObjectType;
    if total_plaintext_len == 0 {
        return Err(ProtoError::Invalid("载荷不能为空".to_owned()));
    }
    if part_count == 0 {
        return Err(ProtoError::Invalid("part_count 必须 ≥1".to_owned()));
    }
    if part_index >= part_count {
        return Err(ProtoError::Invalid(
            "part_index 必须 < part_count".to_owned(),
        ));
    }
    let expected_count = part_count_for(total_plaintext_len, object_type);
    if u64::from(part_count) != expected_count {
        return Err(ProtoError::Invalid(format!(
            "part_count={part_count} 与固定切片计算值 {expected_count} 不符"
        )));
    }
    let expected_len = expected_part_plain_len(object_type, total_plaintext_len, part_index)?;
    if usize::try_from(this_plaintext_len).ok() != Some(expected_len) {
        return Err(ProtoError::Invalid(format!(
            "this_plaintext_len={this_plaintext_len}，应为 {expected_len}"
        )));
    }
    if object_type == ObjectType::KeySlot {
        if total_plaintext_len > MAX_KEY_SLOT_PLAINTEXT as u64 {
            return Err(ProtoError::TooLarge);
        }
        if part_count != 1 {
            return Err(ProtoError::Invalid("KEY_SLOT 必须单片".to_owned()));
        }
    }
    if object_type == ObjectType::Asset && total_plaintext_len > MAX_ASSET_RAW_BYTES as u64 {
        return Err(ProtoError::TooLarge);
    }
    Ok(())
}
