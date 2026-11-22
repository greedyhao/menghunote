//! ADR §31/§31.1：ASSET 的 AAD 不含 `producer_device_id`，header.producer 固定 0。
//!
//! relay 侧配套：ASSET 是 vault 内共享对象——任一有效成员设备都能 PUT/complete 同一
//! 信封；对象行 producer 记为 0（不写上传者），complete 的签名仍必须由**该 actor**
//! 的有效设备 key 给出。UPDATE/SNAPSHOT/MANIFEST/KEY_SLOT 的严格 producer 检查不变。

mod support;

use std::sync::Arc;

use ed25519_dalek::{Signer, SigningKey};
use relay::auth::{AuthContext, Realm};
use relay::envelope::{self, Header, ObjectType};
use relay::error::ErrorCode;
use relay::ids::{Hash32, Id16};
use relay::objects;
use relay::scope::VaultScope;
use relay::sig::SignatureHook;
use relay::verify::Ed25519Hook;
use support::*;

#[allow(clippy::too_many_arguments)]
fn raw_envelope(
    fx: &Fixture,
    object_id: Hash32,
    object_type: ObjectType,
    producer: [u8; 16],
    generation: u64,
    sender_seq: u64,
    plain_len: u64,
) -> Vec<u8> {
    let header = Header {
        protocol_version: envelope::PROTOCOL_VERSION,
        object_type,
        cipher_suite: envelope::CIPHER_SUITE_XCHACHA20_POLY1305,
        flags: 0,
        vault_id: fx.vault_id,
        object_id,
        message_id: message_id_for(&object_id),
        producer_device_id: producer,
        generation,
        key_epoch: 1,
        sender_seq,
        part_index: 0,
        part_count: 1,
        total_plaintext_len: plain_len,
        this_plaintext_len: u32::try_from(plain_len).expect("fits"),
        ciphertext_len: u32::try_from(plain_len + 16).expect("fits"),
        context_hash: [0u8; 32],
        nonce: [9u8; 24],
    };
    header.encode(&vec![0xAAu8; plain_len as usize + 16])
}

fn asset_envelope(fx: &Fixture, object_id: Hash32, producer: [u8; 16], plain_len: u64) -> Vec<u8> {
    raw_envelope(fx, object_id, ObjectType::Asset, producer, 0, 0, plain_len)
}

fn device_ctx(fx: &Fixture, device: Id16) -> AuthContext {
    AuthContext {
        realm: Realm::Device,
        subject: device,
        device_id: Some(device),
        auth_rev: fx.vault().auth_rev,
        authz_rev: 0,
        authority_rev: 0,
        cap_id: None,
        vault: VaultScope::new(fx.vault_id),
    }
}

fn put_as(
    fx: &Fixture,
    actor: &AuthContext,
    object_id: &Hash32,
    envelope: Vec<u8>,
) -> relay::Result<objects::PartPutOutcome> {
    let actor = *actor;
    let object_id = *object_id;
    let vault_id = fx.vault_id;
    let db = fx.db.clone();
    let clock = fx.clock.clone();
    db.write_blocking(move |conn| {
        objects::put_part(
            conn,
            clock.as_ref(),
            &objects::PartUpload {
                actor: &actor,
                vault_id: &vault_id,
                object_id: &object_id,
                part_no: 0,
                envelope: &envelope,
                reservation: None,
            },
        )
    })
}

fn complete_as(
    fx: &Fixture,
    actor: &AuthContext,
    object_id: &Hash32,
    commit: Vec<u8>,
    signature: Vec<u8>,
    hook: Arc<dyn SignatureHook>,
) -> relay::Result<objects::CompleteOutcome> {
    let actor = *actor;
    let object_id = *object_id;
    let vault_id = fx.vault_id;
    let db = fx.db.clone();
    let clock = fx.clock.clone();
    db.write_blocking(move |conn| {
        objects::complete(
            conn,
            clock.as_ref(),
            hook.as_ref(),
            &objects::CompleteUpload {
                actor: &actor,
                vault_id: &vault_id,
                object_id: &object_id,
                commit: &commit,
                signature: &signature,
            },
        )
    })
}

/// 真实签名：`proto::ObjectCommit` + `object_commit_signing_input`（与 sync 客户端同构）。
fn signed_commit(
    fx: &Fixture,
    object_id: &Hash32,
    envelope: &[u8],
    object_type: ObjectType,
    generation: u64,
    sender_seq: u64,
    signer: &SigningKey,
) -> (Vec<u8>, Vec<u8>) {
    let digest: Hash32 = digest_of(&[envelope.to_vec()]).expect("digest");
    let commit = relay::proto::ObjectCommit {
        vault_id: fx.vault_id,
        object_id: *object_id,
        message_id: message_id_for(object_id),
        object_type: match object_type {
            ObjectType::Update => relay::proto::ObjectType::Update,
            ObjectType::Asset => relay::proto::ObjectType::Asset,
            other => panic!("unexpected type {other:?}"),
        },
        generation,
        key_epoch: 1,
        sender_seq,
        part_count: 1,
        total_len: u64::try_from(envelope.len() - 212).expect("plain len"),
        object_digest: digest,
    };
    let input = relay::proto::object_commit_signing_input(&commit).expect("signing input");
    let signature = signer.sign(&input).to_bytes().to_vec();
    (commit.encode().expect("commit cde"), signature)
}

fn error_code(err: &relay::Error) -> Option<ErrorCode> {
    err.code()
}

#[test]
fn shared_asset_upload_is_device_agnostic_and_idempotent() {
    let fx = Fixture::new("asset-shared");
    let second = [9u8; 16];
    fx.add_device(second);
    let object_id = random_object_id();
    let envelope = asset_envelope(&fx, object_id, [0u8; 16], 64);
    let commit = commit_for(&digest_of(std::slice::from_ref(&envelope)).expect("digest"));

    // 设备 A（genesis）上传并 complete。
    let a = put_as(&fx, &fx.device_ctx(), &object_id, envelope.clone()).expect("A put");
    assert_eq!(a.object_state, objects::ObjectState::Staging);
    let first = complete_as(
        &fx,
        &fx.device_ctx(),
        &object_id,
        commit.clone(),
        b"signature".to_vec(),
        fx.hook.clone(),
    )
    .expect("A complete");
    assert!(!first.duplicate);
    let stored = fx.object_row(&object_id);
    assert_eq!(
        stored.producer, [0u8; 16],
        "ASSET 对象行 producer 必须是 0（共享资产，不记录上传者）"
    );
    assert_eq!(stored.state, objects::ObjectState::Complete);

    // 设备 B 用同一份信封（逐字节相同）PUT/complete：都成功且幂等。
    let b = put_as(&fx, &device_ctx(&fx, second), &object_id, envelope).expect("B put");
    assert!(b.duplicate, "同字节重传必须走幂等分支");
    let replay = complete_as(
        &fx,
        &device_ctx(&fx, second),
        &object_id,
        commit,
        b"other-signature".to_vec(),
        fx.hook.clone(),
    )
    .expect("B complete 幂等");
    assert!(replay.duplicate);
    assert_eq!(replay.relay_seq, first.relay_seq, "不得重复分配 seq");

    // 资产登记只有一份，分片只有一份（没有按设备复制）。
    let (assets, parts): (i64, i64) = fx
        .db
        .read(move |conn| {
            let assets: i64 = conn.query_row(
                "SELECT COUNT(*) FROM asset_objects WHERE object_id = ?1",
                [object_id.as_slice()],
                |row| row.get(0),
            )?;
            let parts: i64 = conn.query_row(
                "SELECT COUNT(*) FROM object_parts WHERE object_id = ?1",
                [object_id.as_slice()],
                |row| row.get(0),
            )?;
            Ok((assets, parts))
        })
        .expect("registry");
    assert_eq!(assets, 1);
    assert_eq!(parts, 1);

    // 同一设备重复 complete 也保持幂等。
    let again = complete_as(
        &fx,
        &fx.device_ctx(),
        &object_id,
        commit_for(&digest_of(&[asset_envelope(&fx, object_id, [0u8; 16], 64)]).expect("digest")),
        b"signature".to_vec(),
        fx.hook.clone(),
    )
    .expect("A complete replay");
    assert!(again.duplicate);
    assert_eq!(again.relay_seq, first.relay_seq);
}

#[test]
fn asset_complete_binds_the_actor_device_key_not_the_zero_producer() {
    let fx = Fixture::new("asset-key");
    let hook: Arc<dyn SignatureHook> = Arc::new(Ed25519Hook);

    // 正例：ASSET(producer=0) 由 actor 的有效设备 key 签名 → complete 成功。
    let object_id = random_object_id();
    let envelope = asset_envelope(&fx, object_id, [0u8; 16], 64);
    put_as(&fx, &fx.device_ctx(), &object_id, envelope.clone()).expect("put");
    let (commit, signature) = signed_commit(
        &fx,
        &object_id,
        &envelope,
        ObjectType::Asset,
        0,
        0,
        &fx.device_signing_key,
    );
    complete_as(
        &fx,
        &fx.device_ctx(),
        &object_id,
        commit,
        signature,
        hook.clone(),
    )
    .expect("valid actor key must complete shared asset");
    assert_eq!(fx.object_row(&object_id).producer, [0u8; 16]);

    // 负例：签名来自另一把（非 actor）设备 key → 验签失败（不是 REVOKED/Forbidden）。
    let other_id = random_object_id();
    let other_env = asset_envelope(&fx, other_id, [0u8; 16], 64);
    put_as(&fx, &fx.device_ctx(), &other_id, other_env.clone()).expect("put2");
    let (commit, signature) = signed_commit(
        &fx,
        &other_id,
        &other_env,
        ObjectType::Asset,
        0,
        0,
        &SigningKey::from_bytes(&[0x66u8; 32]),
    );
    let err = complete_as(
        &fx,
        &fx.device_ctx(),
        &other_id,
        commit,
        signature,
        hook.clone(),
    )
    .expect_err("wrong signer must be rejected");
    assert_eq!(error_code(&err), Some(ErrorCode::SignatureInvalid));

    // 负例：commit 字段被篡改（total_len 变）→ relay 重算绑定失败，仍是签名拒绝。
    let tampered_id = random_object_id();
    let tampered_env = asset_envelope(&fx, tampered_id, [0u8; 16], 64);
    put_as(&fx, &fx.device_ctx(), &tampered_id, tampered_env.clone()).expect("put3");
    let (mut commit, signature) = signed_commit(
        &fx,
        &tampered_id,
        &tampered_env,
        ObjectType::Asset,
        0,
        0,
        &fx.device_signing_key,
    );
    // ObjectCommit 的 total_len 是最后一个 u64 字段之前的固定布局；直接改字节不可靠，
    // 这里用重编码一条字段不一致但签名覆盖原 commit 的路径：改 total_len 后不重签。
    let mut decoded = relay::proto::ObjectCommit::decode(&commit).expect("decode commit");
    decoded.total_len = decoded.total_len.saturating_add(1);
    commit = decoded.encode().expect("re-encode");
    let err = complete_as(&fx, &fx.device_ctx(), &tampered_id, commit, signature, hook)
        .expect_err("tampered commit must be rejected");
    assert_eq!(error_code(&err), Some(ErrorCode::SignatureInvalid));
}

#[test]
fn non_asset_producer_rules_stay_strict() {
    let fx = Fixture::new("asset-producer-strict");
    let second = [9u8; 16];
    fx.add_device(second);

    // UPDATE：producer 必须是 actor；0 / 其他设备一律 403。
    // 构造一个 producer=0 的 UPDATE 信封（与 ASSET builder 同一 header 布局，只改类型字节）。
    let object_id = random_object_id();
    let zero_update = raw_envelope(&fx, object_id, ObjectType::Update, [0u8; 16], 1, 1, 32);
    let err = put_as(&fx, &fx.device_ctx(), &object_id, zero_update).expect_err("update zero");
    assert_eq!(error_code(&err), Some(ErrorCode::Forbidden));
    let foreign = single_part_envelope_at(
        ObjectType::Update,
        fx.vault_id,
        random_object_id(),
        1,
        1,
        32,
        1,
    );
    // support 的 producer 是 [2;16]=genesis 设备；用第二台设备 token 上传 → 403。
    let err = put_as(
        &fx,
        &device_ctx(&fx, second),
        &{
            // 从信封里取 object_id 不可行（Header::parse 可解）。
            Header::parse(&foreign).expect("header").object_id
        },
        foreign,
    )
    .expect_err("foreign update");
    assert_eq!(error_code(&err), Some(ErrorCode::Forbidden));

    // ASSET：producer 为其他设备（非 0、非 actor）→ 403。
    let foreign_asset = asset_envelope(&fx, random_object_id(), second, 32);
    let foreign_id = Header::parse(&foreign_asset).expect("header").object_id;
    let err = put_as(&fx, &fx.device_ctx(), &foreign_id, foreign_asset)
        .expect_err("foreign asset producer");
    assert_eq!(error_code(&err), Some(ErrorCode::Forbidden));

    // ASSET：producer 为 actor（兼容旧写法）→ 允许，但对象行仍记 0。
    let legacy_id = random_object_id();
    let legacy = asset_envelope(&fx, legacy_id, fx.device, 32);
    put_as(&fx, &fx.device_ctx(), &legacy_id, legacy).expect("legacy asset allowed");
    assert_eq!(fx.object_row(&legacy_id).producer, [0u8; 16]);
}

#[test]
fn different_assets_are_still_object_id_scoped() {
    let fx = Fixture::new("asset-two-ids");
    let second = [9u8; 16];
    fx.add_device(second);
    for _ in 0..2 {
        let object_id = random_object_id();
        let envelope = asset_envelope(&fx, object_id, [0u8; 16], 64);
        let commit = commit_for(&digest_of(std::slice::from_ref(&envelope)).expect("digest"));
        put_as(&fx, &fx.device_ctx(), &object_id, envelope.clone()).expect("put");
        let done = complete_as(
            &fx,
            &fx.device_ctx(),
            &object_id,
            commit.clone(),
            b"signature".to_vec(),
            fx.hook.clone(),
        )
        .expect("complete");
        assert!(!done.duplicate);
        // 另一台设备独立复核同一对象（幂等）。
        let dup = put_as(&fx, &device_ctx(&fx, second), &object_id, envelope).expect("put dup");
        assert!(dup.duplicate);
        let replay = complete_as(
            &fx,
            &device_ctx(&fx, second),
            &object_id,
            commit,
            b"signature".to_vec(),
            fx.hook.clone(),
        )
        .expect("complete dup");
        assert!(replay.duplicate);
        assert_eq!(replay.relay_seq, done.relay_seq);
    }
    let assets: i64 = fx
        .db
        .read(
            |conn| Ok(conn.query_row("SELECT COUNT(*) FROM asset_objects", [], |row| row.get(0))?),
        )
        .expect("assets");
    assert_eq!(assets, 2, "不同 object_id 各自独立登记");
}
