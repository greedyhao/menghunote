//! task-122：relay 的 checkpoint 验证修订 2 门禁。
//!
//! 覆盖：
//! - publish 只接受 `validation_version=2`（v1 与未知版本都拒绝，且不落任何状态）；
//! - 版本号写进行、不是只放在签名字节里（GC/候选 promote 依赖行版本）；
//! - GC 只把当前修订的 published baseline 当保留根；旧修订再多也不算数（不足两份
//!   当前修订 → blocked，不得清日志/对象）。

mod support;

use support::*;

use relay::checkpoints;
use relay::envelope::ObjectType;
use relay::error::ErrorCode;
use relay::proto;
use relay::refs::RefRole;
use relay::Id16;

use ed25519_dalek::Signer;

/// 构建一份已 staging + 有 refs 的 checkpoint（不发布），返回 (cid, epoch, covered, manifest)。
fn stage_checkpoint(fx: &Fixture) -> (Id16, u64, u64, [u8; 32]) {
    let snapshot = fx.create_data_object(ObjectType::Snapshot, 64);
    let manifest = fx.create_data_object(ObjectType::Manifest, 32);
    let ctx = fx.device_ctx();
    let db = fx.db.clone();
    let clock = fx.clock.clone();
    let outcome = db
        .write_blocking(move |conn| {
            checkpoints::create(conn, clock.as_ref(), &ctx, 4096, None, None, None)
        })
        .expect("create checkpoint");
    let cid = outcome.checkpoint_id;
    let db = fx.db.clone();
    db.write_blocking(move |conn| {
        checkpoints::add_refs(
            conn,
            1024,
            &cid,
            &[(snapshot, RefRole::Data), (manifest, RefRole::Manifest)],
        )
    })
    .expect("add refs");
    (cid, outcome.epoch, outcome.covered_seq, manifest)
}

fn signed_commit(
    fx: &Fixture,
    cid: Id16,
    epoch: u64,
    covered: u64,
    manifest: [u8; 32],
    version: u16,
) -> (Vec<u8>, Vec<u8>) {
    let refs_digest = [9u8; 32];
    let checkpoint_id: [u8; 16] = cid.as_slice().try_into().expect("cid");
    let commit = proto::CheckpointCommit {
        vault_id: fx.vault_id,
        checkpoint_id,
        epoch,
        covered_relay_seq: covered,
        manifest_object_id: manifest,
        manifest_digest: [9u8; 32],
        refs_digest,
        producer_device_id: fx.device,
        validation_version: version,
        validation_digest: if version == 2 { [0x5A; 32] } else { [0u8; 32] },
    };
    let message = commit.encode().expect("commit encode");
    let input = proto::checkpoint_signing_input(&commit).expect("signing input");
    let signature = fx.device_signing_key.sign(&input).to_bytes().to_vec();
    (message, signature)
}

fn publish(
    fx: &Fixture,
    cid: Id16,
    covered: u64,
    manifest: [u8; 32],
    signed_commit: &[u8],
    signature: &[u8],
) -> relay::Result<checkpoints::PublishOutcome> {
    let ctx = fx.device_ctx();
    let db = fx.db.clone();
    let clock = fx.clock.clone();
    let signed_commit = signed_commit.to_vec();
    let signature = signature.to_vec();
    db.write_blocking(move |conn| {
        checkpoints::publish(
            conn,
            clock.as_ref(),
            &relay::verify::Ed25519Hook,
            &ctx,
            &cid,
            &checkpoints::PublishRequest {
                covered_seq: covered,
                manifest_oid: manifest,
                refs_digest: [9u8; 32],
                signed_commit: &signed_commit,
                signature: &signature,
            },
        )
    })
}

#[test]
fn publish_rejects_revision_1_without_writing_published_state() {
    let fx = Fixture::new("cp-v2-reject-v1");
    let (cid, epoch, covered, manifest) = stage_checkpoint(&fx);
    let (message, signature) = signed_commit(&fx, cid, epoch, covered, manifest, 1);

    let error = publish(&fx, cid, covered, manifest, &message, &signature)
        .expect_err("v1 checkpoint 不得发布");
    match error {
        relay::Error::Api(api) => assert_eq!(api.code, ErrorCode::InvalidRequest),
        other => panic!("期望 InvalidRequest，实际 {other:?}"),
    }
    // 状态零副作用：仍是 staging、未写 signed_commit、没有 GC 计划。
    let row = fx.checkpoint_row(&cid);
    assert_eq!(row.state, checkpoints::CheckpointState::Staging);
    assert!(row.signed_commit.is_none());
    assert!(row.published_at.is_none());
    assert!(fx.gc_plans().is_empty(), "v1 不得产生 GC 计划");
}

#[test]
fn publish_rejects_unknown_validation_version() {
    let fx = Fixture::new("cp-v2-reject-unknown");
    let (cid, epoch, covered, manifest) = stage_checkpoint(&fx);
    let (message, signature) = signed_commit(&fx, cid, epoch, covered, manifest, 2);
    // 把字段 8 的版本号改成 3（其余字节不动）：decode 阶段必须拒绝未知修订。
    let mut value = proto::decode_cde(&message).expect("cde");
    let proto::CdeValue::Map(ref mut entries) = value else {
        panic!("commit map");
    };
    let version = entries
        .iter_mut()
        .find(|(key, _)| *key == 8)
        .expect("version field");
    version.1 = proto::CdeValue::UInt(3);
    let message = proto::encode_cde(&value).expect("cde encode");

    let error =
        publish(&fx, cid, covered, manifest, &message, &signature).expect_err("未知修订不得发布");
    match error {
        relay::Error::Api(api) => assert_eq!(api.code, ErrorCode::InvalidRequest),
        other => panic!("期望 InvalidRequest，实际 {other:?}"),
    }
    assert_eq!(
        fx.checkpoint_row(&cid).state,
        checkpoints::CheckpointState::Staging
    );
}

#[test]
fn relay_records_v2_version_on_published_row() {
    let fx = Fixture::new("cp-v2-row-version");
    // 走真实 Ed25519 + 真实 refs 的发布路径（support 已按 v2 构造 commit）。
    let cid = fx.publish_signed_baseline();
    let row = fx.checkpoint_row(&cid);
    assert_eq!(row.state, checkpoints::CheckpointState::Published);
    assert_eq!(
        row.validation_version,
        u64::from(proto::CheckpointCommit::CURRENT_VALIDATION_VERSION)
    );
}

#[test]
fn gc_does_not_use_legacy_version_baselines_as_retention_roots() {
    let fx = Fixture::new("cp-v2-gc-gate");
    // 只有一份当前修订 baseline + 一份旧修订 published（手工插入，模拟升级前遗留）。
    let current = fx.publish_signed_baseline();
    let current_row = fx.checkpoint_row(&current);
    let legacy = [0x7Au8; 16];
    let db = fx.db.clone();
    let vault_id = fx.vault_id;
    let epoch = current_row.epoch;
    let covered = current_row.covered_seq;
    let published_at = fx.clock_now() + 10;
    db.write_blocking(move |conn| {
        conn.execute(
            "INSERT INTO checkpoints(vault_id, checkpoint_id, epoch, covered_seq,
                                     manifest_oid, refs_digest, signed_commit, commit_signature,
                                     state, role, validation_version, health, created_at,
                                     published_at)
             VALUES (?1, ?2, ?3, ?4, NULL, NULL, NULL, NULL, 'published', NULL, 1, 'healthy',
                     0, ?5)",
            (
                vault_id.as_slice(),
                legacy.as_slice(),
                i64::try_from(epoch).expect("epoch"),
                i64::try_from(covered).expect("covered"),
                published_at,
            ),
        )?;
        Ok(())
    })
    .expect("insert legacy baseline");

    // 日志里有可回收内容，但基线不足两份当前修订 → GC 必须 blocked 且 floor 不动。
    let log_before: i64 = fx
        .db
        .read(|conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM log WHERE vault_id = ?1",
                [vault_id.as_slice()],
                |row| row.get(0),
            )?)
        })
        .expect("log count");
    let outcome = fx.gc();
    assert!(outcome.blocked, "只有一份 v2 基线时 GC 必须 blocked");
    assert_eq!(outcome.cut_seq, 0);
    assert_eq!(outcome.floor, 0);
    assert_eq!(outcome.deleted_log_rows, 0);
    let log_after: i64 = fx
        .db
        .read(|conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM log WHERE vault_id = ?1",
                [vault_id.as_slice()],
                |row| row.get(0),
            )?)
        })
        .expect("log count");
    assert_eq!(log_before, log_after, "GC 不得基于请求的旧修订删日志");

    // 旧修订行本身可以保留为历史，但它没有被当作保留根：一旦出现第二份 v2 基线，
    // GC 只按两份 v2 计算。
    let second = fx.publish_signed_baseline();
    let outcome = fx.gc();
    assert!(!outcome.blocked, "两份 v2 基线后 GC 应可继续");
    let _ = second;
    // 旧修订行没有被 GC 当作"最新基线"（否则 floor 会用它）；这里只断言它仍存在，
    // 且没有被 promote/plan 消费。
    let legacy_count: i64 = fx
        .db
        .read(|conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM checkpoints WHERE checkpoint_id = ?1 AND vault_id = ?2",
                (legacy.as_slice(), vault_id.as_slice()),
                |row| row.get(0),
            )?)
        })
        .expect("legacy count");
    assert_eq!(legacy_count, 1);
}
