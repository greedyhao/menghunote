//! S7-31：轮换共享 CDE 记录的 golden vectors 与解码边界。
//!
//! golden 文件由独立脚本按 relay 字段编号生成，测试只读 hex；本文件不参与生成。

use std::fmt::Write as _;

use menghunote_sync_proto::{
    decode_cde, encode_cde, grant_package_digest, member_cert_digest, slot_set_digest,
    ActivationCommitV1, ActivationTV1, CdeValue, ControlEvent, ControlKind, DeviceApproveBodyV1,
    EpochActivated, GrantStageBodyV1, HttpMethod, InviteCreateBodyV1, MemberCertDigestInputV1,
    NewManagerV1, RevocationsAdded, RotationAbortBodyV1, RotationAborted, RotationActivateBodyV1,
    RotationBeginBodyV1, RotationRestarted, SignedControlRecord, SlotActivateBodyV1,
    SlotSetDigestInputV1,
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
    include_str!("golden_rotation_records.hex")
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

fn signature(value: u8) -> Vec<u8> {
    vec![value; 64]
}

fn mutate_map(bytes: &[u8], mutate: impl FnOnce(&mut Vec<(u64, CdeValue)>)) -> Vec<u8> {
    let CdeValue::Map(mut entries) = decode_cde(bytes).expect("decode map") else {
        panic!("期望 map");
    };
    mutate(&mut entries);
    encode_cde(&CdeValue::Map(entries)).expect("encode mutated")
}

fn sample_control_record() -> SignedControlRecord {
    SignedControlRecord {
        kind: ControlKind::EpochActivated,
        request_id: ids(1),
        manager_rev: 2,
        expected_control_rev: 7,
        signed_body: b"body".to_vec(),
        signature: signature(0xAA),
        event: Some(b"event".to_vec()),
    }
}

/// ADR §34 的完整 activation T 样本；与事件/activate body 的重复事实保持一致。
fn sample_activation_t() -> ActivationTV1 {
    ActivationTV1 {
        vault_id: ids(1),
        instance_id: ids(2),
        rotation_id: ids(3),
        from_epoch: 1,
        to_epoch: 2,
        freeze_seq: 9,
        old_manager_rev: 2,
        new_manager_rev: 3,
        new_manager_pubkey: hash(0xB1),
        prev_control_hash: hash(0xB2),
        expected_control_rev: 11,
        primary_cid: ids(4),
        primary_commit_digest: hash(0xC1),
        fallback_cid: ids(5),
        fallback_commit_digest: hash(0xC2),
        slot_set_digest: hash(0xC3),
        member_cert_digest: hash(0xC4),
        // 必须与 EpochActivated.grants_digest 一致。
        grants_digest: hash(8),
        old_asset_set_digest: hash(0xC6),
        new_asset_set_digest: hash(0xC7),
    }
}

fn sample_epoch_activated() -> EpochActivated {
    let activation_t = sample_activation_t();
    EpochActivated {
        vault_id: ids(1),
        instance_id: ids(2),
        rotation_id: ids(3),
        from_epoch: 1,
        to_epoch: 2,
        covered_seq: 9,
        primary_checkpoint: ids(4),
        fallback_checkpoint: ids(5),
        activation_commit: hash(6),
        member_set_hash: hash(7),
        grants_digest: hash(8),
        control_rev: 11,
        auth_rev: 12,
        activation_t: Some(activation_t.encode().expect("T encode")),
        old_t_signature: Some(signature(0xBB)),
        new_t_signature: Some(signature(0xCC)),
    }
}

/// 升级前的 `epoch.activated`（无 T/双签），用于锁定向后兼容解码。
fn legacy_epoch_activated() -> EpochActivated {
    EpochActivated {
        activation_t: None,
        old_t_signature: None,
        new_t_signature: None,
        ..sample_epoch_activated()
    }
}

fn sample_restarted() -> RotationRestarted {
    RotationRestarted {
        old_rotation: ids(1),
        new_rotation: ids(2),
        from_epoch: 1,
        old_to_epoch: 2,
        new_to_epoch: 3,
        freeze_seq: 5,
    }
}

fn sample_revocations(restarted: bool, new_rotation: Option<[u8; 16]>) -> RevocationsAdded {
    RevocationsAdded {
        rotation_id: ids(1),
        devices: vec![ids(2), ids(3)],
        restarted,
        new_rotation,
    }
}

/// §35.3：abort 事件样本（rid/method/path/body digest）。
///
/// path 故意用固定文本而不是由 rid 拼出来：golden 锁的是字段编号与 CDE 编码，
/// 客户端/relay 的 path 一致性检查在各自 crate 的验证测试里覆盖。
fn sample_aborted() -> RotationAborted {
    RotationAborted {
        rotation_id: ids(3),
        method: HttpMethod::Post,
        path: "/v1/vaults/1/rotations/1/abort".to_owned(),
        body_digest: hash(0xDD),
    }
}

fn sample_activation_commit(with_asset: bool) -> ActivationCommitV1 {
    ActivationCommitV1 {
        vault_id: ids(1),
        instance_id: ids(2),
        rotation_id: ids(3),
        from_epoch: 1,
        to_epoch: 2,
        covered_seq: 9,
        primary_checkpoint: ids(4),
        fallback_checkpoint: ids(5),
        primary_covered_seq: 7,
        fallback_covered_seq: 8,
        primary_manifest: hash(6),
        fallback_manifest: hash(7),
        member_set_hash: hash(8),
        grants_digest: hash(9),
        asset_set_digest: with_asset.then(|| hash(10)),
        control_rev: 11,
    }
}

fn sample_activate_body() -> RotationActivateBodyV1 {
    RotationActivateBodyV1 {
        new_manager: Some(NewManagerV1 {
            transition: b"tr".to_vec(),
            old_signature: Some(signature(0xAA)),
            new_signature: None,
            activation_t: Some(sample_activation_t().encode().expect("T encode")),
            old_t_signature: Some(signature(0xBB)),
            new_t_signature: Some(signature(0xCC)),
        }),
        primary_checkpoint: Some(ids(4)),
        fallback_checkpoint: Some(ids(5)),
        member_cert_digest: Some(hash(6)),
        grants_digest: Some(hash(7)),
        slot_set_digest: Some(hash(8)),
        asset_set_digest: Some(hash(9)),
    }
}

fn sample_approve_body() -> DeviceApproveBodyV1 {
    DeviceApproveBodyV1 {
        certificate: b"cert".to_vec(),
        certificate_signature: signature(0xAA),
        grant_id: ids(1),
        hpke_enc: hash(2),
        hpke_ciphertext: b"ct".to_vec(),
        grant_signature: signature(0xBB),
        request_id: ids(3),
        expected_control_rev: 7,
    }
}

#[test]
fn control_record_golden_and_event_decoding() {
    let record = sample_control_record();
    let encoded = record.encode().expect("encode");
    assert_eq!(hex(&encoded), golden("control"));
    assert_eq!(
        SignedControlRecord::decode(&encoded).expect("decode"),
        record
    );
    assert_eq!(record.kind.as_str(), "epoch.activated");

    let event = sample_epoch_activated().encode().expect("event");
    let mut with_event = record;
    with_event.event = Some(event);
    assert_eq!(
        with_event.decoded_event().expect("decoded"),
        Some(ControlEvent::EpochActivated(sample_epoch_activated()))
    );
    let mut restart = with_event;
    restart.kind = ControlKind::RotationRestarted;
    assert!(restart.decoded_event().is_err());

    // §35.3：abort 记录现在携带可重建签名输入的 typed 事件。
    let mut abort = restart;
    abort.kind = ControlKind::RotationAbort;
    abort.event = Some(sample_aborted().encode().expect("abort event"));
    assert_eq!(
        abort.decoded_event().expect("abort decoded"),
        Some(ControlEvent::RotationAborted(sample_aborted()))
    );
    // kind 与 event 形状不匹配（epoch 事件塞进 abort）→ 拒绝，不能把畸形事件
    // 当成"可重建"。
    let mut mismatch = abort.clone();
    mismatch.event = Some(sample_epoch_activated().encode().expect("epoch event"));
    assert!(mismatch.decoded_event().is_err());
    // begin 仍不允许携带事件。
    let mut begin = abort;
    begin.kind = ControlKind::RotationBegin;
    assert!(begin.decoded_event().is_err());

    let unknown_kind = mutate_map(&encoded, |entries| {
        entries[0] = (0, CdeValue::Text("epoch.unknown".to_owned()));
    });
    assert!(SignedControlRecord::decode(&unknown_kind).is_err());
    let unknown_key = mutate_map(&encoded, |entries| entries.push((7, CdeValue::UInt(1))));
    assert!(SignedControlRecord::decode(&unknown_key).is_err());
    assert!(SignedControlRecord::decode(&encoded[..encoded.len() - 1]).is_err());
}

#[test]
fn rotation_events_golden_and_unknown_keys_rejected() {
    let epoch = sample_epoch_activated();
    let encoded = epoch.encode().expect("epoch encode");
    assert_eq!(hex(&encoded), golden("epoch"));
    assert_eq!(
        EpochActivated::decode(&encoded).expect("epoch decode"),
        epoch
    );
    assert!(EpochActivated::decode(&mutate_map(&encoded, |entries| {
        entries.push((16, CdeValue::UInt(0)));
    }))
    .is_err());
    assert!(EpochActivated::decode(&encoded[..encoded.len() - 1]).is_err());

    let restarted = sample_restarted();
    let encoded = restarted.encode().expect("restart encode");
    assert_eq!(hex(&encoded), golden("restart"));
    assert_eq!(
        RotationRestarted::decode(&encoded).expect("restart decode"),
        restarted
    );
    assert!(RotationRestarted::decode(&mutate_map(&encoded, |entries| {
        entries.push((6, CdeValue::UInt(0)));
    }))
    .is_err());
    assert!(RotationRestarted::decode(&encoded[..encoded.len() - 1]).is_err());

    let revoked_some = sample_revocations(true, Some(ids(4)));
    let encoded = revoked_some.encode().expect("revoked encode");
    assert_eq!(hex(&encoded), golden("revoked_some"));
    assert_eq!(
        RevocationsAdded::decode(&encoded).expect("revoked decode"),
        revoked_some
    );
    assert!(RevocationsAdded::decode(&encoded[..encoded.len() - 1]).is_err());

    let revoked_none = sample_revocations(false, None);
    let encoded = revoked_none.encode().expect("revoked none encode");
    assert_eq!(hex(&encoded), golden("revoked_none"));
    assert_eq!(
        RevocationsAdded::decode(&encoded).expect("revoked none decode"),
        revoked_none
    );
    assert!(RevocationsAdded::decode(&mutate_map(&encoded, |entries| {
        entries.push((4, CdeValue::UInt(0)));
    }))
    .is_err());

    let aborted = sample_aborted();
    let encoded = aborted.encode().expect("abort event encode");
    assert_eq!(hex(&encoded), golden("abort_event"));
    assert_eq!(
        RotationAborted::decode(&encoded).expect("abort event decode"),
        aborted
    );
    assert_eq!(aborted.encode().expect("abort re-encode"), encoded);
    // 未知键 / 截断 / 非法 method 标签都必须拒绝。
    assert!(RotationAborted::decode(&mutate_map(&encoded, |entries| {
        entries.push((4, CdeValue::UInt(0)));
    }))
    .is_err());
    assert!(RotationAborted::decode(&encoded[..encoded.len() - 1]).is_err());
    let bad_method = mutate_map(&encoded, |entries| {
        entries[1] = (1, CdeValue::UInt(0));
    });
    assert!(RotationAborted::decode(&bad_method).is_err());
    assert!(RotationAborted::decode(&mutate_map(&encoded, |entries| {
        entries[0] = (0, CdeValue::Bytes(vec![0x03; 15]));
    }))
    .is_err());
}

#[test]
fn activation_commit_golden_and_unknown_key_rejected() {
    let with_asset = sample_activation_commit(true);
    let encoded = with_asset.encode().expect("encode");
    assert_eq!(hex(&encoded), golden("commit"));
    assert_eq!(
        ActivationCommitV1::decode(&encoded).expect("decode"),
        with_asset
    );
    assert_eq!(with_asset.encode().expect("re-encode"), encoded);

    let without_asset = sample_activation_commit(false);
    let encoded = without_asset.encode().expect("encode no asset");
    assert_eq!(hex(&encoded), golden("commit_noasset"));
    assert_eq!(
        ActivationCommitV1::decode(&encoded).expect("decode no asset"),
        without_asset
    );
    let unknown_key = mutate_map(&encoded, |entries| entries.push((16, CdeValue::UInt(0))));
    assert!(ActivationCommitV1::decode(&unknown_key).is_err());
    assert!(ActivationCommitV1::decode(&encoded[..encoded.len() - 1]).is_err());
}

#[test]
fn epoch_activated_carries_t_and_keeps_legacy_records_decodable() {
    // 升级前的 13 字段事件仍可解码（向后兼容），但 T 字段为 None。
    let legacy = legacy_epoch_activated();
    let encoded = legacy.encode().expect("legacy encode");
    assert_eq!(hex(&encoded), golden("epoch_legacy"));
    assert_eq!(
        EpochActivated::decode(&encoded).expect("legacy decode"),
        legacy
    );

    let epoch = sample_epoch_activated();
    let encoded = epoch.encode().expect("epoch encode");
    assert_eq!(hex(&encoded), golden("epoch"));
    assert_eq!(
        EpochActivated::decode(&encoded).expect("epoch decode"),
        epoch
    );

    // 13–15 必须同时存在（不允许只带 T 或只带一签）。
    let mut partial = epoch.clone();
    partial.old_t_signature = None;
    assert!(partial.encode().is_err());
    let missing_signature = mutate_map(&encoded, |entries| entries.retain(|(key, _)| *key != 14));
    assert!(EpochActivated::decode(&missing_signature).is_err());

    // T 双签各 64B。
    let mut short_signature = epoch.clone();
    short_signature.new_t_signature = Some(vec![0xCC; 63]);
    assert!(short_signature.encode().is_err());

    // T 与事件重复事实不一致 → 拒绝（防 relay 把 T 拼到另一条事件上）。
    let mut mismatched = epoch.clone();
    mismatched.to_epoch = 3;
    assert!(mismatched.encode().is_err());

    // T 内的重复事实被改（但 T 本身仍自洽）→ 拒绝。
    let mut tampered = epoch.clone();
    let mut activation_t =
        ActivationTV1::decode(tampered.activation_t.as_deref().expect("T")).expect("T decode");
    activation_t.to_epoch = 5;
    tampered.activation_t = Some(activation_t.encode().expect("T re-encode"));
    assert!(tampered.encode().is_err());

    // T 字节本身损坏 → 拒绝。
    let mut corrupted = epoch.clone();
    let mut activation_t = corrupted.activation_t.clone().expect("T");
    activation_t[0] = 0xFF;
    corrupted.activation_t = Some(activation_t);
    assert!(corrupted.encode().is_err());
}

#[test]
fn new_manager_requires_all_or_none_t_attachment_and_64_byte_t_signatures() {
    let manager = sample_activate_body().new_manager.expect("manager");
    let encoded = manager.encode().expect("manager encode");
    assert_eq!(
        NewManagerV1::decode(&encoded).expect("manager decode"),
        manager
    );

    // 3–5 全缺：合法（渐进接入/历史 body）；relay 执行时再判 409。
    let without_t = mutate_map(&encoded, |entries| {
        entries.retain(|(key, _)| !(3..=5).contains(key));
    });
    let decoded = NewManagerV1::decode(&without_t).expect("all-or-none: 全缺可解码");
    assert_eq!(decoded.activation_t, None);
    assert_eq!(decoded.old_t_signature, None);
    assert_eq!(decoded.new_t_signature, None);
    assert_eq!(decoded.encode().expect("re-encode"), without_t);

    // 只缺 3–5 中的任意一个 → 拒绝（不允许半套 T 材料）。
    for missing in [3u64, 4, 5] {
        let mutated = mutate_map(&encoded, |entries| {
            entries.retain(|(key, _)| *key != missing);
        });
        assert!(
            NewManagerV1::decode(&mutated).is_err(),
            "缺 key {missing} 必须拒绝"
        );
    }

    // 未知键 / 截断 → 拒绝。
    assert!(NewManagerV1::decode(&mutate_map(&encoded, |entries| {
        entries.push((6, CdeValue::UInt(0)));
    }))
    .is_err());
    assert!(NewManagerV1::decode(&encoded[..encoded.len() - 1]).is_err());

    // T 双签必须各 64B。
    let short = mutate_map(&encoded, |entries| {
        for (key, value) in entries.iter_mut() {
            if *key == 4 {
                *value = CdeValue::Bytes(vec![0xBB; 63]);
            }
        }
    });
    assert!(NewManagerV1::decode(&short).is_err());

    // key 3 必须是可解码的 ActivationTV1 CDE。
    let not_a_t = mutate_map(&encoded, |entries| {
        for (key, value) in entries.iter_mut() {
            if *key == 3 {
                *value = CdeValue::Bytes(vec![0xA0]);
            }
        }
    });
    assert!(NewManagerV1::decode(&not_a_t).is_err());
}

#[test]
fn management_canonical_bodies_golden() {
    let begin = RotationBeginBodyV1 {
        expected_epoch: 1,
        expected_control_rev: 7,
        request_id: ids(1),
        revoke: vec![ids(2), ids(3)],
    };
    let encoded = begin.encode().expect("begin encode");
    assert_eq!(hex(&encoded), golden("begin"));
    assert_eq!(
        RotationBeginBodyV1::decode(&encoded).expect("begin decode"),
        begin
    );
    assert!(RotationBeginBodyV1::decode(&encoded[..encoded.len() - 1]).is_err());
    assert!(
        RotationBeginBodyV1::decode(&mutate_map(&encoded, |entries| {
            entries.push((4, CdeValue::UInt(0)));
        }))
        .is_err()
    );

    let activate = sample_activate_body();
    let encoded = activate.encode().expect("activate encode");
    assert_eq!(hex(&encoded), golden("activate"));
    assert_eq!(
        RotationActivateBodyV1::decode(&encoded).expect("activate decode"),
        activate
    );
    assert!(RotationActivateBodyV1::decode(&encoded[..encoded.len() - 1]).is_err());
    assert!(
        RotationActivateBodyV1::decode(&mutate_map(&encoded, |entries| {
            entries.push((7, CdeValue::UInt(1)));
        }))
        .is_err()
    );

    let grant = GrantStageBodyV1 {
        grant_id: ids(1),
        hpke_enc: hash(2),
        hpke_ciphertext: b"ct".to_vec(),
        signed_header: b"hd".to_vec(),
        signature: signature(0xAA),
        manager_rev: 3,
    };
    let encoded = grant.encode().expect("grant encode");
    assert_eq!(hex(&encoded), golden("grant"));
    assert_eq!(
        GrantStageBodyV1::decode(&encoded).expect("grant decode"),
        grant
    );
    assert!(GrantStageBodyV1::decode(&encoded[..encoded.len() - 1]).is_err());
    assert!(GrantStageBodyV1::decode(&mutate_map(&encoded, |entries| {
        entries.push((6, CdeValue::UInt(0)));
    }))
    .is_err());

    let abort = RotationAbortBodyV1;
    let encoded = abort.encode().expect("abort encode");
    assert_eq!(hex(&encoded), golden("abort"));
    assert_eq!(
        RotationAbortBodyV1::decode(&encoded).expect("abort decode"),
        abort
    );
    assert!(RotationAbortBodyV1::decode(&encoded[..encoded.len() - 1]).is_err());
    let abort_unknown = mutate_map(&[0xa0], |entries| entries.push((0, CdeValue::UInt(1))));
    assert!(RotationAbortBodyV1::decode(&abort_unknown).is_err());

    let slot = SlotActivateBodyV1 {
        expected_slot_set_rev: 7,
        new_slot_id: hash(8),
    };
    let encoded = slot.encode().expect("slot encode");
    assert_eq!(hex(&encoded), golden("slot"));
    assert_eq!(
        SlotActivateBodyV1::decode(&encoded).expect("slot decode"),
        slot
    );
    assert!(SlotActivateBodyV1::decode(&encoded[..encoded.len() - 1]).is_err());
    assert!(SlotActivateBodyV1::decode(&mutate_map(&encoded, |entries| {
        entries.push((2, CdeValue::UInt(0)));
    }))
    .is_err());

    let invite = InviteCreateBodyV1 {
        token_hash: hash(1),
        expires_at: Some(123),
    };
    let encoded = invite.encode().expect("invite encode");
    assert_eq!(hex(&encoded), golden("invite"));
    assert_eq!(
        InviteCreateBodyV1::decode(&encoded).expect("invite decode"),
        invite
    );
    assert!(InviteCreateBodyV1::decode(&encoded[..encoded.len() - 1]).is_err());
    assert!(InviteCreateBodyV1::decode(&mutate_map(&encoded, |entries| {
        entries.push((2, CdeValue::UInt(0)));
    }))
    .is_err());
    let no_expiry = InviteCreateBodyV1 {
        token_hash: hash(1),
        expires_at: None,
    };
    let encoded = no_expiry.encode().expect("invite no expiry");
    assert_eq!(hex(&encoded), golden("invite_noexp"));
    assert_eq!(
        InviteCreateBodyV1::decode(&encoded).expect("invite no expiry decode"),
        no_expiry
    );

    let approve = sample_approve_body();
    let encoded = approve.encode().expect("approve encode");
    assert_eq!(hex(&encoded), golden("approve"));
    assert_eq!(
        DeviceApproveBodyV1::decode(&encoded).expect("approve decode"),
        approve
    );
    assert!(DeviceApproveBodyV1::decode(&encoded[..encoded.len() - 1]).is_err());
    assert!(
        DeviceApproveBodyV1::decode(&mutate_map(&encoded, |entries| {
            entries.push((8, CdeValue::UInt(0)));
        }))
        .is_err()
    );
}

#[test]
fn digest_inputs_match_relay_preimages() {
    let members = vec![
        MemberCertDigestInputV1 {
            device_id: ids(2),
            certificate: b"b".to_vec(),
            certificate_signature: None,
        },
        MemberCertDigestInputV1 {
            device_id: ids(1),
            certificate: b"a".to_vec(),
            certificate_signature: Some(vec![0xAA, 0xBB]),
        },
    ];
    let mut expected = Vec::new();
    expected.extend_from_slice(&ids(1));
    expected.extend_from_slice(b"a");
    expected.extend_from_slice(&[0xAA, 0xBB]);
    expected.extend_from_slice(&ids(2));
    expected.extend_from_slice(b"b");
    assert_eq!(member_cert_digest(&members), expected);

    let slots = vec![
        SlotSetDigestInputV1 {
            slot_id: hash(2),
            descriptor_sha256: hash(3),
        },
        SlotSetDigestInputV1 {
            slot_id: hash(1),
            descriptor_sha256: hash(0),
        },
    ];
    let mut expected = Vec::new();
    expected.extend_from_slice(&hash(1));
    expected.extend_from_slice(&hash(0));
    expected.extend_from_slice(&hash(2));
    expected.extend_from_slice(&hash(3));
    assert_eq!(slot_set_digest(&slots), expected);

    let mut expected = Vec::new();
    expected.extend_from_slice(&hash(1));
    expected.extend_from_slice(&hash(2));
    expected.extend_from_slice(&hash(3));
    expected.extend_from_slice(&[0xAA, 0xBB, 0xCC]);
    assert_eq!(
        grant_package_digest(&hash(1), &hash(2), &hash(3), &[0xAA, 0xBB, 0xCC]),
        expected
    );
}
