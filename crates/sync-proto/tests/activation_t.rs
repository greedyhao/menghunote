//! task-111 第一步：完整 activation T（`ActivationTV1`）的 golden / canonical / 边界测试。
//!
//! golden 文件 `tests/golden_activation_t.hex` 由独立 Python CBOR 编码脚本按字段表生成，
//! 本文件只读 hex 比对，不参与生成——否则 golden 只是实现的镜像，锁不住字段编号。

use std::fmt::Write as _;

use menghunote_sync_proto::{
    activation_t_signing_input, decode_cde, domain_ends_with_nul, encode_cde, ActivationTV1,
    CdeValue, ProtoError, AAD_DOMAIN, ACTIVATION_T_FIELD_COUNT, CURSOR_HMAC_DOMAIN,
    SIGN_ACTIVATION_T_DOMAIN, SIGN_AUTH_DOMAIN, SIGN_CHECKPOINT_DOMAIN, SIGN_DEVICE_CERT_DOMAIN,
    SIGN_MANAGE_DOMAIN, SIGN_OBJECT_COMMIT_DOMAIN,
};

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut out, "{byte:02x}").expect("写 hex");
    }
    out
}

fn golden(name: &str) -> String {
    let prefix = format!("{name} ");
    include_str!("golden_activation_t.hex")
        .lines()
        .find_map(|line| line.strip_prefix(&prefix).map(str::to_owned))
        .unwrap_or_else(|| panic!("golden 缺少 {name}"))
}

fn ids(value: u8) -> [u8; 16] {
    [value; 16]
}

fn hash(value: u8) -> [u8; 32] {
    [value; 32]
}

fn sample() -> ActivationTV1 {
    ActivationTV1 {
        vault_id: ids(0xA1),
        instance_id: ids(0xA2),
        rotation_id: ids(0xA3),
        from_epoch: 7,
        to_epoch: 8,
        freeze_seq: 1_234_567,
        old_manager_rev: 3,
        new_manager_rev: 4,
        new_manager_pubkey: hash(0xB1),
        prev_control_hash: hash(0xB2),
        expected_control_rev: 42,
        primary_cid: ids(0xA4),
        primary_commit_digest: hash(0xC1),
        fallback_cid: ids(0xA5),
        fallback_commit_digest: hash(0xC2),
        slot_set_digest: hash(0xC3),
        member_cert_digest: hash(0xC4),
        grants_digest: hash(0xC5),
        old_asset_set_digest: hash(0xC6),
        new_asset_set_digest: hash(0xC7),
    }
}

fn entries_of(bytes: &[u8]) -> Vec<(u64, CdeValue)> {
    let CdeValue::Map(entries) = decode_cde(bytes).expect("decode map") else {
        panic!("期望 map");
    };
    entries
}

fn encode_entries(entries: Vec<(u64, CdeValue)>) -> Vec<u8> {
    encode_cde(&CdeValue::Map(entries)).expect("encode map")
}

/// `(key, 已编码 value)` 的扁平形式，便于手工篡改单个字段的 wire 字节。
fn raw_entries(bytes: &[u8]) -> Vec<(u64, Vec<u8>)> {
    entries_of(bytes)
        .into_iter()
        .map(|(key, value)| (key, encode_cde(&value).expect("encode value")))
        .collect()
}

fn cbor_head(major: u8, value: u64) -> Vec<u8> {
    let major = major << 5;
    if value < 24 {
        vec![major | value as u8]
    } else if value <= u8::MAX as u64 {
        vec![major | 24, value as u8]
    } else if value <= u16::MAX as u64 {
        let mut out = vec![major | 25];
        out.extend_from_slice(&(value as u16).to_be_bytes());
        out
    } else if value <= u32::MAX as u64 {
        let mut out = vec![major | 26];
        out.extend_from_slice(&(value as u32).to_be_bytes());
        out
    } else {
        let mut out = vec![major | 27];
        out.extend_from_slice(&value.to_be_bytes());
        out
    }
}

/// 按给定顺序手工拼 CBOR map（**不排序、不去重**），用于构造非 canonical 输入。
fn raw_map(entries: &[(u64, Vec<u8>)]) -> Vec<u8> {
    let mut out = cbor_head(5, entries.len() as u64);
    for (key, value) in entries {
        out.extend(cbor_head(0, *key));
        out.extend_from_slice(value);
    }
    out
}

#[test]
fn activation_t_golden_round_trip_and_field_layout() {
    let record = sample();
    let encoded = record.encode().expect("encode");
    assert_eq!(hex(&encoded), golden("activation_t"));

    let decoded = ActivationTV1::decode(&encoded).expect("decode");
    assert_eq!(decoded, record);
    assert_eq!(decoded.encode().expect("re-encode"), encoded);

    assert_eq!(ACTIVATION_T_FIELD_COUNT, 20);
    let entries = entries_of(&encoded);
    assert_eq!(entries.len(), ACTIVATION_T_FIELD_COUNT);
    assert_eq!(
        entries.iter().map(|(key, _)| *key).collect::<Vec<_>>(),
        (0..ACTIVATION_T_FIELD_COUNT as u64).collect::<Vec<_>>(),
        "字段编号必须恰好是 0..20 的连续升序"
    );

    // 每个键的 wire 类型/长度也是冻结契约的一部分。
    for (key, value) in entries {
        match key {
            3 | 4 | 5 | 6 | 7 | 10 => {
                assert!(matches!(value, CdeValue::UInt(_)), "key {key} 应为 uint")
            }
            0 | 1 | 2 | 11 | 13 => {
                let CdeValue::Bytes(bytes) = value else {
                    panic!("key {key} 应为 byte string");
                };
                assert_eq!(bytes.len(), 16, "key {key} 长度必须 16B");
            }
            8 | 9 | 12 | 14 | 15 | 16 | 17 | 18 | 19 => {
                let CdeValue::Bytes(bytes) = value else {
                    panic!("key {key} 应为 byte string");
                };
                assert_eq!(bytes.len(), 32, "key {key} 长度必须 32B");
            }
            other => panic!("未知键 {other}"),
        }
    }
}

#[test]
fn activation_t_is_canonical_unique() {
    let encoded = sample().encode().expect("encode");
    let golden_hex = hex(&encoded);

    // 同一语义值、map 条目逆序 → 编码器按 key 升序，仍是同一字节串。
    let mut reversed = entries_of(&encoded);
    reversed.reverse();
    assert_eq!(hex(&encode_entries(reversed)), golden_hex);

    // 手写乱序 map（不经过 CDE encoder）→ 解码必须拒绝。
    let mut shuffled = raw_entries(&encoded);
    shuffled.swap(0, 1);
    assert!(matches!(
        ActivationTV1::decode(&raw_map(&shuffled)),
        Err(ProtoError::NonCanonical(_))
    ));

    // 非最短整数（字段 3 的值 7 写成 0x18 0x07）→ 解码必须拒绝。
    let mut raw = raw_entries(&encoded);
    assert_eq!(raw[3].0, 3);
    raw[3].1 = vec![0x18, 0x07];
    assert!(matches!(
        ActivationTV1::decode(&raw_map(&raw)),
        Err(ProtoError::NonCanonical(_))
    ));

    // 同一字节串只对应一个语义值：decode → encode 原样返回。
    assert_eq!(
        ActivationTV1::decode(&encoded)
            .expect("decode")
            .encode()
            .expect("re-encode"),
        encoded
    );
}

#[test]
fn activation_t_rejects_unknown_missing_truncated_overlong_and_bad_lengths() {
    let encoded = sample().encode().expect("encode");

    // 未知键，字段总数不变：把最后一个键 19 换成 30。
    let mut entries = entries_of(&encoded);
    entries.pop();
    entries.push((30, CdeValue::UInt(0)));
    assert!(matches!(
        ActivationTV1::decode(&encode_entries(entries)),
        Err(ProtoError::UnknownField(30))
    ));

    // 未知键，字段总数 +1。
    let mut entries = entries_of(&encoded);
    entries.push((ACTIVATION_T_FIELD_COUNT as u64, CdeValue::UInt(0)));
    assert!(ActivationTV1::decode(&encode_entries(entries)).is_err());

    // 缺一个字段（字段数不足）。
    let mut entries = entries_of(&encoded);
    entries.remove(7);
    assert!(ActivationTV1::decode(&encode_entries(entries)).is_err());

    // 截断。
    assert!(matches!(
        ActivationTV1::decode(&encoded[..encoded.len() - 1]),
        Err(ProtoError::Truncated)
    ));
    assert!(ActivationTV1::decode(&encoded[..10]).is_err());
    assert!(ActivationTV1::decode(&[]).is_err());

    // 尾部多余字节（超长）。
    let mut trailing = encoded.clone();
    trailing.push(0x00);
    assert!(matches!(
        ActivationTV1::decode(&trailing),
        Err(ProtoError::NonCanonical(_))
    ));

    // 错误长度：16B 字段 15B/17B，32B 字段 31B/33B。
    for (key, len) in [(0_u64, 15_usize), (2, 17), (8, 31), (12, 33)] {
        let mut entries = entries_of(&encoded);
        entries[key as usize].1 = CdeValue::Bytes(vec![0xA0; len]);
        assert!(
            ActivationTV1::decode(&encode_entries(entries)).is_err(),
            "key {key} 长度 {len} 必须拒绝"
        );
    }

    // 类型错：epoch 写成 byte string。
    let mut entries = entries_of(&encoded);
    entries[3].1 = CdeValue::Bytes(vec![7]);
    assert!(ActivationTV1::decode(&encode_entries(entries)).is_err());
}

#[test]
fn activation_t_validate_rejects_zero_and_out_of_range() {
    let record = sample();
    assert!(record.validate().is_ok());

    // 14 个字节字段各自全零 → validate/encode/签名输入全部拒绝。
    let zeroed = [
        ActivationTV1 {
            vault_id: [0; 16],
            ..record.clone()
        },
        ActivationTV1 {
            instance_id: [0; 16],
            ..record.clone()
        },
        ActivationTV1 {
            rotation_id: [0; 16],
            ..record.clone()
        },
        ActivationTV1 {
            new_manager_pubkey: [0; 32],
            ..record.clone()
        },
        ActivationTV1 {
            prev_control_hash: [0; 32],
            ..record.clone()
        },
        ActivationTV1 {
            primary_cid: [0; 16],
            ..record.clone()
        },
        ActivationTV1 {
            primary_commit_digest: [0; 32],
            ..record.clone()
        },
        ActivationTV1 {
            fallback_cid: [0; 16],
            ..record.clone()
        },
        ActivationTV1 {
            fallback_commit_digest: [0; 32],
            ..record.clone()
        },
        ActivationTV1 {
            slot_set_digest: [0; 32],
            ..record.clone()
        },
        ActivationTV1 {
            member_cert_digest: [0; 32],
            ..record.clone()
        },
        ActivationTV1 {
            grants_digest: [0; 32],
            ..record.clone()
        },
        ActivationTV1 {
            old_asset_set_digest: [0; 32],
            ..record.clone()
        },
        ActivationTV1 {
            new_asset_set_digest: [0; 32],
            ..record.clone()
        },
    ];
    for broken in zeroed {
        assert!(broken.validate().is_err());
        assert!(broken.encode().is_err());
        assert!(activation_t_signing_input(&broken).is_err());
    }

    // 零值 / 越界 / 单调性违规。
    let broken = [
        ActivationTV1 {
            from_epoch: 0,
            ..record.clone()
        },
        ActivationTV1 {
            to_epoch: 0,
            ..record.clone()
        },
        ActivationTV1 {
            to_epoch: 7,
            ..record.clone()
        },
        ActivationTV1 {
            to_epoch: 6,
            ..record.clone()
        },
        ActivationTV1 {
            old_manager_rev: 0,
            ..record.clone()
        },
        ActivationTV1 {
            new_manager_rev: 0,
            ..record.clone()
        },
        ActivationTV1 {
            new_manager_rev: 3,
            ..record.clone()
        },
        ActivationTV1 {
            old_manager_rev: u32::MAX,
            new_manager_rev: u32::MAX,
            ..record.clone()
        },
        ActivationTV1 {
            expected_control_rev: 0,
            ..record.clone()
        },
        ActivationTV1 {
            freeze_seq: u64::MAX,
            ..record.clone()
        },
        ActivationTV1 {
            expected_control_rev: u64::MAX,
            ..record.clone()
        },
        ActivationTV1 {
            from_epoch: u64::MAX,
            to_epoch: u64::MAX,
            ..record.clone()
        },
    ];
    for broken in broken {
        assert!(broken.validate().is_err(), "{broken:?}");
        assert!(broken.encode().is_err(), "{broken:?}");
    }

    // 正面：restart 可跨多个已分配 epoch，不要求 to_epoch == from_epoch + 1。
    let restarted = ActivationTV1 {
        to_epoch: 11,
        ..record.clone()
    };
    assert!(restarted.validate().is_ok());
    assert_eq!(
        ActivationTV1::decode(&restarted.encode().expect("restart encode"))
            .expect("restart decode"),
        restarted
    );

    // 正面：空库首次激活时 S（freeze_seq）可以为 0。
    let empty = ActivationTV1 {
        freeze_seq: 0,
        ..record
    };
    assert!(empty.validate().is_ok());
    assert_eq!(
        ActivationTV1::decode(&empty.encode().expect("empty encode")).expect("empty decode"),
        empty
    );
}

#[test]
fn activation_t_signing_input_uses_dedicated_nul_domain() {
    let record = sample();
    let encoded = record.encode().expect("encode");
    let signing = activation_t_signing_input(&record).expect("signing input");
    assert_eq!(hex(&signing), golden("activation_t_signing_input"));

    // `MN/v1/activation-t\0`：真实 NUL 结尾，不是两个 ASCII 字符 `\0`。
    assert_eq!(
        &signing[..19],
        &[
            0x4d, 0x4e, 0x2f, 0x76, 0x31, 0x2f, 0x61, 0x63, 0x74, 0x69, 0x76, 0x61, 0x74, 0x69,
            0x6f, 0x6e, 0x2d, 0x74, 0x00,
        ]
    );
    assert_eq!(&signing[19..], encoded.as_slice());
    assert_eq!(signing.len(), 19 + encoded.len());
    assert_eq!(record.signing_input().expect("method"), signing);

    assert_eq!(SIGN_ACTIVATION_T_DOMAIN, b"MN/v1/activation-t\0");
    assert_eq!(SIGN_ACTIVATION_T_DOMAIN.len(), 19);
    assert_eq!(SIGN_ACTIVATION_T_DOMAIN.last(), Some(&0x00));
    assert!(domain_ends_with_nul(SIGN_ACTIVATION_T_DOMAIN));
    assert!(!SIGN_ACTIVATION_T_DOMAIN.ends_with(b"\\0"));

    // 专用域：不得与任何既有签名域/AAD 复用。
    for other in [
        AAD_DOMAIN,
        CURSOR_HMAC_DOMAIN,
        SIGN_AUTH_DOMAIN,
        SIGN_MANAGE_DOMAIN,
        SIGN_OBJECT_COMMIT_DOMAIN,
        SIGN_DEVICE_CERT_DOMAIN,
        SIGN_CHECKPOINT_DOMAIN,
    ] {
        assert_ne!(SIGN_ACTIVATION_T_DOMAIN, other);
    }
}
