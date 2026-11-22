//! S7-31：轮换共享 CDE 记录的 golden vectors 与解码边界。
//!
//! golden 文件由独立脚本按 relay 字段编号生成，测试只读 hex；本文件不参与生成。

use std::fmt::Write as _;

use menghunote_sync_proto::{
    decode_cde, encode_cde, grant_package_digest, member_cert_digest, slot_set_digest,
    ActivationCommitV1, CdeValue, ControlEvent, ControlKind, DeviceApproveBodyV1, EpochActivated,
    GrantStageBodyV1, InviteCreateBodyV1, MemberCertDigestInputV1, NewManagerV1, RevocationsAdded,
    RotationAbortBodyV1, RotationActivateBodyV1, RotationBeginBodyV1, RotationRestarted,
    SignedControlRecord, SlotActivateBodyV1, SlotSetDigestInputV1,
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

fn sample_epoch_activated() -> EpochActivated {
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
        entries.push((13, CdeValue::UInt(0)));
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
