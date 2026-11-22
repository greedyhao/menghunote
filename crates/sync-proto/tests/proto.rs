//! sync-proto 的 golden vectors / canonical / §16 规范化测试。
//!
//! 这些向量提交后即冻结；wire 变更必须升 protocol_version。

use menghunote_sync_proto::*;

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

fn header_update() -> EnvelopeHeader {
    let cap = MAX_NON_ASSET_PART_PLAINTEXT as u64;
    EnvelopeHeader {
        protocol_version: 1,
        header_len: 196,
        object_type: ObjectType::Update,
        cipher_suite: 1,
        flags: 0,
        vault_id: std::array::from_fn(|index| index as u8),
        object_id: std::array::from_fn(|index| index as u8),
        message_id: std::array::from_fn(|index| index as u8),
        producer_device_id: std::array::from_fn(|index| index as u8),
        generation: 0x0102_0304_0506_0708,
        key_epoch: 0x1112_1314_1516_1718,
        sender_seq: 0x2122_2324_2526_2728,
        part_index: 2,
        part_count: 3,
        total_plaintext_len: cap * 2 + 1,
        this_plaintext_len: 1,
        ciphertext_len: 17,
        context_hash: [0; 32],
        nonce: std::array::from_fn(|index| index as u8 + 0x60),
    }
}

#[test]
fn header_golden_vector_196_bytes_and_offsets() {
    let header = header_update();
    let bytes = header.encode().expect("header encode");
    assert_eq!(bytes.len(), 196);

    let expected = concat!(
        "4d4e4531",
        "0001",
        "00c4",
        "01",
        "01",
        "0000",
        "000102030405060708090a0b0c0d0e0f",
        "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f",
        "000102030405060708090a0b0c0d0e0f",
        "000102030405060708090a0b0c0d0e0f",
        "0102030405060708",
        "1112131415161718",
        "2122232425262728",
        "00000002",
        "00000003",
        "00000000001ffe59",
        "00000001",
        "00000011",
        "0000000000000000000000000000000000000000000000000000000000000000",
        "606162636465666768696a6b6c6d6e6f7071727374757677",
    );
    assert_eq!(hex(&bytes), expected);
    assert_eq!(&bytes[0..4], b"MNE1");
    assert_eq!(u16::from_be_bytes([bytes[4], bytes[5]]), 1);
    assert_eq!(u16::from_be_bytes([bytes[6], bytes[7]]), 196);
    assert_eq!(bytes[8], 1);
    assert_eq!(
        u64::from_be_bytes(bytes[92..100].try_into().unwrap()),
        header.generation
    );
    assert_eq!(
        u64::from_be_bytes(bytes[124..132].try_into().unwrap()),
        header.total_plaintext_len
    );
    assert_eq!(u32::from_be_bytes(bytes[136..140].try_into().unwrap()), 17);
    assert_eq!(&bytes[172..196], &header.nonce);

    assert_eq!(EnvelopeHeader::decode(&bytes).expect("decode"), header);
}

#[test]
fn header_rejects_bad_version_len_magic_and_flags() {
    let header = header_update();
    let mut bytes = header.encode().unwrap();
    bytes[0] = b'X';
    assert!(EnvelopeHeader::decode(&bytes).is_err());

    let mut bytes = header.encode().unwrap();
    bytes[4..6].copy_from_slice(&2u16.to_be_bytes());
    assert!(matches!(
        EnvelopeHeader::decode(&bytes),
        Err(ProtoError::UnsupportedVersion(2))
    ));

    let mut bytes = header.encode().unwrap();
    bytes[6..8].copy_from_slice(&195u16.to_be_bytes());
    assert!(EnvelopeHeader::decode(&bytes).is_err());

    let mut bytes = header.encode().unwrap();
    bytes[10] = 1;
    assert!(EnvelopeHeader::decode(&bytes).is_err());

    assert!(EnvelopeHeader::decode(&header.encode().unwrap()[..195]).is_err());
}

#[test]
fn aad_golden_vector_has_true_nul_domain() {
    let header = header_update();
    let bytes = header.encode().unwrap();
    let aad_bytes = aad(&header, &[]).expect("aad");
    assert_eq!(&aad_bytes[..10], b"MN/e2e/v1\0");
    assert_eq!(aad_bytes[9], 0x00);
    assert_eq!(
        &aad_bytes[..10],
        &[0x4d, 0x4e, 0x2f, 0x65, 0x32, 0x65, 0x2f, 0x76, 0x31, 0x00]
    );
    assert!(
        !AAD_DOMAIN.ends_with(b"\\0"),
        "必须是 0x00，不是两个 ASCII 字符"
    );
    assert_eq!(&aad_bytes[10..206], &bytes);
    assert_eq!(&aad_bytes[206..210], &[0, 0, 0, 0]);
    assert_eq!(aad_bytes.len(), 10 + 196 + 4);

    let mut with_descriptor = header.clone();
    with_descriptor.object_type = ObjectType::KeySlot;
    with_descriptor.generation = 0;
    with_descriptor.sender_seq = 0;
    with_descriptor.total_plaintext_len = 1;
    with_descriptor.part_count = 1;
    with_descriptor.part_index = 0;
    with_descriptor.this_plaintext_len = 1;
    with_descriptor.ciphertext_len = 17;
    let descriptor = SlotDescriptorV1 {
        protocol_version: 1,
        slot_id: [7; 32],
        vault_id: header.vault_id,
        key_epoch: 1,
        kind: SlotKind::Password,
        wrap_suite: 1,
        kdf: Kdf::RecoveryHkdf,
        created_at: 1,
        manager_rev: 1,
    }
    .encode()
    .unwrap();
    let slot_aad = aad(&with_descriptor, &descriptor).expect("key slot aad");
    assert_eq!(slot_aad.len(), 10 + 196 + 4 + descriptor.len());
    assert_eq!(&slot_aad[210..], &descriptor);
}

#[test]
fn cde_primitives_are_canonical_and_shortest() {
    let value = CdeValue::Map(vec![(1, CdeValue::UInt(1)), (0, CdeValue::UInt(0))]);
    let encoded = encode_cde(&value).expect("encode");
    assert_eq!(hex(&encoded), "a200000101");
    assert_eq!(
        decode_cde(&encoded).expect("decode"),
        CdeValue::Map(vec![(0, CdeValue::UInt(0)), (1, CdeValue::UInt(1))])
    );

    assert_eq!(hex(&encode_cde(&CdeValue::UInt(23)).unwrap()), "17");
    assert_eq!(hex(&encode_cde(&CdeValue::UInt(24)).unwrap()), "1818");
    assert_eq!(hex(&encode_cde(&CdeValue::UInt(255)).unwrap()), "18ff");
    assert_eq!(hex(&encode_cde(&CdeValue::UInt(256)).unwrap()), "190100");
    assert_eq!(
        hex(&encode_cde(&CdeValue::UInt(65536)).unwrap()),
        "1a00010000"
    );
    assert_eq!(
        hex(&encode_cde(&CdeValue::UInt(u64::MAX)).unwrap()),
        "1bffffffffffffffff"
    );
    assert_eq!(hex(&encode_cde(&CdeValue::Bool(true)).unwrap()), "f5");
}

#[test]
fn cde_rejects_noncanonical_and_forbidden_values() {
    // 非最短整数 0。
    assert!(decode_cde(&[0x18, 0x00]).is_err());
    // map 键乱序。
    assert!(decode_cde(&[0xa2, 0x01, 0x01, 0x00, 0x00]).is_err());
    // null。
    assert!(decode_cde(&[0xf6]).is_err());
    // 不定长数组。
    assert!(decode_cde(&[0x9f, 0x01, 0xff]).is_err());
    // 截断。
    assert!(decode_cde(&[0x18]).is_err());
    // 尾部多余字节。
    assert!(decode_cde(&[0x01, 0x00]).is_err());
}

fn sample_update() -> UpdateV1 {
    UpdateV1 {
        doc_id: "doc-1".to_owned(),
        generation: 3,
        codec_version: "loro-1".to_owned(),
        app_doc_schema: 2,
        min_reader: 1,
        update_id: "u-1".to_owned(),
        batch_id: "b-1".to_owned(),
        origin_device_id: [1; 16],
        origin_seq: 9,
        base_vv: vec![1, 2],
        end_vv: vec![3, 4],
        end_frontier: vec![5, 6],
        loro_update_bytes: vec![7, 8, 9],
    }
}

fn sample_snapshot() -> SnapshotV1 {
    SnapshotV1 {
        doc_id: "doc-1".to_owned(),
        kind: 1,
        generation: 3,
        codec_version: "loro-1".to_owned(),
        app_doc_schema: 2,
        min_reader: 1,
        full_history: true,
        vv: vec![1, 2],
        frontier: vec![3, 4],
        loro_snapshot_bytes: vec![5, 6, 7],
    }
}

fn sample_manifest() -> ManifestV1 {
    ManifestV1 {
        vault_id: [1; 16],
        key_epoch: 2,
        checkpoint_id: [2; 16],
        covered_relay_seq: 12,
        min_reader: 1,
        structure_doc_id: "structure".to_owned(),
        docs: vec![
            ManifestDoc {
                doc_id: "a".to_owned(),
                kind: 1,
                generation: 1,
                key_epoch: 1,
                vv: vec![1],
                frontier: vec![2],
                snapshot_object_id: [3; 32],
                part_count: 1,
                total_plain_len: 10,
                object_digest: [4; 32],
            },
            ManifestDoc {
                doc_id: "b".to_owned(),
                kind: 1,
                generation: 1,
                key_epoch: 1,
                vv: vec![5],
                frontier: vec![6],
                snapshot_object_id: [7; 32],
                part_count: 1,
                total_plain_len: 11,
                object_digest: [8; 32],
            },
        ],
        assets: vec![
            ManifestAsset {
                real_sha256: [1; 32],
                byte_len: 3,
                media_type: "text/plain".to_owned(),
                object_id: [9; 32],
                key_epoch: 1,
                part_count: 1,
                total_plain_len: 50,
                object_digest: [10; 32],
            },
            ManifestAsset {
                real_sha256: [2; 32],
                byte_len: 4,
                media_type: "image/png".to_owned(),
                object_id: [11; 32],
                key_epoch: 1,
                part_count: 1,
                total_plain_len: 51,
                object_digest: [12; 32],
            },
        ],
    }
}

fn sample_slot() -> SlotDescriptorV1 {
    SlotDescriptorV1 {
        protocol_version: 1,
        slot_id: [1; 32],
        vault_id: [2; 16],
        key_epoch: 3,
        kind: SlotKind::Password,
        wrap_suite: 1,
        kdf: Kdf::Argon2id(Argon2idParams {
            version: 0x13,
            m_kib: 65536,
            t: 3,
            p: 1,
            salt: [4; 16],
            output_len: 32,
        }),
        created_at: 10,
        manager_rev: 2,
    }
}

fn sample_cert() -> DeviceCertV1 {
    DeviceCertV1 {
        vault_id: [1; 16],
        device_id: [2; 16],
        sign_ed25519_pub: [3; 32],
        hpke_x25519_pub: [4; 32],
        cert_serial: 5,
        issued_manager_rev: 2,
        rights: Rights::new(Rights::ALL).unwrap(),
    }
}

fn sample_challenge() -> Challenge {
    Challenge {
        pv: 1,
        relay_instance_id: [1; 16],
        origin_sha256: [2; 32],
        vault_id: [3; 16],
        realm: AuthRealm::Device,
        subject: [4; 16],
        challenge_id: [5; 16],
        nonce: [6; 32],
        expires_at: 100,
    }
}

fn sample_commit() -> ObjectCommit {
    ObjectCommit {
        vault_id: [1; 16],
        object_id: [2; 32],
        message_id: [3; 16],
        object_type: ObjectType::Update,
        generation: 1,
        key_epoch: 2,
        sender_seq: 3,
        part_count: 1,
        total_len: 1,
        object_digest: [4; 32],
    }
}

fn sample_checkpoint() -> CheckpointCommit {
    CheckpointCommit {
        vault_id: [1; 16],
        checkpoint_id: [2; 16],
        epoch: 2,
        covered_relay_seq: 9,
        manifest_object_id: [3; 32],
        manifest_digest: [4; 32],
        refs_digest: [5; 32],
        producer_device_id: [6; 16],
        validation_version: 1,
    }
}

fn roundtrip<T: PartialEq + std::fmt::Debug>(
    value: &T,
    encode: impl Fn(&T) -> Result<Vec<u8>, ProtoError>,
    decode: impl Fn(&[u8]) -> Result<T, ProtoError>,
) {
    let bytes = encode(value).expect("encode");
    let decoded = decode(&bytes).expect("decode");
    assert_eq!(&decoded, value);
    assert_eq!(
        encode(&decoded).expect("re-encode"),
        bytes,
        "canonical 唯一"
    );
}

#[test]
fn all_records_roundtrip_and_are_canonical() {
    roundtrip(&sample_update(), UpdateV1::encode, UpdateV1::decode);
    roundtrip(&sample_snapshot(), SnapshotV1::encode, SnapshotV1::decode);
    roundtrip(&sample_manifest(), ManifestV1::encode, ManifestV1::decode);
    roundtrip(
        &sample_slot(),
        SlotDescriptorV1::encode,
        SlotDescriptorV1::decode,
    );
    roundtrip(
        &RootSecretV1 {
            vault_id: [1; 16],
            key_epoch: 1,
            root: [2; 32],
        },
        RootSecretV1::encode,
        RootSecretV1::decode,
    );
    roundtrip(&sample_cert(), DeviceCertV1::encode, DeviceCertV1::decode);
    roundtrip(&sample_challenge(), Challenge::encode, Challenge::decode);
    roundtrip(&sample_commit(), ObjectCommit::encode, ObjectCommit::decode);
    roundtrip(
        &sample_checkpoint(),
        CheckpointCommit::encode,
        CheckpointCommit::decode,
    );
    roundtrip(
        &ObjectRefV1 {
            object_id: [1; 32],
            object_type: ObjectType::Update,
            generation: 1,
            key_epoch: 1,
            part_count: 1,
            total_plain_len: 1,
            object_digest: [2; 32],
        },
        ObjectRefV1::encode,
        ObjectRefV1::decode,
    );
}

#[test]
fn records_reject_unknown_missing_and_truncated_fields() {
    let bytes = sample_update().encode().unwrap();
    assert!(UpdateV1::decode(&bytes[..bytes.len() - 1]).is_err());

    // 追加未知键 13 → 字段数不对/未知键都必须拒绝。
    let CdeValue::Map(mut entries) = decode_cde(&bytes).unwrap() else {
        panic!("record 必须是 map");
    };
    entries.push((13, CdeValue::UInt(0)));
    let extra = encode_cde(&CdeValue::Map(entries)).unwrap();
    assert!(matches!(
        UpdateV1::decode(&extra),
        Err(ProtoError::Invalid(_)) | Err(ProtoError::UnknownField(13))
    ));

    // 删除最后一个字段。
    let CdeValue::Map(mut entries) = decode_cde(&bytes).unwrap() else {
        panic!("record 必须是 map");
    };
    entries.pop();
    let missing = encode_cde(&CdeValue::Map(entries)).unwrap();
    assert!(UpdateV1::decode(&missing).is_err());
}

#[test]
fn slot_rejects_unknown_kdf_tag() {
    let mut slot = sample_slot();
    let CdeValue::Map(mut entries) = slot.to_cde().unwrap() else {
        panic!("slot map");
    };
    // 键 6 是 Kdf union；把 tag 改成未知 3。
    if let CdeValue::Array(items) = &mut entries[6].1 {
        items[0] = CdeValue::UInt(3);
    }
    let bytes = encode_cde(&CdeValue::Map(entries)).unwrap();
    assert!(SlotDescriptorV1::decode(&bytes).is_err());
    slot.kdf = Kdf::RecoveryHkdf;
    assert!(slot.encode().is_ok());
}

#[test]
fn manifest_requires_sorted_docs_and_assets() {
    let mut manifest = sample_manifest();
    manifest.docs.reverse();
    assert!(manifest.encode().is_err());
    let mut manifest = sample_manifest();
    manifest.assets.reverse();
    assert!(manifest.encode().is_err());
}

#[test]
fn asset_prefix_layout_and_mime_limit() {
    let prefix = AssetPrefix::new([7; 32], 3, "text/plain").unwrap();
    let bytes = prefix.encode().unwrap();
    assert_eq!(bytes.len(), 42 + "text/plain".len());
    assert_eq!(&bytes[0..32], &[7; 32]);
    assert_eq!(&bytes[32..40], &3u64.to_be_bytes());
    assert_eq!(&bytes[40..42], &(10u16).to_be_bytes());

    let (decoded, offset) = AssetPrefix::parse(&bytes).unwrap();
    assert_eq!(decoded, prefix);
    assert_eq!(offset, bytes.len());

    let mut payload = bytes.clone();
    payload.extend_from_slice(b"abc");
    let (decoded, data) = AssetPrefix::split_payload(&payload).unwrap();
    assert_eq!(decoded, prefix);
    assert_eq!(data, b"abc");

    assert!(AssetPrefix::new([0; 32], 0, "x".repeat(256)).is_err());
}

#[test]
fn fixed_slicing_boundaries_and_key_slot() {
    let cap = MAX_NON_ASSET_PART_PLAINTEXT as u64;
    assert_eq!(part_count_for(0, ObjectType::Update), 0);
    assert_eq!(part_count_for(1, ObjectType::Update), 1);
    assert_eq!(part_count_for(cap, ObjectType::Update), 1);
    assert_eq!(part_count_for(cap + 1, ObjectType::Update), 2);
    assert_eq!(
        expected_part_plain_len(ObjectType::Update, 1, 0).unwrap(),
        1
    );
    assert_eq!(
        expected_part_plain_len(ObjectType::Update, cap + 1, 0).unwrap(),
        cap as usize
    );
    assert_eq!(
        expected_part_plain_len(ObjectType::Update, cap + 1, 1).unwrap(),
        1
    );
    assert!(expected_part_plain_len(ObjectType::Update, cap + 1, 2).is_err());

    // 非最后一片长度必须恰好为 cap。
    assert!(validate_slicing(ObjectType::Update, cap + 1, 0, 2, 1).is_err());
    assert!(validate_slicing(ObjectType::Update, cap + 1, 0, 2, cap as u32).is_ok());
    // 最后一片不能为 0。
    assert!(validate_slicing(ObjectType::Update, cap, 0, 2, cap as u32).is_err());

    // KEY_SLOT 必然单片，明文 ≤4KiB。
    assert!(validate_slicing(ObjectType::KeySlot, 4096, 0, 1, 4096).is_ok());
    assert!(validate_slicing(ObjectType::KeySlot, 4097, 0, 1, 4097).is_err());
    assert!(validate_slicing(ObjectType::KeySlot, 4096, 0, 2, 4096).is_err());
}

#[test]
fn envelope_normalization_generation_and_sender_rules() {
    let mut update = header_update();
    update.generation = 0;
    assert!(update.validate().is_err());
    let mut update = header_update();
    update.object_type = ObjectType::Snapshot;
    update.sender_seq = 0;
    assert!(update.validate().is_ok());
    update.generation = 0;
    assert!(update.validate().is_err());

    for object_type in [ObjectType::Manifest, ObjectType::Asset, ObjectType::KeySlot] {
        let mut header = header_update();
        header.object_type = object_type;
        header.generation = 0;
        header.sender_seq = 0;
        header.total_plaintext_len = 1;
        header.part_count = 1;
        header.part_index = 0;
        header.this_plaintext_len = 1;
        header.ciphertext_len = 17;
        assert!(header.validate().is_ok(), "{object_type:?}");
        header.generation = 1;
        assert!(
            header.validate().is_err(),
            "{object_type:?} generation 必须 0"
        );
        header.generation = 0;
        header.sender_seq = 1;
        assert!(
            header.validate().is_err(),
            "{object_type:?} sender_seq 必须 0"
        );
    }
}

#[test]
fn update_envelope_payload_rules_are_checked() {
    let update = sample_update();
    let mut header = header_update();
    header.generation = update.generation;
    header.sender_seq = update.origin_seq;
    header.total_plaintext_len = update.encode().unwrap().len() as u64;
    header.part_count = 1;
    header.part_index = 0;
    header.this_plaintext_len = update.encode().unwrap().len() as u32;
    header.ciphertext_len = header.this_plaintext_len + 16;
    assert!(header.validate_update_payload(&update).is_ok());

    header.generation += 1;
    assert!(header.validate_update_payload(&update).is_err());
    header.generation = update.generation;
    header.sender_seq += 1;
    assert!(header.validate_update_payload(&update).is_err());

    let snapshot = sample_snapshot();
    let mut header = header_update();
    header.object_type = ObjectType::Snapshot;
    header.generation = snapshot.generation;
    header.sender_seq = 0;
    header.total_plaintext_len = snapshot.encode().unwrap().len() as u64;
    header.part_count = 1;
    header.part_index = 0;
    header.this_plaintext_len = snapshot.encode().unwrap().len() as u32;
    header.ciphertext_len = header.this_plaintext_len + 16;
    assert!(header.validate_snapshot_payload(&snapshot).is_ok());
    header.generation += 1;
    assert!(header.validate_snapshot_payload(&snapshot).is_err());
}

#[test]
fn envelope_roundtrip_and_length_rejection() {
    let header = EnvelopeHeader::default();
    let envelope =
        Envelope::new(header.clone(), vec![0u8; header.ciphertext_len as usize]).unwrap();
    let bytes = envelope.encode().unwrap();
    assert_eq!(bytes.len(), 196 + 17);
    assert_eq!(Envelope::decode(&bytes).unwrap(), envelope);
    assert!(Envelope::decode(&bytes[..bytes.len() - 1]).is_err());
    assert!(Envelope::new(header.clone(), vec![0u8; 16]).is_err());
}

#[test]
fn cursor_roundtrip_base64_and_mac_hook() {
    struct Accept;
    impl CursorMacVerifier for Accept {
        fn verify(&self, input: &[u8], mac: &[u8; CURSOR_MAC_LEN]) -> bool {
            input.len() == CURSOR_MAC_INPUT_LEN && mac == &[9; 32]
        }
    }
    struct Reject;
    impl CursorMacVerifier for Reject {
        fn verify(&self, _input: &[u8], _mac: &[u8; CURSOR_MAC_LEN]) -> bool {
            false
        }
    }

    let cursor = Cursor::new([1; 16], [2; 16], [3; 16], 7, [9; 32]).unwrap();
    let bytes = cursor.encode_bytes().unwrap();
    assert_eq!(bytes.len(), CURSOR_RAW_LEN);
    assert_eq!(bytes[0], CURSOR_VERSION);
    assert_eq!(Cursor::decode_bytes(&bytes).unwrap(), cursor);
    let encoded = cursor.to_base64url().unwrap();
    assert_eq!(encoded.len(), CURSOR_BASE64_LEN);
    assert!(!encoded.contains('='));
    assert_eq!(Cursor::from_base64url(&encoded).unwrap(), cursor);
    assert!(Cursor::decode_verified(&bytes, &Accept).is_ok());
    assert!(Cursor::decode_verified(&bytes, &Reject).is_err());
    assert!(Cursor::decode_bytes(&bytes[..88]).is_err());
}

#[test]
fn object_digest_and_refs_inputs_are_deterministic() {
    let digest = object_digest_input(2, &[[1; 32], [2; 32]]).unwrap();
    assert_eq!(&digest[0..4], &2u32.to_be_bytes());
    assert_eq!(&digest[4..8], &0u32.to_be_bytes());
    assert_eq!(&digest[8..40], &[1; 32]);
    assert_eq!(&digest[40..44], &1u32.to_be_bytes());
    assert_eq!(&digest[44..76], &[2; 32]);
    assert!(object_digest_input(2, &[[1; 32]]).is_err());

    let a = ObjectRefV1 {
        object_id: [2; 32],
        object_type: ObjectType::Snapshot,
        generation: 1,
        key_epoch: 1,
        part_count: 1,
        total_plain_len: 1,
        object_digest: [1; 32],
    };
    let b = ObjectRefV1 {
        object_id: [1; 32],
        object_type: ObjectType::Asset,
        generation: 0,
        ..a.clone()
    };
    let refs = refs_digest_input(&[a, b]).unwrap();
    let decoded = decode_cde(&refs).unwrap();
    let CdeValue::Array(items) = decoded else {
        panic!("refs 必须是 array");
    };
    assert_eq!(items.len(), 2);
}

#[test]
fn limits_and_nul_domains_are_frozen() {
    assert_eq!(MAX_NON_ASSET_PART_PLAINTEXT, 1024 * 1024 - 212);
    assert_eq!(MAX_ASSET_PART_PLAINTEXT, 4 * 1024 * 1024 - 212);
    assert_eq!(MAX_UPDATE_CDE_BYTES, 64 * 1024 * 1024);
    for domain in [
        AAD_DOMAIN,
        HKDF_DATA_INFO_PREFIX,
        HKDF_ASSET_ID_INFO,
        HKDF_PASSWORD_WRAP_PREFIX,
        HKDF_RECOVERY_WRAP_PREFIX,
        HKDF_MANAGEMENT_ED25519_INFO,
        HPKE_INFO,
        DPAPI_ENTROPY_PREFIX,
        SIGN_AUTH_DOMAIN,
        SIGN_MANAGE_DOMAIN,
        SIGN_OBJECT_COMMIT_DOMAIN,
        SIGN_CHECKPOINT_DOMAIN,
    ] {
        assert_eq!(domain.last(), Some(&0x00), "{domain:?}");
        assert!(domain_ends_with_nul(domain));
        assert!(!domain.ends_with(b"\\0"));
        assert_eq!(hex(&domain[domain.len() - 1..]), "00");
    }
    let info = hkdf_data_info(ObjectType::Update, 1, &[2; 32]).unwrap();
    assert_eq!(&info[..11], b"MN/v1/data\0");
    assert_eq!(info[11], 1);
    assert_eq!(info.len(), 11 + 1 + 8 + 32);
}

fn sample_grant() -> GrantAadV1 {
    GrantAadV1 {
        pv: 1,
        vault_id: std::array::from_fn(|index| index as u8),
        epoch: 2,
        recipient_device_id: std::array::from_fn(|index| 0x10 + index as u8),
        recipient_x25519_pub: std::array::from_fn(|index| 0x20 + index as u8),
        manager_rev: 3,
        grant_id: std::array::from_fn(|index| 0x40 + index as u8),
    }
}

#[test]
fn grant_aad_golden_vector_and_signed_bytes() {
    let grant = sample_grant();
    let cde = grant.encode().expect("grant encode");
    assert_eq!(
        hex(&cde),
        concat!(
            "a7000101",
            "50000102030405060708090a0b0c0d0e0f",
            "0202",
            "0350101112131415161718191a1b1c1d1e1f",
            "045820202122232425262728292a2b2c2d2e2f303132333435363738393a3b3c3d3e3f",
            "0503",
            "0650404142434445464748494a4b4c4d4e4f",
        )
    );
    assert_eq!(GrantAadV1::decode(&cde).expect("grant decode"), grant);

    let enc = [0xaa; 32];
    let ciphertext_sha = [0xbb; 32];
    let signed = grant
        .grant_signed_bytes(&enc, &ciphertext_sha)
        .expect("grant signed bytes");
    assert_eq!(&signed[..cde.len()], cde.as_slice());
    assert_eq!(&signed[cde.len()..cde.len() + 32], &enc);
    assert_eq!(&signed[cde.len() + 32..], &ciphertext_sha);
    assert_eq!(signed.len(), cde.len() + 64);
    assert_eq!(
        grant_signed_bytes(&grant, &enc, &ciphertext_sha).expect("free fn"),
        signed
    );

    // 未知键/截断拒绝。
    let mut value = decode_cde(&cde).unwrap();
    let CdeValue::Map(entries) = &mut value else {
        panic!("map");
    };
    entries.push((7, CdeValue::UInt(0)));
    assert!(GrantAadV1::decode(&encode_cde(&value).unwrap()).is_err());
    assert!(GrantAadV1::decode(&cde[..cde.len() - 1]).is_err());
}

#[test]
fn device_cert_signed_bytes_golden_domain() {
    let cert = sample_cert();
    let cde = cert.encode().expect("cert encode");
    let signed = cert.device_cert_signed_bytes().expect("signed");
    assert_eq!(
        &signed[..18],
        &[
            0x4d, 0x4e, 0x2f, 0x76, 0x31, 0x2f, 0x64, 0x65, 0x76, 0x69, 0x63, 0x65, 0x2d, 0x63,
            0x65, 0x72, 0x74, 0x00,
        ],
        "MN/v1/device-cert 后接单个 0x00"
    );
    assert_eq!(&signed[18..], cde.as_slice());
    assert_eq!(
        hex(&signed[18..]),
        "a700500101010101010101010101010101010101500202020202020202020202020202020202582003030303030303030303030303030303030303030303030303030303030303030358200404040404040404040404040404040404040404040404040404040404040404040505020603"
    );
    assert_eq!(device_cert_signed_bytes(&cert).expect("free fn"), signed);
    assert!(SIGN_DEVICE_CERT_DOMAIN.ends_with(b"\0"));
}

#[test]
fn error_codes_are_stable() {
    assert_eq!(ErrorCode::BadCursor.as_str(), "BAD_CURSOR");
    assert_eq!(ErrorCode::ObjectBusy.as_str(), "OBJECT_BUSY");
    assert!(ErrorCode::Busy.retryable());
    assert!(!ErrorCode::Revoked.retryable());
    assert_eq!(ErrorBody::new(ErrorCode::Busy).code, ErrorCode::Busy);
}

#[test]
fn fixed_caps_are_enforced_at_1mib_4mib_64mib() {
    let cap = MAX_NON_ASSET_PART_PLAINTEXT as u64;
    let mut header = header_update();
    header.total_plaintext_len = cap + 1;
    header.part_count = 2;
    header.part_index = 0;
    header.this_plaintext_len = cap as u32;
    header.ciphertext_len = cap as u32 + 16;
    assert!(header.validate().is_ok());

    header.total_plaintext_len = 64 * 1024 * 1024 + 1;
    header.part_count = part_count_for(header.total_plaintext_len, ObjectType::Update) as u32;
    header.part_index = 0;
    header.this_plaintext_len = cap as u32;
    header.ciphertext_len = cap as u32 + 16;
    assert!(matches!(header.validate(), Err(ProtoError::TooLarge)));

    let mut asset = header_update();
    asset.object_type = ObjectType::Asset;
    asset.generation = 0;
    asset.sender_seq = 0;
    asset.total_plaintext_len = MAX_ASSET_PART_PLAINTEXT as u64 + 1;
    asset.part_count = 2;
    asset.part_index = 0;
    asset.this_plaintext_len = MAX_ASSET_PART_PLAINTEXT as u32;
    asset.ciphertext_len = asset.this_plaintext_len + 16;
    assert!(asset.validate().is_ok(), "{:?}", asset.validate());

    // KeySlot 的 4KiB 硬上限。
    let mut key_slot = header_update();
    key_slot.object_type = ObjectType::KeySlot;
    key_slot.generation = 0;
    key_slot.sender_seq = 0;
    key_slot.total_plaintext_len = 4097;
    key_slot.part_count = 1;
    key_slot.part_index = 0;
    key_slot.this_plaintext_len = 4097;
    key_slot.ciphertext_len = 4097 + 16;
    assert!(matches!(key_slot.validate(), Err(ProtoError::TooLarge)));
}

#[test]
fn records_reject_unknown_object_and_slot_tags() {
    let commit = sample_commit();
    let CdeValue::Map(mut entries) = decode_cde(&commit.encode().unwrap()).unwrap() else {
        panic!("commit map");
    };
    entries[3].1 = CdeValue::UInt(9);
    let bytes = encode_cde(&CdeValue::Map(entries)).unwrap();
    assert!(ObjectCommit::decode(&bytes).is_err());

    let slot = sample_slot();
    let CdeValue::Map(mut entries) = slot.to_cde().unwrap() else {
        panic!("slot map");
    };
    entries[4].1 = CdeValue::UInt(9);
    let bytes = encode_cde(&CdeValue::Map(entries)).unwrap();
    assert!(SlotDescriptorV1::decode(&bytes).is_err());
}
