//! ADR 0015 S4a1：ManagementCapV2 系列的 golden vectors、严格 CDE 与签名域测试。
//!
//! `tests/golden_management_cap_v2.hex` 由**独立 Python 脚本**按 ADR 字段表手工拼
//! CBOR（RFC 8949 canonical）并用 hashlib 算 SHA-256 生成；本文件只读 hex 比对，
//! 不参与生成——否则 golden 只是实现的镜像，锁不住字段编号与域。
//!
//! 测试内的 `sha256` 是独立实现（sync-proto 无 crypto 依赖），先用 FIPS 向量自检，
//! 再用于复算 `h=SHA256(CDE(OperationV2))` 与链 hash。

use std::fmt::Write as _;

use menghunote_sync_proto::*;

// —— 独立 SHA-256（仅测试用）——

const K: [u32; 64] = [
    0x428a_2f98,
    0x7137_4491,
    0xb5c0_fbcf,
    0xe9b5_dba5,
    0x3956_c25b,
    0x59f1_11f1,
    0x923f_82a4,
    0xab1c_5ed5,
    0xd807_aa98,
    0x1283_5b01,
    0x2431_85be,
    0x550c_7dc3,
    0x72be_5d74,
    0x80de_b1fe,
    0x9bdc_06a7,
    0xc19b_f174,
    0xe49b_69c1,
    0xefbe_4786,
    0x0fc1_9dc6,
    0x240c_a1cc,
    0x2de9_2c6f,
    0x4a74_84aa,
    0x5cb0_a9dc,
    0x76f9_88da,
    0x983e_5152,
    0xa831_c66d,
    0xb003_27c8,
    0xbf59_7fc7,
    0xc6e0_0bf3,
    0xd5a7_9147,
    0x06ca_6351,
    0x1429_2967,
    0x27b7_0a85,
    0x2e1b_2138,
    0x4d2c_6dfc,
    0x5338_0d13,
    0x650a_7354,
    0x766a_0abb,
    0x81c2_c92e,
    0x9272_2c85,
    0xa2bf_e8a1,
    0xa81a_664b,
    0xc24b_8b70,
    0xc76c_51a3,
    0xd192_e819,
    0xd699_0624,
    0xf40e_3585,
    0x106a_a070,
    0x19a4_c116,
    0x1e37_6c08,
    0x2748_774c,
    0x34b0_bcb5,
    0x391c_0cb3,
    0x4ed8_aa4a,
    0x5b9c_ca4f,
    0x682e_6ff3,
    0x748f_82ee,
    0x78a5_636f,
    0x84c8_7814,
    0x8cc7_0208,
    0x90be_fffa,
    0xa450_6ceb,
    0xbef9_a3f7,
    0xc671_78f2,
];

#[allow(clippy::many_single_char_names)]
fn sha256(bytes: &[u8]) -> [u8; 32] {
    let mut state = [
        0x6a09_e667u32,
        0xbb67_ae85,
        0x3c6e_f372,
        0xa54f_f53a,
        0x510e_527f,
        0x9b05_688c,
        0x1f83_d9ab,
        0x5be0_cd19,
    ];
    let bit_len = (bytes.len() as u64).wrapping_mul(8);
    let mut data = bytes.to_vec();
    data.push(0x80);
    while data.len() % 64 != 56 {
        data.push(0);
    }
    data.extend_from_slice(&bit_len.to_be_bytes());

    let (blocks, _) = data.as_chunks::<64>();
    for chunk in blocks {
        let mut w = [0u32; 64];
        let (words, _) = chunk.as_chunks::<4>();
        for (word, bytes) in w.iter_mut().take(16).zip(words) {
            *word = u32::from_be_bytes(*bytes);
        }
        for index in 16..64 {
            let s0 = w[index - 15].rotate_right(7)
                ^ w[index - 15].rotate_right(18)
                ^ (w[index - 15] >> 3);
            let s1 = w[index - 2].rotate_right(17)
                ^ w[index - 2].rotate_right(19)
                ^ (w[index - 2] >> 10);
            w[index] = w[index - 16]
                .wrapping_add(s0)
                .wrapping_add(w[index - 7])
                .wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d) = (state[0], state[1], state[2], state[3]);
        let (mut e, mut f, mut g, mut h) = (state[4], state[5], state[6], state[7]);
        for (&k, &wi) in K.iter().zip(w.iter()) {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(k)
                .wrapping_add(wi);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
        state[4] = state[4].wrapping_add(e);
        state[5] = state[5].wrapping_add(f);
        state[6] = state[6].wrapping_add(g);
        state[7] = state[7].wrapping_add(h);
    }

    let mut out = [0u8; 32];
    let (words, _) = out.as_chunks_mut::<4>();
    for (chunk, word) in words.iter_mut().zip(state.iter()) {
        *chunk = word.to_be_bytes();
    }
    out
}

fn hash_fn(bytes: &[u8]) -> [u8; 32] {
    sha256(bytes)
}

// —— golden / hex 工具 ——

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut out, "{byte:02x}").expect("写 hex");
    }
    out
}

fn golden(name: &str) -> String {
    let prefix = format!("{name} ");
    include_str!("golden_management_cap_v2.hex")
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

fn signature(value: u8) -> [u8; 64] {
    [value; 64]
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

/// 不定长 map：`0xbf ... 0xff`。
fn raw_map_indefinite(entries: &[(u64, Vec<u8>)]) -> Vec<u8> {
    let mut out = vec![0xbf];
    for (key, value) in entries {
        out.extend(cbor_head(0, *key));
        out.extend_from_slice(value);
    }
    out.push(0xff);
    out
}

fn mutate_fields(bytes: &[u8], mutate: impl FnOnce(&mut Vec<(u64, CdeValue)>)) -> Vec<u8> {
    let mut entries = entries_of(bytes);
    mutate(&mut entries);
    encode_entries(entries)
}

fn field(entries: &[(u64, CdeValue)], key: u64) -> &CdeValue {
    &entries[key as usize].1
}

/// 所有对象共用的严格 CDE 负例：乱序/重复/未知/缺字段/非最短/尾随/truncated/null/tag/float/不定长。
fn assert_strict_cde_rejections(encoded: &[u8], label: &str, accepted: impl Fn(&[u8]) -> bool) {
    let raw = raw_entries(encoded);

    // 单字段 payload（如 CapGrant）没有"乱序"可测，跳过该负例。
    if raw.len() >= 2 {
        let mut shuffled = raw.clone();
        shuffled.swap(0, 1);
        assert!(
            !accepted(&raw_map(&shuffled)),
            "{label}: map 键乱序必须拒绝"
        );
    }

    let mut duplicated = raw.clone();
    duplicated.insert(1, duplicated[0].clone());
    assert!(!accepted(&raw_map(&duplicated)), "{label}: 重复键必须拒绝");

    let mut unknown = raw.clone();
    unknown.push((0x7fff, vec![0x00]));
    assert!(!accepted(&raw_map(&unknown)), "{label}: 未知键必须拒绝");

    let mut missing = raw.clone();
    missing.pop();
    assert!(!accepted(&raw_map(&missing)), "{label}: 缺字段必须拒绝");

    let mut non_shortest = raw.clone();
    non_shortest[0].1 = vec![0x18, 0x02];
    assert!(
        !accepted(&raw_map(&non_shortest)),
        "{label}: 非最短整数必须拒绝"
    );

    let mut null = raw.clone();
    null[0].1 = vec![0xf6];
    assert!(!accepted(&raw_map(&null)), "{label}: null 必须拒绝");

    let mut tag = raw.clone();
    tag[0].1 = vec![0xc0, 0x00];
    assert!(!accepted(&raw_map(&tag)), "{label}: tag 必须拒绝");

    let mut float = raw.clone();
    float[0].1 = vec![0xfb, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
    assert!(!accepted(&raw_map(&float)), "{label}: float 必须拒绝");

    assert!(
        !accepted(&raw_map_indefinite(&raw)),
        "{label}: 不定长 map 必须拒绝"
    );

    let mut trailing = encoded.to_vec();
    trailing.push(0x00);
    assert!(!accepted(&trailing), "{label}: 尾随字节必须拒绝");

    assert!(
        !accepted(&encoded[..encoded.len() - 1]),
        "{label}: 截断必须拒绝"
    );
    assert!(!accepted(&[]), "{label}: 空输入必须拒绝");
}

// —— 固定样本（与 Python golden 生成脚本逐字段对应）——

fn sample_cap() -> ManagementCapV2 {
    ManagementCapV2 {
        version: V2_WIRE_VERSION,
        instance_id: ids(0x12),
        vault_id: ids(0x11),
        cap_id: ids(0x13),
        subject_device_id: ids(0x21),
        subject_ed_pk: hash(0x31),
        scopes: CapScope::new(CapScope::INVITE | CapScope::APPROVE | CapScope::CAP_ADMIN)
            .expect("scopes"),
        not_before: 1_700_000_000,
        expires_at: 1_700_086_400,
        issued_control_rev: 7,
        issued_authz_rev: 8,
        issuer_authority_rev: 1,
        delegable: false,
    }
}

fn sample_policy() -> PolicyV2 {
    PolicyV2 {
        version: V2_WIRE_VERSION,
        highrisk_threshold: PolicyV2::HIGHRISK_THRESHOLD,
        min_quorum: PolicyV2::MIN_QUORUM,
        max_cap_ttl: MAX_CAP_TTL_SECS_V2,
        max_invite_ttl: MAX_INVITE_TTL_SECS_V2,
    }
}

fn sample_signer_device() -> SignerV2 {
    SignerV2 {
        role: SignerRole::Device,
        device_id: Some(ids(0x21)),
        cap_id: Some(ids(0x13)),
        pk: hash(0x31),
    }
}

fn sample_signer_creator() -> SignerV2 {
    SignerV2 {
        role: SignerRole::CreatorUpgrade,
        device_id: None,
        cap_id: None,
        pk: hash(0x32),
    }
}

fn sample_signer_pop() -> SignerV2 {
    SignerV2 {
        role: SignerRole::TargetPop,
        device_id: Some(ids(0x22)),
        cap_id: None,
        pk: hash(0x33),
    }
}

fn sample_operation() -> OperationV2 {
    let payload = encode_cde(&CdeValue::Map(vec![(
        0,
        CdeValue::Bytes(ids(0x13).to_vec()),
    )]))
    .expect("payload cde");
    OperationV2 {
        version: V2_WIRE_VERSION,
        instance_id: ids(0x12),
        vault_id: ids(0x11),
        kind: ControlKindV2::CapRevoke,
        request_id: ids(0x41),
        expected_control_rev: 7,
        prev_control_hash: hash(0x51),
        expected_authz_rev: 8,
        authority_rev: 1,
        signed_at: 1_700_000_000,
        not_after: 1_700_000_300,
        canonical_path: canonical_submit_path(&ids(0x11)),
        payload,
        signers: vec![
            SignerV2 {
                role: SignerRole::Device,
                device_id: Some(ids(0x21)),
                cap_id: None,
                pk: hash(0x31),
            },
            SignerV2 {
                role: SignerRole::Device,
                device_id: Some(ids(0x22)),
                cap_id: Some(ids(0x13)),
                pk: hash(0x32),
            },
        ],
    }
}

fn sample_signed_control() -> SignedControlV2 {
    SignedControlV2 {
        operation: sample_operation(),
        signatures: vec![signature(0xA1), signature(0xA2)],
    }
}

fn sample_device_cert() -> DeviceCertV2 {
    DeviceCertV2 {
        version: V2_WIRE_VERSION,
        instance_id: ids(0x12),
        vault_id: ids(0x11),
        device_id: ids(0x22),
        sign_ed25519_pub: hash(0x34),
        hpke_x25519_pub: hash(0x62),
        cert_serial: 3,
        data_rights: Rights::new(Rights::ALL).expect("rights"),
        issued_authz_rev: 8,
    }
}

fn sample_grant_aad() -> GrantAadV2 {
    GrantAadV2 {
        version: V2_WIRE_VERSION,
        instance_id: ids(0x12),
        vault_id: ids(0x11),
        epoch: 2,
        recipient_device_id: ids(0x22),
        recipient_hpke_pub: hash(0x61),
        authority_rev: 1,
        grant_id: ids(0x71),
        issued_authz_rev: 8,
    }
}

fn sample_grant_package() -> GrantPackageV2 {
    GrantPackageV2 {
        aad: sample_grant_aad(),
        enc: hash(0x63),
        ciphertext: vec![0xC3; 48],
    }
}

fn sample_activation() -> ActivationV2 {
    ActivationV2 {
        version: V2_WIRE_VERSION,
        rotation_id: ids(0x81),
        from_epoch: 2,
        to_epoch: 3,
        freeze_seq: 42,
        primary_cid: ids(0x82),
        primary_commit_hash: hash(0x91),
        fallback_cid: ids(0x83),
        fallback_commit_hash: hash(0x92),
        slot_set_hash: hash(0x93),
        member_cert_hash: hash(0x94),
        grants_hash: hash(0x95),
        old_asset_hash: hash(0x96),
        new_asset_hash: hash(0x97),
    }
}

// —— 测试 ——

#[test]
fn sha256_matches_fips_vectors() {
    assert_eq!(
        hex(&sha256(b"")),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(
        hex(&sha256(b"abc")),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(
        hex(&sha256(
            b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
        )),
        "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
    );
}

#[test]
fn management_cap_v2_golden_layout_and_round_trip() {
    let cap = sample_cap();
    let encoded = cap.encode().expect("encode");
    assert_eq!(hex(&encoded), golden("cap"));
    assert_eq!(MANAGEMENT_CAP_V2_FIELD_COUNT, 13);

    let decoded = ManagementCapV2::decode(&encoded).expect("decode");
    assert_eq!(decoded, cap);
    assert_eq!(decoded.encode().expect("re-encode"), encoded);

    let entries = entries_of(&encoded);
    assert_eq!(
        entries.iter().map(|(key, _)| *key).collect::<Vec<_>>(),
        (0..13).collect::<Vec<_>>(),
        "字段编号必须恰好是 0..13 的连续升序"
    );
    for (key, value) in entries {
        match key {
            0 | 6 | 7 | 8 | 9 | 10 | 11 => {
                assert!(matches!(value, CdeValue::UInt(_)), "key {key} 应为 uint")
            }
            12 => assert_eq!(value, CdeValue::Bool(false), "delegable 必须编码成 false"),
            1..=4 => {
                let CdeValue::Bytes(bytes) = value else {
                    panic!("key {key} 应为 byte string");
                };
                assert_eq!(bytes.len(), 16, "key {key} 长度必须 16B");
            }
            5 => {
                let CdeValue::Bytes(bytes) = value else {
                    panic!("key 5 应为 byte string");
                };
                assert_eq!(bytes.len(), 32, "key 5 长度必须 32B");
            }
            other => panic!("未知键 {other}"),
        }
    }

    // 同一语义值、条目逆序 → canonical 编码器仍输出同一字节串。
    let mut reversed = entries_of(&encoded);
    reversed.reverse();
    assert_eq!(encode_entries(reversed), encoded);
}

#[test]
fn policy_v2_golden_and_fixed_fields() {
    let policy = sample_policy();
    let encoded = policy.encode().expect("encode");
    assert_eq!(hex(&encoded), golden("policy"));
    assert_eq!(POLICY_V2_FIELD_COUNT, 5);
    assert_eq!(PolicyV2::decode(&encoded).expect("decode"), policy);

    let entries = entries_of(&encoded);
    for (key, value) in &entries {
        assert!(matches!(value, CdeValue::UInt(_)), "key {key} 应为 uint");
    }
    assert_eq!(field(&entries, 0), &CdeValue::UInt(2));
    assert_eq!(field(&entries, 1), &CdeValue::UInt(2));
    assert_eq!(field(&entries, 2), &CdeValue::UInt(2));
    assert_eq!(field(&entries, 3), &CdeValue::UInt(MAX_CAP_TTL_SECS_V2));
    assert_eq!(field(&entries, 4), &CdeValue::UInt(MAX_INVITE_TTL_SECS_V2));

    // 固定值/上限之外必须拒绝（base 是合法上界，取 +1）。
    for broken in [
        PolicyV2 {
            version: 1,
            ..policy.clone()
        },
        PolicyV2 {
            highrisk_threshold: 1,
            ..policy.clone()
        },
        PolicyV2 {
            min_quorum: 3,
            ..policy.clone()
        },
        PolicyV2 {
            max_cap_ttl: MAX_CAP_TTL_SECS_V2 + 1,
            ..policy.clone()
        },
        PolicyV2 {
            max_invite_ttl: MAX_INVITE_TTL_SECS_V2 + 1,
            ..policy.clone()
        },
    ] {
        assert!(broken.validate().is_err(), "{broken:?}");
        assert!(broken.encode().is_err(), "{broken:?}");
    }
}

#[test]
fn signer_v2_golden_and_role_rules() {
    let device = sample_signer_device();
    let creator = sample_signer_creator();
    let pop = sample_signer_pop();

    assert_eq!(
        hex(&device.encode().expect("device encode")),
        golden("signer_device")
    );
    assert_eq!(
        hex(&creator.encode().expect("creator encode")),
        golden("signer_creator")
    );
    assert_eq!(
        hex(&pop.encode().expect("pop encode")),
        golden("signer_pop")
    );

    for signer in [device.clone(), creator.clone(), pop.clone()] {
        let encoded = signer.encode().expect("encode");
        assert_eq!(SignerV2::decode(&encoded).expect("decode"), signer);
        let entries = entries_of(&encoded);
        assert_eq!(
            entries.iter().map(|(key, _)| *key).collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );
    }

    // 可选 ID 的 wire 形状：0 或 16B。
    assert_eq!(
        field(&entries_of(&device.encode().expect("e")), 1),
        &CdeValue::Bytes(ids(0x21).to_vec())
    );
    assert_eq!(
        field(&entries_of(&creator.encode().expect("e")), 1),
        &CdeValue::Bytes(Vec::new())
    );

    // 角色/必填性/长度/全零负例。
    let pk = hash(0x31);
    for broken in [
        SignerV2 {
            role: SignerRole::Device,
            device_id: None,
            cap_id: None,
            pk,
        },
        SignerV2 {
            role: SignerRole::TargetPop,
            device_id: Some(ids(0x21)),
            cap_id: Some(ids(0x13)),
            pk,
        },
        SignerV2 {
            role: SignerRole::CreatorUpgrade,
            device_id: Some(ids(0x21)),
            cap_id: None,
            pk,
        },
        SignerV2 {
            role: SignerRole::OfflineRecovery,
            device_id: None,
            cap_id: Some(ids(0x13)),
            pk,
        },
        SignerV2 {
            role: SignerRole::Device,
            device_id: Some([0; 16]),
            cap_id: None,
            pk,
        },
        SignerV2 {
            role: SignerRole::Device,
            device_id: Some(ids(0x21)),
            cap_id: None,
            pk: [0; 32],
        },
    ] {
        assert!(broken.validate().is_err(), "{broken:?}");
        assert!(broken.encode().is_err(), "{broken:?}");
    }

    // 未知 role、以及 15B/17B 的 device_id/cap_id。
    let encoded = device.encode().expect("encode");
    for role in [4_u64, 5, 255] {
        let mutated = mutate_fields(&encoded, |entries| entries[0].1 = CdeValue::UInt(role));
        assert!(
            SignerV2::decode(&mutated).is_err(),
            "未知 role {role} 必须拒绝"
        );
    }
    for (key, len) in [(1_u64, 15_usize), (1, 17), (2, 15), (2, 17)] {
        let mutated = mutate_fields(&encoded, |entries| {
            entries[key as usize].1 = CdeValue::Bytes(vec![0x21; len]);
        });
        assert!(
            SignerV2::decode(&mutated).is_err(),
            "key {key} 长度 {len} 必须拒绝"
        );
    }
}

#[test]
fn operation_v2_golden_layout_and_signer_order() {
    let operation = sample_operation();
    let encoded = operation.encode().expect("encode");
    assert_eq!(hex(&encoded), golden("operation"));
    assert_eq!(OPERATION_V2_FIELD_COUNT, 15);
    assert_eq!(OperationV2::decode(&encoded).expect("decode"), operation);

    let entries = entries_of(&encoded);
    assert_eq!(entries.len(), OPERATION_V2_FIELD_COUNT);
    assert_eq!(
        entries.iter().map(|(key, _)| *key).collect::<Vec<_>>(),
        (0..15).collect::<Vec<_>>()
    );
    assert_eq!(field(&entries, 0), &CdeValue::UInt(2));
    assert_eq!(field(&entries, 11), &CdeValue::UInt(2), "method 固定 POST");
    assert_eq!(
        field(&entries, 12),
        &CdeValue::Text(canonical_submit_path(&ids(0x11)))
    );
    match field(&entries, 13) {
        CdeValue::Bytes(payload) => {
            assert_eq!(payload, &operation.payload, "payload 是 bytes(CDE)");
        }
        other => panic!("payload 必须是 byte string，实际 {other:?}"),
    }
    assert!(matches!(field(&entries, 14), CdeValue::Array(_)));
    assert_eq!(
        operation.payload_cde().expect("payload cde"),
        CdeValue::Map(vec![(0, CdeValue::Bytes(ids(0x13).to_vec()))])
    );

    // signer 数组必须按 (role,device,cap,pk) 严格升序且不重复。
    let mut swapped = operation.clone();
    swapped.signers.swap(0, 1);
    assert!(swapped.validate().is_err(), "signer 逆序必须拒绝");

    let mut duplicated = operation.clone();
    duplicated.signers[1] = duplicated.signers[0].clone();
    assert!(duplicated.validate().is_err(), "重复 signer 必须拒绝");

    let mut empty = operation.clone();
    empty.signers.clear();
    assert!(empty.validate().is_err(), "空 signer 必须拒绝");

    let mut too_many = operation.clone();
    too_many.signers = (0..=MAX_OPERATION_SIGNERS_V2 as u8)
        .map(|index| SignerV2 {
            role: SignerRole::Device,
            device_id: Some(ids(index)),
            cap_id: None,
            pk: hash(index),
        })
        .collect();
    assert!(too_many.validate().is_err(), "signer 超上限必须拒绝");

    // 空派生：cap 为 None 的 signer 排在带 cap 的同 role/device 之前。
    let mut ordered = operation.clone();
    ordered.signers = vec![
        SignerV2 {
            role: SignerRole::Device,
            device_id: Some(ids(0x21)),
            cap_id: None,
            pk: hash(0x31),
        },
        SignerV2 {
            role: SignerRole::Device,
            device_id: Some(ids(0x21)),
            cap_id: Some(ids(0x13)),
            pk: hash(0x32),
        },
    ];
    assert!(ordered.validate().is_ok());
    ordered.signers.swap(0, 1);
    assert!(ordered.validate().is_err());
}

#[test]
fn operation_v2_rejects_bad_kind_method_path_payload_and_ranges() {
    let operation = sample_operation();
    let encoded = operation.encode().expect("encode");

    // kind：15 未分配，17/255 未知。
    for kind in [15_u64, 17, 255] {
        let mutated = mutate_fields(&encoded, |entries| entries[3].1 = CdeValue::UInt(kind));
        assert!(
            matches!(OperationV2::decode(&mutated), Err(ProtoError::Invalid(_))),
            "kind {kind} 必须拒绝"
        );
    }

    // method 必须是 2(POST)。
    let mutated = mutate_fields(&encoded, |entries| entries[11].1 = CdeValue::UInt(1));
    assert!(OperationV2::decode(&mutated).is_err());

    // 唯一提交路径：不允许 query/fragment/转义/大写 hex/别的 vault/点段/非 ASCII。
    let upper = format!("/v2/vaults/{}/control/submit", "AB".repeat(16));
    let cases = [
        String::new(),
        "relative/path".to_owned(),
        format!(
            "{}/control/submit/?x=1",
            "/v2/vaults/".to_owned() + &"11".repeat(16)
        ),
        format!(
            "{}/control/submit#frag",
            "/v2/vaults/".to_owned() + &"11".repeat(16)
        ),
        format!(
            "{}/control/submit/",
            "/v2/vaults/".to_owned() + &"11".repeat(16)
        ),
        format!("/v2/vaults/{}/control/submit", "22".repeat(16)),
        "/v2/./vaults/11/control/submit".to_owned(),
        "/v2/vaults/../../../control/submit".to_owned(),
        "/v2/vaults/①①①①①①①①①①①①①①①①/control/submit".to_owned(),
        upper,
    ];
    for path in cases {
        let mutated = mutate_fields(&encoded, |entries| {
            entries[12].1 = CdeValue::Text(path.clone());
        });
        assert!(
            OperationV2::decode(&mutated).is_err(),
            "非法 canonical_path {path:?} 必须拒绝"
        );
    }
    // 大写的 vault 与 path 同时改写：hex 必须小写。
    let mut upper_vault = operation.clone();
    upper_vault.vault_id = ids(0xAB);
    upper_vault.canonical_path = format!("/v2/vaults/{}/control/submit", "AB".repeat(16));
    assert!(upper_vault.validate().is_err());

    // payload：空 / 非 canonical / 非法 CDE。
    for payload in [
        Vec::new(),
        vec![0xa2, 0x01, 0x00, 0x00, 0x00],
        vec![0xff],
        vec![0xf6],
    ] {
        let mutated = mutate_fields(&encoded, |entries| {
            entries[13].1 = CdeValue::Bytes(payload.clone());
        });
        assert!(
            OperationV2::decode(&mutated).is_err(),
            "非法 payload {payload:?} 必须拒绝"
        );
    }

    // 时间/rev 的 i64 范围与顺序。
    let mutated = mutate_fields(&encoded, |entries| {
        entries[5].1 = CdeValue::UInt(i64::MAX as u64 + 1);
    });
    assert!(OperationV2::decode(&mutated).is_err());
    let mutated = mutate_fields(&encoded, |entries| {
        entries[10].1 = CdeValue::UInt(operation.signed_at - 1);
    });
    assert!(
        OperationV2::decode(&mutated).is_err(),
        "not_after < signed_at 必须拒绝"
    );

    // 全零 ID。
    let mut zero_instance = operation.clone();
    zero_instance.instance_id = [0; 16];
    assert!(zero_instance.validate().is_err(), "instance 全零必须拒绝");
    let mut zero_vault = operation.clone();
    zero_vault.vault_id = [0; 16];
    assert!(zero_vault.validate().is_err(), "vault 全零必须拒绝");
    let mut zero_request = operation.clone();
    zero_request.request_id = [0; 16];
    assert!(zero_request.validate().is_err(), "request 全零必须拒绝");

    // prev_control_hash 允许全零（链首），但版本必须 2。
    let mut genesis = operation.clone();
    genesis.prev_control_hash = [0; 32];
    assert!(genesis.validate().is_ok());
    let mut wrong_version = operation;
    wrong_version.version = 1;
    assert!(wrong_version.validate().is_err());
    let mutated = mutate_fields(&encoded, |entries| entries[0].1 = CdeValue::UInt(1));
    assert!(matches!(
        OperationV2::decode(&mutated),
        Err(ProtoError::UnsupportedVersion(1))
    ));
}

#[test]
fn signed_control_v2_golden_and_signature_count() {
    let signed = sample_signed_control();
    let encoded = signed.encode().expect("encode");
    assert_eq!(hex(&encoded), golden("signed_control"));
    assert_eq!(SIGNED_CONTROL_V2_FIELD_COUNT, 2);
    assert_eq!(SignedControlV2::decode(&encoded).expect("decode"), signed);

    let entries = entries_of(&encoded);
    assert_eq!(
        field(&entries, 0),
        &CdeValue::Bytes(signed.operation.encode().expect("op"))
    );
    match field(&entries, 1) {
        CdeValue::Array(signatures) => {
            assert_eq!(signatures.len(), 2);
            for (signature, value) in signatures.iter().zip([0xA1_u8, 0xA2]) {
                assert_eq!(signature, &CdeValue::Bytes(vec![value; 64]));
            }
        }
        other => panic!("signatures 必须是 array，实际 {other:?}"),
    }

    // 签名数必须与 signer 一一对应。
    for count in [0_usize, 1, 3] {
        let mut broken = signed.clone();
        broken.signatures = vec![signature(0xA1); count];
        assert!(broken.validate().is_err(), "{count} 个签名必须拒绝");
    }

    // 长度必须 64B。
    for len in [63_usize, 65] {
        let mutated = mutate_fields(&encoded, |entries| {
            entries[1].1 = CdeValue::Array(vec![
                CdeValue::Bytes(vec![0xA1; len]),
                CdeValue::Bytes(vec![0xA2; 64]),
            ]);
        });
        assert!(
            SignedControlV2::decode(&mutated).is_err(),
            "签名长度 {len} 必须拒绝"
        );
    }

    // 内层 operation 必须 canonical 且是合法 OperationV2。
    for operation_bytes in [
        vec![0xa2, 0x01, 0x00, 0x00, 0x00],
        vec![0xff],
        b"not-cbor".to_vec(),
    ] {
        let mutated = mutate_fields(&encoded, |entries| {
            entries[0].1 = CdeValue::Bytes(operation_bytes.clone());
        });
        assert!(
            SignedControlV2::decode(&mutated).is_err(),
            "非法内层 operation 必须拒绝"
        );
    }
}

#[test]
fn device_cert_v2_golden_and_rights() {
    let cert = sample_device_cert();
    let encoded = cert.encode().expect("encode");
    assert_eq!(hex(&encoded), golden("device_cert"));
    assert_eq!(DEVICE_CERT_V2_FIELD_COUNT, 9);
    assert_eq!(DeviceCertV2::decode(&encoded).expect("decode"), cert);

    let entries = entries_of(&encoded);
    assert_eq!(field(&entries, 0), &CdeValue::UInt(2));
    assert_eq!(field(&entries, 6), &CdeValue::UInt(3));
    assert_eq!(field(&entries, 7), &CdeValue::UInt(3), "READ|WRITE");
    assert_eq!(field(&entries, 8), &CdeValue::UInt(8));

    // 管理 scope 位不得混进 data_rights；serial/rev 必须 >0。
    for broken in [
        DeviceCertV2 {
            data_rights: Rights(0),
            ..cert.clone()
        },
        DeviceCertV2 {
            data_rights: Rights(4),
            ..cert.clone()
        },
        DeviceCertV2 {
            data_rights: Rights(0xffff_ffff),
            ..cert.clone()
        },
        DeviceCertV2 {
            cert_serial: 0,
            ..cert.clone()
        },
        DeviceCertV2 {
            issued_authz_rev: 0,
            ..cert.clone()
        },
        DeviceCertV2 {
            version: 1,
            ..cert.clone()
        },
        DeviceCertV2 {
            device_id: [0; 16],
            ..cert.clone()
        },
        DeviceCertV2 {
            sign_ed25519_pub: [0; 32],
            ..cert.clone()
        },
    ] {
        assert!(broken.validate().is_err(), "{broken:?}");
        assert!(broken.encode().is_err(), "{broken:?}");
    }

    // u 范围。
    let mutated = mutate_fields(&encoded, |entries| {
        entries[6].1 = CdeValue::UInt(i64::MAX as u64 + 1);
    });
    assert!(DeviceCertV2::decode(&mutated).is_err());
}

#[test]
fn grant_aad_v2_and_package_golden() {
    let aad = sample_grant_aad();
    let aad_encoded = aad.encode().expect("encode aad");
    assert_eq!(hex(&aad_encoded), golden("grant_aad"));
    assert_eq!(GRANT_AAD_V2_FIELD_COUNT, 9);
    assert_eq!(GrantAadV2::decode(&aad_encoded).expect("decode"), aad);
    assert_eq!(aad.aad_bytes().expect("aad bytes"), aad_encoded);

    let package = sample_grant_package();
    let package_encoded = package.encode().expect("encode package");
    assert_eq!(hex(&package_encoded), golden("grant_package"));
    assert_eq!(GRANT_PACKAGE_V2_FIELD_COUNT, 3);
    assert_eq!(
        GrantPackageV2::decode(&package_encoded).expect("decode"),
        package
    );
    let entries = entries_of(&package_encoded);
    assert_eq!(
        field(&entries, 0),
        &CdeValue::Bytes(aad_encoded.clone()),
        "包内 AAD 必须是 canonical aad_bytes"
    );
    assert_eq!(field(&entries, 1), &CdeValue::Bytes(hash(0x63).to_vec()));
    assert_eq!(field(&entries, 2), &CdeValue::Bytes(vec![0xC3; 48]));

    // AAD 语义负例。
    for broken in [
        GrantAadV2 {
            epoch: 0,
            ..aad.clone()
        },
        GrantAadV2 {
            authority_rev: 0,
            ..aad.clone()
        },
        GrantAadV2 {
            issued_authz_rev: 0,
            ..aad.clone()
        },
        GrantAadV2 {
            version: 1,
            ..aad.clone()
        },
        GrantAadV2 {
            grant_id: [0; 16],
            ..aad.clone()
        },
        GrantAadV2 {
            recipient_hpke_pub: [0; 32],
            ..aad.clone()
        },
    ] {
        assert!(broken.validate().is_err(), "{broken:?}");
        assert!(broken.encode().is_err(), "{broken:?}");
    }
    let mutated = mutate_fields(&aad_encoded, |entries| {
        entries[3].1 = CdeValue::UInt(i64::MAX as u64 + 1);
    });
    assert!(GrantAadV2::decode(&mutated).is_err());

    // 包：enc 必须 32B 且非全零；ciphertext 非空；AAD 必须 canonical。
    for broken in [
        GrantPackageV2 {
            enc: [0; 32],
            ..package.clone()
        },
        GrantPackageV2 {
            ciphertext: Vec::new(),
            ..package.clone()
        },
    ] {
        assert!(broken.validate().is_err(), "{broken:?}");
        assert!(broken.encode().is_err(), "{broken:?}");
    }
    for len in [0_usize, 31, 33] {
        let mutated = mutate_fields(&package_encoded, |entries| {
            entries[1].1 = CdeValue::Bytes(vec![0x63; len]);
        });
        assert!(
            GrantPackageV2::decode(&mutated).is_err(),
            "enc 长度 {len} 必须拒绝"
        );
    }
    for aad_bytes in [vec![0xa2, 0x01, 0x00, 0x00, 0x00], vec![0xff]] {
        let mutated = mutate_fields(&package_encoded, |entries| {
            entries[0].1 = CdeValue::Bytes(aad_bytes.clone());
        });
        assert!(
            GrantPackageV2::decode(&mutated).is_err(),
            "非 canonical AAD 必须拒绝"
        );
    }
}

#[test]
fn activation_v2_golden_and_epoch_order() {
    let activation = sample_activation();
    let encoded = activation.encode().expect("encode");
    assert_eq!(hex(&encoded), golden("activation"));
    assert_eq!(ACTIVATION_V2_FIELD_COUNT, 14);
    assert_eq!(ActivationV2::decode(&encoded).expect("decode"), activation);

    let entries = entries_of(&encoded);
    assert_eq!(
        entries.iter().map(|(key, _)| *key).collect::<Vec<_>>(),
        (0..14).collect::<Vec<_>>()
    );
    assert_eq!(field(&entries, 0), &CdeValue::UInt(2));
    for (key, value) in &entries {
        match key {
            0 | 2 | 3 | 4 => assert!(matches!(value, CdeValue::UInt(_)), "key {key} 应为 uint"),
            1 | 5 | 7 => {
                let CdeValue::Bytes(bytes) = value else {
                    panic!("key {key} 应为 byte string");
                };
                assert_eq!(bytes.len(), 16);
            }
            6 | 8 | 9 | 10 | 11 | 12 | 13 => {
                let CdeValue::Bytes(bytes) = value else {
                    panic!("key {key} 应为 byte string");
                };
                assert_eq!(bytes.len(), 32);
            }
            other => panic!("未知键 {other}"),
        }
    }

    for broken in [
        ActivationV2 {
            from_epoch: 0,
            ..activation.clone()
        },
        ActivationV2 {
            to_epoch: 2,
            ..activation.clone()
        },
        ActivationV2 {
            to_epoch: 1,
            ..activation.clone()
        },
        ActivationV2 {
            rotation_id: [0; 16],
            ..activation.clone()
        },
        ActivationV2 {
            primary_cid: [0; 16],
            ..activation.clone()
        },
        ActivationV2 {
            grants_hash: [0; 32],
            ..activation.clone()
        },
        ActivationV2 {
            version: 1,
            ..activation.clone()
        },
    ] {
        assert!(broken.validate().is_err(), "{broken:?}");
        assert!(broken.encode().is_err(), "{broken:?}");
    }

    // restart 可跨多个已分配 epoch：只要求 to > from。
    let restarted = ActivationV2 {
        to_epoch: 11,
        ..activation.clone()
    };
    assert!(restarted.validate().is_ok());
    // 空库首次激活 S 可以为 0。
    let empty = ActivationV2 {
        freeze_seq: 0,
        ..activation
    };
    assert!(empty.validate().is_ok());
}

#[test]
fn signing_domains_golden_and_domain_discipline() {
    let operation = sample_operation();
    let digest = operation.digest(hash_fn).expect("digest");
    assert_eq!(hex(&digest), golden("operation_digest"));
    assert_eq!(digest, sha256(&operation.encode().expect("encode")));

    let control = control_signing_input(&operation, hash_fn).expect("control input");
    assert_eq!(hex(&control), golden("control_signing_input"));
    assert_eq!(&control[..14], SIGN_V2_CONTROL_DOMAIN);
    assert_eq!(&control[14..], &digest[..]);
    assert_eq!(control.len(), SIGN_V2_CONTROL_DOMAIN.len() + 32);

    let upgrade = upgrade_signing_input(&operation, hash_fn).expect("upgrade input");
    assert_eq!(hex(&upgrade), golden("upgrade_signing_input"));
    assert_eq!(&upgrade[..14], SIGN_V2_UPGRADE_DOMAIN);
    assert_eq!(&upgrade[14..], &digest[..]);

    let recover = recover_signing_input(&operation, hash_fn).expect("recover input");
    assert_eq!(hex(&recover), golden("recover_signing_input"));
    assert_eq!(&recover[..14], SIGN_V2_RECOVER_DOMAIN);

    let pop = pop_signing_input(&operation, &hash(0x31), hash_fn).expect("pop input");
    assert_eq!(hex(&pop), golden("pop_signing_input"));
    assert_eq!(&pop[..10], SIGN_V2_POP_DOMAIN);
    assert_eq!(&pop[10..42], &digest[..]);
    assert_eq!(&pop[42..], &hash(0x31)[..]);

    let challenge = vec![0xCC; 32];
    assert_eq!(
        hex(&auth_signing_input_v2(&challenge)),
        golden("auth_signing_input")
    );
    assert_eq!(
        auth_signing_input_v2(&challenge),
        [SIGN_AUTH_V2_DOMAIN, challenge.as_slice()].concat()
    );

    // 域常量：真实 NUL 结尾、互不复用、也不与任何 v1 域复用。
    let v2_domains = [
        SIGN_V2_CONTROL_DOMAIN,
        SIGN_V2_UPGRADE_DOMAIN,
        SIGN_V2_RECOVER_DOMAIN,
        SIGN_V2_POP_DOMAIN,
        SIGN_AUTH_V2_DOMAIN,
    ];
    let v1_domains = [
        SIGN_AUTH_DOMAIN,
        SIGN_MANAGE_DOMAIN,
        SIGN_OBJECT_COMMIT_DOMAIN,
        SIGN_DEVICE_CERT_DOMAIN,
        SIGN_CHECKPOINT_DOMAIN,
        SIGN_ACTIVATION_T_DOMAIN,
        CURSOR_HMAC_DOMAIN,
        AAD_DOMAIN,
    ];
    for (index, domain) in v2_domains.iter().enumerate() {
        assert!(domain_ends_with_nul(domain), "域必须真实 NUL 结尾");
        assert!(domain.starts_with(b"MN/v2/"));
        for (other_index, other) in v2_domains.iter().enumerate() {
            if index != other_index {
                assert_ne!(domain, other);
            }
        }
        for other in v1_domains {
            assert_ne!(*domain, other, "V2 域不得与 v1 域复用");
        }
    }
    assert_eq!(SIGN_V2_CONTROL_DOMAIN, b"MN/v2/control\0");
    assert_eq!(SIGN_V2_UPGRADE_DOMAIN, b"MN/v2/upgrade\0");
    assert_eq!(SIGN_V2_RECOVER_DOMAIN, b"MN/v2/recover\0");
    assert_eq!(SIGN_V2_POP_DOMAIN, b"MN/v2/pop\0");
    assert_eq!(SIGN_AUTH_V2_DOMAIN, b"MN/v2/auth\0");
    assert_eq!(HPKE_INFO_V2, b"MN/v2/epoch-grant\0");
    assert!(domain_ends_with_nul(HPKE_INFO_V2));

    // 不同域的签名输入互不相同（防域混淆）。
    assert_ne!(control, upgrade);
    assert_ne!(control, recover);
    assert_ne!(control, pop);
}

#[test]
fn chain_hash_golden_and_prev_binding() {
    let signed = sample_signed_control();
    let prev = hash(0x51);
    let chained = signed.chain_hash(&prev, hash_fn).expect("chain hash");
    assert_eq!(hex(&chained), golden("chain_hash"));
    assert_eq!(
        chained,
        sha256(&[prev.to_vec(), signed.encode().expect("encode")].concat())
    );

    let other_prev = hash(0x52);
    assert_ne!(
        signed.chain_hash(&other_prev, hash_fn).expect("chain hash"),
        chained
    );

    // 签名变化会改变链 hash（整条 SignedControlV2 都进摘要）。
    let mut tampered = signed;
    tampered.signatures[1] = signature(0xA3);
    assert_ne!(
        tampered.chain_hash(&prev, hash_fn).expect("chain hash"),
        chained
    );
}

#[test]
fn canonical_submit_path_is_unique_lowerhex() {
    let path = canonical_submit_path(&ids(0xAB));
    assert_eq!(
        path,
        "/v2/vaults/abababababababababababababababab/control/submit"
    );
    assert_eq!(path.len(), 58);
    assert_eq!(
        canonical_submit_path(&ids(0x11)),
        "/v2/vaults/11111111111111111111111111111111/control/submit"
    );

    let operation = sample_operation();
    assert_eq!(operation.canonical_path, canonical_submit_path(&ids(0x11)));
}

#[test]
fn cap_scope_bits_and_limits() {
    assert_eq!(CapScope::ALL, 63);
    for (bit, name) in [
        (CapScope::INVITE, "INVITE"),
        (CapScope::APPROVE, "APPROVE"),
        (CapScope::REVOKE, "REVOKE"),
        (CapScope::CAP_ADMIN, "CAP_ADMIN"),
        (CapScope::ROTATE, "ROTATE"),
        (CapScope::POLICY, "POLICY"),
    ] {
        let scope = CapScope::new(bit).expect(name);
        assert_eq!(scope.bits(), bit);
        assert!(CapScope::new(CapScope::ALL).expect("all").contains(scope));
    }
    assert!(CapScope::new(0).is_err(), "零 scope 必须拒绝");
    assert!(CapScope::new(64).is_err(), "未知位必须拒绝");
    assert!(CapScope::new(0xffff_ffff).is_err());

    let high = CapScope::new(CapScope::INVITE | CapScope::APPROVE).expect("high");
    let low = CapScope::new(CapScope::INVITE).expect("low");
    assert!(low.is_subset_of(high));
    assert!(!high.is_subset_of(low));
    assert!(high.contains(low));
    assert!(!high.contains(CapScope::new(CapScope::POLICY).expect("policy")));

    // cap 的 scope 字段只接受已知位；未知位/零位在解码时拒绝。
    let encoded = sample_cap().encode().expect("cap encode");
    for bits in [0_u64, 64, u64::from(u32::MAX)] {
        let mutated = mutate_fields(&encoded, |entries| entries[6].1 = CdeValue::UInt(bits));
        assert!(
            ManagementCapV2::decode(&mutated).is_err(),
            "scope 位 {bits} 必须拒绝"
        );
    }
    // 超过 u32 的 scope 值。
    let mutated = mutate_fields(&encoded, |entries| {
        entries[6].1 = CdeValue::UInt(u64::from(u32::MAX) + 1);
    });
    assert!(ManagementCapV2::decode(&mutated).is_err());
}

#[test]
fn cap_rejects_bad_ttl_delegable_versions_and_zero_ids() {
    let cap = sample_cap();
    for broken in [
        ManagementCapV2 {
            delegable: true,
            ..cap.clone()
        },
        ManagementCapV2 {
            expires_at: cap.not_before,
            ..cap.clone()
        },
        ManagementCapV2 {
            expires_at: cap.not_before - 1,
            ..cap.clone()
        },
        ManagementCapV2 {
            version: 1,
            ..cap.clone()
        },
        ManagementCapV2 {
            version: 3,
            ..cap.clone()
        },
        ManagementCapV2 {
            instance_id: [0; 16],
            ..cap.clone()
        },
        ManagementCapV2 {
            vault_id: [0; 16],
            ..cap.clone()
        },
        ManagementCapV2 {
            cap_id: [0; 16],
            ..cap.clone()
        },
        ManagementCapV2 {
            subject_device_id: [0; 16],
            ..cap.clone()
        },
        ManagementCapV2 {
            subject_ed_pk: [0; 32],
            ..cap.clone()
        },
        ManagementCapV2 {
            issued_control_rev: 0,
            ..cap.clone()
        },
        ManagementCapV2 {
            issued_authz_rev: 0,
            ..cap.clone()
        },
        ManagementCapV2 {
            issuer_authority_rev: 0,
            ..cap.clone()
        },
        ManagementCapV2 {
            not_before: i64::MAX as u64 + 1,
            ..cap.clone()
        },
        ManagementCapV2 {
            expires_at: i64::MAX as u64 + 1,
            ..cap.clone()
        },
        ManagementCapV2 {
            issued_control_rev: i64::MAX as u64 + 1,
            ..cap.clone()
        },
    ] {
        assert!(broken.validate().is_err(), "{broken:?}");
        assert!(broken.encode().is_err(), "{broken:?}");
    }

    // u 溢出必须在解码路径拒绝（不能只靠构造侧 validate）。
    let encoded = cap.encode().expect("encode");
    for key in [7_u64, 8, 9, 10, 11] {
        let mutated = mutate_fields(&encoded, |entries| {
            entries[key as usize].1 = CdeValue::UInt(i64::MAX as u64 + 1);
        });
        assert!(
            ManagementCapV2::decode(&mutated).is_err(),
            "key {key} 的 u 溢出必须拒绝"
        );
    }
}

#[test]
fn top_level_records_reject_noncanonical_cde() {
    let cap = sample_cap().encode().expect("cap");
    assert_strict_cde_rejections(&cap, "ManagementCapV2", |bytes| {
        ManagementCapV2::decode(bytes).is_ok()
    });

    let policy = sample_policy().encode().expect("policy");
    assert_strict_cde_rejections(&policy, "PolicyV2", |bytes| PolicyV2::decode(bytes).is_ok());

    let signer = sample_signer_device().encode().expect("signer");
    assert_strict_cde_rejections(&signer, "SignerV2", |bytes| SignerV2::decode(bytes).is_ok());

    let operation = sample_operation().encode().expect("operation");
    assert_strict_cde_rejections(&operation, "OperationV2", |bytes| {
        OperationV2::decode(bytes).is_ok()
    });

    let signed = sample_signed_control().encode().expect("signed");
    assert_strict_cde_rejections(&signed, "SignedControlV2", |bytes| {
        SignedControlV2::decode(bytes).is_ok()
    });

    let cert = sample_device_cert().encode().expect("cert");
    assert_strict_cde_rejections(&cert, "DeviceCertV2", |bytes| {
        DeviceCertV2::decode(bytes).is_ok()
    });

    let aad = sample_grant_aad().encode().expect("aad");
    assert_strict_cde_rejections(&aad, "GrantAadV2", |bytes| {
        GrantAadV2::decode(bytes).is_ok()
    });

    let package = sample_grant_package().encode().expect("package");
    assert_strict_cde_rejections(&package, "GrantPackageV2", |bytes| {
        GrantPackageV2::decode(bytes).is_ok()
    });

    let activation = sample_activation().encode().expect("activation");
    assert_strict_cde_rejections(&activation, "ActivationV2", |bytes| {
        ActivationV2::decode(bytes).is_ok()
    });
}

// —— ADR 0015 S4b1：kind payload / ChallengeV2 / SetDigestV2 ——

fn sample_cap_s4b1(cap_id: [u8; 16], subject_ed_pk: [u8; 32]) -> ManagementCapV2 {
    ManagementCapV2 {
        version: 2,
        instance_id: ids(0x12),
        vault_id: ids(0x11),
        cap_id,
        subject_device_id: ids(0x12),
        subject_ed_pk,
        scopes: CapScope::new(11).expect("scopes"),
        not_before: 1_700_000_000,
        expires_at: 1_700_003_600,
        issued_control_rev: 7,
        issued_authz_rev: 8,
        issuer_authority_rev: 1,
        delegable: false,
    }
}

fn sample_cert_s4b1(
    device_id: [u8; 16],
    sign_ed25519_pub: [u8; 32],
    hpke_x25519_pub: [u8; 32],
    cert_serial: u64,
) -> DeviceCertV2 {
    DeviceCertV2 {
        version: 2,
        instance_id: ids(0x12),
        vault_id: ids(0x11),
        device_id,
        sign_ed25519_pub,
        hpke_x25519_pub,
        cert_serial,
        data_rights: Rights::new(3).expect("rights"),
        issued_authz_rev: 8,
    }
}

fn sample_upgrade_payload() -> UpgradePolicyPayloadV2 {
    UpgradePolicyPayloadV2 {
        old_manager_rev: 3,
        policy: PolicyV2 {
            version: 2,
            highrisk_threshold: 2,
            min_quorum: 2,
            max_cap_ttl: 2_592_000,
            max_invite_ttl: 86_400,
        },
        fresh_recovery_pk: hash(0x33),
        bootstrap_caps: vec![
            sample_cap_s4b1(ids(0x13), hash(0x31)),
            sample_cap_s4b1(ids(0x14), hash(0x32)),
        ],
        active_manifest: vec![
            sample_cert_s4b1(ids(0x22), hash(0x44), hash(0x62), 3),
            sample_cert_s4b1(ids(0x23), hash(0x45), hash(0x63), 4),
        ],
    }
}

fn sample_challenge_v2() -> ChallengeV2 {
    ChallengeV2 {
        version: 2,
        instance_id: ids(0x12),
        origin_hash: hash(0x55),
        vault_id: ids(0x11),
        realm: CHALLENGE_V2_REALM_MANAGEMENT_DEVICE,
        subject: ids(0x22),
        challenge_id: ids(0x66),
        nonce: hash(0x77),
        expires_at: 1_800_000_060,
        auth_rev: 12,
        authz_rev: 4,
        cap_id: Some(ids(0x13)),
        authority_rev: 2,
    }
}

#[test]
fn s4b1_kind_payloads_golden_and_roundtrip() {
    let upgrade = sample_upgrade_payload();
    let encoded = upgrade.encode().expect("upgrade payload");
    assert_eq!(hex(&encoded), golden("upgrade_payload"));
    assert_eq!(
        UpgradePolicyPayloadV2::decode(&encoded),
        Ok(upgrade.clone())
    );

    let cap = sample_cap_s4b1(ids(0x13), hash(0x31));
    let grant = CapGrantPayloadV2 { cap: cap.clone() };
    let encoded = grant.encode().expect("cap grant payload");
    assert_eq!(hex(&encoded), golden("cap_grant_payload"));
    assert_eq!(CapGrantPayloadV2::decode(&encoded), Ok(grant.clone()));

    let revoke = CapRevokePayloadV2 { cap_id: ids(0x13) };
    let encoded = revoke.encode().expect("cap revoke payload");
    assert_eq!(hex(&encoded), golden("cap_revoke_payload"));
    assert_eq!(CapRevokePayloadV2::decode(&encoded), Ok(revoke));

    let invite = InviteCreatePayloadV2 {
        invite_id: ids(0x61),
        token_hash: hash(0x62),
        expires_at: 1_800_003_600,
    };
    let encoded = invite.encode().expect("invite payload");
    assert_eq!(hex(&encoded), golden("invite_create_payload"));
    assert_eq!(InviteCreatePayloadV2::decode(&encoded), Ok(invite));

    // 严格 CDE：非 canonical 字节必须拒绝。
    let cap_grant = grant.encode().expect("grant");
    assert_strict_cde_rejections(&cap_grant, "CapGrantPayloadV2", |bytes| {
        CapGrantPayloadV2::decode(bytes).is_ok()
    });
    let upgrade_bytes = upgrade.encode().expect("upgrade");
    assert_strict_cde_rejections(&upgrade_bytes, "UpgradePolicyPayloadV2", |bytes| {
        UpgradePolicyPayloadV2::decode(bytes).is_ok()
    });
    let challenge_bytes = sample_challenge_v2().encode().expect("challenge");
    assert_strict_cde_rejections(&challenge_bytes, "ChallengeV2", |bytes| {
        ChallengeV2::decode(bytes).is_ok()
    });
}

#[test]
fn s4b1_upgrade_payload_rejects_duplicate_caps_unsorted_manifest_and_zero_objects() {
    let payload = sample_upgrade_payload();

    let duplicate_cap = UpgradePolicyPayloadV2 {
        bootstrap_caps: vec![
            sample_cap_s4b1(ids(0x13), hash(0x31)),
            sample_cap_s4b1(ids(0x13), hash(0x32)),
        ],
        ..payload.clone()
    };
    assert!(duplicate_cap.validate().is_err());

    let unsorted = UpgradePolicyPayloadV2 {
        active_manifest: vec![
            sample_cert_s4b1(ids(0x23), hash(0x45), hash(0x63), 4),
            sample_cert_s4b1(ids(0x22), hash(0x44), hash(0x62), 3),
        ],
        ..payload.clone()
    };
    assert!(unsorted.validate().is_err());

    let empty_manifest = UpgradePolicyPayloadV2 {
        active_manifest: Vec::new(),
        ..payload.clone()
    };
    assert!(empty_manifest.validate().is_err());

    for broken in [
        UpgradePolicyPayloadV2 {
            old_manager_rev: 0,
            ..payload.clone()
        },
        UpgradePolicyPayloadV2 {
            fresh_recovery_pk: [0u8; 32],
            ..payload.clone()
        },
        UpgradePolicyPayloadV2 {
            policy: PolicyV2 {
                highrisk_threshold: 1,
                ..payload.policy.clone()
            },
            ..payload.clone()
        },
        UpgradePolicyPayloadV2 {
            policy: PolicyV2 {
                min_quorum: 1,
                ..payload.policy.clone()
            },
            ..payload.clone()
        },
        UpgradePolicyPayloadV2 {
            policy: PolicyV2 {
                max_invite_ttl: 86_401,
                ..payload.policy.clone()
            },
            ..payload.clone()
        },
    ] {
        assert!(broken.validate().is_err(), "{broken:?}");
        assert!(broken.encode().is_err(), "{broken:?}");
    }
}

#[test]
fn s4b1_challenge_v2_golden_realm_binding_and_strict_fields() {
    let challenge = sample_challenge_v2();
    let encoded = challenge.encode().expect("challenge");
    assert_eq!(hex(&encoded), golden("challenge_v2"));
    assert_eq!(ChallengeV2::decode(&encoded), Ok(challenge.clone()));

    // realm 3 必须带 cap；realm 1/4 必须不带 cap；realm 2 不是 V2 取值。
    assert!(ChallengeV2 {
        cap_id: None,
        ..challenge.clone()
    }
    .validate()
    .is_err());
    assert!(ChallengeV2 {
        realm: CHALLENGE_V2_REALM_DEVICE,
        ..challenge.clone()
    }
    .validate()
    .is_err());
    assert!(ChallengeV2 {
        realm: CHALLENGE_V2_REALM_DEVICE,
        cap_id: None,
        ..challenge.clone()
    }
    .validate()
    .is_ok());
    assert!(ChallengeV2 {
        realm: CHALLENGE_V2_REALM_RECOVERY,
        cap_id: None,
        ..challenge.clone()
    }
    .validate()
    .is_ok());
    assert!(ChallengeV2 {
        realm: 2,
        cap_id: None,
        ..challenge.clone()
    }
    .validate()
    .is_err());

    for broken in [
        ChallengeV2 {
            version: 1,
            ..challenge.clone()
        },
        ChallengeV2 {
            instance_id: [0; 16],
            ..challenge.clone()
        },
        ChallengeV2 {
            origin_hash: [0; 32],
            ..challenge.clone()
        },
        ChallengeV2 {
            challenge_id: [0; 16],
            ..challenge.clone()
        },
        ChallengeV2 {
            nonce: [0; 32],
            ..challenge.clone()
        },
        ChallengeV2 {
            auth_rev: 0,
            ..challenge.clone()
        },
        ChallengeV2 {
            expires_at: 0,
            ..challenge.clone()
        },
        ChallengeV2 {
            cap_id: Some([0; 16]),
            ..challenge.clone()
        },
    ] {
        assert!(broken.validate().is_err(), "{broken:?}");
        assert!(broken.encode().is_err(), "{broken:?}");
    }
}

#[test]
fn s4b1_set_digest_is_sorted_deduped_and_conflict_fails() {
    let entries = vec![
        (ids(0x22), proto_bytes(&[0xBB, 0xBB, 0xBB, 0xBB])),
        (ids(0x11), proto_uint(7)),
    ];
    let digest = set_digest_v2(&entries, hash_fn).expect("digest");
    assert_eq!(hex(&digest), golden("set_digest_v2"));
    // 预映像 = CDE(array(bytes))，按 id 升序；id 本身不进预映像。
    // 每个元素是 byte string，内容是该对象的完整 canonical CDE bytes
    // （不是把对象直接嵌进数组）。
    let mut ordered = entries.clone();
    ordered.sort_by_key(|left| left.0);
    let pre_image = CdeValue::Array(
        ordered
            .iter()
            .map(|entry| CdeValue::Bytes(entry.1.clone()))
            .collect(),
    );
    assert_eq!(
        hex(&encode_cde(&pre_image).expect("input")),
        golden("set_digest_v2_input")
    );
    // 乱序输入结果相同。
    let reordered = vec![entries[1].clone(), entries[0].clone()];
    assert_eq!(set_digest_v2(&reordered, hash_fn).expect("digest"), digest);
    // 完全相同的重复项去重。
    let duplicated = vec![entries[0].clone(), entries[1].clone(), entries[0].clone()];
    assert_eq!(set_digest_v2(&duplicated, hash_fn).expect("digest"), digest);
    // 同 id 不同字节 = 集合分叉，必须拒绝。
    let conflicting = vec![
        entries[0].clone(),
        entries[1].clone(),
        (ids(0x22), proto_uint(9)),
    ];
    assert!(set_digest_v2(&conflicting, hash_fn).is_err());
    // 元素必须自身是 canonical CDE。
    let noncanonical = vec![(ids(0x33), vec![0xa1, 0x01])];
    assert!(set_digest_v2(&noncanonical, hash_fn).is_err());
    // 空集合合法：CDE([]) = 0x80。
    assert_eq!(set_digest_v2(&[], hash_fn).expect("empty"), sha256(&[0x80]));
}

/// `CdeValue::UInt` 的 canonical 字节（测试内构造 SetDigest 输入）。
fn proto_uint(value: u64) -> Vec<u8> {
    encode_cde(&CdeValue::UInt(value)).expect("uint")
}

/// `CdeValue::Bytes` 的 canonical 字节。
fn proto_bytes(value: &[u8]) -> Vec<u8> {
    encode_cde(&CdeValue::Bytes(value.to_vec())).expect("bytes")
}

// ---------------------------------------------------------------- S4b2：kind 4/5/16 payload

/// `DeviceApprove`/`DeviceRightsChange` 共用的 payload 样例（serial=1、当前 epoch 一份 grant）。
fn sample_device_credential_payload() -> DeviceCredentialPayloadV2 {
    DeviceCredentialPayloadV2 {
        cert: DeviceCertV2 {
            version: V2_WIRE_VERSION,
            instance_id: ids(0x12),
            vault_id: ids(0x11),
            device_id: ids(0x24),
            sign_ed25519_pub: hash(0x46),
            hpke_x25519_pub: hash(0x64),
            cert_serial: 1,
            data_rights: Rights::new(Rights::ALL).expect("rights"),
            issued_authz_rev: 9,
        },
        grants: vec![GrantPackageV2 {
            aad: GrantAadV2 {
                version: V2_WIRE_VERSION,
                instance_id: ids(0x12),
                vault_id: ids(0x11),
                epoch: 2,
                recipient_device_id: ids(0x24),
                recipient_hpke_pub: hash(0x64),
                authority_rev: 1,
                grant_id: ids(0x72),
                issued_authz_rev: 9,
            },
            enc: hash(0x65),
            ciphertext: vec![0xC4; 40],
        }],
    }
}

#[test]
fn s4b2_device_payloads_golden_and_roundtrip() {
    // 硬性字段数：wire 形状一旦变化必须显式升版本。
    assert_eq!(DEVICE_CREDENTIAL_PAYLOAD_V2_FIELD_COUNT, 2);
    assert_eq!(DEVICE_REVOKE_PAYLOAD_V2_FIELD_COUNT, 1);

    let payload = sample_device_credential_payload();
    let encoded = payload.encode().expect("device credential payload");
    assert_eq!(hex(&encoded), golden("device_approve_payload"));
    assert_eq!(
        DeviceCredentialPayloadV2::decode(&encoded),
        Ok(payload.clone())
    );
    // kind 4/16 是同一 wire 形状（别名解码到同一类型）。
    assert_eq!(
        DeviceApprovePayloadV2::decode(&encoded),
        Ok(payload.clone())
    );
    assert_eq!(
        DeviceRightsChangePayloadV2::decode(&encoded),
        Ok(payload.clone())
    );
    // 证书是 bytes(CDE(DeviceCertV2))，不是嵌套 map。
    let entries = entries_of(&encoded);
    assert!(
        matches!(entries[0].1, CdeValue::Bytes(_)),
        "key 0 必须是 bytes"
    );
    assert!(
        matches!(entries[1].1, CdeValue::Array(_)),
        "key 1 必须是数组"
    );

    let revoke = DeviceRevokePayloadV2 {
        device_ids: vec![ids(0x24), ids(0x25)],
    };
    let encoded = revoke.encode().expect("device revoke payload");
    assert_eq!(hex(&encoded), golden("device_revoke_payload"));
    assert_eq!(DeviceRevokePayloadV2::decode(&encoded), Ok(revoke.clone()));

    // 严格 CDE：乱序/重复/未知键/非最短/尾随字节一律拒绝。
    let payload_bytes = payload.encode().expect("payload");
    assert_strict_cde_rejections(&payload_bytes, "DeviceCredentialPayloadV2", |bytes| {
        DeviceCredentialPayloadV2::decode(bytes).is_ok()
    });
    let revoke_bytes = revoke.encode().expect("revoke");
    assert_strict_cde_rejections(&revoke_bytes, "DeviceRevokePayloadV2", |bytes| {
        DeviceRevokePayloadV2::decode(bytes).is_ok()
    });
}

#[test]
fn s4b2_device_payloads_reject_empty_duplicates_and_unsorted_sets() {
    let payload = sample_device_credential_payload();

    // grants 不能为空（首次准入/改权都必须承诺当轮 grant）。
    let empty_grants = DeviceCredentialPayloadV2 {
        grants: Vec::new(),
        ..payload.clone()
    };
    assert!(empty_grants.validate().is_err());
    assert!(empty_grants.encode().is_err());

    // grant_id / epoch 不能重复（对象身份与投影 PK 不能有二义）。
    let mut duplicate_grant = payload.clone();
    duplicate_grant.grants.push(payload.grants[0].clone());
    assert!(duplicate_grant.validate().is_err());

    let mut duplicate_epoch = payload.clone();
    let mut second = payload.grants[0].clone();
    second.aad.grant_id = ids(0x73);
    duplicate_epoch.grants.push(second);
    assert!(duplicate_epoch.validate().is_err());

    // 证书本身非法（serial=0）时整份 payload 也拒绝。
    let mut bad_cert = payload.clone();
    bad_cert.cert.cert_serial = 0;
    assert!(bad_cert.validate().is_err());

    // DeviceRevoke：空集合 / 未升序 / 重复 / 全零 ID 全部拒绝。
    for broken in [
        DeviceRevokePayloadV2 {
            device_ids: Vec::new(),
        },
        DeviceRevokePayloadV2 {
            device_ids: vec![ids(0x25), ids(0x24)],
        },
        DeviceRevokePayloadV2 {
            device_ids: vec![ids(0x24), ids(0x24)],
        },
        DeviceRevokePayloadV2 {
            device_ids: vec![[0u8; 16]],
        },
    ] {
        assert!(broken.validate().is_err(), "{broken:?}");
        assert!(broken.encode().is_err(), "{broken:?}");
    }
}

// ---------------------------------------------------------------- S4c：kind 6–10/12/13/14 payload

#[test]
fn s4c_rotation_and_activation_payloads_golden_and_roundtrip() {
    assert_eq!(ROTATION_BEGIN_PAYLOAD_V2_FIELD_COUNT, 4);
    assert_eq!(ROTATION_ABORT_PAYLOAD_V2_FIELD_COUNT, 1);
    assert_eq!(ROTATION_RESTART_PAYLOAD_V2_FIELD_COUNT, 3);
    assert_eq!(ROTATION_ADD_REVOKES_PAYLOAD_V2_FIELD_COUNT, 2);
    assert_eq!(EPOCH_ACTIVATE_PAYLOAD_V2_FIELD_COUNT, 1);
    assert_eq!(INVITE_CANCEL_PAYLOAD_V2_FIELD_COUNT, 1);

    let begin = RotationBeginPayloadV2 {
        rotation_id: ids(0x81),
        from_epoch: 2,
        to_epoch: 3,
        revoke_ids: vec![ids(0x24), ids(0x25)],
    };
    let encoded = begin.encode().expect("rotation begin");
    assert_eq!(hex(&encoded), golden("rotation_begin_payload"));
    assert_eq!(RotationBeginPayloadV2::decode(&encoded), Ok(begin.clone()));

    let abort = RotationAbortPayloadV2 {
        rotation_id: ids(0x81),
    };
    let encoded = abort.encode().expect("rotation abort");
    assert_eq!(hex(&encoded), golden("rotation_abort_payload"));
    assert_eq!(RotationAbortPayloadV2::decode(&encoded), Ok(abort.clone()));

    let restart = RotationRestartPayloadV2 {
        rotation_id: ids(0x81),
        new_to_epoch: 4,
        extra_ids: vec![ids(0x26)],
    };
    let encoded = restart.encode().expect("rotation restart");
    assert_eq!(hex(&encoded), golden("rotation_restart_payload"));
    assert_eq!(
        RotationRestartPayloadV2::decode(&encoded),
        Ok(restart.clone())
    );

    let add = RotationAddRevokesPayloadV2 {
        rotation_id: ids(0x81),
        device_ids: vec![ids(0x24), ids(0x25)],
    };
    let encoded = add.encode().expect("add revokes");
    assert_eq!(hex(&encoded), golden("rotation_add_revokes_payload"));
    assert_eq!(
        RotationAddRevokesPayloadV2::decode(&encoded),
        Ok(add.clone())
    );

    // EpochActivate 的键 0 是**整份** ActivationV2 的 bytes（不是嵌套 map）。
    let activate = EpochActivatePayloadV2 {
        activation: sample_activation(),
    };
    let encoded = activate.encode().expect("epoch activate");
    assert_eq!(hex(&encoded), golden("epoch_activate_payload"));
    let entries = entries_of(&encoded);
    assert!(
        matches!(entries[0].1, CdeValue::Bytes(_)),
        "key 0 必须是 bytes"
    );
    assert_eq!(
        EpochActivatePayloadV2::decode(&encoded),
        Ok(activate.clone())
    );

    // 严格 CDE 负例（kind 专属 payload 也走同一套 canonical 规则）。
    for (bytes, label, decode) in [
        (
            begin.encode().expect("begin"),
            "RotationBeginPayloadV2",
            (|b: &[u8]| RotationBeginPayloadV2::decode(b).is_ok()) as fn(&[u8]) -> bool,
        ),
        (
            abort.encode().expect("abort"),
            "RotationAbortPayloadV2",
            (|b: &[u8]| RotationAbortPayloadV2::decode(b).is_ok()) as fn(&[u8]) -> bool,
        ),
        (
            restart.encode().expect("restart"),
            "RotationRestartPayloadV2",
            (|b: &[u8]| RotationRestartPayloadV2::decode(b).is_ok()) as fn(&[u8]) -> bool,
        ),
        (
            add.encode().expect("add"),
            "RotationAddRevokesPayloadV2",
            (|b: &[u8]| RotationAddRevokesPayloadV2::decode(b).is_ok()) as fn(&[u8]) -> bool,
        ),
        (
            activate.encode().expect("activate"),
            "EpochActivatePayloadV2",
            (|b: &[u8]| EpochActivatePayloadV2::decode(b).is_ok()) as fn(&[u8]) -> bool,
        ),
    ] {
        assert_strict_cde_rejections(&bytes, label, decode);
    }
}

#[test]
fn s4c_rotation_payloads_reject_epoch_reuse_and_unsorted_ids() {
    // from_epoch 必须 >0；to_epoch 必须严格大于 from（候选 epoch 单调）。
    for broken in [
        RotationBeginPayloadV2 {
            rotation_id: ids(0x81),
            from_epoch: 0,
            to_epoch: 3,
            revoke_ids: Vec::new(),
        },
        RotationBeginPayloadV2 {
            rotation_id: ids(0x81),
            from_epoch: 3,
            to_epoch: 3,
            revoke_ids: Vec::new(),
        },
        RotationBeginPayloadV2 {
            rotation_id: [0u8; 16],
            from_epoch: 2,
            to_epoch: 3,
            revoke_ids: Vec::new(),
        },
        RotationBeginPayloadV2 {
            rotation_id: ids(0x81),
            from_epoch: 2,
            to_epoch: 3,
            revoke_ids: vec![ids(0x25), ids(0x24)],
        },
        RotationBeginPayloadV2 {
            rotation_id: ids(0x81),
            from_epoch: 2,
            to_epoch: 3,
            revoke_ids: vec![ids(0x24), ids(0x24)],
        },
    ] {
        assert!(broken.validate().is_err(), "{broken:?}");
        assert!(broken.encode().is_err(), "{broken:?}");
    }
    // restart 的新候选 epoch 必须 >0；extra_ids 同样严格升序。
    for broken in [
        RotationRestartPayloadV2 {
            rotation_id: ids(0x81),
            new_to_epoch: 0,
            extra_ids: Vec::new(),
        },
        RotationRestartPayloadV2 {
            rotation_id: ids(0x81),
            new_to_epoch: 4,
            extra_ids: vec![ids(0x26), ids(0x25)],
        },
    ] {
        assert!(broken.validate().is_err(), "{broken:?}");
    }
    // add-revokes 不能是空集合。
    assert!(RotationAddRevokesPayloadV2 {
        rotation_id: ids(0x81),
        device_ids: Vec::new(),
    }
    .validate()
    .is_err());
    // 空撤销列表在本类型层面合法（Begin/Restart 可以不带撤销）。
    assert!(RotationBeginPayloadV2 {
        rotation_id: ids(0x81),
        from_epoch: 2,
        to_epoch: 3,
        revoke_ids: Vec::new(),
    }
    .validate()
    .is_ok());
}

#[test]
fn s4c_invite_cancel_and_recovery_payloads_golden_and_roundtrip() {
    assert_eq!(RECOVERY_RESET_PAYLOAD_V2_FIELD_COUNT, 5);
    assert_eq!(RECOVERY_KEY_CHANGE_PAYLOAD_V2_FIELD_COUNT, 2);

    let cancel = InviteCancelPayloadV2 {
        invite_id: ids(0x61),
    };
    let encoded = cancel.encode().expect("invite cancel");
    assert_eq!(hex(&encoded), golden("invite_cancel_payload"));
    assert_eq!(InviteCancelPayloadV2::decode(&encoded), Ok(cancel.clone()));
    assert_strict_cde_rejections(&encoded, "InviteCancelPayloadV2", |bytes| {
        InviteCancelPayloadV2::decode(bytes).is_ok()
    });

    let reset = RecoveryResetPayloadV2 {
        policy: sample_policy(),
        fresh_recovery_pk: hash(0x33),
        bootstrap_caps: vec![
            sample_cap_s4b1(ids(0x13), hash(0x31)),
            sample_cap_s4b1(ids(0x14), hash(0x32)),
        ],
        device_adds: vec![sample_device_credential_payload()],
        revoke_ids: vec![ids(0x25), ids(0x26)],
    };
    let encoded = reset.encode().expect("recovery reset");
    assert_eq!(hex(&encoded), golden("recovery_reset_payload"));
    assert_eq!(RecoveryResetPayloadV2::decode(&encoded), Ok(reset.clone()));
    assert_strict_cde_rejections(&encoded, "RecoveryResetPayloadV2", |bytes| {
        RecoveryResetPayloadV2::decode(bytes).is_ok()
    });

    let change = RecoveryKeyChangePayloadV2 {
        fresh_recovery_pk: hash(0x33),
        bootstrap_caps: vec![
            sample_cap_s4b1(ids(0x13), hash(0x31)),
            sample_cap_s4b1(ids(0x14), hash(0x32)),
        ],
    };
    let encoded = change.encode().expect("recovery key change");
    assert_eq!(hex(&encoded), golden("recovery_key_change_payload"));
    assert_eq!(
        RecoveryKeyChangePayloadV2::decode(&encoded),
        Ok(change.clone())
    );
    assert_strict_cde_rejections(&encoded, "RecoveryKeyChangePayloadV2", |bytes| {
        RecoveryKeyChangePayloadV2::decode(bytes).is_ok()
    });
}

#[test]
fn s4c_recovery_payloads_reject_duplicate_ids_and_conflicting_add_revoke() {
    let base = RecoveryResetPayloadV2 {
        policy: sample_policy(),
        fresh_recovery_pk: hash(0x33),
        bootstrap_caps: vec![sample_cap_s4b1(ids(0x13), hash(0x31))],
        device_adds: vec![sample_device_credential_payload()],
        revoke_ids: vec![ids(0x25)],
    };
    assert!(base.validate().is_ok());

    // 新恢复公钥不能是全零。
    let mut zero_pk = base.clone();
    zero_pk.fresh_recovery_pk = [0u8; 32];
    assert!(zero_pk.validate().is_err());

    // bootstrap caps 不能为空（否则 authority_rev+1 后无任何有效 cap）。
    let mut no_caps = base.clone();
    no_caps.bootstrap_caps.clear();
    assert!(no_caps.validate().is_err());

    // 重复 cap_id / 重复 add device_id 都拒绝。
    let mut dup_caps = base.clone();
    dup_caps.bootstrap_caps = vec![
        sample_cap_s4b1(ids(0x13), hash(0x31)),
        sample_cap_s4b1(ids(0x13), hash(0x32)),
    ];
    assert!(dup_caps.validate().is_err());
    let mut dup_adds = base.clone();
    dup_adds.device_adds = vec![
        sample_device_credential_payload(),
        sample_device_credential_payload(),
    ];
    assert!(dup_adds.validate().is_err());

    // 同一台设备不能既 add 又 revoke。
    let mut conflict = base.clone();
    conflict.revoke_ids = vec![ids(0x24)];
    assert!(conflict.validate().is_err());

    // revoke_ids 必须严格升序去重。
    let mut unsorted = base.clone();
    unsorted.revoke_ids = vec![ids(0x26), ids(0x25)];
    assert!(unsorted.validate().is_err());

    // KeyChange：空 caps/零公钥同样拒绝。
    assert!(RecoveryKeyChangePayloadV2 {
        fresh_recovery_pk: [0u8; 32],
        bootstrap_caps: vec![sample_cap_s4b1(ids(0x13), hash(0x31))],
    }
    .validate()
    .is_err());
    assert!(RecoveryKeyChangePayloadV2 {
        fresh_recovery_pk: hash(0x33),
        bootstrap_caps: Vec::new(),
    }
    .validate()
    .is_err());
}
